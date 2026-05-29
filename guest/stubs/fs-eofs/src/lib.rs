//! `fs.eofs` — Eo9's native filesystem as a provider component (plan/14-eofs.md, M2).
//!
//! Targets the crate-local `eo9:fs-eofs/eofs` world: imports a raw block device
//! (`eo9:disk/disk`) and exports `eo9:fs/fs` backed by the `eofs-core` engine — the same
//! copy-on-write, Merkle-hashed, lz4-compressed, snapshotting filesystem the host tests
//! and (later) the kernel use. All I/O goes through the imported disk capability, so the
//! identical component runs over `disk.mem` in usermode today and over file-backed or
//! virtio disks later: `disk.mem $ fs.eofs $ program`.
//!
//! Behaviour and defaults (the option-C default-configuration rule, plan/09 Decision 14 —
//! there is no configure interface):
//!
//! * **First use mounts the disk.** If either uberblock slot carries the eofs magic the
//!   image is mounted; a blank device (no magic in either slot) is formatted in place
//!   with the default options (4 KiB blocks, lz4 on). A device that has the magic but
//!   fails to mount is *never* reformatted — the error is reported instead, so corruption
//!   can't silently become data loss.
//! * **Every mutating operation commits.** `open` (when it creates or truncates),
//!   `write`, `create-directory`, and `remove` each end with an eofs commit (root flip),
//!   so completed operations are durable on the disk and crash consistency is the
//!   engine's by construction. This trades write amplification for simplicity — fine for
//!   the MVP, batching is a later refinement.
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

use eo9_guest::provider::ProviderState;
use eofs_core::{BlockDevice, DeviceError, Eofs, FormatOptions, format};

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
        let read = result.map_err(|err| match err {
            disk::ReadError::OutOfRange => DeviceError::OutOfRange,
            disk::ReadError::NotFound | disk::ReadError::Io(_) => DeviceError::Io,
        })?;
        if read.bytes_read < len {
            // eofs never issues reads past the end it knows about; a short read is a
            // device failure, not end-of-device.
            return Err(DeviceError::Io);
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
            disk::WriteError::ReadOnly | disk::WriteError::Io(_) => DeviceError::Io,
        })?;
        if written.bytes_written < len {
            return Err(DeviceError::Io);
        }
        Ok(())
    }

    fn flush(&mut self) -> Result<(), DeviceError> {
        // The engine calls this at every commit boundary (before and after the uberblock
        // write), so durability rides on the imported disk's own flush.
        let result = poll_eager(disk::flush(&self.handle)).ok_or(DeviceError::Io)?;
        result.map_err(|_| DeviceError::Io)
    }
}

// --- state and error mapping -------------------------------------------------------------

/// Mount the imported disk, formatting it first if it is blank (see the module docs).
fn mount_or_format() -> Result<Eofs<DiskDevice>, FsError> {
    let device = DiskDevice::new().map_err(device_error)?;
    let mut has_magic = false;
    let mut magic = [0u8; 8];
    for slot in format::SLOT_OFFSETS {
        if device.size() >= slot + magic.len() as u64 {
            device.read_at(slot, &mut magic).map_err(device_error)?;
            if magic == format::MAGIC {
                has_magic = true;
            }
        }
    }
    if has_magic {
        Eofs::mount(device).map_err(map_error)
    } else {
        Eofs::format(device, &FormatOptions::default()).map_err(map_error)
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

fn device_error(error: DeviceError) -> FsError {
    FsError::Io(alloc::format!("device error: {error}"))
}

/// Map the engine's error type onto the `eo9:fs` error variants.
fn map_error(error: eofs_core::FsError) -> FsError {
    match error {
        eofs_core::FsError::NotFound => FsError::NotFound,
        eofs_core::FsError::AlreadyExists => FsError::AlreadyExists,
        eofs_core::FsError::NotADirectory => FsError::NotADirectory,
        eofs_core::FsError::IsADirectory => FsError::IsADirectory,
        eofs_core::FsError::NoSpace => FsError::NoSpace,
        eofs_core::FsError::DirectoryNotEmpty => {
            FsError::Io(String::from("directory is not empty"))
        }
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

/// An open file: a canonical path into the mounted filesystem plus the write permission
/// captured at open time.
struct OpenFile {
    path: String,
    writable: bool,
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
        with_fs(|eofs| {
            let mut mutated = false;
            match eofs.stat(&path) {
                Ok(stat) => {
                    if stat.kind == eofs_core::NodeKind::Directory {
                        return Err(FsError::IsADirectory);
                    }
                    if truncate && stat.size > 0 {
                        // eofs has no truncate primitive yet: replace the file with an
                        // empty one (same name, fresh object).
                        eofs.remove(&path).map_err(map_error)?;
                        eofs.create_file(&path).map_err(map_error)?;
                        mutated = true;
                    }
                }
                Err(eofs_core::FsError::NotFound) if create => {
                    eofs.create_file(&path).map_err(map_error)?;
                    mutated = true;
                }
                Err(error) => return Err(map_error(error)),
            }
            if mutated {
                eofs.commit().map_err(map_error)?;
            }
            Ok(())
        })?;
        Ok(fs::File::new(OpenFile {
            path,
            writable: options.contains(OpenFlags::WRITE),
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
            if stat.kind == eofs_core::NodeKind::Directory {
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
                    eofs_core::NodeKind::File => NodeKind::File,
                    eofs_core::NodeKind::Directory => NodeKind::Directory,
                },
                size: match stat.kind {
                    eofs_core::NodeKind::File => stat.size,
                    // The engine reports a directory's serialized size; the API promises 0.
                    eofs_core::NodeKind::Directory => 0,
                },
            })
        })
    }

    async fn create_directory(_fs: fs::FsImplBorrow<'_>, path: String) -> Result<(), FsError> {
        let path = canonical(&path);
        if path.is_empty() {
            return Err(FsError::AlreadyExists);
        }
        with_fs(|eofs| {
            eofs.mkdir(&path).map_err(map_error)?;
            eofs.commit().map_err(map_error)?;
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
        with_fs(|eofs| {
            eofs.remove(&path).map_err(map_error)?;
            eofs.commit().map_err(map_error)?;
            Ok(())
        })
    }

    async fn read(
        f: fs::FileBorrow<'_>,
        offset: u64,
        dst: Buffer,
    ) -> (Buffer, Result<ReadResult, FsError>) {
        let file = f.get::<OpenFile>();
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
        let result = with_fs(|eofs| {
            eofs.write(&file.path, offset, &bytes).map_err(map_error)?;
            eofs.commit().map_err(map_error)?;
            Ok(WriteResult { bytes_written: len })
        });
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
