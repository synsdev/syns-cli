//! Binary-level behaviour of the four writing verbs, the checkout guard,
//! the parent spellings and the forced publication (SPEC u271 Tests).
//!
//! Every row of that table stands here under the name the table gives
//! it. Each test drives one mock deployment of its own, so two tests
//! never share a store, a credential or a working directory.

use assert_cmd::Command as AssertCommand;
use serde_json::{Value, json};
use serial_test::serial;
use std::path::Path;
use syns_cli::push::collector::{CollectOptions, HeldBytes, collect_files};
use syns_cli::push::hash::blob_sha1;
use syns_cli::push::manifest::Manifest;
use syns_cli::push::working_copy::WorkingCopy;
use tempfile::TempDir;
use wiremock::matchers::{method, path as path_matcher};
use wiremock::{Mock, MockServer, Request, ResponseTemplate};

const REPO: &str = "alice/notes";
const HEAD_SHA: &str = "aa11bb22cc33dd44ee55ff6600778899001122bb";
const HEAD_PREFIX: &str = "aa11bb2";
const MOVED_SHA: &str = "cc33dd44ee55ff6600778899001122bbaa11bb22";
const NEXT_SHA: &str = "dd44ee55ff6600778899001122bbaa11bb22cc33";
const OLD_SHA: &str = "bb00bb00bb00bb00bb00bb00bb00bb00bb00bb00";
const HEAD_VERSION: u32 = 7;

/// One mock deployment, one config directory holding a credential, one
/// cache directory and one working directory — all derived from the test
/// that made them.
struct Deployment {
    rt: tokio::runtime::Runtime,
    server: MockServer,
    home: TempDir,
    cache: TempDir,
    work: TempDir,
}

impl Deployment {
    fn new() -> Deployment {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime");
        let server = rt.block_on(MockServer::start());
        let home = tempfile::tempdir().expect("config dir");
        let deployment = Deployment {
            rt,
            server,
            home,
            cache: tempfile::tempdir().expect("cache dir"),
            work: tempfile::tempdir().expect("working dir"),
        };
        deployment.seed_credential();
        deployment
    }

    fn seed_credential(&self) {
        let credentials = self.home.path().join("credentials.json");
        std::fs::write(
            &credentials,
            json!({"token": "test-token", "username": "alice"}).to_string(),
        )
        .expect("credential");
    }

    fn mount(&self, mock: Mock) {
        self.rt.block_on(async { mock.mount(&self.server).await });
    }

    fn requests(&self) -> Vec<Request> {
        self.rt
            .block_on(async { self.server.received_requests().await.expect("requests") })
    }

    fn paths(&self) -> Vec<String> {
        self.requests()
            .iter()
            .map(|r| r.url.path().to_string())
            .collect()
    }

    /// Every push body this deployment received, in order.
    fn pushes(&self) -> Vec<Value> {
        self.requests()
            .iter()
            .filter(|r| r.url.path().ends_with("/push"))
            .map(|r| serde_json::from_slice(&r.body).expect("push body"))
            .collect()
    }

    fn version_calls(&self) -> Vec<String> {
        self.requests()
            .iter()
            .filter(|r| r.url.path().contains("/versions/"))
            .map(|r| {
                r.url
                    .path()
                    .rsplit('/')
                    .next()
                    .unwrap_or_default()
                    .to_string()
            })
            .collect()
    }

    fn run(&self, args: &[&str]) -> std::process::Output {
        self.run_in(self.work.path(), &[], b"", args)
    }

    fn run_with_stdin(&self, stdin: &[u8], args: &[&str]) -> std::process::Output {
        self.run_in(self.work.path(), &[], stdin, args)
    }

    fn run_in(
        &self,
        cwd: &Path,
        env: &[(&str, Option<&str>)],
        stdin: &[u8],
        args: &[&str],
    ) -> std::process::Output {
        let mut command = AssertCommand::cargo_bin("syns").expect("syns binary");
        command
            .current_dir(cwd)
            .env("SYNS_CONFIG_DIR", self.home.path())
            .env("SYNS_CACHE_DIR", self.cache.path())
            .env_remove("SYNS_URL")
            .env_remove("SYNS_INTEGRATION")
            .env_remove("SYNS_RUN")
            .env_remove("SYNS_TRIGGER")
            .env_remove("SYNS_TASK");
        for (name, value) in env {
            match value {
                Some(value) => command.env(name, value),
                None => command.env_remove(name),
            };
        }
        command
            .arg("--server")
            .arg(self.server.uri())
            .args(args)
            .write_stdin(stdin.to_vec())
            .output()
            .expect("run syns")
    }
}

