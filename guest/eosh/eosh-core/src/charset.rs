//! A 128-bit ASCII character set — the admissibility primitive.
//!
//! One `u128`, one bit per ASCII byte. Union, count, membership, and "is exactly one
//! character admissible" (`one`, the forced-prefix TAB walk's question) are single
//! integer operations; range iteration ([`Charset::ranges`]) and the display-friendly
//! split at digit/letter class boundaries ([`Charset::nice_ranges`]) serve the editor's
//! "what could come next" hint line.
//!
//! Portions derived from wyager/audio2 code/repl (relicensed by the author for this
//! repository, 2026-06-08): this file is that crate's `charset.rs` minus defmt, with
//! the unstable `core::ascii::Char` surface replaced by guarded `u8` (the bytes are
//! ASCII by construction — `add`/`singleton` assert it).

use core::fmt::Write;
use core::ops::Range;

/// Iterates the maximal contiguous bit-ranges of a charset, low to high.
#[derive(Clone, PartialEq, Eq)]
pub struct CharsetRangeIterator {
    bits: u128,
}

impl Iterator for CharsetRangeIterator {
    type Item = Range<u8>;

    fn next(&mut self) -> Option<Self::Item> {
        let zeros = self.bits.trailing_zeros();
        assert!(zeros <= 128);
        let mut end = zeros;
        loop {
            if end == 128 {
                break;
            } else if self.bits & (1 << end) != 0 {
                self.bits &= !(1 << end);
                end += 1
            } else {
                break;
            }
        }
        if end == zeros {
            None
        } else {
            Some(zeros as u8..end as u8)
        }
    }
}

/// Splits one contiguous range at the digit / uppercase / lowercase class boundaries,
/// so `nice_ranges` never renders a span like `5-C` that crosses character classes.
#[derive(Debug, Clone, PartialEq, Eq)]
struct SplitRange {
    range: Range<u8>,
}

impl SplitRange {
    fn one(&mut self) -> Range<u8> {
        let out = self.range.start..self.range.start + 1;
        self.range.start += 1;
        out
    }
    fn upto(&mut self, exclusive: u8) -> Range<u8> {
        let highest = self.range.end.min(exclusive);
        let out = self.range.start..highest;
        self.range.start = highest;
        out
    }
}

impl Iterator for SplitRange {
    type Item = Range<u8>;

    fn next(&mut self) -> Option<Self::Item> {
        let start = self.range.start;
        let end = self.range.end;
        if start == end {
            None
        } else if start < b'0' {
            Some(self.one())
        } else if start <= b'9' {
            Some(self.upto(b'9' + 1))
        } else if start < b'A' {
            Some(self.one())
        } else if start <= b'Z' {
            Some(self.upto(b'Z' + 1))
        } else if start < b'a' {
            Some(self.one())
        } else if start <= b'z' {
            Some(self.upto(b'z' + 1))
        } else if start <= 0x7F {
            Some(self.one())
        } else {
            None
        }
    }
}

/// The set itself: bit `b` set means ASCII byte `b` is in the set.
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct Charset {
    bits: u128,
}

impl core::fmt::Debug for Charset {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        for range in self.nice_ranges() {
            let size = range.end - range.start;
            if size < 4 {
                for byte in range {
                    write!(f, "{}", printable(byte))?;
                }
            } else {
                write!(f, "{}-{}", printable(range.start), printable(range.end - 1))?;
            }
        }
        Ok(())
    }
}

/// Render one ASCII byte for the debug form (controls as `\xNN`).
fn printable(byte: u8) -> impl core::fmt::Display {
    struct P(u8);
    impl core::fmt::Display for P {
        fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
            if (0x21..=0x7e).contains(&self.0) {
                f.write_char(self.0 as char)
            } else {
                write!(f, "\\x{:02x}", self.0)
            }
        }
    }
    P(byte)
}

impl Charset {
    pub const fn empty() -> Self {
        Charset { bits: 0 }
    }
    /// Every ASCII byte, 0x00..=0x7F.
    pub const fn all() -> Self {
        Charset { bits: !0 }
    }
    pub const fn singleton(i: u8) -> Self {
        assert!(i < 0x80);
        Charset { bits: 1 << i }
    }
    pub const fn add(&mut self, i: u8) {
        assert!(i < 0x80);
        self.bits |= 1 << i;
    }
    pub const fn remove(&mut self, i: u8) {
        assert!(i < 0x80);
        self.bits &= !(1 << i);
    }
    pub fn ranges(&self) -> CharsetRangeIterator {
        CharsetRangeIterator { bits: self.bits }
    }
    pub fn bytes(&self) -> impl Iterator<Item = u8> {
        self.ranges().flatten()
    }
    /// Ranges split at digit/letter class boundaries, for display.
    pub fn nice_ranges(&self) -> impl Iterator<Item = Range<u8>> {
        self.ranges().flat_map(|r| SplitRange { range: r })
    }
    pub const fn contains(&self, i: u8) -> bool {
        i < 0x80 && self.bits & (1 << i) != 0
    }
    pub const fn union(&self, other: &Charset) -> Charset {
        Charset {
            bits: self.bits | other.bits,
        }
    }
    pub const fn count(&self) -> usize {
        self.bits.count_ones() as usize
    }

