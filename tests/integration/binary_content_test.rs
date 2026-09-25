//! u280 — a folder publishes and comes back byte for byte whatever its
//! files hold, and every file a publication or a convergence leaves
//! behind is named with the rule that left it.
//!
//! Every case here drives the built binary: against wiremock where one
//! answer per address is enough, and against the stateful fake
//! `convergence_test` stands up where a run reads, writes and publishes
//! over one moving head.

use std::path::{Path, PathBuf};
use std::process::Output;

use base64::Engine as _;
use serde_json::{Value, json};
use serial_test::serial;
use syns_cli::auth::token::TokenStore;
use syns_cli::push::collector::MAX_FILE_BYTES;
use syns_cli::push::hash::blob_sha1;
use syns_cli::push::manifest::Manifest;
use syns_cli::push::working_copy::{Resolution, WorkingCopy};
use tempfile::TempDir;
use wiremock::matchers::{header, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use super::convergence_test::Fake;

const PNG: &[u8] = b"\x89PNG\r\n\x1a\n\x00\x00\x00\rIHDR";

/// One machine's worth of state for the binary: a configuration holding
/// alice's credential, a cache, and a folder to run in.
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
        self.folder.path().to_path_buf()
    }

    /// Run the binary in `cwd`, off the async runtime so a fake served
    /// on it keeps answering.
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

    fn copy(&self) -> WorkingCopy {
        WorkingCopy::open(self.cache.path(), "alice", "proj", self.folder.path()).unwrap()
    }
}

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

fn document(output: &Output) -> Value {
    serde_json::from_str(stdout(output).trim())
        .unwrap_or_else(|e| panic!("stdout is no one document ({e}): {}", stdout(output)))
}

fn write(dir: &Path, path: &str, bytes: &[u8]) {
    let target = dir.join(path);
    std::fs::create_dir_all(target.parent().unwrap()).unwrap();
    std::fs::write(target, bytes).unwrap();
}

/// A sparse file of exactly `len` bytes.
fn sparse(dir: &Path, path: &str, len: u64) {
    let target = dir.join(path);
    std::fs::create_dir_all(target.parent().unwrap()).unwrap();
    std::fs::File::create(target).unwrap().set_len(len).unwrap();
}

fn identity() -> &'static [u8] {
    b"owner: alice\nname: proj\n"
}

fn push_ok() -> ResponseTemplate {
    ResponseTemplate::new(200).set_body_json(json!({
        "commitSha": "c1c1c1c1c1c1c1c1c1c1c1c1c1c1c1c1c1c1c1c1",
        "version": 1,
        "filesChanged": 1,
        "created": true,
    }))
}

/// A wiremock deployment answering a first publication into
/// `alice/proj`: its tree not found and every `PUT` accepted.
async fn first_push_server() -> MockServer {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/v1/repos/alice/proj/tree"))
        .respond_with(ResponseTemplate::new(404).set_body_json(json!({"error": "not_found"})))
        .mount(&server)
        .await;
    Mock::given(method("PUT"))
        .and(path("/api/v1/repos/alice/proj/push"))
        .respond_with(push_ok())
        .mount(&server)
        .await;
    server
}

async fn put_bodies(server: &MockServer) -> Vec<Value> {
    server
        .received_requests()
        .await
        .unwrap()
        .iter()
        .filter(|r| r.method == reqwest::Method::PUT)
        .map(|r| serde_json::from_slice(&r.body).unwrap())
        .collect()
}

fn entry<'a>(body: &'a Value, path: &str) -> &'a Value {
    body["files"]
        .as_array()
        .unwrap()
        .iter()
        .find(|f| f["path"] == path)
        .unwrap_or_else(|| panic!("no entry for {path} in {body}"))
}

// ---- the build names itself ---------------------------------------------

