//! A forced or path-scoped `syns push` from a repository root stands on
//! the working copy's recorded base, not on the repository-wide local
//! record (issue 157, `D-162`).
//!
//! The local record is one file per repository under a cache and no
//! convergence writes it, so a converged folder — or a folder sharing
//! the cache with another working copy — holds a record naming a commit
//! this folder does not stand on. Each test drives one mock deployment
//! of its own whose publication endpoint accepts only the parent the
//! server's head check would: a claim of any other parent is answered
//! `409`, as `EP-push` answers it.

use assert_cmd::Command as AssertCommand;
use serde_json::{Value, json};
use serial_test::serial;
use std::collections::HashMap;
use std::path::Path;
use syns_cli::config::StoreRoots;
use syns_cli::push::collector::{CollectOptions, HeldBytes, collect_files};
use syns_cli::push::manifest::Manifest;
use syns_cli::push::working_copy::WorkingCopy;
use tempfile::TempDir;
use wiremock::matchers::{body_partial_json, method, path as path_matcher};
use wiremock::{Mock, MockServer, Request, ResponseTemplate};

const OWNER: &str = "alice";
const NAME: &str = "notes";
/// The head the deployment stands at.
const HEAD_SHA: &str = "aa11bb22cc33dd44ee55ff6600778899001122bb";
/// A commit behind the head.
const OLD_SHA: &str = "bb00bb00bb00bb00bb00bb00bb00bb00bb00bb00";
/// The commit a landed publication is answered with.
const NEXT_SHA: &str = "dd44ee55ff6600778899001122bbaa11bb22cc33";

struct Deployment {
    rt: tokio::runtime::Runtime,
    server: MockServer,
    home: TempDir,
    cache: TempDir,
}

impl Deployment {
    fn new() -> Deployment {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime");
        let server = rt.block_on(MockServer::start());
        let home = tempfile::tempdir().expect("config dir");
        std::fs::write(
            home.path().join("credentials.json"),
            json!({"token": "test-token", "username": OWNER}).to_string(),
        )
        .expect("credential");
        Deployment {
            rt,
            server,
            home,
            cache: tempfile::tempdir().expect("cache dir"),
        }
    }

    fn stores(&self) -> StoreRoots {
        StoreRoots::resolve(
            Some(self.cache.path()),
            self.cache.path(),
            self.cache.path(),
        )
    }

    /// `EP-push` with its head check: a body claiming `HEAD_SHA`, or a
    /// forced body claiming none, lands at `NEXT_SHA`; any other claimed
    /// parent is refused `409` naming the head.
    fn mount_push(&self) {
        self.mount_push_at(HEAD_SHA, NEXT_SHA);
    }

    /// `EP-push` standing at `head`, landing a publication at `landed_at`;
    /// every mock and request mounted before is dropped first.
    fn mount_push_at(&self, head: &str, landed_at: &str) {
        let route = format!("/api/v1/repos/{OWNER}/{NAME}/push");
        let landed = ResponseTemplate::new(200).set_body_json(json!({
            "commitSha": landed_at, "version": 9, "filesChanged": 1, "created": false,
        }));
        self.rt.block_on(async {
            self.server.reset().await;
            Mock::given(method("PUT"))
                .and(path_matcher(route.clone()))
                .and(body_partial_json(json!({"parentSha": head})))
                .respond_with(landed.clone())
                .with_priority(1)
                .mount(&self.server)
                .await;
            Mock::given(method("PUT"))
                .and(path_matcher(route.clone()))
                .and(|request: &Request| {
                    serde_json::from_slice::<Value>(&request.body)
                        .map(|body| body.get("parentSha").is_none())
                        .unwrap_or(false)
                })
                .respond_with(landed)
                .with_priority(2)
                .mount(&self.server)
                .await;
            Mock::given(method("PUT"))
                .and(path_matcher(route))
                .respond_with(ResponseTemplate::new(409).set_body_json(json!({
                    "error": "conflict", "message": "Head mismatch", "currentSha": head,
                })))
                .with_priority(3)
                .mount(&self.server)
                .await;
        });
    }

