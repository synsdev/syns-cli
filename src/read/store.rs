//! The content store every read verb shares (roadmap 278, D-242).
//!
//! One store per OS user, server and account, under
//! `<cache root>/store/<server key>/<account key>/`: `blobs/<sha>` holds
//! the bytes a blob hash names, `hints/<owner>/<name>` the last verified
//! hash and media type per path, `size` the bytes of `blobs/`, and `lock`
//! the exclusive lock every change of `size`, of a hint file and every
//! sweep takes (D-242 §1–5, § Amended).
//!
//! A blob is checked on the way in — admitted only where the hash derived
//! from its bytes equals the `ETag` the server named — and again on the
//! way out, read and hashed before its bytes are used or a `304` is asked
//! for them (`CTRL-cache-content-addressed`). A hint is only ever sent to
//! the server as `If-None-Match`: the server resolves the path under the
//! reader's view first (D-211), so a stored blob is answered only after
//! this run's own request, or this run's tree listing, named its hash.
//!
//! A store the run cannot use — a link, another account's directory, or
//! one it cannot create or write — is skipped without a word, and the run
//! reads every file over the network (D-242 §11).

use std::collections::BTreeMap;
use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, SystemTime};

use crate::client::SynsClient;
use crate::config::Config;
use crate::errors::CliError;
use crate::push::collector::is_text;
use crate::push::hash::blob_sha1;
use crate::read::ReadTarget;

/// The cap's environment name.
pub const CACHE_MAX_BYTES_ENV: &str = "SYNS_CACHE_MAX_BYTES";

/// The cap per account where the environment name is unset or parses as
/// no positive integer: 2 GiB (`LIM-cli-cache-bytes`, D-242 §5).
pub const DEFAULT_CACHE_MAX_BYTES: u64 = 2_147_483_648;

/// How old a name in `blobs/` that is no blob hash must be before a sweep
/// removes it: younger, it is a concurrent run's admission in flight
/// (D-242 Amended §2).
pub const TEMPORARY_AGE: Duration = Duration::from_secs(3600);

#[cfg(unix)]
const DIR_MODE: u32 = 0o700;
#[cfg(unix)]
const FILE_MODE: u32 = 0o600;

/// True for a 40-character lowercase-hex blob hash and nothing else.
pub fn is_blob_hash(value: &str) -> bool {
    value.len() == 40
        && value
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

/// The cap this run holds each account's `blobs/` at.
pub fn max_bytes_from_env() -> u64 {
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

/// The server key: the server URL's host lower-cased, with `-<port>`
/// where the URL names one, every character outside `[a-z0-9.-]` made
/// `-` (D-242 §1).
pub fn server_key(server_url: &str) -> String {
    let (host, port) = match url::Url::parse(server_url) {
        Ok(url) => (
            url.host_str().unwrap_or("unknown").to_ascii_lowercase(),
            url.port(),
        ),
        Err(_) => (server_url.to_ascii_lowercase(), None),
    };
    let named = match port {
        Some(port) => format!("{host}-{port}"),
        None => host,
    };
    named
        .chars()
        .map(|c| {
            if c.is_ascii_lowercase() || c.is_ascii_digit() || c == '.' || c == '-' {
                c
            } else {
                '-'
            }
        })
        .collect()
}

/// The account key: `u-<username>` where the credential records a
/// username of `[a-z0-9-]{1,39}` once lower-cased, `t-<16 hex>` of the
/// bearer's SHA-256 where it records none, `anonymous` where no
/// credential stands (D-242 §1). It reads no request.
pub fn account_key(token: Option<&str>, username: Option<&str>) -> String {
    let Some(token) = token else {
        return "anonymous".to_string();
    };
    if let Some(name) = username.map(str::to_ascii_lowercase)
        && (1..=39).contains(&name.len())
        && name
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
    {
        return format!("u-{name}");
    }
    use sha2::Digest;
    let digest = sha2::Sha256::digest(token.as_bytes());
    let hex: String = digest.iter().take(8).map(|b| format!("{b:02x}")).collect();
    format!("t-{hex}")
}

/// The media type a hint line records for a path (D-242 Amended §2).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MediaHint {
    /// `-`: the raw answer named none.
    Unnamed,
    /// `?`: unknown; grep's, which reads no raw `Content-Type`.
    Unknown,
    /// The type as `RawFile.media_type` takes it.
    Named(String),
}

impl MediaHint {
    pub fn from_answer(media_type: Option<&str>) -> MediaHint {
        match media_type {
            Some(t) if !t.is_empty() && !t.contains(['\t', '\n', '\r']) && t != "-" && t != "?" => {
                MediaHint::Named(t.to_string())
            }
            _ => MediaHint::Unnamed,
        }
    }

    fn parse(field: &str) -> MediaHint {
        match field {
            "-" => MediaHint::Unnamed,
            "?" => MediaHint::Unknown,
            named => MediaHint::Named(named.to_string()),
        }
    }

    fn field(&self) -> &str {
        match self {
            MediaHint::Unnamed => "-",
            MediaHint::Unknown => "?",
            MediaHint::Named(named) => named,
        }
    }

    /// The type a raw answer would have carried, none where unknown.
    pub fn media_type(&self) -> Option<String> {
        match self {
            MediaHint::Named(named) => Some(named.clone()),
            _ => None,
        }
    }
}

/// A path's hint: its last verified blob hash and media type.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Hint {
    pub sha: String,
    pub media: MediaHint,
}

