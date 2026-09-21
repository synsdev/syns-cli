//! The shared read layer of the five read verbs (SPEC u270).
//!
//! Every read verb takes the same three options, resolves one repository
//! at one reference before it asks for any content, and sends that
//! reference's decimal ordinal as `ref` on every later request — so a
//! publication landing mid-run changes nothing the run reads, and a
//! time-travel read never pays the commit walk a full content hash costs
//! (`issues/148-ref-by-content-hash-resolved-by-scanning-every-commit`).

pub mod cache;

use crate::auth::token::TokenStore;
use crate::client::SynsClient;
use crate::commands::pull::is_repository_shape;
use crate::config::Config;
use crate::errors::CliError;
use crate::output::Output;
use crate::repo::if_repo::resolve_full_or_skip;

/// The three options every read verb carries, spelt and bound
/// identically on each (SPEC u270 Contract Surface, `ReadOptions`).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ReadOptions {
    pub repo: Option<String>,
    pub version: Option<String>,
    pub if_repo: bool,
}

/// One repository at one instant: `commit_sha` is a full 40-character
/// lowercase-hex hash and `version` its 1-based ordinal (`INV-33`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedRef {
    pub version: u32,
    pub commit_sha: String,
}

/// The run's one read target. Every later request addresses `repo_id` at
/// `reference` and sends `reference.version` in decimal as its `ref`,
/// the tree read included.
#[derive(Debug, Clone)]
pub struct ReadTarget {
    pub repo_id: String,
    pub token: Option<String>,
    pub reference: ResolvedRef,
}

impl ReadTarget {
    /// The pinned ordinal, in decimal, as every later request sends it.
    pub fn version_ref(&self) -> String {
        self.reference.version.to_string()
    }
}

/// The version-not-found refusal (SPEC u270 Contract Surface): it names
/// the `--version` value the caller typed and never a path, so the
/// out-of-range reference and the missing file no longer print one line
/// between them.
pub fn version_not_found_refusal(version: &str) -> String {
    format!("version not found: {version}")
}

/// The path-not-found refusal (SPEC u270 Contract Surface): the pinned
/// version beside the path, in place of the registered not-found line.
pub fn path_not_found_refusal(version: u32, path: &str) -> String {
    format!("path not found at version {version}: {path}")
}

/// Chooses between the two not-found lines a read verb can print: the
/// path-not-found refusal wherever the run resolved its reference from a
/// `--version` the caller gave, and the registered line wherever none
/// was given.
pub fn read_not_found(
    err: CliError,
    opts: &ReadOptions,
    reference: &ResolvedRef,
    path: &str,
) -> CliError {
    match opts.version {
        Some(_) => err.with_versioned_read_context(path_not_found_refusal(reference.version, path)),
        None => err,
    }
}

/// The `--repo OWNER/NAME` value parser. A value lacking exactly one
/// `/`, or carrying a side outside the registered repository spelling
/// `D-025` fixes, ends the run through the argument parser at exit `2`
/// (SPEC u270 Behaviour, `resolve_read_target` 1). The registered
/// spelling is what keeps a value such as `alice/..` from normalising
/// the address out of the repository namespace (u270 CR1-4).
pub fn parse_repo_id(value: &str) -> Result<String, String> {
    if is_repository_shape(value) {
        Ok(value.to_string())
    } else {
        Err("expected OWNER/NAME".to_string())
    }
}

/// `configuration error: version must be ≥ 1` for an all-digit value
/// below `1`, raised before any request leaves. Every other spelling
/// reaches the server unchecked.
pub(crate) fn refuse_version_below_one(value: &str) -> Result<(), CliError> {
    let all_digits = !value.is_empty() && value.bytes().all(|b| b.is_ascii_digit());
    if all_digits && value.parse::<u64>().unwrap_or(u64::MAX) < 1 {
        return Err(CliError::Config {
            message: "version must be \u{2265} 1".to_string(),
        });
    }
    Ok(())
}

