//! A convergence's reads through `/api/v2` (roadmap 279, D-243): the head
//! from a view of the base commit and `changes`, or from the closure; the
//! holder's `.synsignore` from the base's view; and the blobs from the
//! store where this run named them, else in chunks of `POST …/read`.
//!
//! Every function answers `None` where the run reads v1: the client has
//! no v2 reads, or a v2 `404` sent the run there (D-243 §7).

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex, PoisonError};

use super::{
    Blob, Head, Staging, base_of, check_server_paths, is_not_found, read_one, repo_id,
    served_under, stage_bytes,
};
use crate::client::SynsClient;
use crate::errors::CliError;
use crate::push::collector::{HeldBytes, MAX_FILE_BYTES};
use crate::push::hash::blob_sha1;
use crate::push::working_copy::{WorkingCopy, folder_base, holds_in_root_home};
use crate::read::batch::{Ask, FILE_ALONE_BYTES, Frame, IN_FLIGHT, Sink, read_chunks};
use crate::read::run::{RunReads, ViewHead, now_secs};
use crate::read::store::{Store, is_blob_hash};
use crate::read::v2::{TreeAnswer, changes, closure, head, is_absent};
use crate::read::view::{Entry, Ignore, MODE_SUBMODULE, View, scope_key, tag_parts, unmasked_tag};
use crate::repo::folder::{FolderScope, lies_under};

/// An unmasked view is refreshed by the closure this long after the
/// closure it descends from (D-243 §1).
pub(crate) const VIEW_REFRESH_SECS: u64 = 24 * 3600;

/// The root `.synsignore`, as a holder path.
const HOLDER_IGNORE: &str = ".synsignore";

/// Where a copy's v2 reads go: the address, the view's scope, and the
/// folder its paths are counted from at a holder address.
struct Place {
    address: String,
    scope: String,
    /// A folder bound to its holder: the folder's recorded path.
    folder: Option<String>,
    /// A folder copy with no checkout, whose view records the holder's
    /// `.synsignore` (D-243 §10).
    records_ignore: bool,
}

fn place_of(copy: &WorkingCopy) -> Place {
    let records_ignore = copy
        .folder
        .as_ref()
        .is_some_and(|scope| scope.checkout.is_none());
    match served_under(copy) {
        Some(folder) => Place {
            address: repo_id(copy),
            scope: folder.to_string(),
            folder: Some(folder.to_string()),
            records_ignore,
        },
        None => Place {
            address: repo_id(copy),
            scope: String::new(),
            folder: None,
            records_ignore,
        },
    }
}

impl Place {
    /// A path counted from the scope, as the request at the address names it.
    fn remote(&self, path: &str) -> String {
        match &self.folder {
            Some(folder) => format!("{folder}/{path}"),
            None => path.to_string(),
        }
    }

    /// A served path counted from the scope, none where it lies outside.
    fn counted(&self, served: &str) -> Option<String> {
        match &self.folder {
            Some(folder) => {
                lies_under(served, folder).then(|| served[folder.len() + 1..].to_string())
            }
            None => Some(served.to_string()),
        }
    }
}

/// The head a view stands for: its files, sizes and commit.
fn head_from(view: &View, place: &Place) -> Head {
    let mut files = BTreeMap::new();
    let mut sizes = BTreeMap::new();
    for (path, entry) in &view.entries {
        if !entry.is_file() || holds_in_root_home(&place.remote(path)) {
            continue;
        }
        files.insert(path.clone(), entry.sha.clone());
        sizes.insert(path.clone(), entry.size);
    }
    Head {
        commit: Some(view.commit.clone()),
        files,
        sizes,
        truncated: false,
    }
}

/// Names every entry of `view` as read by this run.
fn name_all(reads: &RunReads, view: &View, place: &Place) {
    for (path, entry) in &view.entries {
        reads.name(&view.address, &place.remote(path), &entry.sha);
    }
}

