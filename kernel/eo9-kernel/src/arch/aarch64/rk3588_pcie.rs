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

use crate::kprintln;
use crate::mmio;
use crate::pci::{DwPcie, dw_state};

use super::timer::delay_us;

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

// -----------------------------------------------------------------------------------------
// Shared block bases (constants table above)
// -----------------------------------------------------------------------------------------

const CRU: usize = 0xfd7c_0000;
const PMU: usize = 0xfd8d_8000;
const PHP_GRF: usize = 0xfd5b_0000;
const GPIO3: usize = 0xfec4_0000;
const GPIO4: usize = 0xfec5_0000;

/// PD_PCIE power-gate register: PMU + `rk3588_pmu.pwr_offset` (0x14c) + the domain's
/// `p_offset` (0x4); bit 7, hiword-mask, 1 = gated off (pm-domains.c).
const PD_PCIE_PWR_REG: usize = PMU + 0x14c + 0x4;
const PD_PCIE_BIT: u32 = 1 << 7;

// -----------------------------------------------------------------------------------------
// Low-level pokes (every register here is a Rockchip hiword-mask register unless noted)
// -----------------------------------------------------------------------------------------

/// Write a hiword-mask register: `bits` of `mask` are written, other bits untouched.
fn hiword(address: usize, mask: u16, bits: u16) {
    // SAFETY: all callers pass CRU/GRF/PMU/GPIO register addresses inside the
    // identity-mapped device gigabyte (mmu.rs DEVICE_L1 entry 3); volatile dword writes
    // there are sound, and the hiword mask makes the write field-precise.
    unsafe { mmio::write_u32(address, (u32::from(mask) << 16) | u32::from(bits)) };
}

/// Ungate clocks: CRU gate bits are 1 = gated, write 0s under the mask (clk-rk3588.c).
fn ungate(cru_offset: usize, mask: u16) {
    hiword(CRU + cru_offset, mask, 0);
}

/// Assert soft resets: 1 = held in reset (rst-rk3588.c).
fn reset_assert(cru_offset: usize, mask: u16) {
    hiword(CRU + cru_offset, mask, mask);
}

/// Deassert soft resets.
fn reset_deassert(cru_offset: usize, mask: u16) {
    hiword(CRU + cru_offset, mask, 0);
}

/// Drive one GPIO pin as an output at the given level. RK3588 GPIO banks are the
/// Rockchip v2 layout (drivers/gpio/gpio-rockchip.c): `SWPORT_DR_L/H` at 0x00/0x04 and
/// `SWPORT_DDR_L/H` at 0x08/0x0c, all hiword-mask; pins 0-15 in the L register.
fn gpio_drive(bank: usize, pin: u32, high: bool) {
    let (half, bit) = if pin < 16 { (0, pin) } else { (4, pin - 16) };
    let mask = 1u16 << bit;
    // Level first, then direction, so the pad never glitches through the wrong level.
    hiword(bank + half, mask, if high { mask } else { 0 });
    hiword(bank + 0x8 + half, mask, mask); // DDR: 1 = output
}

/// Plain (non-hiword) device register read/write for PHY mmio, APB and DBI blocks.
fn reg_read(address: usize) -> u32 {
    // SAFETY: callers pass addresses inside the identity-mapped device windows (the low
    // device gigabyte or the DBI gigabyte mapped by mmu.rs DEVICE_L1).
    unsafe { mmio::read_u32(address) }
}

fn reg_write(address: usize, value: u32) {
    // SAFETY: as `reg_read`.
    unsafe { mmio::write_u32(address, value) };
}

// -----------------------------------------------------------------------------------------
// Per-port descriptions
// -----------------------------------------------------------------------------------------

