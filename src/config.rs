#![allow(dead_code)] // Functions used by downstream command units (U09, U10+)

use crate::errors::CliError;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::sync::atomic::{AtomicU64, Ordering};
use url::Url;

const DEFAULT_SERVER_URL: &str = match option_env!("SYNS_CLI_DEFAULT_SERVER_URL") {
    Some(url) => url,
    None => "https://syns.dev",
};
const CONFIG_SUBDIR: &str = "syns";
const CACHE_SUBDIR: &str = "syns";

/// The roots a run keeps its stores under (SPEC u298 Contract Surface,
/// `StoreRoots`): `write` is the root the staging directory and the
/// content cache stand under, and `default_refused` sends every working
/// copy's state writes to the copy's in-root home.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StoreRoots {
    pub default: PathBuf,
    pub write: PathBuf,
    pub default_refused: bool,
}

#[cfg(unix)]
const FALLBACK_ROOT_MODE: u32 = 0o700;

static PROBE_COUNTER: AtomicU64 = AtomicU64::new(0);

impl StoreRoots {
    /// Resolve the run's roots (SPEC u298 Behaviour, `StoreRoots::resolve`
    /// 1–3): a configured root answered as it stands, probing nothing;
    /// otherwise the default root wherever it takes a write, and the
    /// per-account fallback root under `temp` only where the default
    /// refuses one as a permission or read-only failure.
    pub fn resolve(configured: Option<&Path>, default: &Path, temp: &Path) -> StoreRoots {
        // 1 — a configured root never falls back.
        if let Some(configured) = configured {
            return StoreRoots {
                default: configured.to_path_buf(),
                write: configured.to_path_buf(),
                default_refused: false,
            };
        }
        // 2 — the default root, where it takes a write.
        let refused = match probe(default, true) {
            Ok(()) => false,
            Err(err) => is_write_refusal(&err),
        };
        if !refused {
            return StoreRoots {
                default: default.to_path_buf(),
                write: default.to_path_buf(),
                default_refused: false,
            };
        }
        // 3 — the fallback root, confined to the account, where it can be
        // made and written; the default root answered otherwise.
        let fallback = fallback_root(temp);
        let write = match confine_fallback(&fallback) {
            Ok(()) => fallback,
            Err(_) => default.to_path_buf(),
        };
        StoreRoots {
            default: default.to_path_buf(),
            write,
            default_refused: true,
        }
    }
}

/// Whether `err` is a write the filesystem refused as a permission or
/// read-only failure, a sandbox's denial included.
fn is_write_refusal(err: &std::io::Error) -> bool {
    matches!(
        err.kind(),
        std::io::ErrorKind::PermissionDenied | std::io::ErrorKind::ReadOnlyFilesystem
    )
}

/// Create `dir` and every missing parent where `create` is set, then
/// create and remove one probe file in it.
fn probe(dir: &Path, create: bool) -> std::io::Result<()> {
    if create {
        std::fs::create_dir_all(dir)?;
    }
    let probe = dir.join(format!(
        ".syns-probe-{}-{}",
        std::process::id(),
        PROBE_COUNTER.fetch_add(1, Ordering::Relaxed)
    ));
    std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&probe)?;
    std::fs::remove_file(&probe)
}

/// `StoreRoots::resolve` 3: create the fallback root at mode `0700` where
/// absent, refuse one standing as a link or owned by another account, set
/// its mode to `0700` and probe it.
fn confine_fallback(root: &Path) -> std::io::Result<()> {
    if is_foreign_or_link(root) {
        return Err(std::io::Error::other(
            "the fallback root stands as a link or is owned by another account",
        ));
    }
    if !root.is_dir() {
        let mut builder = std::fs::DirBuilder::new();
        builder.recursive(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::DirBuilderExt;
            builder.mode(FALLBACK_ROOT_MODE);
        }
        builder.create(root)?;
        if is_foreign_or_link(root) {
            return Err(std::io::Error::other(
                "the fallback root stands as a link or is owned by another account",
            ));
        }
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(root, std::fs::Permissions::from_mode(FALLBACK_ROOT_MODE))?;
    }
    probe(root, false)
}

