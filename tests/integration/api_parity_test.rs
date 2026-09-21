//! Binary-level behaviour of the ten invocations u272 adds, the page
//! options it puts on the standing collaborator listing, and the guard
//! on the invocation spellings the argument tree registers
//! (SPEC u272 Tests).
//!
//! Every row of that table stands here under the name the table gives
//! it. One mock deployment, one config directory and one working
//! directory per test, so two tests never share a store — the harness
//! `tests/integration/reads_test.rs` already uses.

use assert_cmd::Command as AssertCommand;
use serde_json::{Value, json};
use wiremock::matchers::{method, path as path_matcher, query_param};
use wiremock::{Mock, MockServer, Request, ResponseTemplate};

/// The bodies `units/cli/u272/prototype/bodies/` captured, one literal
/// per entry the added invocations reach.
mod bodies {
    pub const FORKS_TWO: &str = r##"{"data":[{"owner":"u272bob","name":"u272-fork","description":"the fork","commitSha":null,"status":"active","author":null,"tags":[],"visibility":"public","forkedFrom":{"owner":"u272alice","name":"u272-parent"},"forkCount":0,"fileCount":3,"role":null,"createdAt":"2026-09-21T04:43:40.644Z","updatedAt":"2026-09-21T04:43:40.644Z"},{"owner":"u272cara","name":"u272-fork","description":null,"commitSha":null,"status":"draft","author":null,"tags":[],"visibility":"private","forkedFrom":{"owner":"u272alice","name":"u272-parent"},"forkCount":0,"fileCount":0,"role":null,"createdAt":"2026-09-21T04:43:41.101Z","updatedAt":"2026-09-21T04:43:41.101Z"}],"total":2,"limit":20,"offset":0}"##;
    pub const VERSION_HEAD: &str = r##"{"version":596,"sha":"7618bcff37fa4dc19f976c9e29019d56d6aeed68","parentSha":"686ee7156c28aca8f7d9411c3f1a50631257d59d","message":"claude code session","messageBody":"the long half of the caption","author":"bartsoj","createdAt":"2026-09-20T13:55:04Z","filesChanged":["units/cli/u270/SPEC.md"],"provenance":{"publisher":"bartsoj","integration":null,"run":null,"trigger":null,"taskRef":null}}"##;
    pub const SEARCH_TWO: &str = r##"{"data":[{"id":"u272user1111111111111111111111111","username":"u272bar","name":"U272 Bar","image":null},{"id":"u272user2222222222222222222222222","username":"barnaby","name":"Barnaby","image":null}]}"##;
    pub const SEARCH_429: &str = r##"{"error":"rate_limited","message":"Too many requests"}"##;
    pub const PROFILE_SELF: &str = r##"{"id":"6A2PNdiQIutnYqnn3macdtuqgxrg7Yvv","username":"alice","name":"Alice","image":null,"bio":null,"location":"Amsterdam","pronouns":null,"company":"JetBrains","timeZone":null,"links":[{"id":"6efbd0a9-e09d-459d-9277-98838c854b5b","kind":"linkedin","value":"https://www.linkedin.com/in/bartsoj/","label":null,"sortOrder":0,"createdAt":"2026-05-26T11:29:05.737Z","updatedAt":"2026-09-21T04:41:06.239Z"}],"createdAt":"2026-05-02T16:05:18.608Z","repoCount":2}"##;
    pub const PROFILE_ANON: &str = r##"{"id":"6A2PNdiQIutnYqnn3macdtuqgxrg7Yvv","username":"alice","name":"Alice","image":null,"bio":null,"location":"Amsterdam","pronouns":null,"company":"JetBrains","timeZone":null,"links":[],"createdAt":"2026-05-02T16:05:18.608Z","repoCount":1}"##;
    pub const USER_MISSING: &str = r##"{"error":"user_not_found","message":"User not found"}"##;
    pub const LINKS_CREATE: &str = r##"{"links":[{"id":"6efbd0a9-e09d-459d-9277-98838c854b5b","kind":"linkedin","value":"https://www.linkedin.com/in/bartsoj/","label":null,"sortOrder":0,"createdAt":"2026-05-26T11:29:05.737Z","updatedAt":"2026-05-26T11:29:33.124Z"},{"id":"21474465-4502-4150-adf0-aa0b6a0cc4d5","kind":"github","value":"https://github.com/BartSoj","label":null,"sortOrder":1,"createdAt":"2026-05-26T11:28:51.635Z","updatedAt":"2026-05-26T11:29:40.643Z"}]}"##;
    pub const LINKS_CAP: &str = r##"{"error":"conflict","message":"Link limit reached"}"##;
    pub const LINKS_REORDER: &str = r##"{"links":[{"id":"33333333-3333-3333-3333-333333333333","kind":"generic","value":"https://u272.example.test/c","label":null,"sortOrder":0,"createdAt":"2026-09-21T04:41:05.688Z","updatedAt":"2026-09-21T04:41:06.038Z"},{"id":"11111111-1111-1111-1111-111111111111","kind":"github","value":"https://github.com/BartSoj","label":null,"sortOrder":1,"createdAt":"2026-05-26T11:28:51.635Z","updatedAt":"2026-09-21T04:41:06.050Z"},{"id":"22222222-2222-2222-2222-222222222222","kind":"linkedin","value":"https://www.linkedin.com/in/bartsoj/","label":null,"sortOrder":2,"createdAt":"2026-05-26T11:29:05.737Z","updatedAt":"2026-09-21T04:41:06.062Z"}]}"##;
    pub const ROLE_LOCAL: &str = r##"{"user":{"id":"u272user1111111111111111111111111","name":"U272 Bob","username":"u272bob","email":"u272bob@example.test","emailVerified":true,"image":null,"createdAt":"2026-09-21T04:43:40.637Z","updatedAt":"2026-09-21T04:43:40.637Z"},"role":"write","addedBy":"u272alice","createdAt":"2026-09-21T04:43:40.645Z"}"##;
    pub const COLLAB_PAGED: &str = r##"{"data":[{"user":{"id":"u272user2222222222222222222222222","name":"U272 Cara","username":"u272cara","email":"u272cara@example.test","emailVerified":true,"image":null,"createdAt":"2026-09-21T04:43:40.637Z","updatedAt":"2026-09-21T04:43:40.637Z"},"role":"read","addedBy":"u272alice","createdAt":"2026-09-21T04:43:40.645Z"}],"total":3,"limit":2,"offset":1}"##;
    pub const CREATED_REPO: &str = r##"{"owner":"alice","name":"notes","description":null,"commitSha":null,"status":"draft","author":null,"tags":[],"visibility":"private","forkedFrom":null,"forkCount":0,"fileCount":0,"role":"owner","createdAt":"2026-09-21T04:42:46.167Z","updatedAt":"2026-09-21T04:42:46.167Z"}"##;
}

