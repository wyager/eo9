//! The host-OS-backed provider backend.
//!
//! [`Host`] grants capabilities backed by the process's real environment via
//! `eo9-providers-unix`: text on the standard streams, the host's clocks, the OS RNG, and
//! — only when an fs root is configured — the host filesystem rooted at that directory.
//!
//! The completion-callback → future bridge here mirrors the one in the `eo9` binary
//! (`crates/eo9/src/providers.rs`): the unix providers are runtime-agnostic and report
//! completions through callbacks, while the runtime's provider traits speak in futures the
//! task's event loop polls, so each operation is bridged with a one-shot cell. The two
//! copies should be consolidated once the binary depends on `eo9-embed` (see
//! `plan/16-embed.md`); until then they are kept deliberately identical.

use std::collections::HashMap;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll, Waker};

use eo9_providers_unix::entropy::{EntropyHost, EntropyProvider as UnixEntropy};
use eo9_providers_unix::fs::{
    FileHost, FileReadCompletion, FileWriteCompletion, FsError as UnixFsError, FsHost,
    FsProvider as UnixFs, ImmutableHost, NodeKind as UnixNodeKind, OpenFlags as UnixOpenFlags,
};
use eo9_providers_unix::text::{
    OutputStream as UnixOutputStream, ReadLineCompletion, TextError as UnixTextError, TextHost,
    TextProvider as UnixText,
};
use eo9_providers_unix::time::{TimeHost, TimeProvider as UnixTime};
use eo9_providers_unix::{BlockingPool, OwnedBuffer, completer};
use eo9_runtime::providers::{BoxOp, FsError, FsHandle, FsProvider, NodeKind, NodeStat, OpenFlags};
use eo9_runtime::{Datetime, EntropyProvider, OutputStream, TextError, TextProvider, TimeProvider};

use crate::{EmbedError, Grants, ProviderSource, Roots};

/// Re-exported so embedders can choose the `open-exec` snapshot policy without depending on
/// `eo9-providers-unix` directly.
pub use eo9_providers_unix::fs::ExecSnapshotPolicy;

// ---------------------------------------------------------------------------------------
// The backend
// ---------------------------------------------------------------------------------------

/// Host-OS-backed provider backend. See the module docs.
///
/// The filesystem is granted only when a root directory is configured with
/// [`Host::with_fs_root`] — there is no ambient default. Requesting the fs capability
/// (`grant_fs(true)`) without a configured root is an error, mirroring the `eo9` CLI's
/// `--fs-root`-only policy.
#[derive(Debug, Clone, Default)]
pub struct Host {
    fs_root: Option<PathBuf>,
    exec_snapshot: ExecSnapshotPolicy,
}

impl Host {
    /// A host backend with no filesystem root (text/time/entropy only).
    pub fn new() -> Self {
        Self::default()
    }

    /// Root the filesystem capability at `root` (which must exist and be a directory).
    /// Guest paths can never escape it — the unix provider enforces containment.
    pub fn with_fs_root(mut self, root: impl Into<PathBuf>) -> Self {
        self.fs_root = Some(root.into());
        self
    }

    /// Set the `open-exec` snapshot policy (default [`ExecSnapshotPolicy::CloneOrRefuse`]).
    pub fn with_exec_snapshot(mut self, policy: ExecSnapshotPolicy) -> Self {
        self.exec_snapshot = policy;
        self
    }
}

impl ProviderSource for Host {
    fn roots(&self, grants: Grants) -> Result<Roots, EmbedError> {
        let fs: Option<Box<dyn FsProvider>> = if grants.fs {
            match &self.fs_root {
                Some(root) => Some(Box::new(HostFs::new(root, self.exec_snapshot)?)),
                None => {
                    return Err(EmbedError::Provider(
                        "the filesystem capability was granted but this Host backend has \
                         no fs root configured: build it with Host::new().with_fs_root(dir)"
                            .to_string(),
                    ));
                }
            }
        } else {
            None
        };
        Ok(Roots {
            text: grants.text.then(|| {
                Box::new(StdioText {
                    inner: UnixText::stdio(),
                }) as Box<dyn TextProvider>
            }),
            time: grants.time.then(|| {
                Box::new(HostTime {
                    inner: UnixTime::new(),
                }) as Box<dyn TimeProvider>
            }),
            entropy: grants.entropy.then(|| {
                Box::new(HostEntropy {
                    inner: UnixEntropy::new(),
                }) as Box<dyn EntropyProvider>
            }),
            fs,
        })
    }
}

