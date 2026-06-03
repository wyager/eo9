//! cancelcheck — the executable cancel-mid-flight disk probe.
//!
//! Targets the `eo9-examples:cancelcheck/cancelcheck` world (see `wit/world.wit`).
//! Plan/09 D34 pinned the virtio drain-before-reuse invariant by analysis ("no
//! completion is ever attributed to a request other than the one that produced it")
//! because no expressible consumer could land a cancel mid-flight. This program is
//! that consumer:
//!
//! 1. **Seed**: write two regions with distinct offset-dependent patterns and flush.
//! 2. **Probe**: start a large read of region A, then a small read of region B,
//!    awaiting only B. The driver holds its state in a take/put slot for the duration
//!    of an operation, so B's outcome classifies A precisely: a typed *busy* error
//!    means A is mid-flight **right now** (slot taken, request published, the driver
//!    suspended on its interrupt wait) — and since the classification and the cancel
//!    below happen without yielding back to the executor, A cannot complete in
//!    between: the cancel genuinely lands mid-flight.
//! 3. **Cancel**: drop the still-pending read-A future. The SDK's cancel-on-drop
//!    issues the (blocking) `subtask.cancel`; the driver's task is cancelled at its
//!    interrupt await, its drop-guards restore the slot, and the next operation's
//!    drain-before-reuse must settle the leftover completion.
//! 4. **Verify**: re-read both regions chunk-by-chunk and compare every byte. A
//!    mismatch is exactly the misattribution/torn-read corruption the invariant
//!    forbids, reported with offset and differing byte.
//!
//! `hits=0` is an honest report of the window never opening, and two layers can close
//! it: a backend that completes requests synchronously (usermode `disk.mem`; QEMU TCG
//! virtio-blk without an iothread completes under the queue-notify write), and — the
//! one that holds on metal today — the kernel's `eo9:pci/pci.wait` blocking *host-side*
//! (masked-`wfi` inside the host call; see kernel pci_provider.rs), which halts every
//! task until the interrupt, so nothing can be scheduled between publish and
//! completion. The probe starts hitting the moment that wait suspends the calling task
//! instead (plan/09 D39) — the verification machinery is already in place for that day.

#![no_std]

extern crate alloc;

use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;
use core::future::poll_fn;
use core::pin::pin;
use core::task::Poll;

use eo9_guest::api::disk::disk;
use eo9_guest::{buffer, text};

eo9_guest::bindings!({
    world: "cancelcheck",
    apis: [io, disk, text],
});

/// Region A: the large probe read (wide in-flight window on real devices).
const OFF_A: u64 = 0;
const LEN_A: u64 = 1024 * 1024;

/// Region B: the small concurrent read that classifies A, and the first
/// verification target after a cancel.
const OFF_B: u64 = LEN_A;
const LEN_B: u64 = 64 * 1024;

/// Seeding and verification work in chunks so guest-side allocations stay small;
/// only the probe read moves region A in one piece (and its bytes never cross into
/// guest memory — the buffer is dropped with the cancelled call).
const CHUNK: u64 = 64 * 1024;

/// The deterministic, offset-dependent byte pattern of a region (offset-dependent so
/// a read serviced from the wrong offset can never match).
fn pattern(region_tag: u8, index_in_region: u64) -> u8 {
    (index_in_region.wrapping_mul(0x9E37_79B1) >> 16) as u8 ^ region_tag
}

const TAG_A: u8 = 0xA5;
const TAG_B: u8 = 0x5A;

/// How one probe attempt resolved.
enum Attempt {
    /// B saw the driver's typed busy error while A was still pending: the cancel
    /// landed mid-flight.
    Hit,
    /// A completed before B observed anything: the window never opened.
    Eager,
    /// B returned data (or A was still queued unstarted): no mid-flight cancel.
    DataMiss,
}

