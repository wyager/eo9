//! `usb.msd` — a USB mass-storage (Bulk-Only Transport) driver as an ordinary wasm
//! component: the L2 lane of docs/board/usb-msd-plan.md.
//!
//! Targets the crate-local `eo9:usb-msd/msd` world: imports the USB host capability
//! (`eo9:usb/usb`) plus `eo9:time/time` (connect-watch and ready-loop pacing) and
//! `eo9:text/text` (one diagnostic line), and exports `eo9:disk/disk` backed by the
//! first connected port's mass-storage device. The driver holds no policy of its own:
//! which controller (and therefore which ports) it can see is entirely the usb
//! provider's business — `usb.ohci-pci` under QEMU, `usb.ohci --region …` on the
//! board, `usb.deny` to refuse.
//!
//! Shape of the device conversation (BOT 1.0 + the six-command SCSI set, all framing
//! and sequencing host-tested in `crates/eo9-msd`):
//!
//! * **Enumerate.** Watch the root-hub ports (event-driven over `watch-ports` where
//!   the provider routes the controller interrupt, self-paced sweeps otherwise),
//!   `attach` the first connected port, read the descriptor chain, and find the
//!   mass-storage interface — class 08, subclass 06 (SCSI transparent), protocol 0x50
//!   (Bulk-Only) — anything else refuses typed, naming what it found. Warm-state
//!   doctrine: the provider's `attach` always does its own port reset, so whatever
//!   U-Boot left behind is re-enumerated from scratch.
//! * **Bring-up.** SET_CONFIGURATION, GET MAX LUN (a STALL means "LUN 0 only", which
//!   is also the only thing this driver supports — a device insisting on more LUNs
//!   refuses typed), open the bulk pair (toggles start at DATA0 — the eo9:usb
//!   open-after-SET_CONFIGURATION contract), TEST UNIT READY until ready (bounded;
//!   the engine's REQUEST SENSE rung eats the post-reset UNIT ATTENTION), then
//!   READ CAPACITY(10) for the geometry.
//! * **I/O.** One command at a time (QD1 per the plan — TD chaining is the recorded
//!   follow-up): byte-addressed `eo9:disk` reads and writes become READ(10)/WRITE(10)
//!   over 64 KiB-capped chunks, read-modify-writing partial edge blocks (the
//!   disk.virtio `write_bytes` shape, as sdcard-plan §B.1 prescribes). Stall recovery
//!   is the engine's: the provider has already recovered its half when `stall` comes
//!   back, the engine issues CLEAR_FEATURE(ENDPOINT_HALT) and walks the BOT ladder
//!   (sense on failure, mass-storage reset + both clear-halts on phase errors).
//!
//! `flush` is a documented no-op: BOT in this command set has no cache-control verb
//! (SYNCHRONIZE CACHE is outside the six), and sticks do not cache like SD FTLs —
//! durability is the underlying device's, and the flasher's read-back-verify is the
//! real durability check (the same honesty as fs-eofs and the SD flush note; plan §9
//! workaround 2).
//!
//! Like `disk.virtio`, the driver genuinely awaits its imports, the waits stay
//! bounded where the waiting happens (the provider bounds transfers, the connect
//! watch and ready loop bound themselves), and a dead device surfaces as a typed
//! `io` error, never a hang or a trap. The documented default state (no configure
//! interface) is "claim the first connected port's device on first use"; first use
//! prints one `usb.msd: …` line so a session shows what was probed. The synchronous
//! `size` query reports 0 until the first awaited operation has brought the device
//! up (consumers issue one read, then re-ask — the fs.eofs convention).

#![no_std]

extern crate alloc;

use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;
use core::cell::RefCell;

// Linked for its runtime contract (allocator, panic handler, diagnostics import); the
// provider state here is the take/put `Slot` below, not `ProviderState`.
use eo9_guest as _;

use eo9_msd::bot::request;
use eo9_msd::{Bot, MsdError, Transport, TransportError};
use eo9_ohci::descriptor::{self, Descriptor};
use eo9_ohci::setup;

wit_bindgen::generate!({
    world: "msd",
    path: "wit",
    // Pull in bindings for eo9:usb/types, eo9:disk/types, and eo9:io/buffers, which
    // the imported and exported interfaces use but the world does not name directly.
    generate_all,
});

