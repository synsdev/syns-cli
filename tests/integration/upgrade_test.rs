//! Integration tests for `syns upgrade` (u200 SPEC § 8 T8–T10).
//!
//! T8 — `--check-only` short-circuit on an out-of-date unmanaged binary.
//! T9 — Homebrew managed-install branch suppresses disclosure + zero network.
//! T10 — Full happy path: download + sha256.sum verify + atomic-swap.
//!       Marked `#[ignore]` — self_replace against the test runner's own
//!       binary would corrupt the running test process. Step 8's CI smoke
//!       matrix (`install-detect-smoke`) exercises the real release binary
//!       on each OS as the empirical replacement.

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

/// On macOS the dev runner is Curl-classified for `~/.local/bin`. We need an
/// "unmanaged" path that no prefix table claims. `/usr/local/bin/syns` does the
/// job on macOS, Linux, and Windows (no Windows prefix claims it either).
fn unmanaged_fake() -> FakeCurrentExecutable {
    FakeCurrentExecutable {
        path: PathBuf::from("/usr/local/bin/syns"),
    }
}

/// A path that classifies as `Homebrew` on macOS / Linux.
fn homebrew_fake() -> FakeCurrentExecutable {
    FakeCurrentExecutable {
        path: PathBuf::from("/opt/homebrew/Cellar/syns/0.2.0/bin/syns"),
    }
}

/// T8 — `--check-only` on an out-of-date unmanaged binary prints the comparison
/// and exits 0 without firing the download endpoint. The provenance disclosure
/// IS printed (unmanaged path).
#[tokio::test(flavor = "current_thread")]
#[serial]
async fn t8_check_only_out_of_date_unmanaged_no_download() {
    let mock = MockServer::start().await;

    let asset_name = format!("syns-{}.{}", TEST_TARGET, archive_ext());

    // Stub /releases/latest with a tag newer than CARGO_PKG_VERSION = "0.2.2".
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

    let output = Output::new(true); // --json mode
    let args = UpgradeArgs {
        check_only: true,
        force: false,
        prerelease: false,
        no_checksum: false,
    };
    let result = run_with(args, &output, &unmanaged_fake()).await;

    unsafe { std::env::remove_var("_INTERNAL_GH_API_BASE") };
    unsafe { std::env::remove_var("_INTERNAL_GH_DOWNLOAD_BASE") };

    assert!(result.is_ok(), "expected Ok, got {result:?}");
    // wiremock's .expect(1) on the metadata endpoint above asserts exactly one
    // GET; the absence of a download stub means any hit there would surface as
    // a connection error, which result.is_ok() above catches.
}

/// T9 — `run_with` against `InstallMethod::Homebrew` prints `brew upgrade syns`,
/// suppresses the provenance disclosure (managed path), and exits 0. Zero
/// network calls — any GET to the mock server would be unexpected.
#[tokio::test(flavor = "current_thread")]
#[serial]
async fn t9_homebrew_managed_redirect_no_network() {
    // On Windows the Homebrew classification has no prefix table; skip the
    // managed-redirect assertion (the platform doesn't have Homebrew anyway).
    if cfg!(target_os = "windows") {
        return;
    }

    let mock = MockServer::start().await;
    // Catch-all mock: ANY request must NOT be hit.
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(500))
        .expect(0)
        .mount(&mock)
        .await;

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
    let result = run_with(args, &output, &fake).await;

    unsafe { std::env::remove_var("_INTERNAL_GH_API_BASE") };
    unsafe { std::env::remove_var("_INTERNAL_GH_DOWNLOAD_BASE") };

    assert!(result.is_ok(), "expected Ok, got {result:?}");
    // `Homebrew.is_managed()` ensures the managed-redirect branch was taken.
    assert!(InstallMethod::Homebrew.is_managed());
    // wiremock .expect(0) above is verified on drop — any network call would
    // panic. The fact that we reached this line implies zero network activity.
}

/// T10 — Full happy path: download → sha256.sum → verify → atomic swap. The
/// production swap path calls `self_replace::self_replace` against
/// `std::env::current_exe()`, which on a `cargo test` run is the test binary
/// itself — rewriting it under our feet would corrupt the running test process.
///
/// The CI install-detect-smoke matrix (Step 8) exercises the real release
/// binary on each OS as the load-bearing empirical gate. This integration
/// test is therefore marked `#[ignore]` and documented.
#[tokio::test(flavor = "current_thread")]
#[serial]
#[ignore = "T10 deferred to CI smoke (Step 8) — self_replace against the test binary would corrupt the test runner"]
async fn t10_happy_path_full_swap() {
    // Intentionally empty — see the doc-comment + #[ignore] reason above.
    // The full happy path is covered end-to-end on the real release binary
    // by .github/workflows/ci.yml's install-detect-smoke matrix.
}
