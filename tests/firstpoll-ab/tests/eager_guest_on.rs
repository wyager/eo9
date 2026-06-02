//! Arm B only: the seven-row eager-guest matrix under `first-poll-inline`.
//!
//! The intended positive signal of the prototype (docs/spikes/first-poll-inline.md):
//! the three rows whose pinned STARTED came from a *queued call to an async-lifted
//! (callback-ABI) callee* flip to RETURNED, because the callee's initial activation now
//! runs inline on the caller's stack and completes without yielding to the event loop.
//! The row whose STARTED comes from an async-lowered call to a *sync-lifted* callee
//! (no callback, never inlined) is pinned unchanged, as are the three rows that already
//! completed.
//!
//! Outcome encoding (see eager_guest.rs): `code * 1000 + value` — `code` is the status
//! the consumer's call observed (1 = STARTED, 2 = RETURNED, 7 = sync-lowered import),
//! `value` is the relay's own report when the call completed.
#![cfg(feature = "first-poll-inline")]

use eo9_component::{Component, compose};
use eo9_integration::fixtures::{
    ConsumerCall, RelayExport, RelayImport, eager_consumer, time_leaf_async, time_leaf_sync,
    time_relay,
};
use eo9_integration::run::{run_component, success_value};
use eo9_runtime::Providers;

fn chain(leaf: Component, relay: Component, consumer: Component) -> Component {
    let composed = compose(&relay, &consumer).expect("relay $ consumer should compose");
    compose(&leaf, &composed).expect("leaf $ (relay $ consumer) should compose")
}

fn run_chain(leaf: Component, relay: Component, consumer: Component) -> String {
    let outcome = run_component(&chain(leaf, relay, consumer), &[], Providers::none());
    success_value(&outcome).to_string()
}

/// THE WALL FALLS: the async-lifted relay runs inline under the consumer's
/// async-lowered call, its own async-lowered call to the async-lifted leaf runs inline
/// in turn, nothing ever yields, and the eager caller's single poll observes RETURNED
/// (the relay's own poll observed RETURNED too).
#[test]
fn the_wall_row_completes_inline() {
    let value = run_chain(
        time_leaf_async(),
        time_relay(RelayImport::EagerPoll, RelayExport::AsyncLift),
        eager_consumer(ConsumerCall::EagerPoll),
    );
    assert_eq!(value, "2002", "the wall must fall under first-poll-inline");
}

/// Unchanged: the blocking caller still completes, and the relay's report is the same.
/// (The consumer's sync-lowered call now completes without ever suspending — the relay
/// runs inline — but the observable outcome is identical.)
#[test]
fn a_blocking_caller_still_completes() {
    let value = run_chain(
        time_leaf_async(),
        time_relay(RelayImport::EagerPoll, RelayExport::AsyncLift),
        eager_consumer(ConsumerCall::SyncLower),
    );
    assert_eq!(value, "7002");
}

/// Flips: the relay is sync-lifted so the consumer's call still queues, but the relay's
/// own async-lowered call to the async-lifted leaf now runs inline, so the relay's
/// activation never yields and completes inside its work item — Returned overwrites
/// Started before the consumer resumes.
#[test]
fn a_sync_lifted_relay_over_an_inlinable_leaf_completes() {
    let value = run_chain(
        time_leaf_async(),
        time_relay(RelayImport::EagerPoll, RelayExport::SyncLift),
        eager_consumer(ConsumerCall::EagerPoll),
    );
    assert_eq!(value, "2002");
}

/// Pinned unchanged: an async-lowered call to a *sync-lifted* leaf has no callback to
/// inline (the gate requires the callback ABI), so it queues, the relay yields, and the
/// eager caller still observes STARTED. This row is the proof that inlining is scoped
/// to callback-ABI callees and everything else keeps today's path.
#[test]
fn an_async_lower_to_a_sync_lifted_callee_still_queues() {
    let value = run_chain(
        time_leaf_sync(),
        time_relay(RelayImport::EagerPoll, RelayExport::SyncLift),
        eager_consumer(ConsumerCall::EagerPoll),
    );
    assert_eq!(value, "1000");
}

/// Flips: the relay's sync-lowered call to the async-lifted leaf now runs the leaf
/// inline (callback ABI, gate passes) and observes its completion without suspending,
/// so the relay's activation never yields; report 7 = sync-lowered import.
#[test]
fn a_sync_lowered_call_to_an_async_lifted_guest_completes_inline() {
    let value = run_chain(
        time_leaf_async(),
        time_relay(RelayImport::SyncLower, RelayExport::SyncLift),
        eager_consumer(ConsumerCall::EagerPoll),
    );
    assert_eq!(value, "2007");
}

/// Unchanged: the all-sync chain has no callback boundaries anywhere — first-poll-inline
/// is never consulted and the direct fused calls behave exactly as before.
#[test]
fn the_all_sync_chain_is_untouched() {
    let value = run_chain(
        time_leaf_sync(),
        time_relay(RelayImport::SyncLower, RelayExport::SyncLift),
        eager_consumer(ConsumerCall::EagerPoll),
    );
    assert_eq!(value, "2007");
}

/// Unchanged: the minimal-conversion shape (sync-lowered imports, async export lift)
/// already completed; under inlining the consumer's call enters the relay inline rather
/// than observing the Returned-overwrites-Started race, same outcome.
#[test]
fn the_minimal_conversion_shape_is_untouched() {
    let value = run_chain(
        time_leaf_sync(),
        time_relay(RelayImport::SyncLower, RelayExport::AsyncLift),
        eager_consumer(ConsumerCall::EagerPoll),
    );
    assert_eq!(value, "2007");
}
