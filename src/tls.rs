//! The TLS configuration every client of `BND-public-api` is built with
//! (SPEC u298 Contract Surface, `tls_config`): the deployment's certificate
//! judged by the platform verifier, and by the carried `webpki-root-certs`
//! roots only where the platform refuses it and the run's process cannot
//! reach the platform's trust service (`D-114`).

use std::sync::Arc;

use rustls::client::WebPkiServerVerifier;
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::pki_types::{CertificateDer, ServerName, UnixTime};
use rustls::{DigitallySignedStruct, RootCertStore, SignatureScheme};

use crate::errors::CliError;

/// Judges a server's chain and name through `platform`, and through
/// `carried` only where `platform` refuses and `service_reachable` answers
/// false (SPEC u298, `TrustVerifier`).
#[derive(Debug)]
pub(crate) struct TrustVerifier {
    platform: Arc<dyn ServerCertVerifier>,
    carried: Arc<WebPkiServerVerifier>,
    service_reachable: fn() -> bool,
}

impl TrustVerifier {
    pub(crate) fn new(
        platform: Arc<dyn ServerCertVerifier>,
        carried: Arc<WebPkiServerVerifier>,
        service_reachable: fn() -> bool,
    ) -> TrustVerifier {
        TrustVerifier {
            platform,
            carried,
            service_reachable,
        }
    }
}

impl ServerCertVerifier for TrustVerifier {
    /// SPEC u298 Behaviour, `TrustVerifier::verify_server_cert` 1–3: the
    /// platform's acceptance accepts; its refusal stands wherever the trust
    /// service answers this process, whatever it names; otherwise the
    /// carried roots judge the chain for the host addressed at the instant
    /// asked.
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        intermediates: &[CertificateDer<'_>],
        server_name: &ServerName<'_>,
        ocsp_response: &[u8],
        now: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        // 1 — the platform verifier.
        let refusal = match self.platform.verify_server_cert(
            end_entity,
            intermediates,
            server_name,
            ocsp_response,
            now,
        ) {
            Ok(verified) => return Ok(verified),
            Err(refusal) => refusal,
        };
        // 2 — a refusal given with the trust service reachable stands.
        if (self.service_reachable)() {
            return Err(refusal);
        }
        // 3 — the carried roots.
        self.carried
            .verify_server_cert(end_entity, intermediates, server_name, ocsp_response, now)
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        self.platform.verify_tls12_signature(message, cert, dss)
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        self.platform.verify_tls13_signature(message, cert, dss)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.platform.supported_verify_schemes()
    }
}

/// The Mach bootstrap status a lookup refused by the sandbox answers.
#[cfg(target_os = "macos")]
const BOOTSTRAP_NOT_PRIVILEGED: std::ffi::c_int = 1100;

#[cfg(target_os = "macos")]
unsafe extern "C" {
    static bootstrap_port: u32;
    fn bootstrap_look_up(bp: u32, name: *const std::ffi::c_char, sp: *mut u32) -> std::ffi::c_int;
    fn mach_task_self() -> u32;
    fn mach_port_deallocate(task: u32, name: u32) -> std::ffi::c_int;
}

/// Whether this process can reach the platform's trust service (SPEC u298,
/// `trust_service_reachable`): false only on macOS where the bootstrap
/// lookup of `com.apple.trustd.agent` is refused, true on every other
/// platform. It reads no certificate.
#[cfg(target_os = "macos")]
pub(crate) fn trust_service_reachable() -> bool {
    let name = c"com.apple.trustd.agent";
    // SAFETY: `bootstrap_port` is the task's bootstrap port, set before
    // `main`; the lookup writes a send right into `port` on success, which
    // is released at once.
    let status = unsafe {
        let mut port: u32 = 0;
        let status = bootstrap_look_up(bootstrap_port, name.as_ptr(), &mut port);
        if status == 0 && port != 0 {
            mach_port_deallocate(mach_task_self(), port);
        }
        status
    };
    status != BOOTSTRAP_NOT_PRIVILEGED
}

#[cfg(not(target_os = "macos"))]
pub(crate) fn trust_service_reachable() -> bool {
    true
}

/// The one TLS configuration every client of `BND-public-api` is built
/// with (SPEC u298, `tls_config`): offering `h2` then `http/1.1` by ALPN,
/// its verifier a `TrustVerifier` over the platform verifier, the
/// `webpki-root-certs` roots and `trust_service_reachable`.
pub(crate) fn tls_config() -> Result<rustls::ClientConfig, CliError> {
    let refusal = |err: rustls::Error| CliError::Config {
        message: format!("could not build the TLS configuration: {err}"),
    };
    let provider = Arc::new(rustls::crypto::aws_lc_rs::default_provider());
    let platform: Arc<dyn ServerCertVerifier> =
        Arc::new(rustls_platform_verifier::Verifier::new(provider.clone()).map_err(refusal)?);
    let mut roots = RootCertStore::empty();
    roots.add_parsable_certificates(webpki_root_certs::TLS_SERVER_ROOT_CERTS.iter().cloned());
    let carried = WebPkiServerVerifier::builder_with_provider(Arc::new(roots), provider.clone())
        .build()
        .map_err(|err| CliError::Config {
            message: format!("could not build the TLS configuration: {err}"),
        })?;
    let verifier = TrustVerifier::new(platform, carried, trust_service_reachable);
    let mut config = rustls::ClientConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .map_err(refusal)?
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(verifier))
        .with_no_client_auth();
    config.alpn_protocols = vec![b"h2".to_vec(), b"http/1.1".to_vec()];
    Ok(config)
}

