//! Binary-level refusal lines of SPEC u334 Tests: the teams invitation and
//! revocation refusals naming their next move after the bare
//! `server error ({status}): {wire form}` line (Q-01), a code with no move
//! rendered bare, and the one refusal every check of a server address off
//! TLS and off the loopback host answers (issues 263 and 264).
//!
//! Each test drives a raw HTTP/1.1 listener of its own, copied from
//! `identifier_segment_test`'s: `GET /api/v1/teams` answers one team `core`
//! owned by `alice`, and every other request takes the next status and
//! `{"error": wire form}` body from the queue the row gives it. Every
//! request is recorded as `METHOD target`.

use std::collections::VecDeque;
use std::path::Path;
use std::sync::{Arc, Mutex};

use serial_test::serial;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

/// The one refusal line the server-address checks answer with.
const HTTPS_REQUIRED_LINE: &str = "error: configuration error: server URL must use HTTPS, or http on the loopback host \u{2014} localhost, 127.0.0.1 or [::1] \u{2014} at any port";

/// One request read off a raw connection: its method and target.
struct RawRequest {
    method: String,
    target: String,
}

/// Reads one HTTP/1.1 request, or `None` where the peer closed first,
/// copied from `identifier_segment_test`'s `read_raw_request`.
async fn read_raw_request(sock: &mut TcpStream) -> Option<RawRequest> {
    let mut buf = Vec::new();
    let mut chunk = [0u8; 8192];
    let header_end = loop {
        if let Some(pos) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
            break pos + 4;
        }
        let n = sock.read(&mut chunk).await.ok()?;
        if n == 0 {
            return None;
        }
        buf.extend_from_slice(&chunk[..n]);
    };
    let head = String::from_utf8_lossy(&buf[..header_end]).to_string();
    let mut lines = head.split("\r\n");
    let mut request_line = lines.next()?.split(' ');
    let method = request_line.next()?.to_string();
    let target = request_line.next()?.to_string();
    let length = lines
        .filter_map(|l| l.split_once(':'))
        .find(|(k, _)| k.trim().eq_ignore_ascii_case("content-length"))
        .and_then(|(_, v)| v.trim().parse::<usize>().ok())
        .unwrap_or(0);
    let mut body = buf[header_end..].to_vec();
    while body.len() < length {
        let n = sock.read(&mut chunk).await.ok()?;
        if n == 0 {
            return None;
        }
        body.extend_from_slice(&chunk[..n]);
    }
    Some(RawRequest { method, target })
}

/// The one team `core` owned by `alice` a `GET /api/v1/teams` answers,
/// as `resolve_team_id_finds_team_by_name` records it.
const TEAMS: &str = r#"{"data":[{"id":"uuid-core","name":"core","description":null,"owner":{"id":"u1","username":"alice","name":"Alice","image":null},"memberCount":1,"role":"owner","createdAt":"2026-01-01T00:00:00Z","updatedAt":"2026-01-01T00:00:00Z"}]}"#;

/// The reason phrase a status line carries.
fn reason(status: u16) -> &'static str {
    match status {
        403 => "Forbidden",
        404 => "Not Found",
        409 => "Conflict",
        422 => "Unprocessable Entity",
        _ => "Error",
    }
}

/// A listener recording each request as `METHOD target` verbatim and
/// answering each request but the teams list from `queue`.
struct Listener {
    /// The runtime the listener lives on, kept alive with it.
    _rt: tokio::runtime::Runtime,
    url: String,
    seen: Arc<Mutex<Vec<String>>>,
}

