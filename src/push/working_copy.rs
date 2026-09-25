//! The per-working-copy state a convergence keeps outside the folder
//! (SPEC u256 § Contract Surface, `WorkingCopy`).
//!
//! One state directory per repository and canonical root, under the
//! cache root, holding the state lock, the recorded base, a pending
//! resolution, the publication outbox and the two private snapshots.
//! Every file under it is written through `write_atomic`: beside its
//! target, flushed, renamed over the target and its directory flushed,
//! so a killed process or a lost machine leaves the version before a
//! write or the one after it, never a torn one.

use std::collections::{BTreeMap, HashMap};
use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

use crate::errors::CliError;
use crate::push::hash::blob_sha1;
use crate::push::manifest::Manifest;
use crate::push::reconcile::CollisionKind;

const LOCK_FILE: &str = "state.lock";
const BASE_FILE: &str = "base.json";
const RESOLUTION_FILE: &str = "resolution.json";
const OUTBOX_FILE: &str = "outbox.json";
const LOCAL_SNAPSHOT_FILE: &str = "local-snapshot.json";
const REMOTE_SNAPSHOT_FILE: &str = "remote-snapshot.json";
const STAT_RECORD_FILE: &str = "stat-record.json";

/// What a collection saw of each file it read, so the next collection
/// spares a file its read where nothing about it moved (SPEC u280
/// `StatRecord`, `NR-01`): kept per working copy as `stat-record.json`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct StatRecord {
    pub entries: BTreeMap<String, StatEntry>,
    /// The modification time of the probe the writing collection made in
    /// the working copy's root after its last read, none where no probe
    /// could be made. An entry whose times do not both stand earlier than
    /// it is too recent to trust.
    pub stamp: Option<(i64, i64)>,
}

/// One file as a collection read it, symbolic links followed, each time
/// as seconds and nanoseconds.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StatEntry {
    pub size: u64,
    pub mtime: (i64, i64),
    pub ctime: (i64, i64),
    pub inode: u64,
    pub sha: String,
}

/// The size, modification time, change time and inode `meta` answers.
#[cfg(unix)]
fn stat_of(meta: &std::fs::Metadata) -> (u64, (i64, i64), (i64, i64), u64) {
    use std::os::unix::fs::MetadataExt;
    (
        meta.len(),
        (meta.mtime(), meta.mtime_nsec()),
        (meta.ctime(), meta.ctime_nsec()),
        meta.ino(),
    )
}

impl StatRecord {
    /// The record kept in `state_dir`, empty with no stamp where its file
    /// is absent or unreadable.
    pub fn load(state_dir: &Path) -> StatRecord {
        std::fs::read(state_dir.join(STAT_RECORD_FILE))
            .ok()
            .and_then(|bytes| serde_json::from_slice(&bytes).ok())
            .unwrap_or_default()
    }

    /// Write the record into `state_dir`.
    pub fn save(&self, state_dir: &Path) -> Result<(), CliError> {
        write_json(&state_dir.join(STAT_RECORD_FILE), self)
    }

    /// The hash the entry for `path` answers where it trusts `meta`: the
    /// size, both times and the inode all equal, and both times earlier
    /// than the stamp.
    #[cfg(unix)]
    pub fn trusted(&self, path: &str, meta: &std::fs::Metadata) -> Option<String> {
        let stamp = self.stamp?;
        let entry = self.entries.get(path)?;
        let (size, mtime, ctime, inode) = stat_of(meta);
        (entry.size == size
            && entry.mtime == mtime
            && entry.ctime == ctime
            && entry.inode == inode
            && mtime < stamp
            && ctime < stamp)
            .then(|| entry.sha.clone())
    }

    /// A record kept on no platform but unix trusts nothing.
    #[cfg(not(unix))]
    pub fn trusted(&self, _path: &str, _meta: &std::fs::Metadata) -> Option<String> {
        None
    }

    /// Record what a read of `path` hashing to `sha` saw, where the file
    /// answered the same size, times and inode before the read and after
    /// it; drop its entry otherwise.
    #[cfg(unix)]
    pub fn observe(
        &mut self,
        path: &str,
        before: &std::fs::Metadata,
        after: &std::fs::Metadata,
        sha: &str,
    ) {
        let seen = stat_of(before);
        if seen != stat_of(after) {
            self.entries.remove(path);
            return;
        }
        let (size, mtime, ctime, inode) = seen;
        self.entries.insert(
            path.to_string(),
            StatEntry {
                size,
                mtime,
                ctime,
                inode,
                sha: sha.to_string(),
            },
        );
    }

    #[cfg(not(unix))]
    pub fn observe(
        &mut self,
        path: &str,
        _before: &std::fs::Metadata,
        _after: &std::fs::Metadata,
        _sha: &str,
    ) {
        self.entries.remove(path);
    }

