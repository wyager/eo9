//! Minimal SiFive-style PLIC driver — enough to forward the UART receive interrupt to the
//! boot hart's S-mode context so a keystroke can wake the executor's idle wait.
//!
//! QEMU's riscv64 `virt` machine exposes the PLIC at 0x0c00_0000 when started with
//! `aia=none` (which xtask pins, the riscv64 analogue of pinning GICv2 on aarch64). Each
//! hart has two contexts — context 0 is its M-mode context (owned by OpenSBI), context 1 is
//! its S-mode context — so this single-hart kernel drives context 1 only: give the source a
//! non-zero priority, enable it for the context, set the context's priority threshold to 0,
//! and claim/complete sources from the external-interrupt trap (src/arch/riscv64/traps.rs).

/// PLIC base address on the QEMU riscv64 `virt` machine.
const PLIC_BASE: usize = 0x0c00_0000;
/// The boot hart's S-mode context (hart 0: context 0 = M-mode, context 1 = S-mode).
const CONTEXT: usize = 1;
/// Per-source priority registers (4 bytes each, source-indexed; priority 0 = never deliver).
const PRIORITY_BASE: usize = 0x0000;
/// Per-context enable bitmaps (0x80 bytes per context, one bit per source).
const ENABLE_BASE: usize = 0x2000;
/// Stride between per-context enable bitmaps.
const ENABLE_STRIDE: usize = 0x80;
/// Per-context threshold/claim blocks (threshold at +0, claim/complete at +4).
const CONTEXT_BASE: usize = 0x20_0000;
/// Stride between per-context threshold/claim blocks.
const CONTEXT_STRIDE: usize = 0x1000;

/// UART0's interrupt source number on the `virt` machine.
pub(super) const UART0_SOURCE: u32 = 10;

fn mmio_read(offset: usize) -> u32 {
    // SAFETY: `PLIC_BASE + offset` is a valid PLIC register on the `virt` machine, and
    // volatile MMIO reads have no other side conditions.
    unsafe { core::ptr::read_volatile((PLIC_BASE + offset) as *const u32) }
}

fn mmio_write(offset: usize, value: u32) {
    // SAFETY: as above, for writes.
    unsafe { core::ptr::write_volatile((PLIC_BASE + offset) as *mut u32, value) }
}

/// Let every priority through to this hart's S-mode context. Call once during boot.
pub(super) fn init() {
    mmio_write(CONTEXT_BASE + CONTEXT * CONTEXT_STRIDE, 0);
}

/// Forward one interrupt source to this hart's S-mode context: give it a usable (non-zero)
/// priority and set its enable bit.
pub(super) fn enable_source(source: u32) {
    mmio_write(PRIORITY_BASE + 4 * source as usize, 1);
    let enable = ENABLE_BASE + CONTEXT * ENABLE_STRIDE + (source as usize / 32) * 4;
    mmio_write(enable, mmio_read(enable) | (1 << (source % 32)));
}

/// Claim the highest-priority pending source for this context (0 = nothing pending). Pass
/// the same value back to [`complete`] once it has been serviced.
pub(super) fn claim() -> u32 {
    mmio_read(CONTEXT_BASE + CONTEXT * CONTEXT_STRIDE + 4)
}

/// Complete an interrupt previously taken with [`claim`].
pub(super) fn complete(source: u32) {
    mmio_write(CONTEXT_BASE + CONTEXT * CONTEXT_STRIDE + 4, source);
}
