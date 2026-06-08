//! Minimal PCI Express host support: configuration-space access behind the
//! [`ConfigAccess`] shim, bus enumeration, BAR sizing/assignment, and bus-master control.
//!
//! This is the hardware half of the kernel's `eo9:pci` root provider
//! (`src/wasm/pci_provider.rs`). It has no device-class knowledge and no interrupt routing
//! of its own — which is exactly the split the WIT draws: what registers *mean* is the
//! wasm driver's business, this module just gets it to them safely.
//!
//! **How config space is reached differs per machine** (docs/board/rk3588-pcie.md):
//!
//! * QEMU `virt`/`q35` expose **ECAM**: one flat window where `bus:dev:fn:offset` maps
//!   linearly to an address. That is [`Ecam`].
//! * The RK3588's controllers are Synopsys DesignWare cores with **no ECAM**: the root
//!   port's config header lives in the controller's DBI block, and downstream devices are
//!   reached by routing an outbound iATU window to CFG0/CFG1 TLPs. That is [`DwPcie`]
//!   (board builds only).
//!
//! The shim is *address-mapping* shaped (`ConfigAccess::map` returns the CPU address an
//! access of the given offset should hit — Linux's `pci_ops.map_bus` pattern) rather than
//! the read32/write32 pair the design note sketched: deriving sub-word writes from a
//! 32-bit read-modify-write would clobber write-1-to-clear neighbours (a Word write to
//! Command at 0x04 must not write Status at 0x06 back). With `map`, every access width
//! hits the bus exactly as it always has on ECAM — the refactor is bit-for-bit on QEMU.
//!
//! Each controller is one **PCI segment**: QEMU `virt` has a single ECAM segment 0; the
//! board profile has one segment per DW controller (the two RTL8125 NIC ports). Every
//! segment owns its 32-bit MMIO window for BAR assignment ([`assign_bar`] hands out
//! windows with a per-segment bump allocator; the kernel boots without firmware BAR
//! assignment). Where the windows live comes from the per-architecture surface
//! (`crate::arch::pci_map`) on QEMU, and from the board profile's controller table
//! (`crate::arch::rk3588_pcie`) on the Orange Pi 5 Plus.
//!
//! Buses behind PCI-to-PCI bridges are not visible: assigning secondary bus numbers is a
//! firmware job this kernel does not do yet, and every QEMU `virt` device added with a
//! plain `-device …-pci` flag lands directly on bus 0. (The DW root port *is* a bridge,
//! but its secondary bus is programmed once at board bring-up — `arch::rk3588_pcie`.)

use core::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

#[cfg(feature = "board-opi5plus")]
use core::sync::atomic::{AtomicU8, AtomicU32};

/// Configuration space per PCIe function (extended config space).
const CONFIG_SPACE_SIZE: u32 = 4096;

// -----------------------------------------------------------------------------------------
// INTx delivery state
//
// The PCIe host bridge (gpex on the QEMU `virt` machines) has four legacy interrupt lines;
// every function's INTx pin lands on one of them through the standard swizzle
// (`(slot + pin - 1) mod 4`). The architecture's IRQ/trap handler masks a fired line at its
// interrupt controller and records the delivery here; the wasm `eo9:pci` provider's `wait`
// consumes the count and re-arms (unmasks) the line on its next call, after the driver has
// cleared the device-side cause. The counters are the only state shared between the two
// sides, so they live in this arch-independent module.
//
// The board profile does not wire INTx yet (arch::pci_intx::WIRED = false there): the DW
// controllers deliver all four INTx pins on ONE GIC SPI per controller, demuxed through
// PCIE_CLIENT_INTR_STATUS_LEGACY — recorded follow-up (docs/board/rk3588-pcie.md).
// -----------------------------------------------------------------------------------------

/// Number of INTx lines on the host bridge (INTA..INTD; fixed by PCI).
pub const INTX_LINES: usize = 4;

/// Deliveries per gpex line since the last `intx_take`, written by the arch IRQ handler.
static INTX_DELIVERIES: [AtomicU64; INTX_LINES] = [
    AtomicU64::new(0),
    AtomicU64::new(0),
    AtomicU64::new(0),
    AtomicU64::new(0),
];

/// Record one INTx delivery on gpex line `line`. Called from the architecture's IRQ/trap
/// handler with the line already masked at the interrupt controller (so the level-triggered
/// source cannot storm while the driver has not yet cleared its cause).
// The board profile has no caller yet: its per-controller INTx demux is the recorded
// follow-up (arch/aarch64 `pci_intx` board docs), so the swizzle/record flow idles there.
#[cfg_attr(feature = "board-opi5plus", allow(dead_code))]
pub fn intx_record(line: usize) {
    INTX_DELIVERIES[line % INTX_LINES].fetch_add(1, Ordering::Release);
}

