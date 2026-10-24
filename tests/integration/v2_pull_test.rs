//! Roadmap 279: pull and sync on the closure and `changes`, fetching only
//! the blobs this machine lacks, retrying per chunk (FEATURE_SPEC.md
//! § Expected rows 1–25 and 31–36, D-243).
//!
//! Every case drives the built binary against `v2_fake::Fake`. "Identical
//! to the base binary" is checked against the same build over the same
//! fake with v2 off, where every read goes down v1 — today's code, kept
//! unchanged; the verifier compares against the 7153f9c binary itself
//! (row 37). Each test is named for its Expected row (`r<NN>_…`).

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use serial_test::serial;

use crate::integration::v2_fake::{
    Absent, Act, Entry, Env, Fake, Files, IDENTITY, NAME, OWNER, as_refs, blob, files_of, many, ok,
    transcript,
};

/// A directory holding the repository's identity file, so a bare
/// `syns pull` there is a fresh checkout reading no record.
fn checkout(env: &Env, name: &str) -> PathBuf {
    let dir = env.dir(name);
    std::fs::write(dir.join(".syns.yaml"), IDENTITY).unwrap();
    dir
}

fn pull(env: &Env, dir: &Path) -> std::process::Output {
    env.run(dir, &["pull"])
}

fn sync(env: &Env, dir: &Path) -> std::process::Output {
    env.run(dir, &["sync"])
}

/// The requests a run made, v1 and v2, pushes left out.
fn counted(env: &Env) -> Vec<String> {
    env.fake
        .routes()
        .into_iter()
        .filter(|r| r != "v1 push")
        .collect()
}

/// Every recorded base under the machine, as JSON, its time left out.
fn bases(env: &Env) -> Vec<serde_json::Value> {
    let mut out = Vec::new();
    for root in [env.cache.path(), env.root.path()] {
        let mut pending = vec![root.to_path_buf()];
        while let Some(dir) = pending.pop() {
            for entry in std::fs::read_dir(&dir).into_iter().flatten().flatten() {
                let path = entry.path();
                if entry.file_type().unwrap().is_dir() {
                    pending.push(path);
                } else if path.file_name().is_some_and(|n| n == "base.json") {
                    let mut value: serde_json::Value =
                        serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
                    value.as_object_mut().unwrap().remove("recorded_at");
                    out.push(value);
                }
            }
        }
    }
    out
}

fn files_under(dir: &Path) -> Vec<String> {
    let mut out = Vec::new();
    let mut pending = vec![dir.to_path_buf()];
    while let Some(d) = pending.pop() {
        for entry in std::fs::read_dir(&d).into_iter().flatten().flatten() {
            if entry.file_type().unwrap().is_dir() {
                pending.push(entry.path());
            } else {
                out.push(
                    entry
                        .path()
                        .strip_prefix(dir)
                        .unwrap()
                        .to_string_lossy()
                        .to_string(),
                );
            }
        }
    }
    out.sort();
    out
}

/// `text` with every recovery id written `RECOVERY`: each run mints its
/// own.
fn recovery_ids(text: &str) -> String {
    let mut out = text.to_string();
    for marker in [
        "Recovery id: ",
        "recovery id ",
        "Discarded resolution ",
        "\"recoveryId\":\"",
        "\"recoveryId\": \"",
    ] {
        let mut cleaned = String::new();
        let mut rest = out.as_str();
        while let Some(at) = rest.find(marker) {
            cleaned.push_str(&rest[..at + marker.len()]);
            let tail = &rest[at + marker.len()..];
            let n = tail.chars().take_while(|c| c.is_ascii_hexdigit()).count();
            cleaned.push_str("RECOVERY");
            rest = &tail[n..];
        }
        cleaned.push_str(rest);
        out = cleaned;
    }
    out
}

/// The first path of chunk `n` of the head's paths, in path order.
fn chunk_start(env: &Env, n: usize) -> String {
    // The checkout's own identity file already holds the head's.
    let paths: Vec<String> = env
        .fake
        .head()
        .files
        .keys()
        .filter(|p| *p != ".syns.yaml")
        .cloned()
        .collect();
    paths[n * 500].clone()
}

/// One scenario run twice, over v1 alone and over v2: every transcript
/// and every file written equal.
fn same_as_v1(scenario: impl Fn(&Env) -> Vec<std::process::Output>) {
    let run = |v2: bool| {
        let env = Env::new();
        env.fake.v2(v2);
        let outs = scenario(&env);
        let transcripts: Vec<_> = outs
            .iter()
            .map(|o| {
                let (out, err, code) = transcript(o, env.fake.port);
                (recovery_ids(&out), recovery_ids(&err), code)
            })
            .collect();
        let mut written = BTreeMap::new();
        for entry in std::fs::read_dir(env.root.path()).unwrap().flatten() {
            written.insert(
                entry.file_name().to_string_lossy().to_string(),
                files_of(&entry.path()),
            );
        }
        (transcripts, written, bases(&env))
    };
    let v1 = run(false);
    let v2 = run(true);
    assert_eq!(v2.0, v1.0, "transcripts differ");
    assert_eq!(v2.1, v1.1, "files differ");
    assert_eq!(v2.2, v1.2, "bases differ");
}

// ---- the head and the blobs ------------------------------------------------

#[test]
fn r01_a_fresh_checkout_on_a_cold_store() {
    let env = Env::new();
    let files = many(1201);
    let commit = env.fake.commit(&as_refs(&files));
    let a = checkout(&env, "a");
    ok(&pull(&env, &a));
    let log = env.fake.log();
    let routes: Vec<&str> = log.iter().map(|s| s.route.as_str()).collect();
    assert_eq!(routes, vec!["v2 tree", "v2 read", "v2 read", "v2 read"]);
    let reads: Vec<_> = log.iter().filter(|s| s.route == "v2 read").collect();
    let mut sizes: Vec<usize> = reads.iter().map(|r| r.asked().len()).collect();
    sizes.sort();
    assert_eq!(sizes, vec![201, 500, 500]);
    for read in &reads {
        assert_eq!(read.json()["at"], commit.as_str());
        assert_eq!(read.header("syns-lane"), Some("bulk"));
        let asked = read.asked();
        let distinct: std::collections::BTreeSet<_> = asked.iter().collect();
        assert_eq!(distinct.len(), asked.len());
    }
    assert_eq!(log[0].header("syns-lane"), Some("bulk"));
    // Every blob stored and verifying, every tree, and the head's view.
    for (_, bytes) in &files {
        let stored = std::fs::read(env.store().join("blobs").join(blob(bytes))).unwrap();
        assert_eq!(&stored, bytes);
    }
    assert!(files_under(&env.store().join("trees")).len() >= 8);
    let views = files_under(&env.store().join("views"));
    assert_eq!(views.len(), 1, "{views:?}");
    assert!(views[0].ends_with(&commit));
    // The checkout holds the head.
    let written = files_of(&a);
    for (path, bytes) in &files {
        assert_eq!(written.get(path), Some(bytes), "{path}");
    }
}

#[test]
fn r01_the_checkout_and_base_equal_v1s() {
    same_as_v1(|env| {
        env.fake.commit(&as_refs(&many(1200)));
        let a = checkout(env, "a");
        vec![pull(env, &a), env.run(&a, &["--json", "status"])]
    });
}

#[test]
fn r02_a_fresh_checkout_with_every_blob_held() {
    let env = Env::new();
    let files = many(300);
    env.fake.commit(&as_refs(&files));
    let a = checkout(&env, "a");
    ok(&pull(&env, &a));
    env.fake.clear();
    let b = checkout(&env, "b");
    let out = pull(&env, &b);
    ok(&out);
    let log = env.fake.log();
    assert_eq!(log.len(), 1, "{:?}", env.fake.routes());
    assert_eq!(log[0].route, "v2 tree");
    assert_eq!(log[0].status, 304);
    assert_eq!(files_of(&b), files_of(&a));

    // The head moved: a `200`, still no read.
    env.fake.change(&[("unrelated.md", Some(b"u\n"))]);
    std::fs::remove_dir_all(&b).unwrap();
    let b = checkout(&env, "b");
    env.fake.clear();
    ok(&pull(&env, &b));
    let routes = env.fake.routes();
    assert_eq!(
        routes,
        vec!["v2 tree", "v2 read"],
        "the new file alone is read"
    );
    assert_eq!(env.fake.reads()[0].asked(), vec!["unrelated.md"]);

    // One byte changed in a stored blob: that path alone read again.
    let (victim, bytes) = &files[7];
    let stored = env.store().join("blobs").join(blob(bytes));
    let mut altered = bytes.clone();
    altered[0] ^= 1;
    std::fs::write(&stored, &altered).unwrap();
    let c = checkout(&env, "c");
    env.fake.clear();
    ok(&pull(&env, &c));
    let reads = env.fake.reads();
    assert_eq!(reads.len(), 1);
    assert_eq!(reads[0].asked(), vec![victim.clone()]);
    assert_eq!(files_of(&c).get(victim), Some(bytes));
}

