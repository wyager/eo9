//! Minimal GICv2 bring-up — enough to take timer and UART interrupts and let the core sleep.
//!
//! The kernel's executor used to busy-poll on `Poll::Pending` (a guest awaiting
//! `time.sleep`, or eosh awaiting `read-line` at the prompt), pinning a host CPU at 100%.
//! The fix is to `wfi` instead, which only wakes on an interrupt that reaches the PE — so we
//! bring up the GIC distributor + CPU interface and *forward* the EL1 physical timer PPI
//! (INTID 30) and the PL011 UART SPI (INTID 33) to this core.
//!
//! Interrupts are taken as exceptions: IRQs are unmasked (PSTATE.I = 0) once the GIC is up,
//! the EL1 IRQ vector dispatches to `exceptions::kirq`, which reads the IAR, services the
//! source (re-arms/quiets the timer, drains the UART RX FIFO into a ring), and writes EOI.
//! Synchronous exceptions stay fatal as before. The core halts in `wfi` at an idle prompt
//! and wakes promptly on a keystroke or the armed timer deadline (src/timer.rs, src/uart.rs).
//!
//! Both GIC architecture versions are supported and selected at boot by reading the
//! distributor's architectural `GICD_PIDR2.ArchRev` field (the kernel has no device-tree
//! parser yet, and PIDR2 is present at the same offset on both versions — recorded in
//! plan/12; a real board will eventually want DTB-supplied base addresses instead of the
//! QEMU `virt` constants below):
//!
//! * **GICv2** (`-M virt,gic-version=2`, today's default): MMIO CPU interface (GICC),
//!   IAR/EOIR reads and writes at `GICC_BASE`.
//! * **GICv3** (`-M virt,gic-version=3`, the `gicv3` xtask argument; what real boards like
//!   the RK3588's GIC-600 expose): system-register CPU interface (`ICC_*_EL1`), a per-PE
//!   redistributor for SGIs/PPIs (wake + group/enable/priority in the SGI frame), affinity
//!   routing for SPIs (`GICD_IROUTER<n>`), and group-1 delivery (group-0 would be signalled
//!   as FIQ, which this kernel treats as fatal — init puts every INTID in group 1).
//!
//! Either way, acknowledge/EOI hand the handler a linear [`Ack`] token (see below).

/// GIC distributor base on the QEMU `virt` machine (same for GICv2 and GICv3).
const GICD_BASE: usize = 0x0800_0000;
/// GIC CPU interface base on the QEMU `virt` machine (GICv2 only).
const GICC_BASE: usize = 0x0801_0000;
/// Redistributor base on the QEMU `virt` machine (GICv3; PE 0's RD frame — this kernel is
/// single-core, `-smp 1`).
const GICR_BASE: usize = 0x080A_0000;
/// PE 0's SGI/PPI frame (the redistributor's second 64 KiB frame).
const GICR_SGI_BASE: usize = GICR_BASE + 0x1_0000;

/// `GICD_PIDR2` — architectural peripheral ID register; bits [7:4] are the GIC
/// architecture revision (2 = GICv2, 3 = GICv3).
const GICD_PIDR2: usize = 0xFFE8;
/// `GICD_IGROUPR<n>` base — one bit per INTID; 1 = group 1.
const GICD_IGROUPR: usize = 0x080;
/// `GICD_IROUTER<n>` base — 8 bytes per INTID (valid for SPIs, INTID ≥ 32); affinity
/// routing target when `GICD_CTLR.ARE` is set. 0 routes to affinity 0.0.0.0 (this PE).
const GICD_IROUTER: usize = 0x6000;
/// `GICR_WAKER` — bit 1 ProcessorSleep (clear to wake), bit 2 ChildrenAsleep (poll clear).
const GICR_WAKER: usize = 0x0014;

/// The detected GIC architecture revision (set once by [`init`], read by the dispatchers).
/// 0 = not yet initialised (treated as v2, the historical behavior).
static VERSION: core::sync::atomic::AtomicU8 = core::sync::atomic::AtomicU8::new(0);

fn is_v3() -> bool {
    VERSION.load(core::sync::atomic::Ordering::Relaxed) == 3
}

/// Distributor control register.
const GICD_CTLR: usize = 0x000;
/// Set-enable registers (one bit per INTID; write-1-to-set).
const GICD_ISENABLER: usize = 0x100;
/// Clear-enable registers (one bit per INTID; write-1-to-clear).
const GICD_ICENABLER: usize = 0x180;

/// CPU interface control register.
const GICC_CTLR: usize = 0x000;
/// Interrupt priority mask register (only higher-priority — numerically lower — interrupts
/// are forwarded; 0xff lets everything through).
const GICC_PMR: usize = 0x004;
/// Interrupt acknowledge register (read to take the pending interrupt; returns its INTID).
const GICC_IAR: usize = 0x00c;
/// End-of-interrupt register (write the value read from IAR to complete the interrupt).
const GICC_EOIR: usize = 0x010;

