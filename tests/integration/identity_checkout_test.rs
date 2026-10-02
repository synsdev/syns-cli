//! Binary-level behaviour of a folder bound to a shared folder's identity
//! (SPEC u302 Tests): the identity checked out, worked through, returned
//! to its holder, and its collaborators reached.
//!
//! Every binary row of that table stands here under the name the table
//! gives it; `the_base_recorded_under_the_holder_carries_to_the_identity`
//! and `carry_base_never_overwrites_a_later_base` stand in the tests
//! module of `src/push/working_copy.rs`,
//! `a_carried_base_the_identity_does_not_list_is_numbered_through_the_holder`
//! in that of `src/push/folder_check.rs`, and
//! `carol_edits_her_shared_folder_in_a_thread_page_session` is driven at
//! verification against a deployment. Each test drives one deployment of
//! its own — the SPEC Tests default: `U/q3-plan` whose `.syns.yaml` reads
//! `holder: alice/docs`, `path: q3-plan` and `shared_as: docs-q3-plan`
//! with no identity file above `U`, a stored credential, a mock answering
//! the identity's version list at `limit=1` with version `4` changing
//! `document.html`, its tree and contents at `ref=4` and at the hash of
//! version `4`, and the holder's raw `.synsignore` as `404` `not_found`.

use assert_cmd::Command as AssertCommand;
use serde_json::{Value, json};
use serial_test::serial;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use syns_cli::config::StoreRoots;
use syns_cli::push::hash::blob_sha1;
use syns_cli::push::working_copy::WorkingCopy;
use wiremock::matchers::{method, path, path_regex, query_param, query_param_is_missing};
use wiremock::{Mock, MockServer, Request, ResponseTemplate};

const H4: &str = "4444444444444444444444444444444444444444";
const H5: &str = "5555555555555555555555555555555555555555";
const H6: &str = "6666666666666666666666666666666666666666";
const H7: &str = "7777777777777777777777777777777777777777";
const H8: &str = "8888888888888888888888888888888888888888";

const IDENTITY: &str = "/api/v1/repos/alice/docs-q3-plan";
const HOLDER: &str = "/api/v1/repos/alice/docs";

/// The folder's `.syns.yaml`, as the share wrote it.
const SHARED: &str = "holder: alice/docs\npath: q3-plan\nshared_as: docs-q3-plan\n";
/// `document.html` at version 4.
const DOC: &str = "<p>q3 plan</p>\n";

/// A `Repository` record as `EP-get-repo` serves it.
fn record(owner: &str, name: &str, commit: Option<&str>, shared_folder: bool) -> Value {
    json!({
        "owner": owner, "name": name, "description": null,
        "commitSha": commit, "status": "active", "author": null, "tags": [],
        "visibility": "private", "forkedFrom": null, "forkCount": 0,
        "fileCount": 2, "role": "owner", "sharedFolder": shared_folder,
        "createdAt": "2026-10-02T00:00:00Z", "updatedAt": "2026-10-02T00:00:00Z",
    })
}

fn version(number: u32, sha: &str, changed: &[&str]) -> Value {
    json!({
        "version": number, "sha": sha, "parentSha": null, "message": "m",
        "messageBody": null, "author": "alice",
        "createdAt": "2026-10-02T00:00:00Z", "filesChanged": changed,
    })
}

fn page(entries: Vec<Value>, limit: u32) -> Value {
    let total = entries.len();
    json!({"data": entries, "total": total, "limit": limit, "offset": 0})
}

fn refusal(status: u16, error: &str) -> ResponseTemplate {
    ResponseTemplate::new(status).set_body_json(json!({"error": error, "message": error}))
}

fn tree_entry(path: &str, content: &str) -> Value {
    json!({
        "name": path.rsplit('/').next().unwrap(), "path": path, "type": "file",
        "size": content.len(), "sha": blob_sha1(content.as_bytes()),
    })
}

fn raw(content: &str) -> ResponseTemplate {
    ResponseTemplate::new(200)
        .insert_header(
            "ETag",
            format!("\"{}\"", blob_sha1(content.as_bytes())).as_str(),
        )
        .set_body_bytes(content.as_bytes().to_vec())
}

fn file(content: &str) -> ResponseTemplate {
    ResponseTemplate::new(200).set_body_json(json!({
        "path": "document.html", "content": content,
        "sha": blob_sha1(content.as_bytes()), "size": content.len(),
    }))
}

struct Deployment {
    rt: tokio::runtime::Runtime,
    server: MockServer,
    home: tempfile::TempDir,
    cache: tempfile::TempDir,
    _work: tempfile::TempDir,
    /// The working area, canonical; `U` of the SPEC Tests setup.
    u: PathBuf,
}

