//! u256 — a working copy converges with the repository head through one
//! guarded, resumable reconciliation.
//!
//! The tests here drive a fake server of their own rather than wiremock:
//! a stateful repository answering `EP-tree`, `EP-file-read` and
//! `EP-push` the way the head moves under publications, guarding every
//! publication by the parent it names, and able to close a publication's
//! connection unanswered — which no wiremock responder can do.

use std::collections::{BTreeMap, HashMap};
use std::io::{BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use serde_json::{Value, json};
use serial_test::serial;
use syns_cli::client::{PushRequest, SynsClient};
use syns_cli::commands::push::{PushArgs, cmd_push};
use syns_cli::config::Config;
use syns_cli::errors::CliError;
use syns_cli::output::Output;
use syns_cli::push::collector::{CollectOptions, collect_files};
use syns_cli::push::converge::{
    ConvergeMode, ROUND_BOUND, SyncOutcome, WorkingCopyState, continue_resolution, converge,
    discard_resolution, working_copy_state,
};
use syns_cli::push::hash::blob_sha1;
use syns_cli::push::reconcile::CollisionKind;
use syns_cli::push::smart::SmartPushOptions;
use syns_cli::push::working_copy::{Outbox, Resolution, WorkingCopy};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

use crate::common::{TestContext, seed_credentials, setup};

const TOKEN: &str = "test-token";

// ---- a raw HTTP/1.1 listener ------------------------------------------

/// One request read off a raw connection: its method, target and body.
struct RawRequest {
    method: String,
    target: String,
    body: Vec<u8>,
}

/// Reads one HTTP/1.1 request, or `None` where the peer closed first.
async fn read_raw_request(sock: &mut TcpStream) -> Option<RawRequest> {
    let mut buf = Vec::new();
    let mut chunk = [0u8; 8192];
    let header_end = loop {
        if let Some(pos) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
            break pos + 4;
        }
        let n = sock.read(&mut chunk).await.ok()?;
        if n == 0 {
            return None;
        }
        buf.extend_from_slice(&chunk[..n]);
    };
    let head = String::from_utf8_lossy(&buf[..header_end]).to_string();
    let mut lines = head.split("\r\n");
    let mut request_line = lines.next()?.split(' ');
    let method = request_line.next()?.to_string();
    let target = request_line.next()?.to_string();
    let length = lines
        .filter_map(|l| l.split_once(':'))
        .find(|(k, _)| k.trim().eq_ignore_ascii_case("content-length"))
        .and_then(|(_, v)| v.trim().parse::<usize>().ok())
        .unwrap_or(0);
    let mut body = buf[header_end..].to_vec();
    while body.len() < length {
        let n = sock.read(&mut chunk).await.ok()?;
        if n == 0 {
            return None;
        }
        body.extend_from_slice(&chunk[..n]);
    }
    Some(RawRequest {
        method,
        target,
        body,
    })
}

/// A listener answering every `EP-tree` read on a kept-alive connection
/// and closing the connection unanswered on every `EP-push`.
async fn spawn_push_dropping_listener() -> String {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        loop {
            let Ok((mut sock, _)) = listener.accept().await else {
                return;
            };
            tokio::spawn(async move {
                while let Some(req) = read_raw_request(&mut sock).await {
                    if req.method == "PUT" {
                        return;
                    }
                    let body = r#"{"entries":[],"commitSha":"1111111111111111111111111111111111111111","truncated":false}"#;
                    let response = format!(
                        "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\n\r\n{body}",
                        body.len()
                    );
                    if sock.write_all(response.as_bytes()).await.is_err() {
                        return;
                    }
                }
            });
        }
    });
    format!("http://127.0.0.1:{}", addr.port())
}

fn empty_push_request() -> PushRequest {
    PushRequest {
        files: vec![],
        deletions: None,
        message: Some("push".into()),
        author: None,
        parent_sha: None,
        description: None,
        tags: None,
        status: None,
        visibility: None,
        provenance: None,
    }
}

/// A publication closed unanswered after its body left arrives as
/// `SERVER_UNREACHABLE`, on a fresh connection and on a kept-alive one
/// alike, although reqwest reports it as neither a connect nor a timeout
/// failure.
#[tokio::test(flavor = "multi_thread")]
async fn dropped_push_connection_classifies_as_server_unreachable() {
    let uri = spawn_push_dropping_listener().await;

    let bare = reqwest::Client::new()
        .put(format!("{uri}/api/v1/repos/alice/proj/push"))
        .body("{}")
        .send()
        .await
        .expect_err("the listener answers no publication");
    println!(
        "bare reqwest: is_connect={} is_timeout={} is_request={} error={bare:?}",
        bare.is_connect(),
        bare.is_timeout(),
        bare.is_request()
    );

    let fresh = SynsClient::new(&uri).unwrap();
    let fresh_err = fresh
        .push("alice/proj", "t", &empty_push_request())
        .await
        .expect_err("fresh connection");
    println!("fresh connection: {fresh_err:?}");

    let kept = SynsClient::new(&uri).unwrap();
    kept.get_tree("alice/proj", Some("t"), None, true, None)
        .await
        .expect("the tree read answers");
    let kept_err = kept
        .push("alice/proj", "t", &empty_push_request())
        .await
        .expect_err("kept-alive connection");
    println!("kept-alive connection: {kept_err:?}");

    assert!(!bare.is_connect() && !bare.is_timeout() && bare.is_request());
    assert!(
        matches!(fresh_err, CliError::ServerUnreachable { .. }),
        "fresh: {fresh_err:?}"
    );
    assert!(
        matches!(kept_err, CliError::ServerUnreachable { .. }),
        "kept-alive: {kept_err:?}"
    );
}

// ---- a stateful fake repository ----------------------------------------

#[derive(Default)]
struct FakeRepo {
    /// Every commit oldest first: its sha and its whole tree, path to content.
    commits: Vec<(String, BTreeMap<String, String>)>,
    /// Every request received: method, target and body.
    requests: Vec<(String, String, Vec<u8>)>,
    drop_next_push: bool,
}

fn error_body(code: &str) -> String {
    json!({"error": code, "message": code}).to_string()
}

impl FakeRepo {
    fn answer(&mut self, req: &RawRequest) -> Option<(u16, String)> {
        self.requests
            .push((req.method.clone(), req.target.clone(), req.body.clone()));
        let (route, query) = req.target.split_once('?').unwrap_or((&req.target, ""));
        let params: HashMap<String, String> = query
            .split('&')
            .filter_map(|kv| kv.split_once('='))
            .map(|(k, v)| (k.to_string(), urlencoding::decode(v).unwrap().to_string()))
            .collect();
        let tail = route
            .strip_prefix("/api/v1/repos/")
            .and_then(|rest| rest.splitn(3, '/').nth(2))
            .unwrap_or("");
        match (req.method.as_str(), tail) {
            ("GET", "tree") => Some(self.tree(params.get("ref"))),
            ("GET", files) if files.starts_with("files/") => {
                let path = urlencoding::decode(&files["files/".len()..])
                    .unwrap()
                    .to_string();
                Some(self.file(&path, params.get("ref")))
            }
            ("PUT", "push") => {
                if self.drop_next_push {
                    self.drop_next_push = false;
                    return None;
                }
                Some(self.push(&req.body))
            }
            _ => Some((404, error_body("not_found"))),
        }
    }

    fn find(&self, reference: Option<&String>) -> Option<&(String, BTreeMap<String, String>)> {
        match reference {
            Some(sha) => self.commits.iter().find(|(s, _)| s == sha),
            None => self.commits.last(),
        }
    }

    fn tree(&self, reference: Option<&String>) -> (u16, String) {
        if self.commits.is_empty() {
            return (422, error_body("validation_error"));
        }
        let Some((sha, files)) = self.find(reference) else {
            return (404, error_body("ref_not_found"));
        };
        let entries: Vec<Value> = files
            .iter()
            .map(|(path, content)| {
                json!({
                    "name": path.rsplit('/').next().unwrap(),
                    "path": path,
                    "type": "file",
                    "size": content.len(),
                    "sha": blob_sha1(content.as_bytes()),
                })
            })
            .collect();
        (
            200,
            json!({"entries": entries, "commitSha": sha, "truncated": false}).to_string(),
        )
    }

