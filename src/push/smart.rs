use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use base64::Engine as _;

use crate::client::{
    EntryType, PushDeleteEntry, PushFileEntry, PushProvenance, PushRequest, PushResponse,
    RepoStatus, SynsClient, TreeResponse, Visibility,
};
use crate::errors::{ApiErrorContext, CliError};
use crate::push::collector::{
    CollectOptions, CollectResult, CollectedFile, HELD_BYTES_BUDGET, HeldBytes, SkipReason,
    SkippedFile, collect_files, is_text, read_collected, read_collected_into, too_large_label,
};
use crate::push::manifest::Manifest;
use crate::repo::root::path_within_prefix;
use crate::repo::syns_yaml::write_syns_yaml_where_none_stands;

/// Knobs passed into `smart_push` from the CLI layer. Wraps the
/// historical push options (force, message, author, excludes, …) plus
/// the four u213 push-feedback flags (`strict`, `allow_empty`,
/// `debug`, `no_default_excludes`). See SPEC u213 § 3.2 for the
/// per-flag semantics.
pub struct SmartPushOptions {
    pub force: bool,
    pub message: String,
    /// The `author` key every request carries; `None` sends no key, so
    /// the session's own user is the author (SPEC u256, `D-065`).
    pub author: Option<String>,
    pub parent_sha: Option<String>,
    pub excludes: Vec<String>,
    pub cache_dir: PathBuf,
    pub description: Option<String>,
    pub tags: Option<Vec<String>>,
    pub status: Option<RepoStatus>,
    pub visibility: Option<Visibility>,
    /// Mirror of CLI `--strict`. When `true` and a file was dropped for
    /// its size, `smart_push` returns `CliError::PushPartial` before any
    /// wire call (SPEC u280 `smart_push` 2).
    pub strict: bool,
    /// Mirror of CLI `--allow-empty`. When `true`, bypasses the
    /// empty-collection guard so an empty push proceeds to the wire
    /// (SPEC § 3.2, Phase 2c).
    pub allow_empty: bool,
    /// Mirror of CLI `--debug`. When `true`, the collector emits one
    /// `[debug] skip {path}: {reason} ({source})` line per excluded
    /// file to stderr (SPEC § 3.2).
    pub debug: bool,
    /// Mirror of CLI `--no-default-excludes`. When `true`, the
    /// built-in `DEFAULT_EXCLUDE_DIRS` skip list is bypassed (SPEC
    /// § 3.2). Also threaded into the `PushPipelineMeta` and into
    /// `CliError::PushPartial` so the no-default-excludes hint line
    /// is gated correctly in both the success and the error paths.
    pub no_default_excludes: bool,
    /// `ContentScope.prefix` as the pipeline receives it (SPEC u255
    /// § Contract Surface): the subtree, or the single path, a scoped
    /// publication is confined to, relative to `path`.
    ///
    /// `path` stays the repository root whatever the prefix is, so
    /// every path on the wire is repository-relative. The prefix
    /// confines two things and nothing else: what the collector walks,
    /// and which reference paths may be named as deletions.
    pub prefix: Option<String>,
    /// The reference set a convergence hands the pipeline (SPEC u256
    /// § Contract Surface). Where set it stands in for the local record
    /// AND for the `EP-tree` read, as the reference set and as the
    /// source of the parent alike: `parent_sha` alone names the parent,
    /// deletions are named against this map alone, and the record
    /// written afterwards is the collected set alone.
    pub reference: Option<HashMap<String, String>>,
    /// Where set, a collected set whose file hashes differ from this map
    /// is refused with `CliError::CollectedSetChanged` before any request
    /// leaves — what keeps a write landing after a review from being
    /// published unreviewed.
    pub expected: Option<HashMap<String, String>>,
    /// The provenance block every request of this publication carries,
    /// each chunk batch and the missing-blobs retry included.
    pub provenance: Option<PushProvenance>,
    /// The collection the one publication these options serve publishes
    /// from, walking nothing of its own (SPEC u280, `NR-01`). Moved in,
    /// never cloned: a clone of these options carries `None` here.
    pub collected: Option<CollectResult>,
    /// The run's budget of held content; `None` builds one at
    /// `HELD_BYTES_BUDGET` (SPEC u280, `D-091`).
    pub held: Option<Arc<HeldBytes>>,
    /// Whether the run writes machine-readable output, so a convergence
    /// writes no drop line of its own on the diagnostic stream.
    pub json_output: bool,
    /// Whether the command renders a landed publication's drop summary
    /// itself — a bare `syns push` does — so a convergence leaves the
    /// too-large line to that summary where the publication lands.
    pub renders_publication_summary: bool,
}

impl Clone for SmartPushOptions {
    fn clone(&self) -> Self {
        SmartPushOptions {
            force: self.force,
            message: self.message.clone(),
            author: self.author.clone(),
            parent_sha: self.parent_sha.clone(),
            excludes: self.excludes.clone(),
            cache_dir: self.cache_dir.clone(),
            description: self.description.clone(),
            tags: self.tags.clone(),
            status: self.status.clone(),
            visibility: self.visibility.clone(),
            strict: self.strict,
            allow_empty: self.allow_empty,
            debug: self.debug,
            no_default_excludes: self.no_default_excludes,
            prefix: self.prefix.clone(),
            reference: self.reference.clone(),
            expected: self.expected.clone(),
            provenance: self.provenance.clone(),
            collected: None,
            held: self.held.clone(),
            json_output: self.json_output,
            renders_publication_summary: self.renders_publication_summary,
        }
    }
}

impl SmartPushOptions {
    /// The run's budget: the one these options carry, or a fresh one at
    /// `HELD_BYTES_BUDGET`.
    pub fn held_bytes(&self) -> Arc<HeldBytes> {
        self.held
            .clone()
            .unwrap_or_else(|| HeldBytes::new(HELD_BYTES_BUDGET))
    }
}

/// Metadata threaded from `smart_push` to `format_response` so the
/// command layer can render the skip summary and distinguish a
/// first-push-noop (no prior manifest) from a subsequent-push-noop.
#[derive(Debug, Clone)]
pub struct PushPipelineMeta {
    pub skipped: Vec<SkippedFile>,
    pub manifest_existed: bool,
    pub strict: bool,
    pub no_default_excludes: bool,
    /// The parent the run's first request carried.
    pub sent_parent: Option<String>,
    /// SPEC u271: the parent the run held and the body it sent claimed
    /// none of — `--force` drops the record's own parent so the head
    /// check does not run, and `issues/118` is closed by saying so
    /// rather than by changing what the flag sends. `None` on every
    /// unforced run, and on a forced one into an identity holding no
    /// commit.
    pub unclaimed_parent: Option<String>,
    /// The file hashes the run collected.
    pub collected: HashMap<String, String>,
    /// The paths the run named as deletions.
    pub deleted: Vec<String>,
}

/// Per-batch JSON-body budget for the auto-chunker. Set to leave
/// headroom below the deployment-edge cap (Cloud Run HTTP/1.1 ~32 MiB)
/// after JSON-encoding overhead. Best-effort: a single file whose
/// materialised entry alone exceeds this budget still becomes its
/// own one-entry batch and may still 413 — in which case the chunker
/// rewrites the propagated error with the offending batch's metrics.
pub const CHUNK_BUDGET_BYTES: usize = 25 * 1024 * 1024;

/// Returns the size in bytes of the JSON-encoded wire body for a
/// `PushRequest`. Falls back to `usize::MAX` on a (impossible-in-practice)
/// serialization failure so the caller errs on the side of chunking
/// rather than silently bypassing the budget check.
fn estimate_body_bytes(request: &PushRequest) -> usize {
    let mut counted = ByteCount(0);
    serde_json::to_writer(&mut counted, request)
        .map(|()| counted.0)
        .unwrap_or(usize::MAX)
}

/// A sink counting the bytes written to it, so a serialised length is
/// taken without the serialisation standing anywhere.
struct ByteCount(usize);

