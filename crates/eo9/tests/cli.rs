//! End-to-end tests of the `eo9` binary against the merged example components
//! (plan/11-usermode.md): `run` of hello / outcomes / cruncher with the three-way
//! outcome and exit codes, compile-cache behaviour on a second run, the memory-limit
//! flag, store-resolved names, `describe`, `compile`, `store` subcommands, and the
//! readwrite end to end through the unix fs provider, and the `shell` stub.
//!
//! The tests drive the real binary (`CARGO_BIN_EXE_eo9`) as a subprocess, with the
//! module store pointed at a per-test directory under `CARGO_TARGET_TMPDIR`. Example
//! components are built on demand with `cargo xtask build-guest` if they are missing.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Once;

const EXAMPLES: &[&str] = &["hello", "outcomes", "cruncher", "readwrite"];

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .expect("repository root must exist")
}

fn component_path(name: &str) -> PathBuf {
    repo_root()
        .join("guest/target/components")
        .join(format!("eo9-example-{name}.wasm"))
}

/// Build the example components (once per test process) if any are missing.
fn ensure_components() {
    static BUILD: Once = Once::new();
    BUILD.call_once(|| {
        if EXAMPLES.iter().all(|name| component_path(name).exists()) {
            return;
        }
        let status = Command::new("cargo")
            .args(["run", "-p", "xtask", "--", "build-guest"])
            .current_dir(repo_root())
            .status()
            .expect("failed to invoke `cargo run -p xtask -- build-guest`");
        assert!(status.success(), "xtask build-guest failed");
    });
    for name in EXAMPLES {
        assert!(
            component_path(name).exists(),
            "missing example component {}",
            component_path(name).display()
        );
    }
}

/// A fresh store root for one test.
fn temp_store(test: &str) -> PathBuf {
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!("eo9-cli-{test}"));
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

/// Run the eo9 binary with `args`, using `store` as the module store root.
fn eo9(store: &Path, args: &[&str]) -> Run {
    ensure_components();
    let output = Command::new(env!("CARGO_BIN_EXE_eo9"))
        .args(args)
        .env("EO9_STORE", store)
        .current_dir(repo_root())
        .output()
        .expect("failed to run the eo9 binary");
    Run {
        code: output.status.code().expect("eo9 exited without a code"),
        stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
    }
}

fn component_arg(name: &str) -> String {
    component_path(name)
        .to_str()
        .expect("utf-8 path")
        .to_owned()
}

// -----------------------------------------------------------------------------------
// run: the example binaries end to end
// -----------------------------------------------------------------------------------

#[test]
fn run_hello_by_path_end_to_end() {
    let store = temp_store("run-hello");
    let hello = component_arg("hello");
    let run = eo9(
        &store,
        &["run", &hello, "--name", "eo9", "--excited", "true"],
    );
    assert_eq!(run.code, 0, "stderr: {}", run.stderr);
    assert!(
        run.stdout.contains("Hello, eo9!"),
        "program output missing from stdout: {}",
        run.stdout
    );
    assert_eq!(
        run.stdout.lines().last(),
        Some("success(greeted)"),
        "outcome line missing from stdout: {}",
        run.stdout
    );
}

#[test]
fn run_outcomes_covers_success_failure_and_abnormal() {
    let store = temp_store("run-outcomes");
    let outcomes = component_arg("outcomes");
    let run_mode = |mode: &str, detail: &str| {
        eo9(
            &store,
            &["run", &outcomes, "--mode", mode, "--detail", detail],
        )
    };

    let ok = run_mode("ok", "all good");
    assert_eq!(ok.code, 0, "stderr: {}", ok.stderr);
    assert_eq!(
        ok.stdout.lines().last(),
        Some("success(completed(\"all good\"))")
    );

    let quiet = run_mode("quiet", "");
    assert_eq!(quiet.code, 0, "stderr: {}", quiet.stderr);
    assert_eq!(quiet.stdout.trim(), "success(quiet)");

    let fail = run_mode("fail", "went wrong");
    assert_eq!(fail.code, 1, "stderr: {}", fail.stderr);
    assert_eq!(
        fail.stdout.lines().last(),
        Some("failure(requested-failure(\"went wrong\"))")
    );

    let rejected = run_mode("nonsense", "");
    assert_eq!(rejected.code, 1, "stderr: {}", rejected.stderr);
    assert!(
        rejected.stdout.trim().starts_with("failure(bad-arguments("),
        "unexpected outcome: {}",
        rejected.stdout
    );

    // A guest panic lowers to a wasm trap: the executor's abnormal arm, exit code 2.
    let trapped = run_mode("trap", "");
    assert_eq!(trapped.code, 2, "stderr: {}", trapped.stderr);
    assert!(
        trapped.stdout.trim().starts_with("abnormal(trapped("),
        "unexpected outcome: {}",
        trapped.stdout
    );
    assert!(
        trapped.stdout.contains("unreachable"),
        "trap reason missing: {}",
        trapped.stdout
    );
}

