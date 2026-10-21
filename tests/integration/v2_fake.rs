//! Roadmap 279: one scriptable server answering v1 and v2 from one model
//! of commits, as roadmap 276 answers them (FEATURE_SPEC.md § Expected).
//!
//! The model is a list of commits, each a map of path to entry (a file,
//! an executable, a link or a submodule), and a mask of hidden folders
//! every reader reads under. v1 answers the record, the version list, the
//! recursive tree, the raw entry, the file entry and the push; v2 answers
//! `head`, the closure (real git tree ids, `derived` under the mask, a tag
//! and `304`), `changes` and the batch frames. With `v2` off every
//! `/api/v2/…` answers `404` with the body `absent` names. Each route can
//! be made to close unanswered, cut its answer, refuse with
//! `Retry-After`, hold, or answer `too_large`; every request is logged
//! with its headers, body, status and timing.

#![allow(dead_code)]

use std::collections::{BTreeMap, BTreeSet, HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::process::Output;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use base64::Engine as _;
use serde_json::{Value, json};
use sha1::{Digest, Sha1};
use tempfile::TempDir;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

pub const OWNER: &str = "alice";
pub const NAME: &str = "notes";
pub const REPO: &str = "alice/notes";
pub const IDENTITY: &str = "owner: alice\nname: notes\n";

pub fn blob(bytes: &[u8]) -> String {
    let mut h = Sha1::new();
    h.update(format!("blob {}\0", bytes.len()).as_bytes());
    h.update(bytes);
    hex(&h.finalize())
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

// ---- the model ---------------------------------------------------------------

/// One entry of a commit: a file's bytes and mode; a submodule's commit
/// hash as its bytes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Entry {
    pub bytes: Vec<u8>,
    pub mode: &'static str,
}

impl Entry {
    pub fn file(bytes: &[u8]) -> Entry {
        Entry {
            bytes: bytes.to_vec(),
            mode: "100644",
        }
    }
    pub fn sha(&self) -> String {
        if self.mode == "160000" {
            String::from_utf8(self.bytes.clone()).unwrap()
        } else {
            blob(&self.bytes)
        }
    }
    fn is_file(&self) -> bool {
        self.mode == "100644" || self.mode == "100755"
    }
}

pub type Files = BTreeMap<String, Entry>;

#[derive(Clone, Debug)]
pub struct Commit {
    pub sha: String,
    pub files: Files,
}

/// What a route is answered with in place of its answer, once.
#[derive(Clone, Debug)]
pub enum Act {
    /// Answered as it stands: an act queued only to reach a later request.
    Pass,
    /// The connection closed with no answer.
    Close,
    /// A refusal with its status, wire error and `Retry-After`.
    Refuse {
        status: u16,
        error: &'static str,
        retry_after: Option<u32>,
    },
    /// The batch: the header and this many frames, then the connection
    /// closed.
    CutAfterFrames(usize),
    /// The first this many bytes of the body, then closed.
    CutAfterBytes(usize),
    /// The whole answer held this long first.
    Hold(Duration),
    /// The batch: these paths answered `too_large`.
    TooLarge(Vec<String>),
    /// The batch: this path's bytes altered after its frame line.
    Corrupt(String),
    /// The closure: one entry's hash altered.
    CorruptClosure,
    /// The batch: endless bytes in place of the first file of 1,000
    /// bytes or more.
    Endless,
    /// The batch: this path answered `unchanged` naming this other hash.
    UnchangedOther(String, String),
    /// The batch: this path's frame line names no `mode`.
    NoMode(String),
}

/// What `/api/v2/…` answers where v2 is off.
#[derive(Clone, Copy, Debug)]
pub enum Absent {
    Json,
    Empty,
    Html,
}

/// One request the fake read.
#[derive(Clone, Debug)]
pub struct Seen {
    pub method: String,
    /// `v1 tree`, `v1 raw`, `v2 read`, `v2 tree`, …
    pub route: String,
    /// The path after the route, decoded (raw, files, tree/<path>).
    pub path: String,
    pub query: BTreeMap<String, String>,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
    pub status: u16,
    pub start: Instant,
    pub end: Option<Instant>,
}

impl Seen {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k == name)
            .map(|(_, v)| v.as_str())
    }
    pub fn json(&self) -> Value {
        serde_json::from_slice(&self.body).unwrap()
    }
    /// The paths a batch read asked.
    pub fn asked(&self) -> Vec<String> {
        self.json()["paths"]
            .as_array()
            .unwrap()
            .iter()
            .map(|p| p["path"].as_str().unwrap().to_string())
            .collect()
    }
    pub fn is_v2(&self) -> bool {
        self.route.starts_with("v2 ")
    }
}

#[derive(Default)]
pub struct State {
    pub commits: Vec<Commit>,
    pub v2: bool,
    pub absent: Option<Absent>,
    /// Hidden folders: every reader reads under this mask.
    pub hidden: Vec<String>,
    /// The record marks a shared folder identity.
    pub shared_folder: bool,
    /// Every `/api/v2/…` answered `200` with the record, as a fallback
    /// route would.
    pub v2_record: bool,
    /// The v2 answers that were no `404` and no closed connection, never
    /// cleared: an equality guard's v2 run must have used v2.
    pub v2_answered: usize,
    /// Set by `Fake::v2(true)`: the fake, dropped, refuses a run that
    /// never read v2 (REVIEW-syns-cli minor 4).
    pub expect_v2: bool,
    /// A shared folder's identity: its name and the folder it serves.
    pub identity: Option<(String, String)>,
    /// The folder the request in hand reads, at the identity's address.
    scope: Option<String>,
    pub acts: HashMap<String, VecDeque<Act>>,
    pub log: Vec<Seen>,
    /// Every batch answer waits this long.
    pub hold_read: Duration,
    pub in_flight: HashMap<String, usize>,
    pub max_in_flight: HashMap<String, usize>,
    /// The batch's per-file and per-answer limits (8 MiB and 32 MiB).
    pub file_limit: u64,
    pub answer_limit: u64,
}

fn visible(hidden: &[String], path: &str) -> bool {
    !hidden
        .iter()
        .any(|h| path == h || path.starts_with(&format!("{h}/")))
}

