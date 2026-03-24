use std::collections::HashMap;
use std::fmt;
use std::time::Duration;

use reqwest::redirect;
use reqwest::Client;
use serde::{Deserialize, Serialize};

use crate::errors::CliError;

fn encode_path_param(value: &str) -> String {
    urlencoding::encode(value).into_owned()
}

pub(crate) fn encode_file_path(path: &str) -> String {
    path.split('/')
        .map(|seg| urlencoding::encode(seg))
        .collect::<Vec<_>>()
        .join("/")
}

#[derive(Deserialize)]
struct ApiErrorBody {
    error: String,
}

// --- Domain enums ---

#[derive(Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum RepoStatus {
    Active,
    Draft,
    Completed,
    Abandoned,
    #[serde(other)]
    Unknown,
}

impl Serialize for RepoStatus {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        match self {
            RepoStatus::Active => serializer.serialize_str("active"),
            RepoStatus::Draft => serializer.serialize_str("draft"),
            RepoStatus::Completed => serializer.serialize_str("completed"),
            RepoStatus::Abandoned => serializer.serialize_str("abandoned"),
            RepoStatus::Unknown => Err(serde::ser::Error::custom(
                "cannot serialize unknown RepoStatus variant",
            )),
        }
    }
}

impl fmt::Display for RepoStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            RepoStatus::Active => write!(f, "active"),
            RepoStatus::Draft => write!(f, "draft"),
            RepoStatus::Completed => write!(f, "completed"),
            RepoStatus::Abandoned => write!(f, "abandoned"),
            RepoStatus::Unknown => write!(f, "unknown"),
        }
    }
}

#[derive(Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Visibility {
    Public,
    Private,
    #[serde(other)]
    Unknown,
}

impl Serialize for Visibility {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        match self {
            Visibility::Public => serializer.serialize_str("public"),
            Visibility::Private => serializer.serialize_str("private"),
            Visibility::Unknown => Err(serde::ser::Error::custom(
                "cannot serialize unknown Visibility variant",
            )),
        }
    }
}

impl fmt::Display for Visibility {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Visibility::Public => write!(f, "public"),
            Visibility::Private => write!(f, "private"),
            Visibility::Unknown => write!(f, "unknown"),
        }
    }
}

#[derive(Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum EntryType {
    File,
    Dir,
    #[serde(other)]
    Unknown,
}

impl Serialize for EntryType {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        match self {
            EntryType::File => serializer.serialize_str("file"),
            EntryType::Dir => serializer.serialize_str("dir"),
            EntryType::Unknown => Err(serde::ser::Error::custom(
                "cannot serialize unknown EntryType variant",
            )),
        }
    }
}

impl fmt::Display for EntryType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            EntryType::File => write!(f, "file"),
            EntryType::Dir => write!(f, "dir"),
            EntryType::Unknown => write!(f, "unknown"),
        }
    }
}

#[derive(Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum DiffStatus {
    Added,
    Modified,
    Deleted,
    #[serde(other)]
    Unknown,
}

impl Serialize for DiffStatus {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        match self {
            DiffStatus::Added => serializer.serialize_str("added"),
            DiffStatus::Modified => serializer.serialize_str("modified"),
            DiffStatus::Deleted => serializer.serialize_str("deleted"),
            DiffStatus::Unknown => Err(serde::ser::Error::custom(
                "cannot serialize unknown DiffStatus variant",
            )),
        }
    }
}

impl fmt::Display for DiffStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            DiffStatus::Added => write!(f, "added"),
            DiffStatus::Modified => write!(f, "modified"),
            DiffStatus::Deleted => write!(f, "deleted"),
            DiffStatus::Unknown => write!(f, "unknown"),
        }
    }
}

#[derive(Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum CollaboratorRole {
    Owner,
    Admin,
    Write,
    Read,
    #[serde(other)]
    Unknown,
}

impl Serialize for CollaboratorRole {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        match self {
            CollaboratorRole::Owner => serializer.serialize_str("owner"),
            CollaboratorRole::Admin => serializer.serialize_str("admin"),
            CollaboratorRole::Write => serializer.serialize_str("write"),
            CollaboratorRole::Read => serializer.serialize_str("read"),
            CollaboratorRole::Unknown => Err(serde::ser::Error::custom(
                "cannot serialize unknown CollaboratorRole variant",
            )),
        }
    }
}

impl fmt::Display for CollaboratorRole {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            CollaboratorRole::Owner => write!(f, "owner"),
            CollaboratorRole::Admin => write!(f, "admin"),
            CollaboratorRole::Write => write!(f, "write"),
            CollaboratorRole::Read => write!(f, "read"),
            CollaboratorRole::Unknown => write!(f, "unknown"),
        }
    }
}

// --- Request types ---

