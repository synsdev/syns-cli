//! Roadmap 292: binary-level behaviour of a `syns commit` changeset's
//! `edits` (steps/03-build.md Expected rows 1–8; D-257).
//!
//! The model server of roadmap 279 (`v2_fake`) answers the batched read
//! at the parent, the file read where v2 is off, and the push over one
//! model of commits, so each test reads what the push left. The row
//! carrying a move runs on a mock deployment, as roadmap 228's tests do,
//! since the model's push predates `moves`.

use std::io::Write as _;
use std::process::{Output, Stdio};

use serde_json::{Value, json};
use serial_test::serial;
use wiremock::matchers::{method, path as path_matcher, query_param};
use wiremock::{Mock, MockServer, ResponseTemplate};

use crate::integration::v2_fake::{Act, Env, REPO, blob};

const README_AT_PARENT: &str = "# Notes v0.3\n## Old\nbody\n";
const A_AT_PARENT: &str = "foo, then foo again\n";

/// `syns commit ARGS…` with `document` on standard input, from a
/// directory holding no checkout.
fn commit(env: &Env, document: &str, args: &[&str]) -> Output {
    let mut all = vec!["commit"];
    all.extend_from_slice(args);
    commit_with(env, document, &[], &all)
}

fn commit_json(env: &Env, document: &str, args: &[&str]) -> Output {
    let mut all = vec!["commit"];
    all.extend_from_slice(args);
    commit_with(env, document, &["--json"], &all)
}

fn commit_with(env: &Env, document: &str, global: &[&str], args: &[&str]) -> Output {
    let cwd = env.dir("work");
    let mut every: Vec<&str> = global.to_vec();
    every.extend_from_slice(args);
    let mut child = env
        .command(&cwd, &every)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("run syns");
    child
        .stdin
        .take()
        .expect("stdin")
        .write_all(document.as_bytes())
        .expect("write the document");
    child.wait_with_output().expect("syns output")
}

fn stderr_of(out: &Output) -> String {
    String::from_utf8_lossy(&out.stderr).to_string()
}

fn exit_of(out: &Output) -> i32 {
    out.status.code().expect("an exit code")
}

fn one_document(out: &Output) -> Value {
    serde_json::from_str(String::from_utf8_lossy(&out.stdout).trim())
        .expect("one document on the primary stream")
}

/// The model's commit at the parent: `README.md`, `docs/a.md`,
/// `2024.md` and `keep.md`. Answers the parent's hash.
fn seeded(env: &Env) -> String {
    env.fake.commit(&[
        ("README.md", README_AT_PARENT.as_bytes()),
        ("docs/a.md", A_AT_PARENT.as_bytes()),
        ("2024.md", b"line one\nline two\n"),
        ("keep.md", b"kept\n"),
    ])
}

fn text_at_head(env: &Env, path: &str) -> Option<String> {
    env.fake
        .head()
        .files
        .get(path)
        .map(|e| String::from_utf8(e.bytes.clone()).unwrap())
}

fn pushes(env: &Env) -> Vec<Value> {
    env.fake
        .log()
        .into_iter()
        .filter(|s| s.route == "v1 push")
        .map(|s| s.json())
        .collect()
}

/// Bart's sample: two edits to one path, one to another.
const SAMPLE: &str = r###"{ "edits": [
    { "path": "README.md", "old": "v0.3", "new": "v0.4" },
    { "path": "README.md", "old": "## Old", "new": "## New" },
    { "path": "docs/a.md", "old": "foo", "new": "bar", "replaceAll": true } ] }"###;

// ---- rows 1 and 7: several edits, one push of whole files -------------------

