//! Binary-level behaviour of a scoped folder worked on as a working copy
//! of its own: sync, push, pull, status, the resolution verbs and every
//! write run inside it, and one folder checked out alone (SPEC u291
//! Tests).
//!
//! Every binary row of that table stands here under the name the table
//! gives it; `folder_base_narrows_the_holder_base` stands in the tests
//! module of `src/push/working_copy.rs` and
//! `a_folder_collected_alone_keeps_and_attributes_as_its_holder_root_does`
//! in that of `src/push/collector.rs`. Each test drives one deployment of
//! its own: a stateful mock answering the tree, raw, file, push and revert
//! entries of `alice/work` from the commits it holds, a config directory
//! holding a credential, a cache directory, and the checkout `W` naming
//! `alice/work` with the folder `W/clients/vela/q3-board` recording its own
//! place and an empty `sub` under it, `W` and the folder each converged at
//! `h1` by a `pull` run in it, unless its setup says otherwise.

use assert_cmd::Command as AssertCommand;
use base64::Engine as _;
use serde_json::{Value, json};
use serial_test::serial;
use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tempfile::TempDir;
use wiremock::matchers::any;
use wiremock::{Mock, MockServer, Request, Respond, ResponseTemplate};

const REPO: &str = "alice/work";
const FOLDER: &str = "clients/vela/q3-board";
const FOLDER_YAML: &str = "holder: alice/work\npath: clients/vela/q3-board\n";
const W_YAML: &str = "owner: alice\nname: work\n";
const X_H1: &str = "{\"x\":1}\n";
const BOARD_H1: &str = "{\"cards\":[]}\n";
const PREFIX: &str = "/api/v1/repos/alice/work";

/// The commit hash named for `n`: forty characters a write's `--parent`
/// takes as a full hash, so no version is resolved.
fn h(n: u32) -> String {
    format!("{n:x}").repeat(40)[..40].to_string()
}

fn blob_sha1(bytes: &[u8]) -> String {
    syns_cli::push::hash::blob_sha1(bytes)
}

fn in_folder(path: &str) -> String {
    format!("{FOLDER}/{path}")
}

/// The head `h1` the Tests preamble fixes, with the checkout's own
/// identity file beside it.
fn h1_tree() -> BTreeMap<String, Vec<u8>> {
    BTreeMap::from([
        (".syns.yaml".to_string(), W_YAML.as_bytes().to_vec()),
        (".page/x.json".to_string(), X_H1.as_bytes().to_vec()),
        (in_folder(".syns.yaml"), FOLDER_YAML.as_bytes().to_vec()),
        (in_folder(".page/board.json"), BOARD_H1.as_bytes().to_vec()),
    ])
}

// ---- the mock deployment ----------------------------------------------