#[test]
fn r03_only_what_is_missing() {
    let env = Env::new();
    let files = many(1200);
    env.fake.commit(&as_refs(&files[..399]));
    let a = checkout(&env, "a");
    ok(&pull(&env, &a));
    env.fake.commit(&as_refs(&files));
    env.fake.clear();
    let b = checkout(&env, "b");
    ok(&pull(&env, &b));
    let asked: Vec<String> = env.fake.reads().iter().flat_map(|r| r.asked()).collect();
    assert_eq!(asked.len(), 801);
    let wanted: std::collections::BTreeSet<String> =
        files[399..].iter().map(|(p, _)| p.clone()).collect();
    assert_eq!(
        asked.into_iter().collect::<std::collections::BTreeSet<_>>(),
        wanted
    );

    // A local file already matching the head is neither read nor written.
    let c = checkout(&env, "c");
    let (path, bytes) = &files[1000];
    std::fs::create_dir_all(c.join(path).parent().unwrap()).unwrap();
    std::fs::write(c.join(path), bytes).unwrap();
    let before = std::fs::metadata(c.join(path)).unwrap().modified().unwrap();
    std::thread::sleep(Duration::from_millis(20));
    env.fake.clear();
    let out = pull(&env, &c);
    ok(&out);
    assert!(!String::from_utf8_lossy(&out.stderr).contains(&format!("downloaded: {path}")));
    assert_eq!(
        std::fs::metadata(c.join(path)).unwrap().modified().unwrap(),
        before
    );
}

#[test]
#[serial]
fn r04_no_more_than_four_chunks_open() {
    let env = Env::new();
    env.fake.commit(&as_refs(&many(2599)));
    env.fake.with(|s| s.hold_read = Duration::from_millis(300));
    let a = checkout(&env, "a");
    ok(&pull(&env, &a));
    assert_eq!(env.fake.reads().len(), 6);
    let max = env
        .fake
        .with(|s| s.max_in_flight.get("v2 read").copied().unwrap_or(0));
    assert_eq!(max, 4);
}

#[test]
fn r04_chunks_packed_by_named_sizes() {
    let env = Env::new();
    let images: Vec<(String, Vec<u8>)> = (0..119)
        .map(|i| (format!("img{i:03}.png"), vec![i as u8; 300 * 1024]))
        .collect();
    env.fake.commit(&as_refs(&images));
    let a = checkout(&env, "a");
    ok(&pull(&env, &a));
    let reads = env.fake.reads();
    assert_eq!(reads.len(), 2);
    assert!(reads.iter().all(|r| r.asked().len() <= 109));
}

#[test]
fn r05_a_no_op_sync_is_one_request() {
    let env = Env::new();
    env.fake.commit(&as_refs(&many(50)));
    let a = checkout(&env, "a");
    ok(&pull(&env, &a));
    env.fake.clear();
    ok(&sync(&env, &a));
    assert_eq!(counted(&env), vec!["v2 changes"]);
    let changes = &env.fake.log()[0];
    assert_eq!(changes.query.get("to").map(String::as_str), Some("head"));
    assert_eq!(changes.header("syns-lane"), Some("bulk"));

    // The head moved by a version changing nothing in scope: still one.
    let moved = env.fake.with(|s| {
        let files = s.commits.last().unwrap().files.clone();
        s.add(files)
    });
    env.fake.clear();
    ok(&sync(&env, &a));
    assert_eq!(counted(&env), vec!["v2 changes"]);
    assert!(
        bases(&env)
            .iter()
            .any(|b| b["commit_sha"] == moved.as_str())
    );
    env.fake.clear();
    ok(&sync(&env, &a));
    assert_eq!(counted(&env), vec!["v2 changes"]);
}

#[test]
fn r05_no_op_output_equals_v1s() {
    same_as_v1(|env| {
        env.fake.commit(&as_refs(&many(20)));
        let a = checkout(env, "a");
        let first = pull(env, &a);
        env.fake.with(|s| {
            let files = s.commits.last().unwrap().files.clone();
            s.add(files);
        });
        vec![
            first,
            sync(env, &a),
            sync(env, &a),
            env.run(&a, &["sync", "--json"]),
        ]
    });
}

#[test]
fn r06_a_sync_with_m_changes_is_two_requests() {
    let env = Env::new();
    let files = many(700);
    env.fake.commit(&as_refs(&files));
    let a = checkout(&env, "a");
    ok(&pull(&env, &a));
    for m in [1usize, 5, 500] {
        let changes: Vec<(String, Vec<u8>)> = (0..m)
            .map(|i| (files[i].0.clone(), format!("edit {m} {i}\n").into_bytes()))
            .collect();
        let refs: Vec<(&str, Option<&[u8]>)> = changes
            .iter()
            .map(|(p, b)| (p.as_str(), Some(b.as_slice())))
            .collect();
        env.fake.change(&refs);
        env.fake.clear();
        ok(&sync(&env, &a));
        assert_eq!(counted(&env), vec!["v2 changes", "v2 read"], "m = {m}");
        let read = &env.fake.reads()[0];
        assert_eq!(read.asked().len(), m);
        assert_eq!(read.json()["at"], env.fake.head().sha.as_str());
        let written = files_of(&a);
        for (p, b) in &changes {
            assert_eq!(written.get(p), Some(b));
        }
    }
    // An addition and a rename.
    env.fake.change(&[
        ("new/added.md", Some(b"added\n")),
        (&files[600].0, None),
        ("moved/600.md", Some(&files[600].1)),
    ]);
    env.fake.clear();
    ok(&sync(&env, &a));
    assert_eq!(counted(&env), vec!["v2 changes", "v2 read"]);
    assert!(!a.join(&files[600].0).exists());
    assert_eq!(std::fs::read(a.join("moved/600.md")).unwrap(), files[600].1);
    // Deletions only: one request.
    env.fake
        .change(&[(&files[601].0, None), (&files[602].0, None)]);
    env.fake.clear();
    ok(&sync(&env, &a));
    assert_eq!(counted(&env), vec!["v2 changes"]);
    assert!(!a.join(&files[601].0).exists());
    // m = 600: three.
    let changes: Vec<(String, Vec<u8>)> = (0..600)
        .map(|i| (files[i].0.clone(), format!("again {i}\n").into_bytes()))
        .collect();
    let refs: Vec<(&str, Option<&[u8]>)> = changes
        .iter()
        .map(|(p, b)| (p.as_str(), Some(b.as_slice())))
        .collect();
    env.fake.change(&refs);
    env.fake.clear();
    ok(&sync(&env, &a));
    assert_eq!(counted(&env), vec!["v2 changes", "v2 read", "v2 read"]);
}

#[test]
fn r06_every_changed_blob_held_is_still_two() {
    let env = Env::new();
    let files = many(30);
    env.fake.commit(&as_refs(&files));
    let a = checkout(&env, "a");
    let b = checkout(&env, "b");
    ok(&pull(&env, &a));
    ok(&pull(&env, &b));
    env.fake.change(&[(&files[3].0, Some(b"teammate\n"))]);
    ok(&sync(&env, &a));
    env.fake.clear();
    ok(&sync(&env, &b));
    assert_eq!(counted(&env), vec!["v2 changes", "v2 read"]);
    let read = &env.fake.reads()[0];
    assert_eq!(read.json()["paths"][0]["have"], blob(b"teammate\n"));
    assert_eq!(std::fs::read(b.join(&files[3].0)).unwrap(), b"teammate\n");
}

