use std::path::Path;

use crate::errors::CliError;
use crate::output::Output;
use crate::repo::resolve::{RepoIdentity, resolve_repo_identity};

/// Resolves a `RepoIdentity` or emits the silent-skip envelope and returns `Ok(None)`.
///
/// Used by `cmd_push`, which tolerates `identity.owner.is_none()` because it
/// resolves the owner downstream via cached username / `get_session`.
///
/// When `if_repo` is true and the underlying resolver returned
/// `Err(CliError::RepoIdentityUnknown)` (no `--name`, no `.syns.yaml`, no
/// `origin` remote), this helper calls `output.skip()` (which emits the
/// single-line JSON envelope on stdout in JSON mode, nothing in default
/// mode) and returns `Ok(None)`. Every other error variant is propagated
/// verbatim; `--if-repo` does NOT suppress malformed-`.syns.yaml` errors
/// or any other failure class.
pub fn resolve_or_skip(
    name_flag: Option<&str>,
    path: &Path,
    if_repo: bool,
    output: &Output,
) -> Result<Option<RepoIdentity>, CliError> {
    match resolve_repo_identity(name_flag, path) {
        Ok(identity) => Ok(Some(identity)),
        Err(CliError::RepoIdentityUnknown) if if_repo => {
            output.skip();
            Ok(None)
        }
        Err(e) => Err(e),
    }
}

/// Resolves a complete `(owner, name)` pair or emits the silent-skip
/// envelope and returns `Ok(None)`.
///
/// Used by every resolver-using command except `cmd_push`. Suppresses both
/// (a) the resolver's own `CliError::RepoIdentityUnknown` and (b) the
/// synthetic `RepoIdentityUnknown` that occurs when the resolver returns
/// `Ok(identity)` with `identity.owner == None` (e.g., git remote provided
/// only a name). Both branches under `if_repo: true` emit skip and return
/// `Ok(None)`; without `if_repo`, the function returns
/// `Err(CliError::RepoIdentityUnknown)` to match today's behavior.
pub fn resolve_full_or_skip(
    name_flag: Option<&str>,
    path: &Path,
    if_repo: bool,
    output: &Output,
) -> Result<Option<(String, String)>, CliError> {
    match resolve_or_skip(name_flag, path, if_repo, output)? {
        None => Ok(None),
        Some(identity) => match identity.owner {
            Some(owner) => Ok(Some((owner, identity.name))),
            None if if_repo => {
                output.skip();
                Ok(None)
            }
            None => Err(CliError::RepoIdentityUnknown),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn resolve_or_skip_returns_some_when_resolver_succeeds() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(
            dir.path().join(".syns.yaml"),
            "owner: alice\nname: my-repo\n",
        )
        .unwrap();
        let output = Output::new(false);

        let result = resolve_or_skip(None, dir.path(), false, &output);
        assert!(matches!(
            result,
            Ok(Some(RepoIdentity { owner: Some(ref o), ref name })) if o == "alice" && name == "my-repo"
        ));
    }

    #[test]
    fn resolve_or_skip_returns_none_with_if_repo_when_resolver_misses() {
        let dir = tempfile::tempdir().unwrap();
        let output = Output::new(true);

        let result = resolve_or_skip(None, dir.path(), true, &output);
        assert!(matches!(result, Ok(None)));
    }

    #[test]
    fn resolve_or_skip_returns_err_without_if_repo_when_resolver_misses() {
        let dir = tempfile::tempdir().unwrap();
        let output = Output::new(false);

        let result = resolve_or_skip(None, dir.path(), false, &output);
        assert!(matches!(result, Err(CliError::RepoIdentityUnknown)));
    }

    #[test]
    fn resolve_or_skip_propagates_malformed_yaml_even_with_if_repo() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join(".syns.yaml"), "not: valid: yaml: [[").unwrap();
        let output = Output::new(false);

        let result = resolve_or_skip(None, dir.path(), true, &output);
        assert!(matches!(result, Err(CliError::Io { .. })));
    }

    #[test]
    fn resolve_full_or_skip_returns_pair_when_owner_present() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join(".syns.yaml"), "owner: bob\nname: repo\n").unwrap();
        let output = Output::new(false);

        let result = resolve_full_or_skip(None, dir.path(), false, &output);
        assert!(matches!(
            result,
            Ok(Some((ref o, ref n))) if o == "bob" && n == "repo"
        ));
    }

    #[test]
    fn resolve_full_or_skip_returns_none_with_if_repo_when_owner_missing() {
        let dir = tempfile::tempdir().unwrap();
        // Build .git/config with origin remote that yields name-only RepoIdentity
        let git_dir = dir.path().join(".git");
        fs::create_dir_all(&git_dir).unwrap();
        fs::write(
            git_dir.join("config"),
            "[remote \"origin\"]\n\turl = https://github.com/user/just-name.git\n",
        )
        .unwrap();
        let output = Output::new(true);

        let result = resolve_full_or_skip(None, dir.path(), true, &output);
        assert!(matches!(result, Ok(None)));
    }
}
