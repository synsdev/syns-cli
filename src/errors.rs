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
    /// No repository identity could be determined. The remedy names what
    /// the invocation that raised it accepts, and so which line renders.
    RepoIdentityUnknown {
        remedy: IdentityRemedy,
    },
    /// `syns pull OWNER/NAME [PATH]` where the identity file nearest the
    /// starting directory, at or above it, names another repository — a
    /// bare run, a path argument and a run from a sub-folder alike —
    /// refused before any request (SPEC u263, issue 130); and `syns push
    /// [PATH]` resolving a repository other than the one the identity
    /// file standing in its addressed directory names (SPEC u309, issue
    /// 231). `path` is the directory holding the identity file that
    /// decided, `standing` its pair as spelt, `requested` the pair the
    /// run resolved lower-cased, and `remedy` which of the two commands'
    /// moves the line offers.
    PathBelongsToAnotherRepository {
        path: std::path::PathBuf,
        standing: String,
        requested: String,
        remedy: BelongsRemedy,
    },
    /// SPEC u290, the holder-acting refusal (`D-102`): a command that
    /// would change the holding repository, run inside a scoped folder,
    /// refused before any request, credential read or prompt. `command`
    /// is the command as typed up to its verb or first update option,
    /// `holder` the pair lower-cased, `dir` the folder's absolute path.
    HolderActing {
        command: String,
        holder: String,
        dir: std::path::PathBuf,
    },
    /// SPEC u329, the folder visibility redirect (`D-122`): `syns repo
    /// --visibility` given alone inside a folder bound to its holder,
    /// refused before any request under the holder-acting refusal's code
    /// in its place, naming the share verb that sets the folder's own
    /// visibility. `holder` and `dir` as `HolderActing` carries them,
    /// `visibility` the value given.
    FolderVisibilityRedirect {
        holder: String,
        dir: std::path::PathBuf,
        visibility: String,
    },
    /// SPEC u290, the misplaced-folder refusal (`D-101`): a folder whose
    /// place under its holder's checkout at `checkout` differs from the
    /// path it records. SPEC u291: `back` is the absolute directory the
    /// folder stands right at — `checkout` joined with `recorded` under a
    /// holder checkout, and under a folder checked out alone that folder's
    /// directory joined with `recorded` counted from its recorded path.
    FolderMoved {
        dir: std::path::PathBuf,
        holder: String,
        recorded: String,
        actual: String,
        checkout: std::path::PathBuf,
        back: std::path::PathBuf,
    },
    /// SPEC u290, the misplaced-folder refusal (`D-101`): a folder
    /// standing inside `checkout`, a checkout of `standing`, spelt as its
    /// file spells it.
    FolderInAnotherCheckout {
        dir: std::path::PathBuf,
        holder: String,
        checkout: std::path::PathBuf,
        standing: String,
    },
    ServerUnreachable {
        url: String,
    },
    /// SPEC u298, `CliError::TlsRefused`: a failure the TLS layer itself
    /// raised during the handshake, a refused certificate included —
    /// `TLS_REFUSED` (`unregistered`, `D-114`), class `local`, at exit `1`
    /// with no retry, its line led by its wire form. `url` is the address
    /// the request named and `reason` the TLS layer's.
    TlsRefused {
        url: String,
        reason: String,
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
    /// The refusal `SmartPushOptions.expected` raises before any request
    /// leaves: the collected set's file hashes differ from the expected
    /// ones at each named path (SPEC u256 § Contract Surface).
    CollectedSetChanged {
        paths: Vec<String>,
    },
    /// A convergence outcome that does not land, carried to `main` whole:
    /// the one document machine-readable mode writes, the one line human
    /// mode writes, and the exit the outcome registers.
    SyncRefusal {
        document: serde_json::Value,
        line: String,
        exit: i32,
    },
    /// SPEC u270: a reading run whose answer is partial — the tree
    /// arrived truncated, or content at some path was not read. `D-080`
    /// puts it at exit `1` under a code of the binary's own: `line` is
    /// the diagnostic line outside machine-readable mode and `document`
    /// the one document inside it, the result standing beside `error`
    /// rather than replaced by it.
    PartialAnswer {
        document: serde_json::Value,
        line: String,
    },
    /// SPEC u270: `syns read PATH` over a content that is not text, and
    /// SPEC u271 and u283: a write verb handed one where it asked for
    /// text. `surface` picks the wording — `syns cat PATH` keeps passing
    /// those bytes through (`D-080`), and each write wording names the
    /// way that verb publishes bytes exactly (`D-088`).
    NotText {
        path: String,
        surface: NotTextSurface,
    },
    /// SPEC u271: a write refused because the repository moved past the
    /// parent it claimed, the refused answer naming `currentSha`. It
    /// stands at exit `7`, an exit no other outcome of the binary takes,
    /// and its document carries the hash beside `error`.
    WriteConflict {
        parent: String,
        current_sha: String,
    },
    /// SPEC u292, the unsupported-server refusal (`D-107`): a folder
    /// check whose version-list page the server did not narrow to the
    /// folder. `FOLDER_WRITE_UNSUPPORTED` (`unregistered`) at exit `1`,
    /// its document `error` alone, and never answered as a folder that
    /// moved. `folder` is the folder's recorded path.
    FolderWriteUnsupported {
        folder: String,
    },
    /// SPEC u329, `CliError::FolderVisibilityUnsupported` (`D-122`): a
    /// server predating the folder visibility route — a marking answered
    /// `404` `not_found`, or a `visibility` sent through an identity
    /// refused as only such a server refuses it. `FOLDER_VISIBILITY_UNSUPPORTED`
    /// (`unregistered`) at exit `1`, nothing changed. `folder` is
    /// `{path} of {holder}` on a marking and `{owner}/{name}` through
    /// `syns repo`.
    FolderVisibilityUnsupported {
        folder: String,
    },
    /// SPEC u271: `syns commit` over a changeset naming neither a file
    /// nor a deletion, at exit `6` before any request. Its document is
    /// `error` alone — none of the three keys a collected publication's
    /// own emptiness document carries.
    ChangesetEmpty,
    /// SPEC u280, the left-out refusal (`unregistered`): a head file
    /// could not be written because the folder standing at its path
    /// still holds entries no retrieval removes — each named as a
    /// root-relative path, a folder suffixed `/`, in lexical order.
    LeftOut {
        path: String,
        entries: Vec<String>,
    },
    /// SPEC u283, the too-large refusal: a content handed to `write` or
    /// `commit` past `MAX_FILE_BYTES` on its decoded length, raised
    /// before any push is composed. Code `PAYLOAD_TOO_LARGE` at exit `1`,
    /// its document `error` alone.
    FileTooLarge {
        path: String,
        size: u64,
    },
}