// ---------------------------------------------------------------------------------------
// Completion-callback -> future bridge
// ---------------------------------------------------------------------------------------

struct OneshotState<T> {
    value: Option<T>,
    waker: Option<Waker>,
}

struct Oneshot<T> {
    state: Arc<Mutex<OneshotState<T>>>,
}

impl<T: Send + 'static> Future for Oneshot<T> {
    type Output = T;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<T> {
        let mut state = self.state.lock().unwrap();
        match state.value.take() {
            Some(value) => Poll::Ready(value),
            None => {
                state.waker = Some(cx.waker().clone());
                Poll::Pending
            }
        }
    }
}

/// A one-shot operation: the [`BoxOp`] future the runtime polls and the completion closure
/// handed to the provider. The unix providers guarantee exactly-once completion, so the
/// future can never be left dangling.
fn oneshot<T: Send + 'static>() -> (BoxOp<T>, impl FnOnce(T) + Send + 'static) {
    let state = Arc::new(Mutex::new(OneshotState {
        value: None,
        waker: None,
    }));
    let completion_state = Arc::clone(&state);
    let complete = move |value: T| {
        let waker = {
            let mut state = completion_state.lock().unwrap();
            state.value = Some(value);
            state.waker.take()
        };
        if let Some(waker) = waker {
            waker.wake();
        }
    };
    (Box::pin(Oneshot { state }), complete)
}

fn ready_op<T: Send + 'static>(value: T) -> BoxOp<T> {
    Box::pin(std::future::ready(value))
}

// ---------------------------------------------------------------------------------------
// Provider adapters
// ---------------------------------------------------------------------------------------

/// `eo9:text/text` backed by the process's standard streams.
struct StdioText {
    inner: UnixText,
}

impl TextProvider for StdioText {
    fn write(&mut self, to: OutputStream, text: &str) -> Result<(), TextError> {
        let stream = match to {
            OutputStream::Out => UnixOutputStream::Out,
            OutputStream::Err => UnixOutputStream::Err,
        };
        self.inner.write(stream, text).map_err(text_error)
    }

    fn read_line(&mut self) -> BoxOp<Result<Option<String>, TextError>> {
        let (op, complete) = oneshot();
        self.inner
            .read_line(completer(move |result: ReadLineCompletion| {
                complete(result.map_err(text_error));
            }));
        op
    }
}

fn text_error(err: UnixTextError) -> TextError {
    match err {
        UnixTextError::Closed => TextError::Closed,
        UnixTextError::Io(message) => TextError::Io(message),
    }
}

/// `eo9:time/time` backed by the host's real clocks.
struct HostTime {
    inner: UnixTime,
}

impl TimeProvider for HostTime {
    fn now(&mut self) -> Datetime {
        let now = self.inner.now();
        Datetime {
            seconds: now.seconds,
            nanoseconds: now.nanoseconds,
        }
    }

    fn monotonic_now(&mut self) -> u64 {
        self.inner.monotonic_now().nanoseconds
    }

    fn resolution(&mut self) -> u64 {
        self.inner.resolution()
    }

    fn sleep(&mut self, duration_ns: u64) -> BoxOp<()> {
        let (op, complete) = oneshot();
        self.inner
            .sleep(duration_ns, completer(move |()| complete(())));
        op
    }
}

/// `eo9:entropy/entropy` backed by the host OS RNG.
struct HostEntropy {
    inner: UnixEntropy,
}