/// The head through v2 (D-243 §1, §2, §5), none where the run reads v1.
/// `collected` is the folder's collection where the run holds one: a
/// changed path its file already holds is asked with `have`.
pub(super) async fn read_head_v2(
    client: &SynsClient,
    token: Option<&str>,
    copy: &WorkingCopy,
    collected: Option<&BTreeMap<String, String>>,
) -> Result<Option<Head>, CliError> {
    let Some(reads) = client.v2().cloned() else {
        return Ok(None);
    };
    let place = place_of(copy);
    let base = base_of(copy)
        .and_then(|b| b.commit_sha().map(String::from))
        .filter(|c| is_blob_hash(c));
    let store = reads.existing_store();
    let view = match (&store, &base) {
        (Some(store), Some(commit)) => View::load(store, &place.address, &place.scope, commit),
        _ => None,
    };
    reads.set_view_head(None);
    let fresh = |view: &View| {
        !view.masked && now_secs().saturating_sub(view.closure_time) <= VIEW_REFRESH_SECS
    };
    let read = match view {
        Some(view) if fresh(&view) => {
            match by_changes(client, &reads, token, &place, &view, collected).await {
                Ok(Some(head)) => Ok(head),
                Ok(None) => by_closure(client, &reads, token, &place, None).await,
                Err(err) => Err(err),
            }
        }
        Some(view) => by_closure(client, &reads, token, &place, Some(view)).await,
        None => by_closure(client, &reads, token, &place, None).await,
    };
    match read {
        Ok(view) => Ok(Some(head_from(&view, &place))),
        Err(err) if is_absent(&err) => {
            reads.go_v1();
            Ok(None)
        }
        Err(err) => Err(err),
    }
}

/// The head from the closure at `head`, `If-None-Match` set to `hint`'s
/// tag, or to the newest view the store holds for the place.
async fn by_closure(
    client: &SynsClient,
    reads: &Arc<RunReads>,
    token: Option<&str>,
    place: &Place,
    hint: Option<View>,
) -> Result<View, CliError> {
    let hint = match hint {
        Some(view) => Some(view),
        None => reads.existing_store().and_then(|store| {
            store
                .views_of(&place.address, &scope_key(&place.scope))
                .into_iter()
                .find_map(|commit| View::load(&store, &place.address, &place.scope, &commit))
        }),
    };
    let (path, root) = match &place.folder {
        Some(folder) => (folder.as_str(), folder.as_str()),
        None => ("", ""),
    };
    let answer = closure(
        client,
        Some(reads),
        &place.address,
        token,
        "head",
        path,
        root,
        hint.as_ref().map(|view| view.tag.as_str()),
    )
    .await?;
    let view = match (answer, hint) {
        (TreeAnswer::NotModified, Some(mut view)) => {
            // A `304` names the view's entries under the reader's view;
            // a refresh takes the new closure's time (D-243 §1, §4).
            if !view.masked {
                view.closure_time = now_secs();
                if let Some(store) = reads.store() {
                    view.save(&store);
                }
            }
            view
        }
        (TreeAnswer::NotModified, None) => {
            return Err(crate::client::undecodable(
                reqwest::StatusCode::NOT_MODIFIED,
                "the closure answered 304 to no tag",
            ));
        }
        (TreeAnswer::Closure { tag, closure }, _) => {
            let Some((commit, digest)) = tag_parts(&tag) else {
                return Err(crate::client::undecodable(
                    reqwest::StatusCode::OK,
                    "the closure's tag names no commit",
                ));
            };
            let view = View {
                address: place.address.clone(),
                scope: place.scope.clone(),
                commit: commit.clone(),
                root: Some(closure.root().to_string()),
                tag,
                closure_time: now_secs(),
                masked: digest != "-",
                ignore: if place.records_ignore {
                    reads.ignore_at(&place.address, &commit)
                } else {
                    Ignore::Unknown
                },
                entries: closure.entries(),
            };
            if let Some(store) = reads.store() {
                for (id, lines) in closure.stored_blocks() {
                    store.admit_tree(&id, &lines);
                }
                view.save(&store);
            }
            view
        }
    };
    name_all(reads, &view, place);
    Ok(view)
}

