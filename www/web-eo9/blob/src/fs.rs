//! Browser `eo9:fs` (a writable in-memory filesystem) and `eo9:io/buffers` providers.
//!
//! These are the in-page analogue of the kernel's `wasm/shellfs.rs` and the usermode
//! linking in `eo9-runtime::link` / `eo9-providers-unix` — but where the kernel serves a
//! *read-only* view of its baked store image, the web VM gives a program a *writable*
//! memory-backed filesystem (the same shape as the `fs.memfs` stub), so fs programs such as
//! `readwrite` and the coreutils round-trip. Every fs operation completes immediately (the
//! data is in memory), so the async members resolve on their first poll, exactly as on the
//! kernel.
//!
//! eo9-runtime / eo9-providers-unix target host wasmtime and do not compile for
//! `wasm32-unknown-unknown`, so the WIT-shaped host types and the owned-buffer round-trip
//! are mirrored here rather than reused (same approach as `providers.rs`).

use std::boxed::Box;
use std::collections::{BTreeMap, BTreeSet};
use std::format;
use std::future::Future;
use std::pin::Pin;
use std::string::{String, ToString};
use std::vec;
use std::vec::Vec;

use wasmtime::component::{Accessor, ComponentType, Lift, Linker, Lower, Resource, ResourceType};
use wasmtime::{Result, StoreContextMut};

use crate::providers::WebState;

type ConcurrentFuture<'a, R> = Pin<Box<dyn Future<Output = Result<R>> + Send + 'a>>;

// --- Host resource representations ---------------------------------------------------------

/// `eo9:fs/fs.fs-impl` — the stateless root handle (the convention moves `fs-impl` onto the
/// `fs` interface itself).
pub struct FsCap;
/// `eo9:fs/fs.file` — rep indexes the open-file table.
pub struct FileRes;
/// `eo9:fs/fs.immutable-handle` — rep indexes the exec-snapshot table.
pub struct ExecRes;
/// `eo9:io/buffers.buffer` — rep indexes the buffer table.
pub struct BufferRes;

// --- WIT-shaped host types ----------------------------------------------------------------

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

type FsReadReturn = (
    Resource<BufferRes>,
    std::result::Result<WitReadResult, WitFsError>,
);
type FsWriteReturn = (
    Resource<BufferRes>,
    std::result::Result<WitWriteResult, WitFsError>,
);

// --- io buffer table (same design and bounds as the kernel / usermode runtime) -----------

