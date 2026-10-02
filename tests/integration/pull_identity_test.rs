//! u263 — `syns pull OWNER/NAME` refuses wherever the nearest identity
//! file names another repository, and never marks, removes or reverts a
//! local edit to one naming its own (issue 130).
//!
//! Every test drives the `syns` binary as a subprocess against a wiremock
//! server, with temporary config and cache directories.

use assert_cmd::Command as AssertCommand;
use serde_json::json;
use serial_test::serial;
use std::fs;
use std::path::{Path, PathBuf};
use syns_cli::push::hash::blob_sha1;
use tempfile::TempDir;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

/// A mock server, temporary config and cache directories, and a canonical
/// temporary working area `W`, so a path the child prints spells the same
/// path the test expects.
struct Env {
    runtime: tokio::runtime::Runtime,
    server: MockServer,
    config_dir: TempDir,
    cache_dir: TempDir,
    _work: TempDir,
    w: PathBuf,
}

impl Env {
    fn new() -> Self {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let server = runtime.block_on(MockServer::start());
        runtime.block_on(super::common::mount_records(&server));
        let work = tempfile::tempdir().unwrap();
        let w = fs::canonicalize(work.path()).unwrap();
        Env {
            runtime,
            server,
            config_dir: tempfile::tempdir().unwrap(),
            cache_dir: tempfile::tempdir().unwrap(),
            _work: work,
            w,
        }
    }

    fn syns(&self, cwd: &Path, args: &[&str]) -> std::process::Output {
        let uri = self.server.uri();
        let mut full = vec!["--server", uri.as_str()];
        full.extend_from_slice(args);
        AssertCommand::cargo_bin("syns")
            .expect("syns binary")
            .current_dir(cwd)
            .env("SYNS_CONFIG_DIR", self.config_dir.path())
            .env("SYNS_CACHE_DIR", self.cache_dir.path())
            .env_remove("SYNS_URL")
            .args(&full)
            .output()
            .expect("subprocess output")
    }

    fn request_paths(&self) -> Vec<String> {
        self.runtime.block_on(async {
            self.server
                .received_requests()
                .await
                .unwrap()
                .iter()
                .map(|r| r.url.path().to_string())
                .collect()
        })
    }

