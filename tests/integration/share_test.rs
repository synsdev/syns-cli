//! Binary-level behaviour of sharing a folder under an identity of its
//! own, reading which identity it stands under, stopping the sharing, and
//! aiming the collaborator commands at that identity (SPEC u300 Tests);
//! and marking a folder with a visibility of its own, naming it on the
//! lookup, and keeping a marked folder's name across an unshare (SPEC
//! u329 Tests).
//!
//! Every binary row of those tables stands here under the name the table
//! gives it; `share_name_problem_weighs_the_repository_name_kind`,
//! `offered_share_name_takes_the_last_segment_lower_cased`,
//! `the_ask_takes_another_name_where_the_name_is_held` and
//! `a_moved_head_under_the_share_asks_no_other_name` stand in the tests
//! module of `src/commands/share.rs`. Each test drives one deployment of
//! its own: a mock answering `EP-get-repo` for the holder with `role`
//! `owner` and `sharedFolder` `false` where the row names no other
//! answer, a config directory holding a credential for `alice`, a cache
//! directory, and a working directory holding the checkout of the row's
//! holder.

use assert_cmd::Command as AssertCommand;
use serde_json::{Value, json};
use serial_test::serial;
use std::path::{Path, PathBuf};
use wiremock::matchers::{method, path, path_regex};
use wiremock::{Mock, MockServer, Request, ResponseTemplate};

/// The name kind line `share_name_problem` writes for `name`.
fn name_kind_line(name: &str) -> String {
    format!(
        "a name holds lower-case letters, digits, ., _ and -, opens with a letter or digit, ends with neither . nor -, carries no .. or -- run and runs 1 to 100 characters, and is none of con, prn, aux, nul, com0 to com9 or lpt0 to lpt9 (got {name})"
    )
}

/// The identity-holder line for `holder`.
fn identity_holder_line(holder: &str) -> String {
    format!(
        "{holder} is a shared folder's identity; --repo takes the repository the folder stands in"
    )
}

/// A `Repository` record as `EP-get-repo`, the share and the lookup
/// serve it.
fn record(owner: &str, name: &str, shared_folder: bool) -> Value {
    json!({
        "owner": owner, "name": name, "description": null,
        "commitSha": null, "status": "active", "author": null, "tags": [],
        "visibility": "private", "forkedFrom": null, "forkCount": 0,
        "fileCount": 0, "role": "owner", "sharedFolder": shared_folder,
        "createdAt": "2026-10-02T00:00:00Z", "updatedAt": "2026-10-02T00:00:00Z",
    })
}

/// `record` at `visibility`.
fn record_at(owner: &str, name: &str, shared_folder: bool, visibility: &str) -> Value {
    let mut body = record(owner, name, shared_folder);
    body["visibility"] = json!(visibility);
    body
}

fn refusal(status: u16, error: &str) -> ResponseTemplate {
    ResponseTemplate::new(status).set_body_json(json!({"error": error}))
}

const COLLABORATOR: &str = r##"{"user":{"id":"u1","name":"Carol","username":"carol","email":"carol@example.test","emailVerified":true,"image":null,"createdAt":"2026-10-02T00:00:00Z","updatedAt":"2026-10-02T00:00:00Z"},"role":"write","addedBy":"alice","createdAt":"2026-10-02T00:00:00Z"}"##;

struct Deployment {
    rt: tokio::runtime::Runtime,
    server: MockServer,
    home: tempfile::TempDir,
    cache: tempfile::TempDir,
    _work: tempfile::TempDir,
    /// The working directory, canonical.
    w: PathBuf,
}

impl Deployment {
    /// A deployment whose working directory is a checkout of `holder`,
    /// none where `holder` is none; `EP-get-repo` answers `holder` with
    /// `role` `owner` and `sharedFolder` `false`.
    fn new(holder: Option<&str>) -> Deployment {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime");
        let server = rt.block_on(MockServer::start());
        let work = tempfile::Builder::new()
            .prefix("u300-share-")
            .tempdir()
            .expect("working dir");
        let w = std::fs::canonicalize(work.path()).expect("canonical W");
        let home = tempfile::tempdir().expect("config dir");
        syns_cli::auth::token::TokenStore::new(home.path().join("credentials.json"))
            .write_with_username("test-token", Some("alice"))
            .expect("credential");
        let d = Deployment {
            rt,
            server,
            home,
            cache: tempfile::tempdir().expect("cache dir"),
            _work: work,
            w,
        };
        if let Some(holder) = holder {
            let (owner, name) = holder.split_once('/').expect("OWNER/NAME");
            write(
                &d.w.join(".syns.yaml"),
                &format!("owner: {owner}\nname: {name}\n"),
            );
            d.mount(
                Mock::given(method("GET"))
                    .and(path(format!("/api/v1/repos/{holder}")))
                    .respond_with(
                        ResponseTemplate::new(200).set_body_json(record(owner, name, false)),
                    )
                    .with_priority(10),
            );
        }
        d
    }

    fn mount(&self, mock: Mock) {
        self.rt.block_on(mock.mount(&self.server));
    }

    /// `verb` at `address` answering `answer`.
    fn serves(&self, verb: &str, address: &str, answer: ResponseTemplate) {
        self.mount(
            Mock::given(method(verb))
                .and(path(address.to_string()))
                .respond_with(answer),
        );
    }

    /// The share lookup at `address` answering `before` once, then
    /// `after` — the answer up to a removal ahead of the later one, both
    /// ahead of every answer mounted earlier.
    fn until_the_removal(&self, address: &str, before: ResponseTemplate, after: ResponseTemplate) {
        self.mount(
            Mock::given(method("GET"))
                .and(path(address.to_string()))
                .respond_with(before)
                .up_to_n_times(1)
                .with_priority(1),
        );
        self.mount(
            Mock::given(method("GET"))
                .and(path(address.to_string()))
                .respond_with(after)
                .with_priority(2),
        );
    }

    fn requests(&self) -> Vec<Request> {
        self.rt
            .block_on(self.server.received_requests())
            .expect("requests")
    }

    /// Every request of `verb`, by its path.
    fn sent(&self, verb: &str) -> Vec<String> {
        self.requests()
            .into_iter()
            .filter(|r| r.method.as_str() == verb)
            .map(|r| r.url.path().to_string())
            .collect()
    }

