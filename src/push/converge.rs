//! The convergence run, the guarded publication, the continue and the
//! discard, the state reading, and the outcome vocabulary (SPEC u256
//! § Behaviour).
//!
//! Every entry point but `working_copy_state` holds the working copy's
//! state lock for its whole run; the `*_locked` functions below assume
//! it is held and never take it again — the lock is per open file, so a
//! second take from the same process would wait on itself.
//!
//! SPEC u280: a run collects the folder once and publishes from that
//! collection, reads the head's bytes through the raw entry verified by
//! their hash — held within the run's budget or staged outside the
//! folder — and snapshots each content into a file of its own.

use std::borrow::Cow;
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fs::OpenOptions;
use std::io::Write;
use std::path::{Component, Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crate::client::{EntryType, PushResponse, SynsClient};
use crate::errors::{ApiErrorContext, CliError};
use crate::push::collector::{
    CollectOptions, CollectResult, CollectedFile, HELD_BYTES_BUDGET, HeldBytes, Hold,
    MAX_FILE_BYTES, SkippedFile, collect_files, is_text, is_text_reader, read_collected,
    too_large_line,
};
use crate::push::hash::blob_sha1;
use crate::push::reconcile::{
    CONFLICT_MARKERS, CollisionKind, holds_conflict_marker, merge_text, reconcile,
};
use crate::push::smart::{
    PushPipelineMeta, SmartPushOptions, smart_push, strict_refuses, tree_to_sha_map,
};
use crate::push::working_copy::{
    Outbox, Resolution, Snapshot, SnapshotContent, StatRecord, WorkingCopy,
};
use crate::repo::syns_yaml::read_required_checks;

/// The last round a resolution stands at before attention is required:
/// the first candidate and three continuations (Q-03).
pub const ROUND_BOUND: u32 = 4;

/// The wait before the head is read again after a guard refusal at
/// round 1, 2 and 3 (Q-03).
const BACKOFF_SECONDS: [u64; 3] = [2, 8, 30];

/// The most characters a file path holds (`INV-30`, the `file path`
/// field kind).
const FILE_PATH_MAX: usize = 1000;

/// How many times one run re-collects the folder because a writer
/// raced it, before it asks for attention instead.
const MAX_PASSES: usize = 8;

/// The most raw reads a retrieval holds outstanding (`unregistered`).
pub(crate) const READ_CONCURRENCY: usize = 16;

/// The most a retrieval's outstanding raw reads may weigh: their count
/// times the largest tree size among them (`unregistered`, `D-090`).
pub(crate) const READ_BYTES_BOUND: u64 = 67_108_864;

/// What a run is asked to do with the head.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConvergeMode {
    /// Bring the head in. Sends only a pending outbox, and that only
    /// where a credential loaded; only `overwrite` rewrites a local edit.
    Retrieve { overwrite: bool },
    /// Bring the head in and publish the folder past it.
    Publish,
}

/// Where a working copy stands against the head read in the same call.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WorkingCopyState {
    Converged,
    LocalChanges,
    RemoteChanges,
    Diverged,
    ResolutionRequired,
    PublicationPending,
}

impl WorkingCopyState {
    /// The machine-readable value (Q-02).
    pub fn as_key(&self) -> &'static str {
        match self {
            WorkingCopyState::Converged => "converged",
            WorkingCopyState::LocalChanges => "local_changes",
            WorkingCopyState::RemoteChanges => "remote_changes",
            WorkingCopyState::Diverged => "diverged",
            WorkingCopyState::ResolutionRequired => "resolution_required",
            WorkingCopyState::PublicationPending => "publication_pending",
        }
    }

    /// The human rendering.
    pub fn label(&self) -> &'static str {
        match self {
            WorkingCopyState::Converged => "converged",
            WorkingCopyState::LocalChanges => "local changes",
            WorkingCopyState::RemoteChanges => "remote changes",
            WorkingCopyState::Diverged => "diverged",
            WorkingCopyState::ResolutionRequired => "resolution required",
            WorkingCopyState::PublicationPending => "publication pending",
        }
    }
}

/// The one outcome a run renders.
#[derive(Debug)]
pub enum SyncOutcome {
    Synced {
        written: Vec<String>,
        removed: Vec<String>,
        published: Option<(PushResponse, serde_json::Value, PushPipelineMeta)>,
    },
    NoChanges,
    ResolutionRequired(Resolution),
    RetryableFailure(CliError),
    CredentialFailure(CliError),
    ValidationFailure(CliError),
    AttentionRequired(Option<Resolution>),
    NoRepository,
}

// ---- the run's staging (SPEC u280 `Staging`) ---------------------------

#[cfg(unix)]
const STAGING_DIR_MODE: u32 = 0o700;

/// The directory a run stages content in that its budget does not hold:
/// `{cache_dir}/staging/{pid}-{nonce}`, locked through the file
/// `{pid}-{nonce}.lock` beside it for the run's life, and removed with
/// that file when dropped.
#[derive(Debug)]
pub(crate) struct Staging {
    dir: PathBuf,
    lock_path: PathBuf,
    _lock: std::fs::File,
    next: AtomicU64,
}

impl Staging {
    /// Sweep every staging directory no live run holds, then open this
    /// run's own (SPEC u280 `Staging::open` 1–2).
    pub(crate) fn open(cache_dir: &Path) -> Result<Staging, CliError> {
        let root = cache_dir.join("staging");
        Self::sweep(&root);

        let io = |path: &Path, err: std::io::Error| CliError::Io {
            message: format!("could not write {}: {err}", path.display()),
        };
        let mut builder = std::fs::DirBuilder::new();
        builder.recursive(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::DirBuilderExt;
            builder.mode(STAGING_DIR_MODE);
        }
        builder.create(&root).map_err(|err| io(&root, err))?;

        let prefix = format!("{}-", std::process::id());
        let mut named = tempfile::Builder::new();
        named.prefix(&prefix).suffix(".lock").rand_bytes(12);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            named.permissions(std::fs::Permissions::from_mode(0o600));
        }
        let (lock, lock_path) = named
            .tempfile_in(&root)
            .and_then(|file| file.keep().map_err(|err| err.error))
            .map_err(|err| io(&root, err))?;
        lock.lock().map_err(|err| io(&lock_path, err))?;
        let dir = lock_path.with_extension("");
        if let Err(err) = builder.recursive(false).create(&dir) {
            let _ = std::fs::remove_file(&lock_path);
            return Err(io(&dir, err));
        }
        Ok(Staging {
            dir,
            lock_path,
            _lock: lock,
            next: AtomicU64::new(0),
        })
    }

    /// Remove every directory under `root` whose sibling lock no live
    /// process holds, taking that lock first; one it cannot remove is
    /// left for a later run.
    fn sweep(root: &Path) {
        let Ok(entries) = std::fs::read_dir(root) else {
            return;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if !entry.file_type().is_ok_and(|t| t.is_dir()) {
                continue;
            }
            let lock_path = path.with_extension("lock");
            let lock = OpenOptions::new()
                .read(true)
                .write(true)
                .create(true)
                .truncate(false)
                .open(&lock_path);
            let Ok(lock) = lock else {
                continue;
            };
            if lock.try_lock().is_err() {
                continue;
            }
            if std::fs::remove_dir_all(&path).is_ok() {
                drop(lock);
                let _ = std::fs::remove_file(&lock_path);
            }
        }
    }

    /// The next file this run stages into, named by its sequence number.
    pub(crate) fn next_path(&self) -> PathBuf {
        self.dir
            .join(self.next.fetch_add(1, Ordering::Relaxed).to_string())
    }
}

impl Drop for Staging {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
        let _ = std::fs::remove_file(&self.lock_path);
    }
}

/// Write `bytes` into a fresh staged file reachable by the person's own
/// account alone.
fn stage_bytes(staging: &Staging, path: &str, bytes: &[u8]) -> Result<PathBuf, CliError> {
    let dest = staging.next_path();
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let written = options.open(&dest).and_then(|mut f| f.write_all(bytes));
    if let Err(err) = written {
        let _ = std::fs::remove_file(&dest);
        return Err(CliError::Io {
            message: format!("could not write {path}: {err}"),
        });
    }
    Ok(dest)
}

// ---- content held or staged (SPEC u280 `Blob`) -------------------------

/// Content either held and counted against the run's budget until
/// dropped, or standing in a file — a staged answer's under the run's
/// `Staging`, a stored snapshot content's under the state directory.
#[derive(Debug)]
pub(crate) enum Blob {
    /// The bytes, and the hold that counts them until the blob drops.
    Held(Vec<u8>, #[allow(dead_code)] Hold),
    Staged(PathBuf),
}

impl Blob {
    /// What `is_text` answers over the content, one piece at a time
    /// where it stands in a file.
    fn is_text(&self) -> Result<bool, CliError> {
        match self {
            Blob::Held(bytes, _) => Ok(is_text(bytes)),
            Blob::Staged(path) => std::fs::File::open(path)
                .and_then(is_text_reader)
                .map_err(|err| read_failed(path, err)),
        }
    }

    /// The whole content, for the one text merge being computed.
    fn load(&self) -> Result<Cow<'_, [u8]>, CliError> {
        match self {
            Blob::Held(bytes, _) => Ok(Cow::Borrowed(bytes)),
            Blob::Staged(path) => std::fs::read(path)
                .map(Cow::Owned)
                .map_err(|err| read_failed(path, err)),
        }
    }

    /// Store the content as a snapshot content of `copy`.
    fn store(&self, copy: &WorkingCopy) -> Result<(String, bool), CliError> {
        match self {
            Blob::Held(bytes, _) => copy.store_bytes(bytes),
            Blob::Staged(path) => copy.store_file(path),
        }
    }
}

fn read_failed(path: &Path, err: std::io::Error) -> CliError {
    CliError::Io {
        message: format!("could not read {}: {err}", path.display()),
    }
}

// ---- reading the head ------------------------------------------------

#[derive(Debug, Clone, Default)]
struct Head {
    commit: Option<String>,
    files: BTreeMap<String, String>,
    /// Each file's size as the tree answers it, none where it answers
    /// none.
    sizes: BTreeMap<String, Option<u64>>,
    truncated: bool,
}

impl Head {
    fn wanted(&self, path: &str) -> Option<(String, Option<u64>)> {
        self.files
            .get(path)
            .map(|hash| (hash.clone(), self.sizes.get(path).copied().flatten()))
    }
}

/// Which refusals of an `EP-tree` read stand for an empty head.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum HeadReading {
    Retrieval,
    Publication,
    State,
}

fn repo_id(copy: &WorkingCopy) -> String {
    format!("{}/{}", copy.owner, copy.name)
}

async fn read_tree(
    client: &SynsClient,
    token: Option<&str>,
    copy: &WorkingCopy,
    at: Option<&str>,
) -> Result<Head, CliError> {
    let (tree, _raw) = client
        .get_tree(&repo_id(copy), token, None, true, at)
        .await?;
    let sizes = tree
        .entries
        .iter()
        .filter(|e| e.entry_type == EntryType::File && e.sha.is_some())
        .map(|e| (e.path.clone(), e.size))
        .collect();
    Ok(Head {
        commit: Some(tree.commit_sha.clone()).filter(|c| !c.is_empty()),
        files: tree_to_sha_map(&tree).into_iter().collect(),
        sizes,
        truncated: tree.truncated,
    })
}

fn is_empty_repository(err: &CliError) -> bool {
    matches!(err, CliError::Api { status: Some(422), error, .. } if error == "validation_error")
}

fn is_repository_not_found(err: &CliError) -> bool {
    matches!(
        err,
        CliError::Api {
            status: Some(404),
            ..
        }
    )
}

/// Read the head at the tip (`converge` 5): where no base is recorded,
/// the empty repository's `VALIDATION_ERROR` — and on a publication a
/// repository not found — read as an empty head with no commit; where a
/// base is recorded both stop the run.
async fn read_head(
    client: &SynsClient,
    token: Option<&str>,
    copy: &WorkingCopy,
    reading: HeadReading,
    has_base: bool,
) -> Result<Head, CliError> {
    match read_tree(client, token, copy, None).await {
        Ok(head) => Ok(head),
        Err(err)
            if reading == HeadReading::State && is_empty_repository(&err)
                || !has_base
                    && (is_empty_repository(&err)
                        || reading == HeadReading::Publication
                            && is_repository_not_found(&err)) =>
        {
            Ok(Head::default())
        }
        Err(err) => Err(err),
    }
}

fn reading_for(mode: ConvergeMode) -> HeadReading {
    match mode {
        ConvergeMode::Retrieve { .. } => HeadReading::Retrieval,
        ConvergeMode::Publish => HeadReading::Publication,
    }
}

// ---- the retrieval's reads (SPEC u280 `read_blobs`) ----------------------

