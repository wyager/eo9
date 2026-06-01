//! `fs.eofs` — Eo9's native filesystem as a provider component (plan/14-eofs.md, M2).
//!
//! Targets the crate-local `eo9:fs-eofs/eofs` world: imports a raw block device
//! (`eo9:disk/disk`) and exports `eo9:fs/fs` backed by the `eo9-eofs` engine — the same
//! copy-on-write, Merkle-hashed, lz4-compressed, snapshotting filesystem the host tests
//! and (later) the kernel use. All I/O goes through the imported disk capability, so the
//! identical component runs over `disk.mem` in usermode today and over file-backed or
//! virtio disks later: `disk.mem $ fs.eofs $ program`.
//!
//! Behaviour and defaults (the option-C default-configuration rule, plan/09 Decision 14 —
//! there is no configure interface):
//!
//! * **First use mounts the disk.** If either uberblock slot carries the eofs magic the
//!   image is mounted; a blank device (no magic, and all zero everywhere a common foreign
//!   format would leave its mark — see [`eo9_eofs::probe`]) is formatted in place with
//!   the default options (4 KiB blocks, lz4 on). A device that has the magic but fails to
//!   mount is *never* reformatted — the error is reported instead, so corruption can't
//!   silently become data loss; a device holding foreign data is refused outright.
//! * **Every completed mutating operation commits; failed ones roll back.** `write`,
//!   `create-directory`, and `remove` each end with an eofs commit (root flip), so
//!   completed operations are durable on the disk and crash consistency is the engine's
//!   by construction. The one deliberate exception is `open` with TRUNCATE on an existing
//!   file: the truncation is *staged* but not committed, so it becomes durable together
//!   with the write that follows it — a rewrite is atomic, and a failed or abandoned
//!   rewrite leaves the file's previous contents untouched on disk (study 07, S7-4). Any
//!   operation that fails discards every uncommitted change (`Eofs::rollback`), so
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
//! * **The disk import is driven eagerly.** The disk operations are `async func`s, but
//!   the engine underneath is synchronous, so each call is polled to completion on the
//!   spot; a disk that genuinely suspends makes the operation fail with an `io` error
//!   rather than blocking. Every disk wired up today (disk.mem and other compute-only
//!   backends) completes eagerly; the fully asynchronous bridge is a recorded follow-up
//!   (plan/14-eofs.md).
//! * The device size comes from the disk API's `size` query (read once per mount), and
//!   the engine's commit-boundary flushes call straight through to the disk's `flush`,
//!   so durability is the underlying device's (fsync for a file-backed disk, a virtio
//!   cache flush for `disk.virtio`, a no-op for purely in-memory devices).

#![no_std]

extern crate alloc;

use alloc::string::String;
use alloc::vec;
use alloc::vec::Vec;
use core::pin::pin;
use core::task::{Context, Poll, Waker};

use eo9_eofs::{BlockDevice, DeviceError, Eofs, FormatOptions};
use eo9_guest::provider::ProviderState;

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

/// The mounted filesystem: the eofs engine over the imported disk capability.
static STATE: ProviderState<Eofs<DiskDevice>> = ProviderState::new();

// --- the imported disk as an eofs block device ------------------------------------------

/// Drive an async disk import call that completes without suspending (see the module
/// docs: the milestone-2 provider requires an eagerly-completing disk).
fn poll_eager<F: Future>(future: F) -> Option<F::Output> {
    let mut future = pin!(future);
    let mut context = Context::from_waker(Waker::noop());
    match future.as_mut().poll(&mut context) {
        Poll::Ready(value) => Some(value),
        Poll::Pending => None,
    }
}

/// The imported `eo9:disk` capability seen as an eofs [`BlockDevice`].
struct DiskDevice {
    handle: disk::DiskImpl,
    size: u64,
}

impl DiskDevice {
    /// Take the disk's root handle and read its size from the disk API.
    fn new() -> Result<DiskDevice, DeviceError> {
        let handle = disk::default();
        let size = disk::size(&handle);
        Ok(DiskDevice { handle, size })
    }
}

impl BlockDevice for DiskDevice {
    fn size(&self) -> u64 {
        self.size
    }

    fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<(), DeviceError> {
        if buf.is_empty() {
            return Ok(());
        }
        let len = buf.len() as u64;
        let dst = Buffer::new(len);
        let (dst, result) =
            poll_eager(disk::read(&self.handle, offset, dst)).ok_or(DeviceError::Io)?;
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

    fn write_at(&mut self, offset: u64, data: &[u8]) -> Result<(), DeviceError> {
        if data.is_empty() {
            return Ok(());
        }
        let len = data.len() as u64;
        let src = Buffer::new(len);
        src.write(0, data);
        let (_src, result) =
            poll_eager(disk::write(&self.handle, offset, src)).ok_or(DeviceError::Io)?;
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

    fn flush(&mut self) -> Result<(), DeviceError> {
        // The engine calls this at every commit boundary (before and after the uberblock
        // write), so durability rides on the imported disk's own flush.
        let result = poll_eager(disk::flush(&self.handle)).ok_or(DeviceError::Io)?;
        result.map_err(|err| match err {
            disk::WriteError::OutOfRange => DeviceError::OutOfRange,
            disk::WriteError::Io(message) => DeviceError::IoNamed(message),
            disk::WriteError::ReadOnly => {
                DeviceError::IoNamed(String::from("the device is read-only"))
            }
        })
    }
}

// --- state and error mapping -------------------------------------------------------------

/// Mount the imported disk, formatting it first if — and only if — it is blank.
///
/// "Blank" means probed as [`eo9_eofs::ImageState::Blank`]: no eofs filesystem AND the
/// spans where any common format would leave its mark — the leading megabyte, the trailing
/// 64 KiB, or the whole device when it is small — are all zero. A device holding anybody
/// else's data (an ext4 image, a btrfs volume, a file pointed at by mistake) is refused, never formatted over —
/// destroying foreign data is something only an explicit, forced `mkfs` may do (study 07,
/// S7-2). A device with eofs remains that no longer mount is also never reformatted; its
/// mount error is reported instead.
fn mount_or_format() -> Result<Eofs<DiskDevice>, FsError> {
    let device = DiskDevice::new().map_err(device_error)?;
    // A device that reports size 0 is not "too small" — it is a device that could not be
    // probed at all (the `eo9:disk` size query cannot fail, so an unreachable device has
    // no meaningful size). Issue one read so the device reports its *real* typed error —
    // e.g. a driver's "no virtio-blk function is visible …" — instead of burying the cause
    // under a format-options message (study 09 finding 3).
    if device.size() == 0 {
        let mut probe = [0u8; 1];
        return match device.read_at(0, &mut probe) {
            Err(error) => Err(device_error(error)),
            Ok(()) => Err(FsError::Io(String::from(
                "the disk reports a size of 0 bytes",
            ))),
        };
    }
    match eo9_eofs::probe(&device).map_err(map_error)? {
        eo9_eofs::ImageState::Eofs { .. } | eo9_eofs::ImageState::Unmountable => {
            Eofs::mount(device).map_err(map_error)
        }
        eo9_eofs::ImageState::Blank => {
            Eofs::format(device, &FormatOptions::default()).map_err(map_error)
        }
        eo9_eofs::ImageState::Foreign => Err(FsError::Io(String::from(
            "the disk holds data that is not an eofs filesystem; refusing to format over it. If that data is expendable, format the device explicitly: `eo9 mkfs.eofs <image> --force`",
        ))),
    }
}

/// Run `f` over the mounted filesystem, mounting (or formatting a blank disk) on first
/// use — the documented default behaviour, so the unconfigured provider never traps.
fn with_fs<R>(f: impl FnOnce(&mut Eofs<DiskDevice>) -> Result<R, FsError>) -> Result<R, FsError> {
    if !STATE.is_set() {
        STATE.set(mount_or_format()?);
    }
    STATE.with(f)
}

/// Run a *mutating* operation over the mounted filesystem with rewrite atomicity and
/// space reclamation:
///
/// * If the operation runs out of space, the copy-on-write garbage no root references any
///   more — superseded copies of rewritten files, the contents of removed files — is
///   reclaimed (`Eofs::gc`) and the operation retried once. This is what keeps an image
///   usable forever instead of bricking once its append frontier reaches the end (study
///   07, S7-3): `rm` frees space, rewrites reuse the space of the copies they replace.
/// * If the operation still fails, every uncommitted change is discarded
///   (`Eofs::rollback`), so a half-applied multi-step change — a truncation whose rewrite
///   never landed, a partially built directory edit — can never be published by a later
///   commit. The committed, on-disk state only ever moves from one fully-applied
///   operation to the next.
///
/// The operation must be re-runnable from any pending state it may itself have
/// half-applied (the `open` truncate path tolerates an already-removed file for this
/// reason); see the per-operation comments.
fn mutate<R>(
    f: impl Fn(&mut Eofs<DiskDevice>) -> Result<R, eo9_eofs::FsError>,
) -> Result<R, FsError> {
    with_fs(|eofs| {
        let result = match f(eofs) {
            Err(eo9_eofs::FsError::NoSpace) => {
                // Reclaim everything unreachable (the failed attempt's own orphaned
                // blocks included — gc walks the committed and pending roots, and
                // whatever the failure left behind is referenced by neither) and try
                // again. The walk reads every live block, so it only runs when an
                // allocation has actually failed, never on the fast path.
                match eofs.gc() {
                    Ok(_) => f(eofs),
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
        mutate(|eofs| {
            match eofs.stat(&path) {
                Ok(stat) => {
                    if stat.kind == eo9_eofs::NodeKind::Directory {
                        return Err(eo9_eofs::FsError::IsADirectory);
                    }
                }
                Err(eo9_eofs::FsError::NotFound) if create => {
                    // Creating a brand-new (empty) file destroys nothing; commit it so a
                    // bare `touch` is durable on its own.
                    eofs.create_file(&path)?;
                    eofs.commit()?;
                }
                Err(error) => return Err(error),
            }
            Ok(())
        })?;
        // Truncation of an existing, non-empty file is recorded on the handle and applied
        // by the first write through it — the whole rewrite (remove + recreate + write)
        // commits as one transaction, so the previous contents survive anything short of
        // a completed replacement (study 07, S7-4).
        if truncate {
            truncate_pending = with_fs(|eofs| {
                Ok(matches!(eofs.stat(&path), Ok(stat) if stat.size > 0
                    && stat.kind == eo9_eofs::NodeKind::File))
            })?;
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
        let bytes = with_fs(|eofs| {
            let stat = eofs.stat(&path).map_err(map_error)?;
            if stat.kind == eo9_eofs::NodeKind::Directory {
                return Err(FsError::IsADirectory);
            }
            let size = usize::try_from(stat.size)
                .map_err(|_| FsError::Io(String::from("file too large for open-exec")))?;
            let mut bytes = vec![0u8; size];
            let read = eofs.read(&path, 0, &mut bytes).map_err(map_error)?;
            bytes.truncate(read);
            Ok(bytes)
        })?;
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
        with_fs(|eofs| eofs.list(&path).map_err(map_error))
    }

    async fn stat(_fs: fs::FsImplBorrow<'_>, path: String) -> Result<NodeStat, FsError> {
        let path = canonical(&path);
        with_fs(|eofs| {
            let stat = eofs.stat(&path).map_err(map_error)?;
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
    }

    async fn create_directory(_fs: fs::FsImplBorrow<'_>, path: String) -> Result<(), FsError> {
        let path = canonical(&path);
        if path.is_empty() {
            return Err(FsError::AlreadyExists);
        }
        mutate(|eofs| {
            eofs.mkdir(&path)?;
            eofs.commit()?;
            Ok(())
        })
    }

    async fn remove(_fs: fs::FsImplBorrow<'_>, path: String) -> Result<(), FsError> {
        let path = canonical(&path);
        if path.is_empty() {
            return Err(FsError::Io(String::from(
                "cannot remove the root directory",
            )));
        }
        mutate(|eofs| {
            eofs.remove(&path)?;
            eofs.commit()?;
            Ok(())
        })
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
        let result = with_fs(|eofs| {
            let mut bytes = vec![0u8; wanted];
            let read = eofs
                .read(&file.path, offset, &mut bytes)
                .map_err(map_error)?;
            if read > 0 {
                dst.write(0, &bytes[..read]);
            }
            Ok(ReadResult {
                bytes_read: read as u64,
            })
        });
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
        let result = mutate(|eofs| {
            if truncating {
                // The pending truncation and this write commit as one transaction: the
                // file is replaced wholesale (eofs has no truncate primitive, so the
                // replacement is remove + recreate). Every step is guarded so the closure
                // can be re-run from scratch by the NoSpace retry.
                if eofs.stat(&path).is_ok() {
                    eofs.remove(&path)?;
                }
                eofs.create_file(&path)?;
            }
            eofs.write(&path, offset, &bytes)?;
            eofs.commit()?;
            Ok(WriteResult { bytes_written: len })
        });
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
