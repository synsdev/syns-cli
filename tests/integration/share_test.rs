//! Binary-level behaviour of sharing a folder under an identity of its
//! own, reading which identity it stands under, stopping the sharing, and
//! aiming the collaborator commands at that identity (SPEC u300 Tests);
//! and marking a folder with a visibility of its own, naming it on the
//! lookup, and keeping a marked folder's name across an unshare (SPEC
//! u329 Tests); and a marking under a private holder naming the folder's
//! own name where its name is held, and warning where a public identity's
//! name shows the holder's (SPEC u332 Tests); and a bare collaborator
//! command inside a folder addressing its identity or refused as not
//! shared, and the folder's identity file settled by a share or a marking,
//! found converged by the next sync, and where an edit was left standing
//! stopping the next sync for its resolution (SPEC u333 Tests).
//!
//! Every binary row of those tables stands here under the name the table
//! gives it; `share_name_problem_weighs_the_repository_name_kind`,
//! `offered_share_name_takes_the_last_segment_lower_cased`,
//! `the_ask_takes_another_name_where_the_name_is_held`,
//! `a_moved_head_under_the_share_asks_no_other_name`,
//! `offered_marking_name_keys_on_the_holder_visibility` and
//! `carries_holder_name_matches_the_holder_name_and_a_dash_at_its_start`
//! stand in the tests module of `src/commands/share.rs`,
//! `address_segment_refuses_dot_segments_and_encodes_the_rest` in that of
//! `src/client.rs`, and u333's identifier rows in
//! `identifier_segment_test.rs`. Each test drives one deployment of
//! its own: a mock answering `EP-get-repo` for the holder with `role`
//! `owner` and `sharedFolder` `false` where the row names no other
//! answer, a config directory holding a credential for `alice`, a cache
//! directory, and a working directory holding the checkout of the row's
//! holder.

use assert_cmd::Command as AssertCommand;
use serde_json::{Value, json};
use serial_test::serial;
use std::path::{Path, PathBuf};
use wiremock::matchers::{method, path, path_regex, query_param};
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
    // SPEC u333 `settle_identity_file` 5: none of these folders stands on
    // disk, so a share or a lookup finding one standing also writes the
    // identity file warning line, which `reports` leaves aside.
    let expect = |args: &[&str], stdout: &str, stderr: &str| {
        let out = d.run(args);
        assert_eq!(exit_of(&out), 0, "{args:?}: {}", stderr_of(&out));
        assert_eq!(stdout_of(&out), stdout, "{args:?}");
        assert_eq!(reports(&out), stderr, "{args:?}");
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
        reports(&out),
        "shared clients/vela/q3-board of alice/work as alice/work-q3-board\n"
    );
}

