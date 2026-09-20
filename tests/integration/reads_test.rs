//! Binary-level behaviour of the five read verbs, the content cache and
//! the four short aliases (SPEC u270 Tests).
//!
//! Every row of that table stands here under the name the table gives
//! it, but the two the cache's own module cases carry
//! (`a_blob_whose_served_hash_does_not_match_its_bytes_is_not_stored`
//! and `cache_evicts_least_recently_touched_past_the_cap`), which drive
//! `BlobCache` directly rather than through the binary.

use assert_cmd::Command as AssertCommand;
use serde_json::{Value, json};
use serial_test::serial;
use syns_cli::push::hash::blob_sha1;
use tempfile::TempDir;
use wiremock::matchers::{method, path as path_matcher, path_regex};
use wiremock::{Mock, MockServer, Request, ResponseTemplate};

const REPO: &str = "alice/notes";
const HEAD_SHA: &str = "aa11bb22cc33dd44ee55ff6600778899001122bb";
const OLD_SHA: &str = "bb00bb00bb00bb00bb00bb00bb00bb00bb00bb00";
const HEAD_VERSION: u32 = 7;

/// One mock deployment, one config directory, one cache directory and
/// one working directory — all derived from the test that made them, so
/// two tests never share a store.
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
        Deployment {
            rt,
            server,
            home: tempfile::tempdir().expect("config dir"),
            cache: tempfile::tempdir().expect("cache dir"),
            work: tempfile::tempdir().expect("working dir"),
        }
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

    fn file_calls(&self) -> usize {
        self.requests()
            .iter()
            .filter(|r| r.url.path().contains("/files/"))
            .count()
    }

    fn run(&self, args: &[&str]) -> std::process::Output {
        self.run_capped(None, args)
    }

    /// The same run with the cache cap's environment name bound, so a
    /// test can watch the eviction pass over a store its own run
    /// oversized (u270 CR1-5).
    fn run_capped(&self, cap: Option<&str>, args: &[&str]) -> std::process::Output {
        let mut command = AssertCommand::cargo_bin("syns").expect("syns binary");
        command
            .current_dir(self.work.path())
            .env("SYNS_CONFIG_DIR", self.home.path())
            .env("SYNS_CACHE_DIR", self.cache.path())
            .env_remove("SYNS_URL");
        match cap {
            Some(value) => command.env("SYNS_CACHE_MAX_BYTES", value),
            None => command.env_remove("SYNS_CACHE_MAX_BYTES"),
        };
        command
            .arg("--server")
            .arg(self.server.uri())
            .args(args)
            .output()
            .expect("run syns")
    }

    /// The bytes the `blobs` directory holds, which the cap bounds.
    fn blob_bytes(&self) -> u64 {
        let blobs = self.cache.path().join("blobs");
        let Ok(dir) = std::fs::read_dir(&blobs) else {
            return 0;
        };
        dir.flatten()
            .filter_map(|e| e.metadata().ok())
            .filter(|m| m.is_file())
            .map(|m| m.len())
            .sum()
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

/// Mounts the repository read and the version read a run with no
/// `--version` resolves through.
fn mount_head(d: &Deployment) {
    d.mount(
        Mock::given(method("GET"))
            .and(path_matcher(format!("/api/v1/repos/{REPO}")))
            .respond_with(ResponseTemplate::new(200).set_body_json(repo_body(HEAD_SHA))),
    );
    d.mount(
        Mock::given(method("GET"))
            .and(path_matcher(format!(
                "/api/v1/repos/{REPO}/versions/{HEAD_SHA}"
            )))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(version_body(HEAD_VERSION, HEAD_SHA)),
            ),
    );
}

fn mount_version(d: &Deployment, reference: &str, version: u32, sha: &str) {
    d.mount(
        Mock::given(method("GET"))
            .and(path_matcher(format!(
                "/api/v1/repos/{REPO}/versions/{reference}"
            )))
            .respond_with(ResponseTemplate::new(200).set_body_json(version_body(version, sha))),
    );
}

fn mount_file(d: &Deployment, path: &str, content: &str) {
    let body = json!({
        "path": path,
        "sha": blob_sha1(content.as_bytes()),
        "content": content,
        "size": content.len(),
    });
    d.mount(
        Mock::given(method("GET"))
            .and(path_matcher(format!("/api/v1/repos/{REPO}/files/{path}")))
            .respond_with(ResponseTemplate::new(200).set_body_json(body)),
    );
}

fn mount_file_status(d: &Deployment, path: &str, status: u16, body: Value) {
    d.mount(
        Mock::given(method("GET"))
            .and(path_matcher(format!("/api/v1/repos/{REPO}/files/{path}")))
            .respond_with(ResponseTemplate::new(status).set_body_json(body)),
    );
}

fn file_entry(path: &str, content: &str, with_sha: bool) -> Value {
    json!({
        "name": path.rsplit('/').next().unwrap_or(path),
        "path": path,
        "type": "file",
        "size": content.len(),
        "sha": if with_sha { Value::from(blob_sha1(content.as_bytes())) } else { Value::Null },
    })
}

fn mount_tree(d: &Deployment, subpath: Option<&str>, entries: Vec<Value>, truncated: bool) {
    let address = match subpath {
        Some(p) => format!("/api/v1/repos/{REPO}/tree/{p}"),
        None => format!("/api/v1/repos/{REPO}/tree"),
    };
    d.mount(
        Mock::given(method("GET"))
            .and(path_matcher(address))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "entries": entries,
                "commitSha": HEAD_SHA,
                "truncated": truncated,
            }))),
    );
}

/// Answers the recursive tree address with exactly these bytes, so a
/// case can serve a body the deployment itself wrote rather than one
/// this file composed (u270 V2-16).
fn mount_tree_verbatim(d: &Deployment, body: &str) {
    d.mount(
        Mock::given(method("GET"))
            .and(path_matcher(format!("/api/v1/repos/{REPO}/tree")))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_raw(body.as_bytes().to_vec(), "application/json"),
            ),
    );
}

/// The tree two of the search rows share: `src/a.ts` holding `fn one` on
/// line 2 and `src/b.ts` holding `fn two` on lines 1 and 3.
const A_TS: &str = "plain\nfn one\ntail\n";
const B_TS: &str = "fn two\nplain\nfn two\n";

fn mount_search_tree(d: &Deployment, with_sha: bool) {
    mount_head(d);
    mount_tree(
        d,
        None,
        vec![
            file_entry("src/a.ts", A_TS, with_sha),
            file_entry("src/b.ts", B_TS, with_sha),
        ],
        false,
    );
    mount_file(d, "src/a.ts", A_TS);
    mount_file(d, "src/b.ts", B_TS);
}

