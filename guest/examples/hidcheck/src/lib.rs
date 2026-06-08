//! hidcheck — configure a boot-protocol HID device and print decoded reports.
//!
//! Targets the `eo9-examples:hidcheck/hidcheck` world (see `wit/world.wit`): the M3
//! milestone of the USB lane, run QEMU-first against `usb.ohci-pci` + `-device
//! usb-kbd` with QMP key injection (`check-usb`), byte-identical on the board later.
//! Polling paces itself with `time.sleep` between empty reads — a guest hot-spinning
//! on synchronous host calls is invisible to cooperative scheduling (the D46
//! stranded-runnable lesson), and a boot keyboard's interval is milliseconds anyway.

#![no_std]

extern crate alloc;

use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;

use eo9_guest::api::time::time as time_api;
use eo9_guest::api::usb::usb;
use eo9_guest::text;
use eo9_ohci::descriptor::{self, BootInterface};
use eo9_ohci::hid::{self, KeyboardReport, MouseReport};
use eo9_ohci::setup::descriptor_type;

eo9_guest::bindings!({
    world: "hidcheck",
    apis: [usb, time, text],
});

/// Port sweeps before concluding nothing is connected (one host call per port each).
const WATCH_SWEEPS: u32 = 25;
/// Pause between empty endpoint polls, in nanoseconds (2 ms — well under a boot
/// keyboard's 8-10 ms interval, far above hot-spin).
const POLL_PACE_NS: u64 = 2_000_000;

