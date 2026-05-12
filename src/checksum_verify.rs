//! HTTPS + SHA-256 integrity helper for `syns upgrade`.
//!
//! Downloads the per-release `sha256.sum` file from GitHub Releases, streams a
//! SHA-256 hash of the locally-downloaded archive, and compares the two.
//!
//! Wire-form: GNU `shasum`-style lines `{64_HEX}  {FILENAME}` (two-space
//! separator), with BSD-style single-space and binary-mode `*` prefix tolerated
//! via `str::split_whitespace` + `trim_start_matches('*')`.

use sha2::{Digest, Sha256};
use std::fs::File;
use std::io::{self, BufReader};
use std::path::{Path, PathBuf};
use thiserror::Error;

/// Errors surfaced from the checksum verification helpers.
///
/// These map cleanly into `UpgradeError` via the `From` impl declared in
/// `commands::upgrade`; see SPEC § 3.3 for the bridge.
#[derive(Debug, Error)]
pub enum ChecksumError {
    #[error("could not download sha256.sum from {url}: {source}")]
    Sha256sumDownloadFailed {
        #[source]
        source: reqwest::Error,
        url: String,
    },

    #[error("sha256.sum does not contain a line for {filename}")]
    Sha256sumLineMissing { filename: String },

    #[error("SHA-256 mismatch for {filename}: expected {expected}, got {actual}")]
    ChecksumMismatch {
        filename: String,
        expected: String,
        actual: String,
    },

    #[error("I/O error reading {}: {source}", path.display())]
    Io {
        #[source]
        source: io::Error,
        path: PathBuf,
    },
}

/// Downloads the `sha256.sum` file from `{release_base_url}/sha256.sum`.
///
/// Caller-supplied `release_base_url` is the per-release base URL (e.g.
/// `https://github.com/synsdev/syns-cli/releases/download/v0.2.3`); the helper
/// appends `/sha256.sum` exactly once.
pub async fn download_sha256sum(
    client: &reqwest::Client,
    release_base_url: &str,
) -> Result<String, ChecksumError> {
    let url = format!("{}/sha256.sum", release_base_url.trim_end_matches('/'));
    let response = client
        .get(&url)
        .header("User-Agent", format!("syns/{}", env!("CARGO_PKG_VERSION")))
        .timeout(std::time::Duration::from_secs(30))
        .send()
        .await
        .map_err(|e| ChecksumError::Sha256sumDownloadFailed {
            source: e,
            url: url.clone(),
        })?;
    let response =
        response
            .error_for_status()
            .map_err(|e| ChecksumError::Sha256sumDownloadFailed {
                source: e,
                url: url.clone(),
            })?;
    response
        .text()
        .await
        .map_err(|e| ChecksumError::Sha256sumDownloadFailed { source: e, url })
}

/// Streams a SHA-256 hash of `archive_path` and compares it to the line in
/// `sha256sum_body` whose filename matches `archive_filename`.
///
/// Returns `Ok(())` on match. Returns `Err(Sha256sumLineMissing)` when no
/// line matches, `Err(ChecksumMismatch)` when the hash differs, or `Err(Io)`
/// when the local archive cannot be read.
pub fn verify_archive_sha256(
    archive_path: &Path,
    archive_filename: &str,
    sha256sum_body: &str,
) -> Result<(), ChecksumError> {
    let expected = parse_sha256sum_line(sha256sum_body, archive_filename)
        .ok_or_else(|| ChecksumError::Sha256sumLineMissing {
            filename: archive_filename.to_string(),
        })?
        .to_lowercase();
    let file = File::open(archive_path).map_err(|e| ChecksumError::Io {
        source: e,
        path: archive_path.to_path_buf(),
    })?;
    let mut reader = BufReader::new(file);
    let mut hasher = Sha256::new();
    io::copy(&mut reader, &mut hasher).map_err(|e| ChecksumError::Io {
        source: e,
        path: archive_path.to_path_buf(),
    })?;
    let actual = format!("{:x}", hasher.finalize()).to_lowercase();
    if actual == expected {
        Ok(())
    } else {
        Err(ChecksumError::ChecksumMismatch {
            filename: archive_filename.to_string(),
            expected,
            actual,
        })
    }
}

