use crate::auth::token::TokenStore;
use crate::client::SynsClient;
use crate::config::Config;
use crate::errors::CliError;
use crate::output::Output;

pub async fn cmd_whoami(config: &Config, output: &Output) -> Result<(), CliError> {
    let store = TokenStore::new(config.credentials_path());
    let token = store.read()?;

    let token = match token {
        Some(t) => t,
        None => return Err(CliError::AuthRequired),
    };

    let client = SynsClient::new(config.server_url())?;
    let (session, raw) = client.get_session(&token).await?;
    let user = session.user;

    if output.is_json() {
        // Safe to index `raw["user"]` here because `SessionResponse.user` is non-optional
        // (see `src/client.rs::SessionResponse`); if the wire body lacked `"user"` or it
        // was null, the typed-side `serde_json::from_value::<SessionResponse>` inside
        // `process_response_raw` would have already failed with CliError::Api before this
        // branch can run. Revisit this projection if `user` ever becomes Option<…>.
        output.json(&raw["user"]);
    } else {
        output.table(
            &["Field", "Value"],
            vec![
                vec!["Username".into(), user.username],
                vec!["Name".into(), user.name],
                vec!["Email".into(), user.email],
            ],
        );
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
    async fn whoami_returns_auth_required_when_not_logged_in() {
        let dir = tempfile::tempdir().unwrap();

        unsafe { std::env::set_var("SYNS_CONFIG_DIR", dir.path()) };
        let config = Config::new(Some("http://127.0.0.1:1")).unwrap();
        let output = Output::new(false);

        let result = cmd_whoami(&config, &output).await;
        unsafe { std::env::remove_var("SYNS_CONFIG_DIR") };

        assert!(matches!(result, Err(CliError::AuthRequired)));
    }

    #[tokio::test]
    async fn whoami_get_session_raw_preserves_full_user_with_8_fields() {
        use crate::client::SynsClient;

        let mock_server = MockServer::start().await;
        let body = r#"{"session":{"id":"sess-1","userId":"u-1","expiresAt":"2026-05-01T00:00:00.000Z"},"user":{"id":"u-1","username":"alice","name":"Alice","email":"alice@test.com","emailVerified":true,"image":null,"createdAt":"2026-04-17T07:22:30.617Z","updatedAt":"2026-04-17T07:22:30.617Z"}}"#;
        Mock::given(method("GET"))
            .and(path("/api/auth/get-session"))
            .respond_with(ResponseTemplate::new(200).set_body_string(body))
            .mount(&mock_server)
            .await;

        let client = SynsClient::new(&mock_server.uri()).unwrap();
        let (typed, raw) = client.get_session("test-token").await.unwrap();

        assert_eq!(raw["user"].as_object().unwrap().keys().count(), 8);
        assert_eq!(raw["user"]["emailVerified"], serde_json::json!(true));
        assert_eq!(
            raw["user"]["createdAt"],
            serde_json::json!("2026-04-17T07:22:30.617Z")
        );
        assert_eq!(typed.user.username, "alice");
        let expected: serde_json::Value = serde_json::from_str(body).unwrap();
        assert_eq!(raw, expected);
    }

    #[tokio::test]
    #[serial]
    async fn whoami_displays_user_info() {
        let dir = tempfile::tempdir().unwrap();
        let store = TokenStore::new(dir.path().join("credentials.json"));
        store.write("test-token").unwrap();

        let mock_server = MockServer::start().await;

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

        unsafe { std::env::set_var("SYNS_CONFIG_DIR", dir.path()) };
        let config = Config::new(Some(&mock_server.uri())).unwrap();
        let output = Output::new(true);

        let result = cmd_whoami(&config, &output).await;
        unsafe { std::env::remove_var("SYNS_CONFIG_DIR") };

        assert!(result.is_ok());
    }
}