#[test]
#[serial]
fn row1_edits_land_as_one_push_of_whole_files_read_in_one_batch() {
    let env = Env::new();
    let parent = seeded(&env);
    env.fake.clear();

    let out = commit(&env, SAMPLE, &["--parent", &parent, "--repo", REPO]);

    assert_eq!(exit_of(&out), 0, "{}", stderr_of(&out));
    let head = env.fake.head();
    assert_eq!(
        stderr_of(&out),
        format!("wrote version 2, commit {}\n", head.sha)
    );
    assert_eq!(
        text_at_head(&env, "README.md").as_deref(),
        Some("# Notes v0.4\n## New\nbody\n")
    );
    assert_eq!(
        text_at_head(&env, "docs/a.md").as_deref(),
        Some("bar, then bar again\n")
    );
    assert_eq!(text_at_head(&env, "keep.md").as_deref(), Some("kept\n"));
    // One batched read at the parent naming each edited path once, no
    // file read alone, and one push carrying the two whole files.
    let reads = env.fake.reads();
    assert_eq!(reads.len(), 1, "{:?}", env.fake.routes());
    assert_eq!(reads[0].asked(), vec!["README.md", "docs/a.md"]);
    assert_eq!(reads[0].json()["at"], json!(parent));
    assert!(
        !env.fake.routes().iter().any(|r| r == "v1 files"),
        "{:?}",
        env.fake.routes()
    );
    let pushes = pushes(&env);
    assert_eq!(pushes.len(), 1);
    assert_eq!(
        pushes[0]["files"],
        json!([
            {"path": "README.md", "sha": blob(b"# Notes v0.4\n## New\nbody\n"), "content": "# Notes v0.4\n## New\nbody\n"},
            {"path": "docs/a.md", "sha": blob(b"bar, then bar again\n"), "content": "bar, then bar again\n"},
        ])
    );
    assert_eq!(pushes[0]["parentSha"], json!(parent));
    assert!(pushes[0].get("deletions").is_none(), "{}", pushes[0]);
}

#[test]
#[serial]
fn row1_where_the_server_answers_no_v2_each_path_is_read_alone() {
    let env = Env::new();
    seeded(&env);
    env.fake.v2(false);
    env.fake.clear();

    // `--parent` by its ordinal: each path read at that ordinal.
    let out = commit(&env, SAMPLE, &["--parent", "1", "--repo", REPO]);

    assert_eq!(exit_of(&out), 0, "{}", stderr_of(&out));
    assert_eq!(
        text_at_head(&env, "README.md").as_deref(),
        Some("# Notes v0.4\n## New\nbody\n")
    );
    assert_eq!(
        text_at_head(&env, "docs/a.md").as_deref(),
        Some("bar, then bar again\n")
    );
    let files: Vec<(String, Option<String>)> = env
        .fake
        .log()
        .into_iter()
        .filter(|s| s.route == "v1 files")
        .map(|s| (s.path, s.query.get("ref").cloned()))
        .collect();
    let mut files = files;
    files.sort();
    assert_eq!(
        files,
        vec![
            ("README.md".to_string(), Some("1".to_string())),
            ("docs/a.md".to_string(), Some("1".to_string())),
        ]
    );
    assert_eq!(pushes(&env).len(), 1);
}

// Review R1 m1: a path the batch answers `too_large` both times it is
// asked is read alone through the file read at the parent; the rest come
// from the batch, and both land in the one push.
#[test]
#[serial]
fn row1_a_path_the_batch_leaves_is_read_alone_at_the_parent() {
    let env = Env::new();
    seeded(&env);
    for _ in 0..2 {
        env.fake
            .act("v2 read", Act::TooLarge(vec!["README.md".to_string()]));
    }
    env.fake.clear();

    let out = commit(&env, SAMPLE, &["--parent", "1", "--repo", REPO]);

    assert_eq!(exit_of(&out), 0, "{}", stderr_of(&out));
    assert_eq!(
        text_at_head(&env, "README.md").as_deref(),
        Some("# Notes v0.4\n## New\nbody\n")
    );
    assert_eq!(
        text_at_head(&env, "docs/a.md").as_deref(),
        Some("bar, then bar again\n")
    );
    let reads = env.fake.reads();
    assert!(!reads.is_empty(), "{:?}", env.fake.routes());
    assert_eq!(reads[0].asked(), vec!["README.md", "docs/a.md"]);
    let alone: Vec<(String, Option<String>)> = env
        .fake
        .log()
        .into_iter()
        .filter(|s| s.route == "v1 files")
        .map(|s| (s.path, s.query.get("ref").cloned()))
        .collect();
    assert_eq!(
        alone,
        vec![("README.md".to_string(), Some("1".to_string()))]
    );
    assert_eq!(pushes(&env).len(), 1);
}

