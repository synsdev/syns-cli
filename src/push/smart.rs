use std::collections::HashMap;
use std::path::{Path, PathBuf};

use crate::client::{
    EntryType, PushDeleteEntry, PushFileEntry, PushRequest, PushResponse, RepoStatus, SynsClient,
    TreeResponse, Visibility,
};
use crate::errors::CliError;
use crate::push::collector::{
    CollectOptions, CollectResult, SkipReason, SkippedFile, collect_files,
};
use crate::push::hash::blob_sha1;
use crate::push::manifest::Manifest;
use crate::repo::syns_yaml::write_syns_yaml;

/// Knobs passed into `smart_push` from the CLI layer. Wraps the
/// historical push options (force, message, author, excludes, …) plus
/// the four u213 push-feedback flags (`strict`, `allow_empty`,
/// `debug`, `no_default_excludes`). See SPEC u213 § 3.2 for the
/// per-flag semantics.
pub struct SmartPushOptions {
    pub force: bool,
    pub message: String,
    pub author: String,
    pub parent_sha: Option<String>,
    pub excludes: Vec<String>,
    pub cache_dir: PathBuf,
    pub description: Option<String>,
    pub tags: Option<Vec<String>>,
    pub status: Option<RepoStatus>,
    pub visibility: Option<Visibility>,
    /// Mirror of CLI `--strict`. When `true` and any file is skipped,
    /// `smart_push` returns `CliError::PushPartial` before any wire
    /// call (SPEC § 3.2, Phase 2b).
    pub strict: bool,
    /// Mirror of CLI `--allow-empty`. When `true`, bypasses the
    /// empty-collection guard so an empty push proceeds to the wire
    /// (SPEC § 3.2, Phase 2c).
    pub allow_empty: bool,
    /// Mirror of CLI `--debug`. When `true`, the collector emits one
    /// `[debug] skip {path}: {reason} ({source})` line per excluded
    /// file to stderr (SPEC § 3.2).
    pub debug: bool,
    /// Mirror of CLI `--no-default-excludes`. When `true`, the
    /// built-in `DEFAULT_EXCLUDE_DIRS` skip list is bypassed (SPEC
    /// § 3.2). Also threaded into the `PushPipelineMeta` and into
    /// `CliError::PushPartial` so the no-default-excludes hint line
    /// is gated correctly in both the success and the error paths.
    pub no_default_excludes: bool,
}

/// Metadata threaded from `smart_push` to `format_response` so the
/// command layer can render the skip summary and distinguish a
/// first-push-noop (no prior manifest) from a subsequent-push-noop.
#[derive(Debug, Clone)]
pub struct PushPipelineMeta {
    pub skipped: Vec<SkippedFile>,
    pub manifest_existed: bool,
    pub strict: bool,
    pub no_default_excludes: bool,
}

/// Per-batch JSON-body budget for the auto-chunker. Set to leave
/// headroom below the deployment-edge cap (Cloud Run HTTP/1.1 ~32 MiB)
/// after JSON-encoding overhead. Best-effort: a single file whose
/// materialised entry alone exceeds this budget still becomes its
/// own one-entry batch and may still 413 — in which case the chunker
/// rewrites the propagated error with the offending batch's metrics.
pub const CHUNK_BUDGET_BYTES: usize = 25 * 1024 * 1024;

/// Returns the size in bytes of the JSON-encoded wire body for a
/// `PushRequest`. Used both by the pre-flight chunker invocation and
/// inside the chunker's batch-packing loop. Falls back to `usize::MAX`
/// on a (impossible-in-practice) serialization failure so the caller
/// errs on the side of chunking rather than silently bypassing the
/// budget check.
fn estimate_body_bytes(request: &PushRequest) -> usize {
    serde_json::to_string(request)
        .map(|s| s.len())
        .unwrap_or(usize::MAX)
}

fn split_repo_id(repo_id: &str) -> Result<(&str, &str), CliError> {
    repo_id.split_once('/').ok_or_else(|| CliError::Config {
        message: format!("invalid repo id: {repo_id}"),
    })
}

fn tree_to_sha_map(tree: &TreeResponse) -> HashMap<String, String> {
    tree.entries
        .iter()
        .filter(|e| e.entry_type == EntryType::File && e.sha.is_some())
        .map(|e| (e.path.clone(), e.sha.clone().unwrap()))
        .collect()
}

fn build_push_entries(
    local_files: &HashMap<String, Vec<u8>>,
    local_shas: &HashMap<String, String>,
    reference_shas: &HashMap<String, String>,
    force: bool,
) -> Result<(Vec<PushFileEntry>, Vec<PushDeleteEntry>), CliError> {
    let mut entries = Vec::new();
    let mut deletes = Vec::new();

    for (path, sha) in local_shas {
        let changed = force || (reference_shas.get(path) != Some(sha));
        let content = if changed {
            let bytes = &local_files[path];
            let utf8 = String::from_utf8(bytes.clone()).map_err(|_| CliError::Io {
                message: format!("file is not valid UTF-8: {path}"),
            })?;
            Some(utf8)
        } else {
            None
        };
        entries.push(PushFileEntry {
            path: path.clone(),
            sha: sha.clone(),
            content,
        });
    }

    if !force {
        for path in reference_shas.keys() {
            if !local_shas.contains_key(path) {
                deletes.push(PushDeleteEntry { path: path.clone() });
            }
        }
    }

    Ok((entries, deletes))
}

fn upgrade_to_full(
    entries: &[PushFileEntry],
    local_files: &HashMap<String, Vec<u8>>,
) -> Result<Vec<PushFileEntry>, CliError> {
    let mut upgraded = Vec::new();
    for entry in entries {
        if entry.content.is_some() {
            upgraded.push(PushFileEntry {
                path: entry.path.clone(),
                sha: entry.sha.clone(),
                content: entry.content.clone(),
            });
        } else {
            let bytes = &local_files[&entry.path];
            let utf8 = String::from_utf8(bytes.clone()).map_err(|_| CliError::Io {
                message: format!("file is not valid UTF-8: {}", entry.path),
            })?;
            upgraded.push(PushFileEntry {
                path: entry.path.clone(),
                sha: entry.sha.clone(),
                content: Some(utf8),
            });
        }
    }
    Ok(upgraded)
}

