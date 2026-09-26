use std::io::Write;

use crate::client::{SynsClient, hash_mismatch};
use crate::config::Config;
use crate::errors::CliError;
use crate::output::Output;
use crate::push::hash::blob_sha1;
use crate::read::{
    ReadOptions, read_not_found, report_reference, resolve_read_target, with_reference,
};

/// `syns cat PATH` — the bytes cross the primary stream unframed and
/// unrendered whatever reference the run resolved (SPEC u270).
pub async fn cmd_cat(
    config: &Config,
    output: &Output,
    path: String,
    opts: ReadOptions,
) -> Result<(), CliError> {
    // 1 — resolve the target.
    let Some(target) = resolve_read_target(config, output, &opts).await? else {
        return Ok(());
    };
    let client = SynsClient::new(config.server_url())?;

    // 2 — read the path at that reference, sending the pinned ordinal:
    // outside machine-readable mode through the raw entry, whose bytes
    // are the stored ones exactly (SPEC u280 `cmd_cat` 1), and under
    // `--json` through the file read, passed through as served.
    let version_ref = target.version_ref();
    let refused = |e: CliError| -> CliError {
        if opts.version.is_some() {
            return read_not_found(e, &opts, &target.reference, &path);
        }
        if !output.is_json() {
            return e.with_cat_path_context(path.clone());
        }
        e
    };

    if output.is_json() {
        let (_response, raw) = client
            .get_file(
                &target.repo_id,
                target.token.as_deref(),
                &path,
                Some(&version_ref),
            )
            .await
            .map_err(refused)?;
        output.json(&with_reference(raw, &target.reference));
    } else {
        let raw = client
            .get_raw(
                &target.repo_id,
                target.token.as_deref(),
                &path,
                Some(&version_ref),
                None,
            )
            .await
            .map_err(refused)?;
        // `cmd_cat` 2 — the bytes are the ones the `ETag` names, or none
        // is printed.
        if let Some(etag) = &raw.etag {
            let actual = blob_sha1(&raw.bytes);
            if &actual != etag {
                return Err(hash_mismatch(&path, etag, &actual));
            }
        }
        // `cmd_cat` 3 — the bytes unchanged, and no byte more. A write
        // the stream refuses ends the run as `print!` ends it.
        let stdout = std::io::stdout();
        let mut lock = stdout.lock();
        if let Err(err) = lock.write_all(&raw.bytes).and_then(|()| lock.flush()) {
            panic!("failed printing to stdout: {err}");
        }
    }

    // 4 — report the reference.
    report_reference(output, &target.reference);

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serial_test::serial;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    const HEAD_SHA: &str = "def4560000000000000000000000000000000000";

    /// Mounts the two addresses every read verb now resolves through
    /// before it asks for any content (SPEC u270 `resolve_read_target`).
    async fn mount_reference(server: &MockServer, repo_id: &str) {
        Mock::given(method("GET"))
            .and(path(format!("/api/v1/repos/{repo_id}")))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "owner": "alice", "name": "my-project", "description": null,
                "commitSha": HEAD_SHA, "status": "active", "author": null, "tags": [],
                "visibility": "public", "forkedFrom": null, "forkCount": 0,
                "fileCount": 1, "role": null,
                "createdAt": "2026-01-01T00:00:00Z", "updatedAt": "2026-01-01T00:00:00Z",
            })))
            .mount(server)
            .await;
        Mock::given(method("GET"))
            .and(path(format!("/api/v1/repos/{repo_id}/versions/{HEAD_SHA}")))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "version": 7, "sha": HEAD_SHA, "parentSha": null, "message": "m",
                "messageBody": null, "author": "alice",
                "createdAt": "2026-01-01T00:00:00Z", "filesChanged": ["a.md"],
            })))
            .mount(server)
            .await;
    }

    /// Mounts the raw entry answering `bytes` under an `ETag` of their
    /// blob hash (SPEC u280 `cmd_cat` 1–2).
    async fn mount_raw(server: &MockServer, file: &str, bytes: &[u8]) {
        Mock::given(method("GET"))
            .and(path(format!("/api/v1/repos/alice/my-project/raw/{file}")))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("ETag", format!("\"{}\"", blob_sha1(bytes)).as_str())
                    .set_body_bytes(bytes.to_vec()),
            )
            .mount(server)
            .await;
    }

    #[tokio::test]
    #[serial]
    async fn cat_refuses_bytes_the_etag_does_not_name() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join(".syns.yaml"),
            "owner: alice\nname: my-project\n",
        )
        .unwrap();
        std::env::set_current_dir(dir.path()).unwrap();
        unsafe { std::env::set_var("SYNS_CONFIG_DIR", dir.path()) };

        let mock_server = MockServer::start().await;
        mount_reference(&mock_server, "alice/my-project").await;
        Mock::given(method("GET"))
            .and(path("/api/v1/repos/alice/my-project/raw/image.png"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("ETag", format!("\"{}\"", "0".repeat(40)).as_str())
                    .set_body_bytes(b"\x89PNG\x00".to_vec()),
            )
            .mount(&mock_server)
            .await;

        let config = Config::new(Some(&mock_server.uri())).unwrap();
        let result = cmd_cat(
            &config,
            &Output::new(false),
            "image.png".to_string(),
            here(),
        )
        .await;
        unsafe { std::env::remove_var("SYNS_CONFIG_DIR") };

        let err = result.unwrap_err();
        assert!(
            err.to_string().contains(&format!(
                "invalid response body: image.png: expected {}, got {}",
                "0".repeat(40),
                blob_sha1(b"\x89PNG\x00")
            )),
            "{err}"
        );
        assert_eq!(err.exit_code(), 1);
    }

    fn here() -> ReadOptions {
        ReadOptions::default()
    }

    fn here_if_repo() -> ReadOptions {
        ReadOptions {
            if_repo: true,
            ..ReadOptions::default()
        }
    }

    #[tokio::test]
    #[serial]
    async fn cat_outputs_file_content() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join(".syns.yaml"),
            "owner: alice\nname: my-project\n",
        )
        .unwrap();
        std::env::set_current_dir(dir.path()).unwrap();
        unsafe { std::env::set_var("SYNS_CONFIG_DIR", dir.path()) };

        let mock_server = MockServer::start().await;
        mount_reference(&mock_server, "alice/my-project").await;

        mount_raw(&mock_server, "src/main.ts", b"console.log('hello');").await;

        let config = Config::new(Some(&mock_server.uri())).unwrap();
        let output = Output::new(false);

        let result = cmd_cat(&config, &output, "src/main.ts".to_string(), here()).await;
        unsafe { std::env::remove_var("SYNS_CONFIG_DIR") };

        assert!(result.is_ok());
    }

    #[tokio::test]
    #[serial]
    async fn cat_json_output() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join(".syns.yaml"),
            "owner: alice\nname: my-project\n",
        )
        .unwrap();
        std::env::set_current_dir(dir.path()).unwrap();
        unsafe { std::env::set_var("SYNS_CONFIG_DIR", dir.path()) };

        let mock_server = MockServer::start().await;
        mount_reference(&mock_server, "alice/my-project").await;

        Mock::given(method("GET"))
            .and(path("/api/v1/repos/alice/my-project/files/src/main.ts"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "content": "console.log('hello');",
                "sha": "abc123",
                "size": 21
            })))
            .mount(&mock_server)
            .await;

        let config = Config::new(Some(&mock_server.uri())).unwrap();
        let output = Output::new(true);

        let result = cmd_cat(&config, &output, "src/main.ts".to_string(), here()).await;
        unsafe { std::env::remove_var("SYNS_CONFIG_DIR") };

        assert!(result.is_ok());
    }

    #[tokio::test]
    #[serial]
    async fn cat_404_not_found_renders_friendly_message_with_path() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join(".syns.yaml"),
            "owner: alice\nname: my-project\n",
        )
        .unwrap();
        std::env::set_current_dir(dir.path()).unwrap();
        unsafe { std::env::set_var("SYNS_CONFIG_DIR", dir.path()) };

        let mock_server = MockServer::start().await;
        mount_reference(&mock_server, "alice/my-project").await;

        Mock::given(method("GET"))
            .and(path("/api/v1/repos/alice/my-project/raw/src/missing.ts"))
            .respond_with(ResponseTemplate::new(404).set_body_json(serde_json::json!({
                "error": "not_found",
                "message": "File not found"
            })))
            .mount(&mock_server)
            .await;

        let config = Config::new(Some(&mock_server.uri())).unwrap();
        let output = Output::new(false);

        let result = cmd_cat(&config, &output, "src/missing.ts".to_string(), here()).await;
        unsafe { std::env::remove_var("SYNS_CONFIG_DIR") };

        let err = result.unwrap_err();
        assert_eq!(err.to_string(), "file not found: src/missing.ts");
        assert_eq!(err.exit_code(), 1);
    }

    #[tokio::test]
    #[serial]
    async fn cat_404_repo_not_found_preserves_generic_display() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join(".syns.yaml"),
            "owner: alice\nname: my-project\n",
        )
        .unwrap();
        std::env::set_current_dir(dir.path()).unwrap();
        unsafe { std::env::set_var("SYNS_CONFIG_DIR", dir.path()) };

        let mock_server = MockServer::start().await;
        mount_reference(&mock_server, "alice/my-project").await;

        Mock::given(method("GET"))
            .and(path("/api/v1/repos/alice/my-project/raw/README.md"))
            .respond_with(ResponseTemplate::new(404).set_body_json(serde_json::json!({
                "error": "repo_not_found",
                "message": "Repository not found"
            })))
            .mount(&mock_server)
            .await;

        let config = Config::new(Some(&mock_server.uri())).unwrap();
        let output = Output::new(false);

        let result = cmd_cat(&config, &output, "README.md".to_string(), here()).await;
        unsafe { std::env::remove_var("SYNS_CONFIG_DIR") };

        let err = result.unwrap_err();
        assert_eq!(err.to_string(), "server error (404): repo_not_found");
        assert_eq!(err.exit_code(), 1);
    }

    #[tokio::test]
    #[serial]
    async fn cat_404_not_found_in_json_mode_preserves_generic_display() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join(".syns.yaml"),
            "owner: alice\nname: my-project\n",
        )
        .unwrap();
        std::env::set_current_dir(dir.path()).unwrap();
        unsafe { std::env::set_var("SYNS_CONFIG_DIR", dir.path()) };

        let mock_server = MockServer::start().await;
        mount_reference(&mock_server, "alice/my-project").await;

        Mock::given(method("GET"))
            .and(path("/api/v1/repos/alice/my-project/files/src/missing.ts"))
            .respond_with(ResponseTemplate::new(404).set_body_json(serde_json::json!({
                "error": "not_found",
                "message": "File not found"
            })))
            .mount(&mock_server)
            .await;

        let config = Config::new(Some(&mock_server.uri())).unwrap();
        let output = Output::new(true);

        let result = cmd_cat(&config, &output, "src/missing.ts".to_string(), here()).await;
        unsafe { std::env::remove_var("SYNS_CONFIG_DIR") };

        let err = result.unwrap_err();
        assert_eq!(err.to_string(), "server error (404): not_found");
        assert_eq!(err.exit_code(), 1);
    }

    #[tokio::test]
    #[serial]
    async fn cat_with_if_repo_set_and_identity_resolved_runs_normally() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join(".syns.yaml"),
            "owner: alice\nname: my-project\n",
        )
        .unwrap();
        std::env::set_current_dir(dir.path()).unwrap();
        unsafe { std::env::set_var("SYNS_CONFIG_DIR", dir.path()) };

        let mock_server = MockServer::start().await;
        mount_reference(&mock_server, "alice/my-project").await;
        mount_raw(&mock_server, "src/main.ts", b"console.log('hello');").await;

        let config = Config::new(Some(&mock_server.uri())).unwrap();
        let output = Output::new(false);

        let result = cmd_cat(&config, &output, "src/main.ts".to_string(), here_if_repo()).await;
        unsafe { std::env::remove_var("SYNS_CONFIG_DIR") };

        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn cat_get_file_raw_includes_path_field() {
        let mock_server = MockServer::start().await;
        let body =
            r#"{"path":"src/main.ts","sha":"abc123","content":"console.log('hello');","size":21}"#;
        Mock::given(method("GET"))
            .and(path("/api/v1/repos/alice/my-project/files/src/main.ts"))
            .respond_with(ResponseTemplate::new(200).set_body_string(body))
            .mount(&mock_server)
            .await;

        let client = SynsClient::new(&mock_server.uri()).unwrap();
        let (_typed, raw) = client
            .get_file("alice/my-project", None, "src/main.ts", None)
            .await
            .unwrap();

        assert!(raw.get("path").is_some());
        assert_eq!(raw["path"], serde_json::json!("src/main.ts"));
        assert!(raw.get("content").is_some());
        assert!(raw.get("sha").is_some());
        assert!(raw.get("size").is_some());
        let expected: serde_json::Value = serde_json::from_str(body).unwrap();
        assert_eq!(raw, expected);
    }

    #[tokio::test]
    #[serial]
    async fn cat_with_if_repo_set_and_no_identity_skips_silently() {
        let dir = tempfile::tempdir().unwrap();
        std::env::set_current_dir(dir.path()).unwrap();
        unsafe { std::env::set_var("SYNS_CONFIG_DIR", dir.path()) };

        let mock_server = MockServer::start().await;
        let config = Config::new(Some(&mock_server.uri())).unwrap();
        let output = Output::new(false);

        let result = cmd_cat(&config, &output, "anything".to_string(), here_if_repo()).await;
        unsafe { std::env::remove_var("SYNS_CONFIG_DIR") };

        assert!(result.is_ok());
        assert!(mock_server.received_requests().await.unwrap().is_empty());
    }
}