impl std::io::Write for ByteCount {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0 += buf.len();
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

fn split_repo_id(repo_id: &str) -> Result<(&str, &str), CliError> {
    repo_id.split_once('/').ok_or_else(|| CliError::Config {
        message: format!("invalid repo id: {repo_id}"),
    })
}

pub(crate) fn tree_to_sha_map(tree: &TreeResponse) -> HashMap<String, String> {
    tree.entries
        .iter()
        .filter(|e| e.entry_type == EntryType::File && e.sha.is_some())
        .map(|e| (e.path.clone(), e.sha.clone().unwrap()))
        .collect()
}

/// One entry a request carries bytes for, before its bytes are read into
/// it: which field `is_text` chose, the file's length in bytes, and the
/// length the entry serialises to (SPEC u280 `PendingEntry`).
#[derive(Debug, Clone, PartialEq, Eq)]
struct PendingEntry {
    path: String,
    sha: String,
    text: bool,
    size: u64,
    encoded_len: usize,
}

/// The length `serde_json` escapes `text` to inside a string, quotes
/// excluded.
fn escaped_len(text: &[u8]) -> usize {
    text.iter()
        .map(|b| match b {
            b'"' | b'\\' | 0x08 | 0x09 | 0x0a | 0x0c | 0x0d => 2,
            0x00..=0x1f => 6,
            _ => 1,
        })
        .sum()
}

/// The length of `n` bytes as standard padded base64.
fn base64_len(n: usize) -> usize {
    n.div_ceil(3) * 4
}

/// The pending entry for one collected file, its bytes taken through
/// `read_collected` and none of them kept.
fn pend(root: &Path, path: &str, file: &CollectedFile) -> Result<PendingEntry, CliError> {
    let bytes = read_collected(root, path, file)?;
    let text = is_text(&bytes);
    let frame = serde_json::to_vec(&PushFileEntry {
        path: path.to_string(),
        sha: file.sha.clone(),
        content: text.then(String::new),
        content_base64: (!text).then(String::new),
    })
    .map(|frame| frame.len())
    .unwrap_or(usize::MAX / 2);
    let body = if text {
        escaped_len(&bytes)
    } else {
        base64_len(bytes.len())
    };
    Ok(PendingEntry {
        path: path.to_string(),
        sha: file.sha.clone(),
        text,
        size: bytes.len() as u64,
        encoded_len: frame + body,
    })
}

/// Split the reference state against the collected state into the
/// entries a request carries bytes for and the deletion entries a push
/// carries (SPEC u280 `build_push_entries`): each changed path pending in
/// ascending path order, holding none of its bytes.
///
/// `prefix` confines the DELETION side alone (SPEC u255 `smart_push`
/// 4): a scoped publication walks only its own subtree, so every
/// reference path outside that subtree is absent from `local_files`
/// and would otherwise be named as a deletion — which is exactly the
/// data loss issue 119 reports. A reference path lying outside
/// `prefix` is named in neither returned list.
fn build_push_entries(
    root: &Path,
    local_files: &HashMap<String, CollectedFile>,
    reference_shas: &HashMap<String, String>,
    force: bool,
    prefix: Option<&str>,
) -> Result<(Vec<PendingEntry>, Vec<PushDeleteEntry>), CliError> {
    let mut changed: Vec<&String> = local_files
        .iter()
        .filter(|(path, file)| force || reference_shas.get(*path) != Some(&file.sha))
        .map(|(path, _)| path)
        .collect();
    changed.sort();
    let mut entries = Vec::with_capacity(changed.len());
    for path in changed {
        entries.push(pend(root, path, &local_files[path])?);
    }

    let mut deletes = Vec::new();
    if !force {
        for path in reference_shas.keys() {
            if local_files.contains_key(path) {
                continue;
            }
            if let Some(prefix) = prefix
                && !path_within_prefix(path, prefix)
            {
                continue;
            }
            deletes.push(PushDeleteEntry { path: path.clone() });
        }
    }

    Ok((entries, deletes))
}

/// The hash-only entries for every collected path no pending entry
/// carries, in ascending path order.
fn hash_only_entries(
    local_files: &HashMap<String, CollectedFile>,
    pending: &[PendingEntry],
) -> Vec<PushFileEntry> {
    let carried: std::collections::HashSet<&str> =
        pending.iter().map(|p| p.path.as_str()).collect();
    let mut entries: Vec<PushFileEntry> = local_files
        .iter()
        .filter(|(path, _)| !carried.contains(path.as_str()))
        .map(|(path, file)| PushFileEntry {
            path: path.clone(),
            sha: file.sha.clone(),
            content: None,
            content_base64: None,
        })
        .collect();
    entries.sort_by(|a, b| a.path.cmp(&b.path));
    entries
}

/// What every serialised `PushRequest` opens on: its `files` array, the
/// first field it declares.
const FILES_OPEN: &[u8] = b"{\"files\":[";

/// Build one batch's body, only as that batch is sent (SPEC u280
/// `fill_batch`, `D-094`): allocated at the length `projected_body_bytes`
/// answers and ending at it, equal byte for byte to `serde_json`'s
/// serialisation of `frame` with `batch` filled beside the entries it
/// carries, every entry in ascending path order. Each pending entry's
/// content is escaped as `content` where it is text and encoded as
/// `contentBase64` otherwise straight into the body, from its held bytes
/// or from `scratch`, which a not-held file is read into through
/// `read_collected_into`; no copy of any content stands anywhere else.
fn fill_batch(
    root: &Path,
    local_files: &HashMap<String, CollectedFile>,
    frame: &PushRequest,
    batch: &[PendingEntry],
    scratch: &mut Vec<u8>,
) -> Result<Vec<u8>, CliError> {
    let unserialisable = |e: serde_json::Error| CliError::Io {
        message: format!("could not serialise a request body: {e}"),
    };
    // The frame's other fields as `serde_json` writes them: everything
    // its serialisation carries after the `files` array opens.
    let tail = serde_json::to_vec(&request_frame_of(frame)).map_err(unserialisable)?;
    debug_assert!(tail.starts_with(FILES_OPEN));
    let tail = &tail[FILES_OPEN.len()..];

    let projected = projected_body_bytes(frame, batch);
    let mut body = Vec::with_capacity(projected);
    body.extend_from_slice(FILES_OPEN);

    let mut carried: Vec<&PushFileEntry> = frame.files.iter().collect();
    carried.sort_by(|a, b| a.path.cmp(&b.path));
    let mut pending: Vec<&PendingEntry> = batch.iter().collect();
    pending.sort_by(|a, b| a.path.cmp(&b.path));
    let (mut carried, mut pending) = (
        carried.into_iter().peekable(),
        pending.into_iter().peekable(),
    );
    let mut first = true;
    loop {
        let next_is_pending = match (carried.peek(), pending.peek()) {
            (None, None) => break,
            (Some(_), None) => false,
            (None, Some(_)) => true,
            (Some(c), Some(p)) => p.path < c.path,
        };
        if !first {
            body.push(b',');
        }
        first = false;
        if next_is_pending {
            let entry = pending.next().expect("peeked");
            write_pending(&mut body, root, local_files, entry, scratch)?;
        } else {
            let entry = carried.next().expect("peeked");
            serde_json::to_writer(&mut body, entry).map_err(unserialisable)?;
        }
    }
    body.extend_from_slice(tail);
    debug_assert_eq!(body.len(), projected, "the body is its projected length");
    Ok(body)
}

/// One pending entry written into `body` as `serde_json` writes a
/// `PushFileEntry`: its path, its hash, and its content in the one field
/// `is_text` chose.
fn write_pending(
    body: &mut Vec<u8>,
    root: &Path,
    local_files: &HashMap<String, CollectedFile>,
    entry: &PendingEntry,
    scratch: &mut Vec<u8>,
) -> Result<(), CliError> {
    let changed = || CliError::CollectedSetChanged {
        paths: vec![entry.path.clone()],
    };
    let unserialisable = |e: serde_json::Error| CliError::Io {
        message: format!("could not serialise a request body: {e}"),
    };
    let file = local_files.get(&entry.path).ok_or_else(changed)?;
    let bytes: &[u8] = match &file.bytes {
        Some((bytes, _)) => bytes,
        None => {
            read_collected_into(root, &entry.path, file, scratch)?;
            scratch
        }
    };
    body.extend_from_slice(b"{\"path\":");
    serde_json::to_writer(&mut *body, &entry.path).map_err(unserialisable)?;
    body.extend_from_slice(b",\"sha\":");
    serde_json::to_writer(&mut *body, &entry.sha).map_err(unserialisable)?;
    if entry.text {
        let text = std::str::from_utf8(bytes).map_err(|_| changed())?;
        body.extend_from_slice(b",\"content\":");
        serde_json::to_writer(&mut *body, text).map_err(unserialisable)?;
    } else {
        body.extend_from_slice(b",\"contentBase64\":\"");
        let start = body.len();
        body.resize(start + base64_len(bytes.len()), 0);
        base64::engine::general_purpose::STANDARD
            .encode_slice(bytes, &mut body[start..])
            .map_err(|e| CliError::Io {
                message: format!("could not encode {}: {e}", entry.path),
            })?;
        body.push(b'"');
    }
    body.push(b'}');
    Ok(())
}

/// The one buffer a publication pass reads a not-held file into: sized
/// at the largest pending file whose bytes the collection does not hold,
/// and empty where it holds every one (SPEC u280 `smart_push` 4).
fn scratch_for(local_files: &HashMap<String, CollectedFile>, pending: &[PendingEntry]) -> Vec<u8> {
    let largest = pending
        .iter()
        .filter(|entry| {
            local_files
                .get(&entry.path)
                .is_some_and(|file| file.bytes.is_none())
        })
        .map(|entry| entry.size)
        .max();
    match largest {
        Some(size) => Vec::with_capacity(usize::try_from(size).unwrap_or(0)),
        None => Vec::new(),
    }
}

/// The local record a run writes (SPEC u255 `smart_push` 7): the set
/// this run has published laid over the record as it stood when the
/// run began, with the paths the run named as deletions taken out.
///
/// Before u255 every record write was built from the published set
/// ALONE, so a scoped publication rewrote the record to name its own
/// subtree and nothing else — and the next bare publication then read
/// that record as the whole repository's state and named every root
/// path as a deletion (issue 119).
fn merged_record(
    record_base: &HashMap<String, String>,
    deleted_paths: &[String],
    published: &HashMap<String, String>,
) -> HashMap<String, String> {
    let mut merged = record_base.clone();
    for path in deleted_paths {
        merged.remove(path);
    }
    for (path, sha) in published {
        merged.insert(path.clone(), sha.clone());
    }
    merged
}

/// The request every batch of one publication shares, its files and
/// deletions left for the batch to fill.
fn request_frame(
    base: &PushRequest,
    message: Option<String>,
    parent: Option<String>,
) -> PushRequest {
    PushRequest {
        files: Vec::new(),
        deletions: None,
        message,
        author: base.author.clone(),
        parent_sha: parent,
        description: base.description.clone(),
        tags: base.tags.clone(),
        status: base.status.clone(),
        visibility: base.visibility.clone(),
        provenance: base.provenance.clone(),
    }
}

/// Every field of `base` but its files, which the body a batch fills
/// writes of its own.
fn request_frame_of(base: &PushRequest) -> PushRequest {
    let mut frame = request_frame(base, base.message.clone(), base.parent_sha.clone());
    frame.deletions = base.deletions.clone();
    frame
}

/// The serialised length of `base` with `pending` filled in beside the
/// entries it already carries.
fn projected_body_bytes(base: &PushRequest, pending: &[PendingEntry]) -> usize {
    let separators = if base.files.is_empty() {
        pending.len().saturating_sub(1)
    } else {
        pending.len()
    };
    pending
        .iter()
        .fold(estimate_body_bytes(base), |sum, entry| {
            sum.saturating_add(entry.encoded_len)
        })
        .saturating_add(separators)
}

/// The overhead each batch's frame adds to the entries packed into it:
/// the first batch's, which carries every hash-only entry, and every
/// later one's, which carries none.
#[derive(Debug, Clone, Copy)]
struct Overheads {
    first: usize,
    /// Whether the first batch's frame carries entries of its own, so
    /// every pending entry packed beside them adds a separator.
    first_carries: bool,
    later: usize,
}

/// The overheads a chunked publication of `base_request` packs against:
/// the frame each batch shares, its message the longest a batch carries
/// and its deletions the whole list, so no batch packs past the budget on
/// account of either, the first batch's also carrying every hash-only
/// entry `base_request` names (CR3-2).
fn batch_overheads(base_request: &PushRequest) -> Overheads {
    let mut probe = request_frame(
        base_request,
        Some(format!(
            "{} (part {}/{})",
            base_request.message.as_deref().unwrap_or(""),
            usize::MAX,
            usize::MAX
        )),
        base_request.parent_sha.clone(),
    );
    probe.deletions = base_request.deletions.clone();
    let later = estimate_body_bytes(&probe);
    probe.files = base_request.files.clone();
    let first = estimate_body_bytes(&probe);
    Overheads {
        first,
        first_carries: !base_request.files.is_empty(),
        later,
    }
}

/// Pack pending entries into batches by their serialised lengths: in
/// descending `encoded_len`, ascending path among equal lengths, a batch
/// closed before an entry that would carry it past `CHUNK_BUDGET_BYTES`
/// and a lone entry past it standing alone, the first batch's budget
/// counting the hash-only entries its frame carries (SPEC u280
/// `smart_push` 4).
fn pack_batches(mut pending: Vec<PendingEntry>, overheads: Overheads) -> Vec<Vec<PendingEntry>> {
    pending.sort_by(|a, b| {
        b.encoded_len
            .cmp(&a.encoded_len)
            .then_with(|| a.path.cmp(&b.path))
    });
    let mut batches: Vec<Vec<PendingEntry>> = Vec::new();
    let mut current: Vec<PendingEntry> = Vec::new();
    let mut current_len = overheads.first;
    let mut carries = overheads.first_carries;
    for entry in pending {
        let added = entry.encoded_len + usize::from(carries || !current.is_empty());
        if !current.is_empty() && current_len.saturating_add(added) > CHUNK_BUDGET_BYTES {
            batches.push(std::mem::take(&mut current));
            current_len = overheads.later;
            carries = false;
        }
        current_len = current_len
            .saturating_add(entry.encoded_len + usize::from(carries || !current.is_empty()));
        current.push(entry);
    }
    if !current.is_empty() || batches.is_empty() {
        batches.push(current);
    }
    batches
}

/// A publication pass's refusal, and whether it is the `MISSING_BLOBS`
/// a resend answers: one refusing the one request, or a chunked
/// publication's first batch, before any batch of the pass committed
/// (SPEC u280 `smart_push` 5).
struct PassRefusal {
    error: CliError,
    resendable: bool,
}

impl PassRefusal {
    /// A refusal no resend answers.
    fn standing(error: CliError) -> PassRefusal {
        PassRefusal {
            error,
            resendable: false,
        }
    }

    /// A refusal of a request no earlier request of the pass committed
    /// before: a resend answers it where it is `MISSING_BLOBS`.
    fn before_any_commit(error: CliError) -> PassRefusal {
        let resendable = matches!(
            &error,
            CliError::Api { status: Some(409), error, .. } if error == "missing_blobs"
        );
        PassRefusal { error, resendable }
    }
}

/// Where every body of one publication pass is sent from, and the record
/// its intermediate batches save.
struct Target<'a> {
    client: &'a SynsClient,
    token: &'a str,
    repo_id: &'a str,
    root: &'a Path,
    local_files: &'a HashMap<String, CollectedFile>,
    cache_dir: &'a Path,
    owner: &'a str,
    name: &'a str,
    record_base: &'a HashMap<String, String>,
}

/// One publication pass (SPEC u280 `smart_push` 4): `pending` sent beside
/// every hash-only entry and deletion `request` carries as one body, or
/// chunked where that body's projection passes `CHUNK_BUDGET_BYTES` or
/// the edge refused it as `PAYLOAD_TOO_LARGE`. The one buffer a not-held
/// file is read into is allocated once for the pass.
async fn publish_pass(
    target: &Target<'_>,
    request: &PushRequest,
    pending: &[PendingEntry],
) -> Result<(PushResponse, serde_json::Value), PassRefusal> {
    let mut scratch = scratch_for(target.local_files, pending);
    let oversize =
        !pending.is_empty() && projected_body_bytes(request, pending) > CHUNK_BUDGET_BYTES;
    if !oversize {
        let body = fill_batch(
            target.root,
            target.local_files,
            request,
            pending,
            &mut scratch,
        )
        .map_err(PassRefusal::standing)?;
        match target
            .client
            .push_body(target.repo_id, target.token, body)
            .await
        {
            Ok(answered) => return Ok(answered),
            // The 413 fallback: the chunker over the same pending
            // entries, which rewrites the metrics of any batch the edge
            // refuses again.
            Err(CliError::PayloadTooLarge { .. }) => {}
            Err(other) => return Err(PassRefusal::before_any_commit(other)),
        }
    }
    chunked_push(target, request, pending.to_vec(), &mut scratch).await
}

/// Auto-chunk a publication into sequential `push_body` calls, each
/// batch's serialised body under `CHUNK_BUDGET_BYTES` where its entries
/// allow. Used by `publish_pass` both pre-flight (when the projected body
/// exceeds the budget) and as a 413 fallback (when the wire returns
/// `PayloadTooLarge`). Only the pending entries are packed: every
/// hash-only entry rides the first batch's frame and is counted against
/// its budget, and the deletions ride the last (SPEC u280 `smart_push`
/// 4, issue 186). Each batch's body is built only as that batch is sent,
/// so no batch's bytes are held before or after it.
///
/// Best-effort: a single entry whose materialised content alone
/// exceeds the budget still becomes its own one-entry batch and may
/// still 413 — the chunker then rewrites the propagated
/// `PayloadTooLarge` with the offending body's length and file count
/// (both in the loop branch and in the n==1 short-circuit).
///
/// Per-batch manifest save (loop branch only): after each successful
/// batch `k` where `1 ≤ k < n`, the chunker writes a manifest snapshot
/// anchored to batch k's `commit_sha` and the cumulative `path → sha`
/// map of every batch uploaded so far, laid over `record_base` — the
/// record as it stood when the run began. This honours SPEC u225 § 2's
/// "manifest tracks the SHA of the LAST SUCCESSFUL batch" promise: if
/// batches `1..k-1` succeed and batch `k` fails, the local manifest
/// matches server HEAD = batch `k-1`'s commit, and the next push
/// computes diffs from there.
///
/// SPEC u255 `smart_push` 7 binds every record write a run makes,
/// these intermediate snapshots included: seeding them from the
/// published set alone dropped every out-of-scope path from a scoped
/// publication large enough to chunk. What an intermediate snapshot
/// does NOT do is take the run's deletions out — those ride on the
/// final batch alone, so a run failing before it has deleted nothing.
/// The final batch's save is left to `smart_push` Phase 6, which lays
/// the full `local_shas` map over the same base and takes the
/// deletion list out there.
async fn chunked_push(
    target: &Target<'_>,
    base_request: &PushRequest,
    pending: Vec<PendingEntry>,
    scratch: &mut Vec<u8>,
) -> Result<(PushResponse, serde_json::Value), PassRefusal> {
    let batches = pack_batches(pending, batch_overheads(base_request));

    let n = batches.len();

    // Short-circuit if no chunking actually needed: the one body is the
    // request as it stands with the batch filled beside it. The push is
    // wrapped in the same match as the loop body so that a 413 on the
    // only batch still rewrites bytes_sent/file_count with the batch's
    // metrics (HIGH-1).
    if n == 1 {
        let only_batch = batches.into_iter().next().unwrap_or_default();
        let body = fill_batch(
            target.root,
            target.local_files,
            base_request,
            &only_batch,
            scratch,
        )
        .map_err(PassRefusal::standing)?;
        let bytes_sent = body.len() as u64;
        return match target
            .client
            .push_body(target.repo_id, target.token, body)
            .await
        {
            Ok(ok) => Ok(ok),
            Err(CliError::PayloadTooLarge { rejecter, .. }) => {
                Err(PassRefusal::standing(CliError::PayloadTooLarge {
                    bytes_sent,
                    file_count: base_request.files.len() + only_batch.len(),
                    rejecter,
                }))
            }
            Err(other) => Err(PassRefusal::before_any_commit(other)),
        };
    }

    // Sequential batch submission with chained parent_sha.
    // PushResponse does not derive Clone, so we keep commit_sha
    // (which is String: Clone) separately and store the most recent
    // (response, raw) pair in last_completed for the final return.
    let mut previous_commit_sha: Option<String> = base_request.parent_sha.clone();
    let mut last_completed: Option<(PushResponse, serde_json::Value)> = None;
    // Cumulative `path → sha` map of every entry uploaded so far. After
    // each successful batch, the per-batch manifest snapshot is
    // `record_base ∪ uploaded` — i.e., the record the run started
    // from, carrying everything pushed in batches 1..k.
    let mut uploaded: HashMap<String, String> = HashMap::new();

    let batch_failed = |k1: usize, previous: &Option<String>| {
        if k1 > 1 {
            eprintln!(
                "warning: chunked push failed at batch {}/{}; local manifest updated to last successful batch ({}); next push will commit only the remaining files.",
                k1,
                n,
                previous.as_deref().unwrap_or(""),
            );
        }
    };

    for (k, batch) in batches.iter().enumerate() {
        let k1 = k + 1; // 1-indexed for human-facing progress and `(part k/n)`.

        let batch_bytes: usize = batch.iter().map(|e| e.encoded_len).sum();
        let batch_files = batch.len();

        let synthesised_message = format!(
            "{} (part {}/{})",
            base_request.message.as_deref().unwrap_or(""),
            k1,
            n,
        );
        let mut frame = request_frame(
            base_request,
            Some(synthesised_message),
            previous_commit_sha.clone(),
        );
        if k1 == 1 {
            frame.files = base_request.files.clone();
        }
        if k1 == n {
            frame.deletions = base_request.deletions.clone();
        }
        // The batch's body is built only now, as it is sent.
        let body = match fill_batch(target.root, target.local_files, &frame, batch, scratch) {
            Ok(body) => body,
            Err(err) => {
                batch_failed(k1, &previous_commit_sha);
                return Err(PassRefusal::standing(err));
            }
        };
        drop(frame);
        let bytes_sent = body.len() as u64;

        // Progress line — matches existing smart.rs stderr-progress convention.
        eprintln!(
            "chunk {}/{}: {:.1} MiB uploaded across {} files",
            k1,
            n,
            batch_bytes as f64 / (1024.0 * 1024.0),
            batch_files,
        );

        // Submit. On 413, rewrite the variant with this batch's metrics.
        // On any error after at least one batch has committed, emit a
        // stderr warning so the user understands that the local manifest
        // now reflects server HEAD at batch k-1 (HIGH-2 fix).
        match target
            .client
            .push_body(target.repo_id, target.token, body)
            .await
        {
            Ok((response, raw)) => {
                for entry in batch {
                    uploaded.insert(entry.path.clone(), entry.sha.clone());
                }
                previous_commit_sha = Some(response.commit_sha.clone());
                // Per-batch manifest save (HIGH-2): only for intermediate
                // batches, and taking NO deletion out. The deletions ride
                // on the final batch alone, so a run that fails before it
                // has not deleted anything yet; a snapshot that dropped
                // them would leave the next run finding those paths in
                // neither the record nor its walk, naming no deletion,
                // and the server keeping them for good. The Phase 6 write
                // in `smart_push` takes them out, its batch being the one
                // that carried them.
                if k1 < n {
                    let cumulative = merged_record(target.record_base, &[], &uploaded);
                    let mut manifest = Manifest::default();
                    manifest.update(response.commit_sha.clone(), cumulative);
                    if let Err(e) = manifest.save(target.cache_dir, target.owner, target.name) {
                        eprintln!(
                            "warning: could not save per-batch manifest after chunk {}/{}: {}",
                            k1, n, e,
                        );
                    }
                }
                last_completed = Some((response, raw));
            }
            Err(CliError::PayloadTooLarge { rejecter, .. }) => {
                batch_failed(k1, &previous_commit_sha);
                // Every entry the refused body carried, the hash-only
                // ones the first batch's frame names included (CR3-3).
                let carried = if k1 == 1 { base_request.files.len() } else { 0 };
                return Err(PassRefusal::standing(CliError::PayloadTooLarge {
                    bytes_sent,
                    file_count: batch_files + carried,
                    rejecter,
                }));
            }
            // The first batch's refusal comes before any commit of the
            // pass; a later one's after its batches committed.
            Err(other) if k1 == 1 => return Err(PassRefusal::before_any_commit(other)),
            Err(other) => {
                batch_failed(k1, &previous_commit_sha);
                return Err(PassRefusal::standing(other));
            }
        }
    }

    // Return the FINAL batch's response.
    Ok(last_completed.expect("at least one batch ran when n > 1"))
}