/// Auto-chunk a `PushRequest` into multiple sequential `client.push`
/// calls, each below `CHUNK_BUDGET_BYTES`. Used by `smart_push` both
/// pre-flight (when the projected body exceeds the budget) and as a
/// 413 fallback (when the wire returns `PayloadTooLarge`).
///
/// Best-effort: a single entry whose materialised content alone
/// exceeds the budget still becomes its own one-entry batch and may
/// still 413 — the chunker then rewrites the propagated
/// `PayloadTooLarge` with the offending batch's `bytes_sent` and
/// `file_count`.
async fn chunked_push(
    client: &SynsClient,
    token: &str,
    repo_id: &str,
    base_request: &PushRequest,
    local_files: &HashMap<String, Vec<u8>>,
    starting_parent_sha: Option<String>,
) -> Result<(PushResponse, serde_json::Value), CliError> {
    // Step 1: materialise (every entry carries content after this).
    let mut entries = upgrade_to_full(&base_request.files, local_files)?;

    // Step 2: sort descending by content size for FFD packing.
    entries.sort_by_key(|e| std::cmp::Reverse(e.content.as_ref().map_or(0, |s| s.len())));

    // Step 3: greedy-pack into batches.
    let mut batches: Vec<Vec<PushFileEntry>> = vec![Vec::new()];
    for entry in entries {
        // Tentatively append to current batch.
        batches.last_mut().unwrap().push(entry.clone());

        // Re-estimate body bytes for the current batch.
        let probe = PushRequest {
            files: batches.last().unwrap().clone(),
            deletions: None,
            message: base_request.message.clone(),
            author: base_request.author.clone(),
            parent_sha: starting_parent_sha.clone(),
            description: base_request.description.clone(),
            tags: base_request.tags.clone(),
            status: base_request.status.clone(),
            visibility: base_request.visibility.clone(),
        };
        if estimate_body_bytes(&probe) > CHUNK_BUDGET_BYTES
            && batches.last().unwrap().len() > 1
        {
            // Over budget — pop and seed a new batch with this entry.
            let last_entry = batches.last_mut().unwrap().pop().unwrap();
            batches.push(vec![last_entry]);
        }
    }

    let n = batches.len();

    // Step 4: short-circuit if no chunking actually needed.
    if n == 1 {
        let only_batch = batches.into_iter().next().unwrap();
        let request = PushRequest {
            files: only_batch,
            deletions: base_request.deletions.clone(),
            message: base_request.message.clone(),
            author: base_request.author.clone(),
            parent_sha: starting_parent_sha,
            description: base_request.description.clone(),
            tags: base_request.tags.clone(),
            status: base_request.status.clone(),
            visibility: base_request.visibility.clone(),
        };
        return client.push(repo_id, token, &request).await;
    }

    // Step 5: sequential batch submission with chained parent_sha.
    // PushResponse does not derive Clone, so we keep commit_sha
    // (which is String: Clone) separately and store the most recent
    // (response, raw) pair in last_completed for the final return.
    let mut previous_commit_sha: Option<String> = starting_parent_sha;
    let mut last_completed: Option<(PushResponse, serde_json::Value)> = None;

    for (k, batch) in batches.iter().enumerate() {
        let k1 = k + 1; // 1-indexed for human-facing progress and `(part k/n)`.

        // Per-batch metrics (also used for the 413-rewrite path).
        let batch_bytes: usize = batch
            .iter()
            .map(|e| e.content.as_ref().map_or(0, |s| s.len()))
            .sum();
        let batch_files = batch.len();

        // Build per-batch request.
        let deletions = if k1 == n {
            base_request.deletions.clone()
        } else {
            None
        };
        let synthesised_message = format!(
            "{} (part {}/{})",
            base_request.message.as_deref().unwrap_or(""),
            k1,
            n,
        );
        let request = PushRequest {
            files: batch.clone(),
            deletions,
            message: Some(synthesised_message),
            author: base_request.author.clone(),
            parent_sha: previous_commit_sha.clone(),
            description: base_request.description.clone(),
            tags: base_request.tags.clone(),
            status: base_request.status.clone(),
            visibility: base_request.visibility.clone(),
        };

        // Progress line — matches existing smart.rs stderr-progress convention.
        eprintln!(
            "chunk {}/{}: {:.1} MiB uploaded across {} files",
            k1,
            n,
            batch_bytes as f64 / (1024.0 * 1024.0),
            batch_files,
        );

        // Submit. On 413, rewrite the variant with this batch's metrics.
        match client.push(repo_id, token, &request).await {
            Ok((response, raw)) => {
                previous_commit_sha = Some(response.commit_sha.clone());
                last_completed = Some((response, raw));
            }
            Err(CliError::PayloadTooLarge { rejecter, .. }) => {
                return Err(CliError::PayloadTooLarge {
                    bytes_sent: batch_bytes as u64,
                    file_count: batch_files,
                    rejecter,
                });
            }
            Err(other) => return Err(other),
        }
    }

    // Step 6: return the FINAL batch's response.
    Ok(last_completed.expect("at least one batch ran when n > 1"))
}

/// Pick one human-readable most-likely cause for `PushEmpty`'s
/// diagnostic (SPEC u213 § 5 D4). Single-line output; no path list.
fn skip_summary_cause(skipped: &[SkippedFile], source: &Path) -> String {
    use SkipReason::*;

    if skipped.is_empty() {
        return "the source directory contains no files".to_string();
    }

    let total = skipped.len();
    let mut counts: [usize; 5] = [0; 5];
    for sf in skipped {
        counts[sf.reason as usize] += 1;
    }
    let majority = |idx: usize| counts[idx] * 2 > total; // strict majority

    let gitignore_present = source.join(".gitignore").is_file();
    let synsignore_present = source.join(".synsignore").is_file();

    if majority(Binary as usize) {
        "every file appears to be binary (null-byte in first 8 KB)".to_string()
    } else if majority(DefaultExcludeDir as usize) {
        "every file is inside a default-excluded directory (node_modules, dist, build, target, .venv, …); pass --no-default-excludes to override".to_string()
    } else if gitignore_present && majority(Gitignore as usize) {
        format!(
            "a .gitignore file in {} excludes every file",
            source.display()
        )
    } else if synsignore_present && majority(Synsignore as usize) {
        format!(
            "a .synsignore file in {} excludes every file",
            source.display()
        )
    } else if majority(UserExclude as usize) {
        "every file matches a --exclude pattern".to_string()
    } else {
        format!(
            "{} file(s) excluded across multiple reasons; pass --debug for per-file detail",
            total
        )
    }
}

