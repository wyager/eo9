//! `net.rtl8125` — an RTL8125 2.5GbE driver as an ordinary wasm component.
//!
//! Targets the crate-local `eo9:net-rtl8125/rtl8125-net` world: imports the PCI
//! capability (`eo9:pci/pci`) plus `eo9:text/text` for one diagnostic line, and exports
//! `eo9:net/l2` (interfaces, MAC addresses, whole Ethernet frames) backed by a Realtek
//! RTL8125 PCIe NIC — the silicon on the Orange Pi 5 Plus's two 2.5GbE ports
//! (10ec:8125 behind the RK3588 DW root ports; kernel rk3588_pcie module docs). The
//! driver holds no policy of its own: which functions it can see (and therefore claim)
//! is entirely the PCI provider's business — the kernel root only when the boot
//! granted `pci`, an attenuating `pci.filtered` for "exactly this one port" grants
//! (`pci.admit-address`; both NICs share the 10ec:8125 identity), `pci.deny` to refuse.
//!
//! `net.virtio` is the structural template — every discipline carries over: the
//! take/put driver slot, the bring-up claim guard (plan/09 D41), drain-before-reuse on
//! the transmit ring (D34), bounded everything, typed errors never traps, and the
//! short-poll "nothing waiting" receive contract (study 08 F2). What changes is the
//! device conversation, which follows the mainline Linux r8169 driver's RTL8125
//! support — the legacy 16-byte descriptor rings, not the vendor driver's 32-byte v3
//! format — with every register and bit cited in `crates/eo9-rtl8125` (the pure,
//! host-tested device core this component is a thin I/O shell over).
//!
//! Shape of the device conversation (citations in `eo9-rtl8125`):
//!
//! * **Probe.** Enumerate the capability's view of the bus, claim the first
//!   10ec:8125 function, open the MAC register BAR (BAR 2 on every modern Realtek
//!   part; first memory BAR as fallback), read the factory MAC from `MAC0`.
//! * **Bring-up.** Soft reset (`ChipCmd.RST`, bounded wait), interrupts masked and
//!   acknowledged once (`IMR_8125 = 0`, `ISR_8125 = ~0` — the polled driver's ISR
//!   suppression discipline: with the mask clear the NIC never asserts the INTx line
//!   that is unwired on the board anyway), PHY autoneg via the `GPHY_OCP` MDIO window
//!   (advertise 10/100 + 1000FD + 2500FD, restart, bounded link wait — a link that is
//!   still negotiating leaves the interface typed-down, not an error), descriptor
//!   rings (32 receive + 32 transmit slots) in `alloc-dma` memory, `RxMaxSize`,
//!   `TxConfig`/`RxConfig`, bus mastering, receiver + transmitter on.
//! * **I/O.** Transmit copies the frame into a bounce buffer (zero-padded to the
//!   60-byte Ethernet minimum), publishes one OWN'd descriptor, rings the
//!   `TxPoll_8125` doorbell, and polls OWN-clear with the bounded-poll discipline.
//!   Receive polls the next ring slot's OWN bit briefly, decodes the completion
//!   (whole-frame + error summary + length, CRC stripped), copies the frame out, and
//!   re-posts the slot. INTx is deliberately unused: on the board the DW controllers
//!   mux all four pins on one SPI per controller and the demux is not wired
//!   (`arch::pci_intx::WIRED = false`); the provider would answer `unsupported`, and
//!   polled completion with honest bounds is the v1 contract (the interrupt
//!   conversion is recorded alongside plan/12 D59's virtio sibling).
//!
//! The exported `eo9:net/l2` surface is the single interface `rtl0`: `recv-frame`
//! that finds nothing within its short poll window reports an empty result
//! (`bytes-received: 0`) so the consumer owns the wait policy; `send-frame` on a
//! link that has not come up is the typed `link-down`, and device weirdness is always
//! a typed `io` error, never a trap. Multicast acceptance is OFF for v1 (recorded:
//! IPv4/ARP need broadcast + unicast only); promiscuous mode is OFF and has no knob.
//!
//! QEMU cannot emulate an RTL8125 (it stops at rtl8139), so emulated boots exercise
//! exactly two things: composition/instantiation, and the typed refusal naming what
//! was probed (`no RTL8125 (10ec:8125) function is visible…`). Everything beyond the
//! probe is board-validated; the host-testable parts (descriptor and PHY-word
//! encodings) live in `eo9-rtl8125` where `cargo test` pins them.

#![no_std]

extern crate alloc;

use alloc::format;
use alloc::string::String;
use alloc::vec;
use alloc::vec::Vec;

use eo9_guest::provider::ProviderState;
use eo9_rtl8125::{bits, phy, reg};

