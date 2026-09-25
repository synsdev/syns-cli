//! The run's content cache, keyed on the content hash alone (SPEC u270).
//!
//! `INV-29` makes equal byte sequences produce equal hashes; its
//! converse, which a hash-keyed store leans on, holds only where every
//! hash a file was stored under was derived from the bytes stored under
//! it — so an answer is admitted to the store only when the derivation
//! over the answered bytes equals the served `sha`. Unchecked, a
//! deployment a caller points at with `--server` would put its own bytes
//! under a hash it never served, and every later run of any repository
//! against any deployment would answer those bytes with no request made
//! (`SPEC_REVIEW_R3.md` CF-01, `THR-folder-path-escape`).

use std::path::{Path, PathBuf};
use std::time::SystemTime;

use crate::client::SynsClient;
use crate::config::Config;
use crate::errors::CliError;
use crate::push::collector::is_text;
use crate::push::hash::blob_sha1;
use crate::read::ReadTarget;

/// The cache cap's environment name (SPEC u270 Contract Surface).
pub const CACHE_MAX_BYTES_ENV: &str = "SYNS_CACHE_MAX_BYTES";

/// The cap where the environment name is unset or parses as no positive
/// integer — 256 MiB.
pub const DEFAULT_CACHE_MAX_BYTES: u64 = 268_435_456;

#[cfg(unix)]
const BLOBS_DIR_MODE: u32 = 0o700;
#[cfg(unix)]
const BLOB_FILE_MODE: u32 = 0o600;

/// The `blobs` directory under the run's cache directory, and the cap it
/// is held at. Holds no file any other command of the binary reads.
#[derive(Debug, Clone)]
pub struct BlobCache {
    root: PathBuf,
    max_bytes: u64,
}

/// What one path's content came back as.
///
/// `Binary` is the classification a served content takes, on a NUL byte
/// in the answered string. `Refused` carries that refusal's one string.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BlobContent {
    Text(String),
    Binary,
    Refused(String),
}