#[test]
fn r06_output_equals_v1s() {
    same_as_v1(|env| {
        let files = many(40);
        env.fake.commit(&as_refs(&files));
        let a = checkout(env, "a");
        let first = pull(env, &a);
        env.fake.change(&[
            (&files[1].0, Some(b"one\n")),
            ("added.md", Some(b"two\n")),
            (&files[2].0, None),
        ]);
        let second = sync(env, &a);
        env.fake.change(&[(&files[3].0, Some(b"three\n"))]);
        vec![
            first,
            second,
            env.run(&a, &["--json", "sync"]),
            env.run(&a, &["--json", "status"]),
        ]
    });
}

// ---- the view (D-243 §1) ---------------------------------------------------

/// The view files the store holds.
fn views(env: &Env) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let mut pending = vec![env.store().join("views")];
    while let Some(d) = pending.pop() {
        for entry in std::fs::read_dir(&d).into_iter().flatten().flatten() {
            if entry.file_type().unwrap().is_dir() {
                pending.push(entry.path());
            } else {
                out.push(entry.path());
            }
        }
    }
    out
}

fn view_of(env: &Env, commit: &str) -> PathBuf {
    views(env)
        .into_iter()
        .find(|p| p.file_name().unwrap().to_string_lossy() == commit)
        .unwrap_or_else(|| panic!("no view of {commit}"))
}

#[test]
fn r07_an_inconsistent_view_is_dropped_and_the_closure_asked() {
    let env = Env::new();
    let files = many(20);
    let base = env.fake.commit(&as_refs(&files));
    let a = checkout(&env, "a");
    ok(&pull(&env, &a));
    // The view lies about one path's hash: `changes` names another oldSha.
    let path = view_of(&env, &base);
    let text = std::fs::read_to_string(&path).unwrap();
    let altered = text.replacen(&blob(&files[0].1), &blob(b"not it"), 1);
    // Keep the digest honest so only the contradiction drops it.
    let (head, body) = altered.split_once("\n--\n").unwrap();
    use sha2::Digest;
    let digest: String = sha2::Sha256::digest(body.as_bytes())
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    let head: Vec<String> = head
        .lines()
        .map(|l| {
            if l.starts_with("digest ") {
                format!("digest {digest}")
            } else {
                l.to_string()
            }
        })
        .collect();
    std::fs::write(&path, format!("{}\n--\n{body}", head.join("\n"))).unwrap();
    env.fake.change(&[(&files[0].0, Some(b"changed\n"))]);
    env.fake.clear();
    ok(&sync(&env, &a));
    assert_eq!(counted(&env), vec!["v2 changes", "v2 tree", "v2 read"]);
    assert_eq!(std::fs::read(a.join(&files[0].0)).unwrap(), b"changed\n");
}

#[test]
fn r08_no_usable_view_reads_the_closure() {
    let env = Env::new();
    let files = many(20);
    env.fake.commit(&as_refs(&files));
    let a = checkout(&env, "a");
    ok(&pull(&env, &a));
    // The store emptied.
    std::fs::remove_dir_all(env.cache.path().join("store")).unwrap();
    env.fake.clear();
    ok(&sync(&env, &a));
    assert_eq!(counted(&env), vec!["v2 tree"]);
    env.fake.change(&[(&files[0].0, Some(b"x\n"))]);
    std::fs::remove_dir_all(env.cache.path().join("store")).unwrap();
    env.fake.clear();
    ok(&sync(&env, &a));
    assert_eq!(counted(&env), vec!["v2 tree", "v2 read"]);
    assert_eq!(std::fs::read(a.join(&files[0].0)).unwrap(), b"x\n");
}

#[cfg(unix)]
#[test]
fn r08_an_unusable_store_reads_closure_and_blobs() {
    let env = Env::new();
    let files = many(20);
    env.fake.commit(&as_refs(&files));
    // The store's root is a link: unusable, skipped without a word.
    let elsewhere = tempfile::tempdir().unwrap();
    std::os::unix::fs::symlink(elsewhere.path(), env.cache.path().join("store")).unwrap();
    let a = checkout(&env, "a");
    let out = pull(&env, &a);
    ok(&out);
    assert_eq!(counted(&env)[0], "v2 tree");
    assert_eq!(files_of(&a).len(), 21);
    env.fake.clear();
    ok(&sync(&env, &a));
    assert_eq!(counted(&env), vec!["v2 tree"]);
    assert!(
        std::fs::read_dir(elsewhere.path())
            .unwrap()
            .next()
            .is_none()
    );
}

#[test]
fn r09_a_base_the_server_no_longer_serves() {
    let env = Env::new();
    let files = many(10);
    env.fake.commit(&as_refs(&files));
    let a = checkout(&env, "a");
    ok(&pull(&env, &a));
    env.fake.change(&[(&files[0].0, Some(b"new\n"))]);
    env.fake.act(
        "v2 changes",
        Act::Refuse {
            status: 404,
            error: "not_found",
            retry_after: None,
        },
    );
    env.fake.clear();
    ok(&sync(&env, &a));
    assert_eq!(counted(&env), vec!["v2 changes", "v2 tree", "v2 read"]);
    assert_eq!(std::fs::read(a.join(&files[0].0)).unwrap(), b"new\n");
    assert!(
        bases(&env)
            .iter()
            .any(|b| b["commit_sha"] == env.fake.head().sha.as_str())
    );
    env.fake.act(
        "v2 changes",
        Act::Refuse {
            status: 422,
            error: "validation_error",
            retry_after: None,
        },
    );
    env.fake.change(&[(&files[1].0, Some(b"two\n"))]);
    env.fake.clear();
    ok(&sync(&env, &a));
    assert_eq!(counted(&env), vec!["v2 changes", "v2 tree", "v2 read"]);
}

#[test]
fn r10_a_masked_view_is_refreshed_by_its_tag() {
    let env = Env::new();
    let mut files: Files = many(10)
        .into_iter()
        .map(|(p, b)| (p, Entry::file(&b)))
        .collect();
    files.insert("private/secret.md".into(), Entry::file(b"secret\n"));
    env.fake.commit_entries(files);
    env.fake.with(|s| s.hidden = vec!["private".into()]);
    let a = checkout(&env, "a");
    ok(&pull(&env, &a));
    assert!(!a.join("private/secret.md").exists());
    env.fake.clear();
    ok(&sync(&env, &a));
    let log = env.fake.log();
    assert_eq!(counted(&env), vec!["v2 tree"]);
    assert_eq!(log[0].status, 304);
    assert!(
        log[0]
            .header("if-none-match")
            .unwrap()
            .contains(":closure:")
    );
    // The folder made public: a `200`, and the checkout as v1 leaves it.
    env.fake.with(|s| s.hidden.clear());
    env.fake.clear();
    ok(&sync(&env, &a));
    assert_eq!(counted(&env), vec!["v2 tree", "v2 read"]);
    assert_eq!(
        std::fs::read(a.join("private/secret.md")).unwrap(),
        b"secret\n"
    );
}

#[test]
fn r14_links_and_submodules_are_neither_written_nor_removed() {
    same_as_v1(|env| {
        let mut files: Files = BTreeMap::new();
        files.insert("a.md".into(), Entry::file(b"a\n"));
        files.insert(
            "run.sh".into(),
            Entry {
                bytes: b"#!/bin/sh\n".to_vec(),
                mode: "100755",
            },
        );
        files.insert(
            "link".into(),
            Entry {
                bytes: b"a.md".to_vec(),
                mode: "120000",
            },
        );
        files.insert(
            "sub".into(),
            Entry {
                bytes: "c".repeat(40).into_bytes(),
                mode: "160000",
            },
        );
        env.fake.commit_entries(files);
        let a = checkout(env, "a");
        let first = pull(env, &a);
        // A `changes` adding a link and a submodule.
        env.fake.with(|s| {
            let mut files = s.commits.last().unwrap().files.clone();
            files.insert(
                "link2".into(),
                Entry {
                    bytes: b"run.sh".to_vec(),
                    mode: "120000",
                },
            );
            files.insert(
                "sub2".into(),
                Entry {
                    bytes: "d".repeat(40).into_bytes(),
                    mode: "160000",
                },
            );
            files.insert("b.md".into(), Entry::file(b"b\n"));
            s.add(files);
        });
        vec![first, sync(env, &a), env.run(&a, &["--json", "status"])]
    });
}

