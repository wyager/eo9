//! Adapters from the unix root providers onto the runtime's provider traits, plus the
//! blocking helper the built-in drive loop uses.
//!
//! The two library crates deliberately do not know about each other: `eo9-providers-unix`
//! is runtime-agnostic (plain structs, completion callbacks, no wasmtime types), and
//! `eo9-runtime`'s provider traits use plain futures polled from the task's event loop
//! (the waker is the task's doorbell). The glue lives here, in the embedder
//! (plan/11-usermode.md): each adapter (text, time, entropy, fs) wraps a unix provider
//! and bridges its callback-style completions into the runtime's [`BoxOp`] futures with a
//! one-shot cell.

use std::collections::HashMap;
use std::future::Future;
use std::path::Path;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll, Wake, Waker};

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
use eo9_runtime::{
    Datetime, EntropyProvider, OutputStream, Providers, Task, TextError, TextProvider, TimeProvider,
};

use crate::cli::Config;

// ---------------------------------------------------------------------------------------
// Completion-callback -> future bridge
// ---------------------------------------------------------------------------------------

struct OneshotState<T> {
    value: Option<T>,
    waker: Option<Waker>,
}

/// The future half of a one-shot bridge: resolves once the paired completer has run.
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

/// A one-shot operation: the [`BoxOp`] future the runtime polls, and the completion
/// closure handed to the provider. The unix providers guarantee exactly-once completion
/// (on the success and error path alike), so the future can never be left dangling.
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
/// The adapter owns the handle tables: the unix provider hands back `Box<dyn FileHost>` /
/// `Box<dyn ImmutableHost>` objects, and the runtime's trait speaks in `u32` handles (the
/// Component Model resource reps), so open operations park the boxed handle in a shared
/// map keyed by a freshly allocated id and the close callbacks drop it again. Containment
/// (guest paths can never escape the root) is the unix provider's own guarantee; nothing
/// here widens it.
struct HostFs {
    inner: UnixFs,
    files: Arc<Mutex<HashMap<FsHandle, Box<dyn FileHost>>>>,
    execs: Arc<Mutex<HashMap<FsHandle, Box<dyn ImmutableHost>>>>,
    next_handle: FsHandle,
}