wit_bindgen::generate!({
    world: "rtl8125-net",
    path: "wit",
    // Pull in bindings for eo9:pci/types and eo9:io/buffers, which the imported and
    // exported interfaces use but the world does not name directly.
    generate_all,
});

use eo9::pci::pci;
use eo9::text::text;
use exports::eo9::net::l2::{self, Buffer, InterfaceInfo, L2Error, RecvResult, SendResult};

// ------------------------------------------------------------------------------------------
// Sizing and bounds
// ------------------------------------------------------------------------------------------

/// Ring sizes: 32 receive and 32 transmit descriptors (modest by design — one frame
/// in flight per `send-frame`, and 32 × 2 KiB of receive buffering absorbs a burst
/// while the consumer pumps).
const RX_SLOTS: u16 = 32;
const TX_SLOTS: u16 = 32;

/// One DMA page holds both rings: transmit at 0, receive at 2048 (32 × 16 = 512 bytes
/// each; the page-aligned `alloc-dma` base satisfies the chip's 256-byte ring
/// alignment, `eo9_rtl8125::RING_ALIGN`).
const TX_RING_BASE: u64 = 0;
const RX_RING_BASE: u64 = 2048;
const RING_BYTES: u64 = 4096;

/// Receive buffers: 32 slots of 2 KiB (a 1514-byte frame + CRC fits with room).
/// `RxMaxSize` is programmed to the slot size, so a frame can never span slots.
const RX_SLOT_BYTES: u64 = 2048;
const RX_DATA_BYTES: u64 = RX_SLOT_BYTES * RX_SLOTS as u64;
/// Transmit bounce buffer: one frame at a time.
const TX_DATA_BYTES: u64 = 2048;
/// Largest frame `send-frame` accepts.
const MAX_FRAME: u64 = TX_DATA_BYTES;
/// The MTU reported for the interface (classic Ethernet payload size).
const MTU: u32 = 1500;
/// The single interface name this driver exposes.
const INTERFACE_NAME: &str = "rtl0";

/// Soft-reset polling bound (each iteration is a host call): the chip clears
/// `ChipCmd.RST` in well under a millisecond (r8169 waits 100 × 100 µs); hitting
/// this bound means the device is wedged or absent.
const RESET_POLL_LIMIT: u64 = 100_000;
/// GPHY OCP completion bound: one MDIO transaction is tens of microseconds (r8169
/// polls 25 µs × 10); generous because each poll here is a whole host call.
const PHY_POLL_LIMIT: u64 = 100_000;
/// Link wait at bring-up, in PHYstatus reads. Autonegotiation to 2.5G takes one to a
/// few seconds of real time; at roughly a microsecond per host call this bound spends
/// a few seconds waiting, then gives up WITHOUT failing bring-up — the interface
/// reports `up: false` (typed), `send-frame` answers `link-down`, and the next
/// operation re-reads the live status, so a late negotiation is picked up.
const LINK_WAIT_LIMIT: u64 = 2_000_000;
/// Transmit-completion polling bound (net.virtio parity): the NIC consumes a
/// published descriptor in microseconds once kicked, so hitting this means it is
/// wedged — a typed error, never a hang.
const TX_POLL_LIMIT: u64 = 50_000_000;
/// Receive polling bound: how many descriptor reads `recv-frame` spends checking for
/// a delivered frame before reporting "nothing waiting" (an empty result, not an
/// error) — net.virtio's study-08-F2 calibration, a couple of milliseconds of host
/// calls, carried over unchanged so consumers see the same poll economics on both
/// drivers.
const RX_POLL_LIMIT: u64 = 2_000;

// ------------------------------------------------------------------------------------------
// Awaited driving of the async pci imports.
// ------------------------------------------------------------------------------------------

/// Run one PCI operation to completion and flatten its result, labelling failures
/// with `what`. The await is genuine (the SPEC's "boundaries are honestly async"
/// rule): the kernel root resolves within the call — pci operations are plain
/// MMIO/memory work — but an interposed middleware that suspends just parks this
/// driver's activation, and the consumer above absorbs that by awaiting its own l2
/// call.
async fn pci_call<T>(
    what: &str,
    future: impl Future<Output = Result<T, pci::PciError>>,
) -> Result<T, String> {
    match future.await {
        Ok(value) => Ok(value),
        Err(error) => Err(format!("{what}: {error:?}")),
    }
}

// ------------------------------------------------------------------------------------------
// Driver state
// ------------------------------------------------------------------------------------------

/// Link state as last observed in `PHYstatus` (refreshed on every operation that
/// already holds the device; `info` — a sync WIT function — reports the cached view).
#[derive(Clone, Copy, PartialEq, Eq)]
enum Link {
    Down,
    /// Megabits per second as the PHY resolved it (10 / 100 / 1000 / 2500).
    Up(u32),
}