/// The hash-only paths a `MISSING_BLOBS` refusal's `missing` map names —
/// the ones a resend pends for their content (SPEC u280 `smart_push` 5).
fn named_hash_only(error: &CliError, hash_only: &[PushFileEntry]) -> Vec<String> {
    let CliError::Api {
        context: Some(ApiErrorContext::MissingBlobs { missing }),
        ..
    } = error
    else {
        return Vec::new();
    };
    hash_only
        .iter()
        .filter(|entry| missing.contains_key(&entry.path))
        .map(|entry| entry.path.clone())
        .collect()
}

/// Pick one human-readable most-likely cause for `PushEmpty`'s
/// diagnostic (SPEC u213 § 5 D4). Single-line output; no path list.
fn skip_summary_cause(skipped: &[SkippedFile], source: &Path) -> String {
    use SkipReason::*;

    if skipped.is_empty() {
        return "the source directory contains no files".to_string();
    }

    let total = skipped.len();
    let mut counts: [usize; 5] = [0; 5];
    for sf in skipped {
        counts[sf.reason as usize] += 1;
    }
    let majority = |idx: usize| counts[idx] * 2 > total; // strict majority

    let gitignore_present = source.join(".gitignore").is_file();
    let synsignore_present = source.join(".synsignore").is_file();

    if majority(TooLarge as usize) {
        format!("every file is {}", too_large_label())
    } else if majority(DefaultExcludeDir as usize) {
        "every file is inside a default-excluded directory (node_modules, dist, build, target, .venv, …); pass --no-default-excludes to override".to_string()
    } else if gitignore_present && majority(Gitignore as usize) {
        format!(
            "a .gitignore file in {} excludes every file",
            source.display()
        )
    } else if synsignore_present && majority(Synsignore as usize) {
        format!(
            "a .synsignore file in {} excludes every file",
            source.display()
        )
    } else if majority(UserExclude as usize) {
        "every file matches a --exclude pattern".to_string()
    } else {
        format!(
            "{} file(s) excluded across multiple reasons; pass --debug for per-file detail",
            total
        )
    }
}

/// Whether a collection's drops refuse a strict publication: only a file
/// the walk would have published and dropped for its size counts (SPEC
/// u280 `smart_push` 2).
pub(crate) fn strict_refuses(skipped: &[SkippedFile]) -> bool {
    skipped.iter().any(|sf| sf.reason == SkipReason::TooLarge)
}