/// The diagnostic stream of `out` with every identity file warning line
/// left aside.
fn reports(out: &std::process::Output) -> String {
    stderr_of(out)
        .lines()
        .filter(|line| {
            !(line.starts_with("warning: ") && line.ends_with("; syns sync takes it in, stopping for a resolution where that file holds an edit not yet published"))
        })
        .map(|line| format!("{line}\n"))
        .collect()
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
    // Ruled on round 1's open question: the identity on the primary
    // stream, as a first share writes it, so a script reads both alike.
    assert_eq!(stdout_of(&out), "alice/handbook-drafts\n");
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

// ---- a marking under a private holder (SPEC u332) -------------------------

const CLIENTS_MARKING: &str = "/api/v1/repos/alice/clients/folder-visibility";
const Q3_BOARD_LOOKUP: &str = "/api/v1/repos/alice/clients/shares/vela/q3-board";

/// The tree at `folder` of `holder` holding `file`, at the wire's own
/// `address`.
fn serve_tree(d: &Deployment, address: &str, file: &str) {
    d.serves(
        "GET",
        address,
        ResponseTemplate::new(200).set_body_json(json!({
            "entries": [{
                "name": file.rsplit('/').next().expect("a name"), "path": file,
                "type": "file", "size": 6, "sha": null,
            }],
            "commitSha": "a".repeat(40),
            "truncated": false,
        })),
    );
}

/// `P` of SPEC u332 Tests: the credential; `alice/clients` private with
/// `role` `owner` and `sharedFolder` false, its tree at `vela/q3-board`
/// holding `vela/q3-board/board.md`.
fn private_holder_deployment() -> Deployment {
    let d = Deployment::new(Some("alice/clients"));
    serve_tree(
        &d,
        "/api/v1/repos/alice/clients/tree/vela/q3-board",
        "vela/q3-board/board.md",
    );
    d
}

/// The marking at `address` answering `answer`, ahead of every answer
/// at a higher `priority`.
fn marking_at(d: &Deployment, address: &str, answer: ResponseTemplate, priority: u8) {
    d.mount(
        Mock::given(method("PUT"))
            .and(path(address.to_string()))
            .respond_with(answer)
            .with_priority(priority),
    );
}

/// A marking answer: `alice/NAME` at `visibility`, a shared folder.
fn marked(name: &str, visibility: &str) -> ResponseTemplate {
    ResponseTemplate::new(200).set_body_json(record_at("alice", name, true, visibility))
}

/// The lines of `out`'s diagnostic stream opening `warning:`.
fn warnings(out: &std::process::Output) -> Vec<String> {
    stderr_of(out)
        .lines()
        .filter(|line| line.starts_with("warning:"))
        .map(str::to_string)
        .collect()
}

const Q3_BOARD_WARNING: &str = "warning: alice/clients-q3-board is public, and its name shows anyone the name of the private repository alice/clients; to hide it, mark it private again with: syns share vela/q3-board --visibility private --repo alice/clients";

// SPEC u332 Tests, the row of this name.
#[test]
#[serial]
fn a_held_name_under_a_private_holder_names_the_folders_own_name() {
    let d = private_holder_deployment();
    marking_at(&d, CLIENTS_MARKING, refusal(409, "conflict"), 5);
    d.serves("GET", Q3_BOARD_LOOKUP, refusal(404, "not_found"));

    let out = d.run(&[
        "share",
        "vela/q3-board",
        "--visibility",
        "public",
        "--repo",
        "alice/clients",
    ]);

    assert_eq!(exit_of(&out), 1, "{}", stderr_of(&out));
    let err = stderr_of(&out);
    assert!(err.contains("conflict"), "{err}");
    assert_eq!(d.sent("PUT"), vec![CLIENTS_MARKING.to_string()]);
    assert_eq!(
        d.put_bodies(CLIENTS_MARKING),
        vec![json!({"path": "vela/q3-board", "visibility": "public"})]
    );
    assert!(
        err.contains("alice/q3-board is already held; give another name"),
        "{err}"
    );
    assert!(!err.contains("clients-q3-board"), "{err}");
}

// SPEC u332 Tests, the row of this name.
#[test]
#[serial]
fn a_held_name_under_a_public_holder_names_the_holder_prefixed_name() {
    let d = marking_deployment();
    marking_first(&d, refusal(409, "conflict"), 1, 1);
    d.serves("GET", DRAFTS_LOOKUP, refusal(404, "not_found"));

    let out = d.run(&[
        "share",
        "drafts",
        "--visibility",
        "public",
        "--repo",
        "alice/handbook",
    ]);

    assert_eq!(exit_of(&out), 1, "{}", stderr_of(&out));
    let err = stderr_of(&out);
    assert!(err.contains("conflict"), "{err}");
    assert!(
        err.contains("alice/handbook-drafts is already held; give another name"),
        "{err}"
    );
}

// SPEC u332 Tests, the row of this name.
#[test]
#[serial]
fn a_public_marking_under_a_private_holder_whose_name_carries_the_holders_warns() {
    let d = private_holder_deployment();
    marking_at(&d, CLIENTS_MARKING, marked("clients-q3-board", "public"), 5);
    let args = [
        "share",
        "vela/q3-board",
        "--visibility",
        "public",
        "--repo",
        "alice/clients",
    ];

    let human = d.run(&args);
    let json_run = d.run(&[&args[..], &["--json"]].concat());
    marking_at(&d, CLIENTS_MARKING, marked("clients-q3", "public"), 1);
    let named = d.run(&[&args[..], &["--name", "clients-q3"]].concat());

    for out in [&human, &json_run, &named] {
        assert_eq!(exit_of(out), 0, "{}", stderr_of(out));
    }
    assert!(
        stderr_of(&named).contains("warning: alice/clients-q3 is public"),
        "{}",
        stderr_of(&named)
    );
    assert_eq!(
        stderr_of(&human),
        format!(
            "vela/q3-board of alice/clients is public as alice/clients-q3-board\n{Q3_BOARD_WARNING}\n"
        )
    );
    assert_eq!(stdout_of(&human), "alice/clients-q3-board\n");
    let mut expected = record_at("alice", "clients-q3-board", true, "public");
    expected["holder"] = json!("alice/clients");
    expected["path"] = json!("vela/q3-board");
    assert_eq!(document(&json_run), expected);
    assert_eq!(stderr_of(&json_run), format!("{Q3_BOARD_WARNING}\n"));
}

// SPEC u332 Tests, the row of this name.
#[test]
#[serial]
fn the_warnings_remedy_quotes_a_spaced_path() {
    let d = Deployment::new(Some("alice/docs"));
    serve_tree(
        &d,
        "/api/v1/repos/alice/docs/tree/clients/q3%20plan",
        "clients/q3 plan/plan.md",
    );
    marking_at(
        &d,
        "/api/v1/repos/alice/docs/folder-visibility",
        marked("docs-q3-plan", "public"),
        5,
    );

    let out = d.run(&[
        "share",
        "clients/q3 plan",
        "--visibility",
        "public",
        "--repo",
        "alice/docs",
    ]);

    assert_eq!(exit_of(&out), 0, "{}", stderr_of(&out));
    let lines = warnings(&out);
    assert_eq!(lines.len(), 1, "{}", stderr_of(&out));
    assert!(
        lines[0].ends_with("syns share 'clients/q3 plan' --visibility private --repo alice/docs"),
        "{lines:?}"
    );
}

// SPEC u332 Tests, the row of this name.
#[test]
#[serial]
fn a_marking_the_test_does_not_match_warns_of_nothing() {
    let clients = [
        "share",
        "vela/q3-board",
        "--visibility",
        "public",
        "--repo",
        "alice/clients",
    ];

    let own_name = private_holder_deployment();
    marking_at(&own_name, CLIENTS_MARKING, marked("q3-board", "public"), 5);
    let own_name = own_name.run(&clients);

    let private = private_holder_deployment();
    marking_at(
        &private,
        CLIENTS_MARKING,
        marked("clients-q3-board", "private"),
        5,
    );
    let mut private_args = clients;
    private_args[3] = "private";
    let private = private.run(&private_args);

    let public_holder = marking_deployment();
    marking_first(&public_holder, marked("handbook-drafts", "public"), 1, 1);
    let public_holder = public_holder.run(&[
        "share",
        "drafts",
        "--visibility",
        "public",
        "--repo",
        "alice/handbook",
    ]);

    for out in [&own_name, &private, &public_holder] {
        assert_eq!(exit_of(out), 0, "{}", stderr_of(out));
        assert!(warnings(out).is_empty(), "{}", stderr_of(out));
    }
}

// SPEC u332 Tests, the row of this name.
#[test]
#[serial]
fn the_share_name_help_names_a_markings_default_in_a_private_holder() {
    let d = Deployment::new(None);

    let out = d.run(&["share", "--help"]);

    assert_eq!(exit_of(&out), 0, "{}", stderr_of(&out));
    assert!(
        stdout_of(&out).contains(
            "offered as <holder name>-<folder name> where absent, and as <folder name> on a marking in a private holder"
        ),
        "{}",
        stdout_of(&out)
    );
}

// ---- collaborators inside a shared folder, and its identity file (SPEC u333)

/// `B0` of SPEC u333 Tests: the folder's identity file before the share.
const B0: &str = "holder: alice/docs\npath: q3-plan\n";
/// `B1` of SPEC u333 Tests: the folder's identity file the share wrote.
const B1: &str = "holder: alice/docs\npath: q3-plan\nshared_as: docs-q3-plan\n";

const IDENTITY_COLLABORATORS: &str = "/api/v1/repos/alice/docs-q3-plan/collaborators";
const Q3_PLAN_LOOKUP: &str = "/api/v1/repos/alice/docs/shares/q3-plan";
const Q3_PLAN_RAW: &str = "/api/v1/repos/alice/docs/raw/q3-plan/.syns.yaml";

/// `W` of SPEC u333 Tests: a checkout of `alice/docs`, `W/q3-plan`'s
/// identity file holding `B1` and `W/budget`'s recording no `shared_as`.
fn w_deployment() -> Deployment {
    let d = Deployment::new(Some("alice/docs"));
    write(&d.w.join("q3-plan/.syns.yaml"), B1);
    write(
        &d.w.join("budget/.syns.yaml"),
        "holder: alice/docs\npath: budget\n",
    );
    d
}

/// Each request as `METHOD path`.
fn request_lines(d: &Deployment) -> Vec<String> {
    d.requests()
        .into_iter()
        .map(|r| format!("{} {}", r.method, r.url.path()))
        .collect()
}

/// The not-shared refusal line for the folder at `dir`.
fn not_shared_line(command: &str, dir: &Path) -> String {
    format!(
        "error: holder root required: {command} acts on a shared folder's own people, and the folder {} of alice/docs is not shared \u{2014} share it with: syns share .",
        dir.display()
    )
}

// SPEC u333 Tests, the row of this name.
#[test]
#[serial]
fn bare_collaborators_inside_a_shared_folder_address_its_identity() {
    let d = w_deployment();
    d.serves(
        "GET",
        IDENTITY_COLLABORATORS,
        ResponseTemplate::new(200).set_body_string(format!(
            r#"{{"data":[{COLLABORATOR}],"total":1,"limit":100,"offset":0}}"#
        )),
    );
    d.serves("POST", IDENTITY_COLLABORATORS, ResponseTemplate::new(201));
    d.serves(
        "PATCH",
        &format!("{IDENTITY_COLLABORATORS}/u-carol"),
        ResponseTemplate::new(200).set_body_string(COLLABORATOR),
    );
    d.serves(
        "DELETE",
        &format!("{IDENTITY_COLLABORATORS}/u-carol"),
        ResponseTemplate::new(204),
    );
    let folder = d.w.join("q3-plan");

    for args in [
        vec!["--json", "collaborators", "add", "carol", "--role", "read"],
        vec!["--json", "collaborators"],
        vec![
            "--json",
            "collaborators",
            "role",
            "u-carol",
            "--role",
            "write",
        ],
        vec!["--json", "collaborators", "remove", "u-carol", "--yes"],
        vec![
            "--json",
            "collaborators",
            "add",
            "carol",
            "--role",
            "read",
            "--repo",
            "Alice/Docs-Q3-Plan",
        ],
    ] {
        let out = d.run_in(&folder, &args);
        assert_eq!(exit_of(&out), 0, "{args:?}: {}", stderr_of(&out));
    }
    let sent = request_lines(&d);
    assert_eq!(sent.len(), 5, "{sent:?}");
    for line in &sent {
        let address = line.split_once(' ').expect("METHOD path").1;
        assert!(address.starts_with(IDENTITY_COLLABORATORS), "{line}");
        assert!(!address.starts_with("/api/v1/repos/alice/docs/"), "{line}");
    }
}

// SPEC u333 Tests, the row of this name.
#[test]
#[serial]
fn collaborators_at_the_holder_root_address_the_holder() {
    let d = w_deployment();
    d.serves(
        "GET",
        "/api/v1/repos/alice/docs/collaborators",
        ResponseTemplate::new(200).set_body_string(format!(
            r#"{{"data":[{COLLABORATOR}],"total":1,"limit":100,"offset":0}}"#
        )),
    );

    let out = d.run(&["--json", "collaborators"]);

    assert_eq!(exit_of(&out), 0, "{}", stderr_of(&out));
    assert_eq!(
        request_lines(&d),
        vec!["GET /api/v1/repos/alice/docs/collaborators".to_string()]
    );
}

// SPEC u333 Tests, the row of this name.
#[test]
#[serial]
fn a_shared_folder_refuses_the_holder_and_every_other_repository() {
    let d = w_deployment();
    let folder = d.w.join("q3-plan");

    for repo in ["alice/docs", "bob/other"] {
        let out = d.run_in(
            &folder,
            &[
                "collaborators",
                "add",
                "dave",
                "--role",
                "read",
                "--repo",
                repo,
            ],
        );
        let stderr = stderr_of(&out);
        assert_eq!(exit_of(&out), 2, "{repo}: {stderr}");
        assert!(
            stderr.starts_with("error: holder root required: "),
            "{repo}: {stderr}"
        );
        assert!(stderr.contains("alice/docs"), "{repo}: {stderr}");
        assert!(
            stderr.contains("run it from the root of a checkout"),
            "{repo}: {stderr}"
        );
    }
    assert!(d.requests().is_empty());
}

// SPEC u333 Tests, the row of this name.
#[test]
#[serial]
fn admin_on_the_folder_identity_is_the_servers_refusal() {
    let d = w_deployment();
    d.serves(
        "POST",
        IDENTITY_COLLABORATORS,
        refusal(422, "validation_error"),
    );

    let out = d.run_in(
        &d.w.join("q3-plan"),
        &["--json", "collaborators", "add", "dave", "--role", "admin"],
    );

    assert_eq!(
        d.bodies(IDENTITY_COLLABORATORS),
        vec![json!({"username": "dave", "role": "admin"})]
    );
    assert_eq!(request_lines(&d).len(), 1);
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

// SPEC u333 Tests, the row of this name.
#[test]
#[serial]
fn an_unshared_folder_refuses_every_collaborator_command_naming_the_share() {
    let d = w_deployment();
    write(
        &d.w.join("plan/.syns.yaml"),
        "holder: alice/docs\npath: plan\nshared_as: [x]\n",
    );
    let budget = d.w.join("budget");
    let plan = d.w.join("plan");

    for (cwd, args, command) in [
        (&budget, vec!["collaborators"], "syns collaborators"),
        (
            &budget,
            vec!["collaborators", "add", "carol", "--role", "read"],
            "syns collaborators add",
        ),
        (
            &budget,
            vec!["collaborators", "role", "u1", "--role", "read"],
            "syns collaborators role",
        ),
        (
            &budget,
            vec!["collaborators", "remove", "u1", "--yes"],
            "syns collaborators remove",
        ),
        (
            &budget,
            vec![
                "collaborators",
                "add",
                "carol",
                "--role",
                "read",
                "--repo",
                "alice/docs",
            ],
            "syns collaborators add",
        ),
        (
            &budget,
            vec!["collaborators", "--if-repo"],
            "syns collaborators",
        ),
        (&plan, vec!["collaborators"], "syns collaborators"),
    ] {
        let out = d.run_in(cwd, &args);
        let stderr = stderr_of(&out);
        assert_eq!(exit_of(&out), 2, "{args:?}: {stderr}");
        assert_eq!(stderr.trim_end(), not_shared_line(command, cwd), "{args:?}");
        assert!(!stderr.contains("root of a checkout"), "{args:?}: {stderr}");
    }
    assert!(d.requests().is_empty());
}

// SPEC u333 Tests, the row of this name.
#[test]
#[serial]
fn a_shared_as_spelling_no_repository_name_is_a_malformed_identity_file() {
    let d = w_deployment();
    write(
        &d.w.join("odd/.syns.yaml"),
        "holder: alice/docs\npath: odd\nshared_as: ..\n",
    );

    let out = d.run_in(&d.w.join("odd"), &["collaborators"]);

    assert_eq!(exit_of(&out), 1, "{}", stderr_of(&out));
    assert_eq!(
        stderr_of(&out).trim_end(),
        "error: invalid .syns.yaml: shared_as must name a repository under alice (got ..)"
    );
    assert!(d.requests().is_empty());
}

/// `EP-versions` of `alice/docs` at `q3-plan/.syns.yaml` answering version
/// `2`, `h2`, over `h1`; the raw file at `ref=h2` answering `B1` and at
/// `ref=h1` answering `at_h1`.
fn serve_identity_file_versions(d: &Deployment, at_h1: ResponseTemplate) {
    d.mount(
        Mock::given(method("GET"))
            .and(path("/api/v1/repos/alice/docs/versions"))
            .and(query_param("path", "q3-plan/.syns.yaml"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "data": [{
                    "version": 2, "sha": "h2", "parentSha": "h1", "message": "share",
                    "messageBody": null, "author": "alice",
                    "createdAt": "2026-10-06T00:00:00Z",
                    "filesChanged": ["q3-plan/.syns.yaml"],
                }],
                "total": 2, "limit": 1, "offset": 0,
            }))),
    );
    d.mount(
        Mock::given(method("GET"))
            .and(path(Q3_PLAN_RAW))
            .and(query_param("ref", "h2"))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(B1.as_bytes())),
    );
    d.mount(
        Mock::given(method("GET"))
            .and(path(Q3_PLAN_RAW))
            .and(query_param("ref", "h1"))
            .respond_with(at_h1),
    );
}

