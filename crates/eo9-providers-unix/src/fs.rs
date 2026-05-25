//! Root provider for `eo9:fs` — unix-like filesystem operations rooted at a host
//! directory.
//!
//! All operations are potentially blocking and complete asynchronously on the provider's
//! blocking pool via a caller-supplied [`Completer`]. File I/O is offset-addressed with
//! the owned-buffer round-trip (`pread`/`pwrite`), the same shape as the disk API, so
//! any number of operations may be in flight on one file.
//!
//! # Rooting and containment
//!
//! The provider is confined to the host directory given at construction (canonicalized
//! there). Guest paths are interpreted relative to that root; a leading `/` also means
//! the provider root. Containment is enforced in two layers:
//!
//! * lexically — `..` and platform prefixes are rejected outright (`denied`);
//! * on the real tree — the path (or, for paths being created, its parent directory; for
//!   `remove`, the parent of the final component) is canonicalized and must still lie
//!   under the root, so symlinks *inside* the tree may be followed but may not lead out
//!   of it (`denied` if they try).
//!
//! Limits: the canonicalize-then-operate sequence is not atomic, so a host-side actor
//! racing the provider (swapping a directory for a symlink between the check and the
//! syscall) could still redirect an operation outside the root. Closing that hole needs
//! per-component `O_NOFOLLOW` walks or `openat2(RESOLVE_BENEATH)`; the MVP accepts the
//! race and documents it because the usermode provider's root is chosen by the same
//! trusted host user the race would have to be mounted by.
//!
//! # `open-exec` immutability
//!
//! `open-exec` returns an immutable handle by **copy-on-open to an anonymous file**: the
//! source is copied into a freshly created, uniquely named file in the provider's
//! exec-copy directory (the system temp dir by default), which is then immediately
//! unlinked, leaving a file reachable only through the provider's own descriptor.
//! Guarantee: once `open-exec` completes, the bytes observed through the handle cannot
//! change for the life of the handle — no rename, truncate, rewrite, or deletion of the
//! original path (by any process) affects it, and no other process can open the private
//! copy by name because it has none. Limits on a non-COW host filesystem:
//!
//! * the snapshot is taken by a plain copy, so it is not atomic with respect to a writer
//!   actively modifying the source *during* `open-exec` — the handle is still immutable
//!   afterwards, but it may capture a torn intermediate state (callers that need
//!   point-in-time consistency must quiesce writers, as with any non-COW snapshot);
//! * the copy costs O(file size) time and temp space per open (a COW backend — APFS
//!   `clonefile`, Linux reflink, or a content-addressed store — can make this O(1)
//!   without changing the caller-visible guarantee);
//! * host superusers and the owner of the temp directory's filesystem can, as always,
//!   reach the provider's memory or descriptors; the guarantee is about well-behaved
//!   host filesystems, not a defense against a hostile host root.
//!
//! Because the copy always succeeds in providing the promise, this provider never
//! returns `not-immutable`.
//!
//! # Kill behavior
//!
//! In-flight operations are never aborted: they run to completion on a pool thread (a
//! write issued before a kill may still reach the backing file), the completer receives
//! the result — including the returned buffer — and a dead caller's runtime drops it.
//! Dropping the provider (and the pool) drains already-submitted operations first.

use std::fs::{File, OpenOptions};
use std::io;
use std::os::unix::fs::FileExt;
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use crate::buffer::OwnedBuffer;
use crate::completion::Completer;
use crate::pool::BlockingPool;

/// Kind of a filesystem node (WIT `node-kind`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NodeKind {
    /// A regular file.
    File,
    /// A directory.
    Directory,
}

/// Result of `stat` (WIT `node-stat`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NodeStat {
    /// What the node is.
    pub kind: NodeKind,
    /// Size in bytes (0 for directories).
    pub size: u64,
}

/// Filesystem errors (WIT `fs-error`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FsError {
    /// The path does not exist.
    NotFound,
    /// The path already exists.
    AlreadyExists,
    /// A non-directory was used where a directory is required.
    NotADirectory,
    /// A directory was used where a file is required.
    IsADirectory,
    /// Refused by policy (here: the path escapes the provider root, or the host refused
    /// access).
    Denied,
    /// The file or filesystem is read-only.
    ReadOnly,
    /// The backing store is full.
    NoSpace,
    /// The backend cannot promise immutability (never returned by this provider — see
    /// the module docs).
    NotImmutable,
    /// Any other host I/O failure.
    Io(String),
}