#[test]
fn run_cruncher_is_deterministic_pure_compute() {
    let store = temp_store("run-cruncher");
    let cruncher = component_arg("cruncher");

    // The same splitmix64 mix as the example, so the digest can be checked exactly.
    fn mix(state: u64) -> u64 {
        let mut z = state.wrapping_add(0x9e37_79b9_7f4a_7c15);
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        z ^ (z >> 31)
    }
    let expected = (0..1000).fold(9u64, |digest, _| mix(digest));

    let first = eo9(
        &store,
        &["run", &cruncher, "--seed", "9", "--rounds", "1000"],
    );
    assert_eq!(first.code, 0, "stderr: {}", first.stderr);
    assert_eq!(first.stdout.trim(), format!("success(digest({expected}))"));

    // Pure compute with fixed arguments is bit-deterministic run to run.
    let second = eo9(
        &store,
        &["run", &cruncher, "--seed", "9", "--rounds", "1000"],
    );
    assert_eq!(second.code, 0, "stderr: {}", second.stderr);
    assert_eq!(second.stdout, first.stdout);

    // Zero rounds is rejected in the program's own failure vocabulary.
    let rejected = eo9(&store, &["run", &cruncher, "--seed", "9", "--rounds", "0"]);
    assert_eq!(rejected.code, 1);
    assert!(rejected.stdout.trim().starts_with("failure(bad-arguments("));
}

// -----------------------------------------------------------------------------------
// Compile cache
// -----------------------------------------------------------------------------------

#[test]
fn second_run_launches_from_the_cached_image() {
    let store = temp_store("cache-hit");
    let cruncher = component_arg("cruncher");
    let args = ["-v", "run", &cruncher, "--seed", "1", "--rounds", "10"];

    let first = eo9(&store, &args);
    assert_eq!(first.code, 0, "stderr: {}", first.stderr);
    assert!(
        first.stderr.contains("compile cache miss"),
        "first run should miss: {}",
        first.stderr
    );
    assert!(
        first.stderr.contains("cached image"),
        "first run should cache the image it compiled: {}",
        first.stderr
    );

    // The second run takes the cached path: it deserializes the stored image and never
    // reaches codegen (no "compiling"/"compiled" diagnostics), yet the outcome is
    // identical.
    let second = eo9(&store, &args);
    assert_eq!(second.code, 0, "stderr: {}", second.stderr);
    assert!(
        second.stderr.contains("launched from cached image"),
        "second run should launch from the cache: {}",
        second.stderr
    );
    assert!(
        !second.stderr.contains("compiling") && !second.stderr.contains("compiled"),
        "second run must not pay codegen: {}",
        second.stderr
    );
    assert_eq!(second.stdout, first.stdout);

    // The store's cache entry records both uses (insert counts as the first).
    let cache_dir = store.join("cache");
    let mut use_counts = Vec::new();
    for entry in fs::read_dir(&cache_dir).expect("cache directory must exist") {
        let meta = entry.expect("readable cache entry").path().join("meta");
        let text = fs::read_to_string(meta).expect("readable cache metadata");
        let count = text
            .lines()
            .find_map(|line| line.strip_prefix("use-count "))
            .expect("metadata carries a use-count")
            .parse::<u64>()
            .expect("use-count is a number");
        use_counts.push(count);
    }
    assert_eq!(
        use_counts,
        vec![2],
        "unexpected cache usage: {use_counts:?}"
    );
}