/// True for a 40-character lowercase-hex blob hash and nothing else.
fn is_blob_hash(value: &str) -> bool {
    value.len() == 40
        && value
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

/// The cap this run holds the `blobs` directory at.
fn max_bytes_from_env() -> u64 {
    match std::env::var(CACHE_MAX_BYTES_ENV) {
        Ok(value) => value
            .trim()
            .parse::<u64>()
            .ok()
            .filter(|n| *n > 0)
            .unwrap_or(DEFAULT_CACHE_MAX_BYTES),
        Err(_) => DEFAULT_CACHE_MAX_BYTES,
    }
}

impl BlobCache {
    /// The only constructor: the cap and the directory stand at one
    /// value for the whole run.
    pub fn open(config: &Config) -> Result<BlobCache, CliError> {
        let root = config.cache_dir().join("blobs");
        std::fs::create_dir_all(&root).map_err(|e| CliError::Io {
            message: format!("could not create the content cache: {e}"),
        })?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&root, std::fs::Permissions::from_mode(BLOBS_DIR_MODE))
                .map_err(|e| CliError::Io {
                    message: format!("could not create the content cache: {e}"),
                })?;
        }
        Ok(BlobCache {
            root,
            max_bytes: max_bytes_from_env(),
        })
    }

    /// The `blobs` directory this run reads and writes.
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// The cap this run holds it at.
    pub fn max_bytes(&self) -> u64 {
        self.max_bytes
    }

    /// Answers one path's content at the target's pinned reference,
    /// through the store where the hash the tree named already stands
    /// there (SPEC u270 Behaviour, `BlobCache::get_or_fetch`).
    pub async fn get_or_fetch(
        &self,
        client: &SynsClient,
        target: &ReadTarget,
        path: &str,
        sha: Option<&str>,
    ) -> Result<BlobContent, CliError> {
        // 1 — a stored entry answers with no request made, whatever
        // deployment stored it. A read failure falls through to step 2.
        if let Some(hash) = sha.filter(|s| is_blob_hash(s)) {
            let entry = self.root.join(hash);
            if let Ok(bytes) = std::fs::read(&entry)
                && let Ok(text) = String::from_utf8(bytes)
            {
                touch(&entry);
                return Ok(BlobContent::Text(text));
            }
        }

        // 2 — read the path at the target's reference.
        let version_ref = target.version_ref();
        let response = match client
            .get_file(
                &target.repo_id,
                target.token.as_deref(),
                path,
                Some(&version_ref),
            )
            .await
        {
            Ok((response, _raw)) => response,
            Err(e) if is_refused(&e) => return Ok(BlobContent::Refused(e.to_string())),
            Err(e) => return Err(e),
        };

        // 3 — classify the decoded answer as text or binary on a NUL
        // byte, the test `src/push/collector.rs` takes over a local file
        // taken here over the decoded answer.
        // `content: null` is a content that is not text, and so is one
        // `is_text` refuses (SPEC u280 `get_or_fetch` 3).
        let content = match response.content {
            Some(content) if is_text(content.as_bytes()) => content,
            _ => return Ok(BlobContent::Binary),
        };

        // 4 — derive the blob hash of the answered bytes and store a
        // text answer under that hash alone, and only where it equals
        // the served one.
        if is_blob_hash(&response.sha) && blob_sha1(content.as_bytes()) == response.sha {
            self.store(&response.sha, content.as_bytes());
        }
        Ok(BlobContent::Text(content))
    }

    /// Writes one blob through a named temporary in the destination's
    /// own directory, permissioned before the bytes and persisted after
    /// them. A write failure stores nothing and is not raised — the
    /// content is answered either way.
    fn store(&self, hash: &str, bytes: &[u8]) {
        use std::io::Write;
        let Ok(mut tmp) = tempfile::NamedTempFile::new_in(&self.root) else {
            return;
        };
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            if tmp
                .as_file()
                .set_permissions(std::fs::Permissions::from_mode(BLOB_FILE_MODE))
                .is_err()
            {
                return;
            }
        }
        if tmp.write_all(bytes).is_err() {
            return;
        }
        let _ = tmp.persist(self.root.join(hash));
    }

    /// The one eviction pass of a run, taken after its last fetch has
    /// landed and never beside another, so the `blobs` directory is
    /// sized once over a set nothing is still writing into.
    pub fn sweep(&self) -> Result<(), CliError> {
        let Ok(dir) = std::fs::read_dir(&self.root) else {
            return Ok(());
        };
        let mut total: u64 = 0;
        let mut entries: Vec<(SystemTime, u64, PathBuf)> = Vec::new();
        for entry in dir.flatten() {
            let Ok(meta) = entry.metadata() else { continue };
            if !meta.is_file() {
                continue;
            }
            let touched = meta.modified().unwrap_or(SystemTime::UNIX_EPOCH);
            total += meta.len();
            entries.push((touched, meta.len(), entry.path()));
        }
        if total <= self.max_bytes {
            return Ok(());
        }
        entries.sort_by_key(|(touched, _, _)| *touched);
        for (_, len, path) in entries {
            if total <= self.max_bytes {
                break;
            }
            match std::fs::remove_file(&path) {
                Ok(()) => total -= len,
                // A removal failure leaves the entry and stops the pass,
                // the directory standing over the cap until the next run.
                Err(_) => break,
            }
        }
        Ok(())
    }
}

/// Touches a stored entry's access instant, so the eviction pass reads
/// it as the most recently used of the set.
fn touch(entry: &Path) {
    let now = SystemTime::now();
    if let Ok(file) = std::fs::File::options().write(true).open(entry) {
        let _ = file.set_times(
            std::fs::FileTimes::new()
                .set_accessed(now)
                .set_modified(now),
        );
    }
}

