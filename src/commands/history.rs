use crate::auth::token::TokenStore;
use crate::client::SynsClient;
use crate::config::Config;
use crate::errors::CliError;
use crate::output::Output;
use crate::repo::if_repo::resolve_full_or_skip;

pub async fn cmd_history(
    config: &Config,
    output: &Output,
    file: Option<String>,
    limit: u32,
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
    let token = TokenStore::new(config.credentials_path())
        .read()
        .ok()
        .flatten();
    let client = SynsClient::new(config.server_url())?;

    if let Some(path) = file {
        let (response, raw) = client
            .get_file_history(&repo_id, token.as_deref(), &path, limit)
            .await?;

        if output.is_json() {
            output.json(&raw);
        } else {
            let rows = response
                .data
                .iter()
                .map(|entry| {
                    vec![
                        entry.sha[..entry.sha.len().min(8)].to_string(),
                        entry.message.clone(),
                        entry.author.clone(),
                        entry.created_at.clone(),
                    ]
                })
                .collect();
            output.table(&["SHA", "Message", "Author", "Date"], rows);
        }
    } else {
        let (response, raw) = client
            .list_versions(&repo_id, token.as_deref(), limit, 0)
            .await?;

        if output.is_json() {
            output.json(&raw);
        } else {
            let rows = response
                .data
                .iter()
                .map(|entry| {
                    let n = entry.files_changed.len();
                    vec![
                        entry.version.to_string(),
                        entry.sha[..entry.sha.len().min(8)].to_string(),
                        entry.message.clone(),
                        entry.author.clone(),
                        entry.created_at.clone(),
                        if n == 1 {
                            "1 file".to_string()
                        } else {
                            format!("{n} files")
                        },
                    ]
                })
                .collect();
            output.table(
                &["Version", "SHA", "Message", "Author", "Date", "Files"],
                rows,
            );
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serial_test::serial;
    use wiremock::matchers::{method, path, query_param};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    #[tokio::test]
    #[serial]
    async fn history_shows_commits_in_reverse_chronological_order() {
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
            .and(path("/api/v1/repos/alice/my-project/versions"))
            .and(query_param("limit", "50"))
            .and(query_param("offset", "0"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "data": [
                    {
                        "version": 3,
                        "sha": "ccc33333ccc33333",
                        "message": "third commit",
                        "author": "alice",
                        "createdAt": "2025-01-03T00:00:00Z",
                        "filesChanged": ["a.ts", "b.ts"]
                    },
                    {
                        "version": 2,
                        "sha": "bbb22222bbb22222",
                        "message": "second commit",
                        "author": "alice",
                        "createdAt": "2025-01-02T00:00:00Z",
                        "filesChanged": ["a.ts", "c.ts"]
                    },
                    {
                        "version": 1,
                        "sha": "aaa11111aaa11111",
                        "message": "first commit",
                        "author": "alice",
                        "createdAt": "2025-01-01T00:00:00Z",
                        "filesChanged": ["a.ts"]
                    }
                ],
                "total": 3,
                "limit": 50,
                "offset": 0
            })))
            .mount(&mock_server)
            .await;

        let config = Config::new(Some(&mock_server.uri())).unwrap();
        let output = Output::new(false);

        let result = cmd_history(&config, &output, None, 50, false).await;
        unsafe { std::env::remove_var("SYNS_CONFIG_DIR") };

        assert!(result.is_ok());
    }

    #[tokio::test]
    #[serial]
    async fn history_file_filters_to_file_commits() {
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
            .and(path(
                "/api/v1/repos/alice/my-project/files/src/main.ts/history",
            ))
            .and(query_param("limit", "50"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "data": [
                    {
                        "version": 3,
                        "sha": "ccc33333ccc33333",
                        "blobSha": "blob3",
                        "message": "update main",
                        "author": "alice",
                        "createdAt": "2025-01-03T00:00:00Z",
                        "content": "console.log('v3');",
                        "diff": "--- a\n+++ b\n@@ -1 +1 @@\n-v2\n+v3"
                    },
                    {
                        "version": 1,
                        "sha": "aaa11111aaa11111",
                        "blobSha": "blob1",
                        "message": "add main",
                        "author": "alice",
                        "createdAt": "2025-01-01T00:00:00Z",
                        "content": "console.log('v1');",
                        "diff": null
                    }
                ],
                "total": 2,
                "limit": 50,
                "offset": 0
            })))
            .mount(&mock_server)
            .await;

        let config = Config::new(Some(&mock_server.uri())).unwrap();
        let output = Output::new(false);

        let result =
            cmd_history(&config, &output, Some("src/main.ts".to_string()), 50, false).await;
        unsafe { std::env::remove_var("SYNS_CONFIG_DIR") };

        assert!(result.is_ok());
    }

    #[tokio::test]
    #[serial]
    async fn history_with_if_repo_set_and_identity_resolved_runs_normally() {
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
            .and(path("/api/v1/repos/alice/my-project/versions"))
            .and(query_param("limit", "50"))
            .and(query_param("offset", "0"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "data": [],
                "total": 0,
                "limit": 50,
                "offset": 0
            })))
            .mount(&mock_server)
            .await;

        let config = Config::new(Some(&mock_server.uri())).unwrap();
        let output = Output::new(false);

        let result = cmd_history(&config, &output, None, 50, true).await;
        unsafe { std::env::remove_var("SYNS_CONFIG_DIR") };

        assert!(result.is_ok());
    }

    #[tokio::test]
    #[serial]
    async fn history_with_if_repo_set_and_no_identity_skips_silently() {
        let dir = tempfile::tempdir().unwrap();
        std::env::set_current_dir(dir.path()).unwrap();
        unsafe { std::env::set_var("SYNS_CONFIG_DIR", dir.path()) };

        let mock_server = MockServer::start().await;
        let config = Config::new(Some(&mock_server.uri())).unwrap();
        let output = Output::new(false);

        let result = cmd_history(&config, &output, None, 50, true).await;
        unsafe { std::env::remove_var("SYNS_CONFIG_DIR") };

        assert!(result.is_ok());
        assert!(mock_server.received_requests().await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn history_list_versions_raw_includes_parent_sha_and_message_body() {
        let mock_server = MockServer::start().await;
        let body = r#"{"data":[{"version":2,"sha":"bbb22222","parentSha":"aaa11111","message":"second","messageBody":"detailed body\nwith multiple lines","author":"alice","createdAt":"2026-01-02T00:00:00Z","filesChanged":["a.ts"]}],"total":2,"limit":50,"offset":0}"#;
        Mock::given(method("GET"))
            .and(path("/api/v1/repos/alice/my-project/versions"))
            .and(query_param("limit", "50"))
            .and(query_param("offset", "0"))
            .respond_with(ResponseTemplate::new(200).set_body_string(body))
            .mount(&mock_server)
            .await;

        let client = SynsClient::new(&mock_server.uri()).unwrap();
        let (typed, raw) = client
            .list_versions("alice/my-project", None, 50, 0)
            .await
            .unwrap();

        assert!(raw["data"][0].get("parentSha").is_some());
        assert_eq!(raw["data"][0]["parentSha"], serde_json::json!("aaa11111"));
        assert!(raw["data"][0].get("messageBody").is_some());
        assert!(
            raw["data"][0]["messageBody"]
                .as_str()
                .unwrap()
                .contains("detailed body")
        );
        assert_eq!(typed.data[0].version, 2);
        let expected: serde_json::Value = serde_json::from_str(body).unwrap();
        assert_eq!(raw, expected);
    }

    #[tokio::test]
    async fn history_get_file_history_raw_preserves_blob_sha_and_content_and_diff() {
        let mock_server = MockServer::start().await;
        let body = r#"{"data":[{"version":3,"sha":"ccc33333","blobSha":"blob3","message":"update main","author":"alice","createdAt":"2026-01-03T00:00:00Z","content":"console.log('v3');","diff":"--- a\n+++ b\n@@ -1 +1 @@\n-v2\n+v3"},{"version":1,"sha":"aaa11111","blobSha":"blob1","message":"add main","author":"alice","createdAt":"2026-01-01T00:00:00Z","content":"console.log('v1');","diff":null}],"total":2,"limit":50,"offset":0}"#;
        Mock::given(method("GET"))
            .and(path(
                "/api/v1/repos/alice/my-project/files/src/main.ts/history",
            ))
            .and(query_param("limit", "50"))
            .respond_with(ResponseTemplate::new(200).set_body_string(body))
            .mount(&mock_server)
            .await;

        let client = SynsClient::new(&mock_server.uri()).unwrap();
        let (typed, raw) = client
            .get_file_history("alice/my-project", None, "src/main.ts", 50)
            .await
            .unwrap();

        assert!(raw["data"][0].get("blobSha").is_some());
        assert_eq!(raw["data"][0]["blobSha"], serde_json::json!("blob3"));
        assert!(raw["data"][0].get("content").is_some());
        assert_eq!(
            raw["data"][0]["content"],
            serde_json::json!("console.log('v3');")
        );
        assert!(raw["data"][0].get("diff").is_some());
        assert!(raw["data"][0]["diff"].as_str().unwrap().contains("+v3"));
        // The second entry has `diff: null` — verify the null is preserved verbatim.
        assert!(raw["data"][1].get("diff").is_some());
        assert!(raw["data"][1]["diff"].is_null());
        assert_eq!(typed.data[0].version, 3);
        let expected: serde_json::Value = serde_json::from_str(body).unwrap();
        assert_eq!(raw, expected);
    }
}
