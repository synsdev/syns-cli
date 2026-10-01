use std::path::{Path, PathBuf};

use serde::Deserialize;

use super::resolve::RepoIdentity;
use crate::errors::CliError;

const SYNS_YAML_FILENAME: &str = ".syns.yaml";

#[derive(Deserialize)]
struct SynsYaml {
    owner: String,
    name: String,
    /// The commands a reviewed publication must pass (SPEC u256 Q-04),
    /// none where the file declares none. An older binary reads a file
    /// carrying them unchanged, extra keys being tolerated.
    #[serde(default)]
    checks: Vec<String>,
    /// The folder form's key (SPEC u290 Contract Surface, the folder
    /// form). Decoded only so a file carrying it beside `owner` and
    /// `name` is refused as malformed rather than read as the root form.
    #[serde(default)]
    holder: Option<serde_yaml::Value>,
}

/// The refusal a file mixing the two forms takes (SPEC u290 Contract
/// Surface, the folder form): `holder` beside `owner` or `name`.
const MIXED_FORMS: &str = "holder cannot stand beside owner or name";

/// The root form's parse of one identity file's text: the missing-field
/// error a folder form raises stands as it did, and a file carrying
/// `holder` beside `owner` and `name` is refused as mixing the forms.
fn parse_root_text(contents: &str) -> Result<SynsYaml, String> {
    let yaml: SynsYaml = serde_yaml::from_str(contents).map_err(|err| err.to_string())?;
    if yaml.holder.is_some() {
        return Err(MIXED_FORMS.to_string());
    }
    Ok(yaml)
}

/// One identity file read as the form it is written in (SPEC u290
/// Contract Surface, the folder form): the root form carrying `owner`
/// and `name`, any `path` key ignored, or the folder form carrying
/// `holder` and `path` as written. The `holder` key tells them apart.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IdentityForm {
    Root { owner: String, name: String },
    Folder { holder: String, path: String },
}

#[derive(Deserialize)]
struct FolderYaml {
    holder: String,
    path: String,
    /// The commands a reviewed publication continued inside the folder
    /// must pass (SPEC u291, the folder form's `checks`, `D-078`), none
    /// where the file declares none.
    #[serde(default)]
    checks: Vec<String>,
}

fn parse_form_text(contents: &str) -> Result<IdentityForm, String> {
    let value: serde_yaml::Value = serde_yaml::from_str(contents).map_err(|err| err.to_string())?;
    if value.get("holder").is_none() {
        let yaml = parse_root_text(contents)?;
        return Ok(IdentityForm::Root {
            owner: yaml.owner,
            name: yaml.name,
        });
    }
    if value.get("owner").is_some() || value.get("name").is_some() {
        return Err(MIXED_FORMS.to_string());
    }
    let folder: FolderYaml = serde_yaml::from_value(value).map_err(|err| err.to_string())?;
    Ok(IdentityForm::Folder {
        holder: folder.holder,
        path: folder.path,
    })
}

fn invalid(reason: impl std::fmt::Display) -> CliError {
    CliError::Io {
        message: format!("invalid .syns.yaml: {reason}"),
    }
}

fn read_contents(file_path: &Path) -> Result<String, CliError> {
    std::fs::read_to_string(file_path).map_err(|err| CliError::Io {
        message: format!("could not read .syns.yaml: {err}"),
    })
}

/// The identity file at `file_path` read as either form, raw and then by
/// its local side where it carries collision markers, as
/// `read_identity_by_local_side` reads it, the raw parse error raised
/// where both fail.
pub fn read_identity_form(file_path: &Path) -> Result<IdentityForm, CliError> {
    let contents = read_contents(file_path)?;
    match parse_form_text(&contents) {
        Ok(form) => Ok(form),
        Err(raw) => match local_side_of_collision(&contents) {
            Some(local) => parse_form_text(&local).map_err(|_| invalid(&raw)),
            None => Err(invalid(raw)),
        },
    }
}

/// Either form of one identity file's text (SPEC u291, `identity_form_text`):
/// the identity a `.syns.yaml` read from the server holds, a failure
/// refused as the malformed-file error.
pub(crate) fn identity_form_text(contents: &str) -> Result<IdentityForm, CliError> {
    parse_form_text(contents).map_err(invalid)
}