/// One mock deployment, one config directory and one working directory
/// — all made for the test that asked for them.
struct Deployment {
    rt: tokio::runtime::Runtime,
    server: MockServer,
    home: tempfile::TempDir,
    cache: tempfile::TempDir,
    work: tempfile::TempDir,
}

impl Deployment {
    fn new() -> Deployment {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime");
        let server = rt.block_on(MockServer::start());
        Deployment {
            rt,
            server,
            home: tempfile::tempdir().expect("config dir"),
            cache: tempfile::tempdir().expect("cache dir"),
            work: tempfile::tempdir().expect("working dir"),
        }
    }

    fn mount(&self, mock: Mock) {
        self.rt.block_on(async { mock.mount(&self.server).await });
    }

    fn serves(&self, verb: &str, address: &str, status: u16, body: &str) {
        self.mount(
            Mock::given(method(verb))
                .and(path_matcher(address.to_string()))
                .respond_with(ResponseTemplate::new(status).set_body_string(body.to_string())),
        );
    }

    fn requests(&self) -> Vec<Request> {
        self.rt
            .block_on(async { self.server.received_requests().await.expect("requests") })
    }

    fn paths(&self) -> Vec<String> {
        self.requests()
            .iter()
            .map(|r| r.url.path().to_string())
            .collect()
    }

    /// A credential on the machine, recording the handle where one is
    /// given — the shape `syns login` writes.
    fn credential(&self, token: &str, username: Option<&str>) {
        let store =
            syns_cli::auth::token::TokenStore::new(self.home.path().join("credentials.json"));
        store
            .write_with_username(token, username)
            .expect("credential written");
    }

