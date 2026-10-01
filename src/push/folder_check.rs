//! The check of a folder against the versions after the one a run's work
//! stands on (SPEC u292).
//!
//! A write, push or sync made inside a folder is refused under a moved
//! head only where a version after the stated parent changed a path at
//! or under the folder (`D-100`). The check reads one folder-history page
//! at `limit` `1` (`D-103`); where that page is empty, one version-list
//! page with no path, and at most one version read numbering the commit
//! the work stands on (`D-104`). A page the server did not narrow to the
//! folder is refused as an unsupported server, a `transient` refusal is
//! answered as a folder that moved, and every other refusal ends the run
//! on its own code (`D-107`).

use crate::client::{SynsClient, VersionEntry};
use crate::commands::sync::{ErrorClass, error_class};
use crate::errors::{ApiErrorContext, CliError};
use crate::repo::folder::lies_under;

/// The commits from the head among which the engine's path-filtered walk
/// matches a path (`MAX_COMMITS_SCANNED` of `github.com/synsdev/syns-git`
/// `src/git/history.rs`): an empty folder page covers no version further
/// back than this from the head.
pub const SCAN_BOUND: u32 = 10_000;

/// The most publications one run sends for one change inside a folder,
/// the first included, a first send at the head the repository read
/// counted among them.
pub const FOLDER_SEND_BOUND: u32 = 3;

/// The unread-check line, `{path}` the folder's recorded path and
/// `{error}` the refusal's own line.
pub const UNREAD_CHECK_LINE: &str = "warning: the history of the folder {path} could not be read, so the run answers as though the folder moved: {error}";

/// The unread-check line for `folder` over `err`.
pub fn unread_check_line(folder: &str, err: &CliError) -> String {
    UNREAD_CHECK_LINE
        .replace("{path}", folder)
        .replace("{error}", &err.to_string())
}

/// The head a refusal names: the `currentSha` a `409` `conflict`
/// carries, none on any other refusal.
pub fn named_head(err: &CliError) -> Option<&str> {
    match err {
        CliError::Api {
            status: Some(409),
            error,
            context: Some(ApiErrorContext::HeadMoved { current_sha }),
        } if error == "conflict" => Some(current_sha),
        _ => None,
    }
}

/// One run's check of a folder: `folder` is the folder's recorded path,
/// `since` the full commit hash the run's work stands on, and
/// `since_version` that commit's version number wherever the run already
/// holds it — kept once read, so a check asked again reads it no more.
#[derive(Debug, Clone)]
pub struct FolderCheck {
    pub repo_id: String,
    pub folder: String,
    pub since: String,
    pub since_version: Option<u32>,
}

/// Whether `entry` changed a path equal to `folder` or lying under it.
fn changed_the_folder(entry: &VersionEntry, folder: &str) -> bool {
    entry
        .files_changed
        .iter()
        .any(|path| path == folder || lies_under(path, folder))
}

impl FolderCheck {
    /// A read of the check refused: a `transient` refusal answers as a
    /// folder that moved once the unread-check line is written, and any
    /// other refusal ends the check on it.
    fn unread(&self, err: CliError) -> Result<bool, CliError> {
        match error_class(&err) {
            Some(ErrorClass::Transient) => {
                eprintln!("{}", unread_check_line(&self.folder, &err));
                Ok(true)
            }
            _ => Err(err),
        }
    }

