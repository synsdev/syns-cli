use std::path::PathBuf;

use clap::Args;

use crate::auth::token::TokenStore;
use crate::client::{PushResponse, SynsClient};
use crate::commands::repo::{CliRepoStatus, CliVisibility};
use crate::config::Config;
use crate::errors::CliError;
use crate::output::Output;
use crate::push::smart::{smart_push, SmartPushOptions};
use crate::repo::resolve::resolve_repo_identity;

const DEFAULT_COMMIT_MESSAGE: &str = "push";
const SHORT_SHA_LENGTH: usize = 8;

#[derive(Args, Debug)]
pub struct PushArgs {
    /// Override repository name
    #[arg(long, short = 'n')]
    pub name: Option<String>,

    /// Commit message
    #[arg(long, short = 'm')]
    pub message: Option<String>,

    /// Send all files, bypassing manifest diffing
    #[arg(long, short = 'f')]
    pub force: bool,

    /// Glob patterns to exclude from push (repeatable)
    #[arg(long, short = 'e')]
    pub exclude: Vec<String>,

    /// Set repository description
    #[arg(long)]
    pub description: Option<String>,

    /// Set repository tags (repeatable)
    #[arg(long, short = 't')]
    pub tag: Vec<String>,

    /// Set repository status
    #[arg(long)]
    pub status: Option<CliRepoStatus>,

    /// Set repository visibility
    #[arg(long)]
    pub visibility: Option<CliVisibility>,

    /// Directory to push (defaults to current directory)
    #[arg(value_name = "PATH")]
    pub path: Option<PathBuf>,
}

async fn resolve_owner(
    token_store: &TokenStore,
    client: &SynsClient,
    token: &str,
) -> Result<String, CliError> {
    if let Some(username) = token_store.read_username()? {
        return Ok(username);
    }
    let session = client.get_session(token).await?;
    Ok(session.user.username)
}

pub async fn cmd_push(
    config: &Config,
    output: &Output,
    args: &PushArgs,
) -> Result<(), CliError> {
    let push_path = match &args.path {
        Some(p) => p.clone(),
        None => std::env::current_dir().map_err(|e| CliError::Io {
            message: format!("could not determine current directory: {e}"),
        })?,
    };

    let token_store = TokenStore::new(config.credentials_path());
    let token = token_store.read()?.ok_or(CliError::AuthRequired)?;

    let identity = resolve_repo_identity(args.name.as_deref(), &push_path)?;

    let (owner, client) = if let Some(owner) = identity.owner {
        (owner, None)
    } else {
        let c = SynsClient::new(config.server_url())?;
        let owner = resolve_owner(&token_store, &c, &token).await?;
        (owner, Some(c))
    };

    let repo_id = format!("{owner}/{}", identity.name);

    let status = args.status.clone().map(Into::into);
    let visibility = args.visibility.clone().map(Into::into);

    let message = args
        .message
        .clone()
        .unwrap_or_else(|| DEFAULT_COMMIT_MESSAGE.to_string());

    let opts = SmartPushOptions {
        force: args.force,
        message,
        author: owner.clone(),
        parent_sha: None,
        excludes: args.exclude.clone(),
        cache_dir: config.cache_dir().to_path_buf(),
        description: args.description.clone(),
        tags: if args.tag.is_empty() {
            None
        } else {
            Some(args.tag.clone())
        },
        status,
        visibility,
    };

    let client = match client {
        Some(c) => c,
        None => SynsClient::new(config.server_url())?,
    };

    let response = smart_push(&client, &token, &repo_id, &push_path, opts).await?;

    format_response(output, &response, &repo_id);

    Ok(())
}

