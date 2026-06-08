//! Kernel-side root provider for `eo9:platform` — memory-mapped (non-PCI) device access
//! for wasm drivers.
//!
//! This is `pci_provider`'s sibling for devices that sit directly on the SoC bus
//! (wit/platform/platform.wit; docs/board/usb-ohci-plan.md §2 — the first consumer is
//! the OHCI USB lane). The kernel implements region enumeration, exclusive claiming,
//! width-explicit register access through the syndrome-valid `mmio` accessors, and DMA
//! buffers (the shared `super::dma` implementation, coherence brackets included); a
//! wasm component that imports `eo9:platform/platform` drives the device itself — the
//! kernel carries no device-class knowledge.
//!
//! **Containment.** A platform region that can take DMA buffers is, absent an IOMMU,
//! effectively full-memory authority (the same SPEC posture as PCI), so this provider
//! is **never linked by default**: the operator grants it per boot with the `platform`
//! kernel command-line token. The grant is *per region name* when spelled
//! `platform=<name>,<name>,…` — `enumerate` then shows exactly the granted subset and
//! a `claim` of a present-but-ungranted region answers `denied` (least authority: a
//! driver granted the OHCI block cannot wander into a neighbouring region; the bare
//! `platform` token grants the machine's whole table, the operator's explicit
//! choice). Without any token, a program importing `eo9:platform` is refused at
//! instantiation with the capability story (`shellexec::missing_capability`).
//!
//! **Exclusivity is machine-wide.** `claim` takes a name out of a kernel-global
//! claimed set (released when the region handle drops or its task tears down), so two
//! tasks racing for one controller observe `busy` — the WIT's contract. (The PCI
//! provider still tracks claims per task; converging it on this discipline is the
//! recorded follow-up there.)
//!
//! **Interrupts** answer `unsupported` in v1 — drivers poll with honest bounds (the
//! same posture as the board's PCI INTx; the OHCI plan's risk 7 records the follow-up).
//!
//! **Teardown.** DMA buffers free on handle drop / task teardown exactly like PCI's —
//! but a platform device has no bus-master bit to revoke, so the provider cannot
//! generically quiesce a device that is still fetching descriptors (GAPS: the M1 board
//! lane must pair region teardown with a device-specific quiesce hook before any real
//! DMA master joins the table; the v1 QEMU test regions do not master the bus).

use alloc::string::String;
use alloc::vec::Vec;
use core::future::Future;
use core::pin::Pin;

use wasmtime::component::{Accessor, ComponentType, Lift, Linker, Lower, Resource, ResourceType};
use wasmtime::{Result, StoreContextMut};

use super::dma::{DmaBuffer, MAX_DMA_ALLOC_BYTES, MAX_DMA_BUFFERS, dma_byte_range};
use super::providers::KernelState;
use super::shellexec::KLock;
use crate::platform::{self, RegionDef};

/// Boxed future shape for `func_wrap_concurrent` closures (same alias as the other
/// kernel providers).
type ConcurrentFuture<'a, R> = Pin<Box<dyn Future<Output = Result<R>> + Send + 'a>>;

use alloc::boxed::Box;

// -----------------------------------------------------------------------------------------
// Boot-time grant
// -----------------------------------------------------------------------------------------

/// What this boot granted: nothing, the whole region table (`platform`), or a named
/// subset (`platform=<name>,…`).
enum Grant {
    None,
    All,
    Named(Vec<String>),
}

static GRANT: KLock<Grant> = KLock::new(Grant::None);

/// Record the boot-time grant from the kernel command-line tokens (called once from
/// `runner::boot`). The bare `platform` token grants every region in the machine's
/// table; `platform=<name>,<name>,…` grants exactly those names (the least-authority
/// spelling — see the module docs); multiple `platform=` tokens union.
pub fn set_granted_from_tokens<'a>(tokens: impl Iterator<Item = &'a str>) {
    let mut grant = Grant::None;
    for token in tokens {
        if token == "platform" {
            grant = Grant::All;
        } else if let Some(names) = token.strip_prefix("platform=")
            && !matches!(grant, Grant::All)
        {
            let mut list = match core::mem::replace(&mut grant, Grant::None) {
                Grant::Named(list) => list,
                _ => Vec::new(),
            };
            for name in names.split(',').filter(|name| !name.is_empty()) {
                if !list.iter().any(|existing| existing == name) {
                    list.push(String::from(name));
                }
            }
            grant = Grant::Named(list);
        }
    }
    GRANT.with(|slot| *slot = grant);
}