fn repo_body(repo: &str, commit_sha: &str) -> Value {
    let (owner, name) = repo.split_once('/').expect("owner/name");
    json!({
        "owner": owner, "name": name, "description": null,
        "commitSha": commit_sha, "status": "active", "author": null, "tags": [],
        "visibility": "public", "forkedFrom": null, "forkCount": 0,
        "fileCount": 3, "role": null,
        "createdAt": "2026-01-01T00:00:00Z", "updatedAt": "2026-01-01T00:00:00Z",
    })
}

fn version_body(version: u32, sha: &str) -> Value {
    json!({
        "version": version, "sha": sha, "parentSha": null, "message": "m",
        "messageBody": null, "author": "alice",
        "createdAt": "2026-01-01T00:00:00Z", "filesChanged": ["a.md"],
    })
}

fn file_body(content: &str) -> Value {
    json!({ "content": content, "sha": blob_sha1(content.as_bytes()), "size": content.len() })
}

fn push_body(commit_sha: &str, version: u32, files_changed: u32) -> Value {
    json!({
        "commitSha": commit_sha, "version": version,
        "filesChanged": files_changed, "created": false,
    })
}

/// The repository read every write makes before any body carrying a
/// commit leaves (`resolve_write_target` 4).
fn mount_repo(d: &Deployment, repo: &str, commit_sha: &str) {
    d.mount(
        Mock::given(method("GET"))
            .and(path_matcher(format!("/api/v1/repos/{repo}")))
            .respond_with(ResponseTemplate::new(200).set_body_json(repo_body(repo, commit_sha))),
    );
}

fn mount_push(d: &Deployment, repo: &str, body: Value) {
    d.mount(
        Mock::given(method("PUT"))
            .and(path_matcher(format!("/api/v1/repos/{repo}/push")))
            .respond_with(ResponseTemplate::new(200).set_body_json(body)),
    );
}

fn mount_file(d: &Deployment, repo: &str, path: &str, content: &str) {
    d.mount(
        Mock::given(method("GET"))
            .and(path_matcher(format!("/api/v1/repos/{repo}/files/{path}")))
            .respond_with(ResponseTemplate::new(200).set_body_json(file_body(content))),
    );
}

fn stdout_of(output: &std::process::Output) -> String {
    String::from_utf8_lossy(&output.stdout).to_string()
}

fn stderr_of(output: &std::process::Output) -> String {
    String::from_utf8_lossy(&output.stderr).to_string()
}

fn exit_of(output: &std::process::Output) -> i32 {
    output.status.code().expect("an exit code")
}

fn one_document(output: &std::process::Output) -> Value {
    serde_json::from_str(stdout_of(output).trim()).expect("one document on the primary stream")
}

/// A folder tracking `repo` at `root`, with a working copy whose
/// recorded base is the folder as it stands where `record` is set.
fn seed_checkout(d: &Deployment, root: &Path, repo: &str, record: bool) {
    let (owner, name) = repo.split_once('/').expect("owner/name");
    std::fs::write(
        root.join(".syns.yaml"),
        format!("owner: {owner}\nname: {name}\n"),
    )
    .expect("identity file");
    std::fs::write(root.join("a.md"), "keep one").expect("a file");
    if !record {
        return;
    }
    let copy = WorkingCopy::open(d.cache.path(), owner, name, root).expect("working copy");
    copy.record_base(HEAD_SHA, folder_hashes(root))
        .expect("recorded base");
}

/// Every path standing under this repository's working-copy state.
fn state_entries(d: &Deployment, repo: &str) -> Vec<String> {
    let (owner, name) = repo.split_once('/').expect("owner/name");
    let mut out = Vec::new();
    fn walk(dir: &Path, out: &mut Vec<String>) {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        for entry in entries.flatten() {
            out.push(entry.path().display().to_string());
            walk(&entry.path(), out);
        }
    }
    walk(
        &d.cache.path().join("working-copies").join(owner).join(name),
        &mut out,
    );
    out.sort();
    out
}