use eo9::text::text;
use eo9::time::time as time_api;
use eo9::usb::usb;
use exports::eo9::disk::disk::{self, Buffer, ReadError, ReadResult, WriteError, WriteResult};
use exports::eo9::disk::types;

// The user-facing manual, embedded as the `eo9-manual` custom section and rendered by
// `man usb.msd` in eosh (docs/design/component-manuals.md; this component is in the
// required-manual set).
eo9_guest::manual! {
    name: "usb.msd",
    synopsis: "a USB mass-storage stick as a block device — Bulk-Only Transport over the composed eo9:usb",
    description: [
        "Drives the first connected port's USB mass-storage device (class 08.06.50: SCSI-transparent",
        "over Bulk-Only Transport) through the granted usb capability and exports eo9:disk, so",
        "filesystems, partition middleware, and the stick flasher run over a USB stick by composition.",
        "On first use it enumerates the device, prints one line naming it (INQUIRY strings + READ",
        "CAPACITY geometry), and serves byte-addressed reads and writes as READ(10)/WRITE(10) with",
        "read-modify-write at block edges. Errors are typed, never traps: a stalled endpoint walks the",
        "BOT recovery ladder (clear-halt, REQUEST SENSE, mass-storage reset), a device that is not",
        "mass-storage — or insists on more than LUN 0 — refuses with what it found, and `flush` is a",
        "documented no-op (this command set has no cache verb; sticks do not cache like SD FTLs).",
        "Full-speed only on the OHCI shells: ~1 MiB/s is the honest ceiling.",
    ],
    args: [],
    examples: [
        { line: "usb.ohci-pci $ usb.msd $ mdcheck",
          doc: "QEMU: -device pci-ohci + -device usb-storage; scratch write/read-back via eo9:disk" },
        { line: "usb.ohci --region usb-host1-ohci $ usb.msd $ mdcheck",
          doc: "the board: the stick in a USB2-A port (the direct OHCI ports — xhci's are unreachable)" },
        { line: "usb.deny $ usb.msd $ mdcheck",
          doc: "the refusal probe: every disk operation answers a typed io error naming the denial" },
    ],
    see_also: "usbcheck, mdcheck, disk.virtio, fs.eofs",
}

// ------------------------------------------------------------------------------------------
// Constants
// ------------------------------------------------------------------------------------------

/// The mass-storage interface triple this driver supports (USB MSC overview §1:
/// class 08; subclass 06 = SCSI transparent command set; protocol 0x50 = Bulk-Only
/// Transport). Anything else refuses typed.
const CLASS_MASS_STORAGE: u8 = 0x08;
const SUBCLASS_SCSI_TRANSPARENT: u8 = 0x06;
const PROTOCOL_BULK_ONLY: u8 = 0x50;

/// Wall-clock budget for the connect watch before concluding nothing is plugged in
/// (the usb.kbd shape: a stick present at boot answers on the first sweep; the bench
/// shape is "plug, then re-run").
const WATCH_WINDOW_NS: u64 = 2_500_000_000;
/// Sweep pacing where the provider has no event surface (`watch-ports` answers
/// `unsupported` — the v1 board residue; with events the RHSC wait paces the loop
/// and this sleep never runs).
const WATCH_PACE_NS: u64 = 50_000_000;

/// TEST UNIT READY settle loop: attempts and pacing. Sticks come ready within a few
/// hundred ms of reset; each failed attempt consumed its sense (UNIT ATTENTION) via
/// the engine's ladder, so the bound is generous without being a hang.
const READY_ATTEMPTS: u32 = 20;
const READY_PACE_NS: u64 = 50_000_000;

/// Per-command transfer cap: 128 blocks of 512 = 64 KiB per READ(10)/WRITE(10) at
/// QD1 (the plan's v1 shape; the provider loops its own 8 KiB bulk grain under
/// each command). Larger block sizes keep the same byte cap.
const DATA_CAP: u64 = 64 * 1024;

// ------------------------------------------------------------------------------------------
// Driver state (the disk.virtio take/put discipline)
// ------------------------------------------------------------------------------------------

