use crate::auth::token::TokenStore;
use crate::client::SynsClient;
use crate::config::Config;
use crate::errors::CliError;
use crate::output::Output;
use serde_json::json;

pub async fn cmd_whoami(config: &Config, output: &Output) -> Result<(), CliError> {
    let store = TokenStore::new(config.credentials_path());
    let token = store.read()?;

    let token = match token {
        Some(t) => t,
        None => return Err(CliError::AuthRequired),
    };

    let client = SynsClient::new(config.server_url())?;
    let session = client.get_session(&token).await?;
    let user = session.user;

    if output.is_json() {
        let mut value = json!({
            "id": user.id,
            "username": user.username,
            "name": user.name,
            "email": user.email,
        });
        if let Some(image) = &user.image {
            value["image"] = json!(image);
        }
        output.json(&value);
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
