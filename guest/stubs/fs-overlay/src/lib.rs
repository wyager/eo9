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
//! The overlay owns its exported `fs-impl`: `default()` builds one that captures the two
//! underlying root handles (`upper::default()` / `lower::default()`). Open files and
//! immutable handles are this provider's own resources tagging which layer served the
//! open, so each subsequent `read`/`write`/`exec-read` is dispatched back to that layer.
//!
//! STATUS: DRAFT, not yet built — this crate is excluded from the guest workspace
//! (guest/Cargo.toml) and blocked on a toolchain bump. The world below imports two
//! `eo9:fs/fs` instances under named slots (`upper`, `lower`), which `wasm-tools` 1.250
//! accepts and resolves, but the pinned `wit-bindgen` 0.57.1 (wit-parser 0.247) cannot
//! parse (`import upper: eo9:fs/fs@0.1.0;` → "expected `/`, found `:`"; the `use`-alias
//! form fails too). The forwarding logic below is complete, but the generated binding
//! module paths (`upper_fs`/`lower_fs`) and whether the two imports share resource types
//! are UNVERIFIED until bindings can actually be generated. See plan/09 for the unblock.

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
use exports::eo9::fs::types::FsImpl;

// The two imported filesystems. Named slots become distinct binding modules.
use eo9::fs::fs as lower_fs;
use upper::eo9::fs::fs as upper_fs;

/// Which underlying layer a handle came from, so reads/writes dispatch correctly.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Layer {
    Upper,
    Lower,
}

/// Map an underlying provider's fs-error onto this provider's (structurally identical)
/// exported error type. Both imported interfaces are the same `eo9:fs/fs`, so they share
/// the imported error type.
fn map_error(error: lower_fs::FsError) -> FsError {
    match error {
        lower_fs::FsError::NotFound => FsError::NotFound,
        lower_fs::FsError::AlreadyExists => FsError::AlreadyExists,
        lower_fs::FsError::NotADirectory => FsError::NotADirectory,
        lower_fs::FsError::IsADirectory => FsError::IsADirectory,
        lower_fs::FsError::Denied => FsError::Denied,
        lower_fs::FsError::ReadOnly => FsError::ReadOnly,
        lower_fs::FsError::NoSpace => FsError::NoSpace,
        lower_fs::FsError::NotImmutable => FsError::NotImmutable,
        lower_fs::FsError::Io(message) => FsError::Io(message),
    }
}

fn map_stat(stat: lower_fs::NodeStat) -> NodeStat {
    NodeStat {
        kind: match stat.kind {
            lower_fs::NodeKind::File => fs::NodeKind::File,
            lower_fs::NodeKind::Directory => fs::NodeKind::Directory,
        },
        size: stat.size,
    }
}

fn map_flags(options: OpenFlags) -> lower_fs::OpenFlags {
    lower_fs::OpenFlags::from_bits_truncate(options.bits())
}

fn map_read(result: lower_fs::ReadResult) -> ReadResult {
    ReadResult {
        bytes_read: result.bytes_read,
    }
}

/// True if `options` requests any mutation of the file (so the open is routed to `lower`).
fn is_write(options: OpenFlags) -> bool {
    options.intersects(OpenFlags::WRITE | OpenFlags::CREATE | OpenFlags::TRUNCATE)
}

/// The exported root handle: captures both underlying filesystems' root handles.
struct OverlayImpl {
    upper: upper_fs::FsImpl,
    lower: lower_fs::FsImpl,
}

/// An open file of the overlay: the underlying file plus the layer it lives on.
struct OverlayFile {
    layer: Layer,
    inner: lower_fs::File,
}

/// An immutable execution handle of the overlay: the underlying handle plus its layer.
struct OverlayExec {
    layer: Layer,
    inner: lower_fs::ImmutableHandle,
}

struct Stub;

impl fs::GuestFile for OverlayFile {}
impl fs::GuestImmutableHandle for OverlayExec {}

impl fs::Guest for Stub {
    type File = OverlayFile;
    type ImmutableHandle = OverlayExec;

    fn default() -> FsImpl {
        FsImpl::new(OverlayImpl {
            upper: upper_fs::default(),
            lower: lower_fs::default(),
        })
    }

    async fn open(fs: &FsImpl, path: String, options: OpenFlags) -> Result<fs::File, FsError> {
        let overlay = fs.get::<OverlayImpl>();
        if is_write(options) {
            // Writes (and create/truncate) always go to the writable lower layer.
            let inner = lower_fs::open(&overlay.lower, path, map_flags(options))
                .await
                .map_err(map_error)?;
            return Ok(fs::File::new(OverlayFile {
                layer: Layer::Lower,
                inner,
            }));
        }
        // Read-only open: try upper, fall through to lower on not-found.
        match upper_fs::open(&overlay.upper, path.clone(), map_flags(options)).await {
            Ok(inner) => Ok(fs::File::new(OverlayFile {
                layer: Layer::Upper,
                inner,
            })),
            Err(upper_fs::FsError::NotFound) => {
                let inner = lower_fs::open(&overlay.lower, path, map_flags(options))
                    .await
                    .map_err(map_error)?;
                Ok(fs::File::new(OverlayFile {
                    layer: Layer::Lower,
                    inner,
                }))
            }
            Err(other) => Err(map_error(other)),
        }
    }

