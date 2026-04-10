use crate::auth::token::TokenStore;
use crate::client::SynsClient;
use crate::config::Config;
use crate::errors::CliError;
use crate::output::Output;
use crate::repo::resolve::resolve_repo_identity;
use serde_json::json;

pub async fn cmd_history(config: &Config, output: &Output, file: Option<String>, limit: u32) -> Result<(), CliError> {
    let current_dir = std::env::current_dir()
        .map_err(|e| CliError::Io { message: format!("could not determine current directory: {e}") })?;
    let identity = resolve_repo_identity(None, &current_dir)?;
    let owner = identity.owner.ok_or(CliError::RepoIdentityUnknown)?;
    let repo_id = format!("{}/{}", owner, identity.name);
    let token = TokenStore::new(config.credentials_path()).read().ok().flatten();
    let client = SynsClient::new(config.server_url())?;

    if let Some(path) = file {
        let response = client.get_file_history(&repo_id, token.as_deref(), &path, limit).await?;

        if output.is_json() {
            output.json(&json!({
                "commits": response.commits.iter().map(|c| json!({
                    "sha": c.sha,
                    "message": c.message,
                    "author": c.author,
                    "timestamp": c.timestamp,
                    "files_changed": c.files_changed,
                    "file_content": c.file_content,
                    "file_sha": c.file_sha,
                    "file_diff": c.file_diff,
                })).collect::<Vec<_>>(),
                "total": response.total,
                "limit": response.limit,
                "offset": response.offset,
            }));
        } else {
            let rows = response.commits.iter().map(|entry| {
                vec![
                    entry.sha[..entry.sha.len().min(8)].to_string(),
                    entry.message.clone(),
                    entry.author.clone(),
                    entry.timestamp.clone(),
                ]
            }).collect();
            output.table(&["SHA", "Message", "Author", "Date"], rows);
        }
    } else {
        let response = client.list_versions(&repo_id, token.as_deref(), limit, 0).await?;

        if output.is_json() {
            output.json(&json!({
                "versions": response.data.iter().map(|v| json!({
                    "version": v.version,
                    "sha": v.sha,
                    "message": v.message,
                    "author": v.author,
                    "timestamp": v.timestamp,
                    "files_changed": v.files_changed,
                })).collect::<Vec<_>>(),
                "total": response.total,
                "limit": response.limit,
                "offset": response.offset,
            }));
        } else {
            let rows = response.data.iter().map(|entry| {
                let n = entry.files_changed.len();
                vec![
                    entry.version.to_string(),
                    entry.sha[..entry.sha.len().min(8)].to_string(),
                    entry.message.clone(),
                    entry.author.clone(),
                    entry.timestamp.clone(),
                    if n == 1 { "1 file".to_string() } else { format!("{n} files") },
                ]
            }).collect();
            output.table(&["Version", "SHA", "Message", "Author", "Date", "Files"], rows);
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
        ).unwrap();
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
                        "timestamp": "2025-01-03T00:00:00Z",
                        "files_changed": ["a.ts", "b.ts"]
                    },
                    {
                        "version": 2,
                        "sha": "bbb22222bbb22222",
                        "message": "second commit",
                        "author": "alice",
                        "timestamp": "2025-01-02T00:00:00Z",
                        "files_changed": ["a.ts", "c.ts"]
                    },
                    {
                        "version": 1,
                        "sha": "aaa11111aaa11111",
                        "message": "first commit",
                        "author": "alice",
                        "timestamp": "2025-01-01T00:00:00Z",
                        "files_changed": ["a.ts"]
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

        let result = cmd_history(&config, &output, None, 50).await;
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
        ).unwrap();
        std::env::set_current_dir(dir.path()).unwrap();
        unsafe { std::env::set_var("SYNS_CONFIG_DIR", dir.path()) };

        let mock_server = MockServer::start().await;

        Mock::given(method("GET"))
            .and(path("/api/v1/repos/alice/my-project/files/src/main.ts/history"))
            .and(query_param("limit", "50"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "commits": [
                    {
                        "sha": "ccc33333ccc33333",
                        "message": "update main",
                        "author": "alice",
                        "timestamp": "2025-01-03T00:00:00Z",
                        "files_changed": ["src/main.ts"],
                        "file_content": "console.log('v3');",
                        "file_sha": "sha3",
                        "file_diff": "--- a\n+++ b\n@@ -1 +1 @@\n-v2\n+v3"
                    },
                    {
                        "sha": "aaa11111aaa11111",
                        "message": "add main",
                        "author": "alice",
                        "timestamp": "2025-01-01T00:00:00Z",
                        "files_changed": ["src/main.ts"],
                        "file_content": "console.log('v1');",
                        "file_sha": "sha1",
                        "file_diff": null
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

        let result = cmd_history(&config, &output, Some("src/main.ts".to_string()), 50).await;
        unsafe { std::env::remove_var("SYNS_CONFIG_DIR") };

        assert!(result.is_ok());
    }
}
