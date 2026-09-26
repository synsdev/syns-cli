//! Binary-level behaviour of the byte writes, the byte read under
//! `--json`, the `--repo` remedy and the empty reference (SPEC u283
//! Tests).
//!
//! Every suite row of that table stands here under the name the table
//! gives it; `binary_round_trip_against_a_local_stack` and
//! `perf_against_the_release` run as steps of the unit's verification
//! alone. Each test drives one mock deployment of its own, recording
//! every request it receives.

use assert_cmd::Command as AssertCommand;
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use serde_json::{Value, json};
use serial_test::serial;
use std::path::Path;
use syns_cli::push::collector::MAX_FILE_BYTES;
use syns_cli::push::hash::blob_sha1;
use tempfile::TempDir;
use wiremock::matchers::{method, path as path_matcher};
use wiremock::{Mock, MockServer, Request, ResponseTemplate};

const REPO: &str = "alice/notes";
const PNG: &[u8] = b"\x89PNG\r\n\x1a\n\x00\xff";
const COMMIT_SHA: &str = "cc33dd44ee55ff6600778899001122bbaa11bb22";

fn head_sha() -> String {
    "a".repeat(40)
}

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
        let deployment = Deployment {
            rt,
            server,
            home: tempfile::tempdir().expect("config dir"),
            cache: tempfile::tempdir().expect("cache dir"),
            work: tempfile::tempdir().expect("working dir"),
        };
        std::fs::write(
            deployment.home.path().join("credentials.json"),
            json!({"token": "test-token", "username": "alice"}).to_string(),
        )
        .expect("credential");
        deployment
    }

    /// The deployment `write_bytes_publishes_them_exactly` names: `alice/notes`
    /// at `aa…aa`, accepting one `PUT` and recording it.
    fn holding_notes() -> Deployment {
        let d = Deployment::new();
        d.mount(
            Mock::given(method("GET"))
                .and(path_matcher(format!("/api/v1/repos/{REPO}")))
                .respond_with(ResponseTemplate::new(200).set_body_json(repo_body())),
        );
        d.mount(
            Mock::given(method("PUT"))
                .and(path_matcher(format!("/api/v1/repos/{REPO}/push")))
                .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                    "commitSha": COMMIT_SHA, "version": 8,
                    "filesChanged": 1, "created": false,
                }))),
        );
        d
    }

    fn mount(&self, mock: Mock) {
        self.rt.block_on(async { mock.mount(&self.server).await });
    }

    fn requests(&self) -> Vec<Request> {
        self.rt
            .block_on(async { self.server.received_requests().await.expect("requests") })
    }

    /// Every push body this deployment received, in order.
    fn pushes(&self) -> Vec<Value> {
        self.requests()
            .iter()
            .filter(|r| r.method.as_str() == "PUT")
            .map(|r| serde_json::from_slice(&r.body).expect("push body"))
            .collect()
    }

    fn run(&self, stdin: &[u8], args: &[&str]) -> std::process::Output {
        self.run_in(self.work.path(), stdin, args)
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
}

