//! Binary-level rows of roadmap 224 (FEATURE_SPEC § Expected): `syns
//! diff --from V` alone, `--provenance`, `--content` and `--path`.
//!
//! Each test runs the built binary against one wiremock deployment of its
//! own. `alice/notes` is addressed through `--repo` from a directory
//! holding nothing unless the test stands in a checkout. Commit hashes
//! are the version number in 40 lowercase hex digits (`c(n)`), so a
//! version's hash is known wherever its number is.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::time::Duration;

use assert_cmd::Command as AssertCommand;
use serde_json::{Value, json};
use serial_test::serial;
use syns_cli::push::hash::blob_sha1;
use tempfile::TempDir;
use wiremock::matchers::{body_string_contains, method, path as path_matcher, query_param};
use wiremock::matchers::{path_regex, query_param_is_missing};
use wiremock::{Mock, MockServer, Request, Respond, ResponseTemplate};

const REPO: &str = "alice/notes";
const V1: &str = "/api/v1/repos/alice/notes";
const V2: &str = "/api/v2/repos/alice/notes";

/// The hash of version `n`.
fn c(n: u32) -> String {
    format!("{n:040x}")
}

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

    /// Each request as `METHOD path?query`, in the order received.
    fn log(&self) -> Vec<String> {
        self.requests()
            .iter()
            .map(|r| match r.url.query() {
                Some(q) => format!("{} {}?{q}", r.method, r.url.path()),
                None => format!("{} {}", r.method, r.url.path()),
            })
            .collect()
    }

    fn requests_to(&self, suffix: &str) -> Vec<Request> {
        self.requests()
            .into_iter()
            .filter(|r| r.url.path().ends_with(suffix))
            .collect()
    }

    /// The offsets of every version page read, in order.
    fn pages(&self) -> Vec<String> {
        self.requests_to("/versions")
            .iter()
            .map(|r| query_of(r, "offset").unwrap_or_default())
            .collect()
    }

    /// The body of every batch read, in order.
    fn reads(&self) -> Vec<Value> {
        self.requests_to("/read")
            .iter()
            .map(|r| serde_json::from_slice(&r.body).expect("read body"))
            .collect()
    }

    fn run(&self, args: &[&str]) -> std::process::Output {
        self.run_in(self.work.path(), args)
    }

    fn run_in(&self, cwd: &Path, args: &[&str]) -> std::process::Output {
        self.command(cwd).args(args).output().expect("run syns")
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
            .timeout(Duration::from_secs(30))
            .arg("--server")
            .arg(self.server.uri());
        command
    }

    /// `EP-v2-changes` for `from` and `to` as sent, with `path` as sent or
    /// none at all.
    fn changes(&self, from: &str, to: &str, path: Option<&str>, body: Value) {
        let mock = Mock::given(method("GET"))
            .and(path_matcher(format!("{V2}/changes")))
            .and(query_param("from", from))
            .and(query_param("to", to));
        let mock = match path {
            Some(p) => mock.and(query_param("path", p)),
            None => mock.and(query_param_is_missing("path")),
        };
        self.mount(mock.respond_with(ResponseTemplate::new(200).set_body_json(body)));
    }

    /// The version page at `offset` of the whole list (no `path`).
    fn page(&self, offset: u32, data: Vec<Value>, total: u32) {
        self.mount(
            Mock::given(method("GET"))
                .and(path_matcher(format!("{V1}/versions")))
                .and(query_param("limit", "100"))
                .and(query_param("offset", offset.to_string()))
                .and(query_param_is_missing("path"))
                .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                    "data": data, "total": total, "limit": 100, "offset": offset,
                })))
                .with_priority(1),
        );
    }

    /// Every other page: `data: []`.
    fn empty_pages(&self, total: u32) {
        self.mount(
            Mock::given(method("GET"))
                .and(path_matcher(format!("{V1}/versions")))
                .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                    "data": [], "total": total, "limit": 100, "offset": 0,
                })))
                .with_priority(10),
        );
    }

    /// The batch read, answering from `files` at the commit asked.
    fn read(&self, frames: Frames) {
        self.mount(
            Mock::given(method("POST"))
                .and(path_matcher(format!("{V2}/read")))
                .respond_with(frames),
        );
    }

    fn head(&self, version: u32) {
        self.mount(
            Mock::given(method("GET"))
                .and(path_matcher(format!("{V2}/head")))
                .respond_with(
                    ResponseTemplate::new(200)
                        .set_body_json(json!({"commit": c(version), "version": version})),
                ),
        );
    }
}

fn query_of(r: &Request, key: &str) -> Option<String> {
    r.url
        .query_pairs()
        .find(|(k, _)| k == key)
        .map(|(_, v)| v.to_string())
}

/// One `RepoVersion`.
fn ver(n: u32, files: &[&str], provenance: Value, author: &str) -> Value {
    json!({
        "version": n, "sha": c(n), "parentSha": c(n - 1), "message": format!("v{n}"),
        "messageBody": null, "author": author, "createdAt": "2026-10-01T00:00:00Z",
        "filesChanged": files, "provenance": provenance,
    })
}

/// Versions `top` down to `bottom`, each changing what `changed` names
/// for it and `filler.md` otherwise, published by maria through bb.
fn versions(top: u32, bottom: u32, changed: &HashMap<u32, Vec<&str>>) -> Vec<Value> {
    (bottom..=top)
        .rev()
        .map(|n| {
            let files = changed
                .get(&n)
                .cloned()
                .unwrap_or_else(|| vec!["filler.md"]);
            ver(n, &files, prov("maria", Some(&format!("r{n}"))), "maria")
        })
        .collect()
}

fn prov(publisher: &str, run: Option<&str>) -> Value {
    json!({"publisher": publisher, "integration": "bb", "run": run, "trigger": null, "taskRef": null})
}

fn end(n: u32) -> Value {
    json!({"commit": c(n), "version": n})
}

fn changes_body(from: u32, to: u32, entries: Vec<Value>) -> Value {
    json!({"from": end(from), "to": end(to), "entries": entries})
}

fn modified(path: &str, bytes: &[u8]) -> Value {
    json!({"path": path, "status": "modified", "oldSha": "1".repeat(40), "newSha": blob_sha1(bytes), "size": bytes.len()})
}

fn added(path: &str, bytes: &[u8]) -> Value {
    json!({"path": path, "status": "added", "newSha": blob_sha1(bytes), "size": bytes.len()})
}

fn deleted(path: &str) -> Value {
    json!({"path": path, "status": "deleted", "oldSha": "2".repeat(40)})
}

fn renamed(old: &str, path: &str, bytes: &[u8]) -> Value {
    json!({"path": path, "oldPath": old, "status": "renamed", "oldSha": "3".repeat(40), "newSha": blob_sha1(bytes), "size": bytes.len()})
}

/// `EP-v2-read`'s frames: each asked path's bytes from `files`, `too_large`
/// for every path in `too_large` (both asks), `not_found` otherwise, and
/// `unchanged` where `have` names the file's hash.
#[derive(Clone, Default)]
struct Frames {
    files: HashMap<String, Vec<u8>>,
    too_large: HashSet<String>,
    version: u32,
}

impl Frames {
    fn of(version: u32, files: &[(&str, &[u8])]) -> Frames {
        Frames {
            files: files
                .iter()
                .map(|(p, b)| (p.to_string(), b.to_vec()))
                .collect(),
            too_large: HashSet::new(),
            version,
        }
    }
}

