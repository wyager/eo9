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

/// Wall-clock budget for the connect watch before concluding nothing is connected
/// (the old 50 sweeps × 50 ms, expressed as the time window it always was).
const WATCH_WINDOW_NS: u64 = 2_500_000_000;
/// Sweep pacing for the connect watch where the provider has no event surface
/// (`watch-ports` answers `unsupported` — the v1 board residue; with events the
/// RHSC wait paces the loop and this sleep never runs).
const WATCH_PACE_NS: u64 = 50_000_000;
/// Pause between empty endpoint polls (2 ms — under a keyboard's 8-10 ms interval).
/// The capability-gated fallback ONLY: where the provider routes the controller
/// interrupt (`event-driven` answers true — the QEMU PCI leg today, the board once
/// GIC SPIs 216/219 are routed), `read` parks on the interrupt and this pace is
/// skipped entirely (timer-crutch audit A1). Where it does not (eo9:platform v1),
/// reads are short polls and this pace is the documented v1 board residue.
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

    // Find the first connected root port: sweep, then park on the root-hub change
    // event (RHSC) where the provider supports it — `timed-out` answers just mean
    // "re-sweep and wait again" (the wait paced the loop); only the `unsupported`
    // fallback paces itself (audit A4). Bounded by wall clock, as the sweep count
    // always effectively was.
    let info = usb::controller(&root).await.map_err(usb_failure)?;
    let watch_started = time_api::monotonic_now(&time);
    let mut after_timed_out_wait = false;
    let port = 'watch: loop {
        for port in 1..=info.ports {
            let status = usb::port(&root, port).await.map_err(usb_failure)?;
            if status.connected {
                if after_timed_out_wait {
                    // The sweep found a connect the RHSC event never delivered:
                    // loud, never silent (owner doctrine — the fallback may keep
                    // things working but must report the missed event).
                    let _ = text::write_out_line(
                        "liveness: usb.kbd: the port sweep found a connect after a \
                         timed-out watch-ports wait - the RHSC event owed this wake",
                    );
                }
                break 'watch port;
            }
        }
        let now = time_api::monotonic_now(&time);
        if now.nanoseconds.saturating_sub(watch_started.nanoseconds) > WATCH_WINDOW_NS {
            return Err(ProgramFailure::NoKeyboard(String::from(
                "no device connected on any root-hub port",
            )));
        }
        after_timed_out_wait = false;
        match usb::watch_ports(&root).await {
            Ok(usb::WatchOutcome::Changed) => {}
            Ok(usb::WatchOutcome::TimedOut) => after_timed_out_wait = true,
            // No event surface (or the wait failed): the polled fallback paces.
            Ok(usb::WatchOutcome::Unsupported) | Err(_) => {
                time_api::sleep(&time, WATCH_PACE_NS).await;
            }
        }
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

    // The forwarding loop. Event-driven where the provider routes the controller
    // interrupt: `read` parks on WDH and an empty answer is just the provider's
    // bounded wait expiring — call again, no pacing, the core sleeps between
    // keystrokes (audit A1: this service used to wake the core every 2 ms forever).
    // Without events (the v1 board residue) reads are short polls and POLL_PACE_NS
    // keeps the loop honest toward the scheduler, exactly as before.
    let event_driven = usb::event_driven(&opened);
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
            if !event_driven {
                time_api::sleep(&time, POLL_PACE_NS).await;
            }
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
                // existing ring scan recognizes; other chords drop silently
                // (ctrl+arrow included — the plain-or-nothing v1 contract).
                if usage == USAGE_C {
                    bytes.push(0x03);
                }
                continue;
            }
            // The host-tested keymap (eo9_ohci::hid::key_console_bytes): printables
            // with shift, backspace 0x7f, and the ANSI CSI sequences for
            // arrows/Home/End/Delete — exactly what a serial terminal would send,
            // so the kernel KeyDecoder gives USB input the same history recall and
            // line editing as serial input. Multi-byte sequences ride the same
            // per-report inject below (one host call per report, ≪ the 4096 cap).
            let mut seq = [0u8; 4];
            let length = hid::key_console_bytes(usage, current.shift(), &mut seq);
            bytes.extend_from_slice(&seq[..length]);
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