fn repo_body() -> Value {
    json!({
        "owner": "alice", "name": "notes", "description": null,
        "commitSha": head_sha(), "status": "active", "author": null, "tags": [],
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

/// The reference every read resolves before it reads: the head at
/// version `7`, and version `7` named directly.
fn mount_reference(d: &Deployment) {
    d.mount(
        Mock::given(method("GET"))
            .and(path_matcher(format!("/api/v1/repos/{REPO}")))
            .respond_with(ResponseTemplate::new(200).set_body_json(repo_body())),
    );
    for reference in [head_sha(), "7".to_string()] {
        d.mount(
            Mock::given(method("GET"))
                .and(path_matcher(format!(
                    "/api/v1/repos/{REPO}/versions/{reference}"
                )))
                .respond_with(
                    ResponseTemplate::new(200).set_body_json(version_body(7, &head_sha())),
                ),
        );
    }
}

/// The raw entry answering `bytes` under `content_type` and `etag`.
fn mount_raw(d: &Deployment, path: &str, bytes: &[u8], content_type: &str, etag: &str) {
    d.mount(
        Mock::given(method("GET"))
            .and(path_matcher(format!("/api/v1/repos/{REPO}/raw/{path}")))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("Content-Type", content_type)
                    .insert_header("ETag", format!("\"{etag}\"").as_str())
                    .set_body_bytes(bytes.to_vec()),
            ),
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

fn keys_of(document: &Value) -> Vec<String> {
    document
        .as_object()
        .expect("an object")
        .keys()
        .cloned()
        .collect()
}

fn write_args<'a>(head: &'a str, path: &'a str, bytes: bool) -> Vec<&'a str> {
    let mut args = vec!["write", path];
    if bytes {
        args.push("--bytes");
    }
    args.extend(["--parent", head, "--repo", REPO]);
    args
}

fn commit_args(head: &str) -> Vec<&str> {
    vec!["commit", "--parent", head, "--repo", REPO]
}

#[test]
#[serial]
fn write_bytes_publishes_them_exactly() {
    let d = Deployment::holding_notes();
    let head = head_sha();

    let output = d.run(PNG, &write_args(&head, "image.png", true));
    assert_eq!(exit_of(&output), 0, "{}", stderr_of(&output));

    let pushes = d.pushes();
    assert_eq!(pushes.len(), 1);
    let files = pushes[0]["files"].as_array().expect("files");
    assert_eq!(files.len(), 1);
    let entry = &files[0];
    assert_eq!(entry["path"], json!("image.png"));
    let sent = entry["contentBase64"].as_str().expect("contentBase64");
    assert_eq!(STANDARD.decode(sent).expect("standard base64"), PNG);
    assert!(entry.get("content").is_none());
    assert_eq!(entry["sha"], json!(blob_sha1(PNG)));
}

#[test]
#[serial]
fn declared_text_rides_as_content() {
    let d = Deployment::holding_notes();
    let head = head_sha();

    let wrote = d.run(b"hello\n", &write_args(&head, "a.md", true));
    assert_eq!(exit_of(&wrote), 0, "{}", stderr_of(&wrote));
    let committed = d.run(
        br#"{"files":[{"path":"a.md","contentBase64":"aGVsbG8K"}]}"#,
        &commit_args(&head),
    );
    assert_eq!(exit_of(&committed), 0, "{}", stderr_of(&committed));

    let pushes = d.pushes();
    assert_eq!(pushes.len(), 2);
    for push in pushes {
        let entry = &push["files"][0];
        assert_eq!(entry["content"], json!("hello\n"));
        assert!(entry.get("contentBase64").is_none());
        assert_eq!(keys_of(entry), vec!["content", "path", "sha"]);
    }
}

#[test]
#[serial]
fn undeclared_bytes_are_still_refused() {
    let d = Deployment::holding_notes();
    let head = head_sha();

    let output = d.run(b"ab\x00cd", &write_args(&head, "b.bin", false));
    assert_eq!(exit_of(&output), 1);
    assert_eq!(
        stderr_of(&output).trim_end(),
        "error: cannot write content that is not text: b.bin \u{2014} pass --bytes to publish its bytes exactly"
    );
    assert!(d.pushes().is_empty(), "no PUT");
}

#[test]
#[serial]
fn a_content_past_the_bound_sends_nothing() {
    let d = Deployment::holding_notes();
    let head = head_sha();
    let size = MAX_FILE_BYTES as usize + 1;
    let past = vec![0u8; size];
    let line = |path: &str| {
        format!(
            "error: payload_too_large: {path} holds {size} bytes, past the 25 MiB one file may hold; nothing was sent"
        )
    };

    let wrote = d.run(&past, &write_args(&head, "big.bin", true));
    assert_eq!(exit_of(&wrote), 1);
    assert_eq!(stderr_of(&wrote).trim_end(), line("big.bin"));

    let document = json!({"files": [{"path": "big.bin", "contentBase64": STANDARD.encode(&past)}]});
    let committed = d.run(document.to_string().as_bytes(), &commit_args(&head));
    assert_eq!(exit_of(&committed), 1);
    assert_eq!(stderr_of(&committed).trim_end(), line("big.bin"));

    assert!(d.pushes().is_empty(), "no PUT");
}

#[test]
#[serial]
fn a_changeset_carries_text_and_bytes_in_one_commit() {
    let d = Deployment::holding_notes();
    let head = head_sha();
    let jpeg: &[u8] = b"\xff\xd8\xff\x00\x10JFIF";
    let sent = STANDARD.encode(jpeg);
    let document = json!({"files": [
        {"path": "photo.jpg", "contentBase64": sent},
        {"path": "a.md", "content": "# a\n"},
    ]});

    let mut args = vec!["--json"];
    args.extend(commit_args(&head));
    let output = d.run(document.to_string().as_bytes(), &args);
    assert_eq!(exit_of(&output), 0, "{}", stderr_of(&output));

    let pushes = d.pushes();
    assert_eq!(pushes.len(), 1);
    let files = pushes[0]["files"].as_array().expect("files");
    assert_eq!(files.len(), 2);
    assert_eq!(files[0]["path"], json!("a.md"));
    assert_eq!(files[0]["content"], json!("# a\n"));
    assert!(files[0].get("contentBase64").is_none());
    assert_eq!(files[0]["sha"], json!(blob_sha1(b"# a\n")));
    assert_eq!(files[1]["path"], json!("photo.jpg"));
    assert_eq!(files[1]["contentBase64"], json!(sent));
    assert!(files[1].get("content").is_none());
    assert_eq!(files[1]["sha"], json!(blob_sha1(jpeg)));
}

#[test]
#[serial]
fn malformed_changeset_entries_are_refused() {
    let d = Deployment::holding_notes();
    let head = head_sha();

    for (document, line) in [
        (
            r#"{"files":[{"path":"a.md","content":"x","contentBase64":"eA=="}]}"#,
            "error: configuration error: a.md carries both content and contentBase64 in one changeset",
        ),
        (
            r#"{"files":[{"path":"a.md"}]}"#,
            "error: configuration error: a.md carries neither content nor contentBase64 in one changeset",
        ),
        (
            r#"{"files":[{"path":"a.md","contentBase64":"aGVs bG8K"}]}"#,
            "error: configuration error: the contentBase64 of a.md is not standard padded base64",
        ),
        (
            r#"{"files":[{"path":"a.md","contentBase64":"aGVsbG8"}]}"#,
            "error: configuration error: the contentBase64 of a.md is not standard padded base64",
        ),
        (
            r#"{"files":[{"path":"a.md","contentBase64":"aGVsbG9="}]}"#,
            "error: configuration error: the contentBase64 of a.md is not standard padded base64",
        ),
    ] {
        let output = d.run(document.as_bytes(), &commit_args(&head));
        assert_eq!(exit_of(&output), 1, "{document}");
        assert_eq!(stderr_of(&output).trim_end(), line, "{document}");
    }
    assert!(d.pushes().is_empty(), "no PUT");
}

#[test]
#[serial]
fn a_nul_in_content_names_the_byte_member() {
    let d = Deployment::holding_notes();
    let head = head_sha();

    let output = d.run(
        br#"{"files":[{"path":"a.bin","content":"a\u0000b"}]}"#,
        &commit_args(&head),
    );
    assert_eq!(exit_of(&output), 1);
    assert_eq!(
        stderr_of(&output).trim_end(),
        "error: cannot write content that is not text: a.bin \u{2014} send it as contentBase64 to publish its bytes exactly"
    );
    assert!(d.pushes().is_empty(), "no PUT");
}

#[test]
#[serial]
fn edit_refuses_not_text_naming_the_whole_write() {
    let d = Deployment::holding_notes();
    let head = head_sha();
    d.mount(
        Mock::given(method("GET"))
            .and(path_matcher(format!(
                "/api/v1/repos/{REPO}/files/image.png"
            )))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "path": "image.png", "content": null, "sha": blob_sha1(PNG), "size": 10,
            }))),
    );

    let output = d.run(
        b"",
        &[
            "edit",
            "image.png",
            "--old",
            "a",
            "--new",
            "b",
            "--parent",
            &head,
            "--repo",
            REPO,
        ],
    );
    assert_eq!(exit_of(&output), 1);
    assert_eq!(
        stderr_of(&output).trim_end(),
        "error: cannot write content that is not text: image.png \u{2014} edit changes text alone; replace it whole with syns write image.png --bytes"
    );
    assert!(d.pushes().is_empty(), "no PUT");
}