/// A git tree of `files` (path → entry), its blocks emitted as the engine
/// writes the closure under `prefix`, with derived ids where `masked`
/// removed entries.
#[derive(Default, Debug)]
struct Dir {
    files: BTreeMap<String, Entry>,
    dirs: BTreeMap<String, Dir>,
}

fn git_mode(mode: &str) -> &str {
    if mode == "040000" { "40000" } else { mode }
}

fn tree_id(lines: &[(String, String, String)]) -> String {
    let mut body = Vec::new();
    for (name, mode, sha) in lines {
        body.extend_from_slice(git_mode(mode).as_bytes());
        body.push(b' ');
        body.extend_from_slice(name.as_bytes());
        body.push(0);
        for pair in sha.as_bytes().chunks(2) {
            body.push(u8::from_str_radix(std::str::from_utf8(pair).unwrap(), 16).unwrap());
        }
    }
    let mut h = Sha1::new();
    h.update(format!("tree {}\0", body.len()).as_bytes());
    h.update(&body);
    hex(&h.finalize())
}

fn quote(name: &str) -> String {
    let needs = name
        .bytes()
        .any(|b| b < 0x20 || b == 0x7f || b == b'"' || b == b'\\');
    if !needs {
        return name.to_string();
    }
    let mut out = String::from("\"");
    for c in name.chars() {
        match c {
            '\t' => out.push_str("\\t"),
            '\n' => out.push_str("\\n"),
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            c if (c as u32) < 0x20 || c as u32 == 0x7f => {
                out.push_str(&format!("\\{:03o}", c as u32))
            }
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

impl Dir {
    fn of(files: &Files, keep: &dyn Fn(&str) -> bool) -> Dir {
        let mut root = Dir::default();
        for (path, entry) in files {
            if !keep(path) {
                continue;
            }
            let parts: Vec<&str> = path.split('/').collect();
            let mut dir = &mut root;
            for part in &parts[..parts.len() - 1] {
                dir = dir.dirs.entry(part.to_string()).or_default();
            }
            dir.files
                .insert(parts[parts.len() - 1].to_string(), entry.clone());
        }
        root
    }

    fn at(&self, path: &str) -> Option<&Dir> {
        if path.is_empty() {
            return Some(self);
        }
        let mut dir = self;
        for part in path.split('/') {
            dir = dir.dirs.get(part)?;
        }
        Some(dir)
    }

    fn ordered(&self) -> Vec<(String, bool)> {
        let mut names: Vec<(String, bool)> = self
            .files
            .keys()
            .map(|k| (k.clone(), false))
            .chain(self.dirs.keys().map(|k| (k.clone(), true)))
            .collect();
        names.sort_by(|a, b| {
            let key = |(n, d): &(String, bool)| {
                let mut k = n.clone().into_bytes();
                if *d {
                    k.push(b'/');
                }
                k
            };
            key(a).cmp(&key(b))
        });
        names
    }

    /// The tree's id.
    fn id(&self) -> String {
        let lines: Vec<(String, String, String)> = self
            .ordered()
            .into_iter()
            .map(|(name, is_dir)| {
                if is_dir {
                    (name.clone(), "040000".to_string(), self.dirs[&name].id())
                } else {
                    let e = &self.files[&name];
                    (name, e.mode.to_string(), e.sha())
                }
            })
            .collect();
        tree_id(&lines)
    }

    fn emit(&self, path: &str, whole: Option<&Dir>, out: &mut String) {
        let id = self.id();
        out.push_str(&format!("tree\t{}\t{id}", quote(path)));
        if whole.is_some_and(|w| w.id() != id) {
            out.push_str("\tderived");
        }
        out.push('\n');
        let mut subs = Vec::new();
        for (name, is_dir) in self.ordered() {
            if is_dir {
                let sub = &self.dirs[&name];
                out.push_str(&format!("{}\t040000\t{}\t-\n", quote(&name), sub.id()));
                let child = if path.is_empty() {
                    name.clone()
                } else {
                    format!("{path}/{name}")
                };
                subs.push((child, sub, whole.and_then(|w| w.dirs.get(&name))));
            } else {
                let e = &self.files[&name];
                let size = if e.mode == "160000" {
                    "-".to_string()
                } else {
                    e.bytes.len().to_string()
                };
                out.push_str(&format!(
                    "{}\t{}\t{}\t{size}\n",
                    quote(&name),
                    e.mode,
                    e.sha()
                ));
            }
        }
        for (child, sub, w) in subs {
            sub.emit(&child, w, out);
        }
    }
}

impl State {
    /// The folder's files at commit `i`, counted from it.
    fn folder_files(&self, i: usize, folder: &str) -> Files {
        self.commits[i]
            .files
            .iter()
            .filter_map(|(p, e)| {
                p.strip_prefix(&format!("{folder}/"))
                    .map(|rest| (rest.to_string(), e.clone()))
            })
            .collect()
    }

    /// The commits the address in hand lists: every one at the holder,
    /// those changing the folder at an identity's (D-239 §1).
    fn listed(&self) -> Vec<usize> {
        match &self.scope {
            None => (0..self.commits.len()).collect(),
            Some(folder) => (0..self.commits.len())
                .filter(|i| {
                    let now = self.folder_files(*i, folder);
                    let before = match i {
                        0 => Files::new(),
                        i => self.folder_files(i - 1, folder),
                    };
                    !now.is_empty() && now != before
                })
                .collect(),
        }
    }

    fn head(&self) -> Option<(usize, &Commit)> {
        self.listed().last().map(|i| (*i, &self.commits[*i]))
    }

    fn holder_head(&self) -> Option<(usize, &Commit)> {
        self.commits
            .len()
            .checked_sub(1)
            .map(|i| (i, &self.commits[i]))
    }

    /// `at` (`head`, a number, a full hash or a hash prefix of 7 or more
    /// for v1) as an index.
    fn resolve(&self, at: Option<&str>, prefixes: bool) -> Result<Option<usize>, u16> {
        match at {
            None | Some("head") => Ok(self.head().map(|(i, _)| i)),
            Some(n) if n.bytes().all(|b| b.is_ascii_digit()) => {
                let n: usize = n.parse().map_err(|_| 404u16)?;
                if n >= 1 && n <= self.commits.len() && self.listed().contains(&(n - 1)) {
                    Ok(Some(n - 1))
                } else {
                    Err(404)
                }
            }
            Some(sha) => self
                .commits
                .iter()
                .position(|c| c.sha == sha || prefixes && sha.len() >= 7 && c.sha.starts_with(sha))
                .filter(|i| self.listed().contains(i))
                .map(Some)
                .ok_or(404),
        }
    }

    fn visible_files(&self, i: usize) -> Files {
        let all: Files = self.commits[i]
            .files
            .iter()
            .filter(|(p, _)| visible(&self.hidden, p))
            .map(|(p, e)| (p.clone(), e.clone()))
            .collect();
        match &self.scope {
            None => all,
            Some(folder) => all
                .into_iter()
                .filter_map(|(p, e)| {
                    p.strip_prefix(&format!("{folder}/"))
                        .map(|rest| (rest.to_string(), e))
                })
                .collect(),
        }
    }

    fn digest(&self) -> String {
        if self.hidden.is_empty() {
            "-".to_string()
        } else {
            hex(&<sha2::Sha256 as sha2::Digest>::digest(
                self.hidden.join(",").as_bytes(),
            ))
        }
    }

    pub fn add(&mut self, files: Files) -> String {
        let dir = Dir::of(&files, &|_| true);
        let sha = blob(format!("commit {} {}", self.commits.len(), dir.id()).as_bytes());
        self.commits.push(Commit {
            sha: sha.clone(),
            files,
        });
        sha
    }

    fn take_act(&mut self, route: &str) -> Option<Act> {
        self.acts.get_mut(route).and_then(VecDeque::pop_front)
    }
}

// ---- answers ------------------------------------------------------------------

pub enum Reply {
    Whole {
        status: u16,
        headers: Vec<(String, String)>,
        body: Vec<u8>,
    },
    /// The head and these bytes, then the connection closed.
    Cut {
        status: u16,
        headers: Vec<(String, String)>,
        body: Vec<u8>,
    },
    Endless {
        headers: Vec<(String, String)>,
        body: Vec<u8>,
    },
    Close,
}

fn json_reply(status: u16, body: Value) -> Reply {
    Reply::Whole {
        status,
        headers: vec![("content-type".into(), "application/json".into())],
        body: body.to_string().into_bytes(),
    }
}

fn error(status: u16, code: &str) -> Reply {
    json_reply(status, json!({"error": code, "message": code}))
}

fn version_entry(version: usize, sha: &str) -> Value {
    json!({
        "version": version, "sha": sha, "parentSha": null, "message": "m",
        "messageBody": null, "author": "alice",
        "createdAt": "2026-01-01T00:00:00Z", "filesChanged": [],
    })
}

fn tag_matches(inm: &str, tag: &str) -> bool {
    let bare = |t: &str| {
        t.trim()
            .trim_start_matches("W/")
            .trim_matches('"')
            .to_string()
    };
    inm.split(',')
        .any(|t| t.trim() == "*" || bare(t) == bare(tag))
}

fn encode_uri_component(value: &str) -> String {
    let mut out = String::new();
    for b in value.bytes() {
        if b.is_ascii_alphanumeric() || b"-_.!~*'()".contains(&b) {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

fn apply_act(reply: Reply, act: Option<&Act>) -> Reply {
    match (act, reply) {
        (
            Some(Act::CutAfterBytes(n)),
            Reply::Whole {
                status,
                headers,
                body,
            },
        ) => Reply::Cut {
            status,
            headers,
            body: body[..(*n).min(body.len())].to_vec(),
        },
        (_, reply) => reply,
    }
}

impl State {
    #[allow(clippy::too_many_arguments)]
    fn route(
        &mut self,
        method: &str,
        path: &str,
        query: &BTreeMap<String, String>,
        headers: &[(String, String)],
        body: &[u8],
        route: &mut String,
        tail: &mut String,
    ) -> Reply {
        let header = |name: &str| {
            headers
                .iter()
                .find(|(k, _)| k == name)
                .map(|(_, v)| v.clone())
        };
        // The holder's address, or the identity's, its paths counted from
        // the folder.
        self.scope = None;
        let mut path = path.to_string();
        if let Some((name, folder)) = self.identity.clone() {
            for v in ["v1", "v2"] {
                let at = format!("/api/{v}/repos/{OWNER}/{name}");
                if path == at || path.starts_with(&format!("{at}/")) {
                    path = format!("/api/{v}/repos/{REPO}{}", &path[at.len()..]);
                    self.scope = Some(folder.clone());
                }
            }
        }
        let path = path.as_str();
        if let Some(rest) = path.strip_prefix(&format!("/api/v2/repos/{REPO}")) {
            *route = format!("v2 {}", rest.trim_start_matches('/'));
            if self.scope.is_some() {
                route.push_str(" identity");
            }
            if self.v2_record {
                return self.record();
            }
            if !self.v2 {
                return match self.absent.unwrap_or(Absent::Json) {
                    Absent::Json => error(404, "not_found"),
                    Absent::Empty => Reply::Whole {
                        status: 404,
                        headers: vec![],
                        body: vec![],
                    },
                    Absent::Html => Reply::Whole {
                        status: 404,
                        headers: vec![("content-type".into(), "text/html".into())],
                        body: b"<html><body>Not Found</body></html>".to_vec(),
                    },
                };
            }
            // An act aimed at one chunk: keyed by the first path it asks.
            let first = route
                .starts_with("v2 read")
                .then(|| serde_json::from_slice::<Value>(body).ok())
                .flatten()
                .and_then(|b| b["paths"][0]["path"].as_str().map(str::to_string));
            let aimed = first.and_then(|p| self.take_act(&format!("v2 read {p}")));
            let act = aimed.or_else(|| self.take_act(route));
            match &act {
                Some(Act::Close) => return Reply::Close,
                Some(Act::Refuse {
                    status,
                    error: code,
                    retry_after,
                }) => {
                    let mut reply = error(*status, code);
                    if let (Reply::Whole { headers, .. }, Some(wait)) = (&mut reply, retry_after) {
                        headers.push(("retry-after".into(), wait.to_string()));
                    }
                    return reply;
                }
                _ => {}
            }
            let kind = route.trim_end_matches(" identity").to_string();
            let reply = match (method, kind.as_str()) {
                ("GET", "v2 head") => self.v2_head(header("if-none-match")),
                ("GET", "v2 tree") => self.v2_tree(query, header("if-none-match"), act.as_ref()),
                ("GET", "v2 changes") => self.v2_changes(query),
                ("POST", "v2 read") => self.v2_read(body, act.as_ref()),
                _ => error(404, "not_found"),
            };
            return apply_act(reply, act.as_ref());
        }
        let Some(rest) = path.strip_prefix(&format!("/api/v1/repos/{REPO}")) else {
            *route = "other".into();
            return error(404, "not_found");
        };
        let rest = rest.trim_start_matches('/');
        let (kind, after) = rest.split_once('/').unwrap_or((rest, ""));
        *route = format!("v1 {kind}");
        if self.scope.is_some() {
            route.push_str(" identity");
        }
        *tail = after.to_string();
        let act = self.take_act(route);
        match &act {
            Some(Act::Close) => return Reply::Close,
            Some(Act::Refuse {
                status,
                error: code,
                retry_after,
            }) => {
                let mut reply = error(*status, code);
                if let (Reply::Whole { headers, .. }, Some(wait)) = (&mut reply, retry_after) {
                    headers.push(("retry-after".into(), wait.to_string()));
                }
                return reply;
            }
            _ => {}
        }
        let at = query.get("ref").map(String::as_str);
        let reply = match (method, kind) {
            ("GET", "") => self.record(),
            ("GET", "versions") if after.is_empty() => {
                let data: Vec<Value> = self
                    .head()
                    .map(|(i, c)| version_entry(i + 1, &c.sha))
                    .into_iter()
                    .collect();
                json_reply(
                    200,
                    json!({"data": data, "total": data.len(), "limit": 1, "offset": 0}),
                )
            }
            ("GET", "versions") => match self.resolve(Some(after), true) {
                Ok(Some(i)) => json_reply(200, version_entry(i + 1, &self.commits[i].sha)),
                _ => error(404, "not_found"),
            },
            // A path segment that names nothing (`tree/`, `tree//`) is no
            // path the server finds, and one blank once trimmed fails its
            // schema (`filePathSchema`), as the server answers them.
            ("GET", "tree") if rest.contains('/') && after.trim_matches('/').is_empty() => {
                error(404, "not_found")
            }
            ("GET", "tree") if rest.contains('/') && after.trim().is_empty() => {
                error(422, "validation_error")
            }
            ("GET", "tree") => self.v1_tree(
                after.trim_end_matches('/').trim(),
                at,
                query.get("recursive").map(String::as_str) == Some("true"),
            ),
            ("GET", "raw") => self.v1_raw(after, at, header("if-none-match")),
            ("GET", "files") => self.v1_file(after, at),
            ("PUT", "push") => self.push(body),
            _ => error(404, "not_found"),
        };
        apply_act(reply, act.as_ref())
    }

    fn record(&self) -> Reply {
        let name = match (&self.scope, &self.identity) {
            (Some(_), Some((name, _))) => name.clone(),
            _ => NAME.to_string(),
        };
        json_reply(
            200,
            json!({
                "owner": OWNER, "name": name, "description": null,
                "commitSha": self.head().map(|(_, c)| c.sha.clone()),
                "status": "active", "author": null, "tags": [],
                "visibility": "public", "forkedFrom": null, "forkCount": 0,
                "fileCount": 0, "role": "owner", "sharedFolder": self.shared_folder || self.scope.is_some(),
                "createdAt": "2026-01-01T00:00:00Z", "updatedAt": "2026-01-01T00:00:00Z",
            }),
        )
    }

    fn v1_tree(&self, path: &str, at: Option<&str>, recursive: bool) -> Reply {
        if self.commits.is_empty() {
            return error(422, "validation_error");
        }
        let Ok(Some(i)) = self.resolve(at, true) else {
            return error(404, "not_found");
        };
        let files = self.visible_files(i);
        let dir = Dir::of(&files, &|_| true);
        let Some(at_dir) = dir.at(path) else {
            return error(404, "not_found");
        };
        let mut entries: Vec<(String, Value)> = Vec::new();
        if recursive {
            for (p, e) in &files {
                if e.is_file() && (path.is_empty() || p.starts_with(&format!("{path}/"))) {
                    entries.push((
                        p.clone(),
                        json!({
                            "name": p.rsplit('/').next().unwrap(), "path": p, "type": "file",
                            "size": e.bytes.len(), "sha": e.sha(),
                        }),
                    ));
                }
            }
        } else {
            let join = |name: &str| {
                if path.is_empty() {
                    name.to_string()
                } else {
                    format!("{path}/{name}")
                }
            };
            for (name, e) in &at_dir.files {
                if e.is_file() {
                    let p = join(name);
                    entries.push((
                        p.clone(),
                        json!({
                            "name": name, "path": p, "type": "file",
                            "size": e.bytes.len(), "sha": e.sha(),
                        }),
                    ));
                }
            }
            for name in at_dir.dirs.keys() {
                let p = join(name);
                entries.push((
                    p.clone(),
                    json!({
                        "name": name, "path": p, "type": "dir", "size": null, "sha": null,
                    }),
                ));
            }
        }
        entries.sort_by(|a, b| a.0.cmp(&b.0));
        let entries: Vec<Value> = entries.into_iter().map(|(_, v)| v).collect();
        json_reply(
            200,
            json!({"entries": entries, "commitSha": self.commits[i].sha, "truncated": false}),
        )
    }

    fn v1_raw(&self, path: &str, at: Option<&str>, inm: Option<String>) -> Reply {
        if self.commits.is_empty() {
            return error(422, "validation_error");
        }
        let Ok(Some(i)) = self.resolve(at, true) else {
            return error(404, "not_found");
        };
        let files = self.visible_files(i);
        match files.get(path).filter(|e| e.mode != "160000") {
            Some(e) => {
                let sha = blob(&e.bytes);
                if inm.as_deref() == Some(format!("\"{sha}\"").as_str()) {
                    return Reply::Whole {
                        status: 304,
                        headers: vec![("etag".into(), format!("\"{sha}\""))],
                        body: vec![],
                    };
                }
                Reply::Whole {
                    status: 200,
                    headers: vec![
                        ("content-type".into(), "application/octet-stream".into()),
                        ("etag".into(), format!("\"{sha}\"")),
                    ],
                    body: e.bytes.clone(),
                }
            }
            None => error(404, "not_found"),
        }
    }

    fn v1_file(&self, path: &str, at: Option<&str>) -> Reply {
        let Ok(Some(i)) = self.resolve(at, true) else {
            return error(404, "not_found");
        };
        match self.visible_files(i).get(path) {
            Some(e) => {
                let text = !e.bytes.contains(&0) && std::str::from_utf8(&e.bytes).is_ok();
                json_reply(
                    200,
                    json!({
                        "path": path, "sha": blob(&e.bytes), "size": e.bytes.len(),
                        "content": if text { Value::from(String::from_utf8(e.bytes.clone()).unwrap()) } else { Value::Null },
                    }),
                )
            }
            None => error(404, "not_found"),
        }
    }

    fn push(&mut self, body: &[u8]) -> Reply {
        let body: Value = serde_json::from_slice(body).unwrap();
        let under = |path: &str| match &self.scope {
            Some(folder) => format!("{folder}/{path}"),
            None => path.to_string(),
        };
        let head = self.holder_head().map(|(_, c)| c.clone());
        if let Some(parent) = body.get("parentSha").and_then(Value::as_str)
            && head.as_ref().map(|c| c.sha.as_str()) != Some(parent)
        {
            let current = head.as_ref().map(|c| c.sha.clone()).unwrap_or_default();
            return json_reply(
                409,
                json!({"error": "conflict", "message": "Head mismatch", "currentSha": current}),
            );
        }
        let known: HashMap<String, Vec<u8>> = self
            .commits
            .iter()
            .flat_map(|c| c.files.values())
            .map(|e| (blob(&e.bytes), e.bytes.clone()))
            .collect();
        let mut files = head.map(|c| c.files).unwrap_or_default();
        for entry in body["files"].as_array().unwrap() {
            let path = under(entry["path"].as_str().unwrap());
            let bytes = match (
                entry.get("content").and_then(Value::as_str),
                entry.get("contentBase64").and_then(Value::as_str),
            ) {
                (Some(text), _) => text.as_bytes().to_vec(),
                (None, Some(encoded)) => base64::engine::general_purpose::STANDARD
                    .decode(encoded)
                    .unwrap(),
                (None, None) => match known.get(entry["sha"].as_str().unwrap()) {
                    Some(bytes) => bytes.clone(),
                    None => {
                        return json_reply(
                            409,
                            json!({"error": "missing_blobs", "message": "missing", "missing": {path: entry["sha"]}}),
                        );
                    }
                },
            };
            files.insert(path, Entry::file(&bytes));
        }
        for deletion in body
            .get("deletions")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            files.remove(&under(deletion["path"].as_str().unwrap()));
        }
        let sha = self.add(files);
        json_reply(
            200,
            json!({"commitSha": sha, "version": self.commits.len(), "filesChanged": 1, "created": self.commits.len() == 1}),
        )
    }

    fn v2_head(&self, inm: Option<String>) -> Reply {
        let Some((i, c)) = self.head() else {
            return json_reply(200, json!({"commit": null, "version": null}));
        };
        let etag = format!("\"{}\"", c.sha);
        if inm.as_deref().is_some_and(|inm| tag_matches(inm, &etag)) {
            return Reply::Whole {
                status: 304,
                headers: vec![("etag".into(), etag)],
                body: vec![],
            };
        }
        Reply::Whole {
            status: 200,
            headers: vec![
                ("content-type".into(), "application/json".into()),
                ("etag".into(), etag),
            ],
            body: json!({"commit": c.sha, "version": i + 1})
                .to_string()
                .into_bytes(),
        }
    }

    fn at_v2(&self, at: Option<&String>) -> Result<usize, Reply> {
        let Some(at) = at else {
            return Err(error(422, "validation_error"));
        };
        if self.commits.is_empty() {
            return Err(error(422, "validation_error"));
        }
        if at != "head" && !(at.len() == 40 || at.bytes().all(|b| b.is_ascii_digit())) {
            return Err(error(422, "validation_error"));
        }
        match self.resolve(Some(at), false) {
            Ok(Some(i)) => Ok(i),
            _ => Err(error(404, "not_found")),
        }
    }

    fn v2_tree(
        &self,
        query: &BTreeMap<String, String>,
        inm: Option<String>,
        act: Option<&Act>,
    ) -> Reply {
        let i = match self.at_v2(query.get("at")) {
            Ok(i) => i,
            Err(reply) => return reply,
        };
        // The path as 276 reads it (`queryPath`): every trailing `/` off,
        // then the field kind's trim.
        let path = query
            .get("path")
            .map(|p| p.trim_end_matches('/').trim().to_string())
            .unwrap_or_default();
        let commit = &self.commits[i];
        let tag = format!(
            "W/\"{}:{}:closure:{}\"",
            commit.sha,
            encode_uri_component(&path),
            self.digest()
        );
        if inm.as_deref().is_some_and(|inm| tag_matches(inm, &tag)) {
            return Reply::Whole {
                status: 304,
                headers: vec![("etag".into(), tag)],
                body: vec![],
            };
        }
        let kept = Dir::of(&self.visible_files(i), &|_| true);
        let whole = match &self.scope {
            None => Dir::of(&commit.files, &|_| true),
            Some(folder) => Dir::of(&self.folder_files(i, folder), &|_| true),
        };
        let hidden = self.hidden.clone();
        let (Some(dir), true) = (kept.at(&path), visible(&hidden, &path) || path.is_empty()) else {
            return error(404, "not_found");
        };
        let mut out = String::new();
        dir.emit(
            &path,
            (!self.hidden.is_empty()).then(|| whole.at(&path)).flatten(),
            &mut out,
        );
        if matches!(act, Some(Act::CorruptClosure)) {
            // One entry hash altered: its block no longer rehashes.
            if let Some(at) = out.find("\t100644\t") {
                let at = at + 8;
                let c = out.as_bytes()[at];
                let swapped = if c == b'0' { '1' } else { '0' };
                out.replace_range(at..at + 1, &swapped.to_string());
            }
        }
        Reply::Whole {
            status: 200,
            headers: vec![
                ("content-type".into(), "text/plain; charset=utf-8".into()),
                ("etag".into(), tag),
            ],
            body: out.into_bytes(),
        }
    }

    fn v2_changes(&self, query: &BTreeMap<String, String>) -> Reply {
        let from = match self.at_v2(query.get("from")) {
            Ok(i) => i,
            Err(reply) => return reply,
        };
        let to = match self.at_v2(query.get("to")) {
            Ok(i) => i,
            Err(reply) => return reply,
        };
        let a = self.visible_files(from);
        let b = self.visible_files(to);
        let mut added: Vec<(&String, &Entry)> =
            b.iter().filter(|(p, _)| !a.contains_key(*p)).collect();
        let deleted: Vec<(&String, &Entry)> =
            a.iter().filter(|(p, _)| !b.contains_key(*p)).collect();
        let mut entries: Vec<Value> = Vec::new();
        let mut paired: BTreeSet<String> = BTreeSet::new();
        for (old, e) in &deleted {
            if let Some(pos) = added.iter().position(|(_, n)| n.sha() == e.sha()) {
                let (new, n) = added.remove(pos);
                paired.insert((*old).clone());
                entries.push(json!({"path": new, "oldPath": old, "status": "renamed", "oldSha": e.sha(), "newSha": n.sha(), "size": n.bytes.len()}));
            }
        }
        for (old, e) in &deleted {
            if !paired.contains(*old) {
                entries.push(json!({"path": old, "status": "deleted", "oldSha": e.sha()}));
            }
        }
        for (new, n) in added {
            entries.push(
                json!({"path": new, "status": "added", "newSha": n.sha(), "size": n.bytes.len()}),
            );
        }
        for (p, e) in &b {
            if let Some(old) = a.get(p)
                && (old.sha() != e.sha() || old.mode != e.mode)
            {
                entries.push(json!({"path": p, "status": "modified", "oldSha": old.sha(), "newSha": e.sha(), "size": e.bytes.len()}));
            }
        }
        entries.sort_by(|x, y| x["path"].as_str().cmp(&y["path"].as_str()));
        json_reply(
            200,
            json!({
                "from": {"commit": self.commits[from].sha, "version": from + 1},
                "to": {"commit": self.commits[to].sha, "version": to + 1},
                "entries": entries,
            }),
        )
    }

    fn v2_read(&self, body: &[u8], act: Option<&Act>) -> Reply {
        let Ok(body) = serde_json::from_slice::<Value>(body) else {
            return error(422, "validation_error");
        };
        let at = match &body["at"] {
            Value::String(s) => s.clone(),
            Value::Number(n) => n.to_string(),
            _ => return error(422, "validation_error"),
        };
        let i = match self.at_v2(Some(&at)) {
            Ok(i) => i,
            Err(reply) => return reply,
        };
        let paths = body["paths"].as_array().unwrap();
        if paths.len() > 1000 {
            return json_reply(
                422,
                json!({"error": "too_many_files", "message": "too many", "count": paths.len(), "max": 1000}),
            );
        }
        let files = self.visible_files(i);
        let mut out = format!(
            "{}\n",
            json!({"commit": self.commits[i].sha, "version": i + 1})
        )
        .into_bytes();
        let mut carried = 0u64;
        let too_large: Vec<String> = match act {
            Some(Act::TooLarge(paths)) => paths.clone(),
            _ => Vec::new(),
        };
        for (frames, asked) in paths.iter().enumerate() {
            if let Some(Act::CutAfterFrames(n)) = act
                && frames == *n
            {
                return Reply::Cut {
                    status: 200,
                    headers: vec![("content-type".into(), "application/x-syns-frames".into())],
                    body: out,
                };
            }
            let path = asked["path"].as_str().unwrap();
            let have = asked.get("have").and_then(Value::as_str);
            let Some(e) = files.get(path).filter(|e| e.mode != "160000") else {
                out.extend_from_slice(
                    format!("{}\n", json!({"path": path, "status": "not_found"})).as_bytes(),
                );
                continue;
            };
            let sha = e.sha();
            let size = e.bytes.len() as u64;
            if let Some(Act::UnchangedOther(p, other)) = act
                && p == path
            {
                out.extend_from_slice(
                    format!(
                        "{}\n",
                        json!({"path": path, "sha": other, "size": size, "mode": e.mode, "status": "unchanged"})
                    )
                    .as_bytes(),
                );
                continue;
            }
            let status = if have == Some(sha.as_str()) {
                "unchanged"
            } else if size > self.file_limit
                || carried + size > self.answer_limit
                || too_large.iter().any(|p| p == path)
            {
                "too_large"
            } else {
                "ok"
            };
            let mut line =
                json!({"path": path, "sha": sha, "size": size, "mode": e.mode, "status": status});
            if matches!(act, Some(Act::NoMode(p)) if p == path) {
                line.as_object_mut().unwrap().remove("mode");
            }
            out.extend_from_slice(format!("{line}\n").as_bytes());
            if status == "ok" && matches!(act, Some(Act::Endless)) && size >= 1000 {
                // Endless bytes in place of this frame's.
                return Reply::Endless {
                    headers: vec![("content-type".into(), "application/x-syns-frames".into())],
                    body: out,
                };
            }
            if status == "ok" {
                let mut bytes = e.bytes.clone();
                if matches!(act, Some(Act::Corrupt(p)) if p == path)
                    && let Some(b) = bytes.first_mut()
                {
                    *b ^= 1;
                }
                out.extend_from_slice(&bytes);
                carried += size;
            }
        }
        Reply::Whole {
            status: 200,
            headers: vec![("content-type".into(), "application/x-syns-frames".into())],
            body: out,
        }
    }
}

// ---- the listener ---------------------------------------------------------------

struct Request {
    method: String,
    target: String,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
}

async fn read_request(sock: &mut TcpStream, buf: &mut Vec<u8>) -> Option<Request> {
    let mut chunk = [0u8; 65536];
    let end = loop {
        if let Some(pos) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
            break pos + 4;
        }
        let n = sock.read(&mut chunk).await.ok()?;
        if n == 0 {
            return None;
        }
        buf.extend_from_slice(&chunk[..n]);
    };
    let head = String::from_utf8_lossy(&buf[..end]).to_string();
    let mut lines = head.split("\r\n");
    let mut first = lines.next()?.split(' ');
    let method = first.next()?.to_string();
    let target = first.next()?.to_string();
    let headers: Vec<(String, String)> = lines
        .filter_map(|l| l.split_once(':'))
        .map(|(k, v)| (k.trim().to_ascii_lowercase(), v.trim().to_string()))
        .collect();
    let length = headers
        .iter()
        .find(|(k, _)| k == "content-length")
        .and_then(|(_, v)| v.parse::<usize>().ok())
        .unwrap_or(0);
    while buf.len() < end + length {
        let n = sock.read(&mut chunk).await.ok()?;
        if n == 0 {
            return None;
        }
        buf.extend_from_slice(&chunk[..n]);
    }
    let body = buf[end..end + length].to_vec();
    buf.drain(..end + length);
    Some(Request {
        method,
        target,
        headers,
        body,
    })
}

fn parse_query(query: &str) -> BTreeMap<String, String> {
    query
        .split('&')
        .filter_map(|kv| kv.split_once('='))
        .map(|(k, v)| {
            (
                k.to_string(),
                urlencoding::decode(&v.replace('+', " "))
                    .unwrap()
                    .into_owned(),
            )
        })
        .collect()
}

async fn serve(state: Arc<Mutex<State>>, mut sock: TcpStream) {
    let mut buf = Vec::new();
    while let Some(req) = read_request(&mut sock, &mut buf).await {
        let (route_path, query) = req.target.split_once('?').unwrap_or((&req.target, ""));
        let path = urlencoding::decode(route_path).unwrap().into_owned();
        let query = parse_query(query);
        let (reply, hold, index, route) = {
            let mut s = state.lock().unwrap();
            let mut route = String::new();
            let mut tail = String::new();
            let reply = s.route(
                &req.method,
                &path,
                &query,
                &req.headers,
                &req.body,
                &mut route,
                &mut tail,
            );
            let mut hold = Duration::ZERO;
            if route == "v2 read" {
                hold = s.hold_read;
            }
            if let Some(Act::Hold(d)) = s
                .acts
                .get(&format!("hold {route}"))
                .and_then(|q| q.front())
                .cloned()
            {
                s.acts
                    .get_mut(&format!("hold {route}"))
                    .unwrap()
                    .pop_front();
                hold += d;
            }
            *s.in_flight.entry(route.clone()).or_default() += 1;
            let now = s.in_flight[&route];
            let max = s.max_in_flight.entry(route.clone()).or_default();
            *max = (*max).max(now);
            let status = match &reply {
                Reply::Whole { status, .. } | Reply::Cut { status, .. } => *status,
                Reply::Endless { .. } => 200,
                Reply::Close => 0,
            };
            if route.starts_with("v2 ") && status != 404 && status != 0 {
                s.v2_answered += 1;
            }
            s.log.push(Seen {
                method: req.method.clone(),
                route: route.clone(),
                path: tail,
                query,
                headers: req.headers.clone(),
                body: req.body.clone(),
                status,
                start: Instant::now(),
                end: None,
            });
            (reply, hold, s.log.len() - 1, route)
        };
        if !hold.is_zero() {
            tokio::time::sleep(hold).await;
        }
        let head = |status: u16, headers: &[(String, String)], extra: &str| {
            let mut text = format!("HTTP/1.1 {status} X\r\n");
            for (k, v) in headers {
                text.push_str(&format!("{k}: {v}\r\n"));
            }
            text.push_str(extra);
            text.push_str("\r\n");
            text
        };
        let finish = |state: &Arc<Mutex<State>>| {
            let mut s = state.lock().unwrap();
            *s.in_flight.get_mut(&route).unwrap() -= 1;
            s.log[index].end = Some(Instant::now());
        };
        let keep = match reply {
            Reply::Close => {
                finish(&state);
                return;
            }
            Reply::Whole {
                status,
                headers,
                body,
            } => {
                let length = if status == 304 {
                    String::new()
                } else {
                    format!("content-length: {}\r\n", body.len())
                };
                let mut out = head(status, &headers, &length).into_bytes();
                out.extend_from_slice(&body);
                sock.write_all(&out).await.is_ok()
            }
            Reply::Cut {
                status,
                headers,
                body,
            } => {
                let length = format!("content-length: {}\r\n", body.len() + 4096);
                let mut out = head(status, &headers, &length).into_bytes();
                out.extend_from_slice(&body);
                let _ = sock.write_all(&out).await;
                let _ = sock.flush().await;
                finish(&state);
                return;
            }
            Reply::Endless { headers, body } => {
                let mut out = head(200, &headers, "content-length: 100000000\r\n").into_bytes();
                out.extend_from_slice(&body);
                if sock.write_all(&out).await.is_ok() {
                    let start = Instant::now();
                    while start.elapsed() < Duration::from_secs(30) {
                        if sock.write_all(&[b'x'; 4096]).await.is_err() {
                            break;
                        }
                    }
                }
                finish(&state);
                return;
            }
        };
        finish(&state);
        if !keep {
            return;
        }
    }
}

/// The fake deployment.
pub struct Fake {
    rt: tokio::runtime::Runtime,
    pub port: u16,
    pub uri: String,
    pub state: Arc<Mutex<State>>,
}

impl Drop for Fake {
    fn drop(&mut self) {
        let (expected, answered) = self.with(|s| (s.expect_v2, s.v2_answered));
        if expected && answered == 0 && !std::thread::panicking() {
            panic!("the v2 run of an equality guard read no v2: it compared v1 with v1");
        }
    }
}

impl Fake {
    pub fn start() -> Fake {
        Fake::bind(0)
    }

    pub fn bind(port: u16) -> Fake {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(4)
            .enable_all()
            .build()
            .unwrap();
        let listener = rt
            .block_on(TcpListener::bind(("127.0.0.1", port)))
            .or_else(|_| rt.block_on(TcpListener::bind(("127.0.0.1", 0))))
            .unwrap();
        let port = listener.local_addr().unwrap().port();
        let state = Arc::new(Mutex::new(State {
            v2: true,
            file_limit: 8 << 20,
            answer_limit: 32 << 20,
            ..State::default()
        }));
        let shared = state.clone();
        rt.spawn(async move {
            loop {
                let Ok((sock, _)) = listener.accept().await else {
                    return;
                };
                tokio::spawn(serve(shared.clone(), sock));
            }
        });
        Fake {
            rt,
            port,
            uri: format!("http://127.0.0.1:{port}"),
            state,
        }
    }

    pub fn with<T>(&self, f: impl FnOnce(&mut State) -> T) -> T {
        f(&mut self.state.lock().unwrap())
    }

    /// A commit holding `files` and the repository's identity file, as a
    /// checkout a retrieval wrote holds it.
    pub fn commit(&self, files: &[(&str, &[u8])]) -> String {
        let mut files: Files = files
            .iter()
            .map(|(p, b)| (p.to_string(), Entry::file(b)))
            .collect();
        files
            .entry(".syns.yaml".to_string())
            .or_insert_with(|| Entry::file(IDENTITY.as_bytes()));
        self.with(|s| s.add(files))
    }

    /// A commit of these entries.
    pub fn commit_entries(&self, files: Files) -> String {
        self.with(|s| s.add(files))
    }

    /// A commit laying `changes` over the head, `None` removing a path.
    pub fn change(&self, changes: &[(&str, Option<&[u8]>)]) -> String {
        self.with(|s| {
            let mut files = s
                .holder_head()
                .map(|(_, c)| c.files.clone())
                .unwrap_or_default();
            for (path, bytes) in changes {
                match bytes {
                    Some(bytes) => files.insert(path.to_string(), Entry::file(bytes)),
                    None => files.remove(*path),
                };
            }
            s.add(files)
        })
    }

    pub fn head(&self) -> Commit {
        self.with(|s| s.holder_head().unwrap().1.clone())
    }

    pub fn act(&self, route: &str, act: Act) {
        self.with(|s| s.acts.entry(route.to_string()).or_default().push_back(act));
    }

    pub fn log(&self) -> Vec<Seen> {
        self.with(|s| s.log.clone())
    }

    pub fn clear(&self) {
        self.with(|s| {
            s.log.clear();
            s.max_in_flight.clear();
        });
    }

    pub fn v2(&self, on: bool) {
        self.with(|s| {
            s.v2 = on;
            s.expect_v2 = on;
        });
    }

    /// The log's routes, in order.
    pub fn routes(&self) -> Vec<String> {
        self.log().into_iter().map(|s| s.route).collect()
    }

    /// The batch reads logged.
    pub fn reads(&self) -> Vec<Seen> {
        self.log()
            .into_iter()
            .filter(|s| s.route == "v2 read")
            .collect()
    }
}

// ---- one machine running the binary -------------------------------------------

pub struct Env {
    pub fake: Fake,
    pub config: TempDir,
    pub cache: TempDir,
    pub root: TempDir,
}

impl Env {
    pub fn new() -> Env {
        Env::on(Fake::start())
    }

    pub fn on(fake: Fake) -> Env {
        let env = Env {
            fake,
            config: tempfile::tempdir().unwrap(),
            cache: tempfile::tempdir().unwrap(),
            root: tempfile::tempdir().unwrap(),
        };
        env.credential(Some("alice"));
        env
    }

    pub fn credential(&self, username: Option<&str>) {
        let mut body = json!({"token": "test-token"});
        if let Some(name) = username {
            body["username"] = json!(name);
        }
        std::fs::write(
            self.config.path().join("credentials.json"),
            body.to_string(),
        )
        .unwrap();
    }

    pub fn signed_out(&self) {
        let _ = std::fs::remove_file(self.config.path().join("credentials.json"));
    }

    /// A directory under the machine's root.
    pub fn dir(&self, name: &str) -> PathBuf {
        let dir = self.root.path().join(name);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    pub fn store(&self) -> PathBuf {
        self.cache
            .path()
            .join("store")
            .join(format!("127.0.0.1-{}", self.fake.port))
            .join("u-alice")
    }

    pub fn command(&self, cwd: &Path, args: &[&str]) -> std::process::Command {
        let mut command = std::process::Command::new(assert_cmd::cargo::cargo_bin("syns"));
        command
            .current_dir(cwd)
            .env("SYNS_CONFIG_DIR", self.config.path())
            .env("SYNS_CACHE_DIR", self.cache.path())
            .env_remove("SYNS_URL")
            .env_remove("SYNS_CACHE_MAX_BYTES")
            .env_remove("SYNS_CALLER")
            .arg("--server")
            .arg(&self.fake.uri)
            .args(args);
        command
    }

    pub fn run(&self, cwd: &Path, args: &[&str]) -> Output {
        self.command(cwd, args).output().unwrap()
    }

    pub fn run_env(&self, cwd: &Path, envs: &[(&str, &str)], args: &[&str]) -> Output {
        let mut command = self.command(cwd, args);
        for (k, v) in envs {
            command.env(k, v);
        }
        command.output().unwrap()
    }
}

/// Every file under `dir` but the working copy's state, path to bytes.
pub fn files_of(dir: &Path) -> BTreeMap<String, Vec<u8>> {
    let mut out = BTreeMap::new();
    let mut pending = vec![dir.to_path_buf()];
    while let Some(d) = pending.pop() {
        for entry in std::fs::read_dir(&d).unwrap().flatten() {
            let path = entry.path();
            let rel = path
                .strip_prefix(dir)
                .unwrap()
                .to_string_lossy()
                .replace('\\', "/");
            if rel.starts_with(".syns-") || rel.split('/').any(|s| s.starts_with(".syns-")) {
                continue;
            }
            let ft = entry.file_type().unwrap();
            if ft.is_dir() {
                pending.push(path);
            } else {
                out.insert(rel, std::fs::read(&path).unwrap());
            }
        }
    }
    out
}

/// A run's stdout, stderr and exit, the fake's port written `PORT`.
pub fn transcript(out: &Output, port: u16) -> (String, String, Option<i32>) {
    let clean = |b: &[u8]| String::from_utf8_lossy(b).replace(&port.to_string(), "PORT");
    (clean(&out.stdout), clean(&out.stderr), out.status.code())
}

pub fn ok(out: &Output) {
    assert!(
        out.status.success(),
        "exit {:?}\nstdout:\n{}\nstderr:\n{}",
        out.status.code(),
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
}

/// `n` small text files, `f0000.md` on.
pub fn many(n: usize) -> Vec<(String, Vec<u8>)> {
    (0..n)
        .map(|i| {
            (
                format!("d{}/f{i:04}.md", i % 7),
                format!("file {i}\n").into_bytes(),
            )
        })
        .collect()
}

pub fn as_refs(files: &[(String, Vec<u8>)]) -> Vec<(&str, &[u8])> {
    files
        .iter()
        .map(|(p, b)| (p.as_str(), b.as_slice()))
        .collect()
}
