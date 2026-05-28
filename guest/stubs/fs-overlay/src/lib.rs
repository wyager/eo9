//! `fs.overlay` — an overlay filesystem provider (SPEC.md, "Overlay filesystems").
//!
//! Imports two `eo9:fs/fs` instances under the named slots `upper` and `lower` (wired
//! with `with <a> as upper, <b> as lower $ fs.overlay`) and exports a single `eo9:fs/fs`
//! that layers them:
//!
//! * reads — `open`(read), `stat`, `open-exec` try `upper` first and fall through to
//!   `lower` on not-found; `list-directory` returns the union of both layers' entries
//!   (upper wins on a name collision);
//! * writes — `open`(write), `write`, `create-directory`, `remove` are routed to
//!   `lower`; the overlay never mutates `upper`.
//!
//! The overlay's exported `fs-impl` is its own compound root handle capturing the two
//! underlying roots (`upper::default()` / `lower::default()`). Open files and immutable
//! handles are this provider's own resources tagging which layer served the open, so each
//! subsequent `read`/`write`/`exec-read` is dispatched back to that layer. Binding-type
//! notes: each import slot mints its own nominal `fs-impl`/`file`/`immutable-handle`/error
//! types (only the `eo9:io` buffer resource is shared) — hence the per-layer enums and the
//! per-layer mapping helpers below.

#![no_std]

extern crate alloc;

use alloc::string::String;
use alloc::vec::Vec;

// Linked for the guest runtime profile (allocator + panic handler).
use eo9_guest as _;

wit_bindgen::generate!({
    world: "overlay",
    path: "wit",
    generate_all,
});

use exports::eo9::fs::fs::{self, Buffer, FsError, NodeStat, OpenFlags, ReadResult, WriteResult};

/// Map the `upper` slot's error onto the exported (structurally identical) error type.
fn upper_error(error: upper::FsError) -> FsError {
    match error {
        upper::FsError::NotFound => FsError::NotFound,
        upper::FsError::AlreadyExists => FsError::AlreadyExists,
        upper::FsError::NotADirectory => FsError::NotADirectory,
        upper::FsError::IsADirectory => FsError::IsADirectory,
        upper::FsError::Denied => FsError::Denied,
        upper::FsError::ReadOnly => FsError::ReadOnly,
        upper::FsError::NoSpace => FsError::NoSpace,
        upper::FsError::NotImmutable => FsError::NotImmutable,
        upper::FsError::Io(message) => FsError::Io(message),
    }
}

/// Map the `lower` slot's error onto the exported error type.
fn lower_error(error: lower::FsError) -> FsError {
    match error {
        lower::FsError::NotFound => FsError::NotFound,
        lower::FsError::AlreadyExists => FsError::AlreadyExists,
        lower::FsError::NotADirectory => FsError::NotADirectory,
        lower::FsError::IsADirectory => FsError::IsADirectory,
        lower::FsError::Denied => FsError::Denied,
        lower::FsError::ReadOnly => FsError::ReadOnly,
        lower::FsError::NoSpace => FsError::NoSpace,
        lower::FsError::NotImmutable => FsError::NotImmutable,
        lower::FsError::Io(message) => FsError::Io(message),
    }
}

fn upper_stat(stat: upper::NodeStat) -> NodeStat {
    NodeStat {
        kind: match stat.kind {
            upper::NodeKind::File => fs::NodeKind::File,
            upper::NodeKind::Directory => fs::NodeKind::Directory,
        },
        size: stat.size,
    }
}

fn lower_stat(stat: lower::NodeStat) -> NodeStat {
    NodeStat {
        kind: match stat.kind {
            lower::NodeKind::File => fs::NodeKind::File,
            lower::NodeKind::Directory => fs::NodeKind::Directory,
        },
        size: stat.size,
    }
}

fn upper_flags(options: OpenFlags) -> upper::OpenFlags {
    upper::OpenFlags::from_bits_truncate(options.bits())
}

