use std::path::{Path, PathBuf};

use serde::Deserialize;

use super::resolve::RepoIdentity;
use crate::errors::CliError;

const SYNS_YAML_FILENAME: &str = ".syns.yaml";

#[derive(Deserialize)]
struct SynsYaml {
    owner: String,
    name: String,
}

fn find_syns_yaml(path: &Path) -> Option<PathBuf> {
    let mut current = Some(path);
    while let Some(dir) = current {
        let candidate = dir.join(SYNS_YAML_FILENAME);
        if candidate.is_file() {
            return Some(candidate);
        }
        current = dir.parent();
    }
    None
}

fn parse_syns_yaml(file_path: &Path) -> Result<SynsYaml, CliError> {
    let contents = std::fs::read_to_string(file_path).map_err(|err| CliError::Io {
        message: format!("could not read .syns.yaml: {err}"),
    })?;

    serde_yaml::from_str(&contents).map_err(|err| CliError::Io {
        message: format!("invalid .syns.yaml: {err}"),
    })
}

pub fn read_syns_yaml(path: &Path) -> Result<Option<RepoIdentity>, CliError> {
    let file_path = match find_syns_yaml(path) {
        Some(p) => p,
        None => return Ok(None),
    };

    let yaml = parse_syns_yaml(&file_path)?;

    Ok(Some(RepoIdentity {
        owner: Some(yaml.owner),
        name: yaml.name,
    }))
}

/// The directory that owns the repository `owner/name` for a run
/// standing at `start` (SPEC u255 § Contract Surface).
///
/// Walks the ancestor chain `find_syns_yaml` already walks and keeps
/// the *directory* holding the nearest `.syns.yaml` rather than the
/// file. Answers `None` where the walk found no identity file at all,
/// and `None` where the nearest one names a different repository —
/// the caller then treats its own starting directory as the root
/// (`push_scope` 4 / `pull_root` 3), which is what makes
/// `syns push --name other/repo` inside someone else's checkout
/// publish the directory it stands in rather than the enclosing tree.
///
/// The pair comparison is ASCII-case-insensitive on both segments:
/// `resolve_repo_identity` lower-cases a `--name` value while a
/// `.syns.yaml` is returned verbatim, so an exact comparison would
/// miss a file carrying an upper-case character (SPEC_REVIEW CF-02).
///
/// Only the NEAREST identity file is consulted. A nested `.syns.yaml`
/// naming this same repository — the artefact an earlier accidental
/// subtree publication left behind — still wins over the outer root;
/// removing that file is the closure the trigger records.
pub fn find_repo_root_for(
    start: &Path,
    owner: &str,
    name: &str,
) -> Result<Option<PathBuf>, CliError> {
    let file_path = match find_syns_yaml(start) {
        Some(p) => p,
        None => return Ok(None),
    };

    let yaml = parse_syns_yaml(&file_path)?;

    if !yaml.owner.eq_ignore_ascii_case(owner) || !yaml.name.eq_ignore_ascii_case(name) {
        return Ok(None);
    }

    Ok(file_path.parent().map(Path::to_path_buf))
}

pub fn write_syns_yaml(path: &Path, owner: &str, name: &str) -> Result<(), CliError> {
    let file_path = path.join(SYNS_YAML_FILENAME);
    let content = format!("owner: {owner}\nname: {name}\n");
    std::fs::write(&file_path, content).map_err(|err| CliError::Io {
        message: format!("could not write .syns.yaml: {err}"),
    })?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn write_creates_valid_yaml() {
        let dir = tempfile::tempdir().unwrap();
        write_syns_yaml(dir.path(), "alice", "my-project").unwrap();

        let raw = fs::read_to_string(dir.path().join(".syns.yaml")).unwrap();
        assert_eq!(raw, "owner: alice\nname: my-project\n");
    }

    #[test]
    fn write_read_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        write_syns_yaml(dir.path(), "alice", "my-project").unwrap();

        let result = read_syns_yaml(dir.path());
        assert_eq!(
            result.unwrap(),
            Some(RepoIdentity {
                owner: Some("alice".into()),
                name: "my-project".into(),
            })
        );
    }

    #[test]
    fn read_walks_parent_directories() {
        let dir = tempfile::tempdir().unwrap();
        let sub = dir.path().join("sub1").join("sub2");
        fs::create_dir_all(&sub).unwrap();

        fs::write(
            dir.path().join(".syns.yaml"),
            "owner: carol\nname: parent-repo\n",
        )
        .unwrap();

        let result = read_syns_yaml(&sub);
        assert_eq!(
            result.unwrap(),
            Some(RepoIdentity {
                owner: Some("carol".into()),
                name: "parent-repo".into(),
            })
        );
    }

    #[test]
    fn read_returns_none_when_not_found() {
        let dir = tempfile::tempdir().unwrap();
        let result = read_syns_yaml(dir.path());
        assert_eq!(result.unwrap(), None);
    }

    #[test]
    fn read_returns_error_for_invalid_yaml() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join(".syns.yaml"), "not: valid: yaml: [[").unwrap();

        let result = read_syns_yaml(dir.path());
        assert!(result.is_err());
        assert!(matches!(result.unwrap_err(), CliError::Io { .. }));
    }

    #[test]
    fn read_returns_error_for_missing_fields() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join(".syns.yaml"), "owner: alice\n").unwrap();

        let result = read_syns_yaml(dir.path());
        assert!(result.is_err());
        assert!(matches!(result.unwrap_err(), CliError::Io { .. }));
    }

    #[test]
    fn read_tolerates_extra_fields() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(
            dir.path().join(".syns.yaml"),
            "owner: alice\nname: my-repo\nextra: ignored\n",
        )
        .unwrap();

        let result = read_syns_yaml(dir.path());
        assert_eq!(
            result.unwrap(),
            Some(RepoIdentity {
                owner: Some("alice".into()),
                name: "my-repo".into(),
            })
        );
    }

    #[test]
    fn find_repo_root_for_answers_none_where_the_file_names_another_repository() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let deep = root.join("a").join("b");
        fs::create_dir_all(&deep).unwrap();
        fs::write(root.join(".syns.yaml"), "owner: alice\nname: proj\n").unwrap();

        assert_eq!(find_repo_root_for(&deep, "bob", "other").unwrap(), None);
    }

    #[test]
    fn find_repo_root_for_matches_the_pair_case_insensitively() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let deep = root.join("a").join("b");
        fs::create_dir_all(&deep).unwrap();
        fs::write(root.join(".syns.yaml"), "owner: Alice\nname: Proj\n").unwrap();

        assert_eq!(
            find_repo_root_for(&deep, "alice", "proj").unwrap(),
            Some(root.to_path_buf())
        );
    }

    #[test]
    fn find_repo_root_for_answers_none_where_no_identity_file_stands() {
        let dir = tempfile::tempdir().unwrap();
        let deep = dir.path().join("a").join("b");
        fs::create_dir_all(&deep).unwrap();

        assert_eq!(find_repo_root_for(&deep, "alice", "proj").unwrap(), None);
    }

    #[test]
    fn write_overwrites_existing_file() {
        let dir = tempfile::tempdir().unwrap();
        write_syns_yaml(dir.path(), "alice", "first").unwrap();
        write_syns_yaml(dir.path(), "bob", "second").unwrap();

        let raw = fs::read_to_string(dir.path().join(".syns.yaml")).unwrap();
        assert_eq!(raw, "owner: bob\nname: second\n");
    }
}
