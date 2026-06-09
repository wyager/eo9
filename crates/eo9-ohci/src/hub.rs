//! Hub-class request words and descriptor/status decode (USB 2.0 chapter 11) — just
//! enough for the `--hub-peek` diagnostic: NOT hub support (no port reset, no
//! transaction translator, no change processing), only "what is behind this hub and
//! at what speed", which is the input to the hub mini-driver decision
//! (docs/board/usb-ohci-plan.md follow-ups).

use crate::setup::SetupPacket;

/// Device class code for hubs (USB 2.0 §11.23.1).
pub const CLASS_HUB: u8 = 0x09;

/// Hub descriptor type (USB 2.0 table 11-13).
pub const DESCRIPTOR_HUB: u8 = 0x29;

/// PORT_POWER feature selector (USB 2.0 table 11-17).
pub const FEATURE_PORT_POWER: u16 = 8;

/// GET_DESCRIPTOR(hub) — class request to the device (USB 2.0 §11.24.2.5:
/// bmRequestType 1010_0000b, descriptor type in the high byte of wValue).
pub fn get_hub_descriptor(length: u16) -> SetupPacket {
    SetupPacket {
        request_type: 0xa0,
        request: crate::setup::request::GET_DESCRIPTOR,
        value: u16::from(DESCRIPTOR_HUB) << 8,
        index: 0,
        length,
    }
}

/// SET_FEATURE(PORT_POWER, port) — class request to the port (USB 2.0 §11.24.2.13:
/// bmRequestType 0010_0011b; ports are 1-based).
pub fn set_port_power(port: u8) -> SetupPacket {
    SetupPacket {
        request_type: 0x23,
        request: 3, // SET_FEATURE (USB 2.0 table 9-4)
        value: FEATURE_PORT_POWER,
        index: u16::from(port),
        length: 0,
    }
}

/// GET_STATUS(port) — class request returning wPortStatus + wPortChange
/// (USB 2.0 §11.24.2.7: bmRequestType 1010_0011b, 4 bytes).
pub fn get_port_status(port: u8) -> SetupPacket {
    SetupPacket {
        request_type: 0xa3,
        request: 0, // GET_STATUS (USB 2.0 table 9-4)
        value: 0,
        index: u16::from(port),
        length: 4,
    }
}

/// The hub descriptor head (USB 2.0 §11.23.2.1, table 11-13). The variable-length
/// DeviceRemovable/PortPwrCtrlMask tail is ignored — the peek does not need it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct HubDescriptor {
    /// bNbrPorts.
    pub ports: u8,
    /// wHubCharacteristics (power switching mode in [1:0], compound bit 2, …).
    pub characteristics: u16,
    /// bPwrOn2PwrGood in 2 ms units.
    pub power_on_to_power_good_2ms: u8,
    /// bHubContrCurrent in mA.
    pub controller_current_ma: u8,
}

impl HubDescriptor {
    /// Parse from a GET_DESCRIPTOR(hub) read (at least the 7 fixed bytes).
    pub fn parse(bytes: &[u8]) -> Option<HubDescriptor> {
        if bytes.len() < 7 || bytes[0] < 7 || bytes[1] != DESCRIPTOR_HUB {
            return None;
        }
        Some(HubDescriptor {
            ports: bytes[2],
            characteristics: u16::from_le_bytes([bytes[3], bytes[4]]),
            power_on_to_power_good_2ms: bytes[5],
            controller_current_ma: bytes[6],
        })
    }
}

/// One downstream port's decoded wPortStatus (USB 2.0 table 11-21).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct HubPortStatus {
    pub connected: bool,
    pub enabled: bool,
    pub suspended: bool,
    pub powered: bool,
    pub speed: PortSpeed,
    /// wPortChange (table 11-22): connect change in bit 0, …
    pub change: u16,
    /// The raw wPortStatus, for transcripts.
    pub raw: u16,
}

