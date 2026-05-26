//! The shell session's filesystem and I/O-buffer providers (kernel side).
//!
//! eosh resolves program names as `/bin/<name>.wasm` on its granted filesystem and reads
//! the optional session manifest at `/session` (plan/12-kernel.md Decision 21). On bare
//! metal that filesystem is a read-only view of the baked-in store image:
//!
//! ```text
//! /                  directory
//! /bin               directory: one <shell-name>.wasm per store entry
//! /bin/<name>.wasm   the entry's component bytes (open, open-exec, stat, read)
//! /session           the session manifest written by the kernel at boot (env reads it)
//! ```
//!
//! Writes, creation, and removal answer `read-only`; everything else mirrors the usermode
//! provider semantics in `crates/eo9-runtime/src/link.rs` (same WIT shapes, same
//! owned-buffer round trip, same bounds). The `eo9:io/buffers` table is the same design as
//! the usermode `BufferTable`, including the per-buffer and per-task byte ceilings.

use alloc::boxed::Box;
use alloc::format;
use alloc::string::{String, ToString};
use alloc::vec;
use alloc::vec::Vec;
use core::future::Future;
use core::pin::Pin;

use wasmtime::component::{Accessor, ComponentType, Lift, Linker, Lower, Resource, ResourceType};
use wasmtime::{Result, StoreContextMut};

use super::providers::KernelState;
use super::store::StoreEntry;

/// Boxed future shape for `func_wrap_concurrent` closures (same alias as the usermode
/// runtime and the kernel root providers).
type ConcurrentFuture<'a, R> = Pin<Box<dyn Future<Output = Result<R>> + Send + 'a>>;

// -----------------------------------------------------------------------------------------
// Host resource representations
// -----------------------------------------------------------------------------------------

/// Host representation of `eo9:fs/types.fs-impl` (stateless token).
pub struct FsCap;
/// Host representation of `eo9:fs/fs.file`; the rep indexes the open-file table.
pub struct FileRes;
/// Host representation of `eo9:fs/fs.immutable-handle`; the rep indexes the exec table.
pub struct ExecRes;
/// Host representation of `eo9:io/buffers.buffer`; the rep indexes the buffer table.
pub struct BufferRes;

// -----------------------------------------------------------------------------------------
// WIT-shaped host types (eo9:fs, eo9:io)
// -----------------------------------------------------------------------------------------

#[derive(Clone, Copy, ComponentType, Lift, Lower)]
#[component(enum)]
#[repr(u8)]
#[allow(dead_code)]
enum WitNodeKind {
    #[component(name = "file")]
    File,
    #[component(name = "directory")]
    Directory,
}

#[derive(Clone, Copy, ComponentType, Lift, Lower)]
#[component(record)]
struct WitNodeStat {
    kind: WitNodeKind,
    size: u64,
}

#[derive(Clone, ComponentType, Lift, Lower)]
#[component(variant)]
#[allow(dead_code)]
enum WitFsError {
    #[component(name = "not-found")]
    NotFound,
    #[component(name = "already-exists")]
    AlreadyExists,
    #[component(name = "not-a-directory")]
    NotADirectory,
    #[component(name = "is-a-directory")]
    IsADirectory,
    #[component(name = "denied")]
    Denied,
    #[component(name = "read-only")]
    ReadOnly,
    #[component(name = "no-space")]
    NoSpace,
    #[component(name = "not-immutable")]
    NotImmutable,
    #[component(name = "io")]
    Io(String),
}

wasmtime::component::flags! {
    WitOpenFlags {
        #[component(name = "read")]
        const READ;
        #[component(name = "write")]
        const WRITE;
        #[component(name = "create")]
        const CREATE;
        #[component(name = "truncate")]
        const TRUNCATE;
    }
}

#[derive(Clone, Copy, ComponentType, Lift, Lower)]
#[component(record)]
struct WitReadResult {
    #[component(name = "bytes-read")]
    bytes_read: u64,
}

#[derive(Clone, Copy, ComponentType, Lift, Lower)]
#[component(record)]
struct WitWriteResult {
    #[component(name = "bytes-written")]
    bytes_written: u64,
}

