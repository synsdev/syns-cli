use crate::auth::token::TokenStore;
use crate::client::{AddCollaboratorRequest, CollaboratorRole, SynsClient};
use crate::config::Config;
use crate::errors::CliError;
use crate::output::Output;
use crate::prompts::{ConfirmOutcome, confirm_or_yes};
use crate::repo::if_repo::resolve_full_or_skip;
use clap::Subcommand;
use serde_json::json;

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
        /// Username or email of the user to add
        #[arg()]
        target: String,
        /// Role to assign
        #[arg(long)]
        role: AssignableRole,
        /// Silently skip (exit 0) when no Syns repo identity resolves
        #[arg(long)]
        if_repo: bool,
    },
    /// Remove a collaborator from the repository
    Remove {
        /// User ID of the collaborator to remove
        #[arg()]
        user_id: String,
        /// Skip confirmation prompt
        #[arg(long, short)]
        yes: bool,
        /// Silently skip (exit 0) when no Syns repo identity resolves
        #[arg(long)]
        if_repo: bool,
    },
}

const DEFAULT_COLLABORATOR_LIMIT: u32 = 100;

fn confirm_remove(user_id: &str, repo_id: &str, yes: bool) -> Result<bool, CliError> {
    let prompt = format!(
        "Remove collaborator '{}' from '{}'? [y/N]: ",
        user_id, repo_id
    );
    match confirm_or_yes(yes, &prompt)? {
        ConfirmOutcome::SkipPrompt => Ok(true),
        ConfirmOutcome::Input(input) => {
            let trimmed = input.trim().to_lowercase();
            Ok(trimmed == "y" || trimmed == "yes")
        }
    }
}

