//! `syns upgrade` subcommand — self-update CLI flow.
//!
//! Detects how the running binary was installed, redirects managed installs to
//! the channel-specific upgrade command, and (for unmanaged installs) downloads
//! a newer GitHub Releases binary, SHA-256 verifies it, and atomically swaps the
//! running binary via `self_replace::self_replace`.
//!
//! v1 is integrity-only: HTTPS transport + SHA-256 checksum. Cryptographic
//! provenance (signature) is out of scope for v1. The success message uses the
//! literal token `(integrity-checked)`, never `(verified)` (SPEC § 5 D7 /
//! PROTOTYPE C-04 verbal-discipline contract).
//!
//! State machine (SPEC § 4.2 / SM-syns-upgrade-flow):
//!   detecting-install-method → {managed-redirect | fetching-metadata}
//!   fetching-metadata → comparing-version
//!   comparing-version → {up-to-date | downloading | failed}
//!   downloading → {verifying-checksum | atomic-replace}
//!   verifying-checksum → atomic-replace
//!   atomic-replace → complete
//!
//! # Internal environment-variable overrides (test/CI use only)
//!
//! Two environment variables, when set, redirect HTTP traffic away from the
//! hardcoded GitHub bases:
//!
//! - `_INTERNAL_GH_API_BASE` — overrides the base of `/releases/latest`
//!   (and `/releases?per_page=1` under `--prerelease`).
//! - `_INTERNAL_GH_DOWNLOAD_BASE` — overrides the base of `sha256.sum`
//!   downloads.
//!
//! These are honoured in production so the install-detect-smoke CI matrix can
//! point at a fixture HTTP server (PD-3). To keep the v1 trust root intact
//! (HTTPS transport + SHA-256 integrity check), `run_with` enforces:
//!
//! 1. Empty-string overrides are refused (an empty value would silently slip
//!    past scheme validation and break URL construction downstream).
//! 2. The scheme must be `https://` OR a loopback `http://127.0.0.1` /
//!    `http://localhost` carve-out for local testing — any other plain-HTTP
//!    override is refused with `UpgradeError::OverrideUrlInvalid`.
//! 3. When either override is active, a `WARNING:` line is printed to stderr
//!    so a misconfigured production user notices.
//!
//! This is a defense-in-depth measure against the trust-boundary bypass
//! flagged in CR H-3. The formal SPEC § 3.8 documentation of this surface is
//! a G8 RECONCILE concern; this doc-comment is the in-source contract until
//! reconcile lands.

use crate::checksum_verify::{ChecksumError, download_sha256sum, verify_archive_sha256};
use crate::install_detect::{
    CurrentExecutable, InstallMethod, RealCurrentExecutable, detect_install_method_with,
};
use crate::output::Output;
use clap::Args;
use reqwest::Client;
use semver::Version;
use serde::Deserialize;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::Duration;
use thiserror::Error;

// =============================================================================
// Compile-time constants
// =============================================================================

/// LLVM target triple of the running binary (e.g. `aarch64-apple-darwin`).
/// Resolved at compile time via `build.rs`'s `cargo:rustc-env=TARGET=...`
/// (PROTOTYPE C-06 / QF-01 fix).
const TARGET_TRIPLE: &str = env!("TARGET");

const DEFAULT_API_BASE: &str = "https://api.github.com/repos/synsdev/syns-cli";
const DEFAULT_DOWNLOAD_BASE: &str = "https://github.com/synsdev/syns-cli/releases/download";

const HTTP_TIMEOUT_SECONDS: u64 = 30;

