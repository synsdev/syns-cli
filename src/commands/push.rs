use std::path::PathBuf;

use clap::Args;

use crate::auth::token::TokenStore;
use crate::client::{PushResponse, SynsClient};
use crate::commands::repo::{CliRepoStatus, CliVisibility};
use crate::commands::sync::{
    ensure_identity_file, provenance_from_env, render_outcome, render_transfer_lines,
};
use crate::config::Config;
use crate::errors::{CliError, IdentityRemedy};
use crate::output::Output;
use crate::push::collector::{SkippedFile, write_skip_summary};
use crate::push::converge::{ConvergeMode, SyncOutcome, converge};
use crate::push::smart::{PushPipelineMeta, SmartPushOptions, smart_push};
use crate::push::working_copy::WorkingCopy;
use crate::repo::if_repo::resolve_or_skip;
use crate::repo::root::{push_scope, resolve_start_path};

/// The message a publication carries where the invocation names none.
pub(crate) const DEFAULT_COMMIT_MESSAGE: &str = "push";
const SHORT_SHA_LENGTH: usize = 8;

#[derive(Args, Debug)]
pub struct PushArgs {
    /// Override repository name
    #[arg(long, short = 'n')]
    pub name: Option<String>,

    /// Commit message
    #[arg(long, short = 'm')]
    pub message: Option<String>,

    /// Send all files, bypassing manifest diffing; also claims no parent, so a publication that landed since this folder last published is overwritten
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
    // The directory the identity walk starts at (SPEC u255 `cmd_push`
    // 1), in ABSOLUTE form — see `resolve_start_path`, which is what
    // makes `syns push ./sub` work from a subdirectory. The SAME path
    // seeds the scope resolution below, so the identity and the
    // content root can never be resolved from two different places.
    let start_path = resolve_start_path(args.path.as_deref())?;

    // `syns push [PATH]` accepts `--name`, so its identity refusal names
    // that option (SPEC u262 `cmd_push` 1).
    let identity = match resolve_or_skip(args.name.as_deref(), &start_path, args.if_repo, output)
        .map_err(|err| err.with_identity_remedy(IdentityRemedy::NameOption))?
    {
        Some(id) => id,
        None => return Ok(()),
    };

    let token_store = TokenStore::new(config.credentials_path());
    let token = token_store.read()?.ok_or(CliError::AuthRequired)?;

    let name = identity.name;
    let (owner, client) = if let Some(owner) = identity.owner {
        (owner, None)
    } else {
        let c = SynsClient::new(config.server_url())?;
        let owner = resolve_owner(&token_store, &c, &token).await?;
        (owner, Some(c))
    };

    let repo_id = format!("{owner}/{name}");

    // The content root, and the scope inside it (SPEC u255 `cmd_push`
    // 3). Before u255 the publication took `start_path` itself as the
    // root, so a run from `repo/sub/` published `sub/` as though it
    // were the whole repository.
    let scope = push_scope(
        args.path.as_ref().map(|_| start_path.as_path()),
        &start_path,
        &owner,
        &name,
    )?;

    let status = args.status.clone().map(Into::into);
    let visibility = args.visibility.clone().map(Into::into);

    let message = args
        .message
        .clone()
        .filter(|message| !message.trim().is_empty())
        .unwrap_or_else(|| DEFAULT_COMMIT_MESSAGE.to_string());

    let opts = SmartPushOptions {
        force: args.force,
        message,
        // SPEC u258 `cmd_push` 2 (D-065): no author on any branch, so the
        // server names the session's user — a non-owner writer publishing
        // into the owner's repository is recorded as themselves.
        author: None,
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
        prefix: scope.prefix.clone(),
        reference: None,
        expected: None,
        provenance: provenance_from_env(),
        collected: None,
        held: None,
        json_output: output.is_json(),
        // SPEC u280 `converge` 3: a landed publication's own summary
        // carries the too-large line.
        renders_publication_summary: true,
    };

    let client = match client {
        Some(c) => c,
        None => SynsClient::new(config.server_url())?,
    };

    let copy = WorkingCopy::open(config.cache_dir(), &owner, &name, &scope.root)?;