/// Whether a path standing at `path` is a link, or is owned by an account
/// other than the effective one; false where nothing stands there.
pub(crate) fn is_foreign_or_link(path: &Path) -> bool {
    let Ok(meta) = std::fs::symlink_metadata(path) else {
        return false;
    };
    if meta.file_type().is_symlink() {
        return true;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        // SAFETY: `geteuid` reads the process's effective uid and cannot fail.
        let euid = unsafe { libc::geteuid() };
        if meta.uid() != euid {
            return true;
        }
    }
    false
}

/// The per-account fallback root under `temp` (SPEC u298 Contract
/// Surface, `fallback_root`): `{temp}/syns-{effective uid}` on Unix and
/// `{temp}/syns` elsewhere, so two accounts never share one.
pub fn fallback_root(temp: &Path) -> PathBuf {
    #[cfg(unix)]
    {
        // SAFETY: `geteuid` reads the process's effective uid and cannot fail.
        let euid = unsafe { libc::geteuid() };
        temp.join(format!("syns-{euid}"))
    }
    #[cfg(not(unix))]
    {
        temp.join("syns")
    }
}

#[derive(Clone, Debug)]
pub struct Config {
    server_url: String,
    config_dir: PathBuf,
    /// The root `SYNS_CACHE_DIR` names, none where it names none.
    configured_cache: Option<PathBuf>,
    /// The platform default store root, as `v0.3.7` resolved it.
    default_cache: PathBuf,
    /// The run's roots, resolved on the first call of `stores`.
    stores: OnceLock<StoreRoots>,
}

impl Config {
    pub fn new(server_flag: Option<&str>) -> Result<Config, CliError> {
        // Server URL resolution: flag value > default
        // Note: clap resolves SYNS_URL env var before passing to us via server_flag,
        // so we only need to handle Some(non-empty) vs fallback to default.
        let raw_url = match server_flag {
            Some(s) if !s.is_empty() => s.to_string(),
            _ => DEFAULT_SERVER_URL.to_string(),
        };

        let server_url = raw_url.trim_end_matches('/').to_string();

        // Server URL scheme validation
        let parsed = Url::parse(&server_url).map_err(|_| CliError::Config {
            message: format!("invalid server URL: {server_url}"),
        })?;

        match parsed.scheme() {
            "https" => {}
            "http" if is_localhost(&parsed) => {}
            _ => {
                return Err(CliError::Config {
                    message: HTTPS_REQUIRED.into(),
                });
            }
        }

        // Config directory resolution
        let config_dir = match std::env::var("SYNS_CONFIG_DIR") {
            Ok(val) if !val.is_empty() => PathBuf::from(val),
            _ => dirs::config_dir()
                .ok_or_else(|| CliError::Config {
                    message: "could not determine config directory".into(),
                })?
                .join(CONFIG_SUBDIR),
        };

        // Cache directory resolution: the configured root, and the
        // platform default the run falls back from (SPEC u298).
        let configured_cache = match std::env::var("SYNS_CACHE_DIR") {
            Ok(val) if !val.is_empty() => Some(PathBuf::from(val)),
            _ => None,
        };
        let default_cache = match dirs::cache_dir() {
            Some(path) => path.join(CACHE_SUBDIR),
            None => config_dir.join("cache"),
        };

        Ok(Config {
            server_url,
            config_dir,
            configured_cache,
            default_cache,
            stores: OnceLock::new(),
        })
    }

    /// The run's store roots (SPEC u298, `Config::stores`), resolved once,
    /// on the first call, from `SYNS_CACHE_DIR`, the platform default and
    /// the system temp directory; a run that writes no store probes
    /// nothing.
    pub fn stores(&self) -> &StoreRoots {
        self.stores.get_or_init(|| {
            StoreRoots::resolve(
                self.configured_cache.as_deref(),
                &self.default_cache,
                &std::env::temp_dir(),
            )
        })
    }

    pub fn server_url(&self) -> &str {
        &self.server_url
    }

    pub fn credentials_path(&self) -> PathBuf {
        self.config_dir.join("credentials.json")
    }

    /// The root the staging directory and the content cache stand under:
    /// `stores().write`.
    pub fn cache_dir(&self) -> &Path {
        &self.stores().write
    }
}

