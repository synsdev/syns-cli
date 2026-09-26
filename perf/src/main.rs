//! u280's timing harness (SPEC u280 `perf run`, `perf compare`).
//!
//! `perf run` times two binaries, each against its own running stack,
//! sample by sample in one run, and writes one results document per side;
//! `perf compare` judges the after document against the baseline. It runs
//! outside every test suite and CI gate.
//!
//! ```text
//! cargo run --release --manifest-path perf/Cargo.toml -- run --out DIR \
//!     --baseline BINARY,URL,CONFIG_DIR,SERVER_SHA,ENGINE_SHA \
//!     --after BINARY,URL,CONFIG_DIR,SERVER_SHA,ENGINE_SHA [--judged-only]
//! cargo run --release --manifest-path perf/Cargo.toml -- compare BASELINE AFTER
//! ```

use std::collections::BTreeMap;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode, Stdio};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use sha1::{Digest, Sha1};

/// `LIM-file-size`'s value, a literal of the harness's own.
const LIM_FILE_SIZE: usize = 26_214_400;
const KIB: usize = 1024;
const MIB: usize = 1024 * 1024;

/// Unrecorded samples before the recorded ones, and the recorded count.
const WARMUP: usize = 5;
const RECORDED: usize = 30;

/// No invocation of either binary outlives this.
const INVOCATION_BOUND: Duration = Duration::from_secs(1_200);

const FIXTURES: [&str; 3] = ["agent-text", "agent-mixed", "agent-large"];
const OPERATIONS: [&str; 4] = ["push-first", "push-incremental", "pull", "sync"];

/// Whether `perf compare` judges a pair; every other pair is reference.
fn judged(fixture: &str, operation: &str) -> bool {
    fixture == "agent-text" || matches!(operation, "push-incremental" | "sync")
}

/// Whether a fixture is timed on an operation: `agent-large` on
/// `push-incremental` alone, every other fixture on every operation.
fn timed(fixture: &str, operation: &str) -> bool {
    fixture != "agent-large" || operation == "push-incremental"
}

/// The length of each of `agent-large`'s `large/` files.
const LARGE_FILE: usize = 8 * MIB;
/// How many `large/` files `agent-large` holds, and how many of them a
/// `push-incremental` sample rewrites.
const LARGE_FILES: usize = 24;
const LARGE_REWRITTEN: usize = 4;

// ---- the generator and the fixtures ----------------------------------------

/// SplitMix64, seeded `280` for every fixture, so every machine and every
/// run draws the same bytes.
struct SplitMix64(u64);

impl SplitMix64 {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    /// A draw in `low..=high`.
    fn between(&mut self, low: usize, high: usize) -> usize {
        low + (self.next() % (high - low + 1) as u64) as usize
    }

    fn fill(&mut self, out: &mut [u8]) {
        for chunk in out.chunks_mut(8) {
            let word = self.next().to_le_bytes();
            chunk.copy_from_slice(&word[..chunk.len()]);
        }
    }
}

const WORDS: [&str; 19] = [
    "agent",
    "sync",
    "folder",
    "publish",
    "head",
    "merge",
    "review",
    "commit",
    "tree",
    "blob",
    "résumé",
    "naïve",
    "café",
    "über",
    "straße",
    "東京",
    "данные",
    "λόγος",
    "notes",
];

/// UTF-8 prose of at most `size` bytes, cut at a character boundary.
fn prose(rng: &mut SplitMix64, size: usize) -> Vec<u8> {
    let mut text = String::with_capacity(size + 16);
    while text.len() < size {
        text.push_str(WORDS[rng.between(0, WORDS.len() - 1)]);
        text.push_str(if rng.between(0, 9) < 9 { " " } else { ".\n" });
    }
    let mut cut = size;
    while !text.is_char_boundary(cut) {
        cut -= 1;
    }
    text.truncate(cut);
    text.into_bytes()
}

fn note(i: usize, body: Vec<u8>) -> Vec<u8> {
    let mut bytes = format!("# note {i}\n\n").into_bytes();
    bytes.extend(body);
    bytes
}

/// `agent-text`: 2,000 markdown files over 200 folders three deep, each
/// 512 B to 32 KiB of UTF-8 prose.
fn agent_text() -> BTreeMap<String, Vec<u8>> {
    let mut rng = SplitMix64(280);
    let mut files = BTreeMap::new();
    for i in 0..2000 {
        let leaf = i % 200;
        let (a, b, c) = (leaf / 40, (leaf / 8) % 5, leaf % 8);
        let size = rng.between(512, 32 * KIB);
        files.insert(
            format!("docs/a{a}/b{b}/c{c}/note{i:04}.md"),
            note(i, prose(&mut rng, size)),
        );
    }
    files
}

/// The 200 markdown notes `agent-mixed` and `agent-large` both open on,
/// drawn from `rng` in turn.
fn mixed_notes(rng: &mut SplitMix64, files: &mut BTreeMap<String, Vec<u8>>) {
    for i in 0..200 {
        let (a, b, c) = (i % 4, (i / 4) % 5, (i / 20) % 10);
        let size = rng.between(512, 32 * KIB);
        files.insert(
            format!("docs/a{a}/b{b}/c{c}/note{i:03}.md"),
            note(i, prose(rng, size)),
        );
    }
}

