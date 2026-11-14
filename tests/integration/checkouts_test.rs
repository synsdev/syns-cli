//! `syns checkouts [OWNER/NAME]` (cli-checkouts-on-this-machine): the
//! checkouts on this machine whose folder stands and whose own identity
//! file still names the repository, read from the records the working
//! copies keep. Each test records its copies through `WorkingCopy` under a
//! scratch cache, the way every command that opens one records it, and runs
//! the binary over that cache with a scratch home and configuration, and a
//! server address nothing answers at: the listing sends no request.

use assert_cmd::Command as AssertCommand;
use serde_json::{Value, json};
use std::path::{Path, PathBuf};
use syns_cli::config::StoreRoots;
use syns_cli::push::working_copy::WorkingCopy;
use syns_cli::repo::folder::resolve_folder_scope;
use tempfile::TempDir;

/// A scratch machine: a home, a configuration folder, a cache and a
/// folder the checkouts are made in, each its own.
struct Machine {
    home: TempDir,
    config: TempDir,
    cache: TempDir,
    work: TempDir,
}

impl Machine {
    fn new() -> Machine {
        Machine {
            home: tempfile::tempdir().expect("home"),
            config: tempfile::tempdir().expect("config"),
            cache: tempfile::tempdir().expect("cache"),
            work: tempfile::tempdir().expect("work"),
        }
    }

    fn stores(&self) -> StoreRoots {
        StoreRoots {
            default: self.cache.path().to_path_buf(),
            write: self.cache.path().to_path_buf(),
            default_refused: false,
        }
    }

    /// A folder under the work folder, canonical, made where absent.
    fn dir(&self, rel: &str) -> PathBuf {
        let dir = std::fs::canonicalize(self.work.path())
            .expect("work")
            .join(rel);
        std::fs::create_dir_all(&dir).expect("folder");
        dir
    }

    /// A checkout of `owner/name` at `rel`: its root-form identity file,
    /// its copy opened as a command opens it.
    fn checkout(&self, owner: &str, name: &str, rel: &str) -> (PathBuf, WorkingCopy) {
        let dir = self.dir(rel);
        std::fs::write(
            dir.join(".syns.yaml"),
            format!("owner: {owner}\nname: {name}\n"),
        )
        .expect("identity");
        let copy = WorkingCopy::open(&self.stores(), owner, name, &dir).expect("open");
        (dir, copy)
    }

    /// The copy a command run inside the folder at `dir` opens, its
    /// folder-form identity file standing.
    fn folder_copy(&self, dir: &Path) -> WorkingCopy {
        let scope = resolve_folder_scope(dir)
            .expect("a folder scope")
            .expect("a folder form");
        WorkingCopy::open_folder(&self.stores(), &scope).expect("open the folder copy")
    }

    fn run(&self, args: &[&str]) -> std::process::Output {
        AssertCommand::cargo_bin("syns")
            .expect("syns binary")
            .current_dir(self.work.path())
            .env("HOME", self.home.path())
            .env("SYNS_CONFIG_DIR", self.config.path())
            .env("SYNS_CACHE_DIR", self.cache.path())
            .env_remove("SYNS_URL")
            .env_remove("SYNS_INTEGRATION")
            .env_remove("SYNS_RUN")
            .env_remove("SYNS_TRIGGER")
            .arg("--server")
            .arg("http://127.0.0.1:9")
            .args(args)
            .output()
            .expect("run syns")
    }

    fn listed(&self, args: &[&str]) -> Value {
        let mut all = vec!["--json", "checkouts"];
        all.extend_from_slice(args);
        let out = self.run(&all);
        assert_eq!(exit_of(&out), 0, "{}", stderr_of(&out));
        serde_json::from_slice(&out.stdout).expect("one document")
    }

    /// `defaultPath` for `owner/name` on this machine: the scratch home on
    /// unix, whose home folder `HOME` names; Windows' own elsewhere.
    fn default_path(&self, owner: &str, name: &str) -> String {
        let home = if cfg!(windows) {
            dirs::home_dir().expect("a home folder")
        } else {
            self.home.path().to_path_buf()
        };
        home.join("Syns")
            .join(owner)
            .join(name)
            .to_str()
            .expect("a Unicode home")
            .to_string()
    }
}

fn exit_of(out: &std::process::Output) -> i32 {
    out.status.code().unwrap_or(-1)
}

fn stdout_of(out: &std::process::Output) -> String {
    String::from_utf8_lossy(&out.stdout).into_owned()
}

fn stderr_of(out: &std::process::Output) -> String {
    String::from_utf8_lossy(&out.stderr).into_owned()
}