    fn file(&self, path: &str, reference: Option<&String>) -> (u16, String) {
        match self.find(reference).and_then(|(_, files)| files.get(path)) {
            Some(content) => (
                200,
                json!({"content": content, "sha": blob_sha1(content.as_bytes()), "size": content.len()})
                    .to_string(),
            ),
            None => (404, error_body("not_found")),
        }
    }

    fn push(&mut self, body: &[u8]) -> (u16, String) {
        let body: Value = serde_json::from_slice(body).unwrap();
        let head = self.commits.last().cloned();
        if let Some(parent) = body.get("parentSha").and_then(Value::as_str)
            && head.as_ref().map(|(sha, _)| sha.as_str()) != Some(parent)
        {
            return (409, error_body("conflict"));
        }
        let known: HashMap<String, String> = self
            .commits
            .iter()
            .flat_map(|(_, files)| files.values())
            .map(|content| (blob_sha1(content.as_bytes()), content.clone()))
            .collect();
        let mut files = head.map(|(_, files)| files).unwrap_or_default();
        for entry in body["files"].as_array().unwrap() {
            let path = entry["path"].as_str().unwrap().to_string();
            match entry.get("content").and_then(Value::as_str) {
                Some(content) => {
                    files.insert(path, content.to_string());
                }
                None => match known.get(entry["sha"].as_str().unwrap()) {
                    Some(content) => {
                        files.insert(path, content.clone());
                    }
                    None => return (409, error_body("missing_blobs")),
                },
            }
        }
        for deletion in body
            .get("deletions")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            files.remove(deletion["path"].as_str().unwrap());
        }
        let sha = self.add_commit(files);
        (
            200,
            json!({
                "commitSha": sha,
                "version": self.commits.len(),
                "filesChanged": 1,
                "created": self.commits.len() == 1,
            })
            .to_string(),
        )
    }

    fn add_commit(&mut self, files: BTreeMap<String, String>) -> String {
        let sha = blob_sha1(format!("commit {} {files:?}", self.commits.len()).as_bytes());
        self.commits.push((sha.clone(), files));
        sha
    }
}

#[derive(Clone)]
struct Fake {
    uri: String,
    repo: Arc<Mutex<FakeRepo>>,
}

impl Fake {
    async fn start() -> Fake {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let uri = format!("http://127.0.0.1:{}", listener.local_addr().unwrap().port());
        let repo = Arc::new(Mutex::new(FakeRepo::default()));
        let shared = repo.clone();
        tokio::spawn(async move {
            while let Ok((mut sock, _)) = listener.accept().await {
                let shared = shared.clone();
                tokio::spawn(async move {
                    while let Some(req) = read_raw_request(&mut sock).await {
                        let answer = shared.lock().unwrap().answer(&req);
                        let Some((status, body)) = answer else {
                            return;
                        };
                        let response = format!(
                            "HTTP/1.1 {status} FAKE\r\ncontent-type: application/json\r\ncontent-length: {}\r\n\r\n{body}",
                            body.len()
                        );
                        if sock.write_all(response.as_bytes()).await.is_err() {
                            return;
                        }
                    }
                });
            }
        });
        Fake { uri, repo }
    }

    fn client(&self) -> SynsClient {
        SynsClient::new(&self.uri).unwrap()
    }

    /// A commit holding exactly `files`.
    fn commit(&self, files: &[(&str, &str)]) -> String {
        let tree = files
            .iter()
            .map(|(p, c)| (p.to_string(), c.to_string()))
            .collect();
        self.repo.lock().unwrap().add_commit(tree)
    }

    /// A commit laying `changes` over the head, `None` removing a path.
    fn commit_changes(&self, changes: &[(&str, Option<&str>)]) -> String {
        let mut repo = self.repo.lock().unwrap();
        let mut tree = repo
            .commits
            .last()
            .map(|(_, f)| f.clone())
            .unwrap_or_default();
        for (path, content) in changes {
            match content {
                Some(content) => tree.insert(path.to_string(), content.to_string()),
                None => tree.remove(*path),
            };
        }
        repo.add_commit(tree)
    }

    fn head(&self) -> (String, BTreeMap<String, String>) {
        self.repo.lock().unwrap().commits.last().cloned().unwrap()
    }

    fn tree_at(&self, sha: &str) -> BTreeMap<String, String> {
        let repo = self.repo.lock().unwrap();
        repo.commits
            .iter()
            .find(|(s, _)| s == sha)
            .map(|(_, f)| f.clone())
            .unwrap()
    }

    fn push_bodies(&self) -> Vec<Value> {
        self.repo
            .lock()
            .unwrap()
            .requests
            .iter()
            .filter(|(method, _, _)| method == "PUT")
            .map(|(_, _, body)| serde_json::from_slice(body).unwrap())
            .collect()
    }

    fn drop_next_push(&self) {
        self.repo.lock().unwrap().drop_next_push = true;
    }
}

// ---- helpers ------------------------------------------------------------

struct Env {
    ctx: TestContext,
    fake: Fake,
    config: Config,
    output: Output,
}

async fn env() -> Env {
    let ctx = setup().await;
    seed_credentials(&ctx, TOKEN, "alice");
    let fake = Fake::start().await;
    let config = Config::new(Some(&fake.uri)).unwrap();
    Env {
        ctx,
        fake,
        config,
        output: Output::new(false),
    }
}

impl Env {
    fn dir(&self) -> PathBuf {
        self.ctx.project_dir.path().to_path_buf()
    }

    fn copy(&self, dir: &Path) -> WorkingCopy {
        WorkingCopy::open(self.config.cache_dir(), "alice", "proj", dir).unwrap()
    }

    fn opts(&self) -> SmartPushOptions {
        opts_at(self.config.cache_dir())
    }
}

fn opts_at(cache_dir: &Path) -> SmartPushOptions {
    SmartPushOptions {
        force: false,
        message: "push".into(),
        author: None,
        parent_sha: None,
        excludes: vec![],
        cache_dir: cache_dir.to_path_buf(),
        description: None,
        tags: None,
        status: None,
        visibility: None,
        strict: false,
        allow_empty: false,
        debug: false,
        no_default_excludes: false,
        prefix: None,
        reference: None,
        expected: None,
        provenance: None,
    }
}

fn write_files(dir: &Path, files: &[(&str, &str)]) {
    for (path, content) in files {
        let target = dir.join(path);
        std::fs::create_dir_all(target.parent().unwrap()).unwrap();
        std::fs::write(target, content).unwrap();
    }
}

fn read(dir: &Path, path: &str) -> String {
    std::fs::read_to_string(dir.join(path)).unwrap()
}

fn hashes(tree: &BTreeMap<String, String>) -> HashMap<String, String> {
    tree.iter()
        .map(|(p, c)| (p.clone(), blob_sha1(c.as_bytes())))
        .collect()
}

/// Write the tree of commit `sha` into the working copy and record it as
/// the base.
fn checkout(fake: &Fake, copy: &WorkingCopy, sha: &str) {
    let tree = fake.tree_at(sha);
    for (path, content) in &tree {
        write_files(&copy.root, &[(path.as_str(), content.as_str())]);
    }
    copy.record_base(sha, hashes(&tree)).unwrap();
}

fn folder_hashes(dir: &Path) -> BTreeMap<String, String> {
    collect_files(dir, &[], CollectOptions::default())
        .unwrap()
        .files
        .iter()
        .map(|(p, b)| (p.clone(), blob_sha1(b)))
        .collect()
}

fn body_content<'a>(body: &'a Value, path: &str) -> Option<&'a str> {
    body["files"]
        .as_array()
        .unwrap()
        .iter()
        .find(|f| f["path"] == path)
        .and_then(|f| f.get("content"))
        .and_then(Value::as_str)
}

