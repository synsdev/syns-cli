//! Integration tests for the u213 stderr skip-summary block.
//!
//! These tests subprocess via `assert_cmd::Command::cargo_bin("syns")`
//! so we can inspect stderr content end-to-end (the `render_skip_summary`
//! helper writes to `eprintln!` which is only observable through the
//! process boundary).

use assert_cmd::Command as AssertCommand;
use serde_json::json;
use serial_test::serial;
use std::fs;
use tempfile::TempDir;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

/// Spin up a wiremock server, seed credentials into a temp config
/// dir, return all the handles the caller needs to invoke `syns
/// push` as a subprocess.
fn spawn_mock_env(
    mount_put_success: bool,
    mock_uri_out: &mut String,
) -> (MockServer, TempDir, TempDir, TempDir) {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let (server, project_dir, config_dir, cache_dir, uri) = rt.block_on(async {
        let server = MockServer::start().await;
        let project_dir = tempfile::tempdir().unwrap();
        let config_dir = tempfile::tempdir().unwrap();
        let cache_dir = tempfile::tempdir().unwrap();

        if mount_put_success {
            Mock::given(method("PUT"))
                .and(path("/api/v1/repos/alice/repo/push"))
                .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                    "commitSha": "deadbeef00000000000000000000000000000000",
                    "version": 1,
                    "filesChanged": 1,
                    "created": true
                })))
                .mount(&server)
                .await;
            Mock::given(method("GET"))
                .and(path("/api/v1/repos/alice/repo/tree"))
                .respond_with(
                    ResponseTemplate::new(404).set_body_json(json!({"error": "not_found"})),
                )
                .mount(&server)
                .await;
        }
        let uri = server.uri();

        // Seed credentials.
        unsafe { std::env::set_var("SYNS_CONFIG_DIR", config_dir.path()) };
        unsafe { std::env::set_var("SYNS_CACHE_DIR", cache_dir.path()) };
        let config = syns_cli::config::Config::new(Some(&uri)).unwrap();
        let store = syns_cli::auth::token::TokenStore::new(config.credentials_path());
        store
            .write_with_username("test-token", Some("alice"))
            .unwrap();
        unsafe { std::env::remove_var("SYNS_CONFIG_DIR") };
        unsafe { std::env::remove_var("SYNS_CACHE_DIR") };

        (server, project_dir, config_dir, cache_dir, uri)
    });
    *mock_uri_out = uri;
    (server, project_dir, config_dir, cache_dir)
}

#[test]
#[serial]
fn mixed_text_and_binary_emits_stderr_summary_exit_0() {
    let mut mock_uri = String::new();
    let (_server, project_dir, config_dir, cache_dir) = spawn_mock_env(true, &mut mock_uri);

    fs::write(project_dir.path().join("README.md"), "hello world").unwrap();
    // First byte of a real PNG header includes a 0x0A but the third byte
    // is the null which is_binary keys on (null-byte in first 8 KB).
    fs::write(
        project_dir.path().join("logo.png"),
        b"\x89PNG\r\n\x1a\n\x00\x00\x00\rIHDR",
    )
    .unwrap();
    fs::create_dir_all(project_dir.path().join("dist")).unwrap();
    fs::write(project_dir.path().join("dist/bundle.js"), "x=1").unwrap();

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
    assert!(
        output.status.success(),
        "non-zero exit; stderr was: {stderr}"
    );
    assert!(
        stderr.contains("warning: 2 file(s) skipped from push"),
        "stderr did not contain the summary header — stderr: {stderr}"
    );
    assert!(
        stderr.contains("binary content (1): logo.png"),
        "stderr missing binary line — stderr: {stderr}"
    );
    assert!(
        stderr.contains("default-excluded directory (1): dist/bundle.js"),
        "stderr missing default-exclude line — stderr: {stderr}"
    );
    assert!(
        stderr.contains("hint:"),
        "stderr missing hint line — stderr: {stderr}"
    );
}

#[test]
#[serial]
fn default_excluded_dirs_appear_in_stderr_summary() {
    let mut mock_uri = String::new();
    let (_server, project_dir, config_dir, cache_dir) = spawn_mock_env(true, &mut mock_uri);

    fs::write(project_dir.path().join("keep.md"), "keep").unwrap();
    fs::create_dir_all(project_dir.path().join("dist")).unwrap();
    fs::write(project_dir.path().join("dist/a.js"), "a").unwrap();
    fs::create_dir_all(project_dir.path().join("build")).unwrap();
    fs::write(project_dir.path().join("build/b.js"), "b").unwrap();
    fs::create_dir_all(project_dir.path().join(".next")).unwrap();
    fs::write(project_dir.path().join(".next/c.js"), "c").unwrap();

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
    assert!(
        stderr.contains("default-excluded directory (3):"),
        "stderr missing default-exclude category — stderr: {stderr}"
    );
}

#[test]
#[serial]
fn no_default_excludes_flag_re_includes_build_dirs() {
    let mut mock_uri = String::new();
    let (server, project_dir, config_dir, cache_dir) = spawn_mock_env(true, &mut mock_uri);

    fs::write(project_dir.path().join("keep.md"), "keep").unwrap();
    fs::create_dir_all(project_dir.path().join("dist")).unwrap();
    fs::write(project_dir.path().join("dist/index.html"), "h").unwrap();

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
            "--no-default-excludes",
            project_dir.path().to_str().unwrap(),
        ])
        .output()
        .expect("subprocess output");

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(output.status.success(), "stderr was: {stderr}");
    assert!(
        !stderr.contains("warning:"),
        "stderr should not contain skip warning when no files were skipped — stderr: {stderr}"
    );

    // PUT body should include BOTH keep.md and dist/index.html.
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
        assert!(paths.contains(&"keep.md"));
        assert!(paths.contains(&"dist/index.html"));
    });
}
