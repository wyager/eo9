//! Host-side provider traits for the root capabilities the runtime can wire into a task.
//!
//! These traits are the seam between the runtime (this crate) and whoever implements the
//! root capabilities — area 08's unix providers on the usermode path, the kernel's drivers
//! on bare metal, or the in-memory test providers below. They are deliberately small,
//! synchronous where the WIT is synchronous, and use plain `core::future::Future` (no
//! executor, no runtime types) where the WIT returns a `future<T>`: the runtime polls the
//! returned operation from the task's event loop, and the waker it passes *is* the task's
//! doorbell — completing the operation from another thread and waking that waker is all a
//! provider has to do.
//!
//! The shapes mirror `wit/text`, `wit/time`, `wit/entropy`, and `wit/fs` directly; see
//! plan/04-runtime.md § Decisions for the trait-surface rationale.

use std::future::Future;
use std::pin::Pin;

/// A pending provider operation: a plain boxed future, polled from the task's event loop.
///
/// The waker passed to `poll` is the owning task's doorbell; wake it (from any thread) when
/// the operation can make progress. If the task is killed the operation is dropped — the
/// provider's `Drop` impl is the place to abort or complete any underlying work.
pub type BoxOp<T> = Pin<Box<dyn Future<Output = T> + Send + 'static>>;

/// Which output stream a [`TextProvider::write`] targets (`eo9:text/text.output-stream`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OutputStream {
    /// Standard output.
    Out,
    /// Standard error.
    Err,
}

/// Error type for text operations (`eo9:text/text.text-error`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TextError {
    /// The stream is closed (output detached, or stdin hit end of input).
    Closed,
    /// Any other I/O failure.
    Io(String),
}

/// Wall-clock time (`eo9:time/time.datetime`): seconds and nanoseconds since the Unix epoch.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Datetime {
    pub seconds: i64,
    pub nanoseconds: u32,
}

/// Root provider for `eo9:text/text`.
pub trait TextProvider: Send + 'static {
    /// Write UTF-8 text to stdout or stderr.
    fn write(&mut self, to: OutputStream, text: &str) -> Result<(), TextError>;

    /// Read one line from stdin (without the trailing newline); `None` at end of input.
    fn read_line(&mut self) -> BoxOp<Result<Option<String>, TextError>>;
}

/// Root provider for `eo9:time/time`.
pub trait TimeProvider: Send + 'static {
    /// Current wall-clock time.
    fn now(&mut self) -> Datetime;

    /// Current monotonic time in nanoseconds since an arbitrary (per-boot) epoch.
    fn monotonic_now(&mut self) -> u64;

    /// The granularity of this clock in nanoseconds.
    fn resolution(&mut self) -> u64;

    /// Resolves once at least `duration_ns` nanoseconds of monotonic time have elapsed.
    fn sleep(&mut self, duration_ns: u64) -> BoxOp<()>;
}

/// Root provider for `eo9:entropy/entropy`.
pub trait EntropyProvider: Send + 'static {
    /// Return `len` random bytes.
    fn get_bytes(&mut self, len: u64) -> Vec<u8>;

    /// Return a single random 64-bit value.
    fn get_u64(&mut self) -> u64;
}

/// Identifier a filesystem provider assigns to an open `file` or `immutable-handle`.
/// It doubles as the Component Model resource `rep`, so it must stay unique for the life
/// of the handle; the runtime calls [`FsProvider::close_file`] / [`FsProvider::close_exec`]
/// when the guest drops the handle.
pub type FsHandle = u32;

/// Node kind (`eo9:fs/fs.node-kind`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NodeKind {
    File,
    Directory,
}

/// Node metadata (`eo9:fs/fs.node-stat`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NodeStat {
    pub kind: NodeKind,
    /// Size in bytes (0 for directories).
    pub size: u64,
}

/// Error type for filesystem operations (`eo9:fs/fs.fs-error`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FsError {
    NotFound,
    AlreadyExists,
    NotADirectory,
    IsADirectory,
    Denied,
    ReadOnly,
    NoSpace,
    NotImmutable,
    Io(String),
}

/// Open flags (`eo9:fs/fs.open-flags`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct OpenFlags {
    pub read: bool,
    pub write: bool,
    pub create: bool,
    pub truncate: bool,
}

