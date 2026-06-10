//! The shared OHCI driver: controller takeover, the control/periodic schedules,
//! transfers, and enumeration — generic over [`RegionIo`], so the platform-backed
//! board shell (`usb.ohci`) and the PCI-backed QEMU shell (`usb.ohci-pci`) are the
//! same code behind 20-line adapters (docs/board/usb-ohci-plan.md §2).
//!
//! Disciplines carried over verbatim from the D46 driver line: bounded polls
//! everywhere (each iteration of every loop below is a host call; the bounds are
//! seconds at worst, never a minute — the stranded-runnable lesson), typed errors
//! never traps, and the short-poll "empty result = nothing waiting" contract on the
//! interrupt endpoint. The drivers hold no time capability: every wait is counted on
//! HcFmNumber, the controller's own 1 ms frame counter (OHCI 1.0a §7.3.3).
//!
//! Events over polls (the timer-crutch audit A1/A4): where the shell's underlying
//! capability routes the controller's interrupt (`eo9:pci` enable-interrupts under
//! QEMU; `eo9:platform` once the board kernel routes GIC SPIs 216/219), the shell
//! calls [`Ohci::enable_events`] after bring-up and the steady-state waits become
//! event-driven: [`read_report`](Ohci::read_report) parks on WritebackDoneHead and
//! [`wait_port_change`](Ohci::wait_port_change) on Root Hub Status Change, each wait
//! bounded by the provider. Where no interrupt surface exists, both fall back to the
//! original short-poll shape and the CONSUMER paces (capability-gated, not cfg-gated).
//! The frame-counted waits ([`wait_ms`](Ohci::wait_ms) for reset recovery / settle)
//! are honest USB-mandated obligations and stay as they are — a separate owner
//! decision (audit class B).

use crate::enumerate::{Action, Enumerated, Enumeration, EnumerationError, Event};
use crate::hub::{self, HubDescriptor, HubPortStatus};
use crate::schedule::{
    DI_IMMEDIATE, EdDirection, EndpointDescriptor, TdPid, TdToggle, TransferDescriptor, arena,
    hcca, walk_done_queue,
};
use crate::setup::SetupPacket;
use crate::{ConditionCode, bits, fm_interval_restore, periodic_start, reg};

/// What a shell provides: register access into the claimed OHCI register block and
/// byte access into the one DMA arena ([`arena`]) it allocated. Register access is
/// async (it crosses the capability boundary); DMA byte access mirrors the WIT's
/// synchronous `dma-read`/`dma-write` accessors.
#[allow(async_fn_in_trait)]
pub trait RegionIo {
    /// The capability's error (the shell maps it into its own typed error).
    type Error;

    /// Read the 32-bit register at `offset`. All OHCI operational registers are
    /// 32-bit (OHCI 1.0a §7: "All registers should be read and written as Dwords").
    async fn read32(&mut self, offset: u64) -> Result<u32, Self::Error>;

    /// Write the 32-bit register at `offset`.
    async fn write32(&mut self, offset: u64, value: u32) -> Result<(), Self::Error>;

    /// Device-visible (bus) address of arena offset 0. OHCI pointers are 32-bit, so
    /// the shell verifies the arena sits below 4 GiB before constructing the driver.
    fn dma_base(&self) -> u32;

    /// Copy bytes into the arena at `offset` (the provider's coherence brackets ride
    /// inside).
    fn dma_write(&mut self, offset: u64, bytes: &[u8]);

    /// Copy bytes out of the arena at `offset`.
    fn dma_read(&mut self, offset: u64, buf: &mut [u8]);

    /// Wait for the controller's next interrupt delivery, if the shell routes one.
    ///
    /// `Delivered` = at least one delivery arrived (possibly coalesced, possibly a
    /// shared-line spurious — the caller re-checks the actual cause); `TimedOut` =
    /// the provider's bounded wait expired (or the wait failed for any other reason
    /// — the caller falls back to one poll round and may wait again, so a
    /// persistently failing wait degrades to a provider-bound-paced poll, never a
    /// hang and never a hot spin); `Unsupported` = no interrupt surface on this
    /// configuration (the default — e.g. eo9:platform v1, x86_64 PCI), the caller
    /// keeps the polled shape and its consumer owns the pacing.
    async fn wait_interrupt(&mut self) -> Result<WaitOutcome, Self::Error> {
        Ok(WaitOutcome::Unsupported)
    }
}

/// Outcome of one bounded interrupt wait (see [`RegionIo::wait_interrupt`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WaitOutcome {
    /// A delivery arrived; the cause registers say what (and a shared line can make
    /// this spurious — always re-check the cause).
    Delivered,
    /// The bounded wait expired with no delivery.
    TimedOut,
    /// No interrupt surface exists on this configuration.
    Unsupported,
}

/// What [`Ohci::read_report`] answers: the report length when one retired (`None` =
/// nothing arrived within this round), plus the liveness flag — `rescued` is set when
/// the report was found by the post-timeout drain poll, i.e. work the interrupt
/// should have delivered (the shell reports it loudly; owner doctrine: a backstop
/// rescuing work the event path owed is a bug surfacing).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ReadReport {
    pub length: Option<usize>,
    pub rescued: bool,
}

/// Typed driver failures. `Io` carries the capability's own error; everything else is
/// the device conversation refusing.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DriverError<E> {
    /// The underlying capability refused (claim revoked, register access failed, …).
    Io(E),
    /// HcRevision is not an OHCI 1.x controller.
    NotOhci { revision: u32 },
    /// A bounded poll expired; the literal names which wait.
    Timeout(&'static str),
    /// The endpoint answered STALL.
    Stall,
    /// A transfer retired with an error condition code.
    Transfer(ConditionCode),
    /// No such root-hub port.
    NoSuchPort,
    /// Nothing is connected on the port.
    NotConnected,
    /// The enumeration protocol failed (malformed descriptors, oversized config).
    Enumeration(EnumerationError),
    /// The hub-traversal path refused; the literal names the demo-scope limitation
    /// (multiple connected ports, a low-speed child, a hubless device).
    Hub(&'static str),
    /// The done queue's pointer chain exceeded its walk bound (corrupt DMA memory).
    DoneQueueCorrupt,
}

impl<E> From<EnumerationError> for DriverError<E> {
    fn from(error: EnumerationError) -> Self {
        DriverError::Enumeration(error)
    }
}

/// Controller identity, read at bring-up.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ControllerInfo {
    /// HcRevision & 0xff (0x10 = OHCI 1.0).
    pub revision: u8,
    /// HcRhDescriptorA.NDP.
    pub ports: u8,
}

/// One root-hub port's status, decoded from HcRhPortStatus (OHCI 1.0a §7.4.4).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PortStatus {
    pub connected: bool,
    pub enabled: bool,
    pub powered: bool,
    pub low_speed: bool,
    pub connect_change: bool,
    /// The raw register, for diagnostics.
    pub raw: u32,
}

/// An attached, addressed, fully-enumerated device.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Attached {
    pub enumerated: Enumerated,
    pub low_speed: bool,
}

// Poll bounds. Each iteration is one host call (a register read), so these are
// calibrated in host calls, not time: well under the executor's idle backstop, far
// above any healthy completion (the D46 bounded-poll discipline).
/// Software reset completes within 10 µs (§7.1.3); QEMU completes it inline.
const RESET_POLL_LIMIT: u32 = 10_000;
/// SMM ownership handshake (§5.1.1.3); QEMU never sets IR.
const OWNERSHIP_POLL_LIMIT: u32 = 10_000;
/// Port reset completes in ~10 ms (§7.4.4); each poll is a host call.
const PORT_RESET_POLL_LIMIT: u32 = 200_000;
/// One counted millisecond of HcFmNumber, in register reads.
const FRAME_POLL_LIMIT_PER_MS: u32 = 50_000;
/// A control transfer (multi-packet at 8-byte MPS, plus low-speed) spans a few
/// frames; the ED-drain poll is bounded accordingly.
const CONTROL_POLL_LIMIT: u32 = 500_000;
/// Done-queue walk bound: the arena only holds TD_SLOTS TDs.
const DONE_QUEUE_LIMIT: usize = arena::TD_SLOTS as usize + 2;
/// Event mode: the HccaDoneHead writeback for a retired DI_IMMEDIATE TD lands at the
/// next frame boundary (≤1 ms); this bounds the wait for it in host calls — a few
/// frames' worth, far above any healthy controller, far below the executor's bounds.
const WRITEBACK_POLL_LIMIT: u32 = 200_000;

/// The driver. Construct with [`Ohci::new`], then [`bring_up`](Self::bring_up) before
/// anything else.
pub struct Ohci<R: RegionIo> {
    io: R,
    info: Option<ControllerInfo>,
    /// The interrupt-IN endpoint's re-arm state: which TD slot is pending and which
    /// is the dummy tail, plus the buffer length to read back.
    interrupt: Option<InterruptState>,
    /// Whether [`enable_events`](Self::enable_events) unmasked WDH/RHSC — i.e. the
    /// shell routes the controller's interrupt and the event-driven wait paths are
    /// live. False = the original polled shape (the consumer paces).
    events: bool,
}

struct InterruptState {
    pending_slot: u64,
    dummy_slot: u64,
    max_packet: u16,
}

impl<R: RegionIo> Ohci<R> {
    pub fn new(io: R) -> Ohci<R> {
        Ohci {
            io,
            info: None,
            interrupt: None,
            events: false,
        }
    }

    /// The controller info, if bring-up already ran.
    pub fn info(&self) -> Option<ControllerInfo> {
        self.info
    }

    /// Whether the event-driven wait paths are live (see [`enable_events`](Self::enable_events)).
    pub fn events(&self) -> bool {
        self.events
    }

    /// Unmask the two interrupts this driver actually consumes — WritebackDoneHead
    /// (done-queue completion) and Root Hub Status Change (port connect) — plus
    /// MasterInterruptEnable, so the controller drives its interrupt line and the
    /// shell's `wait_interrupt` has an edge to wait on. Call ONLY after bring-up and
    /// only when the shell actually routes the interrupt (a granted vector): with no
    /// listener the unmask buys nothing and the polled discipline stays cheaper.
    ///
    /// Ack discipline (unchanged by the unmask): WDH is acknowledged exactly once,
    /// by [`consume_done_queue`](Self::consume_done_queue) AFTER it takes
    /// HccaDoneHead (§5.2.9 — acking without taking would let the next writeback
    /// overwrite an unread chain); RHSC is acknowledged by the wait paths after a
    /// delivery, with the underlying per-port change bits left in HcRhPortStatus for
    /// the sweeps to read (the status registers carry the data; the interrupt-status
    /// bits are just the edge latches).
    pub async fn enable_events(&mut self) -> Result<(), DriverError<R::Error>> {
        self.io
            .write32(
                reg::HC_INTERRUPT_ENABLE,
                bits::INT_MIE | bits::INT_WDH | bits::INT_RHSC,
            )
            .await
            .map_err(DriverError::Io)?;
        self.events = true;
        Ok(())
    }

