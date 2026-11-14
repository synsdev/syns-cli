//! The per-working-copy state a convergence keeps outside the folder
//! (SPEC u256 § Contract Surface, `WorkingCopy`).
//!
//! One state directory per repository and canonical root, under the
//! cache root, holding the state lock, the recorded base, a pending
//! resolution, the publication outbox and the two private snapshots.
//! SPEC u298: where the default root refuses a run's writes, the copy's
//! state is written to its in-root home, `{canonical root}/.syns-state`,
//! instead; every run reads and locks both homes, the state stamped last
//! answering its reads and adopted by the next lock.
//! Every file under it is written through `write_atomic`: beside its
//! target, flushed, renamed over the target and its directory flushed,
//! so a killed process or a lost machine leaves the version before a
//! write or the one after it, never a torn one.

use std::collections::{BTreeMap, HashMap};
use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

use crate::config::{StoreRoots, is_foreign_or_link};
use crate::errors::CliError;
use crate::push::hash::blob_sha1;
use crate::push::manifest::Manifest;
use crate::push::reconcile::CollisionKind;
use crate::repo::folder::{FolderScope, lies_under};

const LOCK_FILE: &str = "state.lock";
const BASE_FILE: &str = "base.json";
const RESOLUTION_FILE: &str = "resolution.json";
const OUTBOX_FILE: &str = "outbox.json";
const LOCAL_SNAPSHOT_FILE: &str = "local-snapshot.json";
const REMOTE_SNAPSHOT_FILE: &str = "remote-snapshot.json";
const STAT_RECORD_FILE: &str = "stat-record.json";
/// Issue 173, D-163: the identity of the folder the default home's state
/// was recorded for.
const FOLDER_FILE: &str = "folder.json";

/// SPEC u298, `IN_ROOT_HOME`: the directory at a working copy's root its
/// state is kept in where the default root refuses a run's writes. A path
/// holding this segment, letter case aside, is collected, retrieved and
/// placed by nothing.
pub const IN_ROOT_HOME: &str = ".syns-state";

/// SPEC u298, `STATE_STAMP`: the nanoseconds since the Unix epoch a
/// home's state last changed, rewritten under the lock at every write or
/// removal of a state file.
pub const STATE_STAMP: &str = "state.stamp";

/// The local record's file name inside an in-root home (SPEC u298,
/// `Manifest::save`).
pub(crate) const LOCAL_RECORD_FILE: &str = "local-record.json";

const GITIGNORE_FILE: &str = ".gitignore";

/// The state files the `WorkingCopy` reads answer from, beside every
/// content under `snapshot-content`.
const READ_FILES: [&str; 6] = [
    BASE_FILE,
    RESOLUTION_FILE,
    OUTBOX_FILE,
    LOCAL_SNAPSHOT_FILE,
    REMOTE_SNAPSHOT_FILE,
    STAT_RECORD_FILE,
];

#[cfg(unix)]
const IN_ROOT_HOME_MODE: u32 = 0o700;

/// Whether a served or collected path holds the in-root home as one of
/// its segments, under either separator and letter case aside.
pub(crate) fn holds_in_root_home(path: &str) -> bool {
    path.split(['/', '\\'])
        .any(|seg| seg.eq_ignore_ascii_case(IN_ROOT_HOME))
}

/// Why Windows cannot hold a served path as a file on disk, none where it
/// can: a segment ending in a space or a dot, which Windows drops and so
/// names another file; a reserved device name, with or without an
/// extension; or a character Windows refuses in a name, `\` among them,
/// which it reads as a separator. Answered on every platform, so the rule
/// is tested everywhere; `not_held_here` applies it on Windows alone.
pub(crate) fn not_held_by_windows(path: &str) -> Option<String> {
    const RESERVED: [&str; 32] = [
        "CON",
        "PRN",
        "AUX",
        "NUL",
        "COM0",
        "COM1",
        "COM2",
        "COM3",
        "COM4",
        "COM5",
        "COM6",
        "COM7",
        "COM8",
        "COM9",
        "LPT0",
        "LPT1",
        "LPT2",
        "LPT3",
        "LPT4",
        "LPT5",
        "LPT6",
        "LPT7",
        "LPT8",
        "LPT9",
        "COM\u{B9}",
        "COM\u{B2}",
        "COM\u{B3}",
        "LPT\u{B9}",
        "LPT\u{B2}",
        "LPT\u{B3}",
        "CONIN$",
        "CONOUT$",
    ];
    for segment in path.split('/') {
        if let Some(c) = segment
            .chars()
            .find(|c| matches!(c, '<' | '>' | ':' | '"' | '\\' | '|' | '?' | '*'))
        {
            return Some(format!("a name holding {c:?}"));
        }
        if segment.ends_with(' ') {
            return Some("a name ending in a space".to_string());
        }
        if segment.ends_with('.') {
            return Some("a name ending in a dot".to_string());
        }
        let stem = segment
            .split('.')
            .next()
            .unwrap_or(segment)
            .trim_end_matches(' ');
        if RESERVED.iter().any(|name| name.eq_ignore_ascii_case(stem)) {
            return Some(format!("the reserved name {stem}"));
        }
    }
    None
}

/// Why this platform's file system cannot hold a served path, none where
/// it can (`not_held_by_windows`, on Windows alone).
pub(crate) fn not_held_here(path: &str) -> Option<String> {
    if cfg!(windows) {
        not_held_by_windows(path)
    } else {
        None
    }
}

/// Whether a served path is left out of the folder: `served`, the path as
/// the request's address names it, in the in-root home, or `local`, the
/// same path counted from the scope (the place it would take on disk), a
/// name this platform cannot hold. The scope's own name is never judged:
/// it stands on disk under whatever directory the run was given. Such a
/// path reads as absent from every head a convergence or a retrieval
/// reads, so no run writes, removes, compares or publishes a deletion of
/// it, and the server keeps it as it stands. A name this platform cannot
/// hold is named once a run, on stderr, with the reason.
pub(crate) fn left_out_here(served: &str, local: &str) -> bool {
    if holds_in_root_home(served) {
        return true;
    }
    let path = local;
    let Some(reason) = not_held_here(path) else {
        return false;
    };
    static NAMED: std::sync::Mutex<std::collections::BTreeSet<String>> =
        std::sync::Mutex::new(std::collections::BTreeSet::new());
    let first = NAMED
        .lock()
        .map(|mut named| named.insert(path.to_string()))
        .unwrap_or(true);
    if first {
        eprintln!("  warning: could not write {path:?}: Windows cannot hold {reason}");
    }
    true
}

/// How a volume compares two spellings of one name: where `case` holds,
/// spellings differing in letter case alone name one file; where
/// `normalization` holds, spellings differing in Unicode normalisation
/// alone (`é` composed, `e` with a combining acute) name one file.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct Folding {
    pub(crate) case: bool,
    pub(crate) normalization: bool,
}

impl Folding {
    /// Every spelling a volume may hold as one with another: the test a
    /// run applies before it asks the volume what it does.
    pub(crate) const WIDEST: Folding = Folding {
        case: true,
        normalization: true,
    };

    /// The key every spelling of `name` this volume holds as one shares.
    pub(crate) fn key(&self, name: &str) -> String {
        let name: std::borrow::Cow<'_, str> = if self.normalization {
            icu_normalizer::ComposingNormalizerBorrowed::new_nfc().normalize(name)
        } else {
            name.into()
        };
        if self.case {
            name.to_lowercase()
        } else {
            name.into_owned()
        }
    }
}

/// The mark a probe's name carries after a minted partial-write name, in
/// the composed form the probe is created under and the decomposed form a
/// normalising volume may store it as: a probe a killed run left is swept
/// as any partial write is.
pub(crate) const PROBE_MARKS: [&str; 2] = ["\u{e9}", "e\u{301}"];

/// What the volume holding `root` does with two spellings of one name,
/// read once a run for each root by the volume itself, never assumed from
/// the platform: a probe file is created under a minted partial-write
/// name ending in `é` composed, looked up decomposed and in upper case,
/// and removed. A root that is no directory yet, or takes no probe, reads
/// as holding every spelling apart: nothing it holds can alias, and a
/// write landing on another spelling is caught where it is made
/// (`spelled_otherwise_on_disk`).
pub(crate) fn folding_at(root: &Path) -> Folding {
    static KNOWN: std::sync::Mutex<Vec<(PathBuf, Folding)>> = std::sync::Mutex::new(Vec::new());
    if !root.is_dir() {
        return Folding::default();
    }
    let dir = root.to_path_buf();
    if let Some(known) = KNOWN.lock().ok().and_then(|known| {
        known
            .iter()
            .find(|(probed, _)| *probed == dir)
            .map(|(_, folding)| *folding)
    }) {
        return known;
    }
    let minted = mint_probe_name();
    let probe = dir.join(format!("{minted}{}", PROBE_MARKS[0]));
    let folding = match OpenOptions::new().write(true).create_new(true).open(&probe) {
        Ok(file) => {
            drop(file);
            let normalization =
                std::fs::symlink_metadata(dir.join(format!("{minted}{}", PROBE_MARKS[1]))).is_ok();
            let case =
                std::fs::symlink_metadata(dir.join(format!("{}\u{c9}", minted.to_uppercase())))
                    .is_ok();
            let _ = std::fs::remove_file(&probe);
            Folding {
                case,
                normalization,
            }
        }
        Err(_) => Folding::default(),
    };
    if let Ok(mut known) = KNOWN.lock() {
        known.push((dir, folding));
    }
    folding
}

type Listing = std::sync::Arc<std::collections::HashSet<String>>;

/// Each directory's listing as last read or kept true, and the
/// modification time it stood at.
static LISTINGS: std::sync::Mutex<Option<HashMap<PathBuf, (std::time::SystemTime, Listing)>>> =
    std::sync::Mutex::new(None);

/// The names a directory holds, as its listing spells them: read once for
/// each state of the directory, the listing kept until the directory's
/// modification time moves other than by this run's own writes
/// (`note_in_listing`).
fn listing(dir: &Path) -> Option<Listing> {
    listing_read(dir, false)
}

/// `listing`, read from the directory itself where `afresh` holds, the
/// read kept for later asks: a directory's modification time is no sure
/// sign of its entries (NTFS may leave it standing over a new entry).
fn listing_read(dir: &Path, afresh: bool) -> Option<Listing> {
    let modified = std::fs::metadata(dir)
        .and_then(|meta| meta.modified())
        .ok()?;
    if let Some(names) = (!afresh)
        .then(|| LISTINGS.lock().ok())
        .flatten()
        .and_then(|listings| {
            listings
                .as_ref()?
                .get(dir)
                .filter(|(at, _)| *at == modified)
                .map(|(_, names)| names.clone())
        })
    {
        return Some(names);
    }
    let names: std::collections::HashSet<String> = std::fs::read_dir(dir)
        .ok()?
        .filter_map(|entry| entry.ok()?.file_name().into_string().ok())
        .collect();
    let names = std::sync::Arc::new(names);
    if let Ok(mut listings) = LISTINGS.lock() {
        listings
            .get_or_insert_with(HashMap::new)
            .insert(dir.to_path_buf(), (modified, names.clone()));
    }
    Some(names)
}

/// Whether the listing of `dir` holds `name` exactly: the kept listing
/// first, then, where it lacks the name, the directory read afresh, so a
/// listing kept from before an entry stood never reads it as absent.
fn lists_exactly(dir: &Path, name: &str) -> bool {
    listing(dir).is_some_and(|names| names.contains(name))
        || listing_read(dir, true).is_some_and(|names| names.contains(name))
}

/// Keeps the listing of `dir` true to a write (`stands`) or a removal this
/// run made of `name` in it, at the directory's new modification time, so
/// a folder written file by file is not read whole after each file.
pub(crate) fn note_in_listing(dir: &Path, name: &str, stands: bool) {
    let Ok(modified) = std::fs::metadata(dir).and_then(|meta| meta.modified()) else {
        return;
    };
    if let Ok(mut listings) = LISTINGS.lock()
        && let Some((at, names)) = listings.as_mut().and_then(|listings| listings.get_mut(dir))
    {
        let kept = std::sync::Arc::make_mut(names);
        if stands {
            kept.insert(name.to_string());
        } else {
            kept.remove(name);
        }
        *at = modified;
    }
}

/// Whether the directory listing of `dir` spells a name `name` as written,
/// a composed and a decomposed spelling read alike, since a normalising
/// volume stores a name in its own form.
fn lists_as_spelled(dir: &Path, name: &str) -> bool {
    if lists_exactly(dir, name) {
        return true;
    }
    let Some(names) = listing(dir) else {
        return false;
    };
    // A name standing under another normalisation is this name only where
    // the volume rewrites every name it stores into its own form (HFS+
    // stores names decomposed); on a volume keeping names as written it is
    // another spelling, held as one with this one.
    let nfc = |n: &str| {
        icu_normalizer::ComposingNormalizerBorrowed::new_nfc()
            .normalize(n)
            .into_owned()
    };
    let wanted = nfc(name);
    names.iter().any(|n| nfc(n) == wanted) && rewrites_names(dir)
}

/// Whether the volume holding `dir` stores a name in a normalisation form
/// of its own rather than as written: asked once a run for each directory
/// it is asked about, by a probe created composed under a minted
/// partial-write name, read back from the listing, and removed.
fn rewrites_names(dir: &Path) -> bool {
    static KNOWN: std::sync::Mutex<Vec<(PathBuf, bool)>> = std::sync::Mutex::new(Vec::new());
    if let Some(known) = KNOWN.lock().ok().and_then(|known| {
        known
            .iter()
            .find(|(asked, _)| asked == dir)
            .map(|(_, rewrites)| *rewrites)
    }) {
        return known;
    }
    let minted = mint_probe_name();
    let written = format!("{minted}{}", PROBE_MARKS[0]);
    let probe = dir.join(&written);
    let rewrites = match OpenOptions::new().write(true).create_new(true).open(&probe) {
        Ok(file) => {
            drop(file);
            let stored = std::fs::read_dir(dir).ok().is_some_and(|entries| {
                let names: Vec<String> = entries
                    .filter_map(|entry| entry.ok()?.file_name().into_string().ok())
                    .collect();
                !names.contains(&written) && names.contains(&format!("{minted}{}", PROBE_MARKS[1]))
            });
            let _ = std::fs::remove_file(&probe);
            stored
        }
        Err(_) => false,
    };
    if let Ok(mut known) = KNOWN.lock() {
        known.push((dir.to_path_buf(), rewrites));
    }
    rewrites
}

/// A fresh partial-write name for a probe: `PARTIAL_PREFIX`'s form, its
/// 16 hex characters minted from the process, a counter and the clock.
fn mint_probe_name() -> String {
    static MINTED: AtomicU64 = AtomicU64::new(0);
    let seed = format!(
        "{}:{}:{:?}",
        std::process::id(),
        MINTED.fetch_add(1, Ordering::Relaxed),
        std::time::SystemTime::now()
    );
    format!(".syns-partial-{}", &blob_sha1(seed.as_bytes())[..16])
}

/// Whether something stands at `path` under `root` under that path's own
/// spelling, segment by segment: on a volume holding two spellings of one
/// name as one, `Readme.md` reaches a file standing as `README.md`, and
/// that file is no file at `Readme.md`.
pub(crate) fn stands_as_spelled(root: &Path, path: &str) -> bool {
    let mut dir = root.to_path_buf();
    for segment in path.split('/') {
        if !lists_as_spelled(&dir, segment) {
            return false;
        }
        dir.push(segment);
    }
    true
}

/// Where `path` under `root` reaches something standing under another
/// spelling: the path, counted from `root`, of the first segment the disk
/// spells otherwise, as the disk spells it (`Readme.md` where the folder
/// holds `README.md`, `Dir` where it holds `dir/`). None where every
/// segment that stands stands as spelled, or nothing stands.
pub(crate) fn spelled_otherwise_on_disk(root: &Path, path: &str) -> Option<String> {
    let mut dir = root.to_path_buf();
    let mut counted: Vec<String> = Vec::new();
    for segment in path.split('/') {
        let target = dir.join(segment);
        if std::fs::symlink_metadata(&target).is_err() {
            return None;
        }
        if !lists_as_spelled(&dir, segment) {
            // The entry the volume reached: found by the key where the key
            // reads the volume's folding, else by the file's own identity
            // (`ß` reaching `ss` on macOS, which no key here reads).
            let wanted = Folding::WIDEST.key(segment);
            let names = listing(&dir)?;
            let reached = FolderIdentity::of(&target);
            let spelled = names
                .iter()
                .find(|name| Folding::WIDEST.key(name) == wanted)
                .or_else(|| {
                    let reached = reached.as_ref()?;
                    names.iter().find(|name| {
                        FolderIdentity::of(&dir.join(name.as_str())).is_some_and(|entry| {
                            entry.dev == reached.dev && entry.ino == reached.ino
                        })
                    })
                })
                .cloned()
                .unwrap_or_else(|| segment.to_string());
            counted.push(spelled);
            return Some(counted.join("/"));
        }
        counted.push(segment.to_string());
        dir = target;
    }
    None
}

/// The spellings of `paths`, all counted from `root`, that the volume
/// holding `root` holds as one with another of them, each with the
/// spelling it is held as one with, at the folder or file where they meet.
/// Of each such set the spelling kept is the one the folder already holds
/// on disk; else the one the copy kept before, which `kept_before` answers
/// for a path or a folder (the copy's base records only the spelling it
/// kept), so a person's removal of that file is never read as the other
/// spelling's; else the bytewise-smallest, so a fresh working copy keeps
/// the same one on every machine. Every other spelling, and every path
/// under it, is in the answer. The volume is asked what it does only where
/// two spellings meet under the widest folding.
pub(crate) fn held_as_one<'a>(
    root: &Path,
    paths: impl IntoIterator<Item = &'a str>,
    kept_before: impl Fn(&str) -> bool,
) -> BTreeMap<String, String> {
    let paths: Vec<&str> = paths.into_iter().collect();
    // Every spelling of each folder or file, grouped under its parent's
    // spelling by the key the folding gives it.
    let group = |folding: Folding| {
        let mut groups: HashMap<(String, String), std::collections::BTreeSet<String>> =
            HashMap::new();
        for path in &paths {
            let mut parent = String::new();
            for segment in path.split('/') {
                groups
                    .entry((parent.clone(), folding.key(segment)))
                    .or_default()
                    .insert(segment.to_string());
                if !parent.is_empty() {
                    parent.push('/');
                }
                parent.push_str(segment);
            }
        }
        groups
    };
    if group(Folding::WIDEST)
        .values()
        .all(|spellings| spellings.len() < 2)
    {
        return BTreeMap::new();
    }
    let folding = folding_at(root);
    if folding == Folding::default() {
        return BTreeMap::new();
    }
    let groups = group(folding);
    let joined = |parent: &str, segment: &str| {
        if parent.is_empty() {
            segment.to_string()
        } else {
            format!("{parent}/{segment}")
        }
    };
    let kept = |parent: &str, spellings: &std::collections::BTreeSet<String>| -> String {
        let dir = root.join(parent);
        spellings
            .iter()
            .find(|spelling| lists_exactly(&dir, spelling))
            .or_else(|| {
                spellings
                    .iter()
                    .find(|spelling| kept_before(&joined(parent, spelling)))
            })
            .or_else(|| spellings.iter().next())
            .cloned()
            .unwrap_or_default()
    };
    let mut answer = BTreeMap::new();
    for path in &paths {
        let mut parent = String::new();
        for segment in path.split('/') {
            let spellings = &groups[&(parent.clone(), folding.key(segment))];
            if spellings.len() > 1 {
                let keep = kept(&parent, spellings);
                if keep != segment {
                    answer.insert(path.to_string(), joined(&parent, &keep));
                    break;
                }
            }
            parent = joined(&parent, segment);
        }
    }
    answer
}

/// Every served path this run left out under a root because the volume
/// holds it as one with another spelling.
static HELD_AS_ONE: std::sync::Mutex<std::collections::BTreeSet<(PathBuf, String)>> =
    std::sync::Mutex::new(std::collections::BTreeSet::new());

/// `root` as the run's record of what it left out names it.
fn root_key(root: &Path) -> PathBuf {
    std::fs::canonicalize(root).unwrap_or_else(|_| root.to_path_buf())
}

/// Leaves `path` under `root` out for the rest of the run, the volume
/// holding it as one with `kept`, and names it once, on stderr, in the
/// words of the line naming a name Windows cannot hold.
pub(crate) fn name_held_as_one(root: &Path, path: &str, kept: &str) {
    let first = HELD_AS_ONE
        .lock()
        .map(|mut named| named.insert((root_key(root), path.to_string())))
        .unwrap_or(true);
    if first {
        eprintln!(
            "  warning: could not write {path:?}: this file system holds it as one with {kept:?}"
        );
    }
}

/// Starts a run's record of what it leaves out under `root` afresh: what
/// a run left out is a fact of that run alone.
pub(crate) fn forget_held_as_one(root: &Path) {
    let root = root_key(root);
    if let Ok(mut named) = HELD_AS_ONE.lock() {
        named.retain(|(at, _)| *at != root);
    }
}

/// Whether this run left `path` under `root` out, the volume holding it
/// as one with another spelling, and it still reaches that spelling on
/// disk: every later comparison, write and removal of the run holds it out
/// too, until the file it reaches is gone.
pub(crate) fn held_out_as_one(root: &Path, path: &str) -> bool {
    HELD_AS_ONE
        .lock()
        .map(|named| named.contains(&(root_key(root), path.to_string())))
        .unwrap_or(false)
        && spelled_otherwise_on_disk(root, path).is_some()
}

/// Create the in-root home `dir` where it does not stand, at mode `0700`
/// holding a `.gitignore` whose one line is `*`, and set its mode to
/// `0700`; one standing as a link or owned by another account is refused
/// (SPEC u298 Behaviour, `WorkingCopy::open` 1).
pub(crate) fn ensure_in_root_home(dir: &Path) -> Result<(), CliError> {
    let refuse = |reason: &dyn std::fmt::Display| CliError::Io {
        message: format!(
            "could not create working copy state {}: {reason}",
            dir.display()
        ),
    };
    let foreign = "it stands as a link or is owned by another account";
    if is_foreign_or_link(dir) {
        return Err(refuse(&foreign));
    }
    if !dir.is_dir() {
        // Only the unix mode below mutates it (issue 325: Windows clippy).
        #[cfg_attr(not(unix), allow(unused_mut))]
        let mut builder = std::fs::DirBuilder::new();
        #[cfg(unix)]
        {
            use std::os::unix::fs::DirBuilderExt;
            builder.mode(IN_ROOT_HOME_MODE);
        }
        match builder.create(dir) {
            Ok(()) => {}
            Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(err) => return Err(refuse(&err)),
        }
        if is_foreign_or_link(dir) {
            return Err(refuse(&foreign));
        }
        if !dir.is_dir() {
            return Err(refuse(&"it stands as a file"));
        }
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(IN_ROOT_HOME_MODE))
            .map_err(|err| refuse(&err))?;
    }
    let ignore = dir.join(GITIGNORE_FILE);
    if !ignore.exists() {
        write_atomic(&ignore, b"*\n", flush_directory)?;
    }
    Ok(())
}

/// What tells one folder from another created later at its path (D-163):
/// its inode, and its birth time where the file system answers one, so an
/// inode a removed folder freed and a new one took still differs. The
/// device is recorded but not compared.
///
/// `unique_index` is never recorded: it is set on an identity read from
/// disk where the volume is NTFS, whose file index alone names one folder
/// (D-164), and is no part of the comparison of two records.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct FolderIdentity {
    dev: u64,
    ino: u64,
    born: Option<(u64, u32)>,
    #[serde(skip)]
    unique_index: bool,
}

impl PartialEq for FolderIdentity {
    fn eq(&self, other: &FolderIdentity) -> bool {
        (self.dev, self.ino, self.born) == (other.dev, other.ino, other.born)
    }
}

impl Eq for FolderIdentity {}