#[test]
fn r15_a_closure_failing_its_check_is_resent_then_refused() {
    let env = Env::new();
    env.fake.commit(&as_refs(&many(10)));
    for _ in 0..4 {
        env.fake.act("v2 tree", Act::CorruptClosure);
    }
    let a = checkout(&env, "a");
    let out = env.run_env(&a, &[("_INTERNAL_SYNS_WAIT_PERCENT", "1")], &["pull"]);
    assert!(!out.status.success());
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("RESPONSE_UNDECODABLE")
            || String::from_utf8_lossy(&out.stderr).contains("invalid response body"),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(counted(&env), vec!["v2 tree"; 4]);
    assert_eq!(files_of(&a).len(), 1, "only the identity file");
    assert!(
        !env.store().join("trees").exists() || files_under(&env.store().join("trees")).is_empty()
    );
    assert!(views(&env).is_empty());
    // Resent once and healthy: converges.
    env.fake.act("v2 tree", Act::CorruptClosure);
    env.fake.clear();
    ok(&env.run_env(&a, &[("_INTERNAL_SYNS_WAIT_PERCENT", "1")], &["pull"]));
    assert_eq!(&counted(&env)[..2], &["v2 tree", "v2 tree"]);
}

#[test]
fn r15_a_closure_cut_inside_its_last_block_is_resent() {
    let env = Env::new();
    env.fake.commit(&as_refs(&many(30)));
    // An answer cut part-way is an answer that did not arrive: resent.
    env.fake.act("v2 tree", Act::CutAfterBytes(200));
    let a = checkout(&env, "a");
    ok(&env.run_env(&a, &[("_INTERNAL_SYNS_WAIT_PERCENT", "1")], &["pull"]));
    assert_eq!(&counted(&env)[..2], &["v2 tree", "v2 tree"]);
    assert_eq!(files_of(&a).len(), 31);
}

#[test]
fn r16_a_frame_whose_bytes_hash_otherwise_ends_the_run() {
    let env = Env::new();
    let files = many(10);
    env.fake.commit(&as_refs(&files));
    env.fake.act("v2 read", Act::Corrupt(files[2].0.clone()));
    let a = checkout(&env, "a");
    let out = pull(&env, &a);
    assert!(!out.status.success());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains(&files[2].0), "{stderr}");
    assert_eq!(env.fake.reads().len(), 1, "not resent");
    assert_eq!(files_of(&a).len(), 1);
    let mut bad = files[2].1.clone();
    bad[0] ^= 1;
    assert!(!env.store().join("blobs").join(blob(&bad)).exists());
}

#[test]
fn r17_large_files_are_read_alone() {
    let env = Env::new();
    let big = vec![7u8; 9 << 20];
    let mut list: Vec<(String, Vec<u8>)> = vec![("big.bin".into(), big.clone())];
    for i in 0..5 {
        list.push((format!("seven{i}.bin"), vec![i as u8; 7 << 20]));
    }
    list.push(("small.md".into(), b"s\n".to_vec()));
    env.fake.commit(&as_refs(&list));
    let a = checkout(&env, "a");
    ok(&pull(&env, &a));
    let reads = env.fake.reads();
    assert!(
        reads
            .iter()
            .all(|r| !r.asked().contains(&"big.bin".to_string()))
    );
    assert_eq!(reads.len(), 2, "five 7 MiB files packed two chunks apart");
    let raw: Vec<_> = env
        .fake
        .log()
        .into_iter()
        .filter(|s| s.route == "v1 raw")
        .collect();
    assert_eq!(raw.len(), 1);
    assert_eq!(raw[0].path, "big.bin");
    assert_eq!(std::fs::read(a.join("big.bin")).unwrap(), big);
    assert!(env.store().join("blobs").join(blob(&big)).is_file());

    // `too_large` anyway: asked again in a later chunk, then read alone.
    let env = Env::new();
    env.fake.commit(&as_refs(&list[1..]));
    env.fake
        .act("v2 read", Act::TooLarge(vec!["seven4.bin".into()]));
    env.fake
        .act("v2 read", Act::TooLarge(vec!["seven4.bin".into()]));
    env.fake
        .act("v2 read", Act::TooLarge(vec!["seven4.bin".into()]));
    let a = checkout(&env, "a");
    ok(&pull(&env, &a));
    let raw: Vec<_> = env
        .fake
        .log()
        .into_iter()
        .filter(|s| s.route == "v1 raw")
        .collect();
    assert_eq!(raw.len(), 1);
    assert_eq!(raw[0].path, "seven4.bin");
    assert_eq!(std::fs::read(a.join("seven4.bin")).unwrap(), list[5].1);
}

// ---- per-chunk retry ----------------------------------------------------------

#[test]
fn r19_an_injected_chunk_cut_retries_and_converges() {
    let env = Env::new();
    let files = many(1201);
    env.fake.commit(&as_refs(&files));
    // Chunk 2 of 3, by the first path it asks.
    env.fake.act(
        &format!("v2 read {}", chunk_start(&env, 1)),
        Act::CutAfterFrames(120),
    );
    let a = checkout(&env, "a");
    ok(&pull(&env, &a));
    let reads = env.fake.reads();
    assert_eq!(reads.len(), 4, "chunk 2 sent twice, the others once");
    let first = chunk_start(&env, 1);
    let cut = reads
        .iter()
        .find(|r| r.asked().first() == Some(&first))
        .unwrap();
    assert_eq!(cut.asked().len(), 500);
    let again: Vec<_> = reads
        .iter()
        .filter(|r| {
            r.asked().iter().any(|p| cut.asked()[120..].contains(p)) && r.asked() != cut.asked()
        })
        .collect();
    assert_eq!(again.len(), 1);
    assert_eq!(
        again[0].asked(),
        cut.asked()[120..].to_vec(),
        "only the paths after the 120th"
    );
    let written = files_of(&a);
    for (p, b) in &files {
        assert_eq!(written.get(p), Some(b));
    }
}

#[test]
#[serial]
fn r20_a_503_engine_busy_with_retry_after_1_for_roadmap_271_and_284() {
    for (status, error) in [(503u16, "engine_busy"), (429, "rate_limited")] {
        let env = Env::new();
        env.fake.commit(&as_refs(&many(1201)));
        env.fake.act(
            &format!("v2 read {}", chunk_start(&env, 1)),
            Act::Refuse {
                status,
                error,
                retry_after: Some(1),
            },
        );
        let a = checkout(&env, "a");
        ok(&pull(&env, &a));
        let reads = env.fake.reads();
        assert_eq!(reads.len(), 4);
        let refused = reads.iter().find(|r| r.status == status).unwrap();
        let resent = reads
            .iter()
            .find(|r| r.status == 200 && r.asked() == refused.asked())
            .unwrap();
        let wait = resent.start - refused.end.unwrap();
        assert!(wait >= Duration::from_secs(1), "{wait:?}");
        assert!(wait <= Duration::from_millis(1700), "{wait:?}");
        assert_eq!(files_of(&a).len(), 1202);
    }
    // `Retry-After: 11`: not resent; the base binary's error and exit.
    let env = Env::new();
    env.fake.commit(&as_refs(&many(10)));
    env.fake.act(
        "v2 read",
        Act::Refuse {
            status: 503,
            error: "engine_busy",
            retry_after: Some(11),
        },
    );
    let a = checkout(&env, "a");
    let out = pull(&env, &a);
    assert!(!out.status.success());
    assert_eq!(env.fake.reads().len(), 1);
    let v1 = Env::new();
    v1.fake.v2(false);
    v1.fake.commit(&as_refs(&many(10)));
    v1.fake.act(
        "v1 raw",
        Act::Refuse {
            status: 503,
            error: "engine_busy",
            retry_after: Some(11),
        },
    );
    let b = checkout(&v1, "a");
    let base = pull(&v1, &b);
    assert_eq!(out.status.code(), base.status.code());
    assert_eq!(
        transcript(&out, env.fake.port).1,
        transcript(&base, v1.fake.port).1
    );
    // The same for the closure and `changes`.
    let env = Env::new();
    env.fake.commit(&as_refs(&many(10)));
    env.fake.act(
        "v2 tree",
        Act::Refuse {
            status: 503,
            error: "engine_busy",
            retry_after: Some(1),
        },
    );
    let a = checkout(&env, "a");
    ok(&pull(&env, &a));
    assert_eq!(&counted(&env)[..2], &["v2 tree", "v2 tree"]);
    env.fake.act(
        "v2 changes",
        Act::Refuse {
            status: 429,
            error: "rate_limited",
            retry_after: Some(1),
        },
    );
    env.fake.clear();
    ok(&sync(&env, &a));
    assert_eq!(counted(&env), vec!["v2 changes", "v2 changes"]);
}

