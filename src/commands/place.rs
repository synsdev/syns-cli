//! `syns place OWNER/NAME PATH [--version N]` (SPEC u293): one version of
//! the holder adding a new folder that carries a template's files byte for
//! byte beside a folder identity file recording the template, its version
//! and its checks not turned on, the same files written to disk where the
//! run stands, and every base of the copies over them laid at that
//! version.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use crate::auth::token::TokenStore;
use crate::client::{EntryType, PushRequest, SynsClient, undecodable};
use crate::config::Config;
use crate::errors::{CliError, IdentityRemedy};
use crate::output::Output;
use crate::push::converge::check_server_path;
use crate::push::folder_check::named_head;
use crate::push::hash::blob_sha1;
use crate::push::working_copy::{WorkingCopy, holds_in_root_home};
use crate::read::{refuse_reference_spelling, version_not_found_refusal};
use crate::repo::folder::{
    FolderScope, current_dir, folder_checkout, place_under, resolve_folder_scope,
};
use crate::repo::identity::identity_head;
use crate::repo::syns_yaml::{
    TemplateOrigin, declared_checks, folder_identity_text, nearest_identity,
};
use crate::write::{ProvenanceOptions, classify_content, push_file_entry, with_parent_sha};

const SYNS_YAML: &str = ".syns.yaml";

// ---- the lines this command writes ------------------------------------

/// The path refusal: a typed folder path `placement_path` refuses.
pub fn path_refusal(typed: &str) -> CliError {
    CliError::Config {
        message: format!(
            "the folder path must name a folder with no leading /, no empty segment, no ., .., .git or .syns-state segment and no control byte (got {typed})"
        ),
    }
}

/// The occupied-on-disk refusal: `entry`, counted from `dir`, is a file
/// standing where the new folder or a directory above it would stand.
pub fn occupied_on_disk_refusal(dir: &Path, entry: &str) -> CliError {
    CliError::Config {
        message: format!(
            "{} already holds {entry}; place the template into a folder holding no file",
            dir.display()
        ),
    }
}

/// The occupied-at-head refusal: the holder's commit `sha` holds `held`
/// at or above the new folder's path.
pub fn occupied_at_head_refusal(holder: &str, held: &str, sha: &str) -> CliError {
    CliError::Config {
        message: format!(
            "{holder} already holds {held} at commit {sha}; place the template into a folder its head does not hold"
        ),
    }
}

/// The empty-template refusal.
pub fn empty_template_refusal(template: &str) -> CliError {
    CliError::Config {
        message: format!("{template} holds no version to place"),
    }
}

/// The nested-identity refusal.
pub fn nested_identity_refusal(template: &str, version: u32, nested: &str) -> CliError {
    CliError::Config {
        message: format!(
            "{template} at version {version} holds an identity file at {nested}; only a template whose root alone carries one is placed"
        ),
    }
}

/// The too-large refusal: the template's tree arrived truncated.
pub fn too_large_refusal(template: &str, version: u32) -> CliError {
    CliError::Config {
        message: format!(
            "{template} at version {version} holds more entries than one tree answer carries; nothing placed"
        ),
    }
}

/// The not-readable refusal's line, rendered in place of the registered
/// not-found line on a `404` `not_found` whether the template is missing
/// or withheld (`INV-38`).
pub fn not_readable_line(template: &str) -> String {
    format!("not_found: {template} is no repository you can read")
}

/// The malformed-template refusal.
pub fn malformed_template_refusal(template: &str, version: u32, reason: &str) -> CliError {
    CliError::Io {
        message: format!("invalid .syns.yaml in {template} at version {version}: {reason}"),
    }
}

/// What a placement landed as, every line naming it reading it from here.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Landed {
    pub template: String,
    pub template_version: u32,
    pub path: String,
    pub holder: String,
    pub version: u32,
    pub sha: String,
}

/// The unwritten-folder refusal: the one raised once the version landed,
/// `dir` the absolute path whose creation or write failed and `checkout`
/// where a pull retrieves the folder.
pub fn unwritten_folder_refusal(
    landed: &Landed,
    dir: &Path,
    reason: &str,
    checkout: &Path,
) -> CliError {
    CliError::Io {
        message: format!(
            "placed {} into {} as version {} of {}, commit {}, but could not write {}: {reason} \u{2014} syns pull at {} retrieves it",
            landed.template,
            landed.path,
            landed.version,
            landed.holder,
            landed.sha,
            dir.display(),
            checkout.display()
        ),
    }
}