/// Binds the repository, resolves the reference, and pins the pair as
/// the run's one reference (SPEC u270 Behaviour, `resolve_read_target`).
///
/// Answers `None` only where the skip envelope was written. A `--repo`
/// value standing reaches the identity ladder as no name flag — it does
/// not reach it at all — so `--if-repo` beside it never skips.
pub async fn resolve_read_target(
    config: &Config,
    output: &Output,
    opts: &ReadOptions,
) -> Result<Option<ReadTarget>, CliError> {
    // 1 — bind the repository.
    let repo_id = match opts.repo.as_deref() {
        Some(named) => named.to_ascii_lowercase(),
        None => {
            let current_dir = std::env::current_dir().map_err(|e| CliError::Io {
                message: format!("could not determine current directory: {e}"),
            })?;
            match resolve_full_or_skip(None, &current_dir, opts.if_repo, output)? {
                Some((owner, name)) => format!("{owner}/{name}"),
                None => return Ok(None),
            }
        }
    };

    let token = TokenStore::new(config.credentials_path())
        .read()
        .ok()
        .flatten();
    let client = SynsClient::new(config.server_url())?;

    // 2 — the reference to resolve.
    let reference = match opts.version.as_deref() {
        Some(value) => {
            refuse_version_below_one(value)?;
            value.to_string()
        }
        None => {
            let repo = client.get_repo(&repo_id, token.as_deref()).await?;
            match repo.commit_sha {
                Some(sha) => sha,
                None => {
                    return Err(CliError::Api {
                        status: Some(422),
                        error: "validation_error".to_string(),
                        context: None,
                    });
                }
            }
        }
    };

    // 3 — resolve it into a version and a full hash. A branch name, and
    // every other spelling the server's reference form refuses, comes
    // back as its own `validation_error` (`issues/006`).
    let (entry, _raw) = client
        .get_version(&repo_id, token.as_deref(), &reference)
        .await
        .map_err(|e| match opts.version.as_deref() {
            Some(typed) => e.with_versioned_read_context(version_not_found_refusal(typed)),
            None => e,
        })?;

    // 4 — pin the pair.
    Ok(Some(ReadTarget {
        repo_id,
        token,
        reference: ResolvedRef {
            version: entry.version,
            commit_sha: entry.sha,
        },
    }))
}

/// The two options the repository-scoped verbs this unit adds carry,
/// spelt and bound as the read verbs spell and bind them (SPEC u272
/// Contract Surface, `RepoScopeArgs`). There is no `--version` beside
/// them: neither verb reads at a pinned reference.
#[derive(clap::Args, Clone, Debug, Default, PartialEq, Eq)]
pub struct RepoScopeArgs {
    /// Address another repository, as OWNER/NAME
    #[arg(long, value_name = "OWNER/NAME", value_parser = parse_repo_id)]
    pub repo: Option<String>,
    /// Silently skip (exit 0) when no Syns repo identity resolves
    #[arg(long)]
    pub if_repo: bool,
}

/// The run's one repository scope: the bound identity and the stored
/// credential, or none where the machine holds none the reader can use.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RepoScope {
    pub repo_id: String,
    pub token: Option<String>,
}

/// Binds the repository and reads the stored credential, pinning the
/// pair as the run's one scope (SPEC u272 Behaviour, `resolve_repo_scope`).
///
/// Unlike `resolve_read_target` it reads no head and resolves no
/// reference, so a verb built on it pays for no request it never needed.
/// Answers `None` only where the skip envelope was written.
pub async fn resolve_repo_scope(
    config: &Config,
    output: &Output,
    args: &RepoScopeArgs,
) -> Result<Option<RepoScope>, CliError> {
    // 1 — bind the repository.
    let repo_id = match args.repo.as_deref() {
        Some(named) => named.to_ascii_lowercase(),
        None => {
            let current_dir = std::env::current_dir().map_err(|e| CliError::Io {
                message: format!("could not determine current directory: {e}"),
            })?;
            match resolve_full_or_skip(None, &current_dir, args.if_repo, output)? {
                Some((owner, name)) => format!("{owner}/{name}"),
                None => return Ok(None),
            }
        }
    };

    // 2 — the stored credential, carried as none where none stands or
    // where the stored form does not parse. The entries these verbs
    // reach admit an unidentified caller.
    let token = TokenStore::new(config.credentials_path())
        .read()
        .ok()
        .flatten();

    // 3 — pin the pair.
    Ok(Some(RepoScope { repo_id, token }))
}

/// Writes the reference on the diagnostic stream outside
/// machine-readable mode and nothing at all in it, the hash standing in
/// full for a write to take as its parent.
pub fn report_reference(output: &Output, reference: &ResolvedRef) {
    if output.is_json() {
        return;
    }
    eprintln!(
        "read at version {}, commit {}",
        reference.version, reference.commit_sha
    );
}

