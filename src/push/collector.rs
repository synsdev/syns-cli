use crate::errors::CliError;
use crate::push::hash::{blob_sha1, hash_pieces};
use crate::push::working_copy::StatRecord;
use crate::repo::root::{path_is_prefix_ancestor, path_within_prefix, to_forward_slash};
use ignore::gitignore::{Gitignore, GitignoreBuilder};
use ignore::overrides::OverrideBuilder;
use ignore::{Match, WalkBuilder};
use serde::Serialize;
use std::borrow::Cow;
use std::collections::{HashMap, HashSet};
use std::ffi::OsStr;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::sync::{OwnedSemaphorePermit, Semaphore};

/// `LIM-file-size` in bytes: the largest file one publication carries.
/// A file of exactly this size is collected, one a byte larger is dropped
/// unread (SPEC u280, `D-088`).
pub const MAX_FILE_BYTES: u64 = 26_214_400;

/// The file content one run holds in memory at once, beyond the one batch
/// it is sending and the one text merge it is computing (`D-091`). Fixed,
/// never derived from the host's memory.
pub const HELD_BYTES_BUDGET: u64 = 67_108_864;

/// The piece a file is read, hashed or copied in where its bytes are not
/// held whole.
pub(crate) const PIECE_BYTES: usize = 64 * 1024;

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

/// Whether `bytes` are text: valid UTF-8 as a whole, holding no NUL byte.
/// The one test that chooses a push entry's wire field, admits a text
/// merge and reads a not-text answer (SPEC u280, `D-088`) — no prefix of
/// the content decides it.
pub fn is_text(bytes: &[u8]) -> bool {
    !bytes.contains(&0) && std::str::from_utf8(bytes).is_ok()
}

/// What `is_text` answers over everything `reader` yields, holding one
/// piece of it at a time. A character split across two pieces is carried
/// into the next rather than read as invalid.
pub fn is_text_reader<R: Read>(mut reader: R) -> std::io::Result<bool> {
    let mut buf = vec![0u8; PIECE_BYTES + 4];
    let mut carried = 0usize;
    loop {
        let n = match reader.read(&mut buf[carried..carried + PIECE_BYTES]) {
            Ok(n) => n,
            Err(err) if err.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(err) => return Err(err),
        };
        if n == 0 {
            // A character still open at the end is invalid UTF-8.
            return Ok(carried == 0);
        }
        let filled = carried + n;
        if buf[carried..filled].contains(&0) {
            return Ok(false);
        }
        match std::str::from_utf8(&buf[..filled]) {
            Ok(_) => carried = 0,
            // An incomplete character at the end of this piece: carry it.
            Err(err) if err.error_len().is_none() => {
                let valid = err.valid_up_to();
                buf.copy_within(valid..filled, 0);
                carried = filled - valid;
            }
            Err(_) => return Ok(false),
        }
    }
}

/// The count of file-content bytes one run holds in memory, and the
/// limit that count never passes (`D-091`). A run builds one at
/// `HELD_BYTES_BUDGET`; every piece of content it keeps past the step
/// reading it stands under a `Hold` taken from it.
#[derive(Debug)]
pub struct HeldBytes {
    permits: Arc<Semaphore>,
    limit: u64,
}

/// Room for `n` bytes of content, given back when dropped. Neither
/// `Clone` nor `Copy`, so a hold is never given back twice.
#[derive(Debug)]
pub struct Hold {
    _permit: Option<OwnedSemaphorePermit>,
}

impl HeldBytes {
    pub fn new(limit: u64) -> Arc<HeldBytes> {
        // A semaphore's permits are bounded well above any budget a run
        // takes; a larger limit is clamped to that bound.
        let permits = usize::try_from(limit)
            .unwrap_or(usize::MAX)
            .min(Semaphore::MAX_PERMITS);
        Arc::new(HeldBytes {
            permits: Arc::new(Semaphore::new(permits)),
            limit,
        })
    }

