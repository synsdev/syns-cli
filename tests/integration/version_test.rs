//! u253 — global `--version` / `-V` flag prints `syns <semver>` and exits 0,
//! without disturbing `syns pull --version REF` (root-local, not propagated)
//! or clap's `--help` action. (issue 116; CLI_IA.md § Global Flags line 76.)

use assert_cmd::Command as AssertCommand;
use serial_test::serial;

use super::common::{SpawnOpts, spawn_mock_env};

#[test]
#[serial]
fn version_long_flag_prints_version_and_exits_zero() {
    let output = AssertCommand::cargo_bin("syns")
        .expect("syns binary")
        .arg("--version")
        .output()
        .expect("subprocess output");

    assert_eq!(output.status.code(), Some(0));
    assert_eq!(
        String::from_utf8_lossy(&output.stdout).trim(),
        format!("syns {}", env!("CARGO_PKG_VERSION"))
    );
}

#[test]
#[serial]
fn version_short_flag_prints_version_and_exits_zero() {
    let output = AssertCommand::cargo_bin("syns")
        .expect("syns binary")
        .arg("-V")
        .output()
        .expect("subprocess output");

    assert_eq!(output.status.code(), Some(0));
    assert_eq!(
        String::from_utf8_lossy(&output.stdout).trim(),
        format!("syns {}", env!("CARGO_PKG_VERSION"))
    );
}

#[test]
#[serial]
fn pull_version_ref_flag_is_preserved_not_global() {
    // Regression guard: `--version 25` must bind to pull's REF argument,
    // NOT trigger the root version action. spawn_mock_env mounts a default
    // 404 tree mock and seeds credentials (owner=alice, repo=repo).
    let env = spawn_mock_env(SpawnOpts::default());

    let output = AssertCommand::cargo_bin("syns")
        .expect("syns binary")
        .env("SYNS_CONFIG_DIR", env.config_dir.path())
        .env("SYNS_CACHE_DIR", env.cache_dir.path())
        .args([
            "--server",
            &env.mock_uri,
            "pull",
            "--version",
            "25",
            "alice/repo",
            env.project_dir.path().to_str().unwrap(),
        ])
        .output()
        .expect("subprocess output");

    // Load-bearing assertion: the global version banner is ABSENT, proving
    // `--version 25` bound to pull's REF arg rather than the root action.
    // (Banner-absence is the reliable discriminator regardless of mock outcome;
    // an exit-code-only check would pass spuriously against a succeeding mock.)
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        !stdout.contains(&format!("syns {}", env!("CARGO_PKG_VERSION"))),
        "global version banner must be absent — stdout: {stdout}"
    );

    // Positively confirm clap entered the `pull` subcommand and reached the
    // wire layer (instead of short-circuiting on the version action).
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    rt.block_on(async {
        let requests = env.server.received_requests().await.unwrap();
        assert!(
            !requests.is_empty(),
            "pull must reach the wire layer; got 0 requests"
        );
    });
}

#[test]
#[serial]
fn help_flag_still_exits_zero() {
    let output = AssertCommand::cargo_bin("syns")
        .expect("syns binary")
        .arg("--help")
        .output()
        .expect("subprocess output");

    assert_eq!(output.status.code(), Some(0));
    assert!(
        String::from_utf8_lossy(&output.stdout).contains("--version"),
        "rendered help should list the new --version flag"
    );
}
