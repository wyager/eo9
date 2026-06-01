//! `restart.always` — the always-restart policy.
//!
//! Targets the `eo9:svc/restart-always` stub world: a pure policy component ("policies
//! are programs" — SPEC, Eo9 API design) exporting `eo9:svc/restart-policy` that orders
//! an immediate restart after every completed run, forever. A service detached under it
//! keeps coming back until `svc stop` (or the registry's own lifetime ends).
//!
//!   detach ticker = ticker --count 100 restart restart.always
//!
//! The component imports nothing — `describe` shows an empty capability surface — so it
//! provably cannot do anything but return its constant answer.

#![no_std]

extern crate alloc;

// Link the guest SDK for its global allocator and panic handler (the stub itself has no
// state and no imports).
use eo9_guest as _;

wit_bindgen::generate!({
    world: "restart-always",
    path: "../../../wit/svc",
    generate_all,
});

use exports::eo9::svc::restart_policy::{self, FailureHistory, RestartAction};

/// The `restart.always` policy.
struct Stub;

impl restart_policy::Guest for Stub {
    fn decide(_history: FailureHistory) -> RestartAction {
        RestartAction::Restart
    }
}

export!(Stub);
