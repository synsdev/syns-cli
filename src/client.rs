#![allow(dead_code)] // Types and methods used by downstream units (U10, U11, U21+)

use reqwest::redirect::Policy;
use serde::{Deserialize, Serialize};

use crate::errors::{ApiErrorContext, CliError, EdgeRejecter};

// --- Domain Enums ---

#[derive(Deserialize, Serialize, Debug, Clone, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum RepoStatus {
    Active,
    Draft,
    Completed,
    Abandoned,
    #[serde(other)]
    Unknown,
}

impl RepoStatus {
    pub fn as_query_str(&self) -> &'static str {
        match self {
            RepoStatus::Active => "active",
            RepoStatus::Draft => "draft",
            RepoStatus::Completed => "completed",
            RepoStatus::Abandoned => "abandoned",
            RepoStatus::Unknown => "unknown",
        }
    }
}

#[derive(Deserialize, Serialize, Debug, Clone, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum Visibility {
    Public,
    Private,
    #[serde(other)]
    Unknown,
}

#[derive(Deserialize, Serialize, Debug, Clone, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum EntryType {
    File,
    Dir,
    #[serde(other)]
    Unknown,
}

#[derive(Deserialize, Serialize, Debug, Clone, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum DiffStatus {
    Added,
    Modified,
    Deleted,
    #[serde(other)]
    Unknown,
}

#[derive(Deserialize, Serialize, Debug, Clone, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum CollaboratorRole {
    Owner,
    Admin,
    Write,
    Read,
    #[serde(other)]
    Unknown,
}

#[derive(Deserialize, Serialize, Debug, Clone, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum TeamRole {
    Owner,
    Admin,
    Member,
    #[serde(other)]
    Unknown,
}

#[derive(Deserialize, Serialize, Debug, Clone, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum InvitationStatus {
    Pending,
    Accepted,
    Declined,
    #[serde(other)]
    Unknown,
}

// --- Request Types ---

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PushRequest {
    pub files: Vec<PushFileEntry>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub deletions: Option<Vec<PushDeleteEntry>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub author: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parent_sha: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tags: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub status: Option<RepoStatus>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub visibility: Option<Visibility>,
    /// What the publication asserts about where it came from; absent
    /// where nothing is asserted.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub provenance: Option<PushProvenance>,
}

/// The provenance block a publication asserts: all three required
/// fields or no block at all, the task reference left out rather than
/// sent as null.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct PushProvenance {
    pub integration: String,
    pub run: String,
    pub trigger: String,
    #[serde(rename = "taskRef", skip_serializing_if = "Option::is_none", default)]
    pub task_ref: Option<String>,
}

/// A commit's recorded provenance, as the version list and the file
/// history answer it.
#[derive(Deserialize, Serialize, Debug, Clone, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct CommitProvenance {
    pub publisher: String,
    #[serde(default)]
    pub integration: Option<String>,
    #[serde(default)]
    pub run: Option<String>,
    #[serde(default)]
    pub trigger: Option<String>,
    #[serde(default)]
    pub task_ref: Option<String>,
}

/// One file a publication names: its hash alone, or its bytes beside it —
/// `content` where they are text, `content_base64` (standard padded
/// base64 of the bytes) where they are not, never both (SPEC u280,
/// `D-088`).
#[derive(Serialize, Clone, Debug)]
#[serde(rename_all = "camelCase")]
pub struct PushFileEntry {
    pub path: String,
    pub sha: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub content: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub content_base64: Option<String>,
}

