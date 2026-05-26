//! Linker assembly: wiring a task's imports to the root host providers.
//!
//! A task's imports are satisfied from (a) its own fused composition — already inside the
//! component, nothing to do here — and (b) the root providers handed to `spawn`. Anything
//! left unsatisfied is a spawn error (the loader rule from SPEC "WASM runtime"); that check
//! happens naturally when the linker instantiates the component.
//!
//! The loader rule has two halves:
//!
//! * **Required** interfaces (`eo9:X/X`) are registered only when the corresponding
//!   provider was actually supplied, so a component that imports a capability it was not
//!   granted fails to link — capability absence is expressed by composition or by the
//!   optional flavor, never by stub host functions that silently do nothing.
//! * **Optional** flavors (`eo9:X/X-optional`) are always registered: `default()` answers
//!   `some(handle)` when the capability was granted and `none` otherwise, which is
//!   observationally the `X.none` provider. The types-only interfaces (`eo9:X/types`) and
//!   `eo9:io/buffers` are likewise always available — they carry no authority.
//!
//! Blocking operations are `async func`s in the WIT (the async-operations migration), so
//! their host implementations are *concurrent* host functions: the returned future awaits
//! the provider's [`BoxOp`](crate::providers::BoxOp) directly, and the waker that reaches
//! the provider is the task's doorbell. Owned-buffer operations take the bytes out of the
//! task's buffer table for the life of the operation and put them back on both the
//! success and the error path.

use std::future::Future;
use std::pin::Pin;

use wasmtime::component::{
    Accessor, ComponentType, Lift, Linker, LinkerInstance, Lower, Resource, ResourceType,
};
use wasmtime::{Result, StoreContextMut};

use crate::providers::{
    Datetime, FsError, NodeKind, OpenFlags, OutputStream, Providers, TextError,
};
use crate::task::TaskState;

/// Per-call ceiling on `eo9:entropy/entropy.get-bytes` requests. The host materialises the
/// returned `list<u8>` before it is copied into the guest, so the request size must be
/// bounded *before* any allocation happens — otherwise a guest could exhaust host memory
/// regardless of its own linear-memory ceiling. Entropy requests are inherently small;
/// anything larger than this is treated as hostile and traps the task.
const MAX_ENTROPY_REQUEST_BYTES: u64 = 64 * 1024;

/// Register host implementations for the always-available pieces (types, buffers, optional
/// flavors) and for every capability whose provider is present in `providers`.
pub(crate) fn add_providers(linker: &mut Linker<TaskState>, providers: &Providers) -> Result<()> {
    add_types(linker)?;
    add_buffers(linker)?;

    add_optional::<TextCap>(
        linker,
        "eo9:text/text-optional@0.1.0",
        providers.text.is_some(),
    )?;
    add_optional::<TimeCap>(
        linker,
        "eo9:time/time-optional@0.1.0",
        providers.time.is_some(),
    )?;
    add_optional::<EntropyCap>(
        linker,
        "eo9:entropy/entropy-optional@0.1.0",
        providers.entropy.is_some(),
    )?;
    add_optional::<FsCap>(linker, "eo9:fs/fs-optional@0.1.0", providers.fs.is_some())?;

    if providers.text.is_some() {
        add_text(linker)?;
    }
    if providers.time.is_some() {
        add_time(linker)?;
    }
    if providers.entropy.is_some() {
        add_entropy(linker)?;
    }
    if providers.fs.is_some() {
        add_fs(linker)?;
    }
    Ok(())
}

// ---------------------------------------------------------------------------------------
// Host resource representations
// ---------------------------------------------------------------------------------------

/// Host representation of the `eo9:text/types.text-impl` resource (stateless token: all
/// state lives in the provider).
pub struct TextCap;
/// Host representation of `eo9:time/types.time-impl`.
pub struct TimeCap;
/// Host representation of `eo9:entropy/types.entropy-impl`.
pub struct EntropyCap;
/// Host representation of `eo9:fs/types.fs-impl`.
pub struct FsCap;
/// Host representation of `eo9:fs/fs.file`; the rep is the provider's file handle.
pub struct FileRes;
/// Host representation of `eo9:fs/fs.immutable-handle`; the rep is the provider's handle.
pub struct ExecRes;
/// Host representation of `eo9:io/buffers.buffer`; the rep indexes the task's buffer table.
pub struct BufferRes;