fn gicc_read(offset: usize) -> u32 {
    // SAFETY: `GICC_BASE + offset` is a valid GICv2 CPU-interface register on `virt`.
    unsafe { core::ptr::read_volatile((GICC_BASE + offset) as *const u32) }
}

/// An acknowledged-but-not-yet-completed GIC interrupt: the linear token the hardware hands
/// the IRQ handler. Reading the IAR raises the running priority to the taken interrupt's
/// priority; until the matching EOI write drops it again, the CPU interface withholds every
/// further interrupt at or below that priority — at our single shared priority, an
/// acknowledged-but-never-EOI'd interrupt means **no interrupt is ever delivered again**
/// (deaf timer, deaf console, no rescue; see docs/spikes/irq-audit.md, the IAR→EOIR entry).
/// Encoding the obligation in an owned value makes the failure modes structural instead of
/// conventional, exactly like the PLIC's [`Claim`](../riscv64/plic.rs):
///
/// * EOI without acknowledge is impossible (only [`acknowledge`] constructs an `Ack`);
/// * EOI twice is a compile error (the value is moved into [`end_of_interrupt`]);
/// * discarding an unfinished ack is `#[must_use]`-linted at the call site, and a debug
///   build that drops one anyway panics at the drop site (release builds carry no `Drop`
///   impl at all, so the token compiles to the bare register value it wraps).
///
/// What the token does *not* enforce: that the source was actually serviced (level cause
/// cleared, line masked, …) before the EOI — that remains the handler's responsibility.
///
/// Representation: the stored value is the **raw IAR value plus one**, so the all-zeroes
/// bit pattern is free to encode `None` (IAR 0 is a legal INTID — SGI 0 — so the raw value
/// itself has no spare zero). The IAR payload is at most 13 bits on GICv2 (CPUID[12:10] +
/// INTID[9:0]) and 24 bits on GICv3, so the +1 can never wrap.
#[must_use = "an acknowledged GIC interrupt must be passed to end_of_interrupt(), or the running priority never drops and no further interrupt is delivered"]
pub struct Ack(core::num::NonZeroU32);

/// `Option<Ack>` must stay exactly the size of the raw register read (the `NonZeroU32`
/// niche makes `None` free), so the token costs nothing over the bare `u32` it replaced.
const _: () = assert!(core::mem::size_of::<Option<Ack>>() == core::mem::size_of::<u32>());

impl Ack {
    /// The raw acknowledge-register value (what the EOI register expects back).
    fn raw(&self) -> u32 {
        self.0.get() - 1
    }

    /// The acknowledged INTID. On GICv2 the low 10 bits of the raw value (CPUID bits are
    /// zero for everything except SGIs, which this single-core kernel never uses); on
    /// GICv3 `ICC_IAR1_EL1` already returns a bare (up to 24-bit) INTID.
    pub fn intid(&self) -> u32 {
        if is_v3() {
            self.raw() & 0xff_ffff
        } else {
            self.raw() & 0x3ff
        }
    }
}

/// Debug builds make an abandoned ack loud: dropping one (instead of completing it) is a
/// kernel bug that would otherwise surface as a machine that takes no further interrupts.
/// Release builds have no `Drop` impl, keeping the token zero-cost.
#[cfg(debug_assertions)]
impl Drop for Ack {
    fn drop(&mut self) {
        panic!(
            "GIC ack for INTID {} dropped without end_of_interrupt(); interrupt delivery is now stalled",
            self.0.get() - 1
        );
    }
}

/// Acknowledge the highest-priority pending interrupt (`None` = a spurious read, INTID
/// 1020-1023: nothing was actually pending, and no EOI must be written). The returned token
/// must be passed back to [`end_of_interrupt`] once the source is serviced.
pub fn acknowledge() -> Option<Ack> {
    let (raw, intid) = if is_v3() {
        // SAFETY: reading ICC_IAR1_EL1 acknowledges the pending group-1 interrupt; the
        // sysreg interface was enabled by `init` (ICC_SRE_EL1.SRE).
        let raw: u64;
        unsafe {
            core::arch::asm!("mrs {0}, S3_0_C12_C12_0", out(reg) raw, options(nostack))
        };
        let raw = raw as u32;
        (raw, raw & 0xff_ffff)
    } else {
        let raw = gicc_read(GICC_IAR);
        (raw, raw & 0x3ff)
    };
    if (1020..=1023).contains(&intid) {
        // Spurious / special INTIDs must not be EOI'd; no obligation exists.
        return None;
    }
    // `raw + 1` cannot wrap (≤ 13 significant bits on v2, ≤ 24 on v3) and cannot be zero,
    // so the token's niche encoding always holds.
    core::num::NonZeroU32::new(raw + 1).map(Ack)
}