/// The transport the BOT engine drives: the bulk pair plus the control-side
/// requests, over the composed eo9:usb provider. The eo9:usb halt contract is the
/// engine's to honour (a `stall` answer means the provider already recovered its
/// half; the engine owes the CLEAR_FEATURE issued here).
struct UsbTransport {
    /// Keeps the attached device (and the provider's claim) alive.
    device: usb::Device,
    endpoint_in: usb::Endpoint,
    endpoint_out: usb::Endpoint,
    /// Full endpoint addresses (0x8n / 0x0n) — CLEAR_FEATURE's wIndex wants them.
    address_in: u8,
    address_out: u8,
    /// The mass-storage interface number — the class requests' wIndex.
    interface: u8,
}

fn map_usb(err: usb::UsbError) -> TransportError {
    match err {
        usb::UsbError::Stall => TransportError::Stall,
        usb::UsbError::Timeout => TransportError::Timeout,
        other => TransportError::Other(format!("{other:?}")),
    }
}

impl Transport for UsbTransport {
    async fn bulk_out(&mut self, data: &[u8]) -> Result<(), TransportError> {
        usb::bulk_write(&self.endpoint_out, data.to_vec())
            .await
            .map_err(map_usb)
    }

    async fn bulk_in(&mut self, length: u32) -> Result<Vec<u8>, TransportError> {
        usb::bulk_read(&self.endpoint_in, length)
            .await
            .map_err(map_usb)
    }

    async fn clear_halt_in(&mut self) -> Result<(), TransportError> {
        self.clear_halt(self.address_in).await
    }

    async fn clear_halt_out(&mut self) -> Result<(), TransportError> {
        self.clear_halt(self.address_out).await
    }

    async fn mass_storage_reset(&mut self) -> Result<(), TransportError> {
        usb::control_out(
            &self.device,
            request::RESET_REQUEST_TYPE,
            request::RESET,
            0,
            u16::from(self.interface),
            Vec::new(),
        )
        .await
        .map_err(map_usb)
    }
}

impl UsbTransport {
    /// CLEAR_FEATURE(ENDPOINT_HALT) — the consumer's half of the eo9:usb halt
    /// contract (resets the device-side data toggle, USB 2.0 §9.4.5).
    async fn clear_halt(&self, endpoint_address: u8) -> Result<(), TransportError> {
        usb::control_out(
            &self.device,
            request::CLEAR_FEATURE_REQUEST_TYPE,
            request::CLEAR_FEATURE,
            request::FEATURE_ENDPOINT_HALT,
            u16::from(endpoint_address),
            Vec::new(),
        )
        .await
        .map_err(map_usb)
    }
}

/// The brought-up device: the BOT engine over its transport, plus the geometry.
struct Driver {
    bot: Bot<UsbTransport>,
    block_size: u32,
    capacity_bytes: u64,
}

/// Failures of the byte-addressed disk operations, mapped to the WIT error variants
/// by the export glue.
enum DiskFail {
    OutOfRange,
    Io(String),
}

/// The provider's state slot. Operations await mid-flight, so the driver is taken
/// out of the slot for the duration of an operation and put back afterwards; a
/// second activation arriving while the slot is `Busy` gets a typed error — never a
/// `RefCell` re-borrow trap. (The disk.virtio discipline, verbatim.)
enum Slot {
    Empty,
    Busy,
    Ready(Driver),
}

struct DriverState {
    inner: RefCell<Slot>,
}

// SAFETY: guest components run single-threaded (shared-memory threading is an
// ungranted capability — see SPEC "Execution APIs"); `Sync` is only needed for the
// `static`.
unsafe impl Sync for DriverState {}

static STATE: DriverState = DriverState {
    inner: RefCell::new(Slot::Empty),
};

impl DriverState {
    fn take(&self) -> Result<Option<Driver>, DiskFail> {
        let mut slot = self.inner.borrow_mut();
        match core::mem::replace(&mut *slot, Slot::Busy) {
            Slot::Ready(driver) => Ok(Some(driver)),
            Slot::Empty => Ok(None),
            Slot::Busy => Err(DiskFail::Io(String::from(
                "usb.msd: the device is busy with a concurrent request; issue disk \
                 operations sequentially (QD1 by design — docs/board/usb-msd-plan.md)",
            ))),
        }
    }

    fn put(&self, driver: Driver) {
        *self.inner.borrow_mut() = Slot::Ready(driver);
    }

