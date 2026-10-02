//! u255 — both invocations take the resolved repository root as their
//! content root.
//!
//! Every test here drives `cmd_push` / `cmd_pull` in process through
//! `tests/common`'s `setup()` harness and asserts against the body
//! wiremock captured, because the defect issue 119 reports is invisible
//! from the command's own return value: the run SUCCEEDS, and what it
//! got wrong is which paths rode on the wire.
//!
//! The fixture tree is the one SPEC u255 § Tests is written over —
//! a repository root holding `root-a.md` and `root-b.md`, a `sub/`
//! holding `nested.md`, and one `.syns.yaml` at the root alone.
//!
//! Every test is `#[serial]`: they set the process working directory,
//! which is what makes a bare invocation's resolution observable.

use crate::common::{TestContext, seed_credentials, setup};
use serde_json::json;
use serial_test::serial;
use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use syns_cli::auth::token::TokenStore;
use syns_cli::commands::pull::cmd_pull;
use syns_cli::commands::push::{PushArgs, cmd_push};
use syns_cli::errors::{BelongsRemedy, CliError};
use syns_cli::push::hash::blob_sha1;
use syns_cli::push::manifest::Manifest;
use syns_cli::push::working_copy::WorkingCopy;
use wiremock::matchers::{method, path as path_matcher};
use wiremock::{Mock, ResponseTemplate};

/// Restores the process working directory when it goes out of scope.
///
/// A test that leaves the cwd inside a `TempDir` breaks every LATER
/// test in the binary, because the directory is unlinked on drop and
/// `std::env::current_dir` then fails outright.
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

/// The fixture tree, with its one identity file at the root.
fn seed_tree(ctx: &TestContext) -> PathBuf {
    let root = ctx.project_dir.path().to_path_buf();
    fs::write(root.join("root-a.md"), "a").unwrap();
    fs::write(root.join("root-b.md"), "b").unwrap();
    fs::create_dir_all(root.join("sub")).unwrap();
    fs::write(root.join("sub/nested.md"), "n").unwrap();
    fs::write(root.join(".syns.yaml"), "owner: alice\nname: proj\n").unwrap();
    root
}

/// A local record naming every path of the fixture tree at the content
/// it was seeded with.
fn seed_record(ctx: &TestContext) {
    let mut manifest = Manifest::default();
    manifest.update(
        "1111111111111111111111111111111111111111".to_string(),
        HashMap::from([
            ("root-a.md".to_string(), blob_sha1(b"a")),
            ("root-b.md".to_string(), blob_sha1(b"b")),
            ("sub/nested.md".to_string(), blob_sha1(b"n")),
        ]),
    );
    manifest
        .save(ctx.config.stores(), "alice", "proj", ctx.config.cache_dir())
        .unwrap();
}

/// The working copy base and head a bare publication converges from:
/// the fixture tree recorded at one commit, the head standing there too.
/// A bare publication reads this base rather than the local record.
async fn seed_converged_base(ctx: &TestContext, root: &Path) {
    let files = HashMap::from([
        (
            ".syns.yaml".to_string(),
            blob_sha1(b"owner: alice\nname: proj\n"),
        ),
        ("root-a.md".to_string(), blob_sha1(b"a")),
        ("root-b.md".to_string(), blob_sha1(b"b")),
        ("sub/nested.md".to_string(), blob_sha1(b"n")),
    ]);
    WorkingCopy::open(ctx.config.stores(), "alice", "proj", root)
        .unwrap()
        .record_base("1111111111111111111111111111111111111111", files.clone())
        .unwrap();
    let entries: Vec<serde_json::Value> = files
        .iter()
        .map(|(p, s)| json!({"name": p, "path": p, "type": "file", "size": 1, "sha": s}))
        .collect();
    Mock::given(method("GET"))
        .and(path_matcher("/api/v1/repos/alice/proj/tree"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "entries": entries,
            "commitSha": "1111111111111111111111111111111111111111",
            "truncated": false
        })))
        .with_priority(1)
        .mount(&ctx.mock_server)
        .await;
}

async fn mount_push_mocks(ctx: &TestContext, repo_id: &str) {
    Mock::given(method("GET"))
        .and(path_matcher(format!("/api/v1/repos/{repo_id}/tree")))
        .respond_with(ResponseTemplate::new(404).set_body_json(json!({"error": "not_found"})))
        .mount(&ctx.mock_server)
        .await;
    Mock::given(method("PUT"))
        .and(path_matcher(format!("/api/v1/repos/{repo_id}/push")))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "commitSha": "2222222222222222222222222222222222222222",
            "version": 2,
            "filesChanged": 1,
            "created": false
        })))
        .mount(&ctx.mock_server)
        .await;
}

/// The body of the last `PUT .../push` wiremock captured.
async fn last_push_body(ctx: &TestContext) -> serde_json::Value {
    let requests = ctx.mock_server.received_requests().await.unwrap();
    let last = requests
        .iter()
        .rfind(|r| r.url.path().ends_with("/push"))
        .expect("no publication reached the server");
    serde_json::from_slice(&last.body).unwrap()
}

fn body_paths(body: &serde_json::Value) -> Vec<String> {
    let mut paths: Vec<String> = body["files"]
        .as_array()
        .expect("files")
        .iter()
        .map(|f| f["path"].as_str().unwrap().to_string())
        .collect();
    paths.sort();
    paths
}

