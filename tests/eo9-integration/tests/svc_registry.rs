//! Service-registry suite (executor v1, docs/design/executor-model.md): the host-side
//! `eo9:svc` registry observed through real guest components — detach, capability
//! soundness, policy validation, restart policies as programs, stop/clear, and log
//! capture. The eosh-builtin and CLI layers above this are exercised by the CLI
//! transcripts; this suite drives the registry directly so failures localize.

use std::time::{Duration, Instant};

use eo9_component::{compose, configure};
use eo9_integration::guest::{load_example, load_stub};
use eo9_runtime::svc::{DetachError, LogPolicy, ServiceState};
use eo9_runtime::{EngineOptions, NamedArg, ServiceRegistry, SharedRegistry, new_engine};

/// A registry on the pinned engine.
fn registry() -> SharedRegistry {
    let engine = new_engine(&EngineOptions::default()).expect("pinned engine config is valid");
    ServiceRegistry::new(&engine)
}

/// Pump the registry until nothing is runnable or `deadline` passes. Returns whether the
/// named service reached the `Finished` state.
fn pump_until_finished(registry: &SharedRegistry, name: &str, deadline: Duration) -> bool {
    let end = Instant::now() + deadline;
    loop {
        let mut reg = registry.lock().unwrap();
        reg.pump(100_000);
        if let Some(info) = reg.status(name)
            && info.state == ServiceState::Finished
        {
            return true;
        }
        drop(reg);
        if Instant::now() >= end {
            return false;
        }
        // Real time passes between pumps so waiting-restart delays can elapse.
        std::thread::sleep(Duration::from_millis(1));
    }
}

/// `cruncher` (imports only the rt rider — already closed) with small inputs: the
/// canonical quickly-finishing detachable program.
fn quick_cruncher_args() -> Vec<NamedArg> {
    vec![NamedArg::new("seed", "7"), NamedArg::new("rounds", "50")]
}

// -------------------------------------------------------------------------------------
// The basic story: detach, run, inspect, finish
// -------------------------------------------------------------------------------------

/// A composition closed over text (time.frozen $ hello) detaches, runs to completion
/// under the registry, captures its output in the log, and stays inspectable.
#[test]
fn a_detached_service_runs_writes_logs_and_finishes() {
    let registry = registry();

    // hello requires text + time; sealing time with the frozen stub leaves only text,
    // which the registry's log capture satisfies.
    let child = compose(&load_stub("time.frozen"), &load_example("hello"))
        .expect("time.frozen $ hello composes");
    let policy = load_stub("restart.never");

    let name = registry
        .lock()
        .unwrap()
        .detach(
            child,
            policy,
            "greeter",
            vec![NamedArg::new("name", "\"services\"")],
            LogPolicy::Capture,
        )
        .expect("detach should succeed");
    assert_eq!(name, "greeter");

    // The service shows up immediately, with its wiring recorded.
    {
        let reg = registry.lock().unwrap();
        let list = reg.list();
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].name, "greeter");
        assert!(
            list[0].wiring.contains("hello") || !list[0].wiring.is_empty(),
            "the wiring tree is recorded: {}",
            list[0].wiring
        );
    }

    assert!(
        pump_until_finished(&registry, "greeter", Duration::from_secs(30)),
        "the service should run to completion under the registry's pump"
    );

    let reg = registry.lock().unwrap();
    let info = reg.status("greeter").expect("the record is kept");
    assert_eq!(info.state, ServiceState::Finished);
    assert_eq!(info.restarts, 0, "restart.never never restarts");
    let outcome = info.outcome.expect("a finished service has an outcome");
    assert!(
        outcome.starts_with("success"),
        "hello succeeds; outcome was: {outcome}"
    );

    // The log captured hello's greeting.
    let log = reg
        .log("greeter", 0, u32::MAX)
        .expect("captured logs are readable");
    let text = String::from_utf8_lossy(&log);
    assert!(
        text.contains("Hello, services"),
        "the log holds the program's output; log was: {text:?}"
    );
}

// -------------------------------------------------------------------------------------
// Capability soundness
// -------------------------------------------------------------------------------------

/// The capability-soundness rule: a composition that still requires capabilities beyond
/// text (here: eo9:fs) is refused with the typed not-closed error naming them — the
/// registry never lends its own authority.
#[test]
fn a_composition_requiring_more_than_text_is_refused() {
    let registry = registry();

    let child = load_example("readwrite"); // requires eo9:fs
    let policy = load_stub("restart.never");

    let err = registry
        .lock()
        .unwrap()
        .detach(child, policy, "writer", Vec::new(), LogPolicy::Capture)
        .expect_err("a not-closed composition must be refused");

    match err {
        DetachError::NotClosed(needs) => {
            assert!(
                needs.iter().any(|need| need.starts_with("eo9:fs/")),
                "the refusal names the missing capability; named: {needs:?}"
            );
        }
        other => panic!("expected NotClosed, got: {other:?}"),
    }

    // Nothing was registered.
    assert!(registry.lock().unwrap().list().is_empty());
}