    /// Take the controller over and make it operational (OHCI 1.0a §5.1.1):
    /// ownership handshake if SMM holds it, software reset **with HcFmInterval saved
    /// and restored across it** (§5.1.1.4 — the gotcha), HCCA, schedules cleared,
    /// operational state, interrupts masked (a shell that routes the controller's
    /// interrupt unmasks WDH/RHSC afterwards — [`enable_events`](Self::enable_events)),
    /// root-hub power on.
    pub async fn bring_up(&mut self) -> Result<ControllerInfo, DriverError<R::Error>> {
        let io = |e| DriverError::Io(e);

        let revision = self.io.read32(reg::HC_REVISION).await.map_err(io)?;
        if revision & 0xf0 != 0x10 {
            return Err(DriverError::NotOhci { revision });
        }

        // SMM ownership handshake (§5.1.1.3): request the change, wait for IR to drop.
        let control = self.io.read32(reg::HC_CONTROL).await.map_err(io)?;
        if control & bits::CONTROL_IR != 0 {
            self.io
                .write32(reg::HC_COMMAND_STATUS, bits::CMD_OCR)
                .await
                .map_err(io)?;
            let mut polls = 0;
            loop {
                let control = self.io.read32(reg::HC_CONTROL).await.map_err(io)?;
                if control & bits::CONTROL_IR == 0 {
                    break;
                }
                polls += 1;
                if polls > OWNERSHIP_POLL_LIMIT {
                    return Err(DriverError::Timeout("SMM ownership handshake"));
                }
            }
        }

        // Save HcFmInterval, reset, restore (§5.1.1.4). The reset zaps the register to
        // its default; skipping the restore is THE classic OHCI bring-up bug.
        let saved_interval = self.io.read32(reg::HC_FM_INTERVAL).await.map_err(io)?;
        self.io
            .write32(reg::HC_COMMAND_STATUS, bits::CMD_HCR)
            .await
            .map_err(io)?;
        let mut polls = 0;
        loop {
            let status = self.io.read32(reg::HC_COMMAND_STATUS).await.map_err(io)?;
            if status & bits::CMD_HCR == 0 {
                break;
            }
            polls += 1;
            if polls > RESET_POLL_LIMIT {
                return Err(DriverError::Timeout("software reset"));
            }
        }
        let after_reset = self.io.read32(reg::HC_FM_INTERVAL).await.map_err(io)?;
        let frame_interval = saved_interval & bits::FM_INTERVAL_FI_MASK;
        self.io
            .write32(
                reg::HC_FM_INTERVAL,
                fm_interval_restore(frame_interval, after_reset),
            )
            .await
            .map_err(io)?;
        self.io
            .write32(reg::HC_PERIODIC_START, periodic_start(frame_interval))
            .await
            .map_err(io)?;

        // HCCA: zero the whole area, then point the controller at it.
        self.io.dma_write(arena::HCCA, &[0u8; hcca::SIZE as usize]);
        let hcca_bus = self.io.dma_base() + arena::HCCA as u32;
        self.io.write32(reg::HC_HCCA, hcca_bus).await.map_err(io)?;

        // Empty schedules.
        self.io
            .write32(reg::HC_CONTROL_HEAD_ED, 0)
            .await
            .map_err(io)?;
        self.io
            .write32(reg::HC_CONTROL_CURRENT_ED, 0)
            .await
            .map_err(io)?;
        self.io.write32(reg::HC_BULK_HEAD_ED, 0).await.map_err(io)?;
        self.io
            .write32(reg::HC_BULK_CURRENT_ED, 0)
            .await
            .map_err(io)?;

        // Mask everything and clear stale causes: bring-up always starts from the
        // polled shape (the WDH writeback to the HCCA happens regardless). A shell
        // that routes the controller's interrupt unmasks the two consumed causes
        // afterwards via `enable_events` — never here, because with no granted
        // vector an unmasked cause is just an interrupt line nobody listens to.
        self.io
            .write32(reg::HC_INTERRUPT_DISABLE, !0)
            .await
            .map_err(io)?;
        self.io
            .write32(reg::HC_INTERRUPT_STATUS, !0)
            .await
            .map_err(io)?;

        // Operational — must happen promptly after reset or the controller drifts to
        // suspend (§5.1.1.4); the frame counter only runs in this state.
        self.io
            .write32(reg::HC_CONTROL, bits::CONTROL_HCFS_OPERATIONAL)
            .await
            .map_err(io)?;

        // Root hub: global power plus per-port power (covers both switching modes;
        // a no-power-switching hub ignores them), then PowerOnToPowerGoodTime.
        let descriptor_a = self.io.read32(reg::HC_RH_DESCRIPTOR_A).await.map_err(io)?;
        let ports = (descriptor_a & bits::RH_A_NDP_MASK) as u8;
        self.io
            .write32(reg::HC_RH_STATUS, bits::RH_STATUS_LPSC)
            .await
            .map_err(io)?;
        for port in 1..=ports {
            self.io
                .write32(reg::rh_port_status(port), bits::PORT_PPS)
                .await
                .map_err(io)?;
        }
        let potpgt_ms = 2 * ((descriptor_a >> bits::RH_A_POTPGT_SHIFT) & 0xff);
        if potpgt_ms > 0 {
            self.wait_ms(potpgt_ms).await?;
        }

        let info = ControllerInfo {
            revision: (revision & 0xff) as u8,
            ports,
        };
        self.info = Some(info);
        Ok(info)
    }

    /// Decode one port's status. Ports are 1-based.
    pub async fn port_status(&mut self, port: u8) -> Result<PortStatus, DriverError<R::Error>> {
        let ports = self.info.map(|info| info.ports).unwrap_or(0);
        if port == 0 || port > ports {
            return Err(DriverError::NoSuchPort);
        }
        let raw = self
            .io
            .read32(reg::rh_port_status(port))
            .await
            .map_err(DriverError::Io)?;
        Ok(PortStatus {
            connected: raw & bits::PORT_CCS != 0,
            enabled: raw & bits::PORT_PES != 0,
            powered: raw & bits::PORT_PPS != 0,
            low_speed: raw & bits::PORT_LSDA != 0,
            connect_change: raw & bits::PORT_CSC != 0,
            raw,
        })
    }

    /// Count `ms` milliseconds on HcFmNumber (the 1 ms frame counter — the driver's
    /// only clock; guest drivers hold no time capability). Bounded in host calls.
    pub async fn wait_ms(&mut self, ms: u32) -> Result<(), DriverError<R::Error>> {
        let start = self
            .io
            .read32(reg::HC_FM_NUMBER)
            .await
            .map_err(DriverError::Io)?
            & 0xffff;
        let mut polls: u64 = 0;
        let limit = u64::from(FRAME_POLL_LIMIT_PER_MS) * u64::from(ms.max(1));
        loop {
            let now = self
                .io
                .read32(reg::HC_FM_NUMBER)
                .await
                .map_err(DriverError::Io)?
                & 0xffff;
            // 16-bit frame counter: wrapping distance.
            let elapsed = now.wrapping_sub(start) & 0xffff;
            if elapsed >= ms {
                return Ok(());
            }
            polls += 1;
            if polls > limit {
                return Err(DriverError::Timeout("frame-counted wait"));
            }
        }
    }

    /// Reset one port and wait for the controller-timed reset to complete (PRSC).
    pub async fn reset_port(&mut self, port: u8) -> Result<(), DriverError<R::Error>> {
        let io = |e| DriverError::Io(e);
        let status = self.port_status(port).await?;
        if !status.connected {
            return Err(DriverError::NotConnected);
        }
        self.io
            .write32(reg::rh_port_status(port), bits::PORT_PRS)
            .await
            .map_err(io)?;
        let mut polls = 0;
        loop {
            let raw = self
                .io
                .read32(reg::rh_port_status(port))
                .await
                .map_err(io)?;
            if raw & bits::PORT_PRSC != 0 {
                // Write-1-to-clear the change bit; the port comes out enabled.
                self.io
                    .write32(reg::rh_port_status(port), bits::PORT_PRSC)
                    .await
                    .map_err(io)?;
                return Ok(());
            }
            polls += 1;
            if polls > PORT_RESET_POLL_LIMIT {
                return Err(DriverError::Timeout("port reset"));
            }
        }
    }

    /// Run one control transfer on endpoint 0 of `address`. For IN requests the
    /// response lands in `data_in` (returns how many bytes arrived); OUT data (none
    /// in v1's request set beyond zero-length) would ride the same path.
    pub async fn control(
        &mut self,
        address: u8,
        max_packet: u8,
        low_speed: bool,
        setup: SetupPacket,
        data_in: &mut [u8],
    ) -> Result<usize, DriverError<R::Error>> {
        let io = |e| DriverError::Io(e);
        let base = self.io.dma_base();
        let in_length = if setup.is_in() {
            (setup.length as usize)
                .min(data_in.len())
                .min(arena::CONTROL_BUFFER_LEN as usize)
        } else {
            0
        };

        // The setup packet bytes.
        self.io.dma_write(arena::SETUP_BUFFER, &setup.encode());

        // TD chain: SETUP (DATA0) -> optional DATA (DATA1, controller toggles within
        // the TD) -> STATUS (opposite direction, DATA1) -> dummy tail (§4.3.1; USB 2.0
        // §8.5.3 for the toggle/status rules).
        let setup_td = arena::td_slot(0);
        let data_td = arena::td_slot(1);
        let status_td = arena::td_slot(2);
        let dummy_td = arena::td_slot(3);

        // Event mode: every TD requests the done-queue writeback (DI = 0). With the
        // default DI_NONE the controller defers the HccaDoneHead writeback
        // indefinitely; the deferred chain would then flush all at once when the
        // first interrupt-endpoint TD retires — pointing at control TD slots long
        // since rewritten for later transfers (a corrupt walk). Requesting the
        // writeback per transfer keeps the chain ≤ one transfer and lets
        // `reap_done_queue` consume it before any slot is reused. Polled mode keeps
        // DI_NONE and the original drain-by-ED shape, byte for byte.
        let delay_interrupt = if self.events {
            DI_IMMEDIATE
        } else {
            crate::schedule::DI_NONE
        };

        let mut setup_descriptor = TransferDescriptor::new(
            TdPid::Setup,
            TdToggle::Data0,
            Some((base + arena::SETUP_BUFFER as u32, 8)),
        );
        let has_data = in_length > 0;
        setup_descriptor.delay_interrupt = delay_interrupt;
        setup_descriptor.next = base + if has_data { data_td } else { status_td } as u32;
        self.io.dma_write(setup_td, &setup_descriptor.encode());

        if has_data {
            let mut data_descriptor = TransferDescriptor::new(
                TdPid::In,
                TdToggle::Data1,
                Some((base + arena::CONTROL_BUFFER as u32, in_length as u32)),
            );
            data_descriptor.delay_interrupt = delay_interrupt;
            data_descriptor.next = base + status_td as u32;
            self.io.dma_write(data_td, &data_descriptor.encode());
        }

        // Status stage: IN for OUT/no-data requests, OUT for IN requests; always DATA1.
        let status_pid = if has_data { TdPid::Out } else { TdPid::In };
        let mut status_descriptor = TransferDescriptor::new(status_pid, TdToggle::Data1, None);
        status_descriptor.delay_interrupt = delay_interrupt;
        status_descriptor.next = base + dummy_td as u32;
        self.io.dma_write(status_td, &status_descriptor.encode());
        self.io
            .dma_write(dummy_td, &TransferDescriptor::default().encode());

        // The control ED.
        let ed = EndpointDescriptor {
            function_address: address,
            endpoint_number: 0,
            direction: EdDirection::FromTd,
            low_speed,
            max_packet_size: u16::from(max_packet),
            head: base + setup_td as u32,
            tail: base + dummy_td as u32,
            ..EndpointDescriptor::default()
        };
        self.io.dma_write(arena::CONTROL_ED, &ed.encode());

        // Hand the list to the controller: head, ControlListEnable, ControlListFilled.
        self.io
            .write32(reg::HC_CONTROL_HEAD_ED, base + arena::CONTROL_ED as u32)
            .await
            .map_err(io)?;
        self.io
            .write32(reg::HC_CONTROL_CURRENT_ED, 0)
            .await
            .map_err(io)?;
        let control = self.io.read32(reg::HC_CONTROL).await.map_err(io)?;
        self.io
            .write32(reg::HC_CONTROL, control | bits::CONTROL_CLE)
            .await
            .map_err(io)?;
        self.io
            .write32(reg::HC_COMMAND_STATUS, bits::CMD_CLF)
            .await
            .map_err(io)?;

        // Drain: the ED's head catches its tail (all TDs retired), or halts.
        let mut polls = 0;
        let halted = loop {
            let mut ed_bytes = [0u8; 16];
            self.io.dma_read(arena::CONTROL_ED, &mut ed_bytes);
            let current = EndpointDescriptor::decode(&ed_bytes);
            if current.halted || current.head == current.tail {
                break current.halted;
            }
            polls += 1;
            if polls > CONTROL_POLL_LIMIT {
                // Skip the ED and take the list back before giving up, so a wedged
                // transfer cannot keep DMA-ing into the arena.
                self.disable_control_list().await?;
                return Err(DriverError::Timeout("control transfer"));
            }
        };
        self.disable_control_list().await?;
        // Clean drain: every submitted TD retired (head reached the dummy), so every
        // writeback is owed. Halted: only the offender is guaranteed retired (its
        // predecessors' writebacks, if split off, are strays the next consume
        // collects; the ED and slots are re-initialized wholesale on the next
        // transfer, so a stray walk reads retired-then-rewritten content only on the
        // already-rare error path — the happy path is airtight).
        let expected = if halted {
            1
        } else if has_data {
            3
        } else {
            2
        };
        self.reap_done_queue(expected).await?;

        // Judge the retired TDs by their written-back condition codes.
        // DataUnderrun with rounding on is a permitted short packet; the controller
        // only halts on it when rounding is off (we set R).
        let read_td = |io: &mut R, slot: u64| -> TransferDescriptor {
            let mut td_bytes = [0u8; 16];
            io.dma_read(slot, &mut td_bytes);
            TransferDescriptor::decode(&td_bytes)
        };
        let judge = |td: TransferDescriptor| -> Result<TransferDescriptor, DriverError<R::Error>> {
            match td.condition_code {
                ConditionCode::NoError => Ok(td),
                ConditionCode::Stall => Err(DriverError::Stall),
                other => Err(DriverError::Transfer(other)),
            }
        };
        // Judge in PIPELINE order — setup, data, status — so the first stage that
        // errored reports its REAL condition code. A halted ED stops the pipeline at
        // the erroring TD; the stages behind it read back NotAccessed, so checking
        // the halt flag before judging every stage would mask the actual failure
        // behind a meaningless `NotAccessed` (the M3 board lesson: a device that
        // STALLs the status stage of a no-data request — e.g. the optional-for-mice
        // HID SET_IDLE — must surface as `Stall`, not as schedule-never-ran).
        judge(read_td(&mut self.io, setup_td))?;
        let received = if has_data {
            let data = judge(read_td(&mut self.io, data_td))?;
            let received = data
                .bytes_transferred(base + arena::CONTROL_BUFFER as u32, in_length as u32)
                as usize;
            self.io
                .dma_read(arena::CONTROL_BUFFER, &mut data_in[..received]);
            received
        } else {
            0
        };
        judge(read_td(&mut self.io, status_td))?;
        if halted {
            // Halted yet every stage judged clean: genuinely anomalous (a writeback
            // the controller never made) — the one case NotAccessed is the honest
            // answer.
            return Err(DriverError::Transfer(ConditionCode::NotAccessed));
        }
        Ok(received)
    }