/// One naneng combphy in PCIe mode (phy-rockchip-naneng-combphy.c, rk3588 config).
struct Phy {
    /// PHY tuning registers (combphyX mmio block).
    mmio: usize,
    /// The phy's pipe_phyX_grf block.
    grf: usize,
    /// Refclk divider: CLKSEL register offset whose bits [5:0] divide PPLL
    /// (clk-rk3588.c CLK_REF_PIPE_PHYx_PLL_SRC), plus the mux bit in CLKSEL_CON177
    /// selecting the PLL source (1) over the 24 MHz osc (0).
    refclk_div_offset: usize,
    refclk_mux_bit: u16,
    /// (CRU offset, mask) pairs: the phy's "phy" + "apb" resets (combphy `resets`).
    resets: [(usize, u16); 2],
    /// php_grf `pcie1lX_sel` bit routing this combphy to its controller, where one exists
    /// (rk3588_combphy_grfcfgs pipe_pcie1l0/1_sel; write 0 = route to the PCIe controller).
    pipe_sel_bit: Option<u16>,
    /// The phy's APB pclk gate in PHP GATE_CON0 (PCLK_PCIE_COMBO_PIPE_PHYx).
    apb_gate_bit: u16,
    /// The refclk PLL-source gate bit in GATE_CON77 (CLK_REF_PIPE_PHYx_PLL_SRC).
    refclk_gate_bit: u16,
}

/// One DW controller + its board wiring.
struct Port {
    name: &'static str,
    dw: &'static DwPcie,
    /// The "apb" reg block (PCIE_CLIENT_* registers, pcie-dw-rockchip.c).
    apb: usize,
    /// PERST# gpio (bank base, pin). dts `reset-gpios`; driven 0 during setup, 1 to
    /// release the endpoint (mainline `rockchip_pcie_start_link` polarity).
    perst: (usize, u32),
    /// (CRU offset, mask) controller clock gates to open, beyond the shared roots.
    gates: [(usize, u16); 3],
    /// (CRU offset, mask) controller resets ("pwr" + "pipe" in the dts `resets`).
    resets: [(usize, u16); 2],
    /// 32-bit non-prefetchable window (== the pci.rs segment's BAR window): programmed
    /// into the root port's type-1 memory base/limit and an identity MEM iATU region.
    mem: (usize, usize),
    phy: Phy,
}

/// pcie2x1l1 — segment 0, the right ethernet port (combphy2).
static PORT_L1: Port = Port {
    name: "pcie2x1l1/right",
    dw: &PCIE2X1L1,
    apb: 0xfe18_0000,
    perst: (GPIO3, 11), // GPIO3_B3
    gates: [
        // ACLK_PCIE_1L1_DBI g0 + MSTR g5 + SLV g10 + PCLK_PCIE_1L1 g15 (GATE_CON33).
        (0x884, (1 << 0) | (1 << 5) | (1 << 10) | (1 << 15)),
        // CLK_PCIE_AUX3 g4 (GATE_CON34).
        (0x888, 1 << 4),
        // CLK_PIPEPHY2_PIPE_G g5 + CLK_PCIE1L1_PIPE g15 (GATE_CON38).
        (0x898, (1 << 5) | (1 << 15)),
    ],
    // SRST_PCIE3_POWER_UP (33,0) + SRST_P_PCIE3 (33,15) — both in SOFTRST_CON33.
    resets: [(0xa84, 1 << 0), (0xa84, 1 << 15)],
    mem: PCIE2X1L1_MEM,
    phy: Phy {
        mmio: 0xfee2_0000,
        grf: 0xfd5c_4000,         // pipe_phy2_grf
        refclk_div_offset: 0x5c4, // CLKSEL_CON177 [5:0] (CLK_REF_PIPE_PHY2_PLL_SRC)
        refclk_mux_bit: 1 << 8,   // CLKSEL_CON177 bit 8 (CLK_REF_PIPE_PHY2 mux)
        // SRST_REF_PIPE_PHY2 (77,8) + SRST_P_PCIE2_PHY2 (PHPTOP 0,7).
        resets: [(0xb34, 1 << 8), (0x8a00, 1 << 7)],
        pipe_sel_bit: Some(1 << 1), // php_grf pipe_pcie1l1_sel (combphy id 2)
        apb_gate_bit: 1 << 7,       // PCLK_PCIE_COMBO_PIPE_PHY2
        refclk_gate_bit: 1 << 5,    // CLK_REF_PIPE_PHY2_PLL_SRC
    },
};