    /// Every push body this deployment received, in order.
    fn pushes(&self) -> Vec<Value> {
        self.rt
            .block_on(async { self.server.received_requests().await.expect("requests") })
            .iter()
            .filter(|r| r.url.path().ends_with("/push"))
            .map(|r| serde_json::from_slice(&r.body).expect("push body"))
            .collect()
    }

    fn run(&self, cwd: &Path, args: &[&str]) -> std::process::Output {
        AssertCommand::cargo_bin("syns")
            .expect("syns binary")
            .current_dir(cwd)
            .env("SYNS_CONFIG_DIR", self.home.path())
            .env("SYNS_CACHE_DIR", self.cache.path())
            .env_remove("SYNS_URL")
            .env_remove("SYNS_INTEGRATION")
            .env_remove("SYNS_RUN")
            .env_remove("SYNS_TRIGGER")
            .env_remove("SYNS_TASK")
            .arg("--server")
            .arg(self.server.uri())
            .args(args)
            .output()
            .expect("run syns")
    }

    fn copy(&self, root: &Path) -> WorkingCopy {
        WorkingCopy::open(&self.stores(), OWNER, NAME, root).expect("working copy")
    }

    /// The repository-wide local record, written as a publication from
    /// `root` writes it.
    fn save_record(&self, root: &Path, commit: &str, files: HashMap<String, String>) {
        let mut record = Manifest::default();
        record.update(commit.to_string(), files);
        record
            .save(&self.stores(), OWNER, NAME, root)
            .expect("local record");
    }

    fn record(&self, root: &Path) -> Manifest {
        Manifest::load(&self.stores(), OWNER, NAME, root).expect("a local record")
    }
}

/// A folder tracking `alice/notes`, holding `a.md` and `sub/s.md`.
fn checkout() -> TempDir {
    let dir = tempfile::tempdir().expect("working dir");
    let root = dir.path();
    std::fs::write(
        root.join(".syns.yaml"),
        format!("owner: {OWNER}\nname: {NAME}\n"),
    )
    .expect("identity file");
    std::fs::write(root.join("a.md"), "a\n").expect("a file");
    std::fs::create_dir_all(root.join("sub")).expect("a subfolder");
    std::fs::write(root.join("sub/s.md"), "seed s\n").expect("a scoped file");
    dir
}

fn hashes(root: &Path) -> HashMap<String, String> {
    collect_files(
        root,
        &[],
        CollectOptions::default(),
        None,
        &HeldBytes::new(0),
    )
    .expect("collected")
    .hashes()
}

fn stderr_of(output: &std::process::Output) -> String {
    String::from_utf8_lossy(&output.stderr).to_string()
}

fn exit_of(output: &std::process::Output) -> i32 {
    output.status.code().expect("an exit code")
}

/// Issue 157 Observed, step 3: a folder `syns sync` converged to the
/// head — its recorded base at `HEAD_SHA`, holding `other.md` the
/// convergence brought — beside a local record still naming the commit
/// before. A path-scoped publication claims the base's commit and lands,
/// and the record written afterwards is that base with the publication
/// laid over it.
#[test]
#[serial]
fn a_scoped_push_from_a_converged_folder_claims_the_base_commit() {
    let d = Deployment::new();
    d.mount_push();
    let work = checkout();
    let root = work.path();
    d.save_record(root, OLD_SHA, hashes(root));
    std::fs::write(root.join("other.md"), "arrived through sync\n").expect("synced file");
    d.copy(root)
        .record_base(HEAD_SHA, hashes(root))
        .expect("recorded base");

    std::fs::write(root.join("sub/s.md"), "seed s\nW edits the scoped file\n").expect("edit");
    let out = d.run(root, &["push", "sub", "-m", "i157 scoped"]);

    let pushes = d.pushes();
    assert_eq!(pushes.len(), 1, "{}", stderr_of(&out));
    assert_eq!(pushes[0]["parentSha"], json!(HEAD_SHA), "{}", pushes[0]);
    assert_eq!(exit_of(&out), 0, "{}", stderr_of(&out));

    let record = d.record(root);
    assert_eq!(record.commit_sha(), Some(NEXT_SHA));
    assert!(
        record.file_sha("other.md").is_some(),
        "the record is the base laid over, not the stale record"
    );
    let base = d.copy(root).base().expect("a recorded base");
    assert_eq!(
        base.commit_sha(),
        Some(NEXT_SHA),
        "the publication is laid over the base it claimed"
    );
}

