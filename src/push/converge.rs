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
use crate::push::folder_check::{FOLDER_SEND_BOUND, FolderCheck};
use crate::push::hash::blob_sha1;
use crate::push::manifest::Manifest;
use crate::push::reconcile::{
    CONFLICT_MARKERS, CollisionKind, holds_conflict_marker, merge_text, reconcile,
};
use crate::push::smart::{
    PushPipelineMeta, SmartPushOptions, smart_push, strict_refuses, tree_to_sha_map,
};
use crate::push::working_copy::{
    Outbox, Resolution, Snapshot, SnapshotContent, StatRecord, WorkingCopy, folder_base,
    holds_in_root_home,
};
use crate::repo::folder::{FolderScope, lies_under};
use crate::repo::identity::identity_head;
use crate::repo::syns_yaml::{
    IdentityForm, held_root_identity, identity_text, read_identity_form, read_required_checks,
    refuse_marked_root_identity,
};

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
    /// SPEC u291: the directory is that of the working copy whose
    /// resolution the run answered with, none where it is the run's own.
    ResolutionRequired(Resolution, Option<PathBuf>),
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
pub(crate) struct Head {
    pub(crate) commit: Option<String>,
    pub(crate) files: BTreeMap<String, String>,
    /// Each file's size as the tree answers it, none where it answers
    /// none.
    pub(crate) sizes: BTreeMap<String, Option<u64>>,
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

/// The recorded path a folder copy's served paths are joined under: the
/// holder's for a folder bound to its holder, none for a copy bound to a
/// shared folder's identity, which serves them counted from the folder
/// (SPEC u302 `converge` 1), and none for every other copy.
fn served_under(copy: &WorkingCopy) -> Option<&str> {
    copy.folder
        .as_ref()
        .filter(|scope| scope.identity.is_none())
        .map(|scope| scope.path.as_str())
}

/// The holder a `FolderCheck` asked on `scope` numbers a `since` through:
/// the holder on a scope bound to its identity, none otherwise (SPEC u302
/// `converge` 3).
pub(crate) fn check_holder(scope: &FolderScope) -> Option<String> {
    scope.identity.as_ref().map(|_| scope.holder())
}

/// A served tree as a head, every path counted from `folder` where one
/// stands and a path lying outside it left out.
fn head_of(tree: &crate::client::TreeResponse, folder: Option<&str>) -> Head {
    // SPEC u298 `IN_ROOT_HOME`: a served path in the in-root home reads
    // as absent, so a convergence writes and removes none.
    let counted = |path: &str| -> Option<String> {
        if holds_in_root_home(path) {
            return None;
        }
        match folder {
            Some(folder) => lies_under(path, folder).then(|| path[folder.len() + 1..].to_string()),
            None => Some(path.to_string()),
        }
    };
    let sizes = tree
        .entries
        .iter()
        .filter(|e| e.entry_type == EntryType::File && e.sha.is_some())
        .filter_map(|e| counted(&e.path).map(|path| (path, e.size)))
        .collect();
    Head {
        commit: Some(tree.commit_sha.clone()).filter(|c| !c.is_empty()),
        files: tree_to_sha_map(tree)
            .into_iter()
            .filter_map(|(path, sha)| counted(&path).map(|path| (path, sha)))
            .collect(),
        sizes,
        truncated: tree.truncated,
    }
}

async fn read_tree(
    client: &SynsClient,
    token: Option<&str>,
    copy: &WorkingCopy,
    at: Option<&str>,
) -> Result<Head, CliError> {
    // SPEC u291 `converge` 1: a folder copy reads the holder's tree
    // under its recorded path; SPEC u302 `converge` 1: a copy bound to an
    // identity reads the identity's whole tree, every path as served.
    if let Some(path) = served_under(copy) {
        return read_folder_tree(client, token, &repo_id(copy), path, at).await;
    }
    let (tree, _raw) = client
        .get_tree(&repo_id(copy), token, None, true, at)
        .await?;
    Ok(head_of(&tree, None))
}

/// The whole tree of a shared folder's identity at `at` or the tip, every
/// path as served (SPEC u302 `converge` 1, `cmd_push` 1).
pub(crate) async fn read_identity_tree(
    client: &SynsClient,
    token: Option<&str>,
    address: &str,
    at: Option<&str>,
) -> Result<Head, CliError> {
    let (tree, _raw) = client.get_tree(address, token, None, true, at).await?;
    Ok(head_of(&tree, None))
}

/// Whether a refusal is `NOT_FOUND`, the address answering no entry.
fn is_not_found(err: &CliError) -> bool {
    matches!(err, CliError::Api { status: Some(404), error, .. } if error == "not_found")
}

/// The holder's tree under `folder` at `at` or the tip, every path
/// counted from the folder and its commit the holder's, and an empty
/// folder at that commit where the holder holds no folder there (SPEC
/// u291 Behaviour, `read_folder_tree`).
pub(crate) async fn read_folder_tree(
    client: &SynsClient,
    token: Option<&str>,
    repo_id: &str,
    folder: &str,
    at: Option<&str>,
) -> Result<Head, CliError> {
    // 1 — the folder's tree, recursively.
    match client
        .get_tree(repo_id, token, Some(folder), true, at)
        .await
    {
        Ok((tree, _raw)) => Ok(head_of(&tree, Some(folder))),
        Err(err) if is_not_found(&err) => {
            // 2 — the root, not recursively, then the folder again at the
            // commit that answer names.
            let (root, _raw) = client.get_tree(repo_id, token, None, false, at).await?;
            let commit = root.commit_sha.clone();
            match client
                .get_tree(repo_id, token, Some(folder), true, Some(&commit))
                .await
            {
                Ok((tree, _raw)) => Ok(head_of(&tree, Some(folder))),
                Err(err) if is_not_found(&err) => Ok(Head {
                    commit: Some(commit).filter(|c| !c.is_empty()),
                    ..Head::default()
                }),
                Err(err) => Err(err),
            }
        }
        Err(err) => Err(err),
    }
}

/// The bytes of the `.synsignore` at the holder's root at `at` or the
/// tip, none where the holder holds none there or the server answers it
/// to the caller as absent (SPEC u291 Behaviour, `read_holder_synsignore`).
pub async fn read_holder_synsignore(
    client: &SynsClient,
    token: Option<&str>,
    repo_id: &str,
    at: Option<&str>,
) -> Result<Option<Vec<u8>>, CliError> {
    match client
        .get_raw(repo_id, token, ".synsignore", at, None)
        .await
    {
        Ok(raw) => Ok(Some(raw.bytes)),
        Err(CliError::Api {
            status: Some(404),
            ref error,
            ..
        }) if error == "not_found" || error == "repo_not_found" => Ok(None),
        Err(err) => Err(err),
    }
}

/// The holder's root `.synsignore` at `at` or the tip, read through the
/// holder for a folder bound to its identity, every refusal answering
/// none (SPEC u302 Behaviour, `converge` 2): a reader the identity alone
/// admits is answered by the holder as missing.
pub(crate) async fn identity_holder_synsignore(
    client: &SynsClient,
    token: Option<&str>,
    scope: &FolderScope,
    at: Option<&str>,
) -> Option<Vec<u8>> {
    read_holder_synsignore(client, token, &scope.holder(), at)
        .await
        .ok()
        .flatten()
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
    // SPEC u302 `converge` 1: a copy bound to an identity stands on the
    // newest version the identity lists, its tree read at that hash; the
    // tip's tree where it lists none.
    if copy.folder.as_ref().is_some_and(|s| s.identity.is_some())
        && let Some(newest) = identity_head(client, token, &repo_id(copy)).await?
    {
        let mut head = read_tree(client, token, copy, Some(&newest.sha)).await?;
        head.commit = Some(newest.sha);
        return Ok(head);
    }
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
#[allow(clippy::too_many_arguments)]
pub(crate) async fn read_blobs(
    client: &SynsClient,
    token: Option<&str>,
    repo_id: &str,
    folder: Option<&str>,
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
            // SPEC u291 `converge` 1: each wanted path is read at the
            // folder joined with it.
            let remote = match folder {
                Some(folder) => format!("{folder}/{path}"),
                None => path.clone(),
            };
            let hash = hash.clone();
            let held = held.clone();
            set.spawn(async move {
                let blob = read_one(
                    &client,
                    token.as_deref(),
                    &repo_id,
                    &at,
                    &remote,
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

/// Whether `err` is a base read finding the base commit does not hold a
/// wanted path at the hash the base names: `NOT_FOUND`, or the mismatch
/// refusal naming that path and that hash (SPEC u304 Q-01).
fn base_unheld(
    err: &CliError,
    folder: Option<&str>,
    wanted: &BTreeMap<String, (String, Option<u64>)>,
) -> bool {
    if is_not_found(err) {
        return true;
    }
    let CliError::Api {
        status: Some(200),
        error,
        context: None,
    } = err
    else {
        return false;
    };
    wanted.iter().any(|(path, (hash, _))| {
        let remote = match folder {
            Some(folder) => format!("{folder}/{path}"),
            None => path.clone(),
        };
        let named = crate::client::hash_mismatch(&remote, hash, "");
        matches!(named, CliError::Api { error: prefix, .. } if error.starts_with(&prefix))
    })
}

/// A modify/modify collision's base contents read as `read_blobs` reads
/// them, beside every wanted path the base commit does not hold at the
/// hash the base names (SPEC u304 Q-01): where the one read is refused so,
/// each path is read on its own, and a path whose own read is refused so
/// is answered among the unheld rather than read; every other refusal
/// ends the run.
#[allow(clippy::too_many_arguments)]
async fn read_base_blobs(
    client: &SynsClient,
    token: Option<&str>,
    repo_id: &str,
    folder: Option<&str>,
    at: &str,
    wanted: &BTreeMap<String, (String, Option<u64>)>,
    held: &Arc<HeldBytes>,
    staging: &Staging,
) -> Result<(BTreeMap<String, Blob>, BTreeSet<String>), CliError> {
    match read_blobs(client, token, repo_id, folder, at, wanted, held, staging).await {
        Ok(blobs) => return Ok((blobs, BTreeSet::new())),
        Err(err) if base_unheld(&err, folder, wanted) => {}
        Err(err) => return Err(err),
    }
    let mut blobs = BTreeMap::new();
    let mut unheld = BTreeSet::new();
    for (path, entry) in wanted {
        let one = BTreeMap::from([(path.clone(), entry.clone())]);
        match read_blobs(client, token, repo_id, folder, at, &one, held, staging).await {
            Ok(read) => blobs.extend(read),
            Err(err) if base_unheld(&err, folder, &one) => {
                unheld.insert(path.clone());
            }
            Err(err) => return Err(err),
        }
    }
    Ok((blobs, unheld))
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

// ---- where a folder copy is collected from (SPEC u291 `converge` 2) ----

/// Where one run collects a working copy's files from: the walk root, the
/// copy's place under it, and the holder's root `.synsignore` handed to a
/// collection standing in no checkout of its holder. A copy that is no
/// folder walks its own root with neither.
#[derive(Debug, Clone)]
pub(crate) struct FolderRoot {
    root: PathBuf,
    place: Option<String>,
    holder_synsignore: Option<(String, Vec<u8>)>,
}

impl FolderRoot {
    /// The root of a copy collected where it stands.
    pub(crate) fn whole(root: &Path) -> FolderRoot {
        FolderRoot {
            root: root.to_path_buf(),
            place: None,
            holder_synsignore: None,
        }
    }

    /// The root a folder at `folder_dir` is collected from, `holder` the
    /// holder's root `.synsignore` where the scope carries no checkout:
    /// the checkout and the folder's place under it; otherwise the
    /// outermost enclosing folder, or the folder's own directory where
    /// none encloses it, beside that root's recorded path.
    pub(crate) fn of_scope(
        folder_dir: &Path,
        scope: &FolderScope,
        holder: Option<Vec<u8>>,
    ) -> FolderRoot {
        if let Some(checkout) = &scope.checkout {
            return FolderRoot {
                root: checkout.clone(),
                place: Some(scope.path.clone()),
                holder_synsignore: None,
            };
        }
        match scope.enclosing.last() {
            Some(outer) => FolderRoot {
                root: outer.dir.clone(),
                place: Some(scope.path[outer.path.len() + 1..].to_string()),
                holder_synsignore: holder.map(|bytes| (outer.path.clone(), bytes)),
            },
            None => FolderRoot {
                root: folder_dir.to_path_buf(),
                place: None,
                holder_synsignore: holder.map(|bytes| (scope.path.clone(), bytes)),
            },
        }
    }
}

/// The root one run collects `copy` from (SPEC u291 `converge` 2): a
/// folder standing in no checkout of its holder reads the holder's root
/// `.synsignore` at the commit of the base `folder_base` answers, or at
/// the head's commit where it answers none; every other copy sends
/// nothing.
pub(crate) async fn folder_root(
    client: &SynsClient,
    token: Option<&str>,
    copy: &WorkingCopy,
) -> Result<FolderRoot, CliError> {
    let Some(scope) = &copy.folder else {
        return Ok(FolderRoot::whole(&copy.root));
    };
    if scope.checkout.is_some() {
        return Ok(FolderRoot::of_scope(&copy.root, scope, None));
    }
    let based = folder_base(copy.stores(), scope).and_then(|b| b.commit_sha().map(String::from));
    // SPEC u302 `converge` 2: through an identity, the holder's file at the
    // base's commit or the identity's newest, read through the holder.
    if scope.identity.is_some() {
        let at = match based {
            Some(commit) => Some(commit),
            None => identity_head(client, token, &repo_id(copy))
                .await?
                .map(|newest| newest.sha),
        };
        let holder = identity_holder_synsignore(client, token, scope, at.as_deref()).await;
        return Ok(FolderRoot::of_scope(&copy.root, scope, holder));
    }
    let at = match based {
        Some(commit) => Some(commit),
        None => match read_folder_tree(client, token, &repo_id(copy), &scope.path, None).await {
            Ok(head) => head.commit,
            Err(err) if is_empty_repository(&err) => None,
            Err(err) => return Err(err),
        },
    };
    let holder = read_holder_synsignore(client, token, &repo_id(copy), at.as_deref()).await?;
    Ok(FolderRoot::of_scope(&copy.root, scope, holder))
}

/// An `--exclude` pattern matched against a path counted from the folder,
/// as at a walk rooted there, for a walk rooted `place` above it: a
/// pattern anchored by a `/` before its last character is anchored under
/// the place, every other one matching at any depth as it stands.
fn exclude_under(place: &str, pattern: &str) -> String {
    let body = match pattern.char_indices().next_back() {
        Some((last, _)) => &pattern[..last],
        None => pattern,
    };
    if !body.contains('/') {
        return pattern.to_string();
    }
    match pattern.strip_prefix('/') {
        Some(rest) => format!("/{place}/{rest}"),
        None => format!("{place}/{pattern}"),
    }
}

/// Collect a copy's files from where `root` places it (SPEC u291
/// `converge` 2): every kept and skipped path counted from the copy,
/// `opts.prefix` counted from the copy too, each `--exclude` pattern
/// matched as at a walk rooted at the copy, and the stat record's keys
/// counted from the copy before and after the walk.
pub(crate) fn collect_in_place(
    root: &FolderRoot,
    excludes: &[String],
    opts: CollectOptions,
    record: Option<&mut StatRecord>,
    held: &Arc<HeldBytes>,
) -> Result<CollectResult, CliError> {
    let mut opts = opts;
    opts.holder_synsignore = root.holder_synsignore.clone();
    let Some(place) = root.place.as_deref().filter(|p| !p.is_empty()) else {
        return collect_files(&root.root, excludes, opts, record, held);
    };
    let under = |path: &str| format!("{place}/{path}");
    let counted = |path: &str| -> Option<String> {
        path.strip_prefix(place)
            .and_then(|rest| rest.strip_prefix('/'))
            .map(str::to_string)
    };
    opts.prefix = Some(match opts.prefix.as_deref() {
        Some(prefix) if !prefix.is_empty() => under(prefix),
        _ => place.to_string(),
    });
    let excludes: Vec<String> = excludes.iter().map(|p| exclude_under(place, p)).collect();
    let mut joined = record.as_deref().map(|r| StatRecord {
        entries: r
            .entries
            .iter()
            .map(|(path, entry)| (under(path), entry.clone()))
            .collect(),
        stamp: r.stamp,
    });
    let collected = collect_files(&root.root, &excludes, opts, joined.as_mut(), held)?;
    if let (Some(record), Some(joined)) = (record, joined) {
        record.entries = joined
            .entries
            .into_iter()
            .filter_map(|(path, entry)| counted(&path).map(|path| (path, entry)))
            .collect();
    }
    let CollectResult {
        files,
        skipped,
        total_walked,
    } = collected;
    Ok(CollectResult {
        files: files
            .into_iter()
            .filter_map(|(path, file)| counted(&path).map(|path| (path, file)))
            .collect(),
        skipped: skipped
            .into_iter()
            .filter_map(|skip| {
                counted(&skip.path).map(|path| SkippedFile {
                    path,
                    reason: skip.reason,
                })
            })
            .collect(),
        total_walked,
    })
}

/// Collect the folder (SPEC u280 `converge` 1): on macOS and Linux through
/// the working copy's stat record, each path in `forget` read again
/// whatever its entry says. A collection taken under the state lock
/// writes the record back with a fresh stamp where it changed an entry or
/// read a file whose entry stood too recent to trust; a refused record
/// write leaves the record as it stood. SPEC u291: from where `root`
/// places the copy.
fn collect_folder(
    copy: &WorkingCopy,
    opts: &SmartPushOptions,
    forget: &[String],
    under_lock: bool,
    root: &FolderRoot,
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
    let collected = collect_in_place(
        root,
        &opts.excludes,
        CollectOptions {
            no_default_excludes: opts.no_default_excludes,
            debug: opts.debug,
            prefix: None,
            holder_synsignore: None,
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

/// The base a run reads `copy` against: a folder copy's through
/// `folder_base` (SPEC u291 `converge` 3), every other copy's own.
fn base_of(copy: &WorkingCopy) -> Option<Manifest> {
    match &copy.folder {
        Some(scope) => folder_base(copy.stores(), scope),
        None => copy.base(),
    }
}

fn load_base(copy: &WorkingCopy) -> Base {
    base_from(base_of(copy).as_ref())
}

/// The commit and path-to-hash map a recorded base holds, an empty `Base`
/// where none is recorded — the one conversion `load_base` and
/// `working_copy_state` read a base manifest through (SPEC u305).
fn base_from(manifest: Option<&Manifest>) -> Base {
    match manifest {
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
/// and not where the head carries other content while the folder holds
/// the file unedited against the base or `written` holds — the identity
/// record a retrieval wrote, through `retrieval_wrote_root_identity` (SPEC
/// u306 Contract Surface, `holds_root_identity`) — that head content is
/// taken like any other.
fn holds_root_identity(
    retrieving: bool,
    written: bool,
    base_files: &BTreeMap<String, String>,
    folder: &Folder,
    head: &Head,
) -> bool {
    let Some(local) = folder.hashes.get(ROOT_IDENTITY) else {
        return false;
    };
    let head_differs = head
        .files
        .get(ROOT_IDENTITY)
        .is_some_and(|remote| remote != local);
    let unedited = base_files.get(ROOT_IDENTITY) == Some(local) || written;
    retrieving && !(head_differs && unedited)
}

/// Whether a publication holds the root identity file out of every
/// comparison of its run, as a retrieval does (u306 TR-01, `SAGA-cli-pull`
/// step 6): the head stands past the base, neither names `.syns.yaml`, the
/// folder holds it, and `held` — `held_root_identity`, the one test the
/// working-copy state and the checkout guard read the record a retrieval
/// wrote through — holds. The file then counts as no local work, so the
/// publication writes no resolution for it and records the head as the
/// base, naming no `.syns.yaml`; it goes out with the next publication,
/// which finds the head at the base's commit.
fn publication_holds_root_identity(
    held: bool,
    base_commit: Option<&String>,
    base_files: &BTreeMap<String, String>,
    folder: &Folder,
    head: &Head,
) -> bool {
    held && head.commit.is_some()
        && head.commit.as_ref() != base_commit
        && folder.hashes.contains_key(ROOT_IDENTITY)
        && !base_files.contains_key(ROOT_IDENTITY)
        && !head.files.contains_key(ROOT_IDENTITY)
}

/// Drop the root identity file from a collected folder, so the exclusion
/// test that follows counts it as a file standing on disk that the
/// comparison leaves alone.
fn hold_root_identity(folder: &mut Folder) {
    folder.remove(ROOT_IDENTITY);
}

/// The entries a convergence records as the base at the head's commit
/// (SPEC u306 Contract Surface, `recorded_entries`): the head's, but where
/// the run held out a root identity file hashing `held` over a base
/// `prior` recorded before the run, and neither the head nor `prior`
/// names the file at `held`, the path keeps `prior`'s entry, none where
/// `prior` names none — so a locally edited identity file the head names
/// otherwise reads as a difference the next publication takes to review.
fn recorded_entries(
    held: Option<&str>,
    prior: Option<&BTreeMap<String, String>>,
    head_files: &BTreeMap<String, String>,
) -> HashMap<String, String> {
    let mut entries = to_hash_map(head_files);
    if let (Some(held), Some(prior)) = (held, prior) {
        let names_held = |files: &BTreeMap<String, String>| {
            files.get(ROOT_IDENTITY).map(String::as_str) == Some(held)
        };
        if !names_held(head_files) && !names_held(prior) {
            match prior.get(ROOT_IDENTITY) {
                Some(entry) => entries.insert(ROOT_IDENTITY.to_string(), entry.clone()),
                None => entries.remove(ROOT_IDENTITY),
            };
        }
    }
    entries
}

/// Whether the root identity file of the copy at `root` is the record a
/// retrieval wrote (SPEC u306 Contract Surface,
/// `retrieval_wrote_root_identity`): a base is recorded, whatever it names
/// for `.syns.yaml`, and the file holds `identity_text` of `owner` and
/// `name`, ASCII letter case aside. A missing or unreadable file answers
/// false. Unlike `held_root_identity`, which the working-copy state and
/// the checkout guard keep, a base naming the file does not answer false,
/// so a checkout whose base a released binary's retrieval left naming the
/// head's file reads its identity text as written.
fn retrieval_wrote_root_identity(
    root: &Path,
    owner: &str,
    name: &str,
    base: Option<&Manifest>,
) -> bool {
    if base.is_none() {
        return false;
    }
    let Ok(bytes) = std::fs::read(root.join(ROOT_IDENTITY)) else {
        return false;
    };
    bytes.eq_ignore_ascii_case(identity_text(owner, name).as_bytes())
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

/// Whether a head at the base's commit holds the base's tree (SPEC u318
/// Contract Surface, `head_holds_base_tree`): every path outside
/// `excluded` that either map names is named by both, at one hash. A head
/// the server committed at other bytes than a publication sent — a moved
/// shared folder's rewritten identity file — fails it, so its paths reach
/// the folder as remote-only changes rather than being published back.
fn head_holds_base_tree(
    base_files: &BTreeMap<String, String>,
    head_files: &BTreeMap<String, String>,
    excluded: &BTreeSet<String>,
) -> bool {
    without(base_files, excluded) == without(head_files, excluded)
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
    /// SPEC u291 `converge` 4: another working copy of the holder standing
    /// over the same files holds a resolution over a path this one would
    /// review, so nothing was prepared; that resolution and that copy's
    /// directory.
    Elsewhere(Resolution, PathBuf),
}

// ---- one review per path across every copy (SPEC u291 `converge` 4) ---

/// The holder's review lock, one per store root and holder, released
/// when dropped or when its process dies.
pub(crate) struct ReviewLock {
    _file: std::fs::File,
}

/// Take the holder's review lock, waiting while another run holds it. A
/// run holding it takes no other working copy's state lock.
///
/// SPEC u298 Behaviour, `review_lock` 1–2: the lock under the default
/// root wherever it could be taken — created where that root takes
/// writes, and otherwise the standing one opened for writing, else for
/// reading — and, where the default root refuses writes and none could be
/// taken there, the lock under the fallback root, or in the copy's write
/// home where no fallback root stands apart from the default.
pub(crate) fn review_lock(copy: &WorkingCopy) -> Result<ReviewLock, CliError> {
    let stores = copy.stores();
    let holder = |root: &Path| {
        root.join("working-copies")
            .join(copy.owner.to_ascii_lowercase())
            .join(copy.name.to_ascii_lowercase())
    };
    let dir = if !stores.default_refused {
        holder(&stores.default)
    } else {
        // 1 — the default root's standing lock, taken where it can be.
        if let Some(file) = take_standing_review_lock(&holder(&stores.default).join("review.lock"))
        {
            return Ok(ReviewLock { _file: file });
        }
        // 2 — the fallback root's, or the write home's.
        if stores.write != stores.default {
            holder(&stores.write)
        } else {
            copy.state_dir.clone()
        }
    };
    let io = |err: std::io::Error| CliError::Io {
        message: format!(
            "could not lock {}: {err}",
            dir.join("review.lock").display()
        ),
    };
    std::fs::create_dir_all(&dir).map_err(io)?;
    let file = OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(dir.join("review.lock"))
        .map_err(io)?;
    file.lock().map_err(io)?;
    Ok(ReviewLock { _file: file })
}

/// The standing review lock at `path` opened for writing, else for
/// reading, and taken, waiting; none where it is absent, opens neither
/// way or refuses the lock call. Creates nothing.
fn take_standing_review_lock(path: &Path) -> Option<std::fs::File> {
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

/// Every folder form naming `owner/name`, letter case aside, standing on
/// disk below `root` — its directory and recorded path — `.git` passed
/// over and a file parsing as neither form left out.
fn folders_below(root: &Path, owner: &str, name: &str) -> Vec<FolderScope> {
    let holder = format!("{owner}/{name}");
    let mut found = Vec::new();
    for entry in ignore::WalkBuilder::new(root)
        .standard_filters(false)
        .hidden(false)
        .parents(false)
        .filter_entry(|entry| entry.file_name() != std::ffi::OsStr::new(".git"))
        .build()
        .flatten()
    {
        if entry.file_name() != std::ffi::OsStr::new(".syns.yaml")
            || !entry.file_type().is_some_and(|t| t.is_file())
        {
            continue;
        }
        let Some(dir) = entry.path().parent() else {
            continue;
        };
        if dir == root {
            continue;
        }
        if let Ok(IdentityForm::Folder {
            holder: standing,
            path,
        }) = read_identity_form(entry.path())
            && standing.eq_ignore_ascii_case(&holder)
        {
            found.push(FolderScope {
                dir: dir.to_path_buf(),
                owner: owner.to_ascii_lowercase(),
                name: name.to_ascii_lowercase(),
                path,
                checkout: None,
                enclosing: Vec::new(),
                identity: None,
            });
        }
    }
    found
}

/// Every other working copy of `copy`'s holder standing over the same
/// files and recording state, each beside the recorded path its paths are
/// counted from — none for the holder's checkout: the holder's at the
/// scope's checkout, each enclosing folder's, and each folder's whose
/// folder form stands on disk below `copy`'s root.
fn copies_over_the_same_files(
    copy: &WorkingCopy,
) -> Result<Vec<(WorkingCopy, Option<String>)>, CliError> {
    let cache = copy.stores();
    let mut copies = Vec::new();
    if let Some(scope) = &copy.folder {
        if let Some(checkout) = &scope.checkout
            && let Some(holder) =
                WorkingCopy::open_existing(cache, &copy.owner, &copy.name, checkout)?
        {
            copies.push((holder, None));
        }
        for enclosing in &scope.enclosing {
            if let Some(other) = WorkingCopy::open_existing_folder(cache, enclosing)? {
                copies.push((other, Some(enclosing.path.clone())));
            }
        }
    }
    for below in folders_below(&copy.root, &copy.owner, &copy.name) {
        if let Some(other) = WorkingCopy::open_existing_folder(cache, &below)? {
            copies.push((other, Some(below.path.clone())));
        }
    }
    copies.retain(|(other, _)| other.state_dir != copy.state_dir);
    Ok(copies)
}

/// A path counted from a copy's root, counted from the holder's root.
fn from_holder_root(recorded: Option<&str>, path: &str) -> String {
    match recorded {
        Some(recorded) => format!("{recorded}/{path}"),
        None => path.to_string(),
    }
}

/// The resolution of another working copy of the holder standing over the
/// same files whose local, remote, collision or combined paths, counted
/// from the holder's root, name one `names` admits, beside that copy's
/// directory (SPEC u291 `converge` 4); none where no copy holds one.
pub(crate) fn resolution_elsewhere(
    copy: &WorkingCopy,
    names: impl Fn(&str) -> bool,
) -> Result<Option<(Resolution, PathBuf)>, CliError> {
    for (other, recorded) in copies_over_the_same_files(copy)? {
        let Some(standing) = other.resolution()? else {
            continue;
        };
        let named = standing
            .local_paths
            .iter()
            .chain(standing.remote_paths.iter())
            .chain(standing.collisions.iter().map(|(path, _)| path))
            .chain(standing.combined_paths.iter())
            .any(|path| names(&from_holder_root(recorded.as_deref(), path)));
        if named {
            return Ok(Some((standing, other.root.clone())));
        }
    }
    Ok(None)
}

struct Candidate<'a> {
    base_commit: Option<String>,
    base_files: BTreeMap<String, String>,
    head: &'a Head,
    publishing: bool,
    /// The resolution a recomputation keeps the recovery id and round of.
    existing: Option<Resolution>,
    /// Write a resolution whatever the reconciliation finds
    /// (`publish_reviewed` 8), but where the pass holds the root identity
    /// file out: the held record a retrieval wrote is no local work, so a
    /// recomputation finding no local path, no collision and no resolution
    /// standing records the head as the base and answers synced, nothing
    /// published (SPEC u306 `converge` 5, under Q-01) — and that only where
    /// `sent_identity_alone` holds.
    force_resolution: bool,
    /// Whether the refused send's tree differed from the refused parent in
    /// the root identity file alone (`SAGA-cli-converge` step 6, u306
    /// CR5-1): the one recomputation `force_resolution` yields to a hold.
    sent_identity_alone: bool,
    /// Whether this run holds the root identity file out (u263), decided
    /// once and applied to every collection, and the held file's hash.
    hold_root_identity: IdentityHold,
    /// Whether the root identity file is the record a retrieval wrote,
    /// through `retrieval_wrote_root_identity` over the base the run
    /// loaded (SPEC u306 `converge` 1), read once for the run.
    written_root_identity: bool,
    /// Whether the root identity file is the record a retrieval wrote,
    /// through `held_root_identity`, the test the working-copy state and
    /// the checkout guard read, over the base the run loaded (u306 TR-01):
    /// what a publication's hold reads where a pass decides it.
    held_root_identity: bool,
    /// Where every collection of the run is taken from (SPEC u291).
    root: &'a FolderRoot,
}

/// A run's hold over the root identity file (u263), carrying the held
/// file's hash so a collection handed in already held still records the
/// base's own entry for it (SPEC u306 `converge` 5, u306 CR2-1).
#[derive(Clone, Debug, PartialEq, Eq)]
enum IdentityHold {
    /// The first pass decides it from its own collection.
    Undecided,
    /// The run holds nothing out.
    Released,
    /// The run holds the file out; the hash it had before it was dropped.
    Held(String),
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
    let mut hold = candidate.hold_root_identity.clone();
    // Paths a pass refused on as changed since its collection, their
    // record entries dropped before the next collection (`D-093`).
    let mut forget: Vec<String> = Vec::new();
    // Whether a pass of this run wrote the snapshot documents, and the
    // paths whose local snapshot a pass of this run took and then left
    // untouched, which the next pass takes afresh (`D-093`).
    let mut snapshots_written = false;
    let mut untouched: BTreeSet<String> = BTreeSet::new();
    // SPEC u291 `converge` 4: the holder's review lock, taken once a pass
    // goes on to write a resolution and released once it is written.
    let mut review: Option<ReviewLock> = None;
    let recorded = served_under(copy);

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
            None => collect_folder(copy, opts, &forget, true, candidate.root)?,
        };
        forget.clear();
        if hold == IdentityHold::Undecided {
            hold = match folder.hashes.get(ROOT_IDENTITY) {
                Some(hash)
                    if holds_root_identity(
                        !candidate.publishing,
                        candidate.written_root_identity,
                        &candidate.base_files,
                        &folder,
                        head,
                    ) || candidate.publishing
                        && publication_holds_root_identity(
                            candidate.held_root_identity,
                            candidate.base_commit.as_ref(),
                            &candidate.base_files,
                            &folder,
                            head,
                        ) =>
                {
                    IdentityHold::Held(hash.clone())
                }
                _ => IdentityHold::Released,
            };
        }
        // SPEC u306 `converge` 5: the held file's hash, kept for the base
        // a synced pass records — the hold's own where the collection the
        // caller handed in is already held (u306 CR1-1, CR2-1).
        let mut held_hash: Option<String> = None;
        let holding = matches!(hold, IdentityHold::Held(_));
        if let IdentityHold::Held(hash) = &hold {
            held_hash = Some(
                folder
                    .hashes
                    .get(ROOT_IDENTITY)
                    .cloned()
                    .unwrap_or_else(|| hash.clone()),
            );
            hold_root_identity(&mut folder);
        }
        let excluded = excluded_on_disk(
            copy,
            &folder.hashes,
            candidate.base_files.keys().chain(head.files.keys()),
        );
        // SPEC u306 `converge` 5: a run of either mode over the identity
        // record a retrieval wrote, not held, while the head names the
        // file at other bytes, reads the base as naming it at the
        // folder's hash, so the head's bytes replace it as a remote-only
        // change no publication carries; a head naming no `.syns.yaml`
        // leaves the file compared as it stands. `candidate.base_files`
        // stays whole for the base reads.
        let mut compared_base = without(&candidate.base_files, &excluded);
        if !holding
            && candidate.written_root_identity
            && let Some(local) = folder.hashes.get(ROOT_IDENTITY)
            && head
                .files
                .get(ROOT_IDENTITY)
                .is_some_and(|remote| remote != local)
        {
            compared_base.insert(ROOT_IDENTITY.to_string(), local.clone());
        }
        let mut rec = reconcile(
            &compared_base,
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
                recorded,
                &head_commit,
                &head_wanted,
                &held,
                staging,
            )
            .await?
        };
        let base_blobs = match (&candidate.base_commit, base_wanted.is_empty()) {
            (Some(base_commit), false) => {
                let (blobs, unheld) = read_base_blobs(
                    client,
                    token,
                    &repo,
                    recorded,
                    base_commit,
                    &base_wanted,
                    &held,
                    staging,
                )
                .await?;
                // SPEC u304 Q-01: a base path its commit does not hold at
                // the hash the base names reads as added on both sides.
                for (path, kind) in rec.collisions.iter_mut() {
                    if *kind == CollisionKind::ModifyModify && unheld.contains(path) {
                        *kind = CollisionKind::AddAdd;
                    }
                }
                blobs
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

        // SPEC u291 `converge` 4 — where this pass goes on to write a
        // resolution, take the holder's review lock, then prepare nothing
        // over a path another copy standing over the same files holds a
        // resolution for.
        // SPEC u306 `converge` 5 under Q-01: a recomputation holding the
        // root identity file out, after a send differing from its parent
        // in that file alone, is forced to no resolution, so one finding
        // no local path, no collision and none standing records the head.
        let forced = candidate.force_resolution && !(holding && candidate.sent_identity_alone);
        let resolving = forced
            || resolution.is_some()
            || !collisions.is_empty()
            || !rec.collisions.is_empty()
            || !held_collisions.is_empty()
            || candidate.publishing && (!local_paths.is_empty() || !rec.local_only.is_empty());
        if resolving {
            if review.is_none() {
                review = Some(review_lock(copy)?);
            }
            let ours: BTreeSet<String> = local_paths
                .iter()
                .chain(rec.local_only.iter())
                .chain(remote_paths.iter())
                .chain(rec.remote_only.iter())
                .chain(collisions.keys())
                .chain(rec.collisions.iter().map(|(path, _)| path))
                .chain(held_collisions.iter().map(|(path, _)| path))
                .chain(resumed_combined.iter())
                .chain(candidate_hashes.keys())
                .map(|path| from_holder_root(recorded, path))
                .collect();
            if let Some((standing, dir)) = resolution_elsewhere(copy, |path| ours.contains(path))? {
                return Ok(Prepared::Elsewhere(standing, dir));
            }
        }

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

        let needs_resolution = forced
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
        // The resolution stands: the review lock goes with it.
        review = None;

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
                    // SPEC u306 `converge` 5: the base recorded is
                    // `recorded_entries`'.
                    if let Some(commit) = &head.commit {
                        let prior = candidate
                            .base_commit
                            .is_some()
                            .then_some(&candidate.base_files);
                        copy.record_base(
                            commit,
                            recorded_entries(held_hash.as_deref(), prior, &head.files),
                        )?;
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
        Prepared::Resolution(resolution) => SyncOutcome::ResolutionRequired(resolution, None),
        Prepared::Attention(resolution) => SyncOutcome::AttentionRequired(resolution),
        Prepared::Elsewhere(resolution, dir) => {
            SyncOutcome::ResolutionRequired(resolution, Some(dir))
        }
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
    let has_base = base_of(copy).is_some();
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
    let before = recorded_at_of(copy);
    let outcome = converge_locked(client, token, copy, mode, opts).await;
    // SPEC u291 `converge` 6: with the folder copy's lock released, the
    // base the run recorded is laid over every copy enclosing it.
    lay_recorded_base(copy, before, &outcome);
    outcome
}

/// The time the base a folder copy records carries, read before a run so
/// the run can tell whether it recorded one; none for every other copy.
fn recorded_at_of(copy: &WorkingCopy) -> Option<Option<Option<u64>>> {
    copy.folder
        .as_ref()
        .map(|_| copy.base().map(|base| base.recorded_at()))
}

/// Where a run on a folder copy recorded a base, lay it over every copy
/// enclosing the folder, the commit a publication landed standing in for
/// the parent it claimed (SPEC u291 `converge` 6).
fn lay_recorded_base(
    copy: &WorkingCopy,
    before: Option<Option<Option<u64>>>,
    outcome: &Result<SyncOutcome, CliError>,
) {
    let Some(before) = before else {
        return;
    };
    let Some(base) = copy.base() else {
        return;
    };
    if Some(base.recorded_at()) == before {
        return;
    }
    let published = match outcome {
        Ok(SyncOutcome::Synced {
            published: Some((response, _, meta)),
            ..
        }) if !response.commit_sha.is_empty() => {
            Some((meta.sent_parent.clone(), response.commit_sha.clone()))
        }
        _ => None,
    };
    lay_over_enclosing(copy, &base, published);
}

/// Lay `base`, recorded on the folder copy `copy`, over the base of the
/// holder's working copy at the scope's checkout and of each enclosing
/// folder's copy, one at a time under that copy's lock (SPEC u291
/// Behaviour, `converge` 6): each folder path laid at its place, each path
/// under the folder the folder's base lacks taken out, the copy's commit
/// standing — or becoming the landed commit where `published` claimed it
/// as its parent — and the copy's own recorded time kept. A copy recording
/// no base is left with none, and a failed write leaves a base as it stood.
pub(crate) fn lay_over_enclosing(
    copy: &WorkingCopy,
    base: &Manifest,
    published: Option<(Option<String>, String)>,
) {
    let Some(scope) = &copy.folder else {
        return;
    };
    let cache = copy.stores();
    let mut targets: Vec<(WorkingCopy, Option<String>)> = Vec::new();
    if let Some(checkout) = &scope.checkout
        && let Ok(Some(holder)) =
            WorkingCopy::open_existing(cache, &copy.owner, &copy.name, checkout)
    {
        targets.push((holder, None));
    }
    for enclosing in &scope.enclosing {
        if let Ok(Some(other)) = WorkingCopy::open_existing_folder(cache, enclosing) {
            targets.push((other, Some(enclosing.path.clone())));
        }
    }
    for (target, recorded) in targets {
        let Ok(_lock) = target.lock() else {
            continue;
        };
        let Some(standing) = target.base() else {
            continue;
        };
        // A path of the folder counted from the target's root.
        let counted = |path: &str| -> String {
            let from_holder = format!("{}/{path}", scope.path);
            match &recorded {
                Some(recorded) => from_holder[recorded.len() + 1..].to_string(),
                None => from_holder,
            }
        };
        let mut files: HashMap<String, String> = standing
            .file_paths()
            .filter(|path| !lies_under(&from_holder_root(recorded.as_deref(), path), &scope.path))
            .filter_map(|path| {
                standing
                    .file_sha(path)
                    .map(|sha| (path.to_string(), sha.to_string()))
            })
            .collect();
        for path in base.file_paths() {
            if let Some(sha) = base.file_sha(path) {
                files.insert(counted(path), sha.to_string());
            }
        }
        let commit = match (&published, standing.commit_sha()) {
            (Some((Some(parent), landed)), Some(commit)) if commit == parent => landed.clone(),
            (_, commit) => commit.unwrap_or_default().to_string(),
        };
        if let Err(err) = target.record_laid_base(&commit, files, standing.recorded_at()) {
            eprintln!("warning: could not record the working copy base: {err}");
        }
    }
}

/// A publication from a whole-repository copy with no resolution standing
/// refuses a root identity file holding a marked block, before any request
/// and before an outbox resumes, as it was refused before the root readers
/// took that file by its local side (u308 round 2, ruled). A standing
/// resolution is answered as it stands, and a folder copy's identity file
/// is the folder form.
fn refuse_unresolved_marked_identity(copy: &WorkingCopy) -> Result<(), CliError> {
    if copy.folder.is_some() || copy.resolution()?.is_some() {
        return Ok(());
    }
    refuse_marked_root_identity(&copy.root)
}

/// `converge` under the copy's state lock, held for the whole run.
async fn converge_locked(
    client: &SynsClient,
    token: Option<&str>,
    copy: &WorkingCopy,
    mode: ConvergeMode,
    opts: SmartPushOptions,
) -> Result<SyncOutcome, CliError> {
    let _lock = copy.lock()?;
    if mode == ConvergeMode::Publish {
        refuse_unresolved_marked_identity(copy)?;
    }
    let opts = with_run_budget(opts);

    // 1 — the run's staging, then the one collection the run hands on,
    // taken from where the copy stands (SPEC u291 `converge` 2).
    let staging = Staging::open(&opts.stores.write)?;
    let root = folder_root(client, token, copy).await?;
    let folder = collect_folder(copy, &opts, &[], true, &root)?;

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
            // u308 CR2-1: a retrieval resuming an interrupted publication
            // publishes, so it is refused as a publication is.
            Some(token) => match refuse_unresolved_marked_identity(copy) {
                Err(err) => Err(err),
                Ok(()) => {
                    publish_reviewed(
                        client,
                        token,
                        copy,
                        opts,
                        &staging,
                        Some(parent),
                        Some(folder),
                        None,
                        &root,
                    )
                    .await
                }
            },
        },
        Ok(OutboxStep::CarryOn) => {
            converge_from_resolution(
                client,
                token,
                copy,
                mode,
                opts,
                &staging,
                Some(folder),
                &root,
            )
            .await
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
#[allow(clippy::too_many_arguments)]
async fn converge_from_resolution(
    client: &SynsClient,
    token: Option<&str>,
    copy: &WorkingCopy,
    mode: ConvergeMode,
    opts: SmartPushOptions,
    staging: &Staging,
    folder: Option<Folder>,
    root: &FolderRoot,
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
                root,
            )
            .await;
        }
        if standing.round > ROUND_BOUND {
            return Ok(SyncOutcome::AttentionRequired(resolution));
        }
        match mode {
            ConvergeMode::Publish if standing.reviewed_tree.is_some() => {
                let token = token.ok_or(CliError::AuthRequired)?;
                return publish_reviewed(
                    client, token, copy, opts, staging, None, folder, None, root,
                )
                .await;
            }
            ConvergeMode::Retrieve { overwrite: true } => {}
            _ => return Ok(SyncOutcome::ResolutionRequired(standing.clone(), None)),
        }
    }

    // 5 — the one base read, held as its manifest for the written test
    // and as the `Base` every comparison reads.
    let base_manifest = base_of(copy);
    let base = base_from(base_manifest.as_ref());
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
    // SPEC u306 `converge` 1: decided once, over whether the file is the
    // record a retrieval wrote, and the held file's hash kept for the base
    // the run records.
    let mut folder = match folder {
        Some(folder) => folder,
        None => collect_folder(copy, &opts, &[], true, root)?,
    };
    let written =
        retrieval_wrote_root_identity(&copy.root, &copy.owner, &copy.name, base_manifest.as_ref());
    let held_identity =
        held_root_identity(&copy.root, &copy.owner, &copy.name, base_manifest.as_ref());
    // u306 TR-01: a publication past the base holds it out alike where
    // neither side names it and it is the record a retrieval wrote.
    let hold = holds_root_identity(
        matches!(mode, ConvergeMode::Retrieve { .. }),
        written,
        &base.files,
        &folder,
        &head,
    ) || mode == ConvergeMode::Publish
        && publication_holds_root_identity(
            held_identity,
            base.commit.as_ref(),
            &base.files,
            &folder,
            &head,
        );
    let mut held: Option<String> = None;
    if hold {
        held = folder.hashes.get(ROOT_IDENTITY).cloned();
        hold_root_identity(&mut folder);
    }
    let recorded = recorded_entries(
        held.as_deref(),
        base_manifest.as_ref().map(|_| &base.files),
        &head.files,
    );
    let excluded = excluded_on_disk(
        copy,
        &folder.hashes,
        base.files.keys().chain(head.files.keys()),
    );
    let head_files = without(&head.files, &excluded);

    // 7 — SPEC u306 `converge` 2: the base recorded is `recorded_entries`'.
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
            recorded,
        )
        .await;
    }

    // 8 — SPEC u306 `converge` 3: the base recorded is `recorded_entries`'.
    // SPEC u318 `converge` 1: a head at the base's commit naming a path
    // otherwise than the base records `recorded_entries`' at that commit,
    // so the next run compares against the head the folder already holds.
    if folder.hashes == head_files {
        return match &head.commit {
            Some(commit) if base.commit.as_deref() != Some(commit.as_str()) => {
                copy.record_base(commit, recorded)?;
                Ok(SyncOutcome::Synced {
                    written: Vec::new(),
                    removed: Vec::new(),
                    published: None,
                })
            }
            Some(commit) if !head_holds_base_tree(&base.files, &head.files, &excluded) => {
                copy.record_base(commit, recorded)?;
                Ok(SyncOutcome::NoChanges)
            }
            _ => Ok(SyncOutcome::NoChanges),
        };
    }

    // 9 — SPEC u306 `converge` 4: only where the base and the head name
    // the root identity file alike, and no identity record a retrieval
    // wrote stands while the head names that path at other bytes; a base
    // keeping its own entry for a held file the head names otherwise, and
    // a written file a released binary's base names at the head's bytes,
    // go on to the candidate. SPEC u318 `converge` 2 and 3: and only where
    // the head holds the base's tree, every other path included, so a
    // head the server committed at other bytes than were sent goes on to
    // the candidate, its paths taken as remote-only changes.
    let head_replaces_written = written
        && folder
            .hashes
            .get(ROOT_IDENTITY)
            .is_some_and(|local| head.files.get(ROOT_IDENTITY).is_some_and(|r| r != local));
    if head.commit == base.commit
        && base.files.get(ROOT_IDENTITY) == head.files.get(ROOT_IDENTITY)
        && !head_replaces_written
        && head_holds_base_tree(&base.files, &head.files, &excluded)
    {
        return match mode {
            ConvergeMode::Retrieve { .. } => Ok(SyncOutcome::NoChanges),
            ConvergeMode::Publish => {
                let token = token.ok_or(CliError::AuthRequired)?;
                publish_reviewed(
                    client,
                    token,
                    copy,
                    opts,
                    staging,
                    None,
                    Some(folder),
                    None,
                    root,
                )
                .await
            }
        };
    }

    // SPEC u292 `converge` 1 and 2 — a folder copy whose base trails the
    // head only by versions that changed no path under the folder
    // publishes at the head with no resolution written.
    if mode == ConvergeMode::Publish
        && let Some(scope) = &copy.folder
        && let (Some(base_commit), Some(head_commit)) = (&base.commit, &head.commit)
        && head_files == without(&base.files, &excluded)
    {
        let token = token.ok_or(CliError::AuthRequired)?;
        let mut check = FolderCheck {
            repo_id: repo_id(copy),
            folder: scope.path.clone(),
            since: base_commit.clone(),
            since_version: None,
            holder: check_holder(scope),
        };
        if !check.folder_moved(client, Some(token)).await? {
            let at_head = (check, head_commit.clone(), head.files.clone());
            return publish_reviewed(
                client,
                token,
                copy,
                opts,
                staging,
                None,
                Some(folder),
                Some(at_head),
                root,
            )
            .await;
        }
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
            sent_identity_alone: false,
            hold_root_identity: match held {
                Some(hash) => IdentityHold::Held(hash),
                None => IdentityHold::Released,
            },
            written_root_identity: written,
            held_root_identity: held_identity,
            root,
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
    root: &FolderRoot,
) -> Result<SyncOutcome, CliError> {
    let base_manifest = base_of(copy);
    let written =
        retrieval_wrote_root_identity(&copy.root, &copy.owner, &copy.name, base_manifest.as_ref());
    // u306 CR4-1: the publication hold a pass decides reads it alike.
    let held_identity =
        held_root_identity(&copy.root, &copy.owner, &copy.name, base_manifest.as_ref());
    let base = base_from(base_manifest.as_ref());
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
            sent_identity_alone: false,
            hold_root_identity: IdentityHold::Undecided,
            written_root_identity: written,
            held_root_identity: held_identity,
            root,
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
/// removed path snapshotted first and uncollected files left alone, then
/// record `recorded` as the base at the head's commit (SPEC u306
/// `converge` 2).
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
    recorded: HashMap<String, String>,
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
            served_under(copy),
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
        copy.record_base(commit, recorded)?;
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

/// What a folder copy's publication at a head past its base stands on
/// (SPEC u292 `publish_reviewed`, `at_head`): the folder check the run
/// asked, the head's commit, and the head's file hashes.
type AtHead = (FolderCheck, String, BTreeMap<String, String>);

/// `publish_reviewed`, under a lock the caller holds. `resumed` is the
/// head `converge` 3 resumed at; `reviewed` is the collection the run
/// took — the folder `continue_resolution` 3 just recorded, or the one
/// `converge` 1 took — standing in for step 1's walk.
///
/// SPEC u292: `at_head`, set by `converge` 2 alone, names the parent and
/// the reference the first send takes with no tree read; and on a folder
/// copy a moved-head refusal is answered by a send at the named head,
/// with no wait and no round raised, wherever no version after the
/// work's standing version changed the folder — at most
/// `FOLDER_SEND_BOUND` sends in the run.
#[allow(clippy::too_many_arguments)]
async fn publish_reviewed(
    client: &SynsClient,
    token: &str,
    copy: &WorkingCopy,
    opts: SmartPushOptions,
    staging: &Staging,
    resumed: Option<Option<String>>,
    reviewed: Option<Folder>,
    at_head: Option<AtHead>,
    root: &FolderRoot,
) -> Result<SyncOutcome, CliError> {
    let mut reviewed = reviewed;
    let mut forget: Vec<String> = Vec::new();
    let held = opts.held_bytes();
    // SPEC u292: the run's one folder check, the head a pass sends at
    // with the reference it sends over, the tree the outbox at that head
    // recorded, and the sends made.
    let (mut check, mut named) = match at_head {
        Some((check, commit, files)) => (Some(check), Some((commit, files))),
        None => (None, None),
    };
    let mut outbox_tree: Option<BTreeMap<String, String>> = None;
    let mut sends: u32 = 0;
    'passes: for _ in 0..MAX_PASSES {
        // 1 — the run's collection, or a fresh one after a publication
        // pass refused on a file changed since its collection, each such
        // path's record entry dropped first.
        let folder = match reviewed.take() {
            Some(folder) => folder,
            None => collect_folder(copy, &opts, &forget, true, root)?,
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
                    return Ok(SyncOutcome::ResolutionRequired(standing.clone(), None));
                }
                Some(_) => {}
                None => {
                    copy.remove_outbox()?;
                    return Ok(SyncOutcome::ResolutionRequired(standing.clone(), None));
                }
            }

            // 2 — a conflict marker, tested only in the collision paths
            // whose folder bytes are text (SPEC u280 `converge` 7).
            for (path, _kind) in &standing.collisions {
                let Some(file) = folder.files.get(path) else {
                    continue;
                };
                match marked_text(&copy.root, path, file, &held) {
                    Ok(true) => return Ok(SyncOutcome::ResolutionRequired(standing.clone(), None)),
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
                    return Ok(SyncOutcome::ResolutionRequired(standing.clone(), None));
                }
            }
        }

        // 4
        let base = load_base(copy);
        let parent = match (&named, &resumed, &resolution) {
            (Some((head, _)), _, _) => Some(head.clone()),
            (None, Some(head), _) => head.clone(),
            (None, None, Some(standing)) => Some(standing.head_commit.clone()),
            (None, None, None) => base.commit.clone(),
        };
        copy.write_outbox(&Outbox {
            parent_commit: parent.clone(),
            tree: folder.hashes.clone(),
        })?;

        // 5
        // SPEC u292: a send at a named head takes the reference it was
        // handed, reading no tree.
        let reference = match (&named, &parent) {
            (Some((_, files)), _) => files.clone(),
            (None, Some(commit)) if base.commit.as_ref() == Some(commit) => base.files.clone(),
            (None, Some(commit)) => {
                match read_tree(client, Some(token), copy, Some(commit)).await {
                    Ok(tree) => tree.files,
                    Err(err) => return Err(settle_refused_outbox(copy, err)),
                }
            }
            (None, None) => BTreeMap::new(),
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
        // SPEC u292 `publish_reviewed` 3: a send at a named head is held
        // to the tree the outbox at that head recorded.
        push_opts.expected = match outbox_tree.take() {
            Some(tree) => Some(to_hash_map(&tree)),
            None => resolution.as_ref().map(|_| to_hash_map(&hashes)),
        };
        // SPEC u291 `converge` 5: a folder copy publishes under its
        // recorded path, with no local record and no identity file; SPEC
        // u302 `converge` 3: through an identity under an empty one.
        push_opts.folder = copy
            .folder
            .as_ref()
            .map(|_| served_under(copy).unwrap_or_default().to_string());
        // The publication walks nothing: it publishes from this pass's
        // collection (SPEC u280 `converge` 1).
        push_opts.collected = Some(folder.into_collected());

        let sent = smart_push(client, token, &repo_id(copy), &copy.root, push_opts).await;
        if !matches!(sent, Err(CliError::CollectedSetChanged { .. })) {
            sends += 1;
        }
        match sent {
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
                context: Some(ApiErrorContext::HeadMoved { ref current_sha }),
            }) if error == "conflict" => {
                // 7
                copy.remove_outbox()?;
                // SPEC u292 `publish_reviewed` 1 to 3 — on a folder copy
                // below the bound, the run's one folder check decides
                // between a send at the named head and the round.
                if let Some(scope) = &copy.folder
                    && sends < FOLDER_SEND_BOUND
                    && let Some(since) = &parent
                {
                    let check = check.get_or_insert_with(|| FolderCheck {
                        repo_id: repo_id(copy),
                        folder: scope.path.clone(),
                        since: since.clone(),
                        since_version: None,
                        holder: check_holder(scope),
                    });
                    if !check.folder_moved(client, Some(token)).await? {
                        copy.write_outbox(&Outbox {
                            parent_commit: Some(current_sha.clone()),
                            tree: hashes.clone(),
                        })?;
                        outbox_tree = Some(hashes);
                        named = Some((current_sha.clone(), reference));
                        continue 'passes;
                    }
                }
                return guard_refused(
                    client, token, copy, &opts, staging, resolution, parent, reference, &hashes,
                    root,
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
/// parent standing as the base — forced to a resolution but where the
/// refused send's tree `sent` differed from that parent in the root
/// identity file alone and the recomputation holds that file out, which,
/// finding no other local work, answers synced carrying no publication,
/// the head recorded as the base (SPEC u306 `converge` 5, under Q-01,
/// u306 CR5-1).
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
    sent: &BTreeMap<String, String>,
    root: &FolderRoot,
) -> Result<SyncOutcome, CliError> {
    let sent_identity_alone = differing_paths(&reference, sent) == [ROOT_IDENTITY];
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

    // SPEC u306 `converge` 5: the one base read, for whether a base is
    // recorded and whether the root identity file is the record a
    // retrieval wrote, so the recomputation takes the head's file over it,
    // and — through `held_root_identity` over the base recorded when the
    // send was refused — holds that file out of every comparison wherever
    // `publication_holds_root_identity` holds over the refused parent, the
    // folder and the head read here, so the resolution it writes lists the
    // other local work alone.
    let base_manifest = base_of(copy);
    let written =
        retrieval_wrote_root_identity(&copy.root, &copy.owner, &copy.name, base_manifest.as_ref());
    let held_identity =
        held_root_identity(&copy.root, &copy.owner, &copy.name, base_manifest.as_ref());
    let head = read_head(
        client,
        Some(token),
        copy,
        HeadReading::Publication,
        base_manifest.is_some(),
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
            sent_identity_alone,
            hold_root_identity: IdentityHold::Undecided,
            written_root_identity: written,
            held_root_identity: held_identity,
            root,
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
    let before = recorded_at_of(copy);
    let outcome = continue_locked(client, token, copy, opts).await;
    // SPEC u291 `converge` 6.
    lay_recorded_base(copy, before, &outcome);
    outcome
}

/// `continue_resolution` under the copy's state lock.
async fn continue_locked(
    client: &SynsClient,
    token: &str,
    copy: &WorkingCopy,
    opts: SmartPushOptions,
) -> Result<SyncOutcome, CliError> {
    // 1
    let _lock = copy.lock()?;
    refuse_unresolved_marked_identity(copy)?;
    let opts = with_run_budget(opts);
    let staging = Staging::open(&opts.stores.write)?;
    let root = folder_root(client, Some(token), copy).await?;

    // 2
    match settle_outbox(client, Some(token), copy, ConvergeMode::Publish).await? {
        OutboxStep::Completed(outcome) => return Ok(outcome),
        OutboxStep::Resume(parent) => {
            return publish_reviewed(
                client,
                token,
                copy,
                opts,
                &staging,
                Some(parent),
                None,
                None,
                &root,
            )
            .await;
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
            &root,
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
            &root,
        )
        .await;
    }

    // 3 — the one collection, recorded as reviewed and handed on.
    let folder = collect_folder(copy, &opts, &[], true, &root)?;
    resolution.reviewed_tree = Some(folder.hashes.clone());
    copy.write_resolution(&resolution)?;

    // 4
    publish_reviewed(
        client,
        token,
        copy,
        opts,
        &staging,
        None,
        Some(folder),
        None,
        &root,
    )
    .await
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
/// collection reads through the stat record and writes none back. SPEC
/// u305 `working_copy_state` 4: a root `.syns.yaml` `held_root_identity`
/// holds against the base read here — the record a retrieval wrote —
/// counts as no local work, dropped from the folder side alone.
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

    // 2 — the one base read, held as its manifest for step 4 and as the
    // `Base` the comparison reads.
    let base_manifest = base_of(copy);
    let base = base_from(base_manifest.as_ref());
    let head = read_head(
        client,
        token,
        copy,
        HeadReading::State,
        base.commit.is_some(),
    )
    .await?;

    // 3 — collected from where the copy stands (SPEC u291 `converge` 2).
    let root = folder_root(client, token, copy).await?;
    #[cfg(unix)]
    let mut record = Some(copy.stat_record());
    #[cfg(not(unix))]
    let mut record: Option<StatRecord> = None;
    let collected = collect_in_place(
        &root,
        &[],
        CollectOptions::default(),
        record.as_mut(),
        &HeldBytes::new(HELD_BYTES_BUDGET),
    )?;
    let mut folder: BTreeMap<String, String> = collected
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

    // 4 — the identity file a retrieval wrote leaves the folder side
    // alone, the head's and the base's entries for the path kept.
    if held_root_identity(&copy.root, &copy.owner, &copy.name, base_manifest.as_ref()) {
        folder.remove(ROOT_IDENTITY);
        if folder == head_files {
            return Ok(WorkingCopyState::Converged);
        }
    }

    // 5
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
    use crate::push::collector::SkipReason;
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
        let copy = WorkingCopy::open(
            &crate::config::StoreRoots::resolve(Some(cache.path()), cache.path(), cache.path()),
            "alice",
            "r",
            folder.path(),
        )
        .unwrap();
        let staging = Staging::open(cache.path()).unwrap();
        let opts = SmartPushOptions {
            force: false,
            message: "push".into(),
            author: None,
            parent_sha: None,
            excludes: vec![],
            stores: crate::config::StoreRoots::resolve(
                Some(cache.path()),
                cache.path(),
                cache.path(),
            ),
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
            folder: None,
            declined_parent: None,
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
                sent_identity_alone: false,
                hold_root_identity: IdentityHold::Released,
                written_root_identity: false,
                held_root_identity: false,
                root: &FolderRoot::whole(&copy.root),
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

    fn bare_opts(cache: &Path) -> SmartPushOptions {
        SmartPushOptions {
            force: false,
            message: "push".into(),
            author: None,
            parent_sha: None,
            excludes: vec![],
            stores: crate::config::StoreRoots::resolve(Some(cache), cache, cache),
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
            folder: None,
            declined_parent: None,
        }
    }

    fn scope_at(dir: &Path, path: &str, checkout: Option<PathBuf>) -> FolderScope {
        FolderScope {
            dir: dir.to_path_buf(),
            owner: "alice".into(),
            name: "r".into(),
            path: path.into(),
            checkout,
            enclosing: Vec::new(),
            identity: None,
        }
    }

    // ---- u291: the folder's reads ----------------------------------------

    // SPEC u291 Behaviour, `read_folder_tree` 1–3: a folder tree counted
    // from the folder, and where the head lacks the folder the root read
    // without recursion and the folder again at the commit it names.
    #[tokio::test]
    async fn a_folder_tree_is_counted_from_the_folder_and_an_absent_one_reads_empty() {
        use wiremock::matchers::{method, path, query_param};
        use wiremock::{Mock, MockServer, ResponseTemplate};
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/v1/repos/alice/r/tree/clients/q3"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "entries": [
                    {"name": "a.md", "path": "clients/q3/a.md", "type": "file", "size": 1, "sha": "1".repeat(40)},
                    {"name": "d", "path": "clients/q3/d", "type": "dir", "size": null, "sha": null},
                ],
                "commitSha": "h1", "truncated": false,
            })))
            .mount(&server)
            .await;
        let not_found =
            || ResponseTemplate::new(404).set_body_json(serde_json::json!({"error": "not_found"}));
        Mock::given(method("GET"))
            .and(path("/api/v1/repos/alice/r/tree/clients/gone"))
            .respond_with(not_found())
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/api/v1/repos/alice/r/tree"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "entries": [], "commitSha": "h3", "truncated": false,
            })))
            .mount(&server)
            .await;
        let client = SynsClient::new(&server.uri()).unwrap();

        let head = read_folder_tree(&client, None, "alice/r", "clients/q3", None)
            .await
            .unwrap();
        assert_eq!(head.commit.as_deref(), Some("h1"));
        assert_eq!(
            head.files,
            BTreeMap::from([("a.md".to_string(), "1".repeat(40))])
        );

        let empty = read_folder_tree(&client, None, "alice/r", "clients/gone", None)
            .await
            .unwrap();
        assert_eq!(empty.commit.as_deref(), Some("h3"));
        assert!(empty.files.is_empty());
        let requests = server.received_requests().await.unwrap();
        let reads: Vec<(String, Option<String>, Option<String>)> = requests[1..]
            .iter()
            .map(|r| {
                let query = |key: &str| {
                    r.url
                        .query_pairs()
                        .find(|(k, _)| k == key)
                        .map(|(_, v)| v.to_string())
                };
                (r.url.path().to_string(), query("recursive"), query("ref"))
            })
            .collect();
        assert_eq!(
            reads,
            vec![
                (
                    "/api/v1/repos/alice/r/tree/clients/gone".to_string(),
                    Some("true".to_string()),
                    None
                ),
                ("/api/v1/repos/alice/r/tree".to_string(), None, None),
                (
                    "/api/v1/repos/alice/r/tree/clients/gone".to_string(),
                    Some("true".to_string()),
                    Some("h3".to_string())
                ),
            ]
        );
        let _ = query_param("ref", "h3");
    }

    // SPEC u291 Behaviour, `read_holder_synsignore` 1, and `read_blobs`
    // with a folder.
    #[tokio::test]
    async fn the_holder_synsignore_and_folder_contents_are_read_at_their_places() {
        use wiremock::matchers::{method, path, query_param};
        use wiremock::{Mock, MockServer, ResponseTemplate};
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/v1/repos/alice/r/raw/.synsignore"))
            .and(query_param("ref", "h1"))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(b"*.env\n".to_vec()))
            .mount(&server)
            .await;
        for (at, error) in [("h2", "not_found"), ("h3", "repo_not_found")] {
            Mock::given(method("GET"))
                .and(path("/api/v1/repos/alice/r/raw/.synsignore"))
                .and(query_param("ref", at))
                .respond_with(
                    ResponseTemplate::new(404).set_body_json(serde_json::json!({"error": error})),
                )
                .mount(&server)
                .await;
        }
        Mock::given(method("GET"))
            .and(path("/api/v1/repos/alice/r/raw/clients/q3/a.md"))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(b"a".to_vec()))
            .mount(&server)
            .await;
        let client = SynsClient::new(&server.uri()).unwrap();
        assert_eq!(
            read_holder_synsignore(&client, None, "alice/r", Some("h1"))
                .await
                .unwrap(),
            Some(b"*.env\n".to_vec())
        );
        for at in ["h2", "h3"] {
            assert_eq!(
                read_holder_synsignore(&client, None, "alice/r", Some(at))
                    .await
                    .unwrap(),
                None,
                "{at}"
            );
        }
        let cache = tempfile::tempdir().unwrap();
        let staging = Staging::open(cache.path()).unwrap();
        let wanted = BTreeMap::from([("a.md".to_string(), (blob_sha1(b"a"), Some(1)))]);
        let blobs = read_blobs(
            &client,
            None,
            "alice/r",
            Some("clients/q3"),
            "h1",
            &wanted,
            &HeldBytes::new(HELD_BYTES_BUDGET),
            &staging,
        )
        .await
        .unwrap();
        assert_eq!(&*blobs["a.md"].load().unwrap(), b"a");
    }

    // SPEC u291 `converge` 2: a folder copy inside a checkout is collected
    // as a collection rooted at the checkout and confined to the folder,
    // the checkout's ignore files applying, every path counted from the
    // folder.
    #[test]
    fn a_folder_copy_is_collected_from_its_checkout() {
        let cache = tempfile::tempdir().unwrap();
        let tree = tempfile::tempdir().unwrap();
        let w = std::fs::canonicalize(tree.path()).unwrap();
        let folder = w.join("clients/q3");
        std::fs::create_dir_all(folder.join("sub")).unwrap();
        std::fs::write(w.join(".synsignore"), "*.env\n").unwrap();
        std::fs::write(w.join("outside.md"), "o").unwrap();
        std::fs::write(folder.join("a.md"), "a").unwrap();
        std::fs::write(folder.join("k.env"), "k").unwrap();
        std::fs::write(folder.join("sub/b.md"), "b").unwrap();
        let scope = scope_at(&folder, "clients/q3", Some(w.clone()));
        let copy = WorkingCopy::open_folder(
            &crate::config::StoreRoots::resolve(Some(cache.path()), cache.path(), cache.path()),
            &scope,
        )
        .unwrap();
        let root = FolderRoot::of_scope(&copy.root, &scope, None);

        let collected = collect_folder(&copy, &bare_opts(cache.path()), &[], true, &root).unwrap();
        assert_eq!(
            collected.hashes.keys().cloned().collect::<Vec<_>>(),
            vec!["a.md".to_string(), "sub/b.md".to_string()]
        );
        assert_eq!(
            collected
                .skipped
                .iter()
                .map(|s| (s.path.clone(), s.reason))
                .collect::<Vec<_>>(),
            vec![("k.env".to_string(), SkipReason::Synsignore)]
        );
        #[cfg(unix)]
        assert!(
            copy.stat_record()
                .entries
                .keys()
                .all(|path| !path.starts_with("clients/")),
            "the record counts its paths from the folder"
        );
        assert_eq!(exclude_under("clients/q3", "*.tmp"), "*.tmp");
        assert_eq!(
            exclude_under("clients/q3", "sub/b.md"),
            "clients/q3/sub/b.md"
        );
        assert_eq!(exclude_under("clients/q3", "/a.md"), "/clients/q3/a.md");
        assert_eq!(exclude_under("clients/q3", "dist/"), "dist/");
        // CR1-1: a pattern ending in a character of more than one byte.
        assert_eq!(exclude_under("clients/q3", "résumé"), "résumé");
        assert_eq!(
            exclude_under("clients/q3", "docs/résumé"),
            "clients/q3/docs/résumé"
        );
    }

    // SPEC u291 Behaviour, `converge` 6.
    #[test]
    fn a_folder_base_is_laid_over_its_holders_base() {
        let cache = tempfile::tempdir().unwrap();
        let tree = tempfile::tempdir().unwrap();
        let w = std::fs::canonicalize(tree.path()).unwrap();
        std::fs::create_dir_all(w.join("clients/q3")).unwrap();
        let holder = WorkingCopy::open(
            &crate::config::StoreRoots::resolve(Some(cache.path()), cache.path(), cache.path()),
            "alice",
            "r",
            &w,
        )
        .unwrap();
        holder
            .record_laid_base(
                "h1",
                HashMap::from([
                    (".page/x.json".to_string(), "x1".to_string()),
                    ("clients/q3/a.md".to_string(), "a1".to_string()),
                    ("clients/q3/gone.md".to_string(), "g1".to_string()),
                ]),
                Some(5),
            )
            .unwrap();
        let scope = scope_at(&w.join("clients/q3"), "clients/q3", Some(w.clone()));
        let folder = WorkingCopy::open_folder(
            &crate::config::StoreRoots::resolve(Some(cache.path()), cache.path(), cache.path()),
            &scope,
        )
        .unwrap();
        folder
            .record_base(
                "h3",
                HashMap::from([
                    ("a.md".to_string(), "a3".to_string()),
                    ("b.md".to_string(), "b3".to_string()),
                ]),
            )
            .unwrap();

        lay_over_enclosing(
            &folder,
            &folder.base().unwrap(),
            Some((Some("h1".into()), "h3".into())),
        );
        let laid = holder.base().unwrap();
        assert_eq!(laid.commit_sha(), Some("h3"));
        assert_eq!(laid.file_sha(".page/x.json"), Some("x1"));
        assert_eq!(laid.file_sha("clients/q3/a.md"), Some("a3"));
        assert_eq!(laid.file_sha("clients/q3/b.md"), Some("b3"));
        assert_eq!(laid.file_sha("clients/q3/gone.md"), None);
        assert_eq!(laid.recorded_at(), Some(5));

        // A retrieval keeps the commit standing.
        lay_over_enclosing(&folder, &folder.base().unwrap(), None);
        assert_eq!(holder.base().unwrap().commit_sha(), Some("h3"));

        // A copy recording no base is left with none.
        let bare = tempfile::tempdir().unwrap();
        let v = std::fs::canonicalize(bare.path()).unwrap();
        std::fs::create_dir_all(v.join("clients/q3")).unwrap();
        let other_holder = WorkingCopy::open(
            &crate::config::StoreRoots::resolve(Some(cache.path()), cache.path(), cache.path()),
            "alice",
            "r",
            &v,
        )
        .unwrap();
        let other_scope = scope_at(&v.join("clients/q3"), "clients/q3", Some(v.clone()));
        let other_folder = WorkingCopy::open_folder(
            &crate::config::StoreRoots::resolve(Some(cache.path()), cache.path(), cache.path()),
            &other_scope,
        )
        .unwrap();
        other_folder
            .record_base(
                "h3",
                HashMap::from([("a.md".to_string(), "a3".to_string())]),
            )
            .unwrap();
        lay_over_enclosing(&other_folder, &other_folder.base().unwrap(), None);
        assert!(other_holder.base().is_none());
    }

    // SPEC u291 Behaviour, `converge` 4: a preparation over a path
    // another copy of the holder holds a resolution for prepares nothing
    // and answers that resolution and that copy's directory.
    #[tokio::test(flavor = "current_thread")]
    async fn a_review_pending_in_another_copy_holds_this_one_off_its_paths() {
        use wiremock::matchers::{method, path, query_param};
        use wiremock::{Mock, MockServer, ResponseTemplate};
        let base: &[u8] = b"a\nb\n";
        let head_bytes: &[u8] = b"a\nHEAD\n";
        let server = MockServer::start().await;
        for (at, bytes) in [("h0", base), ("h1", head_bytes)] {
            Mock::given(method("GET"))
                .and(path("/api/v1/repos/alice/r/raw/f/a.md"))
                .and(query_param("ref", at))
                .respond_with(ResponseTemplate::new(200).set_body_bytes(bytes.to_vec()))
                .mount(&server)
                .await;
        }
        let client = SynsClient::new(&server.uri()).unwrap();
        let cache = tempfile::tempdir().unwrap();
        let tree = tempfile::tempdir().unwrap();
        let w = std::fs::canonicalize(tree.path()).unwrap();
        std::fs::create_dir_all(w.join("f")).unwrap();
        std::fs::write(w.join(".syns.yaml"), "owner: alice\nname: r\n").unwrap();
        std::fs::write(w.join("f/.syns.yaml"), "holder: alice/r\npath: f\n").unwrap();
        std::fs::write(w.join("f/a.md"), b"a\nlocal\n").unwrap();

        let scope = scope_at(&w.join("f"), "f", Some(w.clone()));
        let folder = WorkingCopy::open_folder(
            &crate::config::StoreRoots::resolve(Some(cache.path()), cache.path(), cache.path()),
            &scope,
        )
        .unwrap();
        let standing = Resolution {
            recovery_id: "rec-f".into(),
            base_commit: Some("h0".into()),
            head_commit: "h1".into(),
            round: 1,
            local_paths: vec![],
            remote_paths: vec![],
            collisions: vec![("a.md".into(), CollisionKind::ModifyModify)],
            combined_paths: vec!["a.md".into()],
            reviewed_tree: None,
            pending_writes: None,
        };
        folder.write_resolution(&standing).unwrap();

        let holder = WorkingCopy::open(
            &crate::config::StoreRoots::resolve(Some(cache.path()), cache.path(), cache.path()),
            "alice",
            "r",
            &w,
        )
        .unwrap();
        let staging = Staging::open(cache.path()).unwrap();
        let identity = blob_sha1(b"owner: alice\nname: r\n");
        let folder_identity = blob_sha1(b"holder: alice/r\npath: f\n");
        let head = Head {
            commit: Some("h1".into()),
            files: BTreeMap::from([
                (".syns.yaml".to_string(), identity.clone()),
                ("f/.syns.yaml".to_string(), folder_identity.clone()),
                ("f/a.md".to_string(), blob_sha1(head_bytes)),
            ]),
            sizes: BTreeMap::new(),
            truncated: false,
        };
        let prepared = prepare_candidate(
            &client,
            None,
            &holder,
            &bare_opts(cache.path()),
            &staging,
            Candidate {
                base_commit: Some("h0".into()),
                base_files: BTreeMap::from([
                    (".syns.yaml".to_string(), identity),
                    ("f/.syns.yaml".to_string(), folder_identity),
                    ("f/a.md".to_string(), blob_sha1(base)),
                ]),
                head: &head,
                publishing: false,
                existing: None,
                force_resolution: false,
                sent_identity_alone: false,
                hold_root_identity: IdentityHold::Released,
                written_root_identity: false,
                held_root_identity: false,
                root: &FolderRoot::whole(&holder.root),
            },
            None,
        )
        .await
        .unwrap();
        match prepared {
            Prepared::Elsewhere(resolution, dir) => {
                assert_eq!(resolution, standing);
                assert_eq!(dir, folder.root);
            }
            _ => panic!("expected the review standing elsewhere"),
        }
        assert!(holder.resolution().unwrap().is_none());
        assert!(!holder.local_snapshot_path().exists());
        assert!(!holder.remote_snapshot_path().exists());
        assert_eq!(std::fs::read(w.join("f/a.md")).unwrap(), b"a\nlocal\n");
    }

    // CR1-3, SPEC u291 Behaviour `converge` 4: a preparation going on to
    // write a resolution waits on the holder's review lock, and once it
    // takes it answers the review written under it by another copy over
    // the same path, writing nothing of its own. The collision — a local
    // edit the head deleted — reads no content, so the preparation reaches
    // the lock at once.
    #[test]
    fn a_preparation_waits_on_the_review_lock_and_then_answers_the_review_written_under_it() {
        let cache = tempfile::tempdir().unwrap();
        let tree = tempfile::tempdir().unwrap();
        let w = std::fs::canonicalize(tree.path()).unwrap();
        std::fs::create_dir_all(w.join("f")).unwrap();
        std::fs::write(w.join(".syns.yaml"), "owner: alice\nname: r\n").unwrap();
        std::fs::write(w.join("f/.syns.yaml"), "holder: alice/r\npath: f\n").unwrap();
        std::fs::write(w.join("f/a.md"), b"a\nlocal\n").unwrap();
        let holder = WorkingCopy::open(
            &crate::config::StoreRoots::resolve(Some(cache.path()), cache.path(), cache.path()),
            "alice",
            "r",
            &w,
        )
        .unwrap();
        let scope = scope_at(&w.join("f"), "f", Some(w.clone()));
        let folder = WorkingCopy::open_folder(
            &crate::config::StoreRoots::resolve(Some(cache.path()), cache.path(), cache.path()),
            &scope,
        )
        .unwrap();
        let standing = Resolution {
            recovery_id: "rec-w".into(),
            base_commit: Some("h0".into()),
            head_commit: "h1".into(),
            round: 1,
            local_paths: vec![],
            remote_paths: vec![],
            collisions: vec![("f/a.md".into(), CollisionKind::ModifyDelete)],
            combined_paths: vec!["f/a.md".into()],
            reviewed_tree: None,
            pending_writes: None,
        };

        // Everything slow to build is built before the lock is taken, so
        // the preparation alone stands between the spawn and the lock.
        let client = SynsClient::new("http://127.0.0.1:9").unwrap();
        let staging = Staging::open(cache.path()).unwrap();
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let held = review_lock(&holder).unwrap();
        std::thread::scope(|threads| {
            let preparing = threads.spawn(|| {
                runtime.block_on(async {
                    let identity = blob_sha1(b"holder: alice/r\npath: f\n");
                    let head = Head {
                        commit: Some("h1".into()),
                        files: BTreeMap::from([(".syns.yaml".to_string(), identity.clone())]),
                        sizes: BTreeMap::new(),
                        truncated: false,
                    };
                    prepare_candidate(
                        &client,
                        None,
                        &folder,
                        &bare_opts(cache.path()),
                        &staging,
                        Candidate {
                            base_commit: Some("h0".into()),
                            base_files: BTreeMap::from([
                                (".syns.yaml".to_string(), identity),
                                ("a.md".to_string(), blob_sha1(b"a\nb\n")),
                            ]),
                            head: &head,
                            publishing: false,
                            existing: None,
                            force_resolution: false,
                            sent_identity_alone: false,
                            hold_root_identity: IdentityHold::Released,
                            written_root_identity: false,
                            held_root_identity: false,
                            root: &FolderRoot::of_scope(&folder.root, &scope, None),
                        },
                        None,
                    )
                    .await
                })
            });
            std::thread::sleep(Duration::from_millis(300));
            assert!(
                !preparing.is_finished(),
                "the preparation did not wait on the review lock"
            );
            holder.write_resolution(&standing).unwrap();
            drop(held);
            match preparing.join().unwrap().unwrap() {
                Prepared::Elsewhere(resolution, dir) => {
                    assert_eq!(resolution, standing);
                    assert_eq!(dir, holder.root);
                }
                _ => panic!("expected the review standing in the holder's checkout"),
            }
        });
        assert!(!folder.local_snapshot_path().exists());
        assert!(!folder.remote_snapshot_path().exists());
        assert!(folder.resolution().unwrap().is_none());
        assert_eq!(std::fs::read(w.join("f/a.md")).unwrap(), b"a\nlocal\n");
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

            let answers = read_blobs(
                &client, None, "alice/r", None, "h", &wanted, &held, &staging,
            )
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
            None,
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
            None,
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

    // SPEC u298 `IN_ROOT_HOME`: a served path in the in-root home, letter
    // case aside, reads from a head as absent.
    #[test]
    fn a_head_reads_the_in_root_home_as_absent() {
        let entry = |path: &str| crate::client::TreeEntry {
            name: path.rsplit('/').next().unwrap_or(path).to_string(),
            path: path.to_string(),
            entry_type: EntryType::File,
            size: Some(1),
            sha: Some("1".repeat(40)),
        };
        let tree = crate::client::TreeResponse {
            entries: vec![
                entry("a.md"),
                entry(".SYNS-STATE/z"),
                entry("f/.syns-state/base.json"),
            ],
            commit_sha: "h1".into(),
            truncated: false,
        };
        let head = head_of(&tree, None);
        assert_eq!(head.files.keys().collect::<Vec<_>>(), vec!["a.md"]);
        assert_eq!(head.sizes.len(), 1);
        assert!(head_of(&tree, Some("f")).files.is_empty());
    }

    // SPEC u298 Tests, `review_lock_falls_to_the_fallback_root`.
    #[cfg(unix)]
    #[test]
    fn review_lock_falls_to_the_fallback_root() {
        use std::os::unix::fs::PermissionsExt;
        if unsafe { libc::geteuid() } == 0 {
            return;
        }
        let scratch = tempfile::tempdir().unwrap();
        let base = std::fs::canonicalize(scratch.path()).unwrap();
        let default = base.join("default");
        let temp = base.join("temp");
        let one = base.join("one");
        let two = base.join("two");
        for dir in [&default, &temp, &one, &two] {
            std::fs::create_dir_all(dir).unwrap();
        }
        std::fs::set_permissions(&default, std::fs::Permissions::from_mode(0o500)).unwrap();
        let stores = crate::config::StoreRoots::resolve(None, &default, &temp);
        assert!(stores.default_refused);
        let first = WorkingCopy::open(&stores, "Alice", "R", &one).unwrap();
        let second = WorkingCopy::open(&stores, "alice", "r", &two).unwrap();

        let held = review_lock(&first).unwrap();
        let standing = crate::config::fallback_root(&temp)
            .join("working-copies")
            .join("alice")
            .join("r")
            .join("review.lock");
        assert!(standing.is_file(), "{}", standing.display());
        assert!(!default.join("working-copies").exists());

        let (taken, waited) = std::sync::mpsc::channel();
        let waiter = std::thread::spawn(move || {
            let lock = review_lock(&second).unwrap();
            taken.send(std::time::Instant::now()).unwrap();
            drop(lock);
        });
        std::thread::sleep(std::time::Duration::from_millis(300));
        assert!(
            waited.try_recv().is_err(),
            "the second copy did not wait on the review lock"
        );
        let released = std::time::Instant::now();
        drop(held);
        let returned = waited.recv().unwrap();
        waiter.join().unwrap();
        assert!(returned >= released);
        std::fs::set_permissions(&default, std::fs::Permissions::from_mode(0o700)).unwrap();
    }

    // SPEC u298 Behaviour, `review_lock` 1: where the default root refuses
    // writes, the holder's standing review lock there is opened for
    // reading where it refuses writing, and queued on.
    #[cfg(unix)]
    #[test]
    fn review_lock_queues_on_a_read_only_standing_default_lock() {
        use std::os::unix::fs::PermissionsExt;
        if unsafe { libc::geteuid() } == 0 {
            return;
        }
        let scratch = tempfile::tempdir().unwrap();
        let base = std::fs::canonicalize(scratch.path()).unwrap();
        let default = base.join("default");
        let fallback = base.join("fallback");
        let root = base.join("root");
        for dir in [&default, &fallback, &root] {
            std::fs::create_dir_all(dir).unwrap();
        }
        let standing = default.join("working-copies/alice/r/review.lock");
        std::fs::create_dir_all(standing.parent().unwrap()).unwrap();
        std::fs::write(&standing, b"").unwrap();
        std::fs::set_permissions(&standing, std::fs::Permissions::from_mode(0o400)).unwrap();
        let stores = crate::config::StoreRoots {
            default: default.clone(),
            write: fallback.clone(),
            default_refused: true,
        };
        let copy = WorkingCopy::open(&stores, "alice", "r", &root).unwrap();
        let holder = std::fs::File::open(&standing).unwrap();
        holder.lock().unwrap();

        let (taken, waited) = std::sync::mpsc::channel();
        let waiter = std::thread::spawn(move || {
            let lock = review_lock(&copy).unwrap();
            taken.send(()).unwrap();
            drop(lock);
        });
        std::thread::sleep(std::time::Duration::from_millis(300));
        assert!(
            waited.try_recv().is_err(),
            "the review lock did not queue on the standing default lock"
        );
        holder.unlock().unwrap();
        waited.recv().unwrap();
        waiter.join().unwrap();
        assert!(
            !fallback.join("working-copies").exists(),
            "the review lock fell to the fallback root with the default's standing"
        );
    }
}
