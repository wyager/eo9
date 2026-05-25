//! Content hashes: blake3 digests that identify store objects.

use std::fmt;
use std::str::FromStr;

use crate::StoreError;

/// The blake3 content hash of a store object. An object's identity *is* its hash.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ObjectHash([u8; 32]);

impl ObjectHash {
    /// Hash `bytes` with blake3.
    pub fn of(bytes: &[u8]) -> ObjectHash {
        ObjectHash(*blake3::hash(bytes).as_bytes())
    }

    /// The raw 32-byte digest.
    pub fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }

    /// Lowercase hex encoding (64 characters); this is the on-disk object file name.
    pub fn to_hex(self) -> String {
        encode_hex(&self.0)
    }

    /// Parse a 64-character lowercase hex digest.
    pub fn from_hex(input: &str) -> Result<ObjectHash, StoreError> {
        let bytes = decode_hex(input).map_err(|reason| StoreError::InvalidHash {
            input: input.to_owned(),
            reason,
        })?;
        Ok(ObjectHash(bytes))
    }
}

impl fmt::Display for ObjectHash {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.to_hex())
    }
}

impl fmt::Debug for ObjectHash {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "ObjectHash({self})")
    }
}

impl FromStr for ObjectHash {
    type Err = StoreError;

    fn from_str(s: &str) -> Result<ObjectHash, StoreError> {
        ObjectHash::from_hex(s)
    }
}

/// Lowercase hex encoding of a 32-byte digest.
pub(crate) fn encode_hex(bytes: &[u8; 32]) -> String {
    const TABLE: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(64);
    for byte in bytes {
        out.push(TABLE[(byte >> 4) as usize] as char);
        out.push(TABLE[(byte & 0x0f) as usize] as char);
    }
    out
}

/// Decode a 64-character lowercase hex digest; the error is a human-readable reason.
pub(crate) fn decode_hex(input: &str) -> Result<[u8; 32], String> {
    if input.len() != 64 {
        return Err(format!("expected 64 hex characters, found {}", input.len()));
    }
    let mut out = [0u8; 32];
    for (i, chunk) in input.as_bytes().chunks_exact(2).enumerate() {
        let hi = hex_value(chunk[0])?;
        let lo = hex_value(chunk[1])?;
        out[i] = (hi << 4) | lo;
    }
    Ok(out)
}

fn hex_value(c: u8) -> Result<u8, String> {
    match c {
        b'0'..=b'9' => Ok(c - b'0'),
        b'a'..=b'f' => Ok(c - b'a' + 10),
        _ => Err(format!(
            "invalid character {:?} (only lowercase hex is accepted)",
            c as char
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hex_round_trip() {
        let hash = ObjectHash::of(b"hello eo9");
        let hex = hash.to_hex();
        assert_eq!(hex.len(), 64);
        assert_eq!(ObjectHash::from_hex(&hex).unwrap(), hash);
    }

    #[test]
    fn matches_blake3() {
        let hash = ObjectHash::of(b"abc");
        assert_eq!(hash.to_hex(), blake3::hash(b"abc").to_hex().to_string());
    }

    #[test]
    fn rejects_bad_digests() {
        assert!(ObjectHash::from_hex("abc").is_err());
        assert!(ObjectHash::from_hex(&"G".repeat(64)).is_err());
        assert!(
            ObjectHash::from_hex(&"A".repeat(64)).is_err(),
            "uppercase is rejected"
        );
    }
}