impl Deployment {
    /// The SPEC Tests default deployment.
    fn new() -> Deployment {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime");
        let server = rt.block_on(MockServer::start());
        let work = tempfile::Builder::new()
            .prefix("u302-identity-")
            .tempdir()
            .expect("working dir");
        let u = std::fs::canonicalize(work.path()).expect("canonical U");
        let home = tempfile::tempdir().expect("config dir");
        syns_cli::auth::token::TokenStore::new(home.path().join("credentials.json"))
            .write_with_username("test-token", Some("alice"))
            .expect("credential");
        let d = Deployment {
            rt,
            server,
            home,
            cache: tempfile::tempdir().expect("cache dir"),
            _work: work,
            u,
        };
        write(&d.folder().join(".syns.yaml"), SHARED);
        d.identity_newest(version(4, H4, &["document.html"]));
        for at in ["4", H4] {
            d.mount(
                Mock::given(method("GET"))
                    .and(path(format!("{IDENTITY}/tree")))
                    .and(query_param("ref", at))
                    .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                        "entries": [tree_entry(".syns.yaml", SHARED), tree_entry("document.html", DOC)],
                        "commitSha": H4, "truncated": false,
                    }))),
            );
            d.mount(
                Mock::given(method("GET"))
                    .and(path(format!("{IDENTITY}/files/document.html")))
                    .and(query_param("ref", at))
                    .respond_with(file(DOC)),
            );
            d.mount(
                Mock::given(method("GET"))
                    .and(path(format!("{IDENTITY}/raw/document.html")))
                    .and(query_param("ref", at))
                    .respond_with(raw(DOC)),
            );
        }
        d.serves("GET", &format!("{IDENTITY}/raw/.syns.yaml"), raw(SHARED));
        d.serves(
            "GET",
            &format!("{HOLDER}/raw/.synsignore"),
            refusal(404, "not_found"),
        );
        d
    }

    /// `U/q3-plan`.
    fn folder(&self) -> PathBuf {
        self.u.join("q3-plan")
    }

    fn stores(&self) -> StoreRoots {
        StoreRoots::resolve(
            Some(self.cache.path()),
            self.cache.path(),
            self.cache.path(),
        )
    }

    fn mount(&self, mock: Mock) {
        self.rt.block_on(mock.mount(&self.server));
    }

    fn serves(&self, verb: &str, address: &str, answer: ResponseTemplate) {
        self.mount(
            Mock::given(method(verb))
                .and(path(address.to_string()))
                .respond_with(answer),
        );
    }

    /// `verb` at `address` answering `answer` ahead of every default.
    fn overrides(&self, verb: &str, address: &str, answer: ResponseTemplate) {
        self.mount(
            Mock::given(method(verb))
                .and(path(address.to_string()))
                .respond_with(answer)
                .with_priority(1),
        );
    }

    /// The identity's version list at `limit=1` naming no path answering
    /// `newest` alone, ahead of any earlier answer.
    fn identity_newest(&self, newest: Value) {
        self.mount(
            Mock::given(method("GET"))
                .and(path(format!("{IDENTITY}/versions")))
                .and(query_param("limit", "1"))
                .and(query_param_is_missing("path"))
                .respond_with(ResponseTemplate::new(200).set_body_json(page(vec![newest], 1)))
                .with_priority(3),
        );
    }

    /// The identity's record, marking a shared folder, at `commit`.
    fn identity_record(&self, commit: &str) {
        self.serves(
            "GET",
            IDENTITY,
            ResponseTemplate::new(200).set_body_json(record(
                "alice",
                "docs-q3-plan",
                Some(commit),
                true,
            )),
        );
    }

    /// The identity copy at `U/q3-plan` recording `commit` over the
    /// folder's files as they stand on disk.
    fn record_base(&self, commit: &str) {
        let copy = WorkingCopy::open(&self.stores(), "alice", "docs-q3-plan", &self.folder())
            .expect("the identity copy");
        let mut files = HashMap::new();
        for (place, bytes) in files_under(&self.folder()) {
            files.insert(place, blob_sha1(&bytes));
        }
        copy.record_base(commit, files).expect("a base");
    }

    fn requests(&self) -> Vec<Request> {
        self.rt
            .block_on(self.server.received_requests())
            .expect("requests")
    }

    /// Every request as `METHOD path?query`.
    fn targets(&self) -> Vec<String> {
        self.requests()
            .into_iter()
            .map(|r| {
                let query = r.url.query().map(|q| format!("?{q}")).unwrap_or_default();
                format!("{} {}{query}", r.method, r.url.path())
            })
            .collect()
    }

    /// The bodies of every push sent to `address`.
    fn pushes(&self, address: &str) -> Vec<Value> {
        self.requests()
            .into_iter()
            .filter(|r| r.method.as_str() == "PUT" && r.url.path() == format!("{address}/push"))
            .map(|r| serde_json::from_slice(&r.body).expect("a JSON body"))
            .collect()
    }

    fn command(&self, cwd: &Path, args: &[&str]) -> AssertCommand {
        let mut command = std::process::Command::new(assert_cmd::cargo::cargo_bin("syns"));
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
            .env_remove("SYNS_TASK")
            .env_remove("CI");
        command.arg("--server").arg(self.server.uri()).args(args);
        AssertCommand::from_std(command)
    }

    /// `syns` with `args` in `cwd`, standard input holding `stdin`.
    fn run_with(&self, cwd: &Path, args: &[&str], stdin: &str) -> std::process::Output {
        self.command(cwd, args)
            .write_stdin(stdin.as_bytes().to_vec())
            .timeout(std::time::Duration::from_secs(120))
            .output()
            .expect("run syns")
    }

    /// `syns` with `args` in `cwd`, standard input closed.
    fn run_in(&self, cwd: &Path, args: &[&str]) -> std::process::Output {
        self.run_with(cwd, args, "")
    }

    /// `syns` with `args` in `U/q3-plan`.
    fn run(&self, args: &[&str]) -> std::process::Output {
        self.run_in(&self.folder(), args)
    }
}

fn write(file: &Path, content: &str) {
    std::fs::create_dir_all(file.parent().expect("a parent")).expect("parent dir");
    std::fs::write(file, content).expect("file written");
}

