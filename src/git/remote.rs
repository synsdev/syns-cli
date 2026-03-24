use std::path::Path;
use std::process::Command;

use crate::errors::CliError;

/// Normalise any Git remote URL into a canonical `https://` form.
///
/// The function is infallible and always returns a `String`.
pub fn normalize_remote(url: &str) -> String {
    let mut url = url.trim().to_string();
    if url.is_empty() {
        return String::new();
    }

    // --- 1. SSH shorthand: git@host:path --------------------------------
    // Detected when there is `@` AND `:` after the `@` AND no `://` before
    // the colon.
    if let Some(at_pos) = url.find('@') {
        let after_at = &url[at_pos + 1..];
        if let Some(colon_pos) = after_at.find(':') {
            let before_colon = &url[..at_pos + 1 + colon_pos];
            if !before_colon.contains("://") {
                let host = &after_at[..colon_pos];
                let rest = &after_at[colon_pos + 1..];
                // Check if text between `:` and first `/` is all digits
                // (port number). If so, treat as host:port/path, not SSH
                // shorthand.
                let is_port = match rest.find('/') {
                    Some(pos) if pos > 0 => rest[..pos].bytes().all(|b| b.is_ascii_digit()),
                    None if !rest.is_empty() => rest.bytes().all(|b| b.is_ascii_digit()),
                    _ => false,
                };
                if is_port {
                    url = format!("https://{host}:{rest}");
                } else {
                    // Strip leading `/` from path to avoid double slash.
                    let path = rest.trim_start_matches('/');
                    url = format!("https://{host}/{path}");
                }
            }
        }
    }

    // --- 2. Scheme normalisation ----------------------------------------
    if let Some(rest) = url.strip_prefix("ssh://") {
        url = format!("https://{rest}");
    } else if let Some(rest) = url.strip_prefix("git://") {
        url = format!("https://{rest}");
    } else if let Some(rest) = url.strip_prefix("http://") {
        url = format!("https://{rest}");
    } else if !url.starts_with("https://") {
        // Unknown scheme — only lowercase the host portion, not the path.
        if let Some(scheme_end) = url.find("://") {
            let scheme = &url[..scheme_end + 3];
            let after_scheme = &url[scheme_end + 3..];
            return match after_scheme.find('/') {
                Some(pos) => {
                    format!(
                        "{}{}{}",
                        scheme,
                        after_scheme[..pos].to_lowercase(),
                        &after_scheme[pos..]
                    )
                }
                None => format!("{}{}", scheme, after_scheme.to_lowercase()),
            };
        }
        return url.to_lowercase();
    }

    // --- 3. Strip user info (e.g. https://user@host/…) ------------------
    if let Some(rest) = url.strip_prefix("https://") {
        if let Some(at_pos) = rest.find('@') {
            // Only strip when `@` sits before the first `/` (i.e. it is part
            // of the authority, not the path).
            let slash_pos = rest.find('/').unwrap_or(rest.len());
            if at_pos < slash_pos {
                let after_at = &rest[at_pos + 1..];
                url = format!("https://{after_at}");
            }
        }
    }

    // --- 4. Strip trailing `/` ------------------------------------------
    let trimmed_len = url.trim_end_matches('/').len();
    url.truncate(trimmed_len);

    // Guard: trailing-slash stripping may have eaten into the scheme prefix
    // for degenerate inputs like "https://".
    if !url.starts_with("https://") {
        return "https://".to_string();
    }

    // --- 5. Strip `.git` suffix -----------------------------------------
    if let Some(without_git) = url.strip_suffix(".git") {
        url = without_git.to_string();
    }

    // --- 6. Lowercase host only -----------------------------------------
    if let Some(rest) = url.strip_prefix("https://") {
        match rest.find('/') {
            Some(pos) => {
                let host = rest[..pos].to_lowercase();
                let path = &rest[pos..]; // includes leading `/`
                url = format!("https://{host}{path}");
            }
            None => {
                url = format!("https://{}", rest.to_lowercase());
            }
        }
    }

    // --- 7. Strip trailing `/` (may reappear after transformations) ------
    let trimmed_len = url.trim_end_matches('/').len();
    url.truncate(trimmed_len);

    url
}