/// Root provider for `eo9:fs/fs`.
///
/// File I/O follows the owned-buffer round-trip of the WIT: `read`/`write`/`exec_read`
/// take the buffer's bytes by value and give them back when the operation completes, on
/// both the success and the error path, so the provider has exclusive possession of the
/// bytes for the life of the operation.
pub trait FsProvider: Send + 'static {
    /// Open (or create) the file at `path`, yielding a provider-assigned handle.
    fn open(&mut self, path: &str, flags: OpenFlags) -> BoxOp<Result<FsHandle, FsError>>;

    /// Open the file at `path` *for execution*, yielding an immutable handle whose
    /// contents are stable for the life of the handle.
    fn open_exec(&mut self, path: &str) -> BoxOp<Result<FsHandle, FsError>>;

    /// Names of the entries of the directory at `path`.
    fn list_directory(&mut self, path: &str) -> BoxOp<Result<Vec<String>, FsError>>;

    /// Metadata of the node at `path`.
    fn stat(&mut self, path: &str) -> BoxOp<Result<NodeStat, FsError>>;

    /// Create a directory at `path`.
    fn create_directory(&mut self, path: &str) -> BoxOp<Result<(), FsError>>;

    /// Remove a file or an empty directory at `path`.
    fn remove(&mut self, path: &str) -> BoxOp<Result<(), FsError>>;

    /// Read from an open file at `offset` into `dst`, returning the buffer and the number
    /// of bytes read.
    fn read(
        &mut self,
        file: FsHandle,
        offset: u64,
        dst: Vec<u8>,
    ) -> BoxOp<(Vec<u8>, Result<u64, FsError>)>;

    /// Write `src` to an open file at `offset`, returning the buffer and the number of
    /// bytes written.
    fn write(
        &mut self,
        file: FsHandle,
        offset: u64,
        src: Vec<u8>,
    ) -> BoxOp<(Vec<u8>, Result<u64, FsError>)>;

    /// Size in bytes of an immutable handle.
    fn exec_size(&mut self, handle: FsHandle) -> u64;

    /// Read from an immutable handle at `offset` into `dst`.
    fn exec_read(
        &mut self,
        handle: FsHandle,
        offset: u64,
        dst: Vec<u8>,
    ) -> BoxOp<(Vec<u8>, Result<u64, FsError>)>;

    /// The guest dropped an open `file` handle.
    fn close_file(&mut self, file: FsHandle);

    /// The guest dropped an `immutable-handle`.
    fn close_exec(&mut self, handle: FsHandle);
}

/// Error type for block-device operations (the union of `eo9:disk/disk.read-error` and
/// `write-error`; the linker maps each value onto whichever variant the failing operation
/// declares).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DiskError {
    /// The backing device is gone.
    NotFound,
    /// The requested range does not lie entirely inside the device.
    OutOfRange,
    /// The device was opened read-only (writes only).
    ReadOnly,
    /// Any other I/O failure.
    Io(String),
}

/// Root provider for `eo9:disk/disk` — a raw block device, a flat span of bytes addressed
/// by offset. No filesystem semantics live here (that is `eo9:fs`'s business; the eofs
/// provider component is the bridge between the two).
///
/// Reads and writes follow the owned-buffer round-trip of the WIT: the operation takes the
/// buffer's bytes by value and gives them back when it completes, on both the success and
/// the error path, so the provider has exclusive possession of the bytes for the life of
/// the operation. An operation that would touch bytes outside the device fails with
/// [`DiskError::OutOfRange`] without touching the backing store.
pub trait DiskProvider: Send + 'static {
    /// Read `dst.len()` bytes starting at byte `offset` into `dst`, returning the buffer
    /// and the number of bytes read.
    fn read(&mut self, offset: u64, dst: Vec<u8>) -> BoxOp<(Vec<u8>, Result<u64, DiskError>)>;

    /// Write `src` starting at byte `offset`, returning the buffer and the number of
    /// bytes written.
    fn write(&mut self, offset: u64, src: Vec<u8>) -> BoxOp<(Vec<u8>, Result<u64, DiskError>)>;
}

