use std::path::PathBuf;

use clap::Args;

use crate::auth::token::TokenStore;
use crate::client::{PushResponse, SynsClient};
use crate::commands::repo::{CliRepoStatus, CliVisibility};
use crate::config::Config;
use crate::errors::CliError;
use crate::output::Output;
use crate::push::collector::{SkipReason, SkippedFile};
use crate::push::smart::{PushPipelineMeta, SmartPushOptions, smart_push};
use crate::repo::if_repo::resolve_or_skip;

const DEFAULT_COMMIT_MESSAGE: &str = "push";
const SHORT_SHA_LENGTH: usize = 8;

#[derive(Args, Debug)]
pub struct PushArgs {
    /// Override repository name
    #[arg(long, short = 'n')]
    pub name: Option<String>,

    /// Commit message
    #[arg(long, short = 'm')]
    pub message: Option<String>,

    /// Send all files, bypassing manifest diffing
    #[arg(long, short = 'f')]
    pub force: bool,

    /// Glob patterns to exclude from push (repeatable)
    #[arg(long, short = 'e')]
    pub exclude: Vec<String>,

    /// Set repository description
    #[arg(long)]
    pub description: Option<String>,

    /// Set repository tags (repeatable)
    #[arg(long, short = 't')]
    pub tag: Vec<String>,

    /// Set repository status
    #[arg(long)]
    pub status: Option<CliRepoStatus>,

    /// Set repository visibility
    #[arg(long)]
    pub visibility: Option<CliVisibility>,

    /// Directory to push (defaults to current directory)
    #[arg(value_name = "PATH")]
    pub path: Option<PathBuf>,

    /// Silently skip (exit 0) when no Syns repo identity resolves
    #[arg(long)]
    pub if_repo: bool,

    /// Fail the push (exit 3, PUSH_PARTIAL) when one or more files were excluded.
    #[arg(long)]
    pub strict: bool,

    /// Allow pushing an empty change set (mirrors `git commit --allow-empty`).
    #[arg(long)]
    pub allow_empty: bool,

    /// Print per-file exclusion decisions to stderr (useful for debugging PUSH_EMPTY).
    #[arg(long)]
    pub debug: bool,

    /// Disable the built-in skip list (node_modules, dist, build, target, .next, .nuxt, __pycache__, .venv, .tox, .cache).
    #[arg(long)]
    pub no_default_excludes: bool,
}

async fn resolve_owner(
    token_store: &TokenStore,
    client: &SynsClient,
    token: &str,
) -> Result<String, CliError> {
    if let Some(username) = token_store.read_username()? {
        return Ok(username);
    }
    let (session, _raw) = client.get_session(token).await?;
    Ok(session.user.username)
}

pub async fn cmd_push(config: &Config, output: &Output, args: &PushArgs) -> Result<(), CliError> {
    let push_path = match &args.path {
        Some(p) => p.clone(),
        None => std::env::current_dir().map_err(|e| CliError::Io {
            message: format!("could not determine current directory: {e}"),
        })?,
    };

    let identity = match resolve_or_skip(args.name.as_deref(), &push_path, args.if_repo, output)? {
        Some(id) => id,
        None => return Ok(()),
    };

    let token_store = TokenStore::new(config.credentials_path());
    let token = token_store.read()?.ok_or(CliError::AuthRequired)?;

    let (owner, client) = if let Some(owner) = identity.owner {
        (owner, None)
    } else {
        let c = SynsClient::new(config.server_url())?;
        let owner = resolve_owner(&token_store, &c, &token).await?;
        (owner, Some(c))
    };

    let repo_id = format!("{owner}/{}", identity.name);

    let status = args.status.clone().map(Into::into);
    let visibility = args.visibility.clone().map(Into::into);

    let message = args
        .message
        .clone()
        .unwrap_or_else(|| DEFAULT_COMMIT_MESSAGE.to_string());

    let opts = SmartPushOptions {
        force: args.force,
        message,
        author: owner.clone(),
        parent_sha: None,
        excludes: args.exclude.clone(),
        cache_dir: config.cache_dir().to_path_buf(),
        description: args.description.clone(),
        tags: if args.tag.is_empty() {
            None
        } else {
            Some(args.tag.clone())
        },
        status,
        visibility,
        strict: args.strict,
        allow_empty: args.allow_empty,
        debug: args.debug,
        no_default_excludes: args.no_default_excludes,
    };

    let client = match client {
        Some(c) => c,
        None => SynsClient::new(config.server_url())?,
    };

    let push_result = smart_push(&client, &token, &repo_id, &push_path, opts).await;
    let (response, raw, meta) = match push_result {
        Ok(triple) => triple,
        Err(CliError::PushPartial { skipped }) => {
            // Render the per-category breakdown to stderr BEFORE
            // bubbling the error so the one-line Display message and
            // the breakdown appear in the right order (SPEC § 7).
            render_skip_summary(&skipped, /* strict = */ true, args.no_default_excludes);
            return Err(CliError::PushPartial { skipped });
        }
        Err(e) => return Err(e),
    };

    format_response(output, &response, &raw, &repo_id, &meta);

    Ok(())
}

