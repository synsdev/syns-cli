//! `syns commit --parent REF` — one commit composed from a changeset
//! document read on standard input (SPEC u271, u283).
//!
//! The whole changeset lands as one commit or none of it lands at all:
//! every refusal below is raised before any request leaves. Roadmap 228:
//! the document is read, parsed and checked ahead of the write target's
//! requests, so a document refused for its own shape, its members' paths
//! or their overlaps (D-253 §1) leaves nothing on the wire.

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;

use crate::config::Config;
use crate::errors::{CliError, NotTextSurface};
use crate::output::Output;
use crate::read::{as_server_reads, resolve_lexically};
use crate::repo::folder::FolderScope;
use crate::write::{
    Changeset, FileContent, Member, MemberKind, WriteOptions, bind_write_target, classify_content,
    commit_changeset, default_message, finish_write_target, guard_write_target, member_path,
    read_standard_input, refuse_overlaps, refuse_terminal_standard_input, text_or_refuse,
};

/// The changeset document: `{ files: [{ path, content }` or
/// `{ path, contentBase64 }], deletions: [{ path }], moves: [{ from, to
/// }], copies: [{ from, to }] }` (roadmap 228 adds the last two).
///
/// Every member is optional and absent reads as empty, and a key
/// outside the eight refuses the document. Each file entry carries
/// exactly one of its two content members, which `classify_changeset`
/// checks rather than the parse, so the refusal names the path. A string escape naming an
/// unpaired surrogate is refused by the parse itself rather than by a
/// check of this unit's own — `serde_json`'s string reader never builds
/// such a `String` (`SPEC_REVIEW_R2.md` Library Candidates).
#[derive(serde::Deserialize, Debug, Default, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ChangesetDocument {
    #[serde(default)]
    pub files: Vec<ChangesetFile>,
    #[serde(default)]
    pub deletions: Vec<ChangesetDeletion>,
    /// Roadmap 228: each path the commit moves, a file or a whole folder.
    #[serde(default)]
    pub moves: Vec<ChangesetMember>,
    /// Roadmap 228: each path the commit copies, alike.
    #[serde(default)]
    pub copies: Vec<ChangesetMember>,
}

/// One `moves` or `copies` entry: the path left or read, and the new path
/// itself.
#[derive(serde::Deserialize, Debug, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ChangesetMember {
    pub from: String,
    pub to: String,
}

#[derive(serde::Deserialize, Debug, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ChangesetFile {
    pub path: String,
    #[serde(default)]
    pub content: Option<String>,
    /// The file's bytes in the `file bytes` form: standard padded base64
    /// (SPEC u283, `D-088`).
    #[serde(default, rename = "contentBase64")]
    pub content_base64: Option<String>,
}

#[derive(serde::Deserialize, Debug, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ChangesetDeletion {
    pub path: String,
}

/// The refusal a path standing in both members takes.
pub fn path_in_both_members(path: &str) -> String {
    format!("{path} stands as a file and as a deletion in one changeset")
}

/// The refusal a path named twice under `files` takes: two entries at
/// one path would ride in one body at two hashes, leaving which content
/// the commit holds to the receiving side (CR1-3).
pub fn path_named_twice(path: &str) -> String {
    format!("{path} is named twice under files in one changeset")
}

/// The refusal an entry carrying both content members takes.
pub fn both_content_members(path: &str) -> String {
    format!("{path} carries both content and contentBase64 in one changeset")
}

/// The refusal an entry carrying neither content member takes.
pub fn neither_content_member(path: &str) -> String {
    format!("{path} carries neither content nor contentBase64 in one changeset")
}

/// The refusal a `contentBase64` the `file bytes` form does not admit
/// takes — whitespace, missing padding or trailing bits among it.
pub fn content_base64_undecodable(path: &str) -> String {
    format!("the contentBase64 of {path} is not standard padded base64")
}

/// Issue 160: every refusal of the changeset document's own shape is
/// `invalid_changeset` under `--json`, its line unchanged.
fn invalid_changeset(message: String) -> CliError {
    CliError::InvalidChangeset { message }
}

