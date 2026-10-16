//! roadmap/232 (`D-173`): a publication carries no machine-local file —
//! a socket, pipe or device by its file type, and R2–R7 by the shape of
//! its repository path — beneath the owner's ignore files, outside
//! `--no-default-excludes`, named under `machine-local`.
//!
//! Each test drives the built binary against a mock answering a first
//! publication of `alice/repo`, with `HOME` and `XDG_CONFIG_HOME` set to
//! an empty scratch directory so no global git ignore of the machine
//! running the suite decides what a collection keeps (FEATURE_SPEC
//! § Expected, the runs' environment). Rows are named for FEATURE_SPEC's
//! E rows.

use assert_cmd::Command as AssertCommand;
use serde_json::{Value, json};
use serial_test::serial;
use std::path::PathBuf;
use tempfile::TempDir;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

/// Fixture F's files (FEATURE_SPEC § Expected).
const F: &[(&str, &str)] = &[
    ("README.md", "# readme\n"),
    (
        ".claude/scheduled_tasks.lock",
        "{\"sessionId\":\"s1\",\"pid\":1}",
    ),
    (".claude/settings.local.json", "{\"local\":true}\n"),
    (".claude/.cc-writes/w1", "w\n"),
    (".claude/skills/x/SKILL.md", "# skill\n"),
    (".codex/config.toml", "model = \"x\"\n"),
    (".codex/session.pid", "4242\n"),
    (".vscode/settings.json", "{}\n"),
    (".env.local", "SECRET=1\n"),
    (".env.local.example", "SECRET=\n"),
    ("CLAUDE.local.md", "# local\n"),
    ("Cargo.lock", "# lock\n"),
    ("package-lock.json", "{}\n"),
    ("yarn.lock", "# yarn\n"),
    ("flake.lock", "{}\n"),
    (".claude/skills/x/bun.lock", "{}\n"),
    (".github/workflows/ci.yml", "on: push\n"),
    (".DS_Store", "\0\0\0\x01Bud1"),
    ("media/.DS_Store", "\0\0\0\x01Bud1"),
    ("media/Thumbs.db", "thumbs"),
    ("media/._cover.png", "\0\x05\x16\x07"),
    ("media/desktop.ini", "[.ShellClassInfo]\n"),
    ("media/cover.png", "\u{89}PNG"),
];

/// The eight F-drops, in byte order.
const F_DROPS: [&str; 8] = [
    ".DS_Store",
    ".claude/scheduled_tasks.lock",
    ".claude/settings.local.json",
    ".codex/session.pid",
    ".env.local",
    "media/.DS_Store",
    "media/._cover.png",
    "media/Thumbs.db",
];

/// E1's kept paths, beside the `.syns.yaml` the run writes.
const F_KEPT: [&str; 15] = [
    "README.md",
    ".claude/.cc-writes/w1",
    ".claude/skills/x/SKILL.md",
    ".claude/skills/x/bun.lock",
    ".codex/config.toml",
    ".vscode/settings.json",
    ".env.local.example",
    "CLAUDE.local.md",
    "Cargo.lock",
    "package-lock.json",
    "yarn.lock",
    "flake.lock",
    ".github/workflows/ci.yml",
    "media/cover.png",
    "media/desktop.ini",
];

/// Fixture G (roadmap/237's).
const G: &[(&str, &str)] = &[
    ("notes.md", "notes\n"),
    (".DS_Store", "\0\0\0\x01Bud1"),
    ("a/Thumbs.db", "thumbs"),
    ("a/._x.png", "\0\x05\x16\x07"),
    ("b/desktop.ini", "[.ShellClassInfo]\n"),
];

struct Env {
    rt: tokio::runtime::Runtime,
    server: MockServer,
    /// Holds `dir`, the folder published.
    _work: TempDir,
    dir: PathBuf,
    config: TempDir,
    cache: TempDir,
    home: TempDir,
}

