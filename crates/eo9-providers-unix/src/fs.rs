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
//! `open-exec` returns an immutable handle by **snapshot-to-an-anonymous-file**: a
//! snapshot of the source lands in a freshly created, uniquely named file in the
//! provider's exec-copy directory, which is then immediately unlinked, leaving a file
//! reachable only through the provider's own descriptor. The default exec-copy
//! directory is an unpredictably named (`eo9-exec-<pid>-<random>`), owner-only `0o700`
//! subdirectory of the system temp dir, created fresh at provider construction — a
//! pre-existing path of that name is refused, never adopted. Both the default and any
//! caller-supplied exec-copy directory are vetted via `lstat` before use: they must be
//! real directories (not symlinks) owned by the current effective user.
//!
//! The snapshot is **clone-first**: the provider first attempts a zero-overhead COW
//! clone of the source — `clonefile(2)` on macOS/APFS, the `FICLONE` ioctl (reflink) on
//! Linux btrfs/XFS. What happens when the backing filesystem cannot clone is the
//! provider's [`ExecSnapshotPolicy`]:
//!
//! * [`CloneOrRefuse`](ExecSnapshotPolicy::CloneOrRefuse) (the default) — `open-exec`
//!   fails with `not-immutable`: only filesystems that can promise a point-in-time COW
//!   snapshot back execution. Note this includes the case where the exec-copy directory
//!   lies on a different filesystem/volume than the source (clones cannot cross
//!   filesystems), so keep the exec-copy directory on the same volume as the root.
//! * [`CloneOrCopy`](ExecSnapshotPolicy::CloneOrCopy) (opt-in) — fall back to a
//!   byte-for-byte copy. The copy is not atomic with respect to a writer actively
//!   modifying the source *during* `open-exec`: the handle is still immutable
//!   afterwards, but it may capture a torn intermediate state, and it costs
//!   O(file size) time and temp space per open.
//!
//! Snapshot files are owner-only: the copy path (and the Linux clone path) creates them
//! mode `0o600` atomically at `open(2)` time; the macOS clone path re-modes the clone to
//! `0o600` immediately after it is created, with the `0o700` exec-copy directory
//! covering that instant. So no other local user can read or tamper with the snapshot of
//! a program that is about to be executed, even between creation and unlink.
//!
//! Guarantee: once `open-exec` completes, the bytes observed through the handle cannot
//! change for the life of the handle — no rename, truncate, rewrite, or deletion of the
//! original path (by any process) affects it, and no other process can open the private
//! snapshot by name because it has none. Host superusers and the owner of the temp
//! directory's filesystem can, as always, reach the provider's memory or descriptors;
//! the guarantee is about well-behaved host filesystems, not a defense against a hostile
//! host root. `not-immutable` is returned exactly when the policy is `CloneOrRefuse` and
//! the backend cannot produce a COW clone.
//!
//! # Kill behavior
//!
//! In-flight operations are never aborted: they run to completion on a pool thread (a
//! write issued before a kill may still reach the backing file), the completer receives
//! the result — including the returned buffer — and a dead caller's runtime drops it.
//! Dropping the provider (and the pool) drains already-submitted operations first.

use std::fs::{File, OpenOptions};
use std::io;
use std::os::unix::fs::{DirBuilderExt, FileExt, OpenOptionsExt};
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

/// How `open-exec` obtains its immutable snapshot (see the module docs).
///
/// Not part of the WIT surface: this is host-side provider configuration, chosen by
/// whoever constructs the provider.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum ExecSnapshotPolicy {
    /// Take a zero-copy COW clone of the source; if the backing filesystem cannot
    /// clone, refuse `open-exec` with `not-immutable`. The default: execution is a
    /// property of the filesystem, and a backend that cannot clone does not get to
    /// pretend otherwise by silently copying.
    #[default]
    CloneOrRefuse,
    /// Take a COW clone where supported and fall back to a byte-for-byte copy
    /// otherwise (opt-in; the copy fallback has the torn-snapshot limitation described
    /// in the module docs).
    CloneOrCopy,
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
    /// Where `open-exec` places its private snapshots before unlinking them.
    exec_copy_dir: PathBuf,
    /// How `open-exec` snapshots are taken (clone-first; refuse or copy on fallback).
    exec_snapshot_policy: ExecSnapshotPolicy,
    pool: Arc<BlockingPool>,
}

