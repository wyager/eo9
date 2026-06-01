//! `eo9 init` tests (executor v1, milestone 5): the service-boot program — config
//! parsing, service startup, the console loop, capability soundness through init, and
//! the scripted-session console-restart default.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Once;

use eo9_integration::guest;

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
    binary
}

fn temp_dir(test: &str) -> PathBuf {
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!("eo9-svc-init-{test}"));
    if dir.exists() {
        std::fs::remove_dir_all(&dir).expect("failed to clear the test dir");
    }
    std::fs::create_dir_all(&dir).expect("failed to create the test dir");
    dir
}

struct Run {
    stdout: String,
    stderr: String,
    code: i32,
}

/// Run `eo9 init [config]` with `lines` piped into the console's stdin.
fn init_session(dir: &Path, config: Option<&str>, lines: &[&str]) -> Run {
    guest::ensure_components(&["eosh", "init", "eo9-stub-restart-never"]);
    let store = dir.join("store");
    let mut command = Command::new(eo9_binary());
    command.arg("init");
    if let Some(config_text) = config {
        let config_path = dir.join("services.cfg");
        std::fs::write(&config_path, config_text).expect("writing the test config");
        command.arg(&config_path);
    }
    command
        .env("EO9_STORE", &store)
        .current_dir(guest::repo_root())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = command.spawn().expect("failed to spawn eo9 init");
    {
        let stdin = child.stdin.as_mut().expect("piped stdin");
        let mut script = lines.join("\n");
        script.push('\n');
        stdin
            .write_all(script.as_bytes())
            .expect("writing the session script");
    }
    let output = child.wait_with_output().expect("waiting for init");
    Run {
        stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        code: output.status.code().unwrap_or(-1),
    }
}

/// `eo9 init` with no config: just the console, with the svc capability granted to it.
#[test]
fn default_config_runs_a_console_with_the_svc_grant() {
    let dir = temp_dir("default");
    let run = init_session(&dir, None, &["svc list", "exit"]);
    assert_eq!(run.code, 0, "stderr: {}", run.stderr);
    // The console held the services half: `svc list` answered (with emptiness), not
    // with the not-granted refusal.
    assert!(
        run.stdout.contains("no services"),
        "the console can inspect the (empty) registry:\n{}",
        run.stdout
    );
    assert!(
        !run.stdout.contains("does not hold"),
        "no not-granted refusal:\n{}",
        run.stdout
    );
    // Scripted stdin: the console exits once and init follows.
    assert!(
        run.stdout
            .contains("init: console-restart is `never`; init exiting")
            || run
                .stdout
                .contains("init: no services running; init exiting"),
        "init exited after the console:\n{}",
        run.stdout
    );
}

/// A config's services start before the console; the console can see and manage them;
/// teardown stops what is left.
#[test]
fn config_services_start_and_die_with_the_process() {
    let dir = temp_dir("services");
    let config = "\
# test config
worker = cruncher --seed 7 --rounds 900000000 restart restart.always
banner = echo --text from-a-service restart restart.never
";
    let run = init_session(&dir, Some(config), &["svc list", "svc log banner", "exit"]);
    assert_eq!(run.code, 0, "stderr: {}", run.stderr);

    // Both started.
    assert!(
        run.stdout
            .contains("init: started `worker` (cruncher under restart.always)"),
        "worker started:\n{}",
        run.stdout
    );
    assert!(
        run.stdout
            .contains("init: started `banner` (echo under restart.never)"),
        "banner started:\n{}",
        run.stdout
    );

    // The console saw them.
    assert!(
        run.stdout.contains("worker") && run.stdout.contains("running"),
        "the console's svc list shows the worker:\n{}",
        run.stdout
    );

    // The echo service's output landed in its log, readable from the console.
    assert!(
        run.stdout.contains("from-a-service"),
        "svc log shows the service's output:\n{}",
        run.stdout
    );

    // Teardown: the still-running worker was stopped with the lifetime explanation.
    assert!(
        run.stderr.contains("stopped: worker"),
        "the worker was stopped at teardown:\n{}",
        run.stderr
    );
    assert!(
        run.stderr
            .contains("services live only as long as this eo9 process"),
        "the lifetime rule is explained:\n{}",
        run.stderr
    );
}