    async fn disable_control_list(&mut self) -> Result<(), DriverError<R::Error>> {
        let control = self
            .io
            .read32(reg::HC_CONTROL)
            .await
            .map_err(DriverError::Io)?;
        self.io
            .write32(reg::HC_CONTROL, control & !bits::CONTROL_CLE)
            .await
            .map_err(DriverError::Io)?;
        Ok(())
    }

    /// Take HccaDoneHead if the controller wrote one, clear it, and acknowledge WDH so
    /// the next writeback can happen (§5.2.9). Returns the head pointer (0 = nothing).
    async fn consume_done_queue(&mut self) -> Result<u32, DriverError<R::Error>> {
        let mut head_bytes = [0u8; 4];
        self.io
            .dma_read(arena::HCCA + hcca::DONE_HEAD, &mut head_bytes);
        let head = u32::from_le_bytes(head_bytes);
        if head == 0 {
            return Ok(0);
        }
        self.io
            .dma_write(arena::HCCA + hcca::DONE_HEAD, &0u32.to_le_bytes());
        self.io
            .write32(reg::HC_INTERRUPT_STATUS, bits::INT_WDH)
            .await
            .map_err(DriverError::Io)?;
        // Validate the chain (bounded), but the per-TD judgement is the caller's.
        let mut reads: heapless_chain::Chain = Default::default();
        let mut walked = 0u32;
        let result = walk_done_queue(
            head,
            DONE_QUEUE_LIMIT,
            |address| {
                let offset = u64::from(address - self.io.dma_base());
                let mut td_bytes = [0u8; 16];
                self.io.dma_read(offset, &mut td_bytes);
                TransferDescriptor::decode(&td_bytes)
            },
            |address, _| {
                reads.push(address);
                walked += 1;
            },
        );
        if result.is_err() {
            return Err(DriverError::DoneQueueCorrupt);
        }
        Ok(walked)
    }

    /// Take and acknowledge the done-queue writeback for TDs known to have retired,
    /// COUNTED: `expected` is the number of retired TDs whose writebacks must be
    /// collected before any of their slots may be reused.
    ///
    /// Event mode: the retired TDs requested the interrupt (DI_IMMEDIATE), so the
    /// writebacks land within a frame each — wait them out (bounded) and consume
    /// until the count is in, keeping the TD slots clean BEFORE they are reused (a
    /// slot rewritten while its writeback is still pending would leave HccaDoneHead
    /// pointing into recycled memory — the corrupt-walk class this discipline exists
    /// to prevent). Counting matters because the writebacks can SPLIT: a chain whose
    /// TDs retire across frame boundaries flushes its first batch at the next tick
    /// and holds the rest behind the WDH it just set — a reap that stops at the
    /// first non-empty take would leave the held batch to land after slot reuse
    /// (the recycled-slot walk again, in a narrower window).
    ///
    /// Polled mode: DI_NONE TDs never arm the writeback, so this is one best-effort
    /// take — the original shape, byte for byte; `expected` is ignored.
    async fn reap_done_queue(&mut self, expected: u32) -> Result<(), DriverError<R::Error>> {
        let mut taken = self.consume_done_queue().await?;
        if !self.events {
            return Ok(());
        }
        let mut polls = 0u32;
        while taken < expected {
            taken += self.consume_done_queue().await?;
            polls += 1;
            if polls > WRITEBACK_POLL_LIMIT {
                return Err(DriverError::Timeout("done-queue writeback"));
            }
        }
        Ok(())
    }

    /// Reset the port, address the device, and run the GET_DESCRIPTOR chain — the
    /// [`Enumeration`] machine drives, this driver executes. The device address is the
    /// port number (1..=15 — collision-free without an allocator).
    pub async fn attach(
        &mut self,
        port: u8,
        config_out: &mut [u8],
    ) -> Result<(Attached, usize), DriverError<R::Error>> {
        let status = self.port_status(port).await?;
        if !status.connected {
            return Err(DriverError::NotConnected);
        }
        let low_speed = status.low_speed;
        let mut machine = Enumeration::new(port);
        loop {
            match machine.next_action() {
                Action::ResetPort => {
                    self.reset_port(port).await?;
                    machine.event(Event::PortResetComplete)?;
                }
                Action::WaitMs(ms) => {
                    self.wait_ms(ms).await?;
                    machine.event(Event::Waited)?;
                }
                Action::Control {
                    address,
                    max_packet,
                    setup,
                } => {
                    let mut buffer = [0u8; crate::enumerate::MAX_CONFIG_BYTES];
                    let received = self
                        .control(address, max_packet, low_speed, setup, &mut buffer)
                        .await?;
                    machine.event(Event::ControlDone {
                        data: &buffer[..received],
                    })?;
                }
                Action::Done => break,
            }
        }
        let enumerated = machine
            .result()
            .ok_or(DriverError::Enumeration(EnumerationError::ProtocolMismatch))?;
        let configuration = machine.configuration();
        let length = configuration.len().min(config_out.len());
        config_out[..length].copy_from_slice(&configuration[..length]);
        Ok((
            Attached {
                enumerated,
                low_speed,
            },
            length,
        ))
    }

    /// Traverse ONE hub level to the single device behind it — the demo-scope hub
    /// mini-driver (the bench keyboard sits on port 1 of its own built-in 3-port FS
    /// hub). `hub` is the already-attached hub (class 09, full speed); the method
    /// configures it, powers its ports, finds exactly ONE connected full-speed
    /// child, runs the hub-timed PORT_RESET (USB 2.0 §11.5.1.5, completion via
    /// C_PORT_RESET), and enumerates the child with the standard machine — the same
    /// SET_ADDRESS/GET_DESCRIPTOR chain, reset mediated by hub-class requests
    /// instead of the root-hub register.
    ///
    /// Deliberately NOT hub support: one level deep, one child, FS-behind-FS only
    /// (no low-speed children — that needs nothing OHCI-side but is untested
    /// hardware territory; refused typed until a bench device needs it), and
    /// port-change detection is CONTROL POLLING on the caller's cadence — the hub's
    /// status-change interrupt endpoint (255 ms on the bench hub) buys nothing for
    /// a one-shot traversal and would occupy the one interrupt ED the schedule
    /// carries (the HID endpoint's slot).
    ///
    /// The child's address is `hub address + 16` (root devices are addressed by
    /// port number 1..=15, so 17..=31 is collision-free for one hub level).
    pub async fn attach_hub_child(
        &mut self,
        hub_address: u8,
        hub_mps: u8,
        hub_low_speed: bool,
        hub_class: u8,
        hub_config: &[u8],
        config_out: &mut [u8],
    ) -> Result<(Attached, usize), DriverError<R::Error>> {
        if hub_class != hub::CLASS_HUB {
            return Err(DriverError::Hub("the attached device is not a hub"));
        }
        if hub_low_speed {
            return Err(DriverError::Hub("a low-speed hub is not a USB device"));
        }

        // The hub must be configured before any port operation (USB 2.0 §11.24).
        // Its configuration value comes from the blob the caller kept from attach().
        let configuration = crate::descriptor::ConfigurationDescriptor::parse(hub_config)
            .map(|c| c.configuration_value)
            .unwrap_or(1);
        let mut scratch = [0u8; 16];
        self.control(
            hub_address,
            hub_mps,
            hub_low_speed,
            crate::setup::set_configuration(configuration),
            &mut [],
        )
        .await?;

        // Hub descriptor head: port count + power-good time.
        let received = self
            .control(
                hub_address,
                hub_mps,
                hub_low_speed,
                hub::get_hub_descriptor(9),
                &mut scratch[..9],
            )
            .await?;
        let descriptor = HubDescriptor::parse(&scratch[..received])
            .ok_or(DriverError::Hub("the hub descriptor did not parse"))?;

        // Power every port, wait the hub's own declared power-good time (+ slack).
        for port in 1..=descriptor.ports {
            self.control(
                hub_address,
                hub_mps,
                hub_low_speed,
                hub::set_port_power(port),
                &mut [],
            )
            .await?;
        }
        self.wait_ms(u32::from(descriptor.power_on_to_power_good_2ms) * 2 + 10)
            .await?;

        // Exactly one connected child (the demo scope), full speed only.
        let mut connected_port = None;
        for port in 1..=descriptor.ports {
            let received = self
                .control(
                    hub_address,
                    hub_mps,
                    hub_low_speed,
                    hub::get_port_status(port),
                    &mut scratch[..4],
                )
                .await?;
            let Some(status) = HubPortStatus::parse(&scratch[..received]) else {
                continue;
            };
            if status.connected {
                if connected_port.is_some() {
                    return Err(DriverError::Hub(
                        "multiple devices behind the hub (one-child demo scope)",
                    ));
                }
                if status.speed == hub::PortSpeed::Low {
                    return Err(DriverError::Hub(
                        "a low-speed child behind the hub (FS-behind-FS demo scope)",
                    ));
                }
                connected_port = Some(port);
            }
        }
        let child_port =
            connected_port.ok_or(DriverError::Hub("no device connected behind the hub"))?;

        // Enumerate the child: the standard machine, with the port reset mediated by
        // hub-class requests (PORT_RESET / C_PORT_RESET) instead of root-hub bits.
        let child_address = hub_address + 16;
        let mut machine = Enumeration::new(child_address);
        loop {
            match machine.next_action() {
                Action::ResetPort => {
                    self.hub_port_reset(hub_address, hub_mps, hub_low_speed, child_port)
                        .await?;
                    machine.event(Event::PortResetComplete)?;
                }
                Action::WaitMs(ms) => {
                    self.wait_ms(ms).await?;
                    machine.event(Event::Waited)?;
                }
                Action::Control {
                    address,
                    max_packet,
                    setup,
                } => {
                    let mut buffer = [0u8; crate::enumerate::MAX_CONFIG_BYTES];
                    let received = self
                        .control(address, max_packet, false, setup, &mut buffer)
                        .await?;
                    machine.event(Event::ControlDone {
                        data: &buffer[..received],
                    })?;
                }
                Action::Done => break,
            }
        }
        let enumerated = machine
            .result()
            .ok_or(DriverError::Enumeration(EnumerationError::ProtocolMismatch))?;
        let configuration_blob = machine.configuration();
        let length = configuration_blob.len().min(config_out.len());
        config_out[..length].copy_from_slice(&configuration_blob[..length]);
        Ok((
            Attached {
                enumerated,
                low_speed: false,
            },
            length,
        ))
    }

