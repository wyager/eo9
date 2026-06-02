//! `fs.eofs` — Eo9's native filesystem as a provider component (plan/14-eofs.md, M2).
//!
//! Targets the crate-local `eo9:fs-eofs/eofs` world: imports a raw block device
//! (`eo9:disk/disk`) and exports `eo9:fs/fs` backed by the `eo9-eofs` engine — the same
//! copy-on-write, Merkle-hashed, lz4-compressed, snapshotting filesystem the host tests
//! and the kernel use. All I/O goes through the imported disk capability, so the
//! identical component runs over `disk.mem` in usermode today and over file-backed or
//! virtio disks later: `disk.mem $ fs.eofs $ program`.
//!
//! Behaviour and defaults (the option-C default-configuration rule, plan/09 Decision 14 —
//! there is no configure interface):
//!
//! * **First use mounts the disk.** If either uberblock slot carries the eofs magic the
//!   image is mounted; a blank device (no magic, and all zero everywhere a common foreign
//!   format would leave its mark — see [`eo9_eofs::probe_async`]) is formatted in place
//!   with the default options (4 KiB blocks, lz4 on). A device that has the magic but
//!   fails to mount is *never* reformatted — the error is reported instead, so corruption
//!   can't silently become data loss; a device holding foreign data is refused outright.
//! * **Every completed mutating operation commits; failed ones roll back.** `write`,
//!   `create-directory`, and `remove` each end with an eofs commit (root flip), so
//!   completed operations are durable on the disk and crash consistency is the engine's
//!   by construction. The one deliberate exception is `open` with TRUNCATE on an existing
//!   file: the truncation is *staged* but not committed, so it becomes durable together
//!   with the write that follows it — a rewrite is atomic, and a failed or abandoned
//!   rewrite leaves the file's previous contents untouched on disk (study 07, S7-4). Any
//!   operation that fails discards every uncommitted change (`AsyncEofs::rollback`), so
//!   half-applied state is never published. This trades write amplification for
//!   simplicity — fine for the MVP, batching is a later refinement.
//! * **Paths** are `/`-separated; empty and `.` segments are ignored and `..` steps up
//!   one level (never above the root) — the same rules `fs.memfs` documents. The root is
//!   a directory that cannot be opened, removed, or recreated.
//! * **Open files are path references**, not pinned objects: removing a file while a
//!   handle is open makes further reads/writes through that handle fail with `not-found`
//!   (unlike memfs's unlink semantics). `open-exec` snapshots the contents at open time —
//!   honest immutability by copy; pinning the Merkle object instead is a later
//!   refinement (the hash is already content-stable).
//! * **The disk import is genuinely awaited** (SPEC, "Boundaries are honestly async"):
//!   the engine core runs over an [`eo9_eofs::AsyncBlockDevice`] whose operations await
//!   the imported `eo9:disk` calls, so a disk that parks — a deferred guest chain, an
//!   interrupt-paced driver — suspends the filesystem operation and resumes it when the
//!   device completes, instead of failing with the old "device suspended" error. The
//!   await's *bound* is the device layer's own (every shipped disk bounds its waits:
//!   `disk.virtio`'s interrupt/poll limits, the host providers' eager completion), so a
//!   filesystem operation can wait no longer than its disk is allowed to — recorded as
//!   the deadline story in plan/14.
//! * **Operations are serialized.** The engine is a single mutable state; while one
//!   operation is awaiting the disk, a concurrently delivered operation fails with a
//!   typed `io` ("the filesystem is busy") rather than corrupting state or trapping.
//!   Every shipped consumer issues filesystem calls sequentially, so this surfaces only
//!   under deliberately concurrent callers; a queueing upgrade is a recorded refinement.
//! * The device size comes from the disk API's `size` query (read once per mount), and
//!   the engine's commit-boundary flushes call straight through to the disk's `flush`,
//!   so durability is the underlying device's (fsync for a file-backed disk, a virtio
//!   cache flush for `disk.virtio`, a no-op for purely in-memory devices).

