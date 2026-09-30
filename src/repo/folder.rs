//! The scoped folder (SPEC u290): a folder whose identity file names its
//! holding repository and its path under it, read by every read verb
//! run inside it as the scope of that run (`D-100`, `D-101`).
//!
//! Every mapping between a path counted from the folder and a path in
//! the holder goes through `FolderScope::repository_path`,
//! `FolderScope::folder_path` and `lies_under`, so no caller slices a
//! path prefix of its own.

use std::path::{Component, Path, PathBuf};

use crate::commands::pull::is_repository_shape;
use crate::errors::CliError;
use crate::output::Output;
use crate::push::converge::check_server_path;
use crate::repo::if_repo::resolve_full_or_skip;
use crate::repo::syns_yaml::{IdentityForm, find_syns_yaml, read_identity_form};

/// One scoped folder: `dir` is the absolute folder holding the identity
/// file, `owner` and `name` the holder lower-cased, and `path` the
/// recorded path with no leading or trailing `/`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FolderScope {
    pub dir: PathBuf,
    pub owner: String,
    pub name: String,
    pub path: String,
}

/// Whether `path` lies under the folder `folder`: it begins with the
/// folder followed by `/`, so a sibling sharing the folder's name as a
/// prefix, and the folder itself, lie under it nowhere.
pub fn lies_under(path: &str, folder: &str) -> bool {
    path.len() > folder.len() && path.starts_with(folder) && path.as_bytes()[folder.len()] == b'/'
}

impl FolderScope {
    /// The holder as `OWNER/NAME`, lower-cased.
    pub fn holder(&self) -> String {
        format!("{}/{}", self.owner, self.name)
    }

    /// A path counted from the folder, as a path in the holder.
    pub fn repository_path(&self, relative: &str) -> String {
        if relative.is_empty() {
            self.path.clone()
        } else {
            format!("{}/{relative}", self.path)
        }
    }

    /// A path in the holder, counted from the folder, and none where it
    /// does not lie under the folder.
    pub fn folder_path(&self, repository: &str) -> Option<String> {
        lies_under(repository, &self.path).then(|| repository[self.path.len() + 1..].to_string())
    }

    /// Rewrites every path a diff's header lines name — `diff --git`,
    /// `---`, `+++`, `rename from`, `rename to` and `Binary files` —
    /// counted from the folder, `/dev/null` and the `a/` and `b/`
    /// prefixes kept, and a path outside the folder left as served. Only
    /// the lines ahead of each file's first hunk are headers, so a
    /// content line opening `--- ` is never rewritten.
    pub fn rebase_diff_headers(&self, diff: &str) -> String {
        let mut in_header = true;
        let mut out = String::with_capacity(diff.len());
        for (index, line) in diff.split('\n').enumerate() {
            if index > 0 {
                out.push('\n');
            }
            if let Some(rest) = line.strip_prefix("diff --git ") {
                in_header = true;
                out.push_str("diff --git ");
                out.push_str(&self.rebase_pair(rest, " "));
                continue;
            }
            if line.starts_with("@@") {
                in_header = false;
            }
            if !in_header {
                out.push_str(line);
                continue;
            }
            if let Some(rest) = line.strip_prefix("--- ") {
                out.push_str("--- ");
                out.push_str(&self.rebase_side(rest, "a/"));
            } else if let Some(rest) = line.strip_prefix("+++ ") {
                out.push_str("+++ ");
                out.push_str(&self.rebase_side(rest, "b/"));
            } else if let Some(rest) = line.strip_prefix("rename from ") {
                out.push_str("rename from ");
                out.push_str(&self.folder_path(rest).unwrap_or_else(|| rest.to_string()));
            } else if let Some(rest) = line.strip_prefix("rename to ") {
                out.push_str("rename to ");
                out.push_str(&self.folder_path(rest).unwrap_or_else(|| rest.to_string()));
            } else if let Some(rest) = line
                .strip_prefix("Binary files ")
                .and_then(|rest| rest.strip_suffix(" differ"))
            {
                out.push_str("Binary files ");
                out.push_str(&self.rebase_pair(rest, " and "));
                out.push_str(" differ");
            } else {
                out.push_str(line);
            }
        }
        out
    }