#[derive(Serialize)]
pub struct PushRequest {
    pub files: HashMap<String, PushEntry>,
    pub delete: Vec<String>,
    pub message: String,
    pub author: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parent_sha: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
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
pub struct RepoUpdate {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
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

// --- Response types ---

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
    pub name: Option<String>,
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
    pub total: u32,
    pub limit: u32,
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
    pub email: String,
    pub image: Option<String>,
}

#[derive(Deserialize, Debug)]
pub struct RevertResponse {
    pub commit_sha: String,
    pub changed: bool,
}

pub type PullResponse = TreeResponse;

// --- Client ---

pub struct SynsClient {
    client: Client,
    base_url: String,
}

/// Check response for error status codes and return the appropriate CliError.
/// Returns `Ok(response)` if the status is successful.
async fn check_error_response(response: reqwest::Response) -> Result<reqwest::Response, CliError> {
    if response.status().is_success() {
        return Ok(response);
    }
    let status = response.status().as_u16();
    if status == 401 {
        return Err(CliError::AuthRequired);
    }
    let body = response.text().await.unwrap_or_default();
    let error = serde_json::from_str::<ApiErrorBody>(&body)
        .map(|b| b.error)
        .unwrap_or_else(|_| "unknown error".to_string());
    Err(CliError::Api { status, error })
}

async fn handle_response<T: serde::de::DeserializeOwned>(
    response: reqwest::Response,
) -> Result<T, CliError> {
    let response = check_error_response(response).await?;
    let status = response.status().as_u16();
    response.json::<T>().await.map_err(|e| CliError::Api {
        status,
        error: format!("invalid response body: {e}"),
    })
}

async fn handle_empty_response(response: reqwest::Response) -> Result<(), CliError> {
    check_error_response(response).await?;
    Ok(())
}

impl SynsClient {
    pub fn new(server_url: &str) -> Result<SynsClient, CliError> {
        let server_url = server_url.trim_end_matches('/');
        if !server_url.starts_with("https://") && !crate::config::is_localhost_url(server_url) {
            return Err(CliError::Config {
                message: "server URL must use HTTPS (or http://localhost for development)"
                    .to_string(),
            });
        }

        let client = Client::builder()
            .timeout(Duration::from_secs(30))
            .redirect(redirect::Policy::none())
            .build()
            .map_err(|e| CliError::Config {
                message: format!("failed to build HTTP client: {e}"),
            })?;

        Ok(SynsClient {
            client,
            base_url: server_url.to_string(),
        })
    }

    pub async fn push(
        &self,
        repo_id: &str,
        token: &str,
        request: &PushRequest,
    ) -> Result<PushResponse, CliError> {
        let url = format!(
            "{}/api/v1/repos/{}/push",
            self.base_url,
            encode_path_param(repo_id)
        );
        let response = self
            .client
            .put(&url)
            .bearer_auth(token)
            .json(request)
            .send()
            .await?;
        handle_response(response).await
    }

    pub async fn pull(
        &self,
        repo_id: &str,
        token: Option<&str>,
    ) -> Result<PullResponse, CliError> {
        let url = format!(
            "{}/api/v1/repos/{}/tree",
            self.base_url,
            encode_path_param(repo_id)
        );
        let mut req = self.client.get(&url);
        if let Some(t) = token {
            req = req.bearer_auth(t);
        }
        req = req.query(&[("recursive", "true")]);
        let response = req.send().await?;
        handle_response(response).await
    }

    pub async fn list_repos(
        &self,
        token: Option<&str>,
        search: Option<&str>,
        limit: u32,
        offset: u32,
    ) -> Result<RepoListResponse, CliError> {
        let url = format!("{}/api/v1/repos", self.base_url);
        let mut req = self.client.get(&url);
        if let Some(t) = token {
            req = req.bearer_auth(t);
        }
        req = req.query(&[("limit", limit.to_string()), ("offset", offset.to_string())]);
        if let Some(s) = search {
            req = req.query(&[("search", s)]);
        }
        let response = req.send().await?;
        handle_response(response).await
    }

    pub async fn get_repo(
        &self,
        repo_id: &str,
        token: Option<&str>,
    ) -> Result<RepoResponse, CliError> {
        let url = format!(
            "{}/api/v1/repos/{}",
            self.base_url,
            encode_path_param(repo_id)
        );
        let mut req = self.client.get(&url);
        if let Some(t) = token {
            req = req.bearer_auth(t);
        }
        let response = req.send().await?;
        handle_response(response).await
    }

