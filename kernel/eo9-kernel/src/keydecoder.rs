//! The console escape-sequence decoder: raw UART bytes in, semantic keystrokes out.
//!
//! Shared by `wasm::providers::ReadLine` (the kernel's own line discipline) and
//! `wasm::providers::ReadKey` (the per-keystroke editor surface). It lives at the
//! crate root, not under `wasm/`, because `mod wasm` only compiles on bare metal —
//! here the state machine stays wasmtime-free and host-testable under the
//! featureless `cargo test` pass (the fbcon/gfxfb pattern).
//!
//! The decode contract is one half of a two-sided agreement: `eo9_ohci::hid::
//! key_console_bytes` (the usb.kbd keymap) emits exactly the sequences this decoder
//! parses, so USB keyboard input is indistinguishable from typed serial input
//! downstream. Unknown CSI finals — including parameter-byte shapes like Delete's
//! `ESC [ 3 ~` — are consumed silently rather than leaking `[A`-style garbage into a
//! line; the tests below pin that property.

/// Escape-sequence parser state for [`KeyDecoder`]: arrow keys (and friends) arrive
/// over serial as `ESC [ <final>` / `ESC O <final>` sequences, with optional parameter
/// bytes (`0x30..=0x3f`) and intermediates (`0x20..=0x2f`) before the final
/// (`0x40..=0x7e`).
#[derive(Default, Clone, Copy, PartialEq)]
enum EscState {
    /// Not inside an escape sequence.
    #[default]
    Idle,
    /// Saw ESC; deciding whether a CSI/SS3 sequence follows.
    Esc,
    /// Inside `ESC [` / `ESC O`; consuming until the final byte.
    Csi,
}

/// One decoded keystroke from the console UART — the shared output of [`KeyDecoder`],
/// consumed semantically by `ReadKey` (surfaced to the guest) and `ReadLine` (acted
/// on by the kernel's own line discipline).
#[cfg_attr(test, derive(Debug))]
#[derive(Clone, Copy, PartialEq)]
pub(crate) enum KeyEvent {
    /// A non-control byte: printable ASCII, or a UTF-8 lead/continuation byte.
    Char(u8),
    Enter,
    Backspace,
    Tab,
    Up,
    Down,
    Left,
    Right,
    /// Any other control byte, raw (3 = Ctrl-C, 4 = Ctrl-D, …).
    Ctrl(u8),
}

/// Escape-sequence decoder shared by `ReadLine` and `ReadKey`: raw UART bytes in,
/// semantic [`KeyEvent`]s out. Arrows decode; unknown CSI finals are consumed silently
/// (they no longer leak `[A`-style garbage into a line); a lone ESC is dropped and the
/// byte after it decodes normally. Mirrors the usermode decoder in
/// `eo9-providers-unix::text` (mirrored, not reused — this crate is `no_std` bare
/// metal).
#[derive(Default)]
pub(crate) struct KeyDecoder {
    state: EscState,
}

impl KeyDecoder {
    /// Feed one byte; `Some(event)` when a complete keystroke decodes.
    pub(crate) fn push(&mut self, byte: u8) -> Option<KeyEvent> {
        match self.state {
            EscState::Esc => {
                if byte == b'[' || byte == b'O' {
                    self.state = EscState::Csi;
                    return None;
                }
                if byte == 0x1b {
                    // ESC ESC: stay armed for a sequence.
                    return None;
                }
                // A lone ESC: drop it, decode this byte normally.
                self.state = EscState::Idle;
                Self::plain(byte)
            }
            EscState::Csi => {
                if (0x20..=0x3f).contains(&byte) {
                    // Parameter / intermediate bytes.
                    return None;
                }
                self.state = EscState::Idle;
                match byte {
                    b'A' => Some(KeyEvent::Up),
                    b'B' => Some(KeyEvent::Down),
                    b'C' => Some(KeyEvent::Right),
                    b'D' => Some(KeyEvent::Left),
                    // Home/End/Delete and other finals: consumed, ignored (v1).
                    _ => None,
                }
            }
            EscState::Idle => {
                if byte == 0x1b {
                    self.state = EscState::Esc;
                    return None;
                }
                Self::plain(byte)
            }
        }
    }

