//! Minimal PCI Express host support for the QEMU machines: ECAM configuration-space
//! access, bus enumeration, BAR sizing/assignment, and bus-master control.
//!
//! This is the hardware half of the kernel's `eo9:pci` root provider
//! (`src/wasm/pci_provider.rs`). It speaks raw ECAM only — no device-class knowledge, no
//! interrupt routing yet — which is exactly the split the WIT draws: what registers *mean*
//! is the wasm driver's business, this module just gets it to them safely.
//!
//! Where the ECAM and the 32-bit BAR window live differs per machine, so the addresses come
//! from the per-architecture surface (`crate::arch::pci_map`): aarch64 `virt` with
//! `highmem=off` (ECAM `0x3f00_0000`, BARs from `0x1000_0000..0x3eff_0000`), riscv64 `virt`
//! (ECAM `0x3000_0000`, BARs from `0x4000_0000..0x8000_0000`), x86_64 `q35` (documented,
//! not wired). Both verified machines keep these regions inside the identity map, so
//! configuration space and assigned BARs are reachable without new page tables. The kernel
//! boots without firmware BAR assignment on the `virt` machines; [`assign_bar`] hands out
//! windows from the per-arch range with a bump allocator when a driver opens a BAR.
//! Reading the ECAM base from the device tree instead is a noted follow-up (plan/12
//! Decisions).
//!
//! Buses behind PCI-to-PCI bridges are not visible: assigning secondary bus numbers is a
//! firmware job this kernel does not do yet, and every QEMU `virt` device added with a
//! plain `-device …-pci` flag lands directly on bus 0.

use core::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

use crate::arch::pci_map::{
    ECAM_BASE, ECAM_BUSES, MMIO_BASE as PCIE_MMIO_BASE, MMIO_END as PCIE_MMIO_END,
};

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
pub fn intx_record(line: usize) {
    INTX_DELIVERIES[line % INTX_LINES].fetch_add(1, Ordering::Release);
}

