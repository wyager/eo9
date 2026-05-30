//! Kernel-side root provider for `eo9:pci` — the capability wasm drivers hold.
//!
//! This is the bare-metal root the WIT talks about (wit/pci/pci.wit, plan/02 D14): the
//! kernel implements enumeration, configuration-space access, BAR register windows,
//! bus-master control, and DMA buffers directly against the machine (`crate::pci`, raw
//! ECAM on QEMU `virt`), and a wasm component that imports `eo9:pci/pci` drives the device
//! itself — the kernel carries no device-class knowledge.
//!
//! **Containment.** A PCI device that can bus-master is, absent an IOMMU (QEMU `virt` has
//! none configured), effectively full-memory authority, so this provider is **never linked
//! by default**: the operator grants it for a boot by putting the bare `pci` token on the
//! kernel command line (`cargo xtask qemu aarch64 pci …`). Without the token, a program
//! importing `eo9:pci` is refused at instantiation with the capability story
//! (`shellexec::missing_capability`); with it, the loader rule still applies — only
//! programs that actually import `eo9:pci/pci` link it. Finer-grained per-spawn grants
//! (the `pci.filtered` attenuator composed in front of a driver) ride on top of this root
//! exactly as the WIT intends.
//!
//! **Interrupt delivery (INTx).** `enable-interrupts` routes a function's legacy interrupt
//! pin through the platform interrupt controller (the gpex lines: GIC SPIs 35-38 on aarch64
//! `virt`, PLIC sources 0x20-0x23 on riscv64 `virt`; the standard `(slot + pin - 1) mod 4`
//! swizzle picks the line), and `wait` blocks until a delivery arrives. The protocol with
//! the IRQ handler: a fired line is masked at the controller (it is level-sensitive — the
//! device keeps asserting until the driver clears the cause) and counted; `wait` re-arms
//! (unmasks) the line when called, consumes the count when it fires, and returns with the
//! line masked again. **`wait` blocks host-side** — on the same masked-`wfi` + timer
//! primitives the executor's idle path uses, so the CPU sleeps rather than spinning, and a
//! console Ctrl-C or a bound expiry aborts it — instead of suspending the calling task.
//! That is deliberate: every existing driver chain (`fs.eofs $ disk.virtio`,
//! `net.l4.over-l2 $ net.virtio`) drives its imports eagerly (single-poll, plan/09 D16/D18),
//! so a `wait` that returned `Pending` would be unusable by exactly the drivers it exists
//! for. A properly task-suspending wait (parking on the drive loop the way `time.sleep`
//! does) becomes possible when the async disk/net bridge lands; the controller-side
//! machinery here stays the same either way. MSI/MSI-X remain `unsupported`.
//!
//! Not implemented yet (drivers get `unsupported`, never a wrong answer): MSI/MSI-X,
//! function-level `reset`, and I/O-space BARs (the arm64 `virt` PIO window is not mapped).
//! DMA buffers are plain kernel-heap allocations: with the identity map the CPU address
//! *is* the bus address, and QEMU keeps DMA cache-coherent; real hardware will need
//! non-cacheable mappings or explicit maintenance here.

use alloc::boxed::Box;
use alloc::string::String;
use alloc::vec::Vec;
use core::future::Future;
use core::pin::Pin;
use core::sync::atomic::{AtomicBool, Ordering};

use wasmtime::component::{Accessor, ComponentType, Lift, Linker, Lower, Resource, ResourceType};
use wasmtime::{Result, StoreContextMut};

use super::providers::KernelState;
use crate::pci;

/// Boxed future shape for `func_wrap_concurrent` closures (same alias as the other kernel
/// providers).
type ConcurrentFuture<'a, R> = Pin<Box<dyn Future<Output = Result<R>> + Send + 'a>>;

/// Per-allocation ceiling for `alloc-dma`, so one call cannot take a huge bite out of the
/// kernel heap (the buffer is host memory, not guest linear memory).
const MAX_DMA_ALLOC_BYTES: u64 = 4 * 1024 * 1024;
/// Ceiling on live DMA buffers per task.
const MAX_DMA_BUFFERS: usize = 64;
/// DMA buffers are aligned to a page: enough for every virtio structure and friendly to a
/// future IOMMU mapping path.
const DMA_ALIGN: usize = 4096;