#![no_std]

extern crate alloc;

use alloc::boxed::Box;
use alloc::string::String;
use alloc::vec;
use alloc::vec::Vec;
use core::cell::RefCell;

use eo9_eofs::{AsyncBlockDevice, AsyncEofs, DeviceError, FormatOptions};
// Linked for its runtime contract (allocator, panic handler, diagnostics import), not
// for any item: the provider state here is the take/put `Slot`, not `ProviderState`.
use eo9_guest as _;

wit_bindgen::generate!({
    world: "eofs",
    path: "wit",
    // Pull in bindings for eo9:disk/types and eo9:io/buffers, which the imported and
    // exported interfaces use but the world does not name directly.
    generate_all,
});

use eo9::disk::disk;
use exports::eo9::fs::fs::{
    self, Buffer, FsError, NodeKind, NodeStat, OpenFlags, ReadResult, WriteResult,
};

// --- the imported disk as an eofs async block device --------------------------------------

/// The imported `eo9:disk` capability seen as an eofs [`AsyncBlockDevice`].
struct DiskDevice {
    handle: disk::DiskImpl,
    size: u64,
}

impl DiskDevice {
    /// Take the disk's root handle and read its size from the disk API.
    fn new() -> DiskDevice {
        let handle = disk::default();
        let size = disk::size(&handle);
        DiskDevice { handle, size }
    }
}

impl AsyncBlockDevice for DiskDevice {
    fn size(&self) -> u64 {
        self.size
    }

    async fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<(), DeviceError> {
        if buf.is_empty() {
            return Ok(());
        }
        let len = buf.len() as u64;
        let dst = Buffer::new(len);
        let (dst, result) = disk::read(&self.handle, offset, dst).await;
        // Preserve the device's own error text (a driver's typed, labelled message) so a
        // real hardware failure stays attributable at the console (study 09 finding 2).
        let read = result.map_err(|err| match err {
            disk::ReadError::OutOfRange => DeviceError::OutOfRange,
            disk::ReadError::Io(message) => DeviceError::IoNamed(message),
            disk::ReadError::NotFound => DeviceError::Io,
        })?;
        if read.bytes_read < len {
            // eofs never issues reads past the end it knows about; a short read is a
            // device failure, not end-of-device.
            return Err(DeviceError::IoNamed(alloc::format!(
                "short read from the device ({} of {len} bytes)",
                read.bytes_read
            )));
        }
        buf.copy_from_slice(&dst.read(0, len));
        Ok(())
    }

    async fn write_at(&mut self, offset: u64, data: &[u8]) -> Result<(), DeviceError> {
        if data.is_empty() {
            return Ok(());
        }
        let len = data.len() as u64;
        let src = Buffer::new(len);
        src.write(0, data);
        let (_src, result) = disk::write(&self.handle, offset, src).await;
        let written = result.map_err(|err| match err {
            disk::WriteError::OutOfRange => DeviceError::OutOfRange,
            disk::WriteError::Io(message) => DeviceError::IoNamed(message),
            disk::WriteError::ReadOnly => {
                DeviceError::IoNamed(String::from("the device is read-only"))
            }
        })?;
        if written.bytes_written < len {
            return Err(DeviceError::IoNamed(alloc::format!(
                "short write to the device ({} of {len} bytes)",
                written.bytes_written
            )));
        }
        Ok(())
    }

    async fn flush(&mut self) -> Result<(), DeviceError> {
        // The engine calls this at every commit boundary (before and after the uberblock
        // write), so durability rides on the imported disk's own flush.
        disk::flush(&self.handle).await.map_err(|err| match err {
            disk::WriteError::OutOfRange => DeviceError::OutOfRange,
            disk::WriteError::Io(message) => DeviceError::IoNamed(message),
            disk::WriteError::ReadOnly => {
                DeviceError::IoNamed(String::from("the device is read-only"))
            }
        })
    }
}

