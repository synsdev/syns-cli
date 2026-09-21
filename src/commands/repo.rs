use crate::auth::token::TokenStore;
use crate::client::{RepoResponse, RepoStatus, RepoUpdate, SynsClient, Visibility};
use crate::commands::repos::ReposArgs;
use crate::config::Config;
use crate::errors::CliError;
use crate::output::Output;
use crate::repo::if_repo::resolve_full_or_skip;

#[derive(clap::ValueEnum, Debug, Clone)]
pub enum CliRepoStatus {
    Active,
    Draft,
    Completed,
    Abandoned,
}

impl From<CliRepoStatus> for RepoStatus {
    fn from(s: CliRepoStatus) -> Self {
        match s {
            CliRepoStatus::Active => RepoStatus::Active,
            CliRepoStatus::Draft => RepoStatus::Draft,
            CliRepoStatus::Completed => RepoStatus::Completed,
            CliRepoStatus::Abandoned => RepoStatus::Abandoned,
        }
    }
}

#[derive(clap::ValueEnum, Debug, Clone)]
pub enum CliVisibility {
    Public,
    Private,
}

impl From<CliVisibility> for Visibility {
    fn from(v: CliVisibility) -> Self {
        match v {
            CliVisibility::Public => Visibility::Public,
            CliVisibility::Private => Visibility::Private,
        }
    }
}

#[derive(clap::Subcommand, Debug)]
pub enum RepoAction {
    /// List the caller's repositories
    List(ReposArgs),
    /// Create an empty repository under the caller
    Create {
        /// The repository's name, unqualified and owned by the caller
        #[arg(value_name = "NAME")]
        name: String,
        /// The repository's description
        #[arg(long)]
        description: Option<String>,
        /// Whether the repository is reachable without a grant
        #[arg(long)]
        visibility: Option<CliVisibility>,
    },
}

/// Creates an empty repository under the caller (SPEC u272 Behaviour,
/// `cmd_repo_create`).
///
/// No file in the run's directory is written, read or repointed,
/// whatever the run answers: the name is taken as an unqualified one
/// owned by the caller, and no identity file follows the creation.
pub async fn cmd_repo_create(
    config: &Config,
    output: &Output,
    name: String,
    description: Option<String>,
    visibility: Option<CliVisibility>,
) -> Result<(), CliError> {
    // 1 — require a stored credential, before any request.
    let token = TokenStore::new(config.credentials_path())
        .read()?
        .ok_or(CliError::AuthRequired)?;

    // 2 and 3 — the positional as typed, and the create carrying only
    //           the fields the caller gave.
    let visibility: Option<Visibility> = visibility.map(Visibility::from);
    let client = SynsClient::new(config.server_url())?;
    let (created, raw) = client
        .create_repo(&token, &name, description.as_deref(), visibility.as_ref())
        .await?;

    // 4 — render the repository the answer carried, or write the served
    //     body. Nothing on disk is touched either way.
    if output.is_json() {
        output.json(&raw);
    } else {
        display_repo(output, &created);
    }
    Ok(())
}

fn display_repo(output: &Output, response: &RepoResponse) {
    if output.is_json() {
        output.json(&response);
    } else {
        let rows = vec![
            vec![
                "Repository".into(),
                format!("{}/{}", response.owner, response.name),
            ],
            vec![
                "Description".into(),
                response
                    .description
                    .as_deref()
                    .unwrap_or("(none)")
                    .to_string(),
            ],
            vec![
                "Status".into(),
                format!("{:?}", response.status).to_lowercase(),
            ],
            vec![
                "Visibility".into(),
                format!("{:?}", response.visibility).to_lowercase(),
            ],
            vec![
                "Tags".into(),
                if response.tags.is_empty() {
                    "(none)".to_string()
                } else {
                    response.tags.join(", ")
                },
            ],
            vec!["Files".into(), response.file_count.to_string()],
            vec![
                "Commit".into(),
                response
                    .commit_sha
                    .as_deref()
                    .unwrap_or("(no commits)")
                    .to_string(),
            ],
            vec!["Created".into(), response.created_at.clone()],
            vec!["Updated".into(), response.updated_at.clone()],
        ];
        output.table(&["Property", "Value"], rows);
    }
}