/// How long a single `wait` call blocks for a delivery before giving up with a typed `io`
/// error. Generous for any healthy device (QEMU completes block requests in well under a
/// millisecond); a hung device falls back to the driver's polled path, which has its own
/// bound. Bounded so a dead device cannot wedge the machine.
const INTX_WAIT_BOUND_NS: u64 = 2_000_000_000;
/// Timer backstop between `wfi` halts inside a blocked `wait`. The PCI interrupt itself (and
/// any keystroke) wakes the halt immediately — this only bounds how stale the Ctrl-C / bound
/// checks can get if a wake is missed.
const INTX_WAIT_SLICE_NS: u64 = 10_000_000;

// -----------------------------------------------------------------------------------------
// Boot-time grant
// -----------------------------------------------------------------------------------------

/// Whether this boot granted the PCI capability (the bare `pci` kernel command-line token).
static PCI_GRANTED: AtomicBool = AtomicBool::new(false);

/// Record the boot-time grant decision (called once from `runner::boot`).
pub fn set_granted(granted: bool) {
    PCI_GRANTED.store(granted, Ordering::Relaxed);
}

/// Whether linkers built for this boot should include the `eo9:pci` root provider.
pub fn granted() -> bool {
    PCI_GRANTED.load(Ordering::Relaxed)
}

// -----------------------------------------------------------------------------------------
// Host resource representations and per-store state
// -----------------------------------------------------------------------------------------

/// Host representation of `eo9:pci/types.pci-impl` (stateless token; the hardware is the
/// state).
struct PciCap;
/// Host representation of `eo9:pci/pci.device`; the rep indexes the open-device table.
struct DeviceRes;
/// Host representation of `eo9:pci/pci.bar`; the rep indexes the open-BAR table.
struct BarRes;
/// Host representation of `eo9:pci/pci.interrupt`; the rep indexes the interrupt-vector
/// table.
struct InterruptRes;
/// Host representation of `eo9:pci/pci.dma-buffer`; the rep indexes the DMA-buffer table.
struct DmaRes;

/// One claimed PCI function.
struct OpenDevice {
    address: pci::FunctionAddress,
}

/// One opened (assigned and decode-enabled) BAR window.
struct OpenBar {
    base: usize,
    size: u64,
}

/// One allocated INTx vector: the gpex line the device's interrupt pin swizzles onto.
/// The line is masked at the controller except while a `wait` is blocked on it.
struct OpenInterrupt {
    line: usize,
}

/// One DMA-able allocation. The page-aligned window `[offset, offset + len)` inside
/// `storage` is what the guest sees; with the identity map its CPU address is also the
/// bus address the device DMAs to.
struct DmaBuffer {
    storage: Vec<u8>,
    offset: usize,
    len: usize,
}

impl DmaBuffer {
    fn allocate(len: usize) -> DmaBuffer {
        let storage = alloc::vec![0u8; len + DMA_ALIGN];
        let misalignment = storage.as_ptr() as usize % DMA_ALIGN;
        let offset = if misalignment == 0 {
            0
        } else {
            DMA_ALIGN - misalignment
        };
        DmaBuffer {
            storage,
            offset,
            len,
        }
    }

    fn bus_address(&self) -> u64 {
        (self.storage.as_ptr() as usize + self.offset) as u64
    }

    fn bytes(&self) -> &[u8] {
        &self.storage[self.offset..self.offset + self.len]
    }

    fn bytes_mut(&mut self) -> &mut [u8] {
        &mut self.storage[self.offset..self.offset + self.len]
    }
}

/// The task's PCI state: open devices, opened BARs, and live DMA buffers (rep → slot).
/// Lives on [`KernelState`], so each task tracks (and bounds) its own handles; exclusive
/// claiming across *tasks* is not enforced yet (single-driver-per-device machine-wide is a
/// follow-up alongside interrupt delivery).
#[derive(Default)]
pub struct PciTables {
    devices: Vec<Option<OpenDevice>>,
    bars: Vec<Option<OpenBar>>,
    interrupts: Vec<Option<OpenInterrupt>>,
    buffers: Vec<Option<DmaBuffer>>,
}

impl PciTables {
    fn insert<T>(slots: &mut Vec<Option<T>>, value: T) -> u32 {
        match slots.iter().position(Option::is_none) {
            Some(index) => {
                slots[index] = Some(value);
                index as u32
            }
            None => {
                slots.push(Some(value));
                (slots.len() - 1) as u32
            }
        }
    }

