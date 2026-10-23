//! Roadmap 278: `syns cat` over many paths in one run, the `files`
//! answer, `--paths-from=-` and `--max-bytes`, and the one verified
//! content store `cat`, `read` and `grep` share, with `syns logout
//! --purge` (FEATURE_SPEC.md § Expected, D-242).
//!
//! Every case drives the built binary against `Fake`, a local HTTP/1.1
//! listener answering one repository (`alice/notes`) the way the server's
//! raw entry does: the stored bytes under an `ETag` of their blob hash,
//! and a `304` with no body where `If-None-Match` names that hash. Each
//! test is named for its Expected row (`r<NN>_…`).

use std::collections::{BTreeMap, HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::process::Output;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime};

use base64::Engine as _;
use serde_json::{Value, json};
use serial_test::serial;
use syns_cli::push::hash::blob_sha1;
use tempfile::TempDir;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

const OWNER: &str = "alice";
const NAME: &str = "notes";
const REPO: &str = "alice/notes";
const PREFIX: &str = "/api/v1/repos/alice/notes";
const HEAD_SHA: &str = "7777777777777777777777777777777777777777";
const HEAD_VERSION: u32 = 7;

// ---- the fake deployment -------------------------------------------------

/// What one raw read of a path is answered with, in place of its bytes.
#[derive(Clone, Debug)]
enum Act {
    /// A refusal: the status, the wire `error`, and a `Retry-After`.
    Refuse {
        status: u16,
        error: &'static str,
        retry_after: Option<u32>,
    },
    /// The connection closed with no answer.
    Close,
    /// The bytes with no `ETag`.
    NoEtag,
    /// The bytes under an `ETag` naming other bytes.
    WrongEtag,
    /// The bytes with no declared length, chunked.
    Chunked,
    /// A declared length of 10 MB and a body that never ends.
    Endless,
    /// A `304` naming another hash than the one asked.
    OtherNotModified,
    /// A refusal whose status line and headers are written at once and
    /// whose body follows only after this long.
    SlowRefusal {
        status: u16,
        error: &'static str,
        after: Duration,
    },
}

/// One request the fake read.
#[derive(Clone, Debug)]
struct Seen {
    /// The route, percent-decoded, without the query.
    path: String,
    query: String,
    headers: Vec<(String, String)>,
    at: Instant,
    status: u16,
}

impl Seen {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k == name)
            .map(|(_, v)| v.as_str())
    }

    fn raw_path(&self) -> Option<&str> {
        self.path.strip_prefix(&format!("{PREFIX}/raw/"))
    }

    fn files_path(&self) -> Option<&str> {
        self.path.strip_prefix(&format!("{PREFIX}/files/"))
    }

    fn query(&self, key: &str) -> Option<String> {
        self.query.split('&').find_map(|pair| {
            let (k, v) = pair.split_once('=')?;
            (k == key).then(|| v.to_string())
        })
    }
}

#[derive(Default)]
struct State {
    /// Each path at the head and the type its raw answer names.
    files: BTreeMap<String, (Vec<u8>, Option<String>)>,
    head_version: u32,
    head_sha: String,
    /// Versions answered by ordinal or hash prefix besides the head.
    versions: Vec<(u32, String)>,
    /// Acts consumed one per raw read of the path, then `always`.
    once: HashMap<String, VecDeque<Act>>,
    always: HashMap<String, Act>,
    /// Every raw answer waits this long, and a path's own delay more.
    hold: Duration,
    delays: HashMap<String, Duration>,
    /// The record and the newest-version reads close unanswered.
    reference_closes: bool,
    seen: Vec<Seen>,
    in_flight: usize,
    max_in_flight: usize,
    answered: Vec<Instant>,
    /// When each raw answer was written whole, by route.
    finished: Vec<(String, Instant)>,
}

struct Fake {
    rt: tokio::runtime::Runtime,
    port: u16,
    uri: String,
    state: Arc<Mutex<State>>,
}

/// What the listener writes for one request.
enum Reply {
    Whole {
        status: u16,
        headers: Vec<(String, String)>,
        body: Vec<u8>,
    },
    Chunked {
        headers: Vec<(String, String)>,
        body: Vec<u8>,
    },
    Endless {
        headers: Vec<(String, String)>,
    },
    /// A JSON refusal: its head now, its body after `after`.
    Slow {
        status: u16,
        body: Vec<u8>,
        after: Duration,
    },
    Close,
}

fn version_entry(version: u32, sha: &str) -> Value {
    json!({
        "version": version, "sha": sha, "parentSha": null, "message": "m",
        "messageBody": null, "author": "alice",
        "createdAt": "2026-01-01T00:00:00Z", "filesChanged": [],
    })
}

fn json_reply(status: u16, body: Value) -> Reply {
    Reply::Whole {
        status,
        headers: vec![("content-type".into(), "application/json".into())],
        body: body.to_string().into_bytes(),
    }
}

fn is_text(bytes: &[u8]) -> bool {
    !bytes.contains(&0) && std::str::from_utf8(bytes).is_ok()
}

impl State {
    fn route(&mut self, method: &str, path: &str, headers: &[(String, String)]) -> Reply {
        if method != "GET" {
            return json_reply(404, json!({"error": "not_found"}));
        }
        if path == PREFIX || path == format!("{PREFIX}/versions") {
            if self.reference_closes {
                return Reply::Close;
            }
            if path == PREFIX {
                return json_reply(
                    200,
                    json!({
                        "owner": OWNER, "name": NAME, "description": null,
                        "commitSha": self.head_sha, "status": "active", "author": null,
                        "tags": [], "visibility": "public", "forkedFrom": null,
                        "forkCount": 0, "fileCount": self.files.len(), "role": null,
                        "createdAt": "2026-01-01T00:00:00Z",
                        "updatedAt": "2026-01-01T00:00:00Z",
                    }),
                );
            }
            return json_reply(
                200,
                json!({
                    "data": [version_entry(self.head_version, &self.head_sha)],
                    "total": 1, "limit": 1, "offset": 0,
                }),
            );
        }
        if let Some(reference) = path.strip_prefix(&format!("{PREFIX}/versions/")) {
            let mut known = self.versions.clone();
            known.push((self.head_version, self.head_sha.clone()));
            return match known.iter().find(|(v, sha)| {
                v.to_string() == reference || (reference.len() >= 4 && sha.starts_with(reference))
            }) {
                Some((v, sha)) => json_reply(200, version_entry(*v, sha)),
                None => json_reply(404, json!({"error": "not_found"})),
            };
        }
        if path == format!("{PREFIX}/tree") {
            let entries: Vec<Value> = self
                .files
                .iter()
                .map(|(p, (bytes, _))| {
                    json!({
                        "name": p.rsplit('/').next().unwrap(), "path": p, "type": "file",
                        "size": bytes.len(), "sha": blob_sha1(bytes),
                    })
                })
                .collect();
            return json_reply(
                200,
                json!({"entries": entries, "commitSha": self.head_sha, "truncated": false}),
            );
        }
        if let Some(p) = path.strip_prefix(&format!("{PREFIX}/files/")) {
            return match self.files.get(p) {
                Some((bytes, _)) => json_reply(
                    200,
                    json!({
                        "path": p, "sha": blob_sha1(bytes), "size": bytes.len(),
                        "content": if is_text(bytes) {
                            Value::from(String::from_utf8(bytes.clone()).unwrap())
                        } else {
                            Value::Null
                        },
                    }),
                ),
                None => json_reply(
                    404,
                    json!({"error": "not_found", "message": "File not found"}),
                ),
            };
        }
        if let Some(p) = path.strip_prefix(&format!("{PREFIX}/raw/")) {
            return self.raw(p, headers);
        }
        json_reply(404, json!({"error": "repo_not_found"}))
    }

    fn raw(&mut self, p: &str, headers: &[(String, String)]) -> Reply {
        let act = self
            .once
            .get_mut(p)
            .and_then(VecDeque::pop_front)
            .or_else(|| self.always.get(p).cloned());
        match &act {
            Some(Act::Refuse {
                status,
                error,
                retry_after,
            }) => {
                let mut reply = json_reply(*status, json!({"error": error, "message": error}));
                if let (Reply::Whole { headers, .. }, Some(wait)) = (&mut reply, retry_after) {
                    headers.push(("retry-after".into(), wait.to_string()));
                }
                return reply;
            }
            Some(Act::Close) => return Reply::Close,
            Some(Act::SlowRefusal {
                status,
                error,
                after,
            }) => {
                return Reply::Slow {
                    status: *status,
                    body: json!({"error": error, "message": error})
                        .to_string()
                        .into_bytes(),
                    after: *after,
                };
            }
            _ => {}
        }
        let Some((bytes, media)) = self.files.get(p).cloned() else {
            return json_reply(
                404,
                json!({"error": "not_found", "message": "File not found"}),
            );
        };
        let sha = blob_sha1(&bytes);
        let inm = headers
            .iter()
            .find(|(k, _)| k == "if-none-match")
            .map(|(_, v)| v.clone());
        if matches!(act, Some(Act::OtherNotModified)) {
            return Reply::Whole {
                status: 304,
                headers: vec![("etag".into(), format!("\"{}\"", "1".repeat(40)))],
                body: Vec::new(),
            };
        }
        if inm.as_deref() == Some(format!("\"{sha}\"").as_str()) {
            return Reply::Whole {
                status: 304,
                headers: vec![
                    ("etag".into(), format!("\"{sha}\"")),
                    ("cache-control".into(), "private, max-age=0".into()),
                ],
                body: Vec::new(),
            };
        }
        let media = media.unwrap_or_else(|| {
            if is_text(&bytes) {
                "text/plain; charset=utf-8".to_string()
            } else {
                "application/octet-stream".to_string()
            }
        });
        let mut out = vec![("content-type".to_string(), media)];
        match act {
            Some(Act::NoEtag) => {}
            Some(Act::WrongEtag) => out.push(("etag".into(), format!("\"{}\"", "0".repeat(40)))),
            _ => out.push(("etag".into(), format!("\"{sha}\""))),
        }
        match act {
            Some(Act::Chunked) => Reply::Chunked {
                headers: out,
                body: bytes,
            },
            Some(Act::Endless) => Reply::Endless { headers: out },
            _ => Reply::Whole {
                status: 200,
                headers: out,
                body: bytes,
            },
        }
    }
}

/// One request read off a connection, or `None` where the peer closed.
async fn read_request(sock: &mut TcpStream) -> Option<(String, String, Vec<(String, String)>)> {
    let mut buf = Vec::new();
    let mut chunk = [0u8; 8192];
    let end = loop {
        if let Some(pos) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
            break pos + 4;
        }
        let n = sock.read(&mut chunk).await.ok()?;
        if n == 0 {
            return None;
        }
        buf.extend_from_slice(&chunk[..n]);
    };
    let head = String::from_utf8_lossy(&buf[..end]).to_string();
    let mut lines = head.split("\r\n");
    let mut first = lines.next()?.split(' ');
    let method = first.next()?.to_string();
    let target = first.next()?.to_string();
    let headers = lines
        .filter_map(|l| l.split_once(':'))
        .map(|(k, v)| (k.trim().to_ascii_lowercase(), v.trim().to_string()))
        .collect();
    Some((method, target, headers))
}

fn reason(status: u16) -> &'static str {
    match status {
        200 => "OK",
        304 => "Not Modified",
        404 => "Not Found",
        _ => "Status",
    }
}