/// `agent-large`: the 200 notes `agent-mixed` draws, beside twenty-four
/// text files `large/{n}.txt` of `LARGE_FILE` bytes of prose each, padded
/// with ASCII spaces to that length, four of which serialise past
/// `CHUNK_BUDGET_BYTES` together.
fn agent_large() -> BTreeMap<String, Vec<u8>> {
    let mut rng = SplitMix64(280);
    let mut files = BTreeMap::new();
    mixed_notes(&mut rng, &mut files);
    for n in 0..LARGE_FILES {
        let mut bytes = prose(&mut rng, LARGE_FILE);
        bytes.resize(LARGE_FILE, b' ');
        files.insert(format!("large/{n}.txt"), bytes);
    }
    files
}

/// `agent-mixed`: 200 such files beside PNGs, JPEGs and PDFs, each opening
/// on its format's signature and holding a NUL within its first 8,192
/// bytes.
fn agent_mixed() -> BTreeMap<String, Vec<u8>> {
    const PNG: &[u8] = b"\x89PNG\r\n\x1a\n\x00\x00\x00\rIHDR";
    const JPG: &[u8] = b"\xff\xd8\xff\xe0\x00\x10JFIF\x00";
    const PDF: &[u8] = b"%PDF-1.7\n%\xe2\xe3\xcf\xd3\n\x00";
    let binaries: [(&str, &[u8], usize); 8] = [
        ("assets/img/p64k.png", PNG, 64 * KIB),
        ("assets/img/p1m.png", PNG, MIB),
        ("assets/img/p8m.png", PNG, 8 * MIB),
        ("assets/photo/j256k.jpg", JPG, 256 * KIB),
        ("assets/photo/j4m.jpg", JPG, 4 * MIB),
        ("assets/doc/d128k.pdf", PDF, 128 * KIB),
        ("assets/doc/d2m.pdf", PDF, 2 * MIB),
        ("assets/doc/dlim.pdf", PDF, LIM_FILE_SIZE),
    ];
    let mut rng = SplitMix64(280);
    let mut files = BTreeMap::new();
    mixed_notes(&mut rng, &mut files);
    for (path, signature, size) in binaries {
        let mut bytes = vec![0u8; size];
        bytes[..signature.len()].copy_from_slice(signature);
        rng.fill(&mut bytes[signature.len()..]);
        debug_assert!(bytes[..8192].contains(&0));
        files.insert(path.to_string(), bytes);
    }
    files
}

fn fixture(name: &str) -> BTreeMap<String, Vec<u8>> {
    match name {
        "agent-text" => agent_text(),
        "agent-large" => agent_large(),
        _ => agent_mixed(),
    }
}