/// The refusal classes one path answers `Refused` for, the fan-out
/// carrying on past each: a refusal of class not-found or internal, a
/// request that reached no answer at all, and an answer whose `content`
/// is no decodable string (`SPEC_REVIEW_R3.md` QF-03). Every other
/// refusal ends the run at its own exit code.
fn is_refused(err: &CliError) -> bool {
    match err {
        CliError::ServerUnreachable { .. } => true,
        CliError::Api { status: None, .. } => true,
        CliError::Api {
            status: Some(status),
            ..
        } => *status == 404 || *status == 200 || *status >= 500,
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::read::ResolvedRef;
    use serial_test::serial;
    use wiremock::matchers::{method, path as path_matcher};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn open_cache(cache_dir: &Path, cap: Option<&str>) -> BlobCache {
        unsafe { std::env::set_var("SYNS_CACHE_DIR", cache_dir) };
        match cap {
            Some(value) => unsafe { std::env::set_var(CACHE_MAX_BYTES_ENV, value) },
            None => unsafe { std::env::remove_var(CACHE_MAX_BYTES_ENV) },
        }
        let config = Config::new(Some("https://syns.dev")).unwrap();
        let cache = BlobCache::open(&config).unwrap();
        unsafe { std::env::remove_var("SYNS_CACHE_DIR") };
        unsafe { std::env::remove_var(CACHE_MAX_BYTES_ENV) };
        cache
    }

    fn target(repo_id: &str, token: Option<&str>) -> ReadTarget {
        ReadTarget {
            repo_id: repo_id.to_string(),
            token: token.map(String::from),
            reference: ResolvedRef {
                version: 2,
                commit_sha: "b".repeat(40),
            },
        }
    }

    // SPEC u270 Contract Surface, `BlobCache`: `root` is the `blobs`
    // directory under the run's cache directory, created at mode `0700`.
    #[test]
    #[serial]
    fn the_blobs_directory_is_created_at_its_registered_mode() {
        let dir = tempfile::tempdir().unwrap();
        let cache = open_cache(dir.path(), None);
        assert_eq!(cache.root(), dir.path().join("blobs"));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(cache.root())
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(mode & 0o777, 0o700, "the blobs directory stands at 0700");
        }
    }

    // SPEC u270 Contract Surface, the cache cap's environment name: a
    // byte count, `268435456` where it is unset or parses as no
    // positive integer.
    #[test]
    #[serial]
    fn the_cap_falls_back_where_its_environment_value_is_no_positive_integer() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(open_cache(dir.path(), None).max_bytes(), 268_435_456);
        assert_eq!(open_cache(dir.path(), Some("0")).max_bytes(), 268_435_456);
        assert_eq!(open_cache(dir.path(), Some("-1")).max_bytes(), 268_435_456);
        assert_eq!(
            open_cache(dir.path(), Some("many")).max_bytes(),
            268_435_456
        );
        assert_eq!(open_cache(dir.path(), Some("")).max_bytes(), 268_435_456);
        assert_eq!(open_cache(dir.path(), Some("4096")).max_bytes(), 4096);
    }

    // SPEC u270 Tests: `a_blob_whose_served_hash_does_not_match_its_bytes_is_not_stored`.
    #[tokio::test]
    #[serial]
    async fn a_blob_whose_served_hash_does_not_match_its_bytes_is_not_stored() {
        let dir = tempfile::tempdir().unwrap();
        let cache = open_cache(dir.path(), None);
        let server = MockServer::start().await;
        let lie = "a".repeat(40);
        Mock::given(method("GET"))
            .and(path_matcher("/api/v1/repos/alice/notes/files/a.md"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "path": "a.md", "sha": lie, "content": "fn one\n", "size": 7,
            })))
            .mount(&server)
            .await;
        let client = SynsClient::new(&server.uri()).unwrap();
        let target = target("alice/notes", None);

        for _ in 0..2 {
            let answer = cache
                .get_or_fetch(&client, &target, "a.md", Some(&lie))
                .await
                .unwrap();
            assert_eq!(answer, BlobContent::Text("fn one\n".to_string()));
        }

        let stored: Vec<_> = std::fs::read_dir(cache.root())
            .unwrap()
            .flatten()
            .map(|e| e.path())
            .collect();
        assert!(stored.is_empty(), "nothing is stored: {stored:?}");
        assert_eq!(server.received_requests().await.unwrap().len(), 2);
    }

    // A served hash the derivation agrees with is stored, and the
    // second call answers it with no request made.
    #[tokio::test]
    #[serial]
    async fn a_matching_blob_is_stored_and_answered_with_no_request_made() {
        let dir = tempfile::tempdir().unwrap();
        let cache = open_cache(dir.path(), None);
        let server = MockServer::start().await;
        let content = "fn one\n";
        let sha = blob_sha1(content.as_bytes());
        Mock::given(method("GET"))
            .and(path_matcher("/api/v1/repos/alice/notes/files/a.md"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "path": "a.md", "sha": sha, "content": content, "size": content.len(),
            })))
            .mount(&server)
            .await;
        let client = SynsClient::new(&server.uri()).unwrap();
        let target = target("alice/notes", None);

        for _ in 0..2 {
            let answer = cache
                .get_or_fetch(&client, &target, "a.md", Some(&sha))
                .await
                .unwrap();
            assert_eq!(answer, BlobContent::Text(content.to_string()));
        }
        assert_eq!(server.received_requests().await.unwrap().len(), 1);
        assert!(cache.root().join(&sha).is_file());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(cache.root().join(&sha))
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(mode & 0o777, 0o600);
        }
    }

    // SPEC u270 Contract Surface, `BlobContent`: `Binary` is the
    // classification a served content takes, on a NUL byte, and a binary
    // answer is stored by nothing.
    #[tokio::test]
    #[serial]
    async fn a_nul_bearing_content_is_binary_and_is_not_stored() {
        let dir = tempfile::tempdir().unwrap();
        let cache = open_cache(dir.path(), None);
        let server = MockServer::start().await;
        let content = "fn\u{0}one";
        let sha = blob_sha1(content.as_bytes());
        Mock::given(method("GET"))
            .and(path_matcher("/api/v1/repos/alice/notes/files/a.bin"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "path": "a.bin", "sha": sha, "content": content, "size": content.len(),
            })))
            .mount(&server)
            .await;
        let client = SynsClient::new(&server.uri()).unwrap();

        let answer = cache
            .get_or_fetch(&client, &target("alice/notes", None), "a.bin", Some(&sha))
            .await
            .unwrap();
        assert_eq!(answer, BlobContent::Binary);
        assert_eq!(std::fs::read_dir(cache.root()).unwrap().count(), 0);
    }

    // `SPEC_REVIEW_R3.md` QF-03: a refusal of class not-found or
    // internal, and a request that reached no answer at all, each answer
    // `Refused` rather than ending the run.
    #[tokio::test]
    #[serial]
    async fn the_registered_refusal_classes_answer_refused() {
        let dir = tempfile::tempdir().unwrap();
        let cache = open_cache(dir.path(), None);
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path_matcher("/api/v1/repos/alice/notes/files/gone.ts"))
            .respond_with(ResponseTemplate::new(404).set_body_json(serde_json::json!({
                "error": "not_found", "message": "File not found",
            })))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path_matcher("/api/v1/repos/alice/notes/files/boom.ts"))
            .respond_with(ResponseTemplate::new(500).set_body_json(serde_json::json!({
                "error": "internal_error",
            })))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path_matcher("/api/v1/repos/alice/notes/files/lies.ts"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "path": "lies.ts", "sha": "x", "content": 7,
            })))
            .mount(&server)
            .await;
        let client = SynsClient::new(&server.uri()).unwrap();
        let target = target("alice/notes", None);

        for path in ["gone.ts", "boom.ts", "lies.ts"] {
            let answer = cache
                .get_or_fetch(&client, &target, path, None)
                .await
                .unwrap();
            assert!(
                matches!(answer, BlobContent::Refused(_)),
                "{path} answered {answer:?}"
            );
        }

        // A request that reached no answer at all — the likeliest single
        // failure of a fan-out across the cap's paths — is `Refused` too.
        let dead = {
            let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            let port = listener.local_addr().unwrap().port();
            drop(listener);
            SynsClient::new(&format!("http://127.0.0.1:{port}")).unwrap()
        };
        let answer = cache
            .get_or_fetch(&dead, &target, "unreachable.ts", None)
            .await
            .unwrap();
        assert!(
            matches!(answer, BlobContent::Refused(_)),
            "a refused connection answered {answer:?}"
        );

        // A `401` is nobody's per-path refusal: it ends the run.
        Mock::given(method("GET"))
            .and(path_matcher("/api/v1/repos/alice/notes/files/shut.ts"))
            .respond_with(ResponseTemplate::new(401).set_body_json(serde_json::json!({
                "error": "unauthorized",
            })))
            .mount(&server)
            .await;
        let err = cache
            .get_or_fetch(&client, &target, "shut.ts", None)
            .await
            .unwrap_err();
        assert!(matches!(err, CliError::AuthRequired));
    }

    // SPEC u270 Tests: `cache_evicts_least_recently_touched_past_the_cap`.
    #[test]
    #[serial]
    fn cache_evicts_least_recently_touched_past_the_cap() {
        use std::io::Write;
        let dir = tempfile::tempdir().unwrap();
        // Three stored blobs of 100 bytes each, and a cap below their
        // total once a fourth lands.
        let cache = open_cache(dir.path(), Some("350"));
        let names = ["1".repeat(40), "2".repeat(40), "3".repeat(40)];
        let now = SystemTime::now();
        for (index, name) in names.iter().enumerate() {
            let entry = cache.root().join(name);
            let mut file = std::fs::File::create(&entry).unwrap();
            file.write_all(&[b'x'; 100]).unwrap();
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                std::fs::set_permissions(&entry, std::fs::Permissions::from_mode(0o600)).unwrap();
            }
            let touched = now - std::time::Duration::from_secs(300 - (index as u64 * 60));
            std::fs::File::options()
                .write(true)
                .open(&entry)
                .unwrap()
                .set_times(
                    std::fs::FileTimes::new()
                        .set_accessed(touched)
                        .set_modified(touched),
                )
                .unwrap();
        }
        // The fourth blob lands last, so it is the most recently touched.
        let fourth = "4".repeat(40);
        cache.store(&fourth, &[b'y'; 100]);

        cache.sweep().unwrap();

        assert!(cache.root().join(&fourth).is_file(), "the fourth stands");
        assert!(
            !cache.root().join(&names[0]).exists(),
            "the least-recently-touched is gone"
        );
        let mut total = 0u64;
        for entry in std::fs::read_dir(cache.root()).unwrap().flatten() {
            let meta = entry.metadata().unwrap();
            total += meta.len();
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                assert_eq!(meta.permissions().mode() & 0o777, 0o600);
            }
        }
        assert!(total <= cache.max_bytes(), "{total} is over the cap");
    }

    #[test]
    #[serial]
    fn a_directory_inside_the_cap_is_swept_of_nothing() {
        use std::io::Write;
        let dir = tempfile::tempdir().unwrap();
        let cache = open_cache(dir.path(), Some("4096"));
        let name = "5".repeat(40);
        std::fs::File::create(cache.root().join(&name))
            .unwrap()
            .write_all(b"small")
            .unwrap();
        cache.sweep().unwrap();
        assert!(cache.root().join(&name).is_file());
    }

    #[test]
    fn only_a_forty_character_lowercase_hex_value_is_a_blob_hash() {
        assert!(is_blob_hash(&"a".repeat(40)));
        assert!(is_blob_hash("95d09f2b10159347eece71399a7e2e907ea3df4f"));
        assert!(!is_blob_hash(&"A".repeat(40)));
        assert!(!is_blob_hash(&"a".repeat(39)));
        assert!(!is_blob_hash(&"g".repeat(40)));
        assert!(!is_blob_hash("../../etc/passwd"));
    }
}
