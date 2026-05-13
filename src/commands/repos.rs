use crate::auth::token::TokenStore;
use crate::client::{RepoListResponse, RepoStatus, SynsClient, Visibility};
use crate::commands::repo::{CliRepoStatus, CliVisibility};
use crate::config::Config;
use crate::errors::CliError;
use crate::output::Output;
use clap::{Args, ValueEnum};
use console::style;

const LIMIT_MIN: u32 = 1;
const LIMIT_MAX: u32 = 100;

#[derive(Args, Debug)]
pub struct ReposArgs {
    /// List repositories owned by another user instead (subject to access)
    #[arg(long)]
    pub owner: Option<String>,

    /// Substring filter on name or description
    #[arg(long, short = 'q')]
    pub query: Option<String>,

    /// Filter by repository status
    #[arg(long)]
    pub status: Option<CliRepoStatus>,

    /// Filter by visibility
    #[arg(long)]
    pub visibility: Option<CliVisibility>,

    /// Sort order
    #[arg(long)]
    pub sort: Option<CliRepoSort>,

    /// Maximum number of results
    #[arg(long, default_value = "20")]
    pub limit: u32,

    /// Number of results to skip
    #[arg(long, default_value = "0")]
    pub offset: u32,
}

#[derive(ValueEnum, Debug, Clone)]
pub enum CliRepoSort {
    Updated,
    Name,
    Created,
}

impl CliRepoSort {
    pub fn as_query_str(&self) -> &'static str {
        match self {
            CliRepoSort::Updated => "updated",
            CliRepoSort::Name => "name",
            CliRepoSort::Created => "created",
        }
    }
}

