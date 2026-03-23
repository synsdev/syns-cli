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
