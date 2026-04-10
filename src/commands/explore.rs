use crate::client::{RepoStatus, SynsClient};
use crate::config::Config;
use crate::errors::CliError;
use crate::output::Output;
use console::style;
use serde_json::to_value;

fn parse_status(value: &str) -> Result<RepoStatus, CliError> {
    match value.to_lowercase().as_str() {
        "active" => Ok(RepoStatus::Active),
        "draft" => Ok(RepoStatus::Draft),
        "completed" => Ok(RepoStatus::Completed),
        "abandoned" => Ok(RepoStatus::Abandoned),
        _ => Err(CliError::Config {
            message: format!("invalid status: '{}' — valid values: active, draft, completed, abandoned", value),
        }),
    }
}

fn status_display(status: &RepoStatus) -> &str {
    match status {
        RepoStatus::Active => "active",
        RepoStatus::Draft => "draft",
        RepoStatus::Completed => "completed",
        RepoStatus::Abandoned => "abandoned",
        RepoStatus::Unknown => "unknown",
    }
}

pub async fn cmd_explore(
    config: &Config,
    output: &Output,
    query: Option<String>,
    tags: Vec<String>,
    status: Option<String>,
    limit: u32,
    offset: u32,
) -> Result<(), CliError> {
    let client = SynsClient::new(config.server_url())?;
    let status_enum = match &status {
        Some(s) => Some(parse_status(s)?),
        None => None,
    };
    let tag_str = if tags.is_empty() { None } else { Some(tags.join(",")) };

    let response = client.explore(query.as_deref(), tag_str.as_deref(), status_enum.as_ref(), limit, offset).await?;

    if output.is_json() {
        let repos = to_value(&response.data).map_err(|e| CliError::Io {
            message: format!("serialization error: {e}"),
        })?;
        output.json(&serde_json::json!({
            "repositories": repos,
            "total": response.total,
            "limit": response.limit,
            "offset": response.offset,
        }));
    } else {
        let rows: Vec<Vec<String>> = response.data.iter().map(|repo| {
            vec![
                repo.id.clone(),
                repo.description.as_deref().unwrap_or("-").to_string(),
                status_display(&repo.status).to_string(),
                repo.fork_count.to_string(),
            ]
        }).collect();
        output.table(&["Name", "Description", "Status", "Forks"], rows);

        if response.total > response.data.len() as u32 {
            eprintln!("{}", style(format!(
                "Showing {} of {} repositories", response.data.len(), response.total
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
    async fn explore_json_output() {
        let dir = tempfile::tempdir().unwrap();
        unsafe { std::env::set_var("SYNS_CONFIG_DIR", dir.path()) };

        let mock_server = MockServer::start().await;

        Mock::given(method("GET"))
            .and(path("/api/v1/explore"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "data": [
                    {
                        "id": "alice/project-a",
                        "name": "project-a",
                        "description": "First project",
                        "owner_id": "u1",
                        "status": "active",
                        "visibility": "public",
                        "tags": ["api"],
                        "commit_sha": "abc123",
                        "file_count": 10,
                        "fork_count": 2,
                        "forked_from": null,
                        "created_at": "2025-01-01T00:00:00Z",
                        "updated_at": "2025-06-01T00:00:00Z"
                    },
                    {
                        "id": "bob/project-b",
                        "name": "project-b",
                        "description": null,
                        "owner_id": "u2",
                        "status": "draft",
                        "visibility": "public",
                        "tags": [],
                        "commit_sha": null,
                        "file_count": 0,
                        "fork_count": 0,
                        "forked_from": null,
                        "created_at": "2025-02-01T00:00:00Z",
                        "updated_at": "2025-06-02T00:00:00Z"
                    }
                ],
                "total": 3,
                "limit": 2,
                "offset": 0
            })))
            .mount(&mock_server)
            .await;

        let config = Config::new(Some(&mock_server.uri())).unwrap();
        let output = Output::new(true);

        let result = cmd_explore(&config, &output, None, vec![], None, 2, 0).await;
        unsafe { std::env::remove_var("SYNS_CONFIG_DIR") };

        assert!(result.is_ok());
    }

    #[tokio::test]
    #[serial]
    async fn explore_invalid_status() {
        let dir = tempfile::tempdir().unwrap();
        unsafe { std::env::set_var("SYNS_CONFIG_DIR", dir.path()) };

        let mock_server = MockServer::start().await;

        let config = Config::new(Some(&mock_server.uri())).unwrap();
        let output = Output::new(false);

        let result = cmd_explore(&config, &output, None, vec![], Some("invalid".into()), 20, 0).await;
        unsafe { std::env::remove_var("SYNS_CONFIG_DIR") };

        assert!(result.is_err());
        assert!(format!("{:?}", result.unwrap_err()).contains("invalid status"));
    }
}
