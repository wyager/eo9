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
    let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
    // The zero-findings liveness gate (SPEC: a backstop firing that discovers stranded
    // work is a high-priority bug; docs/spikes/backstop-audit.md): every service-bearing
    // session in this suite is a busy workload, and none may trip the park backstop's
    // stranded-work detector. If this fires, do not relax it — find the missing wake edge.
    assert!(
        !stderr.contains("liveness:"),
        "the park backstop found stranded work during the session:\n{stderr}"
    );
    Session {
        stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
        stderr,
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
            // Foreground work — each command gives the registry thousands of pump slices.
            "hello --name one",
            "hello --name two",
            "hello --name three",
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
// The park path (docs/spikes/registry-liveness.md)
// -------------------------------------------------------------------------------------

/// Run a session whose stdin is held open and quiet between two line batches: the drive
/// loop spends the gap parked on a blocked `read-line`, which is exactly the edge the
/// park wake-set serves (restart deadlines and, in the future, service completions).
/// The gap starts only once `marker` has appeared on stdout (e.g. the `detached:`
/// acknowledgment), so slow startup (debug builds) cannot consume the quiet window.
fn shell_session_with_gap(
    store: &Path,
    extra_args: &[&str],
    before: &[&str],
    marker: &str,
    gap: std::time::Duration,
    after: &[&str],
) -> Session {
    use std::io::Read;
    use std::sync::{Arc, Mutex};

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

    // Drain stdout on a thread into a shared buffer so the main thread can watch for the
    // marker without deadlocking the child on a full pipe.
    let mut stdout_pipe = child.stdout.take().expect("piped stdout");
    let stdout_buf: Arc<Mutex<Vec<u8>>> = Arc::new(Mutex::new(Vec::new()));
    let reader_buf = Arc::clone(&stdout_buf);
    let reader = std::thread::spawn(move || {
        let mut chunk = [0u8; 4096];
        loop {
            match stdout_pipe.read(&mut chunk) {
                Ok(0) | Err(_) => break,
                Ok(n) => reader_buf.lock().unwrap().extend_from_slice(&chunk[..n]),
            }
        }
    });

    {
        let stdin = child.stdin.as_mut().expect("piped stdin");
        let mut head = before.join("\n");
        head.push('\n');
        stdin.write_all(head.as_bytes()).expect("first batch");
        stdin.flush().expect("flush first batch");

        // Wait (bounded) for the marker before starting the quiet gap.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
        loop {
            let seen = {
                let buf = stdout_buf.lock().unwrap();
                String::from_utf8_lossy(&buf).contains(marker)
            };
            if seen {
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "the session never printed {marker:?}"
            );
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        std::thread::sleep(gap);

        let mut tail = after.join("\n");
        tail.push('\n');
        stdin.write_all(tail.as_bytes()).expect("second batch");
    }
    drop(child.stdin.take());

    let status = child.wait().expect("waiting for the session");
    reader.join().expect("stdout reader");
    let mut stderr = String::new();
    child
        .stderr
        .take()
        .expect("piped stderr")
        .read_to_string(&mut stderr)
        .expect("reading stderr");
    let stdout = String::from_utf8_lossy(&stdout_buf.lock().unwrap()).into_owned();
    Session {
        stdout,
        stderr,
        code: status.code().unwrap_or(-1),
    }
}

/// A backoff service completes its restart cycles while the foreground sits quietly
/// blocked on `read-line` — service progress during the gap comes entirely from the
/// drive loop's parked edge (deadline-bounded parks; the wake-set fix keeps this path
/// honest). The service traps instantly, restarts twice with a 40ms delay, then gives
/// up: by the end of a 600ms quiet gap the whole lifecycle must have finished.
#[test]
fn restart_cycles_complete_while_the_foreground_is_quietly_blocked() {
    let store = temp_store("park-path");
    let session = shell_session_with_gap(
        &store,
        &["--svc"],
        &[
            "detach crasher = outcomes --mode trap --detail park-path restart \
              restart.backoff --max-restarts 2 --base-delay-ms 40",
        ],
        "detached: crasher",
        std::time::Duration::from_millis(600),
        &["svc list", "exit"],
    );
    assert_eq!(session.code, 0);
    let out = &session.stdout;
    let crasher_line = out
        .lines()
        .find(|line| line.trim_start().starts_with("crasher"))
        .unwrap_or_else(|| panic!("crasher appears in svc list:\n{out}"));
    assert!(
        crasher_line.contains("finished"),
        "the backoff lifecycle (trap, 2 delayed restarts, give-up) completed during the \
         quiet gap:\n{out}"
    );
    let restarts: u32 = crasher_line
        .split_whitespace()
        .nth(2)
        .and_then(|column| column.parse().ok())
        .unwrap_or_else(|| panic!("the crasher line has a restart count: {crasher_line}"));
    assert_eq!(
        restarts, 2,
        "both delayed restarts happened while the foreground was parked:\n{out}"
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
