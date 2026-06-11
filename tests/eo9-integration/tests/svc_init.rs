//! `eo9 init` tests (executor v1, milestone 5): the service-boot program — config
//! parsing, service startup, the console loop, capability soundness through init, and
//! the scripted-session console-restart default.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use eo9_integration::guest;

fn eo9_binary() -> PathBuf {
    // The shared always-build + bundle-freshness helper: a stale eo9 binary (or a
    // stale committed bundle) silently tests OLD init bytes — the trap this lane hit.
    guest::fresh_eo9_binary()
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

/// A chain whose tail is a BINARY keeps the original routing byte-for-byte: the tail's
/// flags bind as `main` arguments (and a non-tail provider's flags still configure it).
/// This is the regression pin for the ordinary-service path around the provider-tail
/// routing (the station-net flagged-tail fix must not disturb it).
#[test]
fn chain_binary_tail_flags_still_bind_as_main_arguments() {
    guest::ensure_components(&["eo9-stub-time-frozen", "eo9-example-hello"]);
    let dir = temp_dir("binary-tail-chain");
    let config = "\
greeter = time.frozen --now-seconds 1700000000 --monotonic-ns 0 $ hello --name tailpin restart restart.never
";
    let run = init_session(&dir, Some(config), &["svc log greeter", "exit"]);
    assert_eq!(run.code, 0, "stderr: {}", run.stderr);
    assert!(
        run.stdout
            .contains("init: started `greeter` (time.frozen $ hello under restart.never)"),
        "the chain with a binary tail started:\n{}",
        run.stdout
    );
    // The tail's flag reached `main` (hello greeted the configured name), proving the
    // tail's flags were bound as arguments, not routed to a configure surface.
    assert!(
        run.stdout.contains("tailpin"),
        "the binary tail's flag bound as a main argument:\n{}",
        run.stdout
    );
}

/// A chain that ENDS on a provider routes the tail's flags to that provider's
/// `configure`, never to `main` — the share-owning factory shape (the station-net
/// silicon bug: `… $ net.l4.over-l2 --address dhcp share …` was refused because the
/// tail's flags were bound as `main` arguments, which a factory service does not have).
///
/// The usermode registry has no `share` clause, so the detach itself still refuses the
/// provider — but the refusal must be the provider-kind one, reached AFTER the flags
/// were consumed by `configure`. An argument-shaped refusal here would mean the flags
/// were mis-bound as main arguments again.
#[test]
fn provider_tail_flags_route_to_configure() {
    guest::ensure_components(&["eo9-stub-time-frozen"]);
    let dir = temp_dir("provider-tail");
    let config = "\
clock = time.frozen --now-seconds 42 --monotonic-ns 7 restart restart.never
";
    let run = init_session(&dir, Some(config), &["exit"]);
    assert_eq!(run.code, 0, "stderr: {}", run.stderr);
    let all = format!("{}{}", run.stdout, run.stderr);
    // The flags bound cleanly against the provider's configure signature; what remains
    // is the usermode registry's kind refusal (no shares there), not anything about
    // arguments or flags.
    assert!(
        all.contains("could not start `clock`") && all.contains("provider, not a runnable program"),
        "the refusal is the provider-kind one (flags consumed by configure):\n{all}"
    );
    assert!(
        !all.contains("takes no arguments"),
        "no argument-shaped refusal for a provider tail's flags:\n{all}"
    );
}

/// The routing direction itself, pinned: an unknown flag on a provider tail fails at
/// the tail's `configure` (typed, naming the segment) — NOT at the registry as a
/// misrouted `main` argument. Before the routing fix this config produced the
/// provider-kind detach refusal instead, because the flag rode through as a main
/// argument.
#[test]
fn provider_tail_unknown_flag_is_a_configure_error() {
    guest::ensure_components(&["eo9-stub-time-frozen"]);
    let dir = temp_dir("provider-tail-bad-flag");
    let config = "\
clock = time.frozen --bogus 1 restart restart.never
";
    let run = init_session(&dir, Some(config), &["exit"]);
    assert_eq!(run.code, 0, "stderr: {}", run.stderr);
    let all = format!("{}{}", run.stdout, run.stderr);
    assert!(
        all.contains("could not start `clock`") && all.contains("configuring `time.frozen` failed"),
        "an unknown tail flag fails at the provider's configure:\n{all}"
    );
}

/// Ungiven `option<…>` configure parameters fill with `none` (eosh's evaluator rule,
/// eosh-core wave.rs) — the algebra's `configure` requires every parameter of the
/// signature, so without the fill the board's own `… $ net.l4.over-l2 --address dhcp
/// share …` line dies with `missing argument `prefix-length`` (the second leg of the
/// station-net silicon bug). Only `address` of
/// `configure(address, prefix-length: option<u8>, gateway: option<string>)` is given
/// here; the two options must fill as `none` and the configure must bind.
#[test]
fn provider_tail_ungiven_option_params_fill_with_none() {
    guest::ensure_components(&["eo9-stub-net-l4-over-l2"]);
    let dir = temp_dir("provider-tail-option-fill");
    let config = "\
lan = net.l4.over-l2 --address dhcp restart restart.never
";
    let run = init_session(&dir, Some(config), &["exit"]);
    assert_eq!(run.code, 0, "stderr: {}", run.stderr);
    let all = format!("{}{}", run.stdout, run.stderr);
    // The configure bound (address given, the two options filled with `none`); the
    // refusal that remains is the usermode registry's (it has no `share` clause),
    // never the algebra's missing-argument one.
    assert!(
        all.contains("could not start `lan`"),
        "the usermode registry still refuses the share-less provider service:\n{all}"
    );
    assert!(
        !all.contains("configuring `net.l4.over-l2` failed"),
        "no configure-time refusal for ungiven option parameters:\n{all}"
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