/// The deployment of `a_share_leaves_the_folder_identity_file_as_the_holder_holds_it`:
/// `W/q3-plan/.syns.yaml` holding `B0`, the lookup `404`, the share `201`
/// naming `alice/docs-q3-plan`, and the versions and raw reads.
fn first_share_deployment() -> Deployment {
    let d = w_deployment();
    write(&d.w.join("q3-plan/.syns.yaml"), B0);
    d.serves("GET", Q3_PLAN_LOOKUP, refusal(404, "not_found"));
    d.serves(
        "POST",
        "/api/v1/repos/alice/docs/shares",
        ResponseTemplate::new(201).set_body_json(record("alice", "docs-q3-plan", true)),
    );
    serve_identity_file_versions(&d, ResponseTemplate::new(200).set_body_bytes(B0.as_bytes()));
    d
}

fn identity_file(d: &Deployment) -> Vec<u8> {
    std::fs::read(d.w.join("q3-plan/.syns.yaml")).expect("the folder's identity file")
}

// SPEC u333 Tests, the row of this name.
#[test]
#[serial]
fn a_share_leaves_the_folder_identity_file_as_the_holder_holds_it() {
    let d = first_share_deployment();
    d.serves("POST", IDENTITY_COLLABORATORS, ResponseTemplate::new(201));

    let share = d.run(&["share", "q3-plan", "--name", "docs-q3-plan", "--json"]);
    assert_eq!(exit_of(&share), 0, "{}", stderr_of(&share));
    assert_eq!(identity_file(&d), B1.as_bytes());

    let add = d.run_in(
        &d.w.join("q3-plan"),
        &["--json", "collaborators", "add", "carol", "--role", "read"],
    );
    assert_eq!(exit_of(&add), 0, "{}", stderr_of(&add));
    assert_eq!(
        d.sent("POST").last(),
        Some(&IDENTITY_COLLABORATORS.to_string())
    );
}

