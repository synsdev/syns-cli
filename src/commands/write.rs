//! `syns write PATH --parent REF` — one commit holding exactly the bytes
//! standard input carried (SPEC u271).
//!
//! Standard input is a data channel alone here: it is read to
//! end-of-input, no confirmation surface contends for it, and an empty
//! stream gives an empty file. No other path of the tree is named.

use crate::config::Config;
use crate::errors::CliError;
use crate::output::Output;
use crate::write::{
    Changeset, WriteOptions, commit_changeset, default_message, read_standard_input,
    refuse_terminal_standard_input, resolve_write_target, text_or_refuse,
};

/// `cmd_write` 3 and 4: the one path at exactly the bytes standard
/// input carried, an empty stream giving an empty file, and no other
/// path of the tree named.
pub fn changeset_for(path: &str, bytes: &[u8]) -> Result<Changeset, CliError> {
    Ok(Changeset {
        files: vec![(path.to_string(), text_or_refuse(path, bytes)?)],
        deletions: Vec::new(),
    })
}

pub async fn cmd_write(
    config: &Config,
    output: &Output,
    path: String,
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
    let bytes = read_standard_input(&format!("the content of {path}"))?;

    // 3 and 4 — classify the bytes read and name that one path at them.
    let changeset = changeset_for(&path, &bytes)?;
    let message = default_message("write", Some(&path));
    commit_changeset(config, output, &target, changeset, &opts, &message).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::push::hash::blob_sha1;

    // SPEC u271 Contract Surface, `cmd_write`: the path holds exactly
    // the bytes standard input carried, an empty stream gives an empty
    // file, and no other path of the tree is named.
    #[test]
    fn the_one_path_carries_the_bytes_read_and_their_derived_hash() {
        let changeset = changeset_for("b.md", b"hello").unwrap();
        assert_eq!(
            changeset.files,
            vec![("b.md".to_string(), "hello".to_string())]
        );
        assert!(changeset.deletions.is_empty());
        assert_eq!(
            blob_sha1(changeset.files[0].1.as_bytes()),
            blob_sha1(b"hello")
        );

        let empty = changeset_for("b.md", b"").unwrap();
        assert_eq!(empty.files, vec![("b.md".to_string(), String::new())]);
    }

    // `cmd_write` 3: a content that is not text is refused ahead of the
    // push, naming the path.
    #[test]
    fn a_content_that_is_not_text_is_refused_naming_the_path() {
        let err = changeset_for("b.bin", b"ab\0cd").unwrap_err();
        assert!(err.to_string().contains("b.bin"));
        assert_eq!(err.exit_code(), 1);
        assert!(changeset_for("b.bin", &[0x80, 0x00]).is_err());
    }
}