async fn serve(state: Arc<Mutex<State>>, mut sock: TcpStream) {
    while let Some((method, target, headers)) = read_request(&mut sock).await {
        let (route, query) = match target.split_once('?') {
            Some((r, q)) => (r.to_string(), q.to_string()),
            None => (target.clone(), String::new()),
        };
        let path = urlencoding::decode(&route)
            .map(|p| p.into_owned())
            .unwrap_or(route.clone());
        let is_raw = path.starts_with(&format!("{PREFIX}/raw/"));
        let (reply, wait, index) = {
            let mut s = state.lock().unwrap();
            let reply = s.route(&method, &path, &headers);
            let mut wait = Duration::ZERO;
            if is_raw {
                s.in_flight += 1;
                s.max_in_flight = s.max_in_flight.max(s.in_flight);
                let p = path.trim_start_matches(&format!("{PREFIX}/raw/"));
                wait = s.hold + s.delays.get(p).copied().unwrap_or_default();
            }
            let status = match &reply {
                Reply::Whole { status, .. } => *status,
                Reply::Chunked { .. } | Reply::Endless { .. } => 200,
                Reply::Slow { status, .. } => *status,
                Reply::Close => 0,
            };
            s.seen.push(Seen {
                path: path.clone(),
                query,
                headers,
                at: Instant::now(),
                status,
            });
            (reply, wait, s.seen.len() - 1)
        };
        let _ = index;
        if !wait.is_zero() {
            tokio::time::sleep(wait).await;
        }
        let raw_route = path.clone();
        let done = |state: &Arc<Mutex<State>>| {
            if is_raw {
                let mut s = state.lock().unwrap();
                s.in_flight -= 1;
                s.answered.push(Instant::now());
                s.finished.push((raw_route.clone(), Instant::now()));
            }
        };
        let head = |status: u16, headers: &[(String, String)], extra: &str| {
            let mut text = format!("HTTP/1.1 {status} {}\r\n", reason(status));
            for (k, v) in headers {
                text.push_str(&format!("{k}: {v}\r\n"));
            }
            text.push_str(extra);
            text.push_str("\r\n");
            text
        };
        let written = match reply {
            Reply::Close => {
                done(&state);
                return;
            }
            Reply::Whole {
                status,
                headers,
                body,
            } => {
                let length = if status == 304 {
                    String::new()
                } else {
                    format!("content-length: {}\r\n", body.len())
                };
                let mut out = head(status, &headers, &length).into_bytes();
                out.extend_from_slice(&body);
                sock.write_all(&out).await
            }
            Reply::Chunked { headers, body } => {
                let mut out = head(200, &headers, "transfer-encoding: chunked\r\n").into_bytes();
                let mid = body.len() / 2;
                for part in [&body[..mid], &body[mid..]] {
                    if !part.is_empty() {
                        out.extend_from_slice(format!("{:x}\r\n", part.len()).as_bytes());
                        out.extend_from_slice(part);
                        out.extend_from_slice(b"\r\n");
                    }
                }
                out.extend_from_slice(b"0\r\n\r\n");
                sock.write_all(&out).await
            }
            Reply::Slow {
                status,
                body,
                after,
            } => {
                let headers = vec![("content-type".to_string(), "application/json".to_string())];
                let length = format!("content-length: {}\r\n", body.len());
                let started = sock
                    .write_all(head(status, &headers, &length).as_bytes())
                    .await;
                if started.is_ok() {
                    tokio::time::sleep(after).await;
                    sock.write_all(&body).await
                } else {
                    started
                }
            }
            Reply::Endless { headers } => {
                let out = head(200, &headers, "content-length: 10000000\r\n");
                if sock.write_all(out.as_bytes()).await.is_ok() {
                    loop {
                        if sock.write_all(&[b'x'; 1024]).await.is_err() {
                            break;
                        }
                        tokio::time::sleep(Duration::from_millis(20)).await;
                    }
                }
                done(&state);
                return;
            }
        };
        done(&state);
        if written.is_err() {
            return;
        }
    }
}

impl Fake {
    fn start() -> Fake {
        Fake::bind(0)
    }

    /// A fake on `port`, or on any port where that one is taken.
    fn bind(port: u16) -> Fake {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .unwrap();
        let listener = rt
            .block_on(TcpListener::bind(("127.0.0.1", port)))
            .or_else(|_| rt.block_on(TcpListener::bind(("127.0.0.1", 0))))
            .unwrap();
        let port = listener.local_addr().unwrap().port();
        let state = Arc::new(Mutex::new(State {
            head_version: HEAD_VERSION,
            head_sha: HEAD_SHA.to_string(),
            ..State::default()
        }));
        let shared = state.clone();
        rt.spawn(async move {
            loop {
                let Ok((sock, _)) = listener.accept().await else {
                    return;
                };
                tokio::spawn(serve(shared.clone(), sock));
            }
        });
        Fake {
            rt,
            port,
            uri: format!("http://127.0.0.1:{port}"),
            state,
        }
    }

    fn with<T>(&self, f: impl FnOnce(&mut State) -> T) -> T {
        f(&mut self.state.lock().unwrap())
    }

    fn put(&self, path: &str, bytes: &[u8]) {
        self.with(|s| s.files.insert(path.to_string(), (bytes.to_vec(), None)));
    }

    fn put_typed(&self, path: &str, bytes: &[u8], media: &str) {
        self.with(|s| {
            s.files
                .insert(path.to_string(), (bytes.to_vec(), Some(media.to_string())))
        });
    }

    fn remove(&self, path: &str) {
        self.with(|s| s.files.remove(path));
    }

    fn once(&self, path: &str, act: Act) {
        self.with(|s| s.once.entry(path.to_string()).or_default().push_back(act));
    }

    fn always(&self, path: &str, act: Act) {
        self.with(|s| s.always.insert(path.to_string(), act));
    }

    fn seen(&self) -> Vec<Seen> {
        self.with(|s| s.seen.clone())
    }

    fn raw_seen(&self) -> Vec<Seen> {
        self.seen()
            .into_iter()
            .filter(|r| r.raw_path().is_some())
            .collect()
    }

    fn clear(&self) {
        self.with(|s| {
            s.seen.clear();
            s.answered.clear();
            s.max_in_flight = 0;
        });
    }
}

impl Drop for Fake {
    fn drop(&mut self) {
        let _ = &self.rt;
    }
}

// ---- one machine running the binary --------------------------------------

struct Env {
    fake: Fake,
    config: TempDir,
    cache: TempDir,
    work: TempDir,
}

fn blob(bytes: &[u8]) -> String {
    blob_sha1(bytes)
}

impl Env {
    /// A machine signed in as alice, standing in a checkout of
    /// `alice/notes`.
    fn new() -> Env {
        Env::on(Fake::start())
    }

    fn on(fake: Fake) -> Env {
        let env = Env {
            fake,
            config: tempfile::tempdir().unwrap(),
            cache: tempfile::tempdir().unwrap(),
            work: tempfile::tempdir().unwrap(),
        };
        std::fs::write(
            env.work.path().join(".syns.yaml"),
            format!("owner: {OWNER}\nname: {NAME}\n"),
        )
        .unwrap();
        env.credential(Some("alice"));
        env
    }

    fn credential(&self, username: Option<&str>) {
        let mut body = json!({"token": "test-token"});
        if let Some(name) = username {
            body["username"] = json!(name);
        }
        std::fs::write(
            self.config.path().join("credentials.json"),
            body.to_string(),
        )
        .unwrap();
    }

    fn signed_out(&self) {
        let _ = std::fs::remove_file(self.config.path().join("credentials.json"));
    }

    fn server_key(&self) -> String {
        format!("127.0.0.1-{}", self.fake.port)
    }

    fn account(&self, key: &str) -> PathBuf {
        self.cache
            .path()
            .join("store")
            .join(self.server_key())
            .join(key)
    }

    /// The store of the run: alice's account directory.
    fn store(&self) -> PathBuf {
        self.account("u-alice")
    }

    fn command(&self, cwd: &Path, args: &[&str]) -> assert_cmd::Command {
        self.command_on(&self.fake.uri, cwd, args)
    }

    fn command_on(&self, uri: &str, cwd: &Path, args: &[&str]) -> assert_cmd::Command {
        let mut command = assert_cmd::Command::cargo_bin("syns").unwrap();
        command
            .current_dir(cwd)
            .env("SYNS_CONFIG_DIR", self.config.path())
            .env("SYNS_CACHE_DIR", self.cache.path())
            .env_remove("SYNS_URL")
            .env_remove("SYNS_CACHE_MAX_BYTES")
            .arg("--server")
            .arg(uri)
            .args(args);
        command
    }

    fn run(&self, args: &[&str]) -> Output {
        self.command(self.work.path(), args).output().unwrap()
    }

    fn run_in(&self, cwd: &Path, args: &[&str]) -> Output {
        self.command(cwd, args).output().unwrap()
    }

    fn run_stdin(&self, stdin: &[u8], args: &[&str]) -> Output {
        self.command(self.work.path(), args)
            .write_stdin(stdin.to_vec())
            .output()
            .unwrap()
    }

    fn run_env(&self, envs: &[(&str, &str)], args: &[&str]) -> Output {
        let mut command = self.command(self.work.path(), args);
        for (k, v) in envs {
            command.env(k, v);
        }
        command.output().unwrap()
    }

    fn hints(&self) -> String {
        std::fs::read_to_string(self.store().join("hints").join(OWNER).join(NAME))
            .unwrap_or_default()
    }

    fn size_file(&self) -> u64 {
        std::fs::read_to_string(self.store().join("size"))
            .unwrap()
            .trim()
            .parse()
            .unwrap()
    }

    fn blobs(&self) -> BTreeMap<String, Vec<u8>> {
        let mut out = BTreeMap::new();
        if let Ok(dir) = std::fs::read_dir(self.store().join("blobs")) {
            for entry in dir.flatten() {
                out.insert(
                    entry.file_name().to_string_lossy().into_owned(),
                    std::fs::read(entry.path()).unwrap(),
                );
            }
        }
        out
    }
}

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

fn code(output: &Output) -> i32 {
    output.status.code().unwrap_or(-1)
}

fn doc(output: &Output) -> Value {
    serde_json::from_slice(&output.stdout).unwrap_or_else(|e| {
        panic!(
            "no one document ({e}): {}{}",
            stdout(output),
            stderr(output)
        )
    })
}

fn ok(output: &Output) {
    assert_eq!(
        code(output),
        0,
        "stdout {}\nstderr {}",
        stdout(output),
        stderr(output)
    );
}

/// The `files` entry the document answers for a read text file.
fn text_entry(path: &str, bytes: &[u8]) -> Value {
    json!({
        "content": String::from_utf8(bytes.to_vec()).unwrap(),
        "path": path, "sha": blob(bytes), "size": bytes.len(),
    })
}

fn error_entry(path: &str, code: &str, message: &str) -> Value {
    json!({"error": {"code": code, "message": message}, "path": path})
}

fn files_doc(files: Vec<Value>, deferred: &[&str]) -> Value {
    json!({
        "commitSha": HEAD_SHA, "deferred": deferred, "files": files, "version": HEAD_VERSION,
    })
}

const PNG: &[u8] = b"\x89PNG\r\n\x1a\n\x00\x00\x00\rIHDR\xff\xfe";
const PDF: &[u8] = b"%PDF-1.7\n\x00\xff binary";
const BIN: &[u8] = b"\x00\x01\x02\x03 generic";

/// Every file under `root`, relative, with `/` for a directory.
fn tree_of(root: &Path) -> Vec<String> {
    let mut out = Vec::new();
    let mut pending = vec![root.to_path_buf()];
    while let Some(dir) = pending.pop() {
        for entry in std::fs::read_dir(&dir).unwrap().flatten() {
            let path = entry.path();
            let rel = path
                .strip_prefix(root)
                .unwrap()
                .to_string_lossy()
                .replace('\\', "/");
            if entry.file_type().unwrap().is_dir() {
                out.push(format!("{rel}/"));
                pending.push(path);
            } else {
                out.push(rel);
            }
        }
    }
    out.sort();
    out
}

