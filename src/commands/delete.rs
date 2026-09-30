use crate::auth::token::TokenStore;
use crate::client::SynsClient;
use crate::config::Config;
use crate::errors::CliError;
use crate::output::Output;
use crate::prompts::{ConfirmOutcome, confirm_or_yes};
use crate::repo::folder::{current_dir, refuse_holder_change};
use crate::repo::if_repo::resolve_full_or_skip;
use console::style;
use serde_json::json;

fn confirm_delete(repo_id: &str, yes: bool) -> Result<bool, CliError> {
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
    let prompt = format!("Type the repository name to confirm ('{}'): ", repo_id);
    match confirm_or_yes(yes, &prompt)? {
        ConfirmOutcome::SkipPrompt => Ok(true),
        ConfirmOutcome::Input(input) => Ok(input.trim() == repo_id),
    }
}

pub async fn cmd_delete(
    config: &Config,
    output: &Output,
    yes: bool,
    if_repo: bool,
) -> Result<(), CliError> {
    let current_dir = current_dir()?;
    // Inside a scoped folder the deletion acts on the holder, so it is
    // refused before the identity, the credential, the typed
    // confirmation and the request (SPEC u290, `D-102`).
    refuse_holder_change(&current_dir, "syns delete")?;
    let (owner, name) = match resolve_full_or_skip(None, &current_dir, if_repo, output)? {
        Some(pair) => pair,
        None => return Ok(()),
    };
    let repo_id = format!("{owner}/{name}");
    let token = TokenStore::new(config.credentials_path())
        .read()?
        .ok_or(CliError::AuthRequired)?;
    let client = SynsClient::new(config.server_url())?;

    if !confirm_delete(&repo_id, yes)? {
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

    /// A checkout `W` naming `alice/work` with the folder
    /// `W/clients/q3` recording its place, a stored credential in `W`,
    /// and the run standing inside the folder.
    fn inside_a_folder() -> (tempfile::TempDir, std::path::PathBuf) {
        let w = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(w.path()).unwrap();
        std::fs::write(root.join(".syns.yaml"), "owner: alice\nname: work\n").unwrap();
        let folder = root.join("clients").join("q3");
        std::fs::create_dir_all(&folder).unwrap();
        std::fs::write(
            folder.join(".syns.yaml"),
            "holder: alice/work\npath: clients/q3\n",
        )
        .unwrap();
        TokenStore::new(root.join("credentials.json"))
            .write("test-token")
            .unwrap();
        std::env::set_current_dir(&folder).unwrap();
        unsafe { std::env::set_var("SYNS_CONFIG_DIR", &root) };
        (w, folder)
    }

    // SPEC u290 Behaviour, `cmd_delete` 1 (`D-102`).
    #[tokio::test]
    #[serial]
    async fn delete_inside_a_folder_is_refused_before_any_request() {
        let (_w, folder) = inside_a_folder();
        let server = MockServer::start().await;
        let config = Config::new(Some(&server.uri())).unwrap();
        let output = Output::new(false);

        let result = cmd_delete(&config, &output, true, true).await;
        unsafe { std::env::remove_var("SYNS_CONFIG_DIR") };

        match result {
            Err(CliError::HolderActing {
                command,
                holder,
                dir,
            }) => {
                assert_eq!(command, "syns delete");
                assert_eq!(holder, "alice/work");
                assert_eq!(dir, folder);
            }
            other => panic!("expected the holder-acting refusal, got {other:?}"),
        }
        assert!(server.received_requests().await.unwrap().is_empty());
    }

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

        let result = cmd_delete(&config, &output, true, false).await;
        unsafe { std::env::remove_var("SYNS_CONFIG_DIR") };

        assert!(result.is_ok());
    }

    #[tokio::test]
    #[serial]
    async fn delete_with_if_repo_set_and_identity_resolved_runs_normally() {
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

        let result = cmd_delete(&config, &output, true, true).await;
        unsafe { std::env::remove_var("SYNS_CONFIG_DIR") };

        assert!(result.is_ok());
    }

    #[tokio::test]
    #[serial]
    async fn delete_with_if_repo_set_and_no_identity_skips_silently() {
        let dir = tempfile::tempdir().unwrap();
        std::env::set_current_dir(dir.path()).unwrap();
        unsafe { std::env::set_var("SYNS_CONFIG_DIR", dir.path()) };

        let mock_server = MockServer::start().await;
        let config = Config::new(Some(&mock_server.uri())).unwrap();
        let output = Output::new(false);

        let result = cmd_delete(&config, &output, false, true).await;
        unsafe { std::env::remove_var("SYNS_CONFIG_DIR") };

        assert!(result.is_ok());
        assert!(mock_server.received_requests().await.unwrap().is_empty());
    }
}
