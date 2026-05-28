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
    ChildPolicy, Datetime, EntropyProvider, ExecProvider, Image, OutputStream, Providers, Task,
    TextError, TextProvider, TimeProvider,
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
/// (Also used by the interactive shell's text provider in `interactive.rs`.)
pub(crate) fn oneshot<T: Send + 'static>() -> (BoxOp<T>, impl FnOnce(T) + Send + 'static) {
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

// ---------------------------------------------------------------------------------------
// The session overlay filesystem
// ---------------------------------------------------------------------------------------

/// Which layer of a session overlay served a handle.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum OverlayLayer {
    Upper,
    Lower,
}

/// `eo9:fs/fs` as the layered session filesystem (SPEC.md "Overlay filesystems"), composed
/// host-side from two root providers: reads resolve in `upper` first and fall through to
/// `lower` on not-found, directory listings are the union of both layers (upper wins on a
/// name collision), and every mutation — write-opens, `create-directory`, `remove` — is
/// routed to `lower`; `upper` is never written through the overlay. Without a `lower`
/// layer the overlay is read-only and mutations report [`FsError::ReadOnly`].
///
/// This is the embedder-side counterpart of the guest `fs.overlay` provider: the session's
/// layers are themselves root providers (the `/bin` program view and the user's
/// `--fs-root`), which the OS core links directly, exactly as it links every other root
/// capability. Swapping in the guest `fs.overlay` component — so the same layering is also
/// available to programs purely through the algebra — is the recorded follow-up
/// (plan/11-usermode.md Decisions).
struct OverlayFs {
    upper: Arc<Mutex<Box<dyn FsProvider>>>,
    lower: Option<Arc<Mutex<Box<dyn FsProvider>>>>,
    /// Outer handle -> (serving layer, the layer's own handle).
    files: Arc<Mutex<HashMap<FsHandle, (OverlayLayer, FsHandle)>>>,
    execs: Arc<Mutex<HashMap<FsHandle, (OverlayLayer, FsHandle)>>>,
    next_handle: FsHandle,
}

impl OverlayFs {
    fn new(upper: Box<dyn FsProvider>, lower: Option<Box<dyn FsProvider>>) -> Self {
        OverlayFs {
            upper: Arc::new(Mutex::new(upper)),
            lower: lower.map(|fs| Arc::new(Mutex::new(fs))),
            files: Arc::new(Mutex::new(HashMap::new())),
            execs: Arc::new(Mutex::new(HashMap::new())),
            next_handle: 1,
        }
    }

    fn alloc_handle(&mut self) -> FsHandle {
        let handle = self.next_handle;
        self.next_handle += 1;
        handle
    }

    /// The provider behind a layer. A `Lower` entry can only exist when the lower layer
    /// does, so the expect is unreachable by construction.
    fn layer(&self, layer: OverlayLayer) -> Arc<Mutex<Box<dyn FsProvider>>> {
        match layer {
            OverlayLayer::Upper => Arc::clone(&self.upper),
            OverlayLayer::Lower => Arc::clone(
                self.lower
                    .as_ref()
                    .expect("a lower-layer handle implies a lower layer"),
            ),
        }
    }
}