/// Open flags (WIT `open-flags`).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct OpenFlags {
    /// Open for reading.
    pub read: bool,
    /// Open for writing.
    pub write: bool,
    /// Create the file if it does not exist (requires `write`).
    pub create: bool,
    /// Truncate the file to zero length on open (requires `write`).
    pub truncate: bool,
}

/// Successful read (WIT `read-result`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReadResult {
    /// Number of bytes read into the buffer, starting at its beginning. May be less than
    /// the buffer length if the read hit end-of-file.
    pub bytes_read: u64,
}

/// Successful write (WIT `write-result`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WriteResult {
    /// Number of bytes written from the buffer.
    pub bytes_written: u64,
}

/// Completion payload of `open`.
pub type OpenCompletion = Result<Box<dyn FileHost>, FsError>;
/// Completion payload of `open-exec`.
pub type OpenExecCompletion = Result<Box<dyn ImmutableHost>, FsError>;
/// Completion payload of `list-directory`.
pub type ListDirectoryCompletion = Result<Vec<String>, FsError>;
/// Completion payload of `stat`.
pub type StatCompletion = Result<NodeStat, FsError>;
/// Completion payload of `create-directory` and `remove`.
pub type UnitCompletion = Result<(), FsError>;
/// Completion payload of file / exec reads: the buffer comes back on success and error.
pub type FileReadCompletion = (OwnedBuffer, Result<ReadResult, FsError>);
/// Completion payload of file writes: the buffer comes back on success and error.
pub type FileWriteCompletion = (OwnedBuffer, Result<WriteResult, FsError>);

/// The host trait mirroring the path operations of the WIT `eo9:fs/fs` interface
/// (minus `default`).
pub trait FsHost: Send + Sync {
    /// Open a file under the provider root.
    fn open(&self, path: &str, options: OpenFlags, complete: Completer<OpenCompletion>);
    /// Open a file for execution, yielding an immutable handle (see the module docs).
    fn open_exec(&self, path: &str, complete: Completer<OpenExecCompletion>);
    /// Names of the entries of a directory, sorted.
    fn list_directory(&self, path: &str, complete: Completer<ListDirectoryCompletion>);
    /// Metadata of a node (symlinks followed).
    fn stat(&self, path: &str, complete: Completer<StatCompletion>);
    /// Create a directory.
    fn create_directory(&self, path: &str, complete: Completer<UnitCompletion>);
    /// Remove a file, a symlink (the link itself, never its target), or an empty
    /// directory.
    fn remove(&self, path: &str, complete: Completer<UnitCompletion>);
}

/// The host trait mirroring the WIT `file` resource: offset-addressed owned-buffer I/O.
pub trait FileHost: Send + Sync {
    /// Read up to `dst.len()` bytes starting at `offset` into `dst`.
    fn read(&self, offset: u64, dst: OwnedBuffer, complete: Completer<FileReadCompletion>);
    /// Write the whole of `src` starting at `offset`, extending the file if needed.
    fn write(&self, offset: u64, src: OwnedBuffer, complete: Completer<FileWriteCompletion>);
}

/// The host trait mirroring the WIT `immutable-handle` resource.
pub trait ImmutableHost: Send + Sync {
    /// Size in bytes of the immutable file.
    fn size(&self) -> u64;
    /// Read up to `dst.len()` bytes starting at `offset` into `dst`.
    fn read(&self, offset: u64, dst: OwnedBuffer, complete: Completer<FileReadCompletion>);
}

/// The unix filesystem provider, rooted at a host directory. Corresponds to the WIT
/// `fs-impl` root handle.
pub struct FsProvider {
    /// Canonicalized root; every resolved path must stay under it.
    root: PathBuf,
    /// Where `open-exec` places its private copies before unlinking them.
    exec_copy_dir: PathBuf,
    pool: Arc<BlockingPool>,
}

impl FsProvider {
    /// A provider rooted at `root` (which must exist and be a directory), with exec
    /// copies placed in the system temp directory.
    pub fn new(root: impl AsRef<Path>, pool: Arc<BlockingPool>) -> io::Result<Self> {
        Self::with_exec_copy_dir(root, std::env::temp_dir(), pool)
    }