    if args.force || args.path.is_some() {
        // SPEC u256 `cmd_push` 2: a forced or path-scoped publication
        // would publish past a resolution nobody reviewed.
        if let Some(resolution) = copy.resolution()? {
            return render_outcome(
                output,
                Some(&repo_id),
                SyncOutcome::ResolutionRequired(resolution),
                None,
            );
        }

        // `cmd_push` 3 — the registered publication, unchanged.
        //
        // CODE_REVIEW M3 / H2: the per-category breakdown for
        // `PushPartial` is now rendered inside `Display for
        // CliError::PushPartial` (SPEC § 7 — headline-then-detail order
        // structurally enforced). In `--json` mode,
        // `Output::format_error` short-circuits through
        // `CliError::json_value` to the structured wire form before any
        // prose reaches the output stream — no separate intercept site
        // needed.
        let (response, raw, meta) =
            smart_push(&client, &token, &repo_id, &scope.root, opts).await?;
        lay_publication_over_base(&copy, &response, &meta);
        format_response(output, &response, &raw, &repo_id, &meta);
        return Ok(());
    }

    // `cmd_push` 4 — a bare publication converges.
    ensure_identity_file(&copy.root, &owner, &name)?;
    let outcome = match converge(&client, Some(&token), &copy, ConvergeMode::Publish, opts).await {
        Ok(outcome) => outcome,
        // SPEC u280: the left-out refusal is carried as `error` under
        // attention required, as `syns sync` carries it.
        Err(err @ CliError::LeftOut { .. }) => {
            let cause = err.to_string();
            return render_outcome(
                output,
                Some(&repo_id),
                SyncOutcome::AttentionRequired(None),
                Some(cause),
            );
        }
        Err(err) => return Err(err),
    };
    match outcome {
        SyncOutcome::Synced {
            written,
            removed,
            published: Some((response, raw, meta)),
        } => {
            if !output.is_json() {
                render_transfer_lines(&written, &removed);
            }
            format_response(output, &response, &raw, &repo_id, &meta);
            Ok(())
        }
        outcome @ (SyncOutcome::Synced { .. } | SyncOutcome::NoChanges) if output.is_json() => {
            render_outcome(output, Some(&repo_id), outcome, None)
        }
        SyncOutcome::Synced {
            written, removed, ..
        } => {
            render_transfer_lines(&written, &removed);
            output.success(&format!("No changes — {repo_id} is up to date"));
            Ok(())
        }
        SyncOutcome::NoChanges => {
            output.success(&format!("No changes — {repo_id} is up to date"));
            Ok(())
        }
        other => render_outcome(output, Some(&repo_id), other, None),
    }
}

/// SPEC u256 `cmd_push` 3: where the working copy's base names the
/// parent a forced or scoped publication sent, record the acknowledged
/// commit as the base with the collected set laid over it and the named
/// deletions taken out, so the next bare publication does not read the
/// copy's own commit as a moved head. A `--force` run sends no parent and
/// leaves the base alone; a failed write leaves it as it stood.
fn lay_publication_over_base(copy: &WorkingCopy, response: &PushResponse, meta: &PushPipelineMeta) {
    if response.commit_sha.is_empty() {
        return;
    }
    let Ok(_lock) = copy.lock() else {
        return;
    };
    let Some(base) = copy.base() else {
        return;
    };
    if base.commit_sha().is_none() || base.commit_sha() != meta.sent_parent.as_deref() {
        return;
    }
    let mut files: std::collections::HashMap<String, String> = base
        .file_paths()
        .filter_map(|p| base.file_sha(p).map(|s| (p.to_string(), s.to_string())))
        .collect();
    for path in &meta.deleted {
        files.remove(path);
    }
    for (path, sha) in &meta.collected {
        files.insert(path.clone(), sha.clone());
    }
    if let Err(err) = copy.record_base(&response.commit_sha, files) {
        eprintln!("warning: could not record the working copy base: {err}");
    }
}

/// The parent a publication held and did not claim (SPEC u271, the
/// force warning): `Some` only where the run held such a parent and the
/// body it sent claimed none. A publication into an identity holding no
/// commit held none, so it writes neither the line nor the key.
fn unclaimed_parent(meta: &PushPipelineMeta) -> Option<&str> {
    match meta.sent_parent {
        Some(_) => None,
        None => meta.unclaimed_parent.as_deref(),
    }
}

/// The diagnostic line a forced publication that claimed no parent
/// writes, outside machine-readable mode (SPEC u271, the force warning;
/// `issues/118-push-force-silently-disables-overwrite-protection`,
/// `D-007`).
pub(crate) fn force_warning_line(repo_id: &str, parent: &str) -> String {
    format!(
        "warning: --force claimed no parent, so the head check did not run; whatever {repo_id} gained since {parent} is overwritten at every path this publication carried"
    )
}

