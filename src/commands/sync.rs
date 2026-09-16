//! `syns sync` and `syns resolution` — the one idempotent hook entry, the
//! resolution operation's show, continue and discard, and how every
//! convergence outcome renders (SPEC u256 § Contract Surface).
//!
//! Every outcome renders as one document carrying the `outcome` key
//! (Q-02). Exit `0` stands for synced, no changes and the skip; a
//! resolution required exits `4`, an attention required `5`, and every
//! failure keeps its wrapped error's exit. A non-zero outcome is returned
//! as `CliError::SyncRefusal`, which `main` renders and exits with.

use std::fmt::Write as _;
use std::path::{Path, PathBuf};

use serde_json::{Value, json};

use crate::auth::token::TokenStore;
use crate::client::{PushProvenance, SynsClient};
use crate::commands::push::DEFAULT_COMMIT_MESSAGE;
use crate::config::Config;
use crate::errors::CliError;
use crate::output::Output;
use crate::push::converge::{
    ConvergeMode, SyncOutcome, continue_resolution, converge, discard_resolution,
};
use crate::push::reconcile::{CONFLICT_MARKERS, CollisionKind};
use crate::push::smart::SmartPushOptions;
use crate::push::working_copy::{Resolution, WorkingCopy};
use crate::repo::if_repo::resolve_full_or_skip;
use crate::repo::root::push_scope;
use crate::repo::syns_yaml::write_syns_yaml_where_none_stands;

/// The exit a resolution required renders with (Q-02).
pub const EXIT_RESOLUTION_REQUIRED: i32 = 4;
/// The exit an attention required renders with (Q-02).
pub const EXIT_ATTENTION_REQUIRED: i32 = 5;

/// The actions the resolution operation carries (Q-01).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResolutionAction {
    Show,
    Continue,
    Discard,
}

/// The provenance block a publication sends, read from `SYNS_INTEGRATION`,
/// `SYNS_RUN`, `SYNS_TRIGGER` and the optional `SYNS_TASK`.
pub fn provenance_from_env() -> Option<PushProvenance> {
    provenance_from(|name| std::env::var(name).ok())
}

/// A block only where integration, run and trigger are all set, a value
/// empty once trimmed reading as unset; a value the server's form refuses
/// is left to its `VALIDATION_ERROR`.
pub fn provenance_from(lookup: impl Fn(&str) -> Option<String>) -> Option<PushProvenance> {
    let read = |name: &str| -> Option<String> {
        lookup(name)
            .map(|value| value.trim().to_string())
            .filter(|value| !value.is_empty())
    };
    Some(PushProvenance {
        integration: read("SYNS_INTEGRATION")?,
        run: read("SYNS_RUN")?,
        trigger: read("SYNS_TRIGGER")?,
        task_ref: read("SYNS_TASK"),
    })
}

/// The publication knobs every convergence a command starts runs with.
pub fn convergence_options(config: &Config) -> SmartPushOptions {
    SmartPushOptions {
        force: false,
        message: DEFAULT_COMMIT_MESSAGE.to_string(),
        author: None,
        parent_sha: None,
        excludes: Vec::new(),
        cache_dir: config.cache_dir().to_path_buf(),
        description: None,
        tags: None,
        status: None,
        visibility: None,
        strict: false,
        allow_empty: false,
        debug: false,
        no_default_excludes: false,
        prefix: None,
        reference: None,
        expected: None,
        provenance: provenance_from_env(),
    }
}

/// Write the identity file at `root` where a publication would, before
/// the folder is collected, so the set a convergence records is the set
/// it sends.
pub fn ensure_identity_file(root: &Path, owner: &str, name: &str) -> Result<(), CliError> {
    write_syns_yaml_where_none_stands(root, owner, name).map(|_| ())
}

// ---- the error classes a failure outcome follows ----------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ErrorClass {
    Transient,
    Authentication,
    Authorization,
    NotFound,
    Validation,
    Local,
    Conflict,
    Internal,
}

