//! One run's v2 reads (roadmap 279): the store they read through, the
//! names this run read, the switch a v2 `404` throws to v1, and the one
//! window of `Retry-After` waits every request of the run draws on
//! (D-243 §3, §4, §7).

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use crate::config::Config;
use crate::read::store::{Store, StoreSpot};
use crate::read::view::Ignore;

/// The internal variable scaling every bulk wait and budget of a run, in
/// percent (tests; unset or unparsed reads 100).
pub const WAIT_SCALE_ENV: &str = "_INTERNAL_SYNS_WAIT_PERCENT";

/// The internal variable adding seconds to the clock a view's age is read
/// against (tests: the 24-hour refresh, row 33a).
pub const CLOCK_ENV: &str = "_INTERNAL_SYNS_CLOCK_OFFSET";

/// `duration` scaled by `WAIT_SCALE_ENV`.
pub fn scaled(duration: Duration) -> Duration {
    let percent = std::env::var(WAIT_SCALE_ENV)
        .ok()
        .and_then(|v| v.trim().parse::<u32>().ok())
        .unwrap_or(100)
        // At least 1 %: zero waits and budgets would resend without end
        // (REVIEW-syns-cli minor 1).
        .max(1);
    duration * percent / 100
}

/// Seconds since the epoch, moved by `CLOCK_ENV`.
pub fn now_secs() -> u64 {
    let real = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or_default();
    let offset = std::env::var(CLOCK_ENV)
        .ok()
        .and_then(|v| v.trim().parse::<i64>().ok())
        .unwrap_or(0);
    real.saturating_add_signed(offset)
}

/// The run window of `Retry-After` waits: 120 s of wall time from the
/// run's first such wait (D-243 §3).
pub const RUN_WAIT_WINDOW: Duration = Duration::from_secs(120);

/// The view a run's head was read from, for the restart a view-carried
/// path answered `not_found` asks (D-243 §2).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ViewHead {
    pub address: String,
    pub scope: String,
    /// The base commit's view, and the head's it derived.
    pub commit: String,
    pub derived: String,
}

/// One run's v2 reads.
#[derive(Debug)]
pub struct RunReads {
    spot: Option<StoreSpot>,
    store: Mutex<Option<Arc<Store>>>,
    created: AtomicBool,
    v1: AtomicBool,
    /// Each (address, path as sent) and the hash this run's own closure,
    /// closure `304` or `changes` named for it (D-243 §4).
    named: Mutex<HashMap<(String, String), String>>,
    window: Mutex<Option<Instant>>,
    view_head: Mutex<Option<ViewHead>>,
    restart: AtomicBool,
    /// The holder's root `.synsignore` this run read for a folder copy:
    /// the address, the commit and what was found.
    ignore: Mutex<Option<(String, String, Ignore)>>,
}

impl RunReads {
    /// The reads of a run signed in with `token` against `config`'s
    /// server, its store opened once a v2 answer needs it.
    pub fn new(config: &Config, token: Option<&str>) -> RunReads {
        RunReads::with_spot(Some(StoreSpot::of(config, token)))
    }

    /// The reads of a run with no store (an unusable cache root).
    pub fn with_spot(spot: Option<StoreSpot>) -> RunReads {
        RunReads {
            spot,
            store: Mutex::new(None),
            created: AtomicBool::new(false),
            v1: AtomicBool::new(false),
            named: Mutex::new(HashMap::new()),
            window: Mutex::new(None),
            view_head: Mutex::new(None),
            restart: AtomicBool::new(false),
            ignore: Mutex::new(None),
        }
    }

    /// Whether a v2 `404` sent the run to v1 (D-243 §7).
    pub fn on_v1(&self) -> bool {
        self.v1.load(Ordering::Relaxed)
    }

    /// Sends every later read of the run down v1.
    pub fn go_v1(&self) {
        self.v1.store(true, Ordering::Relaxed);
    }

    /// The store as it stands, none where it was never made or cannot be
    /// used: a lookup makes nothing.
    pub fn existing_store(&self) -> Option<Arc<Store>> {
        let mut held = self.store.lock().unwrap_or_else(PoisonError::into_inner);
        if held.is_none() {
            *held = self.spot.as_ref()?.open(false).map(Arc::new);
        }
        held.clone()
    }

    /// The store, made where absent; none where it cannot be used.
    pub fn store(&self) -> Option<Arc<Store>> {
        let mut held = self.store.lock().unwrap_or_else(PoisonError::into_inner);
        if held.is_none() && !self.created.swap(true, Ordering::Relaxed) {
            *held = self.spot.as_ref()?.open(true).map(Arc::new);
        }
        held.clone()
    }

    /// Records that this run read `sha` for `path` at `address`.
    pub fn name(&self, address: &str, path: &str, sha: &str) {
        self.named
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert((address.to_string(), path.to_string()), sha.to_string());
    }

    /// The hash this run read for `path` at `address`, none where it read
    /// none.
    pub fn named(&self, address: &str, path: &str) -> Option<String> {
        self.named
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get(&(address.to_string(), path.to_string()))
            .cloned()
    }

    /// Whether a resend after `wait` may still start within the run's
    /// window, its clock started at the run's first wait.
    pub fn window_admits(&self, wait: Duration) -> bool {
        let mut window = self.window.lock().unwrap_or_else(PoisonError::into_inner);
        let start = *window.get_or_insert_with(Instant::now);
        start.elapsed() + wait <= scaled(RUN_WAIT_WINDOW)
    }

    /// Records the view the run's head was read from.
    pub fn set_view_head(&self, head: Option<ViewHead>) {
        *self
            .view_head
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = head;
    }

    /// The view the run's head was read from.
    pub fn view_head(&self) -> Option<ViewHead> {
        self.view_head
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    /// Asks the head read again from the closure: a path the view carried
    /// was answered `not_found` (D-243 §2).
    pub fn ask_restart(&self) {
        self.restart.store(true, Ordering::Relaxed);
    }

    /// Takes the restart asked, if one was.
    pub fn take_restart(&self) -> bool {
        self.restart.swap(false, Ordering::Relaxed)
    }

    /// Records the holder's root `.synsignore` this run read.
    pub fn set_ignore(&self, address: &str, commit: &str, found: Ignore) {
        *self.ignore.lock().unwrap_or_else(PoisonError::into_inner) =
            Some((address.to_string(), commit.to_string(), found));
    }

    /// What this run read of the holder's `.synsignore` for `address` at
    /// `commit`, not known where it read none there.
    pub fn ignore_at(&self, address: &str, commit: &str) -> Ignore {
        match &*self.ignore.lock().unwrap_or_else(PoisonError::into_inner) {
            Some((a, c, found)) if a == address && c == commit => found.clone(),
            _ => Ignore::Unknown,
        }
    }
}

impl Drop for RunReads {
    /// The run's end: hint updates merged and one sweep where the store
    /// stands past its cap (D-242 §5).
    fn drop(&mut self) {
        if let Some(store) = self
            .store
            .get_mut()
            .unwrap_or_else(PoisonError::into_inner)
            .take()
        {
            store.finish();
        }
    }
}