// SPEC u333 Tests, the row of this name.
#[test]
#[serial]
fn a_share_writes_the_folder_identity_file_where_none_stood() {
    let d = w_deployment();
    std::fs::remove_file(d.w.join("q3-plan/.syns.yaml")).expect("no identity file");
    d.serves("GET", Q3_PLAN_LOOKUP, refusal(404, "not_found"));
    d.serves(
        "POST",
        "/api/v1/repos/alice/docs/shares",
        ResponseTemplate::new(201).set_body_json(record("alice", "docs-q3-plan", true)),
    );
    serve_identity_file_versions(&d, refusal(404, "not_found"));

    let out = d.run(&["share", "q3-plan", "--name", "docs-q3-plan", "--json"]);

    assert_eq!(exit_of(&out), 0, "{}", stderr_of(&out));
    assert_eq!(identity_file(&d), B1.as_bytes());
}

// SPEC u333 Tests, the row of this name.
#[test]
#[serial]
fn a_share_found_standing_settles_the_file_too() {
    let d = w_deployment();
    write(&d.w.join("q3-plan/.syns.yaml"), B0);
    d.serves(
        "GET",
        Q3_PLAN_LOOKUP,
        ResponseTemplate::new(200).set_body_json(record("alice", "docs-q3-plan", true)),
    );
    serve_identity_file_versions(&d, ResponseTemplate::new(200).set_body_bytes(B0.as_bytes()));

    let out = d.run(&["share", "q3-plan", "--json"]);

    assert_eq!(exit_of(&out), 0, "{}", stderr_of(&out));
    assert!(d.sent("POST").is_empty(), "{:?}", d.sent("POST"));
    assert_eq!(document(&out)["created"], false);
    assert_eq!(identity_file(&d), B1.as_bytes());
}

