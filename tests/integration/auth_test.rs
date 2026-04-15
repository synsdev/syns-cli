use crate::common::{setup, seed_credentials};
use syns_cli::auth::token::TokenStore;
use syns_cli::commands::logout::cmd_logout;
use syns_cli::commands::whoami::cmd_whoami;
use syns_cli::errors::CliError;
use serial_test::serial;
use serde_json::json;
use wiremock::matchers::{header, method, path};
use wiremock::{Mock, ResponseTemplate};

#[tokio::test(flavor = "current_thread")]
#[serial]
async fn logout_clears_credentials() {
    let ctx = setup().await;
    seed_credentials(&ctx, "test-token-xyz", "dave");

    let store = TokenStore::new(ctx.config.credentials_path());
    assert_eq!(store.read().unwrap(), Some("test-token-xyz".to_string()));

    Mock::given(method("POST"))
        .and(path("/api/auth/sign-out"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({})))
        .expect(1)
        .mount(&ctx.mock_server)
        .await;

    let result = cmd_logout(&ctx.config, &ctx.output).await;
    assert!(result.is_ok());
    assert_eq!(store.read().unwrap(), None);
}

#[tokio::test(flavor = "current_thread")]
#[serial]
async fn whoami_shows_current_user() {
    let ctx = setup().await;
    seed_credentials(&ctx, "test-token-xyz", "eve");

    Mock::given(method("GET"))
        .and(path("/api/auth/get-session"))
        .and(header("Authorization", "Bearer test-token-xyz"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "user": {
                "id": "user-id-123",
                "name": "Eve Smith",
                "username": "eve",
                "email": "eve@example.com",
                "emailVerified": true,
                "image": null,
                "createdAt": "2026-01-01T00:00:00.000Z",
                "updatedAt": "2026-01-01T00:00:00.000Z"
            },
            "session": {
                "id": "session-id-456",
                "userId": "user-id-123",
                "expiresAt": "2026-12-31T23:59:59.000Z"
            }
        })))
        .expect(1)
        .mount(&ctx.mock_server)
        .await;

    let result = cmd_whoami(&ctx.config, &ctx.output).await;
    assert!(result.is_ok());
}

#[tokio::test(flavor = "current_thread")]
#[serial]
async fn whoami_returns_auth_error_when_not_logged_in() {
    let ctx = setup().await;
    let result = cmd_whoami(&ctx.config, &ctx.output).await;
    assert!(matches!(result, Err(CliError::AuthRequired)));
}