pub async fn cmd_repos(
    config: &Config,
    output: &Output,
    args: &ReposArgs,
) -> Result<(), CliError> {
    // 1. Client-side pagination validation (avoid a wire round-trip for an
    //    out-of-range --limit). Bounds match the server's
    //    paginationFields.limit schema (.min(1).max(100)).
    if args.limit < LIMIT_MIN || args.limit > LIMIT_MAX {
        return Err(CliError::Config {
            message: format!(
                "--limit must be between {LIMIT_MIN} and {LIMIT_MAX} (got {})",
                args.limit
            ),
        });
    }

    // 2. Load cached bearer token; silently fall back to anonymous on read
    //    failure (matches u23's read-command convention).
    let token = TokenStore::new(config.credentials_path())
        .read()
        .ok()
        .flatten();

    // 3. Build the HTTP client.
    let client = SynsClient::new(config.server_url())?;

    // 4. Convert typed flag values into wire forms.
    let status_enum: Option<RepoStatus> = args.status.clone().map(RepoStatus::from);
    let visibility_enum: Option<Visibility> = args.visibility.clone().map(Visibility::from);
    let sort_str: Option<&str> = args.sort.as_ref().map(|s| s.as_query_str());

    // 5. Wire call.
    let response: RepoListResponse = client
        .list_repos(
            token.as_deref(),
            args.query.as_deref(),
            args.owner.as_deref(),
            status_enum.as_ref(),
            visibility_enum.as_ref(),
            sort_str,
            args.limit,
            args.offset,
        )
        .await?;

    // 6. Render.
    if output.is_json() {
        output.json(&response);
        return Ok(());
    }

    if response.data.is_empty() && response.total == 0 {
        output.success("No repositories found.");
        return Ok(());
    }

    let rows: Vec<Vec<String>> = response
        .data
        .iter()
        .map(|repo| {
            vec![
                format!("{}/{}", repo.owner, repo.name),
                format!("{:?}", repo.visibility).to_lowercase(),
                repo.status.as_query_str().to_string(),
                repo.file_count.to_string(),
                repo.fork_count.to_string(),
                repo.updated_at.clone(),
            ]
        })
        .collect();
    output.table(
        &["Name", "Visibility", "Status", "Files", "Forks", "Updated"],
        rows,
    );

    if response.total > response.data.len() as u32 {
        eprintln!(
            "{}",
            style(format!(
                "Showing {} of {} repositories",
                response.data.len(),
                response.total
            ))
            .dim()
        );
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serial_test::serial;
    use wiremock::matchers::{method, path, query_param};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn default_args() -> ReposArgs {
        ReposArgs {
            owner: None,
            query: None,
            status: None,
            visibility: None,
            sort: None,
            limit: 20,
            offset: 0,
        }
    }

    #[tokio::test]
    #[serial]
    async fn repos_table_mode_lists_repos() {
        let dir = tempfile::tempdir().unwrap();
        unsafe { std::env::set_var("SYNS_CONFIG_DIR", dir.path()) };

        let mock_server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/v1/repos"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "data": [
                    {
                        "owner": "bart",
                        "name": "syns",
                        "description": null,
                        "commitSha": "abc123",
                        "status": "active",
                        "author": null,
                        "tags": [],
                        "visibility": "private",
                        "forkedFrom": null,
                        "forkCount": 0,
                        "fileCount": 436,
                        "role": "owner",
                        "createdAt": "2025-01-01T00:00:00Z",
                        "updatedAt": "2026-05-07T13:00:01Z"
                    },
                    {
                        "owner": "bart",
                        "name": "phpstorm",
                        "description": null,
                        "commitSha": "def456",
                        "status": "active",
                        "author": null,
                        "tags": [],
                        "visibility": "public",
                        "forkedFrom": null,
                        "forkCount": 2,
                        "fileCount": 1209,
                        "role": "owner",
                        "createdAt": "2025-01-01T00:00:00Z",
                        "updatedAt": "2026-05-02T17:55:14Z"
                    }
                ],
                "total": 2,
                "limit": 20,
                "offset": 0
            })))
            .mount(&mock_server)
            .await;

        let config = Config::new(Some(&mock_server.uri())).unwrap();
        let output = Output::new(false);

        let result = cmd_repos(&config, &output, &default_args()).await;
        unsafe { std::env::remove_var("SYNS_CONFIG_DIR") };

        assert!(result.is_ok());
        assert_eq!(mock_server.received_requests().await.unwrap().len(), 1);
    }

    #[tokio::test]
    #[serial]
    async fn repos_json_mode_emits_server_envelope_verbatim() {
        let dir = tempfile::tempdir().unwrap();
        unsafe { std::env::set_var("SYNS_CONFIG_DIR", dir.path()) };

        let mock_server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/v1/repos"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "data": [
                    {
                        "owner": "bart",
                        "name": "syns",
                        "description": null,
                        "commitSha": "abc123",
                        "status": "active",
                        "author": null,
                        "tags": [],
                        "visibility": "private",
                        "forkedFrom": null,
                        "forkCount": 0,
                        "fileCount": 436,
                        "role": "owner",
                        "createdAt": "2025-01-01T00:00:00Z",
                        "updatedAt": "2026-05-07T13:00:01Z"
                    }
                ],
                "total": 1,
                "limit": 20,
                "offset": 0
            })))
            .mount(&mock_server)
            .await;

        let config = Config::new(Some(&mock_server.uri())).unwrap();
        let output = Output::new(true); // JSON mode

        let result = cmd_repos(&config, &output, &default_args()).await;
        unsafe { std::env::remove_var("SYNS_CONFIG_DIR") };

        assert!(result.is_ok());
        assert_eq!(mock_server.received_requests().await.unwrap().len(), 1);
    }

    #[tokio::test]
    #[serial]
    async fn repos_empty_result_renders_no_repositories_banner() {
        let dir = tempfile::tempdir().unwrap();
        unsafe { std::env::set_var("SYNS_CONFIG_DIR", dir.path()) };

        let mock_server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/v1/repos"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "data": [],
                "total": 0,
                "limit": 20,
                "offset": 0
            })))
            .mount(&mock_server)
            .await;

        let config = Config::new(Some(&mock_server.uri())).unwrap();
        let output = Output::new(false);

        let result = cmd_repos(&config, &output, &default_args()).await;
        unsafe { std::env::remove_var("SYNS_CONFIG_DIR") };

        assert!(result.is_ok());
        assert_eq!(mock_server.received_requests().await.unwrap().len(), 1);
    }

    #[tokio::test]
    #[serial]
    async fn repos_invalid_limit_rejected_client_side_without_wire_call() {
        let dir = tempfile::tempdir().unwrap();
        unsafe { std::env::set_var("SYNS_CONFIG_DIR", dir.path()) };

        let mock_server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/v1/repos"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "data": [],
                "total": 0,
                "limit": 20,
                "offset": 0
            })))
            .mount(&mock_server)
            .await;

        let config = Config::new(Some(&mock_server.uri())).unwrap();
        let output = Output::new(false);

        let mut args = default_args();
        args.limit = 200; // out of [1, 100]

        let result = cmd_repos(&config, &output, &args).await;
        unsafe { std::env::remove_var("SYNS_CONFIG_DIR") };

        let err = result.expect_err("limit=200 must be rejected client-side");
        match err {
            CliError::Config { message } => {
                assert!(message.contains("--limit"), "message: {message}");
                assert!(message.contains("200"), "message: {message}");
            }
            other => panic!("expected CliError::Config, got {other:?}"),
        }

        assert!(
            mock_server.received_requests().await.unwrap().is_empty(),
            "no wire call should be made when --limit is out of range"
        );
    }

    #[tokio::test]
    #[serial]
    async fn repos_server_422_propagates_as_api_error() {
        let dir = tempfile::tempdir().unwrap();
        unsafe { std::env::set_var("SYNS_CONFIG_DIR", dir.path()) };

        let mock_server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/v1/repos"))
            .and(query_param("owner", "Alice"))
            .respond_with(ResponseTemplate::new(422).set_body_json(serde_json::json!({
                "error": "validation_error",
                "message": "owner: Must contain only lowercase letters, digits, and hyphens"
            })))
            .mount(&mock_server)
            .await;

        let config = Config::new(Some(&mock_server.uri())).unwrap();
        let output = Output::new(false);

        let mut args = default_args();
        args.owner = Some("Alice".to_string()); // uppercase rejected by server's regex

        let result = cmd_repos(&config, &output, &args).await;
        unsafe { std::env::remove_var("SYNS_CONFIG_DIR") };

        match result {
            Err(CliError::Api {
                status: Some(422),
                error,
                ..
            }) => assert_eq!(error, "validation_error"),
            other => panic!("expected CliError::Api {{ status: Some(422), .. }}, got {other:?}"),
        }

        assert_eq!(mock_server.received_requests().await.unwrap().len(), 1);
    }
}