    /// A credential whose form does not parse, which a reading verb
    /// carries as none.
    fn unreadable_credential(&self) {
        std::fs::write(self.home.path().join("credentials.json"), "not json {")
            .expect("credential written");
    }

    /// An identity file in the working directory.
    fn identity(&self, owner: &str, name: &str) {
        std::fs::write(
            self.work.path().join(".syns.yaml"),
            format!("owner: {owner}\nname: {name}\n"),
        )
        .expect("identity file written");
    }

    fn run(&self, args: &[&str]) -> std::process::Output {
        AssertCommand::cargo_bin("syns")
            .expect("syns binary")
            .current_dir(self.work.path())
            .env("SYNS_CONFIG_DIR", self.home.path())
            .env("SYNS_CACHE_DIR", self.cache.path())
            .env_remove("SYNS_URL")
            .env_remove("CI")
            .arg("--server")
            .arg(self.server.uri())
            .args(args)
            .output()
            .expect("run syns")
    }
}

fn stdout(output: &std::process::Output) -> String {
    String::from_utf8_lossy(&output.stdout).to_string()
}

fn stderr(output: &std::process::Output) -> String {
    String::from_utf8_lossy(&output.stderr).to_string()
}

fn document(output: &std::process::Output) -> Value {
    serde_json::from_str(stdout(output).trim()).expect("one document on the primary stream")
}

fn served(raw: &str) -> Value {
    serde_json::from_str(raw).expect("the captured body parses")
}

// --- forks ---

#[test]
fn forks_lists_the_served_page() {
    let deployment = Deployment::new();
    deployment.serves(
        "GET",
        "/api/v1/repos/u272alice/u272-parent/forks",
        200,
        bodies::FORKS_TWO,
    );

    let output = deployment.run(&["forks", "--repo", "u272alice/u272-parent", "--json"]);

    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    let document = document(&output);
    assert_eq!(document, served(bodies::FORKS_TWO));
    assert_eq!(document["total"], json!(2));
    assert_eq!(document["data"].as_array().unwrap().len(), 2);
}

#[test]
fn forks_skips_where_no_identity_resolves() {
    let deployment = Deployment::new();

    let output = deployment.run(&["forks", "--if-repo", "--json"]);

    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    assert_eq!(
        stdout(&output).trim(),
        r#"{"skipped":true,"reason":"no_syns_repo"}"#
    );
    assert!(deployment.paths().is_empty(), "no request is made");
}

#[test]
fn forks_reads_past_an_unreadable_credential() {
    let deployment = Deployment::new();
    deployment.unreadable_credential();
    deployment.serves(
        "GET",
        "/api/v1/repos/u272alice/u272-parent/forks",
        200,
        bodies::FORKS_TWO,
    );

    let output = deployment.run(&["forks", "--repo", "u272alice/u272-parent", "--json"]);

    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    assert_eq!(document(&output), served(bodies::FORKS_TWO));
    let requests = deployment.requests();
    assert_eq!(requests.len(), 1);
    assert!(
        requests[0].headers.get("authorization").is_none(),
        "the request carries no credential"
    );
}

// --- history show ---

#[test]
fn history_show_answers_one_version() {
    let deployment = Deployment::new();
    deployment.serves(
        "GET",
        "/api/v1/repos/u272alice/u272-parent/versions/2",
        200,
        bodies::VERSION_HEAD,
    );

    let output = deployment.run(&[
        "history",
        "show",
        "2",
        "--repo",
        "u272alice/u272-parent",
        "--json",
    ]);

    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    assert_eq!(document(&output), served(bodies::VERSION_HEAD));

    // The render carries the parent hash and the message body.
    let rendered = deployment.run(&["history", "show", "2", "--repo", "u272alice/u272-parent"]);
    assert_eq!(rendered.status.code(), Some(0), "{}", stderr(&rendered));
    let text = stdout(&rendered);
    assert!(
        text.contains("686ee7156c28aca8f7d9411c3f1a50631257d59d"),
        "the parent hash stands in the render: {text}"
    );
    assert!(
        text.contains("the long half of the caption"),
        "the message body stands in the render: {text}"
    );
}