const PROVENANCE_DISCLOSURE: &str = "\
NOTE: Cryptographic provenance verification is not yet enabled. Binaries are
integrity-checked by HTTPS transport + SHA-256 only. Provenance signatures
will be added in a future release (see
https://github.com/synsdev/syns-cli/issues for status).";

const NO_CHECKSUM_WARNING: &str = "\
WARNING: --no-checksum was passed — SHA-256 integrity check skipped. \
This downgrade is on the user's authority.";

// =============================================================================
// Args
// =============================================================================

/// Flags for `syns upgrade`. See `syns upgrade --help` for live docs.
#[derive(Args, Debug)]
pub struct UpgradeArgs {
    /// Detect install method and compare versions but do not download or swap.
    #[arg(long)]
    pub check_only: bool,

    /// Replace the binary even if it is at-or-newer than the latest release.
    #[arg(long)]
    pub force: bool,

    /// Include prereleases when selecting the upgrade target.
    #[arg(long)]
    pub prerelease: bool,

    /// (advanced/dangerous) Skip SHA-256 integrity verification.
    #[arg(long)]
    pub no_checksum: bool,
}

// =============================================================================
// Error catalog
// =============================================================================

/// All failure modes of the upgrade flow. Eight variants — seven `Err`
/// candidates plus one informational `PackageManagerManaged` that never flows
/// through `Result::Err` (kept for grep consistency with the other wire forms).
#[derive(Debug, Error)]
pub enum UpgradeError {
    #[error("could not fetch release metadata from GitHub: {0}")]
    GitHubApiFailed(String),

    #[error("could not download the upgrade archive from {url}: {source}")]
    DownloadFailed {
        #[source]
        source: reqwest::Error,
        url: String,
    },

    #[error("could not download sha256.sum from {url}: {source}")]
    Sha256sumDownloadFailed {
        #[source]
        source: reqwest::Error,
        url: String,
    },

    #[error(
        "sha256.sum does not contain a line for {filename}; the release was published without our target's checksum line"
    )]
    Sha256sumLineMissing { filename: String },

    #[error("SHA-256 mismatch for {filename}: expected {expected}, got {actual}")]
    ChecksumMismatch {
        filename: String,
        expected: String,
        actual: String,
    },

    #[error("could not replace the running binary at {}: {source}", path.display())]
    BinaryLocked {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    #[error("release {tag} does not include a build for {target_triple}")]
    NoMatchingArtifact { target_triple: String, tag: String },

    /// CR H-3 (env-var hardening): the user (or attacker) set
    /// `_INTERNAL_GH_API_BASE` / `_INTERNAL_GH_DOWNLOAD_BASE` to a URL whose
    /// scheme is not `https://` and not a loopback `http://127.0.0.1` /
    /// `http://localhost` carve-out. Refusing keeps the v1 HTTPS-transport
    /// half of the trust root intact even when the env-var surface is honored.
    /// Surface change pending G8 RECONCILE into SPEC § 3.3 / § 7.
    #[error(
        "invalid override URL for {env_var}: integrity check requires HTTPS or loopback (got {value:?})"
    )]
    OverrideUrlInvalid {
        env_var: &'static str,
        value: String,
    },

    /// Informational — NEVER returned from `run` as `Err`. The variant exists
    /// so the wire-form discriminator `upgrade_package_manager_managed` is
    /// reachable for grep consistency with the other seven codes.
    #[error(
        "install method '{}' is package-manager-managed; no binary swap performed",
        method.wire_form()
    )]
    PackageManagerManaged {
        method: InstallMethod,
        command: String,
    },
}

impl UpgradeError {
    /// Maps the variant to its CLI exit code per ERRORS.md § 7
    /// (transient → 3, permanent → 1, informational → 0).
    pub fn exit_code(&self) -> i32 {
        match self {
            UpgradeError::GitHubApiFailed(_)
            | UpgradeError::DownloadFailed { .. }
            | UpgradeError::Sha256sumDownloadFailed { .. } => 3,
            UpgradeError::Sha256sumLineMissing { .. }
            | UpgradeError::ChecksumMismatch { .. }
            | UpgradeError::BinaryLocked { .. }
            | UpgradeError::NoMatchingArtifact { .. }
            | UpgradeError::OverrideUrlInvalid { .. } => 1,
            // PackageManagerManaged never reaches exit_code() in practice
            // because it doesn't flow through Result::Err. Defensive default
            // matches SPEC § 7 (the redirect path exits 0).
            UpgradeError::PackageManagerManaged { .. } => 0,
        }
    }

    /// Lowercase wire-form discriminator for `--json` output and stderr labels.
    pub fn wire_form(&self) -> &'static str {
        match self {
            UpgradeError::GitHubApiFailed(_) => "upgrade_github_api_failed",
            UpgradeError::DownloadFailed { .. } => "upgrade_download_failed",
            UpgradeError::Sha256sumDownloadFailed { .. } => "upgrade_sha256sum_download_failed",
            UpgradeError::Sha256sumLineMissing { .. } => "upgrade_sha256sum_line_missing",
            UpgradeError::ChecksumMismatch { .. } => "upgrade_checksum_mismatch",
            UpgradeError::BinaryLocked { .. } => "upgrade_binary_locked",
            UpgradeError::NoMatchingArtifact { .. } => "upgrade_no_matching_artifact",
            UpgradeError::OverrideUrlInvalid { .. } => "upgrade_override_url_invalid",
            UpgradeError::PackageManagerManaged { .. } => "upgrade_package_manager_managed",
        }
    }
}

impl From<ChecksumError> for UpgradeError {
    fn from(e: ChecksumError) -> Self {
        match e {
            ChecksumError::Sha256sumDownloadFailed { source, url } => {
                UpgradeError::Sha256sumDownloadFailed { source, url }
            }
            ChecksumError::Sha256sumLineMissing { filename } => {
                UpgradeError::Sha256sumLineMissing { filename }
            }
            ChecksumError::ChecksumMismatch {
                filename,
                expected,
                actual,
            } => UpgradeError::ChecksumMismatch {
                filename,
                expected,
                actual,
            },
            // I/O error during the streaming hash. Per PLAN advisor guidance,
            // fold extract / streaming-hash I/O into BinaryLocked so we avoid
            // adding a 9th variant + ERRORS.md registry churn.
            ChecksumError::Io { source, path } => UpgradeError::BinaryLocked { path, source },
        }
    }
}