/// A path's state in a view under construction, its mode not yet known.
const MODE_PENDING: &str = "?";

/// The head from `view` and `changes` since its commit, none where the
/// answer refuses the base (`404`, `422`) or contradicts the view, which
/// is then dropped (D-243 §1, §10).
async fn by_changes(
    client: &SynsClient,
    reads: &Arc<RunReads>,
    token: Option<&str>,
    place: &Place,
    view: &View,
    collected: Option<&BTreeMap<String, String>>,
) -> Result<Option<View>, CliError> {
    let answer = match changes(
        client,
        Some(reads),
        &place.address,
        token,
        &view.commit,
        "head",
        None,
    )
    .await
    {
        Ok(answer) => answer,
        Err(CliError::Api {
            status: Some(404 | 422),
            ..
        }) => return Ok(None),
        Err(err) => return Err(err),
    };
    let store = reads.existing_store();
    let contradicted = || {
        if let Some(store) = &store {
            view.drop_from(store);
        }
        Ok(None)
    };
    if answer.from.commit != view.commit || !is_blob_hash(&answer.to.commit) {
        return contradicted();
    }

    let mut entries = view.entries.clone();
    let mut ignore = view.ignore.clone();
    let mut ignore_asked: Option<String> = None;
    let mut changed: BTreeMap<String, (String, Option<u64>)> = BTreeMap::new();
    let holder_bound = place.folder.is_some();

    // Each side of an entry, counted from the scope.
    for change in &answer.entries {
        let (old_served, new_served) = match change.status.as_str() {
            "added" => (None, Some(change.path.as_str())),
            "modified" => (Some(change.path.as_str()), Some(change.path.as_str())),
            "deleted" => (Some(change.path.as_str()), None),
            "renamed" => (
                Some(change.old_path.as_deref().unwrap_or(&change.path)),
                Some(change.path.as_str()),
            ),
            _ => return contradicted(),
        };
        // The holder's root `.synsignore`, against the view's record.
        if holder_bound {
            let old_ignore = old_served == Some(HOLDER_IGNORE);
            let new_ignore = new_served == Some(HOLDER_IGNORE);
            if old_ignore || new_ignore {
                let consistent = match (&ignore, old_ignore) {
                    (Ignore::Unknown, _) => true,
                    (Ignore::Hash(held), true) => change.old_sha.as_deref() == Some(held.as_str()),
                    (Ignore::None, false) => true,
                    _ => false,
                };
                ignore = match (consistent, new_ignore, &change.new_sha) {
                    (false, _, _) => Ignore::Unknown,
                    (true, true, Some(sha)) if is_blob_hash(sha) => {
                        ignore_asked = Some(sha.clone());
                        Ignore::Hash(sha.clone())
                    }
                    (true, true, _) => Ignore::Unknown,
                    (true, false, _) => Ignore::None,
                };
            }
        }
        let old = old_served.and_then(|p| place.counted(p));
        let new = new_served.and_then(|p| place.counted(p));
        let same_path = old.is_some() && old == new;
        if let Some(old) = &old {
            // A path the view lacks, or holds at another hash.
            match entries.get(old) {
                Some(entry) if change.old_sha.as_deref() == Some(entry.sha.as_str()) => {}
                _ => return contradicted(),
            }
            if !same_path {
                entries.remove(old);
            }
        }
        if let Some(new) = new {
            if !same_path && entries.contains_key(&new) {
                return contradicted();
            }
            let Some(sha) = change.new_sha.clone().filter(|s| is_blob_hash(s)) else {
                return contradicted();
            };
            entries.insert(
                new.clone(),
                Entry {
                    mode: MODE_PENDING.to_string(),
                    sha: sha.clone(),
                    size: change.size,
                },
            );
            changed.insert(new, (sha, change.size));
        }
    }

    // One batch of every path added, modified or renamed at `to`: its
    // frame settles the kind and mode (D-243 §2), and its bytes go to the
    // store for the blob reads that follow.
    let to = answer.to.commit.clone();
    let mut asks: Vec<Ask> = Vec::new();
    let mut keys: Vec<Option<String>> = Vec::new();
    let store_now = reads.store();
    for (path, (sha, size)) in &changed {
        let held = store_now
            .as_ref()
            .is_some_and(|s| s.verified(sha).is_some())
            || collected.is_some_and(|c| c.get(path) == Some(sha));
        asks.push(Ask {
            path: place.remote(path),
            have: held.then(|| sha.clone()),
            expect: Some(sha.clone()),
            size: *size,
        });
        keys.push(Some(path.clone()));
    }
    if let Some(sha) = &ignore_asked {
        let held = store_now
            .as_ref()
            .is_some_and(|s| s.verified(sha).is_some());
        asks.push(Ask {
            path: HOLDER_IGNORE.to_string(),
            have: held.then(|| sha.clone()),
            expect: Some(sha.clone()),
            size: None,
        });
        keys.push(None);
    }
    if !asks.is_empty() {
        let settled: Mutex<Vec<Option<String>>> = Mutex::new(vec![None; asks.len()]);
        let sink = Settle {
            store: store_now.as_deref(),
            paths: asks.iter().map(|ask| ask.path.clone()).collect(),
            settled: &settled,
        };
        read_chunks(
            client,
            Some(reads),
            &place.address,
            token,
            &to,
            &asks,
            IN_FLIGHT,
            &sink,
        )
        .await?;
        let settled = settled.into_inner().unwrap_or_else(PoisonError::into_inner);
        for ((ask, key), mode) in asks.iter().zip(&keys).zip(settled) {
            match key {
                Some(path) => {
                    if let Some(entry) = entries.get_mut(path) {
                        // `not_found`: a submodule, or a path hidden since;
                        // left out of the head either way (D-243 §2).
                        entry.mode = mode.unwrap_or_else(|| MODE_SUBMODULE.to_string());
                        if entry.mode == MODE_SUBMODULE {
                            entry.size = None;
                        }
                    }
                    reads.name(
                        &place.address,
                        &ask.path,
                        ask.expect.as_deref().unwrap_or(""),
                    );
                }
                None if mode.is_none() => ignore = Ignore::None,
                None => {}
            }
        }
    }

    let derived = View {
        address: place.address.clone(),
        scope: place.scope.clone(),
        commit: to.clone(),
        root: if to == view.commit {
            view.root.clone()
        } else {
            None
        },
        // At the base's own commit the view keeps the tag it was read
        // under; past it, the tag 276 computes for it (D-243 §1).
        tag: if to == view.commit {
            view.tag.clone()
        } else {
            unmasked_tag(&to, &place.scope)
        },
        closure_time: view.closure_time,
        masked: false,
        ignore: if !place.records_ignore {
            Ignore::Unknown
        } else if holder_bound {
            ignore
        } else if to == view.commit {
            view.ignore.clone()
        } else {
            Ignore::Unknown
        },
        entries,
    };
    if (derived.commit != view.commit || derived != *view)
        && let Some(store) = &store_now
    {
        derived.save(store);
    }
    reads.set_view_head(Some(ViewHead {
        address: place.address.clone(),
        scope: place.scope.clone(),
        commit: view.commit.clone(),
        derived: to.clone(),
    }));
    Ok(Some(derived))
}

