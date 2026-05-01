use std::path::{Path, PathBuf};

use super::resolve::RepoIdentity;

const REPONAME_MAX_LENGTH: usize = 100;

pub fn extract_from_git_remote(path: &Path) -> Option<RepoIdentity> {
    let git_dir = find_git_dir(path)?;
    let config_content = std::fs::read_to_string(git_dir.join("config")).ok()?;
    let url = parse_origin_url(&config_content)?;
    let name = extract_name_from_url(&url)?;
    Some(RepoIdentity { owner: None, name })
}

fn find_git_dir(path: &Path) -> Option<PathBuf> {
    let mut current = Some(path);
    while let Some(dir) = current {
        let candidate = dir.join(".git");
        if candidate.is_dir() {
            return Some(candidate);
        }
        current = dir.parent();
    }
    None
}

fn parse_origin_url(config_content: &str) -> Option<String> {
    let mut in_origin_section = false;
    for line in config_content.lines() {
        let trimmed = line.trim();
        if trimmed == "[remote \"origin\"]" {
            in_origin_section = true;
            continue;
        }
        if in_origin_section {
            if trimmed.starts_with('[') {
                return None;
            }
            if let Some((key, value)) = trimmed.split_once('=') {
                if key.trim() == "url" {
                    return Some(value.trim().to_string());
                }
            }
        }
    }
    None
}

fn extract_name_from_url(url: &str) -> Option<String> {
    let normalized = if url.contains('@') && !url.starts_with("ssh://") {
        if let Some(at_pos) = url.find('@') {
            let after_at = &url[at_pos + 1..];
            if let Some(colon_pos) = after_at.find(':') {
                let mut result = String::with_capacity(url.len());
                result.push_str(&url[..at_pos + 1 + colon_pos]);
                result.push('/');
                result.push_str(&after_at[colon_pos + 1..]);
                result
            } else {
                url.to_string()
            }
        } else {
            url.to_string()
        }
    } else {
        url.to_string()
    };

    let segment = normalized.split('/').rev().find(|s| !s.is_empty())?;

    let name = segment.strip_suffix(".git").unwrap_or(segment);
    let name = name.to_lowercase();

    if !is_valid_reponame(&name) {
        return None;
    }

    Some(name)
}

fn is_valid_reponame(name: &str) -> bool {
    let len = name.len();
    if len == 0 || len > REPONAME_MAX_LENGTH {
        return false;
    }
    if !name
        .chars()
        .all(|c| matches!(c, 'a'..='z' | '0'..='9' | '.' | '_' | '-'))
    {
        return false;
    }
    let first = name.as_bytes()[0];
    if !(first.is_ascii_lowercase() || first.is_ascii_digit()) {
        return false;
    }
    let last = name.as_bytes()[len - 1];
    if last == b'.' || last == b'-' {
        return false;
    }
    if name.contains("..") {
        return false;
    }
    if name.contains("--") {
        return false;
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    fn setup_git_config(dir: &Path, config_content: &str) {
        let git_dir = dir.join(".git");
        fs::create_dir_all(&git_dir).unwrap();
        fs::write(git_dir.join("config"), config_content).unwrap();
    }

    #[test]
    fn https_url_extracts_name() {
        let tmp = TempDir::new().unwrap();
        setup_git_config(
            tmp.path(),
            "[remote \"origin\"]\n\turl = https://github.com/user/my-project.git\n",
        );
        assert_eq!(
            extract_from_git_remote(tmp.path()),
            Some(RepoIdentity {
                owner: None,
                name: "my-project".into()
            })
        );
    }

    #[test]
    fn ssh_url_extracts_name() {
        let tmp = TempDir::new().unwrap();
        setup_git_config(
            tmp.path(),
            "[remote \"origin\"]\n\turl = git@github.com:user/ssh-project.git\n",
        );
        assert_eq!(
            extract_from_git_remote(tmp.path()),
            Some(RepoIdentity {
                owner: None,
                name: "ssh-project".into()
            })
        );
    }

    #[test]
    fn https_url_without_git_suffix() {
        let tmp = TempDir::new().unwrap();
        setup_git_config(
            tmp.path(),
            "[remote \"origin\"]\n\turl = https://github.com/user/my-project\n",
        );
        assert_eq!(
            extract_from_git_remote(tmp.path()),
            Some(RepoIdentity {
                owner: None,
                name: "my-project".into()
            })
        );
    }

    #[test]
    fn url_with_trailing_slash() {
        let tmp = TempDir::new().unwrap();
        setup_git_config(
            tmp.path(),
            "[remote \"origin\"]\n\turl = https://github.com/user/my-project/\n",
        );
        assert_eq!(
            extract_from_git_remote(tmp.path()),
            Some(RepoIdentity {
                owner: None,
                name: "my-project".into()
            })
        );
    }

    #[test]
    fn uppercase_name_lowercased() {
        let tmp = TempDir::new().unwrap();
        setup_git_config(
            tmp.path(),
            "[remote \"origin\"]\n\turl = https://github.com/user/My-Project.git\n",
        );
        assert_eq!(
            extract_from_git_remote(tmp.path()),
            Some(RepoIdentity {
                owner: None,
                name: "my-project".into()
            })
        );
    }

    #[test]
    fn no_git_dir_returns_none() {
        let tmp = TempDir::new().unwrap();
        assert_eq!(extract_from_git_remote(tmp.path()), None);
    }

    #[test]
    fn no_origin_remote_returns_none() {
        let tmp = TempDir::new().unwrap();
        setup_git_config(
            tmp.path(),
            "[remote \"upstream\"]\n\turl = https://github.com/user/repo.git\n",
        );
        assert_eq!(extract_from_git_remote(tmp.path()), None);
    }

    #[test]
    fn invalid_name_returns_none() {
        let tmp = TempDir::new().unwrap();
        setup_git_config(
            tmp.path(),
            "[remote \"origin\"]\n\turl = https://github.com/user/..git\n",
        );
        assert_eq!(extract_from_git_remote(tmp.path()), None);
    }

    #[test]
    fn git_dir_found_in_parent() {
        let tmp = TempDir::new().unwrap();
        setup_git_config(
            tmp.path(),
            "[remote \"origin\"]\n\turl = https://github.com/user/parent-repo.git\n",
        );
        let sub = tmp.path().join("sub");
        fs::create_dir_all(&sub).unwrap();
        assert_eq!(
            extract_from_git_remote(&sub),
            Some(RepoIdentity {
                owner: None,
                name: "parent-repo".into()
            })
        );
    }

    #[test]
    fn is_valid_reponame_rules() {
        // Valid names
        assert!(is_valid_reponame("my-project"));
        assert!(is_valid_reponame("repo.v2"));
        assert!(is_valid_reponame("a"));
        assert!(is_valid_reponame("a-b.c_d"));

        // Invalid names
        assert!(!is_valid_reponame(""));
        assert!(!is_valid_reponame(".foo"));
        assert!(!is_valid_reponame("-foo"));
        assert!(!is_valid_reponame("foo."));
        assert!(!is_valid_reponame("foo-"));
        assert!(!is_valid_reponame("fo..o"));
        assert!(!is_valid_reponame("fo--o"));
        assert!(!is_valid_reponame("foo bar"));
        assert!(!is_valid_reponame(&"a".repeat(101)));
    }
}
