//! The no_std guest runtime profile: global allocator and panic handler.
//!
//! Eo9 guest components are `no_std + alloc` so that the only imports a component ever
//! has are the `eo9:*` interfaces it explicitly asked for — no hidden WASI or libc
//! capability imports (see plan/07-guest-sdk.md, Decisions). That profile needs two
//! things every Rust program normally gets from `std`:
//!
//! * a global allocator — provided here by `dlmalloc`, the same allocator rustc's own
//!   `std` uses on wasm targets, working directly on linear memory via `memory.grow`;
//! * a panic handler — a guest panic lowers to the wasm `unreachable` instruction, so
//!   the host observes a trap and the task fails without the guest needing any
//!   capability to report it.
//!
//! Both are defined only when compiling for wasm32 so that host-side tooling
//! (rust-analyzer, doc builds) never collides with `std`'s own definitions.
//!
//! Before trapping, the panic handler reports the panic message and source location
//! through `eo9:rt/diagnostics.report-panic` — the executor's write-once diagnostics
//! sink for the trap path (see wit/rt/rt.wit and plan/07 Decision 12). Because the call
//! sits in the panic handler, every component built with this SDK carries the
//! `eo9:rt/diagnostics` import; it is part of the runtime contract (like the allocator),
//! carries no authority, and is always admitted by `only` (see
//! `eo9-component::restrict`).

#[cfg(target_arch = "wasm32")]
#[global_allocator]
static ALLOCATOR: dlmalloc::GlobalDlmalloc = dlmalloc::GlobalDlmalloc;

#[cfg(target_arch = "wasm32")]
#[panic_handler]
fn panic(info: &core::panic::PanicInfo<'_>) -> ! {
    use core::sync::atomic::{AtomicBool, Ordering};

    // Re-entrancy guard: if formatting or reporting the message itself panics (e.g. the
    // allocator fails), skip straight to the trap instead of recursing.
    static REPORTING: AtomicBool = AtomicBool::new(false);
    if !REPORTING.swap(true, Ordering::Relaxed) {
        let message = match info.location() {
            Some(location) => alloc::format!(
                "{} at {}:{}",
                info.message(),
                location.file(),
                location.line()
            ),
            None => alloc::format!("{}", info.message()),
        };
        crate::bindings::eo9::rt::diagnostics::report_panic(&message);
    }
    core::arch::wasm32::unreachable()
}