/// pcie2x1l2 — segment 1, the left ethernet port (combphy0).
static PORT_L2: Port = Port {
    name: "pcie2x1l2/left",
    dw: &PCIE2X1L2,
    apb: 0xfe19_0000,
    perst: (GPIO4, 2), // GPIO4_A2
    gates: [
        // ACLK_PCIE_1L2_DBI g1 + MSTR g6 + SLV g11 (GATE_CON33).
        (0x884, (1 << 1) | (1 << 6) | (1 << 11)),
        // PCLK_PCIE_1L2 g0 + CLK_PCIE_AUX4 g5 (GATE_CON34).
        (0x888, (1 << 0) | (1 << 5)),
        // CLK_PIPEPHY0_PIPE_G g3 + CLK_PCIE1L2_PIPE g13 (GATE_CON38).
        (0x898, (1 << 3) | (1 << 13)),
    ],
    // SRST_PCIE4_POWER_UP (33,1) + SRST_P_PCIE4 (34,0).
    resets: [(0xa84, 1 << 1), (0xa88, 1 << 0)],
    mem: PCIE2X1L2_MEM,
    phy: Phy {
        mmio: 0xfee0_0000,
        grf: 0xfd5b_c000,         // pipe_phy0_grf
        refclk_div_offset: 0x5c0, // CLKSEL_CON176 [5:0] (CLK_REF_PIPE_PHY0_PLL_SRC)
        refclk_mux_bit: 1 << 6,   // CLKSEL_CON177 bit 6 (CLK_REF_PIPE_PHY0 mux)
        // SRST_REF_PIPE_PHY0 (77,6) + SRST_P_PCIE2_PHY0 (PHPTOP 0,5).
        resets: [(0xb34, 1 << 6), (0x8a00, 1 << 5)],
        pipe_sel_bit: None, // combphy0 (id 0) has no pipe sel — hardwired to pcie2x1l2
        apb_gate_bit: 1 << 5, // PCLK_PCIE_COMBO_PIPE_PHY0
        refclk_gate_bit: 1 << 3, // CLK_REF_PIPE_PHY0_PLL_SRC
    },
};

// -----------------------------------------------------------------------------------------
// APB (PCIE_CLIENT) and DBI port-logic registers (pcie-dw-rockchip.c / pcie-designware.h)
// -----------------------------------------------------------------------------------------

const CLIENT_GENERAL_CON: usize = 0x0;
const CLIENT_GENERAL_DEBUG: usize = 0x104;
const CLIENT_HOT_RESET_CTRL: usize = 0x180;
const CLIENT_LTSSM_STATUS: usize = 0x300;
/// HIWORD_UPDATE_BIT(0x40): RC device type (PCIE_CLIENT_RC_MODE).
const RC_MODE: u32 = (0x40 << 16) | 0x40;
/// HIWORD_UPDATE_BIT(0xc): LTSSM enable (PCIE_CLIENT_ENABLE_LTSSM).
const ENABLE_LTSSM: u32 = (0xc << 16) | 0xc;
/// HIWORD_UPDATE_BIT(BIT(4)): app_ltssm_enable controls LTSSM (PCIE_LTSSM_ENABLE_ENHANCE).
const LTSSM_ENABLE_ENHANCE: u32 = (0x10 << 16) | 0x10;
/// LTSSM_STATUS: smlh/rdlh link-up flags + the [5:0] LTSSM state (0x11 = L0).
const SMLH_LINKUP: u32 = 1 << 16;
const RDLH_LINKUP: u32 = 1 << 17;
const LTSSM_STATE_MASK: u32 = 0x3f;
const LTSSM_L0: u32 = 0x11;

/// DBI port-logic offsets (pcie-designware.h).
const PORT_LINK_CONTROL: usize = 0x710; // LINK_CAPABLE in [21:16]
const LINK_WIDTH_SPEED_CONTROL: usize = 0x80c; // NUM_OF_LANES in [12:8], bit 17 speed change
const MISC_CONTROL_1: usize = 0x8bc; // bit 0 = DBI_RO_WR_EN

/// Unrolled iATU outbound region 1, used as the permanent identity MEM window (region 0
/// belongs to the config shim — src/pci.rs `dw_regs`).
const ATU_OB1: usize = 0x30_0000 + 0x200;
const ATU_TYPE_MEM: u32 = 0x0;
const ATU_ENABLE: u32 = 1 << 31;

// -----------------------------------------------------------------------------------------
// Bring-up
// -----------------------------------------------------------------------------------------

