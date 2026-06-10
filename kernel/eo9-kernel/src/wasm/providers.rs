//! Kernel-side root providers for the eo9 OS APIs (bare-metal analogue of the usermode
//! linking in `crates/eo9-runtime/src/link.rs`).
//!
//! These are the hardware roots the spec talks about: host implementations of the
//! `eo9:text`, `eo9:time`, and `eo9:entropy` capability interfaces, registered on a
//! component [`Linker`] so a program's imports resolve directly to the machine —
//! text → the PL011 serial console, time → the generic timer plus the PL031 RTC for
//! wall-clock seconds, entropy → a splitmix64 stream seeded from the cycle counter at
//! boot (QEMU `virt` has no entropy source the kernel drives yet; virtio-rng is a later
//! milestone).
//!
//! Both the synchronous functions and the async members (`text.read-line`, `time.sleep`)
//! of each interface are registered; the async ones go through wasmtime's
//! component-model-async machinery, available on this no_std target via the patched
//! vendor/wasmtime copy (plan/12-kernel.md Decisions, kernel/vendor/README.md). `sleep`
//! is a real await against the generic timer; `read-line` reports end-of-input because
//! serial input is not wired up yet.
//!
//! The WIT-shaped host types below are structural copies of the ones in
//! `eo9-runtime::link`; that crate targets host wasmtime (std + async + WAVE) and does not
//! compile for `aarch64-unknown-none`, so the shapes are mirrored rather than reused.

use alloc::boxed::Box;
use alloc::string::String;
use alloc::vec::Vec;
use core::future::Future;
use core::pin::Pin;
use core::task::{Context, Poll};

use wasmtime::component::{
    Accessor, ComponentType, Lift, Linker, LinkerInstance, Lower, Resource, ResourceType, Val,
};
use wasmtime::{Result, StoreContextMut};

/// Boxed future returned by the `func_wrap_concurrent` closures below (the same shape as
/// the usermode runtime's alias in `eo9-runtime::link`).
type ConcurrentFuture<'a, R> = Pin<Box<dyn Future<Output = Result<R>> + Send + 'a>>;

/// Per-call ceiling on `eo9:entropy/entropy.get-bytes` requests, mirroring the usermode
/// runtime: the host materialises the returned `list<u8>` before it is copied into the
/// guest, so the request must be bounded before any allocation happens.
const MAX_ENTROPY_REQUEST_BYTES: u64 = 64 * 1024;

/// Store data for programs run against the kernel's root providers.
pub struct KernelState {
    /// Deterministic splitmix64 stream behind `eo9:entropy/entropy`.
    entropy_state: u64,
    /// How many generations of the `eo9:svc` capability this task holds (0 = not
    /// granted). The boot grants init 2, so init's console holds 1 and the console's
    /// children hold 0 — never a default grant (owner ruling B; mirrors the usermode
    /// generation count in crates/eo9/src/providers.rs).
    pub svc_generations: u32,
    /// The task's `eo9:rt/diagnostics` slot: the panic message the guest reported just
    /// before trapping (write-once, bounded; surfaced only in `trapped(reason)`).
    pub panic_message: Option<alloc::string::String>,
    /// Resource limits enforced where wasm asks the host for memory/tables (set at spawn).
    limits: KernelLimits,
    /// The session's state (fs view, buffers, exec tables) — present on the store that
    /// runs eosh and on every spawned child (children inherit the full session
    /// environment); headless demo runs carry `None`.
    #[cfg(feature = "wasm-store")]
    pub shell: Option<alloc::boxed::Box<super::shell::ShellState>>,
    /// The task's PCI handles (open devices, BARs, DMA buffers); only populated when the
    /// boot granted PCI and the program imports `eo9:pci` (see `super::pci_provider`).
    #[cfg(feature = "wasm-store")]
    pub pci: super::pci_provider::PciTables,
    /// The task's platform-device handles (claimed regions, DMA buffers); only populated
    /// when the boot granted the `platform` token and the program imports `eo9:platform`
    /// (see `super::platform_provider`).
    #[cfg(feature = "wasm-store")]
    pub platform: super::platform_provider::PlatformTables,
    /// The task's gate grant (shared-resources design): present when this spawn's
    /// `eo9:net/l4` import was routed through the kernel call gate to a config-shared
    /// owner. Dropping the store releases the grant's kernel-side state.
    #[cfg(feature = "wasm-store")]
    pub gate: Option<super::gate::GrantHandle>,
}

