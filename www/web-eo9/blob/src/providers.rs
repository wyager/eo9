//! Browser root providers for the Eo9 OS APIs (the in-page analogue of the kernel's
//! `kernel/eo9-kernel/src/wasm/providers.rs` and the usermode linking in
//! `eo9-runtime::link`).
//!
//! These are the machine roots of the web VM: host implementations of `eo9:text`,
//! `eo9:time`, and `eo9:entropy`, registered on a component [`Linker`] so an unmodified
//! Eo9 program's imports resolve directly to the page — text → the page terminal, time →
//! the browser clocks (`Date.now` / `performance.now`), entropy → `crypto.getRandomValues`.
//!
//! The genuinely-blocking operations (`time.sleep`, `text.read-line`) call JSPI
//! [`WebAssembly.Suspending`] imports: from the blob's point of view the import call is
//! synchronous, but the browser parks the whole blob activation until the timer fires or
//! the visitor presses Enter, then resumes it — so the guest's await spans real wall-clock
//! time / real input without the blob needing a fiber backend (the guest call itself runs
//! on the vendored fiberless path, exactly as on the bare-metal kernel's polling executor).
//!
//! The WIT-shaped host types are structural copies of the kernel's (which themselves mirror
//! `eo9-runtime::link`); that crate targets host wasmtime and does not compile for
//! wasm32-unknown-unknown, so the shapes are mirrored rather than reused.

use std::boxed::Box;
use std::future::Future;
use std::pin::Pin;
use std::string::String;
use std::vec::Vec;

use wasmtime::component::{
    Accessor, ComponentType, Lift, Linker, LinkerInstance, Lower, Resource, ResourceType,
};
use wasmtime::{Result, StoreContextMut};

use crate::host;

/// Boxed future returned by the `func_wrap_concurrent` closures below.
type ConcurrentFuture<'a, R> = Pin<Box<dyn Future<Output = Result<R>> + Send + 'a>>;

/// Per-call ceiling on `eo9:entropy/entropy.get-bytes`, mirroring usermode and the kernel.
const MAX_ENTROPY_REQUEST_BYTES: u64 = 64 * 1024;

/// Upper bound on a single `read-line` line (mirrors the kernel's cap).
const MAX_READ_LINE_BYTES: usize = 4096;

/// Store data for programs run against the browser's root providers. text/time/entropy are
/// the browser APIs themselves (nothing to carry), but `eo9:fs`/`eo9:io` are backed by an
/// in-blob writable memory filesystem and buffer table (see `crate::fs`).
pub struct WebState {
    pub fs: crate::fs::MemFs,
    pub buffers: crate::fs::BufferTable,
    pub exec: crate::execsurface::ExecTables,
}

impl WebState {
    pub fn new() -> Self {
        let mut fs = crate::fs::MemFs::seeded();
        // Seed `/bin/<name>.wasm` so eosh's `resolve` finds the page's programs.
        crate::execsurface::seed_bin(&mut fs);
        WebState {
            fs,
            buffers: crate::fs::BufferTable::default(),
            exec: crate::execsurface::ExecTables::default(),
        }
    }
}

// --- Host resource representations (stateless tokens; all state is the browser) ----------

struct TextCap;
struct TimeCap;
struct EntropyCap;

// --- WIT-shaped host types ----------------------------------------------------------------

#[derive(Clone, Copy, ComponentType, Lift, Lower)]
#[component(enum)]
#[repr(u8)]
#[allow(dead_code)] // constructed only by the generated `Lift` impl
enum WitOutputStream {
    #[component(name = "out")]
    Out,
    #[component(name = "err")]
    Err,
}

#[derive(Clone, ComponentType, Lift, Lower)]
#[component(variant)]
#[allow(dead_code)] // the page terminal cannot fail; the arms satisfy the interface type
enum WitTextError {
    #[component(name = "closed")]
    Closed,
    #[component(name = "io")]
    Io(String),
}

#[derive(Clone, Copy, ComponentType, Lift, Lower)]
#[component(record)]
struct WitDatetime {
    seconds: i64,
    nanoseconds: u32,
}

#[derive(Clone, Copy, ComponentType, Lift, Lower)]
#[component(record)]
struct WitInstant {
    nanoseconds: u64,
}

