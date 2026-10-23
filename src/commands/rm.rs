//! `syns rm PATH --parent REF` — one delete-only commit (SPEC u271).
//!
//! The path is absent from the head afterwards and every path outside it
//! survives. A `PATH` the parent's tree holds as a directory takes every
//! path beneath it out of the repository in that one commit while the
//! answer counts the entry removed rather than the files under it — both
//! measured in `units/cli/u271/prototype/`. A path the parent's tree does
//! not hold is refused under `NOT_FOUND` at exit `1`, naming the path and
//! the parent as `syns edit PATH` names them (issue 160): the server
//! answers that one deletion by publishing no version and counting no
//! changed file, which is the only answer counting none a deletion-only
//! commit can take, so the refusal costs no request beyond the push.
//!
//! A `PATH` empty once trimmed is refused before any request, credential
//! read or folder resolution, inside a scoped folder or not and with or
//! without `--repo`: inside a folder it would name the folder itself, and
//! outside one it names nothing (SPEC u297 Behaviour, `cmd_rm` 1).

use crate::commands::edit::path_not_found_at_parent;
use crate::config::Config;
use crate::errors::CliError;
use crate::output::Output;
use crate::read::refuse_empty_path;
use crate::write::{
    Changeset, WriteOptions, commit_changeset_or, default_message, resolve_write_target,
};

pub async fn cmd_rm(
    config: &Config,
    output: &Output,
    path: String,
    opts: WriteOptions,
) -> Result<(), CliError> {
    // 1 — refuse an empty `PATH` ahead of the `--parent` spelling
    // refusals and of every request.
    refuse_empty_path(&path)?;

    // 2 — resolve the write target, which has already read the
    // repository by the time it answers; compose the one deletion and
    // commit it.
    let cwd = std::env::current_dir().map_err(|e| CliError::Io {
        message: format!("could not determine current directory: {e}"),
    })?;
    let target = resolve_write_target(config, &cwd, &opts).await?;

    // One path as a deletion and no file.
    let changeset = Changeset {
        files: Vec::new(),
        deletions: vec![path.clone()],
    };

    // Commit it; an answer counting no changed file means the parent's
    // tree held no such path, and published nothing.
    let message = default_message("rm", Some(&path));
    let absent = CliError::PathNotFound {
        line: path_not_found_at_parent(&opts.parent, &path),
    };
    commit_changeset_or(
        config,
        output,
        &target,
        changeset,
        &opts,
        &message,
        Some(absent),
    )
    .await
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

    /// A deployment whose push answers `files_changed`, a credential, and a
    /// working directory of the test's own holding no checkout.
    async fn deployment(files_changed: u32) -> (MockServer, Config, [tempfile::TempDir; 3]) {
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
        // An answer counting no changed file stands at the unchanged head,
        // as the server answers a deletion of a path its tree lacks.
        let (sha, version) = match files_changed {
            0 => (HEAD_SHA.to_string(), 7),
            _ => ("b".repeat(40), 8),
        };
        Mock::given(method("PUT"))
            .and(path_matcher("/api/v1/repos/alice/notes/push"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "commitSha": sha, "version": version,
                "filesChanged": files_changed, "created": false,
            })))
            .mount(&server)
            .await;
        let home = tempfile::tempdir().unwrap();
        let cache = tempfile::tempdir().unwrap();
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
        (server, config, [home, cache, work])
    }

    fn at_head(parent: &str) -> WriteOptions {
        WriteOptions {
            repo: Some("alice/notes".to_string()),
            parent: parent.to_string(),
            message: None,
            provenance: Default::default(),
        }
    }

    // Issue 160, step 1: a path the parent's tree does not hold is refused
    // under `not_found` at exit `1`, naming the path and the parent as
    // `syns edit` names them — the head stood still, nothing published.
    #[tokio::test]
    #[serial]
    async fn removing_a_path_the_parent_does_not_hold_is_refused_as_not_found() {
        for typed in ["no-such-file-160.md", "2024-plan.md", "docs/./gone.md"] {
            let (_server, config, _dirs) = deployment(0).await;
            let err = cmd_rm(
                &config,
                &Output::new(true),
                typed.to_string(),
                at_head(HEAD_SHA),
            )
            .await
            .unwrap_err();
            assert!(
                matches!(err, CliError::PathNotFound { .. }),
                "{typed}: {err:?}"
            );
            assert_eq!(err.exit_code(), 1);
            assert_eq!(
                err.json_document(),
                serde_json::json!({
                    "code": "not_found",
                    "error": format!("path not found at version {HEAD_SHA}: {typed}"),
                })
            );
        }
    }

    // Issue 160: a directory, whose deletion the answer counts as the one
    // entry removed, still publishes at exit `0`.
    #[tokio::test]
    #[serial]
    async fn removing_a_directory_the_parent_holds_still_publishes() {
        let (server, config, _dirs) = deployment(1).await;
        cmd_rm(
            &config,
            &Output::new(true),
            "docs".to_string(),
            at_head(HEAD_SHA),
        )
        .await
        .unwrap();
        let requests = server.received_requests().await.unwrap();
        let push = requests
            .iter()
            .find(|r| r.url.path().ends_with("/push"))
            .expect("one push");
        let body: serde_json::Value = push.body_json().unwrap();
        assert_eq!(body["deletions"], serde_json::json!([{"path": "docs"}]));
    }
}