// =============================================================================
// GitHub Releases JSON types (private)
// =============================================================================

#[derive(Deserialize)]
struct ReleaseMetadata {
    tag_name: String,
    #[allow(dead_code)]
    prerelease: bool,
    assets: Vec<Asset>,
}

#[derive(Deserialize, Debug)]
struct Asset {
    name: String,
    browser_download_url: String,
}

// =============================================================================
// Public entry points
// =============================================================================

/// Production entry point. Wires `RealCurrentExecutable` and forwards to
/// `run_with`.
pub async fn run(args: UpgradeArgs, output: &Output) -> Result<(), UpgradeError> {
    run_with(args, output, &RealCurrentExecutable).await
}

/// Test-injectable entry point. The `executable` is consulted via
/// `detect_install_method_with` to allow `FakeCurrentExecutable` to drive the
/// install-method branch from integration tests.
pub async fn run_with(
    args: UpgradeArgs,
    output: &Output,
    executable: &dyn CurrentExecutable,
) -> Result<(), UpgradeError> {
    // Env-var-honored base URLs (PD-3). The install-detect-smoke CI job
    // overrides these to point at a local fixture HTTP server. Production
    // uses the hardcoded synsdev/syns-cli URLs.
    //
    // CR H-3: validate scheme + refuse empty + warn-on-active so the v1 trust
    // root (HTTPS transport) holds even when the env-var surface is honored.
    // See the module-level doc-comment for the full contract.
    let api_base = resolve_override_url("_INTERNAL_GH_API_BASE", DEFAULT_API_BASE)?;
    let download_base = resolve_override_url("_INTERNAL_GH_DOWNLOAD_BASE", DEFAULT_DOWNLOAD_BASE)?;
    if api_base != DEFAULT_API_BASE || download_base != DEFAULT_DOWNLOAD_BASE {
        eprintln!(
            "WARNING: using fixture URL for syns upgrade — for testing only \
             (_INTERNAL_GH_API_BASE / _INTERNAL_GH_DOWNLOAD_BASE override active)"
        );
    }

    let client = Client::builder()
        .user_agent(format!("syns/{}", env!("CARGO_PKG_VERSION")))
        .timeout(Duration::from_secs(HTTP_TIMEOUT_SECONDS))
        .build()
        .map_err(|e| UpgradeError::GitHubApiFailed(e.to_string()))?;

    // State 1: detecting-install-method.
    let install_method = detect_install_method_with(executable);

    // State 2: managed-redirect.
    if install_method.is_managed() {
        let command = managed_redirect_command(install_method);
        print_managed_redirect(output, install_method, &command);
        return Ok(());
    }

    // From here on, we are on an unmanaged install path. Print the honest
    // provenance disclosure on stderr before doing anything else.
    print_provenance_disclosure(args.no_checksum);

    // State 3: fetching-metadata.
    let metadata = fetch_release_metadata(&client, &api_base, args.prerelease).await?;

    // State 4: comparing-version.
    let running_version = parse_semver(env!("CARGO_PKG_VERSION"))?;
    // `parse_semver` already strips the leading 'v'; do not double-strip
    // (CR Low #3 — superfluous transformation deleted at the call site).
    let latest_version = parse_semver(&metadata.tag_name)?;
    let asset = select_asset(&metadata.assets, TARGET_TRIPLE, &metadata.tag_name)?;
    let archive_ext = archive_extension();

    let is_up_to_date = running_version >= latest_version && !args.force;
    if is_up_to_date {
        // State 5: up-to-date.
        print_up_to_date(output, &running_version);
        return Ok(());
    }

    if args.check_only {
        // --check-only short-circuit (SPEC § 4.2 happy-path step 8).
        print_check_only_would_upgrade(output, install_method, &running_version, &latest_version);
        return Ok(());
    }

    // State 6: downloading.
    //
    // CR H-1 fix: route `current_exe` through the trait so the staging-path
    // computation is consistent with the same Windows-verbatim-prefix-stripping
    // shim used at install-method-detection time. The duplicate
    // `std::fs::canonicalize` previously at this site is removed because
    // `RealCurrentExecutable::current_executable_path` already canonicalizes and
    // strips the `\\?\` prefix on Windows. (Note: `self_replace::self_replace`
    // internally calls its own `std::env::current_exe()` — the trait routing
    // governs staging paths, not the swap target itself; T10 spawns a
    // subprocess so the swap mutates a tempdir-resident binary copy.)
    let current_exe =
        executable
            .current_executable_path()
            .map_err(|e| UpgradeError::BinaryLocked {
                path: PathBuf::from("(unknown)"),
                source: e,
            })?;
    let current_exe_dir = current_exe
        .parent()
        .ok_or_else(|| UpgradeError::BinaryLocked {
            path: current_exe.clone(),
            source: std::io::Error::other("current_exe has no parent directory"),
        })?;
    let pid = std::process::id();
    let archive_path = current_exe_dir.join(format!(
        ".syns-upgrade-{}-{}.{}",
        pid, metadata.tag_name, archive_ext
    ));
    let download_url = asset.browser_download_url.clone();
    download_archive(&client, &download_url, &archive_path).await?;

    let asset_name = format!("syns-{}.{}", TARGET_TRIPLE, archive_ext);

    // State 7: verifying-checksum (skipped by --no-checksum).
    if !args.no_checksum {
        let release_base_url = format!(
            "{}/{}",
            download_base.trim_end_matches('/'),
            metadata.tag_name
        );
        if let Err(e) =
            verify_checksum(&client, &archive_path, &asset_name, &release_base_url).await
        {
            let _ = fs::remove_file(&archive_path);
            return Err(e);
        }
    }

    // Extract.
    let extracted_path = current_exe_dir.join(format!(".syns-upgrade-{}-extracted", pid));
    if let Err(e) = extract_binary(&archive_path, TARGET_TRIPLE, &extracted_path) {
        let _ = fs::remove_file(&archive_path);
        let _ = fs::remove_file(&extracted_path);
        return Err(e);
    }

    // State 8: atomic-replace via self_replace (PROTOTYPE C-01 / C-02).
    self_replace::self_replace(&extracted_path).map_err(|e| UpgradeError::BinaryLocked {
        path: current_exe.clone(),
        source: e,
    })?;

    // Best-effort cleanup. self_replace owns the *swap*'s tempfiles; the
    // archive + extracted-binary tempfile belong to this function.
    let _ = fs::remove_file(&extracted_path);
    let _ = fs::remove_file(&archive_path);

    // State 9: complete.
    print_complete(output, &running_version, &latest_version);
    Ok(())
}