// --- state: the mounted engine, safe across awaits ----------------------------------------

/// The provider's state slot. Unlike `ProviderState`, whose borrow must never be held
/// across an await, operations here *do* await mid-operation (the disk calls), so the
/// engine is **taken out** of the slot for the duration of an operation and put back
/// afterwards. A second activation arriving while the slot is `Busy` gets a typed error —
/// never a `RefCell` re-borrow trap.
enum Slot {
    /// Not mounted yet (first use mounts or formats).
    Empty,
    /// An operation is in flight (the engine is on that operation's stack).
    Busy,
    /// Mounted and idle. Boxed: the engine is ~400 bytes and the other variants are
    /// empty (clippy `large_enum_variant`); take/put then moves one pointer.
    Ready(Box<AsyncEofs<DiskDevice>>),
}

struct FsState {
    inner: RefCell<Slot>,
}

// SAFETY: guest components run single-threaded (shared-memory threading is an
// ungranted capability — see SPEC "Execution APIs"); `Sync` is only needed for the
// `static`. Re-entrant access is handled by the `Busy` state, not by panicking.
unsafe impl Sync for FsState {}

static STATE: FsState = FsState {
    inner: RefCell::new(Slot::Empty),
};

impl FsState {
    /// Take the engine for one operation. `Ok(Some)` = mounted engine; `Ok(None)` = first
    /// use (the slot is now `Busy`; mount and then [`put`](Self::put) or
    /// [`clear`](Self::clear)); `Err` = another operation is in flight.
    fn take(&self) -> Result<Option<Box<AsyncEofs<DiskDevice>>>, FsError> {
        let mut slot = self.inner.borrow_mut();
        match core::mem::replace(&mut *slot, Slot::Busy) {
            Slot::Ready(eofs) => Ok(Some(eofs)),
            Slot::Empty => Ok(None),
            Slot::Busy => Err(FsError::Io(String::from(
                "the filesystem is busy with a concurrent operation; \
                 issue filesystem calls sequentially",
            ))),
        }
    }

    /// Put the engine back after an operation.
    fn put(&self, eofs: Box<AsyncEofs<DiskDevice>>) {
        *self.inner.borrow_mut() = Slot::Ready(eofs);
    }

    /// First-use mount failed: return the slot to `Empty` so a later call can retry.
    fn clear(&self) {
        *self.inner.borrow_mut() = Slot::Empty;
    }
}

/// Mount the imported disk, formatting it first if — and only if — it is blank.
///
/// "Blank" means probed as [`eo9_eofs::ImageState::Blank`]: no eofs filesystem AND the
/// spans where any common format would leave its mark — the leading megabyte, the trailing
/// 64 KiB, or the whole device when it is small — are all zero. A device holding anybody
/// else's data (an ext4 image, a btrfs volume, a file pointed at by mistake) is refused,
/// never formatted over — destroying foreign data is something only an explicit, forced
/// `mkfs` may do (study 07, S7-2). A device with eofs remains that no longer mount is also
/// never reformatted; its mount error is reported instead.
async fn mount_or_format() -> Result<AsyncEofs<DiskDevice>, FsError> {
    let mut device = DiskDevice::new();
    // A device that reports size 0 has not answered yet: either it cannot be probed at
    // all, or it is a driver that brings its hardware up lazily on the first *awaited*
    // operation (`disk.virtio` — its `size` is a synchronous query and bring-up awaits).
    // Issue one read: a failure carries the device's *real* typed error — e.g. "no
    // virtio-blk function is visible …" — instead of burying the cause under a
    // format-options message (study 09 finding 3); a success means the read woke the
    // device, so re-ask for the size and carry on mounting.
    if device.size == 0 {
        let mut probe = [0u8; 1];
        match device.read_at(0, &mut probe).await {
            Err(error) => return Err(device_error(error)),
            Ok(()) => {
                device.size = disk::size(&device.handle);
                if device.size == 0 {
                    return Err(FsError::Io(String::from(
                        "the disk reports a size of 0 bytes",
                    )));
                }
            }
        }
    }
    match eo9_eofs::probe_async(&device).await.map_err(map_error)? {
        eo9_eofs::ImageState::Eofs { .. } | eo9_eofs::ImageState::Unmountable => {
            AsyncEofs::mount(device).await.map_err(map_error)
        }
        eo9_eofs::ImageState::Blank => AsyncEofs::format(device, &FormatOptions::default())
            .await
            .map_err(map_error),
        eo9_eofs::ImageState::Foreign => Err(FsError::Io(String::from(
            "the disk holds data that is not an eofs filesystem; refusing to format over it. If that data is expendable, format the device explicitly: `eo9 mkfs.eofs <image> --force`",
        ))),
    }
}