/// The set of root providers wired into one task at spawn.
///
/// Every field is optional: a task's component is linked only against the interfaces it
/// imports, and an import with no corresponding provider is a spawn-time error (the
/// loader rule from SPEC.md "WASM runtime").
#[derive(Default)]
pub struct Providers {
    pub text: Option<Box<dyn TextProvider>>,
    pub time: Option<Box<dyn TimeProvider>>,
    pub entropy: Option<Box<dyn EntropyProvider>>,
    pub fs: Option<Box<dyn FsProvider>>,
    /// The raw block device (`eo9:disk`); usermode grants it only via an explicit
    /// `--disk <image>` (no ambient default), mirroring the fs grant posture.
    pub disk: Option<Box<dyn DiskProvider>>,
    /// The exec capability (component algebra + compile + task). Granting it makes the
    /// task a native executor; see [`crate::exec::ExecProvider`].
    pub exec: Option<crate::exec::ExecProvider>,
}

impl Providers {
    /// No providers at all (a task with no capabilities).
    pub fn none() -> Self {
        Self::default()
    }
}

// ---------------------------------------------------------------------------------------
// In-memory providers, for tests and deterministic runs inside this crate. The real root
// providers on the usermode path are area 08's (`eo9-providers-unix`).
// ---------------------------------------------------------------------------------------

/// In-memory text provider: captures writes, serves scripted stdin lines.
///
/// Cloning shares the underlying buffers, so a test can keep a clone and read what the
/// task wrote after the provider itself has been moved into the task.
#[derive(Default, Clone)]
pub struct CaptureText {
    /// Everything written to `out`, concatenated.
    pub out: std::sync::Arc<std::sync::Mutex<String>>,
    /// Everything written to `err`, concatenated.
    pub err: std::sync::Arc<std::sync::Mutex<String>>,
    /// Lines `read_line` will serve, in order; end of input afterwards.
    stdin: std::sync::Arc<std::sync::Mutex<std::collections::VecDeque<String>>>,
}

impl CaptureText {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_stdin(lines: impl IntoIterator<Item = String>) -> Self {
        let capture = Self::default();
        capture.stdin.lock().unwrap().extend(lines);
        capture
    }

    /// Everything written to `out` so far.
    pub fn stdout(&self) -> String {
        self.out.lock().unwrap().clone()
    }

    /// Everything written to `err` so far.
    pub fn stderr(&self) -> String {
        self.err.lock().unwrap().clone()
    }
}

impl TextProvider for CaptureText {
    fn write(&mut self, to: OutputStream, text: &str) -> Result<(), TextError> {
        match to {
            OutputStream::Out => self.out.lock().unwrap().push_str(text),
            OutputStream::Err => self.err.lock().unwrap().push_str(text),
        }
        Ok(())
    }

    fn read_line(&mut self) -> BoxOp<Result<Option<String>, TextError>> {
        let line = self.stdin.lock().unwrap().pop_front();
        Box::pin(std::future::ready(Ok(line)))
    }
}

/// Frozen clock: both clocks report a fixed instant; `sleep` resolves immediately.
#[derive(Debug, Clone, Copy)]
pub struct FrozenTime {
    pub now: Datetime,
    pub monotonic_ns: u64,
}

impl FrozenTime {
    pub fn new(now_seconds: i64, monotonic_ns: u64) -> Self {
        Self {
            now: Datetime {
                seconds: now_seconds,
                nanoseconds: 0,
            },
            monotonic_ns,
        }
    }
}

impl TimeProvider for FrozenTime {
    fn now(&mut self) -> Datetime {
        self.now
    }

    fn monotonic_now(&mut self) -> u64 {
        self.monotonic_ns
    }

    fn resolution(&mut self) -> u64 {
        1
    }

    fn sleep(&mut self, _duration_ns: u64) -> BoxOp<()> {
        Box::pin(std::future::ready(()))
    }
}

/// Deterministic PRNG from a fixed seed (splitmix64), for reproducible tests.
#[derive(Debug, Clone, Copy)]
pub struct SeededEntropy {
    state: u64,
}

impl SeededEntropy {
    pub fn new(seed: u64) -> Self {
        Self { state: seed }
    }

    fn next(&mut self) -> u64 {
        // splitmix64: tiny, dependency-free, and good enough for deterministic tests.
        self.state = self.state.wrapping_add(0x9e3779b97f4a7c15);
        let mut z = self.state;
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58476d1ce4e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d049bb133111eb);
        z ^ (z >> 31)
    }
}

