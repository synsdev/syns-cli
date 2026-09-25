//! The shared write layer of the four writing verbs (SPEC u271).
//!
//! Every write verb takes the same options, resolves one repository at
//! one parent before it composes any body carrying a commit, and
//! publishes exactly one commit through `EP-push`. It writes to no
//! folder on disk at all: the working copy a run may stand in is read to
//! guard the write and never touched.
//!
//! The repository read stands ahead of every push these verbs make
//! (`resolve_write_target` 4), so no folderless write can be the request
//! that creates a repository —
//! `issues/069-failed-push-leaves-stuck-postgres-row-and-bare-git-repo`
//! owns the half-created row that path leaves behind.

use std::collections::BTreeMap;
use std::io::{IsTerminal, Read};
use std::path::{Path, PathBuf};

use crate::auth::token::TokenStore;
use crate::client::{
    PushDeleteEntry, PushFileEntry, PushProvenance, PushRequest, PushResponse, SynsClient,
};
use crate::commands::sync::provenance_from;
use crate::config::Config;
use crate::errors::{ApiErrorContext, CliError, IdentityRemedy, NotTextSurface};
use crate::output::Output;
use crate::push::collector::{CollectOptions, HeldBytes, collect_files};
use crate::push::converge::{excluded_local_files, is_partial_write};
use crate::push::hash::blob_sha1;
use crate::push::working_copy::WorkingCopy;
use crate::repo::if_repo::resolve_full_or_skip;
use crate::repo::syns_yaml::nearest_identity;

/// The options every write verb carries, spelt and bound identically on
/// each (SPEC u271 Contract Surface, `WriteOptions`). `parent` holds a
/// value on every parsed invocation — the argument parser requires it —
/// so no run of these verbs composes a push claiming none.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct WriteOptions {
    pub repo: Option<String>,
    pub parent: String,
    pub message: Option<String>,
    pub provenance: ProvenanceOptions,
}

/// What the publication asserts about where it came from. Each field
/// takes its option's value where one stands and otherwise the
/// environment name beside it — `SYNS_INTEGRATION`, `SYNS_RUN`,
/// `SYNS_TRIGGER`, and `SYNS_TASK` for `--task-ref`, which carries no
/// spelling of its own — a value empty once trimmed reading as unset.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ProvenanceOptions {
    pub integration: Option<String>,
    pub run: Option<String>,
    pub trigger: Option<String>,
    pub task_ref: Option<String>,
}

impl ProvenanceOptions {
    /// The environment name each field falls back to. One binary reads
    /// one name per provenance field: these are the four
    /// `syns push [PATH]` already reads.
    fn option_for(&self, name: &str) -> Option<&str> {
        match name {
            "SYNS_INTEGRATION" => self.integration.as_deref(),
            "SYNS_RUN" => self.run.as_deref(),
            "SYNS_TRIGGER" => self.trigger.as_deref(),
            "SYNS_TASK" => self.task_ref.as_deref(),
            _ => None,
        }
    }

    /// The block this run's publication sends, whole or not at all: the
    /// three required names must all resolve or nothing is asserted.
    pub fn block(&self) -> Option<PushProvenance> {
        self.block_over(|name| std::env::var(name).ok())
    }

    /// `block` over a named environment rather than the process's own,
    /// so a module case can watch an option outrank a name.
    pub fn block_over(&self, env: impl Fn(&str) -> Option<String>) -> Option<PushProvenance> {
        provenance_from(|name| {
            self.option_for(name)
                .map(str::to_string)
                .filter(|value| !value.trim().is_empty())
                .or_else(|| env(name))
        })
    }
}

/// The parent every request of the run claims. `commit_sha` is a full
/// 40-character lowercase-hex hash; `version` stands only where the run
/// resolved a spelling that was not already such a hash, and is the
/// decimal ordinal a later read addresses its reference by.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParentRef {
    pub commit_sha: String,
    pub version: Option<u32>,
}

impl ParentRef {
    /// How `cmd_edit` 2 addresses its read: the resolved ordinal where
    /// one stands, and the full hash otherwise. The hash rides
    /// unresolved because resolving it would pay the same commit walk
    /// twice (`Q-02`,
    /// `issues/148-ref-by-content-hash-resolved-by-scanning-every-commit`).
    pub fn read_ref(&self) -> String {
        match self.version {
            Some(version) => version.to_string(),
            None => self.commit_sha.clone(),
        }
    }
}

/// The run's one write target: every request addresses `repo_id` under
/// `token` at `parent`, and `checkout` is the canonical root of a
/// working copy in the run's directory tracking this repository, `None`
/// where none does.
#[derive(Debug, Clone)]
pub struct WriteTarget {
    pub repo_id: String,
    pub token: String,
    pub parent: ParentRef,
    pub checkout: Option<PathBuf>,
}

/// What one commit carries: every content has already been answered by
/// `text_or_refuse`, and no path stands in both members.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Changeset {
    pub files: Vec<(String, String)>,
    pub deletions: Vec<String>,
}

// ---- the constants these verbs write ---------------------------------

/// The write report's first line.
pub fn wrote_line(version: u32, commit_sha: &str) -> String {
    format!("wrote version {version}, commit {commit_sha}")
}

/// The write report's line for an answer counting no changed file.
pub fn nothing_changed_line(version: u32, commit_sha: &str) -> String {
    format!("nothing changed; the head is still version {version}, commit {commit_sha}")
}

/// The behind-folder notice, which follows the first line alone — an
/// answer counting no changed file leaves a clean folder unnamed,
/// because the head it matches never moved.
pub fn checkout_behind_line(root: &Path) -> String {
    format!(
        "the checkout at {} is now one version behind; syns sync converges it",
        root.display()
    )
}