/// Every file under `dir`, by its `/`-joined place, beside its bytes.
fn files_under(dir: &Path) -> Vec<(String, Vec<u8>)> {
    fn walk(root: &Path, dir: &Path, out: &mut Vec<(String, Vec<u8>)>) {
        for entry in std::fs::read_dir(dir).into_iter().flatten().flatten() {
            let p = entry.path();
            if p.is_dir() {
                walk(root, &p, out);
            } else {
                let place = p
                    .strip_prefix(root)
                    .unwrap()
                    .to_string_lossy()
                    .replace('\\', "/");
                out.push((place, std::fs::read(&p).unwrap()));
            }
        }
    }
    let mut out = Vec::new();
    walk(dir, dir, &mut out);
    out.sort();
    out
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

fn document(output: &std::process::Output) -> Value {
    serde_json::from_str(stdout_of(output).trim()).expect("one document on the primary stream")
}

/// The paths a push body's `files` name.
fn pushed_paths(body: &Value) -> Vec<String> {
    let mut paths: Vec<String> = body["files"]
        .as_array()
        .expect("files")
        .iter()
        .map(|f| f["path"].as_str().unwrap().to_string())
        .collect();
    paths.sort();
    paths
}

/// The paths a push body's `files` carry content for.
fn sent_content(body: &Value) -> Vec<String> {
    let mut paths: Vec<String> = body["files"]
        .as_array()
        .expect("files")
        .iter()
        .filter(|f| f.get("content").is_some() || f.get("contentBase64").is_some())
        .map(|f| f["path"].as_str().unwrap().to_string())
        .collect();
    paths.sort();
    paths
}

// ---- binding --------------------------------------------------------------

// SPEC u302 Tests, the row of this name.
#[test]
#[serial]
fn a_folder_alone_carrying_shared_as_works_through_its_identity() {
    let d = Deployment::new();
    d.mount(
        Mock::given(method("GET"))
            .and(path(format!("{IDENTITY}/versions")))
            .and(query_param("limit", "50"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(page(vec![version(4, H4, &["document.html"])], 50)),
            ),
    );
    d.mount(
        Mock::given(method("GET"))
            .and(path(format!("{IDENTITY}/diff")))
            .and(query_param("from", "3"))
            .and(query_param("to", "4"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "from": {"version": 3, "sha": H5}, "to": {"version": 4, "sha": H4},
                "files": [{"path": "document.html", "status": "modified", "diff": "@@ -1 +1 @@\n-a\n+b\n"}],
            }))),
    );
    let sub = d.folder().join("sub");
    std::fs::create_dir_all(&sub).unwrap();

    let ls = d.run_in(&sub, &["--json", "ls"]);
    let read = d.run_in(&sub, &["--json", "read", "document.html"]);
    let history = d.run_in(&sub, &["--json", "history"]);
    let diff = d.run_in(&sub, &["--json", "diff"]);

    for (verb, out) in [
        ("ls", &ls),
        ("read", &read),
        ("history", &history),
        ("diff", &diff),
    ] {
        assert_eq!(
            exit_of(out),
            0,
            "{verb}: {}{}",
            stderr_of(out),
            stdout_of(out)
        );
    }
    let targets = d.targets();
    for target in &targets {
        assert!(
            target.contains(&format!("{IDENTITY}/")) && !target.contains(&format!("{HOLDER}/")),
            "{target}"
        );
    }
    let trees: Vec<&String> = targets.iter().filter(|t| t.contains("/tree")).collect();
    assert_eq!(trees.len(), 1, "{trees:?}");
    assert!(
        trees[0].starts_with(&format!("GET {IDENTITY}/tree?")) && trees[0].contains("ref=4"),
        "{}",
        trees[0]
    );
    assert!(
        targets
            .iter()
            .filter(|t| t.contains("/versions"))
            .all(|t| !t.contains("path=")),
        "{targets:?}"
    );
    let listing = document(&ls);
    let mut listed: Vec<&str> = listing["entries"]
        .as_array()
        .expect("entries")
        .iter()
        .map(|e| e["path"].as_str().unwrap())
        .collect();
    listed.sort();
    assert_eq!(listed, vec![".syns.yaml", "document.html"]);
    assert_eq!(listing["version"], json!(4));
    assert_eq!(
        document(&history)["data"][0]["filesChanged"],
        json!(["document.html"])
    );
    assert_eq!(document(&diff)["files"][0]["path"], json!("document.html"));
}

// SPEC u302 Tests, the row of this name.
#[test]
#[serial]
fn a_shared_folder_in_its_holders_checkout_stays_bound_to_the_holder() {
    let d = Deployment::new();
    let w = d.u.join("W");
    write(&w.join(".syns.yaml"), "owner: alice\nname: docs\n");
    write(&w.join("q3-plan/.syns.yaml"), SHARED);
    holder_reads_at_q3_plan(&d, "q3-plan");

    let out = d.run_in(&w.join("q3-plan"), &["--json", "ls"]);

    assert_eq!(exit_of(&out), 0, "{}", stderr_of(&out));
    assert!(
        d.targets()
            .iter()
            .any(|t| t.starts_with(&format!("GET {HOLDER}/tree/q3-plan?"))),
        "{:?}",
        d.targets()
    );
}

/// The holder's record at `H8`, version 8 at that hash, and its tree at
/// `place` answering `.syns.yaml` and `document.html` under it.
fn holder_reads_at_q3_plan(d: &Deployment, place: &str) {
    d.serves(
        "GET",
        HOLDER,
        ResponseTemplate::new(200).set_body_json(record("alice", "docs", Some(H8), false)),
    );
    d.serves(
        "GET",
        &format!("{HOLDER}/versions/{H8}"),
        ResponseTemplate::new(200).set_body_json(version(8, H8, &["q3-plan/document.html"])),
    );
    d.serves(
        "GET",
        &format!("{HOLDER}/tree/{place}"),
        ResponseTemplate::new(200).set_body_json(json!({
            "entries": [
                tree_entry(&format!("{place}/.syns.yaml"), "holder: alice/docs\n"),
                tree_entry(&format!("{place}/document.html"), DOC),
            ],
            "commitSha": H8, "truncated": false,
        })),
    );
}

// SPEC u302 Tests, the row of this name.
#[test]
#[serial]
fn a_shared_folder_under_a_folder_bound_to_its_holder_stays_bound_to_the_holder() {
    let d = Deployment::new();
    let v = d.u.join("V");
    write(
        &v.join("q3-plan/.syns.yaml"),
        "holder: alice/docs\npath: q3-plan\n",
    );
    write(
        &v.join("q3-plan/appendix/.syns.yaml"),
        "holder: alice/docs\npath: q3-plan/appendix\nshared_as: docs-q3-plan-appendix\n",
    );
    holder_reads_at_q3_plan(&d, "q3-plan/appendix");

    let out = d.run_in(&v.join("q3-plan/appendix"), &["--json", "ls"]);

    assert_eq!(exit_of(&out), 0, "{}", stderr_of(&out));
    assert!(
        d.targets()
            .iter()
            .any(|t| t.starts_with(&format!("GET {HOLDER}/tree/q3-plan/appendix?"))),
        "{:?}",
        d.targets()
    );
}

// SPEC u302 Tests, the row of this name.
#[test]
#[serial]
fn a_folder_nested_in_an_identity_checkout_reads_as_part_of_it() {
    let d = Deployment::new();
    write(
        &d.folder().join("appendix/.syns.yaml"),
        "holder: alice/docs\npath: q3-plan/appendix\nshared_as: docs-q3-plan-appendix\n",
    );
    write(
        &d.folder().join("notes/.syns.yaml"),
        "owner: bob\nname: notes\n",
    );

    let appendix = d.run_in(&d.folder().join("appendix"), &["--json", "ls"]);
    let notes = d.run_in(&d.folder().join("notes"), &["--json", "ls"]);

    assert_eq!(exit_of(&appendix), 0, "{}", stderr_of(&appendix));
    assert_eq!(exit_of(&notes), 0, "{}", stderr_of(&notes));
    let trees: Vec<String> = d
        .targets()
        .into_iter()
        .filter(|t| t.contains("/tree"))
        .collect();
    assert_eq!(trees.len(), 2, "{trees:?}");
    for tree in &trees {
        assert!(tree.starts_with(&format!("GET {IDENTITY}/tree?")), "{tree}");
        assert!(!tree.contains("path="), "{tree}");
    }
}

