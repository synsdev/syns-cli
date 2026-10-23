//! `syns diff` under `--provenance` or `--content` (roadmap 224, D-254):
//! what changed since a version, each path's text at the newer end and
//! the provenance of the newest version that changed it, built from one
//! `EP-v2-changes`, the batch read at the same commit and a walk of
//! `EP-versions` pages; no patch is read.

use std::collections::{HashMap, HashSet};
use std::sync::{Mutex, PoisonError};

use console::style;
use serde::Serialize;

use crate::client::{
    CommitProvenance, SynsClient, VersionEntry, VersionListResponse, hash_mismatch, undecodable,
};
use crate::config::Config;
use crate::errors::CliError;
use crate::output::Output;
use crate::push::collector::is_text;
use crate::read::batch::{Ask, FILE_ALONE_BYTES, Frame, IN_FLIGHT, Sink, read_chunks};
use crate::read::store::{Store, is_blob_hash};
use crate::read::v2::{Change, Changes};
use crate::read::{
    RepoScope, counted_from, reference_refusal, refuse_reference_spelling, repository_read_refusal,
    version_not_found_refusal,
};
use crate::repo::folder::FolderScope;

/// The most versions one `EP-versions` page answers (`Page<T>`).
pub const PAGE: u32 = 100;

/// `CS-diff-str-needs-from`.
pub const NEEDS_FROM: &str =
    "--provenance and --content need --from: name the version the answer starts from";

/// `CS-diff-str-from-newer`.
fn from_newer(from: impl std::fmt::Display, to: impl std::fmt::Display) -> CliError {
    CliError::Config {
        message: format!("--from {from} is newer than --to {to}: name the older version first"),
    }
}

/// What the new mode was asked for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Wants {
    pub provenance: bool,
    pub content: bool,
}

/// A typed `--path` as the changes request sends it: mapped as `syns glob
/// --path` maps it (`repository_argument`, D-134), every trailing `/`
/// dropped, a value naming the root sending none. Inside a folder bound to
/// its holder nothing typed is the folder's recorded path.
pub fn changes_path(
    folder: Option<&FolderScope>,
    typed: Option<&str>,
) -> Result<Option<String>, CliError> {
    if folder.is_none() && typed.is_none() {
        return Ok(None);
    }
    Ok(crate::read::repository_argument(folder, typed)?.and_then(trimmed))
}

/// `path` with every trailing `/` dropped, none where nothing is left.
pub fn trimmed(path: String) -> Option<String> {
    let kept = path.trim_end_matches('/');
    (!kept.is_empty()).then(|| kept.to_string())
}

/// An end as the changes request sends it: digits and full hashes as
/// typed, any other spelling (a hash prefix) resolved through
/// `EP-get-version` to the number of the version it names.
async fn sent_end(
    client: &SynsClient,
    repo_id: &str,
    token: Option<&str>,
    typed: &str,
) -> Result<String, CliError> {
    if typed.bytes().all(|b| b.is_ascii_digit()) || is_blob_hash(typed) {
        return Ok(typed.to_string());
    }
    let (entry, _) = client
        .get_version(repo_id, token, typed)
        .await
        .map_err(|e| reference_refusal(e, repo_id, typed))?;
    Ok(entry.version.to_string())
}

/// A refusal of the changes request: the repository's where the `404`
/// names it, the versions the run was asked for where it names a version.
fn changes_refusal(err: CliError, repo_id: &str, from: &str, to: Option<&str>) -> CliError {
    if err.names_missing_repository() {
        return err.repo_or(repo_id);
    }
    if err.names_missing_version() {
        let asked = match to {
            Some(to) if to != from => format!("{from} or {to}"),
            _ => from.to_string(),
        };
        return err.with_version_not_found_context(version_not_found_refusal(&asked));
    }
    err
}

// ---- the walk ----------------------------------------------------------------

/// The pages the walk reads: the address's whole list, never with `path`.
pub(crate) trait Pages {
    /// The page of `PAGE` versions at `offset`.
    async fn page(&mut self, offset: u32) -> Result<VersionListResponse, CliError>;
    /// Where `commit` stands in that list (`EP-version-place`), none
    /// where the address answers `NOT_FOUND` (it numbers densely).
    async fn place(&mut self, commit: &str) -> Result<Option<u32>, CliError>;
}

/// The span the walk attributes in: (`from`, `to`], `to` named by
/// `to_commit`, asked as the head where `to_is_head`.
#[derive(Debug, Clone)]
pub(crate) struct Span {
    pub from: u32,
    pub to: u32,
    pub to_commit: String,
    pub to_is_head: bool,
}

