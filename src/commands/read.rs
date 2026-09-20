//! `syns read PATH [--offset N] [--limit N]` — the numbered window over
//! one file at one version (SPEC u270).
//!
//! `D-080` splits this verb from `syns cat PATH`: the numbered read
//! refuses a content that is not text, while `cat` keeps passing those
//! bytes through.

use crate::client::SynsClient;
use crate::config::Config;
use crate::errors::CliError;
use crate::output::Output;
use crate::read::{ReadOptions, read_not_found, report_reference, resolve_read_target};

/// The default window: `--offset` counts from `1`, `--limit` from
/// `2000` (SPEC u270 Contract Surface, `cmd_read`).
pub const DEFAULT_OFFSET: u32 = 1;
pub const DEFAULT_LIMIT: u32 = 2000;

/// The numbered line cuts its text at this many characters.
const LINE_CUT: usize = 2000;

/// The line's 1-based number right-aligned in six characters, a tab,
/// then the line cut at its first 2000 characters.
fn numbered_line(number: usize, text: &str) -> String {
    let cut: String = text.chars().take(LINE_CUT).collect();
    format!("{number:>6}\t{cut}")
}

pub async fn cmd_read(
    config: &Config,
    output: &Output,
    path: String,
    offset: u32,
    limit: u32,
    opts: ReadOptions,
) -> Result<(), CliError> {
    // 1 — refuse an `--offset` or a `--limit` of `0` before any request,
    // then resolve the target.
    if offset == 0 {
        return Err(CliError::Config {
            message: "--offset must be \u{2265} 1".to_string(),
        });
    }
    if limit == 0 {
        return Err(CliError::Config {
            message: "--limit must be \u{2265} 1".to_string(),
        });
    }
    let Some(target) = resolve_read_target(config, output, &opts).await? else {
        return Ok(());
    };
    let client = SynsClient::new(config.server_url())?;

    // 2 — read the path at that reference.
    let version_ref = target.version_ref();
    let (response, raw) = match client
        .get_file(
            &target.repo_id,
            target.token.as_deref(),
            &path,
            Some(&version_ref),
        )
        .await
    {
        Ok(tuple) => tuple,
        Err(e) => {
            if opts.version.is_some() {
                return Err(read_not_found(e, &opts, &target.reference, &path));
            }
            if !output.is_json() {
                return Err(e.with_cat_path_context(path.clone()));
            }
            return Err(e);
        }
    };

    // 3 — classify the decoded content, then split a text one on
    // newlines and take the window the two options name.
    if response.content.contains('\0') {
        return Err(CliError::NotText { path });
    }
    let lines: Vec<&str> = response.content.lines().collect();
    let total_lines = lines.len();
    let start = (offset as usize) - 1;
    let window: Vec<&str> = lines.into_iter().skip(start).take(limit as usize).collect();

    // 4 — write the numbered lines, or the read document.
    if output.is_json() {
        let document = serde_json::json!({
            "path": raw.get("path").cloned().unwrap_or(serde_json::Value::from(path.as_str())),
            "sha": response.sha,
            "size": response.size,
            "version": target.reference.version,
            "commitSha": target.reference.commit_sha,
            "offset": offset,
            "limit": limit,
            "totalLines": total_lines,
            "content": window.join("\n"),
        });
        output.json(&document);
    } else {
        for (index, text) in window.iter().enumerate() {
            println!("{}", numbered_line(start + index + 1, text));
        }
    }

    // 5 — report the reference.
    report_reference(output, &target.reference);

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serial_test::serial;
    use wiremock::matchers::{method, path as path_matcher};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    const HEAD_SHA: &str = "def4560000000000000000000000000000000000";

    async fn mount_reference(server: &MockServer) {
        Mock::given(method("GET"))
            .and(path_matcher("/api/v1/repos/alice/notes"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "owner": "alice", "name": "notes", "description": null,
                "commitSha": HEAD_SHA, "status": "active", "author": null, "tags": [],
                "visibility": "public", "forkedFrom": null, "forkCount": 0,
                "fileCount": 1, "role": null,
                "createdAt": "2026-01-01T00:00:00Z", "updatedAt": "2026-01-01T00:00:00Z",
            })))
            .mount(server)
            .await;
        Mock::given(method("GET"))
            .and(path_matcher(format!(
                "/api/v1/repos/alice/notes/versions/{HEAD_SHA}"
            )))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "version": 7, "sha": HEAD_SHA, "parentSha": null, "message": "m",
                "messageBody": null, "author": "alice",
                "createdAt": "2026-01-01T00:00:00Z", "filesChanged": ["a.md"],
            })))
            .mount(server)
            .await;
    }

    async fn mount_file(server: &MockServer, content: &str) {
        Mock::given(method("GET"))
            .and(path_matcher("/api/v1/repos/alice/notes/files/a.md"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "path": "a.md", "sha": "abc", "content": content, "size": content.len(),
            })))
            .mount(server)
            .await;
    }

    fn named() -> ReadOptions {
        ReadOptions {
            repo: Some("alice/notes".into()),
            version: None,
            if_repo: false,
        }
    }

    // SPEC u270 Contract Surface, the numbered line.
    #[test]
    fn the_numbered_line_right_aligns_its_number_and_cuts_its_text() {
        assert_eq!(numbered_line(2, "second"), "     2\tsecond");
        assert_eq!(numbered_line(1234567, "x"), "1234567\tx");
        let long = "a".repeat(2100);
        let rendered = numbered_line(3, &long);
        let text = rendered.split_once('\t').unwrap().1;
        assert_eq!(text.chars().count(), 2000);
    }

    // SPEC u270 Tests: `read_prints_the_numbered_window`.
    #[tokio::test]
    #[serial]
    async fn read_prints_the_numbered_window() {
        let dir = tempfile::tempdir().unwrap();
        unsafe { std::env::set_var("SYNS_CONFIG_DIR", dir.path()) };
        let server = MockServer::start().await;
        mount_reference(&server).await;
        let third = "c".repeat(2100);
        let content = format!("one\ntwo\n{third}\nfour\nfive\n");
        mount_file(&server, &content).await;

        let config = Config::new(Some(&server.uri())).unwrap();
        let output = Output::new(true);
        let result = cmd_read(&config, &output, "a.md".into(), 2, 2, named()).await;
        unsafe { std::env::remove_var("SYNS_CONFIG_DIR") };
        assert!(result.is_ok(), "{result:?}");
    }

    // SPEC u270 Tests: `read_window_past_the_last_line_prints_nothing`.
    #[tokio::test]
    #[serial]
    async fn read_window_past_the_last_line_exits_zero() {
        let dir = tempfile::tempdir().unwrap();
        unsafe { std::env::set_var("SYNS_CONFIG_DIR", dir.path()) };
        let server = MockServer::start().await;
        mount_reference(&server).await;
        mount_file(&server, "one\ntwo\n").await;

        let config = Config::new(Some(&server.uri())).unwrap();
        let output = Output::new(false);
        let result = cmd_read(&config, &output, "a.md".into(), 99, DEFAULT_LIMIT, named()).await;
        unsafe { std::env::remove_var("SYNS_CONFIG_DIR") };
        assert!(result.is_ok(), "{result:?}");
    }

    // SPEC u270 Behaviour, `cmd_read` 3 and `D-080`.
    #[tokio::test]
    #[serial]
    async fn a_nul_bearing_content_is_refused_by_the_numbered_read() {
        let dir = tempfile::tempdir().unwrap();
        unsafe { std::env::set_var("SYNS_CONFIG_DIR", dir.path()) };
        let server = MockServer::start().await;
        mount_reference(&server).await;
        mount_file(&server, "one\u{0}two").await;

        let config = Config::new(Some(&server.uri())).unwrap();
        let output = Output::new(false);
        let err = cmd_read(
            &config,
            &output,
            "a.md".into(),
            DEFAULT_OFFSET,
            DEFAULT_LIMIT,
            named(),
        )
        .await
        .unwrap_err();
        unsafe { std::env::remove_var("SYNS_CONFIG_DIR") };

        assert_eq!(
            err.to_string(),
            "cannot number content that is not text: a.md \u{2014} read it with syns cat a.md"
        );
        assert_eq!(err.exit_code(), 1);
    }

    // SPEC u270 Behaviour, `cmd_read` 1: a `0` on either option is
    // refused before any request.
    #[tokio::test]
    #[serial]
    async fn a_zero_window_option_is_refused_before_any_request() {
        let dir = tempfile::tempdir().unwrap();
        unsafe { std::env::set_var("SYNS_CONFIG_DIR", dir.path()) };
        let server = MockServer::start().await;
        let config = Config::new(Some(&server.uri())).unwrap();
        let output = Output::new(false);

        let offset_err = cmd_read(&config, &output, "a.md".into(), 0, 10, named())
            .await
            .unwrap_err();
        let limit_err = cmd_read(&config, &output, "a.md".into(), 1, 0, named())
            .await
            .unwrap_err();
        unsafe { std::env::remove_var("SYNS_CONFIG_DIR") };

        assert_eq!(
            offset_err.to_string(),
            "configuration error: --offset must be \u{2265} 1"
        );
        assert_eq!(
            limit_err.to_string(),
            "configuration error: --limit must be \u{2265} 1"
        );
        assert!(server.received_requests().await.unwrap().is_empty());
    }
}
