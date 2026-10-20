//! Roadmap 271 — engine admission lanes: `syns pull` and `syns sync` send
//! `Syns-Lane: bulk` on every request to `EP-tree` and `EP-file-read-bytes`
//! (C-1), and `syns push`, its convergence reads included, and every other
//! verb send no `Syns-Lane` (C-2).
//!
//! Every case drives the built binary against the stateful fake
//! `convergence_test` stands up, which records each request's `Syns-Lane`.

use std::path::{Path, PathBuf};
use std::process::Output;

use serial_test::serial;
use syns_cli::auth::token::TokenStore;
use tempfile::TempDir;

use super::convergence_test::Fake;

/// One machine's worth of state for the binary: a configuration holding
/// alice's credential, a cache, and a folder to run in.
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

    /// Run the binary in `cwd`, off the async runtime so the fake served on
    /// it keeps answering.
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

    async fn run(&self, args: &[&str]) -> Output {
        let dir = self.dir();
        self.run_in(&dir, args).await
    }
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

/// A request to `EP-tree` or `EP-file-read-bytes`.
fn is_read(target: &str) -> bool {
    let route = target.split('?').next().unwrap_or(target);
    let Some(rest) = route.strip_prefix("/api/v1/repos/") else {
        return false;
    };
    let tail = rest.splitn(3, '/').nth(2).unwrap_or("");
    tail == "tree" || tail.starts_with("tree/") || tail.starts_with("raw/")
}

type Recorded = Vec<(String, String, Option<String>)>;

/// C-1: every tree and content read carries `bulk`, at least one was made,
/// and no other request carries a lane.
fn assert_bulk_reads(verb: &str, recorded: &Recorded) {
    let reads: Vec<_> = recorded.iter().filter(|(_, t, _)| is_read(t)).collect();
    assert!(
        !reads.is_empty(),
        "{verb} made no tree or raw read: {recorded:?}"
    );
    for (method, target, lane) in &reads {
        assert_eq!(
            lane.as_deref(),
            Some("bulk"),
            "{verb}: {method} {target} carried {lane:?}"
        );
    }
    for (method, target, lane) in recorded.iter().filter(|(_, t, _)| !is_read(t)) {
        assert_eq!(lane, &None, "{verb}: {method} {target} carried {lane:?}");
    }
}

/// C-2: no request carries a lane.
fn assert_no_lane(verb: &str, recorded: &Recorded) {
    assert!(!recorded.is_empty(), "{verb} sent nothing");
    for (method, target, lane) in recorded {
        assert_eq!(lane, &None, "{verb}: {method} {target} carried {lane:?}");
    }
}

#[tokio::test(flavor = "multi_thread")]
#[serial]
async fn pull_and_sync_send_their_reads_bulk_and_every_other_verb_sends_no_lane() {
    let fake = Fake::start().await;
    let m = Machine::new(&fake.uri);
    std::fs::write(m.dir().join("a.md"), "alpha\n").unwrap();
    std::fs::create_dir_all(m.dir().join("docs")).unwrap();
    std::fs::write(m.dir().join("docs/b.md"), "beta\nline two\n").unwrap();

    // C-2: a push, its convergence reads included.
    let out = m.run(&["push", "--name", "proj"]).await;
    assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
    assert_no_lane("push", &fake.lanes(true));
    std::fs::write(m.dir().join("a.md"), "alpha two\n").unwrap();
    let out = m.run(&["push"]).await;
    assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
    let pushed = fake.lanes(true);
    assert!(pushed.iter().any(|(_, t, _)| is_read(t)), "{pushed:?}");
    assert_no_lane("push over a head", &pushed);

    // C-1: a sync converging a moved head, then one publishing an edit.
    fake.commit_changes(&[("docs/c.md", Some("gamma\n"))]);
    let out = m.run(&["sync"]).await;
    assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
    assert_bulk_reads("sync", &fake.lanes(true));
    assert_eq!(
        std::fs::read_to_string(m.dir().join("docs/c.md")).unwrap(),
        "gamma\n"
    );
    std::fs::write(m.dir().join("a.md"), "alpha three\n").unwrap();
    let out = m.run(&["sync"]).await;
    assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
    let published = fake.lanes(true);
    assert!(
        published.iter().any(|(m, _, _)| m == "PUT"),
        "{published:?}"
    );
    assert_bulk_reads("sync publishing", &published);

    // C-1: a pull into a new directory, then into the checkout it made.
    let other = Machine::new(&fake.uri);
    let out = other.run(&["pull", "alice/proj", "copy"]).await;
    assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
    assert_bulk_reads("pull into a new directory", &fake.lanes(true));
    let checkout = other.dir().join("copy");
    assert_eq!(
        std::fs::read_to_string(checkout.join("docs/c.md")).unwrap(),
        "gamma\n"
    );
    fake.commit_changes(&[("docs/d.md", Some("delta\n"))]);
    let out = other.run_in(&checkout, &["pull"]).await;
    assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
    assert_bulk_reads("pull into a checkout", &fake.lanes(true));
    assert_eq!(
        std::fs::read_to_string(checkout.join("docs/d.md")).unwrap(),
        "delta\n"
    );

    // C-2: `resolution` is its own verb: a continued resolution's reads
    // carry no lane, though the sync that prepared it sent its own bulk.
    fake.commit_changes(&[("a.md", Some("remote edit\n"))]);
    std::fs::write(m.dir().join("a.md"), "local edit\n").unwrap();
    let out = m.run(&["sync"]).await;
    assert_eq!(out.status.code(), Some(4), "{}", stderr(&out));
    assert_bulk_reads("sync meeting a collision", &fake.lanes(true));
    std::fs::write(m.dir().join("a.md"), "resolved\n").unwrap();
    let out = m.run(&["resolution", "continue"]).await;
    assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
    let continued = fake.lanes(true);
    assert!(
        continued.iter().any(|(_, t, _)| is_read(t)),
        "{continued:?}"
    );
    assert!(
        continued.iter().any(|(m, _, _)| m == "PUT"),
        "{continued:?}"
    );
    assert_no_lane("resolution continue", &continued);

    // C-2: the reading verbs, whatever they answer.
    for args in [
        &["cat", "a.md"][..],
        &["ls"][..],
        &["read", "docs/b.md"][..],
        &["place", "alice/proj", "placed"][..],
    ] {
        let _ = other.run_in(&checkout, args).await;
        assert_no_lane(args[0], &fake.lanes(true));
    }
}
