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
    Accessor, ComponentType, Lift, Linker, LinkerInstance, Lower, Resource, ResourceType, Val,
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
    /// The page-canvas framebuffer's backing copy (`eo9:gfx`): tightly packed xrgb8888,
    /// `GFX_WIDTH * GFX_HEIGHT * 4` bytes, allocated on the first gfx operation. `read`
    /// answers from here (the provider's own copy of what was presented, per the WIT
    /// contract); the canvas blit is display-only.
    gfx: Option<Vec<u8>>,
    /// The store's `eo9:rt/diagnostics` slot: the panic message the guest reported just
    /// before trapping, if any (write-once, bounded — see [`WebState::report_panic`]).
    /// Read host-side when a trap is rendered into a `trapped(reason)`; never guest-readable.
    pub panic_message: Option<String>,
    /// Partial-line buffers for the two terminal output streams. Guests write line text
    /// and the terminating newline as separate `text.write` calls; the page renders one
    /// terminal line per host write, so emitting on every call produced doubled spacing
    /// and stray prefix-only lines (user study 10, finding 12). Buffering until a newline
    /// arrives renders exactly the lines the guest meant to print.
    out_line: String,
    err_line: String,
}

/// Ceiling on a reported panic message (bytes); anything longer is truncated on a char
/// boundary — mirrors `eo9-runtime`'s bound so diagnostics never hold unbounded data.
const MAX_PANIC_MESSAGE_BYTES: usize = 1024;

impl WebState {
    pub fn new() -> Self {
        let mut fs = crate::fs::MemFs::seeded();
        // Seed `/bin/<name>.wasm` so eosh's `resolve` finds the page's programs.
        crate::execsurface::seed_bin(&mut fs);
        // Leave the session manifest where eosh's `env` builtin reads it (the same
        // `eo9-session 1` format the usermode and kernel embedders write to `/session`):
        // a plain-text, informational description of what this browser session grants.
        // The linker registrations in this module are the authority; this only describes
        // them so `env` has something honest to say in the page.
        fs.seed_file("/session", session_manifest().as_bytes());
        WebState {
            fs,
            buffers: crate::fs::BufferTable::default(),
            exec: crate::execsurface::ExecTables::default(),
            gfx: None,
            panic_message: None,
            out_line: String::new(),
            err_line: String::new(),
        }
    }

    /// The gfx backing framebuffer, allocated (black) on first use.
    fn gfx_backing(&mut self) -> &mut Vec<u8> {
        self.gfx
            .get_or_insert_with(|| std::vec![0u8; (GFX_WIDTH * GFX_HEIGHT * 4) as usize])
    }

    /// Append terminal output, emitting one host write per *complete* line. Standard-error
    /// lines are prefixed with U+0001 — an in-band marker the page strips and styles with
    /// its error class (and the verify harnesses strip before matching). Empty content is
    /// a no-op; a bare newline terminates whatever is buffered (possibly an intentionally
    /// blank line).
    ///
    /// Module-private: the only caller is this module's `eo9:text/text.write` handler
    /// (and `WitOutputStream` is itself module-private).
    fn write_text(&mut self, stream: WitOutputStream, content: &str) {
        let (buffer, marker) = match stream {
            WitOutputStream::Out => (&mut self.out_line, ""),
            WitOutputStream::Err => (&mut self.err_line, "\u{1}"),
        };
        buffer.push_str(content);
        while let Some(newline) = buffer.find('\n') {
            let line: String = buffer.drain(..=newline).collect();
            let line = line.trim_end_matches('\n');
            host::write_out(&std::format!("{marker}{line}"));
        }
    }

    /// Flush buffered *partial* lines to the page. Reading input is the terminal's flush
    /// point — exactly C stdio's contract — because a partial line written just before a
    /// read is a prompt: eosh writes `eosh> ` with no newline and then calls `read-line`.
    /// Without this flush the prompt sits in the buffer accumulating one `eosh> ` per
    /// read until the shell's next complete line drags them all out glued together
    /// ("eosh> eosh> eosh> ok: greeted" — the prompt-accumulation regression), and the
    /// page attaches the visitor's typing to whatever line happened to render last.
    /// Also called when a run ends, so a program whose final output lacks a trailing
    /// newline still gets that text onto the page.
    pub fn flush_partial_lines(&mut self) {
        if !self.out_line.is_empty() {
            let line = core::mem::take(&mut self.out_line);
            host::write_out(&line);
        }
        if !self.err_line.is_empty() {
            let line = core::mem::take(&mut self.err_line);
            host::write_out(&std::format!("\u{1}{line}"));
        }
    }