    /// A provider rooted at `root` with an explicit exec-copy directory (created if
    /// missing). The exec-copy directory should not live inside `root`, or the private
    /// copies would be briefly visible to guests while being written.
    pub fn with_exec_copy_dir(
        root: impl AsRef<Path>,
        exec_copy_dir: impl AsRef<Path>,
        pool: Arc<BlockingPool>,
    ) -> io::Result<Self> {
        let root = std::fs::canonicalize(root.as_ref())?;
        if !root.is_dir() {
            return Err(io::Error::new(
                io::ErrorKind::NotADirectory,
                "fs provider root must be a directory",
            ));
        }
        let exec_copy_dir = exec_copy_dir.as_ref().to_path_buf();
        std::fs::create_dir_all(&exec_copy_dir)?;
        Ok(Self {
            root,
            exec_copy_dir,
            pool,
        })
    }

    /// The canonicalized host directory this provider is rooted at.
    pub fn root(&self) -> &Path {
        &self.root
    }
}

impl FsHost for FsProvider {
    fn open(&self, path: &str, options: OpenFlags, complete: Completer<OpenCompletion>) {
        let root = self.root.clone();
        let pool = Arc::clone(&self.pool);
        let path = path.to_owned();
        self.pool.submit(move || {
            let result = do_open(&root, &path, options, &pool)
                .map(|file| Box::new(file) as Box<dyn FileHost>);
            complete(result);
        });
    }

    fn open_exec(&self, path: &str, complete: Completer<OpenExecCompletion>) {
        let root = self.root.clone();
        let exec_copy_dir = self.exec_copy_dir.clone();
        let pool = Arc::clone(&self.pool);
        let path = path.to_owned();
        self.pool.submit(move || {
            let result = do_open_exec(&root, &exec_copy_dir, &path, &pool)
                .map(|exec| Box::new(exec) as Box<dyn ImmutableHost>);
            complete(result);
        });
    }

    fn list_directory(&self, path: &str, complete: Completer<ListDirectoryCompletion>) {
        let root = self.root.clone();
        let path = path.to_owned();
        self.pool
            .submit(move || complete(do_list_directory(&root, &path)));
    }

    fn stat(&self, path: &str, complete: Completer<StatCompletion>) {
        let root = self.root.clone();
        let path = path.to_owned();
        self.pool.submit(move || complete(do_stat(&root, &path)));
    }

    fn create_directory(&self, path: &str, complete: Completer<UnitCompletion>) {
        let root = self.root.clone();
        let path = path.to_owned();
        self.pool
            .submit(move || complete(do_create_directory(&root, &path)));
    }

    fn remove(&self, path: &str, complete: Completer<UnitCompletion>) {
        let root = self.root.clone();
        let path = path.to_owned();
        self.pool.submit(move || complete(do_remove(&root, &path)));
    }
}

/// An open file under the provider root.
struct FsFile {
    file: Arc<File>,
    readable: bool,
    writable: bool,
    pool: Arc<BlockingPool>,
}

impl FileHost for FsFile {
    fn read(&self, offset: u64, mut dst: OwnedBuffer, complete: Completer<FileReadCompletion>) {
        if !self.readable {
            complete((
                dst,
                Err(FsError::Io("file is not open for reading".to_owned())),
            ));
            return;
        }
        let file = Arc::clone(&self.file);
        self.pool.submit(move || {
            let result = read_at_filling(&file, offset, dst.as_mut_slice())
                .map(|bytes_read| ReadResult { bytes_read })
                .map_err(|err| io_to_fs(&err));
            complete((dst, result));
        });
    }

    fn write(&self, offset: u64, src: OwnedBuffer, complete: Completer<FileWriteCompletion>) {
        if !self.writable {
            complete((src, Err(FsError::ReadOnly)));
            return;
        }
        let file = Arc::clone(&self.file);
        self.pool.submit(move || {
            let result = file
                .write_all_at(src.as_slice(), offset)
                .map(|()| WriteResult {
                    bytes_written: src.len(),
                })
                .map_err(|err| io_to_fs(&err));
            complete((src, result));
        });
    }
}

/// A file opened for execution: an anonymous (unlinked) private copy of the source,
/// reachable only through this handle's descriptor.
struct ExecFile {
    copy: Arc<File>,
    size: u64,
    pool: Arc<BlockingPool>,
}

impl ImmutableHost for ExecFile {
    fn size(&self) -> u64 {
        self.size
    }

