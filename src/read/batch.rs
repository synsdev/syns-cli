//! `POST …/read` in chunks (roadmap 279, D-243 §3, §8).
//!
//! The paths a run asks are packed in the order given into chunks of at
//! most `CHUNK_PATHS` paths, `CHUNK_BODY_BYTES` of request body and
//! `CHUNK_NAMED_BYTES` of file sizes as the caller names them (a size it
//! does not name counted as `FILE_ALONE_BYTES`). At most `IN_FLIGHT`
//! chunks are open at once. Each frame is checked as it arrives and handed
//! to the caller's sink, so what arrived stays whatever happens to the
//! rest; a chunk is resent for the paths not yet answered, under the
//! budgets of `v2::bulk`. A path answered `too_large` is asked again in a
//! later chunk, and handed on as `too_large` only the second time.

use std::sync::Mutex;

use futures_util::stream::{FuturesUnordered, StreamExt};
use serde::Deserialize;

use crate::client::{ANSWER_STALL, SynsClient, send_unretried, undecodable};
use crate::errors::CliError;
use crate::push::hash::blob_sha1;
use crate::read::run::RunReads;
use crate::read::store::is_blob_hash;
use crate::read::v2::{Attempt, bulk, refused, unsent};

/// The most paths one chunk asks (Bart's chunk size, Q-0150).
pub const CHUNK_PATHS: usize = 500;

/// The most request body one chunk sends: under `LIM-body-default`'s
/// 1 MiB with room for its envelope (D-243 §3).
pub const CHUNK_BODY_BYTES: usize = 786_432;

/// The most file bytes one chunk names: `LIM-read-batch-answer`.
pub const CHUNK_NAMED_BYTES: u64 = 33_554_432;

/// The batch's per-file limit: a file the head names larger is read
/// alone through `EP-file-read-bytes` (D-243 §3).
pub const FILE_ALONE_BYTES: u64 = 8_388_608;

/// The longest frame line an answer may send: a path is at most 1,000
/// characters.
const FRAME_LINE_BOUND: usize = 65_536;

/// The most chunks of one run in flight at once.
pub const IN_FLIGHT: usize = 4;

/// One path a run asks.
#[derive(Debug, Clone)]
pub struct Ask {
    /// The path as the request sends it.
    pub path: String,
    /// The hash the store or the collected file holds, sent as `have`.
    pub have: Option<String>,
    /// The hash the head named for the path, an `ok` frame naming another
    /// refused (D-243 §3).
    pub expect: Option<String>,
    /// The size the head named.
    pub size: Option<u64>,
}

/// One frame, checked.
#[derive(Debug)]
pub enum Frame {
    /// The bytes, hashing to `sha`.
    Ok {
        sha: String,
        mode: String,
        bytes: Vec<u8>,
    },
    /// `have` named the blob: no bytes.
    Unchanged {
        sha: String,
        mode: String,
        size: u64,
    },
    NotFound,
    /// Left out of the answer a second time.
    TooLarge {
        sha: String,
        size: u64,
    },
    /// An `ok` frame whose bytes hash to `actual`, not to `expected` (its
    /// own `sha`, or the one the head named): not resent (D-243 §3).
    Mismatch {
        expected: String,
        actual: String,
    },
}

/// Where a chunked read hands its frames.
pub trait Sink: Sync {
    /// One frame for ask `index`; an error ends the read, not resent.
    fn frame(&self, index: usize, frame: Frame) -> Result<(), CliError>;

    /// Asked before the bytes of each frame in the order asked, under
    /// `--max-bytes`: false stops the read there (D-243 §8).
    fn admit(&self, _index: usize, _size: u64) -> bool {
        true
    }

    /// The asks a chunk that gave up left unanswered, under `keep_going`:
    /// each counts as an error frame does (REVIEW-syns-cli minor 5).
    fn gave_up(&self, _indices: &[usize]) {}
}

/// What the header of an answer named.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Header {
    pub commit: String,
    pub version: Option<u32>,
}

#[derive(Deserialize)]
struct HeaderLine {
    commit: String,
    version: Option<u32>,
}

#[derive(Deserialize)]
struct FrameLine {
    path: String,
    #[serde(default)]
    sha: Option<String>,
    #[serde(default)]
    size: Option<u64>,
    #[serde(default)]
    mode: Option<String>,
    status: String,
}