/// Run `f` over the mounted filesystem, mounting (or formatting a blank disk) on first
/// use — the documented default behaviour, so the unconfigured provider never traps.
async fn with_fs<R>(
    f: impl AsyncFnOnce(&mut AsyncEofs<DiskDevice>) -> Result<R, FsError>,
) -> Result<R, FsError> {
    let mut eofs = match STATE.take()? {
        Some(eofs) => eofs,
        None => match mount_or_format().await {
            Ok(eofs) => Box::new(eofs),
            Err(error) => {
                STATE.clear();
                return Err(error);
            }
        },
    };
    let result = f(&mut eofs).await;
    STATE.put(eofs);
    result
}

/// Run a *mutating* operation over the mounted filesystem with rewrite atomicity and
/// space reclamation:
///
/// * If the operation runs out of space, the copy-on-write garbage no root references any
///   more — superseded copies of rewritten files, the contents of removed files — is
///   reclaimed (`AsyncEofs::gc`) and the operation retried once. This is what keeps an
///   image usable forever instead of bricking once its append frontier reaches the end
///   (study 07, S7-3): `rm` frees space, rewrites reuse the space of the copies they
///   replace.
/// * If the operation still fails, every uncommitted change is discarded
///   (`AsyncEofs::rollback`), so a half-applied multi-step change — a truncation whose
///   rewrite never landed, a partially built directory edit — can never be published by a
///   later commit. The committed, on-disk state only ever moves from one fully-applied
///   operation to the next.
///
/// The operation must be re-runnable from any pending state it may itself have
/// half-applied (the `open` truncate path tolerates an already-removed file for this
/// reason); see the per-operation comments.
async fn mutate<R>(
    f: impl AsyncFn(&mut AsyncEofs<DiskDevice>) -> Result<R, eo9_eofs::FsError>,
) -> Result<R, FsError> {
    with_fs(async |eofs| {
        let result = match f(eofs).await {
            Err(eo9_eofs::FsError::NoSpace) => {
                // Reclaim everything unreachable (the failed attempt's own orphaned
                // blocks included — gc walks the committed and pending roots, and
                // whatever the failure left behind is referenced by neither) and try
                // again. The walk reads every live block, so it only runs when an
                // allocation has actually failed, never on the fast path.
                match eofs.gc().await {
                    Ok(_) => f(eofs).await,
                    // gc itself failing (a corrupt image) is more important to report
                    // than the space condition that triggered it.
                    Err(error) => Err(error),
                }
            }
            other => other,
        };
        match result {
            Ok(value) => Ok(value),
            Err(error) => {
                // Discard whatever the failed operation half-applied (and, deliberately,
                // any earlier uncommitted state it was meant to complete — e.g. the
                // pending truncate of a rewrite whose write just failed). What was last
                // committed is exactly what a remount sees, so old content is never lost
                // to a failure.
                eofs.rollback();
                Err(map_error(error))
            }
        }
    })
    .await
}