/// A blob the store holds for a path, read and verified before any
/// request names it.
#[derive(Debug, Clone)]
pub struct Held {
    pub sha: String,
    pub bytes: Vec<u8>,
    pub media: MediaHint,
}

/// How a run's hint for a path changes its line.
#[derive(Debug, Clone)]
enum HintUpdate {
    /// A raw `200`: the hash and the type it named.
    Set(Hint),
    /// grep: the hash, keeping the stored type where the stored hash is
    /// the same and `?` otherwise.
    Seen(String),
}

type HintMap = BTreeMap<String, Hint>;

/// The run's open store.
#[derive(Debug)]
pub struct Store {
    dir: PathBuf,
    blobs: PathBuf,
    hints: PathBuf,
    max_bytes: u64,
    admitted: AtomicBool,
    /// A stored blob this run found short or altered and removed: the
    /// length `size` counted for it is unknown, so `finish` recomputes
    /// `size` from `blobs/` (VERIFY.md F-1, REVIEW-delta-2 finding 2).
    resync: AtomicBool,
    /// The walks of `blobs/` this store made (tests).
    walks: std::sync::atomic::AtomicUsize,
    /// The repository whose hint file is loaded, and its lines.
    loaded: Mutex<Option<(String, HintMap)>>,
    /// The run's updates, by repository and path, merged at `finish`.
    updates: Mutex<BTreeMap<(String, String), HintUpdate>>,
}

/// Whether `path` stands as a link or is owned by another account.
fn foreign_or_link(meta: &std::fs::Metadata) -> bool {
    if meta.file_type().is_symlink() {
        return true;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        // SAFETY: `geteuid` reads the process's effective uid and cannot fail.
        if meta.uid() != unsafe { libc::geteuid() } {
            return true;
        }
    }
    false
}

/// A directory of the store, made at `0700` where absent: usable only
/// where it stands as a directory, not a link, owned by this account.
fn confined_dir(path: &Path) -> bool {
    match std::fs::symlink_metadata(path) {
        Ok(_) => {}
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            let mut builder = std::fs::DirBuilder::new();
            #[cfg(unix)]
            {
                use std::os::unix::fs::DirBuilderExt;
                builder.mode(DIR_MODE);
            }
            match builder.create(path) {
                Ok(()) => {}
                Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => {}
                Err(_) => return false,
            }
        }
        Err(_) => return false,
    }
    let Ok(meta) = std::fs::symlink_metadata(path) else {
        return false;
    };
    if !meta.is_dir() || foreign_or_link(&meta) {
        return false;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if meta.permissions().mode() & 0o777 != DIR_MODE
            && std::fs::set_permissions(path, std::fs::Permissions::from_mode(DIR_MODE)).is_err()
        {
            return false;
        }
    }
    true
}

/// A file of the store opened for writing, made at `0600` where absent.
fn store_file(path: &Path, truncate: bool) -> std::io::Result<File> {
    let mut options = OpenOptions::new();
    options
        .read(true)
        .write(true)
        .create(true)
        .truncate(truncate);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(FILE_MODE);
        options.custom_flags(libc::O_NOFOLLOW);
    }
    options.open(path)
}

/// Removes a link, a file or a whole directory standing at `path`, a link
/// as a link, never walking its target.
fn remove_standing(path: &Path) {
    let Ok(meta) = std::fs::symlink_metadata(path) else {
        return;
    };
    if meta.is_dir() {
        let _ = std::fs::remove_dir_all(path);
    } else if std::fs::remove_file(path).is_err() {
        // A directory link on Windows is removed as a directory.
        let _ = std::fs::remove_dir(path);
    }
}

/// A valid repository side for a hint file's address.
fn is_name(side: &str) -> bool {
    !side.is_empty()
        && side != "."
        && side != ".."
        && side
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'.' || b == b'_' || b == b'-')
}

fn parse_hints(text: &str) -> HintMap {
    let mut map = HintMap::new();
    for line in text.lines() {
        let mut fields = line.splitn(3, '\t');
        let (Some(sha), Some(media), Some(path)) = (fields.next(), fields.next(), fields.next())
        else {
            continue;
        };
        if !is_blob_hash(sha) || path.is_empty() {
            continue;
        }
        map.insert(
            path.to_string(),
            Hint {
                sha: sha.to_string(),
                media: MediaHint::parse(media),
            },
        );
    }
    map
}