#[test]
fn corrupted_cache_entries_are_ignored_not_trusted() {
    let store = temp_store("cache-corrupt");
    let cruncher = component_arg("cruncher");
    let args = ["-v", "run", &cruncher, "--seed", "3", "--rounds", "10"];

    let first = eo9(&store, &args);
    assert_eq!(first.code, 0, "stderr: {}", first.stderr);

    // Flip the last byte of the cached artifact: the envelope's recorded content hash no
    // longer matches, so the entry must be refused and the component recompiled.
    let cache_dir = store.join("cache");
    let entry = fs::read_dir(&cache_dir)
        .expect("cache directory must exist")
        .next()
        .expect("one cache entry")
        .expect("readable cache entry");
    let image_path = entry.path().join("image");
    let mut bytes = fs::read(&image_path).expect("readable cached image");
    let last = bytes.len() - 1;
    bytes[last] ^= 0x01;
    fs::write(&image_path, &bytes).expect("cached image is writable in the test store");

    let second = eo9(&store, &args);
    assert_eq!(second.code, 0, "stderr: {}", second.stderr);
    assert!(
        second.stderr.contains("ignoring compile-cache entry"),
        "the tampered entry should be refused: {}",
        second.stderr
    );
    assert!(
        !second.stderr.contains("launched from cached image"),
        "the tampered entry must not be launched: {}",
        second.stderr
    );
    assert_eq!(second.stdout, first.stdout);
}

#[test]
fn an_unwritable_cache_never_fails_a_run() {
    use std::os::unix::fs::PermissionsExt;

    fn set_mode(path: &Path, mode: u32) {
        fs::set_permissions(path, fs::Permissions::from_mode(mode))
            .unwrap_or_else(|err| panic!("cannot chmod {}: {err}", path.display()));
    }

    let store = temp_store("cache-readonly");
    let cruncher = component_arg("cruncher");
    let args = ["-v", "run", &cruncher, "--seed", "5", "--rounds", "10"];

    // Cold cache, read-only cache directory: the insert after compiling fails, the run
    // still succeeds with a warning.
    let cache_dir = store.join("cache");
    fs::create_dir_all(&cache_dir).expect("create the cache directory");
    set_mode(&cache_dir, 0o555);
    let cold = eo9(&store, &args);
    set_mode(&cache_dir, 0o755);
    assert_eq!(cold.code, 0, "stderr: {}", cold.stderr);
    assert!(
        cold.stderr.contains("could not be cached"),
        "expected an insert warning: {}",
        cold.stderr
    );
    assert!(cold.stdout.trim().starts_with("success(digest("));

    // Populate the cache normally, then make the entry read-only: the lookup's
    // use-count bump fails, which is treated as a miss (warn + recompile), not an error.
    let warm = eo9(&store, &args);
    assert_eq!(warm.code, 0, "stderr: {}", warm.stderr);
    let entry_dir = fs::read_dir(&cache_dir)
        .expect("cache directory must exist")
        .next()
        .expect("one cache entry")
        .expect("readable cache entry")
        .path();
    set_mode(&entry_dir, 0o555);
    let bumped = eo9(&store, &args);
    set_mode(&entry_dir, 0o755);
    assert_eq!(bumped.code, 0, "stderr: {}", bumped.stderr);
    assert!(
        bumped.stderr.contains("compile-cache lookup failed"),
        "expected a lookup warning: {}",
        bumped.stderr
    );
    assert_eq!(bumped.stdout, cold.stdout);
}

#[test]
fn compile_warms_the_cache_for_a_later_run() {
    let store = temp_store("compile-warm");
    let outcomes = component_arg("outcomes");

    let warm = eo9(&store, &["compile", &outcomes]);
    assert_eq!(warm.code, 0, "stderr: {}", warm.stderr);
    assert!(
        warm.stdout.starts_with("compiled and cached:"),
        "unexpected compile output: {}",
        warm.stdout
    );

    let run = eo9(
        &store,
        &["-v", "run", &outcomes, "--mode", "quiet", "--detail", ""],
    );
    assert_eq!(run.code, 0, "stderr: {}", run.stderr);
    assert!(
        run.stderr.contains("launched from cached image"),
        "run after warm should launch from the cache: {}",
        run.stderr
    );

    let rewarm = eo9(&store, &["compile", &outcomes]);
    assert_eq!(rewarm.code, 0);
    assert!(rewarm.stdout.starts_with("compile cache hit:"));
}

