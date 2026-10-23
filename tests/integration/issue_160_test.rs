//! Binary-level rows of issue 160 (Q-0171, D-250): `syns diff` and
//! `syns revert` reach a repository named through `--repo` from a
//! directory holding no checkout, `syns revert` takes the writing verbs'
//! options and a required `--parent`, `syns rm` of a path the parent does
//! not hold is refused under `not_found`, every `--json` refusal carries a
//! `code` beside `error`, and the three not-found causes of the read
//! verbs answer three documents of their own in both output modes.
//!
//! Each test runs the built binary from a working directory of its own
//! holding nothing, against one mock deployment: `alice/notes` stands at
//! version 7 and holds version 3, and every request naming `alice/gone`
//! answers the server's `404` for a repository that stands nowhere or
//! that the caller may not read.

use assert_cmd::Command as AssertCommand;
use serde_json::{Value, json};
use serial_test::serial;
use syns_cli::auth::token::TokenStore;
use tempfile::TempDir;
use wiremock::matchers::{method, path as path_matcher, path_regex, query_param};
use wiremock::{Mock, MockServer, Request, ResponseTemplate};

const HEAD_SHA: &str = "aa11bb22cc33dd44ee55ff6600778899001122bb";
const OLD_SHA: &str = "bb00bb00bb00bb00bb00bb00bb00bb00bb00bb00";
const MOVED_SHA: &str = "cc00cc00cc00cc00cc00cc00cc00cc00cc00cc00";

struct Deployment {
    rt: tokio::runtime::Runtime,
    server: MockServer,
    home: TempDir,
    cache: TempDir,
    work: TempDir,
}

impl Deployment {
    /// The deployment, with a credential stored for `alice` where
    /// `signed_in`.
    fn new(signed_in: bool) -> Deployment {
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
        if signed_in {
            unsafe { std::env::set_var("SYNS_CONFIG_DIR", d.home.path()) };
            let config = syns_cli::config::Config::new(Some(&d.server.uri())).unwrap();
            TokenStore::new(config.credentials_path())
                .write_with_username("test-token", Some("alice"))
                .unwrap();
            unsafe { std::env::remove_var("SYNS_CONFIG_DIR") };
        }
        d.mount_standing();
        d
    }

    fn mount(&self, mock: Mock) {
        self.rt.block_on(async { mock.mount(&self.server).await });
    }

    fn requests(&self) -> Vec<Request> {
        self.rt
            .block_on(async { self.server.received_requests().await.expect("requests") })
            .into_iter()
            .filter(|r| !r.url.path().starts_with("/api/v2/"))
            .collect()
    }

    fn run(&self, args: &[&str], stdin: Option<&str>) -> std::process::Output {
        let mut command = AssertCommand::cargo_bin("syns").expect("syns binary");
        command
            .current_dir(self.work.path())
            .env("SYNS_CONFIG_DIR", self.home.path())
            .env("SYNS_CACHE_DIR", self.cache.path())
            .env_remove("SYNS_URL")
            .env_remove("SYNS_INTEGRATION")
            .env_remove("SYNS_RUN")
            .env_remove("SYNS_TRIGGER")
            .env_remove("SYNS_TASK");
        if let Some(input) = stdin {
            command.write_stdin(input.to_string());
        }
        command
            .arg("--server")
            .arg(self.server.uri())
            .args(args)
            .output()
            .expect("run syns")
    }