impl Env {
    /// A mock answering a first publication of `alice/repo`, and an empty
    /// folder named `name` to publish.
    fn new(name: &str) -> Env {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let server = rt.block_on(async {
            let server = MockServer::start().await;
            Mock::given(method("GET"))
                .and(path("/api/v1/repos/alice/repo/tree"))
                .respond_with(
                    ResponseTemplate::new(404).set_body_json(json!({"error": "not_found"})),
                )
                .mount(&server)
                .await;
            Mock::given(method("PUT"))
                .and(path("/api/v1/repos/alice/repo/push"))
                .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                    "commitSha": "deadbeef00000000000000000000000000000000",
                    "version": 1,
                    "filesChanged": 1,
                    "created": true,
                })))
                .mount(&server)
                .await;
            server
        });
        let work = tempfile::tempdir().unwrap();
        let dir = std::fs::canonicalize(work.path()).unwrap().join(name);
        std::fs::create_dir_all(&dir).unwrap();
        let config = tempfile::tempdir().unwrap();
        std::fs::write(
            config.path().join("credentials.json"),
            json!({"token": "test-token", "username": "alice"}).to_string(),
        )
        .unwrap();
        Env {
            rt,
            server,
            _work: work,
            dir,
            config,
            cache: tempfile::tempdir().unwrap(),
            home: tempfile::tempdir().unwrap(),
        }
    }

    fn with(name: &str, files: &[(&str, &str)]) -> Env {
        let env = Env::new(name);
        for (path, content) in files {
            env.write(path, content);
        }
        env
    }

    fn write(&self, rel: &str, content: &str) {
        let at = self.dir.join(rel);
        std::fs::create_dir_all(at.parent().unwrap()).unwrap();
        std::fs::write(at, content).unwrap();
    }

    /// `syns [--json] push --name repo <flags> DIR`.
    fn push(&self, json_mode: bool, flags: &[&str]) -> std::process::Output {
        let mut command = AssertCommand::cargo_bin("syns").expect("syns binary");
        command
            .env("SYNS_CONFIG_DIR", self.config.path())
            .env("SYNS_CACHE_DIR", self.cache.path())
            .env("HOME", self.home.path())
            .env("XDG_CONFIG_HOME", self.home.path().join(".config"))
            .env_remove("SYNS_URL")
            .arg("--server")
            .arg(self.server.uri());
        if json_mode {
            command.arg("--json");
        }
        command
            .args(["push", "--name", "repo"])
            .args(flags)
            .arg(&self.dir)
            .output()
            .expect("run syns")
    }

    /// Every path the publications carried, sorted.
    fn published(&self) -> Vec<String> {
        let requests = self
            .rt
            .block_on(async { self.server.received_requests().await.unwrap() });
        let mut paths: Vec<String> = requests
            .iter()
            .filter(|r| r.method.as_str() == "PUT")
            .flat_map(|r| {
                let body: Value = serde_json::from_slice(&r.body).unwrap();
                body["files"]
                    .as_array()
                    .cloned()
                    .unwrap_or_default()
                    .into_iter()
                    .map(|f| f["path"].as_str().unwrap().to_string())
                    .collect::<Vec<_>>()
            })
            .collect();
        paths.sort();
        paths.dedup();
        paths
    }
}

fn stderr_of(out: &std::process::Output) -> String {
    String::from_utf8_lossy(&out.stderr).to_string()
}

fn sorted(paths: &[&str]) -> Vec<String> {
    let mut paths: Vec<String> = paths.iter().map(|p| p.to_string()).collect();
    paths.sort();
    paths
}

/// The machine-local line naming `paths`, in the order given.
fn machine_local_line(paths: &[&str]) -> String {
    format!("  machine-local ({}): {}", paths.len(), paths.join(", "))
}

fn assert_succeeded(out: &std::process::Output) {
    assert!(
        out.status.success(),
        "exit {:?}: {}",
        out.status.code(),
        stderr_of(out)
    );
}

// E1, E2: F publishes every kept path and none of the eight, each named on
// one uncapped line with no hint for it.
#[test]
#[serial]
fn e1_e2_a_push_of_f_publishes_its_kept_files_and_names_the_eight_drops() {
    let env = Env::with("f", F);
    let out = env.push(false, &[]);
    assert_succeeded(&out);

    let mut kept: Vec<&str> = F_KEPT.to_vec();
    kept.push(".syns.yaml");
    assert_eq!(env.published(), sorted(&kept));

    let stderr = stderr_of(&out);
    assert!(
        stderr.contains("warning: 8 file(s) skipped from push"),
        "{stderr}"
    );
    assert!(
        stderr
            .lines()
            .any(|line| line == machine_local_line(&F_DROPS)),
        "{stderr}"
    );
    assert!(!stderr.contains("more"), "{stderr}");
    assert!(!stderr.contains("--no-default-excludes"), "{stderr}");
}