/// The required checks the identity file standing at `root` declares,
/// in either form, none where no identity file stands there or it
/// declares none (SPEC u291, the folder form's `checks`). A file mixing
/// the two forms is refused as it is everywhere.
pub fn read_required_checks(root: &Path) -> Result<Vec<String>, CliError> {
    let file_path = root.join(SYNS_YAML_FILENAME);
    if !file_path.is_file() {
        return Ok(Vec::new());
    }
    let contents = read_contents(&file_path)?;
    let value: serde_yaml::Value = serde_yaml::from_str(&contents).map_err(invalid)?;
    if value.get("holder").is_none() {
        return Ok(parse_root_text(&contents).map_err(invalid)?.checks);
    }
    if value.get("owner").is_some() || value.get("name").is_some() {
        return Err(invalid(MIXED_FORMS));
    }
    let folder: FolderYaml = serde_yaml::from_value(value).map_err(invalid)?;
    Ok(folder.checks)
}

pub(crate) fn find_syns_yaml(path: &Path) -> Option<PathBuf> {
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
    let contents = read_contents(file_path)?;
    parse_root_text(&contents).map_err(invalid)
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

/// The identity file standing nearest a starting directory (SPEC u263
/// § Contract Surface): `dir` is the absolute directory holding it, and
/// `owner` and `name` are spelt as that file spells them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StandingIdentity {
    pub dir: PathBuf,
    pub owner: String,
    pub name: String,
}

impl StandingIdentity {
    /// Whether this identity names `owner/name`, ASCII letter case aside
    /// on both segments (`D-025`).
    pub fn names(&self, owner: &str, name: &str) -> bool {
        self.owner.eq_ignore_ascii_case(owner) && self.name.eq_ignore_ascii_case(name)
    }
}

/// The identity file nearest `start`, `start` itself included, and none
/// where no identity file stands at or above it.
///
/// A file a convergence left carrying collision markers is read as the
/// side that stood there before the collision — the lines outside every
/// marked block and those of each block's local side — so a re-run over a
/// same-repository collision still reaches the convergence while one over
/// another repository's file is claimed by neither. The nearest file that
/// parses as no identity file either way raises the malformed-file error,
/// and no farther file is consulted.
pub fn nearest_identity(start: &Path) -> Result<Option<StandingIdentity>, CliError> {
    let Some(file_path) = find_syns_yaml(start) else {
        return Ok(None);
    };
    let yaml = read_identity_by_local_side(&file_path)?;
    Ok(Some(StandingIdentity {
        dir: file_path
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_else(|| start.to_path_buf()),
        owner: yaml.owner,
        name: yaml.name,
    }))
}

/// The identity file at `file_path`, parsed raw, then by its local side
/// where it carries collision markers, the raw parse error raised where
/// both fail.
fn read_identity_by_local_side(file_path: &Path) -> Result<SynsYaml, CliError> {
    match parse_syns_yaml(file_path) {
        Ok(yaml) => Ok(yaml),
        Err(err) => {
            let contents = std::fs::read_to_string(file_path).map_err(|e| CliError::Io {
                message: format!("could not read .syns.yaml: {e}"),
            })?;
            match local_side_of_collision(&contents) {
                Some(local) => parse_root_text(&local).map_err(|_| err),
                None => Err(err),
            }
        }
    }
}

/// The text a marked file held on its local side before the collision,
/// none where it carries no marked block.
fn local_side_of_collision(contents: &str) -> Option<String> {
    enum Side {
        Outside,
        Local,
        Other,
    }
    let mut side = Side::Outside;
    let mut marked = false;
    let mut local = String::new();
    for line in contents.lines() {
        match (&side, line) {
            (Side::Outside, l) if l.starts_with("<<<<<<< ") => {
                marked = true;
                side = Side::Local;
            }
            (Side::Local, l) if l.starts_with("||||||| ") || l == "=======" => side = Side::Other,
            (Side::Local | Side::Other, l) if l.starts_with(">>>>>>> ") => side = Side::Outside,
            (Side::Outside | Side::Local, l) => {
                local.push_str(l);
                local.push('\n');
            }
            (Side::Other, _) => {}
        }
    }
    marked.then_some(local)
}