/// Build the JSON envelope for `--json` mode: the verbatim server
/// response, augmented with `skipped: [...]` iff non-empty.
fn build_json_envelope(raw: &serde_json::Value, skipped: &[SkippedFile]) -> serde_json::Value {
    if skipped.is_empty() {
        return raw.clone();
    }
    let mut envelope = raw.as_object().cloned().unwrap_or_default();
    envelope.insert(
        "skipped".into(),
        serde_json::to_value(skipped).unwrap_or(serde_json::Value::Null),
    );
    serde_json::Value::Object(envelope)
}

/// Render the SPEC § 3.4 skip-summary block to stderr.
/// `strict` and `no_default_excludes` are the runtime flag values
/// used to gate the hint lines.
fn render_skip_summary(skipped: &[SkippedFile], strict: bool, no_default_excludes: bool) {
    use SkipReason::*;
    const MAX_PER_CATEGORY: usize = 5;

    // Group by reason (fixed declaration-order indexing).
    let mut groups: [(SkipReason, Vec<&str>); 5] = [
        (Binary, Vec::new()),
        (DefaultExcludeDir, Vec::new()),
        (UserExclude, Vec::new()),
        (Gitignore, Vec::new()),
        (Synsignore, Vec::new()),
    ];
    for sf in skipped {
        let idx = sf.reason as usize;
        groups[idx].1.push(sf.path.as_str());
    }
    // Local lex sort per bucket — defensive even though the
    // collector already sorts globally.
    for (_, paths) in groups.iter_mut() {
        paths.sort();
    }

    eprintln!("warning: {} file(s) skipped from push", skipped.len());
    for (reason, paths) in &groups {
        if paths.is_empty() {
            continue;
        }
        let total = paths.len();
        let shown: Vec<&str> = paths.iter().take(MAX_PER_CATEGORY).copied().collect();
        let joined = shown.join(", ");
        if total > MAX_PER_CATEGORY {
            let more = total - MAX_PER_CATEGORY;
            eprintln!("  {reason} ({total}): {joined}, +{more} more");
        } else {
            eprintln!("  {reason} ({total}): {joined}");
        }
    }

    // Conditional hints (SPEC § 3.4 rule 6).
    if !strict {
        eprintln!("  hint: pass --strict to fail the push when any file is skipped");
    }
    if skipped.iter().any(|sf| sf.reason == Binary) {
        eprintln!(
            "  hint: add binary extensions (e.g. *.png, *.pdf) to .synsignore to silence the binary warning"
        );
    }
    if !no_default_excludes && skipped.iter().any(|sf| sf.reason == DefaultExcludeDir) {
        eprintln!(
            "  hint: pass --no-default-excludes to include build / cache directories in the push"
        );
    }
}

