#[derive(Debug)]
#[allow(dead_code)] // Variants used by downstream units (U09, U10, etc.)
pub enum CliError {
    Api { status: Option<u16>, error: String },
    AuthRequired,
    RepoIdentityUnknown,
    ServerUnreachable { url: String },
    Io { message: String },
    Config { message: String },
}

impl CliError {
    pub fn exit_code(&self) -> i32 {
        match self {
            CliError::RepoIdentityUnknown => 2,
            CliError::ServerUnreachable { .. } => 3,
            _ => 1,
        }
    }
}

impl std::fmt::Display for CliError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CliError::Api { status: Some(s), error } => write!(f, "server error ({s}): {error}"),
            CliError::Api { status: None, error } => write!(f, "network error: {error}"),
            CliError::AuthRequired => write!(f, "authentication required \u{2014} run 'syns login' first"),
            CliError::RepoIdentityUnknown => write!(f, "cannot determine repo identity \u{2014} provide --name or create .syns.yaml"),
            CliError::ServerUnreachable { url } => write!(f, "could not reach server at {url}"),
            CliError::Io { message } => write!(f, "{message}"),
            CliError::Config { message } => write!(f, "configuration error: {message}"),
        }
    }
}
