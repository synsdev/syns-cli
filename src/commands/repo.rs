use crate::auth::token::TokenStore;
use crate::client::{RepoResponse, RepoStatus, RepoUpdate, SynsClient, Visibility};
use crate::config::Config;
use crate::errors::CliError;
use crate::output::Output;
use crate::repo::resolve::resolve_repo_identity;

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

fn display_repo(output: &Output, response: &RepoResponse) {
    if output.is_json() {
        output.json(&response);
    } else {
        let rows = vec![
            vec!["Repository".into(), format!("{}/{}", response.owner, response.name)],
            vec!["Description".into(), response.description.as_deref().unwrap_or("(none)").to_string()],
            vec!["Status".into(), format!("{:?}", response.status).to_lowercase()],
            vec!["Visibility".into(), format!("{:?}", response.visibility).to_lowercase()],
            vec!["Tags".into(), if response.tags.is_empty() { "(none)".to_string() } else { response.tags.join(", ") }],
            vec!["Files".into(), response.file_count.to_string()],
            vec!["Commit".into(), response.commit_sha.as_deref().unwrap_or("(no commits)").to_string()],
            vec!["Created".into(), response.created_at.clone()],
            vec!["Updated".into(), response.updated_at.clone()],
        ];
        output.table(&["Property", "Value"], rows);
    }
}

pub async fn cmd_repo(
    config: &Config,
    output: &Output,
    description: Option<String>,
    status: Option<CliRepoStatus>,
    visibility: Option<CliVisibility>,
    tags: Vec<String>,
) -> Result<(), CliError> {
    let is_update = description.is_some() || status.is_some() || visibility.is_some() || !tags.is_empty();

    let current_dir = std::env::current_dir()
        .map_err(|e| CliError::Io { message: format!("could not determine current directory: {e}") })?;
    let identity = resolve_repo_identity(None, &current_dir)?;
    let owner = identity.owner.ok_or(CliError::RepoIdentityUnknown)?;
    let repo_id = format!("{}/{}", owner, identity.name);
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
        let token = TokenStore::new(config.credentials_path()).read().ok().flatten();
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
        ).unwrap();
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

        let result = cmd_repo(&config, &output, None, None, None, vec![]).await;
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
        ).unwrap();
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

        let result = cmd_repo(&config, &output, None, None, Some(CliVisibility::Public), vec![]).await;
        unsafe { std::env::remove_var("SYNS_CONFIG_DIR") };

        assert!(result.is_ok());
    }
}