impl Respond for Frames {
    fn respond(&self, request: &Request) -> ResponseTemplate {
        let body: Value = serde_json::from_slice(&request.body).expect("read body");
        let at = body["at"].as_str().expect("at is a hash").to_string();
        let mut out = format!("{}\n", json!({"commit": at, "version": self.version})).into_bytes();
        for asked in body["paths"].as_array().expect("paths") {
            let path = asked["path"].as_str().unwrap();
            let Some(bytes) = self.files.get(path) else {
                out.extend(format!("{}\n", json!({"path": path, "status": "not_found"})).bytes());
                continue;
            };
            let sha = blob_sha1(bytes);
            let status = if self.too_large.contains(path) {
                "too_large"
            } else if asked.get("have").and_then(Value::as_str) == Some(sha.as_str()) {
                "unchanged"
            } else {
                "ok"
            };
            out.extend(
                format!(
                    "{}\n",
                    json!({"path": path, "sha": sha, "size": bytes.len(), "mode": "100644", "status": status})
                )
                .bytes(),
            );
            if status == "ok" {
                out.extend_from_slice(bytes);
            }
        }
        ResponseTemplate::new(200)
            .insert_header("content-type", "application/x-syns-frames")
            .set_body_bytes(out)
    }
}

fn exit_of(out: &std::process::Output) -> i32 {
    out.status.code().unwrap_or(-1)
}

fn stdout_of(out: &std::process::Output) -> String {
    String::from_utf8_lossy(&out.stdout).to_string()
}

fn stderr_of(out: &std::process::Output) -> String {
    String::from_utf8_lossy(&out.stderr).to_string()
}

fn document(out: &std::process::Output) -> Value {
    serde_json::from_slice(&out.stdout)
        .unwrap_or_else(|e| panic!("one document: {e}\n{}{}", stdout_of(out), stderr_of(out)))
}

fn refusal(code: &str, line: &str) -> Value {
    json!({"error": line, "code": code})
}

/// A `CONFIG_ERROR` refusal as it renders.
fn config(line: &str) -> Value {
    refusal("config_error", &format!("configuration error: {line}"))
}

const PRICING: &[u8] = b"# Pricing\nnew\n";
const NOTES_B: &[u8] = b"notes b\n";
const LOGO: &[u8] = b"\x89PNG\r\n\x1a\n\0\0\0\rIHDR";

/// The C1 sample's history: V = 4090, H = 4098.
fn mount_sample(d: &Deployment) -> Vec<Value> {
    let entries = vec![
        added("assets/logo.png", LOGO),
        modified("docs/pricing.md", PRICING),
        deleted("drafts/old-plan.md"),
        renamed("notes/a.md", "notes/b.md", NOTES_B),
    ];
    d.changes(
        "4090",
        "head",
        None,
        changes_body(4090, 4098, entries.clone()),
    );
    let mut page = vec![
        ver(4098, &["other.md"], prov("maria", None), "maria"),
        ver(
            4097,
            &["docs/pricing.md"],
            json!({"publisher": "maria", "integration": "bb", "run": "thr_k2v", "trigger": "page-edit", "taskRef": null}),
            "maria",
        ),
        ver(4096, &["other.md"], prov("maria", None), "maria"),
        ver(
            4095,
            &["notes/a.md", "notes/b.md"],
            json!({"publisher": "ci-bot", "integration": "github-actions", "run": "39533", "trigger": null, "taskRef": null}),
            "ci-bot",
        ),
        ver(
            4094,
            &["docs/pricing.md"],
            prov("maria", Some("old")),
            "maria",
        ),
        ver(4093, &["assets/logo.png"], prov("maria", None), "maria"),
        ver(4092, &["other.md"], prov("maria", None), "maria"),
        ver(4091, &["drafts/old-plan.md"], Value::Null, "bart"),
    ];
    page.extend(versions(4090, 3999, &HashMap::new()));
    d.page(0, page, 4098);
    d.read(Frames::of(
        4098,
        &[
            ("docs/pricing.md", PRICING),
            ("notes/b.md", NOTES_B),
            ("assets/logo.png", LOGO),
        ],
    ));
    entries
}

// Rows 1, 8 and 10: what changed since V, from no checkout, in three
// requests; the texts read at the answer's commit, each ask a path the
// changes answer gave as standing, `have` sent once the store holds it.
#[test]
#[serial]
fn from_alone_with_provenance_and_content_from_no_checkout() {
    let d = Deployment::new();
    let entries = mount_sample(&d);

    let out = d.run(&[
        "diff",
        "--from",
        "4090",
        "--provenance",
        "--content",
        "--repo",
        REPO,
        "--json",
    ]);

    assert_eq!(exit_of(&out), 0, "{}", stderr_of(&out));
    let doc = document(&out);
    let expected = json!({
        "from": end(4090),
        "to": end(4098),
        "entries": [
            {
                "path": "assets/logo.png", "status": "added",
                "newSha": blob_sha1(LOGO), "size": LOGO.len(),
                "provenance": {"version": 4093, "publisher": "maria", "integration": "bb", "recorded": true},
                "text": null,
            },
            {
                "path": "docs/pricing.md", "status": "modified",
                "oldSha": "1".repeat(40), "newSha": blob_sha1(PRICING), "size": PRICING.len(),
                "provenance": {"version": 4097, "publisher": "maria", "integration": "bb", "run": "thr_k2v", "trigger": "page-edit", "recorded": true},
                "text": "# Pricing\nnew\n",
            },
            {
                "path": "drafts/old-plan.md", "status": "deleted", "oldSha": "2".repeat(40),
                "provenance": {"version": 4091, "publisher": "bart", "recorded": false},
            },
            {
                "path": "notes/b.md", "oldPath": "notes/a.md", "status": "renamed",
                "oldSha": "3".repeat(40), "newSha": blob_sha1(NOTES_B), "size": NOTES_B.len(),
                "provenance": {"version": 4095, "publisher": "ci-bot", "integration": "github-actions", "run": "39533", "recorded": true},
                "text": "notes b\n",
            },
        ],
        "deferred": [],
    });
    assert_eq!(doc, expected);
    // `entries` is the changes answer's, apart from the added keys.
    let served: Vec<Value> = doc["entries"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| {
            let mut e = e.clone();
            e.as_object_mut().unwrap().remove("provenance");
            e.as_object_mut().unwrap().remove("text");
            e
        })
        .collect();
    assert_eq!(served, entries);
    // The documented member order.
    let raw = stdout_of(&out);
    let order: Vec<usize> = ["\"from\"", "\"to\"", "\"entries\"", "\"deferred\""]
        .iter()
        .map(|k| raw.find(k).expect(k))
        .collect();
    assert!(order.windows(2).all(|w| w[0] < w[1]), "{raw}");

    // Row 8: 1 changes request, 1 read, 1 history page; nothing else.
    let log = d.log();
    assert_eq!(log.len(), 3, "{log:?}");
    assert!(log[0].starts_with(&format!("GET {V2}/changes?")), "{log:?}");
    assert_eq!(d.requests_to("/read").len(), 1);
    assert_eq!(d.pages(), vec!["0"]);
    // Row 10: at the answer's commit, standing paths only.
    let reads = d.reads();
    assert_eq!(reads[0]["at"], json!(c(4098)));
    let asked: Vec<&str> = reads[0]["paths"]
        .as_array()
        .unwrap()
        .iter()
        .map(|p| p["path"].as_str().unwrap())
        .collect();
    assert_eq!(
        asked,
        vec!["assets/logo.png", "docs/pricing.md", "notes/b.md"]
    );
    assert!(reads[0]["paths"][1].get("have").is_none());

    // Row 10: a second run sends `have` for what the store now holds.
    let again = d.run(&[
        "diff",
        "--from",
        "4090",
        "--provenance",
        "--content",
        "--repo",
        REPO,
        "--json",
    ]);
    assert_eq!(exit_of(&again), 0, "{}", stderr_of(&again));
    assert_eq!(document(&again), expected);
    let reads = d.reads();
    assert_eq!(reads.len(), 2);
    assert_eq!(reads[1]["paths"][1]["have"], json!(blob_sha1(PRICING)));
}