    /// A hold on `n` bytes where the holds outstanding leave room for
    /// them, and none otherwise. Nothing is ever waited for; empty
    /// content holds nothing and is always admitted.
    pub fn try_hold(self: &Arc<Self>, n: u64) -> Option<Hold> {
        if n == 0 {
            return Some(Hold { _permit: None });
        }
        if n > self.limit {
            return None;
        }
        let count = u32::try_from(n).ok()?;
        self.permits
            .clone()
            .try_acquire_many_owned(count)
            .ok()
            .map(|permit| Hold {
                _permit: Some(permit),
            })
    }

    /// The limit this budget was built at.
    pub fn limit(&self) -> u64 {
        self.limit
    }
}

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
/// SPEC u280: `TooLarge > DefaultExcludeDir > UserExclude > Gitignore >
/// Synsignore`, and no reason names content.
///
/// IMPORTANT: declaration order is contractual. The enum is cast as
/// `u8` (sort key of `collect_files`' `skipped`) and as `usize` (group
/// index in `write_skip_summary` and `skip_summary_cause`).
#[repr(u8)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SkipReason {
    TooLarge,
    DefaultExcludeDir,
    UserExclude,
    Gitignore,
    Synsignore,
}

/// The summary source of the size drop: `larger than {n} MiB`, `{n}` the
/// whole MiB `MAX_FILE_BYTES` holds.
pub fn too_large_label() -> String {
    format!("larger than {} MiB", MAX_FILE_BYTES / (1024 * 1024))
}

impl SkipReason {
    /// The `--debug` source naming the rule that dropped a file.
    pub fn debug_source(&self) -> &'static str {
        match self {
            SkipReason::TooLarge => "size-limit",
            SkipReason::DefaultExcludeDir => "default-excludes",
            SkipReason::UserExclude => "--exclude",
            SkipReason::Gitignore => ".gitignore",
            SkipReason::Synsignore => ".synsignore",
        }
    }
}

impl std::fmt::Display for SkipReason {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SkipReason::TooLarge => write!(f, "{}", too_large_label()),
            SkipReason::DefaultExcludeDir => write!(f, "default-excluded directory"),
            SkipReason::UserExclude => write!(f, "--exclude pattern"),
            SkipReason::Gitignore => write!(f, ".gitignore rule"),
            SkipReason::Synsignore => write!(f, ".synsignore rule"),
        }
    }
}

/// One excluded file plus its attribution.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SkippedFile {
    pub path: String,
    pub reason: SkipReason,
}

/// One kept file as a collection took it: its blob hash, and its bytes
/// where the collection read them and the run's budget admitted them.
#[derive(Debug)]
pub struct CollectedFile {
    pub sha: String,
    pub bytes: Option<(Vec<u8>, Hold)>,
}

impl CollectedFile {
    /// The file keeping its hash and giving back the bytes it held.
    pub fn drop_bytes(&mut self) {
        self.bytes = None;
    }
}

/// Output of `collect_files` — kept files plus every excluded file
/// plus the count of file leaves the walker actually visited.
#[derive(Debug, Default)]
pub struct CollectResult {
    pub files: HashMap<String, CollectedFile>,
    pub skipped: Vec<SkippedFile>,
    pub total_walked: usize,
}

