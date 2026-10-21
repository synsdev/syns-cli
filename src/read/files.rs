//! Reading paths through the store and the raw entry (roadmap 278).
//!
//! `read_path` reads one path: the blob the store holds for it is read
//! and verified first, then the raw entry is asked once with
//! `If-None-Match` naming that blob, and a `304` is answered from the
//! verified bytes; a `200` is checked against its `ETag` and admitted.
//! `read_many` reads the distinct paths of one run, at most `IN_FLIGHT` at
//! once on the run's one client, each through `resend_read`, and — under
//! `--max-bytes` — decides in the order asked, from each answer's declared
//! length, which paths are deferred, reading no body byte of one (D-242
//! §6, § Amended §6).

use std::sync::{Arc, Mutex, PoisonError};

use tokio::sync::watch;
use tokio::task::{AbortHandle, JoinSet};

use crate::client::{RawHead, SynsClient, hash_mismatch, resend_read};
use crate::errors::CliError;
use crate::push::collector::is_text;
use crate::push::hash::blob_sha1;
use crate::read::store::{Held, MediaHint, Store};
use crate::read::{IN_FLIGHT, ReadTarget, path_not_found_refusal};

/// How a run reads one path.
#[derive(Clone, Copy, Debug, Default)]
pub struct Policy {
    /// Ask `Accept-Encoding: identity`, so the length stands declared
    /// (`--max-bytes`).
    pub identity: bool,
    /// The answer writes the media type (one path under `--json`): a
    /// stored blob that is not text, whose hint names the type unknown,
    /// is asked with no `If-None-Match` (D-242 Amended §4).
    pub want_type: bool,
}

/// One path's bytes, as the store or the raw entry gave them.
#[derive(Debug)]
pub struct Answer {
    pub bytes: Vec<u8>,
    /// The blob hash of `bytes`.
    pub sha: String,
    /// The type the raw answer named, or the hint recorded on a `304`.
    pub media_type: Option<String>,
    /// Answered `304`, from the store.
    pub not_modified: bool,
}

/// What reading one path came to.
#[derive(Debug)]
pub enum Outcome {
    Read(Answer),
    /// Past the `--max-bytes` cut: no body byte of it was read.
    Deferred,
}

/// What one raw send came to, inside `resend_read`.
enum Got {
    NotModified(Option<String>),
    Body {
        bytes: Vec<u8>,
        etag: Option<String>,
        media_type: Option<String>,
    },
    Deferred,
    /// A refusal already counted 0 by the budget, never sent again.
    Refused(CliError),
}

/// Reads `mapped` at the target's pinned reference through the store
/// (D-242 §4): the blob a hint names verified before the request, the
/// request sent once with `If-None-Match` for a verified blob, a `304`
/// answered from those bytes, and a `200` whose bytes are not the ones its
/// `ETag` names refused as `hash_mismatch` naming `typed`. A `304`
/// naming another hash than the one sent is no answer: the path is asked
/// again with no header.
pub async fn read_path(
    client: &SynsClient,
    target: &ReadTarget,
    store: Option<&Store>,
    mapped: &str,
    typed: &str,
    policy: Policy,
    gate: Option<&Gate>,
) -> Result<Outcome, CliError> {
    let mut held: Option<Held> = store
        .and_then(|store| store.held(&target.repo_id, mapped))
        .filter(|held| {
            !(policy.want_type && held.media == MediaHint::Unknown && !is_text(&held.bytes))
        });
    let version_ref = target.version_ref();
    loop {
        let sent = held.as_ref().map(|held| held.sha.clone());
        let got = resend_read(|| {
            let sent = sent.clone();
            let version_ref = &version_ref;
            async move {
                // Under the budget a refusal counts 0 at its status line,
                // before its body is read, and is not sent again: the
                // places after it may already be decided on that 0.
                let mut refused = false;
                let head = client
                    .raw_head(
                        &target.repo_id,
                        target.token.as_deref(),
                        mapped,
                        version_ref,
                        sent.as_deref(),
                        policy.identity,
                        || {
                            if let Some(gate) = gate {
                                refused = true;
                                gate.report(0);
                            }
                        },
                    )
                    .await;
                let head = match head {
                    Ok(head) => head,
                    Err(err) if refused => return Ok(Got::Refused(err)),
                    Err(err) => return Err(err),
                };
                match head {
                    RawHead::NotModified { etag } => Ok(Got::NotModified(etag)),
                    RawHead::Body {
                        response,
                        etag,
                        media_type,
                        declared,
                    } => {
                        if let (Some(gate), Some(length)) = (gate, declared) {
                            gate.report(length);
                            if !gate.go().await {
                                return Ok(Got::Deferred);
                            }
                        }
                        let bytes = RawHead::body(response, declared).await?;
                        Ok(Got::Body {
                            bytes,
                            etag,
                            media_type,
                        })
                    }
                }
            }
        })
        .await?;
        match got {
            Got::Deferred => return Ok(Outcome::Deferred),
            Got::Refused(err) => return Err(err),
            Got::NotModified(etag) => {
                let Some(stored) = held.take() else {
                    return Err(CliError::Api {
                        status: Some(304),
                        error: "unexpected status 304".to_string(),
                        context: None,
                    });
                };
                if etag.as_deref().is_none_or(|etag| etag == stored.sha) {
                    return Ok(Outcome::Read(Answer {
                        media_type: stored.media.media_type(),
                        bytes: stored.bytes,
                        sha: stored.sha,
                        not_modified: true,
                    }));
                }
            }
            Got::Body {
                bytes,
                etag,
                media_type,
            } => {
                let sha = blob_sha1(&bytes);
                if let Some(etag) = &etag
                    && etag != &sha
                {
                    return Err(hash_mismatch(typed, etag, &sha));
                }
                if let Some(store) = store
                    && etag.is_some()
                {
                    store.admit(&sha, &bytes);
                    store.note_answer(
                        &target.repo_id,
                        mapped,
                        &sha,
                        MediaHint::from_answer(media_type.as_deref()),
                    );
                }
                return Ok(Outcome::Read(Answer {
                    bytes,
                    sha,
                    media_type,
                    not_modified: false,
                }));
            }
        }
    }
}

