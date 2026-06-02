//! The eager-guest-calls-guest suspension, reproduced and resolved at the canonical-ABI
//! level (docs/spikes/eager-guest-forwarding.md, plan/09 D31, study 09).
//!
//! Cast: a single-poll consumer (the eager middleware-as-caller), a forwarding relay
//! whose body performs a nested import call (the middleware-as-callee), and a trivial
//! guest `eo9:time` leaf. Composition: `leaf $ relay $ consumer`, no host providers.
//!
//! The consumer's outcome is `code * 1000 + value`: `code` is the canonical-ABI status
//! its single poll observed (1 = STARTED, 2 = RETURNED), `value` is the relay's report
//! (its own observed status, or 7 for a sync-lowered import) when the call completed.
//!
//! What these pin, together: an eager caller's single poll completes if and only if the
//! callee's activation never yields to the event loop, and a callee yields exactly when
//! it makes a *queued* call — an async-lowered call (any callee), or a sync-lowered call
//! to an async-lifted guest. Sync-lowered calls to sync-lifted guests are direct fused
//! calls and host calls complete inline, so a chain whose imports are all sync-lowered
//! and whose guests are all sync-lifted (or host) never yields — at any depth.

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

/// The wall, pinned: with today's shapes everywhere (async-lifted relay whose body
/// async-lowers its own import), the eager caller's one poll observes STARTED — even
/// though every body in the chain completes promptly — because the relay's queued
/// nested call makes it yield after its STARTED event was already posted.
#[test]
fn an_eager_caller_observes_started_when_the_callee_nests_a_queued_call() {
    let value = run_chain(
        time_leaf_async(),
        time_relay(RelayImport::EagerPoll, RelayExport::AsyncLift),
        eager_consumer(ConsumerCall::EagerPoll),
    );
    assert_eq!(value, "1000", "expected the wall (STARTED), got {value}");
}

/// The control: the same chain, but the consumer blocks (sync-lowered call) — completes;
/// and the relay's own single poll of the *trivial* leaf observed RETURNED, pinning that
/// a callee whose activation never yields completes eagerly even when async-lifted
/// (which is why `net.l2.deny $ net.l4.over-l2` and `disk.mem $ fs.eofs` work today).
#[test]
fn a_blocking_caller_completes_and_a_trivial_callee_is_eager() {
    let value = run_chain(
        time_leaf_async(),
        time_relay(RelayImport::EagerPoll, RelayExport::AsyncLift),
        eager_consumer(ConsumerCall::SyncLower),
    );
    assert_eq!(value, "7002");
}

/// Sync-lifting the callee's export alone does NOT lift the wall: its body still
/// async-lowers its own import, which is a queued call, so it still yields and the
/// eager caller still sees STARTED. (Pinned so the conversion is never argued as
/// "just change the lifts".)
#[test]
fn a_sync_lifted_export_alone_does_not_unblock_the_eager_caller() {
    let value = run_chain(
        time_leaf_async(),
        time_relay(RelayImport::EagerPoll, RelayExport::SyncLift),
        eager_consumer(ConsumerCall::EagerPoll),
    );
    assert_eq!(value, "1000");
}

/// An async-lowered call yields even against a sync-lifted callee: the queue is taken
/// for every async-lower, so converting the *consumers* of a provider is not optional.
#[test]
fn an_async_lower_yields_even_against_a_sync_lifted_callee() {
    let value = run_chain(
        time_leaf_sync(),
        time_relay(RelayImport::EagerPoll, RelayExport::SyncLift),
        eager_consumer(ConsumerCall::EagerPoll),
    );
    assert_eq!(value, "1000");
}

/// A sync-lowered call to an *async-lifted* guest is still a queued call: the relay
/// blocks until the leaf completes, which is a yield, and the eager caller above sees
/// STARTED. Every guest below an eager chain must itself be sync-lifted (or host).
#[test]
fn a_sync_lowered_call_to_an_async_lifted_guest_still_yields() {
    let value = run_chain(
        time_leaf_async(),
        time_relay(RelayImport::SyncLower, RelayExport::SyncLift),
        eager_consumer(ConsumerCall::EagerPoll),
    );
    assert_eq!(value, "1000");
}

/// THE FIX, full form: sync-lowered imports over sync-lifted guests, all the way down.
/// No call in the chain ever queues, no activation ever yields, and the eager caller's
/// single poll observes RETURNED — at this depth and (by induction on the same rule)
/// any other.
#[test]
fn an_all_sync_chain_completes_for_an_eager_caller() {
    let value = run_chain(
        time_leaf_sync(),
        time_relay(RelayImport::SyncLower, RelayExport::SyncLift),
        eager_consumer(ConsumerCall::EagerPoll),
    );
    assert_eq!(value, "2007");
}

/// The conversion shape for the real middlewares: imports sync-lowered, export lift
/// UNCHANGED (async). The body never yields — sync-lowered call to a sync-lifted leaf
/// is a direct fused call — so the eager caller completes; the async export lift is
/// irrelevant to callers because Returned overwrites Started within the activation.
/// This is the least-invasive conversion: only the import bindings change.
#[test]
fn sync_lowered_imports_alone_make_an_async_lifted_callee_eager() {
    let value = run_chain(
        time_leaf_sync(),
        time_relay(RelayImport::SyncLower, RelayExport::AsyncLift),
        eager_consumer(ConsumerCall::EagerPoll),
    );
    assert_eq!(value, "2007");
}