eo9_guest::main! {
    async fn main(
        reports: Option<u32>,
        window_ms: Option<u32>,
    ) -> Result<ProgramSuccess, ProgramFailure> {
        let io_failure = |err: text::TextError| ProgramFailure::Io(format!("{err:?}"));
        let usb_failure = |err: usb::UsbError| match err {
            usb::UsbError::Denied => ProgramFailure::Denied,
            usb::UsbError::NoController => ProgramFailure::NoController,
            other => ProgramFailure::Io(format!("{other:?}")),
        };
        let target_reports = match reports {
            Some(0) => return Err(ProgramFailure::BadArguments(String::from(
                "--reports must be at least 1",
            ))),
            Some(count) => count,
            None => 5,
        };
        let window_ns = u64::from(window_ms.unwrap_or(30_000)) * 1_000_000;

        let root = usb::default();
        let time = time_api::default();

        // Controller + first connected port (the usbcheck preamble, terse).
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
        }
        let Some(port) = connected_port else {
            return Err(ProgramFailure::NoHid(String::from(
                "no device connected on any root-hub port",
            )));
        };
        let device = usb::attach(&root, port).await.map_err(usb_failure)?;

        // The configuration blob, parsed down to the boot interface + endpoint.
        let head = usb::control_in(
            &device, 0x80, 6, u16::from(descriptor_type::CONFIGURATION) << 8, 0, 9,
        )
        .await
        .map_err(usb_failure)?;
        let configuration =
            descriptor::ConfigurationDescriptor::parse(&head).ok_or_else(|| {
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
        let BootInterface { interface, endpoint } = descriptor::find_boot_interface(&blob)
            .ok_or_else(|| {
                ProgramFailure::NoHid(String::from(
                    "the device has no boot-protocol HID interface",
                ))
            })?;
        let kind = match interface.protocol {
            descriptor::PROTOCOL_KEYBOARD => "keyboard",
            descriptor::PROTOCOL_MOUSE => "mouse",
            _ => "other",
        };
        text::write_out_line(&format!(
            "hidcheck: boot {kind} on port {port}, interrupt-IN {:#04x} every {} ms",
            endpoint.address, endpoint.interval,
        ))
        .map_err(io_failure)?;

        // Class setup (HID 1.11 §7.2): configuration, boot protocol, indefinite idle
        // (report only on change — what makes pressed_since edges meaningful).
        let class_failure = |err: usb::UsbError| match err {
            usb::UsbError::Stall => ProgramFailure::NoHid(String::from(
                "the device STALLed boot-protocol setup",
            )),
            other => usb_failure(other),
        };
        usb::control_out(&device, 0x00, 9, u16::from(configuration.configuration_value), 0, Vec::new())
            .await
            .map_err(class_failure)?;
        usb::control_out(&device, 0x21, 0x0b, 0, u16::from(interface.interface_number), Vec::new())
            .await
            .map_err(class_failure)?;
        usb::control_out(&device, 0x21, 0x0a, 0, u16::from(interface.interface_number), Vec::new())
            .await
            .map_err(class_failure)?;

        let opened = usb::open_interrupt_in(
            &device,
            endpoint.number(),
            endpoint.max_packet_size,
            endpoint.interval,
        )
        .await
        .map_err(usb_failure)?;

        // Poll, decode, count. The reads are short polls (empty = nothing waiting);
        // pacing sleeps keep the loop honest toward the scheduler.
        text::write_out_line(&format!(
            "hidcheck: polling for {target_reports} report(s) (type into the device / \
             inject events now)"
        ))
        .map_err(io_failure)?;
        let started = time_api::monotonic_now(&time);
        let mut first_report_at = None;
        let mut last_report_at = started;
        let mut count: u32 = 0;
        let mut previous = KeyboardReport::default();
        loop {
            let now = time_api::monotonic_now(&time);
            if now.nanoseconds.saturating_sub(started.nanoseconds) > window_ns {
                break;
            }
            let report = usb::read(&opened).await.map_err(usb_failure)?;
            if report.is_empty() {
                time_api::sleep(&time, POLL_PACE_NS).await;
                continue;
            }
            count += 1;
            last_report_at = now;
            if first_report_at.is_none() {
                first_report_at = Some(now);
            }
            let raw: Vec<String> =
                report.iter().map(|byte| format!("{byte:02x}")).collect();
            let decoded = match interface.protocol {
                descriptor::PROTOCOL_KEYBOARD => match KeyboardReport::parse(&report) {
                    Some(current) if current.is_rollover_error() => {
                        String::from("rollover error")
                    }
                    Some(current) => {
                        let mut keys = String::new();
                        for usage in current.pressed_since(&previous) {
                            if !keys.is_empty() {
                                keys.push(' ');
                            }
                            match hid::key_ascii(usage, current.shift()) {
                                Some('\n') => keys.push_str("<enter>"),
                                Some('\t') => keys.push_str("<tab>"),
                                Some(ch) => {
                                    keys.push('\'');
                                    keys.push(ch);
                                    keys.push('\'');
                                }
                                None => match hid::key_name(usage) {
                                    Some(name) => {
                                        keys.push('<');
                                        keys.push_str(name);
                                        keys.push('>');
                                    }
                                    None => keys.push_str(&format!("usage({usage:#04x})")),
                                },
                            }
                        }
                        previous = current;
                        if keys.is_empty() {
                            String::from("(release)")
                        } else {
                            keys
                        }
                    }
                    None => String::from("(short keyboard report)"),
                },
                descriptor::PROTOCOL_MOUSE => match MouseReport::parse(&report) {
                    Some(mouse) => format!(
                        "buttons {:#04b} dx {} dy {}",
                        mouse.buttons, mouse.dx, mouse.dy,
                    ),
                    None => String::from("(short mouse report)"),
                },
                _ => String::from("(unknown protocol)"),
            };
            text::write_out_line(&format!(
                "hidcheck: report {count} [{}] {decoded}",
                raw.join(" "),
            ))
            .map_err(io_failure)?;
            if count >= target_reports {
                break;
            }
        }

        if count == 0 {
            return Err(ProgramFailure::NoHid(String::from(
                "no reports arrived within the window",
            )));
        }
        // reports/s over the span from first to last report (autorepeat cadence —
        // the plan's dropped-poll detector; 1 report has no span, reported as such).
        let span_ns = last_report_at
            .nanoseconds
            .saturating_sub(first_report_at.expect("count > 0").nanoseconds);
        if count > 1 && span_ns > 0 {
            let per_second = (u64::from(count - 1) * 1_000_000_000) / span_ns;
            text::write_out_line(&format!(
                "hidcheck: {count} report(s), ~{per_second} reports/s across the burst"
            ))
            .map_err(io_failure)?;
        } else {
            text::write_out_line(&format!(
                "hidcheck: {count} report(s) (too few for a rate)"
            ))
            .map_err(io_failure)?;
        }
        Ok(ProgramSuccess::Reports(count))
    }
}