/// Run `git remote get-url origin` inside `dir` and return the raw URL.
pub fn parse_git_remote(dir: &Path) -> Result<String, CliError> {
    let output = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(["remote", "get-url", "origin"])
        .output()
        .map_err(|e| CliError::Io {
            message: format!("failed to run git: {e}"),
        })?;

    if output.status.success() {
        let stdout = String::from_utf8_lossy(&output.stdout);
        return Ok(stdout.trim().to_string());
    }

    let stderr = String::from_utf8_lossy(&output.stderr);
    let lower = stderr.to_lowercase();

    if lower.contains("not a git repository") {
        Err(CliError::NotInGitRepo)
    } else if lower.contains("no such remote") {
        Err(CliError::Config {
            message: "no 'origin' remote configured".to_string(),
        })
    } else {
        Err(CliError::Io {
            message: format!("git remote failed: {}", stderr.trim()),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── normalize_remote ────────────────────────────────────────────────

    #[test]
    fn normalize_strips_git_suffix() {
        assert_eq!(
            normalize_remote("https://github.com/user/repo.git"),
            "https://github.com/user/repo"
        );
    }

    #[test]
    fn normalize_ssh_to_https() {
        assert_eq!(
            normalize_remote("git@github.com:user/repo"),
            "https://github.com/user/repo"
        );
    }

    #[test]
    fn normalize_lowercases_host() {
        assert_eq!(
            normalize_remote("https://GitHub.COM/User/Repo"),
            "https://github.com/User/Repo"
        );
    }

    #[test]
    fn normalize_ssh_with_git_suffix_and_uppercase_host() {
        assert_eq!(
            normalize_remote("git@GitHub.com:User/Repo.git"),
            "https://github.com/User/Repo"
        );
    }

    #[test]
    fn normalize_strips_trailing_slash() {
        assert_eq!(
            normalize_remote("https://GitHub.COM/User/Repo.git/"),
            "https://github.com/User/Repo"
        );
    }

    #[test]
    fn normalize_ssh_scheme() {
        assert_eq!(
            normalize_remote("ssh://git@github.com/User/Repo"),
            "https://github.com/User/Repo"
        );
    }

    #[test]
    fn normalize_git_scheme() {
        assert_eq!(
            normalize_remote("git://github.com/User/Repo.git"),
            "https://github.com/User/Repo"
        );
    }

    #[test]
    fn normalize_http_scheme() {
        assert_eq!(
            normalize_remote("http://github.com/User/Repo"),
            "https://github.com/User/Repo"
        );
    }

    #[test]
    fn normalize_already_canonical() {
        assert_eq!(
            normalize_remote("https://github.com/User/Repo"),
            "https://github.com/User/Repo"
        );
    }

    #[test]
    fn normalize_empty_string() {
        assert_eq!(normalize_remote(""), "");
    }

    #[test]
    fn normalize_url_with_port() {
        assert_eq!(
            normalize_remote("https://git.example.com:8443/User/Repo.git"),
            "https://git.example.com:8443/User/Repo"
        );
    }

    #[test]
    fn normalize_strips_user_info() {
        assert_eq!(
            normalize_remote("https://user@github.com/foo/bar"),
            "https://github.com/foo/bar"
        );
    }

    #[test]
    fn normalize_degenerate_https_only() {
        // CRITICAL 1: should not panic on scheme-only input
        let result = normalize_remote("https://");
        assert_eq!(result, "https://");
    }

    #[test]
    fn normalize_degenerate_https_trailing_slashes() {
        let result = normalize_remote("https:///");
        assert_eq!(result, "https://");
    }

    #[test]
    fn normalize_ssh_shorthand_with_port() {
        // HIGH 1: port number should not be treated as path component
        assert_eq!(
            normalize_remote("git@gitlab.internal:2222/team/project"),
            "https://gitlab.internal:2222/team/project"
        );
    }

    #[test]
    fn normalize_ssh_shorthand_port_only_no_path() {
        // MEDIUM 7: git@host:2222 with no path should treat 2222 as port
        assert_eq!(
            normalize_remote("git@gitlab.internal:2222"),
            "https://gitlab.internal:2222"
        );
    }

    #[test]
    fn normalize_ssh_shorthand_leading_slash() {
        // HIGH 2: leading slash should not produce double slash
        assert_eq!(
            normalize_remote("git@github.com:/user/repo"),
            "https://github.com/user/repo"
        );
    }

    #[test]
    fn normalize_unknown_scheme_preserves_path_case() {
        // MEDIUM 4: unknown scheme should only lowercase host
        assert_eq!(
            normalize_remote("ftp://GitHub.COM/User/Repo"),
            "ftp://github.com/User/Repo"
        );
    }

    // ── parse_git_remote ────────────────────────────────────────────────

    use std::process::Command as StdCommand;
    use tempfile::TempDir;

    #[test]
    fn parse_git_remote_not_a_repo() {
        let tmp = TempDir::new().unwrap();
        let result = parse_git_remote(tmp.path());
        assert!(result.is_err());
        match result.unwrap_err() {
            CliError::NotInGitRepo => {}
            other => panic!("expected NotInGitRepo, got: {other:?}"),
        }
    }

    #[test]
    fn parse_git_remote_no_origin() {
        let tmp = TempDir::new().unwrap();
        StdCommand::new("git")
            .args(["init", tmp.path().to_str().unwrap()])
            .output()
            .unwrap();
        let result = parse_git_remote(tmp.path());
        assert!(result.is_err());
        match result.unwrap_err() {
            CliError::Config { message } => {
                assert!(message.contains("origin"), "message was: {message}")
            }
            other => panic!("expected Config error about origin, got: {other:?}"),
        }
    }

    #[test]
    fn parse_git_remote_success() {
        let tmp = TempDir::new().unwrap();
        StdCommand::new("git")
            .args(["init", tmp.path().to_str().unwrap()])
            .output()
            .unwrap();
        StdCommand::new("git")
            .args([
                "-C",
                tmp.path().to_str().unwrap(),
                "remote",
                "add",
                "origin",
                "https://github.com/user/repo.git",
            ])
            .output()
            .unwrap();
        let result = parse_git_remote(tmp.path()).unwrap();
        assert_eq!(result, "https://github.com/user/repo.git");
    }
}
