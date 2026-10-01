//! Binary-level behaviour of placing a template into a new folder and of
//! turning its recorded checks on (SPEC u293 Tests).
//!
//! Every binary row of that table stands here under the name the table
//! gives it; `folder_identity_text_reads_back_through_the_folder_form`
//! stands in the tests module of `src/repo/syns_yaml.rs` and
//! `a_typed_path_is_checked_and_quoted_for_the_enable_command` in that of
//! `src/commands/place.rs`. Each test drives one deployment of its own: a
//! stateful mock answering `alice/work` and
//! `bartsoj/syns-whiteboard-template` from the commits each holds, a
//! config directory holding a credential, a cache directory, and the
//! checkout `W` naming `alice/work` beside `W/README.md`. `alice/work`'s
//! head is `h1`, numbered version `42` so the placement lands as `43`, and
//! the template's head is `t14`, numbered `14`, holding `.syns.yaml`
//! declaring `checks: ["test ! -d clients"]`, `.page/index.html` and the
//! PNG `.page/logo.png`, unless the row's setup says otherwise.

use assert_cmd::Command as AssertCommand;
use base64::Engine as _;
use serde_json::{Value, json};
use serial_test::serial;
use std::collections::{BTreeMap, HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use tempfile::TempDir;
use wiremock::matchers::any;
use wiremock::{Mock, MockServer, Request, Respond, ResponseTemplate};

const HOLDER: &str = "alice/work";
const TEMPLATE: &str = "bartsoj/syns-whiteboard-template";
const FOLDER: &str = "clients/vela/q3-board";
const W_YAML: &str = "owner: alice\nname: work\n";
const README: &str = "# work\n";
const CHECK: &str = "test ! -d clients";
const INDEX_14: &str = "<!doctype html><title>board</title>\n";
const INDEX_12: &str = "<!doctype html><title>board twelve</title>\n";
const LOGO: &[u8] = b"\x89PNG\r\n\x1a\n\x00\x00\x00\rIHDR\xff";

/// The holder's commit hash named for `n`: forty hex characters.
fn h(n: u32) -> String {
    format!("{n:x}").repeat(40)[..40].to_string()
}

/// The template's commit hash named for version `n`, spelt with a letter
/// so no YAML reader takes it for a number.
fn t(n: u32) -> String {
    format!("e{n:03x}").repeat(10)
}

fn blob_sha1(bytes: &[u8]) -> String {
    syns_cli::push::hash::blob_sha1(bytes)
}

fn in_folder(path: &str) -> String {
    format!("{FOLDER}/{path}")
}

type Tree = BTreeMap<String, Vec<u8>>;

fn tree(files: &[(&str, &[u8])]) -> Tree {
    files
        .iter()
        .map(|(p, b)| (p.to_string(), b.to_vec()))
        .collect()
}

/// The holder's head `h1`: its identity file and `README.md`.
fn h1_tree() -> Tree {
    tree(&[
        (".syns.yaml", W_YAML.as_bytes()),
        ("README.md", README.as_bytes()),
    ])
}

/// The template's identity file declaring `checks`, none where empty.
fn template_yaml(checks: &[&str]) -> String {
    let mut text = "owner: bartsoj\nname: syns-whiteboard-template\n".to_string();
    if !checks.is_empty() {
        let quoted: Vec<String> = checks.iter().map(|c| format!("\"{c}\"")).collect();
        text.push_str(&format!("checks: [{}]\n", quoted.join(", ")));
    }
    text
}

/// The template's tree declaring `checks`.
fn template_tree(checks: &[&str]) -> Tree {
    tree(&[
        (".syns.yaml", template_yaml(checks).as_bytes()),
        (".page/index.html", INDEX_14.as_bytes()),
        (".page/logo.png", LOGO),
    ])
}

/// The folder identity file a placement of the fixture's template into
/// `path` writes.
fn placed_text(path: &str, checks: &[&str]) -> String {
    let mut text = format!(
        "holder: alice/work\npath: {path}\ntemplate:\n  repo: {TEMPLATE}\n  version: 14\n  sha: {}\n",
        t(14)
    );
    if !checks.is_empty() {
        text.push_str("  checks:\n");
        for c in checks {
            text.push_str(&format!("  - {c}\n"));
        }
    }
    text
}

// ---- the mock deployment ----------------------------------------------

struct Commit {
    version: u32,
    sha: String,
    tree: Tree,
}

/// One repository the mock serves: its commits, the head among them, and
/// the answers a test bends.
struct Repo {
    owner: &'static str,
    name: &'static str,
    commits: Vec<Commit>,
    head: Option<usize>,
    /// The refusal the repository record answers in place of the record.
    record_refusal: Option<(u16, &'static str)>,
    /// Raw reads answered with these bytes in place of the stored ones.
    raw_bytes: HashMap<String, Vec<u8>>,
    /// Paths a recursive tree lists beside the stored ones.
    extra_tree_paths: Vec<String>,
    /// Whether the recursive tree at the root is served truncated.
    truncated: bool,
}

impl Repo {
    fn new(owner: &'static str, name: &'static str) -> Repo {
        Repo {
            owner,
            name,
            commits: Vec::new(),
            head: None,
            record_refusal: None,
            raw_bytes: HashMap::new(),
            extra_tree_paths: Vec::new(),
            truncated: false,
        }
    }

    fn commit(&mut self, version: u32, sha: String, tree: Tree) -> String {
        self.commits.push(Commit {
            version,
            sha: sha.clone(),
            tree,
        });
        self.head = Some(self.commits.len() - 1);
        sha
    }

    fn at(&self, reference: Option<&str>) -> Option<&Commit> {
        match reference {
            None => self.head.map(|i| &self.commits[i]),
            Some(r) => self
                .commits
                .iter()
                .find(|c| c.sha == r || c.version.to_string() == r),
        }
    }

    fn changed_at(&self, index: usize) -> Vec<String> {
        let tree = &self.commits[index].tree;
        let empty = Tree::new();
        let before = if index == 0 {
            &empty
        } else {
            &self.commits[index - 1].tree
        };
        tree.keys()
            .chain(before.keys())
            .filter(|p| tree.get(*p) != before.get(*p))
            .cloned()
            .collect::<std::collections::BTreeSet<_>>()
            .into_iter()
            .collect()
    }

    fn version_entry(&self, index: usize) -> Value {
        let c = &self.commits[index];
        json!({
            "version": c.version, "sha": c.sha, "parentSha": null,
            "message": "m", "messageBody": null, "author": "alice",
            "createdAt": "2026-01-01T00:00:00Z", "filesChanged": self.changed_at(index),
        })
    }
}

/// What the mock holds: every repository, and the races its next pushes
/// meet.
struct State {
    repos: BTreeMap<String, Repo>,
    /// Each push to the holder takes the first entry: a new head carrying
    /// these changes is made, and the push refused naming it.
    races: VecDeque<Vec<(String, Option<Vec<u8>>)>>,
    next: u32,
}

impl State {
    fn holder(&mut self) -> &mut Repo {
        self.repos.get_mut(HOLDER).expect("holder")
    }

    fn template(&mut self) -> &mut Repo {
        self.repos.get_mut(TEMPLATE).expect("template")
    }

    /// The holder's head with `changes` laid over it as a new head.
    fn advance(&mut self, changes: &[(String, Option<Vec<u8>>)]) -> String {
        let sha = h(self.next);
        self.next += 1;
        let holder = self.holder();
        let head = holder.head.expect("a head");
        let mut tree = holder.commits[head].tree.clone();
        for (path, content) in changes {
            match content {
                Some(bytes) => {
                    tree.insert(path.clone(), bytes.clone());
                }
                None => {
                    tree.remove(path);
                }
            }
        }
        let version = holder.commits[head].version + 1;
        holder.commit(version, sha, tree)
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
        let Some(rest) = path.strip_prefix("/api/v1/repos/") else {
            return refusal(404, "not_found");
        };
        let mut parts = rest.splitn(3, '/');
        let (Some(owner), Some(name)) = (parts.next(), parts.next()) else {
            return refusal(404, "not_found");
        };
        let id = format!("{owner}/{name}");
        let rest = parts.next().map(|r| format!("/{r}")).unwrap_or_default();
        let method = request.method.as_str();
        if id == HOLDER && method == "PUT" && rest == "/push" {
            return push_answer(&mut state, request);
        }
        let Some(repo) = state.repos.get(&id) else {
            return refusal(404, "not_found");
        };
        if rest.is_empty() && method == "GET" {
            if let Some((status, error)) = repo.record_refusal {
                return refusal(status, error);
            }
            let head = repo.head.map(|i| &repo.commits[i]);
            return ResponseTemplate::new(200).set_body_json(json!({
                "owner": repo.owner, "name": repo.name, "description": null,
                "commitSha": head.map(|c| c.sha.clone()), "status": "active",
                "author": null, "tags": [], "visibility": "public", "forkedFrom": null,
                "forkCount": 0, "fileCount": head.map(|c| c.tree.len()).unwrap_or(0),
                "role": null, "createdAt": "2026-01-01T00:00:00Z",
                "updatedAt": "2026-01-01T00:00:00Z",
            }));
        }
        if method == "GET" && (rest == "/tree" || rest.starts_with("/tree/")) {
            return tree_answer(repo, &rest, request);
        }
        if (method == "GET" || method == "HEAD")
            && let Some(file) = rest.strip_prefix("/raw/")
        {
            let Some(commit) = repo.at(query(request, "ref").as_deref()) else {
                return refusal(404, "ref_not_found");
            };
            let bytes = match (repo.raw_bytes.get(file), commit.tree.get(file)) {
                (Some(bent), Some(_)) => bent.clone(),
                (_, Some(bytes)) => bytes.clone(),
                _ => return refusal(404, "not_found"),
            };
            return ResponseTemplate::new(200).set_body_bytes(bytes);
        }
        if method == "GET"
            && let Some(file) = rest.strip_prefix("/files/")
        {
            let Some(commit) = repo.at(query(request, "ref").as_deref()) else {
                return refusal(404, "ref_not_found");
            };
            return match commit.tree.get(file) {
                Some(bytes) => {
                    let content = String::from_utf8_lossy(bytes).to_string();
                    ResponseTemplate::new(200).set_body_json(json!({
                        "content": content, "sha": blob_sha1(bytes), "size": bytes.len(),
                    }))
                }
                None => refusal(404, "not_found"),
            };
        }
        if method == "GET" && rest == "/versions" {
            return versions_answer(repo, request);
        }
        if method == "GET"
            && let Some(reference) = rest.strip_prefix("/versions/")
        {
            let found = repo
                .commits
                .iter()
                .position(|c| c.sha == reference || c.version.to_string() == reference);
            return match found {
                Some(index) => ResponseTemplate::new(200).set_body_json(repo.version_entry(index)),
                None => refusal(404, "not_found"),
            };
        }
        refusal(404, "not_found")
    }
}

fn tree_answer(repo: &Repo, rest: &str, request: &Request) -> ResponseTemplate {
    let Some(commit) = repo.at(query(request, "ref").as_deref()) else {
        return refusal(404, "ref_not_found");
    };
    let under = rest.strip_prefix("/tree/").unwrap_or("");
    let recursive = query(request, "recursive").as_deref() == Some("true");
    let within = |path: &str| under.is_empty() || path.starts_with(&format!("{under}/"));
    let mut entries: BTreeMap<String, Value> = BTreeMap::new();
    let name_of = |p: &str| p.rsplit('/').next().unwrap_or(p).to_string();
    for (path, bytes) in commit.tree.iter().filter(|(p, _)| within(p)) {
        let relative = if under.is_empty() {
            path.as_str()
        } else {
            &path[under.len() + 1..]
        };
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
    if recursive && under.is_empty() {
        for extra in &repo.extra_tree_paths {
            entries.insert(
                extra.clone(),
                json!({ "name": name_of(extra), "path": extra, "type": "file",
                        "size": 1, "sha": blob_sha1(b"x") }),
            );
        }
    }
    if !under.is_empty() && entries.is_empty() {
        return refusal(404, "not_found");
    }
    ResponseTemplate::new(200).set_body_json(json!({
        "entries": entries.into_values().collect::<Vec<_>>(),
        "commitSha": commit.sha,
        "truncated": repo.truncated && recursive && under.is_empty(),
    }))
}

fn versions_answer(repo: &Repo, request: &Request) -> ResponseTemplate {
    let path = query(request, "path");
    let limit: usize = query(request, "limit")
        .and_then(|l| l.parse().ok())
        .unwrap_or(20);
    let offset: usize = query(request, "offset")
        .and_then(|o| o.parse().ok())
        .unwrap_or(0);
    let Some(head) = repo.head else {
        return ResponseTemplate::new(200).set_body_json(json!({
            "data": [], "total": 0, "limit": limit, "offset": offset,
        }));
    };
    let listed: Vec<usize> = (0..=head)
        .rev()
        .filter(|index| match &path {
            Some(folder) => repo
                .changed_at(*index)
                .iter()
                .any(|p| p == folder || p.starts_with(&format!("{folder}/"))),
            None => true,
        })
        .collect();
    let data: Vec<Value> = listed
        .iter()
        .skip(offset)
        .take(limit)
        .map(|index| repo.version_entry(*index))
        .collect();
    ResponseTemplate::new(200).set_body_json(json!({
        "data": data, "total": listed.len(), "limit": limit, "offset": offset,
    }))
}

fn push_answer(state: &mut State, request: &Request) -> ResponseTemplate {
    let body: Value = serde_json::from_slice(&request.body).expect("a push body");
    if let Some(changes) = state.races.pop_front() {
        let sha = state.advance(&changes);
        return ResponseTemplate::new(409).set_body_json(json!({
            "error": "conflict", "currentSha": sha,
        }));
    }
    let next = h(state.next);
    let holder = state.holder();
    let head = holder.head.map(|i| {
        (
            holder.commits[i].sha.clone(),
            holder.commits[i].tree.clone(),
            holder.commits[i].version,
        )
    });
    let (head_sha, head_tree, head_version) = head.unwrap_or_default();
    if let Some(parent) = body["parentSha"].as_str()
        && parent != head_sha
    {
        return ResponseTemplate::new(409).set_body_json(json!({
            "error": "conflict", "currentSha": head_sha,
        }));
    }
    let known: HashMap<String, Vec<u8>> = holder
        .commits
        .iter()
        .flat_map(|c| c.tree.values())
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
            "commitSha": head_sha, "version": head_version,
            "filesChanged": 0, "created": false,
        }));
    }
    let changed = tree
        .keys()
        .chain(head_tree.keys())
        .filter(|p| tree.get(*p) != head_tree.get(*p))
        .collect::<std::collections::BTreeSet<_>>()
        .len();
    let version = head_version + 1;
    holder.commit(version, next.clone(), tree);
    state.next += 1;
    ResponseTemplate::new(200).set_body_json(json!({
        "commitSha": next, "version": version,
        "filesChanged": changed, "created": false,
    }))
}

