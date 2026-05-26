//! CLI golden-transcript suite (plan/13-tests.md milestone 2): the `eo9` binary driven as
//! a subprocess against a per-test module store — run-by-name through the store, the
//! three-way outcome → exit-code mapping, and the cached-second-run behaviour.
//!
//! Each transcript is the full stdout of a scripted command sequence (normalized only
//! where output is legitimately non-reproducible: content hashes and trap reasons),
//! compared against an inline golden text. Diagnostics intentionally go to stderr, so
//! they are asserted separately and never make the goldens flaky.
//!
//! fs-from-the-CLI cases are deliberately absent: area 11 is wiring the fs provider into
//! the binary in parallel, and this suite must not depend on that work.

use std::fmt::Write as _;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Once;

use eo9_integration::guest;

// -----------------------------------------------------------------------------------------
// Harness: the binary, a per-test store, and the transcript runner
// -----------------------------------------------------------------------------------------

/// The `eo9` binary built by this workspace, locating it next to this test executable
/// (`target/<profile>/eo9`) and building it once if it is missing (e.g. when this suite is
/// run on its own rather than via `cargo test --workspace`).
fn eo9_binary() -> PathBuf {
    static BUILD: Once = Once::new();
    let profile_dir = std::env::current_exe()
        .expect("test executable path")
        .parent()
        .expect("deps dir")
        .parent()
        .expect("profile dir")
        .to_path_buf();
    let binary = profile_dir.join("eo9");
    if !binary.exists() {
        BUILD.call_once(|| {
            let mut args = vec!["build", "-p", "eo9", "--bin", "eo9"];
            if profile_dir.file_name().and_then(|n| n.to_str()) == Some("release") {
                args.push("--release");
            }
            let status = Command::new("cargo")
                .args(&args)
                .current_dir(guest::repo_root())
                .status()
                .expect("failed to invoke cargo to build the eo9 binary");
            assert!(status.success(), "building the eo9 binary failed");
        });
    }
    assert!(
        binary.exists(),
        "eo9 binary is missing at {}",
        binary.display()
    );
    binary
}

/// A fresh store root for one test.
fn temp_store(test: &str) -> PathBuf {
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!("eo9-transcripts-{test}"));
    if dir.exists() {
        fs::remove_dir_all(&dir).expect("failed to clear the test store");
    }
    fs::create_dir_all(&dir).expect("failed to create the test store");
    dir
}

struct Run {
    code: i32,
    stdout: String,
    stderr: String,
}

/// Run `eo9 <args>` with the module store pointed at `store`.
fn eo9(store: &Path, args: &[&str]) -> Run {
    let output = Command::new(eo9_binary())
        .args(args)
        .env("EO9_STORE", store)
        .current_dir(guest::repo_root())
        .output()
        .expect("failed to run the eo9 binary");
    Run {
        code: output.status.code().expect("eo9 exited without a code"),
        stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
    }
}

/// One scripted step: how the command is displayed in the transcript, and the argv that
/// is actually run (they differ only where an absolute component path is involved).
struct Step<'a> {
    shown: &'a str,
    args: Vec<String>,
}

impl<'a> Step<'a> {
    fn new(shown: &'a str, args: &[&str]) -> Self {
        Step {
            shown,
            args: args.iter().map(|a| a.to_string()).collect(),
        }
    }
}