fn stdout_of(output: &std::process::Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn stderr_of(output: &std::process::Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

fn document(output: &std::process::Output) -> Value {
    serde_json::from_str(&stdout_of(output)).unwrap_or_else(|e| {
        panic!("stdout is no one document ({e}): {}", stdout_of(output));
    })
}

fn query_of(request: &Request, key: &str) -> Option<String> {
    request
        .url
        .query_pairs()
        .find(|(k, _)| k == key)
        .map(|(_, v)| v.into_owned())
}

// ---------------------------------------------------------------- cat

#[test]
#[serial]
fn cat_at_a_version_reads_that_version_and_reports_it() {
    let d = Deployment::new();
    mount_version(&d, "2", 2, OLD_SHA);
    mount_file(&d, "a.md", "old");

    let output = d.run(&["cat", "a.md", "--repo", REPO, "--version", "2"]);
    assert_eq!(output.status.code(), Some(0), "{}", stderr_of(&output));
    assert_eq!(stdout_of(&output), "old");
    assert!(
        stderr_of(&output).contains(&format!("read at version 2, commit {OLD_SHA}")),
        "stderr: {}",
        stderr_of(&output)
    );

    let requests = d.requests();
    let file = requests
        .iter()
        .find(|r| r.url.path().contains("/files/"))
        .expect("a file request");
    assert_eq!(query_of(file, "ref").as_deref(), Some("2"));
}

#[test]
#[serial]
fn cat_json_carries_the_resolved_reference_beside_the_served_body() {
    let d = Deployment::new();
    mount_version(&d, "2", 2, OLD_SHA);
    mount_file(&d, "a.md", "old");

    let output = d.run(&["--json", "cat", "a.md", "--repo", REPO, "--version", "2"]);
    assert_eq!(output.status.code(), Some(0), "{}", stderr_of(&output));
    let body = document(&output);
    assert_eq!(body["path"], json!("a.md"));
    assert_eq!(body["sha"], json!(blob_sha1(b"old")));
    assert_eq!(body["content"], json!("old"));
    assert_eq!(body["size"], json!(3));
    assert_eq!(body["version"], json!(2));
    assert_eq!(body["commitSha"], json!(OLD_SHA));
    assert_eq!(
        stderr_of(&output),
        "",
        "nothing stands on the diagnostic stream"
    );
}

#[test]
#[serial]
fn a_hash_version_is_resolved_once_and_sent_as_an_ordinal() {
    let d = Deployment::new();
    mount_version(&d, OLD_SHA, 2, OLD_SHA);
    mount_file(&d, "a.md", "old");

    let output = d.run(&["cat", "a.md", "--repo", REPO, "--version", OLD_SHA]);
    assert_eq!(output.status.code(), Some(0), "{}", stderr_of(&output));

    let requests = d.requests();
    let version_requests: Vec<_> = requests
        .iter()
        .filter(|r| r.url.path().contains("/versions/"))
        .collect();
    assert_eq!(version_requests.len(), 1, "one version request");
    assert!(version_requests[0].url.path().ends_with(OLD_SHA));

    let file = requests
        .iter()
        .find(|r| r.url.path().contains("/files/"))
        .expect("a file request");
    assert_eq!(query_of(file, "ref").as_deref(), Some("2"));
}

#[test]
#[serial]
fn version_below_one_is_refused_before_any_request() {
    let d = Deployment::new();
    let output = d.run(&["cat", "a.md", "--repo", REPO, "--version", "0"]);
    assert_eq!(output.status.code(), Some(1));
    assert!(
        stderr_of(&output).contains("error: configuration error: version must be \u{2265} 1"),
        "stderr: {}",
        stderr_of(&output)
    );
    assert!(d.requests().is_empty(), "nothing was sent");
}

#[test]
#[serial]
fn an_out_of_range_version_names_the_version_rather_than_the_path() {
    let d = Deployment::new();
    d.mount(
        Mock::given(method("GET"))
            .and(path_matcher(format!("/api/v1/repos/{REPO}/versions/999")))
            .respond_with(ResponseTemplate::new(404).set_body_json(json!({
                "error": "not_found", "message": "Version not found",
            }))),
    );

    let output = d.run(&["cat", "a.md", "--repo", REPO, "--version", "999"]);
    assert_eq!(output.status.code(), Some(1));
    assert!(
        stderr_of(&output).contains("error: version not found: 999"),
        "stderr: {}",
        stderr_of(&output)
    );
    assert_eq!(d.file_calls(), 0, "no file request was made");
}

#[test]
#[serial]
fn a_path_absent_at_the_pinned_version_names_that_version() {
    let d = Deployment::new();
    mount_version(&d, "58", 58, OLD_SHA);
    mount_file_status(
        &d,
        "a.md",
        404,
        json!({ "error": "not_found", "message": "File not found" }),
    );

    let output = d.run(&["cat", "a.md", "--repo", REPO, "--version", "58"]);
    assert_eq!(output.status.code(), Some(1));
    assert!(
        stderr_of(&output).contains("error: path not found at version 58: a.md"),
        "stderr: {}",
        stderr_of(&output)
    );
}

// ----------------------------------------------------------------- ls

#[test]
#[serial]
fn if_repo_beside_repo_reads_rather_than_skipping() {
    let d = Deployment::new();
    mount_head(&d);
    mount_tree(&d, None, vec![file_entry("README.md", "x", true)], false);

    let output = d.run(&["--json", "ls", "--repo", REPO, "--if-repo"]);
    assert_eq!(output.status.code(), Some(0), "{}", stderr_of(&output));
    let body = document(&output);
    assert_eq!(body["entries"].as_array().map(Vec::len), Some(1));
    assert!(
        body.get("skipped").is_none(),
        "no skip envelope: {}",
        stdout_of(&output)
    );
}

#[test]
#[serial]
fn repo_option_outranks_the_identity_file_it_stands_in() {
    let d = Deployment::new();
    std::fs::write(
        d.work.path().join(".syns.yaml"),
        "owner: bob\nname: other\n",
    )
    .expect("write identity file");
    mount_head(&d);
    mount_tree(&d, None, vec![file_entry("README.md", "x", true)], false);

    let output = d.run(&["ls", "--repo", REPO]);
    assert_eq!(output.status.code(), Some(0), "{}", stderr_of(&output));
    for path in d.paths() {
        assert!(
            path.starts_with(&format!("/api/v1/repos/{REPO}")),
            "a request addressed {path}"
        );
        assert!(!path.contains("bob/other"), "a request named bob/other");
    }
}

#[test]
#[serial]
fn branch_name_version_reaches_the_server_unchecked() {
    let d = Deployment::new();
    d.mount(
        Mock::given(method("GET"))
            .and(path_matcher(format!("/api/v1/repos/{REPO}/versions/main")))
            .respond_with(ResponseTemplate::new(422).set_body_json(json!({
                "error": "validation_error",
                "message": "Must be a positive integer or a hex string",
            }))),
    );

    let output = d.run(&["ls", "--repo", REPO, "--version", "main"]);
    assert_eq!(output.status.code(), Some(1));
    assert!(
        d.paths().iter().any(|p| p.ends_with("/versions/main")),
        "paths: {:?}",
        d.paths()
    );
}

#[test]
#[serial]
fn ls_recursive_lists_the_whole_subtree_at_a_version() {
    let d = Deployment::new();
    mount_version(&d, "2", 2, OLD_SHA);
    mount_tree(
        &d,
        Some("src"),
        vec![
            file_entry("src/a.ts", "a", true),
            file_entry("src/deep/b.ts", "b", true),
        ],
        false,
    );

    let output = d.run(&[
        "--json",
        "ls",
        "src",
        "--repo",
        REPO,
        "--version",
        "2",
        "--recursive",
    ]);
    assert_eq!(output.status.code(), Some(0), "{}", stderr_of(&output));

    let requests = d.requests();
    let tree = requests
        .iter()
        .find(|r| r.url.path().contains("/tree"))
        .expect("a tree request");
    assert_eq!(query_of(tree, "recursive").as_deref(), Some("true"));
    assert_eq!(query_of(tree, "ref").as_deref(), Some("2"));

    let body = document(&output);
    assert_eq!(body["version"], json!(2));
    let paths: Vec<String> = body["entries"]
        .as_array()
        .expect("entries")
        .iter()
        .map(|e| e["path"].as_str().unwrap_or_default().to_string())
        .collect();
    assert_eq!(paths, vec!["src/a.ts", "src/deep/b.ts"]);
}

#[test]
#[serial]
fn recursive_listing_of_a_truncated_tree_refuses() {
    let d = Deployment::new();
    mount_head(&d);
    mount_tree(&d, None, vec![file_entry("README.md", "x", true)], true);

    let output = d.run(&["--json", "ls", "--repo", REPO, "--recursive"]);
    assert_eq!(output.status.code(), Some(1));
    let body = document(&output);
    assert_eq!(body["entries"].as_array().map(Vec::len), Some(1));
    assert_eq!(body["truncated"], json!(true));
    assert!(
        body["error"].as_str().is_some_and(|e| !e.is_empty()),
        "a populated error: {body}"
    );
}

// --------------------------------------------------------------- read

#[test]
#[serial]
fn read_prints_the_numbered_window() {
    let d = Deployment::new();
    mount_head(&d);
    let third = "c".repeat(2100);
    mount_file(&d, "a.md", &format!("one\ntwo\n{third}\nfour\nfive\n"));

    let output = d.run(&[
        "read", "a.md", "--repo", REPO, "--offset", "2", "--limit", "2",
    ]);
    assert_eq!(output.status.code(), Some(0), "{}", stderr_of(&output));
    let stdout = stdout_of(&output);
    let lines: Vec<&str> = stdout.lines().collect();
    assert_eq!(lines.len(), 2, "stdout: {stdout}");
    assert_eq!(lines[0], "     2\ttwo");
    let (number, text) = lines[1].split_once('\t').expect("a tab");
    assert_eq!(number, "     3");
    assert_eq!(text.chars().count(), 2000);
}

#[test]
#[serial]
fn read_window_past_the_last_line_prints_nothing() {
    let d = Deployment::new();
    mount_head(&d);
    mount_file(&d, "a.md", "one\ntwo\nthree\n");

    let output = d.run(&["read", "a.md", "--repo", REPO, "--offset", "99"]);
    assert_eq!(output.status.code(), Some(0), "{}", stderr_of(&output));
    assert_eq!(stdout_of(&output), "");
}

#[test]
#[serial]
fn read_refuses_a_content_that_is_not_text_where_cat_passes_it_through() {
    let d = Deployment::new();
    mount_head(&d);
    mount_file(&d, "a.md", "one\u{0}two");

    let numbered = d.run(&["read", "a.md", "--repo", REPO]);
    assert_eq!(numbered.status.code(), Some(1));
    assert_eq!(
        stdout_of(&numbered),
        "",
        "nothing crosses the primary stream"
    );
    let diagnostic = stderr_of(&numbered);
    assert!(diagnostic.contains("a.md"), "stderr: {diagnostic}");
    assert!(diagnostic.contains("syns cat"), "stderr: {diagnostic}");

    let passed = d.run(&["cat", "a.md", "--repo", REPO]);
    assert_eq!(passed.status.code(), Some(0), "{}", stderr_of(&passed));
    assert_eq!(passed.stdout, b"one\0two");
}

// --------------------------------------------------------------- glob

#[test]
#[serial]
fn glob_answers_matching_paths_in_ascending_path_order() {
    let d = Deployment::new();
    mount_head(&d);
    mount_tree(
        &d,
        None,
        vec![
            file_entry("src/deep/b.ts", "b", true),
            file_entry("a.ts", "a", true),
            file_entry("README.md", "r", true),
            file_entry("src/a.ts", "s", true),
        ],
        false,
    );
    d.mount(
        Mock::given(method("GET"))
            .and(path_regex(r"^/api/v1/repos/alice/notes/files/.*$"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "path": "x", "sha": blob_sha1(b"x"), "content": "x", "size": 1,
            }))),
    );

    let output = d.run(&["--json", "glob", "**/*.ts", "--repo", REPO]);
    assert_eq!(output.status.code(), Some(0), "{}", stderr_of(&output));

    let body = document(&output);
    let paths: Vec<String> = body["matches"]
        .as_array()
        .expect("matches")
        .iter()
        .map(|m| m["path"].as_str().unwrap_or_default().to_string())
        .collect();
    assert_eq!(paths, vec!["a.ts", "src/a.ts", "src/deep/b.ts"]);

    let requests = d.requests();
    let tree = requests
        .iter()
        .find(|r| r.url.path().ends_with("/tree"))
        .expect("a tree request");
    assert_eq!(
        query_of(tree, "ref").as_deref(),
        Some(HEAD_VERSION.to_string().as_str())
    );
    assert_eq!(d.file_calls(), 0, "the file endpoint received nothing");
    assert!(
        !d.paths().iter().any(|p| p.ends_with("/versions")),
        "no version-listing request was made: {:?}",
        d.paths()
    );
}