// -----------------------------------------------------------------------------------
// Limits and refusal paths
// -----------------------------------------------------------------------------------

#[test]
fn memory_limit_flag_is_enforced() {
    let store = temp_store("memory-limit");
    let cruncher = component_arg("cruncher");

    // Far below the component's initial linear memory: the limiter refuses the very
    // first allocation and the spawn fails before any guest code runs.
    let denied = eo9(
        &store,
        &[
            "--max-memory",
            "65536",
            "run",
            &cruncher,
            "--seed",
            "1",
            "--rounds",
            "10",
        ],
    );
    assert_eq!(denied.code, 3, "stdout: {}", denied.stdout);
    assert!(
        denied.stderr.contains("memory"),
        "expected a memory-limit spawn error: {}",
        denied.stderr
    );

    // A generous ceiling changes nothing about a well-behaved program.
    let allowed = eo9(
        &store,
        &[
            "--max-memory",
            "67108864",
            "run",
            &cruncher,
            "--seed",
            "1",
            "--rounds",
            "10",
        ],
    );
    assert_eq!(allowed.code, 0, "stderr: {}", allowed.stderr);
}

#[test]
fn readwrite_round_trips_through_the_unix_fs() {
    // The async fs example end to end from the CLI: the program's eo9:fs capability is
    // the unix fs provider rooted at --fs-root, the owned-buffer write/read round-trip
    // goes through real host files, and the outcome is the program's own success value.
    let store = temp_store("readwrite");
    let fs_root = temp_store("readwrite-fsroot");
    let readwrite = component_arg("readwrite");
    let run = eo9(
        &store,
        &[
            "--fs-root",
            fs_root.to_str().expect("utf-8 fs root"),
            "run",
            &readwrite,
            "--path",
            "note.txt",
            "--contents",
            "hello disk",
        ],
    );
    assert_eq!(run.code, 0, "stderr: {}", run.stderr);
    assert_eq!(run.stdout.trim(), "success(round-tripped(10))");

    // The write really landed on the host filesystem, under the fs root.
    assert_eq!(
        fs::read_to_string(fs_root.join("note.txt")).expect("note.txt exists under the fs root"),
        "hello disk"
    );
}

#[test]
fn readwrite_failures_stay_in_the_programs_vocabulary_and_inside_the_root() {
    let store = temp_store("readwrite-fail");
    let fs_root = temp_store("readwrite-fail-fsroot");
    let readwrite = component_arg("readwrite");
    let run_with_path = |path: &str| {
        eo9(
            &store,
            &[
                "--fs-root",
                fs_root.to_str().expect("utf-8 fs root"),
                "run",
                &readwrite,
                "--path",
                path,
                "--contents",
                "should not land",
            ],
        )
    };

    // A path that tries to climb out of the fs root is refused by the provider's
    // containment and surfaces as the program's own fs(...) failure case.
    let escape = run_with_path("../escaped.txt");
    assert_eq!(escape.code, 1, "stderr: {}", escape.stderr);
    assert!(
        escape.stdout.trim().starts_with("failure(fs("),
        "expected the program's fs failure: {}",
        escape.stdout
    );
    assert!(
        escape.stdout.contains("Denied"),
        "expected a denied error: {}",
        escape.stdout
    );
    assert!(
        !fs_root
            .parent()
            .expect("fs root has a parent")
            .join("escaped.txt")
            .exists(),
        "nothing may be created outside the fs root"
    );

    // Opening inside a directory that does not exist fails through the provider and is
    // reported the same way (not-found, in the program's vocabulary).
    let missing = run_with_path("no-such-dir/note.txt");
    assert_eq!(missing.code, 1, "stderr: {}", missing.stderr);
    assert!(
        missing.stdout.trim().starts_with("failure(fs("),
        "expected the program's fs failure: {}",
        missing.stdout
    );
}