// Edge inputs: a name opening with a digit, an `old` holding a newline,
// an empty `new` (a cut), and dot segments resolved lexically.
#[test]
#[serial]
fn row1_edge_inputs_a_digit_name_a_newline_and_a_cut() {
    let env = Env::new();
    let parent = seeded(&env);

    let document = r#"{"edits":[
        {"path":"./docs/../2024.md","old":"line one\nline","new":"line"},
        {"path":"2024.md","old":" two","new":""}]}"#;
    let out = commit(&env, document, &["--parent", &parent, "--repo", REPO]);

    assert_eq!(exit_of(&out), 0, "{}", stderr_of(&out));
    assert_eq!(text_at_head(&env, "2024.md").as_deref(), Some("line\n"));
    let pushes = pushes(&env);
    assert_eq!(pushes[0]["files"].as_array().unwrap().len(), 1);
    assert_eq!(pushes[0]["files"][0]["path"], json!("2024.md"));
}

#[test]
#[serial]
fn row7_an_unknown_key_in_an_edit_refuses_the_document_and_no_edit_is_empty() {
    let env = Env::new();
    let parent = seeded(&env);
    env.fake.clear();

    let unknown = commit_json(
        &env,
        r#"{"edits":[{"path":"README.md","old":"v0.3","new":"v0.4","replace_all":true}]}"#,
        &["--parent", &parent, "--repo", REPO],
    );
    assert_eq!(exit_of(&unknown), 1, "{}", stderr_of(&unknown));
    let document = one_document(&unknown);
    assert_eq!(document["code"], json!("invalid_changeset"));
    let line = document["error"].as_str().unwrap();
    assert!(line.contains("unknown field `replace_all`"), "{line}");
    assert!(line.contains("`replaceAll`"), "{line}");

    let empty = commit(
        &env,
        r#"{"edits":[]}"#,
        &["--parent", &parent, "--repo", REPO],
    );
    assert_eq!(exit_of(&empty), 6, "{}", stderr_of(&empty));
    assert!(stderr_of(&empty).starts_with("error: push_empty:"));

    assert!(env.fake.log().is_empty(), "{:?}", env.fake.routes());
}

// ---- row 2: a careless anchor publishes nothing -------------------------------

