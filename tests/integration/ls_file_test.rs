//! Issue 359: `syns ls PATH` naming a file answers that file's one entry,
//! the entry its parent's listing holds at the same commit, in both output
//! modes, over v2 and v1 alike; a path naming nothing keeps the refusal.

use std::path::{Path, PathBuf};

use serde_json::Value;

use crate::integration::v2_fake::{Env, IDENTITY, blob, ok};

fn setup(env: &Env) {
    env.fake.commit(&[
        ("a.md", b"a at 1\n"),
        ("docs/guide.md", b"# guide\n"),
        ("docs/deep/more.md", b"more\n"),
        ("1st.md", b"first\n"),
        ("private/secret.md", b"secret\n"),
        (
            "clients/q3/.syns.yaml",
            b"holder: alice/notes\npath: clients/q3\n",
        ),
        ("clients/q3/a.md", b"q3 a\n"),
        ("clients/q3/b/c.md", b"q3 c\n"),
    ]);
    env.fake
        .change(&[("a.md", Some(b"a at 2!\n")), ("late.md", Some(b"late\n"))]);
}

/// A fake at both ends of the v2 switch, set up, and a bound checkout.
fn envs() -> Vec<(bool, Env, PathBuf)> {
    [true, false]
        .into_iter()
        .map(|v2| {
            let env = Env::new();
            env.fake.v2(v2);
            setup(&env);
            let dir = env.dir("w");
            std::fs::write(dir.join(".syns.yaml"), IDENTITY).unwrap();
            (v2, env, dir)
        })
        .collect()
}

fn json(out: &std::process::Output) -> Value {
    ok(out);
    serde_json::from_slice(&out.stdout).unwrap()
}

fn stdout(out: &std::process::Output) -> String {
    ok(out);
    String::from_utf8_lossy(&out.stdout).into_owned()
}

/// The entry `syns ls <parent> --json` answers for `name`.
fn listed_entry(env: &Env, dir: &Path, parent: &[&str], name: &str) -> Value {
    let mut args = vec!["--json", "ls"];
    args.extend_from_slice(parent);
    let doc = json(&env.run(dir, &args));
    doc["entries"]
        .as_array()
        .unwrap()
        .iter()
        .find(|e| e["name"] == name)
        .cloned()
        .unwrap()
}

#[test]
fn a_file_at_the_root_answers_its_one_entry() {
    for (v2, env, dir) in envs() {
        let doc = json(&env.run(&dir, &["--json", "ls", "a.md"]));
        let entries = doc["entries"].as_array().unwrap();
        assert_eq!(entries.len(), 1, "v2={v2}: {doc}");
        assert_eq!(entries[0]["path"], "a.md", "v2={v2}");
        assert_eq!(entries[0]["type"], "file", "v2={v2}");
        assert_eq!(entries[0]["size"], 8, "v2={v2}");
        assert_eq!(entries[0]["sha"], blob(b"a at 2!\n"), "v2={v2}");
        assert_eq!(doc["truncated"], false, "v2={v2}");
        assert_eq!(doc["version"], 2, "v2={v2}");
        assert_eq!(doc["commitSha"], env.fake.head().sha, "v2={v2}");
        assert_eq!(
            entries[0],
            listed_entry(&env, &dir, &[], "a.md"),
            "v2={v2}: the same entry the parent lists"
        );

        let table = stdout(&env.run(&dir, &["ls", "a.md"]));
        let rows: Vec<Vec<&str>> = table
            .lines()
            .map(|l| {
                l.split(|c: char| "│┆|".contains(c))
                    .map(str::trim)
                    .filter(|c| !c.is_empty())
                    .collect()
            })
            .filter(|r: &Vec<&str>| r.len() == 3)
            .collect();
        assert_eq!(
            rows,
            vec![vec!["Name", "Type", "Size"], vec!["a.md", "file", "8"]],
            "v2={v2}: {table}"
        );
    }
}