    /// Whether an entry stands at or past the stamp, or no stamp stands —
    /// an entry the next collection could not trust.
    pub fn holds_untrusted(&self) -> bool {
        match self.stamp {
            None => !self.entries.is_empty(),
            Some(stamp) => self
                .entries
                .values()
                .any(|entry| entry.mtime >= stamp || entry.ctime >= stamp),
        }
    }

    /// Take a fresh stamp: the modification time of a probe created in
    /// `root` under a name a collection sweeps as a partial write, and
    /// removed at once. None where the probe could not be made.
    #[cfg(unix)]
    pub fn restamp(&mut self, root: &Path) {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or_default();
        let minted = &blob_sha1(format!("{nanos}-{}", std::process::id()).as_bytes())[..16];
        let probe = root.join(format!(".syns-partial-{minted}"));
        self.stamp = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&probe)
            .and_then(|file| file.metadata())
            .ok()
            .map(|meta| {
                let (_, mtime, _, _) = stat_of(&meta);
                mtime
            });
        let _ = std::fs::remove_file(&probe);
    }
}

/// One folder a repository is worked on in, and where its state lives.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkingCopy {
    pub owner: String,
    pub name: String,
    pub root: PathBuf,
    pub state_dir: PathBuf,
}

/// A reconciliation handed to a reviewer: what both sides changed, and
/// what the folder held when it was last continued.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Resolution {
    pub recovery_id: String,
    pub base_commit: Option<String>,
    pub head_commit: String,
    pub round: u32,
    pub local_paths: Vec<String>,
    pub remote_paths: Vec<String>,
    pub collisions: Vec<(String, CollisionKind)>,
    pub combined_paths: Vec<String>,
    pub reviewed_tree: Option<BTreeMap<String, String>>,
    /// Each folder path the candidate's preparation owes a write or a
    /// removal, mapped to the hash it is to hold — none for a removal —
    /// recorded before the first folder write and cleared after the last,
    /// so a resolution carrying it is one a killed or failed run left
    /// half-written.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pending_writes: Option<BTreeMap<String, Option<String>>>,
}

/// A publication that may have landed without its commit recorded as
/// the base: the parent it was sent against and the folder it sent.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Outbox {
    pub parent_commit: Option<String>,
    pub tree: BTreeMap<String, String>,
}

/// One path's content in a snapshot (SPEC u280 `SnapshotContent`). Every
/// snapshot this build writes carries `Stored`: the blob hash naming the
/// file `snapshot-content/{stored}` in the state directory, which holds
/// the content. `Text` and `Bytes` are the inline forms a released build
/// wrote, still loaded as they stand.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum SnapshotContent {
    Text(String),
    Bytes(Vec<u8>),
    Stored { stored: String },
}

/// The directory under the state directory each snapshot content is
/// stored in, one file per content.
const SNAPSHOT_CONTENT_DIR: &str = "snapshot-content";

#[cfg(unix)]
const CONTENT_DIR_MODE: u32 = 0o700;
#[cfg(unix)]
const CONTENT_FILE_MODE: u32 = 0o600;

/// Each snapshotted path mapped to its content, or to `None` where the
/// path was absent.
pub type Snapshot = BTreeMap<String, Option<SnapshotContent>>;

/// The working copy's exclusive state lock, released when dropped or
/// when its process dies.
#[derive(Debug)]
pub struct StateLock {
    _file: File,
}

impl WorkingCopy {
    /// Open the state directory for `owner/name` worked on at `root`.
    ///
    /// The key is the canonical root, so two spellings of one directory
    /// — a link to it included — open one state directory.
    pub fn open(
        cache_dir: &Path,
        owner: &str,
        name: &str,
        root: &Path,
    ) -> Result<WorkingCopy, CliError> {
        let canonical = std::fs::canonicalize(root).map_err(|err| CliError::Io {
            message: format!("could not resolve working copy {}: {err}", root.display()),
        })?;
        let state_dir = Self::state_dir_for(cache_dir, owner, name, &canonical);
        std::fs::create_dir_all(&state_dir).map_err(|err| CliError::Io {
            message: format!(
                "could not create working copy state {}: {err}",
                state_dir.display()
            ),
        })?;
        let canonical_state = std::fs::canonicalize(&state_dir).map_err(|err| CliError::Io {
            message: format!(
                "could not resolve working copy state {}: {err}",
                state_dir.display()
            ),
        })?;
        if canonical_state.starts_with(&canonical) {
            return Err(CliError::Io {
                message: format!(
                    "the working copy state {} lies inside the working copy {}; point SYNS_CACHE_DIR elsewhere",
                    canonical_state.display(),
                    canonical.display()
                ),
            });
        }
        Ok(WorkingCopy {
            owner: owner.to_string(),
            name: name.to_string(),
            root: canonical,
            state_dir: canonical_state,
        })
    }

