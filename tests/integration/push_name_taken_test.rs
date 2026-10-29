//! Issue 386: a bare `syns push --name` from a folder the repository
//! never stood in — no identity file naming it, no base recorded — is
//! refused where that repository already holds a commit, before any
//! write, rather than converging the folder with its head. A name that
//! is free, a repository holding no commit, and a folder already linked
//! to the repository publish as before.

use std::path::{Path, PathBuf};
use std::process::Output;

use serde_json::{Value, json};
use serial_test::serial;
use syns_cli::auth::token::TokenStore;
use tempfile::TempDir;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use super::convergence_test::Fake;

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

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

fn write(dir: &Path, path: &str, bytes: &[u8]) {
    let target = dir.join(path);
    std::fs::create_dir_all(target.parent().unwrap()).unwrap();
    std::fs::write(target, bytes).unwrap();
}

/// Every entry of `dir`, recursively, as sorted relative paths.
fn listing(dir: &Path) -> Vec<String> {
    let mut found = Vec::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(at) = stack.pop() {
        for entry in std::fs::read_dir(&at).unwrap() {
            let entry = entry.unwrap();
            let path = entry.path();
            found.push(
                path.strip_prefix(dir)
                    .unwrap()
                    .to_string_lossy()
                    .replace('\\', "/"),
            );
            if path.is_dir() {
                stack.push(path);
            }
        }
    }
    found.sort();
    found
}

const TAKEN: &str = "alice/proj already exists \u{2014} to work in it here, run syns pull alice/proj .; to start a new repository, push with another name";

/// The issue's Observed run: a fresh folder pushing under a name whose
/// repository another session made is refused, on the diagnostic stream
/// at exit 1, with nothing written into the folder and nothing sent.
#[tokio::test(flavor = "multi_thread")]
#[serial]
async fn a_fresh_folder_pushing_a_taken_name_is_refused_and_left_alone() {
    let fake = Fake::start().await;
    fake.commit(&[("board/board.json", "{}\n")]);
    let m = Machine::new(&fake.uri);
    write(&m.dir(), "a.md", b"a\n");

    let out = m
        .run(&["push", "-n", "proj", "--visibility", "private"])
        .await;

    assert_eq!(out.status.code(), Some(1), "{}", stderr(&out));
    assert_eq!(stderr(&out), format!("error: {TAKEN}\n"));
    assert_eq!(stdout(&out), "");
    assert_eq!(listing(&m.dir()), vec!["a.md"], "the folder is untouched");
    assert!(fake.push_bodies().is_empty(), "nothing was published");
}

/// The same refusal under `--json`, `code` beside the line, and for an
/// owner-qualified name.
#[tokio::test(flavor = "multi_thread")]
#[serial]
async fn the_taken_name_refusal_reads_as_name_taken_under_json() {
    let fake = Fake::start().await;
    fake.commit(&[("b.md", "b\n")]);
    let m = Machine::new(&fake.uri);
    write(&m.dir(), "a.md", b"a\n");

    let out = m.run(&["push", "-n", "alice/proj", "--json"]).await;

    assert_eq!(out.status.code(), Some(1), "{}", stderr(&out));
    let document: Value = serde_json::from_str(stdout(&out).trim())
        .unwrap_or_else(|e| panic!("no one document ({e}): {}", stdout(&out)));
    assert_eq!(document, json!({"code": "name_taken", "error": TAKEN}));
    assert_eq!(listing(&m.dir()), vec!["a.md"]);
    assert!(fake.push_bodies().is_empty());
}

/// A repository holding no commit — as `syns repo create` leaves it —
/// takes its first publication from a fresh folder.
#[tokio::test(flavor = "multi_thread")]
#[serial]
async fn a_repository_holding_no_commit_takes_a_fresh_folders_push() {
    let fake = Fake::start().await;
    let m = Machine::new(&fake.uri);
    write(&m.dir(), "a.md", b"a\n");

    let out = m.run(&["push", "-n", "proj"]).await;

    assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
    assert_eq!(fake.push_bodies().len(), 1);
    assert!(m.dir().join(".syns.yaml").is_file());
}

/// A name no repository holds creates one, as before.
#[tokio::test(flavor = "multi_thread")]
#[serial]
async fn a_free_name_still_creates_the_repository() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/v1/repos/alice/proj/tree"))
        .respond_with(ResponseTemplate::new(404).set_body_json(json!({"error": "not_found"})))
        .mount(&server)
        .await;
    Mock::given(method("PUT"))
        .and(path("/api/v1/repos/alice/proj/push"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "commitSha": "c1c1c1c1c1c1c1c1c1c1c1c1c1c1c1c1c1c1c1c1",
            "version": 1, "filesChanged": 1, "created": true,
        })))
        .mount(&server)
        .await;
    let m = Machine::new(&server.uri());
    write(&m.dir(), "a.md", b"a\n");

    let out = m.run(&["push", "-n", "proj"]).await;

    assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
    let puts = server
        .received_requests()
        .await
        .unwrap()
        .iter()
        .filter(|r| r.method == reqwest::Method::PUT)
        .count();
    assert_eq!(puts, 1);
}