/// The served body with `version` added and `commitSha` added where the
/// body carries none, every other key and value as served.
pub fn with_reference(body: serde_json::Value, reference: &ResolvedRef) -> serde_json::Value {
    let mut body = body;
    if let Some(map) = body.as_object_mut() {
        map.insert(
            "version".to_string(),
            serde_json::Value::from(reference.version),
        );
        map.entry("commitSha")
            .or_insert_with(|| serde_json::Value::from(reference.commit_sha.clone()));
    }
    body
}

/// The document with `error` added carrying the refusal's one string,
/// every other key as written, so one document stands on the primary
/// stream whether the answer was whole or partial.
pub fn mark_partial(document: serde_json::Value, refusal: &str) -> serde_json::Value {
    let mut document = document;
    if let Some(map) = document.as_object_mut() {
        map.insert("error".to_string(), serde_json::Value::from(refusal));
    }
    document
}

#[cfg(test)]
mod tests {
    use super::*;
    use serial_test::serial;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn version_body(version: u32, sha: &str) -> serde_json::Value {
        serde_json::json!({
            "version": version,
            "sha": sha,
            "parentSha": null,
            "message": "m",
            "messageBody": null,
            "author": "alice",
            "createdAt": "2026-01-01T00:00:00Z",
            "filesChanged": ["a.md"],
        })
    }

    // SPEC u270 Behaviour, `resolve_read_target` 2: an all-digit value
    // below `1` is refused before any request.
    #[tokio::test]
    #[serial]
    async fn a_version_below_one_is_refused_before_any_request() {
        let dir = tempfile::tempdir().unwrap();
        unsafe { std::env::set_var("SYNS_CONFIG_DIR", dir.path()) };
        let server = MockServer::start().await;
        let config = Config::new(Some(&server.uri())).unwrap();
        let output = Output::new(false);

        let err = resolve_read_target(
            &config,
            &output,
            &ReadOptions {
                repo: Some("alice/notes".into()),
                version: Some("0".into()),
                if_repo: false,
            },
        )
        .await
        .unwrap_err();
        unsafe { std::env::remove_var("SYNS_CONFIG_DIR") };

        assert_eq!(
            err.to_string(),
            "configuration error: version must be \u{2265} 1"
        );
        assert_eq!(err.exit_code(), 1);
        assert!(server.received_requests().await.unwrap().is_empty());
    }

    #[test]
    fn a_reference_spelling_reaches_the_server_unchecked() {
        assert!(refuse_version_below_one("1").is_ok());
        assert!(refuse_version_below_one("main").is_ok());
        assert!(refuse_version_below_one(&"b".repeat(40)).is_ok());
        assert!(refuse_version_below_one("00").is_err());
        assert!(refuse_version_below_one("000000").is_err());
    }

