use crate::auth::token::TokenStore;
use crate::client::{EntryType, SynsClient};
use crate::config::Config;
use crate::errors::CliError;
use crate::output::Output;
use crate::push::manifest::Manifest;
use crate::repo::resolve::resolve_repo_identity;
use crate::repo::syns_yaml::write_syns_yaml;
use console::style;
use serde_json::json;
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;

pub async fn cmd_pull(
    config: &Config,
    output: &Output,
    repo_arg: Option<String>,
    path_arg: Option<String>,
    version: Option<String>,
) -> Result<(), CliError> {
    let target_dir = match &path_arg {
        Some(p) => PathBuf::from(p),
        None => std::env::current_dir()
            .map_err(|e| CliError::Io { message: format!("could not determine current directory: {e}") })?,
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
        let identity = resolve_repo_identity(None, &target_dir)?;
        let owner = identity.owner.ok_or(CliError::RepoIdentityUnknown)?;
        (owner, identity.name)
    };

    let repo_id = format!("{owner}/{name}");
    let token = TokenStore::new(config.credentials_path()).read().ok().flatten();
    let client = SynsClient::new(config.server_url())?;

    let tree_response = if version.is_some() {
        client.get_tree(&repo_id, token.as_deref(), None, true, version.as_deref()).await?
    } else {
        client.pull(&repo_id, token.as_deref()).await?
    };

    let server_files: Vec<_> = tree_response.entries.iter()
        .filter(|e| e.entry_type == EntryType::File)
        .collect();

    let mut manifest = Manifest::load(config.cache_dir(), &owner, &name)
        .unwrap_or_default();

    let server_paths: HashSet<String> = server_files.iter().map(|e| e.path.clone()).collect();
    let mut to_download = Vec::new();
    let mut unchanged_count: usize = 0;

    for entry in &server_files {
        if version.is_none() {
            if let Some(server_sha) = &entry.sha {
                if let Some(local_sha) = manifest.file_sha(&entry.path) {
                    if server_sha == local_sha {
                        unchanged_count += 1;
                        continue;
                    }
                }
            }
        }
        to_download.push(*entry);
    }

    let to_delete: Vec<String> = if version.is_none() {
        manifest.file_paths()
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
        let response = client.get_file(&repo_id, token.as_deref(), &entry.path, version.as_deref()).await?;
        let file_path = target_dir.join(&entry.path);
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
        let file_path = target_dir.join(path);
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
        let files_map: HashMap<String, String> = server_files.iter()
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
            "commit_sha": tree_response.commit_sha,
            "downloaded": downloaded,
            "unchanged": unchanged_count,
            "deleted": deleted,
        });
        if let Some(ref v) = version {
            summary.as_object_mut().unwrap().insert("version".into(), json!(v));
        }
        output.json(&summary);
    } else {
        output.success(&format!(
            "Pulled {repo_id}: {downloaded} downloaded, {unchanged_count} unchanged, {deleted} deleted"
        ));
    }

    Ok(())
}
