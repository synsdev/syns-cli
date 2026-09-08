use crate::errors::CliError;
use crate::repo::root::{path_is_prefix_ancestor, path_within_prefix, to_forward_slash};
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
#[derive(Debug, Clone, Default)]
pub struct CollectOptions {
    /// Mirror of CLI `--no-default-excludes`. When `true`, the
    /// built-in skip list (`DEFAULT_EXCLUDE_DIRS`) is bypassed and
    /// build / cache directory contents flow into the kept set.
    pub no_default_excludes: bool,
    /// Mirror of CLI `--debug`. When `true`, every skip decision is
    /// emitted to stderr as `[debug] skip {path}: {reason} ({source})`.
    /// Uncoloured / not styled — these lines are documented as
    /// copy-pastable in SPEC u213 § 3.2.
    pub debug: bool,
    /// The subtree — or the single path — both walks are confined to,
    /// `/`-separated and relative to the `path` given to
    /// `collect_files` (SPEC u255 § Contract Surface). `None` walks
    /// the whole tree below `path`.
    ///
    /// The prefix is applied inside BOTH walkers' `filter_entry`, not
    /// by re-rooting the walk: `D-067` fixes the ignore root at the
    /// path given, and re-rooting would make a scoped publication
    /// stop honouring the repository root's ignore files. Applying it
    /// to the kept walk alone would be worse still — the enumeration
    /// walk would then report every out-of-scope path as dropped and
    /// `--strict` would refuse every scoped publication.
    pub prefix: Option<String>,
}

/// One reason a file was excluded from the push. First match wins per
/// SPEC u213 § 5 D2: `Binary > DefaultExcludeDir > UserExclude > Gitignore > Synsignore`.
///
/// IMPORTANT: declaration order is contractual. The enum is cast as
/// `u8` (sort key in `SkippedFile.cmp` at line 352) and as `usize`
/// (group index in `render_skip_summary` and `skip_summary_cause`).
/// New variants MUST be appended at the END; reordering or inserting
/// in the middle silently corrupts the sort order on existing data
/// and the stderr layout.
#[repr(u8)]
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

