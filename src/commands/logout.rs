use crate::auth::token::TokenStore;
use crate::config::Config;
use crate::errors::CliError;
use crate::output::Output;
use serde_json::json;
use std::time::Duration;

const SIGN_OUT_PATH: &str = "/api/auth/sign-out";
const SIGN_OUT_TIMEOUT_SECS: u64 = 5;

pub async fn cmd_logout(config: &Config, output: &Output) -> Result<(), CliError> {
    let store = TokenStore::new(config.credentials_path());
    let token = store.read().ok().flatten();

    if let Some(token) = &token
        && let Ok(client) = reqwest::Client::builder()
            .timeout(Duration::from_secs(SIGN_OUT_TIMEOUT_SECS))
            .build()
    {
        let url = format!("{}{SIGN_OUT_PATH}", config.server_url());
        let _ = client.post(&url).bearer_auth(token).send().await;
    }

    store.clear()?;

    if output.is_json() {
        output.json(&json!({"message": "Logged out"}));
    } else {
        output.success("Logged out");
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serial_test::serial;

    #[tokio::test]
    #[serial]
    async fn logout_clears_credentials_file() {
        let dir = tempfile::tempdir().unwrap();
        let store = TokenStore::new(dir.path().join("credentials.json"));
        store.write("existing-token").unwrap();

        unsafe { std::env::set_var("SYNS_CONFIG_DIR", dir.path()) };
        let config = Config::new(Some("http://127.0.0.1:1")).unwrap();
        let output = Output::new(false);

        let result = cmd_logout(&config, &output).await;
        unsafe { std::env::remove_var("SYNS_CONFIG_DIR") };

        assert!(result.is_ok());
        assert!(!config.credentials_path().exists());
        let read_result = TokenStore::new(config.credentials_path()).read().unwrap();
        assert_eq!(read_result, None);
    }

    #[tokio::test]
    #[serial]
    async fn logout_succeeds_when_not_logged_in() {
        let dir = tempfile::tempdir().unwrap();

        unsafe { std::env::set_var("SYNS_CONFIG_DIR", dir.path()) };
        let config = Config::new(Some("http://127.0.0.1:1")).unwrap();
        let output = Output::new(false);

        let result = cmd_logout(&config, &output).await;
        unsafe { std::env::remove_var("SYNS_CONFIG_DIR") };

        assert!(result.is_ok());
    }
}
