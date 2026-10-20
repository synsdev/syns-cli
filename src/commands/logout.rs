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

    // Roadmap 277: sent through the one client every request shares, held
    // to its own short bound.
    if let Some(token) = &token
        && let Ok(client) = crate::client::api_client(config.server_url())
    {
        let url = format!("{}{SIGN_OUT_PATH}", config.server_url());
        // better-auth's sign-out route declares a body schema and answers a
        // body-less POST with 415, leaving the session live (issue 317).
        let _ = client
            .post(&url)
            .bearer_auth(token)
            .timeout(Duration::from_secs(SIGN_OUT_TIMEOUT_SECS))
            .json(&json!({}))
            .send()
            .await;
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
    async fn logout_posts_sign_out_as_json_empty_object_with_bearer() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path(SIGN_OUT_PATH))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"success": true})))
            .mount(&server)
            .await;

        let dir = tempfile::tempdir().unwrap();
        unsafe { std::env::set_var("SYNS_CONFIG_DIR", dir.path()) };
        let config = Config::new(Some(&server.uri())).unwrap();
        TokenStore::new(config.credentials_path())
            .write("live-token")
            .unwrap();
        let output = Output::new(false);

        let result = cmd_logout(&config, &output).await;
        unsafe { std::env::remove_var("SYNS_CONFIG_DIR") };
        assert!(result.is_ok());
        assert!(!config.credentials_path().exists());

        let requests = server.received_requests().await.unwrap();
        assert_eq!(requests.len(), 1, "one sign-out request");
        let req = &requests[0];
        assert_eq!(
            req.headers
                .get("authorization")
                .and_then(|v| v.to_str().ok()),
            Some("Bearer live-token")
        );
        let content_type = req
            .headers
            .get("content-type")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("");
        assert!(
            content_type.starts_with("application/json"),
            "sign-out must be sent as JSON (better-auth answers 415 otherwise), got {content_type:?}"
        );
        let body: serde_json::Value = serde_json::from_slice(&req.body).expect("JSON body");
        assert_eq!(body, json!({}));
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