#[derive(Serialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct PushDeleteEntry {
    pub path: String,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ForkRequest {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RepoUpdate {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub status: Option<RepoStatus>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub visibility: Option<Visibility>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub author: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tags: Option<Vec<String>>,
}

#[derive(Serialize, Debug)]
#[serde(untagged)]
pub enum AddCollaboratorRequest {
    Username {
        username: String,
        role: CollaboratorRole,
    },
    Email {
        email: String,
        role: CollaboratorRole,
    },
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct UpdateCollaboratorRoleRequest {
    pub role: CollaboratorRole,
}

/// `EP-create-repo`'s body (SPEC u272 Contract Surface, `create_repo`):
/// a field the caller left unset is left out rather than sent as null,
/// so the entry's own default stands.
#[derive(Serialize, Debug)]
#[serde(rename_all = "camelCase")]
pub struct CreateRepoRequest {
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub visibility: Option<Visibility>,
}

/// `EP-create-user-link`'s body.
#[derive(Serialize, Debug)]
#[serde(rename_all = "camelCase")]
pub struct CreateUserLinkRequest {
    pub kind: String,
    pub value: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
}

/// `EP-update-user-link`'s body: at least one member present, an
/// omitted one leaving what the entry holds rather than clearing it.
#[derive(Serialize, Debug)]
#[serde(rename_all = "camelCase")]
pub struct UpdateUserLinkRequest {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub kind: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub value: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sort_order: Option<u32>,
}

/// `EP-reorder-user-links`' body.
#[derive(Serialize, Debug)]
#[serde(rename_all = "camelCase")]
pub struct ReorderUserLinksRequest {
    pub order: Vec<String>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RevertFileRequest {
    pub to: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
}

// --- Response Types ---

#[derive(Deserialize, Serialize, Debug)]
#[serde(rename_all = "camelCase")]
pub struct PushResponse {
    pub commit_sha: String,
    pub version: u32,
    pub files_changed: u32,
    pub created: bool,
}

#[derive(Deserialize, Debug)]
#[serde(rename_all = "camelCase")]
pub struct TreeResponse {
    pub entries: Vec<TreeEntry>,
    pub commit_sha: String,
    pub truncated: bool,
}

pub type PullResponse = TreeResponse;

#[derive(Deserialize, Debug)]
pub struct TreeEntry {
    pub name: String,
    pub path: String,
    #[serde(rename = "type")]
    pub entry_type: EntryType,
    pub size: Option<u64>,
    pub sha: Option<String>,
}

/// `EP-file-read`'s answer: `content` is `None` exactly where the answer
/// carried `content: null`, a content that is not text (SPEC u280,
/// `D-088`).
#[derive(Deserialize, Debug)]
pub struct FileResponse {
    pub content: Option<String>,
    pub sha: String,
    pub size: u64,
}

/// A raw read's answer: the body exactly as it arrived, and the `ETag`
/// with its quotes removed.
#[derive(Debug)]
pub struct RawFile {
    pub bytes: Vec<u8>,
    pub etag: Option<String>,
}

/// A raw read staged into a file: the blob hash of the bytes written, and
/// the `ETag` as `RawFile` takes it.
#[derive(Debug)]
pub struct StagedRaw {
    pub sha: String,
    pub etag: Option<String>,
}

fn etag_of(response: &reqwest::Response) -> Option<String> {
    response
        .headers()
        .get(reqwest::header::ETAG)
        .and_then(|v| v.to_str().ok())
        .map(|v| {
            v.trim()
                .trim_start_matches("W/")
                .trim_matches('"')
                .to_string()
        })
}

#[derive(Deserialize, Debug)]
#[serde(rename_all = "camelCase")]
pub struct FileHistoryResponse {
    pub data: Vec<FileVersionEntry>,
    #[serde(default)]
    pub total: u32,
    #[serde(default)]
    pub limit: u32,
    #[serde(default)]
    pub offset: u32,
}

#[derive(Deserialize, Debug)]
#[serde(rename_all = "camelCase")]
pub struct FileVersionEntry {
    pub version: u32,
    pub sha: String,
    /// `None` on the entry of a commit that removed the path.
    pub blob_sha: Option<String>,
    pub message: String,
    pub author: String,
    pub created_at: String,
    /// `None` on the entry of a commit that removed the path.
    pub content: Option<String>,
    pub diff: Option<String>,
    #[serde(default)]
    pub provenance: Option<CommitProvenance>,
}

#[derive(Deserialize, Serialize, Debug, Clone)]
#[serde(rename_all = "camelCase")]
pub struct ForkedFrom {
    pub owner: String,
    pub name: String,
}

#[derive(Deserialize, Serialize, Debug)]
#[serde(rename_all = "camelCase")]
pub struct RepoResponse {
    pub owner: String,
    pub name: String,
    pub description: Option<String>,
    pub commit_sha: Option<String>,
    pub status: RepoStatus,
    pub author: Option<String>,
    pub tags: Vec<String>,
    pub visibility: Visibility,
    pub forked_from: Option<ForkedFrom>,
    pub fork_count: u32,
    pub file_count: u32,
    pub role: Option<CollaboratorRole>,
    pub created_at: String,
    pub updated_at: String,
}

#[derive(Deserialize, Serialize, Debug)]
#[serde(rename_all = "camelCase")]
pub struct RepoListResponse {
    pub data: Vec<RepoResponse>,
    pub total: u32,
    pub limit: u32,
    pub offset: u32,
}

#[derive(Deserialize, Debug)]
#[serde(rename_all = "camelCase")]
pub struct VersionListResponse {
    pub data: Vec<VersionEntry>,
    pub total: u32,
    pub limit: u32,
    pub offset: u32,
}

#[derive(Deserialize, Debug)]
#[serde(rename_all = "camelCase")]
pub struct VersionEntry {
    pub version: u32,
    pub sha: String,
    /// `None` on the initial commit (`INV-26`). The version listing this
    /// model also decodes carries the key too; `default` keeps a body
    /// serving neither key decoding as it did before u272.
    #[serde(default)]
    pub parent_sha: Option<String>,
    pub message: String,
    /// Everything past the caption's first line, `None` where the commit
    /// carried nothing but a caption.
    #[serde(default)]
    pub message_body: Option<String>,
    pub author: String,
    pub created_at: String,
    pub files_changed: Vec<String>,
    #[serde(default)]
    pub provenance: Option<CommitProvenance>,
}

#[derive(Deserialize, Debug)]
#[serde(rename_all = "camelCase")]
pub struct DiffEndpoint {
    pub version: u32,
    pub sha: String,
}

#[derive(Deserialize, Debug)]
#[serde(rename_all = "camelCase")]
pub struct DiffResponse {
    pub from: DiffEndpoint,
    pub to: DiffEndpoint,
    pub files: Vec<DiffEntry>,
}

#[derive(Deserialize, Debug)]
#[serde(rename_all = "camelCase")]
pub struct DiffEntry {
    pub path: String,
    pub status: DiffStatus,
    pub diff: Option<String>,
}

#[derive(Deserialize, Debug)]
#[serde(rename_all = "camelCase")]
pub struct CollaboratorUser {
    pub id: String,
    pub name: String,
    pub username: String,
    pub email: String,
}

#[derive(Deserialize, Debug)]
#[serde(rename_all = "camelCase")]
pub struct CollaboratorListResponse {
    pub data: Vec<Collaborator>,
    #[serde(default)]
    pub total: u32,
    #[serde(default)]
    pub limit: u32,
    #[serde(default)]
    pub offset: u32,
}

#[derive(Deserialize, Debug)]
#[serde(rename_all = "camelCase")]
pub struct Collaborator {
    pub user: CollaboratorUser,
    pub role: CollaboratorRole,
    pub added_by: Option<String>,
    pub created_at: String,
}

#[derive(Deserialize, Debug)]
#[serde(rename_all = "camelCase")]
pub struct ExploreResponse {
    pub data: Vec<RepoResponse>,
    pub total: u32,
    pub limit: u32,
    pub offset: u32,
}

/// Fork response is the full Repository object.
pub type ForkResponse = RepoResponse;

// --- Team Helper Structs ---

#[derive(Deserialize, Debug, Clone)]
#[serde(rename_all = "camelCase")]
pub struct UserSummary {
    pub id: String,
    pub username: String,
    pub name: String,
    pub email: Option<String>,
    pub image: Option<String>,
}

/// One link on a profile (`UserLink`). `kind` is left as the string the
/// entry served rather than folded into the value set the argument
/// parser admits, so a kind the boundary registers after this build
/// renders rather than refusing the whole answer.
#[derive(Deserialize, Serialize, Debug, Clone)]
#[serde(rename_all = "camelCase")]
pub struct UserLink {
    pub id: String,
    pub kind: String,
    pub value: String,
    pub label: Option<String>,
    pub sort_order: u32,
    pub created_at: String,
    pub updated_at: String,
}

/// A person's public profile (`UserProfile`). `repo_count` is what the
/// entry counted for the caller that asked, which differs by viewer.
#[derive(Deserialize, Serialize, Debug, Clone)]
#[serde(rename_all = "camelCase")]
pub struct UserProfile {
    pub id: String,
    pub username: String,
    pub name: String,
    pub image: Option<String>,
    pub bio: Option<String>,
    pub location: Option<String>,
    pub pronouns: Option<String>,
    pub company: Option<String>,
    pub time_zone: Option<String>,
    pub links: Vec<UserLink>,
    pub created_at: String,
    pub repo_count: u32,
}

/// `EP-users-search`'s answer: the `Collection<UserSummary>` envelope
/// the entry serves, never a bare vector — the bare form refuses the
/// served body outright (`PROTOTYPE.md` Constraints).
#[derive(Deserialize, Debug)]
pub struct UserSearchResponse {
    pub data: Vec<UserSummary>,
}

/// What all four link entries answer: the caller's whole ordered list
/// under `links`, never a bare vector (`PROTOTYPE.md` Constraints).
#[derive(Deserialize, Debug)]
pub struct UserLinksResponse {
    pub links: Vec<UserLink>,
}

#[derive(Deserialize, Debug, Clone)]
#[serde(rename_all = "camelCase")]
pub struct TeamSummary {
    pub id: String,
    pub name: String,
    pub description: Option<String>,
}

// --- Team Response Types ---

#[derive(Deserialize, Debug)]
#[serde(rename_all = "camelCase")]
pub struct TeamResponse {
    pub id: String,
    pub name: String,
    pub description: Option<String>,
    pub owner: UserSummary,
    pub member_count: u32,
    pub role: TeamRole,
    pub created_at: String,
    pub updated_at: String,
}

#[derive(Deserialize, Debug)]
pub struct TeamListResponse {
    pub data: Vec<TeamResponse>,
}

#[derive(Deserialize, Debug)]
#[serde(rename_all = "camelCase")]
pub struct TeamMemberResponse {
    pub user: UserSummary,
    pub role: TeamRole,
    pub joined_at: String,
}

#[derive(Deserialize, Debug)]
pub struct TeamMembersResponse {
    pub data: Vec<TeamMemberResponse>,
}

#[derive(Deserialize, Debug)]
#[serde(rename_all = "camelCase")]
pub struct InvitationResponse {
    pub id: String,
    pub team: TeamSummary,
    pub email: String,
    pub role: TeamRole,
    pub invited_by: Option<UserSummary>,
    pub status: InvitationStatus,
    pub expires_at: String,
    pub created_at: String,
}

#[derive(Deserialize, Debug)]
pub struct InvitationListResponse {
    pub data: Vec<InvitationResponse>,
}

#[derive(Deserialize, Debug)]
#[serde(rename_all = "camelCase")]
pub struct TeamRepoResponse {
    pub owner: String,
    pub name: String,
    pub description: Option<String>,
    pub visibility: Visibility,
    pub role: CollaboratorRole,
    pub added_by: Option<UserSummary>,
    pub added_at: String,
}

#[derive(Deserialize, Debug)]
pub struct TeamReposResponse {
    pub data: Vec<TeamRepoResponse>,
}

// --- Team Request Types ---

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CreateTeamRequest {
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct UpdateTeamRequest {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<Option<String>>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct InviteRequest {
    pub email: String,
    pub role: TeamRole,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ChangeRoleRequest {
    pub role: TeamRole,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TeamRepoAccessRequest {
    pub role: CollaboratorRole,
}

#[derive(Deserialize, Debug)]
pub struct SessionResponse {
    pub user: SessionUser,
}

#[derive(Deserialize, Debug)]
pub struct SessionUser {
    pub id: String,
    pub name: String,
    pub username: String,
    pub email: String,
    pub image: Option<String>,
}

#[derive(Deserialize, Serialize, Debug)]
#[serde(rename_all = "camelCase")]
pub struct RevertResponse {
    pub commit_sha: String,
    pub version: u32,
    pub files_changed: u32,
    pub created: bool,
}

// --- Private Helpers ---

#[derive(Deserialize)]
struct ApiErrorBody {
    error: String,
}

#[derive(Deserialize)]
struct AddCollaboratorErrorBody {
    error: String,
    #[serde(default)]
    reason: Option<String>,
}

fn encode_path_segments(path: &str) -> String {
    path.split('/')
        .filter(|segment| !segment.is_empty())
        .map(|segment| urlencoding::encode(segment))
        .collect::<Vec<_>>()
        .join("/")
}

// --- The bounded transport (SPEC u280, `D-089`, `D-090`) ---

/// The `User-Agent` every request the binary sends to `BND-public-api`
/// carries, naming the running build (`D-089`).
pub const USER_AGENT: &str = concat!("syns/", env!("CARGO_PKG_VERSION"));

/// How long an answer's body may go with no byte of it arriving.
pub const ANSWER_STALL: std::time::Duration = std::time::Duration::from_secs(30);

/// The longest any request lives, `OPS-res-server-runtime`'s ceiling.
pub const REQUEST_CEILING: std::time::Duration = std::time::Duration::from_secs(300);

/// How long a request waits for its answer's head: 30 seconds, and one
/// more per 256 KiB of the body it sends (`D-089`).
pub fn request_deadline(body_len: usize) -> std::time::Duration {
    let millis = 30_000u64.saturating_add((body_len as u64).saturating_mul(1_000) / 262_144);
    std::time::Duration::from_millis(millis)
}

/// The one client builder every client of `BND-public-api` is built by:
/// the build's `User-Agent`, no redirect followed, and `REQUEST_CEILING`
/// as its only client-wide bound.
pub(crate) fn api_client() -> Result<reqwest::Client, CliError> {
    reqwest::Client::builder()
        .user_agent(USER_AGENT)
        .timeout(REQUEST_CEILING)
        .redirect(Policy::none())
        .build()
        .map_err(|e| CliError::Config {
            message: e.to_string(),
        })
}

fn unreachable_at(url: &str) -> CliError {
    CliError::ServerUnreachable {
        url: url.to_string(),
    }
}

/// Send `request`, awaiting its answer's head within the deadline a body
/// of `body_len` bytes earns, the client's ceiling bounding the rest.
pub(crate) async fn send_bounded(
    request: reqwest::RequestBuilder,
    body_len: usize,
) -> Result<reqwest::Response, CliError> {
    let (client, request) = request.build_split();
    let request = request?;
    let url = request.url().to_string();
    match tokio::time::timeout(request_deadline(body_len), client.execute(request)).await {
        Err(_elapsed) => Err(unreachable_at(&url)),
        Ok(Err(err)) if err.is_builder() => Err(CliError::from(err)),
        Ok(Err(_transport)) => Err(unreachable_at(&url)),
        Ok(Ok(response)) => Ok(response),
    }
}

/// Read an answer's body chunk by chunk, each chunk arriving within
/// `stall` of the head or of the chunk before it; a stalled, failed or
/// short body ends as `SERVER_UNREACHABLE`. A given `capacity` allocates
/// the one buffer the body is read into before its first chunk is
/// awaited (SPEC u280 `read_body`, `D-094`); `None` sizes it from the
/// declared length, up to 1 MiB.
pub(crate) async fn read_body(
    mut response: reqwest::Response,
    stall: std::time::Duration,
    capacity: Option<usize>,
) -> Result<Vec<u8>, CliError> {
    let url = response.url().to_string();
    let declared = response.content_length();
    let mut body =
        Vec::with_capacity(capacity.unwrap_or_else(|| declared.unwrap_or(0).min(1 << 20) as usize));
    loop {
        match tokio::time::timeout(stall, response.chunk()).await {
            Err(_elapsed) => return Err(unreachable_at(&url)),
            Ok(Err(_transport)) => return Err(unreachable_at(&url)),
            Ok(Ok(None)) => break,
            Ok(Ok(Some(chunk))) => body.extend_from_slice(&chunk),
        }
    }
    if declared.is_some_and(|declared| declared != body.len() as u64) {
        return Err(unreachable_at(&url));
    }
    Ok(body)
}

/// Write an answer's body to `dest` as it arrives, one chunk held at a
/// time, under `read_body`'s stall and short-body refusal, answering the
/// blob hash of the bytes written.
pub(crate) async fn read_body_to(
    mut response: reqwest::Response,
    stall: std::time::Duration,
    dest: &mut std::fs::File,
) -> Result<String, CliError> {
    use sha1::{Digest, Sha1};
    use std::io::Write;
    let url = response.url().to_string();
    let declared = response.content_length();
    let write_failed = |err: std::io::Error| CliError::Io {
        message: format!("could not write a staged answer for {url}: {err}"),
    };
    let mut hasher = declared.map(|len| {
        let mut hasher = Sha1::new();
        hasher.update(format!("blob {len}\0").as_bytes());
        hasher
    });
    let mut written = 0u64;
    loop {
        match tokio::time::timeout(stall, response.chunk()).await {
            Err(_elapsed) => return Err(unreachable_at(&url)),
            Ok(Err(_transport)) => return Err(unreachable_at(&url)),
            Ok(Ok(None)) => break,
            Ok(Ok(Some(chunk))) => {
                written += chunk.len() as u64;
                if let Some(hasher) = hasher.as_mut() {
                    hasher.update(&chunk);
                }
                dest.write_all(&chunk).map_err(write_failed)?;
            }
        }
    }
    if declared.is_some_and(|declared| declared != written) {
        return Err(unreachable_at(&url));
    }
    dest.flush().map_err(write_failed)?;
    match hasher {
        Some(hasher) => Ok(hex_digest(&hasher.finalize())),
        // No declared length: hash what was written, once it all stands.
        None => {
            use std::io::{Read, Seek, SeekFrom};
            dest.seek(SeekFrom::Start(0)).map_err(write_failed)?;
            let mut hasher = Sha1::new();
            hasher.update(format!("blob {written}\0").as_bytes());
            let mut buf = vec![0u8; 64 * 1024];
            loop {
                let n = dest.read(&mut buf).map_err(write_failed)?;
                if n == 0 {
                    break;
                }
                hasher.update(&buf[..n]);
            }
            Ok(hex_digest(&hasher.finalize()))
        }
    }
}

fn hex_digest(digest: &[u8]) -> String {
    digest.iter().fold(String::new(), |mut acc, b| {
        use std::fmt::Write;
        let _ = write!(acc, "{b:02x}");
        acc
    })
}

/// A body serialised once, the bytes the request sends.
fn json_body<T: Serialize + ?Sized>(body: &T) -> Result<Option<Vec<u8>>, CliError> {
    serde_json::to_vec(body)
        .map(Some)
        .map_err(|e| CliError::Io {
            message: format!("could not serialise a request body: {e}"),
        })
}

/// Sending a request through `send_bounded`, with a JSON body serialised
/// once or with none.
pub(crate) trait BoundedSend {
    async fn send_json_bounded<T: Serialize + ?Sized>(
        self,
        body: &T,
    ) -> Result<reqwest::Response, CliError>;
    async fn send_empty_bounded(self) -> Result<reqwest::Response, CliError>;
}

impl BoundedSend for reqwest::RequestBuilder {
    async fn send_json_bounded<T: Serialize + ?Sized>(
        self,
        body: &T,
    ) -> Result<reqwest::Response, CliError> {
        let body = json_body(body)?.unwrap_or_default();
        let len = body.len();
        send_bounded(
            self.header(reqwest::header::CONTENT_TYPE, "application/json")
                .body(body),
            len,
        )
        .await
    }

    async fn send_empty_bounded(self) -> Result<reqwest::Response, CliError> {
        send_bounded(self, 0).await
    }
}

/// The mismatch refusal: a raw answer whose bytes are not the ones named
/// (SPEC u280 Contract Surface, `D-089`).
pub(crate) fn hash_mismatch(path: &str, expected: &str, actual: &str) -> CliError {
    CliError::Api {
        status: Some(200),
        error: format!("invalid response body: {path}: expected {expected}, got {actual}"),
        context: None,
    }
}

async fn check_response(response: reqwest::Response) -> Result<reqwest::Response, CliError> {
    let status = response.status();
    if status == reqwest::StatusCode::UNAUTHORIZED {
        return Err(CliError::AuthRequired);
    }
    if status == reqwest::StatusCode::PAYLOAD_TOO_LARGE {
        // A stalled or short body is the server unreachable, as it is on
        // every other answer (CR1-1).
        let bytes = match read_body(response, ANSWER_STALL, None).await {
            Ok(bytes) => bytes,
            Err(err @ CliError::ServerUnreachable { .. }) => return Err(err),
            Err(_) => Vec::new(),
        };
        let prefix = &bytes[..bytes.len().min(4096)];
        let lower = String::from_utf8_lossy(prefix).to_ascii_lowercase();
        let rejecter = if lower.contains("cloudflare") {
            EdgeRejecter::Cloudflare
        } else if serde_json::from_slice::<ApiErrorBody>(prefix)
            .map(|b| b.error == "payload_too_large")
            .unwrap_or(false)
        {
            EdgeRejecter::Server
        } else {
            EdgeRejecter::CloudRunOrFrontend
        };
        // CONTRACT: bytes_sent and file_count are placeholder zeros here —
        // check_response is on the response side of the wire and cannot
        // see the outbound request. They are transit-state values: the
        // push chunker (push::smart::chunked_push) rewrites both fields
        // with the offending batch's actual metrics on both the n==1
        // short-circuit and the n>1 loop paths before the variant
        // escapes Phase 5. For non-push callers (no large body exists
        // in today's stack) the placeholders propagate verbatim; the
        // Display arm in errors.rs renders "attempted 0 file(s),
        // ~0.0 MiB", which is the documented diagnostic for that
        // unlikely path.
        return Err(CliError::PayloadTooLarge {
            bytes_sent: 0,
            file_count: 0,
            rejecter,
        });
    }
    if status.is_client_error() || status.is_server_error() {
        let code = status.as_u16();
        // SPEC u271: the moved head's hash is carried out of the answer
        // rather than reduced to its presence — the conflict refusal
        // renders it and its document carries it, and nothing downstream
        // can read the body again once this fold has consumed it.
        let body = match read_body(response, ANSWER_STALL, None).await {
            Ok(bytes) => Some(bytes),
            Err(err @ CliError::ServerUnreachable { .. }) => return Err(err),
            Err(_) => None,
        };
        let parsed = body
            .as_deref()
            .and_then(|bytes| serde_json::from_slice::<serde_json::Value>(bytes).ok());
        let error = match parsed
            .as_ref()
            .map(|body| serde_json::from_value::<ApiErrorBody>(body.clone()))
        {
            Some(Ok(known)) => known.error,
            _ => "unknown error".to_string(),
        };
        let context = match (code, error.as_str(), &parsed) {
            (409, "conflict", Some(body)) => {
                body.get("currentSha")
                    .and_then(|v| v.as_str())
                    .map(|current_sha| ApiErrorContext::HeadMoved {
                        current_sha: current_sha.to_string(),
                    })
            }
            // SPEC u280 `ApiErrorContext::MissingBlobs`: the `missing`
            // map crosses whole, and only where it is a map of path to
            // hash.
            (409, "missing_blobs", Some(body)) => body
                .get("missing")
                .and_then(|v| v.as_object())
                .and_then(|map| {
                    map.iter()
                        .map(|(path, sha)| Some((path.clone(), sha.as_str()?.to_string())))
                        .collect::<Option<std::collections::BTreeMap<String, String>>>()
                })
                .map(|missing| ApiErrorContext::MissingBlobs { missing }),
            _ => None,
        };
        return Err(CliError::Api {
            status: Some(code),
            error,
            context,
        });
    }
    if !status.is_success() {
        return Err(CliError::Api {
            status: Some(status.as_u16()),
            error: format!("unexpected status {}", status.as_u16()),
            context: None,
        });
    }
    Ok(response)
}

fn undecodable(status: reqwest::StatusCode, e: impl std::fmt::Display) -> CliError {
    CliError::Api {
        status: Some(status.as_u16()),
        error: format!("invalid response body: {e}"),
        context: None,
    }
}

async fn process_response<T: serde::de::DeserializeOwned>(
    response: reqwest::Response,
) -> Result<T, CliError> {
    let response = check_response(response).await?;
    let status = response.status();
    let bytes = read_body(response, ANSWER_STALL, None).await?;
    serde_json::from_slice::<T>(&bytes).map_err(|e| undecodable(status, e))
}

/// Reads the response body once, parses it to `serde_json::Value`, and additionally
/// materializes the typed `T`. Returns `(T, serde_json::Value)`.
///
/// Used by every `SynsClient::*` method whose response is emitted under `--json`,
/// so the CLI can pass the server's byte sequence through verbatim (modulo
/// pretty-print whitespace and key order) while still feeding the typed
/// representation to comfy-table renders, manifest writes, and success banners.
///
/// `Value::clone` is a deep clone, but its cost is dwarfed by the network round-trip
/// for typical CLI payloads; the response body cannot be read twice because
/// reading it consumes the response.
async fn process_response_raw<T: serde::de::DeserializeOwned>(
    response: reqwest::Response,
) -> Result<(T, serde_json::Value), CliError> {
    let response = check_response(response).await?;
    let status = response.status();
    let bytes = read_body(response, ANSWER_STALL, None).await?;
    let value: serde_json::Value =
        serde_json::from_slice(&bytes).map_err(|e| undecodable(status, e))?;
    let typed: T = serde_json::from_value(value.clone()).map_err(|e| undecodable(status, e))?;
    Ok((typed, value))
}

async fn process_empty_response(response: reqwest::Response) -> Result<(), CliError> {
    let response = check_response(response).await?;
    read_body(response, ANSWER_STALL, None).await?;
    Ok(())
}

/// The bodies `units/cli/u272/prototype/bodies/` captured from the
/// deployment and from the origin this run stood up on a scratch
/// database, one literal per model the added entries answer.
#[cfg(test)]
pub(crate) mod u272_bodies {
    pub const SEARCH_BART: &str = r##"{"data":[{"id":"eyXhkCLfkB0dZKGhsqF4rJxVYbXGl0Kp","username":"bartosz-sojka","name":"Bartosz Sójka","image":"https://lh3.googleusercontent.com/a/ACg8ocL4FWyOjIFB0ypTBmxLTIltsOv8F1GiGee3cNQaBRdX-Y6pBQk=s96-c"}]}"##;
    pub const SEARCH_LOCAL: &str = r##"{"data":[{"id":"u272user1111111111111111111111111","username":"u272bob","name":"U272 Bob","image":null}]}"##;
    pub const PROFILE_SELF: &str = r##"{"id":"6A2PNdiQIutnYqnn3macdtuqgxrg7Yvv","username":"bartsoj","name":"Bartosz","image":"https://avatars.githubusercontent.com/u/42448881?v=4","bio":"By the end of 2027, software will be written entirely by AI agents. Software builders will be needed more than ever. The key is the specification. With the right context and enough iterations, any software can be built. This is no longer a model limitation. It is an engineering problem.","location":"Amsterdam","pronouns":null,"company":"JetBrains","timeZone":null,"links":[{"id":"6efbd0a9-e09d-459d-9277-98838c854b5b","kind":"linkedin","value":"https://www.linkedin.com/in/bartsoj/","label":null,"sortOrder":0,"createdAt":"2026-05-26T11:29:05.737Z","updatedAt":"2026-09-21T04:41:06.239Z"},{"id":"21474465-4502-4150-adf0-aa0b6a0cc4d5","kind":"github","value":"https://github.com/BartSoj","label":null,"sortOrder":1,"createdAt":"2026-05-26T11:28:51.635Z","updatedAt":"2026-09-21T04:41:06.252Z"}],"createdAt":"2026-05-02T16:05:18.608Z","repoCount":49}"##;
    pub const PROFILE_LOCAL: &str = r##"{"id":"u272user1111111111111111111111111","username":"u272bob","name":"U272 Bob","image":null,"bio":null,"location":null,"pronouns":null,"company":null,"timeZone":null,"links":[],"createdAt":"2026-09-21T04:43:40.637Z","repoCount":1}"##;
    pub const LINKS_CREATE: &str = r##"{"links":[{"id":"6efbd0a9-e09d-459d-9277-98838c854b5b","kind":"linkedin","value":"https://www.linkedin.com/in/bartsoj/","label":null,"sortOrder":0,"createdAt":"2026-05-26T11:29:05.737Z","updatedAt":"2026-05-26T11:29:33.124Z"},{"id":"21474465-4502-4150-adf0-aa0b6a0cc4d5","kind":"github","value":"https://github.com/BartSoj","label":null,"sortOrder":1,"createdAt":"2026-05-26T11:28:51.635Z","updatedAt":"2026-05-26T11:29:40.643Z"},{"id":"c3927ea2-a953-498b-8e83-86c870887758","kind":"generic","value":"https://u272.example.test/probe","label":"u272 probe","sortOrder":2,"createdAt":"2026-09-21T04:41:05.688Z","updatedAt":"2026-09-21T04:41:05.688Z"}]}"##;
    pub const LINKS_UPDATE: &str = r##"{"links":[{"id":"6efbd0a9-e09d-459d-9277-98838c854b5b","kind":"linkedin","value":"https://www.linkedin.com/in/bartsoj/","label":null,"sortOrder":0,"createdAt":"2026-05-26T11:29:05.737Z","updatedAt":"2026-05-26T11:29:33.124Z"},{"id":"21474465-4502-4150-adf0-aa0b6a0cc4d5","kind":"github","value":"https://github.com/BartSoj","label":null,"sortOrder":1,"createdAt":"2026-05-26T11:28:51.635Z","updatedAt":"2026-05-26T11:29:40.643Z"},{"id":"c3927ea2-a953-498b-8e83-86c870887758","kind":"generic","value":"https://u272.example.test/probe","label":"u272 probe updated","sortOrder":2,"createdAt":"2026-09-21T04:41:05.688Z","updatedAt":"2026-09-21T04:41:05.854Z"}]}"##;
    pub const LINKS_REORDER: &str = r##"{"links":[{"id":"c3927ea2-a953-498b-8e83-86c870887758","kind":"generic","value":"https://u272.example.test/probe","label":"u272 probe updated","sortOrder":0,"createdAt":"2026-09-21T04:41:05.688Z","updatedAt":"2026-09-21T04:41:06.038Z"},{"id":"21474465-4502-4150-adf0-aa0b6a0cc4d5","kind":"github","value":"https://github.com/BartSoj","label":null,"sortOrder":1,"createdAt":"2026-05-26T11:28:51.635Z","updatedAt":"2026-09-21T04:41:06.050Z"},{"id":"6efbd0a9-e09d-459d-9277-98838c854b5b","kind":"linkedin","value":"https://www.linkedin.com/in/bartsoj/","label":null,"sortOrder":2,"createdAt":"2026-05-26T11:29:05.737Z","updatedAt":"2026-09-21T04:41:06.062Z"}]}"##;
    pub const LINKS_DELETE: &str = r##"{"links":[{"id":"6efbd0a9-e09d-459d-9277-98838c854b5b","kind":"linkedin","value":"https://www.linkedin.com/in/bartsoj/","label":null,"sortOrder":0,"createdAt":"2026-05-26T11:29:05.737Z","updatedAt":"2026-09-21T04:41:06.239Z"},{"id":"21474465-4502-4150-adf0-aa0b6a0cc4d5","kind":"github","value":"https://github.com/BartSoj","label":null,"sortOrder":1,"createdAt":"2026-05-26T11:28:51.635Z","updatedAt":"2026-09-21T04:41:06.252Z"}]}"##;
    pub const VERSION_HEAD: &str = r##"{"version":596,"sha":"7618bcff37fa4dc19f976c9e29019d56d6aeed68","parentSha":"686ee7156c28aca8f7d9411c3f1a50631257d59d","message":"claude code session","messageBody":null,"author":"bartsoj","createdAt":"2026-09-20T13:55:04Z","filesChanged":["decisions/D-080-partial-and-not-text-reads/DECISION.md","decisions/_NEXT_DECISION_ID","issues/144-cli-short-aliases-absent-from-clap-tree/ISSUE.md","issues/148-ref-by-content-hash-resolved-by-scanning-every-commit/ISSUE.md","issues/149-versions-paging-cost-rises-with-offset/ISSUE.md","issues/_NEXT_TRIGGER_ID","roadmap/028-cli-time-travel-reads/ROADMAP.md","roadmap/213-cli-repository-reads/ROADMAP.md","roadmap/214-cli-grep-file-type-filter-and-multiline/ROADMAP.md","roadmap/_NEXT_TRIGGER_ID","units/_NEXT_UNIT_ID","units/_PHASES/PH-decide","units/cli/u270/PROTOTYPE.md","units/cli/u270/SPEC.md","units/cli/u270/SPEC_REVIEW.md","units/cli/u270/SPEC_REVIEW_R2.md","units/cli/u270/_PHASES/PH-pickup"],"provenance":{"publisher":"bartsoj","integration":null,"run":null,"trigger":null,"taskRef":null}}"##;
    pub const FORKS_LOCAL: &str = r##"{"data":[{"owner":"u272bob","name":"u272-fork","description":"the fork","commitSha":null,"status":"active","author":null,"tags":[],"visibility":"public","forkedFrom":{"owner":"u272alice","name":"u272-parent"},"forkCount":0,"fileCount":3,"role":null,"createdAt":"2026-09-21T04:43:40.644Z","updatedAt":"2026-09-21T04:43:40.644Z"}],"total":1,"limit":20,"offset":0}"##;
    pub const FORKS_EMPTY: &str = r##"{"data":[],"total":0,"limit":20,"offset":0}"##;
    pub const ROLE_LOCAL: &str = r##"{"user":{"id":"u272user1111111111111111111111111","name":"U272 Bob","username":"u272bob","email":"u272bob@example.test","emailVerified":true,"image":null,"createdAt":"2026-09-21T04:43:40.637Z","updatedAt":"2026-09-21T04:43:40.637Z"},"role":"write","addedBy":"u272alice","createdAt":"2026-09-21T04:43:40.645Z"}"##;
    pub const CREATED_REPO: &str = r##"{"owner":"bartsoj","name":"u272-parity-probe","description":"u272 prototype probe","commitSha":null,"status":"draft","author":null,"tags":[],"visibility":"private","forkedFrom":null,"forkCount":0,"fileCount":0,"role":"owner","createdAt":"2026-09-21T04:42:46.167Z","updatedAt":"2026-09-21T04:42:46.167Z"}"##;
    pub const SEARCH_429: &str = r##"{"error":"rate_limited","message":"Too many requests"}"##;
    pub const COLLABORATORS_LOCAL: &str = r##"{"data":[{"user":{"id":"u272user1111111111111111111111111","name":"U272 Bob","username":"u272bob","email":"u272bob@example.test","emailVerified":true,"image":null,"createdAt":"2026-09-21T04:43:40.637Z","updatedAt":"2026-09-21T04:43:40.637Z"},"role":"read","addedBy":"u272alice","createdAt":"2026-09-21T04:43:40.645Z"}],"total":1,"limit":100,"offset":0}"##;
}

// --- SynsClient ---

#[derive(Debug, Clone)]
pub struct SynsClient {
    client: reqwest::Client,
    base_url: String,
}

impl SynsClient {
    pub fn new(server_url: &str) -> Result<SynsClient, CliError> {
        if !server_url.starts_with("https://") && !crate::config::is_localhost_url(server_url) {
            return Err(CliError::Config {
                message:
                    "HTTPS required for server URL (http://localhost permitted for development)"
                        .to_string(),
            });
        }

        let base_url = server_url.trim_end_matches('/').to_string();

        let client = api_client()?;

        Ok(SynsClient { client, base_url })
    }

    /// One `EP-push` request carrying `body` as it stands (SPEC u280
    /// `push_body`, `D-094`): the one sender of every publication's body,
    /// which `fill_batch` built at its final length.
    pub async fn push_body(
        &self,
        repo_id: &str,
        token: &str,
        body: Vec<u8>,
    ) -> Result<(PushResponse, serde_json::Value), CliError> {
        let url = format!("{}/api/v1/repos/{}/push", self.base_url, repo_id);
        let len = body.len();
        let response = send_bounded(
            self.client
                .put(&url)
                .bearer_auth(token)
                .header(reqwest::header::CONTENT_TYPE, "application/json")
                .body(body),
            len,
        )
        .await?;
        process_response_raw(response).await
    }

    pub async fn pull(&self, repo_id: &str, token: Option<&str>) -> Result<PullResponse, CliError> {
        let url = format!("{}/api/v1/repos/{}/tree", self.base_url, repo_id);
        let mut req = self.client.get(&url).query(&[("recursive", "true")]);
        if let Some(t) = token {
            req = req.bearer_auth(t);
        }
        let response = req.send_empty_bounded().await?;
        process_response(response).await
    }

    #[allow(clippy::too_many_arguments)]
    pub async fn list_repos(
        &self,
        token: Option<&str>,
        q: Option<&str>,
        owner: Option<&str>,
        status: Option<&RepoStatus>,
        visibility: Option<&Visibility>,
        sort: Option<&str>,
        limit: u32,
        offset: u32,
    ) -> Result<RepoListResponse, CliError> {
        let url = format!("{}/api/v1/repos", self.base_url);
        let mut req = self
            .client
            .get(&url)
            .query(&[("limit", limit.to_string()), ("offset", offset.to_string())]);
        if let Some(q_val) = q {
            req = req.query(&[("q", q_val)]);
        }
        if let Some(owner_val) = owner {
            req = req.query(&[("owner", owner_val)]);
        }
        if let Some(status_val) = status {
            req = req.query(&[("status", status_val.as_query_str())]);
        }
        if let Some(visibility_val) = visibility {
            let v = format!("{:?}", visibility_val).to_lowercase();
            req = req.query(&[("visibility", v.as_str())]);
        }
        if let Some(sort_val) = sort {
            req = req.query(&[("sort", sort_val)]);
        }
        if let Some(t) = token {
            req = req.bearer_auth(t);
        }
        let response = req.send_empty_bounded().await?;
        process_response(response).await
    }

    pub async fn get_repo(
        &self,
        repo_id: &str,
        token: Option<&str>,
    ) -> Result<RepoResponse, CliError> {
        let url = format!("{}/api/v1/repos/{}", self.base_url, repo_id);
        let mut req = self.client.get(&url);
        if let Some(t) = token {
            req = req.bearer_auth(t);
        }
        let response = req.send_empty_bounded().await?;
        process_response(response).await
    }

    pub async fn get_tree(
        &self,
        repo_id: &str,
        token: Option<&str>,
        path: Option<&str>,
        recursive: bool,
        version_ref: Option<&str>,
    ) -> Result<(TreeResponse, serde_json::Value), CliError> {
        let url = match path {
            Some(p) => format!(
                "{}/api/v1/repos/{}/tree/{}",
                self.base_url,
                repo_id,
                encode_path_segments(p)
            ),
            None => format!("{}/api/v1/repos/{}/tree", self.base_url, repo_id),
        };
        let mut req = self.client.get(&url);
        if recursive {
            req = req.query(&[("recursive", "true")]);
        }
        if let Some(r) = version_ref {
            req = req.query(&[("ref", r)]);
        }
        if let Some(t) = token {
            req = req.bearer_auth(t);
        }
        let response = req.send_empty_bounded().await?;
        process_response_raw(response).await
    }

    pub async fn get_file(
        &self,
        repo_id: &str,
        token: Option<&str>,
        path: &str,
        version_ref: Option<&str>,
    ) -> Result<(FileResponse, serde_json::Value), CliError> {
        let url = format!(
            "{}/api/v1/repos/{}/files/{}",
            self.base_url,
            repo_id,
            encode_path_segments(path)
        );
        let mut req = self.client.get(&url);
        if let Some(r) = version_ref {
            req = req.query(&[("ref", r)]);
        }
        if let Some(t) = token {
            req = req.bearer_auth(t);
        }
        let response = req.send_empty_bounded().await?;
        process_response_raw(response).await
    }

    fn raw_request(
        &self,
        repo_id: &str,
        token: Option<&str>,
        path: &str,
        version_ref: Option<&str>,
    ) -> reqwest::RequestBuilder {
        let url = format!(
            "{}/api/v1/repos/{}/raw/{}",
            self.base_url,
            repo_id,
            encode_path_segments(path)
        );
        let mut req = self.client.get(&url);
        if let Some(r) = version_ref {
            req = req.query(&[("ref", r)]);
        }
        if let Some(t) = token {
            req = req.bearer_auth(t);
        }
        req
    }

    /// One `GET` of the raw entry (SPEC u280 `get_raw`, `D-088`): the
    /// stored bytes exactly, `ref` sent as `get_file` sends it, a refusal
    /// raised under the code `get_file` raises for the path. A given
    /// `capacity` is the size a retrieval's hold admitted, the buffer the
    /// bytes are read into allocated at it (`D-094`).
    pub async fn get_raw(
        &self,
        repo_id: &str,
        token: Option<&str>,
        path: &str,
        version_ref: Option<&str>,
        capacity: Option<usize>,
    ) -> Result<RawFile, CliError> {
        let response = self
            .raw_request(repo_id, token, path, version_ref)
            .send_empty_bounded()
            .await?;
        let response = check_response(response).await?;
        let etag = etag_of(&response);
        let bytes = read_body(response, ANSWER_STALL, capacity).await?;
        Ok(RawFile { bytes, etag })
    }

    /// The `GET` `get_raw` sends, its body written into a file created at
    /// `dest` as it arrives rather than held.
    pub async fn get_raw_staged(
        &self,
        repo_id: &str,
        token: Option<&str>,
        path: &str,
        version_ref: Option<&str>,
        dest: &std::path::Path,
    ) -> Result<StagedRaw, CliError> {
        let response = self
            .raw_request(repo_id, token, path, version_ref)
            .send_empty_bounded()
            .await?;
        let response = check_response(response).await?;
        let etag = etag_of(&response);
        let mut options = std::fs::OpenOptions::new();
        options.write(true).read(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut file = options.open(dest).map_err(|err| CliError::Io {
            message: format!("could not write {}: {err}", dest.display()),
        })?;
        let sha = read_body_to(response, ANSWER_STALL, &mut file).await?;
        Ok(StagedRaw { sha, etag })
    }

    /// Reads one version of a repository (`EP-get-version`).
    ///
    /// SPEC u270 Contract Surface, `get_version`: the reference is the
    /// address's own last segment and nothing is sent as a query, so the
    /// server's reference form — a decimal ordinal or a hex prefix — is
    /// what decides, and a branch name is refused by the server rather
    /// than by the client (`issues/006`).
    pub async fn get_version(
        &self,
        repo_id: &str,
        token: Option<&str>,
        reference: &str,
    ) -> Result<(VersionEntry, serde_json::Value), CliError> {
        let url = format!(
            "{}/api/v1/repos/{}/versions/{}",
            self.base_url,
            repo_id,
            encode_path_segments(reference)
        );
        let mut req = self.client.get(&url);
        if let Some(t) = token {
            req = req.bearer_auth(t);
        }
        let response = req.send_empty_bounded().await?;
        process_response_raw(response).await
    }

    pub async fn get_file_history(
        &self,
        repo_id: &str,
        token: Option<&str>,
        path: &str,
        limit: u32,
    ) -> Result<(FileHistoryResponse, serde_json::Value), CliError> {
        let url = format!(
            "{}/api/v1/repos/{}/files/{}/history",
            self.base_url,
            repo_id,
            encode_path_segments(path)
        );
        let mut req = self.client.get(&url).query(&[("limit", limit.to_string())]);
        if let Some(t) = token {
            req = req.bearer_auth(t);
        }
        let response = req.send_empty_bounded().await?;
        process_response_raw(response).await
    }

    pub async fn list_versions(
        &self,
        repo_id: &str,
        token: Option<&str>,
        limit: u32,
        offset: u32,
    ) -> Result<(VersionListResponse, serde_json::Value), CliError> {
        let url = format!("{}/api/v1/repos/{}/versions", self.base_url, repo_id);
        let mut req = self
            .client
            .get(&url)
            .query(&[("limit", limit.to_string()), ("offset", offset.to_string())]);
        if let Some(t) = token {
            req = req.bearer_auth(t);
        }
        let response = req.send_empty_bounded().await?;
        process_response_raw(response).await
    }

    pub async fn get_diff(
        &self,
        repo_id: &str,
        token: Option<&str>,
        from: &str,
        to: &str,
    ) -> Result<(DiffResponse, serde_json::Value), CliError> {
        let url = format!("{}/api/v1/repos/{}/diff", self.base_url, repo_id);
        let mut req = self.client.get(&url).query(&[("from", from), ("to", to)]);
        if let Some(t) = token {
            req = req.bearer_auth(t);
        }
        let response = req.send_empty_bounded().await?;
        process_response_raw(response).await
    }

    pub async fn list_collaborators(
        &self,
        repo_id: &str,
        token: Option<&str>,
        limit: u32,
        offset: u32,
    ) -> Result<(CollaboratorListResponse, serde_json::Value), CliError> {
        let url = format!("{}/api/v1/repos/{}/collaborators", self.base_url, repo_id);
        let mut req = self
            .client
            .get(&url)
            .query(&[("limit", limit.to_string()), ("offset", offset.to_string())]);
        if let Some(t) = token {
            req = req.bearer_auth(t);
        }
        let response = req.send_empty_bounded().await?;
        process_response_raw(response).await
    }

    pub async fn add_collaborator(
        &self,
        repo_id: &str,
        token: &str,
        request: &AddCollaboratorRequest,
    ) -> Result<(), CliError> {
        let url = format!("{}/api/v1/repos/{}/collaborators", self.base_url, repo_id);
        let response = self
            .client
            .post(&url)
            .bearer_auth(token)
            .send_json_bounded(request)
            .await?;

        if response.status().as_u16() == 404 {
            // Bespoke 404 carve-out: the server distinguishes between
            // "target user not found" (carries a `reason` discriminator)
            // and "repo not visible" (no `reason`). We surface the
            // `reason` through CliError::Api.error so the command layer
            // can format a target-bearing message.
            let bytes = match read_body(response, ANSWER_STALL, None).await {
                Ok(b) => b,
                Err(err @ CliError::ServerUnreachable { .. }) => return Err(err),
                Err(_) => {
                    return Err(CliError::Api {
                        status: Some(404),
                        error: "unknown error".to_string(),
                        context: None,
                    });
                }
            };
            return match serde_json::from_slice::<AddCollaboratorErrorBody>(&bytes) {
                Ok(body) => Err(CliError::Api {
                    status: Some(404),
                    error: body.reason.unwrap_or(body.error),
                    context: None,
                }),
                Err(_) => Err(CliError::Api {
                    status: Some(404),
                    error: "unknown error".to_string(),
                    context: None,
                }),
            };
        }

        process_empty_response(response).await
    }

    pub async fn remove_collaborator(
        &self,
        repo_id: &str,
        token: &str,
        user_id: &str,
    ) -> Result<(), CliError> {
        let url = format!(
            "{}/api/v1/repos/{}/collaborators/{}",
            self.base_url,
            repo_id,
            urlencoding::encode(user_id)
        );
        let response = self
            .client
            .delete(&url)
            .bearer_auth(token)
            .send_empty_bounded()
            .await?;
        process_empty_response(response).await
    }

    /// Changes one standing grant (`EP-set-collaborator-role`).
    ///
    /// SPEC u272 Contract Surface, `update_collaborator_role`: the entry
    /// answers the collaborator it changed, so the pair carries the
    /// render's typed member beside the body the machine-readable mode
    /// writes — the four user keys `CollaboratorUser` leaves unread
    /// standing in that raw member.
    pub async fn update_collaborator_role(
        &self,
        repo_id: &str,
        token: &str,
        user_id: &str,
        request: &UpdateCollaboratorRoleRequest,
    ) -> Result<(Collaborator, serde_json::Value), CliError> {
        let url = format!(
            "{}/api/v1/repos/{}/collaborators/{}",
            self.base_url,
            repo_id,
            urlencoding::encode(user_id)
        );
        let response = self
            .client
            .patch(&url)
            .bearer_auth(token)
            .send_json_bounded(request)
            .await?;
        process_response_raw(response).await
    }

    pub async fn explore(
        &self,
        query: Option<&str>,
        tag: Option<&str>,
        status: Option<&RepoStatus>,
        limit: u32,
        offset: u32,
    ) -> Result<(ExploreResponse, serde_json::Value), CliError> {
        let url = format!("{}/api/v1/explore", self.base_url);
        let mut req = self
            .client
            .get(&url)
            .query(&[("limit", limit.to_string()), ("offset", offset.to_string())]);
        if let Some(q) = query {
            req = req.query(&[("search", q)]);
        }
        if let Some(t) = tag {
            req = req.query(&[("tag", t)]);
        }
        if let Some(s) = status {
            req = req.query(&[("status", s.as_query_str())]);
        }
        let response = req.send_empty_bounded().await?;
        process_response_raw(response).await
    }

    pub async fn fork(
        &self,
        repo_id: &str,
        token: &str,
        request: &ForkRequest,
    ) -> Result<(ForkResponse, serde_json::Value), CliError> {
        let url = format!("{}/api/v1/repos/{}/fork", self.base_url, repo_id);
        let response = self
            .client
            .post(&url)
            .bearer_auth(token)
            .send_json_bounded(request)
            .await?;
        process_response_raw(response).await
    }

    pub async fn delete_repo(&self, repo_id: &str, token: &str) -> Result<(), CliError> {
        let url = format!("{}/api/v1/repos/{}", self.base_url, repo_id);
        let response = self
            .client
            .delete(&url)
            .bearer_auth(token)
            .send_empty_bounded()
            .await?;
        process_empty_response(response).await
    }

    pub async fn get_session(
        &self,
        token: &str,
    ) -> Result<(SessionResponse, serde_json::Value), CliError> {
        let url = format!("{}/api/auth/get-session", self.base_url);
        let response = self
            .client
            .get(&url)
            .bearer_auth(token)
            .send_empty_bounded()
            .await?;
        let response = check_response(response).await?;
        // Special handling: better-auth returns 200 with null when token is invalid.
        // Every body-read / parse / typed-deserialize failure on this endpoint
        // maps to AuthRequired (not the generic "invalid response body" surface),
        // preserving observable behavior on token-expiry for cmd_login / cmd_whoami.
        let bytes = read_body(response, ANSWER_STALL, None).await?;
        let value: serde_json::Value =
            serde_json::from_slice(&bytes).map_err(|_| CliError::AuthRequired)?;
        let typed: SessionResponse =
            serde_json::from_value(value.clone()).map_err(|_| CliError::AuthRequired)?;
        Ok((typed, value))
    }

    pub async fn update_repo(
        &self,
        repo_id: &str,
        token: &str,
        update: &RepoUpdate,
    ) -> Result<RepoResponse, CliError> {
        let url = format!("{}/api/v1/repos/{}", self.base_url, repo_id);
        let response = self
            .client
            .patch(&url)
            .bearer_auth(token)
            .send_json_bounded(update)
            .await?;
        process_response(response).await
    }

    pub async fn revert_file(
        &self,
        repo_id: &str,
        token: &str,
        path: &str,
        request: &RevertFileRequest,
    ) -> Result<(RevertResponse, serde_json::Value), CliError> {
        let url = format!(
            "{}/api/v1/repos/{}/files/{}/revert",
            self.base_url,
            repo_id,
            encode_path_segments(path)
        );
        let response = self
            .client
            .post(&url)
            .bearer_auth(token)
            .send_json_bounded(request)
            .await?;
        process_response_raw(response).await
    }

    /// Lists a repository's forks (`EP-list-forks`).
    ///
    /// SPEC u272 Contract Surface, `list_forks`: the page window the
    /// caller named reaches the entry unchanged, so the `limit` and
    /// `offset` the answer carries are the ones the invocation asked for.
    pub async fn list_forks(
        &self,
        repo_id: &str,
        token: Option<&str>,
        limit: u32,
        offset: u32,
    ) -> Result<(RepoListResponse, serde_json::Value), CliError> {
        let url = format!("{}/api/v1/repos/{}/forks", self.base_url, repo_id);
        let mut req = self
            .client
            .get(&url)
            .query(&[("limit", limit.to_string()), ("offset", offset.to_string())]);
        if let Some(t) = token {
            req = req.bearer_auth(t);
        }
        let response = req.send_empty_bounded().await?;
        process_response_raw(response).await
    }

    /// Searches people by handle or display name (`EP-users-search`).
    ///
    /// SPEC u272 Contract Surface, `search_users`: the typed member
    /// holds the answer's own order, which no local sort replaces.
    pub async fn search_users(
        &self,
        token: &str,
        q: &str,
        limit: u32,
    ) -> Result<(UserSearchResponse, serde_json::Value), CliError> {
        let url = format!("{}/api/v1/users", self.base_url);
        let response = self
            .client
            .get(&url)
            .query(&[("q", q.to_string()), ("limit", limit.to_string())])
            .bearer_auth(token)
            .send_empty_bounded()
            .await?;
        process_response_raw(response).await
    }

    /// Reads one person's profile (`EP-user-profile`).
    ///
    /// SPEC u272 Contract Surface, `get_user_profile`: the stored
    /// credential rides where one stands and nothing rides where none
    /// does, so the entry decides which repository count it answers.
    pub async fn get_user_profile(
        &self,
        username: &str,
        token: Option<&str>,
    ) -> Result<(UserProfile, serde_json::Value), CliError> {
        let url = format!(
            "{}/api/v1/users/{}",
            self.base_url,
            urlencoding::encode(username)
        );
        let mut req = self.client.get(&url);
        if let Some(t) = token {
            req = req.bearer_auth(t);
        }
        let response = req.send_empty_bounded().await?;
        process_response_raw(response).await
    }

    /// The four profile-link entries behind one function
    /// (`EP-create-user-link`, `EP-update-user-link`,
    /// `EP-delete-user-link`, `EP-reorder-user-links`).
    ///
    /// SPEC u272 Contract Surface, `user_links`: each arm answers the
    /// whole ordered list the entry served.
    pub async fn user_links(
        &self,
        token: &str,
        action: &crate::commands::links::LinksAction,
    ) -> Result<(UserLinksResponse, serde_json::Value), CliError> {
        use crate::commands::links::LinksAction;
        let base = format!("{}/api/v1/me/links", self.base_url);
        let (request, body) = match action {
            LinksAction::Add { kind, value, label } => (
                self.client.post(&base),
                json_body(&CreateUserLinkRequest {
                    kind: kind.as_wire_str().to_string(),
                    value: value.clone(),
                    label: label.clone(),
                })?,
            ),
            LinksAction::Update {
                id,
                kind,
                value,
                label,
                sort_order,
            } => (
                self.client
                    .patch(format!("{}/{}", base, urlencoding::encode(id))),
                json_body(&UpdateUserLinkRequest {
                    kind: kind.map(|k| k.as_wire_str().to_string()),
                    value: value.clone(),
                    label: label.clone(),
                    sort_order: *sort_order,
                })?,
            ),
            LinksAction::Remove { id } => (
                self.client
                    .delete(format!("{}/{}", base, urlencoding::encode(id))),
                None,
            ),
            LinksAction::Reorder { order } => (
                self.client.post(format!("{base}/reorder")),
                json_body(&ReorderUserLinksRequest {
                    order: order.clone(),
                })?,
            ),
        };
        let request = request.bearer_auth(token);
        let response = match body {
            Some(body) => {
                let len = body.len();
                send_bounded(
                    request
                        .header(reqwest::header::CONTENT_TYPE, "application/json")
                        .body(body),
                    len,
                )
                .await?
            }
            None => send_bounded(request, 0).await?,
        };
        process_response_raw(response).await
    }

    /// Creates an empty repository under the caller (`EP-create-repo`).
    ///
    /// SPEC u272 Contract Surface, `create_repo`: only the fields the
    /// caller gave are sent, an omitted one leaving the entry's own
    /// default to stand.
    pub async fn create_repo(
        &self,
        token: &str,
        name: &str,
        description: Option<&str>,
        visibility: Option<&Visibility>,
    ) -> Result<(RepoResponse, serde_json::Value), CliError> {
        let url = format!("{}/api/v1/repos", self.base_url);
        let request = CreateRepoRequest {
            name: name.to_string(),
            description: description.map(str::to_string),
            visibility: visibility.cloned(),
        };
        let response = self
            .client
            .post(&url)
            .bearer_auth(token)
            .send_json_bounded(&request)
            .await?;
        process_response_raw(response).await
    }
}

// --- Team Methods ---

impl SynsClient {
    // Team CRUD

    pub async fn create_team(
        &self,
        token: &str,
        request: &CreateTeamRequest,
    ) -> Result<(TeamResponse, serde_json::Value), CliError> {
        let url = format!("{}/api/v1/teams", self.base_url);
        let response = self
            .client
            .post(&url)
            .bearer_auth(token)
            .send_json_bounded(request)
            .await?;
        process_response_raw(response).await
    }

    pub async fn list_teams(
        &self,
        token: &str,
    ) -> Result<(TeamListResponse, serde_json::Value), CliError> {
        let url = format!("{}/api/v1/teams", self.base_url);
        let response = self
            .client
            .get(&url)
            .bearer_auth(token)
            .send_empty_bounded()
            .await?;
        process_response_raw(response).await
    }

    pub async fn get_team(
        &self,
        token: &str,
        team_id: &str,
    ) -> Result<(TeamResponse, serde_json::Value), CliError> {
        let url = format!("{}/api/v1/teams/{}", self.base_url, team_id);
        let response = self
            .client
            .get(&url)
            .bearer_auth(token)
            .send_empty_bounded()
            .await?;
        process_response_raw(response).await
    }

    pub async fn update_team(
        &self,
        token: &str,
        team_id: &str,
        request: &UpdateTeamRequest,
    ) -> Result<(TeamResponse, serde_json::Value), CliError> {
        let url = format!("{}/api/v1/teams/{}", self.base_url, team_id);
        let response = self
            .client
            .patch(&url)
            .bearer_auth(token)
            .send_json_bounded(request)
            .await?;
        process_response_raw(response).await
    }

    pub async fn delete_team(&self, token: &str, team_id: &str) -> Result<(), CliError> {
        let url = format!("{}/api/v1/teams/{}", self.base_url, team_id);
        let response = self
            .client
            .delete(&url)
            .bearer_auth(token)
            .send_empty_bounded()
            .await?;
        process_empty_response(response).await
    }

    // Member management

    pub async fn list_members(
        &self,
        token: &str,
        team_id: &str,
    ) -> Result<(TeamMembersResponse, serde_json::Value), CliError> {
        let url = format!("{}/api/v1/teams/{}/members", self.base_url, team_id);
        let response = self
            .client
            .get(&url)
            .bearer_auth(token)
            .send_empty_bounded()
            .await?;
        process_response_raw(response).await
    }

    pub async fn invite_member(
        &self,
        token: &str,
        team_id: &str,
        request: &InviteRequest,
    ) -> Result<(InvitationResponse, serde_json::Value), CliError> {
        let url = format!("{}/api/v1/teams/{}/invite", self.base_url, team_id);
        let response = self
            .client
            .post(&url)
            .bearer_auth(token)
            .send_json_bounded(request)
            .await?;
        process_response_raw(response).await
    }

    pub async fn change_role(
        &self,
        token: &str,
        team_id: &str,
        user_id: &str,
        request: &ChangeRoleRequest,
    ) -> Result<(TeamMemberResponse, serde_json::Value), CliError> {
        let url = format!(
            "{}/api/v1/teams/{}/members/{}/role",
            self.base_url, team_id, user_id
        );
        let response = self
            .client
            .post(&url)
            .bearer_auth(token)
            .send_json_bounded(request)
            .await?;
        process_response_raw(response).await
    }

    pub async fn remove_member(
        &self,
        token: &str,
        team_id: &str,
        user_id: &str,
    ) -> Result<(), CliError> {
        let url = format!(
            "{}/api/v1/teams/{}/members/{}",
            self.base_url, team_id, user_id
        );
        let response = self
            .client
            .delete(&url)
            .bearer_auth(token)
            .send_empty_bounded()
            .await?;
        process_empty_response(response).await
    }

    // Invitation flow

    pub async fn list_my_invitations(
        &self,
        token: &str,
    ) -> Result<(InvitationListResponse, serde_json::Value), CliError> {
        let url = format!("{}/api/v1/teams/invitations", self.base_url);
        let response = self
            .client
            .get(&url)
            .bearer_auth(token)
            .send_empty_bounded()
            .await?;
        process_response_raw(response).await
    }

    pub async fn accept_invitation(
        &self,
        token: &str,
        invitation_id: &str,
    ) -> Result<(TeamMemberResponse, serde_json::Value), CliError> {
        let url = format!(
            "{}/api/v1/teams/invitations/{}/accept",
            self.base_url, invitation_id
        );
        let response = self
            .client
            .post(&url)
            .bearer_auth(token)
            .send_empty_bounded()
            .await?;
        process_response_raw(response).await
    }

    pub async fn decline_invitation(
        &self,
        token: &str,
        invitation_id: &str,
    ) -> Result<(), CliError> {
        let url = format!(
            "{}/api/v1/teams/invitations/{}/decline",
            self.base_url, invitation_id
        );
        let response = self
            .client
            .post(&url)
            .bearer_auth(token)
            .send_empty_bounded()
            .await?;
        process_empty_response(response).await
    }

    // Team-repo access

    pub async fn add_team_repo(
        &self,
        token: &str,
        team_id: &str,
        owner: &str,
        name: &str,
        request: &TeamRepoAccessRequest,
    ) -> Result<(TeamRepoResponse, serde_json::Value), CliError> {
        let url = format!(
            "{}/api/v1/teams/{}/repos/{}/{}",
            self.base_url, team_id, owner, name
        );
        let response = self
            .client
            .put(&url)
            .bearer_auth(token)
            .send_json_bounded(request)
            .await?;
        process_response_raw(response).await
    }

    pub async fn remove_team_repo(
        &self,
        token: &str,
        team_id: &str,
        owner: &str,
        name: &str,
    ) -> Result<(), CliError> {
        let url = format!(
            "{}/api/v1/teams/{}/repos/{}/{}",
            self.base_url, team_id, owner, name
        );
        let response = self
            .client
            .delete(&url)
            .bearer_auth(token)
            .send_empty_bounded()
            .await?;
        process_empty_response(response).await
    }

    pub async fn list_team_repos(
        &self,
        token: &str,
        team_id: &str,
    ) -> Result<(TeamReposResponse, serde_json::Value), CliError> {
        let url = format!("{}/api/v1/teams/{}/repos", self.base_url, team_id);
        let response = self
            .client
            .get(&url)
            .bearer_auth(token)
            .send_empty_bounded()
            .await?;
        process_response_raw(response).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use wiremock::matchers::{header, method, path, query_param, query_param_is_missing};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn version_body(version: u32, sha: &str) -> serde_json::Value {
        serde_json::json!({
            "version": version,
            "sha": sha,
            "parentSha": null,
            "message": "m",
            "messageBody": null,
            "author": "alice",
            "createdAt": "2026-01-01T00:00:00Z",
            "filesChanged": ["a.md"],
        })
    }

    // SPEC u270 Contract Surface, `get_version`: "sends the reference as
    // the address's own last segment and nothing as a query".
    #[tokio::test]
    async fn get_version_puts_the_reference_in_the_address_and_sends_no_query() {
        let mock_server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/v1/repos/alice/notes/versions/2"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(version_body(2, &"b".repeat(40))),
            )
            .mount(&mock_server)
            .await;

        let client = SynsClient::new(&mock_server.uri()).unwrap();
        let (typed, raw) = client.get_version("alice/notes", None, "2").await.unwrap();

        assert_eq!(typed.version, 2);
        assert_eq!(typed.sha, "b".repeat(40));
        assert_eq!(raw["sha"], serde_json::json!("b".repeat(40)));

        let received = mock_server.received_requests().await.unwrap();
        assert_eq!(received.len(), 1);
        let url = &received[0].url;
        assert!(
            url.path().ends_with("/versions/2"),
            "expected the reference as the last segment, got {}",
            url.path()
        );
        assert_eq!(url.query(), None, "no query is sent");
    }

    // A full content hash reaches the same address as its own last
    // segment — the one resolution a run makes before it pins an ordinal.
    #[tokio::test]
    async fn get_version_sends_a_content_hash_as_the_last_segment() {
        let mock_server = MockServer::start().await;
        let sha = "b".repeat(40);
        Mock::given(method("GET"))
            .and(path(format!("/api/v1/repos/alice/notes/versions/{sha}")))
            .respond_with(ResponseTemplate::new(200).set_body_json(version_body(2, &sha)))
            .mount(&mock_server)
            .await;

        let client = SynsClient::new(&mock_server.uri()).unwrap();
        let (typed, _raw) = client.get_version("alice/notes", None, &sha).await.unwrap();
        assert_eq!(typed.version, 2);

        let received = mock_server.received_requests().await.unwrap();
        assert_eq!(received.len(), 1);
        assert!(received[0].url.path().ends_with(&sha));
        assert_eq!(received[0].url.query(), None);
    }

    #[test]
    fn new_accepts_https() {
        let client = SynsClient::new("https://syns.dev").unwrap();
        assert_eq!(client.base_url, "https://syns.dev");
    }

    #[test]
    fn new_accepts_localhost_http() {
        let client = SynsClient::new("http://localhost:3000").unwrap();
        assert_eq!(client.base_url, "http://localhost:3000");
    }

    #[test]
    fn new_rejects_plain_http() {
        let err = SynsClient::new("http://example.com").unwrap_err();
        assert!(matches!(err, CliError::Config { ref message } if message.contains("HTTPS")));
    }

    #[test]
    fn new_rejects_localhost_prefix_attack() {
        assert!(SynsClient::new("http://localhost.evil.com").is_err());
        assert!(SynsClient::new("http://localhostevil").is_err());
    }

    #[test]
    fn new_strips_trailing_slash() {
        let client = SynsClient::new("https://syns.dev/").unwrap();
        assert_eq!(client.base_url, "https://syns.dev");
    }

    #[test]
    fn new_strips_multiple_trailing_slashes() {
        let client = SynsClient::new("https://syns.dev///").unwrap();
        assert_eq!(client.base_url, "https://syns.dev");
    }

    #[test]
    fn repo_status_deserializes_known() {
        assert_eq!(
            serde_json::from_str::<RepoStatus>("\"active\"").unwrap(),
            RepoStatus::Active
        );
        assert_eq!(
            serde_json::from_str::<RepoStatus>("\"draft\"").unwrap(),
            RepoStatus::Draft
        );
        assert_eq!(
            serde_json::from_str::<RepoStatus>("\"completed\"").unwrap(),
            RepoStatus::Completed
        );
        assert_eq!(
            serde_json::from_str::<RepoStatus>("\"abandoned\"").unwrap(),
            RepoStatus::Abandoned
        );
    }

    #[test]
    fn repo_status_deserializes_unknown_to_unknown() {
        assert_eq!(
            serde_json::from_str::<RepoStatus>("\"archived\"").unwrap(),
            RepoStatus::Unknown
        );
        assert_eq!(
            serde_json::from_str::<RepoStatus>("\"some_future_status\"").unwrap(),
            RepoStatus::Unknown
        );
    }

    #[test]
    fn repo_status_serializes() {
        assert_eq!(
            serde_json::to_string(&RepoStatus::Active).unwrap(),
            "\"active\""
        );
        assert_eq!(
            serde_json::to_string(&RepoStatus::Draft).unwrap(),
            "\"draft\""
        );
        assert_eq!(
            serde_json::to_string(&RepoStatus::Completed).unwrap(),
            "\"completed\""
        );
        assert_eq!(
            serde_json::to_string(&RepoStatus::Abandoned).unwrap(),
            "\"abandoned\""
        );
        assert_eq!(
            serde_json::to_string(&RepoStatus::Unknown).unwrap(),
            "\"unknown\""
        );
    }

    #[test]
    fn visibility_serializes() {
        assert_eq!(
            serde_json::to_string(&Visibility::Public).unwrap(),
            "\"public\""
        );
        assert_eq!(
            serde_json::to_string(&Visibility::Private).unwrap(),
            "\"private\""
        );
        assert_eq!(
            serde_json::to_string(&Visibility::Unknown).unwrap(),
            "\"unknown\""
        );
    }

    #[test]
    fn other_enums_roundtrip() {
        assert_eq!(
            serde_json::from_str::<Visibility>("\"public\"").unwrap(),
            Visibility::Public
        );
        assert_eq!(
            serde_json::from_str::<Visibility>("\"private\"").unwrap(),
            Visibility::Private
        );
        assert_eq!(
            serde_json::from_str::<Visibility>("\"future\"").unwrap(),
            Visibility::Unknown
        );
        assert_eq!(
            serde_json::from_str::<EntryType>("\"file\"").unwrap(),
            EntryType::File
        );
        assert_eq!(
            serde_json::from_str::<EntryType>("\"dir\"").unwrap(),
            EntryType::Dir
        );
        assert_eq!(
            serde_json::from_str::<EntryType>("\"unknown_type\"").unwrap(),
            EntryType::Unknown
        );
        assert_eq!(
            serde_json::from_str::<DiffStatus>("\"added\"").unwrap(),
            DiffStatus::Added
        );
        assert_eq!(
            serde_json::from_str::<DiffStatus>("\"modified\"").unwrap(),
            DiffStatus::Modified
        );
        assert_eq!(
            serde_json::from_str::<DiffStatus>("\"deleted\"").unwrap(),
            DiffStatus::Deleted
        );
        assert_eq!(
            serde_json::from_str::<CollaboratorRole>("\"owner\"").unwrap(),
            CollaboratorRole::Owner
        );
        assert_eq!(
            serde_json::from_str::<CollaboratorRole>("\"admin\"").unwrap(),
            CollaboratorRole::Admin
        );
        assert_eq!(
            serde_json::from_str::<CollaboratorRole>("\"write\"").unwrap(),
            CollaboratorRole::Write
        );
        assert_eq!(
            serde_json::from_str::<CollaboratorRole>("\"read\"").unwrap(),
            CollaboratorRole::Read
        );
        assert_eq!(
            serde_json::from_str::<CollaboratorRole>("\"superadmin\"").unwrap(),
            CollaboratorRole::Unknown
        );
    }

    #[test]
    fn repo_status_as_query_str() {
        assert_eq!(RepoStatus::Active.as_query_str(), "active");
        assert_eq!(RepoStatus::Draft.as_query_str(), "draft");
        assert_eq!(RepoStatus::Completed.as_query_str(), "completed");
        assert_eq!(RepoStatus::Abandoned.as_query_str(), "abandoned");
        assert_eq!(RepoStatus::Unknown.as_query_str(), "unknown");
    }

    #[test]
    fn encode_path_preserves_slashes() {
        assert_eq!(encode_path_segments("src/main.rs"), "src/main.rs");
        assert_eq!(encode_path_segments("src/my file.rs"), "src/my%20file.rs");
        assert_eq!(
            encode_path_segments("dir/sub dir/file #2.txt"),
            "dir/sub%20dir/file%20%232.txt"
        );
    }

    #[test]
    fn encode_path_filters_empty_segments() {
        assert_eq!(encode_path_segments("/src/main.rs"), "src/main.rs");
        assert_eq!(encode_path_segments("src//main.rs"), "src/main.rs");
        assert_eq!(encode_path_segments("src/main.rs/"), "src/main.rs");
    }

    #[test]
    fn team_response_deserializes_nested_owner() {
        let json = r#"{
            "id": "550e8400-e29b-41d4-a716-446655440000",
            "name": "backend-team",
            "description": null,
            "owner": {
                "id": "user-123",
                "username": "alice",
                "name": "Alice Smith",
                "image": null
            },
            "memberCount": 5,
            "role": "owner",
            "createdAt": "2026-03-18T10:00:00.000Z",
            "updatedAt": "2026-03-18T12:00:00.000Z"
        }"#;

        let response: TeamResponse = serde_json::from_str(json).unwrap();
        assert_eq!(response.id, "550e8400-e29b-41d4-a716-446655440000");
        assert_eq!(response.name, "backend-team");
        assert!(response.description.is_none());
        assert_eq!(response.owner.id, "user-123");
        assert_eq!(response.owner.username, "alice");
        assert_eq!(response.owner.name, "Alice Smith");
        assert!(response.owner.email.is_none());
        assert!(response.owner.image.is_none());
        assert_eq!(response.member_count, 5);
        assert_eq!(response.role, TeamRole::Owner);
        assert_eq!(response.created_at, "2026-03-18T10:00:00.000Z");
        assert_eq!(response.updated_at, "2026-03-18T12:00:00.000Z");
    }

    #[test]
    fn team_request_structs_serialize_to_camel_case() {
        // CreateTeamRequest
        let create = CreateTeamRequest {
            name: "test-team".to_string(),
            description: Some("A test team".to_string()),
        };
        let create_json = serde_json::to_value(&create).unwrap();
        assert_eq!(create_json["name"], "test-team");
        assert_eq!(create_json["description"], "A test team");

        // CreateTeamRequest with no description — field omitted
        let create_no_desc = CreateTeamRequest {
            name: "minimal".to_string(),
            description: None,
        };
        let create_no_desc_json = serde_json::to_value(&create_no_desc).unwrap();
        assert_eq!(create_no_desc_json["name"], "minimal");
        assert!(create_no_desc_json.get("description").is_none());

        // UpdateTeamRequest with name change and description cleared
        let update = UpdateTeamRequest {
            name: Some("renamed".to_string()),
            description: Some(None),
        };
        let update_json = serde_json::to_value(&update).unwrap();
        assert_eq!(update_json["name"], "renamed");
        assert!(update_json["description"].is_null());

        // UpdateTeamRequest with both omitted
        let update_empty = UpdateTeamRequest {
            name: None,
            description: None,
        };
        let update_empty_json = serde_json::to_value(&update_empty).unwrap();
        assert!(update_empty_json.get("name").is_none());
        assert!(update_empty_json.get("description").is_none());
    }

    #[tokio::test]
    async fn list_repos_emits_q_param_not_search_param() {
        let mock_server = MockServer::start().await;

        Mock::given(method("GET"))
            .and(path("/api/v1/repos"))
            .and(query_param("q", "myproject"))
            .and(query_param("limit", "20"))
            .and(query_param("offset", "0"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "data": [],
                "total": 0,
                "limit": 20,
                "offset": 0
            })))
            .mount(&mock_server)
            .await;

        let client = SynsClient::new(&mock_server.uri()).unwrap();
        let result = client
            .list_repos(None, Some("myproject"), None, None, None, None, 20, 0)
            .await;

        assert!(result.is_ok());
        assert_eq!(mock_server.received_requests().await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn list_repos_emits_all_filter_params() {
        let mock_server = MockServer::start().await;

        Mock::given(method("GET"))
            .and(path("/api/v1/repos"))
            .and(query_param("q", "needle"))
            .and(query_param("owner", "alice"))
            .and(query_param("status", "active"))
            .and(query_param("visibility", "public"))
            .and(query_param("sort", "name"))
            .and(query_param("limit", "10"))
            .and(query_param("offset", "20"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "data": [],
                "total": 0,
                "limit": 10,
                "offset": 20
            })))
            .mount(&mock_server)
            .await;

        let client = SynsClient::new(&mock_server.uri()).unwrap();
        let result = client
            .list_repos(
                None,
                Some("needle"),
                Some("alice"),
                Some(&RepoStatus::Active),
                Some(&Visibility::Public),
                Some("name"),
                10,
                20,
            )
            .await;

        assert!(result.is_ok());
        assert_eq!(mock_server.received_requests().await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn list_repos_omits_absent_filters() {
        let mock_server = MockServer::start().await;

        Mock::given(method("GET"))
            .and(path("/api/v1/repos"))
            .and(query_param("limit", "20"))
            .and(query_param("offset", "0"))
            .and(query_param_is_missing("q"))
            .and(query_param_is_missing("owner"))
            .and(query_param_is_missing("status"))
            .and(query_param_is_missing("visibility"))
            .and(query_param_is_missing("sort"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "data": [],
                "total": 0,
                "limit": 20,
                "offset": 0
            })))
            .mount(&mock_server)
            .await;

        let client = SynsClient::new(&mock_server.uri()).unwrap();
        let result = client
            .list_repos(None, None, None, None, None, None, 20, 0)
            .await;

        assert!(result.is_ok());
        assert_eq!(mock_server.received_requests().await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn list_repos_sends_bearer_token_when_provided() {
        let mock_server = MockServer::start().await;

        Mock::given(method("GET"))
            .and(path("/api/v1/repos"))
            .and(header("authorization", "Bearer test-token-abc"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "data": [],
                "total": 0,
                "limit": 20,
                "offset": 0
            })))
            .mount(&mock_server)
            .await;

        let client = SynsClient::new(&mock_server.uri()).unwrap();
        let result = client
            .list_repos(Some("test-token-abc"), None, None, None, None, None, 20, 0)
            .await;

        assert!(result.is_ok());
        assert_eq!(mock_server.received_requests().await.unwrap().len(), 1);
    }

    #[test]
    fn repo_list_response_serializes_verbatim_envelope() {
        // Lock the Serialize behaviour end-to-end: a hand-built RepoListResponse
        // round-trips through serde_json::to_string and matches the canonical
        // wire string byte-for-byte. Guards JSON-mode pass-through in cmd_repos
        // against silent drops of any field (e.g., a future regression that
        // removes Serialize from RepoListResponse or any transitive type).
        let response = RepoListResponse {
            data: vec![RepoResponse {
                owner: "bart".to_string(),
                name: "syns".to_string(),
                description: Some("a repo".to_string()),
                commit_sha: Some("abc123".to_string()),
                status: RepoStatus::Active,
                author: Some("Alice".to_string()),
                tags: vec!["alpha".to_string(), "beta".to_string()],
                visibility: Visibility::Public,
                forked_from: Some(ForkedFrom {
                    owner: "upstream".to_string(),
                    name: "syns".to_string(),
                }),
                fork_count: 2,
                file_count: 436,
                role: Some(CollaboratorRole::Owner),
                created_at: "2025-01-01T00:00:00Z".to_string(),
                updated_at: "2026-05-07T13:00:01Z".to_string(),
            }],
            total: 1,
            limit: 20,
            offset: 0,
        };

        let actual = serde_json::to_string(&response).unwrap();
        let expected = r#"{"data":[{"owner":"bart","name":"syns","description":"a repo","commitSha":"abc123","status":"active","author":"Alice","tags":["alpha","beta"],"visibility":"public","forkedFrom":{"owner":"upstream","name":"syns"},"forkCount":2,"fileCount":436,"role":"owner","createdAt":"2025-01-01T00:00:00Z","updatedAt":"2026-05-07T13:00:01Z"}],"total":1,"limit":20,"offset":0}"#;
        assert_eq!(actual, expected);
    }

    // --- process_response_raw tests (u210) ---

    #[derive(serde::Deserialize, Debug)]
    struct TestShape {
        foo: String,
        nested: serde_json::Value,
    }

    #[tokio::test]
    async fn process_response_raw_returns_typed_and_raw_value_byte_equivalent() {
        let mock_server = MockServer::start().await;
        let body = r#"{"foo":"bar","nested":{"a":1,"b":[2,3]}}"#;
        Mock::given(method("GET"))
            .and(path("/test-endpoint"))
            .respond_with(ResponseTemplate::new(200).set_body_string(body))
            .mount(&mock_server)
            .await;

        let response = reqwest::Client::new()
            .get(format!("{}/test-endpoint", mock_server.uri()))
            .send()
            .await
            .unwrap();

        let result = process_response_raw::<TestShape>(response).await;
        let (typed, raw) = result.unwrap();
        assert_eq!(typed.foo, "bar");
        assert_eq!(typed.nested, serde_json::json!({"a": 1, "b": [2, 3]}));
        let expected_raw: serde_json::Value = serde_json::from_str(body).unwrap();
        assert_eq!(raw, expected_raw);
    }

    #[tokio::test]
    async fn process_response_raw_propagates_check_response_errors() {
        let mock_server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/test-error"))
            .respond_with(ResponseTemplate::new(401).set_body_string(r#"{"error":"unauthorized"}"#))
            .mount(&mock_server)
            .await;

        let response = reqwest::Client::new()
            .get(format!("{}/test-error", mock_server.uri()))
            .send()
            .await
            .unwrap();

        let result = process_response_raw::<TestShape>(response).await;
        assert!(matches!(result, Err(CliError::AuthRequired)));
    }

    #[tokio::test]
    async fn process_response_raw_fails_on_unparseable_body() {
        let mock_server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/test-bad"))
            .respond_with(ResponseTemplate::new(200).set_body_string("not-json-at-all"))
            .mount(&mock_server)
            .await;

        let response = reqwest::Client::new()
            .get(format!("{}/test-bad", mock_server.uri()))
            .send()
            .await
            .unwrap();

        let result = process_response_raw::<TestShape>(response).await;
        match result {
            Err(CliError::Api {
                status: Some(200),
                ref error,
                ..
            }) => assert!(
                error.starts_with("invalid response body:"),
                "expected 'invalid response body:' prefix, got: {error}"
            ),
            other => panic!("expected Api error with status 200, got: {other:?}"),
        }
    }

    #[tokio::test]
    async fn process_response_raw_fails_on_shape_mismatch() {
        let mock_server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/test-shape"))
            .respond_with(ResponseTemplate::new(200).set_body_string(r#"{"unrelated":"x"}"#))
            .mount(&mock_server)
            .await;

        let response = reqwest::Client::new()
            .get(format!("{}/test-shape", mock_server.uri()))
            .send()
            .await
            .unwrap();

        let result = process_response_raw::<TestShape>(response).await;
        match result {
            Err(CliError::Api {
                status: Some(200),
                ref error,
                ..
            }) => {
                assert!(
                    error.contains("invalid response body"),
                    "expected message containing 'invalid response body', got: {error}"
                );
                assert!(
                    error.contains("foo"),
                    "expected message mentioning missing field 'foo', got: {error}"
                );
            }
            other => panic!("expected Api error with status 200, got: {other:?}"),
        }
    }

    #[tokio::test]
    async fn process_response_raw_413_html_with_cloudflare_marker_maps_to_cloudflare_variant() {
        let mock_server = MockServer::start().await;
        let body = "<html><head><title>413 Request Entity Too Large</title></head><body>\n<center>cloudflare</center>\n</body></html>";
        Mock::given(method("PUT"))
            .and(path("/test-413-cf"))
            .respond_with(ResponseTemplate::new(413).set_body_string(body))
            .mount(&mock_server)
            .await;

        let response = reqwest::Client::new()
            .put(format!("{}/test-413-cf", mock_server.uri()))
            .send()
            .await
            .unwrap();

        let result = process_response_raw::<TestShape>(response).await;
        assert!(
            matches!(
                result,
                Err(CliError::PayloadTooLarge {
                    rejecter: EdgeRejecter::Cloudflare,
                    ..
                })
            ),
            "expected Err(PayloadTooLarge{{ rejecter: Cloudflare, .. }}), got: {result:?}"
        );
    }

    #[tokio::test]
    async fn process_response_raw_413_html_without_cloudflare_marker_maps_to_cloud_run_variant() {
        let mock_server = MockServer::start().await;
        let body = "<html><head><title>413 Request Entity Too Large</title></head><body>413 Request Entity Too Large</body></html>";
        Mock::given(method("PUT"))
            .and(path("/test-413-gcp"))
            .respond_with(ResponseTemplate::new(413).set_body_string(body))
            .mount(&mock_server)
            .await;

        let response = reqwest::Client::new()
            .put(format!("{}/test-413-gcp", mock_server.uri()))
            .send()
            .await
            .unwrap();

        let result = process_response_raw::<TestShape>(response).await;
        assert!(
            matches!(
                result,
                Err(CliError::PayloadTooLarge {
                    rejecter: EdgeRejecter::CloudRunOrFrontend,
                    ..
                })
            ),
            "expected Err(PayloadTooLarge{{ rejecter: CloudRunOrFrontend, .. }}), got: {result:?}"
        );
    }

    // MED-2: cover the third sniff branch — a canonical ApiErrorBody
    // JSON 413 body whose `error == "payload_too_large"` maps to
    // EdgeRejecter::Server. Pins the future-compatibility path
    // documented in SPEC § 3 EdgeRejecter table.
    #[tokio::test]
    async fn process_response_raw_413_json_payload_too_large_maps_to_server_variant() {
        let mock_server = MockServer::start().await;
        Mock::given(method("PUT"))
            .and(path("/test-413-server"))
            .respond_with(ResponseTemplate::new(413).set_body_json(serde_json::json!({
                "error": "payload_too_large",
                "message": "Request body too large",
            })))
            .mount(&mock_server)
            .await;

        let response = reqwest::Client::new()
            .put(format!("{}/test-413-server", mock_server.uri()))
            .send()
            .await
            .unwrap();

        let result = process_response_raw::<TestShape>(response).await;
        assert!(
            matches!(
                result,
                Err(CliError::PayloadTooLarge {
                    rejecter: EdgeRejecter::Server,
                    ..
                })
            ),
            "expected Err(PayloadTooLarge{{ rejecter: Server, .. }}), got: {result:?}"
        );
    }
}

#[cfg(test)]
mod provenance_tests {
    use super::*;

    #[test]
    fn provenance_serialises_only_what_is_asserted() {
        let bare = PushRequest {
            files: vec![],
            deletions: None,
            message: Some("push".into()),
            author: None,
            parent_sha: None,
            description: None,
            tags: None,
            status: None,
            visibility: None,
            provenance: None,
        };
        let body = serde_json::to_value(&bare).unwrap();
        assert!(body.get("provenance").is_none(), "{body}");

        let block = PushProvenance {
            integration: "claude-code".into(),
            run: "session-1".into(),
            trigger: "stop".into(),
            task_ref: None,
        };
        let body = serde_json::to_value(&block).unwrap();
        assert_eq!(
            body,
            serde_json::json!({"integration": "claude-code", "run": "session-1", "trigger": "stop"})
        );
        assert!(body.get("taskRef").is_none());

        let with_task = PushProvenance {
            task_ref: Some("T-1".into()),
            ..block
        };
        assert_eq!(serde_json::to_value(&with_task).unwrap()["taskRef"], "T-1");
    }

    #[test]
    fn file_version_entry_reads_a_null_blob_sha_and_content_as_none() {
        let diff = "--- a/CLAUDE.md\n+++ /dev/null\n@@ -1 +0,0 @@\n-# syns\n";
        let entry: FileVersionEntry = serde_json::from_value(serde_json::json!({
            "version": 436, "sha": "ff7f52cad5c73554fff96676478cd4b2a509fbdc",
            "blobSha": null, "message": "claude code session", "author": "bartsoj",
            "createdAt": "2026-09-13T14:31:20Z", "content": null, "diff": diff,
            "provenance": null
        }))
        .unwrap();
        assert!(entry.blob_sha.is_none());
        assert!(entry.content.is_none());
        assert_eq!(entry.diff.as_deref(), Some(diff));
        assert_eq!(entry.version, 436);
    }

    #[test]
    fn file_version_entry_keeps_a_served_blob_sha_and_content() {
        let entry: FileVersionEntry = serde_json::from_value(serde_json::json!({
            "version": 439, "sha": "739d8dc0c095de7ab390d685c0e2e8629d61db1f",
            "blobSha": "d54fa145810ef1ad6183d93229bb2982571cc3da",
            "message": "claude code session (part 3/3)", "author": "bartsoj",
            "createdAt": "2026-09-13T14:34:58Z", "content": "# syns", "diff": null
        }))
        .unwrap();
        assert_eq!(
            entry.blob_sha.as_deref(),
            Some("d54fa145810ef1ad6183d93229bb2982571cc3da")
        );
        assert_eq!(entry.content.as_deref(), Some("# syns"));
    }

    #[test]
    fn version_entries_read_a_null_or_missing_provenance_as_none() {
        let with_null: VersionEntry = serde_json::from_value(serde_json::json!({
            "version": 1, "sha": "a", "message": "m", "author": "ana",
            "createdAt": "2026-01-01T00:00:00Z", "filesChanged": [], "provenance": null
        }))
        .unwrap();
        assert!(with_null.provenance.is_none());

        let missing: FileVersionEntry = serde_json::from_value(serde_json::json!({
            "version": 1, "sha": "a", "blobSha": "b", "message": "m", "author": "ana",
            "createdAt": "2026-01-01T00:00:00Z", "content": "", "diff": null
        }))
        .unwrap();
        assert!(missing.provenance.is_none());

        let asserted: VersionEntry = serde_json::from_value(serde_json::json!({
            "version": 1, "sha": "a", "message": "m", "author": "ana",
            "createdAt": "2026-01-01T00:00:00Z", "filesChanged": [],
            "provenance": {"publisher": "bo", "integration": "codex", "run": "r", "trigger": null, "taskRef": null}
        }))
        .unwrap();
        let p = asserted.provenance.unwrap();
        assert_eq!(p.publisher, "bo");
        assert_eq!(p.integration.as_deref(), Some("codex"));
        assert!(p.trigger.is_none() && p.task_ref.is_none());
    }
}

#[cfg(test)]
mod head_moved_tests {
    use super::*;
    use crate::errors::ApiErrorContext;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn push_request() -> PushRequest {
        PushRequest {
            files: vec![PushFileEntry {
                path: "a.md".into(),
                sha: "0".repeat(40),
                content: Some("keep two".into()),
                content_base64: None,
            }],
            deletions: None,
            message: Some("edit a.md".into()),
            author: None,
            parent_sha: Some("a".repeat(40)),
            description: None,
            tags: None,
            status: None,
            visibility: None,
            provenance: None,
        }
    }

    fn push_body_bytes() -> Vec<u8> {
        serde_json::to_vec(&push_request()).unwrap()
    }

    /// SPEC u271, `src/client.rs`: the `409` fold carries the refused
    /// answer's `currentSha` out rather than reducing it to a presence
    /// test — nothing downstream can read the body a second time.
    #[tokio::test]
    async fn a_conflict_bodys_current_sha_survives_the_client_error_fold() {
        let server = MockServer::start().await;
        Mock::given(method("PUT"))
            .and(path("/api/v1/repos/alice/notes/push"))
            .respond_with(ResponseTemplate::new(409).set_body_json(serde_json::json!({
                "error": "conflict",
                "message": "Head mismatch",
                "currentSha": "c".repeat(40),
            })))
            .mount(&server)
            .await;

        let client = SynsClient::new(&server.uri()).unwrap();
        let err = client
            .push_body("alice/notes", "t", push_body_bytes())
            .await
            .unwrap_err();

        match err {
            CliError::Api {
                status: Some(409),
                error,
                context: Some(ApiErrorContext::HeadMoved { current_sha }),
            } => {
                assert_eq!(error, "conflict");
                assert_eq!(current_sha, "c".repeat(40));
            }
            other => panic!("expected a moved head, got {other:?}"),
        }
    }

    /// A `conflict` naming no head — an identity already taken — carries
    /// no such context, so the run keeps exit `1`.
    #[tokio::test]
    async fn a_conflict_naming_no_head_carries_no_context() {
        let server = MockServer::start().await;
        Mock::given(method("PUT"))
            .and(path("/api/v1/repos/alice/notes/push"))
            .respond_with(
                ResponseTemplate::new(409).set_body_json(serde_json::json!({"error": "conflict"})),
            )
            .mount(&server)
            .await;

        let client = SynsClient::new(&server.uri()).unwrap();
        let err = client
            .push_body("alice/notes", "t", push_body_bytes())
            .await
            .unwrap_err();

        assert!(matches!(err, CliError::Api { context: None, .. }));
        assert_eq!(err.exit_code(), 1);
    }

    async fn refusing_push(body: serde_json::Value) -> CliError {
        let server = MockServer::start().await;
        Mock::given(method("PUT"))
            .and(path("/api/v1/repos/alice/notes/push"))
            .respond_with(ResponseTemplate::new(409).set_body_json(body))
            .mount(&server)
            .await;
        let client = SynsClient::new(&server.uri()).unwrap();
        client
            .push_body("alice/notes", "t", push_body_bytes())
            .await
            .unwrap_err()
    }

    /// SPEC u280 `ApiErrorContext::MissingBlobs`: the refusal's `missing`
    /// map crosses whole.
    #[tokio::test]
    async fn a_missing_blobs_answer_carries_its_map() {
        let err = refusing_push(serde_json::json!({
            "error": "missing_blobs",
            "message": "some referenced blobs are missing on the server",
            "missing": {"a.md": "1".repeat(40), "b/c.png": "2".repeat(40)},
        }))
        .await;
        match err {
            CliError::Api {
                status: Some(409),
                error,
                context: Some(ApiErrorContext::MissingBlobs { missing }),
            } => {
                assert_eq!(error, "missing_blobs");
                assert_eq!(
                    missing,
                    std::collections::BTreeMap::from([
                        ("a.md".to_string(), "1".repeat(40)),
                        ("b/c.png".to_string(), "2".repeat(40)),
                    ])
                );
            }
            other => panic!("expected the missing map, got {other:?}"),
        }
    }

    /// A `missing_blobs` naming no map, or one that is no map of path to
    /// hash, carries no context.
    #[tokio::test]
    async fn a_missing_blobs_answer_naming_no_map_carries_no_context() {
        for body in [
            serde_json::json!({"error": "missing_blobs"}),
            serde_json::json!({"error": "missing_blobs", "missing": ["a.md"]}),
            serde_json::json!({"error": "missing_blobs", "missing": {"a.md": 1}}),
        ] {
            let err = refusing_push(body.clone()).await;
            assert!(
                matches!(
                    err,
                    CliError::Api {
                        status: Some(409),
                        context: None,
                        ..
                    }
                ),
                "{body}: {err:?}"
            );
        }
    }

    /// SPEC u280 `push_body`: one `EP-push` request carrying the body it
    /// is handed byte for byte, under the bearer token and JSON type.
    #[tokio::test]
    async fn push_body_sends_its_bytes_unchanged() {
        let server = MockServer::start().await;
        Mock::given(method("PUT"))
            .and(path("/api/v1/repos/alice/notes/push"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "commitSha": "c".repeat(40),
                "version": 1,
                "filesChanged": 1,
                "created": false,
            })))
            .mount(&server)
            .await;
        let client = SynsClient::new(&server.uri()).unwrap();
        // Bytes no serialiser of this crate would write: key order and
        // spacing of their own.
        let body = br#"{ "message":"m",  "files":[{"sha":"s","path":"a.md"}] }"#.to_vec();

        let (response, raw) = client
            .push_body("alice/notes", "t", body.clone())
            .await
            .unwrap();

        assert_eq!(response.commit_sha, "c".repeat(40));
        assert_eq!(raw["version"], 1);
        let requests = server.received_requests().await.unwrap();
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0].body, body);
        assert_eq!(
            requests[0].headers.get("authorization").unwrap(),
            "Bearer t"
        );
        assert_eq!(
            requests[0].headers.get("content-type").unwrap(),
            "application/json"
        );
    }
}

#[cfg(test)]
mod u272_model_tests {
    use super::u272_bodies as B;
    use super::*;

    fn value(raw: &str) -> serde_json::Value {
        serde_json::from_str(raw).expect("the captured body parses")
    }

    // SPEC u272 Contract Surface, the added response models — and
    // `PROTOTYPE.md`'s first Constraints row: the search answer is the
    // `{ data: … }` envelope, which the bare vector refuses.
    #[test]
    fn the_search_answer_decodes_under_its_envelope_and_not_as_a_bare_vector() {
        for raw in [B::SEARCH_BART, B::SEARCH_LOCAL] {
            let body = value(raw);
            let typed: UserSearchResponse =
                serde_json::from_value(body.clone()).expect("the envelope decodes");
            assert_eq!(typed.data.len(), 1);
            assert!(
                serde_json::from_value::<Vec<UserSummary>>(body).is_err(),
                "the bare vector must refuse the served envelope"
            );
        }
        let typed: UserSearchResponse = serde_json::from_value(value(B::SEARCH_BART)).unwrap();
        assert_eq!(typed.data[0].username, "bartosz-sojka");
        assert_eq!(typed.data[0].email, None);
    }

    // The same for all four link entries.
    #[test]
    fn every_link_answer_decodes_under_its_envelope_and_not_as_a_bare_vector() {
        for raw in [
            B::LINKS_CREATE,
            B::LINKS_UPDATE,
            B::LINKS_REORDER,
            B::LINKS_DELETE,
        ] {
            let body = value(raw);
            let typed: UserLinksResponse =
                serde_json::from_value(body.clone()).expect("the envelope decodes");
            assert!(!typed.links.is_empty());
            assert!(
                serde_json::from_value::<Vec<UserLink>>(body).is_err(),
                "the bare vector must refuse the served envelope"
            );
        }
        let reordered: UserLinksResponse = serde_json::from_value(value(B::LINKS_REORDER)).unwrap();
        let order: Vec<u32> = reordered.links.iter().map(|l| l.sort_order).collect();
        assert_eq!(order, vec![0, 1, 2]);
        assert_eq!(reordered.links[0].kind, "generic");
        assert_eq!(
            reordered.links[0].label.as_deref(),
            Some("u272 probe updated")
        );
        assert_eq!(reordered.links[1].label, None);
    }

    // SPEC u272 Contract Surface, `UserProfile`.
    #[test]
    fn the_profile_decodes_every_field_its_entry_serves() {
        let full: UserProfile = serde_json::from_value(value(B::PROFILE_SELF)).unwrap();
        assert_eq!(full.username, "bartsoj");
        assert_eq!(full.company.as_deref(), Some("JetBrains"));
        assert_eq!(full.location.as_deref(), Some("Amsterdam"));
        assert_eq!(full.pronouns, None);
        assert_eq!(full.time_zone, None);
        assert_eq!(full.repo_count, 49);
        assert_eq!(full.links.len(), 2);
        assert_eq!(full.links[0].kind, "linkedin");

        let sparse: UserProfile = serde_json::from_value(value(B::PROFILE_LOCAL)).unwrap();
        assert!(sparse.links.is_empty());
        assert_eq!(sparse.bio, None);
        assert_eq!(sparse.repo_count, 1);
    }

    // SPEC u272 Contract Surface: `VersionEntry` widened with the two
    // keys the served version carries and the shipped model dropped.
    #[test]
    fn the_version_entry_reaches_the_parent_and_the_message_body() {
        let entry: VersionEntry = serde_json::from_value(value(B::VERSION_HEAD)).unwrap();
        assert_eq!(entry.version, 596);
        assert_eq!(
            entry.parent_sha.as_deref(),
            Some("686ee7156c28aca8f7d9411c3f1a50631257d59d")
        );
        assert_eq!(entry.message_body, None);
        assert_eq!(entry.files_changed.len(), 17);
        assert_eq!(entry.provenance.unwrap().publisher, "bartsoj");
    }

    // The version listing serves neither key on a body captured before
    // u272; the widened model must still decode it.
    #[test]
    fn a_version_body_serving_neither_added_key_still_decodes() {
        let entry: VersionEntry = serde_json::from_value(serde_json::json!({
            "version": 1, "sha": "a".repeat(40), "message": "m", "author": "alice",
            "createdAt": "2026-01-01T00:00:00Z", "filesChanged": ["a.md"],
        }))
        .unwrap();
        assert_eq!(entry.parent_sha, None);
        assert_eq!(entry.message_body, None);
    }

    // SPEC u272 Bindings, `Page<Repository>`: the fork page is the same
    // page shape the repository listing already decodes.
    #[test]
    fn the_fork_page_decodes_as_the_registered_page() {
        let empty: RepoListResponse = serde_json::from_value(value(B::FORKS_EMPTY)).unwrap();
        assert_eq!(empty.total, 0);
        assert_eq!(empty.limit, 20);

        let full: RepoListResponse = serde_json::from_value(value(B::FORKS_LOCAL)).unwrap();
        assert_eq!(full.data.len(), 1);
        assert_eq!(full.data[0].name, "u272-fork");
        assert_eq!(
            full.data[0].forked_from.as_ref().unwrap().owner,
            "u272alice"
        );
    }

    // SPEC u272 Bindings, `Collaborator` and `Repository`.
    #[test]
    fn the_role_change_and_the_created_repository_decode() {
        let collaborator: Collaborator = serde_json::from_value(value(B::ROLE_LOCAL)).unwrap();
        assert_eq!(collaborator.role, CollaboratorRole::Write);
        assert_eq!(collaborator.user.username, "u272bob");

        let created: RepoResponse = serde_json::from_value(value(B::CREATED_REPO)).unwrap();
        assert_eq!(created.status, RepoStatus::Draft);
        assert_eq!(created.visibility, Visibility::Private);
        assert_eq!(created.role, Some(CollaboratorRole::Owner));
        assert_eq!(created.commit_sha, None);
    }

    // SPEC u272 Contract Surface, `create_repo`: only the fields the
    // caller gave reach the wire.
    #[test]
    fn the_create_body_carries_only_what_the_caller_gave() {
        let bare = serde_json::to_value(&CreateRepoRequest {
            name: "notes".into(),
            description: None,
            visibility: None,
        })
        .unwrap();
        assert_eq!(bare, serde_json::json!({"name": "notes"}));

        let whole = serde_json::to_value(&CreateRepoRequest {
            name: "notes".into(),
            description: Some("d".into()),
            visibility: Some(Visibility::Public),
        })
        .unwrap();
        assert_eq!(
            whole,
            serde_json::json!({"name": "notes", "description": "d", "visibility": "public"})
        );
    }

    // SPEC u272 Contract Surface, `LinksAction`: an omitted `--label`
    // on an update leaves the label the entry holds.
    #[test]
    fn the_link_write_bodies_leave_out_what_the_caller_omitted() {
        let add = serde_json::to_value(&CreateUserLinkRequest {
            kind: "github".into(),
            value: "https://example.test/a".into(),
            label: None,
        })
        .unwrap();
        assert_eq!(
            add,
            serde_json::json!({"kind": "github", "value": "https://example.test/a"})
        );

        let update = serde_json::to_value(&UpdateUserLinkRequest {
            kind: None,
            value: None,
            label: None,
            sort_order: Some(2),
        })
        .unwrap();
        assert_eq!(update, serde_json::json!({"sortOrder": 2}));

        let reorder = serde_json::to_value(&ReorderUserLinksRequest {
            order: vec!["a".into(), "b".into()],
        })
        .unwrap();
        assert_eq!(reorder, serde_json::json!({"order": ["a", "b"]}));
    }

    // The collaborator page the listing answers, as the widened options
    // will send it (`LOCAL-EP-list-collaborators`).
    #[test]
    fn the_collaborator_page_decodes_with_its_window() {
        let page: CollaboratorListResponse =
            serde_json::from_value(value(B::COLLABORATORS_LOCAL)).unwrap();
        assert_eq!(page.limit, 100);
        assert_eq!(page.offset, 0);
        assert_eq!(page.data[0].role, CollaboratorRole::Read);
    }
}

#[cfg(test)]
mod u272_entry_tests {
    use super::u272_bodies as B;
    use super::*;
    use crate::commands::links::{LinkKind, LinksAction};
    use wiremock::matchers::{header, method, path, query_param};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn served(raw: &str) -> ResponseTemplate {
        ResponseTemplate::new(200).set_body_string(raw)
    }

    fn expected(raw: &str) -> serde_json::Value {
        serde_json::from_str(raw).unwrap()
    }

    // SPEC u272 Contract Surface, `list_forks`: the caller's window
    // reaches the entry, and the raw member is the served bytes.
    #[tokio::test]
    async fn list_forks_sends_the_window_and_answers_the_served_page() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/v1/repos/u272alice/u272-parent/forks"))
            .and(query_param("limit", "7"))
            .and(query_param("offset", "3"))
            .and(header("authorization", "Bearer u272-token"))
            .respond_with(served(B::FORKS_LOCAL))
            .mount(&server)
            .await;

        let client = SynsClient::new(&server.uri()).unwrap();
        let (typed, raw) = client
            .list_forks("u272alice/u272-parent", Some("u272-token"), 7, 3)
            .await
            .unwrap();

        assert_eq!(raw, expected(B::FORKS_LOCAL));
        assert_eq!(typed.data.len(), 1);
    }

    // SPEC u272 Behaviour, `resolve_repo_scope` 2: nothing rides where
    // no credential stands.
    #[tokio::test]
    async fn list_forks_carries_no_credential_where_none_stands() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/v1/repos/alice/notes/forks"))
            .respond_with(served(B::FORKS_EMPTY))
            .mount(&server)
            .await;

        let client = SynsClient::new(&server.uri()).unwrap();
        let (_typed, raw) = client.list_forks("alice/notes", None, 20, 0).await.unwrap();

        assert_eq!(raw, expected(B::FORKS_EMPTY));
        let requests = server.received_requests().await.unwrap();
        assert_eq!(requests.len(), 1);
        assert!(requests[0].headers.get("authorization").is_none());
    }

    // SPEC u272 Contract Surface, `search_users`.
    #[tokio::test]
    async fn search_users_sends_the_query_under_the_credential() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/v1/users"))
            .and(query_param("q", "bart"))
            .and(query_param("limit", "20"))
            .and(header("authorization", "Bearer u272-token"))
            .respond_with(served(B::SEARCH_BART))
            .mount(&server)
            .await;

        let client = SynsClient::new(&server.uri()).unwrap();
        let (typed, raw) = client.search_users("u272-token", "bart", 20).await.unwrap();

        assert_eq!(raw, expected(B::SEARCH_BART));
        assert_eq!(typed.data[0].username, "bartosz-sojka");
    }

    // SPEC u272 Contract Surface, `get_user_profile`: the credential
    // rides where one stands and nothing rides where none does.
    #[tokio::test]
    async fn get_user_profile_rides_the_credential_only_where_one_stands() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/v1/users/bartsoj"))
            .respond_with(served(B::PROFILE_SELF))
            .mount(&server)
            .await;

        let client = SynsClient::new(&server.uri()).unwrap();
        let (_typed, raw) = client
            .get_user_profile("bartsoj", Some("u272-token"))
            .await
            .unwrap();
        assert_eq!(raw, expected(B::PROFILE_SELF));
        client.get_user_profile("bartsoj", None).await.unwrap();

        let requests = server.received_requests().await.unwrap();
        assert_eq!(requests.len(), 2);
        assert_eq!(
            requests[0].headers.get("authorization").unwrap(),
            "Bearer u272-token"
        );
        assert!(requests[1].headers.get("authorization").is_none());
    }

    // SPEC u272 Contract Surface, `user_links`: one function, four
    // entries, each answering the whole ordered list.
    #[tokio::test]
    async fn the_add_arm_posts_the_link_and_answers_the_whole_list() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/api/v1/me/links"))
            .and(header("authorization", "Bearer u272-token"))
            .respond_with(ResponseTemplate::new(201).set_body_string(B::LINKS_CREATE))
            .mount(&server)
            .await;

        let client = SynsClient::new(&server.uri()).unwrap();
        let (typed, raw) = client
            .user_links(
                "u272-token",
                &LinksAction::Add {
                    kind: LinkKind::Generic,
                    value: "https://u272.example.test/probe".into(),
                    label: Some("u272 probe".into()),
                },
            )
            .await
            .unwrap();

        assert_eq!(raw, expected(B::LINKS_CREATE));
        assert_eq!(typed.links.len(), 3);
        let sent: serde_json::Value =
            serde_json::from_slice(&server.received_requests().await.unwrap()[0].body).unwrap();
        assert_eq!(
            sent,
            serde_json::json!({
                "kind": "generic",
                "value": "https://u272.example.test/probe",
                "label": "u272 probe",
            })
        );
    }

    #[tokio::test]
    async fn the_update_arm_patches_the_identifier_it_was_given() {
        let server = MockServer::start().await;
        Mock::given(method("PATCH"))
            .and(path(
                "/api/v1/me/links/c3927ea2-a953-498b-8e83-86c870887758",
            ))
            .respond_with(served(B::LINKS_UPDATE))
            .mount(&server)
            .await;

        let client = SynsClient::new(&server.uri()).unwrap();
        let (_typed, raw) = client
            .user_links(
                "u272-token",
                &LinksAction::Update {
                    id: "c3927ea2-a953-498b-8e83-86c870887758".into(),
                    kind: None,
                    value: None,
                    label: Some("u272 probe updated".into()),
                    sort_order: None,
                },
            )
            .await
            .unwrap();

        assert_eq!(raw, expected(B::LINKS_UPDATE));
        let sent: serde_json::Value =
            serde_json::from_slice(&server.received_requests().await.unwrap()[0].body).unwrap();
        assert_eq!(sent, serde_json::json!({"label": "u272 probe updated"}));
    }

    #[tokio::test]
    async fn the_remove_arm_deletes_the_identifier_it_was_given() {
        let server = MockServer::start().await;
        Mock::given(method("DELETE"))
            .and(path(
                "/api/v1/me/links/c3927ea2-a953-498b-8e83-86c870887758",
            ))
            .respond_with(served(B::LINKS_DELETE))
            .mount(&server)
            .await;

        let client = SynsClient::new(&server.uri()).unwrap();
        let (typed, raw) = client
            .user_links(
                "u272-token",
                &LinksAction::Remove {
                    id: "c3927ea2-a953-498b-8e83-86c870887758".into(),
                },
            )
            .await
            .unwrap();

        assert_eq!(raw, expected(B::LINKS_DELETE));
        assert_eq!(typed.links.len(), 2);
    }

    #[tokio::test]
    async fn the_reorder_arm_sends_the_order_it_was_typed() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/api/v1/me/links/reorder"))
            .respond_with(served(B::LINKS_REORDER))
            .mount(&server)
            .await;

        let client = SynsClient::new(&server.uri()).unwrap();
        let order = vec![
            "c3927ea2-a953-498b-8e83-86c870887758".to_string(),
            "21474465-4502-4150-adf0-aa0b6a0cc4d5".to_string(),
            "6efbd0a9-e09d-459d-9277-98838c854b5b".to_string(),
        ];
        let (_typed, raw) = client
            .user_links(
                "u272-token",
                &LinksAction::Reorder {
                    order: order.clone(),
                },
            )
            .await
            .unwrap();

        assert_eq!(raw, expected(B::LINKS_REORDER));
        let sent: serde_json::Value =
            serde_json::from_slice(&server.received_requests().await.unwrap()[0].body).unwrap();
        assert_eq!(sent, serde_json::json!({ "order": order }));
    }

    // SPEC u272 Contract Surface, `create_repo`.
    #[tokio::test]
    async fn create_repo_sends_only_the_fields_the_caller_gave() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/api/v1/repos"))
            .and(header("authorization", "Bearer u272-token"))
            .respond_with(ResponseTemplate::new(201).set_body_string(B::CREATED_REPO))
            .mount(&server)
            .await;

        let client = SynsClient::new(&server.uri()).unwrap();
        let (typed, raw) = client
            .create_repo("u272-token", "u272-parity-probe", None, None)
            .await
            .unwrap();

        assert_eq!(raw, expected(B::CREATED_REPO));
        assert_eq!(typed.name, "u272-parity-probe");
        let sent: serde_json::Value =
            serde_json::from_slice(&server.received_requests().await.unwrap()[0].body).unwrap();
        assert_eq!(sent, serde_json::json!({"name": "u272-parity-probe"}));
    }

    // SPEC u272 Contract Surface, `update_collaborator_role`: the raw
    // member carries the four user keys the typed model leaves unread.
    #[tokio::test]
    async fn the_role_change_answers_the_collaborator_the_entry_served() {
        let server = MockServer::start().await;
        Mock::given(method("PATCH"))
            .and(path(
                "/api/v1/repos/u272alice/u272-parent/collaborators/u272user1111111111111111111111111",
            ))
            .and(header("authorization", "Bearer u272-token"))
            .respond_with(served(B::ROLE_LOCAL))
            .mount(&server)
            .await;

        let client = SynsClient::new(&server.uri()).unwrap();
        let (typed, raw) = client
            .update_collaborator_role(
                "u272alice/u272-parent",
                "u272-token",
                "u272user1111111111111111111111111",
                &UpdateCollaboratorRoleRequest {
                    role: CollaboratorRole::Write,
                },
            )
            .await
            .unwrap();

        assert_eq!(raw, expected(B::ROLE_LOCAL));
        assert_eq!(typed.role, CollaboratorRole::Write);
        for key in ["createdAt", "emailVerified", "image", "updatedAt"] {
            assert!(
                raw["user"].get(key).is_some(),
                "the served body keeps {key}"
            );
        }
        let sent: serde_json::Value =
            serde_json::from_slice(&server.received_requests().await.unwrap()[0].body).unwrap();
        assert_eq!(sent, serde_json::json!({"role": "write"}));
    }

    // SPEC u272 Behaviour, `cmd_users` 2: the rate refusal the entry
    // answers carries neither an interval nor a remaining budget.
    #[tokio::test]
    async fn the_rate_refusal_reaches_the_caller_as_the_entry_wrote_it() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/v1/users"))
            .respond_with(ResponseTemplate::new(429).set_body_string(B::SEARCH_429))
            .mount(&server)
            .await;

        let client = SynsClient::new(&server.uri()).unwrap();
        let err = client
            .search_users("u272-token", "bart", 20)
            .await
            .unwrap_err();

        match err {
            CliError::Api {
                status: Some(429),
                ref error,
                ..
            } => assert_eq!(error, "rate_limited"),
            other => panic!("expected the rate refusal, got {other:?}"),
        }
        assert_eq!(err.exit_code(), 1);
        assert_eq!(err.to_string(), "server error (429): rate_limited");
    }
}

