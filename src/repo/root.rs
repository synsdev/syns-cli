//! The content root both `syns push` and `syns pull` resolve through.
//!
//! Before u255 each invocation took its own working directory as the
//! content root, so `syns push` run from `repo/a/b/` published `a/b/`
//! as if it were the whole repository — and the deletion set it
//! derived from the local record then named every path outside `a/b/`
//! (issue 119). Both invocations now walk up to the directory whose
//! `.syns.yaml` names the repository being addressed and take THAT as
//! the content root; a publication's path argument becomes a *scope*
//! inside that root rather than a root of its own.

use std::path::{Path, PathBuf};

use crate::errors::CliError;
use crate::repo::syns_yaml::find_repo_root_for;

/// Where a publication's paths are taken relative to, and what part of
/// the tree it is confined to (SPEC u255 § Contract Surface).
///
/// `prefix` is a `/`-separated path relative to `root` carrying no
/// leading or trailing separator — the subtree, or the single file,
/// the run is scoped to. `None` means the whole tree below `root`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContentScope {
    pub root: PathBuf,
    pub prefix: Option<String>,
}

/// Membership test for a scope prefix. The ONLY place a
/// repository-relative path is decided in or out of a scope — the
/// collector's two walks and `build_push_entries`' deletion loop all
/// route through here, so a scoped publication cannot disagree with
/// itself about what its scope holds.
///
/// A path equal to the prefix is inside it (the prefix may name a
/// single file); a path merely sharing a leading string is not
/// (`sub2/x` is outside the prefix `sub`).
pub fn path_within_prefix(path: &str, prefix: &str) -> bool {
    if prefix.is_empty() {
        return true;
    }
    path == prefix
        || (path.len() > prefix.len()
            && path.as_bytes()[prefix.len()] == b'/'
            && path.starts_with(prefix))
}

/// True where `path` is a strict ancestor directory of `prefix` — the
/// directories a scoped walk must still descend into to reach the
/// scope at all.
pub fn path_is_prefix_ancestor(path: &str, prefix: &str) -> bool {
    !path.is_empty()
        && prefix.len() > path.len()
        && prefix.as_bytes()[path.len()] == b'/'
        && prefix.starts_with(path)
}

fn absolutize(path: &Path) -> Result<PathBuf, CliError> {
    std::fs::canonicalize(path).map_err(|err| CliError::Io {
        message: format!("could not resolve path {}: {err}", path.display()),
    })
}

/// The absolute directory every walk a run makes starts at: the path
/// argument where one was given, the process working directory
/// otherwise.
///
/// THE one place a command turns its arguments into a starting point.
/// Every ancestor walk in this crate — the identity resolution, the
/// content root, both marker guards — climbs with `Path::parent`, and a
/// relative path climbs to the empty component and stops: `syns push .`
/// from `repo/sub/` never reaches `repo/.syns.yaml` and refuses with
/// `REPO_IDENTITY_UNKNOWN`, or, under `--if-repo`, exits `0` having
/// published nothing. The trigger's own worked example is that spelling,
/// so a call site that skips this function reintroduces the defect the
/// unit exists to close.
///
/// The working directory is read only where no path argument was given,
/// and by `std::path::absolute` where a relative one was: a run that
/// addresses an absolute path must not depend on the process working
/// directory resolving at all.
///
/// `std::path::absolute` rather than canonicalization, because a
/// retrieval's destination need not exist yet.
pub fn resolve_start_path(explicit: Option<&Path>) -> Result<PathBuf, CliError> {
    match explicit {
        Some(path) => std::path::absolute(path).map_err(|err| CliError::Io {
            message: format!("could not resolve path {}: {err}", path.display()),
        }),
        None => std::env::current_dir().map_err(|err| CliError::Io {
            message: format!("could not determine current directory: {err}"),
        }),
    }
}

/// The `/`-separated spelling of a relative path, or `None` where a
/// component is not valid UTF-8.
///
/// The ONE conversion in the tree: `push_scope` builds a prefix with
/// it and the collector keys every walked path with it, and the scope
/// contract holds only while the two agree. A normalisation added to
/// one copy alone — dropping a `.` component, folding case on a
/// case-insensitive filesystem — would make a scoped publication
/// collect nothing, and no test would fail.
pub fn to_forward_slash(path: &Path) -> Option<String> {
    let parts: Option<Vec<&str>> = path.components().map(|c| c.as_os_str().to_str()).collect();
    parts.map(|p| p.join("/"))
}