/// What the mock holds: every commit, the head among them, and the
/// answers a test bends.
struct State {
    commits: Vec<(String, BTreeMap<String, Vec<u8>>)>,
    head: usize,
    /// Whether a push moves the head. A mock that does not still answers
    /// each push with a commit of its own and still serves that commit by
    /// its hash, but keeps serving the head it held — the shape of a mock
    /// answering every push alike that several rows' runs assume.
    advance: bool,
    next: u32,
    visibility: &'static str,
    /// Raw reads answered with a refusal instead: path to status and code.
    raw_refusals: HashMap<String, (u16, &'static str)>,
    /// `EP-file-read` answers by path.
    file_answers: HashMap<String, String>,
    /// The delay the first tree answer is held for.
    hold_first_tree: Option<Duration>,
    trees_answered: usize,
    /// SPEC u292: whether a push claiming a parent other than the head is
    /// refused naming it while a landing push leaves the head standing —
    /// the Tests preamble's mock, whose push lands at the head alone.
    pin_head: bool,
    /// SPEC u292: the commit the repository read answers in place of the
    /// head.
    repo_head: Option<usize>,
    /// SPEC u292: the commit a tree, raw or file read addressing no
    /// reference answers in place of the head.
    tip: Option<usize>,
    /// SPEC u292: whether each push is refused naming a new head one
    /// version past the head, changing `README.md` alone.
    race_each_push: bool,
    /// SPEC u292: whether the version list answers its newest version
    /// whatever `path` it is asked with.
    versions_unnarrowed: bool,
    /// SPEC u292: the refusal the version list asked with `path` answers.
    path_versions_refusal: Option<(u16, &'static str)>,
}

impl State {
    fn new(tree: BTreeMap<String, Vec<u8>>) -> State {
        State {
            commits: vec![(h(1), tree)],
            head: 0,
            advance: true,
            next: 2,
            visibility: "public",
            raw_refusals: HashMap::new(),
            file_answers: HashMap::new(),
            hold_first_tree: None,
            trees_answered: 0,
            pin_head: false,
            repo_head: None,
            tip: None,
            race_each_push: false,
            versions_unnarrowed: false,
            path_versions_refusal: None,
        }
    }

    fn commit_at(&self, reference: Option<&str>) -> Option<usize> {
        match reference {
            None => Some(self.tip.unwrap_or(self.head)),
            Some(reference) => self
                .commits
                .iter()
                .enumerate()
                .position(|(index, (sha, _))| {
                    sha == reference || (index + 1).to_string() == reference
                }),
        }
    }

    /// The paths the commit at `index` changed against the one before it,
    /// every path of the first commit.
    fn changed_at(&self, index: usize) -> Vec<String> {
        let tree = &self.commits[index].1;
        let empty = BTreeMap::new();
        let before = if index == 0 {
            &empty
        } else {
            &self.commits[index - 1].1
        };
        tree.keys()
            .chain(before.keys())
            .filter(|p| tree.get(*p) != before.get(*p))
            .cloned()
            .collect::<std::collections::BTreeSet<_>>()
            .into_iter()
            .collect()
    }

    /// The version-list entry of the commit at `index`.
    fn version_entry(&self, index: usize) -> Value {
        json!({
            "version": index + 1, "sha": self.commits[index].0, "parentSha": null,
            "message": "m", "messageBody": null, "author": "alice",
            "createdAt": "2026-01-01T00:00:00Z", "filesChanged": self.changed_at(index),
        })
    }

    /// Add a commit holding `tree` as the new head, answering its hash.
    fn set_head(&mut self, tree: BTreeMap<String, Vec<u8>>) -> String {
        let sha = h(self.next);
        self.next += 1;
        self.commits.push((sha.clone(), tree));
        self.head = self.commits.len() - 1;
        sha
    }
}

fn refusal(status: u16, error: &str) -> ResponseTemplate {
    ResponseTemplate::new(status).set_body_json(json!({ "error": error }))
}

fn query(request: &Request, key: &str) -> Option<String> {
    request
        .url
        .query_pairs()
        .find(|(k, _)| k == key)
        .map(|(_, v)| v.to_string())
}

fn decoded(path: &str) -> String {
    urlencoding::decode(path)
        .map(|p| p.to_string())
        .unwrap_or_else(|_| path.to_string())
}

struct Server(Arc<Mutex<State>>);

impl Respond for Server {
    fn respond(&self, request: &Request) -> ResponseTemplate {
        let mut state = self.0.lock().expect("state");
        let path = decoded(request.url.path());
        let Some(rest) = path.strip_prefix(PREFIX) else {
            return refusal(404, "not_found");
        };
        let method = request.method.as_str();
        if rest.is_empty() && method == "GET" {
            let (sha, tree) = &state.commits[state.repo_head.unwrap_or(state.head)];
            return ResponseTemplate::new(200).set_body_json(json!({
                "owner": "alice", "name": "work", "description": null,
                "commitSha": sha, "status": "active", "author": null, "tags": [],
                "visibility": state.visibility, "forkedFrom": null, "forkCount": 0,
                "fileCount": tree.len(), "role": null,
                "createdAt": "2026-01-01T00:00:00Z", "updatedAt": "2026-01-01T00:00:00Z",
            }));
        }
        if method == "GET" && (rest == "/tree" || rest.starts_with("/tree/")) {
            return tree_answer(&mut state, rest, request);
        }
        if method == "GET"
            && let Some(file) = rest.strip_prefix("/raw/")
        {
            if let Some((status, error)) = state.raw_refusals.get(file) {
                return refusal(*status, error);
            }
            let Some(at) = state.commit_at(query(request, "ref").as_deref()) else {
                return refusal(404, "ref_not_found");
            };
            return match state.commits[at].1.get(file) {
                Some(bytes) => ResponseTemplate::new(200).set_body_bytes(bytes.clone()),
                None => refusal(404, "not_found"),
            };
        }
        if method == "POST"
            && let Some(file) = rest
                .strip_prefix("/files/")
                .and_then(|r| r.strip_suffix("/revert"))
        {
            let _ = file;
            let (sha, _) = &state.commits[state.head];
            return ResponseTemplate::new(200).set_body_json(json!({
                "commitSha": sha, "version": state.head + 1, "filesChanged": 1, "created": false,
            }));
        }
        if method == "GET"
            && let Some(file) = rest.strip_prefix("/files/")
        {
            let content = match state.file_answers.get(file) {
                Some(content) => content.clone(),
                None => {
                    let Some(at) = state.commit_at(query(request, "ref").as_deref()) else {
                        return refusal(404, "ref_not_found");
                    };
                    match state.commits[at].1.get(file) {
                        Some(bytes) => String::from_utf8_lossy(bytes).to_string(),
                        None => return refusal(404, "not_found"),
                    }
                }
            };
            return ResponseTemplate::new(200).set_body_json(json!({
                "content": content, "sha": blob_sha1(content.as_bytes()), "size": content.len(),
            }));
        }
        if method == "GET" && rest == "/versions" {
            return versions_answer(&state, request);
        }
        if method == "GET"
            && let Some(reference) = rest.strip_prefix("/versions/")
        {
            let found =
                state.commits.iter().enumerate().find(|(index, (sha, _))| {
                    sha == reference || (index + 1).to_string() == reference
                });
            return match found {
                Some((index, (sha, tree))) => ResponseTemplate::new(200).set_body_json(json!({
                    "version": index + 1, "sha": sha, "parentSha": null, "message": "m",
                    "messageBody": null, "author": "alice",
                    "createdAt": "2026-01-01T00:00:00Z",
                    "filesChanged": tree.keys().collect::<Vec<_>>(),
                })),
                None => refusal(404, "ref_not_found"),
            };
        }
        if method == "PUT" && rest == "/push" {
            return push_answer(&mut state, request);
        }
        refusal(404, "not_found")
    }
}

fn tree_answer(state: &mut State, rest: &str, request: &Request) -> ResponseTemplate {
    let delay = match (state.trees_answered, state.hold_first_tree) {
        (0, Some(hold)) => Some(hold),
        _ => None,
    };
    state.trees_answered += 1;
    let Some(at) = state.commit_at(query(request, "ref").as_deref()) else {
        return refusal(404, "ref_not_found");
    };
    let (sha, tree) = &state.commits[at];
    let under = rest.strip_prefix("/tree/").unwrap_or("");
    let recursive = query(request, "recursive").as_deref() == Some("true");
    let within = |path: &str| under.is_empty() || path.starts_with(&format!("{under}/"));
    let mut entries: BTreeMap<String, Value> = BTreeMap::new();
    for (path, bytes) in tree.iter().filter(|(p, _)| within(p)) {
        let relative = if under.is_empty() {
            path.as_str()
        } else {
            &path[under.len() + 1..]
        };
        let name_of = |p: &str| p.rsplit('/').next().unwrap_or(p).to_string();
        if recursive || !relative.contains('/') {
            entries.insert(
                path.clone(),
                json!({ "name": name_of(path), "path": path, "type": "file",
                        "size": bytes.len(), "sha": blob_sha1(bytes) }),
            );
        } else {
            let first = relative.split('/').next().unwrap_or(relative);
            let dir = if under.is_empty() {
                first.to_string()
            } else {
                format!("{under}/{first}")
            };
            entries.insert(
                dir.clone(),
                json!({ "name": first, "path": dir, "type": "dir", "size": null, "sha": null }),
            );
        }
    }
    if !under.is_empty() && entries.is_empty() {
        return refusal(404, "not_found");
    }
    let answer = ResponseTemplate::new(200).set_body_json(json!({
        "entries": entries.into_values().collect::<Vec<_>>(),
        "commitSha": sha,
        "truncated": false,
    }));
    match delay {
        Some(delay) => answer.set_delay(delay),
        None => answer,
    }
}

/// `EP-versions`: the newest versions first, up to the head, narrowed to
/// the versions changing a path at or under `path` where one is asked.
fn versions_answer(state: &State, request: &Request) -> ResponseTemplate {
    let path = query(request, "path");
    if path.is_some()
        && let Some((status, error)) = state.path_versions_refusal
    {
        return refusal(status, error);
    }
    let limit: usize = query(request, "limit")
        .and_then(|l| l.parse().ok())
        .unwrap_or(20);
    let offset: usize = query(request, "offset")
        .and_then(|o| o.parse().ok())
        .unwrap_or(0);
    let listed: Vec<usize> = (0..=state.head)
        .rev()
        .filter(|index| match &path {
            Some(folder) if !state.versions_unnarrowed => state
                .changed_at(*index)
                .iter()
                .any(|p| p == folder || p.starts_with(&format!("{folder}/"))),
            _ => true,
        })
        .collect();
    let data: Vec<Value> = listed
        .iter()
        .skip(offset)
        .take(limit)
        .map(|index| state.version_entry(*index))
        .collect();
    ResponseTemplate::new(200).set_body_json(json!({
        "data": data, "total": listed.len(), "limit": limit, "offset": offset,
    }))
}

fn push_answer(state: &mut State, request: &Request) -> ResponseTemplate {
    let body: Value = serde_json::from_slice(&request.body).expect("a push body");
    if state.race_each_push {
        let mut tree = state.commits[state.head].1.clone();
        let raced = format!("# raced {}\n", state.next);
        tree.insert("README.md".to_string(), raced.into_bytes());
        let sha = state.set_head(tree);
        return ResponseTemplate::new(409).set_body_json(json!({
            "error": "conflict", "currentSha": sha,
        }));
    }
    let (head_sha, head_tree) = state.commits[state.head].clone();
    if let Some(parent) = body["parentSha"].as_str()
        && (state.advance || state.pin_head)
        && parent != head_sha
    {
        return ResponseTemplate::new(409).set_body_json(json!({
            "error": "conflict", "currentSha": head_sha,
        }));
    }
    let known: HashMap<String, Vec<u8>> = state
        .commits
        .iter()
        .flat_map(|(_, tree)| tree.values())
        .map(|bytes| (blob_sha1(bytes), bytes.clone()))
        .collect();
    let mut tree = head_tree.clone();
    let mut missing = serde_json::Map::new();
    for file in body["files"].as_array().into_iter().flatten() {
        let path = file["path"].as_str().expect("path").to_string();
        let sha = file["sha"].as_str().unwrap_or_default().to_string();
        let bytes = if let Some(content) = file["content"].as_str() {
            content.as_bytes().to_vec()
        } else if let Some(encoded) = file["contentBase64"].as_str() {
            base64::engine::general_purpose::STANDARD
                .decode(encoded)
                .expect("base64")
        } else if let Some(bytes) = known.get(&sha) {
            bytes.clone()
        } else {
            missing.insert(path.clone(), Value::from(sha));
            continue;
        };
        tree.insert(path, bytes);
    }
    if !missing.is_empty() {
        return ResponseTemplate::new(409)
            .set_body_json(json!({ "error": "missing_blobs", "missing": missing }));
    }
    for deletion in body["deletions"].as_array().into_iter().flatten() {
        let path = deletion["path"].as_str().expect("path");
        tree.retain(|p, _| p != path && !p.starts_with(&format!("{path}/")));
    }
    if tree == head_tree {
        return ResponseTemplate::new(200).set_body_json(json!({
            "commitSha": head_sha, "version": state.head + 1,
            "filesChanged": 0, "created": false,
        }));
    }
    let changed = tree
        .keys()
        .chain(head_tree.keys())
        .filter(|p| tree.get(*p) != head_tree.get(*p))
        .collect::<std::collections::BTreeSet<_>>()
        .len();
    let sha = h(state.next);
    state.next += 1;
    state.commits.push((sha.clone(), tree));
    if state.advance && !state.pin_head {
        state.head = state.commits.len() - 1;
    }
    ResponseTemplate::new(200).set_body_json(json!({
        "commitSha": sha, "version": state.commits.len(),
        "filesChanged": changed, "created": false,
    }))
}

/// One mock deployment, one config directory, one cache directory, and
/// the checkout `W` the Tests preamble fixes, each taken through
/// `std::fs::canonicalize` so every directory a line names is the one the
/// binary sees.
struct Deployment {
    rt: tokio::runtime::Runtime,
    server: MockServer,
    state: Arc<Mutex<State>>,
    home: TempDir,
    cache: TempDir,
    _work: TempDir,
    w: PathBuf,
    /// SPEC u292: the address the binary is pointed at in place of the
    /// mock's own.
    via: Option<String>,
}

impl Deployment {
    /// The deployment serving `tree` at `h1`, the checkout written, and
    /// nothing converged.
    fn with_head(tree: BTreeMap<String, Vec<u8>>) -> Deployment {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime");
        let server = rt.block_on(MockServer::start());
        let state = Arc::new(Mutex::new(State::new(tree)));
        rt.block_on(
            Mock::given(any())
                .respond_with(Server(state.clone()))
                .mount(&server),
        );
        let work = tempfile::tempdir().expect("working dir");
        let w = std::fs::canonicalize(work.path()).expect("canonical W");
        std::fs::write(w.join(".syns.yaml"), W_YAML).expect("W identity");
        std::fs::create_dir_all(w.join(FOLDER).join("sub")).expect("folder");
        std::fs::write(w.join(FOLDER).join(".syns.yaml"), FOLDER_YAML).expect("folder identity");
        let home = tempfile::tempdir().expect("config dir");
        std::fs::write(
            home.path().join("credentials.json"),
            json!({"token": "test-token", "username": "alice"}).to_string(),
        )
        .expect("credential");
        Deployment {
            rt,
            server,
            state,
            home,
            cache: tempfile::tempdir().expect("cache dir"),
            _work: work,
            w,
            via: None,
        }
    }

    /// The preamble's deployment: `W` and the folder each converged at
    /// `h1` by a `pull` run in it.
    fn converged() -> Deployment {
        Deployment::converged_over(h1_tree())
    }

    fn converged_over(tree: BTreeMap<String, Vec<u8>>) -> Deployment {
        let d = Deployment::with_head(tree);
        d.pull_in(&d.w.clone());
        d.pull_in(&d.folder());
        d
    }

    fn pull_in(&self, dir: &Path) {
        let out = self.run_in(dir, b"", &["pull"]);
        assert_eq!(exit_of(&out), 0, "setup pull: {}", stderr_of(&out));
    }

    fn folder(&self) -> PathBuf {
        self.w.join(FOLDER)
    }

    fn state(&self) -> std::sync::MutexGuard<'_, State> {
        self.state.lock().expect("state")
    }

    /// The head's tree with `changes` laid over it as a new head.
    fn advance_head(&self, changes: &[(&str, Option<&str>)]) -> String {
        let mut state = self.state();
        let mut tree = state.commits[state.head].1.clone();
        for (path, content) in changes {
            match content {
                Some(content) => {
                    tree.insert(path.to_string(), content.as_bytes().to_vec());
                }
                None => {
                    tree.remove(*path);
                }
            }
        }
        state.set_head(tree)
    }

    fn requests(&self) -> Vec<Request> {
        self.rt
            .block_on(async { self.server.received_requests().await.expect("requests") })
    }

    /// The number of requests received so far.
    fn mark(&self) -> usize {
        self.requests().len()
    }

    fn requests_since(&self, mark: usize) -> Vec<Request> {
        self.requests().into_iter().skip(mark).collect()
    }

    fn pushes_since(&self, mark: usize) -> Vec<Value> {
        self.requests_since(mark)
            .into_iter()
            .filter(|r| r.method.as_str() == "PUT" && r.url.path().ends_with("/push"))
            .map(|r| serde_json::from_slice(&r.body).expect("push body"))
            .collect()
    }

    fn trees_since(&self, mark: usize) -> Vec<Request> {
        self.requests_since(mark)
            .into_iter()
            .filter(|r| decoded(r.url.path()).starts_with(&format!("{PREFIX}/tree")))
            .collect()
    }

    fn command(&self, cwd: &Path, envs: &[(&str, &str)], args: &[&str]) -> AssertCommand {
        let mut command = AssertCommand::cargo_bin("syns").expect("syns binary");
        command
            .current_dir(cwd)
            .env("SYNS_CONFIG_DIR", self.home.path())
            .env("SYNS_CACHE_DIR", self.cache.path())
            // No global gitignore of the machine running the suite decides
            // what a collection keeps.
            .env("HOME", self.home.path())
            .env("XDG_CONFIG_HOME", self.home.path())
            .env_remove("SYNS_URL")
            .env_remove("SYNS_INTEGRATION")
            .env_remove("SYNS_RUN")
            .env_remove("SYNS_TRIGGER")
            .env_remove("SYNS_TASK");
        for (key, value) in envs {
            command.env(key, value);
        }
        let server = self.via.clone().unwrap_or_else(|| self.server.uri());
        command.arg("--server").arg(server).args(args);
        command
    }

    fn run_in(&self, cwd: &Path, stdin: &[u8], args: &[&str]) -> std::process::Output {
        self.run_with(cwd, &[], stdin, args)
    }

    fn run_with(
        &self,
        cwd: &Path,
        envs: &[(&str, &str)],
        stdin: &[u8],
        args: &[&str],
    ) -> std::process::Output {
        self.command(cwd, envs, args)
            .write_stdin(stdin.to_vec())
            .output()
            .expect("run syns")
    }

    /// The local record a registered publication or retrieval keeps.
    fn local_record(&self) -> PathBuf {
        self.cache.path().join("alice").join("work.json")
    }
}

/// An empty directory `U` with no identity file above it.
fn empty_u() -> (TempDir, PathBuf) {
    let dir = tempfile::tempdir().expect("U");
    let u = std::fs::canonicalize(dir.path()).expect("canonical U");
    assert!(
        u.ancestors().all(|a| !a.join(".syns.yaml").exists()),
        "an identity file stands above U"
    );
    (dir, u)
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

/// Every path a push body carries content for, or names as a deletion.
fn named(body: &Value) -> Vec<String> {
    let mut paths: Vec<String> = body["files"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|f| f.get("content").is_some() || f.get("contentBase64").is_some())
        .map(|f| f["path"].as_str().expect("path").to_string())
        .chain(
            body["deletions"]
                .as_array()
                .into_iter()
                .flatten()
                .map(|d| d["path"].as_str().expect("path").to_string()),
        )
        .collect();
    paths.sort();
    paths
}

/// Every path a push body carries at all.
fn carried(body: &Value) -> Vec<String> {
    body["files"]
        .as_array()
        .into_iter()
        .flatten()
        .chain(body["deletions"].as_array().into_iter().flatten())
        .map(|f| f["path"].as_str().expect("path").to_string())
        .collect()
}

fn entry<'a>(body: &'a Value, path: &str) -> Option<&'a Value> {
    body["files"]
        .as_array()
        .into_iter()
        .flatten()
        .find(|f| f["path"] == path)
}

