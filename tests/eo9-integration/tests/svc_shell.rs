//! Shell-level service tests (executor v1): the `eo9` binary driven as a subprocess with
//! a scripted stdin session, exercising the `--svc` grant, the eosh `detach`/`svc`
//! builtins, capability soundness through the shell, restart policies, and the
//! process-bound registry lifetime (owner ruling E).
//!
//! The registry mechanics themselves are covered by `svc_registry.rs` (direct, fast);
//! this suite covers the layers above it: the CLI flag, the linker grant plumbing, the
//! eosh builtins, and the end-of-session teardown.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Once;

use eo9_integration::guest;

/// The `eo9` binary built by this workspace (same locator as the CLI transcripts).
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
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!("eo9-svc-shell-{test}"));
    if dir.exists() {
        std::fs::remove_dir_all(&dir).expect("failed to clear the test store");
    }
    std::fs::create_dir_all(&dir).expect("failed to create the test store");
    dir
}

struct Session {
    stdout: String,
    stderr: String,
    code: i32,
}

/// Run `eo9 [args] shell` with `lines` piped one per line into eosh's stdin, against a
/// fresh store. The guest components must already be built (the eo9 binary embeds them
/// at its own build time, so this just ensures the dev tree exists for seeding parity).
fn shell_session(store: &Path, extra_args: &[&str], lines: &[&str]) -> Session {
    guest::ensure_components(&["eosh", "eo9-stub-restart-never"]);
    let mut command = Command::new(eo9_binary());
    command
        .args(extra_args)
        .arg("shell")
        .env("EO9_STORE", store)
        .current_dir(guest::repo_root())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = command.spawn().expect("failed to spawn the eo9 binary");
    {
        let stdin = child.stdin.as_mut().expect("piped stdin");
        let mut script = lines.join("\n");
        script.push('\n');
        stdin
            .write_all(script.as_bytes())
            .expect("writing the session script");
    }
    let output = child.wait_with_output().expect("waiting for the session");
    Session {
        stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        code: output.status.code().unwrap_or(-1),
    }
}

// -------------------------------------------------------------------------------------
// The explicit grant (owner ruling B)
// -------------------------------------------------------------------------------------

/// Without `--svc`, the svc builtins exist but refuse with advice naming the flag; the
/// shell itself still works (the capability is absent, not broken).
#[test]
fn without_the_flag_svc_builtins_refuse_with_advice() {
    let store = temp_store("no-flag");
    let session = shell_session(
        &store,
        &[],
        &[
            "svc list",
            "detach w = cruncher --seed 1 --rounds 5 restart restart.never",
            "exit",
        ],
    );
    assert_eq!(session.code, 0, "the shell session itself exits cleanly");
    let all = format!("{}{}", session.stdout, session.stderr);
    assert!(
        all.contains("needs the eo9:svc capability"),
        "the refusal names the capability:\n{all}"
    );
    assert!(
        all.contains("--svc"),
        "the refusal says how to get it:\n{all}"
    );
    assert!(
        !all.contains("detached:"),
        "nothing was actually detached:\n{all}"
    );
}

/// With `--svc`, the same session works: the registry exists and is empty.
#[test]
fn with_the_flag_the_registry_exists_and_is_empty() {
    let store = temp_store("with-flag");
    let session = shell_session(&store, &["--svc"], &["svc list", "exit"]);
    assert_eq!(session.code, 0);
    assert!(
        session.stdout.contains("no services"),
        "an empty registry says so:\n{}",
        session.stdout
    );
}

// -------------------------------------------------------------------------------------
// The full lifecycle through the shell
// -------------------------------------------------------------------------------------

/// detach → svc list (running) → svc stop → svc list (finished) → svc clear → empty.
#[test]
fn detach_list_stop_clear_lifecycle() {
    let store = temp_store("lifecycle");
    let session = shell_session(
        &store,
        &["--svc"],
        &[
            // Big rounds: still running whenever we look.
            "detach worker = cruncher --seed 1 --rounds 900000000 restart restart.never",
            "svc list",
            "svc stop worker",
            "svc list",
            "svc clear worker",
            "svc list",
            "exit",
        ],
    );
    assert_eq!(session.code, 0);
    let out = &session.stdout;
    assert!(
        out.contains("detached: worker is running in the background"),
        "detach confirms:\n{out}"
    );
    assert!(
        out.contains("worker") && out.contains("running"),
        "the list shows it running:\n{out}"
    );
    assert!(
        out.contains("stopped: worker (abnormal(killed))"),
        "stop kills and reports:\n{out}"
    );
    assert!(out.contains("finished"), "stopped shows finished:\n{out}");
    assert!(out.contains("cleared: worker"), "clear works:\n{out}");
    assert!(
        out.contains("no services"),
        "the registry is empty at the end:\n{out}"
    );
}

// -------------------------------------------------------------------------------------
// Capability soundness through the shell
// -------------------------------------------------------------------------------------