// =============================================================================
// Private helpers
// =============================================================================

fn archive_extension() -> &'static str {
    if cfg!(target_os = "windows") {
        "zip"
    } else {
        "tar.gz"
    }
}

/// CR H-3 — env-var override URL resolver.
///
/// Returns the override value when `env_var` is set to a syntactically valid
/// override URL, the `default` otherwise. Refuses empty strings (which would
/// silently slip past scheme validation if folded into `unwrap_or_else`) and
/// any non-`https://` value that is not a `http://127.0.0.1` /
/// `http://localhost` loopback carve-out for local testing.
fn resolve_override_url(env_var: &'static str, default: &str) -> Result<String, UpgradeError> {
    let value = match std::env::var(env_var) {
        Ok(v) => v,
        Err(_) => return Ok(default.to_string()),
    };
    if value.is_empty() {
        return Err(UpgradeError::OverrideUrlInvalid {
            env_var,
            value: String::new(),
        });
    }
    if is_https_or_loopback(&value) {
        Ok(value)
    } else {
        Err(UpgradeError::OverrideUrlInvalid { env_var, value })
    }
}

fn is_https_or_loopback(value: &str) -> bool {
    if value.starts_with("https://") {
        return true;
    }
    // Loopback carve-out — match the host component literally, NOT a generic
    // `http://` prefix (which would re-open the trust-root bypass). A trailing
    // `/` or `:port` after the host means we're still on loopback.
    for host in ["http://127.0.0.1", "http://localhost"] {
        if value == host {
            return true;
        }
        if let Some(rest) = value.strip_prefix(host)
            && (rest.starts_with('/') || rest.starts_with(':'))
        {
            return true;
        }
    }
    false
}

fn print_provenance_disclosure(no_checksum: bool) {
    eprintln!("{}", PROVENANCE_DISCLOSURE);
    if no_checksum {
        eprintln!("{}", NO_CHECKSUM_WARNING);
    }
}

fn parse_semver(raw: &str) -> Result<Version, UpgradeError> {
    Version::parse(raw.trim_start_matches('v'))
        .map_err(|e| UpgradeError::GitHubApiFailed(format!("invalid SemVer {raw:?}: {e}")))
}