/// The one refusal every check of a server address answers off TLS and
/// off the loopback host (SPEC u334 `HTTPS_REQUIRED`), under `CONFIG_ERROR`.
pub const HTTPS_REQUIRED: &str = "server URL must use HTTPS, or http on the loopback host \u{2014} localhost, 127.0.0.1 or [::1] \u{2014} at any port";

fn is_localhost(parsed: &Url) -> bool {
    matches!(
        parsed.host_str(),
        Some("localhost") | Some("127.0.0.1") | Some("::1") | Some("[::1]")
    )
}

pub fn is_localhost_url(url: &str) -> bool {
    match Url::parse(url) {
        Ok(parsed) => is_localhost(&parsed),
        Err(_) => false,
    }
}

pub fn is_safe_to_open(verification_url: &str, server_url: &str) -> bool {
    let Ok(verification) = Url::parse(verification_url) else {
        return false;
    };
    let Ok(server) = Url::parse(server_url) else {
        return false;
    };

    // Check safe scheme
    let safe_scheme = match verification.scheme() {
        "https" => true,
        "http" => is_localhost(&verification),
        _ => false,
    };

    if !safe_scheme {
        return false;
    }

    // Check same origin: scheme + host + port
    // Treat all loopback addresses (localhost, 127.0.0.1, ::1) as equivalent
    let same_scheme = verification.scheme() == server.scheme();
    let same_port = verification.port() == server.port();
    let same_host = if is_localhost(&verification) && is_localhost(&server) {
        true // All loopback variants are equivalent
    } else {
        verification.host_str() == server.host_str()
    };

    same_scheme && same_host && same_port
}

#[cfg(test)]
mod tests {
    use super::*;
    use serial_test::serial;

    #[test]
    fn config_server_url_from_flag() {
        let config = Config::new(Some("https://flag.example.com")).unwrap();
        assert_eq!(config.server_url(), "https://flag.example.com");
    }

    #[test]
    fn config_default_server_url() {
        let config = Config::new(None).unwrap();
        assert_eq!(config.server_url(), "https://syns.dev");
    }

    #[test]
    fn config_strips_trailing_slash() {
        let config = Config::new(Some("https://example.com/")).unwrap();
        assert_eq!(config.server_url(), "https://example.com");
    }

    #[test]
    fn config_rejects_http() {
        let result = Config::new(Some("http://example.com"));
        let err = result.unwrap_err();
        assert!(matches!(err, CliError::Config { ref message } if message == HTTPS_REQUIRED));
        assert_eq!(
            HTTPS_REQUIRED,
            "server URL must use HTTPS, or http on the loopback host \u{2014} localhost, 127.0.0.1 or [::1] \u{2014} at any port"
        );
    }

    #[test]
    fn config_admits_every_loopback_form() {
        let v4 = Config::new(Some("http://127.0.0.1:8080")).unwrap();
        assert_eq!(v4.server_url(), "http://127.0.0.1:8080");
        let v6 = Config::new(Some("http://[::1]:8080")).unwrap();
        assert_eq!(v6.server_url(), "http://[::1]:8080");
    }

    #[test]
    fn config_allows_localhost_http() {
        let config = Config::new(Some("http://localhost:3000")).unwrap();
        assert_eq!(config.server_url(), "http://localhost:3000");
    }

    #[test]
    fn config_allows_127_http() {
        let config = Config::new(Some("http://127.0.0.1:8080")).unwrap();
        assert_eq!(config.server_url(), "http://127.0.0.1:8080");
    }

    #[test]
    fn is_localhost_url_cases() {
        assert!(is_localhost_url("http://localhost"));
        assert!(is_localhost_url("http://localhost:3000"));
        assert!(is_localhost_url("http://localhost:3000/path"));
        assert!(is_localhost_url("http://127.0.0.1"));
        assert!(is_localhost_url("http://127.0.0.1:8080"));
        assert!(!is_localhost_url("http://localhost.evil.com"));
        assert!(!is_localhost_url("http://example.com"));
        assert!(!is_localhost_url("not a url"));
        assert!(!is_localhost_url("http://localhost:80@evil.com"));
    }

