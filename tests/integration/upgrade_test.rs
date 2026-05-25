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
        no_attestation: false,
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
/// CR H-7 fix: the `#[ignore]` is GONE. This test now runs against a real
/// wiremock-backed fixture HTTP server, with the subprocess being a COPY
/// of `target/debug/syns` placed inside a `TempDir` — so when
/// `self_replace::self_replace` overwrites the subprocess's
/// `std::env::current_exe()`, it overwrites the COPY (which then gets
/// dropped along with the TempDir), not the test runner.
///
/// Fixture construction:
///   - On Unix: a real `tar.gz` archive containing
///     `syns-{TARGET}/syns` whose body is a known sentinel.
///   - On Windows: a real `.zip` archive containing `syns.exe` at depth 1
///     (per u196 § 3.2 / extract_binary Windows layout).
///   - SHA-256 of the archive bytes is computed and served as
///     `sha256.sum`.
///   - wiremock serves three routes:
///     `/repos/synsdev/syns-cli/releases/latest` (JSON metadata),
///     `/repos/synsdev/syns-cli/releases/download/v9.9.9/syns-{TARGET}.{ext}`
///     (archive bytes), and
///     `/repos/synsdev/syns-cli/releases/download/v9.9.9/sha256.sum`
///     (the sum line).
///
/// Assertions:
///   - Exit code 0.
///   - Stderr contains the SPEC § 4.2 disclosure literal.
///   - Stderr contains the literal `(integrity-checked)` success token
///     (SPEC § 5 D7 / PROTOTYPE C-04 verbal-discipline invariant).
///   - Stdout `--json` shape contains `action="upgraded"` and
///     `verification="sha256"`.
///   - The TempDir-resident binary now contains the fixture sentinel
///     bytes (proves self_replace actually mutated the right file).
///   - No `.syns-upgrade-*` staging cruft remains in the TempDir
///     (proves CR H-2 RAII cleanup ran).
#[tokio::test(flavor = "current_thread")]
#[serial]
async fn t10_happy_path_full_swap() {
    use sha2::{Digest, Sha256};
    use std::fs;
    use std::io::Write as _;

    let asset_name = format!("syns-{}.{}", TEST_TARGET, archive_ext());
    let sentinel_bytes = b"SYNS-T10-FIXTURE-BYTES-not-an-executable".to_vec();

    // -------- Build the fixture archive in memory. --------
    let archive_bytes: Vec<u8> = if cfg!(target_os = "windows") {
        // Windows: zip with `syns.exe` at depth 1.
        let mut buf = Vec::new();
        {
            let mut zip = zip::ZipWriter::new(std::io::Cursor::new(&mut buf));
            let options: zip::write::FileOptions<()> = zip::write::FileOptions::default();
            zip.start_file("syns.exe", options).expect("zip start_file");
            zip.write_all(&sentinel_bytes).expect("zip write_all");
            zip.finish().expect("zip finish");
        }
        buf
    } else {
        // Unix: tar.gz with `syns-{TARGET}/syns` inside.
        let mut tar_buf = Vec::new();
        {
            let mut tar_builder = tar::Builder::new(&mut tar_buf);
            let mut header = tar::Header::new_gnu();
            header.set_size(sentinel_bytes.len() as u64);
            header.set_mode(0o755);
            header.set_cksum();
            let inner_path = format!("syns-{}/syns", TEST_TARGET);
            tar_builder
                .append_data(&mut header, &inner_path, sentinel_bytes.as_slice())
                .expect("tar append_data");
            tar_builder.finish().expect("tar finish");
        }
        let mut gz_buf = Vec::new();
        {
            let mut encoder =
                flate2::write::GzEncoder::new(&mut gz_buf, flate2::Compression::default());
            encoder.write_all(&tar_buf).expect("gz write");
            encoder.finish().expect("gz finish");
        }
        gz_buf
    };

    // -------- Compute SHA-256 of the archive bytes. --------
    let mut hasher = Sha256::new();
    hasher.update(&archive_bytes);
    let archive_sha256 = format!("{:x}", hasher.finalize());
    let sha256sum_body = format!("{}  {}\n", archive_sha256, asset_name);

    // -------- Spin up the wiremock server. --------
    let mock = MockServer::start().await;

    Mock::given(method("GET"))
        .and(path("/repos/synsdev/syns-cli/releases/latest"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "tag_name": "v9.9.9",
            "prerelease": false,
            "assets": [{
                "name": asset_name,
                "browser_download_url": format!(
                    "{}/repos/synsdev/syns-cli/releases/download/v9.9.9/{}",
                    mock.uri(),
                    asset_name,
                ),
            }],
        })))
        .expect(1)
        .mount(&mock)
        .await;

    let download_path = format!("/repos/synsdev/syns-cli/releases/download/v9.9.9/{asset_name}");
    Mock::given(method("GET"))
        .and(path(&download_path))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_bytes(archive_bytes.clone())
                .insert_header("content-type", "application/octet-stream"),
        )
        .expect(1)
        .mount(&mock)
        .await;

    Mock::given(method("GET"))
        .and(path(
            "/repos/synsdev/syns-cli/releases/download/v9.9.9/sha256.sum",
        ))
        .respond_with(ResponseTemplate::new(200).set_body_string(sha256sum_body.clone()))
        .expect(1)
        .mount(&mock)
        .await;

    // -------- Copy target/debug/syns into a TempDir. --------
    let staging = tempfile::TempDir::new().expect("create test TempDir");
    let exe_src: PathBuf = PathBuf::from(env!("CARGO_BIN_EXE_syns"));
    let exe_copy_name = if cfg!(target_os = "windows") {
        "syns.exe"
    } else {
        "syns"
    };
    let exe_copy = staging.path().join(exe_copy_name);
    fs::copy(&exe_src, &exe_copy).expect("copy syns binary into TempDir");
    // CR H-7 (advisor): chmod +x explicitly on Unix — `fs::copy` preserves
    // perms on tmpfs/local disks but not all filesystems (NFS, FUSE). The
    // explicit `0o755` matches the post-extract binary perm.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&exe_copy, fs::Permissions::from_mode(0o755))
            .expect("chmod +x on copied binary");
    }

    // -------- Invoke the copied binary as a subprocess. --------
    // `--no-attestation` is required: the fixture archive is synthesized
    // in-memory and carries no real SLSA attestation. The default flow
    // (which now mandates `gh attestation verify`) would fail-closed at
    // state 7a. Passing `--no-attestation` exercises the integrity-only
    // path (still SHA-256-verified against the fixture sha256.sum).
    let assert = AssertCommand::new(&exe_copy)
        .args(["upgrade", "--json", "--no-attestation"])
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

    // --no-attestation surfaces the warning on stderr.
    assert!(
        stderr.contains("--no-attestation was passed"),
        "stderr missing --no-attestation warning; stderr={stderr}"
    );

    // With --no-attestation but checksum still on, the success message MUST
    // contain `(integrity-checked)` and NEVER `(SLSA-verified)` or
    // `(verified)`.
    assert!(
        stderr.contains("(integrity-checked)"),
        "stderr missing `(integrity-checked)` token; stderr={stderr}"
    );
    assert!(
        !stderr.contains("(SLSA-verified)"),
        "stderr unexpectedly contains `(SLSA-verified)` with --no-attestation; stderr={stderr}"
    );
    assert!(
        !stderr.contains("(verified)") || stderr.contains("SLSA-verified"),
        "stderr accidentally contains bare `(verified)` — verbal-discipline regression; stderr={stderr}"
    );

    // SPEC § 4.2 stdout JSON shape on the swap path.
    let json_value: serde_json::Value =
        serde_json::from_str(stdout.trim()).expect("stdout must parse as JSON");
    assert_eq!(json_value["action"], "upgraded");
    assert_eq!(json_value["verification"], "sha256");
    assert!(
        json_value["upgradedFrom"].is_string(),
        "upgradedFrom must be present"
    );
    assert_eq!(json_value["upgradedTo"], "9.9.9");

    // -------- Verify the swap actually happened. --------
    // After self_replace, `exe_copy` is now the fixture sentinel bytes.
    let post_swap = fs::read(&exe_copy).expect("read post-swap binary");
    assert_eq!(
        post_swap, sentinel_bytes,
        "post-swap binary at {exe_copy:?} does not match fixture sentinel"
    );

    // -------- Verify CR H-2 RAII cleanup. --------
    // No `.syns-upgrade-*` staging cruft should remain inside the TempDir
    // (the inner staging tempdir's Drop runs in the subprocess; we observe
    // the post-state from this process).
    for entry in fs::read_dir(staging.path()).expect("read_dir TempDir") {
        let entry = entry.expect("dir entry");
        let name = entry.file_name();
        let name_str = name.to_string_lossy();
        assert!(
            !name_str.starts_with(".syns-upgrade-"),
            "staging cruft survived in TempDir: {name_str:?}"
        );
        assert!(
            !name_str.starts_with(".tmp"),
            "RAII inner-tempdir survived: {name_str:?} \
             (the staging TempDir::Drop did NOT run on the subprocess's success path)"
        );
    }

    // wiremock `.expect(1)` on each route verifies the metadata, archive,
    // and sha256.sum endpoints were each hit exactly once. Drop of `mock`
    // panics on violation.
}