/// Consume the pending delivery count for gpex line `line` (0 = nothing delivered).
pub fn intx_take(line: usize) -> u64 {
    INTX_DELIVERIES[line % INTX_LINES].swap(0, Ordering::AcqRel)
}

/// Sum of pending (recorded, not yet taken) deliveries across every line, without
/// consuming them. Used only by the liveness backstop detector (src/wasm/mod.rs): a
/// nonzero count observed on a backstop-rated late idle wake means a delivery's re-poll
/// edge was missed — the waiting future should have been re-polled by the wake pass the
/// delivery's own interrupt triggered.
#[allow(dead_code)] // wasm/interactive path only; not the feature-less CI build
pub fn intx_pending_total() -> u64 {
    INTX_DELIVERIES
        .iter()
        .map(|line| line.load(Ordering::Acquire))
        .sum()
}

/// The standard INTx swizzle: which host-bridge line the given function's interrupt pin
/// lands on. `pin` is the configuration-space Interrupt Pin value (1 = INTA .. 4 = INTD).
pub fn intx_line(address: FunctionAddress, pin: u8) -> usize {
    (usize::from(address.device) + usize::from(pin) - 1) % INTX_LINES
}

/// Read a function's Interrupt Pin register (configuration space offset 0x3d):
/// 0 = the function has no INTx pin, 1..=4 = INTA..INTD.
pub fn interrupt_pin(address: FunctionAddress) -> Option<u8> {
    config_read(address, 0x3d, AccessWidth::Byte).map(|pin| pin as u8)
}

/// Make sure the function's INTx output is not disabled (command register bit 10): clear
/// the INTx Disable bit so the device can assert its pin. Returns `false` on access failure.
pub fn enable_intx_output(address: FunctionAddress) -> bool {
    let Some(command) = config_read(address, 0x04, AccessWidth::Word) else {
        return false;
    };
    config_write(address, 0x04, AccessWidth::Word, command & !(1 << 10))
}

/// Board diagnostic, printed when a driver claims a function (`when = "claim"`) and again
/// when it enables bus mastering (`when = "busmaster"`): the function's command register
/// plus its segment's root-port (type-1) command, bus numbers, and memory window — so a
/// bench transcript proves on the wire whether inbound DMA can traverse the bridge (root
/// port BME/MSE set, window routing the endpoint's BARs). QEMU builds stay silent (the
/// scripted suites pin their transcripts).
#[cfg(feature = "board-opi5plus")]
pub fn claim_diagnostic(address: FunctionAddress, when: &str) {
    let command = config_read(address, 0x04, AccessWidth::Word).unwrap_or(0xffff);
    crate::kprintln!(
        "pci[{when}]: {:04x}:{:02x}:{:02x}.{} command {command:#06x} (mem{} busmaster{})",
        address.segment,
        address.bus,
        address.device,
        address.function,
        if command & 0x2 != 0 { "+" } else { "-" },
        if command & 0x4 != 0 { "+" } else { "-" },
    );
    // The segment's root port: bus 0, device 0, function 0 — a type-1 bridge on the DW
    // controllers (header type field: low 7 bits of offset 0x0e).
    let bridge = FunctionAddress {
        segment: address.segment,
        bus: 0,
        device: 0,
        function: 0,
    };
    if bridge == address {
        return; // the claimed function IS the bridge; one line is enough
    }
    let header = config_read(bridge, 0x0e, AccessWidth::Byte).unwrap_or(0xff) & 0x7f;
    if header != 0x01 {
        crate::kprintln!(
            "pci[{when}]: segment {:04x} has no type-1 root port at 00:00.0 (header {header:#04x})",
            address.segment
        );
        return;
    }
    let bridge_command = config_read(bridge, 0x04, AccessWidth::Word).unwrap_or(0xffff);
    let buses = config_read(bridge, 0x18, AccessWidth::Dword).unwrap_or(0);
    let window = config_read(bridge, 0x20, AccessWidth::Dword).unwrap_or(0);
    // Type-1 memory window fields: base = bits [15:4] << 16 (address bits [31:20]),
    // limit = bits [31:20] in the high word | 0xfffff (PCI-to-PCI bridge spec — the
    // same fields rk3588_pcie programs at bring-up).
    let mem_base = (window & 0xfff0) << 16;
    let mem_limit = (window & 0xfff0_0000) | 0xf_ffff;
    crate::kprintln!(
        "pci[{when}]: root port {:04x}:00:00.0 command {bridge_command:#06x} (mem{} \
         busmaster{}) buses {:#x}->{:#x}..{:#x} mem window {mem_base:#010x}..{mem_limit:#010x}",
        address.segment,
        if bridge_command & 0x2 != 0 { "+" } else { "-" },
        if bridge_command & 0x4 != 0 { "+" } else { "-" },
        buses & 0xff,
        (buses >> 8) & 0xff,
        (buses >> 16) & 0xff,
    );
}