impl FsProvider {
    /// A provider rooted at `root` (which must exist and be a directory), with exec
    /// snapshots placed in a freshly created, unpredictably named, owner-only (`0o700`)
    /// subdirectory of the system temp directory, and the default
    /// [`ExecSnapshotPolicy::CloneOrRefuse`] snapshot policy (snapshot files are always
    /// owner-only — see the module docs).
    ///
    /// Construction fails if that private directory cannot be created fresh (a
    /// pre-existing path of the same name is refused, never adopted) or does not verify
    /// as a real, owner-only directory owned by the current user.
    pub fn new(root: impl AsRef<Path>, pool: Arc<BlockingPool>) -> io::Result<Self> {
        let root = canonical_root(root.as_ref())?;
        let exec_copy_dir = create_private_exec_dir(&std::env::temp_dir())?;
        verify_exec_copy_dir(&exec_copy_dir, Some(0o700))?;
        Ok(Self {
            root,
            exec_copy_dir,
            exec_snapshot_policy: ExecSnapshotPolicy::default(),
            pool,
        })
    }

    /// A provider rooted at `root` with an explicit exec-copy directory (created mode
    /// `0o700` if missing). The directory — existing or freshly created — must be a
    /// real directory (not a symlink) owned by the current effective user, verified via
    /// `lstat`; its permission bits are otherwise the caller's choice. It should not
    /// live inside `root`, or the private snapshots would be briefly visible to guests
    /// while being taken, and should live on the same filesystem as `root`, or COW
    /// cloning will be unavailable.
    pub fn with_exec_copy_dir(
        root: impl AsRef<Path>,
        exec_copy_dir: impl AsRef<Path>,
        pool: Arc<BlockingPool>,
    ) -> io::Result<Self> {
        let root = canonical_root(root.as_ref())?;
        let exec_copy_dir = exec_copy_dir.as_ref().to_path_buf();
        // Owner-only for any directory we create ourselves; like create_dir_all, an
        // already-existing directory passes here and is then vetted below.
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(&exec_copy_dir)?;
        verify_exec_copy_dir(&exec_copy_dir, None)?;
        Ok(Self {
            root,
            exec_copy_dir,
            exec_snapshot_policy: ExecSnapshotPolicy::default(),
            pool,
        })
    }

    /// Sets how `open-exec` snapshots are taken (default:
    /// [`ExecSnapshotPolicy::CloneOrRefuse`]).
    pub fn with_exec_snapshot_policy(mut self, policy: ExecSnapshotPolicy) -> Self {
        self.exec_snapshot_policy = policy;
        self
    }

    /// The canonicalized host directory this provider is rooted at.
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// The snapshot policy `open-exec` uses.
    pub fn exec_snapshot_policy(&self) -> ExecSnapshotPolicy {
        self.exec_snapshot_policy
    }
}

/// Canonicalize the provider root and require it to be a directory.
fn canonical_root(root: &Path) -> io::Result<PathBuf> {
    let root = std::fs::canonicalize(root)?;
    if !root.is_dir() {
        return Err(io::Error::new(
            io::ErrorKind::NotADirectory,
            "fs provider root must be a directory",
        ));
    }
    Ok(root)
}

/// Create the default private exec-copy directory under `base`: an unpredictably named
/// (`eo9-exec-<pid>-<random hex>`), owner-only directory that must not already exist —
/// a pre-existing path (however it got there) is refused rather than adopted, so no
/// other local user can have planted it.
fn create_private_exec_dir(base: &Path) -> io::Result<PathBuf> {
    let mut suffix = [0u8; 8];
    getrandom::fill(&mut suffix).expect("host OS randomness source failed");
    let suffix: String = suffix.iter().map(|byte| format!("{byte:02x}")).collect();
    let dir = base.join(format!("eo9-exec-{}-{suffix}", std::process::id()));
    // Non-recursive: the system temp directory is assumed to exist, and an
    // already-existing directory of this name is an error, not something to reuse.
    std::fs::DirBuilder::new().mode(0o700).create(&dir)?;
    Ok(dir)
}

