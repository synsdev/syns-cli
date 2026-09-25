use crate::auth::token::TokenStore;
use crate::client::{EntryType, SynsClient};
use crate::commands::sync::{convergence_options_for, render_outcome, render_transfer_lines};
use crate::config::Config;
use crate::errors::{CliError, IdentityRemedy};
use crate::output::Output;
use crate::push::collector::{HELD_BYTES_BUDGET, HeldBytes};
use crate::push::converge::{
    ConvergeMode, Staging, SyncOutcome, converge, read_blobs, replace_file_whole,
};
use crate::push::working_copy::WorkingCopy;
use crate::repo::if_repo::resolve_full_or_skip;
use crate::repo::root::resolve_start_path;
use crate::repo::syns_yaml::{nearest_identity, write_syns_yaml_where_none_stands};
use console::style;
use serde_json::json;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// Whether a positional is spelt as a repository: one `/` joining an owner
/// of 1–39 ASCII letters, digits or `-` to a name of 1–100 ASCII letters,
/// digits, `.`, `_` or `-` opening on a letter or digit, either letter
/// case accepted (SPEC u262, `D-024`, `D-025`).
pub fn is_repository_shape(value: &str) -> bool {
    let Some((owner, name)) = value.split_once('/') else {
        return false;
    };
    let owner_ok = (1..=39).contains(&owner.len())
        && owner
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-');
    let name_ok = (1..=100).contains(&name.len())
        && name.as_bytes()[0].is_ascii_alphanumeric()
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'));
    owner_ok && name_ok
}

/// The `syns pull` positionals as bound: `repository` holds the owner and
/// the name, both lower-cased.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PullPositionals {
    pub repository: Option<(String, String)>,
    pub path: Option<String>,
}

/// A first of two positionals lacking the repository shape, exactly as
/// typed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PositionalRefusal {
    pub value: String,
}

/// Binds the `syns pull` positionals by spelling alone, reading no
/// filesystem state, so one argument vector binds identically in every
/// directory (SPEC u262 `bind_pull_positionals` 1–2).
pub fn bind_pull_positionals(
    first: Option<String>,
    second: Option<String>,
) -> Result<PullPositionals, PositionalRefusal> {
    match (first, second) {
        (Some(first), Some(second)) => match split_repository(&first) {
            Some(repository) => Ok(PullPositionals {
                repository: Some(repository),
                path: Some(second),
            }),
            None => Err(PositionalRefusal { value: first }),
        },
        (Some(lone), None) | (None, Some(lone)) => Ok(match split_repository(&lone) {
            Some(repository) => PullPositionals {
                repository: Some(repository),
                path: None,
            },
            None => PullPositionals {
                repository: None,
                path: Some(lone),
            },
        }),
        (None, None) => Ok(PullPositionals {
            repository: None,
            path: None,
        }),
    }
}

/// The lower-cased owner and name of a value with the repository shape.
fn split_repository(value: &str) -> Option<(String, String)> {
    if !is_repository_shape(value) {
        return None;
    }
    let (owner, name) = value.split_once('/')?;
    Some((owner.to_ascii_lowercase(), name.to_ascii_lowercase()))
}

/// The `INV-30` test a server-named path passes before a retrieval
/// writes it — the one a convergence applies, so both retrievals refuse
/// the same paths.
fn validate_entry_path(path: &str) -> Result<(), CliError> {
    crate::push::converge::check_server_path(path)
}

fn safe_join(target_dir: &Path, entry_path: &str) -> Result<PathBuf, CliError> {
    validate_entry_path(entry_path)?;
    let joined = target_dir.join(entry_path);
    if !joined.starts_with(target_dir) {
        return Err(CliError::Io {
            message: format!("refusing path outside target directory: {entry_path}"),
        });
    }
    Ok(joined)
}