impl FsProvider for OverlayFs {
    fn open(&mut self, path: &str, flags: OpenFlags) -> BoxOp<Result<FsHandle, FsError>> {
        let outer = self.alloc_handle();
        let files = Arc::clone(&self.files);
        let path = path.to_string();
        let wants_write = flags.write || flags.create || flags.truncate;
        if wants_write {
            // Mutations belong to the writable layer; the overlay never writes upper.
            let Some(lower) = self.lower.clone() else {
                return ready_op(Err(FsError::ReadOnly));
            };
            return Box::pin(async move {
                let op = { lower.lock().unwrap().open(&path, flags) };
                op.await.map(|inner| {
                    files
                        .lock()
                        .unwrap()
                        .insert(outer, (OverlayLayer::Lower, inner));
                    outer
                })
            });
        }
        let upper = Arc::clone(&self.upper);
        let lower = self.lower.clone();
        Box::pin(async move {
            let op = { upper.lock().unwrap().open(&path, flags) };
            match op.await {
                Ok(inner) => {
                    files
                        .lock()
                        .unwrap()
                        .insert(outer, (OverlayLayer::Upper, inner));
                    Ok(outer)
                }
                // Only an absence falls through; any other upper answer (denied, a type
                // error, an I/O failure) is the overlay's answer — upper shadows lower.
                Err(FsError::NotFound) => match lower {
                    Some(lower) => {
                        let op = { lower.lock().unwrap().open(&path, flags) };
                        op.await.map(|inner| {
                            files
                                .lock()
                                .unwrap()
                                .insert(outer, (OverlayLayer::Lower, inner));
                            outer
                        })
                    }
                    None => Err(FsError::NotFound),
                },
                Err(err) => Err(err),
            }
        })
    }

    fn open_exec(&mut self, path: &str) -> BoxOp<Result<FsHandle, FsError>> {
        let outer = self.alloc_handle();
        let execs = Arc::clone(&self.execs);
        let path = path.to_string();
        let upper = Arc::clone(&self.upper);
        let lower = self.lower.clone();
        Box::pin(async move {
            let op = { upper.lock().unwrap().open_exec(&path) };
            match op.await {
                Ok(inner) => {
                    execs
                        .lock()
                        .unwrap()
                        .insert(outer, (OverlayLayer::Upper, inner));
                    Ok(outer)
                }
                Err(FsError::NotFound) => match lower {
                    Some(lower) => {
                        let op = { lower.lock().unwrap().open_exec(&path) };
                        op.await.map(|inner| {
                            execs
                                .lock()
                                .unwrap()
                                .insert(outer, (OverlayLayer::Lower, inner));
                            outer
                        })
                    }
                    None => Err(FsError::NotFound),
                },
                Err(err) => Err(err),
            }
        })
    }

    fn list_directory(&mut self, path: &str) -> BoxOp<Result<Vec<String>, FsError>> {
        let upper = Arc::clone(&self.upper);
        let lower = self.lower.clone();
        let path = path.to_string();
        Box::pin(async move {
            let upper_result = {
                let op = { upper.lock().unwrap().list_directory(&path) };
                op.await
            };
            let lower_result = match &lower {
                Some(lower) => Some({
                    let op = { lower.lock().unwrap().list_directory(&path) };
                    op.await
                }),
                None => None,
            };
            match (upper_result, lower_result) {
                // Union of both layers, upper winning on collisions (dedup keeps upper's).
                (Ok(mut names), Some(Ok(extra))) => {
                    for name in extra {
                        if !names.contains(&name) {
                            names.push(name);
                        }
                    }
                    Ok(names)
                }
                (Ok(names), _) => Ok(names),
                (Err(_), Some(Ok(names))) => Ok(names),
                // Both layers failed (or only upper exists and failed): upper's error.
                (Err(err), _) => Err(err),
            }
        })
    }

    fn stat(&mut self, path: &str) -> BoxOp<Result<NodeStat, FsError>> {
        let upper = Arc::clone(&self.upper);
        let lower = self.lower.clone();
        let path = path.to_string();
        Box::pin(async move {
            let op = { upper.lock().unwrap().stat(&path) };
            match op.await {
                Ok(stat) => Ok(stat),
                Err(FsError::NotFound) => match lower {
                    Some(lower) => {
                        let op = { lower.lock().unwrap().stat(&path) };
                        op.await
                    }
                    None => Err(FsError::NotFound),
                },
                Err(err) => Err(err),
            }
        })
    }

    fn create_directory(&mut self, path: &str) -> BoxOp<Result<(), FsError>> {
        let Some(lower) = self.lower.clone() else {
            return ready_op(Err(FsError::ReadOnly));
        };
        let path = path.to_string();
        Box::pin(async move {
            let op = { lower.lock().unwrap().create_directory(&path) };
            op.await
        })
    }

