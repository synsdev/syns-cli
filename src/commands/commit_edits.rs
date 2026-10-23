//! Roadmap 292: a `syns commit` changeset's `edits` — exact-string
//! edits applied to text the CLI reads at the parent, each edited path
//! then riding in the one push as a whole `files` entry (D-257).
//!
//! The document's own checks (`plan_edits`) stand before any request;
//! the read at the parent and the replacements (`apply_edits`) stand
//! after the write target is resolved and before the push is composed,
//! so a refused edit publishes nothing.

use std::collections::HashMap;
use std::sync::{Mutex, PoisonError};

use futures_util::stream::{self, StreamExt};

use crate::client::{SynsClient, hash_mismatch};
use crate::commands::commit::{ChangesetDocument, lexical_path};
use crate::commands::edit::replace_counted;
use crate::config::Config;
use crate::errors::{CliError, NotTextSurface};
use crate::read::batch::{Ask, Frame, IN_FLIGHT, Sink, read_chunks};
use crate::read::version_not_found_refusal;
use crate::repo::folder::{FolderScope, lies_under};
use crate::write::{FileContent, Member, MemberKind, WriteTarget, member_path, text_or_refuse};

/// One edit of the document past its own checks: its index in `edits`,
/// its path as typed, the path it reads at the parent and the path its
/// text lands at (D-257 §1, §2), both counted from the scoped folder
/// where the run stands in one, and the replacement it makes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlannedEdit {
    pub index: usize,
    pub typed: String,
    pub at: String,
    pub lands: String,
    pub old: String,
    pub new: String,
    pub replace_all: bool,
}

/// Every refusal of one edit (D-257 §7): its index in `edits`, counted
/// from 0, and its path as the document typed it.
pub fn edit_line(index: usize, typed: &str, detail: &str) -> String {
    format!("edit {index} ({typed}): {detail}")
}

/// The no-match refusal of one edit, `parent` as the caller spelt it.
pub fn edit_matched_nothing(index: usize, typed: &str, parent: &str) -> String {
    edit_line(
        index,
        typed,
        &format!("old matched no content at version {parent}"),
    )
}

/// The many-matches refusal of one edit.
pub fn edit_matched_many(index: usize, typed: &str, count: usize) -> String {
    edit_line(
        index,
        typed,
        &format!("old matched {count} times; pass replaceAll to replace every occurrence"),
    )
}

/// The not-found refusal of one edit, `parent` as the caller spelt it.
pub fn edit_path_not_found(index: usize, typed: &str, parent: &str) -> String {
    edit_line(index, typed, &format!("path not found at version {parent}"))
}

/// Whether one path is, lies under or lies above the other.
fn nests(path: &str, other: &str) -> bool {
    path == other || lies_under(path, other) || lies_under(other, path)
}

/// The overlap refusal of one edit: `path` (where it reads, or where it
/// lands) named against `other`, and the rule it breaks.
fn edit_overlap(index: usize, typed: &str, path: &str, other: &str, rule: &str) -> CliError {
    CliError::InvalidChangeset {
        message: edit_line(
            index,
            typed,
            &format!("{path} overlaps {other} in one changeset; {rule}"),
        ),
    }
}