impl Listener {
    fn start(queue: &[(u16, &str)]) -> Listener {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .expect("runtime");
        let seen = Arc::new(Mutex::new(Vec::new()));
        let queue: Arc<Mutex<VecDeque<(u16, String)>>> = Arc::new(Mutex::new(
            queue.iter().map(|(s, e)| (*s, e.to_string())).collect(),
        ));
        let listener = rt
            .block_on(TcpListener::bind("127.0.0.1:0"))
            .expect("a loopback listener");
        let url = format!(
            "http://127.0.0.1:{}",
            listener.local_addr().expect("an address").port()
        );
        let recorded = seen.clone();
        rt.spawn(async move {
            loop {
                let Ok((mut sock, _)) = listener.accept().await else {
                    return;
                };
                let recorded = recorded.clone();
                let queue = queue.clone();
                tokio::spawn(async move {
                    while let Some(request) = read_raw_request(&mut sock).await {
                        recorded
                            .lock()
                            .expect("requests")
                            .push(format!("{} {}", request.method, request.target));
                        let (status, body) =
                            if request.method == "GET" && request.target == "/api/v1/teams" {
                                (200, TEAMS.to_string())
                            } else {
                                match queue.lock().expect("queue").pop_front() {
                                    Some((status, error)) => {
                                        (status, serde_json::json!({ "error": error }).to_string())
                                    }
                                    None => (500, r#"{"error":"internal_error"}"#.to_string()),
                                }
                            };
                        let answer = format!(
                            "HTTP/1.1 {status} {}\r\ncontent-type: application/json\r\ncontent-length: {}\r\n\r\n{body}",
                            if status == 200 { "OK" } else { reason(status) },
                            body.len()
                        );
                        if sock.write_all(answer.as_bytes()).await.is_err() {
                            return;
                        }
                    }
                });
            }
        });
        Listener { _rt: rt, url, seen }
    }

    fn requests(&self) -> Vec<String> {
        self.seen.lock().expect("requests").clone()
    }
}

/// The built binary with `args` in `cwd`, `home` its config directory and
/// `url` its server, standard input closed, bounded at a minute.
fn run(home: &Path, cwd: &Path, url: &str, args: &[&str]) -> std::process::Output {
    assert_cmd::Command::cargo_bin("syns")
        .expect("the binary")
        .current_dir(cwd)
        .env("SYNS_CONFIG_DIR", home)
        .env("SYNS_CACHE_DIR", home.join("cache"))
        .env("HOME", home)
        .env("XDG_CONFIG_HOME", home)
        .env("SYNS_URL", url)
        .env_remove("SYNS_INTEGRATION")
        .env_remove("SYNS_RUN")
        .env_remove("SYNS_TRIGGER")
        .env_remove("SYNS_TASK")
        .env_remove("CI")
        .args(args)
        .write_stdin(Vec::new())
        .timeout(std::time::Duration::from_secs(60))
        .output()
        .expect("run syns")
}

/// A config directory holding a credential for `alice`, and an empty
/// working directory with no identity file in or above it.
fn directories() -> (tempfile::TempDir, tempfile::TempDir) {
    let home = tempfile::tempdir().expect("config dir");
    syns_cli::auth::token::TokenStore::new(home.path().join("credentials.json"))
        .write_with_username("test-token", Some("alice"))
        .expect("credential");
    (home, tempfile::tempdir().expect("working dir"))
}

/// The run's diagnostic stream, its trailing newline aside.
fn stderr_of(out: &std::process::Output) -> String {
    String::from_utf8_lossy(&out.stderr).trim_end().to_string()
}

// SPEC u334 Tests, the row of this name.
#[test]
#[serial]
fn remove_repo_refusals_name_the_next_move() {
    let listener = Listener::start(&[(403, "forbidden"), (404, "not_found")]);
    let (home, work) = directories();
    let args = ["teams", "remove-repo", "core", "alice/notes", "--yes"];

    let forbidden = run(home.path(), work.path(), &listener.url, &args);
    assert_eq!(forbidden.status.code(), Some(1));
    assert_eq!(
        stderr_of(&forbidden),
        "error: server error (403): forbidden \u{2014} only the owner of team 'core' or an admin on it revokes its grants, whatever you hold on alice/notes; see who with: syns teams members core"
    );

    let not_found = run(home.path(), work.path(), &listener.url, &args);
    assert_eq!(not_found.status.code(), Some(1));
    assert_eq!(
        stderr_of(&not_found),
        "error: server error (404): not_found \u{2014} team 'core' holds no grant on alice/notes that you can see, a revocation that already landed included; see what it holds with: syns teams repos core"
    );

    let revocations = listener
        .requests()
        .into_iter()
        .filter(|r| r.starts_with("DELETE "))
        .count();
    assert_eq!(revocations, 2, "{:?}", listener.requests());
}

// SPEC u334 Tests, the row of this name.
#[test]
#[serial]
fn invitation_refusals_name_the_next_move() {
    let listener = Listener::start(&[
        (404, "not_found"),
        (409, "conflict"),
        (403, "forbidden"),
        (404, "not_found"),
        (409, "conflict"),
        (403, "forbidden"),
    ]);
    let (home, work) = directories();
    let not_found =
        "no invitation 'inv1' is open to you; see the ones that are with: syns teams invitations";

    for (verb, expected) in [
        ("accept", vec![
            format!("error: server error (404): not_found \u{2014} {not_found}"),
            "error: server error (409): conflict \u{2014} invitation 'inv1' is no longer pending, or you already belong to its team; see your teams with: syns teams, and your pending invitations with: syns teams invitations".to_string(),
            "error: server error (403): forbidden \u{2014} invitation 'inv1' has expired or is addressed to another account; sign in as its invitee with: syns login, or ask the team for a new invitation".to_string(),
        ]),
        ("decline", vec![
            format!("error: server error (404): not_found \u{2014} {not_found}"),
            "error: server error (409): conflict \u{2014} invitation 'inv1' is no longer pending; see the ones that are with: syns teams invitations".to_string(),
            "error: server error (403): forbidden \u{2014} invitation 'inv1' is addressed to another account; sign in as its invitee with: syns login".to_string(),
        ]),
    ] {
        for line in expected {
            let out = run(home.path(), work.path(), &listener.url, &["teams", verb, "inv1"]);
            assert_eq!(out.status.code(), Some(1), "{verb}: {}", stderr_of(&out));
            assert_eq!(stderr_of(&out), line, "{verb}");
        }
    }
    assert_eq!(
        listener.requests(),
        [
            vec!["POST /api/v1/teams/invitations/inv1/accept"; 3],
            vec!["POST /api/v1/teams/invitations/inv1/decline"; 3],
        ]
        .concat()
    );
}

// SPEC u334 Tests, the row of this name.
#[test]
#[serial]
fn a_json_refusal_carries_the_same_line() {
    let listener = Listener::start(&[(409, "conflict")]);
    let (home, work) = directories();

    let out = run(
        home.path(),
        work.path(),
        &listener.url,
        &["--json", "teams", "decline", "inv1"],
    );
    assert_eq!(out.status.code(), Some(1));
    let streams = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    let documents: Vec<serde_json::Value> = streams
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| serde_json::from_str(l).expect("a JSON document"))
        .collect();
    assert_eq!(documents.len(), 1, "{streams}");
    assert_eq!(
        documents[0]["error"],
        "server error (409): conflict \u{2014} invitation 'inv1' is no longer pending; see the ones that are with: syns teams invitations"
    );
}