#[tokio::test(flavor = "multi_thread")]
#[serial]
async fn every_request_names_the_build() {
    let agent = format!("syns/{}", env!("CARGO_PKG_VERSION"));
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/v1/repos/alice/proj/tree"))
        .respond_with(ResponseTemplate::new(404).set_body_json(json!({"error": "not_found"})))
        .mount(&server)
        .await;
    Mock::given(method("PUT"))
        .and(path("/api/v1/repos/alice/proj/push"))
        .and(header("user-agent", agent.as_str()))
        .respond_with(push_ok())
        .mount(&server)
        .await;
    const HEAD: &str = "c1c1c1c1c1c1c1c1c1c1c1c1c1c1c1c1c1c1c1c1";
    Mock::given(method("GET"))
        .and(path("/api/v1/repos/alice/proj"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "owner": "alice", "name": "proj", "description": null,
            "commitSha": HEAD, "status": "active", "author": null, "tags": [],
            "visibility": "public", "forkedFrom": null, "forkCount": 0,
            "fileCount": 1, "role": null,
            "createdAt": "2026-01-01T00:00:00Z", "updatedAt": "2026-01-01T00:00:00Z",
        })))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path(format!("/api/v1/repos/alice/proj/versions/{HEAD}")))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "version": 1, "sha": HEAD, "parentSha": null, "message": "m",
            "messageBody": null, "author": "alice",
            "createdAt": "2026-01-01T00:00:00Z", "filesChanged": ["a.md"],
        })))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/api/v1/repos/alice/proj/files/a.md"))
        .and(header("user-agent", agent.as_str()))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "path": "a.md", "sha": blob_sha1(b"a\n"), "content": "a\n", "size": 2,
        })))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/api/auth/sign-out"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"success": true})))
        .mount(&server)
        .await;
    let m = Machine::new(&server.uri());
    write(&m.dir(), "a.md", b"a\n");

    let pushed = m.run(&["push", "--name", "proj"]).await;
    let read = m.run(&["read", "a.md"]).await;
    let logged_out = m.run(&["logout"]).await;

    assert_eq!(pushed.status.code(), Some(0), "{}", stderr(&pushed));
    assert_eq!(read.status.code(), Some(0), "{}", stderr(&read));
    assert_eq!(logged_out.status.code(), Some(0), "{}", stderr(&logged_out));
    let requests = server.received_requests().await.unwrap();
    let sign_out = requests
        .iter()
        .find(|r| r.url.path() == "/api/auth/sign-out")
        .expect("a sign-out request");
    assert_eq!(
        sign_out
            .headers
            .get("user-agent")
            .and_then(|v| v.to_str().ok()),
        Some(agent.as_str())
    );
    assert!(
        requests.iter().all(
            |r| r.headers.get("user-agent").and_then(|v| v.to_str().ok()) == Some(agent.as_str())
        ),
        "a request left without the build's User-Agent"
    );
}

// ---- reads ----------------------------------------------------------------

async fn mount_reference(server: &MockServer) {
    const HEAD: &str = "def4560000000000000000000000000000000000";
    Mock::given(method("GET"))
        .and(path("/api/v1/repos/alice/proj"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "owner": "alice", "name": "proj", "description": null,
            "commitSha": HEAD, "status": "active", "author": null, "tags": [],
            "visibility": "public", "forkedFrom": null, "forkCount": 0,
            "fileCount": 1, "role": null,
            "createdAt": "2026-01-01T00:00:00Z", "updatedAt": "2026-01-01T00:00:00Z",
        })))
        .mount(server)
        .await;
    Mock::given(method("GET"))
        .and(path(format!("/api/v1/repos/alice/proj/versions/{HEAD}")))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "version": 7, "sha": HEAD, "parentSha": null, "message": "m",
            "messageBody": null, "author": "alice",
            "createdAt": "2026-01-01T00:00:00Z", "filesChanged": ["image.png"],
        })))
        .mount(server)
        .await;
}

#[tokio::test(flavor = "multi_thread")]
#[serial]
async fn cat_passes_bytes_through() {
    let bytes: &[u8] = b"\x89PNG\r\n\x1a\n\x00\xff";
    let server = MockServer::start().await;
    mount_reference(&server).await;
    Mock::given(method("GET"))
        .and(path("/api/v1/repos/alice/proj/raw/image.png"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("ETag", format!("\"{}\"", blob_sha1(bytes)).as_str())
                .set_body_bytes(bytes.to_vec()),
        )
        .mount(&server)
        .await;
    let m = Machine::new(&server.uri());
    write(&m.dir(), ".syns.yaml", identity());

    let out = m.run(&["cat", "image.png"]).await;

    assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
    assert_eq!(out.stdout, bytes);
}

#[tokio::test(flavor = "multi_thread")]
#[serial]
async fn null_content_reads_as_not_text() {
    let server = MockServer::start().await;
    mount_reference(&server).await;
    Mock::given(method("GET"))
        .and(path("/api/v1/repos/alice/proj/files/image.png"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "path": "image.png", "sha": blob_sha1(PNG), "content": null,
            "size": PNG.len(), "mediaType": "image/png",
        })))
        .mount(&server)
        .await;
    let m = Machine::new(&server.uri());
    write(&m.dir(), ".syns.yaml", identity());

    let out = m.run(&["read", "image.png"]).await;

    assert_eq!(out.status.code(), Some(1), "{}", stderr(&out));
    let diagnostic = stderr(&out);
    assert!(
        diagnostic.contains("cannot number content that is not text: image.png"),
        "{diagnostic}"
    );
    assert!(
        !diagnostic.contains("invalid response body"),
        "{diagnostic}"
    );
    assert!(!stdout(&out).contains("invalid response body"));
}