/// The class `ERRORS.md` registers for the code a failure carries, none
/// where it carries no registered code.
fn error_class(err: &CliError) -> Option<ErrorClass> {
    match err {
        CliError::ServerUnreachable { .. } => Some(ErrorClass::Transient),
        CliError::AuthRequired => Some(ErrorClass::Authentication),
        CliError::RepoIdentityUnknown { .. }
        | CliError::PushEmpty { .. }
        | CliError::PushPartial { .. } => Some(ErrorClass::Local),
        CliError::PayloadTooLarge { .. } => Some(ErrorClass::Validation),
        CliError::Api {
            status: Some(_),
            error,
            ..
        } => match error.as_str() {
            "rate_limited" | "authorization_pending" | "slow_down" => Some(ErrorClass::Transient),
            "unauthorized" | "access_denied" | "expired_token" | "expired_code"
            | "invalid_code" => Some(ErrorClass::Authentication),
            "forbidden" => Some(ErrorClass::Authorization),
            "not_found" | "repo_not_found" | "ref_not_found" | "user_not_found"
            | "file_not_found" => Some(ErrorClass::NotFound),
            "validation_error" | "payload_too_large" | "empty_repo" | "file_too_large"
            | "invalid_author" | "invalid_message" | "invalid_path" | "invalid_sha"
            | "push_too_large" | "sha_mismatch" | "symlink_rejected" | "too_many_files"
            | "tree_too_deep" => Some(ErrorClass::Validation),
            "conflict" | "missing_blobs" | "already_approved" | "head_mismatch" | "repo_exists"
            | "target_exists" => Some(ErrorClass::Conflict),
            "internal_error" => Some(ErrorClass::Internal),
            _ => None,
        },
        _ => None,
    }
}

/// The outcome a failure raised at any step of a run renders as
/// (`cmd_sync` 4).
pub fn outcome_for_error(err: CliError) -> SyncOutcome {
    match error_class(&err) {
        Some(ErrorClass::Transient) => SyncOutcome::RetryableFailure(err),
        Some(ErrorClass::Authentication | ErrorClass::Authorization | ErrorClass::NotFound) => {
            SyncOutcome::CredentialFailure(err)
        }
        Some(ErrorClass::Validation | ErrorClass::Local) => SyncOutcome::ValidationFailure(err),
        Some(ErrorClass::Conflict | ErrorClass::Internal) | None => {
            SyncOutcome::AttentionRequired(None)
        }
    }
}

// ---- rendering --------------------------------------------------------

fn collision_key(kind: CollisionKind) -> &'static str {
    match kind {
        CollisionKind::ModifyModify => "modify_modify",
        CollisionKind::AddAdd => "add_add",
        CollisionKind::ModifyDelete => "modify_delete",
        CollisionKind::DeleteModify => "delete_modify",
        CollisionKind::FolderFile => "folder_file",
        CollisionKind::FileFolder => "file_folder",
    }
}

fn collision_label(kind: CollisionKind) -> &'static str {
    match kind {
        CollisionKind::ModifyModify => "changed on both sides",
        CollisionKind::AddAdd => "added on both sides",
        CollisionKind::ModifyDelete => "changed locally, deleted in the repository",
        CollisionKind::DeleteModify => "deleted locally, changed in the repository",
        CollisionKind::FolderFile => "a folder holding local work here, a file in the repository",
        CollisionKind::FileFolder => "a file here, a folder in the repository",
    }
}

fn resolution_document(resolution: &Resolution) -> Value {
    json!({
        "recoveryId": resolution.recovery_id,
        "baseCommit": resolution.base_commit,
        "headCommit": resolution.head_commit,
        "round": resolution.round,
        "localPaths": resolution.local_paths,
        "remotePaths": resolution.remote_paths,
        "collisions": resolution
            .collisions
            .iter()
            .map(|(path, kind)| json!({"path": path, "kind": collision_key(*kind)}))
            .collect::<Vec<_>>(),
        "combinedPaths": resolution.combined_paths,
        "reviewed": resolution.reviewed_tree.is_some(),
    })
}

fn path_list(paths: &[String]) -> String {
    if paths.is_empty() {
        "none".to_string()
    } else {
        paths.join(", ")
    }
}

