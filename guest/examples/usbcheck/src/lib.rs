//! usbcheck — bring up the granted USB host controller, watch its ports, enumerate.
//!
//! Targets the `eo9-examples:usbcheck/usbcheck` world (see `wit/world.wit`). The
//! program holds no controller knowledge: which silicon answers is the provider's
//! business (`usb.ohci-pci` over QEMU's `-device pci-ohci`, `usb.ohci` over the board
//! profile's region table); usbcheck speaks descriptors — the consumer half of the
//! eo9:usb split, with the parsing done by the same host-tested `eo9-ohci` core the
//! shells use.

#![no_std]

extern crate alloc;

use alloc::format;
use alloc::string::String;

use eo9_guest::api::time::time as time_api;
use eo9_guest::api::usb::usb;
use eo9_guest::text;
use eo9_ohci::descriptor::{self, Descriptor, DeviceDescriptor};
use eo9_ohci::setup::descriptor_type;

eo9_guest::bindings!({
    world: "usbcheck",
    apis: [usb, time, text],
});

/// How many port-status sweeps to make before concluding nothing is connected. Each
/// sweep is one host call per port; QEMU's usb-kbd reports connected from boot, and
/// the bench shape is "plug, then re-run".
const WATCH_SWEEPS: u32 = 25;

/// Watch-mode sweep pacing: 100 ms keeps the loop honest toward the scheduler (the
/// D46 stranded-runnable lesson) and is far inside human plug/unplug timing.
const WATCH_PACE_NS: u64 = 100_000_000;

