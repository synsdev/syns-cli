//! Roadmap 279: the read verbs over v2 — one head request with
//! `If-None-Match`, `ls` and `glob` from the closure and `trees/`, and
//! `syns cat --json` over `POST …/read` (FEATURE_SPEC.md § Expected rows
//! 26–30; D-243 §8, §9).

use std::path::PathBuf;
use std::time::{Duration, Instant};

use crate::integration::v2_fake::{Act, Env, IDENTITY, as_refs, blob, many, ok, transcript};

fn work(env: &Env) -> PathBuf {
    let dir = env.dir("w");
    std::fs::write(dir.join(".syns.yaml"), IDENTITY).unwrap();
    dir
}

fn routes(env: &Env) -> Vec<String> {
    env.fake.routes()
}

fn statuses(env: &Env) -> Vec<(String, u16)> {
    env.fake
        .log()
        .into_iter()
        .map(|s| (s.route, s.status))
        .collect()
}

/// Each of `commands` run over v1 alone and over v2: transcripts equal.
fn same_as_v1(setup: impl Fn(&Env), commands: &[&[&str]]) {
    let run = |v2: bool| {
        let env = Env::new();
        env.fake.v2(v2);
        setup(&env);
        let dir = work(&env);
        commands
            .iter()
            .map(|args| transcript(&env.run(&dir, args), env.fake.port))
            .collect::<Vec<_>>()
    };
    let v1 = run(false);
    let v2 = run(true);
    for (i, (a, b)) in v2.iter().zip(&v1).enumerate() {
        assert_eq!(a, b, "{:?}", commands[i]);
    }
}

fn setup(env: &Env) {
    let mut files = many(20);
    files.push(("docs/guide.md".into(), b"# guide\n".to_vec()));
    files.push(("docs/deep/more.md".into(), b"more\n".to_vec()));
    files.push(("logo.png".into(), vec![0x89, b'P', b'N', b'G', 0, 1, 2]));
    env.fake.commit(&as_refs(&files));
    env.fake.change(&[("a.md", Some(b"a at 2\n"))]);
}

// ---- one head request (row 26) -----------------------------------------------

#[test]
fn r26_one_head_request_and_a_304_after() {
    let env = Env::new();
    setup(&env);
    let dir = work(&env);
    ok(&env.run(&dir, &["cat", "a.md"]));
    assert_eq!(routes(&env), vec!["v2 head", "v1 raw"]);
    env.fake.clear();
    let out = env.run(&dir, &["cat", "a.md"]);
    ok(&out);
    assert_eq!(out.stdout, b"a at 2\n");
    let log = env.fake.log();
    assert_eq!(log[0].route, "v2 head");
    assert_eq!(log[0].status, 304);
    assert_eq!(
        log[0].header("if-none-match"),
        Some(format!("\"{}\"", env.fake.head().sha).as_str())
    );
    for verb in [
        vec!["read", "a.md"],
        vec!["grep", "file"],
        vec!["ls"],
        vec!["glob", "**/*.md"],
    ] {
        env.fake.clear();
        ok(&env.run(&dir, &verb));
        let heads = routes(&env).iter().filter(|r| *r == "v2 head").count();
        assert_eq!(heads, 1, "{verb:?}");
        assert!(
            !routes(&env).contains(&"v1 versions".to_string()),
            "{verb:?}"
        );
    }
}

#[test]
fn r26_output_equals_v1s() {
    same_as_v1(
        setup,
        &[
            &["cat", "a.md"],
            &["--json", "cat", "a.md"],
            &["read", "docs/guide.md"],
            &["--json", "read", "docs/guide.md"],
            &["grep", "file 1"],
            &["--json", "grep", "file 1"],
            &["cat", "a.md", "--version", "1"],
            &["cat", "nope.md"],
        ],
    );
}

#[test]
fn r26_an_empty_repository_refuses_as_today() {
    same_as_v1(
        |_| {},
        &[&["cat", "a.md"], &["ls"], &["--json", "glob", "*"]],
    );
}

// ---- ls and glob (row 27) --------------------------------------------------------

#[test]
fn r27_ls_and_glob_equal_v1s() {
    same_as_v1(
        setup,
        &[
            &["ls"],
            &["ls", "docs"],
            &["ls", "--recursive"],
            &["--json", "ls"],
            &["--json", "ls", "docs", "--recursive"],
            &["glob", "**/*.md"],
            &["--json", "glob", "**/*.md"],
            &["glob", "*.md", "--path", "docs"],
            &["ls", "--version", "1"],
            &["--json", "ls", "--version", "1"],
            &["ls", "nope"],
            &["--json", "ls", "nope"],
            &["ls", "a.md"],
            // A trailing `/`, as tab completion writes it (REVIEW-syns-cli I1).
            &["ls", "docs/"],
            &["--json", "ls", "docs/", "--recursive"],
            &["glob", "*.md", "--path", "docs/"],
            &["--json", "glob", "**", "--path", "docs//"],
            // A path that names nothing once trimmed keeps v1 (VERIFY D1).
            &["ls", ""],
            &["ls", "/"],
            &["ls", " "],
            &["--json", "ls", ""],
            &["ls", "", "--version", "1"],
            &["glob", "*", "--path", ""],
            &["--json", "glob", "*", "--path", "/"],
        ],
    );
}

