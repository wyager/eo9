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
    Datetime, DiskError, FsError, NodeKind, OpenFlags, OutputStream, Providers, TextError,
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
    add_diagnostics(linker)?;

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
    add_optional::<DiskCap>(
        linker,
        "eo9:disk/disk-optional@0.1.0",
        providers.disk.is_some(),
    )?;

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
    } else {
        add_fs_handle_only(linker)?;
    }
    if providers.disk.is_some() {
        add_disk(linker)?;
    }
    if providers.exec.is_some() {
        add_exec(linker)?;
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
/// Host representation of `eo9:disk/types.disk-impl`.
pub struct DiskCap;
/// Host representation of `eo9:fs/fs.fs-impl`.
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

/// `eo9:disk/disk.read-error`. Variant order matches the WIT declaration.
#[derive(Clone, ComponentType, Lift, Lower)]
#[component(variant)]
enum WitDiskReadError {
    #[component(name = "not-found")]
    NotFound,
    #[component(name = "io")]
    Io(String),
    #[component(name = "out-of-range")]
    OutOfRange,
}

impl From<DiskError> for WitDiskReadError {
    fn from(value: DiskError) -> Self {
        match value {
            DiskError::NotFound => WitDiskReadError::NotFound,
            DiskError::OutOfRange => WitDiskReadError::OutOfRange,
            DiskError::Io(message) => WitDiskReadError::Io(message),
            // Reads have no read-only arm; a provider that reports it anyway is still an
            // I/O failure from the guest's point of view.
            DiskError::ReadOnly => WitDiskReadError::Io("device is read-only".to_string()),
        }
    }
}

/// `eo9:disk/disk.write-error`. Variant order matches the WIT declaration.
#[derive(Clone, ComponentType, Lift, Lower)]
#[component(variant)]
enum WitDiskWriteError {
    #[component(name = "io")]
    Io(String),
    #[component(name = "out-of-range")]
    OutOfRange,
    #[component(name = "read-only")]
    ReadOnly,
}

impl From<DiskError> for WitDiskWriteError {
    fn from(value: DiskError) -> Self {
        match value {
            DiskError::OutOfRange => WitDiskWriteError::OutOfRange,
            DiskError::ReadOnly => WitDiskWriteError::ReadOnly,
            DiskError::Io(message) => WitDiskWriteError::Io(message),
            // Writes have no not-found arm; a vanished device is an I/O failure.
            DiskError::NotFound => WitDiskWriteError::Io("device is gone".to_string()),
        }
    }
}

/// The payload of `eo9:text/text.read-line`.
type ReadLineItem = Result<Option<String>, WitTextError>;
/// The return value of the owned-buffer fs reads (`read` / `exec-read`).
type FsReadReturn = (Resource<BufferRes>, Result<WitReadResult, WitFsError>);
/// The return value of the owned-buffer fs write.
type FsWriteReturn = (Resource<BufferRes>, Result<WitWriteResult, WitFsError>);
/// The return value of the owned-buffer disk read.
type DiskReadReturn = (Resource<BufferRes>, Result<WitReadResult, WitDiskReadError>);
/// The return value of the owned-buffer disk write.
type DiskWriteReturn = (
    Resource<BufferRes>,
    Result<WitWriteResult, WitDiskWriteError>,
);

/// The boxed-future shape `func_wrap_concurrent` expects.
type ConcurrentFuture<'a, R> = Pin<Box<dyn Future<Output = Result<R>> + Send + 'a>>;

// ---------------------------------------------------------------------------------------
// Always-available pieces: types, buffers, optional flavors
// ---------------------------------------------------------------------------------------

/// Register every always-available root-handle resource. These carry no authority: a
/// handle is a token, and every operation that accepts one is only linked when the
/// capability was granted. For the APIs that still use a types-only sibling interface
/// (text/time/entropy) the resource lives there; for `eo9:fs` it lives in the `fs`
/// interface itself (SPEC: "Multi-instance imports and type identity"), so the resource
/// is registered into that instance unconditionally and `add_fs` adds the operations
/// only when the capability was granted.
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
    linker.instance("eo9:disk/types@0.1.0")?.resource(
        "disk-impl",
        ResourceType::host::<DiskCap>(),
        |_, _| Ok(()),
    )?;
    Ok(())
}

