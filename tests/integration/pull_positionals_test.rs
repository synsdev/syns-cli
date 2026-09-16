//! u262 — `syns pull` binds a lone path to `[PATH]`, each
//! `REPO_IDENTITY_UNKNOWN` refusal names only the remedies its invocation
//! accepts, and a path argument climbing through `..` walks for its
//! identity from the directory it names (issues 091 and 120).
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

use super::common::{SpawnOpts, default_push_response, spawn_mock_env};

const POSITIONAL_AND_PATH_LINE: &str = "name the repository as OWNER/NAME before the path, or create .syns.yaml in that directory or one above it";

/// A mock server, temporary config and cache directories, and a canonical
/// temporary working area `W`, so a relative argument joined onto the
/// child's working directory spells the same path the test expects.
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

    /// The tree and file mocks for a repository whose head holds `a.md`.
    fn mount_tree_with_a_md(&self, repo_id: &str) {
        self.runtime.block_on(async {
            Mock::given(method("GET"))
                .and(path(format!("/api/v1/repos/{repo_id}/tree")))
                .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                    "entries": [
                        {"name": "a.md", "path": "a.md", "type": "file", "size": 1, "sha": blob_sha1(b"a")}
                    ],
                    "commitSha": "4444444444444444444444444444444444444444",
                    "truncated": false
                })))
                .mount(&self.server)
                .await;
            Mock::given(method("GET"))
                .and(path(format!("/api/v1/repos/{repo_id}/files/a.md")))
                .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                    "content": "a", "sha": blob_sha1(b"a"), "size": 1
                })))
                .mount(&self.server)
                .await;
        });
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

/// A second canonical temporary directory `V` holding no identity file at
/// or above it.
fn bare_dir() -> (TempDir, PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let canonical = fs::canonicalize(dir.path()).unwrap();
    (dir, canonical)
}

#[test]
#[serial]
fn positional_repository_under_if_repo_skips_where_no_identity_file_reaches_the_path() {
    let env = Env::new();
    let (_v, v) = bare_dir();
    let elsewhere = v.join("elsewhere");

    let output = env.syns(
        &v,
        &[
            "--json",
            "pull",
            "--if-repo",
            "alice/proj",
            elsewhere.to_str().unwrap(),
        ],
    );

    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    assert_eq!(
        stdout(&output),
        "{\"skipped\":true,\"reason\":\"no_syns_repo\"}\n"
    );
    assert!(env.request_paths().is_empty());
    assert!(!elsewhere.exists());
}

#[test]
#[serial]
fn positional_repository_under_if_repo_pulls_where_an_identity_file_stands_above() {
    let env = Env::new();
    identity(&env.w, "alice", "proj");
    fs::create_dir_all(env.w.join("sub")).unwrap();
    env.mount_tree_with_a_md("alice/proj");

    let output = env.syns(&env.w.join("sub"), &["pull", "--if-repo", "alice/proj"]);

    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    let paths = env.request_paths();
    assert!(
        paths.iter().any(|p| p == "/api/v1/repos/alice/proj/tree"),
        "{paths:?}"
    );
    assert!(env.w.join("a.md").is_file());
}

#[test]
#[serial]
fn lone_relative_path_converges_that_path_from_its_identity_file() {
    let env = Env::new();
    identity(&env.w.join("target"), "alice", "proj");
    identity(&env.w.join("cwd"), "bob", "other");
    env.mount_tree_with_a_md("alice/proj");

    let output = env.syns(&env.w.join("cwd"), &["pull", "../target"]);

    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    let paths = env.request_paths();
    assert!(
        paths.iter().any(|p| p == "/api/v1/repos/alice/proj/tree"),
        "{paths:?}"
    );
    assert!(!paths.iter().any(|p| p.contains("bob/other")), "{paths:?}");
    assert!(env.w.join("target/a.md").is_file());
    let cwd_entries: Vec<_> = fs::read_dir(env.w.join("cwd"))
        .unwrap()
        .map(|e| e.unwrap().file_name().into_string().unwrap())
        .collect();
    assert_eq!(cwd_entries, vec![".syns.yaml".to_string()]);
}