#[cfg(unix)]
fn mode_of(path: &Path) -> u32 {
    use std::os::unix::fs::PermissionsExt;
    std::fs::symlink_metadata(path)
        .unwrap()
        .permissions()
        .mode()
        & 0o777
}

fn set_mtime(path: &Path, ago: Duration) {
    let at = SystemTime::now() - ago;
    std::fs::File::options()
        .write(true)
        .open(path)
        .unwrap()
        .set_times(std::fs::FileTimes::new().set_accessed(at).set_modified(at))
        .unwrap();
}

// ---- Modes and arguments -------------------------------------------------

// Row 1: one path keeps v0.3.23's answer, `mediaType` included.
#[test]
fn r01_one_path_keeps_todays_answer() {
    let env = Env::new();
    env.fake.put("a.md", b"# a\n");

    let bytes = env.run(&["cat", "a.md"]);
    ok(&bytes);
    assert_eq!(stdout(&bytes), "# a\n");
    assert_eq!(
        stderr(&bytes),
        format!("read at version {HEAD_VERSION}, commit {HEAD_SHA}\n")
    );

    let expected = format!(
        "{{\n  \"commitSha\": \"{HEAD_SHA}\",\n  \"content\": \"# a\\n\",\n  \"mediaType\": null,\n  \"path\": \"a.md\",\n  \"sha\": \"{}\",\n  \"size\": 4,\n  \"version\": {HEAD_VERSION}\n}}\n",
        blob(b"# a\n")
    );
    for args in [
        vec!["cat", "a.md", "--json"],
        vec!["cat", "--json", "--", "a.md"],
    ] {
        let _ = std::fs::remove_dir_all(env.cache.path().join("store"));
        let out = env.run(&args);
        ok(&out);
        assert_eq!(stdout(&out), expected, "{args:?}");
        assert_eq!(stderr(&out), "", "{args:?}");
    }
}

// Row 1a: a warm one-path read writes what the cold one wrote, run 2 a `304`.
#[test]
fn r01a_a_warm_one_path_read_writes_what_the_cold_one_wrote() {
    let env = Env::new();
    env.fake.put_typed("logo.png", PNG, "image/png");
    env.fake.put_typed("doc.pdf", PDF, "application/pdf");
    env.fake
        .put_typed("blob.bin", BIN, "application/octet-stream");
    env.fake.put("a.md", b"# a\n");

    for (args, media) in [
        (vec!["--json", "cat", "logo.png"], Some(json!("image/png"))),
        (
            vec!["--json", "cat", "doc.pdf"],
            Some(json!("application/pdf")),
        ),
        (vec!["--json", "cat", "blob.bin"], None),
        (vec!["--json", "cat", "a.md"], Some(Value::Null)),
        (vec!["cat", "logo.png"], None),
        (vec!["cat", "a.md"], None),
    ] {
        env.fake.clear();
        let cold = env.run(&args);
        ok(&cold);
        let warm = env.run(&args);
        ok(&warm);
        assert_eq!(cold.stdout, warm.stdout, "{args:?}");
        let raws = env.fake.raw_seen();
        assert_eq!(raws.len(), 2, "{args:?}");
        assert_eq!(raws[1].status, 304, "{args:?}: run 2 is a 304");
        assert!(raws[1].header("if-none-match").is_some(), "{args:?}");
        if args[0] == "--json" {
            let document = doc(&warm);
            assert_eq!(document.get("mediaType").cloned(), media, "{args:?}");
        }
    }
}

// Row 1a: after grep wrote a `?` type, a binary single-path `--json` read
// sends no `If-None-Match`, records the type, and the next run is a `304`;
// a text one still gets a `304`.
#[test]
fn r01a_a_hint_grep_wrote_costs_one_full_read_of_a_binary() {
    let env = Env::new();
    env.fake.put_typed("copy.png", PNG, "image/png");
    env.fake.put_typed("logo.png", PNG, "image/png");
    env.fake.put("a.md", b"needle\n");
    // The blob stored by a read of another path holding the same bytes.
    ok(&env.run(&["--json", "cat", "copy.png"]));
    ok(&env.run(&["--json", "grep", "needle"]));
    assert!(
        env.hints()
            .contains(&format!("{}\t?\tlogo.png\n", blob(PNG))),
        "{}",
        env.hints()
    );

    env.fake.clear();
    let first = env.run(&["--json", "cat", "logo.png"]);
    ok(&first);
    assert_eq!(doc(&first)["mediaType"], json!("image/png"));
    let raws = env.fake.raw_seen();
    assert_eq!(raws.len(), 1);
    assert_eq!(raws[0].header("if-none-match"), None);
    assert_eq!(raws[0].status, 200);
    assert!(
        env.hints()
            .contains(&format!("{}\timage/png\tlogo.png\n", blob(PNG)))
    );

    env.fake.clear();
    let second = env.run(&["--json", "cat", "logo.png"]);
    assert_eq!(first.stdout, second.stdout);
    assert_eq!(env.fake.raw_seen()[0].status, 304);

    env.fake.clear();
    let text = env.run(&["--json", "cat", "a.md"]);
    ok(&text);
    assert_eq!(doc(&text)["mediaType"], Value::Null);
    assert_eq!(env.fake.raw_seen()[0].status, 304);
}

// Row 2: the `files` answer starts at two paths, `--paths-from=-` or
// `--max-bytes`, its members in lexical order.
#[test]
fn r02_the_files_answer_starts_at_two_paths() {
    let env = Env::new();
    env.fake.put("a.md", b"a\n");
    env.fake.put("b.md", b"b\n");

    let two = env.run(&["--json", "cat", "--", "a.md", "b.md", "missing.md"]);
    ok(&two);
    let expected = format!(
        r#"{{
  "commitSha": "{HEAD_SHA}",
  "deferred": [],
  "files": [
    {{
      "content": "a\n",
      "path": "a.md",
      "sha": "{a}",
      "size": 2
    }},
    {{
      "content": "b\n",
      "path": "b.md",
      "sha": "{b}",
      "size": 2
    }},
    {{
      "error": {{
        "code": "NOT_FOUND",
        "message": "path not found at version {HEAD_VERSION}: missing.md"
      }},
      "path": "missing.md"
    }}
  ],
  "version": {HEAD_VERSION}
}}
"#,
        a = blob(b"a\n"),
        b = blob(b"b\n")
    );
    assert_eq!(stdout(&two), expected);

    let piped = env.run_stdin(b"a.md\n", &["--json", "cat", "--paths-from=-"]);
    ok(&piped);
    assert_eq!(
        doc(&piped),
        files_doc(vec![text_entry("a.md", b"a\n")], &[])
    );

    let budget = env.run(&["--json", "cat", "--max-bytes=100000", "a.md"]);
    ok(&budget);
    assert_eq!(
        doc(&budget),
        files_doc(vec![text_entry("a.md", b"a\n")], &[])
    );
}

// Row 3: order is kept whatever order the answers come back in.
#[test]
fn r03_order_is_kept() {
    let env = Env::new();
    let names = ["p1.md", "p2.md", "p3.md", "p4.md", "p5.md"];
    for (i, name) in names.iter().enumerate() {
        env.fake.put(name, format!("{name}\n").as_bytes());
        // The first asked answers last.
        env.fake.with(|s| {
            s.delays
                .insert(name.to_string(), Duration::from_millis(60 * (5 - i as u64)))
        });
    }
    let mut args = vec!["--json", "cat", "--"];
    args.extend(names);
    let out = env.run(&args);
    ok(&out);
    let files: Vec<Value> = names
        .iter()
        .map(|n| text_entry(n, format!("{n}\n").as_bytes()))
        .collect();
    assert_eq!(doc(&out), files_doc(files, &[]));

    // `deferred` in order too: each file is 6 bytes, the budget holds two.
    let mut args = vec!["--json", "cat", "--max-bytes=12", "--"];
    args.extend(names);
    let out = env.run(&args);
    ok(&out);
    assert_eq!(
        doc(&out),
        files_doc(
            vec![
                text_entry("p1.md", b"p1.md\n"),
                text_entry("p2.md", b"p2.md\n")
            ],
            &["p3.md", "p4.md", "p5.md"]
        )
    );
}

// Row 4: one commit for all files, resolved once.
#[test]
fn r04_one_commit_for_all_files() {
    let env = Env::new();
    env.fake.with(|s| {
        s.versions
            .push((3, "3333333333333333333333333333333333333333".into()))
    });
    for i in 0..6 {
        env.fake
            .put(&format!("f{i}.md"), format!("{i}\n").as_bytes());
    }
    let paths: Vec<String> = (0..6).map(|i| format!("f{i}.md")).collect();
    let mut args: Vec<&str> = vec!["--json", "cat", "--"];
    args.extend(paths.iter().map(String::as_str));
    ok(&env.run(&args));
    let seen = env.fake.seen();
    assert_eq!(seen.iter().filter(|r| r.path == PREFIX).count(), 1);
    assert_eq!(
        seen.iter()
            .filter(|r| r.path == format!("{PREFIX}/versions"))
            .count(),
        1
    );
    let raws = env.fake.raw_seen();
    assert_eq!(raws.len(), 6);
    assert!(raws.iter().all(|r| r.query("ref").as_deref() == Some("7")));

    for version in ["3", "3333"] {
        env.fake.clear();
        let mut pinned = vec!["--json", "cat", "--version", version, "--"];
        pinned.extend(paths.iter().map(String::as_str));
        let out = env.run(&pinned);
        ok(&out);
        assert_eq!(doc(&out)["version"], json!(3));
        let seen = env.fake.seen();
        assert_eq!(
            seen.iter()
                .filter(|r| r.path.starts_with(&format!("{PREFIX}/versions")))
                .count(),
            1,
            "{version}"
        );
        assert!(
            env.fake
                .raw_seen()
                .iter()
                .all(|r| r.query("ref").as_deref() == Some("3"))
        );
    }
}

// Row 5: text as `content`, bytes as standard padded `contentBase64`.
#[test]
fn r05_text_and_bytes() {
    let env = Env::new();
    let nul: &[u8] = b"one\x00two";
    let invalid: &[u8] = b"caf\xe9 \xff";
    env.fake.put("t.md", "héllo\n".as_bytes());
    env.fake.put("p.png", PNG);
    env.fake.put("n.txt", nul);
    env.fake.put("i.txt", invalid);
    env.fake.put("e.md", b"");
    let out = env.run(&[
        "--json", "cat", "--", "t.md", "p.png", "n.txt", "i.txt", "e.md",
    ]);
    ok(&out);
    let files = doc(&out)["files"].clone();
    assert_eq!(files[0], text_entry("t.md", "héllo\n".as_bytes()));
    for (index, (name, bytes)) in [("p.png", PNG), ("n.txt", nul), ("i.txt", invalid)]
        .into_iter()
        .enumerate()
    {
        let entry = &files[index + 1];
        assert_eq!(entry["path"], json!(name));
        assert!(entry.get("content").is_none(), "{name}");
        let encoded = entry["contentBase64"].as_str().unwrap();
        assert_eq!(
            encoded,
            base64::engine::general_purpose::STANDARD.encode(bytes)
        );
        assert_eq!(
            base64::engine::general_purpose::STANDARD
                .decode(encoded)
                .unwrap(),
            bytes
        );
        assert_eq!(entry["sha"], json!(blob(bytes)));
        assert_eq!(entry["size"], json!(bytes.len()));
    }
    assert_eq!(
        files[4],
        json!({"content": "", "path": "e.md", "sha": blob(b""), "size": 0})
    );
}