fn snapshot(root: &Path) -> Vec<(PathBuf, Vec<u8>)> {
    let mut out = Vec::new();
    for entry in walk(root) {
        if entry.is_file() {
            out.push((entry.clone(), std::fs::read(&entry).expect("read")));
        }
    }
    out.sort();
    out
}

fn walk(root: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    for entry in std::fs::read_dir(root).into_iter().flatten().flatten() {
        let path = entry.path();
        out.push(path.clone());
        if path.is_dir() {
            out.extend(walk(&path));
        }
    }
    out
}

fn write(path: &Path, content: &str) {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).expect("parent");
    }
    std::fs::write(path, content).expect("write");
}

fn read(path: &Path) -> String {
    std::fs::read_to_string(path).expect("read")
}

// ---- the rows ----------------------------------------------------------

#[test]
#[serial]
fn sync_inside_a_folder_publishes_the_folder_alone() {
    let d = Deployment::converged();
    write(&d.w.join(".page/x.json"), "{\"x\":\"edited\"}\n");
    write(&d.folder().join(".page/board.json"), "{\"cards\":[1]}\n");
    let mark = d.mark();

    let out = d.run_with(
        &d.folder().join("sub"),
        &[
            ("SYNS_INTEGRATION", "bb"),
            ("SYNS_RUN", "r1"),
            ("SYNS_TRIGGER", "page"),
        ],
        b"",
        &["--json", "sync", "--if-repo"],
    );

    assert_eq!(exit_of(&out), 0, "{}", stderr_of(&out));
    assert_eq!(one_document(&out)["outcome"], "synced");
    let pushes = d.pushes_since(mark);
    assert_eq!(pushes.len(), 1, "{pushes:?}");
    let push = &pushes[0];
    assert_eq!(push["parentSha"], json!(h(1)));
    for file in push["files"].as_array().expect("files") {
        assert!(
            file["path"]
                .as_str()
                .unwrap()
                .starts_with(&format!("{FOLDER}/")),
            "{file}"
        );
    }
    assert_eq!(
        entry(push, &in_folder(".page/board.json")).expect("board.json")["content"],
        json!("{\"cards\":[1]}\n")
    );
    assert!(push.get("deletions").is_none(), "{push}");
    assert_eq!(push["provenance"]["integration"], "bb");
    assert_eq!(push["provenance"]["run"], "r1");
    assert_eq!(push["provenance"]["trigger"], "page");
    assert_eq!(read(&d.w.join(".page/x.json")), "{\"x\":\"edited\"}\n");

    let mark = d.mark();
    let out = d.run_in(&d.w, b"", &["sync"]);
    assert_eq!(exit_of(&out), 0, "{}", stderr_of(&out));
    let pushes = d.pushes_since(mark);
    assert_eq!(pushes.len(), 1, "{pushes:?}");
    assert_eq!(pushes[0]["parentSha"], json!(h(2)));
    assert_eq!(named(&pushes[0]), vec![".page/x.json".to_string()]);
}

#[test]
#[serial]
fn pull_inside_a_folder_writes_nothing_outside_it() {
    let d = Deployment::converged();
    let h2 = d.advance_head(&[
        (&in_folder("notes.md"), Some("notes at h2\n")),
        (".page/x.json", Some("{\"x\":2}\n")),
    ]);
    let mark = d.mark();

    let bare = d.run_in(&d.folder(), b"", &["pull"]);
    assert_eq!(exit_of(&bare), 0, "{}", stderr_of(&bare));
    let named_holder = d.run_in(&d.folder(), b"", &["pull", REPO]);
    assert_eq!(exit_of(&named_holder), 0, "{}", stderr_of(&named_holder));

    assert_eq!(read(&d.folder().join("notes.md")), "notes at h2\n");
    assert_eq!(read(&d.w.join(".page/x.json")), X_H1);
    for request in d.trees_since(mark) {
        let path = decoded(request.url.path());
        let folder_tree = path == format!("{PREFIX}/tree/{FOLDER}");
        let root_alone = path == format!("{PREFIX}/tree") && query(&request, "recursive").is_none();
        assert!(folder_tree || root_alone, "{}", request.url);
    }
    for request in d.requests_since(mark) {
        let path = decoded(request.url.path());
        if let Some(raw) = path.strip_prefix(&format!("{PREFIX}/raw/")) {
            assert!(raw.starts_with(&format!("{FOLDER}/")), "{path}");
        }
    }

    write(&d.folder().join("notes.md"), "notes edited\n");
    let mark = d.mark();
    let sync = d.run_in(&d.folder(), b"", &["sync"]);
    assert_eq!(exit_of(&sync), 0, "{}", stderr_of(&sync));
    let pushes = d.pushes_since(mark);
    assert_eq!(pushes.len(), 1, "{pushes:?}");
    assert_eq!(pushes[0]["parentSha"], json!(h2));
    assert_eq!(named(&pushes[0]), vec![in_folder("notes.md")]);
}

#[test]
#[serial]
fn pull_naming_another_repository_inside_a_folder_is_refused() {
    let d = Deployment::converged();
    let mark = d.mark();

    let out = d.run_in(&d.folder(), b"", &["pull", "bob/other"]);

    assert_eq!(exit_of(&out), 2, "{}", stderr_of(&out));
    assert!(d.requests_since(mark).is_empty());
    let err = stderr_of(&out);
    assert!(err.contains("already belongs to"), "{err}");
    assert!(err.contains(&d.folder().display().to_string()), "{err}");
    assert!(err.contains("alice/work"), "{err}");
}

#[test]
#[serial]
fn a_write_inside_a_folder_is_guarded_by_the_folder_alone() {
    let d = Deployment::converged();
    write(&d.w.join(".page/x.json"), "{\"x\":\"edited\"}\n");
    let parent = format!("--parent={}", h(1));
    let args = [
        "--json",
        "write",
        parent.as_str(),
        "--integration=bb",
        "--run=r1",
        "--trigger=page",
        "--",
        ".page/board.json",
    ];
    let mark = d.mark();

    let first = d.run_in(&d.folder(), b"{}", &args);
    assert_eq!(exit_of(&first), 0, "{}", stderr_of(&first));
    let pushes = d.pushes_since(mark);
    assert_eq!(pushes.len(), 1);
    assert_eq!(pushes[0]["parentSha"], json!(h(1)));
    assert_eq!(named(&pushes[0]), vec![in_folder(".page/board.json")]);

    write(&d.folder().join("notes.md"), "unpublished\n");
    let mark = d.mark();
    let second = d.run_in(&d.folder(), b"{}", &args);
    assert_eq!(exit_of(&second), 1, "{}", stderr_of(&second));
    let error = one_document(&second)["error"]
        .as_str()
        .expect("error")
        .to_string();
    assert!(
        error.contains("holds unpublished local changes for alice/work"),
        "{error}"
    );
    assert!(error.contains(&d.folder().display().to_string()), "{error}");
    assert!(d.pushes_since(mark).is_empty());
}

#[test]
#[serial]
fn the_guard_reads_the_holder_base_where_the_folder_records_none() {
    let d = Deployment::with_head(h1_tree());
    d.pull_in(&d.w.clone());
    let copies = d.cache.path().join("working-copies/alice/work");
    let standing = walk(&copies).len();
    let parent = h(1);

    let first = d.run_in(
        &d.folder(),
        b"{}",
        &["write", ".page/board.json", "--parent", &parent],
    );
    assert_eq!(exit_of(&first), 0, "{}", stderr_of(&first));
    assert_eq!(
        walk(&copies).len(),
        standing,
        "a state directory was created for the folder"
    );

    write(&d.folder().join(".page/board.json"), "{\"cards\":[9]}\n");
    let second = d.run_in(
        &d.folder(),
        b"{}",
        &["write", ".page/board.json", "--parent", &parent],
    );
    assert_eq!(exit_of(&second), 1, "{}", stderr_of(&second));
    let err = stderr_of(&second);
    assert!(
        err.contains(&format!(
            "the checkout at {} holds unpublished local changes for alice/work",
            d.folder().display()
        )),
        "{err}"
    );
}

#[test]
#[serial]
fn write_verbs_inside_a_folder_take_paths_counted_from_it() {
    let d = Deployment::converged();
    {
        let mut state = d.state();
        state.advance = false;
        state
            .file_answers
            .insert(in_folder(".page/board.json"), "a".to_string());
    }
    let parent = h(1);
    let mark = d.mark();

    for (args, stdin) in [
        (vec!["rm", "notes.md", "--parent", &parent], &b""[..]),
        (
            vec![
                "edit",
                ".page/board.json",
                "--old",
                "a",
                "--new",
                "b",
                "--parent",
                &parent,
            ],
            &b""[..],
        ),
        (
            vec!["commit", "--parent", &parent],
            &br#"{"files":[{"path":"a.md","content":"x"}],"deletions":[{"path":"b.md"}]}"#[..],
        ),
        (vec!["revert", ".page/board.json", "--to", "1"], &b""[..]),
    ] {
        let out = d.run_in(&d.folder(), stdin, &args);
        assert_eq!(exit_of(&out), 0, "{args:?}: {}", stderr_of(&out));
    }
    let pushes = d.pushes_since(mark);
    assert_eq!(pushes.len(), 3, "{pushes:?}");
    assert_eq!(named(&pushes[0]), vec![in_folder("notes.md")]);
    assert_eq!(named(&pushes[1]), vec![in_folder(".page/board.json")]);
    assert_eq!(
        entry(&pushes[1], &in_folder(".page/board.json")).expect("board")["content"],
        "b"
    );
    assert_eq!(
        named(&pushes[2]),
        vec![in_folder("a.md"), in_folder("b.md")]
    );
    let reverts: Vec<String> = d
        .requests_since(mark)
        .into_iter()
        .filter(|r| r.method.as_str() == "POST")
        .map(|r| decoded(r.url.path()))
        .collect();
    assert_eq!(
        reverts,
        vec![format!("{PREFIX}/files/{FOLDER}/.page/board.json/revert")]
    );

    let mark = d.mark();
    let leaving = d.run_in(
        &d.folder(),
        b"x",
        &["write", "../../x.md", "--parent", &parent],
    );
    assert_eq!(exit_of(&leaving), 1, "{}", stderr_of(&leaving));
    assert!(
        stderr_of(&leaving).starts_with("error: configuration error: "),
        "{}",
        stderr_of(&leaving)
    );
    assert!(d.pushes_since(mark).is_empty());

    let repo_args = [
        "write",
        ".page/x.json",
        "--repo",
        REPO,
        "--parent",
        parent.as_str(),
    ];
    let mark = d.mark();
    let named_repo = d.run_in(&d.folder(), b"{}", &repo_args);
    assert_eq!(exit_of(&named_repo), 0, "{}", stderr_of(&named_repo));
    let pushes = d.pushes_since(mark);
    assert_eq!(pushes.len(), 1);
    assert_eq!(named(&pushes[0]), vec![".page/x.json".to_string()]);

    write(&d.w.join(".page/x.json"), "{\"x\":\"edited\"}\n");
    let mark = d.mark();
    let guarded = d.run_in(&d.folder(), b"{}", &repo_args);
    assert_eq!(exit_of(&guarded), 1, "{}", stderr_of(&guarded));
    assert!(
        stderr_of(&guarded).contains(&format!("the checkout at {} holds", d.w.display())),
        "{}",
        stderr_of(&guarded)
    );
    assert!(d.pushes_since(mark).is_empty());
}