pub async fn smart_push(
    client: &SynsClient,
    token: &str,
    repo_id: &str,
    path: &Path,
    opts: SmartPushOptions,
) -> Result<(PushResponse, serde_json::Value, PushPipelineMeta), CliError> {
    let mut opts = opts;
    // Phase 1 — Setup (preserved from u21).
    let (owner, name) = split_repo_id(repo_id)?;

    // The identity marker is written only where NO `.syns.yaml` at or
    // above `path` already names this repository, AND no `.syns.yaml`
    // stands in `path` itself. The walk-up test is what stops a
    // publication run from `repo/sub/` writing `repo/sub/.syns.yaml`
    // and entrenching the subtree as a repository of its own — issue
    // 119's most damaging symptom. The exact-directory test beside it
    // is what stops `syns push --name bob/other`, run where a marker
    // names `alice/proj`, from overwriting that marker: the walk-up
    // test answers `None` on the pair mismatch and would otherwise
    // let the write through, re-identifying the whole tree.
    //
    // A convergence's publication (`reference` set) writes no marker: the
    // folder it sends is the one its reviewer recorded, and a marker
    // written here would land after that record and be refused by the
    // `expected` guard below — the command handlers write it before the
    // convergence collects instead (SPEC u256).
    if opts.reference.is_none() {
        write_syns_yaml_where_none_stands(path, owner, name)?;
    }

    // Phase 2a — Collect (SPEC u280 `smart_push` 1): the collection the
    // options carry, or one walk handed no record.
    let CollectResult {
        files: mut local_files,
        skipped,
        total_walked,
    } = match opts.collected.take() {
        Some(collected) => collected,
        None => collect_files(
            path,
            &opts.excludes,
            CollectOptions {
                no_default_excludes: opts.no_default_excludes,
                debug: opts.debug,
                prefix: opts.prefix.clone(),
            },
            None,
            &opts.held_bytes(),
        )?,
    };
    // A folder write sibling a killed convergence left is no repository
    // file, whichever publication collects it (SPEC u256).
    local_files.retain(|path, _| !crate::push::converge::is_partial_write(path));

    // Phase 2b — Strict guard (supersedes empty per SPEC D10), counting
    // the size drops alone (SPEC u280 `smart_push` 2).
    if opts.strict && strict_refuses(&skipped) {
        return Err(CliError::PushPartial {
            skipped,
            no_default_excludes: opts.no_default_excludes,
        });
    }

    // Phase 2c — the empty guard used to stand here. It now stands
    // below Phase 4, because a scoped publication carrying only
    // deletions carries no file and must still reach the server
    // (SPEC u255 `smart_push` 5).

    // Phase 3a — the collected hashes.
    let local_shas: HashMap<String, String> = local_files
        .iter()
        .map(|(p, file)| (p.clone(), file.sha.clone()))
        .collect();

    // Phase 3a' — the expected-set guard (SPEC u256
    // `SmartPushOptions.expected`). A convergence records the folder it
    // reviewed; a write landing since is refused here, before any
    // request, rather than published unreviewed.
    if let Some(expected) = &opts.expected {
        let mut differing: Vec<String> = local_shas
            .iter()
            .filter(|(path, sha)| expected.get(*path) != Some(*sha))
            .map(|(path, _)| path.clone())
            .chain(
                expected
                    .keys()
                    .filter(|path| !local_shas.contains_key(*path))
                    .cloned(),
            )
            .collect();
        if !differing.is_empty() {
            differing.sort();
            return Err(CliError::CollectedSetChanged { paths: differing });
        }
    }

    // Phase 3b / 3c — the record as it stood when the run began (SPEC
    // u255 `smart_push` 7), and the parent it names. A convergence's
    // `reference` stands in for both reads (SPEC u256), so neither the
    // local record nor `EP-tree` is consulted and only `parent_sha`
    // names a parent.
    let (manifest_existed, record_base, remote_parent_sha) = match &opts.reference {
        Some(reference) => (!reference.is_empty(), reference.clone(), None),
        None => {
            // Phase 3b — Load manifest unconditionally (so
            // `manifest_existed` is set even when --force bypasses the
            // reference state from it).
            let loaded_manifest = Manifest::load(&opts.cache_dir, owner, name);
            let manifest_existed = loaded_manifest.is_some();
            let record_from_manifest: Option<(HashMap<String, String>, Option<String>)> =
                loaded_manifest.map(|manifest| {
                    let shas: HashMap<String, String> = manifest
                        .file_paths()
                        .filter_map(|p| {
                            manifest
                                .file_sha(p)
                                .map(|sha| (p.to_string(), sha.to_string()))
                        })
                        .collect();
                    let parent = manifest.commit_sha().map(String::from);
                    (shas, parent)
                });

            // Phase 3c — Phase 6 lays the collected set over THIS map,
            // which is what keeps a scoped publication's record naming
            // the whole tree rather than just its scope.
            //
            // Where no local record loads — a checkout that has never
            // written one, or `--force` having discarded it — the remote
            // path set through `EP-tree` is the base instead. The extra
            // request on that path is deliberate: a `--force` scoped
            // publication with no record on disk is the one corner that
            // would otherwise rewrite the record from the scope alone
            // and hand the NEXT bare publication a deletion for every
            // out-of-scope path.
            let (record_base, remote_parent_sha) = match &record_from_manifest {
                Some((shas, parent)) => (shas.clone(), parent.clone()),
                None => match client.pull(repo_id, Some(token)).await {
                    Ok(tree) => {
                        let parent = Some(tree.commit_sha.clone());
                        (tree_to_sha_map(&tree), parent)
                    }
                    Err(CliError::Api {
                        status: Some(404), ..
                    }) => (HashMap::new(), None),
                    Err(e) => return Err(e),
                },
            };
            (manifest_existed, record_base, remote_parent_sha)
        }
    };

    // Phase 3d — Build reference state: the diff base the wire payload
    // is computed against. `--force` empties it so every collected
    // file rides with content and nothing is named as a deletion.
    let (reference_shas, base_parent_sha, unclaimed_parent) = if opts.force {
        // SPEC u271: the parent `--force` drops rides out on the meta so
        // the command layer can name it — the flag keeps all three of
        // its shipped effects and the run says so (`D-007`).
        (HashMap::new(), None, remote_parent_sha)
    } else {
        (record_base.clone(), remote_parent_sha, None)
    };

    let parent_sha = if opts.parent_sha.is_some() {
        opts.parent_sha.clone()
    } else {
        base_parent_sha
    };

    // Phase 4 — the payload (SPEC u280 `smart_push` 3): each changed
    // path pending for its bytes, every other path named by its hash,
    // prefix-gated on the deletion side per SPEC u255 `smart_push` 4.
    let (pending, deletes) = build_push_entries(
        path,
        &local_files,
        &reference_shas,
        opts.force,
        opts.prefix.as_deref(),
    )?;
    let hash_only = hash_only_entries(&local_files, &pending);

    // Phase 4b — Empty guard. A publication is refused where it
    // carries neither a file nor a deletion, and ALSO where its walk
    // collected nothing and no path argument scoped it.
    //
    // The second clause is not redundant. An unscoped walk that
    // collected nothing names every path the reference set holds as a
    // deletion, so the first clause alone lets a misconfigured root
    // ignore file — or `--exclude '*'` — publish a body that empties
    // the repository on the server. Only a SCOPED publication may
    // carry deletions and no file: that is the delete-only run whose
    // subtree was removed from disk, and its blast radius is the
    // subtree the caller named.
    let carries_nothing = pending.is_empty() && hash_only.is_empty() && deletes.is_empty();
    let unscoped_walk_found_nothing = local_files.is_empty() && opts.prefix.is_none();
    if (carries_nothing || unscoped_walk_found_nothing) && !opts.allow_empty {
        // CR1-5: name the directory the run addressed, not the content
        // root it resolved — a run scoped to one subtree that reports
        // the whole repository as empty sends its reader looking in
        // the wrong place.
        let addressed = match opts.prefix.as_deref() {
            Some(prefix) => path.join(prefix),
            None => path.to_path_buf(),
        };
        let cause = skip_summary_cause(&skipped, path);
        return Err(CliError::PushEmpty {
            path: addressed.display().to_string(),
            total_walked,
            cause,
        });
    }

    // The paths this run takes out of the local record (SPEC u255
    // `smart_push` 7). Captured before `deletes` is moved onto the
    // request.
    let deleted_paths: Vec<String> = deletes.iter().map(|d| d.path.clone()).collect();

    let deletions = if deletes.is_empty() {
        None
    } else {
        Some(deletes)
    };

    let mut request = PushRequest {
        files: hash_only,
        deletions,
        message: Some(opts.message.clone()),
        author: opts.author.clone(),
        parent_sha,
        description: opts.description.clone(),
        tags: opts.tags.clone(),
        status: opts.status.clone(),
        visibility: opts.visibility.clone(),
        provenance: opts.provenance.clone(),
    };
    let sent_parent = request.parent_sha.clone();

    // Phase 5 — Submit (SPEC u280 `smart_push` 4–5): the pending entries
    // beside every hash-only entry and deletion as one body, chunked
    // where that body passes the budget or the edge refuses it, and a
    // `MISSING_BLOBS` on the one request or on a chunked publication's
    // first batch answered by one more pass carrying content for the
    // hash-only paths its `missing` map names.
    let target = Target {
        client,
        token,
        repo_id,
        root: path,
        local_files: &local_files,
        cache_dir: &opts.cache_dir,
        owner,
        name,
        record_base: &record_base,
    };
    let mut pending = pending;
    let mut resent = false;
    let (response, raw) = loop {
        let refusal = match publish_pass(&target, &request, &pending).await {
            Ok(answered) => break answered,
            Err(refusal) => refusal,
        };
        if resent || !refusal.resendable {
            return Err(refusal.error);
        }
        let named = named_hash_only(&refusal.error, &request.files);
        if named.is_empty() {
            return Err(refusal.error);
        }
        resent = true;
        for named_path in &named {
            let file =
                local_files
                    .get(named_path)
                    .ok_or_else(|| CliError::CollectedSetChanged {
                        paths: vec![named_path.clone()],
                    })?;
            pending.push(pend(path, named_path, file)?);
        }
        pending.sort_by(|a, b| a.path.cmp(&b.path));
        request.files = hash_only_entries(&local_files, &pending);
    };

    // Phase 6 — Manifest save (guarded per SPEC u213 § 4 Phase 6).
    if response.commit_sha.is_empty() {
        eprintln!(
            "warning: server response had empty commit_sha; not updating local manifest \
             (this typically indicates that no files were uploaded — see `syns push --debug`)"
        );
    } else {
        let record = if opts.reference.is_some() {
            local_shas.clone()
        } else {
            merged_record(&record_base, &deleted_paths, &local_shas)
        };
        let mut manifest = Manifest::default();
        manifest.update(response.commit_sha.clone(), record);
        if let Err(e) = manifest.save(&opts.cache_dir, owner, name) {
            eprintln!("warning: could not save manifest (next push will re-upload all files): {e}");
        }
    }

    // Phase 7 — Return with meta.
    Ok((
        response,
        raw,
        PushPipelineMeta {
            skipped,
            manifest_existed,
            strict: opts.strict,
            no_default_excludes: opts.no_default_excludes,
            sent_parent,
            unclaimed_parent,
            collected: local_shas,
            deleted: deleted_paths,
        },
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::errors::EdgeRejecter;
    use crate::push::hash::blob_sha1;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    #[tokio::test]
    async fn first_push_sends_all_files_with_content() {
        let mock_server = MockServer::start().await;

        Mock::given(method("GET"))
            .and(path("/api/v1/repos/alice/new-repo/tree"))
            .respond_with(
                ResponseTemplate::new(404).set_body_json(serde_json::json!({"error": "not_found"})),
            )
            .mount(&mock_server)
            .await;

        Mock::given(method("PUT"))
            .and(path("/api/v1/repos/alice/new-repo/push"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "commitSha": "abc123",
                "version": 1,
                "filesChanged": 2,
                "created": true
            })))
            .mount(&mock_server)
            .await;

        let temp_dir = tempfile::tempdir().unwrap();
        std::fs::write(temp_dir.path().join("main.txt"), "hello").unwrap();
        std::fs::create_dir_all(temp_dir.path().join("sub")).unwrap();
        std::fs::write(temp_dir.path().join("sub/other.txt"), "world").unwrap();

        let cache_dir = tempfile::tempdir().unwrap();
        let client = SynsClient::new(&mock_server.uri()).unwrap();

        let result = smart_push(
            &client,
            "test-token",
            "alice/new-repo",
            temp_dir.path(),
            SmartPushOptions {
                force: false,
                message: "init".into(),
                author: Some("alice".into()),
                parent_sha: None,
                excludes: vec![],
                cache_dir: cache_dir.path().to_path_buf(),
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
            },
        )
        .await;

        assert!(result.is_ok());
        let (response, _raw, _meta) = result.unwrap();
        assert_eq!(response.commit_sha, "abc123");

        // .syns.yaml was auto-created
        let syns_yaml = std::fs::read_to_string(temp_dir.path().join(".syns.yaml")).unwrap();
        assert_eq!(syns_yaml, "owner: alice\nname: new-repo\n");

        // Manifest was saved
        assert!(
            cache_dir
                .path()
                .join("alice")
                .join("new-repo.json")
                .exists()
        );

        // Verify request body
        let requests = mock_server.received_requests().await.unwrap();
        let put_request = requests
            .iter()
            .find(|r| r.method == reqwest::Method::PUT)
            .unwrap();
        let body: serde_json::Value = serde_json::from_slice(&put_request.body).unwrap();

        let files = body["files"].as_array().unwrap();
        let has_main = files
            .iter()
            .any(|f| f["path"] == "main.txt" && f["content"].is_string());
        let has_other = files
            .iter()
            .any(|f| f["path"] == "sub/other.txt" && f["content"].is_string());
        assert!(has_main);
        assert!(has_other);
        assert!(body["parentSha"].is_null());
        assert!(body.get("deletions").is_none() || body["deletions"].is_null());
    }

    #[tokio::test]
    async fn subsequent_push_sends_only_changed_files() {
        let mock_server = MockServer::start().await;

        Mock::given(method("PUT"))
            .and(path("/api/v1/repos/bob/my-repo/push"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "commitSha": "def456",
                "version": 2,
                "filesChanged": 1,
                "created": false
            })))
            .mount(&mock_server)
            .await;

        let temp_dir = tempfile::tempdir().unwrap();
        std::fs::write(temp_dir.path().join("a.txt"), "unchanged").unwrap();
        std::fs::write(temp_dir.path().join("b.txt"), "modified").unwrap();
        std::fs::write(
            temp_dir.path().join(".syns.yaml"),
            "owner: bob\nname: my-repo\n",
        )
        .unwrap();

        let cache_dir = tempfile::tempdir().unwrap();
        let mut manifest = Manifest::default();
        manifest.update(
            "old-sha".into(),
            HashMap::from([
                ("a.txt".into(), blob_sha1(b"unchanged")),
                ("b.txt".into(), blob_sha1(b"original")),
            ]),
        );
        manifest.save(cache_dir.path(), "bob", "my-repo").unwrap();

        let client = SynsClient::new(&mock_server.uri()).unwrap();

        let result = smart_push(
            &client,
            "test-token",
            "bob/my-repo",
            temp_dir.path(),
            SmartPushOptions {
                force: false,
                message: "update".into(),
                author: Some("bob".into()),
                parent_sha: None,
                excludes: vec![],
                cache_dir: cache_dir.path().to_path_buf(),
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
            },
        )
        .await;

        assert!(result.is_ok());

        let requests = mock_server.received_requests().await.unwrap();
        let put_request = requests
            .iter()
            .find(|r| r.method == reqwest::Method::PUT)
            .unwrap();
        let body: serde_json::Value = serde_json::from_slice(&put_request.body).unwrap();

        let files = body["files"].as_array().unwrap();
        let a_entry = files.iter().find(|f| f["path"] == "a.txt").unwrap();
        assert!(
            a_entry["content"].is_null(),
            "unchanged file should be sha-only"
        );
        let b_entry = files.iter().find(|f| f["path"] == "b.txt").unwrap();
        assert_eq!(b_entry["content"].as_str(), Some("modified"));
        assert_eq!(body["parentSha"].as_str(), Some("old-sha"));
        assert!(body.get("deletions").is_none() || body["deletions"].is_null());
    }

    #[tokio::test]
    async fn deleted_files_in_delete_list() {
        let mock_server = MockServer::start().await;

        Mock::given(method("PUT"))
            .and(path("/api/v1/repos/owner/repo/push"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "commitSha": "del123",
                "version": 3,
                "filesChanged": 1,
                "created": false
            })))
            .mount(&mock_server)
            .await;

        let temp_dir = tempfile::tempdir().unwrap();
        std::fs::write(temp_dir.path().join("keep.txt"), "keep").unwrap();
        std::fs::write(
            temp_dir.path().join(".syns.yaml"),
            "owner: owner\nname: repo\n",
        )
        .unwrap();

        let cache_dir = tempfile::tempdir().unwrap();
        let mut manifest = Manifest::default();
        manifest.update(
            "prev-sha".into(),
            HashMap::from([
                ("keep.txt".into(), blob_sha1(b"keep")),
                ("removed.txt".into(), blob_sha1(b"gone")),
            ]),
        );
        manifest.save(cache_dir.path(), "owner", "repo").unwrap();

        let client = SynsClient::new(&mock_server.uri()).unwrap();

        let result = smart_push(
            &client,
            "test-token",
            "owner/repo",
            temp_dir.path(),
            SmartPushOptions {
                force: false,
                message: "delete".into(),
                author: Some("owner".into()),
                parent_sha: None,
                excludes: vec![],
                cache_dir: cache_dir.path().to_path_buf(),
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
            },
        )
        .await;

        assert!(result.is_ok());

        let requests = mock_server.received_requests().await.unwrap();
        let put_request = requests
            .iter()
            .find(|r| r.method == reqwest::Method::PUT)
            .unwrap();
        let body: serde_json::Value = serde_json::from_slice(&put_request.body).unwrap();

        let deletes: Vec<&str> = body["deletions"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v["path"].as_str().unwrap())
            .collect();
        assert_eq!(deletes, vec!["removed.txt"]);

        let files = body["files"].as_array().unwrap();
        let keep_entry = files.iter().find(|f| f["path"] == "keep.txt").unwrap();
        assert!(
            keep_entry["content"].is_null(),
            "unchanged file should be sha-only"
        );
    }

    #[tokio::test]
    async fn missing_blobs_409_triggers_retry() {
        let mock_server = MockServer::start().await;

        // 200 response with lower priority (fallback)
        Mock::given(method("PUT"))
            .and(path("/api/v1/repos/owner/repo/push"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "commitSha": "retry123",
                "version": 4,
                "filesChanged": 2,
                "created": false
            })))
            .with_priority(2)
            .mount(&mock_server)
            .await;

        // 409 response with higher priority, only once
        Mock::given(method("PUT"))
            .and(path("/api/v1/repos/owner/repo/push"))
            .respond_with(ResponseTemplate::new(409).set_body_json(serde_json::json!({
                "error": "missing_blobs",
                "missing": {"b.txt": blob_sha1(b"old-b")},
            })))
            .with_priority(1)
            .up_to_n_times(1)
            .mount(&mock_server)
            .await;

        let temp_dir = tempfile::tempdir().unwrap();
        std::fs::write(temp_dir.path().join("a.txt"), "new-a").unwrap();
        std::fs::write(temp_dir.path().join("b.txt"), "old-b").unwrap();
        std::fs::write(
            temp_dir.path().join(".syns.yaml"),
            "owner: owner\nname: repo\n",
        )
        .unwrap();

        let cache_dir = tempfile::tempdir().unwrap();
        let mut manifest = Manifest::default();
        manifest.update(
            "base-sha".into(),
            HashMap::from([
                ("a.txt".into(), blob_sha1(b"old-a")),
                ("b.txt".into(), blob_sha1(b"old-b")),
            ]),
        );
        manifest.save(cache_dir.path(), "owner", "repo").unwrap();

        let client = SynsClient::new(&mock_server.uri()).unwrap();

        let result = smart_push(
            &client,
            "test-token",
            "owner/repo",
            temp_dir.path(),
            SmartPushOptions {
                force: false,
                message: "retry".into(),
                author: Some("owner".into()),
                parent_sha: None,
                excludes: vec![],
                cache_dir: cache_dir.path().to_path_buf(),
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
            },
        )
        .await;

        assert!(result.is_ok());

        let requests = mock_server.received_requests().await.unwrap();
        let put_requests: Vec<_> = requests
            .iter()
            .filter(|r| r.method == reqwest::Method::PUT)
            .collect();
        assert_eq!(put_requests.len(), 2, "should have made 2 PUT requests");

        // First request: b.txt should be sha-only (unchanged per manifest)
        let body1: serde_json::Value = serde_json::from_slice(&put_requests[0].body).unwrap();
        let b_entry1 = body1["files"]
            .as_array()
            .unwrap()
            .iter()
            .find(|f| f["path"] == "b.txt")
            .unwrap()
            .clone();
        assert!(b_entry1["content"].is_null());

        // Second request (retry): b.txt should have content (upgraded)
        let body2: serde_json::Value = serde_json::from_slice(&put_requests[1].body).unwrap();
        let b_entry2 = body2["files"]
            .as_array()
            .unwrap()
            .iter()
            .find(|f| f["path"] == "b.txt")
            .unwrap()
            .clone();
        assert_eq!(b_entry2["content"].as_str(), Some("old-b"));
    }

    #[tokio::test]
    async fn force_bypasses_manifest_sends_all() {
        let mock_server = MockServer::start().await;

        Mock::given(method("PUT"))
            .and(path("/api/v1/repos/owner/repo/push"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "commitSha": "force123",
                "version": 5,
                "filesChanged": 2,
                "created": false
            })))
            .mount(&mock_server)
            .await;

        let temp_dir = tempfile::tempdir().unwrap();
        std::fs::write(temp_dir.path().join("a.txt"), "aaa").unwrap();
        std::fs::write(temp_dir.path().join("b.txt"), "bbb").unwrap();
        std::fs::write(
            temp_dir.path().join(".syns.yaml"),
            "owner: owner\nname: repo\n",
        )
        .unwrap();

        let cache_dir = tempfile::tempdir().unwrap();
        let mut manifest = Manifest::default();
        manifest.update(
            "old-sha".into(),
            HashMap::from([
                ("a.txt".into(), blob_sha1(b"aaa")),
                ("b.txt".into(), blob_sha1(b"bbb")),
            ]),
        );
        manifest.save(cache_dir.path(), "owner", "repo").unwrap();

        let client = SynsClient::new(&mock_server.uri()).unwrap();

        let result = smart_push(
            &client,
            "test-token",
            "owner/repo",
            temp_dir.path(),
            SmartPushOptions {
                force: true,
                message: "force".into(),
                author: Some("owner".into()),
                parent_sha: None,
                excludes: vec![],
                cache_dir: cache_dir.path().to_path_buf(),
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
            },
        )
        .await;

        assert!(result.is_ok());

        let requests = mock_server.received_requests().await.unwrap();
        // Only PUT requests, no GET (force doesn't fetch tree)
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0].method, reqwest::Method::PUT);

        let body: serde_json::Value = serde_json::from_slice(&requests[0].body).unwrap();
        let files = body["files"].as_array().unwrap();
        let a_entry = files.iter().find(|f| f["path"] == "a.txt").unwrap();
        assert!(
            a_entry["content"].is_string(),
            "force should send all content"
        );
        let b_entry = files.iter().find(|f| f["path"] == "b.txt").unwrap();
        assert!(
            b_entry["content"].is_string(),
            "force should send all content"
        );
        assert!(
            body["parentSha"].is_null(),
            "force defaults parent_sha to None"
        );
        assert!(body.get("deletions").is_none() || body["deletions"].is_null());

        // Manifest was still saved
        assert!(cache_dir.path().join("owner").join("repo.json").exists());
    }

    fn opts_with(cache: PathBuf, strict: bool, allow_empty: bool) -> SmartPushOptions {
        SmartPushOptions {
            force: false,
            message: "msg".into(),
            author: Some("alice".into()),
            parent_sha: None,
            excludes: vec![],
            cache_dir: cache,
            description: None,
            tags: None,
            status: None,
            visibility: None,
            strict,
            allow_empty,
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
        }
    }

    #[tokio::test]
    async fn response_with_empty_commit_sha_does_not_write_manifest() {
        let mock_server = MockServer::start().await;

        Mock::given(method("GET"))
            .and(path("/api/v1/repos/alice/repo/tree"))
            .respond_with(
                ResponseTemplate::new(404).set_body_json(serde_json::json!({"error": "not_found"})),
            )
            .mount(&mock_server)
            .await;

        Mock::given(method("PUT"))
            .and(path("/api/v1/repos/alice/repo/push"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "commitSha": "",
                "version": 0,
                "filesChanged": 0,
                "created": true
            })))
            .mount(&mock_server)
            .await;

        let temp_dir = tempfile::tempdir().unwrap();
        std::fs::write(temp_dir.path().join("text.txt"), "hello").unwrap();
        let cache_dir = tempfile::tempdir().unwrap();
        let client = SynsClient::new(&mock_server.uri()).unwrap();

        let result = smart_push(
            &client,
            "test-token",
            "alice/repo",
            temp_dir.path(),
            opts_with(cache_dir.path().to_path_buf(), false, false),
        )
        .await;

        let (response, _raw, meta) = result.unwrap();
        assert_eq!(response.commit_sha, "");
        assert!(!meta.manifest_existed);
        assert!(
            !cache_dir.path().join("alice").join("repo.json").exists(),
            "manifest must NOT be persisted when commit_sha is empty"
        );
    }

    #[tokio::test]
    async fn legacy_stub_treated_as_no_manifest() {
        let mock_server = MockServer::start().await;

        Mock::given(method("GET"))
            .and(path("/api/v1/repos/alice/repo/tree"))
            .respond_with(
                ResponseTemplate::new(404).set_body_json(serde_json::json!({"error": "not_found"})),
            )
            .mount(&mock_server)
            .await;

        Mock::given(method("PUT"))
            .and(path("/api/v1/repos/alice/repo/push"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "commitSha": "realsha",
                "version": 1,
                "filesChanged": 1,
                "created": true
            })))
            .mount(&mock_server)
            .await;

        let temp_dir = tempfile::tempdir().unwrap();
        std::fs::write(temp_dir.path().join("text.txt"), "hello").unwrap();

        let cache_dir = tempfile::tempdir().unwrap();
        let stub_path = cache_dir.path().join("alice").join("repo.json");
        std::fs::create_dir_all(stub_path.parent().unwrap()).unwrap();
        std::fs::write(&stub_path, r#"{"commit_sha":"","files":{}}"#).unwrap();

        let client = SynsClient::new(&mock_server.uri()).unwrap();

        let result = smart_push(
            &client,
            "test-token",
            "alice/repo",
            temp_dir.path(),
            opts_with(cache_dir.path().to_path_buf(), false, false),
        )
        .await;

        let (_response, _raw, meta) = result.unwrap();
        assert!(
            !meta.manifest_existed,
            "stub should be rejected by Manifest::load"
        );

        // After call: cache file is overwritten by Phase 6 save.
        let body = std::fs::read_to_string(&stub_path).unwrap();
        let v: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(v["commit_sha"].as_str(), Some("realsha"));
        assert!(v["files"].as_object().unwrap().contains_key("text.txt"));

        // Verify wire request had parentSha: null (no manifest carried forward).
        let requests = mock_server.received_requests().await.unwrap();
        let put_request = requests
            .iter()
            .find(|r| r.method == reqwest::Method::PUT)
            .unwrap();
        let body: serde_json::Value = serde_json::from_slice(&put_request.body).unwrap();
        assert!(body["parentSha"].is_null());
    }

    #[tokio::test]
    async fn strict_mode_aborts_when_files_skipped() {
        let mock_server = MockServer::start().await;
        // No mock mounted — strict guard fires before any wire call.

        let temp_dir = tempfile::tempdir().unwrap();
        std::fs::write(temp_dir.path().join("text.txt"), "hello").unwrap();
        std::fs::File::create(temp_dir.path().join("binary.bin"))
            .unwrap()
            .set_len(crate::push::collector::MAX_FILE_BYTES + 1)
            .unwrap();

        let cache_dir = tempfile::tempdir().unwrap();
        let client = SynsClient::new(&mock_server.uri()).unwrap();

        let result = smart_push(
            &client,
            "test-token",
            "alice/repo",
            temp_dir.path(),
            opts_with(cache_dir.path().to_path_buf(), true, false),
        )
        .await;

        match result {
            Err(CliError::PushPartial { skipped, .. }) => {
                assert_eq!(skipped.len(), 1);
                assert_eq!(skipped[0].path, "binary.bin");
            }
            other => panic!("expected PushPartial, got {other:?}"),
        }

        let requests = mock_server.received_requests().await.unwrap();
        assert!(
            requests.is_empty(),
            "strict guard must fire before any wire call"
        );
    }

    #[tokio::test]
    async fn empty_collection_aborts_with_push_empty() {
        let mock_server = MockServer::start().await;

        // Pre-create .syns.yaml so Phase 1 is a no-op, then use a
        // catch-all --exclude pattern so the collector produces an
        // empty `files` map. (Phase 1 always writes .syns.yaml if
        // absent, so the dir is never literally empty post-Phase 1;
        // the SPEC's "empty collection" scenario is therefore tested
        // by ensuring `files` is empty, not the dir.)
        let temp_dir = tempfile::tempdir().unwrap();
        std::fs::write(
            temp_dir.path().join(".syns.yaml"),
            "owner: alice\nname: repo\n",
        )
        .unwrap();

        let cache_dir = tempfile::tempdir().unwrap();
        let client = SynsClient::new(&mock_server.uri()).unwrap();

        let mut opts = opts_with(cache_dir.path().to_path_buf(), false, false);
        opts.excludes = vec!["*".to_string()];

        let result = smart_push(&client, "test-token", "alice/repo", temp_dir.path(), opts).await;

        match result {
            Err(CliError::PushEmpty {
                path: _,
                total_walked: _,
                cause,
            }) => {
                // CODE_REVIEW M6: assert the SPECIFIC user-exclude
                // cause. Setup uses `excludes: ["*"]`, so the only
                // skipped file (.syns.yaml) lands in UserExclude and
                // the cause sentence MUST contain "--exclude pattern".
                // Previously the assertion `contains("exclude")` would
                // also pass for the DefaultExcludeDir, Gitignore, and
                // Synsignore majority strings — a regression that
                // misattributed user-exclude as gitignore would have
                // gone unnoticed.
                assert!(cause.contains("--exclude pattern"), "cause was: {cause}");
            }
            other => panic!("expected PushEmpty, got {other:?}"),
        }

        // SPEC u255 `smart_push` 5 moved the empty guard BELOW the
        // reference-set build, so a run may now read `EP-tree` before
        // refusing — a publication carrying only deletions has to be
        // distinguishable from one carrying nothing at all. What the
        // guard still promises is that no publication reaches the
        // server: no PUT is issued.
        let requests = mock_server.received_requests().await.unwrap();
        assert!(
            requests.iter().all(|r| r.method != reqwest::Method::PUT),
            "empty guard must fire before the publication reaches the server"
        );
    }

    #[tokio::test]
    async fn allow_empty_bypasses_push_empty_guard() {
        let mock_server = MockServer::start().await;

        Mock::given(method("GET"))
            .and(path("/api/v1/repos/alice/repo/tree"))
            .respond_with(
                ResponseTemplate::new(404).set_body_json(serde_json::json!({"error": "not_found"})),
            )
            .mount(&mock_server)
            .await;

        Mock::given(method("PUT"))
            .and(path("/api/v1/repos/alice/repo/push"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "commitSha": "abc",
                "version": 1,
                "filesChanged": 0,
                "created": true
            })))
            .mount(&mock_server)
            .await;

        // Pre-create .syns.yaml + catch-all exclude → empty `files`
        // collection; --allow-empty should let the wire call proceed.
        let temp_dir = tempfile::tempdir().unwrap();
        std::fs::write(
            temp_dir.path().join(".syns.yaml"),
            "owner: alice\nname: repo\n",
        )
        .unwrap();
        let cache_dir = tempfile::tempdir().unwrap();
        let client = SynsClient::new(&mock_server.uri()).unwrap();

        let mut opts = opts_with(cache_dir.path().to_path_buf(), false, true);
        opts.excludes = vec!["*".to_string()];

        let result = smart_push(&client, "test-token", "alice/repo", temp_dir.path(), opts).await;

        let (_response, _raw, _meta) = result.unwrap();
        // meta.skipped is non-empty here (the .syns.yaml is excluded
        // by `*`), but the wire call proceeds because of --allow-empty.

        let requests = mock_server.received_requests().await.unwrap();
        let put_requests: Vec<_> = requests
            .iter()
            .filter(|r| r.method == reqwest::Method::PUT)
            .collect();
        assert_eq!(put_requests.len(), 1);
    }

    #[tokio::test]
    async fn strict_supersedes_empty_when_both_apply() {
        let mock_server = MockServer::start().await;

        let temp_dir = tempfile::tempdir().unwrap();
        std::fs::File::create(temp_dir.path().join("binary.bin"))
            .unwrap()
            .set_len(crate::push::collector::MAX_FILE_BYTES + 1)
            .unwrap();
        let cache_dir = tempfile::tempdir().unwrap();
        let client = SynsClient::new(&mock_server.uri()).unwrap();

        let result = smart_push(
            &client,
            "test-token",
            "alice/repo",
            temp_dir.path(),
            opts_with(cache_dir.path().to_path_buf(), true, false),
        )
        .await;

        match result {
            Err(CliError::PushPartial { skipped, .. }) => {
                assert_eq!(skipped.len(), 1);
            }
            Err(CliError::PushEmpty { .. }) => {
                panic!("strict must supersede empty when both apply (D10)");
            }
            other => panic!("expected PushPartial, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn smart_push_auto_chunks_when_pre_flight_body_exceeds_budget() {
        let mock_server = MockServer::start().await;

        // First-push 404 on /tree so smart_push treats this as first push.
        // Envelope matches INTERFACES.md § 1.4 (GET /tree returns
        // {error: "repo_not_found", message: "Repository not found"}).
        Mock::given(method("GET"))
            .and(path("/api/v1/repos/alice/repo/tree"))
            .respond_with(ResponseTemplate::new(404).set_body_json(serde_json::json!({
                "error": "repo_not_found",
                "message": "Repository not found",
            })))
            .mount(&mock_server)
            .await;

        // First PUT → batch 1 response (commitSha aaaa...001).
        Mock::given(method("PUT"))
            .and(path("/api/v1/repos/alice/repo/push"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "commitSha": "aaaaaaaa00000000000000000000000000000001",
                "version": 1,
                "filesChanged": 1,
                "created": true,
            })))
            .with_priority(1)
            .up_to_n_times(1)
            .mount(&mock_server)
            .await;

        // Subsequent PUT → batch 2 response (commitSha bbbb...002).
        Mock::given(method("PUT"))
            .and(path("/api/v1/repos/alice/repo/push"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "commitSha": "bbbbbbbb00000000000000000000000000000002",
                "version": 2,
                "filesChanged": 2,
                "created": false,
            })))
            .with_priority(2)
            .mount(&mock_server)
            .await;

        let temp_dir = tempfile::tempdir().unwrap();
        std::fs::write(
            temp_dir.path().join("big1.txt"),
            vec![b'a'; 14 * 1024 * 1024],
        )
        .unwrap();
        std::fs::write(
            temp_dir.path().join("big2.txt"),
            vec![b'b'; 14 * 1024 * 1024],
        )
        .unwrap();
        std::fs::write(temp_dir.path().join("small.txt"), vec![b'c'; 1024]).unwrap();

        let cache_dir = tempfile::tempdir().unwrap();
        let client = SynsClient::new(&mock_server.uri()).unwrap();

        let result = smart_push(
            &client,
            "test-token",
            "alice/repo",
            temp_dir.path(),
            SmartPushOptions {
                force: false,
                message: "init".into(),
                author: Some("alice".into()),
                parent_sha: None,
                excludes: vec![],
                cache_dir: cache_dir.path().to_path_buf(),
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
            },
        )
        .await;

        let (response, _raw, _meta) = result.expect("smart_push must succeed with chunker");
        assert_eq!(
            response.commit_sha, "bbbbbbbb00000000000000000000000000000002",
            "final response should carry the FINAL batch's commit_sha",
        );

        let requests = mock_server.received_requests().await.unwrap();
        let puts: Vec<_> = requests
            .iter()
            .filter(|r| r.method == reqwest::Method::PUT)
            .collect();
        assert_eq!(puts.len(), 2, "expected exactly 2 PUTs from the chunker");

        let body1: serde_json::Value = serde_json::from_slice(&puts[0].body).unwrap();
        assert!(
            body1["parentSha"].is_null() || body1.get("parentSha").is_none(),
            "first batch parentSha should be null/absent, got: {}",
            body1["parentSha"]
        );
        assert!(
            body1["message"]
                .as_str()
                .is_some_and(|m| m.contains("(part 1/2)")),
            "first batch message should include '(part 1/2)', got: {:?}",
            body1["message"]
        );

        let body2: serde_json::Value = serde_json::from_slice(&puts[1].body).unwrap();
        assert_eq!(
            body2["parentSha"],
            serde_json::Value::String("aaaaaaaa00000000000000000000000000000001".into()),
        );
        assert!(
            body2["message"]
                .as_str()
                .is_some_and(|m| m.contains("(part 2/2)")),
        );

        let manifest = Manifest::load(cache_dir.path(), "alice", "repo")
            .expect("manifest should be saved after successful chunked push");
        assert_eq!(
            manifest.commit_sha(),
            Some("bbbbbbbb00000000000000000000000000000002"),
        );
        let stored: std::collections::HashSet<&str> = manifest.file_paths().collect();
        assert!(stored.contains("big1.txt"));
        assert!(stored.contains("big2.txt"));
        assert!(stored.contains("small.txt"));
    }

    #[tokio::test]
    async fn smart_push_propagates_payload_too_large_with_offending_batch_metrics_when_single_file_too_big()
     {
        let mock_server = MockServer::start().await;

        // First-push 404 on /tree.
        // Envelope matches INTERFACES.md § 1.4 (GET /tree returns
        // {error: "repo_not_found", message: "Repository not found"}).
        Mock::given(method("GET"))
            .and(path("/api/v1/repos/alice/repo/tree"))
            .respond_with(ResponseTemplate::new(404).set_body_json(serde_json::json!({
                "error": "repo_not_found",
                "message": "Repository not found",
            })))
            .mount(&mock_server)
            .await;

        // Every PUT returns 413 with Cloudflare HTML body (real edge response).
        Mock::given(method("PUT"))
            .and(path("/api/v1/repos/alice/repo/push"))
            .respond_with(
                ResponseTemplate::new(413).set_body_string(
                    "<html><head><title>413 Request Entity Too Large</title></head><body>\n<center>cloudflare</center>\n</body></html>",
                ),
            )
            .mount(&mock_server)
            .await;

        let temp_dir = tempfile::tempdir().unwrap();
        // One file at `MAX_FILE_BYTES` — its entry alone exceeds
        // CHUNK_BUDGET_BYTES once framed.
        // Pre-create .syns.yaml AND exclude it so the collector yields
        // exactly ONE entry. Without the exclude, the collector picks
        // up .syns.yaml as a second file (`.hidden(false)` in the walker
        // does not skip hidden files), the chunker produces n==2
        // batches, and the LOOP's rewrite arm fires — masking the
        // n==1 short-circuit's correctness. This is the HIGH-1 fix.
        std::fs::write(
            temp_dir.path().join(".syns.yaml"),
            "owner: alice\nname: repo\n",
        )
        .unwrap();
        let single_size = crate::push::collector::MAX_FILE_BYTES as usize;
        std::fs::write(temp_dir.path().join("huge.txt"), vec![b'x'; single_size]).unwrap();

        let cache_dir = tempfile::tempdir().unwrap();
        let client = SynsClient::new(&mock_server.uri()).unwrap();

        let result = smart_push(
            &client,
            "test-token",
            "alice/repo",
            temp_dir.path(),
            SmartPushOptions {
                force: false,
                message: "init".into(),
                author: Some("alice".into()),
                parent_sha: None,
                excludes: vec![".syns.yaml".into()],
                cache_dir: cache_dir.path().to_path_buf(),
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
            },
        )
        .await;

        match result {
            Err(CliError::PayloadTooLarge {
                bytes_sent,
                file_count,
                rejecter,
            }) => {
                // SPEC u280: the batch's serialised length — the content
                // and the frame around it.
                assert!(
                    bytes_sent > single_size as u64 && bytes_sent < single_size as u64 + 1024,
                    "bytes_sent should be the offending batch's serialised length, got {bytes_sent}",
                );
                assert_eq!(file_count, 1, "file_count should be the batch size (1)");
                assert!(
                    matches!(rejecter, EdgeRejecter::Cloudflare),
                    "rejecter should be Cloudflare from the HTML body sniff, got {rejecter:?}",
                );
            }
            other => panic!("expected Err(PayloadTooLarge), got: {other:?}"),
        }
    }

    // MED-3: 409 missing_blobs retry → upgraded body exceeds budget →
    // chunker fires. Primes the manifest with SHA-only entries whose
    // upgraded full content total > CHUNK_BUDGET_BYTES (14 MiB + 14 MiB
    // = 28 MiB > 25 MiB). Mocks: first PUT → 409 missing_blobs; second
    // and subsequent PUTs → 200. Verifies (a) chunker fires after the
    // 409 (≥ 2 PUTs after the first one), (b) final response is the
    // last batch's, (c) parentSha chains across the chunked PUTs.
    #[tokio::test]
    async fn smart_push_chunks_when_409_missing_blobs_retry_body_exceeds_budget() {
        let mock_server = MockServer::start().await;

        // Priority 1, up_to_n_times(1) → first PUT returns 409 missing_blobs.
        Mock::given(method("PUT"))
            .and(path("/api/v1/repos/charlie/repo/push"))
            .respond_with(ResponseTemplate::new(409).set_body_json(serde_json::json!({
                "error": "missing_blobs",
                "message": "some referenced blobs are missing on the server",
                "missing": {
                    "big1.txt": blob_sha1(&vec![b'a'; 14 * 1024 * 1024]),
                    "big2.txt": blob_sha1(&vec![b'b'; 14 * 1024 * 1024]),
                },
            })))
            .with_priority(1)
            .up_to_n_times(1)
            .mount(&mock_server)
            .await;

        // Priority 2, up_to_n_times(1) → second PUT (chunked batch 1) returns 200 / aaaa...001.
        Mock::given(method("PUT"))
            .and(path("/api/v1/repos/charlie/repo/push"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "commitSha": "aaaaaaaa00000000000000000000000000000001",
                "version": 2,
                "filesChanged": 1,
                "created": false,
            })))
            .with_priority(2)
            .up_to_n_times(1)
            .mount(&mock_server)
            .await;

        // Priority 3 → third PUT (chunked batch 2) returns 200 / bbbb...002.
        Mock::given(method("PUT"))
            .and(path("/api/v1/repos/charlie/repo/push"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "commitSha": "bbbbbbbb00000000000000000000000000000002",
                "version": 3,
                "filesChanged": 1,
                "created": false,
            })))
            .with_priority(3)
            .mount(&mock_server)
            .await;

        // Pre-create local files (14 MiB + 14 MiB = 28 MiB > 25 MiB
        // after upgrade) and .syns.yaml + matching manifest so the
        // happy path emits SHA-only entries (no content) on the first
        // PUT — the 409 then forces an upgrade to full content, and
        // the upgraded body exceeds the budget → chunker fires.
        let temp_dir = tempfile::tempdir().unwrap();
        let big1_content = vec![b'a'; 14 * 1024 * 1024];
        let big2_content = vec![b'b'; 14 * 1024 * 1024];
        std::fs::write(temp_dir.path().join("big1.txt"), &big1_content).unwrap();
        std::fs::write(temp_dir.path().join("big2.txt"), &big2_content).unwrap();
        std::fs::write(
            temp_dir.path().join(".syns.yaml"),
            "owner: charlie\nname: repo\n",
        )
        .unwrap();

        // Prime the manifest with matching SHAs for big1 and big2 (so
        // build_push_entries produces SHA-only entries — content is
        // None for the changed=false files because their SHA matches
        // the manifest's reference).
        let cache_dir = tempfile::tempdir().unwrap();
        let big1_sha = blob_sha1(&big1_content);
        let big2_sha = blob_sha1(&big2_content);
        let mut manifest = Manifest::default();
        manifest.update(
            "old-parent-sha".to_string(),
            HashMap::from([
                ("big1.txt".to_string(), big1_sha.clone()),
                ("big2.txt".to_string(), big2_sha.clone()),
            ]),
        );
        manifest.save(cache_dir.path(), "charlie", "repo").unwrap();

        let client = SynsClient::new(&mock_server.uri()).unwrap();
        let result = smart_push(
            &client,
            "test-token",
            "charlie/repo",
            temp_dir.path(),
            SmartPushOptions {
                force: false,
                message: "retry-upgrade".into(),
                author: Some("charlie".into()),
                parent_sha: None,
                excludes: vec![".syns.yaml".into()],
                cache_dir: cache_dir.path().to_path_buf(),
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
            },
        )
        .await;

        let (response, _raw, _meta) = result.expect("smart_push must succeed on 409→chunker path");
        assert_eq!(
            response.commit_sha, "bbbbbbbb00000000000000000000000000000002",
            "final response should carry the LAST batch's commit_sha",
        );

        let requests = mock_server.received_requests().await.unwrap();
        let puts: Vec<_> = requests
            .iter()
            .filter(|r| r.method == reqwest::Method::PUT)
            .collect();
        assert_eq!(
            puts.len(),
            3,
            "expected 1 initial 409 PUT + 2 chunked PUTs = 3 total",
        );

        // First PUT: SHA-only entries (no content), parentSha = manifest.
        let body0: serde_json::Value = serde_json::from_slice(&puts[0].body).unwrap();
        assert_eq!(body0["parentSha"].as_str(), Some("old-parent-sha"));

        // Second PUT (chunked batch 1): full content, parentSha inherits
        // the original request's parent_sha (i.e. manifest's commit_sha).
        let body1: serde_json::Value = serde_json::from_slice(&puts[1].body).unwrap();
        assert_eq!(body1["parentSha"].as_str(), Some("old-parent-sha"));
        assert!(
            body1["message"]
                .as_str()
                .is_some_and(|m| m.contains("(part 1/2)")),
            "chunked batch 1 message should include '(part 1/2)', got: {:?}",
            body1["message"],
        );

        // Third PUT (chunked batch 2): parentSha chains to batch 1's commit.
        let body2: serde_json::Value = serde_json::from_slice(&puts[2].body).unwrap();
        assert_eq!(
            body2["parentSha"].as_str(),
            Some("aaaaaaaa00000000000000000000000000000001"),
        );
        assert!(
            body2["message"]
                .as_str()
                .is_some_and(|m| m.contains("(part 2/2)")),
        );
    }

    // HIGH-2 backstop: when batch k of n > 1 fails AFTER batch k-1 has
    // committed, the chunker MUST save a per-batch manifest snapshot
    // anchored to batch k-1's commit_sha so the next push computes
    // diffs from the actual server HEAD (not the stale pre-push
    // parent). Mocks: first PUT → 200 / aaaa...001; second PUT → 413
    // Cloudflare HTML. Verifies (a) the chunker returns Err, (b) the
    // local manifest now points at aaaa...001 (not at the original
    // parent, which would be None for a first push).
    #[tokio::test]
    async fn smart_push_chunker_saves_partial_manifest_on_intermediate_batch_failure() {
        let mock_server = MockServer::start().await;

        // First-push 404 on /tree (matches INTERFACES.md § 1.4).
        Mock::given(method("GET"))
            .and(path("/api/v1/repos/dave/repo/tree"))
            .respond_with(ResponseTemplate::new(404).set_body_json(serde_json::json!({
                "error": "repo_not_found",
                "message": "Repository not found",
            })))
            .mount(&mock_server)
            .await;

        // Priority 1, up_to_n_times(1): batch 1 succeeds with aaaa...001.
        Mock::given(method("PUT"))
            .and(path("/api/v1/repos/dave/repo/push"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "commitSha": "aaaaaaaa00000000000000000000000000000001",
                "version": 1,
                "filesChanged": 1,
                "created": true,
            })))
            .with_priority(1)
            .up_to_n_times(1)
            .mount(&mock_server)
            .await;

        // Priority 2: batch 2 onwards fail with Cloudflare 413.
        Mock::given(method("PUT"))
            .and(path("/api/v1/repos/dave/repo/push"))
            .respond_with(ResponseTemplate::new(413).set_body_string(
                "<html><head><title>413 Request Entity Too Large</title></head><body>\n<center>cloudflare</center>\n</body></html>",
            ))
            .with_priority(2)
            .mount(&mock_server)
            .await;

        // Two 14 MiB files force n=2 (FFD packs big1 alone + big2 alone
        // because 2 × 14 MiB > 25 MiB budget; small files would coalesce).
        let temp_dir = tempfile::tempdir().unwrap();
        std::fs::write(
            temp_dir.path().join("big1.txt"),
            vec![b'a'; 14 * 1024 * 1024],
        )
        .unwrap();
        std::fs::write(
            temp_dir.path().join("big2.txt"),
            vec![b'b'; 14 * 1024 * 1024],
        )
        .unwrap();

        let cache_dir = tempfile::tempdir().unwrap();
        let client = SynsClient::new(&mock_server.uri()).unwrap();

        let result = smart_push(
            &client,
            "test-token",
            "dave/repo",
            temp_dir.path(),
            SmartPushOptions {
                force: false,
                message: "partial-fail".into(),
                author: Some("dave".into()),
                parent_sha: None,
                excludes: vec![".syns.yaml".into()],
                cache_dir: cache_dir.path().to_path_buf(),
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
            },
        )
        .await;

        // The chunker fails on batch 2 — Err propagates out.
        assert!(
            matches!(result, Err(CliError::PayloadTooLarge { .. })),
            "expected Err(PayloadTooLarge) on partial chunker failure, got: {result:?}",
        );

        // The per-batch manifest snapshot (HIGH-2) MUST have been
        // saved against batch 1's commit_sha — not absent, not at the
        // pre-push parent (None for a first push). The next push will
        // diff from aaaa...001 and only re-upload the missing files.
        let loaded = Manifest::load(cache_dir.path(), "dave", "repo")
            .expect("per-batch manifest should be saved after batch 1 success");
        assert_eq!(
            loaded.commit_sha(),
            Some("aaaaaaaa00000000000000000000000000000001"),
            "manifest must point at batch 1's commit_sha (server HEAD), not the original parent",
        );
        let stored: std::collections::HashSet<&str> = loaded.file_paths().collect();
        // Exactly the file in batch 1 should be recorded (which one
        // depends on FFD order — both are 14 MiB so it's stable but
        // we don't depend on it; we just assert at least one of the
        // big files is recorded so the next push can dedup).
        assert!(
            stored.contains("big1.txt") || stored.contains("big2.txt"),
            "manifest should contain at least one of the big files after batch 1, got: {stored:?}",
        );
    }

    // ---- u255: prefix-confined deletions, and the record merge -----

    fn sha_map(pairs: &[(&str, &str)]) -> HashMap<String, String> {
        pairs
            .iter()
            .map(|(p, s)| (p.to_string(), s.to_string()))
            .collect()
    }

    #[test]
    fn build_push_entries_names_no_deletion_outside_the_prefix() {
        let local_files: HashMap<String, CollectedFile> = HashMap::new();
        let reference_shas = sha_map(&[
            ("root-a.md", "aaa"),
            ("root-b.md", "bbb"),
            ("sub/nested.md", "nnn"),
        ]);

        let (entries, deletes) = build_push_entries(
            Path::new("."),
            &local_files,
            &reference_shas,
            false,
            Some("sub"),
        )
        .unwrap();

        assert!(entries.is_empty());
        let paths: Vec<&str> = deletes.iter().map(|d| d.path.as_str()).collect();
        assert_eq!(paths, vec!["sub/nested.md"]);
    }

    #[test]
    fn build_push_entries_without_a_prefix_names_every_absent_reference_path() {
        let local_files: HashMap<String, CollectedFile> = HashMap::new();
        let reference_shas = sha_map(&[("root-a.md", "aaa"), ("sub/nested.md", "nnn")]);

        let (_entries, deletes) =
            build_push_entries(Path::new("."), &local_files, &reference_shas, false, None).unwrap();

        let mut paths: Vec<&str> = deletes.iter().map(|d| d.path.as_str()).collect();
        paths.sort_unstable();
        assert_eq!(paths, vec!["root-a.md", "sub/nested.md"]);
    }

    // ---- u256: reference, expected, author, provenance --------------

    async fn mount_ok_push(mock_server: &MockServer, repo: &str, sha: &str) {
        Mock::given(method("PUT"))
            .and(path(format!("/api/v1/repos/{repo}/push")))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "commitSha": sha,
                "version": 2,
                "filesChanged": 1,
                "created": false
            })))
            .mount(mock_server)
            .await;
    }

    async fn put_bodies(mock_server: &MockServer) -> Vec<serde_json::Value> {
        mock_server
            .received_requests()
            .await
            .unwrap()
            .iter()
            .filter(|r| r.method == reqwest::Method::PUT)
            .map(|r| serde_json::from_slice(&r.body).unwrap())
            .collect()
    }

    #[tokio::test]
    async fn reference_names_the_parent_and_deletions_alone() {
        let mock_server = MockServer::start().await;
        mount_ok_push(&mock_server, "alice/repo", "new-sha").await;

        let temp_dir = tempfile::tempdir().unwrap();
        std::fs::write(
            temp_dir.path().join(".syns.yaml"),
            "owner: alice\nname: repo\n",
        )
        .unwrap();
        std::fs::write(temp_dir.path().join("a.txt"), "a").unwrap();

        // A local record naming another parent and another deletion set,
        // which a set reference must leave unread.
        let cache_dir = tempfile::tempdir().unwrap();
        let mut manifest = Manifest::default();
        manifest.update(
            "record-sha".into(),
            HashMap::from([("record-only.txt".into(), "x".into())]),
        );
        manifest.save(cache_dir.path(), "alice", "repo").unwrap();

        let client = SynsClient::new(&mock_server.uri()).unwrap();
        let mut opts = opts_with(cache_dir.path().to_path_buf(), false, false);
        opts.parent_sha = Some("parent-sha".into());
        opts.reference = Some(HashMap::from([
            ("a.txt".into(), blob_sha1(b"a")),
            ("gone.txt".into(), "g".into()),
        ]));
        let (_response, _raw, meta) =
            smart_push(&client, "t", "alice/repo", temp_dir.path(), opts.clone())
                .await
                .unwrap();

        let requests = mock_server.received_requests().await.unwrap();
        assert!(
            requests.iter().all(|r| r.method == reqwest::Method::PUT),
            "a set reference must stand in for the tree read"
        );
        let body = &put_bodies(&mock_server).await[0];
        assert_eq!(body["parentSha"], "parent-sha");
        let deletions: Vec<&str> = body["deletions"]
            .as_array()
            .unwrap()
            .iter()
            .map(|d| d["path"].as_str().unwrap())
            .collect();
        assert_eq!(deletions, vec!["gone.txt"]);
        assert_eq!(meta.sent_parent.as_deref(), Some("parent-sha"));
        assert_eq!(meta.deleted, vec!["gone.txt".to_string()]);
        assert_eq!(meta.collected.get("a.txt"), Some(&blob_sha1(b"a")));

        let record = Manifest::load(cache_dir.path(), "alice", "repo").unwrap();
        assert_eq!(record.commit_sha(), Some("new-sha"));
        assert!(record.file_sha("record-only.txt").is_none());
        assert!(record.file_sha("gone.txt").is_none());
        assert!(record.file_sha("a.txt").is_some());

        // No parent named: none is sent, whatever the record held.
        let second = MockServer::start().await;
        mount_ok_push(&second, "alice/repo", "other-sha").await;
        let client = SynsClient::new(&second.uri()).unwrap();
        opts.parent_sha = None;
        smart_push(&client, "t", "alice/repo", temp_dir.path(), opts)
            .await
            .unwrap();
        let body = &put_bodies(&second).await[0];
        assert!(body.get("parentSha").is_none(), "{body}");
    }

    #[tokio::test]
    async fn expected_mismatch_refuses_before_any_request() {
        let mock_server = MockServer::start().await;
        let temp_dir = tempfile::tempdir().unwrap();
        std::fs::write(
            temp_dir.path().join(".syns.yaml"),
            "owner: alice\nname: repo\n",
        )
        .unwrap();
        std::fs::write(temp_dir.path().join("a.txt"), "late write").unwrap();
        let cache_dir = tempfile::tempdir().unwrap();
        let client = SynsClient::new(&mock_server.uri()).unwrap();

        let mut opts = opts_with(cache_dir.path().to_path_buf(), false, false);
        opts.reference = Some(HashMap::new());
        opts.expected = Some(HashMap::from([
            (
                ".syns.yaml".into(),
                blob_sha1(b"owner: alice\nname: repo\n"),
            ),
            ("a.txt".into(), blob_sha1(b"reviewed")),
            ("removed.txt".into(), "r".into()),
        ]));
        let result = smart_push(&client, "t", "alice/repo", temp_dir.path(), opts).await;

        match result {
            Err(CliError::CollectedSetChanged { paths }) => {
                assert_eq!(paths, vec!["a.txt".to_string(), "removed.txt".to_string()]);
            }
            other => panic!("expected CollectedSetChanged, got {other:?}"),
        }
        assert!(mock_server.received_requests().await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn absent_author_sends_no_author_key() {
        let mock_server = MockServer::start().await;
        mount_ok_push(&mock_server, "alice/repo", "sha").await;
        let temp_dir = tempfile::tempdir().unwrap();
        std::fs::write(
            temp_dir.path().join(".syns.yaml"),
            "owner: alice\nname: repo\n",
        )
        .unwrap();
        let cache_dir = tempfile::tempdir().unwrap();
        let client = SynsClient::new(&mock_server.uri()).unwrap();

        let mut opts = opts_with(cache_dir.path().to_path_buf(), false, false);
        opts.author = None;
        opts.reference = Some(HashMap::new());
        smart_push(&client, "t", "alice/repo", temp_dir.path(), opts)
            .await
            .unwrap();

        let body = &put_bodies(&mock_server).await[0];
        assert!(body.get("author").is_none(), "{body}");
        assert!(body.get("provenance").is_none(), "{body}");
    }

    #[tokio::test]
    async fn provenance_rides_every_chunked_batch() {
        let mock_server = MockServer::start().await;
        mount_ok_push(&mock_server, "alice/repo", "chunk-sha").await;
        let temp_dir = tempfile::tempdir().unwrap();
        std::fs::write(
            temp_dir.path().join("big1.txt"),
            vec![b'a'; 14 * 1024 * 1024],
        )
        .unwrap();
        std::fs::write(
            temp_dir.path().join("big2.txt"),
            vec![b'b'; 14 * 1024 * 1024],
        )
        .unwrap();
        let cache_dir = tempfile::tempdir().unwrap();
        let client = SynsClient::new(&mock_server.uri()).unwrap();

        let mut opts = opts_with(cache_dir.path().to_path_buf(), false, false);
        opts.excludes = vec![".syns.yaml".into()];
        opts.reference = Some(HashMap::new());
        opts.provenance = Some(PushProvenance {
            integration: "codex".into(),
            run: "run-7".into(),
            trigger: "stop".into(),
            task_ref: None,
        });
        smart_push(&client, "t", "alice/repo", temp_dir.path(), opts)
            .await
            .unwrap();

        let bodies = put_bodies(&mock_server).await;
        assert_eq!(bodies.len(), 2, "two batches expected");
        for body in &bodies {
            assert_eq!(
                body["provenance"],
                serde_json::json!({"integration": "codex", "run": "run-7", "trigger": "stop"})
            );
        }
    }

    #[test]
    fn merged_record_keeps_the_run_start_paths_a_scoped_run_never_walked() {
        let base = sha_map(&[
            ("root-a.md", "aaa"),
            ("root-b.md", "bbb"),
            ("sub/nested.md", "nnn"),
        ]);
        let published = sha_map(&[("sub/added.md", "ddd")]);

        let merged = merged_record(&base, &["sub/nested.md".to_string()], &published);

        let mut paths: Vec<&str> = merged.keys().map(String::as_str).collect();
        paths.sort_unstable();
        assert_eq!(paths, vec!["root-a.md", "root-b.md", "sub/added.md"]);
    }
}

#[cfg(test)]
mod unclaimed_parent_tests {
    use super::*;
    use crate::push::hash::blob_sha1;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    const RECORDED: &str = "aa11bb22cc33dd44ee55ff6600778899001122bb";

    fn opts(cache_dir: &std::path::Path, force: bool) -> SmartPushOptions {
        SmartPushOptions {
            force,
            message: "push".to_string(),
            author: None,
            parent_sha: None,
            excludes: vec![],
            cache_dir: cache_dir.to_path_buf(),
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
        }
    }

    async fn mount_push(server: &MockServer) {
        Mock::given(method("PUT"))
            .and(path("/api/v1/repos/alice/notes/push"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "commitSha": "b".repeat(40), "version": 2,
                "filesChanged": 1, "created": false,
            })))
            .mount(server)
            .await;
    }

    /// SPEC u271, `src/push/smart.rs`: the parent the forced branch
    /// dropped rides out on the meta beside the parent it sent, which
    /// reads none on every forced run.
    #[tokio::test]
    async fn a_forced_run_names_the_parent_its_body_did_not_claim() {
        let server = MockServer::start().await;
        mount_push(&server).await;
        let folder = tempfile::tempdir().unwrap();
        std::fs::write(folder.path().join("a.md"), "keep one").unwrap();
        let cache = tempfile::tempdir().unwrap();

        let mut manifest = Manifest::default();
        manifest.update(
            RECORDED.to_string(),
            HashMap::from([("a.md".to_string(), blob_sha1(b"was one"))]),
        );
        manifest.save(cache.path(), "alice", "notes").unwrap();

        let client = SynsClient::new(&server.uri()).unwrap();
        let (_response, _raw, meta) = smart_push(
            &client,
            "t",
            "alice/notes",
            folder.path(),
            opts(cache.path(), true),
        )
        .await
        .unwrap();

        assert_eq!(meta.sent_parent, None, "a forced body claims no parent");
        assert_eq!(meta.unclaimed_parent.as_deref(), Some(RECORDED));

        let body: serde_json::Value = server.received_requests().await.unwrap()[0]
            .body_json()
            .unwrap();
        assert!(
            body.get("parentSha").is_none(),
            "the flag ships unchanged: it claims no parent"
        );
        let files = body["files"].as_array().unwrap();
        let sent: Vec<&str> = files.iter().map(|f| f["path"].as_str().unwrap()).collect();
        assert!(
            sent.contains(&"a.md"),
            "every collected file rides: {sent:?}"
        );
        let a = files.iter().find(|f| f["path"] == "a.md").unwrap();
        assert_eq!(a["content"], serde_json::json!("keep one"));
        assert!(
            body.get("deletions").is_none(),
            "the flag names no deletion"
        );
    }

    /// An unforced run claims the record's parent, so there is nothing
    /// unclaimed to name.
    #[tokio::test]
    async fn an_unforced_run_claims_its_parent_and_names_none_unclaimed() {
        let server = MockServer::start().await;
        mount_push(&server).await;
        let folder = tempfile::tempdir().unwrap();
        std::fs::write(folder.path().join("a.md"), "keep one").unwrap();
        let cache = tempfile::tempdir().unwrap();

        let mut manifest = Manifest::default();
        manifest.update(
            RECORDED.to_string(),
            HashMap::from([("a.md".to_string(), blob_sha1(b"was one"))]),
        );
        manifest.save(cache.path(), "alice", "notes").unwrap();

        let client = SynsClient::new(&server.uri()).unwrap();
        let (_response, _raw, meta) = smart_push(
            &client,
            "t",
            "alice/notes",
            folder.path(),
            opts(cache.path(), false),
        )
        .await
        .unwrap();

        assert_eq!(meta.sent_parent.as_deref(), Some(RECORDED));
        assert_eq!(meta.unclaimed_parent, None);
    }

    /// A forced publication into an identity holding no commit held no
    /// parent either, so it names none.
    #[tokio::test]
    async fn a_forced_run_into_an_identity_holding_no_commit_names_none() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/v1/repos/alice/notes/tree"))
            .respond_with(
                ResponseTemplate::new(404).set_body_json(serde_json::json!({"error":"not_found"})),
            )
            .mount(&server)
            .await;
        mount_push(&server).await;
        let folder = tempfile::tempdir().unwrap();
        std::fs::write(folder.path().join("a.md"), "keep one").unwrap();
        let cache = tempfile::tempdir().unwrap();

        let client = SynsClient::new(&server.uri()).unwrap();
        let (_response, _raw, meta) = smart_push(
            &client,
            "t",
            "alice/notes",
            folder.path(),
            opts(cache.path(), true),
        )
        .await
        .unwrap();

        assert_eq!(meta.sent_parent, None);
        assert_eq!(meta.unclaimed_parent, None);
    }
}