/// Consume the pending delivery count for gpex line `line` (0 = nothing delivered).
pub fn intx_take(line: usize) -> u64 {
    INTX_DELIVERIES[line % INTX_LINES].swap(0, Ordering::AcqRel)
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

/// Bump pointer for BAR assignment (no firmware has placed anything, so the whole window
/// is ours). Single core; the atomic is for soundness, not contention.
static NEXT_BAR_ADDRESS: AtomicUsize = AtomicUsize::new(PCIE_MMIO_BASE);

/// One PCI(e) function address on segment 0 (the only segment QEMU `virt` has).
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct FunctionAddress {
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


/// Syndrome-valid MMIO accessors. `read_volatile`/`write_volatile` leave the register
/// class to LLVM, which may emit SIMD/FP loads (`ldr s0, [x]` was observed for the ECAM
/// dword read) — architecturally fine, but such accesses produce ISV=0 data-abort
/// syndromes that hardware virtualizers cannot decode: QEMU's HVF backend aborts
/// (`Assertion failed: (isv)`), and KVM has the same restriction. On aarch64 these
/// helpers pin every device access to a single general-purpose-register form
/// (`ldrb/ldrh/ldr`, `strb/strh/str` — ISV=1); other architectures keep plain volatile.
/// The asm blocks deliberately omit `nomem` so the accesses order like memory operations.
pub(crate) mod mmio {
    #[cfg(target_arch = "aarch64")]
    pub unsafe fn read_u8(address: usize) -> u8 {
        let value: u64;
        // SAFETY: the caller guarantees `address` is a mapped, readable device register.
        unsafe {
            core::arch::asm!("ldrb {value:w}, [{address}]", address = in(reg) address, value = out(reg) value, options(nostack, preserves_flags));
        }
        value as u8
    }
    #[cfg(target_arch = "aarch64")]
    pub unsafe fn read_u16(address: usize) -> u16 {
        let value: u64;
        // SAFETY: as `read_u8`, naturally aligned.
        unsafe {
            core::arch::asm!("ldrh {value:w}, [{address}]", address = in(reg) address, value = out(reg) value, options(nostack, preserves_flags));
        }
        value as u16
    }
    #[cfg(target_arch = "aarch64")]
    pub unsafe fn read_u32(address: usize) -> u32 {
        let value: u64;
        // SAFETY: as `read_u8`, naturally aligned.
        unsafe {
            core::arch::asm!("ldr {value:w}, [{address}]", address = in(reg) address, value = out(reg) value, options(nostack, preserves_flags));
        }
        value as u32
    }
    #[cfg(target_arch = "aarch64")]
    pub unsafe fn read_u64(address: usize) -> u64 {
        let value: u64;
        // SAFETY: as `read_u8`, naturally aligned.
        unsafe {
            core::arch::asm!("ldr {value}, [{address}]", address = in(reg) address, value = out(reg) value, options(nostack, preserves_flags));
        }
        value
    }
    #[cfg(target_arch = "aarch64")]
    pub unsafe fn write_u8(address: usize, value: u8) {
        // SAFETY: the caller guarantees `address` is a mapped, writable device register.
        unsafe {
            core::arch::asm!("strb {value:w}, [{address}]", address = in(reg) address, value = in(reg) u64::from(value), options(nostack, preserves_flags));
        }
    }
    #[cfg(target_arch = "aarch64")]
    pub unsafe fn write_u16(address: usize, value: u16) {
        // SAFETY: as `write_u8`, naturally aligned.
        unsafe {
            core::arch::asm!("strh {value:w}, [{address}]", address = in(reg) address, value = in(reg) u64::from(value), options(nostack, preserves_flags));
        }
    }
    #[cfg(target_arch = "aarch64")]
    pub unsafe fn write_u32(address: usize, value: u32) {
        // SAFETY: as `write_u8`, naturally aligned.
        unsafe {
            core::arch::asm!("str {value:w}, [{address}]", address = in(reg) address, value = in(reg) u64::from(value), options(nostack, preserves_flags));
        }
    }
    #[cfg(target_arch = "aarch64")]
    pub unsafe fn write_u64(address: usize, value: u64) {
        // SAFETY: as `write_u8`, naturally aligned.
        unsafe {
            core::arch::asm!("str {value}, [{address}]", address = in(reg) address, value = in(reg) value, options(nostack, preserves_flags));
        }
    }

    #[cfg(not(target_arch = "aarch64"))]
    pub unsafe fn read_u8(address: usize) -> u8 {
        // SAFETY: forwarded caller contract.
        unsafe { core::ptr::read_volatile(address as *const u8) }
    }
    #[cfg(not(target_arch = "aarch64"))]
    pub unsafe fn read_u16(address: usize) -> u16 {
        // SAFETY: forwarded caller contract.
        unsafe { core::ptr::read_volatile(address as *const u16) }
    }
    #[cfg(not(target_arch = "aarch64"))]
    pub unsafe fn read_u32(address: usize) -> u32 {
        // SAFETY: forwarded caller contract.
        unsafe { core::ptr::read_volatile(address as *const u32) }
    }
    #[cfg(not(target_arch = "aarch64"))]
    pub unsafe fn read_u64(address: usize) -> u64 {
        // SAFETY: forwarded caller contract.
        unsafe { core::ptr::read_volatile(address as *const u64) }
    }
    #[cfg(not(target_arch = "aarch64"))]
    pub unsafe fn write_u8(address: usize, value: u8) {
        // SAFETY: forwarded caller contract.
        unsafe { core::ptr::write_volatile(address as *mut u8, value) }
    }
    #[cfg(not(target_arch = "aarch64"))]
    pub unsafe fn write_u16(address: usize, value: u16) {
        // SAFETY: forwarded caller contract.
        unsafe { core::ptr::write_volatile(address as *mut u16, value) }
    }
    #[cfg(not(target_arch = "aarch64"))]
    pub unsafe fn write_u32(address: usize, value: u32) {
        // SAFETY: forwarded caller contract.
        unsafe { core::ptr::write_volatile(address as *mut u32, value) }
    }
    #[cfg(not(target_arch = "aarch64"))]
    pub unsafe fn write_u64(address: usize, value: u64) {
        // SAFETY: forwarded caller contract.
        unsafe { core::ptr::write_volatile(address as *mut u64, value) }
    }
}

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

/// The ECAM address of `offset` within `address`'s configuration space, or `None` when the
/// address or offset is outside the window this kernel maps.
fn ecam_address(address: FunctionAddress, offset: u32) -> Option<usize> {
    if address.bus >= ECAM_BUSES
        || address.device >= 32
        || address.function >= 8
        || offset >= CONFIG_SPACE_SIZE
    {
        return None;
    }
    Some(
        ECAM_BASE
            + ((address.bus as usize) << 20)
            + ((address.device as usize) << 15)
            + ((address.function as usize) << 12)
            + offset as usize,
    )
}

/// Read from configuration space. Accesses must be naturally aligned and at most a dword
/// (the ECAM region is not specified for 64-bit accesses); the value is zero-extended.
/// `None` when the address, offset, alignment, or width is invalid.
pub fn config_read(address: FunctionAddress, offset: u32, width: AccessWidth) -> Option<u64> {
    if width == AccessWidth::Qword || !offset.is_multiple_of(width.bytes()) {
        return None;
    }
    let ecam = ecam_address(address, offset)?;
    if offset + width.bytes() > CONFIG_SPACE_SIZE {
        return None;
    }
    // SAFETY: `ecam` lies inside the identity-mapped ECAM window computed above; volatile,
    // naturally aligned device reads of at most 32 bits are architecturally sound there.
    let value = unsafe {
        match width {
            AccessWidth::Byte => u64::from(mmio::read_u8(ecam)),
            AccessWidth::Word => u64::from(mmio::read_u16(ecam)),
            AccessWidth::Dword => u64::from(mmio::read_u32(ecam)),
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
    let Some(ecam) = ecam_address(address, offset) else {
        return false;
    };
    if offset + width.bytes() > CONFIG_SPACE_SIZE {
        return false;
    }
    // SAFETY: as in `config_read`; writes of at most 32 bits to the mapped ECAM window.
    unsafe {
        match width {
            AccessWidth::Byte => mmio::write_u8(ecam, value as u8),
            AccessWidth::Word => mmio::write_u16(ecam, value as u16),
            AccessWidth::Dword => mmio::write_u32(ecam, value as u32),
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

/// Walk the ECAM window and report every function that answers, in address order.
///
/// Multi-function devices are walked through all eight functions; single-function devices
/// only at function 0 (per the header-type multifunction bit).
pub fn enumerate() -> alloc::vec::Vec<FunctionInfo> {
    let mut found = alloc::vec::Vec::new();
    for bus in 0..ECAM_BUSES {
        for device in 0..32u8 {
            let function0 = FunctionAddress {
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

/// Make sure a memory BAR has a bus address, assigning one from the 32-bit PCIe MMIO
/// window if firmware (which this kernel has none of) left it at zero, and enable memory
/// decode on the function. Returns the CPU-visible base address (identity map: the same
/// number the device decodes), or `None` for I/O-space BARs, exhausted window, or invalid
/// BAR index.
pub fn assign_bar(address: FunctionAddress, bar: &BarDescription) -> Option<usize> {
    if bar.io_space || bar.size == 0 {
        return None;
    }
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
            let next = NEXT_BAR_ADDRESS.load(Ordering::Relaxed);
            base = next.checked_add(size - 1)? & !(size - 1);
            let end = base.checked_add(size)?;
            if end > PCIE_MMIO_END {
                return None;
            }
            if NEXT_BAR_ADDRESS
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
    config_write(address, 0x04, AccessWidth::Word, command)
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