#[test]
fn r27_run_2_is_two_304s_read_from_trees() {
    let env = Env::new();
    setup(&env);
    let dir = work(&env);
    let first = env.run(&dir, &["--json", "ls", "--recursive"]);
    ok(&first);
    assert_eq!(routes(&env), vec!["v2 head", "v2 tree"]);
    env.fake.clear();
    let second = env.run(&dir, &["--json", "ls", "--recursive"]);
    ok(&second);
    assert_eq!(
        statuses(&env),
        vec![("v2 head".to_string(), 304), ("v2 tree".to_string(), 304)]
    );
    assert_eq!(second.stdout, first.stdout);
    env.fake.clear();
    let glob = env.run(&dir, &["glob", "docs/**"]);
    ok(&glob);
    assert_eq!(
        String::from_utf8_lossy(&glob.stdout),
        "docs/deep/more.md\ndocs/guide.md\n"
    );
}

// ---- cat --json over the batch (rows 28–30) ---------------------------------------

#[test]
fn r28_sixteen_paths_in_one_request() {
    let env = Env::new();
    let files = many(16);
    env.fake.commit(&as_refs(&files));
    let dir = work(&env);
    let paths: Vec<&str> = files.iter().map(|(p, _)| p.as_str()).collect();
    let mut args = vec!["--json", "cat", "--"];
    args.extend(&paths);
    let out = env.run(&dir, &args);
    ok(&out);
    assert_eq!(routes(&env), vec!["v2 read"]);
    let read = &env.fake.reads()[0];
    assert_eq!(read.json()["at"], "head");
    // The document equals 278's over v1.
    let v1 = Env::new();
    v1.fake.v2(false);
    v1.fake.commit(&as_refs(&files));
    let base = v1.run(&work(&v1), &args);
    assert_eq!(
        transcript(&out, env.fake.port),
        transcript(&base, v1.fake.port)
    );
}

#[test]
fn r28_many_chunks_pin_the_first_chunk_s_commit() {
    let env = Env::new();
    let files = many(1200);
    env.fake.commit(&as_refs(&files));
    let dir = work(&env);
    let mut input = String::new();
    for (p, _) in &files {
        input.push_str(p);
        input.push('\n');
    }
    let mut command = env.command(&dir, &["--json", "cat", "--paths-from=-"]);
    command.stdin(std::process::Stdio::piped());
    command.stdout(std::process::Stdio::piped());
    let mut child = command.spawn().unwrap();
    use std::io::Write;
    child
        .stdin
        .take()
        .unwrap()
        .write_all(input.as_bytes())
        .unwrap();
    let out = child.wait_with_output().unwrap();
    ok(&out);
    let reads = env.fake.reads();
    assert_eq!(reads.len(), 3);
    assert_eq!(reads[0].json()["at"], "head");
    let head = env.fake.head().sha;
    assert_eq!(reads[1].json()["at"], head.as_str());
    assert_eq!(reads[2].json()["at"], head.as_str());
    let doc: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(doc["files"].as_array().unwrap().len(), 1200);
}

#[test]
fn r28_a_pinned_read_resolves_then_reads_once() {
    let env = Env::new();
    setup(&env);
    let dir = work(&env);
    let out = env.run(
        &dir,
        &[
            "--json",
            "cat",
            "--version",
            "1",
            "--",
            "a.md",
            "docs/guide.md",
        ],
    );
    ok(&out);
    let routes = routes(&env);
    assert_eq!(routes.last().map(String::as_str), Some("v2 read"));
    assert_eq!(routes.iter().filter(|r| *r == "v2 read").count(), 1);
    let first = env.fake.with(|s| s.commits[0].sha.clone());
    assert_eq!(env.fake.reads()[0].json()["at"], first.as_str());
}

#[test]
fn r28_the_files_document_equals_v1s() {
    same_as_v1(
        setup,
        &[
            &[
                "--json",
                "cat",
                "--",
                "a.md",
                "docs/guide.md",
                "logo.png",
                "nope.md",
            ],
            &["--json", "cat", "--", "a.md", "a.md", "./a.md"],
            &[
                "--json",
                "cat",
                "--version",
                "1",
                "--",
                "a.md",
                "docs/guide.md",
            ],
            &[
                "--json",
                "cat",
                "--max-bytes",
                "10",
                "--",
                "a.md",
                "docs/guide.md",
                "logo.png",
            ],
            &["cat", "--", "a.md", "docs/guide.md"],
        ],
    );
}

