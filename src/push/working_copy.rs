//! The per-working-copy state a convergence keeps outside the folder
//! (SPEC u256 § Contract Surface, `WorkingCopy`).
//!
//! One state directory per repository and canonical root, under the
//! cache root, holding the state lock, the recorded base, a pending
//! resolution, the publication outbox and the two private snapshots.
//! SPEC u298: where the default root refuses a run's writes, the copy's
//! state is written to its in-root home, `{canonical root}/.syns-state`,
//! instead; every run reads and locks both homes, the state stamped last
//! answering its reads and adopted by the next lock.
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

use crate::config::{StoreRoots, is_foreign_or_link};
use crate::errors::CliError;
use crate::push::hash::blob_sha1;
use crate::push::manifest::Manifest;
use crate::push::reconcile::CollisionKind;
use crate::repo::folder::{FolderScope, lies_under};

const LOCK_FILE: &str = "state.lock";
const BASE_FILE: &str = "base.json";
const RESOLUTION_FILE: &str = "resolution.json";
const OUTBOX_FILE: &str = "outbox.json";
const LOCAL_SNAPSHOT_FILE: &str = "local-snapshot.json";
const REMOTE_SNAPSHOT_FILE: &str = "remote-snapshot.json";
const STAT_RECORD_FILE: &str = "stat-record.json";

/// SPEC u298, `IN_ROOT_HOME`: the directory at a working copy's root its
/// state is kept in where the default root refuses a run's writes. A path
/// holding this segment, letter case aside, is collected, retrieved and
/// placed by nothing.
pub const IN_ROOT_HOME: &str = ".syns-state";

/// SPEC u298, `STATE_STAMP`: the nanoseconds since the Unix epoch a
/// home's state last changed, rewritten under the lock at every write or
/// removal of a state file.
pub const STATE_STAMP: &str = "state.stamp";

/// The local record's file name inside an in-root home (SPEC u298,
/// `Manifest::save`).
pub(crate) const LOCAL_RECORD_FILE: &str = "local-record.json";

const GITIGNORE_FILE: &str = ".gitignore";

/// The state files the `WorkingCopy` reads answer from, beside every
/// content under `snapshot-content`.
const READ_FILES: [&str; 6] = [
    BASE_FILE,
    RESOLUTION_FILE,
    OUTBOX_FILE,
    LOCAL_SNAPSHOT_FILE,
    REMOTE_SNAPSHOT_FILE,
    STAT_RECORD_FILE,
];

#[cfg(unix)]
const IN_ROOT_HOME_MODE: u32 = 0o700;

/// Whether a served or collected path holds the in-root home as one of
/// its segments, under either separator and letter case aside.
pub(crate) fn holds_in_root_home(path: &str) -> bool {
    path.split(['/', '\\'])
        .any(|seg| seg.eq_ignore_ascii_case(IN_ROOT_HOME))
}

/// Create the in-root home `dir` where it does not stand, at mode `0700`
/// holding a `.gitignore` whose one line is `*`, and set its mode to
/// `0700`; one standing as a link or owned by another account is refused
/// (SPEC u298 Behaviour, `WorkingCopy::open` 1).
pub(crate) fn ensure_in_root_home(dir: &Path) -> Result<(), CliError> {
    let refuse = |reason: &dyn std::fmt::Display| CliError::Io {
        message: format!(
            "could not create working copy state {}: {reason}",
            dir.display()
        ),
    };
    let foreign = "it stands as a link or is owned by another account";
    if is_foreign_or_link(dir) {
        return Err(refuse(&foreign));
    }
    if !dir.is_dir() {
        let mut builder = std::fs::DirBuilder::new();
        #[cfg(unix)]
        {
            use std::os::unix::fs::DirBuilderExt;
            builder.mode(IN_ROOT_HOME_MODE);
        }
        match builder.create(dir) {
            Ok(()) => {}
            Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(err) => return Err(refuse(&err)),
        }
        if is_foreign_or_link(dir) {
            return Err(refuse(&foreign));
        }
        if !dir.is_dir() {
            return Err(refuse(&"it stands as a file"));
        }
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(IN_ROOT_HOME_MODE))
            .map_err(|err| refuse(&err))?;
    }
    let ignore = dir.join(GITIGNORE_FILE);
    if !ignore.exists() {
        write_atomic(&ignore, b"*\n", flush_directory)?;
    }
    Ok(())
}

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
///
/// SPEC u291: `folder` is the scoped folder a folder copy works, its
/// holder's `owner` and `name` standing beside it; none on every copy
/// `WorkingCopy::open` answers.
///
/// SPEC u298: `state_dir` names the copy's write home — its in-root home
/// where the default root refuses the run's writes, its default home
/// otherwise — and `other_home` the home the run does not write.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkingCopy {
    pub owner: String,
    pub name: String,
    pub root: PathBuf,
    pub state_dir: PathBuf,
    pub folder: Option<FolderScope>,
    stores: StoreRoots,
    other_home: PathBuf,
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
/// when its process dies: the write home's, and the other home's where
/// one could be taken (SPEC u298, `StateLock`).
#[derive(Debug)]
pub struct StateLock {
    _write: File,
    _other: Option<File>,
}

#[cfg(test)]
impl StateLock {
    /// Whether the other home's lock was taken beside the write home's.
    fn holds_other(&self) -> bool {
        self._other.is_some()
    }
}

/// Where a home stands in the order the `WorkingCopy` reads follow: a
/// stamped home after every unstamped one, stamps compared by value, and
/// unstamped homes by the newest modification time among the files the
/// reads answer from, a home holding none of them first.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Standing {
    Empty,
    Unstamped(std::time::SystemTime),
    Stamped(u128),
}

/// Open the standing lock file at `path` for writing, else for reading,
/// and take its lock, waiting; none where it is absent, opens neither way
/// or refuses the lock call. Creates nothing.
fn take_standing_lock(path: &Path) -> Option<File> {
    if !path.is_file() {
        return None;
    }
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .open(path)
        .or_else(|_| OpenOptions::new().read(true).open(path))
        .ok()?;
    file.lock().ok()?;
    Some(file)
}

impl WorkingCopy {
    /// Open the state of `owner/name` worked on at `root` over `stores`.
    ///
    /// The key is the canonical root, so two spellings of one directory
    /// — a link to it included — open one state directory. The write home
    /// is created where it does not stand (SPEC u298, `WorkingCopy::open`).
    pub fn open(
        stores: &StoreRoots,
        owner: &str,
        name: &str,
        root: &Path,
    ) -> Result<WorkingCopy, CliError> {
        let canonical = std::fs::canonicalize(root).map_err(|err| CliError::Io {
            message: format!("could not resolve working copy {}: {err}", root.display()),
        })?;
        if !stores.default_refused {
            let state_dir = Self::state_dir_for(&stores.default, owner, name, &canonical);
            std::fs::create_dir_all(&state_dir).map_err(|err| CliError::Io {
                message: format!(
                    "could not create working copy state {}: {err}",
                    state_dir.display()
                ),
            })?;
        }
        let copy = Self::at(stores, owner, name, canonical)?;
        copy.ensure_write_home()?;
        Ok(copy)
    }

    /// The copy of `owner/name` at the canonical root, both homes named
    /// and neither created; a default home lying inside the root refused.
    fn at(
        stores: &StoreRoots,
        owner: &str,
        name: &str,
        canonical: PathBuf,
    ) -> Result<WorkingCopy, CliError> {
        let default_home = Self::default_home_for(stores, owner, name, &canonical)?;
        let in_root = canonical.join(IN_ROOT_HOME);
        let (state_dir, other_home) = if stores.default_refused {
            (in_root, default_home)
        } else {
            (default_home, in_root)
        };
        Ok(WorkingCopy {
            owner: owner.to_string(),
            name: name.to_string(),
            root: canonical,
            state_dir,
            folder: None,
            stores: stores.clone(),
            other_home,
        })
    }

    /// The default home of `owner/name` at a canonical root, canonical
    /// where it stands; the refusal of one lying inside the copy's root.
    fn default_home_for(
        stores: &StoreRoots,
        owner: &str,
        name: &str,
        canonical: &Path,
    ) -> Result<PathBuf, CliError> {
        let raw = Self::state_dir_for(&stores.default, owner, name, canonical);
        let home = match std::fs::canonicalize(&raw) {
            Ok(home) => home,
            Err(_) => match std::fs::canonicalize(&stores.default) {
                Ok(default) => Self::state_dir_for(&default, owner, name, canonical),
                Err(_) => raw,
            },
        };
        if home.starts_with(canonical) {
            return Err(CliError::Io {
                message: format!(
                    "the working copy state {} lies inside the working copy {}; point SYNS_CACHE_DIR elsewhere",
                    home.display(),
                    canonical.display()
                ),
            });
        }
        Ok(home)
    }

