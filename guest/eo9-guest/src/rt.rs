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

#[cfg(target_arch = "wasm32")]
#[global_allocator]
static ALLOCATOR: dlmalloc::GlobalDlmalloc = dlmalloc::GlobalDlmalloc;

#[cfg(target_arch = "wasm32")]
#[panic_handler]
fn panic(_info: &core::panic::PanicInfo<'_>) -> ! {
    core::arch::wasm32::unreachable()
}
