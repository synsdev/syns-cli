//! `syns glob PATTERN [--path P]` — pattern matching over the tree at
//! one version, fetching no content (SPEC u270).
//!
//! The ordering is by path. Ordering by the version that last changed
//! each matched path was ruled out on its cost: `units/cli/u270/PROTOTYPE.md`
//! measured 3.579s of version paging against the 0.203s tree read that
//! paging would order.

use globset::{Glob, GlobBuilder, GlobMatcher};

use crate::client::{EntryType, SynsClient};
use crate::config::Config;
use crate::errors::{CliError, partial_truncated_tree};
use crate::output::Output;
use crate::read::{
    ReadOptions, mark_partial, read_not_found, report_reference, resolve_read_target,
};

/// Compiles one pattern against a whole repository-relative path:
/// `**/` stands for zero or more whole directories, and `*` and `?`
/// match no path separator.
pub fn compile_whole_path(pattern: &str) -> Result<GlobMatcher, CliError> {
    build(pattern).map(|glob| glob.compile_matcher())
}

fn build(pattern: &str) -> Result<Glob, CliError> {
    GlobBuilder::new(pattern)
        .literal_separator(true)
        .build()
        .map_err(|e| CliError::Config {
            message: format!("invalid pattern {pattern}: {e}"),
        })
}

pub async fn cmd_glob(
    config: &Config,
    output: &Output,
    pattern: String,
    path: Option<String>,
    opts: ReadOptions,
) -> Result<(), CliError> {
    // 1 — compile the pattern, before any request.
    let matcher = compile_whole_path(&pattern)?;

    // 2 — resolve the target.
    let Some(target) = resolve_read_target(config, output, &opts).await? else {
        return Ok(());
    };
    let client = SynsClient::new(config.server_url())?;

    // 3 — read the tree recursively at that reference under `--path`.
    let version_ref = target.version_ref();
    let (response, _raw) = match client
        .get_tree(
            &target.repo_id,
            target.token.as_deref(),
            path.as_deref(),
            true,
            Some(&version_ref),
        )
        .await
    {
        Ok(tuple) => tuple,
        Err(e) => {
            if let Some(p) = path.as_ref() {
                if opts.version.is_some() {
                    return Err(read_not_found(e, &opts, &target.reference, p));
                }
                if !output.is_json() {
                    return Err(e.with_ls_path_context(p.clone()));
                }
            }
            return Err(e);
        }
    };

    // 4 — keep every entry of kind file whose whole path the pattern
    // matches, and 5 — order the kept set by ascending path.
    let mut matches: Vec<_> = response
        .entries
        .into_iter()
        .filter(|e| e.entry_type == EntryType::File && matcher.is_match(&e.path))
        .collect();
    matches.sort_by(|a, b| a.path.cmp(&b.path));

    // 6 — write the paths, or one document carrying the matches.
    let truncated = response.truncated;
    let mut document = serde_json::json!({
        "version": target.reference.version,
        "commitSha": target.reference.commit_sha,
        "pattern": pattern,
        "path": path,
        "truncated": truncated,
        "matches": matches
            .iter()
            .map(|e| serde_json::json!({ "path": e.path, "sha": e.sha, "size": e.size }))
            .collect::<Vec<_>>(),
    });

    if !output.is_json() {
        for entry in &matches {
            println!("{}", entry.path);
        }
        // 7 — report the reference.
        report_reference(output, &target.reference);
    } else if !truncated {
        output.json(&document);
    }

    if truncated {
        let refusal = partial_truncated_tree(target.reference.version);
        document = mark_partial(document, &refusal);
        return Err(CliError::PartialAnswer {
            document,
            line: refusal,
        });
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    // SPEC u270 Contract Surface, `cmd_glob`: the pattern is matched
    // against each entry's whole repository-relative path, `**/`
    // standing for zero or more whole directories.
    #[test]
    fn a_leading_double_star_stands_for_zero_or_more_whole_directories() {
        let matcher = compile_whole_path("**/*.ts").unwrap();
        assert!(matcher.is_match("a.ts"));
        assert!(matcher.is_match("src/a.ts"));
        assert!(matcher.is_match("src/deep/b.ts"));
        assert!(!matcher.is_match("README.md"));
    }

    // SPEC u270 Tests: `a_single_star_crosses_no_separator`.
    #[test]
    fn a_single_star_crosses_no_separator() {
        let matcher = compile_whole_path("src/*.ts").unwrap();
        assert!(matcher.is_match("src/a.ts"));
        assert!(!matcher.is_match("src/deep/b.ts"));
        assert!(!matcher.is_match("a.ts"));
    }

    #[test]
    fn a_question_mark_crosses_no_separator() {
        let matcher = compile_whole_path("src/?.ts").unwrap();
        assert!(matcher.is_match("src/a.ts"));
        assert!(!matcher.is_match("src//.ts"));
    }

    // SPEC u270 Behaviour, `cmd_glob` 1: a pattern that does not compile
    // is refused before any request.
    #[test]
    fn a_pattern_that_does_not_compile_is_refused_naming_itself() {
        let err = compile_whole_path("src/[a-").unwrap_err();
        assert!(
            err.to_string().contains("src/[a-"),
            "the refusal names the pattern: {err}"
        );
        assert_eq!(err.exit_code(), 1);
    }
}
