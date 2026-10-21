//! Roadmap 277 — what every request the binary sends carries, and how a
//! read is asked: the `User-Agent` naming the build and the caller
//! `SYNS_CALLER` names (X2), the install id (X3), both compressions
//! accepted and decoded (X4), and an unpinned read asking the record and
//! the newest version at once (X7).
//!
//! Every case drives the built binary against a wiremock deployment.

use std::ffi::OsStr;
use std::io::Write;
use std::path::Path;
use std::process::Output;
use std::time::{Duration, Instant};

use serde_json::json;
use serial_test::serial;
use syns_cli::push::hash::blob_sha1;
use tempfile::TempDir;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, Request, ResponseTemplate};

pub(crate) const HEAD: &str = "def4560000000000000000000000000000000000";
const BYTES: &[u8] = b"alpha\nbeta\n";

/// One wiremock deployment beside a configuration and a cache directory
/// of the binary's own.
pub(crate) struct Deployment {
    rt: tokio::runtime::Runtime,
    pub(crate) server: MockServer,
    pub(crate) config: TempDir,
    cache: TempDir,
}

impl Deployment {
    pub(crate) fn new() -> Deployment {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let server = rt.block_on(MockServer::start());
        Deployment {
            rt,
            server,
            config: tempfile::tempdir().unwrap(),
            cache: tempfile::tempdir().unwrap(),
        }
    }

    pub(crate) fn mount(&self, mock: Mock) {
        self.rt.block_on(mock.mount(&self.server));
    }

    pub(crate) fn requests(&self) -> Vec<Request> {
        // Roadmap 279: a server answering no v2 is asked v2 first, its `404`
        // sending the run down these v1 reads (D-243 §7): left out here.
        self.rt
            .block_on(self.server.received_requests())
            .unwrap_or_default()
            .into_iter()
            .filter(|r| !r.url.path().starts_with("/api/v2/"))
            .collect()
    }

    /// Run the binary in `cwd` with `config` as its configuration
    /// directory and `caller` as its `SYNS_CALLER`, unset where none.
    pub(crate) fn run_with(
        &self,
        cwd: &Path,
        config: &Path,
        caller: Option<&OsStr>,
        args: &[&OsStr],
    ) -> Output {
        let mut command = assert_cmd::Command::cargo_bin("syns").unwrap();
        command
            .current_dir(cwd)
            .env("SYNS_CONFIG_DIR", config)
            .env("SYNS_CACHE_DIR", self.cache.path())
            .env_remove("SYNS_URL")
            .env_remove("SYNS_CALLER")
            .arg("--server")
            .arg(self.server.uri())
            .args(args);
        if let Some(caller) = caller {
            command.env("SYNS_CALLER", caller);
        }
        command.output().unwrap()
    }

    pub(crate) fn run(&self, cwd: &Path, args: &[&str]) -> Output {
        let args: Vec<&OsStr> = args.iter().map(OsStr::new).collect();
        self.run_with(cwd, self.config.path(), None, &args)
    }

    /// The record of `alice/proj` at `HEAD`, the newest version `newest`
    /// at number 8, `HEAD` at number 7 by its hash, and `a.md`'s bytes.
    fn mount_read(&self, newest: &str) {
        self.mount(
            Mock::given(method("GET"))
                .and(path("/api/v1/repos/alice/proj"))
                .respond_with(ResponseTemplate::new(200).set_body_json(record())),
        );
        self.mount(
            Mock::given(method("GET"))
                .and(path("/api/v1/repos/alice/proj/versions"))
                .respond_with(ResponseTemplate::new(200).set_body_json(newest_page(newest))),
        );
        self.mount(
            Mock::given(method("GET"))
                .and(path(format!("/api/v1/repos/alice/proj/versions/{HEAD}")))
                .respond_with(ResponseTemplate::new(200).set_body_json(version(7, HEAD))),
        );
        self.mount(
            Mock::given(method("GET"))
                .and(path("/api/v1/repos/alice/proj/raw/a.md"))
                .respond_with(raw(BYTES)),
        );
    }
}

fn record() -> serde_json::Value {
    json!({
        "owner": "alice", "name": "proj", "description": null,
        "commitSha": HEAD, "status": "active", "author": null, "tags": [],
        "visibility": "public", "forkedFrom": null, "forkCount": 0,
        "fileCount": 1, "role": null,
        "createdAt": "2026-01-01T00:00:00Z", "updatedAt": "2026-01-01T00:00:00Z",
    })
}

