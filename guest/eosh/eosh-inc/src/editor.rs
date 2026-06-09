//! The per-keystroke line editor (the study's M2, docs/study/incremental-repl-for-eosh.md).
//!
//! A pure state machine: the embedder feeds decoded [`Key`]s (from the `eo9:text`
//! `read-key` operation) and drains the bytes the editor wants echoed
//! ([`Editor::take_output`]); nothing here does I/O, so the whole behavior is
//! host-testable with key vectors against the emitted byte sequences.
//!
//! Behavior (the study's design, refined by the M2 brief):
//!
//! * **Accept-and-mark, never refuse.** Every printable character is taken into the
//!   line. While the line is viable (some parse of the [`crate::grammar`] continues
//!   through it), characters echo plainly; the first character with no viable
//!   continuation sets `red_from`, emits the marker-begin sequence once (SGR 31 by
//!   default — see [`Marker`]), and everything from there on echoes inside the marked
//!   region. The grammar's superset rule guarantees the mark is honest: red means the
//!   real parser *will* reject, green only means it still might accept.
//! * **Backspace** erases one character (`\b \b` — one column) and, when it rewinds to
//!   or below `red_from`, emits marker-end and reparses the line from the start
//!   (microseconds at the 4096-byte line cap; the study's measured trade).
//! * **TAB**: in a red line, just the bell — completion cannot help input the parser
//!   already rejects. Otherwise the forced-prefix walk first (bytes the grammar forces,
//!   e.g. `svc lo` → `g`), then `completions()`: a unique candidate completes the word
//!   plus a space; several extend to their longest common prefix; no progress prints the
//!   candidate list on its own line and repaints the prompt + line (the shape of the
//!   retired host editor, crates/eo9/src/editor.rs).
//! * **Enter** always submits — the editor never blocks a line; `parse_command`'s
//!   curated errors are the user-facing diagnosis for red lines (feeding `Eof` to decide
//!   otherwise would only duplicate that verdict). The marker is closed first so the
//!   command's own output is never tinted.
//! * **↑/↓** recall over the history snapshot the embedder passed in — a capped view
//!   (64 entries, [`RECALL_CAP`]) of the session history (whose own cap is the GAPS
//!   "eosh session history is unbounded" fix, in eosh-core). Same semantics as the
//!   kernel console's read-line recall: the in-progress line is stashed on the first ↑,
//!   restored by ↓ past the newest entry, and any edit commits the shown entry.
//!
//! UTF-8: `read-key` delivers multi-byte characters as their individual `char` bytes;
//! the editor assembles them (echoing only complete characters, so the emitted stream
//! stays valid UTF-8 for the `text.write` API) and steps the parser per byte with the
//! [`crate::inc::feed_bytes`] substitution policy, so its viability verdicts agree
//! exactly with a reparse. Invalid sequences are dropped, like the host editor did.
//!
//! Marker choice: SGR 31 (red) by default. The study calls for inverse video (SGR 7/27)
//! on the framebuffer console because red can render wrong-hued under the
//! boot-state-dependent HDMI chroma issue — but eosh has no way to know its transport
//! today (`eo9:text` deliberately says nothing about the terminal), so the default
//! stays red everywhere and fbcon — which strips CSI today — degrades silently. The
//! [`Marker::INVERSE`] constant is the ready hook for the fbcon SGR-subset follow-up.
//!
//! Known deltas vs the retired host editor (crates/eo9/src/editor.rs), all deliberate:
//! no mid-line cursor (←/→/Home/End/Delete are consumed and ignored — the red-region
//! model is end-of-line-based; v1 scope), no Ctrl-A/E/B/F/K/U/W/L editing chords, and a
//! unique completion always appends a space (the host kept `/` directories open; `/bin`
//! names have no directories). Ctrl-C (cancel the line, `^C`), Ctrl-D (end of input on
//! an empty line), history recall, TAB completion, and the candidate-list rendering
//! carry over exactly.

use alloc::string::String;
use alloc::vec::Vec;

use crate::grammar::{Vocab, command_line};
use crate::inc::{BoxP, Completion, Step, forced_prefix};
use crate::input::Input;