/// Issue 157 Observed, step 8 (the loss arm): two working copies of one
/// repository under one cache directory. The other copy published last,
/// so the shared local record names the head; this copy's base is behind
/// it. A path-scoped publication from this copy claims its own base and
/// is refused by the head check, rather than claiming the other copy's
/// commit and overwriting its line at exit 0.
#[test]
#[serial]
fn a_scoped_push_claims_this_copys_base_not_a_record_shared_with_another_copy() {
    let d = Deployment::new();
    d.mount_push();
    let mine = checkout();
    let theirs = checkout();
    d.copy(mine.path())
        .record_base(OLD_SHA, hashes(mine.path()))
        .expect("this copy's base");
    std::fs::write(theirs.path().join("sub/s.md"), "W2 rewrote it\n").expect("their edit");
    d.copy(theirs.path())
        .record_base(HEAD_SHA, hashes(theirs.path()))
        .expect("their base");
    d.save_record(theirs.path(), HEAD_SHA, hashes(theirs.path()));

    std::fs::write(mine.path().join("sub/s.md"), "W overwrites unseen\n").expect("my edit");
    let out = d.run(mine.path(), &["push", "sub", "-m", "i157 scoped overwrite"]);

    let pushes = d.pushes();
    assert_eq!(pushes.len(), 1, "{}", stderr_of(&out));
    assert_eq!(pushes[0]["parentSha"], json!(OLD_SHA), "{}", pushes[0]);
    assert_eq!(
        exit_of(&out),
        1,
        "the head check refuses the claim: {}",
        stderr_of(&out)
    );
    assert!(stderr_of(&out).contains("409"), "{}", stderr_of(&out));
}

/// Issue 157 Observed, step 7: `syns push --force` from a converged
/// folder names the parent it drops — the base's commit, which the same
/// run unforced would have claimed — on the diagnostic stream and as
/// `unclaimedParent` under `--json`.
#[test]
#[serial]
fn a_forced_push_names_the_base_commit_it_did_not_claim() {
    let d = Deployment::new();
    d.mount_push();
    let work = checkout();
    let root = work.path();
    let seed = || {
        d.save_record(root, OLD_SHA, hashes(root));
        d.copy(root)
            .record_base(HEAD_SHA, hashes(root))
            .expect("recorded base");
    };

    seed();
    let human = d.run(root, &["push", "--force", "-m", "i157 forced"]);
    assert_eq!(exit_of(&human), 0, "{}", stderr_of(&human));
    let line = stderr_of(&human);
    assert!(line.contains("the head check did not run"), "{line}");
    assert!(line.contains(HEAD_SHA), "{line}");
    assert!(!line.contains(OLD_SHA), "{line}");

    seed();
    let machine = d.run(root, &["--json", "push", "--force", "-m", "i157 forced"]);
    assert_eq!(exit_of(&machine), 0, "{}", stderr_of(&machine));
    let document: Value =
        serde_json::from_slice(&machine.stdout).expect("one document on the primary stream");
    assert_eq!(document["unclaimedParent"], json!(HEAD_SHA), "{document}");

    for push in d.pushes() {
        assert!(
            push.get("parentSha").is_none(),
            "--force claims none: {push}"
        );
    }
}