pub fn write_syns_yaml(path: &Path, owner: &str, name: &str) -> Result<(), CliError> {
    let file_path = path.join(SYNS_YAML_FILENAME);
    let content = format!("owner: {owner}\nname: {name}\n");
    std::fs::write(&file_path, content).map_err(|err| CliError::Io {
        message: format!("could not write .syns.yaml: {err}"),
    })?;
    Ok(())
}

/// Write the identity file into `dir` only where the nearest identity file
/// at or above `dir`, read by its local side, does not name `owner/name`
/// and none stands in `dir` itself,
/// answering whether it wrote — the one guard every writer of the file
/// shares.
///
/// The walk-up clause keeps a run from `repo/sub/` from entrenching
/// `sub/` as a repository of its own (issue 119). The exact-directory
/// clause keeps a run naming another repository from overwriting the
/// file standing where it runs, and with it the checks that file
/// declares.
///
/// The exact-directory clause is tested first and reads no content: a
/// convergence colliding on `.syns.yaml` itself leaves that file carrying
/// markers for review, and parsing it here would replace the
/// convergence's own refusal with a malformed-file error (u262 V1-11).
pub fn write_syns_yaml_where_none_stands(
    dir: &Path,
    owner: &str,
    name: &str,
) -> Result<bool, CliError> {
    if dir.join(SYNS_YAML_FILENAME).exists() {
        return Ok(false);
    }
    if nearest_identity(dir)?.is_some_and(|standing| standing.names(owner, name)) {
        return Ok(false);
    }
    write_syns_yaml(dir, owner, name)?;
    Ok(true)
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
    fn read_required_checks_answers_the_declared_list_and_none_where_absent() {
        let dir = tempfile::tempdir().unwrap();
        assert!(read_required_checks(dir.path()).unwrap().is_empty());

        fs::write(dir.path().join(".syns.yaml"), "owner: alice\nname: proj\n").unwrap();
        assert!(read_required_checks(dir.path()).unwrap().is_empty());

        fs::write(
            dir.path().join(".syns.yaml"),
            "owner: alice\nname: proj\nchecks:\n  - make lint\n  - ./scripts/verify.sh --fast\n",
        )
        .unwrap();
        assert_eq!(
            read_required_checks(dir.path()).unwrap(),
            vec![
                "make lint".to_string(),
                "./scripts/verify.sh --fast".to_string()
            ]
        );
        assert_eq!(
            read_syns_yaml(dir.path()).unwrap().unwrap().name,
            "proj",
            "an identity file carrying checks still resolves"
        );
    }

    // SPEC u291, the folder form's `checks`: a folder form declaring
    // `checks` answers them in order, one declaring none answers none,
    // and a file mixing the forms is still refused.
    #[test]
    fn read_required_checks_answers_a_folder_forms_checks() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(
            dir.path().join(".syns.yaml"),
            "holder: alice/work\npath: clients/q3\nchecks:\n  - test -f a.md\n  - make lint\n",
        )
        .unwrap();
        assert_eq!(
            read_required_checks(dir.path()).unwrap(),
            vec!["test -f a.md".to_string(), "make lint".to_string()]
        );
        fs::write(
            dir.path().join(".syns.yaml"),
            "holder: alice/work\npath: clients/q3\n",
        )
        .unwrap();
        assert!(read_required_checks(dir.path()).unwrap().is_empty());
        fs::write(
            dir.path().join(".syns.yaml"),
            "owner: alice\nname: work\nholder: alice/work\npath: x\nchecks: [a]\n",
        )
        .unwrap();
        assert!(read_required_checks(dir.path()).is_err());
        assert_eq!(
            identity_form_text("holder: Alice/Work\npath: x\n").unwrap(),
            IdentityForm::Folder {
                holder: "Alice/Work".into(),
                path: "x".into()
            }
        );
        assert!(
            identity_form_text("holder: [")
                .unwrap_err()
                .to_string()
                .starts_with("invalid .syns.yaml: ")
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
    fn write_where_none_stands_refuses_both_a_standing_file_and_an_ancestor_naming_the_repository()
    {
        let dir = tempfile::tempdir().unwrap();
        let sub = dir.path().join("sub");
        fs::create_dir_all(&sub).unwrap();

        assert!(write_syns_yaml_where_none_stands(dir.path(), "alice", "proj").unwrap());
        assert!(!write_syns_yaml_where_none_stands(&sub, "alice", "proj").unwrap());
        assert!(!sub.join(".syns.yaml").exists());

        let standing = "owner: bob\nname: other\nchecks:\n  - make lint\n";
        fs::write(dir.path().join(".syns.yaml"), standing).unwrap();
        assert!(!write_syns_yaml_where_none_stands(dir.path(), "alice", "proj").unwrap());
        assert_eq!(
            fs::read_to_string(dir.path().join(".syns.yaml")).unwrap(),
            standing
        );
    }

    #[test]
    fn nearest_identity_reads_the_nearest_file_by_its_local_side() {
        let r = tempfile::tempdir().unwrap();
        let r = fs::canonicalize(r.path()).unwrap();
        let deep = r.join("sub").join("deep");
        fs::create_dir_all(&deep).unwrap();
        write_syns_yaml(&r, "bob", "other").unwrap();
        fs::write(
            r.join("sub").join(".syns.yaml"),
            "<<<<<<< local\nowner: alice\nname: proj\n=======\nowner: carol\nname: else\n>>>>>>> remote\n",
        )
        .unwrap();
        let v = tempfile::tempdir().unwrap();

        assert_eq!(
            nearest_identity(&deep).unwrap(),
            Some(StandingIdentity {
                dir: r.join("sub"),
                owner: "alice".into(),
                name: "proj".into(),
            })
        );
        assert_eq!(
            nearest_identity(&r).unwrap(),
            Some(StandingIdentity {
                dir: r.clone(),
                owner: "bob".into(),
                name: "other".into(),
            })
        );
        assert_eq!(nearest_identity(v.path()).unwrap(), None);
    }

    #[test]
    fn nearest_identity_reads_a_marked_file_as_its_local_side() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(
            dir.path().join(".syns.yaml"),
            "owner: bob\n<<<<<<< local\nname: other\n||||||| base\nname: base\n=======\nname: proj\n>>>>>>> remote\n",
        )
        .unwrap();
        let standing = nearest_identity(dir.path()).unwrap().unwrap();
        assert_eq!(
            (standing.owner.as_str(), standing.name.as_str()),
            ("bob", "other")
        );

        fs::write(
            dir.path().join(".syns.yaml"),
            "<<<<<<< local\nowner: bob\n=======\nowner: alice\n>>>>>>> remote\n",
        )
        .unwrap();
        assert!(nearest_identity(dir.path()).is_err());
    }

    #[test]
    fn nearest_identity_raises_the_malformed_file_error_without_consulting_a_farther_one() {
        let dir = tempfile::tempdir().unwrap();
        let sub = dir.path().join("sub");
        fs::create_dir_all(&sub).unwrap();
        write_syns_yaml(dir.path(), "alice", "proj").unwrap();
        fs::write(sub.join(".syns.yaml"), "owner: [alice").unwrap();

        match nearest_identity(&sub) {
            Err(CliError::Io { message }) => {
                assert!(message.starts_with("invalid .syns.yaml: "), "{message}")
            }
            other => panic!("expected the malformed-file error, got {other:?}"),
        }
    }

    #[test]
    fn standing_identity_names_its_pair_letter_case_aside() {
        let standing = StandingIdentity {
            dir: PathBuf::from("/w"),
            owner: "Alice".into(),
            name: "Notes".into(),
        };
        assert!(standing.names("alice", "notes"));
        assert!(!standing.names("alice", "other"));
        assert!(!standing.names("bob", "notes"));
    }

    #[test]
    fn write_where_none_stands_writes_nothing_under_a_marked_ancestor_naming_the_repository() {
        let dir = tempfile::tempdir().unwrap();
        let sub = dir.path().join("sub");
        fs::create_dir_all(&sub).unwrap();
        let marked = "<<<<<<< local\nowner: alice\nname: proj\n=======\nowner: alice\nname: proj\nchecks:\n  - make test\n>>>>>>> remote\n";
        fs::write(dir.path().join(".syns.yaml"), marked).unwrap();

        assert!(!write_syns_yaml_where_none_stands(&sub, "alice", "proj").unwrap());
        assert!(!sub.join(".syns.yaml").exists());
    }

    #[test]
    fn write_where_none_stands_leaves_a_marker_carrying_file_unread_and_unchanged() {
        let dir = tempfile::tempdir().unwrap();
        let markers = "<<<<<<< local\nowner: bob\nname: other\n||||||| base\n=======\nowner: alice\nname: proj\n>>>>>>> remote\n";
        fs::write(dir.path().join(".syns.yaml"), markers).unwrap();

        assert!(!write_syns_yaml_where_none_stands(dir.path(), "alice", "proj").unwrap());
        assert_eq!(
            fs::read_to_string(dir.path().join(".syns.yaml")).unwrap(),
            markers
        );
    }

    // SPEC u290 Contract Surface, the folder form: a file carrying
    // `holder` beside `owner` and `name` is malformed under every reader
    // of the identity file.
    #[test]
    fn a_file_mixing_both_forms_is_refused_by_every_reader() {
        let dir = tempfile::tempdir().unwrap();
        let sub = dir.path().join("sub");
        fs::create_dir_all(&sub).unwrap();
        fs::write(
            dir.path().join(".syns.yaml"),
            "owner: alice\nname: work\nholder: alice/work\npath: x\n",
        )
        .unwrap();

        let refused = |result: Result<(), CliError>| match result {
            Err(CliError::Io { message }) => {
                assert!(message.starts_with("invalid .syns.yaml: "), "{message}")
            }
            other => panic!("expected the malformed-file error, got {other:?}"),
        };
        refused(read_syns_yaml(&sub).map(|_| ()));
        refused(find_repo_root_for(&sub, "alice", "work").map(|_| ()));
        refused(read_required_checks(dir.path()).map(|_| ()));
        refused(nearest_identity(&sub).map(|_| ()));
        refused(read_identity_form(&dir.path().join(".syns.yaml")).map(|_| ()));
    }

    #[test]
    fn the_root_readers_keep_refusing_the_folder_form_as_missing_its_owner() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(
            dir.path().join(".syns.yaml"),
            "holder: alice/work\npath: x\n",
        )
        .unwrap();
        match nearest_identity(dir.path()) {
            Err(CliError::Io { message }) => {
                assert!(message.starts_with("invalid .syns.yaml: "), "{message}");
                assert!(message.contains("owner"), "{message}");
            }
            other => panic!("expected the malformed-file error, got {other:?}"),
        }
    }

    #[test]
    fn read_identity_form_tells_the_two_forms_apart() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join(".syns.yaml");
        fs::write(&file, "owner: Alice\nname: work\npath: ignored\n").unwrap();
        assert_eq!(
            read_identity_form(&file).unwrap(),
            IdentityForm::Root {
                owner: "Alice".into(),
                name: "work".into()
            }
        );

        fs::write(
            &file,
            "holder: alice/work\npath: clients/vela/q3-board\nextra: kept\n",
        )
        .unwrap();
        assert_eq!(
            read_identity_form(&file).unwrap(),
            IdentityForm::Folder {
                holder: "alice/work".into(),
                path: "clients/vela/q3-board".into()
            }
        );

        fs::write(
            &file,
            "<<<<<<< local\nholder: alice/work\npath: q3\n=======\nowner: bob\nname: other\n>>>>>>> remote\n",
        )
        .unwrap();
        assert_eq!(
            read_identity_form(&file).unwrap(),
            IdentityForm::Folder {
                holder: "alice/work".into(),
                path: "q3".into()
            }
        );

        fs::write(&file, "holder: alice/work\n").unwrap();
        assert!(matches!(
            read_identity_form(&file),
            Err(CliError::Io { .. })
        ));
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
