use crate::auth::token::TokenStore;
use crate::client::{CommitProvenance, SynsClient};
use crate::config::Config;
use crate::errors::CliError;
use crate::output::Output;
use crate::read::{
    RepoScopeArgs, refuse_version_below_one, resolve_repo_scope, version_not_found_refusal,
};
use crate::repo::if_repo::resolve_full_or_skip;

/// A version row's provenance cell: the publisher and each asserted
/// field under its label, where the commit recorded provenance and either
/// asserted a field or was published by someone other than its author;
/// empty otherwise.
pub(crate) fn provenance_cell(provenance: Option<&CommitProvenance>, author: &str) -> String {
    let Some(provenance) = provenance else {
        return String::new();
    };
    let asserted = [
        ("Integration", &provenance.integration),
        ("Run", &provenance.run),
        ("Trigger", &provenance.trigger),
        ("Task", &provenance.task_ref),
    ];
    if asserted.iter().all(|(_, value)| value.is_none()) && provenance.publisher == author {
        return String::new();
    }
    let mut lines = vec![format!("Published by: {}", provenance.publisher)];
    for (label, value) in asserted {
        if let Some(value) = value {
            lines.push(format!("{label}: {value}"));
        }
    }
    lines.join("\n")
}

/// The verbs standing under the history noun (SPEC u272). The bare
/// noun keeps listing versions; `show` reads one.
#[derive(clap::Subcommand, Debug)]
pub enum HistoryAction {
    /// Show one version's whole record
    Show {
        /// The version to show — an ordinal, a commit hash, or any
        /// other spelling the server's own reference form admits
        #[arg(value_name = "REF")]
        reference: String,
        #[command(flatten)]
        scope: RepoScopeArgs,
    },
}

/// Reads one version's record (SPEC u272 Behaviour, `cmd_history_show`).
///
/// The reference reaches the wire as the caller typed it. `NR-03` leaves
/// what the single-version entry answers for a spelling that is neither
/// all-digit nor a full hash unmeasured, so the binary adds no local
/// check for one and reports whichever form the entry served.
pub async fn cmd_history_show(
    config: &Config,
    output: &Output,
    reference: String,
    args: RepoScopeArgs,
) -> Result<(), CliError> {
    // 1 — resolve the scope, and refuse an all-digit reference below 1.
    let scope = match resolve_repo_scope(config, output, &args).await? {
        Some(scope) => scope,
        None => return Ok(()),
    };
    refuse_version_below_one(&reference)?;

    // 2 — ask the single-version entry for the reference as typed.
    let client = SynsClient::new(config.server_url())?;
    let (entry, raw) = client
        .get_version(&scope.repo_id, scope.token.as_deref(), &reference)
        .await
        .map_err(|e| e.with_versioned_read_context(version_not_found_refusal(&reference)))?;

    // 3 — render the record, or write the served body.
    if output.is_json() {
        output.json(&raw);
        return Ok(());
    }
    output.table(&["Property", "Value"], version_rows(&entry));
    Ok(())
}

