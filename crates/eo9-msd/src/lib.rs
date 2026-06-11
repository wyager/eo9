//! The pure core of the `usb.msd` driver component: USB Mass Storage Class
//! Bulk-Only Transport (BOT) framing, the three-phase command ladder with the
//! BOT error-recovery rules, and the minimal SCSI command set a boot stick needs.
//!
//! Everything here is byte arithmetic and protocol sequencing over values the
//! component moves through `eo9:usb` (bulk transfers and class control requests),
//! so it is host-testable without any device: the wasm component
//! (`guest/stubs/usb-msd`) is a thin I/O shell over this crate, and
//! `cargo test -p eo9-msd` pins the encodings and the recovery ladder against
//! scripted endpoint mocks (the eo9-ohci / eo9-rtl8125 precedent for keeping pure
//! logic out of untestable component crates).
//!
//! ## References (the citation rule: every constant names its source)
//!
//! * **BOT**: USB Mass Storage Class Bulk-Only Transport, Revision 1.0
//!   (USB-IF, 1999) — CBW/CSW wire formats (§5.1/§5.2), the class requests
//!   (§3.1/§3.2), the host-side state machine and error recovery (§5.3, §6.6/§6.7,
//!   figure 2).
//! * **SCSI**: the six commands are the SBC/SPC subset every USB stick's
//!   "SCSI transparent command set" (interface subclass 06) implements —
//!   opcodes and CDB layouts per SPC-3 (INQUIRY §6.4, TEST UNIT READY §6.33,
//!   REQUEST SENSE §6.27, fixed sense format §4.5.3) and SBC-2
//!   (READ CAPACITY(10) §5.10, READ(10) §5.6, WRITE(10) §5.25).

#![cfg_attr(not(test), no_std)]

extern crate alloc;

pub mod bot;
pub mod device;
pub mod scsi;

pub use device::{Bot, MsdError, Transport, TransportError};