// Row 2: plain `--from` alone resolves the head once, then `EP-diff`.
#[test]
#[serial]
fn plain_from_alone_resolves_the_head_once() {
    let served = json!({
        "from": {"version": 4090, "sha": c(4090)},
        "to": {"version": 4098, "sha": c(4098)},
        "files": [{"path": "a.md", "status": "modified", "diff": "@@ -1 +1 @@\n-a\n+b\n"}],
    });
    let mount_diff = |d: &Deployment| {
        d.mount(
            Mock::given(method("GET"))
                .and(path_matcher(format!("{V1}/diff")))
                .and(query_param("from", "4090"))
                .and(query_param("to", "4098"))
                .respond_with(ResponseTemplate::new(200).set_body_json(served.clone())),
        );
    };

    let d = Deployment::new();
    d.head(4098);
    mount_diff(&d);
    let out = d.run(&["diff", "--from", "4090", "--repo", REPO, "--json"]);
    assert_eq!(exit_of(&out), 0, "{}", stderr_of(&out));
    assert_eq!(document(&out), served);
    let log = d.log();
    assert_eq!(log.len(), 2, "{log:?}");
    assert_eq!(log[0], format!("GET {V2}/head"));
    assert!(log[1].starts_with(&format!("GET {V1}/diff?")));

    let human = d.run(&["diff", "--from", "4090", "--repo", REPO]);
    assert_eq!(exit_of(&human), 0, "{}", stderr_of(&human));
    assert_eq!(stdout_of(&human), "@@ -1 +1 @@\n-a\n+b\n\n");

    // A server answering no v2: one 1-entry page of the whole list.
    let d = Deployment::new();
    d.mount(
        Mock::given(method("GET"))
            .and(path_matcher(format!("{V1}/versions")))
            .and(query_param("limit", "1"))
            .and(query_param("offset", "0"))
            .and(query_param_is_missing("path"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "data": [ver(4098, &["a.md"], Value::Null, "alice")],
                "total": 4098, "limit": 1, "offset": 0,
            }))),
    );
    mount_diff(&d);
    let out = d.run(&["diff", "--from", "4090", "--repo", REPO, "--json"]);
    assert_eq!(exit_of(&out), 0, "{}", stderr_of(&out));
    assert_eq!(document(&out), served);
    let v1: Vec<String> = d
        .log()
        .into_iter()
        .filter(|l| !l.contains("/api/v2/"))
        .collect();
    assert_eq!(v1.len(), 2, "{v1:?}");
}

// Rows 4 and 8: 120 versions since V, read in 2 pages; 4 requests in all
// with `--content`.
#[test]
#[serial]
fn the_walk_crosses_a_page_boundary() {
    let d = Deployment::new();
    let changed: HashMap<u32, Vec<&str>> = HashMap::from([
        (1001, vec!["a"]),
        (1120, vec!["b"]),
        (1030, vec!["c"]),
        (1101, vec!["c"]),
    ]);
    d.changes(
        "1000",
        "head",
        None,
        changes_body(
            1000,
            1120,
            vec![
                modified("a", b"a"),
                modified("b", b"b"),
                modified("c", b"c"),
            ],
        ),
    );
    d.page(0, versions(1120, 1021, &changed), 1120);
    d.page(100, versions(1020, 921, &changed), 1120);
    d.empty_pages(1120);
    d.read(Frames::of(
        1120,
        &[("a", b"a" as &[u8]), ("b", b"b"), ("c", b"c")],
    ));

    let out = d.run(&[
        "diff",
        "--from",
        "1000",
        "--provenance",
        "--content",
        "--repo",
        REPO,
        "--json",
    ]);

    assert_eq!(exit_of(&out), 0, "{}", stderr_of(&out));
    let doc = document(&out);
    let versions_of: Vec<Value> = doc["entries"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["provenance"]["version"].clone())
        .collect();
    assert_eq!(versions_of, vec![json!(1001), json!(1120), json!(1101)]);
    assert_eq!(doc["entries"][0]["provenance"]["run"], json!("r1001"));
    assert_eq!(d.pages(), vec!["0", "100"]);
    assert_eq!(d.log().len(), 4, "{:?}", d.log());
}

// Row 4a: a list ending above V, and an empty page past the scan limit,
// each end the walk after 2 pages; the entry no listed version names
// carries `null`.
#[test]
#[serial]
fn a_list_ending_above_from_ends_the_walk() {
    let d = Deployment::new();
    let changed: HashMap<u32, Vec<&str>> = HashMap::from([(1050, vec!["x"])]);
    d.changes(
        "1000",
        "head",
        None,
        changes_body(1000, 1150, vec![modified("x", b"x"), modified("y", b"y")]),
    );
    d.page(0, versions(1150, 1051, &changed), 150);
    d.page(100, versions(1050, 1001, &changed), 150);
    d.empty_pages(150);
    let out = d
        .command(d.work.path())
        .timeout(Duration::from_secs(5))
        .args([
            "diff",
            "--from",
            "1000",
            "--provenance",
            "--repo",
            REPO,
            "--json",
        ])
        .output()
        .expect("run");
    assert_eq!(exit_of(&out), 0, "{}", stderr_of(&out));
    let doc = document(&out);
    assert_eq!(doc["entries"][0]["provenance"]["version"], json!(1050));
    assert_eq!(doc["entries"][1]["provenance"], Value::Null);
    assert_eq!(d.pages(), vec!["0", "100"]);

    // The scan limit's shape: a full page above V, then `data: []`.
    let d = Deployment::new();
    d.changes(
        "1",
        "head",
        None,
        changes_body(1, 100_100, vec![modified("y", b"y")]),
    );
    d.page(0, versions(100_100, 100_001, &HashMap::new()), 100_000);
    d.empty_pages(100_000);
    let out = d
        .command(d.work.path())
        .timeout(Duration::from_secs(5))
        .args([
            "diff",
            "--from",
            "1",
            "--provenance",
            "--repo",
            REPO,
            "--json",
        ])
        .output()
        .expect("run");
    assert_eq!(exit_of(&out), 0, "{}", stderr_of(&out));
    assert_eq!(document(&out)["entries"][0]["provenance"], Value::Null);
    assert_eq!(d.pages().len(), 2, "{:?}", d.pages());
}

// Row 5: no provenance recorded, and a publisher alone.
#[test]
#[serial]
fn no_provenance_recorded_and_publisher_only() {
    let d = Deployment::new();
    d.changes(
        "10",
        "head",
        None,
        changes_body(
            10,
            12,
            vec![deleted("drafts/old-plan.md"), modified("p.md", b"p")],
        ),
    );
    d.page(
        0,
        vec![
            ver(12, &["p.md"], json!({"publisher": "maria"}), "maria"),
            ver(11, &["drafts/old-plan.md"], Value::Null, "bart"),
            ver(10, &["z"], Value::Null, "bart"),
        ],
        12,
    );

    let out = d.run(&[
        "diff",
        "--from",
        "10",
        "--provenance",
        "--repo",
        REPO,
        "--json",
    ]);
    assert_eq!(exit_of(&out), 0, "{}", stderr_of(&out));
    let doc = document(&out);
    assert_eq!(
        doc["entries"][0]["provenance"],
        json!({"version": 11, "publisher": "bart", "recorded": false})
    );
    assert_eq!(
        doc["entries"][1]["provenance"],
        json!({"version": 12, "publisher": "maria", "recorded": true})
    );

    let human = d.run(&["diff", "--from", "10", "--provenance", "--repo", REPO]);
    assert_eq!(exit_of(&human), 0, "{}", stderr_of(&human));
    assert_eq!(
        stdout_of(&human),
        "deleted   drafts/old-plan.md  bart   (no provenance recorded)\n\
         modified  p.md                maria\n\
         10 \u{2192} 12 \u{b7} 2 paths\n"
    );
    assert_eq!(stderr_of(&human), "");
}

