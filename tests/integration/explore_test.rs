//! Binary-level behaviour of `syns explore`'s filters (SPEC u294 Tests):
//! every option given on the command line reaches `EP-explore` under the
//! key the endpoint registers for it.

use assert_cmd::Command as AssertCommand;
use serde_json::{Value, json};
use serial_test::serial;
use std::collections::BTreeSet;
use tempfile::TempDir;
use wiremock::matchers::{method, path as path_matcher};
use wiremock::{Mock, MockServer, Request, ResponseTemplate};

/// One mock deployment and one config directory holding no credential,
/// both derived from the test that made them.
struct Deployment {
    rt: tokio::runtime::Runtime,
    server: MockServer,
    home: TempDir,
    work: TempDir,
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
            work: tempfile::tempdir().expect("working dir"),
        }
    }

    fn mount(&self, mock: Mock) {
        self.rt.block_on(async { mock.mount(&self.server).await });
    }

    fn requests(&self) -> Vec<Request> {
        self.rt
            .block_on(async { self.server.received_requests().await.expect("requests") })
    }

    fn run(&self, args: &[&str]) -> std::process::Output {
        AssertCommand::cargo_bin("syns")
            .expect("syns binary")
            .current_dir(self.work.path())
            .env("SYNS_CONFIG_DIR", self.home.path())
            .env_remove("SYNS_URL")
            .arg("--server")
            .arg(self.server.uri())
            .args(args)
            .output()
            .expect("run syns")
    }
}

fn stdout_of(output: &std::process::Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn stderr_of(output: &std::process::Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

fn document(output: &std::process::Output) -> Value {
    serde_json::from_str(&stdout_of(output)).unwrap_or_else(|e| {
        panic!("stdout is no one document ({e}): {}", stdout_of(output));
    })
}

/// A request's decoded query pairs, compared as a set so the order they
/// are sent in decides nothing.
fn pairs_of(request: &Request) -> BTreeSet<(String, String)> {
    request
        .url
        .query_pairs()
        .map(|(k, v)| (k.into_owned(), v.into_owned()))
        .collect()
}

#[test]
#[serial]
fn explore_flags_from_the_command_line_reach_the_endpoint() {
    let d = Deployment::new();
    d.mount(
        Mock::given(method("GET"))
            .and(path_matcher("/api/v1/explore"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "data": [{
                    "owner": "alice", "name": "project-a",
                    "description": "First project", "commitSha": "abc123",
                    "status": "active", "author": null, "tags": ["syns-app", "cli"],
                    "visibility": "public", "forkedFrom": null, "forkCount": 2,
                    "fileCount": 10, "role": null,
                    "createdAt": "2025-01-01T00:00:00Z",
                    "updatedAt": "2025-06-01T00:00:00Z",
                }],
                "total": 1,
                "limit": 5,
                "offset": 10,
            }))),
    );

    let output = d.run(&[
        "explore", "-q", "tmpl", "-t", "syns-app", "-t", "cli", "--status", "active", "--limit",
        "5", "--offset", "10", "--json",
    ]);

    assert_eq!(output.status.code(), Some(0), "{}", stderr_of(&output));
    assert_eq!(document(&output)["data"][0]["name"], json!("project-a"));

    let requests = d.requests();
    assert_eq!(requests.len(), 1, "exactly one request is sent");
    let expected: BTreeSet<(String, String)> = [
        ("q", "tmpl"),
        ("tags", "syns-app,cli"),
        ("status", "active"),
        ("limit", "5"),
        ("offset", "10"),
    ]
    .iter()
    .map(|(k, v)| (k.to_string(), v.to_string()))
    .collect();
    assert_eq!(pairs_of(&requests[0]), expected);
}