fn expect_resolution(outcome: SyncOutcome) -> Resolution {
    match outcome {
        SyncOutcome::ResolutionRequired(resolution) => resolution,
        other => panic!("expected ResolutionRequired, got {other:?}"),
    }
}

fn dummy_resolution(head: &str) -> Resolution {
    Resolution {
        recovery_id: "rec".into(),
        base_commit: None,
        head_commit: head.into(),
        round: 1,
        local_paths: vec![],
        remote_paths: vec![],
        collisions: vec![],
        combined_paths: vec![],
        reviewed_tree: None,
        pending_writes: None,
    }
}

struct CwdGuard(PathBuf);

impl CwdGuard {
    fn enter(dir: &Path) -> Self {
        let previous = std::env::current_dir().expect("cwd");
        std::env::set_current_dir(dir).expect("set cwd");
        CwdGuard(previous)
    }
}

impl Drop for CwdGuard {
    fn drop(&mut self) {
        let _ = std::env::set_current_dir(&self.0);
    }
}

fn push_args(path: Option<PathBuf>) -> PushArgs {
    PushArgs {
        name: None,
        path,
        message: None,
        force: false,
        exclude: vec![],
        description: None,
        tag: vec![],
        status: None,
        visibility: None,
        if_repo: false,
        strict: false,
        allow_empty: false,
        debug: false,
        no_default_excludes: false,
    }
}

const IDENTITY: &str = "owner: alice\nname: proj\n";

// ---- retrieval ------------------------------------------------------------

#[tokio::test(flavor = "current_thread")]
#[serial]
async fn retrieval_without_local_edits_takes_the_moved_head() {
    let e = env().await;
    let dir = e.dir();
    let copy = e.copy(&dir);
    let h0 = e.fake.commit(&[("a.md", "a0\n")]);
    checkout(&e.fake, &copy, &h0);
    let h1 = e.fake.commit(&[("a.md", "a1\n"), ("b.md", "b1\n")]);
    let client = e.fake.client();

    let outcome = converge(
        &client,
        Some(TOKEN),
        &copy,
        ConvergeMode::Retrieve { overwrite: false },
        e.opts(),
    )
    .await
    .unwrap();

    assert!(matches!(outcome, SyncOutcome::Synced { .. }), "{outcome:?}");
    assert_eq!(read(&dir, "a.md"), "a1\n");
    assert_eq!(read(&dir, "b.md"), "b1\n");
    assert_eq!(copy.base().unwrap().commit_sha(), Some(h1.as_str()));
    assert_eq!(
        working_copy_state(&client, Some(TOKEN), &copy)
            .await
            .unwrap(),
        WorkingCopyState::Converged
    );
}

#[tokio::test(flavor = "current_thread")]
#[serial]
async fn retrieval_keeps_local_edits_beside_remote_only_paths() {
    let e = env().await;
    let dir = e.dir();
    let copy = e.copy(&dir);
    let h0 = e.fake.commit(&[("local.md", "l0\n")]);
    checkout(&e.fake, &copy, &h0);
    write_files(&dir, &[("local.md", "edited\n")]);
    let h1 = e.fake.commit_changes(&[("remote.md", Some("r1\n"))]);

    let outcome = converge(
        &e.fake.client(),
        Some(TOKEN),
        &copy,
        ConvergeMode::Retrieve { overwrite: false },
        e.opts(),
    )
    .await
    .unwrap();

    assert!(matches!(outcome, SyncOutcome::Synced { .. }), "{outcome:?}");
    assert_eq!(read(&dir, "local.md"), "edited\n");
    assert_eq!(read(&dir, "remote.md"), "r1\n");
    assert!(e.fake.push_bodies().is_empty());
    assert_eq!(copy.base().unwrap().commit_sha(), Some(h1.as_str()));
}

#[tokio::test(flavor = "current_thread")]
#[serial]
async fn retrieval_with_lost_base_prepares_every_differing_path() {
    let e = env().await;
    let dir = e.dir();
    let copy = e.copy(&dir);
    e.fake
        .commit(&[("a.md", "remote a\n"), ("same.md", "same\n")]);
    write_files(&dir, &[("a.md", "local a\n"), ("same.md", "same\n")]);

    let resolution = expect_resolution(
        converge(
            &e.fake.client(),
            Some(TOKEN),
            &copy,
            ConvergeMode::Retrieve { overwrite: false },
            e.opts(),
        )
        .await
        .unwrap(),
    );

    assert_eq!(
        resolution.collisions,
        vec![("a.md".to_string(), CollisionKind::AddAdd)]
    );
    assert!(!resolution.collisions.iter().any(|(p, _)| p == "same.md"));
    assert!(read(&dir, "a.md").lines().any(|l| l == "<<<<<<< local"));
    let snapshot = copy.local_snapshot().unwrap();
    assert_eq!(
        snapshot["a.md"].clone().unwrap().into_bytes(),
        b"local a\n".to_vec()
    );
}

// ---- publication ------------------------------------------------------------

#[tokio::test(flavor = "current_thread")]
#[serial]
async fn publication_past_a_moved_head_requires_resolution_for_every_change_shape() {
    let e = env().await;
    type Shape = (
        &'static str,
        &'static [(&'static str, &'static str)],
        fn(&Path),
        &'static [(&'static str, &'static str)],
    );
    let shapes: [Shape; 5] = [
        (
            "modify/modify",
            &[("x.md", "0\n")],
            |d| write_files(d, &[("x.md", "L\n")]),
            &[("x.md", "R\n")],
        ),
        (
            "add/add",
            &[("k.md", "k\n")],
            |d| write_files(d, &[("n.md", "L\n")]),
            &[("k.md", "k\n"), ("n.md", "R\n")],
        ),
        (
            "modify/delete",
            &[("x.md", "0\n"), ("k.md", "k\n")],
            |d| write_files(d, &[("x.md", "L\n")]),
            &[("k.md", "k\n")],
        ),
        (
            "delete/modify",
            &[("x.md", "0\n"), ("k.md", "k\n")],
            |d| std::fs::remove_file(d.join("x.md")).unwrap(),
            &[("x.md", "R\n"), ("k.md", "k\n")],
        ),
        (
            "disjoint",
            &[("x.md", "0\n"), ("y.md", "0\n")],
            |d| write_files(d, &[("x.md", "L\n")]),
            &[("x.md", "0\n"), ("y.md", "R\n")],
        ),
    ];

    for (label, h0_tree, edit, h1_tree) in shapes {
        let fake = Fake::start().await;
        let folder = tempfile::tempdir().unwrap();
        let copy = e.copy(folder.path());
        let h0 = fake.commit(h0_tree);
        checkout(&fake, &copy, &h0);
        edit(folder.path());
        let h1 = fake.commit(h1_tree);

        let outcome = converge(
            &fake.client(),
            Some(TOKEN),
            &copy,
            ConvergeMode::Publish,
            e.opts(),
        )
        .await
        .unwrap();

        let resolution = match outcome {
            SyncOutcome::ResolutionRequired(r) => r,
            other => panic!("{label}: expected ResolutionRequired, got {other:?}"),
        };
        assert_eq!(
            resolution.base_commit.as_deref(),
            Some(h0.as_str()),
            "{label}"
        );
        assert_eq!(resolution.head_commit, h1, "{label}");
        assert!(
            fake.push_bodies().is_empty(),
            "{label}: a publication was sent"
        );
    }
}

/// A resolution on H1 over one collision, `a.md` edited on both sides.
async fn collided(e: &Env, dir: &Path, copy: &WorkingCopy) -> (String, Resolution) {
    let h0 = e.fake.commit(&[("a.md", "a\nb\nc\n"), ("z.md", "z0\n")]);
    checkout(&e.fake, copy, &h0);
    write_files(dir, &[("a.md", "a\nL\nc\n")]);
    let h1 = e.fake.commit_changes(&[("a.md", Some("a\nR\nc\n"))]);
    let resolution = expect_resolution(
        converge(
            &e.fake.client(),
            Some(TOKEN),
            copy,
            ConvergeMode::Publish,
            e.opts(),
        )
        .await
        .unwrap(),
    );
    (h1, resolution)
}