    async fn open_exec(fs: &FsImpl, path: String) -> Result<fs::ImmutableHandle, FsError> {
        let overlay = fs.get::<OverlayImpl>();
        match upper_fs::open_exec(&overlay.upper, path.clone()).await {
            Ok(inner) => Ok(fs::ImmutableHandle::new(OverlayExec {
                layer: Layer::Upper,
                inner,
            })),
            Err(upper_fs::FsError::NotFound) => {
                let inner = lower_fs::open_exec(&overlay.lower, path)
                    .await
                    .map_err(map_error)?;
                Ok(fs::ImmutableHandle::new(OverlayExec {
                    layer: Layer::Lower,
                    inner,
                }))
            }
            Err(other) => Err(map_error(other)),
        }
    }

    async fn list_directory(fs: &FsImpl, path: String) -> Result<Vec<String>, FsError> {
        let overlay = fs.get::<OverlayImpl>();
        let upper = upper_fs::list_directory(&overlay.upper, path.clone()).await;
        let lower = lower_fs::list_directory(&overlay.lower, path).await;
        match (upper, lower) {
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
            (Err(up), Err(_)) => Err(map_error(up)),
        }
    }

    async fn stat(fs: &FsImpl, path: String) -> Result<NodeStat, FsError> {
        let overlay = fs.get::<OverlayImpl>();
        match upper_fs::stat(&overlay.upper, path.clone()).await {
            Ok(stat) => Ok(map_stat(stat)),
            Err(upper_fs::FsError::NotFound) => lower_fs::stat(&overlay.lower, path)
                .await
                .map(map_stat)
                .map_err(map_error),
            Err(other) => Err(map_error(other)),
        }
    }

    async fn create_directory(fs: &FsImpl, path: String) -> Result<(), FsError> {
        let overlay = fs.get::<OverlayImpl>();
        lower_fs::create_directory(&overlay.lower, path)
            .await
            .map_err(map_error)
    }

    async fn remove(fs: &FsImpl, path: String) -> Result<(), FsError> {
        let overlay = fs.get::<OverlayImpl>();
        lower_fs::remove(&overlay.lower, path).await.map_err(map_error)
    }

    async fn read(
        f: fs::FileBorrow<'_>,
        offset: u64,
        dst: Buffer,
    ) -> (Buffer, Result<ReadResult, FsError>) {
        let file = f.get::<OverlayFile>();
        match file.layer {
            Layer::Upper => {
                let (dst, result) = upper_fs::read(&file.inner, offset, dst).await;
                (dst, result.map(map_read).map_err(map_error))
            }
            Layer::Lower => {
                let (dst, result) = lower_fs::read(&file.inner, offset, dst).await;
                (dst, result.map(map_read).map_err(map_error))
            }
        }
    }

    async fn write(
        f: fs::FileBorrow<'_>,
        offset: u64,
        src: Buffer,
    ) -> (Buffer, Result<WriteResult, FsError>) {
        let file = f.get::<OverlayFile>();
        match file.layer {
            // A file opened for writing lives on lower; this is the normal path.
            Layer::Lower => {
                let (src, result) = lower_fs::write(&file.inner, offset, src).await;
                (
                    src,
                    result
                        .map(|w| WriteResult {
                            bytes_written: w.bytes_written,
                        })
                        .map_err(map_error),
                )
            }
            // Writing through a read-opened upper file: forward so upper's own policy
            // (typically read-only) decides — the overlay never special-cases it.
            Layer::Upper => {
                let (src, result) = upper_fs::write(&file.inner, offset, src).await;
                (
                    src,
                    result
                        .map(|w| WriteResult {
                            bytes_written: w.bytes_written,
                        })
                        .map_err(map_error),
                )
            }
        }
    }

    fn exec_size(h: fs::ImmutableHandleBorrow<'_>) -> u64 {
        let exec = h.get::<OverlayExec>();
        match exec.layer {
            Layer::Upper => upper_fs::exec_size(&exec.inner),
            Layer::Lower => lower_fs::exec_size(&exec.inner),
        }
    }

    async fn exec_read(
        h: fs::ImmutableHandleBorrow<'_>,
        offset: u64,
        dst: Buffer,
    ) -> (Buffer, Result<ReadResult, FsError>) {
        let exec = h.get::<OverlayExec>();
        match exec.layer {
            Layer::Upper => {
                let (dst, result) = upper_fs::exec_read(&exec.inner, offset, dst).await;
                (dst, result.map(map_read).map_err(map_error))
            }
            Layer::Lower => {
                let (dst, result) = lower_fs::exec_read(&exec.inner, offset, dst).await;
                (dst, result.map(map_read).map_err(map_error))
            }
        }
    }
}

export!(Stub);