fn parse_sha256sum_line<'a>(body: &'a str, filename: &str) -> Option<&'a str> {
    for line in body.lines() {
        let mut parts = line.split_whitespace();
        let digest = parts.next()?;
        let name = parts.next()?;
        // GNU shasum format: "{64_HEX}  {FILENAME}" (two spaces).
        // Tolerate BSD-style variations via split_whitespace.
        // Filename may have a leading '*' marker in binary mode — strip it.
        let name = name.trim_start_matches('*');
        if name == filename && digest.len() == 64 && digest.chars().all(|c| c.is_ascii_hexdigit()) {
            return Some(digest);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    // SHA-256 of the ASCII string "hello":
    const HELLO_SHA256: &str = "2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824";

    fn write_temp_hello() -> tempfile::NamedTempFile {
        let mut f = tempfile::NamedTempFile::new().unwrap();
        f.write_all(b"hello").unwrap();
        f.flush().unwrap();
        f
    }

    #[test]
    fn t5_verify_matching_digest_returns_ok() {
        let f = write_temp_hello();
        let body = format!("{HELLO_SHA256}  syns-fixture.tar.gz\n");
        let result = verify_archive_sha256(f.path(), "syns-fixture.tar.gz", &body);
        assert!(matches!(result, Ok(())), "expected Ok, got {result:?}");
    }

    #[test]
    fn t6_verify_mismatching_digest_returns_checksum_mismatch() {
        let f = write_temp_hello();
        let body = "0000000000000000000000000000000000000000000000000000000000000000  syns-fixture.tar.gz\n";
        let result = verify_archive_sha256(f.path(), "syns-fixture.tar.gz", body);
        match result {
            Err(ChecksumError::ChecksumMismatch {
                filename,
                expected,
                actual,
            }) => {
                assert_eq!(filename, "syns-fixture.tar.gz");
                assert_eq!(expected, "0".repeat(64));
                assert_eq!(actual, HELLO_SHA256);
            }
            other => panic!("expected ChecksumMismatch, got {other:?}"),
        }
    }

    #[test]
    fn t7_verify_missing_line_returns_sha256sum_line_missing() {
        let f = write_temp_hello();
        let body = format!("{HELLO_SHA256}  syns-other.tar.gz\n");
        let result = verify_archive_sha256(f.path(), "syns-fixture.tar.gz", &body);
        match result {
            Err(ChecksumError::Sha256sumLineMissing { filename }) => {
                assert_eq!(filename, "syns-fixture.tar.gz");
            }
            other => panic!("expected Sha256sumLineMissing, got {other:?}"),
        }
    }

    #[test]
    fn parse_handles_bsd_star_prefix() {
        let body = format!("{HELLO_SHA256} *syns-fixture.tar.gz\n");
        // The parser tolerates BSD-style "binary mode" lines where the
        // filename is prefixed with '*'.
        assert_eq!(
            parse_sha256sum_line(&body, "syns-fixture.tar.gz"),
            Some(HELLO_SHA256)
        );
    }

    #[test]
    fn parse_rejects_non_hex_digest() {
        let body = "zzzz1111zzzz1111zzzz1111zzzz1111zzzz1111zzzz1111zzzz1111zzzz1111  syns-fixture.tar.gz\n";
        assert_eq!(parse_sha256sum_line(body, "syns-fixture.tar.gz"), None);
    }

    #[test]
    fn io_error_on_missing_archive_returns_io_variant() {
        let body = format!("{HELLO_SHA256}  ghost.tar.gz\n");
        let result = verify_archive_sha256(
            Path::new("/nonexistent/path/ghost.tar.gz"),
            "ghost.tar.gz",
            &body,
        );
        assert!(matches!(result, Err(ChecksumError::Io { .. })));
    }
}