#[tokio::test(flavor = "current_thread")]
#[serial]
async fn continued_resolution_publishes_the_reviewed_folder() {
    let e = env().await;
    let dir = e.dir();
    let copy = e.copy(&dir);
    let (h1, _resolution) = collided(&e, &dir, &copy).await;
    write_files(&dir, &[("a.md", "a\nL and R\nc\n")]);

    let outcome = continue_resolution(&e.fake.client(), TOKEN, &copy, e.opts())
        .await
        .unwrap();

    let returned = match outcome {
        SyncOutcome::Synced {
            published: Some((response, _, _)),
            ..
        } => response.commit_sha,
        other => panic!("expected a publication, got {other:?}"),
    };
    let bodies = e.fake.push_bodies();
    assert_eq!(bodies.len(), 1);
    assert_eq!(bodies[0]["parentSha"], h1.as_str());
    assert!(bodies[0].get("author").is_none(), "{}", bodies[0]);
    assert_eq!(copy.base().unwrap().commit_sha(), Some(returned.as_str()));
    assert_eq!(e.fake.head().1["a.md"], "a\nL and R\nc\n");
    assert!(copy.resolution().unwrap().is_none());
    assert!(copy.outbox().unwrap().is_none());
    assert!(!copy.local_snapshot_path().exists());
    assert!(!copy.remote_snapshot_path().exists());
}

#[tokio::test(flavor = "current_thread")]
#[serial]
async fn marker_in_a_collision_path_blocks_publication() {
    let e = env().await;
    let dir = e.dir();
    let copy = e.copy(&dir);
    collided(&e, &dir, &copy).await;
    assert!(read(&dir, "a.md").contains("<<<<<<< local"));

    let outcome = continue_resolution(&e.fake.client(), TOKEN, &copy, e.opts())
        .await
        .unwrap();

    expect_resolution(outcome);
    assert!(e.fake.push_bodies().is_empty());
}

#[tokio::test(flavor = "current_thread")]
#[serial]
async fn publication_carries_every_writer_edit_in_the_folder() {
    let e = env().await;
    let dir = e.dir();
    let copy = e.copy(&dir);
    let h0 = e.fake.commit(&[("a.md", "a0\n"), ("b.md", "b0\n")]);
    checkout(&e.fake, &copy, &h0);
    write_files(&dir, &[("a.md", "a1\n")]);
    let other_writer = dir.clone();
    std::thread::spawn(move || write_files(&other_writer, &[("b.md", "b1\n")]))
        .join()
        .unwrap();

    let first = converge(
        &e.fake.client(),
        Some(TOKEN),
        &copy,
        ConvergeMode::Publish,
        e.opts(),
    )
    .await
    .unwrap();
    let h1 = match first {
        SyncOutcome::Synced {
            published: Some((response, _, _)),
            ..
        } => response.commit_sha,
        other => panic!("expected a publication, got {other:?}"),
    };
    write_files(&dir, &[("c.md", "c1\n")]);
    converge(
        &e.fake.client(),
        Some(TOKEN),
        &copy,
        ConvergeMode::Publish,
        e.opts(),
    )
    .await
    .unwrap();

    let bodies = e.fake.push_bodies();
    assert_eq!(bodies.len(), 2);
    assert_eq!(body_content(&bodies[0], "a.md"), Some("a1\n"));
    assert_eq!(body_content(&bodies[0], "b.md"), Some("b1\n"));
    assert_eq!(bodies[0]["parentSha"], h0.as_str());
    assert_eq!(body_content(&bodies[1], "c.md"), Some("c1\n"));
    assert_eq!(body_content(&bodies[1], "a.md"), None);
    assert_eq!(body_content(&bodies[1], "b.md"), None);
    assert_eq!(bodies[1]["parentSha"], h1.as_str());
}

#[tokio::test(flavor = "current_thread")]
#[serial]
async fn publication_without_local_changes_takes_the_moved_head() {
    let e = env().await;
    let dir = e.dir();
    let copy = e.copy(&dir);
    let h0 = e.fake.commit(&[("a.md", "a0\n")]);
    checkout(&e.fake, &copy, &h0);
    e.fake.commit_changes(&[("b.md", Some("b1\n"))]);

    let outcome = converge(
        &e.fake.client(),
        Some(TOKEN),
        &copy,
        ConvergeMode::Publish,
        e.opts(),
    )
    .await
    .unwrap();

    assert!(
        matches!(
            outcome,
            SyncOutcome::Synced {
                published: None,
                ..
            }
        ),
        "{outcome:?}"
    );
    assert_eq!(read(&dir, "b.md"), "b1\n");
    assert!(copy.resolution().unwrap().is_none());
    assert!(e.fake.push_bodies().is_empty());
}

#[tokio::test(flavor = "current_thread")]
#[serial]
async fn bare_push_past_a_moved_head_converges_hand_edits() {
    let e = env().await;
    let dir = e.dir();
    let copy = e.copy(&dir);
    let h0 = e
        .fake
        .commit(&[(".syns.yaml", IDENTITY), ("x.md", "x0\n"), ("y.md", "y0\n")]);
    checkout(&e.fake, &copy, &h0);
    write_files(&dir, &[("y.md", "y by hand\n")]);
    e.fake.commit_changes(&[("x.md", Some("x1\n"))]);

    let refused = {
        let _cwd = CwdGuard::enter(&dir);
        cmd_push(&e.config, &e.output, &push_args(None)).await
    };
    assert!(
        matches!(refused, Err(CliError::SyncRefusal { exit: 4, .. })),
        "{refused:?}"
    );
    assert!(e.fake.push_bodies().is_empty());

    let outcome = continue_resolution(&e.fake.client(), TOKEN, &copy, e.opts())
        .await
        .unwrap();

    assert!(
        matches!(
            outcome,
            SyncOutcome::Synced {
                published: Some(_),
                ..
            }
        ),
        "{outcome:?}"
    );
    let (_, published) = e.fake.head();
    assert_eq!(published["x.md"], "x1\n");
    assert_eq!(published["y.md"], "y by hand\n");
}

#[tokio::test(flavor = "current_thread")]
#[serial]
async fn killed_resolution_resumes_under_its_recovery_id() {
    let e = env().await;
    let dir = e.dir();
    let copy = e.copy(&dir);
    let (_h1, earlier) = collided(&e, &dir, &copy).await;

    let outcome = converge(
        &e.fake.client(),
        Some(TOKEN),
        &copy,
        ConvergeMode::Publish,
        e.opts(),
    )
    .await
    .unwrap();

    let resumed = expect_resolution(outcome);
    assert_eq!(resumed.recovery_id, earlier.recovery_id);
    assert_eq!(
        copy.local_snapshot().unwrap()["a.md"]
            .clone()
            .unwrap()
            .into_bytes(),
        b"a\nL\nc\n".to_vec()
    );
}

#[tokio::test(flavor = "current_thread")]
#[serial]
async fn interrupted_publication_reads_as_pending_then_completes() {
    let e = env().await;
    let dir = e.dir();
    let copy = e.copy(&dir);
    let h0 = e.fake.commit(&[("a.md", "a0\n")]);
    checkout(&e.fake, &copy, &h0);
    write_files(&dir, &[("a.md", "a1\n")]);
    e.fake.drop_next_push();
    let client = e.fake.client();

    let dropped = converge(&client, Some(TOKEN), &copy, ConvergeMode::Publish, e.opts()).await;
    assert!(
        matches!(dropped, Err(CliError::ServerUnreachable { .. })),
        "{dropped:?}"
    );
    assert_eq!(
        working_copy_state(&client, Some(TOKEN), &copy)
            .await
            .unwrap(),
        WorkingCopyState::PublicationPending
    );

    let outcome = converge(&client, Some(TOKEN), &copy, ConvergeMode::Publish, e.opts())
        .await
        .unwrap();

    assert!(
        matches!(
            outcome,
            SyncOutcome::Synced {
                published: Some(_),
                ..
            }
        ),
        "{outcome:?}"
    );
    let (head, tree) = e.fake.head();
    assert_ne!(head, h0);
    assert_eq!(tree["a.md"], "a1\n");
    assert_eq!(copy.base().unwrap().commit_sha(), Some(head.as_str()));
    assert!(copy.outbox().unwrap().is_none());
}

