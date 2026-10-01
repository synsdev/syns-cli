use crate::config::{StoreRoots, is_foreign_or_link};
use crate::errors::CliError;
use crate::push::working_copy::{IN_ROOT_HOME, LOCAL_RECORD_FILE, ensure_in_root_home};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

#[derive(Serialize, Deserialize, Default)]
pub struct Manifest {
    commit_sha: Option<String>,
    files: HashMap<String, String>,
    /// SPEC u291, `Manifest.recorded_at`: the nanoseconds since the Unix
    /// epoch the system clock read as a working copy recorded this base,
    /// carried in the record's own bytes so records written back to back
    /// are ordered whatever their files' modification times say. None on
    /// a record written before u291 and on every local record.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    recorded_at: Option<u64>,
}

/// Where the local record of `owner/name` stands under the default root
/// (`D-025`), and where it stands in the in-root home of the content root
/// `root` (SPEC u298, `Manifest::save`).
fn record_places(stores: &StoreRoots, owner: &str, name: &str, root: &Path) -> (PathBuf, PathBuf) {
    let canonical = std::fs::canonicalize(root).unwrap_or_else(|_| root.to_path_buf());
    (
        stores.default.join(owner).join(format!("{name}.json")),
        canonical.join(IN_ROOT_HOME).join(LOCAL_RECORD_FILE),
    )
}

