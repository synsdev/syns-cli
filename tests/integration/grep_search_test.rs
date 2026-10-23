//! Binary-level behaviour of `syns grep` over `EP-v2-search`, and of its
//! fallback to the client-side search (roadmap/215 FEATURE_SPEC
//! § Expected, CLI rows 14–22; D-251, D-252 §1, §3, §6, §8, §9).
//!
//! The server is a stub built to the spec's wire contract: a fake engine
//! answering `POST /api/v2/repos/{owner}/{name}/search` from the same
//! fixture the stub's v1 reads (repository record, version, tree, file)
//! serve to the fallback, its rows found with the binary's own matcher
//! and globs, paged by row with `total`, `truncated`, `nextOffset`, the
//! page's `skipped` span and `skippedTotal`. A stub answering no search
//! route is the base server. Row 21a's cut bodies come from a raw TCP
//! stub, which no HTTP mock can cut.

use std::collections::BTreeMap;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};

use assert_cmd::Command as AssertCommand;
use serde_json::{Value, json};
use serial_test::serial;
use syns_cli::commands::grep::{build_matcher, compile_glob, glob_admits, search_text};
use syns_cli::push::hash::blob_sha1;
use tempfile::TempDir;
use wiremock::matchers::{method, path as path_matcher, path_regex};
use wiremock::{Mock, MockServer, Request, ResponseTemplate};

const REPO: &str = "alice/notes";

fn sha_of(version: u32) -> String {
    format!("{version:02x}").repeat(20)
}

/// One version's files, path to content, in ascending byte order.
type Files = BTreeMap<String, String>;

/// The repository the stub serves: each version's files, and the version
/// its head stands at (moved by a test to stand for a push).
#[derive(Clone)]
struct Fixture {
    versions: Arc<BTreeMap<u32, Files>>,
    head: Arc<AtomicU32>,
}

impl Fixture {
    fn one(files: Files) -> Fixture {
        Fixture::at(vec![(7, files)], 7)
    }

    fn at(versions: Vec<(u32, Files)>, head: u32) -> Fixture {
        Fixture {
            versions: Arc::new(versions.into_iter().collect()),
            head: Arc::new(AtomicU32::new(head)),
        }
    }
}

/// What the stub answers on the search route.
#[derive(Clone)]
enum Search {
    /// No route: the base server, its unmatched route a bare `404`.
    Absent,
    /// The fake engine, to the wire contract.
    Engine,
    /// One fixed answer: a status, a content type and a body.
    Fixed(u16, &'static str, String),
}

fn files(entries: &[(&str, &str)]) -> Files {
    entries
        .iter()
        .map(|(path, content)| (path.to_string(), content.to_string()))
        .collect()
}

fn is_binary(content: &str) -> bool {
    content.contains('\0')
}

/// The fake engine's answer to one search body over `files`, written as
/// the server relays it: `{"commit":…,"version":…,` then the engine body.
fn engine_answer(files: &Files, version: u32, body: &Value) -> ResponseTemplate {
    let refused = |status: u16, error: &str, message: &str| {
        ResponseTemplate::new(status).set_body_json(json!({ "error": error, "message": message }))
    };
    let allowed = [
        "at",
        "pattern",
        "ignoreCase",
        "before",
        "after",
        "output",
        "path",
        "glob",
        "offset",
        "limit",
    ];
    let Some(object) = body.as_object() else {
        return refused(422, "validation_error", "body");
    };
    if let Some(unknown) = object.keys().find(|k| !allowed.contains(&k.as_str())) {
        return refused(422, "validation_error", unknown);
    }
    let pattern = body["pattern"].as_str().unwrap_or_default();
    let ignore_case = body["ignoreCase"].as_bool().unwrap_or(false);
    let before = body["before"].as_u64().unwrap_or(0) as usize;
    let after = body["after"].as_u64().unwrap_or(0) as usize;
    let output = body["output"].as_str().unwrap_or("content").to_string();
    let prefix = body["path"].as_str().unwrap_or_default().to_string();
    let offset = body["offset"].as_u64().unwrap_or(0) as usize;
    let limit = body["limit"].as_u64().unwrap_or(1000) as usize;
    if !(1..=1000).contains(&limit) {
        return refused(422, "validation_error", "limit");
    }
    let Ok(matcher) = build_matcher(pattern, ignore_case) else {
        return refused(422, "validation_error", "invalid pattern");
    };
    let mut rules = Vec::new();
    for value in body["glob"].as_array().cloned().unwrap_or_default() {
        match compile_glob(value.as_str().unwrap_or_default()) {
            Ok(rule) => rules.push(rule),
            Err(_) => return refused(422, "validation_error", "invalid glob"),
        }
    }
    let under = |path: &str| {
        prefix.is_empty() || (path.len() > prefix.len() && path.starts_with(&format!("{prefix}/")))
    };
    if !files.keys().any(|p| under(p)) {
        return refused(404, "not_found", "Not found");
    }

    // Each searched path in byte order: its rows, or binary.
    enum Found {
        Rows(Vec<Value>),
        Binary,
    }
    let mut searched: Vec<(String, Found)> = Vec::new();
    for (path, content) in files {
        if !under(path) || !glob_admits(&rules, path) {
            continue;
        }
        if is_binary(content) {
            searched.push((path.clone(), Found::Binary));
            continue;
        }
        let rows = search_text(&matcher, content, before, after).expect("search");
        if rows.is_empty() {
            continue;
        }
        let rows: Vec<Value> = match output.as_str() {
            "files" => vec![json!(path)],
            "count" => vec![json!({ "path": path, "count": rows.len() })],
            _ => rows
                .iter()
                .map(|row| {
                    json!({
                        "path": path,
                        "line": row.line,
                        "text": row.text,
                        "context": row.context.iter()
                            .map(|c| json!({ "line": c.line, "text": c.text }))
                            .collect::<Vec<_>>(),
                    })
                })
                .collect(),
        };
        searched.push((path.clone(), Found::Rows(rows)));
    }

    // The row set, each row beside the index of its path.
    let mut set: Vec<(usize, Value)> = Vec::new();
    for (index, (_, found)) in searched.iter().enumerate() {
        if let Found::Rows(rows) = found {
            set.extend(rows.iter().cloned().map(|row| (index, row)));
        }
    }
    let total = set.len();
    let start = offset.min(total);
    let end = start.saturating_add(limit).min(total);
    let page = &set[start..end];
    let truncated = end < total;
    // D-252 §9: the skipped paths inside the page's span, at most 1,000.
    let (from, to) = if page.is_empty() {
        (searched.len(), searched.len())
    } else {
        let from = if start == 0 { 0 } else { set[start - 1].0 + 1 };
        let to = if truncated {
            page[page.len() - 1].0
        } else {
            searched.len()
        };
        (from, to)
    };
    let span: Vec<Value> = searched[from..to]
        .iter()
        .filter(|(_, found)| matches!(found, Found::Binary))
        .map(|(path, _)| json!({ "path": path, "reason": "binary" }))
        .collect();
    let key = match output.as_str() {
        "files" => "files",
        "count" => "counts",
        _ => "matches",
    };
    let engine = json!({
        "total": total,
        "truncated": truncated,
        "nextOffset": if truncated { json!(end) } else { Value::Null },
        "skipped": span.iter().take(1000).cloned().collect::<Vec<_>>(),
        "skippedTotal": span.len(),
        key: page.iter().map(|(_, row)| row.clone()).collect::<Vec<_>>(),
    });
    let engine = serde_json::to_string(&engine).expect("engine body");
    let relayed = format!(
        "{{\"commit\":\"{}\",\"version\":{version},{}",
        sha_of(version),
        &engine[1..]
    );
    ResponseTemplate::new(200)
        .insert_header("Cache-Control", "private, no-cache")
        .set_body_raw(relayed.into_bytes(), "application/json")
}

/// `%XX` escapes decoded, as the server reads a path segment.
fn decoded(path: &str) -> String {
    urlencoding::decode(path)
        .map(|p| p.into_owned())
        .unwrap_or_else(|_| path.to_string())
}

/// One stub deployment, its config, cache and working directories, and
/// a checkout `W` of `alice/notes` the folder rows run inside.
struct Deployment {
    rt: tokio::runtime::Runtime,
    server: MockServer,
    home: TempDir,
    cache: TempDir,
    _work: TempDir,
    w: PathBuf,
    /// The count of requests received before the last `reset_log`.
    mark: std::cell::Cell<usize>,
}

impl Deployment {
    fn new(fixture: &Fixture, search: Search) -> Deployment {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime");
        let server = rt.block_on(MockServer::start());
        let work = tempfile::tempdir().expect("working dir");
        let w = std::fs::canonicalize(work.path()).expect("canonical W");
        std::fs::write(w.join(".syns.yaml"), "owner: alice\nname: notes\n").expect("W identity");
        let d = Deployment {
            rt,
            server,
            home: tempfile::tempdir().expect("config dir"),
            cache: tempfile::tempdir().expect("cache dir"),
            _work: work,
            w,
            mark: std::cell::Cell::new(0),
        };
        d.mount_v1(fixture);
        match search {
            Search::Absent => {}
            Search::Engine => {
                let fixture = fixture.clone();
                d.mount(
                    Mock::given(method("POST"))
                        .and(path_matcher(format!("/api/v2/repos/{REPO}/search")))
                        .respond_with(move |request: &Request| {
                            let body: Value =
                                serde_json::from_slice(&request.body).unwrap_or(Value::Null);
                            let version = body["at"].as_u64().unwrap_or(0) as u32;
                            match fixture.versions.get(&version) {
                                Some(files) => engine_answer(files, version, &body),
                                None => ResponseTemplate::new(404).set_body_json(
                                    json!({"error": "not_found", "message": "Not found"}),
                                ),
                            }
                        }),
                );
            }
            Search::Fixed(status, media, body) => {
                let mut answer =
                    ResponseTemplate::new(status).set_body_raw(body.into_bytes(), media);
                if status == 503 || status == 429 {
                    answer = answer.insert_header("Retry-After", "1");
                }
                d.mount(
                    Mock::given(method("POST"))
                        .and(path_matcher(format!("/api/v2/repos/{REPO}/search")))
                        .respond_with(answer),
                );
            }
        }
        d
    }

