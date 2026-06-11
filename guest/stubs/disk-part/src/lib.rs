//! `disk.part` — partition-table middleware: one partition of an underlying device as
//! a windowed block device.
//!
//! Targets the `eo9:disk/part` stub world: imports `eo9:disk/disk` (a whole device) and
//! re-exports `eo9:disk/disk` as a strict window over the partition `configure` selected
//! (1-based, fdisk numbering; the documented default is partition 1, the
//! default-configuration rule of plan/09 Decision 14). Composed as ordinary middleware,
//! the same shape both storage lanes share (docs/board/usb-msd-plan.md §2,
//! docs/board/sdcard-plan.md §B.3):
//!
//!   disk.virtio $ disk.part --partition 1 $ <consumer>          (QEMU / metal)
//!   disk.sdmmc  $ disk.part --partition 2 $ fs.eofs $ program   (the SD card plan)
//!
//! Semantics of the exported window:
//!
//! * offset 0 is the partition's first byte; `size()` is the partition's length; an
//!   access whose range does not lie entirely inside the window refuses `out-of-range`
//!   without touching the device (a zero-length access at any offset up to the size
//!   succeeds, the disk.mem convention); `flush` forwards to the underlying device
//!   (durability is whole-device).
//! * The partition table itself — sector 0 and every EBR of an extended chain — lies
//!   outside every selectable window (`eo9-partwalk` refuses tables where that would
//!   not hold, including the extended *container* as a selection), so the table is
//!   read-only through this component **by construction**, not by check.
//! * The MBR is parsed once, on the first awaited operation (the synchronous `size`
//!   cannot read the device — the disk.virtio wake convention: `size` answers 0 until
//!   a first read/write/flush brings the window up, and consumers like `fs.eofs`
//!   already issue a wake read before trusting `size`). A parse refusal is answered as
//!   this op's typed `io` error, labelled `disk.part: …`, and is re-attempted on the
//!   next operation (an underlying device that was not ready is not wedged forever).
//! * GPT disks (any protective/hybrid 0xEE entry) refuse typed — "not supported in
//!   v1", never a misread of the protective MBR as one giant partition.
//!
//! Errors are typed, never traps; underlying errors pass through verbatim (a driver's
//! labelled message stays attributable at the console).

#![no_std]

extern crate alloc;

use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;

use eo9_guest::provider::ProviderState;
use eo9_partwalk::{PartitionTable, SECTOR_SIZE, Step, Walker};

wit_bindgen::generate!({
    world: "part",
    path: "../../../wit/disk",
    // Pull in bindings for eo9:io/buffers, which the disk interfaces use but the world
    // does not name directly.
    generate_all,
});

use eo9::disk::disk as underlying;
use exports::eo9::disk::disk::{self, Buffer, ReadError, ReadResult, WriteError, WriteResult};
use exports::eo9::disk::part_config;
use exports::eo9::disk::types;

eo9_guest::manual! {
    name: "disk.part",
    synopsis: "one MBR partition of an underlying disk as a windowed eo9:disk",
    description: [
        "Partition-table middleware: imports a whole eo9:disk, parses its MBR on first use",
        "(primaries and classic extended chains; GPT answers a typed refusal, never a misread),",
        "and re-exports the selected partition as a strict window — offset 0 is the partition",
        "start, size is the partition length, anything beyond refuses out-of-range. The table",
        "itself (the MBR and every EBR) lies outside every window, so it is read-only through",
        "this component by construction. Invalid tables (missing signature, entries past the",
        "device end, overlaps, chain cycles) refuse typed rather than being guessed at.",
    ],
    args: [
        { name: "partition", ty: "u32", optional,
          doc: "which partition to window, 1-based like fdisk (primaries 1-4, logicals 5+); default 1" },
    ],
    examples: [
        { line: "disk.virtio $ disk.part --partition 1 $ partcheck --mode window",
          doc: "probe the first partition's window over a QEMU virtio disk" },
        { line: "disk.virtio $ disk.part --partition 2 $ fs.eofs $ readwrite f contents",
          doc: "a filesystem on partition 2, the boot partition untouchable by construction" },
    ],
    see_also: "disk.virtio, disk.mem, fs.eofs, partcheck",
}

