use crate::errors::CliError;
use ignore::gitignore::{Gitignore, GitignoreBuilder};
use ignore::overrides::OverrideBuilder;
use ignore::{Match, WalkBuilder};
use serde::Serialize;
use std::collections::{HashMap, HashSet};
use std::ffi::OsStr;
use std::io::Read;
use std::path::{Path, PathBuf};

const BINARY_CHECK_SIZE: usize = 8192;

const DEFAULT_EXCLUDE_DIRS: &[&str] = &[
    "node_modules",
    "__pycache__",
    ".venv",
    ".tox",
    "target",
    ".next",
    ".nuxt",
    "dist",
    "build",
    ".cache",
];

/// Options for `collect_files`. Wired from the CLI flags
/// `--no-default-excludes` and `--debug` (see SPEC u213 § 3.2).
#[derive(Debug, Clone)]
pub struct CollectOptions {
    pub no_default_excludes: bool,
    pub debug: bool,
}

impl CollectOptions {
    /// Zero-config defaults — preserves the u13 walker contract.
    pub const fn default_const() -> Self {
        CollectOptions {
            no_default_excludes: false,
            debug: false,
        }
    }
}

impl Default for CollectOptions {
    fn default() -> Self {
        Self::default_const()
    }
}

/// One reason a file was excluded from the push. First match wins per
/// SPEC u213 § 5 D2: `Binary > DefaultExcludeDir > UserExclude > Gitignore > Synsignore`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SkipReason {
    Binary,
    DefaultExcludeDir,
    UserExclude,
    Gitignore,
    Synsignore,
}

impl std::fmt::Display for SkipReason {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let label = match self {
            SkipReason::Binary => "binary content",
            SkipReason::DefaultExcludeDir => "default-excluded directory",
            SkipReason::UserExclude => "--exclude pattern",
            SkipReason::Gitignore => ".gitignore rule",
            SkipReason::Synsignore => ".synsignore rule",
        };
        write!(f, "{label}")
    }
}

/// One excluded file plus its attribution.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SkippedFile {
    pub path: String,
    pub reason: SkipReason,
}

/// Output of `collect_files` — kept files plus every excluded file
/// plus the count of file leaves the walker actually visited.
#[derive(Debug, Clone, Default)]
pub struct CollectResult {
    pub files: HashMap<String, Vec<u8>>,
    pub skipped: Vec<SkippedFile>,
    pub total_walked: usize,
}

fn to_forward_slash_path(path: &Path) -> Option<String> {
    let parts: Option<Vec<&str>> = path.components().map(|c| c.as_os_str().to_str()).collect();
    parts.map(|p| p.join("/"))
}

fn is_binary(path: &Path) -> Result<bool, std::io::Error> {
    let mut file = std::fs::File::open(path)?;
    let mut buf = [0u8; BINARY_CHECK_SIZE];
    let n = file.read(&mut buf)?;
    Ok(buf[..n].contains(&0))
}

fn in_default_exclude_dir(rel_path: &Path) -> bool {
    if let Some(parent) = rel_path.parent() {
        for component in parent.components() {
            let name = component.as_os_str();
            if DEFAULT_EXCLUDE_DIRS.iter().any(|d| name == OsStr::new(d)) {
                return true;
            }
        }
    }
    false
}