// Row 6: many paths without `--json` print like `cat a b`.
#[test]
fn r06_many_paths_without_json_print_like_cat() {
    let env = Env::new();
    env.fake.put("a.md", b"alpha\n");
    env.fake.put("b.md", b"beta");
    env.fake.put("c.md", b"gamma\n");
    let out = env.run(&["cat", "--", "a.md", "b.md"]);
    ok(&out);
    assert_eq!(stdout(&out), "alpha\nbeta");
    assert_eq!(
        stderr(&out),
        format!("read at version {HEAD_VERSION}, commit {HEAD_SHA}\n")
    );

    env.fake.remove("b.md");
    let out = env.run(&["cat", "--", "a.md", "b.md", "c.md"]);
    assert_eq!(code(&out), 1);
    assert_eq!(stdout(&out), "alpha\n");
    assert_eq!(stderr(&out), "error: file not found: b.md\n");

    let out = env.run(&["cat", "--version", "7", "--", "a.md", "b.md", "c.md"]);
    assert_eq!(code(&out), 1);
    assert_eq!(stdout(&out), "alpha\n");
    assert_eq!(stderr(&out), "error: path not found at version 7: b.md\n");
}

// Row 7: parser refusals, exit 2, no request.
#[test]
fn r07_parser_refusals_exit_2_with_no_request() {
    let env = Env::new();
    env.fake.put("a.md", b"a");
    // Each case's own refusal, so an unknown-argument refusal (v0.3.23's
    // answer to every one of these flags) does not pass for it.
    for (args, refusal) in [
        (
            vec!["cat", "--max-bytes=10", "a.md"],
            "error: --max-bytes needs --json",
        ),
        (
            vec!["--json", "cat", "--paths-from=-", "a.md"],
            "error: the argument '--paths-from <->' cannot be used with '[PATH]...'",
        ),
        (
            vec!["--json", "cat", "--paths-from=list.txt"],
            "error: invalid value 'list.txt' for '--paths-from <->': only - (standard input) is accepted",
        ),
        (
            vec!["--json", "cat", "--max-bytes=-1", "a.md"],
            "error: invalid value '-1' for '--max-bytes <N>': invalid digit found in string",
        ),
        (
            vec!["--json", "cat", "--max-bytes=many", "a.md"],
            "error: invalid value 'many' for '--max-bytes <N>': invalid digit found in string",
        ),
        (
            vec!["--json", "cat"],
            "error: the following required arguments were not provided:\n  <PATH>...",
        ),
        (
            vec!["cat"],
            "error: the following required arguments were not provided:\n  <PATH>...",
        ),
    ] {
        let out = env.run(&args);
        assert_eq!(code(&out), 2, "{args:?}: {}", stderr(&out));
        assert!(
            stderr(&out).starts_with(refusal),
            "{args:?}: {}",
            stderr(&out)
        );
    }
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt;
        let out = env
            .command(env.work.path(), &["--json", "cat", "--", "a.md"])
            .arg(std::ffi::OsStr::from_bytes(b"bad\xff.md"))
            .output()
            .unwrap();
        assert_eq!(code(&out), 2, "{}", stderr(&out));
        assert!(
            stderr(&out).starts_with("error: invalid UTF-8 was detected in one or more arguments"),
            "{}",
            stderr(&out)
        );
    }
    assert!(env.fake.seen().is_empty(), "{:?}", env.fake.seen());
}

// Row 8: standard input.
#[test]
fn r08_standard_input() {
    let env = Env::new();
    env.fake.put("a.md", b"a\n");
    env.fake.put("b.md", b"b\n");
    let out = env.run_stdin(b"a.md\r\n\nb.md\n", &["--json", "cat", "--paths-from=-"]);
    ok(&out);
    assert_eq!(
        doc(&out),
        files_doc(
            vec![text_entry("a.md", b"a\n"), text_entry("b.md", b"b\n")],
            &[]
        )
    );
    let raws: Vec<String> = env
        .fake
        .raw_seen()
        .iter()
        .map(|r| r.raw_path().unwrap().to_string())
        .collect();
    assert_eq!(raws.len(), 2, "{raws:?}");

    env.fake.clear();
    let out = env.run_stdin(b"a.md\n\xff\xfe\n", &["--json", "cat", "--paths-from=-"]);
    assert_eq!(code(&out), 1, "{}", stdout(&out));
    assert!(
        doc(&out)["error"]
            .as_str()
            .unwrap()
            .starts_with("configuration error:"),
        "{}",
        stdout(&out)
    );
    assert!(env.fake.seen().is_empty());

    let out = env.run_stdin(b"", &["--json", "cat", "--paths-from=-"]);
    ok(&out);
    assert_eq!(doc(&out), files_doc(vec![], &[]));
    assert!(env.fake.raw_seen().is_empty());

    env.fake.clear();
    let bare = tempfile::tempdir().unwrap();
    let out = env
        .command(
            bare.path(),
            &["--json", "cat", "--if-repo", "--paths-from=-"],
        )
        .write_stdin(b"a.md\n".to_vec())
        .output()
        .unwrap();
    ok(&out);
    assert_eq!(
        doc(&out),
        json!({"skipped": true, "reason": "no_syns_repo"})
    );
    assert!(env.fake.seen().is_empty());
}

// Row 9: 16 in flight.
#[test]
#[serial]
fn r09_sixteen_in_flight() {
    let env = Env::new();
    env.fake.with(|s| s.hold = Duration::from_millis(200));
    let paths: Vec<String> = (0..40).map(|i| format!("f{i:02}.md")).collect();
    for p in &paths {
        env.fake.put(p, p.as_bytes());
    }
    let mut args: Vec<&str> = vec!["--json", "cat", "--"];
    args.extend(paths.iter().map(String::as_str));
    let out = env.run(&args);
    ok(&out);
    assert_eq!(doc(&out)["files"].as_array().unwrap().len(), 40);
    let (max, first, last) = env.fake.with(|s| {
        let first = s
            .seen
            .iter()
            .filter(|r| r.raw_path().is_some())
            .map(|r| r.at)
            .min()
            .unwrap();
        (s.max_in_flight, first, *s.answered.iter().max().unwrap())
    });
    assert!(max <= 16, "{max} in flight");
    assert!(max >= 8, "the reads ran together: {max}");
    let took = last - first;
    assert!(took < Duration::from_secs(2), "{took:?}");
    assert!(took >= Duration::from_millis(600), "{took:?}");
}

// Row 10: a path asked twice is read once.
#[test]
fn r10_a_path_asked_twice_is_read_once() {
    let env = Env::new();
    env.fake.put("a.md", b"a\n");
    let out = env.run(&["--json", "cat", "--", "a.md", "./a.md", "a.md"]);
    ok(&out);
    assert_eq!(
        doc(&out),
        files_doc(
            vec![
                text_entry("a.md", b"a\n"),
                text_entry("./a.md", b"a\n"),
                text_entry("a.md", b"a\n"),
            ],
            &[]
        )
    );
    assert_eq!(env.fake.raw_seen().len(), 1);
}

// ---- Per-file errors and run-wide failures ---------------------------------

// Row 11: missing.
#[test]
fn r11_missing() {
    let env = Env::new();
    env.fake.put("a.md", b"a\n");
    env.fake.put("b.md", b"b\n");
    let out = env.run(&["--json", "cat", "--", "a.md", "missing.md", "b.md"]);
    ok(&out);
    assert_eq!(
        doc(&out),
        files_doc(
            vec![
                text_entry("a.md", b"a\n"),
                error_entry(
                    "missing.md",
                    "NOT_FOUND",
                    &format!("path not found at version {HEAD_VERSION}: missing.md")
                ),
                text_entry("b.md", b"b\n"),
            ],
            &[]
        )
    );
}

// Row 12: hash mismatch.
#[test]
fn r12_hash_mismatch() {
    let env = Env::new();
    env.fake.put("a.md", b"a\n");
    env.fake.put("lie.md", b"lie\n");
    env.fake.always("lie.md", Act::WrongEtag);
    let out = env.run(&["--json", "cat", "--", "a.md", "lie.md"]);
    ok(&out);
    assert_eq!(
        doc(&out)["files"][1],
        error_entry(
            "lie.md",
            "RESPONSE_UNDECODABLE",
            &format!(
                "invalid response body: lie.md: expected {}, got {}",
                "0".repeat(40),
                blob(b"lie\n")
            )
        )
    );
    let blobs = env.blobs();
    assert!(!blobs.contains_key(&blob(b"lie\n")));
    assert!(!blobs.contains_key(&"0".repeat(40)));
    assert!(!env.hints().contains("lie.md"), "{}", env.hints());
}

// Row 13: other per-file classes.
#[test]
fn r13_other_per_file_classes() {
    let env = Env::new();
    env.fake.put("a.md", b"a\n");
    env.fake.put("gone.md", b"g\n");
    env.fake.always("gone.md", Act::Close);
    env.fake.always(
        "boom.md",
        Act::Refuse {
            status: 500,
            error: "internal_error",
            retry_after: None,
        },
    );
    env.fake.always(
        "bad.md",
        Act::Refuse {
            status: 422,
            error: "validation_error",
            retry_after: None,
        },
    );
    let out = env.run(&[
        "--json", "cat", "--", "a.md", "gone.md", "boom.md", "bad.md",
    ]);
    ok(&out);
    let files = doc(&out)["files"].clone();
    assert_eq!(files[0], text_entry("a.md", b"a\n"));
    assert_eq!(files[1]["error"]["code"], json!("SERVER_UNREACHABLE"));
    assert_eq!(files[2]["error"]["code"], json!("INTERNAL_ERROR"));
    assert_eq!(files[3]["error"]["code"], json!("VALIDATION_ERROR"));
    assert_eq!(
        env.fake
            .raw_seen()
            .iter()
            .filter(|r| r.raw_path() == Some("gone.md"))
            .count(),
        4,
        "resent up to READ_ATTEMPTS"
    );
}

// Row 14: run-wide failures write the error envelope at their own exit.
#[test]
fn r14_run_wide_failures() {
    let env = Env::new();
    env.fake.put("a.md", b"a\n");
    env.fake.put("b.md", b"b\n");

    let bare = tempfile::tempdir().unwrap();
    let out = env.run_in(bare.path(), &["--json", "cat", "--", "a.md", "b.md"]);
    assert_eq!(code(&out), 2);
    assert!(
        doc(&out)["error"]
            .as_str()
            .unwrap()
            .starts_with("cannot determine repo identity")
    );

    // A folder out of place.
    let w = std::fs::canonicalize(env.work.path()).unwrap();
    let moved = w.join("archive/q3");
    std::fs::create_dir_all(&moved).unwrap();
    std::fs::write(
        moved.join(".syns.yaml"),
        "holder: alice/notes\npath: clients/q3\n",
    )
    .unwrap();
    let out = env.run_in(&moved, &["--json", "cat", "--", "a.md", "b.md"]);
    assert_eq!(code(&out), 2, "{}", stdout(&out));
    assert!(
        doc(&out)["error"]
            .as_str()
            .unwrap()
            .starts_with("folder out of place")
    );

    let out = env.run(&["--json", "cat", "--version=999999", "--", "a.md", "b.md"]);
    assert_eq!(code(&out), 1);
    assert_eq!(
        doc(&out),
        json!({"code": "version_not_found", "error": "version not found: 999999"})
    );

    for (act, exit, line, wire) in [
        (
            Act::Refuse {
                status: 401,
                error: "unauthorized",
                retry_after: None,
            },
            1,
            "authentication required \u{2014} run 'syns login' first".to_string(),
            "auth_required",
        ),
        (
            Act::Refuse {
                status: 404,
                error: "repo_not_found",
                retry_after: None,
            },
            1,
            "server error (404): repo_not_found".to_string(),
            "repo_not_found",
        ),
        (
            Act::Refuse {
                status: 403,
                error: "forbidden",
                retry_after: None,
            },
            1,
            "server error (403): forbidden \u{2014} forbidden".to_string(),
            "forbidden",
        ),
    ] {
        env.fake.always("b.md", act);
        let out = env.run(&["--json", "cat", "--", "a.md", "b.md"]);
        assert_eq!(code(&out), exit, "{}", stdout(&out));
        // Issue 160: the cause's code beside `error`.
        assert_eq!(doc(&out), json!({"code": wire, "error": line}));
    }

    env.fake.with(|s| s.reference_closes = true);
    let out = env.run(&["--json", "cat", "--", "a.md", "b.md"]);
    assert_eq!(code(&out), 3, "{}", stdout(&out));
    assert!(
        doc(&out)["error"]
            .as_str()
            .unwrap()
            .starts_with("could not reach server")
    );
}