/// The placement caption: the `message` of every placement's push.
pub fn placement_caption(template: &str, template_version: u32, path: &str) -> String {
    format!("place {template} version {template_version} into {path}")
}

/// The placement report's first line.
pub fn placed_line(landed: &Landed) -> String {
    format!(
        "placed {} version {} into {} of {}: version {}, commit {}",
        landed.template,
        landed.template_version,
        landed.path,
        landed.holder,
        landed.version,
        landed.sha
    )
}

/// The label standing ahead of the recorded commands.
pub fn not_turned_on_line(path: &str) -> String {
    format!("checks recorded in {path}/.syns.yaml and not turned on, so none runs anywhere:")
}

/// The line standing after the recorded commands.
pub fn turn_on_line(holder: &str, enable: &str) -> String {
    format!("turn them on for everyone working in {holder} with: {enable}")
}

/// The line written where the template recorded no check.
pub fn no_checks_line(path: &str) -> String {
    format!("no checks recorded in {path}/.syns.yaml")
}

/// The enable command for `path` (SPEC u293 Contract Surface, the enable
/// command): `path` as given where it holds only ASCII letters, digits,
/// `.`, `_`, `-` and `/`, and otherwise single-quoted with each `'`
/// written `'\''`, so a POSIX shell reads it back as one word.
pub fn enable_command(path: &str) -> String {
    let plain = !path.is_empty()
        && path
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-' | b'/'));
    if plain {
        format!("syns enable-checks {path}")
    } else {
        format!("syns enable-checks '{}'", path.replace('\'', "'\\''"))
    }
}

/// A typed folder path, its trailing `/` removed, refused under the path
/// refusal where it is empty, opens with `/`, holds an empty, `.`, `..`
/// or `.git` segment, holds a byte `INV-30` bars (SPEC u293 Contract
/// Surface, `placement_path`), or holds the in-root home's segment,
/// letter case aside (SPEC u300, `placement_path`; issue 219).
pub fn placement_path(typed: &str) -> Result<String, CliError> {
    let trimmed = typed.trim_end_matches('/');
    if trimmed.is_empty()
        || trimmed.starts_with('/')
        || trimmed.split('/').any(str::is_empty)
        || holds_in_root_home(trimmed)
        || check_server_path(trimmed).is_err()
    {
        return Err(path_refusal(typed));
    }
    Ok(trimmed.to_string())
}

// ---- binding ----------------------------------------------------------

/// Where a run counts a typed folder path from (SPEC u293 Behaviour,
/// `cmd_place` 3 and 4): inside a scoped folder that folder, and
/// otherwise the directory of the nearest identity file.
#[derive(Debug, Clone)]
pub(crate) struct Counted {
    /// The holder, `OWNER/NAME` lower-cased.
    pub holder: String,
    /// The directory a typed path is counted from.
    pub dir: PathBuf,
    /// The folder the run stands in, where it stands in one.
    pub scope: Option<FolderScope>,
    /// The holder checkout, where one stands.
    pub checkout: Option<PathBuf>,
}

impl Counted {
    /// A typed path, counted from `dir`, as a path in the holder.
    pub fn repository_path(&self, path: &str) -> String {
        match &self.scope {
            Some(scope) => scope.repository_path(path),
            None => path.to_string(),
        }
    }
}

/// `cmd_place` 3 and 4 from `cwd`.
pub(crate) fn counted_from(cwd: &Path) -> Result<Counted, CliError> {
    if let Some(scope) = resolve_folder_scope(cwd)? {
        return Ok(Counted {
            holder: scope.holder(),
            dir: scope.dir.clone(),
            checkout: scope.checkout.clone(),
            scope: Some(scope),
        });
    }
    let identity = nearest_identity(cwd)?.ok_or(CliError::RepoIdentityUnknown {
        remedy: IdentityRemedy::IdentityFile,
    })?;
    Ok(Counted {
        holder: format!("{}/{}", identity.owner, identity.name).to_ascii_lowercase(),
        checkout: Some(identity.dir.clone()),
        dir: identity.dir,
        scope: None,
    })
}