/// Read each wanted path's content at `at` through the raw entry, every
/// answer hashing to the tree hash `wanted` names for it: held where
/// `held` admits it, staged into `staging` otherwise. A read goes out
/// only while the reads outstanding with it number at most
/// `READ_CONCURRENCY` and their count times the largest size among them
/// stays within `READ_BYTES_BOUND`, a `null` size counted as
/// `MAX_FILE_BYTES`; a read with none outstanding always goes out. On
/// the first refusal the rest are abandoned and every file staged here
/// removed, nothing written.
pub(crate) async fn read_blobs(
    client: &SynsClient,
    token: Option<&str>,
    repo_id: &str,
    at: &str,
    wanted: &BTreeMap<String, (String, Option<u64>)>,
    held: &Arc<HeldBytes>,
    staging: &Staging,
) -> Result<BTreeMap<String, Blob>, CliError> {
    // 1 — every path checked before any request.
    check_server_paths(wanted.keys())?;

    let mut staged: Vec<PathBuf> = Vec::new();
    let mut set: tokio::task::JoinSet<Result<(String, u64, Blob), CliError>> =
        tokio::task::JoinSet::new();
    let mut outstanding: Vec<u64> = Vec::new();
    let mut answers: BTreeMap<String, Blob> = BTreeMap::new();

    let result: Result<(), CliError> = async {
        for (path, (hash, size)) in wanted {
            let counted = size.unwrap_or(MAX_FILE_BYTES);
            // 2 — admission by count and by size.
            loop {
                let heaviest = outstanding.iter().copied().max().unwrap_or(0).max(counted);
                let count = outstanding.len() as u64 + 1;
                if outstanding.is_empty()
                    || (outstanding.len() < READ_CONCURRENCY
                        && count.saturating_mul(heaviest) <= READ_BYTES_BOUND)
                {
                    break;
                }
                let (done, weight, blob) = join_one(&mut set).await?;
                forget_weight(&mut outstanding, weight);
                answers.insert(done, blob);
            }
            outstanding.push(counted);
            let hold = held.try_hold(counted);
            let dest = if hold.is_none() {
                let dest = staging.next_path();
                staged.push(dest.clone());
                Some(dest)
            } else {
                None
            };
            let client = client.clone();
            let token = token.map(str::to_string);
            let repo_id = repo_id.to_string();
            let at = at.to_string();
            let path = path.clone();
            let hash = hash.clone();
            let held = held.clone();
            set.spawn(async move {
                let blob = read_one(
                    &client,
                    token.as_deref(),
                    &repo_id,
                    &at,
                    &path,
                    &hash,
                    hold,
                    counted,
                    dest,
                    &held,
                )
                .await?;
                Ok((path, counted, blob))
            });
        }
        while !set.is_empty() {
            let (done, weight, blob) = join_one(&mut set).await?;
            forget_weight(&mut outstanding, weight);
            answers.insert(done, blob);
        }
        Ok(())
    }
    .await;

    match result {
        Ok(()) => Ok(answers),
        Err(err) => {
            set.abort_all();
            while set.join_next().await.is_some() {}
            drop(answers);
            for path in staged {
                let _ = std::fs::remove_file(path);
            }
            Err(err)
        }
    }
}

fn forget_weight(outstanding: &mut Vec<u64>, weight: u64) {
    if let Some(at) = outstanding.iter().position(|w| *w == weight) {
        outstanding.swap_remove(at);
    }
}

async fn join_one(
    set: &mut tokio::task::JoinSet<Result<(String, u64, Blob), CliError>>,
) -> Result<(String, u64, Blob), CliError> {
    match set.join_next().await {
        Some(Ok(answer)) => answer,
        Some(Err(err)) => Err(CliError::Io {
            message: format!("a raw read did not finish: {err}"),
        }),
        None => Err(CliError::Io {
            message: "a raw read was lost".to_string(),
        }),
    }
}

/// One raw read, held under `hold` or staged into `dest`, verified
/// against `hash` (SPEC u280 `read_blobs` 2–3).
#[allow(clippy::too_many_arguments)]
async fn read_one(
    client: &SynsClient,
    token: Option<&str>,
    repo_id: &str,
    at: &str,
    path: &str,
    hash: &str,
    hold: Option<Hold>,
    counted: u64,
    dest: Option<PathBuf>,
    held: &Arc<HeldBytes>,
) -> Result<Blob, CliError> {
    match (hold, dest) {
        (Some(hold), _) => {
            // The answer is read into a buffer of the size its hold
            // counted, allocated before its first byte arrives (`D-094`).
            let capacity = usize::try_from(counted).unwrap_or(usize::MAX);
            let raw = client
                .get_raw(repo_id, token, path, Some(at), Some(capacity))
                .await?;
            let actual = blob_sha1(&raw.bytes);
            if actual != hash {
                return Err(crate::client::hash_mismatch(path, hash, &actual));
            }
            let mut bytes = raw.bytes;
            let len = bytes.len() as u64;
            // A buffer sized for more than arrived — a `null` tree size
            // counted as `MAX_FILE_BYTES` — gives the rest back with it.
            if len < counted {
                bytes.shrink_to_fit();
            }
            // The hold is fitted to what arrived: the part past it given
            // back, or the rest taken where more arrived than counted.
            let hold = if len == counted {
                hold
            } else {
                match held.try_hold(len) {
                    Some(fitted) => {
                        drop(hold);
                        fitted
                    }
                    None if len < counted => hold,
                    None => {
                        return Err(CliError::Io {
                            message: format!(
                                "could not read {path}: the answer is larger than the tree names"
                            ),
                        });
                    }
                }
            };
            Ok(Blob::Held(bytes, hold))
        }
        (None, Some(dest)) => {
            let staged = client
                .get_raw_staged(repo_id, token, path, Some(at), &dest)
                .await?;
            if staged.sha != hash {
                return Err(crate::client::hash_mismatch(path, hash, &staged.sha));
            }
            Ok(Blob::Staged(dest))
        }
        (None, None) => Err(CliError::Io {
            message: format!("could not read {path}: no room was made for it"),
        }),
    }
}

// ---- the folder ------------------------------------------------------

/// One collection of the folder: each kept path's hash, the collected
/// files, and what the collection dropped.
struct Folder {
    hashes: BTreeMap<String, String>,
    files: HashMap<String, CollectedFile>,
    skipped: Vec<SkippedFile>,
    total_walked: usize,
}

impl Folder {
    /// Give back every byte the collection holds, each file keeping its
    /// hash (SPEC u280 `converge` 4).
    fn drop_bytes(&mut self) {
        for file in self.files.values_mut() {
            file.drop_bytes();
        }
    }

    fn remove(&mut self, path: &str) {
        self.hashes.remove(path);
        self.files.remove(path);
    }

    /// The collection a publication takes as its files and its drops.
    fn into_collected(self) -> CollectResult {
        CollectResult {
            files: self.files,
            skipped: self.skipped,
            total_walked: self.total_walked,
        }
    }
}

/// Collect the folder (SPEC u280 `converge` 1): on macOS and Linux through
/// the working copy's stat record, each path in `forget` read again
/// whatever its entry says. A collection taken under the state lock
/// writes the record back with a fresh stamp where it changed an entry or
/// read a file whose entry stood too recent to trust; a refused record
/// write leaves the record as it stood.
fn collect_folder(
    copy: &WorkingCopy,
    opts: &SmartPushOptions,
    forget: &[String],
    under_lock: bool,
) -> Result<Folder, CliError> {
    let held = opts.held_bytes();
    #[cfg(unix)]
    let mut record: Option<StatRecord> = Some(copy.stat_record());
    #[cfg(not(unix))]
    let mut record: Option<StatRecord> = None;
    if let Some(record) = record.as_mut() {
        for path in forget {
            record.entries.remove(path);
        }
    }
    let loaded = record.clone();
    let collected = collect_files(
        &copy.root,
        &opts.excludes,
        CollectOptions {
            no_default_excludes: opts.no_default_excludes,
            debug: opts.debug,
            prefix: None,
        },
        record.as_mut(),
        &held,
    )?;
    #[cfg(unix)]
    if under_lock
        && let Some(mut record) = record
        && (Some(&record.entries) != loaded.as_ref().map(|r| &r.entries)
            || record.holds_untrusted())
    {
        record.restamp(&copy.root);
        let _ = copy.write_stat_record(&record);
    }
    #[cfg(not(unix))]
    let _ = (under_lock, loaded);

    if opts.strict && strict_refuses(&collected.skipped) {
        return Err(CliError::PushPartial {
            skipped: collected.skipped,
            no_default_excludes: opts.no_default_excludes,
        });
    }
    // A write sibling standing at collection is a killed run's: every
    // caller holds the state lock, so no live write owns it.
    let CollectResult {
        mut files,
        skipped,
        total_walked,
    } = collected;
    for stray in files
        .keys()
        .filter(|path| is_partial_write(path))
        .cloned()
        .collect::<Vec<_>>()
    {
        if under_lock {
            remove_folder_file(copy, &stray)?;
        }
        files.remove(&stray);
    }
    let hashes = files
        .iter()
        .map(|(path, file)| (path.clone(), file.sha.clone()))
        .collect();
    Ok(Folder {
        hashes,
        files,
        skipped,
        total_walked,
    })
}

#[derive(Debug, Clone, Default)]
struct Base {
    commit: Option<String>,
    files: BTreeMap<String, String>,
}

fn load_base(copy: &WorkingCopy) -> Base {
    match copy.base() {
        Some(manifest) => Base {
            commit: manifest.commit_sha().map(String::from),
            files: manifest
                .file_paths()
                .filter_map(|p| manifest.file_sha(p).map(|s| (p.to_string(), s.to_string())))
                .collect(),
        },
        None => Base::default(),
    }
}

fn to_hash_map(map: &BTreeMap<String, String>) -> HashMap<String, String> {
    map.iter().map(|(k, v)| (k.clone(), v.clone())).collect()
}

/// Refuse a server-named path breaking `INV-30` before any byte of it
/// reaches the disk: past the length bound, carrying a non-printable
/// character or surrounding whitespace, rooted, or holding a `.`, `..`
/// or `.git` segment under either separator.
pub fn check_server_path(path: &str) -> Result<(), CliError> {
    let refuse = || {
        Err(CliError::Io {
            message: format!("refusing a server path breaking the file path constraint: {path:?}"),
        })
    };
    let length = path.chars().count();
    if length == 0
        || length > FILE_PATH_MAX
        || path.trim() != path
        || path.starts_with('/')
        || path.starts_with('\\')
        || path.chars().any(char::is_control)
    {
        return refuse();
    }
    if path
        .split(['/', '\\'])
        .any(|seg| seg == "." || seg == ".." || seg.eq_ignore_ascii_case(".git"))
    {
        return refuse();
    }
    if Path::new(path)
        .components()
        .any(|c| !matches!(c, Component::Normal(_)))
    {
        return refuse();
    }
    Ok(())
}

fn check_server_paths<'a>(paths: impl IntoIterator<Item = &'a String>) -> Result<(), CliError> {
    paths.into_iter().try_for_each(|p| check_server_path(p))
}

/// Whether a file stands at `path` in the folder: a folder standing
/// where the path names a file holds no file there.
fn file_stands(copy: &WorkingCopy, path: &str) -> bool {
    copy.root.join(path).is_file()
}

/// The blob hash of the file standing at `path`, read in pieces, none
/// where no file stands there.
fn disk_hash(copy: &WorkingCopy, path: &str) -> Result<Option<String>, CliError> {
    let target = copy.root.join(path);
    if !target.is_file() {
        return Ok(None);
    }
    let mut sink = std::io::sink();
    match hash_copying(&target, &mut sink) {
        Ok(sha) => Ok(Some(sha)),
        Err(err)
            if matches!(
                err.kind(),
                std::io::ErrorKind::NotFound | std::io::ErrorKind::NotADirectory
            ) =>
        {
            Ok(None)
        }
        Err(err) => Err(CliError::Io {
            message: format!("could not read {path}: {err}"),
        }),
    }
}

/// Copy `source` into `dest` in pieces, answering the blob hash of what
/// was copied.
fn hash_copying(source: &Path, dest: &mut impl Write) -> std::io::Result<String> {
    let from = std::fs::File::open(source)?;
    let declared = from.metadata()?.len();
    crate::push::hash::hash_pieces(from, declared, dest)?
        .ok_or_else(|| std::io::Error::other("the file changed while it was read"))
}

/// The file-name prefix of the sibling a folder write lands through.
const PARTIAL_PREFIX: &str = ".syns-partial-";

