use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::errors::CliError;

#[cfg(unix)]
use std::os::unix::fs::OpenOptionsExt;

#[derive(Serialize, Deserialize)]
struct Credentials {
    token: String,
}

pub struct TokenStore {
    credentials_path: PathBuf,
}

impl TokenStore {
    pub fn new(credentials_path: PathBuf) -> TokenStore {
        TokenStore { credentials_path }
    }

    pub fn read(&self) -> Result<Option<String>, CliError> {
        let contents = match std::fs::read_to_string(&self.credentials_path) {
            Ok(contents) => contents,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(err) => {
                return Err(CliError::Io {
                    message: format!("could not read credentials: {err}"),
                })
            }
        };

        let credentials: Credentials =
            serde_json::from_str(&contents).map_err(|_| CliError::Config {
                message: "could not parse credentials file".to_string(),
            })?;

        Ok(Some(credentials.token))
    }

    pub fn write(&self, token: &str) -> Result<(), CliError> {
        let token = token.trim();
        if token.is_empty() {
            return Err(CliError::Config {
                message: "token must not be empty".to_string(),
            });
        }

        if let Some(parent) = self.credentials_path.parent() {
            std::fs::create_dir_all(parent).map_err(|err| CliError::Io {
                message: format!("could not create config directory: {err}"),
            })?;
        }

        let credentials = Credentials {
            token: token.to_string(),
        };
        let json = serde_json::to_string(&credentials).map_err(|e| CliError::Config {
            message: format!("could not serialize credentials: {e}"),
        })?;

        // Write to a temp file first, then atomically rename over the target.
        // This prevents partial writes from corrupting the credentials file.
        let tmp_path = self.credentials_path.with_extension("tmp");

        #[cfg(unix)]
        {
            use std::io::Write as _;
            let mut file = std::fs::OpenOptions::new()
                .write(true)
                .create(true)
                .truncate(true)
                .mode(0o600)
                .open(&tmp_path)
                .map_err(|err| CliError::Io {
                    message: format!("could not write credentials: {err}"),
                })?;
            file.write_all(json.as_bytes()).map_err(|err| CliError::Io {
                message: format!("could not write credentials: {err}"),
            })?;
        }

        #[cfg(not(unix))]
        {
            std::fs::write(&tmp_path, &json).map_err(|err| CliError::Io {
                message: format!("could not write credentials: {err}"),
            })?;
        }

        std::fs::rename(&tmp_path, &self.credentials_path).map_err(|err| CliError::Io {
            message: format!("could not write credentials: {err}"),
        })?;

        Ok(())
    }

    pub fn clear(&self) -> Result<(), CliError> {
        match std::fs::remove_file(&self.credentials_path) {
            Ok(()) => Ok(()),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(err) => Err(CliError::Io {
                message: format!("could not remove credentials: {err}"),
            }),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn token_write_creates_file_with_correct_permissions() {
        let dir = tempfile::tempdir().unwrap();
        let store = TokenStore::new(dir.path().join("credentials.json"));

        store.write("test-token-123").unwrap();

        let contents = std::fs::read_to_string(dir.path().join("credentials.json")).unwrap();
        let creds: Credentials = serde_json::from_str(&contents).unwrap();
        assert_eq!(creds.token, "test-token-123");

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let metadata = std::fs::metadata(dir.path().join("credentials.json")).unwrap();
            let mode = metadata.permissions().mode() & 0o777;
            assert_eq!(mode, 0o600);
        }
    }

    #[test]
    fn token_read_returns_stored_token() {
        let dir = tempfile::tempdir().unwrap();
        let store = TokenStore::new(dir.path().join("credentials.json"));

        store.write("my-secret-token").unwrap();
        let result = store.read().unwrap();
        assert_eq!(result, Some("my-secret-token".to_string()));
    }

    #[test]
    fn token_clear_removes_file() {
        let dir = tempfile::tempdir().unwrap();
        let store = TokenStore::new(dir.path().join("credentials.json"));

        store.write("token-to-clear").unwrap();
        assert!(std::fs::metadata(dir.path().join("credentials.json")).is_ok());

        store.clear().unwrap();
        assert!(std::fs::metadata(dir.path().join("credentials.json")).is_err());
    }

    #[test]
    fn token_read_returns_none_when_no_file() {
        let dir = tempfile::tempdir().unwrap();
        let store = TokenStore::new(dir.path().join("credentials.json"));

        let result = store.read().unwrap();
        assert!(result.is_none());
    }

    #[test]
    fn token_write_rejects_empty_token() {
        let dir = tempfile::tempdir().unwrap();
        let store = TokenStore::new(dir.path().join("credentials.json"));

        let result = store.write("");
        assert!(result.is_err());
        assert!(std::fs::metadata(dir.path().join("credentials.json")).is_err());
    }

    #[test]
    fn token_write_rejects_whitespace_only_token() {
        let dir = tempfile::tempdir().unwrap();
        let store = TokenStore::new(dir.path().join("credentials.json"));

        let result = store.write("   ");
        assert!(result.is_err());
        assert!(std::fs::metadata(dir.path().join("credentials.json")).is_err());
    }

    #[test]
    fn token_write_trims_whitespace() {
        let dir = tempfile::tempdir().unwrap();
        let store = TokenStore::new(dir.path().join("credentials.json"));

        store.write("  my-token  ").unwrap();
        let result = store.read().unwrap();
        assert_eq!(result, Some("my-token".to_string()));
    }
}
