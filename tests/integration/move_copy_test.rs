//! Binary-level behaviour of `syns mv FROM TO` and `syns cp FROM TO`,
//! the changeset's `moves` and `copies`, and `syns history` across a
//! recorded move (roadmap 228, FEATURE_SPEC § Expected · CLI, rows 26 to
//! 33; D-253 §1, §2, §9).
//!
//! Each test drives one mock deployment of its own, so two tests never
//! share a store, a credential or a working directory.

use assert_cmd::Command as AssertCommand;
use serde_json::{Value, json};
use serial_test::serial;
use std::path::Path;
use syns_cli::push::collector::{CollectOptions, HeldBytes, collect_files};
use syns_cli::push::working_copy::WorkingCopy;
use tempfile::TempDir;
use wiremock::matchers::{method, path as path_matcher, query_param};
use wiremock::{Mock, MockServer, Request, ResponseTemplate};

const REPO: &str = "alice/notes";
const HEAD_SHA: &str = "aa11bb22cc33dd44ee55ff6600778899001122bb";
const MOVED_SHA: &str = "cc33dd44ee55ff6600778899001122bbaa11bb22";
const NEXT_SHA: &str = "dd44ee55ff6600778899001122bbaa11bb22cc33";
/// A string written into every moved or copied file of a fixture: no
/// request body may carry it (FEATURE_SPEC § Expected, *No content*).
const MARKER: &str = "r228-marker-7f3a";

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
        let d = Deployment {
            rt,
            server,
            home: tempfile::tempdir().expect("config dir"),
            cache: tempfile::tempdir().expect("cache dir"),
            work: tempfile::tempdir().expect("working dir"),
        };
        std::fs::write(
            d.home.path().join("credentials.json"),
            json!({"token": "test-token", "username": "alice"}).to_string(),
        )
        .expect("credential");
        d
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
            .map(|r| format!("{} {}", r.method, r.url.path()))
            .collect()
    }

    fn pushes(&self) -> Vec<Value> {
        self.requests()
            .iter()
            .filter(|r| r.url.path().ends_with("/push"))
            .map(|r| serde_json::from_slice(&r.body).expect("push body"))
            .collect()
    }

    fn run(&self, args: &[&str]) -> std::process::Output {
        self.run_in(self.work.path(), b"", args)
    }

    fn run_with_stdin(&self, stdin: &[u8], args: &[&str]) -> std::process::Output {
        self.run_in(self.work.path(), stdin, args)
    }

    fn command(&self, cwd: &Path) -> AssertCommand {
        let mut command = AssertCommand::cargo_bin("syns").expect("syns binary");
        command
            .current_dir(cwd)
            .env("SYNS_CONFIG_DIR", self.home.path())
            .env("SYNS_CACHE_DIR", self.cache.path())
            .env_remove("SYNS_URL")
            .env_remove("SYNS_INTEGRATION")
            .env_remove("SYNS_RUN")
            .env_remove("SYNS_TRIGGER")
            .env_remove("SYNS_TASK")
            .arg("--server")
            .arg(self.server.uri());
        command
    }

    fn run_in(&self, cwd: &Path, stdin: &[u8], args: &[&str]) -> std::process::Output {
        self.command(cwd)
            .args(args)
            .write_stdin(stdin.to_vec())
            .output()
            .expect("run syns")
    }
}