    fn read(&self, offset: u64, mut dst: OwnedBuffer, complete: Completer<FileReadCompletion>) {
        let file = Arc::clone(&self.copy);
        self.pool.submit(move || {
            let result = read_at_filling(&file, offset, dst.as_mut_slice())
                .map(|bytes_read| ReadResult { bytes_read })
                .map_err(|err| io_to_fs(&err));
            complete((dst, result));
        });
    }
}

// ---------------------------------------------------------------------------
// Blocking operation bodies (run on the pool)
// ---------------------------------------------------------------------------

fn do_open(
    root: &Path,
    path: &str,
    options: OpenFlags,
    pool: &Arc<BlockingPool>,
) -> Result<FsFile, FsError> {
    let resolved = resolve_full(root, path)?;
    if resolved.is_dir() {
        return Err(FsError::IsADirectory);
    }
    let file = OpenOptions::new()
        .read(options.read)
        .write(options.write)
        .create(options.create)
        .truncate(options.truncate)
        .open(&resolved)
        .map_err(|err| io_to_fs(&err))?;
    Ok(FsFile {
        file: Arc::new(file),
        readable: options.read,
        writable: options.write,
        pool: Arc::clone(pool),
    })
}

static EXEC_COPY_ID: AtomicU64 = AtomicU64::new(0);

fn do_open_exec(
    root: &Path,
    exec_copy_dir: &Path,
    path: &str,
    pool: &Arc<BlockingPool>,
) -> Result<ExecFile, FsError> {
    let resolved = resolve_full(root, path)?;
    let metadata = std::fs::metadata(&resolved).map_err(|err| io_to_fs(&err))?;
    if metadata.is_dir() {
        return Err(FsError::IsADirectory);
    }
    if !metadata.is_file() {
        return Err(FsError::Io(
            "open-exec target is not a regular file".to_owned(),
        ));
    }
    let mut source = File::open(&resolved).map_err(|err| io_to_fs(&err))?;

    // Copy-on-open: snapshot the source into a fresh private file, then unlink it so the
    // copy has no name and only this handle's descriptor can reach it.
    let (mut copy, copy_path) = create_exec_copy_target(exec_copy_dir)?;
    let copied = io::copy(&mut source, &mut copy);
    let unlinked = std::fs::remove_file(&copy_path);
    let copied = copied.map_err(|err| io_to_fs(&err))?;
    unlinked.map_err(|err| io_to_fs(&err))?;
    Ok(ExecFile {
        copy: Arc::new(copy),
        size: copied,
        pool: Arc::clone(pool),
    })
}

fn create_exec_copy_target(exec_copy_dir: &Path) -> Result<(File, PathBuf), FsError> {
    loop {
        let name = format!(
            "eo9-exec-{}-{}",
            std::process::id(),
            EXEC_COPY_ID.fetch_add(1, Ordering::Relaxed)
        );
        let path = exec_copy_dir.join(name);
        match OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .open(&path)
        {
            Ok(file) => return Ok((file, path)),
            Err(err) if err.kind() == io::ErrorKind::AlreadyExists => {}
            Err(err) => return Err(io_to_fs(&err)),
        }
    }
}

fn do_list_directory(root: &Path, path: &str) -> Result<Vec<String>, FsError> {
    let resolved = resolve_full(root, path)?;
    let entries = std::fs::read_dir(&resolved).map_err(|err| io_to_fs(&err))?;
    let mut names = Vec::new();
    for entry in entries {
        let entry = entry.map_err(|err| io_to_fs(&err))?;
        names.push(entry.file_name().to_string_lossy().into_owned());
    }
    // Host readdir order is arbitrary; a sorted listing is deterministic and friendlier
    // to reproducible runs.
    names.sort();
    Ok(names)
}

fn do_stat(root: &Path, path: &str) -> Result<NodeStat, FsError> {
    let resolved = resolve_full(root, path)?;
    let metadata = std::fs::metadata(&resolved).map_err(|err| io_to_fs(&err))?;
    if metadata.is_dir() {
        Ok(NodeStat {
            kind: NodeKind::Directory,
            size: 0,
        })
    } else if metadata.is_file() {
        Ok(NodeStat {
            kind: NodeKind::File,
            size: metadata.len(),
        })
    } else {
        Err(FsError::Io("unsupported node kind".to_owned()))
    }
}

fn do_create_directory(root: &Path, path: &str) -> Result<(), FsError> {
    let resolved = resolve_full(root, path)?;
    std::fs::create_dir(&resolved).map_err(|err| io_to_fs(&err))
}