/// Return shape of the owned-buffer fs reads.
type FsReadReturn = (Resource<BufferRes>, Result<WitReadResult, WitFsError>);
/// Return shape of the owned-buffer fs write.
type FsWriteReturn = (Resource<BufferRes>, Result<WitWriteResult, WitFsError>);

// -----------------------------------------------------------------------------------------
// Buffer table (eo9:io/buffers) — same design and bounds as the usermode runtime
// -----------------------------------------------------------------------------------------

/// Per-buffer allocation ceiling (bytes); host-side memory must be bounded before
/// allocation, exactly as in usermode.
const MAX_BUFFER_BYTES: u64 = 16 * 1024 * 1024;
/// Ceiling on the total bytes held by all live buffers of the session.
const MAX_TOTAL_BUFFER_BYTES: u64 = 64 * 1024 * 1024;

/// The backing store for the guest's buffer handles: rep → bytes.
#[derive(Default)]
pub struct BufferTable {
    slots: Vec<Option<Vec<u8>>>,
    total_bytes: u64,
}

impl BufferTable {
    fn alloc(&mut self, len: u64) -> Result<u32> {
        if len > MAX_BUFFER_BYTES {
            return Err(wasmtime::Error::msg(format!(
                "buffer of {len} bytes exceeds the per-buffer cap of {MAX_BUFFER_BYTES} bytes"
            )));
        }
        if self.total_bytes + len > MAX_TOTAL_BUFFER_BYTES {
            return Err(wasmtime::Error::msg(format!(
                "session buffer budget exceeded: {len} more bytes would pass the \
                 {MAX_TOTAL_BUFFER_BYTES}-byte ceiling"
            )));
        }
        let bytes = vec![0; len as usize];
        self.total_bytes += len;
        let index = self.slots.iter().position(Option::is_none);
        let index = match index {
            Some(index) => {
                self.slots[index] = Some(bytes);
                index
            }
            None => {
                self.slots.push(Some(bytes));
                self.slots.len() - 1
            }
        };
        u32::try_from(index).map_err(|_| wasmtime::Error::msg("buffer table full"))
    }

    fn bytes(&mut self, rep: u32) -> Result<&mut Vec<u8>> {
        self.slots
            .get_mut(rep as usize)
            .and_then(Option::as_mut)
            .ok_or_else(|| wasmtime::Error::msg(format!("unknown buffer handle {rep}")))
    }

    fn free(&mut self, rep: u32) {
        if let Some(slot) = self.slots.get_mut(rep as usize)
            && let Some(bytes) = slot.take()
        {
            self.total_bytes = self.total_bytes.saturating_sub(bytes.len() as u64);
        }
    }
}

/// Bounds-check an `(offset, len)` range against a buffer of `size` bytes.
fn byte_range(size: usize, offset: u64, len: u64) -> Result<(usize, usize)> {
    let out_of_bounds = || {
        wasmtime::Error::msg(format!(
            "buffer range out of bounds: offset {offset} + len {len} > size {size}"
        ))
    };
    let start = usize::try_from(offset).map_err(|_| out_of_bounds())?;
    let count = usize::try_from(len).map_err(|_| out_of_bounds())?;
    let end = start.checked_add(count).ok_or_else(out_of_bounds)?;
    if end > size {
        return Err(out_of_bounds());
    }
    Ok((start, end))
}

// -----------------------------------------------------------------------------------------
// The shell filesystem: a read-only view of the store image plus the session manifest
// -----------------------------------------------------------------------------------------

/// What an open file handle points at.
enum FileBacking {
    /// `/bin/<name>.wasm`: the store entry's component bytes.
    StoreComponent(usize),
    /// `/session`: the session manifest text.
    Manifest,
}

/// The shell session's fs state: the store entries it serves, the session manifest, and
/// the open-handle tables.
pub struct ShellFs {
    entries: &'static [StoreEntry],
    manifest: String,
    files: Vec<Option<FileBacking>>,
    execs: Vec<Option<usize>>,
}

