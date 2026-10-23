//! The v2 reads a run makes (roadmap 276's `EP-v2-head`, `EP-v2-tree`,
//! `EP-v2-changes`; `EP-v2-read` in `batch`), and the budgets their
//! resends draw on (roadmap 279, D-243 §3).
//!
//! A bulk read is resent, asking only what has not arrived, after a send
//! that got no answer or was cut, and after a `503` or `429` naming a
//! `Retry-After` of at most `RETRY_AFTER_CAP`. Four sends that got no
//! answer or were cut before their first checked frame end it; a cut after
//! one costs none. `Retry-After` waits draw on 60 s per read and on the
//! run's one window of 120 s. Every wait carries 0–50 % jitter, and a
//! progress cut is resent after 0–250 ms.

use std::future::Future;
use std::time::{Duration, Instant};

use serde::Deserialize;

use crate::client::{
    ANSWER_STALL, READ_ATTEMPTS, RESEND_PAUSE, RESEND_WITHIN, SynsClient, check_response, jittered,
    read_body, retry_after, send_bounded, send_unretried, undecodable, uniform,
};
use crate::errors::CliError;
use crate::read::closure::Closure;
use crate::read::run::{RunReads, scaled};

/// The most one read waits on `Retry-After` answers in all (D-243 §3).
pub const READ_WAIT_BUDGET: Duration = Duration::from_secs(60);

/// The most a resend after a cut that delivered a checked frame waits.
pub const PROGRESS_PAUSE: Duration = Duration::from_millis(250);

/// What one send of a bulk read came to.
pub(crate) enum Attempt<T> {
    /// The answer, whole and checked.
    Done(T),
    /// No answer, an answer cut, or one failing its check; `progress`
    /// where it delivered at least one checked frame.
    Failed { err: CliError, progress: bool },
    /// A `503` or `429` naming a `Retry-After` within the cap.
    Wait { wait: Duration, err: CliError },
    /// The read's answer, not sent again.
    Final(CliError),
}

/// Sends `attempt` until it is done or its budgets are spent (D-243 §3).
pub(crate) async fn bulk<T, F, Fut>(reads: Option<&RunReads>, attempt: F) -> Result<T, CliError>
where
    F: FnMut() -> Fut,
    Fut: Future<Output = Attempt<T>>,
{
    bulk_within(reads, scaled(READ_WAIT_BUDGET), attempt).await
}

/// `bulk`, its `Retry-After` waits drawing on `budget`.
pub(crate) async fn bulk_within<T, F, Fut>(
    reads: Option<&RunReads>,
    budget: Duration,
    mut attempt: F,
) -> Result<T, CliError>
where
    F: FnMut() -> Fut,
    Fut: Future<Output = Attempt<T>>,
{
    // Measured from the last send that made progress: a chunk that keeps
    // delivering checked frames is bounded only by the paths left (D-243
    // §3, row 21a; REVIEW-syns-cli I2).
    let mut since = Instant::now();
    let mut failures = 0u32;
    let mut waited = Duration::ZERO;
    let mut pause = scaled(RESEND_PAUSE);
    loop {
        match attempt().await {
            Attempt::Done(answer) => return Ok(answer),
            Attempt::Final(err) => return Err(err),
            Attempt::Failed { progress: true, .. } => {
                since = Instant::now();
                tokio::time::sleep(PROGRESS_PAUSE.mul_f64(uniform())).await;
            }
            Attempt::Failed {
                err,
                progress: false,
            } => {
                failures += 1;
                if failures >= READ_ATTEMPTS || since.elapsed() >= RESEND_WITHIN {
                    return Err(err);
                }
                tokio::time::sleep(jittered(pause)).await;
                pause *= 2;
            }
            Attempt::Wait { wait, err } => {
                let wait = jittered(scaled(wait));
                if waited + wait > budget || reads.is_some_and(|reads| !reads.window_admits(wait)) {
                    return Err(err);
                }
                waited += wait;
                tokio::time::sleep(wait).await;
            }
        }
    }
}