fn folder_hashes(root: &Path) -> std::collections::HashMap<String, String> {
    collect_files(
        root,
        &[],
        CollectOptions::default(),
        None,
        &HeldBytes::new(0),
    )
    .expect("collected")
    .hashes()
}

// ---- edit --------------------------------------------------------------

#[test]
#[serial]
fn edit_at_the_head_publishes_one_commit_changing_that_path() {
    let d = Deployment::new();
    mount_repo(&d, REPO, HEAD_SHA);
    mount_file(&d, REPO, "a.md", "keep one");
    mount_push(&d, REPO, push_body(NEXT_SHA, 8, 1));

    let out = d.run(&[
        "edit", "a.md", "--old", "one", "--new", "two", "--parent", HEAD_SHA, "--repo", REPO,
    ]);

    assert_eq!(exit_of(&out), 0, "{}", stderr_of(&out));
    let pushes = d.pushes();
    assert_eq!(pushes.len(), 1);
    assert_eq!(pushes[0]["parentSha"], json!(HEAD_SHA));
    assert_eq!(pushes[0]["files"].as_array().unwrap().len(), 1);
    assert_eq!(pushes[0]["files"][0]["path"], json!("a.md"));
    assert_eq!(pushes[0]["files"][0]["content"], json!("keep two"));
    assert!(pushes[0].get("deletions").is_none());
    assert!(pushes[0].get("author").is_none());
    assert!(
        d.version_calls().is_empty(),
        "a full-hash parent makes no version request"
    );
}

#[test]
#[serial]
fn edit_against_a_stale_parent_names_the_current_head_and_publishes_nothing() {
    let d = Deployment::new();
    mount_repo(&d, REPO, MOVED_SHA);
    mount_file(&d, REPO, "a.md", "keep one");
    d.mount(
        Mock::given(method("PUT"))
            .and(path_matcher(format!("/api/v1/repos/{REPO}/push")))
            .respond_with(ResponseTemplate::new(409).set_body_json(json!({
                "error": "conflict", "message": "Head mismatch", "currentSha": MOVED_SHA,
            }))),
    );

    let out = d.run(&[
        "--json", "edit", "a.md", "--old", "one", "--new", "two", "--parent", HEAD_SHA, "--repo",
        REPO,
    ]);

    assert_eq!(exit_of(&out), 7, "{}", stderr_of(&out));
    let document = one_document(&out);
    assert_eq!(document["currentSha"], json!(MOVED_SHA));
    assert!(
        document["error"].as_str().unwrap().contains(HEAD_SHA),
        "the refusal names the parent it claimed"
    );
    assert_eq!(d.pushes().len(), 1, "nothing was republished");
}

#[test]
#[serial]
fn an_old_matching_twice_is_refused_without_replace_all() {
    let d = Deployment::new();
    mount_repo(&d, REPO, HEAD_SHA);
    mount_file(&d, REPO, "a.md", "one one");
    mount_push(&d, REPO, push_body(NEXT_SHA, 8, 1));

    let out = d.run(&[
        "edit", "a.md", "--old", "one", "--new", "two", "--parent", HEAD_SHA, "--repo", REPO,
    ]);

    assert_eq!(exit_of(&out), 1);
    let line = stderr_of(&out);
    assert!(line.contains("2 times"), "{line}");
    assert!(line.contains("--replace-all"), "{line}");
    assert!(d.pushes().is_empty());
}

#[test]
#[serial]
fn replace_all_replaces_every_occurrence_in_one_commit() {
    let d = Deployment::new();
    mount_repo(&d, REPO, HEAD_SHA);
    mount_file(&d, REPO, "a.md", "one one");
    mount_push(&d, REPO, push_body(NEXT_SHA, 8, 1));

    let out = d.run(&[
        "edit",
        "a.md",
        "--old",
        "one",
        "--new",
        "two",
        "--replace-all",
        "--parent",
        HEAD_SHA,
        "--repo",
        REPO,
    ]);

    assert_eq!(exit_of(&out), 0, "{}", stderr_of(&out));
    let pushes = d.pushes();
    assert_eq!(pushes.len(), 1);
    assert_eq!(pushes[0]["files"][0]["content"], json!("two two"));
}

