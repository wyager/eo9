//! `net.l3.deny` — the network-layer capability, present but refusing.
//!
//! Targets the `eo9:net/l3-deny` stub world: exports `eo9:net/l3` where every operation
//! that could grant reachability or visibility (`addresses`, `routes`, `open-raw`)
//! fails with the layer's own `denied` error (see SPEC.md, "The capability algebra").
//!
//! Because no raw socket can ever be opened, the operations on raw sockets
//! (`send-datagram`, `recv-datagram`) are unreachable: their resource type is
//! uninhabited, which the match below makes explicit.

#![no_std]

extern crate alloc;

use alloc::string::String;
use alloc::vec::Vec;

// Linked for the guest runtime profile (allocator + panic handler).
use eo9_guest as _;

wit_bindgen::generate!({
    world: "l3-deny",
    path: "../../../wit/net",
    // Pull in bindings for eo9:io/buffers, which the exported l3 interface uses but
    // the world does not name directly.
    generate_all,
});

use exports::eo9::net::l3::{
    self, Buffer, IpAddress, IpPrefix, L3Error, RecvResult, Route, SendResult,
};
use exports::eo9::net::l3_deny_config;

/// The `net.l3.deny` provider.
struct Stub;

/// The root-handle resource: a token — there is no state behind it.
struct DenyL3;

/// Uninhabited representation for the raw-socket resource: `net.l3.deny` never opens
/// one, so the operations on it can never be reached.
enum NoSocket {}

impl NoSocket {
    /// Statically-checked unreachability: having a borrow of an uninhabited resource is
    /// a contradiction, which the empty match discharges.
    fn unreachable<T>(&self) -> T {
        match *self {}
    }
}

impl l3::GuestL3Impl for DenyL3 {}
impl l3::GuestRawSocket for NoSocket {}

impl l3_deny_config::Guest for Stub {
    fn configure() -> Result<l3::L3Impl, String> {
        Ok(l3::L3Impl::new(DenyL3))
    }
}

impl l3::Guest for Stub {
    type L3Impl = DenyL3;
    type RawSocket = NoSocket;

    fn default() -> l3::L3Impl {
        l3::L3Impl::new(DenyL3)
    }

    async fn addresses(_l3: l3::L3ImplBorrow<'_>) -> Result<Vec<IpPrefix>, L3Error> {
        Err(L3Error::Denied)
    }

    async fn routes(_l3: l3::L3ImplBorrow<'_>) -> Result<Vec<Route>, L3Error> {
        Err(L3Error::Denied)
    }

    async fn open_raw(_l3: l3::L3ImplBorrow<'_>, _protocol: u8) -> Result<l3::RawSocket, L3Error> {
        Err(L3Error::Denied)
    }

    async fn send_datagram(
        s: l3::RawSocketBorrow<'_>,
        _destination: IpAddress,
        _payload: Buffer,
    ) -> (Buffer, Result<SendResult, L3Error>) {
        s.get::<NoSocket>().unreachable()
    }

    async fn recv_datagram(
        s: l3::RawSocketBorrow<'_>,
        _dst: Buffer,
    ) -> (Buffer, Result<(RecvResult, IpAddress), L3Error>) {
        s.get::<NoSocket>().unreachable()
    }
}

export!(Stub);