impl EntropyProvider for SeededEntropy {
    fn get_bytes(&mut self, len: u64) -> Vec<u8> {
        let len = usize::try_from(len).unwrap_or(usize::MAX);
        let mut out = Vec::with_capacity(len);
        while out.len() < len {
            let chunk = self.next().to_le_bytes();
            let take = usize::min(8, len - out.len());
            out.extend_from_slice(&chunk[..take]);
        }
        out
    }

    fn get_u64(&mut self) -> u64 {
        self.next()
    }
}

/// In-memory filesystem provider: a deterministic scratch filesystem for tests (the
/// host-side analogue of the `fs.memfs` stub). Cloning shares the underlying state, so a
/// test can keep a clone to pre-populate files or inspect what the task wrote.
#[derive(Default, Clone)]
pub struct MemFs {
    inner: std::sync::Arc<std::sync::Mutex<MemFsInner>>,
}

#[derive(Default)]
struct MemFsInner {
    /// Path -> file contents. Directories are tracked separately; "/" always exists.
    files: std::collections::BTreeMap<String, Vec<u8>>,
    dirs: std::collections::BTreeSet<String>,
    /// Open file handles -> path.
    open_files: std::collections::BTreeMap<FsHandle, String>,
    /// Immutable (exec) handles -> snapshotted contents.
    exec_handles: std::collections::BTreeMap<FsHandle, Vec<u8>>,
    next_handle: FsHandle,
}

impl MemFs {
    pub fn new() -> Self {
        Self::default()
    }

    /// Pre-populate a file (for tests).
    pub fn insert_file(&self, path: &str, contents: impl Into<Vec<u8>>) {
        self.inner
            .lock()
            .unwrap()
            .files
            .insert(path.to_string(), contents.into());
    }

    /// Pre-create a directory (for tests).
    pub fn insert_dir(&self, path: &str) {
        self.inner.lock().unwrap().dirs.insert(path.to_string());
    }

    /// The current contents of a file, if it exists (for test assertions).
    pub fn file_contents(&self, path: &str) -> Option<Vec<u8>> {
        self.inner.lock().unwrap().files.get(path).cloned()
    }
}

impl MemFsInner {
    fn alloc_handle(&mut self) -> FsHandle {
        let handle = self.next_handle;
        self.next_handle += 1;
        handle
    }
}

fn ready<T: Send + 'static>(value: T) -> BoxOp<T> {
    Box::pin(std::future::ready(value))
}

impl FsProvider for MemFs {
    fn open(&mut self, path: &str, flags: OpenFlags) -> BoxOp<Result<FsHandle, FsError>> {
        let mut inner = self.inner.lock().unwrap();
        let result = if inner.dirs.contains(path) || path == "/" {
            Err(FsError::IsADirectory)
        } else if inner.files.contains_key(path) {
            if flags.truncate {
                inner.files.insert(path.to_string(), Vec::new());
            }
            let handle = inner.alloc_handle();
            inner.open_files.insert(handle, path.to_string());
            Ok(handle)
        } else if flags.create {
            inner.files.insert(path.to_string(), Vec::new());
            let handle = inner.alloc_handle();
            inner.open_files.insert(handle, path.to_string());
            Ok(handle)
        } else {
            Err(FsError::NotFound)
        };
        ready(result)
    }

    fn open_exec(&mut self, path: &str) -> BoxOp<Result<FsHandle, FsError>> {
        let mut inner = self.inner.lock().unwrap();
        let result = match inner.files.get(path).cloned() {
            // Snapshotting the contents gives the immutability the handle promises.
            Some(contents) => {
                let handle = inner.alloc_handle();
                inner.exec_handles.insert(handle, contents);
                Ok(handle)
            }
            None if inner.dirs.contains(path) => Err(FsError::IsADirectory),
            None => Err(FsError::NotFound),
        };
        ready(result)
    }

    fn list_directory(&mut self, path: &str) -> BoxOp<Result<Vec<String>, FsError>> {
        let inner = self.inner.lock().unwrap();
        let prefix = if path == "/" {
            "/".to_string()
        } else if inner.dirs.contains(path) {
            format!("{path}/")
        } else if inner.files.contains_key(path) {
            return ready(Err(FsError::NotADirectory));
        } else {
            return ready(Err(FsError::NotFound));
        };
        let mut entries: Vec<String> = Vec::new();
        for name in inner.files.keys().chain(inner.dirs.iter()) {
            if let Some(rest) = name.strip_prefix(&prefix)
                && !rest.is_empty()
                && !rest.contains('/')
                && !entries.contains(&rest.to_string())
            {
                entries.push(rest.to_string());
            }
        }
        ready(Ok(entries))
    }

