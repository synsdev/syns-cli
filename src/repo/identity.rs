//! A shared folder's identity worked through from a folder bound to it
//! (SPEC u302): the head a run through an identity stands on, the marker
//! telling an identity checkout apart, and the folder's return to its
//! holder once its identity is gone.

use crate::client::{SynsClient, VersionEntry};
use crate::config::StoreRoots;
use crate::errors::CliError;
use crate::push::working_copy::WorkingCopy;
use crate::repo::folder::FolderScope;
use crate::repo::syns_yaml::{IdentityForm, identity_form_text};

/// The `shared_as` string a served identity file's text carries in the
/// folder form, none where the text is the root form, the key is absent
/// or holds anything but a string — read as `folder_shared_as` reads a
/// file on disk.
fn served_shared_as(text: &str) -> Option<String> {
    let value: serde_yaml::Value = serde_yaml::from_str(text).ok()?;
    value.get("holder")?;
    value
        .get("shared_as")
        .and_then(serde_yaml::Value::as_str)
        .map(str::to_string)
}

/// Whether a refusal is `NOT_FOUND`.
fn is_not_found(err: &CliError) -> bool {
    matches!(err, CliError::Api { status: Some(404), error, .. } if error == "not_found")
}

/// Whether a refusal is the `VALIDATION_ERROR` a repository holding no
/// commit answers a read with.
fn is_no_commit(err: &CliError) -> bool {
    matches!(err, CliError::Api { status: Some(422), error, .. } if error == "validation_error")
}

/// The newest version the identity at `address` lists, its number and
/// hash as served, none where its history lists none (SPEC u302
/// Behaviour, `identity_head` 1–2, `D-119`).
pub async fn identity_head(
    client: &SynsClient,
    token: Option<&str>,
    address: &str,
) -> Result<Option<VersionEntry>, CliError> {
    // 1 — one page at `limit` 1, naming no path.
    let (page, _raw) = client.list_versions(address, token, 1, 0, None).await?;
    // 2
    Ok(page.data.into_iter().next())
}

/// The holder and the recorded path where the root `.syns.yaml` of
/// `owner/name` at `version`, or the tip, is the folder form naming `name`
/// under `shared_as` and a holder owned by `owner`, letter case aside;
/// none for every other answer (SPEC u302 Behaviour,
/// `check_identity_marker` 1–2, `D-118`, `D-119`). It writes nothing.
pub async fn check_identity_marker(
    client: &SynsClient,
    token: Option<&str>,
    owner: &str,
    name: &str,
    version: Option<&str>,
) -> Result<Option<(String, String)>, CliError> {
    // 1 — the root identity file, no file and no commit answering none.
    let repo_id = format!("{owner}/{name}");
    let raw = match client
        .get_raw(&repo_id, token, ".syns.yaml", version, None)
        .await
    {
        Ok(raw) => raw,
        Err(err) if is_not_found(&err) || is_no_commit(&err) => return Ok(None),
        Err(err) => return Err(err),
    };

    // 2 — the folder form naming this identity under its holder's owner.
    let Ok(text) = String::from_utf8(raw.bytes) else {
        return Ok(None);
    };
    let Ok(IdentityForm::Folder { holder, path }) = identity_form_text(&text) else {
        return Ok(None);
    };
    let Some(shared_as) = served_shared_as(&text) else {
        return Ok(None);
    };
    let holder_owner = holder.split_once('/').map(|(owner, _)| owner);
    if !holder_owner.is_some_and(|holder_owner| holder_owner.eq_ignore_ascii_case(owner))
        || !shared_as.eq_ignore_ascii_case(name)
    {
        return Ok(None);
    }
    Ok(Some((holder, path)))
}