#[test]
#[serial]
fn a_single_star_crosses_no_separator() {
    let d = Deployment::new();
    mount_head(&d);
    mount_tree(
        &d,
        None,
        vec![
            file_entry("a.ts", "a", true),
            file_entry("src/a.ts", "s", true),
            file_entry("src/deep/b.ts", "b", true),
            file_entry("README.md", "r", true),
        ],
        false,
    );

    let output = d.run(&["glob", "src/*.ts", "--repo", REPO]);
    assert_eq!(output.status.code(), Some(0), "{}", stderr_of(&output));
    assert_eq!(stdout_of(&output), "src/a.ts\n");
}

// --------------------------------------------------------------- grep

#[test]
#[serial]
fn grep_returns_the_matches_in_each_output_mode() {
    let d = Deployment::new();
    mount_search_tree(&d, true);

    let content = d.run(&[
        "--json", "grep", "fn ", "--repo", REPO, "--output", "content",
    ]);
    assert_eq!(content.status.code(), Some(0), "{}", stderr_of(&content));
    let body = document(&content);
    let rows: Vec<(String, u64)> = body["matches"]
        .as_array()
        .expect("matches")
        .iter()
        .map(|m| {
            (
                m["path"].as_str().unwrap_or_default().to_string(),
                m["line"].as_u64().unwrap_or_default(),
            )
        })
        .collect();
    assert_eq!(
        rows,
        vec![
            ("src/a.ts".to_string(), 2),
            ("src/b.ts".to_string(), 1),
            ("src/b.ts".to_string(), 3),
        ]
    );

    let files = d.run(&["--json", "grep", "fn ", "--repo", REPO, "--output", "files"]);
    assert_eq!(files.status.code(), Some(0), "{}", stderr_of(&files));
    assert_eq!(document(&files)["files"], json!(["src/a.ts", "src/b.ts"]));

    let count = d.run(&["--json", "grep", "fn ", "--repo", REPO, "--output", "count"]);
    assert_eq!(count.status.code(), Some(0), "{}", stderr_of(&count));
    assert_eq!(
        document(&count)["counts"],
        json!([
            { "path": "src/a.ts", "count": 1 },
            { "path": "src/b.ts", "count": 2 },
        ])
    );
}