fn lower_flags(options: OpenFlags) -> lower::OpenFlags {
    lower::OpenFlags::from_bits_truncate(options.bits())
}

fn upper_read(result: upper::ReadResult) -> ReadResult {
    ReadResult {
        bytes_read: result.bytes_read,
    }
}

fn lower_read(result: lower::ReadResult) -> ReadResult {
    ReadResult {
        bytes_read: result.bytes_read,
    }
}

/// True if `options` requests any mutation of the file (so the open is routed to `lower`).
fn is_write(options: OpenFlags) -> bool {
    options.intersects(OpenFlags::WRITE | OpenFlags::CREATE | OpenFlags::TRUNCATE)
}

/// The exported root handle: captures both underlying filesystems' root handles
/// (each slot's `fs-impl` is its own nominal type).
struct OverlayImpl {
    upper: upper::FsImpl,
    lower: lower::FsImpl,
}

/// An open file of the overlay, tagged with the layer that served the open.
enum OverlayFile {
    Upper(upper::File),
    Lower(lower::File),
}

/// An immutable execution handle of the overlay, tagged with its layer.
enum OverlayExec {
    Upper(upper::ImmutableHandle),
    Lower(lower::ImmutableHandle),
}

struct Stub;

impl fs::GuestFsImpl for OverlayImpl {}

impl fs::GuestFile for OverlayFile {}
impl fs::GuestImmutableHandle for OverlayExec {}

impl fs::Guest for Stub {
    type FsImpl = OverlayImpl;
    type File = OverlayFile;
    type ImmutableHandle = OverlayExec;

    fn default() -> fs::FsImpl {
        fs::FsImpl::new(OverlayImpl {
            upper: upper::default(),
            lower: lower::default(),
        })
    }

    async fn open(
        fs: fs::FsImplBorrow<'_>,
        path: String,
        options: OpenFlags,
    ) -> Result<fs::File, FsError> {
        let overlay = fs.get::<OverlayImpl>();
        if is_write(options) {
            // Writes (and create/truncate) always go to the writable lower layer.
            let inner = lower::open(&overlay.lower, path, lower_flags(options))
                .await
                .map_err(lower_error)?;
            return Ok(fs::File::new(OverlayFile::Lower(inner)));
        }
        // Read-only open: try upper, fall through to lower on not-found.
        match upper::open(&overlay.upper, path.clone(), upper_flags(options)).await {
            Ok(inner) => Ok(fs::File::new(OverlayFile::Upper(inner))),
            Err(upper::FsError::NotFound) => {
                let inner = lower::open(&overlay.lower, path, lower_flags(options))
                    .await
                    .map_err(lower_error)?;
                Ok(fs::File::new(OverlayFile::Lower(inner)))
            }
            Err(other) => Err(upper_error(other)),
        }
    }

    async fn open_exec(
        fs: fs::FsImplBorrow<'_>,
        path: String,
    ) -> Result<fs::ImmutableHandle, FsError> {
        let overlay = fs.get::<OverlayImpl>();
        match upper::open_exec(&overlay.upper, path.clone()).await {
            Ok(inner) => Ok(fs::ImmutableHandle::new(OverlayExec::Upper(inner))),
            Err(upper::FsError::NotFound) => {
                let inner = lower::open_exec(&overlay.lower, path)
                    .await
                    .map_err(lower_error)?;
                Ok(fs::ImmutableHandle::new(OverlayExec::Lower(inner)))
            }
            Err(other) => Err(upper_error(other)),
        }
    }

    async fn list_directory(
        fs: fs::FsImplBorrow<'_>,
        path: String,
    ) -> Result<Vec<String>, FsError> {
        let overlay = fs.get::<OverlayImpl>();
        let upper_entries = upper::list_directory(&overlay.upper, path.clone()).await;
        let lower_entries = lower::list_directory(&overlay.lower, path).await;
        match (upper_entries, lower_entries) {
            // Union both layers' entries (upper wins on collisions — entries are names,
            // so the union is just a dedup).
            (Ok(mut up), Ok(low)) => {
                for name in low {
                    if !up.contains(&name) {
                        up.push(name);
                    }
                }
                Ok(up)
            }
            (Ok(up), Err(_)) => Ok(up),
            (Err(_), Ok(low)) => Ok(low),
            // Neither layer has the directory: report the upper's error (typically
            // not-found), matching read-through precedence.
            (Err(up), Err(_)) => Err(upper_error(up)),
        }
    }

