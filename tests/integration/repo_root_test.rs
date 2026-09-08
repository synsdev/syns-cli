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
use syns_cli::commands::pull::cmd_pull;
use syns_cli::commands::push::{PushArgs, cmd_push};
use syns_cli::errors::CliError;
use syns_cli::push::hash::blob_sha1;
use syns_cli::push::manifest::Manifest;
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
        .save(ctx.config.cache_dir(), "alice", "proj")
        .unwrap();
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

    let record = Manifest::load(ctx.config.cache_dir(), "alice", "proj").expect("record");
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

    let record = Manifest::load(ctx.config.cache_dir(), "alice", "proj").expect("record");
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

    let record = Manifest::load(ctx.config.cache_dir(), "alice", "proj").expect("record");
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
            "/api/v1/repos/{repo_id}/files/root-a.md"
        )))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "content": "a", "sha": blob_sha1(b"a"), "size": 1
        })))
        .mount(&ctx.mock_server)
        .await;
    Mock::given(method("GET"))
        .and(path_matcher(format!(
            "/api/v1/repos/{repo_id}/files/sub/nested.md"
        )))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "content": "n", "sha": blob_sha1(b"n"), "size": 1
        })))
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
        cmd_pull(&ctx.config, &ctx.output, None, None, None, false)
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

    let mut manifest = Manifest::default();
    manifest.update(
        "1111111111111111111111111111111111111111".to_string(),
        HashMap::from([("root-b.md".to_string(), blob_sha1(b"b"))]),
    );
    manifest
        .save(ctx.config.cache_dir(), "alice", "proj")
        .unwrap();

    mount_pull_mocks(&ctx, "alice/proj", server_tree()).await;

    {
        let _cwd = CwdGuard::enter(&root.join("sub"));
        cmd_pull(&ctx.config, &ctx.output, None, None, None, false)
            .await
            .unwrap();
    }

    assert!(
        !root.join("root-b.md").exists(),
        "the reconciled removal was not taken at the repository root"
    );
}

#[tokio::test(flavor = "current_thread")]
#[serial]
async fn pull_of_another_repository_writes_its_own_identity_file() {
    let ctx = setup().await;
    seed_credentials(&ctx, "test-token", "alice");
    let root = ctx.project_dir.path().to_path_buf();
    fs::create_dir_all(root.join("sub")).unwrap();
    fs::write(root.join(".syns.yaml"), "owner: alice\nname: proj\n").unwrap();
    mount_pull_mocks(&ctx, "other/repo", server_tree()).await;

    {
        let _cwd = CwdGuard::enter(&root.join("sub"));
        cmd_pull(
            &ctx.config,
            &ctx.output,
            Some("other/repo".into()),
            None,
            None,
            false,
        )
        .await
        .unwrap();
    }

    assert!(
        root.join("sub/root-a.md").is_file(),
        "the retrieval did not write under sub/"
    );
    let marker = fs::read_to_string(root.join("sub/.syns.yaml")).expect("sub/.syns.yaml");
    assert!(marker.contains("other"), "{marker}");
    assert!(marker.contains("repo"), "{marker}");
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
    fs::write(root.join(".gitignore"), "*\n").unwrap();
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
    fs::write(root.join(".gitignore"), "*\n").unwrap();
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

#[tokio::test(flavor = "current_thread")]
#[serial]
async fn push_naming_another_repository_leaves_a_standing_identity_file_alone() {
    let ctx = setup().await;
    seed_credentials(&ctx, "test-token", "alice");
    let root = seed_tree(&ctx);
    mount_push_mocks(&ctx, "bob/other").await;

    let mut args = push_args(Some(root.clone()));
    args.name = Some("bob/other".into());
    cmd_push(&ctx.config, &ctx.output, &args).await.unwrap();

    let marker = fs::read_to_string(root.join(".syns.yaml")).unwrap();
    assert_eq!(
        marker, "owner: alice\nname: proj\n",
        "the publication overwrote the identity file standing at its content root"
    );
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
            Some("alice/proj".into()),
            Some("dest".into()),
            None,
            false,
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