/// Build a positive-only matcher from every `<filename>` file found
/// by walking the source tree. The matcher is used only for skip
/// attribution (gitignore vs. synsignore disambiguation in the
/// classifier); the actual exclusion decision uses the kept walker's
/// hierarchical filters which respect nested negation semantics.
///
/// Positive-only means: lines starting with `!` (negation) are
/// dropped. This prevents a deeper-tree negation from masking a
/// shallower-tree positive pattern in the flat matcher — at the cost
/// of slightly inflating attribution: a file matched only by a
/// negation would not appear here, but in that case the kept walker
/// would have re-included the file anyway, so no attribution is
/// needed.
fn build_positive_only_matcher(source: &Path, filename: &str) -> Gitignore {
    let mut builder = GitignoreBuilder::new(source);
    for entry in WalkBuilder::new(source)
        .hidden(false)
        .require_git(false)
        .parents(false)
        .standard_filters(false)
        .build()
        .flatten()
    {
        if !entry.file_type().is_some_and(|ft| ft.is_file()) {
            continue;
        }
        if entry.file_name() != OsStr::new(filename) {
            continue;
        }
        let content = match std::fs::read_to_string(entry.path()) {
            Ok(s) => s,
            Err(_) => continue,
        };
        let from = entry.path().to_path_buf();
        for line in content.lines() {
            let trimmed = line.trim();
            if trimmed.is_empty() || trimmed.starts_with('#') {
                continue;
            }
            if trimmed.starts_with('!') {
                continue;
            }
            let _ = builder.add_line(Some(from.clone()), trimmed);
        }
    }
    builder.build().unwrap_or_else(|_| Gitignore::empty())
}