#[test]
#[serial]
fn lone_absolute_path_converges_that_path() {
    let env = Env::new();
    identity(&env.w.join("target"), "alice", "proj");
    identity(&env.w.join("cwd"), "bob", "other");
    env.mount_tree_with_a_md("alice/proj");
    let target = env.w.join("target");

    let output = env.syns(&env.w.join("cwd"), &["pull", target.to_str().unwrap()]);

    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    assert!(target.join("a.md").is_file());
}

#[test]
#[serial]
fn lone_path_with_no_identity_above_refuses_naming_the_path() {
    let env = Env::new();
    identity(&env.w.join("cwd"), "alice", "proj");
    let (_v, v) = bare_dir();
    let elsewhere = v.join("elsewhere");

    let output = env.syns(&env.w.join("cwd"), &["pull", elsewhere.to_str().unwrap()]);

    assert_eq!(output.status.code(), Some(2));
    assert_eq!(
        stderr(&output),
        format!(
            "error: cannot determine repo identity for {} \u{2014} {POSITIONAL_AND_PATH_LINE}\n",
            elsewhere.display()
        )
    );
    assert!(env.request_paths().is_empty());
    assert!(!elsewhere.exists());
}

#[test]
#[serial]
fn lone_path_climbing_out_with_no_identity_above_refuses() {
    let env = Env::new();
    identity(&env.w.join("cwd"), "alice", "proj");
    let elsewhere = env.w.join("elsewhere");

    let output = env.syns(&env.w.join("cwd"), &["pull", "../elsewhere"]);

    assert_eq!(output.status.code(), Some(2));
    assert_eq!(
        stderr(&output),
        format!(
            "error: cannot determine repo identity for {} \u{2014} {POSITIONAL_AND_PATH_LINE}\n",
            elsewhere.display()
        )
    );
    assert!(env.request_paths().is_empty());
    assert!(!elsewhere.exists());
}

#[test]
#[serial]
fn lone_path_refusal_in_json_mode_is_one_document() {
    let env = Env::new();
    identity(&env.w.join("cwd"), "alice", "proj");
    let (_v, v) = bare_dir();
    let elsewhere = v.join("elsewhere");

    let output = env.syns(
        &env.w.join("cwd"),
        &["--json", "pull", elsewhere.to_str().unwrap()],
    );

    assert_eq!(output.status.code(), Some(2));
    let expected = json!({
        "error": format!(
            "cannot determine repo identity for {} \u{2014} {POSITIONAL_AND_PATH_LINE}",
            elsewhere.display()
        )
    });
    assert_eq!(stdout(&output), format!("{expected}\n"));
}

#[test]
#[serial]
fn lone_path_under_if_repo_skips_where_no_identity_file_reaches_it() {
    let env = Env::new();
    identity(&env.w.join("cwd"), "alice", "proj");
    let (_v, v) = bare_dir();
    let elsewhere = v.join("elsewhere");

    let output = env.syns(
        &env.w.join("cwd"),
        &["--json", "pull", "--if-repo", elsewhere.to_str().unwrap()],
    );

    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    assert_eq!(
        stdout(&output),
        "{\"skipped\":true,\"reason\":\"no_syns_repo\"}\n"
    );
    assert!(env.request_paths().is_empty());
}

#[test]
#[serial]
fn bare_pull_refusal_names_the_positional() {
    let env = Env::new();

    let output = env.syns(&env.w, &["pull"]);

    assert_eq!(output.status.code(), Some(2));
    assert_eq!(
        stderr(&output),
        "error: cannot determine repo identity \u{2014} name the repository as OWNER/NAME, or run inside a directory at or below one holding .syns.yaml\n"
    );
}

#[test]
#[serial]
fn malformed_first_of_two_refuses_at_the_parser() {
    let env = Env::new();

    for args in [
        &["pull", "./a", "./b"][..],
        &["--json", "pull", "./a", "./b"][..],
    ] {
        let output = env.syns(&env.w, args);

        assert_eq!(output.status.code(), Some(2), "{args:?}");
        assert_eq!(stdout(&output), "", "{args:?}");
        let err = stderr(&output);
        assert!(
            err.starts_with(
                "error: invalid value './a' for '[OWNER/NAME]': expected OWNER/NAME, or a lone PATH"
            ),
            "{err}"
        );
        assert!(
            err.contains("Usage: syns pull [OPTIONS] [OWNER/NAME] [PATH]"),
            "{err}"
        );
    }
    assert!(env.request_paths().is_empty());
    assert!(!env.w.join("b").exists());
}

