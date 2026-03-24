use std::env;
use std::path::{Path, PathBuf};

use crate::errors::CliError;

const DEFAULT_SERVER_URL: &str = "https://syns.dev";
const CONFIG_SUBDIR: &str = "syns";
const CACHE_SUBDIR: &str = "syns";

/// Check whether `url` starts with `http://localhost` followed by end-of-string,
/// `/`, or `:` (port). This prevents bypass via e.g. `http://localhost.evil.com`.
pub(crate) fn is_localhost_url(url: &str) -> bool {
    let Some(rest) = url.strip_prefix("http://localhost") else {
        return false;
    };
    if rest.is_empty() || rest.starts_with('/') {
        return true;
    }
    if let Some(after_colon) = rest.strip_prefix(':') {
        // After the colon, everything until next '/' or end must be all digits.
        // This rejects userinfo bypass like http://localhost:80@evil.com
        let port_end = after_colon.find('/').unwrap_or(after_colon.len());
        let port = &after_colon[..port_end];
        return !port.is_empty() && port.bytes().all(|b| b.is_ascii_digit());
    }
    false
}

/// Validate that a URL is safe to open in a browser (must be https:// or http://localhost).
pub(crate) fn is_safe_to_open(url: &str) -> bool {
    url.starts_with("https://") || is_localhost_url(url)
}

#[derive(Clone, Debug)]
pub struct Config {
    server_url: String,
    config_dir: PathBuf,
    cache_dir: PathBuf,
}

impl Config {
    pub fn new(server_flag: Option<&str>) -> Result<Config, CliError> {
        // Server URL resolution: flag > env > default
        let server_url = match server_flag {
            Some(url) if !url.is_empty() => url.to_string(),
            _ => match env::var("SYNS_URL") {
                Ok(val) if !val.is_empty() => val,
                _ => DEFAULT_SERVER_URL.to_string(),
            },
        };
        let server_url = server_url.trim_end_matches('/').to_string();

        if !server_url.starts_with("https://") && !is_localhost_url(&server_url) {
            return Err(CliError::Config {
                message: "server URL must use HTTPS (or http://localhost for development)"
                    .to_string(),
            });
        }

        // Config directory resolution
        let config_dir = match env::var("SYNS_CONFIG_DIR") {
            Ok(val) if !val.is_empty() => PathBuf::from(val),
            _ => match dirs::config_dir() {
                Some(path) => path.join(CONFIG_SUBDIR),
                None => {
                    return Err(CliError::Config {
                        message: "Could not determine config directory".to_string(),
                    });
                }
            },
        };

        // Cache directory resolution
        let cache_dir = match dirs::cache_dir() {
            Some(path) => path.join(CACHE_SUBDIR),
            None => config_dir.join("cache"),
        };

        Ok(Config {
            server_url,
            config_dir,
            cache_dir,
        })
    }

    pub fn server_url(&self) -> &str {
        &self.server_url
    }

    pub fn credentials_path(&self) -> PathBuf {
        self.config_dir.join("credentials.json")
    }

    pub fn cache_dir(&self) -> &Path {
        &self.cache_dir
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serial_test::serial;
    use std::env;

    #[test]
    #[serial]
    fn config_server_flag_wins_over_env() {
        unsafe { env::set_var("SYNS_URL", "https://env.example.com") };
        let config = Config::new(Some("https://flag.example.com")).unwrap();
        assert_eq!(config.server_url(), "https://flag.example.com");
        unsafe { env::remove_var("SYNS_URL") };
    }

    #[test]
    #[serial]
    fn config_env_wins_over_default() {
        unsafe { env::set_var("SYNS_URL", "https://env.example.com") };
        let config = Config::new(None).unwrap();
        assert_eq!(config.server_url(), "https://env.example.com");
        unsafe { env::remove_var("SYNS_URL") };
    }

    #[test]
    #[serial]
    fn config_default_when_nothing_set() {
        unsafe { env::remove_var("SYNS_URL") };
        let config = Config::new(None).unwrap();
        assert_eq!(config.server_url(), "https://syns.dev");
    }

    #[test]
    fn config_trailing_slash_stripped() {
        let config = Config::new(Some("https://example.com/")).unwrap();
        assert_eq!(config.server_url(), "https://example.com");
    }

    #[test]
    #[serial]
    fn config_empty_flag_treated_as_none() {
        unsafe { env::remove_var("SYNS_URL") };
        let config = Config::new(Some("")).unwrap();
        assert_eq!(config.server_url(), "https://syns.dev");
    }

    #[test]
    fn config_credentials_path() {
        let config = Config::new(None).unwrap();
        let cred_path = config.credentials_path();
        assert!(cred_path.ends_with("credentials.json"));
        let parent = cred_path.parent().unwrap();
        assert!(parent.to_string_lossy().contains("syns"));
    }

    #[test]
    fn config_rejects_http_url() {
        let result = Config::new(Some("http://example.com"));
        assert!(result.is_err());
    }

    #[test]
    fn config_allows_localhost_http() {
        let config = Config::new(Some("http://localhost:3000")).unwrap();
        assert_eq!(config.server_url(), "http://localhost:3000");
    }

    #[test]
    fn is_localhost_url_bare() {
        assert!(is_localhost_url("http://localhost"));
    }

    #[test]
    fn is_localhost_url_with_port() {
        assert!(is_localhost_url("http://localhost:3000"));
    }

    #[test]
    fn is_localhost_url_with_path() {
        assert!(is_localhost_url("http://localhost/api"));
    }

    #[test]
    fn is_localhost_url_rejects_evil_subdomain() {
        assert!(!is_localhost_url("http://localhost.evil.com"));
    }

    #[test]
    fn is_localhost_url_rejects_https() {
        assert!(!is_localhost_url("https://localhost"));
    }

    #[test]
    fn is_safe_to_open_allows_https() {
        assert!(is_safe_to_open("https://example.com"));
    }

    #[test]
    fn is_safe_to_open_allows_localhost() {
        assert!(is_safe_to_open("http://localhost:3000"));
    }

    #[test]
    fn is_safe_to_open_rejects_javascript() {
        assert!(!is_safe_to_open("javascript:alert(1)"));
    }

    #[test]
    fn is_safe_to_open_rejects_file() {
        assert!(!is_safe_to_open("file:///etc/passwd"));
    }

    #[test]
    fn is_localhost_url_rejects_userinfo_bypass() {
        assert!(!is_localhost_url("http://localhost:80@evil.com"));
    }

    #[test]
    fn is_localhost_url_rejects_userinfo_bypass_with_path() {
        assert!(!is_localhost_url("http://localhost:80@evil.com/callback"));
    }

    #[test]
    fn is_localhost_url_with_port_and_path() {
        assert!(is_localhost_url("http://localhost:3000/api/callback"));
    }

    #[test]
    fn is_localhost_url_rejects_colon_no_digits() {
        assert!(!is_localhost_url("http://localhost:abc"));
    }

    #[test]
    fn is_localhost_url_rejects_empty_port() {
        assert!(!is_localhost_url("http://localhost:"));
    }

    #[test]
    fn is_safe_to_open_rejects_data_uri() {
        assert!(!is_safe_to_open("data:text/html,<script>alert(1)</script>"));
    }

    #[test]
    fn is_safe_to_open_rejects_plain_http() {
        assert!(!is_safe_to_open("http://evil.com"));
    }

    #[test]
    fn is_safe_to_open_rejects_localhost_userinfo_bypass() {
        assert!(!is_safe_to_open("http://localhost:80@evil.com"));
    }
}
