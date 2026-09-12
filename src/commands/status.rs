use crate::auth::token::TokenStore;
use crate::client::SynsClient;
use crate::config::Config;
use crate::errors::CliError;
use crate::output::Output;
use crate::push::converge::{WorkingCopyState, working_copy_state};
use crate::push::working_copy::WorkingCopy;
use crate::repo::if_repo::resolve_full_or_skip;
use crate::repo::root::push_scope;

/// The machine-readable status document: the repository reply with the
/// working copy's state laid over it (SPEC u256 Q-02).
fn status_document(
    response: &crate::client::RepoResponse,
    state: WorkingCopyState,
) -> Result<serde_json::Value, CliError> {
    let mut document = serde_json::to_value(response).map_err(|e| CliError::Io {
        message: format!("could not render the status document: {e}"),
    })?;
    if let Some(object) = document.as_object_mut() {
        object.insert("workingCopyState".into(), state.as_key().into());
    }
    Ok(document)
}

pub async fn cmd_status(config: &Config, output: &Output, if_repo: bool) -> Result<(), CliError> {
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

    let response = client.get_repo(&repo_id, token.as_deref()).await?;

    // SPEC u256 `cmd_status` 2: the working copy's state, against a head
    // read in this same run.
    let scope = push_scope(None, &current_dir, &owner, &name)?;
    let copy = WorkingCopy::open(config.cache_dir(), &owner, &name, &scope.root)?;
    let state = working_copy_state(&client, token.as_deref(), &copy).await?;

    if output.is_json() {
        output.json(&status_document(&response, state)?);
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
            vec!["Created".into(), response.created_at],
            vec!["Updated".into(), response.updated_at],
            vec!["Working copy".into(), state.label().to_string()],
        ];
        output.table(&["Property", "Value"], rows);
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serial_test::serial;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    async fn mount_tree(mock_server: &MockServer) {
        Mock::given(method("GET"))
            .and(path("/api/v1/repos/alice/my-project/tree"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "entries": [],
                "commitSha": "abc12345def67890",
                "truncated": false
            })))
            .mount(mock_server)
            .await;
    }

    #[tokio::test]
    #[serial]
    async fn status_shows_repo_metadata() {
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
        mount_tree(&mock_server).await;

        let config = Config::new(Some(&mock_server.uri())).unwrap();
        let output = Output::new(false);

        let result = cmd_status(&config, &output, false).await;
        unsafe { std::env::remove_var("SYNS_CONFIG_DIR") };

        assert!(result.is_ok(), "{result:?}");
    }

    #[tokio::test]
    #[serial]
    async fn status_json_output() {
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
        mount_tree(&mock_server).await;

        let config = Config::new(Some(&mock_server.uri())).unwrap();
        let output = Output::new(true);

        let result = cmd_status(&config, &output, false).await;
        unsafe { std::env::remove_var("SYNS_CONFIG_DIR") };

        assert!(result.is_ok(), "{result:?}");

        let client = SynsClient::new(&mock_server.uri()).unwrap();
        let response = client.get_repo("alice/my-project", None).await.unwrap();
        let document = status_document(&response, WorkingCopyState::LocalChanges).unwrap();
        assert_eq!(document["workingCopyState"], "local_changes");
        assert_eq!(document["name"], "my-project");
    }

    #[tokio::test]
    #[serial]
    async fn status_with_if_repo_set_and_identity_resolved_runs_normally() {
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
        mount_tree(&mock_server).await;

        let config = Config::new(Some(&mock_server.uri())).unwrap();
        let output = Output::new(false);

        let result = cmd_status(&config, &output, true).await;
        unsafe { std::env::remove_var("SYNS_CONFIG_DIR") };

        assert!(result.is_ok(), "{result:?}");
    }

    #[tokio::test]
    #[serial]
    async fn status_with_if_repo_set_and_no_identity_skips_silently() {
        let dir = tempfile::tempdir().unwrap();
        std::env::set_current_dir(dir.path()).unwrap();
        unsafe { std::env::set_var("SYNS_CONFIG_DIR", dir.path()) };

        let mock_server = MockServer::start().await;
        let config = Config::new(Some(&mock_server.uri())).unwrap();
        let output = Output::new(false);

        let result = cmd_status(&config, &output, true).await;
        unsafe { std::env::remove_var("SYNS_CONFIG_DIR") };

        assert!(result.is_ok());
        assert!(mock_server.received_requests().await.unwrap().is_empty());
    }
}