/// Register `eo9:rt/diagnostics`: the write-once panic-message sink for the trap path.
///
/// Always registered — it is part of the runtime contract between the SDK and the
/// executor (the SDK's panic handler is its only intended caller), not a capability:
/// the host stores at most one bounded message per task, no guest can ever read it, and
/// it is surfaced in exactly one place — a subsequent `abnormal(trapped(reason))`
/// outcome. A task that calls it and does not trap has said nothing observable.
fn add_diagnostics(linker: &mut Linker<TaskState>) -> Result<()> {
    let mut diagnostics = linker.instance("eo9:rt/diagnostics@0.1.0")?;
    diagnostics.func_wrap(
        "report-panic",
        |store: StoreContextMut<'_, TaskState>, (message,): (String,)| -> Result<()> {
            store.data().report_panic(message);
            Ok(())
        },
    )?;
    Ok(())
}

/// Register only the `eo9:fs/fs` root-handle resource — the always-available shape of the
/// fs interface when the capability was *not* granted, so components that merely name the
/// handle type (a types-only `use`, e.g. `fs.none` or an `fs-optional` consumer) still
/// instantiate. The operations are added by `add_fs` only when fs was granted.
fn add_fs_handle_only(linker: &mut Linker<TaskState>) -> Result<()> {
    linker.instance("eo9:fs/fs@0.1.0")?.resource(
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
    fs.resource("fs-impl", ResourceType::host::<FsCap>(), |_, _| Ok(()))?;
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
// eo9:disk
// ---------------------------------------------------------------------------------------

/// Register `eo9:disk/disk` against the granted block-device provider. The `disk-impl`
/// root-handle resource itself lives in `eo9:disk/types` and is registered by
/// [`add_types`] unconditionally; only the operations require the grant.
fn add_disk(linker: &mut Linker<TaskState>) -> Result<()> {
    let mut disk = linker.instance("eo9:disk/disk@0.1.0")?;
    add_default_handle::<DiskCap>(&mut disk)?;

    disk.func_wrap(
        "size",
        |mut store: StoreContextMut<'_, TaskState>,
         (_dev,): (Resource<DiskCap>,)|
         -> Result<(u64,)> { Ok((store.data_mut().disk_provider()?.size(),)) },
    )?;

    disk.func_wrap_concurrent(
        "flush",
        |accessor: &Accessor<TaskState>,
         (_dev,): (Resource<DiskCap>,)|
         -> ConcurrentFuture<'_, (Result<(), WitDiskWriteError>,)> {
            Box::pin(async move {
                let op = accessor.with(|mut access| -> Result<_> {
                    Ok(access.data_mut().disk_provider()?.flush())
                })?;
                let result = op.await;
                Ok((result.map_err(WitDiskWriteError::from),))
            })
        },
    )?;

    disk.func_wrap_concurrent(
        "read",
        |accessor: &Accessor<TaskState>,
         (_dev, offset, dst): (Resource<DiskCap>, u64, Resource<BufferRes>)|
         -> ConcurrentFuture<'_, (DiskReadReturn,)> {
            Box::pin(async move {
                let buffer_rep = dst.rep();
                let op = accessor.with(|mut access| -> Result<_> {
                    let state = access.data_mut();
                    let bytes = state.buffers.take(buffer_rep)?;
                    Ok(state.disk_provider()?.read(offset, bytes))
                })?;
                let (bytes, result) = op.await;
                accessor.with(|mut access| access.data_mut().buffers.restore(buffer_rep, bytes));
                Ok(((
                    Resource::new_own(buffer_rep),
                    result
                        .map(|bytes_read| WitReadResult { bytes_read })
                        .map_err(WitDiskReadError::from),
                ),))
            })
        },
    )?;

    disk.func_wrap_concurrent(
        "write",
        |accessor: &Accessor<TaskState>,
         (_dev, offset, src): (Resource<DiskCap>, u64, Resource<BufferRes>)|
         -> ConcurrentFuture<'_, (DiskWriteReturn,)> {
            Box::pin(async move {
                let buffer_rep = src.rep();
                let op = accessor.with(|mut access| -> Result<_> {
                    let state = access.data_mut();
                    let bytes = state.buffers.take(buffer_rep)?;
                    Ok(state.disk_provider()?.write(offset, bytes))
                })?;
                let (bytes, result) = op.await;
                accessor.with(|mut access| access.data_mut().buffers.restore(buffer_rep, bytes));
                Ok(((
                    Resource::new_own(buffer_rep),
                    result
                        .map(|bytes_written| WitWriteResult { bytes_written })
                        .map_err(WitDiskWriteError::from),
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

    fn exec_provider(&mut self) -> Result<&mut crate::exec::ExecProvider> {
        self.providers
            .exec
            .as_mut()
            .ok_or_else(|| wasmtime::Error::msg("exec capability was not granted to this task"))
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

    fn disk_provider(&mut self) -> Result<&mut dyn crate::providers::DiskProvider> {
        self.providers
            .disk
            .as_deref_mut()
            .map(|provider| provider as &mut dyn crate::providers::DiskProvider)
            .ok_or_else(|| wasmtime::Error::msg("disk capability was not granted to this task"))
    }
}

// ---------------------------------------------------------------------------------------
// eo9:exec — component algebra, compile, task (granted only through the exec provider)
// ---------------------------------------------------------------------------------------

/// Host representation of `eo9:exec/component-algebra.component`.
pub struct AlgComponentRes;
/// Host representation of `eo9:exec/images.image`.
pub struct ExecImageRes;
/// Host representation of `eo9:exec/task.task` (a child task).
pub struct ChildTaskRes;

#[derive(Clone, Copy, ComponentType, Lift, Lower)]
#[component(enum)]
#[repr(u8)]
#[allow(dead_code)]
enum WitComponentKind {
    #[component(name = "binary")]
    Binary,
    #[component(name = "provider")]
    Provider,
}

#[derive(Clone, ComponentType, Lift, Lower)]
#[component(record)]
struct WitImportNeed {
    slot: String,
    interface: String,
    version: String,
    required: bool,
}

#[derive(Clone, ComponentType, Lift, Lower)]
#[component(record)]
struct WitExportSlot {
    name: String,
    interface: String,
    version: String,
}

#[derive(Clone, ComponentType, Lift, Lower)]
#[component(record)]
struct WitArgSpec {
    name: String,
    ty: String,
}

#[derive(Clone, ComponentType, Lift, Lower)]
#[component(record)]
struct WitComponentInfo {
    kind: WitComponentKind,
    imports: Vec<WitImportNeed>,
    exports: Vec<WitExportSlot>,
    args: Vec<WitArgSpec>,
}

#[derive(Clone, ComponentType, Lift, Lower)]
#[component(record)]
struct WitInterfaceRef {
    interface: String,
    version: Option<String>,
}

#[derive(Clone, ComponentType, Lift, Lower)]
#[component(variant)]
enum WitLoadError {
    #[component(name = "invalid-component")]
    InvalidComponent(String),
    #[component(name = "not-an-eo9-module")]
    NotAnEo9Module(String),
}

#[derive(Clone, ComponentType, Lift, Lower)]
#[component(variant)]
enum WitComposeError {
    #[component(name = "not-a-provider")]
    NotAProvider,
    #[component(name = "type-mismatch")]
    TypeMismatch(String),
    #[component(name = "internal")]
    Internal(String),
}

#[derive(Clone, ComponentType, Lift, Lower)]
#[component(variant)]
enum WitRestrictError {
    #[component(name = "required-outside-allow-list")]
    RequiredOutsideAllowList(Vec<String>),
    #[component(name = "invalid-allow-list")]
    InvalidAllowList(String),
    #[component(name = "internal")]
    Internal(String),
}

#[derive(Clone, ComponentType, Lift, Lower)]
#[component(variant)]
enum WitRenameError {
    #[component(name = "no-such-slot")]
    NoSuchSlot(String),
    #[component(name = "slot-collision")]
    SlotCollision(String),
    #[component(name = "internal")]
    Internal(String),
}

#[derive(Clone, ComponentType, Lift, Lower)]
#[component(variant)]
enum WitConfigureError {
    #[component(name = "not-a-provider")]
    NotAProvider,
    #[component(name = "no-config-interface")]
    NoConfigInterface,
    #[component(name = "invalid-args")]
    InvalidArgs(String),
    #[component(name = "internal")]
    Internal(String),
}

#[derive(Clone, Copy, ComponentType, Lift, Lower)]
#[component(record)]
struct WitCompileOpts {
    #[component(name = "debug-info")]
    debug_info: bool,
    #[component(name = "safepoint-maps")]
    safepoint_maps: bool,
}

#[derive(Clone, ComponentType, Lift, Lower)]
#[component(variant)]
enum WitCompileError {
    #[component(name = "not-a-binary")]
    NotABinary,
    #[component(name = "not-closed")]
    NotClosed(Vec<String>),
    #[component(name = "codegen")]
    Codegen(String),
}

#[derive(Clone, ComponentType, Lift, Lower)]
#[component(record)]
struct WitNamedArg {
    name: String,
    value: String,
}

#[derive(Clone, ComponentType, Lift, Lower)]
#[component(record)]
struct WitWaveValue {
    ty: String,
    value: String,
}

#[derive(Clone, ComponentType, Lift, Lower)]
#[component(variant)]
enum WitAbnormalExit {
    #[component(name = "trapped")]
    Trapped(String),
    #[component(name = "killed")]
    Killed,
}

#[derive(Clone, ComponentType, Lift, Lower)]
#[component(variant)]
enum WitProgramOutcome {
    #[component(name = "success")]
    Success(WitWaveValue),
    #[component(name = "failure")]
    Failure(WitWaveValue),
    #[component(name = "abnormal")]
    Abnormal(WitAbnormalExit),
}

#[derive(Clone, Copy, ComponentType, Lift, Lower)]
#[component(record)]
struct WitSpawnLimits {
    #[component(name = "max-memory")]
    max_memory: Option<u64>,
}

#[derive(Clone, ComponentType, Lift, Lower)]
#[component(variant)]
enum WitSpawnError {
    #[component(name = "bad-arguments")]
    BadArguments(String),
    #[component(name = "internal")]
    Internal(String),
}

#[derive(Clone, ComponentType, Lift, Lower)]
#[component(variant)]
enum WitResumeOutcome {
    #[component(name = "out-of-fuel")]
    OutOfFuel,
    #[component(name = "blocked")]
    Blocked,
    #[component(name = "done")]
    Done(WitProgramOutcome),
}

fn wit_outcome(outcome: &crate::outcome::Outcome) -> WitProgramOutcome {
    use crate::outcome::Outcome;
    match outcome {
        Outcome::Success(value) => WitProgramOutcome::Success(WitWaveValue {
            ty: value.ty.clone(),
            value: value.value.clone(),
        }),
        Outcome::Failure(value) => WitProgramOutcome::Failure(WitWaveValue {
            ty: value.ty.clone(),
            value: value.value.clone(),
        }),
        Outcome::Trapped(reason) => {
            WitProgramOutcome::Abnormal(WitAbnormalExit::Trapped(reason.clone()))
        }
        Outcome::Killed => WitProgramOutcome::Abnormal(WitAbnormalExit::Killed),
    }
}

fn wit_component_info(info: &eo9_component::ComponentInfo) -> WitComponentInfo {
    WitComponentInfo {
        kind: match info.kind {
            eo9_component::ComponentKind::Binary => WitComponentKind::Binary,
            eo9_component::ComponentKind::Provider => WitComponentKind::Provider,
        },
        imports: info
            .imports
            .iter()
            .map(|need| WitImportNeed {
                slot: need.slot.clone(),
                interface: need.interface.clone(),
                version: need.version.clone(),
                required: need.required,
            })
            .collect(),
        exports: info
            .exports
            .iter()
            .map(|slot| WitExportSlot {
                name: slot.name.clone(),
                interface: slot.interface.clone(),
                version: slot.version.clone(),
            })
            .collect(),
        args: info
            .args
            .iter()
            .map(|arg| WitArgSpec {
                name: arg.name.clone(),
                ty: arg.ty.clone(),
            })
            .collect(),
    }
}

/// Pull a consumed (`own`) component out of the table, releasing its byte budget.
fn take_component(
    exec: &mut crate::exec::ExecProvider,
    rep: u32,
) -> Result<eo9_component::Component> {
    let component = exec.components.take(rep)?;
    exec.component_bytes = exec
        .component_bytes
        .saturating_sub(component.save().len() as u64);
    Ok(component)
}

/// Insert an algebra result, accounting its bytes, and mint the guest handle.
fn insert_component(
    exec: &mut crate::exec::ExecProvider,
    component: eo9_component::Component,
) -> Result<Resource<AlgComponentRes>> {
    let size = component.save().len() as u64;
    let rep = exec.insert_component(component, size)?;
    Ok(Resource::new_own(rep))
}

fn add_exec(linker: &mut Linker<TaskState>) -> Result<()> {
    // ----- component-algebra ------------------------------------------------------------
    let mut algebra = linker.instance("eo9:exec/component-algebra@0.1.0")?;
    algebra.resource(
        "component",
        ResourceType::host::<AlgComponentRes>(),
        |mut store: StoreContextMut<'_, TaskState>, rep| {
            if let Ok(exec) = store.data_mut().exec_provider() {
                exec.free_component(rep);
            }
            Ok(())
        },
    )?;

    algebra.func_wrap(
        "load",
        |mut store: StoreContextMut<'_, TaskState>,
         (bytes,): (Vec<u8>,)|
         -> Result<(Result<Resource<AlgComponentRes>, WitLoadError>,)> {
            let exec = store.data_mut().exec_provider()?;
            if bytes.len() as u64 > crate::exec::MAX_COMPONENT_BYTES {
                return Err(wasmtime::Error::msg(
                    "component image exceeds the per-task component byte budget",
                ));
            }
            let size = bytes.len() as u64;
            Ok((match eo9_component::Component::load(bytes) {
                Ok(component) => {
                    let rep = exec.insert_component(component, size)?;
                    Ok(Resource::new_own(rep))
                }
                Err(eo9_component::LoadError::InvalidComponent(msg)) => {
                    Err(WitLoadError::InvalidComponent(msg))
                }
                Err(eo9_component::LoadError::NotAnEo9Module(msg)) => {
                    Err(WitLoadError::NotAnEo9Module(msg))
                }
            },))
        },
    )?;

    algebra.func_wrap(
        "save",
        |mut store: StoreContextMut<'_, TaskState>,
         (component,): (Resource<AlgComponentRes>,)|
         -> Result<(Vec<u8>,)> {
            let exec = store.data_mut().exec_provider()?;
            Ok((exec.components.get_mut(component.rep())?.save(),))
        },
    )?;

    algebra.func_wrap(
        "describe",
        |mut store: StoreContextMut<'_, TaskState>,
         (component,): (Resource<AlgComponentRes>,)|
         -> Result<(WitComponentInfo,)> {
            let exec = store.data_mut().exec_provider()?;
            let info = exec.components.get_mut(component.rep())?.describe();
            Ok((wit_component_info(&info),))
        },
    )?;

    algebra.func_wrap(
        "wiring",
        |mut store: StoreContextMut<'_, TaskState>,
         (component,): (Resource<AlgComponentRes>,)|
         -> Result<(String,)> {
            // Composition provenance is in-memory metadata on the algebra value (see
            // eo9-component's Wiring); a component this table built by composing renders
            // its full tree, a freshly loaded one renders as a single leaf.
            let exec = store.data_mut().exec_provider()?;
            Ok((exec.components.get_mut(component.rep())?.wiring_tree(),))
        },
    )?;

    algebra.func_wrap(
        "compose",
        |mut store: StoreContextMut<'_, TaskState>,
         (provider, consumer): (Resource<AlgComponentRes>, Resource<AlgComponentRes>)|
         -> Result<(Result<Resource<AlgComponentRes>, WitComposeError>,)> {
            let exec = store.data_mut().exec_provider()?;
            let provider = take_component(exec, provider.rep())?;
            let consumer = take_component(exec, consumer.rep())?;
            Ok((match eo9_component::compose(&provider, &consumer) {
                Ok(result) => Ok(insert_component(exec, result)?),
                Err(err) => Err(wit_compose_error(err)),
            },))
        },
    )?;

    algebra.func_wrap(
        "extend",
        |mut store: StoreContextMut<'_, TaskState>,
         (base, layer): (Resource<AlgComponentRes>, Resource<AlgComponentRes>)|
         -> Result<(Result<Resource<AlgComponentRes>, WitComposeError>,)> {
            let exec = store.data_mut().exec_provider()?;
            let base = take_component(exec, base.rep())?;
            let layer = take_component(exec, layer.rep())?;
            Ok((match eo9_component::extend(&base, &layer) {
                Ok(result) => Ok(insert_component(exec, result)?),
                Err(err) => Err(wit_compose_error(err)),
            },))
        },
    )?;

    algebra.func_wrap(
        "restrict",
        |mut store: StoreContextMut<'_, TaskState>,
         (component, allow): (Resource<AlgComponentRes>, Vec<WitInterfaceRef>)|
         -> Result<(Result<Resource<AlgComponentRes>, WitRestrictError>,)> {
            let exec = store.data_mut().exec_provider()?;
            let component = take_component(exec, component.rep())?;
            let allow: Vec<eo9_component::InterfaceRef> = allow
                .into_iter()
                .map(|entry| eo9_component::InterfaceRef {
                    interface: entry.interface,
                    version: entry.version,
                })
                .collect();
            Ok((match eo9_component::restrict(&component, &allow) {
                Ok(result) => Ok(insert_component(exec, result)?),
                Err(eo9_component::RestrictError::RequiredOutsideAllowList(names)) => {
                    Err(WitRestrictError::RequiredOutsideAllowList(names))
                }
                Err(eo9_component::RestrictError::InvalidAllowList(msg)) => {
                    Err(WitRestrictError::InvalidAllowList(msg))
                }
                Err(eo9_component::RestrictError::Internal(msg)) => {
                    Err(WitRestrictError::Internal(msg))
                }
            },))
        },
    )?;

    algebra.func_wrap(
        "rename",
        |mut store: StoreContextMut<'_, TaskState>,
         (component, old_name, new_name): (Resource<AlgComponentRes>, String, String)|
         -> Result<(Result<Resource<AlgComponentRes>, WitRenameError>,)> {
            let exec = store.data_mut().exec_provider()?;
            let component = take_component(exec, component.rep())?;
            Ok((
                match eo9_component::rename(&component, &old_name, &new_name) {
                    Ok(result) => Ok(insert_component(exec, result)?),
                    Err(eo9_component::RenameError::NoSuchSlot(msg)) => {
                        Err(WitRenameError::NoSuchSlot(msg))
                    }
                    Err(eo9_component::RenameError::SlotCollision(msg)) => {
                        Err(WitRenameError::SlotCollision(msg))
                    }
                    Err(eo9_component::RenameError::Internal(msg)) => {
                        Err(WitRenameError::Internal(msg))
                    }
                },
            ))
        },
    )?;

    algebra.func_wrap(
        "configure",
        |mut store: StoreContextMut<'_, TaskState>,
         (component, args): (Resource<AlgComponentRes>, Vec<WitNamedArg>)|
         -> Result<(Result<Resource<AlgComponentRes>, WitConfigureError>,)> {
            let exec = store.data_mut().exec_provider()?;
            let provider = take_component(exec, component.rep())?;
            let args: Vec<(String, String)> =
                args.into_iter().map(|arg| (arg.name, arg.value)).collect();
            Ok((match eo9_component::configure(&provider, &args) {
                Ok(result) => Ok(insert_component(exec, result)?),
                Err(eo9_component::ConfigureError::NotAProvider) => {
                    Err(WitConfigureError::NotAProvider)
                }
                Err(eo9_component::ConfigureError::NoConfigInterface) => {
                    Err(WitConfigureError::NoConfigInterface)
                }
                Err(eo9_component::ConfigureError::UnknownArgument(msg))
                | Err(eo9_component::ConfigureError::MissingArgument(msg)) => {
                    Err(WitConfigureError::InvalidArgs(msg))
                }
                Err(eo9_component::ConfigureError::InvalidArgument { name, message }) => {
                    Err(WitConfigureError::InvalidArgs(format!("{name}: {message}")))
                }
                Err(eo9_component::ConfigureError::Internal(msg)) => {
                    Err(WitConfigureError::Internal(msg))
                }
            },))
        },
    )?;

    // The record-only args interface carries no functions or resources, but rebuilt guests
    // import it as an instance; make sure the linker has a definition for it.
    let _ = linker.instance("eo9:exec/args@0.1.0")?;

    // ----- images + compile --------------------------------------------------------------
    let mut images = linker.instance("eo9:exec/images@0.1.0")?;
    images.resource(
        "image",
        ResourceType::host::<ExecImageRes>(),
        |mut store: StoreContextMut<'_, TaskState>, rep| {
            if let Ok(exec) = store.data_mut().exec_provider() {
                exec.images.free(rep);
            }
            Ok(())
        },
    )?;

    let mut compile = linker.instance("eo9:exec/compile@0.1.0")?;
    compile.func_wrap(
        "compile",
        |mut store: StoreContextMut<'_, TaskState>,
         (component, _opts): (Resource<AlgComponentRes>, WitCompileOpts)|
         -> Result<(Result<Resource<ExecImageRes>, WitCompileError>,)> {
            let exec = store.data_mut().exec_provider()?;
            let component = take_component(exec, component.rep())?;
            // Feed the executor the `implements`-stripped form: plain-named slots (a
            // renamed residual import, a multi-instance consumer) carry an annotation the
            // pinned runtime's parser predates, so compiling the saved bytes would fail
            // with an opaque parse error. `save`/`describe` keep the annotation; only the
            // bytes handed to codegen drop it. (Identical to the saved bytes when there is
            // no annotation, so a plain program is unaffected.)
            let bytes = component.executable_bytes();
            Ok((match crate::image::Image::compile(&exec.engine, bytes) {
                Ok(image) => {
                    let rep = exec.images.insert(image)?;
                    Ok(Resource::new_own(rep))
                }
                Err(crate::image::CompileError::NotABinary) => Err(WitCompileError::NotABinary),
                Err(err) => Err(WitCompileError::Codegen(err.to_string())),
            },))
        },
    )?;

    // ----- task ---------------------------------------------------------------------------
    let mut task = linker.instance("eo9:exec/task@0.1.0")?;
    task.resource(
        "task",
        ResourceType::host::<ChildTaskRes>(),
        |mut store: StoreContextMut<'_, TaskState>, rep| {
            // Dropping the handle kills the child (its store and in-flight work are dropped).
            if let Ok(exec) = store.data_mut().exec_provider() {
                exec.children.lock().unwrap().free(rep);
            }
            Ok(())
        },
    )?;

    task.func_wrap(
        "spawn",
        |mut store: StoreContextMut<'_, TaskState>,
         (image, args, limits): (Resource<ExecImageRes>, Vec<WitNamedArg>, WitSpawnLimits)|
         -> Result<(Result<Resource<ChildTaskRes>, WitSpawnError>,)> {
            let exec = store.data_mut().exec_provider()?;
            let providers = exec.policy.providers_for_child();
            let args: Vec<crate::wave::NamedArg> = args
                .into_iter()
                .map(|arg| crate::wave::NamedArg::new(arg.name, arg.value))
                .collect();
            let limits = crate::task::SpawnLimits {
                max_memory: limits.max_memory,
                max_table_elements: None,
            };
            let image = exec.images.get_mut(image.rep())?;
            let spawned = crate::task::Task::spawn(image, &args, limits, providers);
            let exec = store.data_mut().exec_provider()?;
            Ok((match spawned {
                Ok(child) => {
                    let rep = exec.children.lock().unwrap().insert(child)?;
                    Ok(Resource::new_own(rep))
                }
                Err(crate::task::SpawnError::BadArguments(msg)) => {
                    Err(WitSpawnError::BadArguments(msg))
                }
                Err(crate::task::SpawnError::Internal(msg)) => Err(WitSpawnError::Internal(msg)),
            },))
        },
    )?;

    task.func_wrap(
        "resume",
        |mut store: StoreContextMut<'_, TaskState>,
         (child, _fuel): (Resource<ChildTaskRes>, u64)|
         -> Result<(WitResumeOutcome,)> {
            // Executing a child inline would re-enter the event loop (wasmtime forbids
            // recursive `run_concurrent`), so guest-level resume is not supported yet:
            // children run on the parent's own resumes and are observed via `wait`.
            // If the child has already finished, report that; otherwise fail loudly.
            let exec = store.data_mut().exec_provider()?;
            let mut children = exec.children.lock().unwrap();
            let child = children.get_mut(child.rep())?;
            if let Some(outcome) = child.outcome() {
                return Ok((WitResumeOutcome::Done(wit_outcome(outcome)),));
            }
            Err(wasmtime::Error::msg(
                "eo9:exec/task.resume is not supported by this runtime yet: child tasks \
                 run on their parent's donated fuel; use wait (escalated, see plan/04)",
            ))
        },
    )?;

    task.func_wrap_concurrent(
        "wait",
        |accessor: &Accessor<TaskState>,
         (child,): (Resource<ChildTaskRes>,)|
         -> ConcurrentFuture<'_, (WitProgramOutcome,)> {
            Box::pin(async move {
                let rep = child.rep();
                let outcome = std::future::poll_fn(move |cx| {
                    accessor.with(|mut access| {
                        let exec = match access.data_mut().exec_provider() {
                            Ok(exec) => exec,
                            Err(err) => return std::task::Poll::Ready(Err(err)),
                        };
                        let children = exec.children.clone();
                        let mut children = children.lock().unwrap();
                        let child = match children.get_mut(rep) {
                            Ok(child) => child,
                            Err(err) => return std::task::Poll::Ready(Err(err)),
                        };
                        if let Some(outcome) = child.outcome() {
                            return std::task::Poll::Ready(Ok(wit_outcome(outcome)));
                        }
                        if child.is_runnable() {
                            // The child still needs CPU; it executes inside the parent's
                            // next resume iteration, so keep the parent awake.
                            cx.waker().wake_by_ref();
                        } else {
                            // The child is blocked on its own I/O: wake the parent when
                            // the child's doorbell rings.
                            let runnable = child.runnable();
                            let mut runnable = std::pin::pin!(runnable);
                            let _ = runnable.as_mut().poll(cx);
                        }
                        std::task::Poll::Pending
                    })
                })
                .await?;
                Ok((outcome,))
            })
        },
    )?;

    task.func_wrap_concurrent(
        "runnable",
        |accessor: &Accessor<TaskState>,
         (child,): (Resource<ChildTaskRes>,)|
         -> ConcurrentFuture<'_, ()> {
            Box::pin(async move {
                let rep = child.rep();
                std::future::poll_fn(move |cx| {
                    accessor.with(|mut access| {
                        let exec = match access.data_mut().exec_provider() {
                            Ok(exec) => exec,
                            Err(err) => return std::task::Poll::Ready(Err(err)),
                        };
                        let children = exec.children.clone();
                        let mut children = children.lock().unwrap();
                        let child = match children.get_mut(rep) {
                            Ok(child) => child,
                            Err(err) => return std::task::Poll::Ready(Err(err)),
                        };
                        if child.outcome().is_some() || child.is_runnable() {
                            return std::task::Poll::Ready(Ok(()));
                        }
                        let runnable = child.runnable();
                        let mut runnable = std::pin::pin!(runnable);
                        match runnable.as_mut().poll(cx) {
                            std::task::Poll::Ready(()) => std::task::Poll::Ready(Ok(())),
                            std::task::Poll::Pending => std::task::Poll::Pending,
                        }
                    })
                })
                .await?;
                Ok(())
            })
        },
    )?;

    task.func_wrap_concurrent(
        "kill",
        |accessor: &Accessor<TaskState>,
         (child,): (Resource<ChildTaskRes>,)|
         -> ConcurrentFuture<'_, (WitProgramOutcome,)> {
            Box::pin(async move {
                let outcome = accessor.with(|mut access| -> Result<_> {
                    let exec = access.data_mut().exec_provider()?;
                    let children = exec.children.clone();
                    let mut children = children.lock().unwrap();
                    // The entry stays in the table (as a finished task) so a later `wait`
                    // on the same handle resolves to abnormal(killed) instead of trapping.
                    Ok(wit_outcome(&children.get_mut(child.rep())?.kill_in_place()))
                })?;
                Ok((outcome,))
            })
        },
    )?;

    Ok(())
}

fn wit_compose_error(err: eo9_component::ComposeError) -> WitComposeError {
    match err {
        eo9_component::ComposeError::NotAProvider => WitComposeError::NotAProvider,
        eo9_component::ComposeError::TypeMismatch(msg) => WitComposeError::TypeMismatch(msg),
        eo9_component::ComposeError::Internal(msg) => WitComposeError::Internal(msg),
    }
}
