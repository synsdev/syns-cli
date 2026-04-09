#![allow(dead_code)] // Functions used by downstream command units (U09, U10+)

use crate::errors::CliError;
use std::path::{Path, PathBuf};
use url::Url;

const DEFAULT_SERVER_URL: &str = "https://syns.dev";
const CONFIG_SUBDIR: &str = "syns";
const CACHE_SUBDIR: &str = "syns";

#[derive(Clone, Debug)]
pub struct Config {
    server_url: String,
    config_dir: PathBuf,
    cache_dir: PathBuf,
}

impl Config {
    pub fn new(server_flag: Option<&str>) -> Result<Config, CliError> {
        // Server URL resolution: flag value > default
        // Note: clap resolves SYNS_URL env var before passing to us via server_flag,
        // so we only need to handle Some(non-empty) vs fallback to default.
        let raw_url = match server_flag {
            Some(s) if !s.is_empty() => s.to_string(),
            _ => DEFAULT_SERVER_URL.to_string(),
        };

        let server_url = raw_url.trim_end_matches('/').to_string();

        // Server URL scheme validation
        let parsed = Url::parse(&server_url).map_err(|_| CliError::Config {
            message: format!("invalid server URL: {server_url}"),
        })?;

        match parsed.scheme() {
            "https" => {}
            "http" if is_localhost_url(&server_url) => {}
            _ => {
                return Err(CliError::Config {
                    message: "server URL must use HTTPS (except http://localhost for local development)".into(),
                });
            }
        }

        // Config directory resolution
        let config_dir = match std::env::var("SYNS_CONFIG_DIR") {
            Ok(val) if !val.is_empty() => PathBuf::from(val),
            _ => dirs::config_dir()
                .ok_or_else(|| CliError::Config {
                    message: "could not determine config directory".into(),
                })?
                .join(CONFIG_SUBDIR),
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

pub fn is_localhost_url(url: &str) -> bool {
    match Url::parse(url) {
        Ok(parsed) => matches!(parsed.host_str(), Some("localhost") | Some("127.0.0.1")),
        Err(_) => false,
    }
}

pub fn is_safe_to_open(verification_url: &str, server_url: &str) -> bool {
    let Ok(verification) = Url::parse(verification_url) else {
        return false;
    };
    let Ok(server) = Url::parse(server_url) else {
        return false;
    };

    // Check safe scheme
    let safe_scheme = match verification.scheme() {
        "https" => true,
        "http" => is_localhost_url(verification_url),
        _ => false,
    };

    if !safe_scheme {
        return false;
    }

    // Check same origin: scheme, host, port
    verification.scheme() == server.scheme()
        && verification.host_str() == server.host_str()
        && verification.port() == server.port()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serial_test::serial;

    #[test]
    fn config_server_url_from_flag() {
        let config = Config::new(Some("https://flag.example.com")).unwrap();
        assert_eq!(config.server_url(), "https://flag.example.com");
    }

    #[test]
    fn config_default_server_url() {
        let config = Config::new(None).unwrap();
        assert_eq!(config.server_url(), "https://syns.dev");
    }

    #[test]
    fn config_strips_trailing_slash() {
        let config = Config::new(Some("https://example.com/")).unwrap();
        assert_eq!(config.server_url(), "https://example.com");
    }

    #[test]
    fn config_rejects_http() {
        let result = Config::new(Some("http://example.com"));
        assert!(result.is_err());
    }

    #[test]
    fn config_allows_localhost_http() {
        let config = Config::new(Some("http://localhost:3000")).unwrap();
        assert_eq!(config.server_url(), "http://localhost:3000");
    }

    #[test]
    fn config_allows_127_http() {
        let config = Config::new(Some("http://127.0.0.1:8080")).unwrap();
        assert_eq!(config.server_url(), "http://127.0.0.1:8080");
    }

    #[test]
    fn is_localhost_url_cases() {
        assert!(is_localhost_url("http://localhost"));
        assert!(is_localhost_url("http://localhost:3000"));
        assert!(is_localhost_url("http://localhost:3000/path"));
        assert!(is_localhost_url("http://127.0.0.1"));
        assert!(is_localhost_url("http://127.0.0.1:8080"));
        assert!(!is_localhost_url("http://localhost.evil.com"));
        assert!(!is_localhost_url("http://example.com"));
        assert!(!is_localhost_url("not a url"));
    }

    #[test]
    fn is_safe_to_open_cases() {
        // Same origin, HTTPS
        assert!(is_safe_to_open("https://syns.dev/auth/verify", "https://syns.dev"));
        // Same origin, localhost HTTP
        assert!(is_safe_to_open("http://localhost:3000/auth/verify", "http://localhost:3000"));
        // Different host
        assert!(!is_safe_to_open("https://evil.com/phish", "https://syns.dev"));
        // Different scheme
        assert!(!is_safe_to_open("http://syns.dev/auth", "https://syns.dev"));
        // javascript: scheme
        assert!(!is_safe_to_open("javascript:alert(1)", "https://syns.dev"));
        // data: scheme
        assert!(!is_safe_to_open("data:text/html,<h1>phish</h1>", "https://syns.dev"));
    }

    #[test]
    fn config_credentials_path() {
        let config = Config::new(Some("https://syns.dev")).unwrap();
        assert!(config.credentials_path().ends_with("credentials.json"));
    }

    #[test]
    #[serial]
    fn config_uses_syns_config_dir_env() {
        // SAFETY: This test runs serially (via #[serial]) so no other thread
        // is reading/writing env vars concurrently.
        unsafe { std::env::set_var("SYNS_CONFIG_DIR", "/tmp/syns-test-config") };
        let config = Config::new(Some("https://syns.dev")).unwrap();
        unsafe { std::env::remove_var("SYNS_CONFIG_DIR") };
        assert_eq!(config.credentials_path(), PathBuf::from("/tmp/syns-test-config/credentials.json"));
    }
}
