//! lspci — list the PCI functions visible through the granted eo9:pci capability.
//!
//! Targets the `eo9-examples:lspci/lspci` world (see `wit/world.wit`): enumerate the
//! capability's view of the bus and print one line per function — address, vendor:device,
//! class/subclass/prog-if, revision, header kind. The program holds no policy of its own:
//! on the bare-metal kernel the real bus is only visible when the boot granted PCI (the
//! `pci` kernel command-line token), and an attenuating provider (`pci.filtered`,
//! `pci.deny`) composed in front of it narrows or refuses the view without the program
//! changing.

#![no_std]

extern crate alloc;

use alloc::format;

use eo9_guest::api::pci::pci;
use eo9_guest::text;

eo9_guest::bindings!({
    world: "lspci",
    apis: [pci, text],
});

eo9_guest::main! {
    async fn main() -> Result<ProgramSuccess, ProgramFailure> {
        let io_failure = |err: text::TextError| ProgramFailure::Io(format!("{err:?}"));

        let root = pci::default();
        let devices = pci::enumerate(&root).await.map_err(|err| match err {
            pci::PciError::Denied => ProgramFailure::Denied,
            other => ProgramFailure::Io(format!("{other:?}")),
        })?;

        for device in &devices {
            let address = device.address;
            let header = match device.header {
                pci::HeaderType::Endpoint => "endpoint",
                pci::HeaderType::PciBridge => "pci-bridge",
                pci::HeaderType::CardbusBridge => "cardbus-bridge",
            };
            let line = format!(
                "{:04x}:{:02x}:{:02x}.{} {:04x}:{:04x} class {:02x}.{:02x}.{:02x} rev {:02x} {}",
                address.segment,
                address.bus,
                address.device,
                address.function,
                device.vendor_id,
                device.device_id,
                device.class_code,
                device.subclass,
                device.prog_if,
                device.revision,
                header,
            );
            text::write_out_line(&line).map_err(io_failure)?;
        }
        if devices.is_empty() {
            text::write_out_line("(no PCI functions visible through this capability)")
                .map_err(io_failure)?;
        }
        Ok(ProgramSuccess::Devices(devices.len() as u32))
    }
}
