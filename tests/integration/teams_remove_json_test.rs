//! Integration test for the `--json` envelope shape of
//! `syns teams remove` (u214 SPEC §5 D4 — invariant guard for the
//! `"member"` key rename from `"userId"`).
//!
//! Closes the residual M1 gap flagged in
//! `units/cli/u214/CODE_REVIEW_R2.md`: the in-process unit tests at
//! `src/commands/teams.rs` reach `result.is_ok()` plus a path-segment
//! recording on the wiremock server, but they cannot observe the
//! literal envelope content emitted via `println!`. This subprocess
//! test captures the actual stdout of
//! `syns --json teams remove alpha bob --yes` against a wiremock
//! fixture and asserts the parsed JSON is exactly
//! `{"removed": true, "member": "bob"}`.
//!
//! Invariants protected:
//!   - The envelope's top-level keys are exactly `removed` and
//!     `member` — a regression to the pre-u214 `"userId"` key surfaces
//!     here as a key-set mismatch.
//!   - The `"member"` value is the user-typed string (`"bob"`), NOT
//!     the resolver's resolved better-auth UUID (`"u_bob_001"`) — a
//!     regression where the resolved UUID leaks into the user-facing
//!     JSON would render `"member": "u_bob_001"` and fail.
//!
//! The subprocess pattern follows `if_repo_test.rs` —
//! `assert_cmd::Command::cargo_bin("syns")` with `SYNS_URL` and
//! `SYNS_CONFIG_DIR` injected, then `serde_json::from_slice` over
//! the captured stdout.

use assert_cmd::Command as AssertCommand;
use serde_json::Value;
use serial_test::serial;
use syns_cli::auth::token::TokenStore;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

#[test]
#[serial]
fn syns_json_teams_remove_emits_member_key_with_user_typed_value() {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();

    // Spin up the mock server first; the returned MockServer manages
    // its own worker thread, so it stays serving requests across the
    // block_on boundary while the subprocess runs synchronously below.
    let mock_server = rt.block_on(async {
        let server = MockServer::start().await;

        // Mock 1: GET /api/v1/teams — used by `resolve_team_id` to map
        // the user-typed team name "alpha" to its UUID `uuid-alpha`.
        Mock::given(method("GET"))
            .and(path("/api/v1/teams"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "data": [{
                    "id": "uuid-alpha",
                    "name": "alpha",
                    "description": null,
                    "owner": {
                        "id": "u_alice_002",
                        "username": "alice",
                        "name": "Alice",
                        "image": null
                    },
                    "memberCount": 2,
                    "role": "owner",
                    "createdAt": "2026-03-01T10:00:00Z",
                    "updatedAt": "2026-04-01T10:00:00Z"
                }]
            })))
            .mount(&server)
            .await;

        // Mock 2: GET /api/v1/teams/uuid-alpha/members — used by
        // `resolve_member_user_id` to map the user-typed "bob" to
        // the better-auth user id `u_bob_001`.
        Mock::given(method("GET"))
            .and(path("/api/v1/teams/uuid-alpha/members"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "data": [
                    {
                        "user": {
                            "id": "u_bob_001",
                            "username": "bob",
                            "name": "Bob",
                            "email": "bob@example.com",
                            "image": null
                        },
                        "role": "member",
                        "joinedAt": "2026-04-01T10:00:00Z"
                    },
                    {
                        "user": {
                            "id": "u_alice_002",
                            "username": "alice",
                            "name": "Alice",
                            "email": "alice@example.com",
                            "image": null
                        },
                        "role": "owner",
                        "joinedAt": "2026-03-01T10:00:00Z"
                    }
                ]
            })))
            .mount(&server)
            .await;

        // Mock 3: DELETE on the resolved-UUID path returns 204
        // (the no-body contract of `SynsClient::remove_member`). The
        // explicit path segment `u_bob_001` proves the resolver fired
        // and supplied the resolved UUID rather than the raw "bob"
        // string; this is the load-bearing fact that lets the JSON
        // success branch fire.
        Mock::given(method("DELETE"))
            .and(path("/api/v1/teams/uuid-alpha/members/u_bob_001"))
            .respond_with(ResponseTemplate::new(204))
            .mount(&server)
            .await;

        server
    });
    let mock_uri = mock_server.uri();

    // Seed credentials at `<config_dir>/credentials.json` via the
    // library's TokenStore so the subprocess's required-token load
    // resolves cleanly (it would otherwise return `AuthRequired`).
    let config_dir = tempfile::tempdir().expect("tempdir");
    TokenStore::new(config_dir.path().join("credentials.json"))
        .write("test-token")
        .expect("write credentials");

    let assert = AssertCommand::cargo_bin("syns")
        .expect("syns binary")
        .env("SYNS_URL", &mock_uri)
        .env("SYNS_CONFIG_DIR", config_dir.path())
        .args(["--json", "teams", "remove", "alpha", "bob", "--yes"])
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

    // SPEC §5 D4: the top-level keys MUST be exactly {"removed",
    // "member"} — any extra `"userId"` (or other) key would surface
    // as a key-set mismatch here.
    let mut keys: Vec<&str> = obj.keys().map(String::as_str).collect();
    keys.sort();
    assert_eq!(
        keys,
        vec!["member", "removed"],
        "expected exactly two top-level keys (`member`, `removed`) — \
         a regression to `userId` or extra fields would surface here. \
         got: {keys:?}; full payload: {parsed}"
    );

    // Defensive: explicit `userId` absence assertion. Redundant with
    // the strict key-set above when present, but valuable because it
    // names the regression in the failure message that future
    // maintainers see first.
    assert!(
        obj.get("userId").is_none(),
        "SPEC §5 D4 violation: `userId` MUST NOT appear in the \
         success envelope (the pre-u214 key was renamed to `member`). \
         got: {parsed}"
    );

    assert_eq!(
        obj.get("removed"),
        Some(&Value::Bool(true)),
        "expected `removed: true`; got: {parsed}"
    );

    // The defining D4 invariant: the rendered identifier is the
    // user-typed string, NOT the resolver's resolved UUID.
    assert_eq!(
        obj.get("member"),
        Some(&Value::String("bob".to_string())),
        "SPEC §5 D4 violation: `member` MUST render the user-typed \
         identifier (`bob`), NOT the resolver-resolved UUID \
         (`u_bob_001`). got: {parsed}"
    );

    // Keep the mock server alive until after the subprocess exits.
    // wiremock manages its own worker thread, but holding the value
    // here makes the lifetime explicit for future readers.
    drop(mock_server);
}