// Row 15: a `503` with `Retry-After: 1` inside a batch (IN-0041), for
// roadmap/271.
#[test]
#[serial]
fn r15_a_503_with_retry_after_inside_a_batch_for_roadmap_271() {
    let env = Env::new();
    let paths: Vec<String> = (1..=20).map(|i| format!("p{i:02}.md")).collect();
    for p in &paths {
        env.fake.put(p, p.as_bytes());
        // Each answer takes a moment, so the slots turn over the wait.
        env.fake
            .with(|s| s.delays.insert(p.clone(), Duration::from_millis(50)));
    }
    env.fake.once(
        "p01.md",
        Act::Refuse {
            status: 503,
            error: "engine_busy",
            retry_after: Some(1),
        },
    );
    let mut args: Vec<&str> = vec!["--json", "cat", "--"];
    args.extend(paths.iter().map(String::as_str));
    let started = Instant::now();
    let out = env.run(&args);
    ok(&out);
    assert!(started.elapsed() >= Duration::from_secs(1));
    let files = doc(&out)["files"].clone();
    for (i, p) in paths.iter().enumerate() {
        assert_eq!(files[i], text_entry(p, p.as_bytes()), "{p}");
    }
    let raws = env.fake.raw_seen();
    let first: Vec<&Seen> = raws
        .iter()
        .filter(|r| r.raw_path() == Some("p01.md"))
        .collect();
    assert_eq!(first.len(), 2);
    assert!(first[1].at - first[0].at >= Duration::from_millis(990));
    for late in ["p17.md", "p18.md", "p19.md", "p20.md"] {
        let sent = raws.iter().find(|r| r.raw_path() == Some(late)).unwrap();
        assert!(
            sent.at < first[1].at,
            "{late} was sent before p01.md's resend"
        );
    }

    for (status, error, code_name) in [
        (503, "engine_busy", "ENGINE_BUSY"),
        (429, "rate_limited", "RATE_LIMITED"),
    ] {
        env.fake.clear();
        env.fake.always(
            "p02.md",
            Act::Refuse {
                status,
                error,
                retry_after: Some(0),
            },
        );
        let out = env.run(&["--json", "cat", "--", "p01.md", "p02.md"]);
        ok(&out);
        assert_eq!(doc(&out)["files"][1]["error"]["code"], json!(code_name));
        assert_eq!(
            env.fake
                .raw_seen()
                .iter()
                .filter(|r| r.raw_path() == Some("p02.md"))
                .count(),
            4
        );
    }
}

// ---- --max-bytes and deferred --------------------------------------------

// Row 16: the C1 sample.
#[test]
fn r16_the_c1_sample() {
    let env = Env::new();
    for (name, size) in [("a.md", 18342), ("b.md", 40211), ("c.md", 7000)] {
        env.fake.put(name, &vec![b'x'; size]);
    }
    let out = env.run(&[
        "--json",
        "cat",
        "--max-bytes=60000",
        "--",
        "a.md",
        "b.md",
        "c.md",
    ]);
    ok(&out);
    let document = doc(&out);
    let files: Vec<&str> = document["files"]
        .as_array()
        .unwrap()
        .iter()
        .map(|f| f["path"].as_str().unwrap())
        .collect();
    assert_eq!(files, ["a.md", "b.md"]);
    assert_eq!(document["deferred"], json!(["c.md"]));
}

// Row 17: decided from `Content-Length`, in order, with `identity` asked.
#[test]
#[serial]
fn r17_decided_from_content_length_in_order() {
    let env = Env::new();
    env.fake.put("a.md", &[b'a'; 100]);
    env.fake.put("big.bin", &[b'b'; 50]);
    env.fake.always("big.bin", Act::Endless);
    env.fake.put("c.md", b"c");
    let started = Instant::now();
    let out = env.run(&[
        "--json",
        "cat",
        "--max-bytes=1000",
        "--",
        "a.md",
        "big.bin",
        "c.md",
    ]);
    ok(&out);
    assert!(started.elapsed() < Duration::from_secs(5));
    assert_eq!(
        doc(&out),
        files_doc(vec![text_entry("a.md", &[b'a'; 100])], &["big.bin", "c.md"])
    );
    for raw in env.fake.raw_seen() {
        assert_eq!(raw.header("accept-encoding"), Some("identity"), "{raw:?}");
    }
    assert_eq!(
        env.blobs().keys().cloned().collect::<Vec<_>>(),
        [blob(&[b'a'; 100])]
    );

    env.fake.clear();
    ok(&env.run(&["--json", "cat", "--", "a.md", "c.md"]));
    for raw in env.fake.raw_seen() {
        assert_eq!(raw.header("accept-encoding"), Some("zstd, gzip"), "{raw:?}");
    }
}

// Row 18: deferred paths are never fetched.
#[test]
fn r18_deferred_paths_are_never_fetched() {
    let env = Env::new();
    let paths: Vec<String> = (0..40).map(|i| format!("f{i:02}.md")).collect();
    for p in &paths {
        env.fake.put(p, p.as_bytes());
    }
    let mut args: Vec<&str> = vec!["--json", "cat", "--max-bytes=0", "--"];
    args.extend(paths.iter().map(String::as_str));
    let out = env.run(&args);
    ok(&out);
    let document = doc(&out);
    assert_eq!(document["files"], json!([]));
    assert_eq!(document["deferred"], json!(paths));
    assert!(
        env.fake.raw_seen().len() <= 16,
        "{}",
        env.fake.raw_seen().len()
    );
}

// Row 19: an undeclared length is read and counted.
#[test]
fn r19_undeclared_length() {
    let env = Env::new();
    env.fake.put("a.md", &[b'a'; 40]);
    env.fake.put("chunked.md", &[b'c'; 80]);
    env.fake.always("chunked.md", Act::Chunked);
    env.fake.put("z.md", b"z");
    let out = env.run(&[
        "--json",
        "cat",
        "--max-bytes=100",
        "--",
        "a.md",
        "chunked.md",
        "z.md",
    ]);
    ok(&out);
    assert_eq!(
        doc(&out),
        files_doc(
            vec![text_entry("a.md", &[b'a'; 40])],
            &["chunked.md", "z.md"]
        )
    );
    assert!(env.blobs().contains_key(&blob(&[b'c'; 80])));
}

// REVIEW-syns-cli finding 1: under `--max-bytes`, a refused path counts 0
// as soon as its status line arrives, so the paths after it are decided —
// and their bodies read — while its own refusal body is still on the way;
// an error answer never waits behind bodies left unread. Each later body
// here outgrows the socket buffers, so the listener's write of it ends only
// once the CLI reads it.
#[test]
#[serial]
fn r19b_a_refusal_counts_zero_at_its_status_line() {
    let env = Env::new();
    let big = vec![b'b'; 24 << 20];
    env.fake.put("b.bin", &big);
    env.fake.put("c.md", b"c\n");
    env.fake.always(
        "a.md",
        Act::SlowRefusal {
            status: 404,
            error: "not_found",
            after: Duration::from_secs(3),
        },
    );
    let out = env.run(&[
        "--json",
        "cat",
        "--max-bytes=100000000",
        "--",
        "a.md",
        "b.bin",
        "c.md",
    ]);
    ok(&out);
    let document = doc(&out);
    assert_eq!(document["files"][0]["error"]["code"], json!("NOT_FOUND"));
    assert_eq!(document["files"][1]["size"], json!(big.len()));
    assert_eq!(document["deferred"], json!([]));
    let finished = env.fake.with(|s| s.finished.clone());
    let at = |p: &str| {
        finished
            .iter()
            .find(|(route, _)| route.ends_with(&format!("/raw/{p}")))
            .map(|(_, at)| *at)
            .unwrap_or_else(|| panic!("{p} answered: {finished:?}"))
    };
    assert!(
        at("b.bin") < at("a.md"),
        "b.bin's body was read while a.md's refusal body was still held"
    );
}

// Row 20: what counts: each repetition, a `304` at the stored length, an
// error at 0.
#[test]
fn r20_what_counts() {
    let env = Env::new();
    env.fake.put("a.md", &[b'a'; 100]);
    env.fake.put("b.md", &[b'b'; 50]);
    let args = |n: &'static str| {
        vec![
            "--json",
            "cat",
            n,
            "--",
            "a.md",
            "a.md",
            "missing.md",
            "b.md",
        ]
    };
    for _warm in 0..2 {
        let out = env.run(&args("--max-bytes=250"));
        ok(&out);
        assert_eq!(doc(&out)["deferred"], json!([]));
        assert_eq!(doc(&out)["files"].as_array().unwrap().len(), 4);
        let out = env.run(&args("--max-bytes=249"));
        ok(&out);
        assert_eq!(doc(&out)["deferred"], json!(["b.md"]));
    }
    assert!(env.fake.raw_seen().iter().any(|r| r.status == 304));
}

// ---- The store -------------------------------------------------------------

// Row 21: layout and modes.
#[test]
fn r21_layout_and_modes() {
    let env = Env::on(Fake::bind(18278));
    env.fake.put("a.md", b"a\n");
    env.fake.put_typed("logo.png", PNG, "image/png");
    ok(&env.run(&["--json", "cat", "--", "a.md", "logo.png"]));
    let key = env.server_key();
    let store = format!("store/{key}/u-alice");
    let mut expected = vec![
        "store/".to_string(),
        format!("store/{key}/"),
        format!("{store}/"),
        format!("{store}/blobs/"),
        format!("{store}/blobs/{}", blob(b"a\n")),
        format!("{store}/blobs/{}", blob(PNG)),
        format!("{store}/hints/"),
        format!("{store}/hints/alice/"),
        format!("{store}/hints/alice/notes"),
        format!("{store}/lock"),
        format!("{store}/size"),
    ];
    expected.sort();
    assert_eq!(tree_of(env.cache.path()), expected);
    assert_eq!(env.hints().lines().count(), 2, "{}", env.hints());
    assert_eq!(env.size_file(), (2 + PNG.len()) as u64);
    #[cfg(unix)]
    for entry in &expected {
        let path = env.cache.path().join(entry.trim_end_matches('/'));
        let want = if entry.ends_with('/') { 0o700 } else { 0o600 };
        assert_eq!(mode_of(&path), want, "{entry}");
    }

    env.signed_out();
    ok(&env.run(&["--json", "cat", "--", "a.md", "logo.png"]));
    assert!(
        env.account("anonymous")
            .join("blobs")
            .join(blob(b"a\n"))
            .is_file()
    );

    env.credential(None);
    ok(&env.run(&["--json", "cat", "--", "a.md", "logo.png"]));
    use sha2::Digest;
    let digest = sha2::Sha256::digest(b"test-token");
    let hex: String = digest.iter().map(|b| format!("{b:02x}")).collect();
    assert!(
        env.account(&format!("t-{}", &hex[..16]))
            .join("size")
            .is_file()
    );

    // A second server shares nothing with the first.
    let other = Fake::start();
    other.put("a.md", b"a\n");
    env.credential(Some("alice"));
    let out = env
        .command_on(
            &other.uri,
            env.work.path(),
            &["--json", "cat", "--", "a.md", "a.md"],
        )
        .output()
        .unwrap();
    ok(&out);
    let second = env
        .cache
        .path()
        .join("store")
        .join(format!("127.0.0.1-{}", other.port))
        .join("u-alice");
    assert!(second.join("blobs").join(blob(b"a\n")).is_file());
    assert_eq!(other.raw_seen()[0].header("if-none-match"), None);
}