    /// One side of a header: `/dev/null` kept, and a path behind
    /// `prefix` counted from the folder where it lies under it.
    fn rebase_side(&self, side: &str, prefix: &str) -> String {
        if let Some(path) = side.strip_prefix(prefix)
            && let Some(counted) = self.folder_path(path)
        {
            return format!("{prefix}{counted}");
        }
        side.to_string()
    }

    fn side_maps(&self, side: &str, prefix: &str) -> bool {
        side == "/dev/null"
            || side
                .strip_prefix(prefix)
                .is_some_and(|path| lies_under(path, &self.path))
    }

    /// Two sides joined by `separator`, split where both sides map — a
    /// path may itself hold the separator — and left as served where no
    /// split does.
    fn rebase_pair(&self, rest: &str, separator: &str) -> String {
        let chosen = rest.match_indices(separator).map(|(at, _)| at).find(|&at| {
            self.side_maps(&rest[..at], "a/") && self.side_maps(&rest[at + separator.len()..], "b/")
        });
        let Some(at) = chosen else {
            return rest.to_string();
        };
        format!(
            "{}{separator}{}",
            self.rebase_side(&rest[..at], "a/"),
            self.rebase_side(&rest[at + separator.len()..], "b/")
        )
    }
}

/// Checks the folder form's two keys as `resolve_folder_scope` 2 takes
/// them: `holder` in the `OWNER/NAME` shape `D-025` fixes, and `path` an
/// `INV-30` path holding no empty, `.` or `..` segment.
fn check_folder_form(holder: &str, path: &str) -> Result<(), CliError> {
    if !is_repository_shape(holder) {
        return Err(CliError::Io {
            message: format!("invalid .syns.yaml: holder must name OWNER/NAME (got {holder})"),
        });
    }
    let refused = || CliError::Io {
        message: format!(
            "invalid .syns.yaml: path must name a folder of the holder with no leading or trailing / (got {path})"
        ),
    };
    if path.split('/').any(str::is_empty) {
        return Err(refused());
    }
    check_server_path(path).map_err(|_| refused())
}

