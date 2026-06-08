//! The pure core of the `net.rtl8125` driver: the RTL8125 register map, the legacy
//! 16-byte descriptor ring encode/decode, and the GPHY OCP (MDIO) command words.
//!
//! Everything here is plain bit arithmetic over values the driver moves through
//! `eo9:pci` (BAR registers and DMA bytes), so it is host-testable without any device:
//! the wasm component (`guest/stubs/net-rtl8125`) is a thin I/O shell over this crate,
//! and `cargo test -p eo9-rtl8125` pins the encodings (the eofs / eosh-core precedent
//! for keeping pure logic out of untestable component crates).
//!
//! ## References (the citation rule: every constant names its source)
//!
//! The primary reference is the mainline Linux **r8169** driver
//! (`drivers/net/ethernet/realtek/r8169_main.c`, v6.12), which drives the RTL8125
//! family (`RTL_GIGA_MAC_VER_61..=66`) with the same legacy 16-byte descriptor format
//! as the rest of the RTL8168/8169 line — proof that the 8125 does not need the
//! vendor driver's 32-byte "v3" receive descriptors for basic operation. Cross
//! references where the 8125 differs from the older parts: the Realtek vendor driver
//! **r8125** (`r8125_n.c` / `r8125.h`) and OpenBSD **rge(4)** (`sys/dev/pci/if_rge.c`,
//! `if_rgereg.h`), the two drivers written specifically for this chip.

#![cfg_attr(not(test), no_std)]

/// PCI identity of the RTL8125 2.5GbE controller (r8169_main.c `rtl8169_pci_tbl`:
/// `PCI_VDEVICE(REALTEK, 0x8125)`; both Orange Pi 5 Plus NICs enumerate as this —
/// kernel/eo9-kernel/src/arch/aarch64/rk3588_pcie.rs module docs).
pub const PCI_VENDOR_REALTEK: u16 = 0x10ec;
pub const PCI_DEVICE_RTL8125: u16 = 0x8125;

/// The BAR carrying the MAC register block on the modern (PCIe) parts: region 2
/// (r8169_main.c `rtl_init_one`: "use first MMIO region with proper size" — region 2
/// on every PCIe chip, a 64-bit memory BAR). The driver prefers BAR 2 and falls back
/// to the first memory BAR so a quirky bridge setup still probes.
pub const MMIO_BAR_INDEX: u8 = 2;

// ------------------------------------------------------------------------------------
// MAC registers (offsets into the MMIO BAR). Sources: r8169_main.c `enum
// rtl_registers` and `enum rtl8125_registers`.
// ------------------------------------------------------------------------------------

