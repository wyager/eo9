//! `fs.memfs` — a RAM-backed filesystem.
//!
//! Targets the `eo9:fs/memfs` stub world: exports `eo9:fs/fs` over an in-memory tree of
//! directories and files. The standard scratch/test filesystem and part of the
//! deterministic environment of integration milestone I2: directory listings come back
//! sorted and everything observable is a pure function of the operations performed.
//! The documented default state is the empty filesystem — `configure` (which takes no
//! arguments) creates exactly that, and an unconfigured memfs self-initializes to it on
//! first use, so plain `fs.memfs $ program` works and never traps (plan/09 Decision 14).
//!
//! Semantics (the MVP surface, documented here because the WIT leaves them open):
//!
//! * Paths are `/`-separated; empty and `.` segments are ignored and `..` steps up one
//!   level (never above the root). The root itself is a directory that cannot be
//!   removed, recreated, or opened as a file.
//! * `open` requires the file to exist unless `create` is given (the parent directory
//!   must always exist); `truncate` clears the contents; files opened without the
//!   `write` flag refuse `write` with an `io` error. Open files follow Unix unlink
//!   semantics: `remove` unlinks the name, but existing handles keep working.
//! * `read` returns however many bytes lie between `offset` and end-of-file (zero at or
//!   past the end); `write` zero-fills any gap between end-of-file and `offset` and
//!   extends the file.
//! * `remove` deletes a file or an *empty* directory.
//! * `open-exec` snapshots the file's current contents into the immutable handle — memfs
//!   can honestly promise immutability by copying, so handles stay content-stable no
//!   matter what is written afterwards.

#![no_std]

extern crate alloc;

use alloc::collections::BTreeMap;
use alloc::rc::Rc;
use alloc::string::String;
use alloc::vec::Vec;
use core::cell::RefCell;

use eo9_guest::provider::ProviderState;

wit_bindgen::generate!({
    world: "memfs",
    path: "../../../wit/fs",
    // Pull in bindings for eo9:io/buffers, which the exported fs interface uses but the
    // world does not name directly.
    generate_all,
});

use exports::eo9::fs::fs::{
    self, Buffer, FsError, NodeKind, NodeStat, OpenFlags, ReadResult, WriteResult,
};
use exports::eo9::fs::memfs_config;

/// A file's contents, shared between the directory tree and any open handles
/// (Unix unlink semantics: removing the name does not invalidate open files).
type FileData = Rc<RefCell<Vec<u8>>>;

/// One node of the tree.
enum Node {
    File(FileData),
    Directory(BTreeMap<String, Node>),
}

/// The filesystem state: the root directory's entries.
struct Memfs {
    root: BTreeMap<String, Node>,
}

static STATE: ProviderState<Memfs> = ProviderState::new();

/// Run `f` over the filesystem state. An unconfigured memfs defaults to the documented
/// empty filesystem — exactly the state `configure` creates, since `configure` takes no
/// arguments — so plain `fs.memfs $ program` works and never traps (the option-C
/// default-configuration rule, plan/09 Decision 14).
fn with_state<R>(f: impl FnOnce(&mut Memfs) -> R) -> R {
    if !STATE.is_set() {
        STATE.set(Memfs {
            root: BTreeMap::new(),
        });
    }
    STATE.with(f)
}

/// Resolve `path` into segments: empty and `.` segments are ignored, `..` pops one
/// level and never climbs above the root.
fn segments(path: &str) -> Vec<&str> {
    let mut segments = Vec::new();
    for segment in path.split('/') {
        match segment {
            "" | "." => {}
            ".." => {
                segments.pop();
            }
            name => segments.push(name),
        }
    }
    segments
}

impl Memfs {
    /// The directory holding the last segment of `segments` (the root for a single
    /// segment), or an error if any intermediate segment is missing or not a directory.
    fn parent_of(&mut self, segments: &[&str]) -> Result<&mut BTreeMap<String, Node>, FsError> {
        let mut directory = &mut self.root;
        for segment in &segments[..segments.len() - 1] {
            match directory.get_mut(*segment) {
                Some(Node::Directory(entries)) => directory = entries,
                Some(Node::File(_)) => return Err(FsError::NotADirectory),
                None => return Err(FsError::NotFound),
            }
        }
        Ok(directory)
    }