impl CollectResult {
    /// Each kept path mapped to its blob hash.
    pub fn hashes(&self) -> HashMap<String, String> {
        self.files
            .iter()
            .map(|(path, file)| (path.clone(), file.sha.clone()))
            .collect()
    }
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

/// The blob hash of the file at `path`, read in pieces under the header
/// `blob_sha1` writes, and the length it read. `None` where the file's
/// length moved while it was read.
fn hash_file_in_pieces(path: &Path) -> std::io::Result<Option<(String, u64)>> {
    let file = std::fs::File::open(path)?;
    let declared = file.metadata()?.len();
    Ok(hash_pieces(file, declared, &mut std::io::sink())?.map(|sha| (sha, declared)))
}

/// The blob hash of a file a collection does not hold, retried while its
/// length moves under the read.
fn hash_unheld(path: &Path) -> std::io::Result<String> {
    for _ in 0..3 {
        if let Some((sha, _)) = hash_file_in_pieces(path)? {
            return Ok(sha);
        }
    }
    Err(std::io::Error::other(
        "the file kept changing while it was read",
    ))
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
/// attributed list of every file the walker rejected (SPEC u280
/// `collect_files` 1–5).
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
///    reason via a flat positive-only matcher. It reads no file's
///    bytes: a file a rule leaves out is attributed to that rule alone.
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
///
/// A kept file larger than `MAX_FILE_BYTES` is dropped unread as
/// `TooLarge`. Every other kept file answers its blob hash — from the
/// `record` entry that trusts it where a record is handed in, and
/// otherwise from one read of its bytes, which are held where `held`
/// admits their size and hashed in pieces where it does not.
pub fn collect_files(
    path: &Path,
    excludes: &[String],
    opts: CollectOptions,
    record: Option<&mut StatRecord>,
    held: &Arc<HeldBytes>,
) -> Result<CollectResult, CliError> {
    let mut record = record;
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

    let mut kept_files: HashMap<String, CollectedFile> = HashMap::new();
    let mut too_large: HashSet<String> = HashSet::new();
    let mut reached: HashSet<String> = HashSet::new();

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
        let unreadable = |err: std::io::Error| CliError::Io {
            message: format!("could not read {rel_path_str}: {err}"),
        };

        // 1 — the size, symbolic links followed, and the size drop.
        let before = std::fs::metadata(entry.path()).map_err(unreadable)?;
        if before.len() > MAX_FILE_BYTES {
            too_large.insert(rel_path_str);
            continue;
        }

        // 2 — the hash a trusted record entry answers, no byte read.
        if let Some(sha) = record
            .as_deref()
            .and_then(|r| r.trusted(&rel_path_str, &before))
        {
            reached.insert(rel_path_str.clone());
            kept_files.insert(rel_path_str, CollectedFile { sha, bytes: None });
            continue;
        }

        // 3 — one read, held where the budget admits the size.
        let file = match held.try_hold(before.len()) {
            Some(hold) => {
                let bytes = std::fs::read(entry.path()).map_err(unreadable)?;
                let sha = blob_sha1(&bytes);
                // The file moved under the read: hold what it now holds,
                // or keep its hash alone.
                let hold = if bytes.len() as u64 == before.len() {
                    Some(hold)
                } else {
                    drop(hold);
                    held.try_hold(bytes.len() as u64)
                };
                CollectedFile {
                    sha,
                    bytes: hold.map(|hold| (bytes, hold)),
                }
            }
            None => CollectedFile {
                sha: hash_unheld(entry.path()).map_err(unreadable)?,
                bytes: None,
            },
        };
        if let Some(record) = record.as_deref_mut()
            && let Ok(after) = std::fs::metadata(entry.path())
        {
            record.observe(&rel_path_str, &before, &after, &file.sha);
        }
        reached.insert(rel_path_str.clone());
        kept_files.insert(rel_path_str, file);
    }

    // 4 — the record keeps no entry for a path this collection did not
    // reach.
    if let Some(record) = record {
        record.entries.retain(|path, _| reached.contains(path));
    }

    // 5 — attribution. Walk 2: full enumeration — yields every file
    // regardless of hierarchical filters (only .git is excluded
    // unconditionally). The difference between this set and the kept set
    // identifies files the kept walker excluded; none of their bytes is
    // read. The positive-only matchers disambiguate Gitignore vs
    // Synsignore; the gitignore chain also folds in `.ignore` files (M2
    // case 1).
    let attribution_gitignore =
        build_positive_only_matcher(path, ".gitignore", &[".ignore"], opts.debug);
    let attribution_synsignore = build_positive_only_matcher(path, ".synsignore", &[], opts.debug);
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
            continue;
        }