/// What a path names on the shell filesystem.
enum Node {
    RootDir,
    BinDir,
    StoreComponent(usize),
    Manifest,
}

impl ShellFs {
    pub fn new(entries: &'static [StoreEntry], manifest: String) -> Self {
        ShellFs {
            entries,
            manifest,
            files: Vec::new(),
            execs: Vec::new(),
        }
    }

    pub fn entries(&self) -> &'static [StoreEntry] {
        self.entries
    }

    fn resolve(&self, path: &str) -> Option<Node> {
        let path = path.trim_end_matches('/');
        match path {
            "" | "/" => Some(Node::RootDir),
            "/bin" => Some(Node::BinDir),
            "/session" => Some(Node::Manifest),
            _ => {
                let name = path.strip_prefix("/bin/")?.strip_suffix(".wasm")?;
                let index = self.entries.iter().position(|entry| entry.name == name)?;
                Some(Node::StoreComponent(index))
            }
        }
    }

    fn insert_file(&mut self, backing: FileBacking) -> u32 {
        let index = self.files.iter().position(Option::is_none);
        let index = match index {
            Some(index) => {
                self.files[index] = Some(backing);
                index
            }
            None => {
                self.files.push(Some(backing));
                self.files.len() - 1
            }
        };
        index as u32
    }

    fn insert_exec(&mut self, entry: usize) -> u32 {
        let index = self.execs.iter().position(Option::is_none);
        let index = match index {
            Some(index) => {
                self.execs[index] = Some(entry);
                index
            }
            None => {
                self.execs.push(Some(entry));
                self.execs.len() - 1
            }
        };
        index as u32
    }

    fn close_file(&mut self, rep: u32) {
        if let Some(slot) = self.files.get_mut(rep as usize) {
            *slot = None;
        }
    }

    fn close_exec(&mut self, rep: u32) {
        if let Some(slot) = self.execs.get_mut(rep as usize) {
            *slot = None;
        }
    }

    fn open(&mut self, path: &str, flags: WitOpenFlags) -> Result<u32, WitFsError> {
        if flags.contains(WitOpenFlags::WRITE)
            || flags.contains(WitOpenFlags::CREATE)
            || flags.contains(WitOpenFlags::TRUNCATE)
        {
            return Err(WitFsError::ReadOnly);
        }
        match self.resolve(path) {
            Some(Node::StoreComponent(index)) => {
                Ok(self.insert_file(FileBacking::StoreComponent(index)))
            }
            Some(Node::Manifest) => Ok(self.insert_file(FileBacking::Manifest)),
            Some(Node::RootDir) | Some(Node::BinDir) => Err(WitFsError::IsADirectory),
            None => Err(WitFsError::NotFound),
        }
    }

    fn open_exec(&mut self, path: &str) -> Result<u32, WitFsError> {
        match self.resolve(path) {
            // The store image is baked into the kernel image: immutable by construction.
            Some(Node::StoreComponent(index)) => Ok(self.insert_exec(index)),
            Some(Node::Manifest) => Err(WitFsError::NotImmutable),
            Some(Node::RootDir) | Some(Node::BinDir) => Err(WitFsError::IsADirectory),
            None => Err(WitFsError::NotFound),
        }
    }

    fn stat(&self, path: &str) -> Result<WitNodeStat, WitFsError> {
        match self.resolve(path) {
            Some(Node::RootDir) | Some(Node::BinDir) => Ok(WitNodeStat {
                kind: WitNodeKind::Directory,
                size: 0,
            }),
            Some(Node::StoreComponent(index)) => Ok(WitNodeStat {
                kind: WitNodeKind::File,
                size: self.entries[index].component.len() as u64,
            }),
            Some(Node::Manifest) => Ok(WitNodeStat {
                kind: WitNodeKind::File,
                size: self.manifest.len() as u64,
            }),
            None => Err(WitFsError::NotFound),
        }
    }

    fn list_directory(&self, path: &str) -> Result<Vec<String>, WitFsError> {
        match self.resolve(path) {
            Some(Node::RootDir) => Ok(vec!["bin".to_string(), "session".to_string()]),
            Some(Node::BinDir) => Ok(self
                .entries
                .iter()
                .map(|entry| format!("{}.wasm", entry.name))
                .collect()),
            Some(Node::StoreComponent(_)) | Some(Node::Manifest) => Err(WitFsError::NotADirectory),
            None => Err(WitFsError::NotFound),
        }
    }

    /// Copy from a backing byte slice into `dst` (reusing the owned-buffer round trip):
    /// returns the number of bytes copied (0 at end of file).
    fn read_at(source: &[u8], offset: u64, dst: &mut [u8]) -> u64 {
        let offset = usize::try_from(offset).unwrap_or(usize::MAX);
        if offset >= source.len() {
            return 0;
        }
        let take = usize::min(dst.len(), source.len() - offset);
        dst[..take].copy_from_slice(&source[offset..offset + take]);
        take as u64
    }

    fn exec_size(&self, rep: u32) -> Result<u64> {
        let entry = self
            .execs
            .get(rep as usize)
            .and_then(|slot| *slot)
            .ok_or_else(|| wasmtime::Error::msg(format!("unknown immutable handle {rep}")))?;
        Ok(self.entries[entry].component.len() as u64)
    }
}