fn deletion_paths(body: &serde_json::Value) -> Vec<String> {
    let mut paths: Vec<String> = body["deletions"]
        .as_array()
        .map(|a| {
            a.iter()
                .map(|d| d["path"].as_str().unwrap().to_string())
                .collect()
        })
        .unwrap_or_default();
    paths.sort();
    paths
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

// ---- bare publication -------------------------------------------------

#[tokio::test(flavor = "current_thread")]
#[serial]
async fn bare_push_from_subdirectory_matches_a_push_from_the_root() {
    let ctx = setup().await;
    seed_credentials(&ctx, "test-token", "alice");
    let root = seed_tree(&ctx);
    mount_push_mocks(&ctx, "alice/proj").await;

    let from_root = {
        let _cwd = CwdGuard::enter(&root);
        cmd_push(&ctx.config, &ctx.output, &push_args(None))
            .await
            .unwrap();
        body_paths(&last_push_body(&ctx).await)
    };

    // A bare publication converges: the first run recorded its commit as
    // the working copy's base. Clearing that state makes the second run a
    // first publication again, so the two bodies stay comparable.
    fs::remove_dir_all(ctx.config.cache_dir().join("working-copies")).unwrap();

    let from_sub = {
        let _cwd = CwdGuard::enter(&root.join("sub"));
        cmd_push(&ctx.config, &ctx.output, &push_args(None))
            .await
            .unwrap();
        body_paths(&last_push_body(&ctx).await)
    };

    for expected in ["root-a.md", "root-b.md", "sub/nested.md"] {
        assert!(
            from_sub.contains(&expected.to_string()),
            "{expected} missing from the run started in sub/: {from_sub:?}"
        );
    }
    assert_eq!(
        from_root, from_sub,
        "a run from the root and a run from sub/ published different path sets"
    );
}

#[tokio::test(flavor = "current_thread")]
#[serial]
async fn bare_push_from_subdirectory_names_no_deletion() {
    let ctx = setup().await;
    seed_credentials(&ctx, "test-token", "alice");
    let root = seed_tree(&ctx);
    seed_record(&ctx);
    mount_push_mocks(&ctx, "alice/proj").await;

    let _cwd = CwdGuard::enter(&root.join("sub"));
    cmd_push(&ctx.config, &ctx.output, &push_args(None))
        .await
        .unwrap();

    let body = last_push_body(&ctx).await;
    assert!(
        body.get("deletions").is_none_or(|d| d.is_null()),
        "a bare publication from sub/ named deletions: {body}"
    );
}

#[tokio::test(flavor = "current_thread")]
#[serial]
async fn bare_push_from_subdirectory_writes_no_nested_identity_file() {
    let ctx = setup().await;
    seed_credentials(&ctx, "test-token", "alice");
    let root = seed_tree(&ctx);
    mount_push_mocks(&ctx, "alice/proj").await;

    {
        let _cwd = CwdGuard::enter(&root.join("sub"));
        cmd_push(&ctx.config, &ctx.output, &push_args(None))
            .await
            .unwrap();
    }

    assert!(
        !root.join("sub/.syns.yaml").exists(),
        "a bare publication from sub/ entrenched sub/ as a repository of its own"
    );
}

#[tokio::test(flavor = "current_thread")]
#[serial]
async fn first_push_writes_the_identity_file_where_none_resolves() {
    let ctx = setup().await;
    seed_credentials(&ctx, "test-token", "alice");
    let root = ctx.project_dir.path().to_path_buf();
    fs::write(root.join("only.md"), "o").unwrap();
    mount_push_mocks(&ctx, "alice/proj").await;

    let mut args = push_args(Some(root.clone()));
    args.name = Some("alice/proj".into());
    cmd_push(&ctx.config, &ctx.output, &args).await.unwrap();

    assert!(root.join(".syns.yaml").exists());
    assert!(body_paths(&last_push_body(&ctx).await).contains(&".syns.yaml".to_string()));
}

// ---- scoped publication ----------------------------------------------

#[tokio::test(flavor = "current_thread")]
#[serial]
async fn scoped_push_confines_deletions_to_the_named_subtree() {
    let ctx = setup().await;
    seed_credentials(&ctx, "test-token", "alice");
    let root = seed_tree(&ctx);
    seed_record(&ctx);
    fs::remove_file(root.join("sub/nested.md")).unwrap();
    mount_push_mocks(&ctx, "alice/proj").await;

    let _cwd = CwdGuard::enter(&root);
    cmd_push(&ctx.config, &ctx.output, &push_args(Some("sub".into())))
        .await
        .unwrap();

    let body = last_push_body(&ctx).await;
    assert_eq!(deletion_paths(&body), vec!["sub/nested.md".to_string()]);
    assert!(
        body_paths(&body).is_empty(),
        "a delete-only publication carried files: {body}"
    );
}

#[tokio::test(flavor = "current_thread")]
#[serial]
async fn scoped_push_sends_repository_relative_paths() {
    let ctx = setup().await;
    seed_credentials(&ctx, "test-token", "alice");
    let root = seed_tree(&ctx);
    seed_record(&ctx);
    fs::write(root.join("sub/added.md"), "x").unwrap();
    mount_push_mocks(&ctx, "alice/proj").await;

    let _cwd = CwdGuard::enter(&root);
    cmd_push(&ctx.config, &ctx.output, &push_args(Some("sub".into())))
        .await
        .unwrap();

    assert!(
        body_paths(&last_push_body(&ctx).await).contains(&"sub/added.md".to_string()),
        "the scoped publication did not name the path relative to the repository root"
    );
}

#[tokio::test(flavor = "current_thread")]
#[serial]
async fn scoped_push_leaves_the_local_record_naming_the_whole_tree() {
    let ctx = setup().await;
    seed_credentials(&ctx, "test-token", "alice");
    let root = seed_tree(&ctx);
    seed_record(&ctx);
    mount_push_mocks(&ctx, "alice/proj").await;

    {
        let _cwd = CwdGuard::enter(&root);
        cmd_push(&ctx.config, &ctx.output, &push_args(Some("sub".into())))
            .await
            .unwrap();
    }

    let record = Manifest::load(ctx.config.stores(), "alice", "proj", ctx.config.cache_dir())
        .expect("record");
    assert!(record.file_sha("root-a.md").is_some());
    assert!(record.file_sha("root-b.md").is_some());
    assert!(record.file_sha("sub/nested.md").is_some());
}

#[tokio::test(flavor = "current_thread")]
#[serial]
async fn forced_scoped_push_leaves_the_local_record_naming_the_whole_tree() {
    let ctx = setup().await;
    seed_credentials(&ctx, "test-token", "alice");
    let root = seed_tree(&ctx);
    seed_record(&ctx);
    mount_push_mocks(&ctx, "alice/proj").await;

    {
        let _cwd = CwdGuard::enter(&root);
        let mut args = push_args(Some("sub".into()));
        args.force = true;
        cmd_push(&ctx.config, &ctx.output, &args).await.unwrap();
    }

    let record = Manifest::load(ctx.config.stores(), "alice", "proj", ctx.config.cache_dir())
        .expect("record");
    assert!(record.file_sha("root-a.md").is_some());
    assert!(record.file_sha("root-b.md").is_some());
}

/// The corner the plan's provisional rule left undefined and the filer
/// settled: `--force` with NO local record on disk. The reference set
/// is empty by force and the record would otherwise be rewritten from
/// the scope alone, handing the NEXT bare publication a deletion for
/// every path outside it. A scoped publication establishes the real
/// remote state through `EP-tree` first, whatever the flags.
#[tokio::test(flavor = "current_thread")]
#[serial]
async fn forced_scoped_push_with_no_local_record_rebuilds_it_from_the_remote_tree() {
    let ctx = setup().await;
    seed_credentials(&ctx, "test-token", "alice");
    let root = seed_tree(&ctx);

    Mock::given(method("GET"))
        .and(path_matcher("/api/v1/repos/alice/proj/tree"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "entries": [
                {"name": "root-a.md", "path": "root-a.md", "type": "file", "size": 1, "sha": blob_sha1(b"a")},
                {"name": "root-b.md", "path": "root-b.md", "type": "file", "size": 1, "sha": blob_sha1(b"b")},
                {"name": "nested.md", "path": "sub/nested.md", "type": "file", "size": 1, "sha": blob_sha1(b"n")}
            ],
            "commitSha": "3333333333333333333333333333333333333333",
            "truncated": false
        })))
        .mount(&ctx.mock_server)
        .await;
    Mock::given(method("PUT"))
        .and(path_matcher("/api/v1/repos/alice/proj/push"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "commitSha": "2222222222222222222222222222222222222222",
            "version": 2,
            "filesChanged": 1,
            "created": false
        })))
        .mount(&ctx.mock_server)
        .await;

    {
        let _cwd = CwdGuard::enter(&root);
        let mut args = push_args(Some("sub".into()));
        args.force = true;
        cmd_push(&ctx.config, &ctx.output, &args).await.unwrap();
    }

    let record = Manifest::load(ctx.config.stores(), "alice", "proj", ctx.config.cache_dir())
        .expect("record");
    assert!(
        record.file_sha("root-a.md").is_some(),
        "a forced scoped publication with no record dropped root-a.md"
    );
    assert!(
        record.file_sha("root-b.md").is_some(),
        "a forced scoped publication with no record dropped root-b.md"
    );
}