/// Optional imports do not block a detach: hello's composition needs text required and
/// nothing else once time is sealed; text itself is what the registry supplies.
#[test]
fn text_and_rt_riders_are_the_allowed_residuals() {
    let registry = registry();
    let child = compose(&load_stub("time.frozen"), &load_example("hello"))
        .expect("time.frozen $ hello composes");
    let policy = load_stub("restart.never");
    registry
        .lock()
        .unwrap()
        .detach(child, policy, "ok", Vec::new(), LogPolicy::Discard)
        .expect("text + rt riders are exactly what the registry supplies");
}

// -------------------------------------------------------------------------------------
// Restart policies are programs: validation
// -------------------------------------------------------------------------------------

/// A binary is not a policy.
#[test]
fn a_binary_is_not_a_valid_restart_policy() {
    let registry = registry();
    let child = load_example("cruncher");
    let not_a_policy = load_example("hello"); // a binary

    let err = registry
        .lock()
        .unwrap()
        .detach(
            child,
            not_a_policy,
            "svc",
            quick_cruncher_args(),
            LogPolicy::Capture,
        )
        .expect_err("a binary cannot be a restart policy");
    assert!(
        matches!(err, DetachError::InvalidPolicy(_)),
        "expected InvalidPolicy, got: {err:?}"
    );
}

/// A provider that does not export eo9:svc/restart-policy is not a policy either.
#[test]
fn a_non_policy_provider_is_not_a_valid_restart_policy() {
    let registry = registry();
    let child = load_example("cruncher");
    let wrong_provider = load_stub("time.frozen"); // a provider, but of eo9:time

    let err = registry
        .lock()
        .unwrap()
        .detach(
            child,
            wrong_provider,
            "svc",
            quick_cruncher_args(),
            LogPolicy::Capture,
        )
        .expect_err("a non-restart-policy provider must be refused");
    match err {
        DetachError::InvalidPolicy(reason) => {
            assert!(
                reason.contains("restart-policy"),
                "the refusal explains what a policy must export: {reason}"
            );
        }
        other => panic!("expected InvalidPolicy, got: {other:?}"),
    }
}

// -------------------------------------------------------------------------------------
// Restart policies in action
// -------------------------------------------------------------------------------------

/// restart.always brings a finished service back, indefinitely, until stopped.
#[test]
fn restart_always_restarts_until_stopped() {
    let registry = registry();
    let child = load_example("cruncher");
    let policy = load_stub("restart.always");

    registry
        .lock()
        .unwrap()
        .detach(
            child,
            policy,
            "comeback",
            quick_cruncher_args(),
            LogPolicy::Discard,
        )
        .expect("detach should succeed");

    // Pump until at least two restarts have happened (the run finishes, the policy says
    // restart, the run finishes again, …).
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        registry.lock().unwrap().pump(100_000);
        let restarts = registry
            .lock()
            .unwrap()
            .status("comeback")
            .expect("the service exists")
            .restarts;
        if restarts >= 2 {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "restart.always should have restarted the service at least twice by now"
        );
    }

    // Stop ends it for good: a killed run never consults the policy again.
    let outcome = registry
        .lock()
        .unwrap()
        .stop("comeback")
        .expect("stop returns the final outcome");
    assert!(
        outcome.contains("killed") || outcome.starts_with("success"),
        "stop reports how the last run ended: {outcome}"
    );
    // Further pumping never brings it back.
    for _ in 0..50 {
        registry.lock().unwrap().pump(100_000);
    }
    let info = registry
        .lock()
        .unwrap()
        .status("comeback")
        .expect("the record is kept until cleared");
    assert_eq!(info.state, ServiceState::Finished);
}

/// restart.never runs the service exactly once.
#[test]
fn restart_never_runs_exactly_once() {
    let registry = registry();
    let child = load_example("cruncher");
    let policy = load_stub("restart.never");

    registry
        .lock()
        .unwrap()
        .detach(
            child,
            policy,
            "oneshot",
            quick_cruncher_args(),
            LogPolicy::Discard,
        )
        .expect("detach should succeed");

    assert!(
        pump_until_finished(&registry, "oneshot", Duration::from_secs(30)),
        "the run should finish"
    );
    // Keep pumping: it stays finished, zero restarts.
    for _ in 0..50 {
        registry.lock().unwrap().pump(100_000);
    }
    let info = registry.lock().unwrap().status("oneshot").unwrap();
    assert_eq!(info.state, ServiceState::Finished);
    assert_eq!(info.restarts, 0);
}

