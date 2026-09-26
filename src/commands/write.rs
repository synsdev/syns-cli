//! `syns write PATH --parent REF [--bytes]` — one commit holding exactly
//! the bytes standard input carried (SPEC u271, u283).
//!
//! Standard input is a data channel alone here: it is read to
//! end-of-input, no confirmation surface contends for it, and an empty
//! stream gives an empty file. No other path of the tree is named.
//! Without `--bytes` a content that is not text is refused; under it the
//! bytes ride whatever they hold (`D-088`).

use crate::config::Config;
use crate::errors::{CliError, NotTextSurface};
use crate::output::Output;
use crate::write::{
    Changeset, WriteOptions, classify_content, commit_changeset, default_message,
    read_standard_input, refuse_terminal_standard_input, resolve_write_target, text_or_refuse,
};

/// `cmd_write` 3 and 4: the one path at exactly the bytes standard
/// input carried, an empty stream giving an empty file, and no other
/// path of the tree named. Under `declared_bytes` (`--bytes`) the content
/// is classified whatever it holds; otherwise one that is not text is
/// refused under the `Write` wording.
pub fn changeset_for(
    path: &str,
    declared_bytes: bool,
    content: Vec<u8>,
) -> Result<Changeset, CliError> {
    let content = if declared_bytes {
        classify_content(path, content, None)?
    } else {
        text_or_refuse(path, content, NotTextSurface::Write)?
    };
    Ok(Changeset {
        files: vec![(path.to_string(), content)],
        deletions: Vec::new(),
    })
}

pub async fn cmd_write(
    config: &Config,
    output: &Output,
    path: String,
    bytes: bool,
    opts: WriteOptions,
) -> Result<(), CliError> {
    // 2's refusal, raised ahead of step 1: a run whose standard input
    // is a terminal makes no request at all.
    refuse_terminal_standard_input(&format!("the content of {path}"))?;

    // 1 — resolve the write target.
    let cwd = std::env::current_dir().map_err(|e| CliError::Io {
        message: format!("could not determine current directory: {e}"),
    })?;
    let target = resolve_write_target(config, &cwd, &opts).await?;

    // 2 — read standard input to end-of-input.
    let content = read_standard_input(&format!("the content of {path}"))?;

    // 3 and 4 — classify the bytes read and name that one path at them.
    let changeset = changeset_for(&path, bytes, content)?;
    let message = default_message("write", Some(&path));
    commit_changeset(config, output, &target, changeset, &opts, &message).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::push::hash::blob_sha1;
    use crate::write::FileContent;
    use base64::Engine as _;
    use base64::engine::general_purpose::STANDARD;

    // SPEC u271 Contract Surface, `cmd_write`: the path holds exactly
    // the bytes standard input carried, an empty stream gives an empty
    // file, and no other path of the tree is named.
    #[test]
    fn the_one_path_carries_the_bytes_read_and_their_derived_hash() {
        let changeset = changeset_for("b.md", false, b"hello".to_vec()).unwrap();
        assert_eq!(
            changeset.files,
            vec![("b.md".to_string(), FileContent::Text("hello".to_string()))]
        );
        assert!(changeset.deletions.is_empty());

        let empty = changeset_for("b.md", false, Vec::new()).unwrap();
        assert_eq!(
            empty.files,
            vec![("b.md".to_string(), FileContent::Text(String::new()))]
        );
    }

    // `cmd_write` 3: a content that is not text is refused ahead of the
    // push without `--bytes`, naming the path and the flag.
    #[test]
    fn a_content_that_is_not_text_is_refused_naming_the_path() {
        let err = changeset_for("b.bin", false, b"ab\0cd".to_vec()).unwrap_err();
        assert_eq!(
            err.to_string(),
            "cannot write content that is not text: b.bin \u{2014} pass --bytes to publish its bytes exactly"
        );
        assert_eq!(err.exit_code(), 1);
        assert!(changeset_for("b.bin", false, vec![0x80, 0x00]).is_err());
    }

    // SPEC u283, `cmd_write` 3: under `--bytes` bytes that are not text
    // are encoded beside their blob hash, and text still rides as text.
    #[test]
    fn declared_bytes_classify_whatever_they_hold() {
        let png = b"\x89PNG\r\n\x1a\n\x00\xff".to_vec();
        let changeset = changeset_for("image.png", true, png.clone()).unwrap();
        assert_eq!(
            changeset.files,
            vec![(
                "image.png".to_string(),
                FileContent::Encoded {
                    base64: STANDARD.encode(&png),
                    sha: blob_sha1(&png),
                }
            )]
        );

        let text = changeset_for("a.md", true, b"hello\n".to_vec()).unwrap();
        assert_eq!(
            text.files,
            vec![("a.md".to_string(), FileContent::Text("hello\n".to_string()))]
        );
    }
}