/// The version credited with a path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Credit {
    pub version: u32,
    pub author: String,
    pub provenance: Option<CommitProvenance>,
}

impl Credit {
    fn of(entry: &VersionEntry) -> Credit {
        Credit {
            version: entry.version,
            author: entry.author.clone(),
            provenance: entry.provenance.clone(),
        }
    }
}

/// The walk's state across pages.
struct Ledger<'a> {
    span: &'a Span,
    wanted: HashMap<&'a str, Vec<usize>>,
    found: Vec<Option<Credit>>,
    left: usize,
    seen: HashSet<u32>,
}

impl Ledger<'_> {
    /// Credits each wanted path the page's newest version in the span
    /// names; answers whether the page holds a version at or below
    /// `from`. A version above `to` (one landing during the run) and one
    /// already seen (an overlap between pages) are skipped (row 7).
    fn consume(&mut self, page: &VersionListResponse) -> bool {
        let mut reached = false;
        for entry in &page.data {
            if entry.version > self.span.to || !self.seen.insert(entry.version) {
                continue;
            }
            if entry.version <= self.span.from {
                reached = true;
                continue;
            }
            for path in &entry.files_changed {
                if let Some(places) = self.wanted.remove(path.as_str()) {
                    for place in places {
                        self.found[place] = Some(Credit::of(entry));
                        self.left -= 1;
                    }
                }
            }
        }
        reached
    }
}

/// Whether `page`, read at `offset`, is the list's last (row 4a): short of
/// a whole page, an empty one included, or reaching `total`.
fn is_last(page: &VersionListResponse, offset: u32) -> bool {
    page.data.len() < PAGE as usize
        || u64::from(offset) + page.data.len() as u64 >= u64::from(page.total)
}

/// Credits each of `paths` the newest listed version in the span whose
/// `filesChanged` names it, `None` where no version the walk read does
/// (rows 3, 4, 4a, 7, 8a).
pub(crate) async fn walk<P: Pages>(
    pages: &mut P,
    span: &Span,
    paths: &[&str],
) -> Result<Vec<Option<Credit>>, CliError> {
    let mut wanted: HashMap<&str, Vec<usize>> = HashMap::new();
    for (place, path) in paths.iter().enumerate() {
        wanted.entry(path).or_default().push(place);
    }
    let mut ledger = Ledger {
        span,
        wanted,
        found: vec![None; paths.len()],
        left: paths.len(),
        seen: HashSet::new(),
    };
    if ledger.left == 0 {
        return Ok(ledger.found);
    }

    // The start (row 8a): page 0, then, where `--to` stands below it, one
    // jump to `to`'s place.
    let mut offset = 0;
    let mut page = pages.page(0).await?;
    let holds_to = page.data.iter().any(|v| v.version <= span.to);
    if !span.to_is_head
        && !holds_to
        && !is_last(&page, 0)
        && let Some(top) = page.data.first().map(|v| v.version)
    {
        let jump = match pages.place(&span.to_commit).await? {
            Some(position) => position,
            None => top.saturating_sub(span.to),
        };
        if jump > 0 {
            let jumped = pages.page(jump).await?;
            if jumped.data.first().is_some_and(|v| v.version < span.to) {
                // A list with gaps the place did not name: page by page
                // from the second page on (Known issue 3).
                ledger.consume(&page);
                offset = PAGE;
                page = pages.page(offset).await?;
            } else {
                offset = jump;
                page = jumped;
            }
        }
    }

    loop {
        let reached = ledger.consume(&page);
        if ledger.left == 0 || reached || is_last(&page, offset) {
            return Ok(ledger.found);
        }
        offset += PAGE;
        page = pages.page(offset).await?;
    }
}

/// The address's own list over the wire.
struct ListPages<'a> {
    client: &'a SynsClient,
    repo_id: &'a str,
    token: Option<&'a str>,
}

impl Pages for ListPages<'_> {
    async fn page(&mut self, offset: u32) -> Result<VersionListResponse, CliError> {
        self.client
            .list_versions(self.repo_id, self.token, PAGE, offset, None)
            .await
            .map(|(page, _)| page)
            .map_err(|e| repository_read_refusal(e, self.repo_id))
    }

    async fn place(&mut self, commit: &str) -> Result<Option<u32>, CliError> {
        match self
            .client
            .get_version_place(self.repo_id, self.token, commit)
            .await
        {
            Ok(place) => Ok(Some(place.position)),
            Err(CliError::Api {
                status: Some(404), ..
            }) => Ok(None),
            Err(err) => Err(err),
        }
    }
}

