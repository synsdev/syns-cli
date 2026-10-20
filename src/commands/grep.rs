//! `syns grep PATTERN …` — the search over one repository at one
//! version (SPEC u270).
//!
//! The search is the client's own: the tree names the paths, the cache
//! answers the content, and the matcher runs here. Server-side search is
//! deferred by `roadmap/213-cli-repository-reads`, whose revisit
//! condition is a cold search past ten seconds at eight requests in
//! flight — the fan-out cap and the in-flight count below are sized from
//! `units/cli/u270/PROTOTYPE.md`'s figures against that condition.

use std::collections::{BTreeMap, HashSet};
use std::sync::Arc;

use globset::{GlobBuilder, GlobMatcher};
use grep_regex::{RegexMatcher, RegexMatcherBuilder};
use grep_searcher::{Searcher, SearcherBuilder, Sink, SinkMatch};
use tokio::task::JoinSet;

use crate::client::{EntryType, SynsClient};
use crate::config::Config;
use crate::errors::{CliError, partial_truncated_tree, partial_unread_paths};
use crate::output::Output;
use crate::read::store::{BlobContent, Store, get_or_fetch};
use crate::read::{
    ReadOptions, ReadTarget, bind_read_folder, counted_from, mark_partial, read_not_found,
    report_reference, repository_argument, resolve_read_target,
};

/// The grep fan-out cap: the count of kept paths whose content is
/// fetched, taken from `--offset` in the ascending path order. A kept set
/// standing past the window leaves the answer marked `truncated`, naming
/// the window's end as `nextOffset`, and the run at exit `0` — `D-080`
/// puts the bound a run applies to itself outside the partial-answer
/// refusal, and `D-156` pages past it.
pub const FAN_OUT_CAP: usize = 400;

/// The `--offset` help, naming the cap from `FAN_OUT_CAP` so the two
/// cannot drift apart.
pub fn offset_help() -> String {
    format!(
        "Skip this many of the matching paths (the files --path and --glob keep), \
         in ascending path order. One call searches at most {FAN_OUT_CAP} files; \
         where more remain, the JSON answer's nextOffset names the offset to continue from, \
         at the same --version"
    )
}

/// The count of content requests outstanding at once, with nothing else
/// pacing them: the one bound `syns cat PATH…` shares (roadmap 278).
pub use crate::read::IN_FLIGHT;

/// The three output modes, spelt `content`, `files` and `count`. A value
/// outside the three ends the run through the argument parser at exit
/// `2`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, clap::ValueEnum)]
pub enum GrepOutput {
    #[default]
    Content,
    Files,
    Count,
}

impl GrepOutput {
    fn wire(self) -> &'static str {
        match self {
            GrepOutput::Content => "content",
            GrepOutput::Files => "files",
            GrepOutput::Count => "count",
        }
    }
}

/// Everything the search takes beside its pattern and the shared read
/// options.
#[derive(Debug, Clone, Default)]
pub struct GrepArgs {
    pub path: Option<String>,
    pub glob: Vec<String>,
    pub ignore_case: bool,
    pub line_number: bool,
    pub after: Option<u32>,
    pub before: Option<u32>,
    pub context: Option<u32>,
    pub output: GrepOutput,
    pub head_limit: Option<u32>,
    /// The count of kept paths, in ascending path order, the window
    /// starts past (`D-156`).
    pub offset: u32,
}

/// The window `--offset` names over `kept` kept paths: its start and
/// end, clamped to the kept count, and whether kept paths stand past its
/// end (`D-156`).
fn window(kept: usize, offset: u32) -> (usize, usize, bool) {
    let start = usize::try_from(offset).unwrap_or(usize::MAX).min(kept);
    let end = start.saturating_add(FAN_OUT_CAP).min(kept);
    (start, end, kept > end)
}

/// One matched line, with the context lines that are not themselves
/// matches.
#[derive(Debug, Clone)]
pub struct MatchRow {
    pub line: u64,
    pub text: String,
    pub context: Vec<ContextRow>,
}

#[derive(Debug, Clone)]
pub struct ContextRow {
    pub line: u64,
    pub text: String,
}

/// One path the search keeps: `path` in the repository, fetched, and
/// `shown` counted from the run's folder, written.
#[derive(Debug)]
struct Kept {
    path: String,
    shown: String,
    sha: Option<String>,
}

/// What one path's search came to.
#[derive(Debug)]
enum PathOutcome {
    Hits(Vec<MatchRow>),
    Binary,
    Refused,
}

/// The option-combination refusal, raised before any request so nothing
/// is sent (SPEC u270 Contract Surface).
pub fn refuse_option_combination(args: &GrepArgs) -> Result<(), CliError> {
    if args.output != GrepOutput::Content {
        for (stands, option) in [
            (args.line_number, "--line-number"),
            (args.after.is_some(), "--after-context"),
            (args.before.is_some(), "--before-context"),
            (args.context.is_some(), "--context"),
        ] {
            if stands {
                return Err(CliError::Config {
                    message: format!("{option} applies only under --output content"),
                });
            }
        }
    }
    if args.context.is_some() && (args.after.is_some() || args.before.is_some()) {
        return Err(CliError::Config {
            message: "--context cannot stand beside --after-context or --before-context"
                .to_string(),
        });
    }
    // A row bound of `0` carries no row and would still open the
    // fan-out's first batch, so it is refused where the window options
    // of the numbered read are (u270 CR1-3).
    if args.head_limit == Some(0) {
        return Err(CliError::Config {
            message: "--head-limit must be \u{2265} 1".to_string(),
        });
    }
    Ok(())
}