// E3: `--json` lists exactly the eight under `machine_local`.
#[test]
#[serial]
fn e3_the_json_document_lists_the_eight_drops_as_machine_local() {
    let env = Env::with("f", F);
    let out = env.push(true, &[]);
    assert_succeeded(&out);
    let doc: Value = serde_json::from_slice(&out.stdout).expect("one document");
    let skipped: Vec<(String, String)> = doc["skipped"]
        .as_array()
        .expect("skipped")
        .iter()
        .map(|s| {
            (
                s["path"].as_str().unwrap().to_string(),
                s["reason"].as_str().unwrap().to_string(),
            )
        })
        .collect();
    let expected: Vec<(String, String)> = F_DROPS
        .iter()
        .map(|p| (p.to_string(), "machine_local".to_string()))
        .collect();
    assert_eq!(skipped, expected);
}

// E4: `--no-default-excludes` reopens no machine-local file.
#[test]
#[serial]
fn e4_no_default_excludes_keeps_the_eight_dropped() {
    let env = Env::with("f", F);
    let out = env.push(false, &["--no-default-excludes"]);
    assert_succeeded(&out);
    let published = env.published();
    for drop in F_DROPS {
        assert!(!published.contains(&drop.to_string()), "{drop}");
    }
    assert!(
        stderr_of(&out)
            .lines()
            .any(|line| line == machine_local_line(&F_DROPS)),
        "{}",
        stderr_of(&out)
    );
}

// E5: the built-in skip list's line comes first, and `--debug` names the
// source `machine-local`.
#[test]
#[serial]
fn e5_the_summary_orders_the_directory_line_first_and_debug_names_the_source() {
    let env = Env::with("f", F);
    env.write("node_modules/a.js", "x");
    env.write(".claude/x.lock", "x");
    let out = env.push(false, &["--debug"]);
    assert_succeeded(&out);
    let stderr = stderr_of(&out);
    let dir_line = stderr
        .find("  default-excluded directory (1): node_modules/a.js")
        .expect(&stderr);
    let ml_line = stderr.find("  machine-local (9): ").expect(&stderr);
    assert!(dir_line < ml_line, "{stderr}");
    assert!(
        stderr.contains("[debug] skip .claude/x.lock: machine-local (machine-local)"),
        "{stderr}"
    );
}

// E6: a `!` line in `.synsignore` publishes a machine-local file.
#[test]
#[serial]
fn e6_a_negation_in_synsignore_publishes_the_files_it_decides() {
    let env = Env::with("f", F);
    env.write(".synsignore", "!.codex/session.pid\n!.DS_Store\n");
    let out = env.push(false, &[]);
    assert_succeeded(&out);
    let published = env.published();
    for path in [".codex/session.pid", ".DS_Store", "media/.DS_Store"] {
        assert!(
            published.contains(&path.to_string()),
            "{path}: {published:?}"
        );
    }
    assert!(
        stderr_of(&out).lines().any(|line| line
            == machine_local_line(&[
                ".claude/scheduled_tasks.lock",
                ".claude/settings.local.json",
                ".env.local",
                "media/._cover.png",
                "media/Thumbs.db",
            ])),
        "{}",
        stderr_of(&out)
    );
}

// E7: a path the owner's `.gitignore` excludes is attributed to it.
#[test]
#[serial]
fn e7_the_owners_gitignore_attributes_the_paths_it_excludes() {
    let env = Env::with("f", F);
    env.write(".gitignore", ".claude/settings.local.json\nThumbs.db\n");
    let out = env.push(false, &[]);
    assert_succeeded(&out);
    let stderr = stderr_of(&out);
    assert!(
        stderr.contains("  .gitignore rule (2): .claude/settings.local.json, media/Thumbs.db"),
        "{stderr}"
    );
    assert!(stderr.contains("  machine-local (6): "), "{stderr}");
}