#[allow(clippy::too_many_arguments)]
pub async fn cmd_repo(
    config: &Config,
    output: &Output,
    description: Option<String>,
    status: Option<CliRepoStatus>,
    visibility: Option<CliVisibility>,
    tags: Vec<String>,
    if_repo: bool,
    action: Option<RepoAction>,
) -> Result<(), CliError> {
    match action {
        Some(RepoAction::List(repos_args)) => {
            return crate::commands::repos::cmd_repos(config, output, &repos_args).await;
        }
        // The create returns from the same early branch, so no identity
        // and no working directory is reached.
        Some(RepoAction::Create {
            name,
            description,
            visibility,
        }) => {
            return cmd_repo_create(config, output, name, description, visibility).await;
        }
        None => {}
    }

    let is_update =
        description.is_some() || status.is_some() || visibility.is_some() || !tags.is_empty();

    let current_dir = std::env::current_dir().map_err(|e| CliError::Io {
        message: format!("could not determine current directory: {e}"),
    })?;
    let (owner, name) = match resolve_full_or_skip(None, &current_dir, if_repo, output)? {
        Some(pair) => pair,
        None => return Ok(()),
    };
    let repo_id = format!("{owner}/{name}");
    let client = SynsClient::new(config.server_url())?;

    if is_update {
        let token = TokenStore::new(config.credentials_path())
            .read()?
            .ok_or(CliError::AuthRequired)?;
        let update = RepoUpdate {
            description,
            status: status.map(|s| s.into()),
            visibility: visibility.map(|v| v.into()),
            author: None,
            tags: if tags.is_empty() { None } else { Some(tags) },
        };
        let response = client.update_repo(&repo_id, &token, &update).await?;
        display_repo(output, &response);
    } else {
        let token = TokenStore::new(config.credentials_path())
            .read()
            .ok()
            .flatten();
        let response = client.get_repo(&repo_id, token.as_deref()).await?;
        display_repo(output, &response);
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
    async fn repo_displays_metadata() {
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
            .and(path("/api/v1/repos/alice/my-project"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "owner": "alice",
                "name": "my-project",
                "description": "Test repo",
                "status": "active",
                "visibility": "public",
                "tags": ["api", "v2"],
                "commitSha": "abc12345def67890",
                "fileCount": 42,
                "forkCount": 0,
                "forkedFrom": null,
                "createdAt": "2025-01-01T00:00:00Z",
                "updatedAt": "2025-06-01T00:00:00Z"
            })))
            .mount(&mock_server)
            .await;

        let config = Config::new(Some(&mock_server.uri())).unwrap();
        let output = Output::new(false);

        let result = cmd_repo(&config, &output, None, None, None, vec![], false, None).await;
        unsafe { std::env::remove_var("SYNS_CONFIG_DIR") };

        assert!(result.is_ok());
    }

    #[tokio::test]
    #[serial]
    async fn repo_updates_visibility() {
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

        Mock::given(method("PATCH"))
            .and(path("/api/v1/repos/alice/my-project"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "owner": "alice",
                "name": "my-project",
                "description": "Test repo",
                "status": "active",
                "visibility": "public",
                "tags": [],
                "commitSha": "abc12345def67890",
                "fileCount": 42,
                "forkCount": 0,
                "forkedFrom": null,
                "createdAt": "2025-01-01T00:00:00Z",
                "updatedAt": "2025-06-01T00:00:00Z"
            })))
            .mount(&mock_server)
            .await;

        let config = Config::new(Some(&mock_server.uri())).unwrap();
        let output = Output::new(false);

        let result = cmd_repo(
            &config,
            &output,
            None,
            None,
            Some(CliVisibility::Public),
            vec![],
            false,
            None,
        )
        .await;
        unsafe { std::env::remove_var("SYNS_CONFIG_DIR") };

        assert!(result.is_ok());
    }

    #[tokio::test]
    #[serial]
    async fn repo_with_if_repo_set_and_identity_resolved_runs_normally() {
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
            .and(path("/api/v1/repos/alice/my-project"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "owner": "alice",
                "name": "my-project",
                "description": "Test repo",
                "status": "active",
                "visibility": "public",
                "tags": [],
                "commitSha": "abc12345def67890",
                "fileCount": 42,
                "forkCount": 0,
                "forkedFrom": null,
                "createdAt": "2025-01-01T00:00:00Z",
                "updatedAt": "2025-06-01T00:00:00Z"
            })))
            .mount(&mock_server)
            .await;

        let config = Config::new(Some(&mock_server.uri())).unwrap();
        let output = Output::new(false);

        let result = cmd_repo(&config, &output, None, None, None, vec![], true, None).await;
        unsafe { std::env::remove_var("SYNS_CONFIG_DIR") };

        assert!(result.is_ok());
    }

    #[tokio::test]
    #[serial]
    async fn repo_with_if_repo_set_and_no_identity_skips_silently() {
        let dir = tempfile::tempdir().unwrap();
        std::env::set_current_dir(dir.path()).unwrap();
        unsafe { std::env::set_var("SYNS_CONFIG_DIR", dir.path()) };

        let mock_server = MockServer::start().await;
        let config = Config::new(Some(&mock_server.uri())).unwrap();
        let output = Output::new(false);

        let result = cmd_repo(&config, &output, None, None, None, vec![], true, None).await;
        unsafe { std::env::remove_var("SYNS_CONFIG_DIR") };

        assert!(result.is_ok());
        assert!(mock_server.received_requests().await.unwrap().is_empty());
    }

    // --- `syns repo create NAME` (SPEC u272) ---

    use crate::client::u272_bodies as B;

    // SPEC u272 Behaviour, `cmd_repo_create` 3 and 4: only the fields
    // the caller gave are sent, and the run's directory holds no file
    // afterwards.
    #[tokio::test]
    #[serial]
    async fn a_create_sends_only_what_was_given_and_writes_no_file() {
        use wiremock::matchers::body_json;

        let dir = tempfile::tempdir().unwrap();
        TokenStore::new(dir.path().join("credentials.json"))
            .write("u272-token")
            .unwrap();
        let work = tempfile::tempdir().unwrap();
        std::env::set_current_dir(work.path()).unwrap();
        unsafe { std::env::set_var("SYNS_CONFIG_DIR", dir.path()) };

        let mock_server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/api/v1/repos"))
            .and(body_json(serde_json::json!({"name": "u272-parity-probe"})))
            .respond_with(ResponseTemplate::new(201).set_body_string(B::CREATED_REPO))
            .mount(&mock_server)
            .await;

        let config = Config::new(Some(&mock_server.uri())).unwrap();
        let output = Output::new(true);

        let result = cmd_repo_create(
            &config,
            &output,
            "u272-parity-probe".to_string(),
            None,
            None,
        )
        .await;
        unsafe { std::env::remove_var("SYNS_CONFIG_DIR") };

        assert!(result.is_ok(), "got {:?}", result.err());
        assert_eq!(mock_server.received_requests().await.unwrap().len(), 1);
        assert_eq!(
            std::fs::read_dir(work.path()).unwrap().count(),
            0,
            "the run's directory holds no file afterwards"
        );
    }

    #[tokio::test]
    #[serial]
    async fn a_create_naming_both_fields_sends_both() {
        use wiremock::matchers::body_json;

        let dir = tempfile::tempdir().unwrap();
        TokenStore::new(dir.path().join("credentials.json"))
            .write("u272-token")
            .unwrap();
        std::env::set_current_dir(dir.path()).unwrap();
        unsafe { std::env::set_var("SYNS_CONFIG_DIR", dir.path()) };

        let mock_server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/api/v1/repos"))
            .and(body_json(serde_json::json!({
                "name": "notes",
                "description": "u272 probe",
                "visibility": "public",
            })))
            .respond_with(ResponseTemplate::new(201).set_body_string(B::CREATED_REPO))
            .mount(&mock_server)
            .await;

        let config = Config::new(Some(&mock_server.uri())).unwrap();
        let output = Output::new(false);

        let result = cmd_repo_create(
            &config,
            &output,
            "notes".to_string(),
            Some("u272 probe".to_string()),
            Some(CliVisibility::Public),
        )
        .await;
        unsafe { std::env::remove_var("SYNS_CONFIG_DIR") };

        assert!(result.is_ok(), "got {:?}", result.err());
    }

    // SPEC u272 Behaviour, `cmd_repo_create` 1: no credential refuses
    // before any request.
    #[tokio::test]
    #[serial]
    async fn a_create_with_no_credential_refuses_before_any_request() {
        let dir = tempfile::tempdir().unwrap();
        std::env::set_current_dir(dir.path()).unwrap();
        unsafe { std::env::set_var("SYNS_CONFIG_DIR", dir.path()) };

        let mock_server = MockServer::start().await;
        let config = Config::new(Some(&mock_server.uri())).unwrap();
        let output = Output::new(false);

        let err = cmd_repo_create(&config, &output, "notes".to_string(), None, None)
            .await
            .unwrap_err();
        unsafe { std::env::remove_var("SYNS_CONFIG_DIR") };

        assert!(matches!(err, CliError::AuthRequired));
        assert!(mock_server.received_requests().await.unwrap().is_empty());
    }

    // The create branch returns before any identity is reached, so a
    // working directory no identity file touches is no obstacle.
    #[tokio::test]
    #[serial]
    async fn the_create_arm_reaches_no_identity_ladder() {
        let dir = tempfile::tempdir().unwrap();
        TokenStore::new(dir.path().join("credentials.json"))
            .write("u272-token")
            .unwrap();
        let work = tempfile::tempdir().unwrap();
        std::env::set_current_dir(work.path()).unwrap();
        unsafe { std::env::set_var("SYNS_CONFIG_DIR", dir.path()) };

        let mock_server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/api/v1/repos"))
            .respond_with(ResponseTemplate::new(201).set_body_string(B::CREATED_REPO))
            .mount(&mock_server)
            .await;

        let config = Config::new(Some(&mock_server.uri())).unwrap();
        let output = Output::new(true);

        let result = cmd_repo(
            &config,
            &output,
            None,
            None,
            None,
            vec![],
            false,
            Some(RepoAction::Create {
                name: "u272-parity-probe".to_string(),
                description: None,
                visibility: None,
            }),
        )
        .await;
        unsafe { std::env::remove_var("SYNS_CONFIG_DIR") };

        assert!(result.is_ok(), "got {:?}", result.err());
        assert_eq!(std::fs::read_dir(work.path()).unwrap().count(), 0);
    }
}