    /// The bodies of every `POST` sent to `address`.
    fn bodies(&self, address: &str) -> Vec<Value> {
        self.requests()
            .into_iter()
            .filter(|r| r.method.as_str() == "POST" && r.url.path() == address)
            .map(|r| serde_json::from_slice(&r.body).expect("a JSON body"))
            .collect()
    }

    /// The bodies of every `PUT` sent to `address`.
    fn put_bodies(&self, address: &str) -> Vec<Value> {
        self.requests()
            .into_iter()
            .filter(|r| r.method.as_str() == "PUT" && r.url.path() == address)
            .map(|r| serde_json::from_slice(&r.body).expect("a JSON body"))
            .collect()
    }

    /// The share lookups sent, by path.
    fn lookups(&self) -> Vec<String> {
        self.sent("GET")
            .into_iter()
            .filter(|p| p.contains("/shares"))
            .collect()
    }

    fn command(&self, cwd: &Path, args: &[&str]) -> AssertCommand {
        AssertCommand::from_std(self.process(cwd, args))
    }

    /// The built binary with `args` in `cwd`, its standard streams left to
    /// the caller.
    fn process(&self, cwd: &Path, args: &[&str]) -> std::process::Command {
        let mut command = std::process::Command::new(assert_cmd::cargo::cargo_bin("syns"));
        command
            .current_dir(cwd)
            .env("SYNS_CONFIG_DIR", self.home.path())
            .env("SYNS_CACHE_DIR", self.cache.path())
            .env("HOME", self.home.path())
            .env("XDG_CONFIG_HOME", self.home.path())
            .env_remove("SYNS_URL")
            .env_remove("SYNS_INTEGRATION")
            .env_remove("SYNS_RUN")
            .env_remove("SYNS_TRIGGER")
            .env_remove("SYNS_TASK")
            .env_remove("CI");
        command.arg("--server").arg(self.server.uri()).args(args);
        command
    }

    /// `syns` with `args` in `cwd`, standard input closed.
    fn run_in(&self, cwd: &Path, args: &[&str]) -> std::process::Output {
        self.command(cwd, args)
            .write_stdin(Vec::new())
            .output()
            .expect("run syns")
    }

    fn run(&self, args: &[&str]) -> std::process::Output {
        self.run_in(&self.w.clone(), args)
    }

    /// `syns` with `args` in the working directory, standard input a
    /// pseudo-terminal holding `typed`, the run bounded at a minute.
    #[cfg(unix)]
    fn run_on_a_terminal(&self, args: &[&str], typed: &str) -> std::process::Output {
        use std::io::Write;
        use std::os::fd::{FromRawFd, OwnedFd};

        let (mut master, mut slave): (libc::c_int, libc::c_int) = (0, 0);
        let opened = unsafe {
            libc::openpty(
                &mut master,
                &mut slave,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
            )
        };
        assert_eq!(opened, 0, "openpty");
        let mut master = std::fs::File::from(unsafe { OwnedFd::from_raw_fd(master) });
        let slave = unsafe { OwnedFd::from_raw_fd(slave) };
        master.write_all(typed.as_bytes()).expect("typed input");

        let child = self
            .process(&self.w.clone(), args)
            .stdin(std::process::Stdio::from(slave))
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .expect("run syns");
        let pid = child.id() as libc::pid_t;
        let (done, outcome) = std::sync::mpsc::channel();
        std::thread::spawn(move || done.send(child.wait_with_output()));
        let out = match outcome.recv_timeout(std::time::Duration::from_secs(60)) {
            Ok(out) => out.expect("syns output"),
            Err(_) => {
                unsafe { libc::kill(pid, libc::SIGKILL) };
                panic!("syns {args:?} still waiting on the terminal after a minute");
            }
        };
        drop(master);
        out
    }
}

fn write(file: &Path, content: &str) {
    std::fs::create_dir_all(file.parent().expect("a parent")).expect("parent dir");
    std::fs::write(file, content).expect("file written");
}

fn stdout_of(output: &std::process::Output) -> String {
    String::from_utf8_lossy(&output.stdout).to_string()
}

fn stderr_of(output: &std::process::Output) -> String {
    String::from_utf8_lossy(&output.stderr).to_string()
}

fn exit_of(output: &std::process::Output) -> i32 {
    output.status.code().expect("an exit code")
}

fn document(output: &std::process::Output) -> Value {
    serde_json::from_str(stdout_of(output).trim()).expect("one document on the primary stream")
}

// ---- share --------------------------------------------------------------

// SPEC u300 Tests, the row of this name.
#[test]
#[serial]
fn a_first_share_offers_the_holder_prefixed_name() {
    let d = Deployment::new(Some("alice/docs"));
    d.serves(
        "GET",
        "/api/v1/repos/alice/docs/shares/q3-plan",
        refusal(404, "not_found"),
    );
    d.serves(
        "POST",
        "/api/v1/repos/alice/docs/shares",
        ResponseTemplate::new(201).set_body_json(record("alice", "docs-q3-plan", true)),
    );

    let out = d.run(&["share", "q3-plan/", "--json"]);

    assert_eq!(exit_of(&out), 0, "{}", stderr_of(&out));
    assert_eq!(
        d.bodies("/api/v1/repos/alice/docs/shares"),
        vec![json!({"path": "q3-plan", "name": "docs-q3-plan"})]
    );
    let doc = document(&out);
    assert_eq!(doc["owner"], "alice");
    assert_eq!(doc["name"], "docs-q3-plan");
    assert_eq!(doc["holder"], "alice/docs");
    assert_eq!(doc["path"], "q3-plan");
    assert_eq!(doc["created"], true);
}