// -----------------------------------------------------------------------------------------
// State plumbing
// -----------------------------------------------------------------------------------------

impl KernelState {
    fn shell_fs(&mut self) -> Result<&mut ShellFs> {
        self.shell
            .as_mut()
            .map(|shell| &mut shell.fs)
            .ok_or_else(|| wasmtime::Error::msg("the fs capability was not granted to this task"))
    }

    fn shell_buffers(&mut self) -> Result<&mut BufferTable> {
        self.shell
            .as_mut()
            .map(|shell| &mut shell.buffers)
            .ok_or_else(|| wasmtime::Error::msg("io buffers are not available to this task"))
    }
}

// -----------------------------------------------------------------------------------------
// Linker registration
// -----------------------------------------------------------------------------------------

/// Register `eo9:io/buffers` against the shell session's buffer table.
pub fn add_buffers(linker: &mut Linker<KernelState>) -> Result<()> {
    let mut buffers = linker.instance("eo9:io/buffers@0.1.0")?;

    buffers.resource(
        "buffer",
        ResourceType::host::<BufferRes>(),
        |mut store: StoreContextMut<'_, KernelState>, rep| {
            if let Ok(buffers) = store.data_mut().shell_buffers() {
                buffers.free(rep);
            }
            Ok(())
        },
    )?;

    buffers.func_wrap(
        "[constructor]buffer",
        |mut store: StoreContextMut<'_, KernelState>,
         (len,): (u64,)|
         -> Result<(Resource<BufferRes>,)> {
            let rep = store.data_mut().shell_buffers()?.alloc(len)?;
            Ok((Resource::new_own(rep),))
        },
    )?;

    buffers.func_wrap(
        "[method]buffer.len",
        |mut store: StoreContextMut<'_, KernelState>,
         (buffer,): (Resource<BufferRes>,)|
         -> Result<(u64,)> {
            Ok((store.data_mut().shell_buffers()?.bytes(buffer.rep())?.len() as u64,))
        },
    )?;

    buffers.func_wrap(
        "[method]buffer.read",
        |mut store: StoreContextMut<'_, KernelState>,
         (buffer, offset, len): (Resource<BufferRes>, u64, u64)|
         -> Result<(Vec<u8>,)> {
            let bytes = store.data_mut().shell_buffers()?.bytes(buffer.rep())?;
            let (start, end) = byte_range(bytes.len(), offset, len)?;
            Ok((bytes[start..end].to_vec(),))
        },
    )?;

    buffers.func_wrap(
        "[method]buffer.write",
        |mut store: StoreContextMut<'_, KernelState>,
         (buffer, offset, data): (Resource<BufferRes>, u64, Vec<u8>)|
         -> Result<()> {
            let bytes = store.data_mut().shell_buffers()?.bytes(buffer.rep())?;
            let (start, end) = byte_range(bytes.len(), offset, data.len() as u64)?;
            bytes[start..end].copy_from_slice(&data);
            Ok(())
        },
    )?;

    Ok(())
}

