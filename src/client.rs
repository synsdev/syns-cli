#![allow(dead_code)] // Types and methods used by downstream units (U10, U11, U21+)

use std::collections::HashMap;

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
    pub fn as_query_str(&self) -> &str {
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

// --- Request Types ---

#[derive(Serialize)]
pub struct PushRequest {
    pub files: HashMap<String, PushEntry>,
    pub delete: Vec<String>,
    pub message: String,
    pub author: String,
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

#[derive(Serialize)]
pub struct PushEntry {
    pub sha: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub content: Option<String>,
}

#[derive(Serialize)]
pub struct ForkRequest {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub target_name: Option<String>,
}

#[derive(Serialize)]
pub struct RepoUpdate {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub status: Option<RepoStatus>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub visibility: Option<Visibility>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tags: Option<Vec<String>>,
}

#[derive(Serialize)]
pub struct AddCollaboratorRequest {
    pub user_id: String,
    pub role: CollaboratorRole,
}

#[derive(Serialize)]
pub struct UpdateCollaboratorRoleRequest {
    pub role: CollaboratorRole,
}

#[derive(Serialize)]
pub struct RevertFileRequest {
    pub to: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
}

// --- Response Types ---

#[derive(Deserialize, Debug)]
pub struct PushResponse {
    pub commit_sha: String,
    pub added: u32,
    pub updated: u32,
    pub deleted: u32,
    pub file_count: u32,
    pub changed: bool,
}

#[derive(Deserialize, Debug)]
pub struct TreeResponse {
    pub entries: Vec<TreeEntry>,
    pub commit_sha: String,
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
pub struct FileHistoryResponse {
    pub commits: Vec<FileVersionEntry>,
    #[serde(default)]
    pub total: u32,
    #[serde(default)]
    pub limit: u32,
    #[serde(default)]
    pub offset: u32,
}

#[derive(Deserialize, Debug)]
pub struct FileVersionEntry {
    pub sha: String,
    pub message: String,
    pub author: String,
    pub timestamp: String,
    pub files_changed: Vec<String>,
    pub file_content: String,
    pub file_sha: String,
    pub file_diff: Option<String>,
}

#[derive(Deserialize, Serialize, Debug)]
pub struct RepoResponse {
    pub id: String,
    pub name: String,
    pub description: Option<String>,
    pub owner_id: String,
    pub status: RepoStatus,
    pub visibility: Visibility,
    pub tags: Vec<String>,
    pub commit_sha: Option<String>,
    pub file_count: u32,
    pub fork_count: u32,
    pub forked_from: Option<String>,
    pub created_at: String,
    pub updated_at: String,
}

#[derive(Deserialize, Debug)]
pub struct RepoListResponse {
    pub data: Vec<RepoResponse>,
    pub total: u32,
    pub limit: u32,
    pub offset: u32,
}

#[derive(Deserialize, Debug)]
pub struct VersionListResponse {
    pub data: Vec<VersionEntry>,
    pub total: u32,
    pub limit: u32,
    pub offset: u32,
}

#[derive(Deserialize, Debug)]
pub struct VersionEntry {
    pub version: u32,
    pub sha: String,
    pub message: String,
    pub author: String,
    pub timestamp: String,
    pub files_changed: Vec<String>,
}

#[derive(Deserialize, Debug)]
pub struct DiffResponse {
    pub from_sha: String,
    pub to_sha: String,
    pub entries: Vec<DiffEntry>,
}

#[derive(Deserialize, Debug)]
pub struct DiffEntry {
    pub path: String,
    pub status: DiffStatus,
    pub diff: String,
}

#[derive(Deserialize, Debug)]
pub struct CollaboratorListResponse {
    pub collaborators: Vec<Collaborator>,
    #[serde(default)]
    pub total: u32,
    #[serde(default)]
    pub limit: u32,
    #[serde(default)]
    pub offset: u32,
}

#[derive(Deserialize, Debug)]
pub struct Collaborator {
    pub user_id: String,
    pub name: String,
    pub email: String,
    pub role: CollaboratorRole,
}

#[derive(Deserialize, Debug)]
pub struct ExploreResponse {
    pub data: Vec<RepoResponse>,
    pub total: u32,
    pub limit: u32,
    pub offset: u32,
}

#[derive(Deserialize, Debug)]
pub struct ForkResponse {
    pub id: String,
    pub commit_sha: String,
    pub file_count: u32,
    pub commit_count: u32,
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

#[derive(Deserialize, Debug)]
pub struct RevertResponse {
    pub commit_sha: String,
    pub changed: bool,
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
        return Err(CliError::Api { status: Some(code), error });
    }
    if !status.is_success() {
        return Err(CliError::Api {
            status: Some(status.as_u16()),
            error: format!("unexpected status {}", status.as_u16()),
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
    })
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
                message: "HTTPS required for server URL (http://localhost permitted for development)".to_string(),
            });
        }

        let base_url = server_url.trim_end_matches('/').to_string();

        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(30))
            .redirect(Policy::none())
            .build()
            .map_err(|e| CliError::Config { message: e.to_string() })?;

        Ok(SynsClient { client, base_url })
    }

    pub async fn push(&self, repo_id: &str, token: &str, request: &PushRequest) -> Result<PushResponse, CliError> {
        let url = format!("{}/api/v1/repos/{}/push", self.base_url, repo_id);
        let response = self.client.put(&url).bearer_auth(token).json(request).send().await?;
        process_response(response).await
    }

    pub async fn pull(&self, repo_id: &str, token: Option<&str>) -> Result<PullResponse, CliError> {
        let url = format!("{}/api/v1/repos/{}/tree", self.base_url, repo_id);
        let mut req = self.client.get(&url).query(&[("recursive", "true")]);
        if let Some(t) = token { req = req.bearer_auth(t); }
        let response = req.send().await?;
        process_response(response).await
    }

    pub async fn list_repos(&self, token: Option<&str>, search: Option<&str>, limit: u32, offset: u32) -> Result<RepoListResponse, CliError> {
        let url = format!("{}/api/v1/repos", self.base_url);
        let mut req = self.client.get(&url)
            .query(&[("limit", limit.to_string()), ("offset", offset.to_string())]);
        if let Some(q) = search { req = req.query(&[("search", q)]); }
        if let Some(t) = token { req = req.bearer_auth(t); }
        let response = req.send().await?;
        process_response(response).await
    }

    pub async fn get_repo(&self, repo_id: &str, token: Option<&str>) -> Result<RepoResponse, CliError> {
        let url = format!("{}/api/v1/repos/{}", self.base_url, repo_id);
        let mut req = self.client.get(&url);
        if let Some(t) = token { req = req.bearer_auth(t); }
        let response = req.send().await?;
        process_response(response).await
    }

    pub async fn get_tree(&self, repo_id: &str, token: Option<&str>, path: Option<&str>, recursive: bool) -> Result<TreeResponse, CliError> {
        let url = match path {
            Some(p) => format!("{}/api/v1/repos/{}/tree/{}", self.base_url, repo_id, encode_path_segments(p)),
            None => format!("{}/api/v1/repos/{}/tree", self.base_url, repo_id),
        };
        let mut req = self.client.get(&url);
        if recursive { req = req.query(&[("recursive", "true")]); }
        if let Some(t) = token { req = req.bearer_auth(t); }
        let response = req.send().await?;
        process_response(response).await
    }

    pub async fn get_file(&self, repo_id: &str, token: Option<&str>, path: &str, version_ref: Option<&str>) -> Result<FileResponse, CliError> {
        let url = format!("{}/api/v1/repos/{}/files/{}", self.base_url, repo_id, encode_path_segments(path));
        let mut req = self.client.get(&url);
        if let Some(r) = version_ref { req = req.query(&[("ref", r)]); }
        if let Some(t) = token { req = req.bearer_auth(t); }
        let response = req.send().await?;
        process_response(response).await
    }

    pub async fn get_file_history(&self, repo_id: &str, token: Option<&str>, path: &str, limit: u32) -> Result<FileHistoryResponse, CliError> {
        let url = format!("{}/api/v1/repos/{}/files/{}/history", self.base_url, repo_id, encode_path_segments(path));
        let mut req = self.client.get(&url).query(&[("limit", limit.to_string())]);
        if let Some(t) = token { req = req.bearer_auth(t); }
        let response = req.send().await?;
        process_response(response).await
    }

    pub async fn list_versions(&self, repo_id: &str, token: Option<&str>, limit: u32, offset: u32) -> Result<VersionListResponse, CliError> {
        let url = format!("{}/api/v1/repos/{}/versions", self.base_url, repo_id);
        let mut req = self.client.get(&url)
            .query(&[("limit", limit.to_string()), ("offset", offset.to_string())]);
        if let Some(t) = token { req = req.bearer_auth(t); }
        let response = req.send().await?;
        process_response(response).await
    }

    pub async fn get_diff(&self, repo_id: &str, token: Option<&str>, from: &str, to: &str) -> Result<DiffResponse, CliError> {
        let url = format!("{}/api/v1/repos/{}/diff", self.base_url, repo_id);
        let mut req = self.client.get(&url).query(&[("from", from), ("to", to)]);
        if let Some(t) = token { req = req.bearer_auth(t); }
        let response = req.send().await?;
        process_response(response).await
    }

    pub async fn list_collaborators(&self, repo_id: &str, token: &str, limit: u32, offset: u32) -> Result<CollaboratorListResponse, CliError> {
        let url = format!("{}/api/v1/repos/{}/collaborators", self.base_url, repo_id);
        let response = self.client.get(&url)
            .bearer_auth(token)
            .query(&[("limit", limit.to_string()), ("offset", offset.to_string())])
            .send().await?;
        process_response(response).await
    }

    pub async fn add_collaborator(&self, repo_id: &str, token: &str, request: &AddCollaboratorRequest) -> Result<(), CliError> {
        let url = format!("{}/api/v1/repos/{}/collaborators", self.base_url, repo_id);
        let response = self.client.post(&url).bearer_auth(token).json(request).send().await?;
        process_empty_response(response).await
    }

    pub async fn remove_collaborator(&self, repo_id: &str, token: &str, user_id: &str) -> Result<(), CliError> {
        let url = format!("{}/api/v1/repos/{}/collaborators/{}", self.base_url, repo_id, urlencoding::encode(user_id));
        let response = self.client.delete(&url).bearer_auth(token).send().await?;
        process_empty_response(response).await
    }

    pub async fn update_collaborator_role(&self, repo_id: &str, token: &str, user_id: &str, request: &UpdateCollaboratorRoleRequest) -> Result<(), CliError> {
        let url = format!("{}/api/v1/repos/{}/collaborators/{}", self.base_url, repo_id, urlencoding::encode(user_id));
        let response = self.client.patch(&url).bearer_auth(token).json(request).send().await?;
        process_empty_response(response).await
    }

    pub async fn explore(&self, query: Option<&str>, tag: Option<&str>, status: Option<&RepoStatus>, limit: u32, offset: u32) -> Result<ExploreResponse, CliError> {
        let url = format!("{}/api/v1/explore", self.base_url);
        let mut req = self.client.get(&url)
            .query(&[("limit", limit.to_string()), ("offset", offset.to_string())]);
        if let Some(q) = query { req = req.query(&[("search", q)]); }
        if let Some(t) = tag { req = req.query(&[("tag", t)]); }
        if let Some(s) = status {
            req = req.query(&[("status", s.as_query_str())]);
        }
        let response = req.send().await?;
        process_response(response).await
    }

    pub async fn fork(&self, repo_id: &str, token: &str, request: &ForkRequest) -> Result<ForkResponse, CliError> {
        let url = format!("{}/api/v1/repos/{}/fork", self.base_url, repo_id);
        let response = self.client.post(&url).bearer_auth(token).json(request).send().await?;
        process_response(response).await
    }

    pub async fn delete_repo(&self, repo_id: &str, token: &str) -> Result<(), CliError> {
        let url = format!("{}/api/v1/repos/{}", self.base_url, repo_id);
        let response = self.client.delete(&url).bearer_auth(token).send().await?;
        process_empty_response(response).await
    }

    pub async fn get_session(&self, token: &str) -> Result<SessionResponse, CliError> {
        let url = format!("{}/api/auth/get-session", self.base_url);
        let response = self.client.get(&url).bearer_auth(token).send().await?;
        let response = check_response(response).await?;
        // Special handling: better-auth returns 200 with null when token is invalid
        response.json::<SessionResponse>().await.map_err(|_| CliError::AuthRequired)
    }

    pub async fn update_repo(&self, repo_id: &str, token: &str, update: &RepoUpdate) -> Result<RepoResponse, CliError> {
        let url = format!("{}/api/v1/repos/{}", self.base_url, repo_id);
        let response = self.client.patch(&url).bearer_auth(token).json(update).send().await?;
        process_response(response).await
    }

    pub async fn revert_file(&self, repo_id: &str, token: &str, path: &str, request: &RevertFileRequest) -> Result<RevertResponse, CliError> {
        let url = format!("{}/api/v1/repos/{}/files/{}/revert", self.base_url, repo_id, encode_path_segments(path));
        let response = self.client.post(&url).bearer_auth(token).json(request).send().await?;
        process_response(response).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
        assert_eq!(serde_json::from_str::<RepoStatus>("\"active\"").unwrap(), RepoStatus::Active);
        assert_eq!(serde_json::from_str::<RepoStatus>("\"draft\"").unwrap(), RepoStatus::Draft);
        assert_eq!(serde_json::from_str::<RepoStatus>("\"completed\"").unwrap(), RepoStatus::Completed);
        assert_eq!(serde_json::from_str::<RepoStatus>("\"abandoned\"").unwrap(), RepoStatus::Abandoned);
    }

    #[test]
    fn repo_status_deserializes_unknown_to_unknown() {
        assert_eq!(serde_json::from_str::<RepoStatus>("\"archived\"").unwrap(), RepoStatus::Unknown);
        assert_eq!(serde_json::from_str::<RepoStatus>("\"some_future_status\"").unwrap(), RepoStatus::Unknown);
    }

    #[test]
    fn repo_status_serializes() {
        assert_eq!(serde_json::to_string(&RepoStatus::Active).unwrap(), "\"active\"");
        assert_eq!(serde_json::to_string(&RepoStatus::Draft).unwrap(), "\"draft\"");
        assert_eq!(serde_json::to_string(&RepoStatus::Completed).unwrap(), "\"completed\"");
        assert_eq!(serde_json::to_string(&RepoStatus::Abandoned).unwrap(), "\"abandoned\"");
        assert_eq!(serde_json::to_string(&RepoStatus::Unknown).unwrap(), "\"unknown\"");
    }

    #[test]
    fn visibility_serializes() {
        assert_eq!(serde_json::to_string(&Visibility::Public).unwrap(), "\"public\"");
        assert_eq!(serde_json::to_string(&Visibility::Private).unwrap(), "\"private\"");
        assert_eq!(serde_json::to_string(&Visibility::Unknown).unwrap(), "\"unknown\"");
    }

    #[test]
    fn other_enums_roundtrip() {
        assert_eq!(serde_json::from_str::<Visibility>("\"public\"").unwrap(), Visibility::Public);
        assert_eq!(serde_json::from_str::<Visibility>("\"private\"").unwrap(), Visibility::Private);
        assert_eq!(serde_json::from_str::<Visibility>("\"future\"").unwrap(), Visibility::Unknown);
        assert_eq!(serde_json::from_str::<EntryType>("\"file\"").unwrap(), EntryType::File);
        assert_eq!(serde_json::from_str::<EntryType>("\"dir\"").unwrap(), EntryType::Dir);
        assert_eq!(serde_json::from_str::<EntryType>("\"unknown_type\"").unwrap(), EntryType::Unknown);
        assert_eq!(serde_json::from_str::<DiffStatus>("\"added\"").unwrap(), DiffStatus::Added);
        assert_eq!(serde_json::from_str::<DiffStatus>("\"modified\"").unwrap(), DiffStatus::Modified);
        assert_eq!(serde_json::from_str::<DiffStatus>("\"deleted\"").unwrap(), DiffStatus::Deleted);
        assert_eq!(serde_json::from_str::<CollaboratorRole>("\"owner\"").unwrap(), CollaboratorRole::Owner);
        assert_eq!(serde_json::from_str::<CollaboratorRole>("\"admin\"").unwrap(), CollaboratorRole::Admin);
        assert_eq!(serde_json::from_str::<CollaboratorRole>("\"write\"").unwrap(), CollaboratorRole::Write);
        assert_eq!(serde_json::from_str::<CollaboratorRole>("\"read\"").unwrap(), CollaboratorRole::Read);
        assert_eq!(serde_json::from_str::<CollaboratorRole>("\"superadmin\"").unwrap(), CollaboratorRole::Unknown);
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
        assert_eq!(encode_path_segments("dir/sub dir/file #2.txt"), "dir/sub%20dir/file%20%232.txt");
    }

    #[test]
    fn encode_path_filters_empty_segments() {
        assert_eq!(encode_path_segments("/src/main.rs"), "src/main.rs");
        assert_eq!(encode_path_segments("src//main.rs"), "src/main.rs");
        assert_eq!(encode_path_segments("src/main.rs/"), "src/main.rs");
    }
}