/// Complete an interrupt previously taken with [`acknowledge`], consuming the token: the
/// EOI write drops the running priority so further interrupts can be delivered.
pub fn end_of_interrupt(ack: Ack) {
    let raw = ack.raw();
    // The token's obligation is discharged by this very write; in debug builds (where
    // `Ack` carries the loud-drop impl) forget it so the bomb does not fire on the way
    // out. Release builds have no `Drop`, so this is a no-op either way.
    #[cfg(debug_assertions)]
    core::mem::forget(ack);
    if is_v3() {
        // SAFETY: writing the acknowledged INTID to ICC_EOIR1_EL1 performs the priority
        // drop + deactivation (init leaves ICC_CTLR_EL1.EOImode = 0). The `eret` back to
        // the interrupted context synchronizes the write.
        unsafe {
            core::arch::asm!("msr S3_0_C12_C12_1, {0}", in(reg) raw as u64, options(nostack))
        };
    } else {
        gicc_write(GICC_EOIR, raw);
    }
}

fn gicr_read(offset: usize) -> u32 {
    // SAFETY: `GICR_BASE + offset` is a valid GICv3 redistributor register on `virt`
    // (only reached when init detected a v3 distributor).
    unsafe { core::ptr::read_volatile((GICR_BASE + offset) as *const u32) }
}

fn gicr_write(offset: usize, value: u32) {
    // SAFETY: as above, for writes (RD frame).
    unsafe { core::ptr::write_volatile((GICR_BASE + offset) as *mut u32, value) }
}

fn gicr_sgi_write(offset: usize, value: u32) {
    // SAFETY: as above, for the redistributor's SGI/PPI frame.
    unsafe { core::ptr::write_volatile((GICR_SGI_BASE + offset) as *mut u32, value) }
}

fn gicd_read(offset: usize) -> u32 {
    // SAFETY: `GICD_BASE + offset` is a valid distributor register on `virt`.
    unsafe { core::ptr::read_volatile((GICD_BASE + offset) as *const u32) }
}

fn gicd_write(offset: usize, value: u32) {
    // SAFETY: `GICD_BASE + offset` is a valid GICv2 distributor register on `virt`.
    unsafe { core::ptr::write_volatile((GICD_BASE + offset) as *mut u32, value) }
}

fn gicc_write(offset: usize, value: u32) {
    // SAFETY: `GICC_BASE + offset` is a valid GICv2 CPU-interface register on `virt`.
    unsafe { core::ptr::write_volatile((GICC_BASE + offset) as *mut u32, value) }
}

/// Enable the distributor and this core's CPU interface so forwarded interrupts can reach
/// the PE (and thus serve as `wfi` wake-ups). Call once during boot, after the MMU is on
/// (the GIC sits in the device-mapped low gigabyte). Detects the GIC architecture revision
/// from `GICD_PIDR2` and brings up whichever interface the machine exposes.
pub fn init() {
    let arch_rev = (gicd_read(GICD_PIDR2) >> 4) & 0xf;
    if arch_rev >= 3 {
        VERSION.store(3, core::sync::atomic::Ordering::Relaxed);
        init_v3();
        // The v2 path stays print-free so the default boot transcript is unchanged; the
        // v3 path is new, so announce which interface came up.
        crate::kprintln!("gic: v3 (system-register CPU interface, redistributor awake)");
    } else {
        VERSION.store(2, core::sync::atomic::Ordering::Relaxed);
        // Enable the distributor's interrupt forwarding.
        gicd_write(GICD_CTLR, 1);
        // Let interrupts of any priority through the CPU interface (lower value = higher prio).
        gicc_write(GICC_PMR, 0xff);
        // No sub-priority preemption grouping needed.
        gicc_write(0x008 /* GICC_BPR */, 0);
        // Enable the CPU interface so enabled, pending interrupts assert this PE's IRQ line.
        gicc_write(GICC_CTLR, 1);
    }
}

