use crate::push::collector::{SkippedFile, write_skip_summary};

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
    PayloadTooLarge {
        bytes_sent: u64,
        file_count: usize,
        rejecter: EdgeRejecter,
    },
    PushEmpty {
        path: String,
        total_walked: usize,
        cause: String,
    },
    PushPartial {
        skipped: Vec<SkippedFile>,
        /// Mirrors `args.no_default_excludes`; consumed by the
        /// per-category breakdown rendered inside `Display` so the
        /// no-default-excludes hint line is gated correctly (SPEC § 7).
        no_default_excludes: bool,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ApiErrorContext {
    LsPath { path: String },
    CatPath { path: String },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EdgeRejecter {
    Cloudflare,
    CloudRunOrFrontend,
    Server,
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

    /// Render this error as a structured JSON envelope when one is
    /// bound by SPEC § 7 (currently `PushEmpty` and `PushPartial`).
    /// Returning `None` means the generic `{"error":"<Display>"}`
    /// fallback in `Output::format_error` should be used instead.
    ///
    /// Wire shapes (SPEC u213 § 7):
    /// - `PushEmpty` → `{"error":"push_empty","path":..,"cause":..,"totalWalked":..}`
    /// - `PushPartial` → `{"error":"push_partial","skipped":[..]}`
    pub fn json_value(&self) -> Option<serde_json::Value> {
        match self {
            CliError::PushEmpty {
                path,
                total_walked,
                cause,
            } => Some(serde_json::json!({
                "error": "push_empty",
                "path": path,
                "cause": cause,
                "totalWalked": total_walked,
            })),
            CliError::PushPartial { skipped, .. } => Some(serde_json::json!({
                "error": "push_partial",
                "skipped": skipped,
            })),
            CliError::PayloadTooLarge {
                bytes_sent,
                file_count,
                rejecter,
            } => {
                let rejecter_wire = match rejecter {
                    EdgeRejecter::Cloudflare => "cloudflare",
                    EdgeRejecter::CloudRunOrFrontend => "cloud_run",
                    EdgeRejecter::Server => "server",
                };
                Some(serde_json::json!({
                    "error": "payload_too_large",
                    "rejecter": rejecter_wire,
                    "bytesSent": bytes_sent,
                    "fileCount": file_count,
                    "suggestion": "auto-chunked when feasible; otherwise split with --exclude PATTERN and re-run",
                }))
            }
            _ => None,
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
            CliError::PayloadTooLarge {
                bytes_sent,
                file_count,
                rejecter,
            } => {
                let rejecter_phrase = match rejecter {
                    EdgeRejecter::Cloudflare => "Cloudflare's edge",
                    EdgeRejecter::CloudRunOrFrontend => "Cloud Run's frontend",
                    EdgeRejecter::Server => "the Syns server",
                };
                let bytes_mib = *bytes_sent as f64 / (1024.0 * 1024.0);
                write!(
                    f,
                    "push body rejected by {rejecter_phrase} (HTTP 413).\n  attempted {file_count} file(s), ~{bytes_mib:.1} MiB.\n  approximate caps: Cloudflare Free tier ~100 MiB; Cloud Run HTTP/1.1 ~32 MiB.\n  the CLI auto-chunks below a 25 MiB per-commit budget when feasible; if a single file exceeds the budget, split with:\n    syns push --exclude '<pattern>'\n  subsequent pushes deduplicate via the local manifest, so only new content is sent each time."
                )
            }
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
            CliError::PushPartial {
                skipped,
                no_default_excludes,
            } => {
                // SPEC § 7: headline first, then the same per-category
                // skip-summary block from § 3.4 (without the strict
                // hint — strict is true by construction here; the
                // binary and no-default-excludes hints remain).
                write!(
                    f,
                    "push aborted: {} file(s) were skipped under --strict",
                    skipped.len()
                )?;
                if !skipped.is_empty() {
                    writeln!(f)?;
                    write_skip_summary(f, skipped, /* strict = */ true, *no_default_excludes)?;
                }
                Ok(())
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

        assert_eq!(
            (CliError::PushPartial {
                skipped: vec![],
                no_default_excludes: false,
            })
            .exit_code(),
            3
        );
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

        let pp = CliError::PushPartial {
            skipped: vec![],
            no_default_excludes: false,
        };
        // Empty-skipped short-circuit: headline only, no breakdown.
        assert_eq!(
            pp.to_string(),
            "push aborted: 0 file(s) were skipped under --strict"
        );
    }

    #[test]
    fn display_push_partial_includes_per_category_breakdown_after_headline() {
        // SPEC § 7 ordering: headline first, then the per-category
        // skip-summary block (without the strict hint).
        use crate::push::collector::SkipReason;
        let pp = CliError::PushPartial {
            skipped: vec![
                SkippedFile {
                    path: "logo.png".into(),
                    reason: SkipReason::Binary,
                },
                SkippedFile {
                    path: "dist/bundle.js".into(),
                    reason: SkipReason::DefaultExcludeDir,
                },
            ],
            no_default_excludes: false,
        };
        let s = pp.to_string();
        let headline_pos = s.find("push aborted: 2 file(s)").expect("headline missing");
        let warning_pos = s
            .find("warning: 2 file(s) skipped")
            .expect("breakdown missing");
        assert!(
            headline_pos < warning_pos,
            "headline must come BEFORE per-category breakdown (SPEC § 7); got: {s}"
        );
        assert!(s.contains("binary content (1): logo.png"));
        assert!(s.contains("default-excluded directory (1): dist/bundle.js"));
        // strict hint absent (PushPartial implies strict=true).
        assert!(!s.contains("pass --strict to fail the push"));
        // binary hint present.
        assert!(s.contains("add binary extensions"));
        // no-default-excludes hint present (no_default_excludes=false + DefaultExcludeDir entry).
        assert!(s.contains("pass --no-default-excludes"));
    }

    #[test]
    fn json_value_push_empty_uses_structured_shape() {
        let pe = CliError::PushEmpty {
            path: "/tmp/proj".into(),
            total_walked: 3,
            cause: "every file matches a --exclude pattern".into(),
        };
        let v = pe.json_value().expect("PushEmpty has a json_value");
        assert_eq!(v["error"], "push_empty");
        assert_eq!(v["path"], "/tmp/proj");
        assert_eq!(v["cause"], "every file matches a --exclude pattern");
        assert_eq!(v["totalWalked"], 3);
    }

    #[test]
    fn json_value_push_partial_uses_structured_shape() {
        use crate::push::collector::SkipReason;
        let pp = CliError::PushPartial {
            skipped: vec![SkippedFile {
                path: "logo.png".into(),
                reason: SkipReason::Binary,
            }],
            no_default_excludes: false,
        };
        let v = pp.json_value().expect("PushPartial has a json_value");
        assert_eq!(v["error"], "push_partial");
        let arr = v["skipped"].as_array().unwrap();
        assert_eq!(arr.len(), 1);
        assert_eq!(arr[0]["path"], "logo.png");
        assert_eq!(arr[0]["reason"], "binary");
    }

    #[test]
    fn json_value_returns_none_for_unrelated_variants() {
        assert!(CliError::AuthRequired.json_value().is_none());
        assert!(
            CliError::Io {
                message: "x".into()
            }
            .json_value()
            .is_none()
        );
    }

    #[test]
    fn display_payload_too_large_renders_actionable_multi_line_message() {
        let cf = CliError::PayloadTooLarge {
            bytes_sent: 40 * 1024 * 1024,
            file_count: 1234,
            rejecter: EdgeRejecter::Cloudflare,
        };
        let cf_text = cf.to_string();
        assert!(
            cf_text.contains("Cloudflare's edge"),
            "missing Cloudflare's edge phrase: {cf_text}"
        );
        assert!(cf_text.contains("HTTP 413"), "missing HTTP 413: {cf_text}");
        assert!(
            cf_text.contains("1234 file(s)"),
            "missing file count: {cf_text}"
        );
        assert!(cf_text.contains("~40.0 MiB"), "missing MiB: {cf_text}");
        assert!(
            cf_text.contains("25 MiB"),
            "missing budget mention: {cf_text}"
        );
        assert!(
            cf_text.contains("syns push --exclude"),
            "missing exclude hint: {cf_text}"
        );
        assert!(
            cf_text.contains("manifest"),
            "missing manifest mention: {cf_text}"
        );
        assert_eq!(
            cf_text.lines().count(),
            6,
            "expected 6 lines, got:\n{cf_text}"
        );

        let cr = CliError::PayloadTooLarge {
            bytes_sent: 40 * 1024 * 1024,
            file_count: 1234,
            rejecter: EdgeRejecter::CloudRunOrFrontend,
        };
        let cr_text = cr.to_string();
        assert!(cr_text.contains("Cloud Run's frontend"));
        assert!(!cr_text.contains("Cloudflare's edge"));
        assert_eq!(cr_text.lines().count(), 6);

        let srv = CliError::PayloadTooLarge {
            bytes_sent: 40 * 1024 * 1024,
            file_count: 1234,
            rejecter: EdgeRejecter::Server,
        };
        let srv_text = srv.to_string();
        assert!(srv_text.contains("the Syns server"));
        assert!(!srv_text.contains("Cloudflare's edge"));
        assert!(!srv_text.contains("Cloud Run's frontend"));
        assert_eq!(srv_text.lines().count(), 6);
    }

    #[test]
    fn exit_code_payload_too_large_is_one() {
        let err = CliError::PayloadTooLarge {
            bytes_sent: 0,
            file_count: 0,
            rejecter: EdgeRejecter::CloudRunOrFrontend,
        };
        assert_eq!(err.exit_code(), 1);
    }

    #[test]
    fn json_value_payload_too_large_returns_structured_envelope() {
        let cf = CliError::PayloadTooLarge {
            bytes_sent: 33_554_432,
            file_count: 42,
            rejecter: EdgeRejecter::Cloudflare,
        };
        let v = cf.json_value().expect("PayloadTooLarge has a json_value");
        assert_eq!(v["error"], "payload_too_large");
        assert_eq!(v["rejecter"], "cloudflare");
        assert_eq!(v["bytesSent"], 33_554_432);
        assert_eq!(v["fileCount"], 42);
        assert!(v["suggestion"].as_str().is_some_and(|s| !s.is_empty()));

        let cr = CliError::PayloadTooLarge {
            bytes_sent: 0,
            file_count: 0,
            rejecter: EdgeRejecter::CloudRunOrFrontend,
        };
        assert_eq!(cr.json_value().unwrap()["rejecter"], "cloud_run");

        let srv = CliError::PayloadTooLarge {
            bytes_sent: 0,
            file_count: 0,
            rejecter: EdgeRejecter::Server,
        };
        assert_eq!(srv.json_value().unwrap()["rejecter"], "server");
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