    fn device(&self, rep: u32) -> Result<&OpenDevice, WitPciError> {
        self.devices
            .get(rep as usize)
            .and_then(Option::as_ref)
            .ok_or(WitPciError::NotFound)
    }

    fn bar(&self, rep: u32) -> Result<&OpenBar, WitPciError> {
        self.bars
            .get(rep as usize)
            .and_then(Option::as_ref)
            .ok_or(WitPciError::NotFound)
    }

    fn interrupt(&self, rep: u32) -> Result<&OpenInterrupt, WitPciError> {
        self.interrupts
            .get(rep as usize)
            .and_then(Option::as_ref)
            .ok_or(WitPciError::NotFound)
    }

    fn buffer(&self, rep: u32) -> Result<&DmaBuffer, wasmtime::Error> {
        self.buffers
            .get(rep as usize)
            .and_then(Option::as_ref)
            .ok_or_else(|| wasmtime::Error::msg(alloc::format!("unknown dma-buffer handle {rep}")))
    }

    fn buffer_mut(&mut self, rep: u32) -> Result<&mut DmaBuffer, wasmtime::Error> {
        self.buffers
            .get_mut(rep as usize)
            .and_then(Option::as_mut)
            .ok_or_else(|| wasmtime::Error::msg(alloc::format!("unknown dma-buffer handle {rep}")))
    }

    fn close_device(&mut self, rep: u32) {
        if let Some(slot) = self.devices.get_mut(rep as usize) {
            *slot = None;
        }
    }

    fn close_bar(&mut self, rep: u32) {
        if let Some(slot) = self.bars.get_mut(rep as usize) {
            *slot = None;
        }
    }

    fn close_interrupt(&mut self, rep: u32) {
        if let Some(slot) = self.interrupts.get_mut(rep as usize)
            && let Some(vector) = slot.take()
        {
            // Dropping every handle disables delivery: leave the line masked.
            crate::arch::pci_intx::mask(vector.line);
        }
    }

    fn close_buffer(&mut self, rep: u32) {
        if let Some(slot) = self.buffers.get_mut(rep as usize) {
            *slot = None;
        }
    }
}

impl KernelState {
    fn pci_tables(&mut self) -> &mut PciTables {
        &mut self.pci
    }
}

// -----------------------------------------------------------------------------------------
// WIT-shaped host types (eo9:pci)
// -----------------------------------------------------------------------------------------

#[derive(Clone, Copy, ComponentType, Lift, Lower)]
#[component(record)]
struct WitDeviceAddress {
    segment: u16,
    bus: u8,
    device: u8,
    function: u8,
}

#[derive(Clone, Copy, ComponentType, Lift, Lower)]
#[component(enum)]
#[repr(u8)]
#[allow(dead_code)]
enum WitHeaderType {
    #[component(name = "endpoint")]
    Endpoint,
    #[component(name = "pci-bridge")]
    PciBridge,
    #[component(name = "cardbus-bridge")]
    CardbusBridge,
}

#[derive(Clone, Copy, ComponentType, Lift, Lower)]
#[component(record)]
struct WitDeviceInfo {
    address: WitDeviceAddress,
    #[component(name = "vendor-id")]
    vendor_id: u16,
    #[component(name = "device-id")]
    device_id: u16,
    #[component(name = "class-code")]
    class_code: u8,
    subclass: u8,
    #[component(name = "prog-if")]
    prog_if: u8,
    revision: u8,
    header: WitHeaderType,
}

#[derive(Clone, Copy, ComponentType, Lift, Lower)]
#[component(enum)]
#[repr(u8)]
#[allow(dead_code)]
enum WitBarKind {
    #[component(name = "memory")]
    Memory,
    #[component(name = "io")]
    Io,
}

#[derive(Clone, Copy, ComponentType, Lift, Lower)]
#[component(record)]
struct WitBarInfo {
    index: u8,
    kind: WitBarKind,
    size: u64,
    prefetchable: bool,
    wide: bool,
}

#[derive(Clone, Copy, ComponentType, Lift, Lower)]
#[component(enum)]
#[repr(u8)]
#[allow(dead_code)]
enum WitInterruptKind {
    #[component(name = "intx")]
    Intx,
    #[component(name = "msi")]
    Msi,
    #[component(name = "msi-x")]
    MsiX,
}