/// GICv3 bring-up: distributor (affinity routing + group-1 forwarding), this PE's
/// redistributor (wake + everything group 1), and the system-register CPU interface.
fn init_v3() {
    // Distributor: affinity routing on, group-1 forwarding enabled. Written before any
    // enables exist (fresh boot), where changing ARE is architecturally allowed. Bit 4 is
    // ARE (single-security-state view, which QEMU `virt` without `secure=on` presents);
    // bit 1 enables group 1.
    gicd_write(GICD_CTLR, (1 << 4) | (1 << 1));
    // Every SPI in group 1 (one bit per INTID, words 1.. cover INTID 32..; word 0 is the
    // redistributor's business under affinity routing). Group-0 interrupts would be
    // signalled as FIQ, which this kernel treats as fatal — so nothing stays in group 0.
    for word in 1..32 {
        gicd_write(GICD_IGROUPR + word * 4, 0xffff_ffff);
    }
    // Wake this PE's redistributor: clear ProcessorSleep, then wait until ChildrenAsleep
    // reads clear (the redistributor is then forwarding SGIs/PPIs to the CPU interface).
    let waker = gicr_read(GICR_WAKER);
    gicr_write(GICR_WAKER, waker & !(1 << 1));
    while gicr_read(GICR_WAKER) & (1 << 2) != 0 {
        core::hint::spin_loop();
    }
    // Every SGI/PPI in group 1 (the redistributor's IGROUPR0 in the SGI frame).
    gicr_sgi_write(GICD_IGROUPR, 0xffff_ffff);

    // CPU interface (system registers). SAFETY for the block: these are the architected
    // GICv3 CPU-interface registers; QEMU `virt` with gic-version=3 implements them, and
    // the writes follow the architected bring-up order (SRE first, then masks/enables).
    unsafe {
        // ICC_SRE_EL1.SRE = 1: use the system-register interface (QEMU has no MMIO GICC
        // on the v3 machine at all). The isb makes the routing change visible before the
        // ICC_* accesses below.
        core::arch::asm!(
            "msr S3_0_C12_C12_5, {0}",
            "isb",
            in(reg) 0b111u64, // SRE | DFB | DIB
            options(nostack)
        );
        // Priority mask: let everything through (lower value = higher priority).
        core::arch::asm!("msr S3_0_C4_C6_0, {0}", in(reg) 0xffu64, options(nostack));
        // No sub-priority preemption grouping (BPR1), and EOImode = 0 (an EOIR write does
        // the priority drop *and* the deactivation, matching the v2 flow the handler uses).
        core::arch::asm!("msr S3_0_C12_C12_3, {0}", in(reg) 0u64, options(nostack));
        core::arch::asm!("msr S3_0_C12_C12_4, {0}", in(reg) 0u64, options(nostack));
        // Enable group-1 interrupt delivery to this PE.
        core::arch::asm!(
            "msr S3_0_C12_C12_7, {0}",
            "isb",
            in(reg) 1u64,
            options(nostack)
        );
    }
}

/// Give an INTID a usable (non-zero, mid) priority. On GICv3, SGI/PPI priorities live in
/// the redistributor's SGI frame; SPI priorities stay in the distributor (same offset).
pub fn configure_intid(intid: u32) {
    let base = if is_v3() && intid < 32 {
        GICR_SGI_BASE
    } else {
        GICD_BASE
    };
    // Priority register: one byte per INTID.
    let prio_reg = 0x400 + (intid as usize);
    // SAFETY: GICD_/GICR_IPRIORITYR byte accessible at the selected base.
    unsafe { core::ptr::write_volatile((base + prio_reg) as *mut u8, 0x80) };
}

/// Enable forwarding of a single interrupt ID (e.g. INTID 30, the EL1 physical timer PPI).
/// On GICv3, SGI/PPI enables live in the redistributor, and enabled SPIs are additionally
/// routed to this PE (affinity 0.0.0.0) via `GICD_IROUTER<n>`.
pub fn enable_intid(intid: u32) {
    if is_v3() && intid < 32 {
        gicr_sgi_write(GICD_ISENABLER, 1u32 << intid);
        return;
    }
    if is_v3() {
        // Affinity routing is on (ARE=1): an SPI goes wherever its IROUTER points. 0 is
        // affinity 0.0.0.0 — this single core. 8 bytes per INTID; the low word suffices.
        gicd_write(GICD_IROUTER + (intid as usize) * 8, 0);
        gicd_write(GICD_IROUTER + (intid as usize) * 8 + 4, 0);
    }
    let register = GICD_ISENABLER + (intid as usize / 32) * 4;
    // ISENABLER is write-1-to-set: writing the single bit enables just that INTID.
    gicd_write(register, 1u32 << (intid % 32));
}

/// Disable (mask) forwarding of a single interrupt ID. Used to quiet a level-sensitive PCI
/// INTx line when it fires, until the driver has cleared the device-side cause and `wait`
/// re-arms it.
// Only the wasm-store builds route PCI interrupts; the featureless build never masks.
#[allow(dead_code)]
pub fn disable_intid(intid: u32) {
    if is_v3() && intid < 32 {
        gicr_sgi_write(GICD_ICENABLER, 1u32 << intid);
        return;
    }
    let register = GICD_ICENABLER + (intid as usize / 32) * 4;
    // ICENABLER is write-1-to-clear: writing the single bit disables just that INTID.
    gicd_write(register, 1u32 << (intid % 32));
}
