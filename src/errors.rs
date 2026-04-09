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

impl std::error::Error for CliError {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exit_codes() {
        assert_eq!(CliError::RepoIdentityUnknown.exit_code(), 2);
        assert_eq!(CliError::ServerUnreachable { url: "x".into() }.exit_code(), 3);
        assert_eq!(CliError::AuthRequired.exit_code(), 1);
        assert_eq!((CliError::Config { message: "x".into() }).exit_code(), 1);
        assert_eq!((CliError::Api { status: Some(500), error: "x".into() }).exit_code(), 1);
        assert_eq!((CliError::Io { message: "x".into() }).exit_code(), 1);
    }

    #[test]
    fn display_messages() {
        let api_with = CliError::Api { status: Some(404), error: "not_found".into() };
        assert_eq!(api_with.to_string(), "server error (404): not_found");

        let api_without = CliError::Api { status: None, error: "timeout".into() };
        assert_eq!(api_without.to_string(), "network error: timeout");

        assert_eq!(
            CliError::AuthRequired.to_string(),
            "authentication required \u{2014} run 'syns login' first"
        );

        assert_eq!(
            CliError::RepoIdentityUnknown.to_string(),
            "cannot determine repo identity \u{2014} provide --name or create .syns.yaml"
        );

        assert_eq!(
            CliError::ServerUnreachable { url: "https://example.com".into() }.to_string(),
            "could not reach server at https://example.com"
        );

        assert_eq!(
            CliError::Io { message: "file not found".into() }.to_string(),
            "file not found"
        );

        assert_eq!(
            CliError::Config { message: "bad url".into() }.to_string(),
            "configuration error: bad url"
        );
    }
}