/// A folder whose identity file names the repository is linked to it:
/// `--name` there converges as a bare push does, from the folder and from
/// one under it.
#[tokio::test(flavor = "multi_thread")]
#[serial]
async fn a_folder_linked_by_its_identity_file_pushes_with_the_name() {
    let fake = Fake::start().await;
    fake.commit(&[
        (".syns.yaml", "owner: alice\nname: proj\n"),
        ("a.md", "a\n"),
    ]);
    let m = Machine::new(&fake.uri);
    write(&m.dir(), ".syns.yaml", b"owner: alice\nname: proj\n");
    write(&m.dir(), "a.md", b"a\n");

    let out = m.run(&["push", "-n", "proj"]).await;
    assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
    assert!(!stderr(&out).contains("already exists"));

    let sub = m.dir().join("sub");
    write(&sub, "s.md", b"s\n");
    let deeper = m.run_in(&sub, &["push", "-n", "proj"]).await;
    assert_eq!(deeper.status.code(), Some(0), "{}", stderr(&deeper));
    assert_eq!(fake.push_bodies().len(), 1, "the new file published");
}

/// A folder that already published to the repository records a base:
/// its identity file gone, `--name` publishes as before and writes the
/// file back.
#[tokio::test(flavor = "multi_thread")]
#[serial]
async fn a_folder_recording_a_base_pushes_with_the_name() {
    let fake = Fake::start().await;
    let m = Machine::new(&fake.uri);
    write(&m.dir(), "a.md", b"a\n");
    let first = m.run(&["push", "-n", "proj"]).await;
    assert_eq!(first.status.code(), Some(0), "{}", stderr(&first));
    std::fs::remove_file(m.dir().join(".syns.yaml")).unwrap();
    write(&m.dir(), "a.md", b"a changed\n");

    let again = m.run(&["push", "-n", "proj"]).await;

    assert_eq!(again.status.code(), Some(0), "{}", stderr(&again));
    assert_eq!(fake.push_bodies().len(), 2);
    assert!(m.dir().join(".syns.yaml").is_file());
}

/// `--force` and a path argument keep their own registered behaviour.
#[tokio::test(flavor = "multi_thread")]
#[serial]
async fn forced_and_path_scoped_pushes_are_not_refused() {
    let fake = Fake::start().await;
    fake.commit(&[("b.md", "b\n")]);
    let m = Machine::new(&fake.uri);
    write(&m.dir(), "a.md", b"a\n");

    let forced = m.run(&["push", "--force", "-n", "proj"]).await;

    assert_eq!(forced.status.code(), Some(0), "{}", stderr(&forced));
    assert!(!stderr(&forced).contains("already exists"));

    assert_eq!(fake.push_bodies().len(), 1);

    let fake = Fake::start().await;
    fake.commit(&[("b.md", "b\n")]);
    let other = Machine::new(&fake.uri);
    write(&other.dir(), "b.md", b"b\n");
    write(&other.dir(), "c.md", b"c\n");
    let scoped = other.run(&["push", ".", "-n", "proj"]).await;

    assert_eq!(scoped.status.code(), Some(0), "{}", stderr(&scoped));
    assert!(!stderr(&scoped).contains("already exists"));
    assert_eq!(fake.push_bodies().len(), 1);
}

/// A blank `--name` names nothing: the identity comes from the identity
/// file as on a bare run, so the folder is linked and converges.
#[tokio::test(flavor = "multi_thread")]
#[serial]
async fn a_blank_name_takes_the_identity_file_and_is_not_refused() {
    let fake = Fake::start().await;
    fake.commit(&[
        (".syns.yaml", "owner: alice\nname: proj\n"),
        ("a.md", "a\n"),
    ]);
    let m = Machine::new(&fake.uri);
    write(&m.dir(), ".syns.yaml", b"owner: alice\nname: proj\n");
    write(&m.dir(), "a.md", b"a\n");

    let out = m.run(&["push", "-n", "  "]).await;

    assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
    assert!(!stderr(&out).contains("already exists"));
}

/// A digit-led name is refused like any other once taken.
#[tokio::test(flavor = "multi_thread")]
#[serial]
async fn a_digit_led_taken_name_is_refused() {
    let fake = Fake::start().await;
    fake.commit(&[("b.md", "b\n")]);
    let m = Machine::new(&fake.uri);
    write(&m.dir(), "a.md", b"a\n");

    let out = m.run(&["push", "-n", "2026-plan"]).await;

    assert_eq!(out.status.code(), Some(1), "{}", stderr(&out));
    assert!(
        stderr(&out).starts_with("error: alice/2026-plan already exists"),
        "{}",
        stderr(&out)
    );
    assert_eq!(listing(&m.dir()), vec!["a.md"]);
}
