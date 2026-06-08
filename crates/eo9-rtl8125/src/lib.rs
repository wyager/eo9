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
    /// `ChipCmd` bits (r8169 `enum rtl_register_content`).
    pub const CMD_RESET: u64 = 0x10;
    pub const CMD_RX_ENABLE: u64 = 0x08;
    pub const CMD_TX_ENABLE: u64 = 0x04;

    /// `TxConfig` value: unlimited DMA burst (7 << 8, `TX_DMA_BURST`) + the standard
    /// inter-frame gap (3 << 24, `InterFrameGap`) — r8169
    /// `rtl_set_tx_config_registers`.
    pub const TX_CONFIG_VALUE: u64 = (7 << 8) | (3 << 24);

    /// `RxConfig` base for the 8125: `RX_FETCH_DFLT_8125` (8 << 27) |
    /// `RX_DMA_BURST` (7 << 8) — r8169 `rtl_init_rxcfg`, the `RTL_GIGA_MAC_VER_61`
    /// arm. (The 8125B arm adds `RX_PAUSE_SLOT_ON`; omitted — pause handling is not
    /// configured anywhere in this driver, and the bit is reserved on the 8125A.)
    pub const RX_CONFIG_BASE: u64 = (8 << 27) | (7 << 8);
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
        // RxConfig base: (8 << 27) | (7 << 8) (rtl_init_rxcfg, VER_61 arm).
        assert_eq!(bits::RX_CONFIG_BASE, 0x4000_0700);
    }
}