/// Whether the document names `copies` ahead of `moves` at its top level,
/// so the report lists its members in the order the document gives them
/// (D-253 §9). A document that does not read as a map answers no.
pub fn copies_named_first(bytes: &[u8]) -> bool {
    struct Keys(Vec<String>);
    impl<'de> serde::Deserialize<'de> for Keys {
        fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
            struct Visitor;
            impl<'de> serde::de::Visitor<'de> for Visitor {
                type Value = Keys;
                fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
                    f.write_str("a changeset document")
                }
                fn visit_map<A: serde::de::MapAccess<'de>>(
                    self,
                    mut map: A,
                ) -> Result<Keys, A::Error> {
                    let mut keys = Vec::new();
                    while let Some(key) = map.next_key::<String>()? {
                        map.next_value::<serde::de::IgnoredAny>()?;
                        keys.push(key);
                    }
                    Ok(Keys(keys))
                }
            }
            deserializer.deserialize_map(Visitor)
        }
    }
    let Ok(Keys(keys)) = serde_json::from_slice::<Keys>(bytes) else {
        return false;
    };
    let at = |name: &str| keys.iter().position(|key| key == name);
    matches!((at("copies"), at("moves")), (Some(copies), Some(moves)) if copies < moves)
}

/// Roadmap 228: the document's moves and copies as members, in the order
/// the document gives them, each path resolved lexically counted from
/// `folder` (`member_path`), then every overlap D-253 §1 refuses among
/// them and the document's files and deletions — each refusal
/// `invalid_changeset`, before any request.
pub fn changeset_members(
    document: &ChangesetDocument,
    copies_first: bool,
    folder: Option<&FolderScope>,
) -> Result<Vec<Member>, CliError> {
    let resolve = |kind: MemberKind, entry: &ChangesetMember| -> Result<Member, CliError> {
        let lexical = |typed: &str| {
            member_path(folder, typed).map_err(|err| match err {
                CliError::Config { message } => invalid_changeset(message),
                other => other,
            })
        };
        Ok(Member {
            kind,
            from: lexical(&entry.from)?,
            to: lexical(&entry.to)?,
        })
    };
    let moves = document.moves.iter().map(|m| resolve(MemberKind::Move, m));
    let copies = document.copies.iter().map(|m| resolve(MemberKind::Copy, m));
    let members: Vec<Member> = if copies_first {
        copies.chain(moves).collect::<Result<_, _>>()?
    } else {
        moves.chain(copies).collect::<Result<_, _>>()?
    };
    let lexical = |path: &str| {
        let read = as_server_reads(as_server_reads(path));
        resolve_lexically(read).unwrap_or_else(|| read.to_string())
    };
    let files: Vec<String> = document.files.iter().map(|f| lexical(&f.path)).collect();
    let deletions: Vec<String> = document
        .deletions
        .iter()
        .map(|d| lexical(&d.path))
        .collect();
    let files: Vec<&str> = files.iter().map(String::as_str).collect();
    let deletions: Vec<&str> = deletions.iter().map(String::as_str).collect();
    match refuse_overlaps(&members, &files, &deletions) {
        Some(line) => Err(invalid_changeset(line)),
        None => Ok(members),
    }
}

/// Parses the document, refusing every shape the eight keys do not admit.
pub fn parse_changeset_document(bytes: &[u8]) -> Result<ChangesetDocument, CliError> {
    serde_json::from_slice(bytes).map_err(|e| CliError::InvalidChangeset {
        message: format!("the changeset document does not parse: {e}"),
    })
}

/// `cmd_commit` 2 to 4 (SPEC u283): in path order, refuse a path
/// standing in both members or twice under `files` and an entry carrying
/// both content members or neither, before any entry is decoded; then
/// take one entry at a time — a `contentBase64` decoded and classified
/// with the string it arrived as, a `content` classified as text under
/// the `Commit` wording — so no two decoded contents are held at once.
pub fn classify_changeset(document: ChangesetDocument) -> Result<Changeset, CliError> {
    let mut files: Vec<ChangesetFile> = document.files;
    files.sort_by(|a, b| a.path.cmp(&b.path));
    let deletions: Vec<String> = document.deletions.into_iter().map(|d| d.path).collect();

    // 2 — the shape of each entry, before any content is decoded.
    // `files` is in path order here, so a repeat stands beside its first.
    let mut previous: Option<&str> = None;
    for file in &files {
        if deletions.contains(&file.path) {
            return Err(invalid_changeset(path_in_both_members(&file.path)));
        }
        if previous == Some(file.path.as_str()) {
            return Err(invalid_changeset(path_named_twice(&file.path)));
        }
        match (&file.content, &file.content_base64) {
            (Some(_), Some(_)) => return Err(invalid_changeset(both_content_members(&file.path))),
            (None, None) => return Err(invalid_changeset(neither_content_member(&file.path))),
            _ => {}
        }
        previous = Some(file.path.as_str());
    }

    // 3 and 4 — one entry at a time, each content moved rather than
    // copied.
    let mut classified: Vec<(String, FileContent)> = Vec::with_capacity(files.len());
    for file in files {
        let content = match (file.content, file.content_base64) {
            (Some(text), None) => {
                text_or_refuse(&file.path, text.into_bytes(), NotTextSurface::Commit)?
            }
            (None, Some(sent)) => {
                let decoded = STANDARD
                    .decode(sent.as_bytes())
                    .map_err(|_| invalid_changeset(content_base64_undecodable(&file.path)))?;
                classify_content(&file.path, decoded, Some(sent))?
            }
            _ => unreachable!("step 2 refused an entry carrying both members or neither"),
        };
        classified.push((file.path, content));
    }
    Ok(Changeset {
        files: classified,
        deletions,
        members: Vec::new(),
    })
}

