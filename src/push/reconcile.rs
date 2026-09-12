//! The three-way path classification a convergence reads a working copy
//! through, and the marked text merge that prepares a collision's
//! candidate (SPEC u256 § Contract Surface).

use std::collections::{BTreeMap, BTreeSet};

use diffy::{ConflictStyle, IncompleteHunkStyle, MergeOptions};
use serde::{Deserialize, Serialize};

/// The marker lines a prepared collision carries, in the order a
/// marked block writes them: local, base, separator, remote.
///
/// Only the first and the last literal decide whether a path still
/// holds a marker — the middle ones are ordinary markdown underlines.
pub const CONFLICT_MARKERS: [&str; 4] =
    ["<<<<<<< local", "||||||| base", "=======", ">>>>>>> remote"];

/// How both sides changed one path. The first word names the local
/// side's change and the second the remote side's.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CollisionKind {
    ModifyModify,
    AddAdd,
    ModifyDelete,
    DeleteModify,
}

/// Every path either side changed, each in exactly one list, each list
/// sorted by path.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Reconciliation {
    pub local_only: Vec<String>,
    pub remote_only: Vec<String>,
    pub identical: Vec<String>,
    pub collisions: Vec<(String, CollisionKind)>,
}

/// Classify every path of three path-to-hash maps. A path both sides
/// changed to one hash — a deletion on both included — is `identical`.
pub fn reconcile(
    base: &BTreeMap<String, String>,
    local: &BTreeMap<String, String>,
    remote: &BTreeMap<String, String>,
) -> Reconciliation {
    let paths: BTreeSet<&String> = base
        .keys()
        .chain(local.keys())
        .chain(remote.keys())
        .collect();
    let mut out = Reconciliation::default();

    for path in paths {
        let b = base.get(path);
        let l = local.get(path);
        let r = remote.get(path);
        match (l != b, r != b) {
            (false, false) => {}
            (true, false) => out.local_only.push(path.clone()),
            (false, true) => out.remote_only.push(path.clone()),
            (true, true) if l == r => out.identical.push(path.clone()),
            (true, true) => {
                let kind = match (b, l, r) {
                    (None, _, _) => CollisionKind::AddAdd,
                    (Some(_), None, _) => CollisionKind::DeleteModify,
                    (Some(_), _, None) => CollisionKind::ModifyDelete,
                    _ => CollisionKind::ModifyModify,
                };
                out.collisions.push((path.clone(), kind));
            }
        }
    }

    out
}

/// Merge `local` and `remote` over `base`, line by line.
///
/// Non-overlapping hunks come from both sides; each overlapping hunk is
/// written as a marked block in local, base, remote order, every marker
/// opening its own line even at an unterminated final line. The flag is
/// true where any block was written.
pub fn merge_text(base: &str, local: &str, remote: &str) -> (String, bool) {
    let mut options = MergeOptions::new();
    options
        .set_conflict_style(ConflictStyle::Diff3)
        .set_incomplete_hunk_style(IncompleteHunkStyle::Git);

    match options.merge(base, local, remote) {
        Ok(merged) => (merged, false),
        Err(marked) => {
            let renamed = marked.split_inclusive('\n').map(rename_label).collect();
            (renamed, true)
        }
    }
}

/// `diffy` labels its marker lines `ours`, `original` and `theirs`, and
/// offers no setter for them; each whole label line is renamed to its
/// `CONFLICT_MARKERS` literal.
fn rename_label(line: &str) -> String {
    let (body, newline) = match line.strip_suffix('\n') {
        Some(body) => (body, "\n"),
        None => (line, ""),
    };
    let literal = match body {
        "<<<<<<< ours" => CONFLICT_MARKERS[0],
        "||||||| original" => CONFLICT_MARKERS[1],
        ">>>>>>> theirs" => CONFLICT_MARKERS[3],
        _ => return line.to_string(),
    };
    format!("{literal}{newline}")
}

/// True where a line of `text` opens with the first or the last marker
/// literal.
pub fn holds_conflict_marker(text: &str) -> bool {
    text.lines()
        .any(|line| line.starts_with(CONFLICT_MARKERS[0]) || line.starts_with(CONFLICT_MARKERS[3]))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn map(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs
            .iter()
            .map(|(p, h)| (p.to_string(), h.to_string()))
            .collect()
    }

    fn names(paths: &[String]) -> Vec<&str> {
        paths.iter().map(String::as_str).collect()
    }

    #[test]
    fn reconcile_places_each_path_once() {
        let base = map(&[
            ("k", "1"),
            ("lo", "1"),
            ("ro", "1"),
            ("sm", "1"),
            ("mm", "1"),
            ("md", "1"),
            ("dm", "1"),
        ]);
        let local = map(&[
            ("k", "1"),
            ("lo", "2"),
            ("ro", "1"),
            ("sm", "3"),
            ("mm", "4"),
            ("md", "5"),
            ("aa", "7"),
            ("ln", "8"),
        ]);
        let remote = map(&[
            ("k", "1"),
            ("lo", "1"),
            ("ro", "2"),
            ("sm", "3"),
            ("mm", "6"),
            ("dm", "9"),
            ("aa", "10"),
            ("rn", "11"),
        ]);

        let r = reconcile(&base, &local, &remote);

        assert_eq!(names(&r.local_only), vec!["ln", "lo"]);
        assert_eq!(names(&r.remote_only), vec!["rn", "ro"]);
        assert_eq!(names(&r.identical), vec!["sm"]);
        assert_eq!(
            r.collisions,
            vec![
                ("aa".to_string(), CollisionKind::AddAdd),
                ("dm".to_string(), CollisionKind::DeleteModify),
                ("md".to_string(), CollisionKind::ModifyDelete),
                ("mm".to_string(), CollisionKind::ModifyModify),
            ]
        );
    }

    #[test]
    fn merge_text_marks_an_overlapping_hunk() {
        assert_eq!(
            merge_text("a\nb\nc\n", "a\nL\nc\n", "a\nR\nc\n"),
            (
                "a\n<<<<<<< local\nL\n||||||| base\nb\n=======\nR\n>>>>>>> remote\nc\n".to_string(),
                true
            )
        );
    }

    #[test]
    fn merge_text_takes_disjoint_hunks_cleanly() {
        assert_eq!(
            merge_text("a\nb\nc\n", "A\nb\nc\n", "a\nb\nC\n"),
            ("A\nb\nC\n".to_string(), false)
        );
    }

    #[test]
    fn merge_text_keeps_markers_on_their_own_lines_at_an_unterminated_end() {
        assert_eq!(
            merge_text("a\nb", "a\nL", "a\nR"),
            (
                "a\n<<<<<<< local\nL\n||||||| base\nb\n=======\nR\n>>>>>>> remote\n".to_string(),
                true
            )
        );
    }

    #[test]
    fn holds_conflict_marker_reads_only_the_opening_and_closing_literals() {
        assert!(holds_conflict_marker("x\n<<<<<<< local\ny\n"));
        assert!(holds_conflict_marker(">>>>>>> remote"));
        assert!(!holds_conflict_marker("Title\n=======\n||||||| base\n"));
    }
}