// Row 6: V equal to the head: one request, an empty answer.
#[test]
#[serial]
fn from_equal_to_the_head_sends_one_request() {
    let d = Deployment::new();
    d.changes("4098", "head", None, changes_body(4098, 4098, vec![]));

    let out = d.run(&[
        "diff",
        "--from",
        "4098",
        "--provenance",
        "--content",
        "--repo",
        REPO,
        "--json",
    ]);
    assert_eq!(exit_of(&out), 0, "{}", stderr_of(&out));
    assert_eq!(
        document(&out),
        json!({"from": end(4098), "to": end(4098), "entries": [], "deferred": []})
    );
    assert_eq!(d.log().len(), 1);

    let human = d.run(&["diff", "--from", "4098", "--provenance", "--repo", REPO]);
    assert_eq!(exit_of(&human), 0, "{}", stderr_of(&human));
    assert_eq!(stdout_of(&human), "4098 \u{2192} 4098 \u{b7} no change\n");
}

// Row 7: a version landing after the answered head is skipped, and the
// overlap it leaves between pages is dropped.
#[test]
#[serial]
fn a_version_landing_after_the_answered_head_is_skipped() {
    let d = Deployment::new();
    let changed: HashMap<u32, Vec<&str>> = HashMap::from([
        (1101, vec!["docs/pricing.md"]),
        (1050, vec!["docs/pricing.md"]),
        (1001, vec!["q"]),
    ]);
    d.changes(
        "1000",
        "head",
        None,
        changes_body(
            1000,
            1100,
            vec![modified("docs/pricing.md", b"p"), modified("q", b"q")],
        ),
    );
    // 1101 landed before the first page; page 1 opens on 1002 again.
    d.page(0, versions(1101, 1002, &changed), 1101);
    d.page(100, versions(1002, 903, &changed), 1101);
    let out = d.run(&[
        "diff",
        "--from",
        "1000",
        "--provenance",
        "--repo",
        REPO,
        "--json",
    ]);
    assert_eq!(exit_of(&out), 0, "{}", stderr_of(&out));
    let doc = document(&out);
    assert_eq!(doc["to"], end(1100));
    assert_eq!(doc["entries"][0]["provenance"]["version"], json!(1050));
    assert_eq!(doc["entries"][1]["provenance"]["version"], json!(1001));
}

// Row 8a: with `--to` below the head the walk reads page 0, places `to`,
// then reads from there: never the pages wholly above it.
#[test]
#[serial]
fn a_to_below_the_head_reads_page_zero_place_then_the_jumped_page() {
    let changed: HashMap<u32, Vec<&str>> = HashMap::from([(1995, vec!["e"])]);
    let to = c(2000);
    let mount = |d: &Deployment| {
        d.changes(
            "1990",
            &to,
            None,
            changes_body(1990, 2000, vec![modified("e", b"e")]),
        );
        d.page(0, versions(2250, 2151, &changed), 2250);
        d.empty_pages(2250);
    };

    // A dense root: the place is refused, the jump is by number.
    let d = Deployment::new();
    mount(&d);
    d.page(250, versions(2000, 1901, &changed), 2250);
    d.mount(
        Mock::given(method("GET"))
            .and(path_matcher(format!("{V1}/versions/{to}/place")))
            .respond_with(
                ResponseTemplate::new(404)
                    .set_body_json(json!({"error": "not_found", "message": "Version not found"})),
            ),
    );
    let out = d.run(&[
        "diff",
        "--from",
        "1990",
        "--to",
        &to,
        "--provenance",
        "--repo",
        REPO,
        "--json",
    ]);
    assert_eq!(exit_of(&out), 0, "{}", stderr_of(&out));
    assert_eq!(
        document(&out)["entries"][0]["provenance"]["version"],
        json!(1995)
    );
    let log = d.log();
    assert_eq!(log.len(), 4, "{log:?}");
    assert!(
        log[1].contains("/versions?") && log[1].contains("offset=0"),
        "{log:?}"
    );
    assert_eq!(log[2], format!("GET {V1}/versions/{to}/place"));
    assert!(log[3].contains("offset=250"), "{log:?}");

    // A list with gaps: the place names the offset.
    let d = Deployment::new();
    mount(&d);
    d.page(180, versions(2000, 1901, &changed), 2250);
    d.mount(
        Mock::given(method("GET"))
            .and(path_matcher(format!("{V1}/versions/{to}/place")))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "version": ver(2000, &["e"], Value::Null, "maria"),
                "position": 180, "total": 2000, "newer": null, "older": null,
            }))),
    );
    let out = d.run(&[
        "diff",
        "--from",
        "1990",
        "--to",
        &to,
        "--provenance",
        "--repo",
        REPO,
        "--json",
    ]);
    assert_eq!(exit_of(&out), 0, "{}", stderr_of(&out));
    assert_eq!(
        document(&out)["entries"][0]["provenance"]["version"],
        json!(1995)
    );
    assert_eq!(d.pages(), vec!["0", "180"]);
}

// Row 9: a hash prefix costs one `EP-get-version`; one naming nothing is
// refused as that read answers it.
#[test]
#[serial]
fn a_hash_prefix_is_resolved_first() {
    let d = Deployment::new();
    d.mount(
        Mock::given(method("GET"))
            .and(path_matcher(format!("{V1}/versions/9f1c")))
            .respond_with(ResponseTemplate::new(200).set_body_json(ver(
                10,
                &["a"],
                Value::Null,
                "alice",
            ))),
    );
    d.mount(
        Mock::given(method("GET"))
            .and(path_matcher(format!("{V1}/versions/dead")))
            .respond_with(
                ResponseTemplate::new(404)
                    .set_body_json(json!({"error": "not_found", "message": "Version not found"})),
            ),
    );
    d.changes("10", "head", None, changes_body(10, 10, vec![]));

    let out = d.run(&[
        "diff",
        "--from",
        "9f1c",
        "--provenance",
        "--repo",
        REPO,
        "--json",
    ]);
    assert_eq!(exit_of(&out), 0, "{}", stderr_of(&out));
    let log = d.log();
    assert_eq!(log.len(), 2, "{log:?}");
    assert_eq!(log[0], format!("GET {V1}/versions/9f1c"));
    assert!(log[1].contains("from=10"), "{log:?}");

    let out = d.run(&[
        "diff",
        "--from",
        "dead",
        "--provenance",
        "--repo",
        REPO,
        "--json",
    ]);
    assert_eq!(exit_of(&out), 1);
    assert_eq!(
        document(&out),
        refusal("version_not_found", "version not found: dead")
    );
    assert_eq!(d.log().len(), 3);
}

// Row 11: a binary file reads `"text": null`, with no base64.
#[test]
#[serial]
fn a_binary_file_reads_text_null() {
    let d = Deployment::new();
    d.changes(
        "1",
        "head",
        None,
        changes_body(1, 2, vec![added("assets/logo.png", LOGO)]),
    );
    d.read(Frames::of(2, &[("assets/logo.png", LOGO)]));
    let out = d.run(&["diff", "--from", "1", "--content", "--repo", REPO, "--json"]);
    assert_eq!(exit_of(&out), 0, "{}", stderr_of(&out));
    let doc = document(&out);
    assert_eq!(doc["entries"][0]["text"], Value::Null);
    assert!(doc["entries"][0].as_object().unwrap().contains_key("text"));
    assert_eq!(doc["entries"][0]["size"], json!(LOGO.len()));
    assert!(!stdout_of(&out).contains("Base64") && !stdout_of(&out).contains("base64"));
}

