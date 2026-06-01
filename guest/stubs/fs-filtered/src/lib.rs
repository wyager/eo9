//! `fs.filtered` — a path-policy-attenuated view of an underlying filesystem.
//!
//! Targets the `eo9:fs/filtered` stub world: imports `eo9:fs/fs` plus an
//! `eo9:fs/path-policy` decision function, and re-exports `fs` with every *path*
//! operation gated by the policy ("policies are programs" — SPEC, Eo9 API design).
//! Composed as ordinary middleware:
//!
//!   fs.policy-subtree --prefix /docs --access read-write $ fs.filtered $ program
//!
//! Path handling (the load-bearing part — see the `path-policy` interface docs):
//!
//! 1. Every incoming path is **normalized** first: `.` and `..` segments resolved,
//!    repeated separators collapsed. A path that tries to escape the root (more `..`
//!    than depth) is refused with `denied` before the policy is even consulted.
//! 2. The policy rules on the **normalized** path.
//! 3. The **same normalized path** is what gets forwarded to the underlying filesystem,
//!    so the path the policy approved is exactly the path that is accessed — there is no
//!    gap for `/docs/../etc/passwd` to slip through.
//!
//! Verdicts: `allow` forwards; `deny` answers the fs API's own `denied`; `read-only`
//! forwards reads and refuses mutations with `read-only`. Open files and immutable
//! handles are this provider's own resources wrapping the underlying ones, so a consumer
//! can never reach an underlying handle except through an approved open.

#![no_std]

extern crate alloc;

use alloc::string::String;
use alloc::vec::Vec;

// Linked for the guest runtime profile (allocator + panic handler).
use eo9_guest as _;

wit_bindgen::generate!({
    world: "filtered",
    path: "../../../wit/fs",
    // Pull in bindings for eo9:io/buffers, which the fs interface uses but the world
    // does not name directly.
    generate_all,
});

use eo9::fs::fs as underlying;
use eo9::fs::path_policy::{self, FsOperation, Verdict};
use exports::eo9::fs::fs::{self, Buffer, FsError, NodeStat, OpenFlags, ReadResult, WriteResult};

/// Normalize an absolute path: resolve `.` and `..` segments and collapse repeated
/// separators. Returns `None` when the path escapes the root (more `..` than depth) —
/// the middleware refuses such paths outright.
fn normalize(path: &str) -> Option<String> {
    let mut segments: Vec<&str> = Vec::new();
    for segment in path.split('/') {
        match segment {
            "" | "." => {}
            ".." => {
                segments.pop()?;
            }
            other => segments.push(other),
        }
    }
    let mut out = String::from("/");
    out.push_str(&segments.join("/"));
    Some(out)
}

/// Normalize `path`, consult the policy for `op`, and return the normalized path to
/// forward — or the fs error the operation must answer.
fn gate(path: &str, op: FsOperation) -> Result<String, FsError> {
    let Some(normalized) = normalize(path) else {
        // Escaping the root is never forwarded and never reaches the policy.
        return Err(FsError::Denied);
    };
    match path_policy::check(&normalized, op) {
        Verdict::Allow => Ok(normalized),
        Verdict::Deny => Err(FsError::Denied),
        Verdict::ReadOnly => match op {
            FsOperation::Read => Ok(normalized),
            FsOperation::Write => Err(FsError::ReadOnly),
        },
    }
}

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

/// Whether this open is a mutation (write/create/truncate) or a plain read.
fn open_operation(options: OpenFlags) -> FsOperation {
    if options.intersects(OpenFlags::WRITE | OpenFlags::CREATE | OpenFlags::TRUNCATE) {
        FsOperation::Write
    } else {
        FsOperation::Read
    }
}

/// The `fs.filtered` provider.
struct Stub;

/// The exported root handle: a token for the filtered view.
struct FilteredRoot;

impl fs::GuestFsImpl for FilteredRoot {}

/// An open file of the filtered view: wraps the underlying file, opened through an
/// approved path.
struct FilteredFile {
    inner: underlying::File,
}

/// An immutable execution handle of the filtered view: wraps the underlying handle.
struct FilteredExec {
    inner: underlying::ImmutableHandle,
}

impl fs::GuestFile for FilteredFile {}
impl fs::GuestImmutableHandle for FilteredExec {}

impl fs::Guest for Stub {
    type FsImpl = FilteredRoot;
    type File = FilteredFile;
    type ImmutableHandle = FilteredExec;

    fn default() -> fs::FsImpl {
        fs::FsImpl::new(FilteredRoot)
    }

    async fn open(
        _fs: fs::FsImplBorrow<'_>,
        path: String,
        options: OpenFlags,
    ) -> Result<fs::File, FsError> {
        let path = gate(&path, open_operation(options))?;
        let inner = underlying::open(&underlying::default(), path, map_flags(options))
            .await
            .map_err(map_error)?;
        Ok(fs::File::new(FilteredFile { inner }))
    }

    async fn open_exec(
        _fs: fs::FsImplBorrow<'_>,
        path: String,
    ) -> Result<fs::ImmutableHandle, FsError> {
        let path = gate(&path, FsOperation::Read)?;
        let inner = underlying::open_exec(&underlying::default(), path)
            .await
            .map_err(map_error)?;
        Ok(fs::ImmutableHandle::new(FilteredExec { inner }))
    }

    async fn list_directory(
        _fs: fs::FsImplBorrow<'_>,
        path: String,
    ) -> Result<Vec<String>, FsError> {
        let path = gate(&path, FsOperation::Read)?;
        underlying::list_directory(&underlying::default(), path)
            .await
            .map_err(map_error)
    }

    async fn stat(_fs: fs::FsImplBorrow<'_>, path: String) -> Result<NodeStat, FsError> {
        let path = gate(&path, FsOperation::Read)?;
        underlying::stat(&underlying::default(), path)
            .await
            .map(map_stat)
            .map_err(map_error)
    }

    async fn create_directory(_fs: fs::FsImplBorrow<'_>, path: String) -> Result<(), FsError> {
        let path = gate(&path, FsOperation::Write)?;
        underlying::create_directory(&underlying::default(), path)
            .await
            .map_err(map_error)
    }

    async fn remove(_fs: fs::FsImplBorrow<'_>, path: String) -> Result<(), FsError> {
        let path = gate(&path, FsOperation::Write)?;
        underlying::remove(&underlying::default(), path)
            .await
            .map_err(map_error)
    }

    async fn read(
        f: fs::FileBorrow<'_>,
        offset: u64,
        dst: Buffer,
    ) -> (Buffer, Result<ReadResult, FsError>) {
        let file = f.get::<FilteredFile>();
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
        f: fs::FileBorrow<'_>,
        offset: u64,
        src: Buffer,
    ) -> (Buffer, Result<WriteResult, FsError>) {
        // The path-level gate already ran at `open` (a write-capable file can only exist
        // for a path the policy allowed to be opened for writing).
        let file = f.get::<FilteredFile>();
        let (src, result) = underlying::write(&file.inner, offset, src).await;
        (
            src,
            result
                .map(|write| WriteResult {
                    bytes_written: write.bytes_written,
                })
                .map_err(map_error),
        )
    }

    fn exec_size(h: fs::ImmutableHandleBorrow<'_>) -> u64 {
        underlying::exec_size(&h.get::<FilteredExec>().inner)
    }

    async fn exec_read(
        h: fs::ImmutableHandleBorrow<'_>,
        offset: u64,
        dst: Buffer,
    ) -> (Buffer, Result<ReadResult, FsError>) {
        let exec = h.get::<FilteredExec>();
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
