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
                .and(path("/api/v1/repos/alice/notes/files/.syns.yaml"))
                .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                    "content": yaml, "sha": blob_sha1(yaml.as_bytes()), "size": yaml.len()
                })))
                .mount(&self.server)
                .await;
            Mock::given(method("GET"))
                .and(path("/api/v1/repos/alice/notes/files/a.md"))
                .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                    "content": "a", "sha": blob_sha1(b"a"), "size": 1
                })))
                .mount(&self.server)
                .await;
        });
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
