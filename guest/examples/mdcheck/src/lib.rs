//! mdcheck — the block-device probe (docs/board/usb-msd-plan.md §5.1's check-msd
//! leg, but provider-agnostic: anything exporting eo9:disk qualifies).
//!
//! Targets the `eo9-examples:mdcheck/mdcheck` world (see `wit/world.wit`):
//!
//! 1. **Size** — `disk.size`, with the one-read wake for lazy-bring-up providers
//!    (usb.msd / disk.virtio report 0 until their first awaited operation; the
//!    fs.eofs convention).
//! 2. **High-LBA scratch** — write an offset-dependent pattern over the device's
//!    LAST `span` bytes and read it back byte-exact. High addresses on purpose: an
//!    LBA-encoding bug (byte order, off-by-one block math) cannot hide at offset 0.
//!    An odd `span` makes the scratch start mid-block, so a block provider's
//!    read-modify-write edge path runs too (the check-msd gate passes one).
//! 3. **Past-capacity refusal** — a read and a write whose ranges end one byte past
//!    the capacity must answer the typed `out-of-range` (no partial I/O). Over
//!    usb.msd the refusal comes from the driver's READ-CAPACITY-derived bounds
//!    check, before any bytes move on the bus.

#![no_std]

extern crate alloc;

use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;

use eo9_guest::api::disk::disk;
use eo9_guest::{buffer, text};

eo9_guest::bindings!({
    world: "mdcheck",
    apis: [io, disk, text],
});

/// Default scratch span: 64 KiB exercises multiple READ(10)/WRITE(10) chunks over
/// usb.msd without making the full-speed QEMU leg slow.
const DEFAULT_SPAN: u64 = 64 * 1024;

/// The deterministic, offset-dependent scratch byte (the cancelcheck pattern: a
/// read serviced from the wrong offset can never match).
fn pattern(device_offset: u64) -> u8 {
    (device_offset.wrapping_mul(0x9E37_79B1) >> 16) as u8 ^ 0xc3
}

eo9_guest::main! {
    async fn main(span: Option<u32>) -> Result<ProgramSuccess, ProgramFailure> {
        let io_failure = |err: text::TextError| ProgramFailure::Io(format!("{err:?}"));
        let root = disk::default();

        // 1. The size, waking a lazy provider with one read if it reports 0.
        let mut size = disk::size(&root);
        if size == 0 {
            let (_buffer, outcome) = disk::read(&root, 0, buffer::with_capacity(1)).await;
            outcome.map_err(|err| ProgramFailure::Disk(format!("{err:?}")))?;
            size = disk::size(&root);
        }
        if size == 0 {
            return Err(ProgramFailure::TooSmall(String::from(
                "the device reports size 0 even after a wake read",
            )));
        }
        text::write_out_line(&format!("mdcheck: device size {size} bytes"))
            .map_err(io_failure)?;

        let span = match span {
            Some(0) => {
                return Err(ProgramFailure::BadArguments(String::from(
                    "span must be at least 1 byte",
                )));
            }
            Some(value) => u64::from(value).min(size),
            None => DEFAULT_SPAN.min(size),
        };
        let offset = size - span;

        // 2. High-LBA scratch: write the pattern over the device's top `span` bytes.
        let mut scratch: Vec<u8> = Vec::with_capacity(span as usize);
        for index in 0..span {
            scratch.push(pattern(offset + index));
        }
        let (_src, write_outcome) =
            disk::write(&root, offset, buffer::from_bytes(&scratch)).await;
        let written = write_outcome.map_err(|err| ProgramFailure::Disk(format!("{err:?}")))?;
        if written.bytes_written != span {
            return Err(ProgramFailure::Disk(format!(
                "short write: {} of {span} byte(s)",
                written.bytes_written
            )));
        }

        let (dst, read_outcome) =
            disk::read(&root, offset, buffer::with_capacity(span)).await;
        let read = read_outcome.map_err(|err| ProgramFailure::Disk(format!("{err:?}")))?;
        if read.bytes_read != span {
            return Err(ProgramFailure::Disk(format!(
                "short read: {} of {span} byte(s)",
                read.bytes_read
            )));
        }
        let bytes = buffer::prefix_to_vec(&dst, span);
        for (index, &byte) in bytes.iter().enumerate() {
            let expected = pattern(offset + index as u64);
            if byte != expected {
                return Err(ProgramFailure::Mismatch(format!(
                    "offset {}: read {byte:#04x}, wrote {expected:#04x}",
                    offset + index as u64
                )));
            }
        }
        text::write_out_line(&format!(
            "mdcheck: wrote and re-read {span} byte(s) at offset {offset} - byte-exact"
        ))
        .map_err(io_failure)?;

        // 3. Past-capacity accesses answer the typed out-of-range, with no partial
        // I/O. The ranges end exactly one byte past the device.
        let (_buffer, past_read) =
            disk::read(&root, size - span + 1, buffer::with_capacity(span)).await;
        match past_read {
            Err(disk::ReadError::OutOfRange) => {}
            other => {
                return Err(ProgramFailure::NotRefused(format!(
                    "past-capacity read answered {other:?}, not out-of-range"
                )));
            }
        }
        text::write_out_line("mdcheck: past-capacity read answered out-of-range - ok")
            .map_err(io_failure)?;

        let (_buffer, past_write) =
            disk::write(&root, size - span + 1, buffer::from_bytes(&scratch)).await;
        match past_write {
            Err(disk::WriteError::OutOfRange) => {}
            other => {
                return Err(ProgramFailure::NotRefused(format!(
                    "past-capacity write answered {other:?}, not out-of-range"
                )));
            }
        }
        text::write_out_line("mdcheck: past-capacity write answered out-of-range - ok")
            .map_err(io_failure)?;

        Ok(ProgramSuccess::Verified(span))
    }
}