#[test]
#[serial]
fn grep_answers_a_crlf_file_the_same_lines_as_a_newline_one() {
    let d = Deployment::new();
    mount_head(&d);
    let crlf = "fn one\r\nplain\r\nfn two\r\n";
    let newline = "fn one\nplain\nfn two";
    mount_tree(
        &d,
        None,
        vec![
            file_entry("src/crlf.ts", crlf, true),
            file_entry("src/lf.ts", newline, true),
        ],
        false,
    );
    mount_file(&d, "src/crlf.ts", crlf);
    mount_file(&d, "src/lf.ts", newline);

    let output = d.run(&["--json", "grep", "fn ", "--repo", REPO, "-n"]);
    assert_eq!(output.status.code(), Some(0), "{}", stderr_of(&output));
    let body = document(&output);
    let matches = body["matches"].as_array().expect("matches");
    let shape = |path: &str| -> Vec<(u64, String)> {
        matches
            .iter()
            .filter(|m| m["path"] == json!(path))
            .map(|m| {
                (
                    m["line"].as_u64().unwrap_or_default(),
                    m["text"].as_str().unwrap_or_default().to_string(),
                )
            })
            .collect()
    };
    assert_eq!(shape("src/crlf.ts"), shape("src/lf.ts"));
    assert_eq!(
        shape("src/crlf.ts"),
        vec![(1, "fn one".to_string()), (3, "fn two".to_string())]
    );
    assert!(
        matches
            .iter()
            .all(|m| !m["text"].as_str().unwrap_or_default().contains('\r')),
        "a text carries a carriage return: {body}"
    );
}

#[test]
#[serial]
fn a_newline_bearing_pattern_is_refused_before_any_request() {
    let d = Deployment::new();
    mount_search_tree(&d, true);

    let output = d.run(&["grep", "one\\nfn", "--repo", REPO]);
    assert_eq!(output.status.code(), Some(1));
    let diagnostic = stderr_of(&output);
    assert!(
        diagnostic.contains("error: configuration error:"),
        "stderr: {diagnostic}"
    );
    assert!(diagnostic.contains("one\\nfn"), "stderr: {diagnostic}");
    assert!(d.requests().is_empty(), "nothing was sent");
}

#[test]
#[serial]
fn grep_reads_no_content_twice_across_runs() {
    let d = Deployment::new();
    mount_search_tree(&d, true);

    let first = d.run(&["--json", "grep", "fn ", "--repo", REPO]);
    let second = d.run(&["--json", "grep", "fn ", "--repo", REPO]);
    assert_eq!(first.status.code(), Some(0), "{}", stderr_of(&first));
    assert_eq!(second.status.code(), Some(0), "{}", stderr_of(&second));
    assert_eq!(document(&first), document(&second));
    assert_eq!(
        d.file_calls(),
        2,
        "one call per path across the two runs, not {}",
        d.file_calls()
    );
}

#[test]
#[serial]
fn grep_marks_a_refused_path_in_the_one_document_it_writes() {
    let d = Deployment::new();
    mount_head(&d);
    mount_tree(
        &d,
        None,
        vec![
            file_entry("src/a.ts", A_TS, true),
            file_entry("src/b.ts", B_TS, true),
            file_entry("src/boom.ts", "x", false),
            file_entry("src/gone.ts", "y", false),
        ],
        false,
    );
    mount_file(&d, "src/a.ts", A_TS);
    mount_file(&d, "src/b.ts", B_TS);
    mount_file_status(&d, "src/boom.ts", 500, json!({ "error": "internal_error" }));
    // An answer whose `content` is no decodable string — the arm
    // `SPEC_REVIEW_R3.md` QF-03 widened `Refused` to carry beside the
    // not-found and internal classes.
    mount_file_status(
        &d,
        "src/gone.ts",
        200,
        json!({ "path": "src/gone.ts", "sha": "x", "content": 7 }),
    );

    let output = d.run(&["--json", "grep", "fn ", "--repo", REPO]);
    assert_eq!(output.status.code(), Some(1));
    let stdout = stdout_of(&output);
    let documents: Vec<Value> = serde_json::Deserializer::from_str(&stdout)
        .into_iter::<Value>()
        .collect::<Result<_, _>>()
        .unwrap_or_else(|e| panic!("stdout is no run of documents ({e}): {stdout}"));
    assert_eq!(documents.len(), 1, "exactly one document: {stdout}");
    let body = document(&output);
    let paths: Vec<String> = body["matches"]
        .as_array()
        .expect("matches")
        .iter()
        .map(|m| m["path"].as_str().unwrap_or_default().to_string())
        .collect();
    assert!(paths.contains(&"src/a.ts".to_string()));
    assert!(paths.contains(&"src/b.ts".to_string()));
    assert_eq!(
        body["skipped"],
        json!([
            { "path": "src/boom.ts", "reason": "refused" },
            { "path": "src/gone.ts", "reason": "refused" },
        ])
    );
    assert!(
        body["error"].as_str().is_some_and(|e| !e.is_empty()),
        "a populated error: {body}"
    );
}

