#![allow(dead_code)] // Used by downstream command units (U22)

use crate::errors::CliError;

const DEFAULT_POLL_INTERVAL_SECS: u64 = 5;
const SLOW_DOWN_INCREMENT_SECS: u64 = 5;
const MAX_POLL_INTERVAL_SECS: u64 = 60;
const MAX_EXPIRES_IN_SECS: u64 = 3600;
const DEVICE_CODE_PATH: &str = "/api/auth/device/code";
const DEVICE_TOKEN_PATH: &str = "/api/auth/device/token";

#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct DeviceCodeResponse {
    device_code: String,
    user_code: String,
    verification_uri: String,
    verification_uri_complete: Option<String>,
    expires_in: u64,
    interval: Option<u64>,
}

#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
struct TokenPollRequest {
    device_code: String,
}

#[derive(serde::Deserialize)]
struct TokenSuccessResponse {
    token: String,
}

#[derive(serde::Deserialize)]
struct TokenErrorResponse {
    error: String,
}

pub struct DeviceAuthFlow;

impl DeviceAuthFlow {
    pub async fn run(server_url: &str) -> Result<String, CliError> {
        // 1. HTTPS enforcement
        if !server_url.starts_with("https://") && !crate::config::is_localhost_url(server_url) {
            return Err(CliError::Config {
                message: "server URL must use HTTPS (except http://localhost for local development)"
                    .into(),
            });
        }

        // 2. Create reqwest client
        let client = reqwest::Client::new();

        // 3. Request device code
        let code_url = format!("{server_url}{DEVICE_CODE_PATH}");
        let response = client
            .post(&code_url)
            .json(&serde_json::json!({}))
            .send()
            .await
            .map_err(|_| CliError::ServerUnreachable {
                url: server_url.to_string(),
            })?;

        // 4. Handle non-200
        if !response.status().is_success() {
            let code = response.status().as_u16();
            let error = match response.json::<TokenErrorResponse>().await {
                Ok(body) => body.error,
                Err(_) => format!("HTTP {code}"),
            };
            return Err(CliError::Api {
                status: Some(code),
                error,
            });
        }

        // 5. Deserialize response
        let device_code_response: DeviceCodeResponse =
            response.json().await.map_err(|_| CliError::Io {
                message: "unexpected response from device code endpoint".into(),
            })?;

        // 6. Display to stderr
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

        // 7. Browser opening
        if crate::config::is_safe_to_open(display_url, server_url) {
            let _ = open::that(display_url);
        }

        // 8. Polling parameters
        let mut poll_interval_secs = device_code_response
            .interval
            .unwrap_or(DEFAULT_POLL_INTERVAL_SECS)
            .max(DEFAULT_POLL_INTERVAL_SECS);
        let capped_expires_in = device_code_response.expires_in.min(MAX_EXPIRES_IN_SECS);
        let deadline =
            std::time::Instant::now() + std::time::Duration::from_secs(capped_expires_in);

        // 9. Print waiting
        eprintln!("{}", console::style("Waiting for authorization...").dim());

        // 10. Polling loop
        let token_url = format!("{server_url}{DEVICE_TOKEN_PATH}");
        loop {
            let remaining = deadline.saturating_duration_since(std::time::Instant::now());
            if remaining < std::time::Duration::from_secs(poll_interval_secs) {
                return Err(CliError::Io {
                    message: "device code expired \u{2014} please run 'syns login' again".into(),
                });
            }
            tokio::time::sleep(std::time::Duration::from_secs(poll_interval_secs)).await;

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

            if status.is_success() {
                let success: TokenSuccessResponse =
                    response.json().await.map_err(|_| CliError::Io {
                        message: "unexpected response from token endpoint".into(),
                    })?;
                return Ok(success.token);
            }

            if status == reqwest::StatusCode::BAD_REQUEST {
                let error_body: TokenErrorResponse =
                    response.json().await.map_err(|_| CliError::Io {
                        message: "unexpected response from token endpoint".into(),
                    })?;
                match error_body.error.as_str() {
                    "authorization_pending" => continue,
                    "slow_down" => {
                        poll_interval_secs = (poll_interval_secs + SLOW_DOWN_INCREMENT_SECS)
                            .min(MAX_POLL_INTERVAL_SECS);
                        continue;
                    }
                    "expired_token" => {
                        return Err(CliError::Io {
                            message:
                                "device code expired \u{2014} please run 'syns login' again"
                                    .into(),
                        });
                    }
                    "access_denied" => {
                        return Err(CliError::Io {
                            message: "authorization was denied".into(),
                        });
                    }
                    other => {
                        return Err(CliError::Api {
                            status: Some(400),
                            error: format!("login failed: {other}"),
                        });
                    }
                }
            }

            let code = status.as_u16();
            let error = match response.json::<TokenErrorResponse>().await {
                Ok(body) => body.error,
                Err(_) => format!("HTTP {code}"),
            };
            return Err(CliError::Api {
                status: Some(code),
                error,
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn https_enforcement_rejects_http() {
        let result = DeviceAuthFlow::run("http://example.com").await;
        assert!(matches!(result, Err(CliError::Config { .. })));
    }

    #[tokio::test]
    async fn https_enforcement_rejects_ftp() {
        let result = DeviceAuthFlow::run("ftp://example.com").await;
        assert!(matches!(result, Err(CliError::Config { .. })));
    }

    #[tokio::test]
    async fn https_enforcement_allows_localhost() {
        let result = DeviceAuthFlow::run("http://localhost:3000").await;
        // Should NOT be a Config error — HTTPS check passed.
        // It will be ServerUnreachable since no server is running.
        assert!(!matches!(result, Err(CliError::Config { .. })));
        assert!(matches!(result, Err(CliError::ServerUnreachable { .. })));
    }

    #[test]
    fn verification_url_safety() {
        assert!(crate::config::is_safe_to_open(
            "https://syns.dev/verify?code=ABC",
            "https://syns.dev"
        ));
        assert!(!crate::config::is_safe_to_open(
            "https://evil.com/verify",
            "https://syns.dev"
        ));
        assert!(crate::config::is_safe_to_open(
            "http://localhost:3000/verify",
            "http://localhost:3000"
        ));
        assert!(!crate::config::is_safe_to_open(
            "http://localhost:3000/verify",
            "https://syns.dev"
        ));
        assert!(!crate::config::is_safe_to_open(
            "javascript:alert(1)",
            "https://syns.dev"
        ));
    }
}