/// Whether a folder path names the sibling of a write a killed run left
/// part-way through: `PARTIAL_PREFIX` and the 16 lowercase hex characters
/// `replace_file_whole` mints, and no other name.
pub(crate) fn is_partial_write(path: &str) -> bool {
    let name = path.rsplit('/').next().unwrap_or(path);
    name.strip_prefix(PARTIAL_PREFIX).is_some_and(|minted| {
        minted.len() == 16
            && minted
                .bytes()
                .all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
    })
}

/// Replace a folder file whole: write its sibling, then rename it over the
/// target, so a run killed part-way leaves the target as it stood and at
/// worst the sibling, which the next collection sweeps. A target this
/// process may not write is refused, as an in-place write would be, and
/// its permissions carry over. A held content is written as it stands, a
/// staged or stored one copied in pieces (SPEC u280 `replace_file_whole`).
pub(crate) fn replace_file_whole(root: &Path, path: &str, content: &Blob) -> Result<(), CliError> {
    let target = root.join(path);
    let io = |err: std::io::Error| CliError::Io {
        message: format!("could not write {path}: {err}"),
    };
    // 1 — a folder at the target holding folders alone gives way to the
    // file; one still holding a file refuses it, naming what it holds.
    if target.is_dir() && !target.is_symlink() && !remove_empty_folders(&target).map_err(io)? {
        return Err(left_out_refusal(root, path, &target));
    }

    // 2 — the sibling, then the rename.
    let parent = target.parent().unwrap_or(root);
    std::fs::create_dir_all(parent).map_err(io)?;
    let file_name = target
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_default();
    let sibling = parent.join(format!(
        "{PARTIAL_PREFIX}{}",
        &blob_sha1(file_name.as_bytes())[..16]
    ));

    let written = (|| -> std::io::Result<()> {
        let permissions = match OpenOptions::new().write(true).open(&target) {
            Ok(file) => Some(file.metadata()?.permissions()),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => None,
            Err(err) => return Err(err),
        };
        let mut file = OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .open(&sibling)?;
        match content {
            Blob::Held(bytes, _) => file.write_all(bytes)?,
            Blob::Staged(source) => {
                let mut from = std::fs::File::open(source)?;
                std::io::copy(&mut from, &mut file)?;
            }
        }
        if let Some(permissions) = permissions {
            file.set_permissions(permissions)?;
        }
        drop(file);
        std::fs::rename(&sibling, &target)
    })();
    if let Err(err) = written {
        let _ = std::fs::remove_file(&sibling);
        return Err(io(err));
    }
    Ok(())
}

/// Remove every folder under `dir` holding no file, deepest first, and
/// `dir` itself where that leaves it empty; answers whether it did.
fn remove_empty_folders(dir: &Path) -> std::io::Result<bool> {
    let mut emptied = true;
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        if entry.file_type()?.is_dir() {
            emptied &= remove_empty_folders(&entry.path())?;
        } else {
            emptied = false;
        }
    }
    if emptied {
        std::fs::remove_dir(dir)?;
    }
    Ok(emptied)
}

/// The left-out refusal (SPEC u280 Contract Surface): the folder at
/// `path` still holds entries no retrieval removes.
fn left_out_refusal(root: &Path, path: &str, target: &Path) -> CliError {
    let mut entries: Vec<String> = std::fs::read_dir(target)
        .into_iter()
        .flatten()
        .flatten()
        .map(|entry| {
            let name = entry.file_name().to_string_lossy().to_string();
            let folder = entry.file_type().is_ok_and(|t| t.is_dir());
            let relative = entry
                .path()
                .strip_prefix(root)
                .ok()
                .and_then(crate::repo::root::to_forward_slash)
                .unwrap_or_else(|| format!("{path}/{name}"));
            if folder {
                format!("{relative}/")
            } else {
                relative
            }
        })
        .collect();
    entries.sort();
    CliError::LeftOut {
        path: path.to_string(),
        entries,
    }
}

fn remove_folder_file(copy: &WorkingCopy, path: &str) -> Result<(), CliError> {
    let target = copy.root.join(path);
    match std::fs::remove_file(&target) {
        Ok(()) => Ok(()),
        // A folder standing where the path names a file holds no file
        // there.
        Err(err)
            if matches!(
                err.kind(),
                std::io::ErrorKind::NotFound | std::io::ErrorKind::NotADirectory
            ) || target.is_dir() =>
        {
            Ok(())
        }
        Err(err) => Err(CliError::Io {
            message: format!("could not remove {path}: {err}"),
        }),
    }
}

/// Whether removing `removed` clears the way for writing `written`: a file
/// standing where the write needs a folder, or a file inside a folder
/// standing where the write puts a file.
fn clears_way(removed: &str, written: &str) -> bool {
    let inside = |inner: &str, outer: &str| {
        inner
            .strip_prefix(outer)
            .is_some_and(|rest| rest.starts_with('/'))
    };
    inside(written, removed) || inside(removed, written)
}

/// Split a set of removals into those clearing the way for one of `writes`,
/// taken before any write, and the rest, taken after every write.
fn clearing_first<'a>(
    written: &[&String],
    removals: &'a [String],
) -> (Vec<&'a String>, Vec<&'a String>) {
    removals
        .iter()
        .partition(|removed| written.iter().any(|written| clears_way(removed, written)))
}

/// What holds the place of a head file a candidate is to write.
enum InTheWay {
    /// A folder at the file's path, holding these files the candidate
    /// keeps, sorted.
    Folder(Vec<String>),
    /// A file the candidate keeps, at this folder above the file's path.
    File(String),
}

/// Whether local work the candidate keeps — every `collected` path but those
/// in `removed` — stands where the head file `path` is to be written: a file
/// at a folder above it, or a folder at it holding any file. Only the
/// collection decides: a file it leaves out is no work a publication could
/// carry, so it withholds no head file, and the write refuses on it instead.
fn local_work_in_the_way(
    path: &str,
    collected: &BTreeMap<String, String>,
    removed: &BTreeSet<String>,
) -> Option<InTheWay> {
    let kept =
        |candidate: &String| collected.contains_key(candidate) && !removed.contains(candidate);
    let mut above = String::new();
    let segments: Vec<&str> = path.split('/').collect();
    for segment in &segments[..segments.len().saturating_sub(1)] {
        if !above.is_empty() {
            above.push('/');
        }
        above.push_str(segment);
        if kept(&above) {
            return Some(InTheWay::File(above));
        }
    }

    let inside = format!("{path}/");
    let files: Vec<String> = collected
        .range(inside.clone()..)
        .map(|(file, _)| file)
        .take_while(|file| file.starts_with(&inside))
        .filter(|file| kept(file))
        .cloned()
        .collect();
    (!files.is_empty()).then_some(InTheWay::Folder(files))
}

/// Remove a folder file that clears the way for a write, and every folder
/// above it the removal leaves empty, so a file can take a folder's place.
fn remove_clearing(copy: &WorkingCopy, path: &str) -> Result<(), CliError> {
    remove_folder_file(copy, path)?;
    let mut folder = copy.root.join(path);
    while folder.pop() && folder != copy.root && folder.starts_with(&copy.root) {
        if std::fs::remove_dir(&folder).is_err() {
            break;
        }
    }
    Ok(())
}

fn mint_recovery_id(copy: &WorkingCopy) -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or_default();
    let seed = format!(
        "{nanos}-{}-{}",
        std::process::id(),
        copy.state_dir.display()
    );
    blob_sha1(seed.as_bytes())[..16].to_string()
}

fn union_sorted(into: &mut Vec<String>, more: impl IntoIterator<Item = String>) {
    let mut set: BTreeSet<String> = into.drain(..).collect();
    set.extend(more);
    into.extend(set);
}

fn differing_paths(a: &BTreeMap<String, String>, b: &BTreeMap<String, String>) -> Vec<String> {
    a.keys()
        .chain(b.keys())
        .filter(|p| a.get(*p) != b.get(*p))
        .cloned()
        .collect::<BTreeSet<String>>()
        .into_iter()
        .collect()
}

/// The paths among `names` the collection left out although a file stands
/// there on disk — files an exclusion keeps out. A convergence writes over
/// none of them, removes none of them and publishes a deletion of none of
/// them, so each is dropped from both sides before they are compared.
fn excluded_on_disk<'a>(
    copy: &WorkingCopy,
    collected: &BTreeMap<String, String>,
    names: impl IntoIterator<Item = &'a String>,
) -> BTreeSet<String> {
    excluded_local_files(&copy.root, |path| collected.contains_key(path), names)
}

/// `excluded_on_disk` under any root, `collected` answering whether a
/// collection there took a path — the test a retrieval at a version applies
/// before it writes.
pub(crate) fn excluded_local_files<'a>(
    root: &Path,
    collected: impl Fn(&str) -> bool,
    names: impl IntoIterator<Item = &'a String>,
) -> BTreeSet<String> {
    names
        .into_iter()
        .filter(|path| {
            !collected(path) && check_server_path(path).is_ok() && root.join(path).is_file()
        })
        .cloned()
        .collect()
}

/// The identity file standing directly at a working copy's root.
const ROOT_IDENTITY: &str = ".syns.yaml";

/// Whether a retrieval holds the root identity file out of every
/// comparison, write and removal of its run (SPEC u263 `ConvergeMode`):
/// only under `Retrieve`, only where the collected folder holds the file,
/// and not where the folder holds it unedited against the base while the
/// head carries other content — that head edit is taken like any other.
fn holds_root_identity(
    retrieving: bool,
    base_files: &BTreeMap<String, String>,
    folder: &Folder,
    head: &Head,
) -> bool {
    let Some(local) = folder.hashes.get(ROOT_IDENTITY) else {
        return false;
    };
    let head_edited_an_unedited_file = base_files.get(ROOT_IDENTITY) == Some(local)
        && head
            .files
            .get(ROOT_IDENTITY)
            .is_some_and(|remote| remote != local);
    retrieving && !head_edited_an_unedited_file
}

/// Drop the root identity file from a collected folder, so the exclusion
/// test that follows counts it as a file standing on disk that the
/// comparison leaves alone.
fn hold_root_identity(folder: &mut Folder) {
    folder.remove(ROOT_IDENTITY);
}

fn without(
    map: &BTreeMap<String, String>,
    excluded: &BTreeSet<String>,
) -> BTreeMap<String, String> {
    map.iter()
        .filter(|(path, _)| !excluded.contains(*path))
        .map(|(path, hash)| (path.clone(), hash.clone()))
        .collect()
}

/// A folder path's prior content snapshotted as a content file of its
/// own: a collected path through `copy_collected`, any other copied as it
/// stands, none where no file stands (SPEC u280 `converge` 5). Answers the
/// snapshot entry and the hash, and records each content file this call
/// created in `created`.
fn snapshot_prior(
    copy: &WorkingCopy,
    folder: &Folder,
    path: &str,
    created: &mut Vec<String>,
) -> Result<Option<SnapshotContent>, CliError> {
    let stored = match folder.files.get(path) {
        Some(file) => Some(copy.store_collected(&copy.root, path, file)?),
        None if file_stands(copy, path) => Some(copy.store_file(&copy.root.join(path))?),
        None => None,
    };
    Ok(stored.map(|(sha, fresh)| {
        if fresh {
            created.push(sha.clone());
        }
        SnapshotContent::Stored { stored: sha }
    }))
}

/// The hash a folder path held before a pass touches it: the collection's,
/// or the file's standing on disk.
fn prior_hash(copy: &WorkingCopy, folder: &Folder, path: &str) -> Result<Option<String>, CliError> {
    match folder.hashes.get(path) {
        Some(hash) => Ok(Some(hash.clone())),
        None => disk_hash(copy, path),
    }
}

/// Remove every content file a failed step stored.
fn remove_created(copy: &WorkingCopy, created: &[String]) {
    for sha in created {
        copy.remove_stored(sha);
    }
}

// ---- preparing a candidate (`converge` 10 to 12) ---------------------

enum Prepared {
    Synced {
        written: Vec<String>,
        removed: Vec<String>,
    },
    Resolution(Resolution),
    Attention(Option<Resolution>),
}

struct Candidate<'a> {
    base_commit: Option<String>,
    base_files: BTreeMap<String, String>,
    head: &'a Head,
    publishing: bool,
    /// The resolution a recomputation keeps the recovery id and round of.
    existing: Option<Resolution>,
    /// Write a resolution whatever the reconciliation finds
    /// (`publish_reviewed` 8).
    force_resolution: bool,
    /// Whether this run holds the root identity file out (u263), decided
    /// once and applied to every collection; `None` leaves the first pass
    /// to decide it from its own collection.
    hold_root_identity: Option<bool>,
}

/// Where a pass's write takes its bytes from.
enum WriteSource {
    /// The head's content read for the path.
    Head,
    /// A text merge's result.
    Merged(Blob),
}