#[test]
#[serial]
fn grep_skips_a_binary_path_and_still_exits_zero() {
    let d = Deployment::new();
    mount_head(&d);
    let binary = "fn\u{0}one";
    mount_tree(
        &d,
        None,
        vec![
            file_entry("src/a.ts", A_TS, true),
            file_entry("src/b.ts", B_TS, true),
            file_entry("src/logo.bin", binary, true),
        ],
        false,
    );
    mount_file(&d, "src/a.ts", A_TS);
    mount_file(&d, "src/b.ts", B_TS);
    mount_file(&d, "src/logo.bin", binary);

    let output = d.run(&["--json", "grep", "fn ", "--repo", REPO]);
    assert_eq!(output.status.code(), Some(0), "{}", stderr_of(&output));
    let body = document(&output);
    assert_eq!(
        body["skipped"],
        json!([{ "path": "src/logo.bin", "reason": "binary" }])
    );
    assert!(body.get("error").is_none(), "no error key: {body}");
}

/// A tree of `paths` text files, each holding `fn one`, whose entries
/// name no hash — so no path is answered from the store and the fan-out
/// asks for every one it admits.
fn mount_wide_tree(d: &Deployment, paths: usize) {
    mount_head(d);
    let entries: Vec<Value> = (0..paths)
        .map(|index| {
            json!({
                "name": format!("f{index:04}.ts"),
                "path": format!("src/f{index:04}.ts"),
                "type": "file",
                "size": 7,
                "sha": Value::Null,
            })
        })
        .collect();
    mount_tree(d, None, entries, false);
    d.mount(
        Mock::given(method("GET"))
            .and(path_regex(
                r"^/api/v1/repos/alice/notes/files/src/f\d+\.ts$",
            ))
            .respond_with(|request: &Request| {
                let stem = request
                    .url
                    .path()
                    .rsplit('/')
                    .next()
                    .unwrap_or_default()
                    .to_string();
                ResponseTemplate::new(200).set_body_json(json!({
                    "path": format!("src/{stem}"),
                    "sha": blob_sha1(b"fn one\n"),
                    "content": "fn one\n",
                    "size": 7,
                }))
            }),
    );
}

#[test]
#[serial]
fn grep_stops_at_the_fan_out_cap_and_still_exits_zero() {
    let d = Deployment::new();
    mount_wide_tree(&d, 500);

    let output = d.run(&["--json", "grep", "fn ", "--repo", REPO]);
    assert_eq!(output.status.code(), Some(0), "{}", stderr_of(&output));
    let body = document(&output);
    assert_eq!(body["truncated"], json!(true));
    assert!(body.get("error").is_none(), "no error key");
    assert_eq!(d.file_calls(), 400);

    let admitted: Vec<String> = (0..400).map(|i| format!("src/f{i:04}.ts")).collect();
    for row in body["matches"].as_array().expect("matches") {
        let path = row["path"].as_str().unwrap_or_default().to_string();
        assert!(
            admitted.contains(&path),
            "{path} stands past the first 400 in ascending path order"
        );
    }
}

#[test]
#[serial]
fn head_limit_closes_the_fan_out_short_of_the_cap() {
    let d = Deployment::new();
    mount_wide_tree(&d, 500);

    let output = d.run(&["--json", "grep", "fn ", "--repo", REPO, "--head-limit", "3"]);
    assert_eq!(output.status.code(), Some(0), "{}", stderr_of(&output));
    let body = document(&output);
    assert_eq!(body["matches"].as_array().map(Vec::len), Some(3));
    assert!(
        d.file_calls() < 400,
        "the close left {} calls",
        d.file_calls()
    );
}

#[test]
#[serial]
fn a_glob_value_without_a_separator_matches_at_every_depth() {
    let entries = || {
        vec![
            file_entry("README.md", "fn one\n", true),
            file_entry("src/a.ts", "fn one\n", true),
            file_entry("src/deep/b.ts", "fn two\n", true),
        ]
    };

    let unanchored = Deployment::new();
    mount_head(&unanchored);
    mount_tree(&unanchored, None, entries(), false);
    mount_file(&unanchored, "README.md", "fn one\n");
    mount_file(&unanchored, "src/a.ts", "fn one\n");
    mount_file(&unanchored, "src/deep/b.ts", "fn two\n");
    let first = unanchored.run(&["--json", "grep", "fn ", "--repo", REPO, "--glob", "*.ts"]);
    assert_eq!(first.status.code(), Some(0), "{}", stderr_of(&first));
    let fetched: Vec<String> = unanchored
        .requests()
        .iter()
        .filter(|r| r.url.path().contains("/files/"))
        .map(|r| r.url.path().to_string())
        .collect();
    assert_eq!(fetched.len(), 2, "fetched: {fetched:?}");
    assert!(fetched.iter().any(|p| p.ends_with("/files/src/a.ts")));
    assert!(fetched.iter().any(|p| p.ends_with("/files/src/deep/b.ts")));

    let anchored = Deployment::new();
    mount_head(&anchored);
    mount_tree(&anchored, None, entries(), false);
    mount_file(&anchored, "README.md", "fn one\n");
    mount_file(&anchored, "src/a.ts", "fn one\n");
    mount_file(&anchored, "src/deep/b.ts", "fn two\n");
    let second = anchored.run(&[
        "--json", "grep", "fn ", "--repo", REPO, "--glob", "src/*.ts",
    ]);
    assert_eq!(second.status.code(), Some(0), "{}", stderr_of(&second));
    let fetched: Vec<String> = anchored
        .requests()
        .iter()
        .filter(|r| r.url.path().contains("/files/"))
        .map(|r| r.url.path().to_string())
        .collect();
    assert_eq!(fetched.len(), 1, "fetched: {fetched:?}");
    assert!(fetched[0].ends_with("/files/src/a.ts"));
}

#[test]
#[serial]
fn grep_refuses_a_context_option_outside_content_mode() {
    let d = Deployment::new();
    mount_search_tree(&d, true);

    let output = d.run(&[
        "grep", "fn ", "--repo", REPO, "--output", "files", "-A", "2",
    ]);
    assert_eq!(output.status.code(), Some(1));
    assert!(
        stderr_of(&output).contains(
            "error: configuration error: --after-context applies only under --output content"
        ),
        "stderr: {}",
        stderr_of(&output)
    );
    assert!(d.requests().is_empty(), "nothing was sent");
}