fn touch(path: &Path) {
    let now = SystemTime::now();
    if let Ok(file) = OpenOptions::new().write(true).open(path) {
        let _ = file.set_times(
            std::fs::FileTimes::new()
                .set_accessed(now)
                .set_modified(now),
        );
    }
}

impl Store {
    /// Opens the run's store for the account `token` signs in as, `None`
    /// where it cannot be used (D-242 §11). Removes the old grep cache
    /// `<cache root>/blobs/` where it stands (D-242 §9).
    pub fn open(config: &Config, token: Option<&str>) -> Option<Store> {
        let root = config.cache_dir();
        remove_standing(&root.join("blobs"));
        let username = crate::auth::token::TokenStore::new(config.credentials_path())
            .read_username()
            .ok()
            .flatten();
        let account = account_key(token, username.as_deref());
        Store::open_at(
            root,
            &server_key(config.server_url()),
            &account,
            max_bytes_from_env(),
        )
    }

    /// The store under `root` for one server and account key, every
    /// directory confined as it is reached.
    pub fn open_at(root: &Path, server: &str, account: &str, max_bytes: u64) -> Option<Store> {
        if std::fs::create_dir_all(root).is_err() {
            return None;
        }
        let store = root.join("store");
        let dir = store.join(server).join(account);
        let blobs = dir.join("blobs");
        let hints = dir.join("hints");
        for step in [&store, &store.join(server), &dir, &blobs, &hints] {
            if !confined_dir(step) {
                return None;
            }
        }
        // The lock file is the store's write probe.
        store_file(&dir.join("lock"), false).ok()?;
        Some(Store {
            dir,
            blobs,
            hints,
            max_bytes,
            admitted: AtomicBool::new(false),
            resync: AtomicBool::new(false),
            walks: std::sync::atomic::AtomicUsize::new(0),
            loaded: Mutex::new(None),
            updates: Mutex::new(BTreeMap::new()),
        })
    }

    /// The account directory.
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    pub fn max_bytes(&self) -> u64 {
        self.max_bytes
    }

    /// The exclusive lock on `lock`, held until the answer is dropped.
    fn lock(&self) -> Option<File> {
        let file = store_file(&self.dir.join("lock"), false).ok()?;
        file.lock().ok()?;
        Some(file)
    }

    /// `size` as it stands, or the bytes of `blobs/` walked where it is
    /// missing or unreadable. Called under the lock.
    fn read_size(&self) -> u64 {
        match std::fs::read_to_string(self.dir.join("size"))
            .ok()
            .and_then(|s| s.trim().parse::<u64>().ok())
        {
            Some(size) => size,
            None => self.walk().0,
        }
    }

    fn write_size(&self, size: u64) {
        if let Ok(mut file) = store_file(&self.dir.join("size"), true) {
            let _ = file.write_all(size.to_string().as_bytes());
        }
    }

    /// The blobs of `blobs/` (their total, and each one's mtime, length
    /// and path) and every other name there with its mtime.
    #[allow(clippy::type_complexity)]
    fn walk(
        &self,
    ) -> (
        u64,
        Vec<(SystemTime, u64, PathBuf)>,
        Vec<(SystemTime, PathBuf)>,
    ) {
        self.walks.fetch_add(1, Ordering::Relaxed);
        let mut total = 0;
        let mut entries = Vec::new();
        let mut others = Vec::new();
        let Ok(dir) = std::fs::read_dir(&self.blobs) else {
            return (0, entries, others);
        };
        for entry in dir.flatten() {
            let Ok(meta) = entry.metadata() else { continue };
            let touched = meta.modified().unwrap_or(SystemTime::UNIX_EPOCH);
            let name = entry.file_name();
            if meta.is_file() && name.to_str().is_some_and(is_blob_hash) {
                total += meta.len();
                entries.push((touched, meta.len(), entry.path()));
            } else {
                others.push((touched, entry.path()));
            }
        }
        (total, entries, others)
    }

    /// The bytes stored under `sha`, read and hashed: `None` where none
    /// stands, and where they are not the bytes the hash names, the blob
    /// removed (D-242 §4). A blob answered is touched, the sweep's
    /// least-recently-used order.
    pub fn verified(&self, sha: &str) -> Option<Vec<u8>> {
        if !is_blob_hash(sha) {
            return None;
        }
        let path = self.blobs.join(sha);
        let bytes = std::fs::read(&path).ok()?;
        if blob_sha1(&bytes) != sha {
            // Present but short or altered: removed, and `size` recounted
            // once at `finish`.
            self.remove_blob(&path);
            self.resync.store(true, Ordering::Relaxed);
            return None;
        }
        touch(&path);
        Some(bytes)
    }

