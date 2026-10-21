//! Roadmap 279: a folder copy bound to its holder over v2 — `changes` at
//! the holder, the holder's `.synsignore` from the base commit's view, and
//! the snapshot pull (FEATURE_SPEC.md § Expected rows 11, 13; D-243 §7,
//! §10).

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use crate::integration::v2_fake::{Entry, Env, Files, IDENTITY, files_of, ok, transcript};

const FOLDER: &str = "clients/q3";
const MARKER: &str = "holder: alice/notes\npath: clients/q3\n";

fn holder_files() -> Files {
    let mut files = BTreeMap::new();
    files.insert(".syns.yaml".into(), Entry::file(IDENTITY.as_bytes()));
    files.insert(".synsignore".into(), Entry::file(b"*.tmp\n"));
    files.insert(
        format!("{FOLDER}/.syns.yaml"),
        Entry::file(MARKER.as_bytes()),
    );
    files.insert(format!("{FOLDER}/a.md"), Entry::file(b"a\n"));
    files.insert(format!("{FOLDER}/b/c.md"), Entry::file(b"c\n"));
    files.insert("other/x.md".into(), Entry::file(b"x\n"));
    files
}

fn folder_copy(env: &Env) -> PathBuf {
    let dir = env.dir("q3");
    ok(&env.run(&dir, &["pull", "alice/notes", ".", "--path", FOLDER]));
    dir
}

fn change(env: &Env, changes: &[(&str, Option<&[u8]>)]) {
    env.fake.change(changes);
}

fn routes(env: &Env) -> Vec<String> {
    env.fake
        .routes()
        .into_iter()
        .filter(|r| r != "v1 push")
        .collect()
}

#[test]
fn r11_a_folder_copy_bound_to_its_holder() {
    let env = Env::new();
    env.fake.commit_entries(holder_files());
    let dir = folder_copy(&env);
    let written = files_of(&dir);
    assert_eq!(written.get("a.md").map(Vec::as_slice), Some(&b"a\n"[..]));
    assert!(!written.contains_key("other/x.md"));
    // The closure carried the folder's path.
    let tree = env
        .fake
        .log()
        .into_iter()
        .find(|s| s.route == "v2 tree")
        .unwrap();
    assert_eq!(tree.query.get("path").map(String::as_str), Some(FOLDER));

    // A no-op: one request, `changes` at the holder with no path.
    env.fake.clear();
    ok(&env.run(&dir, &["sync"]));
    assert_eq!(routes(&env), vec!["v2 changes"]);
    assert!(!env.fake.log()[0].query.contains_key("path"));

    // A push outside the folder: the view kept, one request.
    change(&env, &[("other/x.md", Some(b"x2\n"))]);
    env.fake.clear();
    ok(&env.run(&dir, &["sync"]));
    assert_eq!(routes(&env), vec!["v2 changes"]);
    env.fake.clear();
    ok(&env.run(&dir, &["sync"]));
    assert_eq!(routes(&env), vec!["v2 changes"]);

    // A push changing only the holder's `.synsignore`: two, then one.
    change(&env, &[(".synsignore", Some(b"*.tmp\n*.log\n"))]);
    env.fake.clear();
    ok(&env.run(&dir, &["sync"]));
    assert_eq!(routes(&env), vec!["v2 changes", "v2 read"]);
    assert_eq!(env.fake.reads()[0].asked(), vec![".synsignore"]);
    env.fake.clear();
    ok(&env.run(&dir, &["sync"]));
    assert_eq!(routes(&env), vec!["v2 changes"]);

    // A push deleting it: one.
    change(&env, &[(".synsignore", None)]);
    env.fake.clear();
    ok(&env.run(&dir, &["sync"]));
    assert_eq!(routes(&env), vec!["v2 changes"]);
    env.fake.clear();
    ok(&env.run(&dir, &["sync"]));
    assert_eq!(routes(&env), vec!["v2 changes"]);

    // A change inside the folder: two, the batch at the holder's paths.
    change(&env, &[(&format!("{FOLDER}/a.md"), Some(b"a2\n"))]);
    env.fake.clear();
    ok(&env.run(&dir, &["sync"]));
    assert_eq!(routes(&env), vec!["v2 changes", "v2 read"]);
    assert_eq!(env.fake.reads()[0].asked(), vec![format!("{FOLDER}/a.md")]);
    assert_eq!(std::fs::read(dir.join("a.md")).unwrap(), b"a2\n");
}

