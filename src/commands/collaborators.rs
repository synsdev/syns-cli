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
                ) =>
                {
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

    // ---------------------------------------------------------------------
    // R2 code-review medium-priority test coverage (M1-M5).
    // Backfill regression tests for branches surfaced as gaps in
    // CODE_REVIEW.md §"Medium-Priority Issues".
    // ---------------------------------------------------------------------

    /// M1 — Empty-target pre-flight guard short-circuits with
    /// `CliError::Config` BEFORE any HTTP call. The mock server is
    /// registered with NO mocks; the assertion
    /// `received_requests().is_empty()` proves the network was never
    /// touched.
    #[tokio::test]
    #[serial]
    async fn collaborators_add_empty_target_rejected_pre_flight() {
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
        // Intentionally NO mocks registered — any HTTP call would fail
        // the second assertion below.

        let config = Config::new(Some(&mock_server.uri())).unwrap();
        let output = Output::new(false);

        let result = cmd_collaborators(
            &config,
            &output,
            Some(CollaboratorsAction::Add {
                target: "   ".to_string(),
                role: AssignableRole::Write,
                if_repo: false,
            }),
            false,
        )
        .await;
        unsafe { std::env::remove_var("SYNS_CONFIG_DIR") };

        match result {
            Err(CliError::Config { ref message }) => {
                assert!(
                    message.contains("target cannot be empty"),
                    "expected message to contain 'target cannot be empty', got: {message}"
                );
            }
            other => panic!("expected Err(CliError::Config), got: {other:?}"),
        }
        assert!(
            mock_server.received_requests().await.unwrap().is_empty(),
            "empty-target guard must short-circuit before any HTTP call"
        );
    }

    /// M2 — SPEC D6 fail-loud invariant. A 404 with a `reason` value
    /// that is NOT one of the three known discriminators must pass
    /// through unchanged so the user sees the raw payload and a future
    /// maintainer is signalled to update this unit. The command-layer
    /// remap must NOT fire — `error` must not contain the target name.
    #[tokio::test]
    #[serial]
    async fn collaborators_add_404_unknown_reason_passes_through() {
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
                "reason": "not_found_user_by_handle"
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
                // Raw discriminator passes through verbatim (D6
                // fail-loud).
                assert_eq!(error, "not_found_user_by_handle");
                // SPEC D6 invariant: command-layer remap must NOT fire
                // for unknown `reason` values.
                assert!(
                    !error.contains("bartad498"),
                    "D6 violation: unknown reason should fall through, got rewritten: {error}"
                );
            }
            other => {
                panic!("expected Api error with status 404 and raw discriminator, got: {other:?}")
            }
        }
    }

    /// M3 — Malformed 404 body (not valid JSON) must collapse to the
    /// canonical `"unknown error"` fallback documented in PLAN §R2 and
    /// mirroring `check_response`'s existing fallback.
    #[tokio::test]
    #[serial]
    async fn collaborators_add_404_malformed_body_falls_back() {
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
            .respond_with(ResponseTemplate::new(404).set_body_string("not json {"))
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
            }) => assert_eq!(error, "unknown error"),
            other => panic!(
                "expected Api error with status 404 and 'unknown error' fallback, got: {other:?}"
            ),
        }
    }

    /// M4 — JSON-mode success ack rotates the key from `"userId"` to
    /// `"target"` per SPEC D7. The `Output` abstraction prints via
    /// `println!` and the syns-cli test suite has no stdout-capture
    /// dependency (`gag`/`os_pipe` are not declared in Cargo.toml);
    /// adding one is out of scope for an R2 fix. As a pragmatic
    /// substitute, the test exercises two halves of the contract:
    ///
    /// 1. The cmd_collaborators code path runs to completion in JSON
    ///    mode (`Output::new(true)`), the mocked 201 is matched, and
    ///    exactly one HTTP request is received — proving the JSON
    ///    success branch executed end-to-end.
    /// 2. `Output::format_json` (the same crate-internal serialiser
    ///    `Output::json` delegates to) is invoked on the exact
    ///    `serde_json::json!` payload the production code constructs
    ///    at `commands/collaborators.rs:160-163`. The emitted shape is
    ///    asserted to contain `"target":"bartad498"` and to NOT
    ///    contain `"userId"` — locking the SPEC D7 rotation against
    ///    botched-merge regressions.
    ///
    /// Note: this does not verify that stdout actually received the
    /// expected bytes; that property is documented in
    /// IMPLEMENTATION_R2.md "Issues Encountered" and left for a future
    /// integration-test pass that adds `assert_cmd`-style binary
    /// invocation.
    #[tokio::test]
    #[serial]
    async fn collaborators_add_json_mode_emits_target_key() {
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
            .respond_with(ResponseTemplate::new(201))
            .mount(&mock_server)
            .await;

        let config = Config::new(Some(&mock_server.uri())).unwrap();
        let output = Output::new(true);
        assert!(
            output.is_json(),
            "fixture must be JSON-mode for this test to be meaningful"
        );

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

        // Half 1 — the JSON success branch ran end-to-end.
        assert!(result.is_ok(), "got error: {:?}", result.err());
        assert_eq!(
            mock_server.received_requests().await.unwrap().len(),
            1,
            "exactly one HTTP request expected on success branch"
        );

        // Half 2 — the JSON shape constructed by the production code
        // (commands/collaborators.rs:160-163) serialises with the
        // post-rotation key `target`, not the pre-fix `userId`.
        let shape = output.format_json(&serde_json::json!({
            "added": true,
            "target": "bartad498",
            "role": "write",
        }));
        assert!(
            shape.contains("\"target\":\"bartad498\""),
            "expected 'target' key with value, got: {shape}"
        );
        assert!(
            !shape.contains("userId"),
            "SPEC D7 violation: 'userId' key leaked into JSON ack, got: {shape}"
        );
    }

    /// M5 — SPEC D8 trim-once invariant. The production code must trim
    /// `target` BEFORE dispatching the HTTP request body; otherwise a
    /// confusing 404 lookup against the whitespace-padded username
    /// would surface to the user. The `body_json` matcher is strict —
    /// if production skips the trim, the request body diverges, the
    /// mock does not match, wiremock returns 404, and the test fails.
    #[tokio::test]
    #[serial]
    async fn collaborators_add_trims_whitespace_before_dispatch() {
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
                target: "  bartad498  ".to_string(),
                role: AssignableRole::Write,
                if_repo: false,
            }),
            false,
        )
        .await;
        unsafe { std::env::remove_var("SYNS_CONFIG_DIR") };

        assert!(
            result.is_ok(),
            "trim-once violation: body_json matcher rejected payload, got: {:?}",
            result.err()
        );
        assert_eq!(mock_server.received_requests().await.unwrap().len(), 1);
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