    /// `alice/notes` at version 7 holding version 3; every path named
    /// `missing…` absent from its trees; `alice/gone` answered as a
    /// repository that stands nowhere.
    fn mount_standing(&self) {
        let not_found = |message: &str| {
            ResponseTemplate::new(404)
                .set_body_json(json!({"error": "not_found", "message": message}))
        };
        self.mount(
            Mock::given(method("GET"))
                .and(path_matcher("/api/v1/repos/alice/notes"))
                .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                    "owner": "alice", "name": "notes", "description": null,
                    "commitSha": HEAD_SHA, "status": "active", "author": null, "tags": [],
                    "visibility": "public", "forkedFrom": null, "forkCount": 0,
                    "fileCount": 3, "role": null,
                    "createdAt": "2026-01-01T00:00:00Z", "updatedAt": "2026-01-01T00:00:00Z",
                }))),
        );
        for (reference, version, sha) in [
            (HEAD_SHA, 7, HEAD_SHA),
            ("7", 7, HEAD_SHA),
            ("3", 3, OLD_SHA),
        ] {
            self.mount(
                Mock::given(method("GET"))
                    .and(path_matcher(format!(
                        "/api/v1/repos/alice/notes/versions/{reference}"
                    )))
                    .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                        "version": version, "sha": sha, "parentSha": null, "message": "m",
                        "messageBody": null, "author": "alice",
                        "createdAt": "2026-01-01T00:00:00Z", "filesChanged": ["a.md"],
                    }))),
            );
        }
        self.mount(
            Mock::given(method("GET"))
                .and(path_matcher("/api/v1/repos/alice/notes/versions/9999"))
                .respond_with(not_found("Version not found")),
        );
        self.mount(
            Mock::given(method("GET"))
                .and(path_regex(
                    r"^/api/v1/repos/alice/notes/(files|raw|tree)/missing",
                ))
                .respond_with(not_found("File not found")),
        );
        self.mount(
            Mock::given(method("GET"))
                .and(path_matcher("/api/v1/repos/alice/notes/diff"))
                .and(query_param("to", "9999"))
                .respond_with(not_found("Version not found")),
        );
        self.mount(
            Mock::given(path_regex(r"^/api/v1/repos/alice/gone(/.*)?$"))
                .respond_with(not_found("Repository not found")),
        );
    }
}

fn document(output: &std::process::Output) -> Value {
    serde_json::from_slice(&output.stdout).unwrap_or_else(|e| {
        panic!(
            "one document on stdout ({e}); exit {:?}\nstdout: {}\nstderr: {}",
            output.status.code(),
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        )
    })
}

fn refusal(code: &str, line: &str) -> Value {
    json!({"code": code, "error": line})
}

/// The read invocations of `Boundary B`: a verb and how it names the
/// missing path, the line its no-`--version` refusal registers.
fn read_runs() -> Vec<(Vec<&'static str>, &'static str)> {
    vec![
        (vec!["cat", "missing.md"], "file not found: missing.md"),
        (vec!["read", "missing.md"], "file not found: missing.md"),
        (vec!["ls", "missing-dir"], "path not found: missing-dir"),
        (
            vec!["glob", "*", "--path", "missing-dir"],
            "path not found: missing-dir",
        ),
        (
            vec!["grep", "x", "--path", "missing-dir"],
            "path not found: missing-dir",
        ),
    ]
}

fn named(
    verb: &[&'static str],
    repo: &'static str,
    version: Option<&'static str>,
) -> Vec<&'static str> {
    let mut args = verb.to_vec();
    args.extend(["--repo", repo]);
    if let Some(version) = version {
        args.extend(["--version", version]);
    }
    args
}

// Issue 160 steps 2–5 (`Boundary B`): a missing path, an unknown version
// and an unreachable repository each answer a document of their own under
// `--json`, the path refusal naming the path the run asked for.
#[test]
#[serial]
fn the_three_not_found_causes_answer_documents_of_their_own_under_json() {
    let d = Deployment::new(true);
    for (verb, path_line) in read_runs() {
        let path_versioned = path_line
            .replace("file not found: ", "path not found at version 3: ")
            .replace("path not found: ", "path not found at version 3: ");
        let cases: Vec<(Vec<&str>, Value)> = vec![
            (
                named(&verb, "alice/notes", None),
                refusal("not_found", path_line),
            ),
            (
                named(&verb, "alice/notes", Some("3")),
                refusal("not_found", &path_versioned),
            ),
            (
                named(&verb, "alice/notes", Some("9999")),
                refusal("version_not_found", "version not found: 9999"),
            ),
            (
                named(&verb, "alice/gone", None),
                refusal("repo_not_found", "repository not found: alice/gone"),
            ),
            (
                named(&verb, "alice/gone", Some("3")),
                refusal("repo_not_found", "repository not found: alice/gone"),
            ),
        ];
        for (mut args, expected) in cases {
            args.push("--json");
            let out = d.run(&args, None);
            assert_eq!(out.status.code(), Some(1), "{args:?}");
            assert_eq!(document(&out), expected, "{args:?}");
        }
    }
}