/// Write the identity file at the write root where the registered
/// retrieval would, and only where no identity file stands there.
fn write_identity_file(
    repository: &Option<(String, String)>,
    target_dir: &Path,
    owner: &str,
    name: &str,
) -> Result<(), CliError> {
    // SPEC u255 `cmd_pull` 4: the marker is written after every
    // fetched path stands and every reconciled removal is taken
    // (`pull-write-then-delete`), and only where no `.syns.yaml` at or
    // above the write root already names this repository — otherwise a
    // retrieval run from `repo/sub/` entrenches `sub/` as a repository
    // of its own and every later publication from there resolves to it.
    // SPEC u256: nor over an identity file standing at the write root,
    // whose declared checks a rewrite would drop.
    if repository.is_some() {
        write_syns_yaml_where_none_stands(target_dir, owner, name)?;
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
pub async fn cmd_pull(
    config: &Config,
    output: &Output,
    repository: Option<(String, String)>,
    path_arg: Option<String>,
    version: Option<String>,
    if_repo: bool,
    overwrite_local: bool,
) -> Result<(), CliError> {
    // SPEC u263 `cmd_pull` 1: the directory the identity walk starts at,
    // in ABSOLUTE form — see `resolve_start_path`. This is NOT yet the
    // write root: a bare retrieval writes into the repository root,
    // whichever descendant of it the run started in.
    let start_dir = resolve_start_path(path_arg.as_deref().map(Path::new))?;

    // `cmd_pull` 2: only a run naming no repository resolves one.
    let bound = repository.is_some();
    let (owner, name) = match &repository {
        Some(pair) => pair.clone(),
        None => {
            // `syns pull` accepts the repository as a positional, so its
            // refusal names that, and the directory a path argument named.
            let remedy = IdentityRemedy::RepositoryPositional {
                path: path_arg.as_ref().map(|_| start_dir.clone()),
            };
            match resolve_full_or_skip(None, &start_dir, if_repo, output)
                .map_err(|err| err.with_identity_remedy(remedy))?
            {
                Some(pair) => pair,
                None => return Ok(()),
            }
        }
    };

    // `cmd_pull` 3: the nearest identity file, read by its local side, is
    // the one read every later decision of this run takes.
    let standing = nearest_identity(&start_dir)?;

    // `cmd_pull` 4: `--if-repo` skips wherever no identity file stands at
    // or above the starting directory, a repository named on the command
    // line included.
    if bound && if_repo && standing.is_none() {
        output.skip();
        return Ok(());
    }

    // `cmd_pull` 5 (issue 130): a repository named on the command line is
    // refused wherever the nearest identity file names another — a bare
    // run, a path argument and a run from a sub-folder alike — before any
    // request, credential read or directory creation, so two repositories
    // never mix in one directory.
    if bound
        && let Some(standing) = &standing
        && !standing.names(&owner, &name)
    {
        return Err(CliError::PathBelongsToAnotherRepository {
            path: standing.dir.clone(),
            standing: format!("{}/{}", standing.owner, standing.name),
            requested: format!("{owner}/{name}"),
        });
    }

    // `cmd_pull` 6: the retrieval's ONE write root. Every fetched path is
    // joined under it, every reconciled removal is taken from it, and the
    // identity file is written into it. A path argument still names a
    // destination rather than a scope.
    let target_dir = match (&path_arg, &standing) {
        (Some(_), _) => start_dir.clone(),
        (None, Some(standing)) if standing.names(&owner, &name) => standing.dir.clone(),
        _ => start_dir.clone(),
    };

    let repo_id = format!("{owner}/{name}");
    let token = TokenStore::new(config.credentials_path())
        .read()
        .ok()
        .flatten();
    let client = SynsClient::new(config.server_url())?;

    if let Some(version) = version {
        return pull_snapshot(
            output,
            &client,
            token.as_deref(),
            &repo_id,
            &repository,
            &target_dir,
            &owner,
            &name,
            &version,
            config.cache_dir(),
        )
        .await;
    }

    // SPEC u256 `cmd_pull` 3: a retrieval converges, leaving every local
    // edit standing unless the overwrite option asks otherwise.
    std::fs::create_dir_all(&target_dir).map_err(|e| CliError::Io {
        message: format!("could not create target directory: {e}"),
    })?;
    let copy = WorkingCopy::open(config.cache_dir(), &owner, &name, &target_dir)?;
    let outcome = converge(
        &client,
        token.as_deref(),
        &copy,
        ConvergeMode::Retrieve {
            overwrite: overwrite_local,
        },
        convergence_options_for(config, output),
    )
    .await?;

    // `cmd_pull` 4
    write_identity_file(&repository, &target_dir, &owner, &name)?;

    let (written, removed) = match outcome {
        SyncOutcome::Synced {
            written, removed, ..
        } => (written, removed),
        SyncOutcome::NoChanges => (Vec::new(), Vec::new()),
        other => return render_outcome(output, Some(&repo_id), other, None),
    };

    if !output.is_json() {
        render_transfer_lines(&written, &removed);
    }

    // The local record the registered forced or scoped publication reads
    // follows the base the convergence recorded.
    let base = copy.base();
    if let Some(base) = &base {
        base.save(config.cache_dir(), &owner, &name)?;
    }
    let commit_sha = base.as_ref().and_then(|b| b.commit_sha().map(String::from));
    let head_count = base.as_ref().map(|b| b.file_paths().count()).unwrap_or(0);
    let downloaded = written.len();
    let unchanged = head_count.saturating_sub(downloaded);
    let deleted = removed.len();

    if output.is_json() {
        output.json(&json!({
            "repo": repo_id,
            "commitSha": commit_sha,
            "downloaded": downloaded,
            "unchanged": unchanged,
            "deleted": deleted,
        }));
    } else {
        output.success(&format!(
            "Pulled {repo_id}: {downloaded} downloaded, {unchanged} unchanged, {deleted} deleted"
        ));
    }

    Ok(())
}

/// The registered snapshot retrieval at `--version`: every file at that
/// version written, nothing removed, no record kept — but for a local file
/// an ignore rule excludes, which is named and left untouched as a
/// convergence leaves it. Every path is checked and read, each answer
/// verified by its hash and held or staged, before the first file is
/// written (SPEC u280 `pull_snapshot` 1–2).
#[allow(clippy::too_many_arguments)]
async fn pull_snapshot(
    output: &Output,
    client: &SynsClient,
    token: Option<&str>,
    repo_id: &str,
    repository: &Option<(String, String)>,
    target_dir: &Path,
    owner: &str,
    name: &str,
    version: &str,
    cache_dir: &Path,
) -> Result<(), CliError> {
    let (tree_response, _raw) = client
        .get_tree(repo_id, token, None, true, Some(version))
        .await?;

    let server_files: Vec<_> = tree_response
        .entries
        .iter()
        .filter(|e| e.entry_type == EntryType::File)
        .collect();
    // 1 — every path the version's tree carries checked before anything
    // is read or written.
    for entry in &server_files {
        validate_entry_path(&entry.path)?;
    }

    let staging = Staging::open(cache_dir)?;
    std::fs::create_dir_all(target_dir).map_err(|e| CliError::Io {
        message: format!("could not create target directory: {e}"),
    })?;

    let held = HeldBytes::new(HELD_BYTES_BUDGET);
    let collected = crate::push::collector::collect_files(
        target_dir,
        &[],
        crate::push::collector::CollectOptions::default(),
        None,
        &held,
    )?;
    let mut excluded = crate::push::converge::excluded_local_files(
        target_dir,
        |path| collected.files.contains_key(path),
        server_files.iter().map(|e| &e.path),
    );
    drop(collected);

    // SPEC u263 `cmd_pull` 7: an identity file already standing at the
    // write root is kept as it stands, counted neither downloaded nor
    // excluded.
    let identity_stands = target_dir.join(".syns.yaml").is_file();
    if identity_stands {
        excluded.remove(".syns.yaml");
    }
    let kept = usize::from(identity_stands && server_files.iter().any(|e| e.path == ".syns.yaml"));

    let wanted: BTreeMap<String, (String, Option<u64>)> = server_files
        .iter()
        .filter(|e| !(identity_stands && e.path == ".syns.yaml"))
        .filter(|e| !excluded.contains(&e.path))
        .map(|e| (e.path.clone(), (e.sha.clone().unwrap_or_default(), e.size)))
        .collect();
    let blobs = read_blobs(client, token, repo_id, version, &wanted, &held, &staging).await?;

    for entry in &server_files {
        if identity_stands && entry.path == ".syns.yaml" {
            continue;
        }
        if excluded.contains(&entry.path) {
            if !output.is_json() {
                eprintln!("  {}", style(format!("excluded: {}", entry.path)).yellow());
            }
            continue;
        }
        let Some(content) = blobs.get(&entry.path) else {
            continue;
        };
        safe_join(target_dir, &entry.path)?;
        // 2 — replaced whole, so a run killed part-way leaves the file as
        // it stood rather than torn.
        replace_file_whole(target_dir, &entry.path, content)?;
        if !output.is_json() {
            eprintln!("  {}", style(format!("downloaded: {}", entry.path)).green());
        }
    }
    drop(blobs);

    write_identity_file(repository, target_dir, owner, name)?;

    let downloaded = server_files.len() - excluded.len() - kept;
    if output.is_json() {
        output.json(&json!({
            "repo": repo_id,
            "commitSha": tree_response.commit_sha,
            "downloaded": downloaded,
            "unchanged": 0,
            "deleted": 0,
            "excluded": excluded,
            "version": version,
        }));
    } else {
        output.success(&format!(
            "Pulled {repo_id}: {downloaded} downloaded, 0 unchanged, 0 deleted"
        ));
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serial_test::serial;

    #[test]
    fn validate_entry_path_accepts_normal_paths() {
        assert!(validate_entry_path("README.md").is_ok());
        assert!(validate_entry_path("src/main.rs").is_ok());
        assert!(validate_entry_path("a/b/c/d.txt").is_ok());
        assert!(validate_entry_path(".hidden").is_ok());
        assert!(validate_entry_path("dir/.hidden/file").is_ok());
    }

    #[test]
    fn validate_entry_path_rejects_dot_dot() {
        assert!(validate_entry_path("..").is_err());
        assert!(validate_entry_path("../etc/passwd").is_err());
        assert!(validate_entry_path("foo/../../etc/passwd").is_err());
        assert!(validate_entry_path("foo/..").is_err());
    }

    #[test]
    fn validate_entry_path_rejects_absolute_paths() {
        assert!(validate_entry_path("/etc/passwd").is_err());
        assert!(validate_entry_path("/tmp/file").is_err());
    }

    #[test]
    fn validate_entry_path_rejects_null_bytes() {
        assert!(validate_entry_path("file\0.txt").is_err());
        assert!(validate_entry_path("\0").is_err());
    }

    #[test]
    fn validate_entry_path_rejects_dot_git() {
        assert!(validate_entry_path(".git").is_err());
        assert!(validate_entry_path(".git/config").is_err());
        assert!(validate_entry_path("foo/.git/hooks").is_err());
    }

    #[test]
    fn validate_entry_path_rejects_empty() {
        assert!(validate_entry_path("").is_err());
    }

    #[test]
    fn safe_join_produces_correct_path() {
        let dir = std::env::temp_dir();
        let result = safe_join(&dir, "src/main.rs").unwrap();
        assert_eq!(result, dir.join("src/main.rs"));
    }

    #[test]
    fn safe_join_rejects_traversal() {
        let dir = std::env::temp_dir();
        assert!(safe_join(&dir, "../../etc/passwd").is_err());
    }

    #[test]
    fn safe_join_rejects_absolute() {
        let dir = std::env::temp_dir();
        assert!(safe_join(&dir, "/etc/passwd").is_err());
    }

    #[test]
    fn validate_entry_path_allows_dot_segments_that_are_not_dot_dot() {
        assert!(validate_entry_path(".gitignore").is_ok());
        assert!(validate_entry_path("src/.gitkeep").is_ok());
        assert!(validate_entry_path("...").is_ok());
    }

    #[tokio::test]
    #[serial]
    async fn pull_with_if_repo_set_and_identity_resolved_runs_normally() {
        let dir = tempfile::tempdir().unwrap();
        let cache = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join(".syns.yaml"),
            "owner: alice\nname: my-project\n",
        )
        .unwrap();
        std::env::set_current_dir(dir.path()).unwrap();
        unsafe { std::env::set_var("SYNS_CONFIG_DIR", dir.path()) };
        unsafe { std::env::set_var("SYNS_CACHE_DIR", cache.path()) };

        let mock_server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path(
                "/api/v1/repos/alice/my-project/tree",
            ))
            .and(wiremock::matchers::query_param("recursive", "true"))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "entries": [],
                    "commitSha": "abc123",
                    "truncated": false
                })),
            )
            .mount(&mock_server)
            .await;

        let config = Config::new(Some(&mock_server.uri())).unwrap();
        let output = Output::new(false);

        let result = cmd_pull(&config, &output, None, None, None, true, false).await;
        unsafe { std::env::remove_var("SYNS_CONFIG_DIR") };
        unsafe { std::env::remove_var("SYNS_CACHE_DIR") };

        assert!(result.is_ok(), "{result:?}");
    }

    #[tokio::test]
    #[serial]
    async fn pull_with_if_repo_set_and_no_identity_skips_silently() {
        let dir = tempfile::tempdir().unwrap();
        std::env::set_current_dir(dir.path()).unwrap();
        unsafe { std::env::set_var("SYNS_CONFIG_DIR", dir.path()) };

        let mock_server = wiremock::MockServer::start().await;
        let config = Config::new(Some(&mock_server.uri())).unwrap();
        let output = Output::new(false);

        let result = cmd_pull(&config, &output, None, None, None, true, false).await;
        unsafe { std::env::remove_var("SYNS_CONFIG_DIR") };

        assert!(result.is_ok());
        assert!(mock_server.received_requests().await.unwrap().is_empty());
    }

    /// SPEC u262 `cmd_pull` 2, inverting the u252-era backstop that let a
    /// positional repository bypass `--if-repo`: the option skips wherever
    /// no identity file stands at or above the starting directory, a
    /// repository named on the command line included.
    #[tokio::test]
    #[serial]
    async fn pull_with_if_repo_and_positional_owner_name_skips_where_no_identity_file_stands() {
        let dir = tempfile::tempdir().unwrap();
        let cache = tempfile::tempdir().unwrap();
        std::env::set_current_dir(dir.path()).unwrap();
        unsafe { std::env::set_var("SYNS_CONFIG_DIR", dir.path()) };
        unsafe { std::env::set_var("SYNS_CACHE_DIR", cache.path()) };

        let mock_server = wiremock::MockServer::start().await;
        let config = Config::new(Some(&mock_server.uri())).unwrap();
        let output = Output::new(false);

        let result = cmd_pull(
            &config,
            &output,
            Some(("alice".into(), "repo".into())),
            None,
            None,
            true,
            false,
        )
        .await;
        unsafe { std::env::remove_var("SYNS_CONFIG_DIR") };
        unsafe { std::env::remove_var("SYNS_CACHE_DIR") };

        assert!(result.is_ok(), "cmd_pull returned: {result:?}");
        assert!(mock_server.received_requests().await.unwrap().is_empty());
        assert!(!dir.path().join(".syns.yaml").exists());
    }

    #[test]
    fn bind_reads_a_lone_owner_and_name_as_the_repository() {
        assert_eq!(
            bind_pull_positionals(Some("Alice/Proj".into()), None),
            Ok(PullPositionals {
                repository: Some(("alice".into(), "proj".into())),
                path: None,
            })
        );
    }

    #[test]
    fn bind_reads_lone_path_spellings_as_the_path() {
        for value in ["/tmp/x", "./somewhere", "../target", ".", "docs", "a/b/c"] {
            assert_eq!(
                bind_pull_positionals(Some(value.into()), None),
                Ok(PullPositionals {
                    repository: None,
                    path: Some(value.into()),
                }),
                "{value}"
            );
        }
    }

    #[test]
    fn bind_takes_repository_and_path_from_two_values() {
        assert_eq!(
            bind_pull_positionals(Some("alice/proj".into()), Some("../x".into())),
            Ok(PullPositionals {
                repository: Some(("alice".into(), "proj".into())),
                path: Some("../x".into()),
            })
        );
    }

    #[test]
    fn bind_refuses_a_first_of_two_lacking_the_repository_shape() {
        assert_eq!(
            bind_pull_positionals(Some("./a".into()), Some("./b".into())),
            Err(PositionalRefusal {
                value: "./a".into()
            })
        );
    }

    #[test]
    fn repository_shape_holds_the_handle_and_name_bounds() {
        let owner_39 = "o".repeat(39);
        let owner_40 = "o".repeat(40);
        let name_100 = "n".repeat(100);
        let name_101 = "n".repeat(101);
        assert!(is_repository_shape(&format!("{owner_39}/proj")));
        assert!(!is_repository_shape(&format!("{owner_40}/proj")));
        assert!(!is_repository_shape("al_ice/proj"));
        assert!(is_repository_shape(&format!("alice/{name_100}")));
        assert!(!is_repository_shape(&format!("alice/{name_101}")));
        assert!(!is_repository_shape("alice/.proj"));
    }

    #[tokio::test]
    #[serial]
    async fn pull_with_if_repo_and_only_git_remote_silent_skips_no_http_call() {
        let dir = tempfile::tempdir().unwrap();
        // Only source: a .git/config with origin remote — no .syns.yaml, no positional.
        let git_dir = dir.path().join(".git");
        std::fs::create_dir_all(&git_dir).unwrap();
        std::fs::write(
            git_dir.join("config"),
            "[remote \"origin\"]\n\turl = https://github.com/user/non-syns-project.git\n",
        )
        .unwrap();

        std::env::set_current_dir(dir.path()).unwrap();
        unsafe { std::env::set_var("SYNS_CONFIG_DIR", dir.path()) };

        // No mocks registered — verifies absence of any HTTP request.
        let mock_server = wiremock::MockServer::start().await;
        let config = Config::new(Some(&mock_server.uri())).unwrap();
        let output = Output::new(false);

        // repo_arg = None, path_arg = None, version = None, if_repo = true.
        // Reaches resolve_full_or_skip, which under u252 emits skip via the
        // narrowed resolve_or_skip and returns Ok(None).
        let result = cmd_pull(&config, &output, None, None, None, true, false).await;
        unsafe { std::env::remove_var("SYNS_CONFIG_DIR") };

        // AC7 representative: a pull from a directory whose only identity source
        // is the git remote silent-skips under --if-repo and makes no HTTP call.
        // Locks the cmd_<read> → helper → short-circuit-before-HTTP linkage for
        // at least one representative read command at the integration level.
        assert!(result.is_ok(), "cmd_pull returned: {result:?}");
        assert!(mock_server.received_requests().await.unwrap().is_empty());
    }
}
