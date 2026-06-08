//! The pure core of the `usb.ohci` drivers: the OHCI 1.0a register map, endpoint/
//! transfer descriptor encode/decode, the HCCA layout, the done-queue walk, the
//! enumeration state machine, USB descriptor parsing, and HID boot-protocol report
//! decode.
//!
//! Everything here is plain bit arithmetic over values the driver moves through
//! `eo9:platform` / `eo9:pci` (region registers and DMA bytes), so it is host-testable
//! without any device: the wasm components (`guest/stubs/usb-ohci`,
//! `guest/stubs/usb-ohci-pci`) are thin I/O shells over this crate, and
//! `cargo test -p eo9-ohci` pins the encodings (the eo9-rtl8125 / eofs precedent for
//! keeping pure logic out of untestable component crates).
//!
//! The shared driver itself — controller takeover, schedule management, transfers,
//! enumeration — also lives here ([`driver`]), generic over the [`driver::RegionIo`]
//! trait, so the platform-backed board shell and the PCI-backed QEMU shell are the
//! same code with different 20-line adapters (docs/board/usb-ohci-plan.md §2: "two
//! thin shells over a RegionIo trait").
//!
//! ## References (the citation rule: every constant names its source)
//!
//! * **OHCI 1.0a** — *OpenHCI: Open Host Controller Interface Specification for USB*,
//!   release 1.0a (Compaq/Microsoft/National Semiconductor). Register map §7,
//!   ED/TD formats §4.2/§4.3, HCCA §4.4, done queue §4.4.1/§5.2.9, reset §5.1.1.4.
//! * **USB 2.0** — *Universal Serial Bus Specification*, revision 2.0. Chapter 9
//!   (device framework: setup packets, standard requests, descriptors), §7.1.7.5
//!   (reset signaling and recovery).
//! * **HID 1.11** — *Device Class Definition for Human Interface Devices*, version
//!   1.11. §4.2 (subclass/protocol codes), §7.2 (class requests), appendix B (boot
//!   protocol report formats), §10 (keyboard usage tables in HUT 1.12 §10).
//! * Cross-checked against mainline Linux `drivers/usb/host/ohci.h` / `ohci-hcd.c`
//!   (v6.12) where the spec leaves latitude (FSMPS computation, periodic start).

#![cfg_attr(not(test), no_std)]

pub mod descriptor;
pub mod driver;
pub mod enumerate;
pub mod hid;
pub mod schedule;
pub mod setup;

/// PCI class/subclass/prog-if identifying an OHCI USB host controller function
/// (PCI Code and ID Assignment: base class 0x0c serial bus, subclass 0x03 USB,
/// programming interface 0x10 OHCI — QEMU's `-device pci-ohci` reports exactly this).
pub const PCI_CLASS_SERIAL_BUS: u8 = 0x0c;
pub const PCI_SUBCLASS_USB: u8 = 0x03;
pub const PCI_PROGIF_OHCI: u8 = 0x10;

