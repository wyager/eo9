//! Bulk-Only Transport wire formats: the 31-byte Command Block Wrapper, the
//! 13-byte Command Status Wrapper, and the class/standard control-request words
//! the recovery ladder issues. Pure encode/decode — the sequencing lives in
//! [`crate::device`].

/// CBW length (BOT §5.1: the CBW is exactly 31 bytes).
pub const CBW_LEN: usize = 31;
/// CSW length (BOT §5.2: the CSW is exactly 13 bytes).
pub const CSW_LEN: usize = 13;

/// dCBWSignature, little-endian "USBC" (BOT §5.1.1: 0x43425355).
pub const CBW_SIGNATURE: [u8; 4] = *b"USBC";
/// dCSWSignature, little-endian "USBS" (BOT §5.2.1: 0x53425355).
pub const CSW_SIGNATURE: [u8; 4] = *b"USBS";

/// bCSWStatus values (BOT §5.2.4 table 5.3).
pub const CSW_PASSED: u8 = 0;
pub const CSW_FAILED: u8 = 1;
pub const CSW_PHASE_ERROR: u8 = 2;

/// The largest command block a CBW carries (BOT §5.1.7: bCBWCBLength is 1..=16).
pub const MAX_CDB_LEN: usize = 16;

/// The control-request words the BOT layer needs, beyond bulk transfers. The
/// component issues these through `eo9:usb`'s `control-in`/`control-out`; the words
/// are pinned here so the host tests cover them.
pub mod request {
    /// Bulk-Only Mass Storage Reset (BOT §3.1): bmRequestType 0x21 (class, OUT,
    /// interface), bRequest 0xFF, wValue 0, wIndex = interface number, no data.
    pub const RESET_REQUEST_TYPE: u8 = 0x21;
    pub const RESET: u8 = 0xff;

    /// Get Max LUN (BOT §3.2): bmRequestType 0xA1 (class, IN, interface),
    /// bRequest 0xFE, wValue 0, wIndex = interface number, one data byte (the
    /// highest LUN index; a device "may STALL" instead, meaning LUN 0 only).
    pub const GET_MAX_LUN_REQUEST_TYPE: u8 = 0xa1;
    pub const GET_MAX_LUN: u8 = 0xfe;

    /// CLEAR_FEATURE(ENDPOINT_HALT) (USB 2.0 §9.4.1/§9.4.5, the BOT §5.3.4 reset
    /// recovery's per-endpoint half): bmRequestType 0x02 (standard, OUT, endpoint),
    /// bRequest 1, wValue 0 (ENDPOINT_HALT), wIndex = the endpoint address.
    pub const CLEAR_FEATURE_REQUEST_TYPE: u8 = 0x02;
    pub const CLEAR_FEATURE: u8 = 1;
    pub const FEATURE_ENDPOINT_HALT: u16 = 0;
}

/// The data-transfer direction a CBW announces (bmCBWFlags bit 7, BOT §5.1.6;
/// meaningful only when dCBWDataTransferLength is non-zero).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Direction {
    /// Device to host (bmCBWFlags 0x80).
    In,
    /// Host to device (bmCBWFlags 0x00).
    Out,
}

/// Encode one CBW (BOT §5.1): signature, tag, transfer length, flags, LUN,
/// bCBWCBLength, and the command block zero-padded to 16 bytes.
///
/// # Panics
///
/// If `cdb` is empty or longer than [`MAX_CDB_LEN`] — caller bug (the six commands
/// this crate builds are all 6 or 10 bytes), not device weather.
pub fn encode_cbw(
    tag: u32,
    transfer_length: u32,
    direction: Direction,
    lun: u8,
    cdb: &[u8],
) -> [u8; CBW_LEN] {
    assert!(
        !cdb.is_empty() && cdb.len() <= MAX_CDB_LEN,
        "CBW command block must be 1..=16 bytes"
    );
    let mut cbw = [0u8; CBW_LEN];
    cbw[0..4].copy_from_slice(&CBW_SIGNATURE);
    cbw[4..8].copy_from_slice(&tag.to_le_bytes());
    cbw[8..12].copy_from_slice(&transfer_length.to_le_bytes());
    cbw[12] = match direction {
        // Bit 7 is ignored by the device when the transfer length is zero
        // (BOT §5.1.6); encoding it unconditionally keeps the function total.
        Direction::In => 0x80,
        Direction::Out => 0x00,
    };
    cbw[13] = lun & 0x0f; // bCBWLUN is bits 3..0 (BOT §5.1.7).
    cbw[14] = cdb.len() as u8;
    cbw[15..15 + cdb.len()].copy_from_slice(cdb);
    cbw
}

/// One decoded CSW (BOT §5.2).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Csw {
    /// dCSWTag — must echo the CBW's (checked by [`decode_csw`]).
    pub tag: u32,
    /// dCSWDataResidue: how much of the announced transfer the device did NOT move.
    pub residue: u32,
    /// bCSWStatus: [`CSW_PASSED`] / [`CSW_FAILED`] / [`CSW_PHASE_ERROR`].
    pub status: u8,
}