/// The brought-up device: claimed function, the MAC register BAR, the ring and data
/// DMA buffers, the ring cursors, and identity.
struct Driver {
    /// Keeps the exclusive claim on the function alive for the component's lifetime.
    _device: pci::Device,
    /// The MAC register block (BAR 2, `eo9_rtl8125::MMIO_BAR_INDEX`).
    mmio: pci::Bar,
    rings: pci::DmaBuffer,
    rx_data: pci::DmaBuffer,
    tx_data: pci::DmaBuffer,
    /// Transmit cursors: descriptors published vs completions consumed
    /// (free-running; their difference is the in-flight count, 0 or 1 between
    /// healthy operations — the D34 drain invariant made visible).
    tx_published: u16,
    tx_consumed: u16,
    /// Next receive ring slot to inspect (free-running).
    rx_cursor: u16,
    mac: [u8; 6],
    link: Link,
}

/// Failures of the link-layer operations, mapped to the WIT error variants by the
/// export glue.
enum L2Fail {
    FrameTooLarge,
    LinkDown,
    Io(String),
}

impl From<L2Fail> for L2Error {
    fn from(fail: L2Fail) -> L2Error {
        match fail {
            L2Fail::FrameTooLarge => L2Error::FrameTooLarge,
            L2Fail::LinkDown => L2Error::LinkDown,
            L2Fail::Io(message) => L2Error::Io(message),
        }
    }
}

/// The driver's home between operations: an operation takes the driver *out* of the
/// slot for its duration (a `ProviderState` borrow must never be held across an
/// await), exactly as net.virtio does.
struct DriverSlot {
    driver: Option<Driver>,
    /// Whether bring-up has been claimed (set before the first `bring_up().await` so
    /// a concurrent first use cannot start a second probe; cleared again if bring-up
    /// fails, so the next use retries — plan/09 D41).
    brought_up: bool,
}

static STATE: ProviderState<DriverSlot> = ProviderState::new();

/// Puts the driver back in its slot when the operation that took it finishes —
/// including by cancellation (the operation's future dropped mid-await), so a
/// cancelled operation can never leave the slot empty. A transmit a cancelled
/// operation left published is settled by the next send's [`Driver::drain_tx`]
/// before any shared state is reused; the guard itself stays synchronous and free of
/// device access (`Drop` cannot await).
struct DriverGuard(Option<Driver>);

impl Drop for DriverGuard {
    fn drop(&mut self) {
        if let Some(driver) = self.0.take() {
            STATE.with(|slot| slot.driver = Some(driver));
        }
    }
}

impl core::ops::Deref for DriverGuard {
    type Target = Driver;
    fn deref(&self) -> &Driver {
        self.0
            .as_ref()
            .expect("the driver is held for the guard's lifetime")
    }
}

impl core::ops::DerefMut for DriverGuard {
    fn deref_mut(&mut self) -> &mut Driver {
        self.0
            .as_mut()
            .expect("the driver is held for the guard's lifetime")
    }
}

/// What `acquire_driver` found in the slot.
enum SlotView {
    Ready(Driver),
    Busy,
    NeedBringUp,
}

/// Take the driver for one operation, probing and initializing the device on first
/// use (the documented default state — there is no configure interface). A second
/// activation arriving while one is parked mid-operation gets a typed error, never a
/// re-entrant borrow trap.
async fn acquire_driver() -> Result<DriverGuard, L2Fail> {
    if !STATE.is_set() {
        STATE.set(DriverSlot {
            driver: None,
            brought_up: false,
        });
    }
    let view = STATE.with(|slot| {
        if let Some(driver) = slot.driver.take() {
            SlotView::Ready(driver)
        } else if slot.brought_up {
            SlotView::Busy
        } else {
            slot.brought_up = true;
            SlotView::NeedBringUp
        }
    });
    match view {
        SlotView::Ready(driver) => Ok(DriverGuard(Some(driver))),
        SlotView::Busy => Err(L2Fail::Io(String::from(
            "net.rtl8125: another operation on this device is in progress",
        ))),
        SlotView::NeedBringUp => {
            // Arm the restore before the first await of bring-up, so an error return
            // *or a future dropped mid-bring-up* clears the claim and the next use
            // retries (instead of wedging the instance behind the typed busy answer).
            let claim = BringUpClaim { armed: true };
            let driver = Driver::bring_up().await.map_err(L2Fail::Io)?;
            claim.defuse();
            Ok(DriverGuard(Some(driver)))
        }
    }
}

/// Releases the bring-up claim (`brought_up`) if bring-up never completes; armed from
/// the instant the claim exists, defused on success when the [`DriverGuard`] takes
/// over (plan/09 D41).
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
            STATE.with(|slot| slot.brought_up = false);
        }
    }
}