pub mod reg {
    /// `MAC0` — the station address, 6 bytes at 0x00 (r8169 `enum rtl_registers`).
    pub const MAC0: u64 = 0x00;
    /// `MAR0` — multicast filter, 8 bytes at 0x08 (r8169). This driver runs with
    /// multicast acceptance OFF (plan/09: multicast = none for v1), so both dwords are
    /// written 0 for determinism.
    pub const MAR0: u64 = 0x08;
    /// `CounterAddrLow`/`High` — the hardware tally-counter dump address (r8169
    /// `enum rtl_registers` + `rtl8169_do_counters`): write the DMA address, then the
    /// low dword again with the command bit; the chip DMA-writes the counter block
    /// and clears the bit. The dump doubles as an inbound-WRITE probe independent of
    /// the receive path.
    pub const COUNTER_ADDR_LOW: u64 = 0x10;
    pub const COUNTER_ADDR_HIGH: u64 = 0x14;
    /// `INT_CFG0_8125` — 8125 interrupt-configuration byte at 0x34; hw_start writes 0
    /// (r8169 `enum rtl8125_registers` + `rtl_hw_start_8125`).
    pub const INT_CFG0_8125: u64 = 0x34;
    /// `Cfg9346` — the config-register write protect at 0x50: 0xc0 unlocks the
    /// Config0..5 group, 0x00 re-locks (r8169 `rtl_unlock_config_regs` /
    /// `rtl_lock_config_regs`).
    pub const CFG9346: u64 = 0x50;
    /// `Config1` at 0x52: hw_start_8125_common clears bit 4 (r8169:
    /// `RTL_W8(tp, Config1, RTL_R8(tp, Config1) & ~0x10)`).
    pub const CONFIG1: u64 = 0x52;
    /// `Config3` at 0x54: bit 1 (`Rdy_to_L23`) cleared to keep the chip out of the
    /// PCIe L2/L3 ready state (r8169 `rtl_pcie_state_l2l3_disable`).
    pub const CONFIG3: u64 = 0x54;
    /// `INT_CFG1_8125` — 16-bit interrupt configuration at 0x7a; hw_start writes 0 on
    /// the 8125B and later (r8169 `rtl_hw_start_8125`, the VER_63+ arm).
    pub const INT_CFG1_8125: u64 = 0x7a;
    /// `OCPDR` — the MAC-side OCP window at 0xb0 (r8169 `__r8168_mac_ocp_write` /
    /// `__r8168_mac_ocp_read`): same word encoding as `GPHY_OCP`, but transactions
    /// complete immediately (the reference drivers never poll it).
    pub const OCPDR: u64 = 0xb0;
    /// `MCU` — the MCU command/status byte at 0xd3 (r8169 `enum
    /// rtl8168_8101_registers` `MCU = 0xd3`; rge(4) `RGE_MCUCMD 0x00d3`): OOB
    /// ownership, FIFO-empty, and link-list-ready all live here.
    pub const MCU: u64 = 0xd3;
    /// Byte 0xf2 carries the RXDV gate: bit 3 here is bit 19 (`RXDV_GATED_EN`) of the
    /// 32-bit `MISC` register at 0xf0 (r8169 `rtl_enable_rxdvgate`; rge(4) sets the
    /// same bit byte-wise: `RGE_SETBIT_1(sc, RGE_PPSW /* 0xf2 */, 0x08)`).
    pub const RXDV_GATE_BYTE: u64 = 0xf2;
    /// The 8125 per-queue interrupt-mitigation block at 0xa00: hw_start zeroes
    /// `0xa00..0xb00` (8125A) or `0xa00..0xa80` + `INT_CFG1_8125 = 0` (8125B and
    /// later) — "disable interrupt coalescing", r8169 `rtl_hw_start_8125`.
    pub const INT_MITI_BASE_8125: u64 = 0xa00;
    pub const INT_MITI_END_8125A: u64 = 0xb00;
    pub const INT_MITI_END_8125B: u64 = 0xa80;
    /// `TxDescStartAddrLow`/`High` — normal-priority transmit ring base (r8169).
    pub const TX_DESC_ADDR_LOW: u64 = 0x20;
    pub const TX_DESC_ADDR_HIGH: u64 = 0x24;
    /// `ChipCmd` (r8169): bit 4 reset, bit 3 receiver enable, bit 2 transmitter enable.
    pub const CHIP_CMD: u64 = 0x37;
    /// `IntrMask_8125` / `IntrStatus_8125` — the 8125 moved IMR/ISR to 32-bit
    /// registers at 0x38/0x3c (r8169 `enum rtl8125_registers`). The polled driver
    /// keeps IMR 0 (no source ever asserts the — unwired on the board — INTx line)
    /// and clears ISR once at bring-up; the suppression discipline net.virtio's
    /// `AVAIL_F_NO_INTERRUPT` documents, in this device's dialect.
    pub const INTR_MASK_8125: u64 = 0x38;
    pub const INTR_STATUS_8125: u64 = 0x3c;
    /// `TxConfig` (r8169): DMA burst in [10:8], inter-frame gap in [25:24].
    pub const TX_CONFIG: u64 = 0x40;
    /// `RxConfig` (r8169): accept bits in [5:0], DMA burst [10:8], 8125 fetch count
    /// [30:27].
    pub const RX_CONFIG: u64 = 0x44;
    /// `PHYstatus` (r8169 `enum rtl_registers`; 16-bit wide on the 8125 — the 2.5G
    /// bit lives above bit 7, r8125.h `PHY status` / rge(4) `RGE_PHYSTAT`).
    pub const PHY_STATUS: u64 = 0x6c;
    /// `TxPoll_8125` — the 8125 transmit doorbell: 16-bit register at 0x90, bit 0
    /// kicks queue 0 (r8169 `enum rtl8125_registers` + `rtl8125_doorbell`).
    pub const TX_POLL_8125: u64 = 0x90;
    /// `GPHY_OCP` — the MAC-integrated PHY's MDIO window (r8169 `enum
    /// rtl8168_8101_registers`; the 8125 keeps it — `r8168g_mdio_write` is the 8125's
    /// mdio_ops too).
    pub const GPHY_OCP: u64 = 0xb8;
    /// `RxMaxSize` — 16-bit receive size limit at 0xda (r8169 `rtl_set_rx_max_size`).
    pub const RX_MAX_SIZE: u64 = 0xda;
    /// `RxDescAddrLow`/`High` — receive ring base (r8169).
    pub const RX_DESC_ADDR_LOW: u64 = 0xe4;
    pub const RX_DESC_ADDR_HIGH: u64 = 0xe8;
}

pub mod bits {
    /// `ChipCmd` bits (r8169 `enum rtl_register_content`; `StopReq` per rge(4)
    /// `RGE_CMD_STOPREQ` and r8169's `RTL_W8(tp, ChipCmd, … | StopReq)` quiesce arm).
    pub const CMD_RESET: u64 = 0x10;
    pub const CMD_RX_ENABLE: u64 = 0x08;
    pub const CMD_TX_ENABLE: u64 = 0x04;
    pub const CMD_STOP_REQ: u64 = 0x80;

    /// `MCU` (0xd3) bits — r8169: `NOW_IS_OOB` (1<<7), `TX_EMPTY` (1<<5),
    /// `RX_EMPTY` (1<<4), `LINK_LIST_RDY` (1<<1); rge(4) `RGE_MCUCMD_IS_OOB` /
    /// `RGE_MCUCMD_TXFIFO_EMPTY` / `RGE_MCUCMD_RXFIFO_EMPTY` agree (its link-list
    /// wait reads the same bit as word 0xd2 bit 9).
    pub const MCU_NOW_IS_OOB: u64 = 1 << 7;
    pub const MCU_TX_EMPTY: u64 = 1 << 5;
    pub const MCU_RX_EMPTY: u64 = 1 << 4;
    pub const MCU_LINK_LIST_RDY: u64 = 1 << 1;

