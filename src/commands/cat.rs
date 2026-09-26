use std::io::Write;

use base64::display::Base64Display;
use base64::engine::general_purpose::STANDARD;
use serde::Serialize;

use crate::client::{RawFile, SynsClient, hash_mismatch};
use crate::config::Config;
use crate::errors::CliError;
use crate::output::Output;
use crate::push::collector::is_text;
use crate::push::hash::blob_sha1;
use crate::read::{
    ReadOptions, ResolvedRef, read_not_found, report_reference, resolve_read_target,
};

/// The generic byte type, which a `--json` document never names.
const GENERIC_BYTE_TYPE: &str = "application/octet-stream";

/// The one document `syns cat PATH --json` writes (SPEC u283 Contract
/// Surface, `CatDocument`): `content` for bytes passing `is_text`, beside
/// a `null` `mediaType`, and `contentBase64` for any other, `mediaType`
/// standing only where the answer named a type other than the generic
/// byte type. Keys are declared in lexical order, the order they are
/// written in.
#[derive(Serialize, Debug)]
#[serde(rename_all = "camelCase")]
pub struct CatDocument {
    commit_sha: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    content: Option<String>,
    #[serde(
        skip_serializing_if = "Option::is_none",
        serialize_with = "base64_as_written"
    )]
    content_base64: Option<Vec<u8>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    media_type: Option<Option<String>>,
    path: String,
    sha: String,
    size: u64,
    version: u32,
}

/// Writes the bytes as standard padded base64 straight into the stream
/// the document is written to, building no encoded copy.
fn base64_as_written<S: serde::Serializer>(
    bytes: &Option<Vec<u8>>,
    serializer: S,
) -> Result<S::Ok, S::Error> {
    match bytes {
        Some(bytes) => serializer.collect_str(&Base64Display::new(bytes, &STANDARD)),
        None => serializer.serialize_none(),
    }
}

impl CatDocument {
    /// The document for one raw answer at the run's reference, the bytes
    /// moved into it rather than copied. `sha` is their blob hash.
    pub fn new(path: String, raw: RawFile, sha: String, reference: &ResolvedRef) -> CatDocument {
        let size = raw.bytes.len() as u64;
        let (content, content_base64, media_type) = if is_text(&raw.bytes) {
            let text = String::from_utf8(raw.bytes).expect("is_text admits valid UTF-8 alone");
            (Some(text), None, Some(None))
        } else {
            let named = raw
                .media_type
                .filter(|media_type| media_type != GENERIC_BYTE_TYPE && !media_type.is_empty());
            (None, Some(raw.bytes), named.map(Some))
        };
        CatDocument {
            commit_sha: reference.commit_sha.clone(),
            content,
            content_base64,
            media_type,
            path,
            sha,
            size,
            version: reference.version,
        }
    }
}

/// `syns cat PATH` — the bytes cross the primary stream unframed and
/// unrendered whatever reference the run resolved (SPEC u270), and under
/// `--json` as one document carrying them whole (SPEC u283).
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

    // 2 — read the path at that reference through the raw entry in either
    // mode, sending the pinned ordinal: its bytes are the stored ones
    // exactly (SPEC u280 `cmd_cat` 1, SPEC u283 `cmd_cat` 2).
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

    // 3 — the bytes are the ones the `ETag` names, or none is printed.
    let sha = blob_sha1(&raw.bytes);
    if let Some(etag) = &raw.etag
        && &sha != etag
    {
        return Err(hash_mismatch(&path, etag, &sha));
    }

    // 4 — the bytes unchanged, and no byte more; or under `--json` the
    // one document, written as it is serialised. A write the stream
    // refuses ends the run as `print!` ends it.
    let stdout = std::io::stdout();
    let written = if output.is_json() {
        let document = CatDocument::new(path, raw, sha, &target.reference);
        let mut stream = std::io::BufWriter::with_capacity(1 << 16, stdout.lock());
        serde_json::to_writer_pretty(&mut stream, &document)
            .map_err(std::io::Error::from)
            .and_then(|()| stream.write_all(b"\n"))
            .and_then(|()| stream.flush())
    } else {
        let mut lock = stdout.lock();
        lock.write_all(&raw.bytes).and_then(|()| lock.flush())
    };
    if let Err(err) = written {
        panic!("failed printing to stdout: {err}");
    }

    // 5 — report the reference.
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
        // SPEC u283 `cmd_cat` 2: `--json` reads the raw entry too.
        mount_raw(&mock_server, "src/main.ts", b"console.log('hello');").await;

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
            .and(path("/api/v1/repos/alice/my-project/raw/src/missing.ts"))
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

    fn reference() -> ResolvedRef {
        ResolvedRef {
            version: 7,
            commit_sha: HEAD_SHA.to_string(),
        }
    }

    fn raw_file(bytes: &[u8], media_type: Option<&str>) -> RawFile {
        RawFile {
            bytes: bytes.to_vec(),
            etag: None,
            media_type: media_type.map(str::to_string),
        }
    }

    // SPEC u283 Contract Surface, `CatDocument`: text rides as `content`
    // beside a `null` `mediaType`, keys in lexical order.
    #[test]
    fn a_text_document_carries_content_and_a_null_media_type() {
        let document = CatDocument::new(
            "a.md".to_string(),
            raw_file(b"# a\n", Some("text/plain")),
            blob_sha1(b"# a\n"),
            &reference(),
        );
        let written = serde_json::to_string(&document).unwrap();
        assert_eq!(
            written,
            format!(
                r##"{{"commitSha":"{HEAD_SHA}","content":"# a\n","mediaType":null,"path":"a.md","sha":"{}","size":4,"version":7}}"##,
                blob_sha1(b"# a\n")
            )
        );
    }

    // `CatDocument`: bytes ride as `contentBase64`, `mediaType` naming a
    // type other than the generic byte type and omitted for it.
    #[test]
    fn a_byte_document_carries_base64_and_names_only_a_specific_type() {
        let png: &[u8] = b"\x89PNG\r\n\x1a\n\x00\xff";
        let named: serde_json::Value = serde_json::to_value(CatDocument::new(
            "image.png".to_string(),
            raw_file(png, Some("image/png")),
            blob_sha1(png),
            &reference(),
        ))
        .unwrap();
        assert_eq!(
            named["contentBase64"],
            serde_json::json!("iVBORw0KGgoA/w==")
        );
        assert_eq!(named["mediaType"], serde_json::json!("image/png"));
        assert_eq!(named["size"], serde_json::json!(10));
        assert!(named.get("content").is_none());

        for generic in [Some("application/octet-stream"), None] {
            let unnamed = serde_json::to_value(CatDocument::new(
                "blob.bin".to_string(),
                raw_file(png, generic),
                blob_sha1(png),
                &reference(),
            ))
            .unwrap();
            assert!(unnamed.get("mediaType").is_none(), "{generic:?}");
            assert!(unnamed.get("content").is_none());
        }
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