/// The document's checks over its `edits`, in the order given, before
/// any request (D-257): for each edit an empty `old`, an `old` equal to
/// `new`, then its path resolved lexically as a member path is
/// (`member_path`), then each overlap with the document's members,
/// deletions and files — each `invalid_changeset`; a `new` holding a NUL
/// byte, which would leave the path holding no text, is
/// `content_not_text` (D-257 ruling 10). `members` are the document's
/// moves and copies as `changeset_members` answered them.
pub fn plan_edits(
    document: &ChangesetDocument,
    members: &[Member],
    folder: Option<&FolderScope>,
) -> Result<Vec<PlannedEdit>, CliError> {
    let files: Vec<String> = document
        .files
        .iter()
        .map(|f| lexical_path(&f.path))
        .collect();
    let deletions: Vec<String> = document
        .deletions
        .iter()
        .map(|d| lexical_path(&d.path))
        .collect();
    let mut planned = Vec::with_capacity(document.edits.len());
    for (index, edit) in document.edits.iter().enumerate() {
        let typed = edit.path.as_str();
        let shape = |detail: &str| CliError::InvalidChangeset {
            message: edit_line(index, typed, detail),
        };
        if edit.old.is_empty() {
            return Err(shape("old cannot be empty"));
        }
        if edit.old == edit.new {
            return Err(shape("old and new are the same value"));
        }
        if edit.new.contains('\0') {
            return Err(CliError::NotText {
                path: typed.to_string(),
                surface: NotTextSurface::ChangesetEdit(index),
            });
        }
        // D-257 §1: the path at the parent, resolved lexically and
        // counted from the folder.
        let at = member_path(folder, typed).map_err(|err| match err {
            CliError::Config { message } => shape(&message),
            other => other,
        })?;

        // §3: a copy's source; §2 and ruling 9: a move's source maps the
        // edit, and an edit above one is refused.
        let mut lands = at.clone();
        for member in members {
            match member.kind {
                MemberKind::Copy if nests(&at, &member.from) => {
                    return Err(edit_overlap(
                        index,
                        typed,
                        &at,
                        &member.from,
                        "an edit cannot be at, under or above the source of a copy; edit the source in a commit of its own",
                    ));
                }
                MemberKind::Move if at == member.from => lands = member.to.clone(),
                MemberKind::Move if lies_under(&at, &member.from) => {
                    lands = format!("{}{}", member.to, &at[member.from.len()..]);
                }
                MemberKind::Move if lies_under(&member.from, &at) => {
                    return Err(edit_overlap(
                        index,
                        typed,
                        &at,
                        &member.from,
                        "an edit cannot lie above the source of a move",
                    ));
                }
                _ => {}
            }
        }
        // §4: any member's destination, judged at the parent.
        for member in members {
            if nests(&at, &member.to) {
                return Err(edit_overlap(
                    index,
                    typed,
                    &at,
                    &member.to,
                    "an edit cannot be at, under or above the destination of a move or a copy",
                ));
            }
        }
        // §5: a deletion, judged where the edit reads. Where it lands, a
        // deletion at or above a move's destination is admitted (D-253
        // §1) and one under it is already refused among the members.
        for deletion in &deletions {
            if nests(&at, deletion) {
                return Err(edit_overlap(
                    index,
                    typed,
                    &at,
                    deletion,
                    "an edit cannot be at, under or above a deletion",
                ));
            }
        }
        // §6: a `files` path, at the parent or where it lands.
        for file in &files {
            for path in [&at, &lands] {
                if path == file {
                    return Err(shape(&format!(
                        "{path} is also named under files in one changeset; carry the edit or the whole file, not both"
                    )));
                }
            }
        }
        planned.push(PlannedEdit {
            index,
            typed: typed.to_string(),
            at,
            lands,
            old: edit.old.clone(),
            new: edit.new.clone(),
            replace_all: edit.replace_all,
        });
    }
    Ok(planned)
}

/// What the parent holds at one edited path.
#[derive(Debug)]
pub enum Fetched {
    Bytes(Vec<u8>),
    /// The file read answered `content: null`: bytes that are not text.
    NotText,
    Missing,
}

/// The batch's sink: each ask's frame kept in its slot, and none for a
/// path the batch leaves to be read alone (`too_large`, `unchanged`).
struct EditSink<'a> {
    paths: &'a [String],
    slots: Mutex<Vec<Option<Fetched>>>,
}