    /// The RXDV gate as seen through byte 0xf2 (bit 3 == `RXDV_GATED_EN` bit 19 of
    /// dword `MISC` 0xf0; r8169 + rge(4), see `reg::RXDV_GATE_BYTE`).
    pub const RXDV_GATE: u64 = 0x08;

    /// `Cfg9346` values (r8169 `Cfg9346_Unlock` / `Cfg9346_Lock`).
    pub const CFG9346_UNLOCK: u64 = 0xc0;
    pub const CFG9346_LOCK: u64 = 0x00;
    /// `Config1` bit hw_start_8125_common clears (r8169, unnamed `~0x10`).
    pub const CONFIG1_SPEED_DOWN: u64 = 0x10;
    /// `Config3.Rdy_to_L23` (1 << 1) — r8169 `rtl_pcie_state_l2l3_disable`.
    pub const CONFIG3_RDY_TO_L23: u64 = 0x02;

    /// `TxConfig` value: unlimited DMA burst (7 << 8, `TX_DMA_BURST`) + the standard
    /// inter-frame gap (3 << 24, `InterFrameGap`) — r8169
    /// `rtl_set_tx_config_registers`.
    pub const TX_CONFIG_VALUE: u64 = (7 << 8) | (3 << 24);

    /// `RxConfig` base for the 8125: `RX_FETCH_DFLT_8125` (8 << 27) |
    /// `RX_DMA_BURST` (7 << 8) — r8169 `rtl_init_rxcfg`, the `RTL_GIGA_MAC_VER_61`
    /// arm; the 8125B-and-later arm adds `RX_PAUSE_SLOT_ON` (1 << 11, "8125b and
    /// later") — use [`super::rx_config_base`] to pick by chip.
    pub const RX_CONFIG_BASE: u64 = (8 << 27) | (7 << 8);
    pub const RX_PAUSE_SLOT_ON: u64 = 1 << 11;
    /// `RxConfig` accept bits (r8169 `rx_mode_bits`): this driver accepts broadcast +
    /// its own station address only — promiscuous (`AcceptAllPhys`) stays off, and
    /// multicast (`AcceptMulticast`) is off for v1 (recorded: multicast = none).
    pub const RX_ACCEPT_BROADCAST: u64 = 0x08;
    pub const RX_ACCEPT_MY_PHYS: u64 = 0x02;

    /// `PHYstatus` bits (r8125.h "PHY status"; rge(4) `RGE_PHYSTAT_*` agrees):
    /// link up, and the speed-resolved bits.
    pub const PHY_STATUS_LINK: u64 = 0x0002;
    pub const PHY_STATUS_10M: u64 = 0x0004;
    pub const PHY_STATUS_100M: u64 = 0x0008;
    pub const PHY_STATUS_1000M_FULL: u64 = 0x0010;
    pub const PHY_STATUS_2500M_FULL: u64 = 0x0400;

    /// `TxPoll_8125` bit 0: poll queue 0 (r8169 `rtl8125_doorbell`).
    pub const TX_POLL_QUEUE0: u64 = 0x01;

    /// Interrupt status bits (r8169 "InterruptStatusBits"; the 8125's 32-bit ISR
    /// keeps the same low-bit layout). The ISR latches events even with IMR = 0, so
    /// the polled driver reads it as a "what has the MAC done since bring-up"
    /// history register for diagnostics: `RxOK` set = a frame was accepted AND
    /// stored to a descriptor; `RxOverflow` (the rx-descriptor-unavailable bit) set
    /// = a frame was accepted but the chip saw no OWN'd descriptor to store it.
    pub const ISR_RX_OK: u64 = 0x0001;
    pub const ISR_RX_ERR: u64 = 0x0002;
    pub const ISR_TX_OK: u64 = 0x0004;
    pub const ISR_TX_ERR: u64 = 0x0008;
    pub const ISR_RX_DESC_UNAVAIL: u64 = 0x0010; // r8169 `RxOverflow`
    pub const ISR_LINK_CHANGE: u64 = 0x0020;
    pub const ISR_RX_FIFO_OVER: u64 = 0x0040;
    pub const ISR_TX_DESC_UNAVAIL: u64 = 0x0080;

    /// Tally-counter commands, OR'd into the `CounterAddrLow` write (r8169
    /// `CounterDump` / `CounterReset`; the chip clears the bit when the DMA dump
    /// completes — `rtl_counters_cond`).
    pub const COUNTER_DUMP: u64 = 0x8;
    pub const COUNTER_RESET: u64 = 0x1;

    /// `RxMaxSize` value: `R8169_RX_BUF_SIZE + 1` = 16384 (r8169
    /// `rtl_set_rx_max_size`, with its in-tree warning — "Low hurts. Let's disable
    /// the filtering." — a small RMS has historically broken receive on this
    /// family). Frames longer than one 2 KiB slot span descriptors and are dropped
    /// whole by the driver's fragment check, so the large limit is safe here.
    pub const RX_MAX_SIZE_VALUE: u64 = 16384;
}

// ------------------------------------------------------------------------------------
// GPHY OCP — the MAC's window onto its integrated PHY (MDIO-equivalent).
// ------------------------------------------------------------------------------------