// ---- the results document ---------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct Machine {
    os: String,
    arch: String,
    cpus: usize,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Measurement {
    samples_ms: Vec<f64>,
    matched_files: Vec<usize>,
    published_files: Vec<usize>,
    median_ms: f64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Run {
    fixture: String,
    operation: String,
    measurements: Vec<Measurement>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Results {
    label: String,
    cli_version: String,
    server_url: String,
    server_commit: String,
    engine_commit: String,
    machine: Machine,
    runs: Vec<Run>,
}

/// The mean of the 15th and 16th of the 30 recorded samples in ascending
/// order.
fn median(samples: &[f64]) -> f64 {
    let mut sorted = samples.to_vec();
    sorted.sort_by(f64::total_cmp);
    let n = sorted.len();
    if n == 0 {
        return 0.0;
    }
    if n.is_multiple_of(2) {
        (sorted[n / 2 - 1] + sorted[n / 2]) / 2.0
    } else {
        sorted[n / 2]
    }
}

fn this_machine() -> Machine {
    Machine {
        os: std::env::consts::OS.to_string(),
        arch: std::env::consts::ARCH.to_string(),
        cpus: std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(1),
    }
}

// ---- comparing ------------------------------------------------------------

/// A measurement stands `slower` where the after median exceeds the
/// baseline median by more than 10% and by more than 50 ms.
fn measurement_slower(baseline: f64, after: f64) -> bool {
    after - baseline > 50.0 && after - baseline > 0.10 * baseline
}

/// Where a pair's first measurement stands `slower` under the bound.
fn first_slower(fixture: &str, operation: &str, baseline: &Run, after: &Run) -> bool {
    judged(fixture, operation)
        && match (baseline.measurements.first(), after.measurements.first()) {
            (Some(b), Some(a)) => measurement_slower(b.median_ms, a.median_ms),
            _ => false,
        }
}

/// The comparison's printed lines and whether any judged pair stands
/// `slower`, or the field that makes the two documents incomparable.
fn compare(baseline: &Results, after: &Results) -> Result<(Vec<String>, bool), String> {
    if baseline.machine != after.machine {
        return Err("machine".to_string());
    }
    let pairs = |r: &Results| -> Vec<(String, String)> {
        let mut p: Vec<_> = r
            .runs
            .iter()
            .map(|run| (run.fixture.clone(), run.operation.clone()))
            .collect();
        p.sort();
        p
    };
    if pairs(baseline) != pairs(after) {
        return Err("runs".to_string());
    }
    let mut lines = Vec::new();
    let mut any_slower = false;
    for fixture in FIXTURES {
        for operation in OPERATIONS {
            let find = |r: &Results| {
                r.runs
                    .iter()
                    .find(|run| run.fixture == fixture && run.operation == operation)
                    .cloned()
            };
            let (Some(b), Some(a)) = (find(baseline), find(after)) else {
                continue;
            };
            let measured: Vec<(f64, f64)> = b
                .measurements
                .iter()
                .zip(a.measurements.iter())
                .map(|(b, a)| (b.median_ms, a.median_ms))
                .collect();
            let Some(&(b0, a0)) = measured.first() else {
                continue;
            };
            let verdict = if !judged(fixture, operation) {
                "reference"
            } else if measured.iter().all(|(b, a)| measurement_slower(*b, *a)) {
                any_slower = true;
                "slower"
            } else {
                "ok"
            };
            let mut line = format!("{fixture} {operation} {b0:.0} ms -> {a0:.0} ms");
            if let Some(&(b1, a1)) = measured.get(1) {
                line.push_str(&format!("; {b1:.0} ms -> {a1:.0} ms"));
            }
            line.push(' ');
            line.push_str(verdict);
            lines.push(line);
        }
    }
    Ok((lines, any_slower))
}

fn load(path: &str) -> Result<Results, String> {
    let bytes = std::fs::read(path).map_err(|e| format!("could not read {path}: {e}"))?;
    serde_json::from_slice(&bytes).map_err(|e| format!("could not read {path}: {e}"))
}

fn cmd_compare(args: &[String]) -> ExitCode {
    let [baseline, after] = args else {
        eprintln!("usage: perf compare BASELINE AFTER");
        return ExitCode::from(2);
    };
    let (baseline, after) = match (load(baseline), load(after)) {
        (Ok(b), Ok(a)) => (b, a),
        (Err(e), _) | (_, Err(e)) => {
            eprintln!("{e}");
            return ExitCode::from(1);
        }
    };
    match compare(&baseline, &after) {
        Err(field) => {
            eprintln!("incomparable: {field}");
            ExitCode::from(2)
        }
        Ok((lines, any_slower)) => {
            for line in lines {
                println!("{line}");
            }
            if any_slower {
                ExitCode::from(1)
            } else {
                ExitCode::SUCCESS
            }
        }
    }
}

// ---- running a side ---------------------------------------------------------

/// One side: its binary, its stack, and the commits that stack stands at.
#[derive(Debug, Clone)]
struct Side {
    label: String,
    binary: PathBuf,
    url: String,
    config_dir: PathBuf,
    server_sha: String,
    engine_sha: String,
    owner: String,
}

fn parse_side(label: &str, spec: &str) -> Result<Side, String> {
    let parts: Vec<&str> = spec.split(',').collect();
    let [binary, url, config_dir, server_sha, engine_sha] = parts[..] else {
        return Err(format!(
            "--{label} takes BINARY,URL,CONFIG_DIR,SERVER_SHA,ENGINE_SHA, got {spec}"
        ));
    };
    let credentials = Path::new(config_dir).join("credentials.json");
    let owner = std::fs::read(&credentials)
        .ok()
        .and_then(|b| serde_json::from_slice::<serde_json::Value>(&b).ok())
        .and_then(|v| v["username"].as_str().map(str::to_string))
        .ok_or_else(|| format!("no username in {}", credentials.display()))?;
    Ok(Side {
        label: label.to_string(),
        binary: PathBuf::from(binary),
        url: url.to_string(),
        config_dir: PathBuf::from(config_dir),
        server_sha: server_sha.to_string(),
        engine_sha: engine_sha.to_string(),
        owner,
    })
}

/// What an invocation of a side's binary answered.
struct Answer {
    code: Option<i32>,
    stdout: Vec<u8>,
    stderr: Vec<u8>,
    took: Duration,
}

/// Run the side's binary in `cwd` under its `SYNS_URL`, its
/// `SYNS_CONFIG_DIR` and `cache`, bounded by `INVOCATION_BOUND`; only the
/// invocation itself is timed.
fn invoke(side: &Side, cwd: &Path, cache: &Path, args: &[&str]) -> Result<Answer, String> {
    let mut child = Command::new(&side.binary)
        .args(args)
        .current_dir(cwd)
        .env("SYNS_URL", &side.url)
        .env("SYNS_CONFIG_DIR", &side.config_dir)
        .env("SYNS_CACHE_DIR", cache)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("could not start {}: {e}", side.binary.display()))?;
    let started = Instant::now();
    let mut stdout = child.stdout.take().unwrap();
    let mut stderr = child.stderr.take().unwrap();
    let out = std::thread::spawn(move || {
        let mut buf = Vec::new();
        let _ = stdout.read_to_end(&mut buf);
        buf
    });
    let err = std::thread::spawn(move || {
        let mut buf = Vec::new();
        let _ = stderr.read_to_end(&mut buf);
        buf
    });
    let status = loop {
        if let Some(status) = child.try_wait().map_err(|e| e.to_string())? {
            break status;
        }
        if started.elapsed() > INVOCATION_BOUND {
            let _ = child.kill();
            let _ = child.wait();
            let _ = out.join();
            let stderr = err.join().unwrap_or_default();
            return Err(format!(
                "{} `syns {}` ran past {:?}:\n{}",
                side.binary.display(),
                args.join(" "),
                INVOCATION_BOUND,
                String::from_utf8_lossy(&stderr)
            ));
        }
        std::thread::sleep(Duration::from_millis(2));
    };
    let took = started.elapsed();
    Ok(Answer {
        code: status.code(),
        stdout: out.join().unwrap_or_default(),
        stderr: err.join().unwrap_or_default(),
        took,
    })
}

/// `invoke`, refusing any exit but `expect`.
fn expect(
    side: &Side,
    fixture: &str,
    operation: &str,
    cwd: &Path,
    cache: &Path,
    args: &[&str],
    code: i32,
) -> Result<Answer, String> {
    let answer = invoke(side, cwd, cache, args)
        .map_err(|e| format!("{} {fixture} {operation}: {e}", side.label))?;
    if answer.code != Some(code) {
        return Err(format!(
            "{} {fixture} {operation}: `syns {}` exited {:?}, not {code}:\n{}",
            side.label,
            args.join(" "),
            answer.code,
            String::from_utf8_lossy(&answer.stderr)
        ));
    }
    Ok(answer)
}

fn write_tree(root: &Path, files: &BTreeMap<String, Vec<u8>>) -> Result<(), String> {
    for (path, bytes) in files {
        let target = root.join(path);
        std::fs::create_dir_all(target.parent().unwrap())
            .and_then(|()| std::fs::write(&target, bytes))
            .map_err(|e| format!("could not write {}: {e}", target.display()))?;
    }
    Ok(())
}

fn blob_sha1(bytes: &[u8]) -> String {
    let mut hasher = Sha1::new();
    hasher.update(format!("blob {}\0", bytes.len()).as_bytes());
    hasher.update(bytes);
    hasher
        .finalize()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// How many of the paths `owed` names the folder at `root` holds with
/// exactly the owed bytes.
fn matched(root: &Path, owed: &BTreeMap<String, Vec<u8>>) -> Result<usize, String> {
    let mut count = 0;
    for (path, bytes) in owed {
        let target = root.join(path);
        match std::fs::read(&target) {
            Ok(held) if held == *bytes => count += 1,
            Ok(_) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(format!("could not read {}: {e}", target.display())),
        }
    }
    Ok(count)
}

fn nonce() -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or_default();
    blob_sha1(format!("{nanos}-{}", std::process::id()).as_bytes())[..8].to_string()
}

/// The markdown paths of a fixture, in ascending order.
fn notes(files: &BTreeMap<String, Vec<u8>>) -> Vec<String> {
    files
        .keys()
        .filter(|p| p.ends_with(".md"))
        .cloned()
        .collect()
}

fn append(
    root: &Path,
    owed: &mut BTreeMap<String, Vec<u8>>,
    paths: &[String],
    line: &str,
) -> Result<(), String> {
    for path in paths {
        let bytes = owed.get_mut(path).expect("a fixture path");
        bytes.extend_from_slice(line.as_bytes());
        std::fs::write(root.join(path), &*bytes)
            .map_err(|e| format!("could not write {path}: {e}"))?;
    }
    Ok(())
}

/// Everything one side keeps for one fixture across its samples: the seed
/// repository's copy and, but for `agent-large`, the two copies of the
/// sync repository, with what each flow owes every fixture path.
struct Kept {
    seed_name: String,
    seed: PathBuf,
    seed_cache: PathBuf,
    seed_owed: BTreeMap<String, Vec<u8>>,
    sync: Option<SyncCopies>,
}

/// The two copies of a side's sync repository for one fixture.
struct SyncCopies {
    a: PathBuf,
    a_cache: PathBuf,
    b: PathBuf,
    b_cache: PathBuf,
    owed: BTreeMap<String, Vec<u8>>,
}

/// One timed sample: its duration and its work counts.
struct Sample {
    ms: f64,
    matched: Option<usize>,
    published: Option<usize>,
}

struct Harness {
    scratch: PathBuf,
    nonce: String,
    fixtures: BTreeMap<String, BTreeMap<String, Vec<u8>>>,
    kept: BTreeMap<(String, String), Kept>,
    /// Every repository made and not yet deleted: side, name, copy, cache.
    made: Vec<(Side, String, PathBuf, PathBuf)>,
    counter: usize,
}

impl Harness {
    fn fresh_dir(&mut self, what: &str) -> Result<PathBuf, String> {
        self.counter += 1;
        let dir = self.scratch.join(format!("{what}-{}", self.counter));
        std::fs::create_dir_all(&dir)
            .map_err(|e| format!("could not write {}: {e}", dir.display()))?;
        Ok(dir)
    }

    /// Publish the seed and sync repositories a side's samples work in,
    /// from the fixture as generated, each through a bare `syns push`.
    fn keep(&mut self, side: &Side, fixture: &str) -> Result<(), String> {
        let key = (side.label.clone(), fixture.to_string());
        if self.kept.contains_key(&key) {
            return Ok(());
        }
        let files = self.fixtures[fixture].clone();
        let label = &side.label;
        let seed_name = format!("u280-perf-{label}-{fixture}-seed-{}", self.nonce);
        let sync_name = format!("u280-perf-{label}-{fixture}-sync-{}", self.nonce);
        let seed = self.fresh_dir("seed")?;
        let seed_cache = self.fresh_dir("seed-cache")?;
        write_tree(&seed, &files)?;
        expect(
            side,
            fixture,
            "setup",
            &seed,
            &seed_cache,
            &["push", "--name", &seed_name],
            0,
        )?;
        self.made.push((
            side.clone(),
            seed_name.clone(),
            seed.clone(),
            seed_cache.clone(),
        ));

        // `agent-large` times `push-incremental` alone, so it publishes
        // no sync repository.
        let sync = if timed(fixture, "sync") {
            Some(self.keep_sync(side, fixture, &files, &sync_name)?)
        } else {
            None
        };
        self.kept.insert(
            key,
            Kept {
                seed_name,
                seed,
                seed_cache,
                seed_owed: files,
                sync,
            },
        );
        Ok(())
    }

    /// Publish the sync repository from the fixture as generated through a
    /// bare `syns push` in one copy, and take it into a second.
    fn keep_sync(
        &mut self,
        side: &Side,
        fixture: &str,
        files: &BTreeMap<String, Vec<u8>>,
        sync_name: &str,
    ) -> Result<SyncCopies, String> {
        let a = self.fresh_dir("sync-a")?;
        let a_cache = self.fresh_dir("sync-a-cache")?;
        write_tree(&a, files)?;
        expect(
            side,
            fixture,
            "setup",
            &a,
            &a_cache,
            &["push", "--name", sync_name],
            0,
        )?;
        self.made.push((
            side.clone(),
            sync_name.to_string(),
            a.clone(),
            a_cache.clone(),
        ));
        let b = self.fresh_dir("sync-b")?;
        let b_cache = self.fresh_dir("sync-b-cache")?;
        let repository = format!("{}/{sync_name}", side.owner);
        expect(
            side,
            fixture,
            "setup",
            &self.scratch,
            &b_cache,
            &["pull", &repository, b.to_str().unwrap()],
            0,
        )?;
        Ok(SyncCopies {
            a,
            a_cache,
            b,
            b_cache,
            owed: files.clone(),
        })
    }

    fn sample(&mut self, side: &Side, fixture: &str, operation: &str) -> Result<Sample, String> {
        self.keep(side, fixture)?;
        self.counter += 1;
        let n = self.counter;
        let key = (side.label.clone(), fixture.to_string());
        match operation {
            "push-first" => {
                let copy = self.fresh_dir("first")?;
                let cache = self.fresh_dir("first-cache")?;
                write_tree(&copy, &self.fixtures[fixture])?;
                let name = format!("u280-perf-{}-{fixture}-{n}-{}", side.label, self.nonce);
                // Registered before the push, so a failed push or delete
                // still leaves it to the teardown (CR1-6).
                self.made
                    .push((side.clone(), name.clone(), copy.clone(), cache.clone()));
                let timed = expect(
                    side,
                    fixture,
                    operation,
                    &copy,
                    &cache,
                    &["push", "--name", &name],
                    0,
                )?;
                expect(
                    side,
                    fixture,
                    operation,
                    &copy,
                    &cache,
                    &["delete", "--yes"],
                    0,
                )?;
                self.made.pop();
                let _ = std::fs::remove_dir_all(&copy);
                let _ = std::fs::remove_dir_all(&cache);
                Ok(Sample {
                    ms: timed.took.as_secs_f64() * 1000.0,
                    matched: None,
                    published: None,
                })
            }
            "push-incremental" => {
                let kept = self.kept.get_mut(&key).unwrap();
                let mut ten: Vec<String> = notes(&kept.seed_owed).into_iter().take(10).collect();
                // `agent-large` also rewrites the first four `large/`
                // files in ascending order, so the change chunks.
                ten.extend(
                    kept.seed_owed
                        .keys()
                        .filter(|p| p.starts_with("large/"))
                        .take(LARGE_REWRITTEN)
                        .cloned(),
                );
                // The same files, rewritten: their fixture bytes and a
                // line naming this sample.
                for path in &ten {
                    let mut bytes = self.fixtures[fixture][path].clone();
                    bytes.extend_from_slice(format!("\nrewrite {n}\n").as_bytes());
                    std::fs::write(kept.seed.join(path), &bytes)
                        .map_err(|e| format!("could not write {path}: {e}"))?;
                    kept.seed_owed.insert(path.clone(), bytes);
                }
                let timed = expect(
                    side,
                    fixture,
                    operation,
                    &kept.seed.clone(),
                    &kept.seed_cache.clone(),
                    &["push"],
                    0,
                )?;
                Ok(Sample {
                    ms: timed.took.as_secs_f64() * 1000.0,
                    matched: None,
                    published: None,
                })
            }
            "pull" => {
                let into = self.scratch.join(format!("pull-{n}"));
                let cache = self.fresh_dir("pull-cache")?;
                let kept = &self.kept[&key];
                let repository = format!("{}/{}", side.owner, kept.seed_name);
                let timed = expect(
                    side,
                    fixture,
                    operation,
                    &self.scratch,
                    &cache,
                    &["pull", &repository, into.to_str().unwrap()],
                    0,
                )?;
                let count = matched(&into, &kept.seed_owed)?;
                let _ = std::fs::remove_dir_all(&into);
                let _ = std::fs::remove_dir_all(&cache);
                Ok(Sample {
                    ms: timed.took.as_secs_f64() * 1000.0,
                    matched: Some(count),
                    published: None,
                })
            }
            _ => {
                let kept = self.kept.get_mut(&key).unwrap();
                let Some(sync) = kept.sync.as_mut() else {
                    return Err(format!(
                        "{} {fixture} {operation}: the fixture keeps no sync repository",
                        side.label
                    ));
                };
                let all = notes(&sync.owed);
                let (b_paths, a_paths) = (all[all.len() - 10..].to_vec(), all[10..20].to_vec());
                // The second copy takes the head and publishes ten edits of
                // its own, outside the timed window.
                expect(
                    side,
                    fixture,
                    operation,
                    &sync.b.clone(),
                    &sync.b_cache.clone(),
                    &["pull"],
                    0,
                )?;
                append(
                    &sync.b.clone(),
                    &mut sync.owed,
                    &b_paths,
                    &format!("\nremote edit {n}\n"),
                )?;
                expect(
                    side,
                    fixture,
                    operation,
                    &sync.b.clone(),
                    &sync.b_cache.clone(),
                    &["push"],
                    0,
                )?;
                // The first copy's ten local edits, then the timed pair.
                let mut local = sync.owed.clone();
                for path in &a_paths {
                    let bytes = local.get_mut(path).unwrap();
                    bytes.extend_from_slice(format!("\nlocal edit {n}\n").as_bytes());
                    std::fs::write(sync.a.join(path), &*bytes)
                        .map_err(|e| format!("could not write {path}: {e}"))?;
                }
                sync.owed = local;
                let (a, cache) = (sync.a.clone(), sync.a_cache.clone());
                let synced = expect(side, fixture, operation, &a, &cache, &["sync"], 4)?;
                let continued = expect(
                    side,
                    fixture,
                    operation,
                    &a,
                    &cache,
                    &["resolution", "continue"],
                    0,
                )?;
                let ms = (synced.took + continued.took).as_secs_f64() * 1000.0;
                let sync = self.kept[&key].sync.as_ref().expect("checked above");
                let count = matched(&sync.a, &sync.owed)?;
                let listing = expect(
                    side,
                    fixture,
                    operation,
                    &sync.a,
                    &sync.a_cache,
                    &["--json", "ls", "--recursive"],
                    0,
                )?;
                let tree: serde_json::Value =
                    serde_json::from_slice(&listing.stdout).map_err(|e| {
                        format!(
                            "{} {fixture} sync: the listing did not decode: {e}",
                            side.label
                        )
                    })?;
                if tree["truncated"] != serde_json::Value::Bool(false) {
                    return Err(format!(
                        "{} {fixture} sync: the listing came back truncated",
                        side.label
                    ));
                }
                let served: BTreeMap<String, String> = tree["entries"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(|e| {
                        Some((
                            e["path"].as_str()?.to_string(),
                            e["sha"].as_str()?.to_string(),
                        ))
                    })
                    .collect();
                let published = sync
                    .owed
                    .iter()
                    .filter(|(path, bytes)| served.get(*path) == Some(&blob_sha1(bytes)))
                    .count();
                Ok(Sample {
                    ms,
                    matched: Some(count),
                    published: Some(published),
                })
            }
        }
    }

    /// Five unrecorded samples, then thirty recorded ones, each timing
    /// both sides in turn, the side timed first alternating.
    fn measure(
        &mut self,
        sides: &[Side; 2],
        fixture: &str,
        operation: &str,
    ) -> Result<[Measurement; 2], String> {
        let mut recorded: [Vec<Sample>; 2] = [Vec::new(), Vec::new()];
        for n in 0..WARMUP + RECORDED {
            let order: [usize; 2] = if n.is_multiple_of(2) { [0, 1] } else { [1, 0] };
            for i in order {
                let sample = self.sample(&sides[i], fixture, operation)?;
                if n >= WARMUP {
                    recorded[i].push(sample);
                }
            }
        }
        let [b, a] = recorded;
        Ok([b, a].map(|samples| {
            let ms: Vec<f64> = samples
                .iter()
                .map(|s| (s.ms * 10.0).round() / 10.0)
                .collect();
            Measurement {
                median_ms: median(&ms),
                matched_files: samples.iter().filter_map(|s| s.matched).collect(),
                published_files: samples.iter().filter_map(|s| s.published).collect(),
                samples_ms: ms,
            }
        }))
    }

    /// Delete every repository the run made, and remove the scratch
    /// directory.
    /// Every step is taken whatever an earlier one answered; the first
    /// refusal is the one answered (CR1-6).
    fn tear_down(&mut self) -> Result<(), String> {
        let mut first = None;
        for (side, name, copy, cache) in std::mem::take(&mut self.made) {
            if let Err(e) = expect(
                &side,
                &name,
                "teardown",
                &copy,
                &cache,
                &["delete", "--yes"],
                0,
            ) {
                first.get_or_insert(e);
            }
        }
        if let Err(e) = std::fs::remove_dir_all(&self.scratch) {
            first.get_or_insert(format!("could not remove {}: {e}", self.scratch.display()));
        }
        first.map_or(Ok(()), Err)
    }

    /// The side's own `--version`, run as every other invocation is.
    fn cli_version(&self, side: &Side) -> Result<String, String> {
        let answer = expect(
            side,
            "-",
            "version",
            &self.scratch,
            &self.scratch,
            &["--version"],
            0,
        )?;
        Ok(String::from_utf8_lossy(&answer.stdout).trim().to_string())
    }
}

fn scratch_dir() -> Result<PathBuf, String> {
    let out = Command::new("mktemp")
        .args(["-d", "-t", "u280-perf.XXXXXX"])
        .output()
        .map_err(|e| format!("could not run mktemp: {e}"))?;
    if !out.status.success() {
        return Err("mktemp -d failed".to_string());
    }
    Ok(PathBuf::from(String::from_utf8_lossy(&out.stdout).trim()))
}

fn cmd_run(args: &[String]) -> ExitCode {
    let mut out = None;
    let mut baseline = None;
    let mut after = None;
    let mut judged_only = false;
    let mut it = args.iter();
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "--out" => out = it.next().cloned(),
            "--baseline" => baseline = it.next().cloned(),
            "--after" => after = it.next().cloned(),
            "--judged-only" => judged_only = true,
            other => {
                eprintln!("unknown argument {other}");
                return ExitCode::from(2);
            }
        }
    }
    let (Some(out), Some(baseline), Some(after)) = (out, baseline, after) else {
        eprintln!("usage: perf run --out DIR --baseline SIDE --after SIDE [--judged-only]");
        return ExitCode::from(2);
    };
    let sides = match (
        parse_side("baseline", &baseline),
        parse_side("after", &after),
    ) {
        (Ok(b), Ok(a)) => [b, a],
        (Err(e), _) | (_, Err(e)) => {
            eprintln!("{e}");
            return ExitCode::from(2);
        }
    };
    match run(&out, &sides, judged_only) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("{e}");
            ExitCode::from(1)
        }
    }
}