#[derive(Clone, Copy, ComponentType, Lift, Lower)]
#[component(enum)]
#[repr(u8)]
#[allow(dead_code)]
enum WitAccessWidth {
    #[component(name = "byte")]
    Byte,
    #[component(name = "word")]
    Word,
    #[component(name = "dword")]
    Dword,
    #[component(name = "qword")]
    Qword,
}

impl From<WitAccessWidth> for pci::AccessWidth {
    fn from(width: WitAccessWidth) -> pci::AccessWidth {
        match width {
            WitAccessWidth::Byte => pci::AccessWidth::Byte,
            WitAccessWidth::Word => pci::AccessWidth::Word,
            WitAccessWidth::Dword => pci::AccessWidth::Dword,
            WitAccessWidth::Qword => pci::AccessWidth::Qword,
        }
    }
}

#[derive(Clone, ComponentType, Lift, Lower)]
#[component(variant)]
#[allow(dead_code)]
enum WitPciError {
    #[component(name = "denied")]
    Denied,
    #[component(name = "not-found")]
    NotFound,
    #[component(name = "busy")]
    Busy,
    #[component(name = "out-of-range")]
    OutOfRange,
    #[component(name = "unsupported")]
    Unsupported,
    #[component(name = "exhausted")]
    Exhausted,
    #[component(name = "io")]
    Io(String),
}

/// The kernel only drives PCI segment 0 (the one PCIe host bridge QEMU `virt` has).
fn function_address(address: WitDeviceAddress) -> Result<pci::FunctionAddress, WitPciError> {
    if address.segment != 0 {
        return Err(WitPciError::NotFound);
    }
    Ok(pci::FunctionAddress {
        bus: address.bus,
        device: address.device,
        function: address.function,
    })
}

fn device_info(info: &pci::FunctionInfo) -> WitDeviceInfo {
    WitDeviceInfo {
        address: WitDeviceAddress {
            segment: 0,
            bus: info.address.bus,
            device: info.address.device,
            function: info.address.function,
        },
        vendor_id: info.vendor_id,
        device_id: info.device_id,
        class_code: info.class_code,
        subclass: info.subclass,
        prog_if: info.prog_if,
        revision: info.revision,
        header: match info.header_type {
            1 => WitHeaderType::PciBridge,
            2 => WitHeaderType::CardbusBridge,
            _ => WitHeaderType::Endpoint,
        },
    }
}

// -----------------------------------------------------------------------------------------
// Linker registration
// -----------------------------------------------------------------------------------------