#[tokio::test(flavor = "current_thread")]
#[serial]
async fn scoped_push_under_strictness_counts_no_out_of_prefix_path_as_dropped() {
    let ctx = setup().await;
    seed_credentials(&ctx, "test-token", "alice");
    let root = seed_tree(&ctx);
    fs::write(root.join(".synsignore"), "root-b.md\n").unwrap();
    mount_push_mocks(&ctx, "alice/proj").await;

    {
        let _cwd = CwdGuard::enter(&root);
        let mut args = push_args(Some("sub".into()));
        args.strict = true;
        cmd_push(&ctx.config, &ctx.output, &args)
            .await
            .expect("a scoped strict publication must not be refused for an out-of-scope drop");
    }

    assert!(body_paths(&last_push_body(&ctx).await).contains(&"sub/nested.md".to_string()));
}

#[tokio::test(flavor = "current_thread")]
#[serial]
async fn scoped_push_applies_the_repository_root_ignore_file() {
    let ctx = setup().await;
    seed_credentials(&ctx, "test-token", "alice");
    let root = seed_tree(&ctx);
    fs::write(root.join(".synsignore"), "*.log\n").unwrap();
    fs::write(root.join("sub/keep.md"), "k").unwrap();
    fs::write(root.join("sub/drop.log"), "l").unwrap();
    mount_push_mocks(&ctx, "alice/proj").await;

    {
        let _cwd = CwdGuard::enter(&root);
        cmd_push(&ctx.config, &ctx.output, &push_args(Some("sub".into())))
            .await
            .unwrap();
    }

    let paths = body_paths(&last_push_body(&ctx).await);
    assert!(paths.contains(&"sub/keep.md".to_string()), "{paths:?}");
    assert!(
        !paths.iter().any(|p| p.ends_with(".log")),
        "the repository root's ignore file was not applied: {paths:?}"
    );
}

#[tokio::test(flavor = "current_thread")]
#[serial]
async fn push_scoped_to_one_file_sends_that_path_alone() {
    let ctx = setup().await;
    seed_credentials(&ctx, "test-token", "alice");
    let root = seed_tree(&ctx);
    seed_record(&ctx);
    fs::write(root.join("sub/nested.md"), "changed").unwrap();
    mount_push_mocks(&ctx, "alice/proj").await;

    {
        let _cwd = CwdGuard::enter(&root);
        cmd_push(
            &ctx.config,
            &ctx.output,
            &push_args(Some("sub/nested.md".into())),
        )
        .await
        .unwrap();
    }

    let body = last_push_body(&ctx).await;
    assert_eq!(body_paths(&body), vec!["sub/nested.md".to_string()]);
    assert!(
        body.get("deletions").is_none_or(|d| d.is_null()),
        "a single-path publication named deletions: {body}"
    );
}