// ---- the identity checkout --------------------------------------------

// SPEC u302 Tests, the row of this name.
#[test]
#[serial]
fn pull_of_an_identity_writes_its_folder_and_no_identity_file_of_its_own() {
    let d = Deployment::new();
    d.identity_record(H6);
    let c = d.u.join("C");
    std::fs::create_dir_all(&c).unwrap();

    let pull = d.run_in(&d.u, &["--json", "pull", "alice/docs-q3-plan", "C"]);
    let repo = d.run_in(&c, &["--json", "repo"]);

    assert_eq!(exit_of(&pull), 0, "{}", stderr_of(&pull));
    assert_eq!(exit_of(&repo), 0, "{}", stderr_of(&repo));
    assert_eq!(
        std::fs::read_to_string(c.join(".syns.yaml")).unwrap(),
        SHARED
    );
    assert_eq!(
        std::fs::read_to_string(c.join("document.html")).unwrap(),
        DOC
    );
    let names: Vec<String> = files_under(&c).into_iter().map(|(p, _)| p).collect();
    assert_eq!(names, vec![".syns.yaml", "document.html"]);
    let document = document(&repo);
    assert_eq!(document["owner"], json!("alice"));
    assert_eq!(document["name"], json!("docs-q3-plan"));
    assert_eq!(document["sharedFolder"], json!(true));
    assert_eq!(document["commitSha"], json!(H4));
    assert_eq!(document["version"], json!(4));
    assert!(document.get("holder").is_none(), "{document}");
    assert!(document.get("path").is_none(), "{document}");
    assert!(
        !d.cache.path().join("alice/docs-q3-plan").exists()
            && !d.cache.path().join("manifests").exists(),
        "a local record was written"
    );
}

// SPEC u302 Tests, the row of this name.
#[test]
#[serial]
fn pull_of_an_identity_at_a_version_predating_its_share_writes_nothing() {
    let d = Deployment::new();
    d.identity_record(H4);
    d.mount(
        Mock::given(method("GET"))
            .and(path(format!("{IDENTITY}/raw/.syns.yaml")))
            .and(query_param("ref", "2"))
            .respond_with(raw("holder: alice/docs\npath: q3-plan\n"))
            .with_priority(1),
    );

    let out = d.run_in(&d.u, &["pull", "alice/docs-q3-plan", "C", "--version", "2"]);

    assert_eq!(exit_of(&out), 1, "{}", stderr_of(&out));
    assert!(
        stderr_of(&out).contains("error: configuration error: alice/docs-q3-plan holds no .syns.yaml naming it as a shared folder at 2; only a shared folder whose .syns.yaml names it is checked out from its name"),
        "{}",
        stderr_of(&out)
    );
    assert!(!d.u.join("C").exists());
    assert!(d.targets().iter().all(|t| !t.contains("/tree")));
}

// SPEC u302 Tests, the row of this name.
#[test]
#[serial]
fn pull_of_an_identity_whose_root_holds_no_identity_file_writes_nothing() {
    let d = Deployment::new();
    d.identity_record(H4);
    d.overrides(
        "GET",
        &format!("{IDENTITY}/raw/.syns.yaml"),
        refusal(404, "not_found"),
    );

    let out = d.run_in(&d.u, &["pull", "alice/docs-q3-plan", "C"]);

    assert_eq!(exit_of(&out), 1, "{}", stderr_of(&out));
    assert!(
        stderr_of(&out).contains("at the tip; only a shared folder whose .syns.yaml names it is checked out from its name"),
        "{}",
        stderr_of(&out)
    );
    assert!(!d.u.join("C").exists());
    assert!(d.targets().iter().all(|t| !t.contains("/tree")));
}

// SPEC u302 Tests, the row of this name.
#[test]
#[serial]
fn pull_naming_the_holder_inside_an_identity_folder_is_refused() {
    let d = Deployment::new();

    let out = d.run(&["pull", "alice/docs"]);

    assert_eq!(exit_of(&out), 2, "{}", stderr_of(&out));
    let stderr = stderr_of(&out);
    assert!(stderr.contains("already belongs to"), "{stderr}");
    assert!(
        stderr.contains(&d.folder().display().to_string()),
        "{stderr}"
    );
    assert!(stderr.contains("alice/docs-q3-plan"), "{stderr}");
    assert!(d.requests().is_empty(), "{:?}", d.targets());
}

// ---- reads, writes and publications -----------------------------------

// SPEC u302 Tests, the row of this name.
#[test]
#[serial]
fn reads_through_an_identity_take_its_newest_listed_version_as_head() {
    let d = Deployment::new();
    d.identity_record(H6);

    let repo = d.run(&["--json", "repo"]);
    let status = d.run(&["--json", "status"]);
    let read = d.run(&["--json", "read", "document.html"]);
    let named = d.run_in(
        &d.u,
        &[
            "--json",
            "read",
            "--repo",
            "alice/docs-q3-plan",
            "document.html",
        ],
    );

    for (verb, out) in [
        ("repo", &repo),
        ("status", &status),
        ("read", &read),
        ("read --repo", &named),
    ] {
        assert_eq!(exit_of(out), 0, "{verb}: {}", stderr_of(out));
    }
    let repo = document(&repo);
    assert_eq!(repo["commitSha"], json!(H4));
    assert_eq!(repo["version"], json!(4));
    let status = document(&status);
    assert_eq!(status["commitSha"], json!(H4));
    assert!(status.get("holder").is_none(), "{status}");
    for out in [&read, &named] {
        assert_eq!(document(out)["version"], json!(4));
    }
    let targets = d.targets();
    let reads: Vec<&String> = targets
        .iter()
        .filter(|t| t.contains("/files/document.html"))
        .collect();
    assert_eq!(reads.len(), 2, "{targets:?}");
    assert!(reads.iter().all(|t| t.ends_with("ref=4")), "{reads:?}");
    assert!(targets.iter().all(|t| !t.contains(H6)), "{targets:?}");
}

