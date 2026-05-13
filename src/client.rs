#![allow(dead_code)] // Types and methods used by downstream units (U10, U11, U21+)

use reqwest::redirect::Policy;
use serde::{Deserialize, Serialize};

use crate::errors::CliError;

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
}

#[derive(Serialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct PushFileEntry {
    pub path: String,
    pub sha: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub content: Option<String>,
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

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AddCollaboratorRequest {
    pub user_id: String,
    pub role: CollaboratorRole,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct UpdateCollaboratorRoleRequest {
    pub role: CollaboratorRole,
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

#[derive(Deserialize, Debug)]
pub struct FileResponse {
    pub content: String,
    pub sha: String,
    pub size: u64,
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
    pub blob_sha: String,
    pub message: String,
    pub author: String,
    pub created_at: String,
    pub content: String,
    pub diff: Option<String>,
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
    pub message: String,
    pub author: String,
    pub created_at: String,
    pub files_changed: Vec<String>,
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

fn encode_path_segments(path: &str) -> String {
    path.split('/')
        .filter(|segment| !segment.is_empty())
        .map(|segment| urlencoding::encode(segment))
        .collect::<Vec<_>>()
        .join("/")
}

async fn check_response(response: reqwest::Response) -> Result<reqwest::Response, CliError> {
    let status = response.status();
    if status == reqwest::StatusCode::UNAUTHORIZED {
        return Err(CliError::AuthRequired);
    }
    if status.is_client_error() || status.is_server_error() {
        let code = status.as_u16();
        let error = match response.json::<ApiErrorBody>().await {
            Ok(body) => body.error,
            Err(_) => "unknown error".to_string(),
        };
        return Err(CliError::Api {
            status: Some(code),
            error,
            context: None,
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

async fn process_response<T: serde::de::DeserializeOwned>(
    response: reqwest::Response,
) -> Result<T, CliError> {
    let response = check_response(response).await?;
    let status = response.status();
    response.json::<T>().await.map_err(|e| CliError::Api {
        status: Some(status.as_u16()),
        error: format!("invalid response body: {e}"),
        context: None,
    })
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
/// `reqwest::Response::bytes` consumes the response.
async fn process_response_raw<T: serde::de::DeserializeOwned>(
    response: reqwest::Response,
) -> Result<(T, serde_json::Value), CliError> {
    let response = check_response(response).await?;
    let status = response.status();
    let bytes = response.bytes().await.map_err(|e| CliError::Api {
        status: Some(status.as_u16()),
        error: format!("invalid response body: {e}"),
        context: None,
    })?;
    let value: serde_json::Value = serde_json::from_slice(&bytes).map_err(|e| CliError::Api {
        status: Some(status.as_u16()),
        error: format!("invalid response body: {e}"),
        context: None,
    })?;
    let typed: T = serde_json::from_value(value.clone()).map_err(|e| CliError::Api {
        status: Some(status.as_u16()),
        error: format!("invalid response body: {e}"),
        context: None,
    })?;
    Ok((typed, value))
}

async fn process_empty_response(response: reqwest::Response) -> Result<(), CliError> {
    check_response(response).await?;
    Ok(())
}

// --- SynsClient ---

#[derive(Debug)]
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

        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(30))
            .redirect(Policy::none())
            .build()
            .map_err(|e| CliError::Config {
                message: e.to_string(),
            })?;

        Ok(SynsClient { client, base_url })
    }

    pub async fn push(
        &self,
        repo_id: &str,
        token: &str,
        request: &PushRequest,
    ) -> Result<(PushResponse, serde_json::Value), CliError> {
        let url = format!("{}/api/v1/repos/{}/push", self.base_url, repo_id);
        let response = self
            .client
            .put(&url)
            .bearer_auth(token)
            .json(request)
            .send()
            .await?;
        process_response_raw(response).await
    }

    pub async fn pull(&self, repo_id: &str, token: Option<&str>) -> Result<PullResponse, CliError> {
        let url = format!("{}/api/v1/repos/{}/tree", self.base_url, repo_id);
        let mut req = self.client.get(&url).query(&[("recursive", "true")]);
        if let Some(t) = token {
            req = req.bearer_auth(t);
        }
        let response = req.send().await?;
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
        let response = req.send().await?;
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
        let response = req.send().await?;
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
        let response = req.send().await?;
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
        let response = req.send().await?;
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
        let response = req.send().await?;
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
        let response = req.send().await?;
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
        let response = req.send().await?;
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
        let response = req.send().await?;
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
            .json(request)
            .send()
            .await?;
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
        let response = self.client.delete(&url).bearer_auth(token).send().await?;
        process_empty_response(response).await
    }

    pub async fn update_collaborator_role(
        &self,
        repo_id: &str,
        token: &str,
        user_id: &str,
        request: &UpdateCollaboratorRoleRequest,
    ) -> Result<(), CliError> {
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
            .json(request)
            .send()
            .await?;
        process_empty_response(response).await
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
        let response = req.send().await?;
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
            .json(request)
            .send()
            .await?;
        process_response_raw(response).await
    }

    pub async fn delete_repo(&self, repo_id: &str, token: &str) -> Result<(), CliError> {
        let url = format!("{}/api/v1/repos/{}", self.base_url, repo_id);
        let response = self.client.delete(&url).bearer_auth(token).send().await?;
        process_empty_response(response).await
    }

    pub async fn get_session(
        &self,
        token: &str,
    ) -> Result<(SessionResponse, serde_json::Value), CliError> {
        let url = format!("{}/api/auth/get-session", self.base_url);
        let response = self.client.get(&url).bearer_auth(token).send().await?;
        let response = check_response(response).await?;
        // Special handling: better-auth returns 200 with null when token is invalid.
        // Every body-read / parse / typed-deserialize failure on this endpoint
        // maps to AuthRequired (not the generic "invalid response body" surface),
        // preserving observable behavior on token-expiry for cmd_login / cmd_whoami.
        let bytes = response.bytes().await.map_err(|_| CliError::AuthRequired)?;
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
            .json(update)
            .send()
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
            .json(request)
            .send()
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
            .json(request)
            .send()
            .await?;
        process_response_raw(response).await
    }

    pub async fn list_teams(
        &self,
        token: &str,
    ) -> Result<(TeamListResponse, serde_json::Value), CliError> {
        let url = format!("{}/api/v1/teams", self.base_url);
        let response = self.client.get(&url).bearer_auth(token).send().await?;
        process_response_raw(response).await
    }

    pub async fn get_team(
        &self,
        token: &str,
        team_id: &str,
    ) -> Result<(TeamResponse, serde_json::Value), CliError> {
        let url = format!("{}/api/v1/teams/{}", self.base_url, team_id);
        let response = self.client.get(&url).bearer_auth(token).send().await?;
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
            .json(request)
            .send()
            .await?;
        process_response_raw(response).await
    }

    pub async fn delete_team(&self, token: &str, team_id: &str) -> Result<(), CliError> {
        let url = format!("{}/api/v1/teams/{}", self.base_url, team_id);
        let response = self.client.delete(&url).bearer_auth(token).send().await?;
        process_empty_response(response).await
    }

    // Member management

    pub async fn list_members(
        &self,
        token: &str,
        team_id: &str,
    ) -> Result<(TeamMembersResponse, serde_json::Value), CliError> {
        let url = format!("{}/api/v1/teams/{}/members", self.base_url, team_id);
        let response = self.client.get(&url).bearer_auth(token).send().await?;
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
            .json(request)
            .send()
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
            .json(request)
            .send()
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
        let response = self.client.delete(&url).bearer_auth(token).send().await?;
        process_empty_response(response).await
    }

    // Invitation flow

    pub async fn list_my_invitations(
        &self,
        token: &str,
    ) -> Result<(InvitationListResponse, serde_json::Value), CliError> {
        let url = format!("{}/api/v1/teams/invitations", self.base_url);
        let response = self.client.get(&url).bearer_auth(token).send().await?;
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
        let response = self.client.post(&url).bearer_auth(token).send().await?;
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
        let response = self.client.post(&url).bearer_auth(token).send().await?;
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
            .json(request)
            .send()
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
        let response = self.client.delete(&url).bearer_auth(token).send().await?;
        process_empty_response(response).await
    }

    pub async fn list_team_repos(
        &self,
        token: &str,
        team_id: &str,
    ) -> Result<(TeamReposResponse, serde_json::Value), CliError> {
        let url = format!("{}/api/v1/teams/{}/repos", self.base_url, team_id);
        let response = self.client.get(&url).bearer_auth(token).send().await?;
        process_response_raw(response).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use wiremock::matchers::{header, method, path, query_param, query_param_is_missing};
    use wiremock::{Mock, MockServer, ResponseTemplate};

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
}
