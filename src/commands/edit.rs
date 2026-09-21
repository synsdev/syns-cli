//! `syns edit PATH --old TEXT --new TEXT --parent REF` — one commit
//! changing one path (SPEC u271).
//!
//! It behaves as the agent's own editing tool does: the content is read
//! at the stated parent, `--old` is refused where it matches nowhere and
//! where it matches more than once with no `--replace-all`, and each
//! refusal is raised before any push is composed. It touches the content
//! cache not at all — that store answers the search verb alone.

use crate::client::SynsClient;
use crate::config::Config;
use crate::errors::CliError;
use crate::output::Output;
use crate::write::{
    Changeset, WriteOptions, commit_changeset, default_message, resolve_write_target,
    text_or_refuse,
};

/// The refusal a `--old` matching nowhere takes, naming the path and the
/// parent as the caller spelt it.
pub fn old_matched_nothing(parent: &str, path: &str) -> String {
    format!("--old matched no content at version {parent}: {path}")
}

/// The refusal a `--old` matching more than once takes where
/// `--replace-all` does not stand.
pub fn old_matched_more_than_once(count: usize, path: &str) -> String {
    format!("--old matched {count} times in {path}; pass --replace-all to replace every occurrence")
}

/// The not-found line `cmd_edit` 2 renders, naming the path and the
/// parent the caller spelt.
pub fn path_not_found_at_parent(parent: &str, path: &str) -> String {
    format!("path not found at version {parent}: {path}")
}

/// `cmd_edit` 4 and 5: the content with `--old` replaced once, or
/// everywhere, and the two refusals the count raises — each before any
/// push is composed, so a refused edit publishes nothing.
pub fn replaced(
    content: &str,
    old: &str,
    new: &str,
    replace_all: bool,
    path: &str,
    parent: &str,
) -> Result<String, CliError> {
    let count = content.matches(old).count();
    if count == 0 {
        return Err(CliError::Config {
            message: old_matched_nothing(parent, path),
        });
    }
    if count > 1 && !replace_all {
        return Err(CliError::Config {
            message: old_matched_more_than_once(count, path),
        });
    }
    Ok(if replace_all {
        content.replace(old, new)
    } else {
        content.replacen(old, new, 1)
    })
}

/// `cmd_edit` 1: the two refusals the options raise before the target is
/// resolved, so neither reaches the wire.
pub fn refuse_option_pair(old: &str, new: &str) -> Result<(), CliError> {
    if old.is_empty() {
        return Err(CliError::Config {
            message: "--old cannot be empty".to_string(),
        });
    }
    if old == new {
        return Err(CliError::Config {
            message: "--old and --new are the same value".to_string(),
        });
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
pub async fn cmd_edit(
    config: &Config,
    output: &Output,
    path: String,
    old: String,
    new: String,
    replace_all: bool,
    opts: WriteOptions,
) -> Result<(), CliError> {
    // 1 — refuse an `--old` that is empty or equal to `--new`, then
    // resolve the write target.
    refuse_option_pair(&old, &new)?;
    let cwd = std::env::current_dir().map_err(|e| CliError::Io {
        message: format!("could not determine current directory: {e}"),
    })?;
    let target = resolve_write_target(config, &cwd, &opts).await?;

    // 2 — read the path at the parent, addressing it by the resolved
    // version's decimal ordinal where one stands and by the full hash
    // otherwise (`Q-02`).
    let client = SynsClient::new(config.server_url())?;
    let reference = target.parent.read_ref();
    let (response, _raw) = client
        .get_file(
            &target.repo_id,
            Some(&target.token),
            &path,
            Some(&reference),
        )
        .await
        .map_err(|e| {
            e.with_versioned_read_context(path_not_found_at_parent(&opts.parent, &path))
        })?;

    // 3 — classify the answered content. A content the store already
    // replaced byte by byte classifies as text and passes
    // (`issues/082-engine-binary-content-corrupted-via-utf8-roundtrip`).
    let content = text_or_refuse(&path, response.content.as_bytes())?;

    // 4 and 5 — count the occurrences and replace them.
    let edited = replaced(&content, &old, &new, replace_all, &path, &opts.parent)?;

    // 6 — commit the one changed path.
    let changeset = Changeset {
        files: vec![(path.clone(), edited)],
        deletions: Vec::new(),
    };
    let message = default_message("edit", Some(&path));
    commit_changeset(config, output, &target, changeset, &opts, &message).await
}

#[cfg(test)]
mod tests {
    use super::*;

    const PARENT: &str = "aa11bb22cc33dd44ee55ff6600778899001122bb";

    // SPEC u271 Contract Surface, the edit refusals: an empty `--old`
    // and an `--old` equal to `--new`, each before any request.
    #[test]
    fn an_empty_old_and_an_old_equal_to_new_are_refused() {
        assert_eq!(
            refuse_option_pair("", "two").unwrap_err().to_string(),
            "configuration error: --old cannot be empty"
        );
        assert_eq!(
            refuse_option_pair("one", "one").unwrap_err().to_string(),
            "configuration error: --old and --new are the same value"
        );
        // A `--new` that is the empty string is admitted, cutting what
        // `--old` names.
        assert!(refuse_option_pair("one", "").is_ok());
        assert_eq!(
            replaced("keep one", "one", "", false, "a.md", PARENT).unwrap(),
            "keep "
        );
    }

    // The refusal an `--old` matching nowhere takes, naming the path and
    // the parent as the caller spelt it.
    #[test]
    fn an_old_matching_nowhere_names_the_path_and_the_parent() {
        let err = replaced("keep one", "absent", "two", false, "a.md", PARENT).unwrap_err();
        assert_eq!(
            err.to_string(),
            format!("configuration error: --old matched no content at version {PARENT}: a.md")
        );
        assert_eq!(err.exit_code(), 1);
    }

    // More than one match is refused without `--replace-all`, and every
    // occurrence is replaced with it.
    #[test]
    fn an_old_matching_twice_needs_replace_all() {
        let err = replaced("one one", "one", "two", false, "a.md", PARENT).unwrap_err();
        assert_eq!(
            err.to_string(),
            "configuration error: --old matched 2 times in a.md; pass --replace-all to replace every occurrence"
        );
        assert_eq!(
            replaced("one one", "one", "two", true, "a.md", PARENT).unwrap(),
            "two two"
        );
        // One match replaces without the option.
        assert_eq!(
            replaced("keep one", "one", "two", false, "a.md", PARENT).unwrap(),
            "keep two"
        );
        // And `--replace-all` over one match is still one replacement.
        assert_eq!(
            replaced("keep one", "one", "two", true, "a.md", PARENT).unwrap(),
            "keep two"
        );
    }

    #[test]
    fn the_not_found_line_names_the_path_and_the_parent() {
        assert_eq!(
            path_not_found_at_parent(PARENT, "a.md"),
            format!("path not found at version {PARENT}: a.md")
        );
    }
}