    /// Where the state directory for `owner/name` at `root` already
    /// stands, the working copy holding it, and `None` where it does not
    /// — the read a guard makes when it must not create one (SPEC u271,
    /// `checkout_of`, which answers none of its three by writing state).
    pub fn open_existing(
        cache_dir: &Path,
        owner: &str,
        name: &str,
        root: &Path,
    ) -> Result<Option<WorkingCopy>, CliError> {
        let Ok(canonical) = std::fs::canonicalize(root) else {
            return Ok(None);
        };
        if !Self::state_dir_for(cache_dir, owner, name, &canonical).is_dir() {
            return Ok(None);
        }
        Self::open(cache_dir, owner, name, root).map(Some)
    }

    /// Where the state for `owner/name` at a canonical root stands. The
    /// key is that root, so two spellings of one directory — a link to
    /// it included — address one state directory.
    fn state_dir_for(cache_dir: &Path, owner: &str, name: &str, canonical: &Path) -> PathBuf {
        cache_dir
            .join("working-copies")
            .join(owner)
            .join(name)
            .join(blob_sha1(canonical.as_os_str().as_encoded_bytes()))
    }

    /// Take the exclusive state lock, waiting while another process
    /// holds it.
    pub fn lock(&self) -> Result<StateLock, CliError> {
        let path = self.state_dir.join(LOCK_FILE);
        let io = |err: std::io::Error| CliError::Io {
            message: format!(
                "could not lock working copy state {}: {err}",
                self.state_dir.display()
            ),
        };
        let file = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(&path)
            .map_err(io)?;
        file.lock().map_err(io)?;
        Ok(StateLock { _file: file })
    }

    /// The recorded base, none where its file is absent or unreadable.
    pub fn base(&self) -> Option<Manifest> {
        let text = std::fs::read_to_string(self.state_dir.join(BASE_FILE)).ok()?;
        serde_json::from_str(&text).ok()
    }

    /// Record `commit` and exactly `files` as the base.
    pub fn record_base(
        &self,
        commit: &str,
        files: HashMap<String, String>,
    ) -> Result<(), CliError> {
        let mut manifest = Manifest::default();
        manifest.update(commit.to_string(), files);
        write_json(&self.state_dir.join(BASE_FILE), &manifest)
    }

    pub fn resolution(&self) -> Result<Option<Resolution>, CliError> {
        read_json(&self.state_dir.join(RESOLUTION_FILE))
    }

    pub fn write_resolution(&self, resolution: &Resolution) -> Result<(), CliError> {
        write_json(&self.state_dir.join(RESOLUTION_FILE), resolution)
    }

    pub fn remove_resolution(&self) -> Result<(), CliError> {
        remove_state_file(&self.state_dir.join(RESOLUTION_FILE))
    }

    pub fn outbox(&self) -> Result<Option<Outbox>, CliError> {
        read_json(&self.state_dir.join(OUTBOX_FILE))
    }

    pub fn write_outbox(&self, outbox: &Outbox) -> Result<(), CliError> {
        write_json(&self.state_dir.join(OUTBOX_FILE), outbox)
    }

    pub fn remove_outbox(&self) -> Result<(), CliError> {
        remove_state_file(&self.state_dir.join(OUTBOX_FILE))
    }

    /// The folder's content before a reconciliation rewrote it.
    pub fn local_snapshot(&self) -> Result<Snapshot, CliError> {
        Ok(read_json(&self.local_snapshot_path())?.unwrap_or_default())
    }

    pub fn write_local_snapshot(&self, snapshot: &Snapshot) -> Result<(), CliError> {
        self.flush_snapshot_content()?;
        write_json(&self.local_snapshot_path(), snapshot)?;
        self.prune_snapshot_content()
    }

    /// Each collision's content at the head it was prepared against.
    pub fn remote_snapshot(&self) -> Result<Snapshot, CliError> {
        Ok(read_json(&self.remote_snapshot_path())?.unwrap_or_default())
    }

    pub fn write_remote_snapshot(&self, snapshot: &Snapshot) -> Result<(), CliError> {
        self.flush_snapshot_content()?;
        write_json(&self.remote_snapshot_path(), snapshot)?;
        self.prune_snapshot_content()
    }

    /// Make every content stored since the last flush durable in its
    /// directory, once for all of them, before a document names any.
    fn flush_snapshot_content(&self) -> Result<(), CliError> {
        let dir = self.snapshot_content_dir();
        if !dir.is_dir() {
            return Ok(());
        }
        finish_directory_flush(&dir, flush_directory(&dir))
    }

