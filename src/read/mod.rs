//! The shared read layer of the five read verbs (SPEC u270).
//!
//! Every read verb takes the same three options, resolves one repository
//! at one reference before it asks for any content, and sends that
//! reference's decimal ordinal as `ref` on every later request — so a
//! publication landing mid-run changes nothing the run reads, and a
//! time-travel read never pays the commit walk a full content hash costs
//! (`issues/148-ref-by-content-hash-resolved-by-scanning-every-commit`).

pub mod batch;
pub mod closure;
pub mod files;
pub mod listing;
pub mod run;
pub mod store;
pub mod v2;
pub mod view;

use crate::auth::token::TokenStore;
use crate::client::SynsClient;
use crate::commands::pull::is_repository_shape;
use crate::config::Config;
use crate::errors::{CliError, IdentityRemedy};
use crate::output::Output;
use crate::repo::folder::{FolderScope, current_dir, resolve_folder_scope, resolve_scoped_or_skip};
use crate::repo::identity::identity_head;
use crate::repo::if_repo::resolve_full_or_skip;

/// The count of content reads outstanding at once, with nothing else
/// pacing them: grep's fan-out and `syns cat PATH…` alike (Q-0119,
/// roadmap 278).
pub const IN_FLIGHT: usize = 16;

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
    /// The scoped folder the run stands in, none wherever `--repo`
    /// stood (SPEC u290 Contract Surface, `ReadTarget.folder`).
    pub folder: Option<FolderScope>,
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

/// Which registered line a read verb's missing path takes where no
/// `--version` stood: `syns cat` and `syns read` name a file, `syns ls`,
/// `syns glob` and `syns grep` a path.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PathLine {
    File,
    Path,
}

/// The refusal a read verb's path read raises (issue 160, `Boundary B`):
/// a `404` naming the repository becomes the repository-not-found
/// refusal, and any other names the path the run asked for — at the
/// pinned version where `--version` stood — in both output modes alike.
pub fn path_refusal(
    err: CliError,
    opts: &ReadOptions,
    target: &ReadTarget,
    path: &str,
    line: PathLine,
) -> CliError {
    if err.names_missing_repository() {
        return err.repo_or(&target.repo_id);
    }
    if opts.version.is_some() {
        return read_not_found(err, opts, &target.reference, path);
    }
    match line {
        PathLine::File => err.with_cat_path_context(path.to_string()),
        PathLine::Path => err.with_ls_path_context(path.to_string()),
    }
}

/// A refusal of the repository's own read — its record, its head, its
/// version list (issue 160): any `404` there names the repository,
/// absent and unreadable alike.
pub fn repository_read_refusal(err: CliError, repo_id: &str) -> CliError {
    match &err {
        CliError::Api {
            status: Some(404),
            error,
            ..
        } if error == "not_found" || error == "repo_not_found" => {
            err.with_repo_not_found_context(crate::errors::repo_not_found_refusal(repo_id))
        }
        _ => err,
    }
}