/// The version block (SPEC u272 Behaviour, `cmd_history_show` 3): the
/// ordinal, the hash, the parent, the caption and body, the author, the
/// instant, the changed paths, and the provenance where the publication
/// asserted one. A key the version carries nothing under draws no row.
pub(crate) fn version_rows(entry: &crate::client::VersionEntry) -> Vec<Vec<String>> {
    let mut rows = vec![
        vec!["Version".to_string(), entry.version.to_string()],
        vec!["Commit".to_string(), entry.sha.clone()],
    ];
    if let Some(parent) = &entry.parent_sha {
        rows.push(vec!["Parent".to_string(), parent.clone()]);
    }
    rows.push(vec!["Message".to_string(), entry.message.clone()]);
    if let Some(body) = &entry.message_body {
        rows.push(vec!["Body".to_string(), body.clone()]);
    }
    rows.push(vec!["Author".to_string(), entry.author.clone()]);
    rows.push(vec!["Date".to_string(), entry.created_at.clone()]);
    rows.push(vec!["Files".to_string(), entry.files_changed.join("\n")]);
    let provenance = provenance_cell(entry.provenance.as_ref(), &entry.author);
    if !provenance.is_empty() {
        rows.push(vec!["Provenance".to_string(), provenance]);
    }
    rows
}

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
                    // A removal entry — the commit that removed the path —
                    // serves neither a blob hash nor content.
                    let message = if entry.blob_sha.is_none() && entry.content.is_none() {
                        format!("(removed) {}", entry.message)
                    } else {
                        entry.message.clone()
                    };
                    vec![
                        entry.sha[..entry.sha.len().min(8)].to_string(),
                        message,
                        entry.author.clone(),
                        entry.created_at.clone(),
                        provenance_cell(entry.provenance.as_ref(), &entry.author),
                    ]
                })
                .collect();
            output.table(&["SHA", "Message", "Author", "Date", "Provenance"], rows);
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
                        provenance_cell(entry.provenance.as_ref(), &entry.author),
                    ]
                })
                .collect();
            output.table(
                &[
                    "Version",
                    "SHA",
                    "Message",
                    "Author",
                    "Date",
                    "Files",
                    "Provenance",
                ],
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

    #[test]
    fn provenance_cell_names_what_the_commit_asserted() {
        let asserted = CommitProvenance {
            publisher: "ana".into(),
            integration: Some("claude-code".into()),
            run: Some("session-9".into()),
            trigger: Some("stop".into()),
            task_ref: None,
        };
        assert_eq!(
            provenance_cell(Some(&asserted), "ana"),
            "Published by: ana\nIntegration: claude-code\nRun: session-9\nTrigger: stop"
        );

        let other_publisher = CommitProvenance {
            publisher: "bo".into(),
            integration: None,
            run: None,
            trigger: None,
            task_ref: None,
        };
        assert_eq!(
            provenance_cell(Some(&other_publisher), "ana"),
            "Published by: bo"
        );

        assert_eq!(provenance_cell(Some(&other_publisher), "bo"), "");
        assert_eq!(provenance_cell(None, "ana"), "");
    }

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

    #[tokio::test]
    async fn get_file_history_returns_a_page_holding_a_removal_entry() {
        let mock_server = MockServer::start().await;
        let body = serde_json::json!({
            "data": [
                {
                    "version": 439,
                    "sha": "739d8dc0c095de7ab390d685c0e2e8629d61db1f",
                    "blobSha": "d54fa145810ef1ad6183d93229bb2982571cc3da",
                    "message": "claude code session (part 3/3)",
                    "author": "bartsoj",
                    "createdAt": "2026-09-13T14:34:58Z",
                    "content": "# syns",
                    "diff": "--- /dev/null\n+++ b/CLAUDE.md\n@@ -0,0 +1 @@\n+# syns\n"
                },
                {
                    "version": 436,
                    "sha": "ff7f52cad5c73554fff96676478cd4b2a509fbdc",
                    "blobSha": null,
                    "message": "claude code session",
                    "author": "bartsoj",
                    "createdAt": "2026-09-13T14:31:20Z",
                    "content": null,
                    "diff": "--- a/CLAUDE.md\n+++ /dev/null\n@@ -1 +0,0 @@\n-# syns\n"
                },
                {
                    "version": 435,
                    "sha": "d1781077fe841cf6422624473c79f370ec24780c",
                    "blobSha": "d54fa145810ef1ad6183d93229bb2982571cc3da",
                    "message": "claude code session (part 3/3)",
                    "author": "bartsoj",
                    "createdAt": "2026-09-13T13:58:41Z",
                    "content": "# syns",
                    "diff": "--- /dev/null\n+++ b/CLAUDE.md\n@@ -0,0 +1 @@\n+# syns\n"
                }
            ],
            "total": 50,
            "limit": 3,
            "offset": 0
        });
        Mock::given(method("GET"))
            .and(path(
                "/api/v1/repos/alice/my-project/files/CLAUDE.md/history",
            ))
            .and(query_param("limit", "3"))
            .respond_with(ResponseTemplate::new(200).set_body_string(body.to_string()))
            .mount(&mock_server)
            .await;

        let client = SynsClient::new(&mock_server.uri()).unwrap();
        let (typed, raw) = client
            .get_file_history("alice/my-project", None, "CLAUDE.md", 3)
            .await
            .unwrap();

        let versions: Vec<u32> = typed.data.iter().map(|entry| entry.version).collect();
        assert_eq!(versions, [439, 436, 435]);
        assert!(typed.data[1].blob_sha.is_none());
        assert!(typed.data[1].content.is_none());
        assert_eq!(raw, body);
    }

    // --- `syns history show REF` (SPEC u272) ---

    use crate::client::u272_bodies as B;
    use crate::read::RepoScopeArgs;

    // SPEC u272 Behaviour, `cmd_history_show` 1: an all-digit reference
    // below `1` is refused before any request.
    #[tokio::test]
    #[serial]
    async fn an_ordinal_below_one_is_refused_before_any_request() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(".syns.yaml"), "owner: alice\nname: notes\n").unwrap();
        std::env::set_current_dir(dir.path()).unwrap();
        unsafe { std::env::set_var("SYNS_CONFIG_DIR", dir.path()) };
        let server = MockServer::start().await;
        let config = Config::new(Some(&server.uri())).unwrap();
        let output = Output::new(false);

        let err = cmd_history_show(&config, &output, "0".into(), RepoScopeArgs::default())
            .await
            .unwrap_err();
        unsafe { std::env::remove_var("SYNS_CONFIG_DIR") };

        assert_eq!(
            err.to_string(),
            "configuration error: version must be \u{2265} 1"
        );
        assert_eq!(err.exit_code(), 1);
        assert!(server.received_requests().await.unwrap().is_empty());
    }

    // SPEC u272 Behaviour, `cmd_history_show` 2 and 3: the served
    // version is rendered whole, the parent and the body among it.
    #[tokio::test]
    #[serial]
    async fn a_served_version_is_rendered_whole() {
        let dir = tempfile::tempdir().unwrap();
        std::env::set_current_dir(dir.path()).unwrap();
        unsafe { std::env::set_var("SYNS_CONFIG_DIR", dir.path()) };
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/v1/repos/bartsoj/syns/versions/596"))
            .respond_with(ResponseTemplate::new(200).set_body_string(B::VERSION_HEAD))
            .mount(&server)
            .await;
        let config = Config::new(Some(&server.uri())).unwrap();
        let output = Output::new(false);

        let result = cmd_history_show(
            &config,
            &output,
            "596".into(),
            RepoScopeArgs {
                repo: Some("bartsoj/syns".into()),
                if_repo: false,
            },
        )
        .await;
        unsafe { std::env::remove_var("SYNS_CONFIG_DIR") };

        assert!(result.is_ok(), "got {:?}", result.err());
        assert_eq!(server.received_requests().await.unwrap().len(), 1);

        let entry: crate::client::VersionEntry = serde_json::from_str(B::VERSION_HEAD).unwrap();
        let rows = version_rows(&entry);
        let labels: Vec<&str> = rows.iter().map(|r| r[0].as_str()).collect();
        assert_eq!(
            labels,
            vec![
                "Version", "Commit", "Parent", "Message", "Author", "Date", "Files"
            ]
        );
        assert_eq!(rows[2][1], "686ee7156c28aca8f7d9411c3f1a50631257d59d");
    }

    // SPEC u272 Tests, "history show names the reference on a miss".
    #[tokio::test]
    #[serial]
    async fn a_miss_names_the_reference_the_caller_typed() {
        let dir = tempfile::tempdir().unwrap();
        std::env::set_current_dir(dir.path()).unwrap();
        unsafe { std::env::set_var("SYNS_CONFIG_DIR", dir.path()) };
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/v1/repos/alice/notes/versions/9"))
            .respond_with(ResponseTemplate::new(404).set_body_json(serde_json::json!({
                "error": "not_found", "message": "Version not found",
            })))
            .mount(&server)
            .await;
        let config = Config::new(Some(&server.uri())).unwrap();
        let output = Output::new(false);

        let err = cmd_history_show(
            &config,
            &output,
            "9".into(),
            RepoScopeArgs {
                repo: Some("alice/notes".into()),
                if_repo: false,
            },
        )
        .await
        .unwrap_err();
        unsafe { std::env::remove_var("SYNS_CONFIG_DIR") };

        assert_eq!(err.to_string(), "version not found: 9");
        assert_eq!(err.exit_code(), 1);
    }

    // SPEC u272 Contract Surface, `cmd_history_show`: the reference
    // reaches the wire as the caller typed it, `NR-03` leaving what the
    // entry answers for it unmeasured and the binary adding no check.
    #[tokio::test]
    #[serial]
    async fn a_short_hash_reaches_the_wire_as_typed() {
        let dir = tempfile::tempdir().unwrap();
        std::env::set_current_dir(dir.path()).unwrap();
        unsafe { std::env::set_var("SYNS_CONFIG_DIR", dir.path()) };
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/v1/repos/alice/notes/versions/7618bcf"))
            .respond_with(ResponseTemplate::new(200).set_body_string(B::VERSION_HEAD))
            .mount(&server)
            .await;
        let config = Config::new(Some(&server.uri())).unwrap();
        let output = Output::new(true);

        let result = cmd_history_show(
            &config,
            &output,
            "7618bcf".into(),
            RepoScopeArgs {
                repo: Some("alice/notes".into()),
                if_repo: false,
            },
        )
        .await;
        unsafe { std::env::remove_var("SYNS_CONFIG_DIR") };

        assert!(result.is_ok(), "got {:?}", result.err());
        let paths: Vec<String> = server
            .received_requests()
            .await
            .unwrap()
            .iter()
            .map(|r| r.url.path().to_string())
            .collect();
        assert_eq!(paths, vec!["/api/v1/repos/alice/notes/versions/7618bcf"]);
    }

    // The body row stands where the commit carried one.
    #[test]
    fn the_body_row_stands_only_where_the_commit_carried_one() {
        let entry: crate::client::VersionEntry = serde_json::from_value(serde_json::json!({
            "version": 2, "sha": "b".repeat(40), "parentSha": null,
            "message": "caption", "messageBody": "the long half",
            "author": "alice", "createdAt": "2026-01-01T00:00:00Z",
            "filesChanged": ["a.md"],
        }))
        .unwrap();
        let rows = version_rows(&entry);
        let labels: Vec<&str> = rows.iter().map(|r| r[0].as_str()).collect();
        assert_eq!(
            labels,
            vec![
                "Version", "Commit", "Message", "Body", "Author", "Date", "Files"
            ]
        );
        assert_eq!(rows[3][1], "the long half");
    }
}
