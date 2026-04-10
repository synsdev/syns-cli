use std::collections::HashMap;
use std::path::{Path, PathBuf};

use crate::client::{
    EntryType, PushEntry, PushRequest, PushResponse, RepoStatus, SynsClient, TreeResponse,
    Visibility,
};
use crate::errors::CliError;
use crate::push::collector::collect_files;
use crate::push::hash::blob_sha1;
use crate::push::manifest::Manifest;
use crate::repo::syns_yaml::write_syns_yaml;

pub struct SmartPushOptions {
    pub force: bool,
    pub message: String,
    pub author: String,
    pub parent_sha: Option<String>,
    pub excludes: Vec<String>,
    pub cache_dir: PathBuf,
    pub description: Option<String>,
    pub tags: Option<Vec<String>>,
    pub status: Option<RepoStatus>,
    pub visibility: Option<Visibility>,
}

fn split_repo_id(repo_id: &str) -> Result<(&str, &str), CliError> {
    repo_id.split_once('/').ok_or_else(|| CliError::Config {
        message: format!("invalid repo id: {repo_id}"),
    })
}

fn tree_to_sha_map(tree: &TreeResponse) -> HashMap<String, String> {
    tree.entries
        .iter()
        .filter(|e| e.entry_type == EntryType::File && e.sha.is_some())
        .map(|e| (e.path.clone(), e.sha.clone().unwrap()))
        .collect()
}

fn build_push_entries(
    local_files: &HashMap<String, Vec<u8>>,
    local_shas: &HashMap<String, String>,
    reference_shas: &HashMap<String, String>,
    force: bool,
) -> Result<(HashMap<String, PushEntry>, Vec<String>), CliError> {
    let mut entries = HashMap::new();
    let mut deletes = Vec::new();

    for (path, sha) in local_shas {
        let changed = force || reference_shas.get(path).map_or(true, |ref_sha| ref_sha != sha);
        let content = if changed {
            let bytes = &local_files[path];
            let utf8 = String::from_utf8(bytes.clone()).map_err(|_| CliError::Io {
                message: format!("file is not valid UTF-8: {path}"),
            })?;
            Some(utf8)
        } else {
            None
        };
        entries.insert(path.clone(), PushEntry { sha: sha.clone(), content });
    }

    if !force {
        for path in reference_shas.keys() {
            if !local_shas.contains_key(path) {
                deletes.push(path.clone());
            }
        }
    }

    Ok((entries, deletes))
}

fn upgrade_to_full(
    entries: &HashMap<String, PushEntry>,
    local_files: &HashMap<String, Vec<u8>>,
) -> Result<HashMap<String, PushEntry>, CliError> {
    let mut upgraded = HashMap::new();
    for (path, entry) in entries {
        if entry.content.is_some() {
            upgraded.insert(
                path.clone(),
                PushEntry {
                    sha: entry.sha.clone(),
                    content: entry.content.clone(),
                },
            );
        } else {
            let bytes = &local_files[path];
            let utf8 = String::from_utf8(bytes.clone()).map_err(|_| CliError::Io {
                message: format!("file is not valid UTF-8: {path}"),
            })?;
            upgraded.insert(
                path.clone(),
                PushEntry {
                    sha: entry.sha.clone(),
                    content: Some(utf8),
                },
            );
        }
    }
    Ok(upgraded)
}