    /// Write whichever snapshot documents are given, then remove every
    /// content file neither standing snapshot names — the one prune a
    /// step writing both documents takes, so no content the second
    /// document names is removed before it is written.
    pub fn write_snapshots(
        &self,
        local: Option<&Snapshot>,
        remote: Option<&Snapshot>,
    ) -> Result<(), CliError> {
        self.flush_snapshot_content()?;
        if let Some(local) = local {
            write_json(&self.local_snapshot_path(), local)?;
        }
        if let Some(remote) = remote {
            write_json(&self.remote_snapshot_path(), remote)?;
        }
        self.prune_snapshot_content()
    }

    /// Remove both snapshots, then every content file neither names.
    pub fn remove_snapshots(&self) -> Result<(), CliError> {
        remove_state_file(&self.local_snapshot_path())?;
        remove_state_file(&self.remote_snapshot_path())?;
        self.prune_snapshot_content()
    }

    /// The directory holding each stored snapshot content.
    pub fn snapshot_content_dir(&self) -> PathBuf {
        self.state_dir.join(SNAPSHOT_CONTENT_DIR)
    }

    /// Create the content directory, reachable by the person's own
    /// account alone, where it does not stand.
    fn content_dir(&self) -> Result<PathBuf, CliError> {
        let dir = self.snapshot_content_dir();
        let io = |err: std::io::Error| CliError::Io {
            message: format!("could not write {}: {err}", dir.display()),
        };
        if !dir.is_dir() {
            let mut builder = std::fs::DirBuilder::new();
            builder.recursive(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::DirBuilderExt;
                builder.mode(CONTENT_DIR_MODE);
            }
            builder.create(&dir).map_err(io)?;
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(CONTENT_DIR_MODE))
                .map_err(io)?;
        }
        Ok(dir)
    }

    /// Store content by its blob hash: `fill` writes it into a fresh file
    /// beside the content files and answers its blob hash, and the file
    /// is made durable and renamed to that hash — its directory entry made
    /// durable by the flush the next snapshot document's write opens with. Answers the hash and
    /// whether this call created the content file, none standing there
    /// before.
    fn store_with(
        &self,
        fill: impl FnOnce(&mut File) -> Result<String, CliError>,
    ) -> Result<(String, bool), CliError> {
        let dir = self.content_dir()?;
        let temp = dir.join(format!(
            ".incoming.{}.{}",
            std::process::id(),
            SIBLING_COUNTER.fetch_add(1, Ordering::Relaxed)
        ));
        let io = |err: std::io::Error| CliError::Io {
            message: format!("could not write {}: {err}", temp.display()),
        };
        let mut options = OpenOptions::new();
        options.write(true).read(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(CONTENT_FILE_MODE);
        }
        let stored = (|| {
            let mut file = options.open(&temp).map_err(io)?;
            let sha = fill(&mut file)?;
            file.sync_all().map_err(io)?;
            drop(file);
            let target = dir.join(&sha);
            let created = !target.exists();
            std::fs::rename(&temp, &target).map_err(io)?;
            Ok((sha, created))
        })();
        if stored.is_err() {
            let _ = std::fs::remove_file(&temp);
        }
        stored
    }

    /// Store `bytes` as a snapshot content.
    pub fn store_bytes(&self, bytes: &[u8]) -> Result<(String, bool), CliError> {
        self.store_with(|file| {
            file.write_all(bytes).map_err(|err| CliError::Io {
                message: format!("could not write a snapshot content: {err}"),
            })?;
            Ok(blob_sha1(bytes))
        })
    }

    /// Store the file at `source` as a snapshot content, copied in
    /// pieces as it stands.
    pub fn store_file(&self, source: &Path) -> Result<(String, bool), CliError> {
        self.store_with(|file| {
            copy_hashing(source, file).map_err(|err| CliError::Io {
                message: format!("could not read {}: {err}", source.display()),
            })
        })
    }

    /// Store a collected file as a snapshot content through
    /// `copy_collected`, so what is stored hashes to what the collection
    /// took.
    pub fn store_collected(
        &self,
        root: &Path,
        path: &str,
        collected: &crate::push::collector::CollectedFile,
    ) -> Result<(String, bool), CliError> {
        let dir = self.content_dir()?;
        let temp = dir.join(format!(
            ".incoming.{}.{}",
            std::process::id(),
            SIBLING_COUNTER.fetch_add(1, Ordering::Relaxed)
        ));
        crate::push::collector::copy_collected(root, path, collected, &temp)?;
        let io = |err: std::io::Error| CliError::Io {
            message: format!("could not write {}: {err}", temp.display()),
        };
        let stored = (|| {
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                std::fs::set_permissions(&temp, std::fs::Permissions::from_mode(CONTENT_FILE_MODE))
                    .map_err(io)?;
            }
            File::open(&temp).and_then(|f| f.sync_all()).map_err(io)?;
            let target = dir.join(&collected.sha);
            let created = !target.exists();
            std::fs::rename(&temp, &target).map_err(io)?;
            Ok((collected.sha.clone(), created))
        })();
        if stored.is_err() {
            let _ = std::fs::remove_file(&temp);
        }
        stored
    }

    /// Remove a content file this run stored and no snapshot came to name.
    pub fn remove_stored(&self, sha: &str) {
        let _ = std::fs::remove_file(self.snapshot_content_dir().join(sha));
    }

    /// The file holding a stored content, answered only where its bytes,
    /// read in pieces, hash to its name.
    pub fn stored_content(&self, sha: &str) -> Result<PathBuf, CliError> {
        let path = self.snapshot_content_dir().join(sha);
        let refused = || CliError::Io {
            message: format!(
                "could not read {}: content does not match its hash",
                path.display()
            ),
        };
        let mut sink = std::io::sink();
        match copy_hashing(&path, &mut sink) {
            Ok(actual) if actual == sha => Ok(path),
            _ => Err(refused()),
        }
    }

    /// A snapshot content's bytes, whatever form it was written in, a
    /// stored one read only where it hashes to its name.
    pub fn content_bytes(&self, content: &SnapshotContent) -> Result<Vec<u8>, CliError> {
        match content {
            SnapshotContent::Text(text) => Ok(text.clone().into_bytes()),
            SnapshotContent::Bytes(bytes) => Ok(bytes.clone()),
            SnapshotContent::Stored { stored } => {
                let path = self.stored_content(stored)?;
                std::fs::read(&path).map_err(|err| CliError::Io {
                    message: format!("could not read {}: {err}", path.display()),
                })
            }
        }
    }

    /// Remove every content file neither standing snapshot names.
    pub fn prune_snapshot_content(&self) -> Result<(), CliError> {
        let dir = self.snapshot_content_dir();
        let Ok(entries) = std::fs::read_dir(&dir) else {
            return Ok(());
        };
        let mut named = std::collections::HashSet::new();
        for snapshot in [self.local_snapshot()?, self.remote_snapshot()?] {
            for content in snapshot.into_values().flatten() {
                if let SnapshotContent::Stored { stored } = content {
                    named.insert(stored);
                }
            }
        }
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().to_string();
            if name.starts_with(".incoming.") || named.contains(&name) {
                continue;
            }
            let _ = std::fs::remove_file(entry.path());
        }
        if named.is_empty() {
            let _ = std::fs::remove_dir(&dir);
        }
        Ok(())
    }

    pub fn local_snapshot_path(&self) -> PathBuf {
        self.state_dir.join(LOCAL_SNAPSHOT_FILE)
    }

    pub fn remote_snapshot_path(&self) -> PathBuf {
        self.state_dir.join(REMOTE_SNAPSHOT_FILE)
    }

    /// The stat record this working copy keeps, empty where none loads.
    pub fn stat_record(&self) -> StatRecord {
        StatRecord::load(&self.state_dir)
    }

    pub fn write_stat_record(&self, record: &StatRecord) -> Result<(), CliError> {
        record.save(&self.state_dir)
    }
}