// SPEC u302 Tests, the row of this name.
#[test]
#[serial]
fn a_sync_through_an_identity_lands_past_a_holder_version_outside_the_folder() {
    let d = Deployment::new();
    write(&d.folder().join("document.html"), DOC);
    d.record_base(H4);
    write(&d.folder().join("document.html"), "<p>edited</p>\n");
    d.mount(
        Mock::given(method("PUT"))
            .and(path(format!("{IDENTITY}/push")))
            .and(wiremock::matchers::body_partial_json(
                json!({"parentSha": H4}),
            ))
            .respond_with(ResponseTemplate::new(409).set_body_json(json!({
                "error": "conflict", "message": "Head mismatch", "currentSha": H6,
            }))),
    );
    d.mount(
        Mock::given(method("PUT"))
            .and(path(format!("{IDENTITY}/push")))
            .and(wiremock::matchers::body_partial_json(
                json!({"parentSha": H6}),
            ))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "commitSha": H7, "version": 7, "filesChanged": 1, "created": false,
            }))),
    );

    let out = d.run(&["--json", "sync"]);

    assert_eq!(exit_of(&out), 0, "{}", stderr_of(&out));
    let pushes = d.pushes(IDENTITY);
    assert_eq!(pushes.len(), 2, "{:?}", d.targets());
    assert_eq!(pushes[0]["parentSha"], json!(H4));
    assert_eq!(pushes[1]["parentSha"], json!(H6));
    for body in &pushes {
        assert_eq!(sent_content(body), vec!["document.html"], "{body}");
        assert!(
            pushed_paths(body)
                .iter()
                .all(|p| p == "document.html" || p == ".syns.yaml"),
            "{body}"
        );
    }
    let copy = WorkingCopy::open_existing(&d.stores(), "alice", "docs-q3-plan", &d.folder())
        .unwrap()
        .expect("the identity copy");
    assert_eq!(copy.base().unwrap().commit_sha(), Some(H7));
}

