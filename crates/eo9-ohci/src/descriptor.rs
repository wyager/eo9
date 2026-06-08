//! USB standard-descriptor parsing (USB 2.0 chapter 9), slice-based and allocation-free:
//! the consumer hands in the raw GET_DESCRIPTOR bytes and walks typed views.

/// The 18-byte device descriptor (USB 2.0 §9.6.1, table 9-8).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DeviceDescriptor {
    pub usb_version: u16,
    pub class: u8,
    pub subclass: u8,
    pub protocol: u8,
    pub max_packet_size_ep0: u8,
    pub vendor_id: u16,
    pub product_id: u16,
    pub device_version: u16,
    pub manufacturer_index: u8,
    pub product_index: u8,
    pub serial_index: u8,
    pub num_configurations: u8,
}

impl DeviceDescriptor {
    /// Parse from the full 18 bytes. `None` if the buffer is short or mis-typed.
    pub fn parse(bytes: &[u8]) -> Option<DeviceDescriptor> {
        if bytes.len() < 18 || bytes[0] < 18 || bytes[1] != crate::setup::descriptor_type::DEVICE {
            return None;
        }
        Some(DeviceDescriptor {
            usb_version: u16::from_le_bytes([bytes[2], bytes[3]]),
            class: bytes[4],
            subclass: bytes[5],
            protocol: bytes[6],
            max_packet_size_ep0: bytes[7],
            vendor_id: u16::from_le_bytes([bytes[8], bytes[9]]),
            product_id: u16::from_le_bytes([bytes[10], bytes[11]]),
            device_version: u16::from_le_bytes([bytes[12], bytes[13]]),
            manufacturer_index: bytes[14],
            product_index: bytes[15],
            serial_index: bytes[16],
            num_configurations: bytes[17],
        })
    }

    /// bMaxPacketSize0 from just the descriptor head — the first 8 bytes are all an
    /// enumerator may read before it knows the endpoint-0 packet size (USB 2.0
    /// §5.5.3 / §9.6.1; the classic chicken-and-egg the 8-byte first read solves).
    pub fn max_packet_size_from_head(head: &[u8]) -> Option<u8> {
        if head.len() < 8 || head[1] != crate::setup::descriptor_type::DEVICE {
            return None;
        }
        Some(head[7])
    }
}

/// The 9-byte configuration descriptor header (USB 2.0 §9.6.3, table 9-10).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ConfigurationDescriptor {
    /// wTotalLength: the configuration plus all interface/endpoint/class descriptors —
    /// what the second GET_DESCRIPTOR(configuration) read asks for.
    pub total_length: u16,
    pub num_interfaces: u8,
    pub configuration_value: u8,
    pub attributes: u8,
    /// bMaxPower in 2 mA units.
    pub max_power: u8,
}

impl ConfigurationDescriptor {
    pub fn parse(bytes: &[u8]) -> Option<ConfigurationDescriptor> {
        if bytes.len() < 9
            || bytes[0] < 9
            || bytes[1] != crate::setup::descriptor_type::CONFIGURATION
        {
            return None;
        }
        Some(ConfigurationDescriptor {
            total_length: u16::from_le_bytes([bytes[2], bytes[3]]),
            num_interfaces: bytes[4],
            configuration_value: bytes[5],
            attributes: bytes[7],
            max_power: bytes[8],
        })
    }
}

/// The 9-byte interface descriptor (USB 2.0 §9.6.5, table 9-12).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct InterfaceDescriptor {
    pub interface_number: u8,
    pub alternate_setting: u8,
    pub num_endpoints: u8,
    pub class: u8,
    pub subclass: u8,
    pub protocol: u8,
}

/// The 7-byte endpoint descriptor (USB 2.0 §9.6.6, table 9-13).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct EndpointDescriptor {
    /// bEndpointAddress: number in [3:0], direction in bit 7 (1 = IN).
    pub address: u8,
    /// bmAttributes: transfer type in [1:0] (0 control, 1 iso, 2 bulk, 3 interrupt).
    pub attributes: u8,
    pub max_packet_size: u16,
    /// bInterval in frames (for full/low-speed interrupt endpoints: the polling period
    /// in milliseconds).
    pub interval: u8,
}

