//! Test-side admissibility checker: does `admissible()` agree with `step()`?
//!
//! Carried from audio2's `sanity_check_admissibility` (relicensed by the author for
//! this repository, 2026-06-08), extended to quantify the residual of `Bind`'s
//! documented admissibility approximation and to pin the Eof contract.
//!
//! The contract, per state, over all 128 ASCII inputs plus Eof:
//!
//! * a byte the charset claims admissible MUST step (over-claiming is a hard bug — the
//!   forced-prefix TAB walk would type a byte the parser then rejects);
//! * when `hard_required`, a byte outside the charset MUST NOT step (a hard state that
//!   secretly accepts more would redden viable input in displays built on the charset);
//! * `hard_required` ⇔ Eof fails (a soft state must wrap up at end of line; a hard
//!   state must not);
//! * RESIDUAL (counted, not failed): in a soft state, a byte outside the charset that
//!   steps to a *continuation* — an under-claimed consumable byte. Finish-and-reject
//!   on such bytes is the normal soft contract; continuing is `Bind`'s approximation
//!   showing (the union with the Eof-completed right side can miss bytes a
//!   differently-completed right side would take). Harmless for soundness (red comes
//!   from `step`, not the charset) and for the TAB walk (soft states never force), but
//!   worth watching — the grammar tests bound it.

use crate::inc::{IncParse, Step};
use crate::input::Input;

/// Check one state. Panics on hard violations; returns the residual count (see module
/// docs).
pub fn sanity_check<T: 'static>(parser: &dyn IncParse<T>) -> usize {
    let admissible = parser.admissible();
    let mut residual = 0usize;

    for byte in 0..0x80u8 {
        let input = Input::byte(byte).expect("ascii");
        let should_work = admissible.charset.contains(byte);
        let stepped = parser.step(input);
        let worked = stepped.is_some();
        if should_work && !worked {
            panic!(
                "Parser claimed to admit byte 0x{byte:02x} ({:?}) but didn't",
                byte as char
            );
        }
        if !should_work && admissible.hard_required && worked {
            panic!(
                "Parser claimed to not admit byte 0x{byte:02x} ({:?}) but did",
                byte as char
            );
        }
        if !should_work
            && !admissible.hard_required
            && matches!(stepped, Some(Step::Continue(_)) | Some(Step::Both { .. }))
        {
            residual += 1;
        }
    }

    // The Text contract: where `non_ascii_ok`, a text byte must CONSUME (it is a
    // generic text byte there); where not, it must never consume (failing or
    // finish-rejecting are both fine — a soft state hands it back like any other
    // non-charset input).
    let text_step = parser.step(Input::Text(0xC3));
    let text_consumes = matches!(text_step, Some(Step::Continue(_)) | Some(Step::Both { .. }));
    if admissible.non_ascii_ok && !text_consumes {
        panic!("Parser claimed non_ascii_ok but did not consume a text byte");
    }
    if !admissible.non_ascii_ok && text_consumes {
        panic!("Parser consumed a text byte without claiming non_ascii_ok");
    }

    let eof_finishes = matches!(
        parser.step(Input::Eof),
        Some(Step::Done { .. }) | Some(Step::Both { .. })
    );
    if admissible.hard_required && eof_finishes {
        panic!("Hard-required state wrapped up at Eof");
    }
    if !admissible.hard_required && !eof_finishes {
        panic!("Finishable state failed to wrap up at Eof");
    }

    residual
}