    fn plain(byte: u8) -> Option<KeyEvent> {
        Some(match byte {
            b'\r' | b'\n' => KeyEvent::Enter,
            0x08 | 0x7f => KeyEvent::Backspace,
            b'\t' => KeyEvent::Tab,
            0x00..=0x1f => KeyEvent::Ctrl(byte),
            _ => KeyEvent::Char(byte),
        })
    }
}

// -------------------------------------------------------------------------------------
// Host tests
// -------------------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::{KeyDecoder, KeyEvent};

    /// Feed a byte string through a fresh decoder; collect the decoded events.
    fn events(bytes: &[u8]) -> Vec<KeyEvent> {
        let mut decoder = KeyDecoder::default();
        bytes
            .iter()
            .filter_map(|&byte| decoder.push(byte))
            .collect()
    }

    #[test]
    fn arrows_decode_to_their_events() {
        // The four arrows — exactly the sequences usb.kbd's key_console_bytes emits.
        assert_eq!(events(b"\x1b[A"), [KeyEvent::Up]);
        assert_eq!(events(b"\x1b[B"), [KeyEvent::Down]);
        assert_eq!(events(b"\x1b[C"), [KeyEvent::Right]);
        assert_eq!(events(b"\x1b[D"), [KeyEvent::Left]);
        // The SS3 spelling some terminals send in application mode.
        assert_eq!(events(b"\x1bOA"), [KeyEvent::Up]);
    }

    #[test]
    fn unknown_csi_finals_are_consumed_silently() {
        // Home / End (usb.kbd emits these now; the v1 editor ignores them): zero
        // events, and the byte after decodes clean — nothing leaks into the line.
        assert_eq!(events(b"\x1b[H"), []);
        assert_eq!(events(b"\x1b[Hx"), [KeyEvent::Char(b'x')]);
        assert_eq!(events(b"\x1b[F"), []);
        assert_eq!(events(b"\x1b[Fx"), [KeyEvent::Char(b'x')]);
    }

    #[test]
    fn parameter_byte_sequences_are_consumed_whole() {
        // Delete is `ESC [ 3 ~`: the parameter byte must not terminate the sequence
        // early, and the `~` final must not leak — zero events, next byte clean.
        assert_eq!(events(b"\x1b[3~"), []);
        assert_eq!(events(b"\x1b[3~x"), [KeyEvent::Char(b'x')]);
        // A modified arrow from a real terminal (`ESC [ 1 ; 2 A`, shift-up): the
        // parameters are consumed and the `A` final decodes as the plain arrow —
        // consistent with the v1 plain-sequence modifier posture.
        assert_eq!(events(b"\x1b[1;2A"), [KeyEvent::Up]);
    }

    #[test]
    fn lone_and_doubled_esc_resolve_to_the_following_byte() {
        // A lone ESC drops; the byte after it decodes normally.
        assert_eq!(events(b"\x1bx"), [KeyEvent::Char(b'x')]);
        // ESC ESC stays armed: the sequence that follows still decodes.
        assert_eq!(events(b"\x1b\x1b[A"), [KeyEvent::Up]);
    }

    #[test]
    fn sequences_split_across_pushes_hold_state() {
        // The decoder is fed byte-at-a-time from the UART ring; a sequence split
        // across polls must hold state until its final byte arrives.
        let mut decoder = KeyDecoder::default();
        assert_eq!(decoder.push(0x1b), None);
        assert_eq!(decoder.push(b'['), None);
        assert_eq!(decoder.push(b'3'), None);
        assert_eq!(decoder.push(b'~'), None);
        assert_eq!(decoder.push(b'q'), Some(KeyEvent::Char(b'q')));
    }

    #[test]
    fn plain_bytes_decode_unchanged() {
        assert_eq!(events(b"\r"), [KeyEvent::Enter]);
        assert_eq!(events(b"\x7f"), [KeyEvent::Backspace]);
        assert_eq!(events(b"\t"), [KeyEvent::Tab]);
        assert_eq!(events(b"\x03"), [KeyEvent::Ctrl(0x03)]);
        assert_eq!(events(b"hi"), [KeyEvent::Char(b'h'), KeyEvent::Char(b'i')]);
    }
}
