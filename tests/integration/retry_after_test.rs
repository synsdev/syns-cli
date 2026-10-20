//! Roadmap 277 (IN-0039, IN-0041, `D-230`) — a read the server refuses
//! `503 engine_busy` with `Retry-After` is sent again once the wait has
//! passed, so a pull meeting a busy engine finishes with every byte; a
//! write is never sent again.
//!
//! Every case drives the built binary against the stateful fake
//! `convergence_test` stands up.

use std::path::{Path, PathBuf};
use std::process::Output;
use std::time::{Duration, Instant};

use serial_test::serial;
use syns_cli::auth::token::TokenStore;
use tempfile::TempDir;

use super::convergence_test::Fake;

/// A configuration holding alice's credential, a cache, and a folder.
struct Machine {
    config: TempDir,
    cache: TempDir,
    folder: TempDir,
    uri: String,
}

impl Machine {
    fn new(uri: &str) -> Machine {
        let config = tempfile::tempdir().unwrap();
        TokenStore::new(config.path().join("credentials.json"))
            .write_with_username("test-token", Some("alice"))
            .unwrap();
        Machine {
            config,
            cache: tempfile::tempdir().unwrap(),
            folder: tempfile::tempdir().unwrap(),
            uri: uri.to_string(),
        }
    }

    fn dir(&self) -> PathBuf {
        std::fs::canonicalize(self.folder.path()).unwrap()
    }

    /// Run the binary in `cwd`, off the async runtime so the fake keeps
    /// answering.
    async fn run_in(&self, cwd: &Path, args: &[&str]) -> Output {
        let config = self.config.path().to_path_buf();
        let cache = self.cache.path().to_path_buf();
        let uri = self.uri.clone();
        let cwd = cwd.to_path_buf();
        let args: Vec<String> = args.iter().map(|a| a.to_string()).collect();
        tokio::task::spawn_blocking(move || {
            let mut full = vec!["--server".to_string(), uri];
            full.extend(args);
            assert_cmd::Command::cargo_bin("syns")
                .unwrap()
                .current_dir(cwd)
                .env("SYNS_CONFIG_DIR", config)
                .env("SYNS_CACHE_DIR", cache)
                .env_remove("SYNS_URL")
                .args(&full)
                .output()
                .unwrap()
        })
        .await
        .unwrap()
    }
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

/// Every file under `root` but the run's own state, path to bytes.
fn files_under(root: &Path) -> std::collections::BTreeMap<String, Vec<u8>> {
    let mut files = std::collections::BTreeMap::new();
    let mut pending = vec![root.to_path_buf()];
    while let Some(dir) = pending.pop() {
        for entry in std::fs::read_dir(&dir).unwrap() {
            let entry = entry.unwrap();
            let name = entry.file_name().to_string_lossy().into_owned();
            if name == ".syns" || name == ".syns.yaml" {
                continue;
            }
            let path = entry.path();
            if path.is_dir() {
                pending.push(path);
            } else {
                let relative = path.strip_prefix(root).unwrap();
                let key = relative.to_string_lossy().replace('\\', "/");
                files.insert(key, std::fs::read(&path).unwrap());
            }
        }
    }
    files
}

/// The requests to a route ending in `tail`, with when each arrived.
fn arrivals_of(fake: &Fake, method: &str, tail: &str) -> Vec<Instant> {
    fake.arrivals()
        .into_iter()
        .filter(|(m, target, _)| {
            m == method && target.split('?').next().unwrap_or(target).ends_with(tail)
        })
        .map(|(_, _, at)| at)
        .collect()
}

// X8: a raw read and a tree read refused `503 engine_busy` with
// `Retry-After: 1` are each sent again a second later, and the pull ends
// with every byte the head holds.
#[tokio::test(flavor = "multi_thread")]
#[serial]
async fn a_pull_meeting_a_busy_engine_waits_and_finishes_byte_identical() {
    let fake = Fake::start().await;
    let binary: Vec<u8> = (0..=255u8).cycle().take(70_000).collect();
    fake.commit_bytes(&[
        ("a.md", b"alpha\n"),
        ("docs/b.md", b"beta\nline two\n"),
        ("img/c.bin", &binary),
    ]);
    let m = Machine::new(&fake.uri);

    fake.busy_next("GET", "raw/img/c.bin", 1);
    let started = Instant::now();
    let out = m.run_in(&m.dir(), &["pull", "alice/proj", "copy"]).await;
    assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
    assert!(started.elapsed() >= Duration::from_secs(1));
    let (_, head) = fake.head_bytes();
    assert_eq!(files_under(&m.dir().join("copy")), head);
    let raw = arrivals_of(&fake, "GET", "raw/img/c.bin");
    assert_eq!(raw.len(), 2, "{raw:?}");
    assert!(raw[1] - raw[0] >= Duration::from_millis(990), "{raw:?}");

    // A tree read refused the same way, on a pull into the checkout.
    fake.commit_byte_changes(&[("docs/d.md", Some(b"delta\n"))]);
    fake.busy_next("GET", "tree", 1);
    let before = arrivals_of(&fake, "GET", "/tree").len();
    let out = m.run_in(&m.dir().join("copy"), &["pull"]).await;
    assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
    let (_, head) = fake.head_bytes();
    assert_eq!(files_under(&m.dir().join("copy")), head);
    let trees = arrivals_of(&fake, "GET", "/tree");
    assert!(trees.len() >= before + 2, "{trees:?}");
    assert!(trees[before + 1] - trees[before] >= Duration::from_millis(990));
}

// X8: a read refused busy at every send is sent four times, and the pull
// ends on the refusal with nothing written.
#[tokio::test(flavor = "multi_thread")]
#[serial]
async fn a_read_refused_busy_four_times_ends_the_pull() {
    let fake = Fake::start().await;
    fake.commit(&[("a.md", "alpha\n"), ("b.md", "beta\n")]);
    let m = Machine::new(&fake.uri);

    fake.busy_next("GET", "raw/b.md", 10);
    let out = m.run_in(&m.dir(), &["pull", "alice/proj", "copy"]).await;
    assert_ne!(out.status.code(), Some(0), "{}", stderr(&out));
    assert!(stderr(&out).contains("engine_busy"), "{}", stderr(&out));
    assert_eq!(arrivals_of(&fake, "GET", "raw/b.md").len(), 4);
    assert!(!m.dir().join("copy/b.md").exists());
}

// X8: a publication refused busy is not sent again.
#[tokio::test(flavor = "multi_thread")]
#[serial]
async fn a_write_refused_busy_is_sent_once() {
    let fake = Fake::start().await;
    let m = Machine::new(&fake.uri);
    std::fs::write(m.dir().join("a.md"), "alpha\n").unwrap();

    fake.busy_next("PUT", "push", 1);
    let out = m.run_in(&m.dir(), &["push", "--name", "proj"]).await;
    assert_ne!(out.status.code(), Some(0), "{}", stderr(&out));
    assert_eq!(arrivals_of(&fake, "PUT", "/push").len(), 1);
}