impl EntropyProvider for HostEntropy {
    fn get_bytes(&mut self, len: u64) -> Vec<u8> {
        self.inner.get_bytes(len)
    }

    fn get_u64(&mut self) -> u64 {
        self.inner.get_u64()
    }
}

/// `eo9:fs/fs` backed by the unix filesystem provider, rooted at a host directory.
///
/// The adapter owns the handle tables: the unix provider hands back boxed handle objects
/// and the runtime's trait speaks in `u32` reps, so open operations park the boxed handle
/// keyed by a freshly allocated id and the close callbacks drop it again. Containment
/// (guest paths can never escape the root) is the unix provider's own guarantee.
struct HostFs {
    inner: UnixFs,
    files: Arc<Mutex<HashMap<FsHandle, Box<dyn FileHost>>>>,
    execs: Arc<Mutex<HashMap<FsHandle, Box<dyn ImmutableHost>>>>,
    next_handle: FsHandle,
}

impl HostFs {
    fn new(root: &Path, policy: ExecSnapshotPolicy) -> Result<Self, EmbedError> {
        let pool = Arc::new(BlockingPool::with_default_size());
        let inner = UnixFs::new(root, pool)
            .map_err(|err| {
                EmbedError::Provider(format!(
                    "cannot create the fs provider rooted at {}: {err}",
                    root.display()
                ))
            })?
            .with_exec_snapshot_policy(policy);
        Ok(Self {
            inner,
            files: Arc::new(Mutex::new(HashMap::new())),
            execs: Arc::new(Mutex::new(HashMap::new())),
            next_handle: 1,
        })
    }

    fn alloc_handle(&mut self) -> FsHandle {
        let handle = self.next_handle;
        self.next_handle += 1;
        handle
    }
}

fn fs_error(err: UnixFsError) -> FsError {
    match err {
        UnixFsError::NotFound => FsError::NotFound,
        UnixFsError::AlreadyExists => FsError::AlreadyExists,
        UnixFsError::NotADirectory => FsError::NotADirectory,
        UnixFsError::IsADirectory => FsError::IsADirectory,
        UnixFsError::Denied => FsError::Denied,
        UnixFsError::ReadOnly => FsError::ReadOnly,
        UnixFsError::NoSpace => FsError::NoSpace,
        UnixFsError::NotImmutable => FsError::NotImmutable,
        UnixFsError::Io(message) => FsError::Io(message),
    }
}

impl FsProvider for HostFs {
    fn open(&mut self, path: &str, flags: OpenFlags) -> BoxOp<Result<FsHandle, FsError>> {
        let handle = self.alloc_handle();
        let files = Arc::clone(&self.files);
        let (op, complete) = oneshot();
        self.inner.open(
            path,
            UnixOpenFlags {
                read: flags.read,
                write: flags.write,
                create: flags.create,
                truncate: flags.truncate,
            },
            completer(move |result: Result<Box<dyn FileHost>, UnixFsError>| {
                complete(match result {
                    Ok(file) => {
                        files.lock().unwrap().insert(handle, file);
                        Ok(handle)
                    }
                    Err(err) => Err(fs_error(err)),
                });
            }),
        );
        op
    }

    fn open_exec(&mut self, path: &str) -> BoxOp<Result<FsHandle, FsError>> {
        let handle = self.alloc_handle();
        let execs = Arc::clone(&self.execs);
        let (op, complete) = oneshot();
        self.inner.open_exec(
            path,
            completer(move |result: Result<Box<dyn ImmutableHost>, UnixFsError>| {
                complete(match result {
                    Ok(exec) => {
                        execs.lock().unwrap().insert(handle, exec);
                        Ok(handle)
                    }
                    Err(err) => Err(fs_error(err)),
                });
            }),
        );
        op
    }

    fn list_directory(&mut self, path: &str) -> BoxOp<Result<Vec<String>, FsError>> {
        let (op, complete) = oneshot();
        self.inner.list_directory(
            path,
            completer(move |result: Result<Vec<String>, UnixFsError>| {
                complete(result.map_err(fs_error));
            }),
        );
        op
    }