// ---- the drops ------------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
#[serial]
async fn strict_refuses_only_for_size() {
    let server = first_push_server().await;
    let m = Machine::new(&server.uri());
    let dir = m.dir();
    write(&dir, ".synsignore", b"shots/\n");
    write(&dir, "shots/a.png", PNG);
    write(&dir, "node_modules/x.js", b"x\n");
    write(&dir, "a.md", b"a\n");

    let out = m.run(&["push", "--name", "proj", "--strict"]).await;

    assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
    assert_eq!(put_bodies(&server).await.len(), 1);
    let diagnostic = stderr(&out);
    assert!(
        diagnostic.contains(".synsignore rule (1): shots/a.png"),
        "{diagnostic}"
    );
    assert!(!diagnostic.contains("--strict"), "{diagnostic}");
    assert!(!diagnostic.contains("binary"), "{diagnostic}");
}

#[tokio::test(flavor = "multi_thread")]
#[serial]
async fn strict_refuses_a_too_large_drop() {
    let server = MockServer::start().await;
    let m = Machine::new(&server.uri());
    let dir = m.dir();
    sparse(&dir, "big.bin", MAX_FILE_BYTES + 1);
    write(&dir, "a.md", b"a\n");

    let out = m.run(&["push", "--name", "proj", "--strict"]).await;

    assert_eq!(out.status.code(), Some(3), "{}", stderr(&out));
    assert!(
        stderr(&out).contains("push aborted: 1 file(s) were skipped under --strict"),
        "{}",
        stderr(&out)
    );
    assert!(server.received_requests().await.unwrap().is_empty());
}

#[tokio::test(flavor = "multi_thread")]
#[serial]
async fn too_large_is_dropped_and_named() {
    let server = first_push_server().await;
    let m = Machine::new(&server.uri());
    let dir = m.dir();
    for i in 0..7 {
        sparse(&dir, &format!("big{i}.bin"), MAX_FILE_BYTES + 1);
    }
    sparse(&dir, "exact.bin", MAX_FILE_BYTES);
    write(&dir, "a.md", b"a\n");

    let out = m.run(&["push", "--name", "proj"]).await;

    assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
    let bodies = put_bodies(&server).await;
    let sent: Vec<&str> = bodies
        .iter()
        .flat_map(|b| b["files"].as_array().unwrap())
        .map(|f| f["path"].as_str().unwrap())
        .collect();
    assert!(sent.contains(&"exact.bin"), "{sent:?}");
    assert!(!sent.iter().any(|p| p.starts_with("big")), "{sent:?}");
    let diagnostic = stderr(&out);
    let names: Vec<String> = (0..7).map(|i| format!("big{i}.bin")).collect();
    assert!(
        diagnostic.contains(&format!(
            "larger than {} MiB (7): {}",
            MAX_FILE_BYTES / (1024 * 1024),
            names.join(", ")
        )),
        "{diagnostic}"
    );
    assert_eq!(
        diagnostic
            .matches(&format!(
                "larger than {} MiB (7)",
                MAX_FILE_BYTES / (1024 * 1024)
            ))
            .count(),
        1,
        "the too-large line stands once: {diagnostic}"
    );
    assert!(!diagnostic.contains("more"), "{diagnostic}");
    assert!(!diagnostic.contains("binary"), "{diagnostic}");
    assert!(!stdout(&out).contains("binary"));
}

/// CR1-2: a convergence writes the too-large line once, and none in
/// machine-readable mode.
#[tokio::test(flavor = "multi_thread")]
#[serial]
async fn sync_names_a_too_large_drop_once() {
    let fake = Fake::start().await;
    let m = Machine::new(&fake.uri);
    write(&m.dir(), "a.md", b"a\n");
    let first = m.run(&["push", "--name", "proj"]).await;
    assert_eq!(first.status.code(), Some(0), "{}", stderr(&first));
    sparse(&m.dir(), "big.bin", MAX_FILE_BYTES + 1);

    let out = m.run(&["sync"]).await;
    let json = m.run(&["--json", "sync"]).await;

    assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
    let line = format!(
        "  larger than {} MiB (1): big.bin",
        MAX_FILE_BYTES / (1024 * 1024)
    );
    assert_eq!(
        stderr(&out).lines().filter(|l| *l == line).count(),
        1,
        "{}",
        stderr(&out)
    );
    assert_eq!(json.status.code(), Some(0), "{}", stderr(&json));
    assert!(!stderr(&json).contains("larger than"), "{}", stderr(&json));
    assert!(!stdout(&json).contains("larger than"), "{}", stdout(&json));
}