/// Which of the two not-text wordings a refused content takes (SPEC
/// u271 Contract Surface, the not-text refusal): the wire form is the
/// same code the numbered read already raises, and only the line differs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NotTextSurface {
    /// `syns read PATH` (SPEC u270).
    NumberedRead,
    /// `syns write PATH` without `--bytes` (SPEC u283).
    Write,
    /// A `content` member of a `syns commit` changeset (SPEC u283).
    Commit,
    /// `syns edit PATH` over a parent content that is not text (SPEC u283).
    Edit,
}

/// The partial-answer refusal's truncated-tree arm (SPEC u270 Contract
/// Surface, the partial-answer refusal).
pub fn partial_truncated_tree(version: u32) -> String {
    format!("partial answer: the tree at version {version} arrived truncated")
}

/// The partial-answer refusal's refused-content arm. `unread` counts the
/// paths that were not read, `total` the paths the fan-out took up, and
/// `first` is the first of the unread ones in ascending path order.
pub fn partial_unread_paths(unread: usize, total: usize, first: &str) -> String {
    format!("partial answer: {unread} of {total} paths were not read, the first {first}")
}

/// Which remedies the invocation raising `RepoIdentityUnknown` accepts
/// (SPEC u262 Contract Surface).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IdentityRemedy {
    /// `syns push [PATH]`, which takes `--name`.
    NameOption,
    /// `syns pull`, which takes the repository as a positional; `path` is
    /// the starting directory where a path positional was bound.
    RepositoryPositional { path: Option<std::path::PathBuf> },
    /// Every other invocation: only an identity file answers it.
    IdentityFile,
    /// Every invocation taking `--repo` (SPEC u283): the option names
    /// the repository, or an identity file does.
    RepoOption,
}