/// QEMU builds: no claim diagnostic (see the board version above).
#[cfg(not(feature = "board-opi5plus"))]
pub fn claim_diagnostic(_address: FunctionAddress, _when: &str) {}

/// One PCI(e) function address. `segment` selects the controller (QEMU `virt` has a single
/// ECAM segment 0; the board profile numbers its DW controllers 0, 1, …).
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct FunctionAddress {
    pub segment: u16,
    pub bus: u8,
    pub device: u8,
    pub function: u8,
}

/// Identity of one function, read from its configuration-space header.
pub struct FunctionInfo {
    pub address: FunctionAddress,
    pub vendor_id: u16,
    pub device_id: u16,
    pub class_code: u8,
    pub subclass: u8,
    pub prog_if: u8,
    pub revision: u8,
    /// Header type field, bits 0–6 (0 endpoint, 1 PCI bridge, 2 CardBus bridge).
    pub header_type: u8,
}

/// One sized base address register of a function.
pub struct BarDescription {
    pub index: u8,
    pub io_space: bool,
    pub size: u64,
    pub prefetchable: bool,
    /// 64-bit memory BAR (occupies two BAR slots).
    pub wide: bool,
}

use crate::mmio;

/// Width of a configuration-space or BAR register access, in bytes (1, 2, 4, or 8).
/// Configuration space is at most dword-wide; qword is only valid for BAR (MMIO) access.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum AccessWidth {
    Byte,
    Word,
    Dword,
    Qword,
}

impl AccessWidth {
    pub fn bytes(self) -> u32 {
        match self {
            AccessWidth::Byte => 1,
            AccessWidth::Word => 2,
            AccessWidth::Dword => 4,
            AccessWidth::Qword => 8,
        }
    }
}

// -----------------------------------------------------------------------------------------
// The ConfigAccess shim and its two implementations
// -----------------------------------------------------------------------------------------

/// How configuration space is reached on one controller (= one PCI segment).
///
/// `map` returns the CPU address where an access to `(bus, device, function, offset)`
/// lands, or `None` when no function can exist at that address on this controller (the
/// caller treats it exactly like an absent function). Implementations may have side
/// effects (the DW shim reprograms its iATU window); the returned address is only valid
/// until the next `map` call on the same controller.
trait ConfigAccess {
    fn map(&self, bus: u8, device: u8, function: u8, offset: u32) -> Option<usize>;
    /// Buses this controller decodes (enumeration walks `0..buses`).
    fn buses(&self) -> u8;
}

/// Flat ECAM (QEMU `virt`/`q35`): `bus:dev:fn:offset` maps linearly. The pre-shim
/// behavior, verbatim — same address arithmetic, same bounds.
#[cfg(not(feature = "board-opi5plus"))]
struct Ecam {
    base: usize,
    buses: u8,
}

#[cfg(not(feature = "board-opi5plus"))]
impl ConfigAccess for Ecam {
    fn map(&self, bus: u8, device: u8, function: u8, offset: u32) -> Option<usize> {
        if bus >= self.buses {
            return None;
        }
        Some(
            self.base
                + ((bus as usize) << 20)
                + ((device as usize) << 15)
                + ((function as usize) << 12)
                + offset as usize,
        )
    }

    fn buses(&self) -> u8 {
        self.buses
    }
}

/// One Synopsys DesignWare PCIe root complex (RK3588), reached per
/// docs/board/rk3588-pcie.md:
///
/// * **bus 0** (the root port itself): its type-1 config header is the DBI block — and the
///   DW core implements *only* device 0, function 0 there. Every other device/function
///   number must map to `None` instead of an address: the DBI decodes any offset, so
///   without the guard enumeration would see 32 ghost copies of the root port.
/// * **bus 1** (secondary — the device right below the root port): an outbound iATU region
///   is routed to type-CFG0 TLPs with target `(bus << 24) | (dev << 19) | (fn << 16)`,
///   then the access goes through the controller's config aperture. Only device 0 exists
///   below a x1 root port (the link partner); other device numbers ghost on some DW
///   revisions, so they are guarded to `None` — mainline's `dw_pcie_valid_device` does the
///   same.
/// * **deeper buses**: the same iATU dance with type CFG1 (routed through bridges).
///
/// iATU register block: "unrolled" layout (DW ≥ 4.80 — the RK3588's core) at
/// `dbi + 0x30_0000`, outbound region `n` at stride `0x200` (Linux pcie-designware.h
/// `PCIE_ATU_UNROLL_BASE`). After enabling a region the enable bit is read back until it
/// sticks — the required settle check (mainline `dw_pcie_prog_outbound_atu` polls the
/// same way).
///
/// A controller only answers once the board bring-up (`arch::rk3588_pcie`) has marked it
/// usable: `DISABLED` (default) maps nothing, `ROOT_ONLY` (link training failed) maps just
/// the root port so the bench still proves config access works, `FULL` maps everything.
#[cfg(feature = "board-opi5plus")]
pub(crate) struct DwPcie {
    /// DBI block (root port config + port logic + unrolled iATU).
    dbi: usize,
    /// CPU-side base of the outbound config aperture (the DT node's "config" reg).
    cfg_base: usize,
    /// Aperture size (≥ 4 KiB; the shim only ever accesses the first 4 KiB).
    cfg_size: usize,
    /// Bring-up state: one of the `dw_state` values.
    state: AtomicU8,
    /// Last iATU target+type routed through outbound region 0 (cache: skip the reprogram
    /// when the same function is accessed repeatedly). `u32::MAX` = nothing programmed.
    last_target: AtomicU32,
}