fn device_error(error: DeviceError) -> FsError {
    FsError::Io(alloc::format!("device error: {error}"))
}

/// Map the engine's error type onto the `eo9:fs` error variants.
///
/// Integrity failures (checksum mismatches, corrupt structures) lead with a fixed
/// "integrity check failed:" prefix so they are distinguishable from ordinary I/O
/// failures even though `eo9:fs` has no dedicated corruption variant yet — a flaky cable
/// and rotting media must not read the same (study 07, S7-5). The complete fix is a WIT
/// addition (`integrity(string)` in `fs-error`), recorded in plan/14.
fn map_error(error: eo9_eofs::FsError) -> FsError {
    match error {
        eo9_eofs::FsError::NotFound => FsError::NotFound,
        eo9_eofs::FsError::AlreadyExists => FsError::AlreadyExists,
        eo9_eofs::FsError::NotADirectory => FsError::NotADirectory,
        eo9_eofs::FsError::IsADirectory => FsError::IsADirectory,
        eo9_eofs::FsError::NoSpace => FsError::NoSpace,
        eo9_eofs::FsError::DirectoryNotEmpty => FsError::Io(String::from("directory is not empty")),
        eo9_eofs::FsError::ChecksumMismatch => FsError::Io(String::from(
            "integrity check failed: block checksum mismatch (the stored data does not \
             match its recorded hash; the device is corrupted at this location)",
        )),
        eo9_eofs::FsError::Corrupt(what) => FsError::Io(alloc::format!(
            "integrity check failed: corrupt filesystem structure ({what})"
        )),
        other => FsError::Io(alloc::format!("{other}")),
    }
}

/// Resolve `path` into the canonical form eofs takes: `/`-joined segments with empty and
/// `.` segments dropped and `..` stepping up (never above the root). The empty string is
/// the root.
fn canonical(path: &str) -> String {
    let mut segments: Vec<&str> = Vec::new();
    for segment in path.split('/') {
        match segment {
            "" | "." => {}
            ".." => {
                segments.pop();
            }
            name => segments.push(name),
        }
    }
    let mut out = String::new();
    for segment in segments {
        out.push('/');
        out.push_str(segment);
    }
    out
}

/// Copy `dst.len()` bytes (or whatever is available) from `data` at `offset` into `dst`.
fn read_slice_at(data: &[u8], offset: u64, dst: &Buffer) -> ReadResult {
    let available = match usize::try_from(offset) {
        Ok(offset) if offset < data.len() => &data[offset..],
        _ => &[],
    };
    let wanted = usize::try_from(dst.len()).unwrap_or(usize::MAX);
    let chunk = &available[..usize::min(wanted, available.len())];
    if !chunk.is_empty() {
        dst.write(0, chunk);
    }
    ReadResult {
        bytes_read: chunk.len() as u64,
    }
}

// --- the provider ------------------------------------------------------------------------

/// The `fs.eofs` provider.
struct Stub;

/// The root-handle resource: a token referring to the mounted filesystem.
struct EofsRoot;

/// An open file: a canonical path into the mounted filesystem, the write permission
/// captured at open time, and whether a TRUNCATE request is still pending.
///
/// Truncation is applied *lazily*: `open` only records it, and the first `write` through
/// the handle replaces the file (remove + recreate + write + commit) as one atomic
/// engine transaction. Until then the on-disk file is untouched, so an abandoned or
/// failed rewrite can never lose the previous contents (study 07, S7-4). Reads through a
/// truncate-pending handle see an empty file, matching what the truncation promised.
struct OpenFile {
    path: String,
    writable: bool,
    truncate_pending: core::cell::Cell<bool>,
}

/// An immutable execution handle: a snapshot of the file's contents at open-exec time.
struct ExecSnapshot {
    bytes: Vec<u8>,
}

impl fs::GuestFsImpl for EofsRoot {}
impl fs::GuestFile for OpenFile {}
impl fs::GuestImmutableHandle for ExecSnapshot {}

