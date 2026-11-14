//! `syns checkouts [OWNER/NAME]`: the checkouts on this machine whose
//! folder stands and still names its repository, read from the records
//! the working copies keep (`folder.json`) and from each folder's own
//! identity file. It sends no request, reads no credential, and creates,
//! locks and writes nothing.

use std::path::Path;

use serde::Serialize;

use crate::config::Config;
use crate::errors::CliError;
use crate::output::Output;
use crate::push::working_copy::{IN_ROOT_HOME, RecordedFolder, STATE_STAMP, recorded_folders};
use crate::repo::folder::resolve_folder_scope;
use crate::repo::syns_yaml::{IdentityForm, read_identity_form};

/// One checkout that stands: its repository, lower case, and its folder.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct CheckoutRow {
    pub path: String,
    pub repo: String,
}

/// The answer for every repository.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Listing {
    pub checkouts: Vec<CheckoutRow>,
}

/// The answer for one repository: its checkouts, the folder a new copy
/// goes to, and where it was where none stands now.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RepositoryListing {
    pub checkouts: Vec<CheckoutRow>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub default_path: Option<String>,
    pub repo: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub was_at: Option<String>,
}

/// The one rule for where a new copy of `owner/name` goes:
/// `<home>/Syns/<owner>/<name>`, lower case; none where no home folder can
/// be determined or it is not valid Unicode.
pub fn default_path(owner: &str, name: &str) -> Option<String> {
    let path = dirs::home_dir()?
        .join("Syns")
        .join(owner.to_ascii_lowercase())
        .join(name.to_ascii_lowercase());
    path.to_str().map(str::to_string)
}

/// A recorded path as it is printed: on Windows without the `\\?\` prefix
/// a canonical path carries.
fn shown(path: &Path) -> Option<String> {
    let text = path.to_str()?;
    if cfg!(windows) {
        if let Some(rest) = text.strip_prefix(r"\\?\UNC\") {
            return Some(format!(r"\\{rest}"));
        }
        if let Some(rest) = text.strip_prefix(r"\\?\") {
            return Some(rest.to_string());
        }
    }
    Some(text.to_string())
}

/// Whether the identity file in `dir` itself names `owner/name` as its
/// own: the root form with that owner and name, letter case aside; or the
/// folder form `resolve_folder_scope` binds to a shared folder's name
/// equal to it, with `dir` as the scope's own directory. A file carrying
/// collision markers is read by its local side.
fn names_itself(dir: &Path, owner: &str, name: &str) -> bool {
    let file = dir.join(".syns.yaml");
    if !file.is_file() {
        return false;
    }
    match read_identity_form(&file) {
        Ok(IdentityForm::Root {
            owner: named_owner,
            name: named,
        }) => named_owner.eq_ignore_ascii_case(owner) && named.eq_ignore_ascii_case(name),
        Ok(IdentityForm::Folder { .. }) => match resolve_folder_scope(dir) {
            Ok(Some(scope)) => {
                let own_dir = match (
                    std::fs::canonicalize(&scope.dir),
                    std::fs::canonicalize(dir),
                ) {
                    (Ok(scoped), Ok(at)) => scoped == at,
                    _ => false,
                };
                own_dir
                    && scope.owner.eq_ignore_ascii_case(owner)
                    && scope
                        .identity
                        .as_deref()
                        .is_some_and(|identity| identity.eq_ignore_ascii_case(name))
            }
            _ => false,
        },
        Err(_) => false,
    }
}

/// When a checkout was used last: its record's stamp, or the stamp of the
/// in-root home in its folder where that is later.
fn used_at(record: &RecordedFolder, dir: &Path) -> Option<u128> {
    let in_root = std::fs::read_to_string(dir.join(IN_ROOT_HOME).join(STATE_STAMP))
        .ok()
        .and_then(|text| text.trim().parse::<u128>().ok());
    record.stamp.max(in_root)
}

