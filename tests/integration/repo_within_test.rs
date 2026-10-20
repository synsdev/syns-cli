//! Roadmap 277 — `syns repo --json` in one round trip (X5) and
//! `--within=<root>` (X6).
//!
//! X5: the record, the newest version and every enclosing share lookup
//! are sent at once, and the document is the one the walk wrote, the
//! nearest identity's visibility winning and any other refusal nearer
//! than it answering none.
//!
//! X6: `"within"` says whether the run's folder lies inside the root and
//! both belong to the same repository, a placed folder counting as its
//! holder's, read from the identity files alone with no request.

use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::process::Output;
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use serial_test::serial;
use wiremock::matchers::{method, path};
use wiremock::{Mock, ResponseTemplate};

use super::client_identity_test::{Deployment, HEAD};

/// A checkout `W` of `alice/work` holding plain folders, a placed folder
/// of its own holder, a nested checkout of another repository, a folder
/// whose name starts with a digit, and a file.
struct Tree {
    _dir: tempfile::TempDir,
    w: PathBuf,
}

impl Tree {
    fn new() -> Tree {
        let dir = tempfile::tempdir().unwrap();
        let w = std::fs::canonicalize(dir.path()).unwrap().join("work");
        let write = |relative: &str, text: &str| {
            let file = w.join(relative);
            std::fs::create_dir_all(file.parent().unwrap()).unwrap();
            std::fs::write(file, text).unwrap();
        };
        write(".syns.yaml", "owner: alice\nname: work\n");
        write("docs/inner/notes.md", "n\n");
        write("a/b/c/.syns.yaml", "holder: alice/work\npath: a/b/c\n");
        std::fs::create_dir_all(w.join("a/b/c/sub")).unwrap();
        write("other/.syns.yaml", "owner: bob\nname: other\n");
        write("2026/q1/plan.md", "p\n");
        write("a.md", "a\n");
        Tree { _dir: dir, w }
    }
}

fn record(owner: &str, name: &str) -> Value {
    json!({
        "owner": owner, "name": name, "description": null,
        "commitSha": HEAD, "status": "active", "author": null, "tags": [],
        "visibility": "public", "forkedFrom": null, "forkCount": 0,
        "fileCount": 3, "role": null,
        "createdAt": "2026-01-01T00:00:00Z", "updatedAt": "2026-01-01T00:00:00Z",
    })
}

fn newest(sha: &str) -> Value {
    json!({"data": [{
        "version": 9, "sha": sha, "parentSha": null, "message": "m",
        "messageBody": null, "author": "alice",
        "createdAt": "2026-01-01T00:00:00Z", "filesChanged": ["a.md"],
    }], "total": 9, "limit": 1, "offset": 0})
}

fn share(visibility: &str) -> Value {
    let mut identity = record("alice", "work-drafts");
    identity["visibility"] = json!(visibility);
    identity["sharedFolder"] = json!(true);
    identity
}

fn not_shared() -> ResponseTemplate {
    ResponseTemplate::new(404).set_body_json(json!({"error": "not_found", "message": "none"}))
}

/// The deployment answering `alice/work`'s record and newest version and
/// `bob/other`'s, each after `delay`.
fn deployment(delay: Duration) -> Deployment {
    let d = Deployment::new();
    for (owner, name) in [("alice", "work"), ("bob", "other")] {
        d.mount(
            Mock::given(method("GET"))
                .and(path(format!("/api/v1/repos/{owner}/{name}")))
                .respond_with(
                    ResponseTemplate::new(200)
                        .set_body_json(record(owner, name))
                        .set_delay(delay),
                ),
        );
        d.mount(
            Mock::given(method("GET"))
                .and(path(format!("/api/v1/repos/{owner}/{name}/versions")))
                .respond_with(
                    ResponseTemplate::new(200)
                        .set_body_json(newest(HEAD))
                        .set_delay(delay),
                ),
        );
    }
    d
}

