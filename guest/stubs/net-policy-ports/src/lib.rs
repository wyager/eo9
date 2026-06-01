//! `net.policy-ports` — the port allow-list connection policy.
//!
//! Targets the `eo9:net/ports-policy` stub world: a pure policy component ("policies
//! are programs" — SPEC, Eo9 API design) exporting `eo9:net/connection-policy`,
//! deciding by port: endpoints whose port is on the configured allow-list are admitted,
//! for any address and any operation kind (connect, listen, bind-udp, send-to).
//!
//!   net.policy-ports --allow "[80, 443]" $ net.l4.filtered $ program
//!
//! * Unconfigured, the policy admits **nothing** (deny-all), so plain composition never
//!   traps and never silently widens (plan/09 D14).
//! * The component imports nothing — its only `use` of `eo9:net` is for the
//!   `socket-address` *type*, which carries no authority — so `describe` shows an empty
//!   capability surface: the policy provably cannot do anything but compute its answer.

#![no_std]

extern crate alloc;

use alloc::string::String;
use alloc::vec::Vec;

use eo9_guest::provider::ProviderState;

wit_bindgen::generate!({
    world: "ports-policy",
    path: "../../../wit/net",
    generate_all,
});

use exports::eo9::net::connection_policy::{self, EndpointKind, SocketAddress};
use exports::eo9::net::ports_config;

/// The configured allow-list of ports. Unconfigured means "admit nothing".
static ALLOW: ProviderState<Vec<u16>> = ProviderState::new();

/// The `net.policy-ports` policy.
struct Stub;

impl ports_config::Guest for Stub {
    fn configure(allow: Vec<u16>) -> Result<(), String> {
        ALLOW.set(allow);
        Ok(())
    }
}

impl connection_policy::Guest for Stub {
    fn admit(_kind: EndpointKind, endpoint: SocketAddress) -> bool {
        if !ALLOW.is_set() {
            return false;
        }
        ALLOW.with(|ports| ports.contains(&endpoint.port))
    }
}

export!(Stub);