// SPEC u300 Tests, the row of this name.
#[test]
#[serial]
fn a_share_inside_a_scoped_folder_counts_the_path_from_it() {
    let d = Deployment::new(Some("alice/work"));
    let vela = d.w.join("clients/vela");
    write(
        &vela.join(".syns.yaml"),
        "holder: alice/work\npath: clients/vela\n",
    );
    d.mount(
        Mock::given(method("GET"))
            .and(path_regex("^/api/v1/repos/alice/work/shares/"))
            .respond_with(refusal(404, "not_found")),
    );
    d.serves(
        "POST",
        "/api/v1/repos/alice/work/shares",
        ResponseTemplate::new(201).set_body_json(record("alice", "work-shared", true)),
    );

    let board = d.run_in(&vela, &["share", "q3-board", "--json"]);
    assert_eq!(exit_of(&board), 0, "{}", stderr_of(&board));
    let itself = d.run_in(&vela, &["share", ".", "--json"]);
    assert_eq!(exit_of(&itself), 0, "{}", stderr_of(&itself));

    assert_eq!(
        d.lookups(),
        vec![
            "/api/v1/repos/alice/work/shares/clients/vela/q3-board".to_string(),
            "/api/v1/repos/alice/work/shares/clients/vela".to_string(),
        ]
    );
    assert_eq!(
        d.bodies("/api/v1/repos/alice/work/shares"),
        vec![
            json!({"path": "clients/vela/q3-board", "name": "work-q3-board"}),
            json!({"path": "clients/vela", "name": "work-vela"}),
        ]
    );
}

// SPEC u300 Tests, the row of this name.
#[test]
#[serial]
fn a_folder_standing_shared_answers_its_identity() {
    let d = Deployment::new(Some("alice/docs"));
    d.serves(
        "GET",
        "/api/v1/repos/alice/docs/shares/q3-plan",
        ResponseTemplate::new(200).set_body_json(record("alice", "docs-q3-plan", true)),
    );

    let out = d.run(&["share", "q3-plan", "--name", "other", "--json"]);

    assert_eq!(exit_of(&out), 0, "{}", stderr_of(&out));
    assert!(d.sent("POST").is_empty());
    let doc = document(&out);
    assert_eq!(doc["name"], "docs-q3-plan");
    assert_eq!(doc["created"], false);
}

// SPEC u300 Tests, the row of this name.
#[test]
#[serial]
fn a_held_name_is_refused_after_the_lookup_reread() {
    let d = Deployment::new(Some("alice/docs"));
    d.serves(
        "GET",
        "/api/v1/repos/alice/docs/shares/q3-plan",
        refusal(404, "not_found"),
    );
    d.serves(
        "POST",
        "/api/v1/repos/alice/docs/shares",
        refusal(409, "conflict"),
    );

    let out = d.run(&["share", "q3-plan", "--json"]);

    assert_eq!(exit_of(&out), 1, "{}", stderr_of(&out));
    let doc = document(&out);
    assert!(
        doc["error"].as_str().expect("error").contains("conflict"),
        "{doc}"
    );
    assert_eq!(d.lookups().len(), 2);
    assert_eq!(d.sent("POST").len(), 1);
}

// SPEC u300 Tests, the row of this name.
#[test]
#[serial]
fn a_conflict_meeting_a_standing_identity_ends_shared() {
    let d = Deployment::new(Some("alice/docs"));
    d.mount(
        Mock::given(method("GET"))
            .and(path("/api/v1/repos/alice/docs/shares/q3-plan"))
            .respond_with(refusal(404, "not_found"))
            .up_to_n_times(1)
            .with_priority(1),
    );
    d.serves(
        "GET",
        "/api/v1/repos/alice/docs/shares/q3-plan",
        ResponseTemplate::new(200).set_body_json(record("alice", "docs-q3-plan", true)),
    );
    d.serves(
        "POST",
        "/api/v1/repos/alice/docs/shares",
        refusal(409, "conflict"),
    );

    let out = d.run(&["share", "q3-plan", "--json"]);

    assert_eq!(exit_of(&out), 0, "{}", stderr_of(&out));
    let doc = document(&out);
    assert_eq!(doc["created"], false);
    assert_eq!(doc["name"], "docs-q3-plan");
}

// SPEC u300 Tests, the row of this name.
#[test]
#[serial]
fn a_share_name_outside_the_kind_is_refused_before_any_request() {
    let d = Deployment::new(Some("alice/docs"));

    let out = d.run(&["share", "q3-plan", "--name", "Docs Q3"]);

    assert_eq!(exit_of(&out), 1, "{}", stderr_of(&out));
    assert_eq!(
        stderr_of(&out).trim_end(),
        format!("error: configuration error: {}", name_kind_line("Docs Q3"))
    );
    assert!(d.requests().is_empty());
}

// SPEC u300 Tests, the row of this name.
#[test]
#[serial]
fn share_show_answers_the_identity_the_offer_or_a_refused_holder() {
    let d = Deployment::new(Some("alice/docs"));
    d.serves(
        "GET",
        "/api/v1/repos/alice/docs/shares/q3-plan",
        ResponseTemplate::new(200).set_body_json(record("alice", "docs-q3-plan", true)),
    );
    d.serves(
        "GET",
        "/api/v1/repos/alice/docs/shares/budget",
        refusal(404, "not_found"),
    );
    d.serves(
        "GET",
        "/api/v1/repos/alice/docs/shares/q3%20plan/a%25b",
        refusal(404, "not_found"),
    );
    d.serves("GET", "/api/v1/repos/bob/gone", refusal(404, "not_found"));
    d.serves(
        "GET",
        "/api/v1/repos/alice/docs-q3-plan",
        ResponseTemplate::new(200).set_body_json(record("alice", "docs-q3-plan", true)),
    );

    let shared = d.run(&["share", "q3-plan", "--show", "--json"]);
    assert_eq!(exit_of(&shared), 0, "{}", stderr_of(&shared));
    let doc = document(&shared);
    assert_eq!(doc["shared"], true);
    assert_eq!(doc["name"], "docs-q3-plan");
    assert_eq!(doc["holderRole"], "owner");

    let budget = d.run(&["share", "budget", "--show", "--json"]);
    assert_eq!(exit_of(&budget), 0, "{}", stderr_of(&budget));
    let doc = document(&budget);
    assert_eq!(doc["shared"], false);
    assert_eq!(doc["offeredName"], "docs-budget");

    let spaced = d.run(&["share", "q3 plan/a%b", "--show", "--json"]);
    assert_eq!(exit_of(&spaced), 0, "{}", stderr_of(&spaced));
    assert_eq!(
        d.lookups().last().map(String::as_str),
        Some("/api/v1/repos/alice/docs/shares/q3%20plan/a%25b")
    );
    assert_eq!(document(&spaced)["offeredName"], Value::Null);

    let looked_up = d.lookups().len();
    let gone = d.run(&["share", "x", "--show", "--repo", "bob/gone", "--json"]);
    assert_eq!(exit_of(&gone), 1, "{}", stderr_of(&gone));
    assert!(
        document(&gone)["error"]
            .as_str()
            .expect("error")
            .contains("not_found"),
        "{}",
        stdout_of(&gone)
    );
    assert_eq!(d.lookups().len(), looked_up, "no lookup sent");

    for args in [
        vec![
            "share",
            "x",
            "--show",
            "--repo",
            "alice/docs-q3-plan",
            "--json",
        ],
        vec!["share", "x", "--repo", "alice/docs-q3-plan", "--json"],
    ] {
        let out = d.run(&args);
        assert_eq!(exit_of(&out), 1, "{args:?}: {}", stderr_of(&out));
        assert_eq!(
            document(&out)["error"],
            format!(
                "configuration error: {}",
                identity_holder_line("alice/docs-q3-plan")
            ),
            "{args:?}"
        );
        assert_eq!(d.lookups().len(), looked_up, "{args:?}: no lookup sent");
    }
    assert!(d.sent("POST").is_empty(), "no share request");
}