#[test]
fn r11_the_holder_s_synsignore_still_decides_the_collection() {
    let run = |v2: bool| {
        let env = Env::new();
        env.fake.v2(v2);
        env.fake.commit_entries(holder_files());
        let dir = folder_copy(&env);
        // A local file the holder's `.synsignore` excludes is never sent.
        std::fs::write(dir.join("scratch.tmp"), b"local\n").unwrap();
        std::fs::write(dir.join("new.md"), b"new\n").unwrap();
        let out = env.run(&dir, &["sync"]);
        let pushed: Vec<String> = env
            .fake
            .log()
            .into_iter()
            .filter(|s| s.route == "v1 push")
            .flat_map(|s| {
                s.json()["files"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|f| f["path"].as_str().unwrap().to_string())
                    .collect::<Vec<_>>()
            })
            .collect();
        let head: Vec<String> = env.fake.head().files.keys().cloned().collect();
        (
            transcript(&out, env.fake.port),
            pushed,
            head,
            files_of(&dir),
        )
    };
    let v1 = run(false);
    let v2 = run(true);
    assert_eq!(v2, v1);
    assert!(!v2.2.iter().any(|p| p.ends_with("scratch.tmp")));
    assert!(v2.2.iter().any(|p| p == &format!("{FOLDER}/new.md")));
}

#[test]
fn r11_output_equals_v1s() {
    let run = |v2: bool| {
        let env = Env::new();
        env.fake.v2(v2);
        env.fake.commit_entries(holder_files());
        let dir = env.dir("q3");
        let mut outs = vec![env.run(&dir, &["pull", "alice/notes", ".", "--path", FOLDER])];
        change(
            &env,
            &[(&format!("{FOLDER}/b/c.md"), None), ("other/x.md", None)],
        );
        outs.push(env.run(&dir, &["sync"]));
        change(&env, &[(&format!("{FOLDER}/d.md"), Some(b"d\n"))]);
        outs.push(env.run(&dir, &["--json", "sync"]));
        outs.push(env.run(&dir, &["--json", "status"]));
        let t: Vec<_> = outs.iter().map(|o| transcript(o, env.fake.port)).collect();
        (t, files_of(&dir))
    };
    assert_eq!(run(true), run(false));
}

