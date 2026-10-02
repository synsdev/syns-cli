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
///
/// SPEC u302: `holder` is the holder's `OWNER/NAME` on a check asked
/// through a shared folder's identity — `repo_id` that identity — through
/// which a `since` the identity does not list is numbered; none on a check
/// asked through a holder.
#[derive(Debug, Clone)]
pub struct FolderCheck {
    pub repo_id: String,
    pub folder: String,
    pub since: String,
    pub since_version: Option<u32>,
    pub holder: Option<String>,
}

/// Whether `entry` changed a path equal to `folder` or lying under it.
fn changed_the_folder(entry: &VersionEntry, folder: &str) -> bool {
    entry
        .files_changed
        .iter()
        .any(|path| path == folder || lies_under(path, folder))
}

/// Whether `repo_id`'s history shows `landed` as the newest version
/// changing a path at or under `folder` and no version numbered after
/// `since` and before `landed` changing one (SPEC u304 Behaviour,
/// `folder_unmoved_until` 1–4): false on every refusal and every answer
/// leaving either unshown, and nothing written to either stream — the
/// caller keeps a copy's commit on false and loses nothing else. SPEC
/// u307 `folder_unmoved_until` 1: with no `folder`, the page names no
/// path and its every entry counts as one that changed what the copy
/// covers — `repo_id`'s whole history, as an identity copy covers it.
pub async fn folder_unmoved_until(
    client: &SynsClient,
    token: Option<&str>,
    repo_id: &str,
    folder: Option<&str>,
    since: &str,
    landed: &str,
) -> bool {
    // 1 — the folder's two newest changes, or the repository's.
    let Ok((page, _raw)) = client.list_versions(repo_id, token, 2, 0, folder).await else {
        return false;
    };
    // 2 — a page not narrowed to the folder, or one whose newest change
    // is not the landed version.
    if let Some(folder) = folder
        && page
            .data
            .iter()
            .any(|entry| !changed_the_folder(entry, folder))
    {
        return false;
    }
    match page.data.first() {
        Some(newest) if newest.sha == landed => {}
        _ => return false,
    }
    // 3 — the change before the landed one is the base's own commit.
    let Some(before) = page.data.get(1) else {
        return false;
    };
    if before.sha == since {
        return true;
    }
    // 4 — that change numbered at or below the base's commit.
    match client.get_version(repo_id, token, since).await {
        Ok((entry, _raw)) => before.version <= entry.version,
        Err(_) => false,
    }
}