const MAX_BUFFER_BYTES: u64 = 16 * 1024 * 1024;
const MAX_TOTAL_BUFFER_BYTES: u64 = 64 * 1024 * 1024;

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
        let index = match self.slots.iter().position(Option::is_none) {
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

// --- the writable in-memory filesystem ----------------------------------------------------

/// Total bytes the memfs will hold across all files (a bound, like the buffer table's).
const MAX_FS_BYTES: u64 = 64 * 1024 * 1024;

/// A normalized absolute path: leading slash, no trailing slash, `/` for the root.
fn normalize(path: &str) -> String {
    let trimmed = path.trim_end_matches('/');
    if trimmed.is_empty() {
        "/".to_string()
    } else if trimmed.starts_with('/') {
        trimmed.to_string()
    } else {
        format!("/{trimmed}")
    }
}

fn parent_of(path: &str) -> Option<String> {
    if path == "/" {
        return None;
    }
    match path.rfind('/') {
        Some(0) => Some("/".to_string()),
        Some(index) => Some(path[..index].to_string()),
        None => Some("/".to_string()),
    }
}

/// A writable memory filesystem: files are byte vectors, directories are a set of paths.
/// The root `/` always exists; parent directories are implied by a path's prefix.
pub struct MemFs {
    files: BTreeMap<String, Vec<u8>>,
    dirs: BTreeSet<String>,
    total_bytes: u64,
    open_files: Vec<Option<String>>,
    execs: Vec<Option<Vec<u8>>>,
}

impl MemFs {
    pub fn new() -> Self {
        let mut dirs = BTreeSet::new();
        dirs.insert("/".to_string());
        MemFs {
            files: BTreeMap::new(),
            dirs,
            total_bytes: 0,
            open_files: Vec::new(),
            execs: Vec::new(),
        }
    }

    /// A fresh filesystem pre-populated with a small sample tree, so the fs-backed coreutils
    /// (`cat`, `ls`, `wc`, `stat`, `find`, `head`) have something to show on the page. The
    /// content is informational only; programs may freely overwrite it (the fs is writable).
    pub fn seeded() -> Self {
        let mut fs = MemFs::new();
        fs.seed_dir("/docs");
        fs.seed_file(
            "/welcome.txt",
            b"Hello from the Eo9 web VM filesystem!\nThis is an in-memory eo9:fs served to \
              guest programs by the blob.\nTry: cat /welcome.txt, ls /, wc /welcome.txt.\n",
        );
        fs.seed_file(
            "/docs/about.txt",
            b"Eo9 is a capability-secure OS on the WebAssembly Component Model.\nThe coreutils \
              you are running here are real Eo9 guest components.\n",
        );
        fs.seed_file("/docs/notes.txt", b"line one\nline two\nline three\n");
        fs
    }

    /// Insert a file directly (used by [`MemFs::seeded`]); creates parent dirs as needed.
    pub fn seed_file(&mut self, path: &str, contents: &[u8]) {
        let path = normalize(path);
        if let Some((parent, _)) = path.rsplit_once('/') {
            let parent = if parent.is_empty() { "/" } else { parent };
            self.seed_dir(parent);
        }
        self.total_bytes = self
            .total_bytes
            .saturating_sub(self.files.get(&path).map(|b| b.len() as u64).unwrap_or(0))
            .saturating_add(contents.len() as u64);
        self.files.insert(path, contents.to_vec());
    }

    /// Insert a directory directly (used by [`MemFs::seeded`]).
    pub fn seed_dir(&mut self, path: &str) {
        self.dirs.insert(normalize(path));
    }

    fn is_dir(&self, path: &str) -> bool {
        if self.dirs.contains(path) {
            return true;
        }
        // A path is also a directory if some file/dir lives strictly beneath it.
        let prefix = if path == "/" {
            "/".to_string()
        } else {
            format!("{path}/")
        };
        self.files.keys().any(|p| p.starts_with(&prefix))
            || self
                .dirs
                .iter()
                .any(|p| p.starts_with(&prefix) && p != path)
    }

    fn insert_open(&mut self, path: String) -> u32 {
        let index = match self.open_files.iter().position(Option::is_none) {
            Some(index) => {
                self.open_files[index] = Some(path);
                index
            }
            None => {
                self.open_files.push(Some(path));
                self.open_files.len() - 1
            }
        };
        index as u32
    }

    fn insert_exec(&mut self, bytes: Vec<u8>) -> u32 {
        let index = match self.execs.iter().position(Option::is_none) {
            Some(index) => {
                self.execs[index] = Some(bytes);
                index
            }
            None => {
                self.execs.push(Some(bytes));
                self.execs.len() - 1
            }
        };
        index as u32
    }

    fn close_file(&mut self, rep: u32) {
        if let Some(slot) = self.open_files.get_mut(rep as usize) {
            *slot = None;
        }
    }

    fn close_exec(&mut self, rep: u32) {
        if let Some(slot) = self.execs.get_mut(rep as usize) {
            *slot = None;
        }
    }

    fn open(&mut self, path: &str, flags: WitOpenFlags) -> std::result::Result<u32, WitFsError> {
        let path = normalize(path);
        if self.is_dir(&path) {
            return Err(WitFsError::IsADirectory);
        }
        let exists = self.files.contains_key(&path);
        if !exists {
            if !flags.contains(WitOpenFlags::CREATE) {
                return Err(WitFsError::NotFound);
            }
            // The parent must be a directory (it exists implicitly unless a file shadows it).
            if let Some(parent) = parent_of(&path)
                && self.files.contains_key(&parent)
            {
                return Err(WitFsError::NotADirectory);
            }
            self.files.insert(path.clone(), Vec::new());
        } else if flags.contains(WitOpenFlags::TRUNCATE) {
            if let Some(bytes) = self.files.get_mut(&path) {
                self.total_bytes = self.total_bytes.saturating_sub(bytes.len() as u64);
                bytes.clear();
            }
        }
        Ok(self.insert_open(path))
    }

    fn open_exec(&mut self, path: &str) -> std::result::Result<u32, WitFsError> {
        let path = normalize(path);
        if self.is_dir(&path) {
            return Err(WitFsError::IsADirectory);
        }
        // A memory file is mutable, so an exec handle pins a snapshot of its current bytes.
        match self.files.get(&path) {
            Some(bytes) => Ok(self.insert_exec(bytes.clone())),
            None => Err(WitFsError::NotFound),
        }
    }

    fn stat(&self, path: &str) -> std::result::Result<WitNodeStat, WitFsError> {
        let path = normalize(path);
        if let Some(bytes) = self.files.get(&path) {
            return Ok(WitNodeStat {
                kind: WitNodeKind::File,
                size: bytes.len() as u64,
            });
        }
        if self.is_dir(&path) {
            return Ok(WitNodeStat {
                kind: WitNodeKind::Directory,
                size: 0,
            });
        }
        Err(WitFsError::NotFound)
    }

    fn list_directory(&self, path: &str) -> std::result::Result<Vec<String>, WitFsError> {
        let path = normalize(path);
        if self.files.contains_key(&path) {
            return Err(WitFsError::NotADirectory);
        }
        if !self.is_dir(&path) {
            return Err(WitFsError::NotFound);
        }
        let prefix = if path == "/" {
            "/".to_string()
        } else {
            format!("{path}/")
        };
        let mut names = BTreeSet::new();
        let collect = |full: &str, names: &mut BTreeSet<String>| {
            if let Some(rest) = full.strip_prefix(&prefix)
                && !rest.is_empty()
            {
                let immediate = rest.split('/').next().unwrap_or(rest);
                names.insert(immediate.to_string());
            }
        };
        for file in self.files.keys() {
            collect(file, &mut names);
        }
        for dir in &self.dirs {
            collect(dir, &mut names);
        }
        Ok(names.into_iter().collect())
    }

    fn create_directory(&mut self, path: &str) -> std::result::Result<(), WitFsError> {
        let path = normalize(path);
        if self.files.contains_key(&path) {
            return Err(WitFsError::AlreadyExists);
        }
        if self.dirs.contains(&path) {
            return Err(WitFsError::AlreadyExists);
        }
        if let Some(parent) = parent_of(&path)
            && self.files.contains_key(&parent)
        {
            return Err(WitFsError::NotADirectory);
        }
        self.dirs.insert(path);
        Ok(())
    }

    fn remove(&mut self, path: &str) -> std::result::Result<(), WitFsError> {
        let path = normalize(path);
        if path == "/" {
            return Err(WitFsError::Denied);
        }
        if let Some(bytes) = self.files.remove(&path) {
            self.total_bytes = self.total_bytes.saturating_sub(bytes.len() as u64);
            return Ok(());
        }
        if self.dirs.contains(&path) {
            // Refuse to remove a non-empty directory.
            let prefix = format!("{path}/");
            let nonempty = self.files.keys().any(|p| p.starts_with(&prefix))
                || self.dirs.iter().any(|p| p.starts_with(&prefix));
            if nonempty {
                return Err(WitFsError::Denied);
            }
            self.dirs.remove(&path);
            return Ok(());
        }
        Err(WitFsError::NotFound)
    }

    fn path_of(&self, rep: u32) -> Result<String> {
        self.open_files
            .get(rep as usize)
            .and_then(Option::as_ref)
            .cloned()
            .ok_or_else(|| wasmtime::Error::msg(format!("unknown file handle {rep}")))
    }

    fn read_into(&self, path: &str, offset: u64, dst: &mut [u8]) -> u64 {
        let Some(source) = self.files.get(path) else {
            return 0;
        };
        let offset = usize::try_from(offset).unwrap_or(usize::MAX);
        if offset >= source.len() {
            return 0;
        }
        let take = usize::min(dst.len(), source.len() - offset);
        dst[..take].copy_from_slice(&source[offset..offset + take]);
        take as u64
    }

    fn write_from(
        &mut self,
        path: &str,
        offset: u64,
        data: &[u8],
    ) -> std::result::Result<u64, WitFsError> {
        let offset =
            usize::try_from(offset).map_err(|_| WitFsError::Io("offset too large".into()))?;
        let end = offset
            .checked_add(data.len())
            .ok_or_else(|| WitFsError::Io("write range overflow".into()))?;
        let bytes = self.files.get_mut(path).ok_or(WitFsError::NotFound)?;
        let grow = end.saturating_sub(bytes.len()) as u64;
        if grow > 0 && self.total_bytes + grow > MAX_FS_BYTES {
            return Err(WitFsError::NoSpace);
        }
        if end > bytes.len() {
            bytes.resize(end, 0);
            self.total_bytes += grow;
        }
        bytes[offset..end].copy_from_slice(data);
        Ok(data.len() as u64)
    }

    fn exec_size(&self, rep: u32) -> Result<u64> {
        self.execs
            .get(rep as usize)
            .and_then(Option::as_ref)
            .map(|bytes| bytes.len() as u64)
            .ok_or_else(|| wasmtime::Error::msg(format!("unknown immutable handle {rep}")))
    }

    fn exec_read(&self, rep: u32, offset: u64, dst: &mut [u8]) -> Result<u64> {
        let source = self
            .execs
            .get(rep as usize)
            .and_then(Option::as_ref)
            .ok_or_else(|| wasmtime::Error::msg(format!("unknown immutable handle {rep}")))?;
        let offset = usize::try_from(offset).unwrap_or(usize::MAX);
        if offset >= source.len() {
            return Ok(0);
        }
        let take = usize::min(dst.len(), source.len() - offset);
        dst[..take].copy_from_slice(&source[offset..offset + take]);
        Ok(take as u64)
    }
}

// --- state plumbing -----------------------------------------------------------------------

impl WebState {
    fn fs(&mut self) -> &mut MemFs {
        &mut self.fs
    }
    fn buffers(&mut self) -> &mut BufferTable {
        &mut self.buffers
    }
}

// --- linker registration ------------------------------------------------------------------

/// Register `eo9:io/buffers` and `eo9:fs/fs` (the writable memfs) on the program linker.
pub fn add_fs_io(linker: &mut Linker<WebState>) -> Result<()> {
    add_buffers(linker)?;
    add_fs(linker)?;
    Ok(())
}

fn add_buffers(linker: &mut Linker<WebState>) -> Result<()> {
    let mut buffers = linker.instance("eo9:io/buffers@0.1.0")?;

    buffers.resource(
        "buffer",
        ResourceType::host::<BufferRes>(),
        |mut store: StoreContextMut<'_, WebState>, rep| {
            store.data_mut().buffers().free(rep);
            Ok(())
        },
    )?;

    buffers.func_wrap(
        "[constructor]buffer",
        |mut store: StoreContextMut<'_, WebState>,
         (len,): (u64,)|
         -> Result<(Resource<BufferRes>,)> {
            let rep = store.data_mut().buffers().alloc(len)?;
            Ok((Resource::new_own(rep),))
        },
    )?;

    buffers.func_wrap(
        "[method]buffer.len",
        |mut store: StoreContextMut<'_, WebState>,
         (buffer,): (Resource<BufferRes>,)|
         -> Result<(u64,)> {
            Ok((store.data_mut().buffers().bytes(buffer.rep())?.len() as u64,))
        },
    )?;

    buffers.func_wrap(
        "[method]buffer.read",
        |mut store: StoreContextMut<'_, WebState>,
         (buffer, offset, len): (Resource<BufferRes>, u64, u64)|
         -> Result<(Vec<u8>,)> {
            let bytes = store.data_mut().buffers().bytes(buffer.rep())?;
            let (start, end) = byte_range(bytes.len(), offset, len)?;
            Ok((bytes[start..end].to_vec(),))
        },
    )?;

    buffers.func_wrap(
        "[method]buffer.write",
        |mut store: StoreContextMut<'_, WebState>,
         (buffer, offset, data): (Resource<BufferRes>, u64, Vec<u8>)|
         -> Result<()> {
            let bytes = store.data_mut().buffers().bytes(buffer.rep())?;
            let (start, end) = byte_range(bytes.len(), offset, data.len() as u64)?;
            bytes[start..end].copy_from_slice(&data);
            Ok(())
        },
    )?;

    Ok(())
}

