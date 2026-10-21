//! The tree closure (`EP-v2-tree`, `format=lines`) read and checked whole
//! before any of it is used (roadmap 279, D-236 §3, D-243 §6).
//!
//! Each tree is a line `tree\t<path>\t<id>` (`\tderived` appended where its
//! id was composed under a mask), followed by one line per entry
//! `<name>\t<mode>\t<sha>\t<size>`. A name holding a byte below `0x20`,
//! `0x7f`, `"`, `\` or bytes that are not UTF-8 is written in double
//! quotes. A closure is used only where every block rehashes to its id as
//! git's tree object, the first block is the scope's root, and every
//! `040000` entry is answered by a block at that path with that id.

use std::collections::{BTreeMap, HashMap};

use sha1::{Digest, Sha1};

use crate::read::store::is_blob_hash;
use crate::read::view::{Entry, MODE_SUBMODULE};

/// One entry line of a block.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Line {
    pub name: Vec<u8>,
    pub mode: String,
    pub sha: String,
    pub size: Option<u64>,
}

/// One tree of the closure: its path (decoded), its id, and its entries in
/// git's order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Block {
    pub path: Vec<u8>,
    pub id: String,
    pub derived: bool,
    pub lines: Vec<Line>,
}

/// Why a closure was not taken.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Refused {
    /// A line that does not parse.
    Unparsed(String),
    /// A block whose lines hash to another id than its own.
    Rehash { path: String, id: String },
    /// No block, or a first block that is not the scope's root.
    NoRoot,
    /// A `040000` entry with no block at its path and id, a block named
    /// by no entry, or a path listed twice.
    Unanswered(String),
}

impl std::fmt::Display for Refused {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Refused::Unparsed(line) => write!(f, "a closure line does not parse: {line}"),
            Refused::Rehash { path, id } => {
                write!(f, "the closure's tree at '{path}' does not hash to {id}")
            }
            Refused::NoRoot => write!(f, "the closure names no root tree"),
            Refused::Unanswered(path) => write!(f, "the closure holds no tree for '{path}'"),
        }
    }
}

/// A closure that checked whole.
#[derive(Debug, Clone)]
pub struct Closure {
    pub blocks: Vec<Block>,
}

/// Decodes one field as D-236 §3 writes it.
pub fn unquote(field: &[u8]) -> Option<Vec<u8>> {
    if field.first() != Some(&b'"') {
        return Some(field.to_vec());
    }
    if field.len() < 2 || field.last() != Some(&b'"') {
        return None;
    }
    let inner = &field[1..field.len() - 1];
    let mut out = Vec::with_capacity(inner.len());
    let mut i = 0;
    while i < inner.len() {
        let b = inner[i];
        if b != b'\\' {
            out.push(b);
            i += 1;
            continue;
        }
        match inner.get(i + 1)? {
            b't' => {
                out.push(b'\t');
                i += 2;
            }
            b'n' => {
                out.push(b'\n');
                i += 2;
            }
            b'"' => {
                out.push(b'"');
                i += 2;
            }
            b'\\' => {
                out.push(b'\\');
                i += 2;
            }
            d0 @ b'0'..=b'3' => {
                let d1 = *inner.get(i + 2)?;
                let d2 = *inner.get(i + 3)?;
                if !(b'0'..=b'7').contains(&d1) || !(b'0'..=b'7').contains(&d2) {
                    return None;
                }
                out.push((d0 - b'0') * 64 + (d1 - b'0') * 8 + (d2 - b'0'));
                i += 4;
            }
            _ => return None,
        }
    }
    Some(out)
}

/// The mode as git writes it in a tree object.
fn git_mode(mode: &str) -> Option<&'static [u8]> {
    Some(match mode {
        "100644" => b"100644",
        "100755" => b"100755",
        "120000" => b"120000",
        "040000" => b"40000",
        "160000" => b"160000",
        _ => return None,
    })
}