// The same rows outside `--json`: the one line on the diagnostic stream,
// the same in both modes.
#[test]
#[serial]
fn the_three_not_found_causes_answer_lines_of_their_own_in_human_mode() {
    let d = Deployment::new(true);
    for (verb, path_line) in read_runs() {
        for (args, line) in [
            (named(&verb, "alice/notes", None), path_line.to_string()),
            (
                named(&verb, "alice/notes", Some("9999")),
                "version not found: 9999".to_string(),
            ),
            (
                named(&verb, "alice/gone", None),
                "repository not found: alice/gone".to_string(),
            ),
        ] {
            let out = d.run(&args, None);
            assert_eq!(out.status.code(), Some(1), "{args:?}");
            assert!(out.stdout.is_empty(), "{args:?}");
            assert_eq!(
                String::from_utf8_lossy(&out.stderr),
                format!("error: {line}\n"),
                "{args:?}"
            );
        }
    }
}

// Issue 160 steps 10 and 11: `syns diff --repo` from a directory holding
// no checkout answers `EP-diff`, and its version and repository refusals
// read apart.
#[test]
#[serial]
fn diff_takes_repo_from_a_directory_holding_no_checkout() {
    let d = Deployment::new(true);
    let served = json!({
        "from": {"version": 3, "sha": OLD_SHA},
        "to": {"version": 7, "sha": HEAD_SHA},
        "files": [{"path": "a.md", "status": "modified", "diff": "@@ -1 +1 @@\n-a\n+b\n"}],
    });
    d.mount(
        Mock::given(method("GET"))
            .and(path_matcher("/api/v1/repos/alice/notes/diff"))
            .and(query_param("from", "3"))
            .and(query_param("to", "7"))
            .respond_with(ResponseTemplate::new(200).set_body_json(served.clone())),
    );

    let out = d.run(
        &[
            "diff",
            "--from",
            "3",
            "--to",
            "7",
            "--repo",
            "alice/notes",
            "--json",
        ],
        None,
    );
    assert_eq!(
        out.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(document(&out), served);

    let out = d.run(
        &[
            "diff",
            "--from",
            "3",
            "--to",
            "9999",
            "--repo",
            "alice/notes",
            "--json",
        ],
        None,
    );
    assert_eq!(out.status.code(), Some(1));
    assert_eq!(
        document(&out),
        refusal("version_not_found", "version not found: 3 or 9999")
    );

    let out = d.run(
        &[
            "diff",
            "--from",
            "3",
            "--to",
            "7",
            "--repo",
            "alice/gone",
            "--json",
        ],
        None,
    );
    assert_eq!(out.status.code(), Some(1));
    assert_eq!(
        document(&out),
        refusal("repo_not_found", "repository not found: alice/gone")
    );

    // With neither `--repo` nor a checkout, the refusal names `--repo`.
    let out = d.run(&["diff", "--from", "3", "--to", "7", "--json"], None);
    assert_eq!(out.status.code(), Some(2));
    assert_eq!(document(&out)["code"], json!("repo_identity_unknown"));
}

// Issue 160 step 12, Q-0171: `syns revert` takes `--repo` and requires
// `--parent`, refused by the parser where it is absent.
#[test]
#[serial]
fn revert_requires_parent_and_takes_repo_and_the_provenance_options() {
    let d = Deployment::new(true);
    let out = d.run(
        &["revert", "x.md", "--to", "1", "--repo", "alice/notes"],
        None,
    );
    assert_eq!(out.status.code(), Some(2));
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("--parent <REF>"),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(d.requests().is_empty());

    d.mount(
        Mock::given(method("POST"))
            .and(path_matcher("/api/v1/repos/alice/notes/files/x.md/revert"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "commitSha": "d".repeat(40), "version": 8, "filesChanged": 1, "created": false,
            }))),
    );
    let out = d.run(
        &[
            "revert",
            "x.md",
            "--to",
            "3",
            "--parent",
            "7",
            "--repo",
            "alice/notes",
            "--integration",
            "syns-bb-plugin",
            "--run",
            "r-1",
            "--trigger",
            "thread-page",
            "--json",
        ],
        None,
    );
    assert_eq!(
        out.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let revert = d
        .requests()
        .into_iter()
        .find(|r| r.url.path().ends_with("/revert"))
        .expect("one revert");
    let body: Value = revert.body_json().unwrap();
    assert_eq!(
        body,
        json!({
            "to": "3",
            "parentSha": HEAD_SHA,
            "provenance": {"integration": "syns-bb-plugin", "run": "r-1", "trigger": "thread-page"},
        })
    );
}