impl KernelState {
    /// Seed entropy from the cycle counter (documented as a stub, not a CSPRNG).
    pub fn new() -> Self {
        KernelState {
            entropy_state: crate::timer::counter() ^ 0x9e37_79b9_7f4a_7c15,
            svc_generations: 0,
            panic_message: None,
            limits: KernelLimits::default(),
            #[cfg(feature = "wasm-store")]
            shell: None,
            #[cfg(feature = "wasm-store")]
            pci: super::pci_provider::PciTables::default(),
            #[cfg(feature = "wasm-store")]
            platform: super::platform_provider::PlatformTables::default(),
            #[cfg(feature = "wasm-store")]
            gate: None,
        }
    }

    /// Record the guest's reported panic message (write-once: the first report wins;
    /// truncated to 1 KiB so diagnostics can never make the kernel hold unbounded data).
    pub fn report_panic(&mut self, message: alloc::string::String) {
        const MAX: usize = 1024;
        if self.panic_message.is_some() {
            return;
        }
        let mut message = message;
        if message.len() > MAX {
            let mut end = MAX;
            while !message.is_char_boundary(end) {
                end -= 1;
            }
            message.truncate(end);
            message.push('…');
        }
        self.panic_message = Some(message);
    }

    /// Next value of the splitmix64 stream (same generator as the usermode seeded stub).
    fn next_entropy(&mut self) -> u64 {
        self.entropy_state = self.entropy_state.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mut z = self.entropy_state;
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        z ^ (z >> 31)
    }

    /// Set the per-task linear-memory ceiling (`eo9:exec/task.spawn-limits.max-memory`),
    /// enforced through [`KernelState::limiter`].
    pub fn set_max_memory(&mut self, max_memory: u64) {
        self.limits.max_memory = Some(max_memory);
        // A memory-limited task must not grow tables without bound either (same derived
        // rule as the usermode runtime: one element per 8 bytes of allowed memory).
        self.limits.max_table_elements = Some((max_memory / 8).max(1));
    }

    /// The store's resource limiter (`Store::limiter` plumbing).
    pub fn limiter(&mut self) -> &mut KernelLimits {
        &mut self.limits
    }
}

/// Resource limits enforced at `memory.grow` / `table.grow` (the kernel-side counterpart
/// of the usermode `StoreLimits`). Unlimited unless a spawn set a ceiling.
#[derive(Default)]
pub struct KernelLimits {
    max_memory: Option<u64>,
    max_table_elements: Option<u64>,
}

impl wasmtime::ResourceLimiter for KernelLimits {
    fn memory_growing(
        &mut self,
        _current: usize,
        desired: usize,
        _maximum: Option<usize>,
    ) -> Result<bool> {
        Ok(match self.max_memory {
            Some(max) => desired as u64 <= max,
            None => true,
        })
    }

    fn table_growing(
        &mut self,
        _current: usize,
        desired: usize,
        _maximum: Option<usize>,
    ) -> Result<bool> {
        Ok(match self.max_table_elements {
            Some(max) => desired as u64 <= max,
            None => true,
        })
    }
}

// --- Host resource representations (stateless tokens; all state is kernel hardware) -----

/// Host representation of the `eo9:text/types.text-impl` resource.
struct TextCap;
/// Host representation of `eo9:time/types.time-impl`.
struct TimeCap;
/// Host representation of `eo9:entropy/types.entropy-impl`.
struct EntropyCap;

// --- WIT-shaped host types ----------------------------------------------------------------

#[derive(Clone, Copy, ComponentType, Lift, Lower)]
#[component(enum)]
#[repr(u8)]
// Constructed only by the generated `Lift` impl (values come in from the guest).
#[allow(dead_code)]
pub(super) enum WitOutputStream {
    #[component(name = "out")]
    Out,
    #[component(name = "err")]
    Err,
}

