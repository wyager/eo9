//! RK3588 DW-PCIe on the Orange Pi 5 Plus: the two controllers serving the onboard
//! 2.5 GbE RTL8125 NICs (hardware goal #2, docs/board/orange-pi-5-plus.md).
//!
//! This module owns the board constants and (once `init` runs) the whole bring-up:
//! power-domain check, CRU clock/reset pokes, naneng-combphy configuration, PERST
//! sequencing, LTSSM start, link wait, and the DW root-port setup — after which the
//! generic `ConfigAccess` shim in `src/pci.rs` (the [`crate::pci::DwPcie`] statics below)
//! serves configuration space and `lspci` sees the NICs.
//!
//! ## Which controllers — and what firmware leaves behind
//!
//! Mainline `rk3588-orangepi-5-plus.dts` is unambiguous about the port map:
//!
//! | controller  | dts comment                | combphy        | PERST           | what     |
//! |-------------|----------------------------|----------------|-----------------|----------|
//! | `pcie2x1l0` | "phy1 - M.KEY socket"      | combphy1       | GPIO4_A5        | M.2 wifi |
//! | `pcie2x1l1` | "phy2 - right ethernet"    | combphy2_psu   | GPIO3_B3        | **NIC**  |
//! | `pcie2x1l2` | "phy0 - left ethernet"     | combphy0_ps    | GPIO4_A2        | **NIC**  |
//! | `pcie3x4`   | (M.2 M-key, x4)            | pcie30phy      | GPIO4_B6        | NVMe     |
//!
//! **The vendor U-Boot pre-initializes none of the NIC controllers.** Its control FDT
//! (captured from the board, `.claude/board-bringup/vendor-control-fdt.dtb`) contains
//! exactly one PCIe node — `pcie@fe150000`, the `pcie3x4` M.2 slot — so the boot log's
//! "PCIe-0 Link Fail" was U-Boot probing the (empty) M.2 socket. The `pcie2x1l1`/
//! `pcie2x1l2` controllers and their combphys get no firmware setup whatsoever: this
//! module must do clocks, resets, PHY mode, refclk, PERST and LTSSM itself.
//!
//! ## Constants table (every value cited; Linux v6.12 sources)
//!
//! Shared blocks:
//!
//! | block | base | source |
//! |---|---|---|
//! | CRU | `0xfd7c_0000` | rk3588-base.dtsi `cru: clock-controller@fd7c0000` |
//! | PHP-CRU offsets | CRU + `0x8000` | drivers/clk/rockchip/clk.h `RK3588_PHP_CRU_BASE` |
//! | PMU | `0xfd8d_8000` | rk3588-base.dtsi `pmu: power-management@fd8d8000` |
//! | php_grf | `0xfd5b_0000` | rk3588-base.dtsi `php_grf: syscon@fd5b0000` |
//! | pipe_phy0_grf | `0xfd5b_c000` | rk3588-base.dtsi `pipe_phy0_grf: syscon@fd5bc000` |
//! | pipe_phy2_grf | `0xfd5c_4000` | rk3588-base.dtsi `pipe_phy2_grf: syscon@fd5c4000` |
//! | combphy0 mmio | `0xfee0_0000` | rk3588-base.dtsi `combphy0_ps: phy@fee00000` |
//! | combphy2 mmio | `0xfee2_0000` | rk3588-base.dtsi `combphy2_psu: phy@fee20000` |
//! | GPIO3 | `0xfec4_0000` | rk3588-base.dtsi `gpio3: gpio@fec40000` |
//! | GPIO4 | `0xfec5_0000` | rk3588-base.dtsi `gpio4: gpio@fec50000` |
//!
//! Per controller (rk3588-base.dtsi `pcie@fe180000` / `pcie@fe190000` `reg` and `ranges`):
//!
//! | | pcie2x1l1 (right NIC, segment 0) | pcie2x1l2 (left NIC, segment 1) |
//! |---|---|---|
//! | DBI | `0x0a_40c0_0000` (+4 MiB) | `0x0a_4100_0000` (+4 MiB) |
//! | APB ("client") | `0xfe18_0000` | `0xfe19_0000` |
//! | config aperture | `0xf300_0000` (+1 MiB) | `0xf400_0000` (+1 MiB) |
//! | 32-bit mem window | `0xf320_0000..0xf400_0000` | `0xf420_0000..0xf500_0000` |
//! | combphy / id | combphy2 / 2 | combphy0 / 0 |
//! | PERST GPIO | GPIO3_B3 (pin 11) | GPIO4_A2 (pin 2) |
//! | legacy INTx SPI | 245 (edge rising) | 250 (edge rising) |
//!
//! (PERST gpios + the shared NIC 3.3 V rail from rk3588-orangepi-5-plus.dts:
//! `vcc3v3_pcie_eth` is gated by **GPIO3_B4, active LOW**, 50 ms startup delay.)
//!
//! CRU pokes (all hiword-mask registers; gate bit 1 = gated, reset bit 1 = held in
//! reset; offsets per clk.h `RK3588_CLKGATE_CON(x) = 0x800+4x`, `RK3588_SOFTRST_CON(x)
//! = 0xa00+4x`, `RK3588_CLKSEL_CON(x) = 0x300+4x`, PHP variants at +0x8000; bit
//! positions per drivers/clk/rockchip/clk-rk3588.c and rst-rk3588.c):
//!
//! | what | register | bits |
//! |---|---|---|
//! | shared roots: PCLK_PHP_ROOT g0, ACLK_PCIE_ROOT g6, ACLK_PHP_ROOT g7, ACLK_PCIE_BRIDGE g8 | GATE_CON32 `0x880` | 0,6,7,8 |
//! | l1: ACLK_PCIE_1L1_DBI g0 / MSTR g5 / SLV g10 / PCLK_PCIE_1L1 g15 | GATE_CON33 `0x884` | 0,5,10,15 |
//! | l2: ACLK_PCIE_1L2_DBI g1 / MSTR g6 / SLV g11 | GATE_CON33 `0x884` | 1,6,11 |
//! | l2 PCLK_PCIE_1L2 g0; CLK_PCIE_AUX3 g4 (l1), AUX4 g5 (l2); ACLK_MMU_PCIE g7, ACLK_MMU_PHP g8 | GATE_CON34 `0x888` | 0,4,5,7,8 |
//! | pipe taps: CLK_PIPEPHY0_PIPE_G g3, CLK_PIPEPHY2_PIPE_G g5, CLK_PCIE1L2_PIPE g13, CLK_PCIE1L1_PIPE g15 | GATE_CON38 `0x898` | 3,5,13,15 |
//! | phy refclk: CLK_REF_PIPE_PHY0_PLL_SRC g3, CLK_REF_PIPE_PHY2_PLL_SRC g5 | GATE_CON77 `0x934` | 3,5 |
//! | phy apb: PCLK_PCIE_COMBO_PIPE_PHY0 g5, _PHY2 g7 | PHP GATE_CON0 `0x8800` | 5,7 |
//! | refclk divs: phy0 div bits[5:0] of SEL176 `0x5c0`; phy2 div bits[5:0], phy0 mux bit6, phy2 mux bit8 of SEL177 `0x5c4` | | div 10 (PPLL 1100 MHz / 11 = 100 MHz, DT `assigned-clock-rates`), mux 1 = pll src |
//! | controller resets: SRST_PCIE3_POWER_UP b0 (l1), SRST_PCIE4_POWER_UP b1 (l2), SRST_P_PCIE3 b15 (l1) | SOFTRST_CON33 `0xa84` | rst-rk3588.c 33,0 / 33,1 / 33,15 |
//! | controller reset: SRST_P_PCIE4 b0 (l2) | SOFTRST_CON34 `0xa88` | rst-rk3588.c 34,0 |
//! | phy resets: SRST_REF_PIPE_PHY0 b6, SRST_REF_PIPE_PHY2 b8 | SOFTRST_CON77 `0xb34` | rst-rk3588.c 77,6 / 77,8 |
//! | phy apb resets: SRST_P_PCIE2_PHY0 b5, SRST_P_PCIE2_PHY2 b7 | PHP SOFTRST_CON0 `0x8a00` | rst-rk3588.c PHPTOPCRU 0,5 / 0,7 |
//!
//! Power domain: PD_PCIE = bit 7 of PMU `pwr_offset 0x14c + 0x4` (pm-domains.c
//! `rk3588_pmu` + `DOMAIN_RK3588("pcie", 0x4, BIT(7), …)`); hiword-mask, 1 = gated off.
//! U-Boot powered it to probe `pcie3x4`, so the expected day-one reading is ON — `init`
//! verifies and force-ungates with a diagnostic if not.
//!
//! ## INTx design note (the follow-up the RTL8125 driver lane picks up)
//!
//! Each DW controller delivers all four INTx pins on a single GIC SPI (245 / 250, edge
//! rising — the `legacy` interrupt of the `pcie2x1l1_intc` / `pcie2x1l2_intc` child
//! nodes). The demux is the controller's `PCIE_CLIENT_INTR_STATUS_LEGACY` APB register
//! (offset 0x8, one status bit per pin; mask register at 0x1c, hiword-mask — mainline
//! `rockchip_pcie_intx_handler`). Wiring it into the shared swizzle model means
//! per-(segment, line) mask/record state instead of the four global gpex lines; deferred
//! with `arch::pci_intx::WIRED = false` until the driver needs interrupts (the lspci
//! milestone is pure config space).

use crate::pci::DwPcie;

// -----------------------------------------------------------------------------------------
// The ConfigAccess controller statics (segment order = src/pci.rs CONTROLLERS order)
// -----------------------------------------------------------------------------------------

/// Segment 0: pcie2x1l1, the right ethernet port.
/// DBI / config-aperture values per the constants table above.
pub(crate) static PCIE2X1L1: DwPcie = DwPcie::new(0x0a_40c0_0000, 0xf300_0000, 0x10_0000);

/// Segment 1: pcie2x1l2, the left ethernet port.
pub(crate) static PCIE2X1L2: DwPcie = DwPcie::new(0x0a_4100_0000, 0xf400_0000, 0x10_0000);

/// 32-bit non-prefetchable MMIO windows (BAR assignment), per controller `ranges`.
pub(crate) const PCIE2X1L1_MEM: (usize, usize) = (0xf320_0000, 0xf400_0000);
pub(crate) const PCIE2X1L2_MEM: (usize, usize) = (0xf420_0000, 0xf500_0000);