    pub const fn is_empty(&self) -> bool {
        self.bits == 0
    }

    /// Only returns the byte if it is the single available option — the forced-prefix
    /// TAB walk's step question.
    pub fn one(&self) -> Option<u8> {
        let mut bytes = self.bytes();
        if let Some(byte) = bytes.next()
            && bytes.next().is_none()
        {
            Some(byte)
        } else {
            None
        }
    }
}

#[cfg(test)]
mod test {
    use super::{Charset, CharsetRangeIterator};
    use alloc::vec::Vec;
    use core::ops::Range;

    #[test]
    fn test_cri() {
        let v: Vec<Range<u8>> = CharsetRangeIterator {
            bits: 0b11110001101011101,
        }
        .collect();
        assert_eq!(&v, &[0..1, 2..5, 6..7, 8..10, 13..17]);

        let v: Vec<Range<u8>> = CharsetRangeIterator {
            bits: 0b111100011010111010,
        }
        .collect();
        assert_eq!(&v, &[1..2, 3..6, 7..8, 9..11, 14..18]);

        let v: Vec<Range<u8>> = CharsetRangeIterator { bits: 0 }.collect();
        assert_eq!(&v, &[]);

        let v: Vec<Range<u8>> = CharsetRangeIterator { bits: !0 }.collect();
        assert_eq!(v, alloc::vec![0..128]);
    }

    #[test]
    fn split_nice() {
        let mut cs = Charset::empty();
        b"!#$%*+2345789:;ABCDEGIJK[rstuyz{|"
            .iter()
            .for_each(|b| cs.add(*b));
        let v: Vec<_> = cs.nice_ranges().collect();
        assert_eq!(
            &v,
            &[
                b'!'..b'"',
                b'#'..b'$',
                b'$'..b'%',
                b'%'..b'&',
                b'*'..b'+',
                b'+'..b',',
                b'2'..b'6',
                b'7'..b':',
                b':'..b';',
                b';'..b'<',
                b'A'..b'F',
                b'G'..b'H',
                b'I'..b'L',
                b'['..b'\\',
                b'r'..b'v',
                b'y'..b'{',
                b'{'..b'|',
                b'|'..b'}'
            ]
        );
    }

    #[test]
    fn contains() {
        let mut cs = Charset::empty();
        assert!(!cs.contains(0));
        assert!(!cs.contains(b'A'));
        assert!(cs.bytes().next().is_none());

        cs.add(0);
        assert!(cs.contains(0));
        assert!(!cs.contains(b'A'));
        assert_eq!(cs.bytes().next(), Some(0));
    }

    /// The carried round-trip property test: decomposing any charset into bytes and
    /// re-adding them reproduces it bit for bit. The source used quickcheck; this crate
    /// is dependency-free, so a seeded xorshift drives the same property.
    #[test]
    fn charset_roundtrip() {
        let mut state: u128 = 0x243F_6A88_85A3_08D3_1319_8A2E_0370_7344;
        for _ in 0..2000 {
            // xorshift over the full 128 bits, two 64-bit lanes.
            let lane = |s: &mut u128, shift: u32| {
                let mut x = (*s >> shift) as u64;
                x ^= x << 13;
                x ^= x >> 7;
                x ^= x << 17;
                *s = (*s & !(0xFFFF_FFFF_FFFF_FFFFu128 << shift)) | ((x as u128) << shift);
                x
            };
            let v = ((lane(&mut state, 64) as u128) << 64) | lane(&mut state, 0) as u128;
            let cs = Charset { bits: v };
            let mut cs2 = Charset::empty();
            for item in cs.bytes() {
                cs2.add(item);
            }
            for bit in 0..128 {
                let b1 = cs.bits & (1 << bit);
                let b2 = cs2.bits & (1 << bit);
                if b1 != b2 {
                    panic!("Mismatch at bit {}", bit);
                }
            }
            assert_eq!(cs2, cs);
        }
    }
}
