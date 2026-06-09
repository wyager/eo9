//! Minimal platform (memory-mapped, non-PCI) device support: the machine's region
//! table and width-explicit register access through the syndrome-valid `mmio`
//! accessors.
//!
//! This is the hardware half of the kernel's `eo9:platform` root provider
//! (`src/wasm/platform_provider.rs`) — `src/pci.rs`'s much smaller sibling. It has no
//! device-class knowledge: what the registers *mean* is the wasm driver's business;
//! this module just gets accesses to them safely (bounds inside the region, explicit
//! width, ISV=1 instruction forms so HVF/KVM can decode the aborts).
//!
//! **The region table is the machine profile's** (the per-arch
//! `crate::arch::platform_regions`): QEMU `virt` (aarch64) exposes two benign test
//! regions so the provider's semantics — claim/busy, bounds, the per-name grant — are
//! exercisable under the battery before any board hardware exists (the M0 lane of
//! docs/board/usb-ohci-plan.md); the Orange Pi 5 Plus profile's table (the four
//! EHCI/OHCI register blocks) lands with the M1 board lane; riscv64/x86_64 tables are
//! empty until a driver needs them. Regions are *named* — the base address never
//! crosses the capability boundary, so a driver cannot aim outside its claimed window
//! by construction.

/// One region in the machine's table.
pub struct RegionDef {
    /// The name `claim` takes and `enumerate` lists (and the `platform=<name>,…` boot
    /// grant restricts to).
    pub name: &'static str,
    /// CPU (== bus, identity map) base address. Never exposed to the guest.
    pub base: usize,
    /// Window size in bytes.
    pub size: u64,
    /// Whether the region has an interrupt line the provider could route (v1 answers
    /// `unsupported` either way; the flag keeps `enumerate` honest).
    pub has_irq: bool,
    /// Device-aware quiesce, run when the region's claim is released (handle drop or
    /// task teardown) and BEFORE any of the task's DMA buffers return to the heap.
    /// A platform device has no bus-master bit to revoke, so containment is per
    /// device class: the OHCI hook drops the controller to UsbReset (no list
    /// processing, no SOF, **no HCCA writes** — OHCI 1.0a §6.1.1), closing the
    /// freed-arena DMA window that PCI closes with its bus-master clear (study 09
    /// finding 6; the M3 board's idle-reset incident is this window hit live).
    /// `None` for regions that never master the bus.
    pub quiesce: Option<fn(usize)>,
}

/// The machine's region table.
pub fn regions() -> &'static [RegionDef] {
    crate::arch::platform_regions::REGIONS
}

/// Look a region up by name.
pub fn region(name: &str) -> Option<&'static RegionDef> {
    regions().iter().find(|region| region.name == name)
}

/// Access width for platform register accesses (mirrors `pci::AccessWidth`; kept
/// separate so the two providers stay independently buildable).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AccessWidth {
    Byte,
    Word,
    Dword,
    Qword,
}

impl AccessWidth {
    pub fn bytes(self) -> u64 {
        match self {
            AccessWidth::Byte => 1,
            AccessWidth::Word => 2,
            AccessWidth::Dword => 4,
            AccessWidth::Qword => 8,
        }
    }
}

/// Whether `offset`+`width` stays inside a region of `size` bytes and is naturally
/// aligned (device registers are width-sensitive; an unaligned device access would
/// fault, so it is refused typed instead).
pub fn access_in_bounds(offset: u64, width: AccessWidth, size: u64) -> bool {
    let bytes = width.bytes();
    offset % bytes == 0
        && match offset.checked_add(bytes) {
            Some(end) => end <= size,
            None => false,
        }
}

/// Read a register inside a region. The caller (the provider) has already checked
/// bounds via [`access_in_bounds`]; this re-checks defensively and answers `None` out
/// of range.
pub fn region_read(region: &RegionDef, offset: u64, width: AccessWidth) -> Option<u64> {
    if !access_in_bounds(offset, width, region.size) {
        return None;
    }
    let address = region.base + offset as usize;
    // SAFETY: the region table only lists mapped device windows (the arch profile's
    // contract), and the bounds/alignment check above keeps the access inside one.
    let value = unsafe {
        match width {
            AccessWidth::Byte => u64::from(crate::mmio::read_u8(address)),
            AccessWidth::Word => u64::from(crate::mmio::read_u16(address)),
            AccessWidth::Dword => u64::from(crate::mmio::read_u32(address)),
            AccessWidth::Qword => crate::mmio::read_u64(address),
        }
    };
    Some(value)
}

/// Write a register inside a region; `false` out of range.
pub fn region_write(region: &RegionDef, offset: u64, width: AccessWidth, value: u64) -> bool {
    if !access_in_bounds(offset, width, region.size) {
        return false;
    }
    let address = region.base + offset as usize;
    // SAFETY: as `region_read`.
    unsafe {
        match width {
            AccessWidth::Byte => crate::mmio::write_u8(address, value as u8),
            AccessWidth::Word => crate::mmio::write_u16(address, value as u16),
            AccessWidth::Dword => crate::mmio::write_u32(address, value as u32),
            AccessWidth::Qword => crate::mmio::write_u64(address, value),
        }
    }
    true
}
