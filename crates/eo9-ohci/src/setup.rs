//! Setup packets and the standard/HID requests the v1 drivers issue (USB 2.0 §9.3-§9.4;
//! HID 1.11 §7.2).

/// An 8-byte SETUP packet (USB 2.0 §9.3, table 9-2). Encoded little-endian.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SetupPacket {
    pub request_type: u8,
    pub request: u8,
    pub value: u16,
    pub index: u16,
    pub length: u16,
}

impl SetupPacket {
    pub fn encode(&self) -> [u8; 8] {
        let mut bytes = [0u8; 8];
        bytes[0] = self.request_type;
        bytes[1] = self.request;
        bytes[2..4].copy_from_slice(&self.value.to_le_bytes());
        bytes[4..6].copy_from_slice(&self.index.to_le_bytes());
        bytes[6..8].copy_from_slice(&self.length.to_le_bytes());
        bytes
    }

    /// Whether the data stage (if any) is device-to-host (bmRequestType bit 7).
    pub fn is_in(&self) -> bool {
        self.request_type & 0x80 != 0
    }
}

/// bmRequestType values (USB 2.0 table 9-2; HID 1.11 §7.2).
pub mod request_type {
    /// Host-to-device | standard | device.
    pub const STANDARD_OUT_DEVICE: u8 = 0x00;
    /// Device-to-host | standard | device.
    pub const STANDARD_IN_DEVICE: u8 = 0x80;
    /// Host-to-device | class | interface (the HID requests).
    pub const CLASS_OUT_INTERFACE: u8 = 0x21;
}

/// bRequest values (USB 2.0 table 9-4; HID 1.11 §7.2).
pub mod request {
    pub const GET_DESCRIPTOR: u8 = 6;
    pub const SET_ADDRESS: u8 = 5;
    pub const SET_CONFIGURATION: u8 = 9;
    /// HID class (HID 1.11 §7.2.5/§7.2.6).
    pub const HID_SET_IDLE: u8 = 0x0a;
    pub const HID_SET_PROTOCOL: u8 = 0x0b;
}

/// Descriptor type codes (USB 2.0 table 9-5; HID 1.11 §7.1).
pub mod descriptor_type {
    pub const DEVICE: u8 = 1;
    pub const CONFIGURATION: u8 = 2;
    pub const STRING: u8 = 3;
    pub const INTERFACE: u8 = 4;
    pub const ENDPOINT: u8 = 5;
    pub const HID: u8 = 0x21;
    pub const HID_REPORT: u8 = 0x22;
}

/// SET_ADDRESS (USB 2.0 §9.4.6): no data stage; the device answers on the new address
/// only after the status stage completes.
pub fn set_address(address: u8) -> SetupPacket {
    SetupPacket {
        request_type: request_type::STANDARD_OUT_DEVICE,
        request: request::SET_ADDRESS,
        value: u16::from(address),
        index: 0,
        length: 0,
    }
}

/// GET_DESCRIPTOR (USB 2.0 §9.4.3): descriptor type in the high byte of wValue, index
/// in the low byte.
pub fn get_descriptor(descriptor: u8, index: u8, length: u16) -> SetupPacket {
    SetupPacket {
        request_type: request_type::STANDARD_IN_DEVICE,
        request: request::GET_DESCRIPTOR,
        value: (u16::from(descriptor) << 8) | u16::from(index),
        index: 0,
        length,
    }
}

/// SET_CONFIGURATION (USB 2.0 §9.4.7).
pub fn set_configuration(configuration: u8) -> SetupPacket {
    SetupPacket {
        request_type: request_type::STANDARD_OUT_DEVICE,
        request: request::SET_CONFIGURATION,
        value: u16::from(configuration),
        index: 0,
        length: 0,
    }
}

/// HID SET_PROTOCOL (HID 1.11 §7.2.6): wValue 0 = boot protocol, 1 = report protocol.
pub fn hid_set_protocol_boot(interface: u8) -> SetupPacket {
    SetupPacket {
        request_type: request_type::CLASS_OUT_INTERFACE,
        request: request::HID_SET_PROTOCOL,
        value: 0,
        index: u16::from(interface),
        length: 0,
    }
}

/// HID SET_IDLE (HID 1.11 §7.2.4): duration 0 = report only on change (high byte of
/// wValue, in 4 ms units); report ID 0 = all reports.
pub fn hid_set_idle_indefinite(interface: u8) -> SetupPacket {
    SetupPacket {
        request_type: request_type::CLASS_OUT_INTERFACE,
        request: request::HID_SET_IDLE,
        value: 0,
        index: u16::from(interface),
        length: 0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn setup_packets_encode_little_endian() {
        // GET_DESCRIPTOR(device, 18 bytes): 80 06 00 01 00 00 12 00 — the most
        // recognizable 8 bytes in USB.
        assert_eq!(
            get_descriptor(descriptor_type::DEVICE, 0, 18).encode(),
            [0x80, 0x06, 0x00, 0x01, 0x00, 0x00, 0x12, 0x00]
        );
        // SET_ADDRESS(2): 00 05 02 00 00 00 00 00.
        assert_eq!(
            set_address(2).encode(),
            [0x00, 0x05, 0x02, 0x00, 0x00, 0x00, 0x00, 0x00]
        );
        // GET_DESCRIPTOR(configuration 0, 9): 80 06 00 02 00 00 09 00.
        assert_eq!(
            get_descriptor(descriptor_type::CONFIGURATION, 0, 9).encode(),
            [0x80, 0x06, 0x00, 0x02, 0x00, 0x00, 0x09, 0x00]
        );
        // SET_CONFIGURATION(1): 00 09 01 00 00 00 00 00.
        assert_eq!(
            set_configuration(1).encode(),
            [0x00, 0x09, 0x01, 0x00, 0x00, 0x00, 0x00, 0x00]
        );
        // HID SET_PROTOCOL(boot, interface 0): 21 0b 00 00 00 00 00 00.
        assert_eq!(
            hid_set_protocol_boot(0).encode(),
            [0x21, 0x0b, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00]
        );
        // HID SET_IDLE(indefinite, interface 1): 21 0a 00 00 01 00 00 00.
        assert_eq!(
            hid_set_idle_indefinite(1).encode(),
            [0x21, 0x0a, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00]
        );
    }

    #[test]
    fn direction_comes_from_bit_seven() {
        assert!(get_descriptor(descriptor_type::DEVICE, 0, 8).is_in());
        assert!(!set_address(1).is_in());
        assert!(!hid_set_protocol_boot(0).is_in());
    }
}