    /// Whether the run writes this copy's state to its in-root home.
    fn writes_in_root(&self) -> bool {
        self.stores.default_refused
    }

    /// The copy's in-root home, whichever role it plays.
    fn in_root_home(&self) -> &Path {
        if self.writes_in_root() {
            &self.state_dir
        } else {
            &self.other_home
        }
    }

    /// Whether `home` may be read: an in-root home standing as a link or
    /// owned by another account is read as holding no state.
    fn readable(&self, home: &Path) -> bool {
        home != self.in_root_home() || !is_foreign_or_link(home)
    }

    /// Create the write home where it does not stand — the in-root home
    /// through `ensure_in_root_home` — before a write reaches it.
    fn ensure_write_home(&self) -> Result<(), CliError> {
        if self.writes_in_root() {
            return ensure_in_root_home(&self.state_dir);
        }
        std::fs::create_dir_all(&self.state_dir).map_err(|err| CliError::Io {
            message: format!(
                "could not create working copy state {}: {err}",
                self.state_dir.display()
            ),
        })
    }

    /// The roots the copy was opened over (SPEC u298, `WorkingCopy::stores`).
    pub fn stores(&self) -> &StoreRoots {
        &self.stores
    }

    /// Open the state of the repository the scope addresses worked on at
    /// the folder's canonical directory (SPEC u291,
    /// `WorkingCopy::open_folder`): a folder and its holder's checkout
    /// never share a state directory. SPEC u302 `WorkingCopy::open_folder`
    /// 1–2: a folder bound to its identity opens the identity's copy, the
    /// base its holder's copy there recorded later carried to it.
    pub fn open_folder(stores: &StoreRoots, scope: &FolderScope) -> Result<WorkingCopy, CliError> {
        let (owner, name) = Self::addressed(scope);
        let mut copy = Self::open(stores, &owner, &name, &scope.dir)?;
        copy.folder = Some(scope.clone());
        if scope.identity.is_some() {
            copy.carry_base(&scope.holder())?;
        }
        Ok(copy)
    }

    /// The owner and the name a scope's `address` joins.
    fn addressed(scope: &FolderScope) -> (String, String) {
        let name = scope.identity.as_ref().unwrap_or(&scope.name);
        (scope.owner.clone(), name.clone())
    }

    /// The folder copy where either home already holds its state, none
    /// where neither does, creating nothing and carrying nothing (SPEC
    /// u291, `WorkingCopy::open_existing_folder`; SPEC u302, keyed on the
    /// scope's address).
    pub fn open_existing_folder(
        stores: &StoreRoots,
        scope: &FolderScope,
    ) -> Result<Option<WorkingCopy>, CliError> {
        let (owner, name) = Self::addressed(scope);
        Ok(
            Self::open_existing(stores, &owner, &name, &scope.dir)?.map(|mut copy| {
                copy.folder = Some(scope.clone());
                copy
            }),
        )
    }

    /// Carry the base the copy of `from` (`OWNER/NAME`) at this copy's
    /// canonical directory records to this copy where it was recorded
    /// after this copy's own, or this copy records none — its commit,
    /// per-file hashes and recorded time unchanged, the copy of `from`
    /// left as it stood — answering whether it did (SPEC u302 Behaviour,
    /// `WorkingCopy::carry_base` 1–3). A record carrying no time ranks
    /// below every one that does.
    pub fn carry_base(&self, from: &str) -> Result<bool, CliError> {
        // 1 — this copy's base, under its lock.
        let _lock = self.lock()?;
        let own = self.base();

        // 2 — the other copy's, in either home, creating no state.
        let Some((owner, name)) = from.split_once('/') else {
            return Ok(false);
        };
        let Some(other) = Self::open_existing(&self.stores, owner, name, &self.root)? else {
            return Ok(false);
        };
        let Some(carried) = other.base() else {
            return Ok(false);
        };
        if own.is_some_and(|own| own.recorded_at() >= carried.recorded_at()) {
            return Ok(false);
        }

        // 3 — recorded as this copy's, unchanged.
        let files = carried
            .file_paths()
            .filter_map(|path| {
                carried
                    .file_sha(path)
                    .map(|sha| (path.to_string(), sha.to_string()))
            })
            .collect();
        self.record_laid_base(
            carried.commit_sha().unwrap_or_default(),
            files,
            carried.recorded_at(),
        )?;
        Ok(true)
    }

