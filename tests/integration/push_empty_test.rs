//! V066A — empty-collection guard fires with exit 6 and a cause
//! diagnostic; `--allow-empty` bypasses the guard.

use assert_cmd::Command as AssertCommand;
use serde_json::json;
use serial_test::serial;
use std::fs;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

#[test]
#[serial]
fn empty_collection_exits_6_with_cause_diagnostic() {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let (server, project_dir, config_dir, cache_dir, mock_uri) = rt.block_on(async {
        let server = MockServer::start().await;
        // No mocks mounted — empty guard must fire before any wire call.

        let project_dir = tempfile::tempdir().unwrap();
        let config_dir = tempfile::tempdir().unwrap();
        let cache_dir = tempfile::tempdir().unwrap();
        let uri = server.uri();

        unsafe { std::env::set_var("SYNS_CONFIG_DIR", config_dir.path()) };
        let config = syns_cli::config::Config::new(Some(&uri)).unwrap();
        let store = syns_cli::auth::token::TokenStore::new(config.credentials_path());
        store
            .write_with_username("test-token", Some("alice"))
            .unwrap();
        unsafe { std::env::remove_var("SYNS_CONFIG_DIR") };

        (server, project_dir, config_dir, cache_dir, uri)
    });

    // .gitignore: * — every file (including .syns.yaml that Phase 1
    // auto-writes) is gitignored, so files is empty.
    fs::write(project_dir.path().join(".gitignore"), "*\n").unwrap();

    let output = AssertCommand::cargo_bin("syns")
        .expect("syns binary")
        .env("SYNS_CONFIG_DIR", config_dir.path())
        .env("SYNS_CACHE_DIR", cache_dir.path())
        .args([
            "--server",
            &mock_uri,
            "push",
            "--name",
            "repo",
            project_dir.path().to_str().unwrap(),
        ])
        .output()
        .expect("subprocess output");

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(
        output.status.code(),
        Some(6),
        "exit code should be 6 (PUSH_EMPTY) — stderr was: {stderr}"
    );
    assert!(
        stderr.contains("nothing to push"),
        "stderr missing 'nothing to push' — stderr: {stderr}"
    );
    assert!(
        stderr.contains(".gitignore file in") || stderr.contains("excluded"),
        "stderr missing cause diagnostic — stderr: {stderr}"
    );
    assert!(
        stderr.contains("--allow-empty"),
        "stderr missing --allow-empty hint — stderr: {stderr}"
    );

    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    rt.block_on(async {
        let requests = server.received_requests().await.unwrap();
        assert!(
            requests.is_empty(),
            "empty guard must fire before any wire call"
        );
    });
}

#[test]
#[serial]
fn allow_empty_lets_empty_collection_through() {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let (server, project_dir, config_dir, cache_dir, mock_uri) = rt.block_on(async {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/v1/repos/alice/repo/tree"))
            .respond_with(ResponseTemplate::new(404).set_body_json(json!({"error": "not_found"})))
            .mount(&server)
            .await;
        Mock::given(method("PUT"))
            .and(path("/api/v1/repos/alice/repo/push"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "commitSha": "abcd",
                "version": 1,
                "filesChanged": 0,
                "created": true
            })))
            .mount(&server)
            .await;

        let project_dir = tempfile::tempdir().unwrap();
        let config_dir = tempfile::tempdir().unwrap();
        let cache_dir = tempfile::tempdir().unwrap();
        let uri = server.uri();

        unsafe { std::env::set_var("SYNS_CONFIG_DIR", config_dir.path()) };
        let config = syns_cli::config::Config::new(Some(&uri)).unwrap();
        let store = syns_cli::auth::token::TokenStore::new(config.credentials_path());
        store
            .write_with_username("test-token", Some("alice"))
            .unwrap();
        unsafe { std::env::remove_var("SYNS_CONFIG_DIR") };

        (server, project_dir, config_dir, cache_dir, uri)
    });

    // .gitignore: * → collection empty; --allow-empty bypasses the guard.
    fs::write(project_dir.path().join(".gitignore"), "*\n").unwrap();

    let output = AssertCommand::cargo_bin("syns")
        .expect("syns binary")
        .env("SYNS_CONFIG_DIR", config_dir.path())
        .env("SYNS_CACHE_DIR", cache_dir.path())
        .args([
            "--server",
            &mock_uri,
            "push",
            "--name",
            "repo",
            "--allow-empty",
            project_dir.path().to_str().unwrap(),
        ])
        .output()
        .expect("subprocess output");

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success(),
        "with --allow-empty, push should succeed — stderr was: {stderr}"
    );

    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    rt.block_on(async {
        let requests = server.received_requests().await.unwrap();
        let put_requests: Vec<_> = requests
            .iter()
            .filter(|r| r.method == reqwest::Method::PUT)
            .collect();
        assert_eq!(put_requests.len(), 1, "exactly one PUT expected");
    });
}
