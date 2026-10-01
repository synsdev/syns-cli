//! Binary-level behaviour of a failure the TLS layer raised during the
//! handshake, told apart from a server that could not be reached (SPEC
//! u298 Tests): a certificate no root trusts ends as `tls_refused:` at exit
//! 1, while a refused connection and a connection closed during the
//! handshake stay `could not reach server at` at exit 3.
//!
//! Each listener here is a thread of this process on the loopback host,
//! stopped and joined before its test returns.

use std::io::Read;
use std::net::{TcpListener, TcpStream};
use std::path::Path;
use std::process::Output;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::JoinHandle;
use std::time::Duration;

use rustls::pki_types::pem::PemObject;
use rustls::pki_types::{CertificateDer, PrivateKeyDer};
use syns_cli::auth::token::TokenStore;
use tempfile::TempDir;

/// What a loopback listener does with each connection it accepts.
#[derive(Clone, Copy)]
enum Answer {
    /// Serve the self-signed `localhost` certificate through a TLS
    /// handshake, which the client refuses.
    SelfSigned,
    /// Close the connection as soon as it is accepted.
    Close,
}

/// A loopback listener answering every connection one way until stopped.
struct Listener {
    port: u16,
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl Listener {
    fn start(answer: Answer) -> Listener {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let port = listener.local_addr().unwrap().port();
        let stop = Arc::new(AtomicBool::new(false));
        let stopping = stop.clone();
        let config = server_config();
        let thread = std::thread::spawn(move || {
            while !stopping.load(Ordering::Relaxed) {
                match listener.accept() {
                    Ok((sock, _)) => {
                        sock.set_nonblocking(false).unwrap();
                        sock.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
                        match answer {
                            Answer::SelfSigned => serve(sock, config.clone()),
                            Answer::Close => drop(sock),
                        }
                    }
                    Err(_) => std::thread::sleep(Duration::from_millis(10)),
                }
            }
        });
        Listener {
            port,
            stop,
            thread: Some(thread),
        }
    }
}

impl Drop for Listener {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

fn fixture(name: &str) -> std::path::PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/tls")
        .join(name)
}

fn server_config() -> Arc<rustls::ServerConfig> {
    let certs = vec![CertificateDer::from_pem_file(fixture("self-signed.pem")).unwrap()];
    let key = PrivateKeyDer::from_pem_file(fixture("self-signed.key")).unwrap();
    Arc::new(
        rustls::ServerConfig::builder_with_provider(Arc::new(
            rustls::crypto::aws_lc_rs::default_provider(),
        ))
        .with_safe_default_protocol_versions()
        .unwrap()
        .with_no_client_auth()
        .with_single_cert(certs, key)
        .unwrap(),
    )
}

/// Run the server side of one handshake until the client ends it.
fn serve(mut sock: TcpStream, config: Arc<rustls::ServerConfig>) {
    let mut conn = rustls::ServerConnection::new(config).unwrap();
    while conn.is_handshaking() {
        if conn.complete_io(&mut sock).is_err() {
            break;
        }
    }
    let mut rest = Vec::new();
    let _ = sock.read_to_end(&mut rest);
}

/// A configuration directory under a scratch directory named for the unit,
/// holding alice's credential.
fn credential() -> TempDir {
    let config = tempfile::Builder::new()
        .prefix("u298-tls-")
        .tempdir()
        .unwrap();
    TokenStore::new(config.path().join("credentials.json"))
        .write_with_username("test-token", Some("alice"))
        .unwrap();
    config
}

fn whoami(config: &TempDir, port: u16) -> Output {
    assert_cmd::Command::cargo_bin("syns")
        .unwrap()
        .env("SYNS_CONFIG_DIR", config.path())
        .env("SYNS_CACHE_DIR", config.path().join("cache"))
        .env_remove("SYNS_URL")
        .env_remove("SYNS_INTEGRATION")
        .env_remove("SYNS_RUN")
        .env_remove("SYNS_TRIGGER")
        .env_remove("SYNS_TASK")
        .args(["whoami", "--server", &format!("https://localhost:{port}")])
        .timeout(Duration::from_secs(60))
        .output()
        .unwrap()
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

// SPEC u298 Tests, `refused_certificate_is_not_unreachable`.
#[test]
fn refused_certificate_is_not_unreachable() {
    let listener = Listener::start(Answer::SelfSigned);
    let config = credential();

    let out = whoami(&config, listener.port);

    let diagnostics = stderr(&out);
    assert_eq!(out.status.code(), Some(1), "{diagnostics}");
    let line = diagnostics
        .lines()
        .find(|l| l.contains("tls_refused:"))
        .unwrap_or_else(|| panic!("no tls_refused line: {diagnostics}"));
    assert!(
        line.trim_start_matches("error: ")
            .starts_with("tls_refused: "),
        "{line}"
    );
    assert!(line.contains("localhost"), "{line}");
    assert!(line.contains("certificate"), "{line}");
    assert!(
        !diagnostics.contains("could not reach server"),
        "{diagnostics}"
    );
}

// SPEC u298 Tests, `refused_connection_stays_unreachable`.
#[test]
fn refused_connection_stays_unreachable() {
    let port = {
        let bound = TcpListener::bind("127.0.0.1:0").unwrap();
        bound.local_addr().unwrap().port()
    };
    let config = credential();

    let out = whoami(&config, port);

    let diagnostics = stderr(&out);
    assert_eq!(out.status.code(), Some(3), "{diagnostics}");
    assert!(
        diagnostics.contains("could not reach server at"),
        "{diagnostics}"
    );
    assert!(!diagnostics.contains("tls_refused"), "{diagnostics}");
}

// SPEC u298 Tests, `handshake_closed_stays_unreachable`.
#[test]
fn handshake_closed_stays_unreachable() {
    let listener = Listener::start(Answer::Close);
    let config = credential();

    let out = whoami(&config, listener.port);

    let diagnostics = stderr(&out);
    assert_eq!(out.status.code(), Some(3), "{diagnostics}");
    assert!(
        diagnostics.contains("could not reach server at"),
        "{diagnostics}"
    );
    assert!(!diagnostics.contains("tls_refused"), "{diagnostics}");
}

// SPEC u298 Files, `src/auth/device.rs`: the device-authorization request's
// refusal is carried out as `send_bounded` answers it, the TLS refusal
// included.
#[test]
fn device_authorization_carries_the_tls_refusal() {
    let listener = Listener::start(Answer::SelfSigned);
    let config = credential();

    let out = assert_cmd::Command::cargo_bin("syns")
        .unwrap()
        .env("SYNS_CONFIG_DIR", config.path())
        .env("SYNS_CACHE_DIR", config.path().join("cache"))
        .env_remove("SYNS_URL")
        .env_remove("SYNS_INTEGRATION")
        .env_remove("SYNS_RUN")
        .env_remove("SYNS_TRIGGER")
        .env_remove("SYNS_TASK")
        .args([
            "login",
            "--server",
            &format!("https://localhost:{}", listener.port),
        ])
        .write_stdin(Vec::new())
        .timeout(Duration::from_secs(60))
        .output()
        .unwrap();

    let diagnostics = stderr(&out);
    assert_eq!(out.status.code(), Some(1), "{diagnostics}");
    assert!(diagnostics.contains("tls_refused: "), "{diagnostics}");
    assert!(
        !diagnostics.contains("could not reach server"),
        "{diagnostics}"
    );
}