#[tokio::test(flavor = "current_thread")]
#[serial]
async fn acknowledged_publication_completes_without_a_second_commit() {
    let e = env().await;
    let dir = e.dir();
    let copy = e.copy(&dir);
    let h0 = e.fake.commit(&[("a.md", "a0\n")]);
    checkout(&e.fake, &copy, &h0);
    let h1 = e.fake.commit(&[("a.md", "a1\n")]);
    write_files(&dir, &[("a.md", "a1\n")]);
    copy.write_outbox(&Outbox {
        parent_commit: Some(h0.clone()),
        tree: hashes(&e.fake.tree_at(&h1)).into_iter().collect(),
    })
    .unwrap();

    converge(
        &e.fake.client(),
        Some(TOKEN),
        &copy,
        ConvergeMode::Publish,
        e.opts(),
    )
    .await
    .unwrap();

    assert!(e.fake.push_bodies().is_empty());
    assert_eq!(copy.base().unwrap().commit_sha(), Some(h1.as_str()));
    assert!(copy.outbox().unwrap().is_none());
}

#[tokio::test(flavor = "current_thread")]
#[serial]
async fn guard_refusal_recomputes_against_the_newest_head() {
    let e = env().await;
    let dir = e.dir();
    let copy = e.copy(&dir);
    let (_h1, earlier) = collided(&e, &dir, &copy).await;
    write_files(&dir, &[("a.md", "a\nL and R\nc\n")]);
    let h2 = e.fake.commit_changes(&[("z.md", Some("z2\n"))]);

    let outcome = continue_resolution(&e.fake.client(), TOKEN, &copy, e.opts())
        .await
        .unwrap();

    let recomputed = expect_resolution(outcome);
    assert_eq!(recomputed.head_commit, h2);
    assert_eq!(recomputed.round, 2);
    assert_eq!(recomputed.recovery_id, earlier.recovery_id);
    assert!(recomputed.reviewed_tree.is_none());
    assert_eq!(read(&dir, "z.md"), "z2\n");
}

#[tokio::test(flavor = "current_thread")]
#[serial]
async fn write_after_the_continue_puts_up_a_fresh_candidate() {
    let e = env().await;
    let dir = e.dir();
    let copy = e.copy(&dir);
    let (_h1, mut resolution) = collided(&e, &dir, &copy).await;
    write_files(&dir, &[("a.md", "a\nL and R\nc\n")]);
    resolution.reviewed_tree = Some(folder_hashes(&dir));
    copy.write_resolution(&resolution).unwrap();
    write_files(&dir, &[("late.md", "late\n")]);

    let outcome = converge(
        &e.fake.client(),
        Some(TOKEN),
        &copy,
        ConvergeMode::Publish,
        e.opts(),
    )
    .await
    .unwrap();

    let fresh = expect_resolution(outcome);
    assert!(
        fresh.local_paths.contains(&"late.md".to_string()),
        "{fresh:?}"
    );
    assert!(
        fresh.combined_paths.contains(&"late.md".to_string()),
        "{fresh:?}"
    );
    assert!(fresh.reviewed_tree.is_none());
    assert_eq!(read(&dir, "late.md"), "late\n");
    assert!(e.fake.push_bodies().is_empty());
}

#[tokio::test(flavor = "current_thread")]
#[serial]
async fn forced_or_scoped_push_refused_while_a_resolution_stands() {
    let e = env().await;
    let dir = e.dir();
    write_files(&dir, &[(".syns.yaml", IDENTITY), ("sub/a.md", "a\n")]);
    let copy = e.copy(&dir);
    copy.write_resolution(&dummy_resolution("h1")).unwrap();

    let _cwd = CwdGuard::enter(&dir);
    let mut forced = push_args(None);
    forced.force = true;
    let forced = cmd_push(&e.config, &e.output, &forced).await;
    let scoped = cmd_push(&e.config, &e.output, &push_args(Some(dir.join("sub")))).await;

    assert!(
        matches!(forced, Err(CliError::SyncRefusal { exit: 4, .. })),
        "{forced:?}"
    );
    assert!(
        matches!(scoped, Err(CliError::SyncRefusal { exit: 4, .. })),
        "{scoped:?}"
    );
    assert!(e.fake.push_bodies().is_empty());
}

#[tokio::test(flavor = "current_thread")]
#[serial]
async fn scoped_push_lays_its_published_paths_over_the_base() {
    let e = env().await;
    let dir = e.dir();
    let copy = e.copy(&dir);
    let h0 = e.fake.commit(&[
        (".syns.yaml", IDENTITY),
        ("a.md", "a0\n"),
        ("sub/x.md", "x0\n"),
    ]);
    checkout(&e.fake, &copy, &h0);
    write_files(&dir, &[("a.md", "a1\n"), ("sub/x.md", "x1\n")]);

    {
        let _cwd = CwdGuard::enter(&dir);
        cmd_push(&e.config, &e.output, &push_args(Some(dir.join("sub"))))
            .await
            .unwrap();
    }
    let (h1, _) = e.fake.head();
    let base = copy.base().unwrap();
    assert_eq!(base.commit_sha(), Some(h1.as_str()));
    assert_eq!(base.file_sha("sub/x.md"), Some(blob_sha1(b"x1\n").as_str()));
    assert_eq!(base.file_sha("a.md"), Some(blob_sha1(b"a0\n").as_str()));

    converge(
        &e.fake.client(),
        Some(TOKEN),
        &copy,
        ConvergeMode::Publish,
        e.opts(),
    )
    .await
    .unwrap();

    let bodies = e.fake.push_bodies();
    assert_eq!(bodies.len(), 2);
    assert_eq!(body_content(&bodies[1], "a.md"), Some("a1\n"));
    assert_eq!(body_content(&bodies[1], "sub/x.md"), None);
    assert_eq!(bodies[1]["parentSha"], h1.as_str());
}

// ---- state, discard, atomic writes ------------------------------------------

#[tokio::test(flavor = "current_thread")]
#[serial]
async fn status_names_each_working_copy_state() {
    let e = env().await;
    let h0 = e.fake.commit(&[("a.md", "a0\n")]);
    let h1 = e.fake.commit(&[("a.md", "a1\n")]);
    let client = e.fake.client();

    let folders: Vec<tempfile::TempDir> = (0..6).map(|_| tempfile::tempdir().unwrap()).collect();
    let copies: Vec<WorkingCopy> = folders.iter().map(|f| e.copy(f.path())).collect();

    checkout(&e.fake, &copies[0], &h1);
    checkout(&e.fake, &copies[1], &h1);
    write_files(folders[1].path(), &[("a.md", "edited\n")]);
    checkout(&e.fake, &copies[2], &h0);
    checkout(&e.fake, &copies[3], &h0);
    write_files(folders[3].path(), &[("a.md", "edited\n")]);
    checkout(&e.fake, &copies[4], &h0);
    copies[4].write_resolution(&dummy_resolution(&h1)).unwrap();
    checkout(&e.fake, &copies[5], &h0);
    copies[5]
        .write_outbox(&Outbox {
            parent_commit: Some(h0.clone()),
            tree: BTreeMap::new(),
        })
        .unwrap();

    let mut states = Vec::new();
    for copy in &copies {
        states.push(
            working_copy_state(&client, Some(TOKEN), copy)
                .await
                .unwrap(),
        );
    }
    assert_eq!(
        states,
        vec![
            WorkingCopyState::Converged,
            WorkingCopyState::LocalChanges,
            WorkingCopyState::RemoteChanges,
            WorkingCopyState::Diverged,
            WorkingCopyState::ResolutionRequired,
            WorkingCopyState::PublicationPending,
        ]
    );
}