/// [`DwPcie`] bring-up states, reported by `arch::rk3588_pcie::init`.
#[cfg(feature = "board-opi5plus")]
pub(crate) mod dw_state {
    /// Bring-up has not run or failed before the DBI was usable: map nothing.
    pub(crate) const DISABLED: u8 = 0;
    /// DBI alive but link training failed: only the root port is visible.
    pub(crate) const ROOT_ONLY: u8 = 1;
    /// Link up: root port + downstream devices visible.
    pub(crate) const FULL: u8 = 2;
}

/// Unrolled iATU outbound region 0 registers, relative to the DBI base (Linux
/// pcie-designware.h: `PCIE_ATU_UNROLL_BASE(0, 0)` = `0x30_0000`, region stride `0x200`;
/// field layouts per `dw_pcie_prog_outbound_atu`).
#[cfg(feature = "board-opi5plus")]
mod dw_regs {
    pub(super) const ATU_OUTBOUND0: usize = 0x30_0000;
    /// TYPE in bits [4:0].
    pub(super) const ATU_REGION_CTRL_1: usize = ATU_OUTBOUND0;
    /// Bit 31 = region enable.
    pub(super) const ATU_REGION_CTRL_2: usize = ATU_OUTBOUND0 + 0x04;
    pub(super) const ATU_LOWER_BASE: usize = ATU_OUTBOUND0 + 0x08;
    pub(super) const ATU_UPPER_BASE: usize = ATU_OUTBOUND0 + 0x0c;
    pub(super) const ATU_LIMIT: usize = ATU_OUTBOUND0 + 0x10;
    pub(super) const ATU_LOWER_TARGET: usize = ATU_OUTBOUND0 + 0x14;
    pub(super) const ATU_UPPER_TARGET: usize = ATU_OUTBOUND0 + 0x18;
    pub(super) const ATU_ENABLE: u32 = 1 << 31;
    /// Config TLP types (PCIe spec; Linux PCIE_ATU_TYPE_CFG0/CFG1).
    pub(super) const TYPE_CFG0: u32 = 0x4;
    pub(super) const TYPE_CFG1: u32 = 0x5;
}

#[cfg(feature = "board-opi5plus")]
impl DwPcie {
    pub(crate) const fn new(dbi: usize, cfg_base: usize, cfg_size: usize) -> DwPcie {
        DwPcie {
            dbi,
            cfg_base,
            cfg_size,
            state: AtomicU8::new(dw_state::DISABLED),
            last_target: AtomicU32::new(u32::MAX),
        }
    }

    /// The DBI base (the board bring-up programs port-logic registers through it).
    pub(crate) fn dbi(&self) -> usize {
        self.dbi
    }

    /// Board bring-up reports the controller's outcome (a `dw_state` value).
    pub(crate) fn set_state(&self, state: u8) {
        // Force the first post-bring-up access to route the iATU from scratch.
        self.last_target.store(u32::MAX, Ordering::Relaxed);
        self.state.store(state, Ordering::Release);
    }