/// The text an agent is handed when the repository advanced past its
/// working copy's base while local work stood.
fn render_resolution_instruction(repo_id: &str, resolution: &Resolution) -> String {
    let mut text = String::new();
    let _ = writeln!(
        text,
        "The repository {repo_id} advanced while this working copy held local work. \
         Your local work is preserved: nothing was published and nothing was discarded."
    );
    let _ = writeln!(text);
    let _ = writeln!(
        text,
        "Base commit: {}",
        resolution
            .base_commit
            .as_deref()
            .unwrap_or("(none recorded)")
    );
    let _ = writeln!(text, "Head commit: {}", resolution.head_commit);
    let _ = writeln!(text, "Recovery id: {}", resolution.recovery_id);
    let _ = writeln!(text, "Round: {}", resolution.round);
    let _ = writeln!(
        text,
        "Changed locally: {}",
        path_list(&resolution.local_paths)
    );
    let _ = writeln!(
        text,
        "Changed in the repository: {}",
        path_list(&resolution.remote_paths)
    );
    if resolution.collisions.is_empty() {
        let _ = writeln!(text, "Collisions: none");
    } else {
        let _ = writeln!(text, "Collisions:");
        for (path, kind) in &resolution.collisions {
            let _ = writeln!(text, "  - {path} ({})", collision_label(*kind));
        }
    }
    let _ = writeln!(
        text,
        "Differs from the head once published: {}",
        path_list(&resolution.combined_paths)
    );
    let _ = writeln!(text);
    let _ = writeln!(text, "To finish it:");
    let _ = writeln!(
        text,
        "1. Read the repository's own instructions and use the workflows they require."
    );
    let _ = writeln!(
        text,
        "2. Inspect both change sets, and the neighbouring material and cross-references they touch."
    );
    let _ = writeln!(
        text,
        "3. In every collision path keep the compatible intent of both sides, remove contradiction \
         and duplication, and delete each marker block's `{}`, `{}`, `{}` and `{}` lines.",
        CONFLICT_MARKERS[0], CONFLICT_MARKERS[1], CONFLICT_MARKERS[2], CONFLICT_MARKERS[3]
    );
    if resolution
        .collisions
        .iter()
        .any(|(_, kind)| matches!(kind, CollisionKind::FolderFile | CollisionKind::FileFolder))
    {
        let _ = writeln!(
            text,
            "   Where a folder and a file of one name collide, the local work stays where it stood \
             and the repository's side is held in the remote snapshot `syns resolution show` \
             names: keep either side, or both under different names, and leave no folder and \
             file of one name."
        );
    }
    let _ = writeln!(
        text,
        "4. Run the repository's required checks. Do not force a publication, and do not discard either side."
    );
    let _ = writeln!(text);
    let _ = writeln!(text, "Show it again with: syns resolution show");
    let _ = write!(
        text,
        "Publish it with: syns resolution continue — continuing is what publishes; nothing is \
         published until it runs."
    );
    text
}

fn failure_document(key: &str, repo_id: Option<&str>, err: &CliError) -> Value {
    let mut document = json!({"outcome": key, "repo": repo_id, "error": err.to_string()});
    if let Some(detail) = err.json_value() {
        document["detail"] = detail;
    }
    document
}

