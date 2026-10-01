//! `syns enable-checks [PATH]` (SPEC u293): one version of the holder
//! appending to a placed folder's top-level `checks` each command its
//! `template` mapping recorded and that list lacks, the same bytes written
//! to disk and laid over every base standing at the parent, and no check
//! run.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use crate::config::Config;
use crate::errors::CliError;
use crate::output::Output;
use crate::push::hash::blob_sha1;
use crate::push::working_copy::{WorkingCopy, folder_base};
use crate::repo::folder::{FolderScope, current_dir, resolve_folder_scope};
use crate::repo::syns_yaml::{declared_checks, enable_checks_text, template_origin};
use crate::write::{
    Changeset, ProvenanceOptions, WriteOptions, checkout_guard_refusal, classify_content,
    publish_changeset, resolve_write_target,
};

use super::place::{counted_from, placement_path};

const SYNS_YAML: &str = ".syns.yaml";

// ---- the lines this command writes ------------------------------------

/// The unplaced-folder refusal, `dir` absolute.
pub fn unplaced_folder_refusal(dir: &Path) -> CliError {
    CliError::Config {
        message: format!(
            "{} holds no folder placed from a template; syns enable-checks turns on only the checks syns place recorded",
            dir.display()
        ),
    }
}

/// The unwritten-checks refusal: the one raised once the version landed,
/// `file` and `folder` absolute.
pub fn unwritten_checks_refusal(
    path: &str,
    version: u32,
    holder: &str,
    sha: &str,
    file: &Path,
    reason: &str,
    folder: &Path,
) -> CliError {
    CliError::Io {
        message: format!(
            "turned on the checks of {path} as version {version} of {holder}, commit {sha}, but could not write {}: {reason} \u{2014} syns pull at {} retrieves it",
            file.display(),
            folder.display()
        ),
    }
}

/// The enable caption: the `message` of every push turning a folder's
/// checks on.
pub fn enable_caption(path: &str) -> String {
    format!("turn on the checks of {path}")
}

/// The enable report's line on the diagnostic stream.
pub fn turned_on_line(path: &str, holder: &str, version: u32, sha: &str) -> String {
    format!(
        "turned on the checks recorded in {path}/.syns.yaml for everyone working in {holder}: version {version}, commit {sha}"
    )
}

/// The enable report's line where no recorded check waited.
pub fn none_waits_line(path: &str) -> String {
    format!("no check recorded in {path}/.syns.yaml waits to be turned on")
}

fn invalid(reason: impl std::fmt::Display) -> CliError {
    CliError::Io {
        message: format!("invalid .syns.yaml: {reason}"),
    }
}

/// `cmd_enable_checks` 2: the directory `PATH` names, counted as
/// `cmd_place` counts its own, or the folder the run stands in where no
/// `PATH` stands.
fn named_dir(cwd: &Path, path: Option<&str>) -> Result<PathBuf, CliError> {
    match path {
        Some(path) => Ok(counted_from(cwd)?.dir.join(path)),
        None => match resolve_folder_scope(cwd)? {
            Some(scope) => Ok(scope.dir),
            None => Err(unplaced_folder_refusal(cwd)),
        },
    }
}

/// `cmd_enable_checks` 3: the placed folder standing at `dir` exactly.
fn placed_folder(dir: &Path) -> Result<FolderScope, CliError> {
    let refused = || unplaced_folder_refusal(dir);
    let scope = resolve_folder_scope(dir)?.ok_or_else(refused)?;
    let same = match (
        std::fs::canonicalize(&scope.dir),
        std::fs::canonicalize(dir),
    ) {
        (Ok(a), Ok(b)) => a == b,
        _ => false,
    };
    if !same {
        return Err(refused());
    }
    Ok(scope)
}