#[test]
fn r21_four_failure_sends_then_the_run_ends_keeping_what_it_checked() {
    let env = Env::new();
    let files = many(1201);
    env.fake.commit(&as_refs(&files));
    let second = format!("v2 read {}", chunk_start(&env, 1));
    for _ in 0..4 {
        env.fake.act(&second, Act::Close);
    }
    let a = checkout(&env, "a");
    let out = env.run_env(&a, &[("_INTERNAL_SYNS_WAIT_PERCENT", "1")], &["pull"]);
    assert_eq!(
        out.status.code(),
        Some(3),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    // SERVER_UNREACHABLE's line, naming the batch's address.
    assert!(
        String::from_utf8_lossy(&out.stderr)
            .contains("error: could not reach server at http://127.0.0.1:"),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(files_of(&a).len(), 1, "no checkout file written");
    assert!(bases(&env).is_empty(), "no base recorded");
    let closed = env.fake.reads().iter().filter(|r| r.status == 0).count();
    assert_eq!(closed, 4);
    // A re-run against a healthy server asks only the paths not stored.
    let stored: usize = std::fs::read_dir(env.store().join("blobs"))
        .unwrap()
        .count();
    assert!(stored >= 500);
    env.fake.clear();
    ok(&pull(&env, &a));
    let asked: usize = env.fake.reads().iter().map(|r| r.asked().len()).sum();
    assert_eq!(asked, 1201 - stored);
    assert_eq!(files_of(&a).len(), 1202);
}

#[test]
fn r21a_progress_cuts_cost_no_failure_send() {
    let env = Env::new();
    env.fake.commit(&as_refs(&many(499)));
    for n in [10usize, 20, 30, 40, 50, 60] {
        env.fake.act("v2 read", Act::CutAfterFrames(n));
    }
    let a = checkout(&env, "a");
    ok(&pull(&env, &a));
    assert_eq!(env.fake.reads().len(), 7);
    assert_eq!(files_of(&a).len(), 500);
}

#[test]
fn r21a_twenty_waits_converge() {
    let env = Env::new();
    env.fake.commit(&as_refs(&many(20)));
    for _ in 0..20 {
        env.fake.act(
            "v2 read",
            Act::Refuse {
                status: 503,
                error: "engine_busy",
                retry_after: Some(1),
            },
        );
    }
    let a = checkout(&env, "a");
    ok(&env.run_env(&a, &[("_INTERNAL_SYNS_WAIT_PERCENT", "5")], &["pull"]));
    assert_eq!(env.fake.reads().len(), 21);
}

#[test]
#[serial]
fn r21a_waits_of_ten_seconds_give_up_on_the_chunk_budget() {
    let env = Env::new();
    env.fake.commit(&as_refs(&many(20)));
    for _ in 0..20 {
        env.fake.act(
            "v2 read",
            Act::Refuse {
                status: 503,
                error: "engine_busy",
                retry_after: Some(10),
            },
        );
    }
    let a = checkout(&env, "a");
    let started = Instant::now();
    let out = env.run_env(&a, &[("_INTERNAL_SYNS_WAIT_PERCENT", "2")], &["pull"]);
    assert!(!out.status.success());
    assert!(started.elapsed() < Duration::from_secs(10));
    let sends = env.fake.reads().len();
    assert!((5..=7).contains(&sends), "{sends}");
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("ENGINE_BUSY")
            || String::from_utf8_lossy(&out.stderr)
                .to_lowercase()
                .contains("busy"),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
}

#[test]
fn r22_refusals_are_not_retried() {
    for (status, error) in [
        (401u16, "unauthorized"),
        (403, "forbidden"),
        (413, "payload_too_large"),
        (422, "validation_error"),
        (500, "internal_error"),
    ] {
        let env = Env::new();
        env.fake.commit(&as_refs(&many(5)));
        env.fake.act(
            "v2 read",
            Act::Refuse {
                status,
                error,
                retry_after: None,
            },
        );
        let a = checkout(&env, "a");
        let out = pull(&env, &a);
        assert!(!out.status.success(), "{status}");
        assert_eq!(env.fake.reads().len(), 1, "{status}");
        let v1 = Env::new();
        v1.fake.v2(false);
        v1.fake.commit(&as_refs(&many(5)));
        for _ in 0..5 {
            v1.fake.act(
                "v1 raw",
                Act::Refuse {
                    status,
                    error,
                    retry_after: None,
                },
            );
        }
        let b = checkout(&v1, "a");
        let base = pull(&v1, &b);
        assert_eq!(out.status.code(), base.status.code(), "{status}");
        assert_eq!(
            transcript(&out, env.fake.port).1,
            transcript(&base, v1.fake.port).1,
            "{status}"
        );
    }
}

// ---- fallback (D-243 §7) -------------------------------------------------------

#[test]
fn r24_a_server_without_v2_is_today() {
    for absent in [Absent::Json, Absent::Empty, Absent::Html] {
        let env = Env::new();
        env.fake.v2(false);
        env.fake.with(|s| s.absent = Some(absent));
        let files = many(30);
        env.fake.commit(&as_refs(&files));
        let a = checkout(&env, "a");
        ok(&pull(&env, &a));
        let v2: usize = env.fake.log().iter().filter(|s| s.is_v2()).count();
        assert!(v2 <= 2, "{:?}", env.fake.routes());
        assert_eq!(files_of(&a).len(), 31);
        env.fake.change(&[(&files[0].0, Some(b"x\n"))]);
        env.fake.clear();
        ok(&sync(&env, &a));
        let v2: usize = env.fake.log().iter().filter(|s| s.is_v2()).count();
        assert!(v2 <= 2, "{:?}", env.fake.routes());
        assert_eq!(std::fs::read(a.join(&files[0].0)).unwrap(), b"x\n");
    }
}

#[test]
fn r25_refusals_stay_todays() {
    // An empty repository: v2 `422`.
    same_as_v1(|env| {
        let a = checkout(env, "a");
        vec![pull(env, &a), env.run(&a, &["--json", "pull"])]
    });
}

// ---- the store (D-243 §4) --------------------------------------------------------

#[test]
fn r31_the_layout() {
    let env = Env::on(Fake::bind(18279));
    env.fake.commit(&as_refs(&many(5)));
    let a = checkout(&env, "a");
    ok(&pull(&env, &a));
    let store = env.store();
    for area in ["blobs", "trees", "views"] {
        assert!(store.join(area).is_dir(), "{area}");
    }
    let empty_key = "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";
    let commit = env.fake.head().sha;
    assert!(
        store
            .join("views/1")
            .join(OWNER)
            .join(NAME)
            .join(empty_key)
            .join(&commit)
            .is_file()
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut pending = vec![store.clone()];
        while let Some(d) = pending.pop() {
            for entry in std::fs::read_dir(&d).unwrap().flatten() {
                let mode = entry.metadata().unwrap().permissions().mode() & 0o777;
                if entry.file_type().unwrap().is_dir() {
                    assert_eq!(mode, 0o700, "{:?}", entry.path());
                    pending.push(entry.path());
                } else {
                    assert_eq!(mode, 0o600, "{:?}", entry.path());
                }
            }
        }
    }
    // The purge walks every root (D-242 §8): each one this run reaches is
    // the Env's own, never the machine's (issue 364).
    let marker = Path::new("store/syns.dev/u-alice/marker");
    for root in [env.default_root(), env.fallback_root()] {
        std::fs::create_dir_all(root.join(marker).parent().unwrap()).unwrap();
        std::fs::write(root.join(marker), b"m").unwrap();
    }
    let out = env.run(&a, &["logout", "--purge"]);
    ok(&out);
    assert!(!env.cache.path().join("store").exists());
    for root in [env.default_root(), env.fallback_root()] {
        assert!(!root.join("store").exists(), "{}", root.display());
    }
}

#[test]
fn r32_the_cap_counts_blobs_trees_and_views() {
    let env = Env::new();
    let files: Vec<(String, Vec<u8>)> = (0..40)
        .map(|i| (format!("f{i:02}.bin"), vec![i as u8; 4096]))
        .collect();
    env.fake.commit(&as_refs(&files));
    let a = checkout(&env, "a");
    let out = env.run_env(&a, &[("SYNS_CACHE_MAX_BYTES", "60000")], &["pull"]);
    ok(&out);
    let size: u64 = std::fs::read_to_string(env.store().join("size"))
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    let mut total = 0u64;
    for area in ["blobs", "trees", "views"] {
        let mut pending = vec![env.store().join(area)];
        while let Some(d) = pending.pop() {
            for entry in std::fs::read_dir(&d).into_iter().flatten().flatten() {
                if entry.file_type().unwrap().is_dir() {
                    pending.push(entry.path());
                } else {
                    total += entry.metadata().unwrap().len();
                }
            }
        }
    }
    assert_eq!(size, total);
    assert!(total <= 54000, "{total}");
    assert_eq!(files_of(&a).len(), 41);
}

#[test]
fn r32a_views_are_bounded_by_count() {
    let env = Env::new();
    let files = many(5);
    env.fake.commit(&as_refs(&files));
    let a = checkout(&env, "a");
    ok(&pull(&env, &a));
    for i in 0..10 {
        env.fake
            .change(&[(&files[0].0, Some(format!("v{i}\n").as_bytes()))]);
        ok(&sync(&env, &a));
    }
    assert_eq!(views(&env).len(), 8);
}

#[test]
fn r33a_the_24_hour_refresh() {
    let env = Env::new();
    let files = many(5);
    env.fake.commit(&as_refs(&files));
    let a = checkout(&env, "a");
    ok(&pull(&env, &a));
    // Each sync a minute short of its 3-hour mark, so sync 8 stands at
    // 24 h after the closure and sync 9 past it.
    let at = |hours: u64| {
        (
            "_INTERNAL_SYNS_CLOCK_OFFSET",
            (hours * 3600 - 60).to_string(),
        )
    };
    for sync_n in 1..=10u64 {
        env.fake
            .change(&[(&files[0].0, Some(format!("s{sync_n}\n").as_bytes()))]);
        env.fake.clear();
        let (k, v) = at(sync_n * 3);
        ok(&env.run_env(&a, &[(k, v.as_str())], &["sync"]));
        let routes = counted(&env);
        if sync_n == 9 {
            assert_eq!(routes, vec!["v2 tree", "v2 read"], "sync {sync_n}");
        } else {
            assert_eq!(routes, vec!["v2 changes", "v2 read"], "sync {sync_n}");
        }
    }
}

#[test]
#[serial]
fn r34_concurrent_fresh_checkouts() {
    let env = Env::new();
    let files = many(200);
    env.fake.commit(&as_refs(&files));
    let dirs: Vec<PathBuf> = (0..16).map(|i| checkout(&env, &format!("c{i}"))).collect();
    let children: Vec<_> = dirs
        .iter()
        .map(|d| env.command(d, &["pull"]).spawn().unwrap())
        .collect();
    for mut child in children {
        assert!(child.wait().unwrap().success());
    }
    for d in &dirs {
        assert_eq!(files_of(d).len(), 201);
    }
    for (_, bytes) in &files {
        assert_eq!(
            std::fs::read(env.store().join("blobs").join(blob(bytes))).unwrap(),
            *bytes
        );
    }
    let size: u64 = std::fs::read_to_string(env.store().join("size"))
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    let mut total = 0u64;
    for area in ["blobs", "trees", "views"] {
        let mut pending = vec![env.store().join(area)];
        while let Some(d) = pending.pop() {
            for entry in std::fs::read_dir(&d).into_iter().flatten().flatten() {
                if entry.file_type().unwrap().is_dir() {
                    pending.push(entry.path());
                } else if !entry.file_name().to_string_lossy().starts_with(".tmp-") {
                    total += entry.metadata().unwrap().len();
                }
            }
        }
    }
    assert_eq!(size, total);
}

#[test]
fn r36_status_output_equals_v1s() {
    same_as_v1(|env| {
        let files = many(10);
        env.fake.commit(&as_refs(&files));
        let a = checkout(env, "a");
        let first = pull(env, &a);
        std::fs::write(a.join(&files[0].0), b"local\n").unwrap();
        env.fake.change(&[(&files[1].0, Some(b"remote\n"))]);
        vec![
            first,
            env.run(&a, &["status"]),
            env.run(&a, &["--json", "status"]),
            sync(env, &a),
            env.run(&a, &["--json", "status"]),
        ]
    });
}

// ---- more of the view, the store and convergence ----------------------------

/// Rewrites a view's file with `edit`, its digest kept honest unless
/// `honest` is unset.
fn edit_view(path: &Path, edit: impl Fn(&str) -> String) {
    let text = std::fs::read_to_string(path).unwrap();
    std::fs::write(path, edit(&text)).unwrap();
}

#[test]
fn r15a_a_view_failing_its_check_sends_the_run_to_the_closure() {
    type Damage = fn(&Path, &str);
    let damages: [(&str, Damage); 4] = [
        ("truncated", |p, _| {
            let bytes = std::fs::read(p).unwrap();
            std::fs::write(p, &bytes[..bytes.len() / 2]).unwrap();
        }),
        ("one entry byte", |p, _| {
            let mut bytes = std::fs::read(p).unwrap();
            let at = bytes.len() - 5;
            bytes[at] ^= 1;
            std::fs::write(p, bytes).unwrap();
        }),
        ("another format", |p, commit| {
            let dir = p.parent().unwrap();
            let moved = dir.to_string_lossy().replace("/views/1/", "/views/2/");
            std::fs::create_dir_all(&moved).unwrap();
            std::fs::rename(p, Path::new(&moved).join(commit)).unwrap();
        }),
        ("another commit", |p, commit| {
            edit_view(p, |t| t.replacen(commit, &"0".repeat(40), 1));
        }),
    ];
    for (name, damage) in damages {
        let run = |v2: bool, damage: Option<Damage>| {
            let env = Env::new();
            env.fake.v2(v2);
            let files = many(10);
            let base = env.fake.commit(&as_refs(&files));
            let a = checkout(&env, "a");
            let first = pull(&env, &a);
            if let Some(damage) = damage {
                damage(&view_of(&env, &base), &base);
            }
            env.fake.change(&[(&files[2].0, Some(b"two\n"))]);
            env.fake.clear();
            let second = sync(&env, &a);
            let routes = counted(&env);
            (
                vec![
                    transcript(&first, env.fake.port),
                    transcript(&second, env.fake.port),
                ],
                files_of(&a),
                routes,
            )
        };
        let (t2, f2, routes) = run(true, Some(damage));
        assert_eq!(routes, vec!["v2 tree", "v2 read"], "{name}");
        let (t1, f1, _) = run(false, None);
        assert_eq!(t2, t1, "{name}");
        assert_eq!(f2, f1, "{name}");
    }
}

#[test]
fn r15a_a_view_with_a_link_and_a_submodule_survives_changes() {
    let env = Env::new();
    let mut files: Files = BTreeMap::new();
    files.insert(".syns.yaml".into(), Entry::file(IDENTITY.as_bytes()));
    files.insert("a.md".into(), Entry::file(b"a\n"));
    files.insert(
        "link".into(),
        Entry {
            bytes: b"a.md".to_vec(),
            mode: "120000",
        },
    );
    files.insert(
        "sub".into(),
        Entry {
            bytes: "c".repeat(40).into_bytes(),
            mode: "160000",
        },
    );
    env.fake.commit_entries(files.clone());
    let a = checkout(&env, "a");
    ok(&pull(&env, &a));
    files.insert(
        "link".into(),
        Entry {
            bytes: b".syns.yaml".to_vec(),
            mode: "120000",
        },
    );
    files.remove("sub");
    files.insert("b.md".into(), Entry::file(b"b\n"));
    env.fake.commit_entries(files);
    env.fake.clear();
    ok(&sync(&env, &a));
    assert_eq!(counted(&env), vec!["v2 changes", "v2 read"]);
    assert!(!a.join("link").exists());
    assert_eq!(std::fs::read(a.join("b.md")).unwrap(), b"b\n");
    // And the next no-op is one request.
    env.fake.clear();
    ok(&sync(&env, &a));
    assert_eq!(counted(&env), vec!["v2 changes"]);
}

#[test]
fn r18_base_contents_for_a_collision_in_one_batch() {
    let scenario = |env: &Env| {
        let base_text = b"one\ntwo\nthree\n";
        env.fake
            .commit(&[("a.md", base_text), ("b.md", b"b\n"), ("c.md", b"c\n")]);
        let a = checkout(env, "a");
        let first = pull(env, &a);
        std::fs::write(a.join("a.md"), b"one\nLOCAL\nthree\n").unwrap();
        env.fake.change(&[("a.md", Some(b"one\ntwo\nREMOTE\n"))]);
        vec![first, env.run(&a, &["--json", "sync"])]
    };
    same_as_v1(scenario);
    let env = Env::new();
    let outs = scenario(&env);
    assert_eq!(
        outs[1].status.code(),
        Some(4),
        "{}",
        String::from_utf8_lossy(&outs[1].stderr)
    );
    // The sync: `changes`, the head's batch, and one batch at the base
    // commit (the pull's own read first).
    assert_eq!(
        env.fake.routes(),
        vec!["v2 tree", "v2 read", "v2 changes", "v2 read", "v2 read"]
    );
    let reads = env.fake.reads();
    let base = env.fake.with(|s| s.commits[0].sha.clone());
    assert_eq!(reads[2].json()["at"], base.as_str());
    assert_eq!(reads[2].asked(), vec!["a.md"]);
}

#[test]
fn r33_a_path_the_view_carries_is_asked_with_have_under_overwrite() {
    let env = Env::new();
    let files = many(5);
    env.fake.commit(&as_refs(&files));
    let a = checkout(&env, "a");
    ok(&pull(&env, &a));
    let (p, bytes) = &files[1];
    std::fs::remove_file(a.join(p)).unwrap();
    env.fake.change(&[("other.md", Some(b"o\n"))]);
    env.fake.clear();
    ok(&env.run(&a, &["pull", "--overwrite"]));
    assert_eq!(std::fs::read(a.join(p)).unwrap(), *bytes);
    let reads = env.fake.reads();
    let asked: Vec<serde_json::Value> = reads
        .iter()
        .flat_map(|r| r.json()["paths"].as_array().unwrap().clone())
        .collect();
    let entry = asked.iter().find(|e| e["path"] == p.as_str()).unwrap();
    assert_eq!(entry["have"], blob(bytes));

    // Now hidden from the reader: `not_found`, the view dropped, and the
    // same run reads the closure and completes as v1 does.
    let run = |v2: bool| {
        let env = Env::new();
        env.fake.v2(v2);
        let files = many(5);
        env.fake.commit(&as_refs(&files));
        let a = checkout(&env, "a");
        let first = pull(&env, &a);
        let (p, _) = &files[1];
        std::fs::remove_file(a.join(p)).unwrap();
        let folder = p.split('/').next().unwrap().to_string();
        env.fake.with(|s| s.hidden = vec![folder]);
        env.fake.change(&[("other.md", Some(b"o\n"))]);
        env.fake.clear();
        let second = env.run(&a, &["pull", "--overwrite"]);
        (
            vec![
                transcript(&first, env.fake.port),
                transcript(&second, env.fake.port),
            ],
            files_of(&a),
            counted(&env),
        )
    };
    let v2 = run(true);
    let v1 = run(false);
    assert_eq!(v2.0, v1.0);
    assert_eq!(v2.1, v1.1);
    assert!(v2.2.contains(&"v2 tree".to_string()), "{:?}", v2.2);
}

#[test]
fn r32_a_swept_base_view_reads_the_closure_once() {
    let env = Env::new();
    let files = many(5);
    env.fake.commit(&as_refs(&files));
    let a = checkout(&env, "a");
    ok(&pull(&env, &a));
    for view in views(&env) {
        std::fs::remove_file(view).unwrap();
    }
    env.fake.clear();
    ok(&sync(&env, &a));
    assert_eq!(counted(&env), vec!["v2 tree"]);
    env.fake.clear();
    ok(&sync(&env, &a));
    assert_eq!(counted(&env), vec!["v2 changes"]);
}

#[test]
fn r33a_a_fresh_checkout_at_a_derived_view_s_commit_gets_a_304() {
    let env = Env::new();
    let files = many(5);
    env.fake.commit(&as_refs(&files));
    let a = checkout(&env, "a");
    ok(&pull(&env, &a));
    env.fake.change(&[(&files[0].0, Some(b"changed\n"))]);
    ok(&sync(&env, &a));
    env.fake.clear();
    let b = checkout(&env, "b");
    ok(&pull(&env, &b));
    let log = env.fake.log();
    assert_eq!(log.len(), 1);
    assert_eq!(log[0].status, 304);
    assert_eq!(files_of(&b), files_of(&a));
}

#[test]
fn r24_a_v2_answer_of_another_shape_reads_as_no_v2() {
    // A fallback route answering every unknown address with a record, as
    // some test servers do: read as a server answering no v2.
    let env = Env::new();
    let files = many(5);
    env.fake.commit(&as_refs(&files));
    env.fake.with(|s| s.v2_record = true);
    let a = checkout(&env, "a");
    ok(&pull(&env, &a));
    assert_eq!(files_of(&a).len(), 6);
    env.fake.change(&[(&files[0].0, Some(b"x\n"))]);
    ok(&sync(&env, &a));
    assert_eq!(std::fs::read(a.join(&files[0].0)).unwrap(), b"x\n");
}

#[test]
fn r35_convergence_scenarios_equal_v1s() {
    same_as_v1(|env| {
        let files = many(12);
        env.fake.commit(&as_refs(&files));
        let a = checkout(env, "a");
        let mut outs = vec![pull(env, &a), sync(env, &a)];
        // A remote edit.
        env.fake.change(&[(&files[0].0, Some(b"remote\n"))]);
        outs.push(sync(env, &a));
        // A local edit published.
        std::fs::write(a.join(&files[1].0), b"local\n").unwrap();
        outs.push(sync(env, &a));
        // A delete/modify collision.
        std::fs::remove_file(a.join(&files[2].0)).unwrap();
        env.fake.change(&[(&files[2].0, Some(b"remote edit\n"))]);
        outs.push(env.run(&a, &["--json", "sync"]));
        outs.push(env.run(&a, &["resolution", "discard"]));
        // `--overwrite`.
        std::fs::write(a.join(&files[3].0), b"scratch\n").unwrap();
        outs.push(env.run(&a, &["pull", "--overwrite"]));
        outs.push(env.run(&a, &["--json", "status"]));
        outs
    });
}

// ---- indicative cost (not a row: the verifier's rows 38 and 39) -----------

#[cfg(unix)]
fn child_cpu() -> (Duration, Duration) {
    let mut usage: libc::rusage = unsafe { std::mem::zeroed() };
    unsafe { libc::getrusage(libc::RUSAGE_CHILDREN, &mut usage) };
    let tv =
        |t: libc::timeval| Duration::from_micros(t.tv_sec as u64 * 1_000_000 + t.tv_usec as u64);
    (tv(usage.ru_utime), tv(usage.ru_stime))
}

/// A fresh cold pull of 4,559 files, then a warm one: wall and CPU on
/// the local fake. Run with `--ignored --nocapture`, best in release.
#[cfg(unix)]
#[test]
#[ignore]
fn indicative_cold_and_warm_pull_of_4559_files() {
    for v2 in [true, false] {
        let env = Env::new();
        env.fake.v2(v2);
        let files: Vec<(String, Vec<u8>)> = (0..4558)
            .map(|i| {
                let size = 200 + (i * 7919) % 12000;
                (
                    format!("d{}/s{}/f{i:05}.md", i % 37, i % 11),
                    vec![b'a' + (i % 26) as u8; size],
                )
            })
            .collect();
        env.fake.commit(&as_refs(&files));
        for (name, dir) in [("cold", "a"), ("warm", "b"), ("no-op sync", "a")] {
            let d = if name == "no-op sync" {
                env.root.path().join(dir)
            } else {
                checkout(&env, dir)
            };
            env.fake.clear();
            let (u0, s0) = child_cpu();
            let started = Instant::now();
            if name == "no-op sync" {
                ok(&sync(&env, &d));
            } else {
                ok(&pull(&env, &d));
            }
            let wall = started.elapsed();
            let (u1, s1) = child_cpu();
            eprintln!(
                "v2 {v2} {name}: wall {wall:?}, user {:?}, sys {:?}, requests {}",
                u1 - u0,
                s1 - s0,
                env.fake.routes().len()
            );
        }
    }
}

#[test]
fn r21a_the_run_window_bounds_every_wait() {
    let env = Env::new();
    env.fake.commit(&as_refs(&many(4999)));
    for _ in 0..200 {
        env.fake.act(
            "v2 read",
            Act::Refuse {
                status: 503,
                error: "engine_busy",
                retry_after: Some(10),
            },
        );
    }
    let a = checkout(&env, "a");
    // At 2 %: waits of 200–300 ms, 1.2 s per chunk, a window of 2.4 s.
    let started = Instant::now();
    let out = env.run_env(&a, &[("_INTERNAL_SYNS_WAIT_PERCENT", "2")], &["pull"]);
    assert!(!out.status.success());
    let reads = env.fake.reads();
    let first_wait = reads[0].end.unwrap();
    let last = reads.iter().map(|r| r.start).max().unwrap();
    assert!(
        last - first_wait <= Duration::from_millis(2400 + 300),
        "{:?}",
        last - first_wait
    );
    assert!(started.elapsed() < Duration::from_secs(30));
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("engine_busy")
            || String::from_utf8_lossy(&out.stderr)
                .to_lowercase()
                .contains("busy")
    );
}

#[test]
fn r31_a_read_verb_records_the_head() {
    let env = Env::new();
    env.fake.commit(&as_refs(&many(3)));
    let a = checkout(&env, "a");
    ok(&env.run(&a, &["cat", ".syns.yaml"]));
    let head = std::fs::read_to_string(env.store().join("heads").join(OWNER).join(NAME)).unwrap();
    assert_eq!(head.trim(), format!("{} 1", env.fake.head().sha));
}

#[test]
fn r32a_a_copy_whose_base_view_was_bounded_out_reads_the_closure_once() {
    let env = Env::new();
    let files = many(5);
    env.fake.commit(&as_refs(&files));
    let a = checkout(&env, "a");
    let b = checkout(&env, "b");
    ok(&pull(&env, &a));
    ok(&pull(&env, &b));
    for i in 0..9 {
        env.fake
            .change(&[(&files[0].0, Some(format!("v{i}\n").as_bytes()))]);
        ok(&sync(&env, &a));
    }
    assert_eq!(views(&env).len(), 8);
    env.fake.clear();
    ok(&sync(&env, &b));
    assert_eq!(counted(&env), vec!["v2 tree"]);
    assert_eq!(files_of(&b), files_of(&a));
}

// ---- fix round 1 (REVIEW-syns-cli I3) ----------------------------------------

#[test]
fn i3_an_unchanged_frame_at_another_hash_is_refused_and_writes_nothing() {
    let env = Env::new();
    let files = many(10);
    env.fake.commit(&as_refs(&files));
    let a = checkout(&env, "a");
    let b = checkout(&env, "b");
    ok(&pull(&env, &a));
    ok(&pull(&env, &b));
    env.fake.change(&[(&files[3].0, Some(b"teammate\n"))]);
    ok(&sync(&env, &a));
    // b holds the new blob, so asks with `have`; the server answers
    // `unchanged` naming another blob the store also holds.
    env.fake.act(
        "v2 read",
        Act::UnchangedOther(files[3].0.clone(), blob(&files[4].1)),
    );
    let out = sync(&env, &b);
    assert!(
        !out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("invalid response body"),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(std::fs::read(b.join(&files[3].0)).unwrap(), files[3].1);
    // A healthy answer then converges.
    ok(&sync(&env, &b));
    assert_eq!(std::fs::read(b.join(&files[3].0)).unwrap(), b"teammate\n");
}

#[test]
fn i3_a_frame_with_no_mode_is_no_answer_and_removes_nothing() {
    let env = Env::new();
    let files = many(10);
    env.fake.commit(&as_refs(&files));
    let a = checkout(&env, "a");
    ok(&pull(&env, &a));
    env.fake.change(&[(&files[2].0, Some(b"edited\n"))]);
    for _ in 0..8 {
        env.fake.act("v2 read", Act::NoMode(files[2].0.clone()));
    }
    let out = env.run_env(&a, &[("_INTERNAL_SYNS_WAIT_PERCENT", "1")], &["sync"]);
    assert!(!out.status.success());
    assert_eq!(
        std::fs::read(a.join(&files[2].0)).unwrap(),
        files[2].1,
        "not removed"
    );
    assert_eq!(
        env.fake.reads().len() - 1,
        4,
        "resent as an answer that did not arrive"
    );
    env.fake.with(|s| s.acts.clear());
    ok(&sync(&env, &a));
    assert_eq!(std::fs::read(a.join(&files[2].0)).unwrap(), b"edited\n");
}

// ---- fix round 2 (VERIFY D2, row 44) -------------------------------------------

/// The bytes under each of the store's areas.
fn area_bytes(env: &Env, area: &str) -> u64 {
    let mut total = 0;
    let mut pending = vec![env.store().join(area)];
    while let Some(d) = pending.pop() {
        for entry in std::fs::read_dir(&d).into_iter().flatten().flatten() {
            if entry.file_type().unwrap().is_dir() {
                pending.push(entry.path());
            } else {
                total += entry.metadata().unwrap().len();
            }
        }
    }
    total
}

#[test]
fn r44_views_do_not_crowd_out_blobs() {
    let env = Env::new();
    // A `scale`-like copy: small files in 60 folders, so one view weighs
    // about a quarter of the blobs.
    let files: Vec<(String, Vec<u8>)> = (0..600)
        .map(|i| {
            let mut bytes = format!("file {i:04}\n").into_bytes();
            bytes.resize(300, b'a' + (i % 26) as u8);
            (format!("f{:02}/n{i:04}.md", i % 60), bytes)
        })
        .collect();
    env.fake.commit(&as_refs(&files));
    let blob_bytes: u64 =
        files.iter().map(|(_, b)| b.len() as u64).sum::<u64>() + IDENTITY.len() as u64;
    let cap = (blob_bytes * 3 / 2).to_string();
    let cap = [("SYNS_CACHE_MAX_BYTES", cap.as_str())];
    let a = checkout(&env, "a");
    ok(&env.run_env(&a, &cap, &["pull"]));
    let view_bytes = area_bytes(&env, "views");
    assert!(
        view_bytes * 5 > blob_bytes,
        "one view is a fifth of the blobs or more"
    );
    for i in 0..50 {
        let (path, _) = &files[i * 7 % files.len()];
        env.fake
            .change(&[(path, Some(format!("edit {i}\n").as_bytes()))]);
        ok(&env.run_env(&a, &cap, &["sync"]));
    }
    eprintln!(
        "blobs {}, trees {}, views {}",
        area_bytes(&env, "blobs"),
        area_bytes(&env, "trees"),
        area_bytes(&env, "views")
    );
    assert!(views(&env).len() <= 8);
    env.fake.clear();
    let b = checkout(&env, "b");
    ok(&env.run_env(&b, &cap, &["pull"]));
    assert!(
        env.fake.reads().is_empty(),
        "the fresh pull read blobs: {:?}",
        env.fake.routes()
    );
    assert_eq!(files_of(&b), files_of(&a));
}