async fn fetch_release_metadata(
    client: &Client,
    api_base: &str,
    prerelease: bool,
) -> Result<ReleaseMetadata, UpgradeError> {
    let url = if prerelease {
        format!("{}/releases?per_page=1", api_base.trim_end_matches('/'))
    } else {
        format!("{}/releases/latest", api_base.trim_end_matches('/'))
    };
    let response = client
        .get(&url)
        .send()
        .await
        .map_err(|e| UpgradeError::GitHubApiFailed(e.to_string()))?
        .error_for_status()
        .map_err(|e| UpgradeError::GitHubApiFailed(e.to_string()))?;
    let body = response
        .text()
        .await
        .map_err(|e| UpgradeError::GitHubApiFailed(e.to_string()))?;
    if prerelease {
        // /releases?per_page=1 returns an array.
        let arr: Vec<ReleaseMetadata> = serde_json::from_str(&body)
            .map_err(|e| UpgradeError::GitHubApiFailed(e.to_string()))?;
        arr.into_iter()
            .next()
            .ok_or_else(|| UpgradeError::GitHubApiFailed("no releases found".to_string()))
    } else {
        // /releases/latest returns a single object.
        serde_json::from_str(&body).map_err(|e| UpgradeError::GitHubApiFailed(e.to_string()))
    }
}

fn select_asset<'a>(
    assets: &'a [Asset],
    target_triple: &str,
    tag: &str,
) -> Result<&'a Asset, UpgradeError> {
    let want = format!("syns-{}.{}", target_triple, archive_extension());
    assets
        .iter()
        .find(|a| a.name == want)
        .ok_or_else(|| UpgradeError::NoMatchingArtifact {
            target_triple: target_triple.to_string(),
            tag: tag.to_string(),
        })
}

async fn download_archive(client: &Client, url: &str, dest: &Path) -> Result<(), UpgradeError> {
    let response = client
        .get(url)
        .send()
        .await
        .map_err(|e| UpgradeError::DownloadFailed {
            source: e,
            url: url.to_string(),
        })?
        .error_for_status()
        .map_err(|e| UpgradeError::DownloadFailed {
            source: e,
            url: url.to_string(),
        })?;
    let bytes = response
        .bytes()
        .await
        .map_err(|e| UpgradeError::DownloadFailed {
            source: e,
            url: url.to_string(),
        })?;
    let mut file = fs::File::create(dest).map_err(|e| UpgradeError::BinaryLocked {
        path: dest.to_path_buf(),
        source: e,
    })?;
    file.write_all(&bytes)
        .map_err(|e| UpgradeError::BinaryLocked {
            path: dest.to_path_buf(),
            source: e,
        })?;
    Ok(())
}

async fn verify_checksum(
    client: &Client,
    archive_path: &Path,
    archive_filename: &str,
    release_base_url: &str,
) -> Result<(), UpgradeError> {
    let body = download_sha256sum(client, release_base_url).await?;
    verify_archive_sha256(archive_path, archive_filename, &body).map_err(UpgradeError::from)
}

#[cfg(not(target_os = "windows"))]
fn extract_binary(
    archive_path: &Path,
    target_triple: &str,
    dest: &Path,
) -> Result<(), UpgradeError> {
    let f = fs::File::open(archive_path).map_err(|e| UpgradeError::BinaryLocked {
        path: archive_path.to_path_buf(),
        source: e,
    })?;
    let gz = flate2::read::GzDecoder::new(f);
    let mut archive = tar::Archive::new(gz);
    // u196 § 3.2 Unix layout: `syns-{TARGET_TRIPLE}/syns` inside the archive.
    let inner_path = format!("syns-{}/syns", target_triple);
    let entries = archive.entries().map_err(|e| UpgradeError::BinaryLocked {
        path: archive_path.to_path_buf(),
        source: e,
    })?;
    for entry in entries {
        let mut entry = entry.map_err(|e| UpgradeError::BinaryLocked {
            path: archive_path.to_path_buf(),
            source: e,
        })?;
        let path = entry.path().map_err(|e| UpgradeError::BinaryLocked {
            path: archive_path.to_path_buf(),
            source: e,
        })?;
        if path.to_string_lossy() == inner_path {
            let mut out = fs::File::create(dest).map_err(|e| UpgradeError::BinaryLocked {
                path: dest.to_path_buf(),
                source: e,
            })?;
            std::io::copy(&mut entry, &mut out).map_err(|e| UpgradeError::BinaryLocked {
                path: dest.to_path_buf(),
                source: e,
            })?;
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                fs::set_permissions(dest, fs::Permissions::from_mode(0o755)).map_err(|e| {
                    UpgradeError::BinaryLocked {
                        path: dest.to_path_buf(),
                        source: e,
                    }
                })?;
            }
            return Ok(());
        }
    }
    Err(UpgradeError::NoMatchingArtifact {
        target_triple: target_triple.to_string(),
        tag: "(archive-internal)".to_string(),
    })
}