/// restart.backoff (a *configured* policy component — the bind entrypoint runs inside
/// the registry's policy invocation) restarts with delays and gives up at its budget.
#[test]
fn configured_backoff_policy_restarts_then_gives_up() {
    let registry = registry();
    let child = load_example("cruncher");
    // max-restarts 2, base delay 10ms: two delayed restarts, then give up.
    let policy = configure(
        &load_stub("restart.backoff"),
        &[("max-restarts", "2"), ("base-delay-ms", "10")],
    )
    .expect("restart.backoff configures");

    registry
        .lock()
        .unwrap()
        .detach(
            child,
            policy,
            "bounded",
            quick_cruncher_args(),
            LogPolicy::Discard,
        )
        .expect("detach should succeed");

    // Pump (with real time passing) until the budget is spent and the service finishes
    // for good: 3 total runs (initial + 2 restarts).
    let deadline = Duration::from_secs(60);
    assert!(
        pump_until_finished(&registry, "bounded", deadline),
        "the backoff budget should be exhausted and the service finished"
    );
    let info = registry.lock().unwrap().status("bounded").unwrap();
    assert_eq!(
        info.restarts, 2,
        "exactly max-restarts restarts were performed"
    );
}

/// An *unconfigured* backoff policy gives up immediately (the deny-all posture).
#[test]
fn unconfigured_backoff_policy_never_restarts() {
    let registry = registry();
    let child = load_example("cruncher");
    let policy = load_stub("restart.backoff"); // no configure

    registry
        .lock()
        .unwrap()
        .detach(
            child,
            policy,
            "unconfigured",
            quick_cruncher_args(),
            LogPolicy::Discard,
        )
        .expect("detach should succeed");

    assert!(
        pump_until_finished(&registry, "unconfigured", Duration::from_secs(30)),
        "the run should finish"
    );
    for _ in 0..50 {
        registry.lock().unwrap().pump(100_000);
    }
    let info = registry.lock().unwrap().status("unconfigured").unwrap();
    assert_eq!(info.state, ServiceState::Finished);
    assert_eq!(info.restarts, 0, "an unconfigured backoff never restarts");
}

// -------------------------------------------------------------------------------------
// stop / clear / names
// -------------------------------------------------------------------------------------

/// Stopping a still-running service kills it; clear removes the record; clearing a
/// running service is refused.
#[test]
fn stop_kills_a_running_service_and_clear_removes_it() {
    let registry = registry();
    // A long run: large rounds keep the cruncher busy across many pump slices.
    let child = load_example("cruncher");
    let policy = load_stub("restart.never");

    registry
        .lock()
        .unwrap()
        .detach(
            child,
            policy,
            "longhaul",
            vec![
                NamedArg::new("seed", "7"),
                NamedArg::new("rounds", "500000000"),
            ],
            LogPolicy::Discard,
        )
        .expect("detach should succeed");

    // Give it a few slices; it must still be running (not finished).
    for _ in 0..5 {
        registry.lock().unwrap().pump(100_000);
    }
    {
        let reg = registry.lock().unwrap();
        let info = reg.status("longhaul").unwrap();
        assert_ne!(
            info.state,
            ServiceState::Finished,
            "a 500M-round cruncher cannot have finished in five small slices"
        );
        // A running service cannot be cleared.
        drop(reg);
        assert!(!registry.lock().unwrap().clear("longhaul"));
    }

    // Stop it: the outcome is the kill.
    let outcome = registry.lock().unwrap().stop("longhaul").unwrap();
    assert!(
        outcome.contains("killed"),
        "stopping a running service kills it: {outcome}"
    );

    // Now clear removes the record.
    assert!(registry.lock().unwrap().clear("longhaul"));
    assert!(registry.lock().unwrap().list().is_empty());
    // Unknown names: stop and clear answer None/false.
    assert!(registry.lock().unwrap().stop("longhaul").is_none());
    assert!(!registry.lock().unwrap().clear("longhaul"));
}