fn format_response(
    output: &Output,
    response: &PushResponse,
    raw: &serde_json::Value,
    repo_id: &str,
    meta: &PushPipelineMeta,
) {
    if output.is_json() {
        // SPEC § 3.5: augment the verbatim server response with the
        // additive `skipped` field when meta.skipped is non-empty.
        let envelope = build_json_envelope(raw, &meta.skipped);
        output.json(&envelope);
        return;
    }

    let changed = response.files_changed > 0 || response.created;

    if changed {
        output.success(&format!("Pushed to {repo_id}"));
        output.table(
            &["", ""],
            vec![
                vec![
                    "commit".into(),
                    response.commit_sha[..response.commit_sha.len().min(SHORT_SHA_LENGTH)].into(),
                ],
                vec!["version".into(), response.version.to_string()],
                vec!["files changed".into(), response.files_changed.to_string()],
            ],
        );
    } else if meta.manifest_existed {
        // Subsequent-push-noop — preserved behaviour from u41.
        output.success(&format!("No changes — {repo_id} is up to date"));
    } else {
        // First-push-noop suspicious branch — SPEC § 4 matrix bottom row.
        let short_name = repo_id.split('/').nth(1).unwrap_or(repo_id);
        eprintln!(
            "warning: Push response indicates no changes were committed and no prior \
             manifest existed for {repo_id}. The server may have accepted an empty push — \
             verify with `syns ls --name {short_name}`."
        );
    }

    if !meta.skipped.is_empty() {
        render_skip_summary(&meta.skipped, meta.strict, meta.no_default_excludes);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serial_test::serial;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn default_push_args() -> PushArgs {
        PushArgs {
            name: None,
            message: None,
            force: false,
            exclude: vec![],
            description: None,
            tag: vec![],
            status: None,
            visibility: None,
            path: None,
            if_repo: false,
            strict: false,
            allow_empty: false,
            debug: false,
            no_default_excludes: false,
        }
    }

    #[tokio::test]
    #[serial]
    async fn push_with_name_flag_resolves_owner_from_cached_username() {
        let mock_server = MockServer::start().await;

        Mock::given(method("GET"))
            .and(path("/api/v1/repos/alice/new-repo/tree"))
            .respond_with(
                ResponseTemplate::new(404).set_body_json(serde_json::json!({"error": "not_found"})),
            )
            .mount(&mock_server)
            .await;

        Mock::given(method("PUT"))
            .and(path("/api/v1/repos/alice/new-repo/push"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "commitSha": "abc12345def67890",
                "version": 1,
                "filesChanged": 1,
                "created": true
            })))
            .expect(1)
            .mount(&mock_server)
            .await;

        let temp_dir = tempfile::tempdir().unwrap();
        std::fs::write(temp_dir.path().join("hello.txt"), "hello").unwrap();

        unsafe { std::env::set_var("SYNS_CONFIG_DIR", temp_dir.path()) };
        let config = Config::new(Some(&mock_server.uri())).unwrap();
        let output = Output::new(false);

        let token_store = TokenStore::new(config.credentials_path());
        token_store
            .write_with_username("test-token", Some("alice"))
            .unwrap();

        let args = PushArgs {
            name: Some("new-repo".into()),
            path: Some(temp_dir.path().into()),
            ..default_push_args()
        };

        let result = cmd_push(&config, &output, &args).await;
        unsafe { std::env::remove_var("SYNS_CONFIG_DIR") };

        assert!(result.is_ok());
    }

    #[tokio::test]
    #[serial]
    async fn push_with_force_flag() {
        let mock_server = MockServer::start().await;

        Mock::given(method("PUT"))
            .and(path("/api/v1/repos/carol/repo/push"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "commitSha": "def45678abc12345",
                "version": 2,
                "filesChanged": 0,
                "created": false
            })))
            .expect(1)
            .mount(&mock_server)
            .await;

        let temp_dir = tempfile::tempdir().unwrap();
        std::fs::write(temp_dir.path().join("file.txt"), "content").unwrap();
        std::fs::write(
            temp_dir.path().join(".syns.yaml"),
            "owner: carol\nname: repo\n",
        )
        .unwrap();

        unsafe { std::env::set_var("SYNS_CONFIG_DIR", temp_dir.path()) };
        let config = Config::new(Some(&mock_server.uri())).unwrap();
        let output = Output::new(false);

        let token_store = TokenStore::new(config.credentials_path());
        token_store
            .write_with_username("test-token", Some("carol"))
            .unwrap();

        let args = PushArgs {
            force: true,
            path: Some(temp_dir.path().into()),
            ..default_push_args()
        };

        let result = cmd_push(&config, &output, &args).await;
        unsafe { std::env::remove_var("SYNS_CONFIG_DIR") };

        assert!(result.is_ok());
    }

    #[tokio::test]
    #[serial]
    async fn push_with_metadata_flags() {
        let mock_server = MockServer::start().await;

        Mock::given(method("PUT"))
            .and(path("/api/v1/repos/dave/project/push"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "commitSha": "meta1234abcd5678",
                "version": 1,
                "filesChanged": 1,
                "created": true
            })))
            .expect(1)
            .mount(&mock_server)
            .await;

        let temp_dir = tempfile::tempdir().unwrap();
        std::fs::write(temp_dir.path().join("main.rs"), "fn main() {}").unwrap();
        std::fs::write(
            temp_dir.path().join(".syns.yaml"),
            "owner: dave\nname: project\n",
        )
        .unwrap();

        unsafe { std::env::set_var("SYNS_CONFIG_DIR", temp_dir.path()) };
        let config = Config::new(Some(&mock_server.uri())).unwrap();
        let output = Output::new(false);

        let token_store = TokenStore::new(config.credentials_path());
        token_store
            .write_with_username("test-token", Some("dave"))
            .unwrap();

        let args = PushArgs {
            description: Some("My project description".into()),
            tag: vec!["rust".into(), "cli".into()],
            status: Some(CliRepoStatus::Active),
            visibility: Some(CliVisibility::Public),
            path: Some(temp_dir.path().into()),
            ..default_push_args()
        };

        let result = cmd_push(&config, &output, &args).await;
        unsafe { std::env::remove_var("SYNS_CONFIG_DIR") };

        assert!(result.is_ok());

        let requests = mock_server.received_requests().await.unwrap();
        let put_request = requests
            .iter()
            .find(|r| r.method == reqwest::Method::PUT)
            .unwrap();
        let body: serde_json::Value = serde_json::from_slice(&put_request.body).unwrap();

        assert_eq!(body["description"].as_str(), Some("My project description"));
        assert_eq!(body["tags"], serde_json::json!(["rust", "cli"]));
        assert_eq!(body["status"].as_str(), Some("active"));
        assert_eq!(body["visibility"].as_str(), Some("public"));
    }

    #[tokio::test]
    #[serial]
    async fn push_requires_authentication() {
        let temp_dir = tempfile::tempdir().unwrap();
        std::fs::write(
            temp_dir.path().join(".syns.yaml"),
            "owner: alice\nname: repo\n",
        )
        .unwrap();

        unsafe { std::env::set_var("SYNS_CONFIG_DIR", temp_dir.path()) };
        let config = Config::new(Some("https://syns.dev")).unwrap();
        let output = Output::new(false);

        let args = PushArgs {
            path: Some(temp_dir.path().into()),
            ..default_push_args()
        };

        let result = cmd_push(&config, &output, &args).await;
        unsafe { std::env::remove_var("SYNS_CONFIG_DIR") };

        assert!(matches!(result, Err(CliError::AuthRequired)));
    }

    #[tokio::test]
    #[serial]
    async fn push_no_changes_returns_ok() {
        let mock_server = MockServer::start().await;

        Mock::given(method("PUT"))
            .and(path("/api/v1/repos/eve/repo/push"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "commitSha": "same1234same5678",
                "version": 1,
                "filesChanged": 0,
                "created": false
            })))
            .mount(&mock_server)
            .await;

        let temp_dir = tempfile::tempdir().unwrap();
        std::fs::write(temp_dir.path().join("file.txt"), "unchanged").unwrap();
        std::fs::write(
            temp_dir.path().join(".syns.yaml"),
            "owner: eve\nname: repo\n",
        )
        .unwrap();

        unsafe { std::env::set_var("SYNS_CONFIG_DIR", temp_dir.path()) };
        let config = Config::new(Some(&mock_server.uri())).unwrap();
        let output = Output::new(false);

        let token_store = TokenStore::new(config.credentials_path());
        token_store
            .write_with_username("test-token", Some("eve"))
            .unwrap();

        let args = PushArgs {
            force: true,
            path: Some(temp_dir.path().into()),
            ..default_push_args()
        };

        let result = cmd_push(&config, &output, &args).await;
        unsafe { std::env::remove_var("SYNS_CONFIG_DIR") };

        assert!(result.is_ok());
    }

    #[tokio::test]
    #[serial]
    async fn push_with_if_repo_set_and_identity_resolved_runs_normally() {
        let mock_server = MockServer::start().await;

        Mock::given(method("GET"))
            .and(path("/api/v1/repos/alice/new-repo/tree"))
            .respond_with(
                ResponseTemplate::new(404).set_body_json(serde_json::json!({"error": "not_found"})),
            )
            .mount(&mock_server)
            .await;

        Mock::given(method("PUT"))
            .and(path("/api/v1/repos/alice/new-repo/push"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "commitSha": "abc12345def67890",
                "version": 1,
                "filesChanged": 1,
                "created": true
            })))
            .expect(1)
            .mount(&mock_server)
            .await;

        let temp_dir = tempfile::tempdir().unwrap();
        std::fs::write(temp_dir.path().join("hello.txt"), "hello").unwrap();

        unsafe { std::env::set_var("SYNS_CONFIG_DIR", temp_dir.path()) };
        let config = Config::new(Some(&mock_server.uri())).unwrap();
        let output = Output::new(false);

        let token_store = TokenStore::new(config.credentials_path());
        token_store
            .write_with_username("test-token", Some("alice"))
            .unwrap();

        let args = PushArgs {
            name: Some("new-repo".into()),
            path: Some(temp_dir.path().into()),
            if_repo: true,
            ..default_push_args()
        };

        let result = cmd_push(&config, &output, &args).await;
        unsafe { std::env::remove_var("SYNS_CONFIG_DIR") };

        assert!(result.is_ok());
    }

    #[tokio::test]
    #[serial]
    async fn push_with_if_repo_set_and_no_identity_skips_silently() {
        let temp_dir = tempfile::tempdir().unwrap();
        // No .syns.yaml, no .git, no --name flag → resolver returns RepoIdentityUnknown.
        std::env::set_current_dir(temp_dir.path()).unwrap();
        unsafe { std::env::set_var("SYNS_CONFIG_DIR", temp_dir.path()) };

        let mock_server = MockServer::start().await;
        let config = Config::new(Some(&mock_server.uri())).unwrap();
        let output = Output::new(false);

        let args = PushArgs {
            path: Some(temp_dir.path().into()),
            if_repo: true,
            ..default_push_args()
        };

        let result = cmd_push(&config, &output, &args).await;
        unsafe { std::env::remove_var("SYNS_CONFIG_DIR") };

        assert!(result.is_ok());
        assert!(mock_server.received_requests().await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn push_raw_emits_full_push_response_envelope() {
        use crate::client::PushRequest;

        let mock_server = MockServer::start().await;
        let body = r#"{"commitSha":"abc1234567890abcdef0123456789abcdef012345","version":3,"filesChanged":2,"created":false}"#;
        Mock::given(method("PUT"))
            .and(path("/api/v1/repos/alice/my-project/push"))
            .respond_with(ResponseTemplate::new(200).set_body_string(body))
            .mount(&mock_server)
            .await;

        let client = SynsClient::new(&mock_server.uri()).unwrap();
        let request = PushRequest {
            files: vec![],
            deletions: None,
            message: None,
            author: None,
            parent_sha: None,
            description: None,
            tags: None,
            status: None,
            visibility: None,
        };
        let (typed, raw) = client
            .push("alice/my-project", "test-token", &request)
            .await
            .unwrap();

        assert_eq!(raw.as_object().unwrap().keys().count(), 4);
        assert_eq!(
            raw["commitSha"],
            serde_json::json!("abc1234567890abcdef0123456789abcdef012345")
        );
        assert_eq!(raw["filesChanged"], serde_json::json!(2));
        assert_eq!(raw["created"], serde_json::json!(false));
        let expected: serde_json::Value = serde_json::from_str(body).unwrap();
        assert_eq!(raw, expected);
        assert!(typed.commit_sha.starts_with("abc1234567890"));
    }

    #[test]
    fn build_json_envelope_includes_skipped_field() {
        let raw = serde_json::json!({
            "commitSha": "abc",
            "version": 1,
            "filesChanged": 1,
            "created": false
        });
        let skipped = vec![
            SkippedFile {
                path: "a.png".into(),
                reason: SkipReason::Binary,
            },
            SkippedFile {
                path: "b/c.js".into(),
                reason: SkipReason::DefaultExcludeDir,
            },
        ];
        let envelope = build_json_envelope(&raw, &skipped);
        let arr = envelope
            .as_object()
            .unwrap()
            .get("skipped")
            .unwrap()
            .as_array()
            .unwrap();
        assert_eq!(arr.len(), 2);
        assert_eq!(arr[0]["path"], "a.png");
        assert_eq!(arr[0]["reason"], "binary");
        assert_eq!(arr[1]["path"], "b/c.js");
        assert_eq!(arr[1]["reason"], "default_exclude_dir");
    }

    #[test]
    fn build_json_envelope_omits_skipped_when_empty() {
        let raw = serde_json::json!({
            "commitSha": "abc",
            "version": 1,
            "filesChanged": 1,
            "created": false
        });
        let envelope = build_json_envelope(&raw, &[]);
        assert!(envelope.as_object().unwrap().get("skipped").is_none());
        assert_eq!(envelope, raw);
    }
}
