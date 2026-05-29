//! `net.l2.deny` — the link-layer network capability, present but refusing.
//!
//! Targets the `eo9:net/l2-deny` stub world: exports `eo9:net/l2` where every operation
//! that could grant reachability (`list-interfaces`, `open-interface`) fails with the
//! layer's own `denied` error (see SPEC.md, "The capability algebra": refusal is
//! meaningful for net, so each layer gets a deny stub in the API's own vocabulary).
//!
//! Because no interface can ever be opened, the operations on opened interfaces
//! (`info`, `send-frame`, `recv-frame`) are unreachable: their resource type is
//! uninhabited, which the match below makes explicit.

#![no_std]

extern crate alloc;

use alloc::string::String;
use alloc::vec::Vec;

// Linked for the guest runtime profile (allocator + panic handler).
use eo9_guest as _;

wit_bindgen::generate!({
    world: "l2-deny",
    path: "../../../wit/net",
    // Pull in bindings for eo9:io/buffers, which the exported l2 interface uses but
    // the world does not name directly.
    generate_all,
});

use exports::eo9::net::l2::{self, Buffer, InterfaceInfo, L2Error, RecvResult, SendResult};
use exports::eo9::net::l2_deny_config;

/// The `net.l2.deny` provider.
struct Stub;

/// The root-handle resource: a token — there is no state behind it.
struct DenyL2;

/// Uninhabited representation for the opened-interface resource: `net.l2.deny` never
/// opens one, so the operations on it can never be reached.
enum NoIface {}

impl NoIface {
    /// Statically-checked unreachability: having a borrow of an uninhabited resource is
    /// a contradiction, which the empty match discharges.
    fn unreachable<T>(&self) -> T {
        match *self {}
    }
}

impl l2::GuestL2Impl for DenyL2 {}
impl l2::GuestL2Interface for NoIface {}

impl l2_deny_config::Guest for Stub {
    fn configure() -> Result<l2::L2Impl, String> {
        Ok(l2::L2Impl::new(DenyL2))
    }
}

impl l2::Guest for Stub {
    type L2Impl = DenyL2;
    type L2Interface = NoIface;

    fn default() -> l2::L2Impl {
        l2::L2Impl::new(DenyL2)
    }

    async fn list_interfaces(_l2: l2::L2ImplBorrow<'_>) -> Result<Vec<InterfaceInfo>, L2Error> {
        Err(L2Error::Denied)
    }

    async fn open_interface(
        _l2: l2::L2ImplBorrow<'_>,
        _name: String,
    ) -> Result<l2::L2Interface, L2Error> {
        Err(L2Error::Denied)
    }

    fn info(iface: l2::L2InterfaceBorrow<'_>) -> InterfaceInfo {
        iface.get::<NoIface>().unreachable()
    }

    async fn send_frame(
        iface: l2::L2InterfaceBorrow<'_>,
        _frame: Buffer,
    ) -> (Buffer, Result<SendResult, L2Error>) {
        iface.get::<NoIface>().unreachable()
    }

    async fn recv_frame(
        iface: l2::L2InterfaceBorrow<'_>,
        _dst: Buffer,
    ) -> (Buffer, Result<RecvResult, L2Error>) {
        iface.get::<NoIface>().unreachable()
    }
}

export!(Stub);