// ---- publication -----------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
#[serial]
async fn latin1_and_png_publish_beside_text() {
    let server = first_push_server().await;
    let m = Machine::new(&server.uri());
    let dir = m.dir();
    write(&dir, "README.md", b"# readme\n");
    write(&dir, "latin1.txt", b"caf\xe9\n");
    write(&dir, "image.png", PNG);

    let out = m.run(&["push", "--name", "proj"]).await;

    assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
    let bodies = put_bodies(&server).await;
    assert_eq!(bodies.len(), 1);
    let readme = entry(&bodies[0], "README.md");
    assert_eq!(readme["content"], json!("# readme\n"));
    assert!(readme.get("contentBase64").is_none());
    for (name, bytes) in [("latin1.txt", &b"caf\xe9\n"[..]), ("image.png", PNG)] {
        let e = entry(&bodies[0], name);
        assert!(e.get("content").is_none(), "{e}");
        let decoded = base64::engine::general_purpose::STANDARD
            .decode(e["contentBase64"].as_str().unwrap())
            .unwrap();
        assert_eq!(decoded, bytes, "{name}");
        assert_eq!(e["sha"], json!(blob_sha1(bytes)), "{name}");
    }
    assert_eq!(readme["sha"], json!(blob_sha1(b"# readme\n")));
}

#[tokio::test(flavor = "multi_thread")]
#[serial]
async fn missing_blobs_resend_keeps_the_byte_field() {
    let server = MockServer::start().await;
    Mock::given(method("PUT"))
        .and(path("/api/v1/repos/alice/proj/push"))
        .respond_with(ResponseTemplate::new(409).set_body_json(json!({"error": "missing_blobs"})))
        .with_priority(1)
        .up_to_n_times(1)
        .mount(&server)
        .await;
    Mock::given(method("PUT"))
        .and(path("/api/v1/repos/alice/proj/push"))
        .respond_with(push_ok())
        .with_priority(2)
        .mount(&server)
        .await;
    let m = Machine::new(&server.uri());
    let dir = m.dir();
    write(&dir, ".syns.yaml", identity());
    write(&dir, "image.png", PNG);
    write(&dir, "a.md", b"a edited\n");
    let mut record = Manifest::default();
    record.update(
        "b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0".into(),
        [
            (".syns.yaml".to_string(), blob_sha1(identity())),
            ("image.png".to_string(), blob_sha1(PNG)),
            ("a.md".to_string(), blob_sha1(b"a\n")),
        ]
        .into_iter()
        .collect(),
    );
    record.save(m.cache.path(), "alice", "proj").unwrap();

    let out = m.run(&["push", dir.to_str().unwrap()]).await;

    assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
    let bodies = put_bodies(&server).await;
    assert_eq!(bodies.len(), 2);
    assert!(
        entry(&bodies[0], "image.png")
            .get("contentBase64")
            .is_none()
    );
    let resent = entry(&bodies[1], "image.png");
    assert!(resent.get("content").is_none(), "{resent}");
    assert_eq!(
        base64::engine::general_purpose::STANDARD
            .decode(resent["contentBase64"].as_str().unwrap())
            .unwrap(),
        PNG
    );
}

#[tokio::test(flavor = "multi_thread")]
#[serial]
async fn chunker_packs_by_encoded_length() {
    let server = first_push_server().await;
    let m = Machine::new(&server.uri());
    let dir = m.dir();
    for (name, seed) in [("big1.bin", 7u8), ("big2.bin", 11u8)] {
        let bytes: Vec<u8> = (0..10 * 1024 * 1024u32)
            .map(|i| (i as u8).wrapping_mul(seed).wrapping_add(seed))
            .collect();
        write(&dir, name, &bytes);
    }
    write(&dir, ".syns.yaml", identity());

    let out = m
        .run(&["push", "--name", "proj", "--exclude", ".syns.yaml"])
        .await;

    assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
    let bodies = put_bodies(&server).await;
    assert_eq!(bodies.len(), 2, "two PUT requests");
    for body in &bodies {
        assert_eq!(
            body["files"].as_array().unwrap().len(),
            1,
            "{}",
            body["files"]
        );
    }
}

// ---- retrieval --------------------------------------------------------------

const H1: &str = "a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1";

