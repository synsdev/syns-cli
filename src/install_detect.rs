//! Install-method detection for the `syns` CLI binary.
//!
//! Classifies the running binary into one of the six [`InstallMethod`] variants by
//! `Path::starts_with`-matching its canonicalized parent directory against a
//! platform-specific prefix table (SPEC § 4.1 / PROTOTYPE u200 R-02).
//!
//! Pure logic — no filesystem mutation, no network. The [`CurrentExecutable`] trait
//! lets tests substitute a [`RealCurrentExecutable`] with a fake.

use std::path::{Path, PathBuf};

/// How the running `syns` binary was installed.
///
/// The three "managed" variants (Homebrew / Scoop / Nix) drive the
/// channel-specific redirect message; the three "unmanaged" variants
/// (Cargo / Curl / Unmanaged) drive the in-process upgrade flow.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InstallMethod {
    Homebrew,
    Scoop,
    Nix,
    Cargo,
    Curl,
    Unmanaged,
}

impl InstallMethod {
    /// Returns `true` for package-manager-managed installs that the upgrade
    /// command MUST redirect to (Homebrew, Scoop, Nix).
    pub fn is_managed(self) -> bool {
        matches!(
            self,
            InstallMethod::Homebrew | InstallMethod::Scoop | InstallMethod::Nix
        )
    }

    /// Lowercase wire-form discriminator for `--json` output.
    pub fn wire_form(self) -> &'static str {
        match self {
            InstallMethod::Homebrew => "homebrew",
            InstallMethod::Scoop => "scoop",
            InstallMethod::Nix => "nix",
            InstallMethod::Cargo => "cargo",
            InstallMethod::Curl => "curl",
            InstallMethod::Unmanaged => "unmanaged",
        }
    }
}

/// Indirection over `std::env::current_exe()` + `std::fs::canonicalize` so tests
/// can substitute a fake path without launching a child process.
pub trait CurrentExecutable: Send + Sync {
    fn current_executable_path(&self) -> std::io::Result<PathBuf>;
}

/// Production implementation that calls `std::env::current_exe()` and
/// canonicalizes the result.
pub struct RealCurrentExecutable;

impl CurrentExecutable for RealCurrentExecutable {
    fn current_executable_path(&self) -> std::io::Result<PathBuf> {
        let raw = std::env::current_exe()?;
        let canon = std::fs::canonicalize(&raw)?;
        Ok(strip_verbatim_prefix(canon))
    }
}

/// On Windows, `std::fs::canonicalize` returns paths prefixed with `\\?\`
/// (the verbatim / extended-length prefix). `dirs::data_local_dir()` and
/// `dirs::home_dir()` return non-verbatim paths, so a `Path::starts_with`
/// comparison fails component-by-component when one side has `\\?\` and the
/// other does not. Strip the prefix on Windows so both sides compare apples-
/// to-apples. No-op on non-Windows.
fn strip_verbatim_prefix(path: PathBuf) -> PathBuf {
    #[cfg(target_os = "windows")]
    {
        let s = path.to_string_lossy();
        if let Some(stripped) = s.strip_prefix(r"\\?\") {
            return PathBuf::from(stripped);
        }
    }
    path
}

/// Top-level entry point — classifies the live binary's install method.
pub fn detect_install_method() -> InstallMethod {
    detect_install_method_with(&RealCurrentExecutable)
}

/// Test-injectable variant: same logic as [`detect_install_method`] but reads
/// the path from the provided [`CurrentExecutable`] mock.
pub fn detect_install_method_with(executable: &dyn CurrentExecutable) -> InstallMethod {
    let canonical = match executable.current_executable_path() {
        Ok(p) => p,
        Err(_) => return InstallMethod::Unmanaged,
    };
    let parent = match canonical.parent() {
        Some(p) => p.to_path_buf(),
        None => return InstallMethod::Unmanaged,
    };
    classify_parent(&parent)
}

#[cfg(target_os = "macos")]
fn classify_parent(parent: &Path) -> InstallMethod {
    // Order matters — first matching prefix wins.
    let prefixes: &[(Option<PathBuf>, InstallMethod)] = &[
        (
            Some(PathBuf::from("/opt/homebrew/")),
            InstallMethod::Homebrew,
        ),
        (
            Some(PathBuf::from("/usr/local/Cellar/")),
            InstallMethod::Homebrew,
        ),
        (
            Some(PathBuf::from("/home/linuxbrew/")),
            InstallMethod::Homebrew,
        ),
        (expand_home_relative(".local/bin/"), InstallMethod::Curl),
        (expand_home_relative(".cargo/bin/"), InstallMethod::Cargo),
    ];
    match_prefix(parent, prefixes)
}