fn format_response(output: &Output, response: &PushResponse, repo_id: &str) {
    if output.is_json() {
        output.json(&serde_json::json!({
            "commitSha": response.commit_sha,
            "version": response.version,
            "filesChanged": response.files_changed,
            "created": response.created,
        }));
    } else if response.files_changed > 0 || response.created {
        output.success(&format!("Pushed to {repo_id}"));
        output.table(
            &["", ""],
            vec![
                vec![
                    "commit".into(),
                    response.commit_sha[..response.commit_sha.len().min(SHORT_SHA_LENGTH)].into(),
                ],
                vec!["version".into(), response.version.to_string()],
                vec!["files changed".into(), response.files_changed.to_string()],
            ],
        );
    } else {
        output.success(&format!("No changes — {repo_id} is up to date"));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serial_test::serial;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn default_push_args() -> PushArgs {
        PushArgs {
            name: None,
            message: None,
            force: false,
            exclude: vec![],
            description: None,
            tag: vec![],
            status: None,
            visibility: None,
            path: None,
        }
    }

    #[tokio::test]
    #[serial]
    async fn push_with_name_flag_resolves_owner_from_cached_username() {
        let mock_server = MockServer::start().await;

        Mock::given(method("GET"))
            .and(path("/api/v1/repos/alice/new-repo/tree"))
            .respond_with(
                ResponseTemplate::new(404)
                    .set_body_json(serde_json::json!({"error": "not_found"})),
            )
            .mount(&mock_server)
            .await;

        Mock::given(method("PUT"))
            .and(path("/api/v1/repos/alice/new-repo/push"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "commitSha": "abc12345def67890",
                "version": 1,
                "filesChanged": 1,
                "created": true
            })))
            .expect(1)
            .mount(&mock_server)
            .await;

        let temp_dir = tempfile::tempdir().unwrap();
        std::fs::write(temp_dir.path().join("hello.txt"), "hello").unwrap();

        unsafe { std::env::set_var("SYNS_CONFIG_DIR", temp_dir.path()) };
        let config = Config::new(Some(&mock_server.uri())).unwrap();
        let output = Output::new(false);

        let token_store = TokenStore::new(config.credentials_path());
        token_store
            .write_with_username("test-token", Some("alice"))
            .unwrap();

        let args = PushArgs {
            name: Some("new-repo".into()),
            path: Some(temp_dir.path().into()),
            ..default_push_args()
        };

        let result = cmd_push(&config, &output, &args).await;
        unsafe { std::env::remove_var("SYNS_CONFIG_DIR") };

        assert!(result.is_ok());
    }

    #[tokio::test]
    #[serial]
    async fn push_with_force_flag() {
        let mock_server = MockServer::start().await;

        Mock::given(method("PUT"))
            .and(path("/api/v1/repos/carol/repo/push"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "commitSha": "def45678abc12345",
                "version": 2,
                "filesChanged": 0,
                "created": false
            })))
            .expect(1)
            .mount(&mock_server)
            .await;

        let temp_dir = tempfile::tempdir().unwrap();
        std::fs::write(temp_dir.path().join("file.txt"), "content").unwrap();
        std::fs::write(
            temp_dir.path().join(".syns.yaml"),
            "owner: carol\nname: repo\n",
        )
        .unwrap();

        unsafe { std::env::set_var("SYNS_CONFIG_DIR", temp_dir.path()) };
        let config = Config::new(Some(&mock_server.uri())).unwrap();
        let output = Output::new(false);

        let token_store = TokenStore::new(config.credentials_path());
        token_store
            .write_with_username("test-token", Some("carol"))
            .unwrap();

        let args = PushArgs {
            force: true,
            path: Some(temp_dir.path().into()),
            ..default_push_args()
        };

        let result = cmd_push(&config, &output, &args).await;
        unsafe { std::env::remove_var("SYNS_CONFIG_DIR") };

        assert!(result.is_ok());
    }

    #[tokio::test]
    #[serial]
    async fn push_with_metadata_flags() {
        let mock_server = MockServer::start().await;

        Mock::given(method("PUT"))
            .and(path("/api/v1/repos/dave/project/push"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "commitSha": "meta1234abcd5678",
                "version": 1,
                "filesChanged": 1,
                "created": true
            })))
            .expect(1)
            .mount(&mock_server)
            .await;

        let temp_dir = tempfile::tempdir().unwrap();
        std::fs::write(temp_dir.path().join("main.rs"), "fn main() {}").unwrap();
        std::fs::write(
            temp_dir.path().join(".syns.yaml"),
            "owner: dave\nname: project\n",
        )
        .unwrap();

        unsafe { std::env::set_var("SYNS_CONFIG_DIR", temp_dir.path()) };
        let config = Config::new(Some(&mock_server.uri())).unwrap();
        let output = Output::new(false);

        let token_store = TokenStore::new(config.credentials_path());
        token_store
            .write_with_username("test-token", Some("dave"))
            .unwrap();

        let args = PushArgs {
            description: Some("My project description".into()),
            tag: vec!["rust".into(), "cli".into()],
            status: Some(CliRepoStatus::Active),
            visibility: Some(CliVisibility::Public),
            path: Some(temp_dir.path().into()),
            ..default_push_args()
        };

        let result = cmd_push(&config, &output, &args).await;
        unsafe { std::env::remove_var("SYNS_CONFIG_DIR") };

        assert!(result.is_ok());

        let requests = mock_server.received_requests().await.unwrap();
        let put_request = requests
            .iter()
            .find(|r| r.method == reqwest::Method::PUT)
            .unwrap();
        let body: serde_json::Value = serde_json::from_slice(&put_request.body).unwrap();

        assert_eq!(
            body["description"].as_str(),
            Some("My project description")
        );
        assert_eq!(
            body["tags"],
            serde_json::json!(["rust", "cli"])
        );
        assert_eq!(body["status"].as_str(), Some("active"));
        assert_eq!(body["visibility"].as_str(), Some("public"));
    }

    #[tokio::test]
    #[serial]
    async fn push_requires_authentication() {
        let temp_dir = tempfile::tempdir().unwrap();

        unsafe { std::env::set_var("SYNS_CONFIG_DIR", temp_dir.path()) };
        let config = Config::new(Some("https://syns.dev")).unwrap();
        let output = Output::new(false);

        let args = PushArgs {
            path: Some(temp_dir.path().into()),
            ..default_push_args()
        };

        let result = cmd_push(&config, &output, &args).await;
        unsafe { std::env::remove_var("SYNS_CONFIG_DIR") };

        assert!(matches!(result, Err(CliError::AuthRequired)));
    }

    #[tokio::test]
    #[serial]
    async fn push_no_changes_returns_ok() {
        let mock_server = MockServer::start().await;

        Mock::given(method("PUT"))
            .and(path("/api/v1/repos/eve/repo/push"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "commitSha": "same1234same5678",
                "version": 1,
                "filesChanged": 0,
                "created": false
            })))
            .mount(&mock_server)
            .await;

        let temp_dir = tempfile::tempdir().unwrap();
        std::fs::write(temp_dir.path().join("file.txt"), "unchanged").unwrap();
        std::fs::write(
            temp_dir.path().join(".syns.yaml"),
            "owner: eve\nname: repo\n",
        )
        .unwrap();

        unsafe { std::env::set_var("SYNS_CONFIG_DIR", temp_dir.path()) };
        let config = Config::new(Some(&mock_server.uri())).unwrap();
        let output = Output::new(false);

        let token_store = TokenStore::new(config.credentials_path());
        token_store
            .write_with_username("test-token", Some("eve"))
            .unwrap();

        let args = PushArgs {
            force: true,
            path: Some(temp_dir.path().into()),
            ..default_push_args()
        };

        let result = cmd_push(&config, &output, &args).await;
        unsafe { std::env::remove_var("SYNS_CONFIG_DIR") };

        assert!(result.is_ok());
    }
}