/// Service names follow the same rules as store names; duplicates are refused.
#[test]
fn invalid_and_duplicate_names_are_refused() {
    let registry = registry();
    let policy = || load_stub("restart.never");
    let child = || load_example("cruncher");

    // Invalid names.
    for bad in ["", ".hidden", "trailing.", "spa ce", "sla/sh"] {
        let err = registry
            .lock()
            .unwrap()
            .detach(
                child(),
                policy(),
                bad,
                quick_cruncher_args(),
                LogPolicy::Discard,
            )
            .expect_err("invalid names are refused");
        assert!(
            matches!(err, DetachError::InvalidName(_)),
            "expected InvalidName for {bad:?}, got: {err:?}"
        );
    }

    // A valid detach, then the same name again.
    registry
        .lock()
        .unwrap()
        .detach(
            child(),
            policy(),
            "taken",
            quick_cruncher_args(),
            LogPolicy::Discard,
        )
        .expect("first detach succeeds");
    let err = registry
        .lock()
        .unwrap()
        .detach(
            child(),
            policy(),
            "taken",
            quick_cruncher_args(),
            LogPolicy::Discard,
        )
        .expect_err("duplicate names are refused");
    assert!(
        matches!(err, DetachError::NameTaken(_)),
        "expected NameTaken, got: {err:?}"
    );
}

/// A provider cannot be detached (services are programs).
#[test]
fn a_provider_cannot_be_detached() {
    let registry = registry();
    let err = registry
        .lock()
        .unwrap()
        .detach(
            load_stub("time.frozen"),
            load_stub("restart.never"),
            "notaprogram",
            Vec::new(),
            LogPolicy::Discard,
        )
        .expect_err("providers are composed, never run");
    assert!(
        matches!(err, DetachError::NotABinary),
        "expected NotABinary, got: {err:?}"
    );
}

/// Discarded logs read as absent (None), not as empty.
#[test]
fn discarded_logs_are_absent_not_empty() {
    let registry = registry();
    let child = compose(&load_stub("time.frozen"), &load_example("hello"))
        .expect("time.frozen $ hello composes");

    registry
        .lock()
        .unwrap()
        .detach(
            child,
            load_stub("restart.never"),
            "quiet",
            Vec::new(),
            LogPolicy::Discard,
        )
        .expect("detach should succeed");
    assert!(
        pump_until_finished(&registry, "quiet", Duration::from_secs(30)),
        "the run should finish"
    );
    assert!(
        registry.lock().unwrap().log("quiet", 0, u32::MAX).is_none(),
        "a discard-policy service has no log to read"
    );
}

/// The directive's crash-recovery case: a service that TRAPS (not merely fails) is
/// brought back by restart.always — and its failure history records the trapped class.
#[test]
fn restart_always_brings_a_crashing_service_back() {
    let registry = registry();
    // `outcomes --mode trap` panics → the run ends abnormal(trapped). It imports text +
    // rt only, so it is detachable as-is.
    let child = load_example("outcomes");
    let policy = load_stub("restart.always");

    registry
        .lock()
        .unwrap()
        .detach(
            child,
            policy,
            "crasher",
            vec![
                NamedArg::new("mode", "\"trap\""),
                NamedArg::new("detail", "\"requested crash\""),
            ],
            LogPolicy::Capture,
        )
        .expect("detach should succeed");

    // Pump until the crash has happened and the policy has restarted it at least twice:
    // crash → restart → crash → restart …
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        registry.lock().unwrap().pump(100_000);
        let info = registry
            .lock()
            .unwrap()
            .status("crasher")
            .expect("the service exists");
        if info.restarts >= 2 {
            // The recorded outcome of completed runs is the trap.
            let outcome = info.outcome.expect("a completed run has an outcome");
            assert!(
                outcome.contains("trapped"),
                "the recorded outcome is the trap: {outcome}"
            );
            break;
        }
        assert!(
            Instant::now() < deadline,
            "restart.always should have restarted the crashing service at least twice"
        );
        std::thread::sleep(Duration::from_millis(1));
    }

    // And restart.never on the same crasher: one trap, no comeback.
    let child = load_example("outcomes");
    let policy = load_stub("restart.never");
    registry
        .lock()
        .unwrap()
        .detach(
            child,
            policy,
            "oneshot-crasher",
            vec![
                NamedArg::new("mode", "\"trap\""),
                NamedArg::new("detail", "\"requested crash\""),
            ],
            LogPolicy::Discard,
        )
        .expect("detach should succeed");
    assert!(
        pump_until_finished(&registry, "oneshot-crasher", Duration::from_secs(30)),
        "the crash finishes the service for good under restart.never"
    );
    let info = registry.lock().unwrap().status("oneshot-crasher").unwrap();
    assert_eq!(info.restarts, 0, "restart.never never brings a crash back");
    assert!(
        info.outcome.unwrap().contains("trapped"),
        "the outcome records the trap"
    );

    // Cleanup the still-crash-looping service.
    registry.lock().unwrap().stop("crasher");
}