/// One mock deployment, one config directory, one cache directory, and
/// the checkout `W`, each taken through `std::fs::canonicalize`.
struct Deployment {
    rt: tokio::runtime::Runtime,
    server: MockServer,
    state: Arc<Mutex<State>>,
    home: TempDir,
    cache: TempDir,
    _work: TempDir,
    w: PathBuf,
}

impl Deployment {
    /// The holder at `holder_tree` and the template at `template_tree`,
    /// the checkout `W` written with nothing converged.
    fn over(holder_tree: Tree, template: Tree) -> Deployment {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime");
        let server = rt.block_on(MockServer::start());
        let mut holder = Repo::new("alice", "work");
        holder.commit(42, h(1), holder_tree);
        let mut tmpl = Repo::new("bartsoj", "syns-whiteboard-template");
        let mut twelve = template.clone();
        twelve.insert(".page/index.html".into(), INDEX_12.as_bytes().to_vec());
        tmpl.commit(12, t(12), twelve);
        tmpl.commit(14, t(14), template);
        let state = Arc::new(Mutex::new(State {
            repos: BTreeMap::from([(HOLDER.to_string(), holder), (TEMPLATE.to_string(), tmpl)]),
            races: VecDeque::new(),
            next: 2,
        }));
        rt.block_on(
            Mock::given(any())
                .respond_with(Server(state.clone()))
                .mount(&server),
        );
        let work = tempfile::tempdir().expect("working dir");
        let w = std::fs::canonicalize(work.path()).expect("canonical W");
        std::fs::write(w.join(".syns.yaml"), W_YAML).expect("W identity");
        std::fs::write(w.join("README.md"), README).expect("README");
        std::fs::create_dir_all(w.join("sub")).expect("sub");
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
        }
    }

    /// The Tests preamble's fixture.
    fn fixture() -> Deployment {
        Deployment::over(h1_tree(), template_tree(&[CHECK]))
    }

    /// The fixture with `W` converged at `h1` by a `pull` run there.
    fn converged() -> Deployment {
        let d = Deployment::fixture();
        d.pull_in(&d.w.clone());
        d
    }

    fn pull_in(&self, dir: &Path) {
        let out = self.run_in(dir, &["pull"]);
        assert_eq!(exit_of(&out), 0, "setup pull: {}", stderr_of(&out));
    }

    fn folder(&self) -> PathBuf {
        self.w.join(FOLDER)
    }

    fn state(&self) -> std::sync::MutexGuard<'_, State> {
        self.state.lock().expect("state")
    }

    fn holder_head(&self) -> String {
        let mut state = self.state();
        let holder = state.holder();
        holder.commits[holder.head.expect("head")].sha.clone()
    }

    fn requests(&self) -> Vec<Request> {
        self.rt
            .block_on(async { self.server.received_requests().await.expect("requests") })
    }

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

    fn command(&self, cwd: &Path, args: &[&str]) -> AssertCommand {
        let mut command = AssertCommand::cargo_bin("syns").expect("syns binary");
        command
            .current_dir(cwd)
            .env("SYNS_CONFIG_DIR", self.home.path())
            .env("SYNS_CACHE_DIR", self.cache.path())
            .env("HOME", self.home.path())
            .env("XDG_CONFIG_HOME", self.home.path())
            .env_remove("SYNS_URL")
            .env_remove("SYNS_INTEGRATION")
            .env_remove("SYNS_RUN")
            .env_remove("SYNS_TRIGGER")
            .env_remove("SYNS_TASK");
        command.arg("--server").arg(self.server.uri()).args(args);
        command
    }

    fn run_in(&self, cwd: &Path, args: &[&str]) -> std::process::Output {
        self.command(cwd, args)
            .write_stdin(Vec::new())
            .output()
            .expect("run syns")
    }

    /// Whether a resolution stands in the copy at `dir`.
    fn resolution_stands(&self, dir: &Path) -> bool {
        exit_of(&self.run_in(dir, &["resolution", "show"])) == 4
    }
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

