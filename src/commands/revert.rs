//! `syns revert PATH --to REF --parent REF` — one commit restoring one
//! path's content at an earlier version (issue 160, D-250 §3).
//!
//! It holds the writing verbs' terms (SPEC u271 `WriteOptions`): the
//! repository bound by `--repo` or from the directory, the checkout
//! guard (`CHECKOUT_UNPUBLISHED_WORK`) before any request but the
//! guard's own, the credential, the repository read, and the parent
//! pinned to a full commit hash — sent as `EP-revert-file`'s `parentSha`
//! beside the provenance block `EP-push` takes, so a head that moved past
//! the parent is refused at exit `7` naming `currentSha`. `--if-repo`
//! skips a run where no `--repo` stands and no identity resolves.

use crate::client::{PushResponse, RevertFileRequest, RevertResponse, SynsClient};
use crate::commands::edit::path_not_found_at_parent;
use crate::config::Config;
use crate::errors::CliError;
use crate::output::Output;
use crate::read::{refuse_empty_path, version_not_found_refusal};
use crate::repo::folder::resolve_scoped_or_skip;
use crate::write::{
    FolderSend, WriteOptions, WriteTarget, checkout_behind_line, checkout_left_behind,
    fold_conflict, given_message, resolve_write_target, send_inside_folder, with_checkout_behind,
};

/// `EP-revert-file`'s one request, its parent set at each send (issue
/// 160, REVIEW-syns-cli M1): inside a folder it is sent as the write
/// verbs send their body, through `send_inside_folder`.
struct RevertSend {
    path: String,
    request: RevertFileRequest,
}

impl FolderSend for RevertSend {
    type Answer = (RevertResponse, serde_json::Value);

    async fn send(
        &mut self,
        client: &SynsClient,
        target: &WriteTarget,
        parent: &str,
    ) -> Result<Self::Answer, CliError> {
        self.request.parent_sha = Some(parent.to_string());
        client
            .revert_file(&target.repo_id, &target.token, &self.path, &self.request)
            .await
    }
}

/// The revert's refusal (issue 160, REVIEW-syns-cli M2), by the `404`'s
/// served message: the repository-not-found refusal, the version-not-found
/// refusal naming `--to`, or the path-not-found refusal naming the path at
/// `--to`; a moved head the conflict refusal at exit `7`.
fn revert_refusal(err: CliError, target: &WriteTarget, to: &str, path: &str) -> CliError {
    if err.names_missing_repository() {
        return err.repo_or(&target.repo_id);
    }
    if err.names_missing_version() {
        return err.with_version_not_found_context(version_not_found_refusal(to));
    }
    match &err {
        CliError::Api {
            status: Some(404),
            error,
            ..
        } if error == "not_found" => {
            err.with_versioned_read_context(path_not_found_at_parent(to, path))
        }
        _ => fold_conflict(err, &target.parent.commit_sha),
    }
}

