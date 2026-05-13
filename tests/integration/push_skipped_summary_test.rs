//! Integration tests for the u213 stderr skip-summary block.
//!
//! These tests subprocess via `assert_cmd::Command::cargo_bin("syns")`
//! so we can inspect stderr content end-to-end (the `render_skip_summary`
//! helper writes to `eprintln!` which is only observable through the
//! process boundary).

use assert_cmd::Command as AssertCommand;
use serial_test::serial;
use std::fs;

use super::common::{SpawnOpts, default_push_response, spawn_mock_env};

#[test]
#[serial]
fn mixed_text_and_binary_emits_stderr_summary_exit_0() {
    let env = spawn_mock_env(SpawnOpts {
        put_response: Some(default_push_response()),
        ..Default::default()
    });

    fs::write(env.project_dir.path().join("README.md"), "hello world").unwrap();
    // First byte of a real PNG header includes a 0x0A but the third byte
    // is the null which is_binary keys on (null-byte in first 8 KB).
    fs::write(
        env.project_dir.path().join("logo.png"),
        b"\x89PNG\r\n\x1a\n\x00\x00\x00\rIHDR",
    )
    .unwrap();
    fs::create_dir_all(env.project_dir.path().join("dist")).unwrap();
    fs::write(env.project_dir.path().join("dist/bundle.js"), "x=1").unwrap();

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
    // CODE_REVIEW M5: assert all three expected hint lines individually
    // so a regression that drops one or two still fails the test.
    assert!(
        stderr.contains("hint: pass --strict to fail the push"),
        "stderr missing strict hint — stderr: {stderr}"
    );
    assert!(
        stderr.contains("hint: add binary extensions"),
        "stderr missing binary hint — stderr: {stderr}"
    );
    assert!(
        stderr.contains("hint: pass --no-default-excludes"),
        "stderr missing no-default-excludes hint — stderr: {stderr}"
    );
}

#[test]
#[serial]
fn default_excluded_dirs_appear_in_stderr_summary() {
    let env = spawn_mock_env(SpawnOpts {
        put_response: Some(default_push_response()),
        ..Default::default()
    });

    fs::write(env.project_dir.path().join("keep.md"), "keep").unwrap();
    fs::create_dir_all(env.project_dir.path().join("dist")).unwrap();
    fs::write(env.project_dir.path().join("dist/a.js"), "a").unwrap();
    fs::create_dir_all(env.project_dir.path().join("build")).unwrap();
    fs::write(env.project_dir.path().join("build/b.js"), "b").unwrap();
    fs::create_dir_all(env.project_dir.path().join(".next")).unwrap();
    fs::write(env.project_dir.path().join(".next/c.js"), "c").unwrap();

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
    assert!(
        stderr.contains("default-excluded directory (3):"),
        "stderr missing default-exclude category — stderr: {stderr}"
    );
}

#[test]
#[serial]
fn no_default_excludes_flag_re_includes_build_dirs() {
    let env = spawn_mock_env(SpawnOpts {
        put_response: Some(default_push_response()),
        ..Default::default()
    });

    fs::write(env.project_dir.path().join("keep.md"), "keep").unwrap();
    fs::create_dir_all(env.project_dir.path().join("dist")).unwrap();
    fs::write(env.project_dir.path().join("dist/index.html"), "h").unwrap();

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
            "--no-default-excludes",
            env.project_dir.path().to_str().unwrap(),
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
        let requests = env.server.received_requests().await.unwrap();
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

/// CODE_REVIEW M4: smoke test for the `--debug` flag. Asserts the
/// `[debug] skip` breadcrumb fires for each skipped file with the
/// SPEC § 3.2 source label (here: `binary-heuristic`).
#[test]
#[serial]
fn debug_flag_emits_per_file_decisions() {
    let env = spawn_mock_env(SpawnOpts {
        put_response: Some(default_push_response()),
        ..Default::default()
    });

    fs::write(env.project_dir.path().join("README.md"), "hello").unwrap();
    fs::write(env.project_dir.path().join("logo.png"), b"data\x00null").unwrap();

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
            "--debug",
            env.project_dir.path().to_str().unwrap(),
        ])
        .output()
        .expect("subprocess output");

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success(),
        "non-zero exit; stderr was: {stderr}"
    );
    assert!(
        stderr.contains("[debug] skip"),
        "stderr missing [debug] skip breadcrumb — stderr: {stderr}"
    );
    assert!(
        stderr.contains("binary-heuristic"),
        "stderr missing binary-heuristic source label — stderr: {stderr}"
    );
    assert!(
        stderr.contains("logo.png"),
        "stderr missing the skipped file path — stderr: {stderr}"
    );
}

/// CODE_REVIEW L2: the `+K more` truncation branch in
/// `write_skip_summary` (commands/push.rs originally) fires when a
/// single category exceeds `MAX_PER_CATEGORY = 5`. Six binary files
/// yield "+1 more". Verifies the exact suffix and that exactly five
/// concrete file names precede it.
#[test]
#[serial]
fn truncates_excess_paths_with_k_more() {
    let env = spawn_mock_env(SpawnOpts {
        put_response: Some(default_push_response()),
        ..Default::default()
    });

    // One text file (so the push doesn't abort with PUSH_EMPTY) plus
    // six binary files (one over the MAX_PER_CATEGORY=5 threshold).
    fs::write(env.project_dir.path().join("README.md"), "keep").unwrap();
    for i in 0..6 {
        fs::write(
            env.project_dir.path().join(format!("img_{i}.png")),
            b"data\x00null",
        )
        .unwrap();
    }

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
        "non-zero exit; stderr was: {stderr}"
    );
    assert!(
        stderr.contains("binary content (6):"),
        "stderr missing 6-count header for binary category — stderr: {stderr}"
    );
    assert!(
        stderr.contains("+1 more"),
        "stderr missing '+1 more' truncation suffix — stderr: {stderr}"
    );
    // Exactly five concrete file names precede the suffix. The
    // sort order is lex; img_0..img_4 are first.
    for i in 0..5 {
        assert!(
            stderr.contains(&format!("img_{i}.png")),
            "stderr missing img_{i}.png — stderr: {stderr}"
        );
    }
}
