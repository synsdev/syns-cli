//! `syns rm PATH --parent REF` — one delete-only commit (SPEC u271).
//!
//! The path is absent from the head afterwards and every path outside it
//! survives. A `PATH` the parent's tree holds as a directory takes every
//! path beneath it out of the repository in that one commit while the
//! answer counts the entry removed rather than the files under it, and a
//! path the parent's tree does not hold leaves the head where it stood
//! at exit `0` — both measured in `units/cli/u271/prototype/`.

use crate::config::Config;
use crate::errors::CliError;
use crate::output::Output;
use crate::write::{
    Changeset, WriteOptions, commit_changeset, default_message, resolve_write_target,
};

pub async fn cmd_rm(
    config: &Config,
    output: &Output,
    path: String,
    opts: WriteOptions,
) -> Result<(), CliError> {
    // 1 — resolve the write target, which has already read the
    // repository by the time it answers.
    let cwd = std::env::current_dir().map_err(|e| CliError::Io {
        message: format!("could not determine current directory: {e}"),
    })?;
    let target = resolve_write_target(config, &cwd, &opts).await?;

    // 2 — one path as a deletion and no file.
    let changeset = Changeset {
        files: Vec::new(),
        deletions: vec![path.clone()],
    };

    // 3 — commit it.
    let message = default_message("rm", Some(&path));
    commit_changeset(config, output, &target, changeset, &opts, &message).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::token::TokenStore;
    use serial_test::serial;
    use wiremock::matchers::{method, path as path_matcher};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    const HEAD_SHA: &str = "aa11bb22cc33dd44ee55ff6600778899001122bb";

    // SPEC u271 Behaviour, `cmd_rm` 2: the body names that one path
    // under `deletions` and carries no file, at the pinned parent.
    #[tokio::test]
    #[serial]
    async fn the_body_names_one_deletion_and_no_file() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path_matcher("/api/v1/repos/alice/notes"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "owner": "alice", "name": "notes", "description": null,
                "commitSha": HEAD_SHA, "status": "active", "author": null, "tags": [],
                "visibility": "public", "forkedFrom": null, "forkCount": 0,
                "fileCount": 1, "role": null,
                "createdAt": "2026-01-01T00:00:00Z", "updatedAt": "2026-01-01T00:00:00Z",
            })))
            .mount(&server)
            .await;
        Mock::given(method("PUT"))
            .and(path_matcher("/api/v1/repos/alice/notes/push"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "commitSha": "b".repeat(40), "version": 8,
                "filesChanged": 1, "created": false,
            })))
            .mount(&server)
            .await;

        let home = tempfile::tempdir().unwrap();
        let cache = tempfile::tempdir().unwrap();
        // A directory of this test's own: a sibling case may have left
        // the process standing in one since unlinked.
        let work = tempfile::tempdir().unwrap();
        std::env::set_current_dir(work.path()).unwrap();
        unsafe { std::env::set_var("SYNS_CONFIG_DIR", home.path()) };
        unsafe { std::env::set_var("SYNS_CACHE_DIR", cache.path()) };
        let config = Config::new(Some(&server.uri())).unwrap();
        TokenStore::new(config.credentials_path())
            .write_with_username("test-token", Some("alice"))
            .unwrap();
        unsafe { std::env::remove_var("SYNS_CONFIG_DIR") };
        unsafe { std::env::remove_var("SYNS_CACHE_DIR") };

        cmd_rm(
            &config,
            &Output::new(true),
            "a.md".to_string(),
            WriteOptions {
                repo: Some("alice/notes".to_string()),
                parent: HEAD_SHA.to_string(),
                message: None,
                provenance: Default::default(),
            },
        )
        .await
        .unwrap();

        let requests = server.received_requests().await.unwrap();
        let push = requests
            .iter()
            .find(|r| r.url.path().ends_with("/push"))
            .expect("one push");
        let body: serde_json::Value = push.body_json().unwrap();
        assert_eq!(body["deletions"], serde_json::json!([{"path": "a.md"}]));
        assert_eq!(body["files"], serde_json::json!([]));
        assert_eq!(body["parentSha"], serde_json::json!(HEAD_SHA));
        assert_eq!(body["message"], serde_json::json!("rm a.md"));
        // The repository read stands ahead of the push.
        assert_eq!(requests.len(), 2);
        assert!(requests[0].url.path().ends_with("/repos/alice/notes"));
    }
}