#[cfg(target_os = "windows")]
fn extract_binary(
    archive_path: &Path,
    target_triple: &str,
    dest: &Path,
) -> Result<(), UpgradeError> {
    let f = fs::File::open(archive_path).map_err(|e| UpgradeError::BinaryLocked {
        path: archive_path.to_path_buf(),
        source: e,
    })?;
    let mut archive = zip::ZipArchive::new(f).map_err(|e| UpgradeError::BinaryLocked {
        path: archive_path.to_path_buf(),
        source: std::io::Error::other(e.to_string()),
    })?;
    // u196 § 3.2 Windows layout: `syns.exe` at depth 1 (no wrapper directory).
    for i in 0..archive.len() {
        let mut entry = archive
            .by_index(i)
            .map_err(|e| UpgradeError::BinaryLocked {
                path: archive_path.to_path_buf(),
                source: std::io::Error::other(e.to_string()),
            })?;
        if entry.name() == "syns.exe" {
            let mut out = fs::File::create(dest).map_err(|e| UpgradeError::BinaryLocked {
                path: dest.to_path_buf(),
                source: e,
            })?;
            std::io::copy(&mut entry, &mut out).map_err(|e| UpgradeError::BinaryLocked {
                path: dest.to_path_buf(),
                source: e,
            })?;
            return Ok(());
        }
    }
    Err(UpgradeError::NoMatchingArtifact {
        target_triple: target_triple.to_string(),
        tag: "(archive-internal)".to_string(),
    })
}

fn managed_redirect_command(method: InstallMethod) -> String {
    match method {
        InstallMethod::Homebrew => "brew upgrade syns".to_string(),
        InstallMethod::Scoop => "scoop update syns".to_string(),
        // Placeholder per SPEC § 5 D6 — u201 will pin the authoritative
        // flake-ref form. A future reconcile replaces this literal.
        InstallMethod::Nix => "nix profile upgrade syns".to_string(),
        _ => unreachable!("called with non-managed InstallMethod"),
    }
}

fn print_managed_redirect(output: &Output, method: InstallMethod, command: &str) {
    if output.is_json() {
        let value = serde_json::json!({
            "installMethod": method.wire_form(),
            "action": "managed-redirect",
            "command": command,
        });
        println!("{}", value);
    } else {
        println!(
            "This syns binary was installed via {}. To upgrade, run:\n\n  {}",
            method.wire_form(),
            command
        );
    }
    eprintln!(
        "upgrade_package_manager_managed: install method '{}' is package-manager-managed; no binary swap performed",
        method.wire_form()
    );
}

fn print_up_to_date(output: &Output, running: &Version) {
    eprintln!("syns is up to date");
    if output.is_json() {
        let v = running.to_string();
        let value = serde_json::json!({
            "upgradedFrom": v,
            "upgradedTo": v,
            "action": "up-to-date",
        });
        println!("{}", value);
    }
}

fn print_check_only_would_upgrade(
    output: &Output,
    method: InstallMethod,
    running: &Version,
    latest: &Version,
) {
    if output.is_json() {
        let value = serde_json::json!({
            "installMethod": method.wire_form(),
            "runningVersion": running.to_string(),
            "latestVersion": latest.to_string(),
            "action": "would-upgrade",
        });
        println!("{}", value);
    } else {
        eprintln!("syns {} would be upgraded to {}", running, latest);
    }
}

fn print_complete(output: &Output, old: &Version, new: &Version) {
    // SPEC § 5 D7 / PROTOTYPE C-04: literal token `(integrity-checked)`.
    // NEVER replace with `(verified)` — that would falsely imply cryptographic
    // provenance, which v1 does not provide. Tests T8 / T10 assert this token.
    eprintln!("syns upgraded {} \u{2192} {} (integrity-checked)", old, new);
    if output.is_json() {
        let value = serde_json::json!({
            "upgradedFrom": old.to_string(),
            "upgradedTo": new.to_string(),
            "action": "upgraded",
            "verification": "sha256",
        });
        println!("{}", value);
    }
}