        let reason = if too_large.contains(&rel_path_str) {
            SkipReason::TooLarge
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
            eprintln!(
                "[debug] skip {rel_path_str}: {reason} ({})",
                reason.debug_source()
            );
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

/// The refusal a file taken after its collection answers where it is gone
/// or no longer hashes to what the collection took.
fn changed_since_collection(path: &str) -> CliError {
    CliError::CollectedSetChanged {
        paths: vec![path.to_string()],
    }
}

/// A collected file's bytes: the ones the collection holds, or the ones
/// the folder holds now where they hash to what the collection took. A
/// caller keeps bytes read from the folder only as the batch it fills, as
/// the local side of the one text merge being computed, or under a hold
/// the run's budget admitted.
pub fn read_collected<'a>(
    root: &Path,
    path: &str,
    file: &'a CollectedFile,
) -> Result<Cow<'a, [u8]>, CliError> {
    if let Some((bytes, _)) = &file.bytes {
        return Ok(Cow::Borrowed(bytes));
    }
    let bytes = std::fs::read(root.join(path)).map_err(|_| changed_since_collection(path))?;
    if blob_sha1(&bytes) != file.sha {
        return Err(changed_since_collection(path));
    }
    Ok(Cow::Owned(bytes))
}

/// Make `dest` hold a collected file's bytes — the held ones, or the
/// folder's copied in pieces — only where they hash to what the
/// collection took; otherwise refuse, leaving no `dest`. `dest` is created
/// anew, reachable by the person's own account alone on unix (CR1-3).
pub fn copy_collected(
    root: &Path,
    path: &str,
    file: &CollectedFile,
    dest: &Path,
) -> Result<(), CliError> {
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let written = options.open(dest).and_then(|mut to| {
        if let Some((bytes, _)) = &file.bytes {
            to.write_all(bytes).and_then(|()| to.flush()).map(|()| true)
        } else {
            copy_verified(&root.join(path), &mut to, &file.sha)
        }
    });
    match written {
        Ok(true) => Ok(()),
        Ok(false) => {
            let _ = std::fs::remove_file(dest);
            Err(changed_since_collection(path))
        }
        Err(err) => {
            let _ = std::fs::remove_file(dest);
            if root.join(path).is_file() {
                Err(CliError::Io {
                    message: format!("could not write {}: {err}", dest.display()),
                })
            } else {
                Err(changed_since_collection(path))
            }
        }
    }
}

/// Copy `source` into `to` in pieces, answering whether what was copied
/// hashes to `sha`.
fn copy_verified(source: &Path, to: &mut std::fs::File, sha: &str) -> std::io::Result<bool> {
    let from = std::fs::File::open(source)?;
    let declared = from.metadata()?.len();
    Ok(hash_pieces(from, declared, to)?.as_deref() == Some(sha))
}

/// Maximum file paths shown per category in the skip-summary block
/// before the `+K more` truncation suffix kicks in (SPEC u213 § 3.4).
/// The too-large line names every path it counts (SPEC u280).
pub const MAX_PER_CATEGORY: usize = 5;

/// The hint a run writes where a file was dropped for its size and
/// `--strict` does not stand.
pub const STRICT_HINT: &str =
    "  hint: pass --strict to fail the push when a file is too large to publish";

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
        (TooLarge, Vec::new()),
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
        if *reason == TooLarge {
            writeln!(out, "  {reason} ({total}): {}", paths.join(", "))?;
            continue;
        }
        let shown: Vec<&str> = paths.iter().take(MAX_PER_CATEGORY).copied().collect();
        let joined = shown.join(", ");
        if total > MAX_PER_CATEGORY {
            let more = total - MAX_PER_CATEGORY;
            writeln!(out, "  {reason} ({total}): {joined}, +{more} more")?;
        } else {
            writeln!(out, "  {reason} ({total}): {joined}")?;
        }
    }

    // Conditional hints (SPEC u280, the drop labels).
    if !strict && skipped.iter().any(|sf| sf.reason == TooLarge) {
        writeln!(out, "{STRICT_HINT}")?;
    }
    if !no_default_excludes && skipped.iter().any(|sf| sf.reason == DefaultExcludeDir) {
        writeln!(
            out,
            "  hint: pass --no-default-excludes to include build / cache directories in the push"
        )?;
    }

    Ok(())
}