    /// One hub-timed port reset: SET_FEATURE(PORT_RESET), poll GET_STATUS for
    /// C_PORT_RESET (bounded — the hub drives 10-20 ms of reset signaling,
    /// USB 2.0 §11.5.1.5), acknowledge the change bit.
    async fn hub_port_reset(
        &mut self,
        hub_address: u8,
        hub_mps: u8,
        hub_low_speed: bool,
        port: u8,
    ) -> Result<(), DriverError<R::Error>> {
        self.control(
            hub_address,
            hub_mps,
            hub_low_speed,
            hub::set_port_reset(port),
            &mut [],
        )
        .await?;
        let mut scratch = [0u8; 4];
        let mut polls = 0u32;
        loop {
            self.wait_ms(2).await?;
            let received = self
                .control(
                    hub_address,
                    hub_mps,
                    hub_low_speed,
                    hub::get_port_status(port),
                    &mut scratch,
                )
                .await?;
            if let Some(status) = HubPortStatus::parse(&scratch[..received])
                && status.reset_complete()
            {
                self.control(
                    hub_address,
                    hub_mps,
                    hub_low_speed,
                    hub::clear_port_feature(port, hub::FEATURE_C_PORT_RESET),
                    &mut [],
                )
                .await?;
                return Ok(());
            }
            polls += 1;
            if polls > 100 {
                return Err(DriverError::Timeout("hub port reset"));
            }
        }
    }

    /// Put an interrupt-IN endpoint on the periodic schedule. `interval_ms` places the
    /// ED at power-of-two-spaced interrupt-table entries (OHCI 1.0a §5.2.7.2); one
    /// pending TD at a time, re-armed by [`poll_interrupt`](Self::poll_interrupt).
    pub async fn open_interrupt_in(
        &mut self,
        address: u8,
        low_speed: bool,
        endpoint: u8,
        max_packet: u16,
        interval_ms: u8,
    ) -> Result<(), DriverError<R::Error>> {
        let io = |e| DriverError::Io(e);
        let base = self.io.dma_base();
        let report_len = u64::from(max_packet).min(arena::INTERRUPT_BUFFER_LEN);

        // First pending TD in slot 4, dummy in slot 5 (the control path uses 0-3).
        let pending = arena::td_slot(4);
        let dummy = arena::td_slot(5);
        let mut td = TransferDescriptor::new(
            TdPid::In,
            // Toggle from the ED, whose carry starts at DATA0 (USB 2.0 §8.5.4:
            // interrupt endpoints start at DATA0 after SET_CONFIGURATION).
            TdToggle::FromEd,
            Some((base + arena::INTERRUPT_BUFFER as u32, report_len as u32)),
        );
        if self.events {
            // Event mode parks on WDH, so this TD must REQUEST the writeback
            // interrupt: at the default DI_NONE the controller never arms the
            // interrupt-delay counter and the edge `read_report` waits on does not
            // exist. (Control TDs request it too in event mode — control_transfer's
            // deferred-chain rationale — and their writebacks are reaped counted
            // before slot reuse.)
            td.delay_interrupt = DI_IMMEDIATE;
        }
        td.next = base + dummy as u32;
        self.io.dma_write(pending, &td.encode());
        self.io
            .dma_write(dummy, &TransferDescriptor::default().encode());

        let ed = EndpointDescriptor {
            function_address: address,
            endpoint_number: endpoint,
            direction: EdDirection::In,
            low_speed,
            max_packet_size: max_packet,
            head: base + pending as u32,
            tail: base + dummy as u32,
            ..EndpointDescriptor::default()
        };
        self.io.dma_write(arena::INTERRUPT_ED, &ed.encode());

        // Power-of-two placement: every `stride` entries of the 32-slot table.
        let stride = match interval_ms {
            0..=1 => 1u64,
            2..=3 => 2,
            4..=7 => 4,
            8..=15 => 8,
            16..=31 => 16,
            _ => 32,
        };
        let ed_bus = (base + arena::INTERRUPT_ED as u32).to_le_bytes();
        for entry in 0..hcca::INTERRUPT_TABLE_ENTRIES {
            let value = if entry % stride == 0 { ed_bus } else { [0; 4] };
            self.io
                .dma_write(arena::HCCA + hcca::interrupt_table_entry(entry), &value);
        }

        // Periodic list on.
        let control = self.io.read32(reg::HC_CONTROL).await.map_err(io)?;
        self.io
            .write32(reg::HC_CONTROL, control | bits::CONTROL_PLE)
            .await
            .map_err(io)?;

        self.interrupt = Some(InterruptState {
            pending_slot: 4,
            dummy_slot: 5,
            max_packet,
        });
        Ok(())
    }

    /// One short poll of the interrupt endpoint: if the pending TD retired, copy the
    /// report into `report`, re-arm, and return its length; `None` means nothing
    /// arrived (the consumer owns the wait policy — the recv-frame contract).
    pub async fn poll_interrupt(
        &mut self,
        report: &mut [u8],
    ) -> Result<Option<usize>, DriverError<R::Error>> {
        let Some(InterruptState {
            pending_slot,
            dummy_slot,
            max_packet,
        }) = self.interrupt.as_ref().map(|state| InterruptState {
            pending_slot: state.pending_slot,
            dummy_slot: state.dummy_slot,
            max_packet: state.max_packet,
        })
        else {
            return Err(DriverError::Timeout("interrupt endpoint not open"));
        };
        let base = self.io.dma_base();

        // Has the pending TD retired? The ED's head moving to the dummy says so.
        let mut ed_bytes = [0u8; 16];
        self.io.dma_read(arena::INTERRUPT_ED, &mut ed_bytes);
        let ed = EndpointDescriptor::decode(&ed_bytes);
        if ed.halted {
            // Judge the halted TD for the typed cause, then re-arm cleanly below the
            // consumer's retry.
            let mut td_bytes = [0u8; 16];
            self.io
                .dma_read(arena::td_slot(pending_slot), &mut td_bytes);
            let td = TransferDescriptor::decode(&td_bytes);
            return Err(match td.condition_code {
                ConditionCode::Stall => DriverError::Stall,
                other => DriverError::Transfer(other),
            });
        }
        if ed.head != base + arena::td_slot(dummy_slot) as u32 {
            return Ok(None);
        }

        // Retired: judge, measure, copy out.
        let mut td_bytes = [0u8; 16];
        self.io
            .dma_read(arena::td_slot(pending_slot), &mut td_bytes);
        let td = TransferDescriptor::decode(&td_bytes);
        match td.condition_code {
            ConditionCode::NoError => {}
            ConditionCode::Stall => return Err(DriverError::Stall),
            other => return Err(DriverError::Transfer(other)),
        }
        let report_len = u64::from(max_packet).min(arena::INTERRUPT_BUFFER_LEN);
        let received =
            td.bytes_transferred(base + arena::INTERRUPT_BUFFER as u32, report_len as u32) as usize;
        let copied = received.min(report.len());
        self.io
            .dma_read(arena::INTERRUPT_BUFFER, &mut report[..copied]);
        self.reap_done_queue(1).await?;

        // Re-arm: the old dummy becomes the pending TD, the old pending the new dummy
        // (the standard tail-swap; the controller owns head, we only move tail).
        let new_pending = dummy_slot;
        let new_dummy = pending_slot;
        let mut next = TransferDescriptor::new(
            TdPid::In,
            TdToggle::FromEd,
            Some((base + arena::INTERRUPT_BUFFER as u32, report_len as u32)),
        );
        if self.events {
            // The re-armed TD must keep requesting the writeback interrupt (see
            // open_interrupt_in): every report's WDH is what the next read parks on.
            next.delay_interrupt = DI_IMMEDIATE;
        }
        next.next = base + arena::td_slot(new_dummy) as u32;
        self.io.dma_write(
            arena::td_slot(new_dummy),
            &TransferDescriptor::default().encode(),
        );
        self.io
            .dma_write(arena::td_slot(new_pending), &next.encode());
        // Publish the new tail.
        let mut ed_bytes = [0u8; 16];
        self.io.dma_read(arena::INTERRUPT_ED, &mut ed_bytes);
        let mut ed = EndpointDescriptor::decode(&ed_bytes);
        ed.tail = base + arena::td_slot(new_dummy) as u32;
        self.io.dma_write(arena::INTERRUPT_ED, &ed.encode());

        self.interrupt = Some(InterruptState {
            pending_slot: new_pending,
            dummy_slot: new_dummy,
            max_packet,
        });
        Ok(Some(copied))
    }

    /// The next interrupt-IN report, event-driven where events are live (audit A1).
    ///
    /// Polled shape (`enable_events` never ran): exactly one
    /// [`poll_interrupt`](Self::poll_interrupt) — empty means nothing waiting, the
    /// consumer owns the pacing.
    ///
    /// Event shape: wait first (a delivery that raced in before this call left the
    /// provider's counter pending, so the wait resolves immediately — no pre-wait
    /// poll is needed and the counter never drifts), then drain:
    /// * `Delivered` + a report — the steady-state keystroke path: take it,
    ///   [`consume_done_queue`](Self::consume_done_queue) acked WDH, line deasserts.
    /// * `Delivered` + nothing — the cause was RHSC (or a stale/shared-line
    ///   delivery): acknowledge RHSC so the level line deasserts instead of storming
    ///   the next wait, and answer empty (the consumer just calls again).
    /// * `TimedOut` + a report — work the interrupt owed but never delivered: the
    ///   `rescued` flag, which the shell reports loudly (owner doctrine — a fallback
    ///   that rescues silently is the named bug class).
    pub async fn read_report(
        &mut self,
        report: &mut [u8],
    ) -> Result<ReadReport, DriverError<R::Error>> {
        if !self.events {
            let length = self.poll_interrupt(report).await?;
            return Ok(ReadReport {
                length,
                rescued: false,
            });
        }
        let outcome = self.io.wait_interrupt().await.map_err(DriverError::Io)?;
        let length = self.poll_interrupt(report).await?;
        match outcome {
            WaitOutcome::Delivered if length.is_none() => {
                // The wake was not a report: consume any stale writeback (so an
                // unacked WDH cannot keep the level line asserted and storm the next
                // wait) and acknowledge RHSC for the same reason. The consumer just
                // calls again.
                self.consume_done_queue().await?;
                self.ack_root_hub_change().await?;
                Ok(ReadReport {
                    length: None,
                    rescued: false,
                })
            }
            WaitOutcome::TimedOut => Ok(ReadReport {
                length,
                rescued: length.is_some(),
            }),
            _ => Ok(ReadReport {
                length,
                rescued: false,
            }),
        }
    }

    /// Park until the root hub signals a status change (RHSC — audit A4), where
    /// events are live; `Unsupported` otherwise (the caller paces its own sweeps).
    /// On a delivery the RHSC latch is acknowledged here — the per-port change bits
    /// stay readable in HcRhPortStatus, so the caller's sweep sees everything; not
    /// acking would leave the level line asserted and turn every later wait into an
    /// instant spurious wake. `TimedOut` is an ordinary answer: re-sweep (free — the
    /// wait already paced the loop) and call again.
    pub async fn wait_port_change(&mut self) -> Result<WaitOutcome, DriverError<R::Error>> {
        if !self.events {
            return Ok(WaitOutcome::Unsupported);
        }
        let outcome = self.io.wait_interrupt().await.map_err(DriverError::Io)?;
        if outcome == WaitOutcome::Delivered {
            self.ack_root_hub_change().await?;
        }
        Ok(outcome)
    }

    /// Acknowledge a pending Root Hub Status Change latch (write-1-to-clear), if one
    /// is set. WDH is deliberately NOT touched here: its one ack site is
    /// [`consume_done_queue`](Self::consume_done_queue), after the head is taken.
    async fn ack_root_hub_change(&mut self) -> Result<(), DriverError<R::Error>> {
        let status = self
            .io
            .read32(reg::HC_INTERRUPT_STATUS)
            .await
            .map_err(DriverError::Io)?;
        if status & bits::INT_RHSC != 0 {
            self.io
                .write32(reg::HC_INTERRUPT_STATUS, bits::INT_RHSC)
                .await
                .map_err(DriverError::Io)?;
        }
        Ok(())
    }

    /// Borrow the underlying region I/O (the shells' diagnostics read raw registers
    /// through this).
    pub fn io(&mut self) -> &mut R {
        &mut self.io
    }
}

/// A tiny fixed-capacity address list for the done-queue validation walk (no_std, no
/// alloc).
mod heapless_chain {
    #[derive(Default)]
    pub struct Chain {
        addresses: [u32; super::DONE_QUEUE_LIMIT],
        length: usize,
    }

