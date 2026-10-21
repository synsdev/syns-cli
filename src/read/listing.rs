//! `syns ls` and `syns glob` from the closure at the resolved commit, a
//! `304` rendered from `trees/` (roadmap 279, D-243 §9). The entries are
//! v1's: `{name, path, type, size, sha}`, a folder with no size and no
//! hash, links and submodules left out, ordered by path.

use crate::client::{EntryType, SynsClient, TreeEntry, TreeResponse};
use crate::errors::CliError;
use crate::read::closure::{Line, decode_lines};
use crate::read::store::{Store, StoreSpot};
use crate::read::v2::{TreeAnswer, closure, is_absent};
use crate::read::view::{Ignore, View, tag_parts};

fn joined(parent: &str, name: &str) -> String {
    if parent.is_empty() {
        name.to_string()
    } else {
        format!("{parent}/{name}")
    }
}

/// The tree's entries as v1 lists them: one level, or every file beneath
/// where `recursive` holds. `lines_of` gives a tree's lines by id.
fn listed(
    root_lines: Vec<Line>,
    base: &str,
    recursive: bool,
    lines_of: &mut dyn FnMut(&str) -> Option<Vec<Line>>,
) -> Option<Vec<TreeEntry>> {
    let mut out = Vec::new();
    let mut pending = vec![(base.to_string(), root_lines)];
    while let Some((path, lines)) = pending.pop() {
        for line in lines {
            let Ok(name) = String::from_utf8(line.name.clone()) else {
                continue;
            };
            let full = joined(&path, &name);
            match line.mode.as_str() {
                "100644" | "100755" => out.push(TreeEntry {
                    name,
                    path: full,
                    entry_type: EntryType::File,
                    size: line.size,
                    sha: Some(line.sha.clone()),
                }),
                "040000" if recursive => pending.push((full, lines_of(&line.sha)?)),
                "040000" => out.push(TreeEntry {
                    name,
                    path: full,
                    entry_type: EntryType::Dir,
                    size: None,
                    sha: None,
                }),
                _ => {}
            }
        }
    }
    out.sort_by(|a, b| a.path.cmp(&b.path));
    Some(out)
}

fn as_value(entries: &[TreeEntry], commit: &str) -> serde_json::Value {
    let entries: Vec<serde_json::Value> = entries
        .iter()
        .map(|e| {
            serde_json::json!({
                "name": e.name,
                "path": e.path,
                "type": match e.entry_type {
                    EntryType::File => "file",
                    EntryType::Dir => "dir",
                    EntryType::Unknown => "unknown",
                },
                "size": e.size,
                "sha": e.sha,
            })
        })
        .collect();
    serde_json::json!({"entries": entries, "commitSha": commit, "truncated": false})
}

/// The listing of `path` at `commit` of `address` through the closure,
/// with `If-None-Match` set to the held view's tag and a `304` read from
/// `trees/`; none where the server answers no v2 (D-243 §7, §9).
#[allow(clippy::too_many_arguments)]
pub async fn list(
    client: &SynsClient,
    spot: &StoreSpot,
    address: &str,
    token: Option<&str>,
    commit: &str,
    path: Option<&str>,
    recursive: bool,
) -> Result<Option<(TreeResponse, serde_json::Value)>, CliError> {
    // A typed path that names nothing once trimmed (`''`, `/`, `' '`) keeps
    // today's v1 read, so its refusal is the base binary's (VERIFY D1);
    // only a path naming something goes to v2.
    if path.is_some_and(|typed| crate::read::as_server_reads(typed).is_empty()) {
        return Ok(None);
    }
    // The path as the server reads it, so the first block names it.
    let path = crate::read::as_server_reads(path.unwrap_or(""));
    let held_store = spot.open(false);
    let held = held_store
        .as_ref()
        .and_then(|store| View::load(store, address, path, commit))
        .filter(|view| view.root.is_some());
    // A `304` is read from the stored trees; one missing a tree asks again.
    for inm in [held.as_ref().map(|v| v.tag.as_str()), None] {
        let answer = match closure(client, None, address, token, commit, path, path, inm).await {
            Ok(answer) => answer,
            Err(err) if is_absent(&err) => return Ok(None),
            Err(err) => return Err(err),
        };
        let entries = match answer {
            TreeAnswer::NotModified => {
                let (Some(store), Some(view)) = (&held_store, &held) else {
                    continue;
                };
                match from_trees(store, view, path, recursive) {
                    Some(entries) => entries,
                    None => continue,
                }
            }
            TreeAnswer::Closure { tag, closure } => {
                let mut by_id = std::collections::HashMap::new();
                for block in &closure.blocks {
                    by_id.insert(block.id.clone(), block.lines.clone());
                }
                let root_lines = closure.blocks[0].lines.clone();
                let entries = listed(root_lines, path, recursive, &mut |id| {
                    by_id.get(id).cloned()
                })
                .unwrap_or_default();
                if let (Some(store), Some((tagged, digest))) = (spot.open(true), tag_parts(&tag))
                    && tagged == commit
                {
                    for (id, lines) in closure.stored_blocks() {
                        store.admit_tree(&id, &lines);
                    }
                    // A view a pull or sync wrote stands: it keeps its
                    // `.synsignore` record and its closure time, and gains
                    // only the root a listing renders from (REVIEW-syns-cli
                    // minor 2).
                    let standing = View::load(&store, address, path, commit);
                    if standing.as_ref().is_none_or(|v| v.root.is_none()) {
                        View {
                            address: address.to_string(),
                            scope: path.to_string(),
                            commit: commit.to_string(),
                            root: Some(closure.root().to_string()),
                            tag,
                            closure_time: standing
                                .as_ref()
                                .map(|v| v.closure_time)
                                .unwrap_or_else(crate::read::run::now_secs),
                            masked: digest != "-",
                            ignore: standing.map(|v| v.ignore).unwrap_or(Ignore::Unknown),
                            entries: closure.entries(),
                        }
                        .save(&store);
                    }
                }
                entries
            }
        };
        let value = as_value(&entries, commit);
        return Ok(Some((
            TreeResponse {
                entries,
                commit_sha: commit.to_string(),
                truncated: false,
            },
            value,
        )));
    }
    Ok(None)
}

/// The listing from the trees a held view's root reaches, none where one
/// is missing or fails its check.
fn from_trees(store: &Store, view: &View, path: &str, recursive: bool) -> Option<Vec<TreeEntry>> {
    let mut lines_of = |id: &str| {
        let bytes = store.tree(id)?;
        let lines = decode_lines(&bytes, id);
        if lines.is_none() {
            store.remove_tree(id);
        }
        lines
    };
    let root = lines_of(view.root.as_deref()?)?;
    listed(root, path, recursive, &mut lines_of)
}
