//! V070a / legacy-stub self-heal — server responses with an empty
//! `commitSha` must not persist a manifest, and a pre-existing 29-byte
//! stub must be silently overwritten on the next real push.

use assert_cmd::Command as AssertCommand;
use serde_json::json;
use serial_test::serial;
use std::fs;

use super::common::{SpawnOpts, spawn_mock_env};

#[test]
#[serial]
fn response_with_empty_commit_sha_does_not_write_manifest() {
    let env = spawn_mock_env(SpawnOpts {
        // Empty-commit-sha response (server accepted the push but
        // emitted no commit — issue 070's symptom). `created: true`
        // routes `format_response` through the success branch, so
        // the first-push-noop banner is NOT under test here — see
        // `first_push_noop_emits_suspicious_warning_banner` below.
        put_response: Some(json!({
            "commitSha": "",
            "version": 0,
            "filesChanged": 0,
            "created": true,
        })),
        ..Default::default()
    });

    fs::write(env.project_dir.path().join("text.txt"), "hello").unwrap();

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
    assert!(output.status.success(), "stderr was: {stderr}");

    // Cache file must NOT have been written.
    let manifest_path = env.cache_dir.path().join("alice").join("repo.json");
    assert!(
        !manifest_path.exists(),
        "manifest must NOT be persisted when commit_sha is empty"
    );

    // Empty-commit-sha warning is the primary V070a stderr contract.
    assert!(
        stderr.contains("empty commit_sha"),
        "stderr missing 'empty commit_sha' warning — stderr: {stderr}"
    );
}

#[test]
#[serial]
fn legacy_stub_is_overwritten_on_next_real_push() {
    let env = spawn_mock_env(SpawnOpts {
        put_response: Some(json!({
            "commitSha": "realsha0000000000000000000000000000000000",
            "version": 1,
            "filesChanged": 1,
            "created": true,
        })),
        ..Default::default()
    });

    fs::write(env.project_dir.path().join("text.txt"), "hello").unwrap();

    // Pre-seed the legacy stub.
    let manifest_path = env.cache_dir.path().join("alice").join("repo.json");
    fs::create_dir_all(manifest_path.parent().unwrap()).unwrap();
    fs::write(&manifest_path, r#"{"commit_sha":"","files":{}}"#).unwrap();

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

/// CODE_REVIEW H3: the SPEC § 8.5 first-push-noop branch fires when
/// the server response is `created: false AND files_changed == 0`
/// AND no prior manifest exists. This is the V070-defense signal —
/// the user sees a banner pointing at the server-accepted-empty
/// scenario. The deviations in IMPLEMENTATION.md §5 (and again in
/// CODE_REVIEW issue #3) leave this branch entirely uncovered: the
/// only existing test for V070 uses `created: true` (success branch);
/// this test exercises the actual first-push-noop wire shape.
#[test]
#[serial]
fn first_push_noop_emits_suspicious_warning_banner() {
    let env = spawn_mock_env(SpawnOpts {
        // CRITICAL: created=false AND files_changed=0 routes
        // format_response through the first-push-noop branch (not
        // the success branch). The cache_dir is fresh (no prior
        // manifest), so `meta.manifest_existed == false`.
        put_response: Some(json!({
            "commitSha": "",
            "version": 0,
            "filesChanged": 0,
            "created": false,
        })),
        ..Default::default()
    });

    fs::write(env.project_dir.path().join("text.txt"), "hello").unwrap();

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
    assert!(
        output.status.success(),
        "subprocess should exit 0 (the banner is a warning, not an error); stderr: {stderr}"
    );
    // SPEC § 8.5: the literal banner text marking the V070 defect
    // signal. Asserted verbatim so a future refactor that swaps
    // `eprintln!` for an `Output::warn` call (or otherwise changes
    // the phrasing) trips this regression.
    assert!(
        stderr.contains(
            "Push response indicates no changes were committed and no prior manifest existed"
        ),
        "stderr missing first-push-noop banner — stderr: {stderr}"
    );
}
