//! HID boot-protocol report decode (HID 1.11 appendix B; key usages from the HID
//! Usage Tables 1.12 §10 keyboard/keypad page).

/// A decoded boot-protocol keyboard report (HID 1.11 §B.1: 8 bytes — modifiers,
/// reserved, six keycodes).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub struct KeyboardReport {
    pub modifiers: u8,
    pub keys: [u8; 6],
}

/// Modifier bits (HID 1.11 §8.3).
pub mod modifier {
    pub const LEFT_CTRL: u8 = 1 << 0;
    pub const LEFT_SHIFT: u8 = 1 << 1;
    pub const LEFT_ALT: u8 = 1 << 2;
    pub const LEFT_GUI: u8 = 1 << 3;
    pub const RIGHT_CTRL: u8 = 1 << 4;
    pub const RIGHT_SHIFT: u8 = 1 << 5;
    pub const RIGHT_ALT: u8 = 1 << 6;
    pub const RIGHT_GUI: u8 = 1 << 7;
}

impl KeyboardReport {
    /// Decode an 8-byte boot keyboard report. Shorter reports are rejected; longer
    /// ones (some keyboards pad) use the first 8 bytes.
    pub fn parse(report: &[u8]) -> Option<KeyboardReport> {
        if report.len() < 8 {
            return None;
        }
        Some(KeyboardReport {
            modifiers: report[0],
            keys: [
                report[2], report[3], report[4], report[5], report[6], report[7],
            ],
        })
    }

    /// The phantom state: every key slot 0x01 means too many keys were pressed
    /// (HID 1.11 §B.1).
    pub fn is_rollover_error(&self) -> bool {
        self.keys.iter().all(|&key| key == 0x01)
    }

    pub fn shift(&self) -> bool {
        self.modifiers & (modifier::LEFT_SHIFT | modifier::RIGHT_SHIFT) != 0
    }

    /// Keys newly pressed relative to `previous` (the per-report press detector:
    /// boot reports carry state, not events).
    pub fn pressed_since(&self, previous: &KeyboardReport) -> impl Iterator<Item = u8> + '_ {
        let previous_keys = previous.keys;
        self.keys
            .into_iter()
            .filter(move |&key| key != 0 && key != 0x01 && !previous_keys.contains(&key))
    }
}

/// Map a keyboard usage (HUT 1.12 §10) to its ASCII character, if it has one.
/// `shift` applies the US layout's shifted symbols. Non-printing keys answer `None`
/// (use [`key_name`] for diagnostics).
pub fn key_ascii(usage: u8, shift: bool) -> Option<char> {
    Some(match usage {
        // 0x04..=0x1d: a..z.
        0x04..=0x1d => {
            let letter = (b'a' + (usage - 0x04)) as char;
            if shift {
                letter.to_ascii_uppercase()
            } else {
                letter
            }
        }
        // 0x1e..=0x27: 1..9, 0 with their US shifted symbols.
        0x1e..=0x27 => {
            const PLAIN: [char; 10] = ['1', '2', '3', '4', '5', '6', '7', '8', '9', '0'];
            const SHIFTED: [char; 10] = ['!', '@', '#', '$', '%', '^', '&', '*', '(', ')'];
            let index = (usage - 0x1e) as usize;
            if shift { SHIFTED[index] } else { PLAIN[index] }
        }
        0x28 => '\n',  // Enter
        0x2b => '\t',  // Tab
        0x2c => ' ',   // Space
        0x2d => if shift { '_' } else { '-' },
        0x2e => if shift { '+' } else { '=' },
        0x2f => if shift { '{' } else { '[' },
        0x30 => if shift { '}' } else { ']' },
        0x31 => if shift { '|' } else { '\\' },
        0x33 => if shift { ':' } else { ';' },
        0x34 => if shift { '"' } else { '\'' },
        0x35 => if shift { '~' } else { '`' },
        0x36 => if shift { '<' } else { ',' },
        0x37 => if shift { '>' } else { '.' },
        0x38 => if shift { '?' } else { '/' },
        _ => return None,
    })
}

/// Diagnostic name for the common non-printing keys.
pub fn key_name(usage: u8) -> Option<&'static str> {
    Some(match usage {
        0x28 => "enter",
        0x29 => "escape",
        0x2a => "backspace",
        0x2b => "tab",
        0x2c => "space",
        0x39 => "capslock",
        0x4f => "right",
        0x50 => "left",
        0x51 => "down",
        0x52 => "up",
        _ => return None,
    })
}

/// A decoded boot-protocol mouse report (HID 1.11 §B.2: buttons, dX, dY).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub struct MouseReport {
    pub buttons: u8,
    pub dx: i8,
    pub dy: i8,
}

impl MouseReport {
    pub fn parse(report: &[u8]) -> Option<MouseReport> {
        if report.len() < 3 {
            return None;
        }
        Some(MouseReport {
            buttons: report[0],
            dx: report[1] as i8,
            dy: report[2] as i8,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keyboard_reports_decode() {
        // 'a' pressed, no modifiers.
        let a = KeyboardReport::parse(&[0, 0, 0x04, 0, 0, 0, 0, 0]).unwrap();
        assert_eq!(a.keys[0], 0x04);
        assert_eq!(key_ascii(0x04, a.shift()), Some('a'));
        // Shift+2 = '@'.
        let at = KeyboardReport::parse(&[modifier::LEFT_SHIFT, 0, 0x1f, 0, 0, 0, 0, 0]).unwrap();
        assert!(at.shift());
        assert_eq!(key_ascii(0x1f, at.shift()), Some('@'));
        // Enter and escape.
        assert_eq!(key_ascii(0x28, false), Some('\n'));
        assert_eq!(key_ascii(0x29, false), None);
        assert_eq!(key_name(0x29), Some("escape"));
        // Short buffers refuse.
        assert_eq!(KeyboardReport::parse(&[0, 0, 0x04]), None);
    }

    #[test]
    fn rollover_and_press_edges() {
        let phantom = KeyboardReport::parse(&[0, 0, 1, 1, 1, 1, 1, 1]).unwrap();
        assert!(phantom.is_rollover_error());

        let none = KeyboardReport::default();
        let ab = KeyboardReport::parse(&[0, 0, 0x04, 0x05, 0, 0, 0, 0]).unwrap();
        let pressed: Vec<u8> = ab.pressed_since(&none).collect();
        assert_eq!(pressed, vec![0x04, 0x05]);
        // Held keys do not re-report; a release produces nothing.
        let still: Vec<u8> = ab.pressed_since(&ab).collect();
        assert!(still.is_empty());
        let released: Vec<u8> = none.pressed_since(&ab).collect();
        assert!(released.is_empty());
    }

    #[test]
    fn mouse_reports_decode_signed_deltas() {
        let report = MouseReport::parse(&[0b101, 0xff, 0x05]).unwrap();
        assert_eq!(report.buttons, 0b101);
        assert_eq!(report.dx, -1);
        assert_eq!(report.dy, 5);
        assert_eq!(MouseReport::parse(&[1, 2]), None);
    }
}