#[test]
fn a_file_one_folder_down_and_deeper() {
    for (v2, env, dir) in envs() {
        for (path, parent, name) in [
            ("docs/guide.md", "docs", "guide.md"),
            ("docs/deep/more.md", "docs/deep", "more.md"),
            ("1st.md", "", "1st.md"),
        ] {
            let doc = json(&env.run(&dir, &["--json", "ls", path]));
            let parent: Vec<&str> = if parent.is_empty() {
                vec![]
            } else {
                vec![parent]
            };
            assert_eq!(
                doc["entries"],
                Value::Array(vec![listed_entry(&env, &dir, &parent, name)]),
                "v2={v2} {path}"
            );
            assert_eq!(doc["entries"][0]["path"], path, "v2={v2}");
        }
    }
}

#[test]
fn recursive_on_a_file_answers_the_same_entry() {
    for (v2, env, dir) in envs() {
        let plain = json(&env.run(&dir, &["--json", "ls", "docs/guide.md"]));
        let deep = json(&env.run(&dir, &["--json", "ls", "docs/guide.md", "--recursive"]));
        assert_eq!(plain, deep, "v2={v2}");
        let table = stdout(&env.run(&dir, &["ls", "docs/guide.md", "--recursive"]));
        assert!(table.contains("docs/guide.md"), "v2={v2}: {table}");
        assert!(!table.contains("more.md"), "v2={v2}: {table}");
    }
}