    fn clear(&self) {
        *self.inner.borrow_mut() = Slot::Empty;
    }

    /// The capacity if the device is up and idle; `None` otherwise. For the
    /// synchronous `size` query only — it cannot bring the device up (bring-up
    /// awaits).
    fn peek_capacity(&self) -> Option<u64> {
        match &*self.inner.borrow() {
            Slot::Ready(driver) => Some(driver.capacity_bytes),
            _ => None,
        }
    }
}

/// Returns the driver to the slot when an operation ends — including by
/// *cancellation* (the operation's future dropped mid-await). A cancelled BOT
/// exchange may have left the device mid-command; the engine tracks that itself
/// (`mid_command`) and runs reset recovery before the next command touches the
/// wire, so the guard's only duty is the slot restore.
struct DriverGuard {
    driver: Option<Driver>,
}

impl Drop for DriverGuard {
    fn drop(&mut self) {
        if let Some(driver) = self.driver.take() {
            STATE.put(driver);
        }
    }
}

/// Releases the bring-up claim (the `Busy` slot) if bring-up never completes: an
/// error return *or a future dropped mid-bring-up* restores `Empty` so the next use
/// retries, instead of wedging the instance behind a permanent typed-busy answer.
struct BringUpClaim {
    armed: bool,
}

impl BringUpClaim {
    fn defuse(mut self) {
        self.armed = false;
    }
}

impl Drop for BringUpClaim {
    fn drop(&mut self) {
        if self.armed {
            STATE.clear();
        }
    }
}

/// Run `f` over the brought-up driver, probing and enumerating on first use (the
/// documented default state — there is no configure interface).
async fn with_driver<R>(
    f: impl AsyncFnOnce(&mut Driver) -> Result<R, DiskFail>,
) -> Result<R, DiskFail> {
    let driver = match STATE.take()? {
        Some(driver) => driver,
        None => {
            // The slot is `Busy` from the `take` above: arm the restore before the
            // first await of bring-up.
            let claim = BringUpClaim { armed: true };
            let driver = bring_up().await.map_err(DiskFail::Io)?;
            claim.defuse();
            driver
        }
    };
    let mut guard = DriverGuard {
        driver: Some(driver),
    };
    f(guard.driver.as_mut().expect("guard holds the driver")).await
    // `guard` drops here (or wherever this future is dropped), returning the driver.
}

// ------------------------------------------------------------------------------------------
// Bring-up: enumerate, find the BOT interface, configure, settle, measure.
// ------------------------------------------------------------------------------------------

/// Render one MsdError for the io(...) channel, naming the sense triple where the
/// device gave one.
fn msd_failure(what: &str, error: MsdError) -> String {
    match error {
        MsdError::CommandFailed { sense: Some(sense) } => format!(
            "usb.msd: {what} failed: {} (key {:#x}, asc {:#04x}, ascq {:#04x})",
            sense.key_name(),
            sense.key,
            sense.asc,
            sense.ascq,
        ),
        MsdError::CommandFailed { sense: None } => {
            format!("usb.msd: {what} failed (no sense data available)")
        }
        MsdError::Protocol(protocol) => format!(
            "usb.msd: {what}: BOT protocol failure {protocol:?} (reset recovery was performed)"
        ),
        MsdError::Transport(transport) => format!("usb.msd: {what}: transport: {transport:?}"),
        MsdError::ShortData { expected, got } => {
            format!("usb.msd: {what}: short data ({got} of {expected} byte(s))")
        }
    }
}

fn usb_failure(what: &str, error: usb::UsbError) -> String {
    match error {
        usb::UsbError::Denied => format!(
            "usb.msd: {what}: the usb capability refused (denied) — was the shell composed \
             over usb.deny, or did the boot withhold the controller's grant?"
        ),
        usb::UsbError::NoController => format!(
            "usb.msd: {what}: no usb host controller is visible through the granted usb \
             capability — compose over usb.ohci-pci (QEMU: -device pci-ohci) or usb.ohci \
             with the board's platform region grant"
        ),
        other => format!("usb.msd: {what}: {other:?}"),
    }
}