impl Driver {
    /// Find, claim, and bring up the first RTL8125 function visible through the
    /// granted PCI capability. Every step reports a typed, labelled error — device
    /// weirdness is an `io` failure of the l2 operation, never a trap.
    async fn bring_up() -> Result<Driver, String> {
        let root = pci::default();
        let devices = pci_call("net.rtl8125: enumerate", pci::enumerate(&root)).await?;
        let target = devices
            .iter()
            .find(|d| {
                d.vendor_id == eo9_rtl8125::PCI_VENDOR_REALTEK
                    && d.device_id == eo9_rtl8125::PCI_DEVICE_RTL8125
            })
            .ok_or_else(|| {
                String::from(
                    "net.rtl8125: no RTL8125 (10ec:8125) function is visible through the \
                     granted pci capability (QEMU has no RTL8125 model — this driver's \
                     device exists on the Orange Pi 5 Plus board; under QEMU compose \
                     net.virtio instead)",
                )
            })?;
        let address = target.address;
        let device = pci_call("net.rtl8125: open", pci::open(&root, address)).await?;

        // The MAC register block: BAR 2 on every modern Realtek part (cited in
        // eo9-rtl8125); fall back to the first memory BAR so a quirky setup still
        // probes (and a setup with no memory BAR at all refuses, typed).
        let bars = pci_call("net.rtl8125: bars", pci::bars(&device)).await?;
        let mmio_index = bars
            .iter()
            .find(|b| b.index == eo9_rtl8125::MMIO_BAR_INDEX && b.kind == pci::BarKind::Memory)
            .or_else(|| bars.iter().find(|b| b.kind == pci::BarKind::Memory))
            .map(|b| b.index)
            .ok_or_else(|| {
                String::from("net.rtl8125: the function exposes no memory BAR for its registers")
            })?;
        let mmio = pci_call("net.rtl8125: open-bar", pci::open_bar(&device, mmio_index)).await?;

        // DMA: one page for both rings, the receive slots, the transmit bounce buffer.
        let rings = pci_call(
            "net.rtl8125: alloc-dma (rings)",
            pci::alloc_dma(&device, RING_BYTES),
        )
        .await?;
        let rx_data = pci_call(
            "net.rtl8125: alloc-dma (receive buffers)",
            pci::alloc_dma(&device, RX_DATA_BYTES),
        )
        .await?;
        let tx_data = pci_call(
            "net.rtl8125: alloc-dma (transmit buffer)",
            pci::alloc_dma(&device, TX_DATA_BYTES),
        )
        .await?;
        // The chip requires 256-byte ring alignment (eo9_rtl8125::RING_ALIGN); the
        // provider documents page alignment, but a different provider may not —
        // check, typed.
        let ring_address = pci::dma_address(&rings);
        if !ring_address.is_multiple_of(eo9_rtl8125::RING_ALIGN) {
            return Err(format!(
                "net.rtl8125: the DMA ring buffer is not {}-byte aligned (got {ring_address:#x})",
                eo9_rtl8125::RING_ALIGN
            ));
        }

        let mut driver = Driver {
            _device: device,
            mmio,
            rings,
            rx_data,
            tx_data,
            tx_published: 0,
            tx_consumed: 0,
            rx_cursor: 0,
            mac: [0; 6],
            link: Link::Down,
        };
        driver.start().await?;
        Ok(driver)
    }