impl FolderIdentity {
    /// The identity of the folder at `root`, none where it cannot be read
    /// or the platform answers no inode or file index.
    #[cfg(unix)]
    fn of(root: &Path) -> Option<FolderIdentity> {
        use std::os::unix::fs::MetadataExt;
        let meta = std::fs::metadata(root).ok()?;
        let born = meta
            .created()
            .ok()
            .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|since| (since.as_secs(), since.subsec_nanos()));
        Some(FolderIdentity {
            dev: meta.dev(),
            ino: meta.ino(),
            born,
            unique_index: false,
        })
    }

    /// On Windows the volume serial stands for the device and the file
    /// index for the inode (D-164): the creation time alone is no identity
    /// there, since NTFS tunnelling hands a name removed and created again
    /// within 15 seconds its old creation time.
    #[cfg(windows)]
    fn of(root: &Path) -> Option<FolderIdentity> {
        use std::os::windows::fs::OpenOptionsExt;
        use std::os::windows::io::AsRawHandle;
        use windows_sys::Win32::Storage::FileSystem::{
            BY_HANDLE_FILE_INFORMATION, FILE_FLAG_BACKUP_SEMANTICS, FILE_SHARE_DELETE,
            FILE_SHARE_READ, FILE_SHARE_WRITE, GetFileInformationByHandle,
            GetVolumeInformationByHandleW,
        };
        // Backup semantics open a directory; full sharing never blocks a
        // concurrent removal or rename of the folder.
        let folder = OpenOptions::new()
            .access_mode(0)
            .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE)
            .custom_flags(FILE_FLAG_BACKUP_SEMANTICS)
            .open(root)
            .ok()?;
        // SAFETY: the handle is open for the call's length, and the
        // structure is plain data the call fills.
        let info = unsafe {
            let mut info: BY_HANDLE_FILE_INFORMATION = std::mem::zeroed();
            if GetFileInformationByHandle(folder.as_raw_handle(), &mut info) == 0 {
                return None;
            }
            info
        };
        let born = folder
            .metadata()
            .ok()
            .and_then(|meta| meta.created().ok())
            .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|since| (since.as_secs(), since.subsec_nanos()));
        // NTFS alone: on FAT and exFAT the index names a directory entry's
        // slot, and on ReFS a 64-bit index is not guaranteed unique.
        let mut system = [0u16; 32];
        // SAFETY: the handle is open for the call's length, the buffer is
        // as long as the size passed, and every other out-pointer is null.
        let answered = unsafe {
            GetVolumeInformationByHandleW(
                folder.as_raw_handle(),
                std::ptr::null_mut(),
                0,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                system.as_mut_ptr(),
                system.len() as u32,
            )
        } != 0;
        let length = system.iter().position(|&c| c == 0).unwrap_or(system.len());
        let unique_index = answered && String::from_utf16_lossy(&system[..length]) == "NTFS";
        Some(FolderIdentity {
            dev: u64::from(info.dwVolumeSerialNumber),
            ino: (u64::from(info.nFileIndexHigh) << 32) | u64::from(info.nFileIndexLow),
            born,
            unique_index,
        })
    }

    #[cfg(not(any(unix, windows)))]
    fn of(_root: &Path) -> Option<FolderIdentity> {
        None
    }

    /// The identity recorded in `home`, none where none stands or it is
    /// unreadable.
    fn recorded(home: &Path) -> Option<FolderIdentity> {
        let bytes = std::fs::read(home.join(FOLDER_FILE)).ok()?;
        serde_json::from_slice(&bytes).ok()
    }

    /// Whether a folder answering `self` was created after the one
    /// `recorded` describes: its inode differs, or both birth times are
    /// known and its own is later. The device is not compared, since it
    /// moves across mounts of one file system; a birth time moved earlier,
    /// as `touch -t` or `rsync -a` on the folder does on APFS, is the same
    /// folder (issue 173 review, C1).
    fn replaces(&self, recorded: &FolderIdentity) -> bool {
        if self.ino != recorded.ino {
            return true;
        }
        matches!((self.born, recorded.born), (Some(now), Some(then)) if now > then)
    }

    /// Whether `home` holds state recorded for a folder other than the
    /// one at `root`, a folder created there since.
    fn stale(home: &Path, root: &Path) -> bool {
        match (Self::recorded(home), Self::of(root)) {
            (Some(recorded), Some(current)) => current.replaces(&recorded),
            _ => false,
        }
    }

    /// Whether `self` and `other` are one folder wherever each was seen:
    /// the same inode and the same birth time, both known. The device is
    /// not compared (D-163), so a folder reached through another mount
    /// is the same folder.
    ///
    /// Where either was read from an NTFS volume, the volume serial and the
    /// file index alone: the index carries a sequence number NTFS changes
    /// when a record is reused (D-164), so no other folder of that volume
    /// answers it, and the serial, which a Windows volume keeps across
    /// mounts, tells the volumes apart; while NTFS tunnelling hands a folder
    /// renamed onto a name vacated within 15 seconds that name's old
    /// creation time, which would make a swap of two folders unrecognisable.
    fn same_folder(&self, other: &FolderIdentity) -> bool {
        if self.unique_index || other.unique_index {
            return self.dev == other.dev && self.ino == other.ino;
        }
        self.ino == other.ino && self.born.is_some() && self.born == other.born
    }

    /// Whether a move of this folder can be recognised at all: its birth
    /// time is known, or its volume's file index alone tells it.
    fn recognisable(&self) -> bool {
        self.unique_index || self.born.is_some()
    }
}

/// What `folder.json` records (D-163, and the record of
/// `cli-checkouts-on-this-machine`): the folder's identity, the canonical
/// root the home's name is the hash of, as text — none where it is not
/// valid Unicode — and whether the copy is a checkout: a repository's at
/// its checkout's root, or a shared folder's under its own name at that
/// folder. A record an older build wrote carries neither of the two.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct FolderRecord {
    #[serde(flatten)]
    identity: FolderIdentity,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    path: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    checkout: Option<bool>,
}

impl FolderRecord {
    /// The record `home` holds, none where none stands or it does not
    /// parse.
    fn read(home: &Path) -> Option<FolderRecord> {
        let bytes = std::fs::read(home.join(FOLDER_FILE)).ok()?;
        serde_json::from_slice(&bytes).ok()
    }

    /// Write the record into `home`, whole.
    fn write(&self, home: &Path) -> Result<(), CliError> {
        let json = serde_json::to_vec(self).map_err(|err| CliError::Io {
            message: format!("could not record {}: {err}", home.display()),
        })?;
        write_atomic(&home.join(FOLDER_FILE), &json, flush_directory)
    }
}

/// What a collection saw of each file it read, so the next collection
/// spares a file its read where nothing about it moved (SPEC u280
/// `StatRecord`, `NR-01`): kept per working copy as `stat-record.json`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct StatRecord {
    pub entries: BTreeMap<String, StatEntry>,
    /// The modification time of the probe the writing collection made in
    /// the working copy's root after its last read, none where no probe
    /// could be made. An entry whose times do not both stand earlier than
    /// it is too recent to trust.
    pub stamp: Option<(i64, i64)>,
}

/// One file as a collection read it, symbolic links followed, each time
/// as seconds and nanoseconds.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StatEntry {
    pub size: u64,
    pub mtime: (i64, i64),
    pub ctime: (i64, i64),
    pub inode: u64,
    pub sha: String,
}

/// The size, modification time, change time and inode `meta` answers.
#[cfg(unix)]
fn stat_of(meta: &std::fs::Metadata) -> (u64, (i64, i64), (i64, i64), u64) {
    use std::os::unix::fs::MetadataExt;
    (
        meta.len(),
        (meta.mtime(), meta.mtime_nsec()),
        (meta.ctime(), meta.ctime_nsec()),
        meta.ino(),
    )
}

impl StatRecord {
    /// The record kept in `state_dir`, empty with no stamp where its file
    /// is absent or unreadable.
    pub fn load(state_dir: &Path) -> StatRecord {
        std::fs::read(state_dir.join(STAT_RECORD_FILE))
            .ok()
            .and_then(|bytes| serde_json::from_slice(&bytes).ok())
            .unwrap_or_default()
    }

    /// Write the record into `state_dir`.
    pub fn save(&self, state_dir: &Path) -> Result<(), CliError> {
        write_json(&state_dir.join(STAT_RECORD_FILE), self)
    }

    /// The hash the entry for `path` answers where it trusts `meta`: the
    /// size, both times and the inode all equal, and both times earlier
    /// than the stamp.
    #[cfg(unix)]
    pub fn trusted(&self, path: &str, meta: &std::fs::Metadata) -> Option<String> {
        let stamp = self.stamp?;
        let entry = self.entries.get(path)?;
        let (size, mtime, ctime, inode) = stat_of(meta);
        (entry.size == size
            && entry.mtime == mtime
            && entry.ctime == ctime
            && entry.inode == inode
            && mtime < stamp
            && ctime < stamp)
            .then(|| entry.sha.clone())
    }

    /// A record kept on no platform but unix trusts nothing.
    #[cfg(not(unix))]
    pub fn trusted(&self, _path: &str, _meta: &std::fs::Metadata) -> Option<String> {
        None
    }

    /// Record what a read of `path` hashing to `sha` saw, where the file
    /// answered the same size, times and inode before the read and after
    /// it; drop its entry otherwise.
    #[cfg(unix)]
    pub fn observe(
        &mut self,
        path: &str,
        before: &std::fs::Metadata,
        after: &std::fs::Metadata,
        sha: &str,
    ) {
        let seen = stat_of(before);
        if seen != stat_of(after) {
            self.entries.remove(path);
            return;
        }
        let (size, mtime, ctime, inode) = seen;
        self.entries.insert(
            path.to_string(),
            StatEntry {
                size,
                mtime,
                ctime,
                inode,
                sha: sha.to_string(),
            },
        );
    }

    #[cfg(not(unix))]
    pub fn observe(
        &mut self,
        path: &str,
        _before: &std::fs::Metadata,
        _after: &std::fs::Metadata,
        _sha: &str,
    ) {
        self.entries.remove(path);
    }

    /// Whether an entry stands at or past the stamp, or no stamp stands —
    /// an entry the next collection could not trust.
    pub fn holds_untrusted(&self) -> bool {
        match self.stamp {
            None => !self.entries.is_empty(),
            Some(stamp) => self
                .entries
                .values()
                .any(|entry| entry.mtime >= stamp || entry.ctime >= stamp),
        }
    }

    /// Take a fresh stamp: the modification time of a probe created in
    /// `root` under a name a collection sweeps as a partial write, and
    /// removed at once. None where the probe could not be made.
    #[cfg(unix)]
    pub fn restamp(&mut self, root: &Path) {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or_default();
        let minted = &blob_sha1(format!("{nanos}-{}", std::process::id()).as_bytes())[..16];
        let probe = root.join(format!(".syns-partial-{minted}"));
        self.stamp = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&probe)
            .and_then(|file| file.metadata())
            .ok()
            .map(|meta| {
                let (_, mtime, _, _) = stat_of(&meta);
                mtime
            });
        let _ = std::fs::remove_file(&probe);
    }
}

/// One folder a repository is worked on in, and where its state lives.
///
/// SPEC u291: `folder` is the scoped folder a folder copy works, its
/// holder's `owner` and `name` standing beside it; none on every copy
/// `WorkingCopy::open` answers.
///
/// SPEC u298: `state_dir` names the copy's write home — its in-root home
/// where the default root refuses the run's writes, its default home
/// otherwise — and `other_home` the home the run does not write.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkingCopy {
    pub owner: String,
    pub name: String,
    pub root: PathBuf,
    pub state_dir: PathBuf,
    pub folder: Option<FolderScope>,
    stores: StoreRoots,
    other_home: PathBuf,
    /// Issue 173, D-163: the default home holds state recorded for a
    /// removed folder that stood at this root, read as absent.
    stale_default: bool,
}

/// A reconciliation handed to a reviewer: what both sides changed, and
/// what the folder held when it was last continued.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Resolution {
    pub recovery_id: String,
    pub base_commit: Option<String>,
    pub head_commit: String,
    pub round: u32,
    pub local_paths: Vec<String>,
    pub remote_paths: Vec<String>,
    pub collisions: Vec<(String, CollisionKind)>,
    pub combined_paths: Vec<String>,
    pub reviewed_tree: Option<BTreeMap<String, String>>,
    /// Each folder path the candidate's preparation owes a write or a
    /// removal, mapped to the hash it is to hold — none for a removal —
    /// recorded before the first folder write and cleared after the last,
    /// so a resolution carrying it is one a killed or failed run left
    /// half-written.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pending_writes: Option<BTreeMap<String, Option<String>>>,
}

/// A publication that may have landed without its commit recorded as
/// the base: the parent it was sent against and the folder it sent.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Outbox {
    pub parent_commit: Option<String>,
    pub tree: BTreeMap<String, String>,
}

/// One path's content in a snapshot (SPEC u280 `SnapshotContent`). Every
/// snapshot this build writes carries `Stored`: the blob hash naming the
/// file `snapshot-content/{stored}` in the state directory, which holds
/// the content. `Text` and `Bytes` are the inline forms a released build
/// wrote, still loaded as they stand.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum SnapshotContent {
    Text(String),
    Bytes(Vec<u8>),
    Stored { stored: String },
}

/// The directory under the state directory each snapshot content is
/// stored in, one file per content.
const SNAPSHOT_CONTENT_DIR: &str = "snapshot-content";

#[cfg(unix)]
const CONTENT_DIR_MODE: u32 = 0o700;
#[cfg(unix)]
const CONTENT_FILE_MODE: u32 = 0o600;

/// Each snapshotted path mapped to its content, or to `None` where the
/// path was absent.
pub type Snapshot = BTreeMap<String, Option<SnapshotContent>>;

/// The working copy's exclusive state lock, released when dropped or
/// when its process dies: the write home's, and the other home's where
/// one could be taken (SPEC u298, `StateLock`).
#[derive(Debug)]
pub struct StateLock {
    _write: File,
    _other: Option<File>,
}

#[cfg(test)]
impl StateLock {
    /// Whether the other home's lock was taken beside the write home's.
    fn holds_other(&self) -> bool {
        self._other.is_some()
    }
}

/// Where a home stands in the order the `WorkingCopy` reads follow: a
/// stamped home after every unstamped one, stamps compared by value, and
/// unstamped homes by the newest modification time among the files the
/// reads answer from, a home holding none of them first.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Standing {
    Empty,
    Unstamped(std::time::SystemTime),
    Stamped(u128),
}

/// Open the standing lock file at `path` for writing, else for reading,
/// and take its lock, waiting; none where it is absent, opens neither way
/// or refuses the lock call. Creates nothing.
fn take_standing_lock(path: &Path) -> Option<File> {
    if !path.is_file() {
        return None;
    }
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .open(path)
        .or_else(|_| OpenOptions::new().read(true).open(path))
        .ok()?;
    file.lock().ok()?;
    Some(file)
}

impl WorkingCopy {
    /// Open the state of `owner/name` worked on at `root` over `stores`.
    ///
    /// The key is the canonical root, so two spellings of one directory
    /// — a link to it included — open one state directory. The write home
    /// is created where it does not stand (SPEC u298, `WorkingCopy::open`).
    ///
    /// The copy is recorded as a checkout: a repository's at its
    /// checkout's root. A scoped folder's copy under its holder is opened
    /// by `open_scoped` or `open_folder`.
    pub fn open(
        stores: &StoreRoots,
        owner: &str,
        name: &str,
        root: &Path,
    ) -> Result<WorkingCopy, CliError> {
        Self::open_recording(stores, owner, name, root, true)
    }

    /// Open the copy of `owner/name` a scoped folder's directory `dir`
    /// holds under its holder, recorded as no checkout (the share
    /// commands, `return_to_holder`).
    pub fn open_scoped(
        stores: &StoreRoots,
        owner: &str,
        name: &str,
        dir: &Path,
    ) -> Result<WorkingCopy, CliError> {
        Self::open_recording(stores, owner, name, dir, false)
    }

    /// `open`, recording the copy as a checkout where `checkout` is set.
    fn open_recording(
        stores: &StoreRoots,
        owner: &str,
        name: &str,
        root: &Path,
        checkout: bool,
    ) -> Result<WorkingCopy, CliError> {
        let canonical = std::fs::canonicalize(root).map_err(|err| CliError::Io {
            message: format!("could not resolve working copy {}: {err}", root.display()),
        })?;
        if !stores.default_refused {
            let state_dir = Self::state_dir_for(&stores.default, owner, name, &canonical);
            std::fs::create_dir_all(&state_dir).map_err(|err| CliError::Io {
                message: format!(
                    "could not create working copy state {}: {err}",
                    state_dir.display()
                ),
            })?;
        }
        let mut copy = Self::at(stores, owner, name, canonical)?;
        if !stores.default_refused {
            copy.settle_folder(checkout)?;
        }
        copy.ensure_write_home()?;
        Ok(copy)
    }

    /// Settle the default home's record for the folder standing at this
    /// root (D-163, and the decision *a working copy follows its folder*):
    ///
    /// 1. the record matches — identity, path and checkout — and nothing
    ///    is written, no lock taken;
    /// 2. otherwise, under the repository's `folders.lock` and then this
    ///    home's lock, a carry an earlier run left unfinished is finished;
    /// 3. state recorded for another folder, one created at this root
    ///    since, is set aside in `<sha1>.gone`, not discarded;
    /// 4. where the home then holds no state file and the folder's birth
    ///    time is known, the state of this very folder recorded under the
    ///    path it left is carried here, every copy of the moved tree with
    ///    it;
    /// 5. the record is written.
    fn settle_folder(&mut self, checkout: bool) -> Result<(), CliError> {
        let home = self.default_home().to_path_buf();
        let Some(current) = FolderIdentity::of(&self.root) else {
            return Ok(());
        };
        let wanted = FolderRecord {
            identity: current.clone(),
            path: self.root.to_str().map(str::to_string),
            checkout: Some(checkout),
        };
        let repo = home
            .parent()
            .map(Path::to_path_buf)
            .ok_or_else(|| CliError::Io {
                message: format!(
                    "could not settle working copy state {}: no parent directory",
                    home.display()
                ),
            })?;
        // 1 — the record matches, and no carry of the repository is listed:
        // a home a carry left unfinished may match while part of its state
        // stands elsewhere.
        if FolderRecord::read(&home).as_ref() == Some(&wanted) && !carry_listed(&repo) {
            self.stale_default = false;
            return Ok(());
        }
        let io = |err: std::io::Error| CliError::Io {
            message: format!(
                "could not settle working copy state {}: {err}",
                home.display()
            ),
        };
        let mut folders = FoldersLock::take(&repo).map_err(io)?;
        let _own = lock_creating(&home.join(LOCK_FILE)).map_err(io)?;
        let carry = Carry {
            repo: &repo,
            own: &home,
        };

        // 2 — a carry left unfinished, finished first. One of this home's
        // that still cannot be finished fails the run, so the copy is never
        // worked on with a part of its state.
        carry.finish(&mut folders).map_err(io)?;
        let own = carry.own_name();
        if folders
            .kept
            .iter()
            .any(|kept| kept.leaves == own || kept.reaches == own)
        {
            return Err(io(std::io::Error::other(
                "a carry of this folder's state could not be finished; run the command again",
            )));
        }
        if FolderRecord::read(&home).as_ref() == Some(&wanted) {
            self.stale_default = false;
            return Ok(());
        }

        // 3 — another folder's state, set aside.
        if FolderIdentity::stale(&home, &self.root) {
            carry.set_aside(&mut folders, &current).map_err(io)?;
        }

        // 4 — this folder's own state, carried from the path it left.
        if !holds_state(&home)
            && current.recognisable()
            && let Some(found) = carry.moved_here(&current, &self.root)
        {
            carry
                .carry_tree(&mut folders, found, &self.root)
                .map_err(io)?;
        }

        // 5 — the record.
        wanted.write(&home)?;
        self.stale_default = false;
        Ok(())
    }

    /// The copy's default home, whichever role it plays.
    fn default_home(&self) -> &Path {
        if self.writes_in_root() {
            &self.other_home
        } else {
            &self.state_dir
        }
    }

    /// The copy of `owner/name` at the canonical root, both homes named
    /// and neither created; a default home lying inside the root refused.
    fn at(
        stores: &StoreRoots,
        owner: &str,
        name: &str,
        canonical: PathBuf,
    ) -> Result<WorkingCopy, CliError> {
        let default_home = Self::default_home_for(stores, owner, name, &canonical)?;
        let in_root = canonical.join(IN_ROOT_HOME);
        let stale_default = FolderIdentity::stale(&default_home, &canonical);
        let (state_dir, other_home) = if stores.default_refused {
            (in_root, default_home)
        } else {
            (default_home, in_root)
        };
        Ok(WorkingCopy {
            owner: owner.to_string(),
            name: name.to_string(),
            root: canonical,
            state_dir,
            folder: None,
            stores: stores.clone(),
            other_home,
            stale_default,
        })
    }

    /// The default home of `owner/name` at a canonical root, canonical
    /// where it stands; the refusal of one lying inside the copy's root.
    fn default_home_for(
        stores: &StoreRoots,
        owner: &str,
        name: &str,
        canonical: &Path,
    ) -> Result<PathBuf, CliError> {
        let raw = Self::state_dir_for(&stores.default, owner, name, canonical);
        let home = match std::fs::canonicalize(&raw) {
            Ok(home) => home,
            Err(_) => match std::fs::canonicalize(&stores.default) {
                Ok(default) => Self::state_dir_for(&default, owner, name, canonical),
                Err(_) => raw,
            },
        };
        if home.starts_with(canonical) {
            return Err(CliError::Io {
                message: format!(
                    "the working copy state {} lies inside the working copy {}; point SYNS_CACHE_DIR elsewhere",
                    home.display(),
                    canonical.display()
                ),
            });
        }
        Ok(home)
    }

    /// Whether the run writes this copy's state to its in-root home.
    fn writes_in_root(&self) -> bool {
        self.stores.default_refused
    }

    /// The copy's in-root home, whichever role it plays.
    fn in_root_home(&self) -> &Path {
        if self.writes_in_root() {
            &self.state_dir
        } else {
            &self.other_home
        }
    }

    /// Whether `home` may be read: an in-root home standing as a link or
    /// owned by another account is read as holding no state.
    fn readable(&self, home: &Path) -> bool {
        if home == self.default_home() {
            return !self.stale_default;
        }
        home != self.in_root_home() || !is_foreign_or_link(home)
    }

    /// Create the write home where it does not stand — the in-root home
    /// through `ensure_in_root_home` — before a write reaches it.
    fn ensure_write_home(&self) -> Result<(), CliError> {
        if self.writes_in_root() {
            return ensure_in_root_home(&self.state_dir);
        }
        std::fs::create_dir_all(&self.state_dir).map_err(|err| CliError::Io {
            message: format!(
                "could not create working copy state {}: {err}",
                self.state_dir.display()
            ),
        })
    }

    /// The roots the copy was opened over (SPEC u298, `WorkingCopy::stores`).
    pub fn stores(&self) -> &StoreRoots {
        &self.stores
    }

    /// Open the state of the repository the scope addresses worked on at
    /// the folder's canonical directory (SPEC u291,
    /// `WorkingCopy::open_folder`): a folder and its holder's checkout
    /// never share a state directory. SPEC u302 `WorkingCopy::open_folder`
    /// 1–2: a folder bound to its identity opens the identity's copy, the
    /// base its holder's copy there recorded later carried to it.
    pub fn open_folder(stores: &StoreRoots, scope: &FolderScope) -> Result<WorkingCopy, CliError> {
        let (owner, name) = Self::addressed(scope);
        // A shared folder's copy under its own name at that folder is a
        // checkout; every other scoped folder's is part of one.
        let checkout = scope.identity.is_some() && scope.enclosing.is_empty();
        let mut copy = Self::open_recording(stores, &owner, &name, &scope.dir, checkout)?;
        copy.folder = Some(scope.clone());
        if scope.identity.is_some() {
            copy.carry_base(&scope.holder())?;
        }
        Ok(copy)
    }

    /// The owner and the name a scope's `address` joins.
    fn addressed(scope: &FolderScope) -> (String, String) {
        let name = scope.identity.as_ref().unwrap_or(&scope.name);
        (scope.owner.clone(), name.clone())
    }

    /// The folder copy where either home already holds its state, none
    /// where neither does, creating nothing and carrying nothing (SPEC
    /// u291, `WorkingCopy::open_existing_folder`; SPEC u302, keyed on the
    /// scope's address).
    pub fn open_existing_folder(
        stores: &StoreRoots,
        scope: &FolderScope,
    ) -> Result<Option<WorkingCopy>, CliError> {
        let (owner, name) = Self::addressed(scope);
        Ok(
            Self::open_existing(stores, &owner, &name, &scope.dir)?.map(|mut copy| {
                copy.folder = Some(scope.clone());
                copy
            }),
        )
    }

    /// Carry the base the copy of `from` (`OWNER/NAME`) at this copy's
    /// canonical directory records to this copy where it was recorded
    /// after this copy's own, or this copy records none — its commit,
    /// per-file hashes and recorded time unchanged, the copy of `from`
    /// left as it stood — answering whether it did (SPEC u302 Behaviour,
    /// `WorkingCopy::carry_base` 1–3). A record carrying no time ranks
    /// below every one that does.
    pub fn carry_base(&self, from: &str) -> Result<bool, CliError> {
        // 1 — this copy's base, under its lock.
        let _lock = self.lock()?;
        let own = self.base();

        // 2 — the other copy's, in either home, creating no state.
        let Some((owner, name)) = from.split_once('/') else {
            return Ok(false);
        };
        let Some(other) = Self::open_existing(&self.stores, owner, name, &self.root)? else {
            return Ok(false);
        };
        let Some(carried) = other.base() else {
            return Ok(false);
        };
        if own.is_some_and(|own| own.recorded_at() >= carried.recorded_at()) {
            return Ok(false);
        }

        // 3 — recorded as this copy's, unchanged.
        let files = carried
            .file_paths()
            .filter_map(|path| {
                carried
                    .file_sha(path)
                    .map(|sha| (path.to_string(), sha.to_string()))
            })
            .collect();
        self.record_laid_base(
            carried.commit_sha().unwrap_or_default(),
            files,
            carried.recorded_at(),
        )?;
        Ok(true)
    }

    /// The copy of `owner/name` at `root` where either of its homes
    /// already stands, and `None` where neither does — the read a guard
    /// makes when it must not create one (SPEC u271, `checkout_of`, which
    /// answers none of its three by writing state). It creates no
    /// directory; a write through the copy creates its write home first.
    pub fn open_existing(
        stores: &StoreRoots,
        owner: &str,
        name: &str,
        root: &Path,
    ) -> Result<Option<WorkingCopy>, CliError> {
        let Ok(canonical) = std::fs::canonicalize(root) else {
            return Ok(None);
        };
        let in_root = canonical.join(IN_ROOT_HOME);
        let default = Self::state_dir_for(&stores.default, owner, name, &canonical);
        let standing = (default.is_dir() && !FolderIdentity::stale(&default, &canonical))
            || (in_root.is_dir() && !is_foreign_or_link(&in_root));
        if !standing {
            return Ok(None);
        }
        Self::at(stores, owner, name, canonical).map(Some)
    }

    /// Where the state for `owner/name` at a canonical root stands. The
    /// key is that root, so two spellings of one directory — a link to
    /// it included — address one state directory.
    fn state_dir_for(cache_dir: &Path, owner: &str, name: &str, canonical: &Path) -> PathBuf {
        cache_dir
            .join("working-copies")
            .join(owner)
            .join(name)
            .join(blob_sha1(canonical.as_os_str().as_encoded_bytes()))
    }