/// Find, attach, and bring up the first connected port's mass-storage device.
/// Every step reports a typed, labelled error — device weirdness is an `io` failure
/// of the disk operation, never a trap.
async fn bring_up() -> Result<Driver, String> {
    let root = usb::default();
    let time = time_api::default();

    // Controller identity (the shell's bring-up claim happens here).
    let info = usb::controller(&root)
        .await
        .map_err(|err| usb_failure("controller", err))?;

    // The connect watch: sweep, then park on the root-hub change event where the
    // provider supports it; only the `unsupported` fallback paces itself (the
    // usb.kbd shape, timer-crutch audit A4). Bounded by wall clock.
    let watch_started = time_api::monotonic_now(&time);
    let mut after_timed_out_wait = false;
    let port = 'watch: loop {
        for port in 1..=info.ports {
            let status = usb::port(&root, port)
                .await
                .map_err(|err| usb_failure("port", err))?;
            if status.connected {
                if after_timed_out_wait {
                    // The sweep found a connect the RHSC event never delivered:
                    // loud, never silent (the liveness doctrine).
                    let _ = text::write(
                        &text::default(),
                        text::OutputStream::Out,
                        "liveness: usb.msd: the port sweep found a connect after a \
                         timed-out watch-ports wait - the RHSC event owed this wake\n",
                    );
                }
                break 'watch port;
            }
        }
        let now = time_api::monotonic_now(&time);
        if now.nanoseconds.saturating_sub(watch_started.nanoseconds) > WATCH_WINDOW_NS {
            return Err(String::from(
                "usb.msd: no device connected on any root-hub port within the watch \
                 window — plug the stick into a root (USB2-A) port and retry",
            ));
        }
        after_timed_out_wait = false;
        match usb::watch_ports(&root).await {
            Ok(usb::WatchOutcome::Changed) => {}
            Ok(usb::WatchOutcome::TimedOut) => after_timed_out_wait = true,
            // No event surface (or the wait failed): the polled fallback paces.
            Ok(usb::WatchOutcome::Unsupported) | Err(_) => {
                time_api::sleep(&time, WATCH_PACE_NS).await;
            }
        }
    };

    // Attach (port reset + SET_ADDRESS — the warm-state doctrine: always our own
    // reset, whatever U-Boot left behind), then the descriptor chain.
    let device = usb::attach(&root, port)
        .await
        .map_err(|err| usb_failure("attach", err))?;
    let device_bytes = usb::control_in(
        &device,
        0x80,
        6,
        u16::from(setup::descriptor_type::DEVICE) << 8,
        0,
        18,
    )
    .await
    .map_err(|err| usb_failure("device descriptor", err))?;
    let parsed = descriptor::DeviceDescriptor::parse(&device_bytes)
        .ok_or_else(|| String::from("usb.msd: the device descriptor did not parse"))?;
    if parsed.class == 0x09 {
        return Err(String::from(
            "usb.msd: the connected device is a hub (class 09) — v1 drives a directly \
             plugged stick only; move it to a root (USB2-A) port",
        ));
    }

    let head = usb::control_in(
        &device,
        0x80,
        6,
        u16::from(setup::descriptor_type::CONFIGURATION) << 8,
        0,
        9,
    )
    .await
    .map_err(|err| usb_failure("configuration head", err))?;
    let configuration = descriptor::ConfigurationDescriptor::parse(&head)
        .ok_or_else(|| String::from("usb.msd: the configuration descriptor did not parse"))?;
    let blob = usb::control_in(
        &device,
        0x80,
        6,
        u16::from(setup::descriptor_type::CONFIGURATION) << 8,
        0,
        configuration.total_length,
    )
    .await
    .map_err(|err| usb_failure("configuration chain", err))?;

    // The mass-storage interface and its bulk pair. The walk tracks which
    // interface its endpoints belong to (the usbcheck probe's shape) and refuses
    // typed on non-BOT flavours — CBI and UFI sticks predate this century.
    let mut current: Option<descriptor::InterfaceDescriptor> = None;
    let mut msd_interface: Option<descriptor::InterfaceDescriptor> = None;
    let mut bulk_in: Option<(u8, u16)> = None;
    let mut bulk_out: Option<(u8, u16)> = None;
    for entry in descriptor::descriptors(&blob) {
        match entry {
            Descriptor::Interface(interface) => {
                if msd_interface.is_some() {
                    break; // past the mass-storage interface: the pair is complete
                }
                if interface.class == CLASS_MASS_STORAGE {
                    if interface.subclass != SUBCLASS_SCSI_TRANSPARENT
                        || interface.protocol != PROTOCOL_BULK_ONLY
                    {
                        return Err(format!(
                            "usb.msd: unsupported mass-storage flavour {:02x}.{:02x}.{:02x} \
                             (need 08.06.50, SCSI-transparent over Bulk-Only Transport)",
                            interface.class, interface.subclass, interface.protocol,
                        ));
                    }
                    msd_interface = Some(interface);
                }
                current = Some(interface);
            }
            Descriptor::Endpoint(endpoint)
                if current.map(|i| i.class) == Some(CLASS_MASS_STORAGE)
                    && endpoint.attributes & 0b11 == 2 =>
            {
                let slot = if endpoint.is_in() {
                    &mut bulk_in
                } else {
                    &mut bulk_out
                };
                slot.get_or_insert((endpoint.address, endpoint.max_packet_size));
            }
            _ => {}
        }
    }
    let interface = msd_interface.ok_or_else(|| {
        format!(
            "usb.msd: the device ({:04x}:{:04x}, class {:02x}) has no mass-storage \
             interface (class 08) — is the stick really on this controller's port?",
            parsed.vendor_id, parsed.product_id, parsed.class,
        )
    })?;
    let (address_in, mps_in) = bulk_in.ok_or_else(|| {
        String::from("usb.msd: the mass-storage interface has no bulk-IN endpoint")
    })?;
    let (address_out, mps_out) = bulk_out.ok_or_else(|| {
        String::from("usb.msd: the mass-storage interface has no bulk-OUT endpoint")
    })?;

    // SET_CONFIGURATION first: bulk toggles start at DATA0 from here (§9.4.5), and
    // the class requests below address a configured interface.
    let configure = setup::set_configuration(configuration.configuration_value);
    usb::control_out(
        &device,
        configure.request_type,
        configure.request,
        configure.value,
        configure.index,
        Vec::new(),
    )
    .await
    .map_err(|err| usb_failure("SET_CONFIGURATION", err))?;

    // GET MAX LUN (BOT §3.2): a STALL is the spec'd "LUN 0 only" answer. More than
    // LUN 0 refuses typed (plan §1.2: LUN 0 unconditionally — sticks don't).
    match usb::control_in(
        &device,
        request::GET_MAX_LUN_REQUEST_TYPE,
        request::GET_MAX_LUN,
        0,
        u16::from(interface.interface_number),
        1,
    )
    .await
    {
        Ok(data) => {
            let max_lun = data.first().copied().unwrap_or(0);
            if max_lun > 0 {
                return Err(format!(
                    "usb.msd: the device reports {} LUNs (max LUN index {max_lun}); \
                     only LUN 0 is supported",
                    u16::from(max_lun) + 1,
                ));
            }
        }
        Err(usb::UsbError::Stall) => {} // "LUN 0 only", per spec
        Err(other) => return Err(usb_failure("GET MAX LUN", other)),
    }

    // The bulk pair (provider-side toggles reset to DATA0 by the open contract).
    let endpoint_in = usb::open_bulk_in(&device, address_in, mps_in)
        .await
        .map_err(|err| usb_failure("open-bulk-in", err))?;
    let endpoint_out = usb::open_bulk_out(&device, address_out, mps_out)
        .await
        .map_err(|err| usb_failure("open-bulk-out", err))?;

    let mut bot = Bot::new(UsbTransport {
        device,
        endpoint_in,
        endpoint_out,
        address_in,
        address_out,
        interface: interface.interface_number,
    });

    // TEST UNIT READY until ready (bounded). Each failure already fetched and
    // consumed the sense — the post-reset UNIT ATTENTION drains here by design.
    let mut ready = false;
    let mut last_failure = None;
    for attempt in 0..READY_ATTEMPTS {
        match bot.test_unit_ready().await {
            Ok(()) => {
                ready = true;
                break;
            }
            Err(MsdError::CommandFailed { sense }) => {
                last_failure = Some(MsdError::CommandFailed { sense });
                if attempt + 1 < READY_ATTEMPTS {
                    time_api::sleep(&time, READY_PACE_NS).await;
                }
            }
            Err(other) => return Err(msd_failure("TEST UNIT READY", other)),
        }
    }
    if !ready {
        return Err(msd_failure(
            "TEST UNIT READY (the unit never came ready within the bound)",
            last_failure.unwrap_or(MsdError::CommandFailed { sense: None }),
        ));
    }

    // INQUIRY (the identity line) + READ CAPACITY(10) (the geometry).
    let inquiry = bot
        .inquiry()
        .await
        .map_err(|err| msd_failure("INQUIRY", err))?;
    if inquiry.device_type != 0 {
        return Err(format!(
            "usb.msd: INQUIRY reports peripheral device type {:#04x} — not a \
             direct-access block device; this driver serves sticks/disks only",
            inquiry.device_type,
        ));
    }
    let capacity = bot
        .read_capacity()
        .await
        .map_err(|err| msd_failure("READ CAPACITY(10)", err))?;
    if capacity.block_size == 0 || capacity.block_size > DATA_CAP as u32 {
        return Err(format!(
            "usb.msd: READ CAPACITY(10) reports an unusable block size of {} byte(s)",
            capacity.block_size,
        ));
    }

    // One best-effort diagnostic line so a session shows what was probed.
    let handle = text::default();
    let line = format!(
        "usb.msd: {:04x}:{:04x} '{}' '{}' rev '{}' on port {port}: {} blocks of {} \
         bytes ({} MiB), bulk IN {address_in:#04x} OUT {address_out:#04x}\n",
        parsed.vendor_id,
        parsed.product_id,
        inquiry.vendor_str(),
        inquiry.product_str(),
        inquiry.revision_str(),
        u64::from(capacity.last_lba) + 1,
        capacity.block_size,
        capacity.bytes() / (1024 * 1024),
    );
    let _ = text::write(&handle, text::OutputStream::Out, &line);

    Ok(Driver {
        bot,
        block_size: capacity.block_size,
        capacity_bytes: capacity.bytes(),
    })
}