#[derive(Clone, ComponentType, Lift, Lower)]
#[component(variant)]
// Closed/Io exist to satisfy the interface type (the serial console cannot fail);
// Unsupported is answered by surfaces with no per-key input (the svc capture text).
#[allow(dead_code)]
pub(super) enum WitTextError {
    #[component(name = "closed")]
    Closed,
    #[component(name = "unsupported")]
    Unsupported,
    #[component(name = "io")]
    Io(String),
}

/// `eo9:text/text.key` — one decoded keystroke (the `read-key` payload). Variant order
/// matches the WIT declaration; mirrors `eo9-runtime::link`'s host type (this crate
/// targets `aarch64-unknown-none` and mirrors shapes rather than reusing them).
#[derive(Clone, Copy, ComponentType, Lift, Lower)]
#[component(variant)]
// Constructed host-side and lowered into the guest; `Lift` exists for shape symmetry.
#[allow(dead_code)]
pub(super) enum WitKey {
    #[component(name = "char")]
    Char(u8),
    #[component(name = "enter")]
    Enter,
    #[component(name = "backspace")]
    Backspace,
    #[component(name = "tab")]
    Tab,
    #[component(name = "up")]
    Up,
    #[component(name = "down")]
    Down,
    #[component(name = "left")]
    Left,
    #[component(name = "right")]
    Right,
    #[component(name = "ctrl")]
    Ctrl(u8),
    #[component(name = "eof")]
    Eof,
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

/// Register the kernel's root providers: the `types` resources and the text, time, and
/// entropy capability interfaces.
pub fn add_providers(linker: &mut Linker<KernelState>) -> Result<()> {
    add_types(linker)?;
    add_diagnostics(linker)?;
    add_text(linker)?;
    add_time(linker)?;
    add_entropy(linker)?;
    // The eo9:svc surface: the real registry on store builds (executor v2; the grant is
    // the store's generation count, 0 everywhere except the boot supervisor's chain),
    // the absent stub on demo-only builds, which have no shell exec tables to detach
    // through.
    #[cfg(feature = "wasm-store")]
    super::svc::add_svc(linker)?;
    #[cfg(not(feature = "wasm-store"))]
    add_svc_absent(linker)?;
    Ok(())
}

/// Host representation of `eo9:svc/detach.detach-impl` (never minted on demo builds).
#[cfg(not(feature = "wasm-store"))]
struct SvcDetachCap;
/// Host representation of `eo9:svc/services.services-impl` (never minted on demo builds).
#[cfg(not(feature = "wasm-store"))]
struct SvcServicesCap;

/// `eo9:svc` — registered as **absent** on demo-only kernel builds (no store, no shell,
/// nothing to detach). Store builds register the real registry (`super::svc`).
///
/// eosh imports both the `-optional` flavors (the honest am-I-granted signal it checks)
/// and the full interfaces (to call them when granted), so all four must be registered
/// for eosh to instantiate at all. The optionals answer `none`; the operations are
/// registered through the dynamic (`func_new`) API — their signatures come from the
/// component's own expectations — and refuse with a clear message if ever called, which
/// a well-behaved client never does after seeing `none`.
#[cfg(not(feature = "wasm-store"))]
fn add_svc_absent(linker: &mut Linker<KernelState>) -> Result<()> {
    fn refuse(
        _store: StoreContextMut<'_, KernelState>,
        _ty: wasmtime::component::types::ComponentFunc,
        _params: &[Val],
        _results: &mut [Val],
    ) -> Result<()> {
        Err(wasmtime::Error::msg(
            "eo9:svc is not available on this kernel yet: background services are              executor v2 (usermode `eo9 --svc` has them today)",
        ))
    }

    fn answer_none(
        _store: StoreContextMut<'_, KernelState>,
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

/// `eo9:rt/diagnostics`: the write-once panic-message sink for the trap path (mirrors the
/// usermode runtime). Always registered; carries no authority — the message is surfaced
/// only inside a subsequent `trapped(reason)`.
pub(super) fn add_diagnostics(linker: &mut Linker<KernelState>) -> Result<()> {
    let mut diagnostics = linker.instance("eo9:rt/diagnostics@0.1.0")?;
    diagnostics.func_wrap(
        "report-panic",
        |mut store: StoreContextMut<'_, KernelState>, (message,): (String,)| -> Result<()> {
            store.data_mut().report_panic(message);
            Ok(())
        },
    )?;
    Ok(())
}

/// The ambient time/entropy roots for the *service* environment (`super::svc`): exactly
/// the providers every console child links unconditionally, minus text — a service's
/// text is the registry's log ring, never the serial console. Registered together with
/// their types-interfaces here so svc.rs needs no access to the private capability
/// marker types.
pub(super) fn add_time_entropy(linker: &mut Linker<KernelState>) -> Result<()> {
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
    add_time(linker)?;
    add_entropy(linker)
}

/// The types-only interfaces: root-handle resources with no-op destructors.
fn add_types(linker: &mut Linker<KernelState>) -> Result<()> {
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
    Ok(())
}

/// `default: func() -> X-impl` — hand out the stateless root handle.
fn add_default_handle<C: 'static>(instance: &mut LinkerInstance<'_, KernelState>) -> Result<()> {
    instance.func_wrap(
        "default",
        |_store: StoreContextMut<'_, KernelState>, (): ()| -> Result<(Resource<C>,)> {
            Ok((Resource::new_own(0),))
        },
    )
}