#[test]
#[serial]
fn the_holder_and_folder_ignore_files_hold_their_paths_out_of_every_publication() {
    let d = Deployment::converged();
    let folder = d.folder();
    write(&d.w.join(".synsignore"), "*.env\n");
    write(&d.w.join("clients/.gitignore"), "*.tmp\n");
    write(&folder.join(".synsignore"), ".claude/settings.local.json\n");
    write(
        &folder.join(".claude/settings.local.json"),
        "{\"local\":true}\n",
    );
    write(
        &folder.join(".page/.claude/settings.local.json"),
        "{\"page\":true}\n",
    );
    write(&folder.join("k.env"), "SECRET=1\n");
    write(&folder.join("n.tmp"), "scratch\n");
    write(&folder.join(".page/board.json"), "{\"cards\":[2]}\n");
    let mark = d.mark();

    let from_folder = d.run_in(&folder, b"", &["sync"]);
    assert_eq!(exit_of(&from_folder), 0, "{}", stderr_of(&from_folder));
    let from_w = d.run_in(&d.w, b"", &["sync"]);
    assert_eq!(exit_of(&from_w), 0, "{}", stderr_of(&from_w));

    let pushes = d.pushes_since(mark);
    assert!(!pushes.is_empty());
    for push in &pushes {
        for path in carried(push) {
            for kept_out in [
                in_folder(".claude/settings.local.json"),
                in_folder("k.env"),
                in_folder("n.tmp"),
            ] {
                assert_ne!(path, kept_out, "{push}");
            }
        }
    }
    let first = named(&pushes[0]);
    assert!(first.contains(&in_folder(".page/board.json")), "{first:?}");
    assert!(
        first.contains(&in_folder(".page/.claude/settings.local.json")),
        "{first:?}"
    );
    assert_eq!(
        read(&folder.join(".claude/settings.local.json")),
        "{\"local\":true}\n"
    );
}

#[test]
#[serial]
fn status_inside_a_folder_names_the_holder_and_the_path() {
    let d = Deployment::converged();
    d.state().visibility = "private";
    let sub = d.folder().join("sub");

    let mark = d.mark();
    let json_out = d.run_in(&sub, b"", &["--json", "status"]);
    assert_eq!(exit_of(&json_out), 0, "{}", stderr_of(&json_out));
    let repo_reads = d
        .requests_since(mark)
        .into_iter()
        .filter(|r| r.url.path() == PREFIX)
        .count();
    assert_eq!(repo_reads, 1);
    let document = one_document(&json_out);
    assert_eq!(document["owner"], "alice");
    assert_eq!(document["name"], "work");
    assert_eq!(document["visibility"], "private");
    assert_eq!(document["holder"], "alice/work");
    assert_eq!(document["path"], FOLDER);
    assert_eq!(document["workingCopyState"], "converged");

    let mark = d.mark();
    let human = d.run_in(&sub, b"", &["status"]);
    assert_eq!(exit_of(&human), 0, "{}", stderr_of(&human));
    let repo_reads = d
        .requests_since(mark)
        .into_iter()
        .filter(|r| r.url.path() == PREFIX)
        .count();
    assert_eq!(repo_reads, 1);
    let rendered = stdout_of(&human);
    let rows: Vec<&str> = rendered
        .lines()
        .filter(|line| line.chars().any(|c| c.is_alphanumeric()))
        .collect();
    let repository = rows
        .iter()
        .position(|l| l.contains("Repository") && l.contains("alice/work"))
        .expect("a Repository row");
    assert!(
        rows[repository + 1].contains("Path") && rows[repository + 1].contains(FOLDER),
        "{rendered}"
    );
}

#[test]
#[serial]
fn a_folder_checked_out_alone_works_on_its_recorded_path() {
    let d = Deployment::with_head(h1_tree());
    d.state().advance = false;
    let (_u_dir, u) = empty_u();
    let q3 = u.join("q3");
    let mark = d.mark();

    let pull = d.run_in(&u, b"", &["pull", REPO, "--path", FOLDER, "q3"]);
    assert_eq!(exit_of(&pull), 0, "{}", stderr_of(&pull));
    let mut held: Vec<String> = walk(&q3)
        .into_iter()
        .filter(|p| p.is_file())
        .map(|p| {
            p.strip_prefix(&q3)
                .unwrap()
                .to_string_lossy()
                .replace('\\', "/")
        })
        .collect();
    held.sort();
    assert_eq!(held, vec![".page/board.json", ".syns.yaml"]);
    assert_eq!(read(&q3.join(".syns.yaml")), FOLDER_YAML);

    let ls = d.run_in(&q3, b"", &["--json", "ls"]);
    assert_eq!(exit_of(&ls), 0, "{} {}", stderr_of(&ls), stdout_of(&ls));
    let paths: Vec<String> = one_document(&ls)["entries"]
        .as_array()
        .expect("entries")
        .iter()
        .map(|e| e["path"].as_str().expect("path").to_string())
        .collect();
    assert_eq!(paths, vec![".page", ".syns.yaml"]);

    write(&q3.join(".page/board.json"), "{\"cards\":[3]}\n");
    let sync = d.run_in(&q3, b"", &["sync"]);
    assert_eq!(exit_of(&sync), 0, "{}", stderr_of(&sync));
    let parent = h(1);
    let note = d.run_in(&q3, b"x", &["write", "notes.md", "--parent", &parent]);
    assert_eq!(exit_of(&note), 0, "{}", stderr_of(&note));

    let pushes = d.pushes_since(mark);
    assert_eq!(pushes.len(), 2, "{pushes:?}");
    for push in &pushes {
        assert_eq!(push["parentSha"], json!(h(1)));
        for path in carried(push) {
            assert!(path.starts_with(&format!("{FOLDER}/")), "{push}");
        }
    }
    assert_eq!(named(&pushes[1]), vec![in_folder("notes.md")]);
    assert!(!d.local_record().exists(), "a local record was written");
}

#[test]
#[serial]
fn a_folder_checkout_out_of_place_or_unmarked_is_refused() {
    let mut tree = h1_tree();
    tree.insert("docs/readme.md".into(), b"docs\n".to_vec());
    let d = Deployment::with_head(tree);
    let (_u_dir, u) = empty_u();
    let elsewhere = d.w.join("elsewhere");
    std::fs::create_dir_all(&elsewhere).expect("elsewhere");

    let mark = d.mark();
    let moved = d.run_in(&elsewhere, b"", &["pull", REPO, "--path", FOLDER]);
    assert_eq!(exit_of(&moved), 2, "{}", stderr_of(&moved));
    assert!(
        stderr_of(&moved).starts_with("error: folder out of place: "),
        "{}",
        stderr_of(&moved)
    );
    let other = d.run_in(&d.w, b"", &["pull", "bob/other", "--path", "x"]);
    assert_eq!(exit_of(&other), 2, "{}", stderr_of(&other));
    assert!(stderr_of(&other).contains("already belongs to"));
    let unnamed = d.run_in(&u, b"", &["pull", "--path", "x"]);
    assert_eq!(exit_of(&unnamed), 1, "{}", stderr_of(&unnamed));
    assert!(
        stderr_of(&unnamed).starts_with("error: configuration error: --path needs"),
        "{}",
        stderr_of(&unnamed)
    );
    assert!(d.requests_since(mark).is_empty());

    let unmarked = d.run_in(&u, b"", &["pull", REPO, "--path", "docs", "d"]);
    assert_eq!(exit_of(&unmarked), 1, "{}", stderr_of(&unmarked));
    assert!(
        stderr_of(&unmarked).contains("holds no .syns.yaml naming the folder docs"),
        "{}",
        stderr_of(&unmarked)
    );
    assert!(walk(&u.join("d")).iter().all(|p| !p.is_file()));

    let q3 = d.run_in(&u, b"", &["pull", REPO, "--path", FOLDER, "q3"]);
    assert_eq!(exit_of(&q3), 0, "{}", stderr_of(&q3));
    let mark = d.mark();
    let nested = d.run_in(&u, b"", &["pull", REPO, "--path", "docs", "q3/docs"]);
    assert_eq!(exit_of(&nested), 2, "{}", stderr_of(&nested));
    assert!(
        stderr_of(&nested).starts_with("error: folder out of place: "),
        "{}",
        stderr_of(&nested)
    );
    assert!(d.requests_since(mark).is_empty());
}

#[test]
#[serial]
fn a_misplaced_folder_is_refused_by_every_command_here() {
    let d = Deployment::converged();
    let moved = d.w.join("clients/vela/archive/q3-board");
    std::fs::create_dir_all(moved.parent().unwrap()).expect("archive");
    std::fs::rename(d.folder(), &moved).expect("move the folder");
    let before = snapshot(&d.w);
    let mark = d.mark();
    let parent = h(1);

    let sync = d.run_in(&moved, b"", &["--json", "sync", "--if-repo"]);
    assert_eq!(exit_of(&sync), 2, "{}", stderr_of(&sync));
    let document = one_document(&sync);
    assert_eq!(document["outcome"], "validation_failure");
    assert!(
        document["error"]
            .as_str()
            .unwrap()
            .starts_with("folder out of place: "),
        "{document}"
    );
    for (args, stdin) in [
        (vec!["status", "--if-repo"], &b""[..]),
        (vec!["push"], &b""[..]),
        (vec!["pull"], &b""[..]),
        (vec!["write", "a.md", "--parent", &parent], &b"x"[..]),
    ] {
        let out = d.run_in(&moved, stdin, &args);
        assert_eq!(exit_of(&out), 2, "{args:?}: {}", stderr_of(&out));
    }
    assert!(d.requests_since(mark).is_empty());
    assert_eq!(snapshot(&d.w), before);
}

#[test]
#[serial]
fn push_options_acting_on_the_holder_are_refused_inside_a_folder() {
    let d = Deployment::converged();
    let mark = d.mark();

    let visibility = d.run_in(&d.folder(), b"", &["push", "--visibility", "public"]);
    assert_eq!(exit_of(&visibility), 2, "{}", stderr_of(&visibility));
    assert!(
        stderr_of(&visibility).starts_with(&format!(
            "error: holder root required: syns push --visibility acts on the holding repository alice/work, not on the folder {}",
            d.folder().display()
        )),
        "{}",
        stderr_of(&visibility)
    );
    let name = d.run_in(&d.folder(), b"", &["push", "--name", "bob/x"]);
    assert_eq!(exit_of(&name), 1, "{}", stderr_of(&name));
    assert!(
        stderr_of(&name)
            .starts_with("error: configuration error: --name cannot stand inside the folder "),
        "{}",
        stderr_of(&name)
    );
    assert!(d.requests_since(mark).is_empty());
}

