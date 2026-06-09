//! `usb.kbd` — boot-protocol keystrokes into the kernel console (the M4 service).
//!
//! Targets the crate-local `eo9:usb-kbd/kbd` world (see `wit/world.wit`): a thin
//! pipeline over the composed eo9:usb provider — find the keyboard (directly or
//! behind one hub level, the bench keyboard's shape), boot-protocol setup, then the
//! forwarding loop: report → `pressed_since` edges → bytes → `console-sink.inject`.
//!
//! Key mapping (v1, per the M4 plan): the US-layout printable set with shift
//! (`eo9_ohci::hid::key_ascii`), enter → `\n`, tab → `\t`, backspace → 0x7f (what a
//! serial terminal sends), and ctrl+c → the raw 0x03 byte, which the kernel's
//! existing Ctrl-C ring scan catches exactly as if the serial key was pressed —
//! a USB Ctrl-C kills the foreground task like a serial one, by construction.
//! Everything else (arrows, F-keys, other ctrl chords) is dropped silently in v1.
//!
//! Console output discipline: ONE banner line at start; never a line per key (this
//! service's output interleaves with the prompt its own keystrokes drive).

#![no_std]

extern crate alloc;

use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;

use eo9_ohci::descriptor::{self, BootInterface};
use eo9_ohci::hid::{self, KeyboardReport};
use eo9_ohci::setup::descriptor_type;

use eo9_guest::api::console_sink::sink;
use eo9_guest::api::time::time as time_api;
use eo9_guest::api::usb::usb;
use eo9_guest::text;

eo9_guest::bindings!({
    world: "kbd",
    apis: [usb, console_sink, time, text],
});

/// Port sweeps before concluding nothing is connected.
const WATCH_SWEEPS: u32 = 50;
/// Pause between empty endpoint polls (2 ms — under a keyboard's 8-10 ms interval).
const POLL_PACE_NS: u64 = 2_000_000;
/// HID usage for 'c' (ctrl+c -> raw 0x03).
const USAGE_C: u8 = 0x06;
/// Modifier mask for either ctrl key.
const CTRL: u8 = hid::modifier::LEFT_CTRL | hid::modifier::RIGHT_CTRL;