impl fs::Guest for Stub {
    type FsImpl = EofsRoot;
    type File = OpenFile;
    type ImmutableHandle = ExecSnapshot;

    fn default() -> fs::FsImpl {
        fs::FsImpl::new(EofsRoot)
    }

    async fn open(
        _fs: fs::FsImplBorrow<'_>,
        path: String,
        options: OpenFlags,
    ) -> Result<fs::File, FsError> {
        let path = canonical(&path);
        if path.is_empty() {
            return Err(FsError::IsADirectory);
        }
        let create = options.contains(OpenFlags::CREATE);
        let truncate = options.contains(OpenFlags::TRUNCATE);
        let mut truncate_pending = false;
        mutate(async |eofs| {
            match eofs.stat(&path).await {
                Ok(stat) => {
                    if stat.kind == eo9_eofs::NodeKind::Directory {
                        return Err(eo9_eofs::FsError::IsADirectory);
                    }
                }
                Err(eo9_eofs::FsError::NotFound) if create => {
                    // Creating a brand-new (empty) file destroys nothing; commit it so a
                    // bare `touch` is durable on its own.
                    eofs.create_file(&path).await?;
                    eofs.commit().await?;
                }
                Err(error) => return Err(error),
            }
            Ok(())
        })
        .await?;
        // Truncation of an existing, non-empty file is recorded on the handle and applied
        // by the first write through it — the whole rewrite (remove + recreate + write)
        // commits as one transaction, so the previous contents survive anything short of
        // a completed replacement (study 07, S7-4).
        if truncate {
            truncate_pending = with_fs(async |eofs| {
                Ok(matches!(eofs.stat(&path).await, Ok(stat) if stat.size > 0
                    && stat.kind == eo9_eofs::NodeKind::File))
            })
            .await?;
        }
        Ok(fs::File::new(OpenFile {
            path,
            writable: options.contains(OpenFlags::WRITE),
            truncate_pending: core::cell::Cell::new(truncate_pending),
        }))
    }

    async fn open_exec(
        _fs: fs::FsImplBorrow<'_>,
        path: String,
    ) -> Result<fs::ImmutableHandle, FsError> {
        let path = canonical(&path);
        if path.is_empty() {
            return Err(FsError::IsADirectory);
        }
        let bytes = with_fs(async |eofs| {
            let stat = eofs.stat(&path).await.map_err(map_error)?;
            if stat.kind == eo9_eofs::NodeKind::Directory {
                return Err(FsError::IsADirectory);
            }
            let size = usize::try_from(stat.size)
                .map_err(|_| FsError::Io(String::from("file too large for open-exec")))?;
            let mut bytes = vec![0u8; size];
            let read = eofs.read(&path, 0, &mut bytes).await.map_err(map_error)?;
            bytes.truncate(read);
            Ok(bytes)
        })
        .await?;
        // eofs is copy-on-write, so the contents behind the snapshot can never be
        // overwritten in place; copying here keeps the handle simple (pinning the Merkle
        // object instead is a recorded refinement).
        Ok(fs::ImmutableHandle::new(ExecSnapshot { bytes }))
    }

    async fn list_directory(
        _fs: fs::FsImplBorrow<'_>,
        path: String,
    ) -> Result<Vec<String>, FsError> {
        let path = canonical(&path);
        with_fs(async |eofs| eofs.list(&path).await.map_err(map_error)).await
    }

    async fn stat(_fs: fs::FsImplBorrow<'_>, path: String) -> Result<NodeStat, FsError> {
        let path = canonical(&path);
        with_fs(async |eofs| {
            let stat = eofs.stat(&path).await.map_err(map_error)?;
            Ok(NodeStat {
                kind: match stat.kind {
                    eo9_eofs::NodeKind::File => NodeKind::File,
                    eo9_eofs::NodeKind::Directory => NodeKind::Directory,
                },
                size: match stat.kind {
                    eo9_eofs::NodeKind::File => stat.size,
                    // The engine reports a directory's serialized size; the API promises 0.
                    eo9_eofs::NodeKind::Directory => 0,
                },
            })
        })
        .await
    }

