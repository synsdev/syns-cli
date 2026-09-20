use crate::client::{EntryType, SynsClient};
use crate::config::Config;
use crate::errors::{CliError, partial_truncated_tree};
use crate::output::Output;
use crate::read::{
    ReadOptions, mark_partial, read_not_found, report_reference, resolve_read_target,
    with_reference,
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
    // 1 — resolve the target.
    let Some(target) = resolve_read_target(config, output, &opts).await? else {
        return Ok(());
    };
    let client = SynsClient::new(config.server_url())?;

    // 2 — read the tree at that reference under the path positional.
    let version_ref = target.version_ref();
    let (mut response, raw) = match client
        .get_tree(
            &target.repo_id,
            target.token.as_deref(),
            path.as_deref(),
            recursive,
            Some(&version_ref),
        )
        .await
    {
        Ok(tuple) => tuple,
        Err(e) => {
            if let Some(p) = path.as_ref() {
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

    // 3 — render the listing, or the served body carrying the reference
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
        // 4 — report the reference.
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