/// The head read's batch: each frame's bytes admitted, its mode kept.
struct Settle<'a> {
    store: Option<&'a Store>,
    paths: Vec<String>,
    settled: &'a Mutex<Vec<Option<String>>>,
}

impl Sink for Settle<'_> {
    fn frame(&self, index: usize, frame: Frame) -> Result<(), CliError> {
        let mode = match frame {
            Frame::Ok { sha, mode, bytes } => {
                if let Some(store) = self.store {
                    store.admit_many(&sha, &bytes);
                }
                Some(mode)
            }
            Frame::Unchanged { mode, .. } => Some(mode),
            Frame::TooLarge { .. } => Some(crate::read::view::MODE_FILE.to_string()),
            Frame::NotFound => None,
            Frame::Mismatch { expected, actual } => {
                return Err(crate::client::hash_mismatch(
                    &self.paths[index],
                    &expected,
                    &actual,
                ));
            }
        };
        self.settled.lock().unwrap_or_else(PoisonError::into_inner)[index] = mode;
        Ok(())
    }
}

/// Drops the view the run's head was read from and the one it derived,
/// so the next head read asks the closure (D-243 §2).
pub(super) fn drop_view_head(client: &SynsClient) {
    let Some(reads) = client.v2() else {
        return;
    };
    let (Some(head), Some(store)) = (reads.view_head(), reads.existing_store()) else {
        return;
    };
    store.remove_view(&head.address, &scope_key(&head.scope), &head.commit);
    store.remove_view(&head.address, &scope_key(&head.scope), &head.derived);
    reads.set_view_head(None);
}

