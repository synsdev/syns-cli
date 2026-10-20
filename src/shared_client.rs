//! The one HTTP client a run's requests to `BND-public-api` share
//! (roadmap 277, LAT-19): one connection pool per server and runtime, so a
//! run pays one TLS handshake and, over HTTP/2, carries every request on
//! one connection.
//!
//! The release host's client (`syns upgrade`) is built apart from this
//! one and shares nothing with it.

use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::task::{Context, Poll};

use hyper_util::client::legacy::connect::Connection;
use reqwest::redirect::Policy;

use crate::client::{ACCEPT_ENCODING, REQUEST_CEILING, request_deadline};
use crate::errors::CliError;

type BoxError = Box<dyn std::error::Error + Send + Sync>;

/// One shared client: the runtime driving its connections, the server it
/// talks to, the client, and its handshake gate.
struct Shared {
    runtime: tokio::runtime::Id,
    origin: String,
    client: reqwest::Client,
    gate: Arc<HandshakeGate>,
}

/// The clients this process shares, one per tokio runtime and server: a
/// pooled connection is driven by the runtime that opened it, so a client
/// is never handed to another runtime, whose requests would meet
/// connections no live runtime drives. A run uses one runtime and one
/// server; a test binary runs a runtime per test, the oldest client
/// dropped past `SHARED_KEPT`.
static SHARED: Mutex<Vec<Shared>> = Mutex::new(Vec::new());

/// The most clients `SHARED` keeps.
const SHARED_KEPT: usize = 32;

/// The scheme, host and port of `url`, as the clients are keyed by.
fn origin_of(url: &str) -> String {
    match url::Url::parse(url) {
        Ok(parsed) => parsed.origin().ascii_serialization(),
        Err(_) => url.to_string(),
    }
}

/// The one client every request of this run to the server at
/// `server_url` goes through: built once per runtime and server, and
/// shared by every `SynsClient`, sign-in and sign-out. Called off any
/// runtime, it answers a client of its own.
pub(crate) fn api_client(server_url: &str) -> Result<reqwest::Client, CliError> {
    let Ok(runtime) = tokio::runtime::Handle::try_current() else {
        return build_api_client(crate::tls::tls_config()?, Arc::default());
    };
    let (runtime, origin) = (runtime.id(), origin_of(server_url));
    let mut shared = SHARED.lock().unwrap_or_else(PoisonError::into_inner);
    if let Some(kept) = shared
        .iter()
        .find(|kept| kept.runtime == runtime && kept.origin == origin)
    {
        return Ok(kept.client.clone());
    }
    let gate = Arc::<HandshakeGate>::default();
    let client = build_api_client(crate::tls::tls_config()?, gate.clone())?;
    keep(&mut shared, runtime, origin, client.clone(), gate);
    Ok(client)
}

fn keep(
    shared: &mut Vec<Shared>,
    runtime: tokio::runtime::Id,
    origin: String,
    client: reqwest::Client,
    gate: Arc<HandshakeGate>,
) {
    shared.retain(|kept| kept.runtime != runtime || kept.origin != origin);
    if shared.len() >= SHARED_KEPT {
        shared.remove(0);
    }
    shared.push(Shared {
        runtime,
        origin,
        client,
        gate,
    });
}

/// Shares a client built over `tls` as this runtime's for `server_url`,
/// in place of the one `api_client` would build: a test's client
/// trusting the test root.
#[cfg(test)]
pub(crate) fn share_for_this_runtime(server_url: &str, tls: rustls::ClientConfig) {
    let gate = Arc::<HandshakeGate>::default();
    let client = build_api_client(tls, gate.clone()).unwrap();
    let runtime = tokio::runtime::Handle::current().id();
    let mut shared = SHARED.lock().unwrap_or_else(PoisonError::into_inner);
    keep(&mut shared, runtime, origin_of(server_url), client, gate);
}

/// The runtimes a client for `server_url` is kept for, oldest first.
#[cfg(test)]
pub(crate) fn kept_runtimes(server_url: &str) -> Vec<tokio::runtime::Id> {
    let origin = origin_of(server_url);
    SHARED
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .iter()
        .filter(|kept| kept.origin == origin)
        .map(|kept| kept.runtime)
        .collect()
}

/// Marks an answer from the server at `url` arrived on this runtime's
/// shared client, the signal its handshake gate waits on.
pub(crate) fn answered(url: &reqwest::Url) {
    let Ok(runtime) = tokio::runtime::Handle::try_current() else {
        return;
    };
    let (runtime, origin) = (runtime.id(), url.origin().ascii_serialization());
    let gate = SHARED
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .iter()
        .find(|kept| kept.runtime == runtime && kept.origin == origin)
        .map(|kept| kept.gate.clone());
    if let Some(gate) = gate {
        gate.answered();
    }
}