/// The operational register file (offsets from the controller base; OHCI 1.0a §7).
pub mod reg {
    /// HcRevision (§7.1.1): BCD revision in [7:0] — 0x10 for OHCI 1.0/1.0a.
    pub const HC_REVISION: u64 = 0x00;
    /// HcControl (§7.1.2): list enables, functional state, interrupt routing.
    pub const HC_CONTROL: u64 = 0x04;
    /// HcCommandStatus (§7.1.3): reset, list filled bits, scheduling overrun count.
    pub const HC_COMMAND_STATUS: u64 = 0x08;
    /// HcInterruptStatus (§7.1.4): event bits (WDH, SF, RHSC, …), write-1-to-clear.
    pub const HC_INTERRUPT_STATUS: u64 = 0x0c;
    /// HcInterruptEnable / HcInterruptDisable (§7.1.5/§7.1.6).
    pub const HC_INTERRUPT_ENABLE: u64 = 0x10;
    pub const HC_INTERRUPT_DISABLE: u64 = 0x14;
    /// HcHCCA (§7.2.1): physical address of the HCCA, 256-byte aligned.
    pub const HC_HCCA: u64 = 0x18;
    /// HcPeriodCurrentED (§7.2.2).
    pub const HC_PERIOD_CURRENT_ED: u64 = 0x1c;
    /// HcControlHeadED / HcControlCurrentED (§7.2.3/§7.2.4).
    pub const HC_CONTROL_HEAD_ED: u64 = 0x20;
    pub const HC_CONTROL_CURRENT_ED: u64 = 0x24;
    /// HcBulkHeadED / HcBulkCurrentED (§7.2.5/§7.2.6).
    pub const HC_BULK_HEAD_ED: u64 = 0x28;
    pub const HC_BULK_CURRENT_ED: u64 = 0x2c;
    /// HcDoneHead (§7.2.7): mirrored into HccaDoneHead at the WDH interrupt.
    pub const HC_DONE_HEAD: u64 = 0x30;
    /// HcFmInterval (§7.3.1): FrameInterval [13:0], FSLargestDataPacket [30:16],
    /// FrameIntervalToggle bit 31. **Reset to its default by a software reset — the
    /// driver must save it across HCR and restore it after** (§5.1.1.4 step "the
    /// Host Controller Driver should restore the value of the HcFmInterval register";
    /// the classic gotcha this crate's tests pin).
    pub const HC_FM_INTERVAL: u64 = 0x34;
    /// HcFmRemaining / HcFmNumber (§7.3.2/§7.3.3). HcFmNumber increments once per
    /// (1 ms) frame — the driver's only clock (guest drivers hold no time capability;
    /// frame-counted waits replace sleeps).
    pub const HC_FM_REMAINING: u64 = 0x38;
    pub const HC_FM_NUMBER: u64 = 0x3c;
    /// HcPeriodicStart (§7.3.4): 90% of the frame interval (Linux ohci-hcd computes
    /// `(fi * 9) / 10`).
    pub const HC_PERIODIC_START: u64 = 0x40;
    /// HcLSThreshold (§7.3.5).
    pub const HC_LS_THRESHOLD: u64 = 0x44;
    /// HcRhDescriptorA (§7.4.1): NDP in [7:0], power-switching mode, POTPGT [31:24].
    pub const HC_RH_DESCRIPTOR_A: u64 = 0x48;
    /// HcRhDescriptorB (§7.4.2).
    pub const HC_RH_DESCRIPTOR_B: u64 = 0x4c;
    /// HcRhStatus (§7.4.3): LPSC (bit 16) = SetGlobalPower.
    pub const HC_RH_STATUS: u64 = 0x50;
    /// HcRhPortStatus[1] (§7.4.4); port N is at `HC_RH_PORT_STATUS + 4 * (N - 1)`.
    pub const HC_RH_PORT_STATUS: u64 = 0x54;

    /// Register offset of root-hub port `port` (1-based, as the spec numbers them).
    pub const fn rh_port_status(port: u8) -> u64 {
        HC_RH_PORT_STATUS + 4 * (port as u64 - 1)
    }
}

/// Field/bit definitions for the registers above (OHCI 1.0a §7).
pub mod bits {
    // --- HcControl (§7.1.2) ---------------------------------------------------------
    /// ControlBulkServiceRatio [1:0].
    pub const CONTROL_CBSR_MASK: u32 = 0b11;
    /// PeriodicListEnable.
    pub const CONTROL_PLE: u32 = 1 << 2;
    /// IsochronousEnable.
    pub const CONTROL_IE: u32 = 1 << 3;
    /// ControlListEnable.
    pub const CONTROL_CLE: u32 = 1 << 4;
    /// BulkListEnable.
    pub const CONTROL_BLE: u32 = 1 << 5;
    /// HostControllerFunctionalState [7:6]: 00 reset, 01 resume, 10 operational,
    /// 11 suspend.
    pub const CONTROL_HCFS_MASK: u32 = 0b11 << 6;
    pub const CONTROL_HCFS_RESET: u32 = 0b00 << 6;
    pub const CONTROL_HCFS_RESUME: u32 = 0b01 << 6;
    pub const CONTROL_HCFS_OPERATIONAL: u32 = 0b10 << 6;
    pub const CONTROL_HCFS_SUSPEND: u32 = 0b11 << 6;
    /// InterruptRouting (SMM ownership; cleared by the takeover handshake §5.1.1.3).
    pub const CONTROL_IR: u32 = 1 << 8;

    // --- HcCommandStatus (§7.1.3) -----------------------------------------------------
    /// HostControllerReset: software reset, self-clearing within 10 µs.
    pub const CMD_HCR: u32 = 1 << 0;
    /// ControlListFilled.
    pub const CMD_CLF: u32 = 1 << 1;
    /// BulkListFilled.
    pub const CMD_BLF: u32 = 1 << 2;
    /// OwnershipChangeRequest (§5.1.1.3 SMM handshake).
    pub const CMD_OCR: u32 = 1 << 3;

    // --- HcInterruptStatus / Enable (§7.1.4-§7.1.6) -------------------------------------
    /// SchedulingOverrun.
    pub const INT_SO: u32 = 1 << 0;
    /// WritebackDoneHead: HcDoneHead was written to HccaDoneHead.
    pub const INT_WDH: u32 = 1 << 1;
    /// StartofFrame.
    pub const INT_SF: u32 = 1 << 2;
    /// ResumeDetected.
    pub const INT_RD: u32 = 1 << 3;
    /// UnrecoverableError.
    pub const INT_UE: u32 = 1 << 4;
    /// FrameNumberOverflow.
    pub const INT_FNO: u32 = 1 << 5;
    /// RootHubStatusChange.
    pub const INT_RHSC: u32 = 1 << 6;
    /// OwnershipChange.
    pub const INT_OC: u32 = 1 << 30;
    /// MasterInterruptEnable (HcInterruptEnable/Disable bit 31).
    pub const INT_MIE: u32 = 1 << 31;

