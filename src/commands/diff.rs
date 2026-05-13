use crate::auth::token::TokenStore;
use crate::client::{DiffStatus, SynsClient};
use crate::config::Config;
use crate::errors::CliError;
use crate::output::Output;
use crate::repo::if_repo::resolve_full_or_skip;
use console::style;
use serde_json::json;

fn status_str(status: &DiffStatus) -> &'static str {
    match status {
        DiffStatus::Added => "added",
        DiffStatus::Modified => "modified",
        DiffStatus::Deleted => "deleted",
        DiffStatus::Unknown => "unknown",
    }
}

fn style_status(status: &DiffStatus) -> console::StyledObject<&'static str> {
    match status {
        DiffStatus::Added => style("added").green(),
        DiffStatus::Modified => style("modified").yellow(),
        DiffStatus::Deleted => style("deleted").red(),
        DiffStatus::Unknown => style("unknown").dim(),
    }
}

pub async fn cmd_diff(
    config: &Config,
    output: &Output,
    from: Option<String>,
    to: Option<String>,
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

    let (from_val, to_val) = match (from, to) {
        (Some(f), Some(t)) => (f, t),
        (None, None) => {
            let response = client
                .list_versions(&repo_id, token.as_deref(), 2, 0)
                .await?;
            if response.data.len() < 2 {
                return Err(CliError::Api {
                    status: None,
                    error: "repository has fewer than 2 versions — cannot diff".to_string(),
                    context: None,
                });
            }
            (
                response.data[1].version.to_string(),
                response.data[0].version.to_string(),
            )
        }
        _ => {
            return Err(CliError::Config {
                message: "both --from and --to must be specified, or neither".to_string(),
            });
        }
    };

    let response = client
        .get_diff(&repo_id, token.as_deref(), &from_val, &to_val)
        .await?;

    if output.is_json() {
        output.json(&json!({
            "from": { "version": response.from.version, "sha": response.from.sha },
            "to": { "version": response.to.version, "sha": response.to.sha },
            "files": response.files.iter().map(|e| json!({
                "path": e.path,
                "status": status_str(&e.status),
                "diff": e.diff,
            })).collect::<Vec<_>>(),
        }));
    } else {
        if response.files.is_empty() {
            output.success("No differences found.");
            return Ok(());
        }
        for (i, entry) in response.files.iter().enumerate() {
            eprintln!("{}  {}", style_status(&entry.status), entry.path);
            if let Some(ref diff) = entry.diff {
                println!("{}", diff);
            }
            if i < response.files.len() - 1 {
                println!();
            }
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
    async fn diff_displays_unified_diffs_between_versions() {
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
            .and(path("/api/v1/repos/alice/my-project/diff"))
            .and(query_param("from", "1"))
            .and(query_param("to", "3"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "from": { "version": 1, "sha": "aaa11111" },
                "to": { "version": 3, "sha": "ccc33333" },
                "files": [
                    {
                        "path": "src/config.ts",
                        "status": "modified",
                        "diff": "--- a/src/config.ts\n+++ b/src/config.ts\n@@ -1 +1 @@\n-old\n+new"
                    },
                    {
                        "path": "src/new.ts",
                        "status": "added",
                        "diff": "--- /dev/null\n+++ b/src/new.ts\n@@ -0,0 +1 @@\n+content"
                    }
                ]
            })))
            .mount(&mock_server)
            .await;

        let config = Config::new(Some(&mock_server.uri())).unwrap();
        let output = Output::new(false);

        let result = cmd_diff(
            &config,
            &output,
            Some("1".to_string()),
            Some("3".to_string()),
            false,
        )
        .await;
        unsafe { std::env::remove_var("SYNS_CONFIG_DIR") };

        assert!(result.is_ok());
    }

    #[tokio::test]
    #[serial]
    async fn diff_auto_detects_latest_two_versions() {
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
            .and(query_param("limit", "2"))
            .and(query_param("offset", "0"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "data": [
                    {
                        "version": 5,
                        "sha": "eee55555",
                        "message": "fifth",
                        "author": "alice",
                        "createdAt": "2025-01-05T00:00:00Z",
                        "filesChanged": ["a.ts"]
                    },
                    {
                        "version": 4,
                        "sha": "ddd44444",
                        "message": "fourth",
                        "author": "alice",
                        "createdAt": "2025-01-04T00:00:00Z",
                        "filesChanged": ["b.ts"]
                    }
                ],
                "total": 5,
                "limit": 2,
                "offset": 0
            })))
            .mount(&mock_server)
            .await;

        Mock::given(method("GET"))
            .and(path("/api/v1/repos/alice/my-project/diff"))
            .and(query_param("from", "4"))
            .and(query_param("to", "5"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "from": { "version": 4, "sha": "ddd44444" },
                "to": { "version": 5, "sha": "eee55555" },
                "files": [
                    {
                        "path": "a.ts",
                        "status": "modified",
                        "diff": "--- a/a.ts\n+++ b/a.ts\n@@ -1 +1 @@\n-v4\n+v5"
                    }
                ]
            })))
            .mount(&mock_server)
            .await;

        let config = Config::new(Some(&mock_server.uri())).unwrap();
        let output = Output::new(false);

        let result = cmd_diff(&config, &output, None, None, false).await;
        unsafe { std::env::remove_var("SYNS_CONFIG_DIR") };

        assert!(result.is_ok());
    }

    #[tokio::test]
    #[serial]
    async fn diff_rejects_mixed_from_to() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join(".syns.yaml"),
            "owner: alice\nname: my-project\n",
        )
        .unwrap();
        std::env::set_current_dir(dir.path()).unwrap();
        unsafe { std::env::set_var("SYNS_CONFIG_DIR", dir.path()) };

        let mock_server = MockServer::start().await;
        let config = Config::new(Some(&mock_server.uri())).unwrap();
        let output = Output::new(false);

        let result = cmd_diff(&config, &output, Some("1".to_string()), None, false).await;
        unsafe { std::env::remove_var("SYNS_CONFIG_DIR") };

        assert!(matches!(result, Err(CliError::Config { .. })));
    }

    #[tokio::test]
    #[serial]
    async fn diff_with_if_repo_set_and_identity_resolved_runs_normally() {
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
            .and(path("/api/v1/repos/alice/my-project/diff"))
            .and(query_param("from", "1"))
            .and(query_param("to", "3"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "from": { "version": 1, "sha": "aaa11111" },
                "to": { "version": 3, "sha": "ccc33333" },
                "files": []
            })))
            .mount(&mock_server)
            .await;

        let config = Config::new(Some(&mock_server.uri())).unwrap();
        let output = Output::new(false);

        let result = cmd_diff(
            &config,
            &output,
            Some("1".to_string()),
            Some("3".to_string()),
            true,
        )
        .await;
        unsafe { std::env::remove_var("SYNS_CONFIG_DIR") };

        assert!(result.is_ok());
    }

    #[tokio::test]
    #[serial]
    async fn diff_with_if_repo_set_and_no_identity_skips_silently() {
        let dir = tempfile::tempdir().unwrap();
        std::env::set_current_dir(dir.path()).unwrap();
        unsafe { std::env::set_var("SYNS_CONFIG_DIR", dir.path()) };

        let mock_server = MockServer::start().await;
        let config = Config::new(Some(&mock_server.uri())).unwrap();
        let output = Output::new(false);

        let result = cmd_diff(&config, &output, None, None, true).await;
        unsafe { std::env::remove_var("SYNS_CONFIG_DIR") };

        assert!(result.is_ok());
        assert!(mock_server.received_requests().await.unwrap().is_empty());
    }
}