    fn mount(&self, mock: Mock) {
        self.rt.block_on(async { mock.mount(&self.server).await });
    }

    /// The v1 reads the version resolution and the fallback make.
    fn mount_v1(&self, fixture: &Fixture) {
        let head = Arc::clone(&fixture.head);
        self.mount(
            Mock::given(method("GET"))
                .and(path_matcher(format!("/api/v1/repos/{REPO}")))
                .respond_with(move |_: &Request| {
                    let sha = sha_of(head.load(Ordering::SeqCst));
                    ResponseTemplate::new(200).set_body_json(json!({
                        "owner": "alice", "name": "notes", "description": null,
                        "commitSha": sha, "status": "active", "author": null, "tags": [],
                        "visibility": "public", "forkedFrom": null, "forkCount": 0,
                        "fileCount": 3, "role": null,
                        "createdAt": "2026-01-01T00:00:00Z", "updatedAt": "2026-01-01T00:00:00Z",
                    }))
                }),
        );
        let held = fixture.clone();
        self.mount(
            Mock::given(method("GET"))
                .and(path_regex(format!("^/api/v1/repos/{REPO}/versions/[^/]+$")))
                .respond_with(move |request: &Request| {
                    let reference = request.url.path().rsplit('/').next().unwrap_or_default();
                    let version = held
                        .versions
                        .keys()
                        .copied()
                        .find(|v| v.to_string() == reference || sha_of(*v) == reference);
                    match version {
                        Some(v) => ResponseTemplate::new(200).set_body_json(json!({
                            "version": v, "sha": sha_of(v), "parentSha": null, "message": "m",
                            "messageBody": null, "author": "alice",
                            "createdAt": "2026-01-01T00:00:00Z", "filesChanged": [],
                        })),
                        None => ResponseTemplate::new(404).set_body_json(
                            json!({"error": "not_found", "message": "Version not found"}),
                        ),
                    }
                }),
        );
        let held = fixture.clone();
        self.mount(
            Mock::given(method("GET"))
                .and(path_regex(format!("^/api/v1/repos/{REPO}/tree(/.*)?$")))
                .respond_with(move |request: &Request| {
                    let version: u32 = request
                        .url
                        .query_pairs()
                        .find(|(k, _)| k == "ref")
                        .and_then(|(_, v)| v.parse().ok())
                        .unwrap_or(0);
                    let Some(files) = held.versions.get(&version) else {
                        return ResponseTemplate::new(404).set_body_json(
                            json!({"error": "not_found", "message": "Version not found"}),
                        );
                    };
                    let tail = request
                        .url
                        .path()
                        .strip_prefix(&format!("/api/v1/repos/{REPO}/tree"))
                        .unwrap_or_default()
                        .trim_start_matches('/');
                    let prefix = decoded(tail);
                    let entries: Vec<Value> = files
                        .iter()
                        .filter(|(p, _)| prefix.is_empty() || p.starts_with(&format!("{prefix}/")))
                        .map(|(p, c)| {
                            json!({
                                "name": p.rsplit('/').next().unwrap_or(p),
                                "path": p, "type": "file", "size": c.len(),
                                "sha": blob_sha1(c.as_bytes()),
                            })
                        })
                        .collect();
                    if entries.is_empty() && !prefix.is_empty() {
                        return ResponseTemplate::new(404).set_body_json(
                            json!({"error": "not_found", "message": "Path not found"}),
                        );
                    }
                    ResponseTemplate::new(200).set_body_json(json!({
                        "entries": entries, "commitSha": sha_of(version), "truncated": false,
                    }))
                }),
        );
        let held = fixture.clone();
        self.mount(
            Mock::given(method("GET"))
                .and(path_regex(format!("^/api/v1/repos/{REPO}/files/.+$")))
                .respond_with(move |request: &Request| {
                    let version: u32 = request
                        .url
                        .query_pairs()
                        .find(|(k, _)| k == "ref")
                        .and_then(|(_, v)| v.parse().ok())
                        .unwrap_or(0);
                    let path = decoded(
                        request
                            .url
                            .path()
                            .strip_prefix(&format!("/api/v1/repos/{REPO}/files/"))
                            .unwrap_or_default(),
                    );
                    match held.versions.get(&version).and_then(|f| f.get(&path)) {
                        Some(content) => ResponseTemplate::new(200).set_body_json(json!({
                            "path": path, "sha": blob_sha1(content.as_bytes()),
                            "content": content, "size": content.len(),
                        })),
                        None => ResponseTemplate::new(404).set_body_json(
                            json!({"error": "not_found", "message": "File not found"}),
                        ),
                    }
                }),
        );
    }

    fn all_received(&self) -> Vec<Request> {
        self.rt
            .block_on(async { self.server.received_requests().await.expect("requests") })
    }

    /// The requests received since the last `reset_log`.
    fn received(&self) -> Vec<Request> {
        self.all_received().split_off(self.mark.get())
    }

    fn reset_log(&self) {
        self.mark.set(self.all_received().len());
    }

    fn searches(&self) -> Vec<Request> {
        self.received()
            .into_iter()
            .filter(|r| r.url.path() == format!("/api/v2/repos/{REPO}/search"))
            .collect()
    }

    /// Every request of the tree, content or batch reads the server's
    /// answer leaves unsent (row 14).
    fn content_reads(&self) -> Vec<String> {
        self.received()
            .into_iter()
            .map(|r| r.url.path().to_string())
            .filter(|p| {
                p.contains("/tree")
                    || p.contains("/files/")
                    || p.contains("/raw/")
                    || p.ends_with("/read")
            })
            .collect()
    }

    fn run(&self, args: &[&str]) -> std::process::Output {
        self.run_in(&self.w, args)
    }

    fn run_in(&self, cwd: &Path, args: &[&str]) -> std::process::Output {
        AssertCommand::cargo_bin("syns")
            .expect("syns binary")
            .current_dir(cwd)
            .env("SYNS_CONFIG_DIR", self.home.path())
            .env("SYNS_CACHE_DIR", self.cache.path())
            .env_remove("SYNS_URL")
            .env_remove("SYNS_CACHE_MAX_BYTES")
            .arg("--server")
            .arg(self.server.uri())
            .args(args)
            .output()
            .expect("run syns")
    }