/// The checkout-guard refusal. It names no option that would lift it,
/// none standing.
pub fn checkout_guard_refusal(root: &Path, repo_id: &str) -> String {
    format!(
        "the checkout at {} holds unpublished local changes for {repo_id}; edit those files instead \u{2014} they publish at the end of the turn \u{2014} or publish them with syns sync, then write again",
        root.display()
    )
}

/// The verb default messages, each standing where `--message` does not,
/// so every commit these verbs make carries a non-empty caption.
pub fn default_message(verb: &str, path: Option<&str>) -> String {
    match path {
        Some(path) => format!("{verb} {path}"),
        None => verb.to_string(),
    }
}

// ---- the guard --------------------------------------------------------

/// Answers the canonical root of a working copy in the run's directory
/// tracking `repo_id`, and only where that copy holds no unpublished
/// local work. It sends no request and writes no state to reach any of
/// its three answers.
pub fn checkout_of(
    config: &Config,
    cwd: &Path,
    repo_id: &str,
) -> Result<Option<PathBuf>, CliError> {
    // 1 — the identity file nearest `cwd`, that directory included.
    let Some(identity) = nearest_identity(cwd)? else {
        return Ok(None);
    };
    let Some((owner, name)) = repo_id.split_once('/') else {
        return Ok(None);
    };
    if !identity.names(owner, name) {
        return Ok(None);
    }
    let root = identity.dir.clone();
    let refuse = || CliError::Io {
        message: checkout_guard_refusal(&root, repo_id),
    };

    // 2 — the working copy at the directory that identity file stands
    // in, where its state already stands. It is opened rather than
    // created: a guard that creates the state directory writes state to
    // answer, which this function does not do (CR1-2). A state
    // directory that does not stand carries no recorded base, which
    // step 3 reads as unpublished local work.
    let base = match WorkingCopy::open_existing(config.cache_dir(), owner, name, &identity.dir) {
        Ok(Some(copy)) => {
            if copy.outbox()?.is_some() || copy.resolution()?.is_some() {
                return Err(refuse());
            }
            copy.base()
        }
        Ok(None) | Err(_) => None,
    };

    // 3 — the folder's file hashes against the recorded base, collected
    // under the built-in skip list and the folder's own ignore files
    // with no `--exclude` pattern. No working copy records the pair its
    // base was written under, so a folder last published under either
    // collection flag reads as holding unpublished work.
    // The guard compares hashes alone, so its collection is handed no
    // record and holds no byte: a budget of zero keeps each kept file's
    // hash and nothing more (SPEC u280, the `src/write/mod.rs` row).
    let collected = collect_files(
        &identity.dir,
        &[],
        CollectOptions::default(),
        None,
        &HeldBytes::new(0),
    )?;
    let folder: BTreeMap<String, String> = collected
        .files
        .iter()
        // A sibling a killed convergence left is no local work: the
        // next collection sweeps it.
        .filter(|(path, _)| !is_partial_write(path))
        .map(|(path, file)| (path.clone(), file.sha.clone()))
        .collect();

    let Some(base) = base else {
        // A copy recording no base has published nothing from here, so
        // every file it holds is unpublished work.
        return if folder.is_empty() {
            Ok(Some(identity.dir))
        } else {
            Err(refuse())
        };
    };
    let recorded: BTreeMap<String, String> = base
        .file_paths()
        .filter_map(|path| {
            base.file_sha(path)
                .map(|sha| (path.to_string(), sha.to_string()))
        })
        .collect();
    // A path the base names that a file stands at and the collection
    // left out is an exclusion, not an edit — a convergence drops it
    // from both sides before comparing, and so does this guard. Without
    // it, a checkout of a repository holding any path the local
    // collector skips is refused every write for good, with no override
    // (CR1-1).
    let excluded = excluded_local_files(
        &identity.dir,
        |path| folder.contains_key(path),
        recorded.keys(),
    );
    let recorded: BTreeMap<String, String> = recorded
        .into_iter()
        .filter(|(path, _)| !excluded.contains(path))
        .collect();
    if folder != recorded {
        return Err(refuse());
    }
    Ok(Some(identity.dir))
}

// ---- the data channel -------------------------------------------------

/// The refusal a standard input that is a terminal takes, naming what
/// was expected there.
///
/// The two verbs that read standard input raise it ahead of the target
/// resolution rather than in their own numbered order, because the
/// failure cell it discharges reads "before any request" — a run whose
/// standard input is a terminal ends rather than waiting, and leaves
/// nothing on the wire behind it.
pub fn refuse_terminal_standard_input(what: &str) -> Result<(), CliError> {
    if std::io::stdin().is_terminal() {
        return Err(CliError::Config {
            message: format!(
                "{what} is read from standard input, and standard input is a terminal \u{2014} pipe it in or redirect a file"
            ),
        });
    }
    Ok(())
}

/// The whole of standard input, up to end-of-input. Where that stream is
/// a terminal the run is refused before any request, naming what was
/// expected there, so no run of these verbs waits on a person who was
/// never going to type.
pub fn read_standard_input(what: &str) -> Result<Vec<u8>, CliError> {
    refuse_terminal_standard_input(what)?;
    let stdin = std::io::stdin();
    let mut bytes = Vec::new();
    stdin
        .lock()
        .read_to_end(&mut bytes)
        .map_err(|e| CliError::Io {
            message: format!("could not read standard input: {e}"),
        })?;
    Ok(bytes)
}

/// The decoded content where the bytes are valid UTF-8 and carry no NUL
/// byte, and otherwise the not-text refusal naming the path — so no run
/// of these verbs hands the wire a content the repository cannot hold.
pub fn text_or_refuse(path: &str, bytes: &[u8]) -> Result<String, CliError> {
    match std::str::from_utf8(bytes) {
        Ok(text) if !text.contains('\0') => Ok(text.to_string()),
        _ => Err(CliError::NotText {
            path: path.to_string(),
            surface: NotTextSurface::Write,
        }),
    }
}