/// Resolve a publication's content root and scope (SPEC u255
/// § Behaviour, `push_scope`).
///
/// `explicit` is the command line's `PATH` argument, absent for a bare
/// `syns push`. `cwd` is the working directory the run started in.
/// `owner` / `name` are the repository the run has already resolved —
/// the walk stops at a `.syns.yaml` naming a *different* repository,
/// so `syns push --name other/repo` inside a checkout publishes the
/// directory it stands in rather than the enclosing tree.
///
/// A bare invocation never takes a prefix: two runs started anywhere
/// under one identity file return equal values, which is the whole
/// point of the unit.
pub fn push_scope(
    explicit: Option<&Path>,
    cwd: &Path,
    owner: &str,
    name: &str,
) -> Result<ContentScope, CliError> {
    // 1 — absolute form of the path the run addresses.
    let target = absolutize(explicit.unwrap_or(cwd))?;

    // 2 — the directory the ancestor walk starts at. A path argument
    // naming a file scopes the publication to that one file, and the
    // walk starts at its parent.
    let start = if target.is_dir() {
        target.clone()
    } else if target.is_file() {
        match target.parent() {
            Some(parent) => parent.to_path_buf(),
            None => {
                return Err(CliError::Io {
                    message: format!("could not resolve path {}", target.display()),
                });
            }
        }
    } else {
        return Err(CliError::Io {
            message: format!(
                "path is neither a directory nor a file: {}",
                target.display()
            ),
        });
    };

    // 3, 4 — the repository root, or the starting directory where the
    // walk found no identity file naming this repository.
    let root = match find_repo_root_for(&start, owner, name)? {
        Some(root) => root,
        None => {
            return Ok(ContentScope {
                root: start,
                prefix: None,
            });
        }
    };

    // 5 — the addressed path's position under the root. A bare
    // invocation carries no path argument and therefore no prefix,
    // whichever descendant of the root it started in.
    let prefix = match explicit {
        None => None,
        Some(_) => match target.strip_prefix(&root) {
            Ok(rel) => to_forward_slash(rel).filter(|p| !p.is_empty()),
            Err(_) => None,
        },
    };

    Ok(ContentScope { root, prefix })
}

