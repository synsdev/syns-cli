use crate::auth::device::DeviceAuthFlow;
use crate::auth::token::TokenStore;
use crate::client::SynsClient;
use crate::config::Config;
use crate::errors::CliError;
use crate::output::Output;
use serde_json::json;

pub async fn cmd_login(config: &Config, output: &Output) -> Result<(), CliError> {
    let token = DeviceAuthFlow::run(config.server_url()).await?;

    let store = TokenStore::new(config.credentials_path());
    store.write(&token)?;

    let client = SynsClient::new(config.server_url())?;
    match client.get_session(&token).await {
        Ok(session) => {
            let username = &session.user.username;
            // Re-write credentials with username for downstream identity resolution (U11)
            let _ = store.write_with_username(&token, Some(username));
            if output.is_json() {
                output.json(&json!({"username": username}));
            } else {
                output.success(&format!("Logged in as {username}"));
            }
        }
        Err(_) => {
            if output.is_json() {
                output.json(&json!({"message": "Login successful"}));
            } else {
                output.success("Login successful");
            }
        }
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
    async fn login_stores_token_and_reports_username() {
        let mock_server = MockServer::start().await;

        Mock::given(method("POST"))
            .and(path("/api/auth/device/code"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "deviceCode": "test-device-code",
                "userCode": "TEST-1234",
                "verificationUri": format!("{}/verify", mock_server.uri()),
                "expiresIn": 30,
                "interval": 5
            })))
            .mount(&mock_server)
            .await;

        Mock::given(method("POST"))
            .and(path("/api/auth/device/token"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "token": "test-bearer-token"
            })))
            .mount(&mock_server)
            .await;

        Mock::given(method("GET"))
            .and(path("/api/auth/get-session"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "user": {
                    "id": "u1",
                    "username": "testuser",
                    "name": "Test User",
                    "email": "test@example.com",
                    "image": null
                }
            })))
            .mount(&mock_server)
            .await;

        let dir = tempfile::tempdir().unwrap();
        unsafe { std::env::set_var("SYNS_CONFIG_DIR", dir.path()) };
        let config = Config::new(Some(&mock_server.uri())).unwrap();
        let output = Output::new(true);

        let result = cmd_login(&config, &output).await;
        unsafe { std::env::remove_var("SYNS_CONFIG_DIR") };

        assert!(result.is_ok());
        let store = TokenStore::new(config.credentials_path());
        assert_eq!(store.read().unwrap(), Some("test-bearer-token".to_string()));
        assert_eq!(
            store.read_username().unwrap(),
            Some("testuser".to_string())
        );
    }
}