    #[test]
    fn is_safe_to_open_cases() {
        // Same origin, HTTPS
        assert!(is_safe_to_open(
            "https://syns.dev/auth/verify",
            "https://syns.dev"
        ));
        // Same origin, localhost HTTP
        assert!(is_safe_to_open(
            "http://localhost:3000/auth/verify",
            "http://localhost:3000"
        ));
        // Different host
        assert!(!is_safe_to_open(
            "https://evil.com/phish",
            "https://syns.dev"
        ));
        // Different scheme
        assert!(!is_safe_to_open("http://syns.dev/auth", "https://syns.dev"));
        // javascript: scheme
        assert!(!is_safe_to_open("javascript:alert(1)", "https://syns.dev"));
        // data: scheme
        assert!(!is_safe_to_open(
            "data:text/html,<h1>phish</h1>",
            "https://syns.dev"
        ));
    }

    #[test]
    fn config_credentials_path() {
        let config = Config::new(Some("https://syns.dev")).unwrap();
        assert!(config.credentials_path().ends_with("credentials.json"));
    }

    #[test]
    #[serial]
    fn config_uses_syns_config_dir_env() {
        // SAFETY: This test runs serially (via #[serial]) so no other thread
        // is reading/writing env vars concurrently.
        unsafe { std::env::set_var("SYNS_CONFIG_DIR", "/tmp/syns-test-config") };
        let config = Config::new(Some("https://syns.dev")).unwrap();
        unsafe { std::env::remove_var("SYNS_CONFIG_DIR") };
        assert_eq!(
            config.credentials_path(),
            PathBuf::from("/tmp/syns-test-config/credentials.json")
        );
    }

    #[test]
    #[serial]
    fn config_uses_syns_cache_dir_env() {
        unsafe { std::env::set_var("SYNS_CACHE_DIR", "/tmp/syns-test-cache") };
        let config = Config::new(Some("https://syns.dev")).unwrap();
        unsafe { std::env::remove_var("SYNS_CACHE_DIR") };
        assert_eq!(config.cache_dir(), Path::new("/tmp/syns-test-cache"));
    }

    /// The entries standing directly in `dir`, sorted.
    fn entries(dir: &Path) -> Vec<std::ffi::OsString> {
        let mut names: Vec<_> = std::fs::read_dir(dir)
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        names.sort();
        names
    }

    /// Whether the run is the superuser, whom a mode-`0500` directory
    /// refuses nothing.
    #[cfg(unix)]
    fn superuser() -> bool {
        // SAFETY: `geteuid` reads the process's effective uid and cannot fail.
        unsafe { libc::geteuid() == 0 }
    }

