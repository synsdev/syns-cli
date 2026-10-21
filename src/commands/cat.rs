use std::io::{Read, Write};
use std::sync::Arc;

use base64::display::Base64Display;
use base64::engine::general_purpose::STANDARD;
use serde::Serialize;

use crate::client::{RawFile, SynsClient};
use crate::config::Config;
use crate::errors::CliError;
use crate::output::Output;
use crate::push::collector::is_text;
use crate::read::files::{
    Distinct, Outcome, PathResult, Policy, Stop, per_file, read_many, read_path,
};
use crate::read::store::Store;
use crate::read::{
    ReadOptions, ResolvedRef, bind_read_folder, bind_repository, listed_argument, read_not_found,
    report_reference, repository_argument, resolve_read_target,
};

/// The generic byte type, which a `--json` document never names.
const GENERIC_BYTE_TYPE: &str = "application/octet-stream";

/// The one document `syns cat PATH --json` writes (SPEC u283 Contract
/// Surface, `CatDocument`): `content` for bytes passing `is_text`, beside
/// a `null` `mediaType`, and `contentBase64` for any other, `mediaType`
/// standing only where the answer named a type other than the generic
/// byte type. Keys are declared in lexical order, the order they are
/// written in.
#[derive(Serialize, Debug)]
#[serde(rename_all = "camelCase")]
pub struct CatDocument {
    commit_sha: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    content: Option<String>,
    #[serde(
        skip_serializing_if = "Option::is_none",
        serialize_with = "base64_as_written"
    )]
    content_base64: Option<Vec<u8>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    media_type: Option<Option<String>>,
    path: String,
    sha: String,
    size: u64,
    version: u32,
}

/// Writes the bytes as standard padded base64 straight into the stream
/// the document is written to, building no encoded copy.
fn base64_as_written<S: serde::Serializer>(
    bytes: &Option<Vec<u8>>,
    serializer: S,
) -> Result<S::Ok, S::Error> {
    match bytes {
        Some(bytes) => serializer.collect_str(&Base64Display::new(bytes, &STANDARD)),
        None => serializer.serialize_none(),
    }
}

impl CatDocument {
    /// The document for one raw answer at the run's reference, the bytes
    /// moved into it rather than copied. `sha` is their blob hash.
    pub fn new(path: String, raw: RawFile, sha: String, reference: &ResolvedRef) -> CatDocument {
        let size = raw.bytes.len() as u64;
        let (content, content_base64, media_type) = if is_text(&raw.bytes) {
            let text = String::from_utf8(raw.bytes).expect("is_text admits valid UTF-8 alone");
            (Some(text), None, Some(None))
        } else {
            let named = raw
                .media_type
                .filter(|media_type| media_type != GENERIC_BYTE_TYPE && !media_type.is_empty());
            (None, Some(raw.bytes), named.map(Some))
        };
        CatDocument {
            commit_sha: reference.commit_sha.clone(),
            content,
            content_base64,
            media_type,
            path,
            sha,
            size,
            version: reference.version,
        }
    }
}