pub async fn cmd_revert(
    config: &Config,
    output: &Output,
    path: String,
    to: String,
    if_repo: bool,
    opts: WriteOptions,
) -> Result<(), CliError> {
    // 1 — the refusals the caller alone earned, before any request.
    refuse_empty_path(&path)?;
    let message = given_message(&opts)?;
    let cwd = std::env::current_dir().map_err(|e| CliError::Io {
        message: format!("could not determine current directory: {e}"),
    })?;

    // 2 — `--if-repo` with no `--repo`: no identity resolving skips the
    // run with the skip envelope, as it always has.
    if if_repo && opts.repo.is_none() && resolve_scoped_or_skip(&cwd, true, output)?.is_none() {
        return Ok(());
    }

    // 3 — the write target: the folder, the repository, the credential,
    // the checkout guard, the repository read and the pinned parent.
    let target = resolve_write_target(config, &cwd, &opts).await?;

    // SPEC u291: inside a folder the positional is counted from it, a
    // path leaving the folder refused before the revert.
    let mapped = target.repository_path(&path)?;

    // 4 — the one revert under the provenance the run asserts: at the
    // pinned parent outside a folder, and inside one as the write verbs
    // send (SPEC u292).
    let client = SynsClient::new(config.server_url())?;
    let mut sender = RevertSend {
        path: mapped,
        request: RevertFileRequest {
            to: to.clone(),
            message,
            parent_sha: None,
            provenance: opts.provenance.block(),
        },
    };
    let answer = match &target.folder {
        None => sender
            .send(&client, &target, &target.parent.commit_sha)
            .await
            .map_err(|err| fold_conflict(err, &target.parent.commit_sha)),
        Some(folder) => send_inside_folder(&client, &target, folder, &mut sender)
            .await
            .map(|(answer, _claimed)| answer),
    };
    let (response, raw) = answer.map_err(|err| revert_refusal(err, &target, &to, &path))?;

    // 5 — the answer, naming the checkout the revert left one version
    // behind as the write verbs name it (REVIEW-syns-cli L1).
    let published = PushResponse {
        commit_sha: response.commit_sha.clone(),
        version: response.version,
        files_changed: response.files_changed,
        created: response.created,
    };
    let written = [path.clone()];
    if output.is_json() {
        output.json(&with_checkout_behind(raw, &target, &published, &written));
        return Ok(());
    }
    if response.files_changed > 0 {
        output.success(&format!(
            "Reverted {} to {} (commit {})",
            path,
            to,
            &response.commit_sha[..response.commit_sha.len().min(8)]
        ));
        if let Some(root) = checkout_left_behind(&target, &published, &written) {
            eprintln!("{}", checkout_behind_line(root));
        }
    } else {
        output.success(&format!(
            "File {} already matches version {}, no changes made",
            path, to
        ));
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::token::TokenStore;
    use crate::write::ProvenanceOptions;
    use serial_test::serial;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    const PARENT: &str = "aa11bb22cc33dd44ee55ff6600778899001122bb";
    const MOVED: &str = "cc11bb22cc33dd44ee55ff6600778899001122cc";

    fn repo_body() -> serde_json::Value {
        serde_json::json!({
            "owner": "alice", "name": "notes", "description": null,
            "commitSha": PARENT, "status": "active", "author": null, "tags": [],
            "visibility": "public", "forkedFrom": null, "forkCount": 0,
            "fileCount": 1, "role": null,
            "createdAt": "2026-01-01T00:00:00Z", "updatedAt": "2026-01-01T00:00:00Z",
        })
    }

    /// A config directory holding a credential, a cache directory, and a
    /// working directory of the test's own holding nothing — no checkout.
    struct Setup {
        _home: tempfile::TempDir,
        _cache: tempfile::TempDir,
        work: tempfile::TempDir,
        config: Config,
    }

    fn setup(server: &MockServer) -> Setup {
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
        Setup {
            _home: home,
            _cache: cache,
            work,
            config,
        }
    }

    fn opts(repo: Option<&str>) -> WriteOptions {
        WriteOptions {
            repo: repo.map(str::to_string),
            parent: PARENT.to_string(),
            message: None,
            provenance: ProvenanceOptions {
                integration: Some("syns-bb-plugin".to_string()),
                run: Some("run-1".to_string()),
                trigger: Some("thread-page".to_string()),
                task_ref: Some("T-7".to_string()),
            },
        }
    }

    async fn mount_repo(server: &MockServer) {
        Mock::given(method("GET"))
            .and(path("/api/v1/repos/alice/notes"))
            .respond_with(ResponseTemplate::new(200).set_body_json(repo_body()))
            .mount(server)
            .await;
    }

    // Issue 160, steps 8, 9 and 12: from a directory holding no checkout,
    // `--repo` names the repository, and the request carries the pinned
    // parent and the provenance block exactly as the push request's.
    #[tokio::test]
    #[serial]
    async fn revert_through_repo_sends_the_parent_and_the_provenance() {
        let server = MockServer::start().await;
        mount_repo(&server).await;
        Mock::given(method("POST"))
            .and(path("/api/v1/repos/alice/notes/files/src/a.md/revert"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "commitSha": "d".repeat(40), "version": 4, "filesChanged": 1, "created": false
            })))
            .mount(&server)
            .await;
        let s = setup(&server);

        cmd_revert(
            &s.config,
            &Output::new(true),
            "src/a.md".to_string(),
            "3".to_string(),
            false,
            opts(Some("alice/notes")),
        )
        .await
        .unwrap();

        let requests = server.received_requests().await.unwrap();
        let revert = requests
            .iter()
            .find(|r| r.url.path().ends_with("/revert"))
            .expect("one revert");
        let body: serde_json::Value = revert.body_json().unwrap();
        assert_eq!(
            body,
            serde_json::json!({
                "to": "3",
                "parentSha": PARENT,
                "provenance": {
                    "integration": "syns-bb-plugin",
                    "run": "run-1",
                    "trigger": "thread-page",
                    "taskRef": "T-7",
                },
            })
        );
        // The repository read stands ahead of the revert.
        assert!(requests[0].url.path().ends_with("/repos/alice/notes"));
        drop(s.work);
    }

    // Issue 160: a head that moved past the parent answers push's `409`
    // `conflict` naming `currentSha`, the conflict refusal at exit `7`.
    #[tokio::test]
    #[serial]
    async fn revert_on_a_moved_head_is_the_conflict_refusal_at_exit_seven() {
        let server = MockServer::start().await;
        mount_repo(&server).await;
        Mock::given(method("POST"))
            .and(path("/api/v1/repos/alice/notes/files/a.md/revert"))
            .respond_with(ResponseTemplate::new(409).set_body_json(serde_json::json!({
                "error": "conflict", "message": "Head mismatch", "currentSha": MOVED,
            })))
            .mount(&server)
            .await;
        let s = setup(&server);

        let err = cmd_revert(
            &s.config,
            &Output::new(true),
            "a.md".to_string(),
            "1".to_string(),
            false,
            opts(Some("alice/notes")),
        )
        .await
        .unwrap_err();

        assert!(
            matches!(&err, CliError::WriteConflict { current_sha, .. } if current_sha == MOVED),
            "{err:?}"
        );
        assert_eq!(err.exit_code(), 7);
        let doc = err.json_document();
        assert_eq!(doc["code"], serde_json::json!("write_conflict"));
        assert_eq!(doc["currentSha"], serde_json::json!(MOVED));
        drop(s.work);
    }

    // Issue 160, step 8: inside a checkout holding unpublished work the
    // revert is refused by the guard every write verb runs, before any
    // request.
    #[tokio::test]
    #[serial]
    async fn revert_inside_a_checkout_holding_unpublished_work_is_refused() {
        let server = MockServer::start().await;
        let s = setup(&server);
        std::fs::write(
            s.work.path().join(".syns.yaml"),
            "owner: alice\nname: notes\n",
        )
        .unwrap();
        std::fs::write(s.work.path().join("draft.md"), "unpublished\n").unwrap();

        let err = cmd_revert(
            &s.config,
            &Output::new(true),
            "a.md".to_string(),
            "1".to_string(),
            false,
            opts(None),
        )
        .await
        .unwrap_err();

        assert!(matches!(err, CliError::CheckoutDirty { .. }), "{err:?}");
        assert_eq!(
            err.json_document()["code"],
            serde_json::json!("checkout_dirty")
        );
        assert!(
            err.to_string()
                .contains("holds unpublished local changes for alice/notes")
        );
        assert!(server.received_requests().await.unwrap().is_empty());
    }

    #[tokio::test]
    #[serial]
    async fn revert_requires_authentication() {
        let server = MockServer::start().await;
        let work = tempfile::tempdir().unwrap();
        let home = tempfile::tempdir().unwrap();
        std::env::set_current_dir(work.path()).unwrap();
        unsafe { std::env::set_var("SYNS_CONFIG_DIR", home.path()) };
        let config = Config::new(Some(&server.uri())).unwrap();
        unsafe { std::env::remove_var("SYNS_CONFIG_DIR") };

        let result = cmd_revert(
            &config,
            &Output::new(false),
            "a.txt".to_string(),
            "1".to_string(),
            false,
            opts(Some("alice/notes")),
        )
        .await;

        assert!(matches!(result, Err(CliError::AuthRequired)));
        assert!(server.received_requests().await.unwrap().is_empty());
    }

    #[tokio::test]
    #[serial]
    async fn revert_with_if_repo_set_and_no_identity_skips_silently() {
        let server = MockServer::start().await;
        let s = setup(&server);

        let result = cmd_revert(
            &s.config,
            &Output::new(false),
            "foo".to_string(),
            "1".to_string(),
            true,
            opts(None),
        )
        .await;

        assert!(result.is_ok());
        assert!(server.received_requests().await.unwrap().is_empty());
        drop(s.work);
    }

    // Issue 160, edge inputs: an empty path, and one naming the root, are
    // refused before any request.
    #[tokio::test]
    #[serial]
    async fn revert_refuses_an_empty_path_and_the_root_before_any_request() {
        let server = MockServer::start().await;
        mount_repo(&server).await;
        let s = setup(&server);
        for typed in ["", "  ", ".", "a/.."] {
            let err = cmd_revert(
                &s.config,
                &Output::new(true),
                typed.to_string(),
                "1".to_string(),
                false,
                opts(Some("alice/notes")),
            )
            .await
            .unwrap_err();
            assert!(matches!(err, CliError::Config { .. }), "{typed:?}: {err:?}");
        }
        assert!(
            !server
                .received_requests()
                .await
                .unwrap()
                .iter()
                .any(|r| r.url.path().ends_with("/revert"))
        );
        drop(s.work);
    }

    #[tokio::test]
    async fn revert_raw_emits_server_wire_shape_without_synthetic_path() {
        let mock_server = MockServer::start().await;
        let body = r#"{"commitSha":"def4567890abcdef0123456789abcdef01234567","version":5,"filesChanged":1,"created":false}"#;
        Mock::given(method("POST"))
            .and(path(
                "/api/v1/repos/alice/my-project/files/src/main.ts/revert",
            ))
            .respond_with(ResponseTemplate::new(200).set_body_string(body))
            .mount(&mock_server)
            .await;

        let client = SynsClient::new(&mock_server.uri()).unwrap();
        let request = RevertFileRequest {
            to: "3".into(),
            message: None,
            parent_sha: None,
            provenance: None,
        };
        let (typed, raw) = client
            .revert_file("alice/my-project", "test-token", "src/main.ts", &request)
            .await
            .unwrap();

        assert_eq!(raw.as_object().unwrap().keys().count(), 4);
        assert!(raw.get("path").is_none());
        assert_eq!(
            raw["commitSha"],
            serde_json::json!("def4567890abcdef0123456789abcdef01234567")
        );
        assert_eq!(raw["filesChanged"], serde_json::json!(1));
        let expected: serde_json::Value = serde_json::from_str(body).unwrap();
        assert_eq!(raw, expected);
        assert!(typed.commit_sha.starts_with("def4567890"));
    }
}