/// `cmd_place` 6: a file standing at the place of any directory from
/// `counted` down to `path`, or at any depth under `path`.
fn refuse_occupied_on_disk(counted: &Path, path: &str) -> Result<(), CliError> {
    let mut place = String::new();
    for segment in path.split('/') {
        if !place.is_empty() {
            place.push('/');
        }
        place.push_str(segment);
        match std::fs::symlink_metadata(counted.join(&place)) {
            Ok(meta) if meta.is_dir() => {}
            Ok(_) => return Err(occupied_on_disk_refusal(counted, &place)),
            Err(_) => return Ok(()),
        }
    }
    let folder = counted.join(path);
    let mut held = Vec::new();
    files_under(&folder, "", &mut held);
    held.sort();
    match held.first() {
        Some(entry) => Err(occupied_on_disk_refusal(&folder, entry)),
        None => Ok(()),
    }
}

/// Every entry other than a directory at any depth under `dir`, each as
/// its `/`-joined place under it.
fn files_under(dir: &Path, prefix: &str, out: &mut Vec<String>) {
    for entry in std::fs::read_dir(dir).into_iter().flatten().flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        let place = if prefix.is_empty() {
            name
        } else {
            format!("{prefix}/{name}")
        };
        match entry.file_type() {
            Ok(kind) if kind.is_dir() => files_under(&entry.path(), &place, out),
            _ => out.push(place),
        }
    }
}

/// `cmd_place` 9 at `at`: the first directory the repository path passes
/// through, or the path itself, holding a file there, and otherwise the
/// path itself where its tree answers an entry; none where every read
/// answers `NOT_FOUND`.
async fn held_at(
    client: &SynsClient,
    holder: &str,
    token: &str,
    repository_path: &str,
    at: &str,
) -> Result<Option<String>, CliError> {
    let mut place = String::new();
    for segment in repository_path.split('/') {
        if !place.is_empty() {
            place.push('/');
        }
        place.push_str(segment);
        if client
            .raw_holds_file(holder, Some(token), &place, at)
            .await?
        {
            return Ok(Some(place));
        }
    }
    match client
        .get_tree(holder, Some(token), Some(repository_path), false, Some(at))
        .await
    {
        Ok((tree, _)) if tree.entries.is_empty() => Ok(None),
        Ok(_) => Ok(Some(repository_path.to_string())),
        Err(CliError::Api {
            status: Some(404),
            error,
            ..
        }) if error == "not_found" => Ok(None),
        Err(err) => Err(err),
    }
}

// ---- the command ------------------------------------------------------

/// One template file the placement carries: its path counted from the
/// template's root, its vouched bytes, and their hash.
struct Placed {
    path: String,
    bytes: Vec<u8>,
    sha: String,
}