#[tokio::test(flavor = "current_thread")]
#[serial]
async fn discard_restores_the_folder_before_the_resolution() {
    let e = env().await;
    let dir = e.dir();
    let copy = e.copy(&dir);
    let client = e.fake.client();
    let h0 = e.fake.commit(&[("a.md", "a\nb\nc\n")]);
    checkout(&e.fake, &copy, &h0);
    let h1 = e.fake.commit_changes(&[("old.md", Some("old\n"))]);
    let first = converge(
        &client,
        Some(TOKEN),
        &copy,
        ConvergeMode::Retrieve { overwrite: false },
        e.opts(),
    )
    .await
    .unwrap();
    assert!(matches!(first, SyncOutcome::Synced { .. }), "{first:?}");
    assert_eq!(read(&dir, "old.md"), "old\n");

    write_files(&dir, &[("a.md", "a\nL\nc\n")]);
    e.fake
        .commit_changes(&[("a.md", Some("a\nR\nc\n")), ("remote.md", Some("remote\n"))]);
    expect_resolution(
        converge(
            &client,
            Some(TOKEN),
            &copy,
            ConvergeMode::Retrieve { overwrite: false },
            e.opts(),
        )
        .await
        .unwrap(),
    );
    assert!(read(&dir, "a.md").contains("<<<<<<< local"));
    assert_eq!(read(&dir, "remote.md"), "remote\n");

    discard_resolution(&copy).unwrap();

    assert_eq!(read(&dir, "a.md"), "a\nL\nc\n");
    assert!(!dir.join("remote.md").exists());
    assert_eq!(read(&dir, "old.md"), "old\n");
    assert_eq!(copy.base().unwrap().commit_sha(), Some(h1.as_str()));
    assert!(copy.resolution().unwrap().is_none());
}

#[tokio::test(flavor = "current_thread")]
#[serial]
async fn discard_after_a_dropped_continue_publishes_nothing_over_the_head() {
    let e = env().await;
    let dir = e.dir();
    let copy = e.copy(&dir);
    let client = e.fake.client();
    let (h1, _) = collided(&e, &dir, &copy).await;
    write_files(&dir, &[("a.md", "a\nL and R\nc\n")]);
    e.fake.drop_next_push();
    let dropped = continue_resolution(&client, TOKEN, &copy, e.opts()).await;
    assert!(
        matches!(dropped, Err(CliError::ServerUnreachable { .. })),
        "{dropped:?}"
    );
    assert!(copy.outbox().unwrap().is_some());

    discard_resolution(&copy).unwrap();
    let outcome = converge(&client, Some(TOKEN), &copy, ConvergeMode::Publish, e.opts())
        .await
        .unwrap();

    expect_resolution(outcome);
    assert!(copy.outbox().unwrap().is_none());
    assert_eq!(e.fake.head().0, h1, "a publication landed over the head");
}

#[tokio::test(flavor = "current_thread")]
#[serial]
async fn a_write_after_a_dropped_continue_is_reviewed_again_then_published() {
    let e = env().await;
    let dir = e.dir();
    let copy = e.copy(&dir);
    let client = e.fake.client();
    collided(&e, &dir, &copy).await;
    write_files(&dir, &[("a.md", "a\nL and R\nc\n")]);
    e.fake.drop_next_push();
    let dropped = continue_resolution(&client, TOKEN, &copy, e.opts()).await;
    assert!(
        matches!(dropped, Err(CliError::ServerUnreachable { .. })),
        "{dropped:?}"
    );
    write_files(&dir, &[("late.md", "late\n")]);

    let fresh = expect_resolution(
        converge(&client, Some(TOKEN), &copy, ConvergeMode::Publish, e.opts())
            .await
            .unwrap(),
    );
    assert!(
        fresh.local_paths.contains(&"late.md".to_string()),
        "{fresh:?}"
    );
    assert!(copy.outbox().unwrap().is_none());

    let outcome = continue_resolution(&client, TOKEN, &copy, e.opts())
        .await
        .unwrap();

    assert!(
        matches!(
            outcome,
            SyncOutcome::Synced {
                published: Some(_),
                ..
            }
        ),
        "{outcome:?}"
    );
    let (_, tree) = e.fake.head();
    assert_eq!(tree["late.md"], "late\n");
    assert_eq!(tree["a.md"], "a\nL and R\nc\n");
}

#[tokio::test(flavor = "current_thread")]
#[serial]
async fn a_failing_required_check_blocks_publication() {
    let e = env().await;
    let dir = e.dir();
    let copy = e.copy(&dir);
    let client = e.fake.client();
    let h0 = e.fake.commit(&[
        (
            ".syns.yaml",
            "owner: alice\nname: proj\nchecks:\n  - exit 3\n",
        ),
        ("a.md", "a\nb\nc\n"),
    ]);
    checkout(&e.fake, &copy, &h0);
    write_files(&dir, &[("a.md", "a\nL\nc\n")]);
    e.fake.commit_changes(&[("a.md", Some("a\nR\nc\n"))]);
    expect_resolution(
        converge(&client, Some(TOKEN), &copy, ConvergeMode::Publish, e.opts())
            .await
            .unwrap(),
    );
    write_files(&dir, &[("a.md", "a\nL and R\nc\n")]);

    let refused = continue_resolution(&client, TOKEN, &copy, e.opts())
        .await
        .unwrap();
    expect_resolution(refused);
    assert!(e.fake.push_bodies().is_empty());

    write_files(
        &dir,
        &[(
            ".syns.yaml",
            "owner: alice\nname: proj\nchecks:\n  - exit 0\n",
        )],
    );
    let passed = continue_resolution(&client, TOKEN, &copy, e.opts())
        .await
        .unwrap();
    assert!(
        matches!(
            passed,
            SyncOutcome::Synced {
                published: Some(_),
                ..
            }
        ),
        "{passed:?}"
    );
}

#[tokio::test(flavor = "current_thread")]
#[serial]
async fn convergence_leaves_an_excluded_local_file_untouched() {
    let e = env().await;
    let dir = e.dir();
    let copy = e.copy(&dir);
    let client = e.fake.client();
    let h0 = e.fake.commit(&[(".gitignore", ".env\n"), ("a.md", "a0\n")]);
    checkout(&e.fake, &copy, &h0);
    write_files(&dir, &[(".env", "local secret\n")]);
    e.fake
        .commit_changes(&[(".env", Some("remote value\n")), ("b.md", Some("b1\n"))]);

    let retrieved = converge(
        &client,
        Some(TOKEN),
        &copy,
        ConvergeMode::Retrieve { overwrite: false },
        e.opts(),
    )
    .await
    .unwrap();
    assert!(
        matches!(retrieved, SyncOutcome::Synced { .. }),
        "{retrieved:?}"
    );
    assert_eq!(read(&dir, ".env"), "local secret\n");
    assert_eq!(read(&dir, "b.md"), "b1\n");

    write_files(&dir, &[("a.md", "a1\n")]);
    let published = converge(&client, Some(TOKEN), &copy, ConvergeMode::Publish, e.opts())
        .await
        .unwrap();

    assert!(
        matches!(
            published,
            SyncOutcome::Synced {
                published: Some(_),
                ..
            }
        ),
        "{published:?}"
    );
    let bodies = e.fake.push_bodies();
    assert_eq!(bodies.len(), 1);
    assert!(bodies[0].get("deletions").is_none(), "{}", bodies[0]);
    assert_eq!(read(&dir, ".env"), "local secret\n");
    let (_, tree) = e.fake.head();
    assert_eq!(tree[".env"], "remote value\n");
    assert_eq!(tree["a.md"], "a1\n");
}

// ---- an interrupted preparation -------------------------------------------