// Row 22: admission.
#[test]
fn r22_admission() {
    let env = Env::new();
    env.fake.put("a.md", b"a\n");
    env.fake.put("p.png", PNG);
    env.fake.put("bare.md", b"bare\n");
    env.fake.always("bare.md", Act::NoEtag);
    let out = env.run(&["--json", "cat", "--", "a.md", "p.png", "bare.md"]);
    ok(&out);
    assert_eq!(doc(&out)["files"][2], text_entry("bare.md", b"bare\n"));
    let blobs = env.blobs();
    assert_eq!(
        blobs.get(&blob(b"a\n")).map(Vec::as_slice),
        Some(&b"a\n"[..])
    );
    assert_eq!(blobs.get(&blob(PNG)).map(Vec::as_slice), Some(PNG));
    assert!(!blobs.contains_key(&blob(b"bare\n")));
}

// Row 23: the second identical read moves only `304`s.
#[test]
fn r23_the_second_identical_read_moves_only_304s() {
    let env = Env::new();
    let paths: Vec<String> = (0..16).map(|i| format!("d/f{i:02}.md")).collect();
    for p in &paths {
        env.fake.put(p, format!("content of {p}\n").as_bytes());
    }
    let mut args: Vec<&str> = vec!["--json", "cat", "--"];
    args.extend(paths.iter().map(String::as_str));
    let cold = env.run(&args);
    ok(&cold);
    env.fake.clear();
    let warm = env.run(&args);
    ok(&warm);
    assert_eq!(cold.stdout, warm.stdout);
    let raws = env.fake.raw_seen();
    assert_eq!(raws.len(), 16);
    for raw in &raws {
        let p = raw.raw_path().unwrap();
        assert_eq!(
            raw.header("if-none-match"),
            Some(format!("\"{}\"", blob(format!("content of {p}\n").as_bytes())).as_str())
        );
        assert_eq!(raw.status, 304);
    }
}

// Row 24: a hint survives a push.
#[test]
fn r24_a_hint_survives_a_push() {
    let env = Env::new();
    env.fake.put("a.md", b"a\n");
    env.fake.put("b.md", b"b\n");
    ok(&env.run(&["--json", "cat", "--", "a.md", "b.md"]));
    env.fake.put("b.md", b"b changed\n");
    env.fake.with(|s| {
        s.head_version = 8;
        s.head_sha = "8888888888888888888888888888888888888888".into();
    });
    env.fake.clear();
    let out = env.run(&["--json", "cat", "--", "a.md", "b.md"]);
    ok(&out);
    assert_eq!(doc(&out)["files"][1], text_entry("b.md", b"b changed\n"));
    let raws = env.fake.raw_seen();
    let status = |p: &str| {
        raws.iter()
            .find(|r| r.raw_path() == Some(p))
            .unwrap()
            .status
    };
    assert_eq!(status("a.md"), 304);
    assert_eq!(status("b.md"), 200);
    assert!(raws.iter().all(|r| r.query("ref").as_deref() == Some("8")));
}

// Row 25: a tampered, truncated or deleted blob is removed and read again.
#[test]
fn r25_tampered_truncated_deleted() {
    let a: &[u8] = b"the bytes of a\n";
    for damage in ["tampered", "truncated", "deleted"] {
        let env = Env::new();
        env.fake.put("a.md", a);
        env.fake.put("b.md", b"b\n");
        let cold = env.run(&["--json", "cat", "--", "a.md", "b.md"]);
        ok(&cold);
        let entry = env.store().join("blobs").join(blob(a));
        match damage {
            "tampered" => std::fs::write(&entry, b"the bytes of A\n").unwrap(),
            "truncated" => std::fs::write(&entry, b"the bytes").unwrap(),
            _ => std::fs::remove_file(&entry).unwrap(),
        }
        env.fake.clear();
        let warm = env.run(&["--json", "cat", "--", "a.md", "b.md"]);
        ok(&warm);
        assert_eq!(cold.stdout, warm.stdout, "{damage}");
        let raws = env.fake.raw_seen();
        let of_a: Vec<&Seen> = raws
            .iter()
            .filter(|r| r.raw_path() == Some("a.md"))
            .collect();
        assert_eq!(of_a.len(), 1, "{damage}: one request for a.md");
        assert_eq!(of_a[0].header("if-none-match"), None, "{damage}");
        assert_eq!(of_a[0].status, 200, "{damage}");
        let of_b = raws.iter().find(|r| r.raw_path() == Some("b.md")).unwrap();
        assert_eq!(of_b.status, 304, "{damage}");
        assert_eq!(std::fs::read(&entry).unwrap(), a, "{damage}: stored again");
        // VERIFY.md F-1: `size` is the true total of `blobs/` after a blob
        // short or altered; one removed by hand stays counted until the
        // next sweep (REVIEW-delta-2 finding 2, `size` drift).
        let total: u64 = env.blobs().values().map(|b| b.len() as u64).sum();
        let drift = if damage == "deleted" {
            a.len() as u64
        } else {
            0
        };
        assert_eq!(env.size_file(), total + drift, "{damage}: size");
    }
}

// D-242 §4: a `304` naming another hash than the one sent is no answer;
// the path is asked again with no header.
#[test]
fn r25_a_304_naming_another_hash_is_asked_again() {
    let env = Env::new();
    env.fake.put("a.md", b"a\n");
    let cold = env.run(&["--json", "cat", "--", "a.md", "a.md"]);
    ok(&cold);
    env.fake.clear();
    env.fake.once("a.md", Act::OtherNotModified);
    let warm = env.run(&["--json", "cat", "--", "a.md", "a.md"]);
    ok(&warm);
    assert_eq!(cold.stdout, warm.stdout);
    let raws = env.fake.raw_seen();
    assert_eq!(raws.len(), 2);
    assert!(raws[0].header("if-none-match").is_some());
    assert_eq!(raws[1].header("if-none-match"), None);
    assert_eq!(raws[1].status, 200);
}

// Row 26: the hint is only a hint (D-211).
#[test]
fn r26_the_hint_is_only_a_hint() {
    let env = Env::new();
    let secret: &[u8] = b"stored secret bytes\n";
    env.fake.put("a.md", secret);
    env.fake.put("folder/x.md", b"x\n");
    ok(&env.run(&["--json", "cat", "--", "a.md", "folder/x.md"]));
    assert!(env.blobs().contains_key(&blob(secret)));
    assert!(env.hints().contains("\ta.md\n"));

    // Removed from the repository, or outside a grantee's folder: the
    // server answers not found whatever the hint names.
    env.fake.remove("a.md");
    env.fake.clear();
    let out = env.run(&["--json", "cat", "--", "a.md", "folder/x.md"]);
    ok(&out);
    assert_eq!(doc(&out)["files"][0]["error"]["code"], json!("NOT_FOUND"));
    assert!(!stdout(&out).contains("stored secret"));
    assert!(env.fake.raw_seen()[0].header("if-none-match").is_some());

    let one = env.run(&["cat", "a.md"]);
    assert_eq!(code(&one), 1);
    assert!(!stdout(&one).contains("stored secret"));
    let one = env.run(&["--json", "cat", "a.md"]);
    assert_eq!(code(&one), 1);
    assert!(!stdout(&one).contains("stored secret"));

    env.fake.put("a.md", secret);
    env.fake.always(
        "a.md",
        Act::Refuse {
            status: 404,
            error: "not_found",
            retry_after: None,
        },
    );
    let out = env.run(&["--json", "cat", "--", "a.md", "folder/x.md"]);
    ok(&out);
    assert_eq!(doc(&out)["files"][0]["error"]["code"], json!("NOT_FOUND"));
    assert!(!stdout(&out).contains("stored secret"));
}

// Row 27: grep and cat share hits.
#[test]
fn r27_grep_and_cat_share_hits() {
    let env = Env::new();
    env.fake.put("a.md", b"one X here\n");
    ok(&env.run(&["grep", "-n", "X", "--glob", "a.md"]));
    env.fake.clear();
    let out = env.run(&["cat", "a.md"]);
    ok(&out);
    assert_eq!(stdout(&out), "one X here\n");
    let raws = env.fake.raw_seen();
    assert_eq!(raws.len(), 1);
    assert!(raws[0].header("if-none-match").is_some());
    assert_eq!(raws[0].status, 304);

    // cat first, then grep: no content request for a.md.
    let env = Env::new();
    env.fake.put("a.md", b"one X here\n");
    env.fake.put("p.png", PNG);
    ok(&env.run(&["cat", "--", "a.md", "p.png"]));
    env.fake.clear();
    let out = env.run(&["--json", "grep", "-n", "X"]);
    ok(&out);
    let seen = env.fake.seen();
    assert!(
        seen.iter()
            .all(|r| r.files_path().is_none() && r.raw_path().is_none()),
        "{seen:?}"
    );
    assert!(seen.iter().any(|r| r.path == format!("{PREFIX}/tree")));
    let document = doc(&out);
    assert_eq!(document["matches"][0]["path"], json!("a.md"));
    assert_eq!(
        document["skipped"],
        json!([{"path": "p.png", "reason": "binary"}])
    );
}

// Row 28: grep checks on the way out.
#[test]
fn r28_grep_checks_on_the_way_out() {
    let env = Env::new();
    env.fake.put("a.md", b"one X here\nX again\n");
    let clean = env.run(&["--json", "grep", "-n", "X"]);
    ok(&clean);
    let entry = env
        .store()
        .join("blobs")
        .join(blob(b"one X here\nX again\n"));
    std::fs::write(&entry, b"one X here\nX AGAIN\n").unwrap();
    env.fake.clear();
    let again = env.run(&["--json", "grep", "-n", "X"]);
    ok(&again);
    assert_eq!(clean.stdout, again.stdout);
    let seen = env.fake.seen();
    assert_eq!(
        seen.iter()
            .filter(|r| r.files_path() == Some("a.md"))
            .count(),
        1
    );
    assert_eq!(std::fs::read(&entry).unwrap(), b"one X here\nX again\n");
}