pub async fn smart_push(
    client: &SynsClient,
    token: &str,
    repo_id: &str,
    path: &Path,
    opts: SmartPushOptions,
) -> Result<PushResponse, CliError> {
    // Phase 1 — Setup
    let (owner, name) = split_repo_id(repo_id)?;

    if !path.join(".syns.yaml").exists() {
        write_syns_yaml(path, owner, name)?;
    }

    let local_files = collect_files(path, &opts.excludes)?;
    let local_shas: HashMap<String, String> = local_files
        .iter()
        .map(|(p, content)| (p.clone(), blob_sha1(content)))
        .collect();

    // Phase 2 — Reference state
    let (reference_shas, base_parent_sha) = if opts.force {
        (HashMap::new(), None)
    } else if let Some(manifest) = Manifest::load(&opts.cache_dir, owner, name) {
        let ref_shas: HashMap<String, String> = manifest
            .file_paths()
            .filter_map(|p| manifest.file_sha(p).map(|sha| (p.to_string(), sha.to_string())))
            .collect();
        let parent = manifest.commit_sha().map(String::from);
        (ref_shas, parent)
    } else {
        match client.pull(repo_id, Some(token)).await {
            Ok(tree) => {
                let parent = Some(tree.commit_sha.clone());
                (tree_to_sha_map(&tree), parent)
            }
            Err(CliError::Api { status: Some(404), .. }) => (HashMap::new(), None),
            Err(e) => return Err(e),
        }
    };

    let parent_sha = if opts.parent_sha.is_some() {
        opts.parent_sha
    } else {
        base_parent_sha
    };

    // Phase 3 — Build and send
    let (entries, deletes) = build_push_entries(&local_files, &local_shas, &reference_shas, opts.force)?;

    let request = PushRequest {
        files: entries,
        delete: deletes,
        message: opts.message.clone(),
        author: opts.author.clone(),
        parent_sha,
        description: opts.description.clone(),
        tags: opts.tags.clone(),
        status: opts.status.clone(),
        visibility: opts.visibility.clone(),
    };

    let response = match client.push(repo_id, token, &request).await {
        Ok(response) => response,
        Err(CliError::Api { status: Some(409), ref error }) if error == "missing_blobs" => {
            let retry_entries = upgrade_to_full(&request.files, &local_files)?;
            let retry_request = PushRequest {
                files: retry_entries,
                delete: request.delete.clone(),
                message: request.message.clone(),
                author: request.author.clone(),
                parent_sha: request.parent_sha.clone(),
                description: request.description.clone(),
                tags: request.tags.clone(),
                status: request.status.clone(),
                visibility: request.visibility.clone(),
            };
            client.push(repo_id, token, &retry_request).await?
        }
        Err(e) => return Err(e),
    };

    // Phase 4 — Manifest save
    let mut manifest = Manifest::default();
    manifest.update(response.commit_sha.clone(), local_shas);
    let _ = manifest.save(&opts.cache_dir, owner, name);

    Ok(response)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::push::hash::blob_sha1;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    #[tokio::test]
    async fn first_push_sends_all_files_with_content() {
        let mock_server = MockServer::start().await;

        Mock::given(method("GET"))
            .and(path("/api/v1/repos/alice/new-repo/tree"))
            .respond_with(
                ResponseTemplate::new(404).set_body_json(serde_json::json!({"error": "not_found"})),
            )
            .mount(&mock_server)
            .await;

        Mock::given(method("PUT"))
            .and(path("/api/v1/repos/alice/new-repo/push"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "commit_sha": "abc123",
                "added": 2,
                "updated": 0,
                "deleted": 0,
                "file_count": 2,
                "changed": true
            })))
            .mount(&mock_server)
            .await;

        let temp_dir = tempfile::tempdir().unwrap();
        std::fs::write(temp_dir.path().join("main.txt"), "hello").unwrap();
        std::fs::create_dir_all(temp_dir.path().join("sub")).unwrap();
        std::fs::write(temp_dir.path().join("sub/other.txt"), "world").unwrap();

        let cache_dir = tempfile::tempdir().unwrap();
        let client = SynsClient::new(&mock_server.uri()).unwrap();

        let result = smart_push(
            &client,
            "test-token",
            "alice/new-repo",
            temp_dir.path(),
            SmartPushOptions {
                force: false,
                message: "init".into(),
                author: "alice".into(),
                parent_sha: None,
                excludes: vec![],
                cache_dir: cache_dir.path().to_path_buf(),
                description: None,
                tags: None,
                status: None,
                visibility: None,
            },
        )
        .await;

        assert!(result.is_ok());
        let response = result.unwrap();
        assert_eq!(response.commit_sha, "abc123");

        // .syns.yaml was auto-created
        let syns_yaml = std::fs::read_to_string(temp_dir.path().join(".syns.yaml")).unwrap();
        assert_eq!(syns_yaml, "owner: alice\nname: new-repo\n");

        // Manifest was saved
        assert!(cache_dir.path().join("alice").join("new-repo.json").exists());

        // Verify request body
        let requests = mock_server.received_requests().await.unwrap();
        let put_request = requests.iter().find(|r| r.method == reqwest::Method::PUT).unwrap();
        let body: serde_json::Value = serde_json::from_slice(&put_request.body).unwrap();

        let files = body["files"].as_object().unwrap();
        assert!(files["main.txt"]["content"].is_string());
        assert!(files["sub/other.txt"]["content"].is_string());
        assert!(body["parent_sha"].is_null());
        assert!(body["delete"].as_array().unwrap().is_empty());
    }

    #[tokio::test]
    async fn subsequent_push_sends_only_changed_files() {
        let mock_server = MockServer::start().await;

        Mock::given(method("PUT"))
            .and(path("/api/v1/repos/bob/my-repo/push"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "commit_sha": "def456",
                "added": 0,
                "updated": 1,
                "deleted": 0,
                "file_count": 2,
                "changed": true
            })))
            .mount(&mock_server)
            .await;

        let temp_dir = tempfile::tempdir().unwrap();
        std::fs::write(temp_dir.path().join("a.txt"), "unchanged").unwrap();
        std::fs::write(temp_dir.path().join("b.txt"), "modified").unwrap();
        std::fs::write(
            temp_dir.path().join(".syns.yaml"),
            "owner: bob\nname: my-repo\n",
        )
        .unwrap();

        let cache_dir = tempfile::tempdir().unwrap();
        let mut manifest = Manifest::default();
        manifest.update(
            "old-sha".into(),
            HashMap::from([
                ("a.txt".into(), blob_sha1(b"unchanged")),
                ("b.txt".into(), blob_sha1(b"original")),
            ]),
        );
        manifest.save(cache_dir.path(), "bob", "my-repo").unwrap();

        let client = SynsClient::new(&mock_server.uri()).unwrap();

        let result = smart_push(
            &client,
            "test-token",
            "bob/my-repo",
            temp_dir.path(),
            SmartPushOptions {
                force: false,
                message: "update".into(),
                author: "bob".into(),
                parent_sha: None,
                excludes: vec![],
                cache_dir: cache_dir.path().to_path_buf(),
                description: None,
                tags: None,
                status: None,
                visibility: None,
            },
        )
        .await;

        assert!(result.is_ok());

        let requests = mock_server.received_requests().await.unwrap();
        let put_request = requests.iter().find(|r| r.method == reqwest::Method::PUT).unwrap();
        let body: serde_json::Value = serde_json::from_slice(&put_request.body).unwrap();

        let files = body["files"].as_object().unwrap();
        assert!(files["a.txt"]["content"].is_null(), "unchanged file should be sha-only");
        assert_eq!(files["b.txt"]["content"].as_str(), Some("modified"));
        assert_eq!(body["parent_sha"].as_str(), Some("old-sha"));
        assert!(body["delete"].as_array().unwrap().is_empty());
    }

    #[tokio::test]
    async fn deleted_files_in_delete_list() {
        let mock_server = MockServer::start().await;

        Mock::given(method("PUT"))
            .and(path("/api/v1/repos/owner/repo/push"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "commit_sha": "del123",
                "added": 0,
                "updated": 0,
                "deleted": 1,
                "file_count": 1,
                "changed": true
            })))
            .mount(&mock_server)
            .await;

        let temp_dir = tempfile::tempdir().unwrap();
        std::fs::write(temp_dir.path().join("keep.txt"), "keep").unwrap();
        std::fs::write(
            temp_dir.path().join(".syns.yaml"),
            "owner: owner\nname: repo\n",
        )
        .unwrap();

        let cache_dir = tempfile::tempdir().unwrap();
        let mut manifest = Manifest::default();
        manifest.update(
            "prev-sha".into(),
            HashMap::from([
                ("keep.txt".into(), blob_sha1(b"keep")),
                ("removed.txt".into(), blob_sha1(b"gone")),
            ]),
        );
        manifest.save(cache_dir.path(), "owner", "repo").unwrap();

        let client = SynsClient::new(&mock_server.uri()).unwrap();

        let result = smart_push(
            &client,
            "test-token",
            "owner/repo",
            temp_dir.path(),
            SmartPushOptions {
                force: false,
                message: "delete".into(),
                author: "owner".into(),
                parent_sha: None,
                excludes: vec![],
                cache_dir: cache_dir.path().to_path_buf(),
                description: None,
                tags: None,
                status: None,
                visibility: None,
            },
        )
        .await;

        assert!(result.is_ok());

        let requests = mock_server.received_requests().await.unwrap();
        let put_request = requests.iter().find(|r| r.method == reqwest::Method::PUT).unwrap();
        let body: serde_json::Value = serde_json::from_slice(&put_request.body).unwrap();

        let deletes: Vec<&str> = body["delete"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap())
            .collect();
        assert_eq!(deletes, vec!["removed.txt"]);

        let files = body["files"].as_object().unwrap();
        assert!(files["keep.txt"]["content"].is_null(), "unchanged file should be sha-only");
    }

    #[tokio::test]
    async fn missing_blobs_409_triggers_retry() {
        let mock_server = MockServer::start().await;

        // 200 response with lower priority (fallback)
        Mock::given(method("PUT"))
            .and(path("/api/v1/repos/owner/repo/push"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "commit_sha": "retry123",
                "added": 1,
                "updated": 1,
                "deleted": 0,
                "file_count": 2,
                "changed": true
            })))
            .with_priority(2)
            .mount(&mock_server)
            .await;

        // 409 response with higher priority, only once
        Mock::given(method("PUT"))
            .and(path("/api/v1/repos/owner/repo/push"))
            .respond_with(
                ResponseTemplate::new(409)
                    .set_body_json(serde_json::json!({"error": "missing_blobs"})),
            )
            .with_priority(1)
            .up_to_n_times(1)
            .mount(&mock_server)
            .await;

        let temp_dir = tempfile::tempdir().unwrap();
        std::fs::write(temp_dir.path().join("a.txt"), "new-a").unwrap();
        std::fs::write(temp_dir.path().join("b.txt"), "old-b").unwrap();
        std::fs::write(
            temp_dir.path().join(".syns.yaml"),
            "owner: owner\nname: repo\n",
        )
        .unwrap();

        let cache_dir = tempfile::tempdir().unwrap();
        let mut manifest = Manifest::default();
        manifest.update(
            "base-sha".into(),
            HashMap::from([
                ("a.txt".into(), blob_sha1(b"old-a")),
                ("b.txt".into(), blob_sha1(b"old-b")),
            ]),
        );
        manifest.save(cache_dir.path(), "owner", "repo").unwrap();

        let client = SynsClient::new(&mock_server.uri()).unwrap();

        let result = smart_push(
            &client,
            "test-token",
            "owner/repo",
            temp_dir.path(),
            SmartPushOptions {
                force: false,
                message: "retry".into(),
                author: "owner".into(),
                parent_sha: None,
                excludes: vec![],
                cache_dir: cache_dir.path().to_path_buf(),
                description: None,
                tags: None,
                status: None,
                visibility: None,
            },
        )
        .await;

        assert!(result.is_ok());

        let requests = mock_server.received_requests().await.unwrap();
        let put_requests: Vec<_> = requests
            .iter()
            .filter(|r| r.method == reqwest::Method::PUT)
            .collect();
        assert_eq!(put_requests.len(), 2, "should have made 2 PUT requests");

        // First request: b.txt should be sha-only (unchanged per manifest)
        let body1: serde_json::Value = serde_json::from_slice(&put_requests[0].body).unwrap();
        assert!(body1["files"]["b.txt"]["content"].is_null());

        // Second request (retry): b.txt should have content (upgraded)
        let body2: serde_json::Value = serde_json::from_slice(&put_requests[1].body).unwrap();
        assert_eq!(body2["files"]["b.txt"]["content"].as_str(), Some("old-b"));
    }

    #[tokio::test]
    async fn force_bypasses_manifest_sends_all() {
        let mock_server = MockServer::start().await;

        Mock::given(method("PUT"))
            .and(path("/api/v1/repos/owner/repo/push"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "commit_sha": "force123",
                "added": 0,
                "updated": 2,
                "deleted": 0,
                "file_count": 2,
                "changed": true
            })))
            .mount(&mock_server)
            .await;

        let temp_dir = tempfile::tempdir().unwrap();
        std::fs::write(temp_dir.path().join("a.txt"), "aaa").unwrap();
        std::fs::write(temp_dir.path().join("b.txt"), "bbb").unwrap();
        std::fs::write(
            temp_dir.path().join(".syns.yaml"),
            "owner: owner\nname: repo\n",
        )
        .unwrap();

        let cache_dir = tempfile::tempdir().unwrap();
        let mut manifest = Manifest::default();
        manifest.update(
            "old-sha".into(),
            HashMap::from([
                ("a.txt".into(), blob_sha1(b"aaa")),
                ("b.txt".into(), blob_sha1(b"bbb")),
            ]),
        );
        manifest.save(cache_dir.path(), "owner", "repo").unwrap();

        let client = SynsClient::new(&mock_server.uri()).unwrap();

        let result = smart_push(
            &client,
            "test-token",
            "owner/repo",
            temp_dir.path(),
            SmartPushOptions {
                force: true,
                message: "force".into(),
                author: "owner".into(),
                parent_sha: None,
                excludes: vec![],
                cache_dir: cache_dir.path().to_path_buf(),
                description: None,
                tags: None,
                status: None,
                visibility: None,
            },
        )
        .await;

        assert!(result.is_ok());

        let requests = mock_server.received_requests().await.unwrap();
        // Only PUT requests, no GET (force doesn't fetch tree)
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0].method, reqwest::Method::PUT);

        let body: serde_json::Value = serde_json::from_slice(&requests[0].body).unwrap();
        let files = body["files"].as_object().unwrap();
        assert!(files["a.txt"]["content"].is_string(), "force should send all content");
        assert!(files["b.txt"]["content"].is_string(), "force should send all content");
        assert!(body["parent_sha"].is_null(), "force defaults parent_sha to None");
        assert!(body["delete"].as_array().unwrap().is_empty());

        // Manifest was still saved
        assert!(cache_dir.path().join("owner").join("repo.json").exists());
    }
}