// ------------------------------------------------------------------------------------------
// Byte-addressed operations over the block device (the disk.virtio shape)
// ------------------------------------------------------------------------------------------

impl Driver {
    /// Read `len` bytes at byte offset `offset`.
    async fn read_bytes(&mut self, offset: u64, len: u64) -> Result<Vec<u8>, DiskFail> {
        self.check_range(offset, len)?;
        let block_size = u64::from(self.block_size);
        let mut out: Vec<u8> = Vec::with_capacity(len as usize);
        let mut cursor = offset;
        let mut remaining = len;
        while remaining > 0 {
            let first_block = cursor / block_size;
            let within = cursor - first_block * block_size;
            let take = core::cmp::min(remaining, DATA_CAP - within);
            let blocks = (within + take).div_ceil(block_size) as u16;
            let chunk = self
                .bot
                .read10(first_block as u32, blocks, self.block_size)
                .await
                .map_err(|err| DiskFail::Io(msd_failure("READ(10)", err)))?;
            let start = within as usize;
            let end = (within + take) as usize;
            out.extend_from_slice(&chunk[start..end]);
            cursor += take;
            remaining -= take;
        }
        Ok(out)
    }

    /// Write `bytes` at byte offset `offset`, read–modify–writing partial edge
    /// blocks (the disk.virtio `write_bytes` shape).
    async fn write_bytes(&mut self, offset: u64, bytes: &[u8]) -> Result<(), DiskFail> {
        let len = bytes.len() as u64;
        self.check_range(offset, len)?;
        let block_size = u64::from(self.block_size);
        let mut cursor = offset;
        let mut written: u64 = 0;
        while written < len {
            let first_block = cursor / block_size;
            let within = cursor - first_block * block_size;
            let take = core::cmp::min(len - written, DATA_CAP - within);
            let end_within = within + take;
            let blocks = end_within.div_ceil(block_size) as u16;
            let aligned = within == 0 && end_within.is_multiple_of(block_size);
            let span = u64::from(blocks) * block_size;
            let chunk: Vec<u8> = if aligned {
                bytes[written as usize..(written + take) as usize].to_vec()
            } else {
                // Read the covering blocks, overlay the new bytes, write the span
                // back.
                let mut current = self
                    .bot
                    .read10(first_block as u32, blocks, self.block_size)
                    .await
                    .map_err(|err| DiskFail::Io(msd_failure("READ(10) (RMW)", err)))?;
                if current.len() < span as usize {
                    return Err(DiskFail::Io(String::from(
                        "usb.msd: short read during read-modify-write",
                    )));
                }
                current[within as usize..end_within as usize]
                    .copy_from_slice(&bytes[written as usize..(written + take) as usize]);
                current
            };
            self.bot
                .write10(first_block as u32, blocks, self.block_size, &chunk)
                .await
                .map_err(|err| DiskFail::Io(msd_failure("WRITE(10)", err)))?;
            cursor += take;
            written += take;
        }
        Ok(())
    }

