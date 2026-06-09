//! Parser input: one ASCII byte, or the end-of-line marker.
//!
//! `Eof` forces a parser to wrap up (or fail, if it still needs input) — it is what the
//! editor feeds when the user presses Enter, and what [`crate::comb::Bind`] feeds the
//! left side of a bind to peek at the right side's admissibility.
//!
//! Bytes are guarded to 0x00..=0x7F ([`Ascii`]'s constructor is the only way in): the
//! step machinery is byte-oriented ASCII. Non-ASCII bytes never reach `step`; the
//! editor (and [`crate::inc::feed_bytes`]) consults [`crate::inc::Admissible`]'s
//! `non_ascii_ok` instead — positions that take free text (word interiors, quoted
//! strings, compound literals, comments) accept them like any other text byte.
//!
//! Portions derived from wyager/audio2 code/repl (relicensed by the author for this
//! repository, 2026-06-08).

/// An ASCII byte, 0x00..=0x7F by construction.
#[derive(Debug, Copy, Clone, Eq, PartialEq, PartialOrd, Ord)]
pub struct Ascii(u8);

impl Ascii {
    pub const fn new(byte: u8) -> Option<Ascii> {
        if byte < 0x80 { Some(Ascii(byte)) } else { None }
    }
    pub const fn get(self) -> u8 {
        self.0
    }
}

/// One unit of parser input.
#[derive(Debug, Copy, Clone, Eq, PartialEq, PartialOrd, Ord)]
pub enum Input {
    /// End of line: wrap up now or fail.
    Eof,
    Byte(Ascii),
}

impl Input {
    /// An ASCII byte input; `None` for bytes outside 0x00..=0x7F.
    pub const fn byte(byte: u8) -> Option<Input> {
        match Ascii::new(byte) {
            Some(a) => Some(Input::Byte(a)),
            None => None,
        }
    }

    pub fn is_eof(&self) -> bool {
        matches!(self, Input::Eof)
    }
}

impl TryFrom<u8> for Input {
    type Error = u8;

    fn try_from(value: u8) -> Result<Self, Self::Error> {
        Input::byte(value).ok_or(value)
    }
}

impl TryFrom<char> for Input {
    type Error = char;

    fn try_from(value: char) -> Result<Self, Self::Error> {
        if value.is_ascii() {
            Ok(Input::byte(value as u8).expect("ascii"))
        } else {
            Err(value)
        }
    }
}
