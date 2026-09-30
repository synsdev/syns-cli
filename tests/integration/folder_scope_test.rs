//! Binary-level behaviour of the scoped folder: every scoped read, the
//! folder history, the refusals, and the commands left outside it (SPEC
//! u290 Tests).
//!
//! Every binary row of that table stands here under the name the table
//! gives it; the three rows driving `resolve_folder_scope` and
//! `lies_under` stand in the tests module of `src/repo/folder.rs`. Each
//! test drives one mock deployment of its own and runs the binary from a
//! checkout `W` whose `.syns.yaml` names `alice/work`, holding the folder
//! `W/clients/vela/q3-board` recording its own place and an empty `sub`
//! under it, unless its setup says otherwise.

use assert_cmd::Command as AssertCommand;
use serde_json::{Value, json};
use serial_test::serial;
use std::path::{Path, PathBuf};
use tempfile::TempDir;
use wiremock::matchers::{method, path as path_matcher, query_param};
use wiremock::{Mock, MockServer, Request, ResponseTemplate};

const REPO: &str = "alice/work";
const FOLDER: &str = "clients/vela/q3-board";
const FOLDER_YAML: &str = "holder: alice/work\npath: clients/vela/q3-board\n";

fn head_sha() -> String {
    "a".repeat(40)
}

/// One mock deployment, one config directory, one cache directory, and
/// the checkout `W` the Tests preamble fixes, taken through
/// `std::fs::canonicalize` so every directory a line names is the one
/// the binary sees.
struct Deployment {
    rt: tokio::runtime::Runtime,
    server: MockServer,
    home: TempDir,
    cache: TempDir,
    _work: TempDir,
    w: PathBuf,
}

impl Deployment {
    fn new() -> Deployment {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime");
        let server = rt.block_on(MockServer::start());
        let work = tempfile::tempdir().expect("working dir");
        let w = std::fs::canonicalize(work.path()).expect("canonical W");
        std::fs::write(w.join(".syns.yaml"), "owner: alice\nname: work\n").expect("W identity");
        std::fs::create_dir_all(w.join(FOLDER).join("sub")).expect("folder");
        std::fs::write(w.join(FOLDER).join(".syns.yaml"), FOLDER_YAML).expect("folder identity");
        Deployment {
            rt,
            server,
            home: tempfile::tempdir().expect("config dir"),
            cache: tempfile::tempdir().expect("cache dir"),
            _work: work,
            w,
        }
    }

    /// The deployment with a stored credential.
    fn with_credential() -> Deployment {
        let d = Deployment::new();
        std::fs::write(
            d.home.path().join("credentials.json"),
            json!({"token": "test-token", "username": "alice"}).to_string(),
        )
        .expect("credential");
        d
    }

    fn folder(&self) -> PathBuf {
        self.w.join(FOLDER)
    }

    fn mount(&self, mock: Mock) {
        self.rt.block_on(async { mock.mount(&self.server).await });
    }

    fn requests(&self) -> Vec<Request> {
        self.rt
            .block_on(async { self.server.received_requests().await.expect("requests") })
    }

    /// Every request whose address ends with `suffix`.
    fn requests_to(&self, suffix: &str) -> Vec<Request> {
        self.requests()
            .into_iter()
            .filter(|r| r.url.path().ends_with(suffix))
            .collect()
    }

    fn run_in(&self, cwd: &Path, stdin: &[u8], args: &[&str]) -> std::process::Output {
        AssertCommand::cargo_bin("syns")
            .expect("syns binary")
            .current_dir(cwd)
            .env("SYNS_CONFIG_DIR", self.home.path())
            .env("SYNS_CACHE_DIR", self.cache.path())
            .env_remove("SYNS_URL")
            .env_remove("SYNS_INTEGRATION")
            .env_remove("SYNS_RUN")
            .env_remove("SYNS_TRIGGER")
            .env_remove("SYNS_TASK")
            .arg("--server")
            .arg(self.server.uri())
            .args(args)
            .write_stdin(stdin.to_vec())
            .output()
            .expect("run syns")
    }

    /// The head at version `7` of `alice/work`, and version `7` named by
    /// its hash — the reference a read with no `--version` resolves.
    fn mount_reference(&self) {
        self.mount(
            Mock::given(method("GET"))
                .and(path_matcher(format!("/api/v1/repos/{REPO}")))
                .respond_with(ResponseTemplate::new(200).set_body_json(repo_body())),
        );
        self.mount(
            Mock::given(method("GET"))
                .and(path_matcher(format!(
                    "/api/v1/repos/{REPO}/versions/{}",
                    head_sha()
                )))
                .respond_with(ResponseTemplate::new(200).set_body_json(version_body(
                    7,
                    &head_sha(),
                    &["a.md"],
                ))),
        );
    }

    fn mount_versions(&self, body: Value) {
        self.mount(
            Mock::given(method("GET"))
                .and(path_matcher(format!("/api/v1/repos/{REPO}/versions")))
                .respond_with(ResponseTemplate::new(200).set_body_json(body)),
        );
    }

    fn mount_file_history(&self, path: &str, body: Value) {
        self.mount(
            Mock::given(method("GET"))
                .and(path_matcher(format!(
                    "/api/v1/repos/{REPO}/files/{path}/history"
                )))
                .respond_with(ResponseTemplate::new(200).set_body_json(body)),
        );
    }
}

fn repo_body() -> Value {
    json!({
        "owner": "alice", "name": "work", "description": null,
        "commitSha": head_sha(), "status": "active", "author": null, "tags": [],
        "visibility": "public", "forkedFrom": null, "forkCount": 0,
        "fileCount": 3, "role": null,
        "createdAt": "2026-01-01T00:00:00Z", "updatedAt": "2026-01-01T00:00:00Z",
    })
}

fn version_body(version: u32, sha: &str, files: &[&str]) -> Value {
    json!({
        "version": version, "sha": sha, "parentSha": null, "message": "m",
        "messageBody": null, "author": "alice",
        "createdAt": "2026-01-01T00:00:00Z", "filesChanged": files,
    })
}

