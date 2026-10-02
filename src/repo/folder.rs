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
use crate::repo::syns_yaml::{IdentityForm, find_syns_yaml, folder_shared_as, read_identity_form};

/// One scoped folder: `dir` is the absolute folder holding the identity
/// file, `owner` and `name` the holder lower-cased, and `path` the
/// recorded path with no leading or trailing `/`.
///
/// SPEC u291: `checkout` is the absolute directory of the root-form
/// identity file naming the holder above the folder, none for a folder
/// with no checkout of its holder above it; `enclosing` is every folder
/// of the holder standing above it that `enclosing_folders` answers,
/// nearest first, each carrying no checkout and no enclosing folder of
/// its own.
///
/// SPEC u302: `identity` is the shared folder's identity name, lower-cased,
/// on a scope bound to that identity, and none on every scope bound to its
/// holder; `owner`, `name` and `path` name the holder and the recorded
/// path under either binding.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FolderScope {
    pub dir: PathBuf,
    pub owner: String,
    pub name: String,
    pub path: String,
    pub checkout: Option<PathBuf>,
    pub enclosing: Vec<FolderScope>,
    pub identity: Option<String>,
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

    /// The repository every request a run inside the folder sends
    /// addresses — the holder's root `.synsignore` read and the share
    /// routes aside (SPEC u302 Contract Surface, `FolderScope::address`):
    /// the identity under the holder's owner where one is bound, the
    /// holder otherwise.
    pub fn address(&self) -> String {
        match &self.identity {
            Some(identity) => format!("{}/{identity}", self.owner),
            None => self.holder(),
        }
    }

    /// A path counted from the folder as the addressed repository carries
    /// it: itself through an identity, the holder's path otherwise (SPEC
    /// u302 Contract Surface, `FolderScope::request_path`).
    pub fn request_path(&self, relative: &str) -> String {
        match &self.identity {
            Some(_) => relative.to_string(),
            None => self.repository_path(relative),
        }
    }

    /// A path the addressed repository serves, counted from the folder:
    /// itself through an identity, none for an empty one; otherwise what
    /// `folder_path` answers (SPEC u302 Contract Surface,
    /// `FolderScope::served_path`).
    pub fn served_path(&self, served: &str) -> Option<String> {
        match &self.identity {
            Some(_) => (!served.is_empty()).then(|| served.to_string()),
            None => self.folder_path(served),
        }
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
pub(crate) fn place_under(dir: &Path, checkout: &Path) -> String {
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

/// `place` joined under `under`, either of them empty standing for the
/// root it is counted from.
fn joined_under(under: &str, place: &str) -> String {
    match (under.is_empty(), place.is_empty()) {
        (true, _) => place.to_string(),
        (false, true) => under.to_string(),
        (false, false) => format!("{under}/{place}"),
    }
}

/// Each identity file standing above `dir`, nearest first, up to and
/// including the nearest one in the root form, each read as the form it
/// is written in, a marked one by its local side.
fn identity_files_above(dir: &Path) -> Result<Vec<(PathBuf, IdentityForm)>, CliError> {
    let mut found = Vec::new();
    let mut above = dir.parent().and_then(find_syns_yaml);
    while let Some(candidate) = above {
        let candidate_dir = candidate
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_default();
        let form = read_identity_form(&candidate)?;
        let root = matches!(form, IdentityForm::Root { .. });
        found.push((candidate_dir.clone(), form));
        if root {
            break;
        }
        above = candidate_dir.parent().and_then(find_syns_yaml);
    }
    Ok(found)
}

/// The place check for a folder of `holder` recording `path` and
/// standing, or about to stand, at `dir` (SPEC u291 Behaviour,
/// `folder_checkout`): against the holder's checkout above it, answering
/// that checkout's directory, and where no checkout stands above it
/// against the outermost folder form above it, answering none. It sends
/// nothing and writes nothing.
pub fn folder_checkout(dir: &Path, holder: &str, path: &str) -> Result<Option<PathBuf>, CliError> {
    // 1 — the nearest root form above, past every folder form.
    let above = identity_files_above(dir)?;
    let refuse_moved = |actual: String, checkout: PathBuf, back: PathBuf| CliError::FolderMoved {
        dir: dir.to_path_buf(),
        holder: holder.to_string(),
        recorded: path.to_string(),
        actual,
        checkout,
        back,
    };
    if let Some((checkout, IdentityForm::Root { owner, name })) = above.last() {
        // 2 — a checkout of another repository.
        let standing = format!("{owner}/{name}");
        if !standing.eq_ignore_ascii_case(holder) {
            return Err(CliError::FolderInAnotherCheckout {
                dir: dir.to_path_buf(),
                holder: holder.to_string(),
                checkout: checkout.clone(),
                standing,
            });
        }
        // 3 — the holder's checkout: the place must be the record.
        let actual = place_under(dir, checkout);
        if actual != path {
            return Err(refuse_moved(actual, checkout.clone(), checkout.join(path)));
        }
        return Ok(Some(checkout.clone()));
    }

    // 4 — no checkout above: the outermost folder form above fixes the
    // place, whatever it names and records.
    let Some((
        outer,
        IdentityForm::Folder {
            holder: outer_holder,
            path: outer_path,
        },
    )) = above.last()
    else {
        return Ok(None);
    };
    if !outer_holder.eq_ignore_ascii_case(holder) || !lies_under(path, outer_path) {
        return Err(CliError::FolderInAnotherCheckout {
            dir: dir.to_path_buf(),
            holder: holder.to_string(),
            checkout: outer.clone(),
            standing: format!("{outer_holder}'s {outer_path}"),
        });
    }
    let actual = joined_under(outer_path, &place_under(dir, outer));
    if actual != path {
        let counted = &path[outer_path.len() + 1..];
        return Err(refuse_moved(actual, outer.clone(), outer.join(counted)));
    }
    Ok(None)
}

/// Every folder of `holder` standing above `dir` below the nearest
/// root-form identity file, nearest first, whose recorded path `path`
/// lies under and whose place agrees with it (SPEC u291 Behaviour,
/// `enclosing_folders`). Each is answered at its directory and recorded
/// path, carrying no checkout and no enclosing folder of its own. It
/// sends nothing and writes nothing.
pub fn enclosing_folders(
    dir: &Path,
    holder: &str,
    path: &str,
) -> Result<Vec<FolderScope>, CliError> {
    let mut kept = Vec::new();
    for (form_dir, form) in identity_files_above(dir)? {
        let IdentityForm::Folder {
            holder: form_holder,
            path: form_path,
        } = form
        else {
            break;
        };
        if !form_holder.eq_ignore_ascii_case(holder) || !lies_under(path, &form_path) {
            continue;
        }
        if joined_under(&form_path, &place_under(dir, &form_dir)) != path {
            continue;
        }
        let Some((owner, name)) = holder.split_once('/') else {
            continue;
        };
        kept.push(FolderScope {
            dir: form_dir,
            owner: owner.to_ascii_lowercase(),
            name: name.to_ascii_lowercase(),
            path: form_path,
            checkout: None,
            enclosing: Vec::new(),
            identity: None,
        });
    }
    Ok(kept)
}

/// The scope a run standing at `start` reads (SPEC u290 Behaviour,
/// `resolve_folder_scope`): a scope only where the nearest identity file
/// at or above `start` is the folder form and its place agrees with its
/// record, none where that file is the root form or no file stands. It
/// sends nothing and writes nothing. SPEC u291: its steps 3 to 5 are
/// `folder_checkout`'s, whose answer the scope carries as `checkout`
/// beside the folders `enclosing_folders` answers.
pub fn resolve_folder_scope(start: &Path) -> Result<Option<FolderScope>, CliError> {
    // SPEC u302 `resolve_folder_scope` 1 to 4 — a folder bound to its
    // identity, every identity file beneath it read as content.
    if let Some(scope) = identity_scope(start)? {
        return Ok(Some(scope));
    }

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

    // 3 to 5 — the place check, and the folders enclosing this one.
    let holder = format!("{owner}/{name}");
    let checkout = folder_checkout(&dir, &holder, &path)?;
    let enclosing = enclosing_folders(&dir, &holder, &path)?;

    // 6 — the scope at its recorded path.
    Ok(Some(FolderScope {
        dir,
        owner,
        name,
        path,
        checkout,
        enclosing,
        identity: None,
    }))
}

/// How one identity file at or above a start reads (SPEC u302
/// `resolve_folder_scope` 1): the root form, the folder form, or neither,
/// a refused parse kept rather than raised.
enum Standing {
    Root,
    Folder,
    Neither,
}

/// Every identity file at or above `start` to the filesystem root,
/// nearest first, beside the form it reads as.
fn identity_files_from(start: &Path) -> Vec<(PathBuf, Standing)> {
    let mut found = Vec::new();
    let mut next = find_syns_yaml(start);
    while let Some(file) = next {
        let dir = file.parent().map(Path::to_path_buf).unwrap_or_default();
        let standing = match read_identity_form(&file) {
            Ok(IdentityForm::Root { .. }) => Standing::Root,
            Ok(IdentityForm::Folder { .. }) => Standing::Folder,
            Err(_) => Standing::Neither,
        };
        next = dir.parent().and_then(find_syns_yaml);
        found.push((dir, standing));
    }
    found
}

/// SPEC u302 `resolve_folder_scope` 2 to 4: the scope bound to the
/// identity the outermost folder form at or above `start` names, where no
/// root form and no file read as neither form stands above it; none
/// wherever u290's and u291's steps answer the start.
fn identity_scope(start: &Path) -> Result<Option<FolderScope>, CliError> {
    // 2 — the outermost folder form, nothing but folder forms above it.
    let files = identity_files_from(start);
    let Some(outermost) = files
        .iter()
        .rposition(|(_, standing)| matches!(standing, Standing::Folder))
    else {
        return Ok(None);
    };
    if files[outermost + 1..]
        .iter()
        .any(|(_, standing)| !matches!(standing, Standing::Folder))
    {
        return Ok(None);
    }
    let dir = files[outermost].0.clone();
    let Some(identity) = folder_shared_as(&dir)? else {
        return Ok(None);
    };

    // 3 — that folder's holder and path, checked.
    let IdentityForm::Folder { holder, path } = read_identity_form(&dir.join(".syns.yaml"))? else {
        return Ok(None);
    };
    check_folder_form(&holder, &path)?;
    let (owner, name) = holder
        .split_once('/')
        .expect("is_repository_shape admits one /");
    let owner = owner.to_ascii_lowercase();
    let identity = identity.to_ascii_lowercase();
    if !is_repository_shape(&format!("{owner}/{identity}")) {
        return Err(CliError::Io {
            message: format!(
                "invalid .syns.yaml: shared_as must name a repository under {owner} (got {identity})"
            ),
        });
    }

    // 4 — the scope bound to the identity.
    Ok(Some(FolderScope {
        dir,
        owner,
        name: name.to_ascii_lowercase(),
        path,
        checkout: None,
        enclosing: Vec::new(),
        identity: Some(identity),
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
        // SPEC u302 `resolve_scoped_or_skip` 1: the owner and the name the
        // scope's address joins.
        let name = scope.identity.clone().unwrap_or_else(|| scope.name.clone());
        return Ok(Some((scope.owner.clone(), name, Some(scope))));
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
            checkout: None,
            enclosing: Vec::new(),
            identity: None,
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

    /// A directory `U` holding no identity file above it, each
    /// `(place, yaml)` written as `U/{place}/.syns.yaml`.
    fn forms(files: &[(&str, &str)]) -> (tempfile::TempDir, PathBuf) {
        let u = tempfile::tempdir().unwrap();
        let root = fs::canonicalize(u.path()).unwrap();
        for (place, yaml) in files {
            let dir = root.join(place);
            fs::create_dir_all(&dir).unwrap();
            fs::write(dir.join(".syns.yaml"), yaml).unwrap();
        }
        (u, root)
    }

    // SPEC u291 Behaviour, `folder_checkout` 3: a folder in its holder's
    // checkout answers that checkout, and no folder encloses it.
    #[test]
    fn a_folder_in_its_holder_checkout_answers_the_checkout() {
        let (_w, root) = checkout("holder: alice/work\npath: clients/vela/q3-board\n");
        let scope = resolve_folder_scope(&root.join("clients/vela/q3-board/sub"))
            .unwrap()
            .expect("a scope");
        assert_eq!(scope.checkout, Some(root.clone()));
        assert!(scope.enclosing.is_empty());
    }

    // `folder_checkout` 4: with no checkout above, the outermost folder
    // form fixes the place, and a folder moved under it is told to move
    // back to its place under that folder.
    #[test]
    fn a_folder_moved_under_a_folder_checked_out_alone_is_told_its_place() {
        let (_u, u) = forms(&[
            ("q3", "holder: alice/work\npath: clients/vela/q3-board\n"),
            (
                "q3/old/inner",
                "holder: alice/work\npath: clients/vela/q3-board/inner\n",
            ),
        ]);
        match resolve_folder_scope(&u.join("q3/old/inner")) {
            Err(err @ CliError::FolderMoved { .. }) => {
                let CliError::FolderMoved {
                    back,
                    checkout,
                    actual,
                    ..
                } = &err
                else {
                    unreachable!()
                };
                assert_eq!(back, &u.join("q3/inner"));
                assert_eq!(checkout, &u.join("q3"));
                assert_eq!(actual, "clients/vela/q3-board/old/inner");
                assert!(
                    err.to_string().contains(&format!(
                        "move the folder back to {}",
                        u.join("q3/inner").display()
                    )),
                    "{err}"
                );
            }
            other => panic!("expected the moved refusal, got {other:?}"),
        }
        let scope = resolve_folder_scope(&u.join("q3")).unwrap().expect("q3");
        assert_eq!(scope.checkout, None);
        assert!(scope.enclosing.is_empty());
    }

    // `folder_checkout` 4: an outermost folder form naming another
    // repository is refused by its holder and its recorded path.
    #[test]
    fn a_folder_under_another_repositorys_folder_is_refused() {
        let (_u, u) = forms(&[
            ("x", "holder: bob/other\npath: x\n"),
            ("x/q3", "holder: alice/work\npath: clients/vela/q3-board\n"),
        ]);
        match resolve_folder_scope(&u.join("x/q3")) {
            Err(CliError::FolderInAnotherCheckout {
                standing, checkout, ..
            }) => {
                assert_eq!(standing, "bob/other's x");
                assert_eq!(checkout, u.join("x"));
            }
            other => panic!("expected the other-checkout refusal, got {other:?}"),
        }
    }

    // SPEC u291 Behaviour, `enclosing_folders` 1–2: nearest first, every
    // form naming another holder, recording a path the folder's does not
    // lie under, or standing where its record disagrees passed over.
    #[test]
    fn enclosing_folders_keeps_the_folders_whose_place_agrees_nearest_first() {
        let (_u, u) = forms(&[
            ("q3", "holder: alice/work\npath: clients/vela/q3-board\n"),
            ("q3/a", "holder: bob/other\npath: y\n"),
            (
                "q3/a/b",
                "holder: Alice/Work\npath: clients/vela/q3-board/a/b\n",
            ),
            ("q3/a/b/c", "holder: alice/work\npath: other/z\n"),
            (
                "q3/a/b/c/mid",
                "holder: alice/work\npath: clients/vela/q3-board/a/b/c/x\n",
            ),
            (
                "q3/a/b/c/mid/inner",
                "holder: alice/work\npath: clients/vela/q3-board/a/b/c/mid/inner\n",
            ),
        ]);
        let kept = enclosing_folders(
            &u.join("q3/a/b/c/mid/inner"),
            "alice/work",
            "clients/vela/q3-board/a/b/c/mid/inner",
        )
        .unwrap();
        let dirs: Vec<PathBuf> = kept.iter().map(|f| f.dir.clone()).collect();
        assert_eq!(dirs, vec![u.join("q3/a/b"), u.join("q3")]);
        assert_eq!(kept[0].path, "clients/vela/q3-board/a/b");
        assert_eq!(kept[0].holder(), "alice/work");
        assert!(
            kept.iter()
                .all(|f| f.checkout.is_none() && f.enclosing.is_empty())
        );

        let (_v, v) = forms(&[
            ("q3", "holder: alice/work\npath: clients/vela/q3-board\n"),
            (
                "q3/x/mid",
                "holder: alice/work\npath: clients/vela/q3-board/x\n",
            ),
            (
                "q3/x/mid/inner",
                "holder: alice/work\npath: clients/vela/q3-board/x/mid/inner\n",
            ),
        ]);
        let scope = resolve_folder_scope(&v.join("q3/x/mid/inner"))
            .unwrap()
            .expect("inner");
        let dirs: Vec<PathBuf> = scope.enclosing.iter().map(|f| f.dir.clone()).collect();
        assert_eq!(dirs, vec![v.join("q3")]);
    }

    const SHARED: &str = "holder: Alice/Docs\npath: q3-plan\nshared_as: Docs-Q3-Plan\n";

    // SPEC u302 `resolve_folder_scope` 2–4: a folder form carrying
    // `shared_as` with no identity file above it binds its identity,
    // lower-cased, from anywhere beneath it.
    #[test]
    fn a_shared_folder_alone_binds_its_identity() {
        let (_u, u) = forms(&[("q3-plan", SHARED)]);
        fs::create_dir_all(u.join("q3-plan/sub")).unwrap();
        let scope = resolve_folder_scope(&u.join("q3-plan/sub"))
            .unwrap()
            .expect("a scope");
        assert_eq!(scope.identity.as_deref(), Some("docs-q3-plan"));
        assert_eq!(scope.holder(), "alice/docs");
        assert_eq!(scope.address(), "alice/docs-q3-plan");
        assert_eq!(scope.path, "q3-plan");
        assert_eq!(scope.dir, u.join("q3-plan"));
        assert_eq!(scope.checkout, None);
        assert!(scope.enclosing.is_empty());
        let (owner, name, _) =
            resolve_scoped_or_skip(&u.join("q3-plan/sub"), false, &Output::new(true))
                .unwrap()
                .expect("bound");
        assert_eq!(format!("{owner}/{name}"), "alice/docs-q3-plan");
    }

    // `resolve_folder_scope` 2: a root form above and a folder form above
    // carrying no `shared_as` each leave the folder bound to its holder,
    // and a file read as neither form above leaves it to u290's steps.
    #[test]
    fn a_shared_folder_under_any_other_file_stays_bound_to_its_holder() {
        let (_w, w) = forms(&[
            ("", "owner: alice\nname: docs\n"),
            (
                "q3-plan",
                "holder: alice/docs\npath: q3-plan\nshared_as: docs-q3-plan\n",
            ),
        ]);
        let scope = resolve_folder_scope(&w.join("q3-plan"))
            .unwrap()
            .expect("w");
        assert_eq!(scope.identity, None);
        assert_eq!(scope.address(), "alice/docs");
        assert_eq!(scope.checkout, Some(w.clone()));

        let (_v, v) = forms(&[
            ("q3-plan", "holder: alice/docs\npath: q3-plan\n"),
            (
                "q3-plan/appendix",
                "holder: alice/docs\npath: q3-plan/appendix\nshared_as: docs-q3-plan-appendix\n",
            ),
        ]);
        let scope = resolve_folder_scope(&v.join("q3-plan/appendix"))
            .unwrap()
            .expect("v");
        assert_eq!(scope.identity, None);
        assert_eq!(scope.path, "q3-plan/appendix");

        // u290's steps answer a file read as neither form above with
        // their malformed-file refusal, no identity bound.
        let (_x, x) = forms(&[("", "not: [a, form\n"), ("q3-plan", SHARED)]);
        match resolve_folder_scope(&x.join("q3-plan")) {
            Err(CliError::Io { message }) => {
                assert!(message.starts_with("invalid .syns.yaml: "), "{message}")
            }
            other => panic!("expected u290's refusal, got {other:?}"),
        }
    }

    // `resolve_folder_scope` 2: under a folder bound to its identity, a
    // nested folder form — `shared_as` included — a root form and a file
    // read as neither form are content of that identity.
    #[test]
    fn identity_files_inside_an_identity_folder_read_as_content() {
        let (_u, u) = forms(&[
            ("q3-plan", SHARED),
            (
                "q3-plan/appendix",
                "holder: alice/docs\npath: q3-plan/appendix\nshared_as: docs-q3-plan-appendix\n",
            ),
            ("q3-plan/notes", "owner: bob\nname: notes\n"),
            ("q3-plan/broken", "not: [a, form\n"),
        ]);
        for place in ["q3-plan/appendix", "q3-plan/notes", "q3-plan/broken"] {
            let scope = resolve_folder_scope(&u.join(place))
                .unwrap()
                .unwrap_or_else(|| panic!("{place}"));
            assert_eq!(scope.address(), "alice/docs-q3-plan", "{place}");
            assert_eq!(scope.dir, u.join("q3-plan"), "{place}");
        }
    }

    // SPEC u302 Contract Surface, `FolderScope::request_path` and
    // `FolderScope::served_path`: through an identity every path stands
    // as counted from the folder, the holder mapping kept beside it.
    #[test]
    fn paths_through_an_identity_stand_as_counted_from_the_folder() {
        let mut s = scope("q3-plan");
        s.identity = Some("docs-q3-plan".into());
        assert_eq!(s.address(), "alice/docs-q3-plan");
        assert_eq!(s.holder(), "alice/work");
        assert_eq!(s.request_path(""), "");
        assert_eq!(s.request_path("a/b.md"), "a/b.md");
        assert_eq!(s.served_path("a/b.md").as_deref(), Some("a/b.md"));
        assert_eq!(s.served_path(""), None);
        assert_eq!(s.repository_path("a/b.md"), "q3-plan/a/b.md");
        let s = scope("q3-plan");
        assert_eq!(s.address(), "alice/work");
        assert_eq!(s.request_path("a.md"), "q3-plan/a.md");
        assert_eq!(s.served_path("q3-plan/a.md").as_deref(), Some("a.md"));
        assert_eq!(s.served_path("other/a.md"), None);
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