    // --- HcFmInterval (§7.3.1) ------------------------------------------------------------
    /// FrameInterval mask [13:0]; the default is 11999 bit times (0x2edf).
    pub const FM_INTERVAL_FI_MASK: u32 = 0x3fff;
    pub const FM_INTERVAL_DEFAULT_FI: u32 = 0x2edf;
    /// FSLargestDataPacket shift ([30:16]).
    pub const FM_INTERVAL_FSMPS_SHIFT: u32 = 16;
    /// FrameIntervalToggle (bit 31): must flip whenever FrameInterval is written.
    pub const FM_INTERVAL_FIT: u32 = 1 << 31;

    // --- HcRhDescriptorA (§7.4.1) -------------------------------------------------------
    /// NumberDownstreamPorts [7:0].
    pub const RH_A_NDP_MASK: u32 = 0xff;
    /// NoPowerSwitching (bit 9): ports always powered when set.
    pub const RH_A_NPS: u32 = 1 << 9;
    /// PowerOnToPowerGoodTime [31:24], in 2 ms units.
    pub const RH_A_POTPGT_SHIFT: u32 = 24;

    // --- HcRhStatus (§7.4.3) ---------------------------------------------------------------
    /// Write: SetGlobalPower (powers all ports under global switching).
    pub const RH_STATUS_LPSC: u32 = 1 << 16;

    // --- HcRhPortStatus (§7.4.4) -------------------------------------------------------------
    /// CurrentConnectStatus (read) / ClearPortEnable (write 1).
    pub const PORT_CCS: u32 = 1 << 0;
    /// PortEnableStatus (read) / SetPortEnable (write 1).
    pub const PORT_PES: u32 = 1 << 1;
    /// PortSuspendStatus (read) / SetPortSuspend (write 1).
    pub const PORT_PSS: u32 = 1 << 2;
    /// PortOverCurrentIndicator (read) / ClearSuspendStatus (write 1).
    pub const PORT_POCI: u32 = 1 << 3;
    /// PortResetStatus (read) / SetPortReset (write 1). The controller times the
    /// (≥10 ms, §7.4.4 "approximately 10 ms") reset itself and sets PRSC when done.
    pub const PORT_PRS: u32 = 1 << 4;
    /// PortPowerStatus (read) / SetPortPower (write 1).
    pub const PORT_PPS: u32 = 1 << 8;
    /// LowSpeedDeviceAttached (read) / ClearPortPower (write 1).
    pub const PORT_LSDA: u32 = 1 << 9;
    /// ConnectStatusChange (write 1 to clear).
    pub const PORT_CSC: u32 = 1 << 16;
    /// PortEnableStatusChange (write 1 to clear).
    pub const PORT_PESC: u32 = 1 << 17;
    /// PortSuspendStatusChange (write 1 to clear).
    pub const PORT_PSSC: u32 = 1 << 18;
    /// PortOverCurrentIndicatorChange (write 1 to clear).
    pub const PORT_OCIC: u32 = 1 << 19;
    /// PortResetStatusChange (write 1 to clear; set when the reset completes).
    pub const PORT_PRSC: u32 = 1 << 20;
}

/// Compute the FSLargestDataPacket field for a frame interval: the largest number of
/// bit times a full-speed packet may take and still fit in the frame, with the spec's
/// worst-case bit-stuffing factor of 7/6 and a 210-bit-time per-frame overhead
/// (OHCI 1.0a §5.4 / §7.3.1; Linux ohci.h `FSMP`: `(fi - 210) * 6 / 7`).
pub const fn fsmps(frame_interval: u32) -> u32 {
    ((frame_interval - 210) * 6) / 7
}

/// The HcFmInterval value to write when (re)programming the frame interval — after a
/// software reset in particular, which zaps HcFmInterval back to its default
/// (§5.1.1.4: the driver saves HcFmInterval before HCR and restores it after; this
/// crate's tests pin the gotcha). `previous` is the register's current value: the
/// FrameIntervalToggle bit must FLIP relative to it whenever FrameInterval is written
/// (§7.3.1), and FSLargestDataPacket is recomputed for the interval.
pub const fn fm_interval_restore(saved_fi: u32, previous: u32) -> u32 {
    let fi = saved_fi & bits::FM_INTERVAL_FI_MASK;
    let toggled = (previous & bits::FM_INTERVAL_FIT) ^ bits::FM_INTERVAL_FIT;
    toggled | (fsmps(fi) << bits::FM_INTERVAL_FSMPS_SHIFT) | fi
}