// ---------------------------------------------------------------------------------------
// WIT-shaped host types (structurally matched against the eo9 interfaces)
// ---------------------------------------------------------------------------------------

#[derive(Clone, Copy, ComponentType, Lift, Lower)]
#[component(enum)]
#[repr(u8)]
// The variants are only ever constructed by the generated `Lift` implementation (they come
// in from the guest), which dead-code analysis does not see.
#[allow(dead_code)]
enum WitOutputStream {
    #[component(name = "out")]
    Out,
    #[component(name = "err")]
    Err,
}

impl From<WitOutputStream> for OutputStream {
    fn from(value: WitOutputStream) -> Self {
        match value {
            WitOutputStream::Out => OutputStream::Out,
            WitOutputStream::Err => OutputStream::Err,
        }
    }
}

#[derive(Clone, ComponentType, Lift, Lower)]
#[component(variant)]
enum WitTextError {
    #[component(name = "closed")]
    Closed,
    #[component(name = "io")]
    Io(String),
}

impl From<TextError> for WitTextError {
    fn from(value: TextError) -> Self {
        match value {
            TextError::Closed => WitTextError::Closed,
            TextError::Io(message) => WitTextError::Io(message),
        }
    }
}

#[derive(Clone, Copy, ComponentType, Lift, Lower)]
#[component(record)]
struct WitDatetime {
    seconds: i64,
    nanoseconds: u32,
}

impl From<Datetime> for WitDatetime {
    fn from(value: Datetime) -> Self {
        WitDatetime {
            seconds: value.seconds,
            nanoseconds: value.nanoseconds,
        }
    }
}

#[derive(Clone, Copy, ComponentType, Lift, Lower)]
#[component(record)]
struct WitInstant {
    nanoseconds: u64,
}

#[derive(Clone, Copy, ComponentType, Lift, Lower)]
#[component(enum)]
#[repr(u8)]
enum WitNodeKind {
    #[component(name = "file")]
    File,
    #[component(name = "directory")]
    Directory,
}

impl From<NodeKind> for WitNodeKind {
    fn from(value: NodeKind) -> Self {
        match value {
            NodeKind::File => WitNodeKind::File,
            NodeKind::Directory => WitNodeKind::Directory,
        }
    }
}

#[derive(Clone, Copy, ComponentType, Lift, Lower)]
#[component(record)]
struct WitNodeStat {
    kind: WitNodeKind,
    size: u64,
}

#[derive(Clone, ComponentType, Lift, Lower)]
#[component(variant)]
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