#[test]
fn history_show_refuses_an_ordinal_below_one() {
    let deployment = Deployment::new();
    deployment.identity("u272alice", "u272-parent");

    let output = deployment.run(&["history", "show", "0"]);

    assert_eq!(output.status.code(), Some(1));
    assert!(
        stderr(&output).contains("configuration error: version must be \u{2265} 1"),
        "{}",
        stderr(&output)
    );
    assert!(deployment.paths().is_empty(), "no request is made");
}

#[test]
fn history_show_names_the_reference_on_a_miss() {
    let deployment = Deployment::new();
    deployment.identity("u272alice", "u272-parent");
    deployment.serves(
        "GET",
        "/api/v1/repos/u272alice/u272-parent/versions/9",
        404,
        r#"{"error":"not_found","message":"Version not found"}"#,
    );

    let output = deployment.run(&["history", "show", "9"]);

    assert_eq!(output.status.code(), Some(1));
    let refusal = stderr(&output);
    assert!(refusal.contains("version not found: 9"), "{refusal}");
    assert!(
        !refusal.contains("path not found"),
        "the refusal names the reference rather than a path: {refusal}"
    );
}

// --- users ---

#[test]
fn users_passes_the_search_body_through() {
    let deployment = Deployment::new();
    deployment.credential("u272-token", Some("alice"));
    deployment.mount(
        Mock::given(method("GET"))
            .and(path_matcher("/api/v1/users"))
            .and(query_param("q", "bar"))
            .respond_with(ResponseTemplate::new(200).set_body_string(bodies::SEARCH_TWO)),
    );

    let output = deployment.run(&["users", "bar", "--json"]);

    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    let document = document(&output);
    assert_eq!(document, served(bodies::SEARCH_TWO));
    let handles: Vec<&str> = document["data"]
        .as_array()
        .unwrap()
        .iter()
        .map(|u| u["username"].as_str().unwrap())
        .collect();
    assert_eq!(
        handles,
        vec!["u272bar", "barnaby"],
        "the answer's own order"
    );
}

#[test]
fn users_refuses_with_no_credential() {
    let deployment = Deployment::new();

    let output = deployment.run(&["users", "bar"]);

    assert_eq!(output.status.code(), Some(1));
    assert!(
        stderr(&output).contains("authentication required"),
        "{}",
        stderr(&output)
    );
    assert!(deployment.paths().is_empty(), "no request is made");
}

#[test]
fn users_carries_the_rate_refusal() {
    let deployment = Deployment::new();
    deployment.credential("u272-token", Some("alice"));
    deployment.serves("GET", "/api/v1/users", 429, bodies::SEARCH_429);

    let output = deployment.run(&["users", "bar", "--json"]);

    assert_eq!(output.status.code(), Some(1));
    let document = document(&output);
    assert_eq!(
        document.as_object().unwrap().keys().collect::<Vec<_>>(),
        vec!["error"],
        "one document whose single key is `error`"
    );
    let refusal = document["error"].as_str().unwrap();
    assert!(refusal.contains("429"), "{refusal}");
    assert!(refusal.contains("rate_limited"), "{refusal}");
    // `PROTOTYPE.md` NR-01: the origin sends no interval and no
    // remaining budget, so the report names neither.
    for invented in ["retry", "Retry", "seconds", "remaining", "wait"] {
        assert!(
            !refusal.contains(invented),
            "the refusal invents no wait: {refusal}"
        );
    }
}

// --- user ---

#[test]
fn user_reads_the_callers_own_profile() {
    let deployment = Deployment::new();
    deployment.credential("u272-token", Some("alice"));
    deployment.serves("GET", "/api/v1/users/alice", 200, bodies::PROFILE_SELF);

    let output = deployment.run(&["user", "--json"]);

    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    assert_eq!(document(&output), served(bodies::PROFILE_SELF));
    assert_eq!(
        deployment.paths(),
        vec!["/api/v1/users/alice".to_string()],
        "no session request is made"
    );
}

#[test]
fn user_refuses_where_the_session_names_none() {
    let deployment = Deployment::new();
    deployment.credential("u272-token", None);
    deployment.serves("GET", "/api/auth/get-session", 200, "null");

    let output = deployment.run(&["user", "--json"]);

    assert_eq!(output.status.code(), Some(1));
    assert!(
        document(&output)["error"]
            .as_str()
            .unwrap()
            .contains("authentication required"),
        "{}",
        stdout(&output)
    );
    assert_eq!(
        deployment.paths(),
        vec!["/api/auth/get-session".to_string()],
        "no profile request is made"
    );
}

