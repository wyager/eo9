//! Protocol pieces shared by the bare-metal stub and the host-side tests.
//!
//! The wire protocol (all integers little-endian, raw bytes, no echo):
//!
//! ```text
//!   magic   4 bytes  "EO9L" (the ASCII bytes E, O, 9, L in wire order)
//!   header 24 bytes  load_addr: u64, length: u64, x0_value: u64
//!   payload          `length` bytes, stored at `load_addr`
//!   crc     4 bytes  CRC-32 (IEEE, reflected, init 0xFFFF_FFFF, final xor) of payload
//! ```
//!
//! The stub answers on the same line: `k` after every full 64 KiB of payload received
//! (progress, not flow control — the per-byte service loop is orders of magnitude faster
//! than the 1.5 Mbaud line, so the 32-byte RX FIFO cannot overflow), then `K` (CRC ok →
//! it jumps to `load_addr` with x0 = `x0_value`, or the entry x0 when `x0_value` is 0)
//! or `E` (CRC mismatch → back to waiting for a fresh magic). A stall longer than ~3 s
//! mid-transfer answers `T` and resets to idle.

#![cfg_attr(target_os = "none", no_std)]

/// Wire magic, in receive order.
pub const MAGIC: [u8; 4] = *b"EO9L";

/// Bytes of header following the magic: load_addr, length, x0_value.
pub const HEADER_LEN: usize = 24;

/// A `k` progress byte is emitted after every this-many payload bytes.
pub const ACK_INTERVAL: u64 = 64 * 1024;

/// Payload length ceiling (1 GiB) — anything larger is a corrupt header.
pub const MAX_LENGTH: u64 = 0x4000_0000;

/// The stub's own home; payloads overlapping [base, base+0x10000) are refused.
pub const STUB_BASE: u64 = 0x0400_0000;
pub const STUB_GUARD_LEN: u64 = 0x1_0000;

/// One step of CRC-32 (IEEE 802.3, reflected polynomial 0xEDB88320).
#[inline]
pub fn crc32_update(crc: u32, byte: u8) -> u32 {
    let mut c = crc ^ u32::from(byte);
    let mut i = 0;
    while i < 8 {
        c = if c & 1 != 0 {
            (c >> 1) ^ 0xEDB8_8320
        } else {
            c >> 1
        };
        i += 1;
    }
    c
}

/// CRC-32 of a whole buffer (init 0xFFFF_FFFF, final xor) — matches Python's
/// `binascii.crc32` and U-Boot's `crc32` command.
pub fn crc32(bytes: &[u8]) -> u32 {
    let mut c = 0xFFFF_FFFFu32;
    for &b in bytes {
        c = crc32_update(c, b);
    }
    !c
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn crc32_check_vector() {
        // The CRC-32 check vector: "123456789" -> 0xCBF43926.
        assert_eq!(crc32(b"123456789"), 0xCBF4_3926);
    }

    #[test]
    fn crc32_empty_and_incremental() {
        assert_eq!(crc32(b""), 0);
        let mut c = 0xFFFF_FFFFu32;
        for &b in b"EO9L" {
            c = crc32_update(c, b);
        }
        assert_eq!(!c, crc32(b"EO9L"));
    }

    #[test]
    fn header_layout() {
        // magic + (load_addr, length, x0_value) — must agree with tools/send_image.py.
        assert_eq!(MAGIC.len() + HEADER_LEN, 28);
    }
}