/// With no recorded base the publication stands as before: the local
/// record names the parent.
#[test]
#[serial]
fn a_scoped_push_with_no_base_claims_the_local_records_commit() {
    let d = Deployment::new();
    d.mount_push();
    let work = checkout();
    let root = work.path();
    d.save_record(root, HEAD_SHA, hashes(root));
    assert!(d.copy(root).base().is_none(), "no base recorded");

    std::fs::write(root.join("sub/s.md"), "seed s\nedited\n").expect("edit");
    let out = d.run(root, &["push", "sub", "-m", "i157 no base"]);

    assert_eq!(exit_of(&out), 0, "{}", stderr_of(&out));
    let pushes = d.pushes();
    assert_eq!(pushes.len(), 1);
    assert_eq!(pushes[0]["parentSha"], json!(HEAD_SHA), "{}", pushes[0]);
    assert_eq!(d.record(root).commit_sha(), Some(NEXT_SHA));
}

/// The paths a push body names as deletions.
fn deletions(push: &Value) -> Vec<String> {
    push.get("deletions")
        .and_then(Value::as_array)
        .map(|entries| {
            entries
                .iter()
                .filter_map(|entry| entry["path"].as_str().map(String::from))
                .collect()
        })
        .unwrap_or_default()
}

/// A converged folder whose base, at `HEAD_SHA`, also records two files
/// standing on disk under `sub/` that this folder's collection leaves
/// out: `sub/build/out.md` under the default skip list, and
/// `sub/private.md` under the folder's `.synsignore` — as a base does
/// after a convergence with the head another copy published them to.
fn converged_with_excluded_files(d: &Deployment, root: &Path) {
    std::fs::write(root.join(".synsignore"), "sub/private.md\n").expect("ignore file");
    let mut base = hashes(root);
    std::fs::create_dir_all(root.join("sub/build")).expect("a skipped folder");
    std::fs::write(root.join("sub/build/out.md"), "built\n").expect("a skipped file");
    std::fs::write(root.join("sub/private.md"), "private\n").expect("an ignored file");
    base.insert("sub/build/out.md".into(), "11".repeat(20));
    base.insert("sub/private.md".into(), "22".repeat(20));
    d.copy(root)
        .record_base(HEAD_SHA, base)
        .expect("recorded base");
}

/// `D-162`, ruled on review (I-2): the base stands in with the paths
/// excluded on disk taken out, so `syns push sub` names none of the
/// files the folder holds but does not collect as a deletion.
#[test]
#[serial]
fn a_scoped_push_names_no_file_excluded_on_disk_as_a_deletion() {
    let d = Deployment::new();
    d.mount_push();
    let work = checkout();
    let root = work.path();
    converged_with_excluded_files(&d, root);

    std::fs::write(root.join("sub/s.md"), "seed s\nedited\n").expect("edit");
    let out = d.run(root, &["push", "sub", "-m", "i157 excluded"]);

    assert_eq!(exit_of(&out), 0, "{}", stderr_of(&out));
    let pushes = d.pushes();
    assert_eq!(pushes.len(), 1, "{}", stderr_of(&out));
    assert_eq!(pushes[0]["parentSha"], json!(HEAD_SHA), "{}", pushes[0]);
    assert_eq!(deletions(&pushes[0]), Vec::<String>::new(), "{}", pushes[0]);
}

/// The same for `syns push .`, whose scope is the whole folder.
#[test]
#[serial]
fn a_push_of_the_root_names_no_file_excluded_on_disk_as_a_deletion() {
    let d = Deployment::new();
    d.mount_push();
    let work = checkout();
    let root = work.path();
    converged_with_excluded_files(&d, root);

    std::fs::write(root.join("a.md"), "a\nedited\n").expect("edit");
    let out = d.run(root, &["push", ".", "-m", "i157 excluded root"]);

    assert_eq!(exit_of(&out), 0, "{}", stderr_of(&out));
    let pushes = d.pushes();
    assert_eq!(pushes.len(), 1, "{}", stderr_of(&out));
    assert_eq!(pushes[0]["parentSha"], json!(HEAD_SHA), "{}", pushes[0]);
    assert_eq!(deletions(&pushes[0]), Vec::<String>::new(), "{}", pushes[0]);
}

