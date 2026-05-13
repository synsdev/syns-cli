//! V044b — `syns push --strict` aborts with exit code 3 and the
//! per-category skip summary on stderr; no wire request is made.

use assert_cmd::Command as AssertCommand;
use serial_test::serial;
use std::fs;

use super::common::{SpawnOpts, spawn_mock_env};

#[test]
#[serial]
fn mixed_text_and_binary_with_strict_exits_3() {
    let env = spawn_mock_env(SpawnOpts::default());
    // No PUT mock mounted — strict guard must fire before any wire call.

    fs::write(env.project_dir.path().join("README.md"), "hello").unwrap();
    fs::write(env.project_dir.path().join("logo.png"), b"data\x00more").unwrap();

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
            "--strict",
            env.project_dir.path().to_str().unwrap(),
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
        let requests = env.server.received_requests().await.unwrap();
        assert!(
            requests.is_empty(),
            "strict guard must fire before any wire call; got {} requests",
            requests.len()
        );
    });
}

/// CODE_REVIEW H2: `--json` + `--strict` must emit the SPEC § 7
/// structured envelope on stdout AND must not leak the human-prose
/// skip summary to stderr. Pre-fix, `render_skip_summary` was called
/// unconditionally in the `cmd_push` intercept block; M3 moved
/// rendering inside `Display for CliError::PushPartial`, and
/// `Output::format_error` short-circuits to `json_value` in JSON
/// mode so the multi-line Display content is never consulted.
#[test]
#[serial]
fn strict_with_json_does_not_emit_stderr_summary() {
    let env = spawn_mock_env(SpawnOpts::default());
    // No PUT mock mounted — strict guard must fire before any wire call.

    fs::write(env.project_dir.path().join("README.md"), "hello").unwrap();
    fs::write(env.project_dir.path().join("logo.png"), b"data\x00more").unwrap();
    fs::create_dir_all(env.project_dir.path().join("dist")).unwrap();
    fs::write(env.project_dir.path().join("dist/bundle.js"), "x=1").unwrap();

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
            "--strict",
            env.project_dir.path().to_str().unwrap(),
        ])
        .output()
        .expect("subprocess output");

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(output.status.code(), Some(3), "stderr: {stderr}");
    assert!(
        !stderr.contains("warning:"),
        "JSON mode must not emit 'warning:' prose to stderr — stderr: {stderr}"
    );
    assert!(
        !stderr.contains("hint:"),
        "JSON mode must not emit 'hint:' prose to stderr — stderr: {stderr}"
    );
}

/// CODE_REVIEW H1 (partial — partner of `push_empty_test`): under
/// `--json`, the `PUSH_PARTIAL` error envelope must include a
/// structured `skipped: [{path, reason}, ...]` array per SPEC § 7.
#[test]
#[serial]
fn push_partial_json_envelope_has_structured_skipped_field() {
    let env = spawn_mock_env(SpawnOpts::default());

    fs::write(env.project_dir.path().join("README.md"), "hello").unwrap();
    fs::write(env.project_dir.path().join("logo.png"), b"data\x00more").unwrap();

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
            "--strict",
            env.project_dir.path().to_str().unwrap(),
        ])
        .output()
        .expect("subprocess output");

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(
        output.status.code(),
        Some(3),
        "exit code should be 3; stdout was: {stdout}, stderr was: {stderr}"
    );

    let trimmed = stdout.trim();
    let parsed: serde_json::Value = serde_json::from_str(trimmed).unwrap_or_else(|e| {
        panic!("stdout was not valid JSON ({e}): {stdout}");
    });
    assert_eq!(
        parsed["error"], "push_partial",
        "envelope.error mismatch: {parsed}"
    );
    let arr = parsed["skipped"]
        .as_array()
        .unwrap_or_else(|| panic!("envelope.skipped is not an array: {parsed}"));
    assert_eq!(arr.len(), 1, "expected exactly one skipped entry: {parsed}");
    assert_eq!(arr[0]["path"], "logo.png");
    assert_eq!(arr[0]["reason"], "binary");
}

/// CODE_REVIEW M5 (negative): under `--strict`, the strict hint
/// ("pass --strict to fail the push") is structurally suppressed —
/// the user already opted into strict, so the hint would be circular.
#[test]
#[serial]
fn strict_mode_omits_strict_hint() {
    let env = spawn_mock_env(SpawnOpts::default());

    fs::write(env.project_dir.path().join("README.md"), "hello").unwrap();
    fs::write(env.project_dir.path().join("logo.png"), b"data\x00more").unwrap();

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
            "--strict",
            env.project_dir.path().to_str().unwrap(),
        ])
        .output()
        .expect("subprocess output");

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(output.status.code(), Some(3), "stderr: {stderr}");
    assert!(
        !stderr.contains("pass --strict to fail the push"),
        "strict hint must NOT appear under --strict — stderr: {stderr}"
    );
    // Binary hint should still appear (the binary file justifies it).
    assert!(
        stderr.contains("add binary extensions"),
        "binary hint should still appear under --strict — stderr: {stderr}"
    );
}