/// Which command's move the ownership refusal offers (SPEC u309
/// Contract Surface): every retrieval site carries `Pull`, and
/// `refuse_another_repository` alone carries `Push`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BelongsRemedy {
    /// `syns pull`: retrieve the repository into another directory.
    Pull,
    /// `syns push [PATH]`: publish the repository from another directory.
    Push,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ApiErrorContext {
    LsPath {
        path: String,
    },
    CatPath {
        path: String,
    },
    /// SPEC u270: the refusal line a read verb chose because the run
    /// resolved its reference from a `--version` the caller gave — the
    /// version-not-found refusal or the path-not-found refusal, both
    /// spelt in `crate::read`. It stands in place of the registered
    /// not-found line on a `404` `not_found` and nowhere else.
    VersionedRead {
        line: String,
    },
    /// A `409` `conflict` whose body names `currentSha`: the head moved
    /// past the parent the publication claimed, and `current_sha` is the
    /// hash that answer named. A `conflict` naming no head — an identity
    /// already taken — carries no such context.
    HeadMoved {
        current_sha: String,
    },
    /// A `409` `missing_blobs` whose body names a `missing` map from path
    /// to hash: the paths the publication must resend carrying their
    /// content (SPEC u280, `smart_push` 5). An answer naming no such map
    /// carries no context.
    MissingBlobs {
        missing: std::collections::BTreeMap<String, String>,
    },
    /// SPEC u329 `ApiErrorContext::Refusal`: the string `message` a `422`
    /// `validation_error` served, kept so a caller can tell one refusal
    /// from another. It renders as a refusal carrying no context renders.
    Refusal {
        message: String,
    },
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
            CliError::RepoIdentityUnknown { .. } => 2,
            CliError::PathBelongsToAnotherRepository { .. } => 2,
            CliError::HolderActing { .. } => 2,
            CliError::FolderVisibilityRedirect { .. } => 2,
            CliError::FolderMoved { .. } => 2,
            CliError::FolderInAnotherCheckout { .. } => 2,
            CliError::ServerUnreachable { .. } => 3,
            CliError::PushPartial { .. } => 3,
            CliError::PushEmpty { .. } => 6,
            // SPEC u271: the empty-changeset refusal reads to a caller as
            // "nothing left this machine" exactly as a folder
            // publication's own emptiness does.
            CliError::ChangesetEmpty => 6,
            // SPEC u271: the one exit no other outcome of the binary
            // takes, reached only by a write whose answer named
            // `currentSha`.
            CliError::WriteConflict { .. } => 7,
            CliError::SyncRefusal { exit, .. } => *exit,
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
            CliError::SyncRefusal { document, .. } => Some(document.clone()),
            CliError::PartialAnswer { document, .. } => Some(document.clone()),
            // SPEC u271, the conflict refusal: the moved head stands
            // beside `error` rather than inside its one string alone, so
            // a caller reading the document never parses the line.
            CliError::WriteConflict { current_sha, .. } => Some(serde_json::json!({
                "error": self.to_string(),
                "currentSha": current_sha,
            })),
            _ => None,
        }
    }

    /// Replaces the remedy on `RepoIdentityUnknown`; every other variant
    /// passes through unchanged.
    pub fn with_identity_remedy(self, remedy: IdentityRemedy) -> CliError {
        match self {
            CliError::RepoIdentityUnknown { .. } => CliError::RepoIdentityUnknown { remedy },
            other => other,
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

    /// Puts a `crate::read` refusal line on an `Api` error, so a `404`
    /// `not_found` renders it in place of the registered not-found line
    /// (SPEC u270 Contract Surface, the path-not-found refusal). Every
    /// other variant, and every other status, passes through unchanged.
    pub fn with_versioned_read_context(self, line: String) -> CliError {
        match self {
            CliError::Api { status, error, .. } => CliError::Api {
                status,
                error,
                context: Some(ApiErrorContext::VersionedRead { line }),
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
                context: Some(ApiErrorContext::VersionedRead { line }),
            } if error == "not_found" => write!(f, "{line}"),
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
            CliError::RepoIdentityUnknown { remedy } => match remedy {
                IdentityRemedy::NameOption => write!(
                    f,
                    "cannot determine repo identity \u{2014} provide --name or create .syns.yaml"
                ),
                IdentityRemedy::RepositoryPositional { path: None } => write!(
                    f,
                    "cannot determine repo identity \u{2014} name the repository as OWNER/NAME, or run inside a directory at or below one holding .syns.yaml"
                ),
                IdentityRemedy::RepositoryPositional { path: Some(path) } => write!(
                    f,
                    "cannot determine repo identity for {} \u{2014} name the repository as OWNER/NAME before the path, or create .syns.yaml in that directory or one above it",
                    path.display()
                ),
                IdentityRemedy::IdentityFile => write!(
                    f,
                    "cannot determine repo identity \u{2014} run inside a directory at or below one holding .syns.yaml"
                ),
                IdentityRemedy::RepoOption => write!(
                    f,
                    "cannot determine repo identity \u{2014} pass --repo OWNER/NAME, or run inside a directory at or below one holding .syns.yaml"
                ),
            },
            CliError::PathBelongsToAnotherRepository {
                path,
                standing,
                requested,
                remedy,
            } => match remedy {
                BelongsRemedy::Pull => write!(
                    f,
                    "{} already belongs to {standing} \u{2014} pull {requested} into another directory, or remove {}",
                    path.display(),
                    path.join(".syns.yaml").display()
                ),
                BelongsRemedy::Push => write!(
                    f,
                    "{} already belongs to {standing} \u{2014} push {requested} from another directory, or remove {}",
                    path.display(),
                    path.join(".syns.yaml").display()
                ),
            },
            CliError::HolderActing {
                command,
                holder,
                dir,
            } => write!(
                f,
                "holder root required: {command} acts on the holding repository {holder}, not on the folder {} \u{2014} run it from the root of a checkout of {holder}",
                dir.display()
            ),
            CliError::FolderVisibilityRedirect {
                holder,
                dir,
                visibility,
            } => write!(
                f,
                "holder root required: syns repo --visibility acts on the holding repository {holder}, not on the folder {} \u{2014} set the folder's own visibility with: syns share . --visibility {visibility}",
                dir.display()
            ),
            CliError::FolderMoved {
                dir,
                holder,
                recorded,
                actual,
                checkout,
                back,
            } => write!(
                f,
                "folder out of place: {} records {recorded} in {holder} but stands at {actual} in its checkout at {} \u{2014} move the folder back to {}, or correct the path its .syns.yaml records to {actual}",
                dir.display(),
                checkout.display(),
                back.display()
            ),
            CliError::FolderInAnotherCheckout {
                dir,
                holder,
                checkout,
                standing,
            } => write!(
                f,
                "folder out of place: {} is a folder of {holder} but stands inside {}, a checkout of {standing} \u{2014} move it into a checkout of {holder}, or remove {}/.syns.yaml",
                dir.display(),
                checkout.display(),
                dir.display()
            ),
            CliError::ServerUnreachable { url } => write!(f, "could not reach server at {url}"),
            CliError::TlsRefused { url, reason } => {
                write!(f, "tls_refused: the TLS layer refused {url}: {reason}")
            }
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
                // SPEC u280: the count names the drops `--strict`
                // refuses — the size drops — while the block beneath it
                // names every drop.
                write!(
                    f,
                    "push aborted: {} file(s) were skipped under --strict",
                    skipped
                        .iter()
                        .filter(|sf| sf.reason == crate::push::collector::SkipReason::TooLarge)
                        .count()
                )?;
                if !skipped.is_empty() {
                    writeln!(f)?;
                    write_skip_summary(f, skipped, /* strict = */ true, *no_default_excludes)?;
                }
                Ok(())
            }
            CliError::CollectedSetChanged { paths } => write!(
                f,
                "the folder changed while its publication was prepared: {}",
                paths.join(", ")
            ),
            CliError::SyncRefusal { line, .. } => write!(f, "{line}"),
            CliError::PartialAnswer { line, .. } => write!(f, "{line}"),
            CliError::NotText {
                path,
                surface: NotTextSurface::NumberedRead,
            } => write!(
                f,
                "cannot number content that is not text: {path} \u{2014} read it with syns cat {path}"
            ),
            CliError::NotText {
                path,
                surface: NotTextSurface::Write,
            } => write!(
                f,
                "cannot write content that is not text: {path} \u{2014} pass --bytes to publish its bytes exactly"
            ),
            CliError::NotText {
                path,
                surface: NotTextSurface::Commit,
            } => write!(
                f,
                "cannot write content that is not text: {path} \u{2014} send it as contentBase64 to publish its bytes exactly"
            ),
            CliError::NotText {
                path,
                surface: NotTextSurface::Edit,
            } => write!(
                f,
                "cannot write content that is not text: {path} \u{2014} edit changes text alone; replace it whole with syns write {path} --bytes"
            ),
            CliError::WriteConflict {
                parent,
                current_sha,
            } => write!(
                f,
                "conflict: the repository moved past {parent}; its head is now {current_sha}"
            ),
            CliError::FolderWriteUnsupported { folder } => write!(
                f,
                "folder_write_unsupported: the server does not support writes inside a folder yet, so {folder} cannot be written from inside it"
            ),
            CliError::FolderVisibilityUnsupported { folder } => write!(
                f,
                "folder_visibility_unsupported: the server does not support a folder's own visibility yet, so {folder} keeps the visibility it had"
            ),
            CliError::ChangesetEmpty => write!(
                f,
                "push_empty: the changeset names neither a file nor a deletion"
            ),
            CliError::LeftOut { path, entries } => write!(
                f,
                "could not write {path}: the folder there still holds {}, which no retrieval removes \u{2014} move or remove them, then run again",
                entries.join(", ")
            ),
            CliError::FileTooLarge { path, size } => write!(
                f,
                "payload_too_large: {path} holds {size} bytes, past the {} one file may hold; nothing was sent",
                crate::push::collector::file_bound_label()
            ),
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
// locally because reqwest::Error constructors are private. Covered by integration tests (U59),
// and by `dropped_push_connection_classifies_as_server_unreachable` (u256).
//
// A request whose connection closed before any status came back — sent, and answered by
// nothing — is `SERVER_UNREACHABLE` as a refused connection is: the retry the class invites
// is the right move, and a convergence's outbox settles whether the dropped publication
// landed (SPEC u256).
impl From<reqwest::Error> for CliError {
    fn from(err: reqwest::Error) -> Self {
        if err.is_connect() || err.is_timeout() || (err.is_request() && err.status().is_none()) {
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

    // SPEC u270 Contract Surface, the partial-answer refusal: its one
    // string is the diagnostic line outside machine-readable mode and the
    // document's own `error` inside it, at exit `1`.
    #[test]
    fn partial_answer_refusal_carries_its_line_its_document_and_exit_one() {
        let truncated = partial_truncated_tree(58);
        assert_eq!(
            truncated,
            "partial answer: the tree at version 58 arrived truncated"
        );
        let unread = partial_unread_paths(2, 7, "src/a.ts");
        assert_eq!(
            unread,
            "partial answer: 2 of 7 paths were not read, the first src/a.ts"
        );

        let err = CliError::PartialAnswer {
            document: serde_json::json!({
                "version": 58,
                "matches": [],
                "error": unread.clone(),
            }),
            line: unread.clone(),
        };
        assert_eq!(err.to_string(), unread);
        assert_eq!(err.exit_code(), 1);
        let doc = err.json_value().expect("the refusal carries a document");
        assert_eq!(doc["error"], serde_json::json!(unread));
        assert_eq!(doc["version"], serde_json::json!(58));
        assert!(
            doc.get("matches").is_some(),
            "the result stands beside `error`"
        );
    }

    // SPEC u270 Contract Surface, the not-text refusal: raised on the
    // numbered read alone, naming the path and the verb that passes the
    // bytes through.
    #[test]
    fn not_text_refusal_names_the_path_and_cat_at_exit_one() {
        let err = CliError::NotText {
            path: "assets/logo.png".to_string(),
            surface: NotTextSurface::NumberedRead,
        };
        assert_eq!(
            err.to_string(),
            "cannot number content that is not text: assets/logo.png \u{2014} read it with syns cat assets/logo.png"
        );
        assert_eq!(err.exit_code(), 1);
        assert!(
            err.json_value().is_none(),
            "the not-text refusal takes the generic envelope"
        );
    }

    // SPEC u270 Contract Surface, the path-not-found refusal: it stands
    // in place of the registered not-found line on a `404` `not_found`,
    // and nowhere else.
    #[test]
    fn a_versioned_read_line_replaces_the_registered_not_found_line() {
        let err = CliError::Api {
            status: Some(404),
            error: "not_found".to_string(),
            context: None,
        }
        .with_versioned_read_context("path not found at version 58: a.md".to_string());
        assert_eq!(err.to_string(), "path not found at version 58: a.md");
        assert_eq!(err.exit_code(), 1);

        let other = CliError::Api {
            status: Some(500),
            error: "internal_error".to_string(),
            context: None,
        }
        .with_versioned_read_context("path not found at version 58: a.md".to_string());
        assert_eq!(other.to_string(), "server error (500): internal_error");
    }

    // SPEC u271 Contract Surface, the conflict refusal: its line names
    // the parent and the moved head, its document carries the hash
    // beside `error`, and it stands at exit `7`.
    #[test]
    fn the_conflict_refusal_carries_its_line_its_document_and_exit_seven() {
        let err = CliError::WriteConflict {
            parent: "a".repeat(40),
            current_sha: "c".repeat(40),
        };
        assert_eq!(
            err.to_string(),
            format!(
                "conflict: the repository moved past {}; its head is now {}",
                "a".repeat(40),
                "c".repeat(40)
            )
        );
        assert_eq!(err.exit_code(), 7);
        let doc = err.json_value().expect("the refusal carries a document");
        assert_eq!(doc["currentSha"], serde_json::json!("c".repeat(40)));
        assert_eq!(doc["error"], serde_json::json!(err.to_string()));
        assert_eq!(
            doc.as_object().map(|m| m.len()),
            Some(2),
            "the document carries `error` and `currentSha` and nothing else"
        );
    }

    // SPEC u298 Contract Surface, `CliError::TlsRefused`: its one line is
    // led by its wire form and names the address and the TLS layer's
    // reason, at exit `1`, its document `error` alone.
    #[test]
    fn the_tls_refusal_names_the_address_and_the_reason_at_exit_one() {
        let err = CliError::TlsRefused {
            url: "https://localhost:9/api/auth/get-session".to_string(),
            reason: "invalid peer certificate: UnknownIssuer".to_string(),
        };
        assert_eq!(
            err.to_string(),
            "tls_refused: the TLS layer refused https://localhost:9/api/auth/get-session: invalid peer certificate: UnknownIssuer"
        );
        assert_eq!(err.exit_code(), 1);
        assert!(err.json_value().is_none());
    }

    // SPEC u292 Contract Surface, `CliError::FolderWriteUnsupported`: its
    // one line names the folder, it stands at exit `1`, and its document
    // is `error` alone.
    #[test]
    fn the_unsupported_server_refusal_names_the_folder_at_exit_one() {
        let err = CliError::FolderWriteUnsupported {
            folder: "clients/vela/q3-board".to_string(),
        };
        assert_eq!(
            err.to_string(),
            "folder_write_unsupported: the server does not support writes inside a folder yet, so clients/vela/q3-board cannot be written from inside it"
        );
        assert_eq!(err.exit_code(), 1);
        assert!(err.json_value().is_none());
    }

    // SPEC u271 Contract Surface, the empty-changeset refusal: exit `6`
    // and a document of `error` alone, carrying none of the three keys a
    // collected publication's own emptiness document carries.
    #[test]
    fn the_empty_changeset_refusal_is_exit_six_and_error_alone() {
        let err = CliError::ChangesetEmpty;
        assert_eq!(
            err.to_string(),
            "push_empty: the changeset names neither a file nor a deletion"
        );
        assert_eq!(err.exit_code(), 6);
        assert!(
            err.json_value().is_none(),
            "the refusal takes the generic `error`-alone envelope"
        );
        let rendered = crate::output::Output::new(true).format_error(&err);
        let doc: serde_json::Value = serde_json::from_str(&rendered).unwrap();
        assert_eq!(doc.as_object().map(|m| m.len()), Some(1));
        assert!(doc.get("path").is_none());
        assert!(doc.get("cause").is_none());
        assert!(doc.get("totalWalked").is_none());
    }

    // SPEC u283 Contract Surface, the not-text refusals: arms of the
    // code the numbered read already raises, at exit `1`, each naming the
    // path it was raised on and the way its verb publishes bytes.
    #[test]
    fn the_write_not_text_refusal_is_a_second_arm_of_the_same_code() {
        let err = CliError::NotText {
            path: "b.bin".to_string(),
            surface: NotTextSurface::Write,
        };
        assert_eq!(
            err.to_string(),
            "cannot write content that is not text: b.bin \u{2014} pass --bytes to publish its bytes exactly"
        );
        for (surface, line) in [
            (
                NotTextSurface::Commit,
                "cannot write content that is not text: b.bin \u{2014} send it as contentBase64 to publish its bytes exactly",
            ),
            (
                NotTextSurface::Edit,
                "cannot write content that is not text: b.bin \u{2014} edit changes text alone; replace it whole with syns write b.bin --bytes",
            ),
        ] {
            let other = CliError::NotText {
                path: "b.bin".to_string(),
                surface,
            };
            assert_eq!(other.to_string(), line);
            assert_eq!(other.exit_code(), 1);
            assert!(other.json_value().is_none());
            assert!(!line.contains("holds UTF-8 text alone"));
        }
        assert_eq!(err.exit_code(), 1);
        assert!(err.json_value().is_none());
        assert_ne!(
            err.to_string(),
            CliError::NotText {
                path: "b.bin".to_string(),
                surface: NotTextSurface::NumberedRead,
            }
            .to_string()
        );
    }

    #[test]
    fn exit_codes() {
        assert_eq!(
            CliError::RepoIdentityUnknown {
                remedy: IdentityRemedy::IdentityFile
            }
            .exit_code(),
            2
        );
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
    fn each_remedy_renders_its_line() {
        let cases = [
            (
                IdentityRemedy::NameOption,
                "cannot determine repo identity \u{2014} provide --name or create .syns.yaml",
            ),
            (
                IdentityRemedy::RepositoryPositional { path: None },
                "cannot determine repo identity \u{2014} name the repository as OWNER/NAME, or run inside a directory at or below one holding .syns.yaml",
            ),
            (
                IdentityRemedy::RepositoryPositional {
                    path: Some(std::path::PathBuf::from("/w/x")),
                },
                "cannot determine repo identity for /w/x \u{2014} name the repository as OWNER/NAME before the path, or create .syns.yaml in that directory or one above it",
            ),
            (
                IdentityRemedy::IdentityFile,
                "cannot determine repo identity \u{2014} run inside a directory at or below one holding .syns.yaml",
            ),
            (
                IdentityRemedy::RepoOption,
                "cannot determine repo identity \u{2014} pass --repo OWNER/NAME, or run inside a directory at or below one holding .syns.yaml",
            ),
        ];
        for (remedy, line) in cases {
            let err = CliError::RepoIdentityUnknown { remedy };
            assert_eq!(err.to_string(), line);
            assert_eq!(err.exit_code(), 2);
        }
    }

    // SPEC u283 Contract Surface, the too-large refusal: code
    // `PAYLOAD_TOO_LARGE` at exit `1`, its document `error` alone.
    #[test]
    fn the_too_large_refusal_names_the_path_its_size_and_the_bound() {
        let size = crate::push::collector::MAX_FILE_BYTES + 1;
        let err = CliError::FileTooLarge {
            path: "big.bin".to_string(),
            size,
        };
        assert_eq!(
            err.to_string(),
            "payload_too_large: big.bin holds 26214401 bytes, past the 25 MiB one file may hold; nothing was sent"
        );
        assert_eq!(err.exit_code(), 1);
        assert!(err.json_value().is_none());
        let rendered = crate::output::Output::new(true).format_error(&err);
        let doc: serde_json::Value = serde_json::from_str(&rendered).unwrap();
        assert_eq!(doc.as_object().map(|m| m.len()), Some(1));
        assert_eq!(doc["error"], serde_json::json!(err.to_string()));
    }

    // SPEC u309 Tests, `path_belongs_line_names_the_refused_commands_move`:
    // the retrieval's line word for word under `Pull`, the publication's
    // under `Push`, exit `2` under both.
    #[test]
    fn path_belongs_line_names_the_refused_commands_move() {
        let refusal = |remedy| CliError::PathBelongsToAnotherRepository {
            path: std::path::PathBuf::from("/w/c"),
            standing: "bob/other".into(),
            requested: "alice/notes".into(),
            remedy,
        };
        let pull = refusal(BelongsRemedy::Pull);
        assert_eq!(
            pull.to_string(),
            "/w/c already belongs to bob/other \u{2014} pull alice/notes into another directory, or remove /w/c/.syns.yaml"
        );
        assert_eq!(pull.exit_code(), 2);
        let push = refusal(BelongsRemedy::Push);
        assert_eq!(
            push.to_string(),
            "/w/c already belongs to bob/other \u{2014} push alice/notes from another directory, or remove /w/c/.syns.yaml"
        );
        assert_eq!(push.exit_code(), 2);
    }

    // SPEC u290 Contract Surface, the holder-acting refusal and the
    // misplaced-folder lines: each line whole, at exit `2`, under the
    // generic error document.
    #[test]
    fn the_folder_refusals_render_their_lines_whole() {
        let acting = CliError::HolderActing {
            command: "syns collaborators add".into(),
            holder: "alice/work".into(),
            dir: std::path::PathBuf::from("/w/clients/q3"),
        };
        assert_eq!(
            acting.to_string(),
            "holder root required: syns collaborators add acts on the holding repository alice/work, not on the folder /w/clients/q3 \u{2014} run it from the root of a checkout of alice/work"
        );
        let moved = CliError::FolderMoved {
            dir: std::path::PathBuf::from("/w/archive/q3"),
            holder: "alice/work".into(),
            recorded: "clients/q3".into(),
            actual: "archive/q3".into(),
            checkout: std::path::PathBuf::from("/w"),
            back: std::path::PathBuf::from("/w/clients/q3"),
        };
        assert_eq!(
            moved.to_string(),
            "folder out of place: /w/archive/q3 records clients/q3 in alice/work but stands at archive/q3 in its checkout at /w \u{2014} move the folder back to /w/clients/q3, or correct the path its .syns.yaml records to archive/q3"
        );
        let other = CliError::FolderInAnotherCheckout {
            dir: std::path::PathBuf::from("/v/x"),
            holder: "alice/work".into(),
            checkout: std::path::PathBuf::from("/v"),
            standing: "bob/other".into(),
        };
        assert_eq!(
            other.to_string(),
            "folder out of place: /v/x is a folder of alice/work but stands inside /v, a checkout of bob/other \u{2014} move it into a checkout of alice/work, or remove /v/x/.syns.yaml"
        );
        for err in [acting, moved, other] {
            assert_eq!(err.exit_code(), 2);
            assert!(err.json_value().is_none());
        }
    }

    // SPEC u329 Contract Surface, the folder visibility redirect and
    // `CliError::FolderVisibilityUnsupported`: each line whole, the
    // redirect at exit `2` and the unsupported refusal at exit `1`, both
    // under the generic error document.
    #[test]
    fn the_folder_visibility_refusals_render_their_lines_whole() {
        let redirect = CliError::FolderVisibilityRedirect {
            holder: "alice/handbook".into(),
            dir: std::path::PathBuf::from("/w/drafts"),
            visibility: "private".into(),
        };
        assert_eq!(
            redirect.to_string(),
            "holder root required: syns repo --visibility acts on the holding repository alice/handbook, not on the folder /w/drafts \u{2014} set the folder's own visibility with: syns share . --visibility private"
        );
        assert_eq!(redirect.exit_code(), 2);
        assert!(redirect.json_value().is_none());
        let unsupported = CliError::FolderVisibilityUnsupported {
            folder: "drafts of alice/handbook".into(),
        };
        assert_eq!(
            unsupported.to_string(),
            "folder_visibility_unsupported: the server does not support a folder's own visibility yet, so drafts of alice/handbook keeps the visibility it had"
        );
        assert_eq!(unsupported.exit_code(), 1);
        assert!(unsupported.json_value().is_none());
    }

    // SPEC u329 Contract Surface, `ApiErrorContext::Refusal`: a refusal
    // holding its served message renders as one holding no context.
    #[test]
    fn a_kept_refusal_message_renders_as_a_refusal_with_no_context() {
        let bare = CliError::Api {
            status: Some(422),
            error: "validation_error".into(),
            context: None,
        };
        let kept = CliError::Api {
            status: Some(422),
            error: "validation_error".into(),
            context: Some(ApiErrorContext::Refusal {
                message: "description is too long".into(),
            }),
        };
        assert_eq!(kept.to_string(), bare.to_string());
        assert_eq!(kept.exit_code(), bare.exit_code());
    }

    #[test]
    fn with_identity_remedy_leaves_other_variants_unchanged() {
        let err = CliError::ServerUnreachable { url: "u".into() }
            .with_identity_remedy(IdentityRemedy::NameOption);
        assert!(matches!(err, CliError::ServerUnreachable { ref url } if url == "u"));
        let swapped = CliError::RepoIdentityUnknown {
            remedy: IdentityRemedy::IdentityFile,
        }
        .with_identity_remedy(IdentityRemedy::NameOption);
        assert!(matches!(
            swapped,
            CliError::RepoIdentityUnknown {
                remedy: IdentityRemedy::NameOption
            }
        ));
    }

    #[test]
    fn collected_set_changed_exits_one_naming_each_path() {
        let err = CliError::CollectedSetChanged {
            paths: vec!["a.md".into(), "b/c.md".into()],
        };
        assert_eq!(err.exit_code(), 1);
        assert!(err.json_value().is_none());
        let text = err.to_string();
        assert!(text.contains("a.md") && text.contains("b/c.md"), "{text}");
    }

    #[test]
    fn sync_refusal_carries_its_document_line_and_exit() {
        let document = serde_json::json!({"outcome": "resolution_required", "repo": "alice/proj"});
        let err = CliError::SyncRefusal {
            document: document.clone(),
            line: "resolution required for alice/proj".into(),
            exit: 4,
        };
        assert_eq!(err.exit_code(), 4);
        assert_eq!(err.json_value(), Some(document));
        assert_eq!(err.to_string(), "resolution required for alice/proj");
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
            CliError::RepoIdentityUnknown {
                remedy: IdentityRemedy::IdentityFile
            }
            .to_string(),
            "cannot determine repo identity \u{2014} run inside a directory at or below one holding .syns.yaml"
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
            cause: "every file is larger than 25 MiB".into(),
        };
        let pe_text = pe.to_string();
        assert!(pe_text.starts_with("nothing to push from /tmp/x"));
        assert!(pe_text.contains("source contained 3 files but all were excluded"));
        assert!(pe_text.contains("every file is larger than 25 MiB"));
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
                    reason: SkipReason::TooLarge,
                },
                SkippedFile {
                    path: "dist/bundle.js".into(),
                    reason: SkipReason::DefaultExcludeDir,
                },
            ],
            no_default_excludes: false,
        };
        let s = pp.to_string();
        // SPEC u280: the headline counts the size drops alone; the block
        // beneath it names every drop.
        let headline_pos = s.find("push aborted: 1 file(s)").expect("headline missing");
        let warning_pos = s
            .find("warning: 2 file(s) skipped")
            .expect("breakdown missing");
        assert!(
            headline_pos < warning_pos,
            "headline must come BEFORE per-category breakdown (SPEC § 7); got: {s}"
        );
        assert!(s.contains("larger than 25 MiB (1): logo.png"));
        assert!(s.contains("default-excluded directory (1): dist/bundle.js"));
        // strict hint absent (PushPartial implies strict=true).
        assert!(!s.contains("pass --strict to fail the push"));
        // no line or hint names binary content.
        assert!(!s.contains("binary"));
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
                reason: SkipReason::TooLarge,
            }],
            no_default_excludes: false,
        };
        let v = pp.json_value().expect("PushPartial has a json_value");
        assert_eq!(v["error"], "push_partial");
        let arr = v["skipped"].as_array().unwrap();
        assert_eq!(arr.len(), 1);
        assert_eq!(arr[0]["path"], "logo.png");
        assert_eq!(arr[0]["reason"], "too_large");
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