    /// Every file the cache directory holds, at any depth.
    fn cache_files(&self) -> Vec<PathBuf> {
        fn walk(dir: &Path, into: &mut Vec<PathBuf>) {
            let Ok(entries) = std::fs::read_dir(dir) else {
                return;
            };
            for entry in entries.flatten() {
                let path = entry.path();
                if path.is_dir() {
                    walk(&path, into);
                } else {
                    into.push(path);
                }
            }
        }
        let mut found = Vec::new();
        walk(self.cache.path(), &mut found);
        found
    }
}

fn stdout_of(output: &std::process::Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn stderr_of(output: &std::process::Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

fn document(output: &std::process::Output) -> Value {
    serde_json::from_str(&stdout_of(output)).unwrap_or_else(|e| {
        panic!(
            "stdout is no one document ({e}): {} / {}",
            stdout_of(output),
            stderr_of(output)
        )
    })
}

fn result_key(document: &Value) -> &'static str {
    ["matches", "files", "counts"]
        .into_iter()
        .find(|k| document.get(*k).is_some())
        .expect("a result key")
}

fn rows_of(document: &Value) -> Vec<Value> {
    document[result_key(document)]
        .as_array()
        .cloned()
        .expect("rows")
}

fn args_with<'a>(head: &[&'a str], options: &[&'a str]) -> Vec<&'a str> {
    let mut args = head.to_vec();
    args.extend_from_slice(options);
    args
}

/// The row-15 repository: matches in several folders and in mixed case,
/// context windows that overlap and meet a file's ends, a CRLF file, a
/// file with no final newline, upper case sorting before lower, and one
/// binary file — none of Known issue 5's kinds.
fn mixed_files() -> Files {
    files(&[
        ("README.md", "intro\nfn readme\nFN shout\n"),
        ("Zeta.ts", "fn upper\n"),
        (
            "docs/guide.md",
            "a\nb\nfn one\nc\nfn two\nd\ne\nf\nfn three",
        ),
        ("docs/img.png", "PNG\0\0binary fn"),
        (
            "docs/notes/deep.md",
            "Fn mixed\nplain\nplain\nplain\nfn deep\n",
        ),
        ("src/a.ts", "plain\nfn alpha\ntail\n"),
        ("src/b.ts", "fn b1\r\nplain\r\nfn b2\r\n"),
        ("src/lib/c.ts", "nothing here\n"),
        ("src/lib/d.ts", "x\nfn d\n"),
        ("src/z.md", "fn md\n"),
    ])
}

/// `count` files each holding `per` matching lines, named so their byte
/// order is their index order.
fn many_files(count: usize, per: usize) -> Files {
    (0..count)
        .map(|i| {
            let body: String = (0..per).map(|l| format!("fn {i} {l}\nplain\n")).collect();
            (format!("src/f{i:05}.ts"), body)
        })
        .collect()
}

// ---------------------------------------------------------------- row 14

// Row 14: a run against a server answering the search sends exactly one
// `POST …/search` beside the version resolution, no tree, content or
// batch read, and leaves the content cache untouched; the body carries
// the resolved version, the page limit and no lane.
#[test]
#[serial]
fn r14_one_search_request_and_no_tree_or_content_read() {
    let fixture = Fixture::one(mixed_files());
    let d = Deployment::new(&fixture, Search::Engine);

    for args in [
        vec!["grep", "fn", "--repo", REPO],
        vec!["--json", "grep", "fn", "--repo", REPO, "--output", "files"],
        vec!["grep", "fn", "--repo", REPO, "--version", "7", "-n"],
    ] {
        d.reset_log();
        let out = d.run(&args);
        assert_eq!(out.status.code(), Some(0), "{args:?}: {}", stderr_of(&out));
        let searches = d.searches();
        assert_eq!(searches.len(), 1, "{args:?}: one search request");
        assert!(
            d.content_reads().is_empty(),
            "{args:?}: {:?}",
            d.content_reads()
        );
        let request = &searches[0];
        assert!(request.headers.get("syns-lane").is_none(), "no lane header");
        assert_eq!(
            request
                .headers
                .get("content-type")
                .and_then(|v| v.to_str().ok()),
            Some("application/json")
        );
        let body: Value = serde_json::from_slice(&request.body).expect("a JSON body");
        assert_eq!(body["at"], json!(7));
        assert_eq!(body["limit"], json!(1000));
        assert_eq!(body["offset"], json!(0));
        assert_eq!(body["path"], json!(""));
        assert_eq!(body["glob"], json!([]));
    }
    assert!(d.cache_files().is_empty(), "{:?}", d.cache_files());
}

// ---------------------------------------------------------------- row 15

/// The option sets of row 15: each alone and every pair the option
/// check admits.
fn option_sets() -> Vec<Vec<&'static str>> {
    let options: Vec<Vec<&'static str>> = vec![
        vec!["--path", "src"],
        vec!["--path", "docs"],
        vec!["--glob", "*.ts"],
        vec!["--glob", "docs/*.md"],
        vec!["-i"],
        vec!["-n"],
        vec!["-A", "1"],
        vec!["-B", "2"],
        vec!["-C", "1"],
        vec!["--output", "files"],
        vec!["--output", "count"],
        vec!["--version", "7"],
    ];
    let content_only = |o: &[&str]| matches!(o[0], "-n" | "-A" | "-B" | "-C");
    let mut sets: Vec<Vec<&'static str>> = options.clone();
    for (i, a) in options.iter().enumerate() {
        for b in &options[i + 1..] {
            let output = |o: &[&str]| o[0] == "--output";
            if (content_only(a) && output(b)) || (output(a) && content_only(b)) {
                continue;
            }
            if (a[0] == "-C" && matches!(b[0], "-A" | "-B"))
                || (b[0] == "-C" && matches!(a[0], "-A" | "-B"))
                || (output(a) && output(b))
                || (a[0] == "--path" && b[0] == "--path")
            {
                continue;
            }
            let mut set = a.clone();
            set.extend(b.iter().copied());
            sets.push(set);
        }
    }
    sets
}

// Row 15: every option keeps its meaning — alone and in pairs, the human
// render (stdout and stderr) and the `--json` result rows the server path
// answers equal the fallback's, in the same order.
#[test]
#[serial]
fn r15_every_option_alone_and_in_pairs_answers_as_the_fallback() {
    let fixture = Fixture::one(mixed_files());
    let served = Deployment::new(&fixture, Search::Engine);
    let fallback = Deployment::new(&fixture, Search::Absent);
    let sets = option_sets();
    assert!(sets.len() > 50, "{} sets", sets.len());

    for options in &sets {
        let human = args_with(&["grep", "fn", "--repo", REPO], options);
        let a = served.run(&human);
        let b = fallback.run(&human);
        assert_eq!(a.status.code(), Some(0), "{human:?}: {}", stderr_of(&a));
        assert_eq!(a.status.code(), b.status.code(), "{human:?}");
        assert_eq!(stdout_of(&a), stdout_of(&b), "{human:?}: stdout");
        assert_eq!(stderr_of(&a), stderr_of(&b), "{human:?}: stderr");

        let machine = args_with(&["--json", "grep", "fn", "--repo", REPO], options);
        let a = document(&served.run(&machine));
        let b = document(&fallback.run(&machine));
        assert_eq!(rows_of(&a), rows_of(&b), "{machine:?}: rows");
        for key in [
            "version",
            "commitSha",
            "pattern",
            "output",
            "truncated",
            "nextOffset",
            "limited",
            "skipped",
        ] {
            assert_eq!(a[key], b[key], "{machine:?}: {key}");
        }
        assert!(
            a.get("total").is_some(),
            "{machine:?}: total on the server path"
        );
        assert!(a.get("skippedTotal").is_some(), "{machine:?}");
        assert!(
            b.get("total").is_none(),
            "{machine:?}: no total on the fallback"
        );
    }
    // The fallback deployment answered every run with its own reads.
    assert!(fallback.searches().len() >= sets.len());
    assert!(!fallback.content_reads().is_empty());
}