pub async fn cmd_commit(
    config: &Config,
    output: &Output,
    opts: WriteOptions,
) -> Result<(), CliError> {
    // 2's refusal: a run whose standard input is a terminal makes no
    // request at all.
    refuse_terminal_standard_input("the changeset document")?;

    // 1 — bind the repository, the folder and the credential: every
    // refusal of the write target that needs no request.
    let cwd = std::env::current_dir().map_err(|e| CliError::Io {
        message: format!("could not determine current directory: {e}"),
    })?;
    let bound = bind_write_target(config, &cwd, &opts)?;

    // The checkout guard, ahead of the document as `cli-commands/commit`
    // orders them (D-256 §5).
    let guarded = guard_write_target(config, &cwd, &opts, bound).await?;

    // 2 — read standard input to end-of-input and parse the document,
    // ahead of the write target's requests (roadmap 228).
    let bytes = read_standard_input("the changeset document")?;
    let document = parse_changeset_document(&bytes)?;

    // 3 — refuse a changeset naming no file, deletion, move or copy,
    // before any request.
    if document.files.is_empty()
        && document.deletions.is_empty()
        && document.moves.is_empty()
        && document.copies.is_empty()
    {
        return Err(CliError::ChangesetEmpty);
    }

    // Roadmap 228 — the moves and copies, their paths and their
    // overlaps, counted from the folder the target binds.
    let members = changeset_members(&document, copies_named_first(&bytes), guarded.folder())?;
    drop(bytes);

    // 2 to 4 — check each entry's shape, then decode and classify each
    // content in path order.
    let mut changeset = classify_changeset(document)?;
    changeset.members = members;

    // 1, continued — the repository read and the parent.
    let target = finish_write_target(config, &opts, guarded).await?;

    // 5 — commit the whole changeset.
    let message = default_message("commit", None);
    commit_changeset(config, output, &target, changeset, &opts, &message).await
}

#[cfg(test)]
mod tests {
    use super::*;

    // Roadmap 228: `moves` and `copies` parse as `{ from, to }` entries,
    // a key outside the two refusing the document.
    #[test]
    fn moves_and_copies_parse_and_refuse_an_unknown_key() {
        let document = parse_changeset_document(
            br#"{"moves":[{"from":"a","to":"b"}],"copies":[{"from":"t","to":"u"}]}"#,
        )
        .unwrap();
        assert_eq!(
            document.moves,
            vec![ChangesetMember {
                from: "a".to_string(),
                to: "b".to_string()
            }]
        );
        assert_eq!(document.copies.len(), 1);
        for refused in [
            br#"{"moves":[{"from":"a","to":"b","keep":true}]}"#.as_slice(),
            br#"{"copies":[{"from":"a"}]}"#.as_slice(),
            br#"{"renames":[]}"#.as_slice(),
        ] {
            let err = parse_changeset_document(refused).unwrap_err();
            assert_eq!(err.code(), "invalid_changeset");
        }
    }