// ---- unshare ------------------------------------------------------------

// SPEC u300 Tests, the row of this name.
#[test]
#[serial]
fn unshare_retires_the_identity_once_confirmed() {
    let d = Deployment::new(Some("alice/docs"));
    d.until_the_removal(
        "/api/v1/repos/alice/docs/shares/q3-plan",
        ResponseTemplate::new(200).set_body_json(record("alice", "docs-q3-plan", true)),
        refusal(404, "not_found"),
    );
    d.serves(
        "DELETE",
        "/api/v1/repos/alice/docs/shares/q3-plan",
        ResponseTemplate::new(204),
    );

    let out = d.run(&["unshare", "q3-plan", "--yes", "--json"]);

    assert_eq!(exit_of(&out), 0, "{}", stderr_of(&out));
    assert_eq!(
        d.sent("DELETE"),
        vec!["/api/v1/repos/alice/docs/shares/q3-plan".to_string()]
    );
    assert_eq!(
        document(&out),
        json!({
            "unshared": true,
            "holder": "alice/docs",
            "path": "q3-plan",
            "owner": "alice",
            "name": "docs-q3-plan",
            "retired": true,
        })
    );
}

// SPEC u300 Tests, the row of this name.
#[test]
#[serial]
fn unshare_of_a_folder_not_shared_raises_no_confirmation() {
    let d = Deployment::new(Some("alice/docs"));
    d.serves(
        "GET",
        "/api/v1/repos/alice/docs/shares/q3-plan",
        refusal(404, "not_found"),
    );

    let out = d.run(&["unshare", "q3-plan"]);

    assert_eq!(exit_of(&out), 1, "{}", stderr_of(&out));
    let stderr = stderr_of(&out);
    assert!(stderr.starts_with("error: "), "{stderr}");
    assert!(stderr.contains("not_found"), "{stderr}");
    assert_eq!(stderr.lines().count(), 1, "{stderr}");
    assert!(d.sent("DELETE").is_empty());
}

// SPEC u300 Tests, the row of this name.
#[test]
#[serial]
fn unshare_under_ci_without_yes_removes_nothing() {
    let d = Deployment::new(Some("alice/docs"));
    d.serves(
        "GET",
        "/api/v1/repos/alice/docs/shares/q3-plan",
        ResponseTemplate::new(200).set_body_json(record("alice", "docs-q3-plan", true)),
    );

    let out = d
        .command(&d.w.clone(), &["unshare", "q3-plan"])
        .env("CI", "true")
        .write_stdin(Vec::new())
        .output()
        .expect("run syns");

    assert_eq!(exit_of(&out), 1, "{}", stderr_of(&out));
    assert!(
        stderr_of(&out).starts_with("error: configuration error: "),
        "{}",
        stderr_of(&out)
    );
    assert!(d.sent("DELETE").is_empty());
}

// CR1-1: an answer typed at the confirmation.
#[test]
#[serial]
fn unshare_refused_at_the_confirmation_removes_nothing() {
    let d = Deployment::new(Some("alice/docs"));
    d.serves(
        "GET",
        "/api/v1/repos/alice/docs/shares/q3-plan",
        ResponseTemplate::new(200).set_body_json(record("alice", "docs-q3-plan", true)),
    );
    d.serves(
        "DELETE",
        "/api/v1/repos/alice/docs/shares/q3-plan",
        ResponseTemplate::new(204),
    );
    let answering = |answer: &[u8]| {
        d.command(&d.w.clone(), &["unshare", "q3-plan", "--json"])
            .write_stdin(answer.to_vec())
            .output()
            .expect("run syns")
    };

    let refused = answering(b"n\n");

    assert_eq!(exit_of(&refused), 0, "{}", stderr_of(&refused));
    assert_eq!(
        stderr_of(&refused),
        "Stop sharing q3-plan of alice/docs as alice/docs-q3-plan, removing its collaborators? [y/N]: Aborted.\n"
    );
    assert_eq!(
        document(&refused),
        json!({
            "unshared": false,
            "holder": "alice/docs",
            "path": "q3-plan",
            "owner": "alice",
            "name": "docs-q3-plan",
        })
    );
    assert!(d.sent("DELETE").is_empty());

    let confirmed = answering(b"y\n");

    assert_eq!(exit_of(&confirmed), 0, "{}", stderr_of(&confirmed));
    assert_eq!(document(&confirmed)["unshared"], true);
    assert_eq!(
        d.sent("DELETE"),
        vec!["/api/v1/repos/alice/docs/shares/q3-plan".to_string()]
    );
}

