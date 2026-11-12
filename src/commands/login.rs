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
        Ok((session, _raw)) => {
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
    use wiremock::matchers::{body_json, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    #[tokio::test]
    #[serial]
    async fn login_stores_token_and_reports_username() {
        let mock_server = MockServer::start().await;

        Mock::given(method("POST"))
            .and(path("/api/auth/device/code"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "device_code": "test-device-code",
                "user_code": "TEST-1234",
                "verification_uri": format!("{}/verify", mock_server.uri()),
                "expires_in": 30,
                "interval": 5
            })))
            .mount(&mock_server)
            .await;

        Mock::given(method("POST"))
            .and(path("/api/auth/device/token"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "access_token": "test-bearer-token"
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
        assert_eq!(store.read_username().unwrap(), Some("testuser".to_string()));
    }

    /// FEATURE_SPEC C2: the code is asked as `syns-machine` with the computer's own name, and every poll is
    /// made under `syns-machine`.
    #[tokio::test]
    #[serial]
    async fn login_asks_as_syns_machine_naming_the_computer() {
        let mock_server = MockServer::start().await;
        let machine = crate::auth::device::system_machine_name();

        Mock::given(method("POST"))
            .and(path("/api/auth/device/code"))
            .and(body_json(
                serde_json::json!({"client_id": "syns-machine", "machine": machine}),
            ))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "device_code": "test-device-code",
                "user_code": "TEST-1234",
                // Another origin than the server's, so the flow prints it and opens no browser.
                "verification_uri": "https://example.invalid/verify",
                "expires_in": 30,
                "interval": 5
            })))
            .expect(1)
            .mount(&mock_server)
            .await;

        Mock::given(method("POST"))
            .and(path("/api/auth/device/token"))
            .and(body_json(serde_json::json!({
                "device_code": "test-device-code",
                "client_id": "syns-machine",
                "grant_type": "urn:ietf:params:oauth:grant-type:device_code"
            })))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "access_token": "test-bearer-token"
            })))
            .expect(1)
            .mount(&mock_server)
            .await;

        Mock::given(method("GET"))
            .and(path("/api/auth/get-session"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "session": { "id": "s1", "machine": "laptop" },
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

        assert!(result.is_ok(), "{result:?}");
        for request in mock_server.received_requests().await.unwrap() {
            if request.url.path() == "/api/auth/device/token" {
                let body: serde_json::Value = serde_json::from_slice(&request.body).unwrap();
                assert_eq!(body["client_id"], "syns-machine");
            }
        }
        let store = TokenStore::new(config.credentials_path());
        assert_eq!(store.read().unwrap(), Some("test-bearer-token".to_string()));
        assert_eq!(store.read_username().unwrap(), Some("testuser".to_string()));
        // credentials.json holds the token and the username, as before.
        let stored: serde_json::Value =
            serde_json::from_slice(&std::fs::read(config.credentials_path()).unwrap()).unwrap();
        let mut keys: Vec<_> = stored.as_object().unwrap().keys().cloned().collect();
        keys.sort();
        assert_eq!(keys, ["token", "username"]);
    }

    /// FEATURE_SPEC C3: against a server from before this item, which admits no `syns-machine`, the login ends
    /// naming `invalid_client`, asks no other way and stores nothing.
    #[tokio::test]
    #[serial]
    async fn login_against_a_server_without_syns_machine_names_invalid_client() {
        let mock_server = MockServer::start().await;

        Mock::given(method("POST"))
            .and(path("/api/auth/device/code"))
            .respond_with(ResponseTemplate::new(400).set_body_json(serde_json::json!({
                "error": "invalid_client",
                "error_description": "Invalid client ID"
            })))
            .expect(1)
            .mount(&mock_server)
            .await;

        let dir = tempfile::tempdir().unwrap();
        unsafe { std::env::set_var("SYNS_CONFIG_DIR", dir.path()) };
        let config = Config::new(Some(&mock_server.uri())).unwrap();
        let output = Output::new(true);

        let result = cmd_login(&config, &output).await;
        unsafe { std::env::remove_var("SYNS_CONFIG_DIR") };

        match result {
            Err(CliError::Api { status, error, .. }) => {
                assert_eq!(status, Some(400));
                assert_eq!(error, "invalid_client");
            }
            other => panic!("expected invalid_client, got {other:?}"),
        }
        let requests = mock_server.received_requests().await.unwrap();
        assert_eq!(requests.len(), 1, "one code request and nothing after it");
        let body: serde_json::Value = serde_json::from_slice(&requests[0].body).unwrap();
        assert_eq!(body["client_id"], "syns-machine");
        assert!(!config.credentials_path().exists());
    }
}