    /// Whether a version numbered after `since` changed a path at or
    /// under the folder (SPEC u292 Behaviour, `FolderCheck::folder_moved`).
    /// A later version restoring the folder changes nothing: the work
    /// still stands on a folder its writer never read.
    pub async fn folder_moved(
        &mut self,
        client: &SynsClient,
        token: Option<&str>,
    ) -> Result<bool, CliError> {
        // 1 — the newest version that changed the folder.
        let page = match client
            .list_versions(&self.repo_id, token, 1, 0, Some(&self.folder))
            .await
        {
            Ok((page, _raw)) => page,
            Err(err) => return self.unread(err),
        };
        // 2 — a page the server did not narrow to the folder.
        if page
            .data
            .iter()
            .any(|entry| !changed_the_folder(entry, &self.folder))
        {
            return Err(CliError::FolderWriteUnsupported {
                folder: self.folder.clone(),
            });
        }
        let newest = page.data.first().map(|entry| (entry.version, &entry.sha));

        // 3 — the work stands on the folder's newest change.
        if let Some((_, sha)) = newest
            && *sha == self.since
        {
            return Ok(false);
        }

        // 4 — no change of the folder among the commits the walk scans:
        // the head's version tells whether those reach back past `since`.
        let head_version = match newest {
            Some(_) => None,
            None => {
                let head = match client.list_versions(&self.repo_id, token, 1, 0, None).await {
                    Ok((head, _raw)) => head,
                    Err(err) => return self.unread(err),
                };
                match head.data.first() {
                    Some(entry) if entry.version > SCAN_BOUND => Some(entry.version),
                    _ => return Ok(false),
                }
            }
        };

        // 5 — the number of the commit the work stands on, read once.
        let since_version = match self.since_version {
            Some(version) => version,
            None => match client.get_version(&self.repo_id, token, &self.since).await {
                Ok((entry, _raw)) => {
                    self.since_version = Some(entry.version);
                    entry.version
                }
                // A parent naming no commit of the holder reads as changed.
                Err(CliError::Api {
                    status: Some(404),
                    ref error,
                    ..
                }) if error == "not_found" => return Ok(true),
                Err(err) => return self.unread(err),
            },
        };

        // 6
        Ok(match (newest, head_version) {
            (Some((version, _)), _) => version > since_version,
            (None, Some(head)) => head.saturating_sub(since_version) > SCAN_BOUND,
            (None, None) => false,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use wiremock::matchers::{method, path, query_param, query_param_is_missing};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    const REPO: &str = "alice/work";
    const FOLDER: &str = "clients/vela/q3-board";

    fn version(number: u32, sha: &str, changed: &[&str]) -> serde_json::Value {
        serde_json::json!({
            "version": number, "sha": sha, "parentSha": null, "message": "m",
            "messageBody": null, "author": "alice",
            "createdAt": "2026-01-01T00:00:00Z", "filesChanged": changed,
        })
    }

    fn page(entries: Vec<serde_json::Value>) -> serde_json::Value {
        let total = entries.len();
        serde_json::json!({"data": entries, "total": total, "limit": 1, "offset": 0})
    }

    /// A deployment answering the folder page with `folder_page`, the
    /// page asked with no path with `head` at the newest version, and
    /// `EP-get-version` at `s7` with `single`.
    async fn deployment(
        folder_page: ResponseTemplate,
        head: Option<u32>,
        single: ResponseTemplate,
    ) -> MockServer {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path(format!("/api/v1/repos/{REPO}/versions")))
            .and(query_param("path", FOLDER))
            .and(query_param("limit", "1"))
            .and(query_param("offset", "0"))
            .respond_with(folder_page)
            .mount(&server)
            .await;
        let head_page = match head {
            Some(number) => page(vec![version(number, "head", &["README.md"])]),
            None => page(Vec::new()),
        };
        Mock::given(method("GET"))
            .and(path(format!("/api/v1/repos/{REPO}/versions")))
            .and(query_param_is_missing("path"))
            .and(query_param("limit", "1"))
            .and(query_param("offset", "0"))
            .respond_with(ResponseTemplate::new(200).set_body_json(head_page))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path(format!("/api/v1/repos/{REPO}/versions/s7")))
            .respond_with(single)
            .mount(&server)
            .await;
        server
    }

    fn folder_page(entries: Vec<serde_json::Value>) -> ResponseTemplate {
        ResponseTemplate::new(200).set_body_json(page(entries))
    }

    fn version_seven() -> ResponseTemplate {
        ResponseTemplate::new(200).set_body_json(version(7, "s7", &["README.md"]))
    }

    fn check(since: &str, since_version: Option<u32>) -> FolderCheck {
        FolderCheck {
            repo_id: REPO.to_string(),
            folder: FOLDER.to_string(),
            since: since.to_string(),
            since_version,
        }
    }

    async fn single_version_reads(server: &MockServer) -> usize {
        server
            .received_requests()
            .await
            .unwrap()
            .iter()
            .filter(|r| {
                r.url
                    .path()
                    .starts_with(&format!("/api/v1/repos/{REPO}/versions/"))
            })
            .count()
    }

    async fn ask(server: &MockServer, check: &mut FolderCheck) -> Result<bool, CliError> {
        let client = SynsClient::new(&server.uri()).unwrap();
        check.folder_moved(&client, Some("t")).await
    }

    // SPEC u292 Tests, `folder_moved_answers_each_case`, in the row's
    // order.
    #[tokio::test]
    async fn folder_moved_answers_each_case() {
        let under = |name: &str| format!("{FOLDER}/{name}");

        // An empty folder page with the head at version 50 and no number.
        let server = deployment(folder_page(Vec::new()), Some(50), version_seven()).await;
        assert!(!ask(&server, &mut check("s7", None)).await.unwrap());
        assert_eq!(single_version_reads(&server).await, 0);

        // An empty folder page with the head past the scan bound.
        let server = deployment(folder_page(Vec::new()), Some(10_002), version_seven()).await;
        assert!(ask(&server, &mut check("s1", Some(1))).await.unwrap());

        // The folder's newest change is the version the work stands on.
        let server = deployment(
            folder_page(vec![version(5, "s5", &[&under("board.json")])]),
            Some(9),
            version_seven(),
        )
        .await;
        assert!(!ask(&server, &mut check("s5", None)).await.unwrap());

        // A change before `since`, its number read once however often
        // the check is asked.
        let server = deployment(
            folder_page(vec![version(3, "s3", &[&under("board.json")])]),
            Some(9),
            version_seven(),
        )
        .await;
        let mut asked_twice = check("s7", None);
        assert!(!ask(&server, &mut asked_twice).await.unwrap());
        assert!(!ask(&server, &mut asked_twice).await.unwrap());
        assert_eq!(single_version_reads(&server).await, 1);

        // A change after `since`.
        let server = deployment(
            folder_page(vec![version(8, "s8", &[&under("notes.md")])]),
            Some(9),
            version_seven(),
        )
        .await;
        assert!(ask(&server, &mut check("s7", Some(7))).await.unwrap());

        // A parent naming no commit of the holder.
        let server = deployment(
            folder_page(vec![version(8, "s8", &[&under("notes.md")])]),
            Some(9),
            ResponseTemplate::new(404).set_body_json(serde_json::json!({"error": "not_found"})),
        )
        .await;
        assert!(ask(&server, &mut check("s7", None)).await.unwrap());

        // A folder page refused under a `transient` code.
        let limited =
            ResponseTemplate::new(429).set_body_json(serde_json::json!({"error": "rate_limited"}));
        let server = deployment(limited, Some(9), version_seven()).await;
        assert!(ask(&server, &mut check("s7", Some(7))).await.unwrap());
        let refusal = CliError::Api {
            status: Some(429),
            error: "rate_limited".to_string(),
            context: None,
        };
        assert_eq!(
            unread_check_line(FOLDER, &refusal),
            format!(
                "warning: the history of the folder {FOLDER} could not be read, so the run answers as though the folder moved: {refusal}"
            )
        );

        // A page the server did not narrow to the folder.
        let server = deployment(
            folder_page(vec![version(8, "s8", &["README.md"])]),
            Some(9),
            version_seven(),
        )
        .await;
        match ask(&server, &mut check("s7", Some(7))).await {
            Err(CliError::FolderWriteUnsupported { folder }) => assert_eq!(folder, FOLDER),
            other => panic!("expected the unsupported-server refusal, got {other:?}"),
        }
    }

    // SPEC u292 Behaviour, `FolderCheck::folder_moved` 1: a refusal of
    // any class but `transient` ends the check on it.
    #[tokio::test]
    async fn a_refusal_of_another_class_ends_the_check_on_it() {
        let server = deployment(
            ResponseTemplate::new(500)
                .set_body_json(serde_json::json!({"error": "internal_error"})),
            Some(9),
            version_seven(),
        )
        .await;
        match ask(&server, &mut check("s7", Some(7))).await {
            Err(CliError::Api {
                status: Some(500),
                error,
                ..
            }) => assert_eq!(error, "internal_error"),
            other => panic!("expected the server's refusal, got {other:?}"),
        }
    }
}