impl From<FsError> for WitFsError {
    fn from(value: FsError) -> Self {
        match value {
            FsError::NotFound => WitFsError::NotFound,
            FsError::AlreadyExists => WitFsError::AlreadyExists,
            FsError::NotADirectory => WitFsError::NotADirectory,
            FsError::IsADirectory => WitFsError::IsADirectory,
            FsError::Denied => WitFsError::Denied,
            FsError::ReadOnly => WitFsError::ReadOnly,
            FsError::NoSpace => WitFsError::NoSpace,
            FsError::NotImmutable => WitFsError::NotImmutable,
            FsError::Io(message) => WitFsError::Io(message),
        }
    }
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

impl From<WitOpenFlags> for OpenFlags {
    fn from(value: WitOpenFlags) -> Self {
        OpenFlags {
            read: value.contains(WitOpenFlags::READ),
            write: value.contains(WitOpenFlags::WRITE),
            create: value.contains(WitOpenFlags::CREATE),
            truncate: value.contains(WitOpenFlags::TRUNCATE),
        }
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

/// The payload of `eo9:text/text.read-line`.
type ReadLineItem = Result<Option<String>, WitTextError>;
/// The return value of the owned-buffer fs reads (`read` / `exec-read`).
type FsReadReturn = (Resource<BufferRes>, Result<WitReadResult, WitFsError>);
/// The return value of the owned-buffer fs write.
type FsWriteReturn = (Resource<BufferRes>, Result<WitWriteResult, WitFsError>);

/// The boxed-future shape `func_wrap_concurrent` expects.
type ConcurrentFuture<'a, R> = Pin<Box<dyn Future<Output = Result<R>> + Send + 'a>>;

// ---------------------------------------------------------------------------------------
// Always-available pieces: types, buffers, optional flavors
// ---------------------------------------------------------------------------------------

/// Register every types-only interface (the root-handle resources). These carry no
/// authority: a handle is a token, and every operation that accepts one is defined on the
/// capability interface, which is only linked when the capability was granted.
fn add_types(linker: &mut Linker<TaskState>) -> Result<()> {
    linker.instance("eo9:text/types@0.1.0")?.resource(
        "text-impl",
        ResourceType::host::<TextCap>(),
        |_, _| Ok(()),
    )?;
    linker.instance("eo9:time/types@0.1.0")?.resource(
        "time-impl",
        ResourceType::host::<TimeCap>(),
        |_, _| Ok(()),
    )?;
    linker.instance("eo9:entropy/types@0.1.0")?.resource(
        "entropy-impl",
        ResourceType::host::<EntropyCap>(),
        |_, _| Ok(()),
    )?;
    linker.instance("eo9:fs/types@0.1.0")?.resource(
        "fs-impl",
        ResourceType::host::<FsCap>(),
        |_, _| Ok(()),
    )?;
    Ok(())
}

/// Register an optional flavor (`eo9:X/X-optional`): `default()` answers `some(handle)`
/// when the capability was granted and `none` otherwise — the auto-sealed absent provider
/// of the loader rule (observationally `X.none`).
fn add_optional<C: 'static>(
    linker: &mut Linker<TaskState>,
    interface: &str,
    granted: bool,
) -> Result<()> {
    let mut instance = linker.instance(interface)?;
    instance.func_wrap(
        "default",
        move |_store: StoreContextMut<'_, TaskState>, (): ()| -> Result<(Option<Resource<C>>,)> {
            Ok((granted.then(|| Resource::new_own(0)),))
        },
    )
}

/// Register `eo9:io/buffers`: host-side byte buffers backed by the task's buffer table.
fn add_buffers(linker: &mut Linker<TaskState>) -> Result<()> {
    let mut buffers = linker.instance("eo9:io/buffers@0.1.0")?;

    buffers.resource(
        "buffer",
        ResourceType::host::<BufferRes>(),
        |mut store: StoreContextMut<'_, TaskState>, rep| {
            store.data_mut().buffers.free(rep);
            Ok(())
        },
    )?;

    buffers.func_wrap(
        "[constructor]buffer",
        |mut store: StoreContextMut<'_, TaskState>,
         (len,): (u64,)|
         -> Result<(Resource<BufferRes>,)> {
            let rep = store.data_mut().buffers.alloc(len)?;
            Ok((Resource::new_own(rep),))
        },
    )?;

    buffers.func_wrap(
        "[method]buffer.len",
        |mut store: StoreContextMut<'_, TaskState>,
         (buffer,): (Resource<BufferRes>,)|
         -> Result<(u64,)> {
            Ok((store.data_mut().buffers.bytes(buffer.rep())?.len() as u64,))
        },
    )?;

    buffers.func_wrap(
        "[method]buffer.read",
        |mut store: StoreContextMut<'_, TaskState>,
         (buffer, offset, len): (Resource<BufferRes>, u64, u64)|
         -> Result<(Vec<u8>,)> {
            let bytes = store.data_mut().buffers.bytes(buffer.rep())?;
            let (start, end) = byte_range(bytes.len(), offset, len)?;
            Ok((bytes[start..end].to_vec(),))
        },
    )?;

    buffers.func_wrap(
        "[method]buffer.write",
        |mut store: StoreContextMut<'_, TaskState>,
         (buffer, offset, data): (Resource<BufferRes>, u64, Vec<u8>)|
         -> Result<()> {
            let bytes = store.data_mut().buffers.bytes(buffer.rep())?;
            let (start, end) = byte_range(bytes.len(), offset, data.len() as u64)?;
            bytes[start..end].copy_from_slice(&data);
            Ok(())
        },
    )?;

    Ok(())
}

/// Bounds-check an `(offset, len)` range against a buffer of `size` bytes; out-of-bounds
/// ranges trap, as the WIT specifies.
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

// ---------------------------------------------------------------------------------------
// eo9:text
// ---------------------------------------------------------------------------------------