/// The folder bound to its holder once the holder's `.syns.yaml` at the
/// recorded path, read through the holder at the tip, is the folder form
/// naming that holder and path and names no `identity` under `shared_as`
/// — the identity copy's base carried to the holder's copy at the
/// folder's directory; none, nothing written, on every other answer and
/// every refusal (SPEC u302 Behaviour, `return_to_holder` 1–3, `D-120`).
pub async fn return_to_holder(
    stores: &StoreRoots,
    client: &SynsClient,
    token: Option<&str>,
    scope: &FolderScope,
) -> Result<Option<FolderScope>, CliError> {
    let Some(identity) = &scope.identity else {
        return Ok(None);
    };
    // 1 — the holder's file at the recorded path, every refusal none.
    let holder = scope.holder();
    let Ok(raw) = client
        .get_raw(
            &holder,
            token,
            &format!("{}/.syns.yaml", scope.path),
            None,
            None,
        )
        .await
    else {
        return Ok(None);
    };

    // 2 — the folder form of this holder and path, naming no identity.
    let Ok(text) = String::from_utf8(raw.bytes) else {
        return Ok(None);
    };
    let Ok(IdentityForm::Folder {
        holder: named,
        path,
    }) = identity_form_text(&text)
    else {
        return Ok(None);
    };
    if !named.eq_ignore_ascii_case(&holder) || path != scope.path {
        return Ok(None);
    }
    if served_shared_as(&text).is_some_and(|shared_as| shared_as.eq_ignore_ascii_case(identity)) {
        return Ok(None);
    }

    // 3 — the base carried to the holder's copy, the scope rebound.
    let copy = WorkingCopy::open(stores, &scope.owner, &scope.name, &scope.dir)?;
    copy.carry_base(&scope.address())?;
    let mut returned = scope.clone();
    returned.identity = None;
    Ok(Some(returned))
}

#[cfg(test)]
mod tests {
    use super::*;
    use wiremock::matchers::{method, path, query_param, query_param_is_missing};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn version(number: u32, sha: &str) -> serde_json::Value {
        serde_json::json!({
            "version": number, "sha": sha, "parentSha": null, "message": "m",
            "messageBody": null, "author": "alice",
            "createdAt": "2026-01-01T00:00:00Z", "filesChanged": ["document.html"],
        })
    }