/// The served body with `unclaimedParent` added where the publication
/// held a parent and claimed none.
///
/// The key is placed OUTSIDE `build_json_envelope`: that function's
/// early return hands back the served body unchanged wherever no file
/// was dropped, so a key placed inside would never reach the document a
/// publication dropping nothing writes (`SPEC_REVIEW_R2.md` CF-01).
pub(crate) fn with_unclaimed_parent(
    body: serde_json::Value,
    meta: &PushPipelineMeta,
) -> serde_json::Value {
    let mut body = body;
    if let (Some(parent), Some(map)) = (unclaimed_parent(meta), body.as_object_mut()) {
        map.insert(
            "unclaimedParent".to_string(),
            serde_json::Value::from(parent),
        );
    }
    body
}

/// Build the JSON envelope for `--json` mode: the verbatim server
/// response, augmented with `skipped: [...]` iff non-empty.
fn build_json_envelope(raw: &serde_json::Value, skipped: &[SkippedFile]) -> serde_json::Value {
    if skipped.is_empty() {
        return raw.clone();
    }
    let mut envelope = raw.as_object().cloned().unwrap_or_default();
    // CODE_REVIEW L6: SkippedFile is `String + unit-enum`, so
    // `to_value` is infallible. Use `.expect` with an explanatory
    // message instead of silently coercing a failure to `null` —
    // that fallback hid bugs if the type ever gained a non-trivial
    // field.
    envelope.insert(
        "skipped".into(),
        serde_json::to_value(skipped)
            .expect("SkippedFile must serialize (String + unit-enum, infallible)"),
    );
    serde_json::Value::Object(envelope)
}

