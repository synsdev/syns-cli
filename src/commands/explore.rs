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
    use std::collections::BTreeSet;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, Request, ResponseTemplate};

    /// An `EP-explore` mock answering an empty page to any `GET`, so a
    /// request reaches it whatever query keys it carries.
    async fn empty_page_server() -> MockServer {
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
        mock_server
    }

    /// The one request the mock recorded, asserted to be the only one.
    async fn only_request(mock_server: &MockServer) -> Request {
        let mut received = mock_server.received_requests().await.unwrap();
        assert_eq!(received.len(), 1, "exactly one request is sent");
        received.remove(0)
    }

    /// A request's decoded query pairs, compared as a set so the order
    /// they are sent in decides nothing.
    fn pairs_of(request: &Request) -> BTreeSet<(String, String)> {
        request
            .url
            .query_pairs()
            .map(|(k, v)| (k.into_owned(), v.into_owned()))
            .collect()
    }

    fn pairs(expected: &[(&str, &str)]) -> BTreeSet<(String, String)> {
        expected
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    #[tokio::test]
    async fn client_sends_tags_under_the_registered_key() {
        let mock_server = empty_page_server().await;
        let client = SynsClient::new(&mock_server.uri()).unwrap();

        client
            .explore(None, Some("syns-app,cli"), None, 20, 0)
            .await
            .unwrap();

        let request = only_request(&mock_server).await;
        assert_eq!(
            pairs_of(&request),
            pairs(&[("limit", "20"), ("offset", "0"), ("tags", "syns-app,cli")])
        );
    }

    #[tokio::test]
    async fn client_sends_query_under_the_registered_key() {
        let mock_server = empty_page_server().await;
        let client = SynsClient::new(&mock_server.uri()).unwrap();

        client
            .explore(Some("templates"), None, None, 20, 0)
            .await
            .unwrap();

        let request = only_request(&mock_server).await;
        assert_eq!(
            pairs_of(&request),
            pairs(&[("limit", "20"), ("offset", "0"), ("q", "templates")])
        );
    }

    #[tokio::test]
    async fn client_sends_every_filter_under_its_registered_key() {
        let mock_server = empty_page_server().await;
        let client = SynsClient::new(&mock_server.uri()).unwrap();

        client
            .explore(Some("tmpl"), Some("a,b"), Some(&RepoStatus::Active), 5, 10)
            .await
            .unwrap();

        let request = only_request(&mock_server).await;
        assert_eq!(
            pairs_of(&request),
            pairs(&[
                ("q", "tmpl"),
                ("tags", "a,b"),
                ("status", "active"),
                ("limit", "5"),
                ("offset", "10"),
            ])
        );
    }

    #[tokio::test]
    async fn client_without_filters_sends_only_paging() {
        let mock_server = empty_page_server().await;
        let client = SynsClient::new(&mock_server.uri()).unwrap();

        client.explore(None, None, None, 20, 0).await.unwrap();

        let request = only_request(&mock_server).await;
        assert_eq!(
            pairs_of(&request),
            pairs(&[("limit", "20"), ("offset", "0")])
        );
    }

    #[tokio::test]
    #[serial]
    async fn repeated_tags_reach_the_endpoint_as_one_joined_value() {
        let dir = tempfile::tempdir().unwrap();
        unsafe { std::env::set_var("SYNS_CONFIG_DIR", dir.path()) };

        let mock_server = empty_page_server().await;
        let config = Config::new(Some(&mock_server.uri())).unwrap();
        let output = Output::new(true);

        let result = cmd_explore(
            &config,
            &output,
            None,
            vec!["syns-app".to_string(), "cli".to_string()],
            None,
            20,
            0,
        )
        .await;
        unsafe { std::env::remove_var("SYNS_CONFIG_DIR") };

        assert!(result.is_ok(), "{result:?}");
        let request = only_request(&mock_server).await;
        // Read the raw pairs rather than the set, so two identical
        // `tags` pairs would count twice.
        let tags: Vec<String> = request
            .url
            .query_pairs()
            .filter(|(k, _)| k == "tags")
            .map(|(_, v)| v.into_owned())
            .collect();
        assert_eq!(tags, vec!["syns-app,cli".to_string()]);
        assert!(
            !request.url.query_pairs().any(|(k, _)| k == "tag"),
            "no tag key is sent: {}",
            request.url
        );
    }

    #[tokio::test]
    #[serial]
    async fn short_query_reaches_the_endpoint_and_its_refusal_answers() {
        let dir = tempfile::tempdir().unwrap();
        unsafe { std::env::set_var("SYNS_CONFIG_DIR", dir.path()) };

        let mock_server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/v1/explore"))
            .respond_with(ResponseTemplate::new(422).set_body_json(serde_json::json!({
                "error": "validation_error",
                "message": "q must be at least 3 characters"
            })))
            .mount(&mock_server)
            .await;
        let config = Config::new(Some(&mock_server.uri())).unwrap();
        let output = Output::new(true);

        let result = cmd_explore(
            &config,
            &output,
            Some("ab".to_string()),
            vec![],
            None,
            20,
            0,
        )
        .await;
        unsafe { std::env::remove_var("SYNS_CONFIG_DIR") };

        let err = result.expect_err("the endpoint's refusal answers the run");
        match &err {
            CliError::Api { status, error, .. } => {
                assert_eq!(*status, Some(422));
                assert_eq!(error, "validation_error");
            }
            other => panic!("expected the API refusal, got {other:?}"),
        }
        assert_eq!(err.exit_code(), 1);
        let request = only_request(&mock_server).await;
        let sent = pairs_of(&request);
        assert!(
            sent.contains(&("q".to_string(), "ab".to_string())),
            "q=ab is sent: {}",
            request.url
        );
        assert!(
            !sent.iter().any(|(k, _)| k == "search"),
            "no search key is sent: {}",
            request.url
        );
    }

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
