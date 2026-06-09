//! RK3588 USB2 host plumbing on the Orange Pi 5 Plus: power domain, clocks, the
//! inno-usb2 PHY host ports, and the VBUS rail for the two USB 2.0 type-A ports —
//! everything below the OHCI/EHCI register blocks the `eo9:platform` capability hands
//! to the `usb.ohci` guest driver (docs/board/usb-ohci-plan.md §2: SoC plumbing stays
//! kernel-side; CRU/GRF are never granted out).
//!
//! The controllers themselves are NOT driven here: this module makes the four
//! register blocks reachable (powered, clocked, PHY out of suspend, VBUS on) and
//! prints one read-only identity peek per controller so a bench transcript localizes
//! failures along the plan's table — no HcRevision at boot → power/clock (this
//! module); HcRevision fine but CCS never sets → VBUS vs PHY (the keyboard backlight
//! discriminates).
//!
//! ## Constants table (every value cited; Linux v6.12 sources)
//!
//! | block | base | source |
//! |---|---|---|
//! | usb_host0_ehci | `0xfc80_0000` (+0x40000) | rk3588-base.dtsi `usb@fc800000` |
//! | usb_host0_ohci | `0xfc84_0000` (+0x40000) | rk3588-base.dtsi `usb@fc840000` |
//! | usb_host1_ehci | `0xfc88_0000` (+0x40000) | rk3588-base.dtsi `usb@fc880000` |
//! | usb_host1_ohci | `0xfc8c_0000` (+0x40000) | rk3588-base.dtsi `usb@fc8c0000` |
//! | usb2phy2_grf | `0xfd5d_8000` | rk3588-base.dtsi `usb2phy2_grf: syscon@fd5d8000` |
//! | usb2phy3_grf | `0xfd5d_c000` | rk3588-base.dtsi `usb2phy3_grf: syscon@fd5dc000` |
//! | CRU | `0xfd7c_0000` | rk3588-base.dtsi `cru: clock-controller@fd7c0000` |
//! | PMU1-CRU offsets | CRU + `0x30000` | clk.h `RK3588_PMU_CRU_BASE` |
//! | PMU | `0xfd8d_8000` | rk3588-base.dtsi `pmu: power-management@fd8d8000` |
//! | GPIO3 | `0xfec4_0000` | rk3588-base.dtsi `gpio3: gpio@fec40000` |
//!
//! All four host controllers sit in the board profile's device GiB 3 (mmu.rs
//! `DEVICE_L1`), so no mapping changes (usb-ohci-plan §0).
//!
//! Clock gates (clk-rk3588.c; `RK3588_CLKGATE_CON(x) = 0x800 + 4x`, gate bit 1 =
//! gated; the controller `clocks` lists are the rk3588-base.dtsi usb_host nodes):
//!
//! | what | register | bits |
//! |---|---|---|
//! | ACLK_VO1USB_TOP_ROOT g0, HCLK_VO1USB_TOP_ROOT g2 (CLK_IS_CRITICAL — the GATE_LINK dependencies of ACLK/HCLK_USB) | GATE_CON74 `0x928` | 0, 2 |
//! | ACLK_USB_ROOT g0, HCLK_USB_ROOT g1, ACLK_USB g2, HCLK_USB g3 | GATE_CON42 `0x8a8` | 0-3 |
//! | HCLK_HOST0 g10, HCLK_HOST_ARB0 g11, HCLK_HOST1 g12, HCLK_HOST_ARB1 g13 | GATE_CON42 `0x8a8` | 10-13 |
//! | CLK_USB2PHY_HDPTXRXPHY_REF — the u2phy `phyclk`, CLK_IS_CRITICAL | PMU GATE_CON4 `0x30810` | 7 |
//! | PCLK_GPIO3 (so the VBUS write takes) | GATE_CON17 `0x844` | 2 |
//!
//! Power domain (pmdomain/rockchip/pm-domains.c `rk3588_pmu` + `DOMAIN_RK3588("usb",
//! 0x4, BIT(11), 0, 0x4, BIT(3), BIT(25), 0x4, BIT(4), BIT(20), true)`): PD_USB power
//! gate = PMU `pwr_offset 0x14c + 0x4` bit 11 (hiword, 1 = gated off; no separate
//! status register — `status_mask 0` reads back the pwr bit); bus-idle request = PMU
//! `req_offset 0x10c + 0x4` bit 4 (hiword), acknowledged in `ack_offset 0x118 + 0x4`
//! and idle state in `idle_offset 0x120 + 0x4`, mask bit 20 for both. U-Boot powers
//! the domain to run its dm-pre-reloc EHCI/OHCI nodes, so the expected day-one
//! reading is ON; the off path powers on + releases the idle request the way
//! mainline's `rockchip_pd_power(true)` does.
//!
//! u2phy host-port init (phy-rockchip-inno-usb2.c `rk3588_phy_cfgs` entries
//! `.reg = 0x8000` (u2phy2) / `0xc000` (u2phy3) — both single HOST ports; every
//! register below is an offset into the phy's own usb2phyN_grf, all hiword-mask):
//!
//! | step | register | field | source |
//! |---|---|---|---|
//! | SIDDQ off (power the analog block) | GRF+0x08 | bit 13 = 0 | `rk3588_usb2phy_tuning`: `regmap_write(grf, 0x0008, GENMASK(29,29) \| 0x0000)` |
//! | PHY reset pulse after IDDQ exit | PMU1-CRU SOFTRST_CON4 `0x30a10` | bit 9 (u2phy2) / 10 (u2phy3); assert, 10 µs, deassert, 200 µs | dtsi `resets = <&cru SRST_OTGPHY_U2_0/1>` → rst-rk3588.c `RK3588_PMU1CRU_RESET_OFFSET(…, 4, 9/10)`; timing `rockchip_usb2phy_reset` |
//! | suspend config: FS terminations + FS transceiver, normal opmode | GRF+0x0c | bits [4:0] = 0x14 | `rk3588_usb2phy_tuning` host arm (`suspend_cfg = 0x14`) |
//! | HS DC level +5.89% | GRF+0x04 | bits [11:8] = 0x9 | `rk3588_usb2phy_tuning` |
//! | HS pre-emphasis 2x | GRF+0x08 | bits [4:3] = 0b10 | `rk3588_usb2phy_tuning` |
//! | 480 MHz clkout on | GRF+0x00 | bit 0 = 0 | `rk3588_phy_cfgs.clkout_ctl {0x0000, 0, 0, 1, 0}` (enable value 0) |
//! | host port un-suspend | GRF+0x08 | bit 2 = 0 | `port_cfgs[HOST].phy_sus {0x0008, 2, 2, 0, 1}` (1 = suspended) |
//! | line-state diagnostic | GRF+0xc0 | bits [10:9] | `port_cfgs[HOST].utmi_ls` (00 = SE0, 01 = J/FS idle, 10 = K/LS idle) |
//!
//! APB resets of the phy GRFs: CRU SOFTRST_CON72 `0xb20` bit 10 (u2phy2) / 11
//! (u2phy3) — dtsi `resets = <&cru SRST_P_USB2PHY_U2_0/1_GRF0>` → rst-rk3588.c
//! `(72, 10)` / `(72, 11)`; deasserted before any GRF write.
//!
//! VBUS: `vcc5v0_usb20` is a fixed regulator feeding BOTH USB2-A ports, gated by
//! **GPIO3_B7 (pin 15), enable-active-high**, no DT startup delay
//! (rk3588-orangepi-5-plus.dts `vcc5v0-usb20-regulator`; both `u2phy2_host` and
//! `u2phy3_host` carry `phy-supply = <&vcc5v0_usb20>`) — the GPIO3_B4 NIC-rail
//! choreography, opposite polarity.
//!
//! Idempotence: every write here is either a hiword field write of the same value
//! mainline programs or a clock ungate, so the sequence is safe whether the vendor
//! U-Boot already ran `usb start` (warm: values re-written, the PHY reset pulse
//! briefly drops and re-acquires line state before any schedule exists) or not
//! (cold: this module is the only init). The defensive EHCI CONFIGFLAG clear (warm
//! U-Boot may have routed ports to EHCI — EHCI 1.0 §4.2) is the guest driver's first
//! act through its granted EHCI regions, not done here.