#[test]
#[serial]
fn a_scoped_or_forced_push_inside_a_folder_stands_on_the_folder_base() {
    let d = Deployment::converged();
    // The setup's pull from `W` kept a local record; the runs below must
    // neither read one nor write one.
    std::fs::remove_file(d.local_record()).expect("the setup's local record");
    std::fs::remove_file(d.folder().join(".page/board.json")).expect("remove board.json");
    write(&d.w.join(".page/x.json"), "{\"x\":\"edited\"}\n");
    let mark = d.mark();

    let scoped = d.run_in(&d.w, b"", &["push", &format!("{FOLDER}/.page")]);
    assert_eq!(exit_of(&scoped), 0, "{}", stderr_of(&scoped));
    let pushes = d.pushes_since(mark);
    assert_eq!(pushes.len(), 1, "{pushes:?}");
    assert_eq!(pushes[0]["parentSha"], json!(h(1)));
    assert_eq!(carried(&pushes[0]), vec![in_folder(".page/board.json")]);
    assert_eq!(
        pushes[0]["deletions"],
        json!([{ "path": in_folder(".page/board.json") }])
    );
    let landed = d.state().commits.last().unwrap().0.clone();

    let mark_forced = d.mark();
    let forced = d.run_in(&d.folder(), b"", &["push", "--force"]);
    assert_eq!(exit_of(&forced), 0, "{}", stderr_of(&forced));
    let pushes = d.pushes_since(mark_forced);
    assert_eq!(pushes.len(), 1, "{pushes:?}");
    assert!(pushes[0].get("parentSha").is_none(), "{}", pushes[0]);
    assert!(pushes[0].get("deletions").is_none(), "{}", pushes[0]);
    let paths = carried(&pushes[0]);
    assert!(!paths.is_empty());
    for path in &paths {
        assert!(path.starts_with(&format!("{FOLDER}/")), "{path}");
    }
    let err = stderr_of(&forced);
    assert!(err.contains("--force claimed no parent"), "{err}");
    assert!(err.contains(&landed), "{err}");

    for request in d.requests_since(mark) {
        assert!(
            !decoded(request.url.path()).starts_with(&format!("{PREFIX}/tree")),
            "a tree was read: {}",
            request.url
        );
    }
    assert!(!d.local_record().exists(), "a local record was written");
}

/// The preamble's deployment with the folder's identity file declaring
/// `check`, a head `h2` adding `notes.md` under the folder — a change of
/// the folder, so the sync behind it prepares a review (SPEC u292 Files)
/// — and the folder's `board.json` edited.
fn behind_with_check(check: &str) -> Deployment {
    let d = Deployment::converged();
    d.advance_head(&[(in_folder("notes.md").as_str(), Some("noted\n"))]);
    write(&d.folder().join(".page/board.json"), "{\"cards\":[4]}\n");
    write(
        &d.folder().join(".syns.yaml"),
        &format!("{FOLDER_YAML}checks: [\"{check}\"]\n"),
    );
    d
}

#[test]
#[serial]
fn a_folder_sync_behind_the_holder_prepares_a_review_and_continues_under_the_folder_checks() {
    let d = behind_with_check("test -f .page/board.json");
    let mark = d.mark();
    let sync = d.run_in(&d.folder(), b"", &["sync"]);
    assert_eq!(exit_of(&sync), 4, "{}", stderr_of(&sync));
    assert!(d.pushes_since(mark).is_empty());
    let continued = d.run_in(&d.folder(), b"", &["resolution", "continue"]);
    assert_eq!(exit_of(&continued), 0, "{}", stderr_of(&continued));
    let pushes = d.pushes_since(mark);
    assert_eq!(pushes.len(), 1, "{pushes:?}");
    assert_eq!(pushes[0]["parentSha"], json!(h(2)));
    let paths = named(&pushes[0]);
    assert!(!paths.is_empty());
    for path in carried(&pushes[0]) {
        assert!(path.starts_with(&format!("{FOLDER}/")), "{path}");
    }

    let failing = behind_with_check("exit 1");
    let mark = failing.mark();
    let sync = failing.run_in(&failing.folder(), b"", &["sync"]);
    assert_eq!(exit_of(&sync), 4, "{}", stderr_of(&sync));
    let continued = failing.run_in(&failing.folder(), b"", &["resolution", "continue"]);
    assert_ne!(exit_of(&continued), 0, "{}", stderr_of(&continued));
    assert!(failing.pushes_since(mark).is_empty());
    let shown = failing.run_in(&failing.folder(), b"", &["resolution", "show"]);
    assert_eq!(exit_of(&shown), 4, "{}", stderr_of(&shown));
}

#[test]
#[serial]
fn a_folder_the_head_lacks_reads_as_an_empty_folder() {
    let d = Deployment::with_head(BTreeMap::from([(
        ".page/x.json".to_string(),
        X_H1.as_bytes().to_vec(),
    )]));
    let h3 = {
        let mut state = d.state();
        state.next = 3;
        let tree = state.commits[0].1.clone();
        state.set_head(tree)
    };
    let mark = d.mark();

    let out = d.run_in(&d.folder(), b"", &["--json", "status"]);

    assert_eq!(exit_of(&out), 0, "{}", stderr_of(&out));
    assert_eq!(one_document(&out)["workingCopyState"], "diverged");
    let trees: Vec<(String, Option<String>, Option<String>)> = d
        .trees_since(mark)
        .iter()
        .map(|r| {
            (
                decoded(r.url.path()),
                query(r, "recursive"),
                query(r, "ref"),
            )
        })
        .collect();
    assert_eq!(
        trees,
        vec![
            (
                format!("{PREFIX}/tree/{FOLDER}"),
                Some("true".to_string()),
                None
            ),
            (format!("{PREFIX}/tree"), None, None),
            (
                format!("{PREFIX}/tree/{FOLDER}"),
                Some("true".to_string()),
                Some(h3)
            ),
        ]
    );
}

#[test]
#[serial]
fn the_holder_root_works_the_folder_files_as_its_own() {
    let d = Deployment::converged();
    let h2 = d.advance_head(&[(&in_folder(".page/board.json"), Some("{\"cards\":[5]}\n"))]);
    let mark = d.mark();

    let pull = d.run_in(&d.w, b"", &["pull"]);
    assert_eq!(exit_of(&pull), 0, "{}", stderr_of(&pull));
    assert_eq!(
        read(&d.folder().join(".page/board.json")),
        "{\"cards\":[5]}\n"
    );
    assert_eq!(read(&d.folder().join(".syns.yaml")), FOLDER_YAML);

    write(&d.folder().join(".page/board.json"), "{\"cards\":[6]}\n");
    let sync = d.run_in(&d.w, b"", &["sync"]);
    assert_eq!(exit_of(&sync), 0, "{}", stderr_of(&sync));
    for request in d.trees_since(mark) {
        assert_eq!(decoded(request.url.path()), format!("{PREFIX}/tree"));
    }
    let pushes = d.pushes_since(mark);
    assert_eq!(pushes.len(), 1, "{pushes:?}");
    assert_eq!(pushes[0]["parentSha"], json!(h2));
    assert_eq!(named(&pushes[0]), vec![in_folder(".page/board.json")]);
}

#[test]
#[serial]
fn a_convergence_from_either_root_leaves_the_other_clean() {
    let d = Deployment::converged();
    let h2 = d.advance_head(&[(&in_folder(".page/board.json"), Some("{}"))]);

    let pull = d.run_in(&d.w, b"", &["pull"]);
    assert_eq!(exit_of(&pull), 0, "{}", stderr_of(&pull));
    let write_folder = d.run_in(
        &d.folder(),
        b"{}",
        &["write", ".page/board.json", "--parent", &h2],
    );
    assert_eq!(exit_of(&write_folder), 0, "{}", stderr_of(&write_folder));

    write(&d.folder().join("notes.md"), "a note\n");
    let mark = d.mark();
    let sync = d.run_in(&d.folder(), b"", &["sync"]);
    assert_eq!(exit_of(&sync), 0, "{}", stderr_of(&sync));
    let pushes = d.pushes_since(mark);
    assert_eq!(pushes.len(), 1, "{pushes:?}");
    assert_eq!(named(&pushes[0]), vec![in_folder("notes.md")]);
    let published = d.state().commits.last().unwrap().0.clone();

    let write_w = d.run_in(
        &d.w,
        b"{}",
        &["write", ".page/x.json", "--parent", &published],
    );
    assert_eq!(exit_of(&write_w), 0, "{}", stderr_of(&write_w));
}

#[test]
#[serial]
fn the_plugin_and_hook_invocations_answer_inside_a_folder() {
    let d = Deployment::converged();
    d.state().advance = false;
    let envs = [
        ("SYNS_INTEGRATION", "syns-bb-plugin"),
        ("SYNS_RUN", "s1"),
        ("SYNS_TRIGGER", "thread-page"),
    ];
    let folder = d.folder();
    let mark = d.mark();

    let pull = d.run_with(&folder, &envs, b"", &["pull", "--if-repo"]);
    assert_eq!(exit_of(&pull), 0, "{}", stderr_of(&pull));
    let png: &[u8] = b"\x89PNG\r\n\x1a\n\x00\xff";
    let parent = format!("--parent={}", h(1));
    let write_mark = d.mark();
    let image = d.run_with(
        &folder,
        &envs,
        png,
        &[
            "write",
            "--bytes",
            parent.as_str(),
            "--message=m",
            "--integration=syns-bb-plugin",
            "--trigger=thread-page",
            "--run=s1",
            "--json",
            "--",
            ".page/img.png",
        ],
    );
    assert_eq!(exit_of(&image), 0, "{}", stderr_of(&image));
    let pushes = d.pushes_since(write_mark);
    assert_eq!(pushes.len(), 1);
    assert_eq!(named(&pushes[0]), vec![in_folder(".page/img.png")]);
    assert!(entry(&pushes[0], &in_folder(".page/img.png")).unwrap()["contentBase64"].is_string());
    let revert_mark = d.mark();
    let revert = d.run_with(
        &folder,
        &envs,
        b"",
        &["revert", "--to=1", "--json", "--", ".page/board.json"],
    );
    assert_eq!(exit_of(&revert), 0, "{}", stderr_of(&revert));
    let reverts: Vec<String> = d
        .requests_since(revert_mark)
        .into_iter()
        .filter(|r| r.method.as_str() == "POST")
        .map(|r| decoded(r.url.path()))
        .collect();
    assert_eq!(
        reverts,
        vec![format!("{PREFIX}/files/{FOLDER}/.page/board.json/revert")]
    );

    write(&folder.join(".page/board.json"), "{\"cards\":[7]}\n");
    let sync_mark = d.mark();
    let sync = d.run_with(&folder, &envs, b"", &["sync", "--if-repo"]);
    assert_eq!(exit_of(&sync), 0, "{}", stderr_of(&sync));
    let pushes = d.pushes_since(sync_mark);
    assert_eq!(pushes.len(), 1);
    for path in carried(&pushes[0]) {
        assert!(path.starts_with(&format!("{FOLDER}/")), "{path}");
    }
    for request in d.trees_since(mark) {
        let path = decoded(request.url.path());
        let folder_tree = path == format!("{PREFIX}/tree/{FOLDER}");
        let root_alone = path == format!("{PREFIX}/tree") && query(&request, "recursive").is_none();
        assert!(folder_tree || root_alone, "{}", request.url);
    }
    assert_eq!(read(&d.w.join(".page/x.json")), X_H1);
}