#[test]
#[serial]
fn row2_zero_or_several_matches_refuse_naming_the_index_and_publish_nothing() {
    let env = Env::new();
    env.fake.commit(&[
        ("README.md", b"v0.1 v0.2 v0.3\n## New\n"),
        ("docs/a.md", b"bar\n"),
    ]);
    let parent = env.fake.head().sha;

    let twice = r###"{ "edits": [ { "path": "README.md", "old": "## New", "new": "## Newer" },
                                  { "path": "README.md", "old": "v0",     "new": "v1" } ] }"###;
    let out = commit(&env, twice, &["--parent", "1", "--repo", REPO]);
    assert_eq!(exit_of(&out), 1);
    assert_eq!(
        stderr_of(&out),
        "error: configuration error: edit 1 (README.md): old matched 3 times; pass replaceAll to replace every occurrence\n"
    );
    let json_out = commit_json(&env, twice, &["--parent", "1", "--repo", REPO]);
    assert_eq!(exit_of(&json_out), 1);
    let document = one_document(&json_out);
    assert_eq!(document["code"], json!("many_matches"));
    assert_eq!(document["count"], json!(3));

    let nowhere = r#"{ "edits": [ { "path": "docs/a.md", "old": "foo", "new": "baz" } ] }"#;
    let out = commit(&env, nowhere, &["--parent", "1", "--repo", REPO]);
    assert_eq!(exit_of(&out), 1);
    assert_eq!(
        stderr_of(&out),
        "error: configuration error: edit 0 (docs/a.md): old matched no content at version 1\n"
    );
    let json_out = commit_json(&env, nowhere, &["--parent", "1", "--repo", REPO]);
    assert_eq!(one_document(&json_out)["code"], json!("no_match"));

    assert!(pushes(&env).is_empty());
    assert_eq!(env.fake.head().sha, parent);

    // `replaceAll` takes every occurrence.
    let all =
        r#"{ "edits": [ { "path": "README.md", "old": "v0", "new": "v1", "replaceAll": true } ] }"#;
    let out = commit(&env, all, &["--parent", "1", "--repo", REPO]);
    assert_eq!(exit_of(&out), 0, "{}", stderr_of(&out));
    assert_eq!(
        text_at_head(&env, "README.md").as_deref(),
        Some("v1.1 v1.2 v1.3\n## New\n")
    );
}

// ---- row 3: the document's own shape, before any request ----------------------

#[test]
#[serial]
fn row3_an_empty_old_or_an_old_equal_to_new_is_refused_before_any_request() {
    let env = Env::new();
    let parent = seeded(&env);
    env.fake.clear();

    for (document, line) in [
        (
            r#"{"edits":[{"path":"README.md","old":"v0.3","new":"v0.4"},{"path":"docs/a.md","old":"","new":"x"}]}"#,
            "configuration error: edit 1 (docs/a.md): old cannot be empty",
        ),
        (
            r#"{"edits":[{"path":"README.md","old":"v0.3","new":"v0.3"}]}"#,
            "configuration error: edit 0 (README.md): old and new are the same value",
        ),
        (
            r#"{"edits":[{"path":"..","old":"a","new":"b"}]}"#,
            "configuration error: edit 0 (..): .. names no path inside the repository; name one counted from its root",
        ),
        (
            r#"{"edits":[{"path":"","old":"a","new":"b"}]}"#,
            "configuration error: edit 0 (): path cannot be empty",
        ),
    ] {
        let out = commit_json(&env, document, &["--parent", &parent, "--repo", REPO]);
        assert_eq!(exit_of(&out), 1, "{document}");
        assert_eq!(
            one_document(&out),
            json!({"error": line, "code": "invalid_changeset"}),
            "{document}"
        );
    }
    assert!(env.fake.log().is_empty(), "{:?}", env.fake.routes());
}

// ---- row 4: beside files, a deletion and a move, one push -----------------------

#[test]
#[serial]
fn row4_edits_beside_a_file_and_a_deletion_land_in_one_version() {
    let env = Env::new();
    let parent = seeded(&env);
    env.fake.clear();

    let document = json!({
        "files": [{"path": "new.md", "content": "new\n"}],
        "deletions": [{"path": "keep.md"}],
        "edits": [
            {"path": "README.md", "old": "v0.3", "new": "v0.4"},
            {"path": "docs/a.md", "old": "foo", "new": "bar", "replaceAll": true},
            {"path": "2024.md", "old": "one", "new": "1"},
        ],
    })
    .to_string();
    let out = commit(&env, &document, &["--parent", &parent, "--repo", REPO]);

    assert_eq!(exit_of(&out), 0, "{}", stderr_of(&out));
    let pushes = pushes(&env);
    assert_eq!(pushes.len(), 1);
    let paths: Vec<&str> = pushes[0]["files"]
        .as_array()
        .unwrap()
        .iter()
        .map(|f| f["path"].as_str().unwrap())
        .collect();
    assert_eq!(paths, vec!["2024.md", "README.md", "docs/a.md", "new.md"]);
    assert_eq!(pushes[0]["deletions"], json!([{"path": "keep.md"}]));
    assert_eq!(env.fake.with(|s| s.commits.len()), 2);
    assert_eq!(text_at_head(&env, "new.md").as_deref(), Some("new\n"));
    assert_eq!(text_at_head(&env, "keep.md"), None);
    assert_eq!(
        text_at_head(&env, "2024.md").as_deref(),
        Some("line 1\nline two\n")
    );
}