#[test]
fn user_counts_by_who_is_reading() {
    let anonymous = Deployment::new();
    anonymous.serves("GET", "/api/v1/users/alice", 200, bodies::PROFILE_ANON);
    let first = anonymous.run(&["user", "alice", "--json"]);
    assert_eq!(first.status.code(), Some(0), "{}", stderr(&first));
    assert_eq!(document(&first)["repoCount"], json!(1));
    assert!(
        anonymous.requests()[0]
            .headers
            .get("authorization")
            .is_none(),
        "nothing rides where no credential stands"
    );

    let own = Deployment::new();
    own.credential("u272-token", Some("alice"));
    own.serves("GET", "/api/v1/users/alice", 200, bodies::PROFILE_SELF);
    let second = own.run(&["user", "alice", "--json"]);
    assert_eq!(second.status.code(), Some(0), "{}", stderr(&second));
    assert_eq!(document(&second)["repoCount"], json!(2));
    assert_eq!(
        own.requests()[0]
            .headers
            .get("authorization")
            .map(|v| v.to_str().unwrap().to_string()),
        Some("Bearer u272-token".to_string())
    );
}

#[test]
fn user_refuses_an_unknown_handle() {
    let deployment = Deployment::new();
    deployment.serves("GET", "/api/v1/users/nobody", 404, bodies::USER_MISSING);

    let output = deployment.run(&["user", "nobody"]);

    assert_eq!(output.status.code(), Some(1));
    assert!(
        stderr(&output).contains("user_not_found"),
        "{}",
        stderr(&output)
    );
}

// --- links ---

#[test]
fn links_add_answers_the_whole_list() {
    let deployment = Deployment::new();
    deployment.credential("u272-token", Some("alice"));
    deployment.serves("POST", "/api/v1/me/links", 201, bodies::LINKS_CREATE);

    let output = deployment.run(&[
        "links",
        "add",
        "--kind",
        "github",
        "--value",
        "https://example.test/a",
        "--json",
    ]);

    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    let document = document(&output);
    assert_eq!(document, served(bodies::LINKS_CREATE));
    let orders: Vec<i64> = document["links"]
        .as_array()
        .unwrap()
        .iter()
        .map(|l| l["sortOrder"].as_i64().unwrap())
        .collect();
    assert_eq!(orders, vec![0, 1]);
}

#[test]
fn links_add_carries_the_cap_refusal() {
    let deployment = Deployment::new();
    deployment.credential("u272-token", Some("alice"));
    deployment.serves("POST", "/api/v1/me/links", 409, bodies::LINKS_CAP);

    let output = deployment.run(&[
        "links",
        "add",
        "--kind",
        "website",
        "--value",
        "https://example.test/b",
        "--json",
    ]);

    assert_eq!(output.status.code(), Some(1));
    let document = document(&output);
    assert_eq!(
        document.as_object().unwrap().keys().collect::<Vec<_>>(),
        vec!["error"]
    );
    let refusal = document["error"].as_str().unwrap();
    assert!(refusal.contains("409"), "{refusal}");
    assert!(refusal.contains("conflict"), "{refusal}");
}

#[test]
fn links_update_refuses_naming_nothing() {
    let deployment = Deployment::new();
    deployment.credential("u272-token", Some("alice"));

    let output = deployment.run(&["links", "update", "11111111-1111-1111-1111-111111111111"]);

    assert_eq!(output.status.code(), Some(1));
    assert!(
        stderr(&output).contains(
            "configuration error: name at least one of --kind, --value, --label or --sort-order"
        ),
        "{}",
        stderr(&output)
    );
    assert!(deployment.paths().is_empty(), "no request is made");
}

#[test]
fn links_reorder_sends_the_typed_order() {
    let deployment = Deployment::new();
    deployment.credential("u272-token", Some("alice"));
    deployment.serves(
        "POST",
        "/api/v1/me/links/reorder",
        200,
        bodies::LINKS_REORDER,
    );

    let order = [
        "33333333-3333-3333-3333-333333333333",
        "11111111-1111-1111-1111-111111111111",
        "22222222-2222-2222-2222-222222222222",
    ];
    let output = deployment.run(&["links", "reorder", order[0], order[1], order[2], "--json"]);

    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    assert_eq!(document(&output), served(bodies::LINKS_REORDER));
    let sent: Value = serde_json::from_slice(&deployment.requests()[0].body).unwrap();
    assert_eq!(sent, json!({ "order": order }));
}

