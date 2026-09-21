//! `syns users QUERY` and `syns user [USERNAME]` — the two entries that
//! answer for a person rather than for a repository (SPEC u272,
//! `EP-users-search` and `EP-user-profile`), and the resolution of the
//! caller's own handle they share with nothing else.

use crate::auth::token::TokenStore;
use crate::client::{SynsClient, UserProfile};
use crate::commands::repos::{LIMIT_MIN, refuse_limit_outside};
use crate::config::Config;
use crate::errors::CliError;
use crate::output::Output;

/// The user-search entry's own ceiling, which is lower than the
/// paged-listing one (`INTERFACES.md`, `EP-users-search`).
pub const SEARCH_LIMIT_MAX: u32 = 50;

/// The window `syns users QUERY` sends where the caller names none.
pub const DEFAULT_SEARCH_LIMIT: u32 = 20;

/// The handle the machine's own credential names (SPEC u272 Behaviour,
/// `resolve_self_handle`).
///
/// The credential's recorded handle answers where it holds one, and the
/// session entry answers otherwise. That entry serves the JSON literal
/// `null` to a credential it does not honour, which the shipped decode
/// already refuses as `AUTH_REQUIRED` — so this branch adds no model of
/// its own and touches no call site that reads the session's user
/// (`PROTOTYPE.md` Constraints, `SPEC_REVIEW_R2.md` CF-01).
pub async fn resolve_self_handle(config: &Config) -> Result<String, CliError> {
    let store = TokenStore::new(config.credentials_path());

    // 1 — the handle the stored credential records.
    let token = store.read()?.ok_or(CliError::AuthRequired)?;
    if let Some(handle) = store.read_username()?
        && !handle.trim().is_empty()
    {
        return Ok(handle);
    }

    // 2 — the session entry, where the credential records none.
    let client = SynsClient::new(config.server_url())?;
    let (session, _raw) = client.get_session(&token).await?;

    // 3 — answer that handle.
    Ok(session.user.username)
}

pub async fn cmd_users(
    config: &Config,
    output: &Output,
    query: String,
    limit: u32,
) -> Result<(), CliError> {
    // 1 — refuse a page size outside the entry's own bound.
    refuse_limit_outside(limit, LIMIT_MIN, SEARCH_LIMIT_MAX)?;

    // 2 — ask the user-search entry under the stored credential. The
    //     rate refusal the entry raises carries no interval and no
    //     remaining budget, so the report names neither and offers no
    //     wait (`PROTOTYPE.md` NR-01).
    let token = TokenStore::new(config.credentials_path())
        .read()?
        .ok_or(CliError::AuthRequired)?;
    let client = SynsClient::new(config.server_url())?;
    let (response, raw) = client.search_users(&token, &query, limit).await?;

    // 3 — render the matches in the order the answer carried.
    if output.is_json() {
        output.json(&raw);
        return Ok(());
    }
    let rows: Vec<Vec<String>> = response
        .data
        .iter()
        .map(|user| vec![user.username.clone(), user.name.clone(), user.id.clone()])
        .collect();
    output.table(&["Username", "Name", "User ID"], rows);
    Ok(())
}

pub async fn cmd_user(
    config: &Config,
    output: &Output,
    username: Option<String>,
) -> Result<(), CliError> {
    // 1 and 2 — the positional as typed, or the caller's own handle.
    let handle = match username {
        Some(handle) => handle,
        None => resolve_self_handle(config).await?,
    };

    // 3 — ask the profile entry, carrying the stored credential where
    //     one stands; the entry decides which repository count it
    //     answers from who asked.
    let token = TokenStore::new(config.credentials_path())
        .read()
        .ok()
        .flatten();
    let client = SynsClient::new(config.server_url())?;
    let (profile, raw) = client.get_user_profile(&handle, token.as_deref()).await?;

    // 4 — render the profile, or write the served body.
    if output.is_json() {
        output.json(&raw);
        return Ok(());
    }
    output.table(&["Property", "Value"], profile_rows(&profile));
    if !profile.links.is_empty() {
        let rows: Vec<Vec<String>> = profile
            .links
            .iter()
            .map(|link| {
                vec![
                    link.kind.clone(),
                    link.value.clone(),
                    link.label.clone().unwrap_or_default(),
                ]
            })
            .collect();
        output.table(&["Kind", "Value", "Label"], rows);
    }
    Ok(())
}