fn tree_entry(path: &str, kind: &str) -> Value {
    let name = path.rsplit('/').next().unwrap_or(path);
    match kind {
        "dir" => json!({ "name": name, "path": path, "type": "dir", "size": null, "sha": null }),
        _ => json!({ "name": name, "path": path, "type": "file", "size": 6, "sha": null }),
    }
}

fn tree_body(entries: Vec<Value>) -> Value {
    json!({ "entries": entries, "commitSha": head_sha(), "truncated": false })
}

fn file_history_entry(version: u32, diff: &str) -> Value {
    json!({
        "version": version, "sha": format!("{version}").repeat(8), "blobSha": "b",
        "message": "m", "author": "alice", "createdAt": "2026-01-01T00:00:00Z",
        "content": "c", "diff": diff,
    })
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

fn query_of(request: &Request, key: &str) -> Option<String> {
    request
        .url
        .query_pairs()
        .find(|(k, _)| k == key)
        .map(|(_, v)| v.into_owned())
}

/// Every file under `root` with its bytes, in path order.
fn snapshot(root: &Path) -> Vec<(PathBuf, Vec<u8>)> {
    let mut files = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        for entry in std::fs::read_dir(&dir).expect("read dir") {
            let path = entry.expect("entry").path();
            if path.is_dir() {
                stack.push(path);
            } else {
                let bytes = std::fs::read(&path).expect("read file");
                files.push((path, bytes));
            }
        }
    }
    files.sort();
    files
}

#[test]
#[serial]
fn ls_inside_a_folder_lists_the_folder_counted_from_it() {
    let d = Deployment::new();
    d.mount_reference();
    d.mount(
        Mock::given(method("GET"))
            .and(path_matcher(format!("/api/v1/repos/{REPO}/tree/{FOLDER}")))
            .respond_with(ResponseTemplate::new(200).set_body_json(tree_body(vec![
                tree_entry(&format!("{FOLDER}/.page"), "dir"),
                tree_entry(&format!("{FOLDER}/.syns.yaml"), "file"),
            ]))),
    );

    let out = d.run_in(&d.folder().join("sub"), b"", &["--json", "ls"]);

    assert_eq!(exit_of(&out), 0, "{}", stderr_of(&out));
    let trees: Vec<Request> = d
        .requests()
        .into_iter()
        .filter(|r| r.url.path().contains("/tree"))
        .collect();
    assert_eq!(trees.len(), 1);
    assert_eq!(
        trees[0].url.path(),
        format!("/api/v1/repos/{REPO}/tree/{FOLDER}")
    );
    let document = one_document(&out);
    let paths: Vec<&str> = document["entries"]
        .as_array()
        .expect("entries")
        .iter()
        .map(|e| e["path"].as_str().expect("path"))
        .collect();
    assert_eq!(paths, vec![".page", ".syns.yaml"]);
}

#[test]
#[serial]
fn every_read_inside_a_folder_answers_paths_counted_from_it() {
    let d = Deployment::new();
    let sha3 = "3".repeat(40);
    d.mount(
        Mock::given(method("GET"))
            .and(path_matcher(format!("/api/v1/repos/{REPO}/versions/3")))
            .respond_with(ResponseTemplate::new(200).set_body_json(version_body(
                3,
                &sha3,
                &[".page/board.json"],
            ))),
    );
    d.mount(
        Mock::given(method("GET"))
            .and(path_matcher(format!("/api/v1/repos/{REPO}/tree/{FOLDER}")))
            .and(query_param("ref", "3"))
            .respond_with(ResponseTemplate::new(200).set_body_json(tree_body(vec![
                tree_entry(&format!("{FOLDER}/.page"), "dir"),
                tree_entry(&format!("{FOLDER}/.page/board.json"), "file"),
                tree_entry(&format!("{FOLDER}/notes"), "dir"),
                tree_entry(&format!("{FOLDER}/notes/a.md"), "file"),
            ]))),
    );
    for file in [".page/board.json", "notes/a.md"] {
        d.mount(
            Mock::given(method("GET"))
                .and(path_matcher(format!(
                    "/api/v1/repos/{REPO}/files/{FOLDER}/{file}"
                )))
                .and(query_param("ref", "3"))
                .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                    "path": format!("{FOLDER}/{file}"), "content": "fn one",
                    "sha": "b".repeat(40), "size": 6,
                }))),
        );
    }
    let etag = syns_cli::push::hash::blob_sha1(b"fn one");
    d.mount(
        Mock::given(method("GET"))
            .and(path_matcher(format!(
                "/api/v1/repos/{REPO}/raw/{FOLDER}/.page/board.json"
            )))
            .and(query_param("ref", "3"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("Content-Type", "text/plain")
                    .insert_header("ETag", format!("\"{etag}\"").as_str())
                    .set_body_bytes(b"fn one".to_vec()),
            ),
    );

    let runs: Vec<Vec<&str>> = vec![
        vec!["--json", "ls", "--recursive", "--version", "3"],
        vec!["--json", "glob", "**/*.json", "--version", "3"],
        vec!["--json", "grep", "fn ", "--version", "3"],
        vec!["--json", "read", ".page/board.json", "--version", "3"],
        vec!["--json", "cat", ".page/board.json", "--version", "3"],
    ];
    let mut documents = Vec::new();
    for args in &runs {
        let out = d.run_in(&d.folder(), b"", args);
        assert_eq!(exit_of(&out), 0, "{args:?}: {}", stderr_of(&out));
        let text = stdout_of(&out);
        assert!(
            !text.contains("\"clients/"),
            "{args:?} named a path from the holder root: {text}"
        );
        documents.push(one_document(&out));
    }

    assert_eq!(documents[0]["version"], json!(3));
    assert_eq!(
        documents[1]["matches"].as_array().expect("matches").len(),
        1
    );
    assert_eq!(
        documents[1]["matches"][0]["path"],
        json!(".page/board.json")
    );
    assert_eq!(documents[3]["path"], json!(".page/board.json"));
    assert_eq!(documents[4]["path"], json!(".page/board.json"));
    for request in d.requests() {
        let address = request.url.path();
        assert!(
            !address.ends_with("/versions"),
            "a version-list request: {address}"
        );
        if address.contains("/tree/") || address.contains("/files/") || address.contains("/raw/") {
            assert!(
                address.contains(&format!("/{FOLDER}/")) || address.ends_with(FOLDER),
                "{address}"
            );
            assert_eq!(query_of(&request, "ref").as_deref(), Some("3"), "{address}");
        }
    }
}