fn hex_to_bytes(hex: &str) -> Option<[u8; 20]> {
    let mut out = [0u8; 20];
    for (i, pair) in hex.as_bytes().chunks(2).enumerate() {
        let text = std::str::from_utf8(pair).ok()?;
        *out.get_mut(i)? = u8::from_str_radix(text, 16).ok()?;
    }
    Some(out)
}

/// The git id of the tree object `lines` make, in the order given.
pub fn tree_id(lines: &[Line]) -> Option<String> {
    let mut body = Vec::new();
    for line in lines {
        body.extend_from_slice(git_mode(&line.mode)?);
        body.push(b' ');
        body.extend_from_slice(&line.name);
        body.push(0);
        body.extend_from_slice(&hex_to_bytes(&line.sha)?);
    }
    let mut hasher = Sha1::new();
    hasher.update(format!("tree {}\0", body.len()).as_bytes());
    hasher.update(&body);
    Some(
        hasher
            .finalize()
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect(),
    )
}

fn parse_entry(fields: &[&[u8]]) -> Option<Line> {
    let [name, mode, sha, size] = fields else {
        return None;
    };
    let mode = std::str::from_utf8(mode).ok()?.to_string();
    git_mode(&mode)?;
    let sha = std::str::from_utf8(sha).ok()?.to_string();
    if !is_blob_hash(&sha) {
        return None;
    }
    let size = match *size {
        b"-" => None,
        digits => Some(std::str::from_utf8(digits).ok()?.parse().ok()?),
    };
    Some(Line {
        name: unquote(name)?,
        mode,
        sha,
        size,
    })
}

/// Splits the answer into blocks, every line ending in `\n`.
fn blocks_of(body: &[u8]) -> Result<Vec<Block>, Refused> {
    let mut blocks: Vec<Block> = Vec::new();
    if body.is_empty() {
        return Err(Refused::NoRoot);
    }
    if body.last() != Some(&b'\n') {
        return Err(Refused::Unparsed("the answer ends inside a line".into()));
    }
    for raw in body[..body.len() - 1].split(|b| *b == b'\n') {
        let fields: Vec<&[u8]> = raw.split(|b| *b == b'\t').collect();
        let unparsed = || Refused::Unparsed(String::from_utf8_lossy(raw).into_owned());
        let tree_line = fields.first() == Some(&b"tree".as_slice())
            && (fields.len() == 3 || fields.len() == 4 && fields[3] == b"derived");
        if tree_line {
            let id = std::str::from_utf8(fields[2]).map_err(|_| unparsed())?;
            if !is_blob_hash(id) {
                return Err(unparsed());
            }
            blocks.push(Block {
                path: unquote(fields[1]).ok_or_else(unparsed)?,
                id: id.to_string(),
                derived: fields.len() == 4,
                lines: Vec::new(),
            });
            continue;
        }
        let line = parse_entry(&fields).ok_or_else(unparsed)?;
        blocks.last_mut().ok_or(Refused::NoRoot)?.lines.push(line);
    }
    Ok(blocks)
}

fn joined(parent: &[u8], name: &[u8]) -> Vec<u8> {
    if parent.is_empty() {
        return name.to_vec();
    }
    let mut out = parent.to_vec();
    out.push(b'/');
    out.extend_from_slice(name);
    out
}

impl Closure {
    /// The closure `body` holds, checked whole against `root_path`, the
    /// path its first block must name (D-243 §6).
    pub fn parse(body: &[u8], root_path: &str) -> Result<Closure, Refused> {
        let blocks = blocks_of(body)?;
        let first = blocks.first().ok_or(Refused::NoRoot)?;
        if first.path != root_path.as_bytes() {
            return Err(Refused::NoRoot);
        }
        let mut by_path: HashMap<&[u8], &Block> = HashMap::new();
        for block in &blocks {
            if tree_id(&block.lines).as_deref() != Some(block.id.as_str()) {
                return Err(Refused::Rehash {
                    path: String::from_utf8_lossy(&block.path).into_owned(),
                    id: block.id.clone(),
                });
            }
            if by_path.insert(&block.path, block).is_some() {
                return Err(Refused::Unanswered(
                    String::from_utf8_lossy(&block.path).into_owned(),
                ));
            }
        }
        let mut named = 1usize;
        for block in &blocks {
            for line in block.lines.iter().filter(|l| l.mode == "040000") {
                let path = joined(&block.path, &line.name);
                match by_path.get(path.as_slice()) {
                    Some(sub) if sub.id == line.sha => named += 1,
                    _ => {
                        return Err(Refused::Unanswered(
                            String::from_utf8_lossy(&path).into_owned(),
                        ));
                    }
                }
            }
        }
        if named != blocks.len() {
            return Err(Refused::Unanswered("a tree no entry names".into()));
        }
        Ok(Closure { blocks })
    }