    /// Route outbound iATU region 0 at the config TLP target, settle-checked. Returns
    /// `false` if the enable never reads back (controller clock/reset trouble) — the
    /// access is then reported as an absent function rather than touching the aperture.
    fn atu_route(&self, target: u32, tlp_type: u32) -> bool {
        // The cache key packs the TLP type into the target's always-zero low bits
        // (targets are `bus<<24 | dev<<19 | fn<<16`).
        let key = target | tlp_type;
        if self.last_target.load(Ordering::Acquire) == key {
            return true;
        }
        // SAFETY: the iATU block lies inside the identity-mapped DBI window of a
        // controller the board bring-up enabled; volatile dword accesses there are sound.
        unsafe {
            mmio::write_u32(self.dbi + dw_regs::ATU_LOWER_BASE, self.cfg_base as u32);
            mmio::write_u32(
                self.dbi + dw_regs::ATU_UPPER_BASE,
                ((self.cfg_base as u64) >> 32) as u32,
            );
            // The region covers the whole aperture even though only the first 4 KiB is
            // ever accessed — one less special case at the DT-given 1 MiB size.
            mmio::write_u32(
                self.dbi + dw_regs::ATU_LIMIT,
                (self.cfg_base + self.cfg_size - 1) as u32,
            );
            mmio::write_u32(self.dbi + dw_regs::ATU_LOWER_TARGET, target);
            mmio::write_u32(self.dbi + dw_regs::ATU_UPPER_TARGET, 0);
            mmio::write_u32(self.dbi + dw_regs::ATU_REGION_CTRL_1, tlp_type);
            mmio::write_u32(self.dbi + dw_regs::ATU_REGION_CTRL_2, dw_regs::ATU_ENABLE);
        }
        // Mainline waits up to 5 × 9 ms for the enable to stick (LINK_WAIT_MAX_IATU_RETRIES
        // × LINK_WAIT_IATU); in practice it lands on the first read. Bounded either way.
        for _ in 0..5 {
            // SAFETY: as above.
            let ctrl2 = unsafe { mmio::read_u32(self.dbi + dw_regs::ATU_REGION_CTRL_2) };
            if ctrl2 & dw_regs::ATU_ENABLE != 0 {
                self.last_target.store(key, Ordering::Release);
                return true;
            }
            crate::arch::timer::delay_us(9_000);
        }
        self.last_target.store(u32::MAX, Ordering::Release);
        false
    }
}

#[cfg(feature = "board-opi5plus")]
impl ConfigAccess for DwPcie {
    fn map(&self, bus: u8, device: u8, function: u8, offset: u32) -> Option<usize> {
        let state = self.state.load(Ordering::Acquire);
        if state == dw_state::DISABLED || bus >= self.buses() {
            return None;
        }
        if bus == 0 {
            // The root port: DBI direct, device 0 function 0 only (ghost-device guard).
            if device != 0 || function != 0 {
                return None;
            }
            return Some(self.dbi + offset as usize);
        }
        if state != dw_state::FULL {
            return None; // link never came up: nothing exists below the root port
        }
        if bus == 1 && device != 0 {
            // Only the link partner exists right below a x1 root port (ghost guard).
            return None;
        }
        let tlp_type = if bus == 1 {
            dw_regs::TYPE_CFG0
        } else {
            dw_regs::TYPE_CFG1
        };
        let target =
            (u32::from(bus) << 24) | (u32::from(device) << 19) | (u32::from(function) << 16);
        if !self.atu_route(target, tlp_type) {
            return None;
        }
        Some(self.cfg_base + offset as usize)
    }

    fn buses(&self) -> u8 {
        // Root port (bus 0) + its secondary (bus 1). The DT gives each controller a
        // 16-bus range, but reaching bus 2+ requires a PCI-to-PCI bridge below the root
        // port with a programmed secondary bus — firmware work this kernel does not do
        // (module docs) — so deeper bus numbers can never hold a reachable device.
        // Walking them anyway would fire hundreds of UR-completing CFG1 probes per
        // enumeration (and UR handling on untested silicon is an SError surface worth
        // zero). The CFG1 arm in `map` stays for the bridge-programming follow-up.
        2
    }
}

// -----------------------------------------------------------------------------------------
// The controller (segment) table
// -----------------------------------------------------------------------------------------

/// One PCI segment: a config-access implementation plus its 32-bit MMIO window for BAR
/// assignment.
struct Controller {
    config: Access,
    mmio_end: usize,
    /// Bump pointer for BAR assignment (no firmware has placed anything, so the whole
    /// window is ours). Single core; the atomic is for soundness, not contention.
    next_bar: AtomicUsize,
}

enum Access {
    #[cfg(not(feature = "board-opi5plus"))]
    Ecam(Ecam),
    #[cfg(feature = "board-opi5plus")]
    Dw(&'static DwPcie),
}

impl Controller {
    fn access(&self) -> &dyn ConfigAccess {
        match &self.config {
            #[cfg(not(feature = "board-opi5plus"))]
            Access::Ecam(ecam) => ecam,
            #[cfg(feature = "board-opi5plus")]
            Access::Dw(dw) => *dw,
        }
    }
}

/// QEMU `virt`/`q35`: the one ECAM host bridge, segment 0, with the per-architecture
/// window constants — exactly the pre-shim behavior.
#[cfg(not(feature = "board-opi5plus"))]
static CONTROLLERS: [Controller; 1] = {
    use crate::arch::pci_map::{ECAM_BASE, ECAM_BUSES, MMIO_BASE, MMIO_END};
    [Controller {
        config: Access::Ecam(Ecam {
            base: ECAM_BASE,
            buses: ECAM_BUSES,
        }),
        mmio_end: MMIO_END,
        next_bar: AtomicUsize::new(MMIO_BASE),
    }]
};

/// Orange Pi 5 Plus: the two DW controllers serving the onboard RTL8125 NICs
/// (segment 0 = pcie2x1l1, the right port; segment 1 = pcie2x1l2, the left port —
/// rk3588-orangepi-5-plus.dts). Disabled until `arch::rk3588_pcie::init` brings them up;
/// each segment's BAR window is its controller's 32-bit non-prefetchable range from
/// rk3588-base.dtsi (the `ranges` 0x02000000 entries).
#[cfg(feature = "board-opi5plus")]
static CONTROLLERS: [Controller; 2] = {
    use crate::arch::rk3588_pcie::{PCIE2X1L1, PCIE2X1L1_MEM, PCIE2X1L2, PCIE2X1L2_MEM};
    [
        Controller {
            config: Access::Dw(&PCIE2X1L1),
            mmio_end: PCIE2X1L1_MEM.1,
            next_bar: AtomicUsize::new(PCIE2X1L1_MEM.0),
        },
        Controller {
            config: Access::Dw(&PCIE2X1L2),
            mmio_end: PCIE2X1L2_MEM.1,
            next_bar: AtomicUsize::new(PCIE2X1L2_MEM.0),
        },
    ]
};

/// The controller serving `segment`, or `None` for segments this machine does not have.
fn controller(segment: u16) -> Option<&'static Controller> {
    CONTROLLERS.get(usize::from(segment))
}