#[test]
#[serial]
fn an_old_matching_nowhere_is_refused_before_any_push() {
    let d = Deployment::new();
    mount_repo(&d, REPO, HEAD_SHA);
    mount_file(&d, REPO, "a.md", "keep one");
    mount_push(&d, REPO, push_body(NEXT_SHA, 8, 1));

    let out = d.run(&[
        "edit", "a.md", "--old", "absent", "--new", "two", "--parent", HEAD_SHA, "--repo", REPO,
    ]);

    assert_eq!(exit_of(&out), 1);
    let line = stderr_of(&out);
    assert!(line.contains("a.md"), "{line}");
    assert!(line.contains(HEAD_SHA), "{line}");
    assert!(d.pushes().is_empty());
}

#[test]
#[serial]
fn an_edit_at_an_ordinal_parent_reads_by_that_ordinal() {
    let d = Deployment::new();
    mount_repo(&d, REPO, HEAD_SHA);
    d.mount(
        Mock::given(method("GET"))
            .and(path_matcher(format!("/api/v1/repos/{REPO}/versions/7")))
            .respond_with(ResponseTemplate::new(200).set_body_json(version_body(7, OLD_SHA))),
    );
    mount_file(&d, REPO, "a.md", "keep one");
    mount_push(&d, REPO, push_body(NEXT_SHA, 8, 1));

    let out = d.run(&[
        "edit", "a.md", "--old", "one", "--new", "two", "--parent", "7", "--repo", REPO,
    ]);

    assert_eq!(exit_of(&out), 0, "{}", stderr_of(&out));
    let file_ref = d
        .requests()
        .iter()
        .find(|r| r.url.path().contains("/files/"))
        .map(|r| {
            r.url
                .query_pairs()
                .find(|(k, _)| k == "ref")
                .map(|(_, v)| v.to_string())
                .unwrap_or_default()
        })
        .expect("one file read");
    assert_eq!(
        file_ref, "7",
        "the read is addressed by the resolved ordinal"
    );
    assert_eq!(d.pushes()[0]["parentSha"], json!(OLD_SHA));
}

// ---- write -------------------------------------------------------------

#[test]
#[serial]
fn write_takes_its_content_from_standard_input() {
    let d = Deployment::new();
    mount_repo(&d, REPO, HEAD_SHA);
    mount_push(&d, REPO, push_body(NEXT_SHA, 8, 1));

    let out = d.run_with_stdin(
        b"hello",
        &["write", "b.md", "--parent", HEAD_SHA, "--repo", REPO],
    );

    assert_eq!(exit_of(&out), 0, "{}", stderr_of(&out));
    let pushes = d.pushes();
    assert_eq!(pushes.len(), 1);
    assert_eq!(pushes[0]["files"][0]["path"], json!("b.md"));
    assert_eq!(pushes[0]["files"][0]["content"], json!("hello"));
    assert_eq!(pushes[0]["files"][0]["sha"], json!(blob_sha1(b"hello")));
    assert!(
        !d.work.path().join("b.md").exists(),
        "the verb writes to no folder on disk"
    );
}

#[test]
#[serial]
fn write_refuses_content_that_is_not_text() {
    let d = Deployment::new();
    mount_repo(&d, REPO, HEAD_SHA);
    mount_push(&d, REPO, push_body(NEXT_SHA, 8, 1));

    for bytes in [b"ab\x00cd".as_slice(), &[0xff, 0xfe, 0x41]] {
        let out = d.run_with_stdin(
            bytes,
            &["write", "b.bin", "--parent", HEAD_SHA, "--repo", REPO],
        );
        assert_eq!(exit_of(&out), 1);
        assert!(stderr_of(&out).contains("b.bin"), "{}", stderr_of(&out));
    }
    assert!(d.pushes().is_empty());
}