/// `syns cat PATH` — the bytes cross the primary stream unframed and
/// unrendered whatever reference the run resolved (SPEC u270), and under
/// `--json` as one document carrying them whole (SPEC u283). The read goes
/// through the store (roadmap 278): a verified blob is asked with
/// `If-None-Match`, and a `304` writes what the `200` wrote.
pub async fn cmd_cat(
    config: &Config,
    output: &Output,
    path: String,
    opts: ReadOptions,
) -> Result<(), CliError> {
    // 1 — bind the folder and map the positional under it (SPEC u290);
    // the positional as typed names the path in every refusal and in
    // the document.
    let folder = bind_read_folder(&opts)?;
    let mapped = repository_argument(folder.as_ref(), Some(&path))?.unwrap_or_default();

    // 2 — resolve the target.
    let Some(target) = resolve_read_target(config, output, &opts).await? else {
        return Ok(());
    };
    let client = SynsClient::new(config.server_url())?;
    let store = Store::open(config, target.token.as_deref());

    // 2 — read the path at that reference through the store and the raw
    // entry in either mode, sending the pinned ordinal: its bytes are the
    // stored ones exactly (SPEC u280 `cmd_cat` 1, SPEC u283 `cmd_cat` 2),
    // the ones the `ETag` names (step 3), or none is printed.
    let refused = |e: CliError| -> CliError {
        if opts.version.is_some() {
            return read_not_found(e, &opts, &target.reference, &path);
        }
        if !output.is_json() {
            return e.with_cat_path_context(path.clone());
        }
        e
    };
    let policy = Policy {
        identity: false,
        want_type: output.is_json(),
    };
    let read = read_path(
        &client,
        &target,
        store.as_ref(),
        &mapped,
        &path,
        policy,
        None,
    )
    .await;
    if let Some(store) = &store {
        store.finish();
    }
    let answer = match read.map_err(refused)? {
        Outcome::Read(answer) => answer,
        Outcome::Deferred => unreachable!("a read with no budget is never deferred"),
    };

    // 4 — the bytes unchanged, and no byte more; or under `--json` the
    // one document, written as it is serialised. A write the stream
    // refuses ends the run as `print!` ends it.
    let stdout = std::io::stdout();
    let written = if output.is_json() {
        let raw = RawFile {
            bytes: answer.bytes,
            etag: None,
            media_type: answer.media_type,
        };
        let document = CatDocument::new(path, raw, answer.sha, &target.reference);
        let mut stream = std::io::BufWriter::with_capacity(1 << 16, stdout.lock());
        serde_json::to_writer_pretty(&mut stream, &document)
            .map_err(std::io::Error::from)
            .and_then(|()| stream.write_all(b"\n"))
            .and_then(|()| stream.flush())
    } else {
        let mut lock = stdout.lock();
        lock.write_all(&answer.bytes).and_then(|()| lock.flush())
    };
    if let Err(err) = written {
        panic!("failed printing to stdout: {err}");
    }

    // 5 — report the reference.
    report_reference(output, &target.reference);

    Ok(())
}

/// What `syns cat` was given beside the shared read options (roadmap
/// 278): the positional paths, `--paths-from=-`, and `--max-bytes`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CatPaths {
    pub paths: Vec<String>,
    pub from_stdin: bool,
    pub max_bytes: Option<u64>,
}

/// `syns cat PATH…`: one path with neither `--paths-from` nor
/// `--max-bytes` answers as `cmd_cat`; any other run reads every path at
/// the one commit the run resolves, at most `IN_FLIGHT` at once, and
/// writes the `files` document under `--json` or each file's bytes in the
/// order asked (Q-0147 defaults 2 and 3, D-242 §6).
pub async fn cmd_cat_paths(
    config: &Config,
    output: &Output,
    request: CatPaths,
    opts: ReadOptions,
) -> Result<(), CliError> {
    if request.paths.len() == 1 && !request.from_stdin && request.max_bytes.is_none() {
        let path = request.paths.into_iter().next().unwrap_or_default();
        return cmd_cat(config, output, path, opts).await;
    }
    cmd_cat_many(config, output, request, opts).await
}

/// The paths standard input names: split on `\n`, one trailing `\r` per
/// line taken off and empty lines skipped; input that is not UTF-8 is a
/// configuration error (D-242 §6).
pub fn paths_from_input(input: &[u8]) -> Result<Vec<String>, CliError> {
    let text = std::str::from_utf8(input).map_err(|_| CliError::Config {
        message: "standard input is not UTF-8: --paths-from=- reads one UTF-8 path per line"
            .to_string(),
    })?;
    Ok(text
        .split('\n')
        .map(|line| line.strip_suffix('\r').unwrap_or(line))
        .filter(|line| !line.is_empty())
        .map(str::to_string)
        .collect())
}

/// The `files` document (D-242 §6): members in lexical order.
#[derive(Serialize, Debug)]
#[serde(rename_all = "camelCase")]
struct FilesDocument<'a> {
    commit_sha: &'a str,
    deferred: Vec<&'a str>,
    files: Vec<FileEntry<'a>>,
    version: u32,
}

#[derive(Serialize, Debug)]
#[serde(untagged)]
enum FileEntry<'a> {
    Read(ReadEntry<'a>),
    Failed(FailedEntry<'a>),
}

/// A read file: `content` where its bytes pass `is_text`, and standard
/// padded `contentBase64` otherwise; no `mediaType`.
#[derive(Serialize, Debug)]
#[serde(rename_all = "camelCase")]
struct ReadEntry<'a> {
    #[serde(skip_serializing_if = "Option::is_none")]
    content: Option<&'a str>,
    #[serde(
        skip_serializing_if = "Option::is_none",
        serialize_with = "base64_slice"
    )]
    content_base64: Option<&'a [u8]>,
    path: &'a str,
    sha: &'a str,
    size: u64,
}