use crate::kprintln;
use crate::mmio;

use super::timer::delay_us;

// -----------------------------------------------------------------------------------------
// Bases (constants table above)
// -----------------------------------------------------------------------------------------

const CRU: usize = 0xfd7c_0000;
const PMU: usize = 0xfd8d_8000;
const GPIO3: usize = 0xfec4_0000;

/// The four host-controller register blocks (rk3588-base.dtsi `reg`, 0x40000 each).
pub(crate) const USB_HOST0_EHCI: usize = 0xfc80_0000;
pub(crate) const USB_HOST0_OHCI: usize = 0xfc84_0000;
pub(crate) const USB_HOST1_EHCI: usize = 0xfc88_0000;
pub(crate) const USB_HOST1_OHCI: usize = 0xfc8c_0000;
pub(crate) const USB_HOST_REGION_SIZE: u64 = 0x4_0000;

/// PD_USB power gate: PMU + pwr_offset 0x14c + p_offset 0x4, bit 11 (pm-domains.c).
const PD_USB_PWR_REG: usize = PMU + 0x14c + 0x4;
const PD_USB_BIT: u32 = 1 << 11;
/// PD_USB bus-idle request/ack/idle (pm-domains.c rk3588_pmu offsets + r_offset 0x4).
const PD_USB_REQ_REG: usize = PMU + 0x10c + 0x4;
const PD_USB_REQ_BIT: u32 = 1 << 4;
const PD_USB_ACK_REG: usize = PMU + 0x118 + 0x4;
const PD_USB_IDLE_REG: usize = PMU + 0x120 + 0x4;
const PD_USB_IDLE_BIT: u32 = 1 << 20;