/// Register `eo9:fs/types` and `eo9:fs/fs` against the shell session's read-only store
/// view. Every operation completes immediately (the data is in memory), so the async
/// members resolve on their first poll.
pub fn add_fs(linker: &mut Linker<KernelState>) -> Result<()> {
    linker.instance("eo9:fs/types@0.1.0")?.resource(
        "fs-impl",
        ResourceType::host::<FsCap>(),
        |_, _| Ok(()),
    )?;

    let mut fs = linker.instance("eo9:fs/fs@0.1.0")?;

    fs.func_wrap(
        "default",
        |_store: StoreContextMut<'_, KernelState>, (): ()| -> Result<(Resource<FsCap>,)> {
            Ok((Resource::new_own(0),))
        },
    )?;

    fs.resource(
        "file",
        ResourceType::host::<FileRes>(),
        |mut store: StoreContextMut<'_, KernelState>, rep| {
            if let Ok(fs) = store.data_mut().shell_fs() {
                fs.close_file(rep);
            }
            Ok(())
        },
    )?;
    fs.resource(
        "immutable-handle",
        ResourceType::host::<ExecRes>(),
        |mut store: StoreContextMut<'_, KernelState>, rep| {
            if let Ok(fs) = store.data_mut().shell_fs() {
                fs.close_exec(rep);
            }
            Ok(())
        },
    )?;

    fs.func_wrap_concurrent(
        "open",
        |accessor: &Accessor<KernelState>,
         (_cap, path, flags): (Resource<FsCap>, String, WitOpenFlags)|
         -> ConcurrentFuture<'_, (Result<Resource<FileRes>, WitFsError>,)> {
            Box::pin(async move {
                let result = accessor.with(|mut access| -> Result<_> {
                    Ok(access.data_mut().shell_fs()?.open(&path, flags))
                })?;
                Ok((result.map(Resource::new_own),))
            })
        },
    )?;

    fs.func_wrap_concurrent(
        "open-exec",
        |accessor: &Accessor<KernelState>,
         (_cap, path): (Resource<FsCap>, String)|
         -> ConcurrentFuture<'_, (Result<Resource<ExecRes>, WitFsError>,)> {
            Box::pin(async move {
                let result = accessor.with(|mut access| -> Result<_> {
                    Ok(access.data_mut().shell_fs()?.open_exec(&path))
                })?;
                Ok((result.map(Resource::new_own),))
            })
        },
    )?;

    fs.func_wrap_concurrent(
        "list-directory",
        |accessor: &Accessor<KernelState>,
         (_cap, path): (Resource<FsCap>, String)|
         -> ConcurrentFuture<'_, (Result<Vec<String>, WitFsError>,)> {
            Box::pin(async move {
                let result = accessor.with(|mut access| -> Result<_> {
                    Ok(access.data_mut().shell_fs()?.list_directory(&path))
                })?;
                Ok((result,))
            })
        },
    )?;

    fs.func_wrap_concurrent(
        "stat",
        |accessor: &Accessor<KernelState>,
         (_cap, path): (Resource<FsCap>, String)|
         -> ConcurrentFuture<'_, (Result<WitNodeStat, WitFsError>,)> {
            Box::pin(async move {
                let result = accessor.with(|mut access| -> Result<_> {
                    Ok(access.data_mut().shell_fs()?.stat(&path))
                })?;
                Ok((result,))
            })
        },
    )?;

    fs.func_wrap_concurrent(
        "create-directory",
        |accessor: &Accessor<KernelState>,
         (_cap, _path): (Resource<FsCap>, String)|
         -> ConcurrentFuture<'_, (Result<(), WitFsError>,)> {
            Box::pin(async move {
                // The whole view is read-only; touch the accessor only to keep the shape
                // identical to the other operations.
                let _ = accessor;
                Ok((Err(WitFsError::ReadOnly),))
            })
        },
    )?;

    fs.func_wrap_concurrent(
        "remove",
        |accessor: &Accessor<KernelState>,
         (_cap, _path): (Resource<FsCap>, String)|
         -> ConcurrentFuture<'_, (Result<(), WitFsError>,)> {
            Box::pin(async move {
                let _ = accessor;
                Ok((Err(WitFsError::ReadOnly),))
            })
        },
    )?;

    fs.func_wrap_concurrent(
        "read",
        |accessor: &Accessor<KernelState>,
         (file, offset, dst): (Resource<FileRes>, u64, Resource<BufferRes>)|
         -> ConcurrentFuture<'_, (FsReadReturn,)> {
            Box::pin(async move {
                let buffer_rep = dst.rep();
                let result = accessor.with(|mut access| -> Result<_> {
                    let state = access.data_mut();
                    let shell = state
                        .shell
                        .as_mut()
                        .ok_or_else(|| wasmtime::Error::msg("no shell session state"))?;
                    let backing = shell
                        .fs
                        .files
                        .get(file.rep() as usize)
                        .and_then(Option::as_ref)
                        .ok_or_else(|| {
                            wasmtime::Error::msg(format!("unknown file handle {}", file.rep()))
                        })?;
                    // Split the borrow: copy out of the (static or session-owned) source
                    // into the buffer slot.
                    let read = match backing {
                        FileBacking::StoreComponent(index) => {
                            let source = shell.fs.entries[*index].component;
                            let dst = shell.buffers.bytes(buffer_rep)?;
                            ShellFs::read_at(source, offset, dst)
                        }
                        FileBacking::Manifest => {
                            let source = shell.fs.manifest.clone();
                            let dst = shell.buffers.bytes(buffer_rep)?;
                            ShellFs::read_at(source.as_bytes(), offset, dst)
                        }
                    };
                    Ok(WitReadResult { bytes_read: read })
                })?;
                Ok(((Resource::new_own(buffer_rep), Ok(result)),))
            })
        },
    )?;

    fs.func_wrap_concurrent(
        "write",
        |accessor: &Accessor<KernelState>,
         (_file, _offset, src): (Resource<FileRes>, u64, Resource<BufferRes>)|
         -> ConcurrentFuture<'_, (FsWriteReturn,)> {
            Box::pin(async move {
                let _ = accessor;
                Ok(((Resource::new_own(src.rep()), Err(WitFsError::ReadOnly)),))
            })
        },
    )?;

    fs.func_wrap(
        "exec-size",
        |mut store: StoreContextMut<'_, KernelState>,
         (handle,): (Resource<ExecRes>,)|
         -> Result<(u64,)> { Ok((store.data_mut().shell_fs()?.exec_size(handle.rep())?,)) },
    )?;

    fs.func_wrap_concurrent(
        "exec-read",
        |accessor: &Accessor<KernelState>,
         (handle, offset, dst): (Resource<ExecRes>, u64, Resource<BufferRes>)|
         -> ConcurrentFuture<'_, (FsReadReturn,)> {
            Box::pin(async move {
                let buffer_rep = dst.rep();
                let result = accessor.with(|mut access| -> Result<_> {
                    let state = access.data_mut();
                    let shell = state
                        .shell
                        .as_mut()
                        .ok_or_else(|| wasmtime::Error::msg("no shell session state"))?;
                    let entry = shell
                        .fs
                        .execs
                        .get(handle.rep() as usize)
                        .and_then(|slot| *slot)
                        .ok_or_else(|| {
                            wasmtime::Error::msg(format!(
                                "unknown immutable handle {}",
                                handle.rep()
                            ))
                        })?;
                    let source = shell.fs.entries[entry].component;
                    let dst = shell.buffers.bytes(buffer_rep)?;
                    let read = ShellFs::read_at(source, offset, dst);
                    Ok(WitReadResult { bytes_read: read })
                })?;
                Ok(((Resource::new_own(buffer_rep), Ok(result)),))
            })
        },
    )?;

    Ok(())
}