/// `syns place OWNER/NAME PATH [--version N]` (SPEC u293 Behaviour,
/// `cmd_place`): either the holder's head gains exactly one version,
/// every path it changes lying under the placed folder, and the directory
/// `PATH` is counted from holds those files under `PATH`, or no version
/// is made and no file on disk changes, a disk write failing after the
/// version landed excepted.
pub async fn cmd_place(
    config: &Config,
    output: &Output,
    template: String,
    path: String,
    version: Option<String>,
) -> Result<(), CliError> {
    let template = template.to_ascii_lowercase();

    // 1 — the typed path.
    let path = placement_path(&path)?;

    // 2 — the version spelling.
    if let Some(version) = version.as_deref() {
        refuse_reference_spelling(version)?;
    }

    // 3 and 4 — the holder, the directory `PATH` is counted from, the
    // placed folder's repository path and the holder checkout.
    let cwd = current_dir()?;
    let counted = counted_from(&cwd)?;
    let holder = counted.holder.clone();
    let repository_path = counted.repository_path(&path);
    let folder_dir = counted.dir.join(&path);
    // SPEC u302 `cmd_place` 1: inside a folder bound to its identity, the
    // placement's commit goes through the identity at the typed path
    // counted from the folder, while the placed folder's identity file
    // records the holder and the holder path.
    let (address, request_path) = match &counted.scope {
        Some(scope) if scope.identity.is_some() => (scope.address(), scope.request_path(&path)),
        _ => (holder.clone(), repository_path.clone()),
    };

    // 5 — the place check for the new folder.
    folder_checkout(&folder_dir, &holder, &repository_path)?;

    // 6 — no file on disk where the folder would stand.
    refuse_occupied_on_disk(&counted.dir, &path)?;

    // 7 — the credential.
    let token = TokenStore::new(config.credentials_path())
        .read()?
        .ok_or(CliError::AuthRequired)?;
    let client = SynsClient::new(config.server_url())?;

    // 8 — the holder's head; through an identity, its newest listed
    // version.
    let head = if address == holder {
        client
            .get_repo(&holder, Some(&token))
            .await?
            .commit_sha
            .filter(|sha| !sha.is_empty())
    } else {
        identity_head(&client, Some(&token), &address)
            .await?
            .map(|newest| newest.sha)
    };

    // 9 — nothing the head holds at or above the folder's path.
    if let Some(head) = head.as_deref()
        && let Some(held) = held_at(&client, &address, &token, &request_path, head).await?
    {
        return Err(occupied_at_head_refusal(&address, &held, head));
    }

    // 10 — the template, refused as missing where it cannot be read.
    let template_repo = client
        .get_repo(&template, Some(&token))
        .await
        .map_err(|err| err.with_versioned_read_context(not_readable_line(&template)))?;

    // 11 — the version placed.
    let (template_version, template_sha) = match version.as_deref() {
        Some(typed) => {
            let (entry, _) = client
                .get_version(&template, Some(&token), typed)
                .await
                .map_err(|err| err.with_versioned_read_context(version_not_found_refusal(typed)))?;
            (entry.version, entry.sha)
        }
        None => {
            let sha = template_repo
                .commit_sha
                .filter(|sha| !sha.is_empty())
                .ok_or_else(|| empty_template_refusal(&template))?;
            let (page, _) = client
                .list_versions(&template, Some(&token), 1, 0, None)
                .await?;
            match page.data.first() {
                Some(entry) if entry.sha == sha => (entry.version, sha),
                _ => {
                    let (entry, _) = client.get_version(&template, Some(&token), &sha).await?;
                    (entry.version, sha)
                }
            }
        }
    };

    // 12 — the template's tree at that version, every served path checked.
    let (tree, _) = client
        .get_tree(&template, Some(&token), None, true, Some(&template_sha))
        .await?;
    if tree.truncated {
        return Err(too_large_refusal(&template, template_version));
    }
    let mut files: Vec<(String, String)> = tree
        .entries
        .into_iter()
        .filter(|entry| entry.entry_type == EntryType::File)
        .map(|entry| (entry.path, entry.sha.unwrap_or_default()))
        .collect();
    files.sort();
    for (served, _) in &files {
        check_server_path(served)?;
    }
    // SPEC u298 `IN_ROOT_HOME`: a template path in the in-root home is
    // neither written into the folder nor carried into the publication.
    files.retain(|(served, _)| !holds_in_root_home(served));
    if let Some((nested, _)) = files
        .iter()
        .find(|(served, _)| served.ends_with("/.syns.yaml"))
    {
        return Err(nested_identity_refusal(&template, template_version, nested));
    }

    // 13 — the checks the template's own identity file declares.
    let mut checks = Vec::new();
    if files.iter().any(|(served, _)| served == SYNS_YAML) {
        let raw = client
            .get_raw(
                &template,
                Some(&token),
                SYNS_YAML,
                Some(&template_sha),
                None,
            )
            .await?;
        let text = String::from_utf8(raw.bytes)
            .map_err(|e| malformed_template_refusal(&template, template_version, &e.to_string()))?;
        checks = declared_checks(&text)
            .map_err(|reason| malformed_template_refusal(&template, template_version, &reason))?;
    }

    // 14 — every other file's bytes, each vouched for by its tree entry.
    let mut placed = Vec::new();
    for (served, sha) in files.into_iter().filter(|(served, _)| served != SYNS_YAML) {
        let raw = client
            .get_raw(&template, Some(&token), &served, Some(&template_sha), None)
            .await?;
        let actual = blob_sha1(&raw.bytes);
        if actual != sha {
            return Err(undecodable(
                reqwest::StatusCode::OK,
                format!("{served}: expected {sha}, got {actual}"),
            ));
        }
        placed.push(Placed {
            path: served,
            bytes: raw.bytes,
            sha,
        });
    }

    // 15 — the one publication.
    let origin = TemplateOrigin {
        repo: template.clone(),
        version: template_version,
        sha: template_sha.clone(),
        checks,
    };
    let identity = folder_identity_text(&holder, &repository_path, &origin);
    let mut entries = Vec::with_capacity(placed.len() + 1);
    for file in &placed {
        let at = format!("{request_path}/{}", file.path);
        let content = classify_content(&at, file.bytes.clone(), None)?;
        entries.push(push_file_entry(at, content));
    }
    let identity_at = format!("{request_path}/{SYNS_YAML}");
    let identity_content = classify_content(&identity_at, identity.clone().into_bytes(), None)?;
    entries.push(push_file_entry(identity_at, identity_content));
    let mut request = PushRequest {
        files: entries,
        deletions: None,
        message: Some(placement_caption(
            &template,
            template_version,
            &repository_path,
        )),
        author: None,
        parent_sha: head.clone(),
        description: None,
        tags: None,
        status: None,
        visibility: None,
        provenance: ProvenanceOptions::default().block(),
    };
    let body = serialised(&request)?;
    let first = client.push_body(&address, &token, body.clone()).await;

    // 16 — a moved head re-read, and the same body sent once more there.
    let (response, raw, claimed) = match first {
        Ok((response, raw)) => (response, raw, head.clone()),
        Err(err) => {
            let Some(moved) = named_head(&err).map(str::to_string) else {
                return Err(err);
            };
            if let Some(held) = held_at(&client, &address, &token, &request_path, &moved).await? {
                return Err(occupied_at_head_refusal(&address, &held, &moved));
            }
            let again = match head {
                Some(_) => with_parent_sha(body, &moved),
                None => {
                    drop(body);
                    request.parent_sha = Some(moved.clone());
                    serialised(&request)?
                }
            };
            match client.push_body(&address, &token, again).await {
                Ok((response, raw)) => (response, raw, Some(moved)),
                Err(err) => match named_head(&err) {
                    Some(current) => {
                        return Err(CliError::WriteConflict {
                            parent: moved,
                            current_sha: current.to_string(),
                        });
                    }
                    None => return Err(err),
                },
            }
        }
    };
    drop(request);

    let landed = Landed {
        template: template.clone(),
        template_version,
        path: repository_path.clone(),
        holder: holder.clone(),
        version: response.version,
        sha: response.commit_sha.clone(),
    };

    // 17 — the same files written where the run counts the path from.
    let checkout = counted.checkout.clone().unwrap_or(counted.dir.clone());
    write_folder(&folder_dir, &placed, &identity)
        .map_err(|(dir, reason)| unwritten_folder_refusal(&landed, &dir, &reason, &checkout))?;

    // 18 — every base over the placed files laid at the placed commit.
    let identity_sha = blob_sha1(identity.as_bytes());
    let mut from_folder: HashMap<String, String> = placed
        .iter()
        .map(|file| (file.path.clone(), file.sha.clone()))
        .collect();
    from_folder.insert(SYNS_YAML.to_string(), identity_sha);
    record_bases(
        config,
        &counted,
        &folder_dir,
        &repository_path,
        &from_folder,
        claimed.as_deref(),
        &landed.sha,
    )?;

    // 19 — the document, or the report.
    let enable = enable_command(&path);
    if output.is_json() {
        let mut document = raw;
        if let Some(map) = document.as_object_mut() {
            map.insert("holder".into(), holder.clone().into());
            map.insert("path".into(), repository_path.clone().into());
            map.insert(
                "template".into(),
                serde_json::json!({
                    "repo": template,
                    "version": template_version,
                    "sha": template_sha,
                }),
            );
            map.insert("checks".into(), origin.checks.clone().into());
            map.insert(
                "enableChecks".into(),
                if origin.checks.is_empty() {
                    serde_json::Value::Null
                } else {
                    enable.clone().into()
                },
            );
        }
        output.json(&document);
        return Ok(());
    }
    eprintln!("{}", placed_line(&landed));
    if origin.checks.is_empty() {
        eprintln!("{}", no_checks_line(&repository_path));
    } else {
        eprintln!("{}", not_turned_on_line(&repository_path));
        for command in &origin.checks {
            println!("{command}");
        }
        eprintln!("{}", turn_on_line(&holder, &enable));
    }
    Ok(())
}