/// Register the `eo9:pci` root provider (the `types` resource plus the full `pci`
/// interface) on a linker. Only call this when the boot granted PCI ([`granted`]); the
/// capability must never be linked by default.
pub fn add_pci(linker: &mut Linker<KernelState>) -> Result<()> {
    linker.instance("eo9:pci/types@0.1.0")?.resource(
        "pci-impl",
        ResourceType::host::<PciCap>(),
        |_, _| Ok(()),
    )?;

    let mut interface = linker.instance("eo9:pci/pci@0.1.0")?;

    interface.resource(
        "device",
        ResourceType::host::<DeviceRes>(),
        |mut store: StoreContextMut<'_, KernelState>, rep| {
            store.data_mut().pci_tables().close_device(rep);
            Ok(())
        },
    )?;
    interface.resource(
        "bar",
        ResourceType::host::<BarRes>(),
        |mut store: StoreContextMut<'_, KernelState>, rep| {
            store.data_mut().pci_tables().close_bar(rep);
            Ok(())
        },
    )?;
    interface.resource(
        "interrupt",
        ResourceType::host::<InterruptRes>(),
        |mut store: StoreContextMut<'_, KernelState>, rep| {
            store.data_mut().pci_tables().close_interrupt(rep);
            Ok(())
        },
    )?;
    interface.resource(
        "dma-buffer",
        ResourceType::host::<DmaRes>(),
        |mut store: StoreContextMut<'_, KernelState>, rep| {
            store.data_mut().pci_tables().close_buffer(rep);
            Ok(())
        },
    )?;

    interface.func_wrap(
        "default",
        |_store: StoreContextMut<'_, KernelState>, (): ()| -> Result<(Resource<PciCap>,)> {
            Ok((Resource::new_own(0),))
        },
    )?;

    // --- enumeration and device access ---------------------------------------------------

    interface.func_wrap_concurrent(
        "enumerate",
        |_accessor: &Accessor<KernelState>,
         (_cap,): (Resource<PciCap>,)|
         -> ConcurrentFuture<'_, (Result<Vec<WitDeviceInfo>, WitPciError>,)> {
            Box::pin(async move {
                let devices: Vec<WitDeviceInfo> =
                    pci::enumerate().iter().map(device_info).collect();
                Ok((Ok(devices),))
            })
        },
    )?;

    interface.func_wrap_concurrent(
        "open",
        |accessor: &Accessor<KernelState>,
         (_cap, address): (Resource<PciCap>, WitDeviceAddress)|
         -> ConcurrentFuture<'_, (Result<Resource<DeviceRes>, WitPciError>,)> {
            Box::pin(async move {
                let opened = function_address(address).and_then(|address| {
                    if pci::function_present(address) {
                        Ok(address)
                    } else {
                        Err(WitPciError::NotFound)
                    }
                });
                let result = match opened {
                    Err(error) => Err(error),
                    Ok(address) => accessor.with(|mut access| {
                        let tables = access.data_mut().pci_tables();
                        let already_claimed = tables
                            .devices
                            .iter()
                            .flatten()
                            .any(|device| device.address == address);
                        if already_claimed {
                            Err(WitPciError::Busy)
                        } else {
                            let rep =
                                PciTables::insert(&mut tables.devices, OpenDevice { address });
                            Ok(Resource::new_own(rep))
                        }
                    }),
                };
                Ok((result,))
            })
        },
    )?;

    // --- configuration space --------------------------------------------------------------

    interface.func_wrap_concurrent(
        "config-read",
        |accessor: &Accessor<KernelState>,
         (device, offset, width): (Resource<DeviceRes>, u32, WitAccessWidth)|
         -> ConcurrentFuture<'_, (Result<u64, WitPciError>,)> {
            Box::pin(async move {
                let address = accessor.with(|mut access| {
                    access
                        .data_mut()
                        .pci_tables()
                        .device(device.rep())
                        .map(|device| device.address)
                });
                let result = address.and_then(|address| {
                    if matches!(width, WitAccessWidth::Qword) {
                        return Err(WitPciError::Unsupported);
                    }
                    pci::config_read(address, offset, width.into()).ok_or(WitPciError::OutOfRange)
                });
                Ok((result,))
            })
        },
    )?;

    interface.func_wrap_concurrent(
        "config-write",
        |accessor: &Accessor<KernelState>,
         (device, offset, width, value): (Resource<DeviceRes>, u32, WitAccessWidth, u64)|
         -> ConcurrentFuture<'_, (Result<(), WitPciError>,)> {
            Box::pin(async move {
                let address = accessor.with(|mut access| {
                    access
                        .data_mut()
                        .pci_tables()
                        .device(device.rep())
                        .map(|device| device.address)
                });
                let result = address.and_then(|address| {
                    if matches!(width, WitAccessWidth::Qword) {
                        return Err(WitPciError::Unsupported);
                    }
                    if pci::config_write(address, offset, width.into(), value) {
                        Ok(())
                    } else {
                        Err(WitPciError::OutOfRange)
                    }
                });
                Ok((result,))
            })
        },
    )?;

    // --- BARs -------------------------------------------------------------------------------

    interface.func_wrap_concurrent(
        "bars",
        |accessor: &Accessor<KernelState>,
         (device,): (Resource<DeviceRes>,)|
         -> ConcurrentFuture<'_, (Result<Vec<WitBarInfo>, WitPciError>,)> {
            Box::pin(async move {
                let address = accessor.with(|mut access| {
                    access
                        .data_mut()
                        .pci_tables()
                        .device(device.rep())
                        .map(|device| device.address)
                });
                let result = address.map(|address| {
                    pci::describe_bars(address)
                        .iter()
                        .map(|bar| WitBarInfo {
                            index: bar.index,
                            kind: if bar.io_space {
                                WitBarKind::Io
                            } else {
                                WitBarKind::Memory
                            },
                            size: bar.size,
                            prefetchable: bar.prefetchable,
                            wide: bar.wide,
                        })
                        .collect()
                });
                Ok((result,))
            })
        },
    )?;

    interface.func_wrap_concurrent(
        "open-bar",
        |accessor: &Accessor<KernelState>,
         (device, index): (Resource<DeviceRes>, u8)|
         -> ConcurrentFuture<'_, (Result<Resource<BarRes>, WitPciError>,)> {
            Box::pin(async move {
                let address = accessor.with(|mut access| {
                    access
                        .data_mut()
                        .pci_tables()
                        .device(device.rep())
                        .map(|device| device.address)
                });
                let opened = address.and_then(|address| {
                    let bars = pci::describe_bars(address);
                    let bar = bars
                        .iter()
                        .find(|bar| bar.index == index)
                        .ok_or(WitPciError::NotFound)?;
                    if bar.io_space {
                        // The arm64 `virt` PIO window is not mapped; I/O-space BARs are a
                        // follow-up if a driver ever needs one.
                        return Err(WitPciError::Unsupported);
                    }
                    let base = pci::assign_bar(address, bar).ok_or(WitPciError::Exhausted)?;
                    Ok(OpenBar {
                        base,
                        size: bar.size,
                    })
                });
                let result = match opened {
                    Err(error) => Err(error),
                    Ok(bar) => accessor.with(|mut access| {
                        let rep = PciTables::insert(&mut access.data_mut().pci_tables().bars, bar);
                        Ok(Resource::new_own(rep))
                    }),
                };
                Ok((result,))
            })
        },
    )?;

    interface.func_wrap_concurrent(
        "bar-read",
        |accessor: &Accessor<KernelState>,
         (bar, offset, width): (Resource<BarRes>, u64, WitAccessWidth)|
         -> ConcurrentFuture<'_, (Result<u64, WitPciError>,)> {
            Box::pin(async move {
                let window = accessor.with(|mut access| {
                    access
                        .data_mut()
                        .pci_tables()
                        .bar(bar.rep())
                        .map(|bar| (bar.base, bar.size))
                });
                let result = window.and_then(|(base, size)| {
                    bar_access_in_bounds(offset, width, size)?;
                    pci::bar_read(base, offset, width.into()).ok_or(WitPciError::OutOfRange)
                });
                Ok((result,))
            })
        },
    )?;

    interface.func_wrap_concurrent(
        "bar-write",
        |accessor: &Accessor<KernelState>,
         (bar, offset, width, value): (Resource<BarRes>, u64, WitAccessWidth, u64)|
         -> ConcurrentFuture<'_, (Result<(), WitPciError>,)> {
            Box::pin(async move {
                let window = accessor.with(|mut access| {
                    access
                        .data_mut()
                        .pci_tables()
                        .bar(bar.rep())
                        .map(|bar| (bar.base, bar.size))
                });
                let result = window.and_then(|(base, size)| {
                    bar_access_in_bounds(offset, width, size)?;
                    if pci::bar_write(base, offset, width.into(), value) {
                        Ok(())
                    } else {
                        Err(WitPciError::OutOfRange)
                    }
                });
                Ok((result,))
            })
        },
    )?;

    // --- device control ---------------------------------------------------------------------

    interface.func_wrap_concurrent(
        "set-bus-master",
        |accessor: &Accessor<KernelState>,
         (device, enable): (Resource<DeviceRes>, bool)|
         -> ConcurrentFuture<'_, (Result<(), WitPciError>,)> {
            Box::pin(async move {
                let address = accessor.with(|mut access| {
                    access
                        .data_mut()
                        .pci_tables()
                        .device(device.rep())
                        .map(|device| device.address)
                });
                let result = address.and_then(|address| {
                    if pci::set_bus_master(address, enable) {
                        Ok(())
                    } else {
                        Err(WitPciError::Io(String::from(
                            "command register write failed",
                        )))
                    }
                });
                Ok((result,))
            })
        },
    )?;

    interface.func_wrap_concurrent(
        "reset",
        |_accessor: &Accessor<KernelState>,
         (_device,): (Resource<DeviceRes>,)|
         -> ConcurrentFuture<'_, (Result<(), WitPciError>,)> {
            // Function-level reset needs a capability-list walk; not implemented yet.
            Box::pin(async move { Ok((Err(WitPciError::Unsupported),)) })
        },
    )?;

    // --- interrupts (INTx through the platform interrupt controller) -------------------------

    interface.func_wrap_concurrent(
        "enable-interrupts",
        |accessor: &Accessor<KernelState>,
         (device, kind, count): (Resource<DeviceRes>, WitInterruptKind, u32)|
         -> ConcurrentFuture<'_, (Result<Vec<Resource<InterruptRes>>, WitPciError>,)> {
            Box::pin(async move {
                let result = accessor.with(|mut access| {
                    let tables = access.data_mut().pci_tables();
                    let address = tables.device(device.rep())?.address;
                    // Only legacy INTx is routed; MSI/MSI-X need a message-address allocator
                    // (GICv2m / the PLIC has no MSI path without AIA) and stay unsupported.
                    if !matches!(kind, WitInterruptKind::Intx) || !crate::arch::pci_intx::WIRED {
                        return Err(WitPciError::Unsupported);
                    }
                    // "Allocate up to `count`": intx allocates exactly one; zero means none.
                    if count == 0 {
                        return Ok(Vec::new());
                    }
                    // The function must actually have an interrupt pin (0 = none), and its
                    // INTx output must not be disabled at the command register.
                    let pin = pci::interrupt_pin(address).ok_or(WitPciError::OutOfRange)?;
                    if pin == 0 || pin > 4 {
                        return Err(WitPciError::Unsupported);
                    }
                    if !pci::enable_intx_output(address) {
                        return Err(WitPciError::Io(String::from(
                            "command register write failed",
                        )));
                    }
                    // The standard swizzle picks which host-bridge line this pin lands on.
                    // The line starts (and stays) masked at the controller; `wait` unmasks it
                    // for exactly the time it is waiting.
                    let line = pci::intx_line(address, pin);
                    let rep = PciTables::insert(&mut tables.interrupts, OpenInterrupt { line });
                    Ok(alloc::vec![Resource::new_own(rep)])
                });
                Ok((result,))
            })
        },
    )?;

    interface.func_wrap_concurrent(
        "wait",
        |accessor: &Accessor<KernelState>,
         (interrupt,): (Resource<InterruptRes>,)|
         -> ConcurrentFuture<'_, (Result<u64, WitPciError>,)> {
            Box::pin(async move {
                let line = accessor.with(|mut access| {
                    access
                        .data_mut()
                        .pci_tables()
                        .interrupt(interrupt.rep())
                        .map(|vector| vector.line)
                });
                let result = line.and_then(wait_for_intx);
                Ok((result,))
            })
        },
    )?;

    // --- DMA ----------------------------------------------------------------------------------

    interface.func_wrap_concurrent(
        "alloc-dma",
        |accessor: &Accessor<KernelState>,
         (device, len): (Resource<DeviceRes>, u64)|
         -> ConcurrentFuture<'_, (Result<Resource<DmaRes>, WitPciError>,)> {
            Box::pin(async move {
                let result = accessor.with(|mut access| {
                    let tables = access.data_mut().pci_tables();
                    tables.device(device.rep())?;
                    if len == 0 || len > MAX_DMA_ALLOC_BYTES {
                        return Err(WitPciError::Exhausted);
                    }
                    if tables.buffers.iter().flatten().count() >= MAX_DMA_BUFFERS {
                        return Err(WitPciError::Exhausted);
                    }
                    let buffer = DmaBuffer::allocate(len as usize);
                    let rep = PciTables::insert(&mut tables.buffers, buffer);
                    Ok(Resource::new_own(rep))
                });
                Ok((result,))
            })
        },
    )?;

    interface.func_wrap(
        "dma-address",
        |mut store: StoreContextMut<'_, KernelState>,
         (buffer,): (Resource<DmaRes>,)|
         -> Result<(u64,)> {
            Ok((store
                .data_mut()
                .pci_tables()
                .buffer(buffer.rep())?
                .bus_address(),))
        },
    )?;

    interface.func_wrap(
        "dma-len",
        |mut store: StoreContextMut<'_, KernelState>,
         (buffer,): (Resource<DmaRes>,)|
         -> Result<(u64,)> {
            Ok((store.data_mut().pci_tables().buffer(buffer.rep())?.len as u64,))
        },
    )?;

    interface.func_wrap(
        "dma-read",
        |mut store: StoreContextMut<'_, KernelState>,
         (buffer, offset, len): (Resource<DmaRes>, u64, u64)|
         -> Result<(Vec<u8>,)> {
            let buffer = store.data_mut().pci_tables().buffer(buffer.rep())?;
            let (start, end) = dma_byte_range(buffer.len, offset, len)?;
            Ok((buffer.bytes()[start..end].to_vec(),))
        },
    )?;

    interface.func_wrap(
        "dma-write",
        |mut store: StoreContextMut<'_, KernelState>,
         (buffer, offset, bytes): (Resource<DmaRes>, u64, Vec<u8>)|
         -> Result<()> {
            let buffer = store.data_mut().pci_tables().buffer_mut(buffer.rep())?;
            let (start, end) = dma_byte_range(buffer.len, offset, bytes.len() as u64)?;
            buffer.bytes_mut()[start..end].copy_from_slice(&bytes);
            Ok(())
        },
    )?;

    Ok(())
}