/// Bring up both NIC controllers. Called once from `kmain` after the MMU, timer and
/// watchdog are alive (the LTSSM wait uses bounded timer delays; the watchdog covers the
/// pathological cases like a powered-down bus hanging an APB read — every step prints
/// *before* its first touch of a new block so a wedge is attributable from the console).
pub(crate) fn init() {
    // --- power domain ----------------------------------------------------------------
    // U-Boot powered PD_PCIE to probe pcie3x4 (the M.2 slot in its control FDT), so the
    // expected reading is ON (bit clear). If it is off, poke it on the way mainline's
    // rockchip_do_pmu_set_power_domain does (hiword write, no bus-idle request needed:
    // the PD_PCIE domain entry has req == 0) and give it a settle.
    let pd = reg_read(PD_PCIE_PWR_REG);
    if pd & PD_PCIE_BIT != 0 {
        kprintln!("pcie: PD_PCIE was gated off (pwr {pd:#010x}) — powering on");
        hiword(PD_PCIE_PWR_REG, PD_PCIE_BIT as u16, 0);
        delay_us(1_000);
        let pd_after = reg_read(PD_PCIE_PWR_REG);
        if pd_after & PD_PCIE_BIT != 0 {
            kprintln!("pcie: PD_PCIE still gated ({pd_after:#010x}) — skipping bring-up");
            return;
        }
    } else {
        kprintln!("pcie: PD_PCIE on (pwr {pd:#010x})");
    }

    // --- PPLL diagnostic --------------------------------------------------------------
    // The 100 MHz combphy refclk divides PPLL; the divider below assumes the 1100 MHz
    // vendor/mainline rate (rk3588_pll_rates: m=550 p=3 s=2 k=0 → 24 MHz·550/3/4). PPLL
    // lives in the PHP CRU block: PLL_CON(128) = CRU + 0x8000 + 128·4 = +0x8200
    // (clk-rk3588.c RK3588_PMU_PLL_CON), mode in MODE_CON0 (+0x280) bits [11:10]
    // (1 = normal). rate = 24 MHz · m / p / 2^s (clk-pll.c rockchip_rk3588_pll).
    let con0 = reg_read(CRU + 0x8200);
    let con1 = reg_read(CRU + 0x8204);
    let mode = (reg_read(CRU + 0x280) >> 10) & 0x3;
    let (m, p, s) = (con0 & 0x3ff, con1 & 0x3f, (con1 >> 8) & 0x3f);
    let ppll_hz = if p != 0 {
        ((24_000_000u64 * u64::from(m)) / u64::from(p)) >> s
    } else {
        0
    };
    kprintln!(
        "pcie: ppll m={m} p={p} s={s} mode={mode} -> {ppll_hz} Hz, refclk div {}{}",
        refclk_div(),
        if ppll_hz == 1_100_000_000 && mode == 1 {
            ""
        } else {
            " (UNEXPECTED ppll — expected 1100 MHz in normal mode; check the div above)"
        }
    );

    // --- the shared NIC 3.3 V rail ----------------------------------------------------
    // vcc3v3_pcie_eth feeds both RTL8125s; gated by GPIO3_B4, ACTIVE LOW, 50 ms startup
    // delay (rk3588-orangepi-5-plus.dts). GPIO bank pclks: PCLK_GPIO3 g2 / PCLK_GPIO4 g4
    // in GATE_CON17 (clk-rk3588.c) — opened first so the writes take.
    ungate(0x844, (1 << 2) | (1 << 4));
    gpio_drive(GPIO3, 12, false);
    kprintln!("pcie: NIC 3v3 rail enabled (GPIO3_B4 low), settling 50 ms");
    delay_us(50_000);

    // --- shared clock roots -------------------------------------------------------------
    // PCLK_PHP_ROOT g0, ACLK_PCIE_ROOT g6, ACLK_PHP_ROOT g7, ACLK_PCIE_BRIDGE g8
    // (GATE_CON32); ACLK_MMU_PCIE g7, ACLK_MMU_PHP g8 (GATE_CON34). All ungated here
    // once; per-port gates follow in bring_up.
    ungate(0x880, (1 << 0) | (1 << 6) | (1 << 7) | (1 << 8));
    ungate(0x888, (1 << 7) | (1 << 8));

    bring_up(&PORT_L1);
    bring_up(&PORT_L2);
}