impl EndpointDescriptor {
    pub fn number(&self) -> u8 {
        self.address & 0xf
    }

    pub fn is_in(&self) -> bool {
        self.address & 0x80 != 0
    }

    pub fn is_interrupt(&self) -> bool {
        self.attributes & 0b11 == 3
    }
}

/// One descriptor in a configuration blob.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Descriptor<'a> {
    Configuration(ConfigurationDescriptor),
    Interface(InterfaceDescriptor),
    Endpoint(EndpointDescriptor),
    /// Class- or vendor-specific (e.g. the HID descriptor, type 0x21): raw bytes,
    /// `bytes[1]` is the type.
    Other(&'a [u8]),
}

/// Iterate the descriptors inside a full configuration read (the wTotalLength blob):
/// each is length-prefixed (USB 2.0 §9.5). A zero or overrunning bLength ends the
/// walk — truncated tails are skipped, never panicked over.
pub fn descriptors(blob: &[u8]) -> DescriptorIter<'_> {
    DescriptorIter { rest: blob }
}

pub struct DescriptorIter<'a> {
    rest: &'a [u8],
}

impl<'a> Iterator for DescriptorIter<'a> {
    type Item = Descriptor<'a>;

    fn next(&mut self) -> Option<Descriptor<'a>> {
        loop {
            if self.rest.len() < 2 {
                return None;
            }
            let length = self.rest[0] as usize;
            if length < 2 || length > self.rest.len() {
                return None;
            }
            let (raw, rest) = self.rest.split_at(length);
            self.rest = rest;
            let parsed = match raw[1] {
                t if t == crate::setup::descriptor_type::CONFIGURATION => {
                    ConfigurationDescriptor::parse(raw).map(Descriptor::Configuration)
                }
                t if t == crate::setup::descriptor_type::INTERFACE && raw.len() >= 9 => {
                    Some(Descriptor::Interface(InterfaceDescriptor {
                        interface_number: raw[2],
                        alternate_setting: raw[3],
                        num_endpoints: raw[4],
                        class: raw[5],
                        subclass: raw[6],
                        protocol: raw[7],
                    }))
                }
                t if t == crate::setup::descriptor_type::ENDPOINT && raw.len() >= 7 => {
                    Some(Descriptor::Endpoint(EndpointDescriptor {
                        address: raw[2],
                        attributes: raw[3],
                        max_packet_size: u16::from_le_bytes([raw[4], raw[5]]),
                        interval: raw[6],
                    }))
                }
                _ => Some(Descriptor::Other(raw)),
            };
            match parsed {
                Some(descriptor) => return Some(descriptor),
                // A malformed standard descriptor: skip it, keep walking.
                None => continue,
            }
        }
    }
}

/// HID class/subclass/protocol codes (HID 1.11 §4.1-§4.3).
pub const CLASS_HID: u8 = 3;
pub const SUBCLASS_BOOT: u8 = 1;
pub const PROTOCOL_KEYBOARD: u8 = 1;
pub const PROTOCOL_MOUSE: u8 = 2;

/// A located HID boot interface and its interrupt-IN endpoint.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BootInterface {
    pub interface: InterfaceDescriptor,
    pub endpoint: EndpointDescriptor,
}