    /// The tree and file mocks for `alice/notes`, whose head holds `a.md`
    /// reading `a` and, where `identity` is true, a `.syns.yaml` naming it.
    fn mount_notes(&self, identity: bool) {
        let yaml = "owner: alice\nname: notes\n";
        let mut entries = vec![
            json!({"name": "a.md", "path": "a.md", "type": "file", "size": 1, "sha": blob_sha1(b"a")}),
        ];
        if identity {
            entries.insert(
                0,
                json!({"name": ".syns.yaml", "path": ".syns.yaml", "type": "file", "size": yaml.len(), "sha": blob_sha1(yaml.as_bytes())}),
            );
        }
        self.runtime.block_on(async {
            Mock::given(method("GET"))
                .and(path("/api/v1/repos/alice/notes/tree"))
                .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                    "entries": entries,
                    "commitSha": "6666666666666666666666666666666666666666",
                    "truncated": false
                })))
                .mount(&self.server)
                .await;
            Mock::given(method("GET"))
                .and(path("/api/v1/repos/alice/notes/raw/.syns.yaml"))
                .respond_with(
                    ResponseTemplate::new(200)
                        .insert_header(
                            "ETag",
                            format!("\"{}\"", blob_sha1(yaml.as_bytes())).as_str(),
                        )
                        .set_body_bytes(yaml.as_bytes().to_vec()),
                )
                .mount(&self.server)
                .await;
            Mock::given(method("GET"))
                .and(path("/api/v1/repos/alice/notes/raw/a.md"))
                .respond_with(
                    ResponseTemplate::new(200)
                        .insert_header("ETag", format!("\"{}\"", blob_sha1(b"a")).as_str())
                        .set_body_bytes("a".as_bytes().to_vec()),
                )
                .mount(&self.server)
                .await;
        });
    }

    /// A stored credential in the config directory, as a write run reads
    /// it (copied from `writes_test.rs`'s `Deployment::seed_credential`).
    fn seed_credential(&self) {
        fs::write(
            self.config_dir.path().join("credentials.json"),
            json!({"token": "test-token", "username": "alice"}).to_string(),
        )
        .unwrap();
    }

    /// The binary run from `cwd` with `stdin` on its standard input, no
    /// provenance variable of the calling environment reaching it (copied
    /// from `writes_test.rs`'s `Deployment::run_in`).
    fn syns_with_stdin(&self, cwd: &Path, stdin: &[u8], args: &[&str]) -> std::process::Output {
        let uri = self.server.uri();
        let mut full = vec!["--server", uri.as_str()];
        full.extend_from_slice(args);
        AssertCommand::cargo_bin("syns")
            .expect("syns binary")
            .current_dir(cwd)
            .env("SYNS_CONFIG_DIR", self.config_dir.path())
            .env("SYNS_CACHE_DIR", self.cache_dir.path())
            .env_remove("SYNS_URL")
            .env_remove("SYNS_INTEGRATION")
            .env_remove("SYNS_RUN")
            .env_remove("SYNS_TRIGGER")
            .env_remove("SYNS_TASK")
            .args(&full)
            .write_stdin(stdin.to_vec())
            .output()
            .expect("subprocess output")
    }

    /// The repository read of `alice/notes` naming head `6666…6666`, ahead
    /// of the record every repository answers, and a push there answering
    /// version `2` with one file changed (copied from `writes_test.rs`'s
    /// `mount_repo` and `mount_push`).
    fn mount_notes_push(&self) {
        self.runtime.block_on(async {
            Mock::given(method("GET"))
                .and(path("/api/v1/repos/alice/notes"))
                .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                    "owner": "alice", "name": "notes", "description": null,
                    "commitSha": PULLED_HEAD, "status": "active", "author": null,
                    "tags": [], "visibility": "private", "forkedFrom": null,
                    "forkCount": 0, "fileCount": 1, "role": "owner",
                    "sharedFolder": false,
                    "createdAt": "2026-01-01T00:00:00Z",
                    "updatedAt": "2026-01-01T00:00:00Z",
                })))
                .with_priority(1)
                .mount(&self.server)
                .await;
            Mock::given(method("PUT"))
                .and(path("/api/v1/repos/alice/notes/push"))
                .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                    "commitSha": "7777777777777777777777777777777777777777",
                    "version": 2, "filesChanged": 1, "created": false,
                })))
                .mount(&self.server)
                .await;
        });
    }

    /// Every push body the server received, in order (copied from
    /// `writes_test.rs`'s `Deployment::pushes`).
    fn pushes(&self) -> Vec<serde_json::Value> {
        self.runtime.block_on(async {
            self.server
                .received_requests()
                .await
                .unwrap()
                .iter()
                .filter(|r| r.url.path().ends_with("/push"))
                .map(|r| serde_json::from_slice(&r.body).expect("push body"))
                .collect()
        })
    }

    fn cache_is_empty(&self) -> bool {
        fs::read_dir(self.cache_dir.path())
            .unwrap()
            .next()
            .is_none()
    }
}

fn identity(dir: &Path, owner: &str, name: &str) {
    fs::create_dir_all(dir).unwrap();
    fs::write(
        dir.join(".syns.yaml"),
        format!("owner: {owner}\nname: {name}\n"),
    )
    .unwrap();
}