fn add_text(linker: &mut Linker<TaskState>) -> Result<()> {
    let mut text = linker.instance("eo9:text/text@0.1.0")?;
    add_default_handle::<TextCap>(&mut text)?;

    text.func_wrap(
        "write",
        |mut store: StoreContextMut<'_, TaskState>,
         (_cap, to, content): (Resource<TextCap>, WitOutputStream, String)|
         -> Result<(Result<(), WitTextError>,)> {
            let provider = store.data_mut().text_provider()?;
            Ok((provider
                .write(to.into(), &content)
                .map_err(WitTextError::from),))
        },
    )?;

    text.func_wrap_concurrent(
        "read-line",
        |accessor: &Accessor<TaskState>,
         (_cap,): (Resource<TextCap>,)|
         -> ConcurrentFuture<'_, (ReadLineItem,)> {
            Box::pin(async move {
                let op = accessor.with(|mut access| -> Result<_> {
                    Ok(access.data_mut().text_provider()?.read_line())
                })?;
                Ok((op.await.map_err(WitTextError::from),))
            })
        },
    )?;

    Ok(())
}

// ---------------------------------------------------------------------------------------
// eo9:time
// ---------------------------------------------------------------------------------------

fn add_time(linker: &mut Linker<TaskState>) -> Result<()> {
    let mut time = linker.instance("eo9:time/time@0.1.0")?;
    add_default_handle::<TimeCap>(&mut time)?;

    time.func_wrap(
        "now",
        |mut store: StoreContextMut<'_, TaskState>,
         (_cap,): (Resource<TimeCap>,)|
         -> Result<(WitDatetime,)> {
            Ok((store.data_mut().time_provider()?.now().into(),))
        },
    )?;

    time.func_wrap(
        "monotonic-now",
        |mut store: StoreContextMut<'_, TaskState>,
         (_cap,): (Resource<TimeCap>,)|
         -> Result<(WitInstant,)> {
            Ok((WitInstant {
                nanoseconds: store.data_mut().time_provider()?.monotonic_now(),
            },))
        },
    )?;

    time.func_wrap(
        "resolution",
        |mut store: StoreContextMut<'_, TaskState>,
         (_cap,): (Resource<TimeCap>,)|
         -> Result<(u64,)> { Ok((store.data_mut().time_provider()?.resolution(),)) },
    )?;

    time.func_wrap_concurrent(
        "sleep",
        |accessor: &Accessor<TaskState>,
         (_cap, duration_ns): (Resource<TimeCap>, u64)|
         -> ConcurrentFuture<'_, ()> {
            Box::pin(async move {
                let op = accessor.with(|mut access| -> Result<_> {
                    Ok(access.data_mut().time_provider()?.sleep(duration_ns))
                })?;
                op.await;
                Ok(())
            })
        },
    )?;

    Ok(())
}

// ---------------------------------------------------------------------------------------
// eo9:entropy
// ---------------------------------------------------------------------------------------

fn add_entropy(linker: &mut Linker<TaskState>) -> Result<()> {
    let mut entropy = linker.instance("eo9:entropy/entropy@0.1.0")?;
    add_default_handle::<EntropyCap>(&mut entropy)?;

    entropy.func_wrap(
        "get-bytes",
        |mut store: StoreContextMut<'_, TaskState>,
         (_cap, len): (Resource<EntropyCap>, u64)|
         -> Result<(Vec<u8>,)> {
            // The bound must hold before any allocation (here or in the provider).
            if len > MAX_ENTROPY_REQUEST_BYTES {
                return Err(wasmtime::Error::msg(format!(
                    "entropy get-bytes request of {len} bytes exceeds the per-call cap of \
                     {MAX_ENTROPY_REQUEST_BYTES} bytes"
                )));
            }
            Ok((store.data_mut().entropy_provider()?.get_bytes(len),))
        },
    )?;

    entropy.func_wrap(
        "get-u64",
        |mut store: StoreContextMut<'_, TaskState>,
         (_cap,): (Resource<EntropyCap>,)|
         -> Result<(u64,)> { Ok((store.data_mut().entropy_provider()?.get_u64(),)) },
    )?;

    Ok(())
}

// ---------------------------------------------------------------------------------------
// eo9:fs
// ---------------------------------------------------------------------------------------