fn version(number: u32, sha: &str) -> serde_json::Value {
    json!({
        "version": number, "sha": sha, "parentSha": null, "message": "m",
        "messageBody": null, "author": "alice",
        "createdAt": "2026-01-01T00:00:00Z", "filesChanged": ["a.md"],
    })
}

fn newest_page(sha: &str) -> serde_json::Value {
    json!({"data": [version(8, sha)], "total": 8, "limit": 1, "offset": 0})
}

fn raw(bytes: &[u8]) -> ResponseTemplate {
    ResponseTemplate::new(200)
        .insert_header("ETag", format!("\"{}\"", blob_sha1(bytes)).as_str())
        .set_body_bytes(bytes.to_vec())
}

fn header(request: &Request, name: &str) -> Option<String> {
    request
        .headers
        .get(name)
        .map(|value| value.to_str().unwrap().to_string())
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

const PLAIN: &str = concat!("syns/", env!("CARGO_PKG_VERSION"));

/// The `User-Agent` of every request a `syns cat` run with `caller` as
/// its `SYNS_CALLER` sent, after checking it sent some.
fn agents_of_a_cat(d: &Deployment, caller: Option<&OsStr>) -> Vec<String> {
    let before = d.requests().len();
    let cwd = tempfile::tempdir().unwrap();
    let args: Vec<&OsStr> = ["cat", "a.md", "--repo", "alice/proj"]
        .iter()
        .map(OsStr::new)
        .collect();
    let out = d.run_with(cwd.path(), d.config.path(), caller, &args);
    assert_eq!(out.status.code(), Some(0), "{caller:?}: {}", stderr(&out));
    assert_eq!(out.stdout, BYTES);
    let sent: Vec<String> = d.requests()[before..]
        .iter()
        .map(|r| header(r, "user-agent").unwrap_or_default())
        .collect();
    assert_eq!(sent.len(), 3, "{caller:?}: {sent:?}");
    sent
}

// X2: an in-bound `SYNS_CALLER` names the caller on every request, and
// every other value — unset, empty, out of bound, not UTF-8 — leaves the
// plain `syns/<version>`, never empty and never a name syns made up.
#[test]
#[serial]
fn the_user_agent_names_an_in_bound_caller_on_every_request() {
    let d = Deployment::new();
    d.mount_read(HEAD);
    let thirty_two = "a".repeat(32);
    let thirty_three = "a".repeat(33);
    let mut cases: Vec<(Option<&OsStr>, String)> = vec![
        (Some(OsStr::new("tool-a")), format!("{PLAIN} (tool-a)")),
        (Some(OsStr::new("9tool")), format!("{PLAIN} (9tool)")),
        (Some(OsStr::new("a.b_c-d")), format!("{PLAIN} (a.b_c-d)")),
        (
            Some(OsStr::new(thirty_two.as_str())),
            format!("{PLAIN} ({thirty_two})"),
        ),
        (Some(OsStr::new(thirty_three.as_str())), PLAIN.to_string()),
        (Some(OsStr::new("Tool-A")), PLAIN.to_string()),
        (Some(OsStr::new("tool a")), PLAIN.to_string()),
        (Some(OsStr::new(".")), PLAIN.to_string()),
        (Some(OsStr::new("..")), PLAIN.to_string()),
        (Some(OsStr::new("-tool")), PLAIN.to_string()),
        (Some(OsStr::new("")), PLAIN.to_string()),
        (None, PLAIN.to_string()),
    ];
    // A value that is not UTF-8, where the platform can carry one.
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt;
        cases.push((Some(OsStr::from_bytes(b"tool\xffa")), PLAIN.to_string()));
    }
    #[cfg(not(unix))]
    cases.push((None, PLAIN.to_string()));
    for (caller, expected) in cases {
        for agent in agents_of_a_cat(&d, caller) {
            assert_eq!(agent, expected, "SYNS_CALLER={caller:?}");
        }
    }
}

/// The install id of every request sent since `before`, each present.
fn install_ids(d: &Deployment, before: usize) -> Vec<Option<String>> {
    d.requests()[before..]
        .iter()
        .map(|r| header(r, "x-syns-install-id"))
        .collect()
}