/// Merge an all-text collision (SPEC u280 `converge` 5, `D-092`): the
/// sides loaded whole and held under no hold while the merge runs, the
/// result then kept under a hold `held` admits or staged. `None` where
/// some side is not text, so the local bytes stand as the candidate.
#[allow(clippy::too_many_arguments)]
fn merge_collision(
    copy: &WorkingCopy,
    path: &str,
    local: &CollectedFile,
    head: &Blob,
    base: Option<&Blob>,
    held: &Arc<HeldBytes>,
    staging: &Staging,
) -> Result<Option<(Blob, String)>, CliError> {
    let local_path = copy.root.join(path);
    let local_is_text = std::fs::File::open(&local_path)
        .and_then(is_text_reader)
        .map_err(|_| CliError::CollectedSetChanged {
            paths: vec![path.to_string()],
        })?;
    if !local_is_text || !head.is_text()? || !base.map_or(Ok(true), Blob::is_text)? {
        return Ok(None);
    }
    let merged = {
        let local = read_collected(&copy.root, path, local)?;
        let remote = head.load()?;
        let base = match base {
            Some(base) => base.load()?,
            None => Cow::Borrowed(&[][..]),
        };
        let as_text = |bytes: &[u8]| -> Result<String, CliError> {
            std::str::from_utf8(bytes).map(str::to_string).map_err(|_| {
                CliError::CollectedSetChanged {
                    paths: vec![path.to_string()],
                }
            })
        };
        let (merged, _marked) = merge_text(&as_text(&base)?, &as_text(&local)?, &as_text(&remote)?);
        merged.into_bytes()
    };
    let hash = blob_sha1(&merged);
    let blob = match held.try_hold(merged.len() as u64) {
        Some(hold) => Blob::Held(merged, hold),
        None => Blob::Staged(stage_bytes(staging, path, &merged)?),
    };
    Ok(Some((blob, hash)))
}

#[allow(clippy::too_many_arguments)]
async fn prepare_candidate(
    client: &SynsClient,
    token: Option<&str>,
    copy: &WorkingCopy,
    opts: &SmartPushOptions,
    staging: &Staging,
    candidate: Candidate<'_>,
    first_folder: Option<Folder>,
) -> Result<Prepared, CliError> {
    let head = candidate.head;
    let head_commit = head.commit.clone().unwrap_or_default();
    check_server_paths(head.files.keys())?;
    let held = opts.held_bytes();

    let mut resolution = candidate.existing.clone();
    let mut written_this_run: HashMap<String, String> = HashMap::new();
    let mut written: BTreeSet<String> = BTreeSet::new();
    let mut removed: BTreeSet<String> = BTreeSet::new();
    let mut local_paths: BTreeSet<String> = BTreeSet::new();
    let mut remote_paths: BTreeSet<String> = BTreeSet::new();
    let mut collisions: BTreeMap<String, CollisionKind> = BTreeMap::new();
    let mut resumed_combined: BTreeSet<String> = BTreeSet::new();
    let mut first_folder = first_folder;
    let mut hold = candidate.hold_root_identity;
    // Paths a pass refused on as changed since its collection, their
    // record entries dropped before the next collection (`D-093`).
    let mut forget: Vec<String> = Vec::new();
    // Whether a pass of this run wrote the snapshot documents, and the
    // paths whose local snapshot a pass of this run took and then left
    // untouched, which the next pass takes afresh (`D-093`).
    let mut snapshots_written = false;
    let mut untouched: BTreeSet<String> = BTreeSet::new();

    // A preparation resumed over a resolution a killed or failed run left
    // half-written keeps that run's summaries, and counts a path already
    // holding the hash it recorded as a landed candidate rather than
    // merging the candidate's markers again.
    if let Some(standing) = candidate
        .existing
        .as_ref()
        .filter(|r| r.pending_writes.is_some())
    {
        local_paths.extend(standing.local_paths.iter().cloned());
        remote_paths.extend(standing.remote_paths.iter().cloned());
        collisions.extend(standing.collisions.iter().cloned());
        resumed_combined.extend(standing.combined_paths.iter().cloned());
        for (path, hash) in standing.pending_writes.iter().flatten() {
            if let Some(hash) = hash {
                written_this_run.insert(path.clone(), hash.clone());
            }
        }
    }

    'passes: for _ in 0..MAX_PASSES {
        // `converge` 1: the run's own collection on the first pass, a
        // fresh one on every pass after the run wrote the folder or after
        // a pass refused on a file changed since its collection.
        let mut folder = match first_folder.take() {
            Some(folder) => folder,
            None => collect_folder(copy, opts, &forget, true)?,
        };
        forget.clear();
        if *hold.get_or_insert_with(|| {
            holds_root_identity(!candidate.publishing, &candidate.base_files, &folder, head)
        }) {
            hold_root_identity(&mut folder);
        }
        let excluded = excluded_on_disk(
            copy,
            &folder.hashes,
            candidate.base_files.keys().chain(head.files.keys()),
        );
        let rec = reconcile(
            &without(&candidate.base_files, &excluded),
            &folder.hashes,
            &without(&head.files, &excluded),
        );
        // `converge` 4 — the collection's bytes given back, then every
        // head file this pass writes, each collision's head content and a
        // modify/modify collision's base content read before any state or
        // folder byte is written.
        folder.drop_bytes();
        let already_candidate = |path: &str| {
            let local_hash = folder.hashes.get(path);
            local_hash.is_some() && written_this_run.get(path) == local_hash
        };
        let mut head_wanted: BTreeMap<String, (String, Option<u64>)> = BTreeMap::new();
        let mut base_wanted: BTreeMap<String, (String, Option<u64>)> = BTreeMap::new();
        for path in &rec.remote_only {
            if let Some(wanted) = head.wanted(path) {
                head_wanted.insert(path.clone(), wanted);
            }
        }
        for (path, kind) in &rec.collisions {
            match kind {
                CollisionKind::DeleteModify | CollisionKind::AddAdd => {
                    head_wanted.extend(head.wanted(path).map(|w| (path.clone(), w)));
                }
                CollisionKind::ModifyModify => {
                    head_wanted.extend(head.wanted(path).map(|w| (path.clone(), w)));
                    if !already_candidate(path)
                        && candidate.base_commit.is_some()
                        && let Some(hash) = candidate.base_files.get(path)
                    {
                        base_wanted.insert(path.clone(), (hash.clone(), None));
                    }
                }
                _ => {}
            }
        }
        let repo = repo_id(copy);
        let mut head_blobs = if head_wanted.is_empty() {
            BTreeMap::new()
        } else {
            read_blobs(
                client,
                token,
                &repo,
                &head_commit,
                &head_wanted,
                &held,
                staging,
            )
            .await?
        };
        let base_blobs = match (&candidate.base_commit, base_wanted.is_empty()) {
            (Some(base_commit), false) => {
                read_blobs(
                    client,
                    token,
                    &repo,
                    base_commit,
                    &base_wanted,
                    &held,
                    staging,
                )
                .await?
            }
            _ => BTreeMap::new(),
        };

        // What this pass writes and removes.
        let mut writes: Vec<(String, WriteSource, String)> = Vec::new();
        let mut removals: Vec<String> = Vec::new();
        let mut candidate_hashes: BTreeMap<String, Option<String>> = BTreeMap::new();
        // Each remote snapshot entry: the head's content for the path, a
        // withheld merge result, or the path absent at the head.
        let mut remote_entries: BTreeMap<String, Option<Option<Blob>>> = BTreeMap::new();

        for path in &rec.remote_only {
            match head.files.get(path) {
                Some(hash) => writes.push((path.clone(), WriteSource::Head, hash.clone())),
                None => removals.push(path.clone()),
            }
        }
        for (path, kind) in &rec.collisions {
            let local_hash = folder.hashes.get(path);
            match kind {
                CollisionKind::ModifyDelete => {
                    remote_entries.insert(path.clone(), None);
                    candidate_hashes.insert(path.clone(), local_hash.cloned());
                }
                CollisionKind::DeleteModify => {
                    remote_entries.insert(path.clone(), Some(None));
                    let hash = head.files.get(path).cloned();
                    candidate_hashes.insert(path.clone(), hash.clone());
                    if !already_candidate(path)
                        && let Some(hash) = hash
                    {
                        writes.push((path.clone(), WriteSource::Head, hash));
                    }
                }
                CollisionKind::ModifyModify | CollisionKind::AddAdd => {
                    remote_entries.insert(path.clone(), Some(None));
                    if already_candidate(path) {
                        candidate_hashes.insert(path.clone(), local_hash.cloned());
                        continue;
                    }
                    let (Some(head_blob), Some(local)) =
                        (head_blobs.get(path), folder.files.get(path))
                    else {
                        candidate_hashes.insert(path.clone(), local_hash.cloned());
                        continue;
                    };
                    let base = match kind {
                        CollisionKind::ModifyModify => base_blobs.get(path),
                        _ => None,
                    };
                    let merged =
                        match merge_collision(copy, path, local, head_blob, base, &held, staging) {
                            Err(CliError::CollectedSetChanged { paths }) => {
                                forget = paths;
                                continue 'passes;
                            }
                            other => other?,
                        };
                    match merged {
                        Some((merged, hash)) => {
                            candidate_hashes.insert(path.clone(), Some(hash.clone()));
                            writes.push((path.clone(), WriteSource::Merged(merged), hash));
                        }
                        // Some side is not text: the local bytes stand as
                        // the candidate, the head's go to the remote
                        // snapshot, neither marker-merged (`D-089`).
                        None => {
                            candidate_hashes.insert(path.clone(), local_hash.cloned());
                        }
                    }
                }
                // Found on disk below, never by `reconcile`.
                CollisionKind::FolderFile | CollisionKind::FileFolder => {}
            }
        }
        drop(base_blobs);

        // A head file whose place local work holds — a folder holding a
        // file the candidate keeps at its path, or a file the candidate
        // keeps at a folder above it — is withheld rather than written: the
        // local side stays in the folder and is snapshotted, the head's
        // content goes to the remote snapshot, the withheld path is
        // snapshotted as absent, and the path both sides hold is a collision.
        let removed_here: BTreeSet<String> = removals.iter().cloned().collect();
        let mut held_paths: Vec<(String, bool)> = Vec::new();
        let mut held_collisions: Vec<(String, CollisionKind)> = Vec::new();
        let mut kept_writes: Vec<(String, WriteSource, String)> = Vec::with_capacity(writes.len());
        for (path, source, hash) in writes {
            let (contested, kind, local_files) =
                match local_work_in_the_way(&path, &folder.hashes, &removed_here) {
                    None => {
                        kept_writes.push((path, source, hash));
                        continue;
                    }
                    Some(InTheWay::Folder(files)) => {
                        (path.clone(), CollisionKind::FolderFile, files)
                    }
                    Some(InTheWay::File(file)) => {
                        (file.clone(), CollisionKind::FileFolder, vec![file])
                    }
                };
            for file in local_files {
                if folder.files.contains_key(&file) {
                    held_paths.push((file, true));
                }
            }
            held_collisions.push((contested, kind));
            held_paths.push((path.clone(), false));
            candidate_hashes.insert(path.clone(), None);
            remote_entries.insert(
                path,
                Some(match source {
                    WriteSource::Head => None,
                    WriteSource::Merged(blob) => Some(blob),
                }),
            );
        }
        let writes = kept_writes;

        // `converge` 5 — the snapshots, each content in a file of its
        // own. Replaced whole where no resolution stood when the run began
        // and no earlier pass of this run wrote them; otherwise a path
        // keeps its first content — taken before this run, or by the pass
        // that then wrote, removed or withheld it — and a path an earlier
        // pass snapshotted and left untouched is taken afresh (`D-093`).
        let keep_first = candidate.existing.is_some() || snapshots_written;
        let mut local_snapshot = if keep_first {
            copy.local_snapshot()?
        } else {
            Snapshot::new()
        };
        let mut retaken = false;
        for path in &untouched {
            retaken |= local_snapshot.remove(path).is_some();
        }
        let mut created: Vec<String> = Vec::new();
        let mut taken: Vec<String> = Vec::new();
        let mut before: HashMap<String, Option<String>> = HashMap::new();
        let snapshotted: Result<Snapshot, CliError> = (|| {
            for path in writes.iter().map(|(p, _, _)| p).chain(removals.iter()) {
                before.insert(path.clone(), prior_hash(copy, &folder, path)?);
                if !(keep_first && local_snapshot.contains_key(path)) {
                    let prior = snapshot_prior(copy, &folder, path, &mut created)?;
                    local_snapshot.insert(path.clone(), prior);
                    taken.push(path.clone());
                }
            }
            for (path, present) in &held_paths {
                if !(keep_first && local_snapshot.contains_key(path)) {
                    let prior = if *present {
                        snapshot_prior(copy, &folder, path, &mut created)?
                    } else {
                        None
                    };
                    local_snapshot.insert(path.clone(), prior);
                }
            }
            let mut remote_snapshot = Snapshot::new();
            for (path, entry) in &remote_entries {
                let content = match entry {
                    None => None,
                    Some(Some(blob)) => Some(blob.store(copy)?),
                    Some(None) => match head_blobs.get(path) {
                        Some(blob) => Some(blob.store(copy)?),
                        None => None,
                    },
                };
                remote_snapshot.insert(
                    path.clone(),
                    content.map(|(sha, fresh)| {
                        if fresh {
                            created.push(sha.clone());
                        }
                        SnapshotContent::Stored { stored: sha }
                    }),
                );
            }
            Ok(remote_snapshot)
        })();
        let remote_snapshot = match snapshotted {
            Ok(remote_snapshot) => remote_snapshot,
            Err(err) => {
                remove_created(copy, &created);
                if let CliError::CollectedSetChanged { paths } = err {
                    forget = paths;
                    continue 'passes;
                }
                return Err(err);
            }
        };
        // The pass stands from here: its summaries join the run's.
        local_paths.extend(rec.local_only.iter().cloned());
        remote_paths.extend(rec.remote_only.iter().cloned());
        for (path, kind) in &rec.collisions {
            collisions.entry(path.clone()).or_insert(*kind);
        }
        collisions.extend(held_collisions);
        let write_local = !writes.is_empty()
            || !removals.is_empty()
            || !held_paths.is_empty()
            || !keep_first
            || retaken;
        let write_remote = !remote_snapshot.is_empty() || !keep_first;
        let remote_standing = if write_remote {
            let mut standing = if keep_first {
                copy.remote_snapshot()?
            } else {
                Snapshot::new()
            };
            standing.extend(remote_snapshot);
            Some(standing)
        } else {
            None
        };
        if let Err(err) = copy.write_snapshots(
            write_local.then_some(&local_snapshot),
            remote_standing.as_ref(),
        ) {
            remove_created(copy, &created);
            return Err(err);
        }
        snapshots_written = true;
        drop(remote_entries);

        let needs_resolution = candidate.force_resolution
            || resolution.is_some()
            || !collisions.is_empty()
            || candidate.publishing && !local_paths.is_empty();
        if needs_resolution {
            let mut combined: BTreeSet<String> = local_paths.clone();
            combined.extend(resumed_combined.iter().cloned());
            for (path, hash) in &candidate_hashes {
                if hash.as_ref() != head.files.get(path) {
                    combined.insert(path.clone());
                }
            }
            let standing = resolution.take();
            // Recorded before the first folder write, so a run ending
            // before the last one leaves a resolution `converge` 4 finishes
            // rather than one a continue would publish half-written.
            let mut owed = standing
                .as_ref()
                .and_then(|r| r.pending_writes.clone())
                .unwrap_or_default();
            owed.extend(
                writes
                    .iter()
                    .map(|(path, _, hash)| (path.clone(), Some(hash.clone()))),
            );
            owed.extend(removals.iter().map(|path| (path.clone(), None)));
            let next = Resolution {
                recovery_id: standing
                    .as_ref()
                    .map(|r| r.recovery_id.clone())
                    .unwrap_or_else(|| mint_recovery_id(copy)),
                base_commit: candidate.base_commit.clone(),
                head_commit: head_commit.clone(),
                round: standing.as_ref().map(|r| r.round).unwrap_or(1),
                local_paths: local_paths.iter().cloned().collect(),
                remote_paths: remote_paths.iter().cloned().collect(),
                collisions: collisions.iter().map(|(p, k)| (p.clone(), *k)).collect(),
                combined_paths: combined.into_iter().collect(),
                reviewed_tree: None,
                pending_writes: (!owed.is_empty()).then_some(owed),
            };
            copy.write_resolution(&next)?;
            resolution = Some(next);
        }

        // `converge` 6 — every write before any removal, but for a removal
        // clearing the way for a write, each path hashed again immediately
        // before it is touched.
        let expected_before = |path: &str| -> Option<String> {
            match folder.hashes.get(path) {
                Some(hash) => Some(hash.clone()),
                None => before.get(path).cloned().flatten(),
            }
        };
        let unchanged = |path: &str| -> Result<bool, CliError> {
            Ok(disk_hash(copy, path)? == expected_before(path))
        };
        let written_paths: Vec<&String> = writes.iter().map(|(p, _, _)| p).collect();
        let (clearing, later) = clearing_first(&written_paths, &removals);
        drop(written_paths);
        let mut left_untouched = false;
        let mut blocked: Vec<&String> = Vec::new();
        let mut acted: BTreeSet<&String> = BTreeSet::new();
        #[cfg(test)]
        tests::before_folder_writes();
        for path in clearing {
            if !unchanged(path)? {
                left_untouched = true;
                blocked.push(path);
                continue;
            }
            remove_clearing(copy, path)?;
            removed.insert(path.clone());
            acted.insert(path);
        }
        for (path, source, hash) in &writes {
            if blocked.iter().any(|removal| clears_way(removal, path)) || !unchanged(path)? {
                left_untouched = true;
                continue;
            }
            let content = match source {
                WriteSource::Head => head_blobs.get(path).ok_or_else(|| CliError::Io {
                    message: format!("could not write {path}: its content was not read"),
                })?,
                WriteSource::Merged(blob) => blob,
            };
            replace_file_whole(&copy.root, path, content)?;
            written_this_run.insert(path.clone(), hash.clone());
            written.insert(path.clone());
            acted.insert(path);
        }
        head_blobs.clear();
        for path in later {
            if !unchanged(path)? {
                left_untouched = true;
                continue;
            }
            remove_folder_file(copy, path)?;
            removed.insert(path.clone());
            acted.insert(path);
        }
        untouched = taken
            .into_iter()
            .filter(|path| !acted.contains(path))
            .collect();
        drop(acted);
        drop(writes);

        if !left_untouched {
            // `converge` 12.
            return match resolution {
                Some(mut resolution) => {
                    if resolution.pending_writes.take().is_some() {
                        copy.write_resolution(&resolution)?;
                    }
                    Ok(Prepared::Resolution(resolution))
                }
                None => {
                    if let Some(commit) = &head.commit {
                        copy.record_base(commit, to_hash_map(&head.files))?;
                    }
                    Ok(Prepared::Synced {
                        written: written.into_iter().collect(),
                        removed: removed.into_iter().collect(),
                    })
                }
            };
        }
    }

    Ok(Prepared::Attention(resolution))
}