/// The provider's state: which partition is selected (bound by `configure`, default 1)
/// and the resolved window once the first awaited operation has parsed the table.
struct State {
    /// 1-based fdisk partition number.
    selected: u32,
    /// `Some` once the MBR has been parsed and the partition found. Refusals are NOT
    /// cached: a failed parse is re-attempted by the next operation, so a slow
    /// underlying device cannot wedge the window forever.
    window: Option<Window>,
}

/// The selected partition's span on the underlying device, in bytes.
#[derive(Clone, Copy)]
struct Window {
    start: u64,
    len: u64,
}

static STATE: ProviderState<State> = ProviderState::new();

/// The documented default selection (an unconfigured `disk.part`): partition 1.
const DEFAULT_PARTITION: u32 = 1;

/// Run `f` over the state, self-initializing the documented default on first use so the
/// provider never traps when composed without `configure`.
fn with_state<R>(f: impl FnOnce(&mut State) -> R) -> R {
    if !STATE.is_set() {
        STATE.set(State {
            selected: DEFAULT_PARTITION,
            window: None,
        });
    }
    STATE.with(f)
}

/// Read one 512-byte table sector from the underlying device. Any failure is rendered
/// as the operator-facing refusal text (the caller wraps it in this op's `io` error).
async fn read_table_sector(dev: &underlying::DiskImpl, lba: u64) -> Result<Vec<u8>, String> {
    let dst = Buffer::new(SECTOR_SIZE);
    let (dst, result) = underlying::read(dev, lba * SECTOR_SIZE, dst).await;
    let read = result
        .map_err(|err| format!("disk.part: reading the partition table at LBA {lba}: {err:?}"))?;
    if read.bytes_read < SECTOR_SIZE {
        return Err(format!(
            "disk.part: short read of the partition table at LBA {lba} ({} of {SECTOR_SIZE} bytes)",
            read.bytes_read
        ));
    }
    Ok(dst.read(0, SECTOR_SIZE))
}

/// The window, parsing the partition table on first use. Every refusal is the full
/// `disk.part: …` text the operation surfaces as its typed `io` error.
async fn ensure_window() -> Result<Window, String> {
    if let Some(window) = with_state(|state| state.window) {
        return Ok(window);
    }
    let selected = with_state(|state| state.selected);

    let dev = underlying::default();
    // Sector 0 first: the read doubles as the wake of a lazily-brought-up underlying
    // driver (disk.virtio answers size 0 until a first awaited op), so the size query
    // below sees the real capacity.
    let sector0 = read_table_sector(&dev, 0).await?;
    let device_size = underlying::size(&dev);

    let mut walker = Walker::new(device_size);
    let table_err =
        |err: eo9_partwalk::TableError| format!("disk.part: invalid partition table: {err}");
    let mut step = walker.start(&sector0).map_err(table_err)?;
    let table: PartitionTable = loop {
        match step {
            Step::Done(table) => break table,
            Step::Need { lba } => {
                let sector = read_table_sector(&dev, lba).await?;
                step = walker.feed(&sector).map_err(table_err)?;
            }
        }
    };

    let partition = table
        .select(selected)
        .map_err(|err| format!("disk.part: {err}"))?;
    let window = Window {
        start: partition.start_bytes(),
        len: partition.len_bytes(),
    };
    with_state(|state| state.window = Some(window));
    Ok(window)
}

/// Translate an access against the window: `Ok(absolute offset)` when
/// `offset .. offset+len` lies entirely inside it (zero-length accesses are valid up
/// to and including the window's end), `Err(())` = out-of-range.
fn translate(window: Window, offset: u64, len: u64) -> Result<u64, ()> {
    if offset > window.len || len > window.len - offset {
        return Err(());
    }
    Ok(window.start + offset)
}

// Map the underlying provider's (structurally identical) results and errors onto the
// exported types, verbatim — the window adds no vocabulary of its own beyond the
// out-of-range and `disk.part: …` io refusals issued above.

