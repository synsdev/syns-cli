//! Machine-local files, told by their file type or the shape of their
//! repository path (`D-173`, roadmap/232 `FEATURE_SPEC.md` § The rule,
//! R1–R7): files a tool, an editor or a file manager leaves in a folder
//! that belong to that machine alone. A collection drops each one beneath
//! the owner's ignore files and outside `--no-default-excludes`, and names
//! it under the reason `machine-local`.
//!
//! Every test here is pure: no file is opened, read or stat'd. R1 reads
//! the file type the walk's directory entry already carries; R2–R7 read
//! the `/`-joined repository path alone, letter case as written.

/// R3's exception list: the dependency lockfiles a package manager writes
/// and an owner publishes on purpose, matched by exact name (Q-0025 row
/// 3). They name package formats for an exception, not project markers
/// (`D-067` § 5).
pub const DEPENDENCY_LOCKFILES: &[&str] = &[
    "bun.lock",
    "yarn.lock",
    "Cargo.lock",
    "flake.lock",
    "Gemfile.lock",
    "poetry.lock",
    "uv.lock",
    "composer.lock",
    "Pipfile.lock",
    "Podfile.lock",
    "pubspec.lock",
    "deno.lock",
    "mix.lock",
];

/// R1: a socket, a FIFO, or a block or character device. Such a file
/// holds no bytes to publish; no `!` line re-includes it.
pub fn by_type(ft: std::fs::FileType) -> bool {
    !ft.is_file() && !ft.is_dir() && !ft.is_symlink()
}

/// Whether a directory segment is a dot-folder.
fn is_dot(segment: &str) -> bool {
    segment.starts_with('.')
}

/// R4's `<stem>.local` or `<stem>.local.<ext>`, `<stem>` not empty and
/// `<ext>` not empty and holding no dot.
fn is_stem_local(name: &str) -> bool {
    if let Some(stem) = name.strip_suffix(".local") {
        return !stem.is_empty();
    }
    let Some((before, ext)) = name.rsplit_once('.') else {
        return false;
    };
    !ext.is_empty()
        && before
            .strip_suffix(".local")
            .is_some_and(|stem| !stem.is_empty())
}