fn repo_body(commit_sha: &str) -> Value {
    json!({
        "owner": "alice", "name": "notes", "description": null,
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

fn mount_repo(d: &Deployment, commit_sha: &str) {
    d.mount(
        Mock::given(method("GET"))
            .and(path_matcher(format!("/api/v1/repos/{REPO}")))
            .respond_with(ResponseTemplate::new(200).set_body_json(repo_body(commit_sha))),
    );
}

fn mount_version(d: &Deployment, spelling: &str, version: u32, sha: &str) {
    d.mount(
        Mock::given(method("GET"))
            .and(path_matcher(format!(
                "/api/v1/repos/{REPO}/versions/{spelling}"
            )))
            .respond_with(ResponseTemplate::new(200).set_body_json(version_body(version, sha))),
    );
}

fn mount_push_answer(d: &Deployment, status: u16, body: Value) {
    d.mount(
        Mock::given(method("PUT"))
            .and(path_matcher(format!("/api/v1/repos/{REPO}/push")))
            .respond_with(ResponseTemplate::new(status).set_body_json(body)),
    );
}

/// The push answer of FEATURE_SPEC row 17 at `version`, carrying `key`
/// (`moves` or `copies`) with the members given.
fn member_answer(version: u32, files_changed: u32, key: &str, members: Value) -> Value {
    let mut body = json!({
        "commitSha": NEXT_SHA, "version": version,
        "filesChanged": files_changed, "created": false,
    });
    body[key] = members;
    body
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

/// Whether any request this deployment received carries `MARKER`.
fn marker_on_the_wire(d: &Deployment) -> bool {
    d.requests().iter().any(|r| {
        String::from_utf8_lossy(&r.body).contains(MARKER) || r.url.as_str().contains(MARKER)
    })
}

/// A checkout of `alice/notes` at `root` holding `q3-plan/` with the
/// marker in each file, its working copy recording the folder as its base.
fn seed_checkout(d: &Deployment, root: &Path) {
    std::fs::write(root.join(".syns.yaml"), "owner: alice\nname: notes\n").expect("identity");
    std::fs::create_dir_all(root.join("q3-plan/images")).expect("folder");
    for file in [
        "q3-plan/document.html",
        "q3-plan/comments.html",
        "q3-plan/images/fig.png",
    ] {
        std::fs::write(root.join(file), format!("{MARKER} {file}")).expect("a file");
    }
    let copy = WorkingCopy::open(
        &syns_cli::config::StoreRoots::resolve(
            Some(d.cache.path()),
            d.cache.path(),
            d.cache.path(),
        ),
        "alice",
        "notes",
        root,
    )
    .expect("working copy");
    let hashes = collect_files(
        root,
        &[],
        CollectOptions::default(),
        None,
        &HeldBytes::new(0),
    )
    .expect("collected")
    .hashes();
    copy.record_base(HEAD_SHA, hashes).expect("recorded base");
}

// ---- row 26: the body ----------------------------------------------------

#[test]
#[serial]
fn row26_mv_sends_two_paths_at_the_resolved_parent_and_no_content() {
    let d = Deployment::new();
    mount_repo(&d, HEAD_SHA);
    mount_version(&d, "41", 41, HEAD_SHA);
    mount_push_answer(
        &d,
        200,
        member_answer(
            42,
            1,
            "moves",
            json!([{"from": "q3-plan", "to": "q3-review", "type": "dir", "files": 3}]),
        ),
    );

    let out = d.run_with_stdin(
        MARKER.as_bytes(),
        &[
            "mv",
            "q3-plan",
            "q3-review",
            "--parent",
            "41",
            "--repo",
            REPO,
        ],
    );

    assert_eq!(exit_of(&out), 0, "{}", stderr_of(&out));
    assert_eq!(
        d.paths(),
        vec![
            format!("GET /api/v1/repos/{REPO}"),
            format!("GET /api/v1/repos/{REPO}/versions/41"),
            format!("PUT /api/v1/repos/{REPO}/push"),
        ],
        "the repository read, the version read and one push; no tree, file or batch read"
    );
    assert_eq!(
        d.pushes()[0],
        json!({
            "moves": [{"from": "q3-plan", "to": "q3-review"}],
            "parentSha": HEAD_SHA,
            "message": "mv q3-plan q3-review",
        })
    );
    assert!(
        !marker_on_the_wire(&d),
        "no content and no standard input on the wire"
    );
}

#[test]
#[serial]
fn row26_cp_sends_copies_and_a_full_hash_parent_makes_no_version_read() {
    let d = Deployment::new();
    mount_repo(&d, HEAD_SHA);
    mount_push_answer(
        &d,
        200,
        member_answer(
            44,
            1,
            "copies",
            json!([{"from": "templates/doc", "to": "docs/new-doc", "type": "dir", "files": 3}]),
        ),
    );

    let out = d
        .command(d.work.path())
        .env("SYNS_INTEGRATION", "bb-pages")
        .env("SYNS_RUN", "run-9")
        .env("SYNS_TRIGGER", "agent")
        .args([
            "cp",
            "templates/doc",
            "docs/new-doc",
            "--parent",
            HEAD_SHA,
            "--repo",
            REPO,
            "-m",
            "Copy the template",
        ])
        .output()
        .expect("run syns");

    assert_eq!(exit_of(&out), 0, "{}", stderr_of(&out));
    assert_eq!(
        d.paths(),
        vec![
            format!("GET /api/v1/repos/{REPO}"),
            format!("PUT /api/v1/repos/{REPO}/push"),
        ]
    );
    assert_eq!(
        d.pushes()[0],
        json!({
            "copies": [{"from": "templates/doc", "to": "docs/new-doc"}],
            "parentSha": HEAD_SHA,
            "message": "Copy the template",
            "provenance": {"integration": "bb-pages", "run": "run-9", "trigger": "agent"},
        })
    );
}

#[test]
#[serial]
fn row26_the_default_caption_is_the_command_as_typed() {
    let d = Deployment::new();
    mount_repo(&d, HEAD_SHA);
    mount_push_answer(
        &d,
        200,
        member_answer(
            8,
            1,
            "copies",
            json!([{"from": "a.md", "to": "b.md", "type": "file", "files": 1}]),
        ),
    );

    let out = d.run(&[
        "cp", "./a.md", "b.md/", "--parent", HEAD_SHA, "--repo", REPO,
    ]);

    assert_eq!(exit_of(&out), 0, "{}", stderr_of(&out));
    let push = &d.pushes()[0];
    assert_eq!(push["message"], json!("cp ./a.md b.md/"));
    assert_eq!(push["copies"], json!([{"from": "a.md", "to": "b.md"}]));
}

// ---- row 27: the report --------------------------------------------------

#[test]
#[serial]
fn row27_the_report_names_the_version_then_each_member() {
    for (verb, from, to, key, kind, files, line) in [
        (
            "mv",
            "q3-plan",
            "q3-review",
            "moves",
            "dir",
            3,
            "moved 3 files: q3-plan/ \u{2192} q3-review/",
        ),
        (
            "mv",
            "a.md",
            "b.md",
            "moves",
            "file",
            1,
            "moved 1 file: a.md \u{2192} b.md",
        ),
        (
            "cp",
            "templates/doc",
            "docs/new-doc",
            "copies",
            "dir",
            3,
            "copied 3 files: templates/doc/ \u{2192} docs/new-doc/",
        ),
        (
            "cp",
            "solo",
            "pair",
            "copies",
            "dir",
            1,
            "copied 1 file: solo/ \u{2192} pair/",
        ),
    ] {
        let d = Deployment::new();
        mount_repo(&d, HEAD_SHA);
        mount_push_answer(
            &d,
            200,
            member_answer(
                42,
                1,
                key,
                json!([{"from": from, "to": to, "type": kind, "files": files}]),
            ),
        );

        let out = d.run(&[verb, from, to, "--parent", HEAD_SHA, "--repo", REPO]);

        assert_eq!(exit_of(&out), 0, "{}", stderr_of(&out));
        assert_eq!(stdout_of(&out), "", "{verb} {from}");
        assert_eq!(
            stderr_of(&out),
            format!("wrote version 42, commit {NEXT_SHA}\n{line}\n"),
            "{verb} {from}"
        );
    }
}

#[test]
#[serial]
fn row27_a_clean_checkout_left_behind_is_named_after_the_member_lines() {
    let d = Deployment::new();
    seed_checkout(&d, d.work.path());
    mount_repo(&d, HEAD_SHA);
    let answer = member_answer(
        42,
        1,
        "moves",
        json!([{"from": "q3-plan", "to": "q3-review", "type": "dir", "files": 3}]),
    );
    mount_push_answer(&d, 200, answer.clone());

    let out = d.run(&["mv", "q3-plan", "q3-review", "--parent", HEAD_SHA]);

    assert_eq!(exit_of(&out), 0, "{}", stderr_of(&out));
    let root = std::fs::canonicalize(d.work.path()).unwrap();
    let lines: Vec<String> = stderr_of(&out).lines().map(str::to_string).collect();
    assert_eq!(lines.len(), 3, "{lines:?}");
    assert_eq!(lines[0], format!("wrote version 42, commit {NEXT_SHA}"));
    assert_eq!(lines[1], "moved 3 files: q3-plan/ \u{2192} q3-review/");
    assert!(
        lines[2].starts_with("the checkout at ") && lines[2].contains("one version behind"),
        "{lines:?}"
    );
    assert!(
        std::fs::canonicalize(
            lines[2]
                .trim_start_matches("the checkout at ")
                .split(" is now")
                .next()
                .unwrap()
        )
        .unwrap()
            == root
    );
    assert!(!marker_on_the_wire(&d));

    // `--json`: the answer verbatim plus `checkoutBehind`.
    let json_out = d.run(&["--json", "mv", "q3-plan", "q3-review", "--parent", HEAD_SHA]);
    assert_eq!(exit_of(&json_out), 0, "{}", stderr_of(&json_out));
    let mut document = one_document(&json_out);
    assert!(document["checkoutBehind"].is_string(), "{document}");
    document.as_object_mut().unwrap().remove("checkoutBehind");
    assert_eq!(document, answer);
    assert_eq!(stderr_of(&json_out), "");
}

#[test]
#[serial]
fn row27_json_passes_the_answer_through_verbatim() {
    let d = Deployment::new();
    mount_repo(&d, HEAD_SHA);
    let answer = member_answer(
        44,
        1,
        "copies",
        json!([{"from": "templates/doc", "to": "docs/new-doc", "type": "dir", "files": 3}]),
    );
    mount_push_answer(&d, 200, answer.clone());

    let out = d.run(&[
        "--json",
        "cp",
        "templates/doc",
        "docs/new-doc",
        "--parent",
        HEAD_SHA,
        "--repo",
        REPO,
    ]);

    assert_eq!(exit_of(&out), 0, "{}", stderr_of(&out));
    assert_eq!(one_document(&out), answer);
}

// ---- row 28: refusals ------------------------------------------------------

#[test]
#[serial]
fn row28_a_stale_parent_is_the_conflict_at_exit_seven() {
    let d = Deployment::new();
    mount_repo(&d, MOVED_SHA);
    mount_push_answer(
        &d,
        409,
        json!({"error": "conflict", "message": "Head mismatch", "currentSha": MOVED_SHA}),
    );

    let out = d.run(&[
        "mv", "q3-retro", "q3-final", "--parent", HEAD_SHA, "--repo", REPO,
    ]);
    assert_eq!(exit_of(&out), 7, "{}", stderr_of(&out));
    assert_eq!(
        stderr_of(&out),
        format!(
            "error: conflict: the repository moved past {HEAD_SHA}; its head is now {MOVED_SHA}\n"
        )
    );

    let json_out = d.run(&[
        "--json", "mv", "q3-retro", "q3-final", "--parent", HEAD_SHA, "--repo", REPO,
    ]);
    assert_eq!(exit_of(&json_out), 7);
    let document = one_document(&json_out);
    assert_eq!(document["currentSha"], json!(MOVED_SHA));
    assert_eq!(document["code"], json!("write_conflict"));
    assert!(document["error"].as_str().unwrap().contains(HEAD_SHA));
}

#[test]
#[serial]
fn row28_source_not_found_names_the_path_and_the_parent() {
    let d = Deployment::new();
    mount_repo(&d, HEAD_SHA);
    mount_version(&d, "44", 44, HEAD_SHA);
    mount_push_answer(
        &d,
        404,
        json!({
            "error": "not_found", "reason": "source_not_found", "path": "nowhere",
            "message": "Source path not found: nowhere",
        }),
    );

    let out = d.run(&["mv", "nowhere", "q3-x", "--parent", "44", "--repo", REPO]);
    assert_eq!(exit_of(&out), 1, "{}", stderr_of(&out));
    assert_eq!(
        stderr_of(&out),
        "error: path not found at version 44: nowhere\n"
    );

    let json_out = d.run(&[
        "--json", "mv", "nowhere", "q3-x", "--parent", "44", "--repo", REPO,
    ]);
    assert_eq!(exit_of(&json_out), 1);
    assert_eq!(
        one_document(&json_out),
        json!({"error": "path not found at version 44: nowhere", "code": "not_found"})
    );
}

#[test]
#[serial]
fn row28_destination_exists_names_the_path_and_the_way_out() {
    let d = Deployment::new();
    mount_repo(&d, HEAD_SHA);
    mount_version(&d, "44", 44, HEAD_SHA);
    mount_push_answer(
        &d,
        409,
        json!({
            "error": "conflict", "reason": "destination_exists", "path": "docs",
            "message": "Destination already exists: docs",
        }),
    );

    let out = d.run(&["mv", "q3-retro", "docs", "--parent", "44", "--repo", REPO]);
    assert_eq!(exit_of(&out), 1, "{}", stderr_of(&out));
    assert_eq!(
        stderr_of(&out),
        "error: docs already exists at version 44; remove it first in the same commit\n"
    );

    let json_out = d.run(&[
        "--json", "cp", "q3-retro", "docs", "--parent", "44", "--repo", REPO,
    ]);
    assert_eq!(exit_of(&json_out), 1);
    assert_eq!(
        one_document(&json_out),
        json!({
            "error": "docs already exists at version 44; remove it first in the same commit",
            "code": "path_exists",
        })
    );
}

#[test]
#[serial]
fn row28_a_checkout_holding_unpublished_work_is_refused_before_any_request() {
    let d = Deployment::new();
    seed_checkout(&d, d.work.path());
    std::fs::write(d.work.path().join("q3-plan/document.html"), "edited since").unwrap();
    mount_repo(&d, HEAD_SHA);

    let out = d.run(&["--json", "mv", "q3-plan", "q3-review", "--parent", HEAD_SHA]);

    assert_eq!(exit_of(&out), 1, "{}", stderr_of(&out));
    let document = one_document(&out);
    assert_eq!(document["code"], json!("checkout_dirty"));
    assert!(
        document["error"]
            .as_str()
            .unwrap()
            .contains("holds unpublished local changes for alice/notes"),
        "{document}"
    );
    assert!(d.requests().is_empty(), "{:?}", d.paths());
}

// ---- row 29: lexical refusals ------------------------------------------------

#[test]
#[serial]
fn row29_overlapping_paths_are_refused_naming_both_before_any_request() {
    let d = Deployment::new();
    mount_repo(&d, HEAD_SHA);
    for (verb, from, to, line) in [
        (
            "mv",
            "docs",
            "docs/archive",
            "error: configuration error: cannot move docs to docs/archive: docs/archive lies under docs\n",
        ),
        (
            "mv",
            "docs/a",
            "docs",
            "error: configuration error: cannot move docs/a to docs: docs lies above docs/a\n",
        ),
        (
            "mv",
            "a",
            "a",
            "error: configuration error: cannot move a to a: they are the same path\n",
        ),
        (
            "cp",
            "docs",
            "./docs/archive/",
            "error: configuration error: cannot copy docs to docs/archive: docs/archive lies under docs\n",
        ),
    ] {
        let out = d.run(&[verb, from, to, "--parent", HEAD_SHA, "--repo", REPO]);
        assert_eq!(exit_of(&out), 1, "{from} {to}: {}", stderr_of(&out));
        assert_eq!(stderr_of(&out), line);
        let json_out = d.run(&[
            "--json", verb, from, to, "--parent", HEAD_SHA, "--repo", REPO,
        ]);
        assert_eq!(one_document(&json_out)["code"], json!("config_error"));
    }
    assert!(d.requests().is_empty(), "{:?}", d.paths());
}

#[test]
#[serial]
fn row29_empty_root_and_climbing_paths_are_refused_before_any_request() {
    let d = Deployment::new();
    mount_repo(&d, HEAD_SHA);
    for (args, line) in [
        (["", "b.md"], "error: configuration error: path cannot be empty\n".to_string()),
        (["a.md", "  "], "error: configuration error: path cannot be empty\n".to_string()),
        (
            [".", "b"],
            "error: configuration error: . names the repository's root; name a file counted from it\n"
                .to_string(),
        ),
        (
            ["a", "x/.."],
            "error: configuration error: x/.. names the repository's root; name a file counted from it\n"
                .to_string(),
        ),
        (
            ["..", "b"],
            "error: configuration error: .. names no path inside the repository; name one counted from its root\n"
                .to_string(),
        ),
        (
            ["a", "../b"],
            "error: configuration error: ../b names no path inside the repository; name one counted from its root\n"
                .to_string(),
        ),
    ] {
        let out = d.run(&["mv", args[0], args[1], "--parent", HEAD_SHA, "--repo", REPO]);
        assert_eq!(exit_of(&out), 1, "{args:?}: {}", stderr_of(&out));
        assert_eq!(stderr_of(&out), line, "{args:?}");
    }
    assert!(d.requests().is_empty(), "{:?}", d.paths());
}

#[test]
#[serial]
fn row29_a_trailing_slash_and_dot_segments_resolve_lexically() {
    let d = Deployment::new();
    mount_repo(&d, HEAD_SHA);
    mount_push_answer(
        &d,
        200,
        member_answer(
            8,
            1,
            "moves",
            json!([{"from": "q3-plan", "to": "a/b", "type": "dir", "files": 2}]),
        ),
    );

    let out = d.run(&[
        "mv", "q3-plan/", "a/./b", "--parent", HEAD_SHA, "--repo", REPO,
    ]);

    assert_eq!(exit_of(&out), 0, "{}", stderr_of(&out));
    assert_eq!(
        d.pushes()[0]["moves"],
        json!([{"from": "q3-plan", "to": "a/b"}])
    );
    assert!(
        stderr_of(&out).contains("moved 2 files: q3-plan/ \u{2192} a/b/"),
        "{}",
        stderr_of(&out)
    );
}

#[cfg(unix)]
#[test]
#[serial]
fn row29_a_non_utf8_argument_is_the_argument_parsers_refusal() {
    use std::ffi::OsStr;
    use std::os::unix::ffi::OsStrExt;
    let d = Deployment::new();
    mount_repo(&d, HEAD_SHA);
    for verb in ["mv", "cp", "write"] {
        let mut command = d.command(d.work.path());
        command.arg(verb).arg(OsStr::from_bytes(b"a\xff.md"));
        if verb != "write" {
            command.arg("b.md");
        }
        let out = command
            .args(["--parent", HEAD_SHA, "--repo", REPO])
            .write_stdin(Vec::new())
            .output()
            .expect("run syns");
        assert_eq!(exit_of(&out), 2, "{verb}: {}", stderr_of(&out));
    }
    assert!(d.requests().is_empty());
}

// ---- row 31: the changeset ---------------------------------------------------

#[test]
#[serial]
fn row31_a_changeset_sends_all_four_members_in_one_push_and_reports_each() {
    let d = Deployment::new();
    mount_repo(&d, HEAD_SHA);
    mount_push_answer(
        &d,
        200,
        json!({
            "commitSha": NEXT_SHA, "version": 43, "filesChanged": 4, "created": false,
            "moves": [{"from": "q3-review", "to": "q3-retro", "type": "dir", "files": 3}],
            "copies": [{"from": "templates/doc", "to": "docs/new-doc", "type": "dir", "files": 3}],
        }),
    );
    // `copies` written ahead of `moves`: the report follows the document.
    let changeset = json!({
        "copies": [{"from": "templates/doc", "to": "docs/new-doc"}],
        "moves": [{"from": "q3-review", "to": "q3-retro"}],
        "files": [{"path": "q3-retro/document.html", "content": "<h1>Q3 retro</h1>"}],
        "deletions": [{"path": "old.md"}],
    })
    .to_string();

    let out = d.run_with_stdin(
        changeset.as_bytes(),
        &[
            "commit",
            "--parent",
            HEAD_SHA,
            "--repo",
            REPO,
            "-m",
            "Retitle: Q3 retro",
        ],
    );

    assert_eq!(exit_of(&out), 0, "{}", stderr_of(&out));
    let pushes = d.pushes();
    assert_eq!(pushes.len(), 1);
    assert_eq!(
        pushes[0]["moves"],
        json!([{"from": "q3-review", "to": "q3-retro"}])
    );
    assert_eq!(
        pushes[0]["copies"],
        json!([{"from": "templates/doc", "to": "docs/new-doc"}])
    );
    assert_eq!(
        pushes[0]["files"][0]["path"],
        json!("q3-retro/document.html")
    );
    assert_eq!(pushes[0]["deletions"], json!([{"path": "old.md"}]));
    assert_eq!(
        stderr_of(&out),
        format!(
            "wrote version 43, commit {NEXT_SHA}\n\
             copied 3 files: templates/doc/ \u{2192} docs/new-doc/\n\
             moved 3 files: q3-review/ \u{2192} q3-retro/\n"
        )
    );
}

#[test]
#[serial]
fn row31_a_changeset_of_members_alone_sends_no_files_key() {
    let d = Deployment::new();
    mount_repo(&d, HEAD_SHA);
    mount_push_answer(
        &d,
        200,
        member_answer(
            9,
            1,
            "moves",
            json!([{"from": "a.md", "to": "b.md", "type": "file", "files": 1}]),
        ),
    );

    let out = d.run_with_stdin(
        br#"{"moves":[{"from":"a.md","to":"b.md"}]}"#,
        &["commit", "--parent", HEAD_SHA, "--repo", REPO],
    );

    assert_eq!(exit_of(&out), 0, "{}", stderr_of(&out));
    assert_eq!(
        d.pushes()[0],
        json!({
            "moves": [{"from": "a.md", "to": "b.md"}],
            "parentSha": HEAD_SHA,
            "message": "commit",
        })
    );
}

#[test]
#[serial]
fn row31_an_empty_or_malformed_changeset_is_refused_before_any_request() {
    let d = Deployment::new();
    mount_repo(&d, HEAD_SHA);

    let empty = d.run_with_stdin(
        br#"{"moves":[]}"#,
        &["commit", "--parent", HEAD_SHA, "--repo", REPO],
    );
    assert_eq!(exit_of(&empty), 6, "{}", stderr_of(&empty));
    assert!(
        stderr_of(&empty).starts_with("error: push_empty:"),
        "{}",
        stderr_of(&empty)
    );

    let unknown = d.run_with_stdin(
        br#"{"moves":[{"from":"a.md","to":"b.md","keep":true}]}"#,
        &["--json", "commit", "--parent", HEAD_SHA, "--repo", REPO],
    );
    assert_eq!(exit_of(&unknown), 1);
    assert_eq!(one_document(&unknown)["code"], json!("invalid_changeset"));

    let empty_path = d.run_with_stdin(
        br#"{"copies":[{"from":"","to":"b.md"}]}"#,
        &["--json", "commit", "--parent", HEAD_SHA, "--repo", REPO],
    );
    assert_eq!(exit_of(&empty_path), 1);
    assert_eq!(
        one_document(&empty_path),
        json!({"error": "configuration error: path cannot be empty", "code": "invalid_changeset"})
    );
    assert!(d.requests().is_empty(), "{:?}", d.paths());
}

#[test]
#[serial]
fn row31_overlapping_members_are_refused_naming_the_path_before_any_request() {
    let d = Deployment::new();
    mount_repo(&d, HEAD_SHA);
    // Written as text: the document's own key order decides which path
    // is the first offending one.
    for (document, named) in [
        (r#"{"moves":[{"from":"docs","to":"docs/x"}]}"#, "docs/x"),
        (r#"{"moves":[{"from":"docs","to":"docs"}]}"#, "docs"),
        (r#"{"copies":[{"from":"docs/a","to":"docs"}]}"#, "docs"),
        (
            r#"{"moves":[{"from":"a","to":"b"},{"from":"b/c","to":"d"}]}"#,
            "b/c",
        ),
        (
            r#"{"moves":[{"from":"a","to":"b"}],"copies":[{"from":"a/x","to":"c"}]}"#,
            "a/x",
        ),
        (
            r#"{"copies":[{"from":"a/x","to":"c"}],"moves":[{"from":"a","to":"b"}]}"#,
            "a",
        ),
        (
            r#"{"copies":[{"from":"t","to":"x"},{"from":"u","to":"x/y"}]}"#,
            "x/y",
        ),
        (
            r#"{"moves":[{"from":"a","to":"b"}],"deletions":[{"path":"a/x"}]}"#,
            "a/x",
        ),
        (
            r#"{"moves":[{"from":"a/x","to":"b"}],"deletions":[{"path":"a"}]}"#,
            "a",
        ),
        (
            r#"{"moves":[{"from":"a","to":"b"}],"deletions":[{"path":"b/x"}]}"#,
            "b/x",
        ),
        (
            r#"{"moves":[{"from":"a","to":"b"}],"files":[{"path":"a","content":"x"}]}"#,
            "a",
        ),
        (
            r#"{"copies":[{"from":"a","to":"b"}],"files":[{"path":"a/y","content":"x"}]}"#,
            "a/y",
        ),
        (
            r#"{"moves":[{"from":"a","to":"b/c"}],"files":[{"path":"b","content":"x"}]}"#,
            "b",
        ),
    ] {
        let out = d.run_with_stdin(
            document.as_bytes(),
            &["--json", "commit", "--parent", HEAD_SHA, "--repo", REPO],
        );
        assert_eq!(exit_of(&out), 1, "{document}: {}", stderr_of(&out));
        let refusal = one_document(&out);
        assert_eq!(refusal["code"], json!("invalid_changeset"), "{document}");
        let line = refusal["error"].as_str().unwrap();
        assert!(
            line.starts_with(&format!("configuration error: {named} overlaps ")),
            "{document}: {line}"
        );
    }
    assert!(d.requests().is_empty(), "{:?}", d.paths());
}

#[test]
#[serial]
fn row31_admitted_combinations_reach_the_push() {
    for document in [
        // two copies sharing a `from`
        json!({"copies": [{"from": "t", "to": "x"}, {"from": "t", "to": "y"}]}),
        // a deletion at or above a `to`
        json!({"moves": [{"from": "q3-retro", "to": "docs"}], "deletions": [{"path": "docs"}]}),
        json!({"moves": [{"from": "x", "to": "archive/x"}], "deletions": [{"path": "archive"}]}),
        // a file at or under a `to`
        json!({"moves": [{"from": "a.md", "to": "b.md"}], "files": [{"path": "b.md", "content": "x"}]}),
        json!({"moves": [{"from": "q3-review", "to": "q3-retro"}], "files": [{"path": "q3-retro/document.html", "content": "x"}]}),
    ] {
        let d = Deployment::new();
        mount_repo(&d, HEAD_SHA);
        let mut answer = json!({
            "commitSha": NEXT_SHA, "version": 9, "filesChanged": 1, "created": false,
        });
        for key in ["moves", "copies"] {
            if let Some(members) = document.get(key) {
                answer[key] = members
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|m| json!({"from": m["from"], "to": m["to"], "type": "dir", "files": 1}))
                    .collect();
            }
        }
        mount_push_answer(&d, 200, answer);
        let out = d.run_with_stdin(
            document.to_string().as_bytes(),
            &["commit", "--parent", HEAD_SHA, "--repo", REPO],
        );
        assert_eq!(exit_of(&out), 0, "{document}: {}", stderr_of(&out));
        assert_eq!(d.pushes().len(), 1, "{document}");
    }
}

// ---- row 32: an old server ---------------------------------------------------

#[test]
#[serial]
fn row32_an_answer_lacking_the_member_echo_is_undecodable_naming_the_version() {
    let d = Deployment::new();
    mount_repo(&d, HEAD_SHA);
    mount_push_answer(
        &d,
        200,
        json!({"commitSha": NEXT_SHA, "version": 9, "filesChanged": 1, "created": false}),
    );

    let out = d.run(&["mv", "a.md", "b.md", "--parent", HEAD_SHA, "--repo", REPO]);
    assert_eq!(exit_of(&out), 1, "{}", stderr_of(&out));
    let line = stderr_of(&out);
    assert!(
        line.starts_with("error: server error (200): invalid response body: "),
        "{line}"
    );
    assert!(line.contains("version 9"), "{line}");
    assert!(!line.contains("wrote version"), "{line}");

    let changeset = json!({
        "moves": [{"from": "a.md", "to": "b.md"}],
        "files": [{"path": "c.md", "content": "c"}],
    })
    .to_string();
    let json_out = d.run_with_stdin(
        changeset.as_bytes(),
        &["--json", "commit", "--parent", HEAD_SHA, "--repo", REPO],
    );
    assert_eq!(exit_of(&json_out), 1);
    let document = one_document(&json_out);
    assert_eq!(document["code"], json!("response_undecodable"));
    assert!(
        document["error"].as_str().unwrap().contains("version 9"),
        "{document}"
    );
}

// ---- row 33: history ---------------------------------------------------------

/// `EP-file-history` for `q3-review/document.html` after `q3-plan` moved
/// to `q3-review` at version 42.
fn followed_history() -> Value {
    json!({
        "data": [
            {
                "version": 42, "sha": "4242424242424242424242424242424242424242",
                "blobSha": "1111111111111111111111111111111111111111",
                "message": "Retitle: Q3 review", "author": "bart",
                "createdAt": "2026-10-23T10:00:00Z", "content": "<h1>Q3</h1>",
                "diff": "diff --git a/q3-plan/document.html b/q3-review/document.html\nsimilarity index 100%\nrename from q3-plan/document.html\nrename to q3-review/document.html\n",
                "path": "q3-review/document.html", "movedFrom": "q3-plan/document.html",
            },
            {
                "version": 40, "sha": "4040404040404040404040404040404040404040",
                "blobSha": "1111111111111111111111111111111111111111",
                "message": "Tighten the intro", "author": "bart",
                "createdAt": "2026-10-22T10:00:00Z", "content": "<h1>Q3</h1>",
                "diff": "--- a/q3-plan/document.html\n+++ b/q3-plan/document.html\n@@ -1 +1 @@\n-<h1>Q</h1>\n+<h1>Q3</h1>\n",
                "path": "q3-plan/document.html", "movedFrom": null,
            }
        ],
        "total": 2, "limit": 50, "offset": 0
    })
}

fn mount_followed_history(d: &Deployment) {
    d.mount(
        Mock::given(method("GET"))
            .and(path_matcher(format!(
                "/api/v1/repos/{REPO}/files/q3-review/document.html/history"
            )))
            .respond_with(ResponseTemplate::new(200).set_body_json(followed_history())),
    );
}

/// The line of the rendered table holding `needle`.
fn line_holding(stdout: &str, needle: &str) -> String {
    stdout
        .lines()
        .find(|line| line.contains(needle))
        .unwrap_or_else(|| panic!("no line holds {needle}: {stdout}"))
        .to_string()
}

#[test]
#[serial]
fn row33_history_marks_the_move_in_its_message_cell() {
    let d = Deployment::new();
    mount_followed_history(&d);

    let out = d.run(&[
        "history",
        "--file",
        "q3-review/document.html",
        "--repo",
        REPO,
    ]);

    assert_eq!(exit_of(&out), 0, "{}", stderr_of(&out));
    let stdout = stdout_of(&out);
    assert!(
        line_holding(&stdout, "42424242")
            .contains("(moved from q3-plan/document.html) Retitle: Q3 review"),
        "{stdout}"
    );
    assert!(
        !line_holding(&stdout, "40404040").contains("moved from"),
        "{stdout}"
    );

    let json_out = d.run(&[
        "--json",
        "history",
        "--file",
        "q3-review/document.html",
        "--repo",
        REPO,
    ]);
    assert_eq!(exit_of(&json_out), 0);
    assert_eq!(
        one_document(&json_out),
        followed_history(),
        "passed through"
    );
}

#[test]
#[serial]
fn row33_inside_a_scoped_folder_paths_are_counted_from_it_where_under_it() {
    let d = Deployment::new();
    mount_followed_history(&d);
    let folder = d.work.path().join("q3-review");
    std::fs::create_dir_all(&folder).unwrap();
    std::fs::write(
        folder.join(".syns.yaml"),
        "holder: alice/notes\npath: q3-review\n",
    )
    .unwrap();

    let out = d.run_in(&folder, b"", &["history", "--file", "document.html"]);
    assert_eq!(exit_of(&out), 0, "{}", stderr_of(&out));
    let stdout = stdout_of(&out);
    // `q3-plan/document.html` lies outside the folder: whole.
    assert!(
        line_holding(&stdout, "42424242").contains("(moved from q3-plan/document.html) Retitle"),
        "{stdout}"
    );

    let json_out = d.run_in(
        &folder,
        b"",
        &["--json", "history", "--file", "document.html"],
    );
    assert_eq!(exit_of(&json_out), 0, "{}", stderr_of(&json_out));
    let document = one_document(&json_out);
    assert_eq!(document["data"][0]["path"], json!("document.html"));
    assert_eq!(
        document["data"][0]["movedFrom"],
        json!("q3-plan/document.html")
    );
    assert_eq!(document["data"][1]["path"], json!("q3-plan/document.html"));
    assert!(document["data"][1]["movedFrom"].is_null());
}

#[test]
#[serial]
fn row33_a_move_inside_the_scoped_folder_is_named_counted_from_it() {
    let d = Deployment::new();
    let mut page = followed_history();
    page["data"][0]["path"] = json!("q3-review/final.html");
    page["data"][0]["movedFrom"] = json!("q3-review/document.html");
    page["data"][1]["path"] = json!("q3-review/document.html");
    d.mount(
        Mock::given(method("GET"))
            .and(path_matcher(format!(
                "/api/v1/repos/{REPO}/files/q3-review/final.html/history"
            )))
            .respond_with(ResponseTemplate::new(200).set_body_json(page)),
    );
    let folder = d.work.path().join("q3-review");
    std::fs::create_dir_all(&folder).unwrap();
    std::fs::write(
        folder.join(".syns.yaml"),
        "holder: alice/notes\npath: q3-review\n",
    )
    .unwrap();

    let out = d.run_in(&folder, b"", &["history", "--file", "final.html"]);
    assert_eq!(exit_of(&out), 0, "{}", stderr_of(&out));
    assert!(
        line_holding(&stdout_of(&out), "42424242").contains("(moved from document.html) Retitle"),
        "{}",
        stdout_of(&out)
    );
    let json_out = d.run_in(&folder, b"", &["--json", "history", "--file", "final.html"]);
    let document = one_document(&json_out);
    assert_eq!(document["data"][0]["path"], json!("final.html"));
    assert_eq!(document["data"][0]["movedFrom"], json!("document.html"));
    assert_eq!(document["data"][1]["path"], json!("document.html"));
}

/// `EP-versions?path=q3-review` after the folder move (FEATURE_SPEC row
/// 22): the move, then a version that changed `q3-plan`.
fn followed_folder_page() -> Value {
    json!({
        "data": [
            {
                "version": 42, "sha": "4242424242424242424242424242424242424242",
                "parentSha": "4141414141414141414141414141414141414141",
                "message": "Retitle: Q3 review", "messageBody": null, "author": "bart",
                "createdAt": "2026-10-23T10:00:00Z",
                "filesChanged": ["q3-review/comments.html", "q3-review/document.html"],
                "path": "q3-review", "movedFrom": "q3-plan",
            },
            {
                "version": 40, "sha": "4040404040404040404040404040404040404040",
                "parentSha": null,
                "message": "Tighten the intro", "messageBody": null, "author": "bart",
                "createdAt": "2026-10-22T10:00:00Z",
                "filesChanged": ["q3-plan/document.html"],
                "path": "q3-plan",
            }
        ],
        "total": 2, "limit": 50, "offset": 0
    })
}

#[test]
#[serial]
fn a_folder_history_across_a_move_reads_each_version_at_its_matched_path() {
    let d = Deployment::new();
    // No file stands at `q3-review`: the file history answers no entry.
    d.mount(
        Mock::given(method("GET"))
            .and(path_matcher(format!(
                "/api/v1/repos/{REPO}/files/q3-review/history"
            )))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "data": [], "total": 0, "limit": 50, "offset": 0
            }))),
    );
    d.mount(
        Mock::given(method("GET"))
            .and(path_matcher(format!("/api/v1/repos/{REPO}/versions")))
            .and(query_param("path", "q3-review"))
            .respond_with(ResponseTemplate::new(200).set_body_json(followed_folder_page())),
    );

    let out = d.run(&["history", "--file", "q3-review", "--repo", REPO]);
    assert_eq!(exit_of(&out), 0, "{}", stderr_of(&out));
    let stdout = stdout_of(&out);
    assert!(
        line_holding(&stdout, "42424242").contains("(moved from q3-plan) Retitle: Q3 review"),
        "{stdout}"
    );
    assert!(line_holding(&stdout, "40404040").contains("Tighten the intro"));

    let json_out = d.run(&["--json", "history", "--file", "q3-review", "--repo", REPO]);
    assert_eq!(exit_of(&json_out), 0, "{}", stderr_of(&json_out));
    let document = one_document(&json_out);
    assert_eq!(document["data"][0]["movedFrom"], json!("q3-plan"));
    assert_eq!(document["data"][1]["path"], json!("q3-plan"));
    assert_eq!(
        document["data"][1]["filesChanged"],
        json!(["q3-plan/document.html"])
    );
}

#[test]
#[serial]
fn a_folder_history_of_a_server_predating_moves_keeps_its_shape() {
    // A page carrying neither `path` nor `movedFrom`: the document written
    // carries neither key, as before this item.
    let d = Deployment::new();
    d.mount(
        Mock::given(method("GET"))
            .and(path_matcher(format!(
                "/api/v1/repos/{REPO}/files/q3-plan/history"
            )))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "data": [], "total": 0, "limit": 50, "offset": 0
            }))),
    );
    let mut page = followed_folder_page();
    page["data"] = json!([page["data"][1].clone()]);
    page["data"][0].as_object_mut().unwrap().remove("path");
    d.mount(
        Mock::given(method("GET"))
            .and(path_matcher(format!("/api/v1/repos/{REPO}/versions")))
            .and(query_param("path", "q3-plan"))
            .respond_with(ResponseTemplate::new(200).set_body_json(page)),
    );

    let out = d.run(&["--json", "history", "--file", "q3-plan", "--repo", REPO]);
    assert_eq!(exit_of(&out), 0, "{}", stderr_of(&out));
    let document = one_document(&out);
    assert!(document["data"][0].get("path").is_none(), "{document}");
    assert!(document["data"][0].get("movedFrom").is_none(), "{document}");
}