    pub async fn get_tree(
        &self,
        repo_id: &str,
        token: Option<&str>,
        path: Option<&str>,
        recursive: bool,
    ) -> Result<TreeResponse, CliError> {
        let encoded_repo = encode_path_param(repo_id);
        let url = match path {
            Some(p) => format!(
                "{}/api/v1/repos/{}/tree/{}",
                self.base_url,
                encoded_repo,
                encode_file_path(p)
            ),
            None => format!("{}/api/v1/repos/{}/tree", self.base_url, encoded_repo),
        };
        let mut req = self.client.get(&url);
        if let Some(t) = token {
            req = req.bearer_auth(t);
        }
        if recursive {
            req = req.query(&[("recursive", "true")]);
        }
        let response = req.send().await?;
        handle_response(response).await
    }

    pub async fn get_file(
        &self,
        repo_id: &str,
        token: Option<&str>,
        path: &str,
        version_ref: Option<&str>,
    ) -> Result<FileResponse, CliError> {
        let url = format!(
            "{}/api/v1/repos/{}/files/{}",
            self.base_url,
            encode_path_param(repo_id),
            encode_file_path(path)
        );
        let mut req = self.client.get(&url);
        if let Some(t) = token {
            req = req.bearer_auth(t);
        }
        if let Some(r) = version_ref {
            req = req.query(&[("ref", r)]);
        }
        let response = req.send().await?;
        handle_response(response).await
    }

    pub async fn get_file_history(
        &self,
        repo_id: &str,
        token: Option<&str>,
        path: &str,
        limit: u32,
    ) -> Result<FileHistoryResponse, CliError> {
        let url = format!(
            "{}/api/v1/repos/{}/files/{}/history",
            self.base_url,
            encode_path_param(repo_id),
            encode_file_path(path)
        );
        let mut req = self.client.get(&url);
        if let Some(t) = token {
            req = req.bearer_auth(t);
        }
        req = req.query(&[("limit", limit.to_string())]);
        let response = req.send().await?;
        handle_response(response).await
    }

    pub async fn list_versions(
        &self,
        repo_id: &str,
        token: Option<&str>,
        limit: u32,
        offset: u32,
    ) -> Result<VersionListResponse, CliError> {
        let url = format!(
            "{}/api/v1/repos/{}/versions",
            self.base_url,
            encode_path_param(repo_id)
        );
        let mut req = self.client.get(&url);
        if let Some(t) = token {
            req = req.bearer_auth(t);
        }
        req = req.query(&[("limit", limit.to_string()), ("offset", offset.to_string())]);
        let response = req.send().await?;
        handle_response(response).await
    }

    pub async fn get_diff(
        &self,
        repo_id: &str,
        token: Option<&str>,
        from: &str,
        to: &str,
    ) -> Result<DiffResponse, CliError> {
        let url = format!(
            "{}/api/v1/repos/{}/diff",
            self.base_url,
            encode_path_param(repo_id)
        );
        let mut req = self.client.get(&url);
        if let Some(t) = token {
            req = req.bearer_auth(t);
        }
        req = req.query(&[("from", from), ("to", to)]);
        let response = req.send().await?;
        handle_response(response).await
    }

    pub async fn list_collaborators(
        &self,
        repo_id: &str,
        token: &str,
        limit: u32,
        offset: u32,
    ) -> Result<CollaboratorListResponse, CliError> {
        let url = format!(
            "{}/api/v1/repos/{}/collaborators",
            self.base_url,
            encode_path_param(repo_id)
        );
        let response = self
            .client
            .get(&url)
            .bearer_auth(token)
            .query(&[("limit", limit.to_string()), ("offset", offset.to_string())])
            .send()
            .await?;
        handle_response(response).await
    }