#[tokio::test(flavor = "current_thread")]
#[serial]
async fn push_naming_another_repository_publishes_the_starting_directory() {
    let ctx = setup().await;
    seed_credentials(&ctx, "test-token", "alice");
    let root = seed_tree(&ctx);
    mount_push_mocks(&ctx, "other/repo").await;

    {
        let _cwd = CwdGuard::enter(&root.join("sub"));
        let mut args = push_args(None);
        args.name = Some("other/repo".into());
        cmd_push(&ctx.config, &ctx.output, &args).await.unwrap();
    }

    let paths = body_paths(&last_push_body(&ctx).await);
    assert!(
        paths.contains(&"nested.md".to_string()),
        "the publication was not rooted at sub/: {paths:?}"
    );
    let marker = fs::read_to_string(root.join("sub/.syns.yaml")).expect("sub/.syns.yaml");
    assert!(marker.contains("other"), "{marker}");
    assert!(marker.contains("repo"), "{marker}");
}

// ---- retrieval --------------------------------------------------------

async fn mount_pull_mocks(ctx: &TestContext, repo_id: &str, entries: serde_json::Value) {
    Mock::given(method("GET"))
        .and(path_matcher(format!("/api/v1/repos/{repo_id}/tree")))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "entries": entries,
            "commitSha": "4444444444444444444444444444444444444444",
            "truncated": false
        })))
        .mount(&ctx.mock_server)
        .await;
    Mock::given(method("GET"))
        .and(path_matcher(format!(
            "/api/v1/repos/{repo_id}/raw/root-a.md"
        )))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("ETag", format!("\"{}\"", blob_sha1(b"a")).as_str())
                .set_body_bytes("a".as_bytes().to_vec()),
        )
        .mount(&ctx.mock_server)
        .await;
    Mock::given(method("GET"))
        .and(path_matcher(format!(
            "/api/v1/repos/{repo_id}/raw/sub/nested.md"
        )))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("ETag", format!("\"{}\"", blob_sha1(b"n")).as_str())
                .set_body_bytes("n".as_bytes().to_vec()),
        )
        .mount(&ctx.mock_server)
        .await;
}

fn server_tree() -> serde_json::Value {
    json!([
        {"name": "root-a.md", "path": "root-a.md", "type": "file", "size": 1, "sha": blob_sha1(b"a")},
        {"name": "sub", "path": "sub", "type": "dir", "size": null, "sha": null},
        {"name": "nested.md", "path": "sub/nested.md", "type": "file", "size": 1, "sha": blob_sha1(b"n")}
    ])
}

#[tokio::test(flavor = "current_thread")]
#[serial]
async fn bare_pull_from_subdirectory_writes_into_the_repository_root() {
    let ctx = setup().await;
    seed_credentials(&ctx, "test-token", "alice");
    let root = ctx.project_dir.path().to_path_buf();
    fs::create_dir_all(root.join("sub")).unwrap();
    fs::write(root.join(".syns.yaml"), "owner: alice\nname: proj\n").unwrap();
    mount_pull_mocks(&ctx, "alice/proj", server_tree()).await;

    {
        let _cwd = CwdGuard::enter(&root.join("sub"));
        cmd_pull(
            &ctx.config,
            &ctx.output,
            None,
            None,
            None,
            false,
            false,
            None,
        )
        .await
        .unwrap();
    }

    assert!(
        root.join("root-a.md").is_file(),
        "the retrieval did not write into the repository root"
    );
    assert!(
        root.join("sub/nested.md").is_file(),
        "the retrieval did not write sub/nested.md at the repository root"
    );
    assert!(
        !root.join("sub/sub").exists(),
        "the retrieval nested the tree under its own working directory"
    );
}

#[tokio::test(flavor = "current_thread")]
#[serial]
async fn bare_pull_from_subdirectory_reconciles_at_the_repository_root() {
    let ctx = setup().await;
    seed_credentials(&ctx, "test-token", "alice");
    let root = ctx.project_dir.path().to_path_buf();
    fs::create_dir_all(root.join("sub")).unwrap();
    fs::write(root.join(".syns.yaml"), "owner: alice\nname: proj\n").unwrap();
    fs::write(root.join("root-b.md"), "b").unwrap();

    // A retrieval converges from the working copy's base, not the local
    // record: the base names `root-b.md`, which the head no longer holds.
    WorkingCopy::open(ctx.config.stores(), "alice", "proj", &root)
        .unwrap()
        .record_base(
            "1111111111111111111111111111111111111111",
            HashMap::from([("root-b.md".to_string(), blob_sha1(b"b"))]),
        )
        .unwrap();

    mount_pull_mocks(&ctx, "alice/proj", server_tree()).await;

    {
        let _cwd = CwdGuard::enter(&root.join("sub"));
        cmd_pull(
            &ctx.config,
            &ctx.output,
            None,
            None,
            None,
            false,
            false,
            None,
        )
        .await
        .unwrap();
    }

    assert!(
        !root.join("root-b.md").exists(),
        "the reconciled removal was not taken at the repository root"
    );
}

/// u263 (issue 130), inverting the u255-era retrieval that wrote a second
/// identity file under `sub/`: a repository named on the command line is
/// refused wherever the nearest identity file above the starting directory
/// names another, naming the directory holding that file.
#[tokio::test(flavor = "current_thread")]
#[serial]
async fn pull_of_another_repository_below_an_identity_file_refuses() {
    let ctx = setup().await;
    seed_credentials(&ctx, "test-token", "alice");
    let root = fs::canonicalize(ctx.project_dir.path()).unwrap();
    fs::create_dir_all(root.join("sub")).unwrap();
    fs::write(root.join(".syns.yaml"), "owner: alice\nname: proj\n").unwrap();
    mount_pull_mocks(&ctx, "other/repo", server_tree()).await;

    let result = {
        let _cwd = CwdGuard::enter(&root.join("sub"));
        cmd_pull(
            &ctx.config,
            &ctx.output,
            Some(("other".into(), "repo".into())),
            None,
            None,
            false,
            false,
            None,
        )
        .await
    };

    match result {
        Err(CliError::PathBelongsToAnotherRepository {
            path,
            standing,
            requested,
            remedy: BelongsRemedy::Pull,
        }) => {
            assert_eq!(path, root);
            assert_eq!(standing, "alice/proj");
            assert_eq!(requested, "other/repo");
        }
        other => panic!("expected the path-belongs refusal, got {other:?}"),
    }
    assert!(
        ctx.mock_server
            .received_requests()
            .await
            .unwrap()
            .is_empty()
    );
    assert_eq!(fs::read_dir(root.join("sub")).unwrap().count(), 0);
    assert_eq!(
        fs::read_to_string(root.join(".syns.yaml")).unwrap(),
        "owner: alice\nname: proj\n"
    );
}

