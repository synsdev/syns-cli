//! A view: the server's entry list for one address, one scope and one
//! commit, as this account read it (roadmap 279, D-243 §1, §4, §10).
//!
//! The file carries a header naming its format version, address, scope,
//! commit, the scope's root tree, the closure's tag, the time of the
//! closure it descends from, whether it was read under a mask, the
//! holder's root `.synsignore` record, the entry count and the SHA-256 of
//! its entry lines. A view whose header does not match where it stands,
//! or whose entries do not match their count or digest, reads as none.

use std::collections::BTreeMap;

use sha2::Digest;

use crate::read::store::{Store, VIEW_FORMAT, is_blob_hash};

/// The mode of an ordinary file.
pub const MODE_FILE: &str = "100644";
/// The mode of an executable file.
pub const MODE_EXECUTABLE: &str = "100755";
/// The mode of a symbolic link.
pub const MODE_LINK: &str = "120000";
/// The mode of a submodule.
pub const MODE_SUBMODULE: &str = "160000";

/// One non-tree entry of a view: its mode, blob hash and size.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    pub mode: String,
    pub sha: String,
    pub size: Option<u64>,
}

impl Entry {
    /// Whether the entry is a file a checkout writes: modes `100644` and
    /// `100755` alone (D-243 §2).
    pub fn is_file(&self) -> bool {
        self.mode == MODE_FILE || self.mode == MODE_EXECUTABLE
    }
}

/// What a view records of the holder's root `.synsignore` (D-243 §10).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Ignore {
    /// Its blob hash at the commit.
    Hash(String),
    /// None stands at the commit, or the holder answered it as absent.
    None,
    /// Not known: no answer this build could take it from.
    Unknown,
}

impl Ignore {
    fn field(&self) -> String {
        match self {
            Ignore::Hash(sha) => sha.clone(),
            Ignore::None => "none".to_string(),
            Ignore::Unknown => "unknown".to_string(),
        }
    }

    fn parse(field: &str) -> Option<Ignore> {
        match field {
            "none" => Some(Ignore::None),
            "unknown" => Some(Ignore::Unknown),
            sha if is_blob_hash(sha) => Some(Ignore::Hash(sha.to_string())),
            _ => None,
        }
    }
}

/// One view.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct View {
    pub address: String,
    /// The scope path as the request sends it, `""` for the root.
    pub scope: String,
    pub commit: String,
    /// The scope's root tree id, none for a view derived by `changes`.
    pub root: Option<String>,
    /// The closure's tag as answered, or computed (D-243 §1).
    pub tag: String,
    /// Seconds since the epoch of the closure this view descends from.
    pub closure_time: u64,
    /// Read under a mask: its tag's digest is not `-` (D-243 §5).
    pub masked: bool,
    pub ignore: Ignore,
    /// Every non-tree entry, by path counted from the scope.
    pub entries: BTreeMap<String, Entry>,
}

