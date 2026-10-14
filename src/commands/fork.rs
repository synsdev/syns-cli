use crate::auth::token::TokenStore;
use crate::client::{ForkRequest, SynsClient};
use crate::config::Config;
use crate::errors::{ApiErrorContext, CliError};
use crate::output::Output;
use crate::repo::folder::refuse_holder_change;
use crate::repo::syns_yaml::repoint_root_identity;
use console::style;

fn validate_repo_id(repo: &str) -> Result<(), CliError> {
    let parts: Vec<&str> = repo.split('/').collect();
    if parts.len() != 2 || parts[0].is_empty() || parts[1].is_empty() {
        return Err(CliError::Config {
            message: "invalid repository: must be in owner/name format (e.g., alice/my-project)"
                .into(),
        });
    }
    Ok(())
}

fn parse_fork_identity(id: &str) -> Option<(&str, &str)> {
    let parts: Vec<&str> = id.splitn(2, '/').collect();
    if parts.len() == 2 && !parts[0].is_empty() && !parts[1].is_empty() {
        Some((parts[0], parts[1]))
    } else {
        None
    }
}

/// Issue 253 (D-142): a `409` `conflict` naming no moved head gains the
/// caller's next move — a source holding no version its caller reads, which
/// no name lifts (`CS-fork-str-nothing-readable`), told apart by its served
/// `reason` from a name already taken, which another name lifts
/// (`CS-fork-str-name-taken`). Every other refusal passes through.
fn with_conflict_move(err: CliError, source: &str, name: Option<&str>) -> CliError {
    let line = match &err {
        CliError::Api {
            status: Some(409),
            error,
            context,
        } if error == "conflict" => match context {
            Some(ApiErrorContext::HeadMoved { .. }) => None,
            Some(ApiErrorContext::ConflictReason { reason }) if reason == "empty_source" => {
                Some(format!(
                    "{source} holds no version you can read, and no other name lifts that; fork it once one lands there"
                ))
            }
            _ => {
                let taken =
                    name.unwrap_or_else(|| source.split_once('/').map_or(source, |(_, n)| n));
                Some(format!(
                    "the name {taken} is taken in your account; fork under another with: syns fork {source} --name NEW_NAME"
                ))
            }
        },
        _ => None,
    };
    match line {
        Some(line) => err.with_next_move(line),
        None => err,
    }
}

