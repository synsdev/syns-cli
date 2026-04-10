use crate::auth::token::TokenStore;
use crate::client::{ForkRequest, SynsClient};
use crate::config::Config;
use crate::errors::CliError;
use crate::output::Output;
use crate::repo::syns_yaml::write_syns_yaml;
use console::style;
use serde_json::json;

fn validate_repo_id(repo: &str) -> Result<(), CliError> {
    let parts: Vec<&str> = repo.split('/').collect();
    if parts.len() != 2 || parts[0].is_empty() || parts[1].is_empty() {
        return Err(CliError::Config {
            message: "invalid repository: must be in owner/name format (e.g., alice/my-project)".into(),
        });
    }
    Ok(())
}

fn parse_fork_identity(id: &str) -> Option<(&str, &str)> {
    let parts: Vec<&str> = id.splitn(2, '/').collect();
    if parts.len() == 2 && !parts[0].is_empty() && !parts[1].is_empty() {
        Some((parts[0], parts[1]))
    } else {
        None
    }
}

pub async fn cmd_fork(
    config: &Config,
    output: &Output,
    repo: String,
    name: Option<String>,
) -> Result<(), CliError> {
    validate_repo_id(&repo)?;

    let token = TokenStore::new(config.credentials_path())
        .read()?
        .ok_or(CliError::AuthRequired)?;

    let client = SynsClient::new(config.server_url())?;

    let request = ForkRequest { target_name: name };

    let response = client.fork(&repo, &token, &request).await?;

    let fork_identity = parse_fork_identity(&response.id);

    if output.is_json() {
        output.json(&json!({
            "id": response.id,
            "commit_sha": response.commit_sha,
            "file_count": response.file_count,
            "commit_count": response.commit_count,
        }));
    } else {
        output.success(&format!("Forked {} → {}", repo, response.id));
        eprintln!("{}", style(format!(
            "{} files, {} commits", response.file_count, response.commit_count
        )).dim());
    }

    let cwd = match std::env::current_dir() {
        Ok(d) => d,
        Err(_) => {
            eprintln!("warning: could not determine current directory, .syns.yaml not updated");
            return Ok(());
        }
    };

    if let Some((fork_owner, fork_name)) = fork_identity {
        if let Err(err) = write_syns_yaml(&cwd, fork_owner, fork_name) {
            eprintln!("warning: could not update .syns.yaml: {err}");
            return Ok(());
        }
        if !output.is_json() {
            eprintln!("{}", style(format!(
                "Updated .syns.yaml → {}/{}", fork_owner, fork_name
            )).dim());
        }
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
    async fn fork_json_output() {
        let dir = tempfile::tempdir().unwrap();
        TokenStore::new(dir.path().join("credentials.json"))
            .write("test-token")
            .unwrap();
        std::env::set_current_dir(dir.path()).unwrap();
        unsafe { std::env::set_var("SYNS_CONFIG_DIR", dir.path()) };

        let mock_server = MockServer::start().await;

        Mock::given(method("POST"))
            .and(path("/api/v1/repos/alice/my-project/fork"))
            .respond_with(ResponseTemplate::new(201).set_body_json(serde_json::json!({
                "id": "bob/my-project",
                "commit_sha": "abc123",
                "file_count": 5,
                "commit_count": 3
            })))
            .mount(&mock_server)
            .await;

        let config = Config::new(Some(&mock_server.uri())).unwrap();
        let output = Output::new(true);

        let result = cmd_fork(&config, &output, "alice/my-project".into(), None).await;
        assert!(result.is_ok());

        let yaml_path = dir.path().join(".syns.yaml");
        assert!(yaml_path.exists());
        let contents = std::fs::read_to_string(&yaml_path).unwrap();
        assert!(contents.contains("owner: bob"));
        assert!(contents.contains("name: my-project"));

        unsafe { std::env::remove_var("SYNS_CONFIG_DIR") };
    }

    #[tokio::test]
    #[serial]
    async fn fork_invalid_repo_format() {
        let dir = tempfile::tempdir().unwrap();
        unsafe { std::env::set_var("SYNS_CONFIG_DIR", dir.path()) };

        let config = Config::new(Some("http://localhost:1")).unwrap();
        let output = Output::new(false);

        let result = cmd_fork(&config, &output, "no-slash".into(), None).await;
        assert!(result.is_err());
        assert!(format!("{:?}", result.unwrap_err()).contains("invalid repository"));

        unsafe { std::env::remove_var("SYNS_CONFIG_DIR") };
    }

    #[tokio::test]
    #[serial]
    async fn fork_no_auth() {
        let dir = tempfile::tempdir().unwrap();
        unsafe { std::env::set_var("SYNS_CONFIG_DIR", dir.path()) };

        let config = Config::new(Some("http://localhost:1")).unwrap();
        let output = Output::new(false);

        let result = cmd_fork(&config, &output, "alice/my-project".into(), None).await;
        assert!(result.is_err());
        assert!(format!("{:?}", result.unwrap_err()).contains("AuthRequired"));

        unsafe { std::env::remove_var("SYNS_CONFIG_DIR") };
    }
}