#[cfg(test)]
mod u280_publication_tests {
    use super::*;
    use crate::push::hash::blob_sha1;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn opts(cache_dir: &std::path::Path) -> SmartPushOptions {
        SmartPushOptions {
            force: false,
            message: "push".into(),
            author: None,
            parent_sha: None,
            excludes: vec![],
            cache_dir: cache_dir.to_path_buf(),
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
        }
    }

    // ---- u280: every content published, packed by its encoded length ----

    /// A frame carrying nothing but a message, as a publication's is.
    fn bare_frame() -> PushRequest {
        PushRequest {
            files: Vec::new(),
            deletions: None,
            message: Some("push".into()),
            author: None,
            parent_sha: None,
            description: None,
            tags: None,
            status: None,
            visibility: None,
            provenance: None,
        }
    }

    /// The entries of the one body `fill_batch` builds for `pending`
    /// under a bare frame.
    fn filled_entries(
        root: &std::path::Path,
        files: &HashMap<String, CollectedFile>,
        pending: &[PendingEntry],
    ) -> Vec<serde_json::Value> {
        let mut scratch = scratch_for(files, pending);
        let body = fill_batch(root, files, &bare_frame(), pending, &mut scratch).unwrap();
        let body: serde_json::Value = serde_json::from_slice(&body).unwrap();
        body["files"].as_array().unwrap().clone()
    }

