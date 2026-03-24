use std::time::{Duration, Instant};

use reqwest::Client;
use serde::{Deserialize, Serialize};

use crate::config::is_localhost_url;
use crate::errors::CliError;

const DEVICE_CODE_PATH: &str = "/api/auth/device/code";
const DEVICE_TOKEN_PATH: &str = "/api/auth/device/token";
const DEFAULT_POLL_INTERVAL_SECS: u64 = 5;
const SLOW_DOWN_INCREMENT_SECS: u64 = 5;
const MAX_EXPIRES_IN_SECS: u64 = 60 * 60;
const MAX_POLL_INTERVAL_SECS: u64 = 60;

/// Extract the origin (scheme + host + port) from a URL for comparison.
/// Returns None if the URL cannot be parsed.
fn url_origin(url: &str) -> Option<String> {
    let url = url::Url::parse(url).ok()?;
    let host = url.host_str()?;
    match url.port() {
        Some(port) => Some(format!("{}://{}:{}", url.scheme(), host, port)),
        None => Some(format!("{}://{}", url.scheme(), host)),
    }
}

/// Check whether `verification_url` is safe to open in a browser.
/// It must use https (or http://localhost) AND share the same origin as `server_url`.
pub(crate) fn is_safe_verification_url(verification_url: &str, server_url: &str) -> bool {
    use crate::config::is_safe_to_open;
    if !is_safe_to_open(verification_url) {
        return false;
    }
    let Some(ver_origin) = url_origin(verification_url) else {
        return false;
    };
    let Some(srv_origin) = url_origin(server_url) else {
        return false;
    };
    ver_origin.eq_ignore_ascii_case(&srv_origin)
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct DeviceCodeResponse {
    device_code: String,
    user_code: String,
    verification_uri: String,
    verification_uri_complete: Option<String>,
    expires_in: u64,
    interval: Option<u64>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct TokenPollRequest {
    device_code: String,
}

#[derive(Deserialize)]
struct TokenSuccessResponse {
    token: String,
}

#[derive(Deserialize)]
struct TokenErrorResponse {
    error: String,
}

/// Classify a token poll response for testability.
#[derive(Debug, PartialEq)]
pub(crate) enum PollResult {
    Success(String),
    Pending,
    SlowDown,
    Expired,
    AccessDenied,
    Error(String),
}

/// Classify a token poll response body (status 400 error field).
pub(crate) fn classify_poll_error(error: &str) -> PollResult {
    match error {
        "authorization_pending" => PollResult::Pending,
        "slow_down" => PollResult::SlowDown,
        "expired_token" => PollResult::Expired,
        "access_denied" => PollResult::AccessDenied,
        other => PollResult::Error(format!("login failed: {other}")),
    }
}

pub struct DeviceAuthFlow;

impl DeviceAuthFlow {
    pub async fn run(server_url: &str) -> Result<String, CliError> {
        // Strip trailing slash and validate URL scheme.
        let server_url = server_url.trim_end_matches('/');
        if !server_url.starts_with("https://") && !is_localhost_url(server_url) {
            return Err(CliError::Config {
                message: "server URL must use HTTPS (or http://localhost for development)"
                    .to_string(),
            });
        }

        let client = Client::builder()
            .timeout(Duration::from_secs(30))
            .build()
            .map_err(|e| CliError::Io {
                message: format!("failed to build HTTP client: {e}"),
            })?;

        // Request device code
        let url = format!("{server_url}{DEVICE_CODE_PATH}");
        let response = client
            .post(&url)
            .json(&serde_json::json!({}))
            .send()
            .await
            .map_err(|_| CliError::ServerUnreachable {
                url: server_url.to_string(),
            })?;

        if !response.status().is_success() {
            let status = response.status().as_u16();
            let error = response
                .json::<TokenErrorResponse>()
                .await
                .map(|e| e.error)
                .unwrap_or_else(|_| format!("HTTP {status}"));
            return Err(CliError::Api { status, error });
        }

        let device_code_response: DeviceCodeResponse =
            response.json().await.map_err(|_| CliError::Api {
                status: 0,
                error: "unexpected server response during login".to_string(),
            })?;

        // Display instructions to stderr
        let display_url = device_code_response
            .verification_uri_complete
            .as_deref()
            .unwrap_or(&device_code_response.verification_uri);
        eprintln!(
            "Open this URL to authenticate: {}",
            console::style(display_url).cyan()
        );
        eprintln!(
            "Enter code: {}",
            console::style(&device_code_response.user_code)
                .bold()
                .yellow()
        );

        // Open browser (best-effort), but only if the URL shares the server origin
        if is_safe_verification_url(display_url, server_url) {
            let _ = open::that(display_url);
        }

        // Set up polling
        let mut poll_interval_secs = device_code_response
            .interval
            .unwrap_or(DEFAULT_POLL_INTERVAL_SECS)
            .max(DEFAULT_POLL_INTERVAL_SECS);
        let expires_in = device_code_response.expires_in.min(MAX_EXPIRES_IN_SECS);
        let deadline = Instant::now() + Duration::from_secs(expires_in);
        eprintln!("{}", console::style("Waiting for authorization...").dim());

        // Polling loop
        loop {
            // Check deadline BEFORE sleeping
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining < Duration::from_secs(poll_interval_secs) {
                return Err(CliError::AuthRequired);
            }

            tokio::time::sleep(Duration::from_secs(poll_interval_secs)).await;

            let token_url = format!("{server_url}{DEVICE_TOKEN_PATH}");
            let response = client
                .post(&token_url)
                .json(&TokenPollRequest {
                    device_code: device_code_response.device_code.clone(),
                })
                .send()
                .await
                .map_err(|_| CliError::ServerUnreachable {
                    url: server_url.to_string(),
                })?;

            let status = response.status();

            if status.as_u16() == 200 {
                let success: TokenSuccessResponse =
                    response.json().await.map_err(|_| CliError::Api {
                        status: 0,
                        error: "unexpected server response during login".to_string(),
                    })?;
                return Ok(success.token);
            }

            if status.as_u16() == 400 {
                let error_resp: TokenErrorResponse =
                    response.json().await.map_err(|_| CliError::Api {
                        status: 400,
                        error: "unexpected server response during login".to_string(),
                    })?;
                match error_resp.error.as_str() {
                    "authorization_pending" => continue,
                    "slow_down" => {
                        poll_interval_secs = poll_interval_secs
                            .saturating_add(SLOW_DOWN_INCREMENT_SECS)
                            .min(MAX_POLL_INTERVAL_SECS);
                        continue;
                    }
                    "expired_token" => {
                        return Err(CliError::AuthRequired);
                    }
                    "access_denied" => {
                        return Err(CliError::AuthRequired);
                    }
                    other => {
                        return Err(CliError::Api {
                            status: 400,
                            error: format!("login failed: {other}"),
                        });
                    }
                }
            }

            // Any other status
            let error_msg = response
                .json::<TokenErrorResponse>()
                .await
                .map(|e| e.error)
                .unwrap_or_else(|_| format!("HTTP {}", status.as_u16()));
            return Err(CliError::Api {
                status: status.as_u16(),
                error: error_msg,
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── classify_poll_error ────────────────────────────────────────────

    #[test]
    fn poll_pending() {
        assert_eq!(
            classify_poll_error("authorization_pending"),
            PollResult::Pending
        );
    }

    #[test]
    fn poll_slow_down() {
        assert_eq!(classify_poll_error("slow_down"), PollResult::SlowDown);
    }

    #[test]
    fn poll_expired() {
        assert_eq!(classify_poll_error("expired_token"), PollResult::Expired);
    }

    #[test]
    fn poll_access_denied() {
        assert_eq!(classify_poll_error("access_denied"), PollResult::AccessDenied);
    }

    #[test]
    fn poll_unknown_error() {
        assert_eq!(
            classify_poll_error("server_error"),
            PollResult::Error("login failed: server_error".to_string())
        );
    }

    // ── is_safe_verification_url ───────────────────────────────────────

    #[test]
    fn verification_url_same_origin_allowed() {
        assert!(is_safe_verification_url(
            "https://syns.dev/auth/device?code=ABC",
            "https://syns.dev"
        ));
    }

    #[test]
    fn verification_url_different_host_rejected() {
        assert!(!is_safe_verification_url(
            "https://evil.com/phish",
            "https://syns.dev"
        ));
    }

    #[test]
    fn verification_url_different_port_rejected() {
        assert!(!is_safe_verification_url(
            "https://syns.dev:9999/auth",
            "https://syns.dev"
        ));
    }

    #[test]
    fn verification_url_http_non_localhost_rejected() {
        assert!(!is_safe_verification_url(
            "http://syns.dev/auth",
            "http://syns.dev"
        ));
    }

    #[test]
    fn verification_url_localhost_same_port() {
        assert!(is_safe_verification_url(
            "http://localhost:3000/auth/device?code=ABC",
            "http://localhost:3000"
        ));
    }

    #[test]
    fn verification_url_localhost_different_port_rejected() {
        assert!(!is_safe_verification_url(
            "http://localhost:9999/auth",
            "http://localhost:3000"
        ));
    }

    #[test]
    fn verification_url_javascript_rejected() {
        assert!(!is_safe_verification_url(
            "javascript:alert(1)",
            "https://syns.dev"
        ));
    }

    // ── HTTPS enforcement ──────────────────────────────────────────────

    #[tokio::test]
    async fn device_auth_rejects_plain_http() {
        let result = DeviceAuthFlow::run("http://example.com").await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn device_auth_rejects_ftp() {
        let result = DeviceAuthFlow::run("ftp://example.com").await;
        assert!(result.is_err());
    }

    // ── poll interval cap ──────────────────────────────────────────────

    #[test]
    fn poll_interval_caps_at_max() {
        let mut interval: u64 = 55;
        // Simulate repeated slow_down
        for _ in 0..5 {
            interval = interval
                .saturating_add(SLOW_DOWN_INCREMENT_SECS)
                .min(MAX_POLL_INTERVAL_SECS);
        }
        assert_eq!(interval, MAX_POLL_INTERVAL_SECS);
    }

    // ── url_origin helper ──────────────────────────────────────────────

    #[test]
    fn url_origin_with_port() {
        assert_eq!(
            url_origin("https://syns.dev:8443/path"),
            Some("https://syns.dev:8443".to_string())
        );
    }

    #[test]
    fn url_origin_without_port() {
        assert_eq!(
            url_origin("https://syns.dev/path"),
            Some("https://syns.dev".to_string())
        );
    }

    #[test]
    fn url_origin_invalid() {
        assert_eq!(url_origin("not-a-url"), None);
    }
}