fn prepared_outcome(prepared: Prepared) -> SyncOutcome {
    match prepared {
        Prepared::Synced { written, removed } => SyncOutcome::Synced {
            written,
            removed,
            published: None,
        },
        Prepared::Resolution(resolution) => SyncOutcome::ResolutionRequired(resolution),
        Prepared::Attention(resolution) => SyncOutcome::AttentionRequired(resolution),
    }
}

// ---- the run (`converge` 1 to 12) ------------------------------------

// One value per run, moved out at once; boxing the outcome buys nothing.
#[allow(clippy::large_enum_variant)]
enum OutboxStep {
    Completed(SyncOutcome),
    Resume(Option<String>),
    CarryOn,
}

/// `converge` 2 and 3: complete an acknowledged publication, resume one
/// the head still carries no other change past, or drop the outbox.
async fn settle_outbox(
    client: &SynsClient,
    token: Option<&str>,
    copy: &WorkingCopy,
    mode: ConvergeMode,
) -> Result<OutboxStep, CliError> {
    let Some(outbox) = copy.outbox()? else {
        return Ok(OutboxStep::CarryOn);
    };

    // 2 sends nothing, so a retrieval holding no credential settles a
    // landed publication too, before it compares anything against a base
    // that publication left behind.
    let has_base = copy.base().is_some();
    let head = read_head(client, token, copy, reading_for(mode), has_base).await?;

    let excluded = excluded_on_disk(copy, &outbox.tree, head.files.keys());
    let head_files = without(&head.files, &excluded);
    if let Some(commit) = &head.commit
        && head_files == outbox.tree
    {
        copy.record_base(commit, to_hash_map(&head.files))?;
        // A resolution the landed publication did not carry — prepared
        // after its outbox by a run that could not settle it — still asks
        // for review: its candidate, markers included, stands in the folder.
        let prepared_since = copy.resolution()?.is_some_and(|standing| {
            standing.pending_writes.is_some()
                || standing.reviewed_tree.as_ref() != Some(&outbox.tree)
        });
        if prepared_since {
            copy.remove_outbox()?;
            return Ok(OutboxStep::CarryOn);
        }
        // The outbox goes last, so a run killed part-way finds it again.
        copy.remove_resolution()?;
        copy.remove_snapshots()?;
        copy.remove_outbox()?;
        return Ok(OutboxStep::Completed(SyncOutcome::Synced {
            written: Vec::new(),
            removed: Vec::new(),
            published: None,
        }));
    }

    // 3
    if matches!(mode, ConvergeMode::Retrieve { .. }) && token.is_none() {
        return Ok(OutboxStep::CarryOn);
    }

    let parent_files = match &outbox.parent_commit {
        Some(parent) => read_tree(client, token, copy, Some(parent)).await?.files,
        None => BTreeMap::new(),
    };
    let resumable = differing_paths(&head_files, &without(&parent_files, &excluded))
        .iter()
        .all(|path| head_files.get(path) == outbox.tree.get(path));
    if resumable {
        return Ok(OutboxStep::Resume(head.commit));
    }

    copy.remove_outbox()?;
    Ok(OutboxStep::CarryOn)
}

/// The options a run carries on: one budget every hold of the run comes
/// from.
fn with_run_budget(opts: SmartPushOptions) -> SmartPushOptions {
    let mut opts = opts;
    if opts.held.is_none() {
        opts.held = Some(HeldBytes::new(HELD_BYTES_BUDGET));
    }
    opts
}

/// Converge the working copy with the repository head, returning exactly
/// one outcome. A run repeated against an unchanged folder and head
/// writes nothing further.
pub async fn converge(
    client: &SynsClient,
    token: Option<&str>,
    copy: &WorkingCopy,
    mode: ConvergeMode,
    opts: SmartPushOptions,
) -> Result<SyncOutcome, CliError> {
    let _lock = copy.lock()?;
    let opts = with_run_budget(opts);

    // 1 — the run's staging, then the one collection the run hands on.
    let staging = Staging::open(&opts.cache_dir)?;
    let folder = collect_folder(copy, &opts, &[], true)?;

    // 3 — the too-large line, once per run, outside machine-readable
    // mode; a bare publication whose own summary carries it writes it
    // only where that publication does not land.
    let too_large = (!opts.json_output)
        .then(|| too_large_line(&folder.skipped))
        .flatten();
    let deferred = opts.renders_publication_summary;
    if !deferred && let Some(line) = &too_large {
        eprintln!("{line}");
    }

    let outcome = match settle_outbox(client, token, copy, mode).await {
        Err(err) => Err(err),
        Ok(OutboxStep::Completed(outcome)) => Ok(outcome),
        Ok(OutboxStep::Resume(parent)) => match token {
            None => Err(CliError::AuthRequired),
            Some(token) => {
                publish_reviewed(
                    client,
                    token,
                    copy,
                    opts,
                    &staging,
                    Some(parent),
                    Some(folder),
                )
                .await
            }
        },
        Ok(OutboxStep::CarryOn) => {
            converge_from_resolution(client, token, copy, mode, opts, &staging, Some(folder)).await
        }
    };

    if deferred
        && let Some(line) = &too_large
        && !matches!(
            outcome,
            Ok(SyncOutcome::Synced {
                published: Some(_),
                ..
            })
        )
    {
        eprintln!("{line}");
    }
    outcome
}

