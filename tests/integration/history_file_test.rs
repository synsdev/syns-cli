//! Binary-level runs of `syns history --file` against a per-path history
//! page holding the entry of a commit that removed the path (u261): its
//! `blobSha` and `content` are served as `null`, the page still decodes,
//! every row renders, and only the removal entry's `Message` cell carries
//! `(removed)`; under `--json` the served body passes through unmarked.

use assert_cmd::Command as AssertCommand;
use serde_json::{Value, json};
use serial_test::serial;
use wiremock::matchers::{method, path, query_param};
use wiremock::{Mock, MockServer, ResponseTemplate};

/// Issue 099's Observed page: versions 439, 436 and 435 of `CLAUDE.md`,
/// 436 being the commit that removed the path.
fn removal_page() -> Value {
    json!({
        "data": [
            {
                "version": 439,
                "sha": "739d8dc0c095de7ab390d685c0e2e8629d61db1f",
                "blobSha": "d54fa145810ef1ad6183d93229bb2982571cc3da",
                "message": "claude code session (part 3/3)",
                "author": "bartsoj",
                "createdAt": "2026-09-13T14:34:58Z",
                "content": "# syns",
                "diff": "--- /dev/null\n+++ b/CLAUDE.md\n@@ -0,0 +1 @@\n+# syns\n"
            },
            {
                "version": 436,
                "sha": "ff7f52cad5c73554fff96676478cd4b2a509fbdc",
                "blobSha": null,
                "message": "claude code session",
                "author": "bartsoj",
                "createdAt": "2026-09-13T14:31:20Z",
                "content": null,
                "diff": "--- a/CLAUDE.md\n+++ /dev/null\n@@ -1 +0,0 @@\n-# syns\n"
            },
            {
                "version": 435,
                "sha": "d1781077fe841cf6422624473c79f370ec24780c",
                "blobSha": "d54fa145810ef1ad6183d93229bb2982571cc3da",
                "message": "claude code session (part 3/3)",
                "author": "bartsoj",
                "createdAt": "2026-09-13T13:58:41Z",
                "content": "# syns",
                "diff": "--- /dev/null\n+++ b/CLAUDE.md\n@@ -0,0 +1 @@\n+# syns\n"
            }
        ],
        "total": 50,
        "limit": 3,
        "offset": 0
    })
}

/// Starts a mock serving the removal page, writes an identity file naming
/// `alice/my-project` into a tempdir with no credentials, runs the `syns`
/// binary there with `args`, and returns the process output.
fn run_against_removal_page(args: &[&str]) -> std::process::Output {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let mock_server = rt.block_on(async {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path(
                "/api/v1/repos/alice/my-project/files/CLAUDE.md/history",
            ))
            .and(query_param("limit", "3"))
            .respond_with(ResponseTemplate::new(200).set_body_string(removal_page().to_string()))
            .mount(&server)
            .await;
        server
    });

    let dir = tempfile::tempdir().expect("tempdir");
    std::fs::write(
        dir.path().join(".syns.yaml"),
        "owner: alice\nname: my-project\n",
    )
    .expect("write .syns.yaml");

    AssertCommand::cargo_bin("syns")
        .expect("syns binary")
        .current_dir(dir.path())
        .env("SYNS_CONFIG_DIR", dir.path())
        .env_remove("SYNS_URL")
        .arg("--server")
        .arg(mock_server.uri())
        .args(args)
        .output()
        .expect("run syns")
}

#[test]
#[serial]
fn history_file_prints_every_row_of_a_page_holding_a_removal_entry() {
    let output = run_against_removal_page(&["history", "--file", "CLAUDE.md", "--limit", "3"]);
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);

    assert_eq!(
        output.status.code(),
        Some(0),
        "stdout: {stdout}\nstderr: {stderr}"
    );
    assert!(
        !stderr.contains("invalid response body"),
        "stderr: {stderr}"
    );

    let position = |needle: &str| {
        stdout
            .find(needle)
            .unwrap_or_else(|| panic!("stdout lacks {needle}: {stdout}"))
    };
    assert!(
        position("739d8dc0") < position("ff7f52ca"),
        "stdout: {stdout}"
    );
    assert!(
        position("ff7f52ca") < position("d1781077"),
        "stdout: {stdout}"
    );

    let line_holding = |needle: &str| {
        stdout
            .lines()
            .find(|line| line.contains(needle))
            .unwrap_or_else(|| panic!("no line holds {needle}: {stdout}"))
    };
    assert!(
        line_holding("ff7f52ca").contains("(removed)"),
        "stdout: {stdout}"
    );
    assert!(
        !line_holding("739d8dc0").contains("(removed)"),
        "stdout: {stdout}"
    );
    assert!(
        !line_holding("d1781077").contains("(removed)"),
        "stdout: {stdout}"
    );
}

#[test]
#[serial]
fn history_file_json_passes_a_removal_entry_through() {
    let output =
        run_against_removal_page(&["--json", "history", "--file", "CLAUDE.md", "--limit", "3"]);
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);

    assert_eq!(
        output.status.code(),
        Some(0),
        "stdout: {stdout}\nstderr: {stderr}"
    );
    assert!(!stdout.contains("(removed)"), "stdout: {stdout}");

    let parsed: Value = serde_json::from_str(&stdout)
        .unwrap_or_else(|e| panic!("stdout is not JSON ({e}): {stdout}"));
    let served = removal_page();
    assert_eq!(parsed, served);
    assert!(parsed["data"][1]["blobSha"].is_null());
    assert!(parsed["data"][1]["content"].is_null());
    assert_eq!(parsed["data"][1]["diff"], served["data"][1]["diff"]);
}