// E8: the machine's global git ignore, as Claude Code writes it, decides
// first.
#[test]
#[serial]
fn e8_the_global_git_ignore_attributes_the_paths_it_lists() {
    let env = Env::with("f", F);
    let ignore = env.home.path().join(".config/git/ignore");
    std::fs::create_dir_all(ignore.parent().unwrap()).unwrap();
    std::fs::write(
        &ignore,
        "**/.claude/settings.local.json\n\n**/.claude/.cc-writes/\n",
    )
    .unwrap();
    let out = env.push(false, &[]);
    assert_succeeded(&out);
    let stderr = stderr_of(&out);
    assert!(
        stderr
            .contains("  .gitignore rule (2): .claude/.cc-writes/w1, .claude/settings.local.json"),
        "{stderr}"
    );
    assert!(stderr.contains("  machine-local (7): "), "{stderr}");
    let published = env.published();
    assert!(!published.contains(&".claude/settings.local.json".to_string()));
    assert!(!published.contains(&".claude/.cc-writes/w1".to_string()));
}

// E12: a socket and a FIFO are dropped by their type, neither failing the
// run nor blocking it; a regular file named `*.sock` publishes.
#[cfg(unix)]
#[test]
#[serial]
fn e12_sockets_and_a_fifo_are_dropped_by_type_and_never_read() {
    let env = Env::with("f", F);
    std::fs::create_dir_all(env.dir.join(".x")).unwrap();
    std::fs::create_dir_all(env.dir.join("run")).unwrap();
    let _a = std::os::unix::net::UnixListener::bind(env.dir.join("app.sock")).unwrap();
    let _d = std::os::unix::net::UnixListener::bind(env.dir.join(".x/d.sock")).unwrap();
    let made = std::process::Command::new("mkfifo")
        .arg(env.dir.join("run/p"))
        .status()
        .unwrap();
    assert!(made.success());
    env.write("notes.sock", "a regular file\n");

    // A FIFO read with no writer blocks: the run is bounded here, and
    // killed past its bound.
    let mut child = std::process::Command::new(assert_cmd::cargo::cargo_bin("syns"))
        .env("SYNS_CONFIG_DIR", env.config.path())
        .env("SYNS_CACHE_DIR", env.cache.path())
        .env("HOME", env.home.path())
        .env("XDG_CONFIG_HOME", env.home.path().join(".config"))
        .env_remove("SYNS_URL")
        .arg("--server")
        .arg(env.server.uri())
        .args(["push", "--name", "repo"])
        .arg(&env.dir)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    let started = std::time::Instant::now();
    while child.try_wait().unwrap().is_none() {
        if started.elapsed() > std::time::Duration::from_secs(60) {
            let _ = child.kill();
            let _ = child.wait();
            panic!("the push blocked on the FIFO");
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    let out = child.wait_with_output().unwrap();
    assert_succeeded(&out);
    let stderr = stderr_of(&out);
    assert!(!stderr.contains("could not read"), "{stderr}");
    let line = stderr
        .lines()
        .find(|l| l.starts_with("  machine-local (11): "))
        .unwrap_or_else(|| panic!("{stderr}"));
    for path in ["app.sock", ".x/d.sock", "run/p"] {
        assert!(line.contains(path), "{path}: {line}");
    }
    assert!(env.published().contains(&"notes.sock".to_string()));
}

// E13: a collection holding machine-local drops alone names them as the
// empty publication's cause.
#[test]
#[serial]
fn e13_an_empty_publication_of_machine_local_files_names_them_as_its_cause() {
    let env = Env::with(
        "f",
        &[
            (".claude/a.lock", "x"),
            (".env.local", "x"),
            (".DS_Store", "x"),
        ],
    );
    let out = env.push(false, &["--exclude", ".syns.yaml"]);
    let stderr = stderr_of(&out);
    assert_eq!(out.status.code(), Some(6), "{stderr}");
    assert!(
        stderr.contains(
            "every file is machine-local (a socket or pipe, a lock, pid or .local file in a \
             dot-folder, a .local dotfile, or OS metadata such as .DS_Store); a ! line in \
             .synsignore publishes one"
        ),
        "{stderr}"
    );
    assert!(env.published().is_empty());
}

// E14: `--strict` counts no machine-local drop.
#[test]
#[serial]
fn e14_strict_lands_over_machine_local_drops() {
    let env = Env::with("f", F);
    let out = env.push(false, &["--strict"]);
    assert_eq!(out.status.code(), Some(0), "{}", stderr_of(&out));
    let mut kept: Vec<&str> = F_KEPT.to_vec();
    kept.push(".syns.yaml");
    assert_eq!(env.published(), sorted(&kept));
    assert!(
        stderr_of(&out)
            .lines()
            .any(|line| line == machine_local_line(&F_DROPS)),
        "{}",
        stderr_of(&out)
    );
}

// E15: a whole-repository copy in a directory named `.claude`: the
// directory's name is part of no repository path.
#[test]
#[serial]
fn e15_the_name_of_the_directory_a_copy_stands_in_is_no_dot_folder() {
    let env = Env::with(
        ".claude",
        &[
            ("x.lock", "x"),
            ("settings.local.json", "{}"),
            (".DS_Store", "x"),
        ],
    );
    let out = env.push(false, &[]);
    assert_succeeded(&out);
    assert_eq!(
        env.published(),
        sorted(&[".syns.yaml", "settings.local.json", "x.lock"])
    );
    assert!(
        stderr_of(&out)
            .lines()
            .any(|l| l == machine_local_line(&[".DS_Store"])),
        "{}",
        stderr_of(&out)
    );
}

// E18: roadmap/237 case 1.
#[test]
#[serial]
fn e18_os_metadata_is_dropped_and_desktop_ini_publishes() {
    let env = Env::with("g", G);
    let out = env.push(false, &[]);
    assert_succeeded(&out);
    assert_eq!(
        env.published(),
        sorted(&[".syns.yaml", "b/desktop.ini", "notes.md"])
    );
    assert!(
        stderr_of(&out)
            .lines()
            .any(|l| l == machine_local_line(&[".DS_Store", "a/._x.png", "a/Thumbs.db"])),
        "{}",
        stderr_of(&out)
    );
}

// E19: roadmap/237 case 2, as Q-0027 changed it.
#[test]
#[serial]
fn e19_no_default_excludes_keeps_os_metadata_dropped() {
    let env = Env::with("g", G);
    let out = env.push(false, &["--no-default-excludes"]);
    assert_succeeded(&out);
    assert_eq!(
        env.published(),
        sorted(&[".syns.yaml", "b/desktop.ini", "notes.md"])
    );
    assert!(
        stderr_of(&out)
            .lines()
            .any(|l| l == machine_local_line(&[".DS_Store", "a/._x.png", "a/Thumbs.db"])),
        "{}",
        stderr_of(&out)
    );
}

// E20: roadmap/237 case 3.
#[test]
#[serial]
fn e20_strict_lands_over_os_metadata() {
    let env = Env::with("g", G);
    let out = env.push(false, &["--strict"]);
    assert_eq!(out.status.code(), Some(0), "{}", stderr_of(&out));
    assert_eq!(
        env.published(),
        sorted(&[".syns.yaml", "b/desktop.ini", "notes.md"])
    );
}

// E23: acceptance 232-3's `.pi/` case.
#[test]
#[serial]
fn e23_a_lock_and_a_pid_file_in_a_pi_folder_are_dropped() {
    let env = Env::with("f", F);
    env.write(".pi/x.lock", "x");
    env.write(".pi/a.pid", "1");
    let out = env.push(false, &[]);
    assert_succeeded(&out);
    let published = env.published();
    assert!(!published.contains(&".pi/x.lock".to_string()));
    assert!(!published.contains(&".pi/a.pid".to_string()));
    let mut drops: Vec<&str> = F_DROPS.to_vec();
    drops.extend([".pi/a.pid", ".pi/x.lock"]);
    drops.sort();
    assert!(
        stderr_of(&out)
            .lines()
            .any(|l| l == machine_local_line(&drops)),
        "{}",
        stderr_of(&out)
    );
}