fn a_cat_from(d: &Deployment, config: &Path) -> Vec<Option<String>> {
    let before = d.requests().len();
    let cwd = tempfile::tempdir().unwrap();
    let args: Vec<&OsStr> = ["cat", "a.md", "--repo", "alice/proj"]
        .iter()
        .map(OsStr::new)
        .collect();
    let out = d.run_with(cwd.path(), config, None, &args);
    assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
    let ids = install_ids(d, before);
    assert!(!ids.is_empty());
    ids
}

// X3: every request carries one install id, made on the first run and
// kept beside the credential, the same on every later run; another
// configuration keeps another.
#[test]
#[serial]
fn every_request_carries_the_install_id_kept_in_the_configuration() {
    let d = Deployment::new();
    d.mount_read(HEAD);
    let config = tempfile::tempdir().unwrap();
    let fresh = config.path().join("syns");

    let first = a_cat_from(&d, &fresh);
    let id = first[0].clone().expect("an install id");
    assert_eq!(id.len(), 32);
    assert!(
        id.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
    );
    assert!(
        first
            .iter()
            .all(|sent| sent.as_deref() == Some(id.as_str()))
    );
    assert_eq!(
        std::fs::read_to_string(fresh.join("install-id")).unwrap(),
        id
    );
    // I1: the folder a first read verb makes is owner-only, as the
    // credential store makes it, so a credential kept there later is in
    // no folder another local user can list.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(&fresh).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o700, "{mode:o}");
    }
    let second = a_cat_from(&d, &fresh);
    assert!(
        second
            .iter()
            .all(|sent| sent.as_deref() == Some(id.as_str())),
        "{second:?}"
    );

    let other = tempfile::tempdir().unwrap();
    let elsewhere = a_cat_from(&d, other.path());
    assert!(elsewhere[0].is_some());
    assert_ne!(elsewhere[0].as_deref(), Some(id.as_str()));
}