// SPEC u333 Tests, the row of this name.
#[test]
#[serial]
fn an_edited_identity_file_is_left_and_named() {
    let d = first_share_deployment();
    let edited = format!("{B0}checks: [lint]\n");
    write(&d.w.join("q3-plan/.syns.yaml"), &edited);

    let out = d.run(&["share", "q3-plan", "--name", "docs-q3-plan", "--json"]);

    assert_eq!(exit_of(&out), 0, "{}", stderr_of(&out));
    assert_eq!(document(&out)["created"], true);
    assert_eq!(identity_file(&d), edited.as_bytes());
    let warning = format!(
        "warning: {}/.syns.yaml was left as it stood and does not yet name alice/docs-q3-plan; syns sync takes it in, stopping for a resolution where that file holds an edit not yet published",
        d.w.join("q3-plan").display()
    );
    assert!(
        stderr_of(&out).lines().any(|line| line == warning),
        "{}",
        stderr_of(&out)
    );
}

/// The identity file warning line for `W/q3-plan` naming
/// `alice/docs-q3-plan`.
fn q3_plan_warning(d: &Deployment) -> String {
    format!(
        "warning: {}/.syns.yaml was left as it stood and does not yet name alice/docs-q3-plan; syns sync takes it in, stopping for a resolution where that file holds an edit not yet published",
        d.w.join("q3-plan").display()
    )
}

// CR1-1 of u333, `settle_identity_file` 2: a newest version whose file
// names no identity is taken as no settling, the file left and named.
#[test]
#[serial]
fn a_newest_version_naming_no_identity_leaves_the_file_and_warns() {
    let d = w_deployment();
    write(&d.w.join("q3-plan/.syns.yaml"), B0);
    d.serves("GET", Q3_PLAN_LOOKUP, refusal(404, "not_found"));
    d.serves(
        "POST",
        "/api/v1/repos/alice/docs/shares",
        ResponseTemplate::new(201).set_body_json(record("alice", "docs-q3-plan", true)),
    );
    d.mount(
        Mock::given(method("GET"))
            .and(path(Q3_PLAN_RAW))
            .and(query_param("ref", "h2"))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(B0.as_bytes()))
            .with_priority(1),
    );
    serve_identity_file_versions(&d, ResponseTemplate::new(200).set_body_bytes(B0.as_bytes()));

    let out = d.run(&["share", "q3-plan", "--name", "docs-q3-plan", "--json"]);

    assert_eq!(exit_of(&out), 0, "{}", stderr_of(&out));
    assert_eq!(identity_file(&d), B0.as_bytes());
    assert!(
        stderr_of(&out)
            .lines()
            .any(|line| line == q3_plan_warning(&d)),
        "{}",
        stderr_of(&out)
    );
}