/// The attempt a refused answer makes: a `503` or `429` naming a wait
/// within the cap waits; every other refusal is final, worded as
/// `check_response` words it.
pub(crate) async fn refused<T>(response: reqwest::Response) -> Attempt<T> {
    let wait = retry_after(&response);
    let err = match check_response(response).await {
        Err(err) => err,
        Ok(response) => undecodable(response.status(), "an unexpected status"),
    };
    if matches!(err, CliError::ServerUnreachable { .. }) {
        return Attempt::Failed {
            err,
            progress: false,
        };
    }
    match wait {
        Some(wait) => Attempt::Wait { wait, err },
        None => Attempt::Final(err),
    }
}

/// The attempt a send failure makes: no answer is resent, every other
/// failure is final.
pub(crate) fn unsent<T>(err: CliError) -> Attempt<T> {
    match err {
        err @ CliError::ServerUnreachable { .. } => Attempt::Failed {
            err,
            progress: false,
        },
        err => Attempt::Final(err),
    }
}

/// Whether `err` is a `404` of any body: the server answers no v2 at
/// that address (D-243 §7).
pub fn is_absent(err: &CliError) -> bool {
    matches!(
        err,
        CliError::Api {
            status: Some(404),
            ..
        }
    )
}

/// Whether an answer carries the entry's own media type: an answer at
/// `/api/v2` in any other (a proxy's page, a fallback route's record) is
/// no v2 answer, read as the server answering no v2 (D-243 §7; the
/// proposed decision this build files).
pub(crate) fn is_shaped(response: &reqwest::Response, media: &str) -> bool {
    response
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.trim().to_ascii_lowercase().starts_with(media))
}

/// The refusal a v2 answer of another shape reads as: the `404` of a
/// server answering no v2.
pub(crate) fn unshaped() -> CliError {
    CliError::Api {
        status: Some(404),
        error: "not_found".to_string(),
        context: None,
    }
}

/// The `ETag` exactly as answered.
fn tag_of(response: &reqwest::Response) -> Option<String> {
    response
        .headers()
        .get(reqwest::header::ETAG)
        .and_then(|v| v.to_str().ok())
        .map(|v| v.trim().to_string())
}

// ---- head ----------------------------------------------------------------

/// The v2 head's answer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HeadAnswer {
    /// `304`: the head is the commit sent.
    NotModified,
    /// The head, none where no version stands.
    Head {
        commit: Option<String>,
        version: Option<u32>,
    },
}

#[derive(Deserialize)]
struct HeadBody {
    commit: Option<String>,
    version: Option<u32>,
}

/// `GET …/head`, `If-None-Match` naming `held` where given; resent as
/// 277's reads are (D-243 §9).
pub async fn head(
    client: &SynsClient,
    address: &str,
    token: Option<&str>,
    held: Option<&str>,
) -> Result<HeadAnswer, CliError> {
    let mut request = client.v2_get(address, "head", &[], token);
    if let Some(commit) = held {
        request = request.header(reqwest::header::IF_NONE_MATCH, format!("\"{commit}\""));
    }
    let response = send_bounded(request, 0).await?;
    if response.status() == reqwest::StatusCode::NOT_MODIFIED && held.is_some() {
        return Ok(HeadAnswer::NotModified);
    }
    let response = check_response(response).await?;
    if !is_shaped(&response, "application/json") {
        return Err(unshaped());
    }
    let status = response.status();
    let bytes = read_body(response, ANSWER_STALL, None).await?;
    let shaped = serde_json::from_slice::<serde_json::Value>(&bytes)
        .ok()
        .and_then(|v| v.as_object().cloned())
        .is_some_and(|o| o.contains_key("commit") && o.contains_key("version"));
    if !shaped {
        return Err(unshaped());
    }
    let body: HeadBody = serde_json::from_slice(&bytes).map_err(|e| undecodable(status, e))?;
    if body
        .commit
        .as_deref()
        .is_some_and(|c| !crate::read::store::is_blob_hash(c))
    {
        return Err(undecodable(status, "the head names no commit hash"));
    }
    Ok(HeadAnswer::Head {
        commit: body.commit,
        version: body.version,
    })
}