async fn mount_tree(server: &MockServer, entries: Value) {
    Mock::given(method("GET"))
        .and(path("/api/v1/repos/alice/r/tree"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "entries": entries, "commitSha": H1, "truncated": false,
        })))
        .mount(server)
        .await;
}

async fn mount_raw(server: &MockServer, name: &str, bytes: &[u8]) {
    Mock::given(method("GET"))
        .and(path(format!("/api/v1/repos/alice/r/raw/{name}")))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("ETag", format!("\"{}\"", blob_sha1(bytes)).as_str())
                .set_body_bytes(bytes.to_vec()),
        )
        .mount(server)
        .await;
}

fn tree_entry(name: &str, bytes: &[u8]) -> Value {
    json!({"name": name, "path": name, "type": "file", "size": bytes.len(), "sha": blob_sha1(bytes)})
}

fn files_under(dir: &Path) -> Vec<String> {
    let mut out = Vec::new();
    for entry in walk(dir) {
        out.push(
            entry
                .strip_prefix(dir)
                .unwrap()
                .to_string_lossy()
                .into_owned(),
        );
    }
    out.sort();
    out
}

fn walk(dir: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    for entry in std::fs::read_dir(dir).into_iter().flatten().flatten() {
        let p = entry.path();
        if p.is_dir() {
            out.extend(walk(&p));
        } else {
            out.push(p);
        }
    }
    out
}

#[tokio::test(flavor = "multi_thread")]
#[serial]
async fn pull_writes_bytes_exact() {
    let bytes: &[u8] = b"\x89PNG\x00\xff\x00\xfe";
    let server = MockServer::start().await;
    mount_tree(&server, json!([tree_entry("image.png", bytes)])).await;
    mount_raw(&server, "image.png", bytes).await;
    let m = Machine::new(&server.uri());
    let target = m.dir().join("DIR");

    let out = m.run(&["pull", "alice/r", target.to_str().unwrap()]).await;

    assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
    assert_eq!(std::fs::read(target.join("image.png")).unwrap(), bytes);
}