fn run(out: &str, sides: &[Side; 2], judged_only: bool) -> Result<(), String> {
    // 1 — both fixtures, under a directory `mktemp -d` made for the run.
    let scratch = scratch_dir()?;
    let mut harness = Harness {
        scratch: scratch.clone(),
        nonce: nonce(),
        fixtures: FIXTURES
            .iter()
            .map(|f| (f.to_string(), fixture(f)))
            .collect(),
        kept: BTreeMap::new(),
        made: Vec::new(),
        counter: 0,
    };
    let measured = measure_all(&mut harness, sides, judged_only).and_then(|runs| {
        let versions = sides
            .iter()
            .map(|side| harness.cli_version(side))
            .collect::<Result<Vec<String>, String>>()?;
        Ok((runs, versions))
    });
    let (runs, versions) = match measured {
        Ok(measured) => measured,
        Err(e) => {
            // The measurement's refusal is the one answered; the teardown
            // still runs.
            let _ = harness.tear_down();
            return Err(e);
        }
    };

    // 5 — the two documents.
    if let Err(e) = std::fs::create_dir_all(out) {
        let _ = harness.tear_down();
        return Err(format!("could not write {out}: {e}"));
    }
    for (i, side) in sides.iter().enumerate() {
        let results = Results {
            label: side.label.clone(),
            cli_version: versions[i].clone(),
            server_url: side.url.clone(),
            server_commit: side.server_sha.clone(),
            engine_commit: side.engine_sha.clone(),
            machine: this_machine(),
            runs: runs
                .iter()
                .map(|(fixture, operation, measurements)| Run {
                    fixture: fixture.clone(),
                    operation: operation.clone(),
                    measurements: measurements.iter().map(|pair| pair[i].clone()).collect(),
                })
                .collect(),
        };
        let path = Path::new(out).join(format!("{}.json", side.label));
        let body = serde_json::to_vec_pretty(&results).map_err(|e| e.to_string())?;
        if let Err(e) = std::fs::write(&path, body) {
            let _ = harness.tear_down();
            return Err(format!("could not write {}: {e}", path.display()));
        }
    }
    // Then every repository deleted and the scratch directory removed.
    harness.tear_down()
}

