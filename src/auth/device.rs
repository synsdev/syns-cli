use std::time::{Duration, Instant};

use reqwest::Client;
use serde::{Deserialize, Serialize};

use crate::errors::CliError;

const DEVICE_CODE_PATH: &str = "/api/auth/device/code";
const DEVICE_TOKEN_PATH: &str = "/api/auth/device/token";
const DEFAULT_POLL_INTERVAL_SECS: u64 = 5;
const SLOW_DOWN_INCREMENT_SECS: u64 = 5;

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

pub struct DeviceAuthFlow;

impl DeviceAuthFlow {
    pub async fn run(server_url: &str) -> Result<String, CliError> {
        // Strip trailing slash and validate URL scheme.
        let server_url = server_url.trim_end_matches('/');
        if !server_url.starts_with("https://") && !server_url.starts_with("http://localhost") {
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

        // Open browser (best-effort)
        let _ = open::that(display_url);

        // Set up polling
        let mut poll_interval_secs = device_code_response
            .interval
            .unwrap_or(DEFAULT_POLL_INTERVAL_SECS)
            .max(DEFAULT_POLL_INTERVAL_SECS);
        let deadline = Instant::now() + Duration::from_secs(device_code_response.expires_in);
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
                        poll_interval_secs += SLOW_DOWN_INCREMENT_SECS;
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
