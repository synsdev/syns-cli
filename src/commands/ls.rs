use crate::auth::token::TokenStore;
use crate::client::{EntryType, SynsClient};
use crate::config::Config;
use crate::errors::CliError;
use crate::output::Output;
use crate::repo::if_repo::resolve_full_or_skip;

pub async fn cmd_ls(
    config: &Config,
    output: &Output,
    path: Option<String>,
    if_repo: bool,
) -> Result<(), CliError> {
    let current_dir = std::env::current_dir().map_err(|e| CliError::Io {
        message: format!("could not determine current directory: {e}"),
    })?;
    let (owner, name) = match resolve_full_or_skip(None, &current_dir, if_repo, output)? {
        Some(pair) => pair,
        None => return Ok(()),
    };
    let repo_id = format!("{owner}/{name}");
    let token = TokenStore::new(config.credentials_path())
        .read()
        .ok()
        .flatten();
    let client = SynsClient::new(config.server_url())?;

    let (mut response, raw) = match client
        .get_tree(&repo_id, token.as_deref(), path.as_deref(), false, None)
        .await
    {
        Ok(tuple) => tuple,
        Err(e) => {
            if !output.is_json()
                && let Some(p) = path.as_ref()
            {
                return Err(e.with_ls_path_context(p.clone()));
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
            .then_with(|| a.name.cmp(&b.name))
    });

    if output.is_json() {
        output.json(&raw);
    } else {
        let rows: Vec<Vec<String>> = response
            .entries
            .iter()
            .map(|e| {
                vec![
                    e.name.clone(),
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
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serial_test::serial;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

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

        let result = cmd_ls(&config, &output, None, false).await;
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

        let result = cmd_ls(&config, &output, None, false).await;
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

        let result = cmd_ls(&config, &output, Some("does/not/exist".to_string()), false).await;
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

        let result = cmd_ls(&config, &output, Some("some/path".to_string()), false).await;
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

        let result = cmd_ls(&config, &output, Some("does/not/exist".to_string()), false).await;
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

        let result = cmd_ls(&config, &output, None, true).await;
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

        let result = cmd_ls(&config, &output, None, true).await;
        unsafe { std::env::remove_var("SYNS_CONFIG_DIR") };

        assert!(result.is_ok());
        assert!(mock_server.received_requests().await.unwrap().is_empty());
    }
}