#[test]
#[serial]
fn folders_checked_out_alone_honour_the_holder_root_synsignore_and_keep_each_other_converged() {
    let mut tree = h1_tree();
    tree.insert(".synsignore".into(), b"*.env\n".to_vec());
    tree.insert(
        in_folder("inner/.syns.yaml"),
        b"holder: alice/work\npath: clients/vela/q3-board/inner\n".to_vec(),
    );
    tree.insert(in_folder("inner/a.md"), b"a at h1\n".to_vec());
    let d = Deployment::with_head(tree);
    let (_u_dir, u) = empty_u();
    let q3 = u.join("q3");
    let mark = d.mark();

    let pull = d.run_in(&u, b"", &["pull", REPO, "--path", FOLDER, "q3"]);
    assert_eq!(exit_of(&pull), 0, "{}", stderr_of(&pull));
    let pull_reads = d.requests_since(mark);
    let synsignore_at = |requests: &[Request]| -> Vec<Option<String>> {
        requests
            .iter()
            .filter(|r| decoded(r.url.path()) == format!("{PREFIX}/raw/.synsignore"))
            .map(|r| query(r, "ref"))
            .collect()
    };
    assert_eq!(synsignore_at(&pull_reads), vec![Some(h(1))]);

    write(&q3.join("inner/a.md"), "a edited\n");
    write(&q3.join(".synsignore"), "*.tmp\n!keep.env\n");
    write(&q3.join(".gitignore"), "!g.env\n");
    for file in ["k.env", "keep.env", "g.env", "inner/a.env", "inner/n.tmp"] {
        write(&q3.join(file), "x\n");
    }

    let first_mark = d.mark();
    let inner = d.run_in(&q3.join("inner"), b"", &["sync"]);
    assert_eq!(exit_of(&inner), 0, "{}", stderr_of(&inner));
    assert_eq!(
        synsignore_at(&d.requests_since(first_mark)),
        vec![Some(h(1))]
    );
    let pushes = d.pushes_since(first_mark);
    assert_eq!(pushes.len(), 1, "{pushes:?}");
    assert_eq!(pushes[0]["parentSha"], json!(h(1)));
    assert_eq!(named(&pushes[0]), vec![in_folder("inner/a.md")]);

    let second_mark = d.mark();
    let outer = d.run_in(&q3, b"", &["sync"]);
    assert_eq!(exit_of(&outer), 0, "{}", stderr_of(&outer));
    let pushes = d.pushes_since(second_mark);
    assert_eq!(pushes.len(), 1, "{pushes:?}");
    assert_eq!(pushes[0]["parentSha"], json!(h(2)));
    assert_eq!(
        named(&pushes[0]),
        vec![
            in_folder(".gitignore"),
            in_folder(".synsignore"),
            in_folder("keep.env")
        ]
    );
    for push in d.pushes_since(mark) {
        for path in carried(&push) {
            for left_out in ["k.env", "g.env", "a.env", "n.tmp"] {
                assert!(
                    path.rsplit('/').next() != Some(left_out),
                    "{path} was published"
                );
            }
        }
    }

    std::fs::create_dir_all(q3.join("old")).expect("old");
    std::fs::rename(q3.join("inner"), q3.join("old/inner")).expect("move inner");
    let moved_mark = d.mark();
    let moved = d.run_in(&q3.join("old/inner"), b"", &["sync"]);
    assert_eq!(exit_of(&moved), 2, "{}", stderr_of(&moved));
    assert!(d.requests_since(moved_mark).is_empty());
    let err = stderr_of(&moved);
    assert!(err.starts_with("error: folder out of place: "), "{err}");
    assert!(
        err.contains(&format!(
            "move the folder back to {}",
            q3.join("inner").display()
        )),
        "{err}"
    );
}

#[test]
#[serial]
fn a_folder_checked_out_alone_by_a_caller_admitted_to_it_alone_collects_under_its_own_ignore_files()
{
    let mut tree = h1_tree();
    tree.insert(".synsignore".into(), b"*.env\n".to_vec());
    let d = Deployment::with_head(tree);
    d.state()
        .raw_refusals
        .insert(".synsignore".into(), (404, "repo_not_found"));
    let (_u_dir, u) = empty_u();
    let q3 = u.join("q3");

    let pull = d.run_in(&u, b"", &["pull", REPO, "--path", FOLDER, "q3"]);
    assert_eq!(exit_of(&pull), 0, "{}", stderr_of(&pull));
    write(&q3.join(".synsignore"), "*.tmp\n");
    write(&q3.join("k.env"), "k\n");
    write(&q3.join("n.tmp"), "n\n");
    let mark = d.mark();

    let sync = d.run_in(&q3, b"", &["sync"]);
    assert_eq!(exit_of(&sync), 0, "{}", stderr_of(&sync));
    let pushes = d.pushes_since(mark);
    assert_eq!(pushes.len(), 1, "{pushes:?}");
    assert_eq!(pushes[0]["parentSha"], json!(h(1)));
    assert_eq!(
        named(&pushes[0]),
        vec![in_folder(".synsignore"), in_folder("k.env")]
    );
    assert!(carried(&pushes[0]).iter().all(|p| !p.ends_with("n.tmp")));
}

#[test]
#[serial]
fn a_review_pending_in_one_copy_holds_every_other_copy_off_its_paths() {
    let mut tree = h1_tree();
    tree.insert(in_folder(".page/board.json"), b"l1\nl2\nl3\nl4\n".to_vec());
    let d = Deployment::converged_over(tree);
    d.advance_head(&[(
        &in_folder(".page/board.json"),
        Some("l1 remote\nl2\nl3\nl4\n"),
    )]);
    write(
        &d.folder().join(".page/board.json"),
        "l1 folder\nl2\nl3\nl4\n",
    );
    d.state().hold_first_tree = Some(Duration::from_millis(1500));
    d.state().trees_answered = 0;
    let mark = d.mark();

    let spawn = |cwd: &Path, args: &[&str]| {
        let mut command = std::process::Command::new(assert_cmd::cargo::cargo_bin("syns"));
        command
            .current_dir(cwd)
            .env("SYNS_CONFIG_DIR", d.home.path())
            .env("SYNS_CACHE_DIR", d.cache.path())
            .env("HOME", d.home.path())
            .env("XDG_CONFIG_HOME", d.home.path())
            .env_remove("SYNS_URL")
            .env_remove("SYNS_INTEGRATION")
            .env_remove("SYNS_RUN")
            .env_remove("SYNS_TRIGGER")
            .env_remove("SYNS_TASK")
            .arg("--server")
            .arg(d.server.uri())
            .args(args)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped());
        command.spawn().expect("spawn syns")
    };
    let folder_sync = spawn(&d.folder(), &["sync"]);
    let w_pull = spawn(&d.w, &["pull"]);
    let folder_out = folder_sync.wait_with_output().expect("folder sync");
    let w_out = w_pull.wait_with_output().expect("W pull");
    assert_eq!(exit_of(&folder_out), 4, "{}", stderr_of(&folder_out));
    assert_eq!(exit_of(&w_out), 4, "{}", stderr_of(&w_out));

    let folder_holds = exit_of(&d.run_in(&d.folder(), b"", &["resolution", "show"])) == 4;
    let w_holds = exit_of(&d.run_in(&d.w, b"", &["resolution", "show"])) == 4;
    assert!(
        folder_holds != w_holds,
        "folder {folder_holds}, W {w_holds}"
    );
    let (holding, quiet_dir, quiet_out) = if folder_holds {
        (d.folder(), d.w.clone(), &w_out)
    } else {
        (d.w.clone(), d.folder(), &folder_out)
    };
    let elsewhere = format!(
        "This review stands in the working copy at {}",
        holding.display()
    );
    assert!(
        stdout_of(quiet_out).contains(&elsewhere),
        "{}",
        stdout_of(quiet_out)
    );

    let later = d.run_in(&quiet_dir, b"", &["sync"]);
    assert_eq!(exit_of(&later), 4, "{}", stderr_of(&later));
    assert!(
        stdout_of(&later).contains(&elsewhere),
        "{}",
        stdout_of(&later)
    );
    assert!(d.pushes_since(mark).is_empty());
    let board = read(&d.folder().join(".page/board.json"));
    assert_eq!(
        board
            .lines()
            .filter(|l| l.starts_with("<<<<<<< local"))
            .count(),
        1,
        "{board}"
    );
}

// SPEC u291 Behaviour, `cmd_pull` 3: at `--version` inside a folder the
// folder's tree at that version is written into the folder alone, as the
// registered snapshot retrieval writes one, and no local record is kept.
#[test]
#[serial]
fn a_pull_at_a_version_inside_a_folder_writes_the_folder_alone() {
    let d = Deployment::converged();
    std::fs::remove_file(d.local_record()).expect("the setup's local record");
    d.advance_head(&[
        (&in_folder(".page/board.json"), Some("{\"cards\":[8]}\n")),
        (".page/x.json", Some("{\"x\":8}\n")),
    ]);
    write(&d.folder().join(".page/board.json"), "{\"cards\":[0]}\n");
    let mark = d.mark();

    let out = d.run_in(&d.folder(), b"", &["pull", "--version", &h(1)]);

    assert_eq!(exit_of(&out), 0, "{}", stderr_of(&out));
    assert_eq!(read(&d.folder().join(".page/board.json")), BOARD_H1);
    assert_eq!(read(&d.w.join(".page/x.json")), X_H1);
    for request in d.trees_since(mark) {
        assert_eq!(
            decoded(request.url.path()),
            format!("{PREFIX}/tree/{FOLDER}")
        );
        assert_eq!(query(&request, "ref"), Some(h(1)));
    }
    assert!(!d.local_record().exists(), "a local record was written");
}

// CR1-2: a pull inside a folder its holder's checkout converged names the
// head the folder stands at, the folder copy recording no base of its own.
#[test]
#[serial]
fn a_pull_inside_a_folder_its_holder_converged_names_the_head() {
    let d = Deployment::with_head(h1_tree());
    d.pull_in(&d.w.clone());

    let out = d.run_in(&d.folder(), b"", &["--json", "pull"]);

    assert_eq!(exit_of(&out), 0, "{}", stderr_of(&out));
    let document = one_document(&out);
    assert_eq!(document["commitSha"], json!(h(1)));
    assert_eq!(document["downloaded"], json!(0));
    assert_eq!(document["unchanged"], json!(2));
}

// ---- SPEC u292: a write, push or sync inside a folder meeting a moved head

const README_H1: &str = "# work\n";

/// The head `h1` the u292 Tests preamble fixes: the checkout's identity
/// file, `README.md`, and the folder holding its identity file and
/// `board.json`.
fn u292_h1() -> BTreeMap<String, Vec<u8>> {
    BTreeMap::from([
        (".syns.yaml".to_string(), W_YAML.as_bytes().to_vec()),
        ("README.md".to_string(), README_H1.as_bytes().to_vec()),
        (in_folder(".syns.yaml"), FOLDER_YAML.as_bytes().to_vec()),
        (in_folder("board.json"), BOARD_H1.as_bytes().to_vec()),
    ])
}

/// The u292 Tests preamble: `W` and the folder each converged at `h1`,
/// the head `h2` changing `README.md` alone, and a push landing at `h2`
/// alone with the head left standing, any other parent refused naming
/// `h2`. The version list asked with the folder's path answers version
/// `1` at `h1`, the newest version changing a path under it.
fn behind_outside() -> Deployment {
    let d = Deployment::converged_over(u292_h1());
    d.advance_head(&[("README.md", Some("# work, moved outside\n"))]);
    d.state().pin_head = true;
    d
}

/// The `h1` deployment, converged, with `changes` laid over it as `h2`
/// and the head pinned as `behind_outside` pins it.
fn behind_with(changes: &[(&str, Option<&str>)]) -> Deployment {
    let d = Deployment::converged_over(u292_h1());
    d.advance_head(changes);
    d.state().pin_head = true;
    d
}

/// The unread-check line's opening, for the folder.
fn unread_line_opening() -> String {
    format!(
        "warning: the history of the folder {FOLDER} could not be read, so the run answers as though the folder moved: "
    )
}

impl Deployment {
    /// Every version-list request since `mark`.
    fn version_lists_since(&self, mark: usize) -> Vec<Request> {
        self.requests_since(mark)
            .into_iter()
            .filter(|r| {
                r.method.as_str() == "GET" && decoded(r.url.path()) == format!("{PREFIX}/versions")
            })
            .collect()
    }

    /// Every single-version request since `mark`, by the reference it
    /// addresses.
    fn single_versions_since(&self, mark: usize) -> Vec<String> {
        self.requests_since(mark)
            .into_iter()
            .filter_map(|r| {
                decoded(r.url.path())
                    .strip_prefix(&format!("{PREFIX}/versions/"))
                    .map(str::to_string)
            })
            .collect()
    }