/// The JSON a path costs in the body: `{"path":…,"have":…},`.
fn body_cost(ask: &Ask) -> usize {
    let path = serde_json::Value::from(ask.path.as_str()).to_string().len();
    let have = if ask.have.is_some() { 50 } else { 0 };
    path + have + 12
}

/// The envelope a body carries besides its paths.
const ENVELOPE: usize = 64;

/// The chunks `asks` pack into, as index ranges in the order given.
pub fn pack(asks: &[Ask]) -> Vec<std::ops::Range<usize>> {
    let mut chunks = Vec::new();
    let mut start = 0;
    let (mut body, mut named) = (ENVELOPE, 0u64);
    for (i, ask) in asks.iter().enumerate() {
        let cost = body_cost(ask);
        let size = ask.size.unwrap_or(FILE_ALONE_BYTES);
        let count = i - start;
        if count > 0
            && (count >= CHUNK_PATHS
                || body + cost > CHUNK_BODY_BYTES
                || named + size > CHUNK_NAMED_BYTES)
        {
            chunks.push(start..i);
            start = i;
            body = ENVELOPE;
            named = 0;
        }
        body += cost;
        named += size;
    }
    if start < asks.len() {
        chunks.push(start..asks.len());
    }
    chunks
}

/// An answer's body read as it arrives, each piece within `ANSWER_STALL`.
struct Stream {
    response: reqwest::Response,
    buf: Vec<u8>,
    at: usize,
    ended: bool,
    url: String,
}

impl Stream {
    fn new(response: reqwest::Response) -> Stream {
        let url = response.url().to_string();
        Stream {
            response,
            buf: Vec::new(),
            at: 0,
            ended: false,
            url,
        }
    }

    fn cut(&self) -> CliError {
        CliError::ServerUnreachable {
            url: self.url.clone(),
        }
    }

    /// One more piece, false at the end.
    async fn more(&mut self) -> Result<bool, CliError> {
        if self.ended {
            return Ok(false);
        }
        if self.at > 0 && self.at * 2 > self.buf.len() {
            self.buf.drain(..self.at);
            self.at = 0;
        }
        match tokio::time::timeout(ANSWER_STALL, self.response.chunk()).await {
            Ok(Ok(Some(chunk))) => {
                self.buf.extend_from_slice(&chunk);
                Ok(true)
            }
            Ok(Ok(None)) => {
                self.ended = true;
                Ok(false)
            }
            _ => Err(self.cut()),
        }
    }

    /// The next line, none at a clean end; a line longer than any frame
    /// line can be is a cut.
    async fn line(&mut self) -> Result<Option<Vec<u8>>, CliError> {
        loop {
            if let Some(pos) = self.buf[self.at..].iter().position(|b| *b == b'\n') {
                let line = self.buf[self.at..self.at + pos].to_vec();
                self.at += pos + 1;
                return Ok(Some(line));
            }
            if self.buf.len() - self.at > FRAME_LINE_BOUND {
                return Err(self.cut());
            }
            if !self.more().await? {
                return if self.at == self.buf.len() {
                    Ok(None)
                } else {
                    Err(self.cut())
                };
            }
        }
    }

    /// Exactly `n` bytes.
    async fn take(&mut self, n: usize) -> Result<Vec<u8>, CliError> {
        while self.buf.len() - self.at < n {
            if !self.more().await? {
                return Err(self.cut());
            }
        }
        let out = self.buf[self.at..self.at + n].to_vec();
        self.at += n;
        Ok(out)
    }
}

/// The state of one chunk across its sends.
struct Chunk {
    /// The asks not yet answered, in order.
    left: Vec<usize>,
    /// Asks answered `too_large` once, for a later chunk.
    again: Vec<usize>,
}

/// Why one send stopped short.
enum Short {
    Cut(CliError),
    Check(CliError),
    Stopped,
}

