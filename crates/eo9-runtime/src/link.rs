//! Linker assembly: wiring a task's imports to the root host providers.
//!
//! A task's imports are satisfied from (a) its own fused composition — already inside the
//! component, nothing to do here — and (b) the root providers handed to `spawn`. Anything
//! left unsatisfied is a spawn error (the loader rule from SPEC "WASM runtime"); that check
//! happens naturally when the linker instantiates the component.
//!
//! Only the interfaces for which a provider was actually supplied are registered, so a
//! component that imports `eo9:text/text` spawned without a text provider fails to link —
//! capability absence is expressed by composition (e.g. `text.none`), not by stub host
//! functions.
//!
//! The host shapes below mirror `wit/text`, `wit/time`, and `wit/entropy`. Host calls that
//! return a WIT `future<T>` create the future with a producer that polls the provider's
//! pending operation; the waker that reaches the provider is the task's doorbell.

use std::pin::Pin;
use std::task::{Context, Poll};

use wasmtime::component::{
    ComponentType, FutureProducer, FutureReader, Lift, Linker, LinkerInstance, Lower, Resource,
    ResourceType,
};
use wasmtime::{Result, StoreContextMut};

use crate::providers::{BoxOp, Datetime, OutputStream, Providers, TextError};
use crate::task::TaskState;

/// Register host implementations for every provider present in `providers`.
pub(crate) fn add_providers(linker: &mut Linker<TaskState>, providers: &Providers) -> Result<()> {
    if providers.text.is_some() {
        add_text(linker)?;
    }
    if providers.time.is_some() {
        add_time(linker)?;
    }
    if providers.entropy.is_some() {
        add_entropy(linker)?;
    }
    Ok(())
}

// ---------------------------------------------------------------------------------------
// WIT-shaped host types (structurally matched against the eo9 interfaces)
// ---------------------------------------------------------------------------------------

/// Host representation of the `eo9:text/types.text-impl` resource (stateless token: all
/// state lives in the provider).
pub struct TextCap;
/// Host representation of `eo9:time/types.time-impl`.
pub struct TimeCap;
/// Host representation of `eo9:entropy/types.entropy-impl`.
pub struct EntropyCap;

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

// ---------------------------------------------------------------------------------------
// future<T> producers backed by provider operations
// ---------------------------------------------------------------------------------------

/// Adapts a pending provider operation into a Component Model `future<T>` payload producer.
struct OpProducer<T, U, F> {
    op: BoxOp<T>,
    map: F,
    _marker: std::marker::PhantomData<fn() -> U>,
}

impl<T, U, F> OpProducer<T, U, F>
where
    F: Fn(T) -> U + Send + 'static,
{
    fn new(op: BoxOp<T>, map: F) -> Self {
        Self {
            op,
            map,
            _marker: std::marker::PhantomData,
        }
    }
}

impl<T, U, F> FutureProducer<TaskState> for OpProducer<T, U, F>
where
    T: Send + 'static,
    U: Send + 'static,
    F: Fn(T) -> U + Send + Unpin + 'static,
{
    type Item = U;

    fn poll_produce(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        _store: StoreContextMut<TaskState>,
        finish: bool,
    ) -> Poll<Result<Option<U>>> {
        let this = self.get_mut();
        match this.op.as_mut().poll(cx) {
            Poll::Ready(value) => Poll::Ready(Ok(Some((this.map)(value)))),
            Poll::Pending if finish => Poll::Ready(Ok(None)),
            Poll::Pending => Poll::Pending,
        }
    }
}

// ---------------------------------------------------------------------------------------
// eo9:text
// ---------------------------------------------------------------------------------------

/// The payload of `eo9:text/text.read-line`'s returned future.
type ReadLineItem = Result<Option<String>, WitTextError>;

fn add_text(linker: &mut Linker<TaskState>) -> Result<()> {
    let mut types = linker.instance("eo9:text/types@0.1.0")?;
    types.resource("text-impl", ResourceType::host::<TextCap>(), |_, _| Ok(()))?;

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

    text.func_wrap(
        "read-line",
        |mut store: StoreContextMut<'_, TaskState>,
         (_cap,): (Resource<TextCap>,)|
         -> Result<(FutureReader<ReadLineItem>,)> {
            let op = store.data_mut().text_provider()?.read_line();
            let producer = OpProducer::new(op, |result: Result<Option<String>, TextError>| {
                result.map_err(WitTextError::from)
            });
            Ok((FutureReader::new(&mut store, producer)?,))
        },
    )?;

    Ok(())
}

// ---------------------------------------------------------------------------------------
// eo9:time
// ---------------------------------------------------------------------------------------

fn add_time(linker: &mut Linker<TaskState>) -> Result<()> {
    let mut types = linker.instance("eo9:time/types@0.1.0")?;
    types.resource("time-impl", ResourceType::host::<TimeCap>(), |_, _| Ok(()))?;

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

    time.func_wrap(
        "sleep",
        |mut store: StoreContextMut<'_, TaskState>,
         (_cap, duration_ns): (Resource<TimeCap>, u64)|
         -> Result<(FutureReader<()>,)> {
            let op = store.data_mut().time_provider()?.sleep(duration_ns);
            let producer = OpProducer::new(op, |()| ());
            Ok((FutureReader::new(&mut store, producer)?,))
        },
    )?;

    Ok(())
}

// ---------------------------------------------------------------------------------------
// eo9:entropy
// ---------------------------------------------------------------------------------------

fn add_entropy(linker: &mut Linker<TaskState>) -> Result<()> {
    let mut types = linker.instance("eo9:entropy/types@0.1.0")?;
    types.resource(
        "entropy-impl",
        ResourceType::host::<EntropyCap>(),
        |_, _| Ok(()),
    )?;

    let mut entropy = linker.instance("eo9:entropy/entropy@0.1.0")?;
    add_default_handle::<EntropyCap>(&mut entropy)?;

    entropy.func_wrap(
        "get-bytes",
        |mut store: StoreContextMut<'_, TaskState>,
         (_cap, len): (Resource<EntropyCap>, u64)|
         -> Result<(Vec<u8>,)> {
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
}