/// One inno-usb2 PHY serving one EHCI/OHCI pair (rk3588_phy_cfgs host entries).
struct U2Phy {
    name: &'static str,
    /// The phy's usb2phyN_grf syscon base.
    grf: usize,
    /// "phy" reset bit in PMU1-CRU SOFTRST_CON4 (rst-rk3588.c (4, 9) / (4, 10)).
    phy_reset_bit: u16,
    /// "apb" reset bit in CRU SOFTRST_CON72 (rst-rk3588.c (72, 10) / (72, 11)).
    apb_reset_bit: u16,
}

static U2PHY2: U2Phy = U2Phy {
    name: "u2phy2/host0",
    grf: 0xfd5d_8000,
    phy_reset_bit: 1 << 9,  // SRST_OTGPHY_U2_0
    apb_reset_bit: 1 << 10, // SRST_P_USB2PHY_U2_0_GRF0
};

static U2PHY3: U2Phy = U2Phy {
    name: "u2phy3/host1",
    grf: 0xfd5d_c000,
    phy_reset_bit: 1 << 10, // SRST_OTGPHY_U2_1
    apb_reset_bit: 1 << 11, // SRST_P_USB2PHY_U2_1_GRF0
};

// -----------------------------------------------------------------------------------------
// Low-level pokes (the rk3588_pcie.rs helpers, redeclared — the modules stay
// independent so either lane can change without re-verifying the other)
// -----------------------------------------------------------------------------------------

/// Write a hiword-mask register: `bits` of `mask` are written, other bits untouched.
fn hiword(address: usize, mask: u16, bits: u16) {
    // SAFETY: all callers pass CRU/GRF/PMU/GPIO register addresses inside the
    // identity-mapped device GiB 3 (mmu.rs DEVICE_L1); volatile dword writes there are
    // sound, and the hiword mask makes the write field-precise.
    unsafe { mmio::write_u32(address, (u32::from(mask) << 16) | u32::from(bits)) };
}

/// Ungate clocks: CRU gate bits are 1 = gated, write 0s under the mask (clk-rk3588.c).
fn ungate(cru_offset: usize, mask: u16) {
    hiword(CRU + cru_offset, mask, 0);
}

fn reg_read(address: usize) -> u32 {
    // SAFETY: callers pass addresses inside the identity-mapped device GiB.
    unsafe { mmio::read_u32(address) }
}

/// Drive one GPIO pin as an output at the given level (Rockchip v2 GPIO layout —
/// see rk3588_pcie.rs `gpio_drive`, the proven NIC-rail path).
fn gpio_drive(bank: usize, pin: u32, high: bool) {
    let (half, bit) = if pin < 16 { (0, pin) } else { (4, pin - 16) };
    let mask = 1u16 << bit;
    // Level first, then direction, so the pad never glitches through the wrong level.
    hiword(bank + half, mask, if high { mask } else { 0 });
    hiword(bank + 0x8 + half, mask, mask); // DDR: 1 = output
}

// -----------------------------------------------------------------------------------------
// Bring-up
// -----------------------------------------------------------------------------------------

