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

/// One path's content in a snapshot — text where the bytes are UTF-8,
/// the bytes themselves otherwise.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum SnapshotContent {
    Text(String),
    Bytes(Vec<u8>),
}

impl SnapshotContent {
    pub fn from_bytes(bytes: Vec<u8>) -> SnapshotContent {
        match String::from_utf8(bytes) {
            Ok(text) => SnapshotContent::Text(text),
            Err(err) => SnapshotContent::Bytes(err.into_bytes()),
        }
    }

    pub fn into_bytes(self) -> Vec<u8> {
        match self {
            SnapshotContent::Text(text) => text.into_bytes(),
            SnapshotContent::Bytes(bytes) => bytes,
        }
    }
}

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
        let key = blob_sha1(canonical.as_os_str().as_encoded_bytes());
        let state_dir = cache_dir
            .join("working-copies")
            .join(owner)
            .join(name)
            .join(key);
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
        write_json(&self.local_snapshot_path(), snapshot)
    }

    /// Each collision's content at the head it was prepared against.
    pub fn remote_snapshot(&self) -> Result<Snapshot, CliError> {
        Ok(read_json(&self.remote_snapshot_path())?.unwrap_or_default())
    }

    pub fn write_remote_snapshot(&self, snapshot: &Snapshot) -> Result<(), CliError> {
        write_json(&self.remote_snapshot_path(), snapshot)
    }

    pub fn remove_snapshots(&self) -> Result<(), CliError> {
        remove_state_file(&self.local_snapshot_path())?;
        remove_state_file(&self.remote_snapshot_path())
    }

    pub fn local_snapshot_path(&self) -> PathBuf {
        self.state_dir.join(LOCAL_SNAPSHOT_FILE)
    }

    pub fn remote_snapshot_path(&self) -> PathBuf {
        self.state_dir.join(REMOTE_SNAPSHOT_FILE)
    }
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
        snapshot.insert(
            "t.md".into(),
            Some(SnapshotContent::from_bytes(b"text".to_vec())),
        );
        snapshot.insert(
            "b.bin".into(),
            Some(SnapshotContent::from_bytes(vec![0xff, 0x00])),
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
}
