//! Kernel-side root provider for `eo9:console-sink` — inject bytes into the console
//! input ring (wit/console-sink/console-sink.wit; docs/board/usb-ohci-plan.md M4).
//!
//! The narrow capability behind the USB keyboard demo: a HID service decodes boot
//! reports and `inject`s the resulting bytes; they land in the SAME ring serial input
//! uses (`arch::uart::inject_input`, the ring's second producer), so serial and USB
//! interleave, the existing Ctrl-C scan catches an injected 0x03 exactly like a
//! serial one, and the read-line provider needs no changes at all.
//!
//! **Containment.** Writing the console's input is the authority to TYPE AS THE
//! OPERATOR (the next prompt line executes whatever was injected), so this provider
//! is **never linked by default**: the bare `console-sink` kernel command-line token
//! grants it per boot — the `pci`/`platform` posture. Ring-full bytes are dropped
//! with a counter, never blocking the injector (drop-with-counter is the plan's
//! ring-full policy).

use alloc::string::String;
use core::sync::atomic::{AtomicBool, Ordering};

use wasmtime::component::{ComponentType, Lift, Linker, Lower, Resource, ResourceType};
use wasmtime::{Result, StoreContextMut};

use super::providers::KernelState;

/// Whether this boot granted the capability (the bare `console-sink` token).
static GRANTED: AtomicBool = AtomicBool::new(false);

/// Record the boot-time grant decision (called once from `runner::boot`).
pub fn set_granted(granted: bool) {
    GRANTED.store(granted, Ordering::Relaxed);
}

/// Whether linkers built for this boot should include the provider.
pub fn granted() -> bool {
    GRANTED.load(Ordering::Relaxed)
}

/// Host representation of `eo9:console-sink/types.sink-impl` (stateless token; the
/// ring is the state).
struct SinkCap;

#[derive(Clone, ComponentType, Lift, Lower)]
#[component(variant)]
#[allow(dead_code)]
enum WitSinkError {
    #[component(name = "denied")]
    Denied,
    #[component(name = "io")]
    Io(String),
}

/// Ceiling per `inject` call: far above any keystroke burst, low enough that a
/// runaway injector cannot make the host copy unbounded data per call.
const MAX_INJECT_BYTES: usize = 4096;

/// Register the `eo9:console-sink` root provider on a linker. Only call when the
/// boot granted it ([`granted`]); never linked by default.
pub fn add_console_sink(linker: &mut Linker<KernelState>) -> Result<()> {
    linker.instance("eo9:console-sink/types@0.1.0")?.resource(
        "sink-impl",
        ResourceType::host::<SinkCap>(),
        |_, _| Ok(()),
    )?;

    let mut interface = linker.instance("eo9:console-sink/sink@0.1.0")?;

    interface.func_wrap(
        "default",
        |_store: StoreContextMut<'_, KernelState>, (): ()| -> Result<(Resource<SinkCap>,)> {
            Ok((Resource::new_own(0),))
        },
    )?;

    interface.func_wrap(
        "inject",
        |_store: StoreContextMut<'_, KernelState>,
         (_sink, bytes): (Resource<SinkCap>, alloc::vec::Vec<u8>)|
         -> Result<(Result<u32, WitSinkError>,)> {
            if bytes.len() > MAX_INJECT_BYTES {
                return Ok((Err(WitSinkError::Io(alloc::format!(
                    "inject of {} bytes exceeds the {MAX_INJECT_BYTES}-byte per-call ceiling",
                    bytes.len()
                ))),));
            }
            let accepted = crate::arch::uart::inject_input(&bytes) as u32;
            Ok((Ok(accepted),))
        },
    )?;

    Ok(())
}