/// The folder's place under `checkout`, its components joined by `/`.
fn place_under(dir: &Path, checkout: &Path) -> String {
    match dir.strip_prefix(checkout) {
        Ok(relative) => relative
            .components()
            .filter_map(|c| match c {
                Component::Normal(part) => Some(part.to_string_lossy().into_owned()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("/"),
        Err(_) => dir.display().to_string(),
    }
}

/// The scope a run standing at `start` reads (SPEC u290 Behaviour,
/// `resolve_folder_scope`): a scope only where the nearest identity file
/// at or above `start` is the folder form and its place agrees with its
/// record, none where that file is the root form or no file stands. It
/// sends nothing and writes nothing.
pub fn resolve_folder_scope(start: &Path) -> Result<Option<FolderScope>, CliError> {
    // 1 — the nearest identity file, a marked one by its local side.
    let Some(file) = find_syns_yaml(start) else {
        return Ok(None);
    };
    let (holder, path) = match read_identity_form(&file)? {
        IdentityForm::Root { .. } => return Ok(None),
        IdentityForm::Folder { holder, path } => (holder, path),
    };

    // 2 — the holder and the path, checked.
    check_folder_form(&holder, &path)?;
    let (owner, name) = holder
        .split_once('/')
        .expect("is_repository_shape admits one /");
    let (owner, name) = (owner.to_ascii_lowercase(), name.to_ascii_lowercase());
    let dir = file
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_else(|| start.to_path_buf());

    // 3 — the nearest root form above the folder, past every folder form.
    let mut above = dir.parent().and_then(find_syns_yaml);
    while let Some(candidate) = above {
        let candidate_dir = candidate
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_default();
        match read_identity_form(&candidate)? {
            IdentityForm::Folder { .. } => {
                above = candidate_dir.parent().and_then(find_syns_yaml);
            }
            IdentityForm::Root {
                owner: standing_owner,
                name: standing_name,
            } => {
                // 4 — a checkout of another repository.
                if !standing_owner.eq_ignore_ascii_case(&owner)
                    || !standing_name.eq_ignore_ascii_case(&name)
                {
                    return Err(CliError::FolderInAnotherCheckout {
                        dir,
                        holder: format!("{owner}/{name}"),
                        checkout: candidate_dir,
                        standing: format!("{standing_owner}/{standing_name}"),
                    });
                }
                // 5 — the holder's checkout: the place must be the record.
                let actual = place_under(&dir, &candidate_dir);
                if actual != path {
                    return Err(CliError::FolderMoved {
                        dir,
                        holder: format!("{owner}/{name}"),
                        recorded: path,
                        actual,
                        checkout: candidate_dir,
                    });
                }
                break;
            }
        }
    }

    // 6 — the scope at its recorded path.
    Ok(Some(FolderScope {
        dir,
        owner,
        name,
        path,
    }))
}

/// The holder and its scope inside a folder, and otherwise the pair
/// `resolve_full_or_skip` answers with no scope (SPEC u290 Behaviour,
/// `resolve_scoped_or_skip`). A misplaced or malformed folder never
/// reads as a skip.
pub fn resolve_scoped_or_skip(
    start: &Path,
    if_repo: bool,
    output: &Output,
) -> Result<Option<(String, String, Option<FolderScope>)>, CliError> {
    if let Some(scope) = resolve_folder_scope(start)? {
        return Ok(Some((scope.owner.clone(), scope.name.clone(), Some(scope))));
    }
    Ok(
        resolve_full_or_skip(None, start, if_repo, output)?
            .map(|(owner, name)| (owner, name, None)),
    )
}

/// Refuses a command that would change the holding repository inside a
/// scoped folder (SPEC u290 Behaviour, `refuse_holder_change`, `D-102`).
/// It sends nothing, reads no credential and raises no prompt.
pub fn refuse_holder_change(start: &Path, command: &str) -> Result<(), CliError> {
    match resolve_folder_scope(start)? {
        Some(scope) => Err(CliError::HolderActing {
            command: command.to_string(),
            holder: scope.holder(),
            dir: scope.dir,
        }),
        None => Ok(()),
    }
}

/// The run's working directory.
pub fn current_dir() -> Result<PathBuf, CliError> {
    std::env::current_dir().map_err(|e| CliError::Io {
        message: format!("could not determine current directory: {e}"),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn scope(path: &str) -> FolderScope {
        FolderScope {
            dir: PathBuf::from("/w/q3"),
            owner: "alice".into(),
            name: "work".into(),
            path: path.into(),
        }
    }

    /// `W` naming `alice/work`, the folder `W/clients/vela/q3-board`
    /// recording its own place, and an empty `sub` under it.
    fn checkout(folder_yaml: &str) -> (tempfile::TempDir, PathBuf) {
        let w = tempfile::tempdir().unwrap();
        let root = fs::canonicalize(w.path()).unwrap();
        fs::write(root.join(".syns.yaml"), "owner: alice\nname: work\n").unwrap();
        let folder = root.join("clients").join("vela").join("q3-board");
        fs::create_dir_all(folder.join("sub")).unwrap();
        fs::write(folder.join(".syns.yaml"), folder_yaml).unwrap();
        (w, root)
    }

    #[test]
    fn resolve_folder_scope_refuses_a_malformed_folder_form() {
        for yaml in [
            "holder: alice/work\npath: ../x\n",
            "holder: alice/work\npath: x/\n",
            "owner: alice\nname: work\nholder: alice/work\npath: x\n",
        ] {
            let (_w, root) = checkout(yaml);
            let start = root.join("clients/vela/q3-board/sub");
            match resolve_folder_scope(&start) {
                Err(CliError::Io { message }) => {
                    assert!(message.starts_with("invalid .syns.yaml: "), "{message}")
                }
                other => panic!("expected the malformed-file error for {yaml:?}, got {other:?}"),
            }
        }
    }

    #[test]
    fn resolve_folder_scope_reads_a_marked_folder_form_by_its_local_side() {
        let (_w, root) = checkout(
            "<<<<<<< local\nholder: alice/work\npath: clients/vela/q3-board\n=======\nowner: bob\nname: other\n>>>>>>> remote\n",
        );
        let scope = resolve_folder_scope(&root.join("clients/vela/q3-board/sub"))
            .unwrap()
            .expect("a scope");
        assert_eq!(scope.owner, "alice");
        assert_eq!(scope.name, "work");
        assert_eq!(scope.path, "clients/vela/q3-board");
        assert_eq!(scope.dir, root.join("clients/vela/q3-board"));
    }

    #[test]
    fn lies_under_matches_at_folder_boundaries() {
        assert!(lies_under("q3-plan/images/fig.png", "q3-plan"));
        assert!(!lies_under("q3-plan-old/document.html", "q3-plan"));
        assert!(!lies_under("q3-plan", "q3-plan"));
    }

    #[test]
    fn a_folder_passes_over_folder_forms_to_the_holder_checkout() {
        let (_w, root) = checkout("holder: Alice/Work\npath: clients/vela/q3-board\n");
        fs::write(
            root.join("clients").join(".syns.yaml"),
            "holder: alice/work\npath: clients\n",
        )
        .unwrap();
        let scope = resolve_folder_scope(&root.join("clients/vela/q3-board"))
            .unwrap()
            .expect("a scope");
        assert_eq!(scope.holder(), "alice/work");
        assert_eq!(resolve_folder_scope(&root).unwrap(), None);
    }

    #[test]
    fn repository_and_folder_paths_map_each_other() {
        let s = scope("clients/q3");
        assert_eq!(s.repository_path(""), "clients/q3");
        assert_eq!(s.repository_path("a/b.md"), "clients/q3/a/b.md");
        assert_eq!(
            s.folder_path("clients/q3/a/b.md").as_deref(),
            Some("a/b.md")
        );
        assert_eq!(s.folder_path("clients/q3"), None);
        assert_eq!(s.folder_path("clients/q3-old/a.md"), None);
    }

    #[test]
    fn rebase_diff_headers_counts_every_header_path_from_the_folder() {
        let s = scope("clients/q3");
        let diff = "diff --git a/clients/q3/a b.md b/clients/q3/a b.md\nindex 1..2 100644\n--- a/clients/q3/a b.md\n+++ b/clients/q3/a b.md\n@@ -1 +1 @@\n--- a/clients/q3/kept\n+new";
        assert_eq!(
            s.rebase_diff_headers(diff),
            "diff --git a/a b.md b/a b.md\nindex 1..2 100644\n--- a/a b.md\n+++ b/a b.md\n@@ -1 +1 @@\n--- a/clients/q3/kept\n+new"
        );
        let rename = "diff --git a/clients/q3/x.md b/clients/q3/y.md\nsimilarity index 100%\nrename from clients/q3/x.md\nrename to clients/q3/y.md\n";
        assert_eq!(
            s.rebase_diff_headers(rename),
            "diff --git a/x.md b/y.md\nsimilarity index 100%\nrename from x.md\nrename to y.md\n"
        );
        let binary = "diff --git a/clients/q3/i.png b/clients/q3/i.png\nnew file mode 100644\nBinary files /dev/null and b/clients/q3/i.png differ";
        assert_eq!(
            s.rebase_diff_headers(binary),
            "diff --git a/i.png b/i.png\nnew file mode 100644\nBinary files /dev/null and b/i.png differ"
        );
        let added = "--- /dev/null\n+++ b/clients/q3/n.md\n@@ -0,0 +1 @@\n+n";
        assert_eq!(
            s.rebase_diff_headers(added),
            "--- /dev/null\n+++ b/n.md\n@@ -0,0 +1 @@\n+n"
        );
    }
}
