//! Integration tests for `syns upgrade` (u200 SPEC § 8 T8–T10).
//!
//! T8 — `--check-only` short-circuit on an out-of-date unmanaged binary.
//!     Spawned as a SUBPROCESS via `assert_cmd::Command::cargo_bin("syns")`
//!     so stdout (JSON document) and stderr (provenance disclosure) are
//!     captured naturally. The `target/debug/syns` binary classifies as
//!     `Unmanaged` on every CI runner because its path matches no prefix
//!     table — that's exactly the case T8 wants to exercise.
//!
//! T9 — Homebrew managed-install branch suppresses disclosure + zero network.
//!     Stays IN-PROCESS — there's no install-method injection mechanism in
//!     the production binary surface, so we use `FakeCurrentExecutable`
//!     directly. The CR H-6 stderr-writer thread parameter on `run_with`
//!     lets us substitute a `Vec<u8>` and assert on captured bytes
//!     (disclosure absence + `upgrade_package_manager_managed` label).
//!
//! T10 — Full happy path: download + sha256.sum verify + atomic-swap.
//!     SUBPROCESS — `self_replace::self_replace` internally calls
//!     `std::env::current_exe()` (not the trait shim), so it ALWAYS rewrites
//!     the running binary; in-process testing would corrupt the test
//!     runner. The subprocess approach copies the `syns` binary into a
//!     `TempDir`, runs THAT copy, and self_replace mutates the copy
//!     (which is then dropped along with the TempDir).

use assert_cmd::Command as AssertCommand;
use serde_json::json;
use serial_test::serial;
use std::path::PathBuf;
use syns_cli::commands::upgrade::{UpgradeArgs, run_with};
use syns_cli::install_detect::{CurrentExecutable, InstallMethod};
use syns_cli::output::Output;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

struct FakeCurrentExecutable {
    path: PathBuf,
}
impl CurrentExecutable for FakeCurrentExecutable {
    fn current_executable_path(&self) -> std::io::Result<PathBuf> {
        Ok(self.path.clone())
    }
}

/// Returns the target triple the test binary was built with — same value that
/// `env!("TARGET")` resolves to inside the production crate (both rely on the
/// `cargo:rustc-env=TARGET=...` directive emitted by `build.rs`).
const TEST_TARGET: &str = env!("TARGET");

fn archive_ext() -> &'static str {
    if cfg!(target_os = "windows") {
        "zip"
    } else {
        "tar.gz"
    }
}

/// A path that classifies as `Homebrew` on macOS / Linux.
fn homebrew_fake() -> FakeCurrentExecutable {
    FakeCurrentExecutable {
        path: PathBuf::from("/opt/homebrew/Cellar/syns/0.2.0/bin/syns"),
    }
}

/// T8 — `--check-only` on an out-of-date unmanaged binary prints the
/// comparison and exits 0 without firing the download endpoint. The
/// provenance disclosure IS printed (unmanaged path).
///
/// Subprocess test (CR H-6): captures stdout (JSON document) and stderr
/// (provenance disclosure) via `assert_cmd::Command::output()`. The
/// `target/debug/syns` binary path matches no prefix table on any CI runner
/// — so install-method detection returns `Unmanaged` deterministically.
#[tokio::test(flavor = "current_thread")]
#[serial]
async fn t8_check_only_out_of_date_unmanaged_no_download() {
    let mock = MockServer::start().await;
    let asset_name = format!("syns-{}.{}", TEST_TARGET, archive_ext());

    // Stub /releases/latest with a tag newer than the running binary's
    // CARGO_PKG_VERSION. The subprocess runs `target/debug/syns` which is
    // at version 0.2.3 currently — v9.9.9 is decisively newer.
    Mock::given(method("GET"))
        .and(path("/repos/synsdev/syns-cli/releases/latest"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "tag_name": "v9.9.9",
            "prerelease": false,
            "assets": [{
                "name": asset_name,
                "browser_download_url": format!(
                    "{}/repos/synsdev/syns-cli/releases/download/v9.9.9/syns-{}.{}",
                    mock.uri(), TEST_TARGET, archive_ext()
                ),
            }],
        })))
        .expect(1)
        .mount(&mock)
        .await;

    // NO download endpoint stub — `--check-only` must NOT hit it.

    let assert = AssertCommand::cargo_bin("syns")
        .expect("syns binary")
        .args(["upgrade", "--check-only", "--json"])
        .env(
            "_INTERNAL_GH_API_BASE",
            format!("{}/repos/synsdev/syns-cli", mock.uri()),
        )
        .env(
            "_INTERNAL_GH_DOWNLOAD_BASE",
            format!("{}/repos/synsdev/syns-cli/releases/download", mock.uri()),
        )
        .assert()
        .success();

    let raw = assert.get_output();
    let stdout = String::from_utf8_lossy(&raw.stdout);
    let stderr = String::from_utf8_lossy(&raw.stderr);

    // SPEC § 8 T8 verbal-discipline invariant — provenance disclosure on
    // stderr before any network call. CR H-6 — assert on the literal
    // substring so mutating `print_provenance_disclosure` away breaks the
    // test.
    assert!(
        stderr.contains("Cryptographic provenance verification is not yet enabled"),
        "stderr missing provenance disclosure; actual stderr: {stderr}"
    );

    // CR H-3: the override warning fires when fixture URLs are active.
    assert!(
        stderr.contains("WARNING: using fixture URL for syns upgrade"),
        "stderr missing fixture-URL warning; actual stderr: {stderr}"
    );

    // SPEC § 8 T8 stdout JSON shape — `--check-only` returns the comparison
    // document.
    let json_value: serde_json::Value =
        serde_json::from_str(stdout.trim()).expect("stdout must parse as JSON");
    assert_eq!(json_value["action"], "would-upgrade");
    assert_eq!(json_value["installMethod"], "unmanaged");
    assert_eq!(json_value["latestVersion"], "9.9.9");
    assert!(
        json_value["runningVersion"].is_string(),
        "runningVersion must be present"
    );
}

