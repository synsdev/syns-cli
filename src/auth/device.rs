use crate::client::{ANSWER_STALL, api_client, read_body, send_bounded};
use crate::errors::CliError;

const DEFAULT_POLL_INTERVAL_SECS: u64 = 5;
const SLOW_DOWN_INCREMENT_SECS: u64 = 5;
const MAX_POLL_INTERVAL_SECS: u64 = 60;
const MAX_EXPIRES_IN_SECS: u64 = 3600;
/// The client `syns login` asks as: the Syns CLI on a computer it names (`EP-device-code`), so the login the
/// approval makes may join that account's door as a machine of that name.
const CLIENT_ID: &str = "syns-machine";
const DEVICE_CODE_PATH: &str = "/api/auth/device/code";
const DEVICE_TOKEN_PATH: &str = "/api/auth/device/token";

/// At most this many characters of the system's name are sent; the server takes 1 to 255.
const MACHINE_MAX_CHARS: usize = 255;
/// What is sent as the computer's name when the system gives none.
const MACHINE_FALLBACK: &str = "machine";

/// The name `syns login` sends as `machine`: the system's own, as it gives it, unshaped (the server makes the
/// kept name from it). Cut to 255 characters; `machine` when there is none or it is empty.
pub(crate) fn machine_name_of(raw: Option<&str>) -> String {
    match raw {
        Some(name) if !name.is_empty() => name.chars().take(MACHINE_MAX_CHARS).collect(),
        _ => MACHINE_FALLBACK.to_string(),
    }
}

/// `machine_name_of` for a name the system gives as bytes: what is not UTF-8 becomes U+FFFD.
pub(crate) fn machine_name_of_bytes(raw: Option<&[u8]>) -> String {
    machine_name_of(raw.map(String::from_utf8_lossy).as_deref())
}

/// The computer's own name, as `hostname` prints it, read from the system.
pub(crate) fn system_machine_name() -> String {
    #[cfg(unix)]
    {
        let mut buf = [0u8; 1024];
        // SAFETY: the buffer is valid for `buf.len()` bytes; gethostname writes at most that many.
        let rc = unsafe { libc::gethostname(buf.as_mut_ptr().cast(), buf.len()) };
        if rc != 0 {
            return machine_name_of(None);
        }
        let end = buf.iter().position(|&b| b == 0).unwrap_or(buf.len());
        machine_name_of_bytes(Some(&buf[..end]))
    }
    #[cfg(windows)]
    {
        use windows_sys::Win32::System::SystemInformation::{
            ComputerNameDnsHostname, GetComputerNameExW,
        };
        let mut buf = [0u16; 1024];
        let mut len = buf.len() as u32;
        // SAFETY: `len` is the buffer's length in UTF-16 units; on success it is set to the name's length.
        let ok = unsafe { GetComputerNameExW(ComputerNameDnsHostname, buf.as_mut_ptr(), &mut len) };
        if ok == 0 {
            return machine_name_of(None);
        }
        let name = String::from_utf16_lossy(&buf[..(len as usize).min(buf.len())]);
        machine_name_of(Some(&name))
    }
    #[cfg(not(any(unix, windows)))]
    {
        machine_name_of(None)
    }
}

#[derive(serde::Deserialize)]
struct DeviceCodeResponse {
    device_code: String,
    user_code: String,
    verification_uri: String,
    verification_uri_complete: Option<String>,
    expires_in: u64,
    interval: Option<u64>,
}

#[derive(serde::Serialize)]
struct TokenPollRequest {
    device_code: String,
    client_id: String,
    grant_type: String,
}

#[derive(serde::Deserialize)]
struct TokenSuccessResponse {
    access_token: String,
}

#[derive(serde::Deserialize)]
struct TokenErrorResponse {
    error: String,
}

async fn extract_api_error(response: reqwest::Response, status_code: u16) -> CliError {
    let body = match read_body(response, ANSWER_STALL, None).await {
        Ok(body) => body,
        Err(err) => return err,
    };
    let error = match serde_json::from_slice::<TokenErrorResponse>(&body) {
        Ok(body) => body.error,
        Err(_) => format!("HTTP {status_code}"),
    };
    CliError::Api {
        status: Some(status_code),
        error,
        context: None,
    }
}