    /// The copy of `owner/name` at `root` where either of its homes
    /// already stands, and `None` where neither does — the read a guard
    /// makes when it must not create one (SPEC u271, `checkout_of`, which
    /// answers none of its three by writing state). It creates no
    /// directory; a write through the copy creates its write home first.
    pub fn open_existing(
        stores: &StoreRoots,
        owner: &str,
        name: &str,
        root: &Path,
    ) -> Result<Option<WorkingCopy>, CliError> {
        let Ok(canonical) = std::fs::canonicalize(root) else {
            return Ok(None);
        };
        let in_root = canonical.join(IN_ROOT_HOME);
        let standing = Self::state_dir_for(&stores.default, owner, name, &canonical).is_dir()
            || (in_root.is_dir() && !is_foreign_or_link(&in_root));
        if !standing {
            return Ok(None);
        }
        Self::at(stores, owner, name, canonical).map(Some)
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

    /// Take the copy's lock in every home where one could be taken, the
    /// default home's first, waiting while another process holds either;
    /// then adopt the other home's state where it was stamped later
    /// (SPEC u298 Behaviour, `WorkingCopy::lock` 1–3).
    pub fn lock(&self) -> Result<StateLock, CliError> {
        let io = |err: std::io::Error| CliError::Io {
            message: format!(
                "could not lock working copy state {}: {err}",
                self.state_dir.display()
            ),
        };
        let take_write = || -> Result<File, CliError> {
            self.ensure_write_home()?;
            let file = OpenOptions::new()
                .create(true)
                .truncate(false)
                .read(true)
                .write(true)
                .open(self.state_dir.join(LOCK_FILE))
                .map_err(io)?;
            file.lock().map_err(io)?;
            Ok(file)
        };
        let take_other = || -> Option<File> {
            if !self.readable(&self.other_home) {
                return None;
            }
            take_standing_lock(&self.other_home.join(LOCK_FILE))
        };
        // 1 — the default home's lock before the in-root home's.
        let lock = if self.writes_in_root() {
            let other = take_other();
            StateLock {
                _write: take_write()?,
                _other: other,
            }
        } else {
            let write = take_write()?;
            StateLock {
                _write: write,
                _other: take_other(),
            }
        };
        // 3 — the state stamped last adopted into the write home.
        if self.other_stamped_later() {
            self.adopt().map_err(|err| CliError::Io {
                message: format!(
                    "could not adopt working copy state into {}: {err}",
                    self.state_dir.display()
                ),
            })?;
        }
        Ok(lock)
    }

    /// Where `home` stands in the order the reads follow.
    fn standing(&self, home: &Path) -> Standing {
        if !self.readable(home) {
            return Standing::Empty;
        }
        if let Some(stamp) = std::fs::read_to_string(home.join(STATE_STAMP))
            .ok()
            .and_then(|text| text.trim().parse::<u128>().ok())
        {
            return Standing::Stamped(stamp);
        }
        read_files(home)
            .into_iter()
            .map(|(_, modified)| modified)
            .max()
            .map_or(Standing::Empty, Standing::Unstamped)
    }

    /// Whether the other home is ordered after the write home, a tie
    /// going to the write home (SPEC u298, `WorkingCopy` reads).
    fn other_stamped_later(&self) -> bool {
        self.standing(&self.other_home) > self.standing(&self.state_dir)
    }

    /// The home the reads answer from: the one ordered later, none where
    /// it may not be read.
    fn read_home(&self) -> Option<&Path> {
        let home: &Path = if self.other_stamped_later() {
            &self.other_home
        } else {
            &self.state_dir
        };
        self.readable(home).then_some(home)
    }

    /// The file `name` in the home the reads answer from.
    fn read_path(&self, name: &str) -> Option<PathBuf> {
        self.read_home().map(|home| home.join(name))
    }

    /// `WorkingCopy::lock` 3: copy every state file the other home's
    /// reads answer from into the write home, the newest-modified last and
    /// each keeping its modification time, after removing each the write
    /// home holds that the other lacks; then the other's stamp, last.
    fn adopt(&self) -> std::io::Result<()> {
        let source = &self.other_home;
        let target = &self.state_dir;
        let mut adopted = read_files(source);
        let keep: std::collections::HashSet<&str> =
            adopted.iter().map(|(rel, _)| rel.as_str()).collect();
        for (rel, _) in read_files(target) {
            if !keep.contains(rel.as_str()) {
                match std::fs::remove_file(target.join(&rel)) {
                    Ok(()) => {}
                    Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
                    Err(err) => return Err(err),
                }
            }
        }
        adopted.sort_by_key(|(_, modified)| *modified);
        if adopted
            .iter()
            .any(|(rel, _)| rel.starts_with(SNAPSHOT_CONTENT_DIR))
        {
            self.content_dir()
                .map_err(|err| std::io::Error::other(err.to_string()))?;
        }
        for (rel, modified) in &adopted {
            copy_keeping_time(&source.join(rel), &target.join(rel), *modified)?;
        }
        let stamp = source.join(STATE_STAMP);
        if stamp.is_file() {
            let bytes = std::fs::read(&stamp)?;
            write_atomic(&target.join(STATE_STAMP), &bytes, flush_directory)
                .map_err(|err| std::io::Error::other(err.to_string()))?;
        }
        Ok(())
    }

    /// Rewrite the write home's stamp, dating its last change.
    fn restamp(&self) -> Result<(), CliError> {
        write_atomic(
            &self.state_dir.join(STATE_STAMP),
            now_nanos_wide().to_string().as_bytes(),
            flush_directory,
        )
    }

    /// Write `value` as the state file `name` in the write home, then
    /// restamp it.
    fn write_state<T: Serialize>(&self, name: &str, value: &T) -> Result<(), CliError> {
        self.ensure_write_home()?;
        write_json(&self.state_dir.join(name), value)?;
        self.restamp()
    }

    /// Remove the state file `name` from the write home, restamping it
    /// where a file was removed.
    fn remove_state(&self, name: &str) -> Result<(), CliError> {
        if remove_state_file(&self.state_dir.join(name))? {
            self.restamp()?;
        }
        Ok(())
    }

    /// The recorded base, none where its file is absent or unreadable.
    pub fn base(&self) -> Option<Manifest> {
        let text = std::fs::read_to_string(self.read_path(BASE_FILE)?).ok()?;
        serde_json::from_str(&text).ok()
    }

    /// Record `commit` and exactly `files` as the base, stamped with the
    /// time the system clock reads as it is recorded (SPEC u291).
    pub fn record_base(
        &self,
        commit: &str,
        files: HashMap<String, String>,
    ) -> Result<(), CliError> {
        self.record_laid_base(commit, files, Some(now_nanos()))
    }

    /// Record `commit` and exactly `files` as the base carrying
    /// `recorded_at` as given: a base laid over this copy's keeps the time
    /// this copy's record carried before it (SPEC u291, `converge` 6).
    pub fn record_laid_base(
        &self,
        commit: &str,
        files: HashMap<String, String>,
        recorded_at: Option<u64>,
    ) -> Result<(), CliError> {
        let mut manifest = Manifest::default();
        manifest.update(commit.to_string(), files);
        manifest.set_recorded_at(recorded_at);
        self.write_state(BASE_FILE, &manifest)
    }

    /// Lay `files`, each path already counted from this copy's root, over
    /// the base it records (SPEC u304 Behaviour, `WorkingCopy::lay_files`
    /// 1–2): under the state lock, where the copy holds no resolution and
    /// no outbox and records a base, that base then carries each of
    /// `files` at its hash beside every path it carried, its commit
    /// `landed` exactly where it was `advance_from` and its own otherwise,
    /// the record's time kept. True where the base was laid; false, with
    /// nothing written and no state created, otherwise.
    pub fn lay_files(
        &self,
        files: &HashMap<String, String>,
        advance_from: Option<&str>,
        landed: &str,
    ) -> Result<bool, CliError> {
        // 1 — nothing written to a copy recording no base, before the lock
        // creates its write home.
        if self.base().is_none() {
            return Ok(false);
        }
        let _lock = self.lock()?;
        if self.resolution()?.is_some() || self.outbox()?.is_some() {
            return Ok(false);
        }
        let Some(standing) = self.base() else {
            return Ok(false);
        };
        // 2 — every path the base carried, beside each of `files`.
        let mut laid: HashMap<String, String> = standing
            .file_paths()
            .filter_map(|path| {
                standing
                    .file_sha(path)
                    .map(|sha| (path.to_string(), sha.to_string()))
            })
            .collect();
        for (path, sha) in files {
            laid.insert(path.clone(), sha.clone());
        }
        let commit = match standing.commit_sha() {
            Some(own) if advance_from == Some(own) => landed,
            own => own.unwrap_or_default(),
        };
        self.record_laid_base(commit, laid, standing.recorded_at())?;
        Ok(true)
    }

    pub fn resolution(&self) -> Result<Option<Resolution>, CliError> {
        match self.read_path(RESOLUTION_FILE) {
            Some(path) => read_json(&path),
            None => Ok(None),
        }
    }

    pub fn write_resolution(&self, resolution: &Resolution) -> Result<(), CliError> {
        self.write_state(RESOLUTION_FILE, resolution)
    }

    pub fn remove_resolution(&self) -> Result<(), CliError> {
        self.remove_state(RESOLUTION_FILE)
    }

    pub fn outbox(&self) -> Result<Option<Outbox>, CliError> {
        match self.read_path(OUTBOX_FILE) {
            Some(path) => read_json(&path),
            None => Ok(None),
        }
    }

    pub fn write_outbox(&self, outbox: &Outbox) -> Result<(), CliError> {
        self.write_state(OUTBOX_FILE, outbox)
    }

    pub fn remove_outbox(&self) -> Result<(), CliError> {
        self.remove_state(OUTBOX_FILE)
    }

    /// A snapshot document as the home the reads answer from holds it.
    fn read_snapshot(&self, name: &str) -> Result<Snapshot, CliError> {
        match self.read_path(name) {
            Some(path) => Ok(read_json(&path)?.unwrap_or_default()),
            None => Ok(Snapshot::new()),
        }
    }

    /// The folder's content before a reconciliation rewrote it.
    pub fn local_snapshot(&self) -> Result<Snapshot, CliError> {
        self.read_snapshot(LOCAL_SNAPSHOT_FILE)
    }

    pub fn write_local_snapshot(&self, snapshot: &Snapshot) -> Result<(), CliError> {
        self.write_snapshots(Some(snapshot), None)
    }

    /// Each collision's content at the head it was prepared against.
    pub fn remote_snapshot(&self) -> Result<Snapshot, CliError> {
        self.read_snapshot(REMOTE_SNAPSHOT_FILE)
    }

    pub fn write_remote_snapshot(&self, snapshot: &Snapshot) -> Result<(), CliError> {
        self.write_snapshots(None, Some(snapshot))
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
        self.ensure_write_home()?;
        self.flush_snapshot_content()?;
        if let Some(local) = local {
            write_json(&self.local_snapshot_path(), local)?;
        }
        if let Some(remote) = remote {
            write_json(&self.remote_snapshot_path(), remote)?;
        }
        self.prune_snapshot_content()?;
        self.restamp()
    }

    /// Remove both snapshots, then every content file neither names.
    pub fn remove_snapshots(&self) -> Result<(), CliError> {
        let local = remove_state_file(&self.local_snapshot_path())?;
        let remote = remove_state_file(&self.remote_snapshot_path())?;
        self.prune_snapshot_content()?;
        if local || remote {
            self.restamp()?;
        }
        Ok(())
    }

    /// The directory holding each stored snapshot content.
    pub fn snapshot_content_dir(&self) -> PathBuf {
        self.state_dir.join(SNAPSHOT_CONTENT_DIR)
    }

    /// Create the content directory, reachable by the person's own
    /// account alone, where it does not stand.
    fn content_dir(&self) -> Result<PathBuf, CliError> {
        self.ensure_write_home()?;
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
            flush_file(&temp).map_err(io)?;
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
        let path = self
            .read_home()
            .unwrap_or(&self.state_dir)
            .join(SNAPSHOT_CONTENT_DIR)
            .join(sha);
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

    /// Remove every content file neither snapshot standing in the write
    /// home names.
    pub fn prune_snapshot_content(&self) -> Result<(), CliError> {
        let dir = self.snapshot_content_dir();
        let Ok(entries) = std::fs::read_dir(&dir) else {
            return Ok(());
        };
        let mut named = std::collections::HashSet::new();
        let standing = |path: PathBuf| -> Result<Snapshot, CliError> {
            Ok(read_json(&path)?.unwrap_or_default())
        };
        for snapshot in [
            standing(self.local_snapshot_path())?,
            standing(self.remote_snapshot_path())?,
        ] {
            for content in snapshot.into_values().flatten() {
                if let SnapshotContent::Stored { stored } = content {
                    named.insert(stored);
                }
            }
        }
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().to_string();
            // Every caller holds the state lock and has renamed each of
            // its own stores, so an `.incoming.` file standing here is a
            // killed run's (CR1-4).
            if named.contains(&name) {
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
        match self.read_home() {
            Some(home) => StatRecord::load(home),
            None => StatRecord::default(),
        }
    }

    pub fn write_stat_record(&self, record: &StatRecord) -> Result<(), CliError> {
        self.ensure_write_home()?;
        record.save(&self.state_dir)?;
        self.restamp()
    }
}

/// Each state file the reads answer from standing in `home` — the
/// documents, and every content under `snapshot-content` — counted from
/// `home`, beside its modification time.
fn read_files(home: &Path) -> Vec<(String, std::time::SystemTime)> {
    let modified = |path: &Path| {
        std::fs::symlink_metadata(path)
            .ok()
            .filter(|meta| meta.is_file())
            .and_then(|meta| meta.modified().ok())
    };
    let mut files: Vec<(String, std::time::SystemTime)> = READ_FILES
        .iter()
        .filter_map(|name| modified(&home.join(name)).map(|time| (name.to_string(), time)))
        .collect();
    for entry in std::fs::read_dir(home.join(SNAPSHOT_CONTENT_DIR))
        .into_iter()
        .flatten()
        .flatten()
    {
        let name = entry.file_name().to_string_lossy().to_string();
        if name.starts_with('.') {
            continue;
        }
        if let Some(time) = modified(&entry.path()) {
            files.push((format!("{SNAPSHOT_CONTENT_DIR}/{name}"), time));
        }
    }
    files
}

/// Copy `source` over `target` through a sibling renamed over it, the
/// sibling carrying `modified` as its modification time before the rename.
fn copy_keeping_time(
    source: &Path,
    target: &Path,
    modified: std::time::SystemTime,
) -> std::io::Result<()> {
    let dir = target
        .parent()
        .ok_or_else(|| std::io::Error::other("no parent directory"))?;
    let sibling = dir.join(format!(
        ".adopt.{}.{}.tmp",
        std::process::id(),
        SIBLING_COUNTER.fetch_add(1, Ordering::Relaxed)
    ));
    let copied = (|| -> std::io::Result<()> {
        let bytes = std::fs::read(source)?;
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(CONTENT_FILE_MODE);
        }
        let mut file = options.open(&sibling)?;
        file.write_all(&bytes)?;
        file.set_modified(modified)?;
        file.sync_all()?;
        drop(file);
        std::fs::rename(&sibling, target)
    })();
    if copied.is_err() {
        let _ = std::fs::remove_file(&sibling);
    }
    copied?;
    match flush_directory(dir) {
        Ok(()) => Ok(()),
        Err(err) if is_unsupported_flush(&err) => Ok(()),
        Err(err) => Err(err),
    }
}

/// The nanoseconds since the Unix epoch the system clock reads now, in
/// full.
fn now_nanos_wide() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or_default()
}

/// The nanoseconds since the Unix epoch the system clock reads now.
fn now_nanos() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| u64::try_from(d.as_nanos()).unwrap_or(u64::MAX))
        .unwrap_or_default()
}