eo9_guest::main! {
    async fn main(attempts: u32) -> Result<ProgramSuccess, ProgramFailure> {
        if attempts == 0 {
            return Err(ProgramFailure::BadArguments(String::from(
                "attempts must be at least 1",
            )));
        }

        let dev = disk::default();
        // `disk.virtio` brings the device up lazily on the first *async* operation and
        // its synchronous `size` answers 0 until then — so probe one byte first.
        let (_probe_buf, probe) = disk::read(&dev, 0, buffer::with_capacity(1)).await;
        probe.map_err(|err| ProgramFailure::Disk(format!("bring-up read: {err:?}")))?;
        let size = disk::size(&dev);
        if size < OFF_B + LEN_B {
            return Err(ProgramFailure::TooSmall(format!(
                "the probe needs {} bytes, the disk has {size}",
                OFF_B + LEN_B
            )));
        }

        seed_region(&dev, OFF_A, LEN_A, TAG_A).await?;
        seed_region(&dev, OFF_B, LEN_B, TAG_B).await?;
        disk::flush(&dev)
            .await
            .map_err(|err| ProgramFailure::Disk(format!("flush: {err:?}")))?;
        say("cancelcheck: regions seeded and flushed")?;

        let mut hits: u32 = 0;
        let mut eager: u32 = 0;
        let mut data_miss: u32 = 0;

        for attempt in 0..attempts {
            let resolution = {
                // Issue A first (large), then B (small): polled in this order, the
                // calls queue in this order, so the driver sees A before B.
                let mut read_a = pin!(disk::read(&dev, OFF_A, buffer::with_capacity(LEN_A)));
                let mut read_b = pin!(disk::read(&dev, OFF_B, buffer::with_capacity(LEN_B)));
                let mut a_result: Option<Result<u64, String>> = None;

                // A manual select, awaiting B while keeping A cancellable: poll A
                // (recording a completion), then poll B; finish when B finishes.
                let (_buf_b, b_result) = poll_fn(|cx| {
                    if a_result.is_none()
                        && let Poll::Ready((_buf, result)) = read_a.as_mut().poll(cx)
                    {
                        a_result = Some(
                            result
                                .map(|r| r.bytes_read)
                                .map_err(|err| format!("{err:?}")),
                        );
                    }
                    read_b.as_mut().poll(cx)
                })
                .await;

                match (&a_result, &b_result) {
                    // The driver refused B because A holds the device slot: A is
                    // mid-flight at this very moment, and nothing yields between
                    // here and `read_a`'s drop below — the cancel lands mid-flight.
                    (None, Err(disk::ReadError::Io(message))) if message.contains("busy") => {
                        Attempt::Hit
                    }
                    // Some other disk error on B is a real failure, not a probe
                    // classification.
                    (_, Err(other)) => {
                        return Err(ProgramFailure::Disk(format!(
                            "concurrent read: {other:?}"
                        )));
                    }
                    // A completed before B observed anything: no window.
                    (Some(Ok(_)), Ok(_)) => Attempt::Eager,
                    (Some(Err(message)), _) => {
                        return Err(ProgramFailure::Disk(format!("probe read: {message}")));
                    }
                    // B got data while A never completed: A had not started (or the
                    // backend serviced them out of order) — a miss, and the drop
                    // below exercises cancel-before-start.
                    (None, Ok(_)) => Attempt::DataMiss,
                }
                // `read_a` (and `read_b`, already complete) drop here. A pending A
                // is cancelled by the SDK's cancel-on-drop: the blocking
                // `subtask.cancel`, the driver's CANCELLED handling, and the
                // drop-guard slot restore all run before the verification below.
            };

            match resolution {
                Attempt::Hit => hits += 1,
                Attempt::Eager => eager += 1,
                Attempt::DataMiss => data_miss += 1,
            }

            // The invariant check: after the cancel (or miss), every byte of both
            // regions must read back exactly as seeded — the next operations' drain
            // must have settled any leftover completion of the cancelled request.
            verify_region(&dev, OFF_B, LEN_B, TAG_B, attempt).await?;
            verify_region(&dev, OFF_A, LEN_A, TAG_A, attempt).await?;
        }

        let report = format!("attempts={attempts} hits={hits} eager={eager} data-miss={data_miss}");
        say(&format!("cancelcheck: {report}"))?;
        Ok(ProgramSuccess::Probed(report))
    }
}

/// Write `len` patterned bytes at `offset`, in chunks.
async fn seed_region(
    dev: &disk::DiskImpl,
    offset: u64,
    len: u64,
    tag: u8,
) -> Result<(), ProgramFailure> {
    let mut written = 0u64;
    while written < len {
        let chunk = CHUNK.min(len - written);
        let mut bytes = Vec::with_capacity(chunk as usize);
        for index in 0..chunk {
            bytes.push(pattern(tag, written + index));
        }
        let (_buf, result) = disk::write(dev, offset + written, buffer::from_bytes(&bytes)).await;
        let wrote = result
            .map_err(|err| ProgramFailure::Disk(format!("seed write at {offset}: {err:?}")))?;
        if wrote.bytes_written != chunk {
            return Err(ProgramFailure::Disk(format!(
                "seed write at {}: short write ({} of {chunk})",
                offset + written,
                wrote.bytes_written
            )));
        }
        written += chunk;
    }
    Ok(())
}

/// Read `len` bytes at `offset` in chunks and compare every byte against the pattern.
async fn verify_region(
    dev: &disk::DiskImpl,
    offset: u64,
    len: u64,
    tag: u8,
    attempt: u32,
) -> Result<(), ProgramFailure> {
    let mut checked = 0u64;
    while checked < len {
        let chunk = CHUNK.min(len - checked);
        let (buf, result) = disk::read(dev, offset + checked, buffer::with_capacity(chunk)).await;
        let read = result.map_err(|err| {
            ProgramFailure::Disk(format!(
                "verification read at {} (attempt {attempt}): {err:?}",
                offset + checked
            ))
        })?;
        if read.bytes_read != chunk {
            return Err(ProgramFailure::Corruption(format!(
                "attempt {attempt}: verification read at {} returned {} of {chunk} bytes",
                offset + checked,
                read.bytes_read
            )));
        }
        let bytes = buffer::prefix_to_vec(&buf, read.bytes_read);
        for (index, byte) in bytes.iter().enumerate() {
            let expected = pattern(tag, checked + index as u64);
            if *byte != expected {
                return Err(ProgramFailure::Corruption(format!(
                    "attempt {attempt}: byte at disk offset {} is {byte:#04x}, expected \
                     {expected:#04x} — a cancelled request's completion leaked into a later read",
                    offset + checked + index as u64
                )));
            }
        }
        checked += chunk;
    }
    Ok(())
}

/// One console line; console failures are program failures (the io arm).
fn say(line: &str) -> Result<(), ProgramFailure> {
    text::write_out_line(line).map_err(|err| ProgramFailure::Io(format!("{err:?}")))
}