#[test]
#[serial]
fn grep_content_render_pipes_line_for_line() {
    let d = Deployment::new();
    mount_search_tree(&d, true);

    let output = d.run(&["grep", "fn ", "--repo", REPO, "-n", "-C", "1"]);
    assert_eq!(output.status.code(), Some(0), "{}", stderr_of(&output));
    let stdout = stdout_of(&output);
    let lines: Vec<&str> = stdout.lines().collect();
    assert_eq!(
        lines,
        vec![
            "src/a.ts-1-plain",
            "src/a.ts:2:fn one",
            "src/a.ts-3-tail",
            "--",
            "src/b.ts:1:fn two",
            "src/b.ts-2-plain",
            "src/b.ts:3:fn two",
        ],
        "stdout: {stdout}"
    );
    assert!(
        stderr_of(&output).contains("read at version"),
        "the reference line stands on the diagnostic stream: {}",
        stderr_of(&output)
    );
}

// u270 V1-05, CR2-1: `rg` writes no `--` where no context option
// stands, so the render that mirrors it writes none either — across the
// gap inside `src/b.ts` and across the boundary between the two paths
// alike. Driven through the binary, so the call deriving the render's
// separator rule from the context window is what the case pins.
#[test]
#[serial]
fn grep_content_render_holds_no_separator_without_a_context_option() {
    let d = Deployment::new();
    mount_search_tree(&d, true);

    let output = d.run(&["grep", "fn ", "--repo", REPO, "-n"]);
    assert_eq!(output.status.code(), Some(0), "{}", stderr_of(&output));
    let stdout = stdout_of(&output);
    let lines: Vec<&str> = stdout.lines().collect();
    assert_eq!(
        lines,
        vec![
            "src/a.ts:2:fn one",
            "src/b.ts:1:fn two",
            "src/b.ts:3:fn two"
        ],
        "stdout: {stdout}"
    );
}

// ------------------------------------------------------------ aliases

#[test]
#[serial]
fn registered_short_aliases_parse() {
    let d = Deployment::new();
    std::fs::write(
        d.work.path().join(".syns.yaml"),
        "owner: alice\nname: notes\n",
    )
    .expect("write identity file");
    mount_head(&d);
    mount_tree(&d, None, vec![file_entry("a.md", "x", true)], false);
    d.mount(
        Mock::given(method("GET"))
            .and(path_regex(r"^/api/v1/.*$"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "data": [], "total": 0, "limit": 20, "offset": 0,
            }))),
    );
    d.mount(
        Mock::given(method("POST"))
            .and(path_regex(r"^/api/v1/.*$"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "owner": "alice", "name": "copy", "description": null,
                "commitSha": HEAD_SHA, "status": "active", "author": null, "tags": [],
                "visibility": "public", "forkedFrom": null, "forkCount": 0,
                "fileCount": 0, "role": null,
                "createdAt": "2026-01-01T00:00:00Z", "updatedAt": "2026-01-01T00:00:00Z",
            }))),
    );
    d.mount(
        Mock::given(method("PATCH"))
            .and(path_regex(r"^/api/v1/.*$"))
            .respond_with(ResponseTemplate::new(200).set_body_json(repo_body(HEAD_SHA))),
    );

    let invocations: Vec<Vec<&str>> = vec![
        vec!["revert", "a.md", "--to", "1", "-m", "msg"],
        vec!["fork", REPO, "-n", "copy"],
        vec!["repo", "-t", "x"],
        vec!["explore", "-t", "x"],
        vec!["upgrade", "-f", "--check-only"],
    ];
    for args in invocations {
        let output = AssertCommand::cargo_bin("syns")
            .expect("syns binary")
            .current_dir(d.work.path())
            .env("SYNS_CONFIG_DIR", d.home.path())
            .env("SYNS_CACHE_DIR", d.cache.path())
            .env_remove("SYNS_URL")
            .env("_INTERNAL_GH_API_BASE", d.server.uri())
            .arg("--server")
            .arg(d.server.uri())
            .args(&args)
            .output()
            .expect("run syns");
        let diagnostic = stderr_of(&output);
        assert_ne!(
            output.status.code(),
            Some(2),
            "`syns {}` ended through the parser: {diagnostic}",
            args.join(" ")
        );
        assert!(
            !diagnostic.contains("unexpected argument"),
            "`syns {}` printed: {diagnostic}",
            args.join(" ")
        );
    }
}

// -------------------------------------------- the findings of round 1

#[test]
#[serial]
fn recursive_listing_names_each_entry_by_its_path() {
    let d = Deployment::new();
    mount_head(&d);
    mount_tree(
        &d,
        None,
        vec![
            file_entry("src/deep/a.ts", "deep", true),
            file_entry("src/a.ts", "shallow", true),
        ],
        false,
    );

    let output = d.run(&["ls", "--repo", REPO, "--recursive"]);
    assert_eq!(output.status.code(), Some(0), "{}", stderr_of(&output));
    let stdout = stdout_of(&output);
    assert!(
        stdout.contains("src/a.ts") && stdout.contains("src/deep/a.ts"),
        "each entry stands under its own path: {stdout}"
    );
    let shallow = stdout.find("src/a.ts").expect("the shallow path");
    let deep = stdout.find("src/deep/a.ts").expect("the deep path");
    assert!(shallow < deep, "the subtree is ordered by path: {stdout}");
}

#[test]
#[serial]
fn a_flat_listing_names_each_entry_by_its_base_name() {
    let d = Deployment::new();
    mount_head(&d);
    mount_tree(
        &d,
        Some("src"),
        vec![file_entry("src/a.ts", "x", true)],
        false,
    );

    let output = d.run(&["ls", "src", "--repo", REPO]);
    assert_eq!(output.status.code(), Some(0), "{}", stderr_of(&output));
    let stdout = stdout_of(&output);
    assert!(
        !stdout.contains("src/a.ts"),
        "the standing listing's Name column is unchanged: {stdout}"
    );
    assert!(stdout.contains("a.ts"), "stdout: {stdout}");
}

#[test]
#[serial]
fn read_at_a_version_sends_the_pinned_ordinal() {
    let d = Deployment::new();
    mount_version(&d, "2", 2, OLD_SHA);
    mount_file(&d, "a.md", "one\ntwo\n");

    let output = d.run(&["read", "a.md", "--repo", REPO, "--version", "2"]);
    assert_eq!(output.status.code(), Some(0), "{}", stderr_of(&output));
    let requests = d.requests();
    let file = requests
        .iter()
        .find(|r| r.url.path().contains("/files/"))
        .expect("a file request");
    assert_eq!(query_of(file, "ref").as_deref(), Some("2"));
}

