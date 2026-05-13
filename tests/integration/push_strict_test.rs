//! V044b — `syns push --strict` aborts with exit code 3 and the
//! per-category skip summary on stderr; no wire request is made.

use assert_cmd::Command as AssertCommand;
use serial_test::serial;
use std::fs;
use wiremock::MockServer;

#[test]
#[serial]
fn mixed_text_and_binary_with_strict_exits_3() {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let (server, project_dir, config_dir, cache_dir, mock_uri) = rt.block_on(async {
        let server = MockServer::start().await;
        // No mocks mounted — strict guard must fire before any wire call.

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

    fs::write(project_dir.path().join("README.md"), "hello").unwrap();
    fs::write(project_dir.path().join("logo.png"), b"data\x00more").unwrap();

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
            "--strict",
            project_dir.path().to_str().unwrap(),
        ])
        .output()
        .expect("subprocess output");

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(
        output.status.code(),
        Some(3),
        "exit code should be 3 (PUSH_PARTIAL) — stderr was: {stderr}"
    );
    assert!(
        stderr.contains("push aborted: 1 file(s) were skipped under --strict"),
        "stderr missing one-line abort message — stderr: {stderr}"
    );
    assert!(
        stderr.contains("binary content (1): logo.png"),
        "stderr missing per-category breakdown — stderr: {stderr}"
    );

    // No PUT was made.
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    rt.block_on(async {
        let requests = server.received_requests().await.unwrap();
        assert!(
            requests.is_empty(),
            "strict guard must fire before any wire call; got {} requests",
            requests.len()
        );
    });
}