    #[cfg(unix)]
    fn set_mode(path: &Path, mode: u32) {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)).unwrap();
    }

    #[cfg(unix)]
    fn mode_of(path: &Path) -> u32 {
        use std::os::unix::fs::PermissionsExt;
        std::fs::metadata(path).unwrap().permissions().mode() & 0o777
    }

    // SPEC u298 Tests, `writable_default_root_is_written`.
    #[test]
    fn writable_default_root_is_written() {
        let scratch = tempfile::tempdir().unwrap();
        let default = scratch.path().join("default");
        let temp = scratch.path().join("temp");
        std::fs::create_dir_all(&default).unwrap();
        std::fs::create_dir_all(&temp).unwrap();
        let roots = StoreRoots::resolve(None, &default, &temp);
        assert_eq!(roots.write, default);
        assert_eq!(roots.default, default);
        assert!(!roots.default_refused);
        assert!(entries(&default).is_empty(), "{:?}", entries(&default));
        assert!(entries(&temp).is_empty(), "{:?}", entries(&temp));
    }

    // SPEC u298 Tests, `default_refusing_writes_falls_back`.
    #[cfg(unix)]
    #[test]
    fn default_refusing_writes_falls_back() {
        if superuser() {
            return;
        }
        let scratch = tempfile::tempdir().unwrap();
        let default = scratch.path().join("default");
        let temp = scratch.path().join("temp");
        std::fs::create_dir_all(&default).unwrap();
        std::fs::create_dir_all(&temp).unwrap();
        set_mode(&default, 0o500);
        let roots = StoreRoots::resolve(None, &default, &temp);
        set_mode(&default, 0o700);
        let fallback = temp.join(format!("syns-{}", unsafe { libc::geteuid() }));
        assert_eq!(roots.write, fallback);
        assert_eq!(fallback_root(&temp), fallback);
        assert!(roots.default_refused);
        assert_eq!(roots.default, default);
        assert_eq!(mode_of(&fallback), 0o700);
        assert!(entries(&fallback).is_empty(), "{:?}", entries(&fallback));
    }

    // SPEC u298 Tests, `configured_root_never_falls_back`.
    #[cfg(unix)]
    #[test]
    fn configured_root_never_falls_back() {
        let scratch = tempfile::tempdir().unwrap();
        let configured = scratch.path().join("configured");
        let default = scratch.path().join("default");
        let temp = scratch.path().join("temp");
        std::fs::create_dir_all(&configured).unwrap();
        std::fs::create_dir_all(&temp).unwrap();
        set_mode(&configured, 0o500);
        let roots = StoreRoots::resolve(Some(&configured), &default, &temp);
        set_mode(&configured, 0o700);
        assert_eq!(roots.write, configured);
        assert_eq!(roots.default, configured);
        assert!(!roots.default_refused);
        assert!(!fallback_root(&temp).exists());
        assert!(
            !default.exists(),
            "a configured run probed the default root"
        );
    }

    // SPEC u298 Tests, `fallback_root_standing_as_a_link_is_refused`.
    #[cfg(unix)]
    #[test]
    fn fallback_root_standing_as_a_link_is_refused() {
        if superuser() {
            return;
        }
        let scratch = tempfile::tempdir().unwrap();
        let default = scratch.path().join("default");
        let temp = scratch.path().join("temp");
        let elsewhere = scratch.path().join("elsewhere");
        std::fs::create_dir_all(&default).unwrap();
        std::fs::create_dir_all(&temp).unwrap();
        std::fs::create_dir_all(&elsewhere).unwrap();
        set_mode(&elsewhere, 0o755);
        let link = fallback_root(&temp);
        std::os::unix::fs::symlink(&elsewhere, &link).unwrap();
        set_mode(&default, 0o500);
        let roots = StoreRoots::resolve(None, &default, &temp);
        set_mode(&default, 0o700);
        assert_eq!(roots.write, default);
        assert!(roots.default_refused);
        assert_eq!(std::fs::read_link(&link).unwrap(), elsewhere);
        assert_eq!(mode_of(&elsewhere), 0o755);
        assert!(entries(&elsewhere).is_empty(), "{:?}", entries(&elsewhere));
    }

    #[test]
    fn config_empty_string_uses_default() {
        let config = Config::new(Some("")).unwrap();
        assert_eq!(config.server_url(), "https://syns.dev");
    }

    #[test]
    fn config_malformed_url_returns_error() {
        let result = Config::new(Some(":::bad"));
        assert!(result.is_err());
    }

    #[test]
    fn config_cache_dir_is_set() {
        let config = Config::new(Some("https://syns.dev")).unwrap();
        let cache = config.cache_dir();
        assert!(!cache.as_os_str().is_empty());
    }

    #[test]
    fn is_safe_to_open_port_mismatch() {
        assert!(!is_safe_to_open(
            "https://syns.dev:8443/auth",
            "https://syns.dev"
        ));
    }

    #[test]
    fn is_localhost_url_ipv6() {
        assert!(is_localhost_url("http://[::1]:3000"));
        assert!(is_localhost_url("http://[::1]"));
    }

    #[test]
    fn config_allows_ipv6_localhost_http() {
        let config = Config::new(Some("http://[::1]:3000")).unwrap();
        assert_eq!(config.server_url(), "http://[::1]:3000");
    }

    #[test]
    fn is_safe_to_open_loopback_normalization() {
        // localhost server with 127.0.0.1 verification URL should be accepted
        assert!(is_safe_to_open(
            "http://127.0.0.1:3000/auth/verify",
            "http://localhost:3000"
        ));
        // vice versa
        assert!(is_safe_to_open(
            "http://localhost:3000/auth/verify",
            "http://127.0.0.1:3000"
        ));
        // IPv6 loopback also equivalent
        assert!(is_safe_to_open(
            "http://[::1]:3000/auth/verify",
            "http://localhost:3000"
        ));
    }
}