// ---- round 2: the refusals the review found missing --------------------

/// The unscoped refusal is the regression this round exists to prevent.
/// A walk that collected nothing, with no path argument scoping it, must
/// be refused as it was before this unit — a body naming every path the
/// local record holds as a deletion empties the repository on the server.
#[tokio::test(flavor = "current_thread")]
#[serial]
async fn unscoped_push_collecting_nothing_is_refused_before_the_server() {
    let ctx = setup().await;
    seed_credentials(&ctx, "test-token", "alice");
    let root = seed_tree(&ctx);
    seed_record(&ctx);
    seed_converged_base(&ctx, &root).await;
    fs::write(root.join(".gitignore"), "*\n").unwrap();
    // A convergence leaves out of its comparison every file an exclusion
    // keeps out while it stands on disk, so the whole-tree deletion the
    // guard exists for is the one a walk collecting nothing names over
    // files actually gone from disk.
    for gone in ["root-a.md", "root-b.md", "sub/nested.md"] {
        fs::remove_file(root.join(gone)).unwrap();
    }
    mount_push_mocks(&ctx, "alice/proj").await;

    let result = {
        let _cwd = CwdGuard::enter(&root);
        cmd_push(&ctx.config, &ctx.output, &push_args(None)).await
    };

    assert!(
        matches!(result, Err(CliError::PushEmpty { .. })),
        "an unscoped publication collecting nothing was not refused: {result:?}"
    );
    let requests = ctx.mock_server.received_requests().await.unwrap();
    assert!(
        requests.iter().all(|r| r.method != reqwest::Method::PUT),
        "a publication deleting the whole repository reached the server"
    );
}

/// `--allow-empty` is the one way past the unscoped refusal, and stays so.
#[tokio::test(flavor = "current_thread")]
#[serial]
async fn allow_empty_still_carries_an_unscoped_publication_past_the_refusal() {
    let ctx = setup().await;
    seed_credentials(&ctx, "test-token", "alice");
    let root = seed_tree(&ctx);
    seed_record(&ctx);
    seed_converged_base(&ctx, &root).await;
    fs::write(root.join(".gitignore"), "*\n").unwrap();
    // A convergence leaves out of its comparison every file an exclusion
    // keeps out while it stands on disk, so the whole-tree deletion the
    // guard exists for is the one a walk collecting nothing names over
    // files actually gone from disk.
    for gone in ["root-a.md", "root-b.md", "sub/nested.md"] {
        fs::remove_file(root.join(gone)).unwrap();
    }
    mount_push_mocks(&ctx, "alice/proj").await;

    {
        let _cwd = CwdGuard::enter(&root);
        let mut args = push_args(None);
        args.allow_empty = true;
        cmd_push(&ctx.config, &ctx.output, &args).await.unwrap();
    }

    let requests = ctx.mock_server.received_requests().await.unwrap();
    assert!(
        requests.iter().any(|r| r.method == reqwest::Method::PUT),
        "--allow-empty did not carry the publication to the server"
    );
}

// SPEC u309 Tests, the row of this name: inverting the u255-era case
// that published another repository's tree from a folder whose own
// identity file names `alice/proj`.
#[tokio::test(flavor = "current_thread")]
#[serial]
async fn push_naming_another_repository_leaves_a_standing_identity_file_alone() {
    let ctx = setup().await;
    seed_credentials(&ctx, "test-token", "alice");
    let (folder, before) = seed_owned_folder(&ctx);
    mount_push_mocks(&ctx, "bob/other").await;

    let mut args = push_args(Some(folder.clone()));
    args.name = Some("bob/other".into());
    let result = cmd_push(&ctx.config, &ctx.output, &args).await;

    assert_refused_as_another_repository(&result, &folder, "alice/proj", "bob/other");
    assert_no_request(&ctx).await;
    assert_no_working_copy(&ctx, "bob");
    assert_eq!(fs::read(folder.join(".syns.yaml")).unwrap(), before);
}

#[tokio::test(flavor = "current_thread")]
#[serial]
async fn pull_into_a_relative_destination_writes_no_nested_identity_file() {
    let ctx = setup().await;
    seed_credentials(&ctx, "test-token", "alice");
    let root = ctx.project_dir.path().to_path_buf();
    fs::create_dir_all(root.join("sub")).unwrap();
    fs::write(root.join(".syns.yaml"), "owner: alice\nname: proj\n").unwrap();
    mount_pull_mocks(&ctx, "alice/proj", server_tree()).await;

    {
        let _cwd = CwdGuard::enter(&root.join("sub"));
        cmd_pull(
            &ctx.config,
            &ctx.output,
            Some(("alice".into(), "proj".into())),
            Some("dest".into()),
            None,
            false,
            false,
            None,
        )
        .await
        .unwrap();
    }

    assert!(
        root.join("sub/dest/root-a.md").is_file(),
        "the retrieval did not write into the relative destination"
    );
    assert!(
        !root.join("sub/dest/.syns.yaml").exists(),
        "a relative destination under an identity file naming this repository was entrenched"
    );
}