/// The integrated PHY's standard MII registers live at OCP `0xa400 + 2 * reg`
/// (r8169 `OCP_STD_PHY_BASE` + `r8168g_mdio_write`: `tp->ocp_base + reg * 2`).
pub const OCP_STD_PHY_BASE: u16 = 0xa400;

/// OCP address of a standard MII register (BMCR = 0, ANAR = 4, GBCR = 9, …).
pub const fn phy_ocp_address(mii_register: u8) -> u16 {
    OCP_STD_PHY_BASE + 2 * (mii_register as u16)
}

/// The OCP busy/done flag, bit 31 of `GPHY_OCP` (r8169 `OCPAR_FLAG`). A write
/// completes when the flag reads back LOW; a read's data is valid when it reads back
/// HIGH (r8169 `r8168_phy_ocp_write` / `r8168_phy_ocp_read` wait conditions).
pub const GPHY_OCP_FLAG: u32 = 0x8000_0000;

/// The `GPHY_OCP` command word that writes `value` to the PHY register at OCP address
/// `ocp_address`: flag | address << 15 | data (r8169 `r8168_phy_ocp_write`:
/// `OCPAR_FLAG | (reg << 15) | data`; OCP addresses are even, so `<< 15` is the
/// documented `(address / 2) << 16` of the vendor driver).
pub const fn gphy_write_command(ocp_address: u16, value: u16) -> u32 {
    GPHY_OCP_FLAG | ((ocp_address as u32) << 15) | value as u32
}

/// The `GPHY_OCP` command word that starts a read of the PHY register at OCP address
/// `ocp_address` (r8169 `r8168_phy_ocp_read`: `reg << 15`, no flag).
pub const fn gphy_read_command(ocp_address: u16) -> u32 {
    (ocp_address as u32) << 15
}

/// Decode a `GPHY_OCP` readback during a read: `Some(data)` once the flag is high
/// (r8169 `r8168_phy_ocp_read`: data is the low 16 bits).
pub const fn gphy_read_result(gphy_ocp: u32) -> Option<u16> {
    if gphy_ocp & GPHY_OCP_FLAG != 0 {
        Some(gphy_ocp as u16)
    } else {
        None
    }
}

/// Whether a `GPHY_OCP` write has completed (flag back low).
pub const fn gphy_write_done(gphy_ocp: u32) -> bool {
    gphy_ocp & GPHY_OCP_FLAG == 0
}

// ------------------------------------------------------------------------------------
// MAC OCP — the MAC-side OCP register space (reached through `OCPDR`), where the
// embedded MCU's ownership and tuning knobs live. Same word layout as the GPHY
// window; no completion flag to poll (r8169 `__r8168_mac_ocp_write` returns
// immediately, `__r8168_mac_ocp_read` reads the data straight back).
// ------------------------------------------------------------------------------------

/// The `OCPDR` word that writes `value` to MAC OCP register `ocp_address`
/// (r8169 `__r8168_mac_ocp_write`: `OCPAR_FLAG | (reg << 15) | data`).
pub const fn mac_ocp_write_command(ocp_address: u16, value: u16) -> u32 {
    GPHY_OCP_FLAG | ((ocp_address as u32) << 15) | value as u32
}

/// The `OCPDR` word that selects MAC OCP register `ocp_address` for the read that
/// follows (r8169 `__r8168_mac_ocp_read`: `reg << 15`; the next `OCPDR` read returns
/// the data in its low 16 bits).
pub const fn mac_ocp_read_command(ocp_address: u16) -> u32 {
    (ocp_address as u32) << 15
}

pub mod mac_ocp {
    /// RealWoW disable: write 0x00ff (rge(4) `rge_exit_oob`: "Disable RealWoW";
    /// the vendor r8125 `rtl8125_realwow_hw_init` does the same write).
    pub const REALWOW_CTRL: u16 = 0xc0bc;
    pub const REALWOW_DISABLE: u16 = 0x00ff;

    /// The OOB-ownership handshake register: clearing bit 14 hands the link list to
    /// the host (r8169 `rtl_hw_init_8125`: `r8168_mac_ocp_modify(tp, 0xe8de,
    /// BIT(14), 0)`; rge(4) `RGE_MAC_CLRBIT(sc, 0xe8de, 0x4000)`).
    pub const OOB_HANDSHAKE: u16 = 0xe8de;
    pub const OOB_HANDSHAKE_BIT14: u16 = 1 << 14;

    /// The three link-list parameter writes between the two link-list-ready waits
    /// (r8169 `rtl_hw_init_8125`: c0aa = 0x07d0, c0a6 = 0x0150, c01e = 0x5555;
    /// rge(4) writes the same registers — 0xc0a6 = 0x01b5 there, mainline's value
    /// is used).
    pub const LL_PARAM_A: (u16, u16) = (0xc0aa, 0x07d0);
    pub const LL_PARAM_B: (u16, u16) = (0xc0a6, 0x0150);
    pub const LL_PARAM_C: (u16, u16) = (0xc01e, 0x5555);

    /// UPS disable: clear bit 4 of 0xd40a (r8169 `rtl_hw_start_8125_common`
    /// "/* disable UPS */").
    pub const UPS_CTRL: u16 = 0xd40a;
    pub const UPS_BIT: u16 = 0x0010;

