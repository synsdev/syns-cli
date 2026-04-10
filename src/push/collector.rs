use crate::errors::CliError;
use ignore::overrides::OverrideBuilder;
use ignore::WalkBuilder;
use std::collections::HashMap;
use std::ffi::OsStr;
use std::io::Read;
use std::path::Path;

const BINARY_CHECK_SIZE: usize = 8192;

fn to_forward_slash_path(path: &Path) -> Option<String> {
    let parts: Option<Vec<&str>> = path.components().map(|c| c.as_os_str().to_str()).collect();
    parts.map(|p| p.join("/"))
}

fn is_binary(path: &Path) -> Result<bool, std::io::Error> {
    let mut file = std::fs::File::open(path)?;
    let mut buf = [0u8; BINARY_CHECK_SIZE];
    let n = file.read(&mut buf)?;
    Ok(buf[..n].contains(&0))
}

pub fn collect_files(
    path: &Path,
    excludes: &[String],
) -> Result<HashMap<String, Vec<u8>>, CliError> {
    let mut overrides = OverrideBuilder::new(path);
    for pattern in excludes {
        overrides.add(&format!("!{pattern}")).map_err(|err| CliError::Io {
            message: format!("invalid exclude pattern '{pattern}': {err}"),
        })?;
    }
    let overrides = overrides.build().map_err(|err| CliError::Io {
        message: format!("invalid exclude pattern: {err}"),
    })?;

    let walker = WalkBuilder::new(path)
        .hidden(false)
        .require_git(false)
        .parents(true)
        .add_custom_ignore_filename(".synsignore")
        .overrides(overrides)
        .filter_entry(|entry| entry.file_name() != OsStr::new(".git"))
        .build();

    let mut file_map = HashMap::new();

    for result in walker {
        let entry = result.map_err(|err| CliError::Io {
            message: format!("walk error: {err}"),
        })?;

        let file_type = match entry.file_type() {
            Some(ft) => ft,
            None => continue,
        };

        if !file_type.is_file() {
            continue;
        }

        let rel_path = match entry.path().strip_prefix(path) {
            Ok(p) => p,
            Err(_) => continue,
        };

        let rel_path = match to_forward_slash_path(rel_path) {
            Some(p) => p,
            None => continue,
        };

        match is_binary(entry.path()) {
            Ok(true) => continue,
            Err(err) => {
                return Err(CliError::Io {
                    message: format!("could not read {rel_path}: {err}"),
                });
            }
            Ok(false) => {}
        }

        let contents = std::fs::read(entry.path()).map_err(|err| CliError::Io {
            message: format!("could not read {rel_path}: {err}"),
        })?;

        file_map.insert(rel_path, contents);
    }

    Ok(file_map)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn collects_all_files_in_directory() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.txt"), "hello").unwrap();
        std::fs::create_dir_all(dir.path().join("sub")).unwrap();
        std::fs::write(dir.path().join("sub/b.txt"), "world").unwrap();
        std::fs::create_dir_all(dir.path().join("sub/deep")).unwrap();
        std::fs::write(dir.path().join("sub/deep/c.txt"), "nested").unwrap();

        let files = collect_files(dir.path(), &[]).unwrap();
        assert_eq!(files.len(), 3);
        assert_eq!(files.get("a.txt").unwrap(), b"hello");
        assert_eq!(files.get("sub/b.txt").unwrap(), b"world");
        assert_eq!(files.get("sub/deep/c.txt").unwrap(), b"nested");
    }

    #[test]
    fn respects_gitignore_patterns() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("keep.txt"), "keep").unwrap();
        std::fs::write(dir.path().join("skip.log"), "skip").unwrap();
        std::fs::write(dir.path().join(".gitignore"), "*.log").unwrap();

        let files = collect_files(dir.path(), &[]).unwrap();
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

        let files = collect_files(dir.path(), &[]).unwrap();
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

        let files = collect_files(dir.path(), &["*.csv".to_string()]).unwrap();
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

        let files = collect_files(dir.path(), &[]).unwrap();
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

        let files = collect_files(dir.path(), &[]).unwrap();
        assert!(files.contains_key("keep.txt"));
        assert!(files.contains_key("real_dir/inner.txt"));
        assert!(!files.contains_key("link_dir"));
    }

    #[test]
    fn binary_files_are_skipped() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("text.txt"), "hello").unwrap();
        std::fs::write(dir.path().join("binary.bin"), b"binary\x00data").unwrap();

        let files = collect_files(dir.path(), &[]).unwrap();
        assert_eq!(files.len(), 1);
        assert_eq!(files.get("text.txt").unwrap(), b"hello");
        assert!(!files.contains_key("binary.bin"));
    }
}
