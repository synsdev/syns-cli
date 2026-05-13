use crate::client::{RepoStatus, SynsClient};
use crate::commands::repo::CliRepoStatus;
use crate::config::Config;
use crate::errors::CliError;
use crate::output::Output;
use console::style;

pub async fn cmd_explore(
    config: &Config,
    output: &Output,
    query: Option<String>,
    tags: Vec<String>,
    status: Option<CliRepoStatus>,
    limit: u32,
    offset: u32,
) -> Result<(), CliError> {
    let client = SynsClient::new(config.server_url())?;
    let status_enum = status.map(RepoStatus::from);
    let tag_str = if tags.is_empty() {
        None
    } else {
        Some(tags.join(","))
    };

    let (response, raw) = client
        .explore(
            query.as_deref(),
            tag_str.as_deref(),
            status_enum.as_ref(),
            limit,
            offset,
        )
        .await?;

    if output.is_json() {
        output.json(&raw);
    } else {
        let rows: Vec<Vec<String>> = response
            .data
            .iter()
            .map(|repo| {
                vec![
                    format!("{}/{}", repo.owner, repo.name),
                    repo.description.as_deref().unwrap_or("-").to_string(),
                    repo.status.as_query_str().to_string(),
                    repo.fork_count.to_string(),
                ]
            })
            .collect();
        output.table(&["Name", "Description", "Status", "Forks"], rows);

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
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::repo::CliRepoStatus;
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
                        "owner": "alice",
                        "name": "project-a",
                        "description": "First project",
                        "status": "active",
                        "visibility": "public",
                        "tags": ["api"],
                        "commitSha": "abc123",
                        "fileCount": 10,
                        "forkCount": 2,
                        "forkedFrom": null,
                        "createdAt": "2025-01-01T00:00:00Z",
                        "updatedAt": "2025-06-01T00:00:00Z"
                    },
                    {
                        "owner": "bob",
                        "name": "project-b",
                        "description": null,
                        "status": "draft",
                        "visibility": "public",
                        "tags": [],
                        "commitSha": null,
                        "fileCount": 0,
                        "forkCount": 0,
                        "forkedFrom": null,
                        "createdAt": "2025-02-01T00:00:00Z",
                        "updatedAt": "2025-06-02T00:00:00Z"
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
    async fn explore_with_status_filter() {
        let dir = tempfile::tempdir().unwrap();
        unsafe { std::env::set_var("SYNS_CONFIG_DIR", dir.path()) };

        let mock_server = MockServer::start().await;

        Mock::given(method("GET"))
            .and(path("/api/v1/explore"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "data": [],
                "total": 0,
                "limit": 20,
                "offset": 0
            })))
            .mount(&mock_server)
            .await;

        let config = Config::new(Some(&mock_server.uri())).unwrap();
        let output = Output::new(true);

        let result = cmd_explore(
            &config,
            &output,
            None,
            vec![],
            Some(CliRepoStatus::Active),
            20,
            0,
        )
        .await;
        unsafe { std::env::remove_var("SYNS_CONFIG_DIR") };

        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn explore_raw_emits_data_envelope_not_repositories_key() {
        use wiremock::matchers::query_param;

        let mock_server = MockServer::start().await;
        let body = r#"{"data":[{"owner":"alice","name":"a","description":null,"commitSha":"abc","status":"active","author":null,"tags":[],"visibility":"public","forkedFrom":null,"forkCount":0,"fileCount":1,"role":null,"createdAt":"2026-01-01T00:00:00Z","updatedAt":"2026-01-01T00:00:00Z"}],"total":1,"limit":20,"offset":0}"#;
        Mock::given(method("GET"))
            .and(path("/api/v1/explore"))
            .and(query_param("limit", "20"))
            .and(query_param("offset", "0"))
            .respond_with(ResponseTemplate::new(200).set_body_string(body))
            .mount(&mock_server)
            .await;

        let client = SynsClient::new(&mock_server.uri()).unwrap();
        let (_typed, raw) = client.explore(None, None, None, 20, 0).await.unwrap();

        assert!(raw["data"].is_array());
        assert!(raw.get("repositories").is_none());
        let expected: serde_json::Value = serde_json::from_str(body).unwrap();
        assert_eq!(raw, expected);
    }
}