// SPEC u302 Tests, the row of this name.
#[test]
#[serial]
fn a_write_through_an_identity_past_a_moved_folder_is_refused() {
    let d = Deployment::new();
    write(&d.folder().join("document.html"), DOC);
    d.record_base(H4);
    d.identity_record(H6);
    d.mount(
        Mock::given(method("GET"))
            .and(path(format!("{IDENTITY}/versions")))
            .and(query_param("limit", "1"))
            .and(query_param_is_missing("path"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(page(vec![version(5, H5, &["document.html"])], 1)),
            )
            .with_priority(1),
    );
    d.serves(
        "GET",
        &format!("{IDENTITY}/versions/{H4}"),
        ResponseTemplate::new(200).set_body_json(version(4, H4, &["document.html"])),
    );

    let out = d.run_with(
        &d.folder(),
        &["--json", "write", "document.html", "--parent", H4],
        "x",
    );

    assert_eq!(exit_of(&out), 7, "{}", stderr_of(&out));
    assert!(
        stdout_of(&out).contains(H5) || stderr_of(&out).contains(H5),
        "{}{}",
        stdout_of(&out),
        stderr_of(&out)
    );
    assert!(d.pushes(IDENTITY).is_empty() && d.pushes(HOLDER).is_empty());
}

// SPEC u302 Tests, the row of this name.
#[test]
#[serial]
fn a_write_inside_an_identity_folder_holding_unpublished_work_is_refused() {
    let d = Deployment::new();
    write(&d.folder().join("document.html"), DOC);
    d.record_base(H4);
    write(&d.folder().join("document.html"), "<p>edited</p>\n");
    d.identity_record(H4);

    let out = d.run_with(
        &d.folder(),
        &["--json", "write", "document.html", "--parent", H4],
        "x",
    );

    assert_eq!(exit_of(&out), 1, "{}", stderr_of(&out));
    let said = format!("{}{}", stdout_of(&out), stderr_of(&out));
    assert!(
        said.contains(&format!(
            "the checkout at {} holds unpublished local changes for alice/docs-q3-plan",
            d.folder().display()
        )),
        "{said}"
    );
    assert!(d.pushes(IDENTITY).is_empty() && d.pushes(HOLDER).is_empty());
}

// The ruling on round 1's open question: a write naming the holder
// through `--repo`, run inside a folder bound to its identity while that
// folder holds unpublished work, is refused as a write to the identity
// there is, so no write to the holder lands over the folder's local work.
#[test]
#[serial]
fn a_write_to_the_holder_inside_an_identity_folder_holding_unpublished_work_is_refused() {
    let d = Deployment::new();
    write(&d.folder().join("document.html"), DOC);
    d.record_base(H4);
    write(&d.folder().join("document.html"), "<p>edited</p>\n");
    d.serves(
        "GET",
        HOLDER,
        ResponseTemplate::new(200).set_body_json(record("alice", "docs", Some(H4), false)),
    );
    d.serves(
        "PUT",
        &format!("{HOLDER}/push"),
        ResponseTemplate::new(200).set_body_json(json!({
            "commitSha": H5, "version": 5, "filesChanged": 1, "created": false,
        })),
    );

    let out = d.run_with(
        &d.folder(),
        &[
            "--json",
            "write",
            "--repo",
            "alice/docs",
            "q3-plan/document.html",
            "--parent",
            H4,
        ],
        "x",
    );

    assert_eq!(exit_of(&out), 1, "{}", stderr_of(&out));
    let said = format!("{}{}", stdout_of(&out), stderr_of(&out));
    assert!(
        said.contains(&format!(
            "the checkout at {} holds unpublished local changes for alice/docs-q3-plan",
            d.folder().display()
        )),
        "{said}"
    );
    assert!(d.pushes(IDENTITY).is_empty() && d.pushes(HOLDER).is_empty());
}

// The ruling on round 1's open question, the clean side: the same write
// from a folder holding no unpublished work goes to the holder.
#[test]
#[serial]
fn a_write_to_the_holder_inside_a_clean_identity_folder_lands_on_the_holder() {
    let d = Deployment::new();
    write(&d.folder().join("document.html"), DOC);
    d.record_base(H4);
    d.serves(
        "GET",
        HOLDER,
        ResponseTemplate::new(200).set_body_json(record("alice", "docs", Some(H4), false)),
    );
    d.serves(
        "PUT",
        &format!("{HOLDER}/push"),
        ResponseTemplate::new(200).set_body_json(json!({
            "commitSha": H5, "version": 5, "filesChanged": 1, "created": false,
        })),
    );

    let out = d.run_with(
        &d.folder(),
        &[
            "--json",
            "write",
            "--repo",
            "alice/docs",
            "q3-plan/document.html",
            "--parent",
            H4,
        ],
        "x",
    );

    assert_eq!(exit_of(&out), 0, "{}", stderr_of(&out));
    let pushes = d.pushes(HOLDER);
    assert_eq!(pushes.len(), 1, "{:?}", d.targets());
    assert_eq!(pushed_paths(&pushes[0]), vec!["q3-plan/document.html"]);
    assert!(d.pushes(IDENTITY).is_empty());
    // The write lands under the folder's holder path, so the identity
    // lists it and the folder is left one version behind (CR2-1).
    assert_eq!(
        document(&out)["checkoutBehind"],
        json!(d.folder().display().to_string())
    );
}

// The ruling on UNP1-1: a `shared_as` spelling no repository name under
// the holder's owner stays refused, before any request.
#[test]
#[serial]
fn a_shared_as_spelling_no_repository_name_is_refused() {
    let d = Deployment::new();
    write(
        &d.folder().join(".syns.yaml"),
        "holder: alice/docs\npath: q3-plan\nshared_as: ../docs\n",
    );

    let out = d.run(&["ls"]);

    assert_eq!(exit_of(&out), 1, "{}", stderr_of(&out));
    assert!(
        stderr_of(&out).contains(
            "invalid .syns.yaml: shared_as must name a repository under alice (got ../docs)"
        ),
        "{}",
        stderr_of(&out)
    );
    assert!(d.targets().is_empty(), "{:?}", d.targets());
}

// SPEC u302 Tests, the row of this name.
#[test]
#[serial]
fn a_path_scoped_push_inside_an_identity_folder_goes_through_it() {
    let d = Deployment::new();
    write(&d.folder().join("document.html"), DOC);
    d.record_base(H4);
    write(&d.folder().join("document.html"), "<p>edited</p>\n");
    d.serves(
        "PUT",
        &format!("{IDENTITY}/push"),
        ResponseTemplate::new(200).set_body_json(json!({
            "commitSha": H5, "version": 5, "filesChanged": 1, "created": false,
        })),
    );

    let out = d.run(&["--json", "push", "document.html"]);

    assert_eq!(exit_of(&out), 0, "{}", stderr_of(&out));
    let pushes = d.pushes(IDENTITY);
    assert_eq!(pushes.len(), 1, "{:?}", d.targets());
    assert_eq!(pushed_paths(&pushes[0]), vec!["document.html"]);
    assert_eq!(pushes[0]["parentSha"], json!(H4));
    let to_holder: Vec<String> = d
        .targets()
        .into_iter()
        .filter(|t| t.contains(&format!("{HOLDER}/")))
        .collect();
    assert!(
        to_holder
            .iter()
            .all(|t| t.starts_with(&format!("GET {HOLDER}/raw/.synsignore"))),
        "{to_holder:?}"
    );
}

// ---- collaborators and the hooks ---------------------------------------

// SPEC u302 Tests, the row of this name.
#[test]
#[serial]
fn bare_collaborators_inside_an_identity_folder_address_it() {
    let d = Deployment::new();
    d.mount(
        Mock::given(method("POST"))
            .and(path_regex(r"^/api/v1/repos/[^/]+/[^/]+/collaborators$"))
            .respond_with(ResponseTemplate::new(201).set_body_json(json!({
                "user": {"id": "u-dave", "name": "Dave", "username": "dave", "email": "dave@example.test",
                         "emailVerified": true, "image": null,
                         "createdAt": "2026-10-02T00:00:00Z", "updatedAt": "2026-10-02T00:00:00Z"},
                "role": "read", "addedBy": "alice", "createdAt": "2026-10-02T00:00:00Z",
            }))),
    );

    // CR1-3: the role change binds the identity as the add does.
    d.mount(
        Mock::given(method("PATCH"))
            .and(path_regex(r"^/api/v1/repos/[^/]+/[^/]+/collaborators/dave$"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "user": {"id": "dave", "name": "Dave", "username": "dave", "email": "dave@example.test",
                         "emailVerified": true, "image": null,
                         "createdAt": "2026-10-02T00:00:00Z", "updatedAt": "2026-10-02T00:00:00Z"},
                "role": "write", "addedBy": "alice", "createdAt": "2026-10-02T00:00:00Z",
            }))),
    );

    let bare = d.run(&["--json", "collaborators", "add", "dave", "--role", "read"]);
    let role = d.run(&["--json", "collaborators", "role", "dave", "--role", "write"]);
    let named = d.run(&[
        "--json",
        "collaborators",
        "add",
        "dave",
        "--role",
        "read",
        "--repo",
        "alice/docs-q3-plan",
    ]);

    for out in [&bare, &role, &named] {
        assert_eq!(exit_of(out), 0, "{}", stderr_of(out));
    }
    let sent: Vec<String> = d
        .targets()
        .into_iter()
        .filter(|t| t.starts_with("POST ") || t.starts_with("PATCH "))
        .collect();
    assert_eq!(
        sent,
        vec![
            format!("POST {IDENTITY}/collaborators"),
            format!("PATCH {IDENTITY}/collaborators/dave"),
            format!("POST {IDENTITY}/collaborators"),
        ]
    );

    let before = d.requests().len();
    for args in [
        vec![
            "collaborators",
            "add",
            "dave",
            "--role",
            "read",
            "--repo",
            "alice/docs",
        ],
        vec![
            "collaborators",
            "role",
            "dave",
            "--role",
            "write",
            "--repo",
            "alice/docs",
        ],
        vec!["delete", "--if-repo"],
        vec!["repo", "--visibility", "public"],
        vec!["fork", "bob/x"],
    ] {
        let out = d.run(&args);
        assert_eq!(exit_of(&out), 2, "{args:?}: {}", stderr_of(&out));
        let stderr = stderr_of(&out);
        assert!(
            stderr.starts_with("error: holder root required: "),
            "{args:?}: {stderr}"
        );
        assert!(stderr.contains("alice/docs"), "{args:?}: {stderr}");
    }
    assert_eq!(d.requests().len(), before, "{:?}", d.targets());
}