// SPEC u334 Tests, the row of this name.
#[test]
#[serial]
fn an_unmoved_code_renders_bare() {
    let listener = Listener::start(&[(422, "validation_error")]);
    let (home, work) = directories();

    let out = run(
        home.path(),
        work.path(),
        &listener.url,
        &["teams", "decline", "inv1"],
    );
    assert_eq!(out.status.code(), Some(1));
    assert_eq!(
        stderr_of(&out),
        "error: server error (422): validation_error"
    );
}

// SPEC u334 Tests, the row of this name.
#[test]
#[serial]
fn a_server_address_off_tls_and_loopback_names_every_loopback_form() {
    let (home, work) = directories();

    let accept = run(
        home.path(),
        work.path(),
        "http://example.invalid",
        &["teams", "accept", "inv1"],
    );
    assert_eq!(accept.status.code(), Some(1));
    assert_eq!(stderr_of(&accept), HTTPS_REQUIRED_LINE);

    let login = assert_cmd::Command::cargo_bin("syns")
        .expect("the binary")
        .current_dir(work.path())
        .env("SYNS_CONFIG_DIR", home.path())
        .env("SYNS_CACHE_DIR", home.path().join("cache"))
        .env("HOME", home.path())
        .env("XDG_CONFIG_HOME", home.path())
        .env_remove("SYNS_URL")
        .env_remove("SYNS_INTEGRATION")
        .env_remove("SYNS_RUN")
        .env_remove("SYNS_TRIGGER")
        .env_remove("SYNS_TASK")
        .env_remove("CI")
        .args(["login", "--server", "http://example.invalid"])
        .write_stdin(Vec::new())
        .timeout(std::time::Duration::from_secs(60))
        .output()
        .expect("run syns");
    assert_eq!(login.status.code(), Some(1));
    assert_eq!(stderr_of(&login), HTTPS_REQUIRED_LINE);
}

// SPEC u334 Tests, the row of this name.
#[test]
#[serial]
fn every_server_address_check_shares_one_refusal() {
    use syns_cli::config::{Config, HTTPS_REQUIRED};
    use syns_cli::errors::CliError;

    let refused = |result: Result<(), CliError>| match result {
        Err(CliError::Config { message }) => assert_eq!(message, HTTPS_REQUIRED),
        other => panic!("answered {other:?}"),
    };
    refused(Config::new(Some("http://example.invalid")).map(|_| ()));
    refused(syns_cli::client::SynsClient::new("http://example.invalid").map(|_| ()));
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("runtime");
    refused(
        rt.block_on(syns_cli::auth::device::DeviceAuthFlow::run(
            "http://example.invalid",
        ))
        .map(|_| ()),
    );

    for admitted in ["http://127.0.0.1:8080", "http://[::1]:8080"] {
        let config = Config::new(Some(admitted)).expect("a loopback address admitted");
        assert_eq!(config.server_url(), admitted);
    }
}
