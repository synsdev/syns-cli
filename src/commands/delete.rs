use crate::auth::token::TokenStore;
use crate::client::SynsClient;
use crate::config::Config;
use crate::errors::CliError;
use crate::output::Output;
use crate::repo::resolve::resolve_repo_identity;
use console::style;
use serde_json::json;
use std::io::Write;

fn confirm_delete(repo_id: &str) -> Result<bool, CliError> {
    eprintln!(
        "{}",
        style(format!(
            "WARNING: This will permanently delete repository '{}' and all its contents.",
            repo_id
        ))
        .red()
        .bold()
    );
    eprintln!("This action cannot be undone.");
    eprint!("Type the repository name to confirm ('{}'): ", repo_id);
    std::io::stderr().flush().map_err(|e| CliError::Io {
        message: format!("could not read confirmation input: {e}"),
    })?;
    let mut input = String::new();
    std::io::stdin()
        .read_line(&mut input)
        .map_err(|e| CliError::Io {
            message: format!("could not read confirmation input: {e}"),
        })?;
    Ok(input.trim() == repo_id)
}

pub async fn cmd_delete(config: &Config, output: &Output, yes: bool) -> Result<(), CliError> {
    let current_dir = std::env::current_dir().map_err(|e| CliError::Io {
        message: format!("could not determine current directory: {e}"),
    })?;
    let identity = resolve_repo_identity(None, &current_dir)?;
    let owner = identity.owner.ok_or(CliError::RepoIdentityUnknown)?;
    let repo_id = format!("{}/{}", owner, identity.name);
    let token = TokenStore::new(config.credentials_path())
        .read()?
        .ok_or(CliError::AuthRequired)?;
    let client = SynsClient::new(config.server_url())?;

    if !yes && !confirm_delete(&repo_id)? {
        eprintln!("Aborted — input did not match repository name.");
        return Ok(());
    }

    client.delete_repo(&repo_id, &token).await?;

    if output.is_json() {
        output.json(&json!({"deleted": true, "repository": repo_id}));
    } else {
        output.success(&format!("Repository '{}' deleted.", repo_id));
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
    async fn delete_with_yes_flag() {
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

        Mock::given(method("DELETE"))
            .and(path("/api/v1/repos/alice/my-project"))
            .respond_with(ResponseTemplate::new(200))
            .mount(&mock_server)
            .await;

        let config = Config::new(Some(&mock_server.uri())).unwrap();
        let output = Output::new(false);

        let result = cmd_delete(&config, &output, true).await;
        unsafe { std::env::remove_var("SYNS_CONFIG_DIR") };

        assert!(result.is_ok());
    }
}