/// The base a run inside the folder `scope` reads the folder's files
/// against (SPEC u291 Behaviour, `folder_base`): of the folder copy's own
/// base and the base of each copy enclosing it — the holder checkout's
/// and each enclosing folder's — narrowed to the folder and counted from
/// it, the one recorded last, a record carrying no time ranking below
/// every one that does and a tie going to the copy whose root lies
/// outermost. It creates no state.
pub fn folder_base(stores: &StoreRoots, scope: &FolderScope) -> Option<Manifest> {
    // 1 — every record standing, outermost first, each narrowed to the
    // folder: the holder checkout's, each enclosing folder's outermost
    // first, then the folder's own.
    let mut standing: Vec<Manifest> = Vec::new();
    let narrowed = |base: Manifest, under: Option<&str>| -> Manifest {
        let files = base
            .file_paths()
            .filter_map(|path| {
                let from_holder = match under {
                    Some(under) if !under.is_empty() => format!("{under}/{path}"),
                    _ => path.to_string(),
                };
                lies_under(&from_holder, &scope.path).then(|| {
                    (
                        from_holder[scope.path.len() + 1..].to_string(),
                        base.file_sha(path).unwrap_or_default().to_string(),
                    )
                })
            })
            .collect();
        let mut manifest = Manifest::default();
        manifest.update(base.commit_sha().unwrap_or_default().to_string(), files);
        manifest.set_recorded_at(base.recorded_at());
        manifest
    };
    if let Some(checkout) = &scope.checkout
        && let Ok(Some(copy)) =
            WorkingCopy::open_existing(stores, &scope.owner, &scope.name, checkout)
        && let Some(base) = copy.base()
    {
        standing.push(narrowed(base, None));
    }
    for enclosing in scope.enclosing.iter().rev() {
        if let Ok(Some(copy)) = WorkingCopy::open_existing_folder(stores, enclosing)
            && let Some(base) = copy.base()
        {
            standing.push(narrowed(base, Some(&enclosing.path)));
        }
    }
    if let Ok(Some(copy)) = WorkingCopy::open_existing_folder(stores, scope)
        && let Some(base) = copy.base()
    {
        standing.push(base);
    }
    // SPEC u302 Contract Surface, `folder_base`: a folder bound to its
    // identity reads its holder's copy at its directory beside its own.
    if scope.identity.is_some()
        && let Ok(Some(copy)) =
            WorkingCopy::open_existing(stores, &scope.owner, &scope.name, &scope.dir)
        && let Some(base) = copy.base()
    {
        standing.insert(0, base);
    }

    // 2 — the greatest recorded time, the outermost root on a tie.
    let mut chosen: Option<Manifest> = None;
    for record in standing {
        let later = match &chosen {
            None => true,
            Some(best) => record.recorded_at() > best.recorded_at(),
        };
        if later {
            chosen = Some(record);
        }
    }
    chosen
}