/// The one client builder every client of `BND-public-api` is built by:
/// the build's `User-Agent` with the caller `SYNS_CALLER` names, the
/// install id, both compressions accepted, no redirect followed,
/// `REQUEST_CEILING` as its only client-wide bound, and `gate` before its
/// connects.
///
/// SPEC u298: built over `tls`, the one TLS configuration every client of
/// `BND-public-api` shares.
pub(crate) fn build_api_client(
    tls: rustls::ClientConfig,
    gate: Arc<HandshakeGate>,
) -> Result<reqwest::Client, CliError> {
    use reqwest::header::{HeaderMap, HeaderValue};
    let mut headers = HeaderMap::new();
    headers.insert(
        reqwest::header::ACCEPT_ENCODING,
        HeaderValue::from_static(ACCEPT_ENCODING),
    );
    if let Some(id) = install_id_dir()
        .and_then(|dir| crate::client_identity::install_id(&dir))
        .and_then(|id| HeaderValue::from_str(&id).ok())
    {
        headers.insert(crate::client_identity::INSTALL_ID_HEADER, id);
    }
    reqwest::Client::builder()
        .tls_backend_preconfigured(tls)
        .user_agent(crate::client_identity::process_user_agent())
        .default_headers(headers)
        .timeout(REQUEST_CEILING)
        .redirect(Policy::none())
        .connector_layer(GateLayer(gate))
        .build()
        .map_err(|e| CliError::Config {
            message: e.to_string(),
        })
}

/// The directory the install id is kept in. A unit test of this crate
/// keeps one only where it named its own `SYNS_CONFIG_DIR`, so the test
/// run leaves the machine's configuration untouched.
fn install_id_dir() -> Option<std::path::PathBuf> {
    if cfg!(test) {
        return std::env::var_os("SYNS_CONFIG_DIR")
            .filter(|dir| !dir.is_empty())
            .map(std::path::PathBuf::from);
    }
    crate::config::config_dir().ok()
}

/// A client's first connects, one at a time, so the requests a run sends
/// at once before it holds any connection pay one TLS handshake.
///
/// The pool cannot know before a handshake that the server speaks
/// HTTP/2, so every request sent at once on an empty pool would open a
/// connection of its own, and all but one are thrown away once ALPN
/// settles on `h2`. The gate lets the first connect through alone. Where
/// it settled on `h2`, the pool hands that one connection to every
/// request waiting on it, and each connect queued behind it runs on with
/// nothing awaiting it: it waits for the first answer on that connection
/// and ends there, opening none. Where no answer comes within the
/// deadline it connects after all, as it does wherever the first connect
/// settled on HTTP/1.1 or failed. Every later connect passes straight
/// through.
#[derive(Default)]
pub(crate) struct HandshakeGate {
    /// Held while the first connect runs; true once it has ended.
    first: tokio::sync::Mutex<bool>,
    /// The first connect has ended.
    settled: AtomicBool,
    /// The first connect settled on HTTP/2.
    h2: AtomicBool,
    /// An answer arrived since the first connect settled on HTTP/2.
    has_answered: AtomicBool,
    answers: tokio::sync::Notify,
}

impl HandshakeGate {
    fn answered(&self) {
        if self.h2.load(Ordering::Acquire) && !self.has_answered.swap(true, Ordering::AcqRel) {
            self.answers.notify_waiters();
        }
    }

    async fn first_answer(&self) {
        let notified = self.answers.notified();
        tokio::pin!(notified);
        notified.as_mut().enable();
        if self.has_answered.load(Ordering::Acquire) {
            return;
        }
        notified.await;
    }
}

#[derive(Clone)]
struct GateLayer(Arc<HandshakeGate>);

impl<S> tower_layer::Layer<S> for GateLayer {
    type Service = Gated<S>;

    fn layer(&self, inner: S) -> Gated<S> {
        Gated {
            inner,
            gate: self.0.clone(),
        }
    }
}

#[derive(Clone)]
struct Gated<S> {
    inner: S,
    gate: Arc<HandshakeGate>,
}

impl<S, R> tower_service::Service<R> for Gated<S>
where
    S: tower_service::Service<R, Error = BoxError> + Clone + Send + 'static,
    S::Response: Connection + Send,
    S::Future: Send + 'static,
    R: Send + 'static,
{
    type Response = S::Response;
    type Error = BoxError;
    type Future = Pin<Box<dyn Future<Output = Result<S::Response, BoxError>> + Send>>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), BoxError>> {
        self.inner.poll_ready(cx)
    }

    fn call(&mut self, dst: R) -> Self::Future {
        // The service polled ready is the one that connects.
        let ready = self.inner.clone();
        let mut inner = std::mem::replace(&mut self.inner, ready);
        let gate = self.gate.clone();
        Box::pin(async move {
            if !gate.settled.load(Ordering::Acquire) {
                let mut first = gate.first.lock().await;
                if !*first {
                    let connected = inner.call(dst).await;
                    if let Ok(io) = &connected
                        && io.connected().is_negotiated_h2()
                    {
                        gate.h2.store(true, Ordering::Release);
                    }
                    *first = true;
                    gate.settled.store(true, Ordering::Release);
                    return connected;
                }
                drop(first);
                if gate.h2.load(Ordering::Acquire)
                    && tokio::time::timeout(request_deadline(0), gate.first_answer())
                        .await
                        .is_ok()
                {
                    return Err("the run's HTTP/2 connection carries this request".into());
                }
            }
            inner.call(dst).await
        })
    }
}