    /// Take the copy's lock in every home where one could be taken, the
    /// default home's first, waiting while another process holds either;
    /// then adopt the other home's state where it was stamped later
    /// (SPEC u298 Behaviour, `WorkingCopy::lock` 1–3).
    pub fn lock(&self) -> Result<StateLock, CliError> {
        let io = |err: std::io::Error| CliError::Io {
            message: format!(
                "could not lock working copy state {}: {err}",
                self.state_dir.display()
            ),
        };
        let take_write = || -> Result<File, CliError> {
            self.ensure_write_home()?;
            let file = OpenOptions::new()
                .create(true)
                .truncate(false)
                .read(true)
                .write(true)
                .open(self.state_dir.join(LOCK_FILE))
                .map_err(io)?;
            file.lock().map_err(io)?;
            Ok(file)
        };
        let take_other = || -> Option<File> {
            if !self.readable(&self.other_home) {
                return None;
            }
            take_standing_lock(&self.other_home.join(LOCK_FILE))
        };
        // 1 — the default home's lock before the in-root home's.
        let lock = if self.writes_in_root() {
            let other = take_other();
            StateLock {
                _write: take_write()?,
                _other: other,
            }
        } else {
            let write = take_write()?;
            StateLock {
                _write: write,
                _other: take_other(),
            }
        };
        // 3 — the state stamped last adopted into the write home.
        if self.other_stamped_later() {
            self.adopt().map_err(|err| CliError::Io {
                message: format!(
                    "could not adopt working copy state into {}: {err}",
                    self.state_dir.display()
                ),
            })?;
        }
        Ok(lock)
    }

    /// Where `home` stands in the order the reads follow.
    fn standing(&self, home: &Path) -> Standing {
        if !self.readable(home) {
            return Standing::Empty;
        }
        if let Some(stamp) = std::fs::read_to_string(home.join(STATE_STAMP))
            .ok()
            .and_then(|text| text.trim().parse::<u128>().ok())
        {
            return Standing::Stamped(stamp);
        }
        read_files(home)
            .into_iter()
            .map(|(_, modified)| modified)
            .max()
            .map_or(Standing::Empty, Standing::Unstamped)
    }

    /// Whether the other home is ordered after the write home, a tie
    /// going to the write home (SPEC u298, `WorkingCopy` reads).
    fn other_stamped_later(&self) -> bool {
        self.standing(&self.other_home) > self.standing(&self.state_dir)
    }

    /// The home the reads answer from: the one ordered later, none where
    /// it may not be read.
    fn read_home(&self) -> Option<&Path> {
        let home: &Path = if self.other_stamped_later() {
            &self.other_home
        } else {
            &self.state_dir
        };
        self.readable(home).then_some(home)
    }

    /// The file `name` in the home the reads answer from.
    fn read_path(&self, name: &str) -> Option<PathBuf> {
        self.read_home().map(|home| home.join(name))
    }

    /// `WorkingCopy::lock` 3: copy every state file the other home's
    /// reads answer from into the write home, the newest-modified last and
    /// each keeping its modification time, after removing each the write
    /// home holds that the other lacks; then the other's stamp, last.
    fn adopt(&self) -> std::io::Result<()> {
        let source = &self.other_home;
        let target = &self.state_dir;
        let mut adopted = read_files(source);
        let keep: std::collections::HashSet<&str> =
            adopted.iter().map(|(rel, _)| rel.as_str()).collect();
        for (rel, _) in read_files(target) {
            if !keep.contains(rel.as_str()) {
                match std::fs::remove_file(target.join(&rel)) {
                    Ok(()) => {}
                    Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
                    Err(err) => return Err(err),
                }
            }
        }
        adopted.sort_by_key(|(_, modified)| *modified);
        if adopted
            .iter()
            .any(|(rel, _)| rel.starts_with(SNAPSHOT_CONTENT_DIR))
        {
            self.content_dir()
                .map_err(|err| std::io::Error::other(err.to_string()))?;
        }
        for (rel, modified) in &adopted {
            copy_keeping_time(&source.join(rel), &target.join(rel), *modified)?;
        }
        let stamp = source.join(STATE_STAMP);
        if stamp.is_file() {
            let bytes = std::fs::read(&stamp)?;
            write_atomic(&target.join(STATE_STAMP), &bytes, flush_directory)
                .map_err(|err| std::io::Error::other(err.to_string()))?;
        }
        Ok(())
    }

    /// Date the copy's last use: its write home's stamp rewritten under
    /// its lock, though no state changed. A stamp that cannot be written
    /// is left as it stood: it orders the listing and guards nothing.
    pub fn mark_used(&self) {
        if let Ok(_lock) = self.lock() {
            let _ = self.restamp();
        }
    }

    /// Rewrite the write home's stamp, dating its last change.
    fn restamp(&self) -> Result<(), CliError> {
        write_atomic(
            &self.state_dir.join(STATE_STAMP),
            now_nanos_wide().to_string().as_bytes(),
            flush_directory,
        )
    }

    /// Write `value` as the state file `name` in the write home, then
    /// restamp it.
    fn write_state<T: Serialize>(&self, name: &str, value: &T) -> Result<(), CliError> {
        self.ensure_write_home()?;
        write_json(&self.state_dir.join(name), value)?;
        self.restamp()
    }

    /// Remove the state file `name` from the write home, restamping it
    /// where a file was removed.
    fn remove_state(&self, name: &str) -> Result<(), CliError> {
        if remove_state_file(&self.state_dir.join(name))? {
            self.restamp()?;
        }
        Ok(())
    }

    /// The recorded base, none where its file is absent or unreadable.
    pub fn base(&self) -> Option<Manifest> {
        let text = std::fs::read_to_string(self.read_path(BASE_FILE)?).ok()?;
        serde_json::from_str(&text).ok()
    }

    /// Record `commit` and exactly `files` as the base, stamped with the
    /// time the system clock reads as it is recorded (SPEC u291).
    pub fn record_base(
        &self,
        commit: &str,
        files: HashMap<String, String>,
    ) -> Result<(), CliError> {
        self.record_laid_base(commit, files, Some(now_nanos()))
    }

    /// Record `commit` and exactly `files` as the base carrying
    /// `recorded_at` as given: a base laid over this copy's keeps the time
    /// this copy's record carried before it (SPEC u291, `converge` 6).
    pub fn record_laid_base(
        &self,
        commit: &str,
        files: HashMap<String, String>,
        recorded_at: Option<u64>,
    ) -> Result<(), CliError> {
        // A path this run left out, the volume holding it as one with
        // another spelling, was never written here: recorded, the next run
        // would read it as removed and publish its deletion.
        let mut files = files;
        files.retain(|path, _| !held_out_as_one(&self.root, path));
        let mut manifest = Manifest::default();
        manifest.update(commit.to_string(), files);
        manifest.set_recorded_at(recorded_at);
        self.write_state(BASE_FILE, &manifest)
    }

    /// Lay `files`, each path already counted from this copy's root, over
    /// the base it records (SPEC u304 Behaviour, `WorkingCopy::lay_files`
    /// 1–2): under the state lock, where the copy holds no resolution and
    /// no outbox and records a base, that base then carries each of
    /// `files` at its hash beside every path it carried, its commit
    /// `landed` exactly where it was `advance_from` and its own otherwise,
    /// the record's time kept. True where the base was laid; false, with
    /// nothing written and no state created, otherwise.
    pub fn lay_files(
        &self,
        files: &HashMap<String, String>,
        advance_from: Option<&str>,
        landed: &str,
    ) -> Result<bool, CliError> {
        // 1 — nothing written to a copy recording no base, before the lock
        // creates its write home.
        if self.base().is_none() {
            return Ok(false);
        }
        let _lock = self.lock()?;
        if self.resolution()?.is_some() || self.outbox()?.is_some() {
            return Ok(false);
        }
        let Some(standing) = self.base() else {
            return Ok(false);
        };
        // 2 — every path the base carried, beside each of `files`.
        let mut laid: HashMap<String, String> = standing
            .file_paths()
            .filter_map(|path| {
                standing
                    .file_sha(path)
                    .map(|sha| (path.to_string(), sha.to_string()))
            })
            .collect();
        for (path, sha) in files {
            laid.insert(path.clone(), sha.clone());
        }
        let commit = match standing.commit_sha() {
            Some(own) if advance_from == Some(own) => landed,
            own => own.unwrap_or_default(),
        };
        self.record_laid_base(commit, laid, standing.recorded_at())?;
        Ok(true)
    }

    pub fn resolution(&self) -> Result<Option<Resolution>, CliError> {
        match self.read_path(RESOLUTION_FILE) {
            Some(path) => read_json(&path),
            None => Ok(None),
        }
    }

    pub fn write_resolution(&self, resolution: &Resolution) -> Result<(), CliError> {
        self.write_state(RESOLUTION_FILE, resolution)
    }

    pub fn remove_resolution(&self) -> Result<(), CliError> {
        self.remove_state(RESOLUTION_FILE)
    }

    pub fn outbox(&self) -> Result<Option<Outbox>, CliError> {
        match self.read_path(OUTBOX_FILE) {
            Some(path) => read_json(&path),
            None => Ok(None),
        }
    }

    pub fn write_outbox(&self, outbox: &Outbox) -> Result<(), CliError> {
        self.write_state(OUTBOX_FILE, outbox)
    }

    pub fn remove_outbox(&self) -> Result<(), CliError> {
        self.remove_state(OUTBOX_FILE)
    }

    /// A snapshot document as the home the reads answer from holds it.
    fn read_snapshot(&self, name: &str) -> Result<Snapshot, CliError> {
        match self.read_path(name) {
            Some(path) => Ok(read_json(&path)?.unwrap_or_default()),
            None => Ok(Snapshot::new()),
        }
    }

    /// The folder's content before a reconciliation rewrote it.
    pub fn local_snapshot(&self) -> Result<Snapshot, CliError> {
        self.read_snapshot(LOCAL_SNAPSHOT_FILE)
    }

    pub fn write_local_snapshot(&self, snapshot: &Snapshot) -> Result<(), CliError> {
        self.write_snapshots(Some(snapshot), None)
    }

    /// Each collision's content at the head it was prepared against.
    pub fn remote_snapshot(&self) -> Result<Snapshot, CliError> {
        self.read_snapshot(REMOTE_SNAPSHOT_FILE)
    }

    pub fn write_remote_snapshot(&self, snapshot: &Snapshot) -> Result<(), CliError> {
        self.write_snapshots(None, Some(snapshot))
    }

    /// Make every content stored since the last flush durable in its
    /// directory, once for all of them, before a document names any.
    fn flush_snapshot_content(&self) -> Result<(), CliError> {
        let dir = self.snapshot_content_dir();
        if !dir.is_dir() {
            return Ok(());
        }
        finish_directory_flush(&dir, flush_directory(&dir))
    }

    /// Write whichever snapshot documents are given, then remove every
    /// content file neither standing snapshot names — the one prune a
    /// step writing both documents takes, so no content the second
    /// document names is removed before it is written.
    pub fn write_snapshots(
        &self,
        local: Option<&Snapshot>,
        remote: Option<&Snapshot>,
    ) -> Result<(), CliError> {
        self.ensure_write_home()?;
        self.flush_snapshot_content()?;
        if let Some(local) = local {
            write_json(&self.local_snapshot_path(), local)?;
        }
        if let Some(remote) = remote {
            write_json(&self.remote_snapshot_path(), remote)?;
        }
        self.prune_snapshot_content()?;
        self.restamp()
    }

    /// Remove both snapshots, then every content file neither names.
    pub fn remove_snapshots(&self) -> Result<(), CliError> {
        let local = remove_state_file(&self.local_snapshot_path())?;
        let remote = remove_state_file(&self.remote_snapshot_path())?;
        self.prune_snapshot_content()?;
        if local || remote {
            self.restamp()?;
        }
        Ok(())
    }

    /// The directory holding each stored snapshot content.
    pub fn snapshot_content_dir(&self) -> PathBuf {
        self.state_dir.join(SNAPSHOT_CONTENT_DIR)
    }

    /// Create the content directory, reachable by the person's own
    /// account alone, where it does not stand.
    fn content_dir(&self) -> Result<PathBuf, CliError> {
        self.ensure_write_home()?;
        let dir = self.snapshot_content_dir();
        let io = |err: std::io::Error| CliError::Io {
            message: format!("could not write {}: {err}", dir.display()),
        };
        if !dir.is_dir() {
            let mut builder = std::fs::DirBuilder::new();
            builder.recursive(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::DirBuilderExt;
                builder.mode(CONTENT_DIR_MODE);
            }
            builder.create(&dir).map_err(io)?;
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(CONTENT_DIR_MODE))
                .map_err(io)?;
        }
        Ok(dir)
    }

    /// Store content by its blob hash: `fill` writes it into a fresh file
    /// beside the content files and answers its blob hash, and the file
    /// is made durable and renamed to that hash — its directory entry made
    /// durable by the flush the next snapshot document's write opens with. Answers the hash and
    /// whether this call created the content file, none standing there
    /// before.
    fn store_with(
        &self,
        fill: impl FnOnce(&mut File) -> Result<String, CliError>,
    ) -> Result<(String, bool), CliError> {
        let dir = self.content_dir()?;
        let temp = dir.join(format!(
            ".incoming.{}.{}",
            std::process::id(),
            SIBLING_COUNTER.fetch_add(1, Ordering::Relaxed)
        ));
        let io = |err: std::io::Error| CliError::Io {
            message: format!("could not write {}: {err}", temp.display()),
        };
        let mut options = OpenOptions::new();
        options.write(true).read(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(CONTENT_FILE_MODE);
        }
        let stored = (|| {
            let mut file = options.open(&temp).map_err(io)?;
            let sha = fill(&mut file)?;
            file.sync_all().map_err(io)?;
            drop(file);
            let target = dir.join(&sha);
            let created = !target.exists();
            std::fs::rename(&temp, &target).map_err(io)?;
            Ok((sha, created))
        })();
        if stored.is_err() {
            let _ = std::fs::remove_file(&temp);
        }
        stored
    }

    /// Store `bytes` as a snapshot content.
    pub fn store_bytes(&self, bytes: &[u8]) -> Result<(String, bool), CliError> {
        self.store_with(|file| {
            file.write_all(bytes).map_err(|err| CliError::Io {
                message: format!("could not write a snapshot content: {err}"),
            })?;
            Ok(blob_sha1(bytes))
        })
    }

    /// Store the file at `source` as a snapshot content, copied in
    /// pieces as it stands.
    pub fn store_file(&self, source: &Path) -> Result<(String, bool), CliError> {
        self.store_with(|file| {
            copy_hashing(source, file).map_err(|err| CliError::Io {
                message: format!("could not read {}: {err}", source.display()),
            })
        })
    }

    /// Store a collected file as a snapshot content through
    /// `copy_collected`, so what is stored hashes to what the collection
    /// took.
    pub fn store_collected(
        &self,
        root: &Path,
        path: &str,
        collected: &crate::push::collector::CollectedFile,
    ) -> Result<(String, bool), CliError> {
        let dir = self.content_dir()?;
        let temp = dir.join(format!(
            ".incoming.{}.{}",
            std::process::id(),
            SIBLING_COUNTER.fetch_add(1, Ordering::Relaxed)
        ));
        crate::push::collector::copy_collected(root, path, collected, &temp)?;
        let io = |err: std::io::Error| CliError::Io {
            message: format!("could not write {}: {err}", temp.display()),
        };
        let stored = (|| {
            flush_file(&temp).map_err(io)?;
            let target = dir.join(&collected.sha);
            let created = !target.exists();
            std::fs::rename(&temp, &target).map_err(io)?;
            Ok((collected.sha.clone(), created))
        })();
        if stored.is_err() {
            let _ = std::fs::remove_file(&temp);
        }
        stored
    }

    /// Remove a content file this run stored and no snapshot came to name.
    pub fn remove_stored(&self, sha: &str) {
        let _ = std::fs::remove_file(self.snapshot_content_dir().join(sha));
    }

    /// The file holding a stored content, answered only where its bytes,
    /// read in pieces, hash to its name.
    pub fn stored_content(&self, sha: &str) -> Result<PathBuf, CliError> {
        let path = self
            .read_home()
            .unwrap_or(&self.state_dir)
            .join(SNAPSHOT_CONTENT_DIR)
            .join(sha);
        let refused = || CliError::Io {
            message: format!(
                "could not read {}: content does not match its hash",
                path.display()
            ),
        };
        let mut sink = std::io::sink();
        match copy_hashing(&path, &mut sink) {
            Ok(actual) if actual == sha => Ok(path),
            _ => Err(refused()),
        }
    }

    /// A snapshot content's bytes, whatever form it was written in, a
    /// stored one read only where it hashes to its name.
    pub fn content_bytes(&self, content: &SnapshotContent) -> Result<Vec<u8>, CliError> {
        match content {
            SnapshotContent::Text(text) => Ok(text.clone().into_bytes()),
            SnapshotContent::Bytes(bytes) => Ok(bytes.clone()),
            SnapshotContent::Stored { stored } => {
                let path = self.stored_content(stored)?;
                std::fs::read(&path).map_err(|err| CliError::Io {
                    message: format!("could not read {}: {err}", path.display()),
                })
            }
        }
    }

    /// Remove every content file neither snapshot standing in the write
    /// home names.
    pub fn prune_snapshot_content(&self) -> Result<(), CliError> {
        let dir = self.snapshot_content_dir();
        let Ok(entries) = std::fs::read_dir(&dir) else {
            return Ok(());
        };
        let mut named = std::collections::HashSet::new();
        let standing = |path: PathBuf| -> Result<Snapshot, CliError> {
            Ok(read_json(&path)?.unwrap_or_default())
        };
        for snapshot in [
            standing(self.local_snapshot_path())?,
            standing(self.remote_snapshot_path())?,
        ] {
            for content in snapshot.into_values().flatten() {
                if let SnapshotContent::Stored { stored } = content {
                    named.insert(stored);
                }
            }
        }
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().to_string();
            // Every caller holds the state lock and has renamed each of
            // its own stores, so an `.incoming.` file standing here is a
            // killed run's (CR1-4).
            if named.contains(&name) {
                continue;
            }
            let _ = std::fs::remove_file(entry.path());
        }
        if named.is_empty() {
            let _ = std::fs::remove_dir(&dir);
        }
        Ok(())
    }

    pub fn local_snapshot_path(&self) -> PathBuf {
        self.state_dir.join(LOCAL_SNAPSHOT_FILE)
    }

    pub fn remote_snapshot_path(&self) -> PathBuf {
        self.state_dir.join(REMOTE_SNAPSHOT_FILE)
    }

    /// The stat record this working copy keeps, empty where none loads.
    pub fn stat_record(&self) -> StatRecord {
        match self.read_home() {
            Some(home) => StatRecord::load(home),
            None => StatRecord::default(),
        }
    }

    pub fn write_stat_record(&self, record: &StatRecord) -> Result<(), CliError> {
        self.ensure_write_home()?;
        record.save(&self.state_dir)?;
        self.restamp()
    }
}

/// Each state file the reads answer from standing in `home` — the
/// documents, and every content under `snapshot-content` — counted from
/// `home`, beside its modification time.
fn read_files(home: &Path) -> Vec<(String, std::time::SystemTime)> {
    let modified = |path: &Path| {
        std::fs::symlink_metadata(path)
            .ok()
            .filter(|meta| meta.is_file())
            .and_then(|meta| meta.modified().ok())
    };
    let mut files: Vec<(String, std::time::SystemTime)> = READ_FILES
        .iter()
        .filter_map(|name| modified(&home.join(name)).map(|time| (name.to_string(), time)))
        .collect();
    for entry in std::fs::read_dir(home.join(SNAPSHOT_CONTENT_DIR))
        .into_iter()
        .flatten()
        .flatten()
    {
        let name = entry.file_name().to_string_lossy().to_string();
        if name.starts_with('.') {
            continue;
        }
        if let Some(time) = modified(&entry.path()) {
            files.push((format!("{SNAPSHOT_CONTENT_DIR}/{name}"), time));
        }
    }
    files
}

// ---- a working copy follows its folder --------------------------------------

/// The lock every run that sets aside or carries a repository's state
/// takes first, beside its `review.lock`; while a carry is under way it
/// holds that carry's list of members, one line of JSON.
const FOLDERS_LOCK: &str = "folders.lock";

/// The suffix of a set-aside: `<sha1>.gone` holds the state recorded for a
/// folder that left the path `<sha1>` is the hash of.
const SET_ASIDE: &str = ".gone";

/// The name a set-aside that is the opening folder's own takes while a
/// newer one is made beside it: `<sha1>.taking.gone`, a set-aside still.
const TAKING: &str = ".taking";

/// Open the lock file at `path`, created where absent, and take its lock,
/// waiting.
fn lock_creating(path: &Path) -> std::io::Result<File> {
    let file = OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(path)?;
    file.lock()?;
    Ok(file)
}

/// Whether `home` holds a state file: a base, a resolution, an outbox, a
/// snapshot or a stat record.
fn holds_state(home: &Path) -> bool {
    !read_files(home).is_empty()
}

/// The stamp `home` carries, none where it carries none.
fn stamp_of(home: &Path) -> Option<u128> {
    std::fs::read_to_string(home.join(STATE_STAMP))
        .ok()?
        .trim()
        .parse()
        .ok()
}

/// Whether the repository folder `repo` lists a carry or a set-aside under
/// way, read without its lock: a first line in `folders.lock` that is not
/// empty, or a file that cannot be read (Windows refuses a read of a file
/// another process holds locked) — either sends the run to take the lock.
fn carry_listed(repo: &Path) -> bool {
    match std::fs::read(repo.join(FOLDERS_LOCK)) {
        Ok(bytes) => bytes
            .split(|b| *b == b'\n')
            .next()
            .is_some_and(|line| !line.is_empty()),
        Err(err) => err.kind() != std::io::ErrorKind::NotFound,
    }
}

/// Whether `name` is a default home's: the 40 hexadecimal digits of a
/// root's hash.
fn is_home_name(name: &str) -> bool {
    name.len() == 40 && name.bytes().all(|b| b.is_ascii_hexdigit())
}

/// Flush `dir`, a flush the file system does not support standing.
fn flush_dir_quietly(dir: &Path) -> std::io::Result<()> {
    match flush_directory(dir) {
        Err(err) if !is_unsupported_flush(&err) => Err(err),
        _ => Ok(()),
    }
}

/// Move `source` to `target`, a directory merged into one standing there
/// entry by entry, and anything else standing at `target` replaced.
fn move_entry(source: &Path, target: &Path) -> std::io::Result<()> {
    let source_is_dir = std::fs::symlink_metadata(source)?.is_dir();
    match std::fs::symlink_metadata(target) {
        Ok(meta) if meta.is_dir() && source_is_dir => {
            for entry in std::fs::read_dir(source)? {
                let entry = entry?;
                move_entry(&entry.path(), &target.join(entry.file_name()))?;
            }
            return match std::fs::remove_dir(source) {
                Err(err) if err.kind() != std::io::ErrorKind::NotFound => Err(err),
                _ => Ok(()),
            };
        }
        Ok(meta) if meta.is_dir() => std::fs::remove_dir_all(target)?,
        Ok(_) if source_is_dir => std::fs::remove_file(target)?,
        _ => {}
    }
    std::fs::rename(source, target)
}

/// Move every entry of the home `from` into `to` but its lock file and its
/// record, then flush both.
fn move_state(from: &Path, to: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(to)?;
    for entry in std::fs::read_dir(from)? {
        let entry = entry?;
        let name = entry.file_name();
        if name == LOCK_FILE || name == FOLDER_FILE {
            continue;
        }
        move_entry(&entry.path(), &to.join(&name))?;
    }
    flush_dir_quietly(to)?;
    flush_dir_quietly(from)
}

/// Remove the file at `path`, one already gone standing.
fn remove_if_standing(path: &Path) -> std::io::Result<()> {
    match std::fs::remove_file(path) {
        Err(err) if err.kind() != std::io::ErrorKind::NotFound => Err(err),
        _ => Ok(()),
    }
}

/// The folder that was moved, and where it now stands, read from the path
/// a copy's state was recorded at and the root it is opened at now: their
/// common trailing folders taken off (`/a/p/docs` and `/a/q/docs` give
/// `/a/p` and `/a/q`). Two equal paths are answered as they stand.
fn moved_between(left: &Path, now: &Path) -> (PathBuf, PathBuf) {
    let (mut from, mut to) = (left.to_path_buf(), now.to_path_buf());
    if from == to {
        return (from, to);
    }
    while let (Some(a), Some(b)) = (from.file_name(), to.file_name()) {
        if a != b {
            break;
        }
        let (Some(up_from), Some(up_to)) = (from.parent(), to.parent()) else {
            break;
        };
        let (up_from, up_to) = (up_from.to_path_buf(), up_to.to_path_buf());
        from = up_from;
        to = up_to;
    }
    (from, to)
}

/// One copy a carry moves: the home it leaves and the home it reaches,
/// both named within the repository's folder, the path it reaches, and
/// the identity of its folder, which must stand there. A set-aside under
/// way (`set_aside`) is listed the same way, reaching `<sha1>.gone`: its
/// folder stands nowhere it is looked for.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct Member {
    leaves: String,
    reaches: String,
    path: String,
    /// The path the folder was recorded at in the home it leaves, where
    /// one was: a carry undone puts the record back with it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    left: Option<String>,
    #[serde(default)]
    dev: u64,
    ino: u64,
    born: Option<(u64, u32)>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    checkout: Option<bool>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    set_aside: bool,
}

impl Member {
    /// The identity the list holds for the member's folder.
    fn identity(&self) -> FolderIdentity {
        FolderIdentity {
            dev: self.dev,
            ino: self.ino,
            born: self.born,
            unique_index: false,
        }
    }

    /// Whether the member leaves or reaches a home `other` leaves or
    /// reaches.
    fn meets(&self, other: &Member) -> bool {
        [&self.leaves, &self.reaches]
            .into_iter()
            .any(|dir| *dir == other.leaves || *dir == other.reaches)
    }

    /// Whether the home the member reaches holds the record its move wrote
    /// there: the listed one, or one naming the path it reaches and its
    /// inode — the birth time is read again when the move writes it, and
    /// NTFS tunnelling may have changed it. A record a released build
    /// wrote names no path.
    fn reached_in(&self, home: &Path) -> bool {
        self.recorded_in(home)
            || FolderRecord::read(home).is_some_and(|r| {
                r.path.as_deref() == Some(self.path.as_str()) && r.identity.ino == self.ino
            })
    }