// Row 12: past the batch's per-file limit, or `too_large` twice, a text
// is deferred and never read alone; exactly the limit is carried.
#[test]
#[serial]
fn texts_past_the_batch_limits_are_deferred() {
    let d = Deployment::new();
    let exact = vec![b'a'; 8_388_608];
    let big_entry = json!({"path": "data/big.csv", "status": "modified", "oldSha": "1".repeat(40), "newSha": "4".repeat(40), "size": 9_437_184});
    d.changes(
        "1",
        "head",
        None,
        changes_body(
            1,
            2,
            vec![
                big_entry,
                modified("data/exact.txt", &exact),
                modified("data/huge.txt", b"h"),
                modified("data/small.txt", b"s"),
            ],
        ),
    );
    let mut frames = Frames::of(
        2,
        &[
            ("data/exact.txt", &exact),
            ("data/huge.txt", b"h"),
            ("data/small.txt", b"s"),
        ],
    );
    frames.too_large.insert("data/huge.txt".to_string());
    d.read(frames);

    let out = d.run(&["diff", "--from", "1", "--content", "--repo", REPO, "--json"]);
    assert_eq!(exit_of(&out), 0, "{}", stderr_of(&out));
    let doc = document(&out);
    assert_eq!(doc["deferred"], json!(["data/big.csv", "data/huge.txt"]));
    assert!(!doc["entries"][0].as_object().unwrap().contains_key("text"));
    assert_eq!(
        doc["entries"][1]["text"].as_str().map(str::len),
        Some(8_388_608)
    );
    assert!(!doc["entries"][2].as_object().unwrap().contains_key("text"));
    assert_eq!(doc["entries"][3]["text"], json!("s"));
    let reads = d.reads();
    assert_eq!(reads.len(), 2, "the too_large path is asked once more");
    for body in &reads {
        assert!(!body.to_string().contains("big.csv"));
    }
    assert_eq!(reads[1]["paths"], json!([{"path": "data/huge.txt"}]));
    assert!(
        d.requests()
            .iter()
            .all(|r| !r.url.path().starts_with("/api/v1/")),
        "{:?}",
        d.log()
    );
}

// Row 13: the read contradicting the changes answer fails the run.
#[test]
#[serial]
fn a_read_contradicting_the_changes_answer_fails() {
    let d = Deployment::new();
    d.changes(
        "1",
        "head",
        None,
        changes_body(1, 2, vec![modified("docs/a.md", b"a")]),
    );
    d.read(Frames::of(2, &[]));
    let out = d.run(&["diff", "--from", "1", "--content", "--repo", REPO, "--json"]);
    assert_eq!(exit_of(&out), 1);
    assert_eq!(
        document(&out),
        refusal(
            "response_undecodable",
            "server error (200): invalid response body: docs/a.md stands at version 2 in the changes answer but not in the read"
        )
    );
}

// Row 14a: a write refused as stale, then the diff to its `currentSha`.
#[test]
#[serial]
fn the_refused_write_then_diff_to_its_current_sha() {
    let d = Deployment::new();
    let current = c(2000);
    d.mount(
        Mock::given(method("GET"))
            .and(path_matcher(V1))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "owner": "alice", "name": "notes", "description": null,
                "commitSha": c(2250), "status": "active", "author": null, "tags": [],
                "visibility": "public", "forkedFrom": null, "forkCount": 0,
                "fileCount": 3, "role": null,
                "createdAt": "2026-01-01T00:00:00Z", "updatedAt": "2026-01-01T00:00:00Z",
            }))),
    );
    d.mount(
        Mock::given(method("PUT"))
            .and(path_matcher(format!("{V1}/push")))
            .respond_with(ResponseTemplate::new(409).set_body_json(json!({
                "error": "conflict", "message": "Head mismatch", "currentSha": current,
            }))),
    );
    let write = d
        .command(d.work.path())
        .args([
            "--json",
            "write",
            "docs/pricing.md",
            "--parent",
            &c(1990),
            "--repo",
            REPO,
        ])
        .write_stdin("new\n")
        .output()
        .expect("write");
    assert_eq!(exit_of(&write), 7, "{}", stderr_of(&write));
    let refused = document(&write);
    let current_sha = refused["currentSha"].as_str().expect("currentSha");
    assert_eq!(current_sha, current);

    let changed: HashMap<u32, Vec<&str>> =
        HashMap::from([(1995, vec!["docs/pricing.md"]), (1999, vec!["docs/faq.md"])]);
    let entries = vec![
        modified("docs/faq.md", b"faq"),
        modified("docs/pricing.md", PRICING),
    ];
    d.changes(
        "1990",
        current_sha,
        Some("docs"),
        changes_body(1990, 2000, entries.clone()),
    );
    d.page(0, versions(2250, 2151, &changed), 2250);
    d.page(250, versions(2000, 1901, &changed), 2250);
    d.empty_pages(2250);
    d.mount(
        Mock::given(method("GET"))
            .and(path_regex(r"/versions/[0-9a-f]+/place$"))
            .respond_with(ResponseTemplate::new(404).set_body_json(json!({"error": "not_found"}))),
    );
    d.read(Frames::of(
        2000,
        &[
            ("docs/faq.md", b"faq" as &[u8]),
            ("docs/pricing.md", PRICING),
        ],
    ));
    let before = d.requests().len();

    let out = d.run(&[
        "diff",
        "--from",
        "1990",
        "--to",
        current_sha,
        "--provenance",
        "--content",
        "--path",
        "docs/",
        "--repo",
        REPO,
        "--json",
    ]);
    assert_eq!(exit_of(&out), 0, "{}", stderr_of(&out));
    let doc = document(&out);
    assert_eq!(doc["to"], end(2000));
    for (i, entry) in doc["entries"].as_array().unwrap().iter().enumerate() {
        let mut served = entry.clone();
        let map = served.as_object_mut().unwrap();
        assert!(map.remove("text").is_some_and(|t| t.is_string()));
        assert!(map.remove("provenance").is_some_and(|p| p.is_object()));
        assert_eq!(served, entries[i]);
    }
    assert_eq!(doc["entries"][0]["provenance"]["version"], json!(1999));
    assert_eq!(doc["entries"][1]["provenance"]["version"], json!(1995));
    let log: Vec<String> = d.log().split_off(before);
    assert_eq!(log.len(), 5, "{log:?}");
    assert!(log[0].contains("/changes?") && log[0].contains(&format!("to={current_sha}")));
    let reads = d.reads();
    assert_eq!(reads.len(), 1);
    assert_eq!(reads[0]["at"], json!(current_sha));
    assert_eq!(d.pages(), vec!["0", "250"]);
}

// Row 16: `--to` without `--from`, in either mode, before any request.
#[test]
#[serial]
fn to_without_from_is_refused_in_both_modes() {
    let d = Deployment::new();
    for args in [
        vec!["diff", "--to", "5", "--repo", REPO, "--json"],
        vec![
            "diff",
            "--to",
            "5",
            "--provenance",
            "--repo",
            REPO,
            "--json",
        ],
    ] {
        let out = d.run(&args);
        assert_eq!(exit_of(&out), 1);
        assert_eq!(
            document(&out),
            config("--to needs --from: name the older version too, or drop --to")
        );
    }
    let human = d.run(&["diff", "--to", "5", "--repo", REPO]);
    assert_eq!(exit_of(&human), 1);
    assert!(
        stderr_of(&human).contains("--to needs --from: name the older version too, or drop --to"),
        "{}",
        stderr_of(&human)
    );
    assert!(d.requests().is_empty());
}

// Row 17: the new mode without `--from`, before any request.
#[test]
#[serial]
fn the_new_mode_without_from_is_refused() {
    let d = Deployment::new();
    for args in [
        vec!["diff", "--provenance", "--repo", REPO, "--json"],
        vec!["diff", "--content", "--repo", REPO, "--json"],
    ] {
        let out = d.run(&args);
        assert_eq!(exit_of(&out), 1);
        assert_eq!(
            document(&out),
            config(
                "--provenance and --content need --from: name the version the answer starts from"
            )
        );
    }
    assert!(d.requests().is_empty());
}