/// Whether linkers built for this boot should include the `eo9:platform` root provider.
pub fn granted() -> bool {
    GRANT.with(|grant| !matches!(grant, Grant::None))
}

/// Whether this boot's grant covers the named region.
fn region_granted(name: &str) -> bool {
    GRANT.with(|grant| match grant {
        Grant::None => false,
        Grant::All => true,
        Grant::Named(names) => names.iter().any(|granted| granted == name),
    })
}

// -----------------------------------------------------------------------------------------
// Machine-wide claim registry
// -----------------------------------------------------------------------------------------

/// Region names currently claimed by *any* task: the busy semantics the WIT promises
/// are machine-wide, not per-task. Entries are released by region-handle drop or task
/// teardown (the `PlatformTables` destructor).
static CLAIMED: KLock<Vec<&'static str>> = KLock::new(Vec::new());

fn try_claim(name: &'static str) -> bool {
    CLAIMED.with(|claimed| {
        if claimed.iter().any(|&existing| existing == name) {
            false
        } else {
            claimed.push(name);
            true
        }
    })
}

fn release_claim(name: &'static str) {
    CLAIMED.with(|claimed| claimed.retain(|&existing| existing != name));
}

// -----------------------------------------------------------------------------------------
// Host resource representations and per-store state
// -----------------------------------------------------------------------------------------

/// Host representation of `eo9:platform/types.platform-impl` (stateless token; the
/// grant + the hardware are the state).
struct PlatformCap;
/// Host representation of `eo9:platform/platform.region`; the rep indexes the
/// claimed-region table.
struct RegionRes;
/// Host representation of `eo9:platform/platform.interrupt` — uninhabited in v1
/// (`enable-interrupts` answers `unsupported`), registered so the WIT links.
struct InterruptRes;
/// Host representation of `eo9:platform/platform.dma-buffer`.
struct DmaRes;

/// The task's platform state: claimed regions and live DMA buffers (rep → slot).
/// Lives on [`KernelState`], so each task tracks (and bounds) its own handles; the
/// claim *exclusivity* is the machine-wide [`CLAIMED`] set above.
#[derive(Default)]
pub struct PlatformTables {
    regions: Vec<Option<&'static RegionDef>>,
    buffers: Vec<Option<DmaBuffer>>,
}

impl PlatformTables {
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

    fn region(&self, rep: u32) -> Result<&'static RegionDef, WitPlatformError> {
        self.regions
            .get(rep as usize)
            .and_then(|slot| *slot)
            .ok_or(WitPlatformError::NotFound)
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

    fn close_region(&mut self, rep: u32) {
        if let Some(slot) = self.regions.get_mut(rep as usize)
            && let Some(region) = slot.take()
        {
            release_claim(region.name);
        }
    }

    fn close_buffer(&mut self, rep: u32) {
        if let Some(slot) = self.buffers.get_mut(rep as usize) {
            *slot = None;
        }
    }
}

impl Drop for PlatformTables {
    /// Task teardown — normal completion, trap, or kill — releases every machine-wide
    /// claim this task held, so a killed driver cannot wedge its controller `busy`
    /// forever. (DMA quiesce-before-free has no generic lever here — see the module
    /// docs and GAPS; the v1 region tables carry no bus-mastering device.)
    fn drop(&mut self) {
        for region in self.regions.iter().flatten() {
            release_claim(region.name);
        }
    }
}

impl KernelState {
    fn platform_tables(&mut self) -> &mut PlatformTables {
        &mut self.platform
    }
}

// -----------------------------------------------------------------------------------------
// WIT-shaped host types (eo9:platform)
// -----------------------------------------------------------------------------------------