/// Render the SPEC § 3.4 skip-summary block to stderr.
/// Thin wrapper over [`write_skip_summary`] that targets a `String`
/// buffer and forwards to `eprint!` (a single syscall keeps the
/// block atomic against interleaved subprocess output).
///
/// In `--json` mode this function is NOT called from the success
/// path — `format_response` early-returns through `output.json` —
/// and the error path no longer rounds through `cmd_push` at all
/// (the `PushPartial` Display impl renders the breakdown itself,
/// and `Output::format_error` short-circuits to the structured
/// envelope via `CliError::json_value` before any prose surfaces).
fn render_skip_summary(skipped: &[SkippedFile], strict: bool, no_default_excludes: bool) {
    let mut buf = String::new();
    let _ = write_skip_summary(&mut buf, skipped, strict, no_default_excludes);
    eprint!("{buf}");
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
        // additive `skipped` field when meta.skipped is non-empty, and
        // SPEC u271: the unclaimed parent beside it, whether or not the
        // collector dropped a file.
        let envelope = with_unclaimed_parent(build_json_envelope(raw, &meta.skipped), meta);
        output.json(&envelope);
        return;
    }

    // SPEC u271: outside machine-readable mode the same fact stands on
    // the diagnostic stream, so a caller of either mode learns the guard
    // did not run.
    if let Some(parent) = unclaimed_parent(meta) {
        eprintln!("{}", force_warning_line(repo_id, parent));
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
    use crate::push::collector::SkipReason;
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
        // Under u252, --if-repo opens the gate ONLY when .syns.yaml provided
        // the identity. The pre-u252 form of this test relied on `--name` to
        // resolve identity under --if-repo, which now silent-skips; switch
        // to .syns.yaml so the test continues to assert "runs normally".
        std::fs::write(
            temp_dir.path().join(".syns.yaml"),
            "owner: alice\nname: new-repo\n",
        )
        .unwrap();

        unsafe { std::env::set_var("SYNS_CONFIG_DIR", temp_dir.path()) };
        let config = Config::new(Some(&mock_server.uri())).unwrap();
        let output = Output::new(false);

        let token_store = TokenStore::new(config.credentials_path());
        token_store
            .write_with_username("test-token", Some("alice"))
            .unwrap();

        let args = PushArgs {
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
    #[serial]
    async fn push_with_if_repo_and_only_git_remote_silent_skips_no_http_call() {
        let temp_dir = tempfile::tempdir().unwrap();
        // Only source: a .git/config with origin remote — no .syns.yaml, no --name.
        let git_dir = temp_dir.path().join(".git");
        std::fs::create_dir_all(&git_dir).unwrap();
        std::fs::write(
            git_dir.join("config"),
            "[remote \"origin\"]\n\turl = https://github.com/user/non-syns-project.git\n",
        )
        .unwrap();

        std::env::set_current_dir(temp_dir.path()).unwrap();
        unsafe { std::env::set_var("SYNS_CONFIG_DIR", temp_dir.path()) };

        // No mocks registered — the test verifies absence of any HTTP request.
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

        // Locks the 2026-05-25 bug fix at the command-handler layer: a push from
        // a directory whose only identity source is the git remote silent-skips
        // under --if-repo and makes no HTTP call. Under u208's semantics, this
        // test would have proceeded to a PUT /api/v1/repos/.../push request and
        // failed the is_empty() assertion.
        assert!(result.is_ok(), "cmd_push returned: {result:?}");
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
            provenance: None,
        };
        let (typed, raw) = client
            .push_body(
                "alice/my-project",
                "test-token",
                serde_json::to_vec(&request).unwrap(),
            )
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
                reason: SkipReason::TooLarge,
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
        assert_eq!(arr[0]["reason"], "too_large");
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

    // u258: a forced or path-scoped publication names no author, so the
    // server records the writer who pushed rather than the owner.

    const NON_OWNER_PUSH_PATH: &str = "/api/v1/repos/alice/test-repo/push";

    /// Isolated config and cache dirs for one test, removed on drop.
    struct NonOwnerEnv {
        config_dir: tempfile::TempDir,
        cache_dir: tempfile::TempDir,
    }

    impl NonOwnerEnv {
        fn new() -> Self {
            let env = NonOwnerEnv {
                config_dir: tempfile::tempdir().unwrap(),
                cache_dir: tempfile::tempdir().unwrap(),
            };
            unsafe { std::env::set_var("SYNS_CONFIG_DIR", env.config_dir.path()) };
            unsafe { std::env::set_var("SYNS_CACHE_DIR", env.cache_dir.path()) };
            env
        }
    }

    impl Drop for NonOwnerEnv {
        fn drop(&mut self) {
            unsafe { std::env::remove_var("SYNS_CONFIG_DIR") };
            unsafe { std::env::remove_var("SYNS_CACHE_DIR") };
            let _ = std::env::set_current_dir(env!("CARGO_MANIFEST_DIR"));
        }
    }

    fn store_bob_credential(config: &Config) {
        TokenStore::new(config.credentials_path())
            .write_with_username("test-token", Some("bob"))
            .unwrap();
    }

    fn alice_identity_folder() -> tempfile::TempDir {
        let folder = tempfile::tempdir().unwrap();
        std::fs::write(
            folder.path().join(".syns.yaml"),
            "owner: alice\nname: test-repo\n",
        )
        .unwrap();
        std::fs::write(folder.path().join("README.md"), "edited by bob\n").unwrap();
        folder
    }

    async fn mount_tree_not_found(server: &MockServer) {
        Mock::given(method("GET"))
            .and(path("/api/v1/repos/alice/test-repo/tree"))
            .respond_with(
                ResponseTemplate::new(404).set_body_json(serde_json::json!({"error": "not_found"})),
            )
            .mount(server)
            .await;
    }

    async fn mount_push_accepted(server: &MockServer, priority: u8) {
        Mock::given(method("PUT"))
            .and(path(NON_OWNER_PUSH_PATH))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "commitSha": "b0b12345b0b12345",
                "version": 2,
                "filesChanged": 1,
                "created": false
            })))
            .with_priority(priority)
            .mount(server)
            .await;
    }

    /// Every `EP-push` body the mock recorded at `alice/test-repo`, in order.
    async fn put_bodies(server: &MockServer) -> Vec<serde_json::Value> {
        server
            .received_requests()
            .await
            .unwrap()
            .iter()
            .filter(|r| r.method == reqwest::Method::PUT && r.url.path() == NON_OWNER_PUSH_PATH)
            .map(|r| serde_json::from_slice(&r.body).unwrap())
            .collect()
    }

    fn file_entry<'a>(body: &'a serde_json::Value, file: &str) -> &'a serde_json::Value {
        body["files"]
            .as_array()
            .unwrap()
            .iter()
            .find(|f| f["path"] == file)
            .unwrap_or_else(|| panic!("{file} missing from push body {body}"))
    }

    #[tokio::test]
    #[serial]
    async fn path_scoped_push_by_a_non_owner_sends_no_author() {
        let server = MockServer::start().await;
        mount_tree_not_found(&server).await;
        mount_push_accepted(&server, 1).await;

        let _env = NonOwnerEnv::new();
        let folder = alice_identity_folder();
        std::env::set_current_dir(folder.path()).unwrap();
        let config = Config::new(Some(&server.uri())).unwrap();
        store_bob_credential(&config);

        let args = PushArgs {
            path: Some(".".into()),
            message: Some("bob edits via team write".into()),
            ..default_push_args()
        };
        let result = cmd_push(&config, &Output::new(false), &args).await;
        assert!(result.is_ok(), "{result:?}");

        let bodies = put_bodies(&server).await;
        assert_eq!(bodies.len(), 1);
        assert_eq!(bodies[0]["message"], "bob edits via team write");
        assert!(bodies[0].get("author").is_none(), "{}", bodies[0]);
    }

    #[tokio::test]
    #[serial]
    async fn forced_push_by_a_non_owner_sends_no_author() {
        let server = MockServer::start().await;
        mount_tree_not_found(&server).await;
        mount_push_accepted(&server, 1).await;

        let _env = NonOwnerEnv::new();
        let folder = alice_identity_folder();
        std::env::set_current_dir(folder.path()).unwrap();
        let config = Config::new(Some(&server.uri())).unwrap();
        store_bob_credential(&config);

        let args = PushArgs {
            force: true,
            ..default_push_args()
        };
        let result = cmd_push(&config, &Output::new(false), &args).await;
        assert!(result.is_ok(), "{result:?}");

        let bodies = put_bodies(&server).await;
        assert_eq!(bodies.len(), 1);
        assert!(bodies[0].get("author").is_none(), "{}", bodies[0]);
    }

    #[tokio::test]
    #[serial]
    async fn push_naming_another_owner_on_the_command_line_sends_no_author() {
        let server = MockServer::start().await;
        mount_tree_not_found(&server).await;
        mount_push_accepted(&server, 1).await;

        let _env = NonOwnerEnv::new();
        let folder = tempfile::tempdir().unwrap();
        std::fs::write(folder.path().join("notes.md"), "bob's notes\n").unwrap();
        std::env::set_current_dir(folder.path()).unwrap();
        let config = Config::new(Some(&server.uri())).unwrap();
        store_bob_credential(&config);

        let args = PushArgs {
            name: Some("alice/test-repo".into()),
            path: Some(".".into()),
            ..default_push_args()
        };
        let result = cmd_push(&config, &Output::new(false), &args).await;
        assert!(result.is_ok(), "{result:?}");

        let bodies = put_bodies(&server).await;
        assert_eq!(bodies.len(), 1);
        file_entry(&bodies[0], "notes.md");
        assert!(bodies[0].get("author").is_none(), "{}", bodies[0]);
    }

    #[tokio::test]
    #[serial]
    async fn missing_blobs_resend_by_a_non_owner_sends_no_author() {
        let server = MockServer::start().await;
        mount_tree_not_found(&server).await;
        mount_push_accepted(&server, 2).await;

        let _env = NonOwnerEnv::new();
        let folder = alice_identity_folder();
        std::env::set_current_dir(folder.path()).unwrap();
        let config = Config::new(Some(&server.uri())).unwrap();
        store_bob_credential(&config);

        let readme = std::fs::read(folder.path().join("README.md")).unwrap();
        Mock::given(method("PUT"))
            .and(path(NON_OWNER_PUSH_PATH))
            .respond_with(ResponseTemplate::new(409).set_body_json(serde_json::json!({
                "error": "missing_blobs",
                "message": "some referenced blobs are missing on the server",
                "missing": {"README.md": crate::push::hash::blob_sha1(&readme)}
            })))
            .with_priority(1)
            .up_to_n_times(1)
            .mount(&server)
            .await;
        let mut record = crate::push::manifest::Manifest::default();
        record.update(
            "a11ce000a11ce000".into(),
            std::collections::HashMap::from([(
                "README.md".to_string(),
                crate::push::hash::blob_sha1(&readme),
            )]),
        );
        record
            .save(config.cache_dir(), "alice", "test-repo")
            .unwrap();

        let args = PushArgs {
            path: Some(".".into()),
            ..default_push_args()
        };
        let result = cmd_push(&config, &Output::new(false), &args).await;
        assert!(result.is_ok(), "{result:?}");

        let bodies = put_bodies(&server).await;
        assert_eq!(bodies.len(), 2);
        assert!(file_entry(&bodies[0], "README.md")["content"].is_null());
        assert_eq!(
            file_entry(&bodies[1], "README.md")["content"].as_str(),
            Some("edited by bob\n")
        );
        for body in &bodies {
            assert!(body.get("author").is_none(), "{body}");
        }
    }
}

