//! `pci.admit-vendor` — the vendor/device identity PCI admit policy.
//!
//! Targets the `eo9:pci/vendor-admit` stub world: a pure policy component ("policies
//! are programs" — SPEC, Eo9 API design) exporting `eo9:pci/admit-policy`, deciding by
//! what a device *is* (vendor:device identity) rather than where it sits (bus address):
//!
//!   pci.admit-vendor --allow "[{vendor-id: 6900, device-id: 4096}]" $ pci.filtered $ driver
//!
//! Identity-keyed grants are stable across boot configurations and slot changes — the
//! address of a virtio NIC moves when QEMU's device order changes, but `1af4:1000` is
//! always virtio-net (user study 09's address-fragility finding).
//!
//! * Unconfigured, the policy admits **nothing** (deny-all), so plain composition never
//!   traps and never silently widens (plan/09 Decision 14).
//! * The component imports nothing — its only `use` of `eo9:pci` is for the
//!   `device-info` *type*, which carries no authority — so `describe` shows an empty
//!   capability surface: the policy provably cannot do anything but compute its answer.

#![no_std]

extern crate alloc;

use alloc::string::String;
use alloc::vec::Vec;

use eo9_guest::provider::ProviderState;

wit_bindgen::generate!({
    world: "vendor-admit",
    path: "../../../wit/pci",
    generate_all,
});

use exports::eo9::pci::admit_policy;
use exports::eo9::pci::vendor_admit_config;

/// The configured allow-list of (vendor-id, device-id) identities.
/// Unconfigured means "admit nothing" (see the module docs).
static ALLOW: ProviderState<Vec<(u16, u16)>> = ProviderState::new();

/// The `pci.admit-vendor` policy.
struct Stub;

impl vendor_admit_config::Guest for Stub {
    fn configure(allow: Vec<vendor_admit_config::VendorDevice>) -> Result<(), String> {
        ALLOW.set(allow.iter().map(|v| (v.vendor_id, v.device_id)).collect());
        Ok(())
    }
}

impl admit_policy::Guest for Stub {
    fn admit(device: admit_policy::DeviceInfo) -> bool {
        if !ALLOW.is_set() {
            return false;
        }
        ALLOW.with(|list| list.contains(&(device.vendor_id, device.device_id)))
    }
}

export!(Stub);