    /// "Disable new tx descriptor format" — bit 0 of 0xeb58 cleared so the chip
    /// parses the legacy 16-byte descriptors this driver writes (r8169
    /// `rtl_hw_start_8125_common`; the prime suspect behind unconsumed transmit
    /// descriptors if left in the MCU's state).
    pub const TX_NEW_DESC_FORMAT: u16 = 0xeb58;
    pub const TX_NEW_DESC_FORMAT_BIT: u16 = 0x0001;

    /// The hw_start completion handshake: write 0xc302 to 0xe098, then wait for
    /// 0xe00e bit 13 to go LOW (r8169 `rtl_hw_start_8125_common` tail:
    /// `r8168_mac_ocp_write(tp, 0xe098, 0xc302)` +
    /// `rtl_loop_wait_low(tp, &rtl_mac_ocp_e00e_cond, …)` where the cond is
    /// `r8168_mac_ocp_read(tp, 0xe00e) & BIT(13)`).
    pub const START_HANDSHAKE: (u16, u16) = (0xe098, 0xc302);
    pub const START_HANDSHAKE_STATUS: u16 = 0xe00e;
    pub const START_HANDSHAKE_BUSY: u16 = 1 << 13;
}

// ------------------------------------------------------------------------------------
// Hardware tally counters (the CounterAddr dump block)
// ------------------------------------------------------------------------------------

/// Byte offsets into the DMA'd counter block (r8169 `struct rtl8169_counters`:
/// `__le64 tx_packets; __le64 rx_packets; __le64 tx_errors; __le32 rx_errors;
/// __le16 rx_missed; __le16 align_errors; __le32 tx_one_collision;
/// __le32 tx_multi_collision; __le64 rx_unicast; __le64 rx_broadcast;
/// __le32 rx_multicast; …` — the RTL8125 appends more u64s after these).
pub mod counters {
    /// Allocation size for the dump target: the 8125's extended block fits well
    /// within this, and the address must carry zero low command bits.
    pub const DUMP_BYTES: u64 = 256;
    pub const TX_PACKETS: u64 = 0; // u64
    pub const RX_PACKETS: u64 = 8; // u64
    pub const TX_ERRORS: u64 = 16; // u64
    pub const RX_ERRORS: u64 = 24; // u32
    pub const RX_MISSED: u64 = 28; // u16
    pub const RX_UNICAST: u64 = 40; // u64
    pub const RX_BROADCAST: u64 = 48; // u64
    pub const RX_MULTICAST: u64 = 56; // u32
}

// ------------------------------------------------------------------------------------
// Chip identification
// ------------------------------------------------------------------------------------

/// The XID identifying the exact 8125 variant: `(TxConfig >> 20) & 0xfcf` (r8169
/// probe: `xid = (txconfig >> 20) & 0xfcf`).
pub const fn xid_from_tx_config(tx_config: u32) -> u16 {
    ((tx_config >> 20) & 0xfcf) as u16
}

/// Whether an XID is the first-generation RTL8125A (r8169 mac-version table:
/// `{ 0x7cf, 0x609, RTL_GIGA_MAC_VER_61 }`; the Orange Pi 5 Plus NICs are 8125B —
/// `{ 0x7cf, 0x641, RTL_GIGA_MAC_VER_63 }` — and anything newer follows the B arms).
pub const fn xid_is_8125a(xid: u16) -> bool {
    (xid & 0x7cf) == 0x609
}

/// The `RxConfig` base value for the chip (see `bits::RX_CONFIG_BASE`).
pub const fn rx_config_base(is_8125a: bool) -> u64 {
    if is_8125a {
        bits::RX_CONFIG_BASE
    } else {
        bits::RX_CONFIG_BASE | bits::RX_PAUSE_SLOT_ON
    }
}

pub mod phy {
    /// MII register numbers (IEEE 802.3 clause 22, as the reference drivers spell
    /// them: BMCR/ANAR/GBCR).
    pub const MII_BMCR: u8 = 0;
    pub const MII_ANAR: u8 = 4;
    pub const MII_GBCR: u8 = 9;

    /// BMCR: autoneg enable (0x1000) + restart (0x0200). Writing this value also
    /// clears the power-down bit (bit 11), so one write both wakes the PHY and starts
    /// negotiation (rge(4) `rge_phy_config` final BMCR write, minus its RESET — the
    /// MAC reset already reset the PHY's digital side).
    pub const BMCR_START_AUTONEG: u16 = 0x1200;

    /// ANAR: advertise 10/100 half+full + the IEEE 802.3 selector field
    /// (0x01e1 = selector 1 | 10HD 0x20 | 10FD 0x40 | 100HD 0x80 | 100FD 0x100).
    /// No pause bits: flow control is not configured anywhere in this driver.
    pub const ANAR_ADVERTISE_10_100: u16 = 0x01e1;

    /// GBCR (MII_CTRL1000): advertise 1000BASE-T full duplex (0x0200).
    pub const GBCR_ADVERTISE_1000_FULL: u16 = 0x0200;

    /// The 2.5G advertisement lives in a Realtek vendor register, OCP `0xa5d4` bit 7
    /// (Linux drivers/net/phy/realtek.c `rtl822x_config_aneg`: paged 0xa5d/0x12 →
    /// OCP 0xa5d4, `MDIO_AN_10GBT_CTRL_ADV2_5G`-equivalent bit; rge(4)
    /// `RGE_ADV_2500TFDX` 0x0080 agrees).
    pub const OCP_ADV_2500: u16 = 0xa5d4;
    pub const ADV_2500_FULL: u16 = 0x0080;
}