/// The CPU address of `offset` within `address`'s configuration space, or `None` when the
/// address or offset is outside what this machine's controllers decode.
fn config_address(address: FunctionAddress, offset: u32) -> Option<usize> {
    if address.device >= 32 || address.function >= 8 || offset >= CONFIG_SPACE_SIZE {
        return None;
    }
    controller(address.segment)?
        .access()
        .map(address.bus, address.device, address.function, offset)
}

/// Read from configuration space. Accesses must be naturally aligned and at most a dword
/// (config space is not specified for 64-bit accesses); the value is zero-extended.
/// `None` when the address, offset, alignment, or width is invalid.
pub fn config_read(address: FunctionAddress, offset: u32, width: AccessWidth) -> Option<u64> {
    if width == AccessWidth::Qword || !offset.is_multiple_of(width.bytes()) {
        return None;
    }
    // Checked: `offset` comes straight from the wasm provider, so the bare add could
    // overflow (a debug-build panic). Out of bounds either way → no bus access (and no
    // DW iATU reprogram, which `config_address` would otherwise side-effect).
    if offset
        .checked_add(width.bytes())
        .is_none_or(|end| end > CONFIG_SPACE_SIZE)
    {
        return None;
    }
    let target = config_address(address, offset)?;
    // SAFETY: `target` lies inside an identity-mapped config window (ECAM, DBI, or a
    // routed DW aperture) computed above; volatile, naturally aligned device reads of at
    // most 32 bits are architecturally sound there.
    let value = unsafe {
        match width {
            AccessWidth::Byte => u64::from(mmio::read_u8(target)),
            AccessWidth::Word => u64::from(mmio::read_u16(target)),
            AccessWidth::Dword => u64::from(mmio::read_u32(target)),
            AccessWidth::Qword => unreachable!(),
        }
    };
    Some(value)
}

/// Write to configuration space (same alignment/width rules as [`config_read`]); the value
/// is truncated to the access width. Returns `false` when the access is invalid.
pub fn config_write(address: FunctionAddress, offset: u32, width: AccessWidth, value: u64) -> bool {
    if width == AccessWidth::Qword || !offset.is_multiple_of(width.bytes()) {
        return false;
    }
    // Checked add: see config_read.
    if offset
        .checked_add(width.bytes())
        .is_none_or(|end| end > CONFIG_SPACE_SIZE)
    {
        return false;
    }
    let Some(target) = config_address(address, offset) else {
        return false;
    };
    // SAFETY: as in `config_read`; writes of at most 32 bits to a mapped config window.
    unsafe {
        match width {
            AccessWidth::Byte => mmio::write_u8(target, value as u8),
            AccessWidth::Word => mmio::write_u16(target, value as u16),
            AccessWidth::Dword => mmio::write_u32(target, value as u32),
            AccessWidth::Qword => unreachable!(),
        }
    }
    true
}

/// Read one function's identity, or `None` if no function answers at that address
/// (an absent function reads its vendor ID as `0xffff`).
fn probe_function(address: FunctionAddress) -> Option<FunctionInfo> {
    let vendor_id = config_read(address, 0x00, AccessWidth::Word)? as u16;
    if vendor_id == 0xffff {
        return None;
    }
    let device_id = config_read(address, 0x02, AccessWidth::Word)? as u16;
    let revision_and_class = config_read(address, 0x08, AccessWidth::Dword)? as u32;
    let header_type = (config_read(address, 0x0e, AccessWidth::Byte)? as u8) & 0x7f;
    Some(FunctionInfo {
        address,
        vendor_id,
        device_id,
        class_code: (revision_and_class >> 24) as u8,
        subclass: (revision_and_class >> 16) as u8,
        prog_if: (revision_and_class >> 8) as u8,
        revision: revision_and_class as u8,
        header_type,
    })
}