/// Normalize legitimately non-reproducible output: content hashes (64 hex characters)
/// become `<hash>`, and trap reasons become `<reason>`.
fn normalize(line: &str) -> String {
    let line = if line.trim_start().starts_with("abnormal(trapped(") {
        "abnormal(trapped(<reason>))".to_string()
    } else {
        line.to_string()
    };
    line.split_whitespace()
        .map(|token| {
            let is_hash = token.len() == 64
                && token
                    .chars()
                    .all(|c| c.is_ascii_hexdigit() && !c.is_uppercase());
            if is_hash { "<hash>" } else { token }.to_string()
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// Run the scripted steps against one store and render the transcript: the displayed
/// command, the normalized stdout, and the exit code of every step. Returns the rendered
/// transcript plus each step's stderr (for the separate diagnostic assertions).
fn transcript(store: &Path, steps: &[Step<'_>]) -> (String, Vec<String>) {
    let mut rendered = String::new();
    let mut stderrs = Vec::new();
    for step in steps {
        let args: Vec<&str> = step.args.iter().map(String::as_str).collect();
        let run = eo9(store, &args);
        writeln!(rendered, "$ eo9 {}", step.shown).unwrap();
        for line in run.stdout.lines() {
            writeln!(rendered, "{}", normalize(line)).unwrap();
        }
        writeln!(rendered, "[exit {}]", run.code).unwrap();
        stderrs.push(run.stderr);
    }
    (rendered, stderrs)
}

/// The splitmix64 round the cruncher example folds over its seed, reproduced here so the
/// golden digest is computed rather than copied.
fn cruncher_digest(seed: u64, rounds: u64) -> u64 {
    fn mix(state: u64) -> u64 {
        let mut z = state.wrapping_add(0x9e37_79b9_7f4a_7c15);
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        z ^ (z >> 31)
    }
    (0..rounds).fold(seed, |digest, _| mix(digest))
}

fn example_path_arg(example: &str) -> String {
    guest::ensure_components(&[&format!("eo9-example-{example}")]);
    guest::component_path(&format!("eo9-example-{example}"))
        .to_str()
        .expect("utf-8 component path")
        .to_owned()
}

// -----------------------------------------------------------------------------------------
// Transcripts
// -----------------------------------------------------------------------------------------

#[test]
fn store_add_and_run_by_name_golden_transcript() {
    let store = temp_store("run-by-name");
    let cruncher = example_path_arg("cruncher");
    let digest = cruncher_digest(9, 3);

    let steps = [
        Step::new(
            "store add eo9-example-cruncher.wasm --name cruncher",
            &["store", "add", &cruncher, "--name", "cruncher"],
        ),
        Step::new(
            "run cruncher --seed 9 --rounds 3",
            &["run", "cruncher", "--seed", "9", "--rounds", "3"],
        ),
        Step::new(
            "run cruncher --seed 9 --rounds 3",
            &["run", "cruncher", "--seed", "9", "--rounds", "3"],
        ),
        Step::new("run no-such-name", &["run", "no-such-name"]),
    ];
    let (rendered, stderrs) = transcript(&store, &steps);

    let expected = format!(
        "$ eo9 store add eo9-example-cruncher.wasm --name cruncher\n\
         <hash>\n\
         cruncher -> <hash>\n\
         [exit 0]\n\
         $ eo9 run cruncher --seed 9 --rounds 3\n\
         success(digest({digest}))\n\
         [exit 0]\n\
         $ eo9 run cruncher --seed 9 --rounds 3\n\
         success(digest({digest}))\n\
         [exit 0]\n\
         $ eo9 run no-such-name\n\
         [exit 3]\n"
    );
    assert_eq!(rendered, expected, "stderr of the steps: {stderrs:#?}");

    // The failing resolution explains itself on stderr (exit code 3 = eo9's own error,
    // before any program outcome existed).
    assert!(
        stderrs[3].contains("eo9: error:") && stderrs[3].contains("no-such-name"),
        "unexpected resolution error: {}",
        stderrs[3]
    );
}

#[test]
fn outcome_arms_map_to_exit_codes_golden_transcript() {
    let store = temp_store("outcome-arms");
    let outcomes = example_path_arg("outcomes");

    let steps = [
        Step::new(
            "store add eo9-example-outcomes.wasm --name outcomes",
            &["store", "add", &outcomes, "--name", "outcomes"],
        ),
        Step::new(
            "run outcomes --mode ok --detail \"all good\"",
            &["run", "outcomes", "--mode", "ok", "--detail", "all good"],
        ),
        Step::new(
            "run outcomes --mode fail --detail \"went wrong\"",
            &[
                "run",
                "outcomes",
                "--mode",
                "fail",
                "--detail",
                "went wrong",
            ],
        ),
        Step::new(
            "run outcomes --mode trap --detail \"\"",
            &["run", "outcomes", "--mode", "trap", "--detail", ""],
        ),
    ];
    let (rendered, stderrs) = transcript(&store, &steps);

    let expected = "$ eo9 store add eo9-example-outcomes.wasm --name outcomes\n\
         <hash>\n\
         outcomes -> <hash>\n\
         [exit 0]\n\
         $ eo9 run outcomes --mode ok --detail \"all good\"\n\
         outcomes: completing with \"all good\"\n\
         success(completed(\"all good\"))\n\
         [exit 0]\n\
         $ eo9 run outcomes --mode fail --detail \"went wrong\"\n\
         failure(requested-failure(\"went wrong\"))\n\
         [exit 1]\n\
         $ eo9 run outcomes --mode trap --detail \"\"\n\
         abnormal(trapped(<reason>))\n\
         [exit 2]\n";
    assert_eq!(rendered, expected, "stderr of the steps: {stderrs:#?}");

    // The failure mode's own diagnostic goes to the program's stderr stream, not stdout.
    assert!(
        stderrs[2].contains("outcomes: failing with"),
        "expected the program's stderr line: {}",
        stderrs[2]
    );
}

#[test]
fn the_second_run_by_name_launches_from_the_cached_image() {
    let store = temp_store("cache-second-run");
    let cruncher = example_path_arg("cruncher");

    let add = eo9(&store, &["store", "add", &cruncher, "--name", "cruncher"]);
    assert_eq!(add.code, 0, "stderr: {}", add.stderr);

    let args = ["-v", "run", "cruncher", "--seed", "4", "--rounds", "10"];
    let first = eo9(&store, &args);
    assert_eq!(first.code, 0, "stderr: {}", first.stderr);
    assert!(
        first.stderr.contains("compile cache miss"),
        "first run should miss the cache: {}",
        first.stderr
    );

    let second = eo9(&store, &args);
    assert_eq!(second.code, 0, "stderr: {}", second.stderr);
    assert!(
        second.stderr.contains("launched from cached image"),
        "second run should launch from the cached image: {}",
        second.stderr
    );
    assert_eq!(
        second.stdout, first.stdout,
        "a cache hit must not change the program's outcome"
    );
    assert_eq!(
        first.stdout.trim(),
        format!("success(digest({}))", cruncher_digest(4, 10))
    );
}