    /// The node at `segments`, or `None` for a missing entry (the root is represented
    /// implicitly and handled by the callers).
    fn lookup(&self, segments: &[&str]) -> Result<Option<&Node>, FsError> {
        let mut directory = &self.root;
        let (last, intermediate) = segments.split_last().expect("segments must be non-empty");
        for segment in intermediate {
            match directory.get(*segment) {
                Some(Node::Directory(entries)) => directory = entries,
                Some(Node::File(_)) => return Err(FsError::NotADirectory),
                None => return Err(FsError::NotFound),
            }
        }
        Ok(directory.get(*last))
    }

    /// The contents of the file at `segments`, with directory/missing cases mapped to
    /// fs errors.
    fn file_data(&self, segments: &[&str]) -> Result<FileData, FsError> {
        match self.lookup(segments)? {
            Some(Node::File(data)) => Ok(Rc::clone(data)),
            Some(Node::Directory(_)) => Err(FsError::IsADirectory),
            None => Err(FsError::NotFound),
        }
    }
}

/// Copy `dst.len()` bytes (or whatever is available) from `data` at `offset` into `dst`.
fn read_at(data: &[u8], offset: u64, dst: &Buffer) -> ReadResult {
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

/// Write the bytes of `src` into `data` at `offset`, zero-filling any gap and extending
/// the file as needed.
fn write_at(data: &mut Vec<u8>, offset: u64, src: &Buffer) -> Result<WriteResult, FsError> {
    let Ok(offset) = usize::try_from(offset) else {
        return Err(FsError::NoSpace);
    };
    let len = usize::try_from(src.len()).unwrap_or(usize::MAX);
    let Some(end) = offset.checked_add(len) else {
        return Err(FsError::NoSpace);
    };
    let bytes = if len == 0 {
        Vec::new()
    } else {
        src.read(0, len as u64)
    };
    if data.len() < end {
        data.resize(end, 0);
    }
    data[offset..end].copy_from_slice(&bytes);
    Ok(WriteResult {
        bytes_written: len as u64,
    })
}

/// The `fs.memfs` provider.
struct Stub;

/// The root-handle resource: a token referring to the shared tree.
struct MemfsRoot;

/// An open file: the shared contents plus whether the `write` flag was given.
struct OpenFile {
    data: FileData,
    writable: bool,
}

/// An immutable execution handle: a snapshot of the file's contents at open-exec time.
struct ExecSnapshot {
    bytes: Vec<u8>,
}

impl fs::GuestFsImpl for MemfsRoot {}

impl fs::GuestFile for OpenFile {}
impl fs::GuestImmutableHandle for ExecSnapshot {}

impl memfs_config::Guest for Stub {
    fn configure() -> Result<fs::FsImpl, String> {
        STATE.set(Memfs {
            root: BTreeMap::new(),
        });
        Ok(fs::FsImpl::new(MemfsRoot))
    }
}

impl fs::Guest for Stub {
    type FsImpl = MemfsRoot;
    type File = OpenFile;
    type ImmutableHandle = ExecSnapshot;

    fn default() -> fs::FsImpl {
        fs::FsImpl::new(MemfsRoot)
    }

    async fn open(
        _fs: fs::FsImplBorrow<'_>,
        path: String,
        options: OpenFlags,
    ) -> Result<fs::File, FsError> {
        let segments = segments(&path);
        if segments.is_empty() {
            return Err(FsError::IsADirectory);
        }
        let data = with_state(|memfs| {
            let existing = match memfs.lookup(&segments)? {
                Some(Node::File(data)) => Some(Rc::clone(data)),
                Some(Node::Directory(_)) => return Err(FsError::IsADirectory),
                None => None,
            };
            match existing {
                Some(data) => Ok(data),
                None if options.contains(OpenFlags::CREATE) => {
                    let (name, _) = segments.split_last().expect("segments are non-empty");
                    let data = FileData::default();
                    let parent = memfs.parent_of(&segments)?;
                    parent.insert(String::from(*name), Node::File(Rc::clone(&data)));
                    Ok(data)
                }
                None => Err(FsError::NotFound),
            }
        })?;
        if options.contains(OpenFlags::TRUNCATE) {
            data.borrow_mut().clear();
        }
        Ok(fs::File::new(OpenFile {
            data,
            writable: options.contains(OpenFlags::WRITE),
        }))
    }

    async fn open_exec(
        _fs: fs::FsImplBorrow<'_>,
        path: String,
    ) -> Result<fs::ImmutableHandle, FsError> {
        let segments = segments(&path);
        if segments.is_empty() {
            return Err(FsError::IsADirectory);
        }
        let data = with_state(|memfs| memfs.file_data(&segments))?;
        // Snapshot the contents: memfs promises immutability by copying.
        let bytes = data.borrow().clone();
        Ok(fs::ImmutableHandle::new(ExecSnapshot { bytes }))
    }

    async fn list_directory(
        _fs: fs::FsImplBorrow<'_>,
        path: String,
    ) -> Result<Vec<String>, FsError> {
        let segments = segments(&path);
        with_state(|memfs| {
            let entries = if segments.is_empty() {
                &memfs.root
            } else {
                match memfs.lookup(&segments)? {
                    Some(Node::Directory(entries)) => entries,
                    Some(Node::File(_)) => return Err(FsError::NotADirectory),
                    None => return Err(FsError::NotFound),
                }
            };
            // BTreeMap iteration is ordered, so listings are deterministic.
            Ok(entries.keys().cloned().collect())
        })
    }

    async fn stat(_fs: fs::FsImplBorrow<'_>, path: String) -> Result<NodeStat, FsError> {
        let segments = segments(&path);
        with_state(|memfs| {
            if segments.is_empty() {
                return Ok(NodeStat {
                    kind: NodeKind::Directory,
                    size: 0,
                });
            }
            match memfs.lookup(&segments)? {
                Some(Node::File(data)) => Ok(NodeStat {
                    kind: NodeKind::File,
                    size: data.borrow().len() as u64,
                }),
                Some(Node::Directory(_)) => Ok(NodeStat {
                    kind: NodeKind::Directory,
                    size: 0,
                }),
                None => Err(FsError::NotFound),
            }
        })
    }

    async fn create_directory(_fs: fs::FsImplBorrow<'_>, path: String) -> Result<(), FsError> {
        let segments = segments(&path);
        if segments.is_empty() {
            return Err(FsError::AlreadyExists);
        }
        with_state(|memfs| {
            if memfs.lookup(&segments)?.is_some() {
                return Err(FsError::AlreadyExists);
            }
            let (name, _) = segments.split_last().expect("segments are non-empty");
            let parent = memfs.parent_of(&segments)?;
            parent.insert(String::from(*name), Node::Directory(BTreeMap::new()));
            Ok(())
        })
    }

    async fn remove(_fs: fs::FsImplBorrow<'_>, path: String) -> Result<(), FsError> {
        let segments = segments(&path);
        if segments.is_empty() {
            return Err(FsError::Io(String::from(
                "cannot remove the root directory",
            )));
        }
        with_state(|memfs| {
            match memfs.lookup(&segments)? {
                Some(Node::Directory(entries)) if !entries.is_empty() => {
                    return Err(FsError::Io(String::from("directory is not empty")));
                }
                Some(_) => {}
                None => return Err(FsError::NotFound),
            }
            let (name, _) = segments.split_last().expect("segments are non-empty");
            let parent = memfs.parent_of(&segments)?;
            parent.remove(*name);
            Ok(())
        })
    }

    async fn read(
        f: fs::FileBorrow<'_>,
        offset: u64,
        dst: Buffer,
    ) -> (Buffer, Result<ReadResult, FsError>) {
        let file = f.get::<OpenFile>();
        let result = read_at(&file.data.borrow(), offset, &dst);
        (dst, Ok(result))
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
        let result = write_at(&mut file.data.borrow_mut(), offset, &src);
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
        let result = read_at(&h.get::<ExecSnapshot>().bytes, offset, &dst);
        (dst, Ok(result))
    }
}

export!(Stub);
