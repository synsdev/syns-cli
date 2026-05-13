use crate::auth::token::TokenStore;
use crate::client::SynsClient;
use crate::config::Config;
use crate::errors::CliError;
use crate::output::Output;
use crate::repo::if_repo::resolve_full_or_skip;

pub async fn cmd_cat(
    config: &Config,
    output: &Output,
    path: String,
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

    let (response, raw) = match client
        .get_file(&repo_id, token.as_deref(), &path, None)
        .await
    {
        Ok(tuple) => tuple,
        Err(e) => {
            if !output.is_json() {
                return Err(e.with_cat_path_context(path.clone()));
            }
            return Err(e);
        }
    };

    if output.is_json() {
        output.json(&raw);
    } else {
        print!("{}", response.content);
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
    async fn cat_outputs_file_content() {
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
            .and(path("/api/v1/repos/alice/my-project/files/src/main.ts"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "content": "console.log('hello');",
                "sha": "abc123",
                "size": 21
            })))
            .mount(&mock_server)
            .await;

        let config = Config::new(Some(&mock_server.uri())).unwrap();
        let output = Output::new(false);

        let result = cmd_cat(&config, &output, "src/main.ts".to_string(), false).await;
        unsafe { std::env::remove_var("SYNS_CONFIG_DIR") };

        assert!(result.is_ok());
    }

    #[tokio::test]
    #[serial]
    async fn cat_json_output() {
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
            .and(path("/api/v1/repos/alice/my-project/files/src/main.ts"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "content": "console.log('hello');",
                "sha": "abc123",
                "size": 21
            })))
            .mount(&mock_server)
            .await;

        let config = Config::new(Some(&mock_server.uri())).unwrap();
        let output = Output::new(true);

        let result = cmd_cat(&config, &output, "src/main.ts".to_string(), false).await;
        unsafe { std::env::remove_var("SYNS_CONFIG_DIR") };

        assert!(result.is_ok());
    }

    #[tokio::test]
    #[serial]
    async fn cat_404_not_found_renders_friendly_message_with_path() {
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
            .and(path("/api/v1/repos/alice/my-project/files/src/missing.ts"))
            .respond_with(ResponseTemplate::new(404).set_body_json(serde_json::json!({
                "error": "not_found",
                "message": "File not found"
            })))
            .mount(&mock_server)
            .await;

        let config = Config::new(Some(&mock_server.uri())).unwrap();
        let output = Output::new(false);

        let result = cmd_cat(&config, &output, "src/missing.ts".to_string(), false).await;
        unsafe { std::env::remove_var("SYNS_CONFIG_DIR") };

        let err = result.unwrap_err();
        assert_eq!(err.to_string(), "file not found: src/missing.ts");
        assert_eq!(err.exit_code(), 1);
    }

    #[tokio::test]
    #[serial]
    async fn cat_404_repo_not_found_preserves_generic_display() {
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
            .and(path("/api/v1/repos/alice/my-project/files/README.md"))
            .respond_with(ResponseTemplate::new(404).set_body_json(serde_json::json!({
                "error": "repo_not_found",
                "message": "Repository not found"
            })))
            .mount(&mock_server)
            .await;

        let config = Config::new(Some(&mock_server.uri())).unwrap();
        let output = Output::new(false);

        let result = cmd_cat(&config, &output, "README.md".to_string(), false).await;
        unsafe { std::env::remove_var("SYNS_CONFIG_DIR") };

        let err = result.unwrap_err();
        assert_eq!(err.to_string(), "server error (404): repo_not_found");
        assert_eq!(err.exit_code(), 1);
    }

    #[tokio::test]
    #[serial]
    async fn cat_404_not_found_in_json_mode_preserves_generic_display() {
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
            .and(path("/api/v1/repos/alice/my-project/files/src/missing.ts"))
            .respond_with(ResponseTemplate::new(404).set_body_json(serde_json::json!({
                "error": "not_found",
                "message": "File not found"
            })))
            .mount(&mock_server)
            .await;

        let config = Config::new(Some(&mock_server.uri())).unwrap();
        let output = Output::new(true);

        let result = cmd_cat(&config, &output, "src/missing.ts".to_string(), false).await;
        unsafe { std::env::remove_var("SYNS_CONFIG_DIR") };

        let err = result.unwrap_err();
        assert_eq!(err.to_string(), "server error (404): not_found");
        assert_eq!(err.exit_code(), 1);
    }

    #[tokio::test]
    #[serial]
    async fn cat_with_if_repo_set_and_identity_resolved_runs_normally() {
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
            .and(path("/api/v1/repos/alice/my-project/files/src/main.ts"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "content": "console.log('hello');",
                "sha": "abc123",
                "size": 21
            })))
            .mount(&mock_server)
            .await;

        let config = Config::new(Some(&mock_server.uri())).unwrap();
        let output = Output::new(false);

        let result = cmd_cat(&config, &output, "src/main.ts".to_string(), true).await;
        unsafe { std::env::remove_var("SYNS_CONFIG_DIR") };

        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn cat_get_file_raw_includes_path_field() {
        let mock_server = MockServer::start().await;
        let body =
            r#"{"path":"src/main.ts","sha":"abc123","content":"console.log('hello');","size":21}"#;
        Mock::given(method("GET"))
            .and(path("/api/v1/repos/alice/my-project/files/src/main.ts"))
            .respond_with(ResponseTemplate::new(200).set_body_string(body))
            .mount(&mock_server)
            .await;

        let client = SynsClient::new(&mock_server.uri()).unwrap();
        let (_typed, raw) = client
            .get_file("alice/my-project", None, "src/main.ts", None)
            .await
            .unwrap();

        assert!(raw.get("path").is_some());
        assert_eq!(raw["path"], serde_json::json!("src/main.ts"));
        assert!(raw.get("content").is_some());
        assert!(raw.get("sha").is_some());
        assert!(raw.get("size").is_some());
        let expected: serde_json::Value = serde_json::from_str(body).unwrap();
        assert_eq!(raw, expected);
    }

    #[tokio::test]
    #[serial]
    async fn cat_with_if_repo_set_and_no_identity_skips_silently() {
        let dir = tempfile::tempdir().unwrap();
        std::env::set_current_dir(dir.path()).unwrap();
        unsafe { std::env::set_var("SYNS_CONFIG_DIR", dir.path()) };

        let mock_server = MockServer::start().await;
        let config = Config::new(Some(&mock_server.uri())).unwrap();
        let output = Output::new(false);

        let result = cmd_cat(&config, &output, "anything".to_string(), true).await;
        unsafe { std::env::remove_var("SYNS_CONFIG_DIR") };

        assert!(result.is_ok());
        assert!(mock_server.received_requests().await.unwrap().is_empty());
    }
}
