//! V066B / T1 — the dotfiles-bare-repo reproduction. A hostile
//! ancestor `.gitignore: *` must NOT empty a push from a subdirectory.

use assert_cmd::Command as AssertCommand;
use serde_json::json;
use serial_test::serial;
use std::fs;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

#[test]
#[serial]
fn parent_gitignore_star_does_not_empty_push() {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let (server, ancestor_dir, config_dir, cache_dir, mock_uri) = rt.block_on(async {
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
                "filesChanged": 2,
                "created": true
            })))
            .mount(&server)
            .await;

        let ancestor = tempfile::tempdir().unwrap();
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

        (server, ancestor, config_dir, cache_dir, uri)
    });

    // Hostile ancestor .gitignore.
    fs::write(ancestor_dir.path().join(".gitignore"), "*\n").unwrap();
    // Child source/ subdir with real content.
    fs::create_dir_all(ancestor_dir.path().join("source")).unwrap();
    fs::write(ancestor_dir.path().join("source/important.md"), "important").unwrap();
    fs::write(ancestor_dir.path().join("source/code.rs"), "fn main() {}").unwrap();

    let source_dir = ancestor_dir.path().join("source");
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
            source_dir.to_str().unwrap(),
        ])
        .output()
        .expect("subprocess output");

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success(),
        "push should succeed — stderr was: {stderr}"
    );
    assert!(
        !stderr.contains("warning:"),
        "no skip warning expected — stderr: {stderr}"
    );

    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    rt.block_on(async {
        let requests = server.received_requests().await.unwrap();
        let put_request = requests
            .iter()
            .find(|r| r.method == reqwest::Method::PUT)
            .expect("PUT request");
        let body: serde_json::Value = serde_json::from_slice(&put_request.body).unwrap();
        let files = body["files"].as_array().unwrap();
        let paths: Vec<&str> = files.iter().map(|f| f["path"].as_str().unwrap()).collect();
        assert!(
            paths.contains(&"important.md"),
            "PUT body missing important.md — paths: {paths:?}"
        );
        assert!(
            paths.contains(&"code.rs"),
            "PUT body missing code.rs — paths: {paths:?}"
        );
    });
}