    /// Record the guest's reported panic message (write-once: the first report wins).
    pub fn report_panic(&mut self, message: String) {
        if self.panic_message.is_some() {
            return;
        }
        let mut message = message;
        if message.len() > MAX_PANIC_MESSAGE_BYTES {
            let mut end = MAX_PANIC_MESSAGE_BYTES;
            while !message.is_char_boundary(end) {
                end -= 1;
            }
            message.truncate(end);
            message.push('…');
        }
        self.panic_message = Some(message);
    }
}

/// The session manifest the browser embedder leaves at `/session` for eosh's `env`
/// builtin (the `eo9-session 1` format from eosh-core's `envinfo`; keep in sync with
/// `eo9::providers::session_manifest` and the kernel's equivalent). Purely informational:
/// it describes exactly what `add_providers`/`add_providers_for` and the exec surface
/// register for this page — the registrations themselves are the authority.
fn session_manifest() -> &'static str {
    "eo9-session 1\n\
     shell text the page terminal\n\
     shell time the browser clock\n\
     shell entropy the browser's crypto random generator\n\
     shell fs an in-memory filesystem seeded with /welcome.txt, /docs, and the /bin programs\n\
     shell exec the component algebra, the in-browser compiler, and spawn\n\
     child text the page terminal (shared with the shell)\n\
     child time the browser clock\n\
     child entropy the browser's crypto random generator\n\
     child fs a fresh in-memory filesystem per run; writes do not persist between commands\n\
     child gfx a 320x200 framebuffer rendered onto this page (try `draw`)\n\
     note everything runs inside this page; nothing reaches the network or your machine\n\
     note programs run from this shell do not receive exec (no nested spawning in the browser)\n\
     note restrict any command with `only` (e.g. `only eo9:text,eo9:time $ hello`)\n"
}