/// The PPLL→100 MHz refclk divider (1-based): exact division of the decoded PPLL rate
/// when it has one within the 6-bit divider range, else the 1100 MHz default (/11).
fn refclk_div() -> u32 {
    let con0 = reg_read(CRU + 0x8200);
    let con1 = reg_read(CRU + 0x8204);
    let (m, p, s) = (con0 & 0x3ff, con1 & 0x3f, (con1 >> 8) & 0x3f);
    if p != 0 {
        let hz = ((24_000_000u64 * u64::from(m)) / u64::from(p)) >> s;
        if hz != 0 && hz % 100_000_000 == 0 && (1..=64).contains(&(hz / 100_000_000)) {
            return (hz / 100_000_000) as u32;
        }
    }
    11
}

/// The mainline-ordered single-controller sequence (rockchip_pcie_probe →
/// rockchip_combphy_init → rockchip_pcie_configure_rc → dw setup → start_link).
fn bring_up(port: &Port) {
    kprintln!(
        "pcie[{}]: bring-up (apb {:#x}, dbi {:#x})",
        port.name,
        port.apb,
        port.dw.dbi()
    );

    // PERST# low while everything resets (devm_gpiod_get(…, GPIOD_OUT_LOW) at probe).
    gpio_drive(port.perst.0, port.perst.1, false);

    // Controller resets asserted before PHY init (probe: reset_control_assert(rst)).
    for (offset, mask) in port.resets {
        reset_assert(offset, mask);
    }

    // --- combphy in PCIe mode (rockchip_combphy_init / rk3588_combphy_cfg) ------------
    let phy = &port.phy;
    // Probe-time state: PHY resets asserted (combphy probe: reset_control_assert).
    for (offset, mask) in phy.resets {
        reset_assert(offset, mask);
    }
    // PHY clocks: APB pclk (PHP GATE_CON0) + the PPLL-sourced refclk gate (GATE_CON77).
    ungate(0x8800, phy.apb_gate_bit);
    ungate(0x934, phy.refclk_gate_bit);
    // Refclk = 100 MHz: divider [5:0] (value = div − 1, the Rockchip divider convention)
    // and the mux to the PLL source (DT assigned-clock-rates = 100 MHz on
    // CLK_REF_PIPE_PHYx). The divider is derived from the PPLL the diagnostic above
    // decoded, falling back to /11 (the mainline 1100 MHz rate) if PPLL looks odd.
    hiword(CRU + phy.refclk_div_offset, 0x3f, (refclk_div() - 1) as u16);
    hiword(CRU + 0x5c4, phy.refclk_mux_bit, phy.refclk_mux_bit);
    // GRF mode select (rk3588_combphy_grfcfgs con0..3_for_pcie; full-width hiword writes).
    hiword(phy.grf + 0x0, 0xffff, 0x1000);
    hiword(phy.grf + 0x4, 0xffff, 0x0000);
    hiword(phy.grf + 0x8, 0xffff, 0x0101);
    hiword(phy.grf + 0xc, 0xffff, 0x0200);
    // Route the combphy pipe to its PCIe controller (php_grf pipe_pcie1lX_sel, value 0).
    if let Some(bit) = phy.pipe_sel_bit {
        hiword(PHP_GRF + 0x100, bit, 0);
    }
    // 100 MHz refclk select: pipe_clk_100m = grf+0x4 bits [14:13] = 2.
    hiword(phy.grf + 0x4, 0x6000, 0x4000);
    // PCIe @ 100 MHz tuning (rk3588_combphy_cfg REF_CLOCK_100MHz arm): PLL KVCO
    // (PHYREG33 [4:2] = 4), random-jitter control (PHYREG12 = 4), rx_trim (PHYREG27 =
    // 0x4c), su_trim (PHYREG11 = 0xf0). Plain registers, not hiword.
    let reg33 = reg_read(phy.mmio + 0x80);
    reg_write(phy.mmio + 0x80, (reg33 & !0x1c) | (4 << 2));
    reg_write(phy.mmio + 0x2c, 0x4);
    reg_write(phy.mmio + 0x6c, 0x4c);
    reg_write(phy.mmio + 0x28, 0xf0);
    // (No rockchip,ext-refclk and no rockchip,enable-ssc on this board's combphy nodes.)
    // PHY out of reset (rockchip_combphy_init tail: reset_control_deassert).
    for (offset, mask) in phy.resets {
        reset_deassert(offset, mask);
    }
    delay_us(100);

    // --- controller out of reset, clocks on (probe: deassert, then clk_init) ----------
    for (offset, mask) in port.resets {
        reset_deassert(offset, mask);
    }
    for (offset, mask) in port.gates {
        ungate(offset, mask);
    }
    delay_us(100);

    // --- RC mode via the APB client block (rockchip_pcie_configure_rc) ----------------
    kprintln!("pcie[{}]: probing client block", port.name);
    reg_write(port.apb + CLIENT_HOT_RESET_CTRL, LTSSM_ENABLE_ENHANCE);
    reg_write(port.apb + CLIENT_GENERAL_CON, RC_MODE);

    // --- DBI sanity + root port setup (dw_pcie_setup_rc, the parts this kernel needs) --
    let dbi = port.dw.dbi();
    kprintln!("pcie[{}]: probing dbi", port.name);
    let id = reg_read(dbi);
    if id == 0 || id == 0xffff_ffff {
        kprintln!(
            "pcie[{}]: DBI dead (id register {id:#010x}) — controller disabled, \
             check PD/clock/reset diagnostics above",
            port.name
        );
        return;
    }
    kprintln!(
        "pcie[{}]: root port {:04x}:{:04x}",
        port.name,
        id & 0xffff,
        id >> 16
    );

    // x1 link: PORT_LINK_CONTROL.LINK_CAPABLE = 1, GEN2_CTRL.NUM_OF_LANES = 1 +
    // direct speed change (dw_pcie_setup / dw_pcie_setup_rc with num-lanes = 1).
    let plc = reg_read(dbi + PORT_LINK_CONTROL);
    reg_write(dbi + PORT_LINK_CONTROL, (plc & !(0x3f << 16)) | (1 << 16));
    let wsc = reg_read(dbi + LINK_WIDTH_SPEED_CONTROL);
    reg_write(
        dbi + LINK_WIDTH_SPEED_CONTROL,
        (wsc & !(0x1f << 8)) | (1 << 8) | (1 << 17),
    );

    // Root port config header (write-protected fields behind DBI_RO_WR_EN, 0x8bc bit 0):
    // class = PCI bridge (06.04.00, keeping the revision byte) — the DW core resets to a
    // device class some OSes reject (dw_pcie_setup_rc does the same fix).
    let misc = reg_read(dbi + MISC_CONTROL_1);
    reg_write(dbi + MISC_CONTROL_1, misc | 1);
    let class = reg_read(dbi + 0x08);
    reg_write(dbi + 0x08, 0x0604_0000 | (class & 0xff));
    reg_write(dbi + MISC_CONTROL_1, misc & !1);

    // Bus numbers: primary 0, secondary 1, subordinate 0xf (the shim's 16-bus segment).
    reg_write(dbi + 0x18, 0x000f_0100);
    // Root port BARs unused (dw_pcie_setup_rc zeroes them).
    reg_write(dbi + 0x10, 0);
    reg_write(dbi + 0x14, 0);
    // Type-1 windows: forward the 32-bit mem window downstream, IO + prefetchable closed
    // (base > limit). Fields per the PCI-to-PCI bridge spec; setup_rc programs these from
    // its windows the same way.
    let mem_base16 = (port.mem.0 >> 16) as u32; // 0xf320 — bits [31:20] in the field
    let mem_limit16 = ((port.mem.1 - 1) >> 16) as u32;
    reg_write(dbi + 0x20, (mem_limit16 << 16) | mem_base16);
    reg_write(dbi + 0x24, 0x0000_fff0); // prefetchable: base 0xfff0 > limit 0 = closed
    reg_write(dbi + 0x1c, 0x0000_00f0); // IO: base 0xf0 > limit 0 = closed
    // Command: IO/MEM decode + bus master + SERR, as dw_pcie_setup_rc leaves the port.
    reg_write(dbi + 0x04, 0x107);

    // Permanent identity MEM window in outbound iATU region 1 (region 0 is the config
    // shim's): CPU window == bus addresses, so BARs the kernel assigns from this range
    // (src/pci.rs per-segment allocator) decode with no translation. Register layout as
    // in dw_pcie_prog_outbound_atu (unrolled).
    reg_write(dbi + ATU_OB1 + 0x08, port.mem.0 as u32); // lower base
    reg_write(dbi + ATU_OB1 + 0x0c, 0); // upper base
    reg_write(dbi + ATU_OB1 + 0x10, (port.mem.1 - 1) as u32); // limit
    reg_write(dbi + ATU_OB1 + 0x14, port.mem.0 as u32); // lower target
    reg_write(dbi + ATU_OB1 + 0x18, 0); // upper target
    reg_write(dbi + ATU_OB1, ATU_TYPE_MEM);
    reg_write(dbi + ATU_OB1 + 0x04, ATU_ENABLE);
    let mut atu_ok = false;
    for _ in 0..5 {
        if reg_read(dbi + ATU_OB1 + 0x04) & ATU_ENABLE != 0 {
            atu_ok = true;
            break;
        }
        delay_us(9_000); // LINK_WAIT_IATU, as in the config shim's settle
    }
    if !atu_ok {
        kprintln!("pcie[{}]: MEM iATU region never enabled", port.name);
    }

    // --- link training (rockchip_pcie_start_link) --------------------------------------
    // PERST# is still low. Enable the LTSSM, hold PERST another 100 ms (the mainline
    // driver's deliberate exaggeration of the 100 µs Tperst-clk so unknown endpoints
    // finish their own reset), then release.
    reg_write(port.apb + CLIENT_GENERAL_CON, ENABLE_LTSSM);
    delay_us(100_000);
    gpio_drive(port.perst.0, port.perst.1, true);

    // Bounded link wait: ~1.1 s in 60 polls (dw_pcie_wait_for_link is 10 × 90 ms;
    // a 2.5 GT/s x1 partner typically trains in well under 100 ms).
    let mut ltssm = 0;
    for _ in 0..60 {
        delay_us(18_000);
        ltssm = reg_read(port.apb + CLIENT_LTSSM_STATUS);
        if ltssm & (SMLH_LINKUP | RDLH_LINKUP) == (SMLH_LINKUP | RDLH_LINKUP)
            && ltssm & LTSSM_STATE_MASK == LTSSM_L0
        {
            break;
        }
    }

    if ltssm & (SMLH_LINKUP | RDLH_LINKUP) == (SMLH_LINKUP | RDLH_LINKUP)
        && ltssm & LTSSM_STATE_MASK == LTSSM_L0
    {
        port.dw.set_state(dw_state::FULL);
        // Link speed/width from the root port's PCIe capability Link Status (cap walk
        // through the DBI: status bit 4 promises a list at 0x34; PCIe cap id 0x10;
        // LNKSTA at cap + 0x12 — speed [3:0] as gen, width [9:4]).
        let (speed, width) = link_status(dbi).unwrap_or((0, 0));
        kprintln!(
            "pcie[{}]: link UP gen{speed} x{width} (ltssm {ltssm:#x})",
            port.name
        );
    } else {
        port.dw.set_state(dw_state::ROOT_ONLY);
        kprintln!(
            "pcie[{}]: link DOWN after 1.1 s (ltssm {ltssm:#x}, debug {:#x}) — \
             root port stays visible, no endpoint",
            port.name,
            reg_read(port.apb + CLIENT_GENERAL_DEBUG)
        );
    }
}

/// (speed-gen, lanes) from the root port's PCIe capability Link Status register, via the DBI.
fn link_status(dbi: usize) -> Option<(u32, u32)> {
    if reg_read(dbi + 0x04) & (1 << 20) == 0 {
        return None; // status bit 4 (dword 0x04 bit 20): no capability list
    }
    let mut pointer = reg_read(dbi + 0x34) & 0xfc;
    for _ in 0..48 {
        if pointer == 0 {
            return None;
        }
        let header = reg_read(dbi + pointer as usize);
        if header & 0xff == 0x10 {
            let lnksta = reg_read(dbi + pointer as usize + 0x10) >> 16;
            return Some((lnksta & 0xf, (lnksta >> 4) & 0x3f));
        }
        pointer = (header >> 8) & 0xfc;
    }
    None
}