#[cfg(test)]
mod force_warning_tests {
    use super::*;
    use crate::push::collector::SkipReason;

    const RECORDED: &str = "aa11bb22cc33dd44ee55ff6600778899001122bb";

    fn meta(
        sent: Option<&str>,
        unclaimed: Option<&str>,
        skipped: Vec<SkippedFile>,
    ) -> PushPipelineMeta {
        PushPipelineMeta {
            skipped,
            manifest_existed: true,
            strict: false,
            no_default_excludes: false,
            sent_parent: sent.map(str::to_string),
            unclaimed_parent: unclaimed.map(str::to_string),
            collected: Default::default(),
            deleted: vec![],
        }
    }

    fn served() -> serde_json::Value {
        serde_json::json!({
            "commitSha": "b".repeat(40), "version": 2,
            "filesChanged": 1, "created": false,
        })
    }

    // SPEC u271, the force warning: the key stands where the collector
    // dropped no file — `build_json_envelope`'s early return hands back
    // the served body unchanged there, so a key placed inside it would
    // never reach this document (`SPEC_REVIEW_R2.md` CF-01).
    #[test]
    fn the_key_stands_where_no_file_was_dropped() {
        let meta = meta(None, Some(RECORDED), vec![]);
        let body = with_unclaimed_parent(build_json_envelope(&served(), &meta.skipped), &meta);
        assert_eq!(body["unclaimedParent"], serde_json::json!(RECORDED));
        assert_eq!(body["version"], serde_json::json!(2));
        assert!(body.get("skipped").is_none());
    }