    /// The root tree's id.
    pub fn root(&self) -> &str {
        &self.blocks[0].id
    }

    /// Every non-tree entry, by path counted from the scope's root; a
    /// name whose bytes are not UTF-8 is left out (D-243 §6).
    pub fn entries(&self) -> BTreeMap<String, Entry> {
        let root = self.blocks[0].path.len();
        let mut out = BTreeMap::new();
        for block in &self.blocks {
            let below = if block.path.len() > root {
                block.path[root + usize::from(root > 0)..].to_vec()
            } else {
                Vec::new()
            };
            for line in block.lines.iter().filter(|l| l.mode != "040000") {
                let Ok(path) = String::from_utf8(joined(&below, &line.name)) else {
                    continue;
                };
                out.insert(
                    path,
                    Entry {
                        mode: line.mode.clone(),
                        sha: line.sha.clone(),
                        size: if line.mode == MODE_SUBMODULE {
                            None
                        } else {
                            line.size
                        },
                    },
                );
            }
        }
        out
    }

    /// Each block's lines as `trees/` holds them, by id.
    pub fn stored_blocks(&self) -> Vec<(String, Vec<u8>)> {
        self.blocks
            .iter()
            .map(|block| (block.id.clone(), encode_lines(&block.lines)))
            .collect()
    }
}

/// Quotes a name as D-236 §3 writes it.
fn quote(name: &[u8]) -> Vec<u8> {
    let needs = name
        .iter()
        .any(|b| *b < 0x20 || *b == 0x7f || *b == b'"' || *b == b'\\')
        || std::str::from_utf8(name).is_err();
    if !needs {
        return name.to_vec();
    }
    let mut out = vec![b'"'];
    for chunk in name.utf8_chunks() {
        for c in chunk.valid().chars() {
            match c {
                '\t' => out.extend_from_slice(b"\\t"),
                '\n' => out.extend_from_slice(b"\\n"),
                '"' => out.extend_from_slice(b"\\\""),
                '\\' => out.extend_from_slice(b"\\\\"),
                c if (c as u32) < 0x20 || c as u32 == 0x7f => {
                    out.extend_from_slice(format!("\\{:03o}", c as u32).as_bytes())
                }
                c => {
                    let mut buf = [0u8; 4];
                    out.extend_from_slice(c.encode_utf8(&mut buf).as_bytes());
                }
            }
        }
        for byte in chunk.invalid() {
            out.extend_from_slice(format!("\\{byte:03o}").as_bytes());
        }
    }
    out.push(b'"');
    out
}

/// A block's entry lines as the closure writes them.
pub fn encode_lines(lines: &[Line]) -> Vec<u8> {
    let mut out = Vec::new();
    for line in lines {
        out.extend_from_slice(&quote(&line.name));
        out.push(b'\t');
        out.extend_from_slice(line.mode.as_bytes());
        out.push(b'\t');
        out.extend_from_slice(line.sha.as_bytes());
        out.push(b'\t');
        match line.size {
            Some(size) => out.extend_from_slice(size.to_string().as_bytes()),
            None => out.push(b'-'),
        }
        out.push(b'\n');
    }
    out
}