fn add_fs(linker: &mut Linker<TaskState>) -> Result<()> {
    let mut fs = linker.instance("eo9:fs/fs@0.1.0")?;
    add_default_handle::<FsCap>(&mut fs)?;

    // The file resources belong to the fs interface itself; dropping a handle tells the
    // provider to release whatever backs it.
    fs.resource(
        "file",
        ResourceType::host::<FileRes>(),
        |mut store: StoreContextMut<'_, TaskState>, rep| {
            if let Some(provider) = store.data_mut().providers.fs.as_mut() {
                provider.close_file(rep);
            }
            Ok(())
        },
    )?;
    fs.resource(
        "immutable-handle",
        ResourceType::host::<ExecRes>(),
        |mut store: StoreContextMut<'_, TaskState>, rep| {
            if let Some(provider) = store.data_mut().providers.fs.as_mut() {
                provider.close_exec(rep);
            }
            Ok(())
        },
    )?;

    fs.func_wrap_concurrent(
        "open",
        |accessor: &Accessor<TaskState>,
         (_cap, path, flags): (Resource<FsCap>, String, WitOpenFlags)|
         -> ConcurrentFuture<'_, (Result<Resource<FileRes>, WitFsError>,)> {
            Box::pin(async move {
                let op = accessor.with(|mut access| -> Result<_> {
                    Ok(access.data_mut().fs_provider()?.open(&path, flags.into()))
                })?;
                Ok((op
                    .await
                    .map(Resource::<FileRes>::new_own)
                    .map_err(WitFsError::from),))
            })
        },
    )?;

    fs.func_wrap_concurrent(
        "open-exec",
        |accessor: &Accessor<TaskState>,
         (_cap, path): (Resource<FsCap>, String)|
         -> ConcurrentFuture<'_, (Result<Resource<ExecRes>, WitFsError>,)> {
            Box::pin(async move {
                let op = accessor.with(|mut access| -> Result<_> {
                    Ok(access.data_mut().fs_provider()?.open_exec(&path))
                })?;
                Ok((op
                    .await
                    .map(Resource::<ExecRes>::new_own)
                    .map_err(WitFsError::from),))
            })
        },
    )?;

    fs.func_wrap_concurrent(
        "list-directory",
        |accessor: &Accessor<TaskState>,
         (_cap, path): (Resource<FsCap>, String)|
         -> ConcurrentFuture<'_, (Result<Vec<String>, WitFsError>,)> {
            Box::pin(async move {
                let op = accessor.with(|mut access| -> Result<_> {
                    Ok(access.data_mut().fs_provider()?.list_directory(&path))
                })?;
                Ok((op.await.map_err(WitFsError::from),))
            })
        },
    )?;

    fs.func_wrap_concurrent(
        "stat",
        |accessor: &Accessor<TaskState>,
         (_cap, path): (Resource<FsCap>, String)|
         -> ConcurrentFuture<'_, (Result<WitNodeStat, WitFsError>,)> {
            Box::pin(async move {
                let op = accessor.with(|mut access| -> Result<_> {
                    Ok(access.data_mut().fs_provider()?.stat(&path))
                })?;
                Ok((op
                    .await
                    .map(|stat| WitNodeStat {
                        kind: stat.kind.into(),
                        size: stat.size,
                    })
                    .map_err(WitFsError::from),))
            })
        },
    )?;

    fs.func_wrap_concurrent(
        "create-directory",
        |accessor: &Accessor<TaskState>,
         (_cap, path): (Resource<FsCap>, String)|
         -> ConcurrentFuture<'_, (Result<(), WitFsError>,)> {
            Box::pin(async move {
                let op = accessor.with(|mut access| -> Result<_> {
                    Ok(access.data_mut().fs_provider()?.create_directory(&path))
                })?;
                Ok((op.await.map_err(WitFsError::from),))
            })
        },
    )?;

    fs.func_wrap_concurrent(
        "remove",
        |accessor: &Accessor<TaskState>,
         (_cap, path): (Resource<FsCap>, String)|
         -> ConcurrentFuture<'_, (Result<(), WitFsError>,)> {
            Box::pin(async move {
                let op = accessor.with(|mut access| -> Result<_> {
                    Ok(access.data_mut().fs_provider()?.remove(&path))
                })?;
                Ok((op.await.map_err(WitFsError::from),))
            })
        },
    )?;

    fs.func_wrap_concurrent(
        "read",
        |accessor: &Accessor<TaskState>,
         (file, offset, dst): (Resource<FileRes>, u64, Resource<BufferRes>)|
         -> ConcurrentFuture<'_, (FsReadReturn,)> {
            Box::pin(async move {
                let buffer_rep = dst.rep();
                let op = accessor.with(|mut access| -> Result<_> {
                    let state = access.data_mut();
                    let bytes = state.buffers.take(buffer_rep)?;
                    Ok(state.fs_provider()?.read(file.rep(), offset, bytes))
                })?;
                let (bytes, result) = op.await;
                accessor.with(|mut access| access.data_mut().buffers.restore(buffer_rep, bytes));
                Ok(((
                    Resource::new_own(buffer_rep),
                    result
                        .map(|bytes_read| WitReadResult { bytes_read })
                        .map_err(WitFsError::from),
                ),))
            })
        },
    )?;

    fs.func_wrap_concurrent(
        "write",
        |accessor: &Accessor<TaskState>,
         (file, offset, src): (Resource<FileRes>, u64, Resource<BufferRes>)|
         -> ConcurrentFuture<'_, (FsWriteReturn,)> {
            Box::pin(async move {
                let buffer_rep = src.rep();
                let op = accessor.with(|mut access| -> Result<_> {
                    let state = access.data_mut();
                    let bytes = state.buffers.take(buffer_rep)?;
                    Ok(state.fs_provider()?.write(file.rep(), offset, bytes))
                })?;
                let (bytes, result) = op.await;
                accessor.with(|mut access| access.data_mut().buffers.restore(buffer_rep, bytes));
                Ok(((
                    Resource::new_own(buffer_rep),
                    result
                        .map(|bytes_written| WitWriteResult { bytes_written })
                        .map_err(WitFsError::from),
                ),))
            })
        },
    )?;

    fs.func_wrap(
        "exec-size",
        |mut store: StoreContextMut<'_, TaskState>,
         (handle,): (Resource<ExecRes>,)|
         -> Result<(u64,)> {
            Ok((store.data_mut().fs_provider()?.exec_size(handle.rep()),))
        },
    )?;

    fs.func_wrap_concurrent(
        "exec-read",
        |accessor: &Accessor<TaskState>,
         (handle, offset, dst): (Resource<ExecRes>, u64, Resource<BufferRes>)|
         -> ConcurrentFuture<'_, (FsReadReturn,)> {
            Box::pin(async move {
                let buffer_rep = dst.rep();
                let op = accessor.with(|mut access| -> Result<_> {
                    let state = access.data_mut();
                    let bytes = state.buffers.take(buffer_rep)?;
                    Ok(state.fs_provider()?.exec_read(handle.rep(), offset, bytes))
                })?;
                let (bytes, result) = op.await;
                accessor.with(|mut access| access.data_mut().buffers.restore(buffer_rep, bytes));
                Ok(((
                    Resource::new_own(buffer_rep),
                    result
                        .map(|bytes_read| WitReadResult { bytes_read })
                        .map_err(WitFsError::from),
                ),))
            })
        },
    )?;

    Ok(())
}