/// Whether the client's run asked its head read again (D-243 §2).
pub(super) fn take_restart(client: &SynsClient) -> bool {
    client.v2().is_some_and(|reads| reads.take_restart())
}

// ---- the holder's `.synsignore` (D-243 §10) ----------------------------

/// The holder's root `.synsignore` for a folder copy with no checkout,
/// from the base commit's view where it records one, else read and
/// recorded there; none where the run reads v1.
pub(super) async fn holder_synsignore_v2(
    client: &SynsClient,
    token: Option<&str>,
    copy: &WorkingCopy,
    scope: &FolderScope,
) -> Result<Option<Option<Vec<u8>>>, CliError> {
    let Some(reads) = client.v2().cloned() else {
        return Ok(None);
    };
    let place = place_of(copy);
    let based = folder_base(copy.stores(), scope)
        .and_then(|b| b.commit_sha().map(String::from))
        .filter(|c| is_blob_hash(c));
    let at = match &based {
        Some(commit) => {
            if let Some(store) = reads.existing_store()
                && let Some(view) = View::load(&store, &place.address, &place.scope, commit)
            {
                match &view.ignore {
                    Ignore::Hash(sha) => {
                        if let Some(bytes) = store.verified(sha) {
                            reads.set_ignore(&place.address, commit, view.ignore.clone());
                            return Ok(Some(Some(bytes)));
                        }
                    }
                    Ignore::None => {
                        reads.set_ignore(&place.address, commit, Ignore::None);
                        return Ok(Some(None));
                    }
                    Ignore::Unknown => {}
                }
            }
            Some(commit.clone())
        }
        // No base: the head's commit, from one v2 head at the address.
        None => match head(client, &place.address, token, None).await {
            Ok(crate::read::v2::HeadAnswer::Head { commit, .. }) => commit,
            Ok(crate::read::v2::HeadAnswer::NotModified) => None,
            Err(err) if is_absent(&err) => {
                reads.go_v1();
                return Ok(None);
            }
            Err(err) => return Err(err),
        },
    };
    let through_identity = scope.identity.is_some();
    let read = client
        .get_raw(&scope.holder(), token, HOLDER_IGNORE, at.as_deref(), None)
        .await;
    let (bytes, found) = match read {
        Ok(raw) => {
            let sha = blob_sha1(&raw.bytes);
            let found = if raw.etag.as_deref() == Some(sha.as_str()) {
                if let Some(store) = reads.store() {
                    store.admit(&sha, &raw.bytes);
                }
                Some(Ignore::Hash(sha))
            } else {
                None
            };
            (Some(raw.bytes), found)
        }
        Err(CliError::Api {
            status: Some(404),
            ref error,
            ..
        }) if error == "not_found" || error == "repo_not_found" => (None, Some(Ignore::None)),
        // Through an identity every refusal reads as none for the run,
        // and is remembered nowhere (SPEC u302, D-243 §10).
        Err(_) if through_identity => (None, None),
        Err(err) => return Err(err),
    };
    if let (Some(found), Some(commit)) = (found, at) {
        reads.set_ignore(&place.address, &commit, found.clone());
        if let Some(store) = reads.store()
            && let Some(mut view) = View::load(&store, &place.address, &place.scope, &commit)
            && view.ignore != found
        {
            view.ignore = found;
            view.save(&store);
        }
    }
    Ok(Some(bytes))
}

