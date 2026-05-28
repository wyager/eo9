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
    Accessor, ComponentType, Lift, Linker, LinkerInstance, Lower, Resource, ResourceType,
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
    /// Resource limits enforced where wasm asks the host for memory/tables (set at spawn).
    limits: KernelLimits,
    /// The session's state (fs view, buffers, exec tables) — present on the store that
    /// runs eosh and on every spawned child (children inherit the full session
    /// environment); headless demo runs carry `None`.
    #[cfg(feature = "wasm-store")]
    pub shell: Option<alloc::boxed::Box<super::shell::ShellState>>,
}

impl KernelState {
    /// Seed entropy from the cycle counter (documented as a stub, not a CSPRNG).
    pub fn new() -> Self {
        KernelState {
            entropy_state: crate::timer::counter() ^ 0x9e37_79b9_7f4a_7c15,
            limits: KernelLimits::default(),
            #[cfg(feature = "wasm-store")]
            shell: None,
        }
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
enum WitOutputStream {
    #[component(name = "out")]
    Out,
    #[component(name = "err")]
    Err,
}

#[derive(Clone, ComponentType, Lift, Lower)]
#[component(variant)]
// The error arms exist to satisfy the interface type; the serial console cannot fail.
#[allow(dead_code)]
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

/// Register the kernel's root providers: the `types` resources and the text, time, and
/// entropy capability interfaces.
pub fn add_providers(linker: &mut Linker<KernelState>) -> Result<()> {
    add_types(linker)?;
    add_text(linker)?;
    add_time(linker)?;
    add_entropy(linker)?;
    Ok(())
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
            Box::pin(async move { Ok((Ok(ReadLine::default().await),)) })
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

/// Future that reads one line from the PL011, echoing input as it arrives.
///
/// Resolves to `Some(line)` on CR/LF (without the terminator) and to `None` (end of
/// input) on Ctrl-D at the start of an empty line. Backspace/DEL erase the last
/// character. Other control bytes are ignored.
#[derive(Default)]
struct ReadLine {
    line: String,
}

impl Future for ReadLine {
    type Output = Option<String>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.get_mut();
        // Consume from the interrupt-filled input ring (src/uart.rs): the UART RX interrupt
        // moves bytes in and wakes the `wfi`, so this just drains what has arrived.
        while let Some(byte) = crate::uart::ring_get_byte() {
            match byte {
                b'\r' | b'\n' => {
                    crate::kprint!("\n");
                    return Poll::Ready(Some(core::mem::take(&mut this.line)));
                }
                // Ctrl-D on an empty line: end of input.
                0x04 if this.line.is_empty() => return Poll::Ready(None),
                // Backspace / DEL.
                0x08 | 0x7f => {
                    if this.line.pop().is_some() {
                        crate::kprint!("\u{8} \u{8}");
                    }
                }
                0x20..=0x7e if this.line.len() < MAX_READ_LINE_BYTES => {
                    let ch = char::from(byte);
                    this.line.push(ch);
                    crate::kprint!("{ch}");
                }
                _ => {}
            }
        }
        // Park instead of self-waking: registering the waker lets `block_on` re-drive this
        // future after its timer-interrupt `wfi` wake, so the core idles rather than
        // wasmtime busy-re-polling here (which would never return to `block_on`'s `wfi`).
        super::register_idle_waker(cx.waker());
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
            super::register_idle_waker(cx.waker());
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