/// `converge` 4 to 12, under a lock the caller holds. `folder` is the
/// run's own collection, taken where the caller took none.
async fn converge_from_resolution(
    client: &SynsClient,
    token: Option<&str>,
    copy: &WorkingCopy,
    mode: ConvergeMode,
    opts: SmartPushOptions,
    staging: &Staging,
    folder: Option<Folder>,
) -> Result<SyncOutcome, CliError> {
    // 4
    let resolution = copy.resolution()?;
    if let Some(standing) = &resolution {
        if standing.pending_writes.is_some() && mode != (ConvergeMode::Retrieve { overwrite: true })
        {
            return finish_preparation(
                client,
                token,
                copy,
                &opts,
                staging,
                mode,
                standing.clone(),
                folder,
            )
            .await;
        }
        if standing.round > ROUND_BOUND {
            return Ok(SyncOutcome::AttentionRequired(resolution));
        }
        match mode {
            ConvergeMode::Publish if standing.reviewed_tree.is_some() => {
                let token = token.ok_or(CliError::AuthRequired)?;
                return publish_reviewed(client, token, copy, opts, staging, None, folder).await;
            }
            ConvergeMode::Retrieve { overwrite: true } => {}
            _ => return Ok(SyncOutcome::ResolutionRequired(standing.clone())),
        }
    }

    // 5
    let base = load_base(copy);
    let head = read_head(
        client,
        token,
        copy,
        reading_for(mode),
        base.commit.is_some(),
    )
    .await?;
    if head.truncated {
        return Ok(SyncOutcome::AttentionRequired(resolution));
    }

    // 6 — a retrieval holding the root identity file out drops it from
    // the folder here, and the exclusion test then drops it from the base
    // and the head that steps 7 to 12 compare, write and remove from.
    let mut folder = match folder {
        Some(folder) => folder,
        None => collect_folder(copy, &opts, &[], true)?,
    };
    let hold = holds_root_identity(
        matches!(mode, ConvergeMode::Retrieve { .. }),
        &base.files,
        &folder,
        &head,
    );
    if hold {
        hold_root_identity(&mut folder);
    }
    let excluded = excluded_on_disk(
        copy,
        &folder.hashes,
        base.files.keys().chain(head.files.keys()),
    );
    let head_files = without(&head.files, &excluded);

    // 7
    if mode == (ConvergeMode::Retrieve { overwrite: true })
        && (folder.hashes != head_files || resolution.is_some())
    {
        return overwrite_with_head(
            client,
            token,
            copy,
            &opts,
            staging,
            &head,
            folder,
            &excluded,
            resolution.is_some(),
        )
        .await;
    }

    // 8
    if folder.hashes == head_files {
        return match &head.commit {
            Some(commit) if base.commit.as_deref() != Some(commit.as_str()) => {
                copy.record_base(commit, to_hash_map(&head.files))?;
                Ok(SyncOutcome::Synced {
                    written: Vec::new(),
                    removed: Vec::new(),
                    published: None,
                })
            }
            _ => Ok(SyncOutcome::NoChanges),
        };
    }

    // 9
    if head.commit == base.commit {
        return match mode {
            ConvergeMode::Retrieve { .. } => Ok(SyncOutcome::NoChanges),
            ConvergeMode::Publish => {
                let token = token.ok_or(CliError::AuthRequired)?;
                publish_reviewed(client, token, copy, opts, staging, None, Some(folder)).await
            }
        };
    }

    // 10 to 12
    let prepared = prepare_candidate(
        client,
        token,
        copy,
        &opts,
        staging,
        Candidate {
            base_commit: base.commit,
            base_files: base.files,
            head: &head,
            publishing: mode == ConvergeMode::Publish,
            existing: None,
            force_resolution: false,
            hold_root_identity: Some(hold),
        },
        Some(folder),
    )
    .await?;
    Ok(prepared_outcome(prepared))
}

/// `converge` 4 over a resolution whose preparation a killed or failed run
/// left half-written: prepare the candidate again against the base and the
/// head that resolution recorded, keeping its recovery id, round and first
/// snapshot content, before the resolution is answered.
#[allow(clippy::too_many_arguments)]
async fn finish_preparation(
    client: &SynsClient,
    token: Option<&str>,
    copy: &WorkingCopy,
    opts: &SmartPushOptions,
    staging: &Staging,
    mode: ConvergeMode,
    standing: Resolution,
    folder: Option<Folder>,
) -> Result<SyncOutcome, CliError> {
    let base = load_base(copy);
    let base_files = match &standing.base_commit {
        Some(commit) if base.commit.as_ref() == Some(commit) => base.files,
        Some(commit) => read_tree(client, token, copy, Some(commit)).await?.files,
        None => BTreeMap::new(),
    };
    let head = if standing.head_commit.is_empty() {
        Head::default()
    } else {
        read_tree(client, token, copy, Some(&standing.head_commit)).await?
    };
    if head.truncated {
        return Ok(SyncOutcome::AttentionRequired(Some(standing)));
    }
    let past_bound = standing.round > ROUND_BOUND;

    let prepared = prepare_candidate(
        client,
        token,
        copy,
        opts,
        staging,
        Candidate {
            base_commit: standing.base_commit.clone(),
            base_files,
            head: &head,
            publishing: mode == ConvergeMode::Publish,
            existing: Some(standing),
            force_resolution: true,
            hold_root_identity: None,
        },
        folder,
    )
    .await?;

    Ok(match prepared {
        Prepared::Resolution(resolution) if past_bound => {
            SyncOutcome::AttentionRequired(Some(resolution))
        }
        other => prepared_outcome(other),
    })
}

/// `converge` 7: make the folder hold the head, every rewritten or
/// removed path snapshotted first and uncollected files left alone.
#[allow(clippy::too_many_arguments)]
async fn overwrite_with_head(
    client: &SynsClient,
    token: Option<&str>,
    copy: &WorkingCopy,
    opts: &SmartPushOptions,
    staging: &Staging,
    head: &Head,
    folder: Folder,
    excluded: &BTreeSet<String>,
    resolution_stands: bool,
) -> Result<SyncOutcome, CliError> {
    check_server_paths(head.files.keys())?;
    let head_commit = head.commit.clone().unwrap_or_default();
    let mut folder = folder;
    folder.drop_bytes();

    let wanted: BTreeMap<String, (String, Option<u64>)> = head
        .files
        .iter()
        .filter(|(path, hash)| !excluded.contains(*path) && folder.hashes.get(*path) != Some(hash))
        .filter_map(|(path, _)| head.wanted(path).map(|w| (path.clone(), w)))
        .collect();
    let blobs = if wanted.is_empty() {
        BTreeMap::new()
    } else {
        read_blobs(
            client,
            token,
            &repo_id(copy),
            &head_commit,
            &wanted,
            &opts.held_bytes(),
            staging,
        )
        .await?
    };
    let writes: Vec<(String, Blob)> = blobs.into_iter().collect();
    let removals: Vec<String> = folder
        .hashes
        .keys()
        .filter(|path| !head.files.contains_key(*path))
        .cloned()
        .collect();

    let mut snapshot = if resolution_stands {
        copy.local_snapshot()?
    } else {
        Snapshot::new()
    };
    let mut created: Vec<String> = Vec::new();
    let snapshotted: Result<(), CliError> = (|| {
        for path in writes.iter().map(|(p, _)| p).chain(removals.iter()) {
            if resolution_stands && snapshot.contains_key(path) {
                continue;
            }
            let prior = snapshot_prior(copy, &folder, path, &mut created)?;
            snapshot.insert(path.clone(), prior);
        }
        Ok(())
    })();
    if let Err(err) = snapshotted.and_then(|()| copy.write_local_snapshot(&snapshot)) {
        remove_created(copy, &created);
        return Err(err);
    }

    let written_paths: Vec<&String> = writes.iter().map(|(p, _)| p).collect();
    let (clearing, later) = clearing_first(&written_paths, &removals);
    drop(written_paths);
    for path in clearing {
        remove_clearing(copy, path)?;
    }
    for (path, content) in &writes {
        replace_file_whole(&copy.root, path, content)?;
    }
    for path in later {
        remove_folder_file(copy, path)?;
    }

    if let Some(commit) = &head.commit {
        copy.record_base(commit, to_hash_map(&head.files))?;
    }
    copy.remove_resolution()?;

    Ok(SyncOutcome::Synced {
        written: writes.into_iter().map(|(p, _)| p).collect(),
        removed: removals,
        published: None,
    })
}

// ---- the guarded publication (`publish_reviewed` 1 to 8) -------------

/// A failure after which the publication may have landed keeps the
/// outbox for `converge` 2 and 3 to settle: the server not reached, an
/// internal error, and a failure carrying no status the server answered.
fn keeps_outbox(err: &CliError) -> bool {
    match err {
        CliError::ServerUnreachable { .. } => true,
        CliError::Api { status: None, .. } => true,
        CliError::Api {
            status: Some(status),
            ..
        } => *status >= 500,
        _ => false,
    }
}

fn settle_refused_outbox(copy: &WorkingCopy, err: CliError) -> CliError {
    if !keeps_outbox(&err)
        && let Err(io) = copy.remove_outbox()
    {
        return io;
    }
    err
}

/// Run one required check through the platform shell at the root, both
/// of its streams on the diagnostic stream.
fn run_check(root: &Path, command: &str) -> bool {
    #[cfg(windows)]
    let mut process = {
        let mut process = Command::new("cmd");
        process.arg("/C").arg(command);
        process
    };
    #[cfg(not(windows))]
    let mut process = {
        let mut process = Command::new("sh");
        process.arg("-c").arg(command);
        process
    };
    process
        .current_dir(root)
        .stdin(Stdio::null())
        .stdout(Stdio::from(std::io::stderr()))
        .stderr(Stdio::inherit());
    match process.status() {
        Ok(status) if status.success() => true,
        Ok(status) => {
            eprintln!("required check failed ({status}): {command}");
            false
        }
        Err(err) => {
            eprintln!("could not run required check `{command}`: {err}");
            false
        }
    }
}

/// Whether a line opening in `pieces` opens with the first or the last
/// marker literal, read one piece at a time — the line test
/// `holds_conflict_marker` applies to text held whole.
struct MarkerScan {
    at_line_start: bool,
    prefix: Vec<u8>,
    found: bool,
}

impl MarkerScan {
    fn new() -> MarkerScan {
        MarkerScan {
            at_line_start: true,
            prefix: Vec::new(),
            found: false,
        }
    }

    fn longest() -> usize {
        CONFLICT_MARKERS[0].len().max(CONFLICT_MARKERS[3].len())
    }

    fn settle_prefix(&mut self) {
        let prefix = &self.prefix;
        if prefix.starts_with(CONFLICT_MARKERS[0].as_bytes())
            || prefix.starts_with(CONFLICT_MARKERS[3].as_bytes())
        {
            self.found = true;
        }
        self.prefix.clear();
    }

    fn feed(&mut self, piece: &[u8]) {
        for &byte in piece {
            if self.found {
                return;
            }
            if byte == b'\n' {
                self.settle_prefix();
                self.at_line_start = true;
                continue;
            }
            if self.at_line_start {
                self.prefix.push(byte);
                if self.prefix.len() >= Self::longest() {
                    self.settle_prefix();
                    self.at_line_start = false;
                }
            }
        }
    }

    fn finish(mut self) -> bool {
        if self.at_line_start {
            self.settle_prefix();
        }
        self.found
    }
}

