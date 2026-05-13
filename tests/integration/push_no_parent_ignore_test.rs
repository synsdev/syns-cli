//! V066B / T1 — the dotfiles-bare-repo reproduction. A hostile
//! ancestor `.gitignore: *` must NOT empty a push from a subdirectory.

use assert_cmd::Command as AssertCommand;
use serde_json::json;
use serial_test::serial;
use std::fs;

use super::common::{SpawnOpts, spawn_mock_env};

#[test]
#[serial]
fn parent_gitignore_star_does_not_empty_push() {
    let env = spawn_mock_env(SpawnOpts {
        put_response: Some(json!({
            "commitSha": "abcd",
            "version": 1,
            "filesChanged": 2,
            "created": true,
        })),
        ..Default::default()
    });

    // Treat env.project_dir as the *ancestor* dir; push from
    // {ancestor}/source/.
    let ancestor_dir = env.project_dir.path();
    fs::write(ancestor_dir.join(".gitignore"), "*\n").unwrap();
    fs::create_dir_all(ancestor_dir.join("source")).unwrap();
    fs::write(ancestor_dir.join("source/important.md"), "important").unwrap();
    fs::write(ancestor_dir.join("source/code.rs"), "fn main() {}").unwrap();

    let source_dir = ancestor_dir.join("source");
    let output = AssertCommand::cargo_bin("syns")
        .expect("syns binary")
        .env("SYNS_CONFIG_DIR", env.config_dir.path())
        .env("SYNS_CACHE_DIR", env.cache_dir.path())
        .args([
            "--server",
            &env.mock_uri,
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
        let requests = env.server.received_requests().await.unwrap();
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