eo9_guest::main! {
    async fn main(watch_ms: Option<u32>) -> Result<ProgramSuccess, ProgramFailure> {
        let io_failure = |err: text::TextError| ProgramFailure::Io(format!("{err:?}"));
        let usb_failure = |err: usb::UsbError| match err {
            usb::UsbError::Denied => ProgramFailure::Denied,
            usb::UsbError::NoController => ProgramFailure::NoController,
            other => ProgramFailure::Io(format!("{other:?}")),
        };

        let root = usb::default();

        // 1. Controller identity (the shell's bring-up claim happens here).
        let info = usb::controller(&root).await.map_err(usb_failure)?;
        text::write_out_line(&format!(
            "usbcheck: controller revision {:x}.{:x}, {} root-hub port(s)",
            info.revision >> 4,
            info.revision & 0xf,
            info.ports,
        ))
        .map_err(io_failure)?;

        // Watch mode (M1 plug/unplug acceptance): poll every port for the window,
        // print each transition, count them. No enumeration — the operator is
        // moving cables; CCS/CSC/LSDA are the observables (OHCI 1.0a §7.4.4).
        if let Some(window_ms) = watch_ms {
            let time = time_api::default();
            let started = time_api::monotonic_now(&time);
            let window_ns = u64::from(window_ms) * 1_000_000;
            let mut previous: [Option<usb::PortStatus>; 16] = [const { None }; 16];
            let mut transitions: u32 = 0;
            loop {
                let now = time_api::monotonic_now(&time);
                if now.nanoseconds.saturating_sub(started.nanoseconds) > window_ns {
                    break;
                }
                for port in 1..=info.ports.min(16) {
                    let status = usb::port(&root, port).await.map_err(usb_failure)?;
                    let slot = &mut previous[(port - 1) as usize];
                    let changed = match slot {
                        Some(seen) => {
                            seen.connected != status.connected
                                || seen.enabled != status.enabled
                                || seen.low_speed != status.low_speed
                                || seen.connect_change != status.connect_change
                        }
                        None => true,
                    };
                    if changed {
                        transitions += 1;
                        text::write_out_line(&format!(
                            "usbcheck: watch port {port}: CCS={} PES={} PPS={} LSDA={} CSC={}",
                            status.connected,
                            status.enabled,
                            status.powered,
                            status.low_speed,
                            status.connect_change,
                        ))
                        .map_err(io_failure)?;
                    }
                    *slot = Some(status);
                }
                time_api::sleep(&time, WATCH_PACE_NS).await;
            }
            text::write_out_line(&format!(
                "usbcheck: watch window closed ({transitions} transition line(s))"
            ))
            .map_err(io_failure)?;
            return Ok(ProgramSuccess::Watched(transitions));
        }

        // 2. Bounded port watch: print each port once, keep sweeping until a
        // connection shows (or the bound expires — a typed success either way).
        let mut connected_port = None;
        'watch: for sweep in 0..WATCH_SWEEPS {
            for port in 1..=info.ports {
                let status = usb::port(&root, port).await.map_err(usb_failure)?;
                if sweep == 0 {
                    text::write_out_line(&format!(
                        "usbcheck: port {port}: connected={} enabled={} powered={} \
                         low-speed={} connect-change={}",
                        status.connected,
                        status.enabled,
                        status.powered,
                        status.low_speed,
                        status.connect_change,
                    ))
                    .map_err(io_failure)?;
                }
                if status.connected {
                    connected_port = Some(port);
                    break 'watch;
                }
            }
        }
        let Some(port) = connected_port else {
            text::write_out_line(
                "usbcheck: no device connected within the watch window (plug one and re-run)",
            )
            .map_err(io_failure)?;
            return Ok(ProgramSuccess::NoDevice);
        };

        // 3. Attach: port reset, SET_ADDRESS, the provider validates the chain.
        let device = usb::attach(&root, port).await.map_err(usb_failure)?;
        text::write_out_line(&format!("usbcheck: attached the device on port {port}"))
            .map_err(io_failure)?;

        // 4. The descriptor chain, read and printed by this consumer (the WIT's
        // split: the shell owns transfers, the program owns meaning).
        let device_bytes =
            usb::control_in(&device, 0x80, 6, u16::from(descriptor_type::DEVICE) << 8, 0, 18)
                .await
                .map_err(usb_failure)?;
        let parsed = DeviceDescriptor::parse(&device_bytes).ok_or_else(|| {
            ProgramFailure::Io(String::from("the device descriptor did not parse"))
        })?;
        text::write_out_line(&format!(
            "usbcheck: device {:04x}:{:04x} usb {:x}.{:02x} class {:02x}.{:02x}.{:02x} \
             ep0-max-packet {} configurations {}",
            parsed.vendor_id,
            parsed.product_id,
            parsed.usb_version >> 8,
            parsed.usb_version & 0xff,
            parsed.class,
            parsed.subclass,
            parsed.protocol,
            parsed.max_packet_size_ep0,
            parsed.num_configurations,
        ))
        .map_err(io_failure)?;

        let head = usb::control_in(
            &device,
            0x80,
            6,
            u16::from(descriptor_type::CONFIGURATION) << 8,
            0,
            9,
        )
        .await
        .map_err(usb_failure)?;
        let configuration = descriptor::ConfigurationDescriptor::parse(&head).ok_or_else(|| {
            ProgramFailure::Io(String::from("the configuration descriptor did not parse"))
        })?;
        let blob = usb::control_in(
            &device,
            0x80,
            6,
            u16::from(descriptor_type::CONFIGURATION) << 8,
            0,
            configuration.total_length,
        )
        .await
        .map_err(usb_failure)?;

        for entry in descriptor::descriptors(&blob) {
            let line = match entry {
                Descriptor::Configuration(c) => format!(
                    "usbcheck:   configuration {}: {} interface(s), total {} bytes, \
                     max-power {} mA",
                    c.configuration_value,
                    c.num_interfaces,
                    c.total_length,
                    u32::from(c.max_power) * 2,
                ),
                Descriptor::Interface(i) => format!(
                    "usbcheck:   interface {}: class {:02x}.{:02x}.{:02x} ({} endpoint(s))",
                    i.interface_number, i.class, i.subclass, i.protocol, i.num_endpoints,
                ),
                Descriptor::Endpoint(e) => format!(
                    "usbcheck:   endpoint {:#04x}: {} {}, max-packet {}, interval {} ms",
                    e.address,
                    match e.attributes & 0b11 {
                        0 => "control",
                        1 => "isochronous",
                        2 => "bulk",
                        _ => "interrupt",
                    },
                    if e.is_in() { "IN" } else { "OUT" },
                    e.max_packet_size,
                    e.interval,
                ),
                Descriptor::Other(raw) => format!(
                    "usbcheck:   class descriptor type {:#04x} ({} bytes)",
                    raw[1],
                    raw.len(),
                ),
            };
            text::write_out_line(&line).map_err(io_failure)?;
        }

        // The HID summary line the QEMU lane pins (usb-kbd: boot keyboard 3/1/1).
        if let Some(boot) = descriptor::find_boot_interface(&blob) {
            text::write_out_line(&format!(
                "usbcheck: boot-protocol HID interface {} (protocol {}), interrupt-IN \
                 {:#04x} every {} ms",
                boot.interface.interface_number,
                match boot.interface.protocol {
                    descriptor::PROTOCOL_KEYBOARD => "keyboard",
                    descriptor::PROTOCOL_MOUSE => "mouse",
                    _ => "other",
                },
                boot.endpoint.address,
                boot.endpoint.interval,
            ))
            .map_err(io_failure)?;
        }

        Ok(ProgramSuccess::Enumerated(port))
    }
}
