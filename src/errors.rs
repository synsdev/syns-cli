use std::fmt;

#[derive(Debug)]
pub enum CliError {
    Api { status: Option<u16>, error: String },
    AuthRequired,
    NotInGitRepo,
    ServerUnreachable { url: String },
    Io { message: String },
    Config { message: String },
}

impl CliError {
    pub fn exit_code(&self) -> i32 {
        match self {
            CliError::NotInGitRepo => 2,
            CliError::ServerUnreachable { .. } => 3,
            _ => 1,
        }
    }
}

impl fmt::Display for CliError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            CliError::Api { status: None, error } => write!(f, "network error: {error}"),
            CliError::Api { status: Some(s), error } => write!(f, "server error ({s}): {error}"),
            CliError::AuthRequired => {
                write!(f, "authentication required \u{2014} run 'syns login' first")
            }
            CliError::NotInGitRepo => write!(f, "not in a git repository"),
            CliError::ServerUnreachable { url } => {
                write!(f, "could not reach server at {url}")
            }
            CliError::Io { message } => write!(f, "{message}"),
            CliError::Config { message } => write!(f, "configuration error: {message}"),
        }
    }
}

impl std::error::Error for CliError {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exit_code_not_in_git_repo() {
        assert_eq!(CliError::NotInGitRepo.exit_code(), 2);
    }

    #[test]
    fn exit_code_server_unreachable() {
        assert_eq!(
            CliError::ServerUnreachable {
                url: "https://example.com".to_string()
            }
            .exit_code(),
            3
        );
    }

    #[test]
    fn exit_code_api_error() {
        assert_eq!(
            CliError::Api {
                status: Some(500),
                error: "internal".to_string()
            }
            .exit_code(),
            1
        );
    }

    #[test]
    fn exit_code_auth_required() {
        assert_eq!(CliError::AuthRequired.exit_code(), 1);
    }

    #[test]
    fn exit_code_config_error() {
        assert_eq!(
            CliError::Config {
                message: "bad".to_string()
            }
            .exit_code(),
            1
        );
    }

    // ── Display tests ──────────────────────────────────────────────────

    #[test]
    fn display_api_error_with_status() {
        let err = CliError::Api {
            status: Some(404),
            error: "not found".to_string(),
        };
        assert_eq!(err.to_string(), "server error (404): not found");
    }

    #[test]
    fn display_api_error_without_status() {
        let err = CliError::Api {
            status: None,
            error: "parse failure".to_string(),
        };
        assert_eq!(err.to_string(), "network error: parse failure");
    }

    #[test]
    fn display_auth_required() {
        assert_eq!(
            CliError::AuthRequired.to_string(),
            "authentication required \u{2014} run 'syns login' first"
        );
    }

    #[test]
    fn display_server_unreachable() {
        let err = CliError::ServerUnreachable {
            url: "https://example.com".to_string(),
        };
        assert_eq!(err.to_string(), "could not reach server at https://example.com");
    }

    #[test]
    fn display_io_error() {
        let err = CliError::Io {
            message: "disk full".to_string(),
        };
        assert_eq!(err.to_string(), "disk full");
    }

    #[test]
    fn display_config_error() {
        let err = CliError::Config {
            message: "bad value".to_string(),
        };
        assert_eq!(err.to_string(), "configuration error: bad value");
    }
}

impl From<reqwest::Error> for CliError {
    fn from(error: reqwest::Error) -> Self {
        if error.is_connect() || error.is_timeout() {
            CliError::ServerUnreachable {
                url: error
                    .url()
                    .map(|u| u.to_string())
                    .unwrap_or_default(),
            }
        } else if error.is_redirect() {
            CliError::Api {
                status: error.status().map(|s| s.as_u16()),
                error: format!("unexpected redirect: {error}"),
            }
        } else {
            CliError::Api {
                status: error.status().map(|s| s.as_u16()),
                error: error.to_string(),
            }
        }
    }
}
