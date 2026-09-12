//! The convergence run, the guarded publication, the continue and the
//! discard, the state reading, and the outcome vocabulary (SPEC u256
//! § Behaviour).
//!
//! Every entry point but `working_copy_state` holds the working copy's
//! state lock for its whole run; the `*_locked` functions below assume
//! it is held and never take it again — the lock is per open file, so a
//! second take from the same process would wait on itself.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fs::OpenOptions;
use std::io::Write;
use std::path::{Component, Path};
use std::process::{Command, Stdio};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crate::client::{PushResponse, SynsClient};
use crate::errors::CliError;
use crate::push::collector::{CollectOptions, collect_files};
use crate::push::hash::blob_sha1;
use crate::push::reconcile::{CollisionKind, holds_conflict_marker, merge_text, reconcile};
use crate::push::smart::{PushPipelineMeta, SmartPushOptions, smart_push, tree_to_sha_map};
use crate::push::working_copy::{Outbox, Resolution, Snapshot, SnapshotContent, WorkingCopy};
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

// ---- reading the head ------------------------------------------------

#[derive(Debug, Clone, Default)]
struct Head {
    commit: Option<String>,
    files: BTreeMap<String, String>,
    truncated: bool,
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
    Ok(Head {
        commit: Some(tree.commit_sha.clone()).filter(|c| !c.is_empty()),
        files: tree_to_sha_map(&tree).into_iter().collect(),
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

// ---- the folder ------------------------------------------------------

struct Folder {
    hashes: BTreeMap<String, String>,
    bytes: HashMap<String, Vec<u8>>,
}

fn collect_folder(copy: &WorkingCopy, opts: &SmartPushOptions) -> Result<Folder, CliError> {
    let collected = collect_files(
        &copy.root,
        &opts.excludes,
        CollectOptions {
            no_default_excludes: opts.no_default_excludes,
            debug: opts.debug,
            prefix: None,
        },
    )?;
    if opts.strict && !collected.skipped.is_empty() {
        return Err(CliError::PushPartial {
            skipped: collected.skipped,
            no_default_excludes: opts.no_default_excludes,
        });
    }
    // A write sibling standing at collection is a killed run's: every
    // caller holds the state lock, so no live write owns it.
    let mut files = collected.files;
    for stray in files
        .keys()
        .filter(|path| is_partial_write(path))
        .cloned()
        .collect::<Vec<_>>()
    {
        remove_folder_file(copy, &stray)?;
        files.remove(&stray);
    }
    let hashes = files
        .iter()
        .map(|(path, bytes)| (path.clone(), blob_sha1(bytes)))
        .collect();
    Ok(Folder {
        hashes,
        bytes: files,
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

fn read_disk(copy: &WorkingCopy, path: &str) -> Result<Option<Vec<u8>>, CliError> {
    match std::fs::read(copy.root.join(path)) {
        Ok(bytes) => Ok(Some(bytes)),
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

/// The file-name prefix of the sibling a folder write lands through.
const PARTIAL_PREFIX: &str = ".syns-partial-";

/// Whether a folder path names the sibling of a write a killed run left
/// part-way through: `PARTIAL_PREFIX` and the 16 lowercase hex characters
/// `write_folder_file` mints, and no other name.
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
/// its permissions carry over.
fn write_folder_file(copy: &WorkingCopy, path: &str, bytes: &[u8]) -> Result<(), CliError> {
    let target = copy.root.join(path);
    let io = |err: std::io::Error| CliError::Io {
        message: format!("could not write {path}: {err}"),
    };
    let parent = target.parent().unwrap_or(&copy.root);
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
        file.write_all(bytes)?;
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

fn remove_folder_file(copy: &WorkingCopy, path: &str) -> Result<(), CliError> {
    match std::fs::remove_file(copy.root.join(path)) {
        Ok(()) => Ok(()),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(err) => Err(CliError::Io {
            message: format!("could not remove {path}: {err}"),
        }),
    }
}

async fn read_content(
    client: &SynsClient,
    token: Option<&str>,
    copy: &WorkingCopy,
    path: &str,
    at: &str,
) -> Result<String, CliError> {
    let (file, _raw) = client
        .get_file(&repo_id(copy), token, path, Some(at))
        .await?;
    Ok(file.content)
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
    names
        .into_iter()
        .filter(|path| {
            !collected.contains_key(*path)
                && check_server_path(path).is_ok()
                && copy.root.join(path).is_file()
        })
        .cloned()
        .collect()
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
}

#[allow(clippy::too_many_arguments)]
async fn prepare_candidate(
    client: &SynsClient,
    token: Option<&str>,
    copy: &WorkingCopy,
    opts: &SmartPushOptions,
    candidate: Candidate<'_>,
    first_folder: Option<Folder>,
) -> Result<Prepared, CliError> {
    let head = candidate.head;
    let head_commit = head.commit.clone().unwrap_or_default();
    check_server_paths(head.files.keys())?;

    let mut resolution = candidate.existing.clone();
    let mut written_this_run: HashMap<String, String> = HashMap::new();
    let mut written: BTreeSet<String> = BTreeSet::new();
    let mut removed: BTreeSet<String> = BTreeSet::new();
    let mut local_paths: BTreeSet<String> = BTreeSet::new();
    let mut remote_paths: BTreeSet<String> = BTreeSet::new();
    let mut collisions: BTreeMap<String, CollisionKind> = BTreeMap::new();
    let mut resumed_combined: BTreeSet<String> = BTreeSet::new();
    let mut first_folder = first_folder;

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

    for pass in 0..MAX_PASSES {
        // `converge` 6, again on every pass after the first.
        let folder = match first_folder.take() {
            Some(folder) => folder,
            None => collect_folder(copy, opts)?,
        };
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
        local_paths.extend(rec.local_only.iter().cloned());
        remote_paths.extend(rec.remote_only.iter().cloned());
        for (path, kind) in &rec.collisions {
            collisions.entry(path.clone()).or_insert(*kind);
        }

        // What this pass writes and removes, every remote read taken
        // before any state or folder byte is written.
        let mut writes: Vec<(String, Vec<u8>)> = Vec::new();
        let mut removals: Vec<String> = Vec::new();
        let mut candidate_hashes: BTreeMap<String, Option<String>> = BTreeMap::new();
        let mut remote_snapshot: Snapshot = Snapshot::new();

        for path in &rec.remote_only {
            if head.files.contains_key(path) {
                let content = read_content(client, token, copy, path, &head_commit).await?;
                writes.push((path.clone(), content.into_bytes()));
            } else {
                removals.push(path.clone());
            }
        }
        for (path, kind) in &rec.collisions {
            let local_hash = folder.hashes.get(path);
            let already_candidate =
                local_hash.is_some() && written_this_run.get(path) == local_hash;
            match kind {
                CollisionKind::ModifyDelete => {
                    remote_snapshot.insert(path.clone(), None);
                    candidate_hashes.insert(path.clone(), local_hash.cloned());
                }
                CollisionKind::DeleteModify => {
                    let remote = read_content(client, token, copy, path, &head_commit).await?;
                    remote_snapshot
                        .insert(path.clone(), Some(SnapshotContent::Text(remote.clone())));
                    candidate_hashes.insert(path.clone(), head.files.get(path).cloned());
                    if !already_candidate {
                        writes.push((path.clone(), remote.into_bytes()));
                    }
                }
                CollisionKind::ModifyModify | CollisionKind::AddAdd => {
                    let remote = read_content(client, token, copy, path, &head_commit).await?;
                    remote_snapshot
                        .insert(path.clone(), Some(SnapshotContent::Text(remote.clone())));
                    if already_candidate {
                        candidate_hashes.insert(path.clone(), local_hash.cloned());
                        continue;
                    }
                    let base_content = match (kind, &candidate.base_commit) {
                        (CollisionKind::ModifyModify, Some(base_commit)) => {
                            read_content(client, token, copy, path, base_commit).await?
                        }
                        _ => String::new(),
                    };
                    let local = String::from_utf8_lossy(
                        folder
                            .bytes
                            .get(path)
                            .map(Vec::as_slice)
                            .unwrap_or_default(),
                    )
                    .to_string();
                    let (merged, _marked) = merge_text(&base_content, &local, &remote);
                    candidate_hashes.insert(path.clone(), Some(blob_sha1(merged.as_bytes())));
                    writes.push((path.clone(), merged.into_bytes()));
                }
            }
        }

        // `converge` 10 — the snapshots. Replaced whole where no
        // resolution stands; while one does — or once an earlier pass of
        // this run has written — a path already held keeps its first
        // content.
        let keep_first = candidate.existing.is_some() || pass > 0;
        let mut local_snapshot = if keep_first {
            copy.local_snapshot()?
        } else {
            Snapshot::new()
        };
        let mut before: HashMap<String, Option<String>> = HashMap::new();
        for path in writes.iter().map(|(p, _)| p).chain(removals.iter()) {
            let prior = match folder.bytes.get(path) {
                Some(bytes) => Some(bytes.clone()),
                None => read_disk(copy, path)?,
            };
            before.insert(path.clone(), prior.as_deref().map(blob_sha1));
            if !(keep_first && local_snapshot.contains_key(path)) {
                local_snapshot.insert(path.clone(), prior.map(SnapshotContent::from_bytes));
            }
        }
        if !writes.is_empty() || !removals.is_empty() || !keep_first {
            copy.write_local_snapshot(&local_snapshot)?;
        }
        if !remote_snapshot.is_empty() || !keep_first {
            let mut standing = if keep_first {
                copy.remote_snapshot()?
            } else {
                Snapshot::new()
            };
            standing.extend(remote_snapshot);
            copy.write_remote_snapshot(&standing)?;
        }

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
                    .map(|(path, bytes)| (path.clone(), Some(blob_sha1(bytes)))),
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

        // `converge` 11 — every write before any removal, each path
        // hashed again immediately before it is touched.
        let expected_before = |path: &str| -> Option<String> {
            match folder.hashes.get(path) {
                Some(hash) => Some(hash.clone()),
                None => before.get(path).cloned().flatten(),
            }
        };
        let mut left_untouched = false;
        for (path, bytes) in &writes {
            let now = read_disk(copy, path)?.as_deref().map(blob_sha1);
            if now != expected_before(path) {
                left_untouched = true;
                continue;
            }
            write_folder_file(copy, path, bytes)?;
            written_this_run.insert(path.clone(), blob_sha1(bytes));
            written.insert(path.clone());
        }
        for path in &removals {
            let now = read_disk(copy, path)?.as_deref().map(blob_sha1);
            if now != expected_before(path) {
                left_untouched = true;
                continue;
            }
            remove_folder_file(copy, path)?;
            removed.insert(path.clone());
        }

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
    if matches!(mode, ConvergeMode::Retrieve { .. }) && token.is_none() {
        return Ok(OutboxStep::CarryOn);
    }

    let has_base = copy.base().is_some();
    let head = read_head(client, token, copy, reading_for(mode), has_base).await?;

    let excluded = excluded_on_disk(copy, &outbox.tree, head.files.keys());
    let head_files = without(&head.files, &excluded);
    if let Some(commit) = &head.commit
        && head_files == outbox.tree
    {
        copy.record_base(commit, to_hash_map(&head.files))?;
        copy.remove_outbox()?;
        copy.remove_resolution()?;
        copy.remove_snapshots()?;
        return Ok(OutboxStep::Completed(SyncOutcome::Synced {
            written: Vec::new(),
            removed: Vec::new(),
            published: None,
        }));
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
    match settle_outbox(client, token, copy, mode).await? {
        OutboxStep::Completed(outcome) => Ok(outcome),
        OutboxStep::Resume(parent) => {
            let token = token.ok_or(CliError::AuthRequired)?;
            publish_reviewed(client, token, copy, opts, Some(parent), None).await
        }
        OutboxStep::CarryOn => converge_from_resolution(client, token, copy, mode, opts).await,
    }
}

/// `converge` 4 to 12, under a lock the caller holds.
async fn converge_from_resolution(
    client: &SynsClient,
    token: Option<&str>,
    copy: &WorkingCopy,
    mode: ConvergeMode,
    opts: SmartPushOptions,
) -> Result<SyncOutcome, CliError> {
    // 4
    let resolution = copy.resolution()?;
    if let Some(standing) = &resolution {
        if standing.pending_writes.is_some() && mode != (ConvergeMode::Retrieve { overwrite: true })
        {
            return finish_preparation(client, token, copy, &opts, mode, standing.clone()).await;
        }
        if standing.round > ROUND_BOUND {
            return Ok(SyncOutcome::AttentionRequired(resolution));
        }
        match mode {
            ConvergeMode::Publish if standing.reviewed_tree.is_some() => {
                let token = token.ok_or(CliError::AuthRequired)?;
                return publish_reviewed(client, token, copy, opts, None, None).await;
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

    // 6
    let folder = collect_folder(copy, &opts)?;
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
            &head,
            &folder,
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
                publish_reviewed(client, token, copy, opts, None, None).await
            }
        };
    }

    // 10 to 12
    let prepared = prepare_candidate(
        client,
        token,
        copy,
        &opts,
        Candidate {
            base_commit: base.commit,
            base_files: base.files,
            head: &head,
            publishing: mode == ConvergeMode::Publish,
            existing: None,
            force_resolution: false,
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
async fn finish_preparation(
    client: &SynsClient,
    token: Option<&str>,
    copy: &WorkingCopy,
    opts: &SmartPushOptions,
    mode: ConvergeMode,
    standing: Resolution,
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
        Candidate {
            base_commit: standing.base_commit.clone(),
            base_files,
            head: &head,
            publishing: mode == ConvergeMode::Publish,
            existing: Some(standing),
            force_resolution: true,
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

/// `converge` 7: make the folder hold the head, every rewritten or
/// removed path snapshotted first and uncollected files left alone.
async fn overwrite_with_head(
    client: &SynsClient,
    token: Option<&str>,
    copy: &WorkingCopy,
    head: &Head,
    folder: &Folder,
    excluded: &BTreeSet<String>,
    resolution_stands: bool,
) -> Result<SyncOutcome, CliError> {
    check_server_paths(head.files.keys())?;
    let head_commit = head.commit.clone().unwrap_or_default();

    let mut writes: Vec<(String, Vec<u8>)> = Vec::new();
    for (path, hash) in &head.files {
        if !excluded.contains(path) && folder.hashes.get(path) != Some(hash) {
            let content = read_content(client, token, copy, path, &head_commit).await?;
            writes.push((path.clone(), content.into_bytes()));
        }
    }
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
    for path in writes.iter().map(|(p, _)| p).chain(removals.iter()) {
        if resolution_stands && snapshot.contains_key(path) {
            continue;
        }
        let prior = match folder.bytes.get(path) {
            Some(bytes) => Some(bytes.clone()),
            None => read_disk(copy, path)?,
        };
        snapshot.insert(path.clone(), prior.map(SnapshotContent::from_bytes));
    }
    copy.write_local_snapshot(&snapshot)?;

    for (path, bytes) in &writes {
        write_folder_file(copy, path, bytes)?;
    }
    for path in &removals {
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

/// `publish_reviewed`, under a lock the caller holds. `resumed` is the
/// head `converge` 3 resumed at; `reviewed` is the folder
/// `continue_resolution` 3 just recorded, standing in for step 1's walk.
async fn publish_reviewed(
    client: &SynsClient,
    token: &str,
    copy: &WorkingCopy,
    opts: SmartPushOptions,
    resumed: Option<Option<String>>,
    reviewed: Option<Folder>,
) -> Result<SyncOutcome, CliError> {
    let mut reviewed = reviewed;
    for _ in 0..MAX_PASSES {
        // 1
        let folder = match reviewed.take() {
            Some(folder) => folder,
            None => collect_folder(copy, &opts)?,
        };
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

            // 2
            for (path, _kind) in &standing.collisions {
                if let Some(bytes) = folder.bytes.get(path)
                    && holds_conflict_marker(&String::from_utf8_lossy(bytes))
                {
                    return Ok(SyncOutcome::ResolutionRequired(standing.clone()));
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
        let mut push_opts = opts.clone();
        push_opts.force = false;
        push_opts.author = None;
        push_opts.prefix = None;
        push_opts.parent_sha = parent.clone();
        push_opts.reference = Some(to_hash_map(&reference));
        push_opts.expected = resolution.as_ref().map(|_| to_hash_map(&folder.hashes));

        match smart_push(client, token, &repo_id(copy), &copy.root, push_opts).await {
            Ok((response, raw, meta)) => {
                // 6
                if !response.commit_sha.is_empty() {
                    copy.record_base(&response.commit_sha, to_hash_map(&folder.hashes))?;
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
            Err(CliError::CollectedSetChanged { .. }) => {
                copy.remove_outbox()?;
                continue;
            }
            Err(CliError::Api {
                status: Some(409),
                ref error,
                ..
            }) if error == "conflict" => {
                // 7
                copy.remove_outbox()?;
                return guard_refused(client, token, copy, &opts, resolution, parent, reference)
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
        Candidate {
            base_commit: parent,
            base_files: reference,
            head: &head,
            publishing: true,
            existing,
            force_resolution: true,
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

    // 2
    match settle_outbox(client, Some(token), copy, ConvergeMode::Publish).await? {
        OutboxStep::Completed(outcome) => return Ok(outcome),
        OutboxStep::Resume(parent) => {
            return publish_reviewed(client, token, copy, opts, Some(parent), None).await;
        }
        OutboxStep::CarryOn => {}
    }
    let Some(mut resolution) = copy.resolution()? else {
        return converge_from_resolution(client, Some(token), copy, ConvergeMode::Publish, opts)
            .await;
    };
    // A half-written candidate is finished and handed back for review,
    // never recorded as the reviewed tree: publishing it would name each
    // head change still unwritten as a local revert.
    if resolution.pending_writes.is_some() {
        return converge_from_resolution(client, Some(token), copy, ConvergeMode::Publish, opts)
            .await;
    }

    // 3
    let folder = collect_folder(copy, &opts)?;
    resolution.reviewed_tree = Some(folder.hashes.clone());
    copy.write_resolution(&resolution)?;

    // 4
    publish_reviewed(client, token, copy, opts, None, Some(folder)).await
}

/// Put the folder back as it stood before the resolution rewrote it,
/// reaching no server, and leave the base as it stood.
pub fn discard_resolution(copy: &WorkingCopy) -> Result<(), CliError> {
    let _lock = copy.lock()?;
    if copy.resolution()?.is_none() {
        return Ok(());
    }
    for (path, content) in copy.local_snapshot()? {
        check_server_path(&path)?;
        match content {
            Some(content) => write_folder_file(copy, &path, &content.into_bytes())?,
            None => remove_folder_file(copy, &path)?,
        }
    }
    // A continued publication that may have landed leaves an outbox; kept,
    // the next run would resume it and publish the restored folder over the
    // head with no resolution standing. Where it did land, the next run
    // finds the head past the base and prepares a candidate instead.
    copy.remove_outbox()?;
    copy.remove_resolution()?;
    copy.remove_snapshots()
}

/// Where the working copy stands, against a head read in this call.
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
    let collected = collect_files(&copy.root, &[], CollectOptions::default())?;
    let folder: BTreeMap<String, String> = collected
        .files
        .iter()
        .filter(|(path, _)| !is_partial_write(path))
        .map(|(path, bytes)| (path.clone(), blob_sha1(bytes)))
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
}
