//! Binary-level behaviour of a run whose default cache root refuses its
//! writes (SPEC u298 Tests): each working copy's state kept in its
//! in-root home, one state with the default home's for every run reaching
//! the folder, and the in-root home carried by no collection and written by
//! no retrieval.
//!
//! Every run here resolves its own store roots: its default root under a
//! scratch `HOME`, `SYNS_CACHE_DIR` and `XDG_CACHE_HOME` removed, and its
//! temp directory a scratch `TMPDIR`, against the stateful loopback
//! repository `Fake` serving `alice/proj`.

#![cfg(unix)]

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Output;

use serde_json::{Value, json};
use serial_test::serial;
use syns_cli::auth::token::TokenStore;
use syns_cli::config::StoreRoots;
use syns_cli::push::collector::{CollectOptions, HELD_BYTES_BUDGET, HeldBytes, collect_files};
use syns_cli::push::hash::blob_sha1;
use syns_cli::push::working_copy::{IN_ROOT_HOME, STATE_STAMP, WorkingCopy};
use tempfile::TempDir;

use super::convergence_test::Fake;

/// One machine for the binary, every directory it reaches made under one
/// scratch directory named for the unit: a `HOME` holding the platform
/// default store root, a `TMPDIR`, a configuration holding alice's
/// credential, and a folder to run in.
struct Machine {
    _scratch: TempDir,
    base: PathBuf,
    uri: String,
}

impl Machine {
    fn new(uri: &str) -> Machine {
        let scratch = tempfile::Builder::new()
            .prefix("u298-state-home-")
            .tempdir()
            .unwrap();
        let base = std::fs::canonicalize(scratch.path()).unwrap();
        for dir in ["home", "tmp", "config", "folder"] {
            std::fs::create_dir_all(base.join(dir)).unwrap();
        }
        TokenStore::new(base.join("config/credentials.json"))
            .write_with_username("test-token", Some("alice"))
            .unwrap();
        Machine {
            _scratch: scratch,
            base,
            uri: uri.to_string(),
        }
    }

    fn dir(&self) -> PathBuf {
        self.base.join("folder")
    }

    fn temp(&self) -> PathBuf {
        self.base.join("tmp")
    }

    /// The platform default store root the binary resolves under `HOME`.
    fn default_root(&self) -> PathBuf {
        let caches = if cfg!(target_os = "macos") {
            "Library/Caches"
        } else {
            ".cache"
        };
        self.base.join("home").join(caches).join("syns")
    }

    /// The roots a run answers where the default root takes writes.
    fn writable(&self) -> StoreRoots {
        StoreRoots {
            default: self.default_root(),
            write: self.default_root(),
            default_refused: false,
        }
    }

    /// The roots a run answers where the default root refuses writes.
    fn refused(&self) -> StoreRoots {
        StoreRoots {
            default: self.default_root(),
            write: syns_cli::config::fallback_root(&self.temp()),
            default_refused: true,
        }
    }

    fn copy(&self, stores: &StoreRoots) -> WorkingCopy {
        WorkingCopy::open(stores, "alice", "proj", &self.dir()).unwrap()
    }

    /// Run the binary in `cwd`, under the Seatbelt profile `profile` where
    /// one is given, off the async runtime so the fake keeps answering.
    async fn run_in(&self, cwd: &Path, args: &[&str], profile: Option<String>) -> Output {
        let base = self.base.clone();
        let uri = self.uri.clone();
        let cwd = cwd.to_path_buf();
        let args: Vec<String> = args.iter().map(|a| a.to_string()).collect();
        tokio::task::spawn_blocking(move || {
            let binary = env!("CARGO_BIN_EXE_syns");
            let mut command = match &profile {
                Some(profile) => {
                    let mut command = std::process::Command::new("sandbox-exec");
                    command.arg("-p").arg(profile).arg(binary);
                    command
                }
                None => std::process::Command::new(binary),
            };
            command
                .current_dir(cwd)
                .env("HOME", base.join("home"))
                .env("TMPDIR", base.join("tmp"))
                .env("SYNS_CONFIG_DIR", base.join("config"))
                .env_remove("SYNS_CACHE_DIR")
                .env_remove("XDG_CACHE_HOME")
                .env_remove("SYNS_URL")
                .env_remove("SYNS_INTEGRATION")
                .env_remove("SYNS_RUN")
                .env_remove("SYNS_TRIGGER")
                .env_remove("SYNS_TASK")
                .arg("--server")
                .arg(uri)
                .args(&args)
                .stdin(std::process::Stdio::null())
                .output()
                .unwrap()
        })
        .await
        .unwrap()
    }

    async fn run(&self, args: &[&str]) -> Output {
        let dir = self.dir();
        self.run_in(&dir, args, None).await
    }
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

fn write(dir: &Path, path: &str, bytes: &[u8]) {
    let target = dir.join(path);
    std::fs::create_dir_all(target.parent().unwrap()).unwrap();
    std::fs::write(target, bytes).unwrap();
}

fn set_mode(path: &Path, mode: u32) {
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)).unwrap();
}