/// A push body serialised once, as the buffer it goes out in.
fn serialised(request: &PushRequest) -> Result<bytes::Bytes, CliError> {
    serde_json::to_vec(request)
        .map(bytes::Bytes::from)
        .map_err(|e| CliError::Io {
            message: format!("could not serialise a request body: {e}"),
        })
}

/// `cmd_place` 17: the folder's directory created, each placed file and
/// the identity file written at the bytes sent, the first failure
/// answered with the absolute path it failed at and every file already
/// written left standing.
fn write_folder(folder: &Path, placed: &[Placed], identity: &str) -> Result<(), (PathBuf, String)> {
    let failed = |at: &Path, err: std::io::Error| (at.to_path_buf(), err.to_string());
    std::fs::create_dir_all(folder).map_err(|e| failed(folder, e))?;
    for file in placed {
        let at = folder.join(&file.path);
        if let Some(parent) = at.parent() {
            std::fs::create_dir_all(parent).map_err(|e| failed(parent, e))?;
        }
        std::fs::write(&at, &file.bytes).map_err(|e| failed(&at, e))?;
    }
    let at = folder.join(SYNS_YAML);
    std::fs::write(&at, identity).map_err(|e| failed(&at, e))
}

/// The refusal a base write raises, naming the state directory.
fn base_refusal(copy: &WorkingCopy, err: CliError) -> CliError {
    CliError::Io {
        message: format!(
            "could not record the working copy base in {}: {err}",
            copy.state_dir.display()
        ),
    }
}