// Row 15, `--head-limit n` (D-252 §3): the rows are the first `n` of the
// same search without it; the document differs from the fallback's only
// by `truncated: true`, `nextOffset: offset + n`, `total`, and the
// stderr line in its `cut at --head-limit <n>` form.
#[test]
#[serial]
fn r15_head_limit_answers_the_first_n_rows_of_the_unlimited_search() {
    let fixture = Fixture::one(many_files(30, 5));
    let served = Deployment::new(&fixture, Search::Engine);
    let fallback = Deployment::new(&fixture, Search::Absent);

    for output in ["content", "files", "count"] {
        let whole =
            document(&served.run(&["--json", "grep", "fn", "--repo", REPO, "--output", output]));
        let every = rows_of(&whole);
        let total = every.len();
        for n in [1usize, 7, total - 1] {
            let limit = n.to_string();
            let args = [
                "--json",
                "grep",
                "fn",
                "--repo",
                REPO,
                "--output",
                output,
                "--head-limit",
                &limit,
            ];
            let cut = document(&served.run(&args));
            assert_eq!(rows_of(&cut), every[..n].to_vec(), "{output} {n}");
            assert_eq!(cut["truncated"], json!(true), "{output} {n}");
            assert_eq!(cut["nextOffset"], json!(n), "{output} {n}");
            assert_eq!(cut["limited"], json!(true), "{output} {n}");
            assert_eq!(cut["total"], json!(total), "{output} {n}");

            let old = document(&fallback.run(&args));
            assert_eq!(old["limited"], json!(true), "{output} {n}");
            assert_eq!(old["nextOffset"], Value::Null, "{output} {n}");
            assert!(old.get("total").is_none());

            let human = served.run(&args[1..]);
            let line = stderr_of(&human).lines().nth(1).map(str::to_string);
            let noun = if output == "content" {
                "matches"
            } else {
                "files"
            };
            assert_eq!(
                line,
                Some(format!(
                    "this call took {noun} 1 to {n} of the {total}, in path order (cut at --head-limit {n}); {} left, continue with --offset {n} --version 7",
                    total - n
                )),
                "{output} {n}"
            );
            let old_human = fallback.run(&args[1..]);
            assert_eq!(
                stderr_of(&old_human).lines().count(),
                1,
                "no line on the fallback"
            );
        }
        // A limit the whole set meets, or one past it, cuts nothing.
        for n in [total, total + 1] {
            let limit = n.to_string();
            let whole_again = document(&served.run(&[
                "--json",
                "grep",
                "fn",
                "--repo",
                REPO,
                "--output",
                output,
                "--head-limit",
                &limit,
            ]));
            assert_eq!(rows_of(&whole_again), every, "{output} {n}");
            assert_eq!(whole_again["truncated"], json!(false));
            assert_eq!(whole_again["nextOffset"], Value::Null);
            assert_eq!(whole_again["limited"], json!(false));
        }
    }
    // The limit goes as the page's `limit`, at most 1000.
    let limits: Vec<Value> = served
        .searches()
        .iter()
        .map(|r| serde_json::from_slice::<Value>(&r.body).unwrap()["limit"].clone())
        .collect();
    assert!(limits.contains(&json!(7)));
    let _ = served.run(&[
        "--json",
        "grep",
        "fn",
        "--repo",
        REPO,
        "--head-limit",
        "5000",
    ]);
    let last = served.searches().pop().expect("a search");
    assert_eq!(
        serde_json::from_slice::<Value>(&last.body).unwrap()["limit"],
        json!(1000)
    );
}

/// Every row of a search, its pages followed from offset 0.
fn unpaged(d: &Deployment, options: &[&str]) -> Vec<Value> {
    let mut rows = Vec::new();
    let mut offset = 0u64;
    loop {
        let at = offset.to_string();
        let mut args = vec!["--json", "grep", "fn", "--repo", REPO, "--offset", &at];
        args.extend_from_slice(options);
        let page = document(&d.run(&args));
        rows.extend(rows_of(&page));
        match page["nextOffset"].as_u64() {
            Some(next) => offset = next,
            None => return rows,
        }
    }
}

// Row 15, `--offset k` (D-251): the rows are rows `k…` of the same
// search's unpaged set, for k ∈ {0, 1, 999, 1000, total − 1, total,
// total + 5}.
#[test]
#[serial]
fn r15_offset_answers_rows_from_k_of_the_unpaged_set() {
    let fixture = Fixture::one(many_files(401, 3));
    let d = Deployment::new(&fixture, Search::Engine);
    let every = unpaged(&d, &["--version", "7"]);
    let total = every.len();
    assert_eq!(total, 1203);

    for k in [0, 1, 999, 1000, total - 1, total, total + 5] {
        let at = k.to_string();
        let page = document(&d.run(&[
            "--json",
            "grep",
            "fn",
            "--repo",
            REPO,
            "--offset",
            &at,
            "--version",
            "7",
        ]));
        let from = k.min(total);
        let to = (k + 1000).min(total);
        assert_eq!(rows_of(&page), every[from..to].to_vec(), "k = {k}");
        assert_eq!(page["total"], json!(total), "k = {k}");
        let next = (to < total).then_some(to);
        assert_eq!(page["nextOffset"], json!(next), "k = {k}");
        assert_eq!(page["truncated"], json!(to < total), "k = {k}");
        assert_eq!(page["limited"], json!(false), "k = {k}");
    }
}

// ---------------------------------------------------------------- row 16

// Row 16: every pre-wire refusal is byte-identical to the base server's
// run and sends no request at all, the search route standing.
#[test]
#[serial]
fn r16_pre_wire_refusals_are_unchanged_and_send_nothing() {
    let fixture = Fixture::one(mixed_files());
    let served = Deployment::new(&fixture, Search::Engine);
    let fallback = Deployment::new(&fixture, Search::Absent);
    let mut runs: Vec<Vec<&str>> = Vec::new();
    for mode in [&[][..], &["--json"][..]] {
        for refused in [
            vec!["grep", "fn", "--repo", REPO, "-n", "--output", "files"],
            vec!["grep", "fn", "--repo", REPO, "-A", "1", "-C", "1"],
            vec!["grep", "fn", "--repo", REPO, "--head-limit", "0"],
            vec!["grep", "(", "--repo", REPO],
            vec!["grep", "a\\nb", "--repo", REPO],
            vec![
                "grep", "fn", "--repo", REPO, "--glob", "*.ts", "--glob", "[z",
            ],
            vec!["grep", "fn", "--repo", REPO, "--path", "../x"],
            vec!["grep", "fn", "--repo", REPO, "--version", ""],
            vec!["grep", "fn", "--repo", REPO, "--version", "0"],
        ] {
            runs.push(args_with(mode, &refused));
        }
    }
    for args in &runs {
        let a = served.run(args);
        let b = fallback.run(args);
        assert_ne!(a.status.code(), Some(0), "{args:?}");
        assert_eq!(a.status.code(), b.status.code(), "{args:?}");
        assert_eq!(stdout_of(&a), stdout_of(&b), "{args:?}");
        assert_eq!(stderr_of(&a), stderr_of(&b), "{args:?}");
    }
    // Identity unknown: a directory holding no `.syns.yaml`.
    let bare = tempfile::tempdir().expect("bare dir");
    let a = served.run_in(bare.path(), &["grep", "fn"]);
    let b = fallback.run_in(bare.path(), &["grep", "fn"]);
    assert_eq!(a.status.code(), Some(2), "{}", stderr_of(&a));
    assert_eq!(stderr_of(&a), stderr_of(&b));
    assert!(served.received().is_empty(), "{:?}", served.received());
}

// Row 16, inside a scoped folder: `--path` leaving it, and a folder out
// of place, are refused unchanged before any request.
#[test]
#[serial]
fn r16_folder_refusals_are_unchanged_and_send_nothing() {
    let fixture = Fixture::one(mixed_files());
    let served = Deployment::new(&fixture, Search::Engine);
    let fallback = Deployment::new(&fixture, Search::Absent);
    for d in [&served, &fallback] {
        let folder = d.w.join("docs");
        std::fs::create_dir_all(&folder).unwrap();
        std::fs::write(
            folder.join(".syns.yaml"),
            "holder: alice/notes\npath: docs\n",
        )
        .unwrap();
        let moved = d.w.join("elsewhere/notes");
        std::fs::create_dir_all(&moved).unwrap();
        std::fs::write(
            moved.join(".syns.yaml"),
            "holder: alice/notes\npath: docs/notes\n",
        )
        .unwrap();
    }
    for (place, args) in [
        ("docs", vec!["grep", "fn", "--path", "../src"]),
        ("docs", vec!["grep", "fn", "--path", "/x"]),
        ("elsewhere/notes", vec!["grep", "fn"]),
    ] {
        let a = served.run_in(&served.w.join(place), &args);
        let b = fallback.run_in(&fallback.w.join(place), &args);
        assert_ne!(a.status.code(), Some(0), "{args:?}");
        assert_eq!(a.status.code(), b.status.code(), "{args:?}");
        // The lines name each deployment's own `W`.
        let ours = stderr_of(&b).replace(fallback.w.to_str().unwrap(), served.w.to_str().unwrap());
        assert_eq!(stderr_of(&a), ours, "{args:?}");
    }
    assert!(served.received().is_empty(), "{:?}", served.received());
}

// ---------------------------------------------------------------- row 17