#[test]
#[serial]
fn a_path_leaving_the_folder_is_refused() {
    let d = Deployment::new();

    for (args, typed) in [
        (vec!["cat", "../../x.md"], "../../x.md"),
        (vec!["ls", "/x"], "/x"),
    ] {
        let out = d.run_in(&d.folder(), b"", &args);
        assert_eq!(exit_of(&out), 1, "{args:?}");
        let err = stderr_of(&out);
        assert_eq!(
            err.trim_end(),
            format!(
                "error: configuration error: {typed} names no path inside the folder {}; name one counted from it",
                d.folder().display()
            )
        );
    }
    assert!(d.requests().is_empty());
}

#[test]
#[serial]
fn history_inside_a_folder_lists_the_versions_that_changed_it() {
    let d = Deployment::new();
    let mut five = version_body(5, &"5".repeat(40), &[&format!("{FOLDER}/.page/board.json")]);
    five["parentSha"] = json!("p4");
    five["provenance"] = json!({
        "publisher": "alice", "integration": "bb", "run": "r1",
        "trigger": null, "taskRef": null,
    });
    let three = version_body(3, &"3".repeat(40), &[&format!("{FOLDER}/notes/a.md")]);
    d.mount_versions(json!({ "data": [five, three], "total": 2, "limit": 50, "offset": 0 }));

    let out = d.run_in(&d.folder(), b"", &["--json", "history"]);

    assert_eq!(exit_of(&out), 0, "{}", stderr_of(&out));
    let lists = d.requests_to("/versions");
    assert_eq!(lists.len(), 1);
    assert_eq!(query_of(&lists[0], "path").as_deref(), Some(FOLDER));
    assert_eq!(query_of(&lists[0], "limit").as_deref(), Some("50"));
    assert_eq!(query_of(&lists[0], "offset").as_deref(), Some("0"));
    assert!(d.requests_to("/history").is_empty());
    let document = one_document(&out);
    let versions: Vec<u64> = document["data"]
        .as_array()
        .expect("data")
        .iter()
        .map(|v| v["version"].as_u64().expect("version"))
        .collect();
    assert_eq!(versions, vec![5, 3]);
    assert_eq!(
        document["data"][0]["filesChanged"],
        json!([".page/board.json"])
    );
    assert_eq!(document["data"][1]["filesChanged"], json!(["notes/a.md"]));
    assert_eq!(document["data"][0]["parentSha"], json!("p4"));
    assert_eq!(
        document["data"][0]["provenance"]["integration"],
        json!("bb")
    );
    assert_eq!(document["data"][0]["provenance"]["run"], json!("r1"));
    assert_eq!(document["total"], json!(2));

    let paged = d.run_in(
        &d.folder(),
        b"",
        &["--json", "history", "--limit", "1", "--offset", "1"],
    );
    assert_eq!(exit_of(&paged), 0, "{}", stderr_of(&paged));
    let lists = d.requests_to("/versions");
    assert_eq!(lists.len(), 2);
    assert_eq!(query_of(&lists[1], "limit").as_deref(), Some("1"));
    assert_eq!(query_of(&lists[1], "offset").as_deref(), Some("1"));
}

#[test]
#[serial]
fn file_history_inside_a_folder_counts_its_diff_paths_from_it() {
    let d = Deployment::new();
    let full = format!("{FOLDER}/.page/board.json");
    d.mount_file_history(
        &full,
        json!({
            "data": [file_history_entry(
                4,
                &format!("diff --git a/{full} b/{full}\nindex 1..2 100644\n--- a/{full}\n+++ b/{full}\n@@ -1 +1 @@\n-a\n+b"),
            )],
            "total": 1, "limit": 50, "offset": 0,
        }),
    );

    let out = d.run_in(
        &d.folder(),
        b"",
        &["--json", "history", "--file", ".page/board.json"],
    );

    assert_eq!(exit_of(&out), 0, "{}", stderr_of(&out));
    let asked = d.requests_to("/history");
    assert_eq!(asked.len(), 1);
    assert_eq!(
        asked[0].url.path(),
        format!("/api/v1/repos/{REPO}/files/{full}/history")
    );
    let document = one_document(&out);
    let diff = document["data"][0]["diff"].as_str().expect("diff");
    assert!(
        diff.starts_with("diff --git a/.page/board.json b/.page/board.json\n"),
        "{diff}"
    );
    assert!(
        diff.contains("--- a/.page/board.json\n+++ b/.page/board.json\n"),
        "{diff}"
    );
}

#[test]
#[serial]
fn history_of_a_folder_from_the_root_lists_every_version_under_it() {
    let d = Deployment::new();
    d.mount_file_history(
        "q3-plan",
        json!({ "data": [], "total": 0, "limit": 50, "offset": 0 }),
    );
    d.mount_versions(json!({
        "data": [
            version_body(4, &"4".repeat(40), &["q3-plan/images/fig.png"]),
            version_body(3, &"3".repeat(40), &["q3-plan/comments.html"]),
            version_body(1, &"1".repeat(40), &["q3-plan/document.html", "q3-plan/comments.html"]),
        ],
        "total": 3, "limit": 50, "offset": 0,
    }));

    let out = d.run_in(&d.w, b"", &["--json", "history", "--file", "q3-plan"]);

    assert_eq!(exit_of(&out), 0, "{}", stderr_of(&out));
    let lists = d.requests_to("/versions");
    assert_eq!(lists.len(), 1);
    assert_eq!(query_of(&lists[0], "path").as_deref(), Some("q3-plan"));
    let document = one_document(&out);
    let versions: Vec<u64> = document["data"]
        .as_array()
        .expect("data")
        .iter()
        .map(|v| v["version"].as_u64().expect("version"))
        .collect();
    assert_eq!(versions, vec![4, 3, 1]);
    assert_eq!(
        document["data"][0]["filesChanged"],
        json!(["q3-plan/images/fig.png"])
    );
    assert_eq!(
        document["data"][2]["filesChanged"],
        json!(["q3-plan/document.html", "q3-plan/comments.html"])
    );
    assert_eq!(document["total"], json!(3));
}