/// Device speed on a hub port (wPortStatus bits 9/10: LS if bit 9, HS if bit 10,
/// else FS — USB 2.0 table 11-21).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PortSpeed {
    Low,
    Full,
    High,
}

impl HubPortStatus {
    /// Decode a 4-byte GET_STATUS(port) response (wPortStatus + wPortChange, LE).
    pub fn parse(bytes: &[u8]) -> Option<HubPortStatus> {
        if bytes.len() < 4 {
            return None;
        }
        let status = u16::from_le_bytes([bytes[0], bytes[1]]);
        let change = u16::from_le_bytes([bytes[2], bytes[3]]);
        Some(HubPortStatus {
            connected: status & (1 << 0) != 0,
            enabled: status & (1 << 1) != 0,
            suspended: status & (1 << 2) != 0,
            powered: status & (1 << 8) != 0,
            speed: if status & (1 << 9) != 0 {
                PortSpeed::Low
            } else if status & (1 << 10) != 0 {
                PortSpeed::High
            } else {
                PortSpeed::Full
            },
            change,
            raw: status,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hub_requests_encode_per_chapter_eleven() {
        // GET_DESCRIPTOR(hub, 9): a0 06 00 29 00 00 09 00.
        assert_eq!(
            get_hub_descriptor(9).encode(),
            [0xa0, 0x06, 0x00, 0x29, 0x00, 0x00, 0x09, 0x00]
        );
        // SET_FEATURE(PORT_POWER, port 2): 23 03 08 00 02 00 00 00.
        assert_eq!(
            set_port_power(2).encode(),
            [0x23, 0x03, 0x08, 0x00, 0x02, 0x00, 0x00, 0x00]
        );
        // GET_STATUS(port 1): a3 00 00 00 01 00 04 00.
        assert_eq!(
            get_port_status(1).encode(),
            [0xa3, 0x00, 0x00, 0x00, 0x01, 0x00, 0x04, 0x00]
        );
    }

    #[test]
    fn hub_descriptor_parses_the_fixed_head() {
        // A 4-port bus-powered hub: 09 29 04 e9 00 32 64 00 ff.
        let bytes = [0x09, 0x29, 0x04, 0xe9, 0x00, 0x32, 0x64, 0x00, 0xff];
        let hub = HubDescriptor::parse(&bytes).unwrap();
        assert_eq!(hub.ports, 4);
        assert_eq!(hub.characteristics, 0x00e9);
        assert_eq!(hub.power_on_to_power_good_2ms, 0x32); // 100 ms
        assert_eq!(hub.controller_current_ma, 0x64);
        // Wrong type / short refuse.
        assert_eq!(HubDescriptor::parse(&bytes[..6]), None);
        let mut wrong = bytes;
        wrong[1] = 0x21;
        assert_eq!(HubDescriptor::parse(&wrong), None);
    }

    #[test]
    fn port_status_decodes_speed_and_change_bits() {
        // Connected + enabled + powered, low-speed, connect-change pending:
        // wPortStatus 0x0303, wPortChange 0x0001.
        let low = HubPortStatus::parse(&[0x03, 0x03, 0x01, 0x00]).unwrap();
        assert!(low.connected && low.enabled && low.powered);
        assert_eq!(low.speed, PortSpeed::Low);
        assert_eq!(low.change, 1);

        // Full-speed: neither bit 9 nor 10.
        let full = HubPortStatus::parse(&[0x03, 0x01, 0x00, 0x00]).unwrap();
        assert_eq!(full.speed, PortSpeed::Full);

        // High-speed: bit 10 (a HS device on a HS hub).
        let high = HubPortStatus::parse(&[0x01, 0x05, 0x00, 0x00]).unwrap();
        assert_eq!(high.speed, PortSpeed::High);

        // Powered, empty.
        let empty = HubPortStatus::parse(&[0x00, 0x01, 0x00, 0x00]).unwrap();
        assert!(!empty.connected && empty.powered);

        assert_eq!(HubPortStatus::parse(&[0, 0, 0]), None);
    }
}