// ---- the texts ---------------------------------------------------------------

/// What the batch answered for one ask.
#[derive(Debug)]
enum Got {
    /// The bytes as a string where they are text, none where not.
    Text(Option<String>),
    Deferred,
}

fn text_of(bytes: Vec<u8>) -> Option<String> {
    if is_text(&bytes) {
        String::from_utf8(bytes).ok()
    } else {
        None
    }
}

/// The batch's sink for `syns diff --content`.
struct TextSink<'a> {
    repo_id: &'a str,
    store: Option<&'a Store>,
    /// Each ask's path as served, and as the answer names it.
    served: &'a [String],
    shown: &'a [String],
    to_version: String,
    got: Mutex<Vec<Option<Got>>>,
}

impl Sink for TextSink<'_> {
    fn frame(&self, index: usize, frame: Frame) -> Result<(), CliError> {
        let got = match frame {
            Frame::Ok { sha, bytes, .. } => {
                if let Some(store) = self.store {
                    store.admit_many(&sha, &bytes);
                    store.note_seen(self.repo_id, &self.served[index], &sha);
                }
                Got::Text(text_of(bytes))
            }
            Frame::Unchanged { sha, .. } => {
                match self.store.and_then(|store| store.verified(&sha)) {
                    Some(bytes) => Got::Text(text_of(bytes)),
                    // The stored blob went between the ask and the answer:
                    // named as deferred, never read alone.
                    None => Got::Deferred,
                }
            }
            Frame::TooLarge { .. } => Got::Deferred,
            Frame::NotFound => {
                return Err(undecodable(
                    reqwest::StatusCode::OK,
                    format!(
                        "{} stands at version {} in the changes answer but not in the read",
                        self.shown[index], self.to_version
                    ),
                ));
            }
            Frame::Mismatch { expected, actual } => {
                return Err(hash_mismatch(&self.shown[index], &expected, &actual));
            }
        };
        self.got.lock().unwrap_or_else(PoisonError::into_inner)[index] = Some(got);
        Ok(())
    }
}

/// Whether an entry still stands at the newer end.
fn stands(change: &Change) -> bool {
    change.status != "deleted"
}

// ---- the document ------------------------------------------------------------

#[derive(Serialize)]
struct EndOut<'a> {
    commit: &'a str,
    version: Option<u32>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct ProvenanceOut {
    version: u32,
    publisher: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    integration: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    run: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    trigger: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    task_ref: Option<String>,
    recorded: bool,
}

impl ProvenanceOut {
    fn of(credit: &Credit) -> ProvenanceOut {
        let recorded = credit.provenance.as_ref();
        ProvenanceOut {
            version: credit.version,
            publisher: recorded
                .map(|p| p.publisher.clone())
                .unwrap_or_else(|| credit.author.clone()),
            integration: recorded.and_then(|p| p.integration.clone()),
            run: recorded.and_then(|p| p.run.clone()),
            trigger: recorded.and_then(|p| p.trigger.clone()),
            task_ref: recorded.and_then(|p| p.task_ref.clone()),
            recorded: recorded.is_some(),
        }
    }

    /// `CS-diff-blk-change-line`'s fourth column.
    fn detail(&self) -> String {
        if !self.recorded {
            return "(no provenance recorded)".to_string();
        }
        let mut parts: Vec<String> = Vec::new();
        if let Some(integration) = &self.integration {
            parts.push(integration.clone());
        }
        if let Some(run) = &self.run {
            parts.push(format!("run {run}"));
        }
        if let Some(trigger) = &self.trigger {
            parts.push(format!("trigger {trigger}"));
        }
        if let Some(task) = &self.task_ref {
            parts.push(format!("task {task}"));
        }
        parts.join(" \u{b7} ")
    }
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct EntryOut {
    path: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    old_path: Option<String>,
    status: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    old_sha: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    new_sha: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    size: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    provenance: Option<Option<ProvenanceOut>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    text: Option<Option<String>>,
}

#[derive(Serialize)]
struct Document<'a> {
    from: EndOut<'a>,
    to: EndOut<'a>,
    entries: &'a [EntryOut],
    #[serde(skip_serializing_if = "Option::is_none")]
    deferred: Option<Vec<String>>,
}

fn style_word(status: &str) -> String {
    match status {
        "added" => style(status).green().to_string(),
        "modified" => style(status).yellow().to_string(),
        "deleted" => style(status).red().to_string(),
        "renamed" => style(status).cyan().to_string(),
        other => style(other).dim().to_string(),
    }
}