// ---- the target -------------------------------------------------------

/// Whether a `--parent` spelling is already what the push claims: a full
/// 40-character lowercase-hex commit hash.
fn is_full_commit_hash(value: &str) -> bool {
    value.len() == 40
        && value
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

/// The two refusals a `--parent` spelling earns on its own: an empty
/// value, and an all-digit value below `1`.
///
/// They are raised ahead of step 1 rather than in step 5's own place,
/// because a refusal the caller alone earned owes the deployment no
/// request — and step 5's failure cell reads "before any request",
/// which the numbered order would otherwise leave false of every run
/// reaching it (CR1 Open Questions).
fn refuse_parent_spelling(value: &str) -> Result<(), CliError> {
    if value.trim().is_empty() {
        return Err(CliError::Config {
            message: "--parent cannot be empty".to_string(),
        });
    }
    let all_digits = value.bytes().all(|b| b.is_ascii_digit());
    if all_digits && value.parse::<u64>().unwrap_or(u64::MAX) < 1 {
        return Err(CliError::Config {
            message: "--parent must be \u{2265} 1".to_string(),
        });
    }
    Ok(())
}

/// `resolve_write_target` 5 and 6: the parent every request of this run
/// claims. A full hash stands unresolved; every other spelling is
/// resolved into a version and that version's full hash.
async fn resolve_parent(
    client: &SynsClient,
    repo_id: &str,
    token: &str,
    spelling: &str,
) -> Result<ParentRef, CliError> {
    if is_full_commit_hash(spelling) {
        // The hash-addressed read `cmd_edit` 2 then makes runs the same
        // commit walk a version request at this hash would run, so
        // resolving it first buys nothing and pays it twice (`Q-02`).
        return Ok(ParentRef {
            commit_sha: spelling.to_string(),
            version: None,
        });
    }
    let (entry, _raw) = client.get_version(repo_id, Some(token), spelling).await?;
    Ok(ParentRef {
        commit_sha: entry.sha,
        version: Some(entry.version),
    })
}

/// Binds the repository, reads the credential, runs the checkout guard,
/// reads the repository and pins the parent (SPEC u271 Behaviour,
/// `resolve_write_target`).
pub async fn resolve_write_target(
    config: &Config,
    cwd: &Path,
    opts: &WriteOptions,
) -> Result<WriteTarget, CliError> {
    // 5's own refusals, raised ahead of step 1: no request is made for
    // a spelling the caller alone got wrong.
    refuse_parent_spelling(&opts.parent)?;

    // 1 — bind the repository. A `--repo` value takes it outright, in
    // exactly the spelling the five read verbs admit; no ladder, no
    // mismatch check and no skip runs beside it.
    let repo_id = match opts.repo.as_deref() {
        Some(named) => named.to_ascii_lowercase(),
        None => {
            // `if_repo: false` refuses rather than skipping — these
            // verbs register no skip, a skipped write being a change its
            // caller believes landed.
            let quiet = Output::new(false);
            match resolve_full_or_skip(None, cwd, false, &quiet)? {
                Some((owner, name)) => format!("{owner}/{name}"),
                None => {
                    return Err(CliError::RepoIdentityUnknown {
                        remedy: IdentityRemedy::IdentityFile,
                    });
                }
            }
        }
    };

    // 2 — the stored credential, before any request.
    let token = TokenStore::new(config.credentials_path())
        .read()?
        .ok_or(CliError::AuthRequired)?;

    // 3 — the checkout guard against the bound repository, before any
    // request.
    let checkout = checkout_of(config, cwd, &repo_id)?;

    let client = SynsClient::new(config.server_url())?;

    // 4 — read the repository, so no push of this run can be the request
    // that creates one (`issues/069`).
    client.get_repo(&repo_id, Some(&token)).await?;

    // 5 and 6 — pin the parent.
    let parent = resolve_parent(&client, &repo_id, &token, &opts.parent).await?;

    Ok(WriteTarget {
        repo_id,
        token,
        parent,
        checkout,
    })
}

// ---- the one publication ----------------------------------------------

/// The caption this run's commit carries (`commit_changeset` 1).
fn caption(opts: &WriteOptions, default: &str) -> Result<String, CliError> {
    match opts.message.as_deref() {
        Some(given) if given.trim().is_empty() => Err(CliError::Config {
            message: "--message cannot be empty".to_string(),
        }),
        Some(given) => Ok(given.to_string()),
        None => Ok(default.to_string()),
    }
}

/// The body this run sends (`commit_changeset` 2 and 3).
fn push_body(
    target: &WriteTarget,
    changeset: &Changeset,
    opts: &WriteOptions,
    message: String,
) -> PushRequest {
    PushRequest {
        files: changeset
            .files
            .iter()
            .map(|(path, content)| PushFileEntry {
                path: path.clone(),
                sha: blob_sha1(content.as_bytes()),
                content: Some(content.clone()),
                content_base64: None,
            })
            .collect(),
        deletions: if changeset.deletions.is_empty() {
            None
        } else {
            Some(
                changeset
                    .deletions
                    .iter()
                    .map(|path| PushDeleteEntry { path: path.clone() })
                    .collect(),
            )
        },
        message: Some(message),
        // No author on any branch, so the server names the session's
        // user (`D-065`).
        author: None,
        parent_sha: Some(target.parent.commit_sha.clone()),
        description: None,
        tags: None,
        status: None,
        visibility: None,
        provenance: opts.provenance.block(),
    }
}

/// An answer carrying `currentSha` becomes the conflict refusal at exit
/// `7`; a `CONFLICT` carrying none keeps exit `1`, so one inbound answer
/// never takes two exits in one run.
fn fold_conflict(err: CliError, parent: &str) -> CliError {
    match err {
        CliError::Api {
            status: Some(409),
            ref error,
            context: Some(ApiErrorContext::HeadMoved { ref current_sha }),
        } if error == "conflict" => CliError::WriteConflict {
            parent: parent.to_string(),
            current_sha: current_sha.clone(),
        },
        other => other,
    }
}

/// The checkout root a run leaves one version behind: `Some` only where
/// the target carries one and the answer counted a changed file. A run
/// that changed none leaves a clean folder unnamed, because the head it
/// matches never moved.
pub fn checkout_left_behind<'a>(
    target: &'a WriteTarget,
    response: &PushResponse,
) -> Option<&'a Path> {
    target
        .checkout
        .as_deref()
        .filter(|_| response.files_changed > 0)
}