pub async fn cmd_collaborators(
    config: &Config,
    output: &Output,
    action: Option<CollaboratorsAction>,
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
    let client = SynsClient::new(config.server_url())?;

    match action {
        None => {
            let token = TokenStore::new(config.credentials_path())
                .read()
                .ok()
                .flatten();
            let (response, raw) = client
                .list_collaborators(&repo_id, token.as_deref(), DEFAULT_COLLABORATOR_LIMIT, 0)
                .await?;
            if output.is_json() {
                output.json(&raw);
            } else {
                let rows = response
                    .data
                    .iter()
                    .map(|c| {
                        vec![
                            c.user.id.clone(),
                            c.user.name.clone(),
                            c.user.email.clone(),
                            format!("{:?}", c.role).to_lowercase(),
                        ]
                    })
                    .collect();
                output.table(&["User ID", "Name", "Email", "Role"], rows);
                if response.total as usize > response.data.len() {
                    eprintln!(
                        "Showing {} of {} collaborators.",
                        response.data.len(),
                        response.total
                    );
                }
            }
        }
        Some(CollaboratorsAction::Add { target, role, .. }) => {
            let token = TokenStore::new(config.credentials_path())
                .read()?
                .ok_or(CliError::AuthRequired)?;
            let trimmed = target.trim();
            if trimmed.is_empty() {
                return Err(CliError::Config {
                    message: "target cannot be empty — provide a username or email".to_string(),
                });
            }
            // Compute role_str (borrows) BEFORE role.into() (moves) — order matters.
            let role_str = role.as_str().to_string();
            let request_role: CollaboratorRole = role.into();
            let request = if trimmed.contains('@') {
                AddCollaboratorRequest::Email {
                    email: trimmed.to_string(),
                    role: request_role,
                }
            } else {
                AddCollaboratorRequest::Username {
                    username: trimmed.to_string(),
                    role: request_role,
                }
            };
            let result = client.add_collaborator(&repo_id, &token, &request).await;
            match result {
                Ok(()) => {
                    if output.is_json() {
                        output.json(&json!({
                            "added": true,
                            "target": trimmed,
                            "role": role_str,
                        }));
                    } else {
                        output.success(&format!(
                            "Added '{}' as {} collaborator.",
                            trimmed, role_str
                        ));
                    }
                }
                Err(CliError::Api {
                    status: Some(404),
                    error,
                    context: None,
                }) if matches!(
                    error.as_str(),
                    "not_found_user_by_username"
                        | "not_found_user_by_email"
                        | "not_found_user_by_id"
                ) => {
                    return Err(CliError::Api {
                        status: Some(404),
                        error: format!("no user '{}' found", trimmed),
                        context: None,
                    });
                }
                Err(e) => return Err(e),
            }
        }
        Some(CollaboratorsAction::Remove { user_id, yes, .. }) => {
            let token = TokenStore::new(config.credentials_path())
                .read()?
                .ok_or(CliError::AuthRequired)?;
            if !confirm_remove(&user_id, &repo_id, yes)? {
                eprintln!("Aborted.");
                return Ok(());
            }
            client
                .remove_collaborator(&repo_id, &token, &user_id)
                .await?;
            if output.is_json() {
                output.json(&json!({"removed": true, "userId": user_id}));
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
    use wiremock::matchers::{body_json, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    #[tokio::test]
    #[serial]
    async fn collaborators_add_sends_username_form_for_bare_identifiers() {
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
            .and(path("/api/v1/repos/alice/my-project/collaborators"))
            .and(body_json(serde_json::json!({
                "username": "bartad498",
                "role": "write"
            })))
            .respond_with(ResponseTemplate::new(201))
            .mount(&mock_server)
            .await;

        let config = Config::new(Some(&mock_server.uri())).unwrap();
        let output = Output::new(false);

        let result = cmd_collaborators(
            &config,
            &output,
            Some(CollaboratorsAction::Add {
                target: "bartad498".to_string(),
                role: AssignableRole::Write,
                if_repo: false,
            }),
            false,
        )
        .await;
        unsafe { std::env::remove_var("SYNS_CONFIG_DIR") };

        assert!(result.is_ok(), "got error: {:?}", result.err());
        let requests = mock_server.received_requests().await.unwrap();
        assert_eq!(requests.len(), 1);
        let auth = requests[0]
            .headers
            .get("authorization")
            .map(|v| v.to_str().unwrap())
            .unwrap_or("");
        assert_eq!(auth, "Bearer test-token");
    }

    #[tokio::test]
    #[serial]
    async fn collaborators_add_sends_email_form_for_at_containing_identifiers() {
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
            .and(path("/api/v1/repos/alice/my-project/collaborators"))
            .and(body_json(serde_json::json!({
                "email": "bart@example.com",
                "role": "admin"
            })))
            .respond_with(ResponseTemplate::new(201))
            .mount(&mock_server)
            .await;

        let config = Config::new(Some(&mock_server.uri())).unwrap();
        let output = Output::new(false);

        let result = cmd_collaborators(
            &config,
            &output,
            Some(CollaboratorsAction::Add {
                target: "bart@example.com".to_string(),
                role: AssignableRole::Admin,
                if_repo: false,
            }),
            false,
        )
        .await;
        unsafe { std::env::remove_var("SYNS_CONFIG_DIR") };

        assert!(result.is_ok(), "got error: {:?}", result.err());
        assert_eq!(mock_server.received_requests().await.unwrap().len(), 1);
    }

    #[tokio::test]
    #[serial]
    async fn collaborators_add_maps_404_with_reason_to_descriptive_error() {
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
            .and(path("/api/v1/repos/alice/my-project/collaborators"))
            .respond_with(ResponseTemplate::new(404).set_body_json(serde_json::json!({
                "error": "not_found",
                "message": "User not found",
                "reason": "not_found_user_by_username"
            })))
            .mount(&mock_server)
            .await;

        let config = Config::new(Some(&mock_server.uri())).unwrap();
        let output = Output::new(false);

        let result = cmd_collaborators(
            &config,
            &output,
            Some(CollaboratorsAction::Add {
                target: "bartad498".to_string(),
                role: AssignableRole::Write,
                if_repo: false,
            }),
            false,
        )
        .await;
        unsafe { std::env::remove_var("SYNS_CONFIG_DIR") };

        match result {
            Err(CliError::Api {
                status: Some(404),
                ref error,
                context: None,
            }) => assert_eq!(error, "no user 'bartad498' found"),
            other => panic!(
                "expected Api error with status 404 and target-bearing message, got: {other:?}"
            ),
        }
    }

    #[tokio::test]
    #[serial]
    async fn collaborators_add_propagates_404_without_reason_unchanged() {
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
            .and(path("/api/v1/repos/alice/my-project/collaborators"))
            .respond_with(ResponseTemplate::new(404).set_body_json(serde_json::json!({
                "error": "not_found",
                "message": "Repository not found"
            })))
            .mount(&mock_server)
            .await;

        let config = Config::new(Some(&mock_server.uri())).unwrap();
        let output = Output::new(false);

        let result = cmd_collaborators(
            &config,
            &output,
            Some(CollaboratorsAction::Add {
                target: "bartad498".to_string(),
                role: AssignableRole::Write,
                if_repo: false,
            }),
            false,
        )
        .await;
        unsafe { std::env::remove_var("SYNS_CONFIG_DIR") };

        match result {
            Err(CliError::Api {
                status: Some(404),
                ref error,
                context: None,
            }) => {
                assert_eq!(error, "not_found");
                assert!(
                    !error.contains("bartad498"),
                    "INV-38 violation: visibility-hidden 404 leaked target name in error: {error}"
                );
            }
            other => panic!(
                "expected Api error with status 404 and unchanged 'not_found' payload, got: {other:?}"
            ),
        }
    }

    #[tokio::test]
    #[serial]
    async fn collaborators_remove_with_yes_flag() {
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
                if_repo: false,
            }),
            false,
        )
        .await;
        unsafe { std::env::remove_var("SYNS_CONFIG_DIR") };

        assert!(result.is_ok());
    }

    #[tokio::test]
    #[serial]
    async fn collaborators_list_with_if_repo_set_and_identity_resolved_runs_normally() {
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
            .and(path("/api/v1/repos/alice/my-project/collaborators"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "data": [],
                "total": 0
            })))
            .mount(&mock_server)
            .await;

        let config = Config::new(Some(&mock_server.uri())).unwrap();
        let output = Output::new(false);

        let result = cmd_collaborators(&config, &output, None, true).await;
        unsafe { std::env::remove_var("SYNS_CONFIG_DIR") };

        assert!(result.is_ok());
    }

    #[tokio::test]
    #[serial]
    async fn collaborators_list_with_if_repo_set_and_no_identity_skips_silently() {
        let dir = tempfile::tempdir().unwrap();
        std::env::set_current_dir(dir.path()).unwrap();
        unsafe { std::env::set_var("SYNS_CONFIG_DIR", dir.path()) };

        let mock_server = MockServer::start().await;
        let config = Config::new(Some(&mock_server.uri())).unwrap();
        let output = Output::new(false);

        let result = cmd_collaborators(&config, &output, None, true).await;
        unsafe { std::env::remove_var("SYNS_CONFIG_DIR") };

        assert!(result.is_ok());
        assert!(mock_server.received_requests().await.unwrap().is_empty());
    }

    #[tokio::test]
    #[serial]
    async fn collaborators_add_with_if_repo_set_and_identity_resolved_runs_normally() {
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
                target: "bob-123".to_string(),
                role: AssignableRole::Write,
                if_repo: true,
            }),
            true,
        )
        .await;
        unsafe { std::env::remove_var("SYNS_CONFIG_DIR") };

        assert!(result.is_ok());
    }

    #[tokio::test]
    #[serial]
    async fn collaborators_add_with_if_repo_set_and_no_identity_skips_silently() {
        let dir = tempfile::tempdir().unwrap();
        std::env::set_current_dir(dir.path()).unwrap();
        unsafe { std::env::set_var("SYNS_CONFIG_DIR", dir.path()) };

        let mock_server = MockServer::start().await;
        let config = Config::new(Some(&mock_server.uri())).unwrap();
        let output = Output::new(false);

        let result = cmd_collaborators(
            &config,
            &output,
            Some(CollaboratorsAction::Add {
                target: "anyone".to_string(),
                role: AssignableRole::Read,
                if_repo: true,
            }),
            true,
        )
        .await;
        unsafe { std::env::remove_var("SYNS_CONFIG_DIR") };

        assert!(result.is_ok());
        assert!(mock_server.received_requests().await.unwrap().is_empty());
    }

    #[tokio::test]
    #[serial]
    async fn collaborators_remove_with_if_repo_set_and_identity_resolved_runs_normally() {
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
                if_repo: true,
            }),
            true,
        )
        .await;
        unsafe { std::env::remove_var("SYNS_CONFIG_DIR") };

        assert!(result.is_ok());
    }

    #[tokio::test]
    #[serial]
    async fn collaborators_remove_with_if_repo_set_and_no_identity_skips_silently() {
        let dir = tempfile::tempdir().unwrap();
        std::env::set_current_dir(dir.path()).unwrap();
        unsafe { std::env::set_var("SYNS_CONFIG_DIR", dir.path()) };

        let mock_server = MockServer::start().await;
        let config = Config::new(Some(&mock_server.uri())).unwrap();
        let output = Output::new(false);

        let result = cmd_collaborators(
            &config,
            &output,
            Some(CollaboratorsAction::Remove {
                user_id: "anyone".to_string(),
                yes: true,
                if_repo: true,
            }),
            true,
        )
        .await;
        unsafe { std::env::remove_var("SYNS_CONFIG_DIR") };

        assert!(result.is_ok());
        assert!(mock_server.received_requests().await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn collaborators_list_raw_preserves_full_user_and_added_by() {
        use wiremock::matchers::query_param;

        let mock_server = MockServer::start().await;
        let body = r#"{"data":[{"user":{"id":"u-bob","username":"bob","name":"Bob","email":"bob@test.com","emailVerified":true,"image":null,"createdAt":"2026-04-17T07:22:30.617Z","updatedAt":"2026-04-17T07:22:30.617Z"},"role":"write","addedBy":"u-alice","createdAt":"2026-04-18T00:00:00Z"}],"total":1,"limit":100,"offset":0}"#;
        Mock::given(method("GET"))
            .and(path("/api/v1/repos/alice/my-project/collaborators"))
            .and(query_param("limit", "100"))
            .and(query_param("offset", "0"))
            .respond_with(ResponseTemplate::new(200).set_body_string(body))
            .mount(&mock_server)
            .await;

        let client = SynsClient::new(&mock_server.uri()).unwrap();
        let (_typed, raw) = client
            .list_collaborators("alice/my-project", None, 100, 0)
            .await
            .unwrap();

        assert_eq!(
            raw["data"][0]["user"].as_object().unwrap().keys().count(),
            8
        );
        assert!(raw["data"][0].get("addedBy").is_some());
        assert_eq!(raw["data"][0]["addedBy"], serde_json::json!("u-alice"));
        assert!(raw["data"][0].get("createdAt").is_some());
        assert!(raw.get("limit").is_some());
        assert!(raw.get("offset").is_some());
        let expected: serde_json::Value = serde_json::from_str(body).unwrap();
        assert_eq!(raw, expected);
    }
}