impl HostFs {
    /// A provider rooted at `root` (which must exist and be a directory), with the given
    /// exec-snapshot policy for `open-exec`.
    fn new(
        root: &Path,
        policy: eo9_providers_unix::fs::ExecSnapshotPolicy,
    ) -> Result<Self, String> {
        let pool = Arc::new(BlockingPool::with_default_size());
        let inner = UnixFs::new(root, pool)
            .map_err(|err| {
                format!(
                    "cannot create the fs provider rooted at {}: {err}",
                    root.display()
                )
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

/// A ready operation, for the defensive unknown-handle paths.
fn ready_op<T: Send + 'static>(value: T) -> BoxOp<T> {
    Box::pin(std::future::ready(value))
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

/// The root providers of a usermode run: text on the process's standard streams, the
/// host's real clocks, the OS RNG, and — only when `--fs-root` was given — the host
/// filesystem rooted at that directory.
///
/// Handing these to `spawn` never widens a program's capability set: the runtime only
/// links the interfaces the component actually imports (the loader rule), and an import
/// with no provider is a spawn error. The fs capability is bounded by its root: the unix
/// provider refuses any path that would escape it.
pub fn root_providers(cfg: &Config) -> Result<Providers, String> {
    // The filesystem is granted only when the user names a root explicitly — there is no
    // ambient default (handing out, say, the current directory unasked would be ambient
    // authority). Without `--fs-root`, a required fs import is refused at spawn and an
    // optional one observes absence.
    let fs: Option<Box<dyn FsProvider>> = match &cfg.fs_root {
        Some(root) => Some(Box::new(HostFs::new(root, cfg.exec_snapshot)?)),
        None => None,
    };
    Ok(Providers {
        text: Some(Box::new(StdioText {
            inner: UnixText::stdio(),
        })),
        time: Some(Box::new(HostTime {
            inner: UnixTime::new(),
        })),
        entropy: Some(Box::new(HostEntropy {
            inner: UnixEntropy::new(),
        })),
        fs,
        exec: None,
    })
}

// ---------------------------------------------------------------------------------------
// Blocking until a task is runnable again
// ---------------------------------------------------------------------------------------

/// Wakes the driving thread when the task's doorbell rings.
struct ThreadWaker(std::thread::Thread);

impl Wake for ThreadWaker {
    fn wake(self: Arc<Self>) {
        self.0.unpark();
    }

    fn wake_by_ref(self: &Arc<Self>) {
        self.0.unpark();
    }
}

/// Block the calling thread until `task` can make progress again — that is, until a
/// provider completion rings its doorbell. Used by the built-in drive loop whenever
/// `resume` reports the task blocked on I/O.
pub fn wait_until_runnable(task: &Task) {
    let waker = Waker::from(Arc::new(ThreadWaker(std::thread::current())));
    let mut context = Context::from_waker(&waker);
    let runnable = task.runnable();
    let mut runnable = std::pin::pin!(runnable);
    while runnable.as_mut().poll(&mut context).is_pending() {
        std::thread::park();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn oneshot_resolves_after_completion_and_wakes_the_waker() {
        let (mut op, complete) = oneshot::<u32>();
        let woken = Arc::new(std::sync::atomic::AtomicBool::new(false));

        struct Flag(Arc<std::sync::atomic::AtomicBool>);
        impl Wake for Flag {
            fn wake(self: Arc<Self>) {
                self.0.store(true, std::sync::atomic::Ordering::SeqCst);
            }
        }

        let waker = Waker::from(Arc::new(Flag(Arc::clone(&woken))));
        let mut context = Context::from_waker(&waker);
        assert!(op.as_mut().poll(&mut context).is_pending());

        complete(17);
        assert!(woken.load(std::sync::atomic::Ordering::SeqCst));
        assert_eq!(op.as_mut().poll(&mut context), Poll::Ready(17));
    }

    #[test]
    fn oneshot_completed_before_first_poll_is_immediately_ready() {
        let (mut op, complete) = oneshot::<&'static str>();
        complete("done");
        let waker = Waker::from(Arc::new(ThreadWaker(std::thread::current())));
        let mut context = Context::from_waker(&waker);
        assert_eq!(op.as_mut().poll(&mut context), Poll::Ready("done"));
    }

    #[test]
    fn time_adapter_reports_monotonic_progress() {
        let mut time = HostTime {
            inner: UnixTime::new(),
        };
        let first = time.monotonic_now();
        let second = time.monotonic_now();
        assert!(second >= first);
        assert!(time.resolution() >= 1);
    }

    #[test]
    fn entropy_adapter_returns_requested_lengths() {
        let mut entropy = HostEntropy {
            inner: UnixEntropy::new(),
        };
        assert_eq!(entropy.get_bytes(16).len(), 16);
        let _ = entropy.get_u64();
    }

    /// Drive a provider operation to completion on the test thread.
    fn block_on<T>(op: BoxOp<T>) -> T {
        let waker = Waker::from(Arc::new(ThreadWaker(std::thread::current())));
        let mut context = Context::from_waker(&waker);
        let mut op = op;
        loop {
            match op.as_mut().poll(&mut context) {
                Poll::Ready(value) => return value,
                Poll::Pending => std::thread::park(),
            }
        }
    }

    /// A fresh scratch directory for one fs-adapter test.
    fn scratch_dir(test: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("eo9-hostfs-{test}-{}", std::process::id()));
        if dir.exists() {
            std::fs::remove_dir_all(&dir).unwrap();
        }
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn fs_adapter_round_trips_a_file_under_its_root() {
        let root = scratch_dir("roundtrip");
        let mut fs = HostFs::new(
            &root,
            eo9_providers_unix::fs::ExecSnapshotPolicy::CloneOrRefuse,
        )
        .unwrap();

        let flags = OpenFlags {
            read: true,
            write: true,
            create: true,
            truncate: true,
        };
        let file = block_on(fs.open("note.txt", flags)).expect("open should succeed");

        let (_, written) = block_on(fs.write(file, 0, b"hello adapter".to_vec()));
        assert_eq!(written.unwrap(), 13);

        let (buffer, read) = block_on(fs.read(file, 0, vec![0u8; 13]));
        assert_eq!(read.unwrap(), 13);
        assert_eq!(buffer, b"hello adapter");

        let stat = block_on(fs.stat("note.txt")).expect("stat should succeed");
        assert_eq!(stat.kind, NodeKind::File);
        assert_eq!(stat.size, 13);

        fs.close_file(file);
        block_on(fs.remove("note.txt")).expect("remove should succeed");
        assert_eq!(block_on(fs.stat("note.txt")), Err(FsError::NotFound));

        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn fs_adapter_keeps_the_root_contained() {
        let root = scratch_dir("contained");
        let mut fs = HostFs::new(
            &root,
            eo9_providers_unix::fs::ExecSnapshotPolicy::CloneOrRefuse,
        )
        .unwrap();

        let flags = OpenFlags {
            read: true,
            write: true,
            create: true,
            truncate: false,
        };
        assert_eq!(
            block_on(fs.open("../escaped.txt", flags)),
            Err(FsError::Denied)
        );
        assert!(!root.parent().unwrap().join("escaped.txt").exists());

        // An operation on a handle the adapter does not know is an error, never a panic.
        let (_, result) = block_on(fs.read(999, 0, vec![0u8; 4]));
        assert!(result.is_err());

        std::fs::remove_dir_all(&root).unwrap();
    }
}