    /// The device side of bring-up, once the function is claimed and the DMA buffers
    /// exist. Order follows the r8169 hw-start path (citations in eo9-rtl8125); the
    /// reference drivers' chip-quirk tables (MAC OCP errata pokes, MCU patches, EEE
    /// tuning) are deliberately omitted from v1 — recorded as the first suspect if
    /// board traffic misbehaves.
    async fn start(&mut self) -> Result<(), String> {
        // The factory MAC, before reset (reset preserves it; reading first means the
        // diagnostic line can name the port even if a later step fails).
        let mut mac = [0u8; 6];
        for (index, byte) in mac.iter_mut().enumerate() {
            *byte = self
                .read(reg::MAC0 + index as u64, pci::AccessWidth::Byte)
                .await? as u8;
        }
        self.mac = mac;

        // Soft reset: ChipCmd.RST, bounded wait for the chip to clear it.
        self.write(reg::CHIP_CMD, pci::AccessWidth::Byte, bits::CMD_RESET)
            .await?;
        let mut spins: u64 = 0;
        while self.read(reg::CHIP_CMD, pci::AccessWidth::Byte).await? & bits::CMD_RESET != 0 {
            spins += 1;
            if spins > RESET_POLL_LIMIT {
                return Err(String::from(
                    "net.rtl8125: the chip did not come out of soft reset (poll limit)",
                ));
            }
        }

        // Interrupts: mask everything, acknowledge anything pending, and leave the
        // mask clear for the driver's lifetime — the polled driver's suppression
        // discipline (no source ever asserts the INTx line, which is unwired on the
        // board; see the module docs).
        self.write(reg::INTR_MASK_8125, pci::AccessWidth::Dword, 0)
            .await?;
        self.write(reg::INTR_STATUS_8125, pci::AccessWidth::Dword, 0xffff_ffff)
            .await?;

        // PHY: advertise 10/100 (ANAR), 1000FD (GBCR), 2500FD (the Realtek vendor
        // register), then restart autonegotiation.
        let adv_2500 = self.phy_ocp_read(phy::OCP_ADV_2500).await?;
        self.phy_ocp_write(phy::OCP_ADV_2500, adv_2500 | phy::ADV_2500_FULL)
            .await?;
        self.phy_write(phy::MII_ANAR, phy::ANAR_ADVERTISE_10_100)
            .await?;
        self.phy_write(phy::MII_GBCR, phy::GBCR_ADVERTISE_1000_FULL)
            .await?;
        self.phy_write(phy::MII_BMCR, phy::BMCR_START_AUTONEG)
            .await?;

        // Receive limits and DMA configuration.
        self.write(reg::RX_MAX_SIZE, pci::AccessWidth::Word, RX_SLOT_BYTES)
            .await?;
        self.write(
            reg::TX_CONFIG,
            pci::AccessWidth::Dword,
            bits::TX_CONFIG_VALUE,
        )
        .await?;

        // Multicast filter: both MAR dwords 0 — multicast acceptance is off (v1:
        // none), this just makes the filter state deterministic.
        self.write(reg::MAR0, pci::AccessWidth::Dword, 0).await?;
        self.write(reg::MAR0 + 4, pci::AccessWidth::Dword, 0)
            .await?;

        // Rings: every receive slot posted (OWN, RingEnd on the last), the transmit
        // ring zeroed (no descriptor owned).
        pci::dma_write(&self.rings, TX_RING_BASE, &[0u8; 512]);
        let rx_buffer_base = pci::dma_address(&self.rx_data);
        for slot in 0..RX_SLOTS {
            let descriptor = eo9_rtl8125::encode_rx_descriptor(
                rx_buffer_base + u64::from(slot) * RX_SLOT_BYTES,
                RX_SLOT_BYTES as u16,
                slot == RX_SLOTS - 1,
            );
            pci::dma_write(
                &self.rings,
                RX_RING_BASE + eo9_rtl8125::descriptor_offset(slot),
                &descriptor,
            );
        }
        let ring_address = pci::dma_address(&self.rings);
        self.write_ring_address(reg::TX_DESC_ADDR_LOW, ring_address + TX_RING_BASE)
            .await?;
        self.write_ring_address(reg::RX_DESC_ADDR_LOW, ring_address + RX_RING_BASE)
            .await?;

        // The device DMAs the rings and buffers from here on.
        pci_call(
            "net.rtl8125: set-bus-master",
            pci::set_bus_master(&self._device, true),
        )
        .await?;

        // Receiver + transmitter on, then the receive configuration (accept
        // broadcast + our station address; promiscuous off, multicast none).
        self.write(
            reg::CHIP_CMD,
            pci::AccessWidth::Byte,
            bits::CMD_RX_ENABLE | bits::CMD_TX_ENABLE,
        )
        .await?;
        self.write(
            reg::RX_CONFIG,
            pci::AccessWidth::Dword,
            bits::RX_CONFIG_BASE | bits::RX_ACCEPT_BROADCAST | bits::RX_ACCEPT_MY_PHYS,
        )
        .await?;

        // Bounded link wait: autoneg to 2.5G takes seconds; an expired bound is NOT
        // an error — the link stays typed-down and later operations re-read it.
        let mut spins: u64 = 0;
        loop {
            self.refresh_link().await?;
            if self.link != Link::Down {
                break;
            }
            spins += 1;
            if spins > LINK_WAIT_LIMIT {
                break;
            }
        }

        // One best-effort diagnostic line so a metal session shows what was probed.
        let handle = text::default();
        let line = match self.link {
            Link::Up(mbps) => format!(
                "net.rtl8125: {} link up {mbps} Mb/s, rings rx/tx {RX_SLOTS}/{TX_SLOTS}\n",
                format_mac(&self.mac)
            ),
            Link::Down => format!(
                "net.rtl8125: {} link DOWN after the bounded autoneg wait (cable? \
                 negotiation still running? operations re-check)\n",
                format_mac(&self.mac)
            ),
        };
        let _ = text::write(&handle, text::OutputStream::Out, &line);
        Ok(())
    }

    // --- register access helpers ----------------------------------------------------------

    async fn read(&self, register: u64, width: pci::AccessWidth) -> Result<u64, String> {
        pci_call(
            "net.rtl8125: register read",
            pci::bar_read(&self.mmio, register, width),
        )
        .await
    }