    /// Removes a blob that failed its check, subtracting its length only
    /// where this run's own removal succeeded (D-242 Amended §5).
    fn remove_blob(&self, path: &Path) {
        let Some(_lock) = self.lock() else { return };
        let Ok(meta) = std::fs::symlink_metadata(path) else {
            return;
        };
        // Read before the removal: a missing index is rebuilt by a walk
        // that must still see the blob it then subtracts.
        let size = self.read_size();
        if std::fs::remove_file(path).is_ok() {
            self.write_size(size.saturating_sub(meta.len()));
        }
    }

    /// Admits `bytes` under `sha`, only where the hash derived from them
    /// is `sha` (D-242 Amended §5): written to a temporary in `blobs/`,
    /// then under the lock renamed in without replacing and counted once.
    /// A failure stores nothing and is not raised.
    pub fn admit(&self, sha: &str, bytes: &[u8]) {
        if !is_blob_hash(sha) || blob_sha1(bytes) != sha {
            return;
        }
        let dest = self.blobs.join(sha);
        if dest.is_file() {
            return;
        }
        let Ok(mut tmp) = tempfile::Builder::new()
            .prefix(".tmp-")
            .tempfile_in(&self.blobs)
        else {
            return;
        };
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            if tmp
                .as_file()
                .set_permissions(std::fs::Permissions::from_mode(FILE_MODE))
                .is_err()
            {
                return;
            }
        }
        if tmp.write_all(bytes).and_then(|()| tmp.flush()).is_err() {
            return;
        }
        let Some(_lock) = self.lock() else { return };
        if std::fs::symlink_metadata(&dest).is_ok() {
            return;
        }
        // Read before the rename: a missing index is rebuilt by a walk
        // that must not yet see the blob it then adds.
        let size = self.read_size();
        if tmp.persist_noclobber(&dest).is_ok() {
            self.write_size(size.saturating_add(bytes.len() as u64));
            self.admitted.store(true, Ordering::Relaxed);
        }
    }

    /// The hint file of `repo_id`, where its sides can address one.
    fn hint_file(&self, repo_id: &str) -> Option<(PathBuf, PathBuf)> {
        let (owner, name) = repo_id.split_once('/')?;
        if !is_name(owner) || !is_name(name) {
            return None;
        }
        let dir = self.hints.join(owner);
        Some((dir.join(name), dir))
    }

    /// The hint for `path` of `repo_id`, as the path was addressed.
    pub fn hint(&self, repo_id: &str, path: &str) -> Option<Hint> {
        let mut loaded = self.loaded.lock().unwrap_or_else(|e| e.into_inner());
        if loaded.as_ref().is_none_or(|(repo, _)| repo != repo_id) {
            let map = self
                .hint_file(repo_id)
                .and_then(|(file, _)| std::fs::read_to_string(file).ok())
                .map(|text| parse_hints(&text))
                .unwrap_or_default();
            *loaded = Some((repo_id.to_string(), map));
        }
        loaded.as_ref()?.1.get(path).cloned()
    }

    /// The verified blob the hint for `path` names, with its type.
    pub fn held(&self, repo_id: &str, path: &str) -> Option<Held> {
        let hint = self.hint(repo_id, path)?;
        // A blob simply missing is most often a sweep's eviction, which
        // `size` already left out: no walk. One removed by hand stays
        // counted until the next sweep (§ Known issues, `size` drift).
        let bytes = self.verified(&hint.sha)?;
        Some(Held {
            sha: hint.sha,
            bytes,
            media: hint.media,
        })
    }

    fn note(&self, repo_id: &str, path: &str, update: HintUpdate) {
        if path.is_empty() || path.contains(['\t', '\n', '\r']) {
            return;
        }
        self.updates
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert((repo_id.to_string(), path.to_string()), update);
    }

    /// Records a raw `200`'s verified hash and type for `path`.
    pub fn note_answer(&self, repo_id: &str, path: &str, sha: &str, media: MediaHint) {
        self.note(
            repo_id,
            path,
            HintUpdate::Set(Hint {
                sha: sha.to_string(),
                media,
            }),
        );
    }

    /// Records the hash grep read `path` at, its type unknown.
    pub fn note_seen(&self, repo_id: &str, path: &str, sha: &str) {
        self.note(repo_id, path, HintUpdate::Seen(sha.to_string()));
    }

    /// The run's end, after its last fetch: every hint update merged into
    /// its file under the lock, and one sweep where `size` stands past
    /// the cap (D-242 §5).
    pub fn finish(&self) {
        let updates = std::mem::take(&mut *self.updates.lock().unwrap_or_else(|e| e.into_inner()));
        let over = || {
            std::fs::read_to_string(self.dir.join("size"))
                .ok()
                .and_then(|s| s.trim().parse::<u64>().ok())
                .is_some_and(|size| size > self.max_bytes)
        };
        let admitted = self.admitted.load(Ordering::Relaxed);
        let resync = self.resync.load(Ordering::Relaxed);
        if updates.is_empty() && !admitted && !resync && !over() {
            return;
        }
        let Some(_lock) = self.lock() else { return };
        self.merge(updates);
        if resync {
            // The true total: a blob truncated or removed by hand was
            // counted at a length no longer on disk (VERIFY.md F-1).
            self.write_size(self.walk().0);
        }
        if self.read_size() > self.max_bytes {
            self.sweep_locked();
        }
    }

    fn merge(&self, updates: BTreeMap<(String, String), HintUpdate>) {
        let mut by_repo: BTreeMap<String, Vec<(String, HintUpdate)>> = BTreeMap::new();
        for ((repo, path), update) in updates {
            by_repo.entry(repo).or_default().push((path, update));
        }
        for (repo, updates) in by_repo {
            let Some((file, dir)) = self.hint_file(&repo) else {
                continue;
            };
            if !confined_dir(&dir) {
                continue;
            }
            let mut map = std::fs::read_to_string(&file)
                .map(|text| parse_hints(&text))
                .unwrap_or_default();
            let mut changed = false;
            for (path, update) in updates {
                let next = match update {
                    HintUpdate::Set(hint) => hint,
                    HintUpdate::Seen(sha) => match map.get(&path) {
                        Some(stored) if stored.sha == sha => continue,
                        _ => Hint {
                            sha,
                            media: MediaHint::Unknown,
                        },
                    },
                };
                if map.get(&path) != Some(&next) {
                    map.insert(path, next);
                    changed = true;
                }
            }
            if !changed {
                continue;
            }
            let mut text = String::new();
            for (path, hint) in &map {
                text.push_str(&format!("{}\t{}\t{path}\n", hint.sha, hint.media.field()));
            }
            let Ok(mut tmp) = tempfile::Builder::new().prefix(".tmp-").tempfile_in(&dir) else {
                continue;
            };
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let _ = tmp
                    .as_file()
                    .set_permissions(std::fs::Permissions::from_mode(FILE_MODE));
            }
            if tmp.write_all(text.as_bytes()).is_ok() {
                let _ = tmp.persist(&file);
            }
        }
    }

    /// The sweep, under the lock: `blobs/` walked and its true total
    /// recomputed, names that are no blob hash removed where older than
    /// `TEMPORARY_AGE`, and the least recently touched blobs evicted to at
    /// most 90 % of the cap; `size` rewritten (D-242 §5, Amended §2).
    fn sweep_locked(&self) {
        let (mut total, mut entries, others) = self.walk();
        let now = SystemTime::now();
        for (touched, path) in others {
            let old = now
                .duration_since(touched)
                .is_ok_and(|age| age > TEMPORARY_AGE);
            if old && std::fs::symlink_metadata(&path).is_ok_and(|m| !m.is_dir()) {
                let _ = std::fs::remove_file(&path);
            }
        }
        let low_water = self.max_bytes / 10 * 9 + (self.max_bytes % 10) * 9 / 10;
        if total > low_water {
            entries.sort_by_key(|(touched, _, _)| *touched);
            for (_, len, path) in entries {
                if total <= low_water {
                    break;
                }
                if std::fs::remove_file(&path).is_ok() {
                    total -= len;
                }
            }
        }
        self.write_size(total);
    }
}