fn do_remove(root: &Path, path: &str) -> Result<(), FsError> {
    // Resolve the parent only: `remove` acts on the final component itself, so a symlink
    // is removed as a link and never followed to its target.
    let resolved = resolve_parent(root, path)?;
    let metadata = std::fs::symlink_metadata(&resolved).map_err(|err| io_to_fs(&err))?;
    if metadata.is_dir() {
        std::fs::remove_dir(&resolved).map_err(|err| io_to_fs(&err))
    } else {
        std::fs::remove_file(&resolved).map_err(|err| io_to_fs(&err))
    }
}

fn read_at_filling(file: &File, offset: u64, dst: &mut [u8]) -> io::Result<u64> {
    let mut filled = 0usize;
    while filled < dst.len() {
        match file.read_at(&mut dst[filled..], offset + filled as u64) {
            Ok(0) => break,
            Ok(n) => filled += n,
            Err(err) if err.kind() == io::ErrorKind::Interrupted => {}
            Err(err) => return Err(err),
        }
    }
    Ok(filled as u64)
}

// ---------------------------------------------------------------------------
// Path resolution
// ---------------------------------------------------------------------------

/// Lexical normalization: a guest path becomes a relative path with only normal
/// components. `.` is dropped, a leading `/` means the provider root, and `..` (or a
/// platform prefix) is refused outright.
fn normalize(path: &str) -> Result<PathBuf, FsError> {
    let mut normalized = PathBuf::new();
    for component in Path::new(path).components() {
        match component {
            Component::Normal(part) => normalized.push(part),
            Component::CurDir | Component::RootDir => {}
            Component::ParentDir | Component::Prefix(_) => return Err(FsError::Denied),
        }
    }
    Ok(normalized)
}

/// Resolve a guest path to a host path, following symlinks, and require the result to
/// stay under `root`. For paths that do not exist yet (creation targets), the parent is
/// resolved instead and the final name re-attached.
fn resolve_full(root: &Path, path: &str) -> Result<PathBuf, FsError> {
    let joined = root.join(normalize(path)?);
    match std::fs::symlink_metadata(&joined) {
        Ok(_) => {
            let canonical = std::fs::canonicalize(&joined).map_err(|err| io_to_fs(&err))?;
            if canonical.starts_with(root) {
                Ok(canonical)
            } else {
                Err(FsError::Denied)
            }
        }
        Err(err) if err.kind() == io::ErrorKind::NotFound => {
            let name = joined.file_name().ok_or(FsError::NotFound)?.to_owned();
            let parent = joined.parent().unwrap_or(root);
            let canonical_parent = std::fs::canonicalize(parent).map_err(|err| io_to_fs(&err))?;
            if canonical_parent.starts_with(root) {
                Ok(canonical_parent.join(name))
            } else {
                Err(FsError::Denied)
            }
        }
        Err(err) => Err(io_to_fs(&err)),
    }
}

/// Resolve a guest path without following its final component: the parent is
/// canonicalized (and must stay under `root`), the final name is re-attached untouched.
/// Used by `remove`, which must act on a symlink itself rather than its target.
fn resolve_parent(root: &Path, path: &str) -> Result<PathBuf, FsError> {
    let joined = root.join(normalize(path)?);
    // Refuse to treat the root itself as a removable final component.
    let name = joined.file_name().ok_or(FsError::Denied)?.to_owned();
    let parent = joined.parent().unwrap_or(root);
    let canonical_parent = std::fs::canonicalize(parent).map_err(|err| io_to_fs(&err))?;
    if canonical_parent.starts_with(root) {
        Ok(canonical_parent.join(name))
    } else {
        Err(FsError::Denied)
    }
}