/// Walk every controller's buses and report every function that answers, in
/// segment-then-address order.
///
/// Multi-function devices are walked through all eight functions; single-function devices
/// only at function 0 (per the header-type multifunction bit).
pub fn enumerate() -> alloc::vec::Vec<FunctionInfo> {
    let mut found = alloc::vec::Vec::new();
    for (index, ctrl) in CONTROLLERS.iter().enumerate() {
        let segment = index as u16;
        for bus in 0..ctrl.access().buses() {
            for device in 0..32u8 {
                let function0 = FunctionAddress {
                    segment,
                    bus,
                    device,
                    function: 0,
                };
                if let Some(info) = probe_function(function0) {
                    let multifunction =
                        config_read(function0, 0x0e, AccessWidth::Byte).unwrap_or(0) & 0x80 != 0;
                    found.push(info);
                    if multifunction {
                        for function in 1..8u8 {
                            let address = FunctionAddress {
                                segment,
                                bus,
                                device,
                                function,
                            };
                            if let Some(info) = probe_function(address) {
                                found.push(info);
                            }
                        }
                    }
                }
            }
        }
    }
    found
}

/// Whether a function answers at this address at all.
pub fn function_present(address: FunctionAddress) -> bool {
    matches!(config_read(address, 0x00, AccessWidth::Word), Some(vendor) if vendor != 0xffff)
}

/// Describe (size) the base address registers of a type-0 (endpoint) function.
///
/// Sizing uses the standard write-all-ones probe and restores the original BAR value
/// afterwards. The kernel boots without firmware so the "original" value is normally 0;
/// decode is not enabled at this point, so the transient all-ones value never reaches the
/// bus. Bridges (header type ≠ 0) report no BARs here — their two BARs are rarely useful
/// and their layout differs.
pub fn describe_bars(address: FunctionAddress) -> alloc::vec::Vec<BarDescription> {
    let mut bars = alloc::vec::Vec::new();
    let header_type = config_read(address, 0x0e, AccessWidth::Byte).unwrap_or(0) & 0x7f;
    if header_type != 0 {
        return bars;
    }
    let mut index = 0u8;
    while index < 6 {
        let offset = 0x10 + u32::from(index) * 4;
        let Some(original_low) = config_read(address, offset, AccessWidth::Dword) else {
            break;
        };
        let io_space = original_low & 0x1 != 0;
        let wide = !io_space && (original_low >> 1) & 0x3 == 0x2;
        let prefetchable = !io_space && original_low & 0x8 != 0;

        config_write(address, offset, AccessWidth::Dword, 0xffff_ffff);
        let mask_low = config_read(address, offset, AccessWidth::Dword).unwrap_or(0) as u32;
        config_write(address, offset, AccessWidth::Dword, original_low);

        // The size mask: address bits read back as written (1), hard-wired-zero bits give
        // the region size. A 64-bit BAR's mask spans two slots; a 32-bit one is padded with
        // ones above bit 31 so the arithmetic below stays in u64.
        let mask: u64 = if wide {
            let high_offset = offset + 4;
            let original_high = config_read(address, high_offset, AccessWidth::Dword).unwrap_or(0);
            config_write(address, high_offset, AccessWidth::Dword, 0xffff_ffff);
            let mask_high = config_read(address, high_offset, AccessWidth::Dword).unwrap_or(0);
            config_write(address, high_offset, AccessWidth::Dword, original_high);
            (mask_high << 32) | u64::from(mask_low & !0xf)
        } else if io_space {
            0xffff_ffff_0000_0000 | u64::from(mask_low & !0x3)
        } else {
            0xffff_ffff_0000_0000 | u64::from(mask_low & !0xf)
        };

        // An unimplemented BAR reads back all zeros from the probe (mask 0 → size 0).
        let size = if mask_low == 0 {
            0
        } else {
            (!mask).wrapping_add(1)
        };
        if size != 0 {
            bars.push(BarDescription {
                index,
                io_space,
                size,
                prefetchable,
                wide,
            });
        }
        index += if wide { 2 } else { 1 };
    }
    bars
}