// CR1-1 of u333, `settle_identity_file` 4: a refusal other than
// `NOT_FOUND` at the version before writes nothing over a folder whose
// identity file stands absent.
#[test]
#[serial]
fn a_refused_prior_version_writes_nothing() {
    let d = w_deployment();
    std::fs::remove_file(d.w.join("q3-plan/.syns.yaml")).expect("no identity file");
    d.serves("GET", Q3_PLAN_LOOKUP, refusal(404, "not_found"));
    d.serves(
        "POST",
        "/api/v1/repos/alice/docs/shares",
        ResponseTemplate::new(201).set_body_json(record("alice", "docs-q3-plan", true)),
    );
    serve_identity_file_versions(&d, refusal(503, "unavailable"));

    let out = d.run(&["share", "q3-plan", "--name", "docs-q3-plan", "--json"]);

    assert_eq!(exit_of(&out), 0, "{}", stderr_of(&out));
    assert!(!d.w.join("q3-plan/.syns.yaml").exists());
    assert!(
        stderr_of(&out)
            .lines()
            .any(|line| line == q3_plan_warning(&d)),
        "{}",
        stderr_of(&out)
    );
}

/// A raw read that saves `.1` over the file at `.0` before answering `200`
/// with `B0`, standing in for an edit saved while that read is in flight.
struct EditsThenAnswers(PathBuf, Vec<u8>);

impl wiremock::Respond for EditsThenAnswers {
    fn respond(&self, _: &Request) -> ResponseTemplate {
        std::fs::write(&self.0, &self.1).expect("the edit saved");
        ResponseTemplate::new(200).set_body_bytes(B0.as_bytes())
    }
}

// CR2-1 of u333, `settle_identity_file` 5: an edit saved while the version
// before is read is left standing and named, the file read again just
// before it would be replaced.
#[test]
#[serial]
fn an_edit_saved_during_the_prior_read_is_left_and_named() {
    let d = first_share_deployment();
    let edited = format!("{B0}checks: [lint]\n");
    d.mount(
        Mock::given(method("GET"))
            .and(path(Q3_PLAN_RAW))
            .and(query_param("ref", "h1"))
            .respond_with(EditsThenAnswers(
                d.w.join("q3-plan/.syns.yaml"),
                edited.clone().into_bytes(),
            ))
            .with_priority(1),
    );

    let out = d.run(&["share", "q3-plan", "--name", "docs-q3-plan", "--json"]);

    assert_eq!(exit_of(&out), 0, "{}", stderr_of(&out));
    assert_eq!(identity_file(&d), edited.as_bytes());
    assert!(
        stderr_of(&out)
            .lines()
            .any(|line| line == q3_plan_warning(&d)),
        "{}",
        stderr_of(&out)
    );
}

// SPEC u333 Tests, the row of this name.
#[test]
#[serial]
fn a_share_naming_its_holder_by_the_repository_option_writes_no_file() {
    let d = first_share_deployment();

    let out = d.run(&[
        "share",
        "q3-plan",
        "--repo",
        "alice/docs",
        "--name",
        "docs-q3-plan",
        "--json",
    ]);

    assert_eq!(exit_of(&out), 0, "{}", stderr_of(&out));
    let sent = request_lines(&d);
    assert!(
        sent.iter()
            .all(|line| !line.contains("/versions") && !line.contains("/raw/")),
        "{sent:?}"
    );
    assert_eq!(identity_file(&d), B0.as_bytes());
}

// SPEC u333 Tests, the row of this name.
#[test]
#[serial]
fn a_marking_settles_the_folder_identity_file() {
    let d = w_deployment();
    write(&d.w.join("q3-plan/.syns.yaml"), B0);
    serve_tree(
        &d,
        "/api/v1/repos/alice/docs/tree/q3-plan",
        "q3-plan/plan.md",
    );
    d.serves(
        "PUT",
        "/api/v1/repos/alice/docs/folder-visibility",
        ResponseTemplate::new(200).set_body_json(record_at(
            "alice",
            "docs-q3-plan",
            true,
            "public",
        )),
    );
    serve_identity_file_versions(&d, ResponseTemplate::new(200).set_body_bytes(B0.as_bytes()));

    let out = d.run(&["share", "q3-plan", "--visibility", "public", "--json"]);

    assert_eq!(exit_of(&out), 0, "{}", stderr_of(&out));
    assert_eq!(identity_file(&d), B1.as_bytes());
}

// ---- the next sync over a settled identity file (SPEC u333) ----------------

const SYNC_H1: &str = "1111111111111111111111111111111111111111";
const SYNC_H2: &str = "2222222222222222222222222222222222222222";

/// One commit of `alice/docs`: its version, its hash and its files.
struct DocsCommit {
    version: u32,
    sha: &'static str,
    tree: std::collections::BTreeMap<String, Vec<u8>>,
}

/// A stateful responder for `alice/docs` alone, copied from
/// `place_test`'s `Server`: its record, tree, raw and file reads and its
/// version list served from `commits`, the last of them the head, and a
/// push refused, so a sync reaching it fails.
struct DocsServer(Vec<DocsCommit>);

impl DocsServer {
    fn at(&self, reference: Option<String>) -> Option<&DocsCommit> {
        match reference {
            None => self.0.last(),
            Some(r) => self
                .0
                .iter()
                .find(|c| c.sha == r || c.version.to_string() == r),
        }
    }

    fn changed_at(&self, index: usize) -> Vec<String> {
        let tree = &self.0[index].tree;
        let empty = std::collections::BTreeMap::new();
        let before = if index == 0 {
            &empty
        } else {
            &self.0[index - 1].tree
        };
        tree.keys()
            .chain(before.keys())
            .filter(|p| tree.get(*p) != before.get(*p))
            .cloned()
            .collect::<std::collections::BTreeSet<_>>()
            .into_iter()
            .collect()
    }

