//! partcheck — the disk window probe (the `disk.part` gate's consumer).
//!
//! Targets the `eo9-examples:partcheck/partcheck` world (see `wit/world.wit`). Composed
//! over any `eo9:disk` — in the gates, over `disk.part`'s exported window:
//!
//! * **window mode**: wake the chain (one read; lazily-binding providers answer `size`
//!   0 until a first awaited op — the disk.virtio convention disk.part inherits),
//!   report the size, verify the fixture magic at offset 0, round-trip a patterned
//!   write inside the window, and pin the boundary semantics — a read or write at or
//!   crossing the window's end must answer the typed `out-of-range`, while a
//!   zero-length read at exactly the end must succeed (the disk.mem convention).
//! * **refusal mode**: the first read itself must fail typed (a GPT disk behind
//!   `disk.part`, an absent partition, an invalid table) and the error text must
//!   carry the expected needle, so a gate can pin *which* refusal fired.
//!
//! Every probe prints one transcript line, so the QEMU gate (and a bench operator)
//! can see each verdict, not just the final outcome.

#![no_std]
// Three typed `main` parameters (two of them options) lower to more core-glue
// arguments than clippy's budget; the WIT signature is the real interface, so the
// lint is noise here (the telnetd precedent).
#![allow(clippy::too_many_arguments)]

extern crate alloc;

use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;

use eo9_guest::api::disk::disk;
use eo9_guest::{buffer, text};

eo9_guest::bindings!({
    world: "partcheck",
    apis: [io, disk, text],
});

/// Length of the write round-trip pattern (window mode).
const PATTERN_LEN: u64 = 4096;

/// The deterministic, offset-dependent probe pattern (offset-dependent so a write or
/// read serviced at the wrong offset can never match).
fn pattern(index: u64) -> u8 {
    (index.wrapping_mul(0x9E37_79B1) >> 13) as u8 ^ 0xC3
}

fn say(line: &str) -> Result<(), ProgramFailure> {
    text::write_out_line(line).map_err(|err| ProgramFailure::Io(format!("{err:?}")))
}

eo9_guest::main! {
    async fn main(
        mode: String,
        magic: Option<String>,
        needle: Option<String>,
    ) -> Result<ProgramSuccess, ProgramFailure> {
        match mode.as_str() {
            "window" => window_mode(magic).await,
            "refusal" => refusal_mode(needle).await,
            other => Err(ProgramFailure::BadArguments(format!(
                "mode must be `window` or `refusal`, not `{other}`"
            ))),
        }
    }
}