// SPEC u302 Tests, the row of this name.
#[test]
#[serial]
fn the_hooks_pull_and_sync_run_through_the_identity() {
    let d = Deployment::new();
    write(&d.folder().join("document.html"), DOC);
    d.record_base(H4);

    let pull = d.run(&["--json", "pull", "--if-repo"]);
    let sync = d.run(&["--json", "sync", "--if-repo"]);

    for out in [&pull, &sync] {
        assert_eq!(exit_of(out), 0, "{}", stderr_of(out));
        assert!(!stdout_of(out).contains("skipped"), "{}", stdout_of(out));
    }
    for target in d.targets() {
        assert!(
            target.contains(&format!("{IDENTITY}/"))
                || target.starts_with(&format!("GET {HOLDER}/raw/.synsignore")),
            "{target}"
        );
    }
    assert!(d.pushes(IDENTITY).is_empty());
}

// ---- the return to the holder ------------------------------------------

/// The holder's `q3-plan/.syns.yaml` once the folder was unshared.
const UNSHARED: &str = "holder: alice/docs\npath: q3-plan\n";

// SPEC u302 Tests, the row of this name.
#[test]
#[serial]
fn an_unshared_identity_returns_a_holder_reader_to_the_holder() {
    let d = Deployment::new();
    write(&d.folder().join("document.html"), DOC);
    d.record_base(H4);
    d.overrides(
        "GET",
        &format!("{IDENTITY}/versions"),
        refusal(404, "not_found"),
    );
    d.serves(
        "GET",
        &format!("{HOLDER}/raw/q3-plan/.syns.yaml"),
        raw(UNSHARED),
    );
    d.serves(
        "GET",
        &format!("{HOLDER}/raw/q3-plan/document.html"),
        raw(DOC),
    );
    d.serves(
        "GET",
        HOLDER,
        ResponseTemplate::new(200).set_body_json(record("alice", "docs", Some(H8), false)),
    );
    d.serves(
        "GET",
        &format!("{HOLDER}/versions/{H8}"),
        ResponseTemplate::new(200).set_body_json(version(8, H8, &["q3-plan/.syns.yaml"])),
    );
    d.serves(
        "GET",
        &format!("{HOLDER}/tree/q3-plan"),
        ResponseTemplate::new(200).set_body_json(json!({
            "entries": [
                tree_entry("q3-plan/.syns.yaml", UNSHARED),
                tree_entry("q3-plan/document.html", DOC),
            ],
            "commitSha": H8, "truncated": false,
        })),
    );

    let pull = d.run(&["--json", "pull"]);
    let ls = d.run(&["--json", "ls"]);

    assert_eq!(exit_of(&pull), 0, "{}", stderr_of(&pull));
    assert_eq!(exit_of(&ls), 0, "{}", stderr_of(&ls));
    let yaml = std::fs::read_to_string(d.folder().join(".syns.yaml")).unwrap();
    assert!(!yaml.contains("shared_as"), "{yaml}");
    let holder = WorkingCopy::open_existing(&d.stores(), "alice", "docs", &d.folder())
        .unwrap()
        .expect("the holder's copy");
    assert_eq!(holder.base().unwrap().commit_sha(), Some(H8));
    let trees: Vec<String> = d
        .targets()
        .into_iter()
        .filter(|t| t.contains("/tree"))
        .collect();
    assert!(
        trees
            .last()
            .is_some_and(|t| t.starts_with(&format!("GET {HOLDER}/tree/q3-plan?"))),
        "{trees:?}"
    );
}

// SPEC u302 Tests, the row of this name.
#[test]
#[serial]
fn an_unshared_identity_stays_missing_for_a_folder_only_reader() {
    let d = Deployment::new();
    write(&d.folder().join("document.html"), DOC);
    d.record_base(H4);
    d.overrides(
        "GET",
        &format!("{IDENTITY}/versions"),
        refusal(404, "not_found"),
    );
    d.serves(
        "GET",
        &format!("{HOLDER}/raw/q3-plan/.syns.yaml"),
        refusal(404, "not_found"),
    );
    let before = files_under(&d.u);

    let out = d.run(&["--json", "pull"]);

    assert_eq!(exit_of(&out), 1, "{}", stderr_of(&out));
    let said = format!("{}{}", stdout_of(&out), stderr_of(&out));
    assert!(said.contains("not_found"), "{said}");
    assert_eq!(files_under(&d.u), before);
}

// ---- place, enable-checks and share -------------------------------------

// SPEC u302 Tests, the row of this name.
#[test]
#[serial]
fn enable_checks_on_a_template_folder_inside_an_identity_checkout_goes_through_it() {
    let d = Deployment::new();
    write(&d.folder().join("document.html"), DOC);
    write(
        &d.folder().join("sub/.syns.yaml"),
        "holder: alice/docs\npath: q3-plan/sub\ntemplate:\n  repo: bartsoj/syns-whiteboard-template\n  version: 14\n  sha: t14\n  checks:\n  - make\n",
    );
    d.record_base(H4);
    d.identity_record(H4);
    d.serves(
        "PUT",
        &format!("{IDENTITY}/push"),
        ResponseTemplate::new(200).set_body_json(json!({
            "commitSha": H5, "version": 5, "filesChanged": 1, "created": false,
        })),
    );

    let out = d.run(&["--json", "enable-checks", "sub"]);

    assert_eq!(exit_of(&out), 0, "{}", stderr_of(&out));
    let pushes = d.pushes(IDENTITY);
    assert_eq!(pushes.len(), 1, "{:?}", d.targets());
    assert_eq!(pushed_paths(&pushes[0]), vec!["sub/.syns.yaml"]);
    assert!(d.pushes(HOLDER).is_empty());
}

// SPEC u302 Tests, the row of this name.
#[test]
#[serial]
fn share_inside_an_identity_folder_counts_from_the_holder() {
    let d = Deployment::new();
    std::fs::create_dir_all(d.folder().join("appendix")).unwrap();
    d.serves(
        "GET",
        HOLDER,
        ResponseTemplate::new(200).set_body_json(record("alice", "docs", Some(H8), false)),
    );
    d.serves(
        "GET",
        &format!("{HOLDER}/shares/q3-plan/appendix"),
        refusal(404, "not_found"),
    );

    let out = d.run(&["--json", "share", "appendix", "--show"]);

    assert_eq!(exit_of(&out), 0, "{}", stderr_of(&out));
    assert!(
        d.targets()
            .iter()
            .any(|t| t == &format!("GET {HOLDER}/shares/q3-plan/appendix")),
        "{:?}",
        d.targets()
    );
}

// ---- the paths the review found untested (CR1-1, CR1-2, CR1-4) ---------