    fn version_entry(&self, index: usize) -> Value {
        let c = &self.0[index];
        let parent = index.checked_sub(1).map(|i| self.0[i].sha);
        json!({
            "version": c.version, "sha": c.sha, "parentSha": parent,
            "message": "m", "messageBody": null, "author": "alice",
            "createdAt": "2026-10-06T00:00:00Z", "filesChanged": self.changed_at(index),
        })
    }
}

fn query_of(request: &Request, key: &str) -> Option<String> {
    request
        .url
        .query_pairs()
        .find(|(k, _)| k == key)
        .map(|(_, v)| v.to_string())
}

impl wiremock::Respond for DocsServer {
    fn respond(&self, request: &Request) -> ResponseTemplate {
        let decoded = urlencoding::decode(request.url.path())
            .map(|p| p.to_string())
            .unwrap_or_else(|_| request.url.path().to_string());
        let Some(rest) = decoded.strip_prefix("/api/v1/repos/alice/docs") else {
            return refusal(404, "not_found");
        };
        let method = request.method.as_str();
        let blob = syns_cli::push::hash::blob_sha1;
        if rest.is_empty() && method == "GET" {
            let head = self.0.last().expect("a head");
            let mut body = record("alice", "docs", false);
            body["commitSha"] = json!(head.sha);
            body["fileCount"] = json!(head.tree.len());
            return ResponseTemplate::new(200).set_body_json(body);
        }
        if method == "GET" && (rest == "/tree" || rest.starts_with("/tree/")) {
            let Some(commit) = self.at(query_of(request, "ref")) else {
                return refusal(404, "ref_not_found");
            };
            let under = rest.strip_prefix("/tree/").unwrap_or("");
            let recursive = query_of(request, "recursive").as_deref() == Some("true");
            let mut entries: std::collections::BTreeMap<String, Value> =
                std::collections::BTreeMap::new();
            for (place, bytes) in &commit.tree {
                if !under.is_empty() && !place.starts_with(&format!("{under}/")) {
                    continue;
                }
                let relative = if under.is_empty() {
                    place.as_str()
                } else {
                    &place[under.len() + 1..]
                };
                if recursive || !relative.contains('/') {
                    let name = place.rsplit('/').next().unwrap_or(place);
                    entries.insert(
                        place.clone(),
                        json!({ "name": name, "path": place, "type": "file",
                                "size": bytes.len(), "sha": blob(bytes) }),
                    );
                } else {
                    let first = relative.split('/').next().unwrap_or(relative);
                    let dir = if under.is_empty() {
                        first.to_string()
                    } else {
                        format!("{under}/{first}")
                    };
                    entries.insert(
                        dir.clone(),
                        json!({ "name": first, "path": dir, "type": "dir", "size": null, "sha": null }),
                    );
                }
            }
            if !under.is_empty() && entries.is_empty() {
                return refusal(404, "not_found");
            }
            return ResponseTemplate::new(200).set_body_json(json!({
                "entries": entries.into_values().collect::<Vec<_>>(),
                "commitSha": commit.sha,
                "truncated": false,
            }));
        }
        if (method == "GET" || method == "HEAD")
            && let Some(file) = rest.strip_prefix("/raw/")
        {
            let Some(commit) = self.at(query_of(request, "ref")) else {
                return refusal(404, "ref_not_found");
            };
            return match commit.tree.get(file) {
                Some(bytes) => ResponseTemplate::new(200).set_body_bytes(bytes.clone()),
                None => refusal(404, "not_found"),
            };
        }
        if method == "GET"
            && let Some(file) = rest.strip_prefix("/files/")
        {
            let Some(commit) = self.at(query_of(request, "ref")) else {
                return refusal(404, "ref_not_found");
            };
            return match commit.tree.get(file) {
                Some(bytes) => ResponseTemplate::new(200).set_body_json(json!({
                    "content": String::from_utf8_lossy(bytes), "sha": blob(bytes),
                    "size": bytes.len(),
                })),
                None => refusal(404, "not_found"),
            };
        }
        if method == "GET" && rest == "/versions" {
            let place = query_of(request, "path");
            let limit: usize = query_of(request, "limit")
                .and_then(|l| l.parse().ok())
                .unwrap_or(20);
            let offset: usize = query_of(request, "offset")
                .and_then(|o| o.parse().ok())
                .unwrap_or(0);
            let listed: Vec<usize> = (0..self.0.len())
                .rev()
                .filter(|index| match &place {
                    Some(place) => self
                        .changed_at(*index)
                        .iter()
                        .any(|p| p == place || p.starts_with(&format!("{place}/"))),
                    None => true,
                })
                .collect();
            let data: Vec<Value> = listed
                .iter()
                .skip(offset)
                .take(limit)
                .map(|index| self.version_entry(*index))
                .collect();
            return ResponseTemplate::new(200).set_body_json(json!({
                "data": data, "total": listed.len(), "limit": limit, "offset": offset,
            }));
        }
        if method == "GET"
            && let Some(reference) = rest.strip_prefix("/versions/")
        {
            return match self
                .0
                .iter()
                .position(|c| c.sha == reference || c.version.to_string() == reference)
            {
                Some(index) => ResponseTemplate::new(200).set_body_json(self.version_entry(index)),
                None => refusal(404, "not_found"),
            };
        }
        refusal(404, "not_found")
    }
}