    fn remove(&mut self, path: &str) -> BoxOp<Result<(), FsError>> {
        let Some(lower) = self.lower.clone() else {
            return ready_op(Err(FsError::ReadOnly));
        };
        let path = path.to_string();
        Box::pin(async move {
            let op = { lower.lock().unwrap().remove(&path) };
            op.await
        })
    }

    fn read(
        &mut self,
        file: FsHandle,
        offset: u64,
        dst: Vec<u8>,
    ) -> BoxOp<(Vec<u8>, Result<u64, FsError>)> {
        let entry = self.files.lock().unwrap().get(&file).copied();
        let Some((layer, inner)) = entry else {
            return ready_op((dst, Err(FsError::Io("unknown file handle".to_string()))));
        };
        let provider = self.layer(layer);
        Box::pin(async move {
            let op = { provider.lock().unwrap().read(inner, offset, dst) };
            op.await
        })
    }

    fn write(
        &mut self,
        file: FsHandle,
        offset: u64,
        src: Vec<u8>,
    ) -> BoxOp<(Vec<u8>, Result<u64, FsError>)> {
        let entry = self.files.lock().unwrap().get(&file).copied();
        let Some((layer, inner)) = entry else {
            return ready_op((src, Err(FsError::Io("unknown file handle".to_string()))));
        };
        let provider = self.layer(layer);
        Box::pin(async move {
            let op = { provider.lock().unwrap().write(inner, offset, src) };
            op.await
        })
    }

    fn exec_size(&mut self, handle: FsHandle) -> u64 {
        let entry = self.execs.lock().unwrap().get(&handle).copied();
        let Some((layer, inner)) = entry else {
            return 0;
        };
        self.layer(layer).lock().unwrap().exec_size(inner)
    }

    fn exec_read(
        &mut self,
        handle: FsHandle,
        offset: u64,
        dst: Vec<u8>,
    ) -> BoxOp<(Vec<u8>, Result<u64, FsError>)> {
        let entry = self.execs.lock().unwrap().get(&handle).copied();
        let Some((layer, inner)) = entry else {
            return ready_op((
                dst,
                Err(FsError::Io("unknown immutable handle".to_string())),
            ));
        };
        let provider = self.layer(layer);
        Box::pin(async move {
            let op = { provider.lock().unwrap().exec_read(inner, offset, dst) };
            op.await
        })
    }

    fn close_file(&mut self, file: FsHandle) {
        if let Some((layer, inner)) = self.files.lock().unwrap().remove(&file) {
            self.layer(layer).lock().unwrap().close_file(inner);
        }
    }