#[test]
#[serial]
fn cat_json_answers_bytes_as_base64() {
    let d = Deployment::new();
    mount_reference(&d);
    mount_raw(&d, "image.png", PNG, "image/png", &blob_sha1(PNG));

    let output = d.run(b"", &["--json", "cat", "image.png", "--repo", REPO]);
    assert_eq!(exit_of(&output), 0, "{}", stderr_of(&output));
    let document = one_document(&output);
    assert_eq!(
        keys_of(&document),
        vec![
            "commitSha",
            "contentBase64",
            "mediaType",
            "path",
            "sha",
            "size",
            "version"
        ]
    );
    let sent = document["contentBase64"].as_str().expect("contentBase64");
    assert_eq!(STANDARD.decode(sent).expect("standard base64"), PNG);
    assert_eq!(document["mediaType"], json!("image/png"));
    assert_eq!(document["size"], json!(10));
    assert_eq!(document["sha"], json!(blob_sha1(PNG)));
    assert_eq!(document["path"], json!("image.png"));
    assert_eq!(document["version"], json!(7));
    assert_eq!(document["commitSha"], json!(head_sha()));
}

#[test]
#[serial]
fn cat_json_omits_the_unnamed_type() {
    let d = Deployment::new();
    mount_reference(&d);
    mount_raw(
        &d,
        "blob.bin",
        PNG,
        "application/octet-stream",
        &blob_sha1(PNG),
    );

    let output = d.run(b"", &["--json", "cat", "blob.bin", "--repo", REPO]);
    assert_eq!(exit_of(&output), 0, "{}", stderr_of(&output));
    let document = one_document(&output);
    assert!(document.get("mediaType").is_none(), "{document}");
    assert!(document.get("content").is_none(), "{document}");
    assert!(document.get("contentBase64").is_some());
}