/// Post `body` as JSON through the bounded transport (SPEC u280
/// `send_bounded`), any failure to reach the server named by its address.
async fn post_json<T: serde::Serialize>(
    client: &reqwest::Client,
    url: &str,
    body: &T,
    server_url: &str,
) -> Result<reqwest::Response, CliError> {
    let unreachable = || CliError::ServerUnreachable {
        url: server_url.to_string(),
    };
    let bytes = serde_json::to_vec(body).map_err(|_| unreachable())?;
    let len = bytes.len();
    send_bounded(
        client
            .post(url)
            .header(reqwest::header::CONTENT_TYPE, "application/json")
            .body(bytes),
        len,
    )
    .await
    .map_err(|err| match err {
        // SPEC u298: a TLS-layer refusal is carried out as it stands.
        CliError::TlsRefused { .. } => err,
        _ => unreachable(),
    })
}

/// Decode an answer read through `read_body`, or `refusal` where it does
/// not decode.
async fn decode<T: serde::de::DeserializeOwned>(
    response: reqwest::Response,
    refusal: &str,
) -> Result<T, CliError> {
    let body = read_body(response, ANSWER_STALL, None).await?;
    serde_json::from_slice(&body).map_err(|_| CliError::Io {
        message: refusal.into(),
    })
}

#[derive(Debug)]
enum PollAction {
    Continue,
    SlowDown(u64),
    Error(CliError),
}

fn classify_poll_error(error: &str, current_interval: u64) -> PollAction {
    match error {
        "authorization_pending" => PollAction::Continue,
        "slow_down" => {
            let new_interval =
                (current_interval + SLOW_DOWN_INCREMENT_SECS).min(MAX_POLL_INTERVAL_SECS);
            PollAction::SlowDown(new_interval)
        }
        "expired_token" => PollAction::Error(CliError::Io {
            message: "device code expired \u{2014} please run 'syns login' again".into(),
        }),
        "access_denied" => PollAction::Error(CliError::Io {
            message: "authorization was denied".into(),
        }),
        other => PollAction::Error(CliError::Api {
            status: Some(400),
            error: format!("login failed: {other}"),
            context: None,
        }),
    }
}

pub struct DeviceAuthFlow;