// ------------------------------------------------------------------------------------
// Descriptor rings: the legacy 16-byte format (r8169 `struct TxDesc` / `struct
// RxDesc`: __le32 opts1, __le32 opts2, __le64 addr).
// ------------------------------------------------------------------------------------

/// One descriptor is 16 bytes.
pub const DESC_BYTES: u64 = 16;

/// `opts1` ownership / ring bits (r8169 `enum desc_status_bit`).
pub const DESC_OWN: u32 = 1 << 31;
pub const DESC_RING_END: u32 = 1 << 30;
/// First/last fragment of a frame: transmit command bits and receive status bits at
/// the same positions (r8169 `enum tx_desc_bit` FirstFrag/LastFrag;
/// `rtl8169_fragmented_frame` checks the same pair on receive completions).
pub const DESC_FIRST_FRAG: u32 = 1 << 29;
pub const DESC_LAST_FRAG: u32 = 1 << 28;

/// Receive completion status (r8169 "RxStatusDesc" bits).
pub const RX_RES: u32 = 1 << 21; // receive error summary
/// Receive frame length: `opts1 & GENMASK(13, 0)` (r8169 `rtl_rx`), CRC included.
pub const RX_LENGTH_MASK: u32 = 0x3fff;
/// The trailing CRC the chip counts into the receive length (r8169 subtracts
/// `ETH_FCS_LEN` from the descriptor length).
pub const ETHER_CRC_LEN: u16 = 4;

/// Largest value the transmit length field carries (low 16 bits of opts1; this driver
/// caps frames far below it).
pub const TX_LENGTH_MASK: u32 = 0xffff;

/// Encode one transmit descriptor for a whole frame in one buffer: owned by the NIC,
/// first + last fragment, `frame_len` bytes at `buffer_address`, `RingEnd` on the
/// ring's final slot (r8169 `rtl8169_start_xmit`: opts1 = DescOwn | FirstFrag |
/// LastFrag | len, plus RingEnd on the wrap slot; opts2 carries only offload/VLAN
/// state and stays 0 here — no offloads).
pub fn encode_tx_descriptor(buffer_address: u64, frame_len: u16, end_of_ring: bool) -> [u8; 16] {
    let mut opts1 = DESC_OWN | DESC_FIRST_FRAG | DESC_LAST_FRAG | u32::from(frame_len);
    if end_of_ring {
        opts1 |= DESC_RING_END;
    }
    encode_descriptor(opts1, buffer_address)
}

/// Encode one receive descriptor handing `buffer_len` bytes at `buffer_address` to
/// the NIC (r8169 `rtl8169_mark_to_asic`: opts1 = DescOwn | eor | buffer size).
pub fn encode_rx_descriptor(buffer_address: u64, buffer_len: u16, end_of_ring: bool) -> [u8; 16] {
    let mut opts1 = DESC_OWN | (u32::from(buffer_len) & RX_LENGTH_MASK);
    if end_of_ring {
        opts1 |= DESC_RING_END;
    }
    encode_descriptor(opts1, buffer_address)
}

fn encode_descriptor(opts1: u32, buffer_address: u64) -> [u8; 16] {
    let mut descriptor = [0u8; 16];
    descriptor[0..4].copy_from_slice(&opts1.to_le_bytes());
    // opts2 (bytes 4..8) stays zero: no VLAN, no offloads.
    descriptor[8..16].copy_from_slice(&buffer_address.to_le_bytes());
    descriptor
}

/// The `opts1` dword from a descriptor's first four bytes (the only part the driver
/// reads back; the NIC never rewrites the address).
pub fn decode_opts1(first_four_bytes: [u8; 4]) -> u32 {
    u32::from_le_bytes(first_four_bytes)
}

/// Whether the NIC still owns the descriptor (nothing to reap yet).
pub const fn owned_by_nic(opts1: u32) -> bool {
    opts1 & DESC_OWN != 0
}

/// A decoded receive completion.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RxCompletion {
    /// The whole frame fits this descriptor (FirstFrag and LastFrag both set —
    /// guaranteed by `RxMaxSize` ≤ buffer size, checked anyway per r8169
    /// `rtl8169_fragmented_frame`).
    pub whole_frame: bool,
    /// The error summary bit (CRC/runt/watchdog rolled up — r8169 `RxRES`).
    pub error: bool,
    /// Frame length as the chip counts it, CRC included.
    pub total_len: u16,
}

/// Decode a receive completion's `opts1` (callers check `owned_by_nic` first).
pub fn decode_rx_completion(opts1: u32) -> RxCompletion {
    RxCompletion {
        whole_frame: opts1 & (DESC_FIRST_FRAG | DESC_LAST_FRAG)
            == (DESC_FIRST_FRAG | DESC_LAST_FRAG),
        error: opts1 & RX_RES != 0,
        total_len: (opts1 & RX_LENGTH_MASK) as u16,
    }
}