/// Find the first boot-protocol HID interface (keyboard or mouse) in a configuration
/// blob, together with its interrupt-IN endpoint (HID 1.11 appendix B: the boot
/// protocol uses exactly one interrupt-IN endpoint).
pub fn find_boot_interface(blob: &[u8]) -> Option<BootInterface> {
    let mut current: Option<InterfaceDescriptor> = None;
    for descriptor in descriptors(blob) {
        match descriptor {
            Descriptor::Interface(interface) => {
                current = (interface.class == CLASS_HID && interface.subclass == SUBCLASS_BOOT)
                    .then_some(interface);
            }
            Descriptor::Endpoint(endpoint) => {
                if let Some(interface) = current
                    && endpoint.is_in()
                    && endpoint.is_interrupt()
                {
                    return Some(BootInterface {
                        interface,
                        endpoint,
                    });
                }
            }
            _ => {}
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The QEMU usb-kbd device descriptor (hw/usb/dev-hid.c: VID 0627, PID 0001,
    /// full-speed, one configuration) — the bytes check-usb sees.
    const QEMU_KBD_DEVICE: [u8; 18] = [
        18, 1, 0x00, 0x02, 0, 0, 0, 8, 0x27, 0x06, 0x01, 0x00, 0x00, 0x00, 1, 4, 5, 1,
    ];

    /// A boot-keyboard configuration blob: configuration(9) + interface(9, HID boot
    /// keyboard) + HID(9) + interrupt-IN endpoint(7), wTotalLength 34 — the QEMU
    /// usb-kbd shape.
    const KBD_CONFIG: [u8; 34] = [
        // configuration: total 34, 1 interface, value 1, attributes 0xe0, 50 mA.
        9, 2, 34, 0, 1, 1, 0, 0xe0, 25, //
        // interface 0: HID (3), boot (1), keyboard (1), one endpoint.
        9, 4, 0, 0, 1, 3, 1, 1, 0, //
        // HID descriptor (type 0x21), report descriptor 63 bytes.
        9, 0x21, 0x11, 0x01, 0, 1, 0x22, 63, 0, //
        // endpoint 0x81: interrupt IN, MPS 8, interval 10 ms.
        7, 5, 0x81, 3, 8, 0, 10,
    ];

    #[test]
    fn device_descriptor_parses_and_heads_carry_mps0() {
        let device = DeviceDescriptor::parse(&QEMU_KBD_DEVICE).unwrap();
        assert_eq!(device.vendor_id, 0x0627);
        assert_eq!(device.product_id, 0x0001);
        assert_eq!(device.max_packet_size_ep0, 8);
        assert_eq!(device.num_configurations, 1);
        assert_eq!(
            DeviceDescriptor::max_packet_size_from_head(&QEMU_KBD_DEVICE[..8]),
            Some(8)
        );
        assert_eq!(DeviceDescriptor::parse(&QEMU_KBD_DEVICE[..17]), None);
    }

    #[test]
    fn configuration_walk_yields_each_descriptor() {
        let mut walk = descriptors(&KBD_CONFIG);
        let Some(Descriptor::Configuration(configuration)) = walk.next() else {
            panic!("expected the configuration head");
        };
        assert_eq!(configuration.total_length, 34);
        assert_eq!(configuration.configuration_value, 1);
        let Some(Descriptor::Interface(interface)) = walk.next() else {
            panic!("expected the interface");
        };
        assert_eq!(
            (interface.class, interface.subclass, interface.protocol),
            (CLASS_HID, SUBCLASS_BOOT, PROTOCOL_KEYBOARD)
        );
        let Some(Descriptor::Other(hid)) = walk.next() else {
            panic!("expected the HID class descriptor");
        };
        assert_eq!(hid[1], crate::setup::descriptor_type::HID);
        let Some(Descriptor::Endpoint(endpoint)) = walk.next() else {
            panic!("expected the endpoint");
        };
        assert!(endpoint.is_in() && endpoint.is_interrupt());
        assert_eq!(endpoint.number(), 1);
        assert_eq!(endpoint.max_packet_size, 8);
        assert_eq!(endpoint.interval, 10);
        assert_eq!(walk.next(), None);
    }

    #[test]
    fn boot_interface_finder_pairs_interface_and_endpoint() {
        let boot = find_boot_interface(&KBD_CONFIG).unwrap();
        assert_eq!(boot.interface.protocol, PROTOCOL_KEYBOARD);
        assert_eq!(boot.endpoint.address, 0x81);
        // A non-HID blob finds nothing.
        let mut other = KBD_CONFIG;
        other[14] = 0xff; // interface class -> vendor specific
        assert_eq!(find_boot_interface(&other), None);
    }

    #[test]
    fn truncated_and_corrupt_blobs_end_the_walk_cleanly() {
        // Cut mid-endpoint: the walk stops before the partial descriptor.
        assert_eq!(descriptors(&KBD_CONFIG[..30]).count(), 3);
        // A zero bLength cannot loop.
        let zeros = [0u8; 16];
        assert_eq!(descriptors(&zeros).count(), 0);
    }
}