    // SPEC u270 Contract Surface, `resolve_read_target`: a `--repo`
    // value standing reaches the identity ladder as no name flag, so
    // `--if-repo` beside it never skips.
    #[tokio::test]
    #[serial]
    async fn a_named_repository_beside_if_repo_reaches_the_read() {
        let dir = tempfile::tempdir().unwrap();
        std::env::set_current_dir(dir.path()).unwrap();
        unsafe { std::env::set_var("SYNS_CONFIG_DIR", dir.path()) };

        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/v1/repos/alice/notes"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "owner": "alice", "name": "notes", "description": null,
                "commitSha": "b".repeat(40), "status": "active", "author": null,
                "tags": [], "visibility": "public", "forkedFrom": null,
                "forkCount": 0, "fileCount": 1, "role": null,
                "createdAt": "2026-01-01T00:00:00Z", "updatedAt": "2026-01-01T00:00:00Z",
            })))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path(format!(
                "/api/v1/repos/alice/notes/versions/{}",
                "b".repeat(40)
            )))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(version_body(2, &"b".repeat(40))),
            )
            .mount(&server)
            .await;

        let config = Config::new(Some(&server.uri())).unwrap();
        let output = Output::new(false);
        let target = resolve_read_target(
            &config,
            &output,
            &ReadOptions {
                repo: Some("Alice/Notes".into()),
                version: None,
                if_repo: true,
            },
        )
        .await
        .unwrap();
        unsafe { std::env::remove_var("SYNS_CONFIG_DIR") };

        let target = target.expect("a named repository never skips");
        assert_eq!(target.repo_id, "alice/notes");
        assert_eq!(target.reference.version, 2);
        assert_eq!(target.version_ref(), "2");
    }

    // SPEC u270 Contract Surface, `with_reference`.
    #[test]
    fn with_reference_leaves_a_served_commit_sha_as_served() {
        let reference = ResolvedRef {
            version: 2,
            commit_sha: "b".repeat(40),
        };
        let served = serde_json::json!({
            "entries": [], "commitSha": "served-sha", "truncated": false,
        });
        let body = with_reference(served, &reference);
        assert_eq!(body["commitSha"], serde_json::json!("served-sha"));
        assert_eq!(body["version"], serde_json::json!(2));
        assert_eq!(body["truncated"], serde_json::json!(false));
    }

    #[test]
    fn with_reference_adds_a_commit_sha_the_body_carries_none_of() {
        let reference = ResolvedRef {
            version: 2,
            commit_sha: "b".repeat(40),
        };
        let served = serde_json::json!({ "path": "a.md", "sha": "abc", "content": "x", "size": 1 });
        let body = with_reference(served, &reference);
        assert_eq!(body["commitSha"], serde_json::json!("b".repeat(40)));
        assert_eq!(body["version"], serde_json::json!(2));
        assert_eq!(body["sha"], serde_json::json!("abc"));
        assert_eq!(body["content"], serde_json::json!("x"));
    }

    #[test]
    fn mark_partial_adds_error_and_leaves_every_other_key() {
        let document = serde_json::json!({ "version": 2, "matches": [{"path": "a.ts"}] });
        let marked = mark_partial(document, "partial answer: x");
        assert_eq!(marked["error"], serde_json::json!("partial answer: x"));
        assert_eq!(marked["version"], serde_json::json!(2));
        assert_eq!(marked["matches"].as_array().unwrap().len(), 1);
    }

    // SPEC u272 Behaviour, `resolve_repo_scope` 1 and 3: a `--repo`
    // value is bound outright, folded to lower case, and no head and no
    // version is read for it.
    #[tokio::test]
    #[serial]
    async fn a_named_repository_scope_is_folded_and_costs_no_request() {
        let dir = tempfile::tempdir().unwrap();
        std::env::set_current_dir(dir.path()).unwrap();
        unsafe { std::env::set_var("SYNS_CONFIG_DIR", dir.path()) };
        let server = MockServer::start().await;
        let config = Config::new(Some(&server.uri())).unwrap();
        let output = Output::new(false);

        let scope = resolve_repo_scope(
            &config,
            &output,
            &RepoScopeArgs {
                repo: Some("Alice/Notes".into()),
                if_repo: true,
            },
        )
        .await
        .unwrap();
        unsafe { std::env::remove_var("SYNS_CONFIG_DIR") };

        let scope = scope.expect("a named repository never skips");
        assert_eq!(scope.repo_id, "alice/notes");
        assert_eq!(scope.token, None);
        assert!(server.received_requests().await.unwrap().is_empty());
    }

    // SPEC u272 Behaviour, `resolve_repo_scope` 1: the skip envelope
    // where the ladder reaches no identity and `--if-repo` stands.
    #[tokio::test]
    #[serial]
    async fn a_scope_under_if_repo_skips_where_no_identity_resolves() {
        let dir = tempfile::tempdir().unwrap();
        std::env::set_current_dir(dir.path()).unwrap();
        unsafe { std::env::set_var("SYNS_CONFIG_DIR", dir.path()) };
        let server = MockServer::start().await;
        let config = Config::new(Some(&server.uri())).unwrap();
        let output = Output::new(true);

        let scope = resolve_repo_scope(
            &config,
            &output,
            &RepoScopeArgs {
                repo: None,
                if_repo: true,
            },
        )
        .await
        .unwrap();
        unsafe { std::env::remove_var("SYNS_CONFIG_DIR") };

        assert!(scope.is_none());
        assert!(server.received_requests().await.unwrap().is_empty());
    }

    // SPEC u272 Behaviour, `resolve_repo_scope` 1: without `--if-repo`
    // the same directory ends the run at exit `2`.
    #[tokio::test]
    #[serial]
    async fn a_scope_without_if_repo_refuses_where_no_identity_resolves() {
        let dir = tempfile::tempdir().unwrap();
        std::env::set_current_dir(dir.path()).unwrap();
        unsafe { std::env::set_var("SYNS_CONFIG_DIR", dir.path()) };
        let server = MockServer::start().await;
        let config = Config::new(Some(&server.uri())).unwrap();
        let output = Output::new(false);

        let err = resolve_repo_scope(&config, &output, &RepoScopeArgs::default())
            .await
            .unwrap_err();
        unsafe { std::env::remove_var("SYNS_CONFIG_DIR") };

        assert!(matches!(err, CliError::RepoIdentityUnknown { .. }));
        assert_eq!(err.exit_code(), 2);
    }

    // SPEC u272 Behaviour, `resolve_repo_scope` 2: a stored credential
    // whose form does not parse is carried as none rather than raised.
    #[tokio::test]
    #[serial]
    async fn a_credential_whose_form_does_not_parse_is_carried_as_none() {
        let dir = tempfile::tempdir().unwrap();
        std::env::set_current_dir(dir.path()).unwrap();
        std::fs::write(dir.path().join("credentials.json"), "not json {").unwrap();
        unsafe { std::env::set_var("SYNS_CONFIG_DIR", dir.path()) };
        let server = MockServer::start().await;
        let config = Config::new(Some(&server.uri())).unwrap();
        let output = Output::new(false);

        let scope = resolve_repo_scope(
            &config,
            &output,
            &RepoScopeArgs {
                repo: Some("alice/notes".into()),
                if_repo: false,
            },
        )
        .await
        .unwrap();
        unsafe { std::env::remove_var("SYNS_CONFIG_DIR") };

        let scope = scope.expect("a named repository never skips");
        assert_eq!(scope.token, None);
    }

    // SPEC u272 Behaviour, `resolve_repo_scope` 2 and 3: the stored
    // credential rides on the scope where one stands.
    #[tokio::test]
    #[serial]
    async fn a_stored_credential_rides_on_the_scope() {
        let dir = tempfile::tempdir().unwrap();
        std::env::set_current_dir(dir.path()).unwrap();
        crate::auth::token::TokenStore::new(dir.path().join("credentials.json"))
            .write("u272-token")
            .unwrap();
        unsafe { std::env::set_var("SYNS_CONFIG_DIR", dir.path()) };
        let server = MockServer::start().await;
        let config = Config::new(Some(&server.uri())).unwrap();
        let output = Output::new(false);

        let scope = resolve_repo_scope(
            &config,
            &output,
            &RepoScopeArgs {
                repo: Some("alice/notes".into()),
                if_repo: false,
            },
        )
        .await
        .unwrap()
        .expect("a named repository never skips");
        unsafe { std::env::remove_var("SYNS_CONFIG_DIR") };

        assert_eq!(scope.token.as_deref(), Some("u272-token"));
    }

    // SPEC u272 Behaviour, `resolve_repo_scope` 1: the identity file in
    // the working directory binds the scope where no `--repo` stands.
    #[tokio::test]
    #[serial]
    async fn the_identity_ladder_binds_the_scope_where_no_repo_stands() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(".syns.yaml"), "owner: Alice\nname: Notes\n").unwrap();
        std::env::set_current_dir(dir.path()).unwrap();
        unsafe { std::env::set_var("SYNS_CONFIG_DIR", dir.path()) };
        let server = MockServer::start().await;
        let config = Config::new(Some(&server.uri())).unwrap();
        let output = Output::new(false);

        let scope = resolve_repo_scope(&config, &output, &RepoScopeArgs::default())
            .await
            .unwrap()
            .expect("an identity file resolves");
        unsafe { std::env::remove_var("SYNS_CONFIG_DIR") };

        // The ladder folds the identity file's own spelling, so the
        // scope reads the same either way in (`src/repo/resolve.rs`).
        assert_eq!(scope.repo_id, "alice/notes");
        assert!(server.received_requests().await.unwrap().is_empty());
    }

    #[test]
    fn repo_values_lacking_exactly_one_separator_are_refused() {
        assert_eq!(parse_repo_id("alice/notes").unwrap(), "alice/notes");
        assert!(parse_repo_id("notes").is_err());
        assert!(parse_repo_id("alice/notes/extra").is_err());
        assert!(parse_repo_id("/notes").is_err());
        assert!(parse_repo_id("alice/").is_err());
    }

    #[test]
    fn the_two_refusal_lines_name_what_they_are_registered_to_name() {
        assert_eq!(version_not_found_refusal("999"), "version not found: 999");
        assert_eq!(
            path_not_found_refusal(58, "a.md"),
            "path not found at version 58: a.md"
        );
    }
}
