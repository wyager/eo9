//! Syndrome-valid MMIO accessors for device memory. `read_volatile`/`write_volatile` leave the register
//! class to LLVM, which may emit SIMD/FP loads (`ldr s0, [x]` was observed for the ECAM
//! dword read) — architecturally fine, but such accesses produce ISV=0 data-abort
//! syndromes that hardware virtualizers cannot decode: QEMU's HVF backend aborts
//! (`Assertion failed: (isv)`), and KVM has the same restriction. On aarch64 these
//! helpers pin every device access to a single general-purpose-register form
//! (`ldrb/ldrh/ldr`, `strb/strh/str` — ISV=1); other architectures keep plain volatile.
//! The asm blocks deliberately omit `nomem` so the accesses order like memory operations.
//!
//! The UART/GIC/RTC drivers use the u32 (and one u8-write) forms unconditionally; the
//! remaining widths serve the PCI module, which is gated behind `wasm-store` — hence the
//! `allow(dead_code)` on those forms (the feature-less CI build never compiles pci.rs).
#[cfg(target_arch = "aarch64")]
#[allow(dead_code)] // pci-only width (wasm-store builds)
pub(crate) unsafe fn read_u8(address: usize) -> u8 {
    let value: u64;
    // SAFETY: the caller guarantees `address` is a mapped, readable device register.
    unsafe {
        core::arch::asm!("ldrb {value:w}, [{address}]", address = in(reg) address, value = out(reg) value, options(nostack, preserves_flags));
    }
    value as u8
}
#[cfg(target_arch = "aarch64")]
#[allow(dead_code)] // pci-only width (wasm-store builds)
pub(crate) unsafe fn read_u16(address: usize) -> u16 {
    let value: u64;
    // SAFETY: as `read_u8`, naturally aligned.
    unsafe {
        core::arch::asm!("ldrh {value:w}, [{address}]", address = in(reg) address, value = out(reg) value, options(nostack, preserves_flags));
    }
    value as u16
}
#[cfg(target_arch = "aarch64")]
pub(crate) unsafe fn read_u32(address: usize) -> u32 {
    let value: u64;
    // SAFETY: as `read_u8`, naturally aligned.
    unsafe {
        core::arch::asm!("ldr {value:w}, [{address}]", address = in(reg) address, value = out(reg) value, options(nostack, preserves_flags));
    }
    value as u32
}
#[cfg(target_arch = "aarch64")]
#[allow(dead_code)] // pci-only width (wasm-store builds)
pub(crate) unsafe fn read_u64(address: usize) -> u64 {
    let value: u64;
    // SAFETY: as `read_u8`, naturally aligned.
    unsafe {
        core::arch::asm!("ldr {value}, [{address}]", address = in(reg) address, value = out(reg) value, options(nostack, preserves_flags));
    }
    value
}
#[cfg(target_arch = "aarch64")]
pub(crate) unsafe fn write_u8(address: usize, value: u8) {
    // SAFETY: the caller guarantees `address` is a mapped, writable device register.
    unsafe {
        core::arch::asm!("strb {value:w}, [{address}]", address = in(reg) address, value = in(reg) u64::from(value), options(nostack, preserves_flags));
    }
}
#[cfg(target_arch = "aarch64")]
#[allow(dead_code)] // pci-only width (wasm-store builds)
pub(crate) unsafe fn write_u16(address: usize, value: u16) {
    // SAFETY: as `write_u8`, naturally aligned.
    unsafe {
        core::arch::asm!("strh {value:w}, [{address}]", address = in(reg) address, value = in(reg) u64::from(value), options(nostack, preserves_flags));
    }
}
#[cfg(target_arch = "aarch64")]
pub(crate) unsafe fn write_u32(address: usize, value: u32) {
    // SAFETY: as `write_u8`, naturally aligned.
    unsafe {
        core::arch::asm!("str {value:w}, [{address}]", address = in(reg) address, value = in(reg) u64::from(value), options(nostack, preserves_flags));
    }
}
#[cfg(target_arch = "aarch64")]
#[allow(dead_code)] // pci-only width (wasm-store builds)
pub(crate) unsafe fn write_u64(address: usize, value: u64) {
    // SAFETY: as `write_u8`, naturally aligned.
    unsafe {
        core::arch::asm!("str {value}, [{address}]", address = in(reg) address, value = in(reg) value, options(nostack, preserves_flags));
    }
}

#[cfg(not(target_arch = "aarch64"))]
#[allow(dead_code)] // pci-only width (wasm-store builds)
pub(crate) unsafe fn read_u8(address: usize) -> u8 {
    // SAFETY: forwarded caller contract.
    unsafe { core::ptr::read_volatile(address as *const u8) }
}
#[cfg(not(target_arch = "aarch64"))]
#[allow(dead_code)] // pci-only width (wasm-store builds)
pub(crate) unsafe fn read_u16(address: usize) -> u16 {
    // SAFETY: forwarded caller contract.
    unsafe { core::ptr::read_volatile(address as *const u16) }
}
#[cfg(not(target_arch = "aarch64"))]
pub(crate) unsafe fn read_u32(address: usize) -> u32 {
    // SAFETY: forwarded caller contract.
    unsafe { core::ptr::read_volatile(address as *const u32) }
}
#[cfg(not(target_arch = "aarch64"))]
#[allow(dead_code)] // pci-only width (wasm-store builds)
pub(crate) unsafe fn read_u64(address: usize) -> u64 {
    // SAFETY: forwarded caller contract.
    unsafe { core::ptr::read_volatile(address as *const u64) }
}
#[cfg(not(target_arch = "aarch64"))]
pub(crate) unsafe fn write_u8(address: usize, value: u8) {
    // SAFETY: forwarded caller contract.
    unsafe { core::ptr::write_volatile(address as *mut u8, value) }
}
#[cfg(not(target_arch = "aarch64"))]
#[allow(dead_code)] // pci-only width (wasm-store builds)
pub(crate) unsafe fn write_u16(address: usize, value: u16) {
    // SAFETY: forwarded caller contract.
    unsafe { core::ptr::write_volatile(address as *mut u16, value) }
}
#[cfg(not(target_arch = "aarch64"))]
pub(crate) unsafe fn write_u32(address: usize, value: u32) {
    // SAFETY: forwarded caller contract.
    unsafe { core::ptr::write_volatile(address as *mut u32, value) }
}
#[cfg(not(target_arch = "aarch64"))]
#[allow(dead_code)] // pci-only width (wasm-store builds)
pub(crate) unsafe fn write_u64(address: usize, value: u64) {
    // SAFETY: forwarded caller contract.
    unsafe { core::ptr::write_volatile(address as *mut u64, value) }
}