// Row 17: a page the server cut writes the continuation line's first
// form exactly, a whole answer writes none, and following `--offset <b>
// --version <v>` until no line prints yields every row once, a push
// landing between pages changing nothing.
#[test]
#[serial]
fn r17_following_the_line_walks_every_row_once_across_a_push() {
    let before = many_files(401, 3);
    let mut after = before.clone();
    after.insert("src/00000-new.ts".into(), "fn pushed\n".into());
    after.remove("src/f00200.ts");
    let fixture = Fixture::at(vec![(7, before), (8, after)], 7);
    let d = Deployment::new(&fixture, Search::Engine);
    let every = unpaged(&d, &["--version", "7"]);
    assert_eq!(every.len(), 1203);

    let mut walked: Vec<Value> = Vec::new();
    let mut args: Vec<String> = ["grep", "fn", "--repo", REPO, "-n"]
        .iter()
        .map(|s| s.to_string())
        .collect();
    let mut lines = Vec::new();
    loop {
        let refs: Vec<&str> = args.iter().map(String::as_str).collect();
        let out = d.run(&refs);
        assert_eq!(out.status.code(), Some(0), "{}", stderr_of(&out));
        let stderr = stderr_of(&out);
        let mut stderr_lines = stderr.lines();
        let reference = stderr_lines.next().expect("the reference line");
        assert_eq!(
            reference,
            format!("read at version 7, commit {}", sha_of(7))
        );
        let mut json_args = vec!["--json"];
        json_args.extend(refs.iter().copied());
        walked.extend(rows_of(&document(&d.run(&json_args))));
        // The push lands after the first page.
        fixture.head.store(8, Ordering::SeqCst);
        match stderr_lines.next() {
            None => break,
            Some(line) => {
                lines.push(line.to_string());
                let next = line
                    .split("--offset ")
                    .nth(1)
                    .and_then(|rest| rest.split(' ').next())
                    .expect("an offset");
                args = [
                    "grep",
                    "fn",
                    "--repo",
                    REPO,
                    "-n",
                    "--offset",
                    next,
                    "--version",
                    "7",
                ]
                .iter()
                .map(|s| s.to_string())
                .collect();
            }
        }
    }
    assert_eq!(
        lines,
        vec![
            "this call took matches 1 to 1000 of the 1203, in path order (at most 1000 a call); 203 left, continue with --offset 1000 --version 7"
                .to_string()
        ]
    );
    assert_eq!(walked, every, "every row once, in order");
    // The head did move: a run naming no version reads version 8.
    let moved = document(&d.run(&["--json", "grep", "fn", "--repo", REPO]));
    assert_eq!(moved["version"], json!(8));
    assert_ne!(rows_of(&moved)[0], every[0]);

    // A whole answer writes the reference line alone.
    let whole = d.run(&["grep", "fn 3 ", "--repo", REPO, "--version", "7"]);
    assert_eq!(
        stderr_of(&whole).lines().count(),
        1,
        "{}",
        stderr_of(&whole)
    );
    assert!(!stdout_of(&whole).is_empty());
}

// Row 17, the files noun: a cut `--output files` page names files.
#[test]
#[serial]
fn r17_a_cut_files_page_names_files() {
    let fixture = Fixture::one(many_files(1203, 1));
    let d = Deployment::new(&fixture, Search::Engine);
    for output in ["files", "count"] {
        let out = d.run(&["grep", "fn", "--repo", REPO, "--output", output]);
        assert_eq!(
            stderr_of(&out).lines().nth(1),
            Some(
                "this call took files 1 to 1000 of the 1203, in path order (at most 1000 a call); 203 left, continue with --offset 1000 --version 7"
            ),
            "{output}"
        );
        assert_eq!(stdout_of(&out).lines().count(), 1000);
    }
}

// ---------------------------------------------------------------- row 18

// Row 18: `--json` on a cut page carries `truncated`, `total`,
// `nextOffset` and `limited: false`; `--head-limit 50` over 3,412 rows
// gives 50 rows, `limited`, `truncated` and `nextOffset: 50`; a whole
// answer gives `truncated: false`, `nextOffset: null`, `limited: false`.
#[test]
#[serial]
fn r18_the_json_document_on_cut_limited_and_whole_pages() {
    let fixture = Fixture::one(many_files(853, 4));
    let d = Deployment::new(&fixture, Search::Engine);

    let cut = document(&d.run(&["--json", "grep", "fn", "--repo", REPO, "-n"]));
    assert_eq!(rows_of(&cut).len(), 1000);
    assert_eq!(cut["truncated"], json!(true));
    assert_eq!(cut["total"], json!(3412));
    assert_eq!(cut["nextOffset"], json!(1000));
    assert_eq!(cut["limited"], json!(false));
    assert_eq!(cut["skipped"], json!([]));
    assert_eq!(cut["skippedTotal"], json!(0));
    assert_eq!(cut["version"], json!(7));
    assert_eq!(cut["commitSha"], json!(sha_of(7)));
    assert_eq!(cut["pattern"], json!("fn"));
    assert_eq!(cut["output"], json!("content"));
    assert_eq!(
        rows_of(&cut)[0],
        json!({"path": "src/f00000.ts", "line": 1, "text": "fn 0 0", "context": []})
    );

    let limited = document(&d.run(&["--json", "grep", "fn", "--repo", REPO, "--head-limit", "50"]));
    assert_eq!(rows_of(&limited).len(), 50);
    assert_eq!(limited["limited"], json!(true));
    assert_eq!(limited["truncated"], json!(true));
    assert_eq!(limited["nextOffset"], json!(50));
    assert_eq!(limited["total"], json!(3412));

    let whole = document(&d.run(&["--json", "grep", "fn 852 ", "--repo", REPO]));
    assert_eq!(rows_of(&whole).len(), 4);
    assert_eq!(whole["truncated"], json!(false));
    assert_eq!(whole["nextOffset"], Value::Null);
    assert_eq!(whole["limited"], json!(false));
    assert_eq!(whole["total"], json!(4));
}

// Row 18 (D-252 §9): a page lists the skipped paths of its own span and
// `skippedTotal` counts them; the paths count from where the rows do.
#[test]
#[serial]
fn r18_skipped_and_skipped_total_are_the_servers() {
    let mut set = many_files(3, 1);
    set.insert("src/a.bin".into(), "\0\0".into());
    set.insert("src/zz.bin".into(), "x\0".into());
    let fixture = Fixture::one(set);
    let d = Deployment::new(&fixture, Search::Engine);
    let page = document(&d.run(&["--json", "grep", "fn", "--repo", REPO]));
    assert_eq!(
        page["skipped"],
        json!([
            {"path": "src/a.bin", "reason": "binary"},
            {"path": "src/zz.bin", "reason": "binary"},
        ])
    );
    assert_eq!(page["skippedTotal"], json!(2));
}

// Row 18: `503 engine_busy` from the server is refused `ENGINE_BUSY`,
// exit 1, `code: "engine_busy"`, sent once, with no fallback.
#[test]
#[serial]
fn r18_engine_busy_is_refused_with_no_fallback_and_no_resend() {
    let fixture = Fixture::one(mixed_files());
    let d = Deployment::new(
        &fixture,
        Search::Fixed(
            503,
            "application/json",
            json!({"error": "engine_busy", "message": "The engine is busy"}).to_string(),
        ),
    );
    for args in [
        vec!["grep", "fn", "--repo", REPO],
        vec!["--json", "grep", "fn", "--repo", REPO],
    ] {
        d.reset_log();
        let out = d.run(&args);
        assert_eq!(out.status.code(), Some(1), "{args:?}: {}", stderr_of(&out));
        assert_eq!(d.searches().len(), 1, "{args:?}: sent once");
        assert!(d.content_reads().is_empty(), "{args:?}: no fallback");
        assert!(stdout_of(&out).is_empty() || args[0] == "--json");
        if args[0] == "--json" {
            let refusal = document(&out);
            assert_eq!(refusal["code"], json!("engine_busy"), "{refusal}");
        } else {
            assert!(
                stderr_of(&out).contains("engine_busy"),
                "{}",
                stderr_of(&out)
            );
        }
    }
}

// ---------------------------------------------------------------- row 19

/// A deployment whose `W` holds the scoped folder `folder` of
/// `alice/notes`.
fn with_folder(fixture: &Fixture, search: Search, folder: &str) -> Deployment {
    let d = Deployment::new(fixture, search);
    let dir = d.w.join(folder);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join(".syns.yaml"),
        format!("holder: alice/notes\npath: '{folder}'\n"),
    )
    .unwrap();
    d
}