#[cfg(unix)]
#[test]
#[serial]
fn a_terminal_standard_input_is_refused_before_any_request() {
    use std::os::fd::FromRawFd;

    let d = Deployment::new();
    mount_repo(&d, REPO, HEAD_SHA);
    mount_push(&d, REPO, push_body(NEXT_SHA, 8, 1));

    let mut master: libc::c_int = 0;
    let mut slave: libc::c_int = 0;
    let opened = unsafe {
        libc::openpty(
            &mut master,
            &mut slave,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
        )
    };
    assert_eq!(opened, 0, "openpty");

    let terminal = unsafe { std::process::Stdio::from_raw_fd(slave) };
    let out = std::process::Command::new(assert_cmd::cargo::cargo_bin("syns"))
        .current_dir(d.work.path())
        .env("SYNS_CONFIG_DIR", d.home.path())
        .env("SYNS_CACHE_DIR", d.cache.path())
        .env_remove("SYNS_URL")
        .arg("--server")
        .arg(d.server.uri())
        .args(["write", "b.md", "--parent", HEAD_SHA, "--repo", REPO])
        .stdin(terminal)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .output()
        .expect("run syns");
    unsafe { libc::close(master) };

    assert_eq!(exit_of(&out), 1, "the run ends rather than waiting");
    assert!(
        stderr_of(&out).contains("standard input"),
        "{}",
        stderr_of(&out)
    );
    assert!(d.requests().is_empty(), "{:?}", d.paths());
}

// ---- rm ----------------------------------------------------------------

#[test]
#[serial]
fn rm_publishes_a_delete_only_push() {
    let d = Deployment::new();
    mount_repo(&d, REPO, HEAD_SHA);
    mount_push(&d, REPO, push_body(NEXT_SHA, 8, 1));

    let out = d.run(&["rm", "a.md", "--parent", HEAD_SHA, "--repo", REPO]);

    assert_eq!(exit_of(&out), 0, "{}", stderr_of(&out));
    let pushes = d.pushes();
    assert_eq!(pushes.len(), 1);
    assert_eq!(pushes[0]["deletions"], json!([{"path": "a.md"}]));
    assert_eq!(pushes[0]["files"], json!([]));
    assert_eq!(pushes[0]["parentSha"], json!(HEAD_SHA));
}

#[test]
#[serial]
fn rm_of_a_path_the_parent_lacks_moves_no_head() {
    let d = Deployment::new();
    mount_repo(&d, REPO, HEAD_SHA);
    mount_push(&d, REPO, push_body(HEAD_SHA, HEAD_VERSION, 0));

    let out = d.run(&["rm", "ghost.md", "--parent", HEAD_SHA, "--repo", REPO]);

    assert_eq!(exit_of(&out), 0, "{}", stderr_of(&out));
    let pushes = d.pushes();
    assert_eq!(pushes.len(), 1);
    assert_eq!(pushes[0]["deletions"], json!([{"path": "ghost.md"}]));
    let line = stderr_of(&out);
    assert!(
        line.contains(&format!(
            "nothing changed; the head is still version {HEAD_VERSION}, commit {HEAD_SHA}"
        )),
        "{line}"
    );
    assert!(!line.contains("wrote version"), "{line}");
    assert!(!line.contains("one version behind"), "{line}");
}

// ---- commit ------------------------------------------------------------

#[test]
#[serial]
fn commit_publishes_two_files_and_one_deletion_as_one_version() {
    let d = Deployment::new();
    mount_repo(&d, REPO, HEAD_SHA);
    mount_push(&d, REPO, push_body(NEXT_SHA, 8, 3));

    let changeset = json!({
        "files": [
            {"path": "a.md", "content": "alpha"},
            {"path": "z.md", "content": "zulu"},
        ],
        "deletions": [{"path": "g.md"}],
    })
    .to_string();

    let out = d.run_with_stdin(
        changeset.as_bytes(),
        &["--json", "commit", "--parent", HEAD_SHA, "--repo", REPO],
    );

    assert_eq!(exit_of(&out), 0, "{}", stderr_of(&out));
    let pushes = d.pushes();
    assert_eq!(pushes.len(), 1, "exactly one push");
    assert_eq!(pushes[0]["deletions"], json!([{"path": "g.md"}]));
    for file in pushes[0]["files"].as_array().unwrap() {
        let content = file["content"].as_str().unwrap();
        assert_eq!(file["sha"], json!(blob_sha1(content.as_bytes())));
    }
    let document = one_document(&out);
    assert_eq!(document["filesChanged"], json!(3));
    assert_eq!(document["commitSha"], json!(NEXT_SHA));
}

