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
    /// SPEC u333, the not-shared refusal (`D-124`, Q-01): a collaborator
    /// command run inside a folder bound to its holder whose own identity
    /// file records no `shared_as`, refused before any credential read or
    /// request under the holder-acting refusal's code. `command`, `holder`
    /// and `dir` as `HolderActing` carries them.
    FolderNotShared {
        command: String,
        holder: String,
        dir: std::path::PathBuf,
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
    /// Issue 160 (D-250 §2): `syns edit`'s `--old` matching no content.
    /// It renders as the configuration refusal it was, `message` after
    /// `configuration error: `, and its document carries `no_match`.
    NoMatch {
        message: String,
    },
    /// Issue 160: `syns edit`'s `--old` matching `count` times with no
    /// `--replace-all`, rendered as `NoMatch` is; its document carries
    /// `many_matches` and the count under `count`.
    ManyMatches {
        count: usize,
        message: String,
    },
    /// Issue 160: a pattern or glob of `syns grep` or `syns glob` that
    /// does not compile, rendered as `NoMatch` is, code `bad_pattern`.
    BadPattern {
        message: String,
    },
    /// Issue 160: a `syns commit` changeset document that does not parse
    /// or whose entries its keys do not admit, rendered as `NoMatch` is,
    /// code `invalid_changeset`.
    InvalidChangeset {
        message: String,
    },
    /// Issue 160: the checkout guard (`CHECKOUT_UNPUBLISHED_WORK`) — the
    /// checkout at `root` holds unpublished local work for `repo_id`.
    /// Its line is `crate::write::checkout_guard_refusal`'s, code
    /// `checkout_dirty`, exit `1`.
    CheckoutDirty {
        root: std::path::PathBuf,
        repo_id: String,
    },
    /// Issue 160: a path the run named that the tree it addressed does
    /// not hold, found without a `404` — `syns rm PATH` whose one
    /// deletion changed no file. `line` names the path; code `not_found`.
    PathNotFound {
        line: String,
    },
    /// Roadmap 228 (D-253 §2, §9): a `moves` or `copies` member whose
    /// destination already stands at the parent — the server's `409`
    /// `destination_exists`. `line` names the path, the parent and the
    /// way out; code `path_exists`, exit `1`.
    PathExists {
        line: String,
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
    /// Roadmap 292 (D-257 §7): the edit of a `syns commit` changeset at
    /// this index in `edits`, over a parent content that is not text or
    /// a `new` that would leave none.
    ChangesetEdit(usize),
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
    /// from another. Issue 374: it renders after the wire form and
    /// ` \u{2014} ` where it is not blank, so the line names the rule that
    /// refused the write; a blank one renders as a refusal carrying no
    /// context renders.
    Refusal {
        message: String,
    },
    /// SPEC u334 `ApiErrorContext::NextMove`: the move a caller makes
    /// next, rendered after the bare `server error ({status}): {error}`
    /// line and ` \u{2014} ` under any status (Q-01, appended rather than
    /// replacing the line).
    NextMove {
        line: String,
    },
    /// Issue 199 (D-132): the non-blank string `message` a `403`
    /// `forbidden` served, rendered after the wire form and ` \u{2014} `
    /// so a ceiling `CTRL-per-owner-quota` holds reads apart from a
    /// missing grant. A command's `NextMove` takes its place.
    Reason {
        message: String,
    },
    /// Issue 253 (D-142): the string `reason` a `409` `conflict` carrying
    /// no `currentSha` served — `empty_source` where a fork's caller reads
    /// no version of the source — so a command can tell that refusal from
    /// a name already taken. It renders as a refusal carrying no context.
    ConflictReason {
        reason: String,
    },
    /// Issue 160: the string `message` a `404` served, kept so a read
    /// verb can tell a repository that stands nowhere (or that the caller
    /// may not read) from a version or a path it lacks. It renders as a
    /// refusal carrying no context renders.
    NotFoundMessage {
        message: String,
    },
    /// Issue 160 (D-250 §2): the refusal line of a `404` that named the
    /// repository — absent and unreadable alike — rendered in place of
    /// the registered line, code `repo_not_found`.
    RepoNotFound {
        line: String,
    },
    /// Issue 160: the version-not-found refusal line a read chose for a
    /// `404` `not_found` answering a reference the caller typed, rendered
    /// in place of the registered line, code `version_not_found`.
    VersionNotFound {
        line: String,
    },
    /// Roadmap 228 (D-253 §2): a push refused as `source_not_found`
    /// (`404`) or `destination_exists` (`409`, no `currentSha`), keeping
    /// the served `reason` and the `path` the request named. It renders as
    /// a refusal carrying no context renders until a write verb reads it.
    PathRefusal {
        reason: String,
        path: String,
    },
}

/// The repository-not-found refusal (issue 160): the repository the run
/// addressed, whether it stands nowhere or the caller may not read it.
pub fn repo_not_found_refusal(repo_id: &str) -> String {
    format!("repository not found: {repo_id}")
}

/// Whether `error` is a wire code the server spells: lower snake case,
/// led by a letter.
fn is_wire_code(error: &str) -> bool {
    error.starts_with(|c: char| c.is_ascii_lowercase())
        && error
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_')
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
            CliError::FolderNotShared { .. } => 2,
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
            // Issue 160 (D-250 §2): the count stands in a key of its own.
            CliError::ManyMatches { count, .. } => Some(serde_json::json!({
                "error": self.to_string(),
                "count": count,
            })),
            _ => None,
        }
    }

    /// The stable lower snake_case code a machine-readable refusal
    /// document carries beside `error` (issue 160, D-250 §2): one value
    /// per cause, held across releases. A server refusal carrying no
    /// context the binary chose takes the wire code it served.
    pub fn code(&self) -> String {
        let fixed = match self {
            CliError::Api {
                status,
                error,
                context,
            } => return api_code(*status, error, context.as_ref()),
            CliError::AuthRequired => "auth_required",
            CliError::RepoIdentityUnknown { .. } => "repo_identity_unknown",
            CliError::PathBelongsToAnotherRepository { .. } => "path_belongs_to_another_repo",
            CliError::HolderActing { .. } => "holder_root_required",
            CliError::FolderVisibilityRedirect { .. } => "folder_visibility_redirect",
            CliError::FolderNotShared { .. } => "folder_not_shared",
            CliError::FolderMoved { .. } => "folder_out_of_place",
            CliError::FolderInAnotherCheckout { .. } => "folder_in_another_checkout",
            CliError::ServerUnreachable { .. } => "server_unreachable",
            CliError::TlsRefused { .. } => "tls_refused",
            CliError::Io { .. } => "io_error",
            CliError::Config { .. } => "config_error",
            CliError::Upgrade(e) => e.wire_form(),
            CliError::PayloadTooLarge { .. } => "payload_too_large",
            CliError::PushEmpty { .. } => "push_empty",
            CliError::PushPartial { .. } => "push_partial",
            CliError::CollectedSetChanged { .. } => "collected_set_changed",
            CliError::SyncRefusal { document, .. } => {
                return ["code", "outcome"]
                    .iter()
                    .find_map(|key| document.get(*key).and_then(|v| v.as_str()))
                    .filter(|value| is_wire_code(value))
                    .unwrap_or("sync_refused")
                    .to_string();
            }
            CliError::PartialAnswer { .. } => "partial_answer",
            CliError::NotText { .. } => "content_not_text",
            CliError::WriteConflict { .. } => "write_conflict",
            CliError::FolderWriteUnsupported { .. } => "folder_write_unsupported",
            CliError::FolderVisibilityUnsupported { .. } => "folder_visibility_unsupported",
            CliError::ChangesetEmpty => "changeset_empty",
            CliError::LeftOut { .. } => "left_out",
            CliError::FileTooLarge { .. } => "file_too_large",
            CliError::NoMatch { .. } => "no_match",
            CliError::ManyMatches { .. } => "many_matches",
            CliError::BadPattern { .. } => "bad_pattern",
            CliError::InvalidChangeset { .. } => "invalid_changeset",
            CliError::CheckoutDirty { .. } => "checkout_dirty",
            CliError::PathNotFound { .. } => "not_found",
            CliError::PathExists { .. } => "path_exists",
        };
        fixed.to_string()
    }

    /// The one document machine-readable mode writes for this refusal:
    /// the shape `json_value` binds, or `{"error": <line>}`, with `code`
    /// beside `error` wherever the document does not already carry one
    /// (issue 160, D-250 §2).
    pub fn json_document(&self) -> serde_json::Value {
        let mut document = self
            .json_value()
            .unwrap_or_else(|| serde_json::json!({"error": self.to_string()}));
        if let Some(map) = document.as_object_mut()
            && !map.contains_key("code")
        {
            map.insert("code".to_string(), serde_json::Value::from(self.code()));
        }
        document
    }

    /// Whether this is a `404` naming the repository itself rather than
    /// a version or a path in it (issue 160): the engine's preserved
    /// `repo_not_found`, or the server's `not_found` served with the
    /// message `Repository not found`, which an absent repository and one
    /// the caller may not read answer alike.
    pub fn names_missing_repository(&self) -> bool {
        match self {
            CliError::Api {
                status: Some(404),
                error,
                context,
            } => {
                error == "repo_not_found"
                    || matches!(
                        context,
                        Some(ApiErrorContext::NotFoundMessage { message })
                            if message.eq_ignore_ascii_case("repository not found")
                    )
                    || matches!(context, Some(ApiErrorContext::RepoNotFound { .. }))
            }
            _ => false,
        }
    }

    /// Whether this is a `404` naming a reference the request sent rather
    /// than the repository or a path (issue 160, REVIEW-syns-cli M2): the
    /// engine's `ref_not_found`, or the server's `not_found` served with
    /// `Version not found` or `Ref not found`.
    pub fn names_missing_version(&self) -> bool {
        match self {
            CliError::Api {
                status: Some(404),
                error,
                context,
            } => {
                error == "ref_not_found"
                    || matches!(
                        context,
                        Some(ApiErrorContext::NotFoundMessage { message })
                            if message.eq_ignore_ascii_case("version not found")
                                || message.eq_ignore_ascii_case("ref not found")
                    )
            }
            _ => false,
        }
    }

    /// Puts the repository-not-found line on a `404` (issue 160); every
    /// other variant and status passes through unchanged.
    pub fn with_repo_not_found_context(self, line: String) -> CliError {
        match self {
            CliError::Api {
                status: Some(404),
                error,
                ..
            } => CliError::Api {
                status: Some(404),
                error,
                context: Some(ApiErrorContext::RepoNotFound { line }),
            },
            other => other,
        }
    }

    /// The repository-not-found refusal for `repo_id` where this `404`
    /// names the repository, and this refusal unchanged otherwise.
    pub fn repo_or(self, repo_id: &str) -> CliError {
        if self.names_missing_repository() {
            self.with_repo_not_found_context(repo_not_found_refusal(repo_id))
        } else {
            self
        }
    }

    /// Puts a version-not-found line on an `Api` error, so a `404`
    /// `not_found` renders it in place of the registered line (issue
    /// 160: as `with_versioned_read_context`, under `version_not_found`).
    pub fn with_version_not_found_context(self, line: String) -> CliError {
        match self {
            CliError::Api { status, error, .. } => CliError::Api {
                status,
                error,
                context: Some(ApiErrorContext::VersionNotFound { line }),
            },
            other => other,
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

    /// SPEC u334 `CliError::with_next_move`: puts `NextMove` on an `Api`
    /// refusal carrying a status; every other variant, and a network
    /// error carrying none, passes through unchanged.
    pub fn with_next_move(self, line: String) -> CliError {
        match self {
            CliError::Api {
                status: Some(status),
                error,
                ..
            } => CliError::Api {
                status: Some(status),
                error,
                context: Some(ApiErrorContext::NextMove { line }),
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

/// `CliError::code` of a server refusal (issue 160): the cause a read
/// verb chose where it chose one, the served wire code otherwise.
fn api_code(status: Option<u16>, error: &str, context: Option<&ApiErrorContext>) -> String {
    let Some(status) = status else {
        return "network_error".to_string();
    };
    let chosen = match (status, error, context) {
        (404, _, Some(ApiErrorContext::RepoNotFound { .. })) => Some("repo_not_found"),
        (404, "not_found", Some(ApiErrorContext::VersionNotFound { .. })) => {
            Some("version_not_found")
        }
        (
            404,
            "not_found",
            Some(
                ApiErrorContext::CatPath { .. }
                | ApiErrorContext::LsPath { .. }
                | ApiErrorContext::VersionedRead { .. },
            ),
        ) => Some("not_found"),
        _ => None,
    };
    if let Some(code) = chosen {
        return code.to_string();
    }
    if is_wire_code(error) {
        return error.to_string();
    }
    if status == 200 && error.starts_with("invalid response body") {
        return "response_undecodable".to_string();
    }
    "unknown_refusal".to_string()
}

impl std::fmt::Display for CliError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CliError::Api {
                status: Some(404),
                context: Some(ApiErrorContext::RepoNotFound { line }),
                ..
            } => write!(f, "{line}"),
            CliError::Api {
                status: Some(404),
                error,
                context: Some(ApiErrorContext::VersionNotFound { line }),
            } if error == "not_found" => write!(f, "{line}"),
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
                context: Some(ApiErrorContext::NextMove { line }),
            } => write!(f, "server error ({s}): {error} \u{2014} {line}"),
            CliError::Api {
                status: Some(s),
                error,
                context: Some(ApiErrorContext::Reason { message }),
            } => write!(f, "server error ({s}): {error} \u{2014} {message}"),
            // Issue 374: a validation refusal names the rule its served
            // message gave.
            CliError::Api {
                status: Some(s),
                error,
                context: Some(ApiErrorContext::Refusal { message }),
            } if !message.trim().is_empty() => {
                write!(f, "server error ({s}): {error} \u{2014} {}", message.trim())
            }
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
            CliError::FolderNotShared {
                command,
                holder,
                dir,
            } => write!(
                f,
                "holder root required: {command} acts on a shared folder's own people, and the folder {} of {holder} is not shared \u{2014} share it with: syns share .",
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
            CliError::NotText {
                path,
                surface: NotTextSurface::ChangesetEdit(index),
            } => write!(
                f,
                "edit {index} ({path}): cannot write content that is not text \u{2014} an edit changes text alone; replace the file whole under files with contentBase64"
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
            CliError::NoMatch { message }
            | CliError::ManyMatches { message, .. }
            | CliError::BadPattern { message }
            | CliError::InvalidChangeset { message } => {
                write!(f, "configuration error: {message}")
            }
            CliError::CheckoutDirty { root, repo_id } => {
                write!(f, "{}", crate::write::checkout_guard_refusal(root, repo_id))
            }
            CliError::PathNotFound { line } => write!(f, "{line}"),
            CliError::PathExists { line } => write!(f, "{line}"),
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
        // Issue 160: `code` beside `error`, and nothing else.
        assert_eq!(doc.as_object().map(|m| m.len()), Some(2));
        assert_eq!(doc["code"], serde_json::json!("changeset_empty"));
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
        assert_eq!(doc.as_object().map(|m| m.len()), Some(2));
        assert_eq!(doc["error"], serde_json::json!(err.to_string()));
        assert_eq!(doc["code"], serde_json::json!("file_too_large"));
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

    // SPEC u333 Contract Surface, the not-shared refusal line (Q-01):
    // the line whole, at exit `2`, under the generic error document.
    #[test]
    fn the_not_shared_refusal_renders_its_line_whole() {
        let refusal = CliError::FolderNotShared {
            command: "syns collaborators".into(),
            holder: "alice/docs".into(),
            dir: std::path::PathBuf::from("/w/budget"),
        };
        assert_eq!(
            refusal.to_string(),
            "holder root required: syns collaborators acts on a shared folder's own people, and the folder /w/budget of alice/docs is not shared \u{2014} share it with: syns share ."
        );
        assert_eq!(refusal.exit_code(), 2);
        assert!(refusal.json_value().is_none());
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

    // SPEC u334 Contract Surface, `ApiErrorContext::NextMove`: the move
    // stands after the bare line and ` \u{2014} `, the same string the
    // JSON document's `error` carries.
    #[test]
    fn a_next_move_renders_after_the_wire_form() {
        let moved = CliError::Api {
            status: Some(409),
            error: "conflict".into(),
            context: None,
        }
        .with_next_move("invitation 'inv1' is no longer pending; see the ones that are with: syns teams invitations".into());
        assert_eq!(
            moved.to_string(),
            "server error (409): conflict \u{2014} invitation 'inv1' is no longer pending; see the ones that are with: syns teams invitations"
        );
        assert!(moved.json_value().is_none());
        assert_eq!(moved.exit_code(), 1);
        let network = CliError::Api {
            status: None,
            error: "connection refused".into(),
            context: None,
        }
        .with_next_move("x".into());
        assert!(matches!(
            network,
            CliError::Api {
                status: None,
                context: None,
                ..
            }
        ));
        assert!(matches!(
            CliError::AuthRequired.with_next_move("x".into()),
            CliError::AuthRequired
        ));
    }

    // SPEC u329 Contract Surface, `ApiErrorContext::Refusal`, as issue 374
    // changes it: a refusal holding its served message renders it after
    // the wire form, and one holding a blank message as one holding no
    // context; the exit code and the code are the bare refusal's.
    #[test]
    fn a_kept_refusal_message_renders_after_the_wire_form() {
        let bare = CliError::Api {
            status: Some(422),
            error: "validation_error".into(),
            context: None,
        };
        let refusal = |message: &str| CliError::Api {
            status: Some(422),
            error: "validation_error".into(),
            context: Some(ApiErrorContext::Refusal {
                message: message.into(),
            }),
        };
        let kept = refusal(" description is too long\n");
        assert_eq!(
            kept.to_string(),
            "server error (422): validation_error \u{2014} description is too long"
        );
        assert_eq!(kept.exit_code(), bare.exit_code());
        assert_eq!(kept.code(), bare.code());
        for blank in ["", " \t\n"] {
            assert_eq!(refusal(blank).to_string(), bare.to_string(), "{blank:?}");
        }
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

    fn api(status: u16, error: &str, context: Option<ApiErrorContext>) -> CliError {
        CliError::Api {
            status: Some(status),
            error: error.to_string(),
            context,
        }
    }

    // Issue 160 (D-250 §2): the values Q-0171 names, each its cause's,
    // carried beside `error` in the one document `--json` writes.
    #[test]
    fn every_named_refusal_carries_its_fixed_code_beside_error() {
        let cases: Vec<(CliError, &str)> = vec![
            (
                api(404, "not_found", None).with_cat_path_context("a.md".into()),
                "not_found",
            ),
            (
                api(404, "not_found", None).with_ls_path_context("docs".into()),
                "not_found",
            ),
            (
                api(404, "not_found", None)
                    .with_versioned_read_context("path not found at version 3: a.md".into()),
                "not_found",
            ),
            (
                CliError::PathNotFound {
                    line: "path not found at version 21: gone.md".into(),
                },
                "not_found",
            ),
            (
                api(404, "not_found", None)
                    .with_version_not_found_context("version not found: 9999".into()),
                "version_not_found",
            ),
            (api(404, "not_found", None).repo_or("alice/x"), "not_found"),
            (
                api(
                    404,
                    "not_found",
                    Some(ApiErrorContext::NotFoundMessage {
                        message: "Repository not found".into(),
                    }),
                )
                .repo_or("alice/x"),
                "repo_not_found",
            ),
            (
                api(404, "repo_not_found", None).repo_or("alice/x"),
                "repo_not_found",
            ),
            (
                CliError::CheckoutDirty {
                    root: "/w".into(),
                    repo_id: "alice/x".into(),
                },
                "checkout_dirty",
            ),
            (CliError::AuthRequired, "auth_required"),
            (
                CliError::NoMatch {
                    message: "m".into(),
                },
                "no_match",
            ),
            (
                CliError::ManyMatches {
                    count: 3,
                    message: "m".into(),
                },
                "many_matches",
            ),
            (
                CliError::BadPattern {
                    message: "m".into(),
                },
                "bad_pattern",
            ),
            (
                CliError::InvalidChangeset {
                    message: "m".into(),
                },
                "invalid_changeset",
            ),
            (
                CliError::WriteConflict {
                    parent: "a".repeat(40),
                    current_sha: "c".repeat(40),
                },
                "write_conflict",
            ),
        ];
        for (err, code) in cases {
            let rendered = crate::output::Output::new(true).format_error(&err);
            let doc: serde_json::Value = serde_json::from_str(&rendered).unwrap();
            assert_eq!(doc["code"], serde_json::json!(code), "{err:?}");
            assert_eq!(doc["error"], serde_json::json!(err.to_string()), "{err:?}");
        }
    }

    // Issue 160: the three not-found causes answer three lines of their
    // own, the repository's naming the repository.
    #[test]
    fn the_three_not_found_causes_answer_lines_of_their_own() {
        let path = api(404, "not_found", None).with_cat_path_context("a.md".into());
        let version = api(404, "not_found", None)
            .with_version_not_found_context("version not found: 9999".into());
        let repo = api(
            404,
            "not_found",
            Some(ApiErrorContext::NotFoundMessage {
                message: "Repository not found".into(),
            }),
        )
        .repo_or("alice/x");
        assert_eq!(path.to_string(), "file not found: a.md");
        assert_eq!(version.to_string(), "version not found: 9999");
        assert_eq!(repo.to_string(), "repository not found: alice/x");
        // A `404` whose message names no repository is not one.
        assert!(
            !api(
                404,
                "not_found",
                Some(ApiErrorContext::NotFoundMessage {
                    message: "Version not found".into(),
                }),
            )
            .names_missing_repository()
        );
        // The served message renders as no context renders.
        assert_eq!(
            api(
                404,
                "not_found",
                Some(ApiErrorContext::NotFoundMessage {
                    message: "Team not found".into(),
                }),
            )
            .to_string(),
            "server error (404): not_found"
        );
    }

    // Issue 160: the many-matches count stands in a key of its own, and
    // the split configuration refusals keep their lines byte for byte.
    #[test]
    fn many_matches_carries_its_count_and_the_split_refusals_keep_their_lines() {
        let err = CliError::ManyMatches {
            count: 4,
            message:
                "--old matched 4 times in a.md; pass --replace-all to replace every occurrence"
                    .into(),
        };
        let doc = err.json_document();
        assert_eq!(doc["count"], serde_json::json!(4));
        assert_eq!(doc["code"], serde_json::json!("many_matches"));
        assert_eq!(
            err.to_string(),
            "configuration error: --old matched 4 times in a.md; pass --replace-all to replace every occurrence"
        );
        for err in [
            CliError::NoMatch {
                message: "x".into(),
            },
            CliError::BadPattern {
                message: "x".into(),
            },
            CliError::InvalidChangeset {
                message: "x".into(),
            },
        ] {
            assert_eq!(err.to_string(), "configuration error: x");
            assert_eq!(err.exit_code(), 1);
        }
        let guard = CliError::CheckoutDirty {
            root: "/w".into(),
            repo_id: "alice/x".into(),
        };
        assert_eq!(
            guard.to_string(),
            crate::write::checkout_guard_refusal(std::path::Path::new("/w"), "alice/x")
        );
        assert_eq!(guard.exit_code(), 1);
    }

    // Issue 160 (D-250 §2): a structured document keeps every key and
    // adds `code`; a document already carrying one keeps it.
    #[test]
    fn structured_documents_keep_their_keys_and_add_code() {
        let empty = CliError::PushEmpty {
            path: "/p".into(),
            total_walked: 2,
            cause: "c".into(),
        }
        .json_document();
        assert_eq!(empty["error"], serde_json::json!("push_empty"));
        assert_eq!(empty["code"], serde_json::json!("push_empty"));
        assert_eq!(empty["totalWalked"], serde_json::json!(2));
        let large = CliError::PayloadTooLarge {
            bytes_sent: 1,
            file_count: 1,
            rejecter: EdgeRejecter::Server,
        }
        .json_document();
        assert_eq!(large["code"], large["error"]);
        let partial = CliError::PartialAnswer {
            document: serde_json::json!({"version": 3, "error": "partial answer: x"}),
            line: "partial answer: x".into(),
        }
        .json_document();
        assert_eq!(partial["code"], serde_json::json!("partial_answer"));
        assert_eq!(partial["version"], serde_json::json!(3));
        let sync = CliError::SyncRefusal {
            document: serde_json::json!({"outcome": "resolution_required"}),
            line: "l".into(),
            exit: 4,
        }
        .json_document();
        assert_eq!(sync["code"], serde_json::json!("resolution_required"));
        let carried = CliError::SyncRefusal {
            document: serde_json::json!({"outcome": "retryable_failure", "code": "server_unreachable"}),
            line: "l".into(),
            exit: 3,
        }
        .json_document();
        assert_eq!(carried["code"], serde_json::json!("server_unreachable"));
    }

    // Issue 160: a server refusal the binary chose no cause for carries
    // the wire code it served; one naming none carries `unknown_refusal`.
    #[test]
    fn a_server_refusal_carries_its_wire_code() {
        assert_eq!(api(403, "forbidden", None).code(), "forbidden");
        assert_eq!(
            api(422, "validation_error", None).code(),
            "validation_error"
        );
        assert_eq!(api(429, "rate_limited", None).code(), "rate_limited");
        assert_eq!(api(502, "unknown error", None).code(), "unknown_refusal");
        assert_eq!(
            api(200, "invalid response body: x", None).code(),
            "response_undecodable"
        );
        let network = CliError::Api {
            status: None,
            error: "boom".into(),
            context: None,
        };
        assert_eq!(network.code(), "network_error");
        assert_eq!(
            CliError::ServerUnreachable { url: "u".into() }.code(),
            "server_unreachable"
        );
    }
}
