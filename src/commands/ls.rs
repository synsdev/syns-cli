use crate::client::{EntryType, SynsClient, TreeResponse};
use crate::config::Config;
use crate::errors::{CliError, partial_truncated_tree};
use crate::output::Output;
use crate::read::{
    ReadOptions, ReadTarget, bind_read_folder, counted_from, mark_partial, read_not_found,
    report_reference, repository_argument, resolve_read_target, with_reference,
};

/// `syns ls [PATH]` — the listing's columns and its ordering stand as
/// they stand, the resolved reference reaching the document and the
/// diagnostic line and no listing column (SPEC u270).
pub async fn cmd_ls(
    config: &Config,
    output: &Output,
    path: Option<String>,
    recursive: bool,
    opts: ReadOptions,
) -> Result<(), CliError> {
    // 1 — bind the folder and map the path positional under it (SPEC
    // u290), the typed path kept for every not-found refusal and the
    // folder's recorded path standing for it where nothing was typed.
    let folder = bind_read_folder(&opts)?;
    let mapped = repository_argument(folder.as_ref(), path.as_deref())?;
    let named = path
        .clone()
        .or_else(|| folder.as_ref().map(|f| f.path.clone()));

    // 2 — resolve the target.
    let Some(target) = resolve_read_target(config, output, &opts).await? else {
        return Ok(());
    };
    let client = SynsClient::new(config.server_url())?;

    // 3 — read the tree at that reference under the mapped path; a
    // path the tree read refuses as `not_found` answers the file of that
    // name its parent lists at the same commit, where one stands there
    // (issue 359).
    let listed = match list_tree(config, &client, &target, mapped.as_deref(), recursive).await {
        Err(e) if is_not_found(&e) => {
            match file_entry(config, &client, &target, mapped.as_deref()).await {
                Some(file) => Ok(file),
                None => Err(e),
            }
        }
        other => other,
    };
    let (mut response, mut raw) = match listed {
        Ok(tuple) => tuple,
        Err(e) => {
            if let Some(p) = named.as_ref() {
                if opts.version.is_some() {
                    return Err(read_not_found(e, &opts, &target.reference, p));
                }
                if !output.is_json() {
                    return Err(e.with_ls_path_context(p.clone()));
                }
            }
            return Err(e);
        }
    };

    // 4 — every entry's path counted from the folder, in the listing and
    // in the served body alike.
    let scope = target.folder.as_ref();
    for entry in &mut response.entries {
        entry.path = counted_from(scope, &entry.path);
    }
    if let Some(entries) = raw.get_mut("entries").and_then(|e| e.as_array_mut()) {
        for entry in entries {
            if let Some(served) = entry.get("path").and_then(|p| p.as_str()) {
                let counted = counted_from(scope, served);
                entry["path"] = serde_json::Value::from(counted);
            }
        }
    }

    response.entries.sort_by(|a, b| {
        let type_order = |t: &EntryType| match t {
            EntryType::Dir => 0,
            EntryType::File => 1,
            EntryType::Unknown => 2,
        };
        type_order(&a.entry_type)
            .cmp(&type_order(&b.entry_type))
            .then_with(|| {
                if recursive {
                    a.path.cmp(&b.path)
                } else {
                    a.name.cmp(&b.name)
                }
            })
    });

    // Render the listing, or the served body carrying the reference
    // and, where the tree arrived truncated, the partial mark.
    let truncated = response.truncated;
    let body = with_reference(raw, &target.reference);
    if !output.is_json() {
        let rows: Vec<Vec<String>> = response
            .entries
            .iter()
            .map(|e| {
                vec![
                    // A recursive listing tells its entries apart by
                    // path alone: two subtrees can hold one base name
                    // (u270 CR1-1).
                    if recursive {
                        e.path.clone()
                    } else {
                        e.name.clone()
                    },
                    match e.entry_type {
                        EntryType::File => "file",
                        EntryType::Dir => "dir",
                        EntryType::Unknown => "unknown",
                    }
                    .to_string(),
                    match e.size {
                        Some(n) => n.to_string(),
                        None => "-".to_string(),
                    },
                ]
            })
            .collect();
        output.table(&["Name", "Type", "Size"], rows);
        // Report the reference.
        report_reference(output, &target.reference);
    } else if !truncated {
        output.json(&body);
    }

    if truncated {
        let refusal = partial_truncated_tree(target.reference.version);
        return Err(CliError::PartialAnswer {
            document: mark_partial(body, &refusal),
            line: refusal,
        });
    }

    Ok(())
}