    pub async fn add_collaborator(
        &self,
        repo_id: &str,
        token: &str,
        request: &AddCollaboratorRequest,
    ) -> Result<(), CliError> {
        let url = format!(
            "{}/api/v1/repos/{}/collaborators",
            self.base_url,
            encode_path_param(repo_id)
        );
        let response = self
            .client
            .post(&url)
            .bearer_auth(token)
            .json(request)
            .send()
            .await?;
        handle_empty_response(response).await
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
            encode_path_param(repo_id),
            encode_path_param(user_id)
        );
        let response = self
            .client
            .delete(&url)
            .bearer_auth(token)
            .send()
            .await?;
        handle_empty_response(response).await
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
            encode_path_param(repo_id),
            encode_path_param(user_id)
        );
        let response = self
            .client
            .patch(&url)
            .bearer_auth(token)
            .json(request)
            .send()
            .await?;
        handle_empty_response(response).await
    }

    pub async fn explore(
        &self,
        query: Option<&str>,
        tag: Option<&str>,
        status: Option<&RepoStatus>,
        limit: u32,
        offset: u32,
    ) -> Result<ExploreResponse, CliError> {
        let url = format!("{}/api/v1/explore", self.base_url);
        let mut req = self.client.get(&url);
        req = req.query(&[("limit", limit.to_string()), ("offset", offset.to_string())]);
        if let Some(q) = query {
            req = req.query(&[("search", q)]);
        }
        if let Some(t) = tag {
            req = req.query(&[("tag", t)]);
        }
        if let Some(s) = status {
            req = req.query(&[("status", s.to_string())]);
        }
        let response = req.send().await?;
        handle_response(response).await
    }

    pub async fn fork(
        &self,
        repo_id: &str,
        token: &str,
    ) -> Result<ForkResponse, CliError> {
        let url = format!(
            "{}/api/v1/repos/{}/fork",
            self.base_url,
            encode_path_param(repo_id)
        );
        let response = self
            .client
            .post(&url)
            .bearer_auth(token)
            .json(&serde_json::json!({}))
            .send()
            .await?;
        handle_response(response).await
    }

    pub async fn delete_repo(
        &self,
        repo_id: &str,
        token: &str,
    ) -> Result<(), CliError> {
        let url = format!(
            "{}/api/v1/repos/{}",
            self.base_url,
            encode_path_param(repo_id)
        );
        let response = self
            .client
            .delete(&url)
            .bearer_auth(token)
            .send()
            .await?;
        handle_empty_response(response).await
    }

    pub async fn get_session(&self, token: &str) -> Result<SessionResponse, CliError> {
        let url = format!("{}/api/auth/get-session", self.base_url);
        let response = self
            .client
            .get(&url)
            .bearer_auth(token)
            .send()
            .await?;
        handle_response(response).await
    }

    pub async fn update_repo(
        &self,
        repo_id: &str,
        token: &str,
        update: &RepoUpdate,
    ) -> Result<RepoResponse, CliError> {
        let url = format!(
            "{}/api/v1/repos/{}",
            self.base_url,
            encode_path_param(repo_id)
        );
        let response = self
            .client
            .patch(&url)
            .bearer_auth(token)
            .json(update)
            .send()
            .await?;
        handle_response(response).await
    }

    pub async fn revert_file(
        &self,
        repo_id: &str,
        token: &str,
        path: &str,
        request: &RevertFileRequest,
    ) -> Result<RevertResponse, CliError> {
        let url = format!(
            "{}/api/v1/repos/{}/files/{}/revert",
            self.base_url,
            encode_path_param(repo_id),
            encode_file_path(path)
        );
        let response = self
            .client
            .post(&url)
            .bearer_auth(token)
            .json(request)
            .send()
            .await?;
        handle_response(response).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn client_new_rejects_plain_http() {
        let result = SynsClient::new("http://example.com");
        assert!(result.is_err());
    }

    #[test]
    fn client_new_allows_https() {
        let client = SynsClient::new("https://syns.dev").unwrap();
        assert_eq!(client.base_url, "https://syns.dev");
    }

    #[test]
    fn client_new_allows_localhost_http() {
        let client = SynsClient::new("http://localhost:3000").unwrap();
        assert_eq!(client.base_url, "http://localhost:3000");
    }

    #[test]
    fn client_new_strips_trailing_slash() {
        let client = SynsClient::new("https://syns.dev/").unwrap();
        assert_eq!(client.base_url, "https://syns.dev");
    }

    #[test]
    fn encode_file_path_simple() {
        assert_eq!(encode_file_path("foo/bar.txt"), "foo/bar.txt");
    }

    #[test]
    fn encode_file_path_encodes_segments() {
        assert_eq!(encode_file_path("my dir/my file.txt"), "my%20dir/my%20file.txt");
    }

    #[test]
    fn encode_file_path_preserves_slashes() {
        assert_eq!(encode_file_path("a/b/c"), "a/b/c");
    }

    #[test]
    fn serialize_unknown_repo_status_fails() {
        let result = serde_json::to_string(&RepoStatus::Unknown);
        assert!(result.is_err());
    }

    #[test]
    fn serialize_known_repo_status_works() {
        let result = serde_json::to_string(&RepoStatus::Active).unwrap();
        assert_eq!(result, "\"active\"");
    }

    #[test]
    fn deserialize_unknown_repo_status() {
        let status: RepoStatus = serde_json::from_str("\"archived\"").unwrap();
        assert_eq!(status, RepoStatus::Unknown);
    }

    #[test]
    fn serialize_unknown_visibility_fails() {
        let result = serde_json::to_string(&Visibility::Unknown);
        assert!(result.is_err());
    }

    #[test]
    fn serialize_unknown_entry_type_fails() {
        let result = serde_json::to_string(&EntryType::Unknown);
        assert!(result.is_err());
    }

    #[test]
    fn serialize_unknown_diff_status_fails() {
        let result = serde_json::to_string(&DiffStatus::Unknown);
        assert!(result.is_err());
    }

    #[test]
    fn serialize_unknown_collaborator_role_fails() {
        let result = serde_json::to_string(&CollaboratorRole::Unknown);
        assert!(result.is_err());
    }
}