/// Why a CSW did not decode. Any of these is "not a valid CSW" in the BOT §6.3
/// sense — the host's mandated answer is reset recovery, which [`crate::device`]
/// performs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CswError {
    /// Wrong byte count (BOT §6.3.1: a valid CSW is exactly 13 bytes).
    Length(usize),
    /// dCSWSignature is not "USBS".
    Signature,
    /// dCSWTag does not echo the CBW's tag (BOT §6.3.1).
    Tag { expected: u32, got: u32 },
    /// bCSWStatus is a reserved value (BOT §5.2.4: 3..=255 are reserved/obsolete).
    Status(u8),
}

/// Decode and validate one CSW against the tag the CBW carried.
pub fn decode_csw(bytes: &[u8], expected_tag: u32) -> Result<Csw, CswError> {
    if bytes.len() != CSW_LEN {
        return Err(CswError::Length(bytes.len()));
    }
    if bytes[0..4] != CSW_SIGNATURE {
        return Err(CswError::Signature);
    }
    let tag = u32::from_le_bytes(bytes[4..8].try_into().expect("13-byte CSW"));
    if tag != expected_tag {
        return Err(CswError::Tag {
            expected: expected_tag,
            got: tag,
        });
    }
    let status = bytes[12];
    if status > CSW_PHASE_ERROR {
        return Err(CswError::Status(status));
    }
    Ok(Csw {
        tag,
        residue: u32::from_le_bytes(bytes[8..12].try_into().expect("13-byte CSW")),
        status,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The 31 bytes of the L1 `usbcheck --bot-probe` INQUIRY CBW (tag 0xe0900001,
    /// 36 bytes IN, LUN 0, INQUIRY(36)) — with one deliberate divergence: the L1
    /// fixture put the allocation length in CDB byte 3 (the big-endian MSB —
    /// allocation 9216, tolerated by devices because they answer min(available,
    /// allocation)); SPC-3 §6.4 puts the LSB in byte 4, which is what this encoder
    /// emits and what the fixture was corrected to.
    #[test]
    fn cbw_matches_the_corrected_l1_probe_fixture() {
        let cdb = crate::scsi::inquiry(36);
        let cbw = encode_cbw(0xe090_0001, 36, Direction::In, 0, &cdb);
        let mut expected = [0u8; 31];
        expected[0..4].copy_from_slice(b"USBC");
        expected[4..8].copy_from_slice(&[0x01, 0x00, 0x90, 0xe0]);
        expected[8..12].copy_from_slice(&36u32.to_le_bytes());
        expected[12] = 0x80;
        expected[13] = 0;
        expected[14] = 6;
        expected[15] = 0x12;
        expected[19] = 36; // ALLOCATION LENGTH LSB — CDB byte 4 (SPC-3 §6.4)
        assert_eq!(cbw, expected);
    }

    #[test]
    fn cbw_out_direction_and_lun_mask() {
        let cdb = crate::scsi::write10(0, 1);
        let cbw = encode_cbw(7, 512, Direction::Out, 0x15, &cdb);
        assert_eq!(cbw[12], 0x00, "OUT clears bit 7");
        assert_eq!(cbw[13], 0x05, "bCBWLUN keeps bits 3..0 only");
        assert_eq!(cbw[14], 10);
        assert_eq!(&cbw[15..25], &cdb);
        assert_eq!(&cbw[25..31], &[0u8; 6], "the CB pad stays zero");
    }

    fn good_csw(tag: u32, residue: u32, status: u8) -> [u8; 13] {
        let mut csw = [0u8; 13];
        csw[0..4].copy_from_slice(b"USBS");
        csw[4..8].copy_from_slice(&tag.to_le_bytes());
        csw[8..12].copy_from_slice(&residue.to_le_bytes());
        csw[12] = status;
        csw
    }

    #[test]
    fn csw_roundtrip() {
        let csw = decode_csw(&good_csw(0xe090_0001, 13, CSW_FAILED), 0xe090_0001).unwrap();
        assert_eq!(
            csw,
            Csw {
                tag: 0xe090_0001,
                residue: 13,
                status: CSW_FAILED
            }
        );
    }

    #[test]
    fn csw_refusals_are_typed() {
        assert_eq!(decode_csw(&[0u8; 12], 1), Err(CswError::Length(12)));
        assert_eq!(decode_csw(&[0u8; 14], 1), Err(CswError::Length(14)));
        let mut bad = good_csw(1, 0, 0);
        bad[0] = b'X';
        assert_eq!(decode_csw(&bad, 1), Err(CswError::Signature));
        assert_eq!(
            decode_csw(&good_csw(2, 0, 0), 1),
            Err(CswError::Tag {
                expected: 1,
                got: 2
            })
        );
        assert_eq!(decode_csw(&good_csw(1, 0, 3), 1), Err(CswError::Status(3)));
    }
}
