use crate::auth::token::TokenStore;
use crate::client::{EntryType, SynsClient};
use crate::config::Config;
use crate::errors::CliError;
use crate::output::Output;
use crate::push::manifest::Manifest;
use crate::repo::if_repo::resolve_full_or_skip;
use crate::repo::syns_yaml::write_syns_yaml;
use console::style;
use serde_json::json;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

fn validate_entry_path(path: &str) -> Result<(), CliError> {
    if path.is_empty()
        || path.starts_with('/')
        || path.contains('\0')
        || path.split('/').any(|seg| seg == ".." || seg == ".git")
    {
        return Err(CliError::Io {
            message: format!("refusing path outside target directory: {path}"),
        });
    }
    Ok(())
}

fn safe_join(target_dir: &Path, entry_path: &str) -> Result<PathBuf, CliError> {
    validate_entry_path(entry_path)?;
    let joined = target_dir.join(entry_path);
    if !joined.starts_with(target_dir) {
        return Err(CliError::Io {
            message: format!("refusing path outside target directory: {entry_path}"),
        });
    }
    Ok(joined)
}

pub async fn cmd_pull(
    config: &Config,
    output: &Output,
    repo_arg: Option<String>,
    path_arg: Option<String>,
    version: Option<String>,
    if_repo: bool,
) -> Result<(), CliError> {
    let target_dir = match &path_arg {
        Some(p) => PathBuf::from(p),
        None => std::env::current_dir().map_err(|e| CliError::Io {
            message: format!("could not determine current directory: {e}"),
        })?,
    };

    let (owner, name) = if let Some(ref arg) = repo_arg {
        let mut parts = arg.splitn(2, '/');
        let o = parts.next().unwrap_or("");
        let n = parts.next().unwrap_or("");
        if o.is_empty() || n.is_empty() {
            return Err(CliError::Io {
                message: "invalid repo identifier — expected OWNER/NAME format".into(),
            });
        }
        (o.to_string(), n.to_string())
    } else {
        match resolve_full_or_skip(None, &target_dir, if_repo, output)? {
            Some(pair) => pair,
            None => return Ok(()),
        }
    };

    let repo_id = format!("{owner}/{name}");
    let token = TokenStore::new(config.credentials_path())
        .read()
        .ok()
        .flatten();
    let client = SynsClient::new(config.server_url())?;

    let tree_response = if version.is_some() {
        client
            .get_tree(&repo_id, token.as_deref(), None, true, version.as_deref())
            .await?
    } else {
        client.pull(&repo_id, token.as_deref()).await?
    };

    let server_files: Vec<_> = tree_response
        .entries
        .iter()
        .filter(|e| e.entry_type == EntryType::File)
        .collect();

    let mut manifest = Manifest::load(config.cache_dir(), &owner, &name).unwrap_or_default();

    let server_paths: HashSet<String> = server_files.iter().map(|e| e.path.clone()).collect();
    let mut to_download = Vec::new();
    let mut unchanged_count: usize = 0;

    for entry in &server_files {
        if version.is_none()
            && let Some(server_sha) = &entry.sha
            && let Some(local_sha) = manifest.file_sha(&entry.path)
            && server_sha == local_sha
        {
            let file_path = safe_join(&target_dir, &entry.path)?;
            if file_path.exists() {
                unchanged_count += 1;
                continue;
            }
        }
        to_download.push(*entry);
    }

    let to_delete: Vec<String> = if version.is_none() {
        manifest
            .file_paths()
            .filter(|p| !server_paths.contains(*p))
            .map(|p| p.to_string())
            .collect()
    } else {
        Vec::new()
    };

    std::fs::create_dir_all(&target_dir).map_err(|e| CliError::Io {
        message: format!("could not create target directory: {e}"),
    })?;

    for entry in &to_download {
        let response = client
            .get_file(&repo_id, token.as_deref(), &entry.path, version.as_deref())
            .await?;
        let file_path = safe_join(&target_dir, &entry.path)?;
        if let Some(parent) = file_path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| CliError::Io {
                message: format!("could not write file {}: {e}", entry.path),
            })?;
        }
        std::fs::write(&file_path, &response.content).map_err(|e| CliError::Io {
            message: format!("could not write file {}: {e}", entry.path),
        })?;
        if !output.is_json() {
            eprintln!("  {}", style(format!("downloaded: {}", entry.path)).green());
        }
    }

    for path in &to_delete {
        let file_path = safe_join(&target_dir, path)?;
        match std::fs::remove_file(&file_path) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => eprintln!("  warning: could not delete {path}: {e}"),
        }
        if !output.is_json() {
            eprintln!("  {}", style(format!("deleted: {path}")).red());
        }
    }

    if repo_arg.is_some() {
        write_syns_yaml(&target_dir, &owner, &name)?;
    }

    if version.is_none() {
        let files_map: HashMap<String, String> = server_files
            .iter()
            .map(|e| (e.path.clone(), e.sha.clone().unwrap_or_default()))
            .collect();
        manifest.update(tree_response.commit_sha.clone(), files_map);
        manifest.save(config.cache_dir(), &owner, &name)?;
    }

    let downloaded = to_download.len();
    let deleted = to_delete.len();

    if output.is_json() {
        let mut summary = json!({
            "repo": repo_id,
            "commitSha": tree_response.commit_sha,
            "downloaded": downloaded,
            "unchanged": unchanged_count,
            "deleted": deleted,
        });
        if let Some(ref v) = version {
            summary
                .as_object_mut()
                .unwrap()
                .insert("version".into(), json!(v));
        }
        output.json(&summary);
    } else {
        output.success(&format!(
            "Pulled {repo_id}: {downloaded} downloaded, {unchanged_count} unchanged, {deleted} deleted"
        ));
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serial_test::serial;

    #[test]
    fn validate_entry_path_accepts_normal_paths() {
        assert!(validate_entry_path("README.md").is_ok());
        assert!(validate_entry_path("src/main.rs").is_ok());
        assert!(validate_entry_path("a/b/c/d.txt").is_ok());
        assert!(validate_entry_path(".hidden").is_ok());
        assert!(validate_entry_path("dir/.hidden/file").is_ok());
    }

    #[test]
    fn validate_entry_path_rejects_dot_dot() {
        assert!(validate_entry_path("..").is_err());
        assert!(validate_entry_path("../etc/passwd").is_err());
        assert!(validate_entry_path("foo/../../etc/passwd").is_err());
        assert!(validate_entry_path("foo/..").is_err());
    }

    #[test]
    fn validate_entry_path_rejects_absolute_paths() {
        assert!(validate_entry_path("/etc/passwd").is_err());
        assert!(validate_entry_path("/tmp/file").is_err());
    }

    #[test]
    fn validate_entry_path_rejects_null_bytes() {
        assert!(validate_entry_path("file\0.txt").is_err());
        assert!(validate_entry_path("\0").is_err());
    }

    #[test]
    fn validate_entry_path_rejects_dot_git() {
        assert!(validate_entry_path(".git").is_err());
        assert!(validate_entry_path(".git/config").is_err());
        assert!(validate_entry_path("foo/.git/hooks").is_err());
    }

    #[test]
    fn validate_entry_path_rejects_empty() {
        assert!(validate_entry_path("").is_err());
    }

    #[test]
    fn safe_join_produces_correct_path() {
        let dir = std::env::temp_dir();
        let result = safe_join(&dir, "src/main.rs").unwrap();
        assert_eq!(result, dir.join("src/main.rs"));
    }

    #[test]
    fn safe_join_rejects_traversal() {
        let dir = std::env::temp_dir();
        assert!(safe_join(&dir, "../../etc/passwd").is_err());
    }

    #[test]
    fn safe_join_rejects_absolute() {
        let dir = std::env::temp_dir();
        assert!(safe_join(&dir, "/etc/passwd").is_err());
    }

    #[test]
    fn validate_entry_path_allows_dot_segments_that_are_not_dot_dot() {
        assert!(validate_entry_path(".gitignore").is_ok());
        assert!(validate_entry_path("src/.gitkeep").is_ok());
        assert!(validate_entry_path("...").is_ok());
    }

    #[test]
    fn pull_downloads_files_missing_from_disk_despite_manifest_match() {
        let dir = tempfile::tempdir().unwrap();
        // Target directory is empty — no files on disk

        let mut manifest = Manifest::default();
        let files_map: HashMap<String, String> = [
            ("src/main.rs".to_string(), "abc123".to_string()),
            ("README.md".to_string(), "def456".to_string()),
        ]
        .into_iter()
        .collect();
        manifest.update("commit1".to_string(), files_map);

        // Simulated server entries with matching SHAs
        let server_entries: Vec<(&str, &str)> =
            vec![("src/main.rs", "abc123"), ("README.md", "def456")];

        let mut to_download = Vec::new();
        let mut unchanged_count: usize = 0;

        for (path, server_sha) in &server_entries {
            if let Some(local_sha) = manifest.file_sha(path)
                && server_sha == &local_sha
            {
                let file_path = safe_join(dir.path(), path).unwrap();
                if file_path.exists() {
                    unchanged_count += 1;
                    continue;
                }
            }
            to_download.push(*path);
        }

        // All files should be in to_download because none exist on disk
        assert_eq!(to_download.len(), 2);
        assert_eq!(unchanged_count, 0);
        assert!(to_download.contains(&"src/main.rs"));
        assert!(to_download.contains(&"README.md"));
    }

    #[test]
    fn pull_skips_files_present_on_disk_with_matching_sha() {
        let dir = tempfile::tempdir().unwrap();
        // Create the file on disk
        std::fs::create_dir_all(dir.path().join("src")).unwrap();
        std::fs::write(dir.path().join("src/main.rs"), "fn main() {}").unwrap();

        let mut manifest = Manifest::default();
        let files_map: HashMap<String, String> =
            [("src/main.rs".to_string(), "abc123".to_string())]
                .into_iter()
                .collect();
        manifest.update("commit1".to_string(), files_map);

        // Simulated server entry with matching SHA
        let server_entries: Vec<(&str, &str)> = vec![("src/main.rs", "abc123")];

        let mut to_download: Vec<&str> = Vec::new();
        let mut unchanged_count: usize = 0;

        for (path, server_sha) in &server_entries {
            if let Some(local_sha) = manifest.file_sha(path)
                && server_sha == &local_sha
            {
                let file_path = safe_join(dir.path(), path).unwrap();
                if file_path.exists() {
                    unchanged_count += 1;
                    continue;
                }
            }
            to_download.push(path);
        }

        // File exists and SHA matches — should be unchanged
        assert_eq!(unchanged_count, 1);
        assert!(to_download.is_empty());
    }

    #[tokio::test]
    #[serial]
    async fn pull_with_if_repo_set_and_identity_resolved_runs_normally() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join(".syns.yaml"),
            "owner: alice\nname: my-project\n",
        )
        .unwrap();
        std::env::set_current_dir(dir.path()).unwrap();
        unsafe { std::env::set_var("SYNS_CONFIG_DIR", dir.path()) };

        let mock_server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path(
                "/api/v1/repos/alice/my-project/tree",
            ))
            .and(wiremock::matchers::query_param("recursive", "true"))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "entries": [],
                    "commitSha": "abc123",
                    "truncated": false
                })),
            )
            .mount(&mock_server)
            .await;

        let config = Config::new(Some(&mock_server.uri())).unwrap();
        let output = Output::new(false);

        let result = cmd_pull(&config, &output, None, None, None, true).await;
        unsafe { std::env::remove_var("SYNS_CONFIG_DIR") };

        assert!(result.is_ok());
    }

    #[tokio::test]
    #[serial]
    async fn pull_with_if_repo_set_and_no_identity_skips_silently() {
        let dir = tempfile::tempdir().unwrap();
        std::env::set_current_dir(dir.path()).unwrap();
        unsafe { std::env::set_var("SYNS_CONFIG_DIR", dir.path()) };

        let mock_server = wiremock::MockServer::start().await;
        let config = Config::new(Some(&mock_server.uri())).unwrap();
        let output = Output::new(false);

        let result = cmd_pull(&config, &output, None, None, None, true).await;
        unsafe { std::env::remove_var("SYNS_CONFIG_DIR") };

        assert!(result.is_ok());
        assert!(mock_server.received_requests().await.unwrap().is_empty());
    }
}