/// Copy `source` into `dest` in pieces, answering the blob hash of what
/// was copied; a file whose length moves under the copy is refused.
fn copy_hashing(source: &Path, dest: &mut impl Write) -> std::io::Result<String> {
    let from = File::open(source)?;
    let declared = from.metadata()?.len();
    crate::push::hash::hash_pieces(from, declared, dest)?
        .ok_or_else(|| std::io::Error::other("the file changed while it was copied"))
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

/// Remove a state file, answering whether one was removed.
fn remove_state_file(path: &Path) -> Result<bool, CliError> {
    match std::fs::remove_file(path) {
        Ok(()) => {}
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(err) => {
            return Err(CliError::Io {
                message: format!("could not remove {}: {err}", path.display()),
            });
        }
    }
    match path.parent() {
        Some(dir) => finish_directory_flush(dir, flush_directory(dir)).map(|()| true),
        None => Ok(true),
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

/// Flush a file written through a handle since closed, reopened for
/// writing: Windows refuses `FlushFileBuffers` on a handle opened only to
/// read, answering `Access is denied`.
fn flush_file(path: &Path) -> std::io::Result<()> {
    OpenOptions::new().write(true).open(path)?.sync_all()
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

    /// Windows refuses a flush through a handle opened only to read, so
    /// the flush must open its file for writing: a file the account may
    /// write but not read is flushed.
    #[cfg(unix)]
    #[test]
    fn a_closed_file_is_flushed_through_a_handle_that_may_write() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("written");
        std::fs::write(&path, b"written").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o200)).unwrap();
        let flushed = flush_file(&path);
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        flushed.unwrap();
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

        let direct = WorkingCopy::open(
            &crate::config::StoreRoots::resolve(Some(cache.path()), cache.path(), cache.path()),
            "alice",
            "proj",
            &target,
        )
        .unwrap();
        let dotted = WorkingCopy::open(
            &crate::config::StoreRoots::resolve(Some(cache.path()), cache.path(), cache.path()),
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
            let linked = WorkingCopy::open(
                &crate::config::StoreRoots::resolve(Some(cache.path()), cache.path(), cache.path()),
                "alice",
                "proj",
                &link,
            )
            .unwrap();
            assert_eq!(direct.state_dir, linked.state_dir);
            assert_eq!(direct.root, linked.root);
        }

        let other = tree.path().join("other");
        std::fs::create_dir_all(&other).unwrap();
        let elsewhere = WorkingCopy::open(
            &crate::config::StoreRoots::resolve(Some(cache.path()), cache.path(), cache.path()),
            "alice",
            "proj",
            &other,
        )
        .unwrap();
        assert_ne!(direct.state_dir, elsewhere.state_dir);
    }

    #[test]
    fn state_files_round_trip_and_an_unreadable_base_reads_as_none() {
        let cache = tempfile::tempdir().unwrap();
        let tree = tempfile::tempdir().unwrap();
        let copy = WorkingCopy::open(
            &crate::config::StoreRoots::resolve(Some(cache.path()), cache.path(), cache.path()),
            "alice",
            "proj",
            tree.path(),
        )
        .unwrap();

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

    /// Every path standing under `root`, sorted.
    fn entries_under(root: &Path) -> Vec<PathBuf> {
        fn walk(dir: &Path, out: &mut Vec<PathBuf>) {
            for entry in std::fs::read_dir(dir).into_iter().flatten().flatten() {
                out.push(entry.path());
                walk(&entry.path(), out);
            }
        }
        let mut out = Vec::new();
        walk(root, &mut out);
        out.sort();
        out
    }

    fn q3_scope(w: &Path, checkout: Option<PathBuf>) -> FolderScope {
        FolderScope {
            dir: w.join("clients/vela/q3-board"),
            owner: "alice".into(),
            name: "work".into(),
            path: "clients/vela/q3-board".into(),
            checkout,
            enclosing: Vec::new(),
            identity: None,
        }
    }

    /// Rewrite the base recorded in `copy` with `recorded_at` in its bytes.
    fn stamp(copy: &WorkingCopy, recorded_at: Option<u64>) {
        let base = copy.base().unwrap();
        let files = base
            .file_paths()
            .map(|p| (p.to_string(), base.file_sha(p).unwrap().to_string()))
            .collect();
        copy.record_laid_base(base.commit_sha().unwrap(), files, recorded_at)
            .unwrap();
    }

    // SPEC u291 Tests, `folder_base_narrows_the_holder_base`.
    #[test]
    fn folder_base_narrows_the_holder_base() {
        let cache = tempfile::tempdir().unwrap();
        let tree = tempfile::tempdir().unwrap();
        let w = std::fs::canonicalize(tree.path()).unwrap();
        std::fs::create_dir_all(w.join("clients/vela/q3-board")).unwrap();
        let holder = WorkingCopy::open(
            &crate::config::StoreRoots::resolve(Some(cache.path()), cache.path(), cache.path()),
            "alice",
            "work",
            &w,
        )
        .unwrap();
        let holder_files = HashMap::from([
            (".page/x.json".to_string(), "x1".to_string()),
            ("clients/vela/q3-board/a.md".to_string(), "a1".to_string()),
        ]);
        holder.record_base("h1", holder_files.clone()).unwrap();
        let scope = q3_scope(&w, Some(w.clone()));
        let answer = |scope: &FolderScope| {
            let before = entries_under(cache.path());
            let base = folder_base(
                &crate::config::StoreRoots::resolve(Some(cache.path()), cache.path(), cache.path()),
                scope,
            );
            assert_eq!(
                entries_under(cache.path()),
                before,
                "folder_base wrote state"
            );
            base.map(|b| {
                let mut files: Vec<(String, String)> = b
                    .file_paths()
                    .map(|p| (p.to_string(), b.file_sha(p).unwrap().to_string()))
                    .collect();
                files.sort();
                (b.commit_sha().unwrap().to_string(), files)
            })
        };
        let h1 = Some((
            "h1".to_string(),
            vec![("a.md".to_string(), "a1".to_string())],
        ));
        let h0 = Some((
            "h0".to_string(),
            vec![("b.md".to_string(), "b0".to_string())],
        ));

        assert_eq!(answer(&scope), h1);
        assert_eq!(answer(&q3_scope(&w, None)), None);

        let folder = WorkingCopy::open_folder(
            &crate::config::StoreRoots::resolve(Some(cache.path()), cache.path(), cache.path()),
            &scope,
        )
        .unwrap();
        folder
            .record_base(
                "h0",
                HashMap::from([("b.md".to_string(), "b0".to_string())]),
            )
            .unwrap();
        holder.record_base("h1", holder_files).unwrap();
        assert_eq!(answer(&scope), h1);

        stamp(&holder, Some(7));
        stamp(&folder, Some(7));
        assert_eq!(answer(&scope), h1);

        stamp(&holder, None);
        assert_eq!(answer(&scope), h0);
    }

    /// `U/q3-plan` bound to `alice/docs-q3-plan`, beside the cache.
    fn identity_folder() -> (tempfile::TempDir, tempfile::TempDir, PathBuf, StoreRoots) {
        let cache = tempfile::tempdir().unwrap();
        let tree = tempfile::tempdir().unwrap();
        let u = std::fs::canonicalize(tree.path()).unwrap();
        std::fs::create_dir_all(u.join("q3-plan")).unwrap();
        std::fs::write(
            u.join("q3-plan/.syns.yaml"),
            "holder: alice/docs\npath: q3-plan\nshared_as: docs-q3-plan\n",
        )
        .unwrap();
        let stores = StoreRoots::resolve(Some(cache.path()), cache.path(), cache.path());
        (cache, tree, u, stores)
    }

    fn base_of(copy: &WorkingCopy) -> (String, Vec<(String, String)>, Option<u64>) {
        let base = copy.base().expect("a base");
        let mut files: Vec<(String, String)> = base
            .file_paths()
            .map(|p| (p.to_string(), base.file_sha(p).unwrap().to_string()))
            .collect();
        files.sort();
        (
            base.commit_sha().unwrap().to_string(),
            files,
            base.recorded_at(),
        )
    }

    // SPEC u302 Tests, `the_base_recorded_under_the_holder_carries_to_the_identity`.
    #[test]
    fn the_base_recorded_under_the_holder_carries_to_the_identity() {
        let (_cache, _tree, u, stores) = identity_folder();
        let holder = WorkingCopy::open(&stores, "alice", "docs", &u.join("q3-plan")).unwrap();
        holder
            .record_base(
                "h4",
                HashMap::from([("document.html".to_string(), "b1".to_string())]),
            )
            .unwrap();
        let recorded = base_of(&holder);
        assert!(
            WorkingCopy::open_existing(&stores, "alice", "docs-q3-plan", &u.join("q3-plan"))
                .unwrap()
                .is_none()
        );

        let scope = crate::repo::folder::resolve_folder_scope(&u.join("q3-plan"))
            .unwrap()
            .expect("a scope");
        let copy = WorkingCopy::open_folder(&stores, &scope).unwrap();
        assert_eq!(
            (copy.owner.as_str(), copy.name.as_str()),
            ("alice", "docs-q3-plan")
        );
        assert_eq!(base_of(&copy), recorded);
        assert_eq!(base_of(&holder), recorded);
        assert_ne!(copy.state_dir, holder.state_dir);
    }

    // SPEC u302 Tests, `carry_base_never_overwrites_a_later_base`.
    #[test]
    fn carry_base_never_overwrites_a_later_base() {
        let (_cache, _tree, u, stores) = identity_folder();
        let dir = u.join("q3-plan");
        let holder = WorkingCopy::open(&stores, "alice", "docs", &dir).unwrap();
        let identity = WorkingCopy::open(&stores, "alice", "docs-q3-plan", &dir).unwrap();
        let files = HashMap::from([("document.html".to_string(), "b1".to_string())]);
        holder.record_base("h4", files.clone()).unwrap();
        identity.record_base("h7", files).unwrap();
        stamp(&holder, Some(4));
        stamp(&identity, Some(7));

        assert!(!identity.carry_base("alice/docs").unwrap());
        assert_eq!(base_of(&identity).0, "h7");
        assert!(holder.carry_base("alice/docs-q3-plan").unwrap());
        assert_eq!(base_of(&holder), base_of(&identity));
        assert_eq!(base_of(&holder).2, Some(7));

        stamp(&identity, None);
        assert!(!holder.carry_base("alice/docs-q3-plan").unwrap());
    }

    // SPEC u291 Contract Surface, `WorkingCopy::open_folder`: a folder and
    // its holder's checkout never share a state directory.
    #[test]
    fn a_folder_copy_and_its_checkout_hold_two_state_directories() {
        let cache = tempfile::tempdir().unwrap();
        let tree = tempfile::tempdir().unwrap();
        let w = std::fs::canonicalize(tree.path()).unwrap();
        std::fs::create_dir_all(w.join("clients/vela/q3-board")).unwrap();
        let scope = q3_scope(&w, Some(w.clone()));
        assert_eq!(
            WorkingCopy::open_existing_folder(
                &crate::config::StoreRoots::resolve(Some(cache.path()), cache.path(), cache.path()),
                &scope
            )
            .unwrap(),
            None
        );
        let folder = WorkingCopy::open_folder(
            &crate::config::StoreRoots::resolve(Some(cache.path()), cache.path(), cache.path()),
            &scope,
        )
        .unwrap();
        let holder = WorkingCopy::open(
            &crate::config::StoreRoots::resolve(Some(cache.path()), cache.path(), cache.path()),
            "alice",
            "work",
            &w,
        )
        .unwrap();
        assert_ne!(folder.state_dir, holder.state_dir);
        assert_eq!(folder.folder.as_ref(), Some(&scope));
        assert_eq!(holder.folder, None);
        assert_eq!(folder.stores().write, cache.path());
        assert_eq!(
            WorkingCopy::open_existing_folder(
                &crate::config::StoreRoots::resolve(Some(cache.path()), cache.path(), cache.path()),
                &scope
            )
            .unwrap()
            .map(|c| c.state_dir),
            Some(folder.state_dir)
        );
    }

    #[test]
    fn open_refuses_a_state_directory_inside_the_working_copy() {
        let tree = tempfile::tempdir().unwrap();
        let cache = tree.path().join(".cache");
        let result = WorkingCopy::open(
            &crate::config::StoreRoots::resolve(Some(&cache), &cache, &cache),
            "alice",
            "proj",
            tree.path(),
        );
        assert!(matches!(result, Err(CliError::Io { .. })), "{result:?}");
    }

    // ---- u280: the stat record and stored snapshot content -------------

    fn open_copy() -> (tempfile::TempDir, tempfile::TempDir, WorkingCopy) {
        let cache = tempfile::tempdir().unwrap();
        let tree = tempfile::tempdir().unwrap();
        let copy = WorkingCopy::open(
            &crate::config::StoreRoots::resolve(Some(cache.path()), cache.path(), cache.path()),
            "alice",
            "proj",
            tree.path(),
        )
        .unwrap();
        (cache, tree, copy)
    }

    // ---- u304: a base laid with a landed version's paths -----------------

    /// A fresh copy recording base `h1` with `a.md` at `b1` and the
    /// recorded time `7`.
    fn copy_at_h1() -> (tempfile::TempDir, tempfile::TempDir, WorkingCopy) {
        let (cache, tree, copy) = open_copy();
        copy.record_laid_base(
            "h1",
            HashMap::from([("a.md".to_string(), "b1".to_string())]),
            Some(7),
        )
        .unwrap();
        (cache, tree, copy)
    }

    fn laid(copy: &WorkingCopy) -> (Option<String>, Vec<(String, String)>, Option<u64>) {
        let base = copy.base().expect("a base");
        let mut files: Vec<(String, String)> = base
            .file_paths()
            .map(|p| (p.to_string(), base.file_sha(p).unwrap().to_string()))
            .collect();
        files.sort();
        (
            base.commit_sha().map(str::to_string),
            files,
            base.recorded_at(),
        )
    }

    /// Each state file the test wrote, at the bytes it holds.
    fn written(copy: &WorkingCopy) -> Vec<(&'static str, Option<Vec<u8>>)> {
        [BASE_FILE, RESOLUTION_FILE, OUTBOX_FILE, STATE_STAMP]
            .into_iter()
            .map(|name| (name, std::fs::read(copy.state_dir.join(name)).ok()))
            .collect()
    }

    // SPEC u304 Tests, the row of this name.
    #[test]
    fn lay_files_lays_over_a_clean_base_advancing_only_from_the_named_commit() {
        let files = HashMap::from([("x/y.md".to_string(), "b2".to_string())]);
        let carried = vec![
            ("a.md".to_string(), "b1".to_string()),
            ("x/y.md".to_string(), "b2".to_string()),
        ];

        let (_cache, _tree, copy) = copy_at_h1();
        assert!(copy.lay_files(&files, Some("h1"), "h3").unwrap());
        assert_eq!(laid(&copy), (Some("h3".into()), carried.clone(), Some(7)));

        let (_cache, _tree, copy) = copy_at_h1();
        assert!(copy.lay_files(&files, Some("h2"), "h3").unwrap());
        assert_eq!(laid(&copy), (Some("h1".into()), carried, Some(7)));
    }

    // SPEC u304 Tests, the row of this name.
    #[test]
    fn lay_files_leaves_a_copy_holding_work_or_no_base_as_it_stood() {
        let files = HashMap::from([("x/y.md".to_string(), "b2".to_string())]);

        let (_cache, _tree, copy) = copy_at_h1();
        copy.write_resolution(&Resolution {
            recovery_id: "r".into(),
            base_commit: Some("h1".into()),
            head_commit: "h0".into(),
            round: 1,
            local_paths: Vec::new(),
            remote_paths: Vec::new(),
            collisions: Vec::new(),
            combined_paths: Vec::new(),
            reviewed_tree: None,
            pending_writes: None,
        })
        .unwrap();
        let before = written(&copy);
        assert!(!copy.lay_files(&files, Some("h1"), "h3").unwrap());
        assert_eq!(written(&copy), before, "a resolution standing");

        let (_cache, _tree, copy) = copy_at_h1();
        copy.write_outbox(&Outbox {
            parent_commit: Some("h1".into()),
            tree: BTreeMap::new(),
        })
        .unwrap();
        let before = written(&copy);
        assert!(!copy.lay_files(&files, Some("h1"), "h3").unwrap());
        assert_eq!(written(&copy), before, "an outbox standing");

        let (_cache, _tree, copy) = open_copy();
        let listed = |copy: &WorkingCopy| {
            std::fs::read_dir(&copy.state_dir)
                .map(|dir| dir.flatten().map(|e| e.file_name()).collect::<Vec<_>>())
                .unwrap_or_default()
        };
        let standing = listed(&copy);
        assert!(!copy.lay_files(&files, Some("h1"), "h3").unwrap());
        assert!(copy.base().is_none());
        assert_eq!(listed(&copy), standing, "no state created");
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

    /// A collected file is stored by its hash, held or copied from the
    /// folder, and storing it again over the standing content succeeds.
    #[test]
    fn a_collected_file_is_stored_by_its_hash() {
        use crate::push::collector::CollectedFile;
        let (_cache, tree, copy) = open_copy();
        let bytes = [0x89u8, b'P', b'N', b'G', 0x00, 0xff];
        std::fs::write(tree.path().join("a.png"), bytes).unwrap();
        let unheld = CollectedFile {
            sha: blob_sha1(&bytes),
            bytes: None,
        };
        let (sha, created) = copy.store_collected(tree.path(), "a.png", &unheld).unwrap();
        assert!(created);
        assert_eq!(sha, blob_sha1(&bytes));
        let (again, created) = copy.store_collected(tree.path(), "a.png", &unheld).unwrap();
        assert_eq!(again, sha);
        assert!(!created);
        let content = SnapshotContent::Stored { stored: sha };
        assert_eq!(copy.content_bytes(&content).unwrap(), bytes.to_vec());
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

    /// CR1-4: a store a killed run left part-way is removed by the next
    /// prune, so the directory goes with the snapshots.
    #[test]
    fn a_killed_runs_incoming_content_is_pruned() {
        let (_cache, _tree, copy) = open_copy();
        copy.store_bytes(b"kept\n").unwrap();
        std::fs::write(copy.snapshot_content_dir().join(".incoming.1.0"), b"torn").unwrap();
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

    // ---- u298: the two homes ------------------------------------------

    /// A scratch default root, fallback root and folder, with the roots a
    /// run answers where the default root takes writes and where it
    /// refuses them.
    struct Homes {
        _scratch: tempfile::TempDir,
        default: PathBuf,
        root: PathBuf,
        writable: StoreRoots,
        refused: StoreRoots,
    }

    fn homes() -> Homes {
        let scratch = tempfile::tempdir().unwrap();
        let base = std::fs::canonicalize(scratch.path()).unwrap();
        let default = base.join("default");
        let fallback = base.join("fallback");
        let root = base.join("folder");
        std::fs::create_dir_all(&default).unwrap();
        std::fs::create_dir_all(&fallback).unwrap();
        std::fs::create_dir_all(&root).unwrap();
        Homes {
            writable: StoreRoots {
                default: default.clone(),
                write: default.clone(),
                default_refused: false,
            },
            refused: StoreRoots {
                default: default.clone(),
                write: fallback,
                default_refused: true,
            },
            _scratch: scratch,
            default,
            root,
        }
    }

    fn files(pairs: &[(&str, &str)]) -> HashMap<String, String> {
        pairs
            .iter()
            .map(|(p, s)| (p.to_string(), s.to_string()))
            .collect()
    }

    fn commit_of(copy: &WorkingCopy) -> Option<String> {
        copy.base().and_then(|b| b.commit_sha().map(String::from))
    }

    fn set_stamp(home: &Path, value: u128) {
        std::fs::write(home.join(STATE_STAMP), value.to_string()).unwrap();
    }

    #[cfg(unix)]
    fn mode_of(path: &Path) -> u32 {
        use std::os::unix::fs::PermissionsExt;
        std::fs::metadata(path).unwrap().permissions().mode() & 0o777
    }

    /// Whether another open file can take the lock at `path` now.
    fn lock_is_free(path: &Path) -> bool {
        let file = File::open(path).unwrap();
        let free = file.try_lock().is_ok();
        if free {
            file.unlock().unwrap();
        }
        free
    }

    // SPEC u298 Tests, `in_root_home_is_confined_and_ignored_by_git`.
    #[test]
    fn in_root_home_is_confined_and_ignored_by_git() {
        let h = homes();
        let copy = WorkingCopy::open(&h.refused, "alice", "proj", &h.root).unwrap();
        let home = h.root.join(IN_ROOT_HOME);
        assert_eq!(copy.state_dir, home);
        assert!(home.is_dir());
        #[cfg(unix)]
        assert_eq!(mode_of(&home), 0o700);
        assert_eq!(
            std::fs::read_to_string(home.join(".gitignore"))
                .unwrap()
                .lines()
                .collect::<Vec<_>>(),
            vec!["*"]
        );
        assert!(
            !h.default.join("working-copies").exists(),
            "a refused run created the default home"
        );
    }

    // SPEC u298 Tests, `in_root_home_standing_as_a_link_is_refused`.
    #[cfg(unix)]
    #[test]
    fn in_root_home_standing_as_a_link_is_refused() {
        let h = homes();
        let elsewhere = h.default.parent().unwrap().join("elsewhere");
        std::fs::create_dir_all(&elsewhere).unwrap();
        set_stamp(&elsewhere, 9);
        let mut base = Manifest::default();
        base.update("h9".into(), files(&[("a.md", "9")]));
        std::fs::write(
            elsewhere.join(BASE_FILE),
            serde_json::to_vec(&base).unwrap(),
        )
        .unwrap();
        let before = entries_under(&elsewhere);
        let link = h.root.join(IN_ROOT_HOME);
        std::os::unix::fs::symlink(&elsewhere, &link).unwrap();

        match WorkingCopy::open(&h.refused, "alice", "proj", &h.root) {
            Err(CliError::Io { message }) => {
                assert!(
                    message.contains("could not create working copy state"),
                    "{message}"
                )
            }
            other => panic!("expected the refusal, got {other:?}"),
        }
        assert_eq!(std::fs::read_link(&link).unwrap(), elsewhere);
        assert_eq!(entries_under(&elsewhere), before);

        // As the other home, it is read as holding no state.
        let copy = WorkingCopy::open(&h.writable, "alice", "proj", &h.root).unwrap();
        assert_eq!(commit_of(&copy), None);
        let lock = copy.lock().unwrap();
        assert!(!lock.holds_other());
        assert_eq!(commit_of(&copy), None);
    }

    // SPEC u298 Tests, `reads_answer_the_state_stamped_last`.
    #[test]
    fn reads_answer_the_state_stamped_last() {
        let h = homes();
        let write = WorkingCopy::open(&h.writable, "alice", "proj", &h.root).unwrap();
        let other = WorkingCopy::open(&h.refused, "alice", "proj", &h.root).unwrap();
        write.record_base("h1", files(&[("a.md", "1")])).unwrap();
        other.record_base("h2", files(&[("a.md", "2")])).unwrap();
        set_stamp(&write.state_dir, 100);
        set_stamp(&other.state_dir, 200);
        assert_eq!(commit_of(&write).as_deref(), Some("h2"));
        set_stamp(&other.state_dir, 100);
        assert_eq!(commit_of(&write).as_deref(), Some("h1"));
        // Seen from the other side, the tie goes to its own write home.
        assert_eq!(commit_of(&other).as_deref(), Some("h2"));
    }

    // SPEC u298 Contract Surface, `WorkingCopy` reads: a home holding no
    // stamp is ordered before every home holding one, and two holding
    // none by the newest modification time among their state files.
    #[test]
    fn unstamped_homes_are_ordered_by_their_newest_state_file() {
        let h = homes();
        let write = WorkingCopy::open(&h.writable, "alice", "proj", &h.root).unwrap();
        let other = WorkingCopy::open(&h.refused, "alice", "proj", &h.root).unwrap();
        write.record_base("h1", files(&[("a.md", "1")])).unwrap();
        other.record_base("h2", files(&[("a.md", "2")])).unwrap();
        std::fs::remove_file(write.state_dir.join(STATE_STAMP)).unwrap();
        // A stamped home is ordered after an unstamped one.
        assert_eq!(commit_of(&write).as_deref(), Some("h2"));
        std::fs::remove_file(other.state_dir.join(STATE_STAMP)).unwrap();
        let at = |secs: u64| std::time::UNIX_EPOCH + std::time::Duration::from_secs(secs);
        let touch = |path: PathBuf, secs: u64| {
            File::options()
                .write(true)
                .open(path)
                .unwrap()
                .set_modified(at(secs))
                .unwrap();
        };
        touch(write.state_dir.join(BASE_FILE), 2_000);
        touch(other.state_dir.join(BASE_FILE), 1_000);
        assert_eq!(commit_of(&write).as_deref(), Some("h1"));
        touch(other.state_dir.join(BASE_FILE), 3_000);
        assert_eq!(commit_of(&write).as_deref(), Some("h2"));
    }

    // SPEC u298 Tests, `open_existing_finds_state_in_either_home`.
    #[test]
    fn open_existing_finds_state_in_either_home() {
        let h = homes();
        assert_eq!(
            WorkingCopy::open_existing(&h.writable, "alice", "proj", &h.root).unwrap(),
            None
        );
        let other = WorkingCopy::open(&h.refused, "alice", "proj", &h.root).unwrap();
        other.record_base("h2", files(&[("a.md", "2")])).unwrap();
        let found = WorkingCopy::open_existing(&h.writable, "alice", "proj", &h.root)
            .unwrap()
            .expect("the copy whose state stands in its in-root home");
        assert_eq!(commit_of(&found).as_deref(), Some("h2"));
        assert!(
            !found.state_dir.exists(),
            "open_existing created the write home"
        );
        assert!(!h.default.join("working-copies").exists());
    }

    // SPEC u298 Tests, `folder_base_reads_a_holder_base_in_its_default_home`.
    #[test]
    fn folder_base_reads_a_holder_base_in_its_default_home() {
        let h = homes();
        let w = h.root.clone();
        std::fs::create_dir_all(w.join("clients/vela/q3-board")).unwrap();
        let holder = WorkingCopy::open(&h.writable, "alice", "work", &w).unwrap();
        holder
            .record_base("h1", files(&[("clients/vela/q3-board/a.md", "a1")]))
            .unwrap();
        let scope = q3_scope(&w, Some(w.clone()));
        let base = folder_base(&h.refused, &scope).expect("the holder's base");
        assert_eq!(base.commit_sha(), Some("h1"));
        assert_eq!(base.file_paths().collect::<Vec<_>>(), vec!["a.md"]);
        assert_eq!(base.file_sha("a.md"), Some("a1"));
        assert!(!w.join(IN_ROOT_HOME).exists(), "folder_base wrote state");
    }

    /// Hold the lock at `path` through another open file from a thread
    /// for a while, answering when it was released.
    fn hold_from_a_thread(
        path: PathBuf,
        writable: bool,
    ) -> std::thread::JoinHandle<std::time::Instant> {
        let (taken, wait) = std::sync::mpsc::channel();
        let holder = std::thread::spawn(move || {
            let file = OpenOptions::new()
                .read(true)
                .write(writable)
                .open(&path)
                .unwrap();
            file.lock().unwrap();
            taken.send(()).unwrap();
            std::thread::sleep(std::time::Duration::from_millis(400));
            let released = std::time::Instant::now();
            file.unlock().unwrap();
            released
        });
        wait.recv().unwrap();
        holder
    }

    // SPEC u298 Tests, `lock_queues_behind_the_other_homes_lock`.
    #[test]
    fn lock_queues_behind_the_other_homes_lock() {
        let h = homes();
        let sandboxed = WorkingCopy::open(&h.refused, "alice", "proj", &h.root).unwrap();
        drop(sandboxed.lock().unwrap());
        let copy = WorkingCopy::open(&h.writable, "alice", "proj", &h.root).unwrap();
        let in_root_lock = h.root.join(IN_ROOT_HOME).join(LOCK_FILE);
        let holder = hold_from_a_thread(in_root_lock.clone(), true);
        let lock = copy.lock().unwrap();
        let returned = std::time::Instant::now();
        let released = holder.join().unwrap();
        assert!(returned >= released, "the lock did not wait");
        assert!(lock.holds_other());
        assert!(!lock_is_free(&copy.state_dir.join(LOCK_FILE)));
        assert!(!lock_is_free(&in_root_lock));
        drop(lock);
        assert!(lock_is_free(&in_root_lock));
    }

    // SPEC u298 Tests, `lock_queues_behind_a_read_only_default_lock`.
    #[cfg(unix)]
    #[test]
    fn lock_queues_behind_a_read_only_default_lock() {
        use std::os::unix::fs::PermissionsExt;
        if unsafe { libc::geteuid() } == 0 {
            return;
        }
        let h = homes();
        let unsandboxed = WorkingCopy::open(&h.writable, "alice", "proj", &h.root).unwrap();
        drop(unsandboxed.lock().unwrap());
        let default_lock = unsandboxed.state_dir.join(LOCK_FILE);
        std::fs::set_permissions(&default_lock, std::fs::Permissions::from_mode(0o400)).unwrap();
        let copy = WorkingCopy::open(&h.refused, "alice", "proj", &h.root).unwrap();
        let holder = hold_from_a_thread(default_lock.clone(), false);
        let lock = copy.lock().unwrap();
        let returned = std::time::Instant::now();
        let released = holder.join().unwrap();
        assert!(returned >= released, "the lock did not wait");
        assert!(lock.holds_other());
        assert!(!lock_is_free(&default_lock));
        assert!(!lock_is_free(&copy.state_dir.join(LOCK_FILE)));
        drop(lock);
        std::fs::set_permissions(&default_lock, std::fs::Permissions::from_mode(0o600)).unwrap();
    }

    // SPEC u298 Tests, `lock_adopts_an_unstamped_default_home`.
    #[test]
    fn lock_adopts_an_unstamped_default_home() {
        let h = homes();
        let released = WorkingCopy::open(&h.writable, "alice", "proj", &h.root).unwrap();
        released.record_base("h1", files(&[("a.md", "1")])).unwrap();
        std::fs::remove_file(released.state_dir.join(STATE_STAMP)).unwrap();
        assert!(!h.root.join(IN_ROOT_HOME).exists());

        let copy = WorkingCopy::open(&h.refused, "alice", "proj", &h.root).unwrap();
        let lock = copy.lock().unwrap();
        assert_eq!(commit_of(&copy).as_deref(), Some("h1"));
        let adopted: Manifest = serde_json::from_slice(
            &std::fs::read(h.root.join(IN_ROOT_HOME).join(BASE_FILE)).unwrap(),
        )
        .unwrap();
        assert_eq!(adopted.commit_sha(), Some("h1"));
        drop(lock);
        // Adopted once: the homes now tie, and the next lock copies nothing.
        assert!(!copy.other_stamped_later());
    }

    // SPEC u298 Tests, `writable_default_run_creates_no_in_root_home`.
    #[test]
    fn writable_default_run_creates_no_in_root_home() {
        let h = homes();
        let copy = WorkingCopy::open(&h.writable, "alice", "proj", &h.root).unwrap();
        let lock = copy.lock().unwrap();
        assert!(!lock.holds_other());
        assert!(!lock_is_free(&copy.state_dir.join(LOCK_FILE)));
        copy.record_base("h1", files(&[("a.md", "1")])).unwrap();
        drop(lock);
        assert!(!h.root.join(IN_ROOT_HOME).exists());
    }

    // SPEC u298 Tests, `lock_adopts_state_stamped_later`.
    #[test]
    fn lock_adopts_state_stamped_later() {
        let h = homes();
        let write = WorkingCopy::open(&h.writable, "alice", "proj", &h.root).unwrap();
        let other = WorkingCopy::open(&h.refused, "alice", "proj", &h.root).unwrap();
        write.record_base("h1", files(&[("a.md", "1")])).unwrap();
        write
            .write_resolution(&Resolution {
                recovery_id: "r".into(),
                base_commit: Some("h1".into()),
                head_commit: "h0".into(),
                round: 1,
                local_paths: Vec::new(),
                remote_paths: Vec::new(),
                collisions: Vec::new(),
                combined_paths: Vec::new(),
                reviewed_tree: None,
                pending_writes: None,
            })
            .unwrap();
        other.record_base("h2", files(&[("a.md", "2")])).unwrap();
        let outbox = Outbox {
            parent_commit: Some("h2".into()),
            tree: BTreeMap::from([("a.md".into(), "3".into())]),
        };
        other.write_outbox(&outbox).unwrap();
        let (stored, _) = other.store_bytes(b"theirs\n").unwrap();
        let mut snapshot = Snapshot::new();
        snapshot.insert(
            "a.md".into(),
            Some(SnapshotContent::Stored {
                stored: stored.clone(),
            }),
        );
        other.write_local_snapshot(&snapshot).unwrap();
        set_stamp(&write.state_dir, 100);
        set_stamp(&other.state_dir, 200);

        let lock = write.lock().unwrap();
        assert_eq!(commit_of(&write).as_deref(), Some("h2"));
        assert_eq!(write.outbox().unwrap(), Some(outbox));
        assert_eq!(write.resolution().unwrap(), None);
        assert!(!write.state_dir.join(RESOLUTION_FILE).exists());
        assert_eq!(
            std::fs::read_to_string(write.state_dir.join(STATE_STAMP)).unwrap(),
            "200"
        );
        assert_eq!(
            write
                .content_bytes(&SnapshotContent::Stored { stored })
                .unwrap(),
            b"theirs\n"
        );
        for name in [BASE_FILE, OUTBOX_FILE, LOCAL_SNAPSHOT_FILE] {
            let copied = std::fs::metadata(write.state_dir.join(name)).unwrap();
            let source = std::fs::metadata(other.state_dir.join(name)).unwrap();
            assert_eq!(
                copied.modified().unwrap(),
                source.modified().unwrap(),
                "{name}"
            );
        }
        drop(lock);
    }

    // SPEC u298 Tests, `discard_under_one_home_is_not_undone`.
    #[test]
    fn discard_under_one_home_is_not_undone() {
        let h = homes();
        let write = WorkingCopy::open(&h.writable, "alice", "proj", &h.root).unwrap();
        let other = WorkingCopy::open(&h.refused, "alice", "proj", &h.root).unwrap();
        let resolution = Resolution {
            recovery_id: "r".into(),
            base_commit: Some("h1".into()),
            head_commit: "h0".into(),
            round: 1,
            local_paths: Vec::new(),
            remote_paths: Vec::new(),
            collisions: Vec::new(),
            combined_paths: Vec::new(),
            reviewed_tree: None,
            pending_writes: None,
        };
        other.write_resolution(&resolution).unwrap();
        write.write_resolution(&resolution).unwrap();
        set_stamp(&write.state_dir, 100);
        set_stamp(&other.state_dir, 200);
        {
            let _lock = write.lock().unwrap();
            assert_eq!(write.resolution().unwrap(), Some(resolution));
            write.remove_resolution().unwrap();
        }
        {
            let _lock = write.lock().unwrap();
            assert_eq!(write.resolution().unwrap(), None);
        }
        // Nor by a run on the other side of the refused default root.
        let _lock = other.lock().unwrap();
        assert_eq!(other.resolution().unwrap(), None);
        assert!(!other.state_dir.join(RESOLUTION_FILE).exists());
    }
}