    /// Whether `home` still holds the record the list names: the same
    /// inode and birth time, exactly.
    fn recorded_in(&self, home: &Path) -> bool {
        FolderRecord::read(home)
            .is_some_and(|r| r.identity.ino == self.ino && r.identity.born == self.born)
    }
}

/// The repository's `folders.lock`, held.
struct FoldersLock {
    file: File,
    /// Members a run could not finish, a file of theirs already moved:
    /// kept at the head of every list this run writes, for a later run.
    kept: Vec<Member>,
}

/// What one member's move came to.
enum Moved {
    /// Every file moved, the record with it.
    Done,
    /// Nothing moved: the member failed its test, or could not be begun.
    Left(Option<std::io::Error>),
    /// It failed once its files had begun to move: it stays listed.
    Broken(std::io::Error),
}

impl FoldersLock {
    /// Take the lock of the repository folder `repo`, waiting.
    fn take(repo: &Path) -> std::io::Result<FoldersLock> {
        std::fs::create_dir_all(repo)?;
        Ok(FoldersLock {
            file: lock_creating(&repo.join(FOLDERS_LOCK))?,
            kept: Vec::new(),
        })
    }

    /// The list of a carry under way, empty where none stands or the line
    /// is torn: a torn list was being written before anything moved.
    fn list(&mut self) -> Vec<Member> {
        use std::io::{Read, Seek, SeekFrom};
        let mut bytes = Vec::new();
        if self.file.seek(SeekFrom::Start(0)).is_err() || self.file.read_to_end(&mut bytes).is_err()
        {
            return Vec::new();
        }
        let line = bytes.split(|b| *b == b'\n').next().unwrap_or_default();
        serde_json::from_slice(line).unwrap_or_default()
    }

    /// Write `members` as the list, its one line ended by a newline, and
    /// flush it; an empty list clears it. The file is written in place, so
    /// a run waiting on its lock waits on the same file.
    /// The members this run keeps for a later one stand first.
    fn write(&mut self, members: &[Member]) -> std::io::Result<()> {
        use std::io::{Seek, SeekFrom};
        let listed: Vec<&Member> = self.kept.iter().chain(members).collect();
        let mut bytes = if listed.is_empty() {
            Vec::new()
        } else {
            serde_json::to_vec(&listed).map_err(std::io::Error::other)?
        };
        bytes.push(b'\n');
        self.file.seek(SeekFrom::Start(0))?;
        self.file.write_all(&bytes)?;
        // Only the first line is read: a tail left where shortening the
        // file is refused counts for nothing.
        let _ = self.file.set_len(bytes.len() as u64);
        self.file.sync_all()
    }
}

/// One record a home or a set-aside of the repository holds.
struct Recorded {
    dir: String,
    set_aside: bool,
    record: FolderRecord,
}

/// The carry and the set-aside of one repository's homes, run holding its
/// `folders.lock` and the opening home's `state.lock`.
struct Carry<'a> {
    repo: &'a Path,
    own: &'a Path,
}

impl Carry<'_> {
    /// The opening home's name within the repository's folder.
    fn own_name(&self) -> String {
        self.own
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_default()
    }

    /// Every record the repository's other homes and its set-asides hold.
    fn records(&self) -> Vec<Recorded> {
        let own = self.own_name();
        let mut found = Vec::new();
        for entry in std::fs::read_dir(self.repo).into_iter().flatten().flatten() {
            let Ok(name) = entry.file_name().into_string() else {
                continue;
            };
            if name == own || !entry.file_type().is_ok_and(|kind| kind.is_dir()) {
                continue;
            }
            let set_aside = name.ends_with(SET_ASIDE);
            if !set_aside && !is_home_name(&name) {
                continue;
            }
            if let Some(record) = FolderRecord::read(&entry.path()) {
                found.push(Recorded {
                    dir: name,
                    set_aside,
                    record,
                });
            }
        }
        found
    }

    /// Step 2: finish the carry or the set-aside the list in `folders`
    /// names, each member tested again before it moves, then clear the
    /// list. A run finishing another folder's carry never fails on it: a
    /// member that fails its test is struck, and one that meets an error
    /// stays listed for a later run — it may be part-moved already — and
    /// every home it touches is left to that run.
    fn finish(&self, folders: &mut FoldersLock) -> std::io::Result<()> {
        let list = folders.list();
        if list.is_empty() {
            return Ok(());
        }
        for member in list {
            let moved = if member.set_aside {
                self.set_aside_member(&member)
            } else {
                self.finish_member(&member)
            };
            // A listed member may already be part-moved, so an error, before
            // or after its files begin to move, keeps it listed; only a
            // failed test strikes it.
            if !matches!(moved, Moved::Done | Moved::Left(None)) {
                folders.kept.push(member);
            }
        }
        folders.write(&[])
    }

    /// Step 3: set the opening home's files aside in `<sha1>.gone`, its
    /// record last, the lock file kept, listed in `folders` as a member
    /// while it moves so that a run killed part-way is finished by step 2.
    /// A set-aside standing there recording the folder `current` — this
    /// very folder's state, set aside when another folder stood here — is
    /// kept aside as `<sha1>.taking.gone` for step 4 to take; any other is
    /// replaced.
    fn set_aside(
        &self,
        folders: &mut FoldersLock,
        current: &FolderIdentity,
    ) -> std::io::Result<()> {
        let own = self.own_name();
        let Some(record) = FolderRecord::read(self.own) else {
            return Ok(());
        };
        let gone_name = format!("{own}{SET_ASIDE}");
        let gone = self.repo.join(&gone_name);
        if gone.is_dir() {
            match FolderRecord::read(&gone) {
                Some(older) if older.identity.same_folder(current) => {
                    let taking = self.repo.join(format!("{own}{TAKING}{SET_ASIDE}"));
                    if taking.exists() {
                        std::fs::remove_dir_all(&taking)?;
                    }
                    std::fs::rename(&gone, &taking)?;
                }
                _ => std::fs::remove_dir_all(&gone)?,
            }
        }
        let member = Member {
            leaves: own,
            reaches: gone_name,
            path: record.path.clone().unwrap_or_default(),
            left: record.path.clone(),
            dev: record.identity.dev,
            ino: record.identity.ino,
            born: record.identity.born,
            checkout: record.checkout,
            set_aside: true,
        };
        if folders.kept.iter().any(|kept| kept.meets(&member)) {
            return Err(std::io::Error::other(
                "a carry of this folder's state could not be finished; run the command again",
            ));
        }
        folders.write(std::slice::from_ref(&member))?;
        match self.set_aside_member(&member) {
            Moved::Done | Moved::Left(None) => folders.write(&[]),
            Moved::Left(Some(err)) => {
                folders.write(&[])?;
                Err(err)
            }
            Moved::Broken(err) => {
                folders.kept.push(member);
                folders.write(&[])?;
                Err(err)
            }
        }
    }

    /// Move a listed set-aside's files from the home it leaves into the
    /// set-aside, its record last, where that home still holds the record
    /// the list names; the home's lock held, the opening home's being held
    /// already.
    fn set_aside_member(&self, member: &Member) -> Moved {
        let leaves = self.repo.join(&member.leaves);
        let gone = self.repo.join(&member.reaches);
        if !leaves.is_dir() {
            return Moved::Left(None);
        }
        let _left = if leaves == self.own {
            None
        } else {
            match lock_creating(&leaves.join(LOCK_FILE)) {
                Ok(lock) => Some(lock),
                Err(err) => return Moved::Left(Some(err)),
            }
        };
        if !member.recorded_in(&leaves) {
            return Moved::Left(None);
        }
        let moved = (|| {
            move_state(&leaves, &gone)?;
            std::fs::rename(leaves.join(FOLDER_FILE), gone.join(FOLDER_FILE))?;
            flush_dir_quietly(&gone)?;
            flush_dir_quietly(&leaves)?;
            flush_dir_quietly(self.repo)
        })();
        match moved {
            Ok(()) => Moved::Done,
            Err(err) => Moved::Broken(err),
        }
    }

    /// Step 4's search: the record of the folder `current`, now at `root`,
    /// that no longer stands at its recorded path — a set-aside, or a home
    /// whose path holds nothing, a folder of another identity, or no
    /// longer resolves to itself — the one stamped last where several do.
    /// A record whose path still holds this folder, reached there as well,
    /// is never taken, nor one with no path.
    fn moved_here(&self, current: &FolderIdentity, root: &Path) -> Option<Recorded> {
        self.records()
            .into_iter()
            .filter(|found| found.record.identity.same_folder(current))
            .filter(|found| {
                let Some(path) = found.record.path.as_deref().map(Path::new) else {
                    return false;
                };
                if path == root {
                    return found.set_aside;
                }
                found.set_aside
                    || match std::fs::canonicalize(path) {
                        Err(_) => true,
                        Ok(resolved) if resolved != path => true,
                        Ok(_) => !FolderIdentity::of(path)
                            .is_some_and(|at| at.same_folder(&found.record.identity)),
                    }
            })
            .max_by_key(|found| stamp_of(&self.repo.join(&found.dir)))
    }

    /// Step 4's carry: every copy of the tree moved from where `found`
    /// was recorded to `root`, each whose folder stands at the same place
    /// under the new path and whose home there holds no state, carried in
    /// one step — the list written into `folders.lock` before the first
    /// file moves and cleared after the last, the opening home's own
    /// member moved last.
    fn carry_tree(
        &self,
        folders: &mut FoldersLock,
        found: Recorded,
        root: &Path,
    ) -> std::io::Result<()> {
        let (Some(left), Some(now)) = (found.record.path.as_deref(), root.to_str()) else {
            return Ok(());
        };
        let (from, to) = moved_between(Path::new(left), root);
        let mut candidates = vec![Member {
            leaves: found.dir.clone(),
            reaches: self.own_name(),
            path: now.to_string(),
            left: found.record.path.clone(),
            dev: found.record.identity.dev,
            ino: found.record.identity.ino,
            born: found.record.identity.born,
            checkout: found.record.checkout,
            set_aside: false,
        }];
        for other in self.records() {
            if other.dir == found.dir {
                continue;
            }
            if let Some(member) = self.member(&other, &from, &to) {
                candidates.push(member);
            }
        }
        // A home an unfinished member touches is left to the run that
        // finishes it; where that is this folder's own state, the run fails
        // rather than record the folder without it.
        candidates.retain(|m| !folders.kept.iter().any(|kept| kept.meets(m)));
        if candidates.first().map(|m| &m.leaves) != Some(&found.dir) {
            return Err(std::io::Error::other(
                "a carry of this folder's state could not be finished; run the command again",
            ));
        }
        // No home is reached twice, and none is both left and reached.
        let leaving: std::collections::HashSet<String> =
            candidates.iter().map(|m| m.leaves.clone()).collect();
        let mut reached = std::collections::HashSet::new();
        let members: Vec<Member> = candidates
            .into_iter()
            .filter(|m| !leaving.contains(&m.reaches) && reached.insert(m.reaches.clone()))
            .collect();

        folders.write(&members)?;
        let mut listed = members.clone();
        let own = members.first().cloned();
        let mut failed = None;
        for member in members.iter().skip(1).chain(members.first()) {
            let is_own = Some(member) == own.as_ref();
            match self.carry_member(member) {
                Moved::Done => {}
                // Struck: nothing of it moved, so it is left where it
                // stands and out of the list.
                Moved::Left(err) => {
                    listed.retain(|m| m != member);
                    folders.write(&listed)?;
                    if is_own && let Some(err) = err {
                        failed = Some(err);
                    }
                }
                // Part of it moved: it stays listed, so the next run
                // finishes it whole.
                Moved::Broken(err) => {
                    folders.kept.push(member.clone());
                    if is_own {
                        failed = Some(err);
                    }
                }
            }
        }
        folders.write(&[])?;
        // This folder's own state that could not be moved fails the run, so
        // the next one tries again; another copy's waits for a run in it.
        failed.map_or(Ok(()), Err)
    }

    /// The member `other` makes of the move from `from` to `to`: its path
    /// at or under `from`, the folder at the same place under `to` of its
    /// own identity, and the home for that place another and holding no
    /// state file.
    fn member(&self, other: &Recorded, from: &Path, to: &Path) -> Option<Member> {
        let path = Path::new(other.record.path.as_deref()?);
        let rel = path.strip_prefix(from).ok()?;
        let place = if rel.as_os_str().is_empty() {
            to.to_path_buf()
        } else {
            to.join(rel)
        };
        let place = std::fs::canonicalize(place).ok()?;
        let at = FolderIdentity::of(&place)?;
        if !at.same_folder(&other.record.identity) || self.repo.starts_with(&place) {
            return None;
        }
        let reaches = blob_sha1(place.as_os_str().as_encoded_bytes());
        if reaches == other.dir || holds_state(&self.repo.join(&reaches)) {
            return None;
        }
        Some(Member {
            leaves: other.dir.clone(),
            reaches,
            path: place.to_str()?.to_string(),
            left: other.record.path.clone(),
            dev: other.record.identity.dev,
            ino: other.record.identity.ino,
            born: other.record.identity.born,
            checkout: other.record.checkout,
            set_aside: false,
        })
    }

    /// Take the locks a member's move holds: of the home it leaves — a
    /// set-aside has none — and of the home it reaches, made where absent,
    /// the opening home's being held already.
    fn member_locks(&self, member: &Member) -> std::io::Result<[Option<File>; 2]> {
        let leaves = self.repo.join(&member.leaves);
        let reaches = self.repo.join(&member.reaches);
        let left = if member.leaves.ends_with(SET_ASIDE) || leaves == self.own {
            None
        } else {
            Some(lock_creating(&leaves.join(LOCK_FILE))?)
        };
        std::fs::create_dir_all(&reaches)?;
        let reached = if reaches == self.own {
            None
        } else {
            Some(lock_creating(&reaches.join(LOCK_FILE))?)
        };
        Ok([left, reached])
    }

    /// Write a member's record in the home it reaches with its new path,
    /// then move its files there and remove the record it left, a
    /// set-aside emptied being removed whole: the record goes first, so no
    /// file of it ever stands in a home without a record, which a released
    /// build would adopt as its own folder's.
    fn move_forward(&self, member: &Member, identity: FolderIdentity) -> std::io::Result<()> {
        let leaves = self.repo.join(&member.leaves);
        let reaches = self.repo.join(&member.reaches);
        FolderRecord {
            identity,
            path: Some(member.path.clone()),
            checkout: member.checkout,
        }
        .write(&reaches)
        .map_err(|err| std::io::Error::other(err.to_string()))?;
        move_state(&leaves, &reaches)?;
        remove_if_standing(&leaves.join(FOLDER_FILE))?;
        flush_dir_quietly(&leaves)?;
        if member.leaves.ends_with(SET_ASIDE) {
            std::fs::remove_dir_all(&leaves)?;
            flush_dir_quietly(self.repo)?;
        }
        Ok(())
    }

    /// Move a member's files back into the home it leaves, its record there
    /// put back where it was removed, and the record it reached removed.
    fn move_back(&self, member: &Member) -> std::io::Result<()> {
        let leaves = self.repo.join(&member.leaves);
        let reaches = self.repo.join(&member.reaches);
        move_state(&reaches, &leaves)?;
        if FolderRecord::read(&leaves).is_none() {
            FolderRecord {
                identity: member.identity(),
                path: member.left.clone(),
                checkout: member.checkout,
            }
            .write(&leaves)
            .map_err(|err| std::io::Error::other(err.to_string()))?;
        }
        if member.reached_in(&reaches) {
            remove_if_standing(&reaches.join(FOLDER_FILE))?;
        }
        flush_dir_quietly(&reaches)?;
        flush_dir_quietly(&leaves)
    }

    /// Step 4's move of one member this run listed: only where, tested once
    /// its locks are held, the home it leaves still records its folder,
    /// the folder stands at the path it reaches and the home there holds no
    /// state file.
    fn carry_member(&self, member: &Member) -> Moved {
        if !self.repo.join(&member.leaves).is_dir() {
            return Moved::Left(None);
        }
        let _locks = match self.member_locks(member) {
            Ok(locks) => locks,
            Err(err) => return Moved::Left(Some(err)),
        };
        let standing = FolderIdentity::of(Path::new(&member.path))
            .filter(|at| at.same_folder(&member.identity()));
        let Some(identity) = standing else {
            return Moved::Left(None);
        };
        if !member.recorded_in(&self.repo.join(&member.leaves))
            || holds_state(&self.repo.join(&member.reaches))
        {
            return Moved::Left(None);
        }
        match self.move_forward(member, identity) {
            Ok(()) => Moved::Done,
            Err(err) => Moved::Broken(err),
        }
    }

    /// Step 2's move of one member an earlier run listed, possibly
    /// part-moved: finished or undone whole, never left split. Where the
    /// home it leaves or the home it reaches records another folder now — a
    /// folder made since at that path, by an older build — neither home is
    /// touched.
    /// Otherwise, its folder standing at the path it reaches, the move is
    /// finished; standing anywhere else — moved back, or on — every file is
    /// moved back into the home it leaves, under its record, where a later
    /// run finds the folder as any moved one.
    fn finish_member(&self, member: &Member) -> Moved {
        let leaves = self.repo.join(&member.leaves);
        // A home left is never removed but a set-aside emptied, last.
        if !leaves.is_dir() {
            return Moved::Left(None);
        }
        let _locks = match self.member_locks(member) {
            Ok(locks) => locks,
            Err(err) => return Moved::Left(Some(err)),
        };
        let reaches = self.repo.join(&member.reaches);
        let has_record = |home: &Path| FolderRecord::read(home).is_some();
        if (has_record(&leaves) && !member.recorded_in(&leaves))
            || (has_record(&reaches) && !member.reached_in(&reaches))
        {
            return Moved::Left(None);
        }
        let standing = FolderIdentity::of(Path::new(&member.path))
            .filter(|at| at.same_folder(&member.identity()));
        let moved = match standing {
            Some(identity) => self.move_forward(member, identity),
            None => self.move_back(member),
        };
        match moved {
            Ok(()) => Moved::Done,
            Err(err) => Moved::Broken(err),
        }
    }
}

/// One record a working copy's default home or a set-aside holds, as
/// `syns checkouts` reads it: the repository its folder names, the path
/// and whether the copy is a checkout as recorded, and the home's stamp.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecordedFolder {
    pub owner: String,
    pub name: String,
    pub path: Option<PathBuf>,
    pub checkout: bool,
    pub set_aside: bool,
    pub stamp: Option<u128>,
}

/// Every record under `cache_root`'s `working-copies`, `repo`'s alone
/// where one is named (letter case aside), creating, locking and writing
/// nothing. A cache holding no `working-copies` holds no record; a folder
/// of it that cannot be read is refused as `could not read {path}`.
pub fn recorded_folders(
    cache_root: &Path,
    repo: Option<(&str, &str)>,
) -> Result<Vec<RecordedFolder>, CliError> {
    let dirs_in = |dir: &Path| -> Result<Vec<(String, PathBuf)>, CliError> {
        let entries = match std::fs::read_dir(dir) {
            Ok(entries) => entries,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(err) => {
                return Err(CliError::Io {
                    message: format!("could not read {}: {err}", dir.display()),
                });
            }
        };
        let mut found = Vec::new();
        for entry in entries.flatten() {
            if !entry.file_type().is_ok_and(|kind| kind.is_dir()) {
                continue;
            }
            if let Ok(name) = entry.file_name().into_string() {
                found.push((name, entry.path()));
            }
        }
        Ok(found)
    };
    let mut records = Vec::new();
    for (owner, owner_dir) in dirs_in(&cache_root.join("working-copies"))? {
        if repo.is_some_and(|(wanted, _)| !wanted.eq_ignore_ascii_case(&owner)) {
            continue;
        }
        for (name, name_dir) in dirs_in(&owner_dir)? {
            if repo.is_some_and(|(_, wanted)| !wanted.eq_ignore_ascii_case(&name)) {
                continue;
            }
            for (dir, home) in dirs_in(&name_dir)? {
                let set_aside = dir.ends_with(SET_ASIDE);
                if !set_aside && !is_home_name(&dir) {
                    continue;
                }
                let Some(record) = FolderRecord::read(&home) else {
                    continue;
                };
                records.push(RecordedFolder {
                    owner: owner.to_ascii_lowercase(),
                    name: name.to_ascii_lowercase(),
                    path: record.path.map(PathBuf::from),
                    checkout: record.checkout == Some(true),
                    set_aside,
                    stamp: stamp_of(&home),
                });
            }
        }
    }
    Ok(records)
}

/// Copy `source` over `target` through a sibling renamed over it, the
/// sibling carrying `modified` as its modification time before the rename.
fn copy_keeping_time(
    source: &Path,
    target: &Path,
    modified: std::time::SystemTime,
) -> std::io::Result<()> {
    let dir = target
        .parent()
        .ok_or_else(|| std::io::Error::other("no parent directory"))?;
    let sibling = dir.join(format!(
        ".adopt.{}.{}.tmp",
        std::process::id(),
        SIBLING_COUNTER.fetch_add(1, Ordering::Relaxed)
    ));
    let copied = (|| -> std::io::Result<()> {
        let bytes = std::fs::read(source)?;
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(CONTENT_FILE_MODE);
        }
        let mut file = options.open(&sibling)?;
        file.write_all(&bytes)?;
        file.set_modified(modified)?;
        file.sync_all()?;
        drop(file);
        std::fs::rename(&sibling, target)
    })();
    if copied.is_err() {
        let _ = std::fs::remove_file(&sibling);
    }
    copied?;
    match flush_directory(dir) {
        Ok(()) => Ok(()),
        Err(err) if is_unsupported_flush(&err) => Ok(()),
        Err(err) => Err(err),
    }
}

/// The nanoseconds since the Unix epoch the system clock reads now, in
/// full.
fn now_nanos_wide() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or_default()
}

/// The nanoseconds since the Unix epoch the system clock reads now.
fn now_nanos() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| u64::try_from(d.as_nanos()).unwrap_or(u64::MAX))
        .unwrap_or_default()
}

/// The base a run inside the folder `scope` reads the folder's files
/// against (SPEC u291 Behaviour, `folder_base`): of the folder copy's own
/// base and the base of each copy enclosing it — the holder checkout's
/// and each enclosing folder's — narrowed to the folder and counted from
/// it, the one recorded last, a record carrying no time ranking below
/// every one that does and a tie going to the copy whose root lies
/// outermost. It creates no state.
pub fn folder_base(stores: &StoreRoots, scope: &FolderScope) -> Option<Manifest> {
    // 1 — every record standing, outermost first, each narrowed to the
    // folder: the holder checkout's, each enclosing folder's outermost
    // first, then the folder's own.
    let mut standing: Vec<Manifest> = Vec::new();
    let narrowed = |base: Manifest, under: Option<&str>| -> Manifest {
        let files = base
            .file_paths()
            .filter_map(|path| {
                let from_holder = match under {
                    Some(under) if !under.is_empty() => format!("{under}/{path}"),
                    _ => path.to_string(),
                };
                lies_under(&from_holder, &scope.path).then(|| {
                    (
                        from_holder[scope.path.len() + 1..].to_string(),
                        base.file_sha(path).unwrap_or_default().to_string(),
                    )
                })
            })
            .collect();
        let mut manifest = Manifest::default();
        manifest.update(base.commit_sha().unwrap_or_default().to_string(), files);
        manifest.set_recorded_at(base.recorded_at());
        manifest
    };
    if let Some(checkout) = &scope.checkout
        && let Ok(Some(copy)) =
            WorkingCopy::open_existing(stores, &scope.owner, &scope.name, checkout)
        && let Some(base) = copy.base()
    {
        standing.push(narrowed(base, None));
    }
    for enclosing in scope.enclosing.iter().rev() {
        if let Ok(Some(copy)) = WorkingCopy::open_existing_folder(stores, enclosing)
            && let Some(base) = copy.base()
        {
            standing.push(narrowed(base, Some(&enclosing.path)));
        }
    }
    if let Ok(Some(copy)) = WorkingCopy::open_existing_folder(stores, scope)
        && let Some(base) = copy.base()
    {
        standing.push(base);
    }
    // SPEC u302 Contract Surface, `folder_base`: a folder bound to its
    // identity reads its holder's copy at its directory beside its own.
    if scope.identity.is_some()
        && let Ok(Some(copy)) =
            WorkingCopy::open_existing(stores, &scope.owner, &scope.name, &scope.dir)
        && let Some(base) = copy.base()
    {
        standing.insert(0, base);
    }

    // 2 — the greatest recorded time, the outermost root on a tie.
    let mut chosen: Option<Manifest> = None;
    for record in standing {
        let later = match &chosen {
            None => true,
            Some(best) => record.recorded_at() > best.recorded_at(),
        };
        if later {
            chosen = Some(record);
        }
    }
    chosen
}

/// Copy `source` into `dest` in pieces, answering the blob hash of what
/// was copied; a file whose length moves under the copy is refused.
fn copy_hashing(source: &Path, dest: &mut impl Write) -> std::io::Result<String> {
    let from = File::open(source)?;
    let declared = from.metadata()?.len();
    crate::push::hash::hash_pieces(from, declared, dest)?
        .ok_or_else(|| std::io::Error::other("the file changed while it was copied"))
}

fn read_json<T: DeserializeOwned>(path: &Path) -> Result<Option<T>, CliError> {
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(err) => {
            return Err(CliError::Io {
                message: format!("could not read {}: {err}", path.display()),
            });
        }
    };
    serde_json::from_str(&text)
        .map(Some)
        .map_err(|err| CliError::Io {
            message: format!("could not read {}: {err}", path.display()),
        })
}

fn write_json<T: Serialize>(path: &Path, value: &T) -> Result<(), CliError> {
    let bytes = serde_json::to_vec_pretty(value).map_err(|err| CliError::Io {
        message: format!("could not serialise {}: {err}", path.display()),
    })?;
    write_atomic(path, &bytes, flush_directory)
}