// Row 29: `syns read` reads through the store.
#[test]
fn r29_read_reads_through_the_store() {
    let env = Env::new();
    let text: &[u8] = b"one\ntwo\nthree\n";
    env.fake.put("a.md", text);
    env.fake.put("clients/q3/n.md", text);
    let human = "     1\tone\n     2\ttwo\n     3\tthree\n";
    let document = |path: &str| {
        format!(
            "{{\n  \"commitSha\": \"{HEAD_SHA}\",\n  \"content\": \"one\\ntwo\\nthree\",\n  \"limit\": 2000,\n  \"offset\": 1,\n  \"path\": \"{path}\",\n  \"sha\": \"{}\",\n  \"size\": 14,\n  \"totalLines\": 3,\n  \"version\": {HEAD_VERSION}\n}}\n",
            blob(text)
        )
    };
    for _ in 0..2 {
        let out = env.run(&["read", "a.md"]);
        ok(&out);
        assert_eq!(stdout(&out), human);
        let out = env.run(&["--json", "read", "a.md"]);
        ok(&out);
        assert_eq!(stdout(&out), document("a.md"));
        let out = env.run(&["--json", "read", "a.md", "--repo", REPO]);
        ok(&out);
        assert_eq!(stdout(&out), document("a.md"));
    }
    let raws = env.fake.raw_seen();
    assert!(env.fake.seen().iter().all(|r| r.files_path().is_none()));
    assert_eq!(raws[0].status, 200);
    assert!(
        raws[1..]
            .iter()
            .all(|r| r.status == 304 && r.header("if-none-match").is_some())
    );

    // Inside a scoped folder: the path counted from the folder.
    let w = std::fs::canonicalize(env.work.path()).unwrap();
    std::fs::create_dir_all(w.join("clients/q3")).unwrap();
    std::fs::write(
        w.join("clients/q3/.syns.yaml"),
        "holder: alice/notes\npath: clients/q3\n",
    )
    .unwrap();
    let out = env.run_in(&w.join("clients/q3"), &["--json", "read", "n.md"]);
    ok(&out);
    assert_eq!(stdout(&out), document("n.md"));
    let out = env.run_in(&w.join("clients/q3"), &["read", "n.md"]);
    ok(&out);
    assert_eq!(stdout(&out), human);

    env.fake.put("p.png", PNG);
    let out = env.run(&["read", "p.png"]);
    assert_eq!(code(&out), 1);
    assert_eq!(
        stderr(&out),
        "error: cannot number content that is not text: p.png \u{2014} read it with syns cat p.png\n"
    );

    env.fake.put("lie.md", b"lie\n");
    env.fake.always("lie.md", Act::WrongEtag);
    let out = env.run(&["read", "lie.md"]);
    assert_eq!(code(&out), 1);
    assert_eq!(stdout(&out), "");
    assert_eq!(
        stderr(&out),
        format!(
            "error: server error (200): invalid response body: lie.md: expected {}, got {}\n",
            "0".repeat(40),
            blob(b"lie\n")
        )
    );
    assert!(!env.blobs().contains_key(&blob(b"lie\n")));
}

// Row 30: the running size, and the sweep past the cap.
#[test]
fn r30_the_running_size() {
    let env = Env::new();
    env.fake.put("seed.md", b"seed");
    ok(&env.run(&["--json", "cat", "seed.md"]));
    assert_eq!(env.size_file(), 4);
    // 5,000 planted bytes the index does not count, and an index of 500.
    let blobs = env.store().join("blobs");
    for i in 0..5 {
        std::fs::write(blobs.join(format!("{i}").repeat(40)), vec![b'p'; 1000]).unwrap();
    }
    std::fs::write(env.store().join("size"), "500").unwrap();
    env.fake.put("hundred.md", &[b'h'; 100]);
    ok(&env.run_env(&[("SYNS_CACHE_MAX_BYTES", "1000")], &["cat", "hundred.md"]));
    assert_eq!(env.size_file(), 600);
    assert_eq!(env.blobs().len(), 7, "nothing evicted");

    // A run taking the index past the cap sweeps.
    let env = Env::new();
    let (a, b, c): (&[u8], &[u8], &[u8]) = (&[b'a'; 300], &[b'b'; 300], &[b'c'; 500]);
    env.fake.put("a.md", a);
    env.fake.put("b.md", b);
    env.fake.put("c.md", c);
    let capped = [("SYNS_CACHE_MAX_BYTES", "1000")];
    ok(&env.run_env(&capped, &["cat", "a.md"]));
    ok(&env.run_env(&capped, &["cat", "b.md"]));
    let blobs = env.store().join("blobs");
    set_mtime(&blobs.join(blob(a)), Duration::from_secs(7200));
    set_mtime(&blobs.join(blob(b)), Duration::from_secs(3600));
    // a is read in the run before the sweep; b was admitted after a but
    // not read since.
    ok(&env.run_env(&capped, &["cat", "a.md"]));
    std::fs::write(blobs.join("tmp-old"), b"left by a killed run").unwrap();
    set_mtime(&blobs.join("tmp-old"), Duration::from_secs(7200));
    std::fs::write(blobs.join("tmp-young"), b"another run's admission").unwrap();
    set_mtime(&blobs.join("tmp-young"), Duration::from_secs(60));
    ok(&env.run_env(&capped, &["cat", "c.md"]));
    let left = env.blobs();
    assert!(left.contains_key(&blob(a)), "read in the previous run");
    assert!(
        !left.contains_key(&blob(b)),
        "the oldest-touched goes first"
    );
    assert!(left.contains_key(&blob(c)));
    assert!(!left.contains_key("tmp-old"));
    assert!(left.contains_key("tmp-young"));
    assert_eq!(env.size_file(), 800);
    assert!(env.size_file() <= 900);
}

// Row 32: the old grep cache goes at the first store open.
#[test]
fn r32_the_old_grep_cache_goes() {
    for verb in [vec!["cat", "a.md"], vec!["read", "a.md"], vec!["grep", "a"]] {
        let env = Env::new();
        env.fake.put("a.md", b"a\n");
        let old = env.cache.path().join("blobs");
        std::fs::create_dir_all(&old).unwrap();
        std::fs::write(old.join("1".repeat(40)), b"old").unwrap();
        let staging = env.cache.path().join("staging").join("123-abc");
        std::fs::create_dir_all(&staging).unwrap();
        std::fs::write(staging.join("0"), b"a running pull's").unwrap();
        std::fs::create_dir_all(env.cache.path().join("alice")).unwrap();
        std::fs::write(env.cache.path().join("alice/notes.json"), b"{}").unwrap();
        ok(&env.run(&verb));
        assert!(!old.exists(), "{verb:?}");
        assert_eq!(
            std::fs::read(staging.join("0")).unwrap(),
            b"a running pull's"
        );
        assert!(env.cache.path().join("alice/notes.json").is_file());
    }
}

// Row 33: an unusable store is skipped without a word.
#[cfg(unix)]
#[test]
fn r33_an_unusable_store_is_skipped() {
    // The account directory standing as a link.
    let env = Env::new();
    env.fake.put("a.md", b"a\n");
    env.fake.put("b.md", b"b\n");
    let elsewhere = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(env.store().parent().unwrap()).unwrap();
    std::os::unix::fs::symlink(elsewhere.path(), env.store()).unwrap();
    let reference = Env::new();
    reference.fake.put("a.md", b"a\n");
    reference.fake.put("b.md", b"b\n");
    let expected = reference.run(&["--json", "cat", "--", "a.md", "b.md"]);
    for _ in 0..2 {
        env.fake.clear();
        let out = env.run(&["--json", "cat", "--", "a.md", "b.md"]);
        ok(&out);
        assert_eq!(out.stdout, expected.stdout);
        assert_eq!(stderr(&out), "");
        assert!(env.fake.raw_seen().iter().all(|r| r.status == 200));
        let out = env.run(&["grep", "a"]);
        ok(&out);
        assert_eq!(stdout(&out), "a.md:a\n");
    }
    assert!(tree_of(elsewhere.path()).is_empty());

    // The root read-only.
    if unsafe { libc::geteuid() } != 0 {
        let env = Env::new();
        env.fake.put("a.md", b"a\n");
        env.fake.put("b.md", b"b\n");
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(env.cache.path(), std::fs::Permissions::from_mode(0o500)).unwrap();
        let out = env.run(&["--json", "cat", "--", "a.md", "b.md"]);
        let grep = env.run(&["grep", "a"]);
        std::fs::set_permissions(env.cache.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        ok(&out);
        assert_eq!(out.stdout, expected.stdout);
        assert_eq!(stderr(&out), "");
        ok(&grep);
        assert!(tree_of(env.cache.path()).is_empty());
    }
}

// Row 34: concurrent runs over one cold store.
#[test]
#[serial]
fn r34_concurrent_runs() {
    let env = Env::new();
    let paths: Vec<String> = (0..16).map(|i| format!("c{i:02}.md")).collect();
    let mut total = 0u64;
    for p in &paths {
        let bytes = format!("{p} {}\n", "x".repeat(100 + p.len()));
        total += bytes.len() as u64;
        env.fake.put(p, bytes.as_bytes());
    }
    let mut args: Vec<&str> = vec!["--json", "cat", "--"];
    args.extend(paths.iter().map(String::as_str));
    let children: Vec<std::process::Child> = (0..16)
        .map(|_| {
            let mut command = std::process::Command::new(assert_cmd::cargo::cargo_bin("syns"));
            command
                .current_dir(env.work.path())
                .env("SYNS_CONFIG_DIR", env.config.path())
                .env("SYNS_CACHE_DIR", env.cache.path())
                .env_remove("SYNS_URL")
                .arg("--server")
                .arg(&env.fake.uri)
                .args(&args)
                .stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::piped());
            command.spawn().unwrap()
        })
        .collect();
    let mut answers = Vec::new();
    for child in children {
        let out = child.wait_with_output().unwrap();
        ok(&out);
        answers.push(doc(&out));
    }
    for answer in &answers {
        assert_eq!(answer, &answers[0]);
        assert_eq!(answer["files"].as_array().unwrap().len(), 16);
    }
    let blobs = env.blobs();
    assert_eq!(blobs.len(), 16);
    for (name, bytes) in &blobs {
        assert_eq!(&blob(bytes), name);
    }
    assert_eq!(env.size_file(), total);
    let hints = env.hints();
    let mut lines: Vec<&str> = hints.lines().collect();
    assert_eq!(lines.len(), 16, "{hints}");
    lines.sort();
    lines.dedup();
    assert_eq!(lines.len(), 16);
    for line in lines {
        assert_eq!(line.split('\t').count(), 3, "{line}");
    }
}

// ---- syns logout --purge ---------------------------------------------------

#[cfg(unix)]
/// The three roots a purge walks, all temporary: the configured one, the
/// platform default under a temporary home, and the fallback under a
/// temporary `TMPDIR`.
struct Roots {
    home: TempDir,
    temp: TempDir,
}

#[cfg(unix)]
impl Roots {
    fn new() -> Roots {
        Roots {
            home: tempfile::tempdir().unwrap(),
            temp: tempfile::tempdir().unwrap(),
        }
    }

    fn default_root(&self) -> PathBuf {
        if cfg!(target_os = "macos") {
            self.home.path().join("Library/Caches/syns")
        } else {
            self.home.path().join(".cache/syns")
        }
    }

    #[cfg(unix)]
    fn fallback_root(&self) -> PathBuf {
        let euid = unsafe { libc::geteuid() };
        self.temp.path().join(format!("syns-{euid}"))
    }

    fn purge(&self, env: &Env, args: &[&str]) -> Output {
        env.command(env.work.path(), args)
            .env("HOME", self.home.path())
            .env("XDG_CACHE_HOME", self.home.path().join(".cache"))
            .env("TMPDIR", self.temp.path())
            .output()
            .unwrap()
    }
}