    fn close_exec(&mut self, handle: FsHandle) {
        if let Some((layer, inner)) = self.execs.lock().unwrap().remove(&handle) {
            self.layer(layer).lock().unwrap().close_exec(inner);
        }
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
    Ok(assemble(fs, None))
}

/// The layered session filesystem: the session directory's read-only program view
/// (`/bin/<name>.wasm`, plus the session manifest) as the upper layer, over the user's
/// writable `--fs-root` as the lower layer (when granted). Degrades to warnings rather
/// than errors: a broken root never blocks a session, the affected layer is just absent.
fn session_overlay_fs(
    session_root: &Path,
    fs_root: Option<&Path>,
    snapshot: eo9_providers_unix::fs::ExecSnapshotPolicy,
) -> Option<Box<dyn FsProvider>> {
    let lower: Option<Box<dyn FsProvider>> = match fs_root {
        Some(root) => match HostFs::new(root, snapshot) {
            Ok(fs) => Some(Box::new(fs)),
            Err(err) => {
                eprintln!("eo9: warning: programs get no writable data root: {err}");
                None
            }
        },
        None => None,
    };
    match HostFs::new(session_root, snapshot) {
        Ok(upper) => Some(Box::new(OverlayFs::new(Box::new(upper), lower))),
        Err(err) => {
            eprintln!("eo9: warning: the session loses its program view (/bin): {err}");
            // Without the program view the data root (if any) is still worth granting.
            lower
        }
    }
}

/// The providers granted to the shell task, and — by the same recipe — to every child it
/// spawns. The session environment is: terminal stdio, the host clocks, the OS RNG, the
/// layered session filesystem (the read-only `/bin` program view over the writable
/// `--fs-root`, see [`OverlayFs`]), and the full `eo9:exec` capability (the component
/// algebra, `compile`, and `task`/spawn).
///
/// A child spawned through the exec capability inherits the **same** environment by
/// default — the same overlaid filesystem (so a nested `eosh` finds `/bin` and a data
/// tool finds the user's files) and a fresh `eo9:exec` whose own child policy rebuilds
/// this very environment, so grandchildren (nested shells, schedulers) are full peers
/// too. This is "the environment is just data, handed down" (SPEC.md, Execution APIs):
/// authority is ambient *within a session*, bounded by the session's own grants. The
/// runtime still links only the interfaces a given child imports (the loader rule), and
/// `only`/`$`/`&`/`configure` attenuate the component *before* spawn — so
/// `only eo9:text/text $ prog` strips exec and fs, and a program that cannot run without
/// a sealed required capability is refused before it starts.
///
/// `editor` replaces the plain stdio text provider with the interactive line editor
/// (history + tab completion) when the session is interactive; it changes how the shell's
/// input is typed, not what is granted (children always get the plain stdio text
/// provider).
pub fn shell_providers(
    cfg: &Config,
    session_root: &Path,
    image: &Image,
    editor: Option<crate::interactive::InteractiveText>,
) -> Result<Providers, String> {
    // The shell itself cannot work without its session filesystem (eosh resolves
    // `/bin/<name>.wasm` through it); surface a broken session root as a hard error here.
    // The per-child factory below degrades problems to warnings instead, never blocking a
    // spawn.
    drop(
        HostFs::new(session_root, cfg.exec_snapshot)
            .map_err(|err| format!("cannot root the shell session filesystem: {err}"))?,
    );

    // Owned, 'static state for the recursive environment factory. `engine`'s concrete
    // type (the wasmtime engine) is only ever captured here, never named — this crate has
    // no direct wasmtime dependency by design.
    let engine = image.engine().clone();
    let exec_snapshot = cfg.exec_snapshot;
    let session_root = session_root.to_path_buf();
    let fs_root = cfg.fs_root.clone();

    // A late-bound self-reference: `make` builds the session environment, and the exec
    // capability it installs carries a child policy that calls `make` again for the next
    // generation. Boxing it as one `Arc<dyn Fn>` keeps the recursion a single type.
    type MakeEnv = dyn Fn() -> Providers + Send + Sync;
    let slot: Arc<Mutex<Option<Arc<MakeEnv>>>> = Arc::new(Mutex::new(None));
    let make: Arc<MakeEnv> = {
        let slot = Arc::clone(&slot);
        Arc::new(move || {
            let fs = session_overlay_fs(&session_root, fs_root.as_deref(), exec_snapshot);
            let child_slot = Arc::clone(&slot);
            let policy = ChildPolicy::with_providers(move || {
                let make = child_slot
                    .lock()
                    .unwrap()
                    .clone()
                    .expect("the session child policy is initialized before the first spawn");
                make()
            });
            assemble(fs, Some(ExecProvider::new(&engine, policy)))
        })
    };
    *slot.lock().unwrap() = Some(Arc::clone(&make));

    let mut providers = make();
    if let Some(editor) = editor {
        providers.text = Some(Box::new(editor));
    }
    Ok(providers)
}

/// The session manifest `eo9 shell` leaves at `<session>/session` for eosh's `env`
/// builtin: a plain-text description of what the shell session holds and what programs
/// started from it receive. Purely informational — the linking rules above are the
/// authority. Keep this in sync with [`shell_providers`] (it describes exactly the
/// environment it assembles) and with eosh-core's `envinfo` parser (the `eo9-session 1`
/// format).
///
/// Children inherit the shell's full environment by default — the same text/time/entropy,
/// the same layered filesystem (the read-only `/bin` program view over the writable
/// `--fs-root`), and the entire `eo9:exec` capability — so a child can itself compose,
/// compile, and spawn (a nested `eosh` is a full peer). Restrict any one command with
/// `only`.
pub fn session_manifest(cfg: &Config) -> String {
    let mut lines = vec![
        "eo9-session 1".to_string(),
        "shell text terminal standard streams".to_string(),
        "shell time host clocks".to_string(),
        "shell entropy host OS RNG".to_string(),
        "shell fs programs at /bin (read-only) layered over the session's data root".to_string(),
        "shell exec the component algebra, compile, and spawn".to_string(),
        "child text terminal standard streams (shared with the shell)".to_string(),
        "child time host clocks".to_string(),
        "child entropy host OS RNG".to_string(),
    ];
    match &cfg.fs_root {
        Some(root) => lines.push(format!(
            "child fs programs at /bin (read-only) over host directory {} (from --fs-root)",
            root.display()
        )),
        None => lines.push(
            "child fs programs at /bin (read-only); writes are refused — no writable data root"
                .to_string(),
        ),
    }
    lines.push("child exec the component algebra, compile, and spawn".to_string());
    lines.push(
        "note children inherit the shell's full environment; restrict a command with `only` \
         (e.g. `only eo9:text/text $ prog`)"
            .to_string(),
    );
    if cfg.fs_root.is_none() {
        lines.push(
            "note start the shell with --fs-root <dir> to give programs a writable data directory"
                .to_string(),
        );
    }
    let mut manifest = lines.join("\n");
    manifest.push('\n');
    manifest
}

/// The fixed part of every provider set this binary hands out: terminal stdio, host
/// clocks, and the OS RNG, plus whatever fs/exec grant the caller decided on.
fn assemble(fs: Option<Box<dyn FsProvider>>, exec: Option<ExecProvider>) -> Providers {
    Providers {
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
        exec,
    }
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
    fn session_manifest_grants_children_the_full_environment() {
        let without = session_manifest(&Config::default());
        assert!(without.starts_with("eo9-session 1\n"));
        // The shell holds the whole environment...
        assert!(without.contains("shell exec "));
        assert!(without.contains("shell fs "));
        // ...and children inherit all of it, including fs (the /bin view) and exec.
        assert!(without.contains("child fs "));
        assert!(without.contains("child exec "));
        assert!(without.contains("child entropy "));
        // The note points at `only` as the way to restrict, not at a withheld capability.
        assert!(without.contains("restrict a command with `only`"));
        assert!(!without.contains("never receive the exec capability"));
        // Without --fs-root the manifest says how to grant a writable data root.
        assert!(without.contains("--fs-root"));
        assert!(without.contains("writes are refused"));

        let cfg = Config {
            fs_root: Some(std::path::PathBuf::from("/tmp/data")),
            ..Config::default()
        };
        let with = session_manifest(&cfg);
        assert!(with.contains("over host directory /tmp/data (from --fs-root)"));
        assert!(!with.contains("writes are refused"));
        assert!(!with.contains("to give programs a writable data directory"));
    }

    #[test]
    fn overlay_fs_layers_reads_and_routes_writes_to_lower() {
        let upper_root = scratch_dir("overlay-upper");
        let lower_root = scratch_dir("overlay-lower");
        std::fs::create_dir_all(upper_root.join("bin")).unwrap();
        std::fs::write(upper_root.join("bin/tool.wasm"), b"program bytes").unwrap();
        std::fs::write(upper_root.join("shared.txt"), b"from upper").unwrap();
        std::fs::write(lower_root.join("shared.txt"), b"from lower!").unwrap();
        std::fs::write(lower_root.join("notes.txt"), b"user data").unwrap();

        let policy = eo9_providers_unix::fs::ExecSnapshotPolicy::CloneOrRefuse;
        let upper = HostFs::new(&upper_root, policy).unwrap();
        let lower = HostFs::new(&lower_root, policy).unwrap();
        let mut overlay = OverlayFs::new(Box::new(upper), Some(Box::new(lower)));

        let read = OpenFlags {
            read: true,
            write: false,
            create: false,
            truncate: false,
        };

        // Upper-only path reads through; lower-only path falls through.
        let tool = block_on(overlay.open("bin/tool.wasm", read)).expect("upper file opens");
        let (buf, n) = block_on(overlay.read(tool, 0, vec![0u8; 13]));
        assert_eq!(n.unwrap(), 13);
        assert_eq!(buf, b"program bytes");
        overlay.close_file(tool);

        let notes = block_on(overlay.open("notes.txt", read)).expect("lower file opens");
        let (buf, n) = block_on(overlay.read(notes, 0, vec![0u8; 9]));
        assert_eq!(n.unwrap(), 9);
        assert_eq!(buf, b"user data");
        overlay.close_file(notes);

        // A name in both layers reads the upper copy (upper shadows lower).
        let shared = block_on(overlay.open("shared.txt", read)).expect("shared opens");
        let (buf, _) = block_on(overlay.read(shared, 0, vec![0u8; 10]));
        assert_eq!(buf, b"from upper");
        overlay.close_file(shared);

        // Listings union both layers; the shadowed name appears once.
        let mut names = block_on(overlay.list_directory("/")).expect("list /");
        names.sort();
        assert_eq!(names, vec!["bin", "notes.txt", "shared.txt"]);

        // Writes are routed to lower and never touch upper.
        let write = OpenFlags {
            read: true,
            write: true,
            create: true,
            truncate: true,
        };
        let out = block_on(overlay.open("new.txt", write)).expect("write-open goes to lower");
        let (_, written) = block_on(overlay.write(out, 0, b"hi".to_vec()));
        assert_eq!(written.unwrap(), 2);
        overlay.close_file(out);
        assert!(lower_root.join("new.txt").is_file());
        assert!(!upper_root.join("new.txt").exists());

        // exec opens resolve upper-first too.
        let exec = block_on(overlay.open_exec("bin/tool.wasm")).expect("open-exec on upper");
        assert_eq!(overlay.exec_size(exec), 13);
        overlay.close_exec(exec);

        std::fs::remove_dir_all(&upper_root).unwrap();
        std::fs::remove_dir_all(&lower_root).unwrap();
    }

    #[test]
    fn overlay_fs_without_a_lower_layer_is_read_only() {
        let upper_root = scratch_dir("overlay-readonly");
        std::fs::write(upper_root.join("present.txt"), b"ro").unwrap();
        let policy = eo9_providers_unix::fs::ExecSnapshotPolicy::CloneOrRefuse;
        let upper = HostFs::new(&upper_root, policy).unwrap();
        let mut overlay = OverlayFs::new(Box::new(upper), None);

        let read = OpenFlags {
            read: true,
            write: false,
            create: false,
            truncate: false,
        };
        assert!(block_on(overlay.open("present.txt", read)).is_ok());
        assert_eq!(
            block_on(overlay.stat("missing.txt")),
            Err(FsError::NotFound)
        );

        let write = OpenFlags {
            read: false,
            write: true,
            create: true,
            truncate: false,
        };
        assert_eq!(
            block_on(overlay.open("new.txt", write)),
            Err(FsError::ReadOnly)
        );
        assert_eq!(
            block_on(overlay.create_directory("dir")),
            Err(FsError::ReadOnly)
        );
        assert_eq!(
            block_on(overlay.remove("present.txt")),
            Err(FsError::ReadOnly)
        );
        assert!(upper_root.join("present.txt").is_file());

        std::fs::remove_dir_all(&upper_root).unwrap();
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
