use crate::errors::CliError;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::Path;

#[derive(Serialize, Deserialize, Default)]
pub struct Manifest {
    commit_sha: Option<String>,
    files: HashMap<String, String>,
}

impl Manifest {
    pub fn load(cache_dir: &Path, owner: &str, name: &str) -> Option<Manifest> {
        let path = cache_dir.join(owner).join(format!("{name}.json"));
        let content = std::fs::read_to_string(&path).ok()?;
        let manifest: Manifest = serde_json::from_str(&content).ok()?;
        // Defensive stub guard (SPEC u213 § 4): reject manifests that
        // carry an empty/missing commit_sha OR an empty files map.
        // Both shapes are produced by issue 070's empty-sha write
        // path or by a corrupted disk artifact; either way they
        // cannot be trusted as an authoritative reference.
        match manifest.commit_sha.as_deref() {
            None | Some("") => return None,
            Some(_) => {}
        }
        if manifest.files.is_empty() {
            return None;
        }
        Some(manifest)
    }

    pub fn save(&self, cache_dir: &Path, owner: &str, name: &str) -> Result<(), CliError> {
        let path = cache_dir.join(owner).join(format!("{name}.json"));
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|err| CliError::Io {
                message: format!("could not create manifest directory: {err}"),
            })?;
        }
        let json = serde_json::to_string_pretty(self).map_err(|e| CliError::Io {
            message: format!("could not serialize manifest: {e}"),
        })?;
        std::fs::write(&path, json).map_err(|err| CliError::Io {
            message: format!("could not save manifest: {err}"),
        })
    }

    pub fn file_sha(&self, path: &str) -> Option<&str> {
        self.files.get(path).map(|s| s.as_str())
    }

    pub fn update(&mut self, commit_sha: String, files: HashMap<String, String>) {
        self.commit_sha = Some(commit_sha);
        self.files = files;
    }

    pub fn commit_sha(&self) -> Option<&str> {
        self.commit_sha.as_deref()
    }

    pub fn file_paths(&self) -> impl Iterator<Item = &str> {
        self.files.keys().map(|s| s.as_str())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn save_and_load_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let mut manifest = Manifest::default();
        manifest.update(
            "abc123".to_string(),
            HashMap::from([
                ("src/main.rs".to_string(), "deadbeef".to_string()),
                ("README.md".to_string(), "cafebabe".to_string()),
            ]),
        );
        manifest.save(dir.path(), "bart", "my-project").unwrap();
        let loaded = Manifest::load(dir.path(), "bart", "my-project").unwrap();
        assert_eq!(loaded.commit_sha(), Some("abc123"));
        assert_eq!(loaded.file_sha("src/main.rs"), Some("deadbeef"));
        assert_eq!(loaded.file_sha("README.md"), Some("cafebabe"));
    }

    #[test]
    fn load_returns_none_for_nonexistent_file() {
        let dir = tempfile::tempdir().unwrap();
        assert!(Manifest::load(dir.path(), "bart", "no-such-repo").is_none());
    }

    #[test]
    fn save_creates_parent_directories() {
        let dir = tempfile::tempdir().unwrap();
        let mut manifest = Manifest::default();
        manifest.update(
            "sha1".to_string(),
            HashMap::from([("a.txt".to_string(), "aaa".to_string())]),
        );
        manifest.save(dir.path(), "alice", "new-repo").unwrap();
        assert!(dir.path().join("alice").join("new-repo.json").exists());
        let loaded = Manifest::load(dir.path(), "alice", "new-repo").unwrap();
        assert_eq!(loaded.commit_sha(), Some("sha1"));
    }

    #[test]
    fn update_replaces_old_entries() {
        let mut manifest = Manifest::default();
        manifest.update(
            "sha1".to_string(),
            HashMap::from([("a.txt".to_string(), "aaa".to_string())]),
        );
        manifest.update(
            "sha2".to_string(),
            HashMap::from([("b.txt".to_string(), "bbb".to_string())]),
        );
        assert_eq!(manifest.commit_sha(), Some("sha2"));
        assert_eq!(manifest.file_sha("b.txt"), Some("bbb"));
        assert_eq!(manifest.file_sha("a.txt"), None);
    }

    #[test]
    fn file_paths_returns_all_keys() {
        let mut manifest = Manifest::default();
        manifest.update(
            "sha1".to_string(),
            HashMap::from([
                ("a.txt".to_string(), "aaa".to_string()),
                ("b.txt".to_string(), "bbb".to_string()),
            ]),
        );
        let mut paths: Vec<&str> = manifest.file_paths().collect();
        paths.sort();
        assert_eq!(paths, vec!["a.txt", "b.txt"]);
    }

    #[test]
    fn file_sha_returns_none_for_unknown_path() {
        let mut manifest = Manifest::default();
        manifest.update(
            "sha1".to_string(),
            HashMap::from([("known.txt".to_string(), "abc123".to_string())]),
        );
        assert_eq!(manifest.file_sha("unknown.txt"), None);
    }

    #[test]
    fn load_rejects_stub_with_empty_commit_sha() {
        let dir = tempfile::tempdir().unwrap();
        let manifest_path = dir.path().join("alice").join("repo.json");
        std::fs::create_dir_all(manifest_path.parent().unwrap()).unwrap();
        std::fs::write(&manifest_path, r#"{"commit_sha":"","files":{}}"#).unwrap();
        assert!(Manifest::load(dir.path(), "alice", "repo").is_none());
    }

    #[test]
    fn load_rejects_stub_with_missing_commit_sha() {
        let dir = tempfile::tempdir().unwrap();
        let manifest_path = dir.path().join("alice").join("repo.json");
        std::fs::create_dir_all(manifest_path.parent().unwrap()).unwrap();
        std::fs::write(&manifest_path, r#"{"files":{"a.txt":"abc"}}"#).unwrap();
        assert!(Manifest::load(dir.path(), "alice", "repo").is_none());
    }

    #[test]
    fn load_rejects_manifest_with_empty_files() {
        let dir = tempfile::tempdir().unwrap();
        let manifest_path = dir.path().join("alice").join("repo.json");
        std::fs::create_dir_all(manifest_path.parent().unwrap()).unwrap();
        std::fs::write(&manifest_path, r#"{"commit_sha":"deadbeef","files":{}}"#).unwrap();
        assert!(Manifest::load(dir.path(), "alice", "repo").is_none());
    }
}