impl Sink for EditSink<'_> {
    fn frame(&self, index: usize, frame: Frame) -> Result<(), CliError> {
        let fetched = match frame {
            Frame::Ok { bytes, .. } => Fetched::Bytes(bytes),
            Frame::NotFound => Fetched::Missing,
            Frame::Unchanged { .. } | Frame::TooLarge { .. } => return Ok(()),
            Frame::Mismatch { expected, actual } => {
                return Err(hash_mismatch(&self.paths[index], &expected, &actual));
            }
        };
        self.slots.lock().unwrap_or_else(PoisonError::into_inner)[index] = Some(fetched);
        Ok(())
    }
}

/// One path read alone through the file read at `reference`, as `syns
/// edit` reads it: a `404` naming the repository or the parent refused
/// as `syns edit` refuses it, any other `404` the path missing.
async fn read_alone(
    client: &SynsClient,
    target: &WriteTarget,
    parent: &str,
    reference: &str,
    path: &str,
) -> Result<Fetched, CliError> {
    match client
        .get_file(&target.repo_id, Some(&target.token), path, Some(reference))
        .await
    {
        Ok((response, _raw)) => Ok(match response.content {
            Some(text) => Fetched::Bytes(text.into_bytes()),
            None => Fetched::NotText,
        }),
        Err(err) if err.names_missing_repository() => Err(err.repo_or(&target.repo_id)),
        Err(err) if err.names_missing_version() => {
            Err(err.with_version_not_found_context(version_not_found_refusal(parent)))
        }
        Err(CliError::Api {
            status: Some(404), ..
        }) => Ok(Fetched::Missing),
        Err(err) => Err(err),
    }
}

/// Reads every path at the parent: one batched read over `POST …/read`
/// (roadmap 279), each path it leaves read alone, and every path read
/// alone, at most `IN_FLIGHT` at once, where the server answers no v2.
/// Answers each path's result in the order given.
pub async fn read_at_parent(
    client: &SynsClient,
    target: &WriteTarget,
    parent: &str,
    paths: &[String],
) -> Result<Vec<Result<Fetched, CliError>>, CliError> {
    let reference = target.parent.read_ref();
    let asks: Vec<Ask> = paths
        .iter()
        .map(|path| Ask {
            path: path.clone(),
            have: None,
            expect: None,
            size: Some(0),
        })
        .collect();
    let sink = EditSink {
        paths,
        slots: Mutex::new((0..paths.len()).map(|_| None).collect()),
    };
    let slots = match read_chunks(
        client,
        None,
        &target.repo_id,
        Some(&target.token),
        &reference,
        &asks,
        IN_FLIGHT,
        &sink,
    )
    .await
    {
        Ok(_) => sink
            .slots
            .into_inner()
            .unwrap_or_else(PoisonError::into_inner),
        Err(err) if crate::read::v2::is_absent(&err) => (0..paths.len()).map(|_| None).collect(),
        Err(err) => return Err(err),
    };
    let reference = reference.as_str();
    Ok(stream::iter(paths.iter().zip(slots))
        .map(|(path, slot)| async move {
            match slot {
                Some(fetched) => Ok(fetched),
                None => read_alone(client, target, parent, reference, path).await,
            }
        })
        .buffered(IN_FLIGHT)
        .collect()
        .await)
}