// ---- the --max-bytes budget ------------------------------------------------

/// How far the budget has decided the entries, in the order asked:
/// `decided` entries stand in `files`, and from `cut` on every entry is
/// deferred.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Progress {
    pub decided: usize,
    pub cut: Option<usize>,
}

/// The budget's ledger: each entry's distinct path (none for a path
/// refused before any request, which counts 0), each distinct path's size
/// once known, and the running sum.
#[derive(Debug)]
struct Ledger {
    budget: u64,
    entries: Vec<Option<usize>>,
    sizes: Vec<Option<u64>>,
    sum: u64,
    progress: Progress,
    tx: watch::Sender<Progress>,
}

impl Ledger {
    /// Records a path's size — the first report stands — and decides as
    /// far as the known sizes reach.
    fn report(&mut self, distinct: usize, size: u64) {
        if self.sizes[distinct].is_none() {
            self.sizes[distinct] = Some(size);
        }
        self.advance();
    }

    fn advance(&mut self) {
        while self.progress.cut.is_none() && self.progress.decided < self.entries.len() {
            let size = match self.entries[self.progress.decided] {
                None => 0,
                Some(distinct) => match self.sizes[distinct] {
                    Some(size) => size,
                    None => break,
                },
            };
            if self.sum.saturating_add(size) > self.budget {
                self.progress.cut = Some(self.progress.decided);
            } else {
                self.sum += size;
                self.progress.decided += 1;
            }
        }
        self.tx.send_replace(self.progress);
    }
}

/// One path's view of the budget: whether its body may be read.
#[derive(Clone, Debug)]
pub struct Gate {
    ledger: Arc<Mutex<Ledger>>,
    rx: watch::Receiver<Progress>,
    distinct: usize,
    /// The first place the path was asked at.
    first: usize,
}

impl Gate {
    /// The declared length of the path's answer.
    fn report(&self, size: u64) {
        self.ledger
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .report(self.distinct, size);
    }

    /// Waits until the path's first place is decided: true where it stands
    /// in `files`, false where it is deferred.
    async fn go(&self) -> bool {
        let first = self.first;
        let mut rx = self.rx.clone();
        match rx
            .wait_for(|p| p.decided > first || p.cut.is_some_and(|cut| cut <= first))
            .await
        {
            Ok(progress) => progress.decided > first,
            Err(_) => false,
        }
    }

    /// Resolves once the cut falls at or before the path's first place.
    async fn cut_reached(&self) {
        let first = self.first;
        let mut rx = self.rx.clone();
        if rx
            .wait_for(|p| p.cut.is_some_and(|cut| cut <= first))
            .await
            .is_ok()
        {
            return;
        }
        std::future::pending::<()>().await;
    }
}

// ---- many paths ------------------------------------------------------------