/// The matcher build: the line terminator set to the newline byte
/// `0x0A`, and CRLF handling set on neither the matcher nor the
/// searcher. A pattern holding a newline is refused at compile rather
/// than compiling and matching nothing.
pub fn build_matcher(pattern: &str, ignore_case: bool) -> Result<RegexMatcher, CliError> {
    RegexMatcherBuilder::new()
        .case_insensitive(ignore_case)
        .line_terminator(Some(b'\n'))
        .build(pattern)
        .map_err(|e| CliError::Config {
            message: format!("invalid pattern {pattern}: {e}"),
        })
}

/// A `--glob` value: one holding no path separator matches an entry's
/// base name at every depth, and one holding a separator is anchored at
/// the repository root with `*` and `?` matching no path separator
/// (`SPEC_REVIEW_R3.md` CF-02).
#[derive(Debug)]
pub enum GlobRule {
    BaseName(GlobMatcher),
    WholePath(GlobMatcher),
}

pub fn compile_glob(value: &str) -> Result<GlobRule, CliError> {
    let matcher = GlobBuilder::new(value)
        .literal_separator(true)
        .build()
        .map_err(|e| CliError::Config {
            message: format!("invalid glob {value}: {e}"),
        })?
        .compile_matcher();
    if value.contains('/') {
        Ok(GlobRule::WholePath(matcher))
    } else {
        Ok(GlobRule::BaseName(matcher))
    }
}

/// A path is searched where it matches any one `--glob` value, and every
/// path is searched where none was given.
pub fn glob_admits(rules: &[GlobRule], path: &str) -> bool {
    if rules.is_empty() {
        return true;
    }
    let base = path.rsplit('/').next().unwrap_or(path);
    rules.iter().any(|rule| match rule {
        GlobRule::BaseName(matcher) => matcher.is_match(base),
        GlobRule::WholePath(matcher) => matcher.is_match(path),
    })
}

/// The context window `--after-context`, `--before-context` and
/// `--context` name, in lines.
fn context_window(args: &GrepArgs) -> (usize, usize) {
    match args.context {
        Some(n) => (n as usize, n as usize),
        None => (
            args.before.unwrap_or(0) as usize,
            args.after.unwrap_or(0) as usize,
        ),
    }
}

/// Strips the line's terminator, and the carriage return a CRLF file
/// carries before it, so the line set and the `text` values are the same
/// on a CRLF file as on a newline one.
fn clean(line: &[u8]) -> String {
    let mut line = line;
    if line.last() == Some(&b'\n') {
        line = &line[..line.len() - 1];
    }
    if line.last() == Some(&b'\r') {
        line = &line[..line.len() - 1];
    }
    String::from_utf8_lossy(line).into_owned()
}

struct Collect {
    hits: Vec<(u64, String)>,
}

impl Sink for Collect {
    type Error = std::io::Error;

    fn matched(&mut self, _searcher: &Searcher, m: &SinkMatch<'_>) -> Result<bool, Self::Error> {
        for (number, line) in (m.line_number().unwrap_or(0)..).zip(m.lines()) {
            self.hits.push((number, clean(line)));
        }
        Ok(true)
    }
}

/// Searches one content for the matcher and attaches each match's
/// context lines.
pub fn search_text(
    matcher: &RegexMatcher,
    content: &str,
    before: usize,
    after: usize,
) -> Result<Vec<MatchRow>, CliError> {
    let mut searcher = SearcherBuilder::new().line_number(true).build();
    let mut sink = Collect { hits: Vec::new() };
    searcher
        .search_slice(matcher, content.as_bytes(), &mut sink)
        .map_err(|e| CliError::Io {
            message: format!("could not search the content: {e}"),
        })?;

    if before == 0 && after == 0 {
        return Ok(sink
            .hits
            .into_iter()
            .map(|(line, text)| MatchRow {
                line,
                text,
                context: Vec::new(),
            })
            .collect());
    }

    let table: Vec<&str> = content.lines().collect();
    let matched: HashSet<u64> = sink.hits.iter().map(|(line, _)| *line).collect();
    Ok(sink
        .hits
        .iter()
        .map(|(line, text)| {
            let start = line.saturating_sub(before as u64).max(1);
            let end = (line + after as u64).min(table.len() as u64);
            let context = (start..=end)
                .filter(|n| !matched.contains(n))
                .map(|n| ContextRow {
                    line: n,
                    text: table[(n - 1) as usize].to_string(),
                })
                .collect();
            MatchRow {
                line: *line,
                text: text.clone(),
                context,
            }
        })
        .collect())
}

/// How many rows one path's outcome carries under the mode in force —
/// matched lines under `content`, the path itself under `files` and
/// `count`.
fn rows_of(outcome: &PathOutcome, mode: GrepOutput) -> usize {
    match outcome {
        PathOutcome::Hits(rows) if !rows.is_empty() => match mode {
            GrepOutput::Content => rows.len(),
            GrepOutput::Files | GrepOutput::Count => 1,
        },
        _ => 0,
    }
}

pub async fn cmd_grep(
    config: &Config,
    output: &Output,
    pattern: String,
    args: GrepArgs,
    opts: ReadOptions,
) -> Result<(), CliError> {
    if let Some(document) = grep_document(config, output, pattern, args, opts).await?
        && output.is_json()
    {
        output.json(&document);
    }
    Ok(())
}

