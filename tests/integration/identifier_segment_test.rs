//! Binary-level behaviour of every typed identifier a collaborator, link,
//! team or user command puts into an address (SPEC u333 Tests, issue 258):
//! an empty, `.` or `..` identifier refused before any request, and every
//! other one sent as one percent-encoded segment.
//!
//! Each test drives a raw HTTP/1.1 listener of its own rather than
//! wiremock, so the request target is recorded verbatim as it left the
//! binary: a `DELETE` answered `204` and every other request `200` with a
//! body its verb decodes. The binary runs from an empty directory with a
//! config directory holding a credential for `alice`, and `SYNS_URL`
//! naming the listener.

use std::path::Path;
use std::sync::{Arc, Mutex};

use serial_test::serial;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

/// One request read off a raw connection: its method and target.
struct RawRequest {
    method: String,
    target: String,
}

/// Reads one HTTP/1.1 request, or `None` where the peer closed first,
/// copied from `convergence_test`'s `read_raw_request`.
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

const COLLABORATOR: &str = r#"{"user":{"id":"u1","name":"Carol","username":"carol","email":"carol@example.test","emailVerified":true,"image":null,"createdAt":"2026-10-02T00:00:00Z","updatedAt":"2026-10-02T00:00:00Z"},"role":"write","addedBy":"alice","createdAt":"2026-10-02T00:00:00Z"}"#;
const LINKS: &str = r#"{"links":[]}"#;
const MEMBER: &str = r#"{"user":{"id":"u1","username":"alice","name":"Alice","email":null,"image":null},"role":"member","joined_at":"2026-10-02T00:00:00Z","joinedAt":"2026-10-02T00:00:00Z"}"#;

/// The answer to one request: `204` to a `DELETE` outside the link
/// routes, and otherwise `200` with a body the verb decodes.
fn answer(request: &RawRequest) -> String {
    let body = if request.target.starts_with("/api/v1/me/links") {
        LINKS
    } else if request.method == "DELETE" {
        return "HTTP/1.1 204 No Content\r\ncontent-length: 0\r\n\r\n".to_string();
    } else if request.target.starts_with("/api/v1/teams/") {
        MEMBER
    } else {
        COLLABORATOR
    };
    format!(
        "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\n\r\n{body}",
        body.len()
    )
}

/// A listener recording each request as `METHOD target` verbatim.
struct Listener {
    /// The runtime the listener lives on, kept alive with it.
    _rt: tokio::runtime::Runtime,
    url: String,
    seen: Arc<Mutex<Vec<String>>>,
}

impl Listener {
    fn start() -> Listener {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .expect("runtime");
        let seen = Arc::new(Mutex::new(Vec::new()));
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
                tokio::spawn(async move {
                    while let Some(request) = read_raw_request(&mut sock).await {
                        recorded
                            .lock()
                            .expect("requests")
                            .push(format!("{} {}", request.method, request.target));
                        if sock.write_all(answer(&request).as_bytes()).await.is_err() {
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

// SPEC u333 Tests, the row of this name.
#[test]
#[serial]
fn dot_segment_identifiers_send_nothing() {
    let listener = Listener::start();
    let (home, work) = directories();

    for (args, label) in [
        (
            vec![
                "collaborators",
                "remove",
                "--repo",
                "alice/work",
                "--yes",
                "--",
                "..",
            ],
            "USER_ID",
        ),
        (
            vec![
                "collaborators",
                "role",
                "--repo",
                "alice/work",
                "--role=admin",
                "--",
                "..",
            ],
            "USER_ID",
        ),
        (
            vec![
                "collaborators",
                "remove",
                "--repo",
                "alice/work",
                "--yes",
                "--",
                ".",
            ],
            "USER_ID",
        ),
        (
            vec![
                "collaborators",
                "role",
                "--repo",
                "alice/work",
                "--role=write",
                "--",
                ".",
            ],
            "USER_ID",
        ),
        (vec!["links", "remove", "--", ".."], "LINK_ID"),
        (vec!["teams", "accept", "--", ".."], "INVITATION_ID"),
        (
            vec![
                "collaborators",
                "remove",
                "--repo",
                "alice/work",
                "--yes",
                "--",
                "",
            ],
            "USER_ID",
        ),
        (
            vec!["links", "update", "--label", "x", "--", "."],
            "LINK_ID",
        ),
        (vec!["teams", "decline", "--", ".."], "INVITATION_ID"),
        (
            vec!["teams", "add-repo", "core", "../x", "--role", "read"],
            "OWNER/REPO",
        ),
        (
            vec!["teams", "remove-repo", "core", "alice/..", "--yes"],
            "OWNER/REPO",
        ),
        (vec!["user", ".."], "USERNAME"),
    ] {
        let out = run(home.path(), work.path(), &listener.url, &args);
        let stderr = String::from_utf8_lossy(&out.stderr).to_string();
        assert_eq!(out.status.code(), Some(1), "{args:?}: {stderr}");
        assert!(
            stderr.starts_with("error: configuration error: "),
            "{args:?}: {stderr}"
        );
        assert!(stderr.contains(label), "{args:?}: {stderr}");
    }
    assert!(listener.requests().is_empty(), "{:?}", listener.requests());
}

// SPEC u333 Tests, the row of this name.
#[test]
#[serial]
fn typed_identifiers_travel_as_one_encoded_segment() {
    let listener = Listener::start();
    let (home, work) = directories();

    for args in [
        vec![
            "collaborators",
            "remove",
            "--repo",
            "alice/work",
            "--yes",
            "--",
            "a/b c",
        ],
        vec![
            "collaborators",
            "role",
            "--repo",
            "alice/work",
            "--role",
            "write",
            "--",
            "%2e%2e",
        ],
        vec!["links", "remove", "--", "x/y"],
        vec!["teams", "accept", "--", "a/b"],
    ] {
        run(home.path(), work.path(), &listener.url, &args);
    }

    assert_eq!(
        listener.requests(),
        vec![
            "DELETE /api/v1/repos/alice/work/collaborators/a%2Fb%20c",
            "PATCH /api/v1/repos/alice/work/collaborators/%252e%252e",
            "DELETE /api/v1/me/links/x%2Fy",
            "POST /api/v1/teams/invitations/a%2Fb/accept",
        ]
    );
}