/// What one path's content came back as for grep.
///
/// `Binary` is the classification a content takes on a NUL byte or
/// invalid UTF-8. `Refused` carries that refusal's one string.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BlobContent {
    Text(String),
    Binary,
    Refused(String),
}

/// Answers one path's content for grep at the target's pinned reference
/// (D-242 §10): from the blob the tree listing's hash names, checked on
/// the way out, and otherwise through `EP-file-read`, a text answer whose
/// served hash its bytes derive admitted. A hint is written for every
/// path read.
pub async fn get_or_fetch(
    store: Option<&Store>,
    client: &SynsClient,
    target: &ReadTarget,
    path: &str,
    sha: Option<&str>,
) -> Result<BlobContent, CliError> {
    // 1 — a stored blob the tree listing named, verified.
    if let (Some(store), Some(hash)) = (store, sha.filter(|s| is_blob_hash(s)))
        && let Some(bytes) = store.verified(hash)
    {
        store.note_seen(&target.repo_id, path, hash);
        return Ok(match String::from_utf8(bytes) {
            Ok(text) if is_text(text.as_bytes()) => BlobContent::Text(text),
            _ => BlobContent::Binary,
        });
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

    // 3 — `content: null`, and one `is_text` refuses, is not text.
    let content = match response.content {
        Some(content) if is_text(content.as_bytes()) => content,
        _ => return Ok(BlobContent::Binary),
    };

    // 4 — stored only where the served hash is the bytes' own.
    if let Some(store) = store
        && is_blob_hash(&response.sha)
        && blob_sha1(content.as_bytes()) == response.sha
    {
        store.admit(&response.sha, content.as_bytes());
        store.note_seen(&target.repo_id, path, &response.sha);
    }
    Ok(BlobContent::Text(content))
}

/// The refusal classes one grep path answers `Refused` for, the fan-out
/// carrying on past each: not found or internal, a request that reached
/// no answer at all, and an answer whose `content` is no decodable
/// string. Every other refusal ends the run at its own exit code.
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

// ---- syns logout --purge (D-242 §8) ---------------------------------------

/// Removes whatever stands at `top` — a link as a link, a file, or a
/// directory walked without following any link in it — answering the
/// bytes of the files removed.
fn remove_tree(top: &Path) -> std::io::Result<u64> {
    let meta = match std::fs::symlink_metadata(top) {
        Ok(meta) => meta,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(0),
        Err(err) => return Err(err),
    };
    if meta.file_type().is_symlink() {
        if std::fs::remove_file(top).is_err() {
            std::fs::remove_dir(top)?;
        }
        return Ok(0);
    }
    if !meta.is_dir() {
        std::fs::remove_file(top)?;
        return Ok(meta.len());
    }
    let mut removed = 0;
    for entry in std::fs::read_dir(top)? {
        removed += remove_tree(&entry?.path())?;
    }
    std::fs::remove_dir(top)?;
    Ok(removed)
}

/// Removes `store/` and the old `blobs/` under each root, a fallback root
/// only where it stands as a directory owned by this account and not a
/// link, never creating one (D-242 §8, Amended §8). Answers the bytes
/// removed; a removal that fails ends the purge.
pub fn purge(roots: &[(PathBuf, bool)]) -> Result<u64, CliError> {
    let mut removed = 0;
    for (root, fallback) in roots {
        if *fallback {
            match std::fs::symlink_metadata(root) {
                Ok(meta) if meta.is_dir() && !foreign_or_link(&meta) => {}
                _ => continue,
            }
        }
        for name in ["store", "blobs"] {
            let top = root.join(name);
            removed += remove_tree(&top).map_err(|err| CliError::Io {
                message: format!(
                    "could not remove the content store at {}: {err}",
                    top.display()
                ),
            })?;
        }
    }
    Ok(removed)
}

/// A byte count as the purge line names it: `B` under 1 KiB, and KiB,
/// MiB or GiB with one decimal above.
pub fn human_bytes(bytes: u64) -> String {
    const UNITS: [&str; 3] = ["KiB", "MiB", "GiB"];
    if bytes < 1024 {
        return format!("{bytes} B");
    }
    let mut value = bytes as f64 / 1024.0;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    format!("{value:.1} {}", UNITS[unit])
}

#[cfg(test)]
mod tests {
    use super::*;
    use serial_test::serial;

    fn open(root: &Path, cap: u64) -> Store {
        Store::open_at(root, "syns.dev", "u-alice", cap).expect("a usable store")
    }

    // Row 31: the cap.
    #[test]
    #[serial]
    fn the_cap_defaults_to_two_gib_and_reads_only_a_positive_integer() {
        let read = |value: Option<&str>| {
            match value {
                Some(v) => unsafe { std::env::set_var(CACHE_MAX_BYTES_ENV, v) },
                None => unsafe { std::env::remove_var(CACHE_MAX_BYTES_ENV) },
            }
            let cap = max_bytes_from_env();
            unsafe { std::env::remove_var(CACHE_MAX_BYTES_ENV) };
            cap
        };
        assert_eq!(read(None), 2_147_483_648);
        assert_eq!(read(Some("4096")), 4096);
        for unset in ["0", "-1", "many", ""] {
            assert_eq!(read(Some(unset)), 2_147_483_648, "{unset:?}");
        }
    }

    // D-242 §1: the keys.
    #[test]
    fn the_server_and_account_keys() {
        assert_eq!(server_key("https://syns.dev"), "syns.dev");
        assert_eq!(server_key("https://Staging.Syns.dev/"), "staging.syns.dev");
        assert_eq!(server_key("http://127.0.0.1:18278"), "127.0.0.1-18278");
        assert_eq!(server_key("http://[::1]:8080"), "---1--8080");
        assert_eq!(account_key(None, Some("alice")), "anonymous");
        assert_eq!(account_key(Some("t"), Some("Alice")), "u-alice");
        assert_eq!(account_key(Some("t"), Some("a-1")), "u-a-1");
        let digest = account_key(Some("test-token"), None);
        assert!(digest.starts_with("t-") && digest.len() == 18, "{digest}");
        assert_eq!(account_key(Some("test-token"), Some("../x")), digest);
        assert_eq!(
            account_key(Some("test-token"), Some(&"a".repeat(40))),
            digest
        );
    }

    #[test]
    fn only_a_forty_character_lowercase_hex_value_is_a_blob_hash() {
        assert!(is_blob_hash(&"a".repeat(40)));
        assert!(is_blob_hash("95d09f2b10159347eece71399a7e2e907ea3df4f"));
        assert!(!is_blob_hash(&"A".repeat(40)));
        assert!(!is_blob_hash(&"a".repeat(39)));
        assert!(!is_blob_hash("../../etc/passwd"));
    }

    // Row 22 and Admission: only bytes whose hash is the one named.
    #[test]
    fn admission_takes_only_bytes_their_hash_names() {
        let root = tempfile::tempdir().unwrap();
        let store = open(root.path(), 1 << 20);
        store.admit(&blob_sha1(b"one"), b"one");
        store.admit(&"0".repeat(40), b"two");
        assert_eq!(store.verified(&blob_sha1(b"one")).unwrap(), b"one");
        assert!(store.verified(&"0".repeat(40)).is_none());
        assert_eq!(
            std::fs::read_to_string(store.dir().join("size")).unwrap(),
            "3"
        );
    }

    // Row 30, concurrent admission (M1): two admitting the same hash at
    // once count it once, and a removal finding the blob gone subtracts
    // nothing.
    #[test]
    fn concurrent_admissions_of_one_hash_count_it_once() {
        let root = tempfile::tempdir().unwrap();
        let bytes = vec![b'z'; 1000];
        let sha = blob_sha1(&bytes);
        for _round in 0..20 {
            let _ = std::fs::remove_dir_all(root.path().join("store"));
            let stores: Vec<Store> = (0..2).map(|_| open(root.path(), 1 << 20)).collect();
            std::thread::scope(|scope| {
                for store in &stores {
                    let (bytes, sha) = (&bytes, &sha);
                    scope.spawn(move || store.admit(sha, bytes));
                }
            });
            assert_eq!(stores[0].read_size(), 1000);
            assert_eq!(std::fs::read_dir(&stores[0].blobs).unwrap().count(), 1);
        }
        let store = open(root.path(), 1 << 20);
        let path = store.blobs.join(&sha);
        std::fs::remove_file(&path).unwrap();
        store.remove_blob(&path);
        assert_eq!(store.read_size(), 1000, "nothing subtracted");
    }

    // VERIFY.md F-1, REVIEW-delta-2 finding 2: a blob present but short or
    // altered is recounted once, leaving `size` the true total; a blob
    // removed by hand, or evicted by a sweep, costs no walk — the removed
    // one stays counted until the next sweep (`size` drift), the evicted
    // one was already left out.
    #[test]
    fn only_a_blob_failing_its_check_recounts_size() {
        let root = tempfile::tempdir().unwrap();
        let (one, two) = (vec![b'1'; 639], vec![b'2'; 300]);
        let (sha_one, sha_two) = (blob_sha1(&one), blob_sha1(&two));
        for damage in ["truncated", "altered", "removed"] {
            let _ = std::fs::remove_dir_all(root.path().join("store"));
            let store = open(root.path(), 1 << 20);
            store.admit(&sha_one, &one);
            store.admit(&sha_two, &two);
            store.note_answer("alice/notes", "one.md", &sha_one, MediaHint::Unnamed);
            store.finish();
            let blob = store.blobs.join(&sha_one);
            match damage {
                "truncated" => std::fs::write(&blob, &one[..100]).unwrap(),
                "altered" => std::fs::write(&blob, vec![b'x'; 639]).unwrap(),
                _ => std::fs::remove_file(&blob).unwrap(),
            }
            let store = open(root.path(), 1 << 20);
            assert!(store.held("alice/notes", "one.md").is_none(), "{damage}");
            // The path is read again over the network and admitted.
            store.admit(&sha_one, &one);
            store.finish();
            let walks = store.walks.load(Ordering::Relaxed);
            if damage == "removed" {
                assert_eq!(walks, 0, "{damage}: no walk");
                assert_eq!(store.read_size(), 939 + 639, "{damage}: counted twice");
            } else {
                assert_eq!(walks, 1, "{damage}: one walk");
                assert_eq!(store.read_size(), 939, "{damage}");
            }
        }

        // Evicted by a sweep, then read: no walk, and `size` already right.
        let _ = std::fs::remove_dir_all(root.path().join("store"));
        let store = open(root.path(), 800);
        store.admit(&sha_one, &one);
        store.note_answer("alice/notes", "one.md", &sha_one, MediaHint::Unnamed);
        let blob = store.blobs.join(&sha_one);
        let old = SystemTime::now() - Duration::from_secs(60);
        File::options()
            .write(true)
            .open(&blob)
            .unwrap()
            .set_times(std::fs::FileTimes::new().set_modified(old))
            .unwrap();
        store.admit(&sha_two, &two);
        store.finish();
        assert!(!blob.exists(), "the sweep evicted one.md's blob");
        assert_eq!(store.read_size(), 300);
        // The next run, under a cap the re-admission stays within.
        let store = open(root.path(), 1 << 20);
        assert!(store.held("alice/notes", "one.md").is_none());
        store.admit(&sha_one, &one);
        store.finish();
        assert_eq!(store.walks.load(Ordering::Relaxed), 0, "no walk");
        assert_eq!(store.read_size(), 939);
    }

    // D-242 §4: a blob failing its check is removed and its length taken
    // off the index.
    #[test]
    fn a_tampered_blob_is_removed_on_the_way_out() {
        let root = tempfile::tempdir().unwrap();
        let store = open(root.path(), 1 << 20);
        let sha = blob_sha1(b"bytes");
        store.admit(&sha, b"bytes");
        std::fs::write(store.blobs.join(&sha), b"bytez").unwrap();
        assert!(store.verified(&sha).is_none());
        assert!(!store.blobs.join(&sha).exists());
        assert_eq!(store.read_size(), 0);
    }

    // Row 33: a store directory owned by another account, or a link, is
    // not used.
    #[cfg(unix)]
    #[test]
    fn a_foreign_or_linked_directory_is_unusable() {
        if unsafe { libc::geteuid() } != 0 {
            // `/` belongs to root: another account than this one.
            assert!(!confined_dir(Path::new("/")));
        }
        let root = tempfile::tempdir().unwrap();
        let target = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(root.path().join("store/syns.dev")).unwrap();
        std::os::unix::fs::symlink(target.path(), root.path().join("store/syns.dev/u-alice"))
            .unwrap();
        assert!(Store::open_at(root.path(), "syns.dev", "u-alice", 10).is_none());
        assert_eq!(std::fs::read_dir(target.path()).unwrap().count(), 0);
    }

    // Row 38a: a fallback root owned by another account, or a link, is not
    // walked; a configured one is taken as it stands.
    #[cfg(unix)]
    #[test]
    fn purge_skips_a_foreign_or_linked_fallback_root() {
        let victim = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(victim.path().join("store/x")).unwrap();
        std::fs::write(victim.path().join("store/x/f"), b"keep").unwrap();
        let temp = tempfile::tempdir().unwrap();
        let link = temp.path().join("syns-1");
        std::os::unix::fs::symlink(victim.path(), &link).unwrap();
        assert_eq!(purge(&[(link, true)]).unwrap(), 0);
        assert!(victim.path().join("store/x/f").is_file());
        if unsafe { libc::geteuid() } != 0 {
            // `/` stands as a directory another account owns.
            assert_eq!(purge(&[(PathBuf::from("/"), true)]).unwrap(), 0);
        }
        let missing = temp.path().join("absent");
        assert_eq!(purge(&[(missing.clone(), true)]).unwrap(), 0);
        assert!(!missing.exists(), "purge creates no root");
        assert_eq!(purge(&[(victim.path().to_path_buf(), false)]).unwrap(), 4);
        assert!(!victim.path().join("store").exists());
    }

    #[test]
    fn the_purge_line_names_its_size() {
        assert_eq!(human_bytes(0), "0 B");
        assert_eq!(human_bytes(1023), "1023 B");
        assert_eq!(human_bytes(1536), "1.5 KiB");
        assert_eq!(human_bytes(5 * 1024 * 1024), "5.0 MiB");
        assert_eq!(human_bytes(3 * 1024 * 1024 * 1024), "3.0 GiB");
    }

    // D-242 Amended §2: hint lines, the type a 200 names and grep's `?`.
    #[test]
    fn hints_merge_a_200s_type_and_keep_it_for_grep() {
        let root = tempfile::tempdir().unwrap();
        let sha = blob_sha1(b"x");
        let store = open(root.path(), 1 << 20);
        store.note_answer(
            "alice/notes",
            "logo.png",
            &sha,
            MediaHint::Named("image/png".into()),
        );
        store.note_seen("alice/notes", "other.png", &sha);
        store.note_answer("alice/notes", "tab\there", &sha, MediaHint::Unnamed);
        store.finish();
        let file = store.hints.join("alice").join("notes");
        assert_eq!(
            std::fs::read_to_string(&file).unwrap(),
            format!("{sha}\timage/png\tlogo.png\n{sha}\t?\tother.png\n")
        );
        let store = open(root.path(), 1 << 20);
        store.note_seen("alice/notes", "logo.png", &sha);
        store.finish();
        assert!(
            std::fs::read_to_string(&file)
                .unwrap()
                .contains("\timage/png\tlogo.png\n")
        );
        assert_eq!(
            store.hint("alice/notes", "logo.png"),
            Some(Hint {
                sha: sha.clone(),
                media: MediaHint::Named("image/png".into())
            })
        );
        assert!(store.hint_file("../x").is_none());
    }
}