#[derive(Serialize, Debug)]
struct FailedEntry<'a> {
    error: EntryError,
    path: &'a str,
}

#[derive(Serialize, Debug)]
struct EntryError {
    code: String,
    message: String,
}

fn base64_slice<S: serde::Serializer>(
    bytes: &Option<&[u8]>,
    serializer: S,
) -> Result<S::Ok, S::Error> {
    match bytes {
        Some(bytes) => serializer.collect_str(&Base64Display::new(bytes, &STANDARD)),
        None => serializer.serialize_none(),
    }
}

/// What reading the `files` answer over the batch came to.
enum Batched {
    /// `--if-repo` skipped the run.
    Skipped,
    /// The server answers no v2: the target where it was resolved.
    NoV2(Option<Box<crate::read::ReadTarget>>),
    Read(Arc<crate::read::ReadTarget>, crate::read::files::Many),
}

/// The `files` answer's reads over `POST …/read` (roadmap 279, D-243 §8):
/// unpinned, the first chunk asks `head` and its header pins the run; with
/// `--version`, today's resolution, then every chunk at that commit. A
/// path the batch leaves out twice, or whose stored blob is gone, is read
/// alone through 278's raw read. None where the server answers no v2.
async fn cat_many_batch(
    config: &Config,
    output: &Output,
    opts: &ReadOptions,
    distinct: &[Distinct],
    slots: &[Option<usize>],
    budget: Option<u64>,
) -> Result<Option<Batched>, CliError> {
    use crate::read::{ReadTarget, files::read_many_batch};
    let client = SynsClient::new(config.server_url())?;
    let (target, at, repo_id, folder) = match &opts.version {
        Some(_) => {
            let Some(target) =
                crate::read::resolve_read_target_through(config, output, opts, false).await?
            else {
                return Ok(Some(Batched::Skipped));
            };
            let at = target.reference.commit_sha.clone();
            let (repo_id, folder) = (target.repo_id.clone(), target.folder.clone());
            (Some(target), at, repo_id, folder)
        }
        None => {
            let Some((repo_id, folder)) =
                bind_repository(opts.repo.as_deref(), opts.if_repo, output)?
            else {
                return Ok(Some(Batched::Skipped));
            };
            (None, "head".to_string(), repo_id, folder)
        }
    };
    let token = match &target {
        Some(target) => target.token.clone(),
        None => crate::auth::token::TokenStore::new(config.credentials_path())
            .read()
            .ok()
            .flatten(),
    };
    let store = Store::open(config, token.as_deref());
    let Some(batch) = read_many_batch(
        &client,
        &repo_id,
        token.as_deref(),
        &at,
        store.as_ref(),
        distinct,
        slots,
        budget,
    )
    .await?
    else {
        if let Some(store) = &store {
            store.finish();
        }
        return Ok(Some(Batched::NoV2(target.map(Box::new))));
    };
    let target = match target {
        Some(target) => target,
        // The first chunk gave up before any answer named the head: the
        // reference from one v2 head, each path the chunk's class.
        None if batch.header.is_none() => {
            match crate::read::resolve_read_target_through(config, output, opts, true).await? {
                Some(target) => target,
                None => return Ok(Some(Batched::Skipped)),
            }
        }
        None => {
            let Some(header) = &batch.header else {
                return Ok(None);
            };
            let version = match header.version {
                Some(version) => version,
                None => {
                    client
                        .get_version(&repo_id, token.as_deref(), &header.commit)
                        .await?
                        .0
                        .version
                }
            };
            ReadTarget {
                repo_id,
                token,
                reference: ResolvedRef {
                    version,
                    commit_sha: header.commit.clone(),
                },
                folder,
            }
        }
    };
    let mut results = batch.results;
    for index in batch.alone {
        let path = &distinct[index];
        results[index] = match read_path(
            &client,
            &target,
            store.as_ref(),
            &path.mapped,
            &path.typed,
            Policy::default(),
            None,
        )
        .await
        {
            Ok(Outcome::Read(answer)) => PathResult::Read(answer),
            Ok(Outcome::Deferred) => PathResult::Deferred,
            Err(err) => PathResult::Failed(err),
        };
    }
    if let Some(store) = &store {
        store.finish();
    }
    Ok(Some(Batched::Read(
        Arc::new(target),
        crate::read::files::Many {
            results,
            cut: batch.cut,
        },
    )))
}