/// `eo9:text/text`: the PL011 serial console. Both output streams go to the one console.
fn add_text(linker: &mut Linker<KernelState>) -> Result<()> {
    let mut text = linker.instance("eo9:text/text@0.1.0")?;
    add_default_handle::<TextCap>(&mut text)?;

    text.func_wrap(
        "write",
        |_store: StoreContextMut<'_, KernelState>,
         (_cap, _to, content): (Resource<TextCap>, WitOutputStream, String)|
         -> Result<(Result<(), WitTextError>,)> {
            super::count_host_call(super::host_call::TEXT_WRITE);
            // Key→echo meter (drive-stats): input arrived since the last guest write —
            // this write is the guest's first observable response to it. Kernel-side
            // ground truth, immune to any bench harness's own pacing.
            #[cfg(feature = "drive-stats")]
            {
                let stamp = crate::rxring::take_input_stamp();
                if stamp != 0 {
                    let elapsed = crate::timer::uptime_ns().saturating_sub(stamp);
                    use core::sync::atomic::Ordering;
                    super::drive_stats::KEY_ECHO_COUNT.fetch_add(1, Ordering::Relaxed);
                    super::drive_stats::KEY_ECHO_NS_TOTAL.fetch_add(elapsed, Ordering::Relaxed);
                    super::drive_stats::KEY_ECHO_NS_MAX.fetch_max(elapsed, Ordering::Relaxed);
                }
            }
            crate::kprint!("{content}");
            Ok((Ok(()),))
        },
    )?;

    // Read one line from the PL011 (QEMU feeds it from stdin under -nographic), echoing
    // as the user types: printable characters echo back, backspace erases, CR/LF ends
    // the line, and Ctrl-D on an empty line is end of input. Polled like time.sleep —
    // the future re-arms its waker until the line is complete.
    text.func_wrap_concurrent(
        "read-line",
        |_accessor: &Accessor<KernelState>,
         (_cap,): (Resource<TextCap>,)|
         -> ConcurrentFuture<'_, (Result<Option<String>, WitTextError>,)> {
            super::count_host_call(super::host_call::READ_LINE);
            Box::pin(async move { Ok((Ok(ReadLine::default().await),)) })
        },
    )?;

    // Read one decoded keystroke (no echo — the consumer owns the line image): the
    // per-key surface behind eosh's incremental editor. Shares the UART RX ring and
    // the escape decoding with read-line; the serial stream never ends, so the future
    // never resolves to `none`.
    text.func_wrap_concurrent(
        "read-key",
        |_accessor: &Accessor<KernelState>,
         (_cap,): (Resource<TextCap>,)|
         -> ConcurrentFuture<'_, (Result<Option<WitKey>, WitTextError>,)> {
            super::count_host_call(super::host_call::READ_KEY);
            Box::pin(async move { Ok((Ok(Some(ReadKey::default().await)),)) })
        },
    )?;

    Ok(())
}