/// Whether the run may write a mode-`0500` directory anyway.
fn superuser() -> bool {
    unsafe { libc::geteuid() == 0 }
}

/// Every path standing under `root`, `/`-joined from it, sorted.
fn entries_under(root: &Path) -> Vec<String> {
    fn walk(root: &Path, dir: &Path, out: &mut Vec<String>) {
        for entry in std::fs::read_dir(dir).into_iter().flatten().flatten() {
            let path = entry.path();
            out.push(
                path.strip_prefix(root)
                    .unwrap()
                    .to_string_lossy()
                    .into_owned(),
            );
            if path.is_dir() && !path.is_symlink() {
                walk(root, &path, out);
            }
        }
    }
    let mut out = Vec::new();
    walk(root, root, &mut out);
    out.sort();
    out
}

/// Every directory directly under `dir` whose name is the in-root home,
/// letter case aside.
fn in_root_homes(dir: &Path) -> Vec<PathBuf> {
    std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .filter(|e| {
            e.file_name()
                .to_str()
                .is_some_and(|n| n.eq_ignore_ascii_case(IN_ROOT_HOME))
        })
        .map(|e| e.path())
        .collect()
}

fn commit_of(copy: &WorkingCopy) -> Option<String> {
    copy.base().and_then(|b| b.commit_sha().map(String::from))
}

/// The paths a push body carries content for, and the paths it deletes.
fn sent(body: &Value) -> (Vec<String>, Vec<String>) {
    let mut contents: Vec<String> = body["files"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|f| f.get("content").is_some() || f.get("contentBase64").is_some())
        .map(|f| f["path"].as_str().unwrap().to_string())
        .collect();
    contents.sort();
    let mut deletions: Vec<String> = body["deletions"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|d| d["path"].as_str().unwrap().to_string())
        .collect();
    deletions.sort();
    (contents, deletions)
}

fn identity() -> &'static str {
    "owner: alice\nname: proj\n"
}

// ---- the rows ----------------------------------------------------------

// SPEC u298 Tests, `in_root_home_is_never_collected_or_retrieved`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn in_root_home_is_never_collected_or_retrieved() {
    let fake = Fake::start().await;
    let m = Machine::new(&fake.uri);
    let dir = m.dir();
    fake.commit(&[
        (".syns.yaml", identity()),
        ("a.md", "a\n"),
        (".SYNS-STATE/z", "served\n"),
    ]);
    write(&dir, ".syns-state/base.json", b"kept\n");
    write(&dir, "x/.Syns-State/y", b"y\n");
    write(&dir, "a.md", b"a\n");

    let collected = collect_files(
        &dir,
        &[],
        CollectOptions {
            no_default_excludes: true,
            ..Default::default()
        },
        None,
        &HeldBytes::new(HELD_BYTES_BUDGET),
    )
    .unwrap();
    assert_eq!(
        collected.files.keys().cloned().collect::<Vec<_>>(),
        vec!["a.md".to_string()]
    );
    assert!(
        collected.skipped.is_empty(),
        "the in-root home reported as skipped: {:?}",
        collected.skipped
    );

    let target = dir.display().to_string();
    let out = m
        .run_in(&m.base, &["pull", "alice/proj", target.as_str()], None)
        .await;

    assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
    for home in in_root_homes(&dir) {
        assert!(
            !home.join("z").exists() && !home.join("Z").exists(),
            "a served path was written into {}",
            home.display()
        );
    }
    assert_eq!(
        std::fs::read(dir.join(".syns-state/base.json")).unwrap(),
        b"kept\n"
    );
    assert_eq!(std::fs::read(dir.join("a.md")).unwrap(), b"a\n");
}

// SPEC u298 Tests, `refused_default_run_publishes_against_the_default_homes_base`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn refused_default_run_publishes_against_the_default_homes_base() {
    if superuser() {
        return;
    }
    let fake = Fake::start().await;
    let m = Machine::new(&fake.uri);
    let dir = m.dir();
    let h1 = fake.commit(&[(".syns.yaml", identity()), ("a.md", "a\n"), ("b.md", "b\n")]);
    let target = dir.display().to_string();
    let pulled = m
        .run_in(&m.base, &["pull", "alice/proj", target.as_str()], None)
        .await;
    assert_eq!(pulled.status.code(), Some(0), "{}", stderr(&pulled));
    assert_eq!(
        commit_of(&m.copy(&m.writable())).as_deref(),
        Some(h1.as_str())
    );
    assert!(!dir.join(IN_ROOT_HOME).exists());
    write(&dir, "a.md", b"a\nedited\n");
    set_mode(&m.default_root(), 0o500);

    let out = m.run(&["push"]).await;
    set_mode(&m.default_root(), 0o700);

    assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
    let bodies = fake.push_bodies();
    assert_eq!(bodies.len(), 1, "{bodies:?}");
    assert_eq!(bodies[0]["parentSha"], json!(h1));
    assert_eq!(sent(&bodies[0]), (vec!["a.md".to_string()], Vec::new()));
    let (head, _) = fake.head();
    assert_ne!(head, h1);
    let in_root = m.copy(&m.refused());
    assert_eq!(in_root.state_dir, dir.join(IN_ROOT_HOME));
    assert_eq!(in_root.resolution().unwrap(), None);
    assert!(!dir.join(IN_ROOT_HOME).join("resolution.json").exists());
    assert_eq!(commit_of(&in_root).as_deref(), Some(head.as_str()));
    let base: Value =
        serde_json::from_slice(&std::fs::read(dir.join(IN_ROOT_HOME).join("base.json")).unwrap())
            .unwrap();
    assert_eq!(base["commit_sha"], json!(head));
}