#[cfg(test)]
mod u280_transport_tests {
    use super::*;
    use crate::push::hash::blob_sha1;
    use std::time::{Duration, Instant};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    #[test]
    fn deadline_grows_with_the_body() {
        assert_eq!(request_deadline(0), Duration::from_secs(30));
        assert_eq!(request_deadline(262_144), Duration::from_secs(31));
        assert_eq!(request_deadline(52_428_800), Duration::from_secs(230));
    }

    /// A listener answering one request under `Content-Length: 40` with
    /// `script`: each piece written after its pause, then the connection
    /// held open for `linger` or closed at once.
    async fn scripted_listener(script: Vec<(Duration, Vec<u8>)>, linger: Duration) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let (mut sock, _) = listener.accept().await.unwrap();
            let mut buf = vec![0u8; 4096];
            let mut seen = Vec::new();
            while !seen.windows(4).any(|w| w == b"\r\n\r\n") {
                let n = sock.read(&mut buf).await.unwrap();
                if n == 0 {
                    return;
                }
                seen.extend_from_slice(&buf[..n]);
            }
            sock.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 40\r\n\r\n")
                .await
                .unwrap();
            for (pause, piece) in script {
                tokio::time::sleep(pause).await;
                if sock.write_all(&piece).await.is_err() {
                    return;
                }
                let _ = sock.flush().await;
            }
            tokio::time::sleep(linger).await;
        });
        format!("http://127.0.0.1:{}/slow", addr.port())
    }

    async fn head_of(url: &str) -> reqwest::Response {
        let client = api_client().unwrap();
        send_bounded(client.get(url), 0).await.unwrap()
    }

    /// SPEC u280 `read_body`, `D-094`: a given capacity is the one buffer
    /// the body is read into, so a body of exactly that length leaves it
    /// neither grown nor reallocated.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_body_of_the_given_capacity_fills_a_buffer_of_that_capacity() {
        let len = 3 * 1024 * 1024 + 17;
        let bytes: Vec<u8> = (0..len).map(|i| (i % 251) as u8).collect();
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/sized"))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(bytes.clone()))
            .mount(&server)
            .await;
        let response = head_of(&format!("{}/sized", server.uri())).await;

        let body = read_body(response, ANSWER_STALL, Some(len)).await.unwrap();

        assert_eq!(body, bytes);
        assert_eq!(body.capacity(), len);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_moving_answer_outlasts_the_stall_bound() {
        let script = (0..40)
            .map(|i| (Duration::from_millis(100), vec![b'a' + (i % 26) as u8]))
            .collect();
        let url = scripted_listener(script, Duration::ZERO).await;
        let response = head_of(&url).await;
        let started = Instant::now();

        let body = read_body(response, Duration::from_secs(1), None)
            .await
            .unwrap();

        assert_eq!(body.len(), 40);
        let took = started.elapsed();
        assert!(
            took >= Duration::from_millis(3_500) && took < Duration::from_secs(8),
            "answered after {took:?}"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_stalled_or_short_answer_ends_as_unreachable() {
        // Ten bytes, then nothing while the connection stays open.
        let stalled = scripted_listener(
            vec![(Duration::ZERO, vec![b'x'; 10])],
            Duration::from_secs(10),
        )
        .await;
        let response = head_of(&stalled).await;
        let started = Instant::now();
        let err = read_body(response, Duration::from_secs(1), None)
            .await
            .unwrap_err();
        assert!(matches!(err, CliError::ServerUnreachable { .. }), "{err:?}");
        assert!(!err.to_string().contains("invalid response body"));
        assert!(started.elapsed() < Duration::from_secs(2));

        // Ten bytes, then the connection closed.
        let short = scripted_listener(vec![(Duration::ZERO, vec![b'x'; 10])], Duration::ZERO).await;
        let response = head_of(&short).await;
        let err = read_body(response, Duration::from_secs(1), None)
            .await
            .unwrap_err();
        assert!(matches!(err, CliError::ServerUnreachable { .. }), "{err:?}");
        assert!(!err.to_string().contains("invalid response body"));
    }

    /// A listener answering one request with `status` under
    /// `Content-Length: 40`, sending 10 bytes of it and then nothing.
    async fn stalled_refusal(status: u16) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let (mut sock, _) = listener.accept().await.unwrap();
            let mut buf = vec![0u8; 4096];
            let mut seen = Vec::new();
            while !seen.windows(4).any(|w| w == b"\r\n\r\n") {
                let n = sock.read(&mut buf).await.unwrap();
                if n == 0 {
                    return;
                }
                seen.extend_from_slice(&buf[..n]);
            }
            let head = format!("HTTP/1.1 {status} X\r\nContent-Length: 40\r\n\r\n{{\"error\":\"x");
            sock.write_all(head.as_bytes()).await.unwrap();
            let _ = sock.flush().await;
            tokio::time::sleep(Duration::from_secs(40)).await;
        });
        format!("http://127.0.0.1:{}/refused", addr.port())
    }

    /// CR1-1: a refusal whose body stalls is the server unreachable, never
    /// an unknown error or a frontend's 413.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_refusal_whose_body_stalls_ends_as_unreachable() {
        // The four statuses wait out the stall bound side by side.
        let mut waits = Vec::new();
        for status in [404u16, 409, 413, 503] {
            let url = stalled_refusal(status).await;
            waits.push(tokio::spawn(async move {
                let response = head_of(&url).await;
                let started = Instant::now();
                let err = tokio::time::timeout(Duration::from_secs(45), check_response(response))
                    .await
                    .expect("the stall bound ended the read")
                    .unwrap_err();
                (status, err, started.elapsed())
            }));
        }
        for wait in waits {
            let (status, err, took) = wait.await.unwrap();
            assert!(
                matches!(err, CliError::ServerUnreachable { .. }),
                "{status}: {err:?}"
            );
            assert!(took >= Duration::from_secs(25), "{status}: {took:?}");
        }
    }

    const BYTES: &[u8] = b"\x89PNG\r\n\x1a\n\x00\xff";

    async fn raw_server() -> MockServer {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/v1/repos/alice/r/raw/image.png"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("ETag", format!("\"{}\"", blob_sha1(BYTES)).as_str())
                    .set_body_bytes(BYTES.to_vec()),
            )
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/api/v1/repos/alice/r/raw/missing.png"))
            .respond_with(ResponseTemplate::new(404).set_body_json(serde_json::json!({
                "error": "not_found", "message": "File not found"
            })))
            .mount(&server)
            .await;
        server
    }

    #[tokio::test]
    async fn a_raw_answer_carries_its_bytes_and_its_etag() {
        let server = raw_server().await;
        let client = SynsClient::new(&server.uri()).unwrap();

        let raw = client
            .get_raw("alice/r", Some("t"), "image.png", Some("2"), None)
            .await
            .unwrap();

        assert_eq!(raw.bytes, BYTES);
        assert_eq!(raw.etag.as_deref(), Some(blob_sha1(BYTES).as_str()));
        let requests = server.received_requests().await.unwrap();
        assert_eq!(requests[0].url.query(), Some("ref=2"));
        assert_eq!(
            requests[0]
                .headers
                .get("user-agent")
                .and_then(|v| v.to_str().ok()),
            Some(USER_AGENT)
        );
    }

    #[tokio::test]
    async fn a_missing_raw_path_is_refused_as_the_file_read_refuses_it() {
        let server = raw_server().await;
        let client = SynsClient::new(&server.uri()).unwrap();

        let err = client
            .get_raw("alice/r", None, "missing.png", None, None)
            .await
            .unwrap_err();

        assert!(
            matches!(&err, CliError::Api { status: Some(404), error, .. } if error == "not_found"),
            "{err:?}"
        );
    }

    #[tokio::test]
    async fn a_staged_raw_answer_holds_the_body_in_its_file() {
        let server = raw_server().await;
        let client = SynsClient::new(&server.uri()).unwrap();
        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path().join("0");

        let staged = client
            .get_raw_staged("alice/r", None, "image.png", None, &dest)
            .await
            .unwrap();

        assert_eq!(std::fs::read(&dest).unwrap(), BYTES);
        assert_eq!(staged.sha, blob_sha1(BYTES));
        assert_eq!(staged.etag.as_deref(), Some(blob_sha1(BYTES).as_str()));
    }
}