// ---- the blobs (D-243 §3, §4) -------------------------------------------

/// What a blob read wants of each path's frame.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Want {
    /// The head's bytes: a frame at another hash is refused.
    Head,
    /// A base content: a path the commit does not hold at the hash the
    /// base names is unheld, not refused (SPEC u304 Q-01).
    Base,
}

/// The blob reads' sink: each frame admitted to the store and held or
/// staged for the run.
struct Blobs<'a> {
    reads: &'a RunReads,
    address: &'a str,
    store: Option<&'a Store>,
    asks: &'a [Ask],
    keys: &'a [String],
    held: &'a Arc<HeldBytes>,
    staging: &'a Staging,
    want: Want,
    out: Mutex<BTreeMap<String, Blob>>,
    /// Asks answered `not_found`, at another hash, `too_large` twice, or
    /// `unchanged` for a blob the store no longer holds.
    missing: Mutex<Vec<usize>>,
    alone: Mutex<Vec<usize>>,
}

impl Blobs<'_> {
    fn keep(&self, index: usize, bytes: Vec<u8>) -> Result<(), CliError> {
        let path = &self.asks[index].path;
        let blob = match self.held.try_hold(bytes.len() as u64) {
            Some(hold) => Blob::Held(bytes, hold),
            None => Blob::Staged(stage_bytes(self.staging, path, &bytes)?),
        };
        self.out
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(self.keys[index].clone(), blob);
        Ok(())
    }
}

impl Sink for Blobs<'_> {
    fn frame(&self, index: usize, frame: Frame) -> Result<(), CliError> {
        let ask = &self.asks[index];
        match frame {
            Frame::Ok { sha, bytes, .. } => {
                if self.want == Want::Base && ask.expect.as_deref() != Some(sha.as_str()) {
                    self.missing
                        .lock()
                        .unwrap_or_else(PoisonError::into_inner)
                        .push(index);
                    return Ok(());
                }
                if let Some(store) = self.store {
                    store.admit_many(&sha, &bytes);
                }
                self.reads.name(self.address, &ask.path, &sha);
                self.keep(index, bytes)
            }
            Frame::Unchanged { sha, .. } => {
                match self.store.and_then(|store| store.verified(&sha)) {
                    // The hash the head named, whatever the read wants
                    // (REVIEW-syns-cli I3).
                    Some(bytes) if ask.expect.as_deref() == Some(sha.as_str()) => {
                        self.reads.name(self.address, &ask.path, &sha);
                        self.keep(index, bytes)
                    }
                    _ => {
                        self.alone
                            .lock()
                            .unwrap_or_else(PoisonError::into_inner)
                            .push(index);
                        Ok(())
                    }
                }
            }
            Frame::NotFound => {
                self.missing
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .push(index);
                Ok(())
            }
            Frame::TooLarge { .. } => {
                self.alone
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .push(index);
                Ok(())
            }
            Frame::Mismatch { expected, actual } => {
                Err(crate::client::hash_mismatch(&ask.path, &expected, &actual))
            }
        }
    }
}

/// Whether `err` is the mismatch refusal naming `path`.
fn names_mismatch(err: &CliError, path: &str) -> bool {
    matches!(
        err,
        CliError::Api { status: Some(200), error, context: None }
            if error.starts_with(&format!("invalid response body: {path}: expected "))
    )
}

/// The raw `404` a path the server no longer serves ends the run with.
fn not_found() -> CliError {
    CliError::Api {
        status: Some(404),
        error: "not_found".to_string(),
        context: None,
    }
}