// Issue 160: a moved head answers the conflict refusal at exit `7`, its
// document carrying `currentSha` and `code`.
#[test]
#[serial]
fn revert_on_a_moved_head_exits_seven_naming_the_head() {
    let d = Deployment::new(true);
    d.mount(
        Mock::given(method("POST"))
            .and(path_matcher("/api/v1/repos/alice/notes/files/x.md/revert"))
            .respond_with(ResponseTemplate::new(409).set_body_json(json!({
                "error": "conflict", "message": "Head mismatch", "currentSha": MOVED_SHA,
            }))),
    );
    let out = d.run(
        &[
            "revert",
            "x.md",
            "--to",
            "3",
            "--parent",
            HEAD_SHA,
            "--repo",
            "alice/notes",
            "--json",
        ],
        None,
    );
    assert_eq!(out.status.code(), Some(7));
    let doc = document(&out);
    assert_eq!(doc["code"], json!("write_conflict"));
    assert_eq!(doc["currentSha"], json!(MOVED_SHA));
}

// Issue 160 step 8: inside a checkout holding unpublished work the revert
// is refused by the checkout guard, before any request.
#[test]
#[serial]
fn revert_inside_a_checkout_holding_unpublished_work_is_refused() {
    let d = Deployment::new(true);
    std::fs::write(
        d.work.path().join(".syns.yaml"),
        "owner: alice\nname: notes\n",
    )
    .unwrap();
    std::fs::write(d.work.path().join("draft.md"), "unpublished\n").unwrap();
    let out = d.run(
        &["revert", "x.md", "--to", "3", "--parent", "7", "--json"],
        None,
    );
    assert_eq!(out.status.code(), Some(1));
    let doc = document(&out);
    assert_eq!(doc["code"], json!("checkout_dirty"));
    assert!(
        doc["error"]
            .as_str()
            .unwrap()
            .contains("holds unpublished local changes for alice/notes")
    );
    assert!(d.requests().is_empty());
}

// Issue 160 step 1: `syns rm` of a path the parent's tree does not hold is
// refused under `not_found` at exit `1`, naming the path.
#[test]
#[serial]
fn rm_of_an_absent_path_is_refused_as_not_found() {
    let d = Deployment::new(true);
    d.mount(
        Mock::given(method("PUT"))
            .and(path_matcher("/api/v1/repos/alice/notes/push"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "commitSha": HEAD_SHA, "version": 7, "filesChanged": 0, "created": false,
            }))),
    );
    let out = d.run(
        &[
            "rm",
            "no-such-file-160.md",
            "--parent",
            "7",
            "--repo",
            "alice/notes",
            "--json",
        ],
        None,
    );
    assert_eq!(out.status.code(), Some(1));
    assert_eq!(
        document(&out),
        refusal(
            "not_found",
            "path not found at version 7: no-such-file-160.md"
        )
    );
    let out = d.run(
        &[
            "rm",
            "no-such-file-160.md",
            "--parent",
            "7",
            "--repo",
            "alice/notes",
        ],
        None,
    );
    assert_eq!(out.status.code(), Some(1));
    assert_eq!(
        String::from_utf8_lossy(&out.stderr),
        "error: path not found at version 7: no-such-file-160.md\n"
    );
}