/// Block until the gpex line `line` delivers an interrupt, the console interrupts the wait
/// (Ctrl-C), or the wait bound expires. Returns the number of deliveries consumed (>= 1).
///
/// Protocol with the IRQ handler (the arch `kirq`/`ktrap`): the line is unmasked here, so a
/// pending or future level-triggered assert is forwarded; when it fires, the handler masks
/// the line (the device keeps asserting until the driver clears its cause) and bumps the
/// delivery counter; this loop consumes the counter and returns with the line still masked.
/// The next `wait` — after the driver has cleared the cause (e.g. read virtio's ISR
/// register) — unmasks again.
///
/// The block happens *host-side*, halting the core in the same masked-`wfi` + timer step the
/// executor's idle path uses (`timer::wait_for_interrupt`), so the CPU sleeps instead of
/// spinning host calls. The PCI interrupt, a keystroke (UART RX), and the timer slice all
/// wake the halt; each wake re-checks the counter, the console, and the bound. See the
/// module docs for why this blocks rather than suspending the calling task.
fn wait_for_intx(line: usize) -> Result<u64, WitPciError> {
    crate::arch::pci_intx::unmask(line);
    let deadline = crate::timer::uptime_ns().saturating_add(INTX_WAIT_BOUND_NS);
    loop {
        let deliveries = crate::pci::intx_take(line);
        if deliveries > 0 {
            // One diagnostic line per boot, the first time a wait is actually served by an
            // interrupt delivery: evidence in every metal transcript that completions are
            // interrupt-driven (not satisfied by the driver's pre-wait ring check).
            static FIRST_SERVED: AtomicBool = AtomicBool::new(false);
            if !FIRST_SERVED.swap(true, Ordering::Relaxed) {
                crate::kprintln!(
                    "pci: INTx delivery on line {line} served an interrupt wait \
                     (the cpu halted instead of polling)"
                );
            }
            // The IRQ handler masked the line when it fired; leave it masked until the next
            // wait, after the driver has cleared the device-side cause.
            return Ok(deliveries);
        }
        if crate::uart::ctrl_c_pending() {
            crate::arch::pci_intx::mask(line);
            return Err(WitPciError::Io(String::from(
                "the interrupt wait was interrupted from the console",
            )));
        }
        if crate::timer::uptime_ns() > deadline {
            crate::arch::pci_intx::mask(line);
            return Err(WitPciError::Io(String::from(
                "no interrupt delivery within the wait bound",
            )));
        }
        // Halt until an interrupt is pending: the PCI line itself, a keystroke, or the timer
        // backstop. Masked-`wfi` semantics (a pending-but-masked IRQ still wakes the halt)
        // close the lost-wakeup window between the checks above and the halt.
        crate::timer::wait_for_interrupt(INTX_WAIT_SLICE_NS);
    }
}

/// Bounds check for a BAR register access: `offset + width` must stay inside the window.
fn bar_access_in_bounds(offset: u64, width: WitAccessWidth, size: u64) -> Result<(), WitPciError> {
    let bytes = match width {
        WitAccessWidth::Byte => 1,
        WitAccessWidth::Word => 2,
        WitAccessWidth::Dword => 4,
        WitAccessWidth::Qword => 8,
    };
    match offset.checked_add(bytes) {
        Some(end) if end <= size => Ok(()),
        _ => Err(WitPciError::OutOfRange),
    }
}

/// Bounds check for the DMA copy accessors; out of range traps (same contract as the
/// `eo9:io` buffer accessors).
fn dma_byte_range(total: usize, offset: u64, len: u64) -> Result<(usize, usize)> {
    let end = offset.checked_add(len);
    match end {
        Some(end) if end <= total as u64 => Ok((offset as usize, end as usize)),
        _ => Err(wasmtime::Error::msg(
            "dma-buffer access out of bounds (this traps, as the WIT documents)",
        )),
    }
}