/// One decoded keystroke, mirroring the WIT `eo9:text/text.key` variant. The transport
/// owns the wire decoding; the editor only sees semantics.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Key {
    /// One byte of printable input (multi-byte UTF-8 arrives byte by byte).
    Char(u8),
    Enter,
    Backspace,
    Tab,
    Up,
    Down,
    Left,
    Right,
    /// A raw control byte (3 = Ctrl-C, 4 = Ctrl-D, …).
    Ctrl(u8),
    /// An explicit end-of-input keystroke (transport stream closure is the embedder's
    /// `none` answer from `read-key`; feed it as this key too).
    Eof,
}

/// What the embedder should do after a key was handled.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    /// Keep feeding keys (drain the output first).
    Pending,
    /// The line is finished: execute it (then build a fresh editor for the next prompt —
    /// the vocabulary and history snapshots are per prompt).
    Submit(String),
    /// End of input: the session is over.
    EndOfInput,
}

/// The inadmissible-region marker: the byte sequences emitted when the line turns red
/// and when it turns back. See the module docs for the fbcon story.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Marker {
    pub begin: &'static str,
    pub end: &'static str,
}

impl Marker {
    /// SGR 31 / SGR 0 — the default everywhere (serial, telnet, usermode terminal).
    pub const RED: Marker = Marker {
        begin: "\u{1b}[31m",
        end: "\u{1b}[0m",
    };
    /// SGR 7 / SGR 27 — inverse video, colorspace-proof; the fbcon follow-up's marker.
    pub const INVERSE: Marker = Marker {
        begin: "\u{1b}[7m",
        end: "\u{1b}[27m",
    };
}

/// How many history entries the recall view keeps (the newest [`RECALL_CAP`] of the
/// session history the embedder snapshots per prompt).
pub const RECALL_CAP: usize = 64;

/// Upper bound on the line, mirroring the kernel console's read-line cap: bytes beyond
/// it are dropped (not echoed) until something is erased.
pub const MAX_LINE_BYTES: usize = 4096;

/// The editor for one prompt. Build it fresh per prompt (vocabulary and history are
/// per-prompt snapshots), feed keys, drain output after every call.
pub struct Editor {
    vocab: Vocab,
    prompt: String,
    marker: Marker,
    /// The line so far — always valid UTF-8 (characters are inserted whole).
    line: Vec<u8>,
    /// Parser state covering `line[..red_from.unwrap_or(line.len())]`.
    state: BoxP<()>,
    /// First byte index with no viable parse, if the line has gone red.
    red_from: Option<usize>,
    /// Bytes of a multi-byte UTF-8 character still being assembled.
    utf8_pending: Vec<u8>,
    /// Recall view, oldest first (at most [`RECALL_CAP`] entries; extra are dropped
    /// from the old end).
    history: Vec<String>,
    /// `None` = typing a fresh line; `Some(i)` = showing `history[i]`.
    recall: Option<usize>,
    /// The fresh line stashed when recall browsing began.
    stash: Vec<u8>,
    /// Bytes to write to the terminal, drained by [`Editor::take_output`].
    out: String,
}

impl Editor {
    /// A fresh editor: `prompt` is what already sits on the screen line (used only to
    /// repaint after a candidate list); `history` is the session recall snapshot,
    /// oldest first (only the newest [`RECALL_CAP`] are kept).
    pub fn new(prompt: &str, vocab: Vocab, mut history: Vec<String>, marker: Marker) -> Editor {
        if history.len() > RECALL_CAP {
            history.drain(..history.len() - RECALL_CAP);
        }
        let state = command_line(&vocab);
        Editor {
            vocab,
            prompt: String::from(prompt),
            marker,
            line: Vec::new(),
            state,
            red_from: None,
            utf8_pending: Vec::new(),
            history,
            recall: None,
            stash: Vec::new(),
            out: String::new(),
        }
    }

    /// Drain everything the editor wants written to the terminal.
    pub fn take_output(&mut self) -> String {
        core::mem::take(&mut self.out)
    }