#[test]
fn r29_have_and_unchanged_frames() {
    let env = Env::new();
    env.fake.commit(&[("a.md", b"a\n"), ("b.md", b"b\n")]);
    let dir = work(&env);
    let args = ["--json", "cat", "--", "a.md", "b.md"];
    ok(&env.run(&dir, &args));
    env.fake.clear();
    let second = env.run(&dir, &args);
    ok(&second);
    let read = &env.fake.reads()[0];
    assert_eq!(read.json()["paths"][0]["have"], blob(b"a\n"));
    assert_eq!(read.json()["paths"][1]["have"], blob(b"b\n"));
    // After a push changing `b.md` only: `a.md` unchanged, `b.md` ok.
    env.fake.change(&[("b.md", Some(b"b2\n"))]);
    env.fake.clear();
    let third = env.run(&dir, &args);
    ok(&third);
    let doc: serde_json::Value = serde_json::from_slice(&third.stdout).unwrap();
    assert_eq!(doc["files"][1]["content"], "b2\n");
    // A tampered blob is sent without `have`, the other with it.
    let stored = env.store().join("blobs").join(blob(b"a\n"));
    std::fs::write(&stored, b"tampered").unwrap();
    env.fake.clear();
    let fourth = env.run(&dir, &args);
    ok(&fourth);
    let read = &env.fake.reads()[0];
    assert!(read.json()["paths"][0].get("have").is_none());
    assert_eq!(read.json()["paths"][1]["have"], blob(b"b2\n"));
    let doc: serde_json::Value = serde_json::from_slice(&fourth.stdout).unwrap();
    assert_eq!(doc["files"][0]["content"], "a\n");
}

#[test]
fn r29_a_chunk_answered_busy_is_resent_then_its_paths_take_the_class() {
    let env = Env::new();
    env.fake.commit(&[("a.md", b"a\n"), ("b.md", b"b\n")]);
    let dir = work(&env);
    env.fake.act(
        "v2 read",
        Act::Refuse {
            status: 503,
            error: "engine_busy",
            retry_after: Some(1),
        },
    );
    let out = env.run(&dir, &["--json", "cat", "--", "a.md", "b.md"]);
    ok(&out);
    assert_eq!(env.fake.reads().len(), 2);
    for _ in 0..30 {
        env.fake.act(
            "v2 read",
            Act::Refuse {
                status: 503,
                error: "engine_busy",
                retry_after: Some(10),
            },
        );
    }
    let out = env.run_env(
        &dir,
        &[("_INTERNAL_SYNS_WAIT_PERCENT", "1")],
        &["--json", "cat", "--", "a.md", "b.md"],
    );
    ok(&out);
    let doc: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    for file in doc["files"].as_array().unwrap() {
        assert_eq!(file["error"]["code"], "ENGINE_BUSY", "{doc}");
    }
}

#[test]
fn r30_max_bytes_over_frames() {
    let env = Env::new();
    let big = vec![b'x'; 4096];
    env.fake
        .commit(&[("a.md", b"aaaa"), ("big.md", &big), ("c.md", b"c")]);
    let dir = work(&env);
    env.fake.act("v2 read", Act::Endless);
    let started = Instant::now();
    let out = env.run(
        &dir,
        &[
            "--json",
            "cat",
            "--max-bytes",
            "100",
            "--",
            "a.md",
            "big.md",
            "c.md",
        ],
    );
    ok(&out);
    assert!(started.elapsed() < Duration::from_secs(5));
    let doc: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(doc["deferred"], serde_json::json!(["big.md", "c.md"]));
    assert_eq!(env.fake.reads().len(), 1, "no further chunk");
    assert!(!env.store().join("blobs").join(blob(&big)).exists());
}

// VERIFY D1: a typed path naming nothing once trimmed is refused as the
// base binary refuses it, through v1.
#[test]
fn d1_a_path_naming_nothing_refuses_as_today() {
    let env = Env::new();
    setup(&env);
    let dir = work(&env);
    for (args, line) in [
        (vec!["ls", ""], "error: path not found: \n"),
        (vec!["ls", "/"], "error: path not found: /\n"),
        (vec!["glob", "*", "--path", ""], "error: path not found: \n"),
        (vec!["ls", " "], "validation_error"),
    ] {
        env.fake.clear();
        let out = env.run(&dir, &args);
        assert_eq!(out.status.code(), Some(1), "{args:?}");
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert!(stderr.contains(line), "{args:?}: {stderr}");
        assert!(
            !env.fake.routes().contains(&"v2 tree".to_string()),
            "{args:?}: {:?}",
            env.fake.routes()
        );
    }
}