/// `syns cat PATH…` past one path (roadmap 278).
async fn cmd_cat_many(
    config: &Config,
    output: &Output,
    request: CatPaths,
    opts: ReadOptions,
) -> Result<(), CliError> {
    // Under `--max-bytes` the run's one client advertises small HTTP/2
    // receive windows, so a read cancelled at the cut lets at most one
    // stream window of its body cross the wire (VERIFY.md row 19a).
    if request.max_bytes.is_some() {
        crate::shared_client::bound_receive_windows();
    }

    // 1 — bind the folder, and with `--paths-from=-` the repository too,
    // then read standard input whole, before any request.
    let folder = bind_read_folder(&opts)?;
    let typed = if request.from_stdin {
        if bind_repository(opts.repo.as_deref(), opts.if_repo, output)?.is_none() {
            return Ok(());
        }
        let mut input = Vec::new();
        std::io::stdin()
            .lock()
            .read_to_end(&mut input)
            .map_err(|err| CliError::Io {
                message: format!("could not read standard input: {err}"),
            })?;
        paths_from_input(&input)?
    } else {
        request.paths
    };

    // 2 — map each path; a path refused before any request is its own
    // entry, and a path asked twice is read once. The byte stream stops
    // at its first refusal, so nothing after one is read.
    let mut distinct: Vec<Distinct> = Vec::new();
    let mut seen: std::collections::HashMap<String, usize> = std::collections::HashMap::new();
    let mut entries: Vec<Result<usize, CliError>> = Vec::with_capacity(typed.len());
    for path in &typed {
        let entry = listed_argument(folder.as_ref(), path).map(|mapped| {
            *seen.entry(mapped).or_insert_with_key(|mapped| {
                distinct.push(Distinct {
                    mapped: mapped.clone(),
                    typed: path.clone(),
                });
                distinct.len() - 1
            })
        });
        let refused = entry.is_err();
        entries.push(entry);
        if refused && !output.is_json() {
            break;
        }
    }

    // 3 and 4 — under `--json`, the `files` answer over `POST …/read`
    // (roadmap 279, D-243 §8): unpinned, the first chunk at `head` names
    // the one reference every path is read at.
    let slots: Vec<Option<usize>> = entries.iter().map(|e| e.as_ref().ok().copied()).collect();
    let batched = if output.is_json() && !distinct.is_empty() {
        cat_many_batch(config, output, &opts, &distinct, &slots, request.max_bytes).await?
    } else {
        None
    };
    let (target, many) = match batched {
        Some(Batched::Skipped) => return Ok(()),
        Some(Batched::Read(target, many)) => (target, many),
        unread => {
            // 3 — resolve the one reference every path is read at; a
            // server answering no v2 for the batch is asked v1 alone.
            let target = match unread {
                Some(Batched::NoV2(Some(target))) => *target,
                _ => {
                    let Some(target) = crate::read::resolve_read_target_through(
                        config,
                        output,
                        &opts,
                        unread.is_none(),
                    )
                    .await?
                    else {
                        return Ok(());
                    };
                    target
                }
            };
            let client = Arc::new(SynsClient::new(config.server_url())?);
            let store = Store::open(config, target.token.as_deref()).map(Arc::new);
            let target = Arc::new(target);

            // 4 — read them.
            let stop = if output.is_json() {
                Stop::RunWide
            } else {
                Stop::InOrder
            };
            let many = read_many(
                client,
                target.clone(),
                store.clone(),
                distinct,
                &slots,
                request.max_bytes,
                stop,
            )
            .await;
            if let Some(store) = &store {
                store.finish();
            }
            (target, many)
        }
    };
    let reference = &target.reference;

    // 5 — the byte stream: each file in order, stopping at the first that
    // failed, its refusal the run's, worded as one path's.
    let mut results = many.results;
    if !output.is_json() {
        let stdout = std::io::stdout();
        let mut lock = std::io::BufWriter::with_capacity(1 << 16, stdout.lock());
        for (place, entry) in entries.into_iter().enumerate() {
            let path = &typed[place];
            let failure = match entry {
                Err(refusal) => refusal,
                Ok(d) => match std::mem::replace(&mut results[d], PathResult::NotRun) {
                    PathResult::Read(answer) => {
                        if let Err(err) = lock.write_all(&answer.bytes) {
                            panic!("failed printing to stdout: {err}");
                        }
                        results[d] = PathResult::Read(answer);
                        continue;
                    }
                    PathResult::Failed(err) => err,
                    PathResult::Deferred | PathResult::NotRun => CliError::Io {
                        message: format!("the read of {path} did not complete"),
                    },
                },
            };
            if let Err(err) = lock.flush() {
                panic!("failed printing to stdout: {err}");
            }
            if opts.version.is_some() {
                return Err(read_not_found(failure, &opts, reference, path));
            }
            return Err(failure.with_cat_path_context(path.clone()));
        }
        if let Err(err) = lock.flush() {
            panic!("failed printing to stdout: {err}");
        }
        report_reference(output, reference);
        return Ok(());
    }

    // 6 — under `--json`, a failure no per-file class takes ends the run
    // in place of the document (D-242 §7).
    if let Some(index) = results.iter().position(
        |result| matches!(result, PathResult::Failed(err) if per_file(err, 0, "").is_none()),
    ) && let PathResult::Failed(err) = std::mem::replace(&mut results[index], PathResult::NotRun)
    {
        return Err(err);
    }

    // 7 — the `files` document, in the order asked.
    let mut files = Vec::new();
    let mut deferred = Vec::new();
    for (place, entry) in entries.iter().enumerate() {
        let path = typed[place].as_str();
        if many.cut.is_some_and(|cut| place >= cut) {
            deferred.push(path);
            continue;
        }
        let failure = |err: &CliError| {
            let (code, message) = per_file(err, reference.version, path)
                .unwrap_or_else(|| ("INTERNAL_ERROR".to_string(), err.to_string()));
            FileEntry::Failed(FailedEntry {
                error: EntryError { code, message },
                path,
            })
        };
        match entry {
            Err(refusal) => files.push(failure(refusal)),
            Ok(d) => match &results[*d] {
                PathResult::Read(answer) => {
                    let text = is_text(&answer.bytes);
                    files.push(FileEntry::Read(ReadEntry {
                        content: text
                            .then(|| std::str::from_utf8(&answer.bytes).ok())
                            .flatten(),
                        content_base64: (!text).then_some(answer.bytes.as_slice()),
                        path,
                        sha: &answer.sha,
                        size: answer.bytes.len() as u64,
                    }));
                }
                PathResult::Failed(err) => files.push(failure(err)),
                PathResult::Deferred | PathResult::NotRun => deferred.push(path),
            },
        }
    }
    let document = FilesDocument {
        commit_sha: &reference.commit_sha,
        deferred,
        files,
        version: reference.version,
    };
    let stdout = std::io::stdout();
    let mut stream = std::io::BufWriter::with_capacity(1 << 16, stdout.lock());
    let written = serde_json::to_writer_pretty(&mut stream, &document)
        .map_err(std::io::Error::from)
        .and_then(|()| stream.write_all(b"\n"))
        .and_then(|()| stream.flush());
    if let Err(err) = written {
        panic!("failed printing to stdout: {err}");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::push::hash::blob_sha1;
    use serial_test::serial;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    const HEAD_SHA: &str = "def4560000000000000000000000000000000000";

    /// Mounts the two addresses every read verb now resolves through
    /// before it asks for any content (SPEC u270 `resolve_read_target`).
    async fn mount_reference(server: &MockServer, repo_id: &str) {
        Mock::given(method("GET"))
            .and(path(format!("/api/v1/repos/{repo_id}")))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "owner": "alice", "name": "my-project", "description": null,
                "commitSha": HEAD_SHA, "status": "active", "author": null, "tags": [],
                "visibility": "public", "forkedFrom": null, "forkCount": 0,
                "fileCount": 1, "role": null,
                "createdAt": "2026-01-01T00:00:00Z", "updatedAt": "2026-01-01T00:00:00Z",
            })))
            .mount(server)
            .await;
        Mock::given(method("GET"))
            .and(path(format!("/api/v1/repos/{repo_id}/versions/{HEAD_SHA}")))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "version": 7, "sha": HEAD_SHA, "parentSha": null, "message": "m",
                "messageBody": null, "author": "alice",
                "createdAt": "2026-01-01T00:00:00Z", "filesChanged": ["a.md"],
            })))
            .mount(server)
            .await;
    }

    /// Mounts the raw entry answering `bytes` under an `ETag` of their
    /// blob hash (SPEC u280 `cmd_cat` 1–2).
    async fn mount_raw(server: &MockServer, file: &str, bytes: &[u8]) {
        Mock::given(method("GET"))
            .and(path(format!("/api/v1/repos/alice/my-project/raw/{file}")))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("ETag", format!("\"{}\"", blob_sha1(bytes)).as_str())
                    .set_body_bytes(bytes.to_vec()),
            )
            .mount(server)
            .await;
    }

    #[tokio::test]
    #[serial]
    async fn cat_refuses_bytes_the_etag_does_not_name() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join(".syns.yaml"),
            "owner: alice\nname: my-project\n",
        )
        .unwrap();
        std::env::set_current_dir(dir.path()).unwrap();
        unsafe { std::env::set_var("SYNS_CONFIG_DIR", dir.path()) };
        unsafe { std::env::set_var("SYNS_CACHE_DIR", dir.path()) };

        let mock_server = MockServer::start().await;
        mount_reference(&mock_server, "alice/my-project").await;
        Mock::given(method("GET"))
            .and(path("/api/v1/repos/alice/my-project/raw/image.png"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("ETag", format!("\"{}\"", "0".repeat(40)).as_str())
                    .set_body_bytes(b"\x89PNG\x00".to_vec()),
            )
            .mount(&mock_server)
            .await;

        let config = Config::new(Some(&mock_server.uri())).unwrap();
        let result = cmd_cat(
            &config,
            &Output::new(false),
            "image.png".to_string(),
            here(),
        )
        .await;
        unsafe { std::env::remove_var("SYNS_CONFIG_DIR") };
        unsafe { std::env::remove_var("SYNS_CACHE_DIR") };

        let err = result.unwrap_err();
        assert!(
            err.to_string().contains(&format!(
                "invalid response body: image.png: expected {}, got {}",
                "0".repeat(40),
                blob_sha1(b"\x89PNG\x00")
            )),
            "{err}"
        );
        assert_eq!(err.exit_code(), 1);
    }

    fn here() -> ReadOptions {
        ReadOptions::default()
    }

    fn here_if_repo() -> ReadOptions {
        ReadOptions {
            if_repo: true,
            ..ReadOptions::default()
        }
    }

    #[tokio::test]
    #[serial]
    async fn cat_outputs_file_content() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join(".syns.yaml"),
            "owner: alice\nname: my-project\n",
        )
        .unwrap();
        std::env::set_current_dir(dir.path()).unwrap();
        unsafe { std::env::set_var("SYNS_CONFIG_DIR", dir.path()) };
        unsafe { std::env::set_var("SYNS_CACHE_DIR", dir.path()) };

        let mock_server = MockServer::start().await;
        mount_reference(&mock_server, "alice/my-project").await;

        mount_raw(&mock_server, "src/main.ts", b"console.log('hello');").await;

        let config = Config::new(Some(&mock_server.uri())).unwrap();
        let output = Output::new(false);

        let result = cmd_cat(&config, &output, "src/main.ts".to_string(), here()).await;
        unsafe { std::env::remove_var("SYNS_CONFIG_DIR") };
        unsafe { std::env::remove_var("SYNS_CACHE_DIR") };

        assert!(result.is_ok());
    }

    #[tokio::test]
    #[serial]
    async fn cat_json_output() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join(".syns.yaml"),
            "owner: alice\nname: my-project\n",
        )
        .unwrap();
        std::env::set_current_dir(dir.path()).unwrap();
        unsafe { std::env::set_var("SYNS_CONFIG_DIR", dir.path()) };
        unsafe { std::env::set_var("SYNS_CACHE_DIR", dir.path()) };

        let mock_server = MockServer::start().await;
        mount_reference(&mock_server, "alice/my-project").await;
        // SPEC u283 `cmd_cat` 2: `--json` reads the raw entry too.
        mount_raw(&mock_server, "src/main.ts", b"console.log('hello');").await;

        let config = Config::new(Some(&mock_server.uri())).unwrap();
        let output = Output::new(true);

        let result = cmd_cat(&config, &output, "src/main.ts".to_string(), here()).await;
        unsafe { std::env::remove_var("SYNS_CONFIG_DIR") };
        unsafe { std::env::remove_var("SYNS_CACHE_DIR") };

        assert!(result.is_ok());
    }

    #[tokio::test]
    #[serial]
    async fn cat_404_not_found_renders_friendly_message_with_path() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join(".syns.yaml"),
            "owner: alice\nname: my-project\n",
        )
        .unwrap();
        std::env::set_current_dir(dir.path()).unwrap();
        unsafe { std::env::set_var("SYNS_CONFIG_DIR", dir.path()) };
        unsafe { std::env::set_var("SYNS_CACHE_DIR", dir.path()) };

        let mock_server = MockServer::start().await;
        mount_reference(&mock_server, "alice/my-project").await;

        Mock::given(method("GET"))
            .and(path("/api/v1/repos/alice/my-project/raw/src/missing.ts"))
            .respond_with(ResponseTemplate::new(404).set_body_json(serde_json::json!({
                "error": "not_found",
                "message": "File not found"
            })))
            .mount(&mock_server)
            .await;

        let config = Config::new(Some(&mock_server.uri())).unwrap();
        let output = Output::new(false);

        let result = cmd_cat(&config, &output, "src/missing.ts".to_string(), here()).await;
        unsafe { std::env::remove_var("SYNS_CONFIG_DIR") };
        unsafe { std::env::remove_var("SYNS_CACHE_DIR") };

        let err = result.unwrap_err();
        assert_eq!(err.to_string(), "file not found: src/missing.ts");
        assert_eq!(err.exit_code(), 1);
    }

    #[tokio::test]
    #[serial]
    async fn cat_404_repo_not_found_preserves_generic_display() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join(".syns.yaml"),
            "owner: alice\nname: my-project\n",
        )
        .unwrap();
        std::env::set_current_dir(dir.path()).unwrap();
        unsafe { std::env::set_var("SYNS_CONFIG_DIR", dir.path()) };
        unsafe { std::env::set_var("SYNS_CACHE_DIR", dir.path()) };

        let mock_server = MockServer::start().await;
        mount_reference(&mock_server, "alice/my-project").await;

        Mock::given(method("GET"))
            .and(path("/api/v1/repos/alice/my-project/raw/README.md"))
            .respond_with(ResponseTemplate::new(404).set_body_json(serde_json::json!({
                "error": "repo_not_found",
                "message": "Repository not found"
            })))
            .mount(&mock_server)
            .await;

        let config = Config::new(Some(&mock_server.uri())).unwrap();
        let output = Output::new(false);

        let result = cmd_cat(&config, &output, "README.md".to_string(), here()).await;
        unsafe { std::env::remove_var("SYNS_CONFIG_DIR") };
        unsafe { std::env::remove_var("SYNS_CACHE_DIR") };

        let err = result.unwrap_err();
        assert_eq!(err.to_string(), "server error (404): repo_not_found");
        assert_eq!(err.exit_code(), 1);
    }

    #[tokio::test]
    #[serial]
    async fn cat_404_not_found_in_json_mode_preserves_generic_display() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join(".syns.yaml"),
            "owner: alice\nname: my-project\n",
        )
        .unwrap();
        std::env::set_current_dir(dir.path()).unwrap();
        unsafe { std::env::set_var("SYNS_CONFIG_DIR", dir.path()) };
        unsafe { std::env::set_var("SYNS_CACHE_DIR", dir.path()) };

        let mock_server = MockServer::start().await;
        mount_reference(&mock_server, "alice/my-project").await;

        Mock::given(method("GET"))
            .and(path("/api/v1/repos/alice/my-project/raw/src/missing.ts"))
            .respond_with(ResponseTemplate::new(404).set_body_json(serde_json::json!({
                "error": "not_found",
                "message": "File not found"
            })))
            .mount(&mock_server)
            .await;

        let config = Config::new(Some(&mock_server.uri())).unwrap();
        let output = Output::new(true);

        let result = cmd_cat(&config, &output, "src/missing.ts".to_string(), here()).await;
        unsafe { std::env::remove_var("SYNS_CONFIG_DIR") };
        unsafe { std::env::remove_var("SYNS_CACHE_DIR") };

        let err = result.unwrap_err();
        assert_eq!(err.to_string(), "server error (404): not_found");
        assert_eq!(err.exit_code(), 1);
    }

    #[tokio::test]
    #[serial]
    async fn cat_with_if_repo_set_and_identity_resolved_runs_normally() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join(".syns.yaml"),
            "owner: alice\nname: my-project\n",
        )
        .unwrap();
        std::env::set_current_dir(dir.path()).unwrap();
        unsafe { std::env::set_var("SYNS_CONFIG_DIR", dir.path()) };
        unsafe { std::env::set_var("SYNS_CACHE_DIR", dir.path()) };

        let mock_server = MockServer::start().await;
        mount_reference(&mock_server, "alice/my-project").await;
        mount_raw(&mock_server, "src/main.ts", b"console.log('hello');").await;

        let config = Config::new(Some(&mock_server.uri())).unwrap();
        let output = Output::new(false);

        let result = cmd_cat(&config, &output, "src/main.ts".to_string(), here_if_repo()).await;
        unsafe { std::env::remove_var("SYNS_CONFIG_DIR") };
        unsafe { std::env::remove_var("SYNS_CACHE_DIR") };

        assert!(result.is_ok());
    }

    fn reference() -> ResolvedRef {
        ResolvedRef {
            version: 7,
            commit_sha: HEAD_SHA.to_string(),
        }
    }

    fn raw_file(bytes: &[u8], media_type: Option<&str>) -> RawFile {
        RawFile {
            bytes: bytes.to_vec(),
            etag: None,
            media_type: media_type.map(str::to_string),
        }
    }

    // SPEC u283 Contract Surface, `CatDocument`: text rides as `content`
    // beside a `null` `mediaType`, keys in lexical order.
    #[test]
    fn a_text_document_carries_content_and_a_null_media_type() {
        let document = CatDocument::new(
            "a.md".to_string(),
            raw_file(b"# a\n", Some("text/plain")),
            blob_sha1(b"# a\n"),
            &reference(),
        );
        let written = serde_json::to_string(&document).unwrap();
        assert_eq!(
            written,
            format!(
                r##"{{"commitSha":"{HEAD_SHA}","content":"# a\n","mediaType":null,"path":"a.md","sha":"{}","size":4,"version":7}}"##,
                blob_sha1(b"# a\n")
            )
        );
    }

    // `CatDocument`: bytes ride as `contentBase64`, `mediaType` naming a
    // type other than the generic byte type and omitted for it.
    #[test]
    fn a_byte_document_carries_base64_and_names_only_a_specific_type() {
        let png: &[u8] = b"\x89PNG\r\n\x1a\n\x00\xff";
        let named: serde_json::Value = serde_json::to_value(CatDocument::new(
            "image.png".to_string(),
            raw_file(png, Some("image/png")),
            blob_sha1(png),
            &reference(),
        ))
        .unwrap();
        assert_eq!(
            named["contentBase64"],
            serde_json::json!("iVBORw0KGgoA/w==")
        );
        assert_eq!(named["mediaType"], serde_json::json!("image/png"));
        assert_eq!(named["size"], serde_json::json!(10));
        assert!(named.get("content").is_none());

        for generic in [Some("application/octet-stream"), None] {
            let unnamed = serde_json::to_value(CatDocument::new(
                "blob.bin".to_string(),
                raw_file(png, generic),
                blob_sha1(png),
                &reference(),
            ))
            .unwrap();
            assert!(unnamed.get("mediaType").is_none(), "{generic:?}");
            assert!(unnamed.get("content").is_none());
        }
    }

    #[tokio::test]
    async fn cat_get_file_raw_includes_path_field() {
        let mock_server = MockServer::start().await;
        let body =
            r#"{"path":"src/main.ts","sha":"abc123","content":"console.log('hello');","size":21}"#;
        Mock::given(method("GET"))
            .and(path("/api/v1/repos/alice/my-project/files/src/main.ts"))
            .respond_with(ResponseTemplate::new(200).set_body_string(body))
            .mount(&mock_server)
            .await;

        let client = SynsClient::new(&mock_server.uri()).unwrap();
        let (_typed, raw) = client
            .get_file("alice/my-project", None, "src/main.ts", None)
            .await
            .unwrap();

        assert!(raw.get("path").is_some());
        assert_eq!(raw["path"], serde_json::json!("src/main.ts"));
        assert!(raw.get("content").is_some());
        assert!(raw.get("sha").is_some());
        assert!(raw.get("size").is_some());
        let expected: serde_json::Value = serde_json::from_str(body).unwrap();
        assert_eq!(raw, expected);
    }

    #[tokio::test]
    #[serial]
    async fn cat_with_if_repo_set_and_no_identity_skips_silently() {
        let dir = tempfile::tempdir().unwrap();
        std::env::set_current_dir(dir.path()).unwrap();
        unsafe { std::env::set_var("SYNS_CONFIG_DIR", dir.path()) };
        unsafe { std::env::set_var("SYNS_CACHE_DIR", dir.path()) };

        let mock_server = MockServer::start().await;
        let config = Config::new(Some(&mock_server.uri())).unwrap();
        let output = Output::new(false);

        let result = cmd_cat(&config, &output, "anything".to_string(), here_if_repo()).await;
        unsafe { std::env::remove_var("SYNS_CONFIG_DIR") };
        unsafe { std::env::remove_var("SYNS_CACHE_DIR") };

        assert!(result.is_ok());
        assert!(mock_server.received_requests().await.unwrap().is_empty());
    }
}
