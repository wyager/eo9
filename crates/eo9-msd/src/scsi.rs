//! The minimal SCSI command set (docs/board/usb-msd-plan.md §1.2): six fixed-size
//! CDB builders and the three response decoders (INQUIRY, READ CAPACITY(10), fixed
//! sense data). READ CAPACITY(10) tops out at 2 TiB with 512-byte blocks — comically
//! sufficient for a boot stick; READ(16)/READ CAPACITY(16) are recorded non-goals.

/// SCSI operation codes (SPC-3 §D.2 / SBC-2 §B.1).
pub const OP_TEST_UNIT_READY: u8 = 0x00;
pub const OP_REQUEST_SENSE: u8 = 0x03;
pub const OP_INQUIRY: u8 = 0x12;
pub const OP_READ_CAPACITY_10: u8 = 0x25;
pub const OP_READ_10: u8 = 0x28;
pub const OP_WRITE_10: u8 = 0x2a;

/// Standard INQUIRY data length the driver asks for (SPC-3 §6.4.2: the 36-byte
/// standard header carries everything we read — type, RMB, vendor, product, rev).
pub const INQUIRY_LEN: u8 = 36;
/// Fixed-format sense length (SPC-3 §4.5.3: 18 bytes reaches the ASCQ).
pub const SENSE_LEN: u8 = 18;
/// READ CAPACITY(10) answers exactly 8 bytes (SBC-2 §5.10.2).
pub const READ_CAPACITY_LEN: usize = 8;

/// INQUIRY (SPC-3 §6.4): EVPD off, page 0, `allocation` bytes back.
pub fn inquiry(allocation: u8) -> [u8; 6] {
    [OP_INQUIRY, 0, 0, 0, allocation, 0]
}

/// TEST UNIT READY (SPC-3 §6.33): six zero bytes — the opcode IS 0x00.
pub fn test_unit_ready() -> [u8; 6] {
    [OP_TEST_UNIT_READY, 0, 0, 0, 0, 0]
}

/// REQUEST SENSE (SPC-3 §6.27): fixed format (DESC off), `allocation` bytes back.
pub fn request_sense(allocation: u8) -> [u8; 6] {
    [OP_REQUEST_SENSE, 0, 0, 0, allocation, 0]
}

/// READ CAPACITY(10) (SBC-2 §5.10): no PMI, LBA 0.
pub fn read_capacity10() -> [u8; 10] {
    let mut cdb = [0u8; 10];
    cdb[0] = OP_READ_CAPACITY_10;
    cdb
}

/// READ(10) (SBC-2 §5.6): big-endian LBA and block count, no protection/cache flags.
pub fn read10(lba: u32, blocks: u16) -> [u8; 10] {
    rw10(OP_READ_10, lba, blocks)
}

/// WRITE(10) (SBC-2 §5.25): same layout as READ(10).
pub fn write10(lba: u32, blocks: u16) -> [u8; 10] {
    rw10(OP_WRITE_10, lba, blocks)
}

fn rw10(op: u8, lba: u32, blocks: u16) -> [u8; 10] {
    let mut cdb = [0u8; 10];
    cdb[0] = op;
    cdb[2..6].copy_from_slice(&lba.to_be_bytes());
    cdb[7..9].copy_from_slice(&blocks.to_be_bytes());
    cdb
}

/// Decoded standard INQUIRY data (SPC-3 §6.4.2, the head every device answers).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Inquiry {
    /// Peripheral device type (bits 4..0 of byte 0): 0x00 = direct-access block
    /// device (the stick), 0x05 = CD/DVD, 0x1f = none/unknown.
    pub device_type: u8,
    /// RMB (byte 1 bit 7): removable medium.
    pub removable: bool,
    /// T10 vendor identification, bytes 8..16 (ASCII, space-padded).
    pub vendor: [u8; 8],
    /// Product identification, bytes 16..32.
    pub product: [u8; 16],
    /// Product revision level, bytes 32..36.
    pub revision: [u8; 4],
}

impl Inquiry {
    /// Parse the 36-byte standard INQUIRY data; `None` if shorter.
    pub fn parse(bytes: &[u8]) -> Option<Inquiry> {
        if bytes.len() < INQUIRY_LEN as usize {
            return None;
        }
        Some(Inquiry {
            device_type: bytes[0] & 0x1f,
            removable: bytes[1] & 0x80 != 0,
            vendor: bytes[8..16].try_into().expect("sliced 8"),
            product: bytes[16..32].try_into().expect("sliced 16"),
            revision: bytes[32..36].try_into().expect("sliced 4"),
        })
    }

    /// The vendor field as trimmed ASCII (non-ASCII bytes read as '?').
    pub fn vendor_str(&self) -> alloc::string::String {
        trimmed_ascii(&self.vendor)
    }