#[derive(Clone, ComponentType, Lift, Lower)]
#[component(record)]
struct WitRegionInfo {
    name: String,
    size: u64,
    #[component(name = "has-irq")]
    has_irq: bool,
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

impl From<WitAccessWidth> for platform::AccessWidth {
    fn from(width: WitAccessWidth) -> platform::AccessWidth {
        match width {
            WitAccessWidth::Byte => platform::AccessWidth::Byte,
            WitAccessWidth::Word => platform::AccessWidth::Word,
            WitAccessWidth::Dword => platform::AccessWidth::Dword,
            WitAccessWidth::Qword => platform::AccessWidth::Qword,
        }
    }
}

#[derive(Clone, ComponentType, Lift, Lower)]
#[component(variant)]
#[allow(dead_code)]
enum WitPlatformError {
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

// -----------------------------------------------------------------------------------------
// Linker registration
// -----------------------------------------------------------------------------------------

/// Register the `eo9:platform` root provider (the `types` resource plus the full
/// `platform` interface) on a linker. Only call this when the boot granted the
/// capability ([`granted`]); it must never be linked by default.
pub fn add_platform(linker: &mut Linker<KernelState>) -> Result<()> {
    linker.instance("eo9:platform/types@0.1.0")?.resource(
        "platform-impl",
        ResourceType::host::<PlatformCap>(),
        |_, _| Ok(()),
    )?;

    let mut interface = linker.instance("eo9:platform/platform@0.1.0")?;

    interface.resource(
        "region",
        ResourceType::host::<RegionRes>(),
        |mut store: StoreContextMut<'_, KernelState>, rep| {
            store.data_mut().platform_tables().close_region(rep);
            Ok(())
        },
    )?;
    interface.resource(
        "interrupt",
        ResourceType::host::<InterruptRes>(),
        |_store: StoreContextMut<'_, KernelState>, _rep| Ok(()),
    )?;
    interface.resource(
        "dma-buffer",
        ResourceType::host::<DmaRes>(),
        |mut store: StoreContextMut<'_, KernelState>, rep| {
            store.data_mut().platform_tables().close_buffer(rep);
            Ok(())
        },
    )?;

    interface.func_wrap(
        "default",
        |_store: StoreContextMut<'_, KernelState>,
         (): ()|
         -> Result<(Resource<PlatformCap>,)> { Ok((Resource::new_own(0),)) },
    )?;

    // --- enumeration and claiming ---------------------------------------------------------

