//! `syns mv FROM TO --parent REF` and `syns cp FROM TO --parent REF` —
//! one commit in which a file or a whole folder stands at `TO` (and, for
//! a move, no longer at `FROM`), sending the two paths and no byte of
//! content (roadmap 228, FEATURE_SPEC § Contract changes · CLI; D-253).
//!
//! Both take `syns write`'s options but `--bytes`, its identity ladder,
//! scoped-folder counting and folder check, checkout guard, `--parent`
//! resolution and refusals. Neither reads standard input, the tree or any
//! file: the body is `{ moves: [{ from, to }] }` (or `copies`), the
//! parent, the caption and any provenance. Every lexical refusal — an
//! empty path, the root or the folder itself, a path leaving either, and
//! a `TO` equal to, under or above `FROM` — is raised before any request.
//! `TO` is the new path itself: `syns mv a.md docs` never means
//! `docs/a.md`.

use crate::config::Config;
use crate::errors::CliError;
use crate::output::Output;
use crate::repo::folder::resolve_folder_scope;
use crate::write::{
    Changeset, Member, MemberKind, WriteOptions, commit_changeset, member_path,
    refuse_self_overlap, resolve_write_target,
};

/// The folder a run's typed paths are counted from, read without a
/// request: none beside `--repo`, which binds no folder, and the scoped
/// folder the working directory stands in otherwise — the folder
/// `resolve_write_target` binds.
pub fn lexical_folder(
    cwd: &std::path::Path,
    opts: &WriteOptions,
) -> Result<Option<crate::repo::folder::FolderScope>, CliError> {
    match opts.repo {
        Some(_) => Ok(None),
        None => resolve_folder_scope(cwd),
    }
}

/// The one member a run names, its paths resolved lexically and refused
/// where `TO` overlaps `FROM` (FEATURE_SPEC row 29), before any request.
pub fn member_for(
    kind: MemberKind,
    folder: Option<&crate::repo::folder::FolderScope>,
    from: &str,
    to: &str,
) -> Result<Member, CliError> {
    let member = Member {
        kind,
        from: member_path(folder, from)?,
        to: member_path(folder, to)?,
    };
    refuse_self_overlap(&member)?;
    Ok(member)
}

/// The default caption: the command as typed, `mv FROM TO` or
/// `cp FROM TO` (D-253 §9).
pub fn default_caption(kind: MemberKind, from: &str, to: &str) -> String {
    let verb = match kind {
        MemberKind::Move => "mv",
        MemberKind::Copy => "cp",
    };
    format!("{verb} {from} {to}")
}

pub async fn cmd_member(
    config: &Config,
    output: &Output,
    kind: MemberKind,
    from: String,
    to: String,
    opts: WriteOptions,
) -> Result<(), CliError> {
    // 1 — the lexical refusals, before any request or credential read.
    let cwd = std::env::current_dir().map_err(|e| CliError::Io {
        message: format!("could not determine current directory: {e}"),
    })?;
    let folder = lexical_folder(&cwd, &opts)?;
    let member = member_for(kind, folder.as_ref(), &from, &to)?;

    // 2 — resolve the write target: the guard, the repository read and
    // the parent, as `syns write` resolves them.
    let target = resolve_write_target(config, &cwd, &opts).await?;

    // 3 — the one member, and no file or deletion.
    let changeset = Changeset {
        files: Vec::new(),
        deletions: Vec::new(),
        members: vec![member],
    };
    let message = default_caption(kind, &from, &to);
    commit_changeset(config, output, &target, changeset, &opts, &message).await
}

#[cfg(test)]
mod tests {
    use super::*;

    // FEATURE_SPEC row 29: a trailing `/` is dropped and dot segments
    // resolve, counted from the root with no folder.
    #[test]
    fn member_paths_resolve_lexically() {
        let member = member_for(MemberKind::Move, None, "q3-plan/", "a/./b").unwrap();
        assert_eq!(member.from, "q3-plan");
        assert_eq!(member.to, "a/b");
        let member = member_for(MemberKind::Copy, None, "x/../y", "z//").unwrap();
        assert_eq!(member.from, "y");
        assert_eq!(member.to, "z");
    }

    // FEATURE_SPEC row 29: `TO` equal to, under or above `FROM`, naming
    // both.
    #[test]
    fn an_overlapping_pair_is_refused_naming_both() {
        for (kind, from, to, line) in [
            (
                MemberKind::Move,
                "docs",
                "docs/archive",
                "configuration error: cannot move docs to docs/archive: docs/archive lies under docs",
            ),
            (
                MemberKind::Move,
                "docs/a",
                "docs",
                "configuration error: cannot move docs/a to docs: docs lies above docs/a",
            ),
            (
                MemberKind::Copy,
                "a",
                "a/",
                "configuration error: cannot copy a to a: they are the same path",
            ),
        ] {
            let err = member_for(kind, None, from, to).unwrap_err();
            assert_eq!(err.to_string(), line);
            assert_eq!(err.exit_code(), 1);
            assert_eq!(err.code(), "config_error");
        }
        // A sibling sharing a prefix is no overlap.
        assert!(member_for(MemberKind::Move, None, "docs", "docs-old").is_ok());
        assert!(member_for(MemberKind::Move, None, "Readme.md", "README.md").is_ok());
        assert!(member_for(MemberKind::Move, None, "2026", "2027").is_ok());
    }

    #[test]
    fn empty_root_and_climbing_paths_are_refused() {
        for (from, to, line) in [
            ("", "b", "configuration error: path cannot be empty"),
            ("a", " ", "configuration error: path cannot be empty"),
            (
                "/",
                "b",
                "configuration error: / names the repository's root; name a file counted from it",
            ),
            (
                "a/..",
                "b",
                "configuration error: a/.. names the repository's root; name a file counted from it",
            ),
            (
                "..",
                "b",
                "configuration error: .. names no path inside the repository; name one counted from its root",
            ),
        ] {
            let err = member_for(MemberKind::Move, None, from, to).unwrap_err();
            assert_eq!(err.to_string(), line, "{from:?} {to:?}");
        }
    }

    #[test]
    fn the_default_caption_is_the_command_as_typed() {
        assert_eq!(
            default_caption(MemberKind::Move, "q3-plan", "q3-review"),
            "mv q3-plan q3-review"
        );
        assert_eq!(
            default_caption(MemberKind::Copy, "./a.md", "b.md/"),
            "cp ./a.md b.md/"
        );
    }
}