/// Resolve a retrieval's one write root (SPEC u255 § Behaviour,
/// `pull_root`).
///
/// A retrieval's path argument names its *destination*, not a scope,
/// so it is taken as given — but in absolute form. `cmd_pull` runs the
/// nested-marker guard over what this returns, and that guard walks
/// ancestors: a relative destination climbs to the empty component and
/// stops, never reaching the identity file the run stands under, so
/// the retrieval writes a marker the same run with an absolute
/// destination would not. Without one the walk answers the repository
/// root, and the working directory where no identity file names this
/// repository.
pub fn pull_root(
    explicit: Option<&Path>,
    cwd: &Path,
    owner: &str,
    name: &str,
) -> Result<PathBuf, CliError> {
    if let Some(path) = explicit {
        return resolve_start_path(Some(path));
    }

    Ok(find_repo_root_for(cwd, owner, name)?.unwrap_or_else(|| cwd.to_path_buf()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    /// Restores the process working directory on drop.
    ///
    /// The restore target falls back to the crate root because the
    /// library test binary reaches this guard with the process working
    /// directory already inside a dropped `TempDir` — CON1-1 — so
    /// reading it is not something a test may rely on.
    struct CwdGuard(PathBuf);

    impl CwdGuard {
        fn enter(dir: &Path) -> Self {
            let previous = std::env::current_dir()
                .unwrap_or_else(|_| PathBuf::from(env!("CARGO_MANIFEST_DIR")));
            std::env::set_current_dir(dir).expect("set cwd");
            CwdGuard(previous)
        }
    }

    impl Drop for CwdGuard {
        fn drop(&mut self) {
            let _ = std::env::set_current_dir(&self.0);
        }
    }

    fn tree() -> (tempfile::TempDir, PathBuf, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(dir.path()).unwrap();
        let deep = root.join("a").join("b");
        fs::create_dir_all(&deep).unwrap();
        fs::write(root.join(".syns.yaml"), "owner: alice\nname: proj\n").unwrap();
        (dir, root, deep)
    }

    #[test]
    fn push_scope_takes_no_prefix_without_a_path_argument() {
        let (_guard, root, deep) = tree();

        let scope = push_scope(None, &deep, "alice", "proj").unwrap();

        assert_eq!(scope.root, root);
        assert_eq!(scope.prefix, None);
    }

    #[test]
    fn push_scope_takes_the_path_argument_position_as_its_prefix() {
        let (_guard, root, deep) = tree();

        let scope = push_scope(Some(&deep), &root, "alice", "proj").unwrap();

        assert_eq!(scope.root, root);
        assert_eq!(scope.prefix.as_deref(), Some("a/b"));
    }

    #[test]
    fn push_scope_takes_a_file_argument_as_a_one_path_prefix() {
        let (_guard, root, deep) = tree();
        let file = deep.join("nested.md");
        fs::write(&file, "x").unwrap();

        let scope = push_scope(Some(&file), &root, "alice", "proj").unwrap();

        assert_eq!(scope.root, root);
        assert_eq!(scope.prefix.as_deref(), Some("a/b/nested.md"));
    }

    #[test]
    fn push_scope_falls_back_to_the_starting_directory_for_another_repository() {
        let (_guard, _root, deep) = tree();

        let scope = push_scope(None, &deep, "bob", "other").unwrap();

        assert_eq!(scope.root, deep);
        assert_eq!(scope.prefix, None);
    }

    #[test]
    fn push_scope_takes_no_prefix_where_the_path_argument_is_the_root() {
        let (_guard, root, _deep) = tree();

        let scope = push_scope(Some(&root), &root, "alice", "proj").unwrap();

        assert_eq!(scope.root, root);
        assert_eq!(scope.prefix, None);
    }

    #[test]
    fn pull_root_answers_the_repository_root_without_a_path_argument() {
        let (_guard, root, deep) = tree();

        assert_eq!(pull_root(None, &deep, "alice", "proj").unwrap(), root);
    }

    #[test]
    #[serial_test::serial]
    fn resolve_start_path_makes_a_relative_argument_absolute() {
        let (_guard, root, deep) = tree();
        let _cwd = CwdGuard::enter(&deep);

        // The trigger's own spelling: `.` from a repository subdirectory.
        let answered = resolve_start_path(Some(Path::new("."))).unwrap();

        assert!(answered.is_absolute(), "answered {}", answered.display());
        assert_eq!(
            find_repo_root_for(&answered, "alice", "proj").unwrap(),
            Some(root),
            "the identity walk did not reach the repository above the argument"
        );
    }

    #[test]
    #[serial_test::serial]
    fn resolve_start_path_answers_the_working_directory_without_an_argument() {
        let (_guard, _root, deep) = tree();
        let _cwd = CwdGuard::enter(&deep);

        assert_eq!(resolve_start_path(None).unwrap(), deep);
    }

    #[test]
    #[serial_test::serial]
    fn pull_root_makes_a_relative_path_argument_absolute() {
        let (_guard, root, _deep) = tree();
        let _cwd = CwdGuard::enter(&root);

        let answered = pull_root(Some(Path::new("dest")), &root, "alice", "proj").unwrap();

        assert!(answered.is_absolute(), "answered {}", answered.display());
        assert!(
            answered.ends_with("dest"),
            "answered {}",
            answered.display()
        );
    }

    #[test]
    fn pull_root_returns_an_absolute_path_argument_as_given() {
        let (_guard, root, deep) = tree();

        assert_eq!(
            pull_root(Some(&deep), &root, "alice", "proj").unwrap(),
            deep
        );
    }

    #[test]
    fn path_within_prefix_rejects_a_sibling_sharing_a_leading_string() {
        assert!(path_within_prefix("sub/nested.md", "sub"));
        assert!(path_within_prefix("sub", "sub"));
        assert!(!path_within_prefix("sub2/nested.md", "sub"));
        assert!(!path_within_prefix("subbed", "sub"));
        assert!(!path_within_prefix("root-a.md", "sub"));
    }

    #[test]
    fn path_is_prefix_ancestor_admits_only_a_strict_ancestor_directory() {
        assert!(path_is_prefix_ancestor("a", "a/b"));
        assert!(path_is_prefix_ancestor("a/b", "a/b/c.md"));
        assert!(!path_is_prefix_ancestor("a/b", "a/b"));
        assert!(!path_is_prefix_ancestor("a2", "a/b"));
        assert!(!path_is_prefix_ancestor("", "a/b"));
    }
}
