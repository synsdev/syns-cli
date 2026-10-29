//! Issue 376: `syns commit -h` and `--help` name every changeset member
//! and its keys, so a caller holding only the binary composes a
//! changeset (`cli-commands/commit/COMMAND_SPEC.md` § Stream Composition).

use assert_cmd::Command as AssertCommand;

/// Each member with the keys its entries admit.
const MEMBERS: &[(&str, &[&str])] = &[
    ("files", &["path", "content", "contentBase64"]),
    ("deletions", &["path"]),
    ("moves", &["from", "to"]),
    ("copies", &["from", "to"]),
    ("edits", &["path", "old", "new", "replaceAll"]),
];

fn help(flag: &str) -> String {
    let output = AssertCommand::cargo_bin("syns")
        .expect("syns binary")
        .args(["commit", flag])
        .output()
        .expect("subprocess output");
    assert_eq!(output.status.code(), Some(0), "syns commit {flag} exits 0");
    String::from_utf8(output.stdout).expect("help is UTF-8")
}

#[test]
fn commit_help_names_every_changeset_member_and_key() {
    for flag in ["-h", "--help"] {
        let text = help(flag);
        for (member, keys) in MEMBERS {
            let line = text
                .lines()
                .find(|l| l.trim_start().starts_with(&format!("{member} ")))
                .unwrap_or_else(|| {
                    panic!("syns commit {flag} names no `{member}` member:\n{text}")
                });
            // The keys stand inside the line's `[{…}]` shape, each a whole word.
            let shape = line
                .split_once("[{")
                .and_then(|(_, rest)| rest.split_once("}]"))
                .map(|(inner, _)| inner)
                .unwrap_or_else(|| {
                    panic!("syns commit {flag}: the `{member}` line has no shape: {line}")
                });
            let words: Vec<&str> = shape
                .split(|c: char| !c.is_ascii_alphanumeric())
                .filter(|w| !w.is_empty())
                .collect();
            for key in *keys {
                assert!(
                    words.contains(key),
                    "syns commit {flag}: the `{member}` line names no `{key}` key: {line}"
                );
            }
        }
        assert!(
            text.contains("| syns commit --parent"),
            "syns commit {flag} gives no pipeable example:\n{text}"
        );
    }
}
