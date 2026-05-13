//! Cross-cutting integration tests for the `--if-repo` flag.
//!
//! These tests are not bound to any single command — they verify
//! invariants that span the whole CLI surface:
//!
//! - Clap REJECTS `--if-repo` on the seven non-resolver commands
//!   (login, logout, whoami, explore, fork, teams, upgrade). The
//!   flag is declared on resolver-using variants ONLY, and a future
//!   contributor who accidentally adds it to a non-resolver variant
//!   (or marks the flag `global = true`) breaks the SPEC § 2 / § 9 / R4
//!   invariant. Asserted via subprocess (the `Cli` struct lives in
//!   `main.rs`, not the lib; the binary's clap-derived parser fires
//!   `ErrorKind::UnknownArgument` which prints `"unexpected argument"`
//!   to stderr with exit code 2).
//!
//! - `syns --json pull --if-repo` from an empty directory emits the
//!   exact wire-shape `{"skipped":true,"reason":"no_syns_repo"}\n`
//!   on stdout with exit 0. Because skip emission is centralised in
//!   `resolve_*_or_skip`, ONE end-to-end stdout-capture test covers
//!   the entire 13-command resolver-using surface.
//!
//! Both tests subprocess via `assert_cmd::Command::cargo_bin("syns")`
//! — the precedent already used in `upgrade_test.rs` for binary-level
//! integration coverage.

use assert_cmd::Command as AssertCommand;
use serial_test::serial;

/// The seven non-resolver commands. They MUST reject `--if-repo`
/// because the flag's semantics are tied to the resolver-miss case
/// — which these commands do not have.
const NON_RESOLVER_COMMANDS: &[&str] = &[
    "login", "logout", "whoami", "explore", "fork", "teams", "upgrade",
];

#[test]
#[serial]
fn clap_rejects_if_repo_on_every_non_resolver_command() {
    for cmd in NON_RESOLVER_COMMANDS {
        let assert = AssertCommand::cargo_bin("syns")
            .expect("syns binary")
            .args([cmd, "--if-repo"])
            .assert()
            .failure();

        let output = assert.get_output();
        let stderr = String::from_utf8_lossy(&output.stderr);

        // Clap's `ErrorKind::UnknownArgument` prints `"unexpected argument"`
        // and names the offending flag. Asserting both pins the error class.
        assert!(
            stderr.contains("unexpected argument"),
            "command `syns {cmd} --if-repo` did NOT reject the flag — \
             stderr was: {stderr}"
        );
        assert!(
            stderr.contains("--if-repo"),
            "command `syns {cmd} --if-repo` rejected something, but the \
             error message did not name `--if-repo` — stderr was: {stderr}"
        );
    }
}

#[test]
#[serial]
fn syns_json_pull_if_repo_emits_exact_skip_envelope_on_stdout() {
    let temp_dir = tempfile::tempdir().expect("tempdir");

    let assert = AssertCommand::cargo_bin("syns")
        .expect("syns binary")
        .current_dir(temp_dir.path())
        .env("SYNS_CONFIG_DIR", temp_dir.path())
        .args(["--json", "pull", "--if-repo"])
        .assert()
        .success();

    let output = assert.get_output();
    let stdout = String::from_utf8(output.stdout.clone()).expect("utf8 stdout");

    // Exact 40-character literal + trailing newline (from `println!`).
    // Total = 41 bytes. The contract is locked at compile time by the
    // raw-string literal in `Output::format_skip`; this assertion locks
    // it at run time through the FULL pipeline:
    //   resolver miss → resolve_or_skip → output.skip() → format_skip
    //   → println!  → child stdout
    // A regression anywhere on that chain (silent swallow, double emit,
    // stray whitespace, accidental serde_json::to_string rewrite) fails
    // this assertion.
    assert_eq!(stdout, "{\"skipped\":true,\"reason\":\"no_syns_repo\"}\n");
}