#[test]
#[serial]
fn history_of_a_file_answers_the_file_history() {
    let d = Deployment::new();
    let body = json!({
        "data": [file_history_entry(2, "--- a/a.md\n+++ b/a.md"), file_history_entry(1, "d")],
        "total": 2, "limit": 50, "offset": 0,
    });
    d.mount_file_history("a.md", body.clone());

    let out = d.run_in(&d.w, b"", &["--json", "history", "--file", "a.md"]);

    assert_eq!(exit_of(&out), 0, "{}", stderr_of(&out));
    assert_eq!(one_document(&out), body);
    assert!(d.requests_to("/versions").is_empty());
}

#[test]
#[serial]
fn folder_history_sends_one_request_whatever_the_history_length() {
    let d = Deployment::new();
    let rows: Vec<Value> = (0..20)
        .map(|n| version_body(20 - n, &"c".repeat(40), &[&format!("{FOLDER}/a.md")]))
        .collect();
    d.mount_versions(json!({ "data": rows, "total": 2000, "limit": 20, "offset": 1980 }));

    let out = d.run_in(
        &d.folder(),
        b"",
        &["--json", "history", "--limit", "20", "--offset", "1980"],
    );

    assert_eq!(exit_of(&out), 0, "{}", stderr_of(&out));
    let lists = d.requests_to("/versions");
    assert_eq!(lists.len(), 1);
    assert_eq!(d.requests().len(), 1);
    assert_eq!(query_of(&lists[0], "limit").as_deref(), Some("20"));
    assert_eq!(query_of(&lists[0], "offset").as_deref(), Some("1980"));
    assert_eq!(one_document(&out)["total"], json!(2000));
}

#[test]
#[serial]
fn a_folder_page_carrying_a_version_outside_the_folder_is_refused() {
    let d = Deployment::new();
    d.mount_file_history(
        "q3-plan",
        json!({ "data": [], "total": 0, "limit": 50, "offset": 0 }),
    );
    d.mount_versions(json!({
        "data": [
            version_body(3, &"3".repeat(40), &["q3-plan/document.html"]),
            version_body(2, &"2".repeat(40), &["q3-plan-old/document.html"]),
        ],
        "total": 2, "limit": 50, "offset": 0,
    }));

    let json_run = d.run_in(&d.w, b"", &["--json", "history", "--file", "q3-plan"]);
    assert_eq!(exit_of(&json_run), 1);
    let document = one_document(&json_run);
    assert!(
        document["error"]
            .as_str()
            .expect("error")
            .starts_with("server error (200): invalid response body"),
        "{document}"
    );
    assert!(document.get("data").is_none());

    let human = d.run_in(&d.w, b"", &["history", "--file", "q3-plan"]);
    assert_eq!(exit_of(&human), 1);
    assert!(
        stderr_of(&human).starts_with("error: server error (200): invalid response body"),
        "{}",
        stderr_of(&human)
    );
    assert!(stdout_of(&human).is_empty(), "{}", stdout_of(&human));
}

#[test]
#[serial]
fn history_offset_reaches_the_file_and_whole_list_windows() {
    let d = Deployment::new();
    d.mount_file_history(
        "a.md",
        json!({ "data": [file_history_entry(7, "d")], "total": 12, "limit": 5, "offset": 5 }),
    );
    d.mount_versions(json!({
        "data": [version_body(7, &"7".repeat(40), &["a.md"])],
        "total": 12, "limit": 5, "offset": 5,
    }));

    let file = d.run_in(
        &d.w,
        b"",
        &[
            "--json", "history", "--file", "a.md", "--limit", "5", "--offset", "5",
        ],
    );
    assert_eq!(exit_of(&file), 0, "{}", stderr_of(&file));
    let whole = d.run_in(
        &d.w,
        b"",
        &["--json", "history", "--limit", "5", "--offset", "5"],
    );
    assert_eq!(exit_of(&whole), 0, "{}", stderr_of(&whole));

    let asked = d.requests_to("/history");
    assert_eq!(asked.len(), 1);
    let lists = d.requests_to("/versions");
    assert_eq!(lists.len(), 1);
    for request in [&asked[0], &lists[0]] {
        assert_eq!(query_of(request, "limit").as_deref(), Some("5"));
        assert_eq!(query_of(request, "offset").as_deref(), Some("5"));
    }
    assert_eq!(query_of(&lists[0], "path"), None);
}

#[test]
#[serial]
fn history_writes_its_count_after_a_partial_block() {
    let d = Deployment::new();
    let rows: Vec<Value> = (0..5)
        .map(|n| version_body(7 - n, &"d".repeat(40), &["a.md"]))
        .collect();
    d.mount_versions(json!({ "data": rows, "total": 7, "limit": 5, "offset": 0 }));

    let human = d.run_in(&d.w, b"", &["history", "--limit", "5"]);
    assert_eq!(exit_of(&human), 0, "{}", stderr_of(&human));
    assert_eq!(stderr_of(&human).trim_end(), "Showing 5 of 7 versions.");
    assert!(stdout_of(&human).contains("Version"));

    let json_run = d.run_in(&d.w, b"", &["--json", "history", "--limit", "5"]);
    assert_eq!(exit_of(&json_run), 0);
    assert!(!stderr_of(&json_run).contains("Showing"));
    assert!(!stdout_of(&json_run).contains("Showing"));
}

#[test]
#[serial]
fn history_limit_outside_the_page_bound_is_refused() {
    let d = Deployment::new();

    for n in ["0", "101"] {
        let out = d.run_in(&d.w, b"", &["history", "--limit", n]);
        assert_eq!(exit_of(&out), 1);
        assert_eq!(
            stderr_of(&out).trim_end(),
            format!("error: configuration error: --limit must be between 1 and 100 (got {n})")
        );
    }
    assert!(d.requests().is_empty());
}