/// `eo9:time/time`: wall-clock seconds from the PL031 RTC, sub-second and monotonic time
/// from the generic timer.
fn add_time(linker: &mut Linker<KernelState>) -> Result<()> {
    let mut time = linker.instance("eo9:time/time@0.1.0")?;
    add_default_handle::<TimeCap>(&mut time)?;

    time.func_wrap(
        "now",
        |_store: StoreContextMut<'_, KernelState>,
         (_cap,): (Resource<TimeCap>,)|
         -> Result<(WitDatetime,)> {
            Ok((WitDatetime {
                seconds: i64::from(crate::rtc::seconds()),
                // Sub-second fraction from the generic timer; not phase-locked to the RTC
                // second boundary, which is fine for a root wall clock on this machine.
                nanoseconds: crate::timer::subsecond_ns(),
            },))
        },
    )?;

    time.func_wrap(
        "monotonic-now",
        |_store: StoreContextMut<'_, KernelState>,
         (_cap,): (Resource<TimeCap>,)|
         -> Result<(WitInstant,)> {
            super::count_host_call(super::host_call::MONOTONIC);
            Ok((WitInstant {
                nanoseconds: crate::timer::uptime_ns(),
            },))
        },
    )?;

    time.func_wrap(
        "resolution",
        |_store: StoreContextMut<'_, KernelState>,
         (_cap,): (Resource<TimeCap>,)|
         -> Result<(u64,)> { Ok((crate::timer::resolution_ns(),)) },
    )?;

    // The awaited operation: returns once the generic timer says `duration-ns` of
    // monotonic time has elapsed. The future re-arms its waker on every poll, so the
    // kernel's polling executor (super::block_on) keeps driving it; with timer
    // interrupts (GIC) this becomes an interrupt-armed wake instead of a busy poll.
    time.func_wrap_concurrent(
        "sleep",
        |_accessor: &Accessor<KernelState>,
         (_cap, duration_ns): (Resource<TimeCap>, u64)|
         -> ConcurrentFuture<'_, ()> {
            super::count_host_call(super::host_call::SLEEP);
            let deadline = crate::timer::uptime_ns().saturating_add(duration_ns);
            Box::pin(async move {
                SleepUntil { deadline }.await;
                Ok(())
            })
        },
    )?;

    Ok(())
}

/// Upper bound on a single `read-line` line, so unbounded input cannot grow the line
/// buffer without limit. Bytes beyond the cap are dropped (not echoed) until the line is
/// ended; backspace still works at the boundary.
const MAX_READ_LINE_BYTES: usize = 4096;

/// Recall entries the console keeps (per boot). Eviction drops the oldest.
const READ_HISTORY_CAP: usize = 32;

/// The console's input recall ring: every line `read-line` returns (trimmed non-empty,
/// not a duplicate of the newest entry) lands here, and the arrow keys at any later
/// `read-line` recall it. One console, one input history — this is the line
/// discipline's recall (what was *typed* at this serial console), not the shell's
/// `history` builtin (which records what the session *executed*); the same split as a
/// host terminal's scrollback vs. a shell's history file.
static READ_HISTORY: super::shellexec::KLock<Vec<String>> =
    super::shellexec::KLock::new(Vec::new());

// The escape-sequence decoder (EscState/KeyEvent/KeyDecoder) lives in
// `crate::keydecoder` — a wasmtime-free crate-root module (the fbcon pattern) so its
// state machine stays host-testable under the featureless `cargo test` pass.
use crate::keydecoder::{KeyDecoder, KeyEvent};

/// Future that resolves with the next decoded keystroke from the console UART — the
/// `read-key` payload (no echo: the consuming editor owns the line image). Each call
/// is one fresh future; an escape sequence split across polls is held in the decoder
/// until its final byte arrives, so a key never decodes partially.
#[derive(Default)]
struct ReadKey {
    decoder: KeyDecoder,
}