/// Remove a state file, answering whether one was removed.
fn remove_state_file(path: &Path) -> Result<bool, CliError> {
    match std::fs::remove_file(path) {
        Ok(()) => {}
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(err) => {
            return Err(CliError::Io {
                message: format!("could not remove {}: {err}", path.display()),
            });
        }
    }
    match path.parent() {
        Some(dir) => finish_directory_flush(dir, flush_directory(dir)).map(|()| true),
        None => Ok(true),
    }
}

/// The directory flush a state write ends with, injectable so both of
/// its outcomes can be driven.
pub type DirectoryFlush = fn(&Path) -> std::io::Result<()>;

static SIBLING_COUNTER: AtomicU64 = AtomicU64::new(0);

/// Replace `target` with `bytes` whole: write a sibling, flush it,
/// rename it over the target, then flush the directory through
/// `flush_dir`.
///
/// A directory flush the filesystem refuses as unsupported — NFS and
/// FUSE mounts among them — stands once the file's own flush and the
/// rename succeeded; any other refusal fails the write.
pub fn write_atomic(
    target: &Path,
    bytes: &[u8],
    flush_dir: DirectoryFlush,
) -> Result<(), CliError> {
    let dir = target.parent().ok_or_else(|| CliError::Io {
        message: format!("could not write {}: no parent directory", target.display()),
    })?;
    let file_name = target
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_default();
    let sibling = dir.join(format!(
        ".{file_name}.{}.{}.tmp",
        std::process::id(),
        SIBLING_COUNTER.fetch_add(1, Ordering::Relaxed)
    ));

    let written = (|| -> std::io::Result<()> {
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&sibling)?;
        file.write_all(bytes)?;
        file.sync_all()?;
        drop(file);
        std::fs::rename(&sibling, target)
    })();
    if let Err(err) = written {
        let _ = std::fs::remove_file(&sibling);
        return Err(CliError::Io {
            message: format!("could not write {}: {err}", target.display()),
        });
    }

    finish_directory_flush(dir, flush_dir(dir))
}

fn finish_directory_flush(dir: &Path, flushed: std::io::Result<()>) -> Result<(), CliError> {
    match flushed {
        Ok(()) => Ok(()),
        Err(err) if is_unsupported_flush(&err) => Ok(()),
        Err(err) => Err(CliError::Io {
            message: format!("could not flush directory {}: {err}", dir.display()),
        }),
    }
}

/// Flush a file written through a handle since closed, reopened for
/// writing: Windows refuses `FlushFileBuffers` on a handle opened only to
/// read, answering `Access is denied`.
fn flush_file(path: &Path) -> std::io::Result<()> {
    OpenOptions::new().write(true).open(path)?.sync_all()
}

#[cfg(windows)]
fn flush_directory(dir: &Path) -> std::io::Result<()> {
    use std::os::windows::fs::OpenOptionsExt;
    const FILE_FLAG_BACKUP_SEMANTICS: u32 = 0x0200_0000;
    OpenOptions::new()
        .read(true)
        .write(true)
        .custom_flags(FILE_FLAG_BACKUP_SEMANTICS)
        .open(dir)?
        .sync_all()
}

#[cfg(not(windows))]
fn flush_directory(dir: &Path) -> std::io::Result<()> {
    File::open(dir)?.sync_all()
}

#[cfg(unix)]
fn is_unsupported_flush(err: &std::io::Error) -> bool {
    matches!(
        err.raw_os_error(),
        Some(code) if code == libc::EINVAL || code == libc::ENOTSUP || code == libc::EOPNOTSUPP
    )
}

#[cfg(windows)]
fn is_unsupported_flush(err: &std::io::Error) -> bool {
    const ERROR_INVALID_FUNCTION: i32 = 1;
    const ERROR_NOT_SUPPORTED: i32 = 50;
    const ERROR_INVALID_PARAMETER: i32 = 87;
    matches!(
        err.raw_os_error(),
        Some(ERROR_INVALID_FUNCTION | ERROR_NOT_SUPPORTED | ERROR_INVALID_PARAMETER)
    )
}