#[test]
#[serial]
fn diff_inside_a_folder_answers_only_its_changes() {
    let d = Deployment::new();
    let board = format!("{FOLDER}/.page/board.json");
    d.mount(
        Mock::given(method("GET"))
            .and(path_matcher(format!("/api/v1/repos/{REPO}/diff")))
            .and(query_param("from", "4"))
            .and(query_param("to", "5"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "from": { "version": 4, "sha": "4".repeat(40) },
                "to": { "version": 5, "sha": "5".repeat(40) },
                "files": [
                    {
                        "path": board, "status": "modified",
                        "diff": format!("diff --git a/{board} b/{board}\nindex 1..2 100644\n--- a/{board}\n+++ b/{board}\n@@ -1 +1 @@\n-a\n+b"),
                    },
                    {
                        "path": ".page/board.json", "status": "modified",
                        "diff": "diff --git a/.page/board.json b/.page/board.json\n--- a/.page/board.json\n+++ b/.page/board.json\n@@ -1 +1 @@\n-a\n+b",
                    },
                    {
                        "path": format!("{FOLDER}/notes/n.md"), "oldPath": "inbox/n.md",
                        "status": "renamed",
                        "diff": format!("diff --git a/inbox/n.md b/{FOLDER}/notes/n.md\nsimilarity index 100%\nrename from inbox/n.md\nrename to {FOLDER}/notes/n.md\n"),
                    },
                    {
                        "path": "archive/old.md", "oldPath": format!("{FOLDER}/old.md"),
                        "status": "renamed",
                        "diff": format!("diff --git a/{FOLDER}/old.md b/archive/old.md\nsimilarity index 100%\nrename from {FOLDER}/old.md\nrename to archive/old.md\n"),
                    },
                ],
            }))),
    );

    let out = d.run_in(
        &d.folder(),
        b"",
        &["--json", "diff", "--from", "4", "--to", "5"],
    );

    assert_eq!(exit_of(&out), 0, "{}", stderr_of(&out));
    let document = one_document(&out);
    let files = document["files"].as_array().expect("files");
    assert_eq!(files.len(), 3, "{document}");
    assert_eq!(files[0]["path"], json!(".page/board.json"));
    assert_eq!(files[0]["status"], json!("modified"));
    let diff = files[0]["diff"].as_str().expect("diff");
    assert_eq!(
        diff,
        "diff --git a/.page/board.json b/.page/board.json\nindex 1..2 100644\n--- a/.page/board.json\n+++ b/.page/board.json\n@@ -1 +1 @@\n-a\n+b"
    );
    assert_eq!(files[1]["path"], json!("notes/n.md"));
    assert_eq!(files[1]["status"], json!("added"));
    assert!(files[1].get("oldPath").is_none());
    assert!(files[1]["diff"].is_null());
    assert_eq!(files[2]["path"], json!("old.md"));
    assert_eq!(files[2]["status"], json!("deleted"));
    assert!(files[2]["diff"].is_null());
}

#[test]
#[serial]
fn diff_inside_a_folder_over_versions_that_left_it_untouched_names_no_file() {
    let d = Deployment::new();
    d.mount(
        Mock::given(method("GET"))
            .and(path_matcher(format!("/api/v1/repos/{REPO}/diff")))
            .and(query_param("from", "3"))
            .and(query_param("to", "h7"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "from": { "version": 3, "sha": "h3" },
                "to": { "version": 7, "sha": "h7" },
                "files": [{
                    "path": ".page/board.json", "status": "modified",
                    "diff": "--- a/.page/board.json\n+++ b/.page/board.json\n@@ -1 +1 @@\n-a\n+b",
                }],
            }))),
    );

    let json_run = d.run_in(
        &d.folder(),
        b"",
        &["--json", "diff", "--from", "3", "--to", "h7"],
    );
    assert_eq!(exit_of(&json_run), 0, "{}", stderr_of(&json_run));
    let document = one_document(&json_run);
    assert_eq!(document["files"], json!([]));
    assert_eq!(document["from"]["version"], json!(3));
    assert_eq!(document["to"]["version"], json!(7));
    assert_eq!(document["to"]["sha"], json!("h7"));

    let human = d.run_in(&d.folder(), b"", &["diff", "--from", "3", "--to", "h7"]);
    assert_eq!(exit_of(&human), 0, "{}", stderr_of(&human));
    assert!(
        format!("{}{}", stdout_of(&human), stderr_of(&human)).contains("No differences found."),
        "{}",
        stdout_of(&human)
    );
}

#[test]
#[serial]
fn diff_inside_a_folder_defaults_to_the_newest_version_that_changed_it() {
    let d = Deployment::new();
    d.mount_versions(json!({
        "data": [version_body(6, &"6".repeat(40), &[&format!("{FOLDER}/a.md")])],
        "total": 4, "limit": 1, "offset": 0,
    }));
    d.mount(
        Mock::given(method("GET"))
            .and(path_matcher(format!("/api/v1/repos/{REPO}/diff")))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "from": { "version": 5, "sha": "5".repeat(40) },
                "to": { "version": 6, "sha": "6".repeat(40) },
                "files": [],
            }))),
    );

    let out = d.run_in(&d.folder(), b"", &["diff"]);

    assert_eq!(exit_of(&out), 0, "{}", stderr_of(&out));
    let lists = d.requests_to("/versions");
    assert_eq!(lists.len(), 1);
    assert_eq!(query_of(&lists[0], "path").as_deref(), Some(FOLDER));
    assert_eq!(query_of(&lists[0], "limit").as_deref(), Some("1"));
    assert_eq!(query_of(&lists[0], "offset").as_deref(), Some("0"));
    let diffs = d.requests_to("/diff");
    assert_eq!(diffs.len(), 1);
    assert_eq!(query_of(&diffs[0], "from").as_deref(), Some("5"));
    assert_eq!(query_of(&diffs[0], "to").as_deref(), Some("6"));
}

