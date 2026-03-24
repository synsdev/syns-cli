use std::collections::HashMap;
use std::time::Duration;

use reqwest::redirect;
use reqwest::Client;
use serde::{Deserialize, Serialize};

use crate::errors::CliError;

fn encode_path_param(value: &str) -> String {
    urlencoding::encode(value).into_owned()
}

fn encode_file_path(path: &str) -> String {
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

#[derive(Deserialize, Serialize, Debug, Clone, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum RepoStatus {
    Active,
    Draft,
    Completed,
    Abandoned,
}

#[derive(Deserialize, Serialize, Debug, Clone, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Visibility {
    Public,
    Private,
}

#[derive(Deserialize, Serialize, Debug, Clone, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum EntryType {
    File,
    Dir,
}

#[derive(Deserialize, Serialize, Debug, Clone, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum DiffStatus {
    Added,
    Modified,
    Deleted,
}

#[derive(Deserialize, Serialize, Debug, Clone, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum CollaboratorRole {
    Owner,
    Admin,
    Write,
    Read,
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
    response.json::<T>().await.map_err(|_| CliError::Api {
        status,
        error: "invalid response body".to_string(),
    })
}

async fn handle_empty_response(response: reqwest::Response) -> Result<(), CliError> {
    check_error_response(response).await?;
    Ok(())
}

impl SynsClient {
    pub fn new(server_url: &str) -> SynsClient {
        SynsClient {
            client: Client::builder()
                .timeout(Duration::from_secs(30))
                .redirect(redirect::Policy::none())
                .build()
                .expect("failed to build HTTP client"),
            base_url: server_url.to_string(),
        }
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
        status: Option<&str>,
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
            req = req.query(&[("status", s)]);
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