// Issue 160: the refusals the issue lists, each carrying its code beside
// `error` and its line unchanged.
#[test]
#[serial]
fn each_listed_refusal_carries_its_code() {
    let signed_out = Deployment::new(false);
    let out = signed_out.run(
        &[
            "rm",
            "a.md",
            "--parent",
            "7",
            "--repo",
            "alice/notes",
            "--json",
        ],
        None,
    );
    assert_eq!(out.status.code(), Some(1));
    assert_eq!(
        document(&out),
        refusal(
            "auth_required",
            "authentication required \u{2014} run 'syns login' first"
        )
    );

    let d = Deployment::new(true);
    let out = d.run(&["grep", "(", "--repo", "alice/notes", "--json"], None);
    assert_eq!(out.status.code(), Some(1));
    let doc = document(&out);
    assert_eq!(doc["code"], json!("bad_pattern"));
    assert!(
        doc["error"]
            .as_str()
            .unwrap()
            .starts_with("configuration error: invalid pattern (")
    );

    let out = d.run(
        &["commit", "--parent", "7", "--repo", "alice/notes", "--json"],
        Some("not a changeset"),
    );
    assert_eq!(out.status.code(), Some(1));
    let doc = document(&out);
    assert_eq!(doc["code"], json!("invalid_changeset"));
    assert!(
        doc["error"]
            .as_str()
            .unwrap()
            .starts_with("configuration error: the changeset document does not parse")
    );

    d.mount(
        Mock::given(method("GET"))
            .and(path_matcher("/api/v1/repos/alice/notes/files/a.md"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "path": "a.md", "sha": "e".repeat(40), "content": "one two one two one\n", "size": 20,
            }))),
    );
    let edit = |old: &'static str| {
        d.run(
            &[
                "edit",
                "a.md",
                "--old",
                old,
                "--new",
                "three",
                "--parent",
                "7",
                "--repo",
                "alice/notes",
                "--json",
            ],
            None,
        )
    };
    let out = edit("four");
    assert_eq!(out.status.code(), Some(1));
    assert_eq!(
        document(&out),
        refusal(
            "no_match",
            "configuration error: --old matched no content at version 7: a.md"
        )
    );
    let out = edit("one");
    assert_eq!(out.status.code(), Some(1));
    assert_eq!(
        document(&out),
        json!({
            "code": "many_matches",
            "count": 3,
            "error": "configuration error: --old matched 3 times in a.md; pass --replace-all to replace every occurrence",
        })
    );
}

// Issue 160, edge inputs: a name that is not UTF-8 ends the run through
// the argument parser at exit `2` before any request, on the verbs this
// item changes, as on every verb taking a path.
#[cfg(unix)]
#[test]
#[serial]
fn a_name_that_is_not_utf8_ends_through_the_parser() {
    use std::ffi::OsStr;
    use std::os::unix::ffi::OsStrExt;
    let d = Deployment::new(true);
    let name = OsStr::from_bytes(b"caf\xe9.md");
    for verb in [
        &[
            "revert",
            "--to",
            "1",
            "--parent",
            "7",
            "--repo",
            "alice/notes",
        ][..],
        &["rm", "--parent", "7", "--repo", "alice/notes"][..],
        &["cat", "--repo", "alice/notes", "--json"][..],
    ] {
        let mut command = AssertCommand::cargo_bin("syns").expect("syns binary");
        let out = command
            .current_dir(d.work.path())
            .env("SYNS_CONFIG_DIR", d.home.path())
            .env("SYNS_CACHE_DIR", d.cache.path())
            .env_remove("SYNS_URL")
            .arg("--server")
            .arg(d.server.uri())
            .arg(verb[0])
            .arg(name)
            .args(&verb[1..])
            .output()
            .expect("run syns");
        assert_eq!(out.status.code(), Some(2), "{verb:?}");
    }
    assert!(d.requests().is_empty());
}