impl Future for ReadKey {
    type Output = WitKey;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<WitKey> {
        let this = self.get_mut();
        while let Some(byte) = crate::uart::ring_get_byte() {
            if let Some(event) = this.decoder.push(byte) {
                return Poll::Ready(match event {
                    KeyEvent::Char(byte) => WitKey::Char(byte),
                    KeyEvent::Enter => WitKey::Enter,
                    KeyEvent::Backspace => WitKey::Backspace,
                    KeyEvent::Tab => WitKey::Tab,
                    KeyEvent::Up => WitKey::Up,
                    KeyEvent::Down => WitKey::Down,
                    KeyEvent::Left => WitKey::Left,
                    KeyEvent::Right => WitKey::Right,
                    KeyEvent::Ctrl(byte) => WitKey::Ctrl(byte),
                });
            }
        }
        // Same parking discipline as ReadLine below.
        super::register_idle_waker(cx.waker(), super::idle_site::READ_KEY);
        Poll::Pending
    }
}

/// Future that reads one line from the console UART, echoing input as it arrives.
///
/// Resolves to `Some(line)` on CR/LF (without the terminator) and to `None` (end of
/// input) on Ctrl-D at the start of an empty line. Backspace/DEL erase the last
/// character. Up/Down arrows recall previously entered lines (the [`READ_HISTORY`]
/// ring), and the recalled line is editable; editing it commits it as the fresh line
/// (recall position and stash reset — the bash-like per-entry edit buffer is out of
/// scope). Left/Right and other escape sequences are consumed and ignored. Other
/// control bytes (including TAB) are ignored, exactly as before the decoder was
/// shared with `read-key`.
#[derive(Default)]
struct ReadLine {
    line: String,
    /// Escape-sequence decoding, shared with [`ReadKey`].
    decoder: KeyDecoder,
    /// History browsing: `None` = typing a fresh line; `Some(i)` = showing entry `i`.
    recall: Option<usize>,
    /// The fresh line stashed when browsing began (restored by ↓ past the newest).
    stash: String,
}

impl ReadLine {
    /// Erase the visible line (every byte in it is a printable ASCII echo, one column
    /// each) and replace it with `text`, echoing the replacement.
    fn replace_line(&mut self, text: String) {
        for _ in 0..self.line.len() {
            crate::kprint!("\u{8} \u{8}");
        }
        self.line = text;
        crate::kprint!("{}", self.line);
    }

    /// ↑ (`back == true`) / ↓ through the recall ring.
    fn recall_step(&mut self, back: bool) {
        let (entry, index) = READ_HISTORY.with(|history| {
            if history.is_empty() {
                return (None, None);
            }
            match (back, self.recall) {
                // First ↑: stash the in-progress line, show the newest entry.
                (true, None) => {
                    let index = history.len() - 1;
                    (Some(history[index].clone()), Some(index))
                }
                // ↑ at the oldest entry: stay.
                (true, Some(0)) => (None, self.recall),
                (true, Some(index)) => (Some(history[index - 1].clone()), Some(index - 1)),
                // ↓ while not browsing: nothing to do.
                (false, None) => (None, None),
                // ↓ past the newest: restore the stashed fresh line.
                (false, Some(index)) if index + 1 >= history.len() => (None, None),
                (false, Some(index)) => (Some(history[index + 1].clone()), Some(index + 1)),
            }
        });
        match (back, self.recall, index) {
            // ↓ past the newest entry: leave browsing, restore the stash.
            (false, Some(_), None) => {
                let stash = core::mem::take(&mut self.stash);
                self.recall = None;
                self.replace_line(stash);
            }
            _ => {
                if let Some(text) = entry {
                    if self.recall.is_none() {
                        self.stash = core::mem::take(&mut self.line);
                    }
                    self.recall = index;
                    self.replace_line(text);
                }
            }
        }
    }

    /// Any edit while browsing commits the shown entry as the fresh line.
    fn commit_recall(&mut self) {
        if self.recall.take().is_some() {
            self.stash.clear();
        }
    }
}

/// Push a submitted line into the recall ring (trimmed non-empty, no immediate dupes).
fn push_read_history(line: &str) {
    let trimmed = line.trim();
    if trimmed.is_empty() {
        return;
    }
    READ_HISTORY.with(|history| {
        if history.last().map(String::as_str) == Some(trimmed) {
            return;
        }
        if history.len() >= READ_HISTORY_CAP {
            history.remove(0);
        }
        history.push(String::from(trimmed));
    });
}