/// Lay `placed`, each path mapped by `counted` into the copy's own
/// paths, over the base `copy` records, where it holds no resolution and
/// no outbox and its commit is `claimed`, the commit becoming `landed`
/// and the record's time kept. One copy's lock is held at a time.
fn lay_placed(
    copy: &WorkingCopy,
    placed: &HashMap<String, String>,
    counted: impl Fn(&str) -> Option<String>,
    claimed: Option<&str>,
    landed: &str,
) -> Result<(), CliError> {
    let _lock = copy.lock().map_err(|err| base_refusal(copy, err))?;
    if copy.resolution()?.is_some() || copy.outbox()?.is_some() {
        return Ok(());
    }
    let Some(standing) = copy.base() else {
        return Ok(());
    };
    if claimed.is_none() || standing.commit_sha() != claimed {
        return Ok(());
    }
    let mut files: HashMap<String, String> = standing
        .file_paths()
        .filter_map(|path| {
            standing
                .file_sha(path)
                .map(|sha| (path.to_string(), sha.to_string()))
        })
        .collect();
    for (path, sha) in placed {
        if let Some(at) = counted(path) {
            files.insert(at, sha.clone());
        }
    }
    copy.record_laid_base(landed, files, standing.recorded_at())
        .map_err(|err| base_refusal(copy, err))
}

/// `cmd_place` 18: the holder checkout's base and each enclosing folder
/// copy's laid where it stood at the claimed parent, then the new
/// folder's own base recorded at the placed commit.
fn record_bases(
    config: &Config,
    counted: &Counted,
    folder_dir: &Path,
    repository_path: &str,
    from_folder: &HashMap<String, String>,
    claimed: Option<&str>,
    landed: &str,
) -> Result<(), CliError> {
    let cache = config.stores();
    let (owner, name) = counted
        .holder
        .split_once('/')
        .unwrap_or((counted.holder.as_str(), ""));
    let in_holder = |path: &str| format!("{repository_path}/{path}");

    // SPEC u302 `cmd_place` 1: inside a folder bound to its identity the
    // placed files are laid over the identity folder's copy at the typed
    // path, the placed folder recording no base of its own — it is a
    // directory of that identity.
    if let Some(scope) = counted.scope.as_ref().filter(|s| s.identity.is_some()) {
        if let Some(copy) = WorkingCopy::open_existing_folder(cache, scope)? {
            let place = place_under(folder_dir, &scope.dir);
            lay_placed(
                &copy,
                from_folder,
                |p| Some(format!("{place}/{p}")),
                claimed,
                landed,
            )?;
        }
        return Ok(());
    }

    if let Some(checkout) = &counted.checkout
        && let Some(copy) = WorkingCopy::open_existing(cache, owner, name, checkout)?
    {
        lay_placed(&copy, from_folder, |p| Some(in_holder(p)), claimed, landed)?;
    }

    let scope = resolve_folder_scope(folder_dir)?.ok_or_else(|| CliError::Io {
        message: format!(
            "could not read the folder identity file in {}",
            folder_dir.display()
        ),
    })?;
    for enclosing in &scope.enclosing {
        if let Some(copy) = WorkingCopy::open_existing_folder(cache, enclosing)? {
            lay_placed(
                &copy,
                from_folder,
                |p| enclosing.folder_path(&in_holder(p)),
                claimed,
                landed,
            )?;
        }
    }

    let copy = WorkingCopy::open_folder(cache, &scope)?;
    let _lock = copy.lock().map_err(|err| base_refusal(&copy, err))?;
    copy.record_base(landed, from_folder.clone())
        .map_err(|err| base_refusal(&copy, err))
}