    fn stat(&mut self, path: &str) -> BoxOp<Result<NodeStat, FsError>> {
        let inner = self.inner.lock().unwrap();
        let result = if let Some(contents) = inner.files.get(path) {
            Ok(NodeStat {
                kind: NodeKind::File,
                size: contents.len() as u64,
            })
        } else if inner.dirs.contains(path) || path == "/" {
            Ok(NodeStat {
                kind: NodeKind::Directory,
                size: 0,
            })
        } else {
            Err(FsError::NotFound)
        };
        ready(result)
    }

    fn create_directory(&mut self, path: &str) -> BoxOp<Result<(), FsError>> {
        let mut inner = self.inner.lock().unwrap();
        let result = if inner.dirs.contains(path) || inner.files.contains_key(path) || path == "/" {
            Err(FsError::AlreadyExists)
        } else {
            inner.dirs.insert(path.to_string());
            Ok(())
        };
        ready(result)
    }

    fn remove(&mut self, path: &str) -> BoxOp<Result<(), FsError>> {
        let mut inner = self.inner.lock().unwrap();
        let result = if inner.files.remove(path).is_some() || inner.dirs.remove(path) {
            Ok(())
        } else {
            Err(FsError::NotFound)
        };
        ready(result)
    }

    fn read(
        &mut self,
        file: FsHandle,
        offset: u64,
        mut dst: Vec<u8>,
    ) -> BoxOp<(Vec<u8>, Result<u64, FsError>)> {
        let inner = self.inner.lock().unwrap();
        let Some(path) = inner.open_files.get(&file) else {
            return ready((dst, Err(FsError::NotFound)));
        };
        let Some(contents) = inner.files.get(path) else {
            return ready((dst, Err(FsError::NotFound)));
        };
        let start = usize::try_from(offset)
            .unwrap_or(usize::MAX)
            .min(contents.len());
        let count = dst.len().min(contents.len() - start);
        dst[..count].copy_from_slice(&contents[start..start + count]);
        ready((dst, Ok(count as u64)))
    }

    fn write(
        &mut self,
        file: FsHandle,
        offset: u64,
        src: Vec<u8>,
    ) -> BoxOp<(Vec<u8>, Result<u64, FsError>)> {
        let mut inner = self.inner.lock().unwrap();
        let Some(path) = inner.open_files.get(&file).cloned() else {
            return ready((src, Err(FsError::NotFound)));
        };
        let Some(contents) = inner.files.get_mut(&path) else {
            return ready((src, Err(FsError::NotFound)));
        };
        let Ok(start) = usize::try_from(offset) else {
            return ready((src, Err(FsError::NoSpace)));
        };
        if contents.len() < start + src.len() {
            contents.resize(start + src.len(), 0);
        }
        contents[start..start + src.len()].copy_from_slice(&src);
        let written = src.len() as u64;
        ready((src, Ok(written)))
    }

    fn exec_size(&mut self, handle: FsHandle) -> u64 {
        self.inner
            .lock()
            .unwrap()
            .exec_handles
            .get(&handle)
            .map(|contents| contents.len() as u64)
            .unwrap_or(0)
    }

    fn exec_read(
        &mut self,
        handle: FsHandle,
        offset: u64,
        mut dst: Vec<u8>,
    ) -> BoxOp<(Vec<u8>, Result<u64, FsError>)> {
        let inner = self.inner.lock().unwrap();
        let Some(contents) = inner.exec_handles.get(&handle) else {
            return ready((dst, Err(FsError::NotFound)));
        };
        let start = usize::try_from(offset)
            .unwrap_or(usize::MAX)
            .min(contents.len());
        let count = dst.len().min(contents.len() - start);
        dst[..count].copy_from_slice(&contents[start..start + count]);
        ready((dst, Ok(count as u64)))
    }

    fn close_file(&mut self, file: FsHandle) {
        self.inner.lock().unwrap().open_files.remove(&file);
    }

    fn close_exec(&mut self, handle: FsHandle) {
        self.inner.lock().unwrap().exec_handles.remove(&handle);
    }
}