/// Applies the edits in document order over what `fetched` answered for
/// each path at the parent, each against the result of the one before
/// (D-257 §1): the first refusal in index order is answered — the path
/// missing (`not_found`), its content not text (`content_not_text`), or
/// `old` matching nowhere (`no_match`) or more than once with no
/// `replaceAll` (`many_matches`). Answers each edited path's whole text
/// at the path it lands at, in the order the paths first appear, each
/// bounded by `LIM-file-size`.
pub fn applied(
    planned: &[PlannedEdit],
    mut fetched: HashMap<String, Result<Fetched, CliError>>,
    parent: &str,
) -> Result<Vec<(String, FileContent)>, CliError> {
    // Each path's text so far, and the first edit naming it.
    let mut texts: Vec<(String, usize, String)> = Vec::new();
    let mut place: HashMap<&str, usize> = HashMap::new();
    for edit in planned {
        let at = match place.get(edit.at.as_str()) {
            Some(at) => *at,
            None => {
                let not_text = || CliError::NotText {
                    path: edit.typed.clone(),
                    surface: NotTextSurface::ChangesetEdit(edit.index),
                };
                let text = match fetched.remove(&edit.at) {
                    Some(Ok(Fetched::Bytes(bytes))) => {
                        String::from_utf8(bytes).map_err(|_| not_text())?
                    }
                    Some(Ok(Fetched::NotText)) => return Err(not_text()),
                    Some(Ok(Fetched::Missing)) | None => {
                        return Err(CliError::PathNotFound {
                            line: edit_path_not_found(edit.index, &edit.typed, parent),
                        });
                    }
                    Some(Err(err)) => return Err(err),
                };
                if text.contains('\0') {
                    return Err(not_text());
                }
                place.insert(edit.at.as_str(), texts.len());
                texts.push((edit.lands.clone(), edit.index, text));
                texts.len() - 1
            }
        };
        let text = &mut texts[at].2;
        *text = replace_counted(text, &edit.old, &edit.new, edit.replace_all).map_err(|count| {
            match count {
                0 => CliError::NoMatch {
                    message: edit_matched_nothing(edit.index, &edit.typed, parent),
                },
                count => CliError::ManyMatches {
                    count,
                    message: edit_matched_many(edit.index, &edit.typed, count),
                },
            }
        })?;
    }
    texts
        .into_iter()
        .map(|(lands, index, text)| {
            let typed = &planned
                .iter()
                .find(|edit| edit.index == index)
                .expect("the first edit of the path")
                .typed;
            let content = text_or_refuse(
                typed,
                text.into_bytes(),
                NotTextSurface::ChangesetEdit(index),
            )?;
            Ok((lands, content))
        })
        .collect()
}

