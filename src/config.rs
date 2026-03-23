use std::env;
use std::path::{Path, PathBuf};

use crate::errors::CliError;

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
        // Server URL resolution: flag > env > default
        let server_url = match server_flag {
            Some(url) if !url.is_empty() => url.to_string(),
            _ => match env::var("SYNS_URL") {
                Ok(val) if !val.is_empty() => val,
                _ => DEFAULT_SERVER_URL.to_string(),
            },
        };
        let server_url = server_url.trim_end_matches('/').to_string();

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
        unsafe { env::set_var("SYNS_URL", "http://env.example.com") };
        let config = Config::new(Some("http://flag.example.com")).unwrap();
        assert_eq!(config.server_url(), "http://flag.example.com");
        unsafe { env::remove_var("SYNS_URL") };
    }

    #[test]
    #[serial]
    fn config_env_wins_over_default() {
        unsafe { env::set_var("SYNS_URL", "http://env.example.com") };
        let config = Config::new(None).unwrap();
        assert_eq!(config.server_url(), "http://env.example.com");
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
        let config = Config::new(Some("http://example.com/")).unwrap();
        assert_eq!(config.server_url(), "http://example.com");
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
}