/// A canonical path as the listing prints it: on Windows without the
/// `\\?\` prefix.
fn shown(path: &Path) -> String {
    let text = path.to_str().expect("a Unicode path");
    text.strip_prefix(r"\\?\").unwrap_or(text).to_string()
}

fn row(repo: &str, path: &Path) -> Value {
    json!({"repo": repo, "path": shown(path)})
}

/// Every name the cache holds, counted from it.
fn cache_entries(machine: &Machine) -> Vec<PathBuf> {
    fn walk(root: &Path, dir: &Path, found: &mut Vec<PathBuf>) {
        for entry in std::fs::read_dir(dir).into_iter().flatten().flatten() {
            let path = entry.path();
            found.push(path.strip_prefix(root).unwrap().to_path_buf());
            if path.is_dir() {
                walk(root, &path, found);
            }
        }
    }
    let mut found = Vec::new();
    walk(machine.cache.path(), machine.cache.path(), &mut found);
    found.sort();
    found
}

// Row L1: an empty cache lists nothing, in both modes, and is left empty.
#[test]
fn an_empty_cache_lists_no_checkout_and_is_left_empty() {
    let m = Machine::new();
    let out = m.run(&["checkouts"]);
    assert_eq!(exit_of(&out), 0, "{}", stderr_of(&out));
    assert_eq!(stdout_of(&out).trim(), "No checkouts on this machine.");
    assert_eq!(m.listed(&[]), json!({"checkouts": []}));
    assert!(cache_entries(&m).is_empty(), "{:?}", cache_entries(&m));
}

// Rows L2 and L3: one checkout, listed in both modes; asked by name in
// either letter case, its row and the rule's folder, no `wasAt`.
#[test]
fn a_checkout_is_listed_and_answered_by_name_in_either_letter_case() {
    let m = Machine::new();
    let (dir, _) = m.checkout("alice", "proj", "Syns/alice/proj");
    assert_eq!(
        m.listed(&[]),
        json!({"checkouts": [row("alice/proj", &dir)]})
    );
    let out = m.run(&["checkouts"]);
    assert_eq!(exit_of(&out), 0);
    let table = stdout_of(&out);
    assert!(table.contains("Name") && table.contains("Path"), "{table}");
    assert!(
        table.contains("alice/proj") && table.contains(&shown(&dir)),
        "{table}"
    );

    let expected = json!({
        "repo": "alice/proj",
        "checkouts": [row("alice/proj", &dir)],
        "defaultPath": m.default_path("alice", "proj"),
    });
    assert_eq!(m.listed(&["alice/proj"]), expected);
    assert_eq!(m.listed(&["Alice/Proj"]), expected);
}

// Rows L4 and L17: a repository with no checkout here, no credential and
// no server: the line, the rule's folder to fetch to, exit 0.
#[test]
fn a_repository_with_no_checkout_here_answers_the_folder_to_fetch_to() {
    let m = Machine::new();
    for name in ["none", "9lives"] {
        let out = m.run(&["checkouts", &format!("alice/{name}")]);
        assert_eq!(exit_of(&out), 0, "{}", stderr_of(&out));
        assert_eq!(
            stdout_of(&out).trim(),
            format!("No checkout of alice/{name} on this machine.")
        );
        assert_eq!(
            stderr_of(&out),
            format!(
                "  Get a copy with: syns pull alice/{name} {}\n",
                m.default_path("alice", name)
            )
        );
        assert_eq!(
            m.listed(&[&format!("alice/{name}")]),
            json!({
                "repo": format!("alice/{name}"),
                "checkouts": [],
                "defaultPath": m.default_path("alice", name),
            })
        );
    }
    assert!(cache_entries(&m).is_empty());
}

// Row L5: within one repository the checkout used last comes first.
#[test]
fn the_checkout_used_last_comes_first() {
    let m = Machine::new();
    let (a, copy_a) = m.checkout("alice", "proj", "a");
    let (b, copy_b) = m.checkout("alice", "proj", "b");
    let (z, _) = m.checkout("alice", "aardvark", "z");
    let files = || std::collections::HashMap::from([("x.md".to_string(), "1".to_string())]);
    copy_a.record_base("h1", files()).unwrap();
    std::thread::sleep(std::time::Duration::from_millis(5));
    copy_b.record_base("h1", files()).unwrap();
    assert_eq!(
        m.listed(&[]),
        json!({"checkouts": [row("alice/aardvark", &z), row("alice/proj", &b), row("alice/proj", &a)]})
    );
    std::thread::sleep(std::time::Duration::from_millis(5));
    copy_a.record_base("h2", files()).unwrap();
    assert_eq!(
        m.listed(&["alice/proj"])["checkouts"],
        json!([row("alice/proj", &a), row("alice/proj", &b)])
    );
}

// Rows L7, L9 and L10: a placed folder inside a checkout and one folder
// checked out alone make no row; a repository of its own inside another's
// checkout makes its own beside the checkout around it.
#[test]
fn only_a_repository_root_or_a_shared_folder_under_its_own_name_makes_a_row() {
    let m = Machine::new();
    let (root, _) = m.checkout("alice", "proj", "proj");
    let placed = m.dir("proj/templates/board");
    std::fs::write(
        placed.join(".syns.yaml"),
        "holder: alice/proj\npath: templates/board\n",
    )
    .unwrap();
    m.folder_copy(&placed);

    let alone = m.dir("y");
    std::fs::write(
        alone.join(".syns.yaml"),
        "holder: alice/proj\npath: templates/board\n",
    )
    .unwrap();
    m.folder_copy(&alone);

    let (inner, _) = m.checkout("alice", "inner", "proj/sub");
    assert_eq!(
        m.listed(&[]),
        json!({"checkouts": [row("alice/inner", &inner), row("alice/proj", &root)]})
    );
}

// Row L8: a shared folder pulled by its own name is listed under that name
// beside its holder's checkout.
#[test]
fn a_shared_folder_pulled_by_its_own_name_is_listed_under_that_name() {
    let m = Machine::new();
    let (root, _) = m.checkout("alice", "proj", "proj");
    let shared = m.dir("x");
    std::fs::write(
        shared.join(".syns.yaml"),
        "holder: alice/proj\npath: docs\nshared_as: proj-docs\n",
    )
    .unwrap();
    m.folder_copy(&shared);
    assert_eq!(
        m.listed(&[]),
        json!({"checkouts": [row("alice/proj", &root), row("alice/proj-docs", &shared)]})
    );
}

// Rows L11 and L12: a checkout whose identity file was removed, or whose
// folder was removed, makes no row; asked by name, where it was.
#[test]
fn a_checkout_no_longer_standing_is_answered_by_where_it_was() {
    let m = Machine::new();
    let (dir, _) = m.checkout("alice", "proj", "proj");
    std::fs::remove_file(dir.join(".syns.yaml")).unwrap();
    assert_eq!(m.listed(&[]), json!({"checkouts": []}));
    assert_eq!(m.listed(&["alice/proj"])["wasAt"], json!(shown(&dir)));

    std::fs::remove_dir_all(&dir).unwrap();
    assert_eq!(m.listed(&[]), json!({"checkouts": []}));
    let out = m.run(&["checkouts", "alice/proj"]);
    assert_eq!(exit_of(&out), 0);
    assert_eq!(
        stdout_of(&out).trim(),
        "No checkout of alice/proj on this machine."
    );
    assert_eq!(
        stderr_of(&out),
        format!(
            "  It was at {}; no checkout of it is there now. If you moved it, run syns status in the moved folder: it is found again with its state.\n  Get a copy with: syns pull alice/proj {}\n",
            shown(&dir),
            m.default_path("alice", "proj")
        )
    );
}

// Row M1: a checkout moved by hand, no command run in it yet: no row, and
// by name the path it was at; a scoped folder's record is never `wasAt`.
#[test]
fn a_moved_checkout_is_answered_by_where_it_was_until_a_command_runs_in_it() {
    let m = Machine::new();
    let (p, _) = m.checkout("alice", "proj", "p");
    let placed = m.dir("p/board");
    std::fs::write(
        placed.join(".syns.yaml"),
        "holder: alice/proj\npath: board\n",
    )
    .unwrap();
    m.folder_copy(&placed);
    let q = p.with_file_name("q");
    std::fs::rename(&p, &q).unwrap();
    assert_eq!(m.listed(&[]), json!({"checkouts": []}));
    let answer = m.listed(&["alice/proj"]);
    assert_eq!(answer["checkouts"], json!([]));
    assert_eq!(answer["wasAt"], json!(shown(&p)));

    WorkingCopy::open(&m.stores(), "alice", "proj", &q).unwrap();
    let births_known = std::fs::metadata(&q).and_then(|m| m.created()).is_ok();
    if births_known {
        assert_eq!(m.listed(&[]), json!({"checkouts": [row("alice/proj", &q)]}));
    }
}

// A set-aside never makes a row, though its path holds a checkout.
#[test]
fn a_set_aside_makes_no_row() {
    let m = Machine::new();
    let (dir, copy) = m.checkout("alice", "proj", "proj");
    let held = dir.with_file_name("held");
    std::fs::rename(&dir, &held).unwrap();
    std::fs::create_dir(&dir).unwrap();
    std::fs::remove_dir_all(&held).unwrap();
    std::fs::write(dir.join(".syns.yaml"), "owner: alice\nname: proj\n").unwrap();
    WorkingCopy::open(&m.stores(), "alice", "proj", &dir).unwrap();
    let gone = copy.state_dir.with_file_name(format!(
        "{}.gone",
        copy.state_dir.file_name().unwrap().to_string_lossy()
    ));
    assert!(gone.join("folder.json").is_file());
    std::fs::remove_file(copy.state_dir.join("folder.json")).unwrap();
    assert_eq!(m.listed(&[]), json!({"checkouts": []}));
    assert_eq!(m.listed(&["alice/proj"])["wasAt"], json!(shown(&dir)));
}

// Row L13: a cache removed whole lists nothing; the next open lists again.
#[test]
fn a_removed_cache_lists_nothing_until_the_next_open() {
    let m = Machine::new();
    let (dir, _) = m.checkout("alice", "proj", "proj");
    std::fs::remove_dir_all(m.cache.path()).unwrap();
    assert_eq!(m.listed(&[]), json!({"checkouts": []}));
    assert!(!m.cache.path().exists(), "the listing creates no cache");
    WorkingCopy::open(&m.stores(), "alice", "proj", &dir).unwrap();
    assert_eq!(
        m.listed(&[]),
        json!({"checkouts": [row("alice/proj", &dir)]})
    );
}

// Row L16: a value not spelt OWNER/NAME is the parser's refusal, exit 2,
// on the diagnostic stream in both modes.
#[test]
fn a_value_not_spelt_owner_slash_name_is_refused_by_the_parser() {
    let m = Machine::new();
    for value in ["", ".", "..", "a/b/c", "alice", "/abs/path"] {
        for json in [false, true] {
            let mut args = Vec::new();
            if json {
                args.push("--json");
            }
            args.extend(["checkouts", value]);
            let out = m.run(&args);
            assert_eq!(exit_of(&out), 2, "{value:?}");
            assert!(out.stdout.is_empty(), "{value:?}: {}", stdout_of(&out));
            assert!(
                stderr_of(&out).contains(&format!(
                    "error: invalid value '{value}' for '[OWNER/NAME]': expected OWNER/NAME"
                )),
                "{value:?}: {}",
                stderr_of(&out)
            );
        }
    }
}

// Row L19: thousands of records of removed folders beside one checkout.
#[test]
fn thousands_of_removed_folders_list_the_one_checkout() {
    let m = Machine::new();
    let (dir, copy) = m.checkout("alice", "proj", "proj");
    let repo = copy.state_dir.parent().unwrap().to_path_buf();
    let gone = m.work.path().join("removed");
    for n in 0..5000u32 {
        let home = repo.join(format!("{n:040x}"));
        std::fs::create_dir(&home).unwrap();
        let record = json!({
            "dev": 1, "ino": n, "born": [1, 0],
            "path": gone.join(n.to_string()).to_str().unwrap(), "checkout": true,
        });
        std::fs::write(home.join("folder.json"), record.to_string()).unwrap();
    }
    let started = std::time::Instant::now();
    assert_eq!(
        m.listed(&[]),
        json!({"checkouts": [row("alice/proj", &dir)]})
    );
    assert!(started.elapsed() < std::time::Duration::from_secs(10));
}

// Row L14: a checkout reached through a link is listed at its real path.
#[cfg(unix)]
#[test]
fn a_checkout_reached_through_a_link_is_listed_at_its_real_path() {
    let m = Machine::new();
    let real = m.dir("real");
    std::fs::write(real.join(".syns.yaml"), "owner: alice\nname: proj\n").unwrap();
    let link = m.dir("links").join("proj");
    std::os::unix::fs::symlink(&real, &link).unwrap();
    WorkingCopy::open(&m.stores(), "alice", "proj", &link).unwrap();
    assert_eq!(
        m.listed(&[]),
        json!({"checkouts": [row("alice/proj", &real)]})
    );
}

// Row L20: a cache folder that cannot be read is refused, exit 1.
#[cfg(unix)]
#[test]
fn an_unreadable_cache_folder_is_refused() {
    use std::os::unix::fs::PermissionsExt;
    let m = Machine::new();
    m.checkout("alice", "proj", "proj");
    let copies = m.cache.path().join("working-copies");
    std::fs::set_permissions(&copies, std::fs::Permissions::from_mode(0o000)).unwrap();
    let readable = std::fs::read_dir(&copies).is_ok();
    let out = m.run(&["--json", "checkouts"]);
    std::fs::set_permissions(&copies, std::fs::Permissions::from_mode(0o755)).unwrap();
    if readable {
        return; // run as an account no permission refuses
    }
    assert_eq!(exit_of(&out), 1, "{}", stderr_of(&out));
    let document: Value = serde_json::from_slice(&out.stdout).expect("one document");
    assert_eq!(document["code"], "io_error");
    let error = document["error"].as_str().unwrap();
    assert!(
        error.starts_with(&format!("could not read {}", copies.display())),
        "{error}"
    );
}