#[cfg(not(any(unix, windows)))]
fn is_unsupported_flush(_err: &std::io::Error) -> bool {
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    // A name Windows cannot hold, in any segment: one ending in a space
    // or a dot, a reserved device name with or without an extension, or
    // one holding a character Windows refuses; any other name it holds,
    // white space at a name's start included.
    #[test]
    fn not_held_by_windows_names_each_reason() {
        for (path, reason) in [
            ("trail.md ", "a name ending in a space"),
            (" ", "a name ending in a space"),
            ("d-sp /c.md", "a name ending in a space"),
            ("notes.", "a name ending in a dot"),
            ("a/b./c.md", "a name ending in a dot"),
            ("con.md", "the reserved name con"),
            ("x/NUL", "the reserved name NUL"),
            ("Com1.txt", "the reserved name Com1"),
            ("lpt\u{B9}.md", "the reserved name lpt\u{B9}"),
            ("x/a:b.md", "a name holding ':'"),
            ("a\\b.md", "a name holding '\\\\'"),
            ("q?.md", "a name holding '?'"),
        ] {
            assert_eq!(
                not_held_by_windows(path).as_deref(),
                Some(reason),
                "{path:?}"
            );
        }
        for path in [
            " lead.md",
            " sub/x.md",
            "console.md",
            "con-x.md",
            "a.b.md",
            ".env",
            "\u{3000}a",
        ] {
            assert_eq!(not_held_by_windows(path), None, "{path:?}");
        }
        assert_eq!(not_held_here("trail.md ").is_some(), cfg!(windows));
    }

    /// What the volume holding `dir` does, asked by hand: letter case,
    /// then normalisation.
    fn volume_holds_as_one(dir: &Path) -> (bool, bool) {
        let probe = dir.join("Twin-Probe-\u{e9}");
        std::fs::write(&probe, "probe").unwrap();
        let case = dir.join("twin-probe-\u{e9}").exists();
        let normalization = dir.join("Twin-Probe-e\u{301}").exists();
        std::fs::remove_file(&probe).unwrap();
        (case, normalization)
    }

    // The probe reads the volume as a hand-made check does, and leaves
    // nothing behind.
    #[test]
    fn folding_at_reads_what_the_volume_does() {
        let dir = tempfile::tempdir().unwrap();
        let (case, normalization) = volume_holds_as_one(dir.path());
        assert_eq!(
            folding_at(dir.path()),
            Folding {
                case,
                normalization
            }
        );
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 0);
    }

    // The key of every spelling a folding holds as one is one key.
    #[test]
    fn a_folding_keys_twins_alike() {
        let both = Folding::WIDEST;
        assert_eq!(both.key("README.md"), both.key("Readme.md"));
        assert_eq!(both.key("caf\u{e9}.md"), both.key("cafe\u{301}.md"));
        assert_eq!(both.key("\u{c4}"), both.key("\u{e4}"));
        let case = Folding {
            case: true,
            normalization: false,
        };
        assert_ne!(case.key("caf\u{e9}.md"), case.key("cafe\u{301}.md"));
        assert_eq!(Folding::default().key("Readme.md"), "Readme.md");
    }

    // Of each set of spellings the volume holds as one, the one on disk is
    // kept, else the bytewise-smallest; a folder twin takes every path
    // under it; spellings the volume holds apart are all kept.
    #[test]
    fn held_as_one_keeps_one_spelling_of_each_set() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let (case, normalization) = volume_holds_as_one(root);
        let paths = [
            "README.md",
            "Readme.md",
            "Dir/a.md",
            "dir/b.md",
            "dir/c/d.md",
            "cafe\u{301}.md",
            "caf\u{e9}.md",
            "a",
            "A/x.md",
        ];
        let mut want = BTreeMap::new();
        if case {
            want.insert("Readme.md".to_string(), "README.md".to_string());
            want.insert("dir/b.md".to_string(), "Dir".to_string());
            want.insert("dir/c/d.md".to_string(), "Dir".to_string());
            want.insert("a".to_string(), "A".to_string());
        }
        if normalization {
            want.insert("caf\u{e9}.md".to_string(), "cafe\u{301}.md".to_string());
        }
        assert_eq!(held_as_one(root, paths, |_| false), want);
        if case {
            // The spelling the folder holds is kept, whatever its order.
            std::fs::write(root.join("Readme.md"), "on disk").unwrap();
            let kept = held_as_one(root, ["README.md", "Readme.md"], |_| false);
            assert_eq!(
                kept,
                BTreeMap::from([("README.md".to_string(), "Readme.md".to_string())])
            );
            assert!(!stands_as_spelled(root, "README.md"));
            assert!(stands_as_spelled(root, "Readme.md"));
            assert_eq!(
                spelled_otherwise_on_disk(root, "README.md").as_deref(),
                Some("Readme.md")
            );
        }
        assert!(held_as_one(root, ["a.md", "b.md"], |_| false).is_empty());
    }

    // A listing kept from before an entry stood never reads it as absent,
    // even where the directory's modification time stands where it was
    // (NTFS may leave it so): the directory is read afresh on a miss.
    #[test]
    fn a_kept_listing_never_reads_a_new_entry_as_absent() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::write(root.join("a.md"), "a").unwrap();
        assert!(stands_as_spelled(root, "a.md"));
        let before = std::fs::metadata(root).unwrap().modified().unwrap();
        std::fs::create_dir(root.join("Dir")).unwrap();
        std::fs::write(root.join("Dir/b.md"), "b").unwrap();
        #[cfg(unix)]
        std::fs::File::open(root)
            .unwrap()
            .set_modified(before)
            .unwrap();
        let _ = before;
        assert!(stands_as_spelled(root, "Dir"));
        assert!(stands_as_spelled(root, "Dir/b.md"));
        assert_eq!(spelled_otherwise_on_disk(root, "Dir/b.md"), None);
    }

    // The in-root home is judged on the path as served, a name this
    // platform cannot hold on the path counted from the scope, so a scope
    // whose own name Windows cannot hold leaves nothing under it out.
    #[test]
    fn left_out_here_judges_a_name_where_it_stands_on_disk() {
        assert!(!left_out_here("d-sp /c.md", "c.md"));
        assert!(!left_out_here("d-sp /.syns.yaml", ".syns.yaml"));
        assert_eq!(left_out_here("d-sp /t.md ", "t.md "), cfg!(windows));
        assert!(left_out_here("d-sp /.syns-state/x", ".syns-state/x"));
        assert!(left_out_here(".syns-state/x", ".syns-state/x"));
    }

    #[cfg(unix)]
    fn unsupported() -> std::io::Error {
        std::io::Error::from_raw_os_error(libc::EINVAL)
    }

    #[cfg(windows)]
    fn unsupported() -> std::io::Error {
        std::io::Error::from_raw_os_error(1)
    }

    #[cfg(unix)]
    fn failing() -> std::io::Error {
        std::io::Error::from_raw_os_error(libc::EIO)
    }

    #[cfg(windows)]
    fn failing() -> std::io::Error {
        std::io::Error::from_raw_os_error(5)
    }

    #[test]
    fn unsupported_directory_flush_still_replaces_the_file() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("base.json");
        std::fs::write(&target, "old").unwrap();

        let result = write_atomic(&target, b"new", |_| Err(unsupported()));

        assert!(result.is_ok(), "{result:?}");
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "new");
        let leftovers: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(
            leftovers.len(),
            1,
            "a sibling was left behind: {leftovers:?}"
        );
    }

    /// Windows refuses a flush through a handle opened only to read, so
    /// the flush must open its file for writing: a file the account may
    /// write but not read is flushed.
    #[cfg(unix)]
    #[test]
    fn a_closed_file_is_flushed_through_a_handle_that_may_write() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("written");
        std::fs::write(&path, b"written").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o200)).unwrap();
        let flushed = flush_file(&path);
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        flushed.unwrap();
    }

    #[test]
    fn failing_directory_flush_fails_the_write() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("base.json");

        let result = write_atomic(&target, b"new", |_| Err(failing()));

        match result {
            Err(CliError::Io { message }) => {
                assert!(message.contains("could not flush directory"), "{message}")
            }
            other => panic!("expected CliError::Io, got {other:?}"),
        }
    }

    #[test]
    fn open_keys_a_link_and_its_target_to_one_state_directory() {
        let cache = tempfile::tempdir().unwrap();
        let tree = tempfile::tempdir().unwrap();
        let target = tree.path().join("copy");
        std::fs::create_dir_all(target.join("sub")).unwrap();

        let direct = WorkingCopy::open(
            &crate::config::StoreRoots::resolve(Some(cache.path()), cache.path(), cache.path()),
            "alice",
            "proj",
            &target,
        )
        .unwrap();
        let dotted = WorkingCopy::open(
            &crate::config::StoreRoots::resolve(Some(cache.path()), cache.path(), cache.path()),
            "alice",
            "proj",
            &target.join("sub").join(".."),
        )
        .unwrap();
        assert_eq!(direct.state_dir, dotted.state_dir);

        #[cfg(unix)]
        {
            let link = tree.path().join("link");
            std::os::unix::fs::symlink(&target, &link).unwrap();
            let linked = WorkingCopy::open(
                &crate::config::StoreRoots::resolve(Some(cache.path()), cache.path(), cache.path()),
                "alice",
                "proj",
                &link,
            )
            .unwrap();
            assert_eq!(direct.state_dir, linked.state_dir);
            assert_eq!(direct.root, linked.root);
        }

        let other = tree.path().join("other");
        std::fs::create_dir_all(&other).unwrap();
        let elsewhere = WorkingCopy::open(
            &crate::config::StoreRoots::resolve(Some(cache.path()), cache.path(), cache.path()),
            "alice",
            "proj",
            &other,
        )
        .unwrap();
        assert_ne!(direct.state_dir, elsewhere.state_dir);
    }

    #[test]
    fn state_files_round_trip_and_an_unreadable_base_reads_as_none() {
        let cache = tempfile::tempdir().unwrap();
        let tree = tempfile::tempdir().unwrap();
        let copy = WorkingCopy::open(
            &crate::config::StoreRoots::resolve(Some(cache.path()), cache.path(), cache.path()),
            "alice",
            "proj",
            tree.path(),
        )
        .unwrap();

        assert!(copy.base().is_none());
        copy.record_base("h0", HashMap::from([("a.md".into(), "1".into())]))
            .unwrap();
        let base = copy.base().unwrap();
        assert_eq!(base.commit_sha(), Some("h0"));
        assert_eq!(base.file_sha("a.md"), Some("1"));

        std::fs::write(copy.state_dir.join(BASE_FILE), "{torn").unwrap();
        assert!(copy.base().is_none());

        let mut snapshot = Snapshot::new();
        snapshot.insert("t.md".into(), Some(SnapshotContent::Text("text".into())));
        snapshot.insert(
            "b.bin".into(),
            Some(SnapshotContent::Bytes(vec![0xff, 0x00])),
        );
        snapshot.insert("gone.md".into(), None);
        copy.write_local_snapshot(&snapshot).unwrap();
        assert_eq!(copy.local_snapshot().unwrap(), snapshot);

        let outbox = Outbox {
            parent_commit: None,
            tree: BTreeMap::from([("a.md".into(), "2".into())]),
        };
        copy.write_outbox(&outbox).unwrap();
        assert_eq!(copy.outbox().unwrap(), Some(outbox));
        copy.remove_outbox().unwrap();
        copy.remove_outbox().unwrap();
        assert_eq!(copy.outbox().unwrap(), None);
    }

    /// Every path standing under `root`, sorted.
    fn entries_under(root: &Path) -> Vec<PathBuf> {
        fn walk(dir: &Path, out: &mut Vec<PathBuf>) {
            for entry in std::fs::read_dir(dir).into_iter().flatten().flatten() {
                out.push(entry.path());
                walk(&entry.path(), out);
            }
        }
        let mut out = Vec::new();
        walk(root, &mut out);
        out.sort();
        out
    }

    fn q3_scope(w: &Path, checkout: Option<PathBuf>) -> FolderScope {
        FolderScope {
            dir: w.join("clients/vela/q3-board"),
            owner: "alice".into(),
            name: "work".into(),
            path: "clients/vela/q3-board".into(),
            checkout,
            enclosing: Vec::new(),
            identity: None,
        }
    }

    /// Rewrite the base recorded in `copy` with `recorded_at` in its bytes.
    fn stamp(copy: &WorkingCopy, recorded_at: Option<u64>) {
        let base = copy.base().unwrap();
        let files = base
            .file_paths()
            .map(|p| (p.to_string(), base.file_sha(p).unwrap().to_string()))
            .collect();
        copy.record_laid_base(base.commit_sha().unwrap(), files, recorded_at)
            .unwrap();
    }

    // SPEC u291 Tests, `folder_base_narrows_the_holder_base`.
    #[test]
    fn folder_base_narrows_the_holder_base() {
        let cache = tempfile::tempdir().unwrap();
        let tree = tempfile::tempdir().unwrap();
        let w = std::fs::canonicalize(tree.path()).unwrap();
        std::fs::create_dir_all(w.join("clients/vela/q3-board")).unwrap();
        let holder = WorkingCopy::open(
            &crate::config::StoreRoots::resolve(Some(cache.path()), cache.path(), cache.path()),
            "alice",
            "work",
            &w,
        )
        .unwrap();
        let holder_files = HashMap::from([
            (".page/x.json".to_string(), "x1".to_string()),
            ("clients/vela/q3-board/a.md".to_string(), "a1".to_string()),
        ]);
        holder.record_base("h1", holder_files.clone()).unwrap();
        let scope = q3_scope(&w, Some(w.clone()));
        let answer = |scope: &FolderScope| {
            let before = entries_under(cache.path());
            let base = folder_base(
                &crate::config::StoreRoots::resolve(Some(cache.path()), cache.path(), cache.path()),
                scope,
            );
            assert_eq!(
                entries_under(cache.path()),
                before,
                "folder_base wrote state"
            );
            base.map(|b| {
                let mut files: Vec<(String, String)> = b
                    .file_paths()
                    .map(|p| (p.to_string(), b.file_sha(p).unwrap().to_string()))
                    .collect();
                files.sort();
                (b.commit_sha().unwrap().to_string(), files)
            })
        };
        let h1 = Some((
            "h1".to_string(),
            vec![("a.md".to_string(), "a1".to_string())],
        ));
        let h0 = Some((
            "h0".to_string(),
            vec![("b.md".to_string(), "b0".to_string())],
        ));

        assert_eq!(answer(&scope), h1);
        assert_eq!(answer(&q3_scope(&w, None)), None);

        let folder = WorkingCopy::open_folder(
            &crate::config::StoreRoots::resolve(Some(cache.path()), cache.path(), cache.path()),
            &scope,
        )
        .unwrap();
        folder
            .record_base(
                "h0",
                HashMap::from([("b.md".to_string(), "b0".to_string())]),
            )
            .unwrap();
        holder.record_base("h1", holder_files).unwrap();
        assert_eq!(answer(&scope), h1);

        stamp(&holder, Some(7));
        stamp(&folder, Some(7));
        assert_eq!(answer(&scope), h1);

        stamp(&holder, None);
        assert_eq!(answer(&scope), h0);
    }

    /// `U/q3-plan` bound to `alice/docs-q3-plan`, beside the cache.
    fn identity_folder() -> (tempfile::TempDir, tempfile::TempDir, PathBuf, StoreRoots) {
        let cache = tempfile::tempdir().unwrap();
        let tree = tempfile::tempdir().unwrap();
        let u = std::fs::canonicalize(tree.path()).unwrap();
        std::fs::create_dir_all(u.join("q3-plan")).unwrap();
        std::fs::write(
            u.join("q3-plan/.syns.yaml"),
            "holder: alice/docs\npath: q3-plan\nshared_as: docs-q3-plan\n",
        )
        .unwrap();
        let stores = StoreRoots::resolve(Some(cache.path()), cache.path(), cache.path());
        (cache, tree, u, stores)
    }

    fn base_of(copy: &WorkingCopy) -> (String, Vec<(String, String)>, Option<u64>) {
        let base = copy.base().expect("a base");
        let mut files: Vec<(String, String)> = base
            .file_paths()
            .map(|p| (p.to_string(), base.file_sha(p).unwrap().to_string()))
            .collect();
        files.sort();
        (
            base.commit_sha().unwrap().to_string(),
            files,
            base.recorded_at(),
        )
    }

    // SPEC u302 Tests, `the_base_recorded_under_the_holder_carries_to_the_identity`.
    #[test]
    fn the_base_recorded_under_the_holder_carries_to_the_identity() {
        let (_cache, _tree, u, stores) = identity_folder();
        let holder = WorkingCopy::open(&stores, "alice", "docs", &u.join("q3-plan")).unwrap();
        holder
            .record_base(
                "h4",
                HashMap::from([("document.html".to_string(), "b1".to_string())]),
            )
            .unwrap();
        let recorded = base_of(&holder);
        assert!(
            WorkingCopy::open_existing(&stores, "alice", "docs-q3-plan", &u.join("q3-plan"))
                .unwrap()
                .is_none()
        );

        let scope = crate::repo::folder::resolve_folder_scope(&u.join("q3-plan"))
            .unwrap()
            .expect("a scope");
        let copy = WorkingCopy::open_folder(&stores, &scope).unwrap();
        assert_eq!(
            (copy.owner.as_str(), copy.name.as_str()),
            ("alice", "docs-q3-plan")
        );
        assert_eq!(base_of(&copy), recorded);
        assert_eq!(base_of(&holder), recorded);
        assert_ne!(copy.state_dir, holder.state_dir);
    }

    // SPEC u302 Tests, `carry_base_never_overwrites_a_later_base`.
    #[test]
    fn carry_base_never_overwrites_a_later_base() {
        let (_cache, _tree, u, stores) = identity_folder();
        let dir = u.join("q3-plan");
        let holder = WorkingCopy::open(&stores, "alice", "docs", &dir).unwrap();
        let identity = WorkingCopy::open(&stores, "alice", "docs-q3-plan", &dir).unwrap();
        let files = HashMap::from([("document.html".to_string(), "b1".to_string())]);
        holder.record_base("h4", files.clone()).unwrap();
        identity.record_base("h7", files).unwrap();
        stamp(&holder, Some(4));
        stamp(&identity, Some(7));

        assert!(!identity.carry_base("alice/docs").unwrap());
        assert_eq!(base_of(&identity).0, "h7");
        assert!(holder.carry_base("alice/docs-q3-plan").unwrap());
        assert_eq!(base_of(&holder), base_of(&identity));
        assert_eq!(base_of(&holder).2, Some(7));

        stamp(&identity, None);
        assert!(!holder.carry_base("alice/docs-q3-plan").unwrap());
    }

    // SPEC u291 Contract Surface, `WorkingCopy::open_folder`: a folder and
    // its holder's checkout never share a state directory.
    #[test]
    fn a_folder_copy_and_its_checkout_hold_two_state_directories() {
        let cache = tempfile::tempdir().unwrap();
        let tree = tempfile::tempdir().unwrap();
        let w = std::fs::canonicalize(tree.path()).unwrap();
        std::fs::create_dir_all(w.join("clients/vela/q3-board")).unwrap();
        let scope = q3_scope(&w, Some(w.clone()));
        assert_eq!(
            WorkingCopy::open_existing_folder(
                &crate::config::StoreRoots::resolve(Some(cache.path()), cache.path(), cache.path()),
                &scope
            )
            .unwrap(),
            None
        );
        let folder = WorkingCopy::open_folder(
            &crate::config::StoreRoots::resolve(Some(cache.path()), cache.path(), cache.path()),
            &scope,
        )
        .unwrap();
        let holder = WorkingCopy::open(
            &crate::config::StoreRoots::resolve(Some(cache.path()), cache.path(), cache.path()),
            "alice",
            "work",
            &w,
        )
        .unwrap();
        assert_ne!(folder.state_dir, holder.state_dir);
        assert_eq!(folder.folder.as_ref(), Some(&scope));
        assert_eq!(holder.folder, None);
        assert_eq!(folder.stores().write, cache.path());
        assert_eq!(
            WorkingCopy::open_existing_folder(
                &crate::config::StoreRoots::resolve(Some(cache.path()), cache.path(), cache.path()),
                &scope
            )
            .unwrap()
            .map(|c| c.state_dir),
            Some(folder.state_dir)
        );
    }

    #[test]
    fn open_refuses_a_state_directory_inside_the_working_copy() {
        let tree = tempfile::tempdir().unwrap();
        let cache = tree.path().join(".cache");
        let result = WorkingCopy::open(
            &crate::config::StoreRoots::resolve(Some(&cache), &cache, &cache),
            "alice",
            "proj",
            tree.path(),
        );
        assert!(matches!(result, Err(CliError::Io { .. })), "{result:?}");
    }

    // ---- u280: the stat record and stored snapshot content -------------

    fn open_copy() -> (tempfile::TempDir, tempfile::TempDir, WorkingCopy) {
        let cache = tempfile::tempdir().unwrap();
        let tree = tempfile::tempdir().unwrap();
        let copy = WorkingCopy::open(
            &crate::config::StoreRoots::resolve(Some(cache.path()), cache.path(), cache.path()),
            "alice",
            "proj",
            tree.path(),
        )
        .unwrap();
        (cache, tree, copy)
    }

    // ---- u304: a base laid with a landed version's paths -----------------

    /// A fresh copy recording base `h1` with `a.md` at `b1` and the
    /// recorded time `7`.
    fn copy_at_h1() -> (tempfile::TempDir, tempfile::TempDir, WorkingCopy) {
        let (cache, tree, copy) = open_copy();
        copy.record_laid_base(
            "h1",
            HashMap::from([("a.md".to_string(), "b1".to_string())]),
            Some(7),
        )
        .unwrap();
        (cache, tree, copy)
    }

    fn laid(copy: &WorkingCopy) -> (Option<String>, Vec<(String, String)>, Option<u64>) {
        let base = copy.base().expect("a base");
        let mut files: Vec<(String, String)> = base
            .file_paths()
            .map(|p| (p.to_string(), base.file_sha(p).unwrap().to_string()))
            .collect();
        files.sort();
        (
            base.commit_sha().map(str::to_string),
            files,
            base.recorded_at(),
        )
    }

    /// Each state file the test wrote, at the bytes it holds.
    fn written(copy: &WorkingCopy) -> Vec<(&'static str, Option<Vec<u8>>)> {
        [BASE_FILE, RESOLUTION_FILE, OUTBOX_FILE, STATE_STAMP]
            .into_iter()
            .map(|name| (name, std::fs::read(copy.state_dir.join(name)).ok()))
            .collect()
    }

    // SPEC u304 Tests, the row of this name.
    #[test]
    fn lay_files_lays_over_a_clean_base_advancing_only_from_the_named_commit() {
        let files = HashMap::from([("x/y.md".to_string(), "b2".to_string())]);
        let carried = vec![
            ("a.md".to_string(), "b1".to_string()),
            ("x/y.md".to_string(), "b2".to_string()),
        ];

        let (_cache, _tree, copy) = copy_at_h1();
        assert!(copy.lay_files(&files, Some("h1"), "h3").unwrap());
        assert_eq!(laid(&copy), (Some("h3".into()), carried.clone(), Some(7)));

        let (_cache, _tree, copy) = copy_at_h1();
        assert!(copy.lay_files(&files, Some("h2"), "h3").unwrap());
        assert_eq!(laid(&copy), (Some("h1".into()), carried, Some(7)));
    }

    // SPEC u304 Tests, the row of this name.
    #[test]
    fn lay_files_leaves_a_copy_holding_work_or_no_base_as_it_stood() {
        let files = HashMap::from([("x/y.md".to_string(), "b2".to_string())]);

        let (_cache, _tree, copy) = copy_at_h1();
        copy.write_resolution(&Resolution {
            recovery_id: "r".into(),
            base_commit: Some("h1".into()),
            head_commit: "h0".into(),
            round: 1,
            local_paths: Vec::new(),
            remote_paths: Vec::new(),
            collisions: Vec::new(),
            combined_paths: Vec::new(),
            reviewed_tree: None,
            pending_writes: None,
        })
        .unwrap();
        let before = written(&copy);
        assert!(!copy.lay_files(&files, Some("h1"), "h3").unwrap());
        assert_eq!(written(&copy), before, "a resolution standing");

        let (_cache, _tree, copy) = copy_at_h1();
        copy.write_outbox(&Outbox {
            parent_commit: Some("h1".into()),
            tree: BTreeMap::new(),
        })
        .unwrap();
        let before = written(&copy);
        assert!(!copy.lay_files(&files, Some("h1"), "h3").unwrap());
        assert_eq!(written(&copy), before, "an outbox standing");

        let (_cache, _tree, copy) = open_copy();
        let listed = |copy: &WorkingCopy| {
            std::fs::read_dir(&copy.state_dir)
                .map(|dir| dir.flatten().map(|e| e.file_name()).collect::<Vec<_>>())
                .unwrap_or_default()
        };
        let standing = listed(&copy);
        assert!(!copy.lay_files(&files, Some("h1"), "h3").unwrap());
        assert!(copy.base().is_none());
        assert_eq!(listed(&copy), standing, "no state created");
    }

    #[test]
    fn the_stat_record_round_trips_and_an_unreadable_one_loads_empty() {
        let (_cache, _tree, copy) = open_copy();
        assert_eq!(copy.stat_record(), StatRecord::default());

        let mut record = StatRecord::default();
        record.entries.insert(
            "a.png".into(),
            StatEntry {
                size: 6,
                mtime: (1, 2),
                ctime: (3, 4),
                inode: 5,
                sha: "0".repeat(40),
            },
        );
        record.stamp = Some((7, 8));
        copy.write_stat_record(&record).unwrap();
        assert_eq!(copy.stat_record(), record);

        std::fs::write(copy.state_dir.join(STAT_RECORD_FILE), "{torn").unwrap();
        assert_eq!(copy.stat_record(), StatRecord::default());
    }

    #[cfg(unix)]
    #[test]
    fn an_entry_whose_mtime_equals_the_stamp_is_not_trusted() {
        use std::os::unix::fs::MetadataExt;
        let (_cache, tree, _copy) = open_copy();
        let file = tree.path().join("a.png");
        std::fs::write(&file, b"\x89PNG\x00").unwrap();
        let meta = std::fs::metadata(&file).unwrap();
        let mut record = StatRecord::default();
        record.observe("a.png", &meta, &meta, "sha");
        record.stamp = Some((meta.mtime(), meta.mtime_nsec()));
        assert_eq!(record.trusted("a.png", &meta), None);
        record.stamp = Some((meta.mtime().max(meta.ctime()) + 1, 0));
        assert_eq!(record.trusted("a.png", &meta).as_deref(), Some("sha"));
    }

    #[cfg(unix)]
    #[test]
    fn a_restamp_leaves_no_probe_behind() {
        let (_cache, tree, _copy) = open_copy();
        let mut record = StatRecord::default();
        record.restamp(tree.path());
        assert!(record.stamp.is_some());
        assert_eq!(std::fs::read_dir(tree.path()).unwrap().count(), 0);
    }

    #[test]
    fn a_stored_content_round_trips_by_its_hash() {
        let (_cache, _tree, copy) = open_copy();
        let bytes = [0x89u8, b'P', b'N', b'G', 0x00, 0xff];
        let (sha, created) = copy.store_bytes(&bytes).unwrap();
        assert!(created);
        assert_eq!(sha, blob_sha1(&bytes));
        let content = SnapshotContent::Stored {
            stored: sha.clone(),
        };
        assert_eq!(copy.content_bytes(&content).unwrap(), bytes.to_vec());
        let json = serde_json::to_string(&content).unwrap();
        assert_eq!(json, format!("{{\"stored\":\"{sha}\"}}"));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let dir = std::fs::metadata(copy.snapshot_content_dir()).unwrap();
            assert_eq!(dir.permissions().mode() & 0o777, 0o700);
            let file = std::fs::metadata(copy.snapshot_content_dir().join(&sha)).unwrap();
            assert_eq!(file.permissions().mode() & 0o777, 0o600);
        }
    }

    /// A collected file is stored by its hash, held or copied from the
    /// folder, and storing it again over the standing content succeeds.
    #[test]
    fn a_collected_file_is_stored_by_its_hash() {
        use crate::push::collector::CollectedFile;
        let (_cache, tree, copy) = open_copy();
        let bytes = [0x89u8, b'P', b'N', b'G', 0x00, 0xff];
        std::fs::write(tree.path().join("a.png"), bytes).unwrap();
        let unheld = CollectedFile {
            sha: blob_sha1(&bytes),
            bytes: None,
        };
        let (sha, created) = copy.store_collected(tree.path(), "a.png", &unheld).unwrap();
        assert!(created);
        assert_eq!(sha, blob_sha1(&bytes));
        let (again, created) = copy.store_collected(tree.path(), "a.png", &unheld).unwrap();
        assert_eq!(again, sha);
        assert!(!created);
        let content = SnapshotContent::Stored { stored: sha };
        assert_eq!(copy.content_bytes(&content).unwrap(), bytes.to_vec());
    }

    #[test]
    fn a_content_file_no_longer_hashing_to_its_name_is_refused() {
        let (_cache, _tree, copy) = open_copy();
        let (sha, _) = copy.store_bytes(b"local\n").unwrap();
        std::fs::write(copy.snapshot_content_dir().join(&sha), b"tampered\n").unwrap();
        match copy.stored_content(&sha) {
            Err(CliError::Io { message }) => {
                assert!(
                    message.contains("content does not match its hash"),
                    "{message}"
                )
            }
            other => panic!("expected the refusal, got {other:?}"),
        }
    }

    #[test]
    fn an_orphaned_content_file_is_removed_on_the_next_snapshot_write() {
        let (_cache, _tree, copy) = open_copy();
        let (kept, _) = copy.store_bytes(b"kept\n").unwrap();
        let (orphan, _) = copy.store_bytes(b"orphan\n").unwrap();
        let mut snapshot = Snapshot::new();
        snapshot.insert(
            "a.md".into(),
            Some(SnapshotContent::Stored {
                stored: kept.clone(),
            }),
        );
        copy.write_local_snapshot(&snapshot).unwrap();
        assert!(copy.snapshot_content_dir().join(&kept).exists());
        assert!(!copy.snapshot_content_dir().join(&orphan).exists());

        copy.remove_snapshots().unwrap();
        assert!(!copy.snapshot_content_dir().exists());
    }

    /// CR1-4: a store a killed run left part-way is removed by the next
    /// prune, so the directory goes with the snapshots.
    #[test]
    fn a_killed_runs_incoming_content_is_pruned() {
        let (_cache, _tree, copy) = open_copy();
        copy.store_bytes(b"kept\n").unwrap();
        std::fs::write(copy.snapshot_content_dir().join(".incoming.1.0"), b"torn").unwrap();
        copy.remove_snapshots().unwrap();
        assert!(!copy.snapshot_content_dir().exists());
    }

    #[test]
    fn a_released_builds_inline_snapshot_still_loads() {
        let (_cache, _tree, copy) = open_copy();
        std::fs::write(
            copy.local_snapshot_path(),
            r#"{"image.png":[137,80,78,71,0,255],"a.md":"a\n","gone.md":null}"#,
        )
        .unwrap();
        let snapshot = copy.local_snapshot().unwrap();
        assert_eq!(
            snapshot["image.png"],
            Some(SnapshotContent::Bytes(vec![137, 80, 78, 71, 0, 255]))
        );
        assert_eq!(snapshot["a.md"], Some(SnapshotContent::Text("a\n".into())));
        assert_eq!(snapshot["gone.md"], None);
    }

    // ---- u298: the two homes ------------------------------------------

    /// A scratch default root, fallback root and folder, with the roots a
    /// run answers where the default root takes writes and where it
    /// refuses them.
    struct Homes {
        _scratch: tempfile::TempDir,
        default: PathBuf,
        root: PathBuf,
        writable: StoreRoots,
        refused: StoreRoots,
    }

    fn homes() -> Homes {
        let scratch = tempfile::tempdir().unwrap();
        let base = std::fs::canonicalize(scratch.path()).unwrap();
        let default = base.join("default");
        let fallback = base.join("fallback");
        let root = base.join("folder");
        std::fs::create_dir_all(&default).unwrap();
        std::fs::create_dir_all(&fallback).unwrap();
        std::fs::create_dir_all(&root).unwrap();
        Homes {
            writable: StoreRoots {
                default: default.clone(),
                write: default.clone(),
                default_refused: false,
            },
            refused: StoreRoots {
                default: default.clone(),
                write: fallback,
                default_refused: true,
            },
            _scratch: scratch,
            default,
            root,
        }
    }

    fn files(pairs: &[(&str, &str)]) -> HashMap<String, String> {
        pairs
            .iter()
            .map(|(p, s)| (p.to_string(), s.to_string()))
            .collect()
    }

    fn commit_of(copy: &WorkingCopy) -> Option<String> {
        copy.base().and_then(|b| b.commit_sha().map(String::from))
    }

    fn set_stamp(home: &Path, value: u128) {
        std::fs::write(home.join(STATE_STAMP), value.to_string()).unwrap();
    }

    #[cfg(unix)]
    fn mode_of(path: &Path) -> u32 {
        use std::os::unix::fs::PermissionsExt;
        std::fs::metadata(path).unwrap().permissions().mode() & 0o777
    }

    /// Whether another open file can take the lock at `path` now.
    fn lock_is_free(path: &Path) -> bool {
        let file = File::open(path).unwrap();
        let free = file.try_lock().is_ok();
        if free {
            file.unlock().unwrap();
        }
        free
    }

    // SPEC u298 Tests, `in_root_home_is_confined_and_ignored_by_git`.
    #[test]
    fn in_root_home_is_confined_and_ignored_by_git() {
        let h = homes();
        let copy = WorkingCopy::open(&h.refused, "alice", "proj", &h.root).unwrap();
        let home = h.root.join(IN_ROOT_HOME);
        assert_eq!(copy.state_dir, home);
        assert!(home.is_dir());
        #[cfg(unix)]
        assert_eq!(mode_of(&home), 0o700);
        assert_eq!(
            std::fs::read_to_string(home.join(".gitignore"))
                .unwrap()
                .lines()
                .collect::<Vec<_>>(),
            vec!["*"]
        );
        assert!(
            !h.default.join("working-copies").exists(),
            "a refused run created the default home"
        );
    }

    // SPEC u298 Tests, `in_root_home_standing_as_a_link_is_refused`.
    #[cfg(unix)]
    #[test]
    fn in_root_home_standing_as_a_link_is_refused() {
        let h = homes();
        let elsewhere = h.default.parent().unwrap().join("elsewhere");
        std::fs::create_dir_all(&elsewhere).unwrap();
        set_stamp(&elsewhere, 9);
        let mut base = Manifest::default();
        base.update("h9".into(), files(&[("a.md", "9")]));
        std::fs::write(
            elsewhere.join(BASE_FILE),
            serde_json::to_vec(&base).unwrap(),
        )
        .unwrap();
        let before = entries_under(&elsewhere);
        let link = h.root.join(IN_ROOT_HOME);
        std::os::unix::fs::symlink(&elsewhere, &link).unwrap();

        match WorkingCopy::open(&h.refused, "alice", "proj", &h.root) {
            Err(CliError::Io { message }) => {
                assert!(
                    message.contains("could not create working copy state"),
                    "{message}"
                )
            }
            other => panic!("expected the refusal, got {other:?}"),
        }
        assert_eq!(std::fs::read_link(&link).unwrap(), elsewhere);
        assert_eq!(entries_under(&elsewhere), before);

        // As the other home, it is read as holding no state.
        let copy = WorkingCopy::open(&h.writable, "alice", "proj", &h.root).unwrap();
        assert_eq!(commit_of(&copy), None);
        let lock = copy.lock().unwrap();
        assert!(!lock.holds_other());
        assert_eq!(commit_of(&copy), None);
    }

    // SPEC u298 Tests, `reads_answer_the_state_stamped_last`.
    #[test]
    fn reads_answer_the_state_stamped_last() {
        let h = homes();
        let write = WorkingCopy::open(&h.writable, "alice", "proj", &h.root).unwrap();
        let other = WorkingCopy::open(&h.refused, "alice", "proj", &h.root).unwrap();
        write.record_base("h1", files(&[("a.md", "1")])).unwrap();
        other.record_base("h2", files(&[("a.md", "2")])).unwrap();
        set_stamp(&write.state_dir, 100);
        set_stamp(&other.state_dir, 200);
        assert_eq!(commit_of(&write).as_deref(), Some("h2"));
        set_stamp(&other.state_dir, 100);
        assert_eq!(commit_of(&write).as_deref(), Some("h1"));
        // Seen from the other side, the tie goes to its own write home.
        assert_eq!(commit_of(&other).as_deref(), Some("h2"));
    }

    // SPEC u298 Contract Surface, `WorkingCopy` reads: a home holding no
    // stamp is ordered before every home holding one, and two holding
    // none by the newest modification time among their state files.
    #[test]
    fn unstamped_homes_are_ordered_by_their_newest_state_file() {
        let h = homes();
        let write = WorkingCopy::open(&h.writable, "alice", "proj", &h.root).unwrap();
        let other = WorkingCopy::open(&h.refused, "alice", "proj", &h.root).unwrap();
        write.record_base("h1", files(&[("a.md", "1")])).unwrap();
        other.record_base("h2", files(&[("a.md", "2")])).unwrap();
        std::fs::remove_file(write.state_dir.join(STATE_STAMP)).unwrap();
        // A stamped home is ordered after an unstamped one.
        assert_eq!(commit_of(&write).as_deref(), Some("h2"));
        std::fs::remove_file(other.state_dir.join(STATE_STAMP)).unwrap();
        let at = |secs: u64| std::time::UNIX_EPOCH + std::time::Duration::from_secs(secs);
        let touch = |path: PathBuf, secs: u64| {
            File::options()
                .write(true)
                .open(path)
                .unwrap()
                .set_modified(at(secs))
                .unwrap();
        };
        touch(write.state_dir.join(BASE_FILE), 2_000);
        touch(other.state_dir.join(BASE_FILE), 1_000);
        assert_eq!(commit_of(&write).as_deref(), Some("h1"));
        touch(other.state_dir.join(BASE_FILE), 3_000);
        assert_eq!(commit_of(&write).as_deref(), Some("h2"));
    }

    // SPEC u298 Tests, `open_existing_finds_state_in_either_home`.
    #[test]
    fn open_existing_finds_state_in_either_home() {
        let h = homes();
        assert_eq!(
            WorkingCopy::open_existing(&h.writable, "alice", "proj", &h.root).unwrap(),
            None
        );
        let other = WorkingCopy::open(&h.refused, "alice", "proj", &h.root).unwrap();
        other.record_base("h2", files(&[("a.md", "2")])).unwrap();
        let found = WorkingCopy::open_existing(&h.writable, "alice", "proj", &h.root)
            .unwrap()
            .expect("the copy whose state stands in its in-root home");
        assert_eq!(commit_of(&found).as_deref(), Some("h2"));
        assert!(
            !found.state_dir.exists(),
            "open_existing created the write home"
        );
        assert!(!h.default.join("working-copies").exists());
    }

    // SPEC u298 Tests, `folder_base_reads_a_holder_base_in_its_default_home`.
    #[test]
    fn folder_base_reads_a_holder_base_in_its_default_home() {
        let h = homes();
        let w = h.root.clone();
        std::fs::create_dir_all(w.join("clients/vela/q3-board")).unwrap();
        let holder = WorkingCopy::open(&h.writable, "alice", "work", &w).unwrap();
        holder
            .record_base("h1", files(&[("clients/vela/q3-board/a.md", "a1")]))
            .unwrap();
        let scope = q3_scope(&w, Some(w.clone()));
        let base = folder_base(&h.refused, &scope).expect("the holder's base");
        assert_eq!(base.commit_sha(), Some("h1"));
        assert_eq!(base.file_paths().collect::<Vec<_>>(), vec!["a.md"]);
        assert_eq!(base.file_sha("a.md"), Some("a1"));
        assert!(!w.join(IN_ROOT_HOME).exists(), "folder_base wrote state");
    }

    /// Hold the lock at `path` through another open file from a thread
    /// for a while, answering when it was released.
    fn hold_from_a_thread(
        path: PathBuf,
        writable: bool,
    ) -> std::thread::JoinHandle<std::time::Instant> {
        let (taken, wait) = std::sync::mpsc::channel();
        let holder = std::thread::spawn(move || {
            let file = OpenOptions::new()
                .read(true)
                .write(writable)
                .open(&path)
                .unwrap();
            file.lock().unwrap();
            taken.send(()).unwrap();
            std::thread::sleep(std::time::Duration::from_millis(400));
            let released = std::time::Instant::now();
            file.unlock().unwrap();
            released
        });
        wait.recv().unwrap();
        holder
    }

    // SPEC u298 Tests, `lock_queues_behind_the_other_homes_lock`.
    #[test]
    fn lock_queues_behind_the_other_homes_lock() {
        let h = homes();
        let sandboxed = WorkingCopy::open(&h.refused, "alice", "proj", &h.root).unwrap();
        drop(sandboxed.lock().unwrap());
        let copy = WorkingCopy::open(&h.writable, "alice", "proj", &h.root).unwrap();
        let in_root_lock = h.root.join(IN_ROOT_HOME).join(LOCK_FILE);
        let holder = hold_from_a_thread(in_root_lock.clone(), true);
        let lock = copy.lock().unwrap();
        let returned = std::time::Instant::now();
        let released = holder.join().unwrap();
        assert!(returned >= released, "the lock did not wait");
        assert!(lock.holds_other());
        assert!(!lock_is_free(&copy.state_dir.join(LOCK_FILE)));
        assert!(!lock_is_free(&in_root_lock));
        drop(lock);
        assert!(lock_is_free(&in_root_lock));
    }

    // SPEC u298 Tests, `lock_queues_behind_a_read_only_default_lock`.
    #[cfg(unix)]
    #[test]
    fn lock_queues_behind_a_read_only_default_lock() {
        use std::os::unix::fs::PermissionsExt;
        if unsafe { libc::geteuid() } == 0 {
            return;
        }
        let h = homes();
        let unsandboxed = WorkingCopy::open(&h.writable, "alice", "proj", &h.root).unwrap();
        drop(unsandboxed.lock().unwrap());
        let default_lock = unsandboxed.state_dir.join(LOCK_FILE);
        std::fs::set_permissions(&default_lock, std::fs::Permissions::from_mode(0o400)).unwrap();
        let copy = WorkingCopy::open(&h.refused, "alice", "proj", &h.root).unwrap();
        let holder = hold_from_a_thread(default_lock.clone(), false);
        let lock = copy.lock().unwrap();
        let returned = std::time::Instant::now();
        let released = holder.join().unwrap();
        assert!(returned >= released, "the lock did not wait");
        assert!(lock.holds_other());
        assert!(!lock_is_free(&default_lock));
        assert!(!lock_is_free(&copy.state_dir.join(LOCK_FILE)));
        drop(lock);
        std::fs::set_permissions(&default_lock, std::fs::Permissions::from_mode(0o600)).unwrap();
    }

    // SPEC u298 Tests, `lock_adopts_an_unstamped_default_home`.
    #[test]
    fn lock_adopts_an_unstamped_default_home() {
        let h = homes();
        let released = WorkingCopy::open(&h.writable, "alice", "proj", &h.root).unwrap();
        released.record_base("h1", files(&[("a.md", "1")])).unwrap();
        std::fs::remove_file(released.state_dir.join(STATE_STAMP)).unwrap();
        assert!(!h.root.join(IN_ROOT_HOME).exists());

        let copy = WorkingCopy::open(&h.refused, "alice", "proj", &h.root).unwrap();
        let lock = copy.lock().unwrap();
        assert_eq!(commit_of(&copy).as_deref(), Some("h1"));
        let adopted: Manifest = serde_json::from_slice(
            &std::fs::read(h.root.join(IN_ROOT_HOME).join(BASE_FILE)).unwrap(),
        )
        .unwrap();
        assert_eq!(adopted.commit_sha(), Some("h1"));
        drop(lock);
        // Adopted once: the homes now tie, and the next lock copies nothing.
        assert!(!copy.other_stamped_later());
    }

    // SPEC u298 Tests, `writable_default_run_creates_no_in_root_home`.
    #[test]
    fn writable_default_run_creates_no_in_root_home() {
        let h = homes();
        let copy = WorkingCopy::open(&h.writable, "alice", "proj", &h.root).unwrap();
        let lock = copy.lock().unwrap();
        assert!(!lock.holds_other());
        assert!(!lock_is_free(&copy.state_dir.join(LOCK_FILE)));
        copy.record_base("h1", files(&[("a.md", "1")])).unwrap();
        drop(lock);
        assert!(!h.root.join(IN_ROOT_HOME).exists());
    }

    // SPEC u298 Tests, `lock_adopts_state_stamped_later`.
    #[test]
    fn lock_adopts_state_stamped_later() {
        let h = homes();
        let write = WorkingCopy::open(&h.writable, "alice", "proj", &h.root).unwrap();
        let other = WorkingCopy::open(&h.refused, "alice", "proj", &h.root).unwrap();
        write.record_base("h1", files(&[("a.md", "1")])).unwrap();
        write
            .write_resolution(&Resolution {
                recovery_id: "r".into(),
                base_commit: Some("h1".into()),
                head_commit: "h0".into(),
                round: 1,
                local_paths: Vec::new(),
                remote_paths: Vec::new(),
                collisions: Vec::new(),
                combined_paths: Vec::new(),
                reviewed_tree: None,
                pending_writes: None,
            })
            .unwrap();
        other.record_base("h2", files(&[("a.md", "2")])).unwrap();
        let outbox = Outbox {
            parent_commit: Some("h2".into()),
            tree: BTreeMap::from([("a.md".into(), "3".into())]),
        };
        other.write_outbox(&outbox).unwrap();
        let (stored, _) = other.store_bytes(b"theirs\n").unwrap();
        let mut snapshot = Snapshot::new();
        snapshot.insert(
            "a.md".into(),
            Some(SnapshotContent::Stored {
                stored: stored.clone(),
            }),
        );
        other.write_local_snapshot(&snapshot).unwrap();
        set_stamp(&write.state_dir, 100);
        set_stamp(&other.state_dir, 200);

        let lock = write.lock().unwrap();
        assert_eq!(commit_of(&write).as_deref(), Some("h2"));
        assert_eq!(write.outbox().unwrap(), Some(outbox));
        assert_eq!(write.resolution().unwrap(), None);
        assert!(!write.state_dir.join(RESOLUTION_FILE).exists());
        assert_eq!(
            std::fs::read_to_string(write.state_dir.join(STATE_STAMP)).unwrap(),
            "200"
        );
        assert_eq!(
            write
                .content_bytes(&SnapshotContent::Stored { stored })
                .unwrap(),
            b"theirs\n"
        );
        for name in [BASE_FILE, OUTBOX_FILE, LOCAL_SNAPSHOT_FILE] {
            let copied = std::fs::metadata(write.state_dir.join(name)).unwrap();
            let source = std::fs::metadata(other.state_dir.join(name)).unwrap();
            assert_eq!(
                copied.modified().unwrap(),
                source.modified().unwrap(),
                "{name}"
            );
        }
        drop(lock);
    }

    // SPEC u298 Tests, `discard_under_one_home_is_not_undone`.
    #[test]
    fn discard_under_one_home_is_not_undone() {
        let h = homes();
        let write = WorkingCopy::open(&h.writable, "alice", "proj", &h.root).unwrap();
        let other = WorkingCopy::open(&h.refused, "alice", "proj", &h.root).unwrap();
        let resolution = Resolution {
            recovery_id: "r".into(),
            base_commit: Some("h1".into()),
            head_commit: "h0".into(),
            round: 1,
            local_paths: Vec::new(),
            remote_paths: Vec::new(),
            collisions: Vec::new(),
            combined_paths: Vec::new(),
            reviewed_tree: None,
            pending_writes: None,
        };
        other.write_resolution(&resolution).unwrap();
        write.write_resolution(&resolution).unwrap();
        set_stamp(&write.state_dir, 100);
        set_stamp(&other.state_dir, 200);
        {
            let _lock = write.lock().unwrap();
            assert_eq!(write.resolution().unwrap(), Some(resolution));
            write.remove_resolution().unwrap();
        }
        {
            let _lock = write.lock().unwrap();
            assert_eq!(write.resolution().unwrap(), None);
        }
        // Nor by a run on the other side of the refused default root.
        let _lock = other.lock().unwrap();
        assert_eq!(other.resolution().unwrap(), None);
        assert!(!other.state_dir.join(RESOLUTION_FILE).exists());
    }

    // ---- issue 173, D-163: a working copy ends with its folder ------------

    /// A copy at `h.root` over the writable roots holding a base, a
    /// resolution and an outbox.
    fn copy_with_state(h: &Homes) -> WorkingCopy {
        let copy = WorkingCopy::open(&h.writable, "alice", "proj", &h.root).unwrap();
        copy.record_base("h0", files(&[("a.md", "a0")])).unwrap();
        copy.write_resolution(&Resolution {
            recovery_id: "rec".into(),
            base_commit: Some("h0".into()),
            head_commit: "h1".into(),
            round: 1,
            local_paths: vec![],
            remote_paths: vec![],
            collisions: vec![],
            combined_paths: vec![],
            reviewed_tree: None,
            pending_writes: None,
        })
        .unwrap();
        copy.write_outbox(&Outbox {
            parent_commit: Some("h0".into()),
            tree: BTreeMap::new(),
        })
        .unwrap();
        copy
    }

    fn re_create(dir: &Path) {
        // A sibling holds the removed folder's inode while the new one is
        // made, so the two differ even where birth times are not kept.
        let held = dir.with_extension("held");
        std::fs::rename(dir, &held).unwrap();
        std::fs::create_dir(dir).unwrap();
        std::fs::remove_dir_all(&held).unwrap();
    }

    #[test]
    fn a_re_created_folder_opens_no_state_of_the_removed_one() {
        let h = homes();
        copy_with_state(&h);
        re_create(&h.root);

        assert!(
            WorkingCopy::open_existing(&h.writable, "alice", "proj", &h.root)
                .unwrap()
                .is_none()
        );
        let copy = WorkingCopy::open(&h.writable, "alice", "proj", &h.root).unwrap();
        assert!(copy.base().is_none());
        assert!(copy.resolution().unwrap().is_none());
        assert!(copy.outbox().unwrap().is_none());
        assert_eq!(read_files(&copy.state_dir), Vec::new());
        copy.record_base("h1", files(&[("b.md", "b1")])).unwrap();
        let again = WorkingCopy::open(&h.writable, "alice", "proj", &h.root).unwrap();
        assert_eq!(commit_of(&again).as_deref(), Some("h1"));
    }

    #[test]
    fn the_same_folder_reopened_or_moved_back_keeps_its_state() {
        let h = homes();
        copy_with_state(&h);
        let reopened = WorkingCopy::open(&h.writable, "alice", "proj", &h.root).unwrap();
        assert_eq!(commit_of(&reopened).as_deref(), Some("h0"));
        assert!(reopened.resolution().unwrap().is_some());

        let away = h.root.with_extension("away");
        std::fs::rename(&h.root, &away).unwrap();
        std::fs::rename(&away, &h.root).unwrap();
        let back = WorkingCopy::open(&h.writable, "alice", "proj", &h.root).unwrap();
        assert_eq!(commit_of(&back).as_deref(), Some("h0"));
        assert!(back.outbox().unwrap().is_some());
    }

    #[test]
    fn state_recorded_before_the_folder_identity_was_kept_is_adopted() {
        let h = homes();
        let copy = copy_with_state(&h);
        std::fs::remove_file(copy.state_dir.join(FOLDER_FILE)).unwrap();
        let reopened = WorkingCopy::open(&h.writable, "alice", "proj", &h.root).unwrap();
        assert_eq!(commit_of(&reopened).as_deref(), Some("h0"));
        assert!(reopened.state_dir.join(FOLDER_FILE).is_file());
    }

    #[test]
    fn a_refused_default_root_reads_a_removed_folders_state_as_absent() {
        let h = homes();
        copy_with_state(&h);
        re_create(&h.root);
        let copy = WorkingCopy::open(&h.refused, "alice", "proj", &h.root).unwrap();
        assert!(copy.base().is_none());
        assert!(copy.resolution().unwrap().is_none());
        let _lock = copy.lock().unwrap();
        assert!(
            copy.base().is_none(),
            "the lock adopts nothing from the stale home"
        );
    }

    /// Rewrite the identity recorded in `home` through `change`.
    fn edit_recorded(home: &Path, change: impl FnOnce(&mut FolderIdentity)) {
        let mut recorded = FolderIdentity::recorded(home).expect("a recorded identity");
        change(&mut recorded);
        std::fs::write(
            home.join(FOLDER_FILE),
            serde_json::to_vec(&recorded).unwrap(),
        )
        .unwrap();
    }

    #[test]
    fn a_birth_time_moved_earlier_or_another_device_keeps_the_state() {
        let h = homes();
        let copy = copy_with_state(&h);
        if FolderIdentity::of(&h.root).unwrap().born.is_none() {
            return;
        }
        // The folder's birth time now stands earlier than the recorded one
        // (`touch -t` or `rsync -a` on APFS), and the device moved.
        edit_recorded(&copy.state_dir, |id| {
            let (secs, nanos) = id.born.unwrap();
            id.born = Some((secs + 60, nanos));
            id.dev = id.dev.wrapping_add(1);
        });
        let reopened = WorkingCopy::open(&h.writable, "alice", "proj", &h.root).unwrap();
        assert_eq!(commit_of(&reopened).as_deref(), Some("h0"));
        assert!(reopened.resolution().unwrap().is_some());
        assert!(reopened.outbox().unwrap().is_some());
        assert_eq!(
            FolderIdentity::recorded(&reopened.state_dir),
            FolderIdentity::of(&h.root)
        );
    }

    #[test]
    fn a_reused_inode_with_a_later_birth_time_reads_as_another_folder() {
        let h = homes();
        let copy = copy_with_state(&h);
        if FolderIdentity::of(&h.root).unwrap().born.is_none() {
            return;
        }
        // A removed folder's inode taken again by one created after it.
        edit_recorded(&copy.state_dir, |id| {
            let (secs, nanos) = id.born.unwrap();
            id.born = Some((secs - 60, nanos));
        });
        assert!(
            WorkingCopy::open_existing(&h.writable, "alice", "proj", &h.root)
                .unwrap()
                .is_none()
        );
        let reopened = WorkingCopy::open(&h.writable, "alice", "proj", &h.root).unwrap();
        assert!(reopened.base().is_none());
        assert!(reopened.resolution().unwrap().is_none());
    }

    /// A folder created at a removed one's path with an earlier birth time,
    /// as NTFS tunnelling hands it the removed folder's own (D-164), still
    /// reads as another folder: its file index differs.
    #[test]
    fn another_inode_with_an_earlier_birth_time_reads_as_another_folder() {
        let h = homes();
        let copy = copy_with_state(&h);
        edit_recorded(&copy.state_dir, |id| {
            id.ino = id.ino.wrapping_add(1);
            id.born = id.born.map(|(secs, nanos)| (secs + 60, nanos));
        });
        let reopened = WorkingCopy::open(&h.writable, "alice", "proj", &h.root).unwrap();
        assert!(reopened.base().is_none());
        assert!(reopened.resolution().unwrap().is_none());
        assert!(reopened.outbox().unwrap().is_none());
    }

    #[cfg(unix)]
    #[test]
    fn touching_the_folder_back_in_time_keeps_the_state() {
        let h = homes();
        copy_with_state(&h);
        let touched = std::process::Command::new("touch")
            .args(["-t", "200001010000"])
            .arg(&h.root)
            .status()
            .unwrap();
        assert!(touched.success());
        let reopened = WorkingCopy::open(&h.writable, "alice", "proj", &h.root).unwrap();
        assert_eq!(commit_of(&reopened).as_deref(), Some("h0"));
        assert!(reopened.resolution().unwrap().is_some());
    }

    // ---- a working copy follows its folder (cli-checkouts-on-this-machine) ----

    /// The default home of `alice/proj` at the folder standing at `dir`.
    fn home_at(h: &Homes, dir: &Path) -> PathBuf {
        WorkingCopy::state_dir_for(
            &h.default,
            "alice",
            "proj",
            &std::fs::canonicalize(dir).unwrap(),
        )
    }

    /// The repository's folder holding every home of `alice/proj`.
    fn repo_dir(h: &Homes) -> PathBuf {
        h.default.join("working-copies").join("alice").join("proj")
    }

    /// The names under the repository's folder, sorted.
    fn entries_of(dir: &Path) -> Vec<String> {
        let mut names: Vec<String> = std::fs::read_dir(dir)
            .map(|entries| {
                entries
                    .flatten()
                    .map(|e| e.file_name().to_string_lossy().into_owned())
                    .collect()
            })
            .unwrap_or_default();
        names.sort();
        names
    }

    /// The set-asides standing under the repository's folder.
    fn set_asides(h: &Homes) -> Vec<String> {
        entries_of(&repo_dir(h))
            .into_iter()
            .filter(|name| name.ends_with(SET_ASIDE))
            .collect()
    }

    /// Whether the file system answers this folder's birth time: where it
    /// does not, a moved folder is not recognised, as D-163's rows skip.
    fn births_known(dir: &Path) -> bool {
        FolderIdentity::of(dir).is_some_and(|id| id.recognisable())
    }

    /// The folder at `root` moved to a sibling named `to`.
    fn move_to(root: &Path, to: &str) -> PathBuf {
        let moved = root.with_file_name(to);
        std::fs::rename(root, &moved).unwrap();
        moved
    }

    #[test]
    fn the_record_names_the_path_and_whether_the_copy_is_a_checkout() {
        let h = homes();
        let copy = WorkingCopy::open(&h.writable, "alice", "proj", &h.root).unwrap();
        let record = FolderRecord::read(&copy.state_dir).unwrap();
        assert_eq!(record.path.as_deref(), h.root.to_str());
        assert_eq!(record.checkout, Some(true));
        assert_eq!(Some(record.identity), FolderIdentity::of(&h.root));

        let docs = h.root.join("docs");
        std::fs::create_dir(&docs).unwrap();
        let scoped = WorkingCopy::open_scoped(&h.writable, "alice", "proj", &docs).unwrap();
        let record = FolderRecord::read(&scoped.state_dir).unwrap();
        assert_eq!(record.path.as_deref(), docs.to_str());
        assert_eq!(record.checkout, Some(false));
    }

    // A record an older build wrote — identity alone — gains the path and
    // the checkout at the next open, its state kept; an equal record is
    // not rewritten.
    #[test]
    fn a_record_without_a_path_gains_it_at_the_next_open_and_keeps_its_state() {
        let h = homes();
        let copy = copy_with_state(&h);
        let identity = FolderIdentity::of(&h.root).unwrap();
        std::fs::write(
            copy.state_dir.join(FOLDER_FILE),
            serde_json::to_vec(&identity).unwrap(),
        )
        .unwrap();
        let reopened = WorkingCopy::open(&h.writable, "alice", "proj", &h.root).unwrap();
        assert_eq!(commit_of(&reopened).as_deref(), Some("h0"));
        assert!(reopened.resolution().unwrap().is_some());
        let record = FolderRecord::read(&reopened.state_dir).unwrap();
        assert_eq!(record.path.as_deref(), h.root.to_str());
        assert_eq!(record.checkout, Some(true));

        let before = std::fs::metadata(reopened.state_dir.join(FOLDER_FILE))
            .unwrap()
            .modified()
            .unwrap();
        std::thread::sleep(std::time::Duration::from_millis(20));
        WorkingCopy::open(&h.writable, "alice", "proj", &h.root).unwrap();
        let after = std::fs::metadata(reopened.state_dir.join(FOLDER_FILE))
            .unwrap()
            .modified()
            .unwrap();
        assert_eq!(before, after);
    }

    // Row M2: a folder moved within its file system keeps its base, its
    // resolution and its outbox at the new path, under the new path's
    // hash; the home it left holds its lock file alone.
    #[test]
    fn a_moved_folder_keeps_its_state_at_its_new_path() {
        let h = homes();
        let copy = copy_with_state(&h);
        if !births_known(&h.root) {
            return;
        }
        let old_home = copy.state_dir.clone();
        let moved = move_to(&h.root, "moved");

        let reopened = WorkingCopy::open(&h.writable, "alice", "proj", &moved).unwrap();
        assert_eq!(reopened.state_dir, home_at(&h, &moved));
        assert_eq!(commit_of(&reopened).as_deref(), Some("h0"));
        assert!(reopened.resolution().unwrap().is_some());
        assert!(reopened.outbox().unwrap().is_some());
        assert!(reopened.state_dir.join(STATE_STAMP).is_file());
        let record = FolderRecord::read(&reopened.state_dir).unwrap();
        assert_eq!(record.path.as_deref(), moved.to_str());
        assert_eq!(entries_of(&old_home), vec![LOCK_FILE.to_string()]);
        assert!(set_asides(&h).is_empty());
        assert_eq!(
            std::fs::read_to_string(repo_dir(&h).join(FOLDERS_LOCK)).unwrap(),
            "\n",
            "no carry is listed once it is done"
        );
    }

    // Row M9: the state follows the folder there and back, each time
    // under the hash of where it stands.
    #[test]
    fn the_state_follows_a_folder_moved_there_and_back() {
        let h = homes();
        let copy = copy_with_state(&h);
        if !births_known(&h.root) {
            return;
        }
        let first = copy.state_dir.clone();
        let moved = move_to(&h.root, "moved");
        let there = WorkingCopy::open(&h.writable, "alice", "proj", &moved).unwrap();
        assert_eq!(commit_of(&there).as_deref(), Some("h0"));
        std::fs::rename(&moved, &h.root).unwrap();
        let back = WorkingCopy::open(&h.writable, "alice", "proj", &h.root).unwrap();
        assert_eq!(back.state_dir, first);
        assert_eq!(commit_of(&back).as_deref(), Some("h0"));
        assert!(back.resolution().unwrap().is_some());
        assert_eq!(entries_of(&there.state_dir), vec![LOCK_FILE.to_string()]);
    }

    // Row M6: a copy of a folder, the original standing, is another
    // folder: it starts with no base and the original keeps its own.
    #[test]
    fn a_copied_folder_starts_with_no_base_and_leaves_the_original_its_own() {
        let h = homes();
        copy_with_state(&h);
        let copied = h.root.with_file_name("copied");
        std::fs::create_dir(&copied).unwrap();
        let copy = WorkingCopy::open(&h.writable, "alice", "proj", &copied).unwrap();
        assert!(copy.base().is_none());
        let original = WorkingCopy::open(&h.writable, "alice", "proj", &h.root).unwrap();
        assert_eq!(commit_of(&original).as_deref(), Some("h0"));
        assert!(original.resolution().unwrap().is_some());
    }

    // Row M10: state recorded before the path was kept is not carried.
    #[test]
    fn a_folder_moved_before_its_path_was_recorded_is_not_carried() {
        let h = homes();
        let copy = copy_with_state(&h);
        let identity = FolderIdentity::of(&h.root).unwrap();
        std::fs::write(
            copy.state_dir.join(FOLDER_FILE),
            serde_json::to_vec(&identity).unwrap(),
        )
        .unwrap();
        let moved = move_to(&h.root, "moved");
        let reopened = WorkingCopy::open(&h.writable, "alice", "proj", &moved).unwrap();
        assert!(reopened.base().is_none());
        assert!(copy.state_dir.join(BASE_FILE).is_file());
    }

    // Row M8: a folder removed and made again at its path starts with no
    // base each time (D-163); the removed folder's state is set aside, and
    // one set-aside stands per path.
    #[test]
    fn a_folder_made_again_sets_the_state_aside_once_per_path() {
        let h = homes();
        copy_with_state(&h);
        re_create(&h.root);
        let again = WorkingCopy::open(&h.writable, "alice", "proj", &h.root).unwrap();
        assert!(again.base().is_none());
        assert!(again.resolution().unwrap().is_none());
        let gone = repo_dir(&h).join(format!(
            "{}{SET_ASIDE}",
            again.state_dir.file_name().unwrap().to_string_lossy()
        ));
        assert!(gone.join(BASE_FILE).is_file());
        again.record_base("h1", files(&[("b.md", "b1")])).unwrap();

        re_create(&h.root);
        let third = WorkingCopy::open(&h.writable, "alice", "proj", &h.root).unwrap();
        assert!(third.base().is_none());
        assert_eq!(set_asides(&h).len(), 1, "{:?}", set_asides(&h));
        let aside: Manifest =
            serde_json::from_slice(&std::fs::read(gone.join(BASE_FILE)).unwrap()).unwrap();
        assert_eq!(
            aside.commit_sha(),
            Some("h1"),
            "the newer replaced the older"
        );
    }

    // Row M5: moved, then a fresh copy at the old path, then the moved
    // folder opened: the fresh copy keeps a state of its own, the moved one
    // takes its own from the set-aside, and no set-aside is left.
    #[test]
    fn a_fresh_copy_at_the_old_path_and_the_moved_folder_each_keep_their_own() {
        let h = homes();
        copy_with_state(&h);
        if !births_known(&h.root) {
            return;
        }
        let moved = move_to(&h.root, "moved");
        std::fs::create_dir(&h.root).unwrap();
        let fresh = WorkingCopy::open(&h.writable, "alice", "proj", &h.root).unwrap();
        assert!(fresh.base().is_none());
        fresh.record_base("h9", files(&[("n.md", "n9")])).unwrap();
        assert_eq!(set_asides(&h).len(), 1);

        let found = WorkingCopy::open(&h.writable, "alice", "proj", &moved).unwrap();
        assert_eq!(commit_of(&found).as_deref(), Some("h0"));
        assert!(found.resolution().unwrap().is_some());
        assert!(found.outbox().unwrap().is_some());
        assert!(set_asides(&h).is_empty());
        let again = WorkingCopy::open(&h.writable, "alice", "proj", &h.root).unwrap();
        assert_eq!(commit_of(&again).as_deref(), Some("h9"));
        assert!(again.resolution().unwrap().is_none());
    }

    // A folder moved away, a fresh copy made at its path and removed, then
    // the folder moved back: its own set-aside is taken back, the removed
    // copy's state set aside in its place.
    #[test]
    fn a_folder_moved_back_over_a_removed_fresh_copy_takes_its_own_state_back() {
        let h = homes();
        copy_with_state(&h);
        if !births_known(&h.root) {
            return;
        }
        let moved = move_to(&h.root, "moved");
        std::fs::create_dir(&h.root).unwrap();
        let fresh = WorkingCopy::open(&h.writable, "alice", "proj", &h.root).unwrap();
        fresh.record_base("h9", files(&[("n.md", "n9")])).unwrap();
        std::fs::remove_dir_all(&h.root).unwrap();
        std::fs::rename(&moved, &h.root).unwrap();

        let back = WorkingCopy::open(&h.writable, "alice", "proj", &h.root).unwrap();
        assert_eq!(commit_of(&back).as_deref(), Some("h0"));
        assert!(back.resolution().unwrap().is_some());
        let asides = set_asides(&h);
        assert_eq!(asides.len(), 1, "{asides:?}");
        let aside: Manifest = serde_json::from_slice(
            &std::fs::read(repo_dir(&h).join(&asides[0]).join(BASE_FILE)).unwrap(),
        )
        .unwrap();
        assert_eq!(aside.commit_sha(), Some("h9"));
    }

    // Row M12: two opens at once in a moved folder: one carries, the
    // other waits and finds the record matching; the state stands once.
    #[test]
    fn two_opens_at_once_in_a_moved_folder_carry_once() {
        let h = homes();
        copy_with_state(&h);
        if !births_known(&h.root) {
            return;
        }
        let moved = move_to(&h.root, "moved");
        let opens: Vec<_> = (0..2)
            .map(|_| {
                let stores = h.writable.clone();
                let moved = moved.clone();
                std::thread::spawn(move || {
                    let copy = WorkingCopy::open(&stores, "alice", "proj", &moved).unwrap();
                    commit_of(&copy)
                })
            })
            .collect();
        for open in opens {
            assert_eq!(open.join().unwrap().as_deref(), Some("h0"));
        }
        let homes_holding: Vec<String> = entries_of(&repo_dir(&h))
            .into_iter()
            .filter(|name| repo_dir(&h).join(name).join(BASE_FILE).is_file())
            .collect();
        assert_eq!(homes_holding.len(), 1, "{homes_holding:?}");
    }

    // Row M13: a fresh copy opened at the old path at once with the moved
    // folder, repeated: every time the moved folder has its whole state
    // and the fresh one none of it.
    #[test]
    fn a_fresh_copy_at_the_old_path_at_once_with_the_moved_folder_never_mixes() {
        for round in 0..8 {
            let h = homes();
            copy_with_state(&h);
            if !births_known(&h.root) {
                return;
            }
            let moved = move_to(&h.root, "moved");
            std::fs::create_dir(&h.root).unwrap();
            let open = |dir: PathBuf| {
                let stores = h.writable.clone();
                std::thread::spawn(move || {
                    let copy = WorkingCopy::open(&stores, "alice", "proj", &dir).unwrap();
                    (
                        commit_of(&copy),
                        copy.resolution().unwrap().is_some(),
                        copy.outbox().unwrap().is_some(),
                    )
                })
            };
            let (at_old, at_moved) = if round % 2 == 0 {
                let a = open(h.root.clone());
                let b = open(moved.clone());
                (a, b)
            } else {
                let b = open(moved.clone());
                let a = open(h.root.clone());
                (a, b)
            };
            assert_eq!(
                at_old.join().unwrap(),
                (None, false, false),
                "round {round}"
            );
            assert_eq!(
                at_moved.join().unwrap(),
                (Some("h0".to_string()), true, true),
                "round {round}"
            );
            assert!(set_asides(&h).is_empty(), "round {round}");
        }
    }

    // Row M14: two checkouts of one repository swapped by hand, each then
    // opened at once: each ends with its own state, and neither hangs.
    #[test]
    fn two_checkouts_swapped_by_hand_each_keep_their_own_state() {
        let h = homes();
        let a = h.root.with_file_name("a");
        let b = h.root.with_file_name("b");
        std::fs::create_dir(&a).unwrap();
        std::fs::create_dir(&b).unwrap();
        if !births_known(&a) {
            return;
        }
        let copy_a = WorkingCopy::open(&h.writable, "alice", "proj", &a).unwrap();
        copy_a.record_base("ha", files(&[("a.md", "a")])).unwrap();
        let copy_b = WorkingCopy::open(&h.writable, "alice", "proj", &b).unwrap();
        copy_b.record_base("hb", files(&[("b.md", "b")])).unwrap();
        std::fs::write(a.join("mark"), "a").unwrap();
        std::fs::write(b.join("mark"), "b").unwrap();

        let t = h.root.with_file_name("t");
        std::fs::rename(&a, &t).unwrap();
        std::fs::rename(&b, &a).unwrap();
        std::fs::rename(&t, &b).unwrap();

        let opens: Vec<_> = [a.clone(), b.clone()]
            .into_iter()
            .map(|dir| {
                let stores = h.writable.clone();
                std::thread::spawn(move || {
                    let copy = WorkingCopy::open(&stores, "alice", "proj", &dir).unwrap();
                    (
                        std::fs::read_to_string(dir.join("mark")).unwrap(),
                        commit_of(&copy),
                    )
                })
            })
            .collect();
        for open in opens {
            let (mark, commit) = open.join().unwrap();
            assert_eq!(commit, Some(format!("h{mark}")));
        }
        assert!(set_asides(&h).is_empty(), "{:?}", set_asides(&h));
    }

    /// Lay a carry under way by hand, as a run killed after its first file
    /// moved leaves it: the list in `folders.lock`, the record written in the
    /// home reached, the base moved, the rest and the record still in the
    /// home left.
    fn killed_after_the_first_file(h: &Homes, from: &Path, to: &Path) {
        let leaves = home_at(h, from);
        std::fs::rename(from, to).unwrap();
        let reaches = home_at(h, to);
        std::fs::create_dir_all(&reaches).unwrap();
        let identity = FolderIdentity::of(to).unwrap();
        let member = Member {
            leaves: leaves.file_name().unwrap().to_string_lossy().into_owned(),
            reaches: reaches.file_name().unwrap().to_string_lossy().into_owned(),
            path: to.to_str().unwrap().to_string(),
            left: from.to_str().map(str::to_string),
            dev: identity.dev,
            ino: identity.ino,
            born: identity.born,
            checkout: Some(true),
            set_aside: false,
        };
        let mut folders = FoldersLock::take(&repo_dir(h)).unwrap();
        folders.write(&[member]).unwrap();
        drop(folders);
        // The record in the home reached first, as a carry writes it.
        FolderRecord {
            identity: identity.clone(),
            path: to.to_str().map(str::to_string),
            checkout: Some(true),
        }
        .write(&reaches)
        .unwrap();
        std::fs::rename(leaves.join(BASE_FILE), reaches.join(BASE_FILE)).unwrap();
    }

    // Row M15: a carry killed after its first file moved is finished by
    // the next open in the moved folder: the outcome of M2, the list
    // cleared.
    #[test]
    fn a_carry_killed_part_way_is_finished_by_the_next_open() {
        let h = homes();
        copy_with_state(&h);
        if !births_known(&h.root) {
            return;
        }
        let moved = h.root.with_file_name("moved");
        killed_after_the_first_file(&h, &h.root, &moved);

        let reopened = WorkingCopy::open(&h.writable, "alice", "proj", &moved).unwrap();
        assert_eq!(commit_of(&reopened).as_deref(), Some("h0"));
        assert!(reopened.resolution().unwrap().is_some());
        assert!(reopened.outbox().unwrap().is_some());
        assert_eq!(
            entries_of(&WorkingCopy::state_dir_for(
                &h.default, "alice", "proj", &h.root
            )),
            vec![LOCK_FILE.to_string()]
        );
        let mut folders = FoldersLock::take(&repo_dir(&h)).unwrap();
        assert!(folders.list().is_empty());
    }

    // Row M15, second half: a released build opening a folder made again
    // at the old path before the carry is finished discards what was left
    // there and records its own; finishing the carry then moves none of
    // that new folder's state.
    #[test]
    fn a_killed_carry_leaves_a_folder_made_since_at_the_old_path_its_own_state() {
        let h = homes();
        copy_with_state(&h);
        if !births_known(&h.root) {
            return;
        }
        let moved = h.root.with_file_name("moved");
        killed_after_the_first_file(&h, &h.root, &moved);
        // What the released build does at the old path: every file but the
        // lock removed, its folder's identity recorded, its own base.
        let old_home = WorkingCopy::state_dir_for(&h.default, "alice", "proj", &h.root);
        std::fs::create_dir(&h.root).unwrap();
        for entry in std::fs::read_dir(&old_home).unwrap().flatten() {
            if entry.file_name() != LOCK_FILE {
                let path = entry.path();
                if path.is_dir() {
                    std::fs::remove_dir_all(path).unwrap();
                } else {
                    std::fs::remove_file(path).unwrap();
                }
            }
        }
        let identity = FolderIdentity::of(&h.root).unwrap();
        std::fs::write(
            old_home.join(FOLDER_FILE),
            serde_json::to_vec(&identity).unwrap(),
        )
        .unwrap();
        let mut base = Manifest::default();
        base.update("h7".into(), files(&[("z.md", "z7")]));
        std::fs::write(old_home.join(BASE_FILE), serde_json::to_vec(&base).unwrap()).unwrap();

        let reopened = WorkingCopy::open(&h.writable, "alice", "proj", &moved).unwrap();
        assert_eq!(commit_of(&reopened).as_deref(), Some("h0"));
        assert!(
            reopened.resolution().unwrap().is_none(),
            "removed by the released build"
        );
        let at_old = WorkingCopy::open(&h.writable, "alice", "proj", &h.root).unwrap();
        assert_eq!(commit_of(&at_old).as_deref(), Some("h7"));
        let mut folders = FoldersLock::take(&repo_dir(&h)).unwrap();
        assert!(folders.list().is_empty());
    }

    // Row M15 for step 3: a set-aside killed after its first file moved —
    // a fresh copy pulled at the old path of a moved folder, the run
    // killed — is finished by the next open, so the moved folder still
    // takes its whole state back.
    #[test]
    fn a_set_aside_killed_part_way_is_finished_before_the_moved_folder_takes_it() {
        let h = homes();
        let copy = copy_with_state(&h);
        if !births_known(&h.root) {
            return;
        }
        let old_home = copy.state_dir.clone();
        let moved = move_to(&h.root, "moved");
        std::fs::create_dir(&h.root).unwrap();
        let own = old_home.file_name().unwrap().to_string_lossy().into_owned();
        let record = FolderRecord::read(&old_home).unwrap();
        let member = Member {
            leaves: own.clone(),
            reaches: format!("{own}{SET_ASIDE}"),
            path: record.path.clone().unwrap(),
            left: record.path.clone(),
            dev: record.identity.dev,
            ino: record.identity.ino,
            born: record.identity.born,
            checkout: record.checkout,
            set_aside: true,
        };
        let gone = repo_dir(&h).join(&member.reaches);
        let mut folders = FoldersLock::take(&repo_dir(&h)).unwrap();
        folders.write(&[member]).unwrap();
        drop(folders);
        std::fs::create_dir_all(&gone).unwrap();
        std::fs::rename(old_home.join(BASE_FILE), gone.join(BASE_FILE)).unwrap();

        let found = WorkingCopy::open(&h.writable, "alice", "proj", &moved).unwrap();
        assert_eq!(commit_of(&found).as_deref(), Some("h0"));
        assert!(found.resolution().unwrap().is_some());
        assert!(found.outbox().unwrap().is_some());
        assert!(set_asides(&h).is_empty(), "{:?}", set_asides(&h));
        let fresh = WorkingCopy::open(&h.writable, "alice", "proj", &h.root).unwrap();
        assert!(fresh.base().is_none());
        let mut folders = FoldersLock::take(&repo_dir(&h)).unwrap();
        assert!(folders.list().is_empty());
    }

    // A carry of this folder's own state that fails once its files have
    // begun to move fails the run and stays listed: the next run finishes
    // it whole, never working the copy with a part of its state.
    #[cfg(unix)]
    #[test]
    fn a_carry_failing_part_way_is_retried_whole() {
        use std::os::unix::fs::PermissionsExt;
        let h = homes();
        copy_with_state(&h);
        if !births_known(&h.root) {
            return;
        }
        let moved = move_to(&h.root, "moved");
        let reaches = home_at(&h, &moved);
        // Not a state file, so the home still holds none; a folder the
        // resolution cannot be renamed over.
        let block = reaches.join(RESOLUTION_FILE).join("sub");
        std::fs::create_dir_all(&block).unwrap();
        std::fs::write(block.join("x"), "x").unwrap();
        std::fs::set_permissions(&block, std::fs::Permissions::from_mode(0o500)).unwrap();
        let refused = std::fs::remove_file(block.join("x")).is_err();
        let first = WorkingCopy::open(&h.writable, "alice", "proj", &moved);
        if !refused {
            return; // run as an account no permission refuses
        }
        std::fs::set_permissions(&block, std::fs::Permissions::from_mode(0o700)).unwrap();
        assert!(first.is_err(), "{first:?}");
        assert!(
            FolderRecord::read(&reaches).is_some(),
            "no file of the carry stands in a home without a record"
        );
        let mut folders = FoldersLock::take(&repo_dir(&h)).unwrap();
        assert_eq!(folders.list().len(), 1, "the carry stays listed");
        drop(folders);

        let second = WorkingCopy::open(&h.writable, "alice", "proj", &moved).unwrap();
        assert_eq!(commit_of(&second).as_deref(), Some("h0"));
        assert!(second.resolution().unwrap().is_some());
        assert!(second.outbox().unwrap().is_some());
        let mut folders = FoldersLock::take(&repo_dir(&h)).unwrap();
        assert!(folders.list().is_empty());
    }

    // A run whose moved folder's state is held by an unfinished set-aside
    // fails rather than record the folder without it; once the cause is
    // gone the folder takes its whole state back.
    #[cfg(unix)]
    #[test]
    fn a_moved_folder_held_by_an_unfinished_set_aside_is_not_recorded_without_its_state() {
        use std::os::unix::fs::PermissionsExt;
        let h = homes();
        let copy = copy_with_state(&h);
        if !births_known(&h.root) {
            return;
        }
        let old_home = copy.state_dir.clone();
        let moved = move_to(&h.root, "moved");
        std::fs::create_dir(&h.root).unwrap();
        let own = old_home.file_name().unwrap().to_string_lossy().into_owned();
        let record = FolderRecord::read(&old_home).unwrap();
        let member = Member {
            leaves: own.clone(),
            reaches: format!("{own}{SET_ASIDE}"),
            path: record.path.clone().unwrap(),
            left: record.path.clone(),
            dev: record.identity.dev,
            ino: record.identity.ino,
            born: record.identity.born,
            checkout: record.checkout,
            set_aside: true,
        };
        let gone = repo_dir(&h).join(&member.reaches);
        let mut folders = FoldersLock::take(&repo_dir(&h)).unwrap();
        folders.write(&[member]).unwrap();
        drop(folders);
        let block = gone.join(RESOLUTION_FILE).join("sub");
        std::fs::create_dir_all(&block).unwrap();
        std::fs::write(block.join("x"), "x").unwrap();
        std::fs::set_permissions(&block, std::fs::Permissions::from_mode(0o500)).unwrap();
        let refused = std::fs::remove_file(block.join("x")).is_err();
        let first = WorkingCopy::open(&h.writable, "alice", "proj", &moved);
        if !refused {
            return; // run as an account no permission refuses
        }
        std::fs::set_permissions(&block, std::fs::Permissions::from_mode(0o700)).unwrap();
        assert!(first.is_err(), "{first:?}");
        assert!(FolderRecord::read(&home_at(&h, &moved)).is_none());

        let second = WorkingCopy::open(&h.writable, "alice", "proj", &moved).unwrap();
        assert_eq!(commit_of(&second).as_deref(), Some("h0"));
        assert!(second.resolution().unwrap().is_some());
        assert!(second.outbox().unwrap().is_some());
    }

    // A listed member, part-moved by a killed run, that meets an error when
    // it is finished stays listed: the folder is never worked with a part
    // of its state, and takes it whole once the cause is gone.
    #[cfg(unix)]
    #[test]
    fn a_listed_member_meeting_an_error_stays_listed() {
        use std::os::unix::fs::PermissionsExt;
        let h = homes();
        copy_with_state(&h);
        if !births_known(&h.root) {
            return;
        }
        let moved = h.root.with_file_name("moved");
        killed_after_the_first_file(&h, &h.root, &moved);
        let old_home = WorkingCopy::state_dir_for(&h.default, "alice", "proj", &h.root);
        std::fs::remove_file(old_home.join(LOCK_FILE)).unwrap();
        std::fs::set_permissions(&old_home, std::fs::Permissions::from_mode(0o500)).unwrap();
        let refused = std::fs::File::create(old_home.join(LOCK_FILE)).is_err();
        let first = WorkingCopy::open(&h.writable, "alice", "proj", &moved);
        std::fs::set_permissions(&old_home, std::fs::Permissions::from_mode(0o700)).unwrap();
        if !refused {
            return; // run as an account no permission refuses
        }
        assert!(first.is_err(), "{first:?}");
        let mut folders = FoldersLock::take(&repo_dir(&h)).unwrap();
        assert_eq!(folders.list().len(), 1, "the carry stays listed");
        drop(folders);

        let second = WorkingCopy::open(&h.writable, "alice", "proj", &moved).unwrap();
        assert_eq!(commit_of(&second).as_deref(), Some("h0"));
        assert!(second.resolution().unwrap().is_some());
        assert!(second.outbox().unwrap().is_some());
    }

    // A copy of the moved tree whose home cannot be opened is left where
    // it stands: the carry of the rest goes on, no list is left behind,
    // and later opens of the repository are not refused.
    #[cfg(unix)]
    #[test]
    fn a_copy_that_cannot_be_moved_is_struck_and_blocks_nothing() {
        use std::os::unix::fs::PermissionsExt;
        let h = homes();
        copy_with_state(&h);
        if !births_known(&h.root) {
            return;
        }
        let docs = h.root.join("docs");
        std::fs::create_dir(&docs).unwrap();
        let folder = WorkingCopy::open_scoped(&h.writable, "alice", "proj", &docs).unwrap();
        folder
            .record_base("hf", files(&[("docs/d.md", "d")]))
            .unwrap();
        let locked = folder.state_dir.clone();
        std::fs::remove_file(locked.join(LOCK_FILE)).unwrap();
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o500)).unwrap();
        let refused = std::fs::File::create(locked.join(LOCK_FILE)).is_err();

        let moved = move_to(&h.root, "moved");
        let at_root = WorkingCopy::open(&h.writable, "alice", "proj", &moved);
        let elsewhere = h.root.with_file_name("elsewhere");
        std::fs::create_dir(&elsewhere).unwrap();
        let fresh = WorkingCopy::open(&h.writable, "alice", "proj", &elsewhere);
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o700)).unwrap();
        if !refused {
            return; // run as an account no permission refuses
        }
        assert_eq!(commit_of(&at_root.unwrap()).as_deref(), Some("h0"));
        assert!(fresh.is_ok(), "{fresh:?}");
        let mut folders = FoldersLock::take(&repo_dir(&h)).unwrap();
        assert!(folders.list().is_empty());
        assert!(locked.join(BASE_FILE).is_file(), "left where it stands");
    }

    // Verify D1: a carry killed part-way, then the folder moved back to the
    // path it left (Finder's Undo Move) before any command ran at the new
    // one: the run at the old path does not trust its record while the
    // carry is listed, and moves every file back.
    #[test]
    fn a_killed_carry_then_a_move_back_keeps_the_whole_state() {
        let h = homes();
        copy_with_state(&h);
        if !births_known(&h.root) {
            return;
        }
        let moved = h.root.with_file_name("moved");
        killed_after_the_first_file(&h, &h.root, &moved);
        std::fs::rename(&moved, &h.root).unwrap();
        let back = WorkingCopy::open(&h.writable, "alice", "proj", &h.root).unwrap();
        assert_eq!(commit_of(&back).as_deref(), Some("h0"));
        assert!(back.resolution().unwrap().is_some());
        assert!(back.outbox().unwrap().is_some());
        let reached = WorkingCopy::state_dir_for(&h.default, "alice", "proj", &moved);
        assert_eq!(entries_of(&reached), vec![LOCK_FILE.to_string()]);
        let mut folders = FoldersLock::take(&repo_dir(&h)).unwrap();
        assert!(folders.list().is_empty());
    }

    // The same, killed later: every file moved and the record written in
    // the home reached, or the record left removed too; moved back, the
    // state and the record return whole to the old path.
    #[test]
    fn a_carry_killed_after_its_files_moved_is_undone_whole_on_a_move_back() {
        for remove_left_record in [false, true] {
            let h = homes();
            copy_with_state(&h);
            if !births_known(&h.root) {
                return;
            }
            let moved = h.root.with_file_name("moved");
            killed_after_the_first_file(&h, &h.root, &moved);
            let leaves = WorkingCopy::state_dir_for(&h.default, "alice", "proj", &h.root);
            let reaches = home_at(&h, &moved);
            move_state(&leaves, &reaches).unwrap();
            let mut record = FolderRecord::read(&leaves).unwrap();
            record.path = moved.to_str().map(str::to_string);
            record.write(&reaches).unwrap();
            if remove_left_record {
                std::fs::remove_file(leaves.join(FOLDER_FILE)).unwrap();
            }
            std::fs::rename(&moved, &h.root).unwrap();

            let back = WorkingCopy::open(&h.writable, "alice", "proj", &h.root).unwrap();
            assert_eq!(
                commit_of(&back).as_deref(),
                Some("h0"),
                "{remove_left_record}"
            );
            assert!(back.resolution().unwrap().is_some());
            assert!(back.outbox().unwrap().is_some());
            assert_eq!(
                FolderRecord::read(&leaves).unwrap().path.as_deref(),
                h.root.to_str()
            );
            assert!(FolderRecord::read(&reaches).is_none());
        }
    }

    // A carry killed part-way, the folder moved back, and an older build
    // then pulling a new folder at the path the carry was going to: the
    // home reached records that folder now, so neither home is touched.
    #[test]
    fn a_move_back_never_takes_another_folders_files() {
        let h = homes();
        copy_with_state(&h);
        if !births_known(&h.root) {
            return;
        }
        let moved = h.root.with_file_name("moved");
        killed_after_the_first_file(&h, &h.root, &moved);
        std::fs::rename(&moved, &h.root).unwrap();
        // What a released build does at the path left empty: the half
        // moved there discarded, its own folder recorded, its own base.
        std::fs::create_dir(&moved).unwrap();
        let reaches = home_at(&h, &moved);
        for entry in std::fs::read_dir(&reaches).unwrap().flatten() {
            if entry.file_name() != LOCK_FILE {
                std::fs::remove_file(entry.path()).unwrap();
            }
        }
        let z = FolderIdentity::of(&moved).unwrap();
        std::fs::write(reaches.join(FOLDER_FILE), serde_json::to_vec(&z).unwrap()).unwrap();
        let mut base = Manifest::default();
        base.update("hZ".into(), files(&[("z.md", "z")]));
        std::fs::write(reaches.join(BASE_FILE), serde_json::to_vec(&base).unwrap()).unwrap();

        let back = WorkingCopy::open(&h.writable, "alice", "proj", &h.root).unwrap();
        assert_ne!(commit_of(&back).as_deref(), Some("hZ"));
        assert!(back.resolution().unwrap().is_some());
        let at_z = WorkingCopy::open(&h.writable, "alice", "proj", &moved).unwrap();
        assert_eq!(commit_of(&at_z).as_deref(), Some("hZ"));
    }

    // A carry killed after the record in the home reached was written with
    // a birth time other than the listed one (NTFS tunnelling), then moved
    // back: that record is still the member's own, and the state returns.
    #[test]
    fn a_reached_record_whose_birth_time_moved_is_still_the_members() {
        let h = homes();
        copy_with_state(&h);
        if !births_known(&h.root) {
            return;
        }
        let moved = h.root.with_file_name("moved");
        killed_after_the_first_file(&h, &h.root, &moved);
        let leaves = WorkingCopy::state_dir_for(&h.default, "alice", "proj", &h.root);
        let reaches = home_at(&h, &moved);
        move_state(&leaves, &reaches).unwrap();
        let mut record = FolderRecord::read(&leaves).unwrap();
        record.path = moved.to_str().map(str::to_string);
        record.identity.born = record.identity.born.map(|(secs, nanos)| (secs + 60, nanos));
        record.write(&reaches).unwrap();
        std::fs::rename(&moved, &h.root).unwrap();

        let back = WorkingCopy::open(&h.writable, "alice", "proj", &h.root).unwrap();
        assert_eq!(commit_of(&back).as_deref(), Some("h0"));
        assert!(back.resolution().unwrap().is_some());
        assert!(FolderRecord::read(&reaches).is_none());
    }

    // A carry killed part-way, then the folder moved on to a third path:
    // the state is gathered back whole in the home it left, and the
    // command at the third path carries it there as any move.
    #[test]
    fn a_killed_carry_then_a_move_on_carries_the_whole_state_to_the_third_path() {
        let h = homes();
        copy_with_state(&h);
        if !births_known(&h.root) {
            return;
        }
        let moved = h.root.with_file_name("moved");
        killed_after_the_first_file(&h, &h.root, &moved);
        let third = move_to(&moved, "third");
        let found = WorkingCopy::open(&h.writable, "alice", "proj", &third).unwrap();
        assert_eq!(commit_of(&found).as_deref(), Some("h0"));
        assert!(found.resolution().unwrap().is_some());
        assert!(found.outbox().unwrap().is_some());
    }

    // A command in another checkout of the repository, whose record
    // matches, finishes a listed carry first: the moved folder then has
    // its whole state at once.
    #[test]
    fn a_command_in_another_checkout_finishes_a_listed_carry() {
        let h = homes();
        copy_with_state(&h);
        let other = h.root.with_file_name("other");
        std::fs::create_dir(&other).unwrap();
        WorkingCopy::open(&h.writable, "alice", "proj", &other).unwrap();
        if !births_known(&h.root) {
            return;
        }
        let moved = h.root.with_file_name("moved");
        killed_after_the_first_file(&h, &h.root, &moved);
        WorkingCopy::open(&h.writable, "alice", "proj", &other).unwrap();
        let mut folders = FoldersLock::take(&repo_dir(&h)).unwrap();
        assert!(folders.list().is_empty());
        drop(folders);
        let reached = home_at(&h, &moved);
        assert!(reached.join(RESOLUTION_FILE).is_file());
        assert!(reached.join(OUTBOX_FILE).is_file());
        assert_eq!(
            FolderRecord::read(&reached).unwrap().path.as_deref(),
            moved.to_str()
        );
    }

    // Verify D5: where the file index alone is the rule (an NTFS volume),
    // the volume serial is compared with it, so one index on two volumes
    // is two folders.
    #[test]
    fn the_file_index_alone_is_compared_within_one_volume() {
        let on = |dev: u64, unique_index: bool| FolderIdentity {
            dev,
            ino: 42,
            born: Some((1, 0)),
            unique_index,
        };
        let tunnelled = FolderIdentity {
            born: Some((9, 0)),
            ..on(7, true)
        };
        assert!(tunnelled.same_folder(&on(7, false)));
        assert!(!on(8, true).same_folder(&on(7, false)));
        assert!(
            on(8, false).same_folder(&on(7, false)),
            "unix: the device moves across mounts"
        );
    }

    // Rows M16 to M18 at the level of the homes: a checkout holding a
    // folder copy of its own, moved; the first open, inside the folder,
    // carries every copy of the moved tree, the root's record naming its
    // new path at once; moved again before the root is opened, every copy
    // is carried again.
    #[test]
    fn every_copy_of_a_moved_checkout_is_carried_by_the_first_open_in_it() {
        let h = homes();
        let root = copy_with_state(&h);
        if !births_known(&h.root) {
            return;
        }
        let docs = h.root.join("docs");
        std::fs::create_dir(&docs).unwrap();
        let folder = WorkingCopy::open_scoped(&h.writable, "alice", "proj", &docs).unwrap();
        folder
            .record_base("hf", files(&[("docs/d.md", "d")]))
            .unwrap();
        folder
            .write_resolution(&Resolution {
                recovery_id: "rec-docs".into(),
                ..root.resolution().unwrap().unwrap()
            })
            .unwrap();

        let moved = move_to(&h.root, "moved");
        let inside =
            WorkingCopy::open_scoped(&h.writable, "alice", "proj", &moved.join("docs")).unwrap();
        assert_eq!(commit_of(&inside).as_deref(), Some("hf"));
        assert_eq!(
            inside.resolution().unwrap().unwrap().recovery_id,
            "rec-docs"
        );
        let root_home = home_at(&h, &moved);
        let record = FolderRecord::read(&root_home).unwrap();
        assert_eq!(record.path.as_deref(), moved.to_str());
        assert_eq!(record.checkout, Some(true));
        assert!(root_home.join(BASE_FILE).is_file());

        let again = move_to(&moved, "again");
        let at_root = WorkingCopy::open(&h.writable, "alice", "proj", &again).unwrap();
        assert_eq!(commit_of(&at_root).as_deref(), Some("h0"));
        assert!(at_root.resolution().unwrap().is_some());
        let inside =
            WorkingCopy::open_scoped(&h.writable, "alice", "proj", &again.join("docs")).unwrap();
        assert_eq!(
            inside.resolution().unwrap().unwrap().recovery_id,
            "rec-docs"
        );
        for name in entries_of(&repo_dir(&h)) {
            let home = repo_dir(&h).join(&name);
            if home != at_root.state_dir && home != inside.state_dir && home.is_dir() {
                assert_eq!(entries_of(&home), vec![LOCK_FILE.to_string()], "{name}");
            }
        }
    }

    // Row M11: on a volume holding two spellings as one, a folder renamed
    // in letter case alone has its state carried to the new spelling.
    #[test]
    fn a_folder_renamed_in_letter_case_keeps_its_state() {
        let h = homes();
        copy_with_state(&h);
        if !births_known(&h.root) {
            return;
        }
        let upper = h.root.with_file_name("FOLDER");
        std::fs::rename(&h.root, &upper).unwrap();
        if !h.root.exists() {
            return; // a case-sensitive volume: another folder, as M2
        }
        let reopened = WorkingCopy::open(&h.writable, "alice", "proj", &upper).unwrap();
        assert_eq!(
            reopened.root.file_name().and_then(|n| n.to_str()),
            Some("FOLDER")
        );
        assert_eq!(commit_of(&reopened).as_deref(), Some("h0"));
        assert!(reopened.resolution().unwrap().is_some());
    }

    // A run whose default root refuses writes records and carries nothing.
    #[test]
    fn a_refused_default_root_carries_nothing() {
        let h = homes();
        let copy = copy_with_state(&h);
        let moved = move_to(&h.root, "moved");
        let sandboxed = WorkingCopy::open(&h.refused, "alice", "proj", &moved).unwrap();
        assert!(sandboxed.base().is_none());
        assert!(copy.state_dir.join(BASE_FILE).is_file());
        assert!(!home_at(&h, &moved).exists());
    }

    #[test]
    fn the_moved_folder_is_read_from_the_two_paths() {
        let p = |s: &str| PathBuf::from(s);
        assert_eq!(
            moved_between(&p("/a/p/docs"), &p("/a/q/docs")),
            (p("/a/p"), p("/a/q"))
        );
        assert_eq!(
            moved_between(&p("/a/proj"), &p("/b/proj")),
            (p("/a"), p("/b"))
        );
        assert_eq!(
            moved_between(&p("/a/p"), &p("/a/P")),
            (p("/a/p"), p("/a/P"))
        );
        assert_eq!(
            moved_between(&p("/a/p"), &p("/a/p")),
            (p("/a/p"), p("/a/p"))
        );
        assert_eq!(moved_between(&p("/p"), &p("/x/p")), (p("/"), p("/x")));
    }

    // The listing's read: every record with its path, checkout and stamp,
    // set-asides marked; a cache with no working copies holds none.
    #[test]
    fn recorded_folders_reads_every_record_and_writes_nothing() {
        let h = homes();
        assert_eq!(recorded_folders(&h.default, None).unwrap(), Vec::new());
        copy_with_state(&h);
        re_create(&h.root);
        let copy = WorkingCopy::open(&h.writable, "alice", "proj", &h.root).unwrap();
        let before = entries_of(&repo_dir(&h));
        let records = recorded_folders(&h.default, Some(("Alice", "PROJ"))).unwrap();
        assert_eq!(entries_of(&repo_dir(&h)), before);
        assert_eq!(records.len(), 2, "{records:?}");
        assert!(
            records
                .iter()
                .all(|r| r.owner == "alice" && r.name == "proj")
        );
        assert!(
            records
                .iter()
                .all(|r| r.path.as_deref() == Some(h.root.as_path()))
        );
        assert_eq!(records.iter().filter(|r| r.set_aside).count(), 1);
        assert!(records.iter().all(|r| r.checkout));
        let live = records.iter().find(|r| !r.set_aside).unwrap();
        assert_eq!(live.stamp, stamp_of(&copy.state_dir));
        assert!(
            recorded_folders(&h.default, Some(("alice", "other")))
                .unwrap()
                .is_empty()
        );
    }
}