    async fn stat(fs: fs::FsImplBorrow<'_>, path: String) -> Result<NodeStat, FsError> {
        let overlay = fs.get::<OverlayImpl>();
        match upper::stat(&overlay.upper, path.clone()).await {
            Ok(stat) => Ok(upper_stat(stat)),
            Err(upper::FsError::NotFound) => lower::stat(&overlay.lower, path)
                .await
                .map(lower_stat)
                .map_err(lower_error),
            Err(other) => Err(upper_error(other)),
        }
    }

    async fn create_directory(
        fs: fs::FsImplBorrow<'_>,
        path: String,
    ) -> Result<(), FsError> {
        let overlay = fs.get::<OverlayImpl>();
        lower::create_directory(&overlay.lower, path)
            .await
            .map_err(lower_error)
    }

    async fn remove(fs: fs::FsImplBorrow<'_>, path: String) -> Result<(), FsError> {
        let overlay = fs.get::<OverlayImpl>();
        lower::remove(&overlay.lower, path)
            .await
            .map_err(lower_error)
    }

    async fn read(
        f: fs::FileBorrow<'_>,
        offset: u64,
        dst: Buffer,
    ) -> (Buffer, Result<ReadResult, FsError>) {
        match f.get::<OverlayFile>() {
            OverlayFile::Upper(inner) => {
                let (dst, result) = upper::read(inner, offset, dst).await;
                (dst, result.map(upper_read).map_err(upper_error))
            }
            OverlayFile::Lower(inner) => {
                let (dst, result) = lower::read(inner, offset, dst).await;
                (dst, result.map(lower_read).map_err(lower_error))
            }
        }
    }

    async fn write(
        f: fs::FileBorrow<'_>,
        offset: u64,
        src: Buffer,
    ) -> (Buffer, Result<WriteResult, FsError>) {
        match f.get::<OverlayFile>() {
            // A file opened for writing lives on lower; this is the normal path.
            OverlayFile::Lower(inner) => {
                let (src, result) = lower::write(inner, offset, src).await;
                (
                    src,
                    result
                        .map(|w| WriteResult {
                            bytes_written: w.bytes_written,
                        })
                        .map_err(lower_error),
                )
            }
            // Writing through a read-opened upper file: forward so upper's own policy
            // (typically read-only) decides — the overlay never special-cases it.
            OverlayFile::Upper(inner) => {
                let (src, result) = upper::write(inner, offset, src).await;
                (
                    src,
                    result
                        .map(|w| WriteResult {
                            bytes_written: w.bytes_written,
                        })
                        .map_err(upper_error),
                )
            }
        }
    }

    fn exec_size(h: fs::ImmutableHandleBorrow<'_>) -> u64 {
        match h.get::<OverlayExec>() {
            OverlayExec::Upper(inner) => upper::exec_size(inner),
            OverlayExec::Lower(inner) => lower::exec_size(inner),
        }
    }

    async fn exec_read(
        h: fs::ImmutableHandleBorrow<'_>,
        offset: u64,
        dst: Buffer,
    ) -> (Buffer, Result<ReadResult, FsError>) {
        match h.get::<OverlayExec>() {
            OverlayExec::Upper(inner) => {
                let (dst, result) = upper::exec_read(inner, offset, dst).await;
                (dst, result.map(upper_read).map_err(upper_error))
            }
            OverlayExec::Lower(inner) => {
                let (dst, result) = lower::exec_read(inner, offset, dst).await;
                (dst, result.map(lower_read).map_err(lower_error))
            }
        }
    }
}

export!(Stub);