#[test]
fn r11_a_view_recording_no_synsignore_reads_it_once() {
    let env = Env::new();
    env.fake.commit_entries(holder_files());
    let dir = folder_copy(&env);
    // Forget the record in the base's view, as an earlier build wrote it.
    let views = view_files(&env.store());
    assert_eq!(views.len(), 1);
    let text = std::fs::read_to_string(&views[0]).unwrap();
    let rewritten: String = text
        .lines()
        .map(|l| {
            if l.starts_with("synsignore ") {
                "synsignore unknown".to_string()
            } else {
                l.to_string()
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
        + "\n";
    std::fs::write(&views[0], rewritten).unwrap();
    env.fake.clear();
    ok(&env.run(&dir, &["sync"]));
    assert_eq!(routes(&env), vec!["v1 raw", "v2 changes"]);
    env.fake.clear();
    ok(&env.run(&dir, &["sync"]));
    assert_eq!(routes(&env), vec!["v2 changes"]);
}

fn view_files(store: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let mut pending = vec![store.join("views")];
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

#[test]
fn r13_the_snapshot_pull() {
    let env = Env::new();
    let mut files = holder_files();
    env.fake.commit_entries(files.clone());
    files.insert("late.md".into(), Entry::file(b"late\n"));
    let second = env.fake.commit_entries(files);
    let run = |env: &Env, version: &str, name: &str| {
        let dir = env.dir(name);
        let out = env.run(
            &dir,
            &["--json", "pull", "alice/notes", ".", "--version", version],
        );
        ok(&out);
        (out, dir)
    };
    env.fake.clear();
    let (out, dir) = run(&env, "1", "v1");
    let doc: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(
        doc["commitSha"],
        env.fake.with(|s| s.commits[0].sha.clone()).as_str()
    );
    assert!(!dir.join("late.md").exists());
    let routes: Vec<String> = env.fake.routes();
    assert!(routes.contains(&"v2 tree".to_string()), "{routes:?}");
    assert!(!routes.contains(&"v1 tree".to_string()), "{routes:?}");
    let tree = env
        .fake
        .log()
        .into_iter()
        .find(|s| s.route == "v2 tree")
        .unwrap();
    assert_eq!(tree.query.get("at").map(String::as_str), Some("1"));
    // A full hash, then a prefix, which keeps v1.
    let (_, dir) = run(&env, &second, "full");
    assert!(dir.join("late.md").exists());
    env.fake.clear();
    let (_, dir) = run(&env, &second[..7], "prefix");
    assert!(dir.join("late.md").exists());
    assert!(env.fake.routes().contains(&"v1 tree".to_string()));
    assert!(!env.fake.routes().contains(&"v2 tree".to_string()));
}

#[test]
fn r13_the_snapshot_pull_equals_v1s() {
    let run = |v2: bool| {
        let env = Env::new();
        env.fake.v2(v2);
        env.fake.commit_entries(holder_files());
        let mut outs = Vec::new();
        let a = env.dir("a");
        outs.push(env.run(&a, &["pull", "alice/notes", ".", "--version", "1"]));
        let b = env.dir("b");
        outs.push(env.run(
            &b,
            &["--json", "pull", "alice/notes", ".", "--version", "1"],
        ));
        let c = env.dir("c");
        outs.push(env.run(
            &c,
            &[
                "pull",
                "alice/notes",
                ".",
                "--path",
                FOLDER,
                "--version",
                "1",
            ],
        ));
        let t: Vec<_> = outs.iter().map(|o| transcript(o, env.fake.port)).collect();
        (t, files_of(&a), files_of(&b), files_of(&c))
    };
    assert_eq!(run(true), run(false));
}

// ---- a shared folder's identity (row 12) -------------------------------------

const SHARED: &str = "holder: alice/notes\npath: clients/q3\nshared_as: notes-q3\n";

fn identity_files() -> Files {
    let mut files = holder_files();
    files.insert(
        format!("{FOLDER}/.syns.yaml"),
        Entry::file(SHARED.as_bytes()),
    );
    files
}

fn identity_env(v2: bool) -> Env {
    let env = Env::new();
    env.fake.v2(v2);
    env.fake
        .with(|s| s.identity = Some(("notes-q3".to_string(), FOLDER.to_string())));
    env.fake.commit_entries(identity_files());
    env
}

fn identity_copy(env: &Env) -> (PathBuf, std::process::Output) {
    let dir = env.dir("q3");
    let out = env.run(&dir, &["pull", "alice/notes-q3", "."]);
    (dir, out)
}

#[test]
fn r12_a_shared_folder_identity_copy() {
    let env = identity_env(true);
    let (dir, out) = identity_copy(&env);
    ok(&out);
    assert_eq!(std::fs::read(dir.join("a.md")).unwrap(), b"a\n");
    assert!(
        env.fake
            .routes()
            .iter()
            .all(|r| !r.starts_with("v1 versions")),
        "{:?}",
        env.fake.routes()
    );
    // A no-op at an unmoved base: one request, at the identity.
    env.fake.clear();
    ok(&env.run(&dir, &["sync"]));
    assert_eq!(routes(&env), vec!["v2 changes identity"]);
    assert!(!env.fake.log()[0].query.contains_key("path"));
    // A push outside the folder leaves the identity's head: still one.
    change(&env, &[("other/x.md", Some(b"x2\n"))]);
    env.fake.clear();
    ok(&env.run(&dir, &["sync"]));
    assert_eq!(routes(&env), vec!["v2 changes identity"]);
    // A version changing the folder: two, the next no-op two (the
    // holder's `.synsignore` read once through the holder), then one.
    change(&env, &[(&format!("{FOLDER}/a.md"), Some(b"a2\n"))]);
    env.fake.clear();
    ok(&env.run(&dir, &["sync"]));
    assert_eq!(
        routes(&env),
        vec!["v2 changes identity", "v2 read identity"]
    );
    assert_eq!(std::fs::read(dir.join("a.md")).unwrap(), b"a2\n");
    env.fake.clear();
    ok(&env.run(&dir, &["sync"]));
    assert_eq!(routes(&env), vec!["v1 raw", "v2 changes identity"]);
    env.fake.clear();
    ok(&env.run(&dir, &["sync"]));
    assert_eq!(routes(&env), vec!["v2 changes identity"]);
}

#[test]
fn r12_output_equals_v1s() {
    let run = |v2: bool| {
        let env = identity_env(v2);
        let (dir, first) = identity_copy(&env);
        let mut outs = vec![first, env.run(&dir, &["sync"])];
        change(
            &env,
            &[
                (&format!("{FOLDER}/b/c.md"), Some(b"c2\n")),
                ("other/x.md", None),
            ],
        );
        outs.push(env.run(&dir, &["--json", "sync"]));
        outs.push(env.run(&dir, &["--json", "status"]));
        outs.push(env.run(&dir, &["ls"]));
        outs.push(env.run(&dir, &["--json", "cat", "a.md"]));
        outs.push(env.run(&dir, &["--json", "cat", "--", "a.md", "b/c.md"]));
        outs.push(env.run(&dir, &["glob", "**/*.md"]));
        let t: Vec<_> = outs.iter().map(|o| transcript(o, env.fake.port)).collect();
        (t, files_of(&dir))
    };
    assert_eq!(run(true), run(false));
}

#[test]
fn r13_a_snapshot_pull_through_an_identity() {
    let run = |v2: bool| {
        let env = identity_env(v2);
        change(&env, &[(&format!("{FOLDER}/a.md"), Some(b"a2\n"))]);
        let dir = env.dir("q3");
        let mut outs = vec![env.run(&dir, &["pull", "alice/notes-q3", ".", "--version", "1"])];
        // A holder version outside the identity's list: v2 `404`, then v1.
        change(&env, &[("other/x.md", Some(b"x3\n"))]);
        let other = env.dir("other");
        outs.push(env.run(&other, &["pull", "alice/notes-q3", ".", "--version", "3"]));
        let t: Vec<_> = outs.iter().map(|o| transcript(o, env.fake.port)).collect();
        (t, files_of(&dir), files_of(&other))
    };
    assert_eq!(run(true), run(false));
}

// ---- the view, not the recorded base, is the head (row 7) ---------------------

#[test]
fn r07_a_laid_base_reads_the_head_from_the_view() {
    let run = |v2: bool| {
        let env = Env::new();
        env.fake.v2(v2);
        let mut files = holder_files();
        files.insert(format!("{FOLDER}/f.md"), Entry::file(b"x\n"));
        env.fake.commit_entries(files);
        let holder = env.dir("h");
        std::fs::write(holder.join(".syns.yaml"), IDENTITY).unwrap();
        let mut outs = vec![env.run(&holder, &["pull"])];
        // The folder copy inside the checkout records Y and lays it over
        // the holder's base, which stays at X.
        change(&env, &[(&format!("{FOLDER}/f.md"), Some(b"y\n"))]);
        let folder = holder.join(FOLDER);
        outs.push(env.run(&folder, &["sync"]));
        // The head reverts the file to X's bytes.
        change(&env, &[(&format!("{FOLDER}/f.md"), Some(b"x\n"))]);
        outs.push(env.run(&holder, &["sync"]));
        outs.push(env.run(&holder, &["--json", "status"]));
        let t: Vec<_> = outs.iter().map(|o| transcript(o, env.fake.port)).collect();
        (t, files_of(&holder))
    };
    let v2 = run(true);
    assert_eq!(
        v2.1.get(&format!("{FOLDER}/f.md")).map(Vec::as_slice),
        Some(&b"x\n"[..]),
        "the holder ends holding the head's bytes"
    );
    assert_eq!(v2, run(false));
}

// REVIEW-syns-cli minor 2: `ls` in a folder copy leaves its view's
// `.synsignore` record, so the next no-op sync is still one request.
#[test]
fn m2_ls_keeps_a_folder_copy_s_view() {
    let env = Env::new();
    env.fake.commit_entries(holder_files());
    let dir = folder_copy(&env);
    // The base's view is now one `changes` derived (no root): the
    // listing's closure is a `200` at that same commit.
    change(&env, &[(&format!("{FOLDER}/a.md"), Some(b"a2\n"))]);
    ok(&env.run(&dir, &["sync"]));
    ok(&env.run(&dir, &["ls"]));
    ok(&env.run(&dir, &["--json", "glob", "**"]));
    env.fake.clear();
    ok(&env.run(&dir, &["sync"]));
    assert_eq!(routes(&env), vec!["v2 changes"]);
    env.fake.clear();
    ok(&env.run(&dir, &["ls"]));
    let statuses: Vec<u16> = env.fake.log().iter().map(|s| s.status).collect();
    assert_eq!(
        statuses,
        vec![304, 304],
        "the listing still renders from trees/"
    );
}