#[test]
#[serial]
fn history_show_inside_a_folder_names_only_its_paths() {
    let d = Deployment::new();
    d.mount(
        Mock::given(method("GET"))
            .and(path_matcher(format!("/api/v1/repos/{REPO}/versions/5")))
            .respond_with(ResponseTemplate::new(200).set_body_json(version_body(
                5,
                &"5".repeat(40),
                &[&format!("{FOLDER}/.page/board.json"), ".page/board.json"],
            ))),
    );

    let out = d.run_in(&d.folder(), b"", &["--json", "history", "show", "5"]);

    assert_eq!(exit_of(&out), 0, "{}", stderr_of(&out));
    assert_eq!(
        one_document(&out)["filesChanged"],
        json!([".page/board.json"])
    );
}

#[test]
#[serial]
fn repo_inside_a_folder_answers_the_holder_and_the_folder() {
    let d = Deployment::new();
    let mut record = repo_body();
    record["commitSha"] = json!("h42");
    record["role"] = json!("write");
    record["visibility"] = json!("private");
    record["fileCount"] = json!(2000);
    d.mount(
        Mock::given(method("GET"))
            .and(path_matcher(format!("/api/v1/repos/{REPO}")))
            .respond_with(ResponseTemplate::new(200).set_body_json(record)),
    );
    d.mount(
        Mock::given(method("GET"))
            .and(path_matcher(format!("/api/v1/repos/{REPO}/versions")))
            .and(query_param("limit", "1"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "data": [version_body(42, "h42", &["a.md"])],
                "total": 42, "limit": 1, "offset": 0,
            }))),
    );

    let out = d.run_in(&d.folder().join("sub"), b"", &["--json", "repo"]);

    assert_eq!(exit_of(&out), 0, "{}", stderr_of(&out));
    let records: Vec<Request> = d
        .requests()
        .into_iter()
        .filter(|r| r.url.path() == format!("/api/v1/repos/{REPO}"))
        .collect();
    assert_eq!(records.len(), 1);
    assert!(
        d.requests()
            .iter()
            .all(|r| !r.url.path().contains("/versions/")),
        "a single-version request was made"
    );
    let document = one_document(&out);
    assert_eq!(document["owner"], json!("alice"));
    assert_eq!(document["name"], json!("work"));
    assert_eq!(document["commitSha"], json!("h42"));
    assert_eq!(document["version"], json!(42));
    assert_eq!(document["role"], json!("write"));
    assert_eq!(document["visibility"], json!("private"));
    assert_eq!(document["holder"], json!("alice/work"));
    assert_eq!(document["path"], json!(FOLDER));
}

#[test]
#[serial]
fn repo_option_and_the_holder_root_read_the_whole_holder() {
    let d = Deployment::new();
    d.mount_reference();
    d.mount(
        Mock::given(method("GET"))
            .and(path_matcher(format!("/api/v1/repos/{REPO}/tree")))
            .and(query_param("recursive", "true"))
            .respond_with(ResponseTemplate::new(200).set_body_json(tree_body(vec![
                tree_entry(".page/board.json", "file"),
                tree_entry(&format!("{FOLDER}/.syns.yaml"), "file"),
            ]))),
    );

    let named = d.run_in(
        &d.folder(),
        b"",
        &["--json", "ls", "--recursive", "--repo", REPO],
    );
    let root = d.run_in(&d.w, b"", &["--json", "ls", "--recursive"]);

    for out in [&named, &root] {
        assert_eq!(exit_of(out), 0, "{}", stderr_of(out));
        let paths: Vec<String> = one_document(out)["entries"]
            .as_array()
            .expect("entries")
            .iter()
            .map(|e| e["path"].as_str().expect("path").to_string())
            .collect();
        assert!(paths.contains(&".page/board.json".to_string()), "{paths:?}");
        assert!(paths.contains(&format!("{FOLDER}/.syns.yaml")), "{paths:?}");
    }
    let trees: Vec<Request> = d
        .requests()
        .into_iter()
        .filter(|r| r.url.path().contains("/tree"))
        .collect();
    assert_eq!(trees.len(), 2);
    for tree in trees {
        assert_eq!(tree.url.path(), format!("/api/v1/repos/{REPO}/tree"));
    }
}

#[test]
#[serial]
fn a_moved_folder_is_refused_naming_both_paths() {
    let d = Deployment::new();
    let moved = d.w.join("clients/vela/archive/q3-board");
    std::fs::create_dir_all(moved.parent().expect("parent")).expect("archive");
    std::fs::rename(d.folder(), &moved).expect("move the folder");
    let line = format!(
        "folder out of place: {} records {FOLDER} in alice/work but stands at clients/vela/archive/q3-board in its checkout at {} \u{2014} move the folder back to {}/{FOLDER}, or correct the path its .syns.yaml records to clients/vela/archive/q3-board",
        moved.display(),
        d.w.display(),
        d.w.display()
    );

    for args in [
        vec!["ls"],
        vec!["ls", "--if-repo"],
        vec!["history"],
        vec!["diff"],
        vec!["delete", "--if-repo"],
    ] {
        let out = d.run_in(&moved, b"", &args);
        assert_eq!(exit_of(&out), 2, "{args:?}: {}", stderr_of(&out));
        assert_eq!(
            stderr_of(&out).trim_end(),
            format!("error: {line}"),
            "{args:?}"
        );
    }
    let json_run = d.run_in(&moved, b"", &["--json", "ls"]);
    assert_eq!(exit_of(&json_run), 2);
    assert_eq!(one_document(&json_run), json!({ "error": line }));
    assert!(d.requests().is_empty());
}

#[test]
#[serial]
fn a_folder_inside_another_checkout_is_refused() {
    let d = Deployment::new();
    let v_dir = tempfile::tempdir().expect("V");
    let v = std::fs::canonicalize(v_dir.path()).expect("canonical V");
    std::fs::write(v.join(".syns.yaml"), "owner: bob\nname: other\n").expect("V identity");
    std::fs::create_dir_all(v.join("x")).expect("V/x");
    std::fs::write(v.join("x/.syns.yaml"), "holder: alice/work\npath: x\n").expect("folder");

    let out = d.run_in(&v.join("x"), b"", &["ls"]);

    assert_eq!(exit_of(&out), 2, "{}", stderr_of(&out));
    assert_eq!(
        stderr_of(&out).trim_end(),
        format!(
            "error: folder out of place: {} is a folder of alice/work but stands inside {}, a checkout of bob/other \u{2014} move it into a checkout of alice/work, or remove {}/.syns.yaml",
            v.join("x").display(),
            v.display(),
            v.join("x").display()
        )
    );
    assert!(d.requests().is_empty());
}