// ---------------------------------------------------------------------------------------
// Shared helpers
// ---------------------------------------------------------------------------------------

/// Register the capability's `default: func() -> <root-handle>` export: every call mints a
/// fresh owned handle token; all real state lives in the provider.
fn add_default_handle<C: 'static>(instance: &mut LinkerInstance<'_, TaskState>) -> Result<()> {
    instance.func_wrap(
        "default",
        |_store: StoreContextMut<'_, TaskState>, (): ()| -> Result<(Resource<C>,)> {
            Ok((Resource::new_own(0),))
        },
    )
}

impl TaskState {
    fn text_provider(&mut self) -> Result<&mut dyn crate::providers::TextProvider> {
        self.providers
            .text
            .as_deref_mut()
            .map(|provider| provider as &mut dyn crate::providers::TextProvider)
            .ok_or_else(|| wasmtime::Error::msg("text capability was not granted to this task"))
    }

    fn time_provider(&mut self) -> Result<&mut dyn crate::providers::TimeProvider> {
        self.providers
            .time
            .as_deref_mut()
            .map(|provider| provider as &mut dyn crate::providers::TimeProvider)
            .ok_or_else(|| wasmtime::Error::msg("time capability was not granted to this task"))
    }

    fn entropy_provider(&mut self) -> Result<&mut dyn crate::providers::EntropyProvider> {
        self.providers
            .entropy
            .as_deref_mut()
            .map(|provider| provider as &mut dyn crate::providers::EntropyProvider)
            .ok_or_else(|| wasmtime::Error::msg("entropy capability was not granted to this task"))
    }

    fn fs_provider(&mut self) -> Result<&mut dyn crate::providers::FsProvider> {
        self.providers
            .fs
            .as_deref_mut()
            .map(|provider| provider as &mut dyn crate::providers::FsProvider)
            .ok_or_else(|| {
                wasmtime::Error::msg("filesystem capability was not granted to this task")
            })
    }
}