    interface.func_wrap_concurrent(
        "enumerate",
        |_accessor: &Accessor<KernelState>,
         (_cap,): (Resource<PlatformCap>,)|
         -> ConcurrentFuture<'_, (Result<Vec<WitRegionInfo>, WitPlatformError>,)> {
            Box::pin(async move {
                // The capability's view IS the grant: a named grant shows exactly its
                // subset of the machine table (least authority — the module docs).
                let regions: Vec<WitRegionInfo> = platform::regions()
                    .iter()
                    .filter(|region| region_granted(region.name))
                    .map(|region| WitRegionInfo {
                        name: String::from(region.name),
                        size: region.size,
                        has_irq: region.has_irq,
                    })
                    .collect();
                Ok((Ok(regions),))
            })
        },
    )?;

    interface.func_wrap_concurrent(
        "claim",
        |accessor: &Accessor<KernelState>,
         (_cap, name): (Resource<PlatformCap>, String)|
         -> ConcurrentFuture<'_, (Result<Resource<RegionRes>, WitPlatformError>,)> {
            Box::pin(async move {
                let result = (|| {
                    // No such region anywhere: not-found. Present but outside this
                    // boot's grant: denied — the typed cross-region refusal.
                    let region = platform::region(&name).ok_or(WitPlatformError::NotFound)?;
                    if !region_granted(region.name) {
                        return Err(WitPlatformError::Denied);
                    }
                    // Machine-wide exclusivity: one driver per region, busy otherwise.
                    if !try_claim(region.name) {
                        return Err(WitPlatformError::Busy);
                    }
                    Ok(region)
                })();
                let result = match result {
                    Err(error) => Err(error),
                    Ok(region) => accessor.with(|mut access| {
                        let tables = access.data_mut().platform_tables();
                        let rep = PlatformTables::insert(&mut tables.regions, region);
                        Ok(Resource::new_own(rep))
                    }),
                };
                Ok((result,))
            })
        },
    )?;

    // --- registers --------------------------------------------------------------------------

    interface.func_wrap_concurrent(
        "read",
        |accessor: &Accessor<KernelState>,
         (region, offset, width): (Resource<RegionRes>, u64, WitAccessWidth)|
         -> ConcurrentFuture<'_, (Result<u64, WitPlatformError>,)> {
            Box::pin(async move {
                let claimed = accessor
                    .with(|mut access| access.data_mut().platform_tables().region(region.rep()));
                let result = claimed.and_then(|region| {
                    platform::region_read(region, offset, width.into())
                        .ok_or(WitPlatformError::OutOfRange)
                });
                Ok((result,))
            })
        },
    )?;

    interface.func_wrap_concurrent(
        "write",
        |accessor: &Accessor<KernelState>,
         (region, offset, width, value): (Resource<RegionRes>, u64, WitAccessWidth, u64)|
         -> ConcurrentFuture<'_, (Result<(), WitPlatformError>,)> {
            Box::pin(async move {
                let claimed = accessor
                    .with(|mut access| access.data_mut().platform_tables().region(region.rep()));
                let result = claimed.and_then(|region| {
                    if platform::region_write(region, offset, width.into(), value) {
                        Ok(())
                    } else {
                        Err(WitPlatformError::OutOfRange)
                    }
                });
                Ok((result,))
            })
        },
    )?;

    // --- interrupts (unsupported in v1 — drivers poll with honest bounds) --------------------

    interface.func_wrap_concurrent(
        "enable-interrupts",
        |_accessor: &Accessor<KernelState>,
         (_region,): (Resource<RegionRes>,)|
         -> ConcurrentFuture<'_, (Result<Resource<InterruptRes>, WitPlatformError>,)> {
            // Platform interrupt routing is the recorded follow-up (usb-ohci-plan
            // risk 7: GIC SPIs 216/219); v1 answers `unsupported`, never a wrong wait.
            Box::pin(async move { Ok((Err(WitPlatformError::Unsupported),)) })
        },
    )?;

    interface.func_wrap_concurrent(
        "wait",
        |_accessor: &Accessor<KernelState>,
         (_interrupt,): (Resource<InterruptRes>,)|
         -> ConcurrentFuture<'_, (Result<u64, WitPlatformError>,)> {
            // Unreachable in v1 (no interrupt handle can be created), but the WIT
            // surface stays complete and typed.
            Box::pin(async move { Ok((Err(WitPlatformError::Unsupported),)) })
        },
    )?;

    // --- DMA ----------------------------------------------------------------------------------

    interface.func_wrap_concurrent(
        "alloc-dma",
        |accessor: &Accessor<KernelState>,
         (region, len): (Resource<RegionRes>, u64)|
         -> ConcurrentFuture<'_, (Result<Resource<DmaRes>, WitPlatformError>,)> {
            Box::pin(async move {
                let result = accessor.with(|mut access| {
                    let tables = access.data_mut().platform_tables();
                    tables.region(region.rep())?;
                    if len == 0 || len > MAX_DMA_ALLOC_BYTES {
                        return Err(WitPlatformError::Exhausted);
                    }
                    if tables.buffers.iter().flatten().count() >= MAX_DMA_BUFFERS {
                        return Err(WitPlatformError::Exhausted);
                    }
                    let buffer = DmaBuffer::allocate(len as usize);
                    let rep = PlatformTables::insert(&mut tables.buffers, buffer);
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
                .platform_tables()
                .buffer(buffer.rep())?
                .bus_address(),))
        },
    )?;

    interface.func_wrap(
        "dma-len",
        |mut store: StoreContextMut<'_, KernelState>,
         (buffer,): (Resource<DmaRes>,)|
         -> Result<(u64,)> {
            Ok((store
                .data_mut()
                .platform_tables()
                .buffer(buffer.rep())?
                .len() as u64,))
        },
    )?;

    interface.func_wrap(
        "dma-read",
        |mut store: StoreContextMut<'_, KernelState>,
         (buffer, offset, len): (Resource<DmaRes>, u64, u64)|
         -> Result<(Vec<u8>,)> {
            let buffer = store.data_mut().platform_tables().buffer(buffer.rep())?;
            let (start, end) = dma_byte_range(buffer.len(), offset, len)?;
            // Invalidate before the read: the device may have DMA'd here since the
            // CPU last looked (no-op on coherent machines).
            buffer.sync_range(start, end);
            Ok((buffer.bytes()[start..end].to_vec(),))
        },
    )?;

    interface.func_wrap(
        "dma-write",
        |mut store: StoreContextMut<'_, KernelState>,
         (buffer, offset, bytes): (Resource<DmaRes>, u64, Vec<u8>)|
         -> Result<()> {
            let buffer = store
                .data_mut()
                .platform_tables()
                .buffer_mut(buffer.rep())?;
            let (start, end) = dma_byte_range(buffer.len(), offset, bytes.len() as u64)?;
            buffer.bytes_mut()[start..end].copy_from_slice(&bytes);
            // Clean to the PoC after the write: the device's next fetch reads DRAM,
            // and the sweep's barrier orders this ahead of any subsequent doorbell
            // register write (no-op on coherent machines).
            buffer.sync_range(start, end);
            Ok(())
        },
    )?;

    Ok(())
}