#[cfg(unix)]
/// A store of two accounts on two servers, and an old `blobs/`, under
/// `root`; answers the bytes it wrote.
fn plant_stores(root: &Path) -> u64 {
    let mut bytes = 0u64;
    for (server, account, size) in [
        ("syns.dev", "u-alice", 700usize),
        ("staging.syns.dev", "anonymous", 1300),
    ] {
        let dir = root.join("store").join(server).join(account);
        std::fs::create_dir_all(dir.join("blobs")).unwrap();
        std::fs::create_dir_all(dir.join("hints/alice")).unwrap();
        std::fs::write(dir.join("blobs").join("a".repeat(40)), vec![b'x'; size]).unwrap();
        std::fs::write(dir.join("hints/alice/notes"), b"line\n").unwrap();
        std::fs::write(dir.join("size"), size.to_string()).unwrap();
        std::fs::write(dir.join("lock"), b"").unwrap();
        bytes += size as u64 + 5 + size.to_string().len() as u64;
    }
    std::fs::create_dir_all(root.join("blobs")).unwrap();
    std::fs::write(root.join("blobs").join("b".repeat(40)), vec![b'y'; 1000]).unwrap();
    bytes + 1000
}

#[cfg(unix)]
fn human_size(bytes: u64) -> String {
    const UNITS: [&str; 3] = ["KiB", "MiB", "GiB"];
    if bytes < 1024 {
        return format!("{bytes} B");
    }
    let mut value = bytes as f64 / 1024.0;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    format!("{value:.1} {}", UNITS[unit])
}

#[cfg(unix)]
fn holds_store_files(root: &Path) -> Vec<String> {
    if !root.exists() {
        return Vec::new();
    }
    tree_of(root)
        .into_iter()
        .filter(|p| p.contains("store") || p.split('/').any(|s| s == "blobs"))
        .collect()
}

// Row 35: `--purge` leaves no store files under any of the three roots.
#[cfg(unix)]
#[test]
fn r35_purge_leaves_no_store_files() {
    let env = Env::new();
    let roots = Roots::new();
    let staging = env.cache.path().join("staging/1-x");
    std::fs::create_dir_all(&staging).unwrap();
    let mut planted = plant_stores(env.cache.path());
    planted += plant_stores(&roots.default_root());
    std::fs::create_dir_all(roots.fallback_root()).unwrap();
    planted += plant_stores(&roots.fallback_root());

    let out = roots.purge(&env, &["logout", "--purge"]);
    ok(&out);
    assert_eq!(
        stdout(&out),
        format!(
            "Logged out; removed the local content store ({})\n",
            human_size(planted)
        )
    );
    assert!(!env.config.path().join("credentials.json").exists());
    for root in [
        env.cache.path().to_path_buf(),
        roots.default_root(),
        roots.fallback_root(),
    ] {
        assert!(holds_store_files(&root).is_empty(), "{}", root.display());
    }
    assert!(staging.is_dir(), "a running pull's staging stands");

    // Under --json.
    let planted = plant_stores(env.cache.path())
        + plant_stores(&roots.default_root())
        + plant_stores(&roots.fallback_root());
    let out = roots.purge(&env, &["--json", "logout", "--purge"]);
    ok(&out);
    assert_eq!(
        doc(&out),
        json!({"message": "Logged out", "removedBytes": planted})
    );
}

// Row 36: with no credential and no store.
#[cfg(unix)]
#[test]
fn r36_purge_with_no_credential_and_no_store() {
    let env = Env::new();
    env.signed_out();
    let roots = Roots::new();
    let out = roots.purge(&env, &["logout", "--purge"]);
    ok(&out);
    assert_eq!(
        stdout(&out),
        "Logged out; removed the local content store (0 B)\n"
    );
    assert!(env.fake.seen().is_empty());
    assert!(
        !roots.fallback_root().exists(),
        "purge creates no fallback root"
    );
}

// Row 37: plain `syns logout` leaves every store file standing.
#[cfg(unix)]
#[test]
fn r37_plain_logout_leaves_the_store() {
    let env = Env::new();
    let roots = Roots::new();
    plant_stores(env.cache.path());
    let before = tree_of(env.cache.path());
    let out = roots.purge(&env, &["logout"]);
    ok(&out);
    assert_eq!(stdout(&out), "Logged out\n");
    assert_eq!(tree_of(env.cache.path()), before);
}

// Row 38: a link at `store/` is removed as a link.
#[cfg(unix)]
#[test]
fn r38_a_link_at_store_is_removed_as_a_link() {
    let env = Env::new();
    let roots = Roots::new();
    let target = tempfile::tempdir().unwrap();
    plant_stores(target.path());
    let before = tree_of(target.path());
    std::os::unix::fs::symlink(target.path().join("store"), env.cache.path().join("store"))
        .unwrap();
    std::os::unix::fs::symlink(target.path().join("blobs"), env.cache.path().join("blobs"))
        .unwrap();
    let out = roots.purge(&env, &["logout", "--purge"]);
    ok(&out);
    assert!(std::fs::symlink_metadata(env.cache.path().join("store")).is_err());
    assert!(std::fs::symlink_metadata(env.cache.path().join("blobs")).is_err());
    assert_eq!(tree_of(target.path()), before);
}

// Row 38a: a planted fallback root is not walked (THR-store-preplant).
#[cfg(unix)]
#[test]
fn r38a_a_planted_fallback_root_is_not_walked() {
    let env = Env::new();
    let roots = Roots::new();
    let victim = tempfile::tempdir().unwrap();
    plant_stores(victim.path());
    let before = tree_of(victim.path());
    std::os::unix::fs::symlink(victim.path(), roots.fallback_root()).unwrap();
    plant_stores(env.cache.path());
    plant_stores(&roots.default_root());
    let out = roots.purge(&env, &["logout", "--purge"]);
    ok(&out);
    assert_eq!(stdout(&out).lines().count(), 1, "{}", stdout(&out));
    assert_eq!(stderr(&out), "");
    assert_eq!(tree_of(victim.path()), before);
    assert!(holds_store_files(env.cache.path()).is_empty());
    assert!(holds_store_files(&roots.default_root()).is_empty());
}

// Row 39: a failed removal ends the run, and a re-run finishes it.
#[cfg(unix)]
#[test]
fn r39_a_failed_removal() {
    if unsafe { libc::geteuid() } == 0 {
        return;
    }
    use std::os::unix::fs::PermissionsExt;
    let env = Env::new();
    let roots = Roots::new();
    plant_stores(env.cache.path());
    let locked = env.cache.path().join("store/syns.dev/u-alice/blobs");
    std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o500)).unwrap();
    let out = roots.purge(&env, &["logout", "--purge"]);
    std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o700)).unwrap();
    assert_eq!(code(&out), 1);
    let prefix = format!(
        "error: could not remove the content store at {}: ",
        env.cache.path().join("store").display()
    );
    assert!(stderr(&out).starts_with(&prefix), "{}", stderr(&out));
    assert!(!env.config.path().join("credentials.json").exists());
    let again = roots.purge(&env, &["logout", "--purge"]);
    ok(&again);
    assert!(holds_store_files(env.cache.path()).is_empty());
}

// ---- Path and input edges (E9) ---------------------------------------------

/// The checkout with the folder `clients/q3` recording its own place.
fn with_folder(env: &Env) -> PathBuf {
    let w = std::fs::canonicalize(env.work.path()).unwrap();
    std::fs::create_dir_all(w.join("clients/q3")).unwrap();
    std::fs::write(
        w.join("clients/q3/.syns.yaml"),
        "holder: alice/notes\npath: clients/q3\n",
    )
    .unwrap();
    w.join("clients/q3")
}

// Row 40: `''`, `.` and `a/..` are their own error entries.
#[test]
fn r40_empty_dot_and_a_dotdot() {
    let env = Env::new();
    env.fake.put("a.md", b"a\n");
    env.fake.put("b.md", b"b\n");
    env.fake.put("clients/q3/a.md", b"qa\n");
    env.fake.put("clients/q3/b.md", b"qb\n");
    let folder = with_folder(&env);
    for (cwd, a, b) in [
        (env.work.path().to_path_buf(), &b"a\n"[..], &b"b\n"[..]),
        (folder, &b"qa\n"[..], &b"qb\n"[..]),
    ] {
        env.fake.clear();
        let out = env.run_in(
            &cwd,
            &["--json", "cat", "--", "a.md", "", ".", "a/..", "b.md"],
        );
        ok(&out);
        let empty = |p: &str| error_entry(p, "CONFIG_ERROR", "path cannot be empty");
        assert_eq!(
            doc(&out),
            files_doc(
                vec![
                    text_entry("a.md", a),
                    empty(""),
                    empty("."),
                    empty("a/.."),
                    text_entry("b.md", b)
                ],
                &[]
            )
        );
        assert_eq!(env.fake.raw_seen().len(), 2);
    }
}

// Row 41: `..`.
#[test]
fn r41_dotdot() {
    let env = Env::new();
    env.fake.put("a.md", b"a\n");
    env.fake.put("clients/q3/a.md", b"qa\n");
    let out = env.run(&["--json", "cat", "--", "a.md", "../x.md"]);
    ok(&out);
    assert_eq!(
        doc(&out)["files"][1],
        error_entry(
            "../x.md",
            "CONFIG_ERROR",
            "../x.md names no path inside the repository; name one counted from its root"
        )
    );
    let folder = with_folder(&env);
    env.fake.clear();
    let out = env.run_in(&folder, &["--json", "cat", "--", "a.md", "../x.md"]);
    ok(&out);
    assert_eq!(
        doc(&out)["files"][1],
        error_entry(
            "../x.md",
            "CONFIG_ERROR",
            &format!(
                "../x.md names no path inside the folder {}; name one counted from it",
                folder.display()
            )
        )
    );
    assert_eq!(env.fake.raw_seen().len(), 1);
}

// Row 43: a name starting with a digit is a path.
#[test]
fn r43_a_name_starting_with_a_digit() {
    let env = Env::new();
    env.fake.with(|s| {
        s.versions
            .push((3, "3333333333333333333333333333333333333333".into()))
    });
    env.fake.put("1.md", b"one\n");
    env.fake.put("2026/notes.md", b"notes\n");
    let out = env.run(&["--json", "cat", "--version=3", "1.md", "2026/notes.md"]);
    ok(&out);
    let document = doc(&out);
    assert_eq!(document["version"], json!(3));
    assert_eq!(document["files"][0], text_entry("1.md", b"one\n"));
    assert_eq!(
        document["files"][1],
        text_entry("2026/notes.md", b"notes\n")
    );
}

// Row 44: names the URL must encode keep their hint; a tab gets none.
#[test]
fn r44_names_the_url_must_encode() {
    let env = Env::new();
    let names = ["a b.md", "x#y.md", "q?.md", "100%.md", "é.md", "日本.md"];
    for name in names {
        env.fake.put(name, name.as_bytes());
    }
    env.fake.put("tab\there.md", b"tab");
    let mut args = vec!["--json", "cat", "--"];
    args.extend(names);
    args.push("tab\there.md");
    let cold = env.run(&args);
    ok(&cold);
    for (i, name) in names.iter().enumerate() {
        assert_eq!(doc(&cold)["files"][i], text_entry(name, name.as_bytes()));
        assert!(env.hints().contains(&format!("\t{name}\n")), "{name}");
    }
    assert_eq!(doc(&cold)["files"][6], text_entry("tab\there.md", b"tab"));
    assert_eq!(env.hints().lines().count(), names.len(), "{}", env.hints());
    env.fake.clear();
    let warm = env.run(&args);
    assert_eq!(cold.stdout, warm.stdout);
    for raw in env.fake.raw_seen() {
        let want = if raw.raw_path() == Some("tab\there.md") {
            200
        } else {
            304
        };
        assert_eq!(raw.status, want, "{raw:?}");
    }
}