/// Reads the asks in `chunk` at `at` once, handing each checked frame to
/// `sink`; answers the header.
#[allow(clippy::too_many_arguments)]
async fn send_chunk(
    client: &SynsClient,
    address: &str,
    token: Option<&str>,
    at: &str,
    asks: &[Ask],
    state: &Mutex<Chunk>,
    second: &[bool],
    sink: &dyn Sink,
) -> Attempt<(Header, bool)> {
    let left: Vec<usize> = state.lock().unwrap_or_else(|e| e.into_inner()).left.clone();
    let paths: Vec<serde_json::Value> = left
        .iter()
        .map(|i| {
            let ask = &asks[*i];
            match &ask.have {
                Some(have) => serde_json::json!({"path": ask.path, "have": have}),
                None => serde_json::json!({"path": ask.path}),
            }
        })
        .collect();
    let at_value = match at.parse::<u64>() {
        Ok(n) if !is_blob_hash(at) => serde_json::Value::from(n),
        _ => serde_json::Value::from(at),
    };
    let body = serde_json::json!({"at": at_value, "paths": paths}).to_string();
    let len = body.len();
    let response =
        match send_unretried(client.v2_read(address, token, body.into_bytes()), len).await {
            Ok(response) => response,
            Err(err) => return unsent(err),
        };
    let status = response.status();
    if !status.is_success() {
        return refused(response).await;
    }
    if !crate::read::v2::is_shaped(&response, "application/x-syns-frames") {
        return Attempt::Final(crate::read::v2::unshaped());
    }
    let mut stream = Stream::new(response);
    let header = match stream.line().await {
        Ok(Some(line)) => match serde_json::from_slice::<HeaderLine>(&line) {
            Ok(h) if is_blob_hash(&h.commit) => Header {
                commit: h.commit,
                version: h.version,
            },
            _ => {
                return Attempt::Failed {
                    err: undecodable(status, "the batch answer's header does not parse"),
                    progress: false,
                };
            }
        },
        Ok(None) => {
            return Attempt::Failed {
                err: stream.cut(),
                progress: false,
            };
        }
        Err(err) => {
            return Attempt::Failed {
                err,
                progress: false,
            };
        }
    };
    if is_blob_hash(at) && header.commit != at {
        return Attempt::Final(undecodable(
            status,
            "the batch answer names another commit than asked",
        ));
    }
    let mut progress = false;
    let outcome: Result<(), Short> = async {
        for &i in &left {
            let ask = &asks[i];
            let line = match stream.line().await {
                Ok(Some(line)) => line,
                Ok(None) => return Err(Short::Cut(stream.cut())),
                Err(err) => return Err(Short::Cut(err)),
            };
            let Ok(frame) = serde_json::from_slice::<FrameLine>(&line) else {
                return Err(Short::Cut(stream.cut()));
            };
            if frame.path != ask.path {
                return Err(Short::Check(undecodable(
                    status,
                    format!("the batch answered {} for {}", frame.path, ask.path),
                )));
            }
            let named_size = match frame.status.as_str() {
                "ok" | "unchanged" | "too_large" => frame.size.unwrap_or(0),
                _ => 0,
            };
            if !sink.admit(i, named_size) {
                return Err(Short::Stopped);
            }
            let checked = match frame.status.as_str() {
                "ok" => {
                    // A frame naming no mode is one that does not parse,
                    // never "no file" (REVIEW-syns-cli I3).
                    let (Some(sha), Some(size), Some(_)) =
                        (frame.sha.clone(), frame.size, frame.mode.as_ref())
                    else {
                        return Err(Short::Cut(stream.cut()));
                    };
                    // No frame carries more than the batch's per-file
                    // limit: a larger one is refused before any of its
                    // bytes is held (REVIEW-syns-cli minor 6).
                    if size > FILE_ALONE_BYTES {
                        return Err(Short::Check(undecodable(
                            status,
                            format!("the batch named {size} bytes for {}", ask.path),
                        )));
                    }
                    let Ok(len) = usize::try_from(size) else {
                        return Err(Short::Cut(stream.cut()));
                    };
                    let bytes = stream.take(len).await.map_err(Short::Cut)?;
                    let actual = blob_sha1(&bytes);
                    match &ask.expect {
                        _ if actual != sha => Frame::Mismatch {
                            expected: sha,
                            actual,
                        },
                        Some(expect) if *expect != sha => Frame::Mismatch {
                            expected: expect.clone(),
                            actual: sha,
                        },
                        _ => Frame::Ok {
                            sha,
                            mode: frame.mode.unwrap_or_default(),
                            bytes,
                        },
                    }
                }
                "unchanged" => {
                    let (Some(sha), Some(mode)) = (frame.sha.clone(), frame.mode.clone()) else {
                        return Err(Short::Cut(stream.cut()));
                    };
                    // `unchanged` answers the hash sent as `have`, and the
                    // one the head named: any other is refused, as a
                    // mismatched `ok` is, and nothing is written from it
                    // (REVIEW-syns-cli I3).
                    let asked = ask.have.as_deref() == Some(sha.as_str())
                        && ask.expect.as_ref().is_none_or(|expect| *expect == sha);
                    if asked {
                        Frame::Unchanged {
                            sha,
                            mode,
                            size: frame.size.unwrap_or(0),
                        }
                    } else {
                        Frame::Mismatch {
                            expected: ask
                                .have
                                .clone()
                                .or_else(|| ask.expect.clone())
                                .unwrap_or_default(),
                            actual: sha,
                        }
                    }
                }
                "not_found" => Frame::NotFound,
                "too_large" => {
                    if !second[i] {
                        let mut state = state.lock().unwrap_or_else(|e| e.into_inner());
                        state.left.retain(|j| *j != i);
                        state.again.push(i);
                        progress = true;
                        continue;
                    }
                    Frame::TooLarge {
                        sha: frame.sha.clone().unwrap_or_default(),
                        size: frame.size.unwrap_or(0),
                    }
                }
                _ => return Err(Short::Cut(stream.cut())),
            };
            sink.frame(i, checked).map_err(Short::Check)?;
            state
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .left
                .retain(|j| *j != i);
            progress = true;
            // The other chunks of the run, and their resend timers, turn
            // between frames: they share this task (D-243 §3).
            tokio::task::yield_now().await;
        }
        Ok(())
    }
    .await;
    match outcome {
        Ok(()) => Attempt::Done((header, false)),
        Err(Short::Stopped) => Attempt::Done((header, true)),
        Err(Short::Check(err)) => Attempt::Final(err),
        Err(Short::Cut(err)) => Attempt::Failed { err, progress },
    }
}