/// Make sure a memory BAR has a bus address, assigning one from the function's segment's
/// 32-bit PCIe MMIO window if firmware (which this kernel has none of) left it at zero,
/// and enable memory decode on the function. Returns the CPU-visible base address
/// (identity map: the same number the device decodes), or `None` for I/O-space BARs,
/// exhausted window, or invalid BAR index.
pub fn assign_bar(address: FunctionAddress, bar: &BarDescription) -> Option<usize> {
    if bar.io_space || bar.size == 0 {
        return None;
    }
    let ctrl = controller(address.segment)?;
    let offset = 0x10 + u32::from(bar.index) * 4;
    let low = config_read(address, offset, AccessWidth::Dword)? as u32;
    let high = if bar.wide {
        config_read(address, offset + 4, AccessWidth::Dword)? as u32
    } else {
        0
    };
    let current = (u64::from(high) << 32) | u64::from(low & !0xf);
    let base = if current != 0 {
        usize::try_from(current).ok()?
    } else {
        // Bump-allocate a naturally aligned window. BAR sizes are powers of two.
        let size = usize::try_from(bar.size).ok()?;
        let mut base;
        loop {
            let next = ctrl.next_bar.load(Ordering::Relaxed);
            base = next.checked_add(size - 1)? & !(size - 1);
            let end = base.checked_add(size)?;
            if end > ctrl.mmio_end {
                return None;
            }
            if ctrl
                .next_bar
                .compare_exchange(next, end, Ordering::Relaxed, Ordering::Relaxed)
                .is_ok()
            {
                break;
            }
        }
        config_write(
            address,
            offset,
            AccessWidth::Dword,
            base as u64 & 0xffff_ffff,
        );
        if bar.wide {
            config_write(address, offset + 4, AccessWidth::Dword, (base as u64) >> 32);
        }
        base
    };
    // Enable memory-space decode (command register bit 1) so the device answers at the BAR.
    let command = config_read(address, 0x04, AccessWidth::Word)?;
    config_write(address, 0x04, AccessWidth::Word, command | 0x2);
    Some(base)
}

/// Enable or disable bus mastering (command register bit 2) — the device's licence to DMA.
pub fn set_bus_master(address: FunctionAddress, enable: bool) -> bool {
    let Some(command) = config_read(address, 0x04, AccessWidth::Word) else {
        return false;
    };
    let command = if enable {
        command | 0x4
    } else {
        command & !0x4
    };
    if !config_write(address, 0x04, AccessWidth::Word, command) {
        return false;
    }
    // Board: a function behind a root port can only DMA if the BRIDGE also forwards —
    // an RC port without Bus Master Enable drops inbound memory requests, and without
    // Memory Space Enable it does not decode at all. rk3588_pcie sets the port command
    // at bring-up; this re-asserts it at the moment a driver is granted DMA, so nothing
    // that ran in between can have silently revoked forwarding.
    #[cfg(feature = "board-opi5plus")]
    if enable && address.bus != 0 {
        let bridge = FunctionAddress {
            segment: address.segment,
            bus: 0,
            device: 0,
            function: 0,
        };
        if let Some(bridge_command) = config_read(bridge, 0x04, AccessWidth::Word)
            && bridge_command & 0x6 != 0x6
        {
            crate::kprintln!(
                "pci: root port {:04x}:00:00.0 command was {bridge_command:#06x} — \
                 re-enabling mem decode + bus-master forwarding",
                address.segment
            );
            config_write(bridge, 0x04, AccessWidth::Word, bridge_command | 0x6);
        }
    }
    true
}

/// Read a register inside an assigned BAR window. `base`/`size` come from [`assign_bar`] /
/// [`describe_bars`]; the caller (the wasm provider) bounds-checks `offset + width` against
/// `size` before calling. Accesses must be naturally aligned.
pub fn bar_read(base: usize, offset: u64, width: AccessWidth) -> Option<u64> {
    if !offset.is_multiple_of(u64::from(width.bytes())) {
        return None;
    }
    let target = base.checked_add(usize::try_from(offset).ok()?)?;
    // SAFETY: the caller established that `[target, target + width)` lies inside a BAR
    // window assigned from the identity-mapped PCIe MMIO range; volatile, naturally
    // aligned device accesses there are sound.
    let value = unsafe {
        match width {
            AccessWidth::Byte => u64::from(mmio::read_u8(target)),
            AccessWidth::Word => u64::from(mmio::read_u16(target)),
            AccessWidth::Dword => u64::from(mmio::read_u32(target)),
            AccessWidth::Qword => mmio::read_u64(target),
        }
    };
    Some(value)
}

/// Write a register inside an assigned BAR window (same contract as [`bar_read`]).
pub fn bar_write(base: usize, offset: u64, width: AccessWidth, value: u64) -> bool {
    if !offset.is_multiple_of(u64::from(width.bytes())) {
        return false;
    }
    let Some(target) = usize::try_from(offset)
        .ok()
        .and_then(|o| base.checked_add(o))
    else {
        return false;
    };
    // SAFETY: as in `bar_read`.
    unsafe {
        match width {
            AccessWidth::Byte => mmio::write_u8(target, value as u8),
            AccessWidth::Word => mmio::write_u16(target, value as u16),
            AccessWidth::Dword => mmio::write_u32(target, value as u32),
            AccessWidth::Qword => mmio::write_u64(target, value),
        }
    }
    true
}
