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

use crate::enumerate::{Action, Enumerated, Enumeration, EnumerationError, Event};
use crate::schedule::{
    EdDirection, EndpointDescriptor, TdPid, TdToggle, TransferDescriptor, arena, hcca,
    walk_done_queue,
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

/// The driver. Construct with [`Ohci::new`], then [`bring_up`](Self::bring_up) before
/// anything else.
pub struct Ohci<R: RegionIo> {
    io: R,
    info: Option<ControllerInfo>,
    /// The interrupt-IN endpoint's re-arm state: which TD slot is pending and which
    /// is the dummy tail, plus the buffer length to read back.
    interrupt: Option<InterruptState>,
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
        }
    }

    /// The controller info, if bring-up already ran.
    pub fn info(&self) -> Option<ControllerInfo> {
        self.info
    }

    /// Take the controller over and make it operational (OHCI 1.0a §5.1.1):
    /// ownership handshake if SMM holds it, software reset **with HcFmInterval saved
    /// and restored across it** (§5.1.1.4 — the gotcha), HCCA, schedules cleared,
    /// operational state, interrupts masked (the polled driver's suppression
    /// discipline), root-hub power on.
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
        self.io
            .write32(reg::HC_BULK_HEAD_ED, 0)
            .await
            .map_err(io)?;
        self.io
            .write32(reg::HC_BULK_CURRENT_ED, 0)
            .await
            .map_err(io)?;

        // No interrupt delivery: the driver is polled (the ISR-suppression discipline
        // in this device's dialect); the WDH writeback to the HCCA happens regardless.
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
            (setup.length as usize).min(data_in.len()).min(arena::CONTROL_BUFFER_LEN as usize)
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

        let mut setup_descriptor = TransferDescriptor::new(
            TdPid::Setup,
            TdToggle::Data0,
            Some((base + arena::SETUP_BUFFER as u32, 8)),
        );
        let has_data = in_length > 0;
        setup_descriptor.next = base + if has_data { data_td } else { status_td } as u32;
        self.io.dma_write(setup_td, &setup_descriptor.encode());

        if has_data {
            let mut data_descriptor = TransferDescriptor::new(
                TdPid::In,
                TdToggle::Data1,
                Some((base + arena::CONTROL_BUFFER as u32, in_length as u32)),
            );
            data_descriptor.next = base + status_td as u32;
            self.io.dma_write(data_td, &data_descriptor.encode());
        }

        // Status stage: IN for OUT/no-data requests, OUT for IN requests; always DATA1.
        let status_pid = if has_data { TdPid::Out } else { TdPid::In };
        let mut status_descriptor = TransferDescriptor::new(status_pid, TdToggle::Data1, None);
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
        self.consume_done_queue().await?;

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
        judge(read_td(&mut self.io, setup_td))?;
        let received = if has_data {
            // A halted ED with a clean setup stage but an erroring data stage means
            // the device refused the data stage — judged typed here.
            let data = judge(read_td(&mut self.io, data_td))?;
            let received =
                data.bytes_transferred(base + arena::CONTROL_BUFFER as u32, in_length as u32)
                    as usize;
            self.io
                .dma_read(arena::CONTROL_BUFFER, &mut data_in[..received]);
            received
        } else {
            0
        };
        if halted {
            // Halted but every TD we judged was clean: surface the strangeness typed.
            return Err(DriverError::Transfer(ConditionCode::NotAccessed));
        }
        judge(read_td(&mut self.io, status_td))?;
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
        self.io.dma_read(arena::HCCA + hcca::DONE_HEAD, &mut head_bytes);
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
        let result = walk_done_queue(
            head,
            DONE_QUEUE_LIMIT,
            |address| {
                let offset = u64::from(address - self.io.dma_base());
                let mut td_bytes = [0u8; 16];
                self.io.dma_read(offset, &mut td_bytes);
                TransferDescriptor::decode(&td_bytes)
            },
            |address, _| reads.push(address),
        );
        if result.is_err() {
            return Err(DriverError::DoneQueueCorrupt);
        }
        Ok(head & !0xf)
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
            self.io.dma_read(arena::td_slot(pending_slot), &mut td_bytes);
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
        self.io.dma_read(arena::td_slot(pending_slot), &mut td_bytes);
        let td = TransferDescriptor::decode(&td_bytes);
        match td.condition_code {
            ConditionCode::NoError => {}
            ConditionCode::Stall => return Err(DriverError::Stall),
            other => return Err(DriverError::Transfer(other)),
        }
        let report_len = u64::from(max_packet).min(arena::INTERRUPT_BUFFER_LEN);
        let received = td.bytes_transferred(
            base + arena::INTERRUPT_BUFFER as u32,
            report_len as u32,
        ) as usize;
        let copied = received.min(report.len());
        self.io
            .dma_read(arena::INTERRUPT_BUFFER, &mut report[..copied]);
        self.consume_done_queue().await?;

        // Re-arm: the old dummy becomes the pending TD, the old pending the new dummy
        // (the standard tail-swap; the controller owns head, we only move tail).
        let new_pending = dummy_slot;
        let new_dummy = pending_slot;
        let mut next = TransferDescriptor::new(
            TdPid::In,
            TdToggle::FromEd,
            Some((base + arena::INTERRUPT_BUFFER as u32, report_len as u32)),
        );
        next.next = base + arena::td_slot(new_dummy) as u32;
        self.io
            .dma_write(arena::td_slot(new_dummy), &TransferDescriptor::default().encode());
        self.io.dma_write(arena::td_slot(new_pending), &next.encode());
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
    }

    const DMA_BASE: u32 = 0x4000_0000;

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
            }
        }

        fn register(&self, offset: u64) -> u32 {
            *self.registers.get(&offset).unwrap_or(&0)
        }

        fn arena_slice(&mut self, offset: u32, len: usize) -> &mut [u8] {
            let start = (offset - self.dma_base) as usize;
            &mut self.arena[start..start + len]
        }

        /// Answer a setup packet the way QEMU's usb-kbd does for the v1 request set.
        fn answer(&mut self, setup_bytes: [u8; 8], buffer: &mut Vec<u8>) {
            let request_type = setup_bytes[0];
            let request = setup_bytes[1];
            let value = u16::from_le_bytes([setup_bytes[2], setup_bytes[3]]);
            let length = u16::from_le_bytes([setup_bytes[6], setup_bytes[7]]) as usize;
            self.requests.push((request_type, request, value));
            buffer.clear();
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
                    self.device_address = value as u8;
                }
                _ => {}
            }
        }

        /// Process the control list: walk the ED's TD chain to its tail, answering
        /// the setup and filling IN buffers, retiring each TD with NoError and
        /// pushing it on the done queue (newest first), then write back HccaDoneHead.
        fn process_control_list(&mut self) {
            let ed_offset = (self.register(reg::HC_CONTROL_HEAD_ED) - self.dma_base) as u64;
            let mut ed_bytes = [0u8; 16];
            ed_bytes.copy_from_slice(self.arena_slice(
                self.dma_base + ed_offset as u32,
                16,
            ));
            let mut ed = EndpointDescriptor::decode(&ed_bytes);
            let mut response: Vec<u8> = Vec::new();
            let mut done: Vec<u32> = Vec::new();
            while ed.head != ed.tail {
                let td_address = ed.head;
                let mut td_bytes = [0u8; 16];
                td_bytes.copy_from_slice(self.arena_slice(td_address, 16));
                let mut td = TransferDescriptor::decode(&td_bytes);
                match td.pid {
                    TdPid::Setup => {
                        let mut setup_bytes = [0u8; 8];
                        setup_bytes.copy_from_slice(self.arena_slice(td.current_buffer, 8));
                        let mut answered = Vec::new();
                        self.answer(setup_bytes, &mut answered);
                        response = answered;
                    }
                    TdPid::In => {
                        if td.current_buffer != 0 {
                            let capacity =
                                (td.buffer_end - td.current_buffer + 1) as usize;
                            let send = response.len().min(capacity);
                            let start = td.current_buffer;
                            let payload: Vec<u8> = response[..send].to_vec();
                            self.arena_slice(start, send).copy_from_slice(&payload);
                            // CBP: 0 if the buffer was filled exactly, else next byte.
                            td.current_buffer = if send == capacity {
                                0
                            } else {
                                td.current_buffer + send as u32
                            };
                        }
                    }
                    TdPid::Out => {}
                }
                td.condition_code = ConditionCode::NoError;
                ed.head = td.next & !0xf;
                // Push on the done queue (newest first).
                let previous_head = done.first().copied().unwrap_or(0);
                td.next = previous_head;
                let encoded = td.encode();
                self.arena_slice(td_address, 16).copy_from_slice(&encoded);
                done.insert(0, td_address);
            }
            let encoded_ed = ed.encode();
            self.arena_slice(self.dma_base + ed_offset as u32, 16)
                .copy_from_slice(&encoded_ed);
            if let Some(&newest) = done.first() {
                let head_offset = (arena::HCCA + hcca::DONE_HEAD) as usize;
                self.arena[head_offset..head_offset + 4]
                    .copy_from_slice(&newest.to_le_bytes());
            }
        }
    }

    #[derive(Debug, PartialEq, Eq, Clone, Copy)]
    struct NoError;

    impl RegionIo for MockOhci {
        type Error = NoError;

        async fn read32(&mut self, offset: u64) -> Result<u32, NoError> {
            if offset == reg::HC_FM_NUMBER {
                // The frame counter ticks once per few reads, so frame-counted waits
                // terminate fast in tests but still exercise the polling shape.
                self.frame_phase += 1;
                if self.frame_phase % 2 == 0 {
                    self.frame = (self.frame + 1) & 0xffff;
                }
                return Ok(self.frame);
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
                    // Write-1-to-clear; nothing modelled needs the bits kept.
                }
                offset if offset == reg::rh_port_status(1) => {
                    let mut status = self.register(offset);
                    if value & bits::PORT_PRS != 0 {
                        // The controller times the reset itself: complete instantly,
                        // port enabled.
                        status |= bits::PORT_PRSC | bits::PORT_PES;
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
            let start = offset as usize;
            buf.copy_from_slice(&self.arena[start..start + buf.len()]);
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
        let expected = fm_interval_restore(
            bits::FM_INTERVAL_DEFAULT_FI,
            bits::FM_INTERVAL_DEFAULT_FI,
        );
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
}