fn set_readonly(path: &Path, readonly: bool) {
    let mut permissions = std::fs::metadata(path).unwrap().permissions();
    #[allow(clippy::permissions_set_readonly_false)]
    permissions.set_readonly(readonly);
    std::fs::set_permissions(path, permissions).unwrap();
}

/// Run `converge` in `mode` with `path` read-only, so its candidate write is
/// refused after the resolution is recorded, then make it writable again;
/// false where this process writes a read-only file anyway.
async fn refuse_preparation_at(
    e: &Env,
    copy: &WorkingCopy,
    path: &str,
    mode: ConvergeMode,
) -> bool {
    let target = copy.root.join(path);
    set_readonly(&target, true);
    if std::fs::OpenOptions::new()
        .write(true)
        .open(&target)
        .is_ok()
    {
        set_readonly(&target, false);
        eprintln!("skipped: this process may write a read-only file");
        return false;
    }
    let failed = converge(&e.fake.client(), Some(TOKEN), copy, mode, e.opts()).await;
    set_readonly(&target, false);
    assert!(matches!(failed, Err(CliError::Io { .. })), "{failed:?}");
    assert!(copy.resolution().unwrap().unwrap().pending_writes.is_some());
    true
}

/// A publication past a head changing `a.md` on both sides and `r.md`
/// remotely, its write to `r.md` refused: the head, none where skipped.
async fn half_written_publication(e: &Env, dir: &Path, copy: &WorkingCopy) -> Option<String> {
    let h0 = e.fake.commit(&[("a.md", "a\nb\nc\n"), ("r.md", "r0\n")]);
    checkout(&e.fake, copy, &h0);
    write_files(dir, &[("a.md", "a\nL\nc\n")]);
    let h1 = e
        .fake
        .commit_changes(&[("a.md", Some("a\nR\nc\n")), ("r.md", Some("r1\n"))]);
    refuse_preparation_at(e, copy, "r.md", ConvergeMode::Publish)
        .await
        .then_some(h1)
}

/// A folder write refused after the resolution is recorded leaves the
/// candidate half-written; a continue finishes the preparation instead of
/// publishing the folder over the head's changes.
#[tokio::test(flavor = "current_thread")]
#[serial]
async fn continue_after_a_failed_preparation_write_publishes_nothing() {
    let e = env().await;
    let dir = e.dir();
    let copy = e.copy(&dir);
    let client = e.fake.client();
    let Some(h1) = half_written_publication(&e, &dir, &copy).await else {
        return;
    };

    let outcome = continue_resolution(&client, TOKEN, &copy, e.opts())
        .await
        .unwrap();

    expect_resolution(outcome);
    assert!(
        e.fake.push_bodies().is_empty(),
        "the continue published a half-prepared folder: {:?}",
        e.fake.push_bodies()
    );
    assert_eq!(e.fake.head().0, h1);
    assert_eq!(read(&dir, "r.md"), "r1\n");
    assert!(read(&dir, "a.md").lines().any(|l| l == "<<<<<<< local"));
}

/// A re-run finishing a half-written preparation leaves a candidate that
/// already landed as it stands, rather than merging its markers again.
#[tokio::test(flavor = "current_thread")]
#[serial]
async fn a_resumed_preparation_keeps_a_candidate_that_already_landed() {
    let e = env().await;
    let dir = e.dir();
    let copy = e.copy(&dir);
    let client = e.fake.client();
    let h0 = e
        .fake
        .commit(&[("a.md", "a\nb\nc\n"), ("b.md", "x\ny\nz\n")]);
    checkout(&e.fake, &copy, &h0);
    write_files(&dir, &[("a.md", "a\nL\nc\n"), ("b.md", "x\nL\nz\n")]);
    e.fake
        .commit_changes(&[("a.md", Some("a\nR\nc\n")), ("b.md", Some("x\nR\nz\n"))]);
    if !refuse_preparation_at(&e, &copy, "b.md", ConvergeMode::Publish).await {
        return;
    }
    let landed = "a\n<<<<<<< local\nL\n||||||| base\nb\n=======\nR\n>>>>>>> remote\nc\n";
    assert_eq!(read(&dir, "a.md"), landed);

    let resumed = expect_resolution(
        converge(&client, Some(TOKEN), &copy, ConvergeMode::Publish, e.opts())
            .await
            .unwrap(),
    );

    assert_eq!(read(&dir, "a.md"), landed);
    assert_eq!(
        read(&dir, "b.md"),
        "x\n<<<<<<< local\nL\n||||||| base\ny\n=======\nR\n>>>>>>> remote\nz\n"
    );
    assert_eq!(
        resumed.collisions,
        vec![
            ("a.md".to_string(), CollisionKind::ModifyModify),
            ("b.md".to_string(), CollisionKind::ModifyModify),
        ]
    );
    assert!(copy.resolution().unwrap().unwrap().pending_writes.is_none());
    assert_eq!(
        copy.local_snapshot().unwrap()["a.md"]
            .clone()
            .unwrap()
            .into_bytes(),
        b"a\nL\nc\n".to_vec()
    );
    assert!(e.fake.push_bodies().is_empty());
}

/// A re-run finishing a half-written preparation keeps naming the
/// delete/modify candidate and the remote-only change that landed before
/// the refusal, although both now read as identical.
#[tokio::test(flavor = "current_thread")]
#[serial]
async fn a_resumed_preparation_keeps_the_summaries_of_its_landed_writes() {
    let e = env().await;
    let dir = e.dir();
    let copy = e.copy(&dir);
    let h0 = e
        .fake
        .commit(&[("d.md", "d0\n"), ("r.md", "r0\n"), ("z.md", "a\nb\nc\n")]);
    checkout(&e.fake, &copy, &h0);
    std::fs::remove_file(dir.join("d.md")).unwrap();
    write_files(&dir, &[("z.md", "a\nL\nc\n")]);
    e.fake.commit_changes(&[
        ("d.md", Some("d1\n")),
        ("r.md", Some("r1\n")),
        ("z.md", Some("a\nR\nc\n")),
    ]);
    let retrieval = ConvergeMode::Retrieve { overwrite: false };
    if !refuse_preparation_at(&e, &copy, "z.md", retrieval).await {
        return;
    }
    assert_eq!(read(&dir, "d.md"), "d1\n");
    assert_eq!(read(&dir, "r.md"), "r1\n");

    let resumed = expect_resolution(
        converge(&e.fake.client(), Some(TOKEN), &copy, retrieval, e.opts())
            .await
            .unwrap(),
    );

    assert!(
        resumed
            .collisions
            .contains(&("d.md".to_string(), CollisionKind::DeleteModify)),
        "{resumed:?}"
    );
    assert!(
        resumed.remote_paths.contains(&"r.md".to_string()),
        "{resumed:?}"
    );
    assert!(read(&dir, "z.md").lines().any(|l| l == "<<<<<<< local"));
}

/// A retrieval asked to overwrite writes the head over a half-written
/// candidate rather than finishing its preparation.
#[tokio::test(flavor = "current_thread")]
#[serial]
async fn overwrite_retrieval_replaces_a_half_written_candidate() {
    let e = env().await;
    let dir = e.dir();
    let copy = e.copy(&dir);
    if half_written_publication(&e, &dir, &copy).await.is_none() {
        return;
    }

    let outcome = converge(
        &e.fake.client(),
        Some(TOKEN),
        &copy,
        ConvergeMode::Retrieve { overwrite: true },
        e.opts(),
    )
    .await
    .unwrap();

    assert!(matches!(outcome, SyncOutcome::Synced { .. }), "{outcome:?}");
    assert!(copy.resolution().unwrap().is_none());
    assert_eq!(read(&dir, "a.md"), "a\nR\nc\n");
    assert_eq!(read(&dir, "r.md"), "r1\n");
}