    /// Whether a resolution stands in the folder copy.
    fn folder_resolution_stands(&self) -> bool {
        exit_of(&self.run_in(&self.folder(), b"", &["resolution", "show"])) == 4
    }
}

/// Read one HTTP/1.1 request off `stream`: its head, and a body of the
/// length it declares.
fn read_request(stream: &mut std::net::TcpStream) -> Option<(String, Vec<u8>)> {
    use std::io::Read;
    let mut head = Vec::new();
    let mut byte = [0u8; 1];
    while !head.ends_with(b"\r\n\r\n") {
        if stream.read(&mut byte).ok()? == 0 {
            return None;
        }
        head.push(byte[0]);
    }
    let head = String::from_utf8_lossy(&head).to_string();
    let length = head
        .lines()
        .find_map(|line| {
            let (name, value) = line.split_once(':')?;
            name.eq_ignore_ascii_case("content-length")
                .then(|| value.trim().parse::<usize>().ok())
                .flatten()
        })
        .unwrap_or(0);
    let mut body = vec![0u8; length];
    stream.read_exact(&mut body).ok()?;
    Some((head, body))
}

/// A listener in front of the mock at `upstream` forwarding each request
/// on a connection of its own, and closing with no status every request
/// whose request line asks the version list with a `path`.
fn path_versions_dropping_proxy(upstream: &str) -> String {
    use std::io::{Read, Write};
    let upstream = upstream.trim_start_matches("http://").to_string();
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("proxy");
    let port = listener.local_addr().expect("proxy address").port();
    std::thread::spawn(move || {
        for incoming in listener.incoming() {
            let Ok(mut client) = incoming else {
                return;
            };
            let upstream = upstream.clone();
            std::thread::spawn(move || {
                let Some((head, body)) = read_request(&mut client) else {
                    return;
                };
                let line = head.lines().next().unwrap_or_default().to_string();
                if line.contains("/versions?") && line.contains("path=") {
                    let _ = client.shutdown(std::net::Shutdown::Both);
                    return;
                }
                let Ok(mut up) = std::net::TcpStream::connect(&upstream) else {
                    return;
                };
                let mut forwarded: String = head
                    .trim_end_matches("\r\n")
                    .lines()
                    .filter(|l| !l.to_ascii_lowercase().starts_with("connection:"))
                    .map(|l| format!("{l}\r\n"))
                    .collect();
                forwarded.push_str("connection: close\r\n\r\n");
                if up.write_all(forwarded.as_bytes()).is_err() || up.write_all(&body).is_err() {
                    return;
                }
                let mut answer = Vec::new();
                let _ = up.read_to_end(&mut answer);
                let _ = client.write_all(&answer);
                let _ = client.shutdown(std::net::Shutdown::Both);
            });
        }
    });
    format!("http://127.0.0.1:{port}")
}

fn sha256(bytes: &[u8]) -> String {
    use sha2::Digest;
    format!("{:x}", sha2::Sha256::digest(bytes))
}

/// A PNG's eight-byte signature and a few bytes past it no text holds.
fn png_bytes() -> Vec<u8> {
    let mut bytes = vec![0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a];
    bytes.extend_from_slice(&[0, 0, 0, 13, b'I', b'H', b'D', b'R', 0xff, 0x00, 0xfe]);
    bytes
}

#[test]
#[serial]
fn every_write_verb_inside_a_folder_lands_over_a_push_outside_it() {
    let d = behind_outside();
    d.state()
        .file_answers
        .insert(in_folder("board.json"), "a".to_string());
    let png = png_bytes();
    let encoded = base64::engine::general_purpose::STANDARD.encode(&png);
    let changeset = json!({
        "files": [
            {"path": "board.json", "content": "{}"},
            {"path": "images/pic.png", "contentBase64": encoded},
        ],
    })
    .to_string();

    let runs: Vec<(Vec<&str>, Vec<u8>)> = vec![
        (vec!["write", "board.json", "--parent", "1"], b"{}".to_vec()),
        (
            vec!["write", "images/pic.png", "--bytes", "--parent", "1"],
            png.clone(),
        ),
        (
            vec![
                "edit",
                "board.json",
                "--old",
                "a",
                "--new",
                "b",
                "--parent",
                "1",
            ],
            Vec::new(),
        ),
        (vec!["rm", "notes.md", "--parent", "1"], Vec::new()),
        (vec!["commit", "--parent", "1"], changeset.into_bytes()),
    ];
    for (args, stdin) in runs {
        let mark = d.mark();
        let out = d.run_in(&d.folder(), &stdin, &args);
        assert_eq!(exit_of(&out), 0, "{args:?}: {}", stderr_of(&out));
        let pushes = d.pushes_since(mark);
        assert_eq!(pushes.len(), 1, "{args:?}: {pushes:?}");
        let push = &pushes[0];
        assert_eq!(push["parentSha"], json!(h(2)), "{args:?}");
        let paths = carried(push);
        assert!(!paths.is_empty(), "{args:?}");
        for path in &paths {
            assert!(path.starts_with(&format!("{FOLDER}/")), "{args:?}: {path}");
        }
        assert!(!paths.contains(&"README.md".to_string()), "{args:?}");
        for file in push["files"].as_array().into_iter().flatten() {
            if let Some(sent) = file["contentBase64"].as_str() {
                let decoded = base64::engine::general_purpose::STANDARD
                    .decode(sent)
                    .expect("base64");
                assert_eq!(sha256(&decoded), sha256(&png), "{args:?}");
            }
        }
        let lists = d.version_lists_since(mark);
        assert_eq!(lists.len(), 1, "{args:?}");
        assert_eq!(query(&lists[0], "path").as_deref(), Some(FOLDER));
        assert_eq!(query(&lists[0], "limit").as_deref(), Some("1"));
        assert_eq!(query(&lists[0], "offset").as_deref(), Some("0"));
    }
}

#[test]
#[serial]
fn a_folder_write_refused_at_its_read_head_checks_then_sends_at_the_named_head() {
    let d = behind_outside();
    d.state().repo_head = Some(0);
    let mark = d.mark();
    let out = d.run_in(
        &d.folder(),
        b"{}",
        &["write", "board.json", "--parent", "1"],
    );
    assert_eq!(exit_of(&out), 0, "{}", stderr_of(&out));
    let parents: Vec<Value> = d
        .pushes_since(mark)
        .iter()
        .map(|p| p["parentSha"].clone())
        .collect();
    assert_eq!(parents, vec![json!(h(1)), json!(h(2))]);
    assert_eq!(d.version_lists_since(mark).len(), 1);

    {
        let mut state = d.state();
        state.head = 0;
    }
    let mark = d.mark();
    let out = d.run_in(
        &d.folder(),
        b"{}",
        &["write", "board.json", "--parent", "1"],
    );
    assert_eq!(exit_of(&out), 0, "{}", stderr_of(&out));
    let parents: Vec<Value> = d
        .pushes_since(mark)
        .iter()
        .map(|p| p["parentSha"].clone())
        .collect();
    assert_eq!(parents, vec![json!(h(1))]);
    assert!(d.version_lists_since(mark).is_empty());
}

#[test]
#[serial]
fn a_write_inside_a_folder_is_refused_by_a_version_that_changed_it() {
    let d = behind_with(&[(in_folder("notes.md").as_str(), Some("noted\n"))]);
    let mark = d.mark();
    let out = d.run_in(
        &d.folder(),
        b"{}",
        &["write", "board.json", "--parent", "1"],
    );
    assert_eq!(exit_of(&out), 7, "{}", stderr_of(&out));
    assert!(
        stderr_of(&out).contains(&format!(
            "error: conflict: the repository moved past {}; its head is now {}",
            h(1),
            h(2)
        )),
        "{}",
        stderr_of(&out)
    );

    let json_out = d.run_in(
        &d.folder(),
        b"{}",
        &["--json", "write", "board.json", "--parent", "1"],
    );
    assert_eq!(exit_of(&json_out), 7, "{}", stderr_of(&json_out));
    assert_eq!(one_document(&json_out)["currentSha"], json!(h(2)));
    assert!(d.pushes_since(mark).is_empty());
}

#[test]
#[serial]
fn a_version_restoring_the_folder_still_refuses_the_write() {
    let d = behind_with(&[(in_folder("board.json").as_str(), Some("{\"cards\":[2]}\n"))]);
    d.advance_head(&[(in_folder("board.json").as_str(), Some(BOARD_H1))]);
    let mark = d.mark();
    let out = d.run_in(
        &d.folder(),
        b"{}",
        &["write", "board.json", "--parent", "1"],
    );
    assert_eq!(exit_of(&out), 7, "{}", stderr_of(&out));
    assert!(stderr_of(&out).contains(&h(3)), "{}", stderr_of(&out));
    assert!(d.pushes_since(mark).is_empty());
}

#[test]
#[serial]
fn a_hash_parent_is_numbered_once() {
    let d = behind_outside();
    d.advance_head(&[("README.md", Some("# work, v3\n"))]);
    d.advance_head(&[("README.md", Some("# work, v4\n"))]);
    let parent = format!("--parent={}", h(2));
    let mark = d.mark();
    let out = d.run_in(
        &d.folder(),
        b"{}",
        &[
            "write",
            &parent,
            "--message=m",
            "--integration=syns-bb-plugin",
            "--trigger=thread-page",
            "--run=s1",
            "--json",
            "--",
            "board.json",
        ],
    );
    assert_eq!(exit_of(&out), 0, "{}", stderr_of(&out));
    let pushes = d.pushes_since(mark);
    assert_eq!(pushes.len(), 1, "{pushes:?}");
    assert_eq!(pushes[0]["parentSha"], json!(h(4)));
    assert_eq!(d.single_versions_since(mark), vec![h(2)]);
}

#[test]
#[serial]
fn the_holder_root_and_repo_option_keep_the_whole_repository_check() {
    let d = behind_outside();
    let runs: [(PathBuf, Vec<&str>); 2] = [
        (
            d.w.clone(),
            vec!["write", "clients/vela/q3-board/board.json", "--parent", "1"],
        ),
        (
            d.folder(),
            vec![
                "write",
                "clients/vela/q3-board/board.json",
                "--repo",
                REPO,
                "--parent",
                "1",
            ],
        ),
    ];
    for (cwd, args) in runs {
        let mark = d.mark();
        let out = d.run_in(&cwd, b"{}", &args);
        assert_eq!(exit_of(&out), 7, "{args:?}: {}", stderr_of(&out));
        assert!(stderr_of(&out).contains(&h(2)), "{}", stderr_of(&out));
        assert_eq!(d.pushes_since(mark).len(), 1, "{args:?}");
        assert!(d.version_lists_since(mark).is_empty(), "{args:?}");
    }
}

#[test]
#[serial]
fn a_folder_checked_out_alone_meets_the_other_copy_change() {
    let d = Deployment::with_head(u292_h1());
    let (_u_dir, u) = empty_u();
    let q3 = u.join("q3");
    let pull = d.run_in(&u, b"", &["pull", REPO, "--path", FOLDER, "q3"]);
    assert_eq!(exit_of(&pull), 0, "{}", stderr_of(&pull));
    d.advance_head(&[(in_folder("notes.md").as_str(), Some("noted\n"))]);
    d.state().pin_head = true;

    let mark = d.mark();
    let out = d.run_in(&q3, b"{}", &["write", "board.json", "--parent", "1"]);
    assert_eq!(exit_of(&out), 7, "{}", stderr_of(&out));
    assert!(stderr_of(&out).contains(&h(2)), "{}", stderr_of(&out));
    assert!(d.pushes_since(mark).is_empty());
}