    fn stat(&mut self, path: &str) -> BoxOp<Result<NodeStat, FsError>> {
        let (op, complete) = oneshot();
        self.inner.stat(
            path,
            completer(
                move |result: Result<eo9_providers_unix::fs::NodeStat, UnixFsError>| {
                    complete(result.map_err(fs_error).map(|stat| NodeStat {
                        kind: match stat.kind {
                            UnixNodeKind::File => NodeKind::File,
                            UnixNodeKind::Directory => NodeKind::Directory,
                        },
                        size: stat.size,
                    }));
                },
            ),
        );
        op
    }

    fn create_directory(&mut self, path: &str) -> BoxOp<Result<(), FsError>> {
        let (op, complete) = oneshot();
        self.inner.create_directory(
            path,
            completer(move |result: Result<(), UnixFsError>| {
                complete(result.map_err(fs_error));
            }),
        );
        op
    }

    fn remove(&mut self, path: &str) -> BoxOp<Result<(), FsError>> {
        let (op, complete) = oneshot();
        self.inner.remove(
            path,
            completer(move |result: Result<(), UnixFsError>| {
                complete(result.map_err(fs_error));
            }),
        );
        op
    }

    fn read(
        &mut self,
        file: FsHandle,
        offset: u64,
        dst: Vec<u8>,
    ) -> BoxOp<(Vec<u8>, Result<u64, FsError>)> {
        let files = self.files.lock().unwrap();
        let Some(open_file) = files.get(&file) else {
            return ready_op((dst, Err(FsError::Io("unknown file handle".to_string()))));
        };
        let (op, complete) = oneshot();
        open_file.read(
            offset,
            OwnedBuffer::from_vec(dst),
            completer(move |(buffer, result): FileReadCompletion| {
                complete((
                    buffer.into_vec(),
                    result.map(|read| read.bytes_read).map_err(fs_error),
                ));
            }),
        );
        op
    }

    fn write(
        &mut self,
        file: FsHandle,
        offset: u64,
        src: Vec<u8>,
    ) -> BoxOp<(Vec<u8>, Result<u64, FsError>)> {
        let files = self.files.lock().unwrap();
        let Some(open_file) = files.get(&file) else {
            return ready_op((src, Err(FsError::Io("unknown file handle".to_string()))));
        };
        let (op, complete) = oneshot();
        open_file.write(
            offset,
            OwnedBuffer::from_vec(src),
            completer(move |(buffer, result): FileWriteCompletion| {
                complete((
                    buffer.into_vec(),
                    result.map(|write| write.bytes_written).map_err(fs_error),
                ));
            }),
        );
        op
    }

    fn exec_size(&mut self, handle: FsHandle) -> u64 {
        self.execs
            .lock()
            .unwrap()
            .get(&handle)
            .map(|exec| exec.size())
            .unwrap_or(0)
    }

    fn exec_read(
        &mut self,
        handle: FsHandle,
        offset: u64,
        dst: Vec<u8>,
    ) -> BoxOp<(Vec<u8>, Result<u64, FsError>)> {
        let execs = self.execs.lock().unwrap();
        let Some(exec) = execs.get(&handle) else {
            return ready_op((
                dst,
                Err(FsError::Io("unknown immutable handle".to_string())),
            ));
        };
        let (op, complete) = oneshot();
        exec.read(
            offset,
            OwnedBuffer::from_vec(dst),
            completer(move |(buffer, result): FileReadCompletion| {
                complete((
                    buffer.into_vec(),
                    result.map(|read| read.bytes_read).map_err(fs_error),
                ));
            }),
        );
        op
    }

    fn close_file(&mut self, file: FsHandle) {
        self.files.lock().unwrap().remove(&file);
    }

    fn close_exec(&mut self, handle: FsHandle) {
        self.execs.lock().unwrap().remove(&handle);
    }
}