#[test]
#[serial]
fn a_changeset_naming_neither_a_file_nor_a_deletion_is_refused_at_exit_six() {
    let d = Deployment::new();
    mount_repo(&d, REPO, HEAD_SHA);
    mount_push(&d, REPO, push_body(NEXT_SHA, 8, 1));

    let out = d.run_with_stdin(
        br#"{"files":[],"deletions":[]}"#,
        &["commit", "--parent", HEAD_SHA, "--repo", REPO],
    );

    assert_eq!(exit_of(&out), 6, "{}", stderr_of(&out));
    assert!(d.pushes().is_empty());
}

#[test]
#[serial]
fn a_changeset_carrying_an_unpaired_surrogate_escape_is_refused_before_any_request() {
    let d = Deployment::new();
    mount_repo(&d, REPO, HEAD_SHA);
    mount_push(&d, REPO, push_body(NEXT_SHA, 8, 1));

    let out = d.run_with_stdin(
        br#"{"files":[{"path":"a.md","content":"\ud800"}]}"#,
        &["commit", "--parent", HEAD_SHA, "--repo", REPO],
    );

    assert_eq!(exit_of(&out), 1);
    assert!(
        stderr_of(&out).contains("error: configuration error:"),
        "{}",
        stderr_of(&out)
    );
    assert!(d.pushes().is_empty());
}

// ---- the parent spellings ----------------------------------------------

#[test]
#[serial]
fn only_a_full_hash_parent_skips_the_version_request() {
    let d = Deployment::new();
    mount_repo(&d, REPO, HEAD_SHA);
    d.mount(
        Mock::given(method("GET"))
            .and(path_matcher(format!("/api/v1/repos/{REPO}/versions/7")))
            .respond_with(ResponseTemplate::new(200).set_body_json(version_body(7, HEAD_SHA))),
    );
    d.mount(
        Mock::given(method("GET"))
            .and(path_matcher(format!(
                "/api/v1/repos/{REPO}/versions/{HEAD_PREFIX}"
            )))
            .respond_with(ResponseTemplate::new(200).set_body_json(version_body(7, HEAD_SHA))),
    );
    mount_push(&d, REPO, push_body(NEXT_SHA, 8, 1));

    let full = d.run(&["rm", "a.md", "--parent", HEAD_SHA, "--repo", REPO]);
    assert_eq!(exit_of(&full), 0, "{}", stderr_of(&full));
    assert!(
        d.version_calls().is_empty(),
        "a full hash stands unresolved: {:?}",
        d.version_calls()
    );

    let ordinal = d.run(&["rm", "a.md", "--parent", "7", "--repo", REPO]);
    assert_eq!(exit_of(&ordinal), 0, "{}", stderr_of(&ordinal));

    let prefix = d.run(&["rm", "a.md", "--parent", HEAD_PREFIX, "--repo", REPO]);
    assert_eq!(exit_of(&prefix), 0, "{}", stderr_of(&prefix));

    assert_eq!(
        d.version_calls(),
        vec!["7".to_string(), HEAD_PREFIX.to_string()],
        "each carries its own spelling as the last segment"
    );
    for push in d.pushes() {
        assert_eq!(
            push["parentSha"],
            json!(HEAD_SHA),
            "each pushes the full hash rather than the spelling"
        );
    }
    assert_eq!(
        d.paths()
            .iter()
            .filter(|p| p.as_str() == format!("/api/v1/repos/{REPO}"))
            .count(),
        3,
        "each run reads the repository once"
    );
}

#[test]
#[serial]
fn a_repository_standing_at_no_identity_is_refused_before_any_push() {
    let d = Deployment::new();
    d.mount(
        Mock::given(method("GET"))
            .and(path_matcher("/api/v1/repos/alice/absent"))
            .respond_with(ResponseTemplate::new(404).set_body_json(json!({"error": "not_found"}))),
    );

    let out = d.run_with_stdin(
        b"hello",
        &[
            "write",
            "b.md",
            "--parent",
            HEAD_SHA,
            "--repo",
            "alice/absent",
        ],
    );

    assert_eq!(exit_of(&out), 1);
    assert!(d.pushes().is_empty(), "no push received at all");
    assert_eq!(
        d.paths(),
        vec!["/api/v1/repos/alice/absent".to_string()],
        "nothing created at that identity"
    );
}