fn folder_files(folder: &str) -> Files {
    let mut set = files(&[("top.md", "fn outside\n"), ("other/x.md", "fn other\n")]);
    for (path, content) in [
        ("a.md", "fn a\n"),
        ("notes/b.md", "fn b\nplain\n"),
        ("notes/deep/c.md", "fn c\n"),
        ("src/d.ts", "fn d\n"),
    ] {
        set.insert(format!("{folder}/{path}"), content.to_string());
    }
    set
}

fn assert_folder_rows(folder: &str) {
    let fixture = Fixture::one(folder_files(folder));
    let served = with_folder(&fixture, Search::Engine, folder);
    let fallback = with_folder(&fixture, Search::Absent, folder);
    let inside_served = served.w.join(folder);
    let inside_fallback = fallback.w.join(folder);

    let all = document(&served.run_in(&inside_served, &["--json", "grep", "fn"]));
    let paths: Vec<Value> = rows_of(&all).iter().map(|r| r["path"].clone()).collect();
    assert_eq!(
        paths,
        vec![
            json!("a.md"),
            json!("notes/b.md"),
            json!("notes/deep/c.md"),
            json!("src/d.ts")
        ],
        "{folder}"
    );
    let anchored = document(&served.run_in(
        &inside_served,
        &[
            "--json",
            "grep",
            "fn",
            "--glob",
            "notes/*.md",
            "--output",
            "files",
        ],
    ));
    assert_eq!(anchored["files"], json!(["notes/b.md"]), "{folder}");
    let search = served.searches().pop().expect("a search");
    let body: Value = serde_json::from_slice(&search.body).unwrap();
    assert_eq!(body["path"], json!(folder));
    assert_eq!(
        body["glob"],
        json!([format!("{}/notes/*.md", globset::escape(folder))])
    );

    for args in [
        vec!["grep", "fn", "-n"],
        vec!["grep", "fn", "--glob", "notes/*.md"],
        vec!["grep", "fn", "--glob", "*.ts"],
        vec!["grep", "fn", "--glob", "**/c.md"],
        vec!["grep", "fn", "--path", "notes", "--output", "count"],
    ] {
        let a = served.run_in(&inside_served, &args);
        let b = fallback.run_in(&inside_fallback, &args);
        assert_eq!(
            a.status.code(),
            Some(0),
            "{folder} {args:?}: {}",
            stderr_of(&a)
        );
        assert_eq!(stdout_of(&a), stdout_of(&b), "{folder} {args:?}");
        assert_eq!(stderr_of(&a), stderr_of(&b), "{folder} {args:?}");
    }
    // From a directory under the folder, the glob still anchors at it.
    let sub = inside_served.join("notes");
    std::fs::create_dir_all(&sub).unwrap();
    let from_sub = document(&served.run_in(
        &sub,
        &[
            "--json",
            "grep",
            "fn",
            "--glob",
            "notes/*.md",
            "--output",
            "files",
        ],
    ));
    assert_eq!(from_sub["files"], json!(["notes/b.md"]), "{folder}");
}

// Row 19: inside a scoped folder rows count from it, and a `--glob`
// holding `/` anchors at it, a folder whose name holds `[` or `{`
// included.
#[test]
#[serial]
fn r19_inside_a_scoped_folder_rows_and_globs_count_from_it() {
    assert_folder_rows("clients/vela/q3-board");
    assert_folder_rows("docs/q[3]{x}");
}

// Row 19, a name holding `*` (a name no Windows directory can take).
#[cfg(unix)]
#[test]
#[serial]
fn r19_a_folder_whose_name_holds_a_star() {
    assert_folder_rows("docs/a*b");
}

// ---------------------------------------------------------------- row 20

/// The runs row 20 compares: D-156's window and continuation line, the
/// modes, a refusal, and no `total`.
fn fallback_runs() -> Vec<Vec<&'static str>> {
    let mut runs = Vec::new();
    for mode in [&[][..], &["--json"][..]] {
        for args in [
            vec!["grep", "fn", "--repo", REPO],
            vec!["grep", "fn", "--repo", REPO, "--output", "files"],
            vec![
                "grep", "fn", "--repo", REPO, "--output", "count", "--offset", "400",
            ],
            vec![
                "grep",
                "fn",
                "--repo",
                REPO,
                "-n",
                "-C",
                "1",
                "--head-limit",
                "3",
            ],
            vec!["grep", "fn", "--repo", REPO, "--path", "nowhere"],
            vec![
                "grep",
                "fn",
                "--repo",
                REPO,
                "--path",
                "nowhere",
                "--version",
                "7",
            ],
            vec!["grep", "fn", "--repo", REPO, "--version", "99"],
        ] {
            runs.push(args_with(mode, &args));
        }
    }
    runs
}

// Row 20: against a stub answering the search route `404` with an HTML
// body, `405`, or `200 text/html`, the run's stdout, stderr and `--json`
// are byte-identical to the base server's — D-156's window, continuation
// line and refusals, and no `total`.
#[test]
#[serial]
fn r20_a_server_without_the_search_gets_todays_run_byte_for_byte() {
    let mut set = many_files(450, 1);
    set.insert("src/blob.bin".into(), "\0".into());
    let fixture = Fixture::one(set);
    let base = Deployment::new(&fixture, Search::Absent);
    let html = "<!doctype html><html><body>Not Found</body></html>".to_string();
    let variants = [
        Search::Fixed(404, "text/html", html.clone()),
        Search::Fixed(
            404,
            "application/json",
            json!({"error": "not_found", "message": "Not found"}).to_string(),
        ),
        Search::Fixed(405, "text/plain", "Method Not Allowed".to_string()),
        Search::Fixed(200, "text/html", html),
    ];
    let expected: Vec<std::process::Output> =
        fallback_runs().iter().map(|args| base.run(args)).collect();
    // D-156's window and line stand in the base server's run.
    assert!(
        stderr_of(&expected[0]).contains(
            "this call took files 1 to 400 of the 451 kept, in path order (at most 400 a call); continue with --offset 400 --version 7"
        ),
        "{}",
        stderr_of(&expected[0])
    );
    assert!(document(&expected[7]).get("total").is_none());

    for variant in variants {
        let d = Deployment::new(&fixture, variant);
        for (args, want) in fallback_runs().iter().zip(&expected) {
            let got = d.run(args);
            assert_eq!(got.status.code(), want.status.code(), "{args:?}");
            assert_eq!(stdout_of(&got), stdout_of(want), "{args:?}");
            assert_eq!(stderr_of(&got), stderr_of(want), "{args:?}");
        }
        assert!(!d.searches().is_empty(), "the search was asked first");
    }
}

// ---------------------------------------------------------------- row 21

// Row 21: `500`, `502`, `503`, `413`, `422` and `429` from the search
// route are refused and never fall back: no tree read follows.
#[test]
#[serial]
fn r21_server_refusals_are_refused_and_never_fall_back() {
    let fixture = Fixture::one(mixed_files());
    for (status, body, code) in [
        (
            500,
            json!({"error": "internal_error", "message": "Internal error"}),
            "internal_error",
        ),
        (502, json!({"error": "bad_gateway"}), "bad_gateway"),
        (503, json!({"error": "engine_busy"}), "engine_busy"),
        (
            413,
            json!({"error": "payload_too_large"}),
            "payload_too_large",
        ),
        (
            422,
            json!({"error": "validation_error", "message": "invalid pattern x: y"}),
            "validation_error",
        ),
        (
            429,
            json!({"error": "rate_limited", "message": "Too many requests"}),
            "rate_limited",
        ),
        // REVIEW-syns-cli finding 3: a `401` is `AUTH_REQUIRED`, no fallback.
        (
            401,
            json!({"error": "unauthorized", "message": "Sign in"}),
            "auth_required",
        ),
    ] {
        let d = Deployment::new(
            &fixture,
            Search::Fixed(status, "application/json", body.to_string()),
        );
        let out = d.run(&["--json", "grep", "fn", "--repo", REPO]);
        assert_eq!(out.status.code(), Some(1), "{status}: {}", stderr_of(&out));
        assert_eq!(document(&out)["code"], json!(code), "{status}");
        let human = d.run(&["grep", "fn", "--repo", REPO]);
        assert_eq!(human.status.code(), Some(1), "{status}");
        assert!(stdout_of(&human).is_empty(), "{status}");
        assert!(
            d.content_reads().is_empty(),
            "{status}: {:?}",
            d.content_reads()
        );
        assert_eq!(d.searches().len(), 2, "{status}: each run sent one search");
    }
}

