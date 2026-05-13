use crate::push::collector::SkippedFile;

#[derive(Debug)]
#[allow(dead_code)] // Variants used by downstream units (U09, U10, etc.)
pub enum CliError {
    Api {
        status: Option<u16>,
        error: String,
        context: Option<ApiErrorContext>,
    },
    AuthRequired,
    RepoIdentityUnknown,
    ServerUnreachable {
        url: String,
    },
    Io {
        message: String,
    },
    Config {
        message: String,
    },
    Upgrade(crate::commands::upgrade::UpgradeError),
    PushEmpty {
        path: String,
        total_walked: usize,
        cause: String,
    },
    PushPartial {
        skipped: Vec<SkippedFile>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ApiErrorContext {
    LsPath { path: String },
    CatPath { path: String },
}

impl CliError {
    pub fn exit_code(&self) -> i32 {
        match self {
            CliError::RepoIdentityUnknown => 2,
            CliError::ServerUnreachable { .. } => 3,
            CliError::PushPartial { .. } => 3,
            CliError::PushEmpty { .. } => 6,
            CliError::Upgrade(e) => e.exit_code(),
            _ => 1,
        }
    }

    pub fn with_ls_path_context(self, path: String) -> CliError {
        match self {
            CliError::Api { status, error, .. } => CliError::Api {
                status,
                error,
                context: Some(ApiErrorContext::LsPath { path }),
            },
            other => other,
        }
    }

    pub fn with_cat_path_context(self, path: String) -> CliError {
        match self {
            CliError::Api { status, error, .. } => CliError::Api {
                status,
                error,
                context: Some(ApiErrorContext::CatPath { path }),
            },
            other => other,
        }
    }
}

impl std::fmt::Display for CliError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CliError::Api {
                status: Some(404),
                error,
                context: Some(ApiErrorContext::LsPath { path }),
            } if error == "not_found" => write!(f, "path not found: {path}"),
            CliError::Api {
                status: Some(404),
                error,
                context: Some(ApiErrorContext::CatPath { path }),
            } if error == "not_found" => write!(f, "file not found: {path}"),
            CliError::Api {
                status: Some(s),
                error,
                ..
            } => write!(f, "server error ({s}): {error}"),
            CliError::Api {
                status: None,
                error,
                ..
            } => write!(f, "network error: {error}"),
            CliError::AuthRequired => {
                write!(f, "authentication required \u{2014} run 'syns login' first")
            }
            CliError::RepoIdentityUnknown => write!(
                f,
                "cannot determine repo identity \u{2014} provide --name or create .syns.yaml"
            ),
            CliError::ServerUnreachable { url } => write!(f, "could not reach server at {url}"),
            CliError::Io { message } => write!(f, "{message}"),
            CliError::Config { message } => write!(f, "configuration error: {message}"),
            CliError::Upgrade(e) => write!(f, "{e}"),
            CliError::PushEmpty {
                path,
                total_walked,
                cause,
            } => {
                write!(
                    f,
                    "nothing to push from {path}\n  source contained {total_walked} files but all were excluded.\n  most likely cause: {cause}.\n  to debug: rerun with --debug to see per-file exclusion decisions.\n  to override: rerun with --allow-empty to push an empty change set."
                )
            }
            CliError::PushPartial { skipped } => {
                write!(
                    f,
                    "push aborted: {} file(s) were skipped under --strict",
                    skipped.len()
                )
            }
        }
    }
}

impl std::error::Error for CliError {}

impl From<crate::commands::upgrade::UpgradeError> for CliError {
    fn from(e: crate::commands::upgrade::UpgradeError) -> Self {
        CliError::Upgrade(e)
    }
}