// ---- the checkout guard ------------------------------------------------

#[test]
#[serial]
fn a_dirty_checkout_refuses_its_own_repository_and_admits_another() {
    let d = Deployment::new();
    seed_checkout(&d, d.work.path(), REPO, true);
    std::fs::write(d.work.path().join("a.md"), "edited since").expect("an edit");
    let before_state = state_entries(&d, REPO);
    mount_repo(&d, REPO, HEAD_SHA);
    mount_repo(&d, "bob/other", HEAD_SHA);
    mount_push(&d, "bob/other", push_body(NEXT_SHA, 8, 1));

    let own = d.run_with_stdin(b"hi", &["write", "c.md", "--parent", HEAD_SHA]);
    assert_eq!(exit_of(&own), 1, "{}", stdout_of(&own));
    let line = stderr_of(&own);
    assert!(line.contains("syns sync"), "{line}");
    assert!(
        line.contains(
            &std::fs::canonicalize(d.work.path())
                .unwrap()
                .display()
                .to_string()
        ),
        "{line}"
    );
    assert!(d.requests().is_empty(), "{:?}", d.paths());
    assert_eq!(
        state_entries(&d, REPO),
        before_state,
        "the working copy's state is left untouched (CR1-2)"
    );

    let other = d.run_with_stdin(
        b"hi",
        &["write", "c.md", "--parent", HEAD_SHA, "--repo", "bob/other"],
    );
    assert_eq!(exit_of(&other), 0, "{}", stderr_of(&other));
    let pushes = d.pushes();
    assert_eq!(pushes.len(), 1);
    assert!(
        d.paths().iter().all(|p| !p.contains("alice/notes")),
        "none addressing the standing repository: {:?}",
        d.paths()
    );
}

#[test]
#[serial]
fn a_checkout_recording_no_base_refuses_a_write_to_its_own_repository() {
    let d = Deployment::new();
    seed_checkout(&d, d.work.path(), REPO, false);
    mount_repo(&d, REPO, HEAD_SHA);
    mount_push(&d, REPO, push_body(NEXT_SHA, 8, 1));

    let before_state = state_entries(&d, REPO);
    let out = d.run_with_stdin(b"hi", &["write", "c.md", "--parent", HEAD_SHA]);

    assert_eq!(exit_of(&out), 1);
    assert!(
        stderr_of(&out).contains(
            &std::fs::canonicalize(d.work.path())
                .unwrap()
                .display()
                .to_string()
        ),
        "{}",
        stderr_of(&out)
    );
    assert!(d.requests().is_empty(), "{:?}", d.paths());
    assert_eq!(
        state_entries(&d, REPO),
        before_state,
        "the guard writes no state to refuse (CR1-2)"
    );
}

#[test]
#[serial]
fn a_clean_checkout_is_written_to_and_told_it_is_behind_on_both_streams() {
    let d = Deployment::new();
    seed_checkout(&d, d.work.path(), REPO, true);
    mount_repo(&d, REPO, HEAD_SHA);
    mount_push(&d, REPO, push_body(NEXT_SHA, 8, 1));
    let root = std::fs::canonicalize(d.work.path()).unwrap();
    let before = folder_hashes(d.work.path());

    let human = d.run_with_stdin(b"hi", &["write", "c.md", "--parent", HEAD_SHA]);
    assert_eq!(exit_of(&human), 0, "{}", stderr_of(&human));
    let line = stderr_of(&human);
    assert!(line.contains(&root.display().to_string()), "{line}");
    assert!(line.contains("syns sync converges it"), "{line}");
    assert_eq!(stdout_of(&human), "", "the primary stream carries nothing");

    let machine = d.run_with_stdin(b"hi", &["--json", "write", "c.md", "--parent", HEAD_SHA]);
    assert_eq!(exit_of(&machine), 0, "{}", stderr_of(&machine));
    let document = one_document(&machine);
    assert_eq!(
        document["checkoutBehind"],
        json!(root.display().to_string())
    );
    assert_eq!(document["commitSha"], json!(NEXT_SHA));
    assert_eq!(
        stderr_of(&machine),
        "",
        "neither line is written in machine-readable mode"
    );

    assert_eq!(
        folder_hashes(d.work.path()),
        before,
        "neither run creates, changes or removes a file of the folder"
    );
}