// --- Registration --------------------------------------------------------------------------

/// Register all browser root providers (text, time, entropy — each with its `types`
/// resource). Used for an unrestricted run.
pub fn add_providers(linker: &mut Linker<WebState>) -> Result<()> {
    add_text(linker)?;
    add_time(linker)?;
    add_entropy(linker)?;
    Ok(())
}

/// Register only the root providers admitted by `allow` (the `only` allow-list recorded on
/// the component). `None` means unrestricted (everything). This is how `only`-attenuation
/// is enforced on the run path: the child runs the base artifact, so a capability the
/// `only` gate sealed must be withheld from the linker — a program importing a sealed-away
/// interface then fails at instantiation, and an optional sealed capability is observed as
/// absent. Each family registers its own authority-free `types` only alongside its
/// authority interface, so a program never needs a family's `types` unless it imports that
/// family.
pub fn add_providers_for(linker: &mut Linker<WebState>, allow: Option<&[String]>) -> Result<()> {
    if family_admitted(allow, "eo9:text/text") {
        add_text(linker)?;
    }
    if family_admitted(allow, "eo9:time/time") {
        add_time(linker)?;
    }
    if family_admitted(allow, "eo9:entropy/entropy") {
        add_entropy(linker)?;
    }
    Ok(())
}

/// True if `iface` (a full interface ref like `eo9:text/text`) is admitted by the allow-list.
/// `None` admits everything. An allow entry admits `iface` when it is the same interface or
/// the bare package of it (the `only eo9:text` shorthand) — version suffixes ignored.
pub fn family_admitted(allow: Option<&[String]>, iface: &str) -> bool {
    match allow {
        None => true,
        Some(list) => list.iter().any(|entry| admits(entry, iface)),
    }
}

fn admits(entry: &str, iface: &str) -> bool {
    let e = entry.split('@').next().unwrap_or(entry);
    let f = iface.split('@').next().unwrap_or(iface);
    // exact interface match, or a bare-package entry (`eo9:text`) matching `eo9:text/...`.
    e == f || (!e.contains('/') && f.strip_prefix(e).is_some_and(|rest| rest.starts_with('/')))
}

fn add_text_types(linker: &mut Linker<WebState>) -> Result<()> {
    linker.instance("eo9:text/types@0.1.0")?.resource(
        "text-impl",
        ResourceType::host::<TextCap>(),
        |_, _| Ok(()),
    )?;
    Ok(())
}

fn add_time_types(linker: &mut Linker<WebState>) -> Result<()> {
    linker.instance("eo9:time/types@0.1.0")?.resource(
        "time-impl",
        ResourceType::host::<TimeCap>(),
        |_, _| Ok(()),
    )?;
    Ok(())
}

fn add_entropy_types(linker: &mut Linker<WebState>) -> Result<()> {
    linker.instance("eo9:entropy/types@0.1.0")?.resource(
        "entropy-impl",
        ResourceType::host::<EntropyCap>(),
        |_, _| Ok(()),
    )?;
    Ok(())
}

/// `default: func() -> X-impl` — hand out the stateless root handle.
fn add_default_handle<C: 'static>(instance: &mut LinkerInstance<'_, WebState>) -> Result<()> {
    instance.func_wrap(
        "default",
        |_store: StoreContextMut<'_, WebState>, (): ()| -> Result<(Resource<C>,)> {
            Ok((Resource::new_own(0),))
        },
    )
}