/// Copy `source` into `dest` in pieces, answering the blob hash of what
/// was copied.
fn copy_hashing(source: &Path, dest: &mut impl Write) -> std::io::Result<String> {
    use sha1::{Digest, Sha1};
    use std::io::Read;
    let mut from = File::open(source)?;
    let declared = from.metadata()?.len();
    let mut hasher = Sha1::new();
    hasher.update(format!("blob {declared}\0").as_bytes());
    let mut buf = vec![0u8; 64 * 1024];
    let mut copied = 0u64;
    loop {
        let n = match from.read(&mut buf) {
            Ok(n) => n,
            Err(err) if err.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(err) => return Err(err),
        };
        if n == 0 {
            break;
        }
        copied += n as u64;
        hasher.update(&buf[..n]);
        dest.write_all(&buf[..n])?;
    }
    if copied != declared {
        return Err(std::io::Error::other(
            "the file changed while it was copied",
        ));
    }
    Ok(hasher.finalize().iter().fold(String::new(), |mut acc, b| {
        use std::fmt::Write as _;
        let _ = write!(acc, "{b:02x}");
        acc
    }))
}

fn read_json<T: DeserializeOwned>(path: &Path) -> Result<Option<T>, CliError> {
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(err) => {
            return Err(CliError::Io {
                message: format!("could not read {}: {err}", path.display()),
            });
        }
    };
    serde_json::from_str(&text)
        .map(Some)
        .map_err(|err| CliError::Io {
            message: format!("could not read {}: {err}", path.display()),
        })
}