impl DeviceAuthFlow {
    pub async fn run(server_url: &str) -> Result<String, CliError> {
        // 1. HTTPS enforcement
        if !server_url.starts_with("https://") && !crate::config::is_localhost_url(server_url) {
            return Err(CliError::Config {
                message: crate::config::HTTPS_REQUIRED.into(),
            });
        }

        // 2. The client every request to `BND-public-api` is built as
        // (SPEC u280 `SynsClient::new`).
        let client = api_client(server_url)?;

        // 3. Request device code, naming this computer
        let code_url = format!("{server_url}{DEVICE_CODE_PATH}");
        let response = post_json(
            &client,
            &code_url,
            &serde_json::json!({"client_id": CLIENT_ID, "machine": system_machine_name()}),
            server_url,
        )
        .await?;

        // 4. Handle non-200
        if !response.status().is_success() {
            let code = response.status().as_u16();
            return Err(extract_api_error(response, code).await);
        }

        // 5. Deserialize response
        let device_code_response: DeviceCodeResponse =
            decode(response, "unexpected response from device code endpoint").await?;

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

            let response = post_json(
                &client,
                &token_url,
                &TokenPollRequest {
                    device_code: device_code_response.device_code.clone(),
                    client_id: CLIENT_ID.to_string(),
                    grant_type: "urn:ietf:params:oauth:grant-type:device_code".to_string(),
                },
                server_url,
            )
            .await?;
            let status = response.status();

            if status.is_success() {
                let success: TokenSuccessResponse =
                    decode(response, "unexpected response from token endpoint").await?;
                return Ok(success.access_token);
            }

            if status == reqwest::StatusCode::BAD_REQUEST {
                let error_body: TokenErrorResponse =
                    decode(response, "unexpected response from token endpoint").await?;
                match classify_poll_error(&error_body.error, poll_interval_secs) {
                    PollAction::Continue => continue,
                    PollAction::SlowDown(new_interval) => {
                        poll_interval_secs = new_interval;
                        continue;
                    }
                    PollAction::Error(err) => return Err(err),
                }
            }

            let code = status.as_u16();
            return Err(extract_api_error(response, code).await);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn https_enforcement_rejects_http() {
        let result = DeviceAuthFlow::run("http://example.com").await;
        assert!(
            matches!(result, Err(CliError::Config { ref message }) if message == crate::config::HTTPS_REQUIRED)
        );
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
    fn classify_poll_error_authorization_pending() {
        let action = classify_poll_error("authorization_pending", 5);
        assert!(matches!(action, PollAction::Continue));
    }

    #[test]
    fn classify_poll_error_slow_down_increments() {
        let action = classify_poll_error("slow_down", 5);
        assert!(matches!(action, PollAction::SlowDown(10)));
    }

    #[test]
    fn classify_poll_error_slow_down_caps_at_max() {
        let action = classify_poll_error("slow_down", 58);
        assert!(matches!(action, PollAction::SlowDown(60)));
    }

    #[test]
    fn classify_poll_error_expired_token() {
        let action = classify_poll_error("expired_token", 5);
        assert!(matches!(action, PollAction::Error(CliError::Io { .. })));
    }

    #[test]
    fn classify_poll_error_access_denied() {
        let action = classify_poll_error("access_denied", 5);
        assert!(matches!(action, PollAction::Error(CliError::Io { .. })));
    }

    #[test]
    fn classify_poll_error_unknown_error() {
        let action = classify_poll_error("server_error", 5);
        assert!(matches!(
            action,
            PollAction::Error(CliError::Api {
                status: Some(400),
                ..
            })
        ));
    }

    // The name `syns login` sends as `machine` (FEATURE_SPEC C2): the system's own, as it gives it.

    #[test]
    fn syns_machine_name_goes_out_whole() {
        assert_eq!(
            machine_name_of(Some("Barts-MacBook-Pro.local")),
            "Barts-MacBook-Pro.local"
        );
        assert_eq!(machine_name_of(Some("Zoë's iMac")), "Zoë's iMac");
        assert_eq!(machine_name_of(Some(" a b ")), " a b ");
    }

    #[test]
    fn syns_machine_name_of_bytes_that_are_not_utf8_is_lossy() {
        assert_eq!(machine_name_of_bytes(Some(b"lap\xfftop")), "lap\u{FFFD}top");
        assert_eq!(machine_name_of_bytes(Some(b"\xff\xfe")), "\u{FFFD}\u{FFFD}");
        assert_eq!(machine_name_of_bytes(Some(b"studio")), "studio");
    }

    #[test]
    fn syns_machine_name_over_255_characters_is_cut_to_255() {
        let long = "a".repeat(300);
        assert_eq!(machine_name_of(Some(&long)), "a".repeat(255));
        assert_eq!(machine_name_of(Some(&"a".repeat(255))), "a".repeat(255));
        // Characters, not bytes: 300 two-byte characters keep 255 of them.
        let wide = "é".repeat(300);
        let sent = machine_name_of(Some(&wide));
        assert_eq!(sent.chars().count(), 255);
        assert_eq!(sent, "é".repeat(255));
    }

    #[test]
    fn syns_machine_name_none_or_empty_is_machine() {
        assert_eq!(machine_name_of(None), "machine");
        assert_eq!(machine_name_of(Some("")), "machine");
        assert_eq!(machine_name_of_bytes(None), "machine");
        assert_eq!(machine_name_of_bytes(Some(b"")), "machine");
    }

    #[test]
    fn syns_machine_name_of_the_system_is_what_it_gives() {
        let name = system_machine_name();
        assert!(!name.is_empty());
        assert!(name.chars().count() <= 255);
        // Every computer this runs on (CI's Linux, macOS and Windows runners) has a name: a read that failed would
        // send the fallback, and every such computer would ask to join as `machine`.
        assert_ne!(name, "machine");
        // What `hostname` prints, where it is there to ask; Windows may print it in another case.
        if let Ok(out) = std::process::Command::new("hostname").output() {
            let printed = String::from_utf8_lossy(&out.stdout).trim_end().to_string();
            if !printed.is_empty() {
                #[cfg(unix)]
                assert_eq!(name, printed);
                #[cfg(windows)]
                assert_eq!(name.to_lowercase(), printed.to_lowercase());
            }
        }
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