pub fn collect_files(
    path: &Path,
    excludes: &[String],
    opts: CollectOptions,
) -> Result<CollectResult, CliError> {
    // Build the user-exclude matcher (preserved semantics from u13).
    let mut overrides_builder = OverrideBuilder::new(path);
    for pattern in excludes {
        overrides_builder
            .add(&format!("!{pattern}"))
            .map_err(|err| CliError::Io {
                message: format!("invalid exclude pattern '{pattern}': {err}"),
            })?;
    }
    let user_overrides = overrides_builder.build().map_err(|err| CliError::Io {
        message: format!("invalid exclude pattern: {err}"),
    })?;

    // Positive-only attribution matchers used by the skip-reason
    // classifier to disambiguate Gitignore vs Synsignore. They are
    // NOT used to decide kept-vs-excluded — that is the kept walker's
    // responsibility.
    let attribution_gitignore = build_positive_only_matcher(path, ".gitignore");
    let attribution_synsignore = build_positive_only_matcher(path, ".synsignore");

    // Walk 1: kept walker — uses the `ignore` crate's hierarchical
    // filters which correctly respect nested .gitignore / .synsignore
    // semantics including negation patterns. D-067: parents(false)
    // means ancestor ignore files are not consulted.
    let no_default_excludes = opts.no_default_excludes;
    let kept_walker = WalkBuilder::new(path)
        .hidden(false)
        .require_git(false)
        .parents(false)
        .add_custom_ignore_filename(".synsignore")
        .overrides(user_overrides.clone())
        .filter_entry(move |entry| {
            let name = entry.file_name();
            if name == OsStr::new(".git") {
                return false;
            }
            if !no_default_excludes
                && entry.file_type().is_some_and(|ft| ft.is_dir())
                && DEFAULT_EXCLUDE_DIRS.iter().any(|d| name == OsStr::new(d))
            {
                return false;
            }
            true
        })
        .build();

    let mut kept_files: HashMap<String, Vec<u8>> = HashMap::new();
    let mut binary_among_kept: HashSet<String> = HashSet::new();

    for result in kept_walker {
        let entry = result.map_err(|err| CliError::Io {
            message: format!("walk error: {err}"),
        })?;
        let file_type = match entry.file_type() {
            Some(ft) => ft,
            None => continue,
        };
        if file_type.is_dir() {
            continue;
        }
        if file_type.is_symlink() && !entry.path().is_file() {
            continue;
        }

        let rel_path_buf: PathBuf = match entry.path().strip_prefix(path) {
            Ok(p) => p.to_path_buf(),
            Err(_) => continue,
        };
        let rel_path_str = match to_forward_slash_path(&rel_path_buf) {
            Some(p) => p,
            None => continue,
        };

        if is_binary(entry.path()).unwrap_or(false) {
            binary_among_kept.insert(rel_path_str);
            continue;
        }

        let contents = std::fs::read(entry.path()).map_err(|err| CliError::Io {
            message: format!("could not read {rel_path_str}: {err}"),
        })?;
        kept_files.insert(rel_path_str, contents);
    }

    // Walk 2: full enumeration — yields every file regardless of
    // hierarchical filters (only .git is excluded unconditionally).
    // The difference between this set and the kept set identifies
    // files the kept walker excluded.
    let full_walker = WalkBuilder::new(path)
        .hidden(false)
        .require_git(false)
        .parents(false)
        .standard_filters(false)
        .filter_entry(|entry| entry.file_name() != OsStr::new(".git"))
        .build();

    let mut skipped: Vec<SkippedFile> = Vec::new();
    let mut total_walked: usize = 0;

    for result in full_walker {
        let entry = result.map_err(|err| CliError::Io {
            message: format!("walk error: {err}"),
        })?;

        let file_type = match entry.file_type() {
            Some(ft) => ft,
            None => continue,
        };
        if file_type.is_dir() {
            continue;
        }
        if file_type.is_symlink() && !entry.path().is_file() {
            continue;
        }

        let rel_path_buf: PathBuf = match entry.path().strip_prefix(path) {
            Ok(p) => p.to_path_buf(),
            Err(_) => continue,
        };
        let rel_path_str = match to_forward_slash_path(&rel_path_buf) {
            Some(p) => p,
            None => continue,
        };

        total_walked += 1;

        if kept_files.contains_key(&rel_path_str) {
            continue; // kept text file — not skipped
        }

        // Attribute the skip reason. Precedence per SPEC § 5 D2:
        // Binary > DefaultExcludeDir > UserExclude > Gitignore > Synsignore.
        let reason = if binary_among_kept.contains(&rel_path_str)
            || is_binary(entry.path()).unwrap_or(false)
        {
            // Files inside default-excluded dirs are also checked
            // for binary content, so the precedence Binary > DefaultExcludeDir
            // is observed.
            SkipReason::Binary
        } else if !opts.no_default_excludes && in_default_exclude_dir(&rel_path_buf) {
            SkipReason::DefaultExcludeDir
        } else if matches!(
            user_overrides.matched(&rel_path_buf, false),
            Match::Ignore(_)
        ) {
            SkipReason::UserExclude
        } else if matches!(
            attribution_gitignore.matched_path_or_any_parents(&rel_path_buf, false),
            Match::Ignore(_)
        ) {
            SkipReason::Gitignore
        } else if matches!(
            attribution_synsignore.matched_path_or_any_parents(&rel_path_buf, false),
            Match::Ignore(_)
        ) {
            SkipReason::Synsignore
        } else {
            // Walker excluded this file but no attribution matcher
            // claims it. Default to Gitignore (the most common cause
            // of walker-side exclusion when no other reason applies).
            SkipReason::Gitignore
        };

        if opts.debug {
            let source_label = match reason {
                SkipReason::Binary => "binary-heuristic",
                SkipReason::DefaultExcludeDir => "default-excludes",
                SkipReason::UserExclude => "--exclude",
                SkipReason::Gitignore => ".gitignore",
                SkipReason::Synsignore => ".synsignore",
            };
            eprintln!("[debug] skip {rel_path_str}: {reason} ({source_label})");
        }

        skipped.push(SkippedFile {
            path: rel_path_str,
            reason,
        });
    }

    // Sort skipped entries deterministically: by reason (declaration
    // order from `SkipReason` enum), then by path lex. This makes the
    // stderr renderer trivial and tests deterministic.
    skipped.sort_by(|a, b| (a.reason as u8, &a.path).cmp(&(b.reason as u8, &b.path)));

    Ok(CollectResult {
        files: kept_files,
        skipped,
        total_walked,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn collects_all_files_in_directory() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.txt"), "hello").unwrap();
        std::fs::create_dir_all(dir.path().join("sub")).unwrap();
        std::fs::write(dir.path().join("sub/b.txt"), "world").unwrap();
        std::fs::create_dir_all(dir.path().join("sub/deep")).unwrap();
        std::fs::write(dir.path().join("sub/deep/c.txt"), "nested").unwrap();

        let files = collect_files(dir.path(), &[], CollectOptions::default())
            .unwrap()
            .files;
        assert_eq!(files.len(), 3);
        assert_eq!(files.get("a.txt").unwrap(), b"hello");
        assert_eq!(files.get("sub/b.txt").unwrap(), b"world");
        assert_eq!(files.get("sub/deep/c.txt").unwrap(), b"nested");
    }

    #[test]
    fn respects_gitignore_patterns() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("keep.txt"), "keep").unwrap();
        std::fs::write(dir.path().join("skip.log"), "skip").unwrap();
        std::fs::write(dir.path().join(".gitignore"), "*.log").unwrap();

        let files = collect_files(dir.path(), &[], CollectOptions::default())
            .unwrap()
            .files;
        assert_eq!(files.len(), 2);
        assert!(files.contains_key("keep.txt"));
        assert!(files.contains_key(".gitignore"));
        assert!(!files.contains_key("skip.log"));
    }

    #[test]
    fn respects_synsignore_patterns() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("keep.txt"), "keep").unwrap();
        std::fs::write(dir.path().join("secret.env"), "secret").unwrap();
        std::fs::write(dir.path().join(".synsignore"), "*.env").unwrap();

        let files = collect_files(dir.path(), &[], CollectOptions::default())
            .unwrap()
            .files;
        assert_eq!(files.len(), 2);
        assert!(files.contains_key("keep.txt"));
        assert!(files.contains_key(".synsignore"));
        assert!(!files.contains_key("secret.env"));
    }

    #[test]
    fn exclude_flag_overrides_inclusion() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("main.rs"), "fn main() {}").unwrap();
        std::fs::write(dir.path().join("test.rs"), "#[test]").unwrap();
        std::fs::write(dir.path().join("data.csv"), "a,b,c").unwrap();

        let files = collect_files(
            dir.path(),
            &["*.csv".to_string()],
            CollectOptions::default(),
        )
        .unwrap()
        .files;
        assert_eq!(files.len(), 2);
        assert!(files.contains_key("main.rs"));
        assert!(files.contains_key("test.rs"));
        assert!(!files.contains_key("data.csv"));
    }

    #[test]
    fn excludes_git_directory() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("keep.txt"), "keep").unwrap();
        std::fs::create_dir_all(dir.path().join(".git/objects")).unwrap();
        std::fs::write(dir.path().join(".git/config"), "[core]").unwrap();
        std::fs::write(dir.path().join(".git/objects/abc"), "blob").unwrap();

        let files = collect_files(dir.path(), &[], CollectOptions::default())
            .unwrap()
            .files;
        assert_eq!(files.len(), 1);
        assert!(files.contains_key("keep.txt"));
        assert!(!files.contains_key(".git/config"));
        assert!(!files.contains_key(".git/objects/abc"));
    }

    #[cfg(unix)]
    #[test]
    fn skips_symlink_to_directory() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("keep.txt"), "keep").unwrap();
        std::fs::create_dir_all(dir.path().join("real_dir")).unwrap();
        std::fs::write(dir.path().join("real_dir/inner.txt"), "inner").unwrap();
        std::os::unix::fs::symlink(dir.path().join("real_dir"), dir.path().join("link_dir"))
            .unwrap();

        let files = collect_files(dir.path(), &[], CollectOptions::default())
            .unwrap()
            .files;
        assert!(files.contains_key("keep.txt"));
        assert!(files.contains_key("real_dir/inner.txt"));
        assert!(!files.contains_key("link_dir"));
        assert!(!files.contains_key("link_dir/inner.txt"));
    }

    #[cfg(unix)]
    #[test]
    fn collects_symlink_to_file() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("real.txt"), "target content").unwrap();
        std::os::unix::fs::symlink(dir.path().join("real.txt"), dir.path().join("link.txt"))
            .unwrap();

        let files = collect_files(dir.path(), &[], CollectOptions::default())
            .unwrap()
            .files;
        assert_eq!(files.len(), 2);
        assert_eq!(files.get("real.txt").unwrap(), b"target content");
        assert_eq!(files.get("link.txt").unwrap(), b"target content");
    }

    #[test]
    fn binary_files_are_skipped() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("text.txt"), "hello").unwrap();
        std::fs::write(dir.path().join("binary.bin"), b"binary\x00data").unwrap();

        let files = collect_files(dir.path(), &[], CollectOptions::default())
            .unwrap()
            .files;
        assert_eq!(files.len(), 1);
        assert_eq!(files.get("text.txt").unwrap(), b"hello");
        assert!(!files.contains_key("binary.bin"));
    }

    #[test]
    fn excludes_common_dependency_and_build_directories() {
        let dir = tempfile::tempdir().unwrap();

        // Files that should be collected
        std::fs::write(dir.path().join("keep.txt"), "keep").unwrap();
        std::fs::create_dir_all(dir.path().join("src")).unwrap();
        std::fs::write(dir.path().join("src/main.rs"), "fn main() {}").unwrap();

        // Directories that should be excluded
        std::fs::create_dir_all(dir.path().join("node_modules/leftpad")).unwrap();
        std::fs::write(
            dir.path().join("node_modules/leftpad/index.js"),
            "module.exports = {};",
        )
        .unwrap();
        std::fs::create_dir_all(dir.path().join("__pycache__")).unwrap();
        std::fs::write(dir.path().join("__pycache__/mod.cpython.pyc"), "cache").unwrap();
        std::fs::create_dir_all(dir.path().join(".venv/lib")).unwrap();
        std::fs::write(dir.path().join(".venv/lib/site.py"), "site").unwrap();
        std::fs::create_dir_all(dir.path().join("target/debug")).unwrap();
        std::fs::write(dir.path().join("target/debug/binary"), "elf").unwrap();
        std::fs::create_dir_all(dir.path().join(".next/static")).unwrap();
        std::fs::write(dir.path().join(".next/static/chunk.js"), "chunk").unwrap();
        std::fs::create_dir_all(dir.path().join("build")).unwrap();
        std::fs::write(dir.path().join("build/output.js"), "built").unwrap();
        std::fs::create_dir_all(dir.path().join(".cache")).unwrap();
        std::fs::write(dir.path().join(".cache/data"), "cached").unwrap();

        let files = collect_files(dir.path(), &[], CollectOptions::default())
            .unwrap()
            .files;

        assert_eq!(files.len(), 2);
        assert!(files.contains_key("keep.txt"));
        assert!(files.contains_key("src/main.rs"));

        // Verify excluded directories are not present
        assert!(!files.keys().any(|k| k.starts_with("node_modules/")));
        assert!(!files.keys().any(|k| k.starts_with("__pycache__/")));
        assert!(!files.keys().any(|k| k.starts_with(".venv/")));
        assert!(!files.keys().any(|k| k.starts_with("target/")));
        assert!(!files.keys().any(|k| k.starts_with(".next/")));
        assert!(!files.keys().any(|k| k.starts_with("build/")));
        assert!(!files.keys().any(|k| k.starts_with(".cache/")));
    }

    #[test]
    fn excludes_directories_not_files_with_same_name() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("node_modules"), "I am a file").unwrap();
        std::fs::write(dir.path().join("keep.txt"), "keep").unwrap();

        let files = collect_files(dir.path(), &[], CollectOptions::default())
            .unwrap()
            .files;

        assert_eq!(files.len(), 2);
        assert!(files.contains_key("node_modules"));
        assert!(files.contains_key("keep.txt"));
    }

    // ===== New u213 tests =====

    #[test]
    fn binary_skip_is_attributed_to_binary_reason() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("text.txt"), "hello").unwrap();
        std::fs::write(dir.path().join("binary.bin"), b"data\x00more").unwrap();

        let result = collect_files(dir.path(), &[], CollectOptions::default()).unwrap();
        assert_eq!(result.files.len(), 1);
        assert_eq!(result.skipped.len(), 1);
        assert_eq!(
            result.skipped[0],
            SkippedFile {
                path: "binary.bin".into(),
                reason: SkipReason::Binary,
            }
        );
        assert_eq!(result.total_walked, 2);
    }

    #[test]
    fn default_excluded_dir_files_attributed_to_default_exclude_dir_reason() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("keep.txt"), "k").unwrap();
        std::fs::create_dir_all(dir.path().join("dist")).unwrap();
        std::fs::write(dir.path().join("dist/index.html"), "h").unwrap();
        std::fs::write(dir.path().join("dist/asset.js"), "j").unwrap();
        std::fs::create_dir_all(dir.path().join("build")).unwrap();
        std::fs::write(dir.path().join("build/output.js"), "o").unwrap();
        std::fs::create_dir_all(dir.path().join("node_modules/leftpad")).unwrap();
        std::fs::write(dir.path().join("node_modules/leftpad/index.js"), "n").unwrap();

        let result = collect_files(dir.path(), &[], CollectOptions::default()).unwrap();
        assert_eq!(result.files.len(), 1);
        assert!(result.files.contains_key("keep.txt"));

        let skipped_paths: Vec<&str> = result.skipped.iter().map(|s| s.path.as_str()).collect();
        assert_eq!(skipped_paths.len(), 4);
        assert!(
            result
                .skipped
                .iter()
                .all(|s| s.reason == SkipReason::DefaultExcludeDir)
        );
        // Sorted by (reason, path) — all same reason → lex order on path.
        assert_eq!(
            skipped_paths,
            vec![
                "build/output.js",
                "dist/asset.js",
                "dist/index.html",
                "node_modules/leftpad/index.js",
            ]
        );
        assert_eq!(result.total_walked, 5);
    }

    #[test]
    fn user_exclude_files_attributed_to_user_exclude_reason() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("keep.rs"), "fn main(){}").unwrap();
        std::fs::write(dir.path().join("data.csv"), "a,b,c").unwrap();

        let result =
            collect_files(dir.path(), &["*.csv".into()], CollectOptions::default()).unwrap();
        assert!(result.files.contains_key("keep.rs"));
        assert!(!result.files.contains_key("data.csv"));
        assert_eq!(result.skipped.len(), 1);
        assert_eq!(
            result.skipped[0],
            SkippedFile {
                path: "data.csv".into(),
                reason: SkipReason::UserExclude,
            }
        );
    }

    #[test]
    fn source_local_gitignore_attributed_to_gitignore_reason() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("keep.txt"), "k").unwrap();
        std::fs::write(dir.path().join("skip.log"), "s").unwrap();
        std::fs::write(dir.path().join(".gitignore"), "*.log").unwrap();

        let result = collect_files(dir.path(), &[], CollectOptions::default()).unwrap();
        assert!(result.files.contains_key("keep.txt"));
        assert!(result.files.contains_key(".gitignore"));
        assert!(!result.files.contains_key("skip.log"));
        assert_eq!(result.skipped.len(), 1);
        assert_eq!(
            result.skipped[0],
            SkippedFile {
                path: "skip.log".into(),
                reason: SkipReason::Gitignore,
            }
        );
    }

    #[test]
    fn source_local_synsignore_attributed_to_synsignore_reason() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("keep.txt"), "k").unwrap();
        std::fs::write(dir.path().join("secret.env"), "s").unwrap();
        std::fs::write(dir.path().join(".synsignore"), "*.env").unwrap();

        let result = collect_files(dir.path(), &[], CollectOptions::default()).unwrap();
        assert!(result.files.contains_key("keep.txt"));
        assert!(result.files.contains_key(".synsignore"));
        assert!(!result.files.contains_key("secret.env"));
        assert_eq!(result.skipped.len(), 1);
        assert_eq!(
            result.skipped[0],
            SkippedFile {
                path: "secret.env".into(),
                reason: SkipReason::Synsignore,
            }
        );
    }

    #[test]
    fn nested_gitignore_precedence() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(".gitignore"), "*.tmp").unwrap();
        std::fs::write(dir.path().join("a.tmp"), "root tmp").unwrap();
        std::fs::create_dir_all(dir.path().join("sub")).unwrap();
        std::fs::write(dir.path().join("sub/.gitignore"), "!*.tmp").unwrap();
        std::fs::write(dir.path().join("sub/a.tmp"), "sub tmp").unwrap();

        let result = collect_files(dir.path(), &[], CollectOptions::default()).unwrap();
        // sub/a.tmp re-included by the nested negation; a.tmp skipped.
        assert!(result.files.contains_key("sub/a.tmp"));
        assert!(!result.files.contains_key("a.tmp"));
        assert!(
            result
                .skipped
                .iter()
                .any(|s| s.path == "a.tmp" && s.reason == SkipReason::Gitignore)
        );
    }

    #[test]
    fn precedence_binary_beats_default_exclude_dir() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("dist")).unwrap();
        std::fs::write(dir.path().join("dist/binary.bin"), b"data\x00").unwrap();

        let result = collect_files(dir.path(), &[], CollectOptions::default()).unwrap();
        assert_eq!(result.skipped.len(), 1);
        assert_eq!(result.skipped[0].reason, SkipReason::Binary);
    }

    #[test]
    fn no_default_excludes_flag_re_includes_build_dirs() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("keep.txt"), "k").unwrap();
        std::fs::create_dir_all(dir.path().join("dist")).unwrap();
        std::fs::write(dir.path().join("dist/index.html"), "h").unwrap();
        std::fs::create_dir_all(dir.path().join("build")).unwrap();
        std::fs::write(dir.path().join("build/output.js"), "o").unwrap();

        let result = collect_files(
            dir.path(),
            &[],
            CollectOptions {
                no_default_excludes: true,
                debug: false,
            },
        )
        .unwrap();
        assert_eq!(result.files.len(), 3);
        assert!(result.files.contains_key("keep.txt"));
        assert!(result.files.contains_key("dist/index.html"));
        assert!(result.files.contains_key("build/output.js"));
        assert!(result.skipped.is_empty());
    }

    #[test]
    fn ancestor_non_hostile_gitignore_also_not_applied() {
        let ancestor = tempfile::tempdir().unwrap();
        std::fs::write(ancestor.path().join(".gitignore"), "*.tmp").unwrap();
        std::fs::create_dir_all(ancestor.path().join("source")).unwrap();
        std::fs::write(ancestor.path().join("source/keep.tmp"), "keep").unwrap();

        let result = collect_files(
            &ancestor.path().join("source"),
            &[],
            CollectOptions::default(),
        )
        .unwrap();
        assert!(result.files.contains_key("keep.tmp"));
        assert!(result.skipped.is_empty());
    }

    #[test]
    fn dotfiles_bare_repo_reproduction_passes_after_fix() {
        let ancestor = tempfile::tempdir().unwrap();
        std::fs::write(ancestor.path().join(".gitignore"), "*\n!keep-this\n").unwrap();
        std::fs::create_dir_all(ancestor.path().join("source")).unwrap();
        std::fs::write(ancestor.path().join("source/important.md"), "imp").unwrap();
        std::fs::write(ancestor.path().join("source/code.rs"), "fn main(){}").unwrap();

        let result = collect_files(
            &ancestor.path().join("source"),
            &[],
            CollectOptions::default(),
        )
        .unwrap();
        assert!(result.files.contains_key("important.md"));
        assert!(result.files.contains_key("code.rs"));
        assert!(result.skipped.is_empty());
    }

    #[test]
    fn collector_source_contains_no_project_marker_constants() {
        let source = include_str!("collector.rs");
        // The test module below intentionally lists project-marker
        // strings to *guard against* their presence in the production
        // section. Split on the test cfg marker so this check only
        // inspects the production-code portion of the file.
        let prod_source = source.split("#[cfg(test)]").next().unwrap_or(source);
        for marker in [
            "Cargo.toml",
            "package.json",
            "pyproject.toml",
            "go.mod",
            "pom.xml",
            "Gemfile",
            "composer.json",
        ] {
            assert!(
                !prod_source.contains(marker),
                "collector.rs production section unexpectedly contains marker \
                 `{marker}` — D-067 § 5 mandates B-flip (parents(false)), \
                 NOT B-bound (marker discovery)"
            );
        }
    }
}