#[test]
#[serial]
fn a_folder_page_the_path_did_not_narrow_ends_on_the_unsupported_server_refusal() {
    let d = behind_outside();
    d.state().versions_unnarrowed = true;
    let refusal = format!(
        "folder_write_unsupported: the server does not support writes inside a folder yet, so {FOLDER} cannot be written from inside it"
    );
    let mark = d.mark();
    let out = d.run_in(
        &d.folder(),
        b"{}",
        &["write", "board.json", "--parent", "1"],
    );
    assert_eq!(exit_of(&out), 1, "{}", stderr_of(&out));
    assert!(
        stderr_of(&out).contains(&format!("error: {refusal}")),
        "{}",
        stderr_of(&out)
    );
    assert!(!stderr_of(&out).contains(&unread_line_opening()));

    write(&d.folder().join("board.json"), "{\"cards\":[9]}\n");
    let sync = d.run_in(&d.folder(), b"", &["--json", "sync"]);
    assert_eq!(exit_of(&sync), 1, "{}", stderr_of(&sync));
    let document = one_document(&sync);
    assert_eq!(document["outcome"], "validation_failure", "{document}");
    assert_eq!(document["error"], json!(refusal), "{document}");
    assert!(!stderr_of(&sync).contains(&unread_line_opening()));
    assert!(d.pushes_since(mark).is_empty());
    assert!(!d.folder_resolution_stands());
}

#[test]
#[serial]
fn a_folder_check_failing_transiently_answers_as_moved() {
    let mut d = behind_outside();
    d.via = Some(path_versions_dropping_proxy(&d.server.uri()));
    let mark = d.mark();
    let out = d.run_in(
        &d.folder(),
        b"{}",
        &["write", "board.json", "--parent", "1"],
    );
    assert_eq!(exit_of(&out), 7, "{}", stderr_of(&out));
    let err = stderr_of(&out);
    let warned = err
        .find(&format!(
            "{}could not reach server at",
            unread_line_opening()
        ))
        .unwrap_or_else(|| panic!("no unread-check line: {err}"));
    let refused = err
        .find("error: conflict: ")
        .unwrap_or_else(|| panic!("no conflict line: {err}"));
    assert!(warned < refused, "{err}");
    assert!(err.contains(&h(2)), "{err}");

    write(&d.folder().join("board.json"), "{\"cards\":[9]}\n");
    let sync = d.run_in(&d.folder(), b"", &["--json", "sync"]);
    assert_eq!(exit_of(&sync), 4, "{}", stderr_of(&sync));
    assert_eq!(one_document(&sync)["outcome"], "resolution_required");
    assert!(
        stderr_of(&sync).contains(&unread_line_opening()),
        "{}",
        stderr_of(&sync)
    );
    assert!(d.pushes_since(mark).is_empty());
}

#[test]
#[serial]
fn a_folder_check_refused_otherwise_ends_on_its_refusal() {
    let d = behind_outside();
    d.state().path_versions_refusal = Some((500, "internal_error"));
    let mark = d.mark();
    let out = d.run_in(
        &d.folder(),
        b"{}",
        &["write", "board.json", "--parent", "1"],
    );
    assert_eq!(exit_of(&out), 1, "{}", stderr_of(&out));
    assert!(
        stderr_of(&out).contains("error: server error (500): internal_error"),
        "{}",
        stderr_of(&out)
    );
    assert!(!stderr_of(&out).contains(&unread_line_opening()));
    assert!(d.pushes_since(mark).is_empty());
}

#[test]
#[serial]
fn sends_stop_at_the_bound() {
    let d = behind_outside();
    d.state().race_each_push = true;
    let mark = d.mark();
    let out = d.run_in(
        &d.folder(),
        b"{}",
        &["write", "board.json", "--parent", "1"],
    );
    assert_eq!(exit_of(&out), 7, "{}", stderr_of(&out));
    assert!(stderr_of(&out).contains(&h(5)), "{}", stderr_of(&out));
    let parents: Vec<Value> = d
        .pushes_since(mark)
        .iter()
        .map(|p| p["parentSha"].clone())
        .collect();
    assert_eq!(parents, vec![json!(h(2)), json!(h(3)), json!(h(4))]);
    assert_eq!(d.version_lists_since(mark).len(), 3);
}

#[test]
#[serial]
fn a_folder_sync_behind_only_outside_publishes_without_review() {
    let d = behind_outside();
    write(&d.folder().join("board.json"), "{\"cards\":[9]}\n");
    let mark = d.mark();
    let out = d.run_in(&d.folder(), b"", &["--json", "sync"]);
    assert_eq!(exit_of(&out), 0, "{}", stderr_of(&out));
    assert_eq!(one_document(&out)["outcome"], "synced");
    let pushes = d.pushes_since(mark);
    assert_eq!(pushes.len(), 1, "{pushes:?}");
    let push = &pushes[0];
    assert_eq!(push["parentSha"], json!(h(2)));
    assert_eq!(named(push), vec![in_folder("board.json")]);
    for path in carried(push) {
        assert!(path.starts_with(&format!("{FOLDER}/")), "{path}");
    }
    assert!(push.get("deletions").is_none(), "{push}");
    assert_eq!(d.version_lists_since(mark).len(), 1);
    assert!(!d.folder_resolution_stands());
}

#[test]
#[serial]
fn a_folder_sync_behind_a_folder_change_prepares_a_review() {
    let d = behind_with(&[(in_folder("board.json").as_str(), Some("{\"cards\":[2]}\n"))]);
    write(&d.folder().join("notes.md"), "noted here\n");
    let mark = d.mark();
    let out = d.run_in(&d.folder(), b"", &["--json", "sync"]);
    assert_eq!(exit_of(&out), 4, "{}", stderr_of(&out));
    assert_eq!(one_document(&out)["outcome"], "resolution_required");
    assert!(d.pushes_since(mark).is_empty());
    assert!(d.version_lists_since(mark).is_empty());
}

#[test]
#[serial]
fn a_folder_publication_refused_by_a_push_outside_sends_again_without_waiting() {
    for scoped in [false, true] {
        let d = Deployment::converged_over(u292_h1());
        d.advance_head(&[("README.md", Some("# work, moved outside\n"))]);
        d.state().tip = Some(0);
        write(&d.folder().join("board.json"), "{\"cards\":[9]}\n");
        let args: &[&str] = if scoped {
            &["push", "board.json"]
        } else {
            &["sync"]
        };
        let mark = d.mark();
        let started = std::time::Instant::now();
        let out = d.run_in(&d.folder(), b"", args);
        let took = started.elapsed();
        assert_eq!(exit_of(&out), 0, "{args:?}: {}", stderr_of(&out));
        assert!(took < Duration::from_secs(2), "{args:?} took {took:?}");
        let mut pushes = d.pushes_since(mark);
        assert_eq!(pushes.len(), 2, "{args:?}: {pushes:?}");
        let parents: Vec<Value> = pushes
            .iter_mut()
            .map(|p| {
                p.as_object_mut()
                    .expect("body")
                    .remove("parentSha")
                    .unwrap_or(Value::Null)
            })
            .collect();
        assert_eq!(parents, vec![json!(h(1)), json!(h(2))], "{args:?}");
        assert_eq!(pushes[0], pushes[1], "{args:?}");
        assert!(!d.folder_resolution_stands(), "{args:?}");

        if scoped {
            d.state().tip = None;
            let status = d.run_in(&d.folder(), b"", &["--json", "status"]);
            assert_eq!(exit_of(&status), 0, "{}", stderr_of(&status));
            assert_eq!(
                one_document(&status)["workingCopyState"],
                "converged",
                "{}",
                stdout_of(&status)
            );

            // CR1-1: the folder copy's base followed the publication that
            // landed at the named head, so a write at that commit finds
            // the folder clean.
            let landed = d.state().commits.last().unwrap().0.clone();
            let parent_arg = format!("--parent={landed}");
            let mark = d.mark();
            let note = d.run_in(
                &d.folder(),
                b"noted",
                &["write", &parent_arg, "--", "notes.md"],
            );
            assert_eq!(exit_of(&note), 0, "{}", stderr_of(&note));
            let pushes = d.pushes_since(mark);
            assert_eq!(pushes.len(), 1, "{pushes:?}");
            assert_eq!(pushes[0]["parentSha"], json!(landed));
        }
    }
}

/// CR1-2: a path-scoped folder push whose holder takes a push elsewhere
/// before each send stops at the send bound.
#[test]
#[serial]
fn a_scoped_folder_push_stops_at_the_bound() {
    let d = Deployment::converged_over(u292_h1());
    d.state().race_each_push = true;
    write(&d.folder().join("board.json"), "{\"cards\":[9]}\n");
    let mark = d.mark();
    // A run the bound no longer stops is killed here rather than holding
    // the serial suite, so the case fails on its missing exit code.
    let out = d
        .command(&d.folder(), &[], &["push", "board.json"])
        .write_stdin(Vec::new())
        .timeout(Duration::from_secs(30))
        .output()
        .expect("run syns");
    assert_eq!(exit_of(&out), 1, "{}", stderr_of(&out));
    let parents: Vec<Value> = d
        .pushes_since(mark)
        .iter()
        .map(|p| p["parentSha"].clone())
        .collect();
    assert_eq!(parents, vec![json!(h(1)), json!(h(2)), json!(h(3))]);
    assert_eq!(d.version_lists_since(mark).len(), 2);
}

/// CR1-3: a folder sync whose holder takes a push elsewhere before each
/// send stops sending at the bound and prepares the review.
#[test]
#[serial]
fn a_folder_sync_stops_sending_at_the_bound() {
    let d = Deployment::converged_over(u292_h1());
    d.state().race_each_push = true;
    write(&d.folder().join("board.json"), "{\"cards\":[9]}\n");
    let mark = d.mark();
    let out = d.run_in(&d.folder(), b"", &["--json", "sync"]);
    assert_eq!(exit_of(&out), 4, "{}", stderr_of(&out));
    assert_eq!(one_document(&out)["outcome"], "resolution_required");
    let parents: Vec<Value> = d
        .pushes_since(mark)
        .iter()
        .map(|p| p["parentSha"].clone())
        .collect();
    assert_eq!(parents, vec![json!(h(1)), json!(h(2)), json!(h(3))]);
    assert_eq!(d.version_lists_since(mark).len(), 2);
}

#[test]
#[serial]
fn ten_page_saves_land_between_ten_pushes_outside_the_folder() {
    let d = Deployment::converged_over(u292_h1());
    let mut parent = h(1);
    for save in 0..10 {
        d.advance_head(&[("README.md", Some(format!("# outside {save}\n").as_str()))]);
        let mark = d.mark();
        let parent_arg = format!("--parent={parent}");
        let drawing = format!("{{\"strokes\":[{save}]}}");
        let out = d.run_in(
            &d.folder(),
            drawing.as_bytes(),
            &[
                "write",
                &parent_arg,
                "--integration=syns-bb-plugin",
                "--trigger=thread-page",
                "--run=s1",
                "--json",
                "--",
                ".page/board.json",
            ],
        );
        assert_eq!(exit_of(&out), 0, "save {save}: {}", stderr_of(&out));
        let pushes = d.pushes_since(mark);
        assert_eq!(pushes.len(), 1, "save {save}: {pushes:?}");
        assert_eq!(named(&pushes[0]), vec![in_folder(".page/board.json")]);
        parent = one_document(&out)["commitSha"]
            .as_str()
            .expect("commitSha")
            .to_string();
    }
}
