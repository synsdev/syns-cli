use std::path::Path;

use crate::errors::CliError;
use crate::output::Output;
use crate::repo::resolve::{IdentitySource, RepoIdentity, resolve_repo_identity};

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
///
/// Under `if_repo: true`, the helper additionally treats a successful
/// resolve whose `IdentitySource` is not `SynsYaml` (i.e., `NameFlag` or
/// `GitRemote`) as a skip case — emits `output.skip()` and returns
/// `Ok(None)`. Only identity supplied by `.syns.yaml` opens the gate.
pub fn resolve_or_skip(
    name_flag: Option<&str>,
    path: &Path,
    if_repo: bool,
    output: &Output,
) -> Result<Option<RepoIdentity>, CliError> {
    match resolve_repo_identity(name_flag, path) {
        Ok((identity, IdentitySource::SynsYaml)) => Ok(Some(identity)),
        Ok((_identity, _source)) if if_repo => {
            output.skip();
            Ok(None)
        }
        Ok((identity, _source)) => Ok(Some(identity)),
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

    /// CR Low-4 regression backstop. Mirrors
    /// `resolve_full_or_skip_returns_none_with_if_repo_when_owner_missing`
    /// but with `if_repo: false` — the synthetic-`RepoIdentityUnknown` arm
    /// on line 59 of this file (the `None => Err(...)` fallthrough when
    /// owner is missing AND if_repo is false). Without this test, that
    /// arm is reachable only through the per-command integration suites
    /// that already covered it pre-u208; a future refactor of
    /// `resolve_full_or_skip` that simplified or restructured its match
    /// could silently regress today's behavior (which is: name-only
    /// `RepoIdentity` without `--if-repo` exits with `RepoIdentityUnknown`
    /// just as it did before this helper existed).
    #[test]
    fn resolve_full_or_skip_returns_err_without_if_repo_when_owner_missing() {
        let dir = tempfile::tempdir().unwrap();
        let git_dir = dir.path().join(".git");
        fs::create_dir_all(&git_dir).unwrap();
        fs::write(
            git_dir.join("config"),
            "[remote \"origin\"]\n\turl = https://github.com/user/just-name.git\n",
        )
        .unwrap();
        let output = Output::new(false);

        let result = resolve_full_or_skip(None, dir.path(), false, &output);
        assert!(matches!(result, Err(CliError::RepoIdentityUnknown)));
    }

    #[test]
    fn resolve_or_skip_silent_skips_with_if_repo_when_name_flag_provided_identity_no_yaml() {
        let dir = tempfile::tempdir().unwrap();
        // No .syns.yaml, no .git — --name wins.
        let output = Output::new(true);

        let result = resolve_or_skip(Some("alice/foo"), dir.path(), true, &output);
        assert!(matches!(result, Ok(None)));
    }

    #[test]
    fn resolve_or_skip_silent_skips_with_if_repo_when_name_flag_provided_identity_yaml_present() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(
            dir.path().join(".syns.yaml"),
            "owner: bob\nname: yaml-repo\n",
        )
        .unwrap();
        let output = Output::new(true);

        // --name value DIFFERS from the yaml's identity — the test would behave
        // observably differently under a yaml-first vs. name-first design.
        // Locks SPEC D6: source=NameFlag wins precedence even when .syns.yaml
        // is also reachable, so --if-repo silent-skips.
        let result = resolve_or_skip(Some("alice/foo"), dir.path(), true, &output);
        assert!(matches!(result, Ok(None)));
    }

    #[test]
    fn resolve_or_skip_silent_skips_with_if_repo_when_git_remote_provided_identity() {
        let dir = tempfile::tempdir().unwrap();
        let git_dir = dir.path().join(".git");
        fs::create_dir_all(&git_dir).unwrap();
        fs::write(
            git_dir.join("config"),
            "[remote \"origin\"]\n\turl = https://github.com/user/non-syns-project.git\n",
        )
        .unwrap();
        // No .syns.yaml.
        let output = Output::new(true);

        // Locks the 2026-05-25 bug fix at the helper layer: source=GitRemote
        // under --if-repo silent-skips.
        let result = resolve_or_skip(None, dir.path(), true, &output);
        assert!(matches!(result, Ok(None)));
    }

    #[test]
    fn resolve_or_skip_returns_some_with_if_repo_when_syns_yaml_provided_identity() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(
            dir.path().join(".syns.yaml"),
            "owner: alice\nname: my-repo\n",
        )
        .unwrap();
        let output = Output::new(false);

        // Backstop: the gate IS opened by .syns.yaml even under --if-repo.
        let result = resolve_or_skip(None, dir.path(), true, &output);
        assert!(matches!(
            result,
            Ok(Some(RepoIdentity { owner: Some(ref o), ref name })) if o == "alice" && name == "my-repo"
        ));
    }

    #[test]
    fn resolve_or_skip_returns_some_without_if_repo_when_name_flag_provided_identity() {
        let dir = tempfile::tempdir().unwrap();
        let output = Output::new(false);

        // Regression backstop: without --if-repo, source=NameFlag passes through.
        let result = resolve_or_skip(Some("alice/foo"), dir.path(), false, &output);
        assert!(matches!(
            result,
            Ok(Some(RepoIdentity { owner: Some(ref o), ref name })) if o == "alice" && name == "foo"
        ));
    }

    #[test]
    fn resolve_or_skip_returns_some_without_if_repo_when_git_remote_provided_identity() {
        let dir = tempfile::tempdir().unwrap();
        let git_dir = dir.path().join(".git");
        fs::create_dir_all(&git_dir).unwrap();
        fs::write(
            git_dir.join("config"),
            "[remote \"origin\"]\n\turl = https://github.com/user/remote-repo.git\n",
        )
        .unwrap();
        let output = Output::new(false);

        // Regression backstop: without --if-repo, source=GitRemote passes through.
        let result = resolve_or_skip(None, dir.path(), false, &output);
        assert!(matches!(
            result,
            Ok(Some(RepoIdentity { owner: None, ref name })) if name == "remote-repo"
        ));
    }

    #[test]
    fn resolve_or_skip_returns_some_with_if_repo_when_both_yaml_and_git_remote_present() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(
            dir.path().join(".syns.yaml"),
            "owner: alice\nname: yaml-repo\n",
        )
        .unwrap();
        let git_dir = dir.path().join(".git");
        fs::create_dir_all(&git_dir).unwrap();
        fs::write(
            git_dir.join("config"),
            "[remote \"origin\"]\n\turl = https://github.com/user/remote-repo.git\n",
        )
        .unwrap();
        let output = Output::new(false);

        // Priority-chain invariant: .syns.yaml > git-remote even under --if-repo.
        // Backstops future refactors that might reorder the resolver's chain.
        let result = resolve_or_skip(None, dir.path(), true, &output);
        assert!(matches!(
            result,
            Ok(Some(RepoIdentity { owner: Some(ref o), ref name })) if o == "alice" && name == "yaml-repo"
        ));
    }
}