// SPEC u333 Tests, the row of this name.
#[test]
#[serial]
fn the_next_sync_finds_the_settled_file_converged() {
    let d = w_deployment();
    write(&d.w.join("q3-plan/.syns.yaml"), B0);
    let on_disk: std::collections::BTreeMap<String, Vec<u8>> = files_in(&d.w).into_iter().collect();
    let mut head = on_disk.clone();
    head.insert("q3-plan/.syns.yaml".into(), B1.as_bytes().to_vec());
    d.mount(
        Mock::given(wiremock::matchers::any())
            .respond_with(DocsServer(vec![
                DocsCommit {
                    version: 1,
                    sha: SYNC_H1,
                    tree: on_disk.clone(),
                },
                DocsCommit {
                    version: 2,
                    sha: SYNC_H2,
                    tree: head,
                },
            ]))
            .with_priority(1),
    );
    let stores =
        syns_cli::config::StoreRoots::resolve(Some(d.cache.path()), d.cache.path(), d.cache.path());
    let copy = syns_cli::push::working_copy::WorkingCopy::open(&stores, "alice", "docs", &d.w)
        .expect("the holder copy");
    copy.record_base(
        SYNC_H1,
        on_disk
            .iter()
            .map(|(place, bytes)| (place.clone(), syns_cli::push::hash::blob_sha1(bytes)))
            .collect(),
    )
    .expect("a base");
    write(&d.w.join("q3-plan/.syns.yaml"), B1);

    let out = d.run(&["--json", "sync"]);

    assert_eq!(exit_of(&out), 0, "{}", stderr_of(&out));
    assert!(
        document(&out).get("recoveryId").is_none(),
        "{}",
        stdout_of(&out)
    );
    let settled = std::fs::read(d.w.join("q3-plan/.syns.yaml")).expect("the identity file");
    assert_eq!(settled, B1.as_bytes());
    assert!(
        !String::from_utf8_lossy(&settled)
            .lines()
            .any(|line| line.starts_with("<<<<<<<"))
    );
    let copy =
        syns_cli::push::working_copy::WorkingCopy::open_existing(&stores, "alice", "docs", &d.w)
            .expect("open")
            .expect("the holder copy");
    assert_eq!(copy.base().expect("a base").commit_sha(), Some(SYNC_H2));
    assert!(
        !request_lines(&d).contains(&"PUT /api/v1/repos/alice/docs/push".to_string()),
        "{:?}",
        request_lines(&d)
    );
}

// SPEC u333 Tests, the row of this name: the next sync over an identity
// file a share left as it stood, holding an edit not yet published, stops
// for the resolution the identity file warning line names.
#[test]
#[serial]
fn the_next_sync_after_a_left_edit_stops_for_its_resolution() {
    let d = w_deployment();
    write(&d.w.join("q3-plan/.syns.yaml"), B0);
    let on_disk: std::collections::BTreeMap<String, Vec<u8>> = files_in(&d.w).into_iter().collect();
    let mut head = on_disk.clone();
    head.insert("q3-plan/.syns.yaml".into(), B1.as_bytes().to_vec());
    d.mount(
        Mock::given(wiremock::matchers::any())
            .respond_with(DocsServer(vec![
                DocsCommit {
                    version: 1,
                    sha: SYNC_H1,
                    tree: on_disk.clone(),
                },
                DocsCommit {
                    version: 2,
                    sha: SYNC_H2,
                    tree: head,
                },
            ]))
            .with_priority(1),
    );
    let stores =
        syns_cli::config::StoreRoots::resolve(Some(d.cache.path()), d.cache.path(), d.cache.path());
    let copy = syns_cli::push::working_copy::WorkingCopy::open(&stores, "alice", "docs", &d.w)
        .expect("the holder copy");
    copy.record_base(
        SYNC_H1,
        on_disk
            .iter()
            .map(|(place, bytes)| (place.clone(), syns_cli::push::hash::blob_sha1(bytes)))
            .collect(),
    )
    .expect("a base");
    write(
        &d.w.join("q3-plan/.syns.yaml"),
        &format!("# local note\n{B0}"),
    );

    let out = d.run(&["--json", "sync"]);

    assert_eq!(exit_of(&out), 4, "{}", stderr_of(&out));
    let document = document(&out);
    assert_eq!(document["outcome"], "resolution_required", "{document}");
    assert_eq!(
        document["resolution"]["collisions"],
        json!([{"path": "q3-plan/.syns.yaml", "kind": "modify_modify"}]),
        "{document}"
    );
    let left = std::fs::read(d.w.join("q3-plan/.syns.yaml")).expect("the identity file");
    assert_eq!(left, format!("# local note\n{B1}").as_bytes());
    assert!(
        !String::from_utf8_lossy(&left)
            .lines()
            .any(|line| line.starts_with("<<<<<<<"))
    );
    assert!(
        !request_lines(&d).contains(&"PUT /api/v1/repos/alice/docs/push".to_string()),
        "{:?}",
        request_lines(&d)
    );
}

/// Every file under `dir`, by its place from `dir`.
fn files_in(dir: &Path) -> Vec<(String, Vec<u8>)> {
    fn walk(root: &Path, dir: &Path, out: &mut Vec<(String, Vec<u8>)>) {
        for entry in std::fs::read_dir(dir).into_iter().flatten().flatten() {
            let p = entry.path();
            if p.is_dir() {
                walk(root, &p, out);
            } else {
                let place = p.strip_prefix(root).expect("under root");
                out.push((
                    place.to_string_lossy().replace('\\', "/"),
                    std::fs::read(&p).expect("read"),
                ));
            }
        }
    }
    let mut out = Vec::new();
    walk(dir, dir, &mut out);
    out.sort();
    out
}