    impl Chain {
        pub fn push(&mut self, address: u32) {
            if self.length < self.addresses.len() {
                self.addresses[self.length] = address;
                self.length += 1;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::setup;
    use std::future::Future;

    /// A miniature OHCI model: registers + the DMA arena, faithful to the slices of
    /// the spec the driver exercises — software reset zaps HcFmInterval (THE gotcha:
    /// a driver that forgets the restore fails the assertion below), the frame
    /// counter ticks per register read, port resets complete with PRSC, and handing
    /// the control list over processes TD chains like a QEMU usb-kbd would answer.
    struct MockOhci {
        registers: std::collections::HashMap<u64, u32>,
        arena: Vec<u8>,
        dma_base: u32,
        frame: u32,
        frame_phase: u32,
        device_address: u8,
        /// (request_type, request, value) log, for pinning the request sequence.
        requests: Vec<(u8, u8, u16)>,
        reset_count: u32,
        fm_interval_at_reset: Vec<u32>,
        /// Interrupt-IN reports the mock device has queued (one per periodic visit).
        pending_reports: std::collections::VecDeque<Vec<u8>>,
        /// Hub topology mode: the root device is a 3-port FS hub (the bench
        /// keyboard's shape) with the keyboard behind `hub_child_port`s.
        hub_topology: bool,
        /// Which hub ports report a connected (full-speed) device.
        hub_ports_connected: [bool; 3],
        /// Assigned addresses (0 = not yet addressed).
        hub_address: u8,
        child_address: u8,
        /// The child answers on address 0 after its hub port reset (USB §9.2.6.3).
        child_at_zero: bool,
        /// A PORT_RESET was issued; the next GET_STATUS reports C_PORT_RESET.
        hub_reset_pending: Option<u8>,
        hub_reset_change: Option<u8>,
        /// The internal HcDoneHead accumulator: retired TDs chain here and are only
        /// written back to HccaDoneHead at a frame tick when WDH is clear — the
        /// silicon-faithful gating QEMU does not model.
        done_head: u32,
        /// Whether any retired TD on the accumulator REQUESTED the writeback
        /// interrupt (DelayInterrupt < 0b111). Until one does, the writeback is
        /// deferred indefinitely (§4.3.1.2 — DI_NONE TDs never arm the
        /// interrupt-delay counter): the strict modeling that catches a driver
        /// waiting on WDH while its own TDs suppress it (the area/37 lesson — QEMU
        /// behaves this way and the first event-mode build missed it).
        done_di_armed: bool,
        wdh: bool,
        /// The RHSC interrupt-status latch: set when a port change bit sets, cleared
        /// only by the write-1-to-clear ack (§7.1.4) — the latch outlives the port
        /// bits, which is exactly what the ack discipline under test must handle.
        rhsc: bool,
        /// HcInterruptEnable, with the real set/clear split: writes to ENABLE set
        /// bits, writes to DISABLE clear them (§7.1.5-6).
        interrupt_enable: u32,
        /// Scripted `wait_interrupt` outcomes (empty = `Unsupported`, the polled
        /// default). A scripted `Delivered` is silicon-faithful: the mock runs frames
        /// until an ENABLED cause is actually pending and DOWNGRADES to `TimedOut` if
        /// none materializes — so a driver that forgets the unmask cannot pass an
        /// event-mode test.
        wait_script: std::collections::VecDeque<WaitOutcome>,
        /// Control TDs retired per processing pass (`u32::MAX` = the whole chain in
        /// one shot, QEMU's shape). A finite limit models silicon retiring one TD
        /// per frame, which SPLITS a transfer's done-queue writebacks across ticks
        /// with WDH gating the later batches — the counted-reap discipline's forcing
        /// case. The remainder is re-driven from `tick`.
        control_retire_limit: u32,
        /// The in-flight control request's answer and refusal flag, carried across
        /// limited passes (the Setup stage runs in an earlier pass than the data
        /// stage it answers).
        control_response: Vec<u8>,
        control_refused: bool,
    }

    const DMA_BASE: u32 = 0x4000_0000;

    /// A 3-port full-speed hub's device descriptor (class 09, the bench keyboard's
    /// built-in hub shape; VID 0409 like the NEC silicon on the bench).
    const HUB_DEVICE: [u8; 18] = [
        18, 1, 0x10, 0x01, 9, 0, 0, 8, 0x09, 0x04, 0x5a, 0x00, 0x00, 0x01, 0, 0, 0, 1,
    ];

    /// The hub's configuration: config(9) + interface(9, class 09) + status-change
    /// interrupt endpoint(7), total 25.
    fn hub_config() -> [u8; 25] {
        let mut blob = [0u8; 25];
        blob[..9].copy_from_slice(&[9, 2, 25, 0, 1, 1, 0, 0xe0, 0]);
        blob[9..18].copy_from_slice(&[9, 4, 0, 0, 1, 9, 0, 0, 0]);
        blob[18..25].copy_from_slice(&[7, 5, 0x81, 3, 2, 0, 255]);
        blob
    }

    /// The hub class descriptor: 3 ports, power-good 2*2 = 4 ms (fast tests).
    const HUB_CLASS_DESCRIPTOR: [u8; 9] = [9, 0x29, 3, 0x0d, 0x00, 2, 100, 0x00, 0xff];

    const DEVICE: [u8; 18] = [
        18, 1, 0x00, 0x02, 0, 0, 0, 8, 0x27, 0x06, 0x01, 0x00, 0x00, 0x00, 1, 4, 5, 1,
    ];

    fn kbd_config() -> [u8; 34] {
        let mut blob = [0u8; 34];
        blob[..9].copy_from_slice(&[9, 2, 34, 0, 1, 1, 0, 0xe0, 25]);
        blob[9..18].copy_from_slice(&[9, 4, 0, 0, 1, 3, 1, 1, 0]);
        blob[18..27].copy_from_slice(&[9, 0x21, 0x11, 0x01, 0, 1, 0x22, 63, 0]);
        blob[27..34].copy_from_slice(&[7, 5, 0x81, 3, 8, 0, 10]);
        blob
    }

    impl MockOhci {
        fn new() -> MockOhci {
            let mut registers = std::collections::HashMap::new();
            registers.insert(reg::HC_REVISION, 0x10);
            registers.insert(reg::HC_FM_INTERVAL, bits::FM_INTERVAL_DEFAULT_FI);
            // One port, no power switching (NPS), POTPGT 0 — QEMU pci-ohci's shape.
            registers.insert(reg::HC_RH_DESCRIPTOR_A, bits::RH_A_NPS | 1);
            // Port 1: connected, powered, full-speed.
            registers.insert(reg::rh_port_status(1), bits::PORT_CCS | bits::PORT_PPS);
            MockOhci {
                registers,
                arena: vec![0u8; arena::SIZE as usize],
                dma_base: DMA_BASE,
                frame: 0,
                frame_phase: 0,
                device_address: 0,
                requests: Vec::new(),
                reset_count: 0,
                fm_interval_at_reset: Vec::new(),
                pending_reports: std::collections::VecDeque::new(),
                hub_topology: false,
                hub_ports_connected: [true, false, false],
                hub_address: 0,
                child_address: 0,
                child_at_zero: false,
                hub_reset_pending: None,
                hub_reset_change: None,
                done_head: 0,
                done_di_armed: false,
                wdh: false,
                rhsc: false,
                interrupt_enable: 0,
                wait_script: std::collections::VecDeque::new(),
                control_retire_limit: u32::MAX,
                control_response: Vec::new(),
                control_refused: false,
            }
        }

        fn register(&self, offset: u64) -> u32 {
            *self.registers.get(&offset).unwrap_or(&0)
        }

        /// HcInterruptStatus as the driver reads it: the cause latches this mock
        /// models (WDH from the done writeback, RHSC from port changes).
        fn interrupt_status(&self) -> u32 {
            let mut status = 0;
            if self.wdh {
                status |= bits::INT_WDH;
            }
            if self.rhsc {
                status |= bits::INT_RHSC;
            }
            status
        }

        fn arena_slice(&mut self, offset: u32, len: usize) -> &mut [u8] {
            let start = (offset - self.dma_base) as usize;
            &mut self.arena[start..start + len]
        }

        /// Answer a setup packet the way a real boot device does for the v1 request
        /// set. Returns `false` for requests the device REFUSES (a protocol STALL of
        /// the next stage): HID SET_IDLE, which is optional for mice (HID 1.11
        /// §7.2.4) and which the M3 board's G500 stalls — QEMU's usb-hid accepts it,
        /// which is exactly the QEMU-tolerance this mock must not share.
        fn answer(
            &mut self,
            function_address: u8,
            setup_bytes: [u8; 8],
            buffer: &mut Vec<u8>,
        ) -> bool {
            let request_type = setup_bytes[0];
            let request = setup_bytes[1];
            let value = u16::from_le_bytes([setup_bytes[2], setup_bytes[3]]);
            let index = u16::from_le_bytes([setup_bytes[4], setup_bytes[5]]);
            let length = u16::from_le_bytes([setup_bytes[6], setup_bytes[7]]) as usize;
            self.requests.push((request_type, request, value));
            buffer.clear();

            // Which device is being addressed (hub topology routes by function
            // address; address 0 is whoever is unaddressed — the hub before its
            // SET_ADDRESS, the child after its hub-port reset).
            let target_is_hub = self.hub_topology
                && (function_address == self.hub_address
                    && !(function_address == 0 && self.child_at_zero));

            if target_is_hub {
                match (request_type, request) {
                    (0x80, setup::request::GET_DESCRIPTOR) => {
                        let payload: &[u8] = match (value >> 8) as u8 {
                            setup::descriptor_type::DEVICE => &HUB_DEVICE,
                            setup::descriptor_type::CONFIGURATION => &hub_config(),
                            _ => &[],
                        };
                        buffer.extend_from_slice(&payload[..length.min(payload.len())]);
                    }
                    (0x00, setup::request::SET_ADDRESS) => {
                        self.hub_address = value as u8;
                    }
                    (0x00, setup::request::SET_CONFIGURATION) => {}
                    (0xa0, setup::request::GET_DESCRIPTOR) => {
                        // Hub class descriptor.
                        buffer.extend_from_slice(
                            &HUB_CLASS_DESCRIPTOR[..length.min(HUB_CLASS_DESCRIPTOR.len())],
                        );
                    }
                    (0x23, 3) => {
                        // SET_FEATURE(port): PORT_POWER accepted silently; PORT_RESET
                        // arms the change bit and surfaces the child at address 0.
                        if value == hub::FEATURE_PORT_RESET {
                            self.hub_reset_pending = Some(index as u8);
                        }
                    }
                    (0x23, 1) => {
                        // CLEAR_FEATURE(C_PORT_RESET).
                        if value == hub::FEATURE_C_PORT_RESET {
                            self.hub_reset_change = None;
                        }
                    }
                    (0xa3, 0) => {
                        // GET_STATUS(port): connected per the test topology, FS,
                        // powered; C_PORT_RESET after a pending reset matured.
                        let port = index as usize;
                        let connected = self
                            .hub_ports_connected
                            .get(port.wrapping_sub(1))
                            .copied()
                            .unwrap_or(false);
                        if self.hub_reset_pending == Some(port as u8) {
                            self.hub_reset_pending = None;
                            self.hub_reset_change = Some(port as u8);
                            self.child_at_zero = true;
                        }
                        let mut status: u16 = 1 << 8; // powered
                        if connected {
                            status |= 1 << 0;
                        }
                        let mut change: u16 = 0;
                        if self.hub_reset_change == Some(port as u8) {
                            status |= 1 << 1; // enabled after reset
                            change |= 1 << 4; // C_PORT_RESET
                        }
                        buffer.extend_from_slice(&status.to_le_bytes());
                        buffer.extend_from_slice(&change.to_le_bytes());
                        buffer.truncate(length);
                    }
                    _ => {}
                }
                return true;
            }

            // The keyboard (the only device in flat mode; the child in hub mode).
            match (request_type, request) {
                (0x80, setup::request::GET_DESCRIPTOR) => {
                    let payload: &[u8] = match (value >> 8) as u8 {
                        setup::descriptor_type::DEVICE => &DEVICE,
                        setup::descriptor_type::CONFIGURATION => &kbd_config(),
                        _ => &[],
                    };
                    buffer.extend_from_slice(&payload[..length.min(payload.len())]);
                }
                (0x00, setup::request::SET_ADDRESS) => {
                    if self.hub_topology {
                        self.child_address = value as u8;
                        self.child_at_zero = false;
                    } else {
                        self.device_address = value as u8;
                    }
                }
                (0x21, setup::request::HID_SET_IDLE) => return false,
                _ => {}
            }
            true
        }

        /// Process the control list: walk the ED's TD chain to its tail, answering
        /// the setup and filling IN buffers, retiring each TD with NoError and
        /// pushing it on the done queue (newest first), then write back HccaDoneHead.
        fn process_control_list(&mut self) {
            let ed_offset = (self.register(reg::HC_CONTROL_HEAD_ED) - self.dma_base) as u64;
            let mut ed_bytes = [0u8; 16];
            ed_bytes.copy_from_slice(self.arena_slice(self.dma_base + ed_offset as u32, 16));
            let mut ed = EndpointDescriptor::decode(&ed_bytes);
            let mut retired_this_pass = 0u32;
            while ed.head != ed.tail && retired_this_pass < self.control_retire_limit {
                let td_address = ed.head;
                let mut td_bytes = [0u8; 16];
                td_bytes.copy_from_slice(self.arena_slice(td_address, 16));
                let mut td = TransferDescriptor::decode(&td_bytes);
                match td.pid {
                    TdPid::Setup => {
                        let mut setup_bytes = [0u8; 8];
                        setup_bytes.copy_from_slice(self.arena_slice(td.current_buffer, 8));
                        let mut answered = Vec::new();
                        self.control_refused =
                            !self.answer(ed.function_address, setup_bytes, &mut answered);
                        self.control_response = answered;
                        td.condition_code = ConditionCode::NoError;
                    }
                    TdPid::In | TdPid::Out if self.control_refused => {
                        // The device refuses the request with a protocol STALL of the
                        // next stage (USB 2.0 §8.5.3.4): retire this TD with the Stall
                        // condition, halt the ED, stop processing its list.
                        td.condition_code = ConditionCode::Stall;
                        ed.halted = true;
                    }
                    TdPid::In => {
                        if td.current_buffer != 0 {
                            let capacity = (td.buffer_end - td.current_buffer + 1) as usize;
                            let send = self.control_response.len().min(capacity);
                            let start = td.current_buffer;
                            let payload: Vec<u8> = self.control_response[..send].to_vec();
                            self.arena_slice(start, send).copy_from_slice(&payload);
                            // CBP: 0 if the buffer was filled exactly, else next byte.
                            td.current_buffer = if send == capacity {
                                0
                            } else {
                                td.current_buffer + send as u32
                            };
                        }
                        td.condition_code = ConditionCode::NoError;
                    }
                    TdPid::Out => {
                        td.condition_code = ConditionCode::NoError;
                    }
                }
                ed.head = td.next & !0xf;
                // Retire onto the internal done accumulator (newest first); the
                // writeback to HccaDoneHead happens at a frame tick, WDH permitting
                // AND only once a retired TD has requested the interrupt
                // (`flush_done`) — the silicon timing, not QEMU's instant one.
                if td.delay_interrupt != crate::schedule::DI_NONE {
                    self.done_di_armed = true;
                }
                td.next = self.done_head;
                let encoded = td.encode();
                self.arena_slice(td_address, 16).copy_from_slice(&encoded);
                self.done_head = td_address;
                retired_this_pass += 1;
                if ed.halted {
                    break;
                }
            }
            let encoded_ed = ed.encode();
            self.arena_slice(self.dma_base + ed_offset as u32, 16)
                .copy_from_slice(&encoded_ed);
        }

        /// One frame tick: advance HcFmNumber, run the periodic schedule, attempt the
        /// done-queue writeback. Called from register AND dma accesses (the driver's
        /// polls are its only clock against this mock, as against silicon).
        fn tick(&mut self) {
            self.frame_phase += 1;
            if !self.frame_phase.is_multiple_of(4) {
                return;
            }
            self.frame = (self.frame + 1) & 0xffff;
            if self.control_retire_limit != u32::MAX {
                // Per-frame retirement model: keep draining the control ED one
                // limited pass per frame (a no-op once head == tail or halted).
                self.process_control_list();
            }
            self.process_periodic();
            self.flush_done();
        }

        /// The periodic schedule, STRICT the way silicon is and QEMU is not: it runs
        /// only when PeriodicListEnable is set AND HcPeriodicStart is non-zero (a
        /// zero PeriodicStart means the periodic region of the frame never begins —
        /// OHCI 1.0a §7.3.4; QEMU's model famously ignores the register, which is
        /// exactly how a missing write survives the QEMU lane) AND the HCCA pointer
        /// and the frame's interrupt-table entry are programmed.
        fn process_periodic(&mut self) {
            let control = self.register(reg::HC_CONTROL);
            if control & bits::CONTROL_PLE == 0 {
                return;
            }
            if self.register(reg::HC_PERIODIC_START) == 0 {
                return; // the strict check: no PeriodicStart, no periodic traffic
            }
            let hcca = self.register(reg::HC_HCCA);
            if hcca == 0 {
                return;
            }
            let entry_offset = (hcca - self.dma_base) as usize
                + hcca::interrupt_table_entry(u64::from(self.frame)) as usize;
            let ed_address = u32::from_le_bytes(
                self.arena[entry_offset..entry_offset + 4]
                    .try_into()
                    .unwrap(),
            );
            if ed_address == 0 {
                return;
            }
            let mut ed_bytes = [0u8; 16];
            ed_bytes.copy_from_slice(self.arena_slice(ed_address, 16));
            let mut ed = EndpointDescriptor::decode(&ed_bytes);
            if ed.skip || ed.halted || ed.head == ed.tail {
                return;
            }
            // The device NAKs when it has nothing to report.
            let Some(report) = self.pending_reports.pop_front() else {
                return;
            };
            let td_address = ed.head;
            let mut td_bytes = [0u8; 16];
            td_bytes.copy_from_slice(self.arena_slice(td_address, 16));
            let mut td = TransferDescriptor::decode(&td_bytes);
            let capacity = (td.buffer_end - td.current_buffer + 1) as usize;
            let send = report.len().min(capacity);
            let start = td.current_buffer;
            self.arena_slice(start, send)
                .copy_from_slice(&report[..send]);
            td.current_buffer = if send == capacity {
                0
            } else {
                td.current_buffer + send as u32
            };
            td.condition_code = ConditionCode::NoError;
            ed.head = td.next & !0xf;
            ed.toggle_carry = !ed.toggle_carry;
            if td.delay_interrupt != crate::schedule::DI_NONE {
                self.done_di_armed = true;
            }
            td.next = self.done_head;
            let encoded = td.encode();
            self.arena_slice(td_address, 16).copy_from_slice(&encoded);
            self.done_head = td_address;
            let encoded_ed = ed.encode();
            self.arena_slice(ed_address, 16)
                .copy_from_slice(&encoded_ed);
        }

        /// HccaDoneHead writeback, gated on WDH exactly as §5.2.9 describes (the HC
        /// only writes when the driver has acknowledged the previous batch) AND on a
        /// retired TD having requested the interrupt (§4.3.1.2: an accumulator of
        /// pure DI_NONE TDs never arms the interrupt-delay counter, so the writeback
        /// — and the WDH edge — is deferred indefinitely; QEMU does the same, which
        /// is how an event-mode driver with DI_NONE report TDs starves).
        fn flush_done(&mut self) {
            if self.wdh || self.done_head == 0 || !self.done_di_armed {
                return;
            }
            let head_offset = (arena::HCCA + hcca::DONE_HEAD) as usize;
            self.arena[head_offset..head_offset + 4].copy_from_slice(&self.done_head.to_le_bytes());
            self.done_head = 0;
            self.done_di_armed = false;
            self.wdh = true;
        }
    }

    #[derive(Debug, PartialEq, Eq, Clone, Copy)]
    struct NoError;

    impl RegionIo for MockOhci {
        type Error = NoError;

        async fn read32(&mut self, offset: u64) -> Result<u32, NoError> {
            // Every register access is a tick: the frame counter advances, periodic
            // processing and the done writeback get their chances — the driver's
            // polls are its only clock, against this mock as against silicon.
            self.tick();
            if offset == reg::HC_FM_NUMBER {
                return Ok(self.frame);
            }
            if offset == reg::HC_INTERRUPT_STATUS {
                return Ok(self.interrupt_status());
            }
            if offset == reg::HC_INTERRUPT_ENABLE {
                return Ok(self.interrupt_enable);
            }
            Ok(self.register(offset))
        }

        async fn write32(&mut self, offset: u64, value: u32) -> Result<(), NoError> {
            match offset {
                reg::HC_COMMAND_STATUS => {
                    if value & bits::CMD_HCR != 0 {
                        // Software reset: HCR self-clears immediately and HcFmInterval
                        // reverts to its default — the restore gotcha under test.
                        self.reset_count += 1;
                        self.fm_interval_at_reset
                            .push(self.register(reg::HC_FM_INTERVAL));
                        self.registers
                            .insert(reg::HC_FM_INTERVAL, bits::FM_INTERVAL_DEFAULT_FI);
                    }
                    if value & bits::CMD_CLF != 0 {
                        self.process_control_list();
                    }
                }
                reg::HC_INTERRUPT_STATUS => {
                    // Write-1-to-clear: acknowledging WDH re-permits the done-queue
                    // writeback (§5.2.9); acknowledging RHSC drops the latch (§7.1.4).
                    if value & bits::INT_WDH != 0 {
                        self.wdh = false;
                    }
                    if value & bits::INT_RHSC != 0 {
                        self.rhsc = false;
                    }
                }
                reg::HC_INTERRUPT_ENABLE => {
                    // Set-bits semantics (§7.1.5).
                    self.interrupt_enable |= value;
                }
                reg::HC_INTERRUPT_DISABLE => {
                    // Clear-bits semantics (§7.1.6).
                    self.interrupt_enable &= !value;
                }
                offset if offset == reg::rh_port_status(1) => {
                    let mut status = self.register(offset);
                    if value & bits::PORT_PRS != 0 {
                        // The controller times the reset itself: complete instantly,
                        // port enabled — a port change, so the RHSC latch sets
                        // (§7.1.4: RHSC reflects a change in HcRhPortStatus).
                        status |= bits::PORT_PRSC | bits::PORT_PES;
                        self.rhsc = true;
                    }
                    if value & bits::PORT_PRSC != 0 {
                        status &= !bits::PORT_PRSC;
                    }
                    self.registers.insert(offset, status);
                }
                _ => {
                    self.registers.insert(offset, value);
                }
            }
            Ok(())
        }

        fn dma_base(&self) -> u32 {
            self.dma_base
        }

        fn dma_write(&mut self, offset: u64, bytes: &[u8]) {
            let start = offset as usize;
            self.arena[start..start + bytes.len()].copy_from_slice(bytes);
        }

        fn dma_read(&mut self, offset: u64, buf: &mut [u8]) {
            // DMA polls tick the frame too: poll_interrupt never reads HcFmNumber,
            // so without this the periodic schedule would only run during register
            // waits — and the round-trip test below could never retire a TD.
            self.tick();
            let start = offset as usize;
            buf.copy_from_slice(&self.arena[start..start + buf.len()]);
        }

        async fn wait_interrupt(&mut self) -> Result<WaitOutcome, NoError> {
            match self.wait_script.pop_front() {
                // No script = no interrupt surface (the polled default).
                None => Ok(WaitOutcome::Unsupported),
                Some(WaitOutcome::Delivered) => {
                    // Silicon-faithful delivery: run frames until an UNMASKED cause
                    // is pending under MasterInterruptEnable; a driver that never
                    // unmasked sees the wait expire instead — the masked-interrupt
                    // regression QEMU's instant model would hide.
                    for _ in 0..50_000 {
                        let deliverable = self.interrupt_enable & bits::INT_MIE != 0
                            && self.interrupt_status() & self.interrupt_enable != 0;
                        if deliverable {
                            return Ok(WaitOutcome::Delivered);
                        }
                        self.tick();
                    }
                    Ok(WaitOutcome::TimedOut)
                }
                Some(outcome) => Ok(outcome),
            }
        }
    }

    /// Drive a ready-everywhere future to completion (every await in the driver is
    /// immediately ready against the mock, so a bounded poll loop suffices).
    fn run<T>(future: impl Future<Output = T>) -> T {
        let mut pinned = std::pin::pin!(future);
        let waker = std::task::Waker::noop();
        let mut context = std::task::Context::from_waker(waker);
        for _ in 0..1_000_000 {
            if let std::task::Poll::Ready(value) = pinned.as_mut().poll(&mut context) {
                return value;
            }
        }
        panic!("the driver future did not complete against the always-ready mock");
    }

    #[test]
    fn bring_up_restores_fm_interval_after_reset() {
        let mut driver = Ohci::new(MockOhci::new());
        let info = run(driver.bring_up()).unwrap();
        assert_eq!(info.revision, 0x10);
        assert_eq!(info.ports, 1);

        let mock = driver.io();
        assert_eq!(mock.reset_count, 1);
        // THE gotcha: after the reset the driver must rewrite HcFmInterval with the
        // saved interval, the recomputed FSMPS, and a flipped FIT — leaving the
        // default in place is the classic OHCI bring-up bug.
        let expected =
            fm_interval_restore(bits::FM_INTERVAL_DEFAULT_FI, bits::FM_INTERVAL_DEFAULT_FI);
        assert_eq!(mock.register(reg::HC_FM_INTERVAL), expected);
        assert_eq!(
            mock.register(reg::HC_PERIODIC_START),
            periodic_start(bits::FM_INTERVAL_DEFAULT_FI)
        );
        // Operational, HCCA programmed, interrupts fully masked.
        assert_eq!(
            mock.register(reg::HC_CONTROL) & bits::CONTROL_HCFS_MASK,
            bits::CONTROL_HCFS_OPERATIONAL
        );
        assert_eq!(mock.register(reg::HC_HCCA), DMA_BASE + arena::HCCA as u32);
    }

    #[test]
    fn a_wrong_revision_is_a_typed_refusal() {
        let mut mock = MockOhci::new();
        mock.registers.insert(reg::HC_REVISION, 0x20); // EHCI-ish nonsense
        let mut driver = Ohci::new(mock);
        assert_eq!(
            run(driver.bring_up()),
            Err(DriverError::NotOhci { revision: 0x20 })
        );
    }

    #[test]
    fn port_status_decodes_and_bounds() {
        let mut driver = Ohci::new(MockOhci::new());
        run(driver.bring_up()).unwrap();
        let status = run(driver.port_status(1)).unwrap();
        assert!(status.connected && status.powered);
        assert!(!status.low_speed);
        assert_eq!(run(driver.port_status(0)), Err(DriverError::NoSuchPort));
        assert_eq!(run(driver.port_status(2)), Err(DriverError::NoSuchPort));
    }

    #[test]
    fn attach_runs_the_full_chain_against_the_mock_keyboard() {
        let mut driver = Ohci::new(MockOhci::new());
        run(driver.bring_up()).unwrap();
        let mut config = [0u8; 256];
        let (attached, config_len) = run(driver.attach(1, &mut config)).unwrap();
        assert_eq!(attached.enumerated.address, 1);
        assert_eq!(attached.enumerated.max_packet_ep0, 8);
        assert_eq!(attached.enumerated.device.vendor_id, 0x0627);
        assert_eq!(attached.enumerated.device.product_id, 0x0001);
        assert_eq!(config_len, 34);
        assert_eq!(&config[..34], &kbd_config());

        // The device saw exactly the textbook sequence, addressed mid-chain.
        let mock = driver.io();
        assert_eq!(mock.device_address, 1);
        assert_eq!(
            mock.requests,
            vec![
                (0x80, setup::request::GET_DESCRIPTOR, 0x0100), // head, addr 0
                (0x00, setup::request::SET_ADDRESS, 0x0001),
                (0x80, setup::request::GET_DESCRIPTOR, 0x0100), // full device
                (0x80, setup::request::GET_DESCRIPTOR, 0x0200), // config head
                (0x80, setup::request::GET_DESCRIPTOR, 0x0200), // full config
            ]
        );

        // And the parsed blob finds the boot keyboard endpoint hidcheck needs.
        let boot = crate::descriptor::find_boot_interface(&config[..config_len]).unwrap();
        assert_eq!(boot.endpoint.address, 0x81);
        assert_eq!(boot.endpoint.interval, 10);
    }

    #[test]
    fn attach_refuses_an_empty_port_typed() {
        let mut mock = MockOhci::new();
        mock.registers
            .insert(reg::rh_port_status(1), bits::PORT_PPS); // powered, nothing there
        let mut driver = Ohci::new(mock);
        run(driver.bring_up()).unwrap();
        let mut config = [0u8; 256];
        assert_eq!(
            run(driver.attach(1, &mut config)),
            Err(DriverError::NotConnected)
        );
    }

    #[test]
    fn bring_up_programs_periodic_start_and_the_mock_enforces_it() {
        // The M3-fix hardening (board round: NotAccessed masked the real failure and
        // pointed suspicion at the periodic plumbing): pin that bring_up programs
        // HcPeriodicStart, that open_interrupt_in sets PLE, and that the mock is
        // STRICT — PeriodicStart=0 means NO periodic traffic, so a regression that
        // QEMU would tolerate fails loudly here.
        let mut driver = Ohci::new(MockOhci::new());
        run(driver.bring_up()).unwrap();
        let expected = periodic_start(bits::FM_INTERVAL_DEFAULT_FI);
        assert_eq!(driver.io().register(reg::HC_PERIODIC_START), expected);

        run(driver.open_interrupt_in(1, false, 1, 8, 1)).unwrap();
        assert_ne!(
            driver.io().register(reg::HC_CONTROL) & bits::CONTROL_PLE,
            0,
            "open_interrupt_in must set PeriodicListEnable"
        );

        // Sabotage PeriodicStart: the queued report must NOT flow.
        driver.io().registers.insert(reg::HC_PERIODIC_START, 0);
        driver.io().pending_reports.push_back(vec![0, 3, 0xfe, 0]);
        let mut report = [0u8; 8];
        for _ in 0..200 {
            assert_eq!(
                run(driver.poll_interrupt(&mut report)).unwrap(),
                None,
                "the strict mock must refuse periodic traffic with PeriodicStart=0"
            );
        }

        // Restore the register: the same queued report flows through.
        driver
            .io()
            .registers
            .insert(reg::HC_PERIODIC_START, expected);
        let mut received = None;
        for _ in 0..200 {
            if let Some(length) = run(driver.poll_interrupt(&mut report)).unwrap() {
                received = Some(length);
                break;
            }
        }
        assert_eq!(received, Some(4));
        assert_eq!(&report[..4], &[0, 3, 0xfe, 0]);
    }

    #[test]
    fn interrupt_round_trip_rearms_and_streams() {
        // The re-arm path (ping-pong TD slots, tail publish) was QEMU-only before
        // this round: stream two distinct mouse reports through the mock's periodic
        // schedule and verify both arrive in order through the re-armed endpoint.
        let mut driver = Ohci::new(MockOhci::new());
        run(driver.bring_up()).unwrap();
        run(driver.open_interrupt_in(1, false, 1, 8, 1)).unwrap();

        let mut report = [0u8; 8];
        for (expected, label) in [([0u8, 3, 0xfe, 0], "first"), ([1u8, 0xff, 2, 0], "second")] {
            driver.io().pending_reports.push_back(expected.to_vec());
            let mut received = None;
            for _ in 0..200 {
                if let Some(length) = run(driver.poll_interrupt(&mut report)).unwrap() {
                    received = Some(length);
                    break;
                }
            }
            assert_eq!(received, Some(4), "{label} report must arrive");
            assert_eq!(&report[..4], &expected, "{label} report content");
        }
    }

    #[test]
    fn a_stalled_set_idle_reports_stall_not_not_accessed() {
        // The M3 board bug, pinned: the mock device STALLs HID SET_IDLE (optional
        // for mice, HID 1.11 §7.2.4 — the G500 does exactly this); the driver must
        // judge the status stage's REAL condition code, not mask the halt as
        // NotAccessed. This test fails against the pre-fix judge order.
        let mut driver = Ohci::new(MockOhci::new());
        run(driver.bring_up()).unwrap();
        let result = run(driver.control(0, 8, false, setup::hid_set_idle_indefinite(0), &mut []));
        assert_eq!(result, Err(DriverError::Stall));

        // And the next transfer on the rebuilt ED works — a protocol stall does not
        // wedge endpoint zero.
        let mut data = [0u8; 18];
        let received = run(driver.control(
            0,
            8,
            false,
            setup::get_descriptor(setup::descriptor_type::DEVICE, 0, 18),
            &mut data,
        ))
        .unwrap();
        assert_eq!(received, 18);
    }

    #[test]
    fn hub_traversal_enumerates_the_child_keyboard() {
        // The demo chain (bench facts: keyboard at FULL speed on port 1 of its own
        // 3-port FS hub): attach the root device -> it is a hub -> traverse one
        // level -> the child enumerates with the standard machine at hub+16, and
        // its config parses down to the boot keyboard endpoint.
        let mut mock = MockOhci::new();
        mock.hub_topology = true;
        let mut driver = Ohci::new(mock);
        run(driver.bring_up()).unwrap();

        let mut hub_config_blob = [0u8; 256];
        let (hub_attached, hub_len) = run(driver.attach(1, &mut hub_config_blob)).unwrap();
        assert_eq!(hub_attached.enumerated.device.class, hub::CLASS_HUB);
        assert_eq!(hub_attached.enumerated.address, 1);

        let mut child_config = [0u8; 256];
        let (child, child_len) = run(driver.attach_hub_child(
            hub_attached.enumerated.address,
            hub_attached.enumerated.max_packet_ep0,
            hub_attached.low_speed,
            hub_attached.enumerated.device.class,
            &hub_config_blob[..hub_len],
            &mut child_config,
        ))
        .unwrap();
        assert_eq!(child.enumerated.address, 17); // hub address + 16
        assert!(!child.low_speed);
        assert_eq!(child.enumerated.device.vendor_id, 0x0627);
        let boot = crate::descriptor::find_boot_interface(&child_config[..child_len]).unwrap();
        assert_eq!(boot.endpoint.address, 0x81);

        // The hub conversation happened in chapter-11 vocabulary: configuration,
        // class descriptor, three port powers, a port reset + its acknowledge.
        let mock = driver.io();
        assert_eq!(mock.hub_address, 1);
        assert_eq!(mock.child_address, 17);
        assert!(mock.requests.contains(&(0xa0, 6, 0x2900))); // hub descriptor
        assert!(mock.requests.contains(&(0x23, 3, hub::FEATURE_PORT_POWER)));
        assert!(mock.requests.contains(&(0x23, 3, hub::FEATURE_PORT_RESET)));
        assert!(
            mock.requests
                .contains(&(0x23, 1, hub::FEATURE_C_PORT_RESET))
        );
    }

    #[test]
    fn hub_traversal_refusals_are_typed() {
        // No child connected: typed, names the situation.
        let mut mock = MockOhci::new();
        mock.hub_topology = true;
        mock.hub_ports_connected = [false, false, false];
        let mut driver = Ohci::new(mock);
        run(driver.bring_up()).unwrap();
        let mut blob = [0u8; 256];
        let (hub_attached, hub_len) = run(driver.attach(1, &mut blob)).unwrap();
        let mut child_config = [0u8; 256];
        assert_eq!(
            run(driver.attach_hub_child(
                hub_attached.enumerated.address,
                hub_attached.enumerated.max_packet_ep0,
                hub_attached.low_speed,
                hub_attached.enumerated.device.class,
                &blob[..hub_len],
                &mut child_config,
            )),
            Err(DriverError::Hub("no device connected behind the hub"))
        );

        // Two children: the one-child demo scope refuses typed.
        let mut mock = MockOhci::new();
        mock.hub_topology = true;
        mock.hub_ports_connected = [true, true, false];
        let mut driver = Ohci::new(mock);
        run(driver.bring_up()).unwrap();
        let (hub_attached, hub_len) = run(driver.attach(1, &mut blob)).unwrap();
        assert_eq!(
            run(driver.attach_hub_child(
                hub_attached.enumerated.address,
                hub_attached.enumerated.max_packet_ep0,
                hub_attached.low_speed,
                hub_attached.enumerated.device.class,
                &blob[..hub_len],
                &mut child_config,
            )),
            Err(DriverError::Hub(
                "multiple devices behind the hub (one-child demo scope)"
            ))
        );

        // A non-hub device refuses immediately.
        let mut driver = Ohci::new(MockOhci::new());
        run(driver.bring_up()).unwrap();
        let (kbd, kbd_len) = run(driver.attach(1, &mut blob)).unwrap();
        assert_eq!(
            run(driver.attach_hub_child(
                kbd.enumerated.address,
                kbd.enumerated.max_packet_ep0,
                kbd.low_speed,
                kbd.enumerated.device.class,
                &blob[..kbd_len],
                &mut child_config,
            )),
            Err(DriverError::Hub("the attached device is not a hub"))
        );
    }

    #[test]
    fn event_mode_reaps_split_writebacks_before_returning() {
        // Silicon-shaped timing: one control TD retires per frame, so the
        // GET_DESCRIPTOR chain's writebacks SPLIT — the first batch flushes at the
        // next tick and sets WDH, the rest is held behind it. The counted reap must
        // collect every batch before control() returns; a first-non-empty reap
        // would return with a writeback still owed, and the next transfer's slot
        // rewrites would then be walked as a done chain (the recycled-slot class).
        let mut driver = Ohci::new(MockOhci::new());
        run(driver.bring_up()).unwrap();
        run(driver.enable_events()).unwrap();
        driver.io().control_retire_limit = 1;
        let mut data = [0u8; 64];
        let received = run(driver.control(
            0,
            8,
            false,
            setup::get_descriptor(setup::descriptor_type::DEVICE, 0, 64),
            &mut data,
        ))
        .unwrap();
        assert_eq!(received, 18, "the split passes still answer the request");
        assert_eq!(&data[..18], &DEVICE);
        // Airtight: nothing may still be owed when control() returns — the
        // accumulator is empty, HccaDoneHead carries no unconsumed chain, and WDH
        // is acknowledged, so every TD slot is safely reusable.
        assert_eq!(driver.io().done_head, 0, "accumulator drained");
        let head_offset = (arena::HCCA + hcca::DONE_HEAD) as usize;
        assert_eq!(
            &driver.io().arena[head_offset..head_offset + 4],
            &[0u8; 4],
            "no unconsumed HccaDoneHead writeback"
        );
        assert!(!driver.io().wdh, "WDH acknowledged after the counted reap");
        // And the very next transfer over the same slots stays clean.
        let received = run(driver.control(
            0,
            8,
            false,
            setup::get_descriptor(setup::descriptor_type::DEVICE, 0, 64),
            &mut data,
        ))
        .unwrap();
        assert_eq!(received, 18);
    }

    #[test]
    fn control_in_copies_short_reads_out() {
        let mut driver = Ohci::new(MockOhci::new());
        run(driver.bring_up()).unwrap();
        // Ask for 64 bytes of device descriptor; the device has 18 — a legal short
        // read that must come back as 18, not an error (R bit set).
        let mut data = [0u8; 64];
        let received = run(driver.control(
            0,
            8,
            false,
            setup::get_descriptor(setup::descriptor_type::DEVICE, 0, 64),
            &mut data,
        ))
        .unwrap();
        assert_eq!(received, 18);
        assert_eq!(&data[..18], &DEVICE);
    }

    #[test]
    fn bring_up_masks_everything_and_enable_events_unmasks_exactly_wdh_rhsc_mie() {
        let mut driver = Ohci::new(MockOhci::new());
        run(driver.bring_up()).unwrap();
        // The polled shape is the bring-up default: everything masked, events off.
        assert_eq!(driver.io().interrupt_enable, 0);
        assert!(!driver.events());

        run(driver.enable_events()).unwrap();
        assert!(driver.events());
        // Exactly the two consumed causes plus the master gate — nothing else (SF in
        // particular would interrupt every millisecond).
        assert_eq!(
            driver.io().interrupt_enable,
            bits::INT_MIE | bits::INT_WDH | bits::INT_RHSC
        );
    }

    #[test]
    fn the_mock_refuses_delivery_while_the_causes_are_masked() {
        // The unmask discipline, pinned at the mock level: a scripted delivery
        // DOWNGRADES to a timeout while HcInterruptEnable is masked — the regression
        // QEMU's instant model would hide (a driver that forgets `enable_events`
        // would pass against a mock that delivered regardless).
        let mut driver = Ohci::new(MockOhci::new());
        run(driver.bring_up()).unwrap();
        run(driver.open_interrupt_in(1, false, 1, 8, 10)).unwrap();
        driver.io().pending_reports.push_back(vec![0, 0, 0x0b, 0]);
        driver.io().wait_script.push_back(WaitOutcome::Delivered);
        assert_eq!(
            run(driver.io().wait_interrupt()),
            Ok(WaitOutcome::TimedOut),
            "a masked cause must never deliver"
        );
    }

    #[test]
    fn event_mode_read_parks_on_wdh_and_acks_once() {
        // The A1 steady-state path: wait → the periodic schedule retires the TD and
        // the done writeback raises WDH → drain takes the report and acks WDH
        // exactly once (consume_done_queue), so the next writeback can happen.
        let mut driver = Ohci::new(MockOhci::new());
        run(driver.bring_up()).unwrap();
        run(driver.enable_events()).unwrap();
        run(driver.open_interrupt_in(1, false, 1, 8, 10)).unwrap();

        let mut report = [0u8; 8];
        for (expected, label) in [([0u8, 0, 0x0b, 0], "first"), ([0u8, 0, 0, 0], "second")] {
            driver.io().pending_reports.push_back(expected.to_vec());
            driver.io().wait_script.push_back(WaitOutcome::Delivered);
            let read = run(driver.read_report(&mut report)).unwrap();
            assert_eq!(
                read.length,
                Some(4),
                "{label} report must arrive via the wait"
            );
            assert!(!read.rescued, "{label}: the event path is not a rescue");
            assert_eq!(&report[..4], &expected, "{label} report content");
            assert!(
                !driver.io().wdh,
                "{label}: WDH must be acknowledged after the drain (one ack site)"
            );
        }
    }

    #[test]
    fn a_timed_out_wait_that_finds_work_reports_the_rescue() {
        // The liveness arm (owner doctrine): a report that retired but whose
        // interrupt never arrived is found by the post-timeout drain and FLAGGED —
        // the shell turns the flag into a loud `liveness:` line.
        let mut driver = Ohci::new(MockOhci::new());
        run(driver.bring_up()).unwrap();
        run(driver.enable_events()).unwrap();
        run(driver.open_interrupt_in(1, false, 1, 8, 10)).unwrap();

        // Retire the report and let the done writeback happen BEFORE the wait.
        driver.io().pending_reports.push_back(vec![0, 0, 0x0b, 0]);
        for _ in 0..400 {
            driver.io().tick();
        }
        assert!(
            driver.io().wdh,
            "the writeback must be pending before the wait"
        );

        driver.io().wait_script.push_back(WaitOutcome::TimedOut);
        let mut report = [0u8; 8];
        let read = run(driver.read_report(&mut report)).unwrap();
        assert_eq!(read.length, Some(4));
        assert!(
            read.rescued,
            "work found after a timed-out wait is a rescue"
        );
    }

    #[test]
    fn a_spurious_delivery_acks_rhsc_instead_of_storming() {
        // A delivery whose cause is RHSC (port change), not WDH: the drain finds no
        // report, the RHSC latch is acknowledged (otherwise the level line stays
        // asserted and every later wait becomes an instant spurious wake), and the
        // answer is empty — the consumer just calls again.
        let mut driver = Ohci::new(MockOhci::new());
        run(driver.bring_up()).unwrap();
        run(driver.enable_events()).unwrap();
        run(driver.open_interrupt_in(1, false, 1, 8, 10)).unwrap();

        driver.io().rhsc = true; // a connect-ish change latched
        driver.io().wait_script.push_back(WaitOutcome::Delivered);
        let mut report = [0u8; 8];
        let read = run(driver.read_report(&mut report)).unwrap();
        assert_eq!(read.length, None);
        assert!(!read.rescued);
        assert!(
            !driver.io().rhsc,
            "the RHSC latch must be acknowledged on the empty-drain path"
        );
    }

    #[test]
    fn polled_mode_read_is_one_short_poll() {
        // Without enable_events the read path is byte-for-byte the old shape: one
        // short poll, no wait call (the empty wait_script would answer Unsupported,
        // but the polled path must not even ask).
        let mut driver = Ohci::new(MockOhci::new());
        run(driver.bring_up()).unwrap();
        run(driver.open_interrupt_in(1, false, 1, 8, 10)).unwrap();
        let mut report = [0u8; 8];
        let read = run(driver.read_report(&mut report)).unwrap();
        assert_eq!(read.length, None);
        assert!(!read.rescued);

        driver.io().pending_reports.push_back(vec![0, 0, 0x0b, 0]);
        let mut received = None;
        for _ in 0..200 {
            let read = run(driver.read_report(&mut report)).unwrap();
            if read.length.is_some() {
                received = read.length;
                break;
            }
        }
        assert_eq!(received, Some(4));
    }

    #[test]
    fn an_event_mode_stall_still_reports_stall_and_reaps_cleanly() {
        // The M3 SET_IDLE corner re-pinned for event mode: the refused request's
        // status stage STALLs, the judge reports the REAL condition code, and the
        // halted transfer's writeback (its TDs request the interrupt now) is reaped
        // — so endpoint zero is not wedged and no unacknowledged WDH leaks into the
        // first endpoint wait.
        let mut driver = Ohci::new(MockOhci::new());
        run(driver.bring_up()).unwrap();
        run(driver.enable_events()).unwrap();
        let result = run(driver.control(0, 8, false, setup::hid_set_idle_indefinite(0), &mut []));
        assert_eq!(result, Err(DriverError::Stall));

        // The next transfer on the rebuilt ED works and leaves the done queue clean.
        let mut data = [0u8; 18];
        let received = run(driver.control(
            0,
            8,
            false,
            setup::get_descriptor(setup::descriptor_type::DEVICE, 0, 18),
            &mut data,
        ))
        .unwrap();
        assert_eq!(received, 18);
        assert_eq!(driver.io().done_head, 0, "every writeback reaped");
        assert!(!driver.io().wdh, "no unacknowledged WDH left behind");
    }

    #[test]
    fn event_mode_enumeration_reaps_writebacks_and_then_streams() {
        // The full event-mode life cycle, the shape that failed live under QEMU:
        // enumeration's control transfers retire dozens of TDs through four reused
        // slots, and each transfer must REAP its own writeback (request it via DI,
        // wait it out, consume + ack) — a deferred chain flushing later would point
        // at recycled slots (the corrupt-done-queue failure). Then the interrupt
        // endpoint streams via WDH waits on the same controller state.
        let mut driver = Ohci::new(MockOhci::new());
        run(driver.bring_up()).unwrap();
        run(driver.enable_events()).unwrap();

        let mut config = [0u8; 256];
        let (attached, _) = run(driver.attach(1, &mut config)).unwrap();
        assert_eq!(attached.enumerated.address, 1);
        assert_eq!(
            driver.io().done_head,
            0,
            "every control transfer must have reaped its own writeback"
        );
        assert!(!driver.io().wdh, "no writeback may be left unacknowledged");

        run(driver.open_interrupt_in(1, false, 1, 8, 10)).unwrap();
        let mut report = [0u8; 8];
        for (expected, label) in [([0u8, 0, 0x0b, 0], "first"), ([0u8, 0, 0, 0], "second")] {
            driver.io().pending_reports.push_back(expected.to_vec());
            // The consumer's loop: an empty answer means "call again" (the first
            // round may consume the stale enumeration RHSC — the port resets latched
            // it and nothing acked it until now).
            let mut received = None;
            for _ in 0..4 {
                driver.io().wait_script.push_back(WaitOutcome::Delivered);
                let read = run(driver.read_report(&mut report)).unwrap();
                assert!(!read.rescued, "{label}: the event path is not a rescue");
                if read.length.is_some() {
                    received = read.length;
                    break;
                }
            }
            assert_eq!(received, Some(4), "{label} report after enumeration");
            assert_eq!(&report[..4], &expected);
        }
    }

    #[test]
    fn wait_port_change_is_event_driven_where_supported_and_typed_unsupported_otherwise() {
        // A4: the connect watch. Polled configuration: Unsupported, immediately —
        // the consumer keeps its sweep pacing.
        let mut driver = Ohci::new(MockOhci::new());
        run(driver.bring_up()).unwrap();
        assert_eq!(run(driver.wait_port_change()), Ok(WaitOutcome::Unsupported));

        // Event configuration: a port change delivers RHSC; the wait resolves and
        // acks the latch (the per-port CSC bits stay for the sweep to read).
        let mut driver = Ohci::new(MockOhci::new());
        run(driver.bring_up()).unwrap();
        run(driver.enable_events()).unwrap();
        driver.io().rhsc = true;
        driver.io().wait_script.push_back(WaitOutcome::Delivered);
        assert_eq!(run(driver.wait_port_change()), Ok(WaitOutcome::Delivered));
        assert!(
            !driver.io().rhsc,
            "the wait must acknowledge the RHSC latch"
        );

        // A bounded expiry is an ordinary answer: the caller re-sweeps and calls
        // again.
        driver.io().wait_script.push_back(WaitOutcome::TimedOut);
        assert_eq!(run(driver.wait_port_change()), Ok(WaitOutcome::TimedOut));
    }
}