    async fn create_directory(_fs: fs::FsImplBorrow<'_>, path: String) -> Result<(), FsError> {
        let path = canonical(&path);
        if path.is_empty() {
            return Err(FsError::AlreadyExists);
        }
        mutate(async |eofs| {
            eofs.mkdir(&path).await?;
            eofs.commit().await?;
            Ok(())
        })
        .await
    }

    async fn remove(_fs: fs::FsImplBorrow<'_>, path: String) -> Result<(), FsError> {
        let path = canonical(&path);
        if path.is_empty() {
            return Err(FsError::Io(String::from(
                "cannot remove the root directory",
            )));
        }
        mutate(async |eofs| {
            eofs.remove(&path).await?;
            eofs.commit().await?;
            Ok(())
        })
        .await
    }

    async fn read(
        f: fs::FileBorrow<'_>,
        offset: u64,
        dst: Buffer,
    ) -> (Buffer, Result<ReadResult, FsError>) {
        let file = f.get::<OpenFile>();
        // A handle whose truncation has not been written yet reads as the empty file the
        // truncation promised; the on-disk contents are still the previous ones.
        if file.truncate_pending.get() {
            return (dst, Ok(ReadResult { bytes_read: 0 }));
        }
        let wanted = usize::try_from(dst.len()).unwrap_or(usize::MAX);
        let result = with_fs(async |eofs| {
            let mut bytes = vec![0u8; wanted];
            let read = eofs
                .read(&file.path, offset, &mut bytes)
                .await
                .map_err(map_error)?;
            if read > 0 {
                dst.write(0, &bytes[..read]);
            }
            Ok(ReadResult {
                bytes_read: read as u64,
            })
        })
        .await;
        (dst, result)
    }

    async fn write(
        f: fs::FileBorrow<'_>,
        offset: u64,
        src: Buffer,
    ) -> (Buffer, Result<WriteResult, FsError>) {
        let file = f.get::<OpenFile>();
        if !file.writable {
            return (
                src,
                Err(FsError::Io(String::from("file is not open for writing"))),
            );
        }
        let len = src.len();
        // Copy out of the buffer before entering the engine, so no buffer call happens
        // while the filesystem state is borrowed.
        let bytes = if len == 0 {
            Vec::new()
        } else {
            src.read(0, len)
        };
        let path = file.path.clone();
        let truncating = file.truncate_pending.get();
        let result = mutate(async |eofs| {
            if truncating {
                // The pending truncation and this write commit as one transaction: the
                // file is replaced wholesale (eofs has no truncate primitive, so the
                // replacement is remove + recreate). Every step is guarded so the closure
                // can be re-run from scratch by the NoSpace retry.
                if eofs.stat(&path).await.is_ok() {
                    eofs.remove(&path).await?;
                }
                eofs.create_file(&path).await?;
            }
            eofs.write(&path, offset, &bytes).await?;
            eofs.commit().await?;
            Ok(WriteResult { bytes_written: len })
        })
        .await;
        if result.is_ok() {
            file.truncate_pending.set(false);
        }
        (src, result)
    }

    fn exec_size(h: fs::ImmutableHandleBorrow<'_>) -> u64 {
        h.get::<ExecSnapshot>().bytes.len() as u64
    }

    async fn exec_read(
        h: fs::ImmutableHandleBorrow<'_>,
        offset: u64,
        dst: Buffer,
    ) -> (Buffer, Result<ReadResult, FsError>) {
        let result = read_slice_at(&h.get::<ExecSnapshot>().bytes, offset, &dst);
        (dst, Ok(result))
    }
}

export!(Stub);
