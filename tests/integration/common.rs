//! Shared fixtures for the subprocess-style integration tests
//! (`assert_cmd::Command::cargo_bin("syns")`). Centralises the
//! tokio-runtime + `MockServer` + credential-seeding boilerplate
//! that was previously duplicated across five `push_*_test.rs`
//! files (CODE_REVIEW L3).
//!
//! Distinct from the in-process `tests/common/mod.rs` (which exposes
//! a `setup() -> TestContext` for tests that drive `cmd_push` /
//! `cmd_pull` directly via the library API). The two helpers serve
//! complementary call sites and should not be merged.

#![allow(dead_code)] // Not every module uses every helper.

use serde_json::{Value, json};
use syns_cli::auth::token::TokenStore;
use tempfile::TempDir;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

/// Convenience knobs for `spawn_mock_env`. Each field is independent;
/// the default value is the most-permissive choice for the field's
/// purpose. Use a fluent literal:
///
/// ```ignore
/// SpawnOpts { put_response: Some(default_push_response()), ..Default::default() }
/// ```
#[derive(Default)]
pub struct SpawnOpts {
    /// Optional mock body for `PUT /api/v1/repos/{owner}/{repo}/push`.
    /// `None` mounts no PUT mock (useful when the guard under test
    /// fires before the wire call).
    pub put_response: Option<Value>,
    /// Optional mock body for `GET /api/v1/repos/{owner}/{repo}/tree`.
    /// Default `None` mounts a 404 (`not_found`) which is the common
    /// "first-push" wire shape; set to `Some(...)` to override.
    pub tree_response: Option<Value>,
    /// Repo owner (default `"alice"`).
    pub owner: Option<&'static str>,
    /// Repo name (default `"repo"`).
    pub repo: Option<&'static str>,
}

/// All the handles a subprocess test needs to invoke `syns push`
/// (or any other identity-resolving command). The `TempDir` fields
/// are kept on the struct so their `Drop` is deferred to the end of
/// the test — dropping them earlier would unlink the directories
/// before the subprocess can read them.
pub struct SpawnEnv {
    pub server: MockServer,
    pub project_dir: TempDir,
    pub config_dir: TempDir,
    pub cache_dir: TempDir,
    pub mock_uri: String,
}

/// Spin up a wiremock `MockServer`, seed credentials into a temp
/// config dir, and return the handles the subprocess needs.
///
/// Credential seeding uses the library's `TokenStore` (not the CLI's
/// `syns login` subcommand) so this helper has no transitive
/// dependency on the rest of the CLI's wire layer.
pub fn spawn_mock_env(opts: SpawnOpts) -> SpawnEnv {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    rt.block_on(async {
        let server = MockServer::start().await;
        let project_dir = tempfile::tempdir().unwrap();
        let config_dir = tempfile::tempdir().unwrap();
        let cache_dir = tempfile::tempdir().unwrap();
        let owner = opts.owner.unwrap_or("alice");
        let repo = opts.repo.unwrap_or("repo");

        // Default tree mock: 404 not_found (first-push wire shape).
        let tree_body = opts
            .tree_response
            .unwrap_or_else(|| json!({"error": "not_found"}));
        let tree_status = if tree_body.as_object().and_then(|o| o.get("error")).is_some() {
            404
        } else {
            200
        };
        Mock::given(method("GET"))
            .and(path(format!("/api/v1/repos/{owner}/{repo}/tree")))
            .respond_with(ResponseTemplate::new(tree_status).set_body_json(tree_body))
            .mount(&server)
            .await;

        if let Some(body) = opts.put_response {
            Mock::given(method("PUT"))
                .and(path(format!("/api/v1/repos/{owner}/{repo}/push")))
                .respond_with(ResponseTemplate::new(200).set_body_json(body))
                .mount(&server)
                .await;
        }

        let uri = server.uri();

        // Seed credentials. Env-vars are set transiently so the
        // library's `Config::new` resolves the temp config_dir;
        // they are removed before returning so the subprocess
        // invocation (which passes `--env SYNS_CONFIG_DIR=...`
        // explicitly) is the only thing setting them.
        unsafe { std::env::set_var("SYNS_CONFIG_DIR", config_dir.path()) };
        let config = syns_cli::config::Config::new(Some(&uri)).unwrap();
        let store = TokenStore::new(config.credentials_path());
        store
            .write_with_username("test-token", Some(owner))
            .unwrap();
        unsafe { std::env::remove_var("SYNS_CONFIG_DIR") };

        SpawnEnv {
            server,
            project_dir,
            config_dir,
            cache_dir,
            mock_uri: uri,
        }
    })
}

/// Standard "successful push" PUT response body.
pub fn default_push_response() -> Value {
    json!({
        "commitSha": "deadbeef00000000000000000000000000000000",
        "version": 1,
        "filesChanged": 1,
        "created": true,
    })
}