/// A refusal of a read naming a reference the caller typed (issue 160):
/// the repository-not-found refusal where the `404` names the
/// repository, and the version-not-found refusal for `asked` otherwise.
pub fn reference_refusal(err: CliError, repo_id: &str, asked: &str) -> CliError {
    if err.names_missing_repository() {
        return err.repo_or(repo_id);
    }
    err.with_version_not_found_context(version_not_found_refusal(asked))
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

/// `configuration error: version cannot be empty` for the empty string
/// and `configuration error: version must be ≥ 1` for an all-digit value
/// below `1`, each raised before any request leaves (SPEC u283,
/// `refuse_reference_spelling`). Every other spelling reaches the server
/// unchecked.
pub(crate) fn refuse_reference_spelling(value: &str) -> Result<(), CliError> {
    if value.is_empty() {
        return Err(CliError::Config {
            message: "version cannot be empty".to_string(),
        });
    }
    let all_digits = value.bytes().all(|b| b.is_ascii_digit());
    if all_digits && value.parse::<u64>().unwrap_or(u64::MAX) < 1 {
        return Err(CliError::Config {
            message: "version must be \u{2265} 1".to_string(),
        });
    }
    Ok(())
}

/// The empty-path refusal (SPEC u297 Contract Surface): a path typed
/// empty, which inside a scoped folder would name the folder itself and
/// under `syns rm` would name the folder or nothing at all.
pub const EMPTY_PATH_REFUSAL: &str = "path cannot be empty";

/// Refuses a typed path left empty once every character the `file path`
/// field kind trims is taken off its ends (SPEC u297 Contract Surface,
/// `refuse_empty_path`). The set is ECMAScript's `String.prototype.trim`,
/// which the server applies, rather than Rust's `char::is_whitespace`:
/// `U+FEFF` is taken off and `U+0085` is not, so the refusal answers
/// exactly where the server would read the path as empty.
pub fn refuse_empty_path(typed: &str) -> Result<(), CliError> {
    if typed.chars().all(trimmed_by_the_file_path_field_kind) {
        return Err(CliError::Config {
            message: EMPTY_PATH_REFUSAL.to_string(),
        });
    }
    Ok(())
}

/// Whether ECMAScript's `String.prototype.trim` takes `c` off an end:
/// its WhiteSpace and LineTerminator productions.
/// A tree path as the server reads it (276 row 17, `queryPath`): every
/// trailing `/` taken off, then what ECMAScript's `trim` takes off its
/// ends (`filePathSchema`). The closure's first block, and so the view's
/// scope, name the path so read (roadmap 279, REVIEW-syns-cli I1).
pub fn as_server_reads(path: &str) -> &str {
    path.trim_end_matches('/')
        .trim_matches(trimmed_by_the_file_path_field_kind)
}

fn trimmed_by_the_file_path_field_kind(c: char) -> bool {
    matches!(
        c,
        '\u{0009}'..='\u{000D}'
            | '\u{0020}'
            | '\u{00A0}'
            | '\u{1680}'
            | '\u{2000}'..='\u{200A}'
            | '\u{2028}'
            | '\u{2029}'
            | '\u{202F}'
            | '\u{205F}'
            | '\u{3000}'
            | '\u{FEFF}'
    )
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
    resolve_read_target_through(config, output, opts, true).await
}

/// `resolve_read_target`, an unpinned reference resolved by one v2 `head`
/// with `If-None-Match` from the store's `heads/` where `v2` holds and the
/// server answers it, else as today (roadmap 279, D-243 §9).
pub async fn resolve_read_target_through(
    config: &Config,
    output: &Output,
    opts: &ReadOptions,
    v2: bool,
) -> Result<Option<ReadTarget>, CliError> {
    // 1 — bind the repository and the folder (SPEC u290).
    let Some((repo_id, folder)) = bind_repository(opts.repo.as_deref(), opts.if_repo, output)?
    else {
        return Ok(None);
    };

    let token = TokenStore::new(config.credentials_path())
        .read()
        .ok()
        .flatten();
    let client = SynsClient::new(config.server_url())?;

    let no_commit = || CliError::Api {
        status: Some(422),
        error: "validation_error".to_string(),
        context: None,
    };

    // 2 — the reference to resolve. SPEC u302 `resolve_read_target` 1:
    // with no `--version`, a run addressing a shared folder's identity —
    // inside a folder bound to it, or through `--repo` where the record
    // marks one — pins the identity's newest listed version, reading no
    // version after it.
    let pinned = |entry: crate::client::VersionEntry, folder| {
        Ok(Some(ReadTarget {
            repo_id: repo_id.clone(),
            token: token.clone(),
            reference: ResolvedRef {
                version: entry.version,
                commit_sha: entry.sha,
            },
            folder,
        }))
    };
    let reference = match opts.version.as_deref() {
        Some(value) => {
            refuse_reference_spelling(value)?;
            value.to_string()
        }
        None => {
            // Roadmap 279: one v2 head, the held commit sent as
            // `If-None-Match`; a `404` reads the head as today.
            if v2
                && let Some(reference) =
                    v2_head(config, &client, token.as_deref(), &repo_id).await?
            {
                return Ok(Some(ReadTarget {
                    repo_id: repo_id.clone(),
                    token: token.clone(),
                    reference,
                    folder,
                }));
            }
            let through_identity = match &folder {
                Some(scope) => scope.identity.is_some(),
                None => false,
            };
            if through_identity {
                let newest = identity_head(&client, token.as_deref(), &repo_id)
                    .await
                    .map_err(|e| repository_read_refusal(e, &repo_id))?
                    .ok_or_else(no_commit)?;
                return pinned(newest, folder);
            }
            // Roadmap 277 (LAT-21): the record and the newest version are
            // asked at once. The newest answers the head wherever its hash
            // is the head's, and the head is looked up by its hash only
            // where it is not; the newest is the reference itself where the
            // record marks a shared folder.
            let (repo, newest) = tokio::join!(
                client.get_repo(&repo_id, token.as_deref()),
                identity_head(&client, token.as_deref(), &repo_id),
            );
            let repo = repo.map_err(|e| repository_read_refusal(e, &repo_id))?;
            if repo.shared_folder {
                let newest = newest.map_err(|e| repository_read_refusal(e, &repo_id))?;
                return pinned(newest.ok_or_else(no_commit)?, folder);
            }
            let sha = repo.commit_sha.ok_or_else(no_commit)?;
            match newest {
                Ok(Some(newest)) if newest.sha == sha => return pinned(newest, folder),
                _ => sha,
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
            Some(typed) => reference_refusal(e, &repo_id, typed),
            None => e.repo_or(&repo_id),
        })?;

    // 4 — pin the pair.
    pinned(entry, folder)
}

/// The head of `repo_id` through one v2 `head` (D-243 §9): a `304` to
/// the commit `heads/` holds answers the held commit and number with no
/// further lookup; `{"commit":null}` today's empty-repository refusal;
/// none where the server answers no v2, or names no number.
pub(crate) async fn v2_head(
    config: &Config,
    client: &SynsClient,
    token: Option<&str>,
    repo_id: &str,
) -> Result<Option<ResolvedRef>, CliError> {
    let spot = store::StoreSpot::of(config, token);
    let held = spot.open(false).and_then(|store| store.head(repo_id));
    match v2::head(
        client,
        repo_id,
        token,
        held.as_ref().map(|(c, _)| c.as_str()),
    )
    .await
    {
        Ok(v2::HeadAnswer::NotModified) => Ok(held.map(|(commit_sha, version)| ResolvedRef {
            version,
            commit_sha,
        })),
        Ok(v2::HeadAnswer::Head {
            commit: Some(commit_sha),
            version: Some(version),
        }) => {
            if let Some(store) = spot.open(true) {
                store.write_head(repo_id, &commit_sha, version);
            }
            Ok(Some(ResolvedRef {
                version,
                commit_sha,
            }))
        }
        Ok(v2::HeadAnswer::Head { commit: None, .. }) => Err(CliError::Api {
            status: Some(422),
            error: "validation_error".to_string(),
            context: None,
        }),
        Ok(v2::HeadAnswer::Head { version: None, .. }) => Ok(None),
        Err(err) if v2::is_absent(&err) => Ok(None),
        Err(err) => Err(err),
    }
}

/// Binds the repository and the folder a read runs against (SPEC u290
/// Behaviour, `resolve_read_target` 1): a `--repo` value lower-cased
/// with no folder and no identity file read, and `resolve_scoped_or_skip`
/// from the working directory otherwise, its refusal naming `--repo`.
/// Answers `None` only where the skip envelope was written.
pub(crate) fn bind_repository(
    repo: Option<&str>,
    if_repo: bool,
    output: &Output,
) -> Result<Option<(String, Option<FolderScope>)>, CliError> {
    if let Some(named) = repo {
        return Ok(Some((named.to_ascii_lowercase(), None)));
    }
    // The ladder's refusal names `--repo`, which every read verb takes
    // (SPEC u283, `IdentityRemedy::RepoOption`).
    Ok(resolve_scoped_or_skip(&current_dir()?, if_repo, output)
        .map_err(|e| e.with_identity_remedy(IdentityRemedy::RepoOption))?
        .map(|(owner, name, folder)| (format!("{owner}/{name}"), folder)))
}

/// The folder a read verb's step 1 binds (SPEC u290 Behaviour, `cmd_ls`
/// 1): none where `--repo` stands, and `resolve_folder_scope` from the
/// working directory otherwise.
pub fn bind_read_folder(opts: &ReadOptions) -> Result<Option<FolderScope>, CliError> {
    if opts.repo.is_some() {
        return Ok(None);
    }
    resolve_folder_scope(&current_dir()?)
}

/// A served path counted from the run's folder, and the path as served
/// where the run stands in none or the path lies outside it.
pub fn counted_from(folder: Option<&FolderScope>, served: &str) -> String {
    folder
        .and_then(|f| f.served_path(served))
        .unwrap_or_else(|| served.to_string())
}

/// The leaving-folder refusal (SPEC u290 Contract Surface).
fn leaving_folder(typed: &str, folder: &FolderScope) -> CliError {
    CliError::Config {
        message: format!(
            "{typed} names no path inside the folder {}; name one counted from it",
            folder.dir.display()
        ),
    }
}

/// The refusal of a path climbing above the repository's root with no
/// folder bound (issue 211, D-134).
fn leaving_repository(typed: &str) -> CliError {
    CliError::Config {
        message: format!(
            "{typed} names no path inside the repository; name one counted from its root"
        ),
    }
}

/// `typed`'s segments with `.` and empty ones dropped and each `..`
/// taking off the segment before it, none where a `..` climbs above the
/// start (issue 211, D-134).
fn resolve_dot_segments(typed: &str) -> Option<Vec<&str>> {
    let mut resolved = Vec::new();
    for segment in typed.split('/') {
        match segment {
            "" | "." => {}
            ".." => {
                resolved.pop()?;
            }
            named => resolved.push(named),
        }
    }
    Some(resolved)
}

/// A typed path argument as a path in the repository (SPEC u290
/// Contract Surface, `repository_argument`; issue 211, D-134): `.` and
/// `..` segments resolved lexically under every binding. With no folder
/// a value holding no dot segment unchanged, one resolving to the root
/// none, and one climbing above the root refused before any request;
/// inside a folder the folder's path where nothing was typed or the value
/// resolves to the folder itself, and the resolved value joined under it
/// otherwise, a value opening with `/` or climbing out refused before any
/// request. Inside a folder a value `refuse_empty_path` refuses is
/// refused with the empty-path refusal (SPEC u297 Behaviour,
/// `repository_argument` 3).
pub fn repository_argument(
    folder: Option<&FolderScope>,
    typed: Option<&str>,
) -> Result<Option<String>, CliError> {
    let Some(folder) = folder else {
        let Some(typed) = typed else {
            return Ok(None);
        };
        if !typed.split('/').any(|seg| seg == "." || seg == "..") {
            return Ok(Some(typed.to_string()));
        }
        let resolved = resolve_dot_segments(typed).ok_or_else(|| leaving_repository(typed))?;
        return Ok((!resolved.is_empty()).then(|| resolved.join("/")));
    };
    // SPEC u302: inside a folder bound to its identity, nothing typed
    // names the identity's whole tree, and a typed path stands as typed.
    let Some(typed) = typed else {
        return Ok(folder.identity.is_none().then(|| folder.path.clone()));
    };
    refuse_empty_path(typed)?;
    if typed.starts_with('/') {
        return Err(leaving_folder(typed, folder));
    }
    let resolved = resolve_dot_segments(typed).ok_or_else(|| leaving_folder(typed, folder))?;
    if resolved.is_empty() {
        return Ok(folder.identity.is_none().then(|| folder.path.clone()));
    }
    Ok(Some(folder.request_path(&resolved.join("/"))))
}

/// A path typed for the `files` answer of `syns cat PATH…` as a path in
/// the repository (D-242 Amended §7): `repository_argument`'s mapping,
/// with a value left empty once trimmed, and every value resolving to
/// the repository's root or to the scoped folder itself (`.`, `a/..`),
/// refused as the empty path before any request, inside a folder or not.
pub fn listed_argument(folder: Option<&FolderScope>, typed: &str) -> Result<String, CliError> {
    let empty = || CliError::Config {
        message: EMPTY_PATH_REFUSAL.to_string(),
    };
    refuse_empty_path(typed)?;
    if resolve_dot_segments(typed).is_some_and(|resolved| resolved.is_empty()) {
        return Err(empty());
    }
    repository_argument(folder, Some(typed))?.ok_or_else(empty)
}

/// A typed path a file verb acts on (`syns revert`, the write verbs) as
/// a path in the repository: `repository_argument`'s mapping, a value
/// whose dot segments resolve to the folder itself or the repository's
/// root refused before any request, since no file stands there (issue
/// 211, D-134; SPEC u297).
pub fn file_argument(folder: Option<&FolderScope>, typed: &str) -> Result<String, CliError> {
    let has_dot_segment = typed.split('/').any(|seg| seg == "." || seg == "..");
    if has_dot_segment && resolve_dot_segments(typed).is_some_and(|resolved| resolved.is_empty()) {
        return Err(CliError::Config {
            message: match folder {
                Some(folder) => format!(
                    "{typed} names the folder {} itself; name a file counted from it",
                    folder.dir.display()
                ),
                None => format!("{typed} names the repository's root; name a file counted from it"),
            },
        });
    }
    Ok(match folder {
        Some(folder) => repository_argument(Some(folder), Some(typed))?
            .unwrap_or_else(|| folder.request_path("")),
        None => repository_argument(None, Some(typed))?.unwrap_or_default(),
    })
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
    /// The scoped folder `resolve_scoped_repo_scope` bound, none on
    /// every scope `resolve_repo_scope` answers (SPEC u290).
    pub folder: Option<FolderScope>,
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
            let current_dir = current_dir()?;
            // The ladder's refusal names `--repo`, which both verbs take
            // (SPEC u283, `IdentityRemedy::RepoOption`).
            match resolve_full_or_skip(None, &current_dir, args.if_repo, output)
                .map_err(|e| e.with_identity_remedy(IdentityRemedy::RepoOption))?
            {
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
    Ok(Some(RepoScope {
        repo_id,
        token,
        folder: None,
    }))
}

/// `syns history show REF`'s binding (SPEC u290 Behaviour,
/// `resolve_scoped_repo_scope`): the repository and the folder bound as
/// `resolve_read_target` binds them, then the credential read as
/// `resolve_repo_scope` reads it. `resolve_repo_scope` stands as it
/// was, so a verb built on it meets the malformed-file refusal inside a
/// folder.
pub async fn resolve_scoped_repo_scope(
    config: &Config,
    output: &Output,
    args: &RepoScopeArgs,
) -> Result<Option<RepoScope>, CliError> {
    let Some((repo_id, folder)) = bind_repository(args.repo.as_deref(), args.if_repo, output)?
    else {
        return Ok(None);
    };
    let token = TokenStore::new(config.credentials_path())
        .read()
        .ok()
        .flatten();
    Ok(Some(RepoScope {
        repo_id,
        token,
        folder,
    }))
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

    fn q3() -> FolderScope {
        FolderScope {
            dir: std::path::PathBuf::from("/w/clients/q3"),
            owner: "alice".into(),
            name: "work".into(),
            path: "clients/q3".into(),
            checkout: None,
            enclosing: Vec::new(),
            identity: None,
        }
    }

    // SPEC u290 Contract Surface, `repository_argument`.
    #[test]
    fn repository_argument_maps_a_typed_path_under_the_folder() {
        assert_eq!(repository_argument(None, None).unwrap(), None);
        assert_eq!(
            repository_argument(None, Some("a/../x"))
                .unwrap()
                .as_deref(),
            Some("x")
        );
        let folder = q3();
        assert_eq!(
            repository_argument(Some(&folder), None).unwrap().as_deref(),
            Some("clients/q3")
        );
        assert_eq!(
            repository_argument(Some(&folder), Some("notes/a.md"))
                .unwrap()
                .as_deref(),
            Some("clients/q3/notes/a.md")
        );
        for typed in ["/x", "../x", "a/../../x", ".."] {
            let err = repository_argument(Some(&folder), Some(typed)).unwrap_err();
            assert_eq!(
                err.to_string(),
                format!(
                    "configuration error: {typed} names no path inside the folder /w/clients/q3; name one counted from it"
                )
            );
            assert_eq!(err.exit_code(), 1);
        }
    }

    // issue 211, D-134: inside a folder a dot segment resolves against
    // it, `.` naming the folder itself as nothing typed does.
    #[test]
    fn repository_argument_resolves_dot_segments_inside_a_folder() {
        let folder = q3();
        for (typed, mapped) in [
            ("./x.md", "clients/q3/x.md"),
            ("sub/../x.md", "clients/q3/x.md"),
            ("a/./b", "clients/q3/a/b"),
            ("welcome/../notes", "clients/q3/notes"),
            ("3/../x", "clients/q3/x"),
            (".", "clients/q3"),
            ("./", "clients/q3"),
            ("a/..", "clients/q3"),
        ] {
            assert_eq!(
                repository_argument(Some(&folder), Some(typed))
                    .unwrap()
                    .as_deref(),
                Some(mapped),
                "{typed}"
            );
        }
        let mut identity = q3();
        identity.identity = Some("work-q3".into());
        assert_eq!(
            repository_argument(Some(&identity), Some("sub/../x.md"))
                .unwrap()
                .as_deref(),
            Some("x.md")
        );
        assert_eq!(
            repository_argument(Some(&identity), Some(".")).unwrap(),
            None
        );
    }

    // issue 211, D-134: with no folder a path holding no dot segment
    // stands as typed, one holding one is resolved, and one climbing above
    // the repository's root is refused before any request.
    #[test]
    fn repository_argument_resolves_dot_segments_with_no_folder() {
        for (typed, mapped) in [
            ("x.md", Some("x.md")),
            ("a//b/", Some("a//b/")),
            ("./x.md", Some("x.md")),
            ("a/./b", Some("a/b")),
            ("/a/../b", Some("b")),
            ("3/../x", Some("x")),
            (".", None),
            ("a/..", None),
        ] {
            assert_eq!(
                repository_argument(None, Some(typed)).unwrap().as_deref(),
                mapped,
                "{typed}"
            );
        }
        for typed in [
            "..",
            "../x",
            "../versions/3",
            "a/../../x",
            "../../../bob/other/files/x",
        ] {
            let err = repository_argument(None, Some(typed)).unwrap_err();
            assert_eq!(
                err.to_string(),
                format!(
                    "configuration error: {typed} names no path inside the repository; name one counted from its root"
                )
            );
            assert_eq!(err.exit_code(), 1);
        }
    }

    // issue 211 review: a file verb's `.` names no file, inside a folder
    // or with none, while a dot segment naming a file resolves.
    #[test]
    fn file_argument_refuses_a_value_resolving_to_the_scope_itself() {
        let folder = q3();
        let mut identity = q3();
        identity.identity = Some("work-q3".into());
        for typed in [".", "./", "a/.."] {
            for scope in [Some(&folder), Some(&identity), None] {
                let err = file_argument(scope, typed).unwrap_err();
                assert!(matches!(err, CliError::Config { .. }), "{typed}: {err:?}");
            }
        }
        assert_eq!(
            file_argument(Some(&folder), ".").unwrap_err().to_string(),
            "configuration error: . names the folder /w/clients/q3 itself; name a file counted from it"
        );
        assert_eq!(
            file_argument(Some(&folder), "sub/../x.md").unwrap(),
            "clients/q3/x.md"
        );
        assert_eq!(file_argument(Some(&identity), "./x.md").unwrap(), "x.md");
        assert_eq!(file_argument(None, "a/../x.md").unwrap(), "x.md");
        assert_eq!(file_argument(None, "x.md").unwrap(), "x.md");
        assert!(file_argument(None, "../x.md").is_err());
        assert!(file_argument(Some(&folder), "../x.md").is_err());
    }

    // SPEC u302 Behaviour, `resolve_read_target` 1: inside a folder bound
    // to its identity nothing typed names the whole tree, a typed path
    // stands as typed, and a served path is counted as served.
    #[test]
    fn paths_inside_an_identity_folder_stand_as_typed_and_served() {
        let mut folder = q3();
        folder.identity = Some("work-q3".into());
        assert_eq!(repository_argument(Some(&folder), None).unwrap(), None);
        assert_eq!(
            repository_argument(Some(&folder), Some("notes/a.md"))
                .unwrap()
                .as_deref(),
            Some("notes/a.md")
        );
        assert!(repository_argument(Some(&folder), Some("../x")).is_err());
        assert_eq!(counted_from(Some(&folder), "notes/a.md"), "notes/a.md");
        assert_eq!(
            counted_from(Some(&q3()), "clients/q3/notes/a.md"),
            "notes/a.md"
        );
    }

    // SPEC u297 Tests, `repository_argument_refuses_an_empty_value_inside_a_folder_alone`.
    #[test]
    fn repository_argument_refuses_an_empty_value_inside_a_folder_alone() {
        let folder = q3();
        for typed in ["", "  ", "\u{FEFF}"] {
            let err = repository_argument(Some(&folder), Some(typed)).unwrap_err();
            assert!(
                matches!(&err, CliError::Config { message } if message == "path cannot be empty"),
                "{typed:?} answered {err:?}"
            );
            assert_eq!(err.exit_code(), 1);
        }
        assert_eq!(
            repository_argument(Some(&folder), None).unwrap().as_deref(),
            Some("clients/q3")
        );
        assert_eq!(
            repository_argument(Some(&folder), Some("a.md"))
                .unwrap()
                .as_deref(),
            Some("clients/q3/a.md")
        );
        assert_eq!(
            repository_argument(None, Some("")).unwrap().as_deref(),
            Some("")
        );
    }

    // SPEC u297 Contract Surface, `refuse_empty_path`: the set
    // ECMAScript's `String.prototype.trim` takes off, never `U+0085`.
    #[test]
    fn refuse_empty_path_takes_off_the_file_path_field_kind_set() {
        let every_trimmed = "\u{9}\u{A}\u{B}\u{C}\u{D}\u{20}\u{A0}\u{1680}\u{2000}\u{200A}\u{2028}\u{2029}\u{202F}\u{205F}\u{3000}\u{FEFF}";
        for typed in ["", "\u{3000}\t\u{FEFF}", every_trimmed] {
            let err = refuse_empty_path(typed).unwrap_err();
            assert_eq!(
                err.to_string(),
                "configuration error: path cannot be empty",
                "{typed:?}"
            );
        }
        for typed in ["\u{0085}", "a", " a ", "\u{FEFF}a"] {
            assert!(refuse_empty_path(typed).is_ok(), "{typed:?} was refused");
        }
    }
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
        assert!(refuse_reference_spelling("1").is_ok());
        assert!(refuse_reference_spelling("main").is_ok());
        assert!(refuse_reference_spelling(" ").is_ok());
        assert!(refuse_reference_spelling(&"b".repeat(40)).is_ok());
        assert!(refuse_reference_spelling("00").is_err());
        assert!(refuse_reference_spelling("000000").is_err());
        // SPEC u283, `refuse_reference_spelling`: the empty value.
        let empty = refuse_reference_spelling("").unwrap_err();
        assert_eq!(
            empty.to_string(),
            "configuration error: version cannot be empty"
        );
        assert_eq!(empty.exit_code(), 1);
    }

    const REPO_OPTION_LINE: &str = "cannot determine repo identity \u{2014} pass --repo OWNER/NAME, or run inside a directory at or below one holding .syns.yaml";

    // SPEC u283, `resolve_read_target` 1 and `resolve_repo_scope` 1: in
    // a directory no identity reaches, each resolver's refusal names
    // `--repo`, and no request is made.
    #[tokio::test]
    #[serial]
    async fn each_read_resolver_names_repo_where_no_identity_reaches() {
        let dir = tempfile::tempdir().unwrap();
        std::env::set_current_dir(dir.path()).unwrap();
        unsafe { std::env::set_var("SYNS_CONFIG_DIR", dir.path()) };
        let server = MockServer::start().await;
        let config = Config::new(Some(&server.uri())).unwrap();
        let output = Output::new(false);

        let read = resolve_read_target(&config, &output, &ReadOptions::default())
            .await
            .unwrap_err();
        let scope = resolve_repo_scope(&config, &output, &RepoScopeArgs::default())
            .await
            .unwrap_err();
        unsafe { std::env::remove_var("SYNS_CONFIG_DIR") };

        for err in [read, scope] {
            assert_eq!(err.to_string(), REPO_OPTION_LINE);
            assert_eq!(err.exit_code(), 2);
        }
        assert!(server.received_requests().await.unwrap().is_empty());
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