/// The lowercase hex SHA-256 of `scope`, the scope's key in `views/`.
pub fn scope_key(scope: &str) -> String {
    hex(&sha2::Sha256::digest(scope.as_bytes()))
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// `encodeURIComponent` of `value`, as the server writes a tag's path.
pub fn encode_uri_component(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for b in value.bytes() {
        if b.is_ascii_alphanumeric() || b"-_.!~*'()".contains(&b) {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

/// The tag of an unmasked closure at `commit` and `scope`, matched weakly
/// by the server (D-243 §1, SPEC_REVIEW m9).
pub fn unmasked_tag(commit: &str, scope: &str) -> String {
    format!("W/\"{commit}:{}:closure:-\"", encode_uri_component(scope))
}

/// A tag's commit and digest: `W/` and the quotes taken off, then the
/// first and the last `:`-separated fields.
pub fn tag_parts(tag: &str) -> Option<(String, String)> {
    let bare = tag.trim().trim_start_matches("W/").trim_matches('"');
    let mut fields = bare.split(':');
    let commit = fields.next()?.to_string();
    let digest = fields.next_back()?.to_string();
    (is_blob_hash(&commit) && !digest.is_empty()).then_some((commit, digest))
}

fn json_string(value: &str) -> String {
    serde_json::Value::from(value).to_string()
}

fn parse_json_string(field: &str) -> Option<String> {
    match serde_json::from_str::<serde_json::Value>(field).ok()? {
        serde_json::Value::String(s) => Some(s),
        _ => None,
    }
}

impl View {
    /// The entry lines whose SHA-256 the header carries.
    fn entry_lines(&self) -> Vec<u8> {
        let mut out = Vec::new();
        for (path, entry) in &self.entries {
            let size = entry
                .size
                .map(|s| s.to_string())
                .unwrap_or_else(|| "-".to_string());
            out.extend_from_slice(
                format!(
                    "{} {} {size} {}\n",
                    entry.mode,
                    entry.sha,
                    json_string(path)
                )
                .as_bytes(),
            );
        }
        out
    }

    /// The view as its file holds it.
    pub fn to_bytes(&self) -> Vec<u8> {
        let lines = self.entry_lines();
        let mut out = format!(
            "syns-view {VIEW_FORMAT}\naddress {}\nscope {}\ncommit {}\nroot {}\ntag {}\nclosure {}\nmasked {}\nsynsignore {}\nentries {}\ndigest {}\n--\n",
            self.address,
            json_string(&self.scope),
            self.commit,
            self.root.as_deref().unwrap_or("-"),
            json_string(&self.tag),
            self.closure_time,
            u8::from(self.masked),
            self.ignore.field(),
            self.entries.len(),
            hex(&sha2::Sha256::digest(&lines)),
        )
        .into_bytes();
        out.extend_from_slice(&lines);
        out
    }

    /// The view the file `bytes` holds, none where its header names
    /// another format, address, scope or commit than asked, or its entries
    /// fail their count or digest (D-243 §1).
    pub fn parse(bytes: &[u8], address: &str, scope: &str, commit: &str) -> Option<View> {
        let text = std::str::from_utf8(bytes).ok()?;
        let (head, body) = text.split_once("\n--\n")?;
        let mut fields: BTreeMap<&str, &str> = BTreeMap::new();
        for line in head.lines() {
            let (key, value) = line.split_once(' ')?;
            fields.insert(key, value);
        }
        if fields.get("syns-view") != Some(&VIEW_FORMAT)
            || fields.get("address") != Some(&address)
            || fields.get("commit") != Some(&commit)
            || parse_json_string(fields.get("scope")?)? != scope
        {
            return None;
        }
        let digest = hex(&sha2::Sha256::digest(body.as_bytes()));
        if fields.get("digest") != Some(&digest.as_str()) {
            return None;
        }
        let count: usize = fields.get("entries")?.parse().ok()?;
        let mut entries = BTreeMap::new();
        for line in body.lines() {
            let mut parts = line.splitn(4, ' ');
            let (mode, sha, size, path) =
                (parts.next()?, parts.next()?, parts.next()?, parts.next()?);
            if !is_blob_hash(sha) {
                return None;
            }
            let size = match size {
                "-" => None,
                digits => Some(digits.parse().ok()?),
            };
            entries.insert(
                parse_json_string(path)?,
                Entry {
                    mode: mode.to_string(),
                    sha: sha.to_string(),
                    size,
                },
            );
        }
        if entries.len() != count {
            return None;
        }
        let root = match *fields.get("root")? {
            "-" => None,
            id if is_blob_hash(id) => Some(id.to_string()),
            _ => return None,
        };
        Some(View {
            address: address.to_string(),
            scope: scope.to_string(),
            commit: commit.to_string(),
            root,
            tag: parse_json_string(fields.get("tag")?)?,
            closure_time: fields.get("closure")?.parse().ok()?,
            masked: *fields.get("masked")? == "1",
            ignore: Ignore::parse(fields.get("synsignore")?)?,
            entries,
        })
    }

    /// The view of `commit` the store holds for `address` and `scope`,
    /// checked; one failing its check is removed and reads as none.
    pub fn load(store: &Store, address: &str, scope: &str, commit: &str) -> Option<View> {
        let key = scope_key(scope);
        let bytes = store.view(address, &key, commit)?;
        match View::parse(&bytes, address, scope, commit) {
            Some(view) => Some(view),
            None => {
                store.remove_view(address, &key, commit);
                None
            }
        }
    }

    /// Writes the view into the store, whole.
    pub fn save(&self, store: &Store) {
        store.write_view(
            &self.address,
            &scope_key(&self.scope),
            &self.commit,
            &self.to_bytes(),
        );
    }

    /// Removes this view's file.
    pub fn drop_from(&self, store: &Store) {
        store.remove_view(&self.address, &scope_key(&self.scope), &self.commit);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> View {
        let mut entries = BTreeMap::new();
        entries.insert(
            "a b/\"c\".md".to_string(),
            Entry {
                mode: MODE_FILE.into(),
                sha: "1".repeat(40),
                size: Some(3),
            },
        );
        entries.insert(
            "link".to_string(),
            Entry {
                mode: MODE_LINK.into(),
                sha: "2".repeat(40),
                size: Some(4),
            },
        );
        entries.insert(
            "sub".to_string(),
            Entry {
                mode: MODE_SUBMODULE.into(),
                sha: "3".repeat(40),
                size: None,
            },
        );
        View {
            address: "alice/notes".into(),
            scope: "plan".into(),
            commit: "a".repeat(40),
            root: Some("b".repeat(40)),
            tag: unmasked_tag(&"a".repeat(40), "plan"),
            closure_time: 1_700_000_000,
            masked: false,
            ignore: Ignore::None,
            entries,
        }
    }

    #[test]
    fn a_view_reads_back_as_written() {
        let view = sample();
        let bytes = view.to_bytes();
        assert_eq!(
            View::parse(&bytes, "alice/notes", "plan", &"a".repeat(40)),
            Some(view)
        );
    }

    // Row 15a: a view truncated, altered, of another commit or another
    // address reads as none.
    #[test]
    fn a_view_failing_its_check_reads_as_none() {
        let view = sample();
        let bytes = view.to_bytes();
        let commit = "a".repeat(40);
        let half = &bytes[..bytes.len() / 2];
        assert_eq!(View::parse(half, "alice/notes", "plan", &commit), None);
        let mut altered = bytes.clone();
        let at = altered.len() - 3;
        altered[at] ^= 1;
        assert_eq!(View::parse(&altered, "alice/notes", "plan", &commit), None);
        assert_eq!(
            View::parse(&bytes, "alice/notes", "plan", &"c".repeat(40)),
            None
        );
        assert_eq!(View::parse(&bytes, "alice/other", "plan", &commit), None);
        assert_eq!(View::parse(&bytes, "alice/notes", "", &commit), None);
        let other_format =
            String::from_utf8(bytes.clone())
                .unwrap()
                .replacen("syns-view 1", "syns-view 2", 1);
        assert_eq!(
            View::parse(other_format.as_bytes(), "alice/notes", "plan", &commit),
            None
        );
    }

    #[test]
    fn the_scope_key_is_the_sha256_of_the_scope() {
        assert_eq!(
            scope_key(""),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
        assert!(!scope_key("../x").contains('/'));
    }

    #[test]
    fn tags_are_written_and_read_as_the_server_writes_them() {
        assert_eq!(encode_uri_component("caf\u{e9}"), "caf%C3%A9");
        assert_eq!(encode_uri_component("a:b/c d"), "a%3Ab%2Fc%20d");
        let commit = "a".repeat(40);
        assert_eq!(
            unmasked_tag(&commit, "plan"),
            format!("W/\"{commit}:plan:closure:-\"")
        );
        assert_eq!(
            tag_parts(&format!("W/\"{commit}:a%3Ab:closure:ff00\"")),
            Some((commit.clone(), "ff00".to_string()))
        );
        assert_eq!(
            tag_parts(&format!("\"{commit}::closure:-\"")),
            Some((commit, "-".to_string()))
        );
    }
}
