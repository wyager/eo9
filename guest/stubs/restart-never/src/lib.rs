//! `restart.never` — the give-up restart policy.
//!
//! Targets the `eo9:svc/restart-never` stub world: a pure policy component ("policies
//! are programs" — SPEC, Eo9 API design) exporting `eo9:svc/restart-policy` that gives
//! up after the first completed run, whatever its outcome. A service detached under it
//! runs exactly once; the finished record stays inspectable until cleared.
//!
//!   detach ticker = ticker --count 100 restart restart.never
//!
//! The component imports nothing — `describe` shows an empty capability surface — so it
//! provably cannot do anything but return its constant answer.

#![no_std]

extern crate alloc;

// Link the guest SDK for its global allocator and panic handler (the stub itself has no
// state and no imports).
use eo9_guest as _;

wit_bindgen::generate!({
    world: "restart-never",
    path: "../../../wit/svc",
    generate_all,
});

use exports::eo9::svc::restart_policy::{self, FailureHistory, RestartAction};

/// The `restart.never` policy.
struct Stub;

impl restart_policy::Guest for Stub {
    fn decide(_history: FailureHistory) -> RestartAction {
        RestartAction::GiveUp
    }
}

export!(Stub);