/// A composition that still requires fs is refused at detach with the typed explanation —
/// the shell's own fs grant is NOT lent to detached children.
#[test]
fn soundness_a_detached_child_cannot_use_what_its_detacher_did_not_compose() {
    let store = temp_store("soundness");
    let session = shell_session(
        &store,
        &["--svc"],
        &[
            // readwrite requires eo9:fs. The shell session HAS fs (the /bin overlay), but
            // a detached service runs only with what was composed into it — so this must
            // be refused, naming fs.
            "detach writer = readwrite --path /tmp/x --contents hi restart restart.never",
            "svc list",
            "exit",
        ],
    );
    assert_eq!(session.code, 0);
    let all = format!("{}{}", session.stdout, session.stderr);
    assert!(
        all.contains("still requires eo9:fs"),
        "the refusal names the missing capability:\n{all}"
    );
    assert!(
        all.contains("no services"),
        "nothing was registered:\n{all}"
    );
}

// -------------------------------------------------------------------------------------
// Restart policies + log capture through the shell
// -------------------------------------------------------------------------------------

/// A finished service's log is readable; restart.always restarts a quick service while
/// the foreground does other things (the background-execution proof).
#[test]
fn logs_are_captured_and_restart_always_restarts_in_the_background() {
    let store = temp_store("logs-restarts");
    let session = shell_session(
        &store,
        &["--svc"],
        &[
            // A quick service under restart.always: every run finishes fast, the policy
            // brings it back, so the restart count climbs while the foreground works.
            "detach phoenix = cruncher --seed 1 --rounds 100 restart restart.always",
            // A capture-logged one-shot greeter (time sealed; text = the log).
            "detach greeter = time.frozen $ hello --name background restart restart.never",
            // Foreground work. Services advance on the root drive loop's pump, and a
            // *blocked* foreground is what yields the loop's parked 10ms wake windows —
            // the same idle a real interactive session has between keystrokes. The
            // sockcheck line blocks on real loopback I/O, providing those windows
            // deterministically. (Three quick hellos used to provide them by accident;
            // the session resolve cache made trivial lines too fast for that.)
            "hello --name one",
            "hello --name two",
            "hello --name three",
            "net.l4.loopback $ sockcheck --payload pacing",
            "svc list",
            "svc log greeter",
            "svc stop phoenix",
            "exit",
        ],
    );
    assert_eq!(session.code, 0);
    let out = &session.stdout;

    // The greeter ran in the background and its output is in the log.
    assert!(
        out.contains("Hello, background"),
        "the captured log holds the service's output:\n{out}"
    );

    // Phoenix was restarted at least once by restart.always (the list shows a restart
    // count; asserting ">= 1" textually: the RESTARTS column for phoenix is not 0).
    let phoenix_line = out
        .lines()
        .find(|line| line.trim_start().starts_with("phoenix"))
        .unwrap_or_else(|| panic!("phoenix appears in svc list:\n{out}"));
    let restarts: u32 = phoenix_line
        .split_whitespace()
        .nth(2)
        .and_then(|column| column.parse().ok())
        .unwrap_or_else(|| panic!("the phoenix line has a restart count: {phoenix_line}"));
    assert!(
        restarts >= 1,
        "restart.always restarted phoenix while the foreground ran ({restarts} restarts):\n{out}"
    );
}

// -------------------------------------------------------------------------------------
// Process-bound lifetime (owner ruling E)
// -------------------------------------------------------------------------------------

/// When the shell (and with it the eo9 process) exits, still-running services are
/// stopped and reported — services live exactly as long as the root process.
#[test]
fn services_die_with_the_process() {
    let store = temp_store("lifetime");
    let session = shell_session(
        &store,
        &["--svc"],
        &[
            "detach forever = cruncher --seed 1 --rounds 900000000 restart restart.always",
            "exit",
        ],
    );
    assert_eq!(session.code, 0);
    assert!(
        session
            .stderr
            .contains("services live only as long as this eo9 process"),
        "the teardown explains the lifetime rule:\n{}",
        session.stderr
    );
    assert!(
        session.stderr.contains("stopped: forever"),
        "the still-running service is named in the teardown:\n{}",
        session.stderr
    );
    // The process actually exited (wait_with_output returned) and nothing is left
    // running: there is no daemon to leak by construction (the registry lives in the
    // process). This assertion is the test completing at all.
}

// -------------------------------------------------------------------------------------
// `eo9 -c` one-shot form
// -------------------------------------------------------------------------------------

/// `-c` one-shot sessions can also hold the grant: the service starts, then dies with
/// the process at the end of the one command.
#[test]
fn one_shot_sessions_grant_and_tear_down() {
    let store = temp_store("one-shot");
    guest::ensure_components(&["eosh", "eo9-stub-restart-never"]);
    let output = Command::new(eo9_binary())
        .args([
            "--svc",
            "-c",
            "detach blip = cruncher --seed 1 --rounds 900000000 restart restart.never",
        ])
        .env("EO9_STORE", temp_store("one-shot-store"))
        .current_dir(guest::repo_root())
        .output()
        .expect("failed to run the eo9 binary");
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stdout.contains("detached: blip"),
        "the one-shot detach succeeded:\n{stdout}\n{stderr}"
    );
    assert!(
        stderr.contains("stopped: blip"),
        "the service was stopped when the process ended:\n{stderr}"
    );
    let _ = store;
}