#[test]
#[serial]
fn a_folder_with_no_checkout_above_reads_its_recorded_path() {
    let d = Deployment::new();
    d.mount_reference();
    d.mount(
        Mock::given(method("GET"))
            .and(path_matcher(format!("/api/v1/repos/{REPO}/tree/{FOLDER}")))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(tree_body(vec![tree_entry(
                    &format!("{FOLDER}/a.md"),
                    "file",
                )])),
            ),
    );
    let u_dir = tempfile::tempdir().expect("U");
    let u = std::fs::canonicalize(u_dir.path()).expect("canonical U");
    std::fs::create_dir_all(u.join("q3")).expect("U/q3");
    std::fs::write(u.join("q3/.syns.yaml"), FOLDER_YAML).expect("folder");

    let out = d.run_in(&u.join("q3"), b"", &["--json", "ls"]);

    assert_eq!(exit_of(&out), 0, "{}", stderr_of(&out));
    let trees: Vec<Request> = d
        .requests()
        .into_iter()
        .filter(|r| r.url.path().contains("/tree"))
        .collect();
    assert_eq!(trees.len(), 1);
    assert_eq!(
        trees[0].url.path(),
        format!("/api/v1/repos/{REPO}/tree/{FOLDER}")
    );
}

#[test]
#[serial]
fn commands_outside_this_unit_refuse_inside_a_folder() {
    let d = Deployment::with_credential();
    std::fs::write(d.folder().join("a.md"), "on disk\n").expect("a.md");
    let before = snapshot(&d.w);

    let sync = d.run_in(&d.folder(), b"", &["sync", "--if-repo"]);
    assert_eq!(exit_of(&sync), 5, "{}", stderr_of(&sync));
    assert!(
        stderr_of(&sync)
            .starts_with("error: attention required for this repository: invalid .syns.yaml: "),
        "{}",
        stderr_of(&sync)
    );
    for (args, stdin) in [
        (vec!["push"], &b""[..]),
        (vec!["status"], &b""[..]),
        (vec!["pull", REPO], &b""[..]),
        (vec!["forks"], &b""[..]),
        (vec!["fork", "bob/other"], &b""[..]),
        (
            vec!["write", "a.md", "--repo", REPO, "--parent", "7"],
            &b"x"[..],
        ),
    ] {
        let out = d.run_in(&d.folder(), stdin, &args);
        assert_eq!(exit_of(&out), 1, "{args:?}: {}", stderr_of(&out));
        assert!(
            stderr_of(&out).starts_with("error: invalid .syns.yaml: "),
            "{args:?}: {}",
            stderr_of(&out)
        );
    }

    assert!(d.requests().is_empty());
    assert_eq!(snapshot(&d.w), before);
    assert!(
        std::fs::read_dir(d.cache.path())
            .expect("cache dir")
            .next()
            .is_none(),
        "the cache directory holds an entry"
    );
}

#[test]
#[serial]
fn commands_changing_the_holder_are_refused_inside_a_folder() {
    let d = Deployment::with_credential();
    d.mount(
        Mock::given(method("PATCH"))
            .and(path_matcher(format!("/api/v1/repos/{REPO}")))
            .respond_with(ResponseTemplate::new(200).set_body_json(repo_body())),
    );
    let sub = d.folder().join("sub");

    let first = d.run_in(&sub, b"", &["repo", "--visibility", "public"]);
    assert_eq!(exit_of(&first), 2, "{}", stderr_of(&first));
    assert_eq!(
        stderr_of(&first).trim_end(),
        format!(
            "error: holder root required: syns repo --visibility acts on the holding repository alice/work, not on the folder {} \u{2014} run it from the root of a checkout of alice/work",
            d.folder().display()
        )
    );
    for (args, command) in [
        (vec!["repo", "--tag", "x"], "syns repo --tag"),
        (vec!["delete", "--if-repo"], "syns delete"),
        (vec!["collaborators"], "syns collaborators"),
        (
            vec!["collaborators", "add", "bob", "--role", "read"],
            "syns collaborators add",
        ),
        (
            vec!["collaborators", "role", "u1", "--role", "write"],
            "syns collaborators role",
        ),
        (
            vec!["collaborators", "remove", "u1"],
            "syns collaborators remove",
        ),
    ] {
        let out = d.run_in(&sub, b"", &args);
        assert_eq!(exit_of(&out), 2, "{args:?}: {}", stderr_of(&out));
        let err = stderr_of(&out);
        assert!(
            err.starts_with(&format!("error: holder root required: {command} acts on")),
            "{args:?}: {err}"
        );
        assert!(err.contains("alice/work"), "{err}");
    }
    assert!(d.requests().is_empty());

    let root = d.run_in(&d.w, b"", &["repo", "--visibility", "public"]);
    assert_eq!(exit_of(&root), 0, "{}", stderr_of(&root));
    let updates: Vec<Request> = d
        .requests()
        .into_iter()
        .filter(|r| r.method.as_str() == "PATCH")
        .collect();
    assert_eq!(updates.len(), 1);
    assert_eq!(updates[0].url.path(), format!("/api/v1/repos/{REPO}"));
}