/// Each wanted path's content at the commit `at`: from the store where
/// this run named that hash for the path, else in chunks (D-243 §3, §4);
/// none where the run reads v1. With `base`, the paths the commit does not
/// hold at the hash wanted are answered apart instead of refused.
#[allow(clippy::too_many_arguments)]
pub(super) async fn read_blobs_v2(
    client: &SynsClient,
    token: Option<&str>,
    repo_id: &str,
    folder: Option<&str>,
    at: &str,
    wanted: &BTreeMap<String, (String, Option<u64>)>,
    held: &Arc<HeldBytes>,
    staging: &Staging,
    base: bool,
) -> Result<Option<(BTreeMap<String, Blob>, Vec<String>)>, CliError> {
    let Some(reads) = client.v2().cloned() else {
        return Ok(None);
    };
    if !is_blob_hash(at) {
        return Ok(None);
    }
    check_server_paths(wanted.keys())?;
    let store = reads.store();
    let remote = |path: &str| match folder {
        Some(folder) => format!("{folder}/{path}"),
        None => path.to_string(),
    };

    let mut out: BTreeMap<String, Blob> = BTreeMap::new();
    let mut asks: Vec<Ask> = Vec::new();
    let mut keys: Vec<String> = Vec::new();
    let mut alone: Vec<(String, String, String, Option<u64>)> = Vec::new();
    for (path, (hash, size)) in wanted {
        let sent = remote(path);
        let stored = store.as_ref().and_then(|s| s.verified(hash));
        if !base
            && reads.named(repo_id, &sent).as_deref() == Some(hash.as_str())
            && let Some(bytes) = stored
        {
            let blob = match held.try_hold(bytes.len() as u64) {
                Some(hold) => Blob::Held(bytes, hold),
                None => Blob::Staged(stage_bytes(staging, &sent, &bytes)?),
            };
            out.insert(path.clone(), blob);
            continue;
        }
        if !base && size.is_some_and(|s| s > FILE_ALONE_BYTES) {
            alone.push((path.clone(), sent, hash.clone(), *size));
            continue;
        }
        asks.push(Ask {
            path: sent,
            have: stored.is_some().then(|| hash.clone()),
            expect: Some(hash.clone()),
            size: *size,
        });
        keys.push(path.clone());
    }
    // A base read checks the hash in the sink, so a frame at another hash
    // answers the path unheld rather than refused.
    let checked: Vec<Ask> = asks
        .iter()
        .map(|ask| Ask {
            expect: if base { None } else { ask.expect.clone() },
            ..ask.clone()
        })
        .collect();

    let mut unheld: Vec<String> = Vec::new();
    if !asks.is_empty() {
        let sink = Blobs {
            reads: &reads,
            address: repo_id,
            store: store.as_deref(),
            asks: &asks,
            keys: &keys,
            held,
            staging,
            want: if base { Want::Base } else { Want::Head },
            out: Mutex::new(BTreeMap::new()),
            missing: Mutex::new(Vec::new()),
            alone: Mutex::new(Vec::new()),
        };
        match read_chunks(
            client,
            Some(&reads),
            repo_id,
            token,
            at,
            &checked,
            IN_FLIGHT,
            &sink,
        )
        .await
        {
            Ok(_) => {}
            Err(err) if is_absent(&err) => {
                reads.go_v1();
                return Ok(None);
            }
            Err(err) => return Err(err),
        }
        out.extend(
            sink.out
                .into_inner()
                .unwrap_or_else(PoisonError::into_inner),
        );
        let missing = sink
            .missing
            .into_inner()
            .unwrap_or_else(PoisonError::into_inner);
        if base {
            unheld.extend(missing.iter().map(|i| keys[*i].clone()));
        } else if let Some(&first) = missing.first() {
            // A path the view carried, named by no request of this run:
            // the head is read again from the closure (D-243 §2). One this
            // run's own read named was hidden mid-run: today's `404`.
            let carried = missing
                .iter()
                .any(|i| reads.named(repo_id, &asks[*i].path).is_none());
            if carried && reads.view_head().is_some() {
                reads.ask_restart();
            } else if let Some(store) = &store {
                store.remove_view(repo_id, &scope_key(folder.unwrap_or("")), at);
            }
            let _ = first;
            return Err(not_found());
        }
        for i in sink
            .alone
            .into_inner()
            .unwrap_or_else(PoisonError::into_inner)
        {
            let ask = &asks[i];
            let hash = ask.expect.clone().unwrap_or_default();
            alone.push((keys[i].clone(), ask.path.clone(), hash, ask.size));
        }
    }

    // Read alone through `EP-file-read-bytes` at the commit, checked
    // against the head's hash, as today (D-243 §3).
    for (path, sent, hash, size) in alone {
        let counted = size.unwrap_or(MAX_FILE_BYTES);
        let hold = held.try_hold(counted);
        let dest = hold.is_none().then(|| staging.next_path());
        let read = read_one(
            client, token, repo_id, at, &sent, &hash, hold, counted, dest, held,
        )
        .await;
        let blob = match read {
            Ok(blob) => blob,
            Err(err) if base && (is_not_found(&err) || names_mismatch(&err, &sent)) => {
                unheld.push(path);
                continue;
            }
            Err(err) => return Err(err),
        };
        if let Some(store) = &store {
            match &blob {
                Blob::Held(bytes, _) => store.admit_many(&hash, bytes),
                Blob::Staged(file) => store.admit_file(&hash, file),
            }
        }
        out.insert(path, blob);
    }
    Ok(Some((out, unheld)))
}