#[tokio::test(flavor = "current_thread")]
#[serial]
async fn a_scoped_push_collecting_nothing_reports_the_scoped_directory() {
    let ctx = setup().await;
    seed_credentials(&ctx, "test-token", "alice");
    let root = seed_tree(&ctx);
    fs::create_dir_all(root.join("empty")).unwrap();
    mount_push_mocks(&ctx, "alice/proj").await;

    let result = {
        let _cwd = CwdGuard::enter(&root);
        cmd_push(&ctx.config, &ctx.output, &push_args(Some("empty".into()))).await
    };

    match result {
        Err(CliError::PushEmpty { path, .. }) => assert!(
            path.ends_with("empty"),
            "the refusal named the repository root rather than the scoped directory: {path}"
        ),
        other => panic!("expected PushEmpty, got {other:?}"),
    }
}

// ---- round 3: a relative path argument, and a mixed-case marker --------

/// The trigger's own `## Expected` sanctions `syns push ./sub`, arguing
/// the parallel with `cd repo/sub && git add .`. Every relative spelling
/// refused with `REPO_IDENTITY_UNKNOWN` from any directory below the
/// repository root, because the identity walk climbed a relative path to
/// the empty component and stopped.
#[tokio::test(flavor = "current_thread")]
#[serial]
async fn push_scoped_by_a_relative_dot_from_a_subdirectory_scopes_that_subtree() {
    let ctx = setup().await;
    seed_credentials(&ctx, "test-token", "alice");
    let root = seed_tree(&ctx);
    mount_push_mocks(&ctx, "alice/proj").await;

    {
        let _cwd = CwdGuard::enter(&root.join("sub"));
        cmd_push(&ctx.config, &ctx.output, &push_args(Some(".".into())))
            .await
            .expect("a relative path argument must resolve the repository above it");
    }

    let paths = body_paths(&last_push_body(&ctx).await);
    assert_eq!(paths, vec!["sub/nested.md".to_string()], "{paths:?}");
}

#[tokio::test(flavor = "current_thread")]
#[serial]
async fn push_scoped_by_a_relative_child_from_a_subdirectory_scopes_that_subtree() {
    let ctx = setup().await;
    seed_credentials(&ctx, "test-token", "alice");
    let root = seed_tree(&ctx);
    fs::create_dir_all(root.join("a/b")).unwrap();
    fs::write(root.join("a/b/deep.md"), "d").unwrap();
    mount_push_mocks(&ctx, "alice/proj").await;

    {
        let _cwd = CwdGuard::enter(&root.join("a"));
        cmd_push(&ctx.config, &ctx.output, &push_args(Some("b".into())))
            .await
            .expect("a relative path argument must resolve the repository above it");
    }

    let paths = body_paths(&last_push_body(&ctx).await);
    assert_eq!(paths, vec!["a/b/deep.md".to_string()], "{paths:?}");
}

/// The severe shape: under `--if-repo` the same refusal became a silent
/// exit `0` that published nothing, telling the user nothing and losing
/// the operation — the zero-detectability failure the trigger reports.
#[tokio::test(flavor = "current_thread")]
#[serial]
async fn push_scoped_by_a_relative_path_under_if_repo_reaches_the_server() {
    let ctx = setup().await;
    seed_credentials(&ctx, "test-token", "alice");
    let root = seed_tree(&ctx);
    mount_push_mocks(&ctx, "alice/proj").await;

    {
        let _cwd = CwdGuard::enter(&root.join("sub"));
        let mut args = push_args(Some(".".into()));
        args.if_repo = true;
        cmd_push(&ctx.config, &ctx.output, &args).await.unwrap();
    }

    let requests = ctx.mock_server.received_requests().await.unwrap();
    assert!(
        requests.iter().any(|r| r.method == reqwest::Method::PUT),
        "--if-repo silently skipped a publication whose identity file stands above it"
    );
    let paths = body_paths(&last_push_body(&ctx).await);
    assert_eq!(paths, vec!["sub/nested.md".to_string()], "{paths:?}");
}

#[tokio::test(flavor = "current_thread")]
#[serial]
async fn pull_into_a_relative_destination_resolves_the_repository_above_it() {
    let ctx = setup().await;
    seed_credentials(&ctx, "test-token", "alice");
    let root = ctx.project_dir.path().to_path_buf();
    fs::create_dir_all(root.join("sub")).unwrap();
    fs::write(root.join(".syns.yaml"), "owner: alice\nname: proj\n").unwrap();
    mount_pull_mocks(&ctx, "alice/proj", server_tree()).await;

    {
        let _cwd = CwdGuard::enter(&root.join("sub"));
        cmd_pull(
            &ctx.config,
            &ctx.output,
            None,
            Some("dest".into()),
            None,
            false,
            false,
            None,
        )
        .await
        .expect("a relative destination must resolve the repository above it");
    }

    assert!(root.join("sub/dest/root-a.md").is_file());
}

/// `resolve_repo_identity` lower-cases a `--name` value and returned an
/// identity file's pair verbatim, so a marker naming `Alice/Proj`
/// addressed a repository the server answers `422` for.
#[tokio::test(flavor = "current_thread")]
#[serial]
async fn a_mixed_case_identity_file_addresses_the_lower_cased_repository() {
    let ctx = setup().await;
    seed_credentials(&ctx, "test-token", "alice");
    let root = seed_tree(&ctx);
    fs::write(root.join(".syns.yaml"), "owner: Alice\nname: Proj\n").unwrap();
    mount_push_mocks(&ctx, "alice/proj").await;

    {
        let _cwd = CwdGuard::enter(&root.join("sub"));
        cmd_push(&ctx.config, &ctx.output, &push_args(None))
            .await
            .unwrap();
    }

    let requests = ctx.mock_server.received_requests().await.unwrap();
    assert!(
        requests
            .iter()
            .any(|r| r.url.path() == "/api/v1/repos/alice/proj/push"),
        "the publication addressed a repository the server holds under another spelling"
    );
}