/// Capability soundness through init: a config entry whose program needs more than the
/// registry supplies is refused (and named), and the rest of the boot continues.
#[test]
fn soundness_an_unclosed_config_entry_is_refused_but_does_not_block_the_boot() {
    let dir = temp_dir("soundness");
    let config = "\
needsfs = readwrite --path /x --contents y restart restart.never
worker = cruncher --seed 1 --rounds 900000000 restart restart.always
";
    let run = init_session(&dir, Some(config), &["svc list", "exit"]);
    assert_eq!(run.code, 0, "stderr: {}", run.stderr);

    // The unclosed entry was refused, naming what it still needs.
    let all = format!("{}{}", run.stdout, run.stderr);
    assert!(
        all.contains("could not start `needsfs`") && all.contains("eo9:fs"),
        "the refusal names the entry and the missing capability:\n{all}"
    );
    // The other service still started: one bad line never blocks the boot.
    assert!(
        all.contains("init: started `worker`"),
        "the good entry still started:\n{all}"
    );
    assert!(
        all.contains("1 of 2 service(s) running"),
        "the summary is honest:\n{all}"
    );
}

/// A malformed config is a clean, line-numbered error before anything starts.
#[test]
fn a_bad_config_is_a_clean_error() {
    let dir = temp_dir("bad-config");
    // Missing the restart clause.
    let config = "worker = cruncher --seed 1 --rounds 5\n";
    let run = init_session(&dir, Some(config), &[]);
    assert_ne!(run.code, 0, "a bad config is an error");
    let all = format!("{}{}", run.stdout, run.stderr);
    assert!(
        all.contains("line 1") && all.contains("restart"),
        "the error names the line and the problem:\n{all}"
    );
}

/// A configured backoff policy in a config line round-trips (policy flags configure the
/// policy component before the detach).
#[test]
fn config_policy_flags_configure_the_policy() {
    let dir = temp_dir("backoff");
    let config = "\
bounded = cruncher --seed 1 --rounds 100 restart restart.backoff --max-restarts 2 --base-delay-ms 10
";
    let run = init_session(&dir, Some(config), &["svc list", "exit"]);
    assert_eq!(run.code, 0, "stderr: {}", run.stderr);
    assert!(
        run.stdout
            .contains("init: started `bounded` (cruncher under restart.backoff)"),
        "the configured-policy entry started:\n{}",
        run.stdout
    );
}

/// init run as an ordinary program *without* the svc grant fails with its own typed
/// refusal — init has no private powers; the capability is everything.
///
/// (Driven at the runtime level: `eo9 run` cannot grant exec, so the only way to give
/// init exec-but-not-svc is to spawn it directly.)
#[test]
fn init_without_the_svc_grant_reports_the_missing_capability() {
    use eo9_integration::run;
    use eo9_runtime::providers::{CaptureText, MemFs};
    use eo9_runtime::{ChildPolicy, ExecProvider, NamedArg, Outcome, Providers};

    let init = guest::load_component("init");
    let image = run::compile_component(&init);
    let engine = image.engine().clone();
    let providers = Providers {
        text: Some(Box::new(CaptureText::new())),
        fs: Some(Box::new(MemFs::new())),
        exec: Some(ExecProvider::new(&engine, ChildPolicy::no_providers())),
        // Deliberately: no svc.
        ..Providers::none()
    };
    let outcome = run::run_image(
        &image,
        &[NamedArg::new("config", "\"console = eosh\"")],
        providers,
    );
    match outcome {
        Outcome::Failure(value) => {
            assert!(
                value.value.contains("no-svc-capability") || value.value.contains("svc capability"),
                "init's failure names the missing capability: {}",
                value.value
            );
        }
        other => panic!("expected init's own typed failure, got {other:?}"),
    }
}