/// What one distinct path of a run came to.
#[derive(Debug)]
pub enum PathResult {
    Read(Answer),
    Failed(CliError),
    Deferred,
    /// Never sent, or cancelled after a failure that stopped the run.
    NotRun,
}

/// One distinct path a run reads: the path sent, and as first typed.
#[derive(Debug, Clone)]
pub struct Distinct {
    pub mapped: String,
    pub typed: String,
}

/// When a failure stops the run's reads.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stop {
    /// A failure that is no per-file class ends every read (`--json`).
    RunWide,
    /// Any failure ends the reads after it in order; the reads before it
    /// finish (the byte stream).
    InOrder,
}

/// The reads of one run, by distinct path, and the `--max-bytes` cut.
#[derive(Debug)]
pub struct Many {
    pub results: Vec<PathResult>,
    pub cut: Option<usize>,
}

/// Reads every distinct path at most `IN_FLIGHT` at once, sent in the
/// order first asked. `entries` maps each place asked to its distinct
/// path, none for a path refused before any request. Under `budget`, the
/// places are decided in order from each answer's declared length (a
/// `304` at its stored length, an error at 0): once one takes the sum past
/// the budget, it and every place after it are deferred, no further read
/// is sent, and every read in flight for a deferred path is cancelled.
pub async fn read_many(
    client: Arc<SynsClient>,
    target: Arc<ReadTarget>,
    store: Option<Arc<Store>>,
    distinct: Vec<Distinct>,
    entries: &[Option<usize>],
    budget: Option<u64>,
    stop: Stop,
) -> Many {
    let n = distinct.len();
    let mut first = vec![usize::MAX; n];
    for (place, entry) in entries.iter().enumerate() {
        if let Some(d) = entry
            && first[*d] == usize::MAX
        {
            first[*d] = place;
        }
    }
    let ledger = budget.map(|budget| {
        let (tx, rx) = watch::channel(Progress::default());
        let mut ledger = Ledger {
            budget,
            entries: entries.to_vec(),
            sizes: vec![None; n],
            sum: 0,
            progress: Progress::default(),
            tx,
        };
        ledger.advance();
        (Arc::new(Mutex::new(ledger)), rx)
    });
    let cut_now = || {
        ledger.as_ref().and_then(|(ledger, _)| {
            ledger
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .progress
                .cut
        })
    };
    let policy = Policy {
        identity: budget.is_some(),
        want_type: false,
    };
    let distinct = Arc::new(distinct);

    let mut results: Vec<PathResult> = (0..n).map(|_| PathResult::NotRun).collect();
    let mut handles: Vec<Option<AbortHandle>> = vec![None; n];
    let mut set: JoinSet<(usize, Result<Outcome, CliError>)> = JoinSet::new();
    let mut next = 0;
    let mut stopped = false;
    loop {
        while !stopped && set.len() < IN_FLIGHT && next < n {
            if cut_now().is_some_and(|cut| first[next] >= cut) {
                break;
            }
            let d = next;
            next += 1;
            let gate = ledger.as_ref().map(|(ledger, rx)| Gate {
                ledger: ledger.clone(),
                rx: rx.clone(),
                distinct: d,
                first: first[d],
            });
            let (client, target, store, distinct) = (
                client.clone(),
                target.clone(),
                store.clone(),
                distinct.clone(),
            );
            handles[d] = Some(set.spawn(async move {
                let path = &distinct[d];
                let work = read_path(
                    &client,
                    &target,
                    store.as_deref(),
                    &path.mapped,
                    &path.typed,
                    policy,
                    gate.as_ref(),
                );
                let outcome = match &gate {
                    None => work.await,
                    Some(gate) => tokio::select! {
                        outcome = work => outcome,
                        () = gate.cut_reached() => Ok(Outcome::Deferred),
                    },
                };
                (d, outcome)
            }));
        }
        let Some(joined) = set.join_next().await else {
            break;
        };
        let (d, outcome) = match joined {
            Ok(done) => done,
            Err(err) if err.is_panic() => std::panic::resume_unwind(err.into_panic()),
            Err(_cancelled) => continue,
        };
        let result = match outcome {
            Ok(Outcome::Read(answer)) => PathResult::Read(answer),
            Ok(Outcome::Deferred) => PathResult::Deferred,
            Err(err) => PathResult::Failed(err),
        };
        if let Some((ledger, _)) = &ledger {
            let size = match &result {
                PathResult::Read(answer) => answer.bytes.len() as u64,
                _ => 0,
            };
            ledger
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .report(d, size);
        }
        if let PathResult::Failed(err) = &result
            && !stopped
            && (stop == Stop::InOrder || per_file(err, 0, "").is_none())
        {
            stopped = true;
            for (other, handle) in handles.iter().enumerate() {
                let cancel = match stop {
                    Stop::RunWide => other != d,
                    Stop::InOrder => other > d,
                };
                if let (true, Some(handle)) = (cancel, handle) {
                    handle.abort();
                }
            }
        }
        results[d] = result;
    }
    Many {
        results,
        cut: cut_now(),
    }
}