/// The usable payload length of a receive completion: the frame without its trailing
/// CRC, or `None` for anything that must be dropped as wire noise (error summary set,
/// a fragmented frame, or a length the CRC alone fills).
pub fn rx_payload_len(completion: &RxCompletion) -> Option<u16> {
    if !completion.whole_frame || completion.error || completion.total_len <= ETHER_CRC_LEN {
        return None;
    }
    Some(completion.total_len - ETHER_CRC_LEN)
}

/// Byte offset of descriptor `index` within a ring.
pub const fn descriptor_offset(index: u16) -> u64 {
    index as u64 * DESC_BYTES
}

/// The minimum Ethernet frame length (without CRC). Frames shorter than this are
/// zero-padded by the driver before transmit: the reference drivers lean on hardware
/// padding, but padding in the bounce buffer costs nothing and removes the one
/// behavior this lane cannot verify off-board (a 42-byte ARP request is the very
/// first frame the acceptance ladder sends).
pub const MIN_FRAME_LEN: u16 = 60;

/// The ring base alignment the chip requires: 256 bytes (r8169 `R8169_RING_ALIGN`,
/// the alignment passed to `dma_alloc_coherent` for both rings).
pub const RING_ALIGN: u64 = 256;

// ------------------------------------------------------------------------------------
// Tests — the encodings pinned against the cited reference values.
// ------------------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tx_descriptor_layout_matches_the_r8169_bits() {
        let descriptor = encode_tx_descriptor(0x1234_5678_9abc_def0, 60, false);
        let opts1 = decode_opts1([descriptor[0], descriptor[1], descriptor[2], descriptor[3]]);
        // DescOwn | FirstFrag | LastFrag | len — and nothing else.
        assert_eq!(opts1, 0x8000_0000 | 0x2000_0000 | 0x1000_0000 | 60);
        // opts2 zero (no offloads, no VLAN).
        assert_eq!(&descriptor[4..8], &[0, 0, 0, 0]);
        // 64-bit little-endian buffer address in the second qword.
        assert_eq!(
            u64::from_le_bytes(descriptor[8..16].try_into().unwrap()),
            0x1234_5678_9abc_def0
        );
    }

    #[test]
    fn the_ring_end_bit_is_bit_30_and_only_on_the_last_slot() {
        let plain = encode_tx_descriptor(0, 1, false);
        let last = encode_tx_descriptor(0, 1, true);
        assert_eq!(decode_opts1(last[0..4].try_into().unwrap()), {
            decode_opts1(plain[0..4].try_into().unwrap()) | 0x4000_0000
        });
    }

    #[test]
    fn rx_post_descriptor_carries_own_and_the_masked_buffer_size() {
        let descriptor = encode_rx_descriptor(0xfee0_0000, 2048, true);
        let opts1 = decode_opts1(descriptor[0..4].try_into().unwrap());
        assert_eq!(opts1, DESC_OWN | DESC_RING_END | 2048);
        assert!(owned_by_nic(opts1));
    }

    #[test]
    fn rx_completion_decodes_length_flags_and_error() {
        // A clean 64-byte frame: FS+LS, no RES, length includes the 4-byte CRC.
        let clean = DESC_FIRST_FRAG | DESC_LAST_FRAG | 64;
        assert!(!owned_by_nic(clean));
        let completion = decode_rx_completion(clean);
        assert_eq!(
            completion,
            RxCompletion {
                whole_frame: true,
                error: false,
                total_len: 64
            }
        );
        assert_eq!(rx_payload_len(&completion), Some(60));
    }

    #[test]
    fn rx_error_fragment_and_runt_completions_are_dropped() {
        // Error summary set (bit 21).
        let error = decode_rx_completion(DESC_FIRST_FRAG | DESC_LAST_FRAG | RX_RES | 64);
        assert!(error.error);
        assert_eq!(rx_payload_len(&error), None);
        // A fragmented frame (FS without LS).
        let fragment = decode_rx_completion(DESC_FIRST_FRAG | 2048);
        assert!(!fragment.whole_frame);
        assert_eq!(rx_payload_len(&fragment), None);
        // A runt the CRC alone fills.
        let runt = decode_rx_completion(DESC_FIRST_FRAG | DESC_LAST_FRAG | 4);
        assert_eq!(rx_payload_len(&runt), None);
    }

    #[test]
    fn rx_length_is_masked_to_14_bits() {
        // Bits above the length mask (e.g. the protocol-id bits the chip sets on IP
        // traffic, bits 17/18) must not leak into the length.
        let opts1 = DESC_FIRST_FRAG | DESC_LAST_FRAG | (1 << 17) | (1 << 18) | 1514;
        assert_eq!(decode_rx_completion(opts1).total_len, 1514);
    }

    #[test]
    fn gphy_ocp_command_words_match_the_r8169_encoding() {
        // Write BMCR (OCP 0xa400) = 0x1200: OCPAR_FLAG | (0xa400 << 15) | 0x1200.
        assert_eq!(phy_ocp_address(phy::MII_BMCR), 0xa400);
        assert_eq!(
            gphy_write_command(0xa400, 0x1200),
            0x8000_0000 | (0xa400 << 15) | 0x1200
        );
        // Read ANAR (OCP 0xa408): address << 15, flag clear.
        assert_eq!(phy_ocp_address(phy::MII_ANAR), 0xa408);
        assert_eq!(gphy_read_command(0xa408), 0xa408 << 15);
        // A read is done when the flag comes back high; data is the low half.
        assert_eq!(gphy_read_result(0x8000_abcd), Some(0xabcd));
        assert_eq!(gphy_read_result(0x0000_abcd), None);
        // A write is done when the flag goes low again.
        assert!(gphy_write_done(0x0000_0000));
        assert!(!gphy_write_done(0x8000_0000));
    }

    #[test]
    fn mii_to_ocp_mapping_is_base_plus_twice_the_register() {
        assert_eq!(phy_ocp_address(phy::MII_GBCR), 0xa412);
        assert_eq!(phy::OCP_ADV_2500, 0xa5d4);
    }

    #[test]
    fn descriptor_offsets_step_by_sixteen() {
        assert_eq!(descriptor_offset(0), 0);
        assert_eq!(descriptor_offset(1), 16);
        assert_eq!(descriptor_offset(31), 496);
    }

    #[test]
    fn config_values_match_the_cited_compositions() {
        // TxConfig: (7 << 8) | (3 << 24) = 0x0300_0700 (rtl_set_tx_config_registers).
        assert_eq!(bits::TX_CONFIG_VALUE, 0x0300_0700);
        // RxConfig base: (8 << 27) | (7 << 8) (rtl_init_rxcfg, VER_61 arm); the B arm
        // adds RX_PAUSE_SLOT_ON (bit 11).
        assert_eq!(bits::RX_CONFIG_BASE, 0x4000_0700);
        assert_eq!(rx_config_base(true), 0x4000_0700);
        assert_eq!(rx_config_base(false), 0x4000_0f00);
    }

    #[test]
    fn mac_ocp_command_words_match_the_r8169_encoding() {
        // Write 0xe8de: OCPAR_FLAG | (reg << 15) | data — same shape as the GPHY
        // window (__r8168_mac_ocp_write).
        assert_eq!(
            mac_ocp_write_command(0xe8de, 0x1234),
            0x8000_0000 | (0xe8de << 15) | 0x1234
        );
        // Read select: reg << 15, no flag (__r8168_mac_ocp_read).
        assert_eq!(mac_ocp_read_command(0xc0bc), 0xc0bc << 15);
    }

    #[test]
    fn xid_decode_matches_the_r8169_probe() {
        // xid = (TxConfig >> 20) & 0xfcf; 8125A = 0x609 (VER_61), 8125B = 0x641
        // (VER_63) under the table's 0x7cf mask.
        assert_eq!(xid_from_tx_config(0x609 << 20), 0x609);
        assert!(xid_is_8125a(0x609));
        assert!(!xid_is_8125a(0x641));
        // The mask strips the 10BASE-T-lite / fuzz bits the table ignores.
        assert_eq!(xid_from_tx_config(0x641 << 20 | 0xfffff), 0x641);
    }

    #[test]
    fn counter_offsets_match_the_r8169_struct_layout() {
        // Walk the cited struct field by field and check the recorded offsets.
        let mut at = 0u64;
        assert_eq!(counters::TX_PACKETS, at);
        at += 8; // tx_packets
        assert_eq!(counters::RX_PACKETS, at);
        at += 8; // rx_packets
        assert_eq!(counters::TX_ERRORS, at);
        at += 8; // tx_errors
        assert_eq!(counters::RX_ERRORS, at);
        at += 4; // rx_errors
        assert_eq!(counters::RX_MISSED, at);
        at += 2 + 2 + 4 + 4; // rx_missed, align_errors, one_collision, multi_collision
        assert_eq!(counters::RX_UNICAST, at);
        at += 8;
        assert_eq!(counters::RX_BROADCAST, at);
        at += 8;
        assert_eq!(counters::RX_MULTICAST, at);
    }

    #[test]
    fn isr_bits_match_the_cited_interrupt_status_layout() {
        // r8169 InterruptStatusBits: RxOK 0x01 … TxDescUnavail 0x80.
        assert_eq!(bits::ISR_RX_OK, 0x0001);
        assert_eq!(bits::ISR_RX_DESC_UNAVAIL, 0x0010);
        assert_eq!(bits::ISR_RX_FIFO_OVER, 0x0040);
        assert_eq!(bits::ISR_TX_OK, 0x0004);
        // RxMaxSize: R8169_RX_BUF_SIZE (SZ_16K - 1) + 1.
        assert_eq!(bits::RX_MAX_SIZE_VALUE, 0x4000);
    }

    #[test]
    fn ownership_bits_match_the_cited_registers() {
        // MCU (0xd3): NOW_IS_OOB bit 7, TX/RX_EMPTY bits 5/4, LINK_LIST_RDY bit 1
        // (r8169; rge's word-0xd2 bit-9 link-list wait is the same physical bit).
        assert_eq!(bits::MCU_NOW_IS_OOB, 0x80);
        assert_eq!(bits::MCU_TX_EMPTY | bits::MCU_RX_EMPTY, 0x30);
        assert_eq!(bits::MCU_LINK_LIST_RDY, 0x02);
        // RXDV gate: byte 0xf2 bit 3 == MISC (0xf0) bit 19.
        assert_eq!(reg::RXDV_GATE_BYTE, 0xf2);
        assert_eq!(bits::RXDV_GATE, 0x08);
        assert_eq!((reg::RXDV_GATE_BYTE - 0xf0) * 8 + 3, 19);
    }
}