/// The profile block (SPEC u272 Contract Surface, the profile render):
/// the handle, the display name, the join instant and the repository
/// count always, and then one row per metadata field the subject set —
/// a field left unset drawing no row at all.
pub(crate) fn profile_rows(profile: &UserProfile) -> Vec<Vec<String>> {
    let mut rows = vec![
        vec!["Username".to_string(), profile.username.clone()],
        vec!["Name".to_string(), profile.name.clone()],
        vec!["Joined".to_string(), profile.created_at.clone()],
        vec!["Repositories".to_string(), profile.repo_count.to_string()],
    ];
    for (label, value) in [
        ("Bio", &profile.bio),
        ("Location", &profile.location),
        ("Pronouns", &profile.pronouns),
        ("Company", &profile.company),
        ("Time zone", &profile.time_zone),
    ] {
        if let Some(value) = value {
            rows.push(vec![label.to_string(), value.clone()]);
        }
    }
    rows
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::client::u272_bodies as B;
    use serial_test::serial;
    use wiremock::matchers::{method, path, query_param};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn seed(dir: &std::path::Path, token: Option<&str>, username: Option<&str>) {
        if let Some(token) = token {
            TokenStore::new(dir.join("credentials.json"))
                .write_with_username(token, username)
                .unwrap();
        }
        std::env::set_current_dir(dir).unwrap();
        unsafe { std::env::set_var("SYNS_CONFIG_DIR", dir) };
    }

    fn unseed() {
        unsafe { std::env::remove_var("SYNS_CONFIG_DIR") };
    }

    // SPEC u272 Behaviour, `resolve_self_handle` 1: the stored handle
    // costs no session request.
    #[tokio::test]
    #[serial]
    async fn a_stored_handle_costs_no_session_request() {
        let dir = tempfile::tempdir().unwrap();
        seed(dir.path(), Some("u272-token"), Some("alice"));
        let server = MockServer::start().await;
        let config = Config::new(Some(&server.uri())).unwrap();

        let handle = resolve_self_handle(&config).await.unwrap();
        unseed();

        assert_eq!(handle, "alice");
        assert!(server.received_requests().await.unwrap().is_empty());
    }

    // SPEC u272 Behaviour, `resolve_self_handle` 1: no credential at
    // all refuses before any request.
    #[tokio::test]
    #[serial]
    async fn no_credential_refuses_before_any_request() {
        let dir = tempfile::tempdir().unwrap();
        seed(dir.path(), None, None);
        let server = MockServer::start().await;
        let config = Config::new(Some(&server.uri())).unwrap();

        let err = resolve_self_handle(&config).await.unwrap_err();
        unseed();

        assert!(matches!(err, CliError::AuthRequired));
        assert_eq!(err.exit_code(), 1);
        assert!(server.received_requests().await.unwrap().is_empty());
    }

    // SPEC u272 Behaviour, `resolve_self_handle` 2: a credential
    // recording no handle asks the session entry, whose bare `null`
    // refuses the shipped decode and raises the same refusal.
    #[tokio::test]
    #[serial]
    async fn a_session_answering_the_bare_null_refuses() {
        let dir = tempfile::tempdir().unwrap();
        seed(dir.path(), Some("u272-token"), None);
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/auth/get-session"))
            .respond_with(ResponseTemplate::new(200).set_body_string("null"))
            .mount(&server)
            .await;
        let config = Config::new(Some(&server.uri())).unwrap();

        let err = resolve_self_handle(&config).await.unwrap_err();
        unseed();

        assert!(matches!(err, CliError::AuthRequired));
        assert_eq!(server.received_requests().await.unwrap().len(), 1);
    }

    #[tokio::test]
    #[serial]
    async fn a_session_naming_a_user_answers_that_handle() {
        let dir = tempfile::tempdir().unwrap();
        seed(dir.path(), Some("u272-token"), None);
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/auth/get-session"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "session": {"token": "u272-token"},
                "user": {
                    "id": "u1", "name": "Alice", "username": "alice",
                    "email": "alice@example.test", "image": null,
                },
            })))
            .mount(&server)
            .await;
        let config = Config::new(Some(&server.uri())).unwrap();

        let handle = resolve_self_handle(&config).await.unwrap();
        unseed();

        assert_eq!(handle, "alice");
    }

    // SPEC u272 Behaviour, `cmd_users` 1: a page size outside the
    // entry's own bound is refused before any request.
    #[tokio::test]
    #[serial]
    async fn a_search_page_size_outside_the_entrys_bound_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        seed(dir.path(), Some("u272-token"), Some("alice"));
        let server = MockServer::start().await;
        let config = Config::new(Some(&server.uri())).unwrap();
        let output = Output::new(false);

        let err = cmd_users(&config, &output, "bart".into(), 51)
            .await
            .unwrap_err();
        unseed();

        assert_eq!(
            err.to_string(),
            "configuration error: --limit must be between 1 and 50 (got 51)"
        );
        assert!(server.received_requests().await.unwrap().is_empty());
    }

    // SPEC u272 Behaviour, `cmd_users` 2: no credential refuses before
    // any request.
    #[tokio::test]
    #[serial]
    async fn a_search_with_no_credential_refuses_before_any_request() {
        let dir = tempfile::tempdir().unwrap();
        seed(dir.path(), None, None);
        let server = MockServer::start().await;
        let config = Config::new(Some(&server.uri())).unwrap();
        let output = Output::new(false);

        let err = cmd_users(&config, &output, "bart".into(), DEFAULT_SEARCH_LIMIT)
            .await
            .unwrap_err();
        unseed();

        assert!(matches!(err, CliError::AuthRequired));
        assert!(server.received_requests().await.unwrap().is_empty());
    }

    // SPEC u272 Contract Surface, `cmd_users`: a query of one character
    // is sent rather than refused.
    #[tokio::test]
    #[serial]
    async fn a_query_of_one_character_is_sent_rather_than_refused() {
        let dir = tempfile::tempdir().unwrap();
        seed(dir.path(), Some("u272-token"), Some("alice"));
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/v1/users"))
            .and(query_param("q", "a"))
            .respond_with(ResponseTemplate::new(200).set_body_string(r#"{"data":[]}"#))
            .mount(&server)
            .await;
        let config = Config::new(Some(&server.uri())).unwrap();
        let output = Output::new(false);

        let result = cmd_users(&config, &output, "a".into(), DEFAULT_SEARCH_LIMIT).await;
        unseed();

        assert!(result.is_ok(), "got {:?}", result.err());
        assert_eq!(server.received_requests().await.unwrap().len(), 1);
    }

    // SPEC u272 Behaviour, `cmd_user` 2 and 3: an absent handle costs
    // at most one session request before the profile request, and a
    // handle given costs none.
    #[tokio::test]
    #[serial]
    async fn a_handle_given_costs_no_session_request() {
        let dir = tempfile::tempdir().unwrap();
        seed(dir.path(), Some("u272-token"), None);
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/v1/users/u272bob"))
            .respond_with(ResponseTemplate::new(200).set_body_string(B::PROFILE_LOCAL))
            .mount(&server)
            .await;
        let config = Config::new(Some(&server.uri())).unwrap();
        let output = Output::new(false);

        let result = cmd_user(&config, &output, Some("u272bob".into())).await;
        unseed();

        assert!(result.is_ok(), "got {:?}", result.err());
        let paths: Vec<String> = server
            .received_requests()
            .await
            .unwrap()
            .iter()
            .map(|r| r.url.path().to_string())
            .collect();
        assert_eq!(paths, vec!["/api/v1/users/u272bob".to_string()]);
    }

    // SPEC u272 Contract Surface, the profile render: a metadata field
    // the subject left unset draws no row at all.
    #[test]
    fn a_metadata_field_left_unset_draws_no_row() {
        let sparse: UserProfile = serde_json::from_str(B::PROFILE_LOCAL).unwrap();
        let rows = profile_rows(&sparse);
        let labels: Vec<&str> = rows.iter().map(|r| r[0].as_str()).collect();
        assert_eq!(labels, vec!["Username", "Name", "Joined", "Repositories"]);
        assert!(sparse.links.is_empty());

        let full: UserProfile = serde_json::from_str(B::PROFILE_SELF).unwrap();
        let full_rows = profile_rows(&full);
        let labels: Vec<&str> = full_rows.iter().map(|r| r[0].as_str()).collect();
        assert_eq!(
            labels,
            vec![
                "Username",
                "Name",
                "Joined",
                "Repositories",
                "Bio",
                "Location",
                "Company",
            ]
        );
    }
}