// X3: where the configuration folder takes no write and holds no id, no
// id is sent, run after run, and none is made.
#[cfg(unix)]
#[test]
#[serial]
fn no_install_id_is_sent_where_none_can_be_kept() {
    use std::os::unix::fs::PermissionsExt;
    // SAFETY: `geteuid` reads the process's effective uid.
    if unsafe { libc::geteuid() } == 0 {
        return;
    }
    let d = Deployment::new();
    d.mount_read(HEAD);
    let config = tempfile::tempdir().unwrap();
    std::fs::set_permissions(config.path(), std::fs::Permissions::from_mode(0o500)).unwrap();

    let first = a_cat_from(&d, config.path());
    let below = a_cat_from(&d, &config.path().join("syns"));
    let again = a_cat_from(&d, config.path());

    std::fs::set_permissions(config.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
    for sent in [first, below, again] {
        assert!(sent.iter().all(Option::is_none), "{sent:?}");
    }
    assert_eq!(std::fs::read_dir(config.path()).unwrap().count(), 0);
}

fn gzip(bytes: &[u8]) -> Vec<u8> {
    let mut encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
    encoder.write_all(bytes).unwrap();
    encoder.finish().unwrap()
}

fn zstd(bytes: &[u8]) -> Vec<u8> {
    zstd::encode_all(bytes, 3).unwrap()
}

fn compressed(encoding: &str, body: Vec<u8>) -> ResponseTemplate {
    ResponseTemplate::new(200)
        .insert_header("content-encoding", encoding)
        .insert_header("content-type", "application/json")
        .set_body_bytes(body)
}

// X4: every request asks for `zstd, gzip`; answers in either are decoded
// before they are read, and a file's decoded bytes are still the ones
// its `ETag` names.
#[test]
#[serial]
fn compressed_answers_are_decoded_and_the_bytes_still_checked() {
    let d = Deployment::new();
    let content: Vec<u8> = (0..4096u32)
        .flat_map(|n| format!("line {n}\n").into_bytes())
        .collect();
    d.mount(
        Mock::given(method("GET"))
            .and(path("/api/v1/repos/alice/proj"))
            .respond_with(compressed("gzip", gzip(record().to_string().as_bytes()))),
    );
    d.mount(
        Mock::given(method("GET"))
            .and(path("/api/v1/repos/alice/proj/versions"))
            .respond_with(compressed(
                "zstd",
                zstd(newest_page(HEAD).to_string().as_bytes()),
            )),
    );
    d.mount(
        Mock::given(method("GET"))
            .and(path("/api/v1/repos/alice/proj/raw/a.md"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("content-encoding", "zstd")
                    .insert_header("ETag", format!("\"{}\"", blob_sha1(&content)).as_str())
                    .set_body_bytes(zstd(&content)),
            ),
    );
    d.mount(
        Mock::given(method("GET"))
            .and(path("/api/v1/repos/alice/proj/raw/b.md"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("content-encoding", "gzip")
                    .insert_header("ETag", format!("\"{}\"", blob_sha1(&content)).as_str())
                    .set_body_bytes(gzip(b"not the named bytes\n")),
            ),
    );
    let cwd = tempfile::tempdir().unwrap();

    let out = d.run(cwd.path(), &["cat", "a.md", "--repo", "alice/proj"]);
    assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
    assert_eq!(out.stdout, content);

    let refused = d.run(cwd.path(), &["cat", "b.md", "--repo", "alice/proj"]);
    assert_eq!(refused.status.code(), Some(1), "{}", stderr(&refused));
    assert!(refused.stdout.is_empty());
    assert!(
        stderr(&refused).contains("invalid response body: b.md"),
        "{}",
        stderr(&refused)
    );

    let requests = d.requests();
    assert!(requests.len() >= 6, "{requests:?}");
    for request in &requests {
        assert_eq!(
            header(request, "accept-encoding").as_deref(),
            Some("zstd, gzip"),
            "{}",
            request.url
        );
    }
}

fn routes_since(d: &Deployment, before: usize) -> Vec<String> {
    let mut routes: Vec<String> = d.requests()[before..]
        .iter()
        .map(|r| r.url.path().to_string())
        .collect();
    routes.sort();
    routes
}

// X7: an unpinned read asks the record and the newest version at once,
// and looks nothing up by hash where the newest is the head.
#[test]
#[serial]
fn an_unpinned_read_asks_the_record_and_the_newest_version_at_once() {
    let d = Deployment::new();
    let delay = Duration::from_millis(1000);
    d.mount(
        Mock::given(method("GET"))
            .and(path("/api/v1/repos/alice/proj"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(record())
                    .set_delay(delay),
            ),
    );
    d.mount(
        Mock::given(method("GET"))
            .and(path("/api/v1/repos/alice/proj/versions"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(newest_page(HEAD))
                    .set_delay(delay),
            ),
    );
    d.mount(
        Mock::given(method("GET"))
            .and(path("/api/v1/repos/alice/proj/raw/a.md"))
            .respond_with(raw(BYTES)),
    );
    let cwd = tempfile::tempdir().unwrap();

    let started = Instant::now();
    let out = d.run(
        cwd.path(),
        &["--json", "cat", "a.md", "--repo", "alice/proj"],
    );
    let took = started.elapsed();

    assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
    let document: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(document["version"], 8);
    assert_eq!(document["commitSha"], HEAD);
    assert_eq!(document["content"], "alpha\nbeta\n");
    assert!(
        took < delay * 2,
        "the two were asked one after the other: {took:?}"
    );
    assert_eq!(
        routes_since(&d, 0),
        vec![
            "/api/v1/repos/alice/proj",
            "/api/v1/repos/alice/proj/raw/a.md",
            "/api/v1/repos/alice/proj/versions",
        ]
    );
}

// X7: where the newest version is not the head, the head alone is looked
// up by its hash, and the run reads at the head.
#[test]
#[serial]
fn an_unpinned_read_looks_the_head_up_where_the_newest_is_not_it() {
    let d = Deployment::new();
    d.mount_read("abc0000000000000000000000000000000000000");
    let cwd = tempfile::tempdir().unwrap();

    let out = d.run(
        cwd.path(),
        &["--json", "cat", "a.md", "--repo", "alice/proj"],
    );

    assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
    let document: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(document["version"], 7);
    assert_eq!(document["commitSha"], HEAD);
    assert_eq!(
        routes_since(&d, 0),
        vec![
            "/api/v1/repos/alice/proj".to_string(),
            "/api/v1/repos/alice/proj/raw/a.md".to_string(),
            "/api/v1/repos/alice/proj/versions".to_string(),
            format!("/api/v1/repos/alice/proj/versions/{HEAD}"),
        ]
    );
}