/// What a chunked read came to.
#[derive(Debug)]
pub struct Read {
    /// The header of the first answer, none where no chunk was sent.
    pub header: Option<Header>,
    /// Whether `Sink::admit` stopped it.
    pub stopped: bool,
    /// With `keep_going`, each chunk that gave up: the asks it left
    /// unanswered and its last answer.
    pub failed: Vec<(Vec<usize>, CliError)>,
}

/// One chunk read to its end under the bulk budgets: its answer, and the
/// asks it was answered `too_large` for the first time.
#[allow(clippy::too_many_arguments)]
async fn run_chunk(
    client: &SynsClient,
    reads: Option<&RunReads>,
    address: &str,
    token: Option<&str>,
    at: &str,
    asks: &[Ask],
    second: &[bool],
    sink: &dyn Sink,
    indices: Vec<usize>,
) -> (Result<(Header, bool), (CliError, Vec<usize>)>, Vec<usize>) {
    let state = Mutex::new(Chunk {
        left: indices,
        again: Vec::new(),
    });
    let result = bulk(reads, || {
        send_chunk(client, address, token, at, asks, &state, second, sink)
    })
    .await;
    let mut state = state.into_inner().unwrap_or_else(|e| e.into_inner());
    let result = result.map_err(|err| (err, std::mem::take(&mut state.left)));
    (result, state.again)
}

/// Reads every ask at `at` in chunks, at most `in_flight` open at once.
/// At `"head"` the first chunk goes alone and the rest at the commit its
/// header names. A chunk that gives up stops new chunks; those in flight
/// run to their end, and the first given-up chunk's error is answered.
#[allow(clippy::too_many_arguments)]
pub async fn read_chunks(
    client: &SynsClient,
    reads: Option<&RunReads>,
    address: &str,
    token: Option<&str>,
    at: &str,
    asks: &[Ask],
    in_flight: usize,
    sink: &dyn Sink,
) -> Result<Read, CliError> {
    read_chunks_with(
        client, reads, address, token, at, asks, in_flight, sink, false,
    )
    .await
}