/// The holder's answers once `q3-plan` was unshared: its file there
/// naming no identity, its tree at `q3-plan` at `H8` holding that file
/// and `document.html` at the base's hash, and the identity's version
/// list answering `404` `not_found`.
fn unshared(d: &Deployment) {
    d.overrides(
        "GET",
        &format!("{IDENTITY}/versions"),
        refusal(404, "not_found"),
    );
    d.serves(
        "GET",
        &format!("{HOLDER}/raw/q3-plan/.syns.yaml"),
        raw(UNSHARED),
    );
    d.serves(
        "GET",
        &format!("{HOLDER}/raw/q3-plan/document.html"),
        raw(DOC),
    );
    d.serves(
        "GET",
        &format!("{HOLDER}/tree/q3-plan"),
        ResponseTemplate::new(200).set_body_json(json!({
            "entries": [
                tree_entry("q3-plan/.syns.yaml", UNSHARED),
                tree_entry("q3-plan/document.html", DOC),
            ],
            "commitSha": H8, "truncated": false,
        })),
    );
}

// SPEC u302 Behaviour, `cmd_sync` 2 (CR1-1): a sync meeting an unshared
// identity returns the folder to its holder and converges it there.
#[test]
#[serial]
fn an_unshared_identity_returns_a_holder_reader_to_the_holder_on_sync() {
    let d = Deployment::new();
    write(&d.folder().join("document.html"), DOC);
    d.record_base(H4);
    unshared(&d);

    let out = d.run(&["--json", "sync"]);

    assert_eq!(exit_of(&out), 0, "{}{}", stderr_of(&out), stdout_of(&out));
    assert!(d.pushes(IDENTITY).is_empty() && d.pushes(HOLDER).is_empty());
    let holder = WorkingCopy::open_existing(&d.stores(), "alice", "docs", &d.folder())
        .unwrap()
        .expect("the holder's copy");
    assert_eq!(holder.base().unwrap().commit_sha(), Some(H8));
    let yaml = std::fs::read_to_string(d.folder().join(".syns.yaml")).unwrap();
    assert!(!yaml.contains("shared_as"), "{yaml}");
}

// SPEC u302 Behaviour, `cmd_pull` 1 at `--version` (CR1-4): the
// identity's whole tree is read at the version, every path as served.
#[test]
#[serial]
fn a_pull_at_a_version_inside_an_identity_folder_reads_its_whole_tree() {
    let d = Deployment::new();
    d.record_base(H4);

    let out = d.run(&["--json", "pull", "--version", "4"]);

    assert_eq!(exit_of(&out), 0, "{}{}", stderr_of(&out), stdout_of(&out));
    let trees: Vec<String> = d
        .targets()
        .into_iter()
        .filter(|t| t.contains("/tree"))
        .collect();
    assert_eq!(
        trees,
        vec![format!("GET {IDENTITY}/tree?recursive=true&ref=4")]
    );
    assert_eq!(
        std::fs::read_to_string(d.folder().join("document.html")).unwrap(),
        DOC
    );
}

const TEMPLATE: &str = "/api/v1/repos/bob/board-template";
const T9: &str = "9999999999999999999999999999999999999999";
const BOARD: &str = "<p>board</p>\n";

// SPEC u302 Behaviour, `cmd_place` 1 (CR1-2): a placement inside a folder
// bound to its identity is sent through the identity at the typed path,
// its identity file recording the holder and the holder path, and laid
// over the identity copy's base.
#[test]
#[serial]
fn place_inside_an_identity_folder_goes_through_it() {
    let d = Deployment::new();
    write(&d.folder().join("document.html"), DOC);
    d.record_base(H4);
    d.serves(
        "GET",
        TEMPLATE,
        ResponseTemplate::new(200).set_body_json(record("bob", "board-template", Some(T9), false)),
    );
    d.mount(
        Mock::given(method("GET"))
            .and(path(format!("{TEMPLATE}/versions")))
            .and(query_param("limit", "1"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(page(vec![version(9, T9, &["board.html"])], 1)),
            ),
    );
    d.serves(
        "GET",
        &format!("{TEMPLATE}/tree"),
        ResponseTemplate::new(200).set_body_json(json!({
            "entries": [tree_entry("board.html", BOARD)],
            "commitSha": T9, "truncated": false,
        })),
    );
    d.serves("GET", &format!("{TEMPLATE}/raw/board.html"), raw(BOARD));
    d.mount(
        Mock::given(method("GET"))
            .and(path_regex(
                r"^/api/v1/repos/alice/docs-q3-plan/(raw|tree)/appendix",
            ))
            .respond_with(refusal(404, "not_found")),
    );
    d.serves(
        "PUT",
        &format!("{IDENTITY}/push"),
        ResponseTemplate::new(200).set_body_json(json!({
            "commitSha": H5, "version": 5, "filesChanged": 2, "created": false,
        })),
    );

    let out = d.run(&["--json", "place", "bob/board-template", "appendix"]);

    assert_eq!(exit_of(&out), 0, "{}{}", stderr_of(&out), stdout_of(&out));
    let pushes = d.pushes(IDENTITY);
    assert_eq!(pushes.len(), 1, "{:?}", d.targets());
    assert_eq!(
        pushed_paths(&pushes[0]),
        vec!["appendix/.syns.yaml", "appendix/board.html"]
    );
    assert_eq!(pushes[0]["parentSha"], json!(H4));
    assert!(d.pushes(HOLDER).is_empty());
    let placed = std::fs::read_to_string(d.folder().join("appendix/.syns.yaml")).unwrap();
    assert!(
        placed.contains("holder: alice/docs") && placed.contains("path: q3-plan/appendix"),
        "{placed}"
    );
    let copy = WorkingCopy::open_existing(&d.stores(), "alice", "docs-q3-plan", &d.folder())
        .unwrap()
        .expect("the identity copy");
    let base = copy.base().unwrap();
    assert_eq!(base.commit_sha(), Some(H5));
    assert!(base.file_sha("appendix/.syns.yaml").is_some());
    assert!(base.file_sha("appendix/board.html").is_some());
    assert!(
        !d.folder().join("appendix").join(".syns-state").exists()
            && WorkingCopy::open_existing(
                &d.stores(),
                "alice",
                "docs",
                &d.folder().join("appendix")
            )
            .unwrap()
            .is_none(),
        "the placed folder recorded a base of its own"
    );
}