/// The per-file class of a failed read, its `code` and `message`, or
/// none where the failure ends the run (D-242 §7).
pub fn per_file(err: &CliError, version: u32, typed: &str) -> Option<(String, String)> {
    let class = |code: &str| Some((code.to_string(), err.to_string()));
    match err {
        CliError::Config { message } => Some(("CONFIG_ERROR".to_string(), message.clone())),
        CliError::ServerUnreachable { .. } | CliError::Api { status: None, .. } => {
            class("SERVER_UNREACHABLE")
        }
        CliError::Api {
            status: Some(404),
            error,
            ..
        } if error == "not_found" => Some((
            "NOT_FOUND".to_string(),
            path_not_found_refusal(version, typed),
        )),
        CliError::Api {
            status: Some(422), ..
        } => class("VALIDATION_ERROR"),
        CliError::Api {
            status: Some(200),
            error,
            ..
        } if error.starts_with("invalid response body: ") => {
            Some(("RESPONSE_UNDECODABLE".to_string(), error.clone()))
        }
        CliError::Api {
            status: Some(status),
            error,
            ..
        } if *status == 429 || *status >= 500 => {
            let named = !error.is_empty()
                && error
                    .bytes()
                    .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_');
            let code = if named {
                error.to_ascii_uppercase()
            } else {
                "INTERNAL_ERROR".to_string()
            };
            Some((code, err.to_string()))
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ledger(budget: u64, entries: Vec<Option<usize>>, n: usize) -> Ledger {
        let (tx, _rx) = watch::channel(Progress::default());
        Ledger {
            budget,
            entries,
            sizes: vec![None; n],
            sum: 0,
            progress: Progress::default(),
            tx,
        }
    }

    // Rows 16, 17 and 20: decided in order; once one passes, every later
    // place is cut, a later small one included; a refusal counts 0, and
    // each repetition counts.
    #[test]
    fn the_budget_decides_in_order() {
        let mut l = ledger(60_000, vec![Some(0), Some(1), Some(2)], 3);
        l.report(2, 7000);
        assert_eq!(
            l.progress,
            Progress {
                decided: 0,
                cut: None
            }
        );
        l.report(0, 18342);
        l.report(1, 40211);
        assert_eq!(
            l.progress,
            Progress {
                decided: 2,
                cut: Some(2)
            }
        );

        let mut l = ledger(100, vec![Some(0), Some(1), Some(2)], 3);
        l.report(0, 10);
        l.report(1, 1000);
        l.report(2, 1);
        assert_eq!(l.progress.cut, Some(1));

        let mut l = ledger(250, vec![Some(0), Some(0), None, Some(1)], 2);
        l.report(0, 100);
        l.report(1, 50);
        assert_eq!(
            l.progress,
            Progress {
                decided: 4,
                cut: None
            }
        );
        let mut l = ledger(249, vec![Some(0), Some(0), None, Some(1)], 2);
        l.report(0, 100);
        l.report(1, 50);
        l.report(1, 0);
        assert_eq!(
            l.progress,
            Progress {
                decided: 3,
                cut: Some(3)
            }
        );
    }

    // D-242 §7: the per-file classes, and the run-wide refusals.
    #[test]
    fn per_file_classes() {
        let api = |status: u16, error: &str| CliError::Api {
            status: Some(status),
            error: error.to_string(),
            context: None,
        };
        let code = |err: CliError| per_file(&err, 7, "x.md").map(|(code, _)| code);
        assert_eq!(
            per_file(&api(404, "not_found"), 7, "x.md"),
            Some((
                "NOT_FOUND".to_string(),
                "path not found at version 7: x.md".to_string()
            ))
        );
        assert_eq!(
            code(api(422, "validation_error")).as_deref(),
            Some("VALIDATION_ERROR")
        );
        assert_eq!(
            code(api(503, "engine_busy")).as_deref(),
            Some("ENGINE_BUSY")
        );
        assert_eq!(
            code(api(429, "rate_limited")).as_deref(),
            Some("RATE_LIMITED")
        );
        assert_eq!(
            code(api(500, "internal_error")).as_deref(),
            Some("INTERNAL_ERROR")
        );
        assert_eq!(
            code(api(502, "unknown error")).as_deref(),
            Some("INTERNAL_ERROR")
        );
        assert_eq!(
            code(hash_mismatch("x.md", &"0".repeat(40), &"1".repeat(40))).as_deref(),
            Some("RESPONSE_UNDECODABLE")
        );
        assert_eq!(
            code(CliError::ServerUnreachable { url: "u".into() }).as_deref(),
            Some("SERVER_UNREACHABLE")
        );
        for run_wide in [
            CliError::AuthRequired,
            api(404, "repo_not_found"),
            api(403, "forbidden"),
            api(400, "bad_request"),
        ] {
            assert_eq!(per_file(&run_wide, 7, "x.md"), None, "{run_wide:?}");
        }
    }
}

// ---- many paths over `POST …/read` (roadmap 279, D-243 §8) ---------------

/// What reading many paths over the batch came to: each distinct path's
/// result (`NotRun` for one the batch left to be read alone), those
/// paths, the header of the first answer, and the `--max-bytes` cut.
#[derive(Debug)]
pub struct BatchMany {
    pub results: Vec<PathResult>,
    pub alone: Vec<usize>,
    pub header: Option<crate::read::batch::Header>,
    pub cut: Option<usize>,
}

/// The `--max-bytes` ledger over frames, decided in the order asked from
/// each frame's size, a path refused before any request counting 0.
struct FrameLedger {
    budget: u64,
    entries: Vec<Option<usize>>,
    first: Vec<usize>,
    sizes: Vec<Option<u64>>,
    sum: u64,
    decided: usize,
    cut: Option<usize>,
}

impl FrameLedger {
    fn report(&mut self, distinct: usize, size: u64) -> bool {
        if self.sizes[distinct].is_none() {
            self.sizes[distinct] = Some(size);
        }
        while self.cut.is_none() && self.decided < self.entries.len() {
            let size = match self.entries[self.decided] {
                None => 0,
                Some(d) => match self.sizes[d] {
                    Some(size) => size,
                    None => break,
                },
            };
            if self.sum.saturating_add(size) > self.budget {
                self.cut = Some(self.decided);
            } else {
                self.sum += size;
                self.decided += 1;
            }
        }
        self.cut.is_none_or(|cut| self.first[distinct] < cut)
    }
}

/// The batch's sink for `syns cat --json`: per-file classes, the store's
/// bytes for an `unchanged` frame, and the ledger.
struct CatSink<'a> {
    repo_id: &'a str,
    store: Option<&'a Store>,
    distinct: &'a [Distinct],
    results: Mutex<Vec<PathResult>>,
    alone: Mutex<Vec<usize>>,
    ledger: Option<Mutex<FrameLedger>>,
}

impl crate::read::batch::Sink for CatSink<'_> {
    fn frame(&self, index: usize, frame: crate::read::batch::Frame) -> Result<(), CliError> {
        use crate::read::batch::Frame;
        let result = match frame {
            Frame::Ok { sha, bytes, .. } => {
                if let Some(store) = self.store {
                    store.admit_many(&sha, &bytes);
                    store.note_seen(self.repo_id, &self.distinct[index].mapped, &sha);
                }
                PathResult::Read(Answer {
                    bytes,
                    sha,
                    media_type: None,
                    not_modified: false,
                })
            }
            Frame::Unchanged { sha, .. } => {
                match self.store.and_then(|store| store.verified(&sha)) {
                    Some(bytes) => PathResult::Read(Answer {
                        bytes,
                        sha,
                        media_type: None,
                        not_modified: true,
                    }),
                    None => {
                        self.alone
                            .lock()
                            .unwrap_or_else(PoisonError::into_inner)
                            .push(index);
                        return Ok(());
                    }
                }
            }
            Frame::NotFound => PathResult::Failed(CliError::Api {
                status: Some(404),
                error: "not_found".to_string(),
                context: None,
            }),
            Frame::TooLarge { .. } => {
                self.alone
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .push(index);
                return Ok(());
            }
            Frame::Mismatch { expected, actual } => PathResult::Failed(hash_mismatch(
                &self.distinct[index].typed,
                &expected,
                &actual,
            )),
        };
        self.results.lock().unwrap_or_else(PoisonError::into_inner)[index] = result;
        Ok(())
    }

    fn gave_up(&self, indices: &[usize]) {
        if let Some(ledger) = &self.ledger {
            let mut ledger = ledger.lock().unwrap_or_else(PoisonError::into_inner);
            for index in indices {
                ledger.report(*index, 0);
            }
        }
    }

    fn admit(&self, index: usize, size: u64) -> bool {
        match &self.ledger {
            Some(ledger) => ledger
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .report(index, size),
            None => true,
        }
    }
}