#[cfg(test)]
mod tests {
    use super::*;

    // SPEC u293 Tests, the row of this name.
    #[test]
    fn a_typed_path_is_checked_and_quoted_for_the_enable_command() {
        assert_eq!(placement_path("a/b/").unwrap(), "a/b");
        for typed in [
            "", "/a", "a//b", "a/./b", "a/../b", "a/.git/b", "a/\tb", "/", "..",
        ] {
            match placement_path(typed) {
                Err(err) => assert_eq!(
                    err.to_string(),
                    format!(
                        "configuration error: the folder path must name a folder with no leading /, no empty segment, no ., .., .git or .syns-state segment and no control byte (got {typed})"
                    )
                ),
                Ok(path) => panic!("{typed:?} was admitted as {path:?}"),
            }
        }
        assert_eq!(enable_command("a/b"), "syns enable-checks a/b");
        assert_eq!(
            enable_command("q3 #2's"),
            "syns enable-checks 'q3 #2'\\''s'"
        );
    }

    // SPEC u300 Tests, the row of this name.
    #[test]
    fn a_placement_path_holding_the_state_home_is_refused() {
        for typed in [
            ".syns-state/planted",
            ".Syns-State/planted",
            "a/.SYNS-STATE",
            "a/.syns-state/",
        ] {
            match placement_path(typed) {
                Err(err) => assert_eq!(
                    err.to_string(),
                    format!(
                        "configuration error: the folder path must name a folder with no leading /, no empty segment, no ., .., .git or .syns-state segment and no control byte (got {typed})"
                    )
                ),
                Ok(path) => panic!("{typed:?} was admitted as {path:?}"),
            }
        }
        assert_eq!(placement_path("a/syns-state").unwrap(), "a/syns-state");
    }

    /// A working copy of `alice/work` at a directory of its own, its base
    /// recording `h1` over `README.md`.
    fn copy_at_h1() -> (tempfile::TempDir, tempfile::TempDir, WorkingCopy) {
        let cache = tempfile::tempdir().unwrap();
        let root = tempfile::tempdir().unwrap();
        let copy = WorkingCopy::open(
            &crate::config::StoreRoots::resolve(Some(cache.path()), cache.path(), cache.path()),
            "alice",
            "work",
            root.path(),
        )
        .unwrap();
        copy.record_laid_base(
            "h1",
            HashMap::from([("README.md".to_string(), "r1".to_string())]),
            Some(7),
        )
        .unwrap();
        (cache, root, copy)
    }

    fn recorded(copy: &WorkingCopy) -> (Option<String>, Vec<(String, String)>, Option<u64>) {
        let base = copy.base().expect("a base");
        let mut files: Vec<(String, String)> = base
            .file_paths()
            .map(|p| (p.to_string(), base.file_sha(p).unwrap().to_string()))
            .collect();
        files.sort();
        (
            base.commit_sha().map(str::to_string),
            files,
            base.recorded_at(),
        )
    }

    /// A review standing over `h1` and `h0`, prepared and not continued.
    fn a_resolution() -> crate::push::working_copy::Resolution {
        crate::push::working_copy::Resolution {
            recovery_id: "r".into(),
            base_commit: Some("h1".into()),
            head_commit: "h0".into(),
            round: 1,
            local_paths: Vec::new(),
            remote_paths: Vec::new(),
            collisions: Vec::new(),
            combined_paths: Vec::new(),
            reviewed_tree: None,
            pending_writes: None,
        }
    }