fn write_json<T: Serialize>(path: &Path, value: &T) -> Result<(), CliError> {
    let bytes = serde_json::to_vec_pretty(value).map_err(|err| CliError::Io {
        message: format!("could not serialise {}: {err}", path.display()),
    })?;
    write_atomic(path, &bytes, flush_directory)
}

fn remove_state_file(path: &Path) -> Result<(), CliError> {
    match std::fs::remove_file(path) {
        Ok(()) => {}
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(err) => {
            return Err(CliError::Io {
                message: format!("could not remove {}: {err}", path.display()),
            });
        }
    }
    match path.parent() {
        Some(dir) => finish_directory_flush(dir, flush_directory(dir)),
        None => Ok(()),
    }
}

/// The directory flush a state write ends with, injectable so both of
/// its outcomes can be driven.
pub type DirectoryFlush = fn(&Path) -> std::io::Result<()>;

static SIBLING_COUNTER: AtomicU64 = AtomicU64::new(0);

/// Replace `target` with `bytes` whole: write a sibling, flush it,
/// rename it over the target, then flush the directory through
/// `flush_dir`.
///
/// A directory flush the filesystem refuses as unsupported — NFS and
/// FUSE mounts among them — stands once the file's own flush and the
/// rename succeeded; any other refusal fails the write.
pub fn write_atomic(
    target: &Path,
    bytes: &[u8],
    flush_dir: DirectoryFlush,
) -> Result<(), CliError> {
    let dir = target.parent().ok_or_else(|| CliError::Io {
        message: format!("could not write {}: no parent directory", target.display()),
    })?;
    let file_name = target
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_default();
    let sibling = dir.join(format!(
        ".{file_name}.{}.{}.tmp",
        std::process::id(),
        SIBLING_COUNTER.fetch_add(1, Ordering::Relaxed)
    ));

    let written = (|| -> std::io::Result<()> {
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&sibling)?;
        file.write_all(bytes)?;
        file.sync_all()?;
        drop(file);
        std::fs::rename(&sibling, target)
    })();
    if let Err(err) = written {
        let _ = std::fs::remove_file(&sibling);
        return Err(CliError::Io {
            message: format!("could not write {}: {err}", target.display()),
        });
    }

    finish_directory_flush(dir, flush_dir(dir))
}

fn finish_directory_flush(dir: &Path, flushed: std::io::Result<()>) -> Result<(), CliError> {
    match flushed {
        Ok(()) => Ok(()),
        Err(err) if is_unsupported_flush(&err) => Ok(()),
        Err(err) => Err(CliError::Io {
            message: format!("could not flush directory {}: {err}", dir.display()),
        }),
    }
}

#[cfg(windows)]
fn flush_directory(dir: &Path) -> std::io::Result<()> {
    use std::os::windows::fs::OpenOptionsExt;
    const FILE_FLAG_BACKUP_SEMANTICS: u32 = 0x0200_0000;
    OpenOptions::new()
        .read(true)
        .write(true)
        .custom_flags(FILE_FLAG_BACKUP_SEMANTICS)
        .open(dir)?
        .sync_all()
}

#[cfg(not(windows))]
fn flush_directory(dir: &Path) -> std::io::Result<()> {
    File::open(dir)?.sync_all()
}

#[cfg(unix)]
fn is_unsupported_flush(err: &std::io::Error) -> bool {
    matches!(
        err.raw_os_error(),
        Some(code) if code == libc::EINVAL || code == libc::ENOTSUP || code == libc::EOPNOTSUPP
    )
}

#[cfg(windows)]
fn is_unsupported_flush(err: &std::io::Error) -> bool {
    const ERROR_INVALID_FUNCTION: i32 = 1;
    const ERROR_NOT_SUPPORTED: i32 = 50;
    const ERROR_INVALID_PARAMETER: i32 = 87;
    matches!(
        err.raw_os_error(),
        Some(ERROR_INVALID_FUNCTION | ERROR_NOT_SUPPORTED | ERROR_INVALID_PARAMETER)
    )
}