type Measured = Vec<(String, String, Vec<[Measurement; 2]>)>;

fn measure_all(
    harness: &mut Harness,
    sides: &[Side; 2],
    judged_only: bool,
) -> Result<Measured, String> {
    // 2 and 3 — every pair's first measurement.
    let mut runs: Measured = Vec::new();
    for fixture in FIXTURES {
        for operation in OPERATIONS {
            if !timed(fixture, operation) || (judged_only && !judged(fixture, operation)) {
                continue;
            }
            eprintln!("measuring {fixture} {operation}");
            let pair = harness.measure(sides, fixture, operation)?;
            eprintln!(
                "  {fixture} {operation} {:.0} ms -> {:.0} ms",
                pair[0].median_ms, pair[1].median_ms
            );
            runs.push((fixture.to_string(), operation.to_string(), vec![pair]));
        }
    }
    // 4 — a second measurement of each judged pair whose first stands
    // `slower`.
    for (fixture, operation, measurements) in runs.iter_mut() {
        let first = &measurements[0];
        let as_run = |m: &Measurement| Run {
            fixture: fixture.clone(),
            operation: operation.clone(),
            measurements: vec![m.clone()],
        };
        if first_slower(fixture, operation, &as_run(&first[0]), &as_run(&first[1])) {
            eprintln!("measuring {fixture} {operation} a second time");
            let second = harness.measure(sides, fixture, operation)?;
            measurements.push(second);
        }
    }
    Ok(runs)
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        Some("run") => cmd_run(&args[1..]),
        Some("compare") => cmd_compare(&args[1..]),
        _ => {
            eprintln!("usage: perf run ... | perf compare BASELINE AFTER");
            ExitCode::from(2)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn two_generations_of_a_fixture_are_equal_byte_for_byte() {
        for name in FIXTURES {
            assert_eq!(fixture(name), fixture(name), "{name}");
        }
        let text = agent_text();
        assert_eq!(text.len(), 2000);
        let folders: std::collections::BTreeSet<_> = text
            .keys()
            .map(|p| Path::new(p).parent().unwrap().to_path_buf())
            .collect();
        assert_eq!(folders.len(), 200);
        for bytes in text.values() {
            assert!(std::str::from_utf8(bytes).is_ok());
        }
        let mixed = agent_mixed();
        assert_eq!(mixed.len(), 208);
        assert_eq!(mixed["assets/doc/dlim.pdf"].len(), LIM_FILE_SIZE);
        for (path, bytes) in &mixed {
            if !path.ends_with(".md") {
                assert!(bytes[..8192.min(bytes.len())].contains(&0), "{path}");
            }
        }
    }

    #[test]
    fn agent_large_holds_224_files_of_which_24_are_large_text() {
        let large = agent_large();
        assert_eq!(large.len(), 224);
        let mixed = agent_mixed();
        let mut big = 0;
        for (path, bytes) in &large {
            if path.starts_with("large/") {
                big += 1;
                assert_eq!(bytes.len(), 8_388_608, "{path}");
                assert!(std::str::from_utf8(bytes).is_ok(), "{path}");
                assert!(!bytes.contains(&0), "{path}");
            } else {
                assert_eq!(Some(bytes), mixed.get(path), "{path} is agent-mixed's note");
            }
        }
        assert_eq!(big, 24);
    }

    #[test]
    fn agent_large_is_timed_on_push_incremental_alone() {
        for operation in OPERATIONS {
            assert_eq!(
                timed("agent-large", operation),
                operation == "push-incremental",
                "{operation}"
            );
            assert!(timed("agent-text", operation) && timed("agent-mixed", operation));
        }
    }

    #[test]
    fn agent_large_push_incremental_slower_on_both_measurements_stands_slower() {
        let b = results(
            "baseline",
            &[("agent-large", "push-incremental", &[1000.0, 1000.0])],
        );
        let a = results(
            "after",
            &[("agent-large", "push-incremental", &[1300.0, 1250.0])],
        );
        let (lines, slower) = compare(&b, &a).unwrap();
        assert!(slower);
        assert_eq!(
            lines,
            vec!["agent-large push-incremental 1000 ms -> 1300 ms; 1000 ms -> 1250 ms slower"]
        );
    }

    fn results(label: &str, medians: &[(&str, &str, &[f64])]) -> Results {
        Results {
            label: label.to_string(),
            cli_version: "syns 0".into(),
            server_url: "http://127.0.0.1:1".into(),
            server_commit: "s".into(),
            engine_commit: "e".into(),
            machine: this_machine(),
            runs: medians
                .iter()
                .map(|(fixture, operation, ms)| Run {
                    fixture: fixture.to_string(),
                    operation: operation.to_string(),
                    measurements: ms
                        .iter()
                        .map(|m| Measurement {
                            samples_ms: vec![*m; RECORDED],
                            matched_files: vec![],
                            published_files: vec![],
                            median_ms: *m,
                        })
                        .collect(),
                })
                .collect(),
        }
    }

    #[test]
    fn a_pair_slower_on_both_measurements_stands_slower() {
        let b = results("baseline", &[("agent-text", "pull", &[1000.0, 1000.0])]);
        let a = results("after", &[("agent-text", "pull", &[1200.0, 1300.0])]);
        let (lines, slower) = compare(&b, &a).unwrap();
        assert!(slower);
        assert_eq!(
            lines,
            vec!["agent-text pull 1000 ms -> 1200 ms; 1000 ms -> 1300 ms slower"]
        );
    }

    #[test]
    fn a_pair_slower_on_its_first_measurement_alone_stands_ok() {
        let b = results("baseline", &[("agent-text", "pull", &[1000.0, 1000.0])]);
        let a = results("after", &[("agent-text", "pull", &[1200.0, 1020.0])]);
        let (lines, slower) = compare(&b, &a).unwrap();
        assert!(!slower);
        assert!(lines[0].ends_with(" ok"), "{lines:?}");
    }

    #[test]
    fn a_reference_pair_is_never_judged() {
        let b = results("baseline", &[("agent-mixed", "pull", &[1000.0])]);
        let a = results("after", &[("agent-mixed", "pull", &[5000.0])]);
        let (lines, slower) = compare(&b, &a).unwrap();
        assert!(!slower);
        assert_eq!(lines, vec!["agent-mixed pull 1000 ms -> 5000 ms reference"]);
    }

    #[test]
    fn documents_from_two_machines_are_incomparable() {
        let b = results("baseline", &[("agent-text", "pull", &[1000.0])]);
        let mut a = results("after", &[("agent-text", "pull", &[1000.0])]);
        a.machine.cpus += 1;
        assert_eq!(compare(&b, &a), Err("machine".to_string()));
        let dir = std::env::temp_dir().join(format!("u280-perf-test-{}", nonce()));
        std::fs::create_dir_all(&dir).unwrap();
        let (bp, ap) = (dir.join("b.json"), dir.join("a.json"));
        std::fs::write(&bp, serde_json::to_vec(&b).unwrap()).unwrap();
        std::fs::write(&ap, serde_json::to_vec(&a).unwrap()).unwrap();
        let code = cmd_compare(&[bp.display().to_string(), ap.display().to_string()]);
        std::fs::remove_dir_all(&dir).unwrap();
        assert_eq!(code, ExitCode::from(2));
    }

    #[test]
    fn the_median_is_the_mean_of_the_middle_two_of_thirty() {
        let samples: Vec<f64> = (1..=30).map(f64::from).collect();
        assert_eq!(median(&samples), 15.5);
    }
}