// Issue 160 (REVIEW-syns-cli M2): the write verbs, `revert` and `history`
// tell an unreachable repository and an unknown version from a missing
// path — `rm` against a repository that stands nowhere never answers
// `not_found`.
#[test]
#[serial]
fn the_write_verbs_tell_a_missing_repository_and_version_from_a_missing_path() {
    use wiremock::matchers::body_partial_json;
    let d = Deployment::new(true);
    d.mount(
        Mock::given(method("POST"))
            .and(path_matcher("/api/v1/repos/alice/notes/files/x.md/revert"))
            .and(body_partial_json(json!({"to": "9999"})))
            .respond_with(
                ResponseTemplate::new(404)
                    .set_body_json(json!({"error": "not_found", "message": "Ref not found"})),
            ),
    );
    d.mount(
        Mock::given(method("POST"))
            .and(path_matcher(
                "/api/v1/repos/alice/notes/files/missing.md/revert",
            ))
            .respond_with(
                ResponseTemplate::new(404)
                    .set_body_json(json!({"error": "not_found", "message": "File not found"})),
            ),
    );
    let changeset = r#"{"files":[{"path":"a.md","content":"x"}]}"#;
    let repo = refusal("repo_not_found", "repository not found: alice/gone");
    let version = refusal("version_not_found", "version not found: 9999");
    let runs: Vec<(Vec<&str>, Option<&str>, Value)> = vec![
        (
            vec!["rm", "x.md", "--parent", "7", "--repo", "alice/gone"],
            None,
            repo.clone(),
        ),
        (
            vec!["write", "x.md", "--parent", "7", "--repo", "alice/gone"],
            Some("x"),
            repo.clone(),
        ),
        (
            vec![
                "edit",
                "x.md",
                "--old",
                "a",
                "--new",
                "b",
                "--parent",
                "7",
                "--repo",
                "alice/gone",
            ],
            None,
            repo.clone(),
        ),
        (
            vec!["commit", "--parent", "7", "--repo", "alice/gone"],
            Some(changeset),
            repo.clone(),
        ),
        (
            vec![
                "revert",
                "x.md",
                "--to",
                "3",
                "--parent",
                "7",
                "--repo",
                "alice/gone",
            ],
            None,
            repo.clone(),
        ),
        (vec!["history", "--repo", "alice/gone"], None, repo.clone()),
        (
            vec!["rm", "x.md", "--parent", "9999", "--repo", "alice/notes"],
            None,
            version.clone(),
        ),
        (
            vec!["write", "x.md", "--parent", "9999", "--repo", "alice/notes"],
            Some("x"),
            version.clone(),
        ),
        (
            vec![
                "edit",
                "x.md",
                "--old",
                "a",
                "--new",
                "b",
                "--parent",
                "9999",
                "--repo",
                "alice/notes",
            ],
            None,
            version.clone(),
        ),
        (
            vec!["commit", "--parent", "9999", "--repo", "alice/notes"],
            Some(changeset),
            version.clone(),
        ),
        (
            vec![
                "revert",
                "x.md",
                "--to",
                "3",
                "--parent",
                "9999",
                "--repo",
                "alice/notes",
            ],
            None,
            version.clone(),
        ),
        (
            vec![
                "revert",
                "x.md",
                "--to",
                "9999",
                "--parent",
                "7",
                "--repo",
                "alice/notes",
            ],
            None,
            version.clone(),
        ),
        (
            vec![
                "revert",
                "missing.md",
                "--to",
                "3",
                "--parent",
                "7",
                "--repo",
                "alice/notes",
            ],
            None,
            refusal("not_found", "path not found at version 3: missing.md"),
        ),
    ];
    for (mut args, stdin, expected) in runs {
        args.push("--json");
        let out = d.run(&args, stdin);
        assert_eq!(out.status.code(), Some(1), "{args:?}");
        assert_eq!(document(&out), expected, "{args:?}");
    }
}