#[cfg(test)]
mod tests {
    use super::*;
    use rustls::CertificateError;
    use rustls::pki_types::pem::PemObject;

    fn fixture(name: &str) -> CertificateDer<'static> {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/tls")
            .join(name);
        CertificateDer::from_pem_file(&path).unwrap()
    }

    /// A platform verifier refusing every chain as untrusted.
    #[derive(Debug)]
    struct Refusing;

    impl ServerCertVerifier for Refusing {
        fn verify_server_cert(
            &self,
            _end_entity: &CertificateDer<'_>,
            _intermediates: &[CertificateDer<'_>],
            _server_name: &ServerName<'_>,
            _ocsp_response: &[u8],
            _now: UnixTime,
        ) -> Result<ServerCertVerified, rustls::Error> {
            Err(rustls::Error::InvalidCertificate(
                CertificateError::UnknownIssuer,
            ))
        }

        fn verify_tls12_signature(
            &self,
            _message: &[u8],
            _cert: &CertificateDer<'_>,
            _dss: &DigitallySignedStruct,
        ) -> Result<HandshakeSignatureValid, rustls::Error> {
            Err(rustls::Error::General("unused".into()))
        }

        fn verify_tls13_signature(
            &self,
            _message: &[u8],
            _cert: &CertificateDer<'_>,
            _dss: &DigitallySignedStruct,
        ) -> Result<HandshakeSignatureValid, rustls::Error> {
            Err(rustls::Error::General("unused".into()))
        }

        fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
            Vec::new()
        }
    }

    /// The carried verifier over the test root alone.
    fn carried() -> Arc<WebPkiServerVerifier> {
        let mut roots = RootCertStore::empty();
        roots.add(fixture("root.pem")).unwrap();
        WebPkiServerVerifier::builder_with_provider(
            Arc::new(roots),
            Arc::new(rustls::crypto::aws_lc_rs::default_provider()),
        )
        .build()
        .unwrap()
    }

    fn verify(
        service_reachable: fn() -> bool,
        leaf: &str,
        host: &'static str,
    ) -> Result<ServerCertVerified, rustls::Error> {
        let verifier = TrustVerifier::new(Arc::new(Refusing), carried(), service_reachable);
        verifier.verify_server_cert(
            &fixture(leaf),
            &[],
            &ServerName::try_from(host).unwrap(),
            &[],
            UnixTime::now(),
        )
    }

    // SPEC u298 Tests, `unreachable_trust_service_admits_carried_roots`.
    #[test]
    fn unreachable_trust_service_admits_carried_roots() {
        let verified = verify(|| false, "localhost.pem", "localhost");
        assert!(verified.is_ok(), "{verified:?}");
    }

    // SPEC u298 Tests, `reachable_trust_service_verdict_stands`.
    #[test]
    fn reachable_trust_service_verdict_stands() {
        let verified = verify(|| true, "localhost.pem", "localhost");
        assert_eq!(
            verified.err(),
            Some(rustls::Error::InvalidCertificate(
                CertificateError::UnknownIssuer
            ))
        );
    }

    // SPEC u298 Tests, `carried_roots_judge_the_host_addressed`.
    #[test]
    fn carried_roots_judge_the_host_addressed() {
        assert!(verify(|| false, "other.pem", "other.test").is_ok());
        let verified = verify(|| false, "other.pem", "localhost");
        assert!(
            matches!(
                verified,
                Err(rustls::Error::InvalidCertificate(
                    CertificateError::NotValidForName
                        | CertificateError::NotValidForNameContext { .. }
                ))
            ),
            "{verified:?}"
        );
    }

    /// The environment variable choosing the probe's sandboxed branch.
    #[cfg(target_os = "macos")]
    const PROBE_CHILD: &str = "SYNS_U298_TRUST_PROBE_CHILD";

    // SPEC u298 Tests, `trust_service_probe_reads_a_denied_lookup`: the
    // probe run plain, then this test re-run under a profile denying the
    // lookup, where it answers false.
    #[cfg(target_os = "macos")]
    #[test]
    fn trust_service_probe_reads_a_denied_lookup() {
        if std::env::var_os(PROBE_CHILD).is_some() {
            assert!(
                !trust_service_reachable(),
                "the denied lookup read as reachable"
            );
            return;
        }
        assert!(
            trust_service_reachable(),
            "the plain lookup read as refused"
        );
        let profile = r#"(version 1)(allow default)(deny mach-lookup (global-name "com.apple.trustd.agent"))"#;
        let child = std::process::Command::new("sandbox-exec")
            .arg("-p")
            .arg(profile)
            .arg(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "tls::tests::trust_service_probe_reads_a_denied_lookup",
                "--test-threads=1",
            ])
            .env(PROBE_CHILD, "1")
            .output()
            .unwrap();
        let printed = String::from_utf8_lossy(&child.stdout);
        assert!(
            child.status.success(),
            "{printed}{}",
            String::from_utf8_lossy(&child.stderr)
        );
        assert!(printed.contains("1 passed"), "{printed}");
    }

    // SPEC u298 Contract Surface, `tls_config`: `h2` then `http/1.1` by
    // ALPN, as reqwest's own configuration offers them.
    #[test]
    fn the_configuration_offers_h2_then_http_1_1() {
        let config = tls_config().unwrap();
        assert_eq!(
            config.alpn_protocols,
            vec![b"h2".to_vec(), b"http/1.1".to_vec()]
        );
    }
}