#[test]
fn missing_arguments_are_a_spawn_error() {
    let store = temp_store("missing-args");
    let hello = component_arg("hello");
    let run = eo9(&store, &["run", &hello, "--name", "eo9"]);
    assert_eq!(run.code, 3, "stdout: {}", run.stdout);
    assert!(
        run.stderr.contains("missing argument `excited`"),
        "unexpected error: {}",
        run.stderr
    );
}

#[test]
fn shell_is_a_clear_stub() {
    let store = temp_store("shell");
    let run = eo9(&store, &["shell"]);
    assert_eq!(run.code, 3);
    assert!(
        run.stderr.contains("not available yet"),
        "unexpected stub message: {}",
        run.stderr
    );
}

// -----------------------------------------------------------------------------------
// Store, names, describe
// -----------------------------------------------------------------------------------

#[test]
fn store_add_ls_gc_and_run_by_name() {
    let store = temp_store("store-name");
    let hello = component_arg("hello");

    let added = eo9(&store, &["store", "add", &hello, "--name", "hello"]);
    assert_eq!(added.code, 0, "stderr: {}", added.stderr);
    assert!(
        added.stdout.contains("hello -> "),
        "binding missing from output: {}",
        added.stdout
    );

    let listed = eo9(&store, &["store", "ls"]);
    assert_eq!(listed.code, 0, "stderr: {}", listed.stderr);
    assert!(
        listed.stdout.contains("hello "),
        "ls output: {}",
        listed.stdout
    );
    assert!(
        listed.stdout.contains("objects: 1"),
        "ls output: {}",
        listed.stdout
    );

    // Bare dotted names resolve through the store and run exactly like paths.
    let run = eo9(
        &store,
        &["run", "hello", "--name", "store", "--excited", "false"],
    );
    assert_eq!(run.code, 0, "stderr: {}", run.stderr);
    assert!(
        run.stdout.contains("Hello, store."),
        "stdout: {}",
        run.stdout
    );
    assert_eq!(run.stdout.lines().last(), Some("success(greeted)"));

    let unknown = eo9(&store, &["run", "nosuchname"]);
    assert_eq!(unknown.code, 3);
    assert!(
        unknown.stderr.contains("does not resolve"),
        "unexpected error: {}",
        unknown.stderr
    );

    // gc under the default budget keeps everything and reports what it saw.
    let gc = eo9(&store, &["store", "gc"]);
    assert_eq!(gc.code, 0, "stderr: {}", gc.stderr);
    assert!(
        gc.stdout.starts_with("gc: evicted 0"),
        "gc output: {}",
        gc.stdout
    );

    // gc with a zero budget evicts the cache entry the run created.
    let gc_all = eo9(&store, &["store", "gc", "--max-cache-bytes", "0"]);
    assert_eq!(gc_all.code, 0, "stderr: {}", gc_all.stderr);
    assert!(
        gc_all.stdout.starts_with("gc: evicted 1"),
        "gc output: {}",
        gc_all.stdout
    );
}

#[test]
fn describe_shows_kind_imports_and_arguments() {
    let store = temp_store("describe");
    let hello = component_arg("hello");
    let run = eo9(&store, &["describe", &hello]);
    assert_eq!(run.code, 0, "stderr: {}", run.stderr);
    assert!(run.stdout.contains("kind: binary"), "{}", run.stdout);
    assert!(
        run.stdout.contains("eo9:text/text@0.1.0 (required)"),
        "{}",
        run.stdout
    );
    assert!(
        run.stdout.contains("eo9:time/time@0.1.0 (required)"),
        "{}",
        run.stdout
    );
    assert!(run.stdout.contains("--name <string>"), "{}", run.stdout);
    assert!(run.stdout.contains("--excited <bool>"), "{}", run.stdout);
}

#[test]
fn unknown_commands_and_bad_flags_are_usage_errors() {
    let store = temp_store("usage");
    let unknown = eo9(&store, &["frobnicate"]);
    assert_eq!(unknown.code, 3);
    assert!(unknown.stderr.contains("unknown command"));

    let bad_policy = eo9(&store, &["--exec-snapshot", "maybe", "run", "x.wasm"]);
    assert_eq!(bad_policy.code, 3);
    assert!(bad_policy.stderr.contains("clone-or-refuse"));

    let help = eo9(&store, &["help"]);
    assert_eq!(help.code, 0);
    assert!(help.stdout.contains("USAGE"));
}