fn add_fs(linker: &mut Linker<WebState>) -> Result<()> {
    let mut fs = linker.instance("eo9:fs/fs@0.1.0")?;

    fs.resource("fs-impl", ResourceType::host::<FsCap>(), |_, _| Ok(()))?;

    fs.func_wrap(
        "default",
        |_store: StoreContextMut<'_, WebState>, (): ()| -> Result<(Resource<FsCap>,)> {
            Ok((Resource::new_own(0),))
        },
    )?;

    fs.resource(
        "file",
        ResourceType::host::<FileRes>(),
        |mut store: StoreContextMut<'_, WebState>, rep| {
            store.data_mut().fs().close_file(rep);
            Ok(())
        },
    )?;
    fs.resource(
        "immutable-handle",
        ResourceType::host::<ExecRes>(),
        |mut store: StoreContextMut<'_, WebState>, rep| {
            store.data_mut().fs().close_exec(rep);
            Ok(())
        },
    )?;

    fs.func_wrap_concurrent(
        "open",
        |accessor: &Accessor<WebState>,
         (_cap, path, flags): (Resource<FsCap>, String, WitOpenFlags)|
         -> ConcurrentFuture<'_, (std::result::Result<Resource<FileRes>, WitFsError>,)> {
            Box::pin(async move {
                let result = accessor.with(|mut access| access.data_mut().fs().open(&path, flags));
                Ok((result.map(Resource::new_own),))
            })
        },
    )?;

    fs.func_wrap_concurrent(
        "open-exec",
        |accessor: &Accessor<WebState>,
         (_cap, path): (Resource<FsCap>, String)|
         -> ConcurrentFuture<'_, (std::result::Result<Resource<ExecRes>, WitFsError>,)> {
            Box::pin(async move {
                let result = accessor.with(|mut access| access.data_mut().fs().open_exec(&path));
                Ok((result.map(Resource::new_own),))
            })
        },
    )?;

    fs.func_wrap_concurrent(
        "list-directory",
        |accessor: &Accessor<WebState>,
         (_cap, path): (Resource<FsCap>, String)|
         -> ConcurrentFuture<'_, (std::result::Result<Vec<String>, WitFsError>,)> {
            Box::pin(async move {
                let result =
                    accessor.with(|mut access| access.data_mut().fs().list_directory(&path));
                Ok((result,))
            })
        },
    )?;

    fs.func_wrap_concurrent(
        "stat",
        |accessor: &Accessor<WebState>,
         (_cap, path): (Resource<FsCap>, String)|
         -> ConcurrentFuture<'_, (std::result::Result<WitNodeStat, WitFsError>,)> {
            Box::pin(async move {
                let result = accessor.with(|mut access| access.data_mut().fs().stat(&path));
                Ok((result,))
            })
        },
    )?;

    fs.func_wrap_concurrent(
        "create-directory",
        |accessor: &Accessor<WebState>,
         (_cap, path): (Resource<FsCap>, String)|
         -> ConcurrentFuture<'_, (std::result::Result<(), WitFsError>,)> {
            Box::pin(async move {
                let result =
                    accessor.with(|mut access| access.data_mut().fs().create_directory(&path));
                Ok((result,))
            })
        },
    )?;

    fs.func_wrap_concurrent(
        "remove",
        |accessor: &Accessor<WebState>,
         (_cap, path): (Resource<FsCap>, String)|
         -> ConcurrentFuture<'_, (std::result::Result<(), WitFsError>,)> {
            Box::pin(async move {
                let result = accessor.with(|mut access| access.data_mut().fs().remove(&path));
                Ok((result,))
            })
        },
    )?;

    fs.func_wrap_concurrent(
        "read",
        |accessor: &Accessor<WebState>,
         (file, offset, dst): (Resource<FileRes>, u64, Resource<BufferRes>)|
         -> ConcurrentFuture<'_, (FsReadReturn,)> {
            Box::pin(async move {
                let buffer_rep = dst.rep();
                let result = accessor.with(|mut access| -> Result<_> {
                    let state = access.data_mut();
                    let path = match state.fs().path_of(file.rep()) {
                        Ok(path) => path,
                        Err(_) => return Ok(Err(WitFsError::NotFound)),
                    };
                    // Read out of the file into the owned buffer (split the borrows by
                    // copying through a temporary).
                    let size = state.fs().files.get(&path).map(Vec::len).unwrap_or(0);
                    let mut scratch = vec![0u8; size];
                    let read = state.fs().read_into(&path, offset, &mut scratch);
                    let dst = state.buffers().bytes(buffer_rep)?;
                    let take = usize::min(read as usize, dst.len());
                    dst[..take].copy_from_slice(&scratch[..take]);
                    Ok(Ok(WitReadResult {
                        bytes_read: take as u64,
                    }))
                })?;
                Ok(((Resource::new_own(buffer_rep), result),))
            })
        },
    )?;

    fs.func_wrap_concurrent(
        "write",
        |accessor: &Accessor<WebState>,
         (file, offset, src): (Resource<FileRes>, u64, Resource<BufferRes>)|
         -> ConcurrentFuture<'_, (FsWriteReturn,)> {
            Box::pin(async move {
                let buffer_rep = src.rep();
                let result = accessor.with(|mut access| -> Result<_> {
                    let state = access.data_mut();
                    let path = match state.fs().path_of(file.rep()) {
                        Ok(path) => path,
                        Err(_) => return Ok(Err(WitFsError::NotFound)),
                    };
                    let data = state.buffers().bytes(buffer_rep)?.clone();
                    let written = state.fs().write_from(&path, offset, &data);
                    Ok(written.map(|bytes_written| WitWriteResult { bytes_written }))
                })?;
                Ok(((Resource::new_own(buffer_rep), result),))
            })
        },
    )?;

    fs.func_wrap(
        "exec-size",
        |mut store: StoreContextMut<'_, WebState>,
         (handle,): (Resource<ExecRes>,)|
         -> Result<(u64,)> { Ok((store.data_mut().fs().exec_size(handle.rep())?,)) },
    )?;

    fs.func_wrap_concurrent(
        "exec-read",
        |accessor: &Accessor<WebState>,
         (handle, offset, dst): (Resource<ExecRes>, u64, Resource<BufferRes>)|
         -> ConcurrentFuture<'_, (FsReadReturn,)> {
            Box::pin(async move {
                let buffer_rep = dst.rep();
                let result = accessor.with(|mut access| -> Result<_> {
                    let state = access.data_mut();
                    let size = state.fs().exec_size(handle.rep())? as usize;
                    let mut scratch = vec![0u8; size];
                    let read = state.fs().exec_read(handle.rep(), offset, &mut scratch)?;
                    let dst = state.buffers().bytes(buffer_rep)?;
                    let take = usize::min(read as usize, dst.len());
                    dst[..take].copy_from_slice(&scratch[..take]);
                    Ok(WitReadResult {
                        bytes_read: take as u64,
                    })
                })?;
                Ok(((Resource::new_own(buffer_rep), Ok(result)),))
            })
        },
    )?;

    Ok(())
}