// CR1-3: the share lines and the unshare lines outside `--json`.
#[test]
#[serial]
fn share_lines_report_on_the_diagnostic_stream_and_name_the_identity() {
    let d = Deployment::new(Some("alice/docs"));
    for (folder, answer) in [
        ("q3-plan", refusal(404, "not_found")),
        ("budget", refusal(404, "not_found")),
        (
            "old",
            ResponseTemplate::new(200).set_body_json(record("alice", "docs-old", true)),
        ),
    ] {
        d.serves(
            "GET",
            &format!("/api/v1/repos/alice/docs/shares/{folder}"),
            answer,
        );
    }
    d.serves(
        "POST",
        "/api/v1/repos/alice/docs/shares",
        ResponseTemplate::new(201).set_body_json(record("alice", "docs-q3-plan", true)),
    );
    d.serves(
        "DELETE",
        "/api/v1/repos/alice/docs/shares/old",
        ResponseTemplate::new(204),
    );
    let expect = |args: &[&str], stdout: &str, stderr: &str| {
        let out = d.run(args);
        assert_eq!(exit_of(&out), 0, "{args:?}: {}", stderr_of(&out));
        assert_eq!(stdout_of(&out), stdout, "{args:?}");
        assert_eq!(stderr_of(&out), stderr, "{args:?}");
    };

    expect(
        &["share", "q3-plan"],
        "alice/docs-q3-plan\n",
        "shared q3-plan of alice/docs as alice/docs-q3-plan; add people with: syns collaborators add USER --role read --repo alice/docs-q3-plan\n",
    );
    expect(
        &["share", "old"],
        "alice/docs-old\n",
        "old of alice/docs is already shared as alice/docs-old\n",
    );
    expect(
        &["share", "old", "--show"],
        "alice/docs-old\n",
        "old of alice/docs is shared as alice/docs-old (private)\n",
    );
    expect(
        &["share", "budget", "--show"],
        "",
        "budget of alice/docs is not shared; syns share offers the name docs-budget\n",
    );
    d.until_the_removal(
        "/api/v1/repos/alice/docs/shares/old",
        ResponseTemplate::new(200).set_body_json(record("alice", "docs-old", true)),
        refusal(404, "not_found"),
    );
    expect(
        &["unshare", "old", "--yes"],
        "",
        "stopped sharing old of alice/docs: alice/docs-old is retired and its collaborators removed\n",
    );

    let scoped = Deployment::new(Some("alice/work"));
    let vela = scoped.w.join("clients/vela");
    write(
        &vela.join(".syns.yaml"),
        "holder: alice/work\npath: clients/vela\n",
    );
    scoped.serves(
        "GET",
        "/api/v1/repos/alice/work/shares/clients/vela/q3-board",
        refusal(404, "not_found"),
    );
    scoped.serves(
        "POST",
        "/api/v1/repos/alice/work/shares",
        ResponseTemplate::new(201).set_body_json(record("alice", "work-q3-board", true)),
    );
    let out = scoped.run_in(&vela, &["share", "q3-board"]);
    assert_eq!(exit_of(&out), 0, "{}", stderr_of(&out));
    assert_eq!(stdout_of(&out), "alice/work-q3-board\n");
    assert_eq!(
        stderr_of(&out),
        "shared clients/vela/q3-board of alice/work as alice/work-q3-board\n"
    );
}

// CR1-2: the name prompt through the built binary on a terminal — not
// raised for a caller holding `write` on the holder, raised for `admin`,
// and for `owner` raised again with no offer after a held name.
#[cfg(unix)]
#[test]
#[serial]
fn the_name_prompt_on_a_terminal_asks_an_owner_or_an_admin_alone() {
    let on_the_holder = |role: &str| {
        let d = Deployment::new(Some("alice/docs"));
        let mut holder = record("alice", "docs", false);
        holder["role"] = json!(role);
        d.mount(
            Mock::given(method("GET"))
                .and(path("/api/v1/repos/alice/docs"))
                .respond_with(ResponseTemplate::new(200).set_body_json(holder))
                .with_priority(1),
        );
        d.serves(
            "GET",
            "/api/v1/repos/alice/docs/shares/q3-plan",
            refusal(404, "not_found"),
        );
        d
    };
    let prompt = "name for q3-plan of alice/docs [docs-q3-plan]: ";

    let write = on_the_holder("write");
    write.serves(
        "POST",
        "/api/v1/repos/alice/docs/shares",
        ResponseTemplate::new(201).set_body_json(record("alice", "docs-q3-plan", true)),
    );
    let out = write.run_on_a_terminal(&["share", "q3-plan"], "mine\n");
    assert_eq!(exit_of(&out), 0, "{}", stderr_of(&out));
    assert!(!stderr_of(&out).contains("name for"), "{}", stderr_of(&out));
    assert_eq!(
        write.bodies("/api/v1/repos/alice/docs/shares"),
        vec![json!({"path": "q3-plan", "name": "docs-q3-plan"})]
    );

    let admin = on_the_holder("admin");
    admin.serves(
        "POST",
        "/api/v1/repos/alice/docs/shares",
        ResponseTemplate::new(201).set_body_json(record("alice", "mine", true)),
    );
    let out = admin.run_on_a_terminal(&["share", "q3-plan"], "mine\n");
    assert_eq!(exit_of(&out), 0, "{}", stderr_of(&out));
    assert!(stderr_of(&out).starts_with(prompt), "{}", stderr_of(&out));
    assert_eq!(stdout_of(&out), "alice/mine\n");
    assert_eq!(
        admin.bodies("/api/v1/repos/alice/docs/shares"),
        vec![json!({"path": "q3-plan", "name": "mine"})]
    );

    let owner = on_the_holder("owner");
    owner.mount(
        Mock::given(method("POST"))
            .and(path("/api/v1/repos/alice/docs/shares"))
            .respond_with(refusal(409, "conflict"))
            .up_to_n_times(1)
            .with_priority(1),
    );
    owner.serves(
        "POST",
        "/api/v1/repos/alice/docs/shares",
        ResponseTemplate::new(201).set_body_json(record("alice", "second", true)),
    );
    let out = owner.run_on_a_terminal(&["share", "q3-plan"], "mine\nsecond\n");
    assert_eq!(exit_of(&out), 0, "{}", stderr_of(&out));
    assert!(
        stderr_of(&out).starts_with(&format!(
            "{prompt}alice/mine is already held; give another name\nname for q3-plan of alice/docs: "
        )),
        "{}",
        stderr_of(&out)
    );
    assert_eq!(stdout_of(&out), "alice/second\n");
    assert_eq!(
        owner.bodies("/api/v1/repos/alice/docs/shares"),
        vec![
            json!({"path": "q3-plan", "name": "mine"}),
            json!({"path": "q3-plan", "name": "second"}),
        ]
    );
}

// ---- collaborators through an identity ---------------------------------

/// The deployment `alice/docs` checked out at `W`, its scoped folder
/// `q3-plan` recording `shared_as: docs-q3-plan`.
fn docs_with_a_shared_folder() -> Deployment {
    let d = Deployment::new(Some("alice/docs"));
    write(
        &d.w.join("q3-plan/.syns.yaml"),
        "holder: alice/docs\npath: q3-plan\nshared_as: docs-q3-plan\n",
    );
    d
}