const HEAD_SHA: &str = "aa11bb22cc33dd44ee55ff6600778899001122bb";
const NEXT_SHA: &str = "dd44ee55ff6600778899001122bbaa11bb22cc33";

/// A mock deployment at `HEAD_SHA` answering the file read of each of
/// `files` at that parent and the push with `answer`; no `POST …/read`
/// is mounted, so the mock answers it `404`, as a server answering no v2
/// does, and each path is read alone. Runs `syns commit` with `document`
/// and answers the run and each push body.
fn commit_on_mock(document: &str, files: &[(&str, &str)], answer: Value) -> (Output, Vec<Value>) {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let server = rt.block_on(MockServer::start());
    let mount = |mock: Mock| rt.block_on(async { mock.mount(&server).await });
    mount(
        Mock::given(method("GET"))
            .and(path_matcher(format!("/api/v1/repos/{REPO}")))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "owner": "alice", "name": "notes", "description": null,
                "commitSha": HEAD_SHA, "status": "active", "author": null, "tags": [],
                "visibility": "public", "forkedFrom": null, "forkCount": 0,
                "fileCount": 3, "role": null,
                "createdAt": "2026-01-01T00:00:00Z", "updatedAt": "2026-01-01T00:00:00Z",
            }))),
    );
    for (path, content) in files {
        mount(
            Mock::given(method("GET"))
                .and(path_matcher(format!("/api/v1/repos/{REPO}/files/{path}")))
                .and(query_param("ref", HEAD_SHA))
                .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                    "path": path, "sha": blob(content.as_bytes()),
                    "size": content.len(), "content": content,
                }))),
        );
    }
    mount(
        Mock::given(method("PUT"))
            .and(path_matcher(format!("/api/v1/repos/{REPO}/push")))
            .respond_with(ResponseTemplate::new(200).set_body_json(answer)),
    );
    let env = Env::new();
    let cwd = env.dir("work");
    let mut child = std::process::Command::new(assert_cmd::cargo::cargo_bin("syns"))
        .current_dir(&cwd)
        .env("SYNS_CONFIG_DIR", env.config.path())
        .env("SYNS_CACHE_DIR", env.cache.path())
        .env_remove("SYNS_URL")
        .env_remove("SYNS_INTEGRATION")
        .env_remove("SYNS_RUN")
        .env_remove("SYNS_TRIGGER")
        .env_remove("SYNS_TASK")
        .args(["--server", &server.uri()])
        .args([
            "commit",
            "--parent",
            HEAD_SHA,
            "--repo",
            REPO,
            "-m",
            "Retitle: Q3 retro",
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(document.as_bytes())
        .unwrap();
    let out = child.wait_with_output().unwrap();
    let requests = rt.block_on(async { server.received_requests().await.unwrap() });
    let pushes = requests
        .iter()
        .filter(|r| r.url.path().ends_with("/push"))
        .map(|r| serde_json::from_slice(&r.body).unwrap())
        .collect();
    (out, pushes)
}

#[test]
#[serial]
fn row4_an_edit_under_a_moved_source_lands_at_the_moved_path_in_one_push() {
    let document = json!({
        "moves": [{"from": "q3-review", "to": "q3-retro"}],
        "files": [{"path": "notes.md", "content": "n\n"}],
        "deletions": [{"path": "old.md"}],
        "edits": [{"path": "q3-review/document.html", "old": "<h1>Q3 review</h1>", "new": "<h1>Q3 retro</h1>"}],
    })
    .to_string();
    let (out, pushes) = commit_on_mock(
        &document,
        &[("q3-review/document.html", "<h1>Q3 review</h1>\n")],
        json!({
            "commitSha": NEXT_SHA, "version": 43, "filesChanged": 4, "created": false,
            "moves": [{"from": "q3-review", "to": "q3-retro", "type": "dir", "files": 3}],
        }),
    );

    assert_eq!(exit_of(&out), 0, "{}", stderr_of(&out));
    assert_eq!(
        stderr_of(&out),
        format!(
            "wrote version 43, commit {NEXT_SHA}\nmoved 3 files: q3-review/ \u{2192} q3-retro/\n"
        )
    );
    assert_eq!(pushes.len(), 1);
    assert_eq!(
        pushes[0],
        json!({
            "files": [
                {"path": "notes.md", "sha": blob(b"n\n"), "content": "n\n"},
                {"path": "q3-retro/document.html", "sha": blob(b"<h1>Q3 retro</h1>\n"), "content": "<h1>Q3 retro</h1>\n"},
            ],
            "deletions": [{"path": "old.md"}],
            "moves": [{"from": "q3-review", "to": "q3-retro"}],
            "message": "Retitle: Q3 retro",
            "parentSha": HEAD_SHA,
        })
    );
}

// Review R1 I1 (D-253 §1, D-257 §5 as patched): a deletion at or above a
// move's destination removes what stood there before the move lands, so
// an edit under the move's source is judged against deletions where it
// reads and lands, edited, at the moved path — as a `files` entry there
// does.
#[test]
#[serial]
fn row4_an_edit_moved_into_a_deleted_destination_pushes_the_edited_text_there() {
    let document = json!({
        "moves": [{"from": "q", "to": "docs"}],
        "deletions": [{"path": "docs"}],
        "edits": [{"path": "q/a.md", "old": "draft", "new": "final"}],
    })
    .to_string();
    let (out, pushes) = commit_on_mock(
        &document,
        &[("q/a.md", "a draft\n")],
        json!({
            "commitSha": NEXT_SHA, "version": 43, "filesChanged": 2, "created": false,
            "moves": [{"from": "q", "to": "docs", "type": "dir", "files": 1}],
        }),
    );

    assert_eq!(exit_of(&out), 0, "{}", stderr_of(&out));
    assert_eq!(pushes.len(), 1);
    assert_eq!(
        pushes[0],
        json!({
            "files": [{"path": "docs/a.md", "sha": blob(b"a final\n"), "content": "a final\n"}],
            "deletions": [{"path": "docs"}],
            "moves": [{"from": "q", "to": "docs"}],
            "message": "Retitle: Q3 retro",
            "parentSha": HEAD_SHA,
        })
    );
}

// ---- row 5: overlaps refused before any request ---------------------------------

#[test]
#[serial]
fn row5_an_edit_overlapping_another_member_is_refused_before_any_request() {
    let env = Env::new();
    let parent = seeded(&env);
    env.fake.clear();

    for (document, opening) in [
        (
            json!({"files": [{"path": "README.md", "content": "x"}],
                   "edits": [{"path": "README.md", "old": "v0.3", "new": "v0.4"}]}),
            "edit 0 (README.md): README.md is also named under files in one changeset",
        ),
        (
            json!({"deletions": [{"path": "docs"}],
                   "edits": [{"path": "docs/a.md", "old": "foo", "new": "bar"}]}),
            "edit 0 (docs/a.md): docs/a.md overlaps docs in one changeset; an edit cannot be at, under or above a deletion",
        ),
        (
            json!({"copies": [{"from": "docs", "to": "docs2"}],
                   "edits": [{"path": "docs/a.md", "old": "foo", "new": "bar"}]}),
            "edit 0 (docs/a.md): docs/a.md overlaps docs in one changeset; an edit cannot be at, under or above the source of a copy",
        ),
        (
            json!({"moves": [{"from": "keep.md", "to": "docs/b.md"}],
                   "edits": [{"path": "README.md", "old": "v0.3", "new": "v0.4"},
                             {"path": "docs", "old": "foo", "new": "bar"}]}),
            "edit 1 (docs): docs overlaps docs/b.md in one changeset; an edit cannot be at, under or above the destination of a move or a copy",
        ),
    ] {
        let document = document.to_string();
        let out = commit_json(&env, &document, &["--parent", &parent, "--repo", REPO]);
        assert_eq!(exit_of(&out), 1, "{document}");
        let refusal = one_document(&out);
        assert_eq!(refusal["code"], json!("invalid_changeset"), "{document}");
        let line = refusal["error"].as_str().unwrap();
        assert!(
            line.starts_with(&format!("configuration error: {opening}")),
            "{document}: {line}"
        );
    }
    assert!(env.fake.log().is_empty(), "{:?}", env.fake.routes());
}

// ---- row 6: a path missing, a content that is not text --------------------------

#[test]
#[serial]
fn row6_a_missing_path_or_a_content_not_text_is_refused_before_the_push() {
    for v2 in [true, false] {
        let env = Env::new();
        env.fake.commit(&[
            ("README.md", README_AT_PARENT.as_bytes()),
            ("photo.bin", &[0xff, 0xfe, b'x', b'\n']),
        ]);
        if !v2 {
            env.fake.v2(false);
        }

        let missing = r#"{"edits":[{"path":"README.md","old":"v0.3","new":"v0.4"},
                                   {"path":"gone.md","old":"a","new":"b"}]}"#;
        let out = commit(&env, missing, &["--parent", "1", "--repo", REPO]);
        assert_eq!(exit_of(&out), 1, "v2 {v2}: {}", stderr_of(&out));
        assert_eq!(
            stderr_of(&out),
            "error: edit 1 (gone.md): path not found at version 1\n",
            "v2 {v2}"
        );
        let json_out = commit_json(&env, missing, &["--parent", "1", "--repo", REPO]);
        assert_eq!(
            one_document(&json_out)["code"],
            json!("not_found"),
            "v2 {v2}"
        );

        let bytes = r#"{"edits":[{"path":"photo.bin","old":"x","new":"y"}]}"#;
        let out = commit_json(&env, bytes, &["--parent", "1", "--repo", REPO]);
        assert_eq!(exit_of(&out), 1, "v2 {v2}");
        let refusal = one_document(&out);
        assert_eq!(refusal["code"], json!("content_not_text"), "v2 {v2}");
        assert!(
            refusal["error"]
                .as_str()
                .unwrap()
                .starts_with("edit 0 (photo.bin): cannot write content that is not text"),
            "v2 {v2}: {refusal}"
        );

        assert!(pushes(&env).is_empty(), "v2 {v2}");
        assert_eq!(env.fake.with(|s| s.commits.len()), 1, "v2 {v2}");
    }
}

// ---- row 8: a stale parent ----------------------------------------------------

#[test]
#[serial]
fn row8_a_stale_parent_is_the_conflict_at_exit_seven() {
    let env = Env::new();
    let parent = seeded(&env);
    let moved = env.fake.change(&[("keep.md", Some(b"moved on\n"))]);

    let out = commit(&env, SAMPLE, &["--parent", &parent, "--repo", REPO]);
    assert_eq!(exit_of(&out), 7, "{}", stderr_of(&out));
    assert_eq!(
        stderr_of(&out),
        format!("error: conflict: the repository moved past {parent}; its head is now {moved}\n")
    );
    let json_out = commit_json(&env, SAMPLE, &["--parent", &parent, "--repo", REPO]);
    assert_eq!(exit_of(&json_out), 7);
    let document = one_document(&json_out);
    assert_eq!(document["code"], json!("write_conflict"));
    assert_eq!(document["currentSha"], json!(moved));
    assert_eq!(env.fake.head().sha, moved);
}