/// `CS-diff-blk-change-line` once per entry, then `CS-diff-blk-summary`.
fn human_lines(entries: &[EntryOut], from: &str, to: &str) -> Vec<String> {
    let cells: Vec<[String; 4]> = entries
        .iter()
        .map(|entry| {
            let path = match &entry.old_path {
                Some(old) => format!("{old} \u{2192} {}", entry.path),
                None => entry.path.clone(),
            };
            let (publisher, detail) = match &entry.provenance {
                Some(Some(p)) => (p.publisher.clone(), p.detail()),
                _ => (String::new(), "(no listed version changed it)".to_string()),
            };
            [entry.status.clone(), path, publisher, detail]
        })
        .collect();
    let width = |column: usize| {
        cells
            .iter()
            .map(|c| c[column].chars().count())
            .max()
            .unwrap_or(0)
    };
    let widths = [width(0), width(1), width(2)];
    let pad = |text: &str, width: usize| " ".repeat(width.saturating_sub(text.chars().count()));
    let mut lines: Vec<String> = cells
        .iter()
        .map(|[status, path, publisher, detail]| {
            let line = format!(
                "{}{}  {path}{}  {publisher}{}  {detail}",
                style_word(status),
                pad(status, widths[0]),
                pad(path, widths[1]),
                pad(publisher, widths[2]),
            );
            line.trim_end().to_string()
        })
        .collect();
    let n = entries.len();
    lines.push(match n {
        0 => format!("{from} \u{2192} {to} \u{b7} no change"),
        1 => format!("{from} \u{2192} {to} \u{b7} 1 path"),
        n => format!("{from} \u{2192} {to} \u{b7} {n} paths"),
    });
    lines
}

// ---- the run -------------------------------------------------------------------