/// `syns enable-checks [PATH]` (SPEC u293 Behaviour, `cmd_enable_checks`):
/// runs no check, and either the holder's head gains exactly one version
/// changing the placed folder's `.syns.yaml` alone, its top-level `checks`
/// gaining every recorded command it lacked, the file on disk carrying
/// the same bytes, or no version is made and no file on disk changes, a
/// disk write failing after the version landed excepted.
pub async fn cmd_enable_checks(
    config: &Config,
    output: &Output,
    path: Option<String>,
) -> Result<(), CliError> {
    // 1 — the typed path.
    let path = path.as_deref().map(placement_path).transpose()?;

    // 2 — the directory it names.
    let cwd = current_dir()?;
    let dir = named_dir(&cwd, path.as_deref())?;

    // 3 — the placed folder, its recorded and its turned-on checks.
    let scope = placed_folder(&dir)?;
    let file = scope.dir.join(SYNS_YAML);
    let text = std::fs::read_to_string(&file).map_err(|err| CliError::Io {
        message: format!("could not read .syns.yaml: {err}"),
    })?;
    let origin = template_origin(&text)
        .map_err(invalid)?
        .ok_or_else(|| unplaced_folder_refusal(&dir))?;
    let standing = declared_checks(&text).map_err(invalid)?;
    let holder = scope.holder();

    // 4 — each recorded command the top-level list lacks.
    let mut waiting: Vec<String> = Vec::new();
    for command in &origin.checks {
        if !standing.contains(command) && !waiting.contains(command) {
            waiting.push(command.clone());
        }
    }
    if waiting.is_empty() {
        if output.is_json() {
            output.json(&serde_json::json!({
                "holder": holder,
                "path": scope.path,
                "enabled": [],
            }));
        } else {
            eprintln!("{}", none_waits_line(&scope.path));
        }
        return Ok(());
    }

    // 5 — the parent: the base the folder's files are read against.
    let cache = config.cache_dir();
    let parent = folder_base(cache, &scope)
        .and_then(|base| base.commit_sha().map(str::to_string))
        .filter(|sha| !sha.is_empty())
        .ok_or_else(|| CliError::Io {
            message: checkout_guard_refusal(&scope.dir, &holder),
        })?;

    // 6 — the write target from the folder's directory.
    let opts = WriteOptions {
        repo: None,
        parent: parent.clone(),
        message: None,
        provenance: ProvenanceOptions::default(),
    };
    let target = resolve_write_target(config, &scope.dir, &opts).await?;

    // 7 — the one publication.
    let turned_on = enable_checks_text(&text, &waiting).map_err(invalid)?;
    let content = classify_content(SYNS_YAML, turned_on.clone().into_bytes(), None)?;
    let changeset = Changeset {
        files: vec![(SYNS_YAML.to_string(), content)],
        deletions: Vec::new(),
    };
    let (response, raw, claimed) = publish_changeset(
        config,
        &target,
        changeset,
        &opts,
        &enable_caption(&scope.path),
    )
    .await?;

    // 8 — the same bytes on disk.
    std::fs::write(&file, &turned_on).map_err(|err| {
        unwritten_checks_refusal(
            &scope.path,
            response.version,
            &holder,
            &response.commit_sha,
            &file,
            &err.to_string(),
            &scope.dir,
        )
    })?;

    // 9 — every base standing at the parent laid at the landed commit.
    let sha = blob_sha1(turned_on.as_bytes());
    lay_turned_on(cache, &scope, &sha, &parent, &claimed, &response.commit_sha)?;

    // 10 — the document, or the report.
    if output.is_json() {
        let mut document = raw;
        if let Some(map) = document.as_object_mut() {
            map.insert("holder".into(), holder.into());
            map.insert("path".into(), scope.path.clone().into());
            map.insert("enabled".into(), waiting.into());
        }
        output.json(&document);
        return Ok(());
    }
    eprintln!(
        "{}",
        turned_on_line(&scope.path, &holder, response.version, &response.commit_sha)
    );
    for command in &waiting {
        println!("{command}");
    }
    Ok(())
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

/// `cmd_enable_checks` 9: the folder copy's base where its commit is the
/// parent step 5 read or the one the landing send claimed, stamped anew;
/// then the holder checkout's and each enclosing folder copy's where it
/// holds no resolution and no outbox and its commit is the claimed
/// parent, each keeping its recorded time. One copy's lock is held at a
/// time, every other base is left standing, and no state is created.
fn lay_turned_on(
    cache: &Path,
    scope: &FolderScope,
    sha: &str,
    parent: &str,
    claimed: &str,
    landed: &str,
) -> Result<(), CliError> {
    let files_of = |base: &crate::push::manifest::Manifest| -> HashMap<String, String> {
        base.file_paths()
            .filter_map(|p| base.file_sha(p).map(|s| (p.to_string(), s.to_string())))
            .collect()
    };
    let identity_in_holder = scope.repository_path(SYNS_YAML);

    if let Some(copy) = WorkingCopy::open_existing_folder(cache, scope)? {
        let _lock = copy.lock().map_err(|err| base_refusal(&copy, err))?;
        if let Some(base) = copy.base()
            && matches!(base.commit_sha(), Some(c) if c == parent || c == claimed)
        {
            let mut files = files_of(&base);
            files.insert(SYNS_YAML.to_string(), sha.to_string());
            copy.record_base(landed, files)
                .map_err(|err| base_refusal(&copy, err))?;
        }
    }

    let mut targets: Vec<(WorkingCopy, Option<&FolderScope>)> = Vec::new();
    if let Some(checkout) = &scope.checkout
        && let Some(copy) = WorkingCopy::open_existing(cache, &scope.owner, &scope.name, checkout)?
    {
        targets.push((copy, None));
    }
    for enclosing in &scope.enclosing {
        if let Some(copy) = WorkingCopy::open_existing_folder(cache, enclosing)? {
            targets.push((copy, Some(enclosing)));
        }
    }
    for (copy, enclosing) in targets {
        let _lock = copy.lock().map_err(|err| base_refusal(&copy, err))?;
        if copy.resolution()?.is_some() || copy.outbox()?.is_some() {
            continue;
        }
        let Some(base) = copy.base() else {
            continue;
        };
        if base.commit_sha() != Some(claimed) {
            continue;
        }
        let at = match enclosing {
            None => Some(identity_in_holder.clone()),
            Some(enclosing) => enclosing.folder_path(&identity_in_holder),
        };
        let mut files = files_of(&base);
        if let Some(at) = at {
            files.insert(at, sha.to_string());
        }
        copy.record_laid_base(landed, files, base.recorded_at())
            .map_err(|err| base_refusal(&copy, err))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    type Recorded = (Option<String>, Vec<(String, String)>, Option<u64>);

    fn recorded(copy: &WorkingCopy) -> Recorded {
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

    fn standing(commit: &str, path: &str, sha: &str, at: Option<u64>) -> Recorded {
        (
            Some(commit.to_string()),
            vec![(path.to_string(), sha.to_string())],
            at,
        )
    }

    // CR1-2, CR2-1: `lay_turned_on` lays over no holder copy holding an
    // outbox or a resolution, none standing at another commit than the
    // claimed parent, and no folder copy standing at neither the parent
    // nor the claimed one; a holder copy it lays keeps its recorded time,
    // and the folder copy's is stamped anew.
    #[test]
    fn turning_checks_on_lays_only_over_clean_bases_at_the_parent() {
        let cache = tempfile::tempdir().unwrap();
        let w = tempfile::tempdir().unwrap();
        let w = std::fs::canonicalize(w.path()).unwrap();
        let folder = w.join("q3");
        std::fs::create_dir_all(&folder).unwrap();
        let scope = FolderScope {
            dir: folder.clone(),
            owner: "alice".into(),
            name: "work".into(),
            path: "q3".into(),
            checkout: Some(w.clone()),
            enclosing: Vec::new(),
        };
        let holder = WorkingCopy::open(cache.path(), "alice", "work", &w).unwrap();
        let held = HashMap::from([("q3/.syns.yaml".to_string(), "old".to_string())]);
        holder
            .record_laid_base("h3", held.clone(), Some(7))
            .unwrap();
        holder
            .write_outbox(&crate::push::working_copy::Outbox {
                parent_commit: Some("h3".to_string()),
                tree: Default::default(),
            })
            .unwrap();
        let own = WorkingCopy::open_folder(cache.path(), &scope).unwrap();
        let mine = HashMap::from([(".syns.yaml".to_string(), "old".to_string())]);
        own.record_laid_base("h1", mine, Some(5)).unwrap();

        lay_turned_on(cache.path(), &scope, "new", "h2", "h3", "h4").unwrap();

        assert_eq!(
            recorded(&holder),
            standing("h3", "q3/.syns.yaml", "old", Some(7)),
            "an outbox standing"
        );
        assert_eq!(recorded(&own), standing("h1", ".syns.yaml", "old", Some(5)));

        holder.remove_outbox().unwrap();
        holder
            .write_resolution(&crate::push::working_copy::Resolution {
                recovery_id: "r".into(),
                base_commit: Some("h3".into()),
                head_commit: "h0".into(),
                round: 1,
                local_paths: Vec::new(),
                remote_paths: Vec::new(),
                collisions: Vec::new(),
                combined_paths: Vec::new(),
                reviewed_tree: None,
                pending_writes: None,
            })
            .unwrap();
        lay_turned_on(cache.path(), &scope, "new", "h2", "h3", "h4").unwrap();
        assert_eq!(
            recorded(&holder),
            standing("h3", "q3/.syns.yaml", "old", Some(7)),
            "a resolution standing"
        );

        holder.remove_resolution().unwrap();
        holder.record_laid_base("h2", held, Some(7)).unwrap();
        lay_turned_on(cache.path(), &scope, "new", "h2", "h3", "h4").unwrap();
        assert_eq!(recorded(&holder).0.as_deref(), Some("h2"));

        lay_turned_on(cache.path(), &scope, "new", "h1", "h2", "h4").unwrap();
        assert_eq!(
            recorded(&holder),
            standing("h4", "q3/.syns.yaml", "new", Some(7))
        );
        let (commit, files, at) = recorded(&own);
        assert_eq!(
            (commit, files),
            (
                Some("h4".to_string()),
                vec![(".syns.yaml".to_string(), "new".to_string())]
            )
        );
        assert_ne!(at, Some(5), "the folder copy's record is stamped anew");
    }

    #[test]
    fn every_enable_line_is_written_whole() {
        assert_eq!(
            unplaced_folder_refusal(Path::new("/w/clients/q2")).to_string(),
            "configuration error: /w/clients/q2 holds no folder placed from a template; syns enable-checks turns on only the checks syns place recorded"
        );
        assert_eq!(
            unwritten_checks_refusal(
                "q3",
                44,
                "alice/work",
                "h5",
                Path::new("/w/q3/.syns.yaml"),
                "denied",
                Path::new("/w/q3")
            )
            .to_string(),
            "turned on the checks of q3 as version 44 of alice/work, commit h5, but could not write /w/q3/.syns.yaml: denied \u{2014} syns pull at /w/q3 retrieves it"
        );
        assert_eq!(enable_caption("q3"), "turn on the checks of q3");
        assert_eq!(
            turned_on_line("q3", "alice/work", 44, "h5"),
            "turned on the checks recorded in q3/.syns.yaml for everyone working in alice/work: version 44, commit h5"
        );
        assert_eq!(
            none_waits_line("q3"),
            "no check recorded in q3/.syns.yaml waits to be turned on"
        );
    }
}