/// Whether a walked entry is inside the scope `prefix` names, or is a
/// directory the walk must descend through to reach it.
///
/// Returns `true` for the walk root itself and for anything that does
/// not sit under `root` at all — neither is the prefix's business, and
/// rejecting the root would empty every scoped walk.
fn prefix_admits(root: &Path, prefix: &str, entry_path: &Path, is_dir: bool) -> bool {
    let rel = match entry_path.strip_prefix(root) {
        Ok(rel) => rel,
        Err(_) => return true,
    };
    let rel_str = match to_forward_slash(rel) {
        Some(s) => s,
        None => return true,
    };
    if rel_str.is_empty() {
        return true;
    }
    if path_within_prefix(&rel_str, prefix) {
        return true;
    }
    is_dir && path_is_prefix_ancestor(&rel_str, prefix)
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
/// `extra_filenames` is folded into the same builder. For the
/// gitignore attribution chain we also read `.ignore` files so the
/// fallback `SkipReason::Gitignore` bucket no longer silently absorbs
/// `.ignore`-driven exclusions (CODE_REVIEW M2). `.ignore` files are
/// part of `WalkBuilder::standard_filters(true)` on the kept walker;
/// without them in the attribution matcher, the kept walker would
/// (correctly) skip the file but the attribution loop would fall
/// through to the catch-all bucket and mis-label it.
///
/// NOTE on the remaining mis-attribution path (M2 case 2): a binary
/// `is_binary` error on the attribution walker still routes through
/// the catch-all `SkipReason::Gitignore` bucket. The lower-effort fix
/// for case 1 (`.ignore` files) is applied here; case 2 is observable
/// via the `[debug]` breadcrumb on the attribution walker's
/// `is_binary` Err path (see `collect_files`).
///
/// Positive-only means: lines starting with `!` (negation) are
/// dropped. This prevents a deeper-tree negation from masking a
/// shallower-tree positive pattern in the flat matcher — at the cost
/// of slightly inflating attribution: a file matched only by a
/// negation would not appear here, but in that case the kept walker
/// would have re-included the file anyway, so no attribution is
/// needed.
fn build_positive_only_matcher(
    source: &Path,
    filename: &str,
    extra_filenames: &[&str],
    debug: bool,
) -> Gitignore {
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
        let name = entry.file_name();
        let matches_primary = name == OsStr::new(filename);
        let matches_extra = extra_filenames.iter().any(|f| name == OsStr::new(*f));
        if !matches_primary && !matches_extra {
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
    builder.build().unwrap_or_else(|err| {
        if debug {
            eprintln!("[debug] build_positive_only_matcher({filename}) build error: {err}");
        }
        Gitignore::empty()
    })
}

/// Collect every file under `path` that should be pushed, plus an
/// attributed list of every file the walker rejected.
///
/// # Two-walk invariant
///
/// This function deliberately runs TWO walkers over the source tree:
///
/// 1. **Kept walker** — uses `WalkBuilder::standard_filters(true)` so
///    the `ignore` crate's own hierarchical filter chain (which
///    correctly scopes nested `.gitignore` / `.synsignore` files,
///    including negation patterns like `!*.tmp` in a deeper subdir)
///    decides keep-vs-skip.
/// 2. **Attribution walker** — uses `standard_filters(false)` so every
///    file leaf surfaces and we can diff against the kept set to
///    discover what the kept walker rejected, then classify the
///    reason via a flat positive-only matcher.
///
/// Folding back to a single walker requires re-implementing nested
/// `.gitignore` precedence by hand, which silently regresses the
/// following tests:
///
/// - `nested_gitignore_precedence`
/// - `source_local_gitignore_attributed_to_gitignore_reason`
/// - `source_local_synsignore_attributed_to_synsignore_reason`
/// - `dotfiles_bare_repo_reproduction_passes_after_fix`
///
/// Per D-067 § 5 both walkers also call `parents(false)` so ancestor
/// ignore files are never consulted (a hostile `~/.gitignore: *`
/// must NOT empty a push from a child directory).
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
    // responsibility. The gitignore attribution chain also folds in
    // `.ignore` files (M2 case 1): the kept walker's `standard_filters`
    // honours `.ignore` files, so without them in the attribution
    // matcher, an `.ignore`-driven skip would mis-attribute via the
    // catch-all fallback bucket.
    let attribution_gitignore =
        build_positive_only_matcher(path, ".gitignore", &[".ignore"], opts.debug);
    let attribution_synsignore = build_positive_only_matcher(path, ".synsignore", &[], opts.debug);

    // Walk 1: kept walker — uses the `ignore` crate's hierarchical
    // filters which correctly respect nested .gitignore / .synsignore
    // semantics including negation patterns. D-067: parents(false)
    // means ancestor ignore files are not consulted.
    let no_default_excludes = opts.no_default_excludes;
    let kept_root = path.to_path_buf();
    let kept_prefix = opts.prefix.clone();
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
            if let Some(prefix) = kept_prefix.as_deref() {
                let is_dir = entry.file_type().is_some_and(|ft| ft.is_dir());
                if !prefix_admits(&kept_root, prefix, entry.path(), is_dir) {
                    return false;
                }
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
        let rel_path_str = match to_forward_slash(&rel_path_buf) {
            Some(p) => p,
            None => continue,
        };

        // M1: explicit error propagation on the kept walker — matches
        // the u13 contract. A short-read failure on `is_binary` must
        // not silently route to the read-and-keep branch (where the
        // same IO error would surface with a different message and
        // partial state); surface it directly.
        match is_binary(entry.path()) {
            Ok(true) => {
                binary_among_kept.insert(rel_path_str);
                continue;
            }
            Ok(false) => {}
            Err(err) => {
                return Err(CliError::Io {
                    message: format!("could not read {rel_path_str}: {err}"),
                });
            }
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
    let full_root = path.to_path_buf();
    let full_prefix = opts.prefix.clone();
    let full_walker = WalkBuilder::new(path)
        .hidden(false)
        .require_git(false)
        .parents(false)
        .standard_filters(false)
        .filter_entry(move |entry| {
            if entry.file_name() == OsStr::new(".git") {
                return false;
            }
            if let Some(prefix) = full_prefix.as_deref() {
                let is_dir = entry.file_type().is_some_and(|ft| ft.is_dir());
                if !prefix_admits(&full_root, prefix, entry.path(), is_dir) {
                    return false;
                }
            }
            true
        })
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
        let rel_path_str = match to_forward_slash(&rel_path_buf) {
            Some(p) => p,
            None => continue,
        };

        total_walked += 1;

        if kept_files.contains_key(&rel_path_str) {
            continue; // kept text file — not skipped
        }

        // Attribute the skip reason. Precedence per SPEC § 5 D2:
        // Binary > DefaultExcludeDir > UserExclude > Gitignore > Synsignore.
        //
        // M1 (attribution side): the attribution loop is recovery-
        // tolerant — a single unreadable file should not kill the
        // push when the kept walker already decided to skip it. But
        // the failure must be observable: emit a debug-gated
        // breadcrumb so the rare path is at least visible under
        // `--debug`. The file then falls through to the non-binary
        // chain and ends up in whichever bucket claims it (or the
        // catch-all `SkipReason::Gitignore` fallback, see M2 case 2).
        let binary_check = match is_binary(entry.path()) {
            Ok(b) => b,
            Err(err) => {
                if opts.debug {
                    eprintln!(
                        "[debug] skip {rel_path_str}: is_binary IO error on attribution walker: {err}"
                    );
                }
                false
            }
        };
        let reason = if binary_among_kept.contains(&rel_path_str) || binary_check {
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

/// Maximum file paths shown per category in the skip-summary block
/// before the `+K more` truncation suffix kicks in (SPEC u213 § 3.4).
pub const MAX_PER_CATEGORY: usize = 5;

/// Write the SPEC § 3.4 / § 7 per-category skip-summary block to
/// `out`. Used by both `commands::push::render_skip_summary` (writes
/// to a `String` buffer then `eprint!`s it to stderr) and by
/// `errors::CliError::PushPartial`'s `Display` impl (writes to the
/// formatter directly, so the headline-then-detail order from SPEC
/// § 7 is structurally enforced — no caller-side intercept needed).
///
/// `strict` and `no_default_excludes` are the runtime flag values
/// used to gate the conditional hint lines. The function emits no
/// output (and no trailing newline) when `skipped.is_empty()`.
pub fn write_skip_summary<W: std::fmt::Write>(
    out: &mut W,
    skipped: &[SkippedFile],
    strict: bool,
    no_default_excludes: bool,
) -> std::fmt::Result {
    use SkipReason::*;

    if skipped.is_empty() {
        return Ok(());
    }

    // Group by reason (fixed declaration-order indexing; relies on
    // the `#[repr(u8)]` ABI contract on `SkipReason`).
    let mut groups: [(SkipReason, Vec<&str>); 5] = [
        (Binary, Vec::new()),
        (DefaultExcludeDir, Vec::new()),
        (UserExclude, Vec::new()),
        (Gitignore, Vec::new()),
        (Synsignore, Vec::new()),
    ];
    for sf in skipped {
        let idx = sf.reason as usize;
        groups[idx].1.push(sf.path.as_str());
    }
    // Local lex sort per bucket — defensive even though the
    // collector already sorts globally.
    for (_, paths) in groups.iter_mut() {
        paths.sort();
    }

    writeln!(out, "warning: {} file(s) skipped from push", skipped.len())?;
    for (reason, paths) in &groups {
        if paths.is_empty() {
            continue;
        }
        let total = paths.len();
        let shown: Vec<&str> = paths.iter().take(MAX_PER_CATEGORY).copied().collect();
        let joined = shown.join(", ");
        if total > MAX_PER_CATEGORY {
            let more = total - MAX_PER_CATEGORY;
            writeln!(out, "  {reason} ({total}): {joined}, +{more} more")?;
        } else {
            writeln!(out, "  {reason} ({total}): {joined}")?;
        }
    }

    // Conditional hints (SPEC § 3.4 rule 6 / § 7).
    if !strict {
        writeln!(
            out,
            "  hint: pass --strict to fail the push when any file is skipped"
        )?;
    }
    if skipped.iter().any(|sf| sf.reason == Binary) {
        writeln!(
            out,
            "  hint: add binary extensions (e.g. *.png, *.pdf) to .synsignore to silence the binary warning"
        )?;
    }
    if !no_default_excludes && skipped.iter().any(|sf| sf.reason == DefaultExcludeDir) {
        writeln!(
            out,
            "  hint: pass --no-default-excludes to include build / cache directories in the push"
        )?;
    }

    Ok(())
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

    /// CODE_REVIEW M2 case 1: `.ignore` files (honoured by the
    /// kept walker's `standard_filters(true)`) must also be folded
    /// into the gitignore attribution chain — otherwise the skip
    /// lands in the fallback `SkipReason::Gitignore` bucket but for
    /// a phantom reason. Regression backstop for the
    /// `build_positive_only_matcher` extension that reads `.ignore`
    /// alongside `.gitignore` / `.synsignore`.
    #[test]
    fn dot_ignore_file_is_attributed_to_gitignore_reason() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("keep.txt"), "k").unwrap();
        std::fs::write(dir.path().join("noisy.log"), "n").unwrap();
        // Source-local `.ignore` file (NOT `.gitignore`) excludes the
        // log file. The kept walker honours this via standard_filters;
        // the attribution chain must too.
        std::fs::write(dir.path().join(".ignore"), "*.log\n").unwrap();

        let result = collect_files(dir.path(), &[], CollectOptions::default()).unwrap();
        assert!(result.files.contains_key("keep.txt"));
        assert!(!result.files.contains_key("noisy.log"));
        assert_eq!(result.skipped.len(), 1);
        assert_eq!(
            result.skipped[0],
            SkippedFile {
                path: "noisy.log".into(),
                reason: SkipReason::Gitignore,
            }
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
                prefix: None,
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

    // ---- u255: prefix-confined collection --------------------------

    /// A repository root holding `root-a.md`, `root-b.md` and
    /// `sub/nested.md` — the tree SPEC u255 § Tests is written over.
    fn scoped_tree() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("root-a.md"), "a").unwrap();
        std::fs::write(dir.path().join("root-b.md"), "b").unwrap();
        std::fs::create_dir_all(dir.path().join("sub")).unwrap();
        std::fs::write(dir.path().join("sub/nested.md"), "n").unwrap();
        dir
    }

    #[test]
    fn a_prefixed_collection_keys_paths_relative_to_the_walk_root() {
        let dir = scoped_tree();

        let result = collect_files(
            dir.path(),
            &[],
            CollectOptions {
                prefix: Some("sub".into()),
                ..Default::default()
            },
        )
        .unwrap();

        assert_eq!(
            result.files.keys().collect::<Vec<_>>(),
            vec!["sub/nested.md"]
        );
    }

    #[test]
    fn a_prefixed_collection_counts_no_out_of_prefix_path_as_dropped() {
        let dir = scoped_tree();
        // A root-level ignore file that would drop `root-b.md` if the
        // enumeration walk still visited it. Under `--strict` a single
        // dropped file refuses the whole publication, so an
        // out-of-prefix path leaking into `skipped` would make every
        // scoped strict publication impossible.
        std::fs::write(dir.path().join(".synsignore"), "root-b.md\n").unwrap();

        let result = collect_files(
            dir.path(),
            &[],
            CollectOptions {
                prefix: Some("sub".into()),
                ..Default::default()
            },
        )
        .unwrap();

        assert!(
            result.skipped.is_empty(),
            "skipped was {:?}",
            result.skipped
        );
        assert_eq!(result.total_walked, 1);
    }

    #[test]
    fn a_prefixed_collection_applies_the_walk_root_ignore_file() {
        let dir = scoped_tree();
        std::fs::write(dir.path().join(".synsignore"), "*.log\n").unwrap();
        std::fs::write(dir.path().join("sub/drop.log"), "l").unwrap();

        let result = collect_files(
            dir.path(),
            &[],
            CollectOptions {
                prefix: Some("sub".into()),
                ..Default::default()
            },
        )
        .unwrap();

        assert!(result.files.contains_key("sub/nested.md"));
        assert!(
            !result.files.keys().any(|p| p.ends_with(".log")),
            "files were {:?}",
            result.files.keys().collect::<Vec<_>>()
        );
    }

    #[test]
    fn a_prefix_naming_one_file_collects_that_path_alone() {
        let dir = scoped_tree();
        std::fs::write(dir.path().join("sub/sibling.md"), "s").unwrap();

        let result = collect_files(
            dir.path(),
            &[],
            CollectOptions {
                prefix: Some("sub/nested.md".into()),
                ..Default::default()
            },
        )
        .unwrap();

        assert_eq!(
            result.files.keys().collect::<Vec<_>>(),
            vec!["sub/nested.md"]
        );
        assert!(result.skipped.is_empty());
    }

    #[test]
    fn a_prefix_admits_no_sibling_sharing_its_leading_string() {
        let dir = scoped_tree();
        std::fs::create_dir_all(dir.path().join("sub2")).unwrap();
        std::fs::write(dir.path().join("sub2/other.md"), "o").unwrap();

        let result = collect_files(
            dir.path(),
            &[],
            CollectOptions {
                prefix: Some("sub".into()),
                ..Default::default()
            },
        )
        .unwrap();

        assert_eq!(
            result.files.keys().collect::<Vec<_>>(),
            vec!["sub/nested.md"]
        );
    }
}