/// R2–R7 over a `/`-joined repository path.
pub fn by_name(rel: &str) -> bool {
    let (dirs, name) = match rel.rsplit_once('/') {
        Some((dirs, name)) => (Some(dirs), name),
        None => (None, rel),
    };
    let mut segments = dirs.into_iter().flat_map(|d| d.split('/'));
    let under_dot = segments.any(is_dot);
    let own_dir_dot = dirs.is_some_and(|d| is_dot(d.rsplit('/').next().unwrap_or(d)));

    // R5, R6, R7: the file's own name, at any depth.
    if name == ".DS_Store" || name == "Thumbs.db" {
        return true;
    }
    if name.len() > 2 && name.starts_with("._") {
        return true;
    }
    // R2: a pid file anywhere below a dot-folder.
    if name.ends_with(".pid") && under_dot {
        return true;
    }
    // R3: a lock file standing directly in a dot-folder, the dependency
    // lockfiles excepted.
    if name.ends_with(".lock") && own_dir_dot && !DEPENDENCY_LOCKFILES.contains(&name) {
        return true;
    }
    // R4: a dotfile ending in `.local`, or `<stem>.local[.<ext>]` below a
    // dot-folder.
    if is_dot(name) && name.strip_suffix(".local").is_some_and(|s| s.len() > 1) {
        return true;
    }
    under_dot && is_stem_local(name)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// One cell per path of the rule table's Caught and Not caught
    /// columns (FEATURE_SPEC § The rule).
    #[test]
    fn each_row_catches_its_caught_column_and_none_of_its_not_caught_column() {
        let rows: &[(&str, &[&str], &[&str])] = &[
            (
                "R2",
                &[
                    ".codex/session.pid",
                    ".pi/a.pid",
                    ".superpowers/brainstorm/1/state/server.pid",
                    "a/.x/b/c.pid",
                ],
                &["tmp/pids/server.pid", "server.pid", ".server.pid"],
            ),
            (
                "R3",
                &[
                    ".claude/scheduled_tasks.lock",
                    ".codex/x.lock",
                    ".pi/x.lock",
                    "a/.claude/b.lock",
                    ".claude/routines/.state/t.lock",
                ],
                &[
                    ".opencode/bun.lock",
                    ".x/Cargo.lock",
                    ".claude/skills/x/bun.lock",
                    ".claude/state/x.lock",
                    ".config/nix/flake.lock",
                    "Cargo.lock",
                    "yarn.lock",
                    "a/Cargo.lock",
                    "package-lock.json",
                    ".terraform.lock.hcl",
                ],
            ),
            (
                "R4",
                &[
                    ".env.local",
                    ".env.development.local",
                    "a/.env.local",
                    ".claude/settings.local.json",
                    ".idea/dataSources.local.xml",
                    ".devcontainer/.env.local.example",
                    ".config/app.local.example",
                ],
                &[
                    "CLAUDE.local.md",
                    "docs/a.local.txt",
                    "test/recordings/status.local.json",
                    ".env.local.example",
                    ".idea/x.local.tests.iml",
                    ".local",
                    "local.md",
                    "my.locale",
                    ".x/y.localhost.json",
                    ".eslintrc.local.json",
                ],
            ),
            (
                "R5",
                &[".DS_Store", "media/.DS_Store"],
                &["DS_Store", ".DS_Store.bak"],
            ),
            (
                "R6",
                &["Thumbs.db", "media/Thumbs.db"],
                &["thumbs.db", "Thumbs.db.bak"],
            ),
            (
                "R7",
                &["media/._cover.png", "._README.md", "a/._x"],
                &["._", "_x", "a._b", ".x"],
            ),
        ];
        for (row, caught, kept) in rows {
            for path in *caught {
                assert!(by_name(path), "{row}: {path} is caught");
            }
            for path in *kept {
                assert!(!by_name(path), "{row}: {path} is not caught");
            }
        }
    }

    #[test]
    fn desktop_ini_and_the_kept_files_publish() {
        for path in [
            "desktop.ini",
            "media/desktop.ini",
            "README.md",
            ".claude/.cc-writes/w1",
            ".claude/skills/x/SKILL.md",
            ".codex/config.toml",
            ".vscode/settings.json",
            ".github/workflows/ci.yml",
            "media/cover.png",
            ".syns.yaml",
        ] {
            assert!(!by_name(path), "{path} publishes");
        }
    }

    // R3: every exception standing directly in a dot-folder is kept, and
    // the names are exact.
    #[test]
    fn the_thirteen_dependency_lockfiles_in_a_dot_folder_are_kept_and_matched_exactly() {
        assert_eq!(DEPENDENCY_LOCKFILES.len(), 13);
        for name in DEPENDENCY_LOCKFILES {
            assert!(!by_name(&format!(".x/{name}")), ".x/{name} is kept");
        }
        assert!(by_name(".x/cargo.lock"), "the exception names are exact");
    }

    #[test]
    fn a_regular_file_a_directory_and_a_symbolic_link_are_not_caught_by_type() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("notes.sock");
        std::fs::write(&file, "x").unwrap();
        let ft = |p: &std::path::Path| std::fs::symlink_metadata(p).unwrap().file_type();
        assert!(!by_type(ft(&file)));
        assert!(!by_type(ft(dir.path())));
        #[cfg(unix)]
        {
            let link = dir.path().join("link");
            std::os::unix::fs::symlink(&file, &link).unwrap();
            assert!(!by_type(ft(&link)));
        }
    }

    // R1, one file kind at a time.
    #[cfg(unix)]
    #[test]
    fn a_socket_a_fifo_and_each_device_kind_are_caught_by_type() {
        use std::os::unix::fs::FileTypeExt;
        let dir = tempfile::tempdir().unwrap();
        let ft = |p: &std::path::Path| std::fs::symlink_metadata(p).unwrap().file_type();

        let sock = dir.path().join("app.sock");
        let _listener = std::os::unix::net::UnixListener::bind(&sock).unwrap();
        assert!(ft(&sock).is_socket());
        assert!(by_type(ft(&sock)), "a socket");

        let fifo = dir.path().join("p");
        let made = std::process::Command::new("mkfifo")
            .arg(&fifo)
            .status()
            .unwrap();
        assert!(made.success());
        assert!(ft(&fifo).is_fifo());
        assert!(by_type(ft(&fifo)), "a FIFO");

        let null = std::path::Path::new("/dev/null");
        assert!(ft(null).is_char_device());
        assert!(by_type(ft(null)), "a character device");

        // A block device stands at one of these on every host the suite
        // runs on that has one; a container may have none.
        let block = [
            "/dev/disk0",
            "/dev/sda",
            "/dev/vda",
            "/dev/nvme0n1",
            "/dev/loop0",
            "/dev/xvda",
        ]
        .into_iter()
        .map(std::path::Path::new)
        .find(|p| std::fs::symlink_metadata(p).is_ok_and(|m| m.file_type().is_block_device()));
        match block {
            Some(block) => assert!(by_type(ft(block)), "a block device"),
            None => eprintln!("no block device on this host; the block device kind is unasserted"),
        }
    }
}