    /// Feed one key.
    pub fn handle(&mut self, key: Key) -> Action {
        match key {
            Key::Char(byte) => {
                self.on_char_byte(byte);
                Action::Pending
            }
            Key::Backspace => {
                self.utf8_pending.clear();
                self.on_backspace();
                Action::Pending
            }
            Key::Tab => {
                self.utf8_pending.clear();
                self.on_tab();
                Action::Pending
            }
            Key::Enter => {
                self.utf8_pending.clear();
                self.close_marker();
                self.emit("\r\n");
                Action::Submit(self.take_line())
            }
            Key::Up => {
                self.utf8_pending.clear();
                self.recall_step(true);
                Action::Pending
            }
            Key::Down => {
                self.utf8_pending.clear();
                self.recall_step(false);
                Action::Pending
            }
            // No mid-line cursor in v1 (documented delta): consumed, ignored.
            Key::Left | Key::Right => Action::Pending,
            // Ctrl-C: cancel the line. Submitting the empty string keeps the shell loop
            // unchanged (`execute_line("")` is `Command::Empty`) — the host editor's shape.
            Key::Ctrl(3) => {
                self.utf8_pending.clear();
                self.close_marker();
                self.emit("^C\r\n");
                self.line.clear();
                Action::Submit(String::new())
            }
            // Ctrl-D on an empty line: end of input. Mid-line it is ignored (the host
            // editor's delete-at-cursor has no cursor to act at here).
            Key::Ctrl(4) if self.line.is_empty() && self.utf8_pending.is_empty() => {
                self.emit("\r\n");
                Action::EndOfInput
            }
            Key::Ctrl(_) => Action::Pending,
            // Transport end of input: an empty line ends the session; a typed one is
            // handed back first (the host editor's stream-closed-mid-line behavior).
            Key::Eof => {
                self.utf8_pending.clear();
                if self.line.is_empty() {
                    self.emit("\r\n");
                    Action::EndOfInput
                } else {
                    self.close_marker();
                    self.emit("\r\n");
                    Action::Submit(self.take_line())
                }
            }
        }
    }

    // -- characters ------------------------------------------------------------------

    /// One `char` byte from the transport: ASCII inserts directly; >= 0x80 assembles
    /// into a whole UTF-8 character first (invalid sequences are dropped).
    fn on_char_byte(&mut self, byte: u8) {
        if self.utf8_pending.is_empty() {
            match byte {
                // Defensive: the decoders never deliver control bytes as `char`.
                0x00..=0x1f | 0x7f => {}
                0x20..=0x7e => self.insert_char(&[byte]),
                // UTF-8 lead byte: start assembling.
                0xc2..=0xf4 => self.utf8_pending.push(byte),
                // Stray continuation or invalid lead: drop.
                _ => {}
            }
            return;
        }
        if (0x80..=0xbf).contains(&byte) {
            self.utf8_pending.push(byte);
            let need = match self.utf8_pending[0] {
                0xc2..=0xdf => 2,
                0xe0..=0xef => 3,
                _ => 4,
            };
            if self.utf8_pending.len() == need {
                let bytes = core::mem::take(&mut self.utf8_pending);
                // Validity check (overlong/surrogate forms drop here, like the host
                // editor's read_utf8 did).
                if core::str::from_utf8(&bytes).is_ok() {
                    self.insert_char(&bytes);
                }
            }
        } else {
            // Broken sequence: drop it and process this byte fresh.
            self.utf8_pending.clear();
            self.on_char_byte(byte);
        }
    }

    /// Insert one complete character (1..=4 bytes, already validated): step the parser
    /// unless the line is red, mark red on the first inadmissible position, echo.
    fn insert_char(&mut self, bytes: &[u8]) {
        if self.line.len() + bytes.len() > MAX_LINE_BYTES {
            // The kernel console's policy at the cap: drop, no echo.
            return;
        }
        self.commit_recall();
        if self.red_from.is_none() && !self.step_state(bytes) {
            self.red_from = Some(self.line.len());
            self.emit(self.marker.begin);
        }
        self.line.extend_from_slice(bytes);
        // `bytes` is one valid UTF-8 character by construction.
        if let Ok(text) = core::str::from_utf8(bytes) {
            self.emit(text);
        }
    }

    /// Try to advance the live parser state over `bytes` (one character), applying the
    /// `feed_bytes` non-ASCII policy so the editor's verdicts agree with a reparse.
    /// Returns false (state unchanged) when no parse continues.
    fn step_state(&mut self, bytes: &[u8]) -> bool {
        let mut state = self.state.clone();
        for &byte in bytes {
            let input = match Input::byte(byte) {
                Some(input) => input,
                None => {
                    if !state.admissible().non_ascii_ok {
                        return false;
                    }
                    Input::byte(b'x').expect("ascii")
                }
            };
            state = match state.step(input).and_then(Step::cont) {
                Some(next) => next,
                None => return false,
            };
        }
        self.state = state;
        true
    }

