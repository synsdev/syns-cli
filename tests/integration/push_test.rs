use crate::common::{seed_credentials, setup};
use serde_json::json;
use serial_test::serial;
use std::collections::HashMap;
use std::fs;
use syns_cli::client::SynsClient;
use syns_cli::commands::push::{PushArgs, cmd_push};
use syns_cli::push::hash::blob_sha1;
use syns_cli::push::manifest::Manifest;
use wiremock::matchers::{method, path, query_param};
use wiremock::{Mock, ResponseTemplate};

#[tokio::test(flavor = "current_thread")]
#[serial]
async fn push_creates_repo_and_sends_files() {
    let ctx = setup().await;
    seed_credentials(&ctx, "test-token-abc", "alice");

    fs::write(ctx.project_dir.path().join("hello.txt"), "hello world").unwrap();
    fs::write(
        ctx.project_dir.path().join(".syns.yaml"),
        "owner: alice\nname: new-repo\n",
    )
    .unwrap();

    Mock::given(method("GET"))
        .and(path("/api/v1/repos/alice/new-repo/tree"))
        .and(query_param("recursive", "true"))
        .respond_with(ResponseTemplate::new(404).set_body_json(json!({
            "error": "repo_not_found",
            "message": "Repository not found"
        })))
        .mount(&ctx.mock_server)
        .await;

    Mock::given(method("PUT"))
        .and(path("/api/v1/repos/alice/new-repo/push"))
        .respond_with(ResponseTemplate::new(201).set_body_json(json!({
            "commitSha": "aabbccddee00112233445566778899aabbccddee",
            "version": 1,
            "filesChanged": 1,
            "created": true
        })))
        .expect(1)
        .mount(&ctx.mock_server)
        .await;

    let args = PushArgs {
        name: None,
        path: Some(ctx.project_dir.path().to_path_buf()),
        message: None,
        force: false,
        exclude: vec![],
        description: None,
        tag: vec![],
        status: None,
        visibility: None,
    };
    let result = cmd_push(&ctx.config, &ctx.output, &args).await;
    assert!(result.is_ok());
}

#[tokio::test(flavor = "current_thread")]
#[serial]
async fn push_sends_only_changed_files() {
    let ctx = setup().await;
    seed_credentials(&ctx, "test-token-abc", "bob");

    fs::write(ctx.project_dir.path().join("a.txt"), "unchanged").unwrap();
    fs::write(ctx.project_dir.path().join("b.txt"), "modified").unwrap();
    fs::write(
        ctx.project_dir.path().join(".syns.yaml"),
        "owner: bob\nname: my-repo\n",
    )
    .unwrap();

    let mut manifest = Manifest::default();
    manifest.update(
        "1111111111111111111111111111111111111111".to_string(),
        HashMap::from([
            ("a.txt".to_string(), blob_sha1(b"unchanged")),
            ("b.txt".to_string(), blob_sha1(b"original")),
        ]),
    );
    manifest
        .save(ctx.config.cache_dir(), "bob", "my-repo")
        .unwrap();

    Mock::given(method("PUT"))
        .and(path("/api/v1/repos/bob/my-repo/push"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "commitSha": "2222222222222222222222222222222222222222",
            "version": 2,
            "filesChanged": 1,
            "created": false
        })))
        .expect(1)
        .mount(&ctx.mock_server)
        .await;

    let args = PushArgs {
        name: None,
        path: Some(ctx.project_dir.path().to_path_buf()),
        message: None,
        force: false,
        exclude: vec![],
        description: None,
        tag: vec![],
        status: None,
        visibility: None,
    };
    let result = cmd_push(&ctx.config, &ctx.output, &args).await;
    assert!(result.is_ok());

    let requests = ctx.mock_server.received_requests().await.unwrap();
    let push_request = requests
        .iter()
        .find(|r| r.url.path() == "/api/v1/repos/bob/my-repo/push")
        .expect("push request not found");
    let body: serde_json::Value = serde_json::from_slice(&push_request.body).unwrap();

    assert_eq!(
        body["parentSha"],
        "1111111111111111111111111111111111111111"
    );

    let files = body["files"].as_array().unwrap();
    let b_entry = files
        .iter()
        .find(|f| f["path"] == "b.txt")
        .expect("b.txt not in files");
    assert!(
        !b_entry["content"].is_null(),
        "b.txt should have content (file changed)"
    );

    let a_entry = files
        .iter()
        .find(|f| f["path"] == "a.txt")
        .expect("a.txt not in files");
    assert!(
        a_entry.get("content").is_none(),
        "a.txt should not have content key (file unchanged)"
    );
}

#[tokio::test(flavor = "current_thread")]
#[serial]
async fn pull_downloads_tree() {
    let ctx = setup().await;
    seed_credentials(&ctx, "test-token-abc", "carol");

    Mock::given(method("GET"))
        .and(path("/api/v1/repos/carol/my-repo/tree"))
        .and(query_param("recursive", "true"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "entries": [
                {"name": "readme.md", "path": "readme.md", "type": "file", "size": 5, "sha": "aaf4c61ddcc5e8a2dabede0f3b482cd9aea9434d"},
                {"name": "src", "path": "src", "type": "dir", "size": null, "sha": null},
                {"name": "main.ts", "path": "src/main.ts", "type": "file", "size": 18, "sha": "7c211433f02024a5b5903e1cd8b1a8a53b6e470c"}
            ],
            "commitSha": "3333333333333333333333333333333333333333",
            "truncated": false
        })))
        .expect(1)
        .mount(&ctx.mock_server)
        .await;

    let client = SynsClient::new(ctx.config.server_url()).unwrap();
    let response = client.pull("carol/my-repo", Some("test-token-abc")).await;

    let response = response.unwrap();
    assert_eq!(response.entries.len(), 3);
    assert_eq!(
        response.commit_sha,
        "3333333333333333333333333333333333333333"
    );
    assert_eq!(response.truncated, false);
}