// SPEC u300 Tests, the row of this name.
#[test]
#[serial]
fn collaborators_reach_an_identity_by_the_repository_option() {
    let d = docs_with_a_shared_folder();
    d.serves(
        "POST",
        "/api/v1/repos/alice/docs-q3-plan/collaborators",
        ResponseTemplate::new(201),
    );
    let root = d.w.clone();
    let folder = d.w.join("q3-plan");

    for (cwd, args) in [
        (
            &root,
            vec![
                "collaborators",
                "--repo",
                "alice/docs-q3-plan",
                "add",
                "carol",
                "--role",
                "read",
                "--json",
            ],
        ),
        (
            &root,
            vec![
                "collaborators",
                "add",
                "carol",
                "--role",
                "read",
                "--repo",
                "alice/docs-q3-plan",
                "--json",
            ],
        ),
        (
            &folder,
            vec![
                "collaborators",
                "add",
                "carol",
                "--role",
                "read",
                "--repo",
                "Alice/Docs-Q3-Plan",
                "--json",
            ],
        ),
    ] {
        let before = d.requests().len();
        let out = d.run_in(cwd, &args);
        assert_eq!(exit_of(&out), 0, "{args:?}: {}", stderr_of(&out));
        let sent: Vec<Request> = d.requests().into_iter().skip(before).collect();
        assert_eq!(sent.len(), 1, "{args:?}");
        assert_eq!(
            sent[0].url.path(),
            "/api/v1/repos/alice/docs-q3-plan/collaborators"
        );
        assert_eq!(
            serde_json::from_slice::<Value>(&sent[0].body).expect("body"),
            json!({"username": "carol", "role": "read"}),
            "{args:?}"
        );
    }
}

// SPEC u300 Tests, the row of this name.
#[test]
#[serial]
fn collaborators_inside_a_folder_refuse_every_other_repository() {
    let d = docs_with_a_shared_folder();
    write(
        &d.w.join("budget/.syns.yaml"),
        "holder: alice/docs\npath: budget\n",
    );
    write(
        &d.w.join("plan/.syns.yaml"),
        "holder: alice/docs\npath: plan\nshared_as:\n  - docs-plan\n",
    );
    let add = ["collaborators", "add", "carol", "--role", "read"];

    for (folder, repo) in [
        ("q3-plan", None),
        ("q3-plan", Some("alice/docs")),
        ("q3-plan", Some("alice/docs-budget")),
        ("budget", Some("alice/docs-budget")),
        ("plan", Some("alice/docs-plan")),
    ] {
        let mut args: Vec<&str> = add.to_vec();
        if let Some(repo) = repo {
            args.extend(["--repo", repo]);
        }
        let out = d.run_in(&d.w.join(folder), &args);
        assert_eq!(exit_of(&out), 2, "{folder} {repo:?}: {}", stderr_of(&out));
        assert!(
            stderr_of(&out).contains("holder root required"),
            "{folder} {repo:?}: {}",
            stderr_of(&out)
        );
    }
    assert!(d.requests().is_empty());
}

// SPEC u300 Tests, the row of this name.
#[test]
#[serial]
fn the_listing_role_and_remove_take_the_repository_option() {
    let d = Deployment::new(None);
    d.serves(
        "GET",
        "/api/v1/repos/alice/docs-q3-plan/collaborators",
        ResponseTemplate::new(200).set_body_string(format!(
            r#"{{"data":[{COLLABORATOR}],"total":1,"limit":100,"offset":0}}"#
        )),
    );
    d.serves(
        "PATCH",
        "/api/v1/repos/alice/docs-q3-plan/collaborators/u1",
        ResponseTemplate::new(200).set_body_string(COLLABORATOR),
    );
    d.serves(
        "DELETE",
        "/api/v1/repos/alice/docs-q3-plan/collaborators/u1",
        ResponseTemplate::new(204),
    );

    for args in [
        vec!["collaborators", "--repo", "alice/docs-q3-plan"],
        vec![
            "collaborators",
            "role",
            "u1",
            "--role",
            "write",
            "--repo",
            "alice/docs-q3-plan",
        ],
        vec![
            "collaborators",
            "remove",
            "u1",
            "--yes",
            "--repo",
            "alice/docs-q3-plan",
        ],
    ] {
        let out = d.run(&args);
        assert_eq!(exit_of(&out), 0, "{args:?}: {}", stderr_of(&out));
    }
    let sent: Vec<(String, String)> = d
        .requests()
        .into_iter()
        .map(|r| (r.method.to_string(), r.url.path().to_string()))
        .collect();
    assert_eq!(sent.len(), 3, "{sent:?}");
    for (verb, address) in &sent {
        assert!(
            address.starts_with("/api/v1/repos/alice/docs-q3-plan/collaborators"),
            "{verb} {address}"
        );
    }
}

// SPEC u300 Tests, the row of this name.
#[test]
#[serial]
fn admin_on_an_identity_is_the_servers_refusal() {
    let d = Deployment::new(Some("alice/docs"));
    d.serves(
        "POST",
        "/api/v1/repos/alice/docs-q3-plan/collaborators",
        refusal(422, "validation_error"),
    );

    let out = d.run(&[
        "collaborators",
        "add",
        "dave",
        "--role",
        "admin",
        "--repo",
        "alice/docs-q3-plan",
        "--json",
    ]);

    assert_eq!(exit_of(&out), 1, "{}", stderr_of(&out));
    assert!(
        document(&out)["error"]
            .as_str()
            .expect("error")
            .contains("validation_error"),
        "{}",
        stdout_of(&out)
    );
}

// ---- a folder's own visibility (SPEC u329) -------------------------------

const MARKING: &str = "/api/v1/repos/alice/handbook/folder-visibility";
const DRAFTS_LOOKUP: &str = "/api/v1/repos/alice/handbook/shares/drafts";