fn io_to_fs(err: &io::Error) -> FsError {
    match err.kind() {
        io::ErrorKind::NotFound => FsError::NotFound,
        io::ErrorKind::AlreadyExists => FsError::AlreadyExists,
        io::ErrorKind::NotADirectory => FsError::NotADirectory,
        io::ErrorKind::IsADirectory => FsError::IsADirectory,
        io::ErrorKind::PermissionDenied => FsError::Denied,
        io::ErrorKind::StorageFull | io::ErrorKind::QuotaExceeded => FsError::NoSpace,
        io::ErrorKind::ReadOnlyFilesystem => FsError::ReadOnly,
        _ => FsError::Io(err.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::completion::completer;
    use crate::testutil::TempDir;
    use std::sync::mpsc;
    use std::time::Duration;

    const TIMEOUT: Duration = Duration::from_secs(10);

    struct Fixture {
        /// Keeps the provider root directory alive for the test's duration.
        _root: TempDir,
        /// Keeps the exec-copy directory alive for the test's duration.
        _exec: TempDir,
        provider: FsProvider,
    }

    fn fixture() -> Fixture {
        let root = TempDir::new();
        let exec = TempDir::new();
        let pool = Arc::new(BlockingPool::new(2));
        let provider = FsProvider::with_exec_copy_dir(root.path(), exec.path(), pool).unwrap();
        Fixture {
            _root: root,
            _exec: exec,
            provider,
        }
    }

    fn wait<T: Send + 'static>(register: impl FnOnce(Completer<T>)) -> T {
        let (tx, rx) = mpsc::channel();
        register(completer(move |value| tx.send(value).unwrap()));
        rx.recv_timeout(TIMEOUT).unwrap()
    }

    fn open(provider: &FsProvider, path: &str, options: OpenFlags) -> OpenCompletion {
        wait(|done| provider.open(path, options, done))
    }

    fn open_exec(provider: &FsProvider, path: &str) -> OpenExecCompletion {
        wait(|done| provider.open_exec(path, done))
    }

    fn stat(provider: &FsProvider, path: &str) -> StatCompletion {
        wait(|done| provider.stat(path, done))
    }

    fn list(provider: &FsProvider, path: &str) -> ListDirectoryCompletion {
        wait(|done| provider.list_directory(path, done))
    }

    fn read_file(file: &dyn FileHost, offset: u64, len: u64) -> FileReadCompletion {
        wait(|done| file.read(offset, OwnedBuffer::new(len), done))
    }

    fn write_file(file: &dyn FileHost, offset: u64, bytes: &[u8]) -> FileWriteCompletion {
        wait(|done| file.write(offset, OwnedBuffer::from_vec(bytes.to_vec()), done))
    }

    fn read_exec(handle: &dyn ImmutableHost, offset: u64, len: u64) -> FileReadCompletion {
        wait(|done| handle.read(offset, OwnedBuffer::new(len), done))
    }

    const RW_CREATE: OpenFlags = OpenFlags {
        read: true,
        write: true,
        create: true,
        truncate: false,
    };
    const RO: OpenFlags = OpenFlags {
        read: true,
        write: false,
        create: false,
        truncate: false,
    };

    #[test]
    fn create_write_read_round_trip() {
        let fx = fixture();
        let file = open(&fx.provider, "notes.txt", RW_CREATE).unwrap();

        let (_, result) = write_file(file.as_ref(), 0, b"hello filesystem");
        assert_eq!(result.unwrap().bytes_written, 16);
        let (_, result) = write_file(file.as_ref(), 6, b"eo9 basefs");
        assert_eq!(result.unwrap().bytes_written, 10);

        let (buf, result) = read_file(file.as_ref(), 0, 16);
        assert_eq!(result.unwrap().bytes_read, 16);
        assert_eq!(buf.as_slice(), b"hello eo9 basefs");

        // Reading past end-of-file returns a short count, not an error.
        let (_, result) = read_file(file.as_ref(), 12, 32);
        assert_eq!(result.unwrap().bytes_read, 4);
        let (_, result) = read_file(file.as_ref(), 100, 8);
        assert_eq!(result.unwrap().bytes_read, 0);

        assert_eq!(
            stat(&fx.provider, "notes.txt").unwrap(),
            NodeStat {
                kind: NodeKind::File,
                size: 16
            }
        );
    }

    #[test]
    fn directories_can_be_created_listed_statted_and_removed() {
        let fx = fixture();
        wait(|done| fx.provider.create_directory("sub", done)).unwrap();
        wait(|done| fx.provider.create_directory("sub/inner", done)).unwrap();
        open(&fx.provider, "sub/b.txt", RW_CREATE).unwrap();
        open(&fx.provider, "sub/a.txt", RW_CREATE).unwrap();

        assert_eq!(
            stat(&fx.provider, "sub").unwrap(),
            NodeStat {
                kind: NodeKind::Directory,
                size: 0
            }
        );
        assert_eq!(stat(&fx.provider, "/").unwrap().kind, NodeKind::Directory);
        assert_eq!(
            list(&fx.provider, "sub").unwrap(),
            vec!["a.txt", "b.txt", "inner"]
        );
        assert_eq!(list(&fx.provider, "").unwrap(), vec!["sub"]);

        // Duplicate creation and non-empty removal are errors.
        assert_eq!(
            wait(|done| fx.provider.create_directory("sub", done)).unwrap_err(),
            FsError::AlreadyExists
        );
        assert!(matches!(
            wait(|done| fx.provider.remove("sub", done)).unwrap_err(),
            FsError::Io(_)
        ));

        wait(|done| fx.provider.remove("sub/a.txt", done)).unwrap();
        wait(|done| fx.provider.remove("sub/b.txt", done)).unwrap();
        wait(|done| fx.provider.remove("sub/inner", done)).unwrap();
        wait(|done| fx.provider.remove("sub", done)).unwrap();
        assert_eq!(stat(&fx.provider, "sub").unwrap_err(), FsError::NotFound);
    }

    #[test]
    fn missing_paths_and_directory_misuse_are_reported() {
        let fx = fixture();
        assert_eq!(stat(&fx.provider, "ghost").unwrap_err(), FsError::NotFound);
        assert_eq!(
            open(&fx.provider, "ghost", RO).err().unwrap(),
            FsError::NotFound
        );
        assert_eq!(list(&fx.provider, "ghost").unwrap_err(), FsError::NotFound);
        assert_eq!(
            open(&fx.provider, "nodir/child.txt", RW_CREATE)
                .err()
                .unwrap(),
            FsError::NotFound
        );

        wait(|done| fx.provider.create_directory("adir", done)).unwrap();
        assert_eq!(
            open(&fx.provider, "adir", RO).err().unwrap(),
            FsError::IsADirectory
        );
        assert_eq!(
            open_exec(&fx.provider, "adir").err().unwrap(),
            FsError::IsADirectory
        );

        open(&fx.provider, "afile", RW_CREATE).unwrap();
        assert!(matches!(
            list(&fx.provider, "afile").unwrap_err(),
            FsError::NotADirectory | FsError::Io(_)
        ));
    }

    #[test]
    fn escaping_paths_are_denied() {
        let fx = fixture();
        assert_eq!(
            open(&fx.provider, "../escape", RO).err().unwrap(),
            FsError::Denied
        );
        assert_eq!(
            stat(&fx.provider, "a/../../b").unwrap_err(),
            FsError::Denied
        );
        assert_eq!(
            wait(|done| fx.provider.remove("..", done)).unwrap_err(),
            FsError::Denied
        );
        // An absolute path is interpreted relative to the provider root, never the host
        // root: /etc/passwd exists on the host but not under the provider root.
        assert_eq!(
            open(&fx.provider, "/etc/passwd", RO).err().unwrap(),
            FsError::NotFound
        );
    }

    #[test]
    fn symlinks_inside_the_root_work_but_may_not_escape() {
        let fx = fixture();
        let outside = TempDir::new();
        std::fs::write(outside.path().join("secret"), b"host secret").unwrap();

        // A link to a sibling inside the root is followed normally.
        let file = open(&fx.provider, "real.txt", RW_CREATE).unwrap();
        write_file(file.as_ref(), 0, b"inside").1.unwrap();
        std::os::unix::fs::symlink(
            fx.provider.root().join("real.txt"),
            fx.provider.root().join("alias"),
        )
        .unwrap();
        let (buf, result) = read_file(open(&fx.provider, "alias", RO).unwrap().as_ref(), 0, 6);
        assert_eq!(result.unwrap().bytes_read, 6);
        assert_eq!(buf.as_slice(), b"inside");

        // A link that points outside the root is refused for reads and writes.
        std::os::unix::fs::symlink(
            outside.path().join("secret"),
            fx.provider.root().join("sneaky"),
        )
        .unwrap();
        assert_eq!(
            open(&fx.provider, "sneaky", RO).err().unwrap(),
            FsError::Denied
        );
        assert_eq!(stat(&fx.provider, "sneaky").unwrap_err(), FsError::Denied);
        assert_eq!(
            open_exec(&fx.provider, "sneaky").err().unwrap(),
            FsError::Denied
        );

        // Removing a symlink removes the link itself, never the target.
        wait(|done| fx.provider.remove("alias", done)).unwrap();
        assert!(fx.provider.root().join("real.txt").exists());
        wait(|done| fx.provider.remove("sneaky", done)).unwrap();
        assert!(outside.path().join("secret").exists());
    }

    #[test]
    fn read_only_and_write_only_files_reject_the_other_direction() {
        let fx = fixture();
        let file = open(&fx.provider, "data", RW_CREATE).unwrap();
        write_file(file.as_ref(), 0, b"fixed").1.unwrap();

        let read_only = open(&fx.provider, "data", RO).unwrap();
        let (buf, result) = write_file(read_only.as_ref(), 0, b"nope");
        assert_eq!(result.unwrap_err(), FsError::ReadOnly);
        assert_eq!(buf.as_slice(), b"nope");

        let write_only = open(
            &fx.provider,
            "data",
            OpenFlags {
                read: false,
                write: true,
                create: false,
                truncate: false,
            },
        )
        .unwrap();
        let (_, result) = read_file(write_only.as_ref(), 0, 5);
        assert!(matches!(result.unwrap_err(), FsError::Io(_)));
    }

    #[test]
    fn open_exec_snapshots_are_immune_to_later_modification() {
        let fx = fixture();
        let file = open(&fx.provider, "prog.wasm", RW_CREATE).unwrap();
        write_file(file.as_ref(), 0, b"original image bytes")
            .1
            .unwrap();
        drop(file);

        let handle = open_exec(&fx.provider, "prog.wasm").unwrap();
        assert_eq!(handle.size(), 20);

        // Overwrite, truncate, and finally delete the original path.
        std::fs::write(fx.provider.root().join("prog.wasm"), b"tampered!").unwrap();
        std::fs::remove_file(fx.provider.root().join("prog.wasm")).unwrap();

        assert_eq!(handle.size(), 20);
        let (buf, result) = read_exec(handle.as_ref(), 0, 20);
        assert_eq!(result.unwrap().bytes_read, 20);
        assert_eq!(buf.as_slice(), b"original image bytes");

        // Offset reads and short reads at the end behave like ordinary file reads.
        let (buf, result) = read_exec(handle.as_ref(), 9, 64);
        assert_eq!(result.unwrap().bytes_read, 11);
        assert_eq!(&buf.as_slice()[..11], b"image bytes");
    }

    #[test]
    fn open_exec_leaves_nothing_behind_in_the_copy_directory() {
        let root = TempDir::new();
        let exec = TempDir::new();
        let pool = Arc::new(BlockingPool::new(2));
        let provider = FsProvider::with_exec_copy_dir(root.path(), exec.path(), pool).unwrap();

        std::fs::write(root.path().join("prog"), b"bytes").unwrap();
        let handle = wait(|done| provider.open_exec("prog", done)).unwrap();
        assert_eq!(handle.size(), 5);
        // The private copy was unlinked as soon as it was populated: the exec-copy
        // directory is empty even while the handle is still alive.
        assert_eq!(std::fs::read_dir(exec.path()).unwrap().count(), 0);
    }

    #[test]
    fn many_concurrent_file_operations_complete_correctly() {
        let fx = fixture();
        let file = open(&fx.provider, "blocks", RW_CREATE).unwrap();

        let (tx, rx) = mpsc::channel();
        for block in 0..64u64 {
            let tx = tx.clone();
            file.write(
                block * 16,
                OwnedBuffer::from_vec(vec![block as u8; 16]),
                completer(move |(_, result)| tx.send(result).unwrap()),
            );
        }
        drop(tx);
        for result in rx.iter() {
            assert_eq!(result.unwrap().bytes_written, 16);
        }

        let (tx, rx) = mpsc::channel();
        for block in 0..64u64 {
            let tx = tx.clone();
            file.read(
                block * 16,
                OwnedBuffer::new(16),
                completer(move |(buf, result)| tx.send((block, buf, result)).unwrap()),
            );
        }
        drop(tx);
        let mut completions = 0;
        for (block, buf, result) in rx.iter() {
            assert_eq!(result.unwrap().bytes_read, 16);
            assert_eq!(buf.as_slice(), &[block as u8; 16][..]);
            completions += 1;
        }
        assert_eq!(completions, 64);
    }

    #[test]
    fn provider_construction_requires_an_existing_directory() {
        let dir = TempDir::new();
        let pool = Arc::new(BlockingPool::new(1));
        assert!(FsProvider::new(dir.path().join("missing"), Arc::clone(&pool)).is_err());
        std::fs::write(dir.path().join("a-file"), b"x").unwrap();
        assert!(FsProvider::new(dir.path().join("a-file"), pool).is_err());
    }
}