    async fn write(
        &self,
        register: u64,
        width: pci::AccessWidth,
        value: u64,
    ) -> Result<(), String> {
        pci_call(
            "net.rtl8125: register write",
            pci::bar_write(&self.mmio, register, width, value),
        )
        .await
    }

    /// Write a 64-bit ring base as the two dword halves (`*_ADDR_LOW` then `+4`).
    async fn write_ring_address(&self, low_register: u64, address: u64) -> Result<(), String> {
        self.write(low_register, pci::AccessWidth::Dword, address & 0xffff_ffff)
            .await?;
        self.write(low_register + 4, pci::AccessWidth::Dword, address >> 32)
            .await
    }

    // --- PHY access (GPHY_OCP, the MAC's MDIO window) ---------------------------------------

    async fn phy_ocp_write(&self, ocp_address: u16, value: u16) -> Result<(), String> {
        self.write(
            reg::GPHY_OCP,
            pci::AccessWidth::Dword,
            u64::from(eo9_rtl8125::gphy_write_command(ocp_address, value)),
        )
        .await?;
        let mut spins: u64 = 0;
        loop {
            let readback = self.read(reg::GPHY_OCP, pci::AccessWidth::Dword).await? as u32;
            if eo9_rtl8125::gphy_write_done(readback) {
                return Ok(());
            }
            spins += 1;
            if spins > PHY_POLL_LIMIT {
                return Err(format!(
                    "net.rtl8125: PHY write to OCP {ocp_address:#06x} never completed (poll limit)"
                ));
            }
        }
    }

    async fn phy_ocp_read(&self, ocp_address: u16) -> Result<u16, String> {
        self.write(
            reg::GPHY_OCP,
            pci::AccessWidth::Dword,
            u64::from(eo9_rtl8125::gphy_read_command(ocp_address)),
        )
        .await?;
        let mut spins: u64 = 0;
        loop {
            let readback = self.read(reg::GPHY_OCP, pci::AccessWidth::Dword).await? as u32;
            if let Some(value) = eo9_rtl8125::gphy_read_result(readback) {
                return Ok(value);
            }
            spins += 1;
            if spins > PHY_POLL_LIMIT {
                return Err(format!(
                    "net.rtl8125: PHY read from OCP {ocp_address:#06x} never completed (poll limit)"
                ));
            }
        }
    }

    /// Write a standard MII register of the integrated PHY.
    async fn phy_write(&self, mii_register: u8, value: u16) -> Result<(), String> {
        self.phy_ocp_write(eo9_rtl8125::phy_ocp_address(mii_register), value)
            .await
    }

    /// Re-read `PHYstatus` and cache the link state (link bit + resolved speed).
    async fn refresh_link(&mut self) -> Result<(), String> {
        let status = self.read(reg::PHY_STATUS, pci::AccessWidth::Word).await?;
        self.link = if status & bits::PHY_STATUS_LINK == 0 {
            Link::Down
        } else if status & bits::PHY_STATUS_2500M_FULL != 0 {
            Link::Up(2500)
        } else if status & bits::PHY_STATUS_1000M_FULL != 0 {
            Link::Up(1000)
        } else if status & bits::PHY_STATUS_100M != 0 {
            Link::Up(100)
        } else if status & bits::PHY_STATUS_10M != 0 {
            Link::Up(10)
        } else {
            // Link bit set but no speed resolved yet: still negotiating.
            Link::Down
        };
        Ok(())
    }

    // --- frames ------------------------------------------------------------------------------

    /// The single interface this driver exposes (the cached link view; async
    /// operations refresh it first).
    fn interface_info(&self) -> InterfaceInfo {
        InterfaceInfo {
            name: String::from(INTERFACE_NAME),
            mac: (
                self.mac[0],
                self.mac[1],
                self.mac[2],
                self.mac[3],
                self.mac[4],
                self.mac[5],
            ),
            mtu: MTU,
            up: self.link != Link::Down,
        }
    }

    /// The opts1 dword of transmit descriptor `index`.
    fn tx_opts1(&self, index: u16) -> u32 {
        let raw = pci::dma_read(
            &self.rings,
            TX_RING_BASE + eo9_rtl8125::descriptor_offset(index),
            4,
        );
        eo9_rtl8125::decode_opts1([raw[0], raw[1], raw[2], raw[3]])
    }

    /// The opts1 dword of receive descriptor `index`.
    fn rx_opts1(&self, index: u16) -> u32 {
        let raw = pci::dma_read(
            &self.rings,
            RX_RING_BASE + eo9_rtl8125::descriptor_offset(index),
            4,
        );
        eo9_rtl8125::decode_opts1([raw[0], raw[1], raw[2], raw[3]])
    }