#[test]
#[serial]
fn glob_at_a_version_sends_the_pinned_ordinal() {
    let d = Deployment::new();
    mount_version(&d, "2", 2, OLD_SHA);
    mount_tree(&d, None, vec![file_entry("src/a.ts", "x", true)], false);

    let output = d.run(&["glob", "**/*.ts", "--repo", REPO, "--version", "2"]);
    assert_eq!(output.status.code(), Some(0), "{}", stderr_of(&output));
    assert_eq!(stdout_of(&output), "src/a.ts\n");
    let requests = d.requests();
    let tree = requests
        .iter()
        .find(|r| r.url.path().ends_with("/tree"))
        .expect("a tree request");
    assert_eq!(query_of(tree, "ref").as_deref(), Some("2"));
}

#[test]
#[serial]
fn grep_at_a_version_sends_the_pinned_ordinal() {
    let d = Deployment::new();
    mount_version(&d, "2", 2, OLD_SHA);
    mount_tree(&d, None, vec![file_entry("src/a.ts", A_TS, true)], false);
    mount_file(&d, "src/a.ts", A_TS);

    let output = d.run(&["grep", "fn ", "--repo", REPO, "--version", "2"]);
    assert_eq!(output.status.code(), Some(0), "{}", stderr_of(&output));
    let requests = d.requests();
    let tree = requests
        .iter()
        .find(|r| r.url.path().ends_with("/tree"))
        .expect("a tree request");
    assert_eq!(query_of(tree, "ref").as_deref(), Some("2"));
    let file = requests
        .iter()
        .find(|r| r.url.path().contains("/files/"))
        .expect("a file request");
    assert_eq!(query_of(file, "ref").as_deref(), Some("2"));
}

#[test]
#[serial]
fn path_narrows_the_tree_read_of_both_search_verbs() {
    let d = Deployment::new();
    mount_head(&d);
    mount_tree(&d, None, vec![file_entry("README.md", "x", true)], false);
    mount_tree(
        &d,
        Some("src"),
        vec![file_entry("src/a.ts", A_TS, true)],
        false,
    );
    mount_file(&d, "src/a.ts", A_TS);

    let globbed = d.run(&["glob", "**/*.ts", "--repo", REPO, "--path", "src"]);
    assert_eq!(globbed.status.code(), Some(0), "{}", stderr_of(&globbed));
    assert_eq!(stdout_of(&globbed), "src/a.ts\n");

    let searched = d.run(&["grep", "fn ", "--repo", REPO, "--path", "src"]);
    assert_eq!(searched.status.code(), Some(0), "{}", stderr_of(&searched));
    assert!(
        stdout_of(&searched).contains("src/a.ts"),
        "stdout: {}",
        stdout_of(&searched)
    );

    let narrowed = d
        .paths()
        .into_iter()
        .filter(|p| p.ends_with("/tree/src"))
        .count();
    assert_eq!(narrowed, 2, "both verbs read the tree under --path");
    assert!(
        !d.paths().iter().any(|p| p.ends_with("/tree")),
        "neither verb read the whole tree: {:?}",
        d.paths()
    );
}

#[test]
#[serial]
fn a_subtree_absent_at_the_pinned_version_names_that_version() {
    let d = Deployment::new();
    mount_version(&d, "2", 2, OLD_SHA);
    d.mount(
        Mock::given(method("GET"))
            .and(path_matcher(format!("/api/v1/repos/{REPO}/tree/nope")))
            .respond_with(ResponseTemplate::new(404).set_body_json(json!({
                "error": "not_found",
            }))),
    );

    for args in [
        vec![
            "glob",
            "*",
            "--repo",
            REPO,
            "--path",
            "nope",
            "--version",
            "2",
        ],
        vec![
            "grep",
            "fn ",
            "--repo",
            REPO,
            "--path",
            "nope",
            "--version",
            "2",
        ],
    ] {
        let output = d.run(&args);
        assert_eq!(output.status.code(), Some(1), "{}", stderr_of(&output));
        assert!(
            stderr_of(&output).contains("path not found at version 2: nope"),
            "`syns {}` printed: {}",
            args.join(" "),
            stderr_of(&output)
        );
    }
}

#[test]
#[serial]
fn head_limit_of_zero_is_refused_before_any_request() {
    let d = Deployment::new();
    mount_search_tree(&d, true);

    let output = d.run(&["grep", "fn ", "--repo", REPO, "--head-limit", "0"]);
    assert_eq!(output.status.code(), Some(1));
    assert!(
        stderr_of(&output).contains("--head-limit must be \u{2265} 1"),
        "stderr: {}",
        stderr_of(&output)
    );
    assert!(d.requests().is_empty(), "no request left: {:?}", d.paths());
}

#[test]
#[serial]
fn a_repo_value_outside_the_registered_spelling_is_refused_at_parse() {
    let d = Deployment::new();
    mount_head(&d);

    for value in ["alice/..", "alice/.", "alice/no tes", "al ice/notes"] {
        let output = d.run(&["ls", "--repo", value]);
        assert_eq!(
            output.status.code(),
            Some(2),
            "`--repo {value}` printed: {}",
            stderr_of(&output)
        );
    }
    assert!(d.requests().is_empty(), "no request left: {:?}", d.paths());
}

#[test]
#[serial]
fn a_fatal_refusal_still_takes_the_eviction_pass() {
    let d = Deployment::new();
    mount_head(&d);
    mount_tree(
        &d,
        None,
        vec![
            file_entry("src/a.ts", A_TS, true),
            file_entry("src/b.ts", B_TS, true),
            file_entry("src/locked.txt", "z", true),
        ],
        false,
    );
    mount_file(&d, "src/a.ts", A_TS);
    mount_file(&d, "src/b.ts", B_TS);
    mount_file_status(
        &d,
        "src/locked.txt",
        401,
        json!({ "error": "unauthorized" }),
    );

    // A first search narrowed past the refusing path fills the store.
    let filled = d.run(&["grep", "fn ", "--repo", REPO, "--glob", "*.ts"]);
    assert_eq!(filled.status.code(), Some(0), "{}", stderr_of(&filled));
    assert!(d.blob_bytes() > 1, "the store holds the two contents");

    // A second search reaches the refusing path and ends at its exit
    // code — with the one eviction pass taken.
    let refused = d.run_capped(Some("1"), &["grep", "fn ", "--repo", REPO]);
    assert_ne!(refused.status.code(), Some(0), "the refusal ends the run");
    assert!(
        d.blob_bytes() <= 1,
        "the store is at or under the cap: {} bytes",
        d.blob_bytes()
    );
}

// ------------------------------------- the truncated-tree arm, u270 V2-16