/// A file removed from disk inside the scope is still a deletion: the
/// base loses only the paths standing on disk.
#[test]
#[serial]
fn a_scoped_push_still_deletes_a_base_file_removed_from_disk() {
    let d = Deployment::new();
    d.mount_push();
    let work = checkout();
    let root = work.path();
    converged_with_excluded_files(&d, root);

    std::fs::remove_file(root.join("sub/s.md")).expect("removal");
    let out = d.run(root, &["push", "sub", "-m", "i157 removal"]);

    assert_eq!(exit_of(&out), 0, "{}", stderr_of(&out));
    let pushes = d.pushes();
    assert_eq!(pushes.len(), 1, "{}", stderr_of(&out));
    assert_eq!(
        deletions(&pushes[0]),
        vec!["sub/s.md".to_string()],
        "{}",
        pushes[0]
    );
}

/// A commit the deployment lands a publication after the forced one at.
const LATER_SHA: &str = "ee55ff6600778899001122bbaa11bb22cc33dd44";

impl Deployment {
    /// `EP-tree` answering `files` at `commit`, at whatever ref is asked.
    fn mount_tree(&self, commit: &str, files: &HashMap<String, String>) {
        let entries: Vec<Value> = files
            .iter()
            .map(|(path, sha)| {
                json!({
                    "name": path.rsplit('/').next().unwrap_or(path),
                    "path": path, "type": "file", "size": 1, "sha": sha,
                })
            })
            .collect();
        self.rt.block_on(async {
            Mock::given(method("GET"))
                .and(path_matcher(format!("/api/v1/repos/{OWNER}/{NAME}/tree")))
                .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                    "entries": entries, "commitSha": commit, "truncated": false,
                })))
                .mount(&self.server)
                .await;
        });
    }
}

/// A converged folder — base and record at `HEAD_SHA` — publishing an
/// edit to `a.md` with `--force`, which lands at `NEXT_SHA`; `EP-tree`
/// answers `extra` beside the folder's own files at that commit.
fn forced_from_a_converged_folder(
    d: &Deployment,
    root: &Path,
    extra: &[(&str, &str)],
) -> serde_json::Value {
    d.save_record(root, HEAD_SHA, hashes(root));
    d.copy(root)
        .record_base(HEAD_SHA, hashes(root))
        .expect("recorded base");
    let before = serde_json::to_value(d.copy(root).base().expect("a base")).expect("base");
    std::fs::write(root.join("a.md"), "a\nforced\n").expect("edit");
    let mut landed = hashes(root);
    for (path, sha) in extra {
        landed.insert(path.to_string(), sha.to_string());
    }
    d.mount_tree(NEXT_SHA, &landed);
    let forced = d.run(root, &["push", "--force", "-m", "i157 forced"]);
    assert_eq!(exit_of(&forced), 0, "{}", stderr_of(&forced));
    before
}