/// Render `outcome` as the run's one document (Q-02), returning every
/// non-zero outcome as `CliError::SyncRefusal`.
pub fn render_outcome(
    output: &Output,
    repo_id: Option<&str>,
    outcome: SyncOutcome,
    cause: Option<String>,
) -> Result<(), CliError> {
    let repo_label = repo_id.unwrap_or("this repository");
    match outcome {
        SyncOutcome::NoRepository => Ok(()),
        SyncOutcome::Synced {
            written,
            removed,
            published,
        } => {
            if output.is_json() {
                let publication = published.as_ref().map(|(_, raw, meta)| {
                    let mut raw = raw.clone();
                    if !meta.skipped.is_empty()
                        && let Some(object) = raw.as_object_mut()
                    {
                        object.insert("skipped".into(), json!(meta.skipped));
                    }
                    raw
                });
                output.json(&json!({
                    "outcome": "synced",
                    "repo": repo_id,
                    "written": written,
                    "removed": removed,
                    "publication": publication,
                }));
            } else {
                render_transfer_lines(&written, &removed);
                match &published {
                    Some((response, _, _)) => output.success(&format!(
                        "Synced {repo_label}: published {} (version {})",
                        &response.commit_sha[..response.commit_sha.len().min(8)],
                        response.version
                    )),
                    None => output.success(&format!("Synced {repo_label}")),
                }
            }
            Ok(())
        }
        SyncOutcome::NoChanges => {
            if output.is_json() {
                output.json(&json!({"outcome": "no_changes", "repo": repo_id}));
            } else {
                output.success(&format!("No changes — {repo_label} is up to date"));
            }
            Ok(())
        }
        SyncOutcome::ResolutionRequired(resolution) => {
            let instruction = render_resolution_instruction(repo_label, &resolution);
            if !output.is_json() {
                println!("{instruction}");
            }
            Err(CliError::SyncRefusal {
                document: json!({
                    "outcome": "resolution_required",
                    "repo": repo_id,
                    "resolution": resolution_document(&resolution),
                    "instruction": instruction,
                }),
                line: format!(
                    "resolution required for {repo_label} (recovery id {})",
                    resolution.recovery_id
                ),
                exit: EXIT_RESOLUTION_REQUIRED,
            })
        }
        SyncOutcome::AttentionRequired(resolution) => {
            let reason = cause.clone().unwrap_or_else(|| match &resolution {
                Some(r) if r.round > crate::push::converge::ROUND_BOUND => {
                    "the repository kept moving past every continuation round".to_string()
                }
                _ => "the run could not settle the working copy on its own".to_string(),
            });
            Err(CliError::SyncRefusal {
                document: json!({
                    "outcome": "attention_required",
                    "repo": repo_id,
                    "resolution": resolution.as_ref().map(resolution_document),
                    "error": reason,
                }),
                line: format!("attention required for {repo_label}: {reason}"),
                exit: EXIT_ATTENTION_REQUIRED,
            })
        }
        SyncOutcome::RetryableFailure(err) => Err(CliError::SyncRefusal {
            document: failure_document("retryable_failure", repo_id, &err),
            line: err.to_string(),
            exit: err.exit_code(),
        }),
        SyncOutcome::CredentialFailure(err) => Err(CliError::SyncRefusal {
            document: failure_document("credential_failure", repo_id, &err),
            line: err.to_string(),
            exit: err.exit_code(),
        }),
        SyncOutcome::ValidationFailure(err) => Err(CliError::SyncRefusal {
            document: failure_document("validation_failure", repo_id, &err),
            line: err.to_string(),
            exit: err.exit_code(),
        }),
    }
}

/// The transfer progress lines a convergence's writes and removals
/// render as.
pub fn render_transfer_lines(written: &[String], removed: &[String]) {
    for path in written {
        eprintln!(
            "  {}",
            console::style(format!("downloaded: {path}")).green()
        );
    }
    for path in removed {
        eprintln!("  {}", console::style(format!("deleted: {path}")).red());
    }
}

fn render_error(output: &Output, repo_id: Option<&str>, err: CliError) -> Result<(), CliError> {
    let cause = err.to_string();
    render_outcome(output, repo_id, outcome_for_error(err), Some(cause))
}

fn resolve_identity(
    output: &Output,
    if_repo: bool,
) -> Result<Option<(PathBuf, String, String)>, CliError> {
    let cwd = std::env::current_dir().map_err(|err| CliError::Io {
        message: format!("could not determine current directory: {err}"),
    })?;
    Ok(resolve_full_or_skip(None, &cwd, if_repo, output)?.map(|(owner, name)| (cwd, owner, name)))
}

fn open_copy(
    config: &Config,
    cwd: &Path,
    owner: &str,
    name: &str,
) -> Result<WorkingCopy, CliError> {
    let scope = push_scope(None, cwd, owner, name)?;
    WorkingCopy::open(config.cache_dir(), owner, name, &scope.root)
}

fn load_token(config: &Config) -> Result<String, CliError> {
    TokenStore::new(config.credentials_path())
        .read()?
        .ok_or(CliError::AuthRequired)
}

// ---- syns sync --------------------------------------------------------