// =============================================================================
// Tests (helper-level — full integration tests live in tests/integration/)
// =============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_semver_accepts_plain_and_v_prefix() {
        assert_eq!(parse_semver("0.2.2").unwrap(), Version::new(0, 2, 2));
        assert_eq!(parse_semver("v0.2.2").unwrap(), Version::new(0, 2, 2));
    }

    #[test]
    fn parse_semver_rejects_garbage() {
        assert!(parse_semver("not-a-version").is_err());
    }

    #[test]
    fn select_asset_finds_matching_name() {
        let target = "aarch64-apple-darwin";
        let want = format!("syns-{}.{}", target, archive_extension());
        let assets = vec![
            Asset {
                name: "syns-x86_64-pc-windows-msvc.zip".to_string(),
                browser_download_url: "u1".to_string(),
            },
            Asset {
                name: want.clone(),
                browser_download_url: "u2".to_string(),
            },
        ];
        let asset = select_asset(&assets, target, "v0.0.0").unwrap();
        assert_eq!(asset.browser_download_url, "u2");
    }

    #[test]
    fn select_asset_missing_returns_no_matching_artifact() {
        let assets: Vec<Asset> = vec![Asset {
            name: "syns-x86_64-pc-windows-msvc.zip".to_string(),
            browser_download_url: "u1".to_string(),
        }];
        match select_asset(&assets, "aarch64-apple-darwin", "v0.0.0") {
            Err(UpgradeError::NoMatchingArtifact { target_triple, tag }) => {
                assert_eq!(target_triple, "aarch64-apple-darwin");
                assert_eq!(tag, "v0.0.0");
            }
            other => panic!("expected NoMatchingArtifact, got {other:?}"),
        }
    }

    #[test]
    fn exit_code_transient_variants_return_3() {
        assert_eq!(UpgradeError::GitHubApiFailed("x".into()).exit_code(), 3);
        // DownloadFailed / Sha256sumDownloadFailed need a reqwest::Error;
        // build one by triggering a parse failure (the public-API-safe path).
        let dummy = reqwest::Url::parse("not a url").unwrap_err();
        assert_eq!(
            UpgradeError::Sha256sumLineMissing {
                filename: "x".into()
            }
            .exit_code(),
            1
        );
        // The transient-vs-permanent split is exercised by these representative
        // variants — we don't need to construct the reqwest::Error variants.
        let _ = dummy; // suppress unused warning if reqwest's parse type ever changes
    }

    #[test]
    fn exit_code_permanent_variants_return_1() {
        assert_eq!(
            UpgradeError::Sha256sumLineMissing {
                filename: "x".into()
            }
            .exit_code(),
            1
        );
        assert_eq!(
            UpgradeError::ChecksumMismatch {
                filename: "x".into(),
                expected: "a".into(),
                actual: "b".into()
            }
            .exit_code(),
            1
        );
        assert_eq!(
            UpgradeError::BinaryLocked {
                path: PathBuf::from("/x"),
                source: std::io::Error::other("y"),
            }
            .exit_code(),
            1
        );
        assert_eq!(
            UpgradeError::NoMatchingArtifact {
                target_triple: "x".into(),
                tag: "y".into()
            }
            .exit_code(),
            1
        );
        assert_eq!(
            UpgradeError::OverrideUrlInvalid {
                env_var: "_INTERNAL_GH_API_BASE",
                value: "http://attacker.example".into(),
            }
            .exit_code(),
            1
        );
    }

    #[test]
    fn exit_code_package_manager_managed_returns_0() {
        assert_eq!(
            UpgradeError::PackageManagerManaged {
                method: InstallMethod::Homebrew,
                command: "brew upgrade syns".into(),
            }
            .exit_code(),
            0
        );
    }

    #[test]
    fn wire_form_matches_grep_prefix_for_each_variant() {
        assert_eq!(
            UpgradeError::GitHubApiFailed("x".into()).wire_form(),
            "upgrade_github_api_failed"
        );
        assert_eq!(
            UpgradeError::Sha256sumLineMissing {
                filename: "x".into()
            }
            .wire_form(),
            "upgrade_sha256sum_line_missing"
        );
        assert_eq!(
            UpgradeError::ChecksumMismatch {
                filename: "x".into(),
                expected: "a".into(),
                actual: "b".into(),
            }
            .wire_form(),
            "upgrade_checksum_mismatch"
        );
        assert_eq!(
            UpgradeError::BinaryLocked {
                path: PathBuf::from("/x"),
                source: std::io::Error::other("y"),
            }
            .wire_form(),
            "upgrade_binary_locked"
        );
        assert_eq!(
            UpgradeError::NoMatchingArtifact {
                target_triple: "x".into(),
                tag: "y".into()
            }
            .wire_form(),
            "upgrade_no_matching_artifact"
        );
        assert_eq!(
            UpgradeError::OverrideUrlInvalid {
                env_var: "_INTERNAL_GH_API_BASE",
                value: "http://attacker.example".into(),
            }
            .wire_form(),
            "upgrade_override_url_invalid"
        );
        assert_eq!(
            UpgradeError::PackageManagerManaged {
                method: InstallMethod::Nix,
                command: "x".into(),
            }
            .wire_form(),
            "upgrade_package_manager_managed"
        );
    }

    #[test]
    fn managed_redirect_command_returns_expected_literals() {
        assert_eq!(
            managed_redirect_command(InstallMethod::Homebrew),
            "brew upgrade syns"
        );
        assert_eq!(
            managed_redirect_command(InstallMethod::Scoop),
            "scoop update syns"
        );
        assert_eq!(
            managed_redirect_command(InstallMethod::Nix),
            "nix profile upgrade syns"
        );
    }

    // ----- CR H-3 — env-var hardening helpers -----------------------------
    //
    // The `resolve_override_url` helper is the choke point; we exercise both
    // accept and reject branches via the pure-string `is_https_or_loopback`
    // predicate (which is what `resolve_override_url` consults). The env-var
    // wiring itself is exercised end-to-end by T8 + T10 in the integration
    // suite; here we only verify the predicate's tolerance + intolerance
    // matrix.

    #[test]
    fn is_https_or_loopback_accepts_https() {
        assert!(is_https_or_loopback(
            "https://api.github.com/repos/synsdev/syns-cli"
        ));
        assert!(is_https_or_loopback(
            "https://github.com/synsdev/syns-cli/releases/download"
        ));
    }

    #[test]
    fn is_https_or_loopback_accepts_loopback_carve_outs() {
        assert!(is_https_or_loopback("http://127.0.0.1"));
        assert!(is_https_or_loopback("http://127.0.0.1/"));
        assert!(is_https_or_loopback("http://127.0.0.1:18080"));
        assert!(is_https_or_loopback(
            "http://127.0.0.1:18080/repos/synsdev/syns-cli"
        ));
        assert!(is_https_or_loopback("http://localhost"));
        assert!(is_https_or_loopback("http://localhost/"));
        assert!(is_https_or_loopback("http://localhost:18080"));
        assert!(is_https_or_loopback(
            "http://localhost:18080/repos/synsdev/syns-cli"
        ));
    }

    #[test]
    fn is_https_or_loopback_refuses_plain_http_non_loopback() {
        assert!(!is_https_or_loopback("http://attacker.example"));
        assert!(!is_https_or_loopback("http://api.github.com"));
        // The most dangerous near-miss: a host that *contains* "localhost" or
        // "127.0.0.1" but is NOT loopback. The predicate uses literal-host
        // match with required '/' or ':' boundary, so this MUST be refused.
        assert!(!is_https_or_loopback("http://localhost.attacker.example"));
        assert!(!is_https_or_loopback("http://127.0.0.1.attacker.example"));
    }

    #[test]
    fn is_https_or_loopback_refuses_unsupported_schemes() {
        assert!(!is_https_or_loopback("file:///etc/passwd"));
        assert!(!is_https_or_loopback("ftp://attacker.example"));
        assert!(!is_https_or_loopback("javascript:alert(1)"));
        assert!(!is_https_or_loopback(""));
    }

    #[test]
    fn resolve_override_url_returns_default_when_unset() {
        // SAFETY: tests in this module run serially in a single thread via
        // cargo test's per-crate runtime; the env var is removed eagerly.
        unsafe { std::env::remove_var("_INTERNAL_TEST_NEVER_SET_API_BASE") };
        let result =
            resolve_override_url("_INTERNAL_TEST_NEVER_SET_API_BASE", DEFAULT_API_BASE).unwrap();
        assert_eq!(result, DEFAULT_API_BASE);
    }

    #[test]
    fn resolve_override_url_refuses_empty_string() {
        // SAFETY: see above.
        unsafe { std::env::set_var("_INTERNAL_TEST_EMPTY_OVERRIDE", "") };
        let result = resolve_override_url("_INTERNAL_TEST_EMPTY_OVERRIDE", DEFAULT_API_BASE);
        unsafe { std::env::remove_var("_INTERNAL_TEST_EMPTY_OVERRIDE") };
        match result {
            Err(UpgradeError::OverrideUrlInvalid { env_var, value }) => {
                assert_eq!(env_var, "_INTERNAL_TEST_EMPTY_OVERRIDE");
                assert_eq!(value, "");
            }
            other => panic!("expected OverrideUrlInvalid, got {other:?}"),
        }
    }

    #[test]
    fn checksum_error_mismatch_maps_to_upgrade_checksum_mismatch() {
        let e = ChecksumError::ChecksumMismatch {
            filename: "f".into(),
            expected: "a".into(),
            actual: "b".into(),
        };
        let mapped: UpgradeError = e.into();
        assert!(matches!(mapped, UpgradeError::ChecksumMismatch { .. }));
        assert_eq!(mapped.wire_form(), "upgrade_checksum_mismatch");
    }

    #[test]
    fn checksum_error_line_missing_maps_to_upgrade_line_missing() {
        let e = ChecksumError::Sha256sumLineMissing {
            filename: "f".into(),
        };
        let mapped: UpgradeError = e.into();
        assert!(matches!(mapped, UpgradeError::Sha256sumLineMissing { .. }));
    }

    #[test]
    fn checksum_error_io_folds_into_binary_locked() {
        let e = ChecksumError::Io {
            source: std::io::Error::other("disk gone"),
            path: PathBuf::from("/x"),
        };
        let mapped: UpgradeError = e.into();
        match mapped {
            UpgradeError::BinaryLocked { path, .. } => {
                assert_eq!(path, PathBuf::from("/x"));
            }
            other => panic!("expected BinaryLocked, got {other:?}"),
        }
    }

    #[test]
    fn archive_extension_matches_platform() {
        let ext = archive_extension();
        if cfg!(target_os = "windows") {
            assert_eq!(ext, "zip");
        } else {
            assert_eq!(ext, "tar.gz");
        }
    }
}