fn mount_share(d: &Deployment, at: &str, answer: ResponseTemplate) {
    d.mount(
        Mock::given(method("GET"))
            .and(path(format!("/api/v1/repos/alice/work/shares/{at}")))
            .respond_with(answer),
    );
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

fn document(output: &Output) -> Value {
    assert_eq!(output.status.code(), Some(0), "{}", stderr(output));
    serde_json::from_slice(&output.stdout).expect("one document")
}

// X5: inside a folder three deep, the record, the newest version and the
// three share lookups go out at once: five answers each a second late
// take about a second, not five. The nearest identity's visibility is
// the folder's, and the document is the one the walk wrote.
#[test]
#[serial]
fn a_scoped_repo_sends_every_lookup_at_once_and_answers_as_the_walk_did() {
    let tree = Tree::new();
    let delay = Duration::from_millis(1000);
    let d = deployment(delay);
    mount_share(&d, "a/b/c", not_shared().set_delay(delay));
    mount_share(
        &d,
        "a/b",
        ResponseTemplate::new(200)
            .set_body_json(share("private"))
            .set_delay(delay),
    );
    mount_share(
        &d,
        "a",
        ResponseTemplate::new(200)
            .set_body_json(share("public"))
            .set_delay(delay),
    );

    let started = Instant::now();
    let out = d.run(&tree.w.join("a/b/c/sub"), &["--json", "repo"]);
    let took = started.elapsed();

    let mut expected = record("alice", "work");
    expected["sharedFolder"] = json!(false);
    expected["version"] = json!(9);
    expected["holder"] = json!("alice/work");
    expected["path"] = json!("a/b/c");
    expected["folderVisibility"] = json!("private");
    assert_eq!(document(&out), expected);
    assert!(
        took < delay * 2,
        "the lookups went one after another: {took:?}"
    );
    let requests = d.requests();
    assert_eq!(requests.len(), 5, "{requests:?}");
}

// X5: a refusal nearer than the identity answers no folder visibility,
// though the identity further up was asked beside it; with no identity
// anywhere none is answered either.
#[test]
#[serial]
fn a_refusal_nearer_than_the_identity_answers_none() {
    let tree = Tree::new();
    let d = deployment(Duration::ZERO);
    mount_share(&d, "a/b/c", not_shared());
    mount_share(
        &d,
        "a/b",
        ResponseTemplate::new(500).set_body_json(json!({"error": "internal_error"})),
    );
    mount_share(
        &d,
        "a",
        ResponseTemplate::new(200).set_body_json(share("public")),
    );

    let refused = document(&d.run(&tree.w.join("a/b/c"), &["--json", "repo"]));
    assert!(refused.get("folderVisibility").is_none(), "{refused}");
    assert_eq!(refused["path"], "a/b/c");

    let d = deployment(Duration::ZERO);
    for at in ["a/b/c", "a/b", "a"] {
        mount_share(&d, at, not_shared());
    }
    let none = document(&d.run(&tree.w.join("a/b/c"), &["--json", "repo"]));
    assert!(none.get("folderVisibility").is_none(), "{none}");
}

/// `"within"` of a `syns repo --json --within=<root>` run in `cwd`, after
/// checking the run sent what one without `--within` sends.
fn within(d: &Deployment, cwd: &Path, root: &OsStr) -> Value {
    let plain = d.requests().len();
    let without = document(&d.run(cwd, &["--json", "repo"]));
    let sent = d.requests().len() - plain;

    let before = d.requests().len();
    let args = [
        OsStr::new("--json"),
        OsStr::new("repo"),
        OsStr::new("--within"),
        root,
    ];
    let out = d.run_with(cwd, d.config.path(), None, &args);
    let mut with = document(&out);
    assert_eq!(d.requests().len() - before, sent, "--within sent a request");
    let answer = with
        .as_object_mut()
        .unwrap()
        .remove("within")
        .unwrap_or(Value::Null);
    assert_eq!(with, without, "--within changed the rest of the answer");
    answer
}

// X6: inside the root in the same repository is `true`, and outside it,
// in another repository, or under a root that does not stand is `false`;
// relative roots count from the run's folder.
#[test]
#[serial]
fn within_answers_membership_from_the_files_on_disk() {
    let tree = Tree::new();
    let d = deployment(Duration::ZERO);
    for at in ["a/b/c", "a/b", "a"] {
        mount_share(&d, at, not_shared());
    }
    let w = &tree.w;
    let inner = w.join("docs/inner");
    let other_checkout = tempfile::tempdir().unwrap();
    std::fs::write(
        other_checkout.path().join(".syns.yaml"),
        "owner: bob\nname: other\n",
    )
    .unwrap();
    let missing = w.join("no-such-folder");

    let cases: Vec<(&str, PathBuf, &OsStr, bool)> = vec![
        ("the root itself", w.clone(), w.as_os_str(), true),
        (
            "a folder below the root",
            inner.clone(),
            w.as_os_str(),
            true,
        ),
        ("`.`", inner.clone(), OsStr::new("."), true),
        (
            "`..`, still the checkout",
            inner.clone(),
            OsStr::new(".."),
            true,
        ),
        (
            "`../..`, the checkout",
            inner.clone(),
            OsStr::new("../.."),
            true,
        ),
        (
            "`..` above the checkout",
            w.clone(),
            OsStr::new(".."),
            false,
        ),
        (
            "a root below the folder",
            w.clone(),
            OsStr::new("docs"),
            false,
        ),
        (
            "a placed folder, its holder's",
            w.join("a/b/c/sub"),
            w.as_os_str(),
            true,
        ),
        (
            "the placed folder as the root",
            w.join("a/b/c/sub"),
            OsStr::new(".."),
            true,
        ),
        (
            "a nested checkout of another repository",
            w.join("other"),
            w.as_os_str(),
            false,
        ),
        (
            "a root in another repository",
            inner.clone(),
            other_checkout.path().as_os_str(),
            false,
        ),
        (
            "a digit-first root",
            w.join("2026/q1"),
            OsStr::new("../../2026"),
            true,
        ),
        (
            "a digit-first relative root",
            w.clone(),
            OsStr::new("2026"),
            false,
        ),
        (
            "a root that does not stand",
            inner.clone(),
            missing.as_os_str(),
            false,
        ),
        ("a file as the root", w.clone(), OsStr::new("a.md"), false),
    ];
    for (case, cwd, root, expected) in cases {
        assert_eq!(within(&d, &cwd, root), json!(expected), "{case}");
    }
}

// X6: a root reached through a link is the folder the link names.
#[cfg(unix)]
#[test]
#[serial]
fn within_follows_a_link_to_the_root() {
    let tree = Tree::new();
    let d = deployment(Duration::ZERO);
    let links = tempfile::tempdir().unwrap();
    let link = links.path().join("w-link");
    std::os::unix::fs::symlink(&tree.w, &link).unwrap();
    assert_eq!(
        within(&d, &tree.w.join("docs/inner"), link.as_os_str()),
        json!(true)
    );
}

// X6: a root that is not UTF-8 is a folder name like any other: where the
// platform lets one stand the run inside it is within it, and where it
// does not, no folder stands there.
#[cfg(unix)]
#[test]
#[serial]
fn within_takes_a_root_that_is_not_utf8() {
    use std::os::unix::ffi::OsStrExt;
    let tree = Tree::new();
    let d = deployment(Duration::ZERO);
    let name = OsStr::from_bytes(b"caf\xe9");
    let root = tree.w.join(name);
    let created = std::fs::create_dir(&root).is_ok();
    let cwd = if created {
        root.clone()
    } else {
        tree.w.clone()
    };
    assert_eq!(within(&d, &cwd, root.as_os_str()), json!(created));
}

// X6: an empty root is refused by the argument parser at exit 2, and
// `--within` beside an update option or a subcommand as a configuration
// error at exit 1, each before any request.
#[test]
#[serial]
fn within_refusals_send_nothing() {
    let tree = Tree::new();
    let d = deployment(Duration::ZERO);
    for (args, exit) in [
        // The argument parser's own refusal of an empty value.
        (vec!["--json", "repo", "--within", ""], 2),
        (vec!["repo", "--within="], 2),
        (vec!["repo", "--within", ".", "--description", "d"], 1),
        (vec!["repo", "--within", ".", "--tag", "t"], 1),
        (vec!["repo", "--within", ".", "create", "fresh"], 1),
        (vec!["repo", "--within", ".", "list"], 1),
    ] {
        let out = d.run(&tree.w, &args);
        assert_eq!(out.status.code(), Some(exit), "{args:?}: {}", stderr(&out));
        assert!(
            stderr(&out).contains("--within"),
            "{args:?}: {}",
            stderr(&out)
        );
    }
    assert!(d.requests().is_empty(), "{:?}", d.requests());
}

// X6: the rendered answer names the membership too.
#[test]
#[serial]
fn within_is_rendered_beside_the_record() {
    let tree = Tree::new();
    let d = deployment(Duration::ZERO);
    let out = d.run(&tree.w.join("docs"), &["repo", "--within", ".."]);
    assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
    let rendered = String::from_utf8_lossy(&out.stdout).into_owned();
    assert!(
        rendered
            .lines()
            .any(|line| line.contains("Within") && line.contains("yes")),
        "{rendered}"
    );
    let plain = d.run(&tree.w.join("docs"), &["repo"]);
    assert!(!String::from_utf8_lossy(&plain.stdout).contains("Within"));
}

// X6: a folder bound to its identity counts as its holder's too: inside
// a checkout of a shared folder's identity, the identity's folder is the
// root it lies within, and the folder above it, in no repository, holds
// it in none.
#[test]
#[serial]
fn within_counts_an_identity_checkout_as_its_holder_s() {
    let dir = tempfile::tempdir().unwrap();
    let base = std::fs::canonicalize(dir.path()).unwrap();
    let copy = base.join("q3-plan");
    std::fs::create_dir_all(copy.join("sub")).unwrap();
    std::fs::write(
        copy.join(".syns.yaml"),
        "holder: alice/docs\npath: q3-plan\nshared_as: docs-q3-plan\n",
    )
    .unwrap();
    let d = Deployment::new();
    let mut identity = record("alice", "docs-q3-plan");
    identity["sharedFolder"] = json!(true);
    d.mount(
        Mock::given(method("GET"))
            .and(path("/api/v1/repos/alice/docs-q3-plan"))
            .respond_with(ResponseTemplate::new(200).set_body_json(identity)),
    );
    d.mount(
        Mock::given(method("GET"))
            .and(path("/api/v1/repos/alice/docs-q3-plan/versions"))
            .respond_with(ResponseTemplate::new(200).set_body_json(newest(HEAD))),
    );

    let sub = copy.join("sub");
    assert_eq!(within(&d, &sub, copy.as_os_str()), json!(true));
    assert_eq!(within(&d, &sub, base.as_os_str()), json!(false));
    let answer = document(&d.run(&sub, &["--json", "repo"]));
    assert_eq!(answer["version"], 9);
    assert_eq!(answer["name"], "docs-q3-plan");
}

// X5 (review m2): the walk's answer settles once every lookup nearer than
// it is answered: an identity at the folder itself is the answer, whatever
// a farther lookup is still waiting on.
#[test]
#[serial]
fn a_scoped_repo_settles_on_the_nearest_identity_without_the_farther_lookups() {
    let tree = Tree::new();
    let d = deployment(Duration::ZERO);
    let slow = Duration::from_secs(5);
    mount_share(
        &d,
        "a/b/c",
        ResponseTemplate::new(200).set_body_json(share("private")),
    );
    mount_share(&d, "a/b", not_shared().set_delay(slow));
    mount_share(
        &d,
        "a",
        ResponseTemplate::new(200)
            .set_body_json(share("public"))
            .set_delay(slow),
    );

    let started = Instant::now();
    let answer = document(&d.run(&tree.w.join("a/b/c"), &["--json", "repo"]));
    let took = started.elapsed();

    assert_eq!(answer["folderVisibility"], "private", "{answer}");
    assert!(
        took < Duration::from_secs(3),
        "waited on a farther lookup: {took:?}"
    );
}