// Row 21: a `200` in JSON that is no search answer is refused
// `RESPONSE_UNDECODABLE`, never a fallback.
#[test]
#[serial]
fn r21_a_json_200_that_is_no_search_answer_is_undecodable() {
    let fixture = Fixture::one(mixed_files());
    let other_key = "{\"commit\":\"x\",\"version\":7,\"total\":0,\"truncated\":false,\"nextOffset\":null,\"skipped\":[],\"skippedTotal\":0,\"files\":[]}";
    // REVIEW-syns-cli finding 1: `truncated` and `nextOffset` disagree,
    // the key absent among them.
    let no_next =
        "{\"total\":2,\"truncated\":true,\"skipped\":[],\"skippedTotal\":0,\"matches\":[]}";
    let cut_without_next = "{\"total\":2,\"truncated\":true,\"nextOffset\":null,\"skipped\":[],\"skippedTotal\":0,\"matches\":[]}";
    let whole_with_next = "{\"total\":2,\"truncated\":false,\"nextOffset\":1,\"skipped\":[],\"skippedTotal\":0,\"matches\":[]}";
    for body in [
        no_next,
        cut_without_next,
        whole_with_next,
        "{\"commit\":\"x\",\"version\":7}",
        "[]",
        "{\"total\":1,",
        other_key,
    ] {
        let d = Deployment::new(
            &fixture,
            Search::Fixed(200, "application/json", body.to_string()),
        );
        let out = d.run(&["--json", "grep", "fn", "--repo", REPO]);
        assert_eq!(out.status.code(), Some(1), "{body}: {}", stderr_of(&out));
        assert_eq!(
            document(&out)["code"],
            json!("response_undecodable"),
            "{body}"
        );
        assert!(d.content_reads().is_empty(), "{body}");
    }
}

// ---------------------------------------------------------------- row 21a

/// How the raw stub answers the search.
#[derive(Clone, Copy)]
enum Cut {
    /// `Content-Length` declared whole, the connection closed after 4 KiB.
    Declared,
    /// No length, `Connection: close`, the stream ended after 4 KiB.
    Unframed,
    /// Chunked, the connection closed inside a chunk after 4 KiB.
    Chunked,
    /// The connection closed with no answer at all.
    Unanswered,
}

/// A raw HTTP/1.1 stub answering the version read of `--version 7`, and
/// the search route as `cut` says, until the test ends; any other
/// request is a bare `404`. Its port is the OS's, so parallel runs never
/// meet.
fn raw_stub(cut: Cut) -> (String, Arc<AtomicU32>) {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
    let address = format!("http://{}", listener.local_addr().unwrap());
    let other = Arc::new(AtomicU32::new(0));
    let counted = Arc::clone(&other);
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { return };
            let counted = Arc::clone(&counted);
            std::thread::spawn(move || {
                let mut head = Vec::new();
                let mut byte = [0u8; 1];
                while !head.ends_with(b"\r\n\r\n") {
                    if stream.read(&mut byte).unwrap_or(0) == 0 {
                        return;
                    }
                    head.push(byte[0]);
                }
                let head = String::from_utf8_lossy(&head).to_string();
                let length: usize = head
                    .lines()
                    .find_map(|l| {
                        let (k, v) = l.split_once(':')?;
                        k.eq_ignore_ascii_case("content-length")
                            .then(|| v.trim().parse().ok())?
                    })
                    .unwrap_or(0);
                let mut body = vec![0u8; length];
                let _ = stream.read_exact(&mut body);
                let target = head.split(' ').nth(1).unwrap_or_default().to_string();
                if target == format!("/api/v1/repos/{REPO}/versions/7") {
                    let body = json!({
                        "version": 7, "sha": sha_of(7), "parentSha": null, "message": "m",
                        "messageBody": null, "author": "alice",
                        "createdAt": "2026-01-01T00:00:00Z", "filesChanged": [],
                    })
                    .to_string();
                    let _ = write!(
                        stream,
                        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                        body.len()
                    );
                    return;
                }
                if target != format!("/api/v2/repos/{REPO}/search") {
                    counted.fetch_add(1, Ordering::SeqCst);
                    let _ = write!(
                        stream,
                        "HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                    );
                    return;
                }
                let row =
                    json!({"path": "src/a.ts", "line": 1, "text": "fn ".repeat(30), "context": []});
                let mut whole = format!(
                    "{{\"commit\":\"{}\",\"version\":7,\"total\":500,\"truncated\":false,\"nextOffset\":null,\"skipped\":[],\"skippedTotal\":0,\"matches\":[",
                    sha_of(7)
                );
                for i in 0..500 {
                    if i > 0 {
                        whole.push(',');
                    }
                    whole.push_str(&row.to_string());
                }
                whole.push_str("]}");
                let sent = &whole.as_bytes()[..4096];
                match cut {
                    Cut::Declared => {
                        let _ = write!(
                            stream,
                            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n",
                            whole.len()
                        );
                        let _ = stream.write_all(sent);
                    }
                    Cut::Unframed => {
                        let _ = write!(
                            stream,
                            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nConnection: close\r\n\r\n"
                        );
                        let _ = stream.write_all(sent);
                    }
                    Cut::Chunked => {
                        let _ = write!(
                            stream,
                            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nTransfer-Encoding: chunked\r\n\r\n{:x}\r\n",
                            whole.len()
                        );
                        let _ = stream.write_all(sent);
                    }
                    Cut::Unanswered => {}
                }
                let _ = stream.flush();
                let _ = stream.shutdown(std::net::Shutdown::Both);
            });
        }
    });
    (address, other)
}

fn run_against(server: &str, args: &[&str]) -> std::process::Output {
    let home = tempfile::tempdir().expect("config dir");
    let cache = tempfile::tempdir().expect("cache dir");
    let work = tempfile::tempdir().expect("work dir");
    AssertCommand::cargo_bin("syns")
        .expect("syns binary")
        .current_dir(work.path())
        .env("SYNS_CONFIG_DIR", home.path())
        .env("SYNS_CACHE_DIR", cache.path())
        .env_remove("SYNS_URL")
        .arg("--server")
        .arg(server)
        .args(args)
        .output()
        .expect("run syns")
}

// Row 21a (D-252 §8): a `200` whose JSON is cut mid-body — the stub
// closing the connection after 4 KiB, framed by a length, by the
// connection's end or by chunks — is `RESPONSE_UNDECODABLE`, exit 1,
// `--json` `code: "response_undecodable"`, no rows on stdout, and no
// fallback.
#[test]
fn r21a_a_body_cut_mid_way_is_undecodable_with_no_fallback() {
    for cut in [Cut::Declared, Cut::Unframed, Cut::Chunked] {
        let (server, other) = raw_stub(cut);
        let human = run_against(&server, &["grep", "fn", "--repo", REPO, "--version", "7"]);
        assert_eq!(human.status.code(), Some(1), "{}", stderr_of(&human));
        assert!(
            stdout_of(&human).is_empty(),
            "no rows: {}",
            stdout_of(&human)
        );
        assert!(
            stderr_of(&human).contains("invalid response body"),
            "{}",
            stderr_of(&human)
        );
        let machine = run_against(
            &server,
            &["--json", "grep", "fn", "--repo", REPO, "--version", "7"],
        );
        assert_eq!(machine.status.code(), Some(1));
        let refusal = document(&machine);
        assert_eq!(refusal["code"], json!("response_undecodable"), "{refusal}");
        assert!(refusal.get("matches").is_none());
        assert_eq!(other.load(Ordering::SeqCst), 0, "no fallback read");
    }
}

// FEATURE_SPEC § `syns grep`: a send that got no answer is
// `SERVER_UNREACHABLE`, never a fallback.
#[test]
fn a_search_that_got_no_answer_is_the_server_unreachable() {
    let (server, other) = raw_stub(Cut::Unanswered);
    let out = run_against(
        &server,
        &["--json", "grep", "fn", "--repo", REPO, "--version", "7"],
    );
    assert_eq!(out.status.code(), Some(3), "{}", stderr_of(&out));
    assert_eq!(document(&out)["code"], json!("server_unreachable"));
    assert_eq!(other.load(Ordering::SeqCst), 0, "no fallback read");
}

// ---------------------------------------------------------------- row 22