/// `commit_changeset` 4 in machine-readable mode: the served body with
/// `checkoutBehind` added where a checkout was left behind, so the
/// caller that never reads the diagnostic stream is told as plainly as
/// the caller that does.
pub fn with_checkout_behind(
    body: serde_json::Value,
    target: &WriteTarget,
    response: &PushResponse,
) -> serde_json::Value {
    let mut body = body;
    if let (Some(root), Some(map)) = (checkout_left_behind(target, response), body.as_object_mut())
    {
        map.insert(
            "checkoutBehind".to_string(),
            serde_json::Value::from(root.display().to_string()),
        );
    }
    body
}

/// The run's one answer (`commit_changeset` 4 and 5): the served body
/// under machine-readable mode, the write report otherwise.
fn report(output: &Output, target: &WriteTarget, response: &PushResponse, raw: serde_json::Value) {
    let changed = response.files_changed > 0;
    let behind = checkout_left_behind(target, response);

    if output.is_json() {
        output.json(&with_checkout_behind(raw, target, response));
        return;
    }

    if changed {
        eprintln!("{}", wrote_line(response.version, &response.commit_sha));
        if let Some(root) = behind {
            eprintln!("{}", checkout_behind_line(root));
        }
    } else {
        eprintln!(
            "{}",
            nothing_changed_line(response.version, &response.commit_sha)
        );
    }
}

