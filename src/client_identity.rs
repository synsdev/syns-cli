//! Who sends a request (roadmap 277, `D-089`, `D-238`): the caller name a
//! program running `syns` gives itself in `SYNS_CALLER`, carried in the
//! `User-Agent`, and the anonymous install id every request carries in
//! `X-Syns-Install-Id`.
//!
//! Both only reach the server's and the engine's log lines. No part of
//! syns reads meaning from either: syns defines no caller names, sets
//! none by default and infers none, and neither value changes an answer.

use std::ffi::OsStr;
use std::io::Write;
use std::path::Path;

/// The environment variable a program running `syns` names itself in.
pub const CALLER_ENV: &str = "SYNS_CALLER";

/// The header carrying the install id.
pub const INSTALL_ID_HEADER: &str = "X-Syns-Install-Id";

/// The file in the configuration directory, beside the credential, the
/// install id is kept in.
pub const INSTALL_ID_FILE: &str = "install-id";

/// The most characters a caller name holds: the server's `CALLER_BOUND`.
const CALLER_MAX: usize = 32;

/// The most characters an install id holds: the server's bound.
const INSTALL_ID_MAX: usize = 64;

/// Whether `name` is within the server's caller bound,
/// `^[a-z0-9][a-z0-9._-]{0,31}$`.
pub fn is_caller_name(name: &str) -> bool {
    let bytes = name.as_bytes();
    match bytes.first() {
        Some(first) if first.is_ascii_lowercase() || first.is_ascii_digit() => {}
        _ => return false,
    }
    bytes.len() <= CALLER_MAX
        && bytes.iter().all(|b| {
            b.is_ascii_lowercase() || b.is_ascii_digit() || matches!(b, b'.' | b'_' | b'-')
        })
}

/// Whether `id` is within the server's install-id bound,
/// `^[A-Za-z0-9_-]{1,64}$`.
pub fn is_install_id(id: &str) -> bool {
    (1..=INSTALL_ID_MAX).contains(&id.len())
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-'))
}

/// The caller a `SYNS_CALLER` value names: the value itself where it is
/// within the bound, none where it is unset, empty, not UTF-8 or outside
/// the bound. Nothing is made up in its place.
pub fn caller_name(value: Option<&OsStr>) -> Option<&str> {
    value
        .and_then(OsStr::to_str)
        .filter(|name| is_caller_name(name))
}

/// The `User-Agent` a run sends: `syns/<version> (<caller>)` for a caller
/// within the bound, and `syns/<version>` otherwise, never empty
/// (`D-089`).
pub fn user_agent(caller: Option<&str>) -> String {
    match caller {
        Some(caller) => format!("{} ({caller})", crate::client::USER_AGENT),
        None => crate::client::USER_AGENT.to_string(),
    }
}

/// The `User-Agent` this process sends, from its `SYNS_CALLER`.
pub fn process_user_agent() -> String {
    let value = std::env::var_os(CALLER_ENV);
    user_agent(caller_name(value.as_deref()))
}

/// A fresh install id: 16 random bytes in lowercase hex.
fn fresh_install_id() -> Option<String> {
    let mut bytes = [0u8; 16];
    rustls::crypto::aws_lc_rs::default_provider()
        .secure_random
        .fill(&mut bytes)
        .ok()?;
    Some(bytes.iter().fold(String::with_capacity(32), |mut acc, b| {
        use std::fmt::Write as _;
        let _ = write!(acc, "{b:02x}");
        acc
    }))
}

/// The install id kept in `config_dir`, read where one within the bound
/// stands there, and otherwise made and kept there. None where none
/// stands and the directory takes no write: a run that cannot keep an id
/// sends none rather than a fresh one each time.
///
/// Two first runs racing each other keep one id: the file is linked into
/// place only where none stands yet, and the run that loses reads the
/// winner's.
pub fn install_id(config_dir: &Path) -> Option<String> {
    let file = config_dir.join(INSTALL_ID_FILE);
    if let Some(id) = read_install_id(&file) {
        return Some(id);
    }
    let id = fresh_install_id()?;
    create_config_dir(config_dir).ok()?;
    let mut staged = tempfile::NamedTempFile::new_in(config_dir).ok()?;
    staged.write_all(id.as_bytes()).ok()?;
    staged.flush().ok()?;
    match staged.persist_noclobber(&file) {
        Ok(_) => Some(id),
        Err(err) if err.error.kind() == std::io::ErrorKind::AlreadyExists => {
            match read_install_id(&file) {
                Some(kept) => Some(kept),
                // A file outside the bound stands there: it is replaced.
                None => err.file.persist(&file).ok().map(|_| id),
            }
        }
        // A directory admitting no link (some network and FAT volumes)
        // takes a rename instead.
        Err(err) => err.file.persist(&file).ok().map(|_| id),
    }
}