/// `read_chunks`; with `keep_going` a chunk that gives up stops nothing
/// else, and is answered among `Read::failed` (`syns cat --json`, D-243
/// §8).
#[allow(clippy::too_many_arguments)]
pub async fn read_chunks_with(
    client: &SynsClient,
    reads: Option<&RunReads>,
    address: &str,
    token: Option<&str>,
    at: &str,
    asks: &[Ask],
    in_flight: usize,
    sink: &dyn Sink,
    keep_going: bool,
) -> Result<Read, CliError> {
    let mut failed: Vec<(Vec<usize>, CliError)> = Vec::new();
    let mut queue: std::collections::VecDeque<Vec<usize>> = pack(asks)
        .into_iter()
        .map(|range| range.collect())
        .collect();
    let mut first_round = vec![false; asks.len()];
    let mut header: Option<Header> = None;
    let mut stopped = false;
    let in_flight = in_flight.max(1);
    let requeue = |again: Vec<usize>, queue: &mut std::collections::VecDeque<Vec<usize>>| {
        let again_asks: Vec<Ask> = again.iter().map(|i| asks[*i].clone()).collect();
        for range in pack(&again_asks) {
            queue.push_back(range.map(|j| again[j]).collect());
        }
    };

    // At `head`, the first chunk alone: the rest go at the commit it names.
    let mut at = at.to_string();
    let mut pending_again: Vec<usize> = Vec::new();
    if !is_blob_hash(&at)
        && let Some(indices) = queue.pop_front()
    {
        let (result, again) = run_chunk(
            client,
            reads,
            address,
            token,
            &at,
            asks,
            &first_round,
            sink,
            indices,
        )
        .await;
        match result {
            Ok((answer, cut)) => {
                stopped = cut;
                at = answer.commit.clone();
                header = Some(answer);
            }
            Err((err, left)) if keep_going => {
                // The rest cannot be pinned to a commit no answer named.
                let mut left = left;
                sink.gave_up(&left);
                left.extend(queue.drain(..).flatten());
                failed.push((left, err));
                return Ok(Read {
                    header,
                    stopped,
                    failed,
                });
            }
            Err((err, _)) => return Err(err),
        }
        pending_again = again;
    }
    for &i in &pending_again {
        first_round[i] = true;
    }
    requeue(pending_again, &mut queue);

    // Which asks have been answered `too_large` once already: those a
    // later chunk hands on as `too_large`. Fixed per wave of requeues.
    let mut first_error: Option<CliError> = None;
    while !queue.is_empty() && first_error.is_none() && !stopped {
        let second = first_round.clone();
        let mut again_all: Vec<usize> = Vec::new();
        {
            let mut running = FuturesUnordered::new();
            loop {
                while running.len() < in_flight && first_error.is_none() && !stopped {
                    let Some(indices) = queue.pop_front() else {
                        break;
                    };
                    running.push(run_chunk(
                        client, reads, address, token, &at, asks, &second, sink, indices,
                    ));
                }
                let Some((result, again)) = running.next().await else {
                    break;
                };
                again_all.extend(again);
                match result {
                    Ok((answer, cut)) => {
                        stopped |= cut;
                        header.get_or_insert(answer);
                    }
                    Err((err, left)) if keep_going => {
                        sink.gave_up(&left);
                        failed.push((left, err));
                    }
                    Err((err, _)) => {
                        first_error.get_or_insert(err);
                    }
                }
            }
        }
        for &i in &again_all {
            first_round[i] = true;
        }
        requeue(again_all, &mut queue);
    }
    match first_error {
        Some(err) => Err(err),
        None => Ok(Read {
            header,
            stopped,
            failed,
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ask(path: &str, size: Option<u64>) -> Ask {
        Ask {
            path: path.to_string(),
            have: None,
            expect: None,
            size,
        }
    }

    // Row 4.
    #[test]
    fn chunks_pack_to_their_three_bounds() {
        let many: Vec<Ask> = (0..2600)
            .map(|i| ask(&format!("f{i:04}"), Some(10)))
            .collect();
        let chunks = pack(&many);
        assert_eq!(chunks.len(), 6);
        assert!(chunks.iter().all(|c| c.len() <= CHUNK_PATHS));

        let long: Vec<Ask> = (0..500)
            .map(|i| ask(&format!("{i:04}{}", "\u{6f22}".repeat(996)), Some(1)))
            .collect();
        let chunks = pack(&long);
        assert!(chunks.len() > 1);
        for chunk in &chunks {
            let body: usize = ENVELOPE + long[chunk.clone()].iter().map(body_cost).sum::<usize>();
            assert!(body <= CHUNK_BODY_BYTES);
        }

        let images: Vec<Ask> = (0..500)
            .map(|i| ask(&format!("img{i:03}.png"), Some(300 * 1024)))
            .collect();
        let chunks = pack(&images);
        assert_eq!(chunks.len(), 5);
        assert!(chunks.iter().all(|c| c.len() <= 109));

        let big: Vec<Ask> = (0..5)
            .map(|i| ask(&format!("b{i}"), Some(7 << 20)))
            .collect();
        let chunks = pack(&big);
        assert_eq!(chunks, vec![0..4, 4..5]);
        assert_eq!(pack(&[ask("u", None)]).len(), 1);
        let nameless: Vec<Ask> = (0..5).map(|i| ask(&format!("u{i}"), None)).collect();
        assert_eq!(pack(&nameless), vec![0..4, 4..5]);
    }
}