/// The too-large line of the drop summary alone, where `skipped` holds a
/// too-large drop — the one line a convergence writes before its
/// publication's own summary (SPEC u280 `converge` 3).
pub fn too_large_line(skipped: &[SkippedFile]) -> Option<String> {
    let paths: Vec<&str> = skipped
        .iter()
        .filter(|sf| sf.reason == SkipReason::TooLarge)
        .map(|sf| sf.path.as_str())
        .collect();
    (!paths.is_empty()).then(|| {
        format!(
            "  {} ({}): {}",
            too_large_label(),
            paths.len(),
            paths.join(", ")
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn collect(
        path: &Path,
        excludes: &[String],
        opts: CollectOptions,
    ) -> Result<CollectResult, CliError> {
        collect_files(
            path,
            excludes,
            opts,
            None,
            &HeldBytes::new(HELD_BYTES_BUDGET),
        )
    }

    fn held_bytes(file: &CollectedFile) -> &[u8] {
        &file
            .bytes
            .as_ref()
            .expect("the collection holds the bytes")
            .0
    }

    #[test]
    fn collects_all_files_in_directory() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.txt"), "hello").unwrap();
        std::fs::create_dir_all(dir.path().join("sub")).unwrap();
        std::fs::write(dir.path().join("sub/b.txt"), "world").unwrap();
        std::fs::create_dir_all(dir.path().join("sub/deep")).unwrap();
        std::fs::write(dir.path().join("sub/deep/c.txt"), "nested").unwrap();

        let files = collect(dir.path(), &[], CollectOptions::default())
            .unwrap()
            .files;
        assert_eq!(files.len(), 3);
        assert_eq!(held_bytes(files.get("a.txt").unwrap()), b"hello");
        assert_eq!(held_bytes(files.get("sub/b.txt").unwrap()), b"world");
        assert_eq!(held_bytes(files.get("sub/deep/c.txt").unwrap()), b"nested");
    }

    #[test]
    fn respects_gitignore_patterns() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("keep.txt"), "keep").unwrap();
        std::fs::write(dir.path().join("skip.log"), "skip").unwrap();
        std::fs::write(dir.path().join(".gitignore"), "*.log").unwrap();

        let files = collect(dir.path(), &[], CollectOptions::default())
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

        let files = collect(dir.path(), &[], CollectOptions::default())
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

        let files = collect(
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

        let files = collect(dir.path(), &[], CollectOptions::default())
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

        let files = collect(dir.path(), &[], CollectOptions::default())
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

        let files = collect(dir.path(), &[], CollectOptions::default())
            .unwrap()
            .files;
        assert_eq!(files.len(), 2);
        assert_eq!(
            held_bytes(files.get("real.txt").unwrap()),
            b"target content"
        );
        assert_eq!(
            held_bytes(files.get("link.txt").unwrap()),
            b"target content"
        );
    }

    #[test]
    fn binary_files_are_collected() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("text.txt"), "hello").unwrap();
        std::fs::write(dir.path().join("binary.bin"), b"binary\x00data").unwrap();

        let result = collect(dir.path(), &[], CollectOptions::default()).unwrap();
        assert_eq!(result.files.len(), 2);
        assert_eq!(held_bytes(result.files.get("text.txt").unwrap()), b"hello");
        assert_eq!(
            held_bytes(result.files.get("binary.bin").unwrap()),
            b"binary\x00data"
        );
        assert!(result.skipped.is_empty());
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

        let files = collect(dir.path(), &[], CollectOptions::default())
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

        let files = collect(dir.path(), &[], CollectOptions::default())
            .unwrap()
            .files;

        assert_eq!(files.len(), 2);
        assert!(files.contains_key("node_modules"));
        assert!(files.contains_key("keep.txt"));
    }

    // ===== New u213 tests =====

    #[test]
    fn a_file_past_the_limit_is_dropped_as_too_large() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("text.txt"), "hello").unwrap();
        let exact = std::fs::File::create(dir.path().join("exact.bin")).unwrap();
        exact.set_len(MAX_FILE_BYTES).unwrap();
        let over = std::fs::File::create(dir.path().join("over.bin")).unwrap();
        over.set_len(MAX_FILE_BYTES + 1).unwrap();

        let result = collect(dir.path(), &[], CollectOptions::default()).unwrap();
        assert_eq!(result.files.len(), 2);
        assert!(result.files.contains_key("exact.bin"));
        assert_eq!(
            result.skipped,
            vec![SkippedFile {
                path: "over.bin".into(),
                reason: SkipReason::TooLarge,
            }]
        );
        assert_eq!(result.total_walked, 3);
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

        let result = collect(dir.path(), &[], CollectOptions::default()).unwrap();
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

        let result = collect(dir.path(), &["*.csv".into()], CollectOptions::default()).unwrap();
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

        let result = collect(dir.path(), &[], CollectOptions::default()).unwrap();
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

        let result = collect(dir.path(), &[], CollectOptions::default()).unwrap();
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

        let result = collect(dir.path(), &[], CollectOptions::default()).unwrap();
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

        let result = collect(dir.path(), &[], CollectOptions::default()).unwrap();
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
    fn a_file_a_rule_leaves_out_is_attributed_to_that_rule_whatever_it_holds() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("dist")).unwrap();
        std::fs::write(dir.path().join("dist/binary.bin"), b"data\x00").unwrap();
        let big = std::fs::File::create(dir.path().join("dist/big.bin")).unwrap();
        big.set_len(MAX_FILE_BYTES + 1).unwrap();

        let result = collect(dir.path(), &[], CollectOptions::default()).unwrap();
        assert_eq!(result.skipped.len(), 2);
        assert!(
            result
                .skipped
                .iter()
                .all(|s| s.reason == SkipReason::DefaultExcludeDir)
        );
    }

    #[test]
    fn no_default_excludes_flag_re_includes_build_dirs() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("keep.txt"), "k").unwrap();
        std::fs::create_dir_all(dir.path().join("dist")).unwrap();
        std::fs::write(dir.path().join("dist/index.html"), "h").unwrap();
        std::fs::create_dir_all(dir.path().join("build")).unwrap();
        std::fs::write(dir.path().join("build/output.js"), "o").unwrap();

        let result = collect(
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

        let result = collect(
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

        let result = collect(
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

        let result = collect(
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

        let result = collect(
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

        let result = collect(
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

        let result = collect(
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

        let result = collect(
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

    // ---- u280: one text test, the held-bytes budget, the record -------

    #[test]
    fn a_nul_past_any_prefix_is_not_text() {
        let mut bytes = "a".repeat(97_778).into_bytes();
        bytes.push(0);
        bytes.extend_from_slice(b"tail");
        assert!(!is_text(&bytes));
        assert!(!is_text_reader(bytes.as_slice()).unwrap());
        assert!(is_text(b"plain\n"));
    }

    #[test]
    fn latin1_is_not_text() {
        assert!(!is_text(b"caf\xe9\n"));
        assert!(!is_text_reader(&b"caf\xe9\n"[..]).unwrap());
    }

    /// A reader handing one byte at a time splits every multi-byte
    /// character across pieces.
    struct Trickle<'a>(&'a [u8]);

    impl Read for Trickle<'_> {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            match self.0.split_first() {
                Some((first, rest)) if !buf.is_empty() => {
                    buf[0] = *first;
                    self.0 = rest;
                    Ok(1)
                }
                _ => Ok(0),
            }
        }
    }

    #[test]
    fn a_character_split_across_pieces_is_still_text() {
        let text = "caf\u{e9} \u{6771}\u{4eac} \u{1f600}\n".as_bytes();
        assert!(is_text_reader(Trickle(text)).unwrap());
        // A character left open at the end is not.
        assert!(!is_text_reader(Trickle(&text[..text.len() - 3])).unwrap());
        assert!(!is_text_reader(Trickle(&"\u{1f600}".as_bytes()[..2])).unwrap());
    }

    #[test]
    fn holds_sum_to_the_limit_and_no_further() {
        let held = HeldBytes::new(10);
        let a = held.try_hold(6).expect("room for six");
        let b = held.try_hold(4).expect("room for four more");
        assert!(held.try_hold(1).is_none(), "one byte past the limit");
        drop(a);
        let c = held
            .try_hold(6)
            .expect("a dropped hold gives its count back");
        drop((b, c));
        assert!(held.try_hold(11).is_none());
    }

    #[test]
    fn a_zero_budget_refuses_every_non_empty_hold() {
        let held = HeldBytes::new(0);
        assert!(held.try_hold(1).is_none());
        assert!(held.try_hold(0).is_some());
    }

    fn three_files() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.md"), "# a\n").unwrap();
        std::fs::write(dir.path().join("latin1.txt"), b"caf\xe9\n").unwrap();
        std::fs::write(
            dir.path().join("image.png"),
            b"\x89PNG\r\n\x1a\n\x00\x00\x00\rIHDR",
        )
        .unwrap();
        dir
    }

    #[test]
    fn a_zero_budget_collection_holds_nothing() {
        let dir = three_files();
        let full = collect(dir.path(), &[], CollectOptions::default()).unwrap();
        let zero = collect_files(
            dir.path(),
            &[],
            CollectOptions::default(),
            None,
            &HeldBytes::new(0),
        )
        .unwrap();

        assert_eq!(zero.files.len(), 3);
        for (path, file) in &zero.files {
            assert!(file.bytes.is_none(), "{path} held bytes");
            assert_eq!(file.sha, full.files[path].sha, "{path}");
            let bytes = read_collected(dir.path(), path, file).unwrap();
            assert_eq!(&*bytes, std::fs::read(dir.path().join(path)).unwrap());
        }
    }

    #[cfg(unix)]
    fn record_trusting(dir: &Path, path: &str, sha: &str, stamp: Option<(i64, i64)>) -> StatRecord {
        use std::os::unix::fs::MetadataExt;
        let meta = std::fs::metadata(dir.join(path)).unwrap();
        let mut record = StatRecord::default();
        record.entries.insert(
            path.to_string(),
            crate::push::working_copy::StatEntry {
                size: meta.len(),
                mtime: (meta.mtime(), meta.mtime_nsec()),
                ctime: (meta.ctime(), meta.ctime_nsec()),
                inode: meta.ino(),
                sha: sha.to_string(),
            },
        );
        record.stamp = stamp.or(Some((meta.mtime().max(meta.ctime()) + 10, 0)));
        record
    }

    #[cfg(unix)]
    #[test]
    fn a_trusted_entry_spares_the_read() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.png"), b"\x89PNG\x00").unwrap();
        let recorded = "0".repeat(40);
        let mut record = record_trusting(dir.path(), "a.png", &recorded, None);

        let result = collect_files(
            dir.path(),
            &[],
            CollectOptions::default(),
            Some(&mut record),
            &HeldBytes::new(HELD_BYTES_BUDGET),
        )
        .unwrap();

        assert_eq!(result.files["a.png"].sha, recorded);
        assert!(result.files["a.png"].bytes.is_none());
    }

    #[cfg(unix)]
    #[test]
    fn a_racy_entry_is_read_again() {
        use std::os::unix::fs::MetadataExt;
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.png"), b"\x89PNG\x00").unwrap();
        let meta = std::fs::metadata(dir.path().join("a.png")).unwrap();
        let recorded = "0".repeat(40);
        let mut record = record_trusting(
            dir.path(),
            "a.png",
            &recorded,
            Some((meta.mtime(), meta.mtime_nsec())),
        );

        let result = collect_files(
            dir.path(),
            &[],
            CollectOptions::default(),
            Some(&mut record),
            &HeldBytes::new(HELD_BYTES_BUDGET),
        )
        .unwrap();

        let actual = blob_sha1(b"\x89PNG\x00");
        assert_eq!(result.files["a.png"].sha, actual);
        assert_eq!(record.entries["a.png"].sha, actual);
    }

    /// A rewrite in place — same inode, same size — whose writer restores
    /// the modification time moves only the change time, and that alone
    /// keeps the record from sparing the read (`NR-01`).
    #[cfg(unix)]
    #[test]
    fn a_same_inode_rewrite_restoring_its_mtime_is_read_again() {
        use std::io::{Seek, Write};
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.png");
        std::fs::write(&path, b"\x89PNG\x00one").unwrap();
        let before = std::fs::metadata(&path).unwrap();
        let mut record = StatRecord::default();
        record.observe("a.png", &before, &before, &blob_sha1(b"\x89PNG\x00one"));
        record.stamp = Some((
            std::os::unix::fs::MetadataExt::mtime(&before)
                .max(std::os::unix::fs::MetadataExt::ctime(&before))
                + 10,
            0,
        ));
        std::thread::sleep(std::time::Duration::from_millis(20));

        let mut file = std::fs::OpenOptions::new().write(true).open(&path).unwrap();
        file.seek(std::io::SeekFrom::Start(0)).unwrap();
        file.write_all(b"\x89PNG\x00two").unwrap();
        file.set_times(std::fs::FileTimes::new().set_modified(before.modified().unwrap()))
            .unwrap();
        drop(file);
        let after = std::fs::metadata(&path).unwrap();
        assert_eq!(after.len(), before.len());
        assert_eq!(after.modified().unwrap(), before.modified().unwrap());

        let result = collect_files(
            dir.path(),
            &[],
            CollectOptions::default(),
            Some(&mut record),
            &HeldBytes::new(HELD_BYTES_BUDGET),
        )
        .unwrap();

        assert_eq!(result.files["a.png"].sha, blob_sha1(b"\x89PNG\x00two"));
    }

    #[test]
    fn a_file_changed_since_collection_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.png"), b"\x89PNG\x00").unwrap();
        let file = CollectedFile {
            sha: blob_sha1(b"\x89PNG\x00"),
            bytes: None,
        };
        std::fs::write(dir.path().join("a.png"), b"\x89PNG\x01").unwrap();

        match read_collected(dir.path(), "a.png", &file) {
            Err(CliError::CollectedSetChanged { paths }) => assert_eq!(paths, vec!["a.png"]),
            other => panic!("expected CollectedSetChanged, got {other:?}"),
        }
        let dest = dir.path().join("copy");
        assert!(matches!(
            copy_collected(dir.path(), "a.png", &file, &dest),
            Err(CliError::CollectedSetChanged { .. })
        ));
        assert!(!dest.exists());
    }

    /// CR1-3: a collected file's copy is created private, held bytes and
    /// bytes read again alike.
    #[cfg(unix)]
    #[test]
    fn a_collected_copy_is_created_reachable_by_its_owner_alone() {
        use std::os::unix::fs::PermissionsExt;
        let dir = three_files();
        for budget in [HELD_BYTES_BUDGET, 0] {
            let collected = collect_files(
                dir.path(),
                &[],
                CollectOptions::default(),
                None,
                &HeldBytes::new(budget),
            )
            .unwrap();
            let dest = dir.path().join(format!("copy-{budget}"));
            copy_collected(
                dir.path(),
                "image.png",
                &collected.files["image.png"],
                &dest,
            )
            .unwrap();
            let mode = std::fs::metadata(&dest).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600, "budget {budget}");
            std::fs::remove_file(&dest).unwrap();
        }
    }

    #[test]
    fn the_summary_names_every_too_large_path_and_no_binary_hint() {
        let skipped: Vec<SkippedFile> = (0..7)
            .map(|i| SkippedFile {
                path: format!("big{i}.bin"),
                reason: SkipReason::TooLarge,
            })
            .collect();
        let mut out = String::new();
        write_skip_summary(&mut out, &skipped, false, false).unwrap();
        assert!(out.contains("larger than 25 MiB (7): big0.bin, big1.bin, big2.bin, big3.bin, big4.bin, big5.bin, big6.bin\n"), "{out}");
        assert!(out.contains(STRICT_HINT), "{out}");
        assert!(!out.contains("more"), "{out}");
        assert!(!out.contains("binary"), "{out}");

        let ignored = vec![SkippedFile {
            path: "x.log".into(),
            reason: SkipReason::Gitignore,
        }];
        let mut out = String::new();
        write_skip_summary(&mut out, &ignored, false, false).unwrap();
        assert!(!out.contains("hint"), "{out}");
    }
}
