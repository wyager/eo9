//! `fs.readonly` — a read-only view of an underlying filesystem.
//!
//! Targets the `eo9:fs/readonly` stub world: imports `eo9:fs/fs` and re-exports it with
//! every mutating operation refused — the first real middleware provider (see SPEC.md,
//! "Environments and the `&` operator": a wrapper imports an interface and re-exports
//! it). Concretely:
//!
//! * `open` with any of the `write`, `create`, or `truncate` flags, `create-directory`,
//!   `remove`, and `write` on an open file all fail with fs's own `read-only` error;
//! * everything else (`open` for reading, `read`, `list-directory`, `stat`, `open-exec`,
//!   `exec-size`, `exec-read`) forwards to the underlying filesystem.
//!
//! The root handle is shared with the underlying provider (`configure`/`default()` hand
//! out the underlying `fs-impl`), while open files and immutable handles are this
//! provider's own resources wrapping the underlying ones — so a consumer can never reach
//! an underlying file object except through the read-only view.

#![no_std]

extern crate alloc;

use alloc::string::String;
use alloc::vec::Vec;

// Linked for the guest runtime profile (allocator + panic handler).
use eo9_guest as _;

wit_bindgen::generate!({
    world: "readonly",
    path: "../../../wit/fs",
    // Pull in bindings for eo9:io/buffers, which the fs interface uses but the world
    // does not name directly.
    generate_all,
});

use eo9::fs::fs as underlying;
use eo9::fs::types::FsImpl;
use exports::eo9::fs::fs::{self, Buffer, FsError, NodeStat, OpenFlags, ReadResult, WriteResult};
use exports::eo9::fs::readonly_config;

/// Map the underlying provider's error onto this provider's (structurally identical)
/// exported error type.
fn map_error(error: underlying::FsError) -> FsError {
    match error {
        underlying::FsError::NotFound => FsError::NotFound,
        underlying::FsError::AlreadyExists => FsError::AlreadyExists,
        underlying::FsError::NotADirectory => FsError::NotADirectory,
        underlying::FsError::IsADirectory => FsError::IsADirectory,
        underlying::FsError::Denied => FsError::Denied,
        underlying::FsError::ReadOnly => FsError::ReadOnly,
        underlying::FsError::NoSpace => FsError::NoSpace,
        underlying::FsError::NotImmutable => FsError::NotImmutable,
        underlying::FsError::Io(message) => FsError::Io(message),
    }
}

/// Map the underlying node-stat onto the exported one.
fn map_stat(stat: underlying::NodeStat) -> NodeStat {
    NodeStat {
        kind: match stat.kind {
            underlying::NodeKind::File => fs::NodeKind::File,
            underlying::NodeKind::Directory => fs::NodeKind::Directory,
        },
        size: stat.size,
    }
}

/// Translate the exported open-flags value into the underlying interface's type.
fn map_flags(options: OpenFlags) -> underlying::OpenFlags {
    underlying::OpenFlags::from_bits_truncate(options.bits())
}

/// The `fs.readonly` provider.
struct Stub;

/// An open file of the read-only view: wraps the underlying file, which was necessarily
/// opened without write/create/truncate.
struct ReadonlyFile {
    inner: underlying::File,
}

/// An immutable execution handle of the read-only view: wraps the underlying handle.
struct ReadonlyExec {
    inner: underlying::ImmutableHandle,
}

impl fs::GuestFile for ReadonlyFile {}
impl fs::GuestImmutableHandle for ReadonlyExec {}

impl readonly_config::Guest for Stub {
    async fn configure() -> Result<FsImpl, String> {
        Ok(underlying::default())
    }
}

impl fs::Guest for Stub {
    type File = ReadonlyFile;
    type ImmutableHandle = ReadonlyExec;

    fn default() -> FsImpl {
        underlying::default()
    }

    async fn open(fs: &FsImpl, path: String, options: OpenFlags) -> Result<fs::File, FsError> {
        if options.intersects(OpenFlags::WRITE | OpenFlags::CREATE | OpenFlags::TRUNCATE) {
            return Err(FsError::ReadOnly);
        }
        let inner = underlying::open(fs, path, map_flags(options))
            .await
            .map_err(map_error)?;
        Ok(fs::File::new(ReadonlyFile { inner }))
    }

    async fn open_exec(fs: &FsImpl, path: String) -> Result<fs::ImmutableHandle, FsError> {
        let inner = underlying::open_exec(fs, path).await.map_err(map_error)?;
        Ok(fs::ImmutableHandle::new(ReadonlyExec { inner }))
    }

    async fn list_directory(fs: &FsImpl, path: String) -> Result<Vec<String>, FsError> {
        underlying::list_directory(fs, path)
            .await
            .map_err(map_error)
    }

    async fn stat(fs: &FsImpl, path: String) -> Result<NodeStat, FsError> {
        underlying::stat(fs, path)
            .await
            .map(map_stat)
            .map_err(map_error)
    }

    async fn create_directory(_fs: &FsImpl, _path: String) -> Result<(), FsError> {
        Err(FsError::ReadOnly)
    }

    async fn remove(_fs: &FsImpl, _path: String) -> Result<(), FsError> {
        Err(FsError::ReadOnly)
    }

    async fn read(
        f: fs::FileBorrow<'_>,
        offset: u64,
        dst: Buffer,
    ) -> (Buffer, Result<ReadResult, FsError>) {
        let file = f.get::<ReadonlyFile>();
        let (dst, result) = underlying::read(&file.inner, offset, dst).await;
        (
            dst,
            result
                .map(|read| ReadResult {
                    bytes_read: read.bytes_read,
                })
                .map_err(map_error),
        )
    }

    async fn write(
        _f: fs::FileBorrow<'_>,
        _offset: u64,
        src: Buffer,
    ) -> (Buffer, Result<WriteResult, FsError>) {
        (src, Err(FsError::ReadOnly))
    }

    fn exec_size(h: fs::ImmutableHandleBorrow<'_>) -> u64 {
        underlying::exec_size(&h.get::<ReadonlyExec>().inner)
    }

    async fn exec_read(
        h: fs::ImmutableHandleBorrow<'_>,
        offset: u64,
        dst: Buffer,
    ) -> (Buffer, Result<ReadResult, FsError>) {
        let exec = h.get::<ReadonlyExec>();
        let (dst, result) = underlying::exec_read(&exec.inner, offset, dst).await;
        (
            dst,
            result
                .map(|read| ReadResult {
                    bytes_read: read.bytes_read,
                })
                .map_err(map_error),
        )
    }
}

export!(Stub);