// Row 18: `--from` unknown or newer than the newer end.
#[test]
#[serial]
fn from_newer_or_unknown() {
    let d = Deployment::new();
    let not_found = |message: &str| {
        ResponseTemplate::new(404).set_body_json(json!({"error": "not_found", "message": message}))
    };
    d.mount(
        Mock::given(method("GET"))
            .and(path_matcher(format!("{V2}/changes")))
            .and(query_param("from", "999999"))
            .respond_with(not_found("Version not found")),
    );
    d.mount(
        Mock::given(method("GET"))
            .and(path_matcher(format!("{V2}/changes")))
            .and(query_param("to", "999999"))
            .respond_with(not_found("Version not found")),
    );
    d.mount(
        Mock::given(method("GET"))
            .and(path_matcher("/api/v2/repos/alice/gone/changes"))
            .respond_with(not_found("Repository not found")),
    );
    d.changes(&c(4098), "4090", None, changes_body(4098, 4090, vec![]));

    let out = d.run(&[
        "diff",
        "--from",
        "999999",
        "--provenance",
        "--repo",
        REPO,
        "--json",
    ]);
    assert_eq!(exit_of(&out), 1);
    assert_eq!(
        document(&out),
        refusal("version_not_found", "version not found: 999999")
    );
    let out = d.run(&[
        "diff",
        "--from",
        "4090",
        "--to",
        "999999",
        "--provenance",
        "--repo",
        REPO,
        "--json",
    ]);
    assert_eq!(
        document(&out),
        refusal("version_not_found", "version not found: 4090 or 999999")
    );
    let out = d.run(&[
        "diff",
        "--from",
        "1",
        "--provenance",
        "--repo",
        "alice/gone",
        "--json",
    ]);
    assert_eq!(exit_of(&out), 1);
    assert_eq!(
        document(&out),
        refusal("repo_not_found", "repository not found: alice/gone")
    );

    let before = d.requests().len();
    let out = d.run(&[
        "diff",
        "--from",
        "4098",
        "--to",
        "4090",
        "--provenance",
        "--repo",
        REPO,
        "--json",
    ]);
    assert_eq!(exit_of(&out), 1);
    assert_eq!(
        document(&out),
        config("--from 4098 is newer than --to 4090: name the older version first")
    );
    assert_eq!(d.requests().len(), before, "refused before any request");

    // A hash end: the answer's numbers, after the one changes request.
    let out = d.run(&[
        "diff",
        "--from",
        &c(4098),
        "--to",
        "4090",
        "--provenance",
        "--repo",
        REPO,
        "--json",
    ]);
    assert_eq!(exit_of(&out), 1);
    assert_eq!(
        document(&out),
        config("--from 4098 is newer than --to 4090: name the older version first")
    );
    assert_eq!(d.requests().len(), before + 1);
}

// Row 19: empty and below-one ends, before any request; a non-UTF-8 value
// is the parser's.
#[test]
#[serial]
fn empty_and_below_one_ends_are_refused() {
    let d = Deployment::new();
    let out = d.run(&[
        "diff",
        "--from",
        "",
        "--provenance",
        "--repo",
        REPO,
        "--json",
    ]);
    assert_eq!(exit_of(&out), 1);
    assert_eq!(document(&out), config("version cannot be empty"));
    let out = d.run(&[
        "diff",
        "--from",
        "0",
        "--provenance",
        "--repo",
        REPO,
        "--json",
    ]);
    assert_eq!(exit_of(&out), 1);
    assert_eq!(document(&out), config("version must be \u{2265} 1"));
    assert!(d.requests().is_empty());

    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt;
        for flag in ["--from", "--to", "--path"] {
            let out = d
                .command(d.work.path())
                .args(["diff", "--provenance", "--repo", REPO, "--json"])
                .arg(flag)
                .arg(std::ffi::OsStr::from_bytes(b"\xff\xfe"))
                .output()
                .expect("run");
            assert_eq!(exit_of(&out), 2, "{flag}");
            assert!(stdout_of(&out).is_empty(), "{flag}");
        }
        assert!(d.requests().is_empty());
    }
}

// Rows 20 and 22: `--path` narrows the changes request, never the walk.
#[test]
#[serial]
fn path_narrows_the_changes_request() {
    let d = Deployment::new();
    d.changes(
        "10",
        "head",
        Some("docs"),
        changes_body(
            10,
            20,
            vec![
                modified("docs/a.md", b"a"),
                added("docs/x.md", b"x"),
                deleted("docs/y.md"),
            ],
        ),
    );
    let changed: HashMap<u32, Vec<&str>> = HashMap::from([
        (19, vec!["src/z.ts"]),
        (15, vec!["docs/a.md"]),
        (13, vec!["notes/x.md", "docs/x.md"]),
        (12, vec!["docs/y.md", "old/y.md"]),
    ]);
    d.page(0, versions(20, 1, &changed), 20);
    let out = d.run(&[
        "diff",
        "--from",
        "10",
        "--path",
        "docs/",
        "--provenance",
        "--repo",
        REPO,
        "--json",
    ]);
    assert_eq!(exit_of(&out), 0, "{}", stderr_of(&out));
    let doc = document(&out);
    let got: Vec<(Value, Value)> = doc["entries"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| (e["path"].clone(), e["provenance"]["version"].clone()))
        .collect();
    assert_eq!(
        got,
        vec![
            (json!("docs/a.md"), json!(15)),
            (json!("docs/x.md"), json!(13)),
            (json!("docs/y.md"), json!(12)),
        ]
    );
    for page in d.requests_to("/versions") {
        assert!(query_of(&page, "path").is_none());
    }
}

/// The `path` each changes request carried, `None` where it carried none.
fn changes_paths(d: &Deployment) -> Vec<Option<String>> {
    d.requests_to("/changes")
        .iter()
        .map(|r| query_of(r, "path"))
        .collect()
}

fn mount_any_changes(d: &Deployment) {
    d.mount(
        Mock::given(method("GET"))
            .and(path_matcher(format!("{V2}/changes")))
            .and(query_param("path", "/docs"))
            .respond_with(
                ResponseTemplate::new(422)
                    .set_body_json(json!({"error": "validation_error", "message": "Invalid path"})),
            )
            .with_priority(1),
    );
    d.mount(
        Mock::given(method("GET"))
            .and(path_matcher(format!("{V2}/changes")))
            .respond_with(ResponseTemplate::new(200).set_body_json(changes_body(1, 2, vec![]))),
    );
}

// Row 23, at a root: each `--path` value as it is sent or refused.
#[test]
#[serial]
fn path_edge_values_at_a_root() {
    let d = Deployment::new();
    mount_any_changes(&d);
    let run = |p: &str| {
        d.run(&[
            "diff",
            "--from",
            "1",
            "--provenance",
            "--path",
            p,
            "--repo",
            REPO,
            "--json",
        ])
    };
    for (typed, sent) in [
        ("", None),
        (".", None),
        ("a/../docs", Some("docs")),
        ("2024/", Some("2024")),
        ("docs/pricing.md", Some("docs/pricing.md")),
        ("nowhere/at/all", Some("nowhere/at/all")),
    ] {
        let before = changes_paths(&d).len();
        let out = run(typed);
        assert_eq!(exit_of(&out), 0, "{typed}: {}", stderr_of(&out));
        assert_eq!(document(&out)["entries"], json!([]), "{typed}");
        let paths = changes_paths(&d);
        assert_eq!(paths.len(), before + 1, "{typed}");
        assert_eq!(paths[before].as_deref(), sent, "{typed}");
    }
    let out = run("..");
    assert_eq!(exit_of(&out), 1);
    assert_eq!(
        document(&out),
        config(".. names no path inside the repository; name one counted from its root")
    );
    let out = run("/docs");
    assert_eq!(exit_of(&out), 1);
    assert_eq!(document(&out)["code"], json!("validation_error"));
    assert_eq!(changes_paths(&d).last().unwrap().as_deref(), Some("/docs"));
    // No history page for an empty answer.
    assert!(d.requests_to("/versions").is_empty());
}

