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
    // Plain volatile is fine here: ISV syndrome decoding is an aarch64-hypervisor
    // concern (see crate::mmio) — riscv64 runs under TCG only.
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

/// Stop forwarding one interrupt source (clear its enable bit). Used to quiet a
/// level-sensitive PCI INTx line when it fires, until the driver has cleared the device-side
/// cause and the wasm provider's `wait` re-arms it.
// Only the wasm-store builds route PCI interrupts; the featureless build never masks.
#[allow(dead_code)]
pub(super) fn disable_source(source: u32) {
    let enable = ENABLE_BASE + CONTEXT * ENABLE_STRIDE + (source as usize / 32) * 4;
    mmio_write(enable, mmio_read(enable) & !(1 << (source % 32)));
}

/// A claimed-but-not-yet-completed PLIC interrupt: the linear token the hardware hands the
/// trap handler. While a `Claim` is live, the PLIC gateway withholds every further delivery
/// of its source; the claim **must** be passed back to [`complete`], or that source is
/// stranded permanently (a never-completed UART claim = a console that is deaf forever, with
/// no rescue — see docs/spikes/irq-audit.md, B1). Encoding the obligation in an owned value
/// makes the failure modes structural instead of conventional:
///
/// * completing without claiming is impossible (only [`claim`] constructs a `Claim`);
/// * completing twice is a compile error (the value is moved into [`complete`]);
/// * discarding an unfinished claim is `#[must_use]`-linted at the call site, and a debug
///   build that drops one anyway panics at the drop site (release builds carry no `Drop`
///   impl at all, so the token compiles to the bare `u32` it wraps).
///
/// What the token does *not* enforce: that the source was actually serviced before
/// completion — "serviced" is a hardware-state fact the type system cannot see. The order
/// claim → service → complete remains the handler's responsibility (the level-sensitive
/// gateway is forgiving there: completing an unserviced source just re-delivers it).
#[must_use = "a claimed PLIC source must be passed to complete(), or the gateway never forwards it again"]
pub(super) struct Claim(core::num::NonZeroU32);

/// `Option<Claim>` must stay exactly the size of the raw register read (the `NonZeroU32`
/// niche encodes `None` as the hardware's own "nothing pending" sentinel, 0), so the token
/// is free: same representation, same compare-with-zero codegen as the bare `u32` it
/// replaced.
const _: () = assert!(core::mem::size_of::<Option<Claim>>() == core::mem::size_of::<u32>());

impl Claim {
    /// The claimed interrupt source number.
    pub(super) fn source(&self) -> u32 {
        self.0.get()
    }
}

/// Debug builds make an abandoned claim loud: dropping one (instead of completing it) is a
/// kernel bug that would otherwise surface as a permanently deaf interrupt source. Release
/// builds have no `Drop` impl, keeping the token zero-cost.
#[cfg(debug_assertions)]
impl Drop for Claim {
    fn drop(&mut self) {
        panic!(
            "PLIC claim for source {} dropped without complete(); the source is now stranded",
            self.0.get()
        );
    }
}

/// Claim the highest-priority pending source for this context (`None` = nothing pending).
/// The returned token must be passed back to [`complete`] once the source is serviced.
pub(super) fn claim() -> Option<Claim> {
    core::num::NonZeroU32::new(mmio_read(CONTEXT_BASE + CONTEXT * CONTEXT_STRIDE + 4)).map(Claim)
}

/// Complete an interrupt previously taken with [`claim`], consuming the token.
pub(super) fn complete(claim: Claim) {
    let source = claim.source();
    // The token's obligation is discharged by this very write; in debug builds (where
    // `Claim` carries the loud-drop impl) forget it so the bomb does not fire on the way
    // out. Release builds have no `Drop`, so this is a no-op either way.
    #[cfg(debug_assertions)]
    core::mem::forget(claim);
    mmio_write(CONTEXT_BASE + CONTEXT * CONTEXT_STRIDE + 4, source);
}