    // CR1-1: `lay_placed` lays over a base standing at the claimed
    // parent alone, and over none holding an outbox or a resolution.
    #[test]
    fn a_placement_lays_only_over_a_clean_base_at_the_claimed_parent() {
        let placed = HashMap::from([(".syns.yaml".to_string(), "y".to_string())]);
        let counted = |p: &str| Some(format!("q3/{p}"));
        let standing = (
            Some("h1".to_string()),
            vec![("README.md".to_string(), "r1".to_string())],
            Some(7),
        );

        let (_c, _r, copy) = copy_at_h1();
        copy.write_outbox(&crate::push::working_copy::Outbox {
            parent_commit: Some("h1".to_string()),
            tree: Default::default(),
        })
        .unwrap();
        lay_placed(&copy, &placed, counted, Some("h1"), "h2").unwrap();
        assert_eq!(recorded(&copy), standing);

        let (_c, _r, copy) = copy_at_h1();
        copy.write_resolution(&a_resolution()).unwrap();
        lay_placed(&copy, &placed, counted, Some("h1"), "h2").unwrap();
        assert_eq!(recorded(&copy), standing, "a resolution standing");

        for claimed in [Some("h0"), None] {
            let (_c, _r, copy) = copy_at_h1();
            lay_placed(&copy, &placed, counted, claimed, "h2").unwrap();
            assert_eq!(recorded(&copy), standing, "{claimed:?}");
        }

        let (_c, _r, copy) = copy_at_h1();
        lay_placed(&copy, &placed, counted, Some("h1"), "h2").unwrap();
        assert_eq!(
            recorded(&copy),
            (
                Some("h2".to_string()),
                vec![
                    ("README.md".to_string(), "r1".to_string()),
                    ("q3/.syns.yaml".to_string(), "y".to_string())
                ],
                Some(7)
            )
        );
    }

    fn landed() -> Landed {
        Landed {
            template: "bartsoj/syns-whiteboard-template".into(),
            template_version: 14,
            path: "clients/vela/q3-board".into(),
            holder: "alice/work".into(),
            version: 43,
            sha: "h2".into(),
        }
    }

    #[test]
    fn every_placement_line_is_written_whole() {
        let template = "bartsoj/syns-whiteboard-template";
        assert_eq!(
            occupied_on_disk_refusal(Path::new("/w"), "notes/a.md").to_string(),
            "configuration error: /w already holds notes/a.md; place the template into a folder holding no file"
        );
        assert_eq!(
            occupied_at_head_refusal("alice/work", "clients", "h1").to_string(),
            "configuration error: alice/work already holds clients at commit h1; place the template into a folder its head does not hold"
        );
        assert_eq!(
            empty_template_refusal(template).to_string(),
            "configuration error: bartsoj/syns-whiteboard-template holds no version to place"
        );
        assert_eq!(
            nested_identity_refusal(template, 14, ".page/.syns.yaml").to_string(),
            "configuration error: bartsoj/syns-whiteboard-template at version 14 holds an identity file at .page/.syns.yaml; only a template whose root alone carries one is placed"
        );
        assert_eq!(
            too_large_refusal(template, 14).to_string(),
            "configuration error: bartsoj/syns-whiteboard-template at version 14 holds more entries than one tree answer carries; nothing placed"
        );
        assert_eq!(
            not_readable_line(template),
            "not_found: bartsoj/syns-whiteboard-template is no repository you can read"
        );
        assert_eq!(
            malformed_template_refusal(template, 14, "bad").to_string(),
            "invalid .syns.yaml in bartsoj/syns-whiteboard-template at version 14: bad"
        );
        assert_eq!(
            unwritten_folder_refusal(&landed(), Path::new("/w/q3"), "denied", Path::new("/w"))
                .to_string(),
            "placed bartsoj/syns-whiteboard-template into clients/vela/q3-board as version 43 of alice/work, commit h2, but could not write /w/q3: denied \u{2014} syns pull at /w retrieves it"
        );
        assert_eq!(
            placement_caption(template, 14, "clients/vela/q3-board"),
            "place bartsoj/syns-whiteboard-template version 14 into clients/vela/q3-board"
        );
        assert_eq!(
            placed_line(&landed()),
            "placed bartsoj/syns-whiteboard-template version 14 into clients/vela/q3-board of alice/work: version 43, commit h2"
        );
        assert_eq!(
            not_turned_on_line("clients/vela/q3-board"),
            "checks recorded in clients/vela/q3-board/.syns.yaml and not turned on, so none runs anywhere:"
        );
        assert_eq!(
            turn_on_line("alice/work", "syns enable-checks clients/vela/q3-board"),
            "turn them on for everyone working in alice/work with: syns enable-checks clients/vela/q3-board"
        );
        assert_eq!(
            no_checks_line("clients/vela/q3-board"),
            "no checks recorded in clients/vela/q3-board/.syns.yaml"
        );
    }
}