/// `D-162`, ruled on review (VERIFY.md Finding 1): a single writer's
/// forced publication leaves a folder holding exactly the forced commit's
/// tree, so the base advances to it and the next `syns push sub` claims
/// the forced commit and lands. The record the forced run writes is the
/// base laid over at the forced commit.
#[test]
#[serial]
fn a_scoped_push_after_a_single_writers_force_claims_the_forced_commit() {
    let d = Deployment::new();
    d.mount_push();
    let work = checkout();
    let root = work.path();
    forced_from_a_converged_folder(&d, root, &[]);

    let base = d.copy(root).base().expect("a base");
    assert_eq!(
        base.commit_sha(),
        Some(NEXT_SHA),
        "the base adopts the forced commit"
    );
    let record = d.record(root);
    assert_eq!(record.commit_sha(), Some(NEXT_SHA));
    let mut named: Vec<&str> = record.file_paths().collect();
    named.sort();
    assert_eq!(
        named,
        vec![".syns.yaml", "a.md", "sub/s.md"],
        "base laid over"
    );
    assert_eq!(
        record.file_sha("a.md"),
        hashes(root).get("a.md").map(String::as_str)
    );

    d.mount_push_at(NEXT_SHA, LATER_SHA);
    std::fs::write(root.join("sub/s.md"), "seed s\nafter the force\n").expect("edit");
    let scoped = d.run(root, &["push", "sub", "-m", "i157 after force"]);

    let pushes = d.pushes();
    assert_eq!(pushes.len(), 1, "{}", stderr_of(&scoped));
    assert_eq!(pushes[0]["parentSha"], json!(NEXT_SHA), "{}", pushes[0]);
    assert_eq!(exit_of(&scoped), 0, "{}", stderr_of(&scoped));
}

/// The same single writer's bare `syns push` after `--force` publishes
/// over the forced commit and raises no resolution.
#[test]
#[serial]
fn a_bare_push_after_a_single_writers_force_raises_no_resolution() {
    let d = Deployment::new();
    d.mount_push();
    let work = checkout();
    let root = work.path();
    forced_from_a_converged_folder(&d, root, &[]);
    let at_force = hashes(root);

    d.mount_push_at(NEXT_SHA, LATER_SHA);
    d.mount_tree(NEXT_SHA, &at_force);
    std::fs::write(root.join("sub/s.md"), "seed s\nbare after the force\n").expect("edit");
    let bare = d.run(root, &["push", "-m", "i157 bare after force"]);

    assert_eq!(exit_of(&bare), 0, "{}", stderr_of(&bare));
    assert!(
        d.copy(root).resolution().expect("state").is_none(),
        "no resolution"
    );
    let pushes = d.pushes();
    assert_eq!(pushes.len(), 1, "{}", stderr_of(&bare));
    assert_eq!(pushes[0]["parentSha"], json!(NEXT_SHA), "{}", pushes[0]);
}

/// A forced commit holding another writer's file the folder lacks is
/// not the folder's tree: the base stays where it stood, so the next
/// publication is refused or resolved as `INV-59` requires.
#[test]
#[serial]
fn a_forced_commit_holding_another_writers_file_leaves_the_base() {
    let d = Deployment::new();
    d.mount_push();
    let work = checkout();
    let root = work.path();
    let before = forced_from_a_converged_folder(&d, root, &[("theirs.md", &"44".repeat(20))]);

    let after = serde_json::to_value(d.copy(root).base().expect("a base")).expect("base");
    assert_eq!(before, after, "the base stays where it stood");
}

/// A scoped publication from a folder whose base and record already
/// agree lays the publication over the base: the base advances to the
/// landed commit with the published bytes.
#[test]
#[serial]
fn a_scoped_push_advances_a_base_the_record_agreed_with() {
    let d = Deployment::new();
    d.mount_push();
    let work = checkout();
    let root = work.path();
    d.save_record(root, HEAD_SHA, hashes(root));
    d.copy(root)
        .record_base(HEAD_SHA, hashes(root))
        .expect("recorded base");

    std::fs::write(root.join("sub/s.md"), "seed s\nagreed\n").expect("edit");
    let out = d.run(root, &["push", "sub", "-m", "i157 agreed"]);
    assert_eq!(exit_of(&out), 0, "{}", stderr_of(&out));
    assert_eq!(d.pushes()[0]["parentSha"], json!(HEAD_SHA));

    let base = d.copy(root).base().expect("a recorded base");
    assert_eq!(base.commit_sha(), Some(NEXT_SHA));
    let now = hashes(root);
    for path in ["a.md", "sub/s.md", ".syns.yaml"] {
        assert_eq!(
            base.file_sha(path),
            now.get(path).map(String::as_str),
            "{path}"
        );
    }
    assert_eq!(base.file_paths().count(), now.len());
}