fn stderr(output: &std::process::Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

fn stdout(output: &std::process::Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

/// The path-belongs line naming `dir` as the directory whose identity file
/// names `bob/other`.
fn belongs_line(dir: &Path) -> String {
    format!(
        "{} already belongs to bob/other \u{2014} pull alice/notes into another directory, or remove {}",
        dir.display(),
        dir.join(".syns.yaml").display()
    )
}

fn is_empty_dir(dir: &Path) -> bool {
    fs::read_dir(dir).unwrap().next().is_none()
}

/// `W/c/.syns.yaml` naming `bob/other` above an empty `W/c/sub`.
fn seed_c(env: &Env) -> PathBuf {
    let c = env.w.join("c");
    identity(&c, "bob", "other");
    fs::create_dir_all(c.join("sub")).unwrap();
    c
}

/// `W/d/.syns.yaml` spelling `alice/notes` in another letter case, with no
/// base recorded.
const LETTER_CASE_IDENTITY: &str = "owner: Alice\nname: Notes\n";

fn seed_d(env: &Env) -> PathBuf {
    let d = env.w.join("d");
    fs::create_dir_all(&d).unwrap();
    fs::write(d.join(".syns.yaml"), LETTER_CASE_IDENTITY).unwrap();
    d
}

#[test]
#[serial]
fn bare_pull_inside_another_repository_refuses_before_any_request() {
    let env = Env::new();
    let a = env.w.join("a");
    identity(&a, "bob", "other");
    fs::write(a.join("standing.md"), "standing").unwrap();
    env.mount_notes(false);

    let output = env.syns(&a, &["pull", "alice/notes"]);

    assert_eq!(output.status.code(), Some(2), "{}", stderr(&output));
    assert_eq!(stderr(&output), format!("error: {}\n", belongs_line(&a)));
    assert!(env.request_paths().is_empty());
    assert_eq!(fs::read_dir(&a).unwrap().count(), 2);
    assert_eq!(
        fs::read_to_string(a.join(".syns.yaml")).unwrap(),
        "owner: bob\nname: other\n"
    );
    assert_eq!(
        fs::read_to_string(a.join("standing.md")).unwrap(),
        "standing"
    );
    assert!(env.cache_is_empty());
}

#[test]
#[serial]
fn bare_pull_from_a_sub_folder_refuses_naming_the_ancestor() {
    let env = Env::new();
    let c = seed_c(&env);
    env.mount_notes(false);

    let output = env.syns(&c.join("sub"), &["pull", "alice/notes"]);

    assert_eq!(output.status.code(), Some(2), "{}", stderr(&output));
    assert_eq!(stderr(&output), format!("error: {}\n", belongs_line(&c)));
    assert!(env.request_paths().is_empty());
    assert!(is_empty_dir(&c.join("sub")));
}

#[test]
#[serial]
fn path_pull_beneath_another_repository_refuses_naming_the_ancestor() {
    let env = Env::new();
    let b = env.w.join("b");
    identity(&b, "bob", "other");
    env.mount_notes(false);

    let output = env.syns(&b, &["pull", "alice/notes", "sub"]);

    assert_eq!(output.status.code(), Some(2), "{}", stderr(&output));
    assert_eq!(stderr(&output), format!("error: {}\n", belongs_line(&b)));
    assert!(env.request_paths().is_empty());
    assert!(!b.join("sub").exists());
}

#[test]
#[serial]
fn refusal_holds_under_every_mode_and_option() {
    let env = Env::new();
    let c = seed_c(&env);
    env.mount_notes(false);
    let line = belongs_line(&c);

    for args in [
        &["--json", "pull", "alice/notes"][..],
        &["pull", "--if-repo", "alice/notes"][..],
        &["pull", "--version", "1", "alice/notes"][..],
        &["pull", "--overwrite", "alice/notes"][..],
    ] {
        let output = env.syns(&c.join("sub"), args);

        assert_eq!(
            output.status.code(),
            Some(2),
            "{args:?}: {}",
            stderr(&output)
        );
        if args[0] == "--json" {
            assert_eq!(stdout(&output), format!("{}\n", json!({ "error": line })));
        } else {
            assert_eq!(stderr(&output), format!("error: {line}\n"), "{args:?}");
        }
        assert!(env.request_paths().is_empty(), "{args:?}");
        assert!(is_empty_dir(&c.join("sub")), "{args:?}");
    }
    assert!(env.cache_is_empty());
}

#[test]
#[serial]
fn nearer_identity_naming_the_positional_admits_the_pull() {
    let env = Env::new();
    identity(&env.w, "bob", "other");
    identity(&env.w.join("sub"), "alice", "notes");
    fs::create_dir_all(env.w.join("sub/deep")).unwrap();
    env.mount_notes(false);

    let first = env.syns(&env.w, &["pull", "alice/notes", "sub"]);
    assert_eq!(first.status.code(), Some(0), "{}", stderr(&first));
    let second = env.syns(&env.w.join("sub/deep"), &["pull", "alice/notes"]);
    assert_eq!(second.status.code(), Some(0), "{}", stderr(&second));

    let paths = env.request_paths();
    assert!(
        paths.iter().any(|p| p == "/api/v1/repos/alice/notes/tree"),
        "{paths:?}"
    );
    assert!(paths.iter().all(|p| !p.contains("bob/other")), "{paths:?}");
    assert_eq!(fs::read_to_string(env.w.join("sub/a.md")).unwrap(), "a");
    assert!(is_empty_dir(&env.w.join("sub/deep")));
}

#[test]
#[serial]
fn marked_nearest_identity_is_judged_by_its_local_side() {
    let env = Env::new();
    let c = env.w.join("c");
    fs::create_dir_all(c.join("sub")).unwrap();
    let marked = "<<<<<<< local\nowner: bob\nname: other\n=======\nowner: alice\nname: notes\n>>>>>>> remote\n";
    fs::write(c.join(".syns.yaml"), marked).unwrap();
    env.mount_notes(false);

    let output = env.syns(&c.join("sub"), &["pull", "alice/notes"]);

    assert_eq!(output.status.code(), Some(2), "{}", stderr(&output));
    assert_eq!(stderr(&output), format!("error: {}\n", belongs_line(&c)));
    assert!(env.request_paths().is_empty());
    assert_eq!(fs::read_to_string(c.join(".syns.yaml")).unwrap(), marked);
}

#[test]
#[serial]
fn marked_ancestor_naming_the_positional_admits_a_path_pull() {
    let env = Env::new();
    fs::write(
        env.w.join(".syns.yaml"),
        "<<<<<<< local\nowner: alice\nname: notes\n=======\nowner: bob\nname: other\n>>>>>>> remote\n",
    )
    .unwrap();
    env.mount_notes(false);

    let output = env.syns(&env.w, &["pull", "alice/notes", "sub"]);

    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    assert!(env.w.join("sub/a.md").is_file());
    assert!(!env.w.join("sub/.syns.yaml").exists());
}

#[test]
#[serial]
fn malformed_ancestor_identity_refuses_before_any_request() {
    let env = Env::new();
    fs::write(env.w.join(".syns.yaml"), "owner: [alice").unwrap();
    env.mount_notes(false);

    let output = env.syns(&env.w, &["pull", "alice/notes", "sub"]);

    assert_eq!(output.status.code(), Some(1), "{}", stderr(&output));
    assert!(
        stderr(&output).starts_with("error: invalid .syns.yaml: "),
        "{}",
        stderr(&output)
    );
    assert!(env.request_paths().is_empty());
    assert!(!env.w.join("sub").exists());
}

#[test]
#[serial]
fn letter_case_identity_file_is_left_as_it_stands() {
    let env = Env::new();
    let d = seed_d(&env);
    env.mount_notes(true);

    let first = env.syns(&env.w, &["pull", "alice/notes", "d"]);
    assert_eq!(first.status.code(), Some(0), "{}", stderr(&first));
    assert!(
        stdout(&first).contains("Pulled alice/notes: 1 downloaded, 1 unchanged, 0 deleted"),
        "{}",
        stdout(&first)
    );
    let second = env.syns(&env.w, &["pull", "alice/notes", "d"]);
    assert_eq!(second.status.code(), Some(0), "{}", stderr(&second));
    assert!(
        stdout(&second).contains("0 downloaded"),
        "{}",
        stdout(&second)
    );

    for output in [&first, &second] {
        assert!(
            !stderr(output).contains("resolution required"),
            "{}",
            stderr(output)
        );
    }
    assert_eq!(fs::read_to_string(d.join("a.md")).unwrap(), "a");
    assert_eq!(
        fs::read_to_string(d.join(".syns.yaml")).unwrap(),
        LETTER_CASE_IDENTITY
    );
}

#[test]
#[serial]
fn version_retrieval_leaves_a_standing_identity_file() {
    let env = Env::new();
    let d = seed_d(&env);
    env.mount_notes(true);

    let output = env.syns(
        &env.w,
        &["--json", "pull", "--version", "1", "alice/notes", "d"],
    );

    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    let document: serde_json::Value =
        serde_json::from_str(stdout(&output).trim()).expect("one JSON document");
    assert_eq!(document["downloaded"], 1, "{document}");
    assert_eq!(document["excluded"], json!([]), "{document}");
    assert_eq!(fs::read_to_string(d.join("a.md")).unwrap(), "a");
    assert_eq!(
        fs::read_to_string(d.join(".syns.yaml")).unwrap(),
        LETTER_CASE_IDENTITY
    );
}

#[test]
#[serial]
fn overwrite_leaves_a_standing_identity_file() {
    let env = Env::new();
    let d = seed_d(&env);
    fs::write(d.join("a.md"), "local").unwrap();
    env.mount_notes(true);

    let output = env.syns(&env.w, &["pull", "--overwrite", "alice/notes", "d"]);

    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    assert_eq!(fs::read_to_string(d.join("a.md")).unwrap(), "a");
    assert_eq!(
        fs::read_to_string(d.join(".syns.yaml")).unwrap(),
        LETTER_CASE_IDENTITY
    );
}

#[test]
#[serial]
fn version_retrieval_counts_an_ignored_standing_identity_file_once() {
    let env = Env::new();
    let d = seed_d(&env);
    fs::write(d.join(".synsignore"), "*.yaml\n").unwrap();
    env.mount_notes(true);

    let output = env.syns(
        &env.w,
        &["--json", "pull", "--version", "1", "alice/notes", "d"],
    );

    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    let document: serde_json::Value =
        serde_json::from_str(stdout(&output).trim()).expect("one JSON document");
    assert_eq!(document["downloaded"], 1, "{document}");
    assert_eq!(document["excluded"], json!([]), "{document}");
    assert_eq!(
        fs::read_to_string(d.join(".syns.yaml")).unwrap(),
        LETTER_CASE_IDENTITY
    );
}

// ---- u303: writes from a fresh pull -----------------------------------

/// The head `mount_notes` serves.
const PULLED_HEAD: &str = "6666666666666666666666666666666666666666";

/// The checkout-guard refusal naming `root`, as the diagnostic stream
/// carries it.
fn guard_refusal(root: &Path) -> String {
    format!(
        "error: the checkout at {} holds unpublished local changes for alice/notes; edit those files instead \u{2014} they publish at the end of the turn \u{2014} or publish them with syns sync, then write again\n",
        root.display()
    )
}

/// `mount_notes` with or without an identity file, the repository read
/// and push of `alice/notes`, and a stored credential; `W/co` stands
/// nowhere.
fn fresh_pull_env(identity: bool) -> Env {
    let env = Env::new();
    env.mount_notes(identity);
    env.mount_notes_push();
    env.seed_credential();
    env
}

// SPEC u303 Tests, `writes_from_a_fresh_pull_of_a_head_holding_no_identity_file_land`.
#[test]
#[serial]
fn writes_from_a_fresh_pull_of_a_head_holding_no_identity_file_land() {
    let env = fresh_pull_env(false);
    let co = env.w.join("co");

    let pull = env.syns_with_stdin(&env.w, b"", &["pull", "alice/notes", "co"]);
    assert_eq!(pull.status.code(), Some(0), "{}", stderr(&pull));
    let identity_bytes = fs::read(co.join(".syns.yaml")).unwrap();
    assert_eq!(identity_bytes, b"owner: alice\nname: notes\n");

    let rm = env.syns_with_stdin(&co, b"", &["rm", "a.md", "--parent", PULLED_HEAD]);
    assert_eq!(rm.status.code(), Some(0), "{}", stderr(&rm));
    let diagnostics = stderr(&rm);
    let lines: Vec<&str> = diagnostics.lines().collect();
    assert_eq!(
        lines.last().copied(),
        Some(
            format!(
                "the checkout at {} is now one version behind; syns sync converges it",
                co.display()
            )
            .as_str()
        ),
        "{diagnostics}"
    );
    assert!(
        lines.len() >= 2 && lines[lines.len() - 2].starts_with("wrote version 2, commit"),
        "{diagnostics}"
    );

    let write = env.syns_with_stdin(
        &co,
        b"x",
        &["--json", "write", "b.md", "--parent", PULLED_HEAD],
    );
    assert_eq!(write.status.code(), Some(0), "{}", stderr(&write));
    let document: serde_json::Value = serde_json::from_str(stdout(&write).trim()).unwrap();
    assert_eq!(document["checkoutBehind"], json!(co.display().to_string()));

    let pushes = env.pushes();
    assert_eq!(pushes.len(), 2, "each write sends one push");
    for push in &pushes {
        assert_eq!(push["parentSha"], json!(PULLED_HEAD));
    }
    assert_eq!(fs::read(co.join(".syns.yaml")).unwrap(), identity_bytes);
    assert_eq!(fs::read(co.join("a.md")).unwrap(), b"a");
}

// SPEC u303 Tests, `a_fresh_pull_with_a_local_edit_still_refuses_a_write`.
#[test]
#[serial]
fn a_fresh_pull_with_a_local_edit_still_refuses_a_write() {
    let env = fresh_pull_env(false);
    let co = env.w.join("co");

    let pull = env.syns_with_stdin(&env.w, b"", &["pull", "alice/notes", "co"]);
    assert_eq!(pull.status.code(), Some(0), "{}", stderr(&pull));
    fs::write(co.join("a.md"), "b").unwrap();

    let rm = env.syns_with_stdin(&co, b"", &["rm", "a.md", "--parent", PULLED_HEAD]);
    assert_eq!(rm.status.code(), Some(1), "{}", stderr(&rm));
    assert_eq!(stderr(&rm), guard_refusal(&co));
    assert!(env.pushes().is_empty(), "no push request");
}

// SPEC u303 Tests, `an_edited_identity_file_a_pulled_head_carried_still_refuses_a_write`.
#[test]
#[serial]
fn an_edited_identity_file_a_pulled_head_carried_still_refuses_a_write() {
    let env = fresh_pull_env(true);
    let co = env.w.join("co");

    let pull = env.syns_with_stdin(&env.w, b"", &["pull", "alice/notes", "co"]);
    assert_eq!(pull.status.code(), Some(0), "{}", stderr(&pull));
    let mut identity = fs::read_to_string(co.join(".syns.yaml")).unwrap();
    assert_eq!(identity, "owner: alice\nname: notes\n");
    identity.push_str("checks:\n  - make test\n");
    fs::write(co.join(".syns.yaml"), identity).unwrap();

    let rm = env.syns_with_stdin(&co, b"", &["rm", "a.md", "--parent", PULLED_HEAD]);
    assert_eq!(rm.status.code(), Some(1), "{}", stderr(&rm));
    assert_eq!(stderr(&rm), guard_refusal(&co));
    assert!(env.pushes().is_empty(), "no push request");
}