// ---- provenance --------------------------------------------------------

#[test]
#[serial]
fn provenance_options_outrank_the_environment_and_ride_the_push() {
    let d = Deployment::new();
    mount_repo(&d, REPO, HEAD_SHA);
    mount_push(&d, REPO, push_body(NEXT_SHA, 8, 1));

    let with_all = [
        ("SYNS_INTEGRATION", Some("shell")),
        ("SYNS_RUN", Some("r-9")),
        ("SYNS_TRIGGER", Some("cron")),
    ];
    let first = d.run_in(
        d.work.path(),
        &with_all,
        b"",
        &[
            "rm",
            "a.md",
            "--parent",
            HEAD_SHA,
            "--repo",
            REPO,
            "--integration",
            "page",
            "--task-ref",
            "t-1",
        ],
    );
    assert_eq!(exit_of(&first), 0, "{}", stderr_of(&first));

    let without_trigger = [
        ("SYNS_INTEGRATION", Some("shell")),
        ("SYNS_RUN", Some("r-9")),
        ("SYNS_TRIGGER", None),
    ];
    let second = d.run_in(
        d.work.path(),
        &without_trigger,
        b"",
        &["rm", "a.md", "--parent", HEAD_SHA, "--repo", REPO],
    );
    assert_eq!(exit_of(&second), 0, "{}", stderr_of(&second));

    let pushes = d.pushes();
    assert_eq!(pushes.len(), 2);
    assert_eq!(pushes[0]["provenance"]["integration"], json!("page"));
    assert_eq!(pushes[0]["provenance"]["run"], json!("r-9"));
    assert_eq!(pushes[0]["provenance"]["trigger"], json!("cron"));
    assert_eq!(pushes[0]["provenance"]["taskRef"], json!("t-1"));
    assert!(
        pushes[1].get("provenance").is_none(),
        "the block is sent whole or not at all: {}",
        pushes[1]
    );
}

// ---- the forced publication --------------------------------------------

#[test]
#[serial]
fn a_forced_publication_names_the_parent_it_did_not_claim() {
    let d = Deployment::new();
    seed_checkout(&d, d.work.path(), REPO, true);
    mount_push(&d, REPO, push_body(NEXT_SHA, 8, 1));

    let seed_record = || {
        let mut manifest = Manifest::default();
        manifest.update(HEAD_SHA.to_string(), folder_hashes(d.work.path()));
        manifest
            .save(d.cache.path(), "alice", "notes")
            .expect("local record");
    };

    seed_record();
    let human = d.run(&["push", "--force"]);
    assert_eq!(exit_of(&human), 0, "{}", stderr_of(&human));
    let line = stderr_of(&human);
    assert!(line.contains(HEAD_SHA), "{line}");
    assert!(line.contains(REPO), "{line}");
    assert!(line.contains("the head check did not run"), "{line}");

    seed_record();
    let machine = d.run(&["--json", "push", "--force"]);
    assert_eq!(exit_of(&machine), 0, "{}", stderr_of(&machine));
    let document = one_document(&machine);
    assert_eq!(document["unclaimedParent"], json!(HEAD_SHA));
    assert!(
        !stderr_of(&machine).contains("the head check did not run"),
        "{}",
        stderr_of(&machine)
    );

    let pushes = d.pushes();
    assert_eq!(pushes.len(), 2);
    for push in &pushes {
        assert!(
            push.get("parentSha").is_none(),
            "the flag claims no parent: {push}"
        );
        let sent: Vec<&str> = push["files"]
            .as_array()
            .unwrap()
            .iter()
            .map(|f| f["path"].as_str().unwrap())
            .collect();
        assert!(
            sent.contains(&"a.md"),
            "every collected path rides: {sent:?}"
        );
        for file in push["files"].as_array().unwrap() {
            assert!(file["content"].is_string(), "each with its content: {file}");
        }
    }
}