pub async fn cmd_fork(
    config: &Config,
    output: &Output,
    repo: String,
    name: Option<String>,
) -> Result<(), CliError> {
    validate_repo_id(&repo)?;

    // Inside a scoped folder the fork would replace the folder's identity
    // file, a file its holder tracks, so it is refused as acting on the
    // holder before the credential, the request and the identity write;
    // a misplaced folder, and a nearest file parsing as neither form, end
    // the run the same way (SPEC u290 Behaviour, `cmd_fork` 1, `D-108`).
    if let Ok(cwd) = std::env::current_dir() {
        refuse_holder_change(&cwd, "syns fork")?;
    }

    let token = TokenStore::new(config.credentials_path())
        .read()?
        .ok_or(CliError::AuthRequired)?;

    let client = SynsClient::new(config.server_url())?;

    let request = ForkRequest { name };

    let (response, raw) = client
        .fork(&repo, &token, &request)
        .await
        .map_err(|e| with_conflict_move(e, &repo, request.name.as_deref()))?;

    let fork_id = format!("{}/{}", response.owner, response.name);
    let fork_identity = parse_fork_identity(&fork_id);

    if output.is_json() {
        output.json(&raw);
    } else {
        output.success(&format!("Forked {} → {}", repo, fork_id));
        eprintln!("{}", style(format!("{} files", response.file_count)).dim());
    }

    // The fork places no content, so it creates no identity: only an
    // identity file standing in the working directory and naming the
    // source is repointed to the copy (issue 162, `D-025`).
    let (Some((source_owner, source_name)), Some(copy)) = (repo.split_once('/'), fork_identity)
    else {
        return Ok(());
    };
    let Ok(cwd) = std::env::current_dir() else {
        return Ok(());
    };
    match repoint_root_identity(&cwd, (source_owner, source_name), copy) {
        Ok(true) if !output.is_json() => eprintln!(
            "{}",
            style(format!("Updated .syns.yaml → {}/{}", copy.0, copy.1)).dim()
        ),
        Ok(_) => {}
        Err(err) => eprintln!("warning: could not update .syns.yaml: {err}"),
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
    async fn fork_json_output() {
        let dir = tempfile::tempdir().unwrap();
        TokenStore::new(dir.path().join("credentials.json"))
            .write("test-token")
            .unwrap();
        std::env::set_current_dir(dir.path()).unwrap();
        unsafe { std::env::set_var("SYNS_CONFIG_DIR", dir.path()) };

        let mock_server = MockServer::start().await;

        Mock::given(method("POST"))
            .and(path("/api/v1/repos/alice/my-project/fork"))
            .respond_with(ResponseTemplate::new(201).set_body_json(serde_json::json!({
                "owner": "bob",
                "name": "my-project",
                "description": null,
                "commitSha": "abc123",
                "status": "active",
                "visibility": "public",
                "tags": [],
                "fileCount": 5,
                "forkCount": 0,
                "forkedFrom": { "owner": "alice", "name": "my-project" },
                "createdAt": "2025-01-01T00:00:00Z",
                "updatedAt": "2025-01-01T00:00:00Z"
            })))
            .mount(&mock_server)
            .await;

        let config = Config::new(Some(&mock_server.uri())).unwrap();
        let output = Output::new(true);

        let result = cmd_fork(&config, &output, "alice/my-project".into(), None).await;
        assert!(result.is_ok());

        // Issue 162: a directory bound to no repository stays unbound.
        assert!(!dir.path().join(".syns.yaml").exists());

        unsafe { std::env::remove_var("SYNS_CONFIG_DIR") };
    }

    /// Issue 162: run a successful `alice/my-project` → `bob/my-copy`
    /// fork standing in `cwd`, `config_dir` holding the credential.
    async fn fork_from(config_dir: &std::path::Path, cwd: &std::path::Path) {
        TokenStore::new(config_dir.join("credentials.json"))
            .write("test-token")
            .unwrap();
        std::env::set_current_dir(cwd).unwrap();
        unsafe { std::env::set_var("SYNS_CONFIG_DIR", config_dir) };
        let mock_server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/api/v1/repos/alice/my-project/fork"))
            .respond_with(ResponseTemplate::new(201).set_body_json(serde_json::json!({
                "owner": "bob", "name": "my-copy", "description": null,
                "commitSha": "abc123", "status": "active", "visibility": "public",
                "tags": [], "fileCount": 5, "forkCount": 0,
                "forkedFrom": { "owner": "alice", "name": "my-project" },
                "createdAt": "2025-01-01T00:00:00Z", "updatedAt": "2025-01-01T00:00:00Z"
            })))
            .mount(&mock_server)
            .await;
        let config = Config::new(Some(&mock_server.uri())).unwrap();
        cmd_fork(
            &config,
            &Output::new(false),
            "alice/my-project".into(),
            None,
        )
        .await
        .unwrap();
        unsafe { std::env::remove_var("SYNS_CONFIG_DIR") };
    }

    #[tokio::test]
    #[serial]
    async fn fork_from_an_unbound_directory_writes_no_identity() {
        let config_dir = tempfile::tempdir().unwrap();
        let cwd = tempfile::tempdir().unwrap();
        std::fs::create_dir(cwd.path().join("sibling")).unwrap();
        std::fs::write(cwd.path().join("sibling/note.md"), "note\n").unwrap();
        fork_from(config_dir.path(), cwd.path()).await;
        assert!(!cwd.path().join(".syns.yaml").exists());
        assert!(!cwd.path().join("sibling/.syns.yaml").exists());
    }

    #[tokio::test]
    #[serial]
    async fn fork_from_a_checkout_of_the_source_repoints_it_keeping_its_checks() {
        let config_dir = tempfile::tempdir().unwrap();
        let cwd = tempfile::tempdir().unwrap();
        let file = cwd.path().join(".syns.yaml");
        std::fs::write(
            &file,
            "owner: alice\nname: my-project\nchecks:\n  - exit 0\n",
        )
        .unwrap();
        fork_from(config_dir.path(), cwd.path()).await;
        let text = std::fs::read_to_string(&file).unwrap();
        assert!(
            crate::repo::syns_yaml::text_names_root_identity(text.as_bytes(), "bob", "my-copy"),
            "{text}"
        );
        assert_eq!(
            crate::repo::syns_yaml::declared_checks(&text).unwrap(),
            vec!["exit 0"]
        );
    }

    #[tokio::test]
    #[serial]
    async fn fork_beside_or_below_another_identity_leaves_every_file_standing() {
        let config_dir = tempfile::tempdir().unwrap();
        let other = tempfile::tempdir().unwrap();
        let other_file = other.path().join(".syns.yaml");
        std::fs::write(&other_file, "owner: carol\nname: other\n").unwrap();
        fork_from(config_dir.path(), other.path()).await;
        assert_eq!(
            std::fs::read_to_string(&other_file).unwrap(),
            "owner: carol\nname: other\n"
        );

        let source = tempfile::tempdir().unwrap();
        std::fs::write(
            source.path().join(".syns.yaml"),
            "owner: alice\nname: my-project\n",
        )
        .unwrap();
        let sub = source.path().join("sub");
        std::fs::create_dir(&sub).unwrap();
        fork_from(config_dir.path(), &sub).await;
        assert!(!sub.join(".syns.yaml").exists());
        assert_eq!(
            std::fs::read_to_string(source.path().join(".syns.yaml")).unwrap(),
            "owner: alice\nname: my-project\n"
        );
    }

    #[tokio::test]
    #[serial]
    async fn fork_invalid_repo_format() {
        let dir = tempfile::tempdir().unwrap();
        unsafe { std::env::set_var("SYNS_CONFIG_DIR", dir.path()) };

        let config = Config::new(Some("http://localhost:1")).unwrap();
        let output = Output::new(false);

        let result = cmd_fork(&config, &output, "no-slash".into(), None).await;
        assert!(result.is_err());
        assert!(format!("{:?}", result.unwrap_err()).contains("invalid repository"));

        unsafe { std::env::remove_var("SYNS_CONFIG_DIR") };
    }

    #[tokio::test]
    #[serial]
    async fn fork_no_auth() {
        let dir = tempfile::tempdir().unwrap();
        unsafe { std::env::set_var("SYNS_CONFIG_DIR", dir.path()) };

        let config = Config::new(Some("http://localhost:1")).unwrap();
        let output = Output::new(false);

        let result = cmd_fork(&config, &output, "alice/my-project".into(), None).await;
        assert!(result.is_err());
        assert!(format!("{:?}", result.unwrap_err()).contains("AuthRequired"));

        unsafe { std::env::remove_var("SYNS_CONFIG_DIR") };
    }

    #[tokio::test]
    async fn fork_raw_emits_full_repository_object() {
        let mock_server = MockServer::start().await;
        let body = r#"{"owner":"bob","name":"my-project","description":"forked","commitSha":"abc123","status":"active","author":"bob","tags":["api"],"visibility":"public","forkedFrom":{"owner":"alice","name":"my-project"},"forkCount":0,"fileCount":5,"role":"owner","createdAt":"2026-04-18T00:00:00Z","updatedAt":"2026-04-18T00:00:00Z"}"#;
        Mock::given(method("POST"))
            .and(path("/api/v1/repos/alice/my-project/fork"))
            .respond_with(ResponseTemplate::new(201).set_body_string(body))
            .mount(&mock_server)
            .await;

        let client = SynsClient::new(&mock_server.uri()).unwrap();
        let request = ForkRequest { name: None };
        let (_typed, raw) = client
            .fork("alice/my-project", "test-token", &request)
            .await
            .unwrap();

        assert_eq!(raw.as_object().unwrap().keys().count(), 14);
        assert_eq!(raw["forkedFrom"]["owner"], serde_json::json!("alice"));
        assert_eq!(raw["role"], serde_json::json!("owner"));
        assert_eq!(raw["visibility"], serde_json::json!("public"));
        let expected: serde_json::Value = serde_json::from_str(body).unwrap();
        assert_eq!(raw, expected);
    }

    /// Issue 253 (D-142): run `cmd_fork` against a `409` body, returning
    /// the refusal's one line.
    async fn fork_refused_with(body: serde_json::Value, name: Option<String>) -> String {
        let dir = tempfile::tempdir().unwrap();
        TokenStore::new(dir.path().join("credentials.json"))
            .write("test-token")
            .unwrap();
        std::env::set_current_dir(dir.path()).unwrap();
        unsafe { std::env::set_var("SYNS_CONFIG_DIR", dir.path()) };
        let mock_server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/api/v1/repos/bob/empty/fork"))
            .respond_with(ResponseTemplate::new(409).set_body_json(body))
            .mount(&mock_server)
            .await;
        let config = Config::new(Some(&mock_server.uri())).unwrap();
        let output = Output::new(false);
        let err = cmd_fork(&config, &output, "bob/empty".into(), name)
            .await
            .unwrap_err();
        unsafe { std::env::remove_var("SYNS_CONFIG_DIR") };
        assert!(!dir.path().join(".syns.yaml").exists());
        err.to_string()
    }

    #[tokio::test]
    #[serial]
    async fn fork_of_a_source_holding_nothing_readable_names_no_other_name() {
        let line = fork_refused_with(
            serde_json::json!({
                "error": "conflict",
                "message": "Cannot fork a repository that has no content",
                "reason": "empty_source",
            }),
            Some("notes".into()),
        )
        .await;
        assert_eq!(
            line,
            "server error (409): conflict \u{2014} bob/empty holds no version you can read, and no other name lifts that; fork it once one lands there"
        );
    }

    #[tokio::test]
    #[serial]
    async fn fork_onto_a_taken_name_names_the_name_and_the_flag() {
        let typed = fork_refused_with(
            serde_json::json!({"error": "conflict", "message": "Repository already exists"}),
            Some("notes".into()),
        )
        .await;
        assert_eq!(
            typed,
            "server error (409): conflict \u{2014} the name notes is taken in your account; fork under another with: syns fork bob/empty --name NEW_NAME"
        );
        let defaulted = fork_refused_with(
            serde_json::json!({"error": "conflict", "message": "Repository already exists"}),
            None,
        )
        .await;
        assert_eq!(
            defaulted,
            "server error (409): conflict \u{2014} the name empty is taken in your account; fork under another with: syns fork bob/empty --name NEW_NAME"
        );
    }

    #[tokio::test]
    #[serial]
    async fn fork_refusal_other_than_conflict_keeps_its_line() {
        let dir = tempfile::tempdir().unwrap();
        TokenStore::new(dir.path().join("credentials.json"))
            .write("test-token")
            .unwrap();
        std::env::set_current_dir(dir.path()).unwrap();
        unsafe { std::env::set_var("SYNS_CONFIG_DIR", dir.path()) };
        let mock_server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/api/v1/repos/bob/empty/fork"))
            .respond_with(
                ResponseTemplate::new(404).set_body_json(serde_json::json!({"error": "not_found"})),
            )
            .mount(&mock_server)
            .await;
        let config = Config::new(Some(&mock_server.uri())).unwrap();
        let output = Output::new(false);
        let err = cmd_fork(&config, &output, "bob/empty".into(), None)
            .await
            .unwrap_err();
        unsafe { std::env::remove_var("SYNS_CONFIG_DIR") };
        assert_eq!(err.to_string(), "server error (404): not_found");
    }
}