    /// The disk-contract range rule: the whole range must lie within the device,
    /// and a zero-length access at any offset up to the capacity succeeds. LBAs are
    /// bounds-checked here, from READ CAPACITY's geometry — a past-capacity access
    /// refuses typed before any bytes move on the bus.
    fn check_range(&self, offset: u64, len: u64) -> Result<(), DiskFail> {
        let end = offset.checked_add(len).ok_or(DiskFail::OutOfRange)?;
        if end > self.capacity_bytes {
            return Err(DiskFail::OutOfRange);
        }
        Ok(())
    }
}

// ------------------------------------------------------------------------------------------
// The exported eo9:disk provider
// ------------------------------------------------------------------------------------------

/// The `usb.msd` provider.
struct Stub;

/// The root-handle resource: a token referring to the enumerated device.
struct MsdDisk;

impl types::Guest for Stub {
    type DiskImpl = MsdDisk;
}

impl types::GuestDiskImpl for MsdDisk {}

impl disk::Guest for Stub {
    fn default() -> types::DiskImpl {
        types::DiskImpl::new(MsdDisk)
    }

    fn size(_dev: disk::DiskImplBorrow<'_>) -> u64 {
        // `size` is a synchronous query and bring-up awaits, so it reports the
        // capacity only once the device is up (any awaited operation brings it up).
        // Before that it reports 0 rather than trapping; consumers issue one read to
        // wake the device (and surface its real, typed error), then ask again — the
        // fs.eofs convention.
        STATE.peek_capacity().unwrap_or(0)
    }