// Row 22: a repository the reader cannot see, an unknown `--version`
// and a `--path` standing nowhere each print today's refusal line and
// exit code, byte for byte, on a server with the search too.
#[test]
#[serial]
fn r22_todays_refusals_stand_on_a_server_with_the_search() {
    let fixture = Fixture::one(mixed_files());
    let served = Deployment::new(&fixture, Search::Engine);
    let fallback = Deployment::new(&fixture, Search::Absent);
    for mode in [&[][..], &["--json"][..]] {
        for args in [
            vec!["grep", "fn", "--repo", "alice/hidden"],
            vec!["grep", "fn", "--repo", "alice/hidden", "--version", "7"],
            vec!["grep", "fn", "--repo", REPO, "--version", "99"],
            vec!["grep", "fn", "--repo", REPO, "--path", "nowhere"],
            vec![
                "grep",
                "fn",
                "--repo",
                REPO,
                "--path",
                "nowhere",
                "--version",
                "7",
            ],
        ] {
            let args = args_with(mode, &args);
            served.reset_log();
            let a = served.run(&args);
            // REVIEW-syns-cli finding 3: `--path nowhere` takes the route
            // search → `404` → fallback → today's refusal; the hidden
            // repository and `--version 99` end at resolution, before any
            // search.
            let reaches_search = args.contains(&"nowhere");
            assert_eq!(
                served.searches().len(),
                usize::from(reaches_search),
                "{args:?}: searches sent"
            );
            let trees = served
                .received()
                .iter()
                .filter(|r| {
                    r.url
                        .path()
                        .starts_with(&format!("/api/v1/repos/{REPO}/tree"))
                })
                .count();
            assert_eq!(trees, usize::from(reaches_search), "{args:?}: tree reads");
            let b = fallback.run(&args);
            assert_ne!(a.status.code(), Some(0), "{args:?}");
            assert_eq!(a.status.code(), b.status.code(), "{args:?}");
            assert_eq!(stdout_of(&a), stdout_of(&b), "{args:?}");
            assert_eq!(stderr_of(&a), stderr_of(&b), "{args:?}");
        }
    }
    let lines = stderr_of(&served.run(&[
        "grep",
        "fn",
        "--repo",
        REPO,
        "--path",
        "nowhere",
        "--version",
        "7",
    ]));
    assert_eq!(
        lines.trim_end(),
        "error: path not found at version 7: nowhere"
    );
}

// ---------------------------------------------------------------- fix round 2

// D-252 §12: a row the server trimmed to fit the page carries `cut: true`
// into the `--json` rows, every whole row carrying no such key; outside
// `--json` one stderr line after the reference and continuation lines
// counts the cut rows.
#[test]
#[serial]
fn a_row_cut_to_fit_the_page_is_marked_and_counted() {
    let fixture = Fixture::one(mixed_files());
    let page = |truncated: bool| {
        json!({
            "commit": sha_of(7), "version": 7,
            "total": if truncated { 3 } else { 2 },
            "truncated": truncated,
            "nextOffset": if truncated { json!(2) } else { Value::Null },
            "skipped": [], "skippedTotal": 0,
            "matches": [
                {"path": "big.txt", "line": 1, "text": "fn trimmed", "context": [], "cut": true},
                {"path": "small.txt", "line": 4, "text": "fn whole", "context": [{"line": 3, "text": "a"}]},
            ],
        })
        .to_string()
    };
    let d = Deployment::new(
        &fixture,
        Search::Fixed(200, "application/json", page(false)),
    );

    let machine = d.run(&["--json", "grep", "fn", "--repo", REPO, "-B", "1"]);
    assert_eq!(machine.status.code(), Some(0), "{}", stderr_of(&machine));
    let rows = rows_of(&document(&machine));
    assert_eq!(rows[0]["cut"], json!(true));
    assert!(
        rows[1].get("cut").is_none(),
        "a whole row carries no cut: {}",
        rows[1]
    );
    assert_eq!(rows[1]["context"], json!([{"line": 3, "text": "a"}]));

    let human = d.run(&["grep", "fn", "--repo", REPO, "-n", "-B", "1"]);
    assert_eq!(human.status.code(), Some(0), "{}", stderr_of(&human));
    assert_eq!(
        stdout_of(&human),
        "big.txt:1:fn trimmed\n--\nsmall.txt-3-a\nsmall.txt:4:fn whole\n"
    );
    assert_eq!(
        stderr_of(&human),
        format!(
            "read at version 7, commit {}\nthis call cut 1 of its 2 matches to fit the page (at most 8 MiB a call); --json marks each with cut: true, and syns cat PATH prints the file whole\n",
            sha_of(7)
        )
    );

    // On a cut page the line follows the continuation line.
    let d = Deployment::new(&fixture, Search::Fixed(200, "application/json", page(true)));
    let human = d.run(&["grep", "fn", "--repo", REPO]);
    let lines: Vec<String> = stderr_of(&human).lines().map(str::to_string).collect();
    assert_eq!(lines.len(), 3, "{lines:?}");
    assert!(
        lines[1].starts_with("this call took matches 1 to 2 of the 3"),
        "{lines:?}"
    );
    assert!(
        lines[2].starts_with("this call cut 1 of its 2 matches"),
        "{lines:?}"
    );

    // A page with no cut row writes no such line.
    let whole = Deployment::new(&fixture, Search::Engine);
    let human = whole.run(&["grep", "fn", "--repo", REPO]);
    assert!(
        !stderr_of(&human).contains("this call cut"),
        "{}",
        stderr_of(&human)
    );
}

// D-252 §13: `422 search_too_costly` is refused as any `422` — exit 1,
// `--json` `code: "search_too_costly"`, the server's message shown, sent
// once, and no fallback.
#[test]
#[serial]
fn a_search_too_costly_is_refused_with_its_message_and_no_fallback() {
    let fixture = Fixture::one(mixed_files());
    let message = "the search passed its 20000 ms scan; narrow it with --path or --glob";
    let d = Deployment::new(
        &fixture,
        Search::Fixed(
            422,
            "application/json",
            json!({"error": "search_too_costly", "message": message}).to_string(),
        ),
    );
    let machine = d.run(&["--json", "grep", "fn", "--repo", REPO]);
    assert_eq!(machine.status.code(), Some(1), "{}", stderr_of(&machine));
    let refusal = document(&machine);
    assert_eq!(refusal["code"], json!("search_too_costly"), "{refusal}");
    assert!(
        refusal["error"]
            .as_str()
            .unwrap_or_default()
            .contains(message),
        "{refusal}"
    );

    let human = d.run(&["grep", "fn", "--repo", REPO]);
    assert_eq!(human.status.code(), Some(1));
    assert!(stdout_of(&human).is_empty());
    assert_eq!(
        stderr_of(&human).trim_end(),
        format!("error: server error (422): search_too_costly \u{2014} {message}")
    );
    assert!(
        d.content_reads().is_empty(),
        "no fallback: {:?}",
        d.content_reads()
    );
    assert_eq!(d.searches().len(), 2, "one search per run, none resent");
}

// ---------------------------------------------------------------- fix round 3

// VERIFY O6: the engine's `invalid_pattern`, relayed as `422
// validation_error`, shows the server's message — a pattern past 4 KiB,
// globs past 4 KiB together — beside the code as served, exit 1, sent
// once, with no fallback.
#[test]
#[serial]
fn an_invalid_pattern_refusal_shows_the_servers_reason() {
    let fixture = Fixture::one(mixed_files());
    let long = "a".repeat(4097);
    for message in [
        format!("invalid pattern {long}: the pattern passes 4096 bytes"),
        "invalid glob gg: the globs pass 4096 bytes together".to_string(),
    ] {
        let d = Deployment::new(
            &fixture,
            Search::Fixed(
                422,
                "application/json",
                json!({"error": "validation_error", "message": message}).to_string(),
            ),
        );
        let machine = d.run(&["--json", "grep", &long, "--repo", REPO]);
        assert_eq!(machine.status.code(), Some(1), "{}", stderr_of(&machine));
        let refusal = document(&machine);
        assert_eq!(refusal["code"], json!("validation_error"));
        assert_eq!(
            refusal["error"],
            json!(format!(
                "server error (422): validation_error \u{2014} {message}"
            ))
        );

        let human = d.run(&["grep", &long, "--repo", REPO]);
        assert_eq!(human.status.code(), Some(1));
        assert!(stdout_of(&human).is_empty());
        assert_eq!(
            stderr_of(&human).trim_end(),
            format!("error: server error (422): validation_error \u{2014} {message}")
        );
        assert!(
            d.content_reads().is_empty(),
            "no fallback: {:?}",
            d.content_reads()
        );
        assert_eq!(d.searches().len(), 2, "one search per run");
    }
}