/// The tree at the run's reference under `path`: the closure at the
/// resolved commit, a `304` read from the store's trees; v1's listing
/// where the server answers no v2 (roadmap 279).
async fn list_tree(
    config: &Config,
    client: &SynsClient,
    target: &ReadTarget,
    path: Option<&str>,
    recursive: bool,
) -> Result<(TreeResponse, serde_json::Value), CliError> {
    let listed = crate::read::listing::list(
        client,
        &crate::read::store::StoreSpot::of(config, target.token.as_deref()),
        &target.repo_id,
        target.token.as_deref(),
        &target.reference.commit_sha,
        path,
        recursive,
    )
    .await?;
    match listed {
        Some(listed) => Ok(listed),
        None => {
            client
                .get_tree(
                    &target.repo_id,
                    target.token.as_deref(),
                    path,
                    recursive,
                    Some(&target.version_ref()),
                )
                .await
        }
    }
}

/// A `404` whose code is `not_found`: the answer the tree read gives a
/// path naming a file and a path naming nothing alike (`EP-tree`,
/// `EP-v2-tree`); `repo_not_found` and every other refusal stand.
fn is_not_found(err: &CliError) -> bool {
    matches!(err, CliError::Api { status: Some(404), error, .. } if error == "not_found")
}

/// The one entry of the file `path` names, from its parent's listing at
/// the run's commit, as `ls FILE` prints the file (issue 359); `--recursive`
/// answers it alike. None where the parent lists no file of that name, or
/// refuses, so the tree read's refusal stands. A path typed with a
/// trailing `/` asks for a folder and names no file.
async fn file_entry(
    config: &Config,
    client: &SynsClient,
    target: &ReadTarget,
    path: Option<&str>,
) -> Option<(TreeResponse, serde_json::Value)> {
    let typed = path?;
    if typed.ends_with('/') {
        return None;
    }
    let path = crate::read::as_server_reads(typed);
    let (parent, name) = match path.rsplit_once('/') {
        Some((parent, name)) => (Some(parent), name),
        None => (None, path),
    };
    if name.is_empty() {
        return None;
    }
    let (response, mut raw) = list_tree(config, client, target, parent, false)
        .await
        .ok()?;
    let entry = response
        .entries
        .into_iter()
        .find(|e| e.name == name && matches!(e.entry_type, EntryType::File))?;
    let served = raw
        .get("entries")
        .and_then(|e| e.as_array())
        .and_then(|entries| {
            entries.iter().find(|e| {
                e.get("name").and_then(|n| n.as_str()) == Some(name)
                    && e.get("type").and_then(|t| t.as_str()) == Some("file")
            })
        })
        .cloned()?;
    raw["entries"] = serde_json::Value::Array(vec![served]);
    raw["truncated"] = serde_json::Value::Bool(false);
    Some((
        TreeResponse {
            entries: vec![entry],
            commit_sha: response.commit_sha,
            truncated: false,
        },
        raw,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serial_test::serial;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    const HEAD_SHA: &str = "def4560000000000000000000000000000000000";

    /// Mounts the two addresses every read verb resolves through before
    /// it asks for any content (SPEC u270 `resolve_read_target`).
    async fn mount_reference(server: &MockServer, repo_id: &str) {
        Mock::given(method("GET"))
            .and(path(format!("/api/v1/repos/{repo_id}")))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "owner": "alice", "name": "my-project", "description": null,
                "commitSha": HEAD_SHA, "status": "active", "author": null, "tags": [],
                "visibility": "public", "forkedFrom": null, "forkCount": 0,
                "fileCount": 1, "role": null,
                "createdAt": "2026-01-01T00:00:00Z", "updatedAt": "2026-01-01T00:00:00Z",
            })))
            .mount(server)
            .await;
        Mock::given(method("GET"))
            .and(path(format!("/api/v1/repos/{repo_id}/versions/{HEAD_SHA}")))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "version": 7, "sha": HEAD_SHA, "parentSha": null, "message": "m",
                "messageBody": null, "author": "alice",
                "createdAt": "2026-01-01T00:00:00Z", "filesChanged": ["a.md"],
            })))
            .mount(server)
            .await;
    }

    fn here() -> ReadOptions {
        ReadOptions::default()
    }

    fn here_if_repo() -> ReadOptions {
        ReadOptions {
            if_repo: true,
            ..ReadOptions::default()
        }
    }

    #[tokio::test]
    #[serial]
    async fn ls_displays_sorted_entries() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join(".syns.yaml"),
            "owner: alice\nname: my-project\n",
        )
        .unwrap();
        std::env::set_current_dir(dir.path()).unwrap();
        unsafe { std::env::set_var("SYNS_CONFIG_DIR", dir.path()) };

        let mock_server = MockServer::start().await;
        mount_reference(&mock_server, "alice/my-project").await;

        Mock::given(method("GET"))
            .and(path("/api/v1/repos/alice/my-project/tree"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "entries": [
                    { "name": "README.md", "path": "README.md", "type": "file", "size": 256, "sha": "abc123" },
                    { "name": "src", "path": "src", "type": "dir", "size": null, "sha": null }
                ],
                "commitSha": "def456",
                "truncated": false
            })))
            .mount(&mock_server)
            .await;

        let config = Config::new(Some(&mock_server.uri())).unwrap();
        let output = Output::new(false);

        let result = cmd_ls(&config, &output, None, false, here()).await;
        unsafe { std::env::remove_var("SYNS_CONFIG_DIR") };

        assert!(result.is_ok());
    }

    #[tokio::test]
    #[serial]
    async fn ls_json_output() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join(".syns.yaml"),
            "owner: alice\nname: my-project\n",
        )
        .unwrap();
        std::env::set_current_dir(dir.path()).unwrap();
        unsafe { std::env::set_var("SYNS_CONFIG_DIR", dir.path()) };

        let mock_server = MockServer::start().await;
        mount_reference(&mock_server, "alice/my-project").await;

        Mock::given(method("GET"))
            .and(path("/api/v1/repos/alice/my-project/tree"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "entries": [
                    { "name": "README.md", "path": "README.md", "type": "file", "size": 256, "sha": "abc123" },
                    { "name": "src", "path": "src", "type": "dir", "size": null, "sha": null }
                ],
                "commitSha": "def456",
                "truncated": false
            })))
            .mount(&mock_server)
            .await;

        let config = Config::new(Some(&mock_server.uri())).unwrap();
        let output = Output::new(true);

        let result = cmd_ls(&config, &output, None, false, here()).await;
        unsafe { std::env::remove_var("SYNS_CONFIG_DIR") };

        assert!(result.is_ok());
    }

    #[tokio::test]
    #[serial]
    async fn ls_404_not_found_renders_friendly_message_with_path() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join(".syns.yaml"),
            "owner: alice\nname: my-project\n",
        )
        .unwrap();
        std::env::set_current_dir(dir.path()).unwrap();
        unsafe { std::env::set_var("SYNS_CONFIG_DIR", dir.path()) };

        let mock_server = MockServer::start().await;
        mount_reference(&mock_server, "alice/my-project").await;

        Mock::given(method("GET"))
            .and(path("/api/v1/repos/alice/my-project/tree/does/not/exist"))
            .respond_with(ResponseTemplate::new(404).set_body_json(serde_json::json!({
                "error": "not_found",
                "message": "File not found"
            })))
            .mount(&mock_server)
            .await;

        let config = Config::new(Some(&mock_server.uri())).unwrap();
        let output = Output::new(false);

        let result = cmd_ls(
            &config,
            &output,
            Some("does/not/exist".to_string()),
            false,
            here(),
        )
        .await;
        unsafe { std::env::remove_var("SYNS_CONFIG_DIR") };

        let err = result.unwrap_err();
        assert_eq!(err.to_string(), "path not found: does/not/exist");
        assert_eq!(err.exit_code(), 1);
    }

    #[tokio::test]
    #[serial]
    async fn ls_404_repo_not_found_preserves_generic_display() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join(".syns.yaml"),
            "owner: alice\nname: my-project\n",
        )
        .unwrap();
        std::env::set_current_dir(dir.path()).unwrap();
        unsafe { std::env::set_var("SYNS_CONFIG_DIR", dir.path()) };

        let mock_server = MockServer::start().await;
        mount_reference(&mock_server, "alice/my-project").await;

        Mock::given(method("GET"))
            .and(path("/api/v1/repos/alice/my-project/tree/some/path"))
            .respond_with(ResponseTemplate::new(404).set_body_json(serde_json::json!({
                "error": "repo_not_found",
                "message": "Repository not found"
            })))
            .mount(&mock_server)
            .await;

        let config = Config::new(Some(&mock_server.uri())).unwrap();
        let output = Output::new(false);

        let result = cmd_ls(
            &config,
            &output,
            Some("some/path".to_string()),
            false,
            here(),
        )
        .await;
        unsafe { std::env::remove_var("SYNS_CONFIG_DIR") };

        let err = result.unwrap_err();
        assert_eq!(err.to_string(), "server error (404): repo_not_found");
        assert_eq!(err.exit_code(), 1);
    }

    #[tokio::test]
    #[serial]
    async fn ls_404_not_found_in_json_mode_preserves_generic_display() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join(".syns.yaml"),
            "owner: alice\nname: my-project\n",
        )
        .unwrap();
        std::env::set_current_dir(dir.path()).unwrap();
        unsafe { std::env::set_var("SYNS_CONFIG_DIR", dir.path()) };

        let mock_server = MockServer::start().await;
        mount_reference(&mock_server, "alice/my-project").await;

        Mock::given(method("GET"))
            .and(path("/api/v1/repos/alice/my-project/tree/does/not/exist"))
            .respond_with(ResponseTemplate::new(404).set_body_json(serde_json::json!({
                "error": "not_found",
                "message": "File not found"
            })))
            .mount(&mock_server)
            .await;

        let config = Config::new(Some(&mock_server.uri())).unwrap();
        let output = Output::new(true);

        let result = cmd_ls(
            &config,
            &output,
            Some("does/not/exist".to_string()),
            false,
            here(),
        )
        .await;
        unsafe { std::env::remove_var("SYNS_CONFIG_DIR") };

        let err = result.unwrap_err();
        assert_eq!(err.to_string(), "server error (404): not_found");
        assert_eq!(err.exit_code(), 1);
    }

    #[tokio::test]
    #[serial]
    async fn ls_with_if_repo_set_and_identity_resolved_runs_normally() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join(".syns.yaml"),
            "owner: alice\nname: my-project\n",
        )
        .unwrap();
        std::env::set_current_dir(dir.path()).unwrap();
        unsafe { std::env::set_var("SYNS_CONFIG_DIR", dir.path()) };

        let mock_server = MockServer::start().await;
        mount_reference(&mock_server, "alice/my-project").await;

        Mock::given(method("GET"))
            .and(path("/api/v1/repos/alice/my-project/tree"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "entries": [
                    { "name": "README.md", "path": "README.md", "type": "file", "size": 256, "sha": "abc123" }
                ],
                "commitSha": "def456",
                "truncated": false
            })))
            .mount(&mock_server)
            .await;

        let config = Config::new(Some(&mock_server.uri())).unwrap();
        let output = Output::new(false);

        let result = cmd_ls(&config, &output, None, false, here_if_repo()).await;
        unsafe { std::env::remove_var("SYNS_CONFIG_DIR") };

        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn ls_get_tree_raw_includes_truncated_field() {
        let mock_server = MockServer::start().await;
        let body = r#"{"entries":[{"name":"README.md","path":"README.md","type":"file","size":256,"sha":"abc123"}],"commitSha":"def456","truncated":false}"#;
        Mock::given(method("GET"))
            .and(path("/api/v1/repos/alice/my-project/tree"))
            .respond_with(ResponseTemplate::new(200).set_body_string(body))
            .mount(&mock_server)
            .await;

        let client = SynsClient::new(&mock_server.uri()).unwrap();
        let (_typed, raw) = client
            .get_tree("alice/my-project", None, None, false, None)
            .await
            .unwrap();

        assert!(raw.get("entries").is_some() && raw["entries"].is_array());
        assert!(raw.get("commitSha").is_some());
        assert!(raw.get("truncated").is_some());
        assert_eq!(raw["truncated"], serde_json::json!(false));
        let expected: serde_json::Value = serde_json::from_str(body).unwrap();
        assert_eq!(raw, expected);
    }

    #[tokio::test]
    #[serial]
    async fn ls_with_if_repo_set_and_no_identity_skips_silently() {
        let dir = tempfile::tempdir().unwrap();
        std::env::set_current_dir(dir.path()).unwrap();
        unsafe { std::env::set_var("SYNS_CONFIG_DIR", dir.path()) };

        let mock_server = MockServer::start().await;
        let config = Config::new(Some(&mock_server.uri())).unwrap();
        let output = Output::new(false);

        let result = cmd_ls(&config, &output, None, false, here_if_repo()).await;
        unsafe { std::env::remove_var("SYNS_CONFIG_DIR") };

        assert!(result.is_ok());
        assert!(mock_server.received_requests().await.unwrap().is_empty());
    }
}