#[test]
#[serial]
fn a_file_mixing_both_forms_is_refused_by_every_command() {
    let d = Deployment::with_credential();
    std::fs::write(
        d.folder().join(".syns.yaml"),
        "owner: alice\nname: work\nholder: alice/work\npath: clients/vela/q3-board\n",
    )
    .expect("mixed identity");
    let before = snapshot(&d.w);

    let sync = d.run_in(&d.folder(), b"", &["sync", "--if-repo"]);
    assert_eq!(exit_of(&sync), 5, "{}", stderr_of(&sync));
    assert!(
        stderr_of(&sync).contains("invalid .syns.yaml: "),
        "{}",
        stderr_of(&sync)
    );
    for (args, stdin) in [
        (vec!["push"], &b""[..]),
        (vec!["pull", REPO], &b""[..]),
        (
            vec!["write", "a.md", "--repo", REPO, "--parent", "7"],
            &b"x"[..],
        ),
        (vec!["ls"], &b""[..]),
    ] {
        let out = d.run_in(&d.folder(), stdin, &args);
        assert_eq!(exit_of(&out), 1, "{args:?}: {}", stderr_of(&out));
        assert!(
            stderr_of(&out).contains("invalid .syns.yaml: "),
            "{args:?}: {}",
            stderr_of(&out)
        );
    }
    assert!(d.requests().is_empty());
    assert_eq!(snapshot(&d.w), before);
}

#[test]
#[serial]
fn grep_anchors_a_separator_glob_at_the_folder() {
    let d = Deployment::new();
    d.mount_reference();
    d.mount(
        Mock::given(method("GET"))
            .and(path_matcher(format!("/api/v1/repos/{REPO}/tree/{FOLDER}")))
            .respond_with(ResponseTemplate::new(200).set_body_json(tree_body(vec![
                tree_entry(&format!("{FOLDER}/a.md"), "file"),
                tree_entry(&format!("{FOLDER}/notes/b.md"), "file"),
            ]))),
    );
    d.mount(
        Mock::given(method("GET"))
            .and(path_matcher(format!(
                "/api/v1/repos/{REPO}/files/{FOLDER}/notes/b.md"
            )))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "path": format!("{FOLDER}/notes/b.md"), "content": "fn two",
                "sha": "b".repeat(40), "size": 6,
            }))),
    );

    let out = d.run_in(
        &d.folder(),
        b"",
        &["--json", "grep", "fn ", "--glob", "notes/*.md"],
    );

    assert_eq!(exit_of(&out), 0, "{}", stderr_of(&out));
    let document = one_document(&out);
    let matches = document["matches"].as_array().expect("matches");
    assert_eq!(matches.len(), 1, "{document}");
    assert_eq!(matches[0]["path"], json!("notes/b.md"));
    assert!(d.requests_to(&format!("{FOLDER}/a.md")).is_empty());
}

#[test]
#[serial]
fn diff_inside_a_folder_no_version_changed_is_refused() {
    let d = Deployment::new();
    d.mount_versions(json!({ "data": [], "total": 0, "limit": 1, "offset": 0 }));

    let out = d.run_in(&d.folder(), b"", &["diff"]);

    assert_eq!(exit_of(&out), 1, "{}", stderr_of(&out));
    assert_eq!(
        stderr_of(&out).trim_end(),
        format!(
            "error: configuration error: no version of alice/work changed {FOLDER} beyond its first \u{2014} name --from and --to"
        )
    );
    assert!(d.requests_to("/diff").is_empty());
}

#[test]
#[serial]
fn repo_inside_a_folder_renders_the_path_row_after_the_identity_row() {
    let d = Deployment::new();
    d.mount(
        Mock::given(method("GET"))
            .and(path_matcher(format!("/api/v1/repos/{REPO}")))
            .respond_with(ResponseTemplate::new(200).set_body_json(repo_body())),
    );
    d.mount_versions(json!({
        "data": [version_body(7, &head_sha(), &["a.md"])],
        "total": 7, "limit": 1, "offset": 0,
    }));

    let out = d.run_in(&d.folder(), b"", &["repo"]);

    assert_eq!(exit_of(&out), 0, "{}", stderr_of(&out));
    let text = stdout_of(&out);
    let row = |label: &str| {
        text.lines()
            .position(|line| line.contains(label))
            .unwrap_or_else(|| panic!("no {label} row: {text}"))
    };
    let identity = row("alice/work");
    let path = row(FOLDER);
    assert_eq!(path, identity + 1, "{text}");
    assert!(
        text.lines().nth(path).expect("row").contains("Path"),
        "{text}"
    );
}

#[test]
#[serial]
fn the_file_and_folder_blocks_write_their_count_and_the_record_its_folder_paths() {
    let d = Deployment::new();
    d.mount_file_history(
        "a.md",
        json!({ "data": [file_history_entry(3, "d")], "total": 3, "limit": 1, "offset": 0 }),
    );
    d.mount(
        Mock::given(method("GET"))
            .and(path_matcher(format!("/api/v1/repos/{REPO}/versions")))
            .and(query_param("path", FOLDER))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "data": [version_body(5, &"5".repeat(40), &[&format!("{FOLDER}/a.md")])],
                "total": 2, "limit": 1, "offset": 0,
            }))),
    );
    d.mount(
        Mock::given(method("GET"))
            .and(path_matcher(format!("/api/v1/repos/{REPO}/versions/5")))
            .respond_with(ResponseTemplate::new(200).set_body_json(version_body(
                5,
                &"5".repeat(40),
                &[&format!("{FOLDER}/notes/n.md"), "outside.md"],
            ))),
    );

    let file = d.run_in(&d.w, b"", &["history", "--file", "a.md", "--limit", "1"]);
    assert_eq!(exit_of(&file), 0, "{}", stderr_of(&file));
    assert_eq!(stderr_of(&file).trim_end(), "Showing 1 of 3 versions.");

    let folder = d.run_in(&d.folder(), b"", &["history", "--limit", "1"]);
    assert_eq!(exit_of(&folder), 0, "{}", stderr_of(&folder));
    assert_eq!(stderr_of(&folder).trim_end(), "Showing 1 of 2 versions.");

    let record = d.run_in(&d.folder(), b"", &["history", "show", "5"]);
    assert_eq!(exit_of(&record), 0, "{}", stderr_of(&record));
    let text = stdout_of(&record);
    assert!(text.contains("notes/n.md"), "{text}");
    assert!(!text.contains(FOLDER), "{text}");
    assert!(!text.contains("outside.md"), "{text}");
}