// ---- the closure -----------------------------------------------------------

/// The closure's answer.
#[derive(Debug, Clone)]
pub enum TreeAnswer {
    /// `304`: the tag sent stands.
    NotModified,
    /// The closure, checked whole, and its tag.
    Closure { tag: String, closure: Closure },
}

/// `GET …/tree?at=…&path=…&closure=1&format=lines`, checked whole and
/// resent under the bulk budgets (D-243 §3, §6). `root_path` is the path
/// the first block names: the scope at a holder address, `""` at a
/// folder's.
#[allow(clippy::too_many_arguments)]
pub async fn closure(
    client: &SynsClient,
    reads: Option<&RunReads>,
    address: &str,
    token: Option<&str>,
    at: &str,
    path: &str,
    root_path: &str,
    if_none_match: Option<&str>,
) -> Result<TreeAnswer, CliError> {
    bulk(reads, || async {
        let mut query = vec![("at", at)];
        if !path.is_empty() {
            query.push(("path", path));
        }
        query.extend([("closure", "1"), ("format", "lines")]);
        let mut request = client.v2_get(address, "tree", &query, token);
        if let Some(tag) = if_none_match {
            request = request.header(reqwest::header::IF_NONE_MATCH, tag);
        }
        let response = match send_unretried(request, 0).await {
            Ok(response) => response,
            Err(err) => return unsent(err),
        };
        let status = response.status();
        if status == reqwest::StatusCode::NOT_MODIFIED && if_none_match.is_some() {
            return Attempt::Done(TreeAnswer::NotModified);
        }
        if !status.is_success() {
            return refused(response).await;
        }
        if !is_shaped(&response, "text/plain") {
            return Attempt::Final(unshaped());
        }
        let Some(tag) = tag_of(&response) else {
            return Attempt::Final(undecodable(status, "the closure carries no tag"));
        };
        let body = match read_body(response, ANSWER_STALL, None).await {
            Ok(body) => body,
            Err(err) => return unsent(err),
        };
        match Closure::parse(&body, root_path) {
            Ok(closure) => Attempt::Done(TreeAnswer::Closure { tag, closure }),
            Err(refused) => Attempt::Failed {
                err: undecodable(status, refused),
                progress: false,
            },
        }
    })
    .await
}

// ---- changes ---------------------------------------------------------------

/// One end of a `changes` answer.
#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub struct End {
    pub commit: String,
    pub version: Option<u32>,
}

/// One entry of a `changes` answer.
#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct Change {
    pub path: String,
    #[serde(default)]
    pub old_path: Option<String>,
    pub status: String,
    #[serde(default)]
    pub old_sha: Option<String>,
    #[serde(default)]
    pub new_sha: Option<String>,
    #[serde(default)]
    pub size: Option<u64>,
}

/// A `changes` answer.
#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub struct Changes {
    pub from: End,
    pub to: End,
    pub entries: Vec<Change>,
}