#[tokio::test(flavor = "multi_thread")]
#[serial]
async fn pull_refuses_a_hash_mismatch() {
    let bytes: &[u8] = b"\x89PNG\x00\xff\x00\xfe";
    let server = MockServer::start().await;
    mount_tree(&server, json!([tree_entry("image.png", bytes)])).await;
    Mock::given(method("GET"))
        .and(path("/api/v1/repos/alice/r/raw/image.png"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(b"other bytes".to_vec()))
        .mount(&server)
        .await;
    let m = Machine::new(&server.uri());
    let target = m.dir().join("DIR");

    let out = m.run(&["pull", "alice/r", target.to_str().unwrap()]).await;

    assert_eq!(out.status.code(), Some(1), "{}", stderr(&out));
    assert!(
        stderr(&out).contains(&format!(
            "invalid response body: image.png: expected {}",
            blob_sha1(bytes)
        )),
        "{}",
        stderr(&out)
    );
    assert!(
        files_under(&target).is_empty(),
        "{:?}",
        files_under(&target)
    );
}

#[tokio::test(flavor = "multi_thread")]
#[serial]
async fn versioned_pull_checks_every_path_first() {
    let server = MockServer::start().await;
    mount_tree(
        &server,
        json!([
            tree_entry("a.md", b"a\n"),
            {"name": "x", "path": "../x", "type": "file", "size": 1, "sha": blob_sha1(b"x")}
        ]),
    )
    .await;
    mount_raw(&server, "a.md", b"a\n").await;
    let m = Machine::new(&server.uri());
    let target = m.dir().join("DIR");

    let out = m
        .run(&[
            "pull",
            "alice/r",
            target.to_str().unwrap(),
            "--version",
            "2",
        ])
        .await;

    assert_eq!(out.status.code(), Some(1), "{}", stderr(&out));
    assert!(
        stderr(&out).contains("refusing a server path breaking the file path constraint"),
        "{}",
        stderr(&out)
    );
    let requests = server.received_requests().await.unwrap();
    assert!(
        !requests.iter().any(|r| r.url.path().contains("/raw/")),
        "a raw read was sent"
    );
    assert!(
        files_under(&target).is_empty(),
        "{:?}",
        files_under(&target)
    );
}

// ---- convergence over a moving head -----------------------------------------

/// A working copy of the fake's `alice/proj` at `sha`, its tree written
/// into the machine's folder and recorded as the base.
fn checkout(fake: &Fake, m: &Machine, sha: &str) {
    let tree = fake.tree_bytes_at(sha);
    for (path, bytes) in &tree {
        write(&m.dir(), path, bytes);
    }
    m.copy()
        .record_base(
            sha,
            tree.iter()
                .map(|(p, b)| (p.clone(), blob_sha1(b)))
                .collect(),
        )
        .unwrap();
}

#[tokio::test(flavor = "multi_thread")]
#[serial]
async fn not_text_collision_is_not_marker_merged() {
    let fake = Fake::start().await;
    let m = Machine::new(&fake.uri);
    let base: &[u8] = b"\x89PNG\x00base";
    let local: &[u8] = b"\x89PNG\x00local";
    let head: &[u8] = b"\x89PNG\x00head";
    let h0 = fake.commit_bytes(&[(".syns.yaml", identity()), ("image.png", base)]);
    checkout(&fake, &m, &h0);
    write(&m.dir(), "image.png", local);
    fake.commit_byte_changes(&[("image.png", Some(head))]);

    let out = m.run(&["--json", "sync"]).await;

    assert_eq!(out.status.code(), Some(4), "{}", stderr(&out));
    assert_eq!(document(&out)["outcome"], json!("resolution_required"));
    assert_eq!(std::fs::read(m.dir().join("image.png")).unwrap(), local);
    let copy = m.copy();
    let remote = copy.remote_snapshot().unwrap();
    let head_side = copy
        .content_bytes(remote["image.png"].as_ref().unwrap())
        .unwrap();
    assert_eq!(head_side, head);
    assert!(!String::from_utf8_lossy(&head_side).contains("<<<<<<<"));
}

#[tokio::test(flavor = "multi_thread")]
#[serial]
async fn text_collision_still_merges() {
    let fake = Fake::start().await;
    let m = Machine::new(&fake.uri);
    let h0 = fake.commit_bytes(&[(".syns.yaml", identity()), ("a.md", b"a\nb\nc\n")]);
    checkout(&fake, &m, &h0);
    write(&m.dir(), "a.md", b"a\nLOCAL\nc\n");
    fake.commit_byte_changes(&[("a.md", Some(b"a\nHEAD\nc\n"))]);

    let out = m.run(&["--json", "sync"]).await;

    assert_eq!(out.status.code(), Some(4), "{}", stderr(&out));
    assert_eq!(document(&out)["outcome"], json!("resolution_required"));
    let merged = std::fs::read_to_string(m.dir().join("a.md")).unwrap();
    assert!(merged.contains("<<<<<<< local\nLOCAL\n"), "{merged}");
    assert!(merged.contains("HEAD\n>>>>>>> remote"), "{merged}");
}

#[tokio::test(flavor = "multi_thread")]
#[serial]
async fn a_full_budget_still_merges_text() {
    let fake = Fake::start().await;
    let m = Machine::new(&fake.uri);
    let big: Vec<(String, Vec<u8>)> = (0..4)
        .map(|i| {
            // A NUL at the head, so no side of these is text.
            let bytes: Vec<u8> = (0..20 * 1024 * 1024u32)
                .map(|j| {
                    if j == 0 {
                        0
                    } else {
                        (j as u8).wrapping_mul(13).wrapping_add(i as u8)
                    }
                })
                .collect();
            (format!("big{i}.bin"), bytes)
        })
        .collect();
    let mut files: Vec<(&str, &[u8])> = vec![(".syns.yaml", identity()), ("a.md", b"a\nb\nc\n")];
    files.extend(big.iter().map(|(p, b)| (p.as_str(), b.as_slice())));
    let h0 = fake.commit_bytes(&files);
    checkout(&fake, &m, &h0);
    write(&m.dir(), "a.md", b"a\nLOCAL\nc\n");
    fake.commit_byte_changes(&[("a.md", Some(b"a\nHEAD\nc\n"))]);

    let out = m.run(&["--json", "sync"]).await;

    assert_eq!(out.status.code(), Some(4), "{}", stderr(&out));
    assert_eq!(document(&out)["outcome"], json!("resolution_required"));
    let merged = std::fs::read_to_string(m.dir().join("a.md")).unwrap();
    assert!(merged.contains("<<<<<<< local\nLOCAL\n"), "{merged}");
    assert!(merged.contains("HEAD\n>>>>>>> remote"), "{merged}");
}

fn left_out_line() -> String {
    "could not write d: the folder there still holds d/node_modules/, which no retrieval removes \u{2014} move or remove them, then run again".to_string()
}

async fn left_out_folder(fake: &Fake, m: &Machine) {
    fake.commit_bytes(&[(".syns.yaml", identity()), ("d", b"file\n")]);
    write(&m.dir(), ".syns.yaml", identity());
    write(&m.dir(), "d/node_modules/p.js", b"p\n");
}

#[tokio::test(flavor = "multi_thread")]
#[serial]
async fn left_out_files_block_the_head_file() {
    let fake = Fake::start().await;
    let m = Machine::new(&fake.uri);
    left_out_folder(&fake, &m).await;

    let out = m.run(&["--json", "sync"]).await;

    assert_eq!(out.status.code(), Some(5), "{}", stderr(&out));
    let doc = document(&out);
    assert_eq!(doc["outcome"], json!("attention_required"));
    assert_eq!(doc["error"], json!(left_out_line()));
    assert_eq!(
        std::fs::read(m.dir().join("d/node_modules/p.js")).unwrap(),
        b"p\n"
    );
}

#[tokio::test(flavor = "multi_thread")]
#[serial]
async fn left_out_files_block_an_overwrite() {
    let fake = Fake::start().await;
    let m = Machine::new(&fake.uri);
    left_out_folder(&fake, &m).await;

    let out = m.run(&["pull", "--overwrite"]).await;

    assert_eq!(out.status.code(), Some(1), "{}", stderr(&out));
    assert!(
        stderr(&out).contains(&format!("error: {}", left_out_line())),
        "{}",
        stderr(&out)
    );
    assert_eq!(
        std::fs::read(m.dir().join("d/node_modules/p.js")).unwrap(),
        b"p\n"
    );
}

#[tokio::test(flavor = "multi_thread")]
#[serial]
async fn snapshots_store_content_by_hash_and_old_ones_load() {
    let six: &[u8] = &[0x89, 0x50, 0x4E, 0x47, 0x00, 0xFF];
    // The first: `image.png` changed at the head, a local edit keeping a
    // resolution standing.
    let fake = Fake::start().await;
    let first = Machine::new(&fake.uri);
    let h0 = fake.commit_bytes(&[
        (".syns.yaml", identity()),
        ("image.png", six),
        ("a.md", b"a\n"),
    ]);
    checkout(&fake, &first, &h0);
    write(&first.dir(), "a.md", b"a local\n");
    fake.commit_byte_changes(&[("image.png", Some(b"\x89PNG\x00head"))]);

    let out = first.run(&["--json", "sync"]).await;
    assert_eq!(out.status.code(), Some(4), "{}", stderr(&out));
    let copy = first.copy();
    let local = serde_json::to_value(copy.local_snapshot().unwrap()).unwrap();
    assert_eq!(local["image.png"], json!({"stored": blob_sha1(six)}));
    assert_eq!(
        std::fs::read(copy.snapshot_content_dir().join(blob_sha1(six))).unwrap(),
        six
    );

    // The second: a snapshot a released build wrote inline.
    let second = Machine::new(&fake.uri);
    write(&second.dir(), ".syns.yaml", identity());
    write(&second.dir(), "image.png", b"head bytes");
    write(&second.dir(), "a.md", b"candidate\n");
    let old = second.copy();
    std::fs::write(
        old.local_snapshot_path(),
        r#"{"image.png":[137,80,78,71,0,255],"a.md":"a\n"}"#,
    )
    .unwrap();
    old.write_resolution(&Resolution {
        recovery_id: "0123456789abcdef".into(),
        base_commit: None,
        head_commit: h0.clone(),
        round: 1,
        local_paths: vec!["a.md".into()],
        remote_paths: vec!["image.png".into()],
        collisions: vec![],
        combined_paths: vec!["a.md".into(), "image.png".into()],
        reviewed_tree: None,
        pending_writes: None,
    })
    .unwrap();

    for m in [&first, &second] {
        let out = m.run(&["resolution", "discard"]).await;
        assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
        assert_eq!(std::fs::read(m.dir().join("image.png")).unwrap(), six);
        assert!(!m.copy().snapshot_content_dir().exists());
    }
    assert_eq!(std::fs::read(second.dir().join("a.md")).unwrap(), b"a\n");
}

// ---- one collection per run, spared reads -------------------------------------

#[tokio::test(flavor = "multi_thread")]
#[serial]
async fn a_bare_push_collects_once() {
    let server = first_push_server().await;
    let m = Machine::new(&server.uri());
    write(&m.dir(), "a.md", b"a\n");
    write(&m.dir(), "x.log", b"log\n");
    write(&m.dir(), ".gitignore", b"x.log\n");

    let out = m.run(&["push", "--name", "proj", "--debug"]).await;

    assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
    let diagnostic = stderr(&out);
    assert_eq!(
        diagnostic
            .lines()
            .filter(|l| l.starts_with("[debug] skip x.log"))
            .count(),
        1,
        "{diagnostic}"
    );
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread")]
#[serial]
async fn a_refused_trusted_entry_is_read_again() {
    let fake = Fake::start().await;
    let m = Machine::new(&fake.uri);
    write(&m.dir(), "a.png", PNG);
    write(&m.dir(), "b.md", b"b\n");
    let first = m.run(&["push", "--name", "proj"]).await;
    assert_eq!(first.status.code(), Some(0), "{}", stderr(&first));
    let copy = m.copy();
    let mut record = copy.stat_record();
    let bogus = "0".repeat(40);
    record
        .entries
        .get_mut("a.png")
        .expect("a recorded a.png")
        .sha = bogus.clone();
    assert!(record.stamp.is_some());
    copy.write_stat_record(&record).unwrap();

    let out = m.run(&["push"]).await;

    assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
    let bodies = fake.push_bodies();
    let last = bodies.last().unwrap();
    assert_eq!(entry(last, "a.png")["sha"], json!(blob_sha1(PNG)));
    assert!(
        bodies.iter().all(|b| b["files"]
            .as_array()
            .unwrap()
            .iter()
            .all(|f| f["sha"] != json!(bogus))),
        "the untrusted hash was sent"
    );
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread")]
#[serial]
async fn a_restoring_tool_cannot_hide_a_same_size_rewrite() {
    let fake = Fake::start().await;
    let m = Machine::new(&fake.uri);
    let dir = m.dir();
    let original: Vec<u8> = PNG
        .iter()
        .copied()
        .chain(std::iter::repeat_n(7u8, 1024))
        .collect();
    write(&dir, "image.png", &original);
    let first = m.run(&["push", "--name", "proj"]).await;
    assert_eq!(first.status.code(), Some(0), "{}", stderr(&first));

    let scratch = tempfile::tempdir().unwrap();
    let tools: [(&str, u8); 3] = [("cp -p", 1), ("rsync -t", 2), ("tar -x", 3)];
    for (tool, seed) in tools {
        let replacement: Vec<u8> = PNG
            .iter()
            .copied()
            .chain(std::iter::repeat_n(seed + 100, 1024))
            .collect();
        assert_eq!(replacement.len(), original.len());
        let staged = scratch.path().join(format!("{seed}"));
        std::fs::create_dir_all(&staged).unwrap();
        let source = staged.join("image.png");
        std::fs::write(&source, &replacement).unwrap();
        let sh = |script: String| {
            let status = std::process::Command::new("sh")
                .arg("-c")
                .arg(&script)
                .status()
                .unwrap();
            assert!(status.success(), "{script}");
        };
        let target = dir.join("image.png");
        sh(format!(
            "touch -r '{}' '{}'",
            target.display(),
            source.display()
        ));
        match tool {
            "cp -p" => sh(format!(
                "cp -p '{}' '{}'",
                source.display(),
                target.display()
            )),
            // `-I`: rsync's own quick check skips a file whose size and
            // modification time already match, placing nothing at all.
            "rsync -t" => sh(format!(
                "rsync -t -I '{}' '{}'",
                source.display(),
                target.display()
            )),
            _ => {
                let archive = scratch.path().join(format!("{seed}.tar"));
                sh(format!(
                    "tar -cf '{}' -C '{}' image.png",
                    archive.display(),
                    staged.display()
                ));
                sh(format!(
                    "tar -xf '{}' -C '{}'",
                    archive.display(),
                    dir.display()
                ));
            }
        }

        assert_eq!(
            std::fs::read(&target).unwrap(),
            replacement,
            "{tool} placed nothing"
        );

        let out = m.run(&["push"]).await;

        assert_eq!(out.status.code(), Some(0), "{tool}: {}", stderr(&out));
        let bodies = fake.push_bodies();
        assert_eq!(
            entry(bodies.last().unwrap(), "image.png")["sha"],
            json!(blob_sha1(&replacement)),
            "{tool} hid the rewrite"
        );
    }
}

#[cfg(windows)]
#[tokio::test(flavor = "multi_thread")]
#[serial]
async fn convergence_on_windows_keeps_no_stat_record() {
    let fake = Fake::start().await;
    let m = Machine::new(&fake.uri);
    write(&m.dir(), "a.md", b"a\n");
    let first = m.run(&["push", "--name", "proj"]).await;
    assert_eq!(first.status.code(), Some(0), "{}", stderr(&first));
    write(&m.dir(), "a.md", b"a changed\n");

    let out = m.run(&["push"]).await;

    assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
    let bodies = fake.push_bodies();
    assert_eq!(
        entry(bodies.last().unwrap(), "a.md")["content"],
        json!("a changed\n")
    );
    assert!(!m.copy().state_dir.join("stat-record.json").exists());
}