/// The checkouts that stand under `cache_root`, `repo`'s alone where one
/// is named: by repository, then the one used last first, a record with
/// no stamp last; one row per path. Beside them, every record read.
pub fn standing_checkouts(
    cache_root: &Path,
    repo: Option<(&str, &str)>,
) -> Result<(Vec<CheckoutRow>, Vec<RecordedFolder>), CliError> {
    let records = recorded_folders(cache_root, repo)?;
    let mut standing: Vec<(String, Option<u128>, String)> = Vec::new();
    for record in records.iter().filter(|record| !record.set_aside) {
        let Some(dir) = record.path.as_deref() else {
            continue;
        };
        if !dir.is_dir() || !names_itself(dir, &record.owner, &record.name) {
            continue;
        }
        let Some(path) = shown(dir) else {
            continue;
        };
        let repo = format!("{}/{}", record.owner, record.name);
        standing.push((repo, used_at(record, dir), path));
    }
    standing.sort_by(|a, b| {
        a.0.cmp(&b.0)
            .then_with(|| match (a.1, b.1) {
                (Some(x), Some(y)) => y.cmp(&x),
                (Some(_), None) => std::cmp::Ordering::Less,
                (None, Some(_)) => std::cmp::Ordering::Greater,
                (None, None) => std::cmp::Ordering::Equal,
            })
            .then_with(|| a.2.cmp(&b.2))
    });
    let mut rows: Vec<CheckoutRow> = Vec::new();
    for (repo, _, path) in standing {
        if !rows.iter().any(|row| row.path == path) {
            rows.push(CheckoutRow { path, repo });
        }
    }
    Ok((rows, records))
}

/// The answer for `owner/name` alone (lower case): its rows, the rule's
/// folder, and — where none stands — the path of the checkout record of
/// it used last, a set-aside's included.
pub fn repository_listing(
    cache_root: &Path,
    owner: &str,
    name: &str,
) -> Result<RepositoryListing, CliError> {
    let (owner, name) = (owner.to_ascii_lowercase(), name.to_ascii_lowercase());
    let (checkouts, records) = standing_checkouts(cache_root, Some((&owner, &name)))?;
    let was_at = if checkouts.is_empty() {
        records
            .iter()
            .filter(|record| record.checkout)
            .filter_map(|record| Some((record.stamp, shown(record.path.as_deref()?)?)))
            .max()
            .map(|(_, path)| path)
    } else {
        None
    };
    Ok(RepositoryListing {
        checkouts,
        default_path: default_path(&owner, &name),
        repo: format!("{owner}/{name}"),
        was_at,
    })
}

/// `syns checkouts [OWNER/NAME]`.
pub fn cmd_checkouts(
    config: &Config,
    output: &Output,
    repo: Option<String>,
) -> Result<(), CliError> {
    let root = config.working_copy_root();
    let rows_of = |rows: &[CheckoutRow]| -> Vec<Vec<String>> {
        rows.iter()
            .map(|row| vec![row.repo.clone(), row.path.clone()])
            .collect()
    };
    let Some(repo) = repo else {
        let (checkouts, _) = standing_checkouts(root, None)?;
        if output.is_json() {
            output.json(&Listing { checkouts });
        } else if checkouts.is_empty() {
            output.success("No checkouts on this machine.");
        } else {
            output.table(&["Name", "Path"], rows_of(&checkouts));
        }
        return Ok(());
    };
    let (owner, name) = repo.split_once('/').unwrap_or((repo.as_str(), ""));
    let listing = repository_listing(root, owner, name)?;
    if output.is_json() {
        output.json(&listing);
        return Ok(());
    }
    if !listing.checkouts.is_empty() {
        output.table(&["Name", "Path"], rows_of(&listing.checkouts));
        return Ok(());
    }
    let repo = &listing.repo;
    output.success(&format!("No checkout of {repo} on this machine."));
    if let Some(was_at) = &listing.was_at {
        eprintln!(
            "  It was at {was_at}; no checkout of it is there now. If you moved it, run syns status in the moved folder: it is found again with its state."
        );
    }
    if let Some(default_path) = &listing.default_path {
        eprintln!("  Get a copy with: syns pull {repo} {default_path}");
    }
    Ok(())
}