#[test]
fn a_path_naming_nothing_keeps_todays_refusal() {
    for (v2, env, dir) in envs() {
        for path in [
            "nope.md",
            "docs/nope.md",
            "nope/x.md",
            "docs/deep",
            "private/nope.md",
        ] {
            if path == "docs/deep" {
                // A folder lists as today.
                ok(&env.run(&dir, &["ls", path]));
                continue;
            }
            let out = env.run(&dir, &["ls", path]);
            assert_eq!(out.status.code(), Some(1), "v2={v2} {path}");
            assert!(out.stdout.is_empty(), "v2={v2} {path}");
            assert_eq!(
                String::from_utf8_lossy(&out.stderr),
                format!("error: path not found: {path}\n"),
                "v2={v2} {path}"
            );
            let out = env.run(&dir, &["--json", "ls", path]);
            assert_eq!(out.status.code(), Some(1), "v2={v2} {path}");
            // Issue 160 (`Boundary B`): the path named under `--json` too.
            assert_eq!(
                String::from_utf8_lossy(&out.stdout).trim(),
                format!(r#"{{"code":"not_found","error":"path not found: {path}"}}"#),
                "v2={v2} {path}"
            );
        }
    }
}

#[test]
fn a_hidden_file_stays_absent() {
    for (v2, env, dir) in envs() {
        env.fake.with(|s| s.hidden = vec!["private".into()]);
        let out = env.run(&dir, &["--json", "ls", "private/secret.md"]);
        assert_eq!(out.status.code(), Some(1), "v2={v2}");
        // Issue 160 (`Boundary B`): a hidden file reads as any absent path.
        assert_eq!(
            String::from_utf8_lossy(&out.stdout).trim(),
            r#"{"code":"not_found","error":"path not found: private/secret.md"}"#,
            "v2={v2}"
        );
    }
}

#[test]
fn a_trailing_slash_asks_for_a_folder() {
    for (v2, env, dir) in envs() {
        let q3 = env.dir("q3");
        ok(&env.run(&q3, &["pull", "alice/notes", ".", "--path", "clients/q3"]));
        for (cwd, path) in [
            (&dir, "a.md/"),
            (&dir, "./a.md/"),
            (&dir, "a.md/."),
            (&dir, "docs/guide.md/"),
            (&dir, "a.md/ "),
            (&q3, "a.md/"),
            (&q3, "a.md/."),
        ] {
            let out = env.run(cwd, &["ls", path]);
            assert_eq!(out.status.code(), Some(1), "v2={v2} {path}");
            assert_eq!(
                String::from_utf8_lossy(&out.stderr),
                format!("error: path not found: {path}\n"),
                "v2={v2} {path}"
            );
        }
        // A trailing `/` on a folder lists it as today.
        ok(&env.run(&dir, &["ls", "docs/"]));
        ok(&env.run(&q3, &["ls", "b/"]));
    }
}

#[test]
fn dot_segments_resolve_before_the_file_is_named() {
    for (v2, env, dir) in envs() {
        for path in ["./a.md", "docs/../a.md"] {
            let doc = json(&env.run(&dir, &["--json", "ls", path]));
            assert_eq!(doc["entries"][0]["path"], "a.md", "v2={v2} {path}");
        }
    }
}

#[test]
fn version_answers_the_file_at_that_version() {
    for (v2, env, dir) in envs() {
        let doc = json(&env.run(&dir, &["--json", "ls", "a.md", "--version", "1"]));
        assert_eq!(doc["version"], 1, "v2={v2}");
        assert_eq!(doc["entries"][0]["sha"], blob(b"a at 1\n"), "v2={v2}");
        assert_eq!(doc["entries"][0]["size"], 7, "v2={v2}");
        // A file added after version 1 stands nowhere at it.
        let out = env.run(&dir, &["ls", "late.md", "--version", "1"]);
        assert_eq!(out.status.code(), Some(1), "v2={v2}");
        let base = env.run(&dir, &["ls", "nope.md", "--version", "1"]);
        assert_eq!(
            String::from_utf8_lossy(&out.stderr),
            String::from_utf8_lossy(&base.stderr).replace("nope.md", "late.md"),
            "v2={v2}"
        );
        ok(&env.run(&dir, &["ls", "late.md"]));
    }
}

#[test]
fn repo_flag_answers_the_file() {
    for (v2, env, _) in envs() {
        let elsewhere = env.dir("elsewhere");
        let doc = json(&env.run(
            &elsewhere,
            &["--json", "ls", "docs/guide.md", "--repo", "alice/notes"],
        ));
        assert_eq!(doc["entries"][0]["path"], "docs/guide.md", "v2={v2}");
        assert_eq!(doc["entries"].as_array().unwrap().len(), 1, "v2={v2}");
    }
}

#[test]
fn inside_a_scoped_folder_the_path_counts_from_the_folder() {
    for (v2, env, _) in envs() {
        let q3 = env.dir("q3");
        ok(&env.run(&q3, &["pull", "alice/notes", ".", "--path", "clients/q3"]));
        for (path, size) in [("a.md", 5), ("b/c.md", 5)] {
            let doc = json(&env.run(&q3, &["--json", "ls", path]));
            let entries = doc["entries"].as_array().unwrap();
            assert_eq!(entries.len(), 1, "v2={v2} {path}: {doc}");
            assert_eq!(entries[0]["path"], path, "v2={v2}");
            assert_eq!(entries[0]["size"], size, "v2={v2}");
        }
        let out = env.run(&q3, &["ls", "nope.md"]);
        assert_eq!(out.status.code(), Some(1), "v2={v2}");
        assert_eq!(
            String::from_utf8_lossy(&out.stderr),
            "error: path not found: nope.md\n"
        );
    }
}

#[test]
fn a_file_costs_one_listing_of_its_parent_and_a_folder_nothing_more() {
    for (v2, env, dir) in envs() {
        let reads = |r: &&String| r.contains("tree");
        ok(&env.run(&dir, &["ls", "docs"]));
        env.fake.clear();
        ok(&env.run(&dir, &["ls", "docs"]));
        // A folder: its own read alone, the closure, or v2's refusal and v1.
        let folder = env.fake.routes();
        assert_eq!(
            folder.iter().filter(reads).count(),
            if v2 { 1 } else { 2 },
            "v2={v2}: {folder:?}"
        );
        env.fake.clear();
        ok(&env.run(&dir, &["ls", "docs/guide.md"]));
        let file = env.fake.routes();
        // The file's own read refused (v2, then v1), then the parent's
        // listing: over v2 the closure; with no v2, v2's refusal and v1.
        assert_eq!(
            file.iter().filter(reads).count(),
            if v2 { 3 } else { 4 },
            "v2={v2}: {file:?}"
        );
    }
}