impl Future for ReadLine {
    type Output = Option<String>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.get_mut();
        // Consume from the interrupt-filled input ring (src/uart.rs): the UART RX interrupt
        // moves bytes in and wakes the `wfi`, so this just drains what has arrived. The
        // shared decoder turns raw bytes (and escape sequences) into keystrokes; this
        // line discipline acts on them exactly as the pre-decoder code did.
        while let Some(byte) = crate::uart::ring_get_byte() {
            let Some(event) = this.decoder.push(byte) else {
                continue;
            };
            match event {
                KeyEvent::Up => this.recall_step(true),
                KeyEvent::Down => this.recall_step(false),
                KeyEvent::Enter => {
                    crate::kprint!("\n");
                    let line = core::mem::take(&mut this.line);
                    push_read_history(&line);
                    return Poll::Ready(Some(line));
                }
                // Ctrl-D on an empty line: end of input.
                KeyEvent::Ctrl(0x04) if this.line.is_empty() => return Poll::Ready(None),
                KeyEvent::Backspace => {
                    if this.line.pop().is_some() {
                        this.commit_recall();
                        crate::kprint!("\u{8} \u{8}");
                    }
                }
                // Printable ASCII only, as before (UTF-8 bytes pass through the decoder
                // as Char but the kernel's own line editor keeps its ASCII-only policy).
                KeyEvent::Char(byte @ 0x20..=0x7e) if this.line.len() < MAX_READ_LINE_BYTES => {
                    this.commit_recall();
                    let ch = char::from(byte);
                    this.line.push(ch);
                    crate::kprint!("{ch}");
                }
                // TAB, other control bytes, non-ASCII bytes, Left/Right: ignored (v1).
                _ => {}
            }
        }
        // Park instead of self-waking: registering the waker lets `block_on` re-drive this
        // future after its timer-interrupt `wfi` wake, so the core idles rather than
        // wasmtime busy-re-polling here (which would never return to `block_on`'s `wfi`).
        super::register_idle_waker(cx.waker(), super::idle_site::READ_LINE);
        Poll::Pending
    }
}

/// Future that resolves once the generic timer's uptime reaches `deadline`.
struct SleepUntil {
    deadline: u64,
}

impl Future for SleepUntil {
    type Output = ();

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<()> {
        if crate::timer::uptime_ns() >= self.deadline {
            Poll::Ready(())
        } else {
            // Park, and tell the executor to arm its `wfi` timer for *this* deadline (it
            // takes the earliest of all pending sleeps), so the wake is precise rather than a
            // fixed polling tick — and so a purely input-bound idle prompt can sleep until a
            // keystroke instead of waking on a timer it does not need.
            super::request_timer_wake(self.deadline);
            super::register_idle_waker(cx.waker(), super::idle_site::SLEEP);
            Poll::Pending
        }
    }
}

/// `eo9:entropy/entropy`: counter-seeded splitmix64 (a stub, not a CSPRNG).
fn add_entropy(linker: &mut Linker<KernelState>) -> Result<()> {
    let mut entropy = linker.instance("eo9:entropy/entropy@0.1.0")?;
    add_default_handle::<EntropyCap>(&mut entropy)?;

    entropy.func_wrap(
        "get-bytes",
        |mut store: StoreContextMut<'_, KernelState>,
         (_cap, len): (Resource<EntropyCap>, u64)|
         -> Result<(Vec<u8>,)> {
            if len > MAX_ENTROPY_REQUEST_BYTES {
                return Err(wasmtime::Error::msg(
                    "entropy get-bytes request exceeds the per-call cap",
                ));
            }
            let len = len as usize;
            let mut out = Vec::with_capacity(len);
            while out.len() < len {
                let chunk = store.data_mut().next_entropy().to_le_bytes();
                let take = usize::min(8, len - out.len());
                out.extend_from_slice(&chunk[..take]);
            }
            Ok((out,))
        },
    )?;

    entropy.func_wrap(
        "get-u64",
        |mut store: StoreContextMut<'_, KernelState>,
         (_cap,): (Resource<EntropyCap>,)|
         -> Result<(u64,)> { Ok((store.data_mut().next_entropy(),)) },
    )?;

    Ok(())
}
