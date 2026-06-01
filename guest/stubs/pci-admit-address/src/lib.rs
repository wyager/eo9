//! `pci.admit-address` — the address allow-list PCI admit policy.
//!
//! Targets the `eo9:pci/address-admit` stub world: a pure policy component ("policies
//! are programs" — SPEC, Eo9 API design) exporting `eo9:pci/admit-policy`, deciding by
//! device *address*: exactly the configured allow-list of bus addresses is admitted.
//! This recovers the original `pci.filtered --allow …` behavior as a separate, composable
//! policy:
//!
//!   pci.admit-address --allow "[{segment: 0, bus: 0, device: 1, function: 0}]" $ pci.filtered $ driver
//!
//! * Unconfigured, the policy admits **nothing** (deny-all), so plain composition never
//!   traps and never silently widens (plan/09 Decision 14).
//! * The component imports nothing — its only `use` of `eo9:pci` is for the
//!   `device-info`/`device-address` *types*, which carries no authority — so `describe`
//!   shows an empty capability surface: the policy provably cannot do anything but
//!   compute its answer.

#![no_std]

extern crate alloc;

use alloc::string::String;
use alloc::vec::Vec;

use eo9_guest::provider::ProviderState;

wit_bindgen::generate!({
    world: "address-admit",
    path: "../../../wit/pci",
    generate_all,
});

use exports::eo9::pci::address_admit_config;
use exports::eo9::pci::admit_policy;

/// The configured allow-list of (segment, bus, device, function) addresses.
/// Unconfigured means "admit nothing" (see the module docs).
static ALLOW: ProviderState<Vec<(u16, u8, u8, u8)>> = ProviderState::new();

/// The `pci.admit-address` policy.
struct Stub;

impl address_admit_config::Guest for Stub {
    fn configure(allow: Vec<address_admit_config::DeviceAddress>) -> Result<(), String> {
        ALLOW.set(
            allow
                .iter()
                .map(|a| (a.segment, a.bus, a.device, a.function))
                .collect(),
        );
        Ok(())
    }
}

impl admit_policy::Guest for Stub {
    fn admit(device: admit_policy::DeviceInfo) -> bool {
        if !ALLOW.is_set() {
            return false;
        }
        let address = (
            device.address.segment,
            device.address.bus,
            device.address.device,
            device.address.function,
        );
        ALLOW.with(|list| list.contains(&address))
    }
}

export!(Stub);
