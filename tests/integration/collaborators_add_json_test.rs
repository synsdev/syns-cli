//! Integration test for the `--json` envelope shape of
//! `syns collaborators add` (u215 SPEC §5 D7 — invariant guard for
//! the `"target"` key rename from `"userId"`).
//!
//! Closes the residual M4 gap flagged in
//! `units/cli/u215/CODE_REVIEW_R2.md`: the in-process unit test
//! `collaborators_add_json_mode_emits_target_key` cannot observe the
//! literal envelope content emitted by `output.json(...)` (it only
//! reaches `result.is_ok()` plus a hand-built `format_json`
//! round-trip). This subprocess test captures the actual stdout of
//! `syns --json collaborators add bartad498 --role write` against a
//! wiremock fixture and asserts the parsed JSON is exactly
//! `{"added": true, "target": "bartad498", "role": "write"}`.
//!
//! Invariants protected:
//!   - The envelope's top-level keys are exactly `added`, `target`,
//!     `role` — a regression to the pre-u215 `"userId"` key surfaces
//!     here as a key-set mismatch.
//!   - The `"target"` value is the user-typed string (`"bartad498"`).
//!     u215 dropped the explicit opaque-`userId` path from the CLI, so
//!     the CLI never holds a resolved `users.id` to emit — but a
//!     future refactor that re-introduced one (and accidentally
//!     emitted it under the `target` key) would surface here.
//!
//! The subprocess pattern follows `if_repo_test.rs` —
//! `assert_cmd::Command::cargo_bin("syns")` with `SYNS_URL`,
//! `SYNS_CONFIG_DIR`, and `current_dir` injected, then
//! `serde_json::from_slice` over the captured stdout.

use assert_cmd::Command as AssertCommand;
use serde_json::Value;
use serial_test::serial;
use syns_cli::auth::token::TokenStore;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

#[test]
#[serial]
fn syns_json_collaborators_add_emits_target_key_with_user_typed_value() {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();

    // Spin up the mock server first; the returned MockServer manages
    // its own worker thread, so it stays serving requests across the
    // block_on boundary while the subprocess runs synchronously below.
    let mock_server = rt.block_on(async {
        let server = MockServer::start().await;

        // POST on the resolved-from-`.syns.yaml` repo path returns 201
        // with a minimal Collaborator-shaped body. The body content is
        // discarded by `SynsClient::add_collaborator` (which calls
        // `process_empty_response`), so any 2xx status would work; the
        // shape mirrors the production wire contract for fidelity.
        Mock::given(method("POST"))
            .and(path("/api/v1/repos/alice/my-project/collaborators"))
            .respond_with(ResponseTemplate::new(201).set_body_json(serde_json::json!({
                "user": {
                    "id": "u_bartad498_001",
                    "username": "bartad498",
                    "name": "Bart",
                    "email": "bartad498@example.com",
                    "image": null
                },
                "role": "write",
                "addedAt": "2026-05-14T10:00:00Z"
            })))
            .mount(&server)
            .await;

        server
    });
    let mock_uri = mock_server.uri();

    // Set up the project directory: `.syns.yaml` is needed so the
    // resolver `resolve_full_or_skip` can derive (`alice`, `my-project`)
    // from the cwd. Credentials live in the same dir, which doubles as
    // SYNS_CONFIG_DIR to mirror the in-process test convention.
    let dir = tempfile::tempdir().expect("tempdir");
    std::fs::write(
        dir.path().join(".syns.yaml"),
        "owner: alice\nname: my-project\n",
    )
    .expect("write .syns.yaml");
    TokenStore::new(dir.path().join("credentials.json"))
        .write("test-token")
        .expect("write credentials");

    let assert = AssertCommand::cargo_bin("syns")
        .expect("syns binary")
        .current_dir(dir.path())
        .env("SYNS_URL", &mock_uri)
        .env("SYNS_CONFIG_DIR", dir.path())
        .args([
            "--json",
            "collaborators",
            "add",
            "bartad498",
            "--role",
            "write",
        ])
        .assert()
        .success();

    let output = assert.get_output();
    let parsed: Value = serde_json::from_slice(&output.stdout).unwrap_or_else(|e| {
        panic!(
            "expected stdout to be valid JSON, but parse failed: {e}\n\
             raw stdout: {}",
            String::from_utf8_lossy(&output.stdout)
        )
    });

    let obj = parsed
        .as_object()
        .expect("expected top-level JSON object, got non-object");

    // SPEC §5 D7: the top-level keys MUST be exactly {"added",
    // "target", "role"} — any extra `"userId"` (or other) key would
    // surface as a key-set mismatch here.
    let mut keys: Vec<&str> = obj.keys().map(String::as_str).collect();
    keys.sort();
    assert_eq!(
        keys,
        vec!["added", "role", "target"],
        "expected exactly three top-level keys (`added`, `role`, \
         `target`) — a regression to `userId` or extra fields would \
         surface here. got: {keys:?}; full payload: {parsed}"
    );

    // Defensive: explicit `userId` absence assertion. Redundant with
    // the strict key-set above when present, but valuable because it
    // names the regression in the failure message that future
    // maintainers see first.
    assert!(
        obj.get("userId").is_none(),
        "SPEC §5 D7 violation: `userId` MUST NOT appear in the \
         success envelope (the pre-u215 key was renamed to `target`). \
         got: {parsed}"
    );

    assert_eq!(
        obj.get("added"),
        Some(&Value::Bool(true)),
        "expected `added: true`; got: {parsed}"
    );

    // The defining D7 invariant: the rendered identifier is the
    // user-typed string. u215 deliberately dropped the opaque-userId
    // path from the CLI, so the CLI never holds a resolved id; this
    // pins that no future refactor leaks one back into the envelope.
    assert_eq!(
        obj.get("target"),
        Some(&Value::String("bartad498".to_string())),
        "SPEC §5 D7 violation: `target` MUST render the user-typed \
         identifier (`bartad498`). got: {parsed}"
    );

    assert_eq!(
        obj.get("role"),
        Some(&Value::String("write".to_string())),
        "expected `role: \"write\"`; got: {parsed}"
    );

    // Keep the mock server alive until after the subprocess exits.
    drop(mock_server);
}
