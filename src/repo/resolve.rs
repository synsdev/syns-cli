use std::path::Path;

use super::remote::extract_from_git_remote;
use super::syns_yaml::read_syns_yaml;
use crate::errors::CliError;

#[derive(Debug, Clone, PartialEq)]
pub struct RepoIdentity {
    pub owner: Option<String>,
    pub name: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IdentitySource {
    NameFlag,
    SynsYaml,
    GitRemote,
}

pub fn resolve_repo_identity(
    name_flag: Option<&str>,
    path: &Path,
) -> Result<(RepoIdentity, IdentitySource), CliError> {
    // 1. If name_flag is present and non-empty after trimming, use it directly.
    if let Some(raw) = name_flag {
        let trimmed = raw.trim();
        if !trimmed.is_empty() {
            if let Some((owner, name)) = trimmed.split_once('/') {
                if owner.is_empty() || name.is_empty() || name.contains('/') {
                    return Err(CliError::Config {
                        message: "invalid repository name: must be 'owner/name' format".into(),
                    });
                }
                return Ok((
                    RepoIdentity {
                        owner: Some(owner.to_lowercase()),
                        name: name.to_lowercase(),
                    },
                    IdentitySource::NameFlag,
                ));
            }
            return Ok((
                RepoIdentity {
                    owner: None,
                    name: trimmed.to_lowercase(),
                },
                IdentitySource::NameFlag,
            ));
        }
    }

    // 2-4. Try .syns.yaml — propagate errors (malformed yaml is a hard error).
    //
    // The pair is ASCII-lower-cased, as a `--name` value is above. A
    // repository address is lower-case on the server, so a marker
    // reading `owner: Alice` / `name: Proj` otherwise addresses a
    // repository the server answers 422 `validation_error` for — while
    // `find_repo_root_for` matches that same marker case-insensitively
    // and resolves the content root correctly, so the run gets as far
    // as the wire before failing.
    if let Some(identity) = read_syns_yaml(path)? {
        return Ok((
            RepoIdentity {
                owner: identity.owner.map(|o| o.to_lowercase()),
                name: identity.name.to_lowercase(),
            },
            IdentitySource::SynsYaml,
        ));
    }

    // 5-7. Fall through to git remote extraction.
    match extract_from_git_remote(path) {
        Some(identity) => Ok((identity, IdentitySource::GitRemote)),
        None => Err(CliError::RepoIdentityUnknown),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::errors::CliError;
    use std::fs;

    #[test]
    fn name_flag_takes_precedence_over_syns_yaml() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(
            dir.path().join(".syns.yaml"),
            "owner: alice\nname: existing-repo\n",
        )
        .unwrap();

        let result = resolve_repo_identity(Some("override-name"), dir.path());
        let (identity, source) = result.unwrap();
        assert_eq!(
            identity,
            RepoIdentity {
                owner: None,
                name: "override-name".into(),
            }
        );
        assert_eq!(source, IdentitySource::NameFlag);
    }

    #[test]
    fn name_flag_is_lowercased() {
        let dir = tempfile::tempdir().unwrap();

        let result = resolve_repo_identity(Some("My-Project"), dir.path());
        assert_eq!(
            result.unwrap().0,
            RepoIdentity {
                owner: None,
                name: "my-project".into(),
            }
        );
    }

    #[test]
    fn whitespace_name_flag_falls_through() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(
            dir.path().join(".syns.yaml"),
            "owner: bob\nname: yaml-repo\n",
        )
        .unwrap();

        let result = resolve_repo_identity(Some("  "), dir.path());
        assert_eq!(
            result.unwrap().0,
            RepoIdentity {
                owner: Some("bob".into()),
                name: "yaml-repo".into(),
            }
        );
    }

    #[test]
    fn syns_yaml_takes_precedence_over_git_remote() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(
            dir.path().join(".syns.yaml"),
            "owner: bob\nname: yaml-repo\n",
        )
        .unwrap();