/// Creates the configuration folder where none stands, as the credential
/// store does: owner-only (`0700`) on unix, so the credential later kept
/// in it is never in a folder another local user can list.
pub(crate) fn create_config_dir(config_dir: &Path) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        std::fs::DirBuilder::new()
            .mode(0o700)
            .recursive(true)
            .create(config_dir)
    }
    #[cfg(not(unix))]
    {
        std::fs::create_dir_all(config_dir)
    }
}

fn read_install_id(file: &Path) -> Option<String> {
    let text = std::fs::read_to_string(file).ok()?;
    let id = text.trim();
    is_install_id(id).then(|| id.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    // X2: the caller bound, each edge.
    #[test]
    fn a_caller_name_is_admitted_only_within_the_bound() {
        let thirty_two = "a".repeat(32);
        let thirty_three = "a".repeat(33);
        for (value, admitted) in [
            ("tool-a", true),
            ("9tool", true),
            ("a", true),
            ("0", true),
            ("a.b_c-d", true),
            (thirty_two.as_str(), true),
            (thirty_three.as_str(), false),
            ("Tool-a", false),
            ("TOOL", false),
            ("tool a", false),
            (" tool", false),
            (".", false),
            ("..", false),
            ("-tool", false),
            ("_tool", false),
            (".tool", false),
            ("", false),
            ("tool/a", false),
            ("tool\u{e9}", false),
            ("tool\n", false),
        ] {
            assert_eq!(is_caller_name(value), admitted, "{value:?}");
            let expected = admitted.then_some(value);
            assert_eq!(caller_name(Some(OsStr::new(value))), expected, "{value:?}");
        }
        assert_eq!(caller_name(None), None);
    }

    #[cfg(unix)]
    #[test]
    fn a_caller_name_not_utf8_names_no_caller() {
        use std::os::unix::ffi::OsStrExt;
        assert_eq!(caller_name(Some(OsStr::from_bytes(b"tool\xff"))), None);
    }

    // X2: the `User-Agent` is never empty and carries no made-up caller.
    #[test]
    fn the_user_agent_names_the_build_and_an_admitted_caller_alone() {
        let plain = concat!("syns/", env!("CARGO_PKG_VERSION"));
        assert_eq!(user_agent(None), plain);
        assert_eq!(user_agent(Some("tool-a")), format!("{plain} (tool-a)"));
    }

    // X3: an id is made once, kept, and read back on every later run.
    #[test]
    fn an_install_id_is_made_once_and_kept() {
        let dir = tempfile::tempdir().unwrap();
        let config = dir.path().join("not-yet");
        let first = install_id(&config).unwrap();
        assert!(is_install_id(&first), "{first}");
        assert_eq!(first.len(), 32);
        assert_eq!(install_id(&config).unwrap(), first);
        assert_eq!(
            std::fs::read_to_string(config.join(INSTALL_ID_FILE)).unwrap(),
            first
        );
        // Only the id stands there: no staged file is left behind.
        assert_eq!(std::fs::read_dir(&config).unwrap().count(), 1);
    }

    // I1: a configuration folder the install id makes first is owner-only,
    // as the credential store makes it.
    #[cfg(unix)]
    #[test]
    fn the_configuration_folder_the_install_id_makes_is_owner_only() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let config = dir.path().join("syns");
        install_id(&config).unwrap();
        let mode = std::fs::metadata(&config).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o700, "{mode:o}");
    }

    // X3: a kept file outside the bound is replaced by a fresh id.
    #[test]
    fn an_install_id_outside_the_bound_is_replaced() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(INSTALL_ID_FILE), "not an id!\n").unwrap();
        let id = install_id(dir.path()).unwrap();
        assert!(is_install_id(&id), "{id}");
        assert_eq!(install_id(dir.path()).unwrap(), id);
    }

    // X3: a directory taking no write keeps no id, and the run sends none.
    #[cfg(unix)]
    #[test]
    fn a_directory_taking_no_write_keeps_no_install_id() {
        use std::os::unix::fs::PermissionsExt;
        // SAFETY: `geteuid` reads the process's effective uid.
        if unsafe { libc::geteuid() } == 0 {
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o500)).unwrap();
        assert_eq!(install_id(dir.path()), None);
        assert_eq!(install_id(&dir.path().join("below")), None);
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 0);
    }

    // X3: an id kept in a directory that takes no write is still read.
    #[cfg(unix)]
    #[test]
    fn a_kept_install_id_is_read_from_a_directory_taking_no_write() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let id = install_id(dir.path()).unwrap();
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o500)).unwrap();
        assert_eq!(install_id(dir.path()), Some(id));
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
    }

    #[test]
    fn the_install_id_bound() {
        assert!(is_install_id("6f1c0b9e4a7d4f539a512d8e0c7b3a14"));
        assert!(is_install_id("A-z_0"));
        assert!(is_install_id(&"a".repeat(64)));
        assert!(!is_install_id(&"a".repeat(65)));
        assert!(!is_install_id(""));
        assert!(!is_install_id("a b"));
        assert!(!is_install_id("a.b"));
    }
}