    // D-253 §9: the report follows the document's own key order.
    #[test]
    fn the_key_order_of_the_document_is_read() {
        assert!(copies_named_first(br#"{"copies":[],"moves":[]}"#));
        assert!(!copies_named_first(br#"{"moves":[],"copies":[]}"#));
        assert!(!copies_named_first(br#"{"copies":[]}"#));
        assert!(!copies_named_first(b"[1]"));
    }

    // Roadmap 228: a member path refused lexically is `invalid_changeset`
    // on `syns commit`, its line the one `syns mv` gives; members keep
    // the document's order.
    #[test]
    fn member_paths_resolve_and_refuse_as_invalid_changeset() {
        let document = parse_changeset_document(
            br#"{"copies":[{"from":"t/","to":"./u"}],"moves":[{"from":"a","to":"b"}]}"#,
        )
        .unwrap();
        let members = changeset_members(&document, true, None).unwrap();
        assert_eq!(
            members,
            vec![
                Member {
                    kind: MemberKind::Copy,
                    from: "t".to_string(),
                    to: "u".to_string()
                },
                Member {
                    kind: MemberKind::Move,
                    from: "a".to_string(),
                    to: "b".to_string()
                },
            ]
        );
        let document = parse_changeset_document(br#"{"moves":[{"from":"..","to":"b"}]}"#).unwrap();
        let err = changeset_members(&document, false, None).unwrap_err();
        assert_eq!(err.code(), "invalid_changeset");
        assert_eq!(
            err.to_string(),
            "configuration error: .. names no path inside the repository; name one counted from its root"
        );
    }

    // SPEC u271 Contract Surface, the changeset document: both members
    // are optional and absent reads as empty.
    #[test]
    fn both_members_are_optional_and_absent_reads_as_empty() {
        assert_eq!(
            parse_changeset_document(b"{}").unwrap(),
            ChangesetDocument::default()
        );
        let files_only =
            parse_changeset_document(br#"{"files":[{"path":"a.md","content":"x"}]}"#).unwrap();
        assert_eq!(files_only.files.len(), 1);
        assert!(files_only.deletions.is_empty());
    }

    // A key outside the four refuses the document, at each of its three
    // levels.
    #[test]
    fn a_key_the_shape_does_not_register_refuses_the_document() {
        for document in [
            br#"{"files":[],"deletions":[],"author":"ana"}"#.as_slice(),
            br#"{"files":[{"path":"a.md","content":"x","sha":"deadbeef"}]}"#.as_slice(),
            br#"{"deletions":[{"path":"a.md","recursive":true}]}"#.as_slice(),
        ] {
            let err = parse_changeset_document(document).unwrap_err();
            assert!(err.to_string().starts_with("configuration error:"), "{err}");
            assert_eq!(err.exit_code(), 1);
        }
    }

    // A string escape naming an unpaired surrogate never becomes a
    // `String`, so the document does not parse (`SPEC_REVIEW_R2.md`
    // Library Candidates): the refusal is the parse's, not a check of
    // this unit's own.
    #[test]
    fn an_unpaired_surrogate_escape_refuses_the_document_at_the_parse() {
        let err = parse_changeset_document(br#"{"files":[{"path":"a.md","content":"\ud800"}]}"#)
            .unwrap_err();
        assert!(err.to_string().starts_with("configuration error:"), "{err}");
        assert_eq!(err.exit_code(), 1);
    }

    // `cmd_commit` 4: a path standing in both members is refused, and
    // each content is classified in path order.
    #[test]
    fn a_path_standing_in_both_members_is_refused() {
        let document = parse_changeset_document(
            br#"{"files":[{"path":"a.md","content":"x"}],"deletions":[{"path":"a.md"}]}"#,
        )
        .unwrap();
        let err = classify_changeset(document).unwrap_err();
        assert_eq!(
            err.to_string(),
            "configuration error: a.md stands as a file and as a deletion in one changeset"
        );
    }

    // CR1-3: a path named twice under `files` never reaches the wire
    // at two hashes.
    #[test]
    fn a_path_named_twice_under_files_is_refused() {
        let document = parse_changeset_document(
            br#"{"files":[{"path":"a.md","content":"x"},{"path":"a.md","content":"y"}]}"#,
        )
        .unwrap();
        let err = classify_changeset(document).unwrap_err();
        assert_eq!(
            err.to_string(),
            "configuration error: a.md is named twice under files in one changeset"
        );
        assert_eq!(err.exit_code(), 1);
    }

    #[test]
    fn contents_are_classified_in_path_order() {
        let document = parse_changeset_document(
            br#"{"files":[{"path":"z.md","content":"ok"},{"path":"a.bin","content":"a\u0000b"}]}"#,
        )
        .unwrap();
        let err = classify_changeset(document).unwrap_err();
        assert!(
            err.to_string().contains("a.bin"),
            "the first refused path in path order: {err}"
        );

        let ordered = parse_changeset_document(
            br#"{"files":[{"path":"z.md","content":"z"},{"path":"a.md","content":"a"}],"deletions":[{"path":"g.md"}]}"#,
        )
        .unwrap();
        let changeset = classify_changeset(ordered).unwrap();
        assert_eq!(
            changeset.files,
            vec![
                ("a.md".to_string(), FileContent::Text("a".to_string())),
                ("z.md".to_string(), FileContent::Text("z".to_string()))
            ]
        );
        assert_eq!(changeset.deletions, vec!["g.md".to_string()]);
    }

    // SPEC u283 Contract Surface, the changeset document: a document the
    // released build admitted classifies to the changeset it did.
    #[test]
    fn a_released_document_classifies_as_it_did() {
        let document = parse_changeset_document(
            br#"{"files":[{"path":"b.md","content":"b\n"}],"deletions":[{"path":"c.md"}]}"#,
        )
        .unwrap();
        assert_eq!(
            classify_changeset(document).unwrap(),
            Changeset {
                files: vec![("b.md".to_string(), FileContent::Text("b\n".to_string()))],
                deletions: vec!["c.md".to_string()],
                members: Vec::new(),
            }
        );
    }

    // SPEC u283 Contract Surface, the changeset entry refusals: both
    // members, neither member, and each malformed `contentBase64`.
    #[test]
    fn each_malformed_entry_is_refused_naming_its_path() {
        for (document, line) in [
            (
                r#"{"files":[{"path":"a.md","content":"x","contentBase64":"eA=="}]}"#,
                "configuration error: a.md carries both content and contentBase64 in one changeset",
            ),
            (
                r#"{"files":[{"path":"a.md"}]}"#,
                "configuration error: a.md carries neither content nor contentBase64 in one changeset",
            ),
            (
                r#"{"files":[{"path":"a.md","contentBase64":"aGVs bG8K"}]}"#,
                "configuration error: the contentBase64 of a.md is not standard padded base64",
            ),
            (
                r#"{"files":[{"path":"a.md","contentBase64":"aGVsbG8"}]}"#,
                "configuration error: the contentBase64 of a.md is not standard padded base64",
            ),
            (
                r#"{"files":[{"path":"a.md","contentBase64":"aGVsbG9="}]}"#,
                "configuration error: the contentBase64 of a.md is not standard padded base64",
            ),
        ] {
            let parsed = parse_changeset_document(document.as_bytes()).unwrap();
            let err = classify_changeset(parsed).unwrap_err();
            assert_eq!(err.to_string(), line, "{document}");
            assert_eq!(err.exit_code(), 1);
        }
    }

    // `cmd_commit` 2: an entry's shape is refused before any entry is
    // decoded, so a malformed later entry wins over an undecodable
    // earlier one.
    #[test]
    fn shapes_are_checked_before_any_entry_is_decoded() {
        let document = parse_changeset_document(
            br#"{"files":[{"path":"a.md","contentBase64":"!!"},{"path":"z.md"}]}"#,
        )
        .unwrap();
        assert_eq!(
            classify_changeset(document).unwrap_err().to_string(),
            "configuration error: z.md carries neither content nor contentBase64 in one changeset"
        );
    }

    // `cmd_commit` 4: a `content` escaping NUL names the byte member.
    #[test]
    fn a_nul_in_content_names_the_byte_member() {
        let document =
            parse_changeset_document(br#"{"files":[{"path":"a.bin","content":"a\u0000b"}]}"#)
                .unwrap();
        assert_eq!(
            classify_changeset(document).unwrap_err().to_string(),
            "cannot write content that is not text: a.bin \u{2014} send it as contentBase64 to publish its bytes exactly"
        );
    }

    // `cmd_commit` 3 and 4: text and bytes in one document, in path
    // order, the bytes carrying the string sent and text decoded from
    // base64 riding as text.
    #[test]
    fn one_document_yields_text_and_encoded_in_path_order() {
        let jpeg: &[u8] = b"\xff\xd8\xff\x00";
        let sent = STANDARD.encode(jpeg);
        let document = parse_changeset_document(
            format!(
                r#"{{"files":[{{"path":"photo.jpg","contentBase64":"{sent}"}},{{"path":"b.md","contentBase64":"aGVsbG8K"}},{{"path":"a.md","content":"a"}}]}}"#
            )
            .as_bytes(),
        )
        .unwrap();
        let changeset = classify_changeset(document).unwrap();
        assert_eq!(
            changeset.files,
            vec![
                ("a.md".to_string(), FileContent::Text("a".to_string())),
                ("b.md".to_string(), FileContent::Text("hello\n".to_string())),
                (
                    "photo.jpg".to_string(),
                    FileContent::Encoded {
                        base64: sent,
                        sha: crate::push::hash::blob_sha1(jpeg),
                    }
                ),
            ]
        );
    }
}