/// Render a trap reason, folding in the guest's reported panic message when one exists —
/// the same shape usermode's `eo9-runtime::trap` produces: the SDK panic handler reports
/// "<message> at <file>:<line>" through `eo9:rt/diagnostics.report-panic` just before the
/// panic lowers to the `unreachable` trap, so a message in the slot means the error that
/// followed is that panic.
pub fn trapped_reason(error: &wasmtime::Error, panic_message: Option<&str>) -> String {
    match panic_message {
        Some(message) => std::format!("guest panicked: {message} — {error}"),
        None => std::format!("{error}"),
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

/// Register all browser root providers (text, time, entropy, gfx — each with its `types`
/// resource). Used for an unrestricted run.
pub fn add_providers(linker: &mut Linker<WebState>) -> Result<()> {
    add_diagnostics(linker)?;
    add_text(linker)?;
    add_time(linker)?;
    add_entropy(linker)?;
    add_gfx(linker)?;
    add_svc_absent(linker)?;
    Ok(())
}

/// Host representation of `eo9:svc/detach.detach-impl` (never minted in the browser).
struct SvcDetachCap;
/// Host representation of `eo9:svc/services.services-impl` (never minted in the browser).
struct SvcServicesCap;

/// `eo9:svc` — registered as **absent**: the page has no service registry (executor v1
/// is usermode; a browser registry is a recorded follow-up — docs/design/executor-model.md).
/// Mirrors the kernel's `add_svc_absent`: eosh imports both the `-optional` flavors (the
/// honest am-I-granted signal it checks) and the full interfaces (to call when granted),
/// so all four must be registered for eosh to instantiate at all. The optionals answer
/// `none`; the operations are registered through the dynamic (`func_new`) API and refuse
/// with a clear message if ever called — which a well-behaved client never does after
/// seeing `none`.
fn add_svc_absent(linker: &mut Linker<WebState>) -> Result<()> {
    fn refuse(
        _store: StoreContextMut<'_, WebState>,
        _ty: wasmtime::component::types::ComponentFunc,
        _params: &[Val],
        _results: &mut [Val],
    ) -> Result<()> {
        Err(wasmtime::Error::msg(
            "eo9:svc is not available in the browser: background services need a service \
             registry (usermode `eo9 --svc` has one today)",
        ))
    }

    fn answer_none(
        _store: StoreContextMut<'_, WebState>,
        _ty: wasmtime::component::types::ComponentFunc,
        _params: &[Val],
        results: &mut [Val],
    ) -> Result<()> {
        results[0] = Val::Option(None);
        Ok(())
    }

    let mut detach = linker.instance("eo9:svc/detach@0.1.0")?;
    detach.resource(
        "detach-impl",
        ResourceType::host::<SvcDetachCap>(),
        |_, _| Ok(()),
    )?;
    detach.func_new("default", refuse)?;
    detach.func_new("detach", refuse)?;

    let mut detach_optional = linker.instance("eo9:svc/detach-optional@0.1.0")?;
    detach_optional.func_new("default", answer_none)?;

    let mut services = linker.instance("eo9:svc/services@0.1.0")?;
    services.resource(
        "services-impl",
        ResourceType::host::<SvcServicesCap>(),
        |_, _| Ok(()),
    )?;
    for operation in ["default", "list", "status", "log", "stop", "clear"] {
        services.func_new(operation, refuse)?;
    }

    let mut services_optional = linker.instance("eo9:svc/services-optional@0.1.0")?;
    services_optional.func_new("default", answer_none)?;

    Ok(())
}

/// Register `eo9:rt/diagnostics`: the write-once panic-message sink for the trap path.
/// Always registered, never gated by an `only` allow-list — it is runtime contract between
/// the SDK and the executor (every SDK-built component imports it from its panic handler),
/// not a capability: the host stores at most one bounded message per store, no guest can
/// ever read it back, and it is observable in exactly one place — a subsequent
/// `trapped(reason)`. Mirrors `eo9-runtime::link::add_diagnostics` and the kernel.
pub fn add_diagnostics(linker: &mut Linker<WebState>) -> Result<()> {
    let mut diagnostics = linker.instance("eo9:rt/diagnostics@0.1.0")?;
    diagnostics.func_wrap(
        "report-panic",
        |mut store: StoreContextMut<'_, WebState>, (message,): (String,)| -> Result<()> {
            store.data_mut().report_panic(message);
            Ok(())
        },
    )?;
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
    // The diagnostics sink is never subject to the allow-list (`only` always admits
    // `eo9:rt/diagnostics` — see `eo9-component::restrict`): it grants no authority and is
    // required for any SDK-built child to instantiate at all.
    add_diagnostics(linker)?;
    if family_admitted(allow, "eo9:text/text") {
        add_text(linker)?;
    }
    if family_admitted(allow, "eo9:time/time") {
        add_time(linker)?;
    }
    if family_admitted(allow, "eo9:entropy/entropy") {
        add_entropy(linker)?;
    }
    if family_admitted(allow, "eo9:gfx/gfx") {
        add_gfx(linker)?;
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

/// `eo9:text/text`: the page terminal. Both output streams go to the one terminal pane;
/// writes are line-buffered in [`WebState::write_text`], and standard-error lines carry an
/// in-band U+0001 marker the page styles as errors (no visible prefix, no stray blank
/// lines — user study 10, finding 12).
fn add_text(linker: &mut Linker<WebState>) -> Result<()> {
    add_text_types(linker)?;
    let mut text = linker.instance("eo9:text/text@0.1.0")?;
    add_default_handle::<TextCap>(&mut text)?;

    text.func_wrap(
        "write",
        |mut store: StoreContextMut<'_, WebState>,
         (_cap, to, content): (Resource<TextCap>, WitOutputStream, String)|
         -> Result<(core::result::Result<(), WitTextError>,)> {
            store.data_mut().write_text(to, &content);
            Ok((Ok(()),))
        },
    )?;

    // One line from the page terminal's input box. The JSPI `Suspending` import parks the
    // whole blob until the visitor presses Enter (or signals end-of-input), then resumes it
    // with the line — the same contract the kernel's PL011 read-line future provides.
    // Reading flushes buffered partial output first: the partial line written just before
    // a read is the prompt, and it must be on the page before we park on the keyboard.
    text.func_wrap_concurrent(
        "read-line",
        |accessor: &Accessor<WebState>,
         (_cap,): (Resource<TextCap>,)|
         -> ConcurrentFuture<'_, (core::result::Result<Option<String>, WitTextError>,)> {
            Box::pin(async move {
                accessor.with(|mut access| access.data_mut().flush_partial_lines());
                Ok((Ok(host::read_line(MAX_READ_LINE_BYTES)),))
            })
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

// --- eo9:gfx — the page canvas --------------------------------------------------------------

/// The page framebuffer's fixed geometry: small enough that a Pulley-interpreted guest
/// fills it quickly, large enough to be a real picture. The page sizes its canvas from
/// the dimensions the blit carries, so this constant is the single source of truth.
const GFX_WIDTH: u32 = 320;
const GFX_HEIGHT: u32 = 200;

struct GfxCap;

#[derive(Clone, Copy, ComponentType, Lift, Lower)]
#[component(enum)]
#[repr(u8)]
#[allow(dead_code)] // constructed only by the generated `Lift` impl
enum WitPixelFormat {
    #[component(name = "xrgb8888")]
    Xrgb8888,
}

#[derive(Clone, Copy, ComponentType, Lift, Lower)]
#[component(record)]
struct WitModeInfo {
    width: u32,
    height: u32,
    stride: u32,
    format: WitPixelFormat,
}

#[derive(Clone, Copy, ComponentType, Lift, Lower)]
#[component(record)]
struct WitRect {
    x: u32,
    y: u32,
    width: u32,
    height: u32,
}

#[derive(Clone, ComponentType, Lift, Lower)]
#[component(variant)]
#[allow(dead_code)] // the canvas never refuses or fails I/O; the arms satisfy the type
enum WitGfxError {
    #[component(name = "denied")]
    Denied,
    #[component(name = "out-of-bounds")]
    OutOfBounds,
    #[component(name = "bad-buffer")]
    BadBuffer(String),
    #[component(name = "io")]
    Io(String),
}

/// The rectangle's byte size (tightly packed rows) if it lies entirely within the mode;
/// `out-of-bounds` otherwise. Zero-area rectangles inside the mode are valid (and their
/// size is 0 — the operations treat them as successful no-ops, per the WIT contract).
fn gfx_rect_len(rect: &WitRect) -> core::result::Result<usize, WitGfxError> {
    let right = u64::from(rect.x) + u64::from(rect.width);
    let bottom = u64::from(rect.y) + u64::from(rect.height);
    if right > u64::from(GFX_WIDTH) || bottom > u64::from(GFX_HEIGHT) {
        return Err(WitGfxError::OutOfBounds);
    }
    Ok(rect.width as usize * rect.height as usize * 4)
}

type GfxBufferReturn = (
    Resource<crate::fs::BufferRes>,
    core::result::Result<(), WitGfxError>,
);

/// `eo9:gfx/gfx`: a real framebuffer rendered onto the page — `present` blits onto a
/// canvas under the terminal (revealed on first use), `read` answers from the provider's
/// backing copy (the WIT contract: a screenshot of the data path, not a host readback),
/// so `draw` at the browser prompt both paints the page and verifies its own checksum.
fn add_gfx(linker: &mut Linker<WebState>) -> Result<()> {
    linker.instance("eo9:gfx/types@0.1.0")?.resource(
        "gfx-impl",
        ResourceType::host::<GfxCap>(),
        |_, _| Ok(()),
    )?;
    let mut gfx = linker.instance("eo9:gfx/gfx@0.1.0")?;
    add_default_handle::<GfxCap>(&mut gfx)?;

    gfx.func_wrap(
        "mode",
        |_store: StoreContextMut<'_, WebState>,
         (_cap,): (Resource<GfxCap>,)|
         -> Result<(core::result::Result<WitModeInfo, WitGfxError>,)> {
            Ok((Ok(WitModeInfo {
                width: GFX_WIDTH,
                height: GFX_HEIGHT,
                stride: GFX_WIDTH * 4,
                format: WitPixelFormat::Xrgb8888,
            }),))
        },
    )?;

    gfx.func_wrap_concurrent(
        "present",
        |accessor: &Accessor<WebState>,
         (_cap, dst, src): (Resource<GfxCap>, WitRect, Resource<crate::fs::BufferRes>)|
         -> ConcurrentFuture<'_, (GfxBufferReturn,)> {
            Box::pin(async move {
                let buffer_rep = src.rep();
                let result = accessor.with(|mut access| -> Result<_> {
                    let state = access.data_mut();
                    let needed = match gfx_rect_len(&dst) {
                        Ok(len) => len,
                        Err(err) => return Ok(Err(err)),
                    };
                    if needed == 0 {
                        return Ok(Ok(()));
                    }
                    let bytes = state.buffers.bytes(buffer_rep)?;
                    if bytes.len() < needed {
                        return Ok(Err(WitGfxError::BadBuffer(std::format!(
                            "present needs {needed} bytes for {}x{}, got {}",
                            dst.width,
                            dst.height,
                            bytes.len()
                        ))));
                    }
                    let pixels = bytes[..needed].to_vec();
                    let backing = state.gfx_backing();
                    let row_bytes = dst.width as usize * 4;
                    for row in 0..dst.height as usize {
                        let to = ((dst.y as usize + row) * GFX_WIDTH as usize + dst.x as usize) * 4;
                        backing[to..to + row_bytes]
                            .copy_from_slice(&pixels[row * row_bytes..][..row_bytes]);
                    }
                    host::gfx_present(
                        &pixels,
                        (GFX_WIDTH, GFX_HEIGHT),
                        (dst.x, dst.y, dst.width, dst.height),
                    );
                    Ok(Ok(()))
                })?;
                Ok(((Resource::new_own(buffer_rep), result),))
            })
        },
    )?;

    gfx.func_wrap_concurrent(
        "read",
        |accessor: &Accessor<WebState>,
         (_cap, src, dst): (Resource<GfxCap>, WitRect, Resource<crate::fs::BufferRes>)|
         -> ConcurrentFuture<'_, (GfxBufferReturn,)> {
            Box::pin(async move {
                let buffer_rep = dst.rep();
                let result = accessor.with(|mut access| -> Result<_> {
                    let state = access.data_mut();
                    let needed = match gfx_rect_len(&src) {
                        Ok(len) => len,
                        Err(err) => return Ok(Err(err)),
                    };
                    if needed == 0 {
                        return Ok(Ok(()));
                    }
                    // Copy the rows out of the backing first (split the borrows by going
                    // through a temporary, the same pattern as the fs read handler).
                    let mut packed = std::vec![0u8; needed];
                    let row_bytes = src.width as usize * 4;
                    {
                        let backing = state.gfx_backing();
                        for row in 0..src.height as usize {
                            let from =
                                ((src.y as usize + row) * GFX_WIDTH as usize + src.x as usize) * 4;
                            packed[row * row_bytes..][..row_bytes]
                                .copy_from_slice(&backing[from..from + row_bytes]);
                        }
                    }
                    let bytes = state.buffers.bytes(buffer_rep)?;
                    if bytes.len() < needed {
                        return Ok(Err(WitGfxError::BadBuffer(std::format!(
                            "read needs at least {needed} bytes for {}x{}, got {}",
                            src.width,
                            src.height,
                            bytes.len()
                        ))));
                    }
                    bytes[..needed].copy_from_slice(&packed);
                    Ok(Ok(()))
                })?;
                Ok(((Resource::new_own(buffer_rep), result),))
            })
        },
    )?;

    gfx.func_wrap_concurrent(
        "clear",
        |accessor: &Accessor<WebState>,
         (_cap, dst, color): (Resource<GfxCap>, WitRect, u32)|
         -> ConcurrentFuture<'_, (core::result::Result<(), WitGfxError>,)> {
            Box::pin(async move {
                let result = accessor.with(|mut access| -> Result<_> {
                    let state = access.data_mut();
                    let needed = match gfx_rect_len(&dst) {
                        Ok(len) => len,
                        Err(err) => return Ok(Err(err)),
                    };
                    if needed == 0 {
                        return Ok(Ok(()));
                    }
                    // `0x00RRGGBB` → memory bytes B, G, R, X (little-endian word).
                    let pixel = [
                        (color & 0xff) as u8,
                        ((color >> 8) & 0xff) as u8,
                        ((color >> 16) & 0xff) as u8,
                        0,
                    ];
                    let mut packed = Vec::with_capacity(needed);
                    for _ in 0..(needed / 4) {
                        packed.extend_from_slice(&pixel);
                    }
                    let backing = state.gfx_backing();
                    let row_bytes = dst.width as usize * 4;
                    for row in 0..dst.height as usize {
                        let to = ((dst.y as usize + row) * GFX_WIDTH as usize + dst.x as usize) * 4;
                        backing[to..to + row_bytes]
                            .copy_from_slice(&packed[row * row_bytes..][..row_bytes]);
                    }
                    host::gfx_present(
                        &packed,
                        (GFX_WIDTH, GFX_HEIGHT),
                        (dst.x, dst.y, dst.width, dst.height),
                    );
                    Ok(Ok(()))
                })?;
                Ok((result,))
            })
        },
    )?;

    Ok(())
}