impl FolderCheck {
    /// A read of the check of `folder` refused: a `transient` refusal
    /// answers as a folder that moved once the unread-check line is
    /// written, and any other refusal ends the check on it.
    fn unread(folder: &str, err: CliError) -> Result<bool, CliError> {
        match error_class(&err) {
            Some(ErrorClass::Transient) => {
                eprintln!("{}", unread_check_line(folder, &err));
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
        if let Some(holder) = self.holder.clone() {
            return self.identity_moved(client, token, &holder).await;
        }
        let folder = self.folder.clone();
        let numbering = [self.repo_id.clone()];
        self.moved_at(client, token, &folder, &numbering).await
    }

    /// Whether a version of the identity numbered after `since` changed a
    /// path at or under `within`, a path counted from the identity folder
    /// (SPEC u307 Behaviour, `FolderCheck::folder_moved_within` 1–3): u292's
    /// steps over the identity's history at `within` alone, `since`
    /// numbered through the identity and, where it answers `NOT_FOUND`,
    /// through `holder`, so a version changing other identity paths alone
    /// never answers true.
    pub async fn folder_moved_within(
        &mut self,
        client: &SynsClient,
        token: Option<&str>,
        within: &str,
    ) -> Result<bool, CliError> {
        let numbering: Vec<String> = std::iter::once(self.repo_id.clone())
            .chain(self.holder.clone())
            .collect();
        self.moved_at(client, token, within, &numbering).await
    }

    /// SPEC u292 Behaviour, `FolderCheck::folder_moved` 1–6, over
    /// `repo_id`'s history at `folder`, the commit the work stands on
    /// numbered through each of `numbering` in turn, a `NOT_FOUND` passed
    /// over and one from every repository read as a folder that moved.
    async fn moved_at(
        &mut self,
        client: &SynsClient,
        token: Option<&str>,
        folder: &str,
        numbering: &[String],
    ) -> Result<bool, CliError> {
        // 1 — the newest version that changed the folder.
        let page = match client
            .list_versions(&self.repo_id, token, 1, 0, Some(folder))
            .await
        {
            Ok((page, _raw)) => page,
            Err(err) => return Self::unread(folder, err),
        };
        // 2 — a page the server did not narrow to the folder.
        if page
            .data
            .iter()
            .any(|entry| !changed_the_folder(entry, folder))
        {
            return Err(CliError::FolderWriteUnsupported {
                folder: folder.to_string(),
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
                    Err(err) => return Self::unread(folder, err),
                };
                match head.data.first() {
                    Some(entry) if entry.version > SCAN_BOUND => Some(entry.version),
                    _ => return Ok(false),
                }
            }
        };

        // 5 — the number of the commit the work stands on, read once; a
        // parent no repository numbers reads as changed.
        let since_version = match self.number_since(client, token, numbering).await {
            Ok(Some(version)) => version,
            Ok(None) => return Ok(true),
            Err(err) => return Self::unread(folder, err),
        };

        // 6
        Ok(match (newest, head_version) {
            (Some((version, _)), _) => version > since_version,
            (None, Some(head)) => head.saturating_sub(since_version) > SCAN_BOUND,
            (None, None) => false,
        })
    }

    /// `folder_moved` through a shared folder's identity, whose every
    /// listed version changed the folder (SPEC u302 Behaviour,
    /// `FolderCheck::folder_moved` 1–4).
    async fn identity_moved(
        &mut self,
        client: &SynsClient,
        token: Option<&str>,
        holder: &str,
    ) -> Result<bool, CliError> {
        // 1 — the identity's newest version, naming no path.
        let page = match client.list_versions(&self.repo_id, token, 1, 0, None).await {
            Ok((page, _raw)) => page,
            Err(err) => return Self::unread(&self.folder, err),
        };
        // 4 — an identity listing no version: u292's steps 4 and 6 over a
        // head page that is this same empty page.
        let Some(newest) = page.data.first() else {
            return Ok(false);
        };
        // 2
        if newest.sha == self.since {
            return Ok(false);
        }
        // 3 — `since` numbered through the identity, then the holder.
        let numbering = [self.repo_id.clone(), holder.to_string()];
        let since_version = match self.number_since(client, token, &numbering).await {
            Ok(Some(version)) => version,
            Ok(None) => return Ok(true),
            Err(err) => return Self::unread(&self.folder, err),
        };
        // 4
        Ok(newest.version > since_version)
    }

    /// The number of `since`, the one already held answered as it stands,
    /// and otherwise read through each of `numbering` in turn and kept: a
    /// `404` `not_found` passes to the next repository, none where every
    /// one answers it, and any other refusal is answered as it came.
    async fn number_since(
        &mut self,
        client: &SynsClient,
        token: Option<&str>,
        numbering: &[String],
    ) -> Result<Option<u32>, CliError> {
        if let Some(version) = self.since_version {
            return Ok(Some(version));
        }
        for repo_id in numbering {
            match client.get_version(repo_id, token, &self.since).await {
                Ok((entry, _raw)) => {
                    self.since_version = Some(entry.version);
                    return Ok(Some(entry.version));
                }
                Err(CliError::Api {
                    status: Some(404),
                    ref error,
                    ..
                }) if error == "not_found" => continue,
                Err(err) => return Err(err),
            }
        }
        Ok(None)
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
            holder: None,
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

    // SPEC u304 Tests, the row of this name.
    #[tokio::test]
    async fn a_folder_counts_unmoved_until_the_landed_version_only_where_its_history_shows_it() {
        let changed = &["budget/board/.syns.yaml"][..];
        let pages = [
            ResponseTemplate::new(200).set_body_json(page(vec![
                version(3, "h3", changed),
                version(1, "h1", changed),
            ])),
            ResponseTemplate::new(200).set_body_json(page(vec![
                version(3, "h3", changed),
                version(2, "h2", changed),
            ])),
            ResponseTemplate::new(200).set_body_json(page(vec![version(3, "h3", changed)])),
            ResponseTemplate::new(200).set_body_json(page(vec![
                version(4, "h4", changed),
                version(3, "h3", changed),
            ])),
            ResponseTemplate::new(200).set_body_json(page(vec![version(3, "h3", &["README.md"])])),
            ResponseTemplate::new(503).set_body_json(serde_json::json!({"error": "unavailable"})),
        ];
        let mut answers = Vec::new();
        let mut first_reads = None;
        for answer in pages {
            let server = MockServer::start().await;
            Mock::given(method("GET"))
                .and(path(format!("/api/v1/repos/{REPO}/versions")))
                .and(query_param("path", "budget"))
                .and(query_param("limit", "2"))
                .and(query_param("offset", "0"))
                .respond_with(answer)
                .mount(&server)
                .await;
            Mock::given(method("GET"))
                .and(path(format!("/api/v1/repos/{REPO}/versions/h1")))
                .respond_with(ResponseTemplate::new(200).set_body_json(version(
                    1,
                    "h1",
                    &["README.md"],
                )))
                .mount(&server)
                .await;
            let client = SynsClient::new(&server.uri()).unwrap();
            answers.push(
                folder_unmoved_until(&client, Some("t"), REPO, Some("budget"), "h1", "h3").await,
            );
            if first_reads.is_none() {
                first_reads = Some(single_version_reads(&server).await);
            }
        }
        assert_eq!(answers, [true, false, false, false, false, false]);
        assert_eq!(first_reads, Some(0));
    }

    // CR1-1: a base commit that changed nothing under the folder counts
    // the folder unmoved where the folder's change before the landed one
    // is numbered at or below it, and moved where that number is unread.
    #[tokio::test]
    async fn a_folder_counts_unmoved_past_a_base_commit_that_changed_nothing_under_it() {
        let changed = &["budget/document.html"][..];
        let numbered = [
            ResponseTemplate::new(200).set_body_json(version(1, "h1", &["README.md"])),
            ResponseTemplate::new(503).set_body_json(serde_json::json!({"error": "unavailable"})),
        ];
        let mut answers = Vec::new();
        for answer in numbered {
            let server = MockServer::start().await;
            Mock::given(method("GET"))
                .and(path(format!("/api/v1/repos/{REPO}/versions")))
                .and(query_param("path", "budget"))
                .and(query_param("limit", "2"))
                .and(query_param("offset", "0"))
                .respond_with(ResponseTemplate::new(200).set_body_json(page(vec![
                    version(3, "h3", changed),
                    version(0, "h0", changed),
                ])))
                .mount(&server)
                .await;
            Mock::given(method("GET"))
                .and(path(format!("/api/v1/repos/{REPO}/versions/h1")))
                .respond_with(answer)
                .mount(&server)
                .await;
            let client = SynsClient::new(&server.uri()).unwrap();
            answers.push(
                folder_unmoved_until(&client, Some("t"), REPO, Some("budget"), "h1", "h3").await,
            );
        }
        assert_eq!(answers, [true, false]);
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

    /// A deployment answering the identity's version list at `limit` 1
    /// with `newest`, and `EP-get-version` at `h5` with `through_identity`
    /// at the identity and `through_holder` at the holder.
    async fn identity_deployment(
        newest: Option<(u32, &str)>,
        through_identity: ResponseTemplate,
        through_holder: ResponseTemplate,
    ) -> MockServer {
        let server = MockServer::start().await;
        let entries = newest
            .map(|(number, sha)| vec![version(number, sha, &["document.html"])])
            .unwrap_or_default();
        Mock::given(method("GET"))
            .and(path("/api/v1/repos/alice/docs-q3-plan/versions"))
            .and(query_param_is_missing("path"))
            .and(query_param("limit", "1"))
            .and(query_param("offset", "0"))
            .respond_with(ResponseTemplate::new(200).set_body_json(page(entries)))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/api/v1/repos/alice/docs-q3-plan/versions/h5"))
            .respond_with(through_identity)
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/api/v1/repos/alice/docs/versions/h5"))
            .respond_with(through_holder)
            .mount(&server)
            .await;
        server
    }

    fn identity_check() -> FolderCheck {
        FolderCheck {
            repo_id: "alice/docs-q3-plan".to_string(),
            folder: "q3-plan".to_string(),
            since: "h5".to_string(),
            since_version: None,
            holder: Some("alice/docs".to_string()),
        }
    }

    fn not_found() -> ResponseTemplate {
        ResponseTemplate::new(404).set_body_json(serde_json::json!({"error": "not_found"}))
    }

    fn version_five() -> ResponseTemplate {
        ResponseTemplate::new(200).set_body_json(version(5, "h5", &["budget/document.html"]))
    }

    // SPEC u307 Tests,
    // `a_check_within_a_folder_beneath_an_identity_reads_that_folders_history_alone`.
    #[tokio::test]
    async fn a_check_within_a_folder_beneath_an_identity_reads_that_folders_history_alone() {
        let pages = [
            version(3, "h3", &["appendix/x"]),
            version(6, "h6", &["appendix/x"]),
            version(6, "h6", &["notes.html"]),
        ];
        let mut answers = Vec::new();
        let mut numbered = Vec::new();
        for entry in pages {
            let server = MockServer::start().await;
            Mock::given(method("GET"))
                .and(path("/api/v1/repos/alice/docs-q3-plan/versions"))
                .and(query_param("path", "appendix"))
                .and(query_param("limit", "1"))
                .and(query_param("offset", "0"))
                .respond_with(ResponseTemplate::new(200).set_body_json(page(vec![entry])))
                .mount(&server)
                .await;
            Mock::given(method("GET"))
                .and(path("/api/v1/repos/alice/docs-q3-plan/versions/h5"))
                .respond_with(not_found())
                .mount(&server)
                .await;
            Mock::given(method("GET"))
                .and(path("/api/v1/repos/alice/docs/versions/h5"))
                .respond_with(version_five())
                .mount(&server)
                .await;
            let client = SynsClient::new(&server.uri()).unwrap();
            let mut check = identity_check();
            answers.push(
                check
                    .folder_moved_within(&client, Some("t"), "appendix")
                    .await,
            );
            numbered.push(check.since_version);
        }
        assert!(matches!(answers[0], Ok(false)), "{:?}", answers[0]);
        assert_eq!(numbered[0], Some(5));
        assert!(matches!(answers[1], Ok(true)), "{:?}", answers[1]);
        match &answers[2] {
            Err(CliError::FolderWriteUnsupported { folder }) => assert_eq!(folder, "appendix"),
            other => panic!("expected the unsupported-server refusal, got {other:?}"),
        }
    }

    // SPEC u302 Tests, `a_carried_base_the_identity_does_not_list_is_numbered_through_the_holder`.
    #[tokio::test]
    async fn a_carried_base_the_identity_does_not_list_is_numbered_through_the_holder() {
        let server = identity_deployment(Some((4, "h4")), not_found(), version_five()).await;
        let mut check = identity_check();
        assert!(!ask(&server, &mut check).await.unwrap());
        assert_eq!(check.since_version, Some(5));
        assert!(
            server
                .received_requests()
                .await
                .unwrap()
                .iter()
                .all(|r| r.url.query_pairs().all(|(key, _)| key != "path")),
            "a request named a path"
        );
    }

    // SPEC u302 Behaviour, `FolderCheck::folder_moved` 1–4 through an
    // identity: its newest version past `since` moved the folder, `since`
    // itself or an empty list did not, and `since` numbered by neither
    // reads as moved.
    #[tokio::test]
    async fn an_identity_check_answers_each_case() {
        let ok_five = || ResponseTemplate::new(200).set_body_json(version(5, "h5", &["d"]));
        let server = identity_deployment(Some((6, "h6")), ok_five(), not_found()).await;
        assert!(ask(&server, &mut identity_check()).await.unwrap());

        let server = identity_deployment(Some((5, "h5")), not_found(), not_found()).await;
        assert!(!ask(&server, &mut identity_check()).await.unwrap());

        let server = identity_deployment(None, not_found(), not_found()).await;
        assert!(!ask(&server, &mut identity_check()).await.unwrap());

        let server = identity_deployment(Some((6, "h6")), not_found(), not_found()).await;
        assert!(ask(&server, &mut identity_check()).await.unwrap());
    }
}