    /// The product field as trimmed ASCII.
    pub fn product_str(&self) -> alloc::string::String {
        trimmed_ascii(&self.product)
    }

    /// The revision field as trimmed ASCII.
    pub fn revision_str(&self) -> alloc::string::String {
        trimmed_ascii(&self.revision)
    }
}

/// Device-supplied identification bytes as a console-safe string: bytes outside
/// printable ASCII (0x20..=0x7E) become `.`, then the SPC-3 space padding is trimmed.
/// Sanitize-at-construction (the manuals/usbcheck precedent — eo9-ohci's
/// `printable_ascii` is the sibling): a malicious device's INQUIRY strings must not
/// carry escape sequences into the console, which fbcon now interprets — and dotting
/// (rather than truncating at the first bad byte) keeps tampering VISIBLE instead of
/// hiding everything after an embedded ESC.
fn trimmed_ascii(bytes: &[u8]) -> alloc::string::String {
    let mut out = alloc::string::String::with_capacity(bytes.len());
    for &byte in bytes {
        out.push(if (0x20..=0x7e).contains(&byte) {
            byte as char
        } else {
            '.'
        });
    }
    while out.ends_with(' ') {
        out.pop();
    }
    out
}

/// Decoded READ CAPACITY(10) data (SBC-2 §5.10.2: two big-endian u32s).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Capacity {
    /// The LAST addressable LBA (capacity in blocks is this + 1).
    pub last_lba: u32,
    /// Block length in bytes (512 on every USB stick that matters).
    pub block_size: u32,
}

impl Capacity {
    /// Parse the 8-byte response; `None` if shorter.
    pub fn parse(bytes: &[u8]) -> Option<Capacity> {
        if bytes.len() < READ_CAPACITY_LEN {
            return None;
        }
        Some(Capacity {
            last_lba: u32::from_be_bytes(bytes[0..4].try_into().expect("sliced 4")),
            block_size: u32::from_be_bytes(bytes[4..8].try_into().expect("sliced 4")),
        })
    }

    /// Total capacity in bytes ((last LBA + 1) × block size; saturates at the
    /// format's 2 TiB-with-512-byte-blocks ceiling, far above any stick).
    pub fn bytes(&self) -> u64 {
        (u64::from(self.last_lba) + 1) * u64::from(self.block_size)
    }
}

/// Sense keys this driver names (SPC-3 §4.5.6 table 27).
pub mod sense_key {
    pub const NO_SENSE: u8 = 0x0;
    pub const NOT_READY: u8 = 0x2;
    pub const MEDIUM_ERROR: u8 = 0x3;
    pub const HARDWARE_ERROR: u8 = 0x4;
    pub const ILLEGAL_REQUEST: u8 = 0x5;
    pub const UNIT_ATTENTION: u8 = 0x6;
    pub const DATA_PROTECT: u8 = 0x7;
}

/// Decoded fixed-format sense data (SPC-3 §4.5.3): the key/ASC/ASCQ triple that
/// names WHY a command failed — the typed payload of the CSW-status-1 ladder rung.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Sense {
    pub key: u8,
    /// Additional sense code (byte 12).
    pub asc: u8,
    /// Additional sense code qualifier (byte 13).
    pub ascq: u8,
}

impl Sense {
    /// Parse fixed-format sense data (response codes 0x70/0x71; SPC-3 §4.5.3).
    /// `None` for descriptor-format or truncated data — the caller reports "no
    /// sense available" rather than fabricating a key.
    pub fn parse(bytes: &[u8]) -> Option<Sense> {
        // Bit 7 of byte 0 is VALID (the INFORMATION field's, not the format's);
        // the response code proper is bits 6..0.
        let response_code = bytes.first()? & 0x7f;
        if response_code != 0x70 && response_code != 0x71 {
            return None;
        }
        Some(Sense {
            key: bytes.get(2)? & 0x0f,
            asc: *bytes.get(12)?,
            ascq: *bytes.get(13)?,
        })
    }