#[cfg(not(any(unix, windows)))]
fn is_unsupported_flush(_err: &std::io::Error) -> bool {
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    fn unsupported() -> std::io::Error {
        std::io::Error::from_raw_os_error(libc::EINVAL)
    }

    #[cfg(windows)]
    fn unsupported() -> std::io::Error {
        std::io::Error::from_raw_os_error(1)
    }

    #[cfg(unix)]
    fn failing() -> std::io::Error {
        std::io::Error::from_raw_os_error(libc::EIO)
    }

    #[cfg(windows)]
    fn failing() -> std::io::Error {
        std::io::Error::from_raw_os_error(5)
    }

    #[test]
    fn unsupported_directory_flush_still_replaces_the_file() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("base.json");
        std::fs::write(&target, "old").unwrap();

        let result = write_atomic(&target, b"new", |_| Err(unsupported()));

        assert!(result.is_ok(), "{result:?}");
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "new");
        let leftovers: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(
            leftovers.len(),
            1,
            "a sibling was left behind: {leftovers:?}"
        );
    }

    #[test]
    fn failing_directory_flush_fails_the_write() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("base.json");

        let result = write_atomic(&target, b"new", |_| Err(failing()));

        match result {
            Err(CliError::Io { message }) => {
                assert!(message.contains("could not flush directory"), "{message}")
            }
            other => panic!("expected CliError::Io, got {other:?}"),
        }
    }

    #[test]
    fn open_keys_a_link_and_its_target_to_one_state_directory() {
        let cache = tempfile::tempdir().unwrap();
        let tree = tempfile::tempdir().unwrap();
        let target = tree.path().join("copy");
        std::fs::create_dir_all(target.join("sub")).unwrap();

        let direct = WorkingCopy::open(cache.path(), "alice", "proj", &target).unwrap();
        let dotted = WorkingCopy::open(
            cache.path(),
            "alice",
            "proj",
            &target.join("sub").join(".."),
        )
        .unwrap();
        assert_eq!(direct.state_dir, dotted.state_dir);

        #[cfg(unix)]
        {
            let link = tree.path().join("link");
            std::os::unix::fs::symlink(&target, &link).unwrap();
            let linked = WorkingCopy::open(cache.path(), "alice", "proj", &link).unwrap();
            assert_eq!(direct.state_dir, linked.state_dir);
            assert_eq!(direct.root, linked.root);
        }

        let other = tree.path().join("other");
        std::fs::create_dir_all(&other).unwrap();
        let elsewhere = WorkingCopy::open(cache.path(), "alice", "proj", &other).unwrap();
        assert_ne!(direct.state_dir, elsewhere.state_dir);
    }

    #[test]
    fn state_files_round_trip_and_an_unreadable_base_reads_as_none() {
        let cache = tempfile::tempdir().unwrap();
        let tree = tempfile::tempdir().unwrap();
        let copy = WorkingCopy::open(cache.path(), "alice", "proj", tree.path()).unwrap();

        assert!(copy.base().is_none());
        copy.record_base("h0", HashMap::from([("a.md".into(), "1".into())]))
            .unwrap();
        let base = copy.base().unwrap();
        assert_eq!(base.commit_sha(), Some("h0"));
        assert_eq!(base.file_sha("a.md"), Some("1"));

        std::fs::write(copy.state_dir.join(BASE_FILE), "{torn").unwrap();
        assert!(copy.base().is_none());

        let mut snapshot = Snapshot::new();
        snapshot.insert("t.md".into(), Some(SnapshotContent::Text("text".into())));
        snapshot.insert(
            "b.bin".into(),
            Some(SnapshotContent::Bytes(vec![0xff, 0x00])),
        );
        snapshot.insert("gone.md".into(), None);
        copy.write_local_snapshot(&snapshot).unwrap();
        assert_eq!(copy.local_snapshot().unwrap(), snapshot);

        let outbox = Outbox {
            parent_commit: None,
            tree: BTreeMap::from([("a.md".into(), "2".into())]),
        };
        copy.write_outbox(&outbox).unwrap();
        assert_eq!(copy.outbox().unwrap(), Some(outbox));
        copy.remove_outbox().unwrap();
        copy.remove_outbox().unwrap();
        assert_eq!(copy.outbox().unwrap(), None);
    }

    #[test]
    fn open_refuses_a_state_directory_inside_the_working_copy() {
        let tree = tempfile::tempdir().unwrap();
        let cache = tree.path().join(".cache");
        let result = WorkingCopy::open(&cache, "alice", "proj", tree.path());
        assert!(matches!(result, Err(CliError::Io { .. })), "{result:?}");
    }

    // ---- u280: the stat record and stored snapshot content -------------

    fn open_copy() -> (tempfile::TempDir, tempfile::TempDir, WorkingCopy) {
        let cache = tempfile::tempdir().unwrap();
        let tree = tempfile::tempdir().unwrap();
        let copy = WorkingCopy::open(cache.path(), "alice", "proj", tree.path()).unwrap();
        (cache, tree, copy)
    }

    #[test]
    fn the_stat_record_round_trips_and_an_unreadable_one_loads_empty() {
        let (_cache, _tree, copy) = open_copy();
        assert_eq!(copy.stat_record(), StatRecord::default());

        let mut record = StatRecord::default();
        record.entries.insert(
            "a.png".into(),
            StatEntry {
                size: 6,
                mtime: (1, 2),
                ctime: (3, 4),
                inode: 5,
                sha: "0".repeat(40),
            },
        );
        record.stamp = Some((7, 8));
        copy.write_stat_record(&record).unwrap();
        assert_eq!(copy.stat_record(), record);

        std::fs::write(copy.state_dir.join(STAT_RECORD_FILE), "{torn").unwrap();
        assert_eq!(copy.stat_record(), StatRecord::default());
    }

    #[cfg(unix)]
    #[test]
    fn an_entry_whose_mtime_equals_the_stamp_is_not_trusted() {
        use std::os::unix::fs::MetadataExt;
        let (_cache, tree, _copy) = open_copy();
        let file = tree.path().join("a.png");
        std::fs::write(&file, b"\x89PNG\x00").unwrap();
        let meta = std::fs::metadata(&file).unwrap();
        let mut record = StatRecord::default();
        record.observe("a.png", &meta, &meta, "sha");
        record.stamp = Some((meta.mtime(), meta.mtime_nsec()));
        assert_eq!(record.trusted("a.png", &meta), None);
        record.stamp = Some((meta.mtime().max(meta.ctime()) + 1, 0));
        assert_eq!(record.trusted("a.png", &meta).as_deref(), Some("sha"));
    }

    #[cfg(unix)]
    #[test]
    fn a_restamp_leaves_no_probe_behind() {
        let (_cache, tree, _copy) = open_copy();
        let mut record = StatRecord::default();
        record.restamp(tree.path());
        assert!(record.stamp.is_some());
        assert_eq!(std::fs::read_dir(tree.path()).unwrap().count(), 0);
    }

    #[test]
    fn a_stored_content_round_trips_by_its_hash() {
        let (_cache, _tree, copy) = open_copy();
        let bytes = [0x89u8, b'P', b'N', b'G', 0x00, 0xff];
        let (sha, created) = copy.store_bytes(&bytes).unwrap();
        assert!(created);
        assert_eq!(sha, blob_sha1(&bytes));
        let content = SnapshotContent::Stored {
            stored: sha.clone(),
        };
        assert_eq!(copy.content_bytes(&content).unwrap(), bytes.to_vec());
        let json = serde_json::to_string(&content).unwrap();
        assert_eq!(json, format!("{{\"stored\":\"{sha}\"}}"));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let dir = std::fs::metadata(copy.snapshot_content_dir()).unwrap();
            assert_eq!(dir.permissions().mode() & 0o777, 0o700);
            let file = std::fs::metadata(copy.snapshot_content_dir().join(&sha)).unwrap();
            assert_eq!(file.permissions().mode() & 0o777, 0o600);
        }
    }

    #[test]
    fn a_content_file_no_longer_hashing_to_its_name_is_refused() {
        let (_cache, _tree, copy) = open_copy();
        let (sha, _) = copy.store_bytes(b"local\n").unwrap();
        std::fs::write(copy.snapshot_content_dir().join(&sha), b"tampered\n").unwrap();
        match copy.stored_content(&sha) {
            Err(CliError::Io { message }) => {
                assert!(
                    message.contains("content does not match its hash"),
                    "{message}"
                )
            }
            other => panic!("expected the refusal, got {other:?}"),
        }
    }

    #[test]
    fn an_orphaned_content_file_is_removed_on_the_next_snapshot_write() {
        let (_cache, _tree, copy) = open_copy();
        let (kept, _) = copy.store_bytes(b"kept\n").unwrap();
        let (orphan, _) = copy.store_bytes(b"orphan\n").unwrap();
        let mut snapshot = Snapshot::new();
        snapshot.insert(
            "a.md".into(),
            Some(SnapshotContent::Stored {
                stored: kept.clone(),
            }),
        );
        copy.write_local_snapshot(&snapshot).unwrap();
        assert!(copy.snapshot_content_dir().join(&kept).exists());
        assert!(!copy.snapshot_content_dir().join(&orphan).exists());

        copy.remove_snapshots().unwrap();
        assert!(!copy.snapshot_content_dir().exists());
    }

    #[test]
    fn a_released_builds_inline_snapshot_still_loads() {
        let (_cache, _tree, copy) = open_copy();
        std::fs::write(
            copy.local_snapshot_path(),
            r#"{"image.png":[137,80,78,71,0,255],"a.md":"a\n","gone.md":null}"#,
        )
        .unwrap();
        let snapshot = copy.local_snapshot().unwrap();
        assert_eq!(
            snapshot["image.png"],
            Some(SnapshotContent::Bytes(vec![137, 80, 78, 71, 0, 255]))
        );
        assert_eq!(snapshot["a.md"], Some(SnapshotContent::Text("a\n".into())));
        assert_eq!(snapshot["gone.md"], None);
    }
}
