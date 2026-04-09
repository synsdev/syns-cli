#![allow(dead_code)] // Used by downstream command units (U22)

use std::path::PathBuf;

use crate::errors::CliError;

#[cfg(unix)]
const CREDENTIALS_FILE_MODE: u32 = 0o600;

#[derive(serde::Serialize, serde::Deserialize)]
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
            Ok(c) => c,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(err) => {
                return Err(CliError::Io {
                    message: format!("could not read credentials: {err}"),
                });
            }
        };

        let credentials: Credentials = serde_json::from_str(&contents).map_err(|err| {
            CliError::Config {
                message: format!("invalid credentials file: {err}"),
            }
        })?;

        Ok(Some(credentials.token))
    }

    pub fn write(&self, token: &str) -> Result<(), CliError> {
        if token.trim().is_empty() {
            return Err(CliError::Config {
                message: "token must not be empty".into(),
            });
        }

        if let Some(parent) = self.credentials_path.parent() {
            std::fs::create_dir_all(parent).map_err(|err| CliError::Io {
                message: format!("could not create config directory: {err}"),
            })?;
        }

        let json = serde_json::to_string_pretty(&Credentials {
            token: token.to_string(),
        })
        .expect("Credentials serialization should never fail");

        #[cfg(unix)]
        {
            use std::io::Write;
            use std::os::unix::fs::OpenOptionsExt;

            let mut file = std::fs::OpenOptions::new()
                .write(true)
                .create(true)
                .truncate(true)
                .mode(CREDENTIALS_FILE_MODE)
                .open(&self.credentials_path)
                .map_err(|err| CliError::Io {
                    message: format!("could not write credentials: {err}"),
                })?;

            file.write_all(json.as_bytes()).map_err(|err| CliError::Io {
                message: format!("could not write credentials: {err}"),
            })?;
        }

        #[cfg(not(unix))]
        {
            std::fs::write(&self.credentials_path, json).map_err(|err| CliError::Io {
                message: format!("could not write credentials: {err}"),
            })?;
        }

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
    fn write_creates_file_with_correct_content_and_permissions() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("credentials.json");
        let store = TokenStore::new(path.clone());

        store.write("test-token-abc").unwrap();

        let raw = std::fs::read_to_string(&path).unwrap();
        let creds: serde_json::Value = serde_json::from_str(&raw).unwrap();
        assert_eq!(creds["token"], "test-token-abc");

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let metadata = std::fs::metadata(&path).unwrap();
            assert_eq!(metadata.permissions().mode() & 0o777, 0o600);
        }
    }

    #[test]
    fn write_then_read_roundtrips() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("credentials.json");
        let store = TokenStore::new(path);

        store.write("my-bearer-token").unwrap();
        let result = store.read().unwrap();
        assert_eq!(result, Some("my-bearer-token".to_string()));
    }

    #[test]
    fn read_returns_none_when_no_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nonexistent").join("credentials.json");
        let store = TokenStore::new(path);

        let result = store.read().unwrap();
        assert_eq!(result, None);
    }

    #[test]
    fn clear_removes_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("credentials.json");
        let store = TokenStore::new(path.clone());

        store.write("to-be-cleared").unwrap();
        assert!(path.exists());

        store.clear().unwrap();
        assert!(!path.exists());
        assert_eq!(store.read().unwrap(), None);
    }

    #[test]
    fn write_rejects_empty_and_whitespace_tokens() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("credentials.json");
        let store = TokenStore::new(path.clone());

        let err_empty = store.write("").unwrap_err();
        assert!(matches!(err_empty, CliError::Config { .. }));

        let err_ws = store.write("   ").unwrap_err();
        assert!(matches!(err_ws, CliError::Config { .. }));

        assert!(!path.exists());
    }

    #[test]
    fn read_returns_error_for_corrupt_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("credentials.json");
        std::fs::write(&path, "not valid json").unwrap();

        let store = TokenStore::new(path);
        let err = store.read().unwrap_err();
        assert!(matches!(err, CliError::Config { .. }));
    }
}