// Note: This impl has branching logic (connect/timeout vs other) that cannot be unit-tested
// locally because reqwest::Error constructors are private. Covered by integration tests (U59).
impl From<reqwest::Error> for CliError {
    fn from(err: reqwest::Error) -> Self {
        if err.is_connect() || err.is_timeout() {
            CliError::ServerUnreachable {
                url: err.url().map(|u| u.to_string()).unwrap_or_default(),
            }
        } else {
            CliError::Api {
                status: err.status().map(|s| s.as_u16()),
                error: err.to_string(),
                context: None,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exit_codes() {
        assert_eq!(CliError::RepoIdentityUnknown.exit_code(), 2);
        assert_eq!(
            CliError::ServerUnreachable { url: "x".into() }.exit_code(),
            3
        );
        assert_eq!(CliError::AuthRequired.exit_code(), 1);
        assert_eq!(
            (CliError::Config {
                message: "x".into()
            })
            .exit_code(),
            1
        );
        assert_eq!(
            (CliError::Api {
                status: Some(500),
                error: "x".into(),
                context: None,
            })
            .exit_code(),
            1
        );
        assert_eq!(
            (CliError::Io {
                message: "x".into()
            })
            .exit_code(),
            1
        );

        // PD-2: Upgrade variant delegates exit codes to inner UpgradeError.
        use crate::commands::upgrade::UpgradeError;
        assert_eq!(
            CliError::Upgrade(UpgradeError::GitHubApiFailed("x".into())).exit_code(),
            3
        );
        assert_eq!(
            CliError::Upgrade(UpgradeError::ChecksumMismatch {
                filename: "x".into(),
                expected: "0".into(),
                actual: "1".into(),
            })
            .exit_code(),
            1
        );

        assert_eq!((CliError::PushPartial { skipped: vec![] }).exit_code(), 3);
        assert_eq!(
            (CliError::PushEmpty {
                path: "/tmp/x".into(),
                total_walked: 0,
                cause: "test".into(),
            })
            .exit_code(),
            6
        );
    }

    #[test]
    fn display_messages() {
        let api_with = CliError::Api {
            status: Some(404),
            error: "not_found".into(),
            context: None,
        };
        assert_eq!(api_with.to_string(), "server error (404): not_found");

        let api_without = CliError::Api {
            status: None,
            error: "timeout".into(),
            context: None,
        };
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
            CliError::ServerUnreachable {
                url: "https://example.com".into()
            }
            .to_string(),
            "could not reach server at https://example.com"
        );

        assert_eq!(
            CliError::Io {
                message: "file not found".into()
            }
            .to_string(),
            "file not found"
        );

        assert_eq!(
            CliError::Config {
                message: "bad url".into()
            }
            .to_string(),
            "configuration error: bad url"
        );

        let pe = CliError::PushEmpty {
            path: "/tmp/x".into(),
            total_walked: 3,
            cause: "every file appears to be binary".into(),
        };
        let pe_text = pe.to_string();
        assert!(pe_text.starts_with("nothing to push from /tmp/x"));
        assert!(pe_text.contains("source contained 3 files but all were excluded"));
        assert!(pe_text.contains("every file appears to be binary"));
        assert!(pe_text.contains("--allow-empty"));

        let pp = CliError::PushPartial { skipped: vec![] };
        assert_eq!(
            pp.to_string(),
            "push aborted: 0 file(s) were skipped under --strict"
        );
    }

    #[test]
    fn display_api_404_not_found_with_ls_context_renders_path_not_found() {
        let err = CliError::Api {
            status: Some(404),
            error: "not_found".into(),
            context: Some(ApiErrorContext::LsPath {
                path: "does/not/exist".into(),
            }),
        };
        assert_eq!(err.to_string(), "path not found: does/not/exist");
    }

    #[test]
    fn display_api_404_not_found_with_cat_context_renders_file_not_found() {
        let err = CliError::Api {
            status: Some(404),
            error: "not_found".into(),
            context: Some(ApiErrorContext::CatPath {
                path: "README.md".into(),
            }),
        };
        assert_eq!(err.to_string(), "file not found: README.md");
    }

    #[test]
    fn display_api_404_repo_not_found_with_ls_context_falls_through_to_generic() {
        let err = CliError::Api {
            status: Some(404),
            error: "repo_not_found".into(),
            context: Some(ApiErrorContext::LsPath { path: "foo".into() }),
        };
        assert_eq!(err.to_string(), "server error (404): repo_not_found");
    }

    #[test]
    fn display_api_404_not_found_without_context_falls_through_to_generic() {
        let err = CliError::Api {
            status: Some(404),
            error: "not_found".into(),
            context: None,
        };
        assert_eq!(err.to_string(), "server error (404): not_found");
    }

    #[test]
    fn display_api_non_404_with_ls_context_falls_through_to_generic() {
        let err = CliError::Api {
            status: Some(500),
            error: "internal_error".into(),
            context: Some(ApiErrorContext::LsPath { path: "foo".into() }),
        };
        assert_eq!(err.to_string(), "server error (500): internal_error");
    }

    #[test]
    fn with_ls_path_context_attaches_context_to_api_variant() {
        let err = CliError::Api {
            status: Some(404),
            error: "not_found".into(),
            context: None,
        };
        let rewrapped = err.with_ls_path_context("a/b/c".into());
        assert!(matches!(
            rewrapped,
            CliError::Api {
                status: Some(404),
                ref error,
                context: Some(ApiErrorContext::LsPath { ref path }),
            } if error == "not_found" && path == "a/b/c"
        ));
    }

    #[test]
    fn with_cat_path_context_attaches_context_to_api_variant() {
        let err = CliError::Api {
            status: Some(404),
            error: "not_found".into(),
            context: None,
        };
        let rewrapped = err.with_cat_path_context("README.md".into());
        assert!(matches!(
            rewrapped,
            CliError::Api {
                status: Some(404),
                ref error,
                context: Some(ApiErrorContext::CatPath { ref path }),
            } if error == "not_found" && path == "README.md"
        ));
    }

    #[test]
    fn with_ls_path_context_passes_through_non_api_variants() {
        let err = CliError::AuthRequired;
        let rewrapped = err.with_ls_path_context("foo".into());
        assert!(matches!(rewrapped, CliError::AuthRequired));
    }
}