#[cfg(target_os = "linux")]
fn classify_parent(parent: &Path) -> InstallMethod {
    let prefixes: &[(Option<PathBuf>, InstallMethod)] = &[
        (
            Some(PathBuf::from("/opt/homebrew/")),
            InstallMethod::Homebrew,
        ),
        (
            Some(PathBuf::from("/home/linuxbrew/")),
            InstallMethod::Homebrew,
        ),
        (expand_home_relative(".local/bin/"), InstallMethod::Curl),
        (expand_home_relative(".cargo/bin/"), InstallMethod::Cargo),
        (expand_home_relative(".nix-profile/"), InstallMethod::Nix),
        (Some(PathBuf::from("/nix/store/")), InstallMethod::Nix),
    ];
    match_prefix(parent, prefixes)
}

#[cfg(target_os = "windows")]
fn classify_parent(parent: &Path) -> InstallMethod {
    let prefixes: &[(Option<PathBuf>, InstallMethod)] = &[
        (
            expand_localappdata("Programs\\syns\\bin\\"),
            InstallMethod::Curl,
        ),
        (
            expand_home_relative("scoop\\apps\\syns\\"),
            InstallMethod::Scoop,
        ),
        (expand_home_relative(".cargo\\bin\\"), InstallMethod::Cargo),
    ];
    match_prefix(parent, prefixes)
}

#[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "windows")))]
fn classify_parent(_parent: &Path) -> InstallMethod {
    InstallMethod::Unmanaged
}

fn match_prefix(parent: &Path, prefixes: &[(Option<PathBuf>, InstallMethod)]) -> InstallMethod {
    for (prefix, method) in prefixes {
        if let Some(p) = prefix
            && parent.starts_with(p)
        {
            return *method;
        }
    }
    InstallMethod::Unmanaged
}

fn expand_home_relative(suffix: &str) -> Option<PathBuf> {
    dirs::home_dir().map(|h| h.join(suffix.trim_start_matches(['/', '\\'])))
}

#[cfg(target_os = "windows")]
fn expand_localappdata(suffix: &str) -> Option<PathBuf> {
    dirs::data_local_dir().map(|d| d.join(suffix.trim_start_matches(['/', '\\'])))
}

#[cfg(test)]
mod tests {
    use super::*;

    struct FakeCurrentExecutable {
        path: PathBuf,
    }
    impl CurrentExecutable for FakeCurrentExecutable {
        fn current_executable_path(&self) -> std::io::Result<PathBuf> {
            Ok(self.path.clone())
        }
    }

    #[test]
    #[cfg(target_os = "macos")]
    fn t1_homebrew_detected_for_opt_homebrew_path() {
        let fake = FakeCurrentExecutable {
            path: PathBuf::from("/opt/homebrew/Cellar/syns/0.2.0/bin/syns"),
        };
        assert_eq!(detect_install_method_with(&fake), InstallMethod::Homebrew);
        assert!(InstallMethod::Homebrew.is_managed());
    }

    #[test]
    #[cfg(target_os = "windows")]
    fn t2_scoop_detected_for_scoop_path() {
        let home = dirs::home_dir().expect("home_dir on windows");
        let path = home
            .join("scoop")
            .join("apps")
            .join("syns")
            .join("current")
            .join("syns.exe");
        let fake = FakeCurrentExecutable { path };
        assert_eq!(detect_install_method_with(&fake), InstallMethod::Scoop);
        assert!(InstallMethod::Scoop.is_managed());
    }

    #[test]
    #[cfg(target_os = "linux")]
    fn t3_nix_detected_for_nix_store_path() {
        let fake = FakeCurrentExecutable {
            path: PathBuf::from("/nix/store/abcd1234-syns-0.2.0/bin/syns"),
        };
        assert_eq!(detect_install_method_with(&fake), InstallMethod::Nix);
        assert!(InstallMethod::Nix.is_managed());
    }

    #[test]
    fn t4_unmanaged_for_manually_placed_binary() {
        // /usr/local/bin/syns matches no prefix on macOS or Linux; on Windows it
        // won't match any Windows prefix either (no LOCALAPPDATA / scoop / cargo).
        let fake = FakeCurrentExecutable {
            path: PathBuf::from("/usr/local/bin/syns"),
        };
        assert_eq!(detect_install_method_with(&fake), InstallMethod::Unmanaged);
        assert!(!InstallMethod::Unmanaged.is_managed());
    }

    #[test]
    fn wire_form_returns_lowercase_for_each_variant() {
        assert_eq!(InstallMethod::Homebrew.wire_form(), "homebrew");
        assert_eq!(InstallMethod::Scoop.wire_form(), "scoop");
        assert_eq!(InstallMethod::Nix.wire_form(), "nix");
        assert_eq!(InstallMethod::Cargo.wire_form(), "cargo");
        assert_eq!(InstallMethod::Curl.wire_form(), "curl");
        assert_eq!(InstallMethod::Unmanaged.wire_form(), "unmanaged");
    }

    #[test]
    fn is_managed_partitions_variants_correctly() {
        assert!(InstallMethod::Homebrew.is_managed());
        assert!(InstallMethod::Scoop.is_managed());
        assert!(InstallMethod::Nix.is_managed());
        assert!(!InstallMethod::Cargo.is_managed());
        assert!(!InstallMethod::Curl.is_managed());
        assert!(!InstallMethod::Unmanaged.is_managed());
    }
}