    // -- backspace ---------------------------------------------------------------------

    fn on_backspace(&mut self) {
        if self.line.is_empty() {
            return;
        }
        self.commit_recall();
        // Pop one whole character (continuation bytes, then the lead).
        while let Some(&byte) = self.line.last() {
            self.line.pop();
            if !(0x80..=0xbf).contains(&byte) {
                break;
            }
        }
        self.emit("\u{8} \u{8}");
        match self.red_from {
            Some(red) if self.line.len() <= red => {
                // Rewound to (or below) the first red byte: the line is green again.
                self.emit(self.marker.end);
                self.red_from = None;
                self.reparse();
            }
            Some(_) => {
                // Still red beyond `red_from`: the green prefix (and its state) is
                // untouched, nothing to recompute.
            }
            None => self.reparse(),
        }
    }

    /// Recompute the parser state (and `red_from`) from the line — the study's
    /// backspace-is-reparse-from-start policy. Emits nothing.
    fn reparse(&mut self) {
        let mut state = command_line(&self.vocab);
        let mut red_from = None;
        for (index, &byte) in self.line.iter().enumerate() {
            let input = match Input::byte(byte) {
                Some(input) => input,
                None if state.admissible().non_ascii_ok => Input::byte(b'x').expect("ascii"),
                None => {
                    red_from = Some(index);
                    break;
                }
            };
            match state.step(input).and_then(Step::cont) {
                Some(next) => state = next,
                None => {
                    red_from = Some(index);
                    break;
                }
            }
        }
        self.state = state;
        self.red_from = red_from;
    }

    // -- TAB --------------------------------------------------------------------------

    fn on_tab(&mut self) {
        if self.red_from.is_some() {
            // Completion cannot help input the parser already rejects.
            self.emit("\u{7}");
            return;
        }
        self.commit_recall();
        // The forced-prefix walk: bytes the grammar forces before any choice opens up.
        let forced = forced_prefix(&*self.state);
        let mut progressed = false;
        if !forced.is_empty() {
            progressed = self.append_completion_bytes(&forced);
        }
        let mut completions: Vec<Completion> = Vec::new();
        self.state.completions(&mut completions);
        if completions.is_empty() {
            if !progressed {
                self.emit("\u{7}");
            }
            return;
        }
        let matched = completions[0].matched;
        let mut words: Vec<String> = completions.into_iter().map(|c| c.word).collect();
        words.sort();
        words.dedup();
        if words.len() == 1 {
            let rest: Vec<u8> = words[0].as_bytes()[matched..].to_vec();
            self.append_completion_bytes(&rest);
            self.append_completion_bytes(b" ");
            return;
        }
        let prefix = longest_common_prefix(&words);
        if prefix.len() > matched {
            let rest: Vec<u8> = prefix.as_bytes()[matched..].to_vec();
            if self.append_completion_bytes(&rest) {
                return;
            }
        }
        if progressed {
            return;
        }
        // No further progress: list the candidates, then repaint prompt + line (which
        // is green here — red lines bailed to the bell above).
        self.emit("\r\n");
        let list = words.join("  ");
        self.emit(&list);
        self.emit("\r\n");
        let prompt = self.prompt.clone();
        self.emit(&prompt);
        let end = self.line.len();
        self.emit_line_bytes(0, end);
    }

    /// Append completion-produced bytes (ASCII, from the grammar/vocabulary): step,
    /// push, echo. Stops at the first byte the parser refuses (defensive — completion
    /// bytes come from the grammar's own offers, so this should not trigger) or at the
    /// line cap. Returns whether anything was appended.
    fn append_completion_bytes(&mut self, bytes: &[u8]) -> bool {
        let mut appended = false;
        for &byte in bytes {
            if self.line.len() >= MAX_LINE_BYTES || !self.step_state(&[byte]) {
                break;
            }
            self.line.push(byte);
            self.out.push(char::from(byte));
            appended = true;
        }
        appended
    }

    // -- recall --------------------------------------------------------------------------

