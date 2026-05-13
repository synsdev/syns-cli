use crate::auth::token::TokenStore;
use crate::client::{RevertFileRequest, SynsClient};
use crate::config::Config;
use crate::errors::CliError;
use crate::output::Output;
use crate::repo::if_repo::resolve_full_or_skip;

pub async fn cmd_revert(
    config: &Config,
    output: &Output,
    path: String,
    to: String,
    message: Option<String>,
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
        .read()?
        .ok_or(CliError::AuthRequired)?;
    let client = SynsClient::new(config.server_url())?;

    let request = RevertFileRequest {
        to: to.clone(),
        message: message.clone(),
    };
    let (response, raw) = client
        .revert_file(&repo_id, &token, &path, &request)
        .await?;

    if output.is_json() {
        output.json(&raw);
    } else if response.files_changed > 0 {
        output.success(&format!(
            "Reverted {} to {} (commit {})",
            path,
            to,
            &response.commit_sha[..response.commit_sha.len().min(8)]
        ));
    } else {
        output.success(&format!(
            "File {} already matches version {}, no changes made",
            path, to
        ));
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
    async fn revert_creates_new_commit() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join(".syns.yaml"),
            "owner: alice\nname: my-project\n",
        )
        .unwrap();
        TokenStore::new(dir.path().join("credentials.json"))
            .write("test-token")
            .unwrap();
        std::env::set_current_dir(dir.path()).unwrap();
        unsafe { std::env::set_var("SYNS_CONFIG_DIR", dir.path()) };

        let mock_server = MockServer::start().await;

        Mock::given(method("POST"))
            .and(path(
                "/api/v1/repos/alice/my-project/files/src/config.ts/revert",
            ))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "commitSha": "ddd44444ddd44444",
                "version": 4,
                "filesChanged": 1,
                "created": false
            })))
            .mount(&mock_server)
            .await;

        let config = Config::new(Some(&mock_server.uri())).unwrap();
        let output = Output::new(false);

        let result = cmd_revert(
            &config,
            &output,
            "src/config.ts".to_string(),
            "3".to_string(),
            None,
            false,
        )
        .await;
        unsafe { std::env::remove_var("SYNS_CONFIG_DIR") };

        assert!(result.is_ok());
    }

    #[tokio::test]
    #[serial]
    async fn revert_requires_authentication() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join(".syns.yaml"),
            "owner: alice\nname: my-project\n",
        )
        .unwrap();
        // Do NOT write a credentials file
        std::env::set_current_dir(dir.path()).unwrap();
        unsafe { std::env::set_var("SYNS_CONFIG_DIR", dir.path()) };

        let mock_server = MockServer::start().await;
        let config = Config::new(Some(&mock_server.uri())).unwrap();
        let output = Output::new(false);

        let result = cmd_revert(
            &config,
            &output,
            "a.txt".to_string(),
            "1".to_string(),
            None,
            false,
        )
        .await;
        unsafe { std::env::remove_var("SYNS_CONFIG_DIR") };

        assert!(matches!(result, Err(CliError::AuthRequired)));
    }

    #[tokio::test]
    #[serial]
    async fn revert_with_if_repo_set_and_identity_resolved_runs_normally() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join(".syns.yaml"),
            "owner: alice\nname: my-project\n",
        )
        .unwrap();
        TokenStore::new(dir.path().join("credentials.json"))
            .write("test-token")
            .unwrap();
        std::env::set_current_dir(dir.path()).unwrap();
        unsafe { std::env::set_var("SYNS_CONFIG_DIR", dir.path()) };

        let mock_server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path(
                "/api/v1/repos/alice/my-project/files/src/config.ts/revert",
            ))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "commitSha": "ddd44444ddd44444",
                "version": 4,
                "filesChanged": 1,
                "created": false
            })))
            .mount(&mock_server)
            .await;

        let config = Config::new(Some(&mock_server.uri())).unwrap();
        let output = Output::new(false);

        let result = cmd_revert(
            &config,
            &output,
            "src/config.ts".to_string(),
            "3".to_string(),
            None,
            true,
        )
        .await;
        unsafe { std::env::remove_var("SYNS_CONFIG_DIR") };

        assert!(result.is_ok());
    }

    #[tokio::test]
    #[serial]
    async fn revert_with_if_repo_set_and_no_identity_skips_silently() {
        let dir = tempfile::tempdir().unwrap();
        std::env::set_current_dir(dir.path()).unwrap();
        unsafe { std::env::set_var("SYNS_CONFIG_DIR", dir.path()) };

        let mock_server = MockServer::start().await;
        let config = Config::new(Some(&mock_server.uri())).unwrap();
        let output = Output::new(false);

        let result = cmd_revert(
            &config,
            &output,
            "foo".to_string(),
            "1".to_string(),
            None,
            true,
        )
        .await;
        unsafe { std::env::remove_var("SYNS_CONFIG_DIR") };

        assert!(result.is_ok());
        assert!(mock_server.received_requests().await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn revert_raw_emits_server_wire_shape_without_synthetic_path() {
        let mock_server = MockServer::start().await;
        let body = r#"{"commitSha":"def4567890abcdef0123456789abcdef01234567","version":5,"filesChanged":1,"created":false}"#;
        Mock::given(method("POST"))
            .and(path(
                "/api/v1/repos/alice/my-project/files/src/main.ts/revert",
            ))
            .respond_with(ResponseTemplate::new(200).set_body_string(body))
            .mount(&mock_server)
            .await;

        let client = SynsClient::new(&mock_server.uri()).unwrap();
        let request = RevertFileRequest {
            to: "3".into(),
            message: None,
        };
        let (typed, raw) = client
            .revert_file("alice/my-project", "test-token", "src/main.ts", &request)
            .await
            .unwrap();

        assert_eq!(raw.as_object().unwrap().keys().count(), 4);
        assert!(raw.get("path").is_none());
        assert_eq!(
            raw["commitSha"],
            serde_json::json!("def4567890abcdef0123456789abcdef01234567")
        );
        assert_eq!(raw["filesChanged"], serde_json::json!(1));
        let expected: serde_json::Value = serde_json::from_str(body).unwrap();
        assert_eq!(raw, expected);
        assert!(typed.commit_sha.starts_with("def4567890"));
    }
}