/// Make the four USB2 host register blocks reachable: PD_USB, clock gates, both
/// u2phy host ports, the shared VBUS rail — then one read-only identity peek per
/// controller. Called once from `kmain` after the watchdog is armed (a wedged bus
/// access on an unpowered block resets to U-Boot instead of hanging the bench;
/// every step prints *before* its first touch of a new block).
pub(crate) fn init() {
    // --- power domain ----------------------------------------------------------------
    // Print state BEFORE touching anything (plan §3 M1 / risk 4: PD_USB gated under
    // the serial-loader `go` flow is a live hypothesis only this print settles).
    let pd = reg_read(PD_USB_PWR_REG);
    kprintln!(
        "usb: PD_USB {} (pwr {:#010x}, bit 11)",
        if pd & PD_USB_BIT == 0 {
            "on"
        } else {
            "GATED OFF"
        },
        pd
    );
    if pd & PD_USB_BIT != 0 {
        // Power on the way mainline rockchip_pd_power(true) does: ungate power, then
        // release the bus-idle request and wait (bounded) for the idle flag to drop.
        kprintln!("usb: powering PD_USB on + releasing the bus-idle request");
        hiword(PD_USB_PWR_REG, (PD_USB_BIT & 0xffff) as u16, 0);
        delay_us(1_000);
        hiword(PD_USB_REQ_REG, (PD_USB_REQ_BIT & 0xffff) as u16, 0);
        let mut settled = false;
        for _ in 0..1_000 {
            if reg_read(PD_USB_IDLE_REG) & PD_USB_IDLE_BIT == 0 {
                settled = true;
                break;
            }
            delay_us(10);
        }
        let (pwr, ack, idle) = (
            reg_read(PD_USB_PWR_REG),
            reg_read(PD_USB_ACK_REG),
            reg_read(PD_USB_IDLE_REG),
        );
        kprintln!(
            "usb: PD_USB after power-on: pwr {pwr:#010x} ack {ack:#010x} idle {idle:#010x}{}",
            if settled {
                ""
            } else {
                " (idle NEVER dropped — skipping USB bring-up)"
            }
        );
        if !settled {
            return;
        }
    }

    // --- clocks -------------------------------------------------------------------------
    // The CLK_IS_CRITICAL roots first (the GATE_LINK dependencies), then the usb
    // block's roots + linked gates, then the per-controller hclks; finally the u2phy
    // ref (PMU1-CRU). All idempotent ungates; muxes/divs are left where firmware put
    // them (the hclk roots come out of reset at workable rates and U-Boot's USB used
    // them as-is).
    kprintln!("usb: ungating clocks (GATE_CON74 0/2, GATE_CON42 0-3 + 10-13, PMU GATE_CON4 7)");
    ungate(0x928, (1 << 0) | (1 << 2)); // ACLK/HCLK_VO1USB_TOP_ROOT
    ungate(
        0x8a8,
        (1 << 0) | (1 << 1) | (1 << 2) | (1 << 3)      // ACLK/HCLK_USB_ROOT + ACLK/HCLK_USB
            | (1 << 10) | (1 << 11) | (1 << 12) | (1 << 13), // HCLK_HOST0/ARB0/HOST1/ARB1
    );
    ungate(0x3_0810, 1 << 7); // CLK_USB2PHY_HDPTXRXPHY_REF (PMU GATE_CON4)

    // --- VBUS rail ------------------------------------------------------------------------
    // vcc5v0_usb20 feeds both USB2-A ports; GPIO3_B7 (pin 15), enable-active-HIGH
    // (rk3588-orangepi-5-plus.dts). GPIO3's pclk first so the write takes (GATE_CON17
    // g2 — the same gate the NIC rail opens). 20 ms settle: the DT declares no
    // startup delay; a fixed 5 V switch is comfortably up in that.
    ungate(0x844, 1 << 2);
    gpio_drive(GPIO3, 15, true);
    kprintln!("usb: VBUS rail enabled (vcc5v0_usb20, GPIO3_B7 high), settling 20 ms");
    delay_us(20_000);

    // --- the two host phys ------------------------------------------------------------------
    phy_init(&U2PHY2);
    phy_init(&U2PHY3);

    // --- identity peeks ------------------------------------------------------------------------
    // Read-only, before any driver claim: HcRevision (OHCI 1.0a §7.1.1, expect
    // 0x...10) and the EHCI HCIVERSION/CAPLENGTH dword (EHCI 1.0 §2.2, expect
    // 0x0100xxxx). THE power/clock discriminator: garbage or a hang here means this
    // module's work didn't land; clean values move the suspect downstream.
    for (name, base) in [
        ("usb-host0-ohci", USB_HOST0_OHCI),
        ("usb-host1-ohci", USB_HOST1_OHCI),
    ] {
        kprintln!("usb: peeking {name} HcRevision @ {base:#x}");
        let revision = reg_read(base);
        kprintln!(
            "usb: {name} HcRevision {revision:#010x}{}",
            if revision & 0xf0 == 0x10 {
                " (OHCI 1.x)"
            } else {
                " (UNEXPECTED)"
            }
        );
    }
    for (name, base) in [
        ("usb-host0-ehci", USB_HOST0_EHCI),
        ("usb-host1-ehci", USB_HOST1_EHCI),
    ] {
        kprintln!("usb: peeking {name} HCCAPBASE @ {base:#x}");
        let capbase = reg_read(base);
        kprintln!(
            "usb: {name} HCCAPBASE {capbase:#010x} (hciversion {:#06x}, caplength {:#04x})",
            (capbase >> 16) & 0xffff,
            capbase & 0xff
        );
    }
}

