use syns_cli::auth::token::TokenStore;
use syns_cli::config::Config;
use syns_cli::output::Output;
use tempfile::TempDir;
use wiremock::MockServer;

pub struct TestContext {
    pub mock_server: MockServer,
    pub config: Config,
    pub output: Output,
    pub project_dir: TempDir,
    _config_dir: TempDir,
    _cache_dir: TempDir,
}

impl Drop for TestContext {
    fn drop(&mut self) {
        unsafe { std::env::remove_var("SYNS_CONFIG_DIR") };
        unsafe { std::env::remove_var("SYNS_CACHE_DIR") };
    }
}

pub async fn setup() -> TestContext {
    let mock_server = MockServer::start().await;
    let project_dir = tempfile::tempdir().unwrap();
    let config_dir = tempfile::tempdir().unwrap();
    let cache_dir = tempfile::tempdir().unwrap();

    unsafe { std::env::set_var("SYNS_CONFIG_DIR", config_dir.path()) };
    unsafe { std::env::set_var("SYNS_CACHE_DIR", cache_dir.path()) };
    let config = Config::new(Some(&mock_server.uri())).unwrap();
    let output = Output::new(false);

    TestContext {
        mock_server,
        config,
        output,
        project_dir,
        _config_dir: config_dir,
        _cache_dir: cache_dir,
    }
}

pub fn seed_credentials(ctx: &TestContext, token: &str, username: &str) {
    let store = TokenStore::new(ctx.config.credentials_path());
    store.write_with_username(token, Some(username)).unwrap();
}