/// The new mode, the scope bound and `--to`-alone refused already.
pub async fn run(
    config: &Config,
    output: &Output,
    scope: RepoScope,
    from: Option<String>,
    to: Option<String>,
    path: Option<&str>,
    wants: Wants,
) -> Result<(), CliError> {
    let RepoScope {
        repo_id,
        token,
        folder,
    } = scope;
    let token = token.as_deref();

    // 1 — every refusal before any request.
    let Some(from) = from else {
        return Err(CliError::Config {
            message: NEEDS_FROM.to_string(),
        });
    };
    refuse_reference_spelling(&from)?;
    if let Some(to) = &to {
        refuse_reference_spelling(to)?;
    }
    let path = changes_path(folder.as_ref(), path)?;
    let digits = |v: &str| v.bytes().all(|b| b.is_ascii_digit());
    if let Some(to) = &to
        && digits(&from)
        && digits(to)
        && from.parse::<u64>().unwrap_or(u64::MAX) > to.parse::<u64>().unwrap_or(u64::MAX)
    {
        return Err(from_newer(&from, to));
    }
    let client = SynsClient::new(config.server_url())?;

    // 2 — the ends as sent, then the one changes request.
    let from_sent = sent_end(&client, &repo_id, token, &from).await?;
    let to_sent = match &to {
        Some(to) => sent_end(&client, &repo_id, token, to).await?,
        None => "head".to_string(),
    };
    let Changes {
        from: from_end,
        to: to_end,
        entries,
    } = crate::read::v2::changes(
        &client,
        None,
        &repo_id,
        token,
        &from_sent,
        &to_sent,
        path.as_deref(),
    )
    .await
    .map_err(|e| changes_refusal(e, &repo_id, &from, to.as_deref()))?;
    if wants.provenance {
        for (name, end) in [("from", &from_end), ("to", &to_end)] {
            if end.version.is_none() {
                return Err(undecodable(
                    reqwest::StatusCode::OK,
                    format!("the changes answer names no version for {name}"),
                ));
            }
        }
    }
    if let (Some(f), Some(t)) = (from_end.version, to_end.version)
        && f > t
    {
        return Err(from_newer(f, t));
    }
    let shown: Vec<String> = entries
        .iter()
        .map(|e| counted_from(folder.as_ref(), &e.path))
        .collect();
    let to_label = to_end
        .version
        .map(|v| v.to_string())
        .unwrap_or_else(|| to_end.commit.clone());

    // 3 — the texts, at the answer's commit.
    let mut texts: Vec<Option<Option<String>>> = vec![None; entries.len()];
    let mut deferred_at: Vec<bool> = vec![false; entries.len()];
    if wants.content && entries.iter().any(stands) {
        let store = Store::open(config, token);
        let mut asked: Vec<usize> = Vec::new();
        let mut asks: Vec<Ask> = Vec::new();
        for (i, entry) in entries.iter().enumerate().filter(|(_, e)| stands(e)) {
            // A submodule (gitlink) is never asked and reads as a file
            // that is not text (the coordinator's row 13 ruling): the
            // changes answer names no mode, and it sizes every blob it
            // lists, so a standing entry it gives no size is the gitlink.
            if entry.size.is_none() {
                texts[i] = Some(None);
                continue;
            }
            if entry.size.is_some_and(|size| size > FILE_ALONE_BYTES) {
                deferred_at[i] = true;
                continue;
            }
            asked.push(i);
            asks.push(Ask {
                path: entry.path.clone(),
                have: store
                    .as_ref()
                    .and_then(|store| store.held(&repo_id, &entry.path))
                    .map(|held| held.sha),
                expect: entry.new_sha.clone().filter(|sha| is_blob_hash(sha)),
                size: entry.size,
            });
        }
        if !asks.is_empty() {
            let served: Vec<String> = asks.iter().map(|a| a.path.clone()).collect();
            let shown_asked: Vec<String> = asked.iter().map(|i| shown[*i].clone()).collect();
            let sink = TextSink {
                repo_id: &repo_id,
                store: store.as_ref(),
                served: &served,
                shown: &shown_asked,
                to_version: to_label.clone(),
                got: Mutex::new((0..asks.len()).map(|_| None).collect()),
            };
            let read = read_chunks(
                &client,
                None,
                &repo_id,
                token,
                &to_end.commit,
                &asks,
                IN_FLIGHT,
                &sink,
            )
            .await;
            if let Some(store) = &store {
                store.finish();
            }
            read?;
            let got = sink
                .got
                .into_inner()
                .unwrap_or_else(PoisonError::into_inner);
            for (k, got) in got.into_iter().enumerate() {
                match got {
                    Some(Got::Text(text)) => texts[asked[k]] = Some(text),
                    Some(Got::Deferred) | None => deferred_at[asked[k]] = true,
                }
            }
        }
    }

    // 4 — the walk.
    let mut credits: Vec<Option<Credit>> = vec![None; entries.len()];
    if wants.provenance && !entries.is_empty() {
        let span = Span {
            from: from_end.version.unwrap_or(0),
            to: to_end.version.unwrap_or(0),
            to_commit: to_end.commit.clone(),
            to_is_head: to.is_none(),
        };
        let paths: Vec<&str> = entries.iter().map(|e| e.path.as_str()).collect();
        let mut pages = ListPages {
            client: &client,
            repo_id: &repo_id,
            token,
        };
        credits = walk(&mut pages, &span, &paths).await?;
    }

    // 5 — the answer.
    let deferred: Vec<String> = (0..entries.len())
        .filter(|i| deferred_at[*i])
        .map(|i| shown[i].clone())
        .collect();
    let rows: Vec<EntryOut> = entries
        .into_iter()
        .enumerate()
        .map(|(i, entry)| EntryOut {
            path: shown[i].clone(),
            old_path: entry
                .old_path
                .as_deref()
                .map(|old| counted_from(folder.as_ref(), old)),
            status: entry.status,
            old_sha: entry.old_sha,
            new_sha: entry.new_sha,
            size: entry.size,
            provenance: wants
                .provenance
                .then(|| credits[i].as_ref().map(ProvenanceOut::of)),
            text: if wants.content { texts[i].take() } else { None },
        })
        .collect();
    if output.is_json() {
        output.json(&Document {
            from: EndOut {
                commit: &from_end.commit,
                version: from_end.version,
            },
            to: EndOut {
                commit: &to_end.commit,
                version: to_end.version,
            },
            entries: &rows,
            deferred: wants.content.then_some(deferred),
        });
    } else {
        let from_label = from_end
            .version
            .map(|v| v.to_string())
            .unwrap_or_else(|| from_end.commit.clone());
        for line in human_lines(&rows, &from_label, &to_label) {
            println!("{line}");
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn entry(version: u32, files: &[&str]) -> VersionEntry {
        VersionEntry {
            version,
            sha: format!("{version:040x}"),
            parent_sha: None,
            message: format!("v{version}"),
            message_body: None,
            author: "maria".to_string(),
            created_at: "2026-10-01T00:00:00Z".to_string(),
            files_changed: files.iter().map(|f| f.to_string()).collect(),
            provenance: Some(CommitProvenance {
                publisher: "maria".to_string(),
                integration: Some("bb".to_string()),
                run: Some(format!("r{version}")),
                trigger: None,
                task_ref: None,
            }),
            path: None,
            moved_from: None,
        }
    }

    /// Versions, newest first, each with the paths it changed.
    type Listed = Vec<(u32, Vec<String>)>;

    /// A whole list, newest first, each version naming what `changed`
    /// gives it and `filler` otherwise; `place` answering `position`, or
    /// `NOT_FOUND` where none; every request counted.
    struct Fake {
        list: Listed,
        total: Option<u32>,
        position: Option<u32>,
        /// Versions landing at the top before the page at this offset.
        landing: Option<(u32, Listed)>,
        log: Vec<String>,
    }

    impl Fake {
        fn dense(top: u32, changed: &[(u32, &[&str])]) -> Fake {
            let changed: HashMap<u32, Vec<String>> = changed
                .iter()
                .map(|(v, f)| (*v, f.iter().map(|s| s.to_string()).collect()))
                .collect();
            Fake {
                list: (1..=top)
                    .rev()
                    .map(|v| {
                        (
                            v,
                            changed
                                .get(&v)
                                .cloned()
                                .unwrap_or_else(|| vec!["filler".to_string()]),
                        )
                    })
                    .collect(),
                total: None,
                position: None,
                landing: None,
                log: Vec::new(),
            }
        }
    }

    impl Pages for Fake {
        async fn page(&mut self, offset: u32) -> Result<VersionListResponse, CliError> {
            self.log.push(format!("page {offset}"));
            if let Some((at, landed)) = &self.landing
                && *at == offset
            {
                let mut landed = landed.clone();
                landed.append(&mut self.list);
                self.list = landed;
                self.landing = None;
            }
            let data: Vec<VersionEntry> = self
                .list
                .iter()
                .skip(offset as usize)
                .take(PAGE as usize)
                .map(|(v, f)| {
                    let files: Vec<&str> = f.iter().map(String::as_str).collect();
                    entry(*v, &files)
                })
                .collect();
            Ok(VersionListResponse {
                data,
                total: self.total.unwrap_or(self.list.len() as u32),
                limit: PAGE,
                offset,
            })
        }

        async fn place(&mut self, commit: &str) -> Result<Option<u32>, CliError> {
            self.log.push(format!("place {commit}"));
            Ok(self.position)
        }
    }

    fn span(from: u32, to: u32, to_is_head: bool) -> Span {
        Span {
            from,
            to,
            to_commit: format!("{to:040x}"),
            to_is_head,
        }
    }

    fn versions(found: &[Option<Credit>]) -> Vec<Option<u32>> {
        found
            .iter()
            .map(|c| c.as_ref().map(|c| c.version))
            .collect()
    }

    // Row 3.
    #[tokio::test]
    async fn newest_of_two_changes_wins() {
        let mut fake = Fake::dense(
            20,
            &[(12, &["docs/pricing.md"]), (17, &["docs/pricing.md"])],
        );
        let found = walk(&mut fake, &span(10, 20, true), &["docs/pricing.md"])
            .await
            .unwrap();
        assert_eq!(versions(&found), vec![Some(17)]);
        assert_eq!(
            found[0]
                .as_ref()
                .unwrap()
                .provenance
                .as_ref()
                .unwrap()
                .run
                .as_deref(),
            Some("r17")
        );
        assert_eq!(fake.log, vec!["page 0"]);
    }

    // Rows 4 and 8: 120 versions since V, 2 pages.
    #[tokio::test]
    async fn a_span_of_120_reads_two_pages() {
        let mut fake = Fake::dense(
            1120,
            &[
                (1001, &["a"]),
                (1120, &["b"]),
                (1030, &["c"]),
                (1101, &["c"]),
            ],
        );
        let found = walk(&mut fake, &span(1000, 1120, true), &["a", "b", "c"])
            .await
            .unwrap();
        assert_eq!(versions(&found), vec![Some(1001), Some(1120), Some(1101)]);
        assert_eq!(fake.log, vec!["page 0", "page 100"]);
    }

    // Row 4a, the first mock: the list ends above V.
    #[tokio::test]
    async fn a_list_ending_above_from_stops_on_its_short_page() {
        let mut fake = Fake::dense(150, &[(50, &["x"])]);
        let found = walk(&mut fake, &span(0, 150, true), &["x", "y"])
            .await
            .unwrap();
        assert_eq!(versions(&found), vec![Some(50), None]);
        assert_eq!(fake.log, vec!["page 0", "page 100"]);
    }

    // Row 4a, the second mock: a full page, then `data: []` with a
    // `total` of 100,000; bounded at 5 s.
    #[tokio::test]
    async fn an_empty_page_past_the_scan_limit_stops_the_walk() {
        let mut fake = Fake::dense(100, &[]);
        fake.total = Some(100_000);
        let found = tokio::time::timeout(
            Duration::from_secs(5),
            walk(&mut fake, &span(0, 100_100, true), &["y"]),
        )
        .await
        .expect("the walk ends")
        .unwrap();
        assert_eq!(versions(&found), vec![None]);
        assert_eq!(fake.log, vec!["page 0", "page 100"]);
    }

    // Row 4a: `offset + data.length >= total` ends the walk on a full
    // page.
    #[tokio::test]
    async fn a_page_reaching_total_stops_the_walk() {
        let mut fake = Fake::dense(200, &[]);
        let found = walk(&mut fake, &span(0, 200, true), &["y"]).await.unwrap();
        assert_eq!(versions(&found), vec![None]);
        assert_eq!(fake.log, vec!["page 0", "page 100"]);
    }

    // Row 7: versions above `to` are skipped, and an overlap between
    // pages is dropped.
    #[tokio::test]
    async fn versions_above_to_are_skipped_and_overlaps_dropped() {
        // 1101 lands before the first page.
        let mut fake = Fake::dense(1100, &[(1050, &["p"]), (1001, &["q"])]);
        fake.landing = Some((0, vec![(1101, vec!["p".to_string()])]));
        let found = walk(&mut fake, &span(1000, 1100, true), &["p", "q"])
            .await
            .unwrap();
        assert_eq!(versions(&found), vec![Some(1050), Some(1001)]);
        assert_eq!(fake.log, vec!["page 0", "page 100"]);

        // 1102 lands between the pages: page 1 opens on 1002 again.
        let mut fake = Fake::dense(1101, &[(1001, &["q"]), (1101, &["p"])]);
        fake.landing = Some((100, vec![(1102, vec!["q".to_string()])]));
        let found = walk(&mut fake, &span(1000, 1100, true), &["q", "p"])
            .await
            .unwrap();
        assert_eq!(versions(&found), vec![Some(1001), None]);
        assert_eq!(fake.log, vec!["page 0", "page 100"]);
    }

    // Row 8a: a dense root refuses the place; the walk jumps by number,
    // never reading the pages wholly above `to`.
    #[tokio::test]
    async fn a_to_below_the_head_jumps_by_number_on_a_dense_list() {
        let mut fake = Fake::dense(2250, &[(1995, &["e"])]);
        let found = walk(&mut fake, &span(1990, 2000, false), &["e"])
            .await
            .unwrap();
        assert_eq!(versions(&found), vec![Some(1995)]);
        assert_eq!(
            fake.log,
            vec![
                "page 0".to_string(),
                format!("place {:040x}", 2000),
                "page 250".to_string()
            ]
        );
    }

    // Row 8a: a list with gaps; the place names the offset.
    #[tokio::test]
    async fn a_to_below_the_head_jumps_to_the_place_answered() {
        let mut fake = Fake::dense(2250, &[(1995, &["e"])]);
        // Take 70 versions above `to` out: `to` now stands at 180.
        fake.list.retain(|(v, _)| !(2001..=2070).contains(v));
        fake.position = Some(180);
        let found = walk(&mut fake, &span(1990, 2000, false), &["e"])
            .await
            .unwrap();
        assert_eq!(versions(&found), vec![Some(1995)]);
        assert_eq!(fake.log[2], "page 180");
        assert_eq!(fake.log.len(), 3);
    }

    // Row 8a step 3: a jump opening below `to` falls back to walking page
    // by page from offset 100.
    #[tokio::test]
    async fn a_jump_opening_below_to_falls_back_to_page_by_page() {
        let mut fake = Fake::dense(2250, &[(1995, &["e"])]);
        // Gaps the place does not name: the jump by number overshoots.
        fake.list.retain(|(v, _)| !(2001..=2070).contains(v));
        let found = walk(&mut fake, &span(1990, 2000, false), &["e"])
            .await
            .unwrap();
        assert_eq!(versions(&found), vec![Some(1995)]);
        assert_eq!(
            fake.log,
            vec![
                "page 0".to_string(),
                format!("place {:040x}", 2000),
                "page 250".to_string(),
                "page 100".to_string()
            ]
        );
    }

    // Row 8a: S = 0 where `--to` stands within page 0.
    #[tokio::test]
    async fn to_within_page_zero_costs_no_place() {
        let mut fake = Fake::dense(2050, &[(1995, &["e"])]);
        let found = walk(&mut fake, &span(1990, 2000, false), &["e"])
            .await
            .unwrap();
        assert_eq!(versions(&found), vec![Some(1995)]);
        assert_eq!(fake.log, vec!["page 0"]);
    }

    // No entry, no page.
    #[tokio::test]
    async fn no_entry_reads_no_page() {
        let mut fake = Fake::dense(10, &[]);
        let found = walk(&mut fake, &span(1, 10, true), &[]).await.unwrap();
        assert!(found.is_empty());
        assert!(fake.log.is_empty());
    }

    // Row 23, the root column, and the folder column's mapping.
    #[test]
    fn path_arguments_map_per_scope() {
        let root = |typed: &str| changes_path(None, Some(typed));
        assert_eq!(root("").unwrap(), None);
        assert_eq!(root(".").unwrap(), None);
        assert_eq!(root("a/..").unwrap(), None);
        assert_eq!(root("a/../docs").unwrap(), Some("docs".to_string()));
        assert_eq!(root("2024/").unwrap(), Some("2024".to_string()));
        assert_eq!(root("docs/").unwrap(), Some("docs".to_string()));
        assert_eq!(root("/docs").unwrap(), Some("/docs".to_string()));
        assert!(matches!(root(".."), Err(CliError::Config { .. })));
        assert_eq!(changes_path(None, None).unwrap(), None);

        let folder = FolderScope {
            dir: std::path::PathBuf::from("/w/clients/vela"),
            owner: "alice".to_string(),
            name: "notes".to_string(),
            path: "clients/vela".to_string(),
            checkout: None,
            enclosing: Vec::new(),
            identity: None,
        };
        let inside = |typed: Option<&str>| changes_path(Some(&folder), typed);
        assert_eq!(inside(None).unwrap(), Some("clients/vela".to_string()));
        assert_eq!(inside(Some(".")).unwrap(), Some("clients/vela".to_string()));
        assert_eq!(
            inside(Some("docs/")).unwrap(),
            Some("clients/vela/docs".to_string())
        );
        assert!(matches!(inside(Some("")), Err(CliError::Config { .. })));
        assert!(matches!(
            inside(Some("/docs")),
            Err(CliError::Config { .. })
        ));

        let identity = FolderScope {
            identity: Some("vela".to_string()),
            ..folder.clone()
        };
        assert_eq!(changes_path(Some(&identity), None).unwrap(), None);
        assert_eq!(changes_path(Some(&identity), Some(".")).unwrap(), None);
        assert_eq!(
            changes_path(Some(&identity), Some("docs")).unwrap(),
            Some("docs".to_string())
        );
    }

    // Row 5 and § Human output.
    #[test]
    fn the_human_lines_pad_each_column_and_mark_what_is_missing() {
        let row =
            |status: &str, path: &str, old: Option<&str>, p: Option<ProvenanceOut>| EntryOut {
                path: path.to_string(),
                old_path: old.map(str::to_string),
                status: status.to_string(),
                old_sha: None,
                new_sha: None,
                size: None,
                provenance: Some(p),
                text: None,
            };
        let recorded =
            |publisher: &str, integration: Option<&str>, run: Option<&str>| ProvenanceOut {
                version: 1,
                publisher: publisher.to_string(),
                integration: integration.map(str::to_string),
                run: run.map(str::to_string),
                trigger: None,
                task_ref: None,
                recorded: true,
            };
        let rows = vec![
            row(
                "modified",
                "docs/pricing.md",
                None,
                Some(recorded("maria", Some("bb"), Some("thr_k2vq9"))),
            ),
            row(
                "deleted",
                "drafts/old-plan.md",
                None,
                Some(ProvenanceOut {
                    recorded: false,
                    ..recorded("bart", None, None)
                }),
            ),
            row(
                "renamed",
                "notes/b.md",
                Some("notes/a.md"),
                Some(recorded("ci-bot", Some("github-actions"), Some("39533"))),
            ),
            row("modified", "data/x.csv", None, None),
        ];
        let lines = human_lines(&rows, "4090", "4098");
        assert_eq!(
            lines,
            vec![
                "modified  docs/pricing.md          maria   bb \u{b7} run thr_k2vq9",
                "deleted   drafts/old-plan.md       bart    (no provenance recorded)",
                "renamed   notes/a.md \u{2192} notes/b.md  ci-bot  github-actions \u{b7} run 39533",
                "modified  data/x.csv                       (no listed version changed it)",
                "4090 \u{2192} 4098 \u{b7} 4 paths",
            ]
        );
        assert_eq!(
            human_lines(&[], "7", "7"),
            vec!["7 \u{2192} 7 \u{b7} no change"]
        );
    }
}