/// One phy's host-port bring-up: APB reset off, the cited rk3588_usb2phy_tuning
/// sequence (SIDDQ → reset pulse → suspend cfg → HS tuning), clkout on, port
/// un-suspended, line state printed.
fn phy_init(phy: &U2Phy) {
    kprintln!("usb[{}]: phy init (grf {:#x})", phy.name, phy.grf);

    // APB reset deasserted before the first GRF write (SOFTRST_CON72; 1 = in reset).
    hiword(CRU + 0xb20, phy.apb_reset_bit, 0);

    // The pre-touch state, for warm/cold attribution in transcripts.
    let (con0, con2, con3) = (
        reg_read(phy.grf),
        reg_read(phy.grf + 0x8),
        reg_read(phy.grf + 0xc),
    );
    kprintln!(
        "usb[{}]: grf before: +0x00 {con0:#06x} +0x08 {con2:#06x} +0x0c {con3:#06x}",
        phy.name
    );

    // rk3588_usb2phy_tuning, host arm — every write cited in the module table:
    // 1. SIDDQ off (GRF+0x08 bit 13 = 0): power the analog block.
    hiword(phy.grf + 0x8, 1 << 13, 0);
    // 2. Reset pulse after IDDQ exit (PMU1-CRU SOFTRST_CON4; rockchip_usb2phy_reset
    //    timing: assert, 10 µs, deassert, then 100-200 µs settle).
    hiword(CRU + 0x3_0a10, phy.phy_reset_bit, phy.phy_reset_bit);
    delay_us(10);
    hiword(CRU + 0x3_0a10, phy.phy_reset_bit, 0);
    delay_us(200);
    // 3. Suspend configuration (GRF+0x0c [4:0] = 0x14): FS terminations on, FS
    //    transceiver, normal opmode.
    hiword(phy.grf + 0xc, 0x1f, 0x14);
    // 4. HS DC voltage +5.89% (GRF+0x04 [11:8] = 9).
    hiword(phy.grf + 0x4, 0xf << 8, 0x9 << 8);
    // 5. HS pre-emphasis 2x (GRF+0x08 [4:3] = 0b10).
    hiword(phy.grf + 0x8, 0x3 << 3, 0x2 << 3);
    // 6. 480 MHz clkout on (GRF+0x00 bit 0 = 0 — clkout_ctl's enable value).
    hiword(phy.grf, 1 << 0, 0);
    // 7. Host port out of suspend (GRF+0x08 bit 2 = 0; phy_sus 1 = suspended).
    hiword(phy.grf + 0x8, 1 << 2, 0);
    delay_us(2_000);

    // Line state (GRF+0xc0 [10:9]): 00 = SE0 (nothing/powered-down), 01 = J (FS idle
    // — a full-speed device or an open powered port), 10 = K (LS idle). With VBUS on
    // and a device plugged this is the cheapest is-the-phy-alive read there is.
    let status = reg_read(phy.grf + 0xc0);
    kprintln!(
        "usb[{}]: grf after: +0x00 {:#06x} +0x08 {:#06x} +0x0c {:#06x}, utmi_ls {:#04b}",
        phy.name,
        reg_read(phy.grf),
        reg_read(phy.grf + 0x8),
        reg_read(phy.grf + 0xc),
        (status >> 9) & 0x3
    );
}