pub async fn smart_push(
    client: &SynsClient,
    token: &str,
    repo_id: &str,
    path: &Path,
    opts: SmartPushOptions,
) -> Result<(PushResponse, serde_json::Value, PushPipelineMeta), CliError> {
    // Phase 1 — Setup (preserved from u21).
    let (owner, name) = split_repo_id(repo_id)?;

    if !path.join(".syns.yaml").exists() {
        write_syns_yaml(path, owner, name)?;
    }

    // Phase 2a — Collect.
    let CollectResult {
        files: local_files,
        skipped,
        total_walked,
    } = collect_files(
        path,
        &opts.excludes,
        CollectOptions {
            no_default_excludes: opts.no_default_excludes,
            debug: opts.debug,
        },
    )?;

    // Phase 2b — Strict guard (supersedes empty per SPEC D10).
    if opts.strict && !skipped.is_empty() {
        return Err(CliError::PushPartial {
            skipped,
            no_default_excludes: opts.no_default_excludes,
        });
    }

    // Phase 2c — Empty guard.
    if local_files.is_empty() && !opts.allow_empty {
        let cause = skip_summary_cause(&skipped, path);
        return Err(CliError::PushEmpty {
            path: path.display().to_string(),
            total_walked,
            cause,
        });
    }

    // Phase 3a — Compute local SHAs (preserved).
    let local_shas: HashMap<String, String> = local_files
        .iter()
        .map(|(p, content)| (p.clone(), blob_sha1(content)))
        .collect();

    // Phase 3b — Load manifest unconditionally (so `manifest_existed`
    // is set even when --force bypasses the reference state from it).
    let loaded_manifest = Manifest::load(&opts.cache_dir, owner, name);
    let manifest_existed = loaded_manifest.is_some();

    // Phase 3c — Build reference state.
    let (reference_shas, base_parent_sha) = if opts.force {
        (HashMap::new(), None)
    } else if let Some(manifest) = loaded_manifest {
        let ref_shas: HashMap<String, String> = manifest
            .file_paths()
            .filter_map(|p| {
                manifest
                    .file_sha(p)
                    .map(|sha| (p.to_string(), sha.to_string()))
            })
            .collect();
        let parent = manifest.commit_sha().map(String::from);
        (ref_shas, parent)
    } else {
        match client.pull(repo_id, Some(token)).await {
            Ok(tree) => {
                let parent = Some(tree.commit_sha.clone());
                (tree_to_sha_map(&tree), parent)
            }
            Err(CliError::Api {
                status: Some(404), ..
            }) => (HashMap::new(), None),
            Err(e) => return Err(e),
        }
    };

    let parent_sha = if opts.parent_sha.is_some() {
        opts.parent_sha
    } else {
        base_parent_sha
    };

    // Phase 4 — Build payload (preserved from u21).
    let (entries, deletes) =
        build_push_entries(&local_files, &local_shas, &reference_shas, opts.force)?;

    let deletions = if deletes.is_empty() {
        None
    } else {
        Some(deletes)
    };

    let request = PushRequest {
        files: entries,
        deletions,
        message: Some(opts.message.clone()),
        author: Some(opts.author.clone()),
        parent_sha,
        description: opts.description.clone(),
        tags: opts.tags.clone(),
        status: opts.status.clone(),
        visibility: opts.visibility.clone(),
    };

    // Phase 5 — Submit (with pre-flight chunker, 409 missing_blobs retry,
    // and 413 fallback chunker per SPEC u225 § 4).
    //
    // Pre-flight: if the projected body exceeds the per-batch budget AND
    // the request carries content (not just SHAs), invoke the chunker
    // before issuing any wire call. This handles the FIRST-push of a
    // large repo (no manifest exists; every entry is materialised).
    let pre_flight_oversize = estimate_body_bytes(&request) > CHUNK_BUDGET_BYTES
        && request.files.iter().any(|e| e.content.is_some());

    let (response, raw) = if pre_flight_oversize {
        chunked_push(
            client,
            token,
            repo_id,
            &request,
            &local_files,
            request.parent_sha.clone(),
        )
        .await?
    } else {
        match client.push(repo_id, token, &request).await {
            Ok((response, raw)) => (response, raw),
            Err(CliError::Api {
                status: Some(409),
                ref error,
                ..
            }) if error == "missing_blobs" => {
                // Existing 409 retry: upgrade SHA-only entries to full
                // content. After the upgrade the body may exceed the
                // budget — if so, invoke the chunker on the retry path.
                let retry_entries = upgrade_to_full(&request.files, &local_files)?;
                let retry_request = PushRequest {
                    files: retry_entries,
                    deletions: request.deletions.clone(),
                    message: request.message.clone(),
                    author: request.author.clone(),
                    parent_sha: request.parent_sha.clone(),
                    description: request.description.clone(),
                    tags: request.tags.clone(),
                    status: request.status.clone(),
                    visibility: request.visibility.clone(),
                };
                if estimate_body_bytes(&retry_request) > CHUNK_BUDGET_BYTES {
                    chunked_push(
                        client,
                        token,
                        repo_id,
                        &retry_request,
                        &local_files,
                        retry_request.parent_sha.clone(),
                    )
                    .await?
                } else {
                    client.push(repo_id, token, &retry_request).await?
                }
            }
            Err(CliError::PayloadTooLarge { .. }) => {
                // 413 fallback (per AC-2): invoke the chunker with the
                // SAME request. The chunker self-rewrites bytes_sent /
                // file_count on a per-batch 413 — its return is verbatim
                // (per AC-1). No outer rewrite is needed.
                chunked_push(
                    client,
                    token,
                    repo_id,
                    &request,
                    &local_files,
                    request.parent_sha.clone(),
                )
                .await?
            }
            Err(e) => return Err(e),
        }
    };

    // Phase 6 — Manifest save (guarded per SPEC u213 § 4 Phase 6).
    if response.commit_sha.is_empty() {
        eprintln!(
            "warning: server response had empty commit_sha; not updating local manifest \
             (this typically indicates that no files were uploaded — see `syns push --debug`)"
        );
    } else {
        let mut manifest = Manifest::default();
        manifest.update(response.commit_sha.clone(), local_shas);
        if let Err(e) = manifest.save(&opts.cache_dir, owner, name) {
            eprintln!("warning: could not save manifest (next push will re-upload all files): {e}");
        }
    }

    // Phase 7 — Return with meta.
    Ok((
        response,
        raw,
        PushPipelineMeta {
            skipped,
            manifest_existed,
            strict: opts.strict,
            no_default_excludes: opts.no_default_excludes,
        },
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::errors::EdgeRejecter;
    use crate::push::hash::blob_sha1;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    #[tokio::test]
    async fn first_push_sends_all_files_with_content() {
        let mock_server = MockServer::start().await;

        Mock::given(method("GET"))
            .and(path("/api/v1/repos/alice/new-repo/tree"))
            .respond_with(
                ResponseTemplate::new(404).set_body_json(serde_json::json!({"error": "not_found"})),
            )
            .mount(&mock_server)
            .await;

        Mock::given(method("PUT"))
            .and(path("/api/v1/repos/alice/new-repo/push"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "commitSha": "abc123",
                "version": 1,
                "filesChanged": 2,
                "created": true
            })))
            .mount(&mock_server)
            .await;

        let temp_dir = tempfile::tempdir().unwrap();
        std::fs::write(temp_dir.path().join("main.txt"), "hello").unwrap();
        std::fs::create_dir_all(temp_dir.path().join("sub")).unwrap();
        std::fs::write(temp_dir.path().join("sub/other.txt"), "world").unwrap();

        let cache_dir = tempfile::tempdir().unwrap();
        let client = SynsClient::new(&mock_server.uri()).unwrap();

        let result = smart_push(
            &client,
            "test-token",
            "alice/new-repo",
            temp_dir.path(),
            SmartPushOptions {
                force: false,
                message: "init".into(),
                author: "alice".into(),
                parent_sha: None,
                excludes: vec![],
                cache_dir: cache_dir.path().to_path_buf(),
                description: None,
                tags: None,
                status: None,
                visibility: None,
                strict: false,
                allow_empty: false,
                debug: false,
                no_default_excludes: false,
            },
        )
        .await;

        assert!(result.is_ok());
        let (response, _raw, _meta) = result.unwrap();
        assert_eq!(response.commit_sha, "abc123");

        // .syns.yaml was auto-created
        let syns_yaml = std::fs::read_to_string(temp_dir.path().join(".syns.yaml")).unwrap();
        assert_eq!(syns_yaml, "owner: alice\nname: new-repo\n");

        // Manifest was saved
        assert!(
            cache_dir
                .path()
                .join("alice")
                .join("new-repo.json")
                .exists()
        );

        // Verify request body
        let requests = mock_server.received_requests().await.unwrap();
        let put_request = requests
            .iter()
            .find(|r| r.method == reqwest::Method::PUT)
            .unwrap();
        let body: serde_json::Value = serde_json::from_slice(&put_request.body).unwrap();

        let files = body["files"].as_array().unwrap();
        let has_main = files
            .iter()
            .any(|f| f["path"] == "main.txt" && f["content"].is_string());
        let has_other = files
            .iter()
            .any(|f| f["path"] == "sub/other.txt" && f["content"].is_string());
        assert!(has_main);
        assert!(has_other);
        assert!(body["parentSha"].is_null());
        assert!(body.get("deletions").is_none() || body["deletions"].is_null());
    }

    #[tokio::test]
    async fn subsequent_push_sends_only_changed_files() {
        let mock_server = MockServer::start().await;

        Mock::given(method("PUT"))
            .and(path("/api/v1/repos/bob/my-repo/push"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "commitSha": "def456",
                "version": 2,
                "filesChanged": 1,
                "created": false
            })))
            .mount(&mock_server)
            .await;

        let temp_dir = tempfile::tempdir().unwrap();
        std::fs::write(temp_dir.path().join("a.txt"), "unchanged").unwrap();
        std::fs::write(temp_dir.path().join("b.txt"), "modified").unwrap();
        std::fs::write(
            temp_dir.path().join(".syns.yaml"),
            "owner: bob\nname: my-repo\n",
        )
        .unwrap();

        let cache_dir = tempfile::tempdir().unwrap();
        let mut manifest = Manifest::default();
        manifest.update(
            "old-sha".into(),
            HashMap::from([
                ("a.txt".into(), blob_sha1(b"unchanged")),
                ("b.txt".into(), blob_sha1(b"original")),
            ]),
        );
        manifest.save(cache_dir.path(), "bob", "my-repo").unwrap();

        let client = SynsClient::new(&mock_server.uri()).unwrap();

        let result = smart_push(
            &client,
            "test-token",
            "bob/my-repo",
            temp_dir.path(),
            SmartPushOptions {
                force: false,
                message: "update".into(),
                author: "bob".into(),
                parent_sha: None,
                excludes: vec![],
                cache_dir: cache_dir.path().to_path_buf(),
                description: None,
                tags: None,
                status: None,
                visibility: None,
                strict: false,
                allow_empty: false,
                debug: false,
                no_default_excludes: false,
            },
        )
        .await;

        assert!(result.is_ok());

        let requests = mock_server.received_requests().await.unwrap();
        let put_request = requests
            .iter()
            .find(|r| r.method == reqwest::Method::PUT)
            .unwrap();
        let body: serde_json::Value = serde_json::from_slice(&put_request.body).unwrap();

        let files = body["files"].as_array().unwrap();
        let a_entry = files.iter().find(|f| f["path"] == "a.txt").unwrap();
        assert!(
            a_entry["content"].is_null(),
            "unchanged file should be sha-only"
        );
        let b_entry = files.iter().find(|f| f["path"] == "b.txt").unwrap();
        assert_eq!(b_entry["content"].as_str(), Some("modified"));
        assert_eq!(body["parentSha"].as_str(), Some("old-sha"));
        assert!(body.get("deletions").is_none() || body["deletions"].is_null());
    }

    #[tokio::test]
    async fn deleted_files_in_delete_list() {
        let mock_server = MockServer::start().await;

        Mock::given(method("PUT"))
            .and(path("/api/v1/repos/owner/repo/push"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "commitSha": "del123",
                "version": 3,
                "filesChanged": 1,
                "created": false
            })))
            .mount(&mock_server)
            .await;

        let temp_dir = tempfile::tempdir().unwrap();
        std::fs::write(temp_dir.path().join("keep.txt"), "keep").unwrap();
        std::fs::write(
            temp_dir.path().join(".syns.yaml"),
            "owner: owner\nname: repo\n",
        )
        .unwrap();

        let cache_dir = tempfile::tempdir().unwrap();
        let mut manifest = Manifest::default();
        manifest.update(
            "prev-sha".into(),
            HashMap::from([
                ("keep.txt".into(), blob_sha1(b"keep")),
                ("removed.txt".into(), blob_sha1(b"gone")),
            ]),
        );
        manifest.save(cache_dir.path(), "owner", "repo").unwrap();

        let client = SynsClient::new(&mock_server.uri()).unwrap();

        let result = smart_push(
            &client,
            "test-token",
            "owner/repo",
            temp_dir.path(),
            SmartPushOptions {
                force: false,
                message: "delete".into(),
                author: "owner".into(),
                parent_sha: None,
                excludes: vec![],
                cache_dir: cache_dir.path().to_path_buf(),
                description: None,
                tags: None,
                status: None,
                visibility: None,
                strict: false,
                allow_empty: false,
                debug: false,
                no_default_excludes: false,
            },
        )
        .await;

        assert!(result.is_ok());

        let requests = mock_server.received_requests().await.unwrap();
        let put_request = requests
            .iter()
            .find(|r| r.method == reqwest::Method::PUT)
            .unwrap();
        let body: serde_json::Value = serde_json::from_slice(&put_request.body).unwrap();

        let deletes: Vec<&str> = body["deletions"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v["path"].as_str().unwrap())
            .collect();
        assert_eq!(deletes, vec!["removed.txt"]);

        let files = body["files"].as_array().unwrap();
        let keep_entry = files.iter().find(|f| f["path"] == "keep.txt").unwrap();
        assert!(
            keep_entry["content"].is_null(),
            "unchanged file should be sha-only"
        );
    }

    #[tokio::test]
    async fn missing_blobs_409_triggers_retry() {
        let mock_server = MockServer::start().await;

        // 200 response with lower priority (fallback)
        Mock::given(method("PUT"))
            .and(path("/api/v1/repos/owner/repo/push"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "commitSha": "retry123",
                "version": 4,
                "filesChanged": 2,
                "created": false
            })))
            .with_priority(2)
            .mount(&mock_server)
            .await;

        // 409 response with higher priority, only once
        Mock::given(method("PUT"))
            .and(path("/api/v1/repos/owner/repo/push"))
            .respond_with(
                ResponseTemplate::new(409)
                    .set_body_json(serde_json::json!({"error": "missing_blobs"})),
            )
            .with_priority(1)
            .up_to_n_times(1)
            .mount(&mock_server)
            .await;

        let temp_dir = tempfile::tempdir().unwrap();
        std::fs::write(temp_dir.path().join("a.txt"), "new-a").unwrap();
        std::fs::write(temp_dir.path().join("b.txt"), "old-b").unwrap();
        std::fs::write(
            temp_dir.path().join(".syns.yaml"),
            "owner: owner\nname: repo\n",
        )
        .unwrap();

        let cache_dir = tempfile::tempdir().unwrap();
        let mut manifest = Manifest::default();
        manifest.update(
            "base-sha".into(),
            HashMap::from([
                ("a.txt".into(), blob_sha1(b"old-a")),
                ("b.txt".into(), blob_sha1(b"old-b")),
            ]),
        );
        manifest.save(cache_dir.path(), "owner", "repo").unwrap();

        let client = SynsClient::new(&mock_server.uri()).unwrap();

        let result = smart_push(
            &client,
            "test-token",
            "owner/repo",
            temp_dir.path(),
            SmartPushOptions {
                force: false,
                message: "retry".into(),
                author: "owner".into(),
                parent_sha: None,
                excludes: vec![],
                cache_dir: cache_dir.path().to_path_buf(),
                description: None,
                tags: None,
                status: None,
                visibility: None,
                strict: false,
                allow_empty: false,
                debug: false,
                no_default_excludes: false,
            },
        )
        .await;

        assert!(result.is_ok());

        let requests = mock_server.received_requests().await.unwrap();
        let put_requests: Vec<_> = requests
            .iter()
            .filter(|r| r.method == reqwest::Method::PUT)
            .collect();
        assert_eq!(put_requests.len(), 2, "should have made 2 PUT requests");

        // First request: b.txt should be sha-only (unchanged per manifest)
        let body1: serde_json::Value = serde_json::from_slice(&put_requests[0].body).unwrap();
        let b_entry1 = body1["files"]
            .as_array()
            .unwrap()
            .iter()
            .find(|f| f["path"] == "b.txt")
            .unwrap()
            .clone();
        assert!(b_entry1["content"].is_null());

        // Second request (retry): b.txt should have content (upgraded)
        let body2: serde_json::Value = serde_json::from_slice(&put_requests[1].body).unwrap();
        let b_entry2 = body2["files"]
            .as_array()
            .unwrap()
            .iter()
            .find(|f| f["path"] == "b.txt")
            .unwrap()
            .clone();
        assert_eq!(b_entry2["content"].as_str(), Some("old-b"));
    }

    #[tokio::test]
    async fn force_bypasses_manifest_sends_all() {
        let mock_server = MockServer::start().await;

        Mock::given(method("PUT"))
            .and(path("/api/v1/repos/owner/repo/push"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "commitSha": "force123",
                "version": 5,
                "filesChanged": 2,
                "created": false
            })))
            .mount(&mock_server)
            .await;

        let temp_dir = tempfile::tempdir().unwrap();
        std::fs::write(temp_dir.path().join("a.txt"), "aaa").unwrap();
        std::fs::write(temp_dir.path().join("b.txt"), "bbb").unwrap();
        std::fs::write(
            temp_dir.path().join(".syns.yaml"),
            "owner: owner\nname: repo\n",
        )
        .unwrap();

        let cache_dir = tempfile::tempdir().unwrap();
        let mut manifest = Manifest::default();
        manifest.update(
            "old-sha".into(),
            HashMap::from([
                ("a.txt".into(), blob_sha1(b"aaa")),
                ("b.txt".into(), blob_sha1(b"bbb")),
            ]),
        );
        manifest.save(cache_dir.path(), "owner", "repo").unwrap();

        let client = SynsClient::new(&mock_server.uri()).unwrap();

        let result = smart_push(
            &client,
            "test-token",
            "owner/repo",
            temp_dir.path(),
            SmartPushOptions {
                force: true,
                message: "force".into(),
                author: "owner".into(),
                parent_sha: None,
                excludes: vec![],
                cache_dir: cache_dir.path().to_path_buf(),
                description: None,
                tags: None,
                status: None,
                visibility: None,
                strict: false,
                allow_empty: false,
                debug: false,
                no_default_excludes: false,
            },
        )
        .await;

        assert!(result.is_ok());

        let requests = mock_server.received_requests().await.unwrap();
        // Only PUT requests, no GET (force doesn't fetch tree)
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0].method, reqwest::Method::PUT);

        let body: serde_json::Value = serde_json::from_slice(&requests[0].body).unwrap();
        let files = body["files"].as_array().unwrap();
        let a_entry = files.iter().find(|f| f["path"] == "a.txt").unwrap();
        assert!(
            a_entry["content"].is_string(),
            "force should send all content"
        );
        let b_entry = files.iter().find(|f| f["path"] == "b.txt").unwrap();
        assert!(
            b_entry["content"].is_string(),
            "force should send all content"
        );
        assert!(
            body["parentSha"].is_null(),
            "force defaults parent_sha to None"
        );
        assert!(body.get("deletions").is_none() || body["deletions"].is_null());

        // Manifest was still saved
        assert!(cache_dir.path().join("owner").join("repo.json").exists());
    }

    fn opts_with(cache: PathBuf, strict: bool, allow_empty: bool) -> SmartPushOptions {
        SmartPushOptions {
            force: false,
            message: "msg".into(),
            author: "alice".into(),
            parent_sha: None,
            excludes: vec![],
            cache_dir: cache,
            description: None,
            tags: None,
            status: None,
            visibility: None,
            strict,
            allow_empty,
            debug: false,
            no_default_excludes: false,
        }
    }

    #[tokio::test]
    async fn response_with_empty_commit_sha_does_not_write_manifest() {
        let mock_server = MockServer::start().await;

        Mock::given(method("GET"))
            .and(path("/api/v1/repos/alice/repo/tree"))
            .respond_with(
                ResponseTemplate::new(404).set_body_json(serde_json::json!({"error": "not_found"})),
            )
            .mount(&mock_server)
            .await;

        Mock::given(method("PUT"))
            .and(path("/api/v1/repos/alice/repo/push"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "commitSha": "",
                "version": 0,
                "filesChanged": 0,
                "created": true
            })))
            .mount(&mock_server)
            .await;

        let temp_dir = tempfile::tempdir().unwrap();
        std::fs::write(temp_dir.path().join("text.txt"), "hello").unwrap();
        let cache_dir = tempfile::tempdir().unwrap();
        let client = SynsClient::new(&mock_server.uri()).unwrap();

        let result = smart_push(
            &client,
            "test-token",
            "alice/repo",
            temp_dir.path(),
            opts_with(cache_dir.path().to_path_buf(), false, false),
        )
        .await;

        let (response, _raw, meta) = result.unwrap();
        assert_eq!(response.commit_sha, "");
        assert!(!meta.manifest_existed);
        assert!(
            !cache_dir.path().join("alice").join("repo.json").exists(),
            "manifest must NOT be persisted when commit_sha is empty"
        );
    }

    #[tokio::test]
    async fn legacy_stub_treated_as_no_manifest() {
        let mock_server = MockServer::start().await;

        Mock::given(method("GET"))
            .and(path("/api/v1/repos/alice/repo/tree"))
            .respond_with(
                ResponseTemplate::new(404).set_body_json(serde_json::json!({"error": "not_found"})),
            )
            .mount(&mock_server)
            .await;

        Mock::given(method("PUT"))
            .and(path("/api/v1/repos/alice/repo/push"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "commitSha": "realsha",
                "version": 1,
                "filesChanged": 1,
                "created": true
            })))
            .mount(&mock_server)
            .await;

        let temp_dir = tempfile::tempdir().unwrap();
        std::fs::write(temp_dir.path().join("text.txt"), "hello").unwrap();

        let cache_dir = tempfile::tempdir().unwrap();
        let stub_path = cache_dir.path().join("alice").join("repo.json");
        std::fs::create_dir_all(stub_path.parent().unwrap()).unwrap();
        std::fs::write(&stub_path, r#"{"commit_sha":"","files":{}}"#).unwrap();

        let client = SynsClient::new(&mock_server.uri()).unwrap();

        let result = smart_push(
            &client,
            "test-token",
            "alice/repo",
            temp_dir.path(),
            opts_with(cache_dir.path().to_path_buf(), false, false),
        )
        .await;

        let (_response, _raw, meta) = result.unwrap();
        assert!(
            !meta.manifest_existed,
            "stub should be rejected by Manifest::load"
        );

        // After call: cache file is overwritten by Phase 6 save.
        let body = std::fs::read_to_string(&stub_path).unwrap();
        let v: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(v["commit_sha"].as_str(), Some("realsha"));
        assert!(v["files"].as_object().unwrap().contains_key("text.txt"));

        // Verify wire request had parentSha: null (no manifest carried forward).
        let requests = mock_server.received_requests().await.unwrap();
        let put_request = requests
            .iter()
            .find(|r| r.method == reqwest::Method::PUT)
            .unwrap();
        let body: serde_json::Value = serde_json::from_slice(&put_request.body).unwrap();
        assert!(body["parentSha"].is_null());
    }

    #[tokio::test]
    async fn strict_mode_aborts_when_files_skipped() {
        let mock_server = MockServer::start().await;
        // No mock mounted — strict guard fires before any wire call.

        let temp_dir = tempfile::tempdir().unwrap();
        std::fs::write(temp_dir.path().join("text.txt"), "hello").unwrap();
        std::fs::write(temp_dir.path().join("binary.bin"), b"data\x00more").unwrap();

        let cache_dir = tempfile::tempdir().unwrap();
        let client = SynsClient::new(&mock_server.uri()).unwrap();

        let result = smart_push(
            &client,
            "test-token",
            "alice/repo",
            temp_dir.path(),
            opts_with(cache_dir.path().to_path_buf(), true, false),
        )
        .await;

        match result {
            Err(CliError::PushPartial { skipped, .. }) => {
                assert_eq!(skipped.len(), 1);
                assert_eq!(skipped[0].path, "binary.bin");
            }
            other => panic!("expected PushPartial, got {other:?}"),
        }

        let requests = mock_server.received_requests().await.unwrap();
        assert!(
            requests.is_empty(),
            "strict guard must fire before any wire call"
        );
    }

    #[tokio::test]
    async fn empty_collection_aborts_with_push_empty() {
        let mock_server = MockServer::start().await;

        // Pre-create .syns.yaml so Phase 1 is a no-op, then use a
        // catch-all --exclude pattern so the collector produces an
        // empty `files` map. (Phase 1 always writes .syns.yaml if
        // absent, so the dir is never literally empty post-Phase 1;
        // the SPEC's "empty collection" scenario is therefore tested
        // by ensuring `files` is empty, not the dir.)
        let temp_dir = tempfile::tempdir().unwrap();
        std::fs::write(
            temp_dir.path().join(".syns.yaml"),
            "owner: alice\nname: repo\n",
        )
        .unwrap();

        let cache_dir = tempfile::tempdir().unwrap();
        let client = SynsClient::new(&mock_server.uri()).unwrap();

        let mut opts = opts_with(cache_dir.path().to_path_buf(), false, false);
        opts.excludes = vec!["*".to_string()];

        let result = smart_push(&client, "test-token", "alice/repo", temp_dir.path(), opts).await;

        match result {
            Err(CliError::PushEmpty {
                path: _,
                total_walked: _,
                cause,
            }) => {
                // CODE_REVIEW M6: assert the SPECIFIC user-exclude
                // cause. Setup uses `excludes: ["*"]`, so the only
                // skipped file (.syns.yaml) lands in UserExclude and
                // the cause sentence MUST contain "--exclude pattern".
                // Previously the assertion `contains("exclude")` would
                // also pass for the DefaultExcludeDir, Gitignore, and
                // Synsignore majority strings — a regression that
                // misattributed user-exclude as gitignore would have
                // gone unnoticed.
                assert!(cause.contains("--exclude pattern"), "cause was: {cause}");
            }
            other => panic!("expected PushEmpty, got {other:?}"),
        }

        let requests = mock_server.received_requests().await.unwrap();
        assert!(requests.is_empty());
    }

    #[tokio::test]
    async fn allow_empty_bypasses_push_empty_guard() {
        let mock_server = MockServer::start().await;

        Mock::given(method("GET"))
            .and(path("/api/v1/repos/alice/repo/tree"))
            .respond_with(
                ResponseTemplate::new(404).set_body_json(serde_json::json!({"error": "not_found"})),
            )
            .mount(&mock_server)
            .await;

        Mock::given(method("PUT"))
            .and(path("/api/v1/repos/alice/repo/push"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "commitSha": "abc",
                "version": 1,
                "filesChanged": 0,
                "created": true
            })))
            .mount(&mock_server)
            .await;

        // Pre-create .syns.yaml + catch-all exclude → empty `files`
        // collection; --allow-empty should let the wire call proceed.
        let temp_dir = tempfile::tempdir().unwrap();
        std::fs::write(
            temp_dir.path().join(".syns.yaml"),
            "owner: alice\nname: repo\n",
        )
        .unwrap();
        let cache_dir = tempfile::tempdir().unwrap();
        let client = SynsClient::new(&mock_server.uri()).unwrap();

        let mut opts = opts_with(cache_dir.path().to_path_buf(), false, true);
        opts.excludes = vec!["*".to_string()];

        let result = smart_push(&client, "test-token", "alice/repo", temp_dir.path(), opts).await;

        let (_response, _raw, _meta) = result.unwrap();
        // meta.skipped is non-empty here (the .syns.yaml is excluded
        // by `*`), but the wire call proceeds because of --allow-empty.

        let requests = mock_server.received_requests().await.unwrap();
        let put_requests: Vec<_> = requests
            .iter()
            .filter(|r| r.method == reqwest::Method::PUT)
            .collect();
        assert_eq!(put_requests.len(), 1);
    }

    #[tokio::test]
    async fn strict_supersedes_empty_when_both_apply() {
        let mock_server = MockServer::start().await;

        let temp_dir = tempfile::tempdir().unwrap();
        std::fs::write(temp_dir.path().join("binary.bin"), b"data\x00").unwrap();
        let cache_dir = tempfile::tempdir().unwrap();
        let client = SynsClient::new(&mock_server.uri()).unwrap();

        let result = smart_push(
            &client,
            "test-token",
            "alice/repo",
            temp_dir.path(),
            opts_with(cache_dir.path().to_path_buf(), true, false),
        )
        .await;

        match result {
            Err(CliError::PushPartial { skipped, .. }) => {
                assert_eq!(skipped.len(), 1);
            }
            Err(CliError::PushEmpty { .. }) => {
                panic!("strict must supersede empty when both apply (D10)");
            }
            other => panic!("expected PushPartial, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn smart_push_auto_chunks_when_pre_flight_body_exceeds_budget() {
        let mock_server = MockServer::start().await;

        // First-push 404 on /tree so smart_push treats this as first push.
        Mock::given(method("GET"))
            .and(path("/api/v1/repos/alice/repo/tree"))
            .respond_with(
                ResponseTemplate::new(404).set_body_json(serde_json::json!({"error": "not_found"})),
            )
            .mount(&mock_server)
            .await;

        // First PUT → batch 1 response (commitSha aaaa...001).
        Mock::given(method("PUT"))
            .and(path("/api/v1/repos/alice/repo/push"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "commitSha": "aaaaaaaa00000000000000000000000000000001",
                "version": 1,
                "filesChanged": 1,
                "created": true,
            })))
            .with_priority(1)
            .up_to_n_times(1)
            .mount(&mock_server)
            .await;

        // Subsequent PUT → batch 2 response (commitSha bbbb...002).
        Mock::given(method("PUT"))
            .and(path("/api/v1/repos/alice/repo/push"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "commitSha": "bbbbbbbb00000000000000000000000000000002",
                "version": 2,
                "filesChanged": 2,
                "created": false,
            })))
            .with_priority(2)
            .mount(&mock_server)
            .await;

        let temp_dir = tempfile::tempdir().unwrap();
        std::fs::write(temp_dir.path().join("big1.txt"), vec![b'a'; 14 * 1024 * 1024]).unwrap();
        std::fs::write(temp_dir.path().join("big2.txt"), vec![b'b'; 14 * 1024 * 1024]).unwrap();
        std::fs::write(temp_dir.path().join("small.txt"), vec![b'c'; 1024]).unwrap();

        let cache_dir = tempfile::tempdir().unwrap();
        let client = SynsClient::new(&mock_server.uri()).unwrap();

        let result = smart_push(
            &client,
            "test-token",
            "alice/repo",
            temp_dir.path(),
            SmartPushOptions {
                force: false,
                message: "init".into(),
                author: "alice".into(),
                parent_sha: None,
                excludes: vec![],
                cache_dir: cache_dir.path().to_path_buf(),
                description: None,
                tags: None,
                status: None,
                visibility: None,
                strict: false,
                allow_empty: false,
                debug: false,
                no_default_excludes: false,
            },
        )
        .await;

        let (response, _raw, _meta) = result.expect("smart_push must succeed with chunker");
        assert_eq!(
            response.commit_sha, "bbbbbbbb00000000000000000000000000000002",
            "final response should carry the FINAL batch's commit_sha",
        );

        let requests = mock_server.received_requests().await.unwrap();
        let puts: Vec<_> = requests
            .iter()
            .filter(|r| r.method == reqwest::Method::PUT)
            .collect();
        assert_eq!(puts.len(), 2, "expected exactly 2 PUTs from the chunker");

        let body1: serde_json::Value = serde_json::from_slice(&puts[0].body).unwrap();
        assert!(
            body1["parentSha"].is_null() || body1.get("parentSha").is_none(),
            "first batch parentSha should be null/absent, got: {}",
            body1["parentSha"]
        );
        assert!(
            body1["message"]
                .as_str()
                .is_some_and(|m| m.contains("(part 1/2)")),
            "first batch message should include '(part 1/2)', got: {:?}",
            body1["message"]
        );

        let body2: serde_json::Value = serde_json::from_slice(&puts[1].body).unwrap();
        assert_eq!(
            body2["parentSha"],
            serde_json::Value::String("aaaaaaaa00000000000000000000000000000001".into()),
        );
        assert!(
            body2["message"]
                .as_str()
                .is_some_and(|m| m.contains("(part 2/2)")),
        );

        let manifest = Manifest::load(cache_dir.path(), "alice", "repo")
            .expect("manifest should be saved after successful chunked push");
        assert_eq!(
            manifest.commit_sha(),
            Some("bbbbbbbb00000000000000000000000000000002"),
        );
        let stored: std::collections::HashSet<&str> = manifest.file_paths().collect();
        assert!(stored.contains("big1.txt"));
        assert!(stored.contains("big2.txt"));
        assert!(stored.contains("small.txt"));
    }

    #[tokio::test]
    async fn smart_push_propagates_payload_too_large_with_offending_batch_metrics_when_single_file_too_big()
     {
        let mock_server = MockServer::start().await;

        // First-push 404 on /tree.
        Mock::given(method("GET"))
            .and(path("/api/v1/repos/alice/repo/tree"))
            .respond_with(
                ResponseTemplate::new(404).set_body_json(serde_json::json!({"error": "not_found"})),
            )
            .mount(&mock_server)
            .await;

        // Every PUT returns 413 with Cloudflare HTML body (real edge response).
        Mock::given(method("PUT"))
            .and(path("/api/v1/repos/alice/repo/push"))
            .respond_with(
                ResponseTemplate::new(413).set_body_string(
                    "<html><head><title>413 Request Entity Too Large</title></head><body>\n<center>cloudflare</center>\n</body></html>",
                ),
            )
            .mount(&mock_server)
            .await;

        let temp_dir = tempfile::tempdir().unwrap();
        // One file at 30 MiB — exceeds CHUNK_BUDGET_BYTES on its own.
        let single_size = 30 * 1024 * 1024;
        std::fs::write(temp_dir.path().join("huge.txt"), vec![b'x'; single_size]).unwrap();

        let cache_dir = tempfile::tempdir().unwrap();
        let client = SynsClient::new(&mock_server.uri()).unwrap();

        let result = smart_push(
            &client,
            "test-token",
            "alice/repo",
            temp_dir.path(),
            SmartPushOptions {
                force: false,
                message: "init".into(),
                author: "alice".into(),
                parent_sha: None,
                excludes: vec![],
                cache_dir: cache_dir.path().to_path_buf(),
                description: None,
                tags: None,
                status: None,
                visibility: None,
                strict: false,
                allow_empty: false,
                debug: false,
                no_default_excludes: false,
            },
        )
        .await;

        match result {
            Err(CliError::PayloadTooLarge {
                bytes_sent,
                file_count,
                rejecter,
            }) => {
                assert_eq!(
                    bytes_sent, single_size as u64,
                    "bytes_sent should be the offending batch's content sum (30 MiB), got {bytes_sent}",
                );
                assert_eq!(file_count, 1, "file_count should be the batch size (1)");
                assert!(
                    matches!(rejecter, EdgeRejecter::Cloudflare),
                    "rejecter should be Cloudflare from the HTML body sniff, got {rejecter:?}",
                );
            }
            other => panic!("expected Err(PayloadTooLarge), got: {other:?}"),
        }
    }
}