/// A half-written resolution past the round bound is finished and handed
/// to a person, not answered as resolution required.
#[tokio::test(flavor = "current_thread")]
#[serial]
async fn a_half_written_resolution_past_the_round_bound_requires_attention() {
    let e = env().await;
    let dir = e.dir();
    let copy = e.copy(&dir);
    if half_written_publication(&e, &dir, &copy).await.is_none() {
        return;
    }
    let mut standing = copy.resolution().unwrap().unwrap();
    standing.round = ROUND_BOUND + 1;
    copy.write_resolution(&standing).unwrap();

    let outcome = converge(
        &e.fake.client(),
        Some(TOKEN),
        &copy,
        ConvergeMode::Publish,
        e.opts(),
    )
    .await
    .unwrap();

    match outcome {
        SyncOutcome::AttentionRequired(Some(resolution)) => {
            assert!(resolution.pending_writes.is_none(), "{resolution:?}")
        }
        other => panic!("expected AttentionRequired, got {other:?}"),
    }
    assert_eq!(read(&dir, "r.md"), "r1\n");
}

const PREPARER_ENV: &str = "SYNS_U256_PREPARER";
const KILLED_FILES: usize = 2000;

/// The preparer half runs in a child process re-running this test binary:
/// it converges the folder as a retrieval against the parent's fake, and
/// the parent kills it once the resolution is recorded, before the
/// preparation clears the writes it still owes.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn a_preparation_killed_after_recording_its_resolution_is_finished_before_publishing() {
    if let Ok(spec) = std::env::var(PREPARER_ENV) {
        let spec: Value = serde_json::from_str(&spec).unwrap();
        let cache = PathBuf::from(spec["cache"].as_str().unwrap());
        let dir = PathBuf::from(spec["dir"].as_str().unwrap());
        let client = SynsClient::new(spec["uri"].as_str().unwrap()).unwrap();
        let copy = WorkingCopy::open(&cache, "alice", "proj", &dir).unwrap();
        let _ = converge(
            &client,
            Some(TOKEN),
            &copy,
            ConvergeMode::Retrieve { overwrite: false },
            opts_at(&cache),
        )
        .await;
        return;
    }

    let e = env().await;
    let dir = e.dir();
    let copy = e.copy(&dir);
    let client = e.fake.client();
    let names: Vec<String> = (1..=KILLED_FILES).map(|i| format!("f{i:04}.md")).collect();
    let mut h0_tree = vec![("a.md", "one\ntwo\nthree\n")];
    h0_tree.extend(names.iter().map(|n| (n.as_str(), "base\n")));
    let h0 = e.fake.commit(&h0_tree);
    checkout(&e.fake, &copy, &h0);
    write_files(&dir, &[("a.md", "one\nLOCAL two\nthree\n")]);
    let mut changes = vec![("a.md", Some("one\nREMOTE two\nthree\n"))];
    changes.extend(names.iter().map(|n| (n.as_str(), Some("remote\n"))));
    e.fake.commit_changes(&changes);

    let spec = json!({"uri": e.fake.uri, "cache": e.config.cache_dir(), "dir": dir}).to_string();
    let mut preparer = std::process::Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "integration::convergence_test::a_preparation_killed_after_recording_its_resolution_is_finished_before_publishing",
            "--test-threads=1",
        ])
        .env(PREPARER_ENV, spec)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .unwrap();
    let recorded = copy.state_dir.join("resolution.json");
    let started = std::time::Instant::now();
    while !recorded.exists() {
        if let Some(status) = preparer.try_wait().unwrap() {
            panic!("the preparer exited {status} before recording a resolution");
        }
        assert!(
            started.elapsed() < std::time::Duration::from_secs(120),
            "the preparer recorded no resolution"
        );
        tokio::time::sleep(std::time::Duration::from_millis(1)).await;
    }
    let _ = preparer.kill();
    preparer.wait().unwrap();
    assert!(
        copy.resolution().unwrap().unwrap().pending_writes.is_some(),
        "the kill landed after the preparation finished, so nothing interrupted it"
    );
    let landed = names.iter().filter(|n| read(&dir, n) == "remote\n").count();
    println!("the kill landed with {landed} of {KILLED_FILES} remote-only writes taken");
    let recovery_id = copy.resolution().unwrap().unwrap().recovery_id;

    let rerun = expect_resolution(
        converge(
            &client,
            Some(TOKEN),
            &copy,
            ConvergeMode::Retrieve { overwrite: false },
            e.opts(),
        )
        .await
        .unwrap(),
    );

    assert_eq!(rerun.recovery_id, recovery_id);
    let unwritten: Vec<&String> = names
        .iter()
        .filter(|n| read(&dir, n) != "remote\n")
        .collect();
    assert!(
        unwritten.is_empty(),
        "the re-run left {} remote-only changes unwritten",
        unwritten.len()
    );
    assert!(read(&dir, "a.md").lines().any(|l| l == "<<<<<<< local"));
    assert_eq!(
        copy.local_snapshot().unwrap()["a.md"]
            .clone()
            .unwrap()
            .into_bytes(),
        b"one\nLOCAL two\nthree\n".to_vec()
    );

    write_files(&dir, &[("a.md", "one\nLOCAL and REMOTE two\nthree\n")]);
    let outcome = continue_resolution(&client, TOKEN, &copy, e.opts())
        .await
        .unwrap();

    assert!(
        matches!(
            outcome,
            SyncOutcome::Synced {
                published: Some(_),
                ..
            }
        ),
        "{outcome:?}"
    );
    let (_, tree) = e.fake.head();
    let undone: Vec<&String> = names.iter().filter(|n| tree[*n] != "remote\n").collect();
    assert!(
        undone.is_empty(),
        "the continue undid {} remote changes",
        undone.len()
    );
    assert_eq!(tree["a.md"], "one\nLOCAL and REMOTE two\nthree\n");
}

const HOLDER_ENV: &str = "SYNS_U256_BASE_HOLDER";
const HOLDING: &str = "u256-holder: holding";
const HELD: &str = "u256-holder: read ";

/// The holder half runs in a child process re-running this test binary:
/// it opens the base, reports it holds it, waits for the parent's word,
/// then reads what its open handle still sees.
#[test]
fn state_write_replaces_a_base_another_process_holds_open() {
    if let Ok(path) = std::env::var(HOLDER_ENV) {
        let mut held = std::fs::File::open(&path).unwrap();
        println!("{HOLDING}");
        std::io::stdout().flush().unwrap();
        let mut word = String::new();
        std::io::stdin().read_line(&mut word).unwrap();
        let mut content = String::new();
        held.read_to_string(&mut content).unwrap();
        println!("{HELD}{}", content.replace('\n', " "));
        return;
    }

    let cache = tempfile::tempdir().unwrap();
    let folder = tempfile::tempdir().unwrap();
    let copy = WorkingCopy::open(cache.path(), "alice", "proj", folder.path()).unwrap();
    copy.record_base("h0-commit", HashMap::from([("a.md".into(), "1".into())]))
        .unwrap();

    let mut holder = std::process::Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "integration::convergence_test::state_write_replaces_a_base_another_process_holds_open",
            "--nocapture",
            "--test-threads=1",
        ])
        .env(HOLDER_ENV, copy.state_dir.join("base.json"))
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    let mut lines = BufReader::new(holder.stdout.take().unwrap()).lines();
    lines
        .by_ref()
        .map(|l| l.unwrap())
        .find(|l| l.contains(HOLDING))
        .expect("the holder never opened the base");

    let written = copy.record_base("h1-commit", HashMap::from([("a.md".into(), "2".into())]));

    holder.stdin.take().unwrap().write_all(b"read\n").unwrap();
    // Read the holder's output to its end, so its closing lines meet no
    // closed pipe.
    let rest: Vec<String> = lines.map(|l| l.unwrap()).collect();
    holder.wait().unwrap();
    let held = rest
        .iter()
        .find(|l| l.contains(HELD))
        .expect("the holder reported nothing");

    assert!(written.is_ok(), "{written:?}");
    assert_eq!(copy.base().unwrap().commit_sha(), Some("h1-commit"));
    assert!(held.contains("h0-commit"), "{held}");
    assert!(!held.contains("h1-commit"), "{held}");
}