impl Manifest {
    /// The local record of `owner/name` for the content root `root`: of
    /// the record under the default root and the one in the root's in-root
    /// home, the one whose file was modified last, a tie going to the one
    /// `save` would write (SPEC u298, `Manifest::load`).
    pub fn load(stores: &StoreRoots, owner: &str, name: &str, root: &Path) -> Option<Manifest> {
        let (default, in_root) = record_places(stores, owner, name, root);
        let modified = |path: &Path| {
            std::fs::metadata(path)
                .ok()
                .filter(|meta| meta.is_file())
                .and_then(|meta| meta.modified().ok())
        };
        let in_root_time = in_root
            .parent()
            .filter(|home| !is_foreign_or_link(home))
            .and_then(|_| modified(&in_root));
        let (saved, other, saved_time, other_time) = if stores.default_refused {
            (&in_root, &default, in_root_time, modified(&default))
        } else {
            (&default, &in_root, modified(&default), in_root_time)
        };
        let path = match (saved_time, other_time) {
            (_, None) => saved,
            (None, Some(_)) => other,
            (Some(saved_time), Some(other_time)) if other_time > saved_time => other,
            _ => saved,
        };
        let content = std::fs::read_to_string(path).ok()?;
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

    /// Write the local record of `owner/name`: at the `D-025` path under
    /// the default root, or as `local-record.json` in the content root's
    /// in-root home where the default root refuses writes (SPEC u298,
    /// `Manifest::save`).
    pub fn save(
        &self,
        stores: &StoreRoots,
        owner: &str,
        name: &str,
        root: &Path,
    ) -> Result<(), CliError> {
        let (default, in_root) = record_places(stores, owner, name, root);
        let path = if stores.default_refused {
            if let Some(home) = in_root.parent() {
                ensure_in_root_home(home)?;
            }
            in_root
        } else {
            default
        };
        // A local record carries no recorded time (SPEC u291).
        let record = Manifest {
            commit_sha: self.commit_sha.clone(),
            files: self.files.clone(),
            recorded_at: None,
        };
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|err| CliError::Io {
                message: format!("could not create manifest directory: {err}"),
            })?;
        }
        let json = serde_json::to_string_pretty(&record).map_err(|e| CliError::Io {
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

    /// When a working copy recorded this base (SPEC u291), none where the
    /// record carries no time.
    pub fn recorded_at(&self) -> Option<u64> {
        self.recorded_at
    }

    pub fn set_recorded_at(&mut self, recorded_at: Option<u64>) {
        self.recorded_at = recorded_at;
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
        manifest
            .save(
                &crate::config::StoreRoots::resolve(Some(dir.path()), dir.path(), dir.path()),
                "bart",
                "my-project",
                dir.path(),
            )
            .unwrap();
        let loaded = Manifest::load(
            &crate::config::StoreRoots::resolve(Some(dir.path()), dir.path(), dir.path()),
            "bart",
            "my-project",
            dir.path(),
        )
        .unwrap();
        assert_eq!(loaded.commit_sha(), Some("abc123"));
        assert_eq!(loaded.file_sha("src/main.rs"), Some("deadbeef"));
        assert_eq!(loaded.file_sha("README.md"), Some("cafebabe"));
    }

    // SPEC u291, `Manifest.recorded_at`: a record carrying a time
    // round-trips it, one written before u291 loads none, and a local
    // record is saved carrying none.
    #[test]
    fn recorded_at_round_trips_and_no_local_record_carries_one() {
        let mut manifest = Manifest::default();
        manifest.update(
            "h1".to_string(),
            HashMap::from([("a.md".to_string(), "1".to_string())]),
        );
        manifest.set_recorded_at(Some(7));
        let text = serde_json::to_string(&manifest).unwrap();
        let read: Manifest = serde_json::from_str(&text).unwrap();
        assert_eq!(read.recorded_at(), Some(7));

        let older: Manifest =
            serde_json::from_str(r#"{"commit_sha":"h1","files":{"a.md":"1"}}"#).unwrap();
        assert_eq!(older.recorded_at(), None);

        let dir = tempfile::tempdir().unwrap();
        manifest
            .save(
                &crate::config::StoreRoots::resolve(Some(dir.path()), dir.path(), dir.path()),
                "alice",
                "work",
                dir.path(),
            )
            .unwrap();
        let saved = std::fs::read_to_string(dir.path().join("alice/work.json")).unwrap();
        assert!(!saved.contains("recorded_at"), "{saved}");
        assert_eq!(
            Manifest::load(
                &crate::config::StoreRoots::resolve(Some(dir.path()), dir.path(), dir.path()),
                "alice",
                "work",
                dir.path()
            )
            .unwrap()
            .recorded_at(),
            None
        );
    }

    #[test]
    fn load_returns_none_for_nonexistent_file() {
        let dir = tempfile::tempdir().unwrap();
        assert!(
            Manifest::load(
                &crate::config::StoreRoots::resolve(Some(dir.path()), dir.path(), dir.path()),
                "bart",
                "no-such-repo",
                dir.path()
            )
            .is_none()
        );
    }

    #[test]
    fn save_creates_parent_directories() {
        let dir = tempfile::tempdir().unwrap();
        let mut manifest = Manifest::default();
        manifest.update(
            "sha1".to_string(),
            HashMap::from([("a.txt".to_string(), "aaa".to_string())]),
        );
        manifest
            .save(
                &crate::config::StoreRoots::resolve(Some(dir.path()), dir.path(), dir.path()),
                "alice",
                "new-repo",
                dir.path(),
            )
            .unwrap();
        assert!(dir.path().join("alice").join("new-repo.json").exists());
        let loaded = Manifest::load(
            &crate::config::StoreRoots::resolve(Some(dir.path()), dir.path(), dir.path()),
            "alice",
            "new-repo",
            dir.path(),
        )
        .unwrap();
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
        assert!(
            Manifest::load(
                &crate::config::StoreRoots::resolve(Some(dir.path()), dir.path(), dir.path()),
                "alice",
                "repo",
                dir.path()
            )
            .is_none()
        );
    }

    #[test]
    fn load_rejects_stub_with_missing_commit_sha() {
        let dir = tempfile::tempdir().unwrap();
        let manifest_path = dir.path().join("alice").join("repo.json");
        std::fs::create_dir_all(manifest_path.parent().unwrap()).unwrap();
        std::fs::write(&manifest_path, r#"{"files":{"a.txt":"abc"}}"#).unwrap();
        assert!(
            Manifest::load(
                &crate::config::StoreRoots::resolve(Some(dir.path()), dir.path(), dir.path()),
                "alice",
                "repo",
                dir.path()
            )
            .is_none()
        );
    }

    #[test]
    fn load_rejects_manifest_with_empty_files() {
        let dir = tempfile::tempdir().unwrap();
        let manifest_path = dir.path().join("alice").join("repo.json");
        std::fs::create_dir_all(manifest_path.parent().unwrap()).unwrap();
        std::fs::write(&manifest_path, r#"{"commit_sha":"deadbeef","files":{}}"#).unwrap();
        assert!(
            Manifest::load(
                &crate::config::StoreRoots::resolve(Some(dir.path()), dir.path(), dir.path()),
                "alice",
                "repo",
                dir.path()
            )
            .is_none()
        );
    }

    // SPEC u298 Tests, `local_record_answers_the_later_record`: of the
    // record under the default root and the one in the in-root home, the
    // one modified last answers, a tie going to the one `save` writes.
    #[test]
    fn local_record_answers_the_later_record() {
        let scratch = tempfile::tempdir().unwrap();
        let default = scratch.path().join("default");
        let root = scratch.path().join("folder");
        std::fs::create_dir_all(&root).unwrap();
        let writable = StoreRoots {
            default: default.clone(),
            write: default.clone(),
            default_refused: false,
        };
        let refused = StoreRoots {
            default: default.clone(),
            write: scratch.path().join("fallback"),
            default_refused: true,
        };
        let record = |commit: &str| {
            let mut manifest = Manifest::default();
            manifest.update(
                commit.to_string(),
                HashMap::from([("a.md".to_string(), commit.to_string())]),
            );
            manifest
        };
        record("h1")
            .save(&writable, "alice", "work", &root)
            .unwrap();
        record("h2").save(&refused, "alice", "work", &root).unwrap();
        let in_root = root.join(IN_ROOT_HOME).join(LOCAL_RECORD_FILE);
        assert!(in_root.is_file());
        assert!(default.join("alice/work.json").is_file());

        let at = |path: &Path, secs: u64| {
            std::fs::File::options()
                .write(true)
                .open(path)
                .unwrap()
                .set_modified(std::time::UNIX_EPOCH + std::time::Duration::from_secs(secs))
                .unwrap();
        };
        let commit = |stores: &StoreRoots| {
            Manifest::load(stores, "alice", "work", &root)
                .and_then(|m| m.commit_sha().map(String::from))
        };
        at(&default.join("alice/work.json"), 1_000);
        at(&in_root, 2_000);
        assert_eq!(commit(&writable).as_deref(), Some("h2"));
        assert_eq!(commit(&refused).as_deref(), Some("h2"));
        at(&default.join("alice/work.json"), 3_000);
        assert_eq!(commit(&writable).as_deref(), Some("h1"));
        assert_eq!(commit(&refused).as_deref(), Some("h1"));
        at(&in_root, 3_000);
        assert_eq!(commit(&writable).as_deref(), Some("h1"));
        assert_eq!(commit(&refused).as_deref(), Some("h2"));
    }
}