// SPEC u298 Tests, `unsandboxed_sync_takes_up_the_in_root_state`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn unsandboxed_sync_takes_up_the_in_root_state() {
    let fake = Fake::start().await;
    let m = Machine::new(&fake.uri);
    let dir = m.dir();
    let h1 = fake.commit(&[(".syns.yaml", identity()), ("a.md", "a\n")]);
    let target = dir.display().to_string();
    let pulled = m
        .run_in(&m.base, &["pull", "alice/proj", target.as_str()], None)
        .await;
    assert_eq!(pulled.status.code(), Some(0), "{}", stderr(&pulled));
    assert_eq!(
        commit_of(&m.copy(&m.writable())).as_deref(),
        Some(h1.as_str())
    );

    // A sandboxed run published `b.md` as `h2` and recorded it in the
    // in-root home alone, stamped later than the default home.
    let h2 = fake.commit_changes(&[("b.md", Some("b\n"))]);
    let sandboxed = m.copy(&m.refused());
    sandboxed
        .record_base(
            &h2,
            fake.tree_at(&h2)
                .iter()
                .map(|(p, c)| (p.clone(), blob_sha1(c.as_bytes())))
                .collect(),
        )
        .unwrap();
    let default_stamp: u128 =
        std::fs::read_to_string(m.copy(&m.writable()).state_dir.join(STATE_STAMP))
            .unwrap()
            .trim()
            .parse()
            .unwrap();
    let in_root_stamp: u128 = std::fs::read_to_string(dir.join(IN_ROOT_HOME).join(STATE_STAMP))
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    assert!(in_root_stamp > default_stamp);
    assert!(!dir.join("b.md").exists());

    let out = m.run(&["sync"]).await;

    assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
    let bodies = fake.push_bodies();
    assert_eq!(bodies.len(), 1, "{bodies:?}");
    assert_eq!(bodies[0]["parentSha"], json!(h2));
    assert_eq!(sent(&bodies[0]), (Vec::new(), vec!["b.md".to_string()]));
    assert!(!dir.join("b.md").exists(), "b.md was written back");
    let (head, files) = fake.head();
    assert_eq!(
        files.keys().cloned().collect::<Vec<_>>(),
        vec![".syns.yaml".to_string(), "a.md".to_string()]
    );
    let default_home = m.copy(&m.writable());
    let recorded: Value =
        serde_json::from_slice(&std::fs::read(default_home.state_dir.join("base.json")).unwrap())
            .unwrap();
    assert_eq!(recorded["commit_sha"], json!(head));
}

// SPEC u298 Tests, `seatbelt_denied_cache_falls_back`.
#[cfg(target_os = "macos")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn seatbelt_denied_cache_falls_back() {
    let fake = Fake::start().await;
    let m = Machine::new(&fake.uri);
    let dir = m.dir();
    let h1 = fake.commit(&[(".syns.yaml", identity()), ("a.md", "a\n"), ("b.md", "b\n")]);
    std::fs::create_dir_all(m.default_root()).unwrap();
    let before = entries_under(&m.default_root());
    let profile = format!(
        "(version 1)(allow default)(deny file-write* (subpath \"{}\"))",
        m.default_root().display()
    );

    let target = dir.display().to_string();
    let pulled = m
        .run_in(
            &m.base,
            &["pull", "alice/proj", target.as_str()],
            Some(profile.clone()),
        )
        .await;
    assert_eq!(pulled.status.code(), Some(0), "{}", stderr(&pulled));
    assert_eq!(std::fs::read(dir.join("a.md")).unwrap(), b"a\n");
    assert_eq!(std::fs::read(dir.join("b.md")).unwrap(), b"b\n");
    write(&dir, "a.md", b"a\nedited\n");
    let pushed = m.run_in(&dir, &["push"], Some(profile)).await;

    assert_eq!(pushed.status.code(), Some(0), "{}", stderr(&pushed));
    let bodies = fake.push_bodies();
    assert_eq!(bodies.len(), 1, "{bodies:?}");
    assert_eq!(bodies[0]["parentSha"], json!(h1));
    let (head, _) = fake.head();
    let in_root = dir.join(IN_ROOT_HOME);
    assert!(in_root.join("state.lock").is_file());
    assert_eq!(
        commit_of(&m.copy(&m.refused())).as_deref(),
        Some(head.as_str())
    );
    assert_eq!(entries_under(&m.default_root()), before);
}