/// The entry lines of one stored tree, checked against `id`; none where
/// they do not parse or hash to another id.
pub fn decode_lines(bytes: &[u8], id: &str) -> Option<Vec<Line>> {
    if !bytes.is_empty() && bytes.last() != Some(&b'\n') {
        return None;
    }
    let mut lines = Vec::new();
    if !bytes.is_empty() {
        for raw in bytes[..bytes.len() - 1].split(|b| *b == b'\n') {
            let fields: Vec<&[u8]> = raw.split(|b| *b == b'\t').collect();
            lines.push(parse_entry(&fields)?);
        }
    }
    (tree_id(&lines).as_deref() == Some(id)).then_some(lines)
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::push::hash::blob_sha1;

    /// A closure of `files` (path, bytes) under `prefix`, as the engine
    /// writes it with no mask.
    pub(crate) fn closure_of(files: &[(&str, &[u8])], prefix: &str) -> Vec<u8> {
        #[derive(Default)]
        struct Dir {
            files: BTreeMap<Vec<u8>, (String, String, u64)>,
            dirs: BTreeMap<Vec<u8>, Dir>,
        }
        let mut root = Dir::default();
        for (path, bytes) in files {
            let mut dir = &mut root;
            let parts: Vec<&str> = path.split('/').collect();
            for part in &parts[..parts.len() - 1] {
                dir = dir.dirs.entry(part.as_bytes().to_vec()).or_default();
            }
            dir.files.insert(
                parts[parts.len() - 1].as_bytes().to_vec(),
                ("100644".into(), blob_sha1(bytes), bytes.len() as u64),
            );
        }
        fn emit(dir: &Dir, path: &[u8], out: &mut Vec<u8>) -> String {
            let mut subs = Vec::new();
            let mut lines = Vec::new();
            let mut names: Vec<(Vec<u8>, bool)> = dir
                .files
                .keys()
                .map(|k| (k.clone(), false))
                .chain(dir.dirs.keys().map(|k| (k.clone(), true)))
                .collect();
            names.sort_by(|a, b| {
                let key = |(n, d): &(Vec<u8>, bool)| {
                    let mut k = n.clone();
                    if *d {
                        k.push(b'/');
                    }
                    k
                };
                key(a).cmp(&key(b))
            });
            for (name, is_dir) in names {
                if is_dir {
                    let mut sub_out = Vec::new();
                    let child = joined(path, &name);
                    let id = emit(&dir.dirs[&name], &child, &mut sub_out);
                    lines.push(Line {
                        name,
                        mode: "040000".into(),
                        sha: id,
                        size: None,
                    });
                    subs.push(sub_out);
                } else {
                    let (mode, sha, size) = dir.files[&name].clone();
                    lines.push(Line {
                        name,
                        mode,
                        sha,
                        size: Some(size),
                    });
                }
            }
            let id = tree_id(&lines).unwrap();
            out.extend_from_slice(b"tree\t");
            out.extend_from_slice(&quote(path));
            out.push(b'\t');
            out.extend_from_slice(id.as_bytes());
            out.push(b'\n');
            out.extend_from_slice(&encode_lines(&lines));
            for sub in subs {
                out.extend_from_slice(&sub);
            }
            id
        }
        let mut out = Vec::new();
        emit(&root, prefix.as_bytes(), &mut out);
        out
    }

    #[test]
    fn quoted_names_decode_to_their_bytes() {
        for name in [
            b"plain".to_vec(),
            b"tab\there".to_vec(),
            b"new\nline".to_vec(),
            b"\"q\"".to_vec(),
            b"back\\slash".to_vec(),
            vec![0x01, b'x'],
            vec![b'a', 0xff, b'b'],
        ] {
            assert_eq!(unquote(&quote(&name)).unwrap(), name);
        }
        assert_eq!(unquote(b"\"abc"), None);
        assert_eq!(unquote(b"\"\\q\""), None);
    }

    // The ids `git` itself wrote for this tree (a file, an executable, a
    // link and a folder two deep).
    #[test]
    fn a_tree_hashes_to_the_id_git_writes() {
        let line = |name: &str, mode: &str, sha: &str| Line {
            name: name.as_bytes().to_vec(),
            mode: mode.to_string(),
            sha: sha.to_string(),
            size: None,
        };
        let e = vec![line("f.md", "100644", &blob_sha1(b"f\n"))];
        assert_eq!(
            tree_id(&e).unwrap(),
            "95d18f78ae81d1821522161d2f89711cbac76706"
        );
        let d = vec![line(
            "e",
            "040000",
            "95d18f78ae81d1821522161d2f89711cbac76706",
        )];
        assert_eq!(
            tree_id(&d).unwrap(),
            "28aae5e52c583847b3b345a70df19129113c0341"
        );
        let root = vec![
            line("a.md", "100644", "78981922613b2afb6025042ff6bd878ac1994e85"),
            line("d", "040000", "28aae5e52c583847b3b345a70df19129113c0341"),
            line("link", "120000", "e77de8562fa61286381cc40e34a75024c20e17eb"),
            line(
                "run.sh",
                "100755",
                "1a2485251c33a70432394c93fb89330ef214bfc9",
            ),
        ];
        assert_eq!(
            tree_id(&root).unwrap(),
            "b3d8916a62aa17a334c881ae36494f6526a8f16c"
        );
    }

    #[test]
    fn the_empty_tree_hashes_to_git_s_id() {
        assert_eq!(
            tree_id(&[]).unwrap(),
            "4b825dc642cb6eb9a060e54bf8d69288fbee4904"
        );
    }

    #[test]
    fn a_closure_checks_and_lists_its_entries() {
        let body = closure_of(
            &[("a.md", b"a\n"), ("d/e/f.md", b"f\n"), ("tree", b"t")],
            "",
        );
        let closure = Closure::parse(&body, "").unwrap();
        let entries = closure.entries();
        assert_eq!(
            entries.keys().cloned().collect::<Vec<_>>(),
            vec!["a.md", "d/e/f.md", "tree"]
        );
        assert_eq!(entries["d/e/f.md"].sha, blob_sha1(b"f\n"));
        for (id, lines) in closure.stored_blocks() {
            assert!(decode_lines(&lines, &id).is_some());
        }
    }

    #[test]
    fn a_closure_under_a_path_counts_from_it() {
        let body = closure_of(&[("x.md", b"x"), ("y/z.md", b"z")], "plan");
        let closure = Closure::parse(&body, "plan").unwrap();
        assert_eq!(
            closure.entries().keys().cloned().collect::<Vec<_>>(),
            vec!["x.md", "y/z.md"]
        );
        assert_eq!(Closure::parse(&body, "").unwrap_err(), Refused::NoRoot);
    }

    // Row 15.
    #[test]
    fn a_closure_failing_a_check_is_refused() {
        let body = closure_of(&[("a.md", b"a\n"), ("d/e/f.md", b"f\n")], "");
        // A block whose lines hash otherwise.
        let altered = String::from_utf8(body.clone())
            .unwrap()
            .replace(&blob_sha1(b"a\n"), &blob_sha1(b"b\n"));
        assert!(matches!(
            Closure::parse(altered.as_bytes(), ""),
            Err(Refused::Rehash { .. })
        ));
        // Cut inside its last block: the last block is missing.
        let text = String::from_utf8(body.clone()).unwrap();
        let cut = text.rfind("tree\t").unwrap();
        assert!(matches!(
            Closure::parse(&body[..cut], ""),
            Err(Refused::Unanswered(_))
        ));
        // Cut inside a line.
        assert!(matches!(
            Closure::parse(&body[..body.len() - 3], ""),
            Err(Refused::Unparsed(_))
        ));
        // No root block.
        let first_end = text.find('\n').unwrap() + 1;
        assert!(Closure::parse(&body[first_end..], "").is_err());
        assert!(matches!(Closure::parse(b"", ""), Err(Refused::NoRoot)));
    }

    #[test]
    fn a_four_field_line_is_a_tree_only_where_it_ends_derived() {
        // An entry named `tree` reads as an entry.
        let body = closure_of(&[("tree", b"x"), ("derived/a", b"y")], "");
        let closure = Closure::parse(&body, "").unwrap();
        assert!(closure.entries().contains_key("tree"));
        assert!(closure.entries().contains_key("derived/a"));
    }
}