/// `GET …/changes?from=…&to=…`, and `&path=…` where `path` is given
/// (roadmap 224), under the bulk budgets.
pub async fn changes(
    client: &SynsClient,
    reads: Option<&RunReads>,
    address: &str,
    token: Option<&str>,
    from: &str,
    to: &str,
    path: Option<&str>,
) -> Result<Changes, CliError> {
    bulk(reads, || async {
        let mut query = vec![("from", from), ("to", to)];
        if let Some(path) = path {
            query.push(("path", path));
        }
        let request = client.v2_get(address, "changes", &query, token);
        let response = match send_unretried(request, 0).await {
            Ok(response) => response,
            Err(err) => return unsent(err),
        };
        let status = response.status();
        if !status.is_success() {
            return refused(response).await;
        }
        if !is_shaped(&response, "application/json") {
            return Attempt::Final(unshaped());
        }
        let body = match read_body(response, ANSWER_STALL, None).await {
            Ok(body) => body,
            Err(err) => return unsent(err),
        };
        match serde_json::from_slice::<Changes>(&body) {
            Ok(changes) => Attempt::Done(changes),
            Err(_) => Attempt::Final(unshaped()),
        }
    })
    .await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::client::RETRY_AFTER_CAP;

    // Row 23.
    #[test]
    fn a_wait_carries_up_to_half_again_of_itself() {
        let wait = Duration::from_secs(1);
        let draws: Vec<Duration> = (0..100).map(|_| jittered(wait)).collect();
        assert!(draws.iter().all(|d| *d >= wait && *d <= wait.mul_f64(1.5)));
        assert!(draws.iter().any(|d| *d != draws[0]));
        assert!(RETRY_AFTER_CAP == Duration::from_secs(10));
    }

    #[tokio::test]
    async fn four_failures_end_a_read_and_progress_costs_none() {
        let mut sends = 0;
        let result: Result<(), CliError> = bulk(None, || {
            sends += 1;
            let progress = sends <= 6;
            async move {
                Attempt::Failed {
                    err: CliError::ServerUnreachable {
                        url: "http://x".into(),
                    },
                    progress,
                }
            }
        })
        .await;
        assert!(result.is_err());
        // Six progress cuts, then four failures.
        assert_eq!(sends, 10);
    }

    // REVIEW-syns-cli I2: a cut that made progress after the ceiling since
    // the first send is resent; the ceiling counts from the last progress.
    #[tokio::test]
    async fn progress_resets_the_resend_ceiling() {
        let mut sends = 0;
        let result: Result<u32, CliError> = bulk(None, || {
            sends += 1;
            let n = sends;
            async move {
                // Each send outlives the test build's ceiling of 1 s.
                tokio::time::sleep(RESEND_WITHIN + Duration::from_millis(100)).await;
                match n {
                    1 | 2 => Attempt::Failed {
                        err: CliError::ServerUnreachable {
                            url: "http://x".into(),
                        },
                        progress: true,
                    },
                    _ => Attempt::Done(n),
                }
            }
        })
        .await;
        assert_eq!(result.unwrap(), 3);
    }

    // Row 21a, at a hundredth of the scale: waits of R draw on a budget
    // of 6 R, so 4 to 6 resends are made.
    #[tokio::test]
    async fn waits_draw_on_their_budget_per_read() {
        let mut sends = 0;
        let started = std::time::Instant::now();
        let result: Result<(), CliError> = bulk_within(None, Duration::from_millis(600), || {
            sends += 1;
            async {
                Attempt::Wait {
                    wait: Duration::from_millis(100),
                    err: CliError::Api {
                        status: Some(503),
                        error: "engine_busy".into(),
                        context: None,
                    },
                }
            }
        })
        .await;
        assert!(result.is_err());
        assert!((5..=7).contains(&sends), "{sends}");
        assert!(started.elapsed() <= Duration::from_millis(700));
    }

    // Row 21a: twenty waits cost no failure send.
    #[tokio::test]
    async fn waits_cost_no_failure_send() {
        let mut sends = 0;
        let result: Result<u32, CliError> = bulk_within(None, Duration::from_secs(60), || {
            sends += 1;
            let n = sends;
            async move {
                if n <= 20 {
                    Attempt::Wait {
                        wait: Duration::from_millis(5),
                        err: CliError::Api {
                            status: Some(503),
                            error: "engine_busy".into(),
                            context: None,
                        },
                    }
                } else {
                    Attempt::Done(n)
                }
            }
        })
        .await;
        assert_eq!(result.unwrap(), 21);
    }
}