#[test]
#[serial]
fn cat_json_keeps_the_released_text_document() {
    let d = Deployment::new();
    mount_reference(&d);
    mount_raw(
        &d,
        "a.md",
        b"# a\n",
        "text/plain; charset=utf-8",
        &blob_sha1(b"# a\n"),
    );

    let output = d.run(b"", &["--json", "cat", "a.md", "--repo", REPO]);
    assert_eq!(exit_of(&output), 0, "{}", stderr_of(&output));
    assert_eq!(
        one_document(&output),
        json!({
            "commitSha": head_sha(),
            "content": "# a\n",
            "mediaType": null,
            "path": "a.md",
            "sha": blob_sha1(b"# a\n"),
            "size": 4,
            "version": 7,
        })
    );
    // Pretty-printed as every document, keys in lexical order.
    assert!(stdout_of(&output).starts_with("{\n  \"commitSha\": "));
}

#[test]
#[serial]
fn cat_json_refuses_a_hash_mismatch() {
    let d = Deployment::new();
    mount_reference(&d);
    mount_raw(
        &d,
        "image.png",
        PNG,
        "image/png",
        &blob_sha1(b"other bytes"),
    );

    let output = d.run(b"", &["--json", "cat", "image.png", "--repo", REPO]);
    assert_eq!(exit_of(&output), 1);
    let document = one_document(&output);
    let error = document["error"].as_str().expect("error");
    assert!(
        error.starts_with("server error (200): invalid response body: image.png: expected "),
        "{error}"
    );
    assert!(document.get("content").is_none());
    assert!(document.get("contentBase64").is_none());
}