/// HcPeriodicStart for a frame interval: 90% of the frame, so periodic transfers get
/// the tail of every frame (OHCI 1.0a §7.3.4; Linux ohci-hcd: `(fi * 9) / 10`).
pub const fn periodic_start(frame_interval: u32) -> u32 {
    ((frame_interval & bits::FM_INTERVAL_FI_MASK) * 9) / 10
}

/// Transfer completion codes (OHCI 1.0a §4.3.3, table 4-7). The default is
/// `NotAccessed` — what a freshly built TD carries.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum ConditionCode {
    NoError,
    Crc,
    BitStuffing,
    DataToggleMismatch,
    Stall,
    DeviceNotResponding,
    PidCheckFailure,
    UnexpectedPid,
    DataOverrun,
    DataUnderrun,
    BufferOverrun,
    BufferUnderrun,
    /// 0b1110/0b1111: the TD has not been touched by the controller yet.
    #[default]
    NotAccessed,
    /// 0b1010/0b1011 are reserved in 1.0a.
    Reserved(u8),
}

impl ConditionCode {
    pub fn from_bits(code: u8) -> ConditionCode {
        match code & 0xf {
            0x0 => ConditionCode::NoError,
            0x1 => ConditionCode::Crc,
            0x2 => ConditionCode::BitStuffing,
            0x3 => ConditionCode::DataToggleMismatch,
            0x4 => ConditionCode::Stall,
            0x5 => ConditionCode::DeviceNotResponding,
            0x6 => ConditionCode::PidCheckFailure,
            0x7 => ConditionCode::UnexpectedPid,
            0x8 => ConditionCode::DataOverrun,
            0x9 => ConditionCode::DataUnderrun,
            0xc => ConditionCode::BufferOverrun,
            0xd => ConditionCode::BufferUnderrun,
            0xe | 0xf => ConditionCode::NotAccessed,
            other => ConditionCode::Reserved(other),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn register_offsets_match_the_spec_table() {
        // OHCI 1.0a §7 register map, spot-pinned end to end.
        assert_eq!(reg::HC_REVISION, 0x00);
        assert_eq!(reg::HC_CONTROL, 0x04);
        assert_eq!(reg::HC_COMMAND_STATUS, 0x08);
        assert_eq!(reg::HC_HCCA, 0x18);
        assert_eq!(reg::HC_DONE_HEAD, 0x30);
        assert_eq!(reg::HC_FM_INTERVAL, 0x34);
        assert_eq!(reg::HC_FM_NUMBER, 0x3c);
        assert_eq!(reg::HC_PERIODIC_START, 0x40);
        assert_eq!(reg::HC_RH_DESCRIPTOR_A, 0x48);
        assert_eq!(reg::HC_RH_STATUS, 0x50);
        assert_eq!(reg::rh_port_status(1), 0x54);
        assert_eq!(reg::rh_port_status(2), 0x58);
    }

    #[test]
    fn fm_interval_restore_flips_fit_and_recomputes_fsmps() {
        // The HcFmInterval-restore-after-reset gotcha (§5.1.1.4): default FI 0x2edf,
        // FSMPS = (0x2edf - 210) * 6 / 7 = 0x2778 (the value Linux programs), and FIT
        // flips relative to whatever the register held after reset.
        let fi = bits::FM_INTERVAL_DEFAULT_FI;
        assert_eq!(fsmps(fi), 0x2778);
        // After HCR the register reads the default with FIT clear: restore sets FIT.
        let restored = fm_interval_restore(fi, fi);
        assert_eq!(restored, bits::FM_INTERVAL_FIT | (0x2778 << 16) | 0x2edf);
        // Writing again flips FIT back off.
        let again = fm_interval_restore(fi, restored);
        assert_eq!(again, (0x2778 << 16) | 0x2edf);
    }

    #[test]
    fn periodic_start_is_ninety_percent_of_the_frame() {
        // Linux ohci-hcd programs 0x2a2f for the default interval ((0x2edf * 9) / 10).
        assert_eq!(periodic_start(bits::FM_INTERVAL_DEFAULT_FI), 0x2a2f);
    }

    #[test]
    fn condition_codes_round_trip() {
        assert_eq!(ConditionCode::from_bits(0), ConditionCode::NoError);
        assert_eq!(ConditionCode::from_bits(4), ConditionCode::Stall);
        assert_eq!(ConditionCode::from_bits(0xe), ConditionCode::NotAccessed);
        assert_eq!(ConditionCode::from_bits(0xf), ConditionCode::NotAccessed);
        assert_eq!(ConditionCode::from_bits(0xa), ConditionCode::Reserved(0xa));
    }
}
