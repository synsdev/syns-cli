//! V066A — empty-collection guard fires with exit 6 and a cause
//! diagnostic; `--allow-empty` bypasses the guard.

use assert_cmd::Command as AssertCommand;
use serde_json::json;
use serial_test::serial;
use std::fs;

use super::common::{SpawnOpts, spawn_mock_env};

#[test]
#[serial]
fn empty_collection_exits_6_with_cause_diagnostic() {
    let env = spawn_mock_env(SpawnOpts::default());
    // No PUT mock mounted — empty guard must fire before any wire call.

    // .gitignore: * — every file (including .syns.yaml that Phase 1
    // auto-writes) is gitignored, so files is empty.
    fs::write(env.project_dir.path().join(".gitignore"), "*\n").unwrap();

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
            env.project_dir.path().to_str().unwrap(),
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
        // SPEC u255 `smart_push` 5 moved the empty guard below the
        // reference-set build (a delete-only publication carries no
        // file and must still reach the server), so the run now reads
        // `EP-tree` first. What the guard still promises is that
        // nothing is PUBLISHED.
        let requests = env.server.received_requests().await.unwrap();
        assert!(
            requests.iter().all(|r| r.method != reqwest::Method::PUT),
            "empty guard must fire before the publication reaches the server"
        );
    });
}

#[test]
#[serial]
fn allow_empty_lets_empty_collection_through() {
    let env = spawn_mock_env(SpawnOpts {
        put_response: Some(json!({
            "commitSha": "abcd",
            "version": 1,
            "filesChanged": 0,
            "created": true,
        })),
        ..Default::default()
    });

    // .gitignore: * → collection empty; --allow-empty bypasses the guard.
    fs::write(env.project_dir.path().join(".gitignore"), "*\n").unwrap();

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
            "--allow-empty",
            env.project_dir.path().to_str().unwrap(),
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
        let requests = env.server.received_requests().await.unwrap();
        let put_requests: Vec<_> = requests
            .iter()
            .filter(|r| r.method == reqwest::Method::PUT)
            .collect();
        assert_eq!(put_requests.len(), 1, "exactly one PUT expected");
    });
}

/// CODE_REVIEW H1: `--json` mode must emit the SPEC § 7 structured
/// envelope `{"error":"push_empty","path":..,"cause":..,"totalWalked":..}`
/// to stdout (NOT stderr — `Output::error` `println!`s in JSON mode).
/// Scripted consumers can therefore `jq '.error == "push_empty"'`
/// directly instead of substring-matching a prose blob.
#[test]
#[serial]
fn push_empty_json_envelope_has_structured_fields() {
    let env = spawn_mock_env(SpawnOpts::default());
    // No PUT mock — empty guard fires first.

    fs::write(env.project_dir.path().join(".gitignore"), "*\n").unwrap();

    let output = AssertCommand::cargo_bin("syns")
        .expect("syns binary")
        .env("SYNS_CONFIG_DIR", env.config_dir.path())
        .env("SYNS_CACHE_DIR", env.cache_dir.path())
        .args([
            "--json",
            "--server",
            &env.mock_uri,
            "push",
            "--name",
            "repo",
            env.project_dir.path().to_str().unwrap(),
        ])
        .output()
        .expect("subprocess output");

    let stderr = String::from_utf8_lossy(&output.stderr);
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert_eq!(
        output.status.code(),
        Some(6),
        "exit code should be 6 (PUSH_EMPTY) — stdout was: {stdout}, stderr was: {stderr}"
    );

    // The JSON envelope is on STDOUT (Output::error in JSON mode uses
    // println! per output.rs:88-89).
    let trimmed = stdout.trim();
    let parsed: serde_json::Value = serde_json::from_str(trimmed).unwrap_or_else(|e| {
        panic!("stdout was not valid JSON ({e}): {stdout}");
    });
    assert_eq!(
        parsed["error"], "push_empty",
        "envelope.error mismatch: {parsed}"
    );
    assert!(
        parsed["path"].is_string(),
        "envelope.path is not a string: {parsed}"
    );
    assert!(
        parsed["cause"].is_string(),
        "envelope.cause is not a string: {parsed}"
    );
    assert!(
        parsed["totalWalked"].is_number(),
        "envelope.totalWalked is not a number: {parsed}"
    );
}