    #[test]
    fn nul_past_the_scan_bound_publishes_as_bytes() {
        let dir = tempfile::tempdir().unwrap();
        let mut bytes = "prose ".repeat(20_000).into_bytes();
        bytes.truncate(97_778);
        bytes.push(0);
        bytes.extend_from_slice(b"tail\n");
        std::fs::write(dir.path().join("notes.md"), &bytes).unwrap();

        let collected = collect_files(
            dir.path(),
            &[],
            CollectOptions::default(),
            None,
            &HeldBytes::new(HELD_BYTES_BUDGET),
        )
        .unwrap();
        assert!(collected.files.contains_key("notes.md"));
        assert!(collected.skipped.is_empty());

        let (pending, _) =
            build_push_entries(dir.path(), &collected.files, &HashMap::new(), false, None).unwrap();
        let filled = filled_entries(dir.path(), &collected.files, &pending);
        assert_eq!(filled.len(), 1);
        assert!(filled[0].get("content").is_none());
        let decoded = base64::engine::general_purpose::STANDARD
            .decode(filled[0]["contentBase64"].as_str().unwrap())
            .unwrap();
        assert_eq!(decoded, bytes);
        assert_eq!(
            pending[0].encoded_len,
            serde_json::to_vec(&filled[0]).unwrap().len(),
            "the pended length is the length the entry serialises to"
        );
    }