    // And beside the collector's dropped files where it dropped some.
    #[test]
    fn the_key_stands_beside_the_dropped_files_too() {
        let meta = meta(
            None,
            Some(RECORDED),
            vec![SkippedFile {
                path: "a/b.png".into(),
                reason: SkipReason::TooLarge,
            }],
        );
        let body = with_unclaimed_parent(build_json_envelope(&served(), &meta.skipped), &meta);
        assert_eq!(body["unclaimedParent"], serde_json::json!(RECORDED));
        assert_eq!(body["skipped"].as_array().unwrap().len(), 1);
    }

    // Neither line nor key where the publication held no parent, or
    // where the body it sent claimed one.
    #[test]
    fn neither_line_nor_key_where_the_publication_held_no_parent() {
        let held_none = meta(None, None, vec![]);
        assert_eq!(unclaimed_parent(&held_none), None);
        assert!(
            with_unclaimed_parent(served(), &held_none)
                .get("unclaimedParent")
                .is_none()
        );

        let claimed = meta(Some(RECORDED), Some(RECORDED), vec![]);
        assert_eq!(unclaimed_parent(&claimed), None);
        assert!(
            with_unclaimed_parent(served(), &claimed)
                .get("unclaimedParent")
                .is_none()
        );
    }

    // The line names the repository and the parent, and says what the
    // dropped parent does to a concurrent publication.
    #[test]
    fn the_line_names_the_repository_and_the_parent_it_did_not_claim() {
        assert_eq!(
            force_warning_line("alice/notes", RECORDED),
            format!(
                "warning: --force claimed no parent, so the head check did not run; whatever alice/notes gained since {RECORDED} is overwritten at every path this publication carried"
            )
        );
    }

    // The help text a caller reads names what the flag does to a
    // concurrent publication (`issues/118`, `D-007`).
    #[test]
    fn the_force_help_text_names_the_concurrency_effect() {
        let command = <PushArgs as clap::Args>::augment_args(clap::Command::new("push"));
        let force = command
            .get_arguments()
            .find(|a| a.get_id() == "force")
            .expect("--force is registered");
        assert_eq!(
            force.get_help().map(|h| h.to_string()).unwrap_or_default(),
            "Send all files, bypassing manifest diffing; also claims no parent, so a publication that landed since this folder last published is overwritten"
        );
    }
}