/// A checkout `W` of `alice/notes` holding the folder `clients/vela`,
/// bound to its holder.
fn checkout_with_folder(d: &Deployment) -> PathBuf {
    let w = std::fs::canonicalize(d.work.path()).expect("W");
    std::fs::write(w.join(".syns.yaml"), "owner: alice\nname: notes\n").expect("W identity");
    let folder = w.join("clients/vela");
    std::fs::create_dir_all(&folder).expect("folder");
    std::fs::write(
        folder.join(".syns.yaml"),
        "holder: alice/notes\npath: clients/vela\n",
    )
    .expect("folder identity");
    folder
}

// Row 23, inside a folder bound to its holder.
#[test]
#[serial]
fn path_edge_values_inside_a_folder() {
    let d = Deployment::new();
    let folder = checkout_with_folder(&d);
    mount_any_changes(&d);
    let run = |p: Option<&str>| {
        let mut args = vec!["diff", "--from", "1", "--provenance", "--json"];
        if let Some(p) = p {
            args.extend(["--path", p]);
        }
        d.run_in(&folder, &args)
    };
    for (typed, sent) in [
        (None, "clients/vela"),
        (Some("."), "clients/vela"),
        (Some("a/../docs"), "clients/vela/docs"),
        (Some("2024/"), "clients/vela/2024"),
        (Some("docs/pricing.md"), "clients/vela/docs/pricing.md"),
    ] {
        let before = changes_paths(&d).len();
        let out = run(typed);
        assert_eq!(exit_of(&out), 0, "{typed:?}: {}", stderr_of(&out));
        let paths = changes_paths(&d);
        assert_eq!(paths[before].as_deref(), Some(sent), "{typed:?}");
    }
    let before = d.requests().len();
    let out = run(Some(""));
    assert_eq!(document(&out), config("path cannot be empty"));
    for typed in ["..", "/docs"] {
        let out = run(Some(typed));
        assert_eq!(exit_of(&out), 1);
        assert_eq!(
            document(&out),
            config(&format!(
                "{typed} names no path inside the folder {}; name one counted from it",
                folder.display()
            ))
        );
    }
    assert_eq!(d.requests().len(), before);
}

// Row 24: a rename keeps one entry, its newest change's provenance, its
// new path's text, and the human line naming both paths.
#[test]
#[serial]
fn rename_status_and_human_line() {
    let d = Deployment::new();
    d.changes(
        "10",
        "head",
        None,
        changes_body(10, 17, vec![renamed("notes/a.md", "notes/b.md", NOTES_B)]),
    );
    let changed: HashMap<u32, Vec<&str>> = HashMap::from([
        (14, vec!["notes/a.md", "notes/b.md"]),
        (16, vec!["notes/b.md"]),
    ]);
    d.page(0, versions(17, 1, &changed), 17);
    d.read(Frames::of(17, &[("notes/b.md", NOTES_B)]));

    let out = d.run(&[
        "diff",
        "--from",
        "10",
        "--provenance",
        "--content",
        "--repo",
        REPO,
        "--json",
    ]);
    assert_eq!(exit_of(&out), 0, "{}", stderr_of(&out));
    let doc = document(&out);
    assert_eq!(doc["entries"][0]["provenance"]["version"], json!(16));
    assert_eq!(doc["entries"][0]["text"], json!("notes b\n"));
    assert_eq!(doc["entries"][0]["oldPath"], json!("notes/a.md"));

    let human = d.run(&["diff", "--from", "10", "--provenance", "--repo", REPO]);
    assert_eq!(
        stdout_of(&human),
        "renamed  notes/a.md \u{2192} notes/b.md  maria  bb \u{b7} run r16\n\
         10 \u{2192} 17 \u{b7} 1 path\n"
    );

    // With no later change, the rename's own version.
    let d = Deployment::new();
    d.changes(
        "10",
        "head",
        None,
        changes_body(10, 17, vec![renamed("notes/a.md", "notes/b.md", NOTES_B)]),
    );
    let changed: HashMap<u32, Vec<&str>> = HashMap::from([(14, vec!["notes/a.md", "notes/b.md"])]);
    d.page(0, versions(17, 1, &changed), 17);
    let out = d.run(&[
        "diff",
        "--from",
        "10",
        "--provenance",
        "--repo",
        REPO,
        "--json",
    ]);
    assert_eq!(
        document(&out)["entries"][0]["provenance"]["version"],
        json!(14)
    );
}

// Row 26: `--content` without `--json` is the parser's refusal.
#[test]
#[serial]
fn content_without_json_is_a_parser_refusal() {
    let d = Deployment::new();
    let out = d.run(&["diff", "--from", "1", "--content", "--repo", REPO]);
    assert_eq!(exit_of(&out), 2);
    assert!(stdout_of(&out).is_empty());
    assert!(
        stderr_of(&out)
            .contains("--content needs --json: the texts are carried only in the JSON answer"),
        "{}",
        stderr_of(&out)
    );
    assert!(d.requests().is_empty());
}

// Row 27: plain `--path` keeps each `EP-diff` entry whose `path` or
// `oldPath` lies at or under it, whole, sending what it sent without.
#[test]
#[serial]
fn plain_path_keeps_entries_by_path_or_old_path() {
    let entry = |path: &str, old: Option<&str>, status: &str| {
        let mut e = json!({"path": path, "status": status, "diff": format!("patch of {path}\n")});
        if let Some(old) = old {
            e["oldPath"] = json!(old);
        }
        e
    };
    let served = json!({
        "from": {"version": 10, "sha": c(10)},
        "to": {"version": 20, "sha": c(20)},
        "files": [
            entry("docs/a.md", None, "modified"),
            entry("docs/x.md", Some("notes/x.md"), "renamed"),
            entry("old/y.md", Some("docs/y.md"), "renamed"),
            entry("src/z.ts", None, "modified"),
            entry("docsier/q.md", None, "modified"),
        ],
    });
    let d = Deployment::new();
    d.head(20);
    d.mount(
        Mock::given(method("GET"))
            .and(path_matcher(format!("{V1}/diff")))
            .and(query_param("from", "10"))
            .and(query_param("to", "20"))
            .respond_with(ResponseTemplate::new(200).set_body_json(served.clone())),
    );
    let out = d.run(&[
        "diff", "--from", "10", "--path", "docs/", "--repo", REPO, "--json",
    ]);
    assert_eq!(exit_of(&out), 0, "{}", stderr_of(&out));
    let mut expected = served.clone();
    expected["files"] = json!([
        served["files"][0].clone(),
        served["files"][1].clone(),
        served["files"][2].clone(),
    ]);
    assert_eq!(document(&out), expected);
    assert_eq!(d.log().len(), 2, "{:?}", d.log());

    let human = d.run(&["diff", "--from", "10", "--path", "docs", "--repo", REPO]);
    assert_eq!(
        stdout_of(&human),
        "patch of docs/a.md\n\n\npatch of docs/x.md\n\n\npatch of old/y.md\n\n"
    );
    let none = d.run(&["diff", "--from", "10", "--path", "nothing", "--repo", REPO]);
    assert_eq!(exit_of(&none), 0);
    assert!(
        format!("{}{}", stdout_of(&none), stderr_of(&none)).contains("No differences found."),
        "{}",
        stdout_of(&none)
    );

    // Inside a folder bound to its holder, P counts from the folder.
    let d = Deployment::new();
    let folder = checkout_with_folder(&d);
    d.head(20);
    d.mount(
        Mock::given(method("GET"))
            .and(path_matcher(format!("{V1}/diff")))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "from": {"version": 10, "sha": c(10)},
                "to": {"version": 20, "sha": c(20)},
                "files": [
                    entry("clients/vela/docs/a.md", None, "modified"),
                    entry("clients/vela/b.md", None, "modified"),
                    entry("docs/a.md", None, "modified"),
                ],
            }))),
    );
    let out = d.run_in(
        &folder,
        &["diff", "--from", "10", "--path", "docs", "--json"],
    );
    assert_eq!(exit_of(&out), 0, "{}", stderr_of(&out));
    let files = document(&out)["files"].clone();
    assert_eq!(files.as_array().unwrap().len(), 1, "{files}");
    assert_eq!(files[0]["path"], json!("docs/a.md"));
}