/// Publishes the changeset as exactly one commit at the pinned parent,
/// and writes the run's one answer (SPEC u271 Behaviour,
/// `commit_changeset`).
///
/// One call of `EP-push` per run and no second request of any kind: the
/// claimed parent is `target.parent.commit_sha` under every combination
/// of options, nothing is retried and nothing is resent, and no file of
/// any folder is read, written or removed.
pub async fn commit_changeset(
    config: &Config,
    output: &Output,
    target: &WriteTarget,
    changeset: Changeset,
    opts: &WriteOptions,
    default_message: &str,
) -> Result<(), CliError> {
    // 1 and 2 — the caption, and each content's blob hash beside its path.
    let message = caption(opts, default_message)?;
    let request = push_body(target, &changeset, opts, message);

    // 3 — the one call of `EP-push`.
    let client = SynsClient::new(config.server_url())?;
    let (response, raw) = client
        .push(&target.repo_id, &target.token, &request)
        .await
        .map_err(|err| fold_conflict(err, &target.parent.commit_sha))?;

    // 4 and 5 — compose the run's one answer and render it.
    report(output, target, &response, raw);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serial_test::serial;
    use wiremock::matchers::{method, path as path_matcher};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    const HEAD_SHA: &str = "aa11bb22cc33dd44ee55ff6600778899001122bb";
    const MOVED_SHA: &str = "cc33dd44ee55ff6600778899001122bbaa11bb22";

    fn repo_body() -> serde_json::Value {
        serde_json::json!({
            "owner": "alice", "name": "notes", "description": null,
            "commitSha": HEAD_SHA, "status": "active", "author": null, "tags": [],
            "visibility": "public", "forkedFrom": null, "forkCount": 0,
            "fileCount": 1, "role": null,
            "createdAt": "2026-01-01T00:00:00Z", "updatedAt": "2026-01-01T00:00:00Z",
        })
    }

    fn version_body(version: u32, sha: &str) -> serde_json::Value {
        serde_json::json!({
            "version": version, "sha": sha, "parentSha": null, "message": "m",
            "messageBody": null, "author": "alice",
            "createdAt": "2026-01-01T00:00:00Z", "filesChanged": ["a.md"],
        })
    }

    fn push_body(files_changed: u32, version: u32, sha: &str) -> serde_json::Value {
        serde_json::json!({
            "commitSha": sha, "version": version,
            "filesChanged": files_changed, "created": false,
        })
    }

    /// A config directory holding a credential and a cache directory of
    /// this test's own, both bound through the environment as the binary
    /// binds them.
    struct Env {
        config: Config,
        _home: tempfile::TempDir,
        _cache: tempfile::TempDir,
    }

    fn env_for(server_uri: &str) -> Env {
        let home = tempfile::tempdir().expect("config dir");
        let cache = tempfile::tempdir().expect("cache dir");
        unsafe { std::env::set_var("SYNS_CONFIG_DIR", home.path()) };
        unsafe { std::env::set_var("SYNS_CACHE_DIR", cache.path()) };
        let config = Config::new(Some(server_uri)).expect("config");
        TokenStore::new(config.credentials_path())
            .write_with_username("test-token", Some("alice"))
            .expect("credential");
        unsafe { std::env::remove_var("SYNS_CONFIG_DIR") };
        unsafe { std::env::remove_var("SYNS_CACHE_DIR") };
        Env {
            config,
            _home: home,
            _cache: cache,
        }
    }

    fn write_options(parent: &str) -> WriteOptions {
        WriteOptions {
            repo: Some("alice/notes".to_string()),
            parent: parent.to_string(),
            message: None,
            provenance: ProvenanceOptions::default(),
        }
    }

    fn target_at(checkout: Option<PathBuf>) -> WriteTarget {
        WriteTarget {
            repo_id: "alice/notes".to_string(),
            token: "test-token".to_string(),
            parent: ParentRef {
                commit_sha: HEAD_SHA.to_string(),
                version: None,
            },
            checkout,
        }
    }

    fn response(files_changed: u32) -> PushResponse {
        serde_json::from_value(push_body(files_changed, 8, MOVED_SHA)).expect("response")
    }

    /// A folder carrying an identity file and one file, with a working
    /// copy whose recorded base is or is not the folder as it stands.
    fn seed_checkout(config: &Config, root: &Path, owner: &str, name: &str, record: bool) {
        std::fs::write(
            root.join(".syns.yaml"),
            format!("owner: {owner}\nname: {name}\n"),
        )
        .unwrap();
        std::fs::write(root.join("a.md"), "keep one").unwrap();
        if !record {
            return;
        }
        let copy = WorkingCopy::open(config.cache_dir(), owner, name, root).unwrap();
        copy.record_base(HEAD_SHA, folder_hashes(root)).unwrap();
    }

    // ---- step 3 -------------------------------------------------------

    // SPEC u271 Behaviour, `checkout_of` 3: a folder equal to its
    // recorded base is answered as the run's checkout root.
    #[test]
    #[serial]
    fn a_clean_checkout_answers_its_own_root() {
        let env = env_for("https://syns.dev");
        let work = tempfile::tempdir().unwrap();
        seed_checkout(&env.config, work.path(), "alice", "notes", true);

        let root = checkout_of(&env.config, work.path(), "alice/notes").unwrap();
        assert_eq!(
            root.map(|r| std::fs::canonicalize(r).unwrap()),
            Some(std::fs::canonicalize(work.path()).unwrap())
        );
    }

    // `checkout_of` 3: a copy recording no base reads as holding
    // unpublished local work wherever its folder holds a file.
    #[test]
    #[serial]
    fn a_checkout_recording_no_base_is_refused() {
        let env = env_for("https://syns.dev");
        let work = tempfile::tempdir().unwrap();
        seed_checkout(&env.config, work.path(), "alice", "notes", false);

        let err = checkout_of(&env.config, work.path(), "alice/notes").unwrap_err();
        assert!(err.to_string().contains(&work.path().display().to_string()));
        assert!(err.to_string().contains("syns sync"));
        assert_eq!(err.exit_code(), 1);
    }

    // `checkout_of` 3: an edited file puts the folder past its base.
    #[test]
    #[serial]
    fn a_dirty_checkout_is_refused_and_names_no_override() {
        let env = env_for("https://syns.dev");
        let work = tempfile::tempdir().unwrap();
        seed_checkout(&env.config, work.path(), "alice", "notes", true);
        std::fs::write(work.path().join("a.md"), "edited since").unwrap();

        let err = checkout_of(&env.config, work.path(), "alice/notes").unwrap_err();
        let line = err.to_string();
        assert!(line.contains("holds unpublished local changes for alice/notes"));
        assert!(!line.contains("--"), "the refusal names no option: {line}");
    }

    // `checkout_of` 1: the nearest identity file naming another
    // repository answers no checkout, letter case aside.
    #[test]
    #[serial]
    fn an_identity_naming_another_repository_answers_no_checkout() {
        let env = env_for("https://syns.dev");
        let work = tempfile::tempdir().unwrap();
        seed_checkout(&env.config, work.path(), "alice", "notes", false);

        assert!(
            checkout_of(&env.config, work.path(), "bob/other")
                .unwrap()
                .is_none()
        );
        assert!(checkout_of(&env.config, work.path(), "ALICE/NOTES").is_err());
    }

    // `checkout_of` 1: no identity file at or above the directory.
    #[test]
    #[serial]
    fn a_directory_holding_no_identity_file_answers_no_checkout() {
        let env = env_for("https://syns.dev");
        let work = tempfile::tempdir().unwrap();
        let deep = work.path().join("a").join("b");
        std::fs::create_dir_all(&deep).unwrap();

        assert!(
            checkout_of(&env.config, &deep, "alice/notes")
                .unwrap()
                .is_none()
        );
    }

    // CR1-1: a path the recorded base names that a file stands at and
    // the collection left out is an exclusion, not an edit — a
    // convergence drops it from both sides, and so does this guard.
    #[test]
    #[serial]
    fn a_path_the_collection_excludes_does_not_read_as_local_work() {
        let env = env_for("https://syns.dev");
        let work = tempfile::tempdir().unwrap();
        seed_checkout(&env.config, work.path(), "alice", "notes", true);

        // A path the built-in skip list keeps out, standing in the head
        // and so in the base a retrieval recorded.
        std::fs::create_dir_all(work.path().join("dist")).unwrap();
        std::fs::write(work.path().join("dist/index.js"), "built").unwrap();
        let copy =
            WorkingCopy::open(env.config.cache_dir(), "alice", "notes", work.path()).unwrap();
        let mut recorded = folder_hashes(work.path());
        recorded.insert("dist/index.js".to_string(), blob_sha1(b"built"));
        copy.record_base(HEAD_SHA, recorded).unwrap();

        assert!(
            checkout_of(&env.config, work.path(), "alice/notes")
                .unwrap()
                .is_some(),
            "the excluded path is dropped from both sides"
        );

        // An edit to a path the collection does take still refuses.
        std::fs::write(work.path().join("a.md"), "edited since").unwrap();
        assert!(checkout_of(&env.config, work.path(), "alice/notes").is_err());
    }

    // CR1-2: the guard answers none of its three by writing state, so a
    // folder whose repository was never synced there leaves no state
    // directory behind.
    #[test]
    #[serial]
    fn the_guard_creates_no_working_copy_state() {
        let env = env_for("https://syns.dev");
        let work = tempfile::tempdir().unwrap();
        seed_checkout(&env.config, work.path(), "alice", "notes", false);
        let before = cache_entries(env.config.cache_dir());

        assert!(checkout_of(&env.config, work.path(), "alice/notes").is_err());
        assert_eq!(
            cache_entries(env.config.cache_dir()),
            before,
            "a refused guard writes no state"
        );
    }

    /// Every path standing under the cache root.
    fn cache_entries(root: &Path) -> Vec<String> {
        fn walk(dir: &Path, out: &mut Vec<String>) {
            let Ok(entries) = std::fs::read_dir(dir) else {
                return;
            };
            for entry in entries.flatten() {
                out.push(entry.path().display().to_string());
                walk(&entry.path(), out);
            }
        }
        let mut out = Vec::new();
        walk(root, &mut out);
        out.sort();
        out
    }

    /// The folder as the guard collects it.
    fn folder_hashes(root: &Path) -> std::collections::HashMap<String, String> {
        collect_files(
            root,
            &[],
            CollectOptions::default(),
            None,
            &HeldBytes::new(0),
        )
        .unwrap()
        .hashes()
    }

    // SPEC u271 Contract Surface, `text_or_refuse`.
    #[test]
    fn a_nul_bearing_or_undecodable_content_is_refused_by_path() {
        assert_eq!(text_or_refuse("a.md", b"keep one").unwrap(), "keep one");
        assert_eq!(text_or_refuse("a.md", b"").unwrap(), "");

        let nul = text_or_refuse("b.bin", b"ab\0cd").unwrap_err();
        assert_eq!(
            nul.to_string(),
            "cannot write content that is not text: b.bin \u{2014} this repository holds UTF-8 text alone"
        );
        let invalid = text_or_refuse("b.bin", &[0xff, 0xfe, 0x00]).unwrap_err();
        assert_eq!(invalid.to_string(), nul.to_string());
        assert_eq!(invalid.exit_code(), 1);
    }

    // SPEC u271 Contract Surface, `ProvenanceOptions`: each option
    // outranks the environment name beside it, `--task-ref` reading
    // `SYNS_TASK`, and the block is sent whole or not at all.
    #[test]
    fn an_option_value_outranks_the_environment_name_beside_it() {
        let env = |name: &str| match name {
            "SYNS_INTEGRATION" => Some("shell".to_string()),
            "SYNS_RUN" => Some("r-9".to_string()),
            "SYNS_TRIGGER" => Some("cron".to_string()),
            "SYNS_TASK" => Some("t-env".to_string()),
            _ => None,
        };
        let opts = ProvenanceOptions {
            integration: Some("page".to_string()),
            run: None,
            trigger: None,
            task_ref: Some("t-1".to_string()),
        };
        let block = opts.block_over(env).expect("all three names resolve");
        assert_eq!(block.integration, "page");
        assert_eq!(block.run, "r-9");
        assert_eq!(block.trigger, "cron");
        assert_eq!(block.task_ref.as_deref(), Some("t-1"));

        // One required name missing everywhere: no block at all.
        let thin = |name: &str| (name == "SYNS_RUN").then(|| "r-9".to_string());
        assert!(ProvenanceOptions::default().block_over(thin).is_none());

        // A value empty once trimmed reads as unset on both sides.
        let blank = ProvenanceOptions {
            integration: Some("   ".to_string()),
            ..Default::default()
        };
        assert_eq!(
            blank.block_over(env).expect("the name stands").integration,
            "shell"
        );
    }

    // SPEC u271 Contract Surface, the verb default messages.
    #[test]
    fn each_verb_carries_its_own_default_caption() {
        assert_eq!(default_message("edit", Some("a.md")), "edit a.md");
        assert_eq!(default_message("write", Some("b.md")), "write b.md");
        assert_eq!(default_message("rm", Some("a.md")), "rm a.md");
        assert_eq!(default_message("commit", None), "commit");
    }

    // ---- step 4 -------------------------------------------------------

    // SPEC u271 Behaviour, `resolve_write_target` 5: a full-hash
    // `--parent` stands unresolved, so no version request is made.
    #[tokio::test]
    #[serial]
    async fn a_full_hash_parent_makes_no_version_request() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path_matcher("/api/v1/repos/alice/notes"))
            .respond_with(ResponseTemplate::new(200).set_body_json(repo_body()))
            .mount(&server)
            .await;
        let env = env_for(&server.uri());
        let work = tempfile::tempdir().unwrap();

        let target = resolve_write_target(&env.config, work.path(), &write_options(HEAD_SHA))
            .await
            .unwrap();
        assert_eq!(target.parent.commit_sha, HEAD_SHA);
        assert_eq!(target.parent.version, None);
        assert_eq!(target.parent.read_ref(), HEAD_SHA);
        assert!(target.checkout.is_none());

        let paths: Vec<String> = server
            .received_requests()
            .await
            .unwrap()
            .iter()
            .map(|r| r.url.path().to_string())
            .collect();
        assert_eq!(paths, vec!["/api/v1/repos/alice/notes".to_string()]);
    }

    // `resolve_write_target` 5: every other spelling is resolved into a
    // version and that version's full hash.
    #[tokio::test]
    #[serial]
    async fn an_ordinal_parent_is_resolved_through_one_version_request() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path_matcher("/api/v1/repos/alice/notes"))
            .respond_with(ResponseTemplate::new(200).set_body_json(repo_body()))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path_matcher("/api/v1/repos/alice/notes/versions/7"))
            .respond_with(ResponseTemplate::new(200).set_body_json(version_body(7, HEAD_SHA)))
            .mount(&server)
            .await;
        let env = env_for(&server.uri());
        let work = tempfile::tempdir().unwrap();

        let target = resolve_write_target(&env.config, work.path(), &write_options("7"))
            .await
            .unwrap();
        assert_eq!(target.parent.commit_sha, HEAD_SHA);
        assert_eq!(target.parent.version, Some(7));
        assert_eq!(target.parent.read_ref(), "7");
    }

    // `resolve_write_target` 5: an empty value and an all-digit value
    // below `1` are refused before any request.
    #[tokio::test]
    #[serial]
    async fn an_empty_or_below_one_parent_is_refused_before_every_request() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path_matcher("/api/v1/repos/alice/notes"))
            .respond_with(ResponseTemplate::new(200).set_body_json(repo_body()))
            .mount(&server)
            .await;
        let env = env_for(&server.uri());
        let work = tempfile::tempdir().unwrap();

        for (value, message) in [
            ("", "configuration error: --parent cannot be empty"),
            ("0", "configuration error: --parent must be \u{2265} 1"),
            ("000", "configuration error: --parent must be \u{2265} 1"),
        ] {
            let err = resolve_write_target(&env.config, work.path(), &write_options(value))
                .await
                .unwrap_err();
            assert_eq!(err.to_string(), message);
            assert_eq!(err.exit_code(), 1);
        }
        // A refusal the caller alone earned owes the deployment no
        // request at all (CR1 Open Questions).
        assert!(
            server.received_requests().await.unwrap().is_empty(),
            "no request is made for a spelling the caller got wrong"
        );
    }

    // `resolve_write_target` 4: a repository standing at no identity is
    // refused before any body carrying a commit leaves.
    #[tokio::test]
    #[serial]
    async fn a_repository_answering_not_found_refuses_before_any_push() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path_matcher("/api/v1/repos/alice/absent"))
            .respond_with(
                ResponseTemplate::new(404).set_body_json(serde_json::json!({"error":"not_found"})),
            )
            .mount(&server)
            .await;
        let env = env_for(&server.uri());
        let work = tempfile::tempdir().unwrap();

        let mut opts = write_options(HEAD_SHA);
        opts.repo = Some("alice/absent".to_string());
        let err = resolve_write_target(&env.config, work.path(), &opts)
            .await
            .unwrap_err();
        assert_eq!(err.exit_code(), 1);

        let paths: Vec<String> = server
            .received_requests()
            .await
            .unwrap()
            .iter()
            .map(|r| r.url.path().to_string())
            .collect();
        assert_eq!(paths, vec!["/api/v1/repos/alice/absent".to_string()]);
    }

    // `resolve_write_target` 2: the credential is read before any
    // request.
    #[tokio::test]
    #[serial]
    async fn a_run_holding_no_credential_is_refused_before_any_request() {
        let server = MockServer::start().await;
        let home = tempfile::tempdir().unwrap();
        let cache = tempfile::tempdir().unwrap();
        unsafe { std::env::set_var("SYNS_CONFIG_DIR", home.path()) };
        unsafe { std::env::set_var("SYNS_CACHE_DIR", cache.path()) };
        let config = Config::new(Some(&server.uri())).unwrap();
        unsafe { std::env::remove_var("SYNS_CONFIG_DIR") };
        unsafe { std::env::remove_var("SYNS_CACHE_DIR") };
        let work = tempfile::tempdir().unwrap();

        let err = resolve_write_target(&config, work.path(), &write_options(HEAD_SHA))
            .await
            .unwrap_err();
        assert!(matches!(err, CliError::AuthRequired));
        assert!(server.received_requests().await.unwrap().is_empty());
    }

    // `resolve_write_target` 3: the guard refuses ahead of the
    // repository read, so a refused write leaves nothing on the wire.
    #[tokio::test]
    #[serial]
    async fn a_dirty_checkout_refuses_ahead_of_every_request() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path_matcher("/api/v1/repos/alice/notes"))
            .respond_with(ResponseTemplate::new(200).set_body_json(repo_body()))
            .mount(&server)
            .await;
        let env = env_for(&server.uri());
        let work = tempfile::tempdir().unwrap();
        seed_checkout(&env.config, work.path(), "alice", "notes", false);

        let mut opts = write_options(HEAD_SHA);
        opts.repo = None;
        let err = resolve_write_target(&env.config, work.path(), &opts)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("holds unpublished local changes"));
        assert!(server.received_requests().await.unwrap().is_empty());
    }

    // ---- step 5 -------------------------------------------------------

    // SPEC u271 Contract Surface, the write report: the two lines, and
    // the notice that follows the first alone.
    #[test]
    fn the_write_report_spells_its_two_lines_and_its_notice() {
        assert_eq!(
            wrote_line(8, MOVED_SHA),
            format!("wrote version 8, commit {MOVED_SHA}")
        );
        assert_eq!(
            nothing_changed_line(7, HEAD_SHA),
            format!("nothing changed; the head is still version 7, commit {HEAD_SHA}")
        );
        assert_eq!(
            checkout_behind_line(Path::new("/w/notes")),
            "the checkout at /w/notes is now one version behind; syns sync converges it"
        );
    }

    // `commit_changeset` 4: the notice and the key stand where the
    // target carries a checkout root and the answer counted a changed
    // file, and nowhere else.
    #[test]
    fn the_behind_notice_is_withheld_where_no_file_changed() {
        let root = PathBuf::from("/w/notes");
        let target = target_at(Some(root.clone()));
        assert_eq!(
            checkout_left_behind(&target, &response(1)),
            Some(root.as_path())
        );
        assert_eq!(checkout_left_behind(&target, &response(0)), None);
        assert_eq!(checkout_left_behind(&target_at(None), &response(1)), None);

        let served = push_body(1, 8, MOVED_SHA);
        let body = with_checkout_behind(served.clone(), &target, &response(1));
        assert_eq!(body["checkoutBehind"], serde_json::json!("/w/notes"));
        assert_eq!(body["version"], serde_json::json!(8));
        assert_eq!(body["filesChanged"], serde_json::json!(1));

        let unchanged = with_checkout_behind(served, &target, &response(0));
        assert!(unchanged.get("checkoutBehind").is_none());
        assert!(
            with_checkout_behind(push_body(1, 8, MOVED_SHA), &target_at(None), &response(1))
                .get("checkoutBehind")
                .is_none()
        );
    }

    // `commit_changeset` 3: an answer carrying `currentSha` takes the
    // conflict refusal at exit `7`; one carrying none keeps exit `1`.
    #[tokio::test]
    #[serial]
    async fn an_answer_carrying_a_current_sha_takes_exit_seven() {
        let server = MockServer::start().await;
        Mock::given(method("PUT"))
            .and(path_matcher("/api/v1/repos/alice/notes/push"))
            .respond_with(ResponseTemplate::new(409).set_body_json(serde_json::json!({
                "error": "conflict", "message": "Head mismatch", "currentSha": MOVED_SHA,
            })))
            .mount(&server)
            .await;
        let env = env_for(&server.uri());

        let err = commit_changeset(
            &env.config,
            &Output::new(true),
            &target_at(None),
            Changeset {
                files: vec![("a.md".to_string(), "keep two".to_string())],
                deletions: vec![],
            },
            &write_options(HEAD_SHA),
            "edit a.md",
        )
        .await
        .unwrap_err();

        assert_eq!(err.exit_code(), 7);
        assert_eq!(
            err.to_string(),
            format!("conflict: the repository moved past {HEAD_SHA}; its head is now {MOVED_SHA}")
        );
        let doc = err.json_value().expect("the refusal carries a document");
        assert_eq!(doc["currentSha"], serde_json::json!(MOVED_SHA));
    }

    #[tokio::test]
    #[serial]
    async fn a_conflict_naming_no_head_keeps_exit_one() {
        let server = MockServer::start().await;
        Mock::given(method("PUT"))
            .and(path_matcher("/api/v1/repos/alice/notes/push"))
            .respond_with(
                ResponseTemplate::new(409).set_body_json(serde_json::json!({"error": "conflict"})),
            )
            .mount(&server)
            .await;
        let env = env_for(&server.uri());

        let err = commit_changeset(
            &env.config,
            &Output::new(true),
            &target_at(None),
            Changeset {
                files: vec![],
                deletions: vec!["a.md".to_string()],
            },
            &write_options(HEAD_SHA),
            "rm a.md",
        )
        .await
        .unwrap_err();
        assert_eq!(err.exit_code(), 1);
    }

    // `commit_changeset` 1, 2 and 3: the caption, each content's derived
    // hash, the pinned parent, and no author.
    #[tokio::test]
    #[serial]
    async fn one_push_carries_the_pinned_parent_the_caption_and_derived_hashes() {
        let server = MockServer::start().await;
        Mock::given(method("PUT"))
            .and(path_matcher("/api/v1/repos/alice/notes/push"))
            .respond_with(ResponseTemplate::new(200).set_body_json(push_body(1, 8, MOVED_SHA)))
            .mount(&server)
            .await;
        let env = env_for(&server.uri());

        commit_changeset(
            &env.config,
            &Output::new(true),
            &target_at(None),
            Changeset {
                files: vec![("a.md".to_string(), "keep two".to_string())],
                deletions: vec!["gone.md".to_string()],
            },
            &write_options(HEAD_SHA),
            "edit a.md",
        )
        .await
        .unwrap();

        let requests = server.received_requests().await.unwrap();
        assert_eq!(requests.len(), 1, "one push per run and no second request");
        let body: serde_json::Value = requests[0].body_json().unwrap();
        assert_eq!(body["parentSha"], serde_json::json!(HEAD_SHA));
        assert_eq!(body["message"], serde_json::json!("edit a.md"));
        assert!(body.get("author").is_none());
        assert_eq!(body["files"][0]["path"], serde_json::json!("a.md"));
        assert_eq!(body["files"][0]["content"], serde_json::json!("keep two"));
        assert_eq!(
            body["files"][0]["sha"],
            serde_json::json!(blob_sha1(b"keep two"))
        );
        assert_eq!(body["deletions"][0]["path"], serde_json::json!("gone.md"));
        assert!(body.get("provenance").is_none());
    }

    // `commit_changeset` 1: a `--message` empty once trimmed is refused
    // before any request.
    #[tokio::test]
    #[serial]
    async fn a_message_empty_once_trimmed_is_refused_before_any_request() {
        let server = MockServer::start().await;
        let env = env_for(&server.uri());
        let mut opts = write_options(HEAD_SHA);
        opts.message = Some("   ".to_string());

        let err = commit_changeset(
            &env.config,
            &Output::new(true),
            &target_at(None),
            Changeset {
                files: vec![],
                deletions: vec!["a.md".to_string()],
            },
            &opts,
            "rm a.md",
        )
        .await
        .unwrap_err();
        assert_eq!(
            err.to_string(),
            "configuration error: --message cannot be empty"
        );
        assert!(server.received_requests().await.unwrap().is_empty());
    }
}