/// u263 (issue 130), inverting the u256-era retrieval that mixed a
/// repository into the working directory whose own identity file names
/// another: the run is refused before any request, and that file — with
/// the checks it declares — stands at its bytes.
#[tokio::test(flavor = "current_thread")]
#[serial]
async fn pull_of_another_repository_where_an_identity_file_stands_refuses() {
    let ctx = setup().await;
    seed_credentials(&ctx, "test-token", "alice");
    let root = fs::canonicalize(ctx.project_dir.path()).unwrap();
    let standing = "owner: bob\nname: other\nchecks:\n  - make lint\n";
    fs::write(root.join(".syns.yaml"), standing).unwrap();
    mount_pull_mocks(&ctx, "alice/proj", server_tree()).await;

    let result = {
        let _cwd = CwdGuard::enter(&root);
        cmd_pull(
            &ctx.config,
            &ctx.output,
            Some(("alice".into(), "proj".into())),
            None,
            None,
            false,
            false,
            None,
        )
        .await
    };

    assert!(
        matches!(
            &result,
            Err(CliError::PathBelongsToAnotherRepository { path, .. }) if *path == root
        ),
        "{result:?}"
    );
    assert!(
        ctx.mock_server
            .received_requests()
            .await
            .unwrap()
            .is_empty()
    );
    assert!(!root.join("root-a.md").exists());
    assert_eq!(
        fs::read_to_string(root.join(".syns.yaml")).unwrap(),
        standing
    );
}

// ---- u309: a publication naming another repository than its folder's --

/// SPEC u309 Tests' `F`, in its canonical form, and `I`, the bytes its
/// identity file holds before the run.
fn seed_owned_folder(ctx: &TestContext) -> (PathBuf, Vec<u8>) {
    let folder = fs::canonicalize(ctx.project_dir.path()).unwrap();
    let identity = b"owner: alice\nname: proj\n".to_vec();
    fs::write(folder.join(".syns.yaml"), &identity).unwrap();
    fs::write(folder.join("a.md"), "a\n").unwrap();
    fs::create_dir_all(folder.join("sub")).unwrap();
    fs::write(folder.join("sub/b.md"), "b\n").unwrap();
    (folder, identity)
}

/// Every file under `dir`, relative to it, `/`-separated and sorted.
fn files_under(dir: &Path) -> Vec<String> {
    fn walk(dir: &Path, out: &mut Vec<PathBuf>) {
        for entry in fs::read_dir(dir).into_iter().flatten().flatten() {
            let path = entry.path();
            if path.is_dir() {
                walk(&path, out);
            } else {
                out.push(path);
            }
        }
    }
    let mut found = Vec::new();
    walk(dir, &mut found);
    let mut out: Vec<String> = found
        .iter()
        .map(|p| {
            p.strip_prefix(dir)
                .unwrap()
                .to_string_lossy()
                .replace('\\', "/")
        })
        .collect();
    out.sort();
    out
}

fn assert_refused_as_another_repository(
    result: &Result<(), CliError>,
    folder: &Path,
    standing: &str,
    requested: &str,
) {
    match result {
        Err(CliError::PathBelongsToAnotherRepository {
            path,
            standing: s,
            requested: r,
            remedy,
        }) => {
            assert_eq!(path, folder);
            assert_eq!(s, standing);
            assert_eq!(r, requested);
            assert_eq!(*remedy, BelongsRemedy::Push);
        }
        other => panic!("expected the publication's path-belongs refusal, got {other:?}"),
    }
}

async fn assert_no_request(ctx: &TestContext) {
    let requests = ctx.mock_server.received_requests().await.unwrap();
    assert!(
        requests.is_empty(),
        "the refused publication sent {:?}",
        requests
            .iter()
            .map(|r| format!("{} {}", r.method, r.url.path()))
            .collect::<Vec<_>>()
    );
}

/// SPEC u309 Contract Surface, `cmd_push`: a refused publication opens no
/// working copy of the repository it named (CR1-1).
fn assert_no_working_copy(ctx: &TestContext, owner: &str) {
    let opened = ctx
        .config
        .stores()
        .default
        .join("working-copies")
        .join(owner);
    assert!(
        !opened.exists(),
        "the refused publication opened a working copy at {}",
        opened.display()
    );
}

// SPEC u309 Tests, the row of this name: issue 231's reproduction.
#[tokio::test(flavor = "current_thread")]
#[serial]
async fn forced_push_naming_another_repository_refuses_before_any_request() {
    let ctx = setup().await;
    seed_credentials(&ctx, "test-token", "alice");
    let (folder, before) = seed_owned_folder(&ctx);
    mount_push_mocks(&ctx, "bob/other").await;

    let result = {
        let _cwd = CwdGuard::enter(&folder);
        let mut args = push_args(None);
        args.force = true;
        args.name = Some("bob/other".into());
        cmd_push(&ctx.config, &ctx.output, &args).await
    };

    assert_refused_as_another_repository(&result, &folder, "alice/proj", "bob/other");
    assert_no_request(&ctx).await;
    assert_no_working_copy(&ctx, "bob");
    assert_eq!(fs::read(folder.join(".syns.yaml")).unwrap(), before);
}

// SPEC u309 Tests, the row of this name.
#[tokio::test(flavor = "current_thread")]
#[serial]
async fn push_scoped_to_a_file_beside_another_repositorys_identity_refuses() {
    let ctx = setup().await;
    seed_credentials(&ctx, "test-token", "alice");
    let (folder, _before) = seed_owned_folder(&ctx);
    mount_push_mocks(&ctx, "bob/other").await;

    let mut args = push_args(Some(folder.join("a.md")));
    args.name = Some("bob/other".into());
    let result = cmd_push(&ctx.config, &ctx.output, &args).await;

    assert_refused_as_another_repository(&result, &folder, "alice/proj", "bob/other");
    assert_no_request(&ctx).await;
}