/// Vet an exec-copy directory before placing snapshots in it: it must be a real
/// directory (not a symlink — checked via `lstat`) owned by the current effective user,
/// and, when `require_mode` is given (the default directory we created ourselves), have
/// exactly those permission bits.
fn verify_exec_copy_dir(dir: &Path, require_mode: Option<u32>) -> io::Result<()> {
    use std::os::unix::fs::MetadataExt;

    let metadata = std::fs::symlink_metadata(dir)?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err(io::Error::other(format!(
            "exec-copy directory {} must be a real directory, not a symlink",
            dir.display()
        )));
    }
    let euid = unsafe { libc::geteuid() };
    if metadata.uid() != euid {
        return Err(io::Error::other(format!(
            "exec-copy directory {} is not owned by the current user",
            dir.display()
        )));
    }
    if let Some(mode) = require_mode
        && metadata.mode() & 0o7777 != mode
    {
        return Err(io::Error::other(format!(
            "exec-copy directory {} does not have mode {mode:o}",
            dir.display()
        )));
    }
    Ok(())
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
        let policy = self.exec_snapshot_policy;
        let pool = Arc::clone(&self.pool);
        let path = path.to_owned();
        self.pool.submit(move || {
            let result = do_open_exec(&root, &exec_copy_dir, policy, &path, &pool)
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

/// A file opened for execution: an anonymous (unlinked) private snapshot of the source
/// (a COW clone, or an opt-in byte copy), reachable only through this handle's
/// descriptor.
struct ExecFile {
    snapshot: Arc<File>,
    size: u64,
    pool: Arc<BlockingPool>,
}

impl ImmutableHost for ExecFile {
    fn size(&self) -> u64 {
        self.size
    }

    fn read(&self, offset: u64, mut dst: OwnedBuffer, complete: Completer<FileReadCompletion>) {
        let file = Arc::clone(&self.snapshot);
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
    policy: ExecSnapshotPolicy,
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

    // Clone-first: a zero-copy COW snapshot into a fresh private file, immediately
    // unlinked so only this handle's descriptor can reach it. If the backing filesystem
    // cannot clone, the policy decides between refusing and a byte-for-byte copy.
    let snapshot = match try_clone_snapshot(&source, exec_copy_dir)? {
        Some(clone) => clone,
        None => match policy {
            ExecSnapshotPolicy::CloneOrRefuse => return Err(FsError::NotImmutable),
            ExecSnapshotPolicy::CloneOrCopy => copy_snapshot(&mut source, exec_copy_dir)?,
        },
    };
    let size = snapshot.metadata().map_err(|err| io_to_fs(&err))?.len();
    Ok(ExecFile {
        snapshot: Arc::new(snapshot),
        size,
        pool: Arc::clone(pool),
    })
}

/// A unique file name for a private exec snapshot.
fn exec_snapshot_name() -> String {
    format!(
        "eo9-exec-{}-{}",
        std::process::id(),
        EXEC_COPY_ID.fetch_add(1, Ordering::Relaxed)
    )
}

/// Attempt a zero-copy COW clone of `source` into `exec_copy_dir`, returning the opened,
/// already-unlinked clone. `Ok(None)` means the backing filesystem cannot clone (not an
/// error: the policy decides what happens next); `Err` is a real I/O failure.
#[cfg(target_os = "macos")]
fn try_clone_snapshot(source: &File, exec_copy_dir: &Path) -> Result<Option<File>, FsError> {
    use std::os::fd::AsRawFd;
    use std::os::unix::ffi::OsStrExt;
    use std::os::unix::fs::PermissionsExt;

    loop {
        let path = exec_copy_dir.join(exec_snapshot_name());
        let c_path = std::ffi::CString::new(path.as_os_str().as_bytes())
            .map_err(|_| FsError::Io("exec snapshot path contains a NUL byte".to_owned()))?;
        // clonefile(2): an APFS copy-on-write clone of the already-open source fd.
        let rc =
            unsafe { libc::fclonefileat(source.as_raw_fd(), libc::AT_FDCWD, c_path.as_ptr(), 0) };
        if rc == 0 {
            let finish = || -> io::Result<File> {
                let snapshot = File::open(&path)?;
                // The clone inherits the source's mode; make it owner-only like every
                // other snapshot file. The 0o700 exec-copy directory covers the instant
                // between the clone appearing and this fchmod.
                snapshot.set_permissions(std::fs::Permissions::from_mode(0o600))?;
                std::fs::remove_file(&path)?;
                Ok(snapshot)
            };
            return match finish() {
                Ok(snapshot) => Ok(Some(snapshot)),
                Err(err) => {
                    let _ = std::fs::remove_file(&path);
                    Err(io_to_fs(&err))
                }
            };
        }
        let err = io::Error::last_os_error();
        match err.raw_os_error() {
            // Name collision (e.g. leftovers from a previous run): try the next name.
            Some(libc::EEXIST) => {}
            // The backend cannot clone: not APFS, or source and exec-copy dir are on
            // different volumes.
            Some(libc::ENOTSUP | libc::EOPNOTSUPP | libc::EXDEV) => return Ok(None),
            _ => return Err(io_to_fs(&err)),
        }
    }
}

/// See the macOS version; on Linux the clone is the `FICLONE` ioctl (reflink), issued on
/// a destination we create ourselves (so it is born mode `0o600`).
#[cfg(target_os = "linux")]
fn try_clone_snapshot(source: &File, exec_copy_dir: &Path) -> Result<Option<File>, FsError> {
    use std::os::fd::AsRawFd;

    let (snapshot, path) = create_exec_copy_target(exec_copy_dir)?;
    let rc = unsafe { libc::ioctl(snapshot.as_raw_fd(), libc::FICLONE, source.as_raw_fd()) };
    if rc == 0 {
        std::fs::remove_file(&path).map_err(|err| io_to_fs(&err))?;
        return Ok(Some(snapshot));
    }
    let err = io::Error::last_os_error();
    let _ = std::fs::remove_file(&path);
    match err.raw_os_error() {
        // The backend cannot reflink: not a COW filesystem, different filesystems, or a
        // kernel without FICLONE.
        Some(libc::EOPNOTSUPP | libc::EINVAL | libc::EXDEV | libc::ENOSYS | libc::ENOTTY) => {
            Ok(None)
        }
        _ => Err(io_to_fs(&err)),
    }
}

/// Fallback for unix hosts without a known clone primitive: cloning is never supported.
#[cfg(not(any(target_os = "macos", target_os = "linux")))]
fn try_clone_snapshot(_source: &File, _exec_copy_dir: &Path) -> Result<Option<File>, FsError> {
    Ok(None)
}

/// The opt-in byte-for-byte snapshot: copy the source into a fresh private file, then
/// unlink it so only the returned descriptor can reach it.
fn copy_snapshot(source: &mut File, exec_copy_dir: &Path) -> Result<File, FsError> {
    let (mut snapshot, path) = create_exec_copy_target(exec_copy_dir)?;
    let copied = io::copy(source, &mut snapshot);
    let unlinked = std::fs::remove_file(&path);
    copied.map_err(|err| io_to_fs(&err))?;
    unlinked.map_err(|err| io_to_fs(&err))?;
    Ok(snapshot)
}

fn create_exec_copy_target(exec_copy_dir: &Path) -> Result<(File, PathBuf), FsError> {
    loop {
        let path = exec_copy_dir.join(exec_snapshot_name());
        match OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            // Owner-only from the moment the file exists: another local user must never
            // be able to read (or, umask permitting, tamper with) the snapshot of a
            // program that is about to be executed.
            .mode(0o600)
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
        // Default policy (clone-or-refuse): on this APFS host the snapshot is a COW
        // clone, and succeeding at all proves the clone path was taken.
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
    fn exec_snapshots_and_their_default_directory_are_owner_only() {
        use std::os::unix::fs::PermissionsExt;

        // A missing exec-copy directory is created owner-only.
        let base = TempDir::new();
        let root = TempDir::new();
        let exec_dir = base.path().join("private-exec");
        let pool = Arc::new(BlockingPool::new(1));
        FsProvider::with_exec_copy_dir(root.path(), &exec_dir, Arc::clone(&pool)).unwrap();
        let dir_mode = std::fs::metadata(&exec_dir).unwrap().permissions().mode();
        assert_eq!(dir_mode & 0o777, 0o700);

        // A copy-path snapshot file is mode 0o600 from the moment it exists (checked via
        // fstat on the descriptor the provider keeps).
        let (copy, copy_path) = create_exec_copy_target(&exec_dir).unwrap();
        let file_mode = copy.metadata().unwrap().permissions().mode();
        assert_eq!(file_mode & 0o777, 0o600);
        std::fs::remove_file(copy_path).unwrap();

        // A full open-exec snapshot (the COW clone on this host) ends up owner-only too,
        // regardless of the source file's own mode (checked via fstat on the handle's
        // descriptor).
        std::fs::write(root.path().join("prog"), b"image").unwrap();
        let canonical_root = std::fs::canonicalize(root.path()).unwrap();
        let exec = do_open_exec(
            &canonical_root,
            &exec_dir,
            ExecSnapshotPolicy::CloneOrCopy,
            "prog",
            &pool,
        )
        .unwrap();
        let snapshot_mode = exec.snapshot.metadata().unwrap().permissions().mode();
        assert_eq!(snapshot_mode & 0o777, 0o600);
    }

    #[test]
    fn default_private_exec_dirs_are_fresh_unpredictable_and_owner_only() {
        use std::os::unix::fs::PermissionsExt;

        let base = TempDir::new();
        let first = create_private_exec_dir(base.path()).unwrap();
        let second = create_private_exec_dir(base.path()).unwrap();

        // Unpredictable: two creations never collide, and both carry the random suffix
        // after the pid prefix.
        assert_ne!(first, second);
        let prefix = format!("eo9-exec-{}-", std::process::id());
        for dir in [&first, &second] {
            let name = dir.file_name().unwrap().to_string_lossy();
            assert!(name.starts_with(&prefix));
            assert_eq!(name.len(), prefix.len() + 16);
            assert_eq!(
                std::fs::metadata(dir).unwrap().permissions().mode() & 0o777,
                0o700
            );
            verify_exec_copy_dir(dir, Some(0o700)).unwrap();
        }
    }

    #[test]
    fn symlinked_exec_copy_dirs_are_rejected() {
        let root = TempDir::new();
        let target = TempDir::new();
        let link_holder = TempDir::new();
        let link = link_holder.path().join("exec-link");
        std::os::unix::fs::symlink(target.path(), &link).unwrap();

        let pool = Arc::new(BlockingPool::new(1));
        // Even though the symlink points at a perfectly good directory we own, the
        // exec-copy directory itself must not be a symlink.
        assert!(FsProvider::with_exec_copy_dir(root.path(), &link, pool).is_err());
    }

    #[test]
    fn copy_fallback_snapshots_are_owner_only_unlinked_and_immutable() {
        use std::os::unix::fs::PermissionsExt;

        let dir = TempDir::new();
        let exec_dir = TempDir::new();
        std::fs::write(dir.path().join("src.bin"), b"copy me exactly").unwrap();
        let mut source = File::open(dir.path().join("src.bin")).unwrap();

        let snapshot = copy_snapshot(&mut source, exec_dir.path()).unwrap();
        // Unlinked immediately, owner-only, exact contents.
        assert_eq!(std::fs::read_dir(exec_dir.path()).unwrap().count(), 0);
        let mode = snapshot.metadata().unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
        assert_eq!(snapshot.metadata().unwrap().len(), 15);

        // Later modification of the source never reaches the snapshot.
        std::fs::write(dir.path().join("src.bin"), b"overwritten").unwrap();
        let mut contents = vec![0u8; 15];
        snapshot.read_exact_at(&mut contents, 0).unwrap();
        assert_eq!(&contents, b"copy me exactly");
    }

    #[test]
    fn clone_or_copy_policy_produces_immutable_handles_too() {
        let root = TempDir::new();
        let exec = TempDir::new();
        let pool = Arc::new(BlockingPool::new(2));
        let provider = FsProvider::with_exec_copy_dir(root.path(), exec.path(), pool)
            .unwrap()
            .with_exec_snapshot_policy(ExecSnapshotPolicy::CloneOrCopy);
        assert_eq!(
            provider.exec_snapshot_policy(),
            ExecSnapshotPolicy::CloneOrCopy
        );

        std::fs::write(root.path().join("prog"), b"fallback ok").unwrap();
        let handle = wait(|done| provider.open_exec("prog", done)).unwrap();
        std::fs::write(root.path().join("prog"), b"changed").unwrap();

        assert_eq!(handle.size(), 11);
        let (buf, result) = read_exec(handle.as_ref(), 0, 11);
        assert_eq!(result.unwrap().bytes_read, 11);
        assert_eq!(buf.as_slice(), b"fallback ok");
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