// ---- the snapshot pull (D-243 §7) ---------------------------------------

/// Whether `version` is a spelling v2's `at` takes: digits, or 40
/// lowercase hex.
fn v2_spelling(version: &str) -> bool {
    is_blob_hash(version)
        || !version.is_empty() && version.bytes().all(|b| b.is_ascii_digit()) && version != "0"
}

/// The files of the closure at `version` for a snapshot pull, counted
/// from `folder` where given, and the commit its tag names; none where
/// the run reads v1, or `version` is spelt otherwise (D-243 §7).
pub(crate) async fn snapshot_head(
    client: &SynsClient,
    token: Option<&str>,
    address: &str,
    folder: Option<&str>,
    version: &str,
) -> Result<Option<(String, Vec<(String, Option<String>, Option<u64>)>)>, CliError> {
    let Some(reads) = client.v2().cloned() else {
        return Ok(None);
    };
    if !v2_spelling(version) {
        return Ok(None);
    }
    let place = Place {
        address: address.to_string(),
        scope: folder.unwrap_or("").to_string(),
        folder: folder.map(str::to_string),
        records_ignore: false,
    };
    let path = folder.unwrap_or("");
    let answer = match closure(
        client,
        Some(&reads),
        address,
        token,
        version,
        path,
        path,
        None,
    )
    .await
    {
        Ok(answer) => answer,
        Err(err) if is_absent(&err) => {
            reads.go_v1();
            return Ok(None);
        }
        Err(err) => return Err(err),
    };
    let TreeAnswer::Closure { tag, closure } = answer else {
        return Err(crate::client::undecodable(
            reqwest::StatusCode::NOT_MODIFIED,
            "the closure answered 304 to no tag",
        ));
    };
    let Some((commit, digest)) = tag_parts(&tag) else {
        return Err(crate::client::undecodable(
            reqwest::StatusCode::OK,
            "the closure's tag names no commit",
        ));
    };
    let view = View {
        address: address.to_string(),
        scope: place.scope.clone(),
        commit: commit.clone(),
        root: Some(closure.root().to_string()),
        tag,
        closure_time: now_secs(),
        masked: digest != "-",
        ignore: Ignore::Unknown,
        entries: closure.entries(),
    };
    if let Some(store) = reads.store() {
        for (id, lines) in closure.stored_blocks() {
            store.admit_tree(&id, &lines);
        }
        view.save(&store);
    }
    name_all(&reads, &view, &place);
    let files = view
        .entries
        .iter()
        .filter(|(_, entry)| entry.is_file())
        .map(|(path, entry)| (path.clone(), Some(entry.sha.clone()), entry.size))
        .collect();
    Ok(Some((commit, files)))
}