#[test]
#[serial]
fn positional_repository_wins_over_the_identity_file_at_the_path() {
    let env = Env::new();
    identity(&env.w.join("target"), "bob", "other");
    fs::create_dir_all(env.w.join("cwd")).unwrap();
    env.mount_tree_with_a_md("alice/proj");

    let output = env.syns(&env.w.join("cwd"), &["pull", "Alice/Proj", "../target"]);

    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    let paths = env.request_paths();
    assert!(
        paths.iter().any(|p| p == "/api/v1/repos/alice/proj/tree"),
        "{paths:?}"
    );
    assert!(!paths.iter().any(|p| p.contains("bob/other")), "{paths:?}");
}

#[test]
#[serial]
fn push_climbing_out_with_no_identity_above_refuses_with_the_name_option_line() {
    let env = Env::new();
    identity(&env.w.join("cwd"), "alice", "proj");
    fs::create_dir_all(env.w.join("elsewhere")).unwrap();
    fs::write(env.w.join("elsewhere/a.md"), "a").unwrap();

    let output = env.syns(&env.w.join("cwd"), &["push", "../elsewhere"]);

    assert_eq!(output.status.code(), Some(2));
    assert_eq!(
        stderr(&output),
        "error: cannot determine repo identity \u{2014} provide --name or create .syns.yaml\n"
    );
    assert!(env.request_paths().is_empty());
}

#[test]
#[serial]
fn push_climbing_out_publishes_into_the_identity_above_that_path() {
    let spawn = spawn_mock_env(SpawnOpts {
        put_response: Some(default_push_response()),
        owner: Some("alice"),
        repo: Some("proj"),
        ..Default::default()
    });
    let w = fs::canonicalize(spawn.project_dir.path()).unwrap();
    identity(&w, "alice", "proj");
    fs::create_dir_all(w.join("outer/elsewhere")).unwrap();
    fs::write(w.join("outer/elsewhere/a.md"), "a").unwrap();
    identity(&w.join("outer/cwd"), "bob", "other");

    let output = AssertCommand::cargo_bin("syns")
        .expect("syns binary")
        .current_dir(w.join("outer/cwd"))
        .env("SYNS_CONFIG_DIR", spawn.config_dir.path())
        .env("SYNS_CACHE_DIR", spawn.cache_dir.path())
        .env_remove("SYNS_URL")
        .args(["--server", &spawn.mock_uri, "push", "../elsewhere"])
        .output()
        .expect("subprocess output");

    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let paths: Vec<String> = rt.block_on(async {
        spawn
            .server
            .received_requests()
            .await
            .unwrap()
            .iter()
            .map(|r| r.url.path().to_string())
            .collect()
    });
    assert!(
        paths
            .iter()
            .any(|p| p.contains("/api/v1/repos/alice/proj/")),
        "{paths:?}"
    );
    assert!(!paths.iter().any(|p| p.contains("bob/other")), "{paths:?}");
}

#[test]
#[serial]
fn status_refusal_names_the_identity_file_alone() {
    let env = Env::new();

    let output = env.syns(&env.w, &["status"]);

    assert_eq!(output.status.code(), Some(2));
    assert_eq!(
        stderr(&output),
        "error: cannot determine repo identity \u{2014} run inside a directory at or below one holding .syns.yaml\n"
    );
}

#[test]
#[serial]
fn sync_outcome_document_carries_the_identity_file_line() {
    let env = Env::new();

    let output = env.syns(&env.w, &["--json", "sync"]);

    let document: serde_json::Value =
        serde_json::from_str(stdout(&output).trim()).expect("one JSON document");
    assert_eq!(
        document["error"],
        "cannot determine repo identity \u{2014} run inside a directory at or below one holding .syns.yaml"
    );
}