    /// Settle a transmit a *cancelled* `send-frame` left published before the bounce
    /// buffer and descriptor slot are reused (the plan/09 D34 drain-before-reuse
    /// invariant, net.virtio's `drain_tx` in this device's dialect). A cancellation
    /// can land in `send`'s doorbell await: the descriptor is published — possibly
    /// unkicked — and the NIC may transmit it at the next doorbell, reading whatever
    /// the bounce buffer holds *then*. So: re-kick (idempotent), poll the leftover
    /// descriptor's OWN bit out with the normal transmit bound, account it.
    async fn drain_tx(&mut self) -> Result<(), L2Fail> {
        let mut spins: u64 = 0;
        while self.tx_consumed != self.tx_published {
            let slot = self.tx_consumed % TX_SLOTS;
            self.write(
                reg::TX_POLL_8125,
                pci::AccessWidth::Word,
                bits::TX_POLL_QUEUE0,
            )
            .await
            .map_err(L2Fail::Io)?;
            while eo9_rtl8125::owned_by_nic(self.tx_opts1(slot)) {
                spins += 1;
                if spins > TX_POLL_LIMIT {
                    return Err(L2Fail::Io(String::from(
                        "net.rtl8125: the chip did not complete a cancelled transmit \
                         (poll limit)",
                    )));
                }
            }
            self.tx_consumed = self.tx_consumed.wrapping_add(1);
        }
        Ok(())
    }

    /// Transmit one Ethernet frame: bounce copy (zero-padded to the 60-byte
    /// minimum), one OWN'd descriptor, doorbell, bounded OWN-clear poll.
    async fn send(&mut self, frame: &[u8]) -> Result<u64, L2Fail> {
        let frame_len = frame.len() as u64;
        if frame_len > MAX_FRAME {
            return Err(L2Fail::FrameTooLarge);
        }
        self.refresh_link().await.map_err(L2Fail::Io)?;
        if self.link == Link::Down {
            return Err(L2Fail::LinkDown);
        }
        self.drain_tx().await?;

        let padded_len = (frame.len() as u16).max(eo9_rtl8125::MIN_FRAME_LEN);
        let mut packet = vec![0u8; usize::from(padded_len)];
        packet[..frame.len()].copy_from_slice(frame);
        pci::dma_write(&self.tx_data, 0, &packet);

        let slot = self.tx_published % TX_SLOTS;
        let descriptor = eo9_rtl8125::encode_tx_descriptor(
            pci::dma_address(&self.tx_data),
            padded_len,
            slot == TX_SLOTS - 1,
        );
        pci::dma_write(
            &self.rings,
            TX_RING_BASE + eo9_rtl8125::descriptor_offset(slot),
            &descriptor,
        );
        self.tx_published = self.tx_published.wrapping_add(1);
        self.write(
            reg::TX_POLL_8125,
            pci::AccessWidth::Word,
            bits::TX_POLL_QUEUE0,
        )
        .await
        .map_err(L2Fail::Io)?;

        let mut spins: u64 = 0;
        while eo9_rtl8125::owned_by_nic(self.tx_opts1(slot)) {
            spins += 1;
            if spins > TX_POLL_LIMIT {
                return Err(L2Fail::Io(String::from(
                    "net.rtl8125: the chip did not consume the transmitted frame (poll limit)",
                )));
            }
        }
        self.tx_consumed = self.tx_consumed.wrapping_add(1);
        Ok(frame_len)
    }

    /// Receive the next delivered frame (CRC stripped), truncated to `max_len`
    /// bytes, re-posting the ring slot afterwards. A short poll that finds nothing
    /// returns an empty frame ("nothing waiting right now") so the consumer decides
    /// how long to keep waiting; error summaries, fragments, and runts also come
    /// back empty (wire noise, not driver failures). The consumption path is
    /// await-free between the completion read and the re-post (all synchronous DMA
    /// accesses), so a cancellation cannot strand a half-consumed slot — and unlike
    /// virtio there is no receive doorbell to lose: the NIC re-fetches the OWN'd
    /// descriptor on its own.
    async fn recv(&mut self, max_len: u64) -> Result<Vec<u8>, L2Fail> {
        let slot = self.rx_cursor % RX_SLOTS;
        let mut spins: u64 = 0;
        let mut opts1 = self.rx_opts1(slot);
        while eo9_rtl8125::owned_by_nic(opts1) {
            spins += 1;
            if spins > RX_POLL_LIMIT {
                return Ok(Vec::new());
            }
            opts1 = self.rx_opts1(slot);
        }

        let completion = eo9_rtl8125::decode_rx_completion(opts1);
        let bytes = match eo9_rtl8125::rx_payload_len(&completion) {
            Some(payload_len) => {
                let copy_len = u64::from(payload_len).min(max_len).min(RX_SLOT_BYTES);
                pci::dma_read(&self.rx_data, u64::from(slot) * RX_SLOT_BYTES, copy_len)
            }
            // Error summary / fragment / runt: drop as wire noise, report "nothing".
            None => Vec::new(),
        };

        // Hand the slot straight back to the NIC and advance.
        let descriptor = eo9_rtl8125::encode_rx_descriptor(
            pci::dma_address(&self.rx_data) + u64::from(slot) * RX_SLOT_BYTES,
            RX_SLOT_BYTES as u16,
            slot == RX_SLOTS - 1,
        );
        pci::dma_write(
            &self.rings,
            RX_RING_BASE + eo9_rtl8125::descriptor_offset(slot),
            &descriptor,
        );
        self.rx_cursor = self.rx_cursor.wrapping_add(1);
        Ok(bytes)
    }
}