// --- collaborators ---

#[test]
fn collaborators_role_changes_one_grant() {
    let deployment = Deployment::new();
    deployment.identity("u272alice", "u272-parent");
    deployment.credential("u272-token", Some("u272alice"));
    deployment.serves(
        "PATCH",
        "/api/v1/repos/u272alice/u272-parent/collaborators/u272user1111111111111111111111111",
        200,
        bodies::ROLE_LOCAL,
    );

    let output = deployment.run(&[
        "collaborators",
        "role",
        "u272user1111111111111111111111111",
        "--role",
        "write",
        "--json",
    ]);

    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    let document = document(&output);
    assert_eq!(document, served(bodies::ROLE_LOCAL));
    assert_eq!(document["role"], json!("write"));
    let sent: Value = serde_json::from_slice(&deployment.requests()[0].body).unwrap();
    assert_eq!(sent, json!({"role": "write"}));
}

#[test]
fn collaborators_pages_its_listing() {
    let deployment = Deployment::new();
    deployment.identity("u272alice", "u272-parent");
    deployment.mount(
        Mock::given(method("GET"))
            .and(path_matcher(
                "/api/v1/repos/u272alice/u272-parent/collaborators",
            ))
            .and(query_param("limit", "2"))
            .and(query_param("offset", "1"))
            .respond_with(ResponseTemplate::new(200).set_body_string(bodies::COLLAB_PAGED)),
    );

    let output = deployment.run(&["collaborators", "--limit", "2", "--offset", "1", "--json"]);

    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    assert_eq!(document(&output), served(bodies::COLLAB_PAGED));
    let query = deployment.requests()[0].url.query().unwrap().to_string();
    assert!(query.contains("limit=2"), "{query}");
    assert!(query.contains("offset=1"), "{query}");
}

// --- repo create ---

#[test]
fn repo_create_writes_no_file() {
    let deployment = Deployment::new();
    deployment.credential("u272-token", Some("alice"));
    deployment.serves("POST", "/api/v1/repos", 201, bodies::CREATED_REPO);

    let output = deployment.run(&["repo", "create", "notes", "--json"]);

    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    let document = document(&output);
    assert_eq!(document, served(bodies::CREATED_REPO));
    assert_eq!(document["status"], json!("draft"));
    assert_eq!(document["visibility"], json!("private"));
    assert_eq!(
        std::fs::read_dir(deployment.work.path()).unwrap().count(),
        0,
        "the directory holds no file afterwards"
    );
}

// --- the noun's own options, read by the arm it routes to ---

// CR1-1: `--if-repo` written ahead of the subcommand word is the
// noun's own flag, and the arm it routes to must read it — the skip
// envelope at exit `0` is what the flag guarantees wherever it stands.
#[test]
fn history_show_skips_under_the_nouns_own_if_repo() {
    let deployment = Deployment::new();

    let output = deployment.run(&["history", "--if-repo", "show", "2", "--json"]);

    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    assert_eq!(
        document(&output),
        json!({"skipped": true, "reason": "no_syns_repo"})
    );
    assert!(deployment.paths().is_empty(), "no request was made");
}

// CR1-4: the role change now reaches its verb through the noun's own
// routing alone, so the flag the noun carries still reaches it.
#[test]
fn collaborators_role_skips_under_the_nouns_own_if_repo() {
    let deployment = Deployment::new();
    deployment.credential("u272-token", Some("alice"));

    let output = deployment.run(&[
        "collaborators",
        "--if-repo",
        "role",
        "u272user1111111111111111111111111",
        "--role",
        "write",
        "--json",
    ]);

    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    assert_eq!(
        document(&output),
        json!({"skipped": true, "reason": "no_syns_repo"})
    );
    assert!(deployment.paths().is_empty(), "no request was made");
}