    /// The sense key's name (SPC-3 table 27), for diagnostics.
    pub fn key_name(&self) -> &'static str {
        match self.key {
            sense_key::NO_SENSE => "no sense",
            0x1 => "recovered error",
            sense_key::NOT_READY => "not ready",
            sense_key::MEDIUM_ERROR => "medium error",
            sense_key::HARDWARE_ERROR => "hardware error",
            sense_key::ILLEGAL_REQUEST => "illegal request",
            sense_key::UNIT_ATTENTION => "unit attention",
            sense_key::DATA_PROTECT => "data protect",
            0x8 => "blank check",
            0xb => "aborted command",
            0xd => "volume overflow",
            0xe => "miscompare",
            _ => "reserved",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cdb_byte_pins() {
        assert_eq!(inquiry(36), [0x12, 0, 0, 0, 36, 0]);
        assert_eq!(test_unit_ready(), [0u8; 6]);
        assert_eq!(request_sense(18), [0x03, 0, 0, 0, 18, 0]);
        assert_eq!(read_capacity10(), [0x25, 0, 0, 0, 0, 0, 0, 0, 0, 0]);
        // LBA and count are BIG-endian (SBC-2 §5.6) — the one byte-order trap.
        assert_eq!(
            read10(0x0102_0304, 0x0506),
            [0x28, 0, 0x01, 0x02, 0x03, 0x04, 0, 0x05, 0x06, 0]
        );
        assert_eq!(
            write10(0xfffe_0000, 128),
            [0x2a, 0, 0xff, 0xfe, 0x00, 0x00, 0, 0x00, 0x80, 0]
        );
    }

    /// QEMU usb-storage's INQUIRY answer shape (vendor "QEMU", product
    /// "QEMU HARDDISK") — the fixture the check-msd gate sees live.
    #[test]
    fn inquiry_decode() {
        let mut bytes = [0u8; 36];
        bytes[0] = 0x00; // direct-access
        bytes[1] = 0x80; // removable
        bytes[8..16].copy_from_slice(b"QEMU    ");
        bytes[16..32].copy_from_slice(b"QEMU HARDDISK   ");
        bytes[32..36].copy_from_slice(b"2.5+");
        let parsed = Inquiry::parse(&bytes).unwrap();
        assert_eq!(parsed.device_type, 0);
        assert!(parsed.removable);
        assert_eq!(parsed.vendor_str(), "QEMU");
        assert_eq!(parsed.product_str(), "QEMU HARDDISK");
        assert_eq!(parsed.revision_str(), "2.5+");
        assert!(Inquiry::parse(&bytes[..35]).is_none());
    }

    #[test]
    fn inquiry_fields_with_unprintable_bytes_render_as_dots() {
        let mut bytes = [0u8; 36];
        bytes[8..16].copy_from_slice(&[b'A', 0x00, 0xff, b'B', b' ', b' ', b' ', b' ']);
        let parsed = Inquiry::parse(&bytes).unwrap();
        assert_eq!(parsed.vendor_str(), "A..B");
        // The security fixture: an SGR escape plus a BEL in the vendor field — the
        // console-injection class fbcon would interpret — renders as dots, byte-pinned.
        let mut bytes = [0u8; 36];
        bytes[8..16].copy_from_slice(b"\x1b[31mAB\x07");
        let parsed = Inquiry::parse(&bytes).unwrap();
        assert_eq!(parsed.vendor_str(), ".[31mAB.");
    }

    #[test]
    fn capacity_decode_is_big_endian() {
        // 16384 blocks of 512 = the gate's 8 MiB scratch stick: last LBA 16383.
        let bytes = [0x00, 0x00, 0x3f, 0xff, 0x00, 0x00, 0x02, 0x00];
        let capacity = Capacity::parse(&bytes).unwrap();
        assert_eq!(capacity.last_lba, 16383);
        assert_eq!(capacity.block_size, 512);
        assert_eq!(capacity.bytes(), 8 * 1024 * 1024);
        assert!(Capacity::parse(&bytes[..7]).is_none());
    }

    #[test]
    fn capacity_ceiling_is_2tib() {
        let capacity = Capacity {
            last_lba: u32::MAX,
            block_size: 512,
        };
        assert_eq!(capacity.bytes(), 2 * 1024 * 1024 * 1024 * 1024);
    }

    /// The post-reset UNIT ATTENTION every stick raises (SPC-3 §4.5.6: key 6,
    /// ASC 0x29 "power on, reset, or bus device reset occurred").
    #[test]
    fn sense_decode() {
        let mut bytes = [0u8; 18];
        bytes[0] = 0x70;
        bytes[2] = 0x06;
        bytes[12] = 0x29;
        bytes[13] = 0x00;
        let sense = Sense::parse(&bytes).unwrap();
        assert_eq!(
            sense,
            Sense {
                key: sense_key::UNIT_ATTENTION,
                asc: 0x29,
                ascq: 0
            }
        );
        assert_eq!(sense.key_name(), "unit attention");

        // VALID bit set does not change the format decision.
        bytes[0] = 0xf0;
        assert!(Sense::parse(&bytes).is_some());
        // Descriptor format (0x72) is honestly "no fixed sense".
        bytes[0] = 0x72;
        assert!(Sense::parse(&bytes).is_none());
        // Truncated before the ASCQ: refuse, never fabricate.
        assert!(Sense::parse(&[0x70, 0, 5]).is_none());
    }
}