/// `M` of SPEC u329 Tests: the credential; `alice/handbook` public with
/// `role` `owner` and `sharedFolder` false, its tree at `drafts` holding
/// `drafts/plan.md`, and the marking answering `alice/handbook-drafts`
/// at `private` with `sharedFolder` true.
fn marking_deployment() -> Deployment {
    let d = Deployment::new(Some("alice/handbook"));
    d.serves(
        "GET",
        "/api/v1/repos/alice/handbook",
        ResponseTemplate::new(200).set_body_json(record_at("alice", "handbook", false, "public")),
    );
    d.serves(
        "GET",
        "/api/v1/repos/alice/handbook/tree/drafts",
        ResponseTemplate::new(200).set_body_json(json!({
            "entries": [{
                "name": "plan.md", "path": "drafts/plan.md", "type": "file",
                "size": 6, "sha": null,
            }],
            "commitSha": "a".repeat(40),
            "truncated": false,
        })),
    );
    d.serves(
        "PUT",
        MARKING,
        ResponseTemplate::new(200).set_body_json(record_at(
            "alice",
            "handbook-drafts",
            true,
            "private",
        )),
    );
    d
}

/// The marking at `MARKING` answering `answer` the next `times` runs
/// reach it, ahead of `M`'s answer.
fn marking_first(d: &Deployment, answer: ResponseTemplate, times: u64, priority: u8) {
    d.mount(
        Mock::given(method("PUT"))
            .and(path(MARKING))
            .respond_with(answer)
            .up_to_n_times(times)
            .with_priority(priority),
    );
}

const UNSUPPORTED_DRAFTS: &str = "folder_visibility_unsupported: the server does not support a folder's own visibility yet, so drafts of alice/handbook keeps the visibility it had";

// SPEC u329 Tests, the row of this name.
#[test]
#[serial]
fn a_marking_sends_the_path_and_the_visibility_alone() {
    let d = marking_deployment();

    let out = d.run(&[
        "share",
        "drafts",
        "--visibility",
        "private",
        "--repo",
        "alice/handbook",
    ]);

    assert_eq!(exit_of(&out), 0, "{}", stderr_of(&out));
    assert_eq!(d.sent("PUT"), vec![MARKING.to_string()]);
    assert_eq!(
        d.put_bodies(MARKING),
        vec![json!({"path": "drafts", "visibility": "private"})]
    );
    assert_eq!(
        stderr_of(&out),
        "drafts of alice/handbook is private as alice/handbook-drafts\n"
    );
}

// SPEC u329 Tests, the row of this name.
#[test]
#[serial]
fn a_marking_under_json_answers_the_served_identity() {
    let d = marking_deployment();

    let out = d.run(&[
        "share",
        "drafts",
        "--visibility",
        "private",
        "--repo",
        "alice/handbook",
        "--json",
        "--name",
        "handbook-drafts",
    ]);

    assert_eq!(exit_of(&out), 0, "{}", stderr_of(&out));
    assert_eq!(
        d.put_bodies(MARKING),
        vec![json!({"path": "drafts", "visibility": "private", "name": "handbook-drafts"})]
    );
    let mut expected = record_at("alice", "handbook-drafts", true, "private");
    expected["holder"] = json!("alice/handbook");
    expected["path"] = json!("drafts");
    assert_eq!(document(&out), expected);
}

// SPEC u329 Tests, the row of this name.
#[test]
#[serial]
fn a_name_outside_the_kind_and_an_option_clash_send_nothing() {
    let d = marking_deployment();

    let bad = d.run(&[
        "share",
        "drafts",
        "--visibility",
        "public",
        "--name",
        "Bad",
        "--repo",
        "alice/handbook",
    ]);
    assert_eq!(exit_of(&bad), 1, "{}", stderr_of(&bad));
    assert_eq!(
        stderr_of(&bad).trim_end(),
        format!("error: configuration error: {}", name_kind_line("Bad"))
    );
    for args in [
        vec!["share", "drafts", "--visibility", "public", "--show"],
        vec!["share", "drafts", "--visibility", "internal"],
    ] {
        let out = d.run(&args);
        assert_eq!(exit_of(&out), 2, "{args:?}: {}", stderr_of(&out));
    }
    assert!(d.requests().is_empty(), "{:?}", d.requests());
}

// SPEC u329 Tests, the row of this name.
#[test]
#[serial]
fn an_older_servers_marking_is_refused_plainly() {
    let d = marking_deployment();
    marking_first(
        &d,
        ResponseTemplate::new(404)
            .set_body_json(json!({"error": "not_found", "message": "Not found"})),
        1,
        1,
    );

    let out = d.run(&[
        "share",
        "drafts",
        "--visibility",
        "public",
        "--repo",
        "alice/handbook",
    ]);

    assert_eq!(exit_of(&out), 1, "{}", stderr_of(&out));
    assert_eq!(
        stderr_of(&out).trim_end(),
        format!("error: {UNSUPPORTED_DRAFTS}")
    );
}

// SPEC u329 Tests, the row of this name.
#[test]
#[serial]
fn a_path_standing_nowhere_is_refused_before_any_marking() {
    let d = marking_deployment();
    d.serves(
        "GET",
        "/api/v1/repos/alice/handbook/tree/nowhere",
        refusal(404, "not_found"),
    );

    let out = d.run(&[
        "share",
        "nowhere",
        "--visibility",
        "public",
        "--repo",
        "alice/handbook",
    ]);

    assert_eq!(exit_of(&out), 1, "{}", stderr_of(&out));
    assert!(stderr_of(&out).contains("not_found"), "{}", stderr_of(&out));
    assert!(d.sent("PUT").is_empty());
}

// SPEC u329 Tests, the row of this name.
#[test]
#[serial]
fn a_held_name_on_a_first_marking_is_asked_of_no_one_who_cannot_answer() {
    let d = marking_deployment();
    let args = [
        "share",
        "drafts",
        "--visibility",
        "private",
        "--name",
        "drafts-q3",
        "--repo",
        "alice/handbook",
    ];
    marking_first(&d, refusal(409, "conflict"), 1, 1);
    d.until_the_removal(
        DRAFTS_LOOKUP,
        refusal(404, "not_found"),
        ResponseTemplate::new(200).set_body_json(record_at(
            "alice",
            "handbook-drafts",
            true,
            "private",
        )),
    );

    let first = d.run(&args);

    assert_eq!(exit_of(&first), 1, "{}", stderr_of(&first));
    let err = stderr_of(&first);
    assert!(err.contains("conflict"), "{err}");
    assert!(
        err.contains("alice/drafts-q3 is already held; give another name"),
        "{err}"
    );
    assert_eq!(d.lookups().len(), 1);
    assert_eq!(d.sent("PUT").len(), 1);

    marking_first(&d, refusal(409, "conflict"), 1, 1);
    let second = d.run(&args);

    assert_eq!(exit_of(&second), 0, "{}", stderr_of(&second));
    let bodies = d.put_bodies(MARKING);
    assert_eq!(bodies.len(), 3, "{bodies:?}");
    assert_eq!(bodies[1]["name"], "drafts-q3");
    assert_eq!(
        bodies[2],
        json!({"path": "drafts", "visibility": "private"})
    );
}