    /// ↑ (`back == true`) / ↓ through the recall view; the kernel console's semantics.
    fn recall_step(&mut self, back: bool) {
        if self.history.is_empty() {
            return;
        }
        match (back, self.recall) {
            (true, None) => {
                // First ↑: show the newest entry, stashing the in-progress line.
                let index = self.history.len() - 1;
                let entry = self.history[index].clone();
                self.recall = Some(index);
                self.stash = self.replace_line(entry.into_bytes());
            }
            (true, Some(0)) => {}
            (true, Some(index)) => {
                let entry = self.history[index - 1].clone();
                self.recall = Some(index - 1);
                self.replace_line(entry.into_bytes());
            }
            (false, None) => {}
            (false, Some(index)) if index + 1 >= self.history.len() => {
                // ↓ past the newest: restore the stashed fresh line.
                self.recall = None;
                let stash = core::mem::take(&mut self.stash);
                self.replace_line(stash);
            }
            (false, Some(index)) => {
                let entry = self.history[index + 1].clone();
                self.recall = Some(index + 1);
                self.replace_line(entry.into_bytes());
            }
        }
    }

    /// Erase the visible line and show `text` instead, repainting the marker if the
    /// replacement itself has a red tail (recalled entries can be lines that never
    /// parsed — history records what was executed, including errors). Returns the
    /// replaced line (the first ↑ stashes it).
    fn replace_line(&mut self, text: Vec<u8>) -> Vec<u8> {
        for _ in 0..char_count(&self.line) {
            self.emit("\u{8} \u{8}");
        }
        self.close_marker();
        let old = core::mem::replace(&mut self.line, text);
        self.utf8_pending.clear();
        self.reparse();
        let (begin, red_from) = (self.marker.begin, self.red_from);
        match red_from {
            Some(red) => {
                self.emit_line_bytes(0, red);
                self.emit(begin);
                let end = self.line.len();
                self.emit_line_bytes(red, end);
            }
            None => {
                let end = self.line.len();
                self.emit_line_bytes(0, end);
            }
        }
        old
    }

    /// Echo `line[start..end]` (always whole characters — `red_from` and the line ends
    /// sit on character boundaries by construction).
    fn emit_line_bytes(&mut self, start: usize, end: usize) {
        if let Ok(text) = core::str::from_utf8(&self.line[start..end]) {
            let text = String::from(text);
            self.emit(&text);
        }
    }

    /// Any edit while browsing commits the shown entry as the fresh line.
    fn commit_recall(&mut self) {
        if self.recall.take().is_some() {
            self.stash.clear();
        }
    }

    // -- plumbing -----------------------------------------------------------------------

    fn emit(&mut self, text: &str) {
        self.out.push_str(text);
    }

    /// Emit the marker-end sequence if the line is currently red (and forget the mark —
    /// callers are ending or replacing the line).
    fn close_marker(&mut self) {
        if self.red_from.take().is_some() {
            self.emit(self.marker.end);
        }
    }

    fn take_line(&mut self) -> String {
        let bytes = core::mem::take(&mut self.line);
        self.recall = None;
        self.stash.clear();
        // Valid UTF-8 by construction (characters are inserted whole); lossy is the
        // belt-and-suspenders.
        match String::from_utf8(bytes) {
            Ok(line) => line,
            Err(err) => String::from_utf8_lossy(err.as_bytes()).into_owned(),
        }
    }
}

/// Characters (not bytes) in a UTF-8 buffer — the erase-column count.
fn char_count(bytes: &[u8]) -> usize {
    bytes
        .iter()
        .filter(|&&byte| !(0x80..=0xbf).contains(&byte))
        .count()
}