fn map_read_error(error: underlying::ReadError) -> ReadError {
    match error {
        underlying::ReadError::NotFound => ReadError::NotFound,
        underlying::ReadError::Io(message) => ReadError::Io(message),
        underlying::ReadError::OutOfRange => ReadError::OutOfRange,
    }
}

fn map_write_error(error: underlying::WriteError) -> WriteError {
    match error {
        underlying::WriteError::Io(message) => WriteError::Io(message),
        underlying::WriteError::OutOfRange => WriteError::OutOfRange,
        underlying::WriteError::ReadOnly => WriteError::ReadOnly,
    }
}

/// The `disk.part` provider.
struct Stub;

/// The root-handle resource: a token for the windowed view (the world exports its own
/// `types` instance, so this resource is this component's — one consistent identity
/// for a consumer wiring `disk` + `types` from the sealed chain).
struct PartDisk;

impl types::Guest for Stub {
    type DiskImpl = PartDisk;
}

impl types::GuestDiskImpl for PartDisk {}

impl part_config::Guest for Stub {
    fn configure(partition: u32) -> Result<types::DiskImpl, String> {
        if partition == 0 {
            return Err(String::from(
                "disk.part: partition numbers are 1-based (fdisk numbering); 0 is not valid",
            ));
        }
        STATE.set(State {
            selected: partition,
            window: None,
        });
        Ok(types::DiskImpl::new(PartDisk))
    }
}

impl disk::Guest for Stub {
    fn default() -> types::DiskImpl {
        types::DiskImpl::new(PartDisk)
    }

    fn size(_dev: disk::DiskImplBorrow<'_>) -> u64 {
        // `size` is a synchronous query and the table parse awaits, so it reports the
        // window only once a first awaited operation has bound it — before that it
        // answers 0 rather than trapping (the disk.virtio wake convention; `fs.eofs`
        // issues one read to wake the chain, then asks again).
        with_state(|state| state.window).map_or(0, |window| window.len)
    }

    async fn read(
        _dev: disk::DiskImplBorrow<'_>,
        offset: u64,
        dst: Buffer,
    ) -> (Buffer, Result<ReadResult, ReadError>) {
        let window = match ensure_window().await {
            Ok(window) => window,
            Err(message) => return (dst, Err(ReadError::Io(message))),
        };
        let Ok(absolute) = translate(window, offset, dst.len()) else {
            return (dst, Err(ReadError::OutOfRange));
        };
        // Underlying errors pass through verbatim — same vocabulary, and a driver's
        // labelled message stays attributable.
        let (dst, result) = underlying::read(&underlying::default(), absolute, dst).await;
        (
            dst,
            result
                .map(|read| ReadResult {
                    bytes_read: read.bytes_read,
                })
                .map_err(map_read_error),
        )
    }

    async fn write(
        _dev: disk::DiskImplBorrow<'_>,
        offset: u64,
        src: Buffer,
    ) -> (Buffer, Result<WriteResult, WriteError>) {
        let window = match ensure_window().await {
            Ok(window) => window,
            Err(message) => return (src, Err(WriteError::Io(message))),
        };
        let Ok(absolute) = translate(window, offset, src.len()) else {
            return (src, Err(WriteError::OutOfRange));
        };
        let (src, result) = underlying::write(&underlying::default(), absolute, src).await;
        (
            src,
            result
                .map(|written| WriteResult {
                    bytes_written: written.bytes_written,
                })
                .map_err(map_write_error),
        )
    }

    async fn flush(_dev: disk::DiskImplBorrow<'_>) -> Result<(), WriteError> {
        // Durability is whole-device, so this forwards — but only behind a bound
        // window, so a consumer that only ever flushes still gets the typed table
        // refusal (a GPT disk, say) instead of a silent success.
        if let Err(message) = ensure_window().await {
            return Err(WriteError::Io(message));
        }
        underlying::flush(&underlying::default())
            .await
            .map_err(map_write_error)
    }
}

export!(Stub);