    #[test]
    fn a_text_entry_pends_its_escaped_length() {
        let dir = tempfile::tempdir().unwrap();
        let text = "quote \" back \\ tab \t line\n bell \u{7} caf\u{e9}\n";
        std::fs::write(dir.path().join("t.md"), text).unwrap();
        let collected = collect_files(
            dir.path(),
            &[],
            CollectOptions::default(),
            None,
            &HeldBytes::new(0),
        )
        .unwrap();
        let (pending, _) =
            build_push_entries(dir.path(), &collected.files, &HashMap::new(), false, None).unwrap();
        let filled = filled_entries(dir.path(), &collected.files, &pending);
        assert_eq!(filled[0]["content"].as_str(), Some(text));
        assert_eq!(
            pending[0].encoded_len,
            serde_json::to_vec(&filled[0]).unwrap().len()
        );
    }

    async fn recording_server() -> MockServer {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/v1/repos/alice/repo/tree"))
            .respond_with(
                ResponseTemplate::new(404).set_body_json(serde_json::json!({"error": "not_found"})),
            )
            .mount(&server)
            .await;
        Mock::given(method("PUT"))
            .and(path("/api/v1/repos/alice/repo/push"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "commitSha": "c0ffee0000000000000000000000000000000000",
                "version": 1,
                "filesChanged": 5,
                "created": true
            })))
            .mount(&server)
            .await;
        server
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_zero_budget_publication_sends_the_same_bodies() {
        let folder = tempfile::tempdir().unwrap();
        std::fs::write(folder.path().join("README.md"), "# readme\n").unwrap();
        std::fs::write(folder.path().join("latin1.txt"), b"caf\xe9\n").unwrap();
        std::fs::write(
            folder.path().join("image.png"),
            b"\x89PNG\r\n\x1a\n\x00\x00\x00\rIHDR",
        )
        .unwrap();
        for (name, seed) in [("big1.bin", 1u8), ("big2.bin", 2u8)] {
            let bytes: Vec<u8> = (0..10 * 1024 * 1024u32)
                .map(|i| (i as u8).wrapping_mul(31).wrapping_add(seed))
                .collect();
            std::fs::write(folder.path().join(name), bytes).unwrap();
        }
        std::fs::write(
            folder.path().join(".syns.yaml"),
            "owner: alice\nname: repo\n",
        )
        .unwrap();

        let mut bodies = Vec::new();
        for budget in [0, HELD_BYTES_BUDGET] {
            let server = recording_server().await;
            let cache = tempfile::tempdir().unwrap();
            let client = SynsClient::new(&server.uri()).unwrap();
            let mut opts = opts(cache.path());
            opts.held = Some(HeldBytes::new(budget));
            smart_push(&client, "t", "alice/repo", folder.path(), opts)
                .await
                .unwrap();
            let puts: Vec<Vec<u8>> = server
                .received_requests()
                .await
                .unwrap()
                .into_iter()
                .filter(|r| r.method == reqwest::Method::PUT)
                .map(|r| r.body)
                .collect();
            assert_eq!(puts.len(), 2, "budget {budget}: the PUT requests");
            bodies.push(puts);
        }
        assert!(bodies[0] == bodies[1], "the two runs' bodies differ");
    }

    /// SPEC u280 `fill_batch`, `D-094`: the body is allocated at its
    /// projected length and ends at it, equal byte for byte to
    /// `serde_json`'s serialisation of the same request, held bytes and
    /// bytes read into the reused buffer alike.
    #[test]
    fn a_batch_body_is_its_projected_serialisation() {
        let folder = tempfile::tempdir().unwrap();
        let text = "quote \" back \\ tab \t line\n bell \u{7} esc \u{1b} del \u{7f} caf\u{e9}\n";
        std::fs::write(folder.path().join("a-text.md"), text).unwrap();
        std::fs::write(folder.path().join("latin1.txt"), b"caf\xe9\n").unwrap();
        let big: Vec<u8> = (0..10 * 1024 * 1024u32)
            .map(|i| (i as u8).wrapping_mul(37).wrapping_add(1))
            .collect();
        std::fs::write(folder.path().join("m.bin"), &big).unwrap();
        let contents: HashMap<&str, &[u8]> = HashMap::from([
            ("a-text.md", text.as_bytes()),
            ("latin1.txt", &b"caf\xe9\n"[..]),
            ("m.bin", &big[..]),
        ]);
        let hash_only = |path: &str| PushFileEntry {
            path: path.into(),
            sha: blob_sha1(path.as_bytes()),
            content: None,
            content_base64: None,
        };
        let frame = PushRequest {
            files: vec![hash_only("b.md"), hash_only("z.md")],
            deletions: Some(vec![PushDeleteEntry {
                path: "gone.md".into(),
            }]),
            message: Some("a \"quoted\" message (part 1/2)".into()),
            author: Some("alice".into()),
            parent_sha: Some("p".repeat(40)),
            description: None,
            tags: Some(vec!["t".into()]),
            status: None,
            visibility: None,
            provenance: Some(PushProvenance {
                integration: "codex".into(),
                run: "r".into(),
                trigger: "manual".into(),
                task_ref: None,
            }),
        };

        for budget in [HELD_BYTES_BUDGET, 0] {
            let collected = collect_files(
                folder.path(),
                &[],
                CollectOptions::default(),
                None,
                &HeldBytes::new(budget),
            )
            .unwrap();
            let held = collected
                .files
                .values()
                .filter(|f| f.bytes.is_some())
                .count();
            assert_eq!(held, if budget == 0 { 0 } else { 3 }, "budget {budget}");
            let (pending, _) = build_push_entries(
                folder.path(),
                &collected.files,
                &HashMap::new(),
                false,
                None,
            )
            .unwrap();
            let mut scratch = scratch_for(&collected.files, &pending);
            let scratch_capacity = scratch.capacity();
            assert_eq!(
                scratch_capacity,
                if budget == 0 { big.len() } else { 0 },
                "budget {budget}: the one buffer a not-held file is read into"
            );

            let body = fill_batch(
                folder.path(),
                &collected.files,
                &frame,
                &pending,
                &mut scratch,
            )
            .unwrap();

            assert_eq!(
                body.len(),
                projected_body_bytes(&frame, &pending),
                "budget {budget}"
            );
            assert_eq!(
                body.capacity(),
                body.len(),
                "budget {budget}: allocated at its length"
            );
            assert_eq!(scratch.capacity(), scratch_capacity, "budget {budget}");
            let mut expected = PushRequest {
                files: frame.files.clone(),
                ..request_frame_of(&frame)
            };
            for entry in &pending {
                let bytes = contents[entry.path.as_str()];
                expected.files.push(PushFileEntry {
                    path: entry.path.clone(),
                    sha: blob_sha1(bytes),
                    content: entry
                        .text
                        .then(|| String::from_utf8(bytes.to_vec()).unwrap()),
                    content_base64: (!entry.text)
                        .then(|| base64::engine::general_purpose::STANDARD.encode(bytes)),
                });
            }
            expected.files.sort_by(|a, b| a.path.cmp(&b.path));
            assert!(
                body == serde_json::to_vec(&expected).unwrap(),
                "budget {budget}: the body is not the request's serialisation"
            );
        }
    }

    /// SPEC u280 `smart_push` 5: a `MISSING_BLOBS` naming no hash-only
    /// path is returned as it stands, after the one `PUT`.
    #[tokio::test]
    async fn a_missing_blobs_naming_no_hash_only_path_ends_the_run() {
        let server = MockServer::start().await;
        Mock::given(method("PUT"))
            .and(path("/api/v1/repos/alice/repo/push"))
            .respond_with(ResponseTemplate::new(409).set_body_json(serde_json::json!({
                "error": "missing_blobs",
                "missing": {"a.txt": blob_sha1(b"new-a"), "elsewhere.md": "0".repeat(40)},
            })))
            .mount(&server)
            .await;
        let folder = tempfile::tempdir().unwrap();
        std::fs::write(folder.path().join("a.txt"), "new-a").unwrap();
        std::fs::write(folder.path().join("b.txt"), "old-b").unwrap();
        std::fs::write(
            folder.path().join(".syns.yaml"),
            "owner: alice\nname: repo\n",
        )
        .unwrap();
        let cache = tempfile::tempdir().unwrap();
        let mut manifest = Manifest::default();
        manifest.update(
            "base-sha".into(),
            HashMap::from([
                ("a.txt".into(), blob_sha1(b"old-a")),
                ("b.txt".into(), blob_sha1(b"old-b")),
                (
                    ".syns.yaml".into(),
                    blob_sha1(b"owner: alice\nname: repo\n"),
                ),
            ]),
        );
        manifest.save(cache.path(), "alice", "repo").unwrap();
        let client = SynsClient::new(&server.uri()).unwrap();

        let err = smart_push(
            &client,
            "t",
            "alice/repo",
            folder.path(),
            opts(cache.path()),
        )
        .await
        .unwrap_err();

        match err {
            CliError::Api {
                status: Some(409),
                error,
                ..
            } => assert_eq!(error, "missing_blobs"),
            other => panic!("expected MISSING_BLOBS, got {other:?}"),
        }
        let puts = server
            .received_requests()
            .await
            .unwrap()
            .into_iter()
            .filter(|r| r.method == reqwest::Method::PUT)
            .count();
        assert_eq!(puts, 1);
    }

    fn put_count(requests: &[wiremock::Request]) -> usize {
        requests
            .iter()
            .filter(|r| r.method == reqwest::Method::PUT)
            .count()
    }

    /// CR3-1: a second `MISSING_BLOBS` ends the run, nothing further
    /// sent (SPEC u280 `smart_push` 5).
    #[tokio::test]
    async fn a_second_missing_blobs_ends_the_run() {
        let server = MockServer::start().await;
        Mock::given(method("PUT"))
            .and(path("/api/v1/repos/alice/repo/push"))
            .respond_with(ResponseTemplate::new(409).set_body_json(serde_json::json!({
                "error": "missing_blobs",
                "missing": {"a.txt": blob_sha1(b"a")},
            })))
            .with_priority(1)
            .up_to_n_times(1)
            .mount(&server)
            .await;
        Mock::given(method("PUT"))
            .and(path("/api/v1/repos/alice/repo/push"))
            .respond_with(ResponseTemplate::new(409).set_body_json(serde_json::json!({
                "error": "missing_blobs",
                "missing": {"b.txt": blob_sha1(b"b")},
            })))
            .with_priority(2)
            .mount(&server)
            .await;
        let folder = tempfile::tempdir().unwrap();
        let identity = b"owner: alice\nname: repo\n";
        std::fs::write(folder.path().join(".syns.yaml"), identity).unwrap();
        std::fs::write(folder.path().join("a.txt"), "a").unwrap();
        std::fs::write(folder.path().join("b.txt"), "b").unwrap();
        std::fs::write(folder.path().join("c.txt"), "c edited").unwrap();
        let cache = tempfile::tempdir().unwrap();
        let mut manifest = Manifest::default();
        manifest.update(
            "base-sha".into(),
            HashMap::from([
                (".syns.yaml".into(), blob_sha1(identity)),
                ("a.txt".into(), blob_sha1(b"a")),
                ("b.txt".into(), blob_sha1(b"b")),
                ("c.txt".into(), blob_sha1(b"c")),
            ]),
        );
        manifest.save(cache.path(), "alice", "repo").unwrap();
        let client = SynsClient::new(&server.uri()).unwrap();

        let err = smart_push(
            &client,
            "t",
            "alice/repo",
            folder.path(),
            opts(cache.path()),
        )
        .await
        .unwrap_err();

        match err {
            CliError::Api {
                status: Some(409),
                error,
                ..
            } => assert_eq!(error, "missing_blobs"),
            other => panic!("expected MISSING_BLOBS, got {other:?}"),
        }
        assert_eq!(put_count(&server.received_requests().await.unwrap()), 2);
    }

    /// CR3-2: the first batch's budget counts the hash-only entries its
    /// frame carries, so two entries each under half the budget that fit
    /// together beside a bare frame are packed apart beside 20,000 of
    /// them (SPEC u280 `smart_push` 4).
    #[test]
    fn the_first_batch_counts_its_hash_only_entries() {
        let mut frame = bare_frame();
        frame.files = (0..20_000)
            .map(|i| PushFileEntry {
                path: format!("docs/{i:05}.md"),
                sha: "0".repeat(40),
                content: None,
                content_base64: None,
            })
            .collect();
        let pending: Vec<PendingEntry> = ["a.bin", "b.bin"]
            .into_iter()
            .map(|path| PendingEntry {
                path: path.into(),
                sha: "1".repeat(40),
                text: false,
                size: 0,
                encoded_len: CHUNK_BUDGET_BYTES / 2 - 4096,
            })
            .collect();

        let beside_none = pack_batches(pending.clone(), batch_overheads(&bare_frame()));
        let beside_many = pack_batches(pending, batch_overheads(&frame));

        assert_eq!(beside_none.len(), 1);
        assert_eq!(beside_many.len(), 2);
        assert!(beside_many.iter().all(|batch| batch.len() == 1));
    }

    /// CR3-3: a refused first batch reports every entry its body carried,
    /// the hash-only ones its frame names included.
    #[tokio::test]
    async fn a_refused_first_batch_counts_its_hash_only_entries() {
        let server = MockServer::start().await;
        Mock::given(method("PUT"))
            .and(path("/api/v1/repos/alice/repo/push"))
            .respond_with(
                ResponseTemplate::new(413)
                    .set_body_json(serde_json::json!({"error": "payload_too_large"})),
            )
            .mount(&server)
            .await;
        let folder = tempfile::tempdir().unwrap();
        let identity = b"owner: alice\nname: repo\n";
        std::fs::write(folder.path().join(".syns.yaml"), identity).unwrap();
        std::fs::write(folder.path().join("n1.md"), "one").unwrap();
        std::fs::write(folder.path().join("n2.md"), "two").unwrap();
        std::fs::write(folder.path().join("big1.txt"), vec![b'a'; 14 * 1024 * 1024]).unwrap();
        std::fs::write(folder.path().join("big2.txt"), vec![b'b'; 14 * 1024 * 1024]).unwrap();
        let cache = tempfile::tempdir().unwrap();
        let mut manifest = Manifest::default();
        manifest.update(
            "base-sha".into(),
            HashMap::from([
                (".syns.yaml".into(), blob_sha1(identity)),
                ("n1.md".into(), blob_sha1(b"one")),
                ("n2.md".into(), blob_sha1(b"two")),
                ("big1.txt".into(), blob_sha1(b"old")),
                ("big2.txt".into(), blob_sha1(b"old")),
            ]),
        );
        manifest.save(cache.path(), "alice", "repo").unwrap();
        let client = SynsClient::new(&server.uri()).unwrap();

        let err = smart_push(
            &client,
            "t",
            "alice/repo",
            folder.path(),
            opts(cache.path()),
        )
        .await
        .unwrap_err();

        match err {
            CliError::PayloadTooLarge { file_count, .. } => assert_eq!(file_count, 4),
            other => panic!("expected PAYLOAD_TOO_LARGE, got {other:?}"),
        }
        assert_eq!(put_count(&server.received_requests().await.unwrap()), 1);
    }
}
