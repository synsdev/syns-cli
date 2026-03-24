use std::fmt;

#[derive(Debug)]
pub enum CliError {
    Api { status: u16, error: String },
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
            CliError::Api { status: 0, error } => write!(f, "network error: {error}"),
            CliError::Api { status, error } => write!(f, "server error ({status}): {error}"),
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
                status: 500,
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
        } else {
            CliError::Api {
                status: error.status().map(|s| s.as_u16()).unwrap_or(0),
                error: error.to_string(),
            }
        }
    }
}