// CR1-2: the noun's update options cannot reach `EP-create-repo`, so a
// run naming one beside `create` is refused before any request rather
// than served a repository at the entry's own defaults, which `INV-12`
// then answers `CONFLICT` on the corrective repeat.
#[test]
fn repo_create_refuses_the_nouns_update_options() {
    let deployment = Deployment::new();
    deployment.credential("u272-token", Some("alice"));
    deployment.serves("POST", "/api/v1/repos", 201, bodies::CREATED_REPO);

    for option in [
        vec!["--visibility", "public"],
        vec!["--description", "d"],
        vec!["--status", "active"],
        vec!["--tag", "t"],
    ] {
        let mut args = vec!["repo"];
        args.extend(option.iter());
        args.extend(["create", "notes", "--json"]);
        let output = deployment.run(&args);

        assert_eq!(output.status.code(), Some(1), "{}", stderr(&output));
        assert_eq!(
            document(&output)["error"],
            json!(
                "configuration error: --description and --visibility belong after create; --status and --tag change a standing repository and reach the create nowhere"
            )
        );
    }

    // CR2-1: the move that refusal names exists for two of the four.
    // `RepoAction::Create` declares `--description` and `--visibility`
    // alone, so a caller who moved `--status` or `--tag` after the
    // subcommand word would meet the argument parser at exit `2`, which
    // is why the refusal tells those two they reach the create nowhere.
    for option in [vec!["--status", "active"], vec!["--tag", "t"]] {
        let mut args = vec!["repo", "create", "notes"];
        args.extend(option.iter());
        let output = deployment.run(&args);

        assert_eq!(output.status.code(), Some(2), "{}", stderr(&output));
    }
    assert!(deployment.paths().is_empty(), "no request was made");
}

// --- the registered invocation set ---

/// The invocation spellings the argument tree is to register: the set
/// `CLI_IA.md` § Command Inventory carries, with the ten this unit adds
/// standing beside them. `teams role` stands in it, `repos` and
/// `repo list` both stand, and nothing else does.
const REGISTERED: &[(&str, &[&str])] = &[
    (
        "",
        &[
            "push",
            "pull",
            "sync",
            "resolution",
            "ls",
            "cat",
            "read",
            "glob",
            "grep",
            "edit",
            "write",
            "rm",
            "commit",
            "status",
            "history",
            "diff",
            "revert",
            "repo",
            "repos",
            "collaborators",
            "delete",
            "explore",
            "fork",
            "forks",
            "users",
            "user",
            "links",
            "teams",
            "upgrade",
            "login",
            "logout",
            "whoami",
        ],
    ),
    ("resolution", &["show", "continue", "discard"]),
    ("history", &["show"]),
    ("repo", &["list", "create"]),
    ("collaborators", &["add", "role", "remove"]),
    ("links", &["add", "update", "remove", "reorder"]),
    (
        "teams",
        &[
            "create",
            "show",
            "update",
            "delete",
            "members",
            "invite",
            "invitations",
            "accept",
            "decline",
            "role",
            "remove",
            "add-repo",
            "remove-repo",
            "repos",
        ],
    ),
];

/// The subcommand names a usage block lists, `help` — which clap adds
/// to every noun carrying subcommands and no inventory registers —
/// left out.
fn subcommands_in(usage: &str) -> Vec<String> {
    let mut names = Vec::new();
    let mut inside = false;
    for line in usage.lines() {
        if line.starts_with("Commands:") {
            inside = true;
            continue;
        }
        if inside {
            if line.trim().is_empty() {
                break;
            }
            if let Some(name) = line.split_whitespace().next()
                && name != "help"
            {
                names.push(name.to_string());
            }
        }
    }
    names
}

#[test]
fn the_registered_invocations_match_their_spellings() {
    let deployment = Deployment::new();
    for (noun, expected) in REGISTERED {
        let args: Vec<&str> = if noun.is_empty() {
            vec!["--help"]
        } else {
            vec![noun, "--help"]
        };
        let output = deployment.run(&args);
        assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
        let listed = subcommands_in(&stdout(&output));
        assert_eq!(
            listed,
            expected.iter().map(|s| s.to_string()).collect::<Vec<_>>(),
            "the usage text of `syns {noun}` lists exactly the registered spellings"
        );
    }
    // `syns teams role NAME MEMBER` is the spelling that stands
    // (`issues/003-cli-subcommand-name-mismatch`), and no `set-role`
    // stands beside it.
    let teams = stdout(&deployment.run(&["teams", "--help"]));
    assert!(!teams.contains("set-role"), "{teams}");
}