/// `eo9:text/text`: the page terminal. Both output streams go to the one terminal pane
/// (stderr lines are prefixed so they are distinguishable).
fn add_text(linker: &mut Linker<WebState>) -> Result<()> {
    add_text_types(linker)?;
    let mut text = linker.instance("eo9:text/text@0.1.0")?;
    add_default_handle::<TextCap>(&mut text)?;

    text.func_wrap(
        "write",
        |_store: StoreContextMut<'_, WebState>,
         (_cap, to, content): (Resource<TextCap>, WitOutputStream, String)|
         -> Result<(core::result::Result<(), WitTextError>,)> {
            match to {
                WitOutputStream::Out => host::write_out(&content),
                WitOutputStream::Err => host::write_out(&std::format!("[stderr] {content}")),
            }
            Ok((Ok(()),))
        },
    )?;

    // One line from the page terminal's input box. The JSPI `Suspending` import parks the
    // whole blob until the visitor presses Enter (or signals end-of-input), then resumes it
    // with the line — the same contract the kernel's PL011 read-line future provides.
    text.func_wrap_concurrent(
        "read-line",
        |_accessor: &Accessor<WebState>,
         (_cap,): (Resource<TextCap>,)|
         -> ConcurrentFuture<'_, (core::result::Result<Option<String>, WitTextError>,)> {
            Box::pin(async move { Ok((Ok(host::read_line(MAX_READ_LINE_BYTES)),)) })
        },
    )?;

    Ok(())
}

/// `eo9:time/time`: wall-clock seconds from `Date.now()`, monotonic time from
/// `performance.now()`, sleeps from `setTimeout` via JSPI.
fn add_time(linker: &mut Linker<WebState>) -> Result<()> {
    add_time_types(linker)?;
    let mut time = linker.instance("eo9:time/time@0.1.0")?;
    add_default_handle::<TimeCap>(&mut time)?;

    time.func_wrap(
        "now",
        |_store: StoreContextMut<'_, WebState>,
         (_cap,): (Resource<TimeCap>,)|
         -> Result<(WitDatetime,)> {
            let ms = host::now_ms();
            let seconds = (ms / 1000.0).floor();
            let nanoseconds = ((ms - seconds * 1000.0) * 1_000_000.0).max(0.0) as u32;
            Ok((WitDatetime {
                seconds: seconds as i64,
                nanoseconds,
            },))
        },
    )?;

    time.func_wrap(
        "monotonic-now",
        |_store: StoreContextMut<'_, WebState>,
         (_cap,): (Resource<TimeCap>,)|
         -> Result<(WitInstant,)> {
            Ok((WitInstant {
                nanoseconds: host::monotonic_ns(),
            },))
        },
    )?;

    time.func_wrap(
        "resolution",
        |_store: StoreContextMut<'_, WebState>, (_cap,): (Resource<TimeCap>,)| -> Result<(u64,)> {
            // performance.now() is millisecond-ish (coarsened by the browser); report 1 ms.
            Ok((1_000_000,))
        },
    )?;

    // The awaited operation: parks the blob on a real browser timer via the JSPI import,
    // so the guest's await spans genuine wall-clock time.
    time.func_wrap_concurrent(
        "sleep",
        |_accessor: &Accessor<WebState>,
         (_cap, duration_ns): (Resource<TimeCap>, u64)|
         -> ConcurrentFuture<'_, ()> {
            Box::pin(async move {
                host::sleep_ms(duration_ns as f64 / 1_000_000.0);
                Ok(())
            })
        },
    )?;

    Ok(())
}

/// `eo9:entropy/entropy`: `crypto.getRandomValues` — the browser's CSPRNG is the machine's
/// entropy root here.
fn add_entropy(linker: &mut Linker<WebState>) -> Result<()> {
    add_entropy_types(linker)?;
    let mut entropy = linker.instance("eo9:entropy/entropy@0.1.0")?;
    add_default_handle::<EntropyCap>(&mut entropy)?;

    entropy.func_wrap(
        "get-bytes",
        |_store: StoreContextMut<'_, WebState>,
         (_cap, len): (Resource<EntropyCap>, u64)|
         -> Result<(Vec<u8>,)> {
            if len > MAX_ENTROPY_REQUEST_BYTES {
                return Err(wasmtime::Error::msg(
                    "entropy get-bytes request exceeds the per-call cap",
                ));
            }
            let mut out = std::vec![0u8; len as usize];
            host::random_fill(&mut out);
            Ok((out,))
        },
    )?;

    entropy.func_wrap(
        "get-u64",
        |_store: StoreContextMut<'_, WebState>,
         (_cap,): (Resource<EntropyCap>,)|
         -> Result<(u64,)> {
            let mut bytes = [0u8; 8];
            host::random_fill(&mut bytes);
            Ok((u64::from_le_bytes(bytes),))
        },
    )?;

    Ok(())
}
