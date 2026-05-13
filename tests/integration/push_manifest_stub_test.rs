//! V070a / legacy-stub self-heal — server responses with an empty
//! `commitSha` must not persist a manifest, and a pre-existing 29-byte
//! stub must be silently overwritten on the next real push.

use assert_cmd::Command as AssertCommand;
use serde_json::json;
use serial_test::serial;
use std::fs;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

#[test]
#[serial]
fn response_with_empty_commit_sha_does_not_write_manifest() {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let (_server, project_dir, config_dir, cache_dir, mock_uri) = rt.block_on(async {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/v1/repos/alice/repo/tree"))
            .respond_with(ResponseTemplate::new(404).set_body_json(json!({"error": "not_found"})))
            .mount(&server)
            .await;
        // Empty-commit-sha response (server accepted the push but
        // emitted no commit — issue 070's symptom).
        Mock::given(method("PUT"))
            .and(path("/api/v1/repos/alice/repo/push"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "commitSha": "",
                "version": 0,
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

    fs::write(project_dir.path().join("text.txt"), "hello").unwrap();

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
    assert!(output.status.success(), "stderr was: {stderr}");

    // Cache file must NOT have been written.
    let manifest_path = cache_dir.path().join("alice").join("repo.json");
    assert!(
        !manifest_path.exists(),
        "manifest must NOT be persisted when commit_sha is empty"
    );

    // Empty-commit-sha warning is the primary V070a stderr contract.
    // (The first-push-noop banner is a separate format_response branch
    // that only fires when !files_changed && !created — the mock here
    // has created=true so format_response takes the "Pushed to" branch.)
    assert!(
        stderr.contains("empty commit_sha"),
        "stderr missing 'empty commit_sha' warning — stderr: {stderr}"
    );
}

#[test]
#[serial]
fn legacy_stub_is_overwritten_on_next_real_push() {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let (_server, project_dir, config_dir, cache_dir, mock_uri) = rt.block_on(async {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/v1/repos/alice/repo/tree"))
            .respond_with(ResponseTemplate::new(404).set_body_json(json!({"error": "not_found"})))
            .mount(&server)
            .await;
        Mock::given(method("PUT"))
            .and(path("/api/v1/repos/alice/repo/push"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "commitSha": "realsha0000000000000000000000000000000000",
                "version": 1,
                "filesChanged": 1,
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

    fs::write(project_dir.path().join("text.txt"), "hello").unwrap();

    // Pre-seed the legacy stub.
    let manifest_path = cache_dir.path().join("alice").join("repo.json");
    fs::create_dir_all(manifest_path.parent().unwrap()).unwrap();
    fs::write(&manifest_path, r#"{"commit_sha":"","files":{}}"#).unwrap();

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
    assert!(output.status.success(), "stderr was: {stderr}");

    // Cache file should now contain a real manifest.
    let body = fs::read_to_string(&manifest_path).unwrap();
    let v: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(
        v["commit_sha"].as_str(),
        Some("realsha0000000000000000000000000000000000")
    );
    assert!(v["files"].as_object().unwrap().contains_key("text.txt"));
}