    // SPEC u302 Behaviour, `identity_head` 1–2: one page at `limit` 1
    // naming no path, its version answered, none for an empty page.
    #[tokio::test]
    async fn the_identity_head_is_the_newest_listed_version() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/v1/repos/alice/docs-q3-plan/versions"))
            .and(query_param("limit", "1"))
            .and(query_param("offset", "0"))
            .and(query_param_is_missing("path"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "data": [version(4, "h4")], "total": 4, "limit": 1, "offset": 0,
            })))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/api/v1/repos/alice/empty/versions"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "data": [], "total": 0, "limit": 1, "offset": 0,
            })))
            .mount(&server)
            .await;
        let client = SynsClient::new(&server.uri()).unwrap();
        let head = identity_head(&client, Some("t"), "alice/docs-q3-plan")
            .await
            .unwrap()
            .expect("a head");
        assert_eq!((head.version, head.sha.as_str()), (4, "h4"));
        assert!(
            identity_head(&client, Some("t"), "alice/empty")
                .await
                .unwrap()
                .is_none()
        );
    }

    /// A deployment answering `alice/docs-q3-plan`'s root `.syns.yaml`
    /// with `answer`.
    async fn marker(answer: ResponseTemplate) -> (MockServer, SynsClient) {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/v1/repos/alice/docs-q3-plan/raw/.syns.yaml"))
            .respond_with(answer)
            .mount(&server)
            .await;
        let client = SynsClient::new(&server.uri()).unwrap();
        (server, client)
    }

    async fn marked(answer: ResponseTemplate) -> Option<(String, String)> {
        let (_server, client) = marker(answer).await;
        check_identity_marker(&client, Some("t"), "alice", "docs-q3-plan", None)
            .await
            .unwrap()
    }

    fn bytes(text: &str) -> ResponseTemplate {
        ResponseTemplate::new(200).set_body_bytes(text.as_bytes().to_vec())
    }

    // SPEC u302 Behaviour, `check_identity_marker` 1–2.
    #[tokio::test]
    async fn the_marker_answers_only_the_folder_form_naming_the_identity() {
        assert_eq!(
            marked(bytes(
                "holder: Alice/Docs\npath: q3-plan\nshared_as: Docs-Q3-Plan\n"
            ))
            .await,
            Some(("Alice/Docs".to_string(), "q3-plan".to_string()))
        );
        for refused in [
            "holder: alice/docs\npath: q3-plan\nshared_as: docs-other\n",
            "holder: bob/docs\npath: q3-plan\nshared_as: docs-q3-plan\n",
            "holder: alice/docs\npath: q3-plan\n",
            "owner: alice\nname: docs-q3-plan\n",
            "not: [a, form\n",
        ] {
            assert_eq!(marked(bytes(refused)).await, None, "{refused:?}");
        }
        assert_eq!(
            marked(ResponseTemplate::new(200).set_body_bytes(vec![0xff, 0xfe, 0x00])).await,
            None
        );
        assert_eq!(
            marked(
                ResponseTemplate::new(404).set_body_json(serde_json::json!({"error": "not_found"}))
            )
            .await,
            None
        );
        assert_eq!(
            marked(
                ResponseTemplate::new(422)
                    .set_body_json(serde_json::json!({"error": "validation_error"}))
            )
            .await,
            None
        );
    }

    // `check_identity_marker` 1: the file is read at the version asked, and
    // a refusal other than no file or no commit ends the run.
    #[tokio::test]
    async fn the_marker_reads_at_the_version_asked_and_raises_other_refusals() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/v1/repos/alice/docs-q3-plan/raw/.syns.yaml"))
            .and(query_param("ref", "2"))
            .respond_with(bytes("holder: alice/docs\npath: q3-plan\n"))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/api/v1/repos/alice/forbidden/raw/.syns.yaml"))
            .respond_with(
                ResponseTemplate::new(403).set_body_json(serde_json::json!({"error": "forbidden"})),
            )
            .mount(&server)
            .await;
        let client = SynsClient::new(&server.uri()).unwrap();
        assert_eq!(
            check_identity_marker(&client, Some("t"), "alice", "docs-q3-plan", Some("2"))
                .await
                .unwrap(),
            None
        );
        assert!(
            check_identity_marker(&client, Some("t"), "alice", "forbidden", None)
                .await
                .is_err()
        );
    }

    /// `U/q3-plan` bound to `alice/docs-q3-plan`, its identity copy
    /// recording base `h4`, beside the cache.
    fn bound_folder() -> (
        tempfile::TempDir,
        tempfile::TempDir,
        FolderScope,
        StoreRoots,
    ) {
        let cache = tempfile::tempdir().unwrap();
        let tree = tempfile::tempdir().unwrap();
        let dir = std::fs::canonicalize(tree.path()).unwrap().join("q3-plan");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join(".syns.yaml"),
            "holder: alice/docs\npath: q3-plan\nshared_as: docs-q3-plan\n",
        )
        .unwrap();
        let stores = StoreRoots::resolve(Some(cache.path()), cache.path(), cache.path());
        let scope = crate::repo::folder::resolve_folder_scope(&dir)
            .unwrap()
            .expect("bound");
        let copy = WorkingCopy::open_folder(&stores, &scope).unwrap();
        copy.record_base(
            "h4",
            std::collections::HashMap::from([("document.html".to_string(), "b1".to_string())]),
        )
        .unwrap();
        (cache, tree, scope, stores)
    }

    async fn returned(answer: ResponseTemplate) -> (Option<FolderScope>, Option<String>) {
        let (_cache, _tree, scope, stores) = bound_folder();
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/v1/repos/alice/docs/raw/q3-plan/.syns.yaml"))
            .respond_with(answer)
            .mount(&server)
            .await;
        let client = SynsClient::new(&server.uri()).unwrap();
        let answer = return_to_holder(&stores, &client, Some("t"), &scope)
            .await
            .unwrap();
        let holder_base = WorkingCopy::open_existing(&stores, "alice", "docs", &scope.dir)
            .unwrap()
            .and_then(|copy| copy.base())
            .and_then(|base| base.commit_sha().map(String::from));
        (answer, holder_base)
    }

    // SPEC u302 Behaviour, `return_to_holder` 1–3.
    #[tokio::test]
    async fn the_folder_returns_to_its_holder_only_once_its_file_names_no_identity() {
        let (answer, holder_base) = returned(bytes("holder: alice/docs\npath: q3-plan\n")).await;
        let scope = answer.expect("returned");
        assert_eq!(scope.identity, None);
        assert_eq!(scope.address(), "alice/docs");
        assert_eq!(holder_base.as_deref(), Some("h4"));

        for (still, why) in [
            (
                bytes("holder: alice/docs\npath: q3-plan\nshared_as: Docs-Q3-Plan\n"),
                "still naming the identity",
            ),
            (
                bytes("holder: alice/docs\npath: elsewhere\n"),
                "another path",
            ),
            (
                ResponseTemplate::new(404).set_body_json(serde_json::json!({"error": "not_found"})),
                "a refused read",
            ),
        ] {
            let (answer, holder_base) = returned(still).await;
            assert_eq!(answer, None, "{why}");
            assert_eq!(holder_base, None, "{why}: the holder's copy was written");
        }
    }
}