// SPEC u329 Tests, the row of this name.
#[test]
#[serial]
fn a_newer_servers_refusals_reach_as_served() {
    let d = marking_deployment();
    marking_first(&d, refusal(403, "forbidden"), 1, 1);
    marking_first(&d, refusal(409, "conflict"), 1, 2);
    d.serves("GET", DRAFTS_LOOKUP, refusal(404, "not_found"));
    let args = [
        "share",
        "drafts",
        "--visibility",
        "public",
        "--repo",
        "alice/handbook",
    ];

    let forbidden = d.run(&args);
    let conflict = d.run(&args);

    assert_eq!(exit_of(&forbidden), 1, "{}", stderr_of(&forbidden));
    assert!(stderr_of(&forbidden).contains("forbidden"));
    assert_eq!(exit_of(&conflict), 1, "{}", stderr_of(&conflict));
    assert!(stderr_of(&conflict).contains("conflict"));
    for out in [&forbidden, &conflict] {
        assert!(
            !stderr_of(out).contains("folder_visibility_unsupported"),
            "{}",
            stderr_of(out)
        );
    }
}

// SPEC u329 Tests, the row of this name.
#[test]
#[serial]
fn the_share_lookup_names_the_folders_visibility() {
    let d = Deployment::new(Some("alice/clients"));
    d.serves(
        "GET",
        "/api/v1/repos/alice/clients/shares/vela/q3-board",
        ResponseTemplate::new(200).set_body_json(record_at(
            "alice",
            "clients-q3-board",
            true,
            "public",
        )),
    );

    let out = d.run(&[
        "share",
        "vela/q3-board",
        "--show",
        "--repo",
        "alice/clients",
    ]);

    assert_eq!(exit_of(&out), 0, "{}", stderr_of(&out));
    assert_eq!(
        stderr_of(&out),
        "vela/q3-board of alice/clients is shared as alice/clients-q3-board (public)\n"
    );
    assert_eq!(stdout_of(&out), "alice/clients-q3-board\n");
}

// SPEC u329 Tests, the row of this name.
#[test]
#[serial]
fn an_unshare_keeping_a_marked_folders_name_says_so() {
    let d = Deployment::new(Some("alice/handbook"));
    d.serves(
        "GET",
        DRAFTS_LOOKUP,
        ResponseTemplate::new(200).set_body_json(record("alice", "handbook-drafts", true)),
    );
    d.serves("DELETE", DRAFTS_LOOKUP, ResponseTemplate::new(204));
    let args = ["unshare", "drafts", "--repo", "alice/handbook", "--yes"];

    let human = d.run(&args);
    let json_run = d.run(&[&args[..], &["--json"]].concat());

    assert_eq!(exit_of(&human), 0, "{}", stderr_of(&human));
    assert_eq!(
        stderr_of(&human),
        "stopped sharing drafts of alice/handbook: its collaborators removed, alice/handbook-drafts standing private by a visibility of its own\n"
    );
    assert_eq!(exit_of(&json_run), 0, "{}", stderr_of(&json_run));
    assert_eq!(
        document(&json_run),
        json!({
            "unshared": true,
            "holder": "alice/handbook",
            "path": "drafts",
            "owner": "alice",
            "name": "handbook-drafts",
            "retired": false,
        })
    );
    assert_eq!(d.sent("DELETE").len(), 2);
    assert_eq!(
        d.lookups().len(),
        4,
        "a lookup before and after each removal"
    );
}

// SPEC u329 Tests, the row of this name.
#[test]
#[serial]
fn an_unshare_retiring_a_folder_says_so_as_before() {
    let d = Deployment::new(Some("alice/handbook"));
    let guides = "/api/v1/repos/alice/handbook/shares/guides";
    d.until_the_removal(
        guides,
        ResponseTemplate::new(200).set_body_json(record("alice", "handbook-guides", true)),
        refusal(404, "not_found"),
    );
    d.serves("DELETE", guides, ResponseTemplate::new(204));

    let out = d.run(&[
        "unshare",
        "guides",
        "--repo",
        "alice/handbook",
        "--yes",
        "--json",
    ]);

    assert_eq!(exit_of(&out), 0, "{}", stderr_of(&out));
    assert_eq!(
        document(&out),
        json!({
            "unshared": true,
            "holder": "alice/handbook",
            "path": "guides",
            "owner": "alice",
            "name": "handbook-guides",
            "retired": true,
        })
    );
}

// SPEC u329 Tests, the row of this name.
#[test]
#[serial]
fn an_unshare_whose_second_lookup_fails_claims_nothing() {
    let d = Deployment::new(Some("alice/handbook"));
    let guides = "/api/v1/repos/alice/handbook/shares/guides";
    d.serves("DELETE", guides, ResponseTemplate::new(204));
    let args = ["unshare", "guides", "--repo", "alice/handbook", "--yes"];
    let before_and_after = || {
        d.until_the_removal(
            guides,
            ResponseTemplate::new(200).set_body_json(record("alice", "handbook-guides", true)),
            refusal(500, "internal_error"),
        )
    };

    before_and_after();
    let human = d.run(&args);
    before_and_after();
    let json_run = d.run(&[&args[..], &["--json"]].concat());

    assert_eq!(exit_of(&human), 0, "{}", stderr_of(&human));
    let lines: Vec<String> = stderr_of(&human).lines().map(str::to_string).collect();
    assert_eq!(lines.len(), 2, "{lines:?}");
    assert!(
        lines[0].starts_with("could not tell whether alice/handbook-guides still stands:"),
        "{lines:?}"
    );
    assert_eq!(
        lines[1],
        "stopped sharing guides of alice/handbook: the collaborators of alice/handbook-guides removed"
    );
    assert_eq!(exit_of(&json_run), 0, "{}", stderr_of(&json_run));
    let doc = document(&json_run);
    assert_eq!(doc["retired"], Value::Null);
    assert!(doc.as_object().expect("an object").contains_key("retired"));
}
