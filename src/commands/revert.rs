use crate::auth::token::TokenStore;
use crate::client::{RevertFileRequest, SynsClient};
use crate::config::Config;
use crate::errors::CliError;
use crate::output::Output;
use crate::repo::resolve::resolve_repo_identity;
use serde_json::json;

pub async fn cmd_revert(config: &Config, output: &Output, path: String, to: String, message: Option<String>) -> Result<(), CliError> {
    let current_dir = std::env::current_dir()
        .map_err(|e| CliError::Io { message: format!("could not determine current directory: {e}") })?;
    let identity = resolve_repo_identity(None, &current_dir)?;
    let owner = identity.owner.ok_or(CliError::RepoIdentityUnknown)?;
    let repo_id = format!("{}/{}", owner, identity.name);
    let token = TokenStore::new(config.credentials_path())
        .read()?
        .ok_or(CliError::AuthRequired)?;
    let client = SynsClient::new(config.server_url())?;

    let request = RevertFileRequest { to: to.clone(), message: message.clone() };
    let response = client.revert_file(&repo_id, &token, &path, &request).await?;

    if output.is_json() {
        output.json(&json!({
            "commit_sha": response.commit_sha,
            "changed": response.changed,
            "path": path,
        }));
    } else if response.changed {
        output.success(&format!(
            "Reverted {} to {} (commit {})",
            path, to, &response.commit_sha[..response.commit_sha.len().min(8)]
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
        ).unwrap();
        TokenStore::new(dir.path().join("credentials.json"))
            .write("test-token")
            .unwrap();
        std::env::set_current_dir(dir.path()).unwrap();
        unsafe { std::env::set_var("SYNS_CONFIG_DIR", dir.path()) };

        let mock_server = MockServer::start().await;

        Mock::given(method("POST"))
            .and(path("/api/v1/repos/alice/my-project/files/src/config.ts/revert"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "commit_sha": "ddd44444ddd44444",
                "changed": true
            })))
            .mount(&mock_server)
            .await;

        let config = Config::new(Some(&mock_server.uri())).unwrap();
        let output = Output::new(false);

        let result = cmd_revert(&config, &output, "src/config.ts".to_string(), "3".to_string(), None).await;
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
        ).unwrap();
        // Do NOT write a credentials file
        std::env::set_current_dir(dir.path()).unwrap();
        unsafe { std::env::set_var("SYNS_CONFIG_DIR", dir.path()) };

        let mock_server = MockServer::start().await;
        let config = Config::new(Some(&mock_server.uri())).unwrap();
        let output = Output::new(false);

        let result = cmd_revert(&config, &output, "a.txt".to_string(), "1".to_string(), None).await;
        unsafe { std::env::remove_var("SYNS_CONFIG_DIR") };

        assert!(matches!(result, Err(CliError::AuthRequired)));
    }
}