        let git_dir = dir.path().join(".git");
        fs::create_dir_all(&git_dir).unwrap();
        fs::write(
            git_dir.join("config"),
            "[remote \"origin\"]\n\turl = https://github.com/user/remote-repo.git\n",
        )
        .unwrap();

        let result = resolve_repo_identity(None, dir.path());
        let (identity, source) = result.unwrap();
        assert_eq!(
            identity,
            RepoIdentity {
                owner: Some("bob".into()),
                name: "yaml-repo".into(),
            }
        );
        assert_eq!(source, IdentitySource::SynsYaml);
    }

    #[test]
    fn falls_through_to_git_remote() {
        let dir = tempfile::tempdir().unwrap();

        let git_dir = dir.path().join(".git");
        fs::create_dir_all(&git_dir).unwrap();
        fs::write(
            git_dir.join("config"),
            "[remote \"origin\"]\n\turl = https://github.com/user/remote-repo.git\n",
        )
        .unwrap();

        let result = resolve_repo_identity(None, dir.path());
        let (identity, source) = result.unwrap();
        assert_eq!(
            identity,
            RepoIdentity {
                owner: None,
                name: "remote-repo".into(),
            }
        );
        assert_eq!(source, IdentitySource::GitRemote);
    }

    #[test]
    fn returns_error_when_all_sources_absent() {
        let dir = tempfile::tempdir().unwrap();

        let result = resolve_repo_identity(None, dir.path());
        assert!(matches!(result, Err(CliError::RepoIdentityUnknown)));
    }

    #[test]
    fn malformed_syns_yaml_is_hard_error() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join(".syns.yaml"), "not: valid: yaml: [[").unwrap();

        let result = resolve_repo_identity(None, dir.path());
        assert!(matches!(result, Err(CliError::Io { .. })));
    }

    #[test]
    fn resolve_repo_identity_with_name_flag_reports_source_name_flag() {
        let dir = tempfile::tempdir().unwrap();
        // No .syns.yaml, no .git — only the --name flag should resolve.

        let result = resolve_repo_identity(Some("alice/my-repo"), dir.path());
        let (identity, source) = result.unwrap();
        assert_eq!(
            identity,
            RepoIdentity {
                owner: Some("alice".into()),
                name: "my-repo".into(),
            }
        );
        assert_eq!(source, IdentitySource::NameFlag);
    }

    #[test]
    fn resolve_repo_identity_with_syns_yaml_reports_source_syns_yaml() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(
            dir.path().join(".syns.yaml"),
            "owner: bob\nname: yaml-repo\n",
        )
        .unwrap();

        let result = resolve_repo_identity(None, dir.path());
        let (identity, source) = result.unwrap();
        assert_eq!(
            identity,
            RepoIdentity {
                owner: Some("bob".into()),
                name: "yaml-repo".into(),
            }
        );
        assert_eq!(source, IdentitySource::SynsYaml);
    }

    #[test]
    fn resolve_repo_identity_with_git_remote_reports_source_git_remote() {
        let dir = tempfile::tempdir().unwrap();
        let git_dir = dir.path().join(".git");
        fs::create_dir_all(&git_dir).unwrap();
        fs::write(
            git_dir.join("config"),
            "[remote \"origin\"]\n\turl = https://github.com/user/remote-repo.git\n",
        )
        .unwrap();

        let result = resolve_repo_identity(None, dir.path());
        let (identity, source) = result.unwrap();
        assert_eq!(
            identity,
            RepoIdentity {
                owner: None,
                name: "remote-repo".into(),
            }
        );
        assert_eq!(source, IdentitySource::GitRemote);
    }

    #[test]
    fn syns_yaml_pair_is_lowercased_like_a_name_flag() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join(".syns.yaml"), "owner: Alice\nname: Proj\n").unwrap();

        let (identity, source) = resolve_repo_identity(None, dir.path()).unwrap();

        assert_eq!(identity.owner.as_deref(), Some("alice"));
        assert_eq!(identity.name, "proj");
        assert_eq!(source, IdentitySource::SynsYaml);
    }
}