    async fn flush(_dev: disk::DiskImplBorrow<'_>) -> Result<(), WriteError> {
        // Documented no-op (plan §9 workaround 2): BOT over the six-command set has
        // no cache-control verb (SYNCHRONIZE CACHE is outside it), and sticks do not
        // cache like SD FTLs — durability is the underlying device's, and the
        // flasher's read-back-verify is the durability check that matters. Flushing
        // an un-brought-up device succeeds without waking it (nothing was written).
        Ok(())
    }

    async fn read(
        _dev: disk::DiskImplBorrow<'_>,
        offset: u64,
        dst: Buffer,
    ) -> (Buffer, Result<ReadResult, ReadError>) {
        let len = dst.len();
        let outcome = with_driver(async |driver| driver.read_bytes(offset, len).await).await;
        match outcome {
            Ok(bytes) => {
                if !bytes.is_empty() {
                    dst.write(0, &bytes);
                }
                (dst, Ok(ReadResult { bytes_read: len }))
            }
            Err(DiskFail::OutOfRange) => (dst, Err(ReadError::OutOfRange)),
            Err(DiskFail::Io(message)) => (dst, Err(ReadError::Io(message))),
        }
    }

    async fn write(
        _dev: disk::DiskImplBorrow<'_>,
        offset: u64,
        src: Buffer,
    ) -> (Buffer, Result<WriteResult, WriteError>) {
        let len = src.len();
        // Copy out of the buffer before driving the device so no buffer call
        // interleaves with the command (the disk.mem / disk.virtio discipline).
        let bytes = if len == 0 {
            Vec::new()
        } else {
            src.read(0, len)
        };
        let outcome = with_driver(async |driver| driver.write_bytes(offset, &bytes).await).await;
        match outcome {
            Ok(()) => (src, Ok(WriteResult { bytes_written: len })),
            Err(DiskFail::OutOfRange) => (src, Err(WriteError::OutOfRange)),
            Err(DiskFail::Io(message)) => (src, Err(WriteError::Io(message))),
        }
    }
}

export!(Stub);
