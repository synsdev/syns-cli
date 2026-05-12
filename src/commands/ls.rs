use crate::auth::token::TokenStore;
use crate::client::{EntryType, SynsClient};
use crate::config::Config;
use crate::errors::CliError;
use crate::output::Output;
use crate::repo::resolve::resolve_repo_identity;
use serde_json::json;

pub async fn cmd_ls(
    config: &Config,
    output: &Output,
    path: Option<String>,
) -> Result<(), CliError> {
    let current_dir = std::env::current_dir().map_err(|e| CliError::Io {
        message: format!("could not determine current directory: {e}"),
    })?;
    let identity = resolve_repo_identity(None, &current_dir)?;
    let owner = identity.owner.ok_or(CliError::RepoIdentityUnknown)?;
    let repo_id = format!("{}/{}", owner, identity.name);
    let token = TokenStore::new(config.credentials_path())
        .read()
        .ok()
        .flatten();
    let client = SynsClient::new(config.server_url())?;

    let mut response = match client
        .get_tree(&repo_id, token.as_deref(), path.as_deref(), false, None)
        .await
    {
        Ok(r) => r,
        Err(e) => {
            if !output.is_json()
                && let Some(p) = path.as_ref()
            {
                return Err(e.with_ls_path_context(p.clone()));
            }
            return Err(e);
        }
    };

    response.entries.sort_by(|a, b| {
        let type_order = |t: &EntryType| match t {
            EntryType::Dir => 0,
            EntryType::File => 1,
            EntryType::Unknown => 2,
        };
        type_order(&a.entry_type)
            .cmp(&type_order(&b.entry_type))
            .then_with(|| a.name.cmp(&b.name))
    });

    if output.is_json() {
        let entries: Vec<_> = response
            .entries
            .iter()
            .map(|e| {
                json!({
                    "name": e.name,
                    "path": e.path,
                    "type": match e.entry_type {
                        EntryType::File => "file",
                        EntryType::Dir => "dir",
                        EntryType::Unknown => "unknown",
                    },
                    "size": e.size,
                    "sha": e.sha,
                })
            })
            .collect();
        output.json(&json!({ "entries": entries, "commitSha": response.commit_sha }));
    } else {
        let rows: Vec<Vec<String>> = response
            .entries
            .iter()
            .map(|e| {
                vec![
                    e.name.clone(),
                    match e.entry_type {
                        EntryType::File => "file",
                        EntryType::Dir => "dir",
                        EntryType::Unknown => "unknown",
                    }
                    .to_string(),
                    match e.size {
                        Some(n) => n.to_string(),
                        None => "-".to_string(),
                    },
                ]
            })
            .collect();
        output.table(&["Name", "Type", "Size"], rows);
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serial_test::serial;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    #[tokio::test]
    #[serial]
    async fn ls_displays_sorted_entries() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join(".syns.yaml"),
            "owner: alice\nname: my-project\n",
        )
        .unwrap();
        std::env::set_current_dir(dir.path()).unwrap();
        unsafe { std::env::set_var("SYNS_CONFIG_DIR", dir.path()) };

        let mock_server = MockServer::start().await;

        Mock::given(method("GET"))
            .and(path("/api/v1/repos/alice/my-project/tree"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "entries": [
                    { "name": "README.md", "path": "README.md", "type": "file", "size": 256, "sha": "abc123" },
                    { "name": "src", "path": "src", "type": "dir", "size": null, "sha": null }
                ],
                "commitSha": "def456",
                "truncated": false
            })))
            .mount(&mock_server)
            .await;

        let config = Config::new(Some(&mock_server.uri())).unwrap();
        let output = Output::new(false);

        let result = cmd_ls(&config, &output, None).await;
        unsafe { std::env::remove_var("SYNS_CONFIG_DIR") };

        assert!(result.is_ok());
    }

    #[tokio::test]
    #[serial]
    async fn ls_json_output() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join(".syns.yaml"),
            "owner: alice\nname: my-project\n",
        )
        .unwrap();
        std::env::set_current_dir(dir.path()).unwrap();
        unsafe { std::env::set_var("SYNS_CONFIG_DIR", dir.path()) };

        let mock_server = MockServer::start().await;

        Mock::given(method("GET"))
            .and(path("/api/v1/repos/alice/my-project/tree"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "entries": [
                    { "name": "README.md", "path": "README.md", "type": "file", "size": 256, "sha": "abc123" },
                    { "name": "src", "path": "src", "type": "dir", "size": null, "sha": null }
                ],
                "commitSha": "def456",
                "truncated": false
            })))
            .mount(&mock_server)
            .await;

        let config = Config::new(Some(&mock_server.uri())).unwrap();
        let output = Output::new(true);

        let result = cmd_ls(&config, &output, None).await;
        unsafe { std::env::remove_var("SYNS_CONFIG_DIR") };

        assert!(result.is_ok());
    }

    #[tokio::test]
    #[serial]
    async fn ls_404_not_found_renders_friendly_message_with_path() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join(".syns.yaml"),
            "owner: alice\nname: my-project\n",
        )
        .unwrap();
        std::env::set_current_dir(dir.path()).unwrap();
        unsafe { std::env::set_var("SYNS_CONFIG_DIR", dir.path()) };

        let mock_server = MockServer::start().await;

        Mock::given(method("GET"))
            .and(path("/api/v1/repos/alice/my-project/tree/does/not/exist"))
            .respond_with(ResponseTemplate::new(404).set_body_json(serde_json::json!({
                "error": "not_found",
                "message": "File not found"
            })))
            .mount(&mock_server)
            .await;

        let config = Config::new(Some(&mock_server.uri())).unwrap();
        let output = Output::new(false);

        let result = cmd_ls(&config, &output, Some("does/not/exist".to_string())).await;
        unsafe { std::env::remove_var("SYNS_CONFIG_DIR") };

        let err = result.unwrap_err();
        assert_eq!(err.to_string(), "path not found: does/not/exist");
        assert_eq!(err.exit_code(), 1);
    }

    #[tokio::test]
    #[serial]
    async fn ls_404_repo_not_found_preserves_generic_display() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join(".syns.yaml"),
            "owner: alice\nname: my-project\n",
        )
        .unwrap();
        std::env::set_current_dir(dir.path()).unwrap();
        unsafe { std::env::set_var("SYNS_CONFIG_DIR", dir.path()) };

        let mock_server = MockServer::start().await;

        Mock::given(method("GET"))
            .and(path("/api/v1/repos/alice/my-project/tree/some/path"))
            .respond_with(ResponseTemplate::new(404).set_body_json(serde_json::json!({
                "error": "repo_not_found",
                "message": "Repository not found"
            })))
            .mount(&mock_server)
            .await;

        let config = Config::new(Some(&mock_server.uri())).unwrap();
        let output = Output::new(false);

        let result = cmd_ls(&config, &output, Some("some/path".to_string())).await;
        unsafe { std::env::remove_var("SYNS_CONFIG_DIR") };

        let err = result.unwrap_err();
        assert_eq!(err.to_string(), "server error (404): repo_not_found");
        assert_eq!(err.exit_code(), 1);
    }

    #[tokio::test]
    #[serial]
    async fn ls_404_not_found_in_json_mode_preserves_generic_display() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join(".syns.yaml"),
            "owner: alice\nname: my-project\n",
        )
        .unwrap();
        std::env::set_current_dir(dir.path()).unwrap();
        unsafe { std::env::set_var("SYNS_CONFIG_DIR", dir.path()) };

        let mock_server = MockServer::start().await;

        Mock::given(method("GET"))
            .and(path("/api/v1/repos/alice/my-project/tree/does/not/exist"))
            .respond_with(ResponseTemplate::new(404).set_body_json(serde_json::json!({
                "error": "not_found",
                "message": "File not found"
            })))
            .mount(&mock_server)
            .await;

        let config = Config::new(Some(&mock_server.uri())).unwrap();
        let output = Output::new(true);

        let result = cmd_ls(&config, &output, Some("does/not/exist".to_string())).await;
        unsafe { std::env::remove_var("SYNS_CONFIG_DIR") };

        let err = result.unwrap_err();
        assert_eq!(err.to_string(), "server error (404): not_found");
        assert_eq!(err.exit_code(), 1);
    }
}