/// Reads every edited path at the parent and applies the edits (D-257),
/// before any push is composed. `parent` is `--parent` as the caller
/// spelt it.
pub async fn apply_edits(
    config: &Config,
    target: &WriteTarget,
    parent: &str,
    planned: &[PlannedEdit],
) -> Result<Vec<(String, FileContent)>, CliError> {
    let mut distinct: Vec<&str> = Vec::new();
    for edit in planned {
        if !distinct.contains(&edit.at.as_str()) {
            distinct.push(edit.at.as_str());
        }
    }
    let read_paths: Vec<String> = distinct
        .iter()
        .map(|at| target.repository_path(at))
        .collect::<Result<_, _>>()?;
    let client = SynsClient::new(config.server_url())?;
    let results = read_at_parent(&client, target, parent, &read_paths).await?;
    let fetched: HashMap<String, Result<Fetched, CliError>> = distinct
        .iter()
        .map(|at| at.to_string())
        .zip(results)
        .collect();
    applied(planned, fetched, parent)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::commit::{changeset_members, parse_changeset_document};

    const PARENT: &str = "42";

    fn plan(document: &str) -> Result<Vec<PlannedEdit>, CliError> {
        let document = parse_changeset_document(document.as_bytes()).unwrap();
        let members = changeset_members(&document, false, None)?;
        plan_edits(&document, &members, None)
    }

    fn refusal(document: &str) -> (String, String) {
        let err = plan(document).unwrap_err();
        (err.code(), err.to_string())
    }

    fn fetched(entries: &[(&str, Fetched)]) -> HashMap<String, Result<Fetched, CliError>> {
        entries
            .iter()
            .map(|(path, f)| {
                let f = match f {
                    Fetched::Bytes(b) => Fetched::Bytes(b.clone()),
                    Fetched::NotText => Fetched::NotText,
                    Fetched::Missing => Fetched::Missing,
                };
                (path.to_string(), Ok(f))
            })
            .collect()
    }

    // Row 3: an empty `old` and an `old` equal to `new`, naming the index
    // and the path as typed, `invalid_changeset`.
    #[test]
    fn an_empty_old_and_an_old_equal_to_new_name_the_index() {
        assert_eq!(
            refusal(
                r#"{"edits":[{"path":"a.md","old":"x","new":"y"},{"path":"./b.md","old":"","new":"y"}]}"#
            ),
            (
                "invalid_changeset".to_string(),
                "configuration error: edit 1 (./b.md): old cannot be empty".to_string()
            )
        );
        assert_eq!(
            refusal(r#"{"edits":[{"path":"a.md","old":"x","new":"x"}]}"#),
            (
                "invalid_changeset".to_string(),
                "configuration error: edit 0 (a.md): old and new are the same value".to_string()
            )
        );
        // A `new` holding NUL leaves no text (D-257 ruling 10).
        assert_eq!(
            refusal(r#"{"edits":[{"path":"a.md","old":"x","new":"a\u0000b"}]}"#),
            (
                "content_not_text".to_string(),
                "edit 0 (a.md): cannot write content that is not text \u{2014} an edit changes text alone; replace the file whole under files with contentBase64".to_string()
            )
        );
    }

    // Edge inputs: the lexical rules of a member path.
    #[test]
    fn edge_paths_resolve_or_refuse_lexically() {
        for (path, line) in [
            ("", "edit 0 (): path cannot be empty"),
            (
                ".",
                "edit 0 (.): . names the repository's root; name a file counted from it",
            ),
            (
                "..",
                "edit 0 (..): .. names no path inside the repository; name one counted from its root",
            ),
            (
                "../x",
                "edit 0 (../x): ../x names no path inside the repository; name one counted from its root",
            ),
        ] {
            let document = format!(r#"{{"edits":[{{"path":"{path}","old":"a","new":"b"}}]}}"#);
            let (code, got) = refusal(&document);
            assert_eq!(code, "invalid_changeset", "{path}");
            assert_eq!(got, format!("configuration error: {line}"), "{path}");
        }
        let planned = plan(
            r#"{"edits":[{"path":"docs/./a/../2024.md","old":"a","new":"b"},{"path":"2024.md/","old":"a","new":"b"}]}"#,
        )
        .unwrap();
        assert_eq!(planned[0].at, "docs/2024.md");
        assert_eq!(planned[0].lands, "docs/2024.md");
        assert_eq!(planned[1].at, "2024.md");
    }

    // Row 4, D-257 §2: an edit at or under a move's source lands at the
    // moved path; one elsewhere lands where it reads.
    #[test]
    fn an_edit_under_a_moved_source_lands_at_the_moved_path() {
        let planned = plan(
            r#"{"moves":[{"from":"q3-review","to":"q3-retro"},{"from":"a.md","to":"b.md"}],
                "edits":[{"path":"q3-review/document.html","old":"x","new":"y"},
                         {"path":"a.md","old":"x","new":"y"},
                         {"path":"q3-reviewed.md","old":"x","new":"y"}]}"#,
        )
        .unwrap();
        let lands: Vec<(&str, &str)> = planned
            .iter()
            .map(|e| (e.at.as_str(), e.lands.as_str()))
            .collect();
        assert_eq!(
            lands,
            vec![
                ("q3-review/document.html", "q3-retro/document.html"),
                ("a.md", "b.md"),
                ("q3-reviewed.md", "q3-reviewed.md"),
            ]
        );
    }

    // Row 5, D-257 §3–6 and ruling 9 (as patched R1): every overlap refused, naming the
    // index and the typed path.
    #[test]
    fn every_overlap_is_refused_naming_the_edit() {
        for (document, start) in [
            // §6: a `files` path, at the parent or where the edit lands.
            (
                r#"{"files":[{"path":"a.md","content":"x"}],"edits":[{"path":"./a.md","old":"x","new":"y"}]}"#,
                "edit 0 (./a.md): a.md is also named under files",
            ),
            (
                r#"{"moves":[{"from":"a","to":"b"}],"files":[{"path":"b/x.md","content":"x"}],"edits":[{"path":"a/x.md","old":"x","new":"y"}]}"#,
                "edit 0 (a/x.md): b/x.md is also named under files",
            ),
            // §5: a deletion at, under or above.
            (
                r#"{"deletions":[{"path":"a.md"}],"edits":[{"path":"a.md","old":"x","new":"y"}]}"#,
                "edit 0 (a.md): a.md overlaps a.md in one changeset; an edit cannot be at, under or above a deletion",
            ),
            (
                r#"{"deletions":[{"path":"docs"}],"edits":[{"path":"docs/a.md","old":"x","new":"y"}]}"#,
                "edit 0 (docs/a.md): docs/a.md overlaps docs",
            ),
            // §3: a copy's source, at, under or above.
            (
                r#"{"copies":[{"from":"t","to":"u"}],"edits":[{"path":"t/a.md","old":"x","new":"y"}]}"#,
                "edit 0 (t/a.md): t/a.md overlaps t in one changeset; an edit cannot be at, under or above the source of a copy",
            ),
            (
                r#"{"copies":[{"from":"t/a.md","to":"u.md"}],"edits":[{"path":"t/a.md","old":"x","new":"y"}]}"#,
                "edit 0 (t/a.md): t/a.md overlaps t/a.md",
            ),
            // §4: a member's destination.
            (
                r#"{"copies":[{"from":"t","to":"u"}],"edits":[{"path":"u/a.md","old":"x","new":"y"}]}"#,
                "edit 0 (u/a.md): u/a.md overlaps u in one changeset; an edit cannot be at, under or above the destination",
            ),
            (
                r#"{"moves":[{"from":"a.md","to":"b.md"}],"edits":[{"path":"b.md","old":"x","new":"y"}]}"#,
                "edit 0 (b.md): b.md overlaps b.md",
            ),
            // Ruling 9: above a move's source.
            (
                r#"{"moves":[{"from":"d/sub","to":"e"}],"edits":[{"path":"d","old":"x","new":"y"}]}"#,
                "edit 0 (d): d overlaps d/sub in one changeset; an edit cannot lie above the source of a move",
            ),
        ] {
            let (code, line) = refusal(document);
            assert_eq!(code, "invalid_changeset", "{document}");
            assert!(
                line.starts_with(&format!("configuration error: {start}")),
                "{document}: {line}"
            );
        }
        // A deletion or a file beside, not overlapping, is admitted.
        assert!(
            plan(r#"{"deletions":[{"path":"docs2"}],"files":[{"path":"a.mdx","content":"x"}],"edits":[{"path":"docs/a.md","old":"x","new":"y"}]}"#)
                .is_ok()
        );
        // Review R1 I1 (D-253 §1): a deletion at or above a move's
        // destination is judged against an edit only where the edit reads,
        // so an edit under the move's source lands at the moved path.
        let planned = plan(
            r#"{"moves":[{"from":"q","to":"docs"}],"deletions":[{"path":"docs"}],"edits":[{"path":"q/a.md","old":"x","new":"y"}]}"#,
        )
        .unwrap();
        assert_eq!(
            (planned[0].at.as_str(), planned[0].lands.as_str()),
            ("q/a.md", "docs/a.md")
        );
    }

    // Rows 1 and 2: edits apply in order, each to the result of the one
    // before; zero or several matches refuse, `replaceAll` takes all.
    #[test]
    fn edits_apply_in_order_and_refuse_on_the_count() {
        let planned = plan(
            r###"{"edits":[{"path":"README.md","old":"v0.3","new":"v0.4"},
                         {"path":"docs/a.md","old":"foo","new":"bar","replaceAll":true},
                         {"path":"README.md","old":"## Old\n","new":""},
                         {"path":"README.md","old":"v0.4","new":"v0.5"}]}"###,
        )
        .unwrap();
        let out = applied(
            &planned,
            fetched(&[
                (
                    "README.md",
                    Fetched::Bytes(b"# v0.3\n## Old\nend\n".to_vec()),
                ),
                ("docs/a.md", Fetched::Bytes(b"foo foo foo".to_vec())),
            ]),
            PARENT,
        )
        .unwrap();
        assert_eq!(
            out,
            vec![
                (
                    "README.md".to_string(),
                    FileContent::Text("# v0.5\nend\n".to_string())
                ),
                (
                    "docs/a.md".to_string(),
                    FileContent::Text("bar bar bar".to_string())
                ),
            ]
        );

        let twice = plan(
            r###"{"edits":[{"path":"README.md","old":"## New","new":"## Newer"},{"path":"README.md","old":"v0","new":"v1"}]}"###,
        )
        .unwrap();
        let err = applied(
            &twice,
            fetched(&[(
                "README.md",
                Fetched::Bytes(b"v0.1 v0.2 v0.3 ## New".to_vec()),
            )]),
            PARENT,
        )
        .unwrap_err();
        assert_eq!(err.code(), "many_matches");
        assert_eq!(
            err.to_string(),
            "configuration error: edit 1 (README.md): old matched 3 times; pass replaceAll to replace every occurrence"
        );
        assert_eq!(err.json_document()["count"], serde_json::json!(3));

        let nowhere = plan(r#"{"edits":[{"path":"docs/a.md","old":"foo","new":"baz"}]}"#).unwrap();
        let err = applied(
            &nowhere,
            fetched(&[("docs/a.md", Fetched::Bytes(b"bar".to_vec()))]),
            PARENT,
        )
        .unwrap_err();
        assert_eq!(err.code(), "no_match");
        assert_eq!(
            err.to_string(),
            "configuration error: edit 0 (docs/a.md): old matched no content at version 42"
        );
    }

    // Row 6: a path missing at the parent, and a content that is not
    // text (NUL, invalid UTF-8, `content: null`), naming the index.
    #[test]
    fn a_missing_path_and_a_content_not_text_name_the_index() {
        let planned = plan(
            r#"{"edits":[{"path":"a.md","old":"x","new":"y"},{"path":"b.bin","old":"x","new":"y"}]}"#,
        )
        .unwrap();
        let err = applied(
            &planned,
            fetched(&[("a.md", Fetched::Missing), ("b.bin", Fetched::NotText)]),
            PARENT,
        )
        .unwrap_err();
        assert_eq!(err.code(), "not_found");
        assert_eq!(
            err.to_string(),
            "edit 0 (a.md): path not found at version 42"
        );
        for parent_content in [
            Fetched::NotText,
            Fetched::Bytes(vec![0xff, 0xfe, b'x']),
            Fetched::Bytes(b"x\0y".to_vec()),
        ] {
            let err = applied(
                &planned,
                fetched(&[
                    ("a.md", Fetched::Bytes(b"x".to_vec())),
                    ("b.bin", parent_content),
                ]),
                PARENT,
            )
            .unwrap_err();
            assert_eq!(err.code(), "content_not_text");
            assert!(
                err.to_string()
                    .starts_with("edit 1 (b.bin): cannot write content that is not text"),
                "{err}"
            );
        }
    }

    // `syns edit` keeps its own lines over the shared matcher.
    #[test]
    fn the_shared_matcher_counts_as_edit_did() {
        assert_eq!(
            replace_counted("a\nb a", "a\nb", "c", false),
            Ok("c a".to_string())
        );
        assert_eq!(
            replace_counted("aaa", "aa", "b", false),
            Ok("ba".to_string())
        );
        assert_eq!(replace_counted("a a", "a", "b", false), Err(2));
        assert_eq!(replace_counted("a", "z", "b", true), Err(0));
    }
}