// ---- verifier row 38, the CLI's side: an older entry read where it stood --

#[test]
#[serial]
fn an_older_entry_is_read_at_its_path_and_version() {
    // The `--json` history entry of version 40 names `path`
    // `q3-plan/document.html`; `syns cat` reads it there at that version.
    let d = Deployment::new();
    mount_version(&d, "40", 40, "4040404040404040404040404040404040404040");
    d.mount(
        Mock::given(method("GET"))
            .and(path_matcher(format!(
                "/api/v1/repos/{REPO}/raw/q3-plan/document.html"
            )))
            .and(query_param("ref", "40"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header(
                        "ETag",
                        format!("\"{}\"", syns_cli::push::hash::blob_sha1(b"<h1>Q3</h1>")).as_str(),
                    )
                    .set_body_bytes(b"<h1>Q3</h1>".to_vec()),
            ),
    );

    let out = d.run(&[
        "cat",
        "q3-plan/document.html",
        "--version",
        "40",
        "--repo",
        REPO,
    ]);
    assert_eq!(exit_of(&out), 0, "{}", stderr_of(&out));
    assert_eq!(stdout_of(&out), "<h1>Q3</h1>");
}

// ---- fix round 1 (REVIEW-syns-cli I1, M1, M2, M3) -------------------------

// I1: `cli-commands/commit` orders the checkout guard ahead of the
// document, so a dirty checkout handed an overlapping, empty or
// unparsable document answers `checkout_dirty`, with no request (a root
// checkout's guard reads nothing from the server).
#[test]
#[serial]
fn a_dirty_checkout_is_refused_ahead_of_the_document() {
    let d = Deployment::new();
    seed_checkout(&d, d.work.path());
    std::fs::write(d.work.path().join("q3-plan/document.html"), "edited since").unwrap();
    mount_repo(&d, HEAD_SHA);

    for document in [
        &br#"{"moves":[{"from":"docs","to":"docs/x"}]}"#[..],
        &br#"{"moves":[]}"#[..],
        &b"not json"[..],
    ] {
        let out = d.run_with_stdin(document, &["--json", "commit", "--parent", HEAD_SHA]);
        assert_eq!(exit_of(&out), 1, "{}", stderr_of(&out));
        assert_eq!(
            one_document(&out)["code"],
            json!("checkout_dirty"),
            "{}",
            String::from_utf8_lossy(document)
        );
    }
    assert!(d.requests().is_empty(), "{:?}", d.paths());

    // A clean checkout reaches the document's own refusal, still with no
    // request.
    let clean = Deployment::new();
    seed_checkout(&clean, clean.work.path());
    mount_repo(&clean, HEAD_SHA);
    let out = clean.run_with_stdin(
        br#"{"moves":[{"from":"docs","to":"docs/x"}]}"#,
        &["--json", "commit", "--parent", HEAD_SHA],
    );
    assert_eq!(exit_of(&out), 1, "{}", stderr_of(&out));
    assert_eq!(one_document(&out)["code"], json!("invalid_changeset"));
    assert!(clean.requests().is_empty(), "{:?}", clean.paths());
}

// M1 (D-256 §7): every binding cleans a member path alike — what the
// server trims, empty segments, a leading `/` — so the overlap check
// sees the path the server would.
#[test]
#[serial]
fn member_paths_are_cleaned_alike_under_every_binding() {
    let d = Deployment::new();
    mount_repo(&d, HEAD_SHA);
    for (from, to, line) in [
        (
            "a//b",
            "a/b",
            "error: configuration error: cannot move a/b to a/b: they are the same path\n",
        ),
        (
            "a ",
            "a",
            "error: configuration error: cannot move a to a: they are the same path\n",
        ),
        (
            "/a",
            "b",
            "error: configuration error: /a names no path inside the repository; name one counted from its root\n",
        ),
    ] {
        let out = d.run(&["mv", from, to, "--parent", HEAD_SHA, "--repo", REPO]);
        assert_eq!(exit_of(&out), 1, "{from:?}: {}", stderr_of(&out));
        assert_eq!(stderr_of(&out), line, "{from:?}");
    }
    let out = d.run_with_stdin(
        br#"{"moves":[{"from":"a//b","to":"c"}],"deletions":[{"path":"a/b/x"}]}"#,
        &["--json", "commit", "--parent", HEAD_SHA, "--repo", REPO],
    );
    assert_eq!(exit_of(&out), 1, "{}", stderr_of(&out));
    assert!(
        one_document(&out)["error"]
            .as_str()
            .unwrap()
            .starts_with("configuration error: a/b/x overlaps a/b "),
        "{}",
        stdout_of(&out)
    );
    assert!(d.requests().is_empty(), "{:?}", d.paths());

    // Cleaned paths are the ones sent.
    mount_push_answer(
        &d,
        200,
        member_answer(
            8,
            1,
            "moves",
            json!([{"from": "a/b", "to": "c", "type": "file", "files": 1}]),
        ),
    );
    let out = d.run(&["mv", "a//b", "c ", "--parent", HEAD_SHA, "--repo", REPO]);
    assert_eq!(exit_of(&out), 0, "{}", stderr_of(&out));
    assert_eq!(d.pushes()[0]["moves"], json!([{"from": "a/b", "to": "c"}]));
}

// M2 (D-256 §6, the scoped-folder half): inside a folder bound to its
// holder, a folder page across a move keeps a changed path outside the
// folder whole where it lies at or under the version's matched `path`,
// and counts `path` and `movedFrom` from the folder where under it.
#[test]
#[serial]
fn a_folder_history_inside_a_scoped_folder_counts_followed_paths_from_it() {
    let d = Deployment::new();
    d.mount(
        Mock::given(method("GET"))
            .and(path_matcher(format!(
                "/api/v1/repos/{REPO}/files/work/q3-review/history"
            )))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "data": [], "total": 0, "limit": 50, "offset": 0
            }))),
    );
    let entry = |version: u32, path: &str, moved_from: Option<&str>, changed: Value| {
        let mut entry = json!({
            "version": version, "sha": format!("{version:02}").repeat(20),
            "parentSha": null, "message": format!("v{version}"), "messageBody": null,
            "author": "bart", "createdAt": "2026-10-23T10:00:00Z",
            "filesChanged": changed, "path": path,
        });
        if let Some(from) = moved_from {
            entry["movedFrom"] = json!(from);
        }
        entry
    };
    d.mount(
        Mock::given(method("GET"))
            .and(path_matcher(format!("/api/v1/repos/{REPO}/versions")))
            .and(query_param("path", "work/q3-review"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "data": [
                    entry(42, "work/q3-review", Some("work/q3-plan"), json!(["work/q3-review/document.html"])),
                    entry(40, "work/q3-plan", Some("archive/q3"), json!(["work/q3-plan/document.html"])),
                    entry(30, "archive/q3", None, json!(["archive/q3/x.html", "unrelated/y.md"])),
                ],
                "total": 3, "limit": 50, "offset": 0
            }))),
    );
    let folder = d.work.path().join("work");
    std::fs::create_dir_all(&folder).unwrap();
    std::fs::write(
        folder.join(".syns.yaml"),
        "holder: alice/notes\npath: work\n",
    )
    .unwrap();

    let json_out = d.run_in(&folder, b"", &["--json", "history", "--file", "q3-review"]);
    assert_eq!(exit_of(&json_out), 0, "{}", stderr_of(&json_out));
    let document = one_document(&json_out);
    assert_eq!(document["data"][0]["path"], json!("q3-review"));
    assert_eq!(document["data"][0]["movedFrom"], json!("q3-plan"));
    assert_eq!(
        document["data"][0]["filesChanged"],
        json!(["q3-review/document.html"])
    );
    assert_eq!(document["data"][1]["movedFrom"], json!("archive/q3"));
    assert_eq!(
        document["data"][1]["filesChanged"],
        json!(["q3-plan/document.html"])
    );
    assert_eq!(document["data"][2]["path"], json!("archive/q3"));
    assert_eq!(
        document["data"][2]["filesChanged"],
        json!(["archive/q3/x.html"]),
        "a path at the matched path outside the folder stands whole; an unrelated one is dropped"
    );

    let out = d.run_in(&folder, b"", &["history", "--file", "q3-review"]);
    assert_eq!(exit_of(&out), 0, "{}", stderr_of(&out));
    let stdout = stdout_of(&out);
    assert!(
        line_holding(&stdout, "v42").contains("(moved from q3-plan) v42"),
        "{stdout}"
    );
    assert!(
        line_holding(&stdout, "v40").contains("(moved from archive/q3) v40"),
        "{stdout}"
    );
    assert!(
        line_holding(&stdout, "30303030").contains("1 file"),
        "{stdout}"
    );
}

// M3: a copy that leaves the tree as it stood reads as the one
// nothing-changed line, no member line beside it.
#[test]
#[serial]
fn a_copy_that_changes_nothing_reads_as_nothing_changed_alone() {
    let d = Deployment::new();
    mount_repo(&d, HEAD_SHA);
    d.mount(
        Mock::given(method("PUT"))
            .and(path_matcher(format!("/api/v1/repos/{REPO}/push")))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "commitSha": HEAD_SHA, "version": 7, "filesChanged": 0, "created": false,
                "copies": [{"from": "a.md", "to": "b.md", "type": "file", "files": 1}],
            }))),
    );

    let out = d.run(&["cp", "a.md", "b.md", "--parent", HEAD_SHA, "--repo", REPO]);

    assert_eq!(exit_of(&out), 0, "{}", stderr_of(&out));
    assert_eq!(
        stderr_of(&out),
        format!("nothing changed; the head is still version 7, commit {HEAD_SHA}\n")
    );
}