// SPEC u309 Tests, the row of this name.
#[tokio::test(flavor = "current_thread")]
#[serial]
async fn bare_push_naming_another_repository_refuses_and_converges_nothing() {
    let ctx = setup().await;
    seed_credentials(&ctx, "test-token", "alice");
    let (folder, before) = seed_owned_folder(&ctx);
    mount_push_mocks(&ctx, "bob/other").await;

    let result = {
        let _cwd = CwdGuard::enter(&folder);
        let mut args = push_args(None);
        args.name = Some("bob/other".into());
        cmd_push(&ctx.config, &ctx.output, &args).await
    };

    assert_refused_as_another_repository(&result, &folder, "alice/proj", "bob/other");
    assert_no_request(&ctx).await;
    assert_no_working_copy(&ctx, "bob");
    assert_eq!(files_under(&folder), [".syns.yaml", "a.md", "sub/b.md"]);
    assert_eq!(fs::read(folder.join(".syns.yaml")).unwrap(), before);
}

// SPEC u309 Tests, the row of this name.
#[tokio::test(flavor = "current_thread")]
#[serial]
async fn bare_name_resolving_another_owner_refuses() {
    let ctx = setup().await;
    seed_credentials(&ctx, "test-token", "bob");
    let (folder, _before) = seed_owned_folder(&ctx);
    mount_push_mocks(&ctx, "bob/proj").await;

    let result = {
        let _cwd = CwdGuard::enter(&folder);
        let mut args = push_args(None);
        args.force = true;
        args.name = Some("proj".into());
        cmd_push(&ctx.config, &ctx.output, &args).await
    };

    assert_refused_as_another_repository(&result, &folder, "alice/proj", "bob/proj");
    assert_no_request(&ctx).await;
}

// SPEC u309 Tests, the row of this name: the session read resolving the
// owner is the one request preceding the refusal.
#[tokio::test(flavor = "current_thread")]
#[serial]
async fn bare_name_on_a_credential_storing_no_username_refuses_after_the_session_read() {
    let ctx = setup().await;
    TokenStore::new(ctx.config.credentials_path())
        .write("test-token")
        .unwrap();
    let (folder, _before) = seed_owned_folder(&ctx);
    Mock::given(method("GET"))
        .and(path_matcher("/api/auth/get-session"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "user": {
                "id": "user-id-bob",
                "name": "Bob",
                "username": "bob",
                "email": "bob@example.com",
                "emailVerified": true,
                "image": null,
                "createdAt": "2026-01-01T00:00:00.000Z",
                "updatedAt": "2026-01-01T00:00:00.000Z"
            },
            "session": {
                "id": "session-id-bob",
                "userId": "user-id-bob",
                "expiresAt": "2026-12-31T23:59:59.000Z"
            }
        })))
        .mount(&ctx.mock_server)
        .await;
    mount_push_mocks(&ctx, "bob/proj").await;

    let result = {
        let _cwd = CwdGuard::enter(&folder);
        let mut args = push_args(None);
        args.force = true;
        args.name = Some("proj".into());
        cmd_push(&ctx.config, &ctx.output, &args).await
    };

    assert_refused_as_another_repository(&result, &folder, "alice/proj", "bob/proj");
    let requests: Vec<String> = ctx
        .mock_server
        .received_requests()
        .await
        .unwrap()
        .iter()
        .map(|r| format!("{} {}", r.method, r.url.path()))
        .collect();
    assert_eq!(requests, ["GET /api/auth/get-session"]);
}

// SPEC u309 Tests, the row of this name.
#[tokio::test(flavor = "current_thread")]
#[serial]
async fn push_beside_a_marked_identity_file_judges_its_local_side() {
    let ctx = setup().await;
    seed_credentials(&ctx, "test-token", "alice");
    let (folder, _before) = seed_owned_folder(&ctx);
    let marked = "<<<<<<< local\nowner: alice\nname: proj\n=======\nowner: bob\nname: other\n>>>>>>> remote\n";
    fs::write(folder.join(".syns.yaml"), marked).unwrap();
    mount_push_mocks(&ctx, "bob/other").await;

    let mut args = push_args(Some(folder.clone()));
    args.force = true;
    args.name = Some("bob/other".into());
    let result = cmd_push(&ctx.config, &ctx.output, &args).await;

    assert_refused_as_another_repository(&result, &folder, "alice/proj", "bob/other");
    assert_no_request(&ctx).await;
    assert_eq!(
        fs::read_to_string(folder.join(".syns.yaml")).unwrap(),
        marked
    );
}

// SPEC u309 Tests, the row of this name: `D-025`'s letter-case rule
// keeps a folder whose identity file spells its repository otherwise
// publishing as before.
#[tokio::test(flavor = "current_thread")]
#[serial]
async fn push_naming_the_folders_repository_in_another_case_publishes() {
    let ctx = setup().await;
    seed_credentials(&ctx, "test-token", "alice");
    let (folder, _before) = seed_owned_folder(&ctx);
    fs::write(folder.join(".syns.yaml"), "owner: Alice\nname: Proj\n").unwrap();
    mount_push_mocks(&ctx, "alice/proj").await;

    {
        let _cwd = CwdGuard::enter(&folder);
        let mut args = push_args(None);
        args.force = true;
        args.name = Some("alice/proj".into());
        cmd_push(&ctx.config, &ctx.output, &args).await.unwrap();
    }

    let pushes: Vec<serde_json::Value> = ctx
        .mock_server
        .received_requests()
        .await
        .unwrap()
        .iter()
        .filter(|r| r.method == reqwest::Method::PUT && r.url.path().ends_with("/push"))
        .map(|r| {
            assert_eq!(r.url.path(), "/api/v1/repos/alice/proj/push");
            serde_json::from_slice(&r.body).unwrap()
        })
        .collect();
    assert_eq!(pushes.len(), 1, "one publication request");
    assert!(
        body_paths(&pushes[0]).contains(&"a.md".to_string()),
        "{:?}",
        body_paths(&pushes[0])
    );
    assert_eq!(
        fs::read_to_string(folder.join(".syns.yaml")).unwrap(),
        "owner: Alice\nname: Proj\n"
    );
}
