//! `syns commit --parent REF` — one commit composed from a changeset
//! document read on standard input (SPEC u271).
//!
//! The whole changeset lands as one commit or none of it lands at all:
//! every refusal below is raised before any request leaves.

use crate::config::Config;
use crate::errors::CliError;
use crate::output::Output;
use crate::write::{
    Changeset, WriteOptions, commit_changeset, default_message, read_standard_input,
    refuse_terminal_standard_input, resolve_write_target, text_or_refuse,
};

/// The changeset document: `{ files: [{ path, content }], deletions: [{ path }] }`.
///
/// Both members are optional and absent reads as empty, and a key
/// outside the four refuses the document. A string escape naming an
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
}

#[derive(serde::Deserialize, Debug, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ChangesetFile {
    pub path: String,
    pub content: String,
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

/// Parses the document, refusing every shape the four keys do not admit.
pub fn parse_changeset_document(bytes: &[u8]) -> Result<ChangesetDocument, CliError> {
    serde_json::from_slice(bytes).map_err(|e| CliError::Config {
        message: format!("the changeset document does not parse: {e}"),
    })
}

/// `cmd_commit` 4: classify each content in path order, and refuse a
/// path standing in both members.
pub fn classify_changeset(document: ChangesetDocument) -> Result<Changeset, CliError> {
    let mut files: Vec<ChangesetFile> = document.files;
    files.sort_by(|a, b| a.path.cmp(&b.path));
    let deletions: Vec<String> = document.deletions.into_iter().map(|d| d.path).collect();

    let mut classified = Vec::with_capacity(files.len());
    for file in &files {
        if deletions.contains(&file.path) {
            return Err(CliError::Config {
                message: path_in_both_members(&file.path),
            });
        }
        classified.push((
            file.path.clone(),
            text_or_refuse(&file.path, file.content.as_bytes())?,
        ));
    }
    Ok(Changeset {
        files: classified,
        deletions,
    })
}

pub async fn cmd_commit(
    config: &Config,
    output: &Output,
    opts: WriteOptions,
) -> Result<(), CliError> {
    // 2's refusal, raised ahead of step 1: a run whose standard input
    // is a terminal makes no request at all.
    refuse_terminal_standard_input("the changeset document")?;

    // 1 — resolve the write target.
    let cwd = std::env::current_dir().map_err(|e| CliError::Io {
        message: format!("could not determine current directory: {e}"),
    })?;
    let target = resolve_write_target(config, &cwd, &opts).await?;

    // 2 — read standard input to end-of-input and parse the document.
    let bytes = read_standard_input("the changeset document")?;
    let document = parse_changeset_document(&bytes)?;

    // 3 — refuse a changeset naming neither a file nor a deletion,
    // before any request.
    if document.files.is_empty() && document.deletions.is_empty() {
        return Err(CliError::ChangesetEmpty);
    }

    // 4 — classify each content in path order.
    let changeset = classify_changeset(document)?;

    // 5 — commit the whole changeset.
    let message = default_message("commit", None);
    commit_changeset(config, output, &target, changeset, &opts, &message).await
}

#[cfg(test)]
mod tests {
    use super::*;

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
                ("a.md".to_string(), "a".to_string()),
                ("z.md".to_string(), "z".to_string())
            ]
        );
        assert_eq!(changeset.deletions, vec!["g.md".to_string()]);
    }
}