/// Converge the working copy the working directory stands in, publishing
/// past the head, and render the one outcome.
pub async fn cmd_sync(config: &Config, output: &Output, if_repo: bool) -> Result<(), CliError> {
    // 1
    let (cwd, owner, name) = match resolve_identity(output, if_repo) {
        Ok(Some(identity)) => identity,
        Ok(None) => return render_outcome(output, None, SyncOutcome::NoRepository, None),
        Err(err) => return render_error(output, None, err),
    };
    let repo_id = format!("{owner}/{name}");

    let run = async {
        // 2
        let copy = open_copy(config, &cwd, &owner, &name)?;
        // 3
        let token = load_token(config)?;
        // 4
        let client = SynsClient::new(config.server_url())?;
        ensure_identity_file(&copy.root, &owner, &name)?;
        converge(
            &client,
            Some(&token),
            &copy,
            ConvergeMode::Publish,
            convergence_options(config),
        )
        .await
    };

    match run.await {
        Ok(outcome) => render_outcome(output, Some(&repo_id), outcome, None),
        Err(err) => render_error(output, Some(&repo_id), err),
    }
}

// ---- syns resolution --------------------------------------------------

enum Rendered {
    Outcome(SyncOutcome),
    Shown(WorkingCopy, Resolution),
    Discarded(Resolution),
}

/// The marker blocks `text` still holds, each from its opening marker
/// line through its closing one.
fn marker_blocks(text: &str) -> Vec<String> {
    let mut blocks = Vec::new();
    let mut current: Option<String> = None;
    for line in text.lines() {
        if line.starts_with(CONFLICT_MARKERS[0]) {
            current = Some(String::new());
        }
        if let Some(block) = current.as_mut() {
            block.push_str(line);
            block.push('\n');
            if line.starts_with(CONFLICT_MARKERS[3]) {
                blocks.extend(current.take());
            }
        }
    }
    blocks
}

fn render_shown(
    output: &Output,
    repo_id: &str,
    copy: &WorkingCopy,
    resolution: Resolution,
) -> Result<(), CliError> {
    let instruction = render_resolution_instruction(repo_id, &resolution);
    let mut markers = serde_json::Map::new();
    for (path, _kind) in &resolution.collisions {
        let text = std::fs::read_to_string(copy.root.join(path)).unwrap_or_default();
        let blocks = marker_blocks(&text);
        if !blocks.is_empty() {
            markers.insert(path.clone(), json!(blocks));
        }
    }

    if !output.is_json() {
        println!("{instruction}");
        println!();
        println!("Local snapshot: {}", copy.local_snapshot_path().display());
        println!("Remote snapshot: {}", copy.remote_snapshot_path().display());
        for (path, blocks) in &markers {
            println!();
            println!("Markers still in {path}:");
            for block in blocks.as_array().into_iter().flatten() {
                print!("{}", block.as_str().unwrap_or_default());
            }
        }
    }

    Err(CliError::SyncRefusal {
        document: json!({
            "outcome": "resolution_required",
            "repo": repo_id,
            "resolution": resolution_document(&resolution),
            "markers": markers,
            "snapshots": {
                "local": copy.local_snapshot_path(),
                "remote": copy.remote_snapshot_path(),
            },
            "instruction": instruction,
        }),
        line: format!(
            "resolution required for {repo_id} (recovery id {})",
            resolution.recovery_id
        ),
        exit: EXIT_RESOLUTION_REQUIRED,
    })
}