/// `converge` 7: whether a collision path's folder bytes are text holding
/// a conflict marker — taken through `read_collected` under a hold `held`
/// admits for the file's size, and otherwise read in pieces and hashed as
/// they are read. Bytes no longer hashing to what the collection took are
/// refused as `CliError::CollectedSetChanged`.
fn marked_text(
    root: &Path,
    path: &str,
    file: &CollectedFile,
    held: &Arc<HeldBytes>,
) -> Result<bool, CliError> {
    let changed = || CliError::CollectedSetChanged {
        paths: vec![path.to_string()],
    };
    if let Some((bytes, _)) = &file.bytes {
        return Ok(is_text(bytes) && holds_conflict_marker(&String::from_utf8_lossy(bytes)));
    }
    let size = std::fs::metadata(root.join(path))
        .map_err(|_| changed())?
        .len();
    if let Some(_hold) = held.try_hold(size) {
        let bytes = read_collected(root, path, file)?;
        return Ok(is_text(&bytes) && holds_conflict_marker(&String::from_utf8_lossy(&bytes)));
    }
    // Read in pieces: the hash, the text test and the marker test in one
    // pass over the file, the two tests fed as the hash's sink.
    struct TextPieces {
        carried: Vec<u8>,
        text: bool,
        markers: MarkerScan,
    }
    impl Write for TextPieces {
        fn write(&mut self, piece: &[u8]) -> std::io::Result<usize> {
            self.markers.feed(piece);
            if !self.text {
                return Ok(piece.len());
            }
            if piece.contains(&0) {
                self.text = false;
                return Ok(piece.len());
            }
            let mut joined = std::mem::take(&mut self.carried);
            joined.extend_from_slice(piece);
            match std::str::from_utf8(&joined) {
                Ok(_) => {}
                Err(err) if err.error_len().is_none() => {
                    self.carried = joined[err.valid_up_to()..].to_vec();
                }
                Err(_) => self.text = false,
            }
            Ok(piece.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    let from = std::fs::File::open(root.join(path)).map_err(|_| changed())?;
    let mut scan = TextPieces {
        carried: Vec::new(),
        text: true,
        markers: MarkerScan::new(),
    };
    let sha = crate::push::hash::hash_pieces(from, size, &mut scan).map_err(|_| changed())?;
    if sha.as_deref() != Some(file.sha.as_str()) {
        return Err(changed());
    }
    let text = scan.text && scan.carried.is_empty();
    Ok(text && scan.markers.finish())
}

/// `publish_reviewed`, under a lock the caller holds. `resumed` is the
/// head `converge` 3 resumed at; `reviewed` is the collection the run
/// took — the folder `continue_resolution` 3 just recorded, or the one
/// `converge` 1 took — standing in for step 1's walk.
#[allow(clippy::too_many_arguments)]
async fn publish_reviewed(
    client: &SynsClient,
    token: &str,
    copy: &WorkingCopy,
    opts: SmartPushOptions,
    staging: &Staging,
    resumed: Option<Option<String>>,
    reviewed: Option<Folder>,
) -> Result<SyncOutcome, CliError> {
    let mut reviewed = reviewed;
    let mut forget: Vec<String> = Vec::new();
    let held = opts.held_bytes();
    'passes: for _ in 0..MAX_PASSES {
        // 1 — the run's collection, or a fresh one after a publication
        // pass refused on a file changed since its collection, each such
        // path's record entry dropped first.
        let folder = match reviewed.take() {
            Some(folder) => folder,
            None => collect_folder(copy, &opts, &forget, true)?,
        };
        forget.clear();
        let mut resolution = copy.resolution()?;
        if let Some(standing) = resolution.as_mut() {
            match &standing.reviewed_tree {
                // Either arm hands the resolution back for review, so a
                // standing outbox — the dropped publication a resume came
                // in on — is dropped with it: kept, it would resume ahead
                // of every later continue and no review would publish.
                Some(tree) if *tree != folder.hashes => {
                    let changed = differing_paths(tree, &folder.hashes);
                    standing.reviewed_tree = None;
                    union_sorted(&mut standing.local_paths, changed.iter().cloned());
                    union_sorted(&mut standing.combined_paths, changed);
                    copy.write_resolution(standing)?;
                    copy.remove_outbox()?;
                    return Ok(SyncOutcome::ResolutionRequired(standing.clone()));
                }
                Some(_) => {}
                None => {
                    copy.remove_outbox()?;
                    return Ok(SyncOutcome::ResolutionRequired(standing.clone()));
                }
            }

            // 2 — a conflict marker, tested only in the collision paths
            // whose folder bytes are text (SPEC u280 `converge` 7).
            for (path, _kind) in &standing.collisions {
                let Some(file) = folder.files.get(path) else {
                    continue;
                };
                match marked_text(&copy.root, path, file, &held) {
                    Ok(true) => return Ok(SyncOutcome::ResolutionRequired(standing.clone())),
                    Ok(false) => {}
                    Err(CliError::CollectedSetChanged { paths }) => {
                        forget = paths;
                        continue 'passes;
                    }
                    Err(err) => return Err(err),
                }
            }

            // 3
            for check in read_required_checks(&copy.root)? {
                if !run_check(&copy.root, &check) {
                    return Ok(SyncOutcome::ResolutionRequired(standing.clone()));
                }
            }
        }

        // 4
        let base = load_base(copy);
        let parent = match (&resumed, &resolution) {
            (Some(head), _) => head.clone(),
            (None, Some(standing)) => Some(standing.head_commit.clone()),
            (None, None) => base.commit.clone(),
        };
        copy.write_outbox(&Outbox {
            parent_commit: parent.clone(),
            tree: folder.hashes.clone(),
        })?;

        // 5
        let reference = match &parent {
            Some(commit) if base.commit.as_ref() == Some(commit) => base.files.clone(),
            Some(commit) => match read_tree(client, Some(token), copy, Some(commit)).await {
                Ok(tree) => tree.files,
                Err(err) => return Err(settle_refused_outbox(copy, err)),
            },
            None => BTreeMap::new(),
        };
        let reference = without(
            &reference,
            &excluded_on_disk(copy, &folder.hashes, reference.keys()),
        );
        let hashes = folder.hashes.clone();
        let mut push_opts = opts.clone();
        push_opts.force = false;
        push_opts.author = None;
        push_opts.prefix = None;
        push_opts.parent_sha = parent.clone();
        push_opts.reference = Some(to_hash_map(&reference));
        push_opts.expected = resolution.as_ref().map(|_| to_hash_map(&hashes));
        // The publication walks nothing: it publishes from this pass's
        // collection (SPEC u280 `converge` 1).
        push_opts.collected = Some(folder.into_collected());

        match smart_push(client, token, &repo_id(copy), &copy.root, push_opts).await {
            Ok((response, raw, meta)) => {
                // 6
                if !response.commit_sha.is_empty() {
                    copy.record_base(&response.commit_sha, to_hash_map(&hashes))?;
                }
                copy.remove_outbox()?;
                copy.remove_resolution()?;
                copy.remove_snapshots()?;
                return Ok(SyncOutcome::Synced {
                    written: Vec::new(),
                    removed: Vec::new(),
                    published: Some((response, raw, meta)),
                });
            }
            Err(CliError::CollectedSetChanged { paths }) => {
                copy.remove_outbox()?;
                forget = paths;
                continue;
            }
            // A `conflict` naming no head is not a moved head — a first
            // publication over an identity whose content store already
            // holds commits — so no round is raised and it is refused below.
            Err(CliError::Api {
                status: Some(409),
                ref error,
                context: Some(ApiErrorContext::HeadMoved { .. }),
            }) if error == "conflict" => {
                // 7
                copy.remove_outbox()?;
                return guard_refused(
                    client, token, copy, &opts, staging, resolution, parent, reference,
                )
                .await;
            }
            Err(err) => return Err(settle_refused_outbox(copy, err)),
        }
    }

    Ok(SyncOutcome::AttentionRequired(copy.resolution()?))
}

/// `publish_reviewed` 7 and 8: raise the round, wait its backoff, and
/// prepare the candidate again over the newest head with the refused
/// parent standing as the base.
#[allow(clippy::too_many_arguments)]
async fn guard_refused(
    client: &SynsClient,
    token: &str,
    copy: &WorkingCopy,
    opts: &SmartPushOptions,
    staging: &Staging,
    resolution: Option<Resolution>,
    parent: Option<String>,
    reference: BTreeMap<String, String>,
) -> Result<SyncOutcome, CliError> {
    let refused_round = resolution.as_ref().map(|r| r.round).unwrap_or(1).max(1);
    let raised = resolution.map(|mut standing| {
        standing.round += 1;
        standing
    });
    if let Some(standing) = &raised {
        copy.write_resolution(standing)?;
    }
    let past_bound = raised.as_ref().is_some_and(|r| r.round > ROUND_BOUND);

    if !past_bound {
        let index = (refused_round as usize - 1).min(BACKOFF_SECONDS.len() - 1);
        tokio::time::sleep(Duration::from_secs(BACKOFF_SECONDS[index])).await;
    }

    let has_base = copy.base().is_some();
    let head = read_head(
        client,
        Some(token),
        copy,
        HeadReading::Publication,
        has_base,
    )
    .await?;
    if head.truncated {
        return Ok(SyncOutcome::AttentionRequired(raised));
    }

    let existing = raised.map(|mut standing| {
        standing.reviewed_tree = None;
        standing
    });
    let prepared = prepare_candidate(
        client,
        Some(token),
        copy,
        opts,
        staging,
        Candidate {
            base_commit: parent,
            base_files: reference,
            head: &head,
            publishing: true,
            existing,
            force_resolution: true,
            hold_root_identity: Some(false),
        },
        None,
    )
    .await?;

    Ok(match prepared {
        Prepared::Resolution(resolution) if past_bound => {
            SyncOutcome::AttentionRequired(Some(resolution))
        }
        other => prepared_outcome(other),
    })
}

// ---- continue, discard, state -----------------------------------------

/// Publish the folder as it stands at the call as the reviewed
/// resolution.
pub async fn continue_resolution(
    client: &SynsClient,
    token: &str,
    copy: &WorkingCopy,
    opts: SmartPushOptions,
) -> Result<SyncOutcome, CliError> {
    // 1
    let _lock = copy.lock()?;
    let opts = with_run_budget(opts);
    let staging = Staging::open(&opts.cache_dir)?;

    // 2
    match settle_outbox(client, Some(token), copy, ConvergeMode::Publish).await? {
        OutboxStep::Completed(outcome) => return Ok(outcome),
        OutboxStep::Resume(parent) => {
            return publish_reviewed(client, token, copy, opts, &staging, Some(parent), None).await;
        }
        OutboxStep::CarryOn => {}
    }
    let Some(mut resolution) = copy.resolution()? else {
        return converge_from_resolution(
            client,
            Some(token),
            copy,
            ConvergeMode::Publish,
            opts,
            &staging,
            None,
        )
        .await;
    };
    // A half-written candidate is finished and handed back for review,
    // never recorded as the reviewed tree: publishing it would name each
    // head change still unwritten as a local revert.
    if resolution.pending_writes.is_some() {
        return converge_from_resolution(
            client,
            Some(token),
            copy,
            ConvergeMode::Publish,
            opts,
            &staging,
            None,
        )
        .await;
    }

    // 3 — the one collection, recorded as reviewed and handed on.
    let folder = collect_folder(copy, &opts, &[], true)?;
    resolution.reviewed_tree = Some(folder.hashes.clone());
    copy.write_resolution(&resolution)?;

    // 4
    publish_reviewed(client, token, copy, opts, &staging, None, Some(folder)).await
}

/// Put the folder back as it stood before the resolution rewrote it,
/// reaching no server, and leave the base as it stood (SPEC u280
/// `discard_resolution` 1–2).
pub fn discard_resolution(copy: &WorkingCopy) -> Result<(), CliError> {
    let _lock = copy.lock()?;
    if copy.resolution()?.is_none() {
        return Ok(());
    }
    // 1 — every stored content checked against its name, and a released
    // build's inline content first stored as a content file of its own,
    // before any path is written.
    let mut writes: Vec<(String, Blob)> = Vec::new();
    let mut removals: Vec<String> = Vec::new();
    for (path, content) in copy.local_snapshot()? {
        check_server_path(&path)?;
        match content {
            Some(SnapshotContent::Stored { stored }) => {
                writes.push((path, Blob::Staged(copy.stored_content(&stored)?)));
            }
            Some(inline) => {
                let bytes = copy.content_bytes(&inline)?;
                let (stored, _) = copy.store_bytes(&bytes)?;
                writes.push((path, Blob::Staged(copy.stored_content(&stored)?)));
            }
            None => removals.push(path),
        }
    }
    // 2
    let written_paths: Vec<&String> = writes.iter().map(|(p, _)| p).collect();
    let (clearing, later) = clearing_first(&written_paths, &removals);
    drop(written_paths);
    for path in clearing {
        remove_clearing(copy, path)?;
    }
    for (path, content) in &writes {
        replace_file_whole(&copy.root, path, content)?;
    }
    for path in later {
        remove_folder_file(copy, path)?;
    }
    // A continued publication that may have landed leaves an outbox; kept,
    // the next run would resume it and publish the restored folder over the
    // head with no resolution standing. Where it did land, the next run
    // finds the head past the base and prepares a candidate instead.
    copy.remove_outbox()?;
    copy.remove_resolution()?;
    copy.remove_snapshots()
}

/// Where the working copy stands, against a head read in this call. Its
/// collection reads through the stat record and writes none back.
pub async fn working_copy_state(
    client: &SynsClient,
    token: Option<&str>,
    copy: &WorkingCopy,
) -> Result<WorkingCopyState, CliError> {
    // 1
    if copy.outbox()?.is_some() {
        return Ok(WorkingCopyState::PublicationPending);
    }
    if copy.resolution()?.is_some() {
        return Ok(WorkingCopyState::ResolutionRequired);
    }

    // 2
    let base = load_base(copy);
    let head = read_head(
        client,
        token,
        copy,
        HeadReading::State,
        base.commit.is_some(),
    )
    .await?;

    // 3
    #[cfg(unix)]
    let mut record = Some(copy.stat_record());
    #[cfg(not(unix))]
    let mut record: Option<StatRecord> = None;
    let collected = collect_files(
        &copy.root,
        &[],
        CollectOptions::default(),
        record.as_mut(),
        &HeldBytes::new(HELD_BYTES_BUDGET),
    )?;
    let folder: BTreeMap<String, String> = collected
        .files
        .iter()
        .filter(|(path, _)| !is_partial_write(path))
        .map(|(path, file)| (path.clone(), file.sha.clone()))
        .collect();
    let excluded = excluded_on_disk(copy, &folder, base.files.keys().chain(head.files.keys()));
    let head_files = without(&head.files, &excluded);
    let base_files = without(&base.files, &excluded);
    if folder == head_files {
        return Ok(WorkingCopyState::Converged);
    }
    let local = folder != base_files;
    let remote = head.commit != base.commit || head_files != base_files;
    Ok(match (local, remote) {
        (true, false) => WorkingCopyState::LocalChanges,
        (false, true) => WorkingCopyState::RemoteChanges,
        _ => WorkingCopyState::Diverged,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;

    thread_local! {
        /// Run once between a candidate pass's snapshot and its first
        /// folder write, on the thread driving the pass.
        static BEFORE_FOLDER_WRITES: RefCell<Option<Box<dyn FnOnce()>>> =
            const { RefCell::new(None) };
    }

    pub(super) fn before_folder_writes() {
        if let Some(hook) = BEFORE_FOLDER_WRITES.with(|hook| hook.borrow_mut().take()) {
            hook();
        }
    }

    /// `D-093`: a path a pass snapshotted and then left untouched — a
    /// writer landing between its snapshot and its write — is snapshotted
    /// again by the next pass, so a discard gives back the writer's bytes.
    #[tokio::test(flavor = "current_thread")]
    async fn a_path_left_untouched_is_snapshotted_again() {
        use wiremock::matchers::{method, path, query_param};
        use wiremock::{Mock, MockServer, ResponseTemplate};
        let base: &[u8] = b"a\nb\nc\n";
        let head_bytes: &[u8] = b"a\nHEAD\nc\n";
        let server = MockServer::start().await;
        for (at, bytes) in [("h0", base), ("h1", head_bytes)] {
            Mock::given(method("GET"))
                .and(path("/api/v1/repos/alice/r/raw/a.md"))
                .and(query_param("ref", at))
                .respond_with(ResponseTemplate::new(200).set_body_bytes(bytes.to_vec()))
                .mount(&server)
                .await;
        }
        let client = SynsClient::new(&server.uri()).unwrap();
        let cache = tempfile::tempdir().unwrap();
        let folder = tempfile::tempdir().unwrap();
        std::fs::write(folder.path().join("a.md"), base).unwrap();
        let copy = WorkingCopy::open(cache.path(), "alice", "r", folder.path()).unwrap();
        let staging = Staging::open(cache.path()).unwrap();
        let opts = SmartPushOptions {
            force: false,
            message: "push".into(),
            author: None,
            parent_sha: None,
            excludes: vec![],
            cache_dir: cache.path().to_path_buf(),
            description: None,
            tags: None,
            status: None,
            visibility: None,
            strict: false,
            allow_empty: false,
            debug: false,
            no_default_excludes: false,
            prefix: None,
            reference: None,
            expected: None,
            provenance: None,
            collected: None,
            held: None,
            json_output: false,
            renders_publication_summary: false,
        };
        let head = Head {
            commit: Some("h1".into()),
            files: BTreeMap::from([("a.md".to_string(), blob_sha1(head_bytes))]),
            sizes: BTreeMap::from([("a.md".to_string(), Some(head_bytes.len() as u64))]),
            truncated: false,
        };
        let target = folder.path().join("a.md");
        BEFORE_FOLDER_WRITES.with(|hook| {
            *hook.borrow_mut() = Some(Box::new(move || {
                std::fs::write(target, b"a\nagent\nc\n").unwrap();
            }));
        });

        let prepared = prepare_candidate(
            &client,
            None,
            &copy,
            &opts,
            &staging,
            Candidate {
                base_commit: Some("h0".into()),
                base_files: BTreeMap::from([("a.md".to_string(), blob_sha1(base))]),
                head: &head,
                publishing: false,
                existing: None,
                force_resolution: false,
                hold_root_identity: Some(false),
            },
            None,
        )
        .await
        .unwrap();

        let Prepared::Resolution(resolution) = prepared else {
            panic!("expected a resolution");
        };
        assert!(
            resolution
                .collisions
                .contains(&("a.md".to_string(), CollisionKind::ModifyModify)),
            "{:?}",
            resolution.collisions
        );
        discard_resolution(&copy).unwrap();
        assert_eq!(
            std::fs::read(folder.path().join("a.md")).unwrap(),
            b"a\nagent\nc\n"
        );
    }

    #[test]
    fn is_partial_write_admits_only_the_minted_sibling_name() {
        assert!(is_partial_write(".syns-partial-0123456789abcdef"));
        assert!(is_partial_write("sub/.syns-partial-0123456789abcdef"));
        assert!(!is_partial_write(".syns-partial-notes.md"));
        assert!(!is_partial_write(".syns-partial-0123456789abcde"));
    }

    #[test]
    fn check_server_path_admits_an_ordinary_path() {
        assert!(check_server_path("docs/a.md").is_ok());
        assert!(check_server_path(".gitignore").is_ok());
        assert!(check_server_path("dir/.hidden/x").is_ok());
    }

    #[test]
    fn check_server_path_refuses_every_breach_of_the_file_path_constraint() {
        for path in [
            "",
            "/etc/passwd",
            "\\x",
            "a/../b",
            "..",
            "./a",
            "a/./b",
            ".git/config",
            "a/.GIT/hooks",
            "a\\..\\b",
            "a\tb",
            "a\nb",
            "a\0b",
            " a.md",
            "a.md ",
        ] {
            assert!(check_server_path(path).is_err(), "{path:?} was admitted");
        }
        assert!(check_server_path(&"a".repeat(1000)).is_ok());
        assert!(check_server_path(&"a".repeat(1001)).is_err());
    }

    // ---- u280: staging and the retrieval's reads -----------------------

    #[test]
    fn a_dead_runs_staging_is_swept() {
        let cache = tempfile::tempdir().unwrap();
        let root = cache.path().join("staging");
        std::fs::create_dir_all(root.join("1-dead")).unwrap();
        std::fs::write(root.join("1-dead/0"), b"left behind").unwrap();
        std::fs::write(root.join("1-dead.lock"), b"").unwrap();
        std::fs::create_dir_all(root.join("2-live")).unwrap();
        std::fs::write(root.join("2-live/0"), b"in use").unwrap();
        let live = std::fs::File::create(root.join("2-live.lock")).unwrap();
        live.lock().unwrap();

        let staging = Staging::open(cache.path()).unwrap();

        assert!(!root.join("1-dead").exists());
        assert!(!root.join("1-dead.lock").exists());
        assert_eq!(std::fs::read(root.join("2-live/0")).unwrap(), b"in use");
        assert!(staging.dir.is_dir());
        assert!(staging.dir.starts_with(&root));
        let own = staging.dir.clone();
        drop(staging);
        assert!(!own.exists());
        drop(live);
    }

    /// A listener answering every raw read after `hold`, recording the
    /// most reads it held outstanding at once; each body is the path's
    /// name.
    async fn counting_listener(hold: Duration) -> (String, Arc<std::sync::atomic::AtomicUsize>) {
        use std::sync::atomic::AtomicUsize;
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let most = Arc::new(AtomicUsize::new(0));
        let now = Arc::new(AtomicUsize::new(0));
        let most_seen = most.clone();
        tokio::spawn(async move {
            loop {
                let Ok((mut sock, _)) = listener.accept().await else {
                    return;
                };
                let (most, now) = (most_seen.clone(), now.clone());
                tokio::spawn(async move {
                    let mut buf = vec![0u8; 8192];
                    loop {
                        let mut seen = Vec::new();
                        while !seen.windows(4).any(|w| w == b"\r\n\r\n") {
                            match sock.read(&mut buf).await {
                                Ok(0) | Err(_) => return,
                                Ok(n) => seen.extend_from_slice(&buf[..n]),
                            }
                        }
                        let head = String::from_utf8_lossy(&seen).to_string();
                        let target = head.split(' ').nth(1).unwrap_or("").to_string();
                        let name = target
                            .split('?')
                            .next()
                            .unwrap_or("")
                            .rsplit('/')
                            .next()
                            .unwrap_or("")
                            .to_string();
                        let current = now.fetch_add(1, Ordering::SeqCst) + 1;
                        most.fetch_max(current, Ordering::SeqCst);
                        tokio::time::sleep(hold).await;
                        now.fetch_sub(1, Ordering::SeqCst);
                        let response = format!(
                            "HTTP/1.1 200 OK\r\ncontent-length: {}\r\n\r\n{name}",
                            name.len()
                        );
                        if sock.write_all(response.as_bytes()).await.is_err() {
                            return;
                        }
                    }
                });
            }
        });
        (format!("http://127.0.0.1:{}", addr.port()), most)
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_retrieval_admits_reads_by_size() {
        let cases: [(&str, usize, Option<u64>, usize); 3] = [
            ("big", 6, Some(20_971_520), 3),
            ("unsized", 6, None, 2),
            ("small", 20, Some(1_024), 16),
        ];
        for (prefix, count, size, expected) in cases {
            let (uri, most) = counting_listener(Duration::from_millis(200)).await;
            let client = SynsClient::new(&uri).unwrap();
            let cache = tempfile::tempdir().unwrap();
            let staging = Staging::open(cache.path()).unwrap();
            let held = HeldBytes::new(HELD_BYTES_BUDGET);
            let wanted: BTreeMap<String, (String, Option<u64>)> = (0..count)
                .map(|i| {
                    let path = format!("{prefix}{i:02}");
                    let hash = blob_sha1(path.as_bytes());
                    (path, (hash, size))
                })
                .collect();

            let answers = read_blobs(&client, None, "alice/r", "h", &wanted, &held, &staging)
                .await
                .unwrap();

            assert_eq!(
                most.load(Ordering::SeqCst),
                expected,
                "{prefix}: the most reads outstanding"
            );
            assert_eq!(answers.len(), count);
            for (path, blob) in &answers {
                assert_eq!(&*blob.load().unwrap(), path.as_bytes(), "{path}");
            }
        }
    }

    #[tokio::test]
    async fn a_zero_budget_retrieval_stages_outside_the_folder() {
        use wiremock::matchers::{method, path, query_param};
        use wiremock::{Mock, MockServer, ResponseTemplate};
        let png: &[u8] = b"\x89PNG\r\n\x1a\n\x00\xff";
        let text: &[u8] = b"# a\n";
        let server = MockServer::start().await;
        for (name, bytes, at) in [
            ("image.png", png, "h1"),
            ("a.md", text, "h1"),
            ("image.png", &b"other bytes"[..], "h2"),
        ] {
            Mock::given(method("GET"))
                .and(path(format!("/api/v1/repos/alice/r/raw/{name}")))
                .and(query_param("ref", at))
                .respond_with(ResponseTemplate::new(200).set_body_bytes(bytes.to_vec()))
                .mount(&server)
                .await;
        }
        let client = SynsClient::new(&server.uri()).unwrap();
        let cache = tempfile::tempdir().unwrap();
        let folder = tempfile::tempdir().unwrap();
        let staging = Staging::open(cache.path()).unwrap();
        let held = HeldBytes::new(0);
        let wanted = |entries: &[(&str, &[u8])]| -> BTreeMap<String, (String, Option<u64>)> {
            entries
                .iter()
                .map(|(p, b)| (p.to_string(), (blob_sha1(b), Some(b.len() as u64))))
                .collect()
        };

        let first = read_blobs(
            &client,
            None,
            "alice/r",
            "h1",
            &wanted(&[("image.png", png), ("a.md", text)]),
            &held,
            &staging,
        )
        .await
        .unwrap();
        for (path, blob) in &first {
            match blob {
                Blob::Staged(file) => assert!(file.starts_with(&staging.dir), "{path}"),
                Blob::Held(..) => panic!("{path} was held under a zero budget"),
            }
            replace_file_whole(folder.path(), path, blob).unwrap();
        }
        assert_eq!(std::fs::read(folder.path().join("image.png")).unwrap(), png);
        assert_eq!(std::fs::read(folder.path().join("a.md")).unwrap(), text);
        drop(first);
        let staged_before = std::fs::read_dir(&staging.dir).unwrap().count();

        let second = read_blobs(
            &client,
            None,
            "alice/r",
            "h2",
            &wanted(&[("image.png", png)]),
            &held,
            &staging,
        )
        .await;
        match second {
            Err(err) => assert!(
                err.to_string().contains(&format!(
                    "invalid response body: image.png: expected {}, got {}",
                    blob_sha1(png),
                    blob_sha1(b"other bytes")
                )),
                "{err}"
            ),
            Ok(_) => panic!("a mismatched answer was taken"),
        }
        assert_eq!(
            std::fs::read_dir(&staging.dir).unwrap().count(),
            staged_before,
            "the refused read left a staged file"
        );
        let own = staging.dir.clone();
        drop(staging);
        assert!(!own.exists());
    }

    #[test]
    fn a_marker_is_found_one_piece_at_a_time_as_it_is_in_whole_text() {
        for text in [
            "a\n<<<<<<< local\nb\n",
            "<<<<<<< local",
            "a\n>>>>>>> remote\n",
            "a\n=======\n",
            "a <<<<<<< local\n",
            "",
        ] {
            let mut scan = MarkerScan::new();
            for byte in text.as_bytes() {
                scan.feed(std::slice::from_ref(byte));
            }
            assert_eq!(scan.finish(), holds_conflict_marker(text), "{text:?}");
        }
    }
}