/// `aa:bb:cc:dd:ee:ff`.
fn format_mac(mac: &[u8; 6]) -> String {
    format!(
        "{:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x}",
        mac[0], mac[1], mac[2], mac[3], mac[4], mac[5]
    )
}

// ------------------------------------------------------------------------------------------
// The exported eo9:net/l2 provider
// ------------------------------------------------------------------------------------------

/// The `net.rtl8125` provider.
struct Stub;

/// The root-handle resource: a token referring to the claimed and brought-up device.
struct RtlL2;

/// The opened-interface resource: a token — the device state lives in [`STATE`].
struct RtlInterface;

impl l2::GuestL2Impl for RtlL2 {}
impl l2::GuestL2Interface for RtlInterface {}

impl l2::Guest for Stub {
    type L2Impl = RtlL2;
    type L2Interface = RtlInterface;

    fn default() -> l2::L2Impl {
        l2::L2Impl::new(RtlL2)
    }

    async fn list_interfaces(_l2: l2::L2ImplBorrow<'_>) -> Result<Vec<InterfaceInfo>, L2Error> {
        match acquire_driver().await {
            Ok(mut driver) => {
                // A live view: a link that came up since bring-up shows up here.
                driver
                    .refresh_link()
                    .await
                    .map_err(|e| L2Error::from(L2Fail::Io(e)))?;
                Ok(alloc::vec![driver.interface_info()])
            }
            Err(fail) => Err(L2Error::from(fail)),
        }
    }

    async fn open_interface(
        _l2: l2::L2ImplBorrow<'_>,
        name: String,
    ) -> Result<l2::L2Interface, L2Error> {
        let _driver = acquire_driver().await.map_err(L2Error::from)?;
        if name.is_empty() || name == INTERFACE_NAME {
            Ok(l2::L2Interface::new(RtlInterface))
        } else {
            Err(L2Error::NoSuchInterface)
        }
    }

    fn info(_iface: l2::L2InterfaceBorrow<'_>) -> InterfaceInfo {
        // `info` is a sync WIT function: it reads the resting driver from its slot
        // (the cached link view) and reports the link down rather than trapping if
        // the state is unavailable (mid-operation, or bring-up went sideways).
        let resting = if STATE.is_set() {
            STATE.with(|slot| slot.driver.as_ref().map(Driver::interface_info))
        } else {
            None
        };
        resting.unwrap_or(InterfaceInfo {
            name: String::from(INTERFACE_NAME),
            mac: (0, 0, 0, 0, 0, 0),
            mtu: 0,
            up: false,
        })
    }

    async fn send_frame(
        _iface: l2::L2InterfaceBorrow<'_>,
        frame: Buffer,
    ) -> (Buffer, Result<SendResult, L2Error>) {
        let len = frame.len();
        // Copy out of the buffer before driving the device so no buffer call
        // interleaves with the request (the disk.virtio / net.virtio discipline).
        let bytes = if len == 0 {
            Vec::new()
        } else {
            frame.read(0, len)
        };
        let mut driver = match acquire_driver().await {
            Ok(driver) => driver,
            Err(fail) => return (frame, Err(L2Error::from(fail))),
        };
        match driver.send(&bytes).await {
            Ok(bytes_sent) => (frame, Ok(SendResult { bytes_sent })),
            Err(fail) => (frame, Err(L2Error::from(fail))),
        }
    }

    async fn recv_frame(
        _iface: l2::L2InterfaceBorrow<'_>,
        dst: Buffer,
    ) -> (Buffer, Result<RecvResult, L2Error>) {
        let capacity = dst.len();
        let mut driver = match acquire_driver().await {
            Ok(driver) => driver,
            Err(fail) => return (dst, Err(L2Error::from(fail))),
        };
        match driver.recv(capacity).await {
            Ok(bytes) => {
                if !bytes.is_empty() {
                    dst.write(0, &bytes);
                }
                (
                    dst,
                    Ok(RecvResult {
                        bytes_received: bytes.len() as u64,
                    }),
                )
            }
            Err(fail) => (dst, Err(L2Error::from(fail))),
        }
    }
}

export!(Stub);