/// Every path a push body carries.
fn carried(body: &Value) -> Vec<String> {
    let mut paths: Vec<String> = body["files"]
        .as_array()
        .into_iter()
        .flatten()
        .chain(body["deletions"].as_array().into_iter().flatten())
        .map(|f| f["path"].as_str().expect("path").to_string())
        .collect();
    paths.sort();
    paths
}

fn entry<'a>(body: &'a Value, path: &str) -> &'a Value {
    body["files"]
        .as_array()
        .into_iter()
        .flatten()
        .find(|f| f["path"] == path)
        .unwrap_or_else(|| panic!("no entry for {path} in {body}"))
}

/// Every file under `root`, by its `/`-joined place, beside its bytes.
fn snapshot(root: &Path) -> BTreeMap<String, Vec<u8>> {
    fn walk(root: &Path, dir: &Path, out: &mut BTreeMap<String, Vec<u8>>) {
        for entry in std::fs::read_dir(dir).into_iter().flatten().flatten() {
            let path = entry.path();
            if path.is_dir() {
                walk(root, &path, out);
            } else {
                let place = path
                    .strip_prefix(root)
                    .expect("under root")
                    .to_string_lossy()
                    .replace('\\', "/");
                out.insert(place, std::fs::read(&path).expect("read"));
            }
        }
    }
    let mut out = BTreeMap::new();
    walk(root, root, &mut out);
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

fn placed_files() -> Vec<String> {
    vec![
        in_folder(".page/index.html"),
        in_folder(".page/logo.png"),
        in_folder(".syns.yaml"),
    ]
}

// ---- the rows ----------------------------------------------------------

#[test]
#[serial]
fn place_publishes_one_version_under_the_folder_and_writes_the_checkout() {
    let d = Deployment::fixture();
    let mark = d.mark();

    let out = d.run_in(
        &d.w,
        &["--json", "place", TEMPLATE, "clients/vela/q3-board/"],
    );

    assert_eq!(exit_of(&out), 0, "{}", stderr_of(&out));
    let pushes = d.pushes_since(mark);
    assert_eq!(pushes.len(), 1, "{pushes:?}");
    let push = &pushes[0];
    assert_eq!(push["parentSha"], json!(h(1)));
    assert!(push.get("deletions").is_none(), "{push}");
    assert_eq!(carried(push), placed_files());
    assert_eq!(
        entry(push, &in_folder(".page/index.html"))["content"],
        json!(INDEX_14)
    );
    assert!(
        entry(push, &in_folder(".page/logo.png"))
            .get("contentBase64")
            .is_some()
    );
    let held = snapshot(&d.folder());
    assert_eq!(
        held.keys().cloned().collect::<Vec<_>>(),
        vec![".page/index.html", ".page/logo.png", ".syns.yaml"]
    );
    assert_eq!(held[".page/index.html"], INDEX_14.as_bytes());
    assert_eq!(held[".page/logo.png"], LOGO);
    assert_eq!(read(&d.w.join("README.md")), README);
    assert_eq!(read(&d.w.join(".syns.yaml")), W_YAML);
    let document = one_document(&out);
    assert_eq!(document["version"], json!(43));
    assert_eq!(document["commitSha"], json!(h(2)));
    assert_eq!(document["holder"], json!(HOLDER));
    assert_eq!(document["path"], json!(FOLDER));
    assert_eq!(
        document["template"],
        json!({"repo": TEMPLATE, "version": 14, "sha": t(14)})
    );
    assert_eq!(document["checks"], json!([CHECK]));
    assert_eq!(
        document["enableChecks"],
        json!("syns enable-checks clients/vela/q3-board")
    );
}

#[test]
#[serial]
fn place_records_the_checks_not_turned_on_and_prints_them() {
    let d = Deployment::fixture();
    let mark = d.mark();

    let out = d.run_in(&d.w.join("sub"), &["place", TEMPLATE, FOLDER]);

    assert_eq!(exit_of(&out), 0, "{}", stderr_of(&out));
    let expected = placed_text(FOLDER, &[CHECK]);
    assert_eq!(read(&d.folder().join(".syns.yaml")), expected);
    let pushes = d.pushes_since(mark);
    assert_eq!(
        entry(&pushes[0], &in_folder(".syns.yaml"))["content"],
        json!(expected)
    );
    assert_eq!(stdout_of(&out), format!("{CHECK}\n"));
    let stderr = stderr_of(&out);
    let placed = stderr
        .find(&format!(
            "placed {TEMPLATE} version 14 into {FOLDER} of {HOLDER}: version 43, commit {}",
            h(2)
        ))
        .expect(&stderr);
    let label = stderr
        .find(&format!(
            "checks recorded in {FOLDER}/.syns.yaml and not turned on, so none runs anywhere:"
        ))
        .expect(&stderr);
    let turn_on = stderr
        .find(&format!(
            "turn them on for everyone working in {HOLDER} with: syns enable-checks {FOLDER}"
        ))
        .expect(&stderr);
    assert!(placed < label && label < turn_on, "{stderr}");

    let bare = Deployment::over(h1_tree(), template_tree(&[]));
    let out = bare.run_in(&bare.w.join("sub"), &["place", TEMPLATE, FOLDER]);
    assert_eq!(exit_of(&out), 0, "{}", stderr_of(&out));
    assert_eq!(stdout_of(&out), "");
    assert!(
        stderr_of(&out).contains(&format!("no checks recorded in {FOLDER}/.syns.yaml")),
        "{}",
        stderr_of(&out)
    );
    let identity = read(&bare.folder().join(".syns.yaml"));
    assert!(!identity.contains("checks"), "{identity}");
}

#[test]
#[serial]
fn place_at_a_named_version_places_that_version() {
    let d = Deployment::fixture();
    let mark = d.mark();

    let out = d.run_in(
        &d.w,
        &["--json", "place", TEMPLATE, FOLDER, "--version", "12"],
    );

    assert_eq!(exit_of(&out), 0, "{}", stderr_of(&out));
    let template_reads: Vec<Request> = d
        .requests_since(mark)
        .into_iter()
        .filter(|r| {
            let p = decoded(r.url.path());
            p.starts_with(&format!("/api/v1/repos/{TEMPLATE}/tree"))
                || p.starts_with(&format!("/api/v1/repos/{TEMPLATE}/raw/"))
        })
        .collect();
    assert!(!template_reads.is_empty());
    for r in &template_reads {
        assert_eq!(query(r, "ref"), Some(t(12)), "{}", r.url);
    }
    assert_eq!(read(&d.folder().join(".page/index.html")), INDEX_12);
    let identity = read(&d.folder().join(".syns.yaml"));
    assert!(identity.contains("  version: 12\n"), "{identity}");
    assert!(
        identity.contains(&format!("  sha: {}\n", t(12))),
        "{identity}"
    );
}

#[test]
#[serial]
fn the_placed_folder_reads_as_converged_and_lists_its_files_alone() {
    let d = Deployment::converged();
    let out = d.run_in(&d.w, &["place", TEMPLATE, FOLDER]);
    assert_eq!(exit_of(&out), 0, "{}", stderr_of(&out));
    assert_eq!(d.holder_head(), h(2));

    let status = d.run_in(&d.folder(), &["--json", "status"]);
    assert_eq!(exit_of(&status), 0, "{}", stderr_of(&status));
    assert_eq!(one_document(&status)["workingCopyState"], "converged");
    let ls = d.run_in(&d.folder(), &["--json", "ls"]);
    assert_eq!(exit_of(&ls), 0, "{}", stderr_of(&ls));
    let paths: Vec<String> = one_document(&ls)["entries"]
        .as_array()
        .expect("entries")
        .iter()
        .map(|e| e["path"].as_str().expect("path").to_string())
        .collect();
    assert_eq!(paths, vec![".page", ".syns.yaml"]);
    let mark = d.mark();
    let sync = d.run_in(&d.w, &["--json", "sync"]);
    assert_eq!(exit_of(&sync), 0, "{}", stderr_of(&sync));
    assert!(d.pushes_since(mark).is_empty());
    assert!(!d.resolution_stands(&d.w));
}

#[test]
#[serial]
fn a_placed_folder_runs_its_checks_only_once_turned_on() {
    for (check, second_continue) in [("exit 1", 4), (CHECK, 0)] {
        let d = Deployment::over(h1_tree(), template_tree(&[check]));
        d.pull_in(&d.w.clone());
        let placed = d.run_in(&d.w, &["place", TEMPLATE, FOLDER]);
        assert_eq!(exit_of(&placed), 0, "{}", stderr_of(&placed));

        let behind = |round: u32| {
            d.state().advance(&[(
                in_folder("notes.md"),
                Some(format!("noted {round}\n").into_bytes()),
            )]);
            write(
                &d.folder().join(".page/index.html"),
                &format!("<p>edited {round}</p>\n"),
            );
        };

        behind(1);
        let mark = d.mark();
        let sync = d.run_in(&d.folder(), &["sync"]);
        assert_eq!(exit_of(&sync), 4, "{check}: {}", stderr_of(&sync));
        let continued = d.run_in(&d.folder(), &["resolution", "continue"]);
        assert_eq!(exit_of(&continued), 0, "{check}: {}", stderr_of(&continued));
        let pushes = d.pushes_since(mark);
        assert_eq!(pushes.len(), 1, "{check}: {pushes:?}");
        for path in carried(&pushes[0]) {
            assert!(path.starts_with(&format!("{FOLDER}/")), "{path}");
        }

        let enabled = d.run_in(&d.folder(), &["enable-checks"]);
        assert_eq!(exit_of(&enabled), 0, "{check}: {}", stderr_of(&enabled));

        behind(2);
        let mark = d.mark();
        let sync = d.run_in(&d.folder(), &["sync"]);
        assert_eq!(exit_of(&sync), 4, "{check}: {}", stderr_of(&sync));
        let continued = d.run_in(&d.folder(), &["resolution", "continue"]);
        assert_eq!(
            exit_of(&continued),
            second_continue,
            "{check}: {}",
            stderr_of(&continued)
        );
        let pushes = d.pushes_since(mark);
        if second_continue == 0 {
            assert_eq!(pushes.len(), 1, "{check}: {pushes:?}");
            for path in carried(&pushes[0]) {
                assert!(path.starts_with(&format!("{FOLDER}/")), "{path}");
            }
        } else {
            assert!(pushes.is_empty(), "{check}: {pushes:?}");
        }
    }
}

#[test]
#[serial]
fn place_into_an_occupied_path_is_refused() {
    type Setup = Box<dyn Fn(&Deployment)>;
    let cases: [(&str, Setup, bool); 4] = [
        (
            "the tree at the folder answering an entry",
            Box::new(|d: &Deployment| {
                let mut state = d.state();
                let holder = state.holder();
                holder.commits[0]
                    .tree
                    .insert(in_folder("notes.md"), b"n\n".to_vec());
            }),
            true,
        ),
        (
            "a file at clients",
            Box::new(|d: &Deployment| {
                let mut state = d.state();
                let holder = state.holder();
                holder.commits[0]
                    .tree
                    .insert("clients".to_string(), b"c\n".to_vec());
            }),
            true,
        ),
        (
            "W/clients a file on disk",
            Box::new(|d: &Deployment| write(&d.w.join("clients"), "c\n")),
            false,
        ),
        (
            "a file under the folder on disk",
            Box::new(|d: &Deployment| write(&d.folder().join("notes/a.md"), "a\n")),
            false,
        ),
    ];
    for (index, (what, setup, asks)) in cases.iter().enumerate() {
        let d = Deployment::fixture();
        setup(&d);
        let before = snapshot(&d.w);
        let mark = d.mark();

        let out = d.run_in(&d.w, &["place", TEMPLATE, FOLDER]);

        assert_eq!(exit_of(&out), 1, "{what}: {}", stderr_of(&out));
        let stderr = stderr_of(&out);
        assert!(
            stderr.starts_with("error: configuration error: "),
            "{what}: {stderr}"
        );
        let line = stderr.trim_end();
        match index {
            0 => assert!(
                line.contains(&format!(
                    "alice/work already holds {FOLDER} at commit {}",
                    h(1)
                )),
                "{line}"
            ),
            1 => assert!(
                line.contains(&format!(
                    "alice/work already holds clients at commit {}",
                    h(1)
                )),
                "{line}"
            ),
            2 => assert!(
                line.contains(&format!("{} already holds clients;", d.w.display())),
                "{line}"
            ),
            _ => assert!(
                line.contains(&format!(
                    "{} already holds notes/a.md;",
                    d.folder().display()
                )),
                "{line}"
            ),
        }
        if !asks {
            assert!(d.requests_since(mark).is_empty(), "{what}");
        }
        assert!(d.pushes_since(mark).is_empty(), "{what}");
        assert_eq!(snapshot(&d.w), before, "{what}");
    }
}

#[test]
#[serial]
fn a_root_sync_after_a_placement_publishes_its_own_edit_without_review() {
    let d = Deployment::converged();
    write(&d.w.join("README.md"), "# work, edited\n");

    let placed = d.run_in(&d.w, &["place", TEMPLATE, FOLDER]);
    assert_eq!(exit_of(&placed), 0, "{}", stderr_of(&placed));
    let mark = d.mark();
    let sync = d.run_in(&d.w, &["--json", "sync"]);

    assert_eq!(exit_of(&sync), 0, "{}", stderr_of(&sync));
    let pushes = d.pushes_since(mark);
    assert_eq!(pushes.len(), 1, "{pushes:?}");
    assert_eq!(pushes[0]["parentSha"], json!(h(2)));
    let named: Vec<String> = pushes[0]["files"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|f| f.get("content").is_some() || f.get("contentBase64").is_some())
        .map(|f| f["path"].as_str().expect("path").to_string())
        .collect();
    assert_eq!(named, vec!["README.md"]);
    assert!(pushes[0].get("deletions").is_none(), "{}", pushes[0]);
    assert!(!d.resolution_stands(&d.w));
}

#[test]
#[serial]
fn a_template_holding_a_nested_identity_file_is_refused() {
    let mut nested = template_tree(&[CHECK]);
    nested.insert(".page/.syns.yaml".into(), b"owner: x\nname: y\n".to_vec());
    let d = Deployment::over(h1_tree(), nested);
    let mark = d.mark();

    let out = d.run_in(&d.w, &["place", TEMPLATE, FOLDER]);

    assert_eq!(exit_of(&out), 1, "{}", stderr_of(&out));
    assert!(
        stderr_of(&out).starts_with(&format!(
            "error: configuration error: {TEMPLATE} at version 14 holds an identity file at .page/.syns.yaml"
        )),
        "{}",
        stderr_of(&out)
    );
    let raw_reads = d
        .requests_since(mark)
        .into_iter()
        .filter(|r| decoded(r.url.path()).starts_with(&format!("/api/v1/repos/{TEMPLATE}/raw/")))
        .count();
    assert_eq!(raw_reads, 0);
    assert!(d.pushes_since(mark).is_empty());
}

#[test]
#[serial]
fn place_of_a_template_the_caller_cannot_read_is_refused_as_missing() {
    let d = Deployment::fixture();
    d.state().template().record_refusal = Some((404, "not_found"));
    let before = snapshot(&d.w);
    let mark = d.mark();

    let out = d.run_in(&d.w, &["--json", "place", TEMPLATE, FOLDER]);

    assert_eq!(exit_of(&out), 1, "{}", stderr_of(&out));
    assert_eq!(
        one_document(&out)["error"],
        json!(format!(
            "not_found: {TEMPLATE} is no repository you can read"
        ))
    );
    let tree_reads = d
        .requests_since(mark)
        .into_iter()
        .filter(|r| decoded(r.url.path()).starts_with(&format!("/api/v1/repos/{TEMPLATE}/tree")))
        .count();
    assert_eq!(tree_reads, 0);
    assert!(d.pushes_since(mark).is_empty());
    assert_eq!(snapshot(&d.w), before);
}

#[test]
#[serial]
fn a_head_moved_elsewhere_is_placed_on_once_more() {
    let d = Deployment::fixture();
    d.state()
        .races
        .push_back(vec![("README.md".into(), Some(b"# raced\n".to_vec()))]);
    let mark = d.mark();

    let out = d.run_in(&d.w, &["--json", "place", TEMPLATE, FOLDER]);

    assert_eq!(exit_of(&out), 0, "{}", stderr_of(&out));
    let h1b = h(2);
    let pushes = d.pushes_since(mark);
    assert_eq!(pushes.len(), 2, "{pushes:?}");
    assert_eq!(pushes[1]["parentSha"], json!(h1b));
    let mut first = pushes[0].clone();
    first["parentSha"] = json!(h1b);
    assert_eq!(pushes[1], first);
    let folder_tree_at_h1b = d.requests_since(mark).into_iter().any(|r| {
        decoded(r.url.path()) == format!("/api/v1/repos/{HOLDER}/tree/{FOLDER}")
            && query(&r, "ref") == Some(h1b.clone())
    });
    assert!(folder_tree_at_h1b);

    let twice = Deployment::fixture();
    {
        let mut state = twice.state();
        state
            .races
            .push_back(vec![("README.md".into(), Some(b"# raced\n".to_vec()))]);
        state.races.push_back(vec![(
            "README.md".into(),
            Some(b"# raced again\n".to_vec()),
        )]);
    }
    let out = twice.run_in(&twice.w, &["--json", "place", TEMPLATE, FOLDER]);
    assert_eq!(exit_of(&out), 7, "{}", stderr_of(&out));
    assert_eq!(one_document(&out)["currentSha"], json!(h(3)));
    assert!(!twice.folder().exists());
}

#[test]
#[serial]
fn a_folder_landed_at_the_moved_head_is_refused() {
    let d = Deployment::fixture();
    d.state()
        .races
        .push_back(vec![(in_folder("notes.md"), Some(b"n\n".to_vec()))]);
    let mark = d.mark();

    let out = d.run_in(&d.w, &["place", TEMPLATE, FOLDER]);

    assert_eq!(exit_of(&out), 1, "{}", stderr_of(&out));
    assert_eq!(
        stderr_of(&out).trim_end(),
        format!(
            "error: configuration error: alice/work already holds {FOLDER} at commit {}; place the template into a folder its head does not hold",
            h(2)
        )
    );
    assert_eq!(d.pushes_since(mark).len(), 1);
    assert!(!d.folder().exists());
}

/// CR1-1: a placement re-sent at a head another push moved leaves the
/// checkout's base standing at the commit it recorded, so the next sync
/// retrieves what that push changed.
#[test]
#[serial]
fn a_placement_over_a_moved_head_leaves_the_checkout_base_standing() {
    let d = Deployment::converged();
    d.state()
        .races
        .push_back(vec![("README.md".into(), Some(b"# raced\n".to_vec()))]);

    let placed = d.run_in(&d.w, &["place", TEMPLATE, FOLDER]);
    assert_eq!(exit_of(&placed), 0, "{}", stderr_of(&placed));
    let sync = d.run_in(&d.w, &["--json", "sync"]);

    assert_eq!(exit_of(&sync), 0, "{}", stderr_of(&sync));
    assert_eq!(read(&d.w.join("README.md")), "# raced\n");
}

/// CR1-3: a template whose tree arrives truncated is refused before any
/// of its files is read.
#[test]
#[serial]
fn a_template_past_one_tree_answer_is_refused() {
    let d = Deployment::fixture();
    d.state().template().truncated = true;
    let before = snapshot(&d.w);
    let mark = d.mark();

    let out = d.run_in(&d.w, &["place", TEMPLATE, FOLDER]);

    assert_eq!(exit_of(&out), 1, "{}", stderr_of(&out));
    assert_eq!(
        stderr_of(&out).trim_end(),
        format!(
            "error: configuration error: {TEMPLATE} at version 14 holds more entries than one tree answer carries; nothing placed"
        )
    );
    let raw_reads = d
        .requests_since(mark)
        .into_iter()
        .filter(|r| decoded(r.url.path()).starts_with(&format!("/api/v1/repos/{TEMPLATE}/raw/")))
        .count();
    assert_eq!(raw_reads, 0);
    assert!(d.pushes_since(mark).is_empty());
    assert_eq!(snapshot(&d.w), before);
}

#[test]
#[serial]
fn place_inside_a_scoped_folder_places_relative_to_it() {
    const VELA_YAML: &str = "holder: alice/work\npath: clients/vela\n";
    let shaped = || {
        let mut holder = h1_tree();
        holder.insert(
            "clients/vela/.syns.yaml".into(),
            VELA_YAML.as_bytes().to_vec(),
        );
        let d = Deployment::over(holder, template_tree(&[CHECK]));
        write(&d.w.join("clients/vela/.syns.yaml"), VELA_YAML);
        d.pull_in(&d.w.join("clients/vela"));
        d
    };

    let d = shaped();
    let vela = d.w.join("clients/vela");
    let mark = d.mark();
    let out = d.run_in(&vela, &["--json", "place", TEMPLATE, "q3-board"]);
    assert_eq!(exit_of(&out), 0, "{}", stderr_of(&out));
    let pushes = d.pushes_since(mark);
    assert_eq!(pushes.len(), 1, "{pushes:?}");
    assert_eq!(carried(&pushes[0]), placed_files());
    assert_eq!(one_document(&out)["path"], json!(FOLDER));
    assert!(
        read(&d.folder().join(".syns.yaml"))
            .starts_with(&format!("holder: alice/work\npath: {FOLDER}\n"))
    );
    let status = d.run_in(&d.folder(), &["--json", "status"]);
    assert_eq!(exit_of(&status), 0, "{}", stderr_of(&status));
    let status = one_document(&status);
    assert_eq!(status["path"], json!(FOLDER));
    assert_eq!(status["workingCopyState"], "converged");
    let ls = d.run_in(&d.folder(), &["--json", "ls"]);
    let paths: Vec<String> = one_document(&ls)["entries"]
        .as_array()
        .expect("entries")
        .iter()
        .map(|e| e["path"].as_str().expect("path").to_string())
        .collect();
    assert_eq!(paths, vec![".page", ".syns.yaml"]);
    let mark = d.mark();
    let sync = d.run_in(&vela, &["--json", "sync"]);
    assert_eq!(exit_of(&sync), 0, "{}", stderr_of(&sync));
    assert!(d.pushes_since(mark).is_empty());
    assert!(!d.resolution_stands(&vela));

    let fresh = shaped();
    let vela = fresh.w.join("clients/vela");
    let out = fresh.run_in(&fresh.w, &["--json", "place", TEMPLATE, FOLDER]);
    assert_eq!(exit_of(&out), 0, "{}", stderr_of(&out));
    let mark = fresh.mark();
    let sync = fresh.run_in(&vela, &["--json", "sync"]);
    assert_eq!(exit_of(&sync), 0, "{}", stderr_of(&sync));
    assert!(fresh.pushes_since(mark).is_empty());
    assert!(!fresh.resolution_stands(&vela));
}

/// Not a `SPEC.md` Tests row: `cmd_place` 18's laying over an enclosing
/// folder copy, which no row's runs observe — a sync there with nothing
/// edited converges whatever base it reads against. The enclosing folder
/// publishes its own edit at the placed commit, no review prepared.
#[test]
#[serial]
fn an_enclosing_folder_publishes_its_own_edit_after_a_placement_without_review() {
    const VELA_YAML: &str = "holder: alice/work\npath: clients/vela\n";
    let mut holder = h1_tree();
    holder.insert(
        "clients/vela/.syns.yaml".into(),
        VELA_YAML.as_bytes().to_vec(),
    );
    holder.insert("clients/vela/notes.md".into(), b"notes\n".to_vec());
    let d = Deployment::over(holder, template_tree(&[CHECK]));
    write(&d.w.join("clients/vela/.syns.yaml"), VELA_YAML);
    let vela = d.w.join("clients/vela");
    d.pull_in(&vela);

    let placed = d.run_in(&vela, &["place", TEMPLATE, "q3-board"]);
    assert_eq!(exit_of(&placed), 0, "{}", stderr_of(&placed));
    write(&vela.join("notes.md"), "notes, edited\n");
    let mark = d.mark();
    let sync = d.run_in(&vela, &["--json", "sync"]);

    assert_eq!(exit_of(&sync), 0, "{}", stderr_of(&sync));
    let pushes = d.pushes_since(mark);
    assert_eq!(pushes.len(), 1, "{pushes:?}");
    assert_eq!(pushes[0]["parentSha"], json!(h(2)));
    assert!(!d.resolution_stands(&vela));
}

/// Not a `SPEC.md` Tests row: `cmd_enable_checks` 9's laying, which the
/// rows' runs do not observe. After the checks are turned on, the folder
/// and the holder checkout each publish their own edit at the landed
/// commit, no review prepared.
#[test]
#[serial]
fn edits_after_turning_checks_on_publish_without_review() {
    let d = Deployment::converged();
    let placed = d.run_in(&d.w, &["place", TEMPLATE, FOLDER]);
    assert_eq!(exit_of(&placed), 0, "{}", stderr_of(&placed));
    let enabled = d.run_in(&d.folder(), &["enable-checks"]);
    assert_eq!(exit_of(&enabled), 0, "{}", stderr_of(&enabled));
    let landed = d.holder_head();

    write(&d.folder().join(".page/index.html"), "<p>edited</p>\n");
    let mark = d.mark();
    let sync = d.run_in(&d.folder(), &["--json", "sync"]);
    assert_eq!(exit_of(&sync), 0, "{}", stderr_of(&sync));
    let pushes = d.pushes_since(mark);
    assert_eq!(pushes.len(), 1, "{pushes:?}");
    assert_eq!(pushes[0]["parentSha"], json!(landed));

    let d = Deployment::converged();
    let placed = d.run_in(&d.w, &["place", TEMPLATE, FOLDER]);
    assert_eq!(exit_of(&placed), 0, "{}", stderr_of(&placed));
    let enabled = d.run_in(&d.w, &["enable-checks", FOLDER]);
    assert_eq!(exit_of(&enabled), 0, "{}", stderr_of(&enabled));
    let landed = d.holder_head();
    write(&d.w.join("README.md"), "# work, edited\n");
    let mark = d.mark();
    let sync = d.run_in(&d.w, &["--json", "sync"]);
    assert_eq!(exit_of(&sync), 0, "{}", stderr_of(&sync));
    let pushes = d.pushes_since(mark);
    assert_eq!(pushes.len(), 1, "{pushes:?}");
    assert_eq!(pushes[0]["parentSha"], json!(landed));
    assert!(!d.resolution_stands(&d.w));
}

#[test]
#[serial]
fn place_is_refused_before_any_request_without_a_path_a_credential_or_a_checkout() {
    let d = Deployment::fixture();
    for typed in ["/x", "a//b", "../x"] {
        let out = d.run_in(&d.w, &["place", TEMPLATE, typed]);
        assert_eq!(exit_of(&out), 1, "{typed}: {}", stderr_of(&out));
        assert!(
            stderr_of(&out).starts_with("error: configuration error: the folder path must name"),
            "{typed}: {}",
            stderr_of(&out)
        );
    }

    std::fs::remove_file(d.home.path().join("credentials.json")).expect("no credential");
    let out = d.run_in(&d.w, &["place", TEMPLATE, "x"]);
    assert_eq!(exit_of(&out), 1, "{}", stderr_of(&out));
    assert!(
        stderr_of(&out).starts_with("error: authentication required"),
        "{}",
        stderr_of(&out)
    );

    let u_dir = tempfile::tempdir().expect("U");
    let u = std::fs::canonicalize(u_dir.path()).expect("canonical U");
    let out = d.run_in(&u, &["place", TEMPLATE, "x"]);
    assert_eq!(exit_of(&out), 2, "{}", stderr_of(&out));
    assert!(
        stderr_of(&out).starts_with("error: cannot determine repo identity"),
        "{}",
        stderr_of(&out)
    );

    write(
        &d.w.join("q2/.syns.yaml"),
        "holder: alice/work\npath: clients/q2\n",
    );
    let out = d.run_in(&d.w.join("q2"), &["place", TEMPLATE, "x"]);
    assert_eq!(exit_of(&out), 2, "{}", stderr_of(&out));
    let stderr = stderr_of(&out);
    assert!(stderr.contains("folder out of place"), "{stderr}");
    assert!(
        stderr.contains(&d.w.join("q2").display().to_string()),
        "{stderr}"
    );
    assert!(stderr.contains("clients/q2"), "{stderr}");

    assert!(d.requests().is_empty());
}

#[test]
#[serial]
fn a_template_path_or_byte_the_tree_does_not_vouch_for_is_refused() {
    let d = Deployment::fixture();
    d.state()
        .template()
        .extra_tree_paths
        .push("../evil.sh".to_string());
    let outside = d.w.parent().expect("parent").join("evil.sh");
    let before = snapshot(&d.w);
    let mark = d.mark();
    let out = d.run_in(&d.w, &["place", TEMPLATE, FOLDER]);
    assert_eq!(exit_of(&out), 1, "{}", stderr_of(&out));
    assert!(d.pushes_since(mark).is_empty());
    assert!(!outside.exists());
    assert_eq!(snapshot(&d.w), before);

    let d = Deployment::fixture();
    d.state()
        .template()
        .raw_bytes
        .insert(".page/index.html".into(), b"<p>other</p>\n".to_vec());
    let before = snapshot(&d.w);
    let mark = d.mark();
    let out = d.run_in(&d.w, &["place", TEMPLATE, FOLDER]);
    assert_eq!(exit_of(&out), 1, "{}", stderr_of(&out));
    assert!(
        stderr_of(&out).contains("invalid response body"),
        "{}",
        stderr_of(&out)
    );
    assert!(d.pushes_since(mark).is_empty());
    assert_eq!(snapshot(&d.w), before);
}

#[test]
#[serial]
fn a_template_identity_file_that_does_not_parse_is_refused() {
    let mut broken = template_tree(&[]);
    broken.insert(".syns.yaml".into(), b"checks: [".to_vec());
    let d = Deployment::over(h1_tree(), broken);
    let mark = d.mark();

    let out = d.run_in(&d.w, &["place", TEMPLATE, FOLDER]);

    assert_eq!(exit_of(&out), 1, "{}", stderr_of(&out));
    assert!(
        stderr_of(&out).starts_with(&format!(
            "error: invalid .syns.yaml in {TEMPLATE} at version 14: "
        )),
        "{}",
        stderr_of(&out)
    );
    assert!(d.pushes_since(mark).is_empty());
}

#[test]
#[serial]
fn enable_checks_publishes_the_recorded_checks_for_everyone() {
    let d = Deployment::converged();
    let placed = d.run_in(&d.w, &["place", TEMPLATE, FOLDER]);
    assert_eq!(exit_of(&placed), 0, "{}", stderr_of(&placed));
    let mark = d.mark();

    let out = d.run_in(&d.w.join("sub"), &["--json", "enable-checks", FOLDER]);

    assert_eq!(exit_of(&out), 0, "{}", stderr_of(&out));
    let pushes = d.pushes_since(mark);
    assert_eq!(pushes.len(), 1, "{pushes:?}");
    assert_eq!(pushes[0]["parentSha"], json!(h(2)));
    assert!(pushes[0].get("deletions").is_none(), "{}", pushes[0]);
    assert_eq!(carried(&pushes[0]), vec![in_folder(".syns.yaml")]);
    let expected = format!("{}checks:\n- {CHECK}\n", placed_text(FOLDER, &[CHECK]));
    assert_eq!(
        entry(&pushes[0], &in_folder(".syns.yaml"))["content"],
        json!(expected)
    );
    assert_eq!(read(&d.folder().join(".syns.yaml")), expected);
    let document = one_document(&out);
    assert_eq!(document["holder"], json!(HOLDER));
    assert_eq!(document["path"], json!(FOLDER));
    assert_eq!(document["enabled"], json!([CHECK]));
    assert_eq!(document["version"], json!(44));
    assert_eq!(document["commitSha"], json!(h(3)));

    let status = d.run_in(&d.folder(), &["--json", "status"]);
    assert_eq!(exit_of(&status), 0, "{}", stderr_of(&status));
    assert_eq!(one_document(&status)["workingCopyState"], "converged");
    let mark = d.mark();
    let sync = d.run_in(&d.w, &["--json", "sync"]);
    assert_eq!(exit_of(&sync), 0, "{}", stderr_of(&sync));
    assert!(d.pushes_since(mark).is_empty());
    assert!(!d.resolution_stands(&d.w));
}

/// CR1-2: turning checks on at a head another push moved leaves the
/// checkout's base standing at the commit it recorded, so the next sync
/// retrieves what that push changed.
#[test]
#[serial]
fn enable_checks_over_a_moved_head_leaves_the_checkout_base_standing() {
    let d = Deployment::converged();
    let placed = d.run_in(&d.w, &["place", TEMPLATE, FOLDER]);
    assert_eq!(exit_of(&placed), 0, "{}", stderr_of(&placed));
    d.state()
        .races
        .push_back(vec![("README.md".into(), Some(b"# raced\n".to_vec()))]);
    let mark = d.mark();

    let out = d.run_in(&d.w, &["enable-checks", FOLDER]);

    assert_eq!(exit_of(&out), 0, "{}", stderr_of(&out));
    assert_eq!(d.pushes_since(mark).len(), 2);
    let sync = d.run_in(&d.w, &["--json", "sync"]);
    assert_eq!(exit_of(&sync), 0, "{}", stderr_of(&sync));
    assert_eq!(read(&d.w.join("README.md")), "# raced\n");
}

#[test]
#[serial]
fn enable_checks_keeps_every_key_a_hand_edit_wrote() {
    const HAND: &str = "# kept by hand\nnote: kept\nholder: alice/work\npath: clients/vela/q3-board\nchecks: [exit 0]\ntemplate:\n  repo: bartsoj/syns-whiteboard-template\n  version: 14\n  sha: t14\n  checks:\n  - test ! -d clients\n";
    let mut holder = h1_tree();
    holder.insert(in_folder(".page/index.html"), INDEX_14.as_bytes().to_vec());
    holder.insert(in_folder(".page/logo.png"), LOGO.to_vec());
    holder.insert(in_folder(".syns.yaml"), HAND.as_bytes().to_vec());
    let d = Deployment::over(holder, template_tree(&[CHECK]));
    d.pull_in(&d.w.clone());
    d.pull_in(&d.folder());
    let head = d.holder_head();
    let mark = d.mark();

    let out = d.run_in(&d.w, &["--json", "enable-checks", FOLDER]);

    assert_eq!(exit_of(&out), 0, "{}", stderr_of(&out));
    let pushes = d.pushes_since(mark);
    assert_eq!(pushes.len(), 1, "{pushes:?}");
    assert_eq!(pushes[0]["parentSha"], json!(head));
    assert_eq!(carried(&pushes[0]), vec![in_folder(".syns.yaml")]);
    let expected = "note: kept\nholder: alice/work\npath: clients/vela/q3-board\nchecks:\n- exit 0\n- test ! -d clients\ntemplate:\n  repo: bartsoj/syns-whiteboard-template\n  version: 14\n  sha: t14\n  checks:\n  - test ! -d clients\n";
    assert_eq!(
        entry(&pushes[0], &in_folder(".syns.yaml"))["content"],
        json!(expected)
    );
    assert_eq!(read(&d.folder().join(".syns.yaml")), expected);
    assert_eq!(one_document(&out)["enabled"], json!([CHECK]));
}

#[test]
#[serial]
fn enable_checks_in_the_folder_turns_on_only_what_waits() {
    let d = Deployment::converged();
    let placed = d.run_in(&d.w, &["place", TEMPLATE, FOLDER]);
    assert_eq!(exit_of(&placed), 0, "{}", stderr_of(&placed));
    let page = d.folder().join(".page");
    let none_waits = format!("no check recorded in {FOLDER}/.syns.yaml waits to be turned on");

    let mark = d.mark();
    let first = d.run_in(&page, &["enable-checks"]);
    assert_eq!(exit_of(&first), 0, "{}", stderr_of(&first));
    let pushes = d.pushes_since(mark);
    assert_eq!(pushes.len(), 1, "{pushes:?}");
    assert_eq!(carried(&pushes[0]), vec![in_folder(".syns.yaml")]);
    assert_eq!(stdout_of(&first), format!("{CHECK}\n"));

    let mark = d.mark();
    let second = d.run_in(&page, &["enable-checks"]);
    assert_eq!(exit_of(&second), 0, "{}", stderr_of(&second));
    assert!(d.requests_since(mark).is_empty());
    assert_eq!(stderr_of(&second).trim_end(), none_waits);

    let bare = Deployment::over(h1_tree(), template_tree(&[]));
    let placed = bare.run_in(&bare.w, &["place", TEMPLATE, FOLDER]);
    assert_eq!(exit_of(&placed), 0, "{}", stderr_of(&placed));
    let mark = bare.mark();
    let third = bare.run_in(&bare.folder(), &["enable-checks"]);
    assert_eq!(exit_of(&third), 0, "{}", stderr_of(&third));
    assert!(bare.requests_since(mark).is_empty());
    assert_eq!(stderr_of(&third).trim_end(), none_waits);
}

#[test]
#[serial]
fn enable_checks_is_refused_outside_a_placed_folder_or_over_unpublished_work() {
    let d = Deployment::fixture();
    write(
        &d.w.join("clients/q2/.syns.yaml"),
        "holder: alice/work\npath: clients/q2\n",
    );
    for (args, named) in [
        (vec!["enable-checks", "clients/q2"], d.w.join("clients/q2")),
        (vec!["enable-checks"], d.w.clone()),
    ] {
        let out = d.run_in(&d.w, &args);
        assert_eq!(exit_of(&out), 1, "{args:?}: {}", stderr_of(&out));
        assert_eq!(
            stderr_of(&out).trim_end(),
            format!(
                "error: configuration error: {} holds no folder placed from a template; syns enable-checks turns on only the checks syns place recorded",
                named.display()
            )
        );
    }
    assert!(d.requests().is_empty());

    let d = Deployment::converged();
    let placed = d.run_in(&d.w, &["place", TEMPLATE, FOLDER]);
    assert_eq!(exit_of(&placed), 0, "{}", stderr_of(&placed));
    write(&d.folder().join(".page/index.html"), "<p>edited</p>\n");
    let identity = read(&d.folder().join(".syns.yaml"));
    let mark = d.mark();
    let out = d.run_in(&d.w, &["enable-checks", FOLDER]);
    assert_eq!(exit_of(&out), 1, "{}", stderr_of(&out));
    let stderr = stderr_of(&out);
    assert!(
        stderr.contains("holds unpublished local changes for"),
        "{stderr}"
    );
    assert!(
        stderr.contains(&d.folder().display().to_string()),
        "{stderr}"
    );
    assert!(d.pushes_since(mark).is_empty());
    assert_eq!(read(&d.folder().join(".syns.yaml")), identity);
}