#[test]
#[serial]
fn cat_json_keeps_its_not_found_refusals() {
    let d = Deployment::new();
    mount_reference(&d);
    d.mount(
        Mock::given(method("GET"))
            .and(path_matcher(format!("/api/v1/repos/{REPO}/raw/gone.md")))
            .respond_with(ResponseTemplate::new(404).set_body_json(json!({
                "error": "not_found", "message": "File not found",
            }))),
    );

    let bare = d.run(b"", &["--json", "cat", "gone.md", "--repo", REPO]);
    assert_eq!(exit_of(&bare), 1);
    assert_eq!(
        one_document(&bare),
        json!({"error": "server error (404): not_found"})
    );

    let versioned = d.run(
        b"",
        &["--json", "cat", "gone.md", "--repo", REPO, "--version", "7"],
    );
    assert_eq!(exit_of(&versioned), 1);
    assert_eq!(
        one_document(&versioned),
        json!({"error": "path not found at version 7: gone.md"})
    );
}

#[test]
#[serial]
fn every_repo_taking_verb_names_repo() {
    let d = Deployment::new();
    let head = head_sha();
    let nowhere = tempfile::tempdir().expect("a directory no identity reaches");
    let line = "cannot determine repo identity \u{2014} pass --repo OWNER/NAME, or run inside a directory at or below one holding .syns.yaml";

    let invocations: Vec<Vec<&str>> = vec![
        vec!["cat", "a.md"],
        vec!["ls"],
        vec!["read", "a.md"],
        vec!["glob", "*.md"],
        vec!["grep", "x"],
        vec!["history", "show", "1"],
        vec!["forks"],
        vec![
            "edit", "a.md", "--old", "a", "--new", "b", "--parent", &head,
        ],
        vec!["write", "a.md", "--parent", &head],
        vec!["rm", "a.md", "--parent", &head],
        vec!["commit", "--parent", &head],
    ];
    for invocation in &invocations {
        let bare = d.run_in(nowhere.path(), b"", invocation);
        assert_eq!(exit_of(&bare), 2, "{invocation:?}: {}", stderr_of(&bare));
        assert_eq!(
            stderr_of(&bare).trim_end(),
            format!("error: {line}"),
            "{invocation:?}"
        );

        let mut json_args = vec!["--json"];
        json_args.extend(invocation.iter().copied());
        let json_run = d.run_in(nowhere.path(), b"", &json_args);
        assert_eq!(exit_of(&json_run), 2, "{invocation:?}");
        assert_eq!(
            one_document(&json_run),
            json!({ "error": line }),
            "{invocation:?}"
        );
    }

    // Every other invocation keeps its line.
    let push = d.run_in(nowhere.path(), b"", &["push"]);
    assert_eq!(exit_of(&push), 2);
    assert!(
        stderr_of(&push).contains("provide --name"),
        "{}",
        stderr_of(&push)
    );
    let status = d.run_in(nowhere.path(), b"", &["status"]);
    assert_eq!(exit_of(&status), 2);
    assert_eq!(
        stderr_of(&status).trim_end(),
        "error: cannot determine repo identity \u{2014} run inside a directory at or below one holding .syns.yaml"
    );
    assert!(d.requests().is_empty(), "no request is made");
}

#[test]
#[serial]
fn an_empty_reference_reaches_no_request() {
    let d = Deployment::new();
    let invocations: Vec<Vec<&str>> = vec![
        vec!["history", "show", "", "--repo", REPO],
        vec!["cat", "a.md", "--version", "", "--repo", REPO],
        vec!["ls", "--version", "", "--repo", REPO],
        vec!["read", "a.md", "--version", "", "--repo", REPO],
        vec!["glob", "*.md", "--version", "", "--repo", REPO],
        vec!["grep", "x", "--version", "", "--repo", REPO],
    ];
    let line = "configuration error: version cannot be empty";
    for invocation in &invocations {
        let bare = d.run(b"", invocation);
        assert_eq!(exit_of(&bare), 1, "{invocation:?}: {}", stderr_of(&bare));
        assert_eq!(
            stderr_of(&bare).trim_end(),
            format!("error: {line}"),
            "{invocation:?}"
        );

        let mut json_args = vec!["--json"];
        json_args.extend(invocation.iter().copied());
        let json_run = d.run(b"", &json_args);
        assert_eq!(exit_of(&json_run), 1, "{invocation:?}");
        assert_eq!(
            one_document(&json_run),
            json!({ "error": line }),
            "{invocation:?}"
        );
    }
    assert!(d.requests().is_empty(), "the mock recorded no request");
}