/// Reads every distinct path at `at` (`"head"` or a commit) over the
/// batch, `have` the hint's hash where the stored blob verifies, under
/// `budget` one chunk at a time (D-243 §8). Answers `None` where the
/// server answers no v2: the run then reads as 278 does.
#[allow(clippy::too_many_arguments)]
pub async fn read_many_batch(
    client: &SynsClient,
    repo_id: &str,
    token: Option<&str>,
    at: &str,
    store: Option<&Store>,
    distinct: &[Distinct],
    entries: &[Option<usize>],
    budget: Option<u64>,
) -> Result<Option<BatchMany>, CliError> {
    use crate::read::batch::{Ask, IN_FLIGHT, read_chunks_with};
    let n = distinct.len();
    let mut first = vec![usize::MAX; n];
    for (place, entry) in entries.iter().enumerate() {
        if let Some(d) = entry
            && first[*d] == usize::MAX
        {
            first[*d] = place;
        }
    }
    let asks: Vec<Ask> = distinct
        .iter()
        .map(|d| Ask {
            path: d.mapped.clone(),
            have: store
                .and_then(|store| store.held(repo_id, &d.mapped))
                .map(|held| held.sha),
            expect: None,
            size: Some(0),
        })
        .collect();
    let sink = CatSink {
        repo_id,
        store,
        distinct,
        results: Mutex::new((0..n).map(|_| PathResult::NotRun).collect()),
        alone: Mutex::new(Vec::new()),
        ledger: budget.map(|budget| {
            let mut ledger = FrameLedger {
                budget,
                entries: entries.to_vec(),
                first: first.clone(),
                sizes: vec![None; n],
                sum: 0,
                decided: 0,
                cut: None,
            };
            // Places refused before any request decide at once.
            if n == 0 {
                ledger.report_none();
            }
            Mutex::new(ledger)
        }),
    };
    let in_flight = if budget.is_some() { 1 } else { IN_FLIGHT };
    let read = match read_chunks_with(
        client, None, repo_id, token, at, &asks, in_flight, &sink, true,
    )
    .await
    {
        Ok(read) => read,
        Err(err) if crate::read::v2::is_absent(&err) => return Ok(None),
        Err(err) => return Err(err),
    };
    if read
        .failed
        .iter()
        .any(|(_, err)| crate::read::v2::is_absent(err))
    {
        return Ok(None);
    }
    let mut results = sink
        .results
        .into_inner()
        .unwrap_or_else(PoisonError::into_inner);
    for (left, err) in read.failed {
        for i in left {
            results[i] = PathResult::Failed(copy_error(&err));
        }
    }
    let cut = sink.ledger.and_then(|ledger| {
        ledger
            .into_inner()
            .unwrap_or_else(PoisonError::into_inner)
            .cut
    });
    if let Some(cut) = cut {
        for (d, result) in results.iter_mut().enumerate() {
            if first[d] >= cut {
                *result = PathResult::Deferred;
            }
        }
    }
    Ok(Some(BatchMany {
        results,
        alone: sink
            .alone
            .into_inner()
            .unwrap_or_else(PoisonError::into_inner),
        header: read.header,
        cut,
    }))
}

impl FrameLedger {
    /// Decides the places that need no frame.
    fn report_none(&mut self) {
        while self.cut.is_none() && self.decided < self.entries.len() {
            if self.entries[self.decided].is_some() {
                break;
            }
            self.decided += 1;
        }
    }
}

/// The same failure again, for each path of a chunk that gave up.
fn copy_error(err: &CliError) -> CliError {
    match err {
        CliError::ServerUnreachable { url } => CliError::ServerUnreachable { url: url.clone() },
        CliError::Api {
            status,
            error,
            context,
        } => CliError::Api {
            status: *status,
            error: error.clone(),
            context: context.clone(),
        },
        CliError::AuthRequired => CliError::AuthRequired,
        other => CliError::Io {
            message: other.to_string(),
        },
    }
}