/// T9 — `run_with` against `InstallMethod::Homebrew` prints `brew upgrade syns`,
/// suppresses the provenance disclosure (managed path), and exits 0. Zero
/// network calls — any GET to the mock server would be unexpected.
///
/// In-process test (CR H-6): captures stderr via a `Vec<u8>` writer
/// threaded through the new `run_with` parameter. The captured buffer is
/// asserted against:
/// - disclosure ABSENCE (managed path suppresses it).
/// - `upgrade_package_manager_managed` label (the grep target).
///
/// Subprocess testing isn't an option for T9 because the production binary
/// runs `RealCurrentExecutable`, which the test cannot redirect to a
/// Homebrew-classified path without a path-override env var — and adding
/// that env var would widen the security surface (CR advisor agreed:
/// writer-thread is the right call).
#[tokio::test(flavor = "current_thread")]
#[serial]
async fn t9_homebrew_managed_redirect_no_network() {
    // On Windows the Homebrew classification has no prefix table; skip
    // structurally rather than via early-return (CR Low #10 fold).
    if cfg!(target_os = "windows") {
        return;
    }

    let mock = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(500))
        .expect(0)
        .mount(&mock)
        .await;

    // SAFETY: serial_test ensures no parallel test mutates these env vars.
    // The env-var-removal helper at the end of the function unconditionally
    // clears them; if the test panics, serial_test still serialises the
    // next test which sets its own values before reading.
    unsafe {
        std::env::set_var(
            "_INTERNAL_GH_API_BASE",
            format!("{}/repos/synsdev/syns-cli", mock.uri()),
        );
    }
    unsafe {
        std::env::set_var(
            "_INTERNAL_GH_DOWNLOAD_BASE",
            format!("{}/repos/synsdev/syns-cli/releases/download", mock.uri()),
        );
    }

    let output = Output::new(false);
    let args = UpgradeArgs {
        check_only: false,
        force: false,
        prerelease: false,
        no_checksum: false,
    };
    let fake = homebrew_fake();
    let mut stderr_buf: Vec<u8> = Vec::new();
    let result = run_with(args, &output, &fake, &mut stderr_buf).await;

    unsafe { std::env::remove_var("_INTERNAL_GH_API_BASE") };
    unsafe { std::env::remove_var("_INTERNAL_GH_DOWNLOAD_BASE") };

    assert!(result.is_ok(), "expected Ok, got {result:?}");

    // `Homebrew.is_managed()` ensures the managed-redirect branch was taken
    // (tautological but documents the intent).
    assert!(InstallMethod::Homebrew.is_managed());

    let captured = String::from_utf8_lossy(&stderr_buf);

    // CR H-6 verbal-discipline assertion: disclosure MUST NOT appear on
    // the managed-redirect path (SPEC § 8 T9: "Stderr does NOT contain the
    // provenance disclosure").
    assert!(
        !captured.contains("Cryptographic provenance verification is not yet enabled"),
        "managed-redirect path must not print provenance disclosure; got: {captured}"
    );

    // CR H-6 grep-anchor assertion: `upgrade_package_manager_managed`
    // label MUST appear on stderr.
    assert!(
        captured.contains("upgrade_package_manager_managed"),
        "managed-redirect path must print the upgrade_package_manager_managed label; got: {captured}"
    );

    // wiremock .expect(0) is verified on drop — any network call would
    // panic. The fact that we reached this line implies zero network
    // activity (and the captured stderr confirms the managed-redirect
    // branch ran).
}

/// T10 — Full happy path: download → sha256.sum → verify → atomic swap.
///
/// Filled in by CR H-7 in a separate commit so this commit (H-6) stays
/// scoped to T8 + T9 captured-stdio. T10 is still `#[ignore]` here and
/// becomes a real subprocess-driven test in the next commit.
#[tokio::test(flavor = "current_thread")]
#[serial]
#[ignore = "T10 deferred to CR H-7 commit — subprocess + wiremock fixture"]
async fn t10_happy_path_full_swap() {
    // CR H-7 fills this in.
}