async fn window_mode(magic: Option<String>) -> Result<ProgramSuccess, ProgramFailure> {
    let dev = disk::default();
    let mut probes: u32 = 0;

    // Wake the chain: lazily-binding providers (disk.virtio, disk.part) answer size 0
    // until a first awaited operation brings them up.
    let (_buf, wake) = disk::read(&dev, 0, buffer::with_capacity(1)).await;
    wake.map_err(|err| ProgramFailure::Disk(format!("wake read: {err:?}")))?;

    let size = disk::size(&dev);
    say(&format!("partcheck: window size={size}"))?;
    if size == 0 {
        return Err(ProgramFailure::Disk(String::from(
            "size is 0 after a successful wake read",
        )));
    }

    // The fixture magic at offset 0 — proof the window starts at the partition start
    // (LBA 0 of the window = the partition's first sector), not anywhere else.
    if let Some(magic) = magic {
        let want = magic.as_bytes();
        let dst = buffer::with_capacity(want.len() as u64);
        let (dst, read) = disk::read(&dev, 0, dst).await;
        let read = read.map_err(|err| ProgramFailure::Disk(format!("magic read: {err:?}")))?;
        let got = buffer::prefix_to_vec(&dst, read.bytes_read);
        if got != want {
            return Err(ProgramFailure::Mismatch(format!(
                "magic at offset 0: wanted {magic:?}, got {:?}",
                String::from_utf8_lossy(&got)
            )));
        }
        say("partcheck: magic at offset 0 matches - ok")?;
        probes += 1;
    }

    // Write round-trip inside the window (away from the magic): the bytes land and
    // read back exactly.
    let len = PATTERN_LEN.min(size / 4).max(1);
    let offset = size / 2;
    let bytes: Vec<u8> = (0..len).map(pattern).collect();
    let src = buffer::from_bytes(&bytes);
    let (_src, written) = disk::write(&dev, offset, src).await;
    let written = written.map_err(|err| ProgramFailure::Disk(format!("write: {err:?}")))?;
    if written.bytes_written != len {
        return Err(ProgramFailure::Disk(format!(
            "short write ({} of {len} bytes)",
            written.bytes_written
        )));
    }
    let (dst, read) = disk::read(&dev, offset, buffer::with_capacity(len)).await;
    let read = read.map_err(|err| ProgramFailure::Disk(format!("read back: {err:?}")))?;
    if buffer::prefix_to_vec(&dst, read.bytes_read) != bytes {
        return Err(ProgramFailure::Mismatch(format!(
            "write round-trip at offset {offset} read back different bytes"
        )));
    }
    say(&format!(
        "partcheck: write+read-back of {len} bytes at offset {offset} - ok"
    ))?;
    probes += 1;

    // Boundary enforcement: at and crossing the end, reads and writes must answer the
    // typed out-of-range — never a trap, never data from beyond the window.
    let (_buf, at_end) = disk::read(&dev, size, buffer::with_capacity(1)).await;
    expect_read_oor("read at the window end", at_end)?;
    say("partcheck: read at the window end answered out-of-range - ok")?;
    probes += 1;

    let (_buf, crossing) = disk::read(&dev, size - 1, buffer::with_capacity(2)).await;
    expect_read_oor("read crossing the window end", crossing)?;
    say("partcheck: read crossing the window end answered out-of-range - ok")?;
    probes += 1;

    let (_buf, write_end) = disk::write(&dev, size, buffer::from_bytes(&[0xEE])).await;
    match write_end {
        Err(disk::WriteError::OutOfRange) => {}
        Ok(_) => {
            return Err(ProgramFailure::UnexpectedSuccess(String::from(
                "write at the window end succeeded",
            )));
        }
        Err(other) => {
            return Err(ProgramFailure::Disk(format!(
                "write at the window end: wanted out-of-range, got {other:?}"
            )));
        }
    }
    say("partcheck: write at the window end answered out-of-range - ok")?;
    probes += 1;

    // A zero-length read at exactly the end is *inside* the contract (the disk.mem
    // convention: a zero-length access at any offset up to the size succeeds).
    let (_buf, zero) = disk::read(&dev, size, buffer::with_capacity(0)).await;
    zero.map_err(|err| {
        ProgramFailure::Disk(format!("zero-length read at the window end: {err:?}"))
    })?;
    say("partcheck: zero-length read at the window end succeeded - ok")?;
    probes += 1;

    disk::flush(&dev)
        .await
        .map_err(|err| ProgramFailure::Disk(format!("flush: {err:?}")))?;
    say("partcheck: flush - ok")?;
    probes += 1;

    Ok(ProgramSuccess::Window(format!(
        "size={size} probes={probes}"
    )))
}

fn expect_read_oor(
    what: &str,
    result: Result<disk::ReadResult, disk::ReadError>,
) -> Result<(), ProgramFailure> {
    match result {
        Err(disk::ReadError::OutOfRange) => Ok(()),
        Ok(_) => Err(ProgramFailure::UnexpectedSuccess(format!(
            "{what} succeeded"
        ))),
        Err(other) => Err(ProgramFailure::Disk(format!(
            "{what}: wanted out-of-range, got {other:?}"
        ))),
    }
}

async fn refusal_mode(needle: Option<String>) -> Result<ProgramSuccess, ProgramFailure> {
    let dev = disk::default();
    let (_buf, result) = disk::read(&dev, 0, buffer::with_capacity(1)).await;
    let error = match result {
        Ok(_) => {
            return Err(ProgramFailure::UnexpectedSuccess(String::from(
                "the first read succeeded where a typed refusal was required",
            )));
        }
        Err(err) => format!("{err:?}"),
    };
    say(&format!("partcheck: refused: {error}"))?;
    if let Some(needle) = needle
        && !error.contains(needle.as_str())
    {
        return Err(ProgramFailure::Mismatch(format!(
            "the refusal text does not contain {needle:?}: {error}"
        )));
    }
    Ok(ProgramSuccess::Refused(error))
}