/// The refusal both verbs raise where the tree arrives truncated, at
/// the version `mount_head` resolves (SPEC u270 Contract Surface, the
/// partial-answer refusal).
const TRUNCATED_REFUSAL: &str = "partial answer: the tree at version 7 arrived truncated";

/// The bytes the deployment itself answered a recursive tree read with,
/// recorded through a forward proxy relaying `https://syns.dev`
/// untouched. `truncated` reads `false` here because every public tree
/// handler writes that literal, which is why no run against the
/// deployment reaches the arm below (u270 V2-16).
const SERVED_TREE: &str = r#"{"entries":[{"name":".syns.yaml","path":".syns.yaml","type":"file","size":43,"sha":"1e51eaa611e1e8040fb0d7e0069d127f991e92c8"},{"name":"CLAUDE.md","path":"CLAUDE.md","type":"file","size":2888,"sha":"174e5d666d0c12f42c49710f282f16283d37e503"},{"name":"DECISIONS.md","path":"DECISIONS.md","type":"file","size":29075,"sha":"242223859d18e348a1d9ad365ae023e51e058662"},{"name":"README.md","path":"README.md","type":"file","size":4623,"sha":"0da0c0444bab6b5d6f44488386764bf40df2f4b8"},{"name":"RESEARCH.md","path":"RESEARCH.md","type":"file","size":4865,"sha":"9b7865f3ad84a7a37ce6837f8ed0aa3c79cac103"},{"name":"SPEC.md","path":"SPEC.md","type":"file","size":44633,"sha":"e12bdb2670ce437787dfbfb1c84110f16776918a"},{"name":"STATUS.md","path":"STATUS.md","type":"file","size":20604,"sha":"69285fe9f0bf8f97b353131a97a08cef2a8f4f77"}],"commitSha":"67e51139a0cb18998e6734c679466d1afc990443","truncated":false}"#;

/// The same bytes with that one key flipped — the only edit the arm
/// needs, and the only one made.
fn served_tree_truncated() -> String {
    let flipped = SERVED_TREE.replace(r#""truncated":false"#, r#""truncated":true"#);
    assert_ne!(flipped, SERVED_TREE, "the served body carries the key");
    flipped
}

#[test]
#[serial]
fn a_served_tree_reaches_the_refusal_on_its_truncated_key_alone() {
    let whole = Deployment::new();
    mount_head(&whole);
    mount_tree_verbatim(&whole, SERVED_TREE);
    let served = whole.run(&["--json", "ls", "--repo", REPO, "--recursive"]);
    assert_eq!(served.status.code(), Some(0), "{}", stderr_of(&served));
    let mut whole_document = document(&served);

    let partial = Deployment::new();
    mount_head(&partial);
    mount_tree_verbatim(&partial, &served_tree_truncated());
    let refused = partial.run(&["--json", "ls", "--repo", REPO, "--recursive"]);
    assert_eq!(refused.status.code(), Some(1), "{}", stderr_of(&refused));
    let partial_document = document(&refused);

    assert_eq!(partial_document["truncated"], json!(true));
    assert_eq!(partial_document["error"], json!(TRUNCATED_REFUSAL));
    assert_eq!(
        partial_document["entries"], whole_document["entries"],
        "the whole listing stands beside the refusal"
    );

    // The two documents differ in the flipped key and the refusal it
    // raised, and in nothing else.
    whole_document["truncated"] = json!(true);
    whole_document["error"] = json!(TRUNCATED_REFUSAL);
    assert_eq!(partial_document, whole_document);
}

#[test]
#[serial]
fn glob_over_a_truncated_tree_refuses() {
    let d = Deployment::new();
    mount_head(&d);
    mount_tree(
        &d,
        None,
        vec![
            file_entry("src/a.ts", A_TS, true),
            file_entry("src/deep/b.ts", B_TS, true),
        ],
        true,
    );

    let output = d.run(&["--json", "glob", "**/*.ts", "--repo", REPO]);
    assert_eq!(output.status.code(), Some(1));
    let body = document(&output);
    let paths: Vec<&str> = body["matches"]
        .as_array()
        .expect("matches")
        .iter()
        .map(|m| m["path"].as_str().unwrap_or_default())
        .collect();
    assert_eq!(paths, vec!["src/a.ts", "src/deep/b.ts"]);
    assert_eq!(body["truncated"], json!(true));
    assert_eq!(body["error"], json!(TRUNCATED_REFUSAL));
}

#[test]
#[serial]
fn a_truncated_tree_names_its_refusal_on_the_diagnostic_stream_alone() {
    let listing = Deployment::new();
    mount_head(&listing);
    mount_tree_verbatim(&listing, &served_tree_truncated());
    let listed = listing.run(&["ls", "--repo", REPO, "--recursive"]);
    assert_eq!(listed.status.code(), Some(1));
    let rows = stdout_of(&listed);
    for path in ["CLAUDE.md", "README.md", "STATUS.md"] {
        assert!(rows.contains(path), "the listing still stands: {rows}");
    }
    assert!(!rows.contains("partial answer"), "stdout: {rows}");
    assert!(
        stderr_of(&listed).contains(&format!("error: {TRUNCATED_REFUSAL}")),
        "stderr: {}",
        stderr_of(&listed)
    );

    let globbing = Deployment::new();
    mount_head(&globbing);
    mount_tree_verbatim(&globbing, &served_tree_truncated());
    let globbed = globbing.run(&["glob", "*.md", "--repo", REPO]);
    assert_eq!(globbed.status.code(), Some(1));
    assert_eq!(
        stdout_of(&globbed)
            .lines()
            .collect::<Vec<&str>>()
            .first()
            .copied(),
        Some("CLAUDE.md"),
        "stdout: {}",
        stdout_of(&globbed)
    );
    assert!(
        stderr_of(&globbed).contains(&format!("error: {TRUNCATED_REFUSAL}")),
        "stderr: {}",
        stderr_of(&globbed)
    );
}

#[test]
#[serial]
fn grep_over_a_truncated_tree_refuses() {
    let d = Deployment::new();
    mount_head(&d);
    mount_tree(&d, None, vec![file_entry("src/a.ts", A_TS, true)], true);
    mount_file(&d, "src/a.ts", A_TS);

    let output = d.run(&["--json", "grep", "fn ", "--repo", REPO]);
    assert_eq!(output.status.code(), Some(1), "{}", stderr_of(&output));
    let body = document(&output);
    assert_eq!(body["truncated"], json!(true));
    assert_eq!(body["error"], json!(TRUNCATED_REFUSAL));
    let matches = body["matches"].as_array().expect("matches");
    assert_eq!(matches.len(), 1, "the readable match stands: {body}");
    assert_eq!(matches[0]["path"], json!("src/a.ts"));
}