// Row 28: `--content` alone reads no history.
#[test]
#[serial]
fn content_without_provenance_reads_no_history() {
    let d = Deployment::new();
    mount_sample(&d);
    let out = d.run(&[
        "diff",
        "--from",
        "4090",
        "--content",
        "--repo",
        REPO,
        "--json",
    ]);
    assert_eq!(exit_of(&out), 0, "{}", stderr_of(&out));
    let doc = document(&out);
    for entry in doc["entries"].as_array().unwrap() {
        assert!(entry.get("provenance").is_none(), "{entry}");
    }
    assert!(d.requests_to("/versions").is_empty());
    assert_eq!(d.log().len(), 2);
}

// Row 29: `--provenance` alone reads no text.
#[test]
#[serial]
fn provenance_without_content_reads_no_text() {
    let d = Deployment::new();
    mount_sample(&d);
    let out = d.run(&[
        "diff",
        "--from",
        "4090",
        "--provenance",
        "--repo",
        REPO,
        "--json",
    ]);
    assert_eq!(exit_of(&out), 0, "{}", stderr_of(&out));
    let doc = document(&out);
    assert!(doc.get("deferred").is_none());
    for entry in doc["entries"].as_array().unwrap() {
        assert!(entry.get("text").is_none(), "{entry}");
    }
    assert!(d.requests_to("/read").is_empty());
}

// Row 30: `--if-repo` with no identity and no `--repo` sends nothing.
#[test]
#[serial]
fn if_repo_skips_both_modes() {
    let d = Deployment::new();
    for args in [
        vec!["diff", "--from", "1", "--provenance", "--if-repo"],
        vec!["diff", "--from", "1", "--if-repo"],
    ] {
        let out = d.run(&args);
        assert_eq!(exit_of(&out), 0, "{}", stderr_of(&out));
    }
    assert!(d.requests().is_empty());
}

// Row 31a: a folder bound to its holder: the changes request carries the
// folder's path, the walk the holder's whole list matched at holder
// paths, the read holder paths, and the answer counts from the folder.
#[test]
#[serial]
fn a_folder_bound_to_its_holder() {
    let d = Deployment::new();
    let folder = checkout_with_folder(&d);
    let a = b"a text\n";
    d.changes(
        "10",
        "head",
        Some("clients/vela"),
        json!({
            "from": end(10), "to": end(13),
            "entries": [
                modified("clients/vela/a.md", a),
                {"path": "clients/vela/b.md", "status": "deleted", "oldSha": "2".repeat(40)},
                {"path": "clients/vela/img.png", "status": "added", "newSha": "5".repeat(40), "size": 9 * 1024 * 1024},
            ],
        }),
    );
    d.page(
        0,
        vec![
            ver(
                13,
                &["clients/vela/a.md", "other/z.md"],
                prov("maria", Some("r13")),
                "maria",
            ),
            ver(12, &["clients/vela/b.md"], prov("ben", None), "ben"),
            ver(11, &["a.md"], prov("cleo", None), "cleo"),
            ver(10, &["x"], prov("cleo", None), "cleo"),
        ],
        13,
    );
    d.mount(
        Mock::given(method("POST"))
            .and(path_matcher(format!("{V2}/read")))
            .and(body_string_contains("\"clients/vela/a.md\""))
            .and(body_string_contains(c(13).as_str()))
            .respond_with(Frames::of(13, &[("clients/vela/a.md", a)])),
    );

    let out = d.run_in(
        &folder,
        &[
            "diff",
            "--from",
            "10",
            "--provenance",
            "--content",
            "--json",
        ],
    );
    assert_eq!(exit_of(&out), 0, "{}", stderr_of(&out));
    let doc = document(&out);
    let entries = doc["entries"].as_array().unwrap();
    assert_eq!(entries[0]["path"], json!("a.md"));
    assert_eq!(entries[0]["provenance"]["version"], json!(13));
    assert_eq!(entries[0]["text"], json!("a text\n"));
    assert_eq!(entries[1]["path"], json!("b.md"));
    assert_eq!(entries[1]["provenance"]["version"], json!(12));
    assert_eq!(entries[2]["path"], json!("img.png"));
    assert_eq!(entries[2]["provenance"], Value::Null);
    assert!(entries[2].get("text").is_none());
    assert_eq!(doc["deferred"], json!(["img.png"]));
    for page in d.requests_to("/versions") {
        assert!(query_of(&page, "path").is_none());
    }
    for body in d.reads() {
        assert_eq!(body["paths"], json!([{"path": "clients/vela/a.md"}]));
    }
    for r in d.requests() {
        let line = format!("{} {}", r.url, String::from_utf8_lossy(&r.body));
        assert!(
            !line.contains("=a.md") && !line.contains("\"a.md\""),
            "{line}"
        );
    }
}

// Row 13, the coordinator's ruling: a submodule (gitlink) under
// `--content` is never sent to the batch read and reads as a file that is
// not text, beside its hash; the run does not fail. The changes answer
// carries no mode: an entry still standing that it gives no `size` is
// the gitlink (the engine sizes every blob it lists).
#[test]
#[serial]
fn a_submodule_is_not_read_and_reads_text_null() {
    let d = Deployment::new();
    let gitlink = "9".repeat(40);
    d.changes(
        "1",
        "head",
        None,
        changes_body(
            1,
            2,
            vec![
                modified("docs/a.md", b"a"),
                json!({"path": "vendor/lib", "status": "added", "newSha": gitlink}),
                json!({"path": "vendor/other", "status": "modified", "oldSha": "8".repeat(40), "newSha": gitlink}),
            ],
        ),
    );
    // The batch answers `not_found` for a gitlink, as `EP-v2-read` does.
    d.read(Frames::of(2, &[("docs/a.md", b"a")]));

    let out = d.run(&["diff", "--from", "1", "--content", "--repo", REPO, "--json"]);
    assert_eq!(exit_of(&out), 0, "{}{}", stdout_of(&out), stderr_of(&out));
    let doc = document(&out);
    assert_eq!(doc["entries"][0]["text"], json!("a"));
    for i in [1, 2] {
        let entry = doc["entries"][i].as_object().unwrap();
        assert_eq!(entry.get("text"), Some(&Value::Null), "{entry:?}");
        assert_eq!(entry.get("newSha"), Some(&json!(gitlink)));
        assert!(entry.get("size").is_none());
    }
    assert_eq!(doc["deferred"], json!([]));
    let reads = d.reads();
    assert_eq!(reads.len(), 1);
    assert_eq!(reads[0]["paths"], json!([{"path": "docs/a.md"}]));

    // A submodule alone sends no read at all.
    let d = Deployment::new();
    d.changes(
        "1",
        "head",
        None,
        changes_body(
            1,
            2,
            vec![json!({"path": "vendor/lib", "status": "added", "newSha": gitlink})],
        ),
    );
    let out = d.run(&["diff", "--from", "1", "--content", "--repo", REPO, "--json"]);
    assert_eq!(exit_of(&out), 0, "{}", stderr_of(&out));
    assert_eq!(document(&out)["entries"][0]["text"], Value::Null);
    assert!(d.requests_to("/read").is_empty());
}