/// Show, continue or discard the resolution standing for the working copy
/// the working directory stands in. `Show` and `Discard` make no request.
pub async fn cmd_resolution(
    config: &Config,
    output: &Output,
    action: ResolutionAction,
    if_repo: bool,
) -> Result<(), CliError> {
    // 1
    let (cwd, owner, name) = match resolve_identity(output, if_repo) {
        Ok(Some(identity)) => identity,
        Ok(None) => return render_outcome(output, None, SyncOutcome::NoRepository, None),
        Err(err) => return render_error(output, None, err),
    };
    let repo_id = format!("{owner}/{name}");

    let run = async {
        let copy = open_copy(config, &cwd, &owner, &name)?;
        match action {
            // 2
            ResolutionAction::Show => Ok(match copy.resolution()? {
                Some(resolution) => Rendered::Shown(copy, resolution),
                None => Rendered::Outcome(SyncOutcome::NoChanges),
            }),
            // 3
            ResolutionAction::Continue => {
                let token = load_token(config)?;
                let client = SynsClient::new(config.server_url())?;
                ensure_identity_file(&copy.root, &owner, &name)?;
                let outcome =
                    continue_resolution(&client, &token, &copy, convergence_options(config))
                        .await?;
                Ok(Rendered::Outcome(outcome))
            }
            // 4
            ResolutionAction::Discard => match copy.resolution()? {
                Some(resolution) => {
                    discard_resolution(&copy)?;
                    Ok(Rendered::Discarded(resolution))
                }
                None => Ok(Rendered::Outcome(SyncOutcome::NoChanges)),
            },
        }
    };

    match run.await {
        Ok(Rendered::Outcome(outcome)) => render_outcome(output, Some(&repo_id), outcome, None),
        Ok(Rendered::Shown(copy, resolution)) => render_shown(output, &repo_id, &copy, resolution),
        Ok(Rendered::Discarded(resolution)) => {
            if output.is_json() {
                output.json(&json!({
                    "outcome": "no_changes",
                    "repo": repo_id,
                    "discarded": resolution.recovery_id,
                }));
            } else {
                output.success(&format!(
                    "Discarded resolution {} for {repo_id}; the folder holds what it held before it",
                    resolution.recovery_id
                ));
            }
            Ok(())
        }
        Err(err) => render_error(output, Some(&repo_id), err),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn api(status: u16, error: &str) -> CliError {
        CliError::Api {
            status: Some(status),
            error: error.into(),
            context: None,
        }
    }

    fn key(outcome: &SyncOutcome) -> &'static str {
        match outcome {
            SyncOutcome::RetryableFailure(_) => "retryable",
            SyncOutcome::CredentialFailure(_) => "credential",
            SyncOutcome::ValidationFailure(_) => "validation",
            SyncOutcome::AttentionRequired(_) => "attention",
            _ => "other",
        }
    }

    #[test]
    fn outcome_for_error_follows_each_code_class() {
        let cases: Vec<(CliError, &str)> = vec![
            (CliError::ServerUnreachable { url: "u".into() }, "retryable"),
            (api(429, "rate_limited"), "retryable"),
            (CliError::AuthRequired, "credential"),
            (api(403, "forbidden"), "credential"),
            (api(404, "repo_not_found"), "credential"),
            (api(422, "validation_error"), "validation"),
            (api(413, "payload_too_large"), "validation"),
            (
                CliError::RepoIdentityUnknown {
                    remedy: crate::errors::IdentityRemedy::IdentityFile,
                },
                "validation",
            ),
            (
                CliError::PushEmpty {
                    path: "p".into(),
                    total_walked: 0,
                    cause: "c".into(),
                },
                "validation",
            ),
            (api(409, "conflict"), "attention"),
            (api(500, "internal_error"), "attention"),
            (api(502, "unknown error"), "attention"),
            (
                CliError::Io {
                    message: "m".into(),
                },
                "attention",
            ),
            (
                CliError::Config {
                    message: "m".into(),
                },
                "attention",
            ),
            (
                CliError::Api {
                    status: None,
                    error: "invalid response body".into(),
                    context: None,
                },
                "attention",
            ),
        ];
        for (err, expected) in cases {
            let label = format!("{err:?}");
            assert_eq!(key(&outcome_for_error(err)), expected, "{label}");
        }
    }

    fn resolution() -> Resolution {
        Resolution {
            recovery_id: "rec-1234".into(),
            base_commit: Some("base-sha".into()),
            head_commit: "head-sha".into(),
            round: 2,
            local_paths: vec!["notes.md".into()],
            remote_paths: vec!["remote.md".into()],
            collisions: vec![("docs/a.md".into(), CollisionKind::ModifyModify)],
            combined_paths: vec!["docs/a.md".into(), "notes.md".into()],
            reviewed_tree: None,
            pending_writes: None,
        }
    }

    #[test]
    fn render_outcome_exits_as_each_outcome_registers() {
        let output = Output::new(true);
        let refusal = |result: Result<(), CliError>| match result {
            Err(CliError::SyncRefusal { document, exit, .. }) => (document, exit),
            other => panic!("expected a refusal, got {other:?}"),
        };

        let (doc, exit) = refusal(render_outcome(
            &output,
            Some("alice/proj"),
            SyncOutcome::ResolutionRequired(resolution()),
            None,
        ));
        assert_eq!(
            (doc["outcome"].as_str(), exit),
            (Some("resolution_required"), 4)
        );
        assert_eq!(doc["resolution"]["recoveryId"], "rec-1234");
        assert_eq!(doc["resolution"]["collisions"][0]["kind"], "modify_modify");

        let (doc, exit) = refusal(render_outcome(
            &output,
            Some("alice/proj"),
            SyncOutcome::AttentionRequired(None),
            Some("server error (409): conflict".into()),
        ));
        assert_eq!(
            (doc["outcome"].as_str(), exit),
            (Some("attention_required"), 5)
        );

        let (doc, exit) = refusal(render_outcome(
            &output,
            Some("alice/proj"),
            SyncOutcome::RetryableFailure(CliError::ServerUnreachable { url: "u".into() }),
            None,
        ));
        assert_eq!(
            (doc["outcome"].as_str(), exit),
            (Some("retryable_failure"), 3)
        );

        let (doc, exit) = refusal(render_outcome(
            &output,
            None,
            SyncOutcome::ValidationFailure(CliError::RepoIdentityUnknown {
                remedy: crate::errors::IdentityRemedy::IdentityFile,
            }),
            None,
        ));
        assert_eq!(
            (doc["outcome"].as_str(), exit),
            (Some("validation_failure"), 2)
        );

        assert!(render_outcome(&output, Some("alice/proj"), SyncOutcome::NoChanges, None).is_ok());
        assert!(render_outcome(&output, None, SyncOutcome::NoRepository, None).is_ok());
        assert!(
            render_outcome(
                &output,
                Some("alice/proj"),
                SyncOutcome::Synced {
                    written: vec![],
                    removed: vec![],
                    published: None
                },
                None
            )
            .is_ok()
        );
    }

    #[test]
    fn instruction_names_everything_a_reviewer_needs() {
        let text = render_resolution_instruction("alice/proj", &resolution());
        for needle in [
            "alice/proj",
            "advanced",
            "preserved",
            "base-sha",
            "head-sha",
            "rec-1234",
            "notes.md",
            "remote.md",
            "docs/a.md",
            "syns resolution show",
            "syns resolution continue",
            "continuing is what publishes",
            "instructions",
            "workflows",
            "both change sets",
            "cross-references",
            "compatible intent",
            "contradiction",
            "duplication",
            "required checks",
            "Do not force",
            "discard either side",
        ] {
            assert!(
                text.contains(needle),
                "the instruction lacks {needle:?}:\n{text}"
            );
        }
    }

    #[test]
    fn provenance_is_sent_only_where_all_three_fields_are_set() {
        let env = |pairs: &[(&str, &str)]| {
            let map: HashMap<String, String> = pairs
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect();
            provenance_from(move |name| map.get(name).cloned())
        };

        assert_eq!(env(&[]), None);
        assert_eq!(
            env(&[("SYNS_INTEGRATION", "codex"), ("SYNS_RUN", "r1")]),
            None
        );
        assert_eq!(
            env(&[
                ("SYNS_INTEGRATION", "codex"),
                ("SYNS_RUN", "   "),
                ("SYNS_TRIGGER", "stop")
            ]),
            None
        );
        assert_eq!(
            env(&[
                ("SYNS_INTEGRATION", " codex "),
                ("SYNS_RUN", "r1"),
                ("SYNS_TRIGGER", "stop"),
                ("SYNS_TASK", "  ")
            ]),
            Some(PushProvenance {
                integration: "codex".into(),
                run: "r1".into(),
                trigger: "stop".into(),
                task_ref: None,
            })
        );
        assert_eq!(
            env(&[
                ("SYNS_INTEGRATION", "codex"),
                ("SYNS_RUN", "r1"),
                ("SYNS_TRIGGER", "stop"),
                ("SYNS_TASK", "T-9")
            ])
            .and_then(|p| p.task_ref),
            Some("T-9".to_string())
        );
    }

    #[test]
    fn marker_blocks_reads_each_block_whole() {
        let text = "intro\n<<<<<<< local\nL\n||||||| base\nB\n=======\nR\n>>>>>>> remote\nmid\n";
        assert_eq!(
            marker_blocks(text),
            vec!["<<<<<<< local\nL\n||||||| base\nB\n=======\nR\n>>>>>>> remote\n".to_string()]
        );
        assert!(marker_blocks("Title\n=======\n").is_empty());
    }
}
