use crate::auth::token::TokenStore;
use crate::client::{AddCollaboratorRequest, CollaboratorRole, SynsClient};
use crate::config::Config;
use crate::errors::CliError;
use crate::output::Output;
use crate::repo::resolve::resolve_repo_identity;
use clap::Subcommand;
use serde_json::json;
use std::io::Write;

#[derive(clap::ValueEnum, Debug, Clone)]
pub enum AssignableRole {
    Admin,
    Write,
    Read,
}

impl AssignableRole {
    fn as_str(&self) -> &str {
        match self {
            AssignableRole::Admin => "admin",
            AssignableRole::Write => "write",
            AssignableRole::Read => "read",
        }
    }
}

impl From<AssignableRole> for CollaboratorRole {
    fn from(r: AssignableRole) -> Self {
        match r {
            AssignableRole::Admin => CollaboratorRole::Admin,
            AssignableRole::Write => CollaboratorRole::Write,
            AssignableRole::Read => CollaboratorRole::Read,
        }
    }
}

#[derive(Subcommand, Debug)]
pub enum CollaboratorsAction {
    /// Add a collaborator to the repository
    Add {
        /// User ID of the collaborator to add
        #[arg()]
        user_id: String,
        /// Role to assign
        #[arg(long)]
        role: AssignableRole,
    },
    /// Remove a collaborator from the repository
    Remove {
        /// User ID of the collaborator to remove
        #[arg()]
        user_id: String,
        /// Skip confirmation prompt
        #[arg(long, short)]
        yes: bool,
    },
}

const DEFAULT_COLLABORATOR_LIMIT: u32 = 100;

fn confirm_remove(user_id: &str, repo_id: &str) -> Result<bool, CliError> {
    eprint!("Remove collaborator '{}' from '{}'? [y/N]: ", user_id, repo_id);
    std::io::stderr().flush().map_err(|e| CliError::Io {
        message: format!("could not read confirmation input: {e}"),
    })?;
    let mut input = String::new();
    std::io::stdin().read_line(&mut input).map_err(|e| CliError::Io {
        message: format!("could not read confirmation input: {e}"),
    })?;
    let trimmed = input.trim().to_lowercase();
    Ok(trimmed == "y" || trimmed == "yes")
}

pub async fn cmd_collaborators(
    config: &Config,
    output: &Output,
    action: Option<CollaboratorsAction>,
) -> Result<(), CliError> {
    let current_dir = std::env::current_dir()
        .map_err(|e| CliError::Io { message: format!("could not determine current directory: {e}") })?;
    let identity = resolve_repo_identity(None, &current_dir)?;
    let owner = identity.owner.ok_or(CliError::RepoIdentityUnknown)?;
    let repo_id = format!("{}/{}", owner, identity.name);
    let client = SynsClient::new(config.server_url())?;

    match action {
        None => {
            let token = TokenStore::new(config.credentials_path()).read().ok().flatten();
            let response = client.list_collaborators(&repo_id, token.as_deref(), DEFAULT_COLLABORATOR_LIMIT, 0).await?;
            if output.is_json() {
                output.json(&json!({
                    "collaborators": response.collaborators.iter().map(|c| json!({
                        "user_id": c.user_id,
                        "name": c.name,
                        "email": c.email,
                        "role": format!("{:?}", c.role).to_lowercase(),
                    })).collect::<Vec<_>>(),
                    "total": response.total,
                }));
            } else {
                let rows = response.collaborators.iter().map(|c| {
                    vec![
                        c.user_id.clone(),
                        c.name.clone(),
                        c.email.clone(),
                        format!("{:?}", c.role).to_lowercase(),
                    ]
                }).collect();
                output.table(&["User ID", "Name", "Email", "Role"], rows);
                if response.total as usize > response.collaborators.len() {
                    eprintln!("Showing {} of {} collaborators.", response.collaborators.len(), response.total);
                }
            }
        }
        Some(CollaboratorsAction::Add { user_id, role }) => {
            let token = TokenStore::new(config.credentials_path())
                .read()?
                .ok_or(CliError::AuthRequired)?;
            let role_str = role.as_str().to_string();
            let request = AddCollaboratorRequest { user_id: user_id.clone(), role: role.into() };
            client.add_collaborator(&repo_id, &token, &request).await?;
            if output.is_json() {
                output.json(&json!({"added": true, "user_id": user_id, "role": role_str}));
            } else {
                output.success(&format!("Added '{}' as {} collaborator.", user_id, role_str));
            }
        }
        Some(CollaboratorsAction::Remove { user_id, yes }) => {
            let token = TokenStore::new(config.credentials_path())
                .read()?
                .ok_or(CliError::AuthRequired)?;
            if !yes && !confirm_remove(&user_id, &repo_id)? {
                eprintln!("Aborted.");
                return Ok(());
            }
            client.remove_collaborator(&repo_id, &token, &user_id).await?;
            if output.is_json() {
                output.json(&json!({"removed": true, "user_id": user_id}));
            } else {
                output.success(&format!("Removed collaborator '{}'.", user_id));
            }
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
    async fn collaborators_add_sends_correct_request() {
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
            .and(path("/api/v1/repos/alice/my-project/collaborators"))
            .respond_with(ResponseTemplate::new(200))
            .mount(&mock_server)
            .await;

        let config = Config::new(Some(&mock_server.uri())).unwrap();
        let output = Output::new(false);

        let result = cmd_collaborators(
            &config,
            &output,
            Some(CollaboratorsAction::Add {
                user_id: "bob-123".to_string(),
                role: AssignableRole::Write,
            }),
        ).await;
        unsafe { std::env::remove_var("SYNS_CONFIG_DIR") };

        assert!(result.is_ok());
    }

    #[tokio::test]
    #[serial]
    async fn collaborators_remove_with_yes_flag() {
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

        Mock::given(method("DELETE"))
            .and(path("/api/v1/repos/alice/my-project/collaborators/bob-123"))
            .respond_with(ResponseTemplate::new(200))
            .mount(&mock_server)
            .await;

        let config = Config::new(Some(&mock_server.uri())).unwrap();
        let output = Output::new(false);

        let result = cmd_collaborators(
            &config,
            &output,
            Some(CollaboratorsAction::Remove {
                user_id: "bob-123".to_string(),
                yes: true,
            }),
        ).await;
        unsafe { std::env::remove_var("SYNS_CONFIG_DIR") };

        assert!(result.is_ok());
    }
}