eo9_guest::main! {
    async fn main(window_ms: Option<u32>) -> Result<ProgramSuccess, ProgramFailure> {
    let io_failure = |err: text::TextError| ProgramFailure::Io(format!("{err:?}"));
    let usb_failure = |err: usb::UsbError| match err {
        usb::UsbError::Denied => ProgramFailure::Denied,
        usb::UsbError::NoController => ProgramFailure::NoController,
        other => ProgramFailure::Io(format!("{other:?}")),
    };

    let root = usb::default();
    let time = time_api::default();
    let console = sink::default();

    // Find the first connected root port.
    let info = usb::controller(&root).await.map_err(usb_failure)?;
    let mut connected_port = None;
    'watch: for _ in 0..WATCH_SWEEPS {
        for port in 1..=info.ports {
            let status = usb::port(&root, port).await.map_err(usb_failure)?;
            if status.connected {
                connected_port = Some(port);
                break 'watch;
            }
        }
        time_api::sleep(&time, 50_000_000).await;
    }
    let Some(port) = connected_port else {
        return Err(ProgramFailure::NoKeyboard(String::from(
            "no device connected on any root-hub port",
        )));
    };

    // Attach; traverse one hub level when the root device is a hub (class 09).
    let mut device = usb::attach(&root, port).await.map_err(usb_failure)?;
    let head = usb::control_in(&device, 0x80, 6, 0x0100, 0, 18)
        .await
        .map_err(usb_failure)?;
    if head.get(4).copied() == Some(0x09) {
        device = usb::attach_child(&device).await.map_err(|err| match err {
            usb::UsbError::Io(message) => ProgramFailure::NoKeyboard(message),
            other => usb_failure(other),
        })?;
    }

    // The configuration blob, down to the boot KEYBOARD interface.
    let config_head = usb::control_in(
        &device,
        0x80,
        6,
        u16::from(descriptor_type::CONFIGURATION) << 8,
        0,
        9,
    )
    .await
    .map_err(usb_failure)?;
    let configuration = descriptor::ConfigurationDescriptor::parse(&config_head)
        .ok_or_else(|| {
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
    let BootInterface {
        interface,
        endpoint,
    } = descriptor::find_boot_interface(&blob).ok_or_else(|| {
        ProgramFailure::NoKeyboard(String::from(
            "the device has no boot-protocol HID interface",
        ))
    })?;
    if interface.protocol != descriptor::PROTOCOL_KEYBOARD {
        return Err(ProgramFailure::NoKeyboard(format!(
            "boot-protocol device found, but it is protocol {} (not a keyboard)",
            interface.protocol,
        )));
    }

    // Boot-protocol setup: configuration + protocol required; idle tolerated
    // (optional for some devices; the M3-fix posture — a refusal costs repeats
    // that pressed_since dedupes anyway).
    usb::control_out(
        &device,
        0x00,
        9,
        u16::from(configuration.configuration_value),
        0,
        Vec::new(),
    )
    .await
    .map_err(usb_failure)?;
    usb::control_out(
        &device,
        0x21,
        0x0b,
        0,
        u16::from(interface.interface_number),
        Vec::new(),
    )
    .await
    .map_err(usb_failure)?;
    let _ = usb::control_out(
        &device,
        0x21,
        0x0a,
        0,
        u16::from(interface.interface_number),
        Vec::new(),
    )
    .await;

    let opened = usb::open_interrupt_in(
        &device,
        endpoint.number(),
        endpoint.max_packet_size,
        endpoint.interval,
    )
    .await
    .map_err(usb_failure)?;

    text::write_out_line(
        "usb.kbd: forwarding boot-protocol keystrokes to the console (ctrl+c interrupts \
         like the serial key)",
    )
    .map_err(io_failure)?;

    // The forwarding loop.
    let started = time_api::monotonic_now(&time);
    let window_ns = u64::from(window_ms.unwrap_or(0)) * 1_000_000;
    let mut previous = KeyboardReport::default();
    let mut forwarded: u32 = 0;
    loop {
        if window_ns > 0 {
            let now = time_api::monotonic_now(&time);
            if now.nanoseconds.saturating_sub(started.nanoseconds) > window_ns {
                return Ok(ProgramSuccess::Forwarded(forwarded));
            }
        }
        let report = usb::read(&opened).await.map_err(|err| {
            ProgramFailure::DeviceLost(format!("interrupt endpoint: {err:?}"))
        })?;
        if report.is_empty() {
            time_api::sleep(&time, POLL_PACE_NS).await;
            continue;
        }
        let Some(current) = KeyboardReport::parse(&report) else {
            continue;
        };
        if current.is_rollover_error() {
            continue;
        }
        let mut bytes: Vec<u8> = Vec::new();
        let ctrl_held = current.modifiers & CTRL != 0;
        for usage in current.pressed_since(&previous) {
            if ctrl_held {
                // v1 ctrl handling: only ctrl+c, as the raw ETX byte the kernel's
                // existing ring scan recognizes; other chords drop silently.
                if usage == USAGE_C {
                    bytes.push(0x03);
                }
                continue;
            }
            match hid::key_ascii(usage, current.shift()) {
                Some(ch) => bytes.push(ch as u8),
                // Backspace as a serial terminal sends it; everything else
                // (arrows, F-keys, …) drops silently in v1.
                None if usage == 0x2a => bytes.push(0x7f),
                None => {}
            }
        }
        previous = current;
        if bytes.is_empty() {
            continue;
        }
        let count = bytes.len() as u32;
        match sink::inject(&console, &bytes) {
            Ok(accepted) => forwarded += accepted.min(count),
            Err(sink::SinkError::Denied) => return Err(ProgramFailure::Denied),
            Err(sink::SinkError::Io(message)) => return Err(ProgramFailure::Io(message)),
        }
    }
    }
}