/// The whole search but the writing of its one document: the render and
/// the reference report outside `--json`, and the document answered —
/// or `None` where `--if-repo` skipped the run. A partial answer stays
/// the refusal carrying its document.
pub async fn grep_document(
    config: &Config,
    output: &Output,
    pattern: String,
    args: GrepArgs,
    opts: ReadOptions,
) -> Result<Option<serde_json::Value>, CliError> {
    // 1 — refuse an option combination outside the registered set, build
    // the matcher and compile every `--glob`, all before any request.
    refuse_option_combination(&args)?;
    let matcher = Arc::new(build_matcher(&pattern, args.ignore_case)?);
    let rules: Vec<GlobRule> = args
        .glob
        .iter()
        .map(|value| compile_glob(value))
        .collect::<Result<_, _>>()?;
    let (before, after) = context_window(&args);
    // Then bind the folder and map `--path` under it (SPEC u290).
    let folder = bind_read_folder(&opts)?;
    let mapped = repository_argument(folder.as_ref(), args.path.as_deref())?;
    let named = args
        .path
        .clone()
        .or_else(|| folder.as_ref().map(|f| f.path.clone()));

    // 2 — resolve the target.
    let Some(target) = resolve_read_target(config, output, &opts).await? else {
        return Ok(None);
    };
    let client = SynsClient::new(config.server_url())?;

    // 3 — read the tree recursively at that reference under `--path`.
    let version_ref = target.version_ref();
    let (response, _raw) = match client
        .get_tree(
            &target.repo_id,
            target.token.as_deref(),
            mapped.as_deref(),
            true,
            Some(&version_ref),
        )
        .await
    {
        Ok(tuple) => tuple,
        Err(e) => {
            if let Some(p) = named.as_ref() {
                if opts.version.is_some() {
                    return Err(read_not_found(e, &opts, &target.reference, p));
                }
                if !output.is_json() {
                    return Err(e.with_ls_path_context(p.clone()));
                }
            }
            return Err(e);
        }
    };
    let tree_truncated = response.truncated;

    // 4 — keep every entry of kind file matching some `--glob` over its
    // path counted from the folder (SPEC u290), ordered by ascending
    // path: each kept entry carries its repository path for the fetch
    // beside its counted path for every write.
    let scope = target.folder.as_ref();
    let mut kept: Vec<Kept> = response
        .entries
        .into_iter()
        .filter(|e| e.entry_type == EntryType::File)
        .map(|e| Kept {
            shown: counted_from(scope, &e.path),
            path: e.path,
            sha: e.sha,
        })
        .filter(|k| glob_admits(&rules, &k.shown))
        .collect();
    kept.sort_by(|a, b| a.shown.cmp(&b.shown));
    // The window `--offset` names, at most the fan-out cap wide
    // (`D-156`): only its paths are fetched.
    let total_kept = kept.len();
    let (start, end, capped) = window(total_kept, args.offset);
    kept.truncate(end);
    kept.drain(..start);

    // 5 — fetch through the store, searching each content as it lands
    // and closing the fan-out at the first content that carries the run
    // past `--head-limit`. A store the run cannot use is skipped without
    // a word (D-242 §11).
    let cache = Store::open(config, target.token.as_deref()).map(Arc::new);
    let client = Arc::new(client);
    let target = Arc::new(target);
    let kept = Arc::new(kept);

    let pool = kept.len();
    let mut results: Vec<Option<PathOutcome>> = (0..pool).map(|_| None).collect();
    let mut set: JoinSet<(usize, Result<PathOutcome, CliError>)> = JoinSet::new();
    let mut issued = 0usize;
    let mut carried = 0usize;
    let mut closed = false;

    macro_rules! fill {
        () => {
            while issued < pool && set.len() < IN_FLIGHT {
                let index = issued;
                issued += 1;
                let cache = cache.clone();
                let client = Arc::clone(&client);
                let target: Arc<ReadTarget> = Arc::clone(&target);
                let matcher = Arc::clone(&matcher);
                let kept = Arc::clone(&kept);
                set.spawn(async move {
                    let entry = &kept[index];
                    let answer = get_or_fetch(
                        cache.as_deref(),
                        &client,
                        &target,
                        &entry.path,
                        entry.sha.as_deref(),
                    )
                    .await;
                    let outcome = match answer {
                        Err(e) => return (index, Err(e)),
                        Ok(BlobContent::Binary) => PathOutcome::Binary,
                        Ok(BlobContent::Refused(_)) => PathOutcome::Refused,
                        Ok(BlobContent::Text(content)) => {
                            match search_text(&matcher, &content, before, after) {
                                Ok(rows) => PathOutcome::Hits(rows),
                                Err(e) => return (index, Err(e)),
                            }
                        }
                    };
                    (index, Ok(outcome))
                });
            }
        };
    }

    // A refusal outside the registered classes ends the run at its own
    // exit code, but only once the eviction pass has been taken — the
    // cap is a guarantee over every run that wrote to the store, the
    // refused ones among them (u270 CR1-5).
    let mut fatal: Option<CliError> = None;
    fill!();
    while let Some(joined) = set.join_next().await {
        let joined = joined.map_err(|e| CliError::Io {
            message: format!("a content fetch did not complete: {e}"),
        });
        let outcome = match joined {
            Ok((index, Ok(outcome))) => {
                carried += rows_of(&outcome, args.output);
                results[index] = Some(outcome);
                Ok(())
            }
            Ok((_, Err(e))) => Err(e),
            Err(e) => Err(e),
        };
        if let Err(e) = outcome {
            fatal = Some(e);
            break;
        }
        if let Some(limit) = args.head_limit
            && carried >= limit as usize
        {
            closed = true;
        }
        if !closed {
            fill!();
        }
    }
    let attempted = issued;

    // The run's hints merged and, where the store stands past its cap,
    // its one sweep, after its last fetch has landed.
    set.shutdown().await;
    if let Some(cache) = &cache {
        cache.finish();
    }
    if let Some(e) = fatal {
        return Err(e);
    }

    // Assemble in ascending path order — the pool is sorted, so the
    // index order is the path order.
    let mut skipped: Vec<(String, &'static str)> = Vec::new();
    let mut refused = 0usize;
    let mut first_refused: Option<String> = None;
    let mut hits: Vec<(String, Vec<MatchRow>)> = Vec::new();
    for (index, outcome) in results.into_iter().enumerate() {
        let Some(outcome) = outcome else { continue };
        let path = kept[index].shown.clone();
        match outcome {
            PathOutcome::Binary => skipped.push((path, "binary")),
            PathOutcome::Refused => {
                refused += 1;
                first_refused.get_or_insert_with(|| path.clone());
                skipped.push((path, "refused"));
            }
            PathOutcome::Hits(rows) => {
                if !rows.is_empty() {
                    hits.push((path, rows));
                }
            }
        }
    }

    // `--head-limit` cuts the sorted pool to the limit.
    let limit = args.head_limit.map(|n| n as usize);
    let truncated = tree_truncated || capped;
    // The next window starts at this one's end where kept paths stand
    // past it and this one was searched whole: every path fetched and no
    // row cut by `--head-limit`, so following it skips nothing (`D-156`).
    let searched_whole = attempted == pool && limit.is_none_or(|l| carried <= l);
    let next_offset = (capped && searched_whole).then_some(end);

    // 6 — write the render, or one document carrying the result.
    let mut document = serde_json::json!({
        "version": target.reference.version,
        "commitSha": target.reference.commit_sha,
        "pattern": pattern,
        "output": args.output.wire(),
        "truncated": truncated,
        "nextOffset": next_offset,
        "skipped": skipped
            .iter()
            .map(|(path, reason)| serde_json::json!({ "path": path, "reason": reason }))
            .collect::<Vec<_>>(),
    });

    match args.output {
        GrepOutput::Content => {
            let mut flat: Vec<(String, MatchRow)> = Vec::new();
            for (path, rows) in &hits {
                for row in rows {
                    flat.push((path.clone(), row.clone()));
                }
            }
            if let Some(limit) = limit {
                flat.truncate(limit);
            }
            document["matches"] = serde_json::Value::Array(
                flat.iter()
                    .map(|(path, row)| {
                        serde_json::json!({
                            "path": path,
                            "line": row.line,
                            "text": row.text,
                            "context": row.context
                                .iter()
                                .map(|c| serde_json::json!({ "line": c.line, "text": c.text }))
                                .collect::<Vec<_>>(),
                        })
                    })
                    .collect(),
            );
            if !output.is_json() {
                render_content(&flat, args.line_number, before > 0 || after > 0);
            }
        }
        GrepOutput::Files => {
            let mut paths: Vec<String> = hits.iter().map(|(path, _)| path.clone()).collect();
            if let Some(limit) = limit {
                paths.truncate(limit);
            }
            document["files"] = serde_json::Value::Array(
                paths
                    .iter()
                    .map(|p| serde_json::Value::from(p.as_str()))
                    .collect(),
            );
            if !output.is_json() {
                for path in &paths {
                    println!("{path}");
                }
            }
        }
        GrepOutput::Count => {
            let mut counts: Vec<(String, usize)> = hits
                .iter()
                .map(|(path, rows)| (path.clone(), rows.len()))
                .collect();
            if let Some(limit) = limit {
                counts.truncate(limit);
            }
            document["counts"] = serde_json::Value::Array(
                counts
                    .iter()
                    .map(|(path, count)| serde_json::json!({ "path": path, "count": count }))
                    .collect(),
            );
            if !output.is_json() {
                for (path, count) in &counts {
                    println!("{path}:{count}");
                }
            }
        }
    }

    // The truncated tree and a refused path each raise the
    // partial-answer refusal; the fan-out cap's own truncation and a
    // reached `--head-limit` each leave the run at exit `0`, and a
    // `binary` skip marks nothing.
    let refusal = if tree_truncated {
        Some(partial_truncated_tree(target.reference.version))
    } else if refused > 0 {
        Some(partial_unread_paths(
            refused,
            attempted,
            first_refused.as_deref().unwrap_or(""),
        ))
    } else {
        None
    };

    if !output.is_json() {
        // 7 — report the reference, and where kept paths stand past the
        // window, the offset and version the next call starts at (`D-156`).
        report_reference(output, &target.reference);
        if let Some(next) = next_offset {
            // Pinned to the version this page read: a commit between
            // pages would shift the kept list (D-156, verify F1).
            eprintln!(
                "this call took files {first} to {end} of the {total_kept} kept, in path order \
                 (at most {FAN_OUT_CAP} a call); continue with --offset {next} --version {version}",
                first = start + 1,
                version = target.reference.version,
            );
        }
    }

    if let Some(refusal) = refusal {
        document = mark_partial(document, &refusal);
        return Err(CliError::PartialAnswer {
            document,
            line: refusal,
        });
    }

    Ok(Some(document))
}

/// The content render: `{path}:{line}:{text}` where `--line-number`
/// stands and `{path}:{text}` where it does not, each context line the
/// same with `-` for each `:`, and — where a context option stands —
/// a line holding `--` between groups that are not adjacent.
fn render_content(flat: &[(String, MatchRow)], line_number: bool, context_stands: bool) {
    for line in content_lines(flat, line_number, context_stands) {
        println!("{line}");
    }
}

/// The content render's lines, in order, separators included where
/// `context_stands`. A render carrying no context line groups nothing,
/// so no separator divides one match from the next.
pub fn content_lines(
    flat: &[(String, MatchRow)],
    line_number: bool,
    context_stands: bool,
) -> Vec<String> {
    // Merge each path's matches and context into one ordered run: the
    // line number, whether that line matched, and its text.
    type Run = BTreeMap<u64, (bool, String)>;
    let mut by_path: Vec<(String, Run)> = Vec::new();
    for (path, row) in flat {
        let slot = match by_path.iter_mut().find(|(p, _)| p == path) {
            Some(slot) => slot,
            None => {
                by_path.push((path.clone(), BTreeMap::new()));
                by_path.last_mut().expect("just pushed")
            }
        };
        for context in &row.context {
            slot.1
                .entry(context.line)
                .or_insert((false, context.text.clone()));
        }
        slot.1.insert(row.line, (true, row.text.clone()));
    }

    let mut rendered: Vec<String> = Vec::new();
    let mut previous: Option<(String, u64)> = None;
    for (path, run) in &by_path {
        for (number, (is_match, text)) in run {
            let adjacent = matches!(
                &previous,
                Some((prev_path, prev_line)) if prev_path == path && *number == prev_line + 1
            );
            if context_stands && previous.is_some() && !adjacent {
                rendered.push("--".to_string());
            }
            let separator = if *is_match { ':' } else { '-' };
            if line_number {
                rendered.push(format!("{path}{separator}{number}{separator}{text}"));
            } else {
                rendered.push(format!("{path}{separator}{text}"));
            }
            previous = Some((path.clone(), *number));
        }
    }
    rendered
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::push::hash::blob_sha1;
    use serial_test::serial;
    use tempfile::TempDir;
    use wiremock::matchers::{method, path as path_matcher, path_regex};
    use wiremock::{Mock, MockServer, Request, ResponseTemplate};

    const HEAD_SHA: &str = "def4560000000000000000000000000000000000";

    fn named() -> ReadOptions {
        ReadOptions {
            repo: Some("alice/notes".into()),
            version: None,
            if_repo: false,
        }
    }

    /// Each path of the synthetic tree holds its own content, so no two
    /// paths share a blob hash and the cache never stands in for a
    /// request the fan-out would otherwise make.
    fn body_for(index: usize) -> String {
        format!("fn one // {index}\n")
    }

    /// The same, but matching `fn ` only below `matching`.
    fn body_matching(index: usize, matching: usize) -> String {
        if index < matching {
            body_for(index)
        } else {
            format!("plain // {index}\n")
        }
    }

    struct FanOutEnv {
        server: MockServer,
        config: Config,
        _config_dir: TempDir,
        _cache_dir: TempDir,
    }

    impl FanOutEnv {
        async fn file_calls(&self) -> usize {
            self.server
                .received_requests()
                .await
                .unwrap()
                .iter()
                .filter(|r| r.url.path().contains("/files/"))
                .count()
        }

        fn release(self) {
            unsafe { std::env::remove_var("SYNS_CONFIG_DIR") };
            unsafe { std::env::remove_var("SYNS_CACHE_DIR") };
        }
    }

    async fn fan_out_env(paths: usize) -> FanOutEnv {
        fan_out_env_matching(paths, paths).await
    }

    /// A tree of `paths` files of which only the first `matching` hold a
    /// match; every other content is answered after a delay, so the
    /// matching ones land first and the fetches past them are in flight.
    async fn fan_out_env_matching(paths: usize, matching: usize) -> FanOutEnv {
        let config_dir = tempfile::tempdir().unwrap();
        let cache_dir = tempfile::tempdir().unwrap();
        unsafe { std::env::set_var("SYNS_CONFIG_DIR", config_dir.path()) };
        unsafe { std::env::set_var("SYNS_CACHE_DIR", cache_dir.path()) };

        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path_matcher("/api/v1/repos/alice/notes"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "owner": "alice", "name": "notes", "description": null,
                "commitSha": HEAD_SHA, "status": "active", "author": null, "tags": [],
                "visibility": "public", "forkedFrom": null, "forkCount": 0,
                "fileCount": paths, "role": null,
                "createdAt": "2026-01-01T00:00:00Z", "updatedAt": "2026-01-01T00:00:00Z",
            })))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path_matcher(format!(
                "/api/v1/repos/alice/notes/versions/{HEAD_SHA}"
            )))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "version": 7, "sha": HEAD_SHA, "parentSha": null, "message": "m",
                "messageBody": null, "author": "alice",
                "createdAt": "2026-01-01T00:00:00Z", "filesChanged": [],
            })))
            .mount(&server)
            .await;

        let entries: Vec<serde_json::Value> = (0..paths)
            .map(|index| {
                let content = body_matching(index, matching);
                serde_json::json!({
                    "name": format!("f{index:04}.ts"),
                    "path": format!("src/f{index:04}.ts"),
                    "type": "file",
                    "size": content.len(),
                    "sha": blob_sha1(content.as_bytes()),
                })
            })
            .collect();
        Mock::given(method("GET"))
            .and(path_matcher("/api/v1/repos/alice/notes/tree"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "entries": entries, "commitSha": HEAD_SHA, "truncated": false,
            })))
            .mount(&server)
            .await;

        Mock::given(method("GET"))
            .and(path_regex(
                r"^/api/v1/repos/alice/notes/files/src/f\d+\.ts$",
            ))
            .respond_with(move |request: &Request| {
                let path = request.url.path().to_string();
                let stem = path.rsplit('/').next().unwrap_or_default().to_string();
                let index: usize = stem
                    .trim_start_matches('f')
                    .trim_end_matches(".ts")
                    .parse()
                    .unwrap_or(0);
                let content = body_matching(index, matching);
                let answer = ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "path": format!("src/{stem}"),
                    "sha": blob_sha1(content.as_bytes()),
                    "content": content,
                    "size": content.len(),
                }));
                if index < matching {
                    answer
                } else {
                    answer.set_delay(std::time::Duration::from_millis(50))
                }
            })
            .mount(&server)
            .await;

        let config = Config::new(Some(&server.uri())).unwrap();
        FanOutEnv {
            server,
            config,
            _config_dir: config_dir,
            _cache_dir: cache_dir,
        }
    }

    fn args(output: GrepOutput) -> GrepArgs {
        GrepArgs {
            output,
            ..GrepArgs::default()
        }
    }

    // SPEC u270 Contract Surface, the matcher build: a pattern holding a
    // newline is refused at compile rather than compiling and matching
    // nothing.
    #[test]
    fn a_newline_bearing_pattern_is_refused_at_compile() {
        let err = build_matcher("one\\nfn", false).unwrap_err();
        assert!(err.to_string().contains("one\\nfn"), "{err}");
        assert_eq!(err.exit_code(), 1);
        assert!(build_matcher("fn ", false).is_ok());
    }

    // `SPEC_REVIEW_R3.md` CF-02: a `--glob` value holding no path
    // separator matches the base name at every depth; one holding a
    // separator is anchored at the repository root.
    #[test]
    fn a_glob_value_without_a_separator_matches_at_every_depth() {
        let rules = vec![compile_glob("*.ts").unwrap()];
        assert!(glob_admits(&rules, "src/a.ts"));
        assert!(glob_admits(&rules, "src/deep/b.ts"));
        assert!(!glob_admits(&rules, "README.md"));

        let anchored = vec![compile_glob("src/*.ts").unwrap()];
        assert!(glob_admits(&anchored, "src/a.ts"));
        assert!(!glob_admits(&anchored, "src/deep/b.ts"));
        assert!(!glob_admits(&anchored, "a.ts"));
    }

    #[test]
    fn no_glob_value_admits_every_path() {
        assert!(glob_admits(&[], "anything/at/all.bin"));
    }

    #[test]
    fn several_glob_values_admit_a_path_matching_any_one() {
        let rules = vec![
            compile_glob("*.ts").unwrap(),
            compile_glob("docs/*.md").unwrap(),
        ];
        assert!(glob_admits(&rules, "src/a.ts"));
        assert!(glob_admits(&rules, "docs/a.md"));
        assert!(!glob_admits(&rules, "deep/docs/a.md"));
    }

    // SPEC u270 Contract Surface, the option-combination refusal.
    #[test]
    fn the_context_options_stand_only_beside_output_content() {
        for (mutate, option) in [
            (
                Box::new(|a: &mut GrepArgs| a.line_number = true) as Box<dyn Fn(&mut GrepArgs)>,
                "--line-number",
            ),
            (
                Box::new(|a: &mut GrepArgs| a.after = Some(2)),
                "--after-context",
            ),
            (
                Box::new(|a: &mut GrepArgs| a.before = Some(2)),
                "--before-context",
            ),
            (
                Box::new(|a: &mut GrepArgs| a.context = Some(2)),
                "--context",
            ),
        ] {
            let mut files = args(GrepOutput::Files);
            mutate(&mut files);
            let err = refuse_option_combination(&files).unwrap_err();
            assert_eq!(
                err.to_string(),
                format!("configuration error: {option} applies only under --output content")
            );

            let mut content = args(GrepOutput::Content);
            mutate(&mut content);
            assert!(refuse_option_combination(&content).is_ok());
        }
    }

    #[test]
    fn context_cannot_stand_beside_after_or_before_context() {
        let mut a = args(GrepOutput::Content);
        a.context = Some(1);
        a.after = Some(1);
        let err = refuse_option_combination(&a).unwrap_err();
        assert_eq!(
            err.to_string(),
            "configuration error: --context cannot stand beside --after-context or --before-context"
        );

        let mut b = args(GrepOutput::Content);
        b.before = Some(1);
        b.after = Some(1);
        assert!(refuse_option_combination(&b).is_ok());
    }

    // SPEC u270 Contract Surface, the matcher build: the line set, the
    // line numbers and the counts are the same on a CRLF file, on a file
    // ending with no terminator, and in every output mode.
    #[test]
    fn a_crlf_file_answers_the_same_lines_as_a_newline_one() {
        let matcher = build_matcher("fn ", false).unwrap();
        let crlf = "fn one\r\nplain\r\nfn two\r\n";
        let newline = "fn one\nplain\nfn two";
        let from_crlf = search_text(&matcher, crlf, 0, 0).unwrap();
        let from_newline = search_text(&matcher, newline, 0, 0).unwrap();

        let shape = |rows: &[MatchRow]| -> Vec<(u64, String)> {
            rows.iter().map(|r| (r.line, r.text.clone())).collect()
        };
        assert_eq!(shape(&from_crlf), shape(&from_newline));
        assert_eq!(
            shape(&from_crlf),
            vec![(1, "fn one".into()), (3, "fn two".into())]
        );
        assert!(from_crlf.iter().all(|r| !r.text.contains('\r')));
    }

    #[test]
    fn a_match_carries_the_context_lines_that_are_not_matches() {
        let matcher = build_matcher("fn ", false).unwrap();
        let rows = search_text(&matcher, "a\nfn one\nb\nfn two\nc\n", 1, 1).unwrap();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].line, 2);
        assert_eq!(
            rows[0].context.iter().map(|c| c.line).collect::<Vec<_>>(),
            vec![1, 3]
        );
        assert_eq!(
            rows[1].context.iter().map(|c| c.line).collect::<Vec<_>>(),
            vec![3, 5]
        );
    }

    #[test]
    fn the_context_window_takes_context_over_after_and_before() {
        let mut a = GrepArgs {
            after: Some(3),
            ..GrepArgs::default()
        };
        assert_eq!(context_window(&a), (0, 3));
        a.before = Some(2);
        assert_eq!(context_window(&a), (2, 3));
        let c = GrepArgs {
            context: Some(1),
            ..GrepArgs::default()
        };
        assert_eq!(context_window(&c), (1, 1));
    }

    // SPEC u270 Tests: `grep_content_render_pipes_line_for_line` —
    // `SPEC_REVIEW_R3.md` QF-02 puts the separator between the group
    // closing one path and the group opening the next.
    #[test]
    fn the_content_render_holds_a_separator_between_groups_that_are_not_adjacent() {
        let matcher = build_matcher("fn ", false).unwrap();
        let a = search_text(&matcher, "plain\nfn one\ntail\n", 1, 1).unwrap();
        let b = search_text(&matcher, "fn two\nplain\nfn three\n", 1, 1).unwrap();
        let flat: Vec<(String, MatchRow)> = a
            .into_iter()
            .map(|row| ("src/a.ts".to_string(), row))
            .chain(b.into_iter().map(|row| ("src/b.ts".to_string(), row)))
            .collect();

        let lines = content_lines(&flat, true, true);
        assert_eq!(
            lines,
            vec![
                "src/a.ts-1-plain",
                "src/a.ts:2:fn one",
                "src/a.ts-3-tail",
                "--",
                "src/b.ts:1:fn two",
                "src/b.ts-2-plain",
                "src/b.ts:3:fn three",
            ]
        );
    }

    #[test]
    fn the_content_render_drops_the_line_number_where_the_option_does_not_stand() {
        let matcher = build_matcher("fn ", false).unwrap();
        let rows = search_text(&matcher, "fn one\n", 0, 0).unwrap();
        let flat: Vec<(String, MatchRow)> = rows
            .into_iter()
            .map(|row| ("a.ts".to_string(), row))
            .collect();
        assert_eq!(content_lines(&flat, false, false), vec!["a.ts:fn one"]);
    }

    #[test]
    fn a_separator_stands_between_two_non_adjacent_groups_of_one_path() {
        let matcher = build_matcher("fn ", false).unwrap();
        let rows = search_text(&matcher, "fn one\nx\ny\nz\nfn two\n", 0, 1).unwrap();
        let flat: Vec<(String, MatchRow)> = rows
            .into_iter()
            .map(|row| ("a.ts".to_string(), row))
            .collect();
        assert_eq!(
            content_lines(&flat, true, true),
            vec!["a.ts:1:fn one", "a.ts-2-x", "--", "a.ts:5:fn two",]
        );
    }

    // u270 V1-05: `rg` writes no separator where no context option
    // stands, whatever the gap between two matches and whether they sit
    // in one path or in two, so neither does the render that mirrors it.
    #[test]
    fn no_separator_stands_where_no_context_option_does() {
        let matcher = build_matcher("fn ", false).unwrap();
        let a = search_text(&matcher, "fn one\nx\ny\nz\nfn two\n", 0, 0).unwrap();
        let b = search_text(&matcher, "fn three\n", 0, 0).unwrap();
        let flat: Vec<(String, MatchRow)> = a
            .into_iter()
            .map(|row| ("a.ts".to_string(), row))
            .chain(b.into_iter().map(|row| ("b.ts".to_string(), row)))
            .collect();
        assert_eq!(
            content_lines(&flat, true, false),
            vec!["a.ts:1:fn one", "a.ts:5:fn two", "b.ts:1:fn three"]
        );
    }

    // SPEC u270 Contract Surface, the grep fan-out cap: a kept set
    // larger than it leaves the answer marked `truncated` and the run at
    // exit `0`, and a path past it is never asked for.
    #[tokio::test]
    #[serial]
    async fn the_fan_out_stops_at_the_cap_over_a_larger_tree() {
        let env = fan_out_env(500).await;
        let output = Output::new(false);
        let result = cmd_grep(
            &env.config,
            &output,
            "fn ".to_string(),
            args(GrepOutput::Files),
            named(),
        )
        .await;
        let file_calls = env.file_calls().await;
        env.release();

        assert!(
            result.is_ok(),
            "the cap leaves the run at exit 0: {result:?}"
        );
        assert_eq!(file_calls, FAN_OUT_CAP);
    }

    // SPEC u270 Contract Surface, `--head-limit`: the fan-out closes at
    // the first content carrying the run past it, so a path past it is
    // never asked for.
    #[tokio::test]
    #[serial]
    async fn head_limit_closes_the_fan_out_short_of_the_cap() {
        let env = fan_out_env(500).await;
        let output = Output::new(false);
        let result = cmd_grep(
            &env.config,
            &output,
            "fn ".to_string(),
            GrepArgs {
                head_limit: Some(3),
                ..args(GrepOutput::Content)
            },
            named(),
        )
        .await;
        let file_calls = env.file_calls().await;
        env.release();

        assert!(result.is_ok(), "{result:?}");
        assert!(
            file_calls < FAN_OUT_CAP,
            "the close left {file_calls} calls, not fewer than {FAN_OUT_CAP}"
        );
        assert!(
            file_calls >= 3,
            "every landed path is carried: {file_calls}"
        );
    }

    /// The paths a `files` document names, in its order.
    fn files_of(document: &serde_json::Value) -> Vec<String> {
        document["files"]
            .as_array()
            .expect("files")
            .iter()
            .map(|p| p.as_str().unwrap_or_default().to_string())
            .collect()
    }

    // D-156, issue 260: `--offset` pages the kept paths in windows of
    // the cap — offset 0 fetches the first 400 and names 400 as the next
    // offset, offset 400 fetches the last 100 and names none, and
    // following `nextOffset` from 0 visits every kept path exactly once.
    #[tokio::test]
    #[serial]
    async fn following_next_offset_searches_every_kept_path_exactly_once() {
        let env = fan_out_env(500).await;
        let output = Output::new(true);

        let first = grep_document(
            &env.config,
            &output,
            "fn ".to_string(),
            args(GrepOutput::Files),
            named(),
        )
        .await;
        let first_calls = env.file_calls().await;
        let second = grep_document(
            &env.config,
            &output,
            "fn ".to_string(),
            GrepArgs {
                offset: 400,
                ..args(GrepOutput::Files)
            },
            named(),
        )
        .await;
        let second_calls = env.file_calls().await - first_calls;
        env.release();

        let first = first.expect("offset 0 exits 0").expect("a document");
        let second = second.expect("offset 400 exits 0").expect("a document");
        assert_eq!(first_calls, FAN_OUT_CAP);
        assert_eq!(first["nextOffset"], serde_json::json!(400));
        assert_eq!(first["truncated"], serde_json::json!(true));
        assert_eq!(second_calls, 100);
        assert_eq!(second["nextOffset"], serde_json::Value::Null);
        assert_eq!(second["truncated"], serde_json::json!(false));

        let mut union = files_of(&first);
        union.extend(files_of(&second));
        let every: Vec<String> = (0..500).map(|i| format!("src/f{i:04}.ts")).collect();
        assert_eq!(union, every, "each kept path once, in ascending order");
    }

    // D-156: an offset at or past the kept count answers no rows and no
    // next offset, and fetches nothing.
    #[tokio::test]
    #[serial]
    async fn an_offset_at_or_past_the_kept_count_answers_no_rows() {
        let env = fan_out_env(500).await;
        let output = Output::new(true);
        let mut answers = Vec::new();
        for offset in [500, 600, u32::MAX] {
            answers.push(
                grep_document(
                    &env.config,
                    &output,
                    "fn ".to_string(),
                    GrepArgs {
                        offset,
                        ..args(GrepOutput::Files)
                    },
                    named(),
                )
                .await,
            );
        }
        let file_calls = env.file_calls().await;
        env.release();

        assert_eq!(file_calls, 0);
        for answer in answers {
            let document = answer.expect("exit 0").expect("a document");
            assert!(files_of(&document).is_empty(), "{document}");
            assert_eq!(document["nextOffset"], serde_json::Value::Null);
            assert_eq!(document["truncated"], serde_json::json!(false));
        }
    }

    // D-156: `--head-limit` closing the fan-out early answers
    // `nextOffset: null`, so following it never skips a path inside a
    // window — the rows equal to the limit tell the caller.
    #[tokio::test]
    #[serial]
    async fn head_limit_closing_the_fan_out_answers_no_next_offset() {
        let env = fan_out_env(500).await;
        let output = Output::new(true);
        let answer = grep_document(
            &env.config,
            &output,
            "fn ".to_string(),
            GrepArgs {
                head_limit: Some(3),
                ..args(GrepOutput::Content)
            },
            named(),
        )
        .await;
        env.release();

        let document = answer.expect("exit 0").expect("a document");
        assert_eq!(document["matches"].as_array().map(Vec::len), Some(3));
        let next = document.get("nextOffset").expect("the key stands");
        assert_eq!(next, &serde_json::Value::Null);
        assert_eq!(document["truncated"], serde_json::json!(true));
    }

    // D-156, "the window was searched whole": a `--head-limit` met only
    // by the window's last path cuts no row, so the next window is named;
    // one row fewer cuts a row and names none.
    #[tokio::test]
    #[serial]
    async fn a_head_limit_cutting_no_row_still_names_the_next_window() {
        let env = fan_out_env(500).await;
        let output = Output::new(true);
        let mut answers = Vec::new();
        for limit in [400, 399] {
            answers.push(
                grep_document(
                    &env.config,
                    &output,
                    "fn ".to_string(),
                    GrepArgs {
                        head_limit: Some(limit),
                        ..args(GrepOutput::Files)
                    },
                    named(),
                )
                .await,
            );
        }
        env.release();

        let mut answers = answers.into_iter();
        let met = answers
            .next()
            .unwrap()
            .expect("exit 0")
            .expect("a document");
        assert_eq!(files_of(&met).len(), 400);
        assert_eq!(met["nextOffset"], serde_json::json!(400));
        let cut = answers
            .next()
            .unwrap()
            .expect("exit 0")
            .expect("a document");
        assert_eq!(files_of(&cut).len(), 399);
        assert_eq!(cut["nextOffset"], serde_json::Value::Null);
    }

    // D-156, "never skips a path inside a window": `--head-limit` met
    // exactly by the first rows closes the fan-out while later window
    // paths were never asked for, and the fetches still in flight land
    // with no row — no row is cut, yet the window was not searched whole,
    // so no next window is named.
    #[tokio::test]
    #[serial]
    async fn a_head_limit_met_before_the_window_is_fetched_names_no_next_window() {
        let env = fan_out_env_matching(500, 2).await;
        let output = Output::new(true);
        let answer = grep_document(
            &env.config,
            &output,
            "fn ".to_string(),
            GrepArgs {
                head_limit: Some(2),
                ..args(GrepOutput::Files)
            },
            named(),
        )
        .await;
        let file_calls = env.file_calls().await;
        env.release();

        let document = answer.expect("exit 0").expect("a document");
        assert_eq!(files_of(&document), vec!["src/f0000.ts", "src/f0001.ts"]);
        assert!(
            file_calls < FAN_OUT_CAP,
            "the close left window paths unfetched: {file_calls} calls"
        );
        assert_eq!(document["nextOffset"], serde_json::Value::Null);
        assert_eq!(document["truncated"], serde_json::json!(true));
    }

    // D-156: a kept set inside one window names no next offset.
    #[tokio::test]
    #[serial]
    async fn a_kept_set_inside_one_window_answers_no_next_offset() {
        let env = fan_out_env(12).await;
        let output = Output::new(true);
        let answer = grep_document(
            &env.config,
            &output,
            "fn ".to_string(),
            args(GrepOutput::Count),
            named(),
        )
        .await;
        env.release();

        let document = answer.expect("exit 0").expect("a document");
        assert_eq!(document["counts"].as_array().map(Vec::len), Some(12));
        assert_eq!(document["nextOffset"], serde_json::Value::Null);
        assert_eq!(document["truncated"], serde_json::json!(false));
    }

    // The window bounds over a kept count: `[start, end)` and whether
    // kept paths stand past the window.
    #[test]
    fn the_window_is_clamped_to_the_kept_count() {
        assert_eq!(window(500, 0), (0, 400, true));
        assert_eq!(window(500, 400), (400, 500, false));
        assert_eq!(window(500, 100), (100, 500, false));
        assert_eq!(window(801, 400), (400, 800, true));
        assert_eq!(window(500, 600), (500, 500, false));
        assert_eq!(window(0, 0), (0, 0, false));
        assert_eq!(window(10, u32::MAX), (10, 10, false));
    }
}