/// The longest common prefix of a non-empty, sorted candidate list (byte-wise; all
/// candidates are valid UTF-8, and a byte-wise prefix of sorted UTF-8 stays on a
/// character boundary for the first/last pair).
fn longest_common_prefix(words: &[String]) -> String {
    let Some(first) = words.first() else {
        return String::new();
    };
    let mut len = first.len();
    for word in &words[1..] {
        let common = first
            .as_bytes()
            .iter()
            .zip(word.as_bytes())
            .take_while(|(a, b)| a == b)
            .count();
        len = len.min(common);
    }
    while len > 0 && !first.is_char_boundary(len) {
        len -= 1;
    }
    String::from(&first[..len])
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::inc::Tag;
    use alloc::borrow::ToOwned;
    use alloc::format;
    use alloc::string::ToString;
    use alloc::vec;

    fn vocab() -> Vocab {
        Vocab::new(
            [
                ("hello", Tag::Program),
                ("outcomes", Tag::Program),
                ("time.frozen", Tag::Program),
                ("time.fuzzy", Tag::Program),
                ("browser", Tag::Program),
                ("det", Tag::Binding),
            ]
            .into_iter()
            .map(|(word, tag)| (word.to_string(), tag))
            .collect(),
        )
    }

    fn editor() -> Editor {
        Editor::new("eosh> ", vocab(), Vec::new(), Marker::RED)
    }

    fn editor_with_history(history: &[&str]) -> Editor {
        Editor::new(
            "eosh> ",
            vocab(),
            history.iter().map(|s| s.to_string()).collect(),
            Marker::RED,
        )
    }

    /// Feed a string as char bytes.
    fn type_text(ed: &mut Editor, text: &str) {
        for &byte in text.as_bytes() {
            assert_eq!(ed.handle(Key::Char(byte)), Action::Pending);
        }
    }

    fn submit(ed: &mut Editor) -> String {
        match ed.handle(Key::Enter) {
            Action::Submit(line) => line,
            other => panic!("expected Submit, got {other:?}"),
        }
    }

    #[test]
    fn plain_typing_echoes_and_submits() {
        let mut ed = editor();
        type_text(&mut ed, "hello --name eo9");
        assert_eq!(ed.take_output(), "hello --name eo9");
        let line = submit(&mut ed);
        assert_eq!(line, "hello --name eo9");
        assert_eq!(ed.take_output(), "\r\n");
    }

    #[test]
    fn invalid_input_marks_red_once_and_enter_resets() {
        let mut ed = editor();
        // `help` takes no arguments: the `x` after `help ` has no viable parse.
        type_text(&mut ed, "help x");
        assert_eq!(ed.take_output(), "help \u{1b}[31mx");
        // More red input does not re-emit the marker.
        type_text(&mut ed, "y");
        assert_eq!(ed.take_output(), "y");
        // Enter closes the marker before the newline and still submits the line.
        let line = submit(&mut ed);
        assert_eq!(line, "help xy");
        assert_eq!(ed.take_output(), "\u{1b}[0m\r\n");
    }

    #[test]
    fn backspace_erases_and_clears_red_at_the_boundary() {
        let mut ed = editor();
        type_text(&mut ed, "help x");
        ed.take_output();
        // Erase `x`: rewinds to red_from → marker-end emitted, line green again.
        assert_eq!(ed.handle(Key::Backspace), Action::Pending);
        assert_eq!(ed.take_output(), "\u{8} \u{8}\u{1b}[0m");
        // Typing viable input now echoes plainly (still inside the trailing-space state).
        type_text(&mut ed, " ");
        assert_eq!(ed.take_output(), " ");
        let line = submit(&mut ed);
        assert_eq!(line, "help  ");
        assert_eq!(ed.take_output(), "\r\n");
    }

    #[test]
    fn backspace_inside_a_red_region_stays_red() {
        let mut ed = editor();
        type_text(&mut ed, "help xy");
        ed.take_output();
        // Erase `y`: still red (red_from points at `x`); no marker-end yet.
        ed.handle(Key::Backspace);
        assert_eq!(ed.take_output(), "\u{8} \u{8}");
        // Erase `x`: now the marker closes.
        ed.handle(Key::Backspace);
        assert_eq!(ed.take_output(), "\u{8} \u{8}\u{1b}[0m");
    }

    #[test]
    fn backspace_on_an_empty_line_does_nothing() {
        let mut ed = editor();
        assert_eq!(ed.handle(Key::Backspace), Action::Pending);
        assert_eq!(ed.take_output(), "");
    }

    #[test]
    fn unique_completion_fills_the_word_and_appends_a_space() {
        let mut ed = editor();
        // `hell` matches only the vocabulary's `hello` (`help` is the builtin branch,
        // but `hell` already diverged from it).
        type_text(&mut ed, "hell");
        ed.take_output();
        ed.handle(Key::Tab);
        assert_eq!(ed.take_output(), "o ");
        assert_eq!(submit(&mut ed), "hello ");
    }

    #[test]
    fn ambiguous_completion_extends_then_lists() {
        let mut ed = editor();
        type_text(&mut ed, "ti");
        ed.take_output();
        // First TAB: extend to the longest common prefix `time.f`.
        ed.handle(Key::Tab);
        assert_eq!(ed.take_output(), "me.f");
        // Second TAB: no further progress — candidate list, then repaint.
        ed.handle(Key::Tab);
        assert_eq!(
            ed.take_output(),
            "\r\ntime.frozen  time.fuzzy\r\neosh> time.f"
        );
        assert_eq!(submit(&mut ed), "time.f");
    }

    #[test]
    fn completion_mid_command_works_on_the_current_word() {
        let mut ed = editor();
        type_text(&mut ed, "time.frozen $ outc");
        ed.take_output();
        ed.handle(Key::Tab);
        assert_eq!(ed.take_output(), "omes ");
        assert_eq!(submit(&mut ed), "time.frozen $ outcomes ");
    }

    #[test]
    fn tab_on_a_red_line_rings_the_bell() {
        let mut ed = editor();
        type_text(&mut ed, "help x");
        ed.take_output();
        ed.handle(Key::Tab);
        assert_eq!(ed.take_output(), "\u{7}");
    }

    #[test]
    fn tab_with_no_candidates_rings_the_bell() {
        let mut ed = editor();
        type_text(&mut ed, "zzz");
        ed.take_output();
        ed.handle(Key::Tab);
        assert_eq!(ed.take_output(), "\u{7}");
    }

    #[test]
    fn forced_prefix_walks_before_completion() {
        let mut ed = editor();
        // After `svc lo` only `svc log` continues: the `g` is forced; the service-name
        // position that follows has no vocabulary, so the walk is all TAB does.
        type_text(&mut ed, "svc lo");
        ed.take_output();
        ed.handle(Key::Tab);
        assert_eq!(ed.take_output(), "g");
        assert_eq!(submit(&mut ed), "svc log");
    }

    #[test]
    fn history_recall_steps_back_and_forward() {
        let mut ed = editor_with_history(&["hello --name one", "outcomes"]);
        // ↑: newest entry.
        ed.handle(Key::Up);
        assert_eq!(ed.take_output(), "outcomes");
        // ↑ again: older entry (erase 8 columns, repaint).
        ed.handle(Key::Up);
        let out = ed.take_output();
        assert!(out.starts_with(&"\u{8} \u{8}".repeat(8)), "{out:?}");
        assert!(out.ends_with("hello --name one"), "{out:?}");
        // ↓ then ↓ past the newest: restores the (empty) fresh line.
        ed.handle(Key::Down);
        assert_eq!(ed.take_output().ends_with("outcomes"), true);
        ed.handle(Key::Down);
        let out = ed.take_output();
        assert!(out.ends_with("\u{8} \u{8}"), "{out:?}");
        assert_eq!(submit(&mut ed), "");
    }

    #[test]
    fn recall_stash_restores_the_fresh_line() {
        let mut ed = editor_with_history(&["outcomes"]);
        type_text(&mut ed, "dra");
        ed.take_output();
        ed.handle(Key::Up);
        let out = ed.take_output();
        assert!(out.contains("outcomes"), "{out:?}");
        ed.handle(Key::Down);
        let out = ed.take_output();
        assert!(out.ends_with("dra"), "{out:?}");
        type_text(&mut ed, "ft");
        assert_eq!(submit(&mut ed), "draft");
    }

    #[test]
    fn editing_a_recalled_entry_commits_it() {
        let mut ed = editor_with_history(&["hello"]);
        ed.handle(Key::Up);
        ed.take_output();
        type_text(&mut ed, " --name x");
        assert_eq!(submit(&mut ed), "hello --name x");
    }

    #[test]
    fn recalling_a_red_history_entry_repaints_the_marker() {
        // History records executed lines, including ones that failed to parse.
        let mut ed = editor_with_history(&["help x"]);
        ed.handle(Key::Up);
        assert_eq!(ed.take_output(), "help \u{1b}[31mx");
        // Backspacing the red tail clears the mark.
        ed.handle(Key::Backspace);
        assert_eq!(ed.take_output(), "\u{8} \u{8}\u{1b}[0m");
        assert_eq!(submit(&mut ed), "help ");
    }

    #[test]
    fn utf8_characters_assemble_echo_whole_and_erase_one_column() {
        let mut ed = editor();
        // `héllo` — the é arrives as its two bytes; the echo is the complete character.
        for &byte in "h".as_bytes() {
            ed.handle(Key::Char(byte));
        }
        assert_eq!(ed.take_output(), "h");
        let e_acute = "é".as_bytes();
        ed.handle(Key::Char(e_acute[0]));
        assert_eq!(ed.take_output(), "");
        ed.handle(Key::Char(e_acute[1]));
        assert_eq!(ed.take_output(), "é");
        type_text(&mut ed, "llo");
        ed.take_output();
        // Backspace erases one column and one whole character.
        ed.handle(Key::Backspace);
        ed.handle(Key::Backspace);
        ed.handle(Key::Backspace);
        ed.handle(Key::Backspace);
        assert_eq!(ed.take_output(), "\u{8} \u{8}".repeat(4));
        assert_eq!(submit(&mut ed), "h");
    }

    #[test]
    fn non_ascii_stays_green_in_word_positions_and_red_in_escapes() {
        let mut ed = editor();
        type_text(&mut ed, "héllo");
        let out = ed.take_output();
        assert!(!out.contains("\u{1b}[31m"), "{out:?}");
        assert_eq!(submit(&mut ed), "héllo");

        // `echo "bad \é"` — the escape position takes no non-ASCII byte.
        let mut ed = editor();
        type_text(&mut ed, "echo \"bad \\é");
        let out = ed.take_output();
        assert!(out.contains("\u{1b}[31m"), "{out:?}");
    }

    #[test]
    fn ctrl_c_cancels_the_line() {
        let mut ed = editor();
        type_text(&mut ed, "oops");
        ed.take_output();
        assert_eq!(ed.handle(Key::Ctrl(3)), Action::Submit(String::new()));
        assert_eq!(ed.take_output(), "^C\r\n");
    }

    #[test]
    fn ctrl_d_and_eof_end_the_session_only_on_an_empty_line() {
        let mut ed = editor();
        assert_eq!(ed.handle(Key::Ctrl(4)), Action::EndOfInput);

        let mut ed = editor();
        type_text(&mut ed, "x");
        assert_eq!(ed.handle(Key::Ctrl(4)), Action::Pending);
        assert_eq!(ed.handle(Key::Eof), Action::Submit("x".to_owned()));

        let mut ed = editor();
        assert_eq!(ed.handle(Key::Eof), Action::EndOfInput);
    }

    #[test]
    fn left_right_and_unknown_ctrl_are_ignored() {
        let mut ed = editor();
        type_text(&mut ed, "ab");
        ed.take_output();
        ed.handle(Key::Left);
        ed.handle(Key::Right);
        ed.handle(Key::Ctrl(11));
        assert_eq!(ed.take_output(), "");
        assert_eq!(submit(&mut ed), "ab");
    }

    #[test]
    fn line_cap_drops_input_silently() {
        let mut ed = editor();
        for _ in 0..MAX_LINE_BYTES {
            ed.handle(Key::Char(b'a'));
        }
        ed.take_output();
        ed.handle(Key::Char(b'b'));
        assert_eq!(ed.take_output(), "");
        let line = submit(&mut ed);
        assert_eq!(line.len(), MAX_LINE_BYTES);
        assert!(line.ends_with('a'));
    }

    #[test]
    fn recall_view_is_capped() {
        let history: Vec<String> = (0..100).map(|i| format!("line{i}")).collect();
        let ed = Editor::new("eosh> ", vocab(), history, Marker::RED);
        assert_eq!(ed.history.len(), RECALL_CAP);
        assert_eq!(ed.history[0], "line36");
        assert_eq!(ed.history.last().unwrap(), "line99");
    }

    #[test]
    fn inverse_marker_swaps_the_sequences() {
        let mut ed = Editor::new("eosh> ", vocab(), vec![], Marker::INVERSE);
        type_text(&mut ed, "help x");
        assert_eq!(ed.take_output(), "help \u{1b}[7mx");
        ed.handle(Key::Backspace);
        assert_eq!(ed.take_output(), "\u{8} \u{8}\u{1b}[27m");
    }
}
