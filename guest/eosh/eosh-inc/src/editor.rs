//! The per-keystroke line editor (the study's M2, docs/study/incremental-repl-for-eosh.md).
//!
//! A pure state machine: the embedder feeds decoded [`Key`]s (from the `eo9:text`
//! `read-key` operation) and drains the bytes the editor wants echoed
//! ([`Editor::take_output`]); nothing here does I/O, so the whole behavior is
//! host-testable with key vectors against the emitted byte sequences.
//!
//! Behavior (the study's design, refined by the M2 brief; M3 adds the vocabulary mark
//! and argument completion):
//!
//! * **Accept-and-mark, never refuse.** Every printable character is taken into the
//!   line. While the line is viable, characters echo plainly; the first dead character
//!   sets `red_from`, emits the marker-begin sequence once (SGR 31 by default — see
//!   [`Marker`]; the style is that one constant), and everything from there on echoes
//!   inside the marked region. A character is dead in either of two ways, and red
//!   means exactly "**this line will not execute successfully**":
//!
//!   - **parse-dead**: no parse of the [`crate::grammar`] continues through it — and
//!     since the unification that grammar IS the parser: the line cannot execute;
//!   - **name-dead** (M3, editor-layer only): in a NAME position — one whose
//!     completions carry name tags ([`crate::inc::Tag::is_name`]): the command head,
//!     post-`$`/`&`/`(`/`=` component positions — the word can no longer
//!     prefix-extend to ANY entry of the per-prompt vocabulary (builtins ∪ reserved ∪
//!     session bindings ∪ /bin, all present as completion sources), so resolution is
//!     guaranteed to fail at run time.
//!
//!   Both marks are honest — neither ever marks a line that would run; green only
//!   means the line still might. Name-marking lives entirely here in the editor: the
//!   grammar's acceptance (and the value it builds) never depends on the vocabulary,
//!   which is pinned by the adversarial-hints gate. Free-text positions (`let`/`save` names, service names,
//!   flag names, flag values, gate slots, quoted/compound/comment interiors) carry no
//!   name-tagged completions and are never name-marked; M3's argument candidates are
//!   tagged `Flag`/`Value` precisely so they cannot make a value position look like a
//!   name position. Words containing non-ASCII text are never name-marked either (the
//!   vocabulary is ASCII-filtered, so the editor cannot tell a typo from a real
//!   non-ASCII program name).
//! * **Backspace** erases one character (`\b \b` — one column) in O(1): the editor
//!   keeps a snapshot stack of parser states, one per green character (forward typing
//!   already produces the predecessor state — it is *moved* to the stack instead of
//!   dropped, so typing cost is unchanged), and backspace pops it. Dead characters
//!   were never committed to the parser, so erasing back to `red_from` just closes
//!   the marker and restores the saved word-tracker — no replay either way. This is
//!   the study's snapshot-per-char fallback, adopted after the board bench measured
//!   ~50 ms per parser step on target (a reparse-from-start backspace cost seconds on
//!   long lines). Memory: one state per line character, bounded by [`MAX_LINE_BYTES`].
//!   Wholesale line replacement (history recall, [`Editor::provide_args`]) rebuilds
//!   the stack in one O(N) replay; every backspace after it is O(1). Name-dead marks
//!   clear exactly like parse-dead ones.
//! * **TAB**: in a red line, just the bell — completion cannot help input the parser
//!   already rejects. Otherwise the forced-prefix walk first (bytes the grammar forces,
//!   e.g. `svc lo` → `g`), then `completions()`: a unique candidate completes the word
//!   plus a space (no space for `glue` candidates — canned prefixes like `http://`);
//!   several extend to their longest common prefix; no progress prints the candidate
//!   list on its own line — with the per-candidate description column when any
//!   candidate has one (M3: manual doc lines) — and repaints the prompt + line (the
//!   shape of the retired host editor, crates/eo9/src/editor.rs). When candidates from
//!   different word positions coexist (a completed flag name and the next value's
//!   candidates), the most-typed group wins — finish the word in progress first. One
//!   M3 guard: in a value position with nothing typed yet (all candidates
//!   `Value`-tagged), TAB always lists instead of auto-filling — values stay
//!   free-form, and a unique manual hint must not put words in the user's mouth.
//! * **Argument completion** (M3): the embedder drives the async edge. After a word
//!   ends (or on TAB), it asks [`Editor::wanted_args`] which typed /bin programs lack
//!   argument data, resolves them through the session's memo (describe + manual), and
//!   hands the result to [`Editor::provide_args`] — which re-arms the grammar with the
//!   program's flag and value candidates (never changing acceptance; the manuals
//!   design's additive-hints rule). Until then the generic v1 grammar applies.
//! * **Enter** always submits — the editor never blocks a line. On a green line the
//!   accumulated parse is finished and handed over in [`Action::Submit`]: the editor's
//!   states built the executed `Command` as it was typed, so the session runs exactly
//!   that value with no second parse. Red or incomplete lines submit without it; the
//!   session's parse of the line renders the positional error. The marker is closed
//!   first so the command's own output is never tinted.
//! * **↑/↓** recall over the history snapshot the embedder passed in — a capped view
//!   (64 entries, [`RECALL_CAP`]) of the session history (whose own cap is the GAPS
//!   "eosh session history is unbounded" fix, in eosh-core). Same semantics as the
//!   kernel console's read-line recall: the in-progress line is stashed on the first ↑,
//!   restored by ↓ past the newest entry, and any edit commits the shown entry.
//!
//! Line wrap (the board-console fix): the editor optionally knows the terminal width
//! (`Editor::new`'s `width`; `None` = the transport's width is unknown and the editor
//! never wraps — byte-for-byte today's behavior, the regression pin). With a width, the
//! editor owns the row geometry instead of trusting terminal auto-wrap (whose deferred
//! last-column state is exactly what made the recall repaint corrupt long lines):
//!
//! * echoing a character that fills the last column is followed by an explicit `\r\n`
//!   (so the cursor is always at a known column, never in the auto-wrap pending state);
//! * backspace across a row boundary moves up and erases the last cell of the previous
//!   row instead of emitting `\b` at column 0 (where it is a no-op);
//! * the recall/replacement repaint clears every row the prompt+line occupies — `\r`
//!   `CSI K`, then `CSI A` `CSI K` per additional row — and re-emits prompt + line.
//!
//! **The emitted-sequence contract** (keep in sync with fbcon's CSI subset — the
//! area/39 lane implements exactly this set; do not add sequences without coordinating):
//!
//! * `\b \b` — erase one cell, same row (the only erase the width-less editor uses);
//! * `\r` — column 0 of the current row; `\r\n` — explicit row advance;
//! * `CSI K` (`ESC [ K`) — erase from the cursor to the end of the row;
//! * `CSI A` (`ESC [ A`) — cursor up one row, column preserved;
//! * `CSI <n> G` (`ESC [ <n> G`) — cursor to absolute column n (1-based; used only by
//!   the wrap-boundary backspace to reach the last column of the row above);
//! * the [`Marker`] SGR pairs (`CSI 31/0 m` or `CSI 7/27 m`), zero-width.
//!
//! Column model: one character = one terminal cell (the same assumption `\b \b` has
//! always made; double-width CJK is out of scope with the rest of v1). The prompt is
//! assumed to fit on one row (`prompt < width`).
//!
//! UTF-8: `read-key` delivers multi-byte characters as their individual `char` bytes;
//! the editor assembles them (echoing only complete characters, so the emitted stream
//! stays valid UTF-8 for the `text.write` API) and steps the parser per byte (a byte
//! at 0x80 or above steps as [`crate::input::Input::Text`], the real byte reaching
//! captured values), so its
//! viability verdicts AND the accumulated parse agree exactly with a reparse. Invalid
//! sequences are dropped, like the host editor did.
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

use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;

use crate::comb::is_word_byte;
use crate::grammar::{ProgramArgs, Vocab, command_line};
use crate::inc::{BoxP, Completion, Step, Tag, finish, forced_prefix};
use crate::input::Input;
use eosh_core::ast::Command;

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
    /// the vocabulary and history snapshots are per prompt). `parsed` is the editor's
    /// accumulated parse, finished at Enter — when present, it IS the parse (the one
    /// grammar; the session must not parse the line again). `None` means the line was
    /// not green to its end (the session's parse produces the user-facing error).
    Submit {
        line: String,
        parsed: Option<Command>,
    },
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

/// One green character's snapshot: the parser state and word tracker as they were
/// BEFORE the character was committed — what backspace restores in O(1).
struct Snap {
    state: BoxP<Command>,
    in_word: bool,
    tracking: bool,
}

/// The editor for one prompt. Build it fresh per prompt (vocabulary and history are
/// per-prompt snapshots), feed keys, drain output after every call.
pub struct Editor {
    vocab: Vocab,
    prompt: String,
    /// The prompt's column count (chars; computed once — the wrap math's offset).
    prompt_cols: usize,
    /// Terminal width in columns, when the transport knows it (the kernel console's
    /// manifest record). `None` = never wrap = the exact pre-width byte stream.
    width: Option<usize>,
    marker: Marker,
    /// The line so far — always valid UTF-8 (characters are inserted whole).
    line: Vec<u8>,
    /// Parser state covering `line[..red_from.unwrap_or(line.len())]`.
    state: BoxP<Command>,
    /// One snapshot per GREEN character of `line` (dead characters are never
    /// committed to the parser): `stack[k]` is the state/tracker before character
    /// k+1. INVARIANT (pinned by the differential editor test): `stack.len()` equals
    /// the number of characters in `line[..red_from.unwrap_or(line.len())]`.
    stack: Vec<Snap>,
    /// First dead byte index (parse-dead or name-dead — module docs), if the line has
    /// gone red.
    red_from: Option<usize>,
    /// The word tracker as it was before the character at `red_from` — restored when
    /// backspace rewinds the mark away (meaningful only while `red_from` is set).
    red_tracker: (bool, bool),
    /// Whether the last (green) line byte was a word byte — the word-boundary tracker
    /// behind the name-dead mark.
    in_word: bool,
    /// Whether the word in progress started at a name position and is still
    /// prefix-viable against the vocabulary (meaningful only while `in_word`).
    tracking: bool,
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
    /// A fresh editor: `prompt` is what already sits on the screen line (used to
    /// repaint after a candidate list and by the wrap-aware repaint); `history` is the
    /// session recall snapshot, oldest first (only the newest [`RECALL_CAP`] are kept);
    /// `width` is the terminal width in columns when the transport knows it (`None` —
    /// or a degenerate `Some(0)` — means never wrap: today's exact byte stream, for
    /// transports that cannot say, e.g. the browser line transport).
    pub fn new(
        prompt: &str,
        vocab: Vocab,
        mut history: Vec<String>,
        marker: Marker,
        width: Option<usize>,
    ) -> Editor {
        if history.len() > RECALL_CAP {
            history.drain(..history.len() - RECALL_CAP);
        }
        let state = command_line(&vocab);
        Editor {
            vocab,
            prompt: String::from(prompt),
            prompt_cols: prompt.chars().count(),
            width: width.filter(|&w| w > 0),
            marker,
            line: Vec::new(),
            state,
            stack: Vec::new(),
            red_from: None,
            red_tracker: (false, false),
            in_word: false,
            tracking: false,
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

    /// The typed words that are /bin programs (per the vocabulary snapshot) and still
    /// lack argument data — what the embedder should resolve (memoized session-side)
    /// and hand back through [`Editor::provide_args`]. The embedder asks on word-end
    /// or TAB keys (the M3 "first TAB-or-word-end after a name resolves" trigger);
    /// asking is cheap and an empty answer means nothing to do.
    pub fn wanted_args(&self) -> Vec<String> {
        let mut wanted: Vec<String> = Vec::new();
        for word in self
            .line
            .split(|&byte| byte < 0x80 && !is_word_byte(byte))
            .filter(|word| !word.is_empty())
        {
            let Ok(word) = core::str::from_utf8(word) else {
                continue;
            };
            let known = self
                .vocab
                .entries
                .iter()
                .any(|(entry, tag)| *tag == Tag::Program && entry == word);
            if known && !self.vocab.programs.contains_key(word) && !wanted.iter().any(|w| w == word)
            {
                wanted.push(String::from(word));
            }
        }
        wanted
    }

    /// Provide one resolved program's argument data (or an empty [`ProgramArgs`] when
    /// resolution failed or the program takes none — either way the name stops being
    /// wanted). The grammar re-arms with the program's flag and value candidates;
    /// acceptance, and therefore the marker, cannot change (the additive-hints rule),
    /// so this never emits anything.
    pub fn provide_args(&mut self, name: &str, args: ProgramArgs) {
        self.vocab.programs.insert(String::from(name), args);
        self.rebuild();
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
                let parsed = self.finish_parse();
                self.close_marker();
                self.emit("\r\n");
                Action::Submit {
                    line: self.take_line(),
                    parsed,
                }
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
                Action::Submit {
                    line: String::new(),
                    parsed: None,
                }
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
                    let parsed = self.finish_parse();
                    self.close_marker();
                    self.emit("\r\n");
                    Action::Submit {
                        line: self.take_line(),
                        parsed,
                    }
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

    /// Advance the word tracker for the first byte of an incoming character, BEFORE
    /// the parser steps it (the arming query reads the pre-character state). The
    /// name-mark arms only on a word that starts at a name position (a strong
    /// name-tagged completion source is alive) with a byte the vocabulary could start
    /// with: not non-ASCII (the vocabulary is ASCII-filtered — module docs) and not a
    /// compound opener (`let [a] = x` makes `[a]` a real, resolvable binding name).
    fn arm_tracker(&mut self, first: u8) {
        if first >= 0x80 || is_word_byte(first) {
            if !self.in_word {
                self.in_word = true;
                self.tracking = first < 0x80
                    && first != b'['
                    && first != b'{'
                    && name_completions(&self.state).strong;
            }
            if first >= 0x80 {
                // Non-ASCII words are never name-marked (module docs).
                self.tracking = false;
            }
        } else {
            self.in_word = false;
            self.tracking = false;
        }
    }

    /// Insert one complete character (1..=4 bytes, already validated): step the parser
    /// unless the line is red, mark red on the first dead position (parse-dead or
    /// name-dead — module docs), echo. A committed (green) character pushes its
    /// predecessor state onto the snapshot stack — the state is moved, not cloned, so
    /// this costs nothing beyond the step itself.
    /// The stack invariant (struct docs): one snapshot per green CHARACTER of the
    /// line. Checked in debug builds after every mutation of line/stack/red_from —
    /// every host test runs it on every key; release builds (the shipped guest)
    /// compile it out. The differential tests additionally pin pop == reparse.
    #[inline]
    fn debug_check_stack(&self) {
        if cfg!(debug_assertions) {
            let green_end = self.red_from.unwrap_or(self.line.len());
            let green_chars = self.line[..green_end]
                .iter()
                .filter(|&&byte| !(0x80..=0xbf).contains(&byte))
                .count();
            debug_assert_eq!(
                self.stack.len(),
                green_chars,
                "snapshot stack out of step with the green prefix"
            );
        }
    }

    fn insert_char(&mut self, bytes: &[u8]) {
        if self.line.len() + bytes.len() > MAX_LINE_BYTES {
            // The kernel console's policy at the cap: drop, no echo.
            return;
        }
        self.commit_recall();
        if self.red_from.is_none() {
            let pre = (self.in_word, self.tracking);
            self.arm_tracker(bytes[0]);
            let next = try_step(&self.state, bytes).filter(|next| {
                // Name-dead: the word can no longer prefix-extend to any vocabulary
                // entry — treat like a failed step (the parser state stays at the
                // green prefix; the parse itself would have continued).
                !(self.tracking && !name_completions(next).any)
            });
            match next {
                Some(next) => {
                    let prev = core::mem::replace(&mut self.state, next);
                    self.stack.push(Snap {
                        state: prev,
                        in_word: pre.0,
                        tracking: pre.1,
                    });
                }
                None => {
                    self.red_from = Some(self.line.len());
                    self.red_tracker = pre;
                    self.emit(self.marker.begin);
                }
            }
        }
        self.line.extend_from_slice(bytes);
        // `bytes` is one valid UTF-8 character by construction.
        if let Ok(text) = core::str::from_utf8(bytes) {
            self.emit(text);
        }
        // Width-aware echo (module docs, "Line wrap"): a character that filled the last
        // column is followed by an explicit row advance — never the auto-wrap limbo.
        self.advance_row_if_full();
        self.debug_check_stack();
    }

    // -- the width-aware output layer ---------------------------------------------------
    //
    // Everything here is OUTPUT ONLY: parser state, the snapshot stack, and the marker
    // logic never consult the width (the M3 differential gates are pinned on that).
    // With `width == None` none of these emit anything beyond the historical bytes.

    /// Display cells occupied by prompt + line (one cell per character — module docs).
    fn display_cols(&self) -> usize {
        self.prompt_cols + char_count(&self.line)
    }

    /// After echoing one character: if it filled the last column of a row, advance to
    /// the next row explicitly. No-op without a width.
    fn advance_row_if_full(&mut self) {
        if let Some(width) = self.width
            && self.display_cols().is_multiple_of(width)
        {
            self.emit("\r\n");
        }
    }

    /// Echo `text`, inserting the explicit `\r\n` row advance whenever a character
    /// fills the last column. `col` is the absolute display column the text starts at,
    /// advanced as it goes (the repaint paths thread it through prompt and line
    /// segments so the marker SGRs — zero-width — can interleave).
    fn emit_wrapped(&mut self, text: &str, width: usize, col: &mut usize) {
        for ch in text.chars() {
            self.out.push(ch);
            *col += 1;
            if col.is_multiple_of(width) {
                self.out.push_str("\r\n");
            }
        }
    }

    /// Erase one display cell, wrap-aware: at column 0 of a wrapped row, move up to the
    /// last column of the row above and clear it (`CSI A`, `CSI <width> G`, `CSI K`);
    /// anywhere else — and always without a width — the historical `\b \b`.
    /// `display_before` is the cursor's display position BEFORE the erased character
    /// left the line.
    fn erase_one_cell(&mut self, display_before: usize) {
        match self.width {
            Some(width) if display_before.is_multiple_of(width) => {
                let sequence = format!("\u{1b}[A\u{1b}[{width}G\u{1b}[K");
                self.emit(&sequence);
            }
            _ => self.emit("\u{8} \u{8}"),
        }
    }

    // -- backspace ---------------------------------------------------------------------

    fn on_backspace(&mut self) {
        if self.line.is_empty() {
            return;
        }
        self.commit_recall();
        // The cursor's display position before the erase — the wrap-boundary test
        // (column 0 of a wrapped row ⇒ the erased character sits on the row above).
        let display_before = self.display_cols();
        // Pop one whole character (continuation bytes, then the lead).
        while let Some(&byte) = self.line.last() {
            self.line.pop();
            if !(0x80..=0xbf).contains(&byte) {
                break;
            }
        }
        self.erase_one_cell(display_before);
        match self.red_from {
            Some(red) if self.line.len() <= red => {
                // Rewound to the first dead byte: green again. Dead characters were
                // never committed to the parser, so the state already covers exactly
                // the surviving prefix — close the marker, restore the word tracker
                // saved when the mark opened. O(1), no replay (the board measured
                // ~50 ms per parser step; a reparse here cost seconds on long lines).
                self.emit(self.marker.end);
                self.red_from = None;
                let (in_word, tracking) = self.red_tracker;
                self.in_word = in_word;
                self.tracking = tracking;
            }
            Some(_) => {
                // Still red beyond `red_from`: the green prefix (and its state) is
                // untouched, nothing to recompute.
            }
            None => {
                // Green: pop the erased character's snapshot — O(1).
                match self.stack.pop() {
                    Some(snap) => {
                        self.state = snap.state;
                        self.in_word = snap.in_word;
                        self.tracking = snap.tracking;
                    }
                    // Unreachable under the stack invariant; rebuild defensively.
                    None => self.rebuild(),
                }
            }
        }
        self.debug_check_stack();
    }

    /// Recompute everything from the line — parser state, snapshot stack, `red_from`
    /// (parse-dead or name-dead), word tracker — character by character, identical to
    /// the incremental path in [`Editor::insert_char`]. The one O(N) path, used only
    /// for wholesale line replacement (history recall, [`Editor::provide_args`]);
    /// backspace pops snapshots instead. Emits nothing.
    fn rebuild(&mut self) {
        self.stack.clear();
        self.state = command_line(&self.vocab);
        self.red_from = None;
        self.red_tracker = (false, false);
        self.in_word = false;
        self.tracking = false;
        let line = core::mem::take(&mut self.line);
        let mut index = 0;
        while index < line.len() {
            // One whole character: the lead byte plus its continuations.
            let mut end = index + 1;
            while end < line.len() && (0x80..=0xbf).contains(&line[end]) {
                end += 1;
            }
            let pre = (self.in_word, self.tracking);
            self.arm_tracker(line[index]);
            let next = try_step(&self.state, &line[index..end])
                .filter(|next| !(self.tracking && !name_completions(next).any));
            match next {
                Some(next) => {
                    let prev = core::mem::replace(&mut self.state, next);
                    self.stack.push(Snap {
                        state: prev,
                        in_word: pre.0,
                        tracking: pre.1,
                    });
                }
                None => {
                    self.red_from = Some(index);
                    self.red_tracker = pre;
                    break;
                }
            }
            index = end;
        }
        self.line = line;
        self.debug_check_stack();
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
        // Candidates from different word positions can coexist (a just-completed flag
        // name and the next value's candidates): the most-typed group is the word in
        // progress — finish that first.
        let matched = completions
            .iter()
            .map(|c| c.matched)
            .max()
            .expect("non-empty");
        let mut candidates: Vec<Completion> = completions
            .into_iter()
            .filter(|c| c.matched == matched)
            .collect();
        candidates.sort_by(|a, b| a.word.cmp(&b.word));
        candidates.dedup_by(|a, b| a.word == b.word);
        // The M3 value guard: in a value position with nothing typed yet (every
        // candidate Value-tagged), always list — values stay free-form, and a unique
        // manual hint must not be auto-typed into the user's line.
        let value_menu = matched == 0 && candidates.iter().all(|c| c.tag == Tag::Value);
        if !value_menu {
            if candidates.len() == 1 {
                let rest: Vec<u8> = candidates[0].word.as_bytes()[matched..].to_vec();
                self.append_completion_bytes(&rest);
                if !candidates[0].glue {
                    self.append_completion_bytes(b" ");
                }
                return;
            }
            let words: Vec<String> = candidates.iter().map(|c| c.word.clone()).collect();
            let prefix = longest_common_prefix(&words);
            if prefix.len() > matched {
                let rest: Vec<u8> = prefix.as_bytes()[matched..].to_vec();
                if self.append_completion_bytes(&rest) {
                    return;
                }
            }
        }
        if progressed {
            return;
        }
        // No further progress: list the candidates, then repaint prompt + line (which
        // is green here — red lines bailed to the bell above). Flags display with
        // their `--`; a description column appears when any candidate has one.
        self.emit("\r\n");
        let shown: Vec<String> = candidates.iter().map(display_word).collect();
        if candidates.iter().any(|c| c.desc.is_some()) {
            let width = shown.iter().map(|w| w.chars().count()).max().unwrap_or(0);
            for (cand, word) in candidates.iter().zip(&shown) {
                let line = match &cand.desc {
                    Some(desc) => format!("{word:<width$}  {desc}"),
                    None => word.clone(),
                };
                self.emit(&line);
                self.emit("\r\n");
            }
        } else {
            let list = shown.join("  ");
            self.emit(&list);
            self.emit("\r\n");
        }
        let prompt = self.prompt.clone();
        match self.width {
            None => {
                self.emit(&prompt);
                let end = self.line.len();
                self.emit_line_bytes(0, end);
            }
            Some(width) => {
                // The candidate list left the cursor at column 0 of a fresh row:
                // wrapped echo keeps the row geometry consistent for what follows.
                // (The line is green here — red lines bailed to the bell above.)
                let mut col = 0;
                self.emit_wrapped(&prompt, width, &mut col);
                let line = line_text(&self.line, 0, self.line.len());
                self.emit_wrapped(&line, width, &mut col);
            }
        }
    }

    /// Append completion-produced bytes (ASCII, from the grammar/vocabulary): step,
    /// push, echo. Stops at the first byte the parser refuses (defensive — completion
    /// bytes come from the grammar's own offers, so this should not trigger) or at the
    /// line cap. Returns whether anything was appended.
    ///
    /// The word tracker and snapshot stack are maintained exactly like typing would
    /// (so a character typed — or erased — right after a completion behaves
    /// correctly), but appended bytes never *mark*: they are the grammar's own
    /// offers, viable by construction.
    fn append_completion_bytes(&mut self, bytes: &[u8]) -> bool {
        let mut appended = false;
        for &byte in bytes {
            if self.line.len() >= MAX_LINE_BYTES {
                break;
            }
            let pre = (self.in_word, self.tracking);
            self.arm_tracker(byte);
            match try_step(&self.state, &[byte]) {
                Some(next) => {
                    let prev = core::mem::replace(&mut self.state, next);
                    self.stack.push(Snap {
                        state: prev,
                        in_word: pre.0,
                        tracking: pre.1,
                    });
                }
                None => {
                    // Defensive (grammar offers step by construction): undo the
                    // tracker advance for the byte we are not taking.
                    self.in_word = pre.0;
                    self.tracking = pre.1;
                    break;
                }
            }
            self.line.push(byte);
            self.out.push(char::from(byte));
            self.advance_row_if_full();
            appended = true;
        }
        self.debug_check_stack();
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
    ///
    /// Without a width: per-character `\b \b` on the one row (today's exact bytes —
    /// the prompt is never repainted). With one: prompt + line span
    /// `display_cols / width + 1` rows; clear the cursor's row (`\r` `CSI K`), then
    /// each row above (`CSI A` `CSI K` — `CSI A` preserves the column, which `\r`
    /// already put at 0), and re-emit prompt + replacement with wrapped echo. The
    /// erase-by-rows is what makes recall correct past the width: `\b` cannot cross a
    /// row boundary (the board bench's render-error report).
    fn replace_line(&mut self, text: Vec<u8>) -> Vec<u8> {
        match self.width {
            None => {
                for _ in 0..char_count(&self.line) {
                    self.emit("\u{8} \u{8}");
                }
            }
            Some(width) => {
                self.close_marker();
                self.emit("\r\u{1b}[K");
                for _ in 0..self.display_cols() / width {
                    self.emit("\u{1b}[A\u{1b}[K");
                }
            }
        }
        self.close_marker();
        let old = core::mem::replace(&mut self.line, text);
        self.utf8_pending.clear();
        self.rebuild();
        match self.width {
            None => {
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
            }
            Some(width) => {
                // The row clear wiped the prompt too: repaint it, then the line, all
                // through the wrapped echo so the cursor lands at a known column.
                let prompt = self.prompt.clone();
                let mut col = 0;
                self.emit_wrapped(&prompt, width, &mut col);
                let (begin, red_from) = (self.marker.begin, self.red_from);
                let end = self.line.len();
                let split = red_from.unwrap_or(end);
                let green = line_text(&self.line, 0, split);
                self.emit_wrapped(&green, width, &mut col);
                if red_from.is_some() {
                    self.emit(begin);
                    let red = line_text(&self.line, split, end);
                    self.emit_wrapped(&red, width, &mut col);
                }
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

    /// The accumulated parse, finished: `Some` exactly when the whole line is green
    /// and the grammar can wrap up at end of line — the submitted [`Command`], built
    /// by the same states every keystroke stepped. Red or incomplete lines yield
    /// `None`; the session's own parse of the line then produces the error message.
    fn finish_parse(&self) -> Option<Command> {
        if self.red_from.is_some() {
            return None;
        }
        finish(&*self.state)
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

/// Step one character's bytes from `state` WITHOUT mutating it (bytes >= 0x80 step
/// as [`Input::Text`], like `feed_bytes`, so the editor's verdicts — and the captured
/// values — agree exactly with a from-scratch reparse). `None` when no parse
/// continues; on success the caller owns the new state (and can move the predecessor
/// onto the snapshot stack).
fn try_step(state: &BoxP<Command>, bytes: &[u8]) -> Option<BoxP<Command>> {
    let mut current: Option<BoxP<Command>> = None;
    for &byte in bytes {
        let at: &BoxP<Command> = current.as_ref().unwrap_or(state);
        current = Some(at.step(Input::of_byte(byte)).and_then(Step::cont)?);
    }
    current
}

/// What the name-marking oracle sees at a parser state (module docs, "name-dead").
struct NameCompletions {
    /// Any name-tagged completion alive ([`Tag::is_name`]) — the word can still
    /// prefix-extend to a vocabulary entry (keywords count: `wit…` is heading to
    /// `with`).
    any: bool,
    /// Any STRONG name-tagged completion alive (name tags minus `Keyword`) — the
    /// position resolves a name, so a word here is worth tracking. Keyword-only
    /// positions (`… as …` slots) also admit free positional words and must not arm
    /// the mark.
    strong: bool,
}

fn name_completions(state: &BoxP<Command>) -> NameCompletions {
    let mut completions: Vec<Completion> = Vec::new();
    state.completions(&mut completions);
    let mut any = false;
    let mut strong = false;
    for completion in &completions {
        if completion.tag.is_name() {
            any = true;
            if completion.tag != Tag::Keyword {
                strong = true;
            }
        }
    }
    NameCompletions { any, strong }
}

/// How a candidate displays in the TAB list: flag names carry their `--`.
fn display_word(completion: &Completion) -> String {
    match completion.tag {
        Tag::Flag => format!("--{}", completion.word),
        _ => completion.word.clone(),
    }
}

/// `line[start..end]` as an owned string (always whole characters by construction;
/// empty on the defensive invalid-UTF-8 path, like [`Editor::emit_line_bytes`]).
fn line_text(line: &[u8], start: usize, end: usize) -> String {
    match core::str::from_utf8(&line[start..end]) {
        Ok(text) => String::from(text),
        Err(_) => String::new(),
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
    use crate::grammar::FlagSpec;
    use crate::inc::Tag;
    use alloc::borrow::ToOwned;
    use alloc::boxed::Box;
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
                ("draft", Tag::Program),
                ("net.l4.over-l2", Tag::Program),
                ("net.text", Tag::Program),
                ("det", Tag::Binding),
            ]
            .into_iter()
            .map(|(word, tag)| (word.to_string(), tag))
            .collect(),
        )
    }

    fn editor() -> Editor {
        Editor::new("eosh> ", vocab(), Vec::new(), Marker::RED, None)
    }

    fn editor_with_history(history: &[&str]) -> Editor {
        Editor::new(
            "eosh> ",
            vocab(),
            history.iter().map(|s| s.to_string()).collect(),
            Marker::RED,
            None,
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
            Action::Submit { line, .. } => line,
            other => panic!("expected Submit, got {other:?}"),
        }
    }

    fn submit_parsed(ed: &mut Editor) -> (String, Option<Command>) {
        match ed.handle(Key::Enter) {
            Action::Submit { line, parsed } => (line, parsed),
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
        // After `svc lo` only `svc log` continues: the `g` is forced; the now-complete
        // keyword is the unique candidate, so TAB also appends the separating space
        // (the service name that follows is free).
        type_text(&mut ed, "svc lo");
        ed.take_output();
        ed.handle(Key::Tab);
        assert_eq!(ed.take_output(), "g ");
        assert_eq!(submit(&mut ed), "svc log ");
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
        // A viable prefix (of `browser`) — the green-line cancel shape.
        type_text(&mut ed, "brows");
        ed.take_output();
        assert_eq!(
            ed.handle(Key::Ctrl(3)),
            Action::Submit {
                line: String::new(),
                parsed: None
            }
        );
        assert_eq!(ed.take_output(), "^C\r\n");
        // On a red line the marker closes before the ^C echo.
        let mut ed = editor();
        type_text(&mut ed, "oops");
        ed.take_output();
        assert_eq!(
            ed.handle(Key::Ctrl(3)),
            Action::Submit {
                line: String::new(),
                parsed: None
            }
        );
        assert_eq!(ed.take_output(), "\u{1b}[0m^C\r\n");
    }

    #[test]
    fn ctrl_d_and_eof_end_the_session_only_on_an_empty_line() {
        let mut ed = editor();
        assert_eq!(ed.handle(Key::Ctrl(4)), Action::EndOfInput);

        let mut ed = editor();
        type_text(&mut ed, "x");
        assert_eq!(ed.handle(Key::Ctrl(4)), Action::Pending);
        // `x` names nothing in this vocabulary (name-dead red), so the accumulated
        // parse is withheld: the session's own parse renders the verdict.
        assert_eq!(
            ed.handle(Key::Eof),
            Action::Submit {
                line: "x".to_owned(),
                parsed: None
            }
        );

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
        let ed = Editor::new("eosh> ", vocab(), history, Marker::RED, None);
        assert_eq!(ed.history.len(), RECALL_CAP);
        assert_eq!(ed.history[0], "line36");
        assert_eq!(ed.history.last().unwrap(), "line99");
    }

    // -- vocabulary-aware marking (M3 deliverable 1) ---------------------------------

    #[test]
    fn name_dead_word_marks_at_the_first_dead_character() {
        // The owner's flagship: `net.x` cannot prefix-extend to any vocabulary entry
        // (net.l4.over-l2, net.text) — the `x` marks, the viable `net.` does not.
        let mut ed = editor();
        type_text(&mut ed, "net.x");
        assert_eq!(ed.take_output(), "net.\u{1b}[31mx");
        // Further input stays inside the marked region without re-emitting the marker.
        type_text(&mut ed, "yz");
        assert_eq!(ed.take_output(), "yz");
        // Enter closes the marker and still submits (accept-and-mark, never refuse).
        assert_eq!(submit(&mut ed), "net.xyz");
        assert_eq!(ed.take_output(), "\u{1b}[0m\r\n");
    }

    #[test]
    fn backspace_clears_a_name_dead_mark_exactly_like_parse_dead() {
        let mut ed = editor();
        type_text(&mut ed, "net.x");
        ed.take_output();
        // Erase the dead `x`: rewinds to red_from → marker-end, green again.
        ed.handle(Key::Backspace);
        assert_eq!(ed.take_output(), "\u{8} \u{8}\u{1b}[0m");
        // …and completing to a LONGER valid name stays green end to end.
        type_text(&mut ed, "l4.over-l2");
        let out = ed.take_output();
        assert!(!out.contains("\u{1b}[31m"), "{out:?}");
        assert_eq!(submit(&mut ed), "net.l4.over-l2");
        assert_eq!(ed.take_output(), "\r\n");
    }

    #[test]
    fn vocabulary_names_and_bindings_never_mark() {
        // A /bin name, a session binding, and a builtin — fully typed, with boundary,
        // in head and post-$ positions: never marked.
        for line in [
            "net.l4.over-l2 ",
            "det ",
            "help",
            "history",
            "time.frozen $ hello",
            "time.frozen & net.text $ browser",
            "(net.text) $ hello",
            "let x = det",
        ] {
            let mut ed = editor();
            type_text(&mut ed, line);
            let out = ed.take_output();
            assert!(!out.contains("\u{1b}[31m"), "{line:?} marked: {out:?}");
        }
    }

    #[test]
    fn a_session_binding_in_the_snapshot_does_not_mark() {
        // The let-binding case: the per-prompt snapshot carries bindings created this
        // session (the embedder's snapshot_vocab), so typing one at the next prompt
        // stays green — here `det`, present as Tag::Binding.
        let mut ed = editor();
        type_text(&mut ed, "det");
        assert_eq!(ed.take_output(), "det");
        assert_eq!(submit(&mut ed), "det");
    }

    #[test]
    fn name_dead_marks_in_post_compose_positions_too() {
        let mut ed = editor();
        type_text(&mut ed, "time.frozen $ net.q");
        let out = ed.take_output();
        assert!(out.ends_with("\u{1b}[31mq"), "{out:?}");
        // And inside parens (a name position behind `(`).
        let mut ed = editor();
        type_text(&mut ed, "hello --p (net.q");
        let out = ed.take_output();
        assert!(out.ends_with("\u{1b}[31mq"), "{out:?}");
    }

    #[test]
    fn free_text_positions_never_name_mark() {
        // let/save names, service names, flag names, flag values, positional words,
        // gate slots, quoted and comment interiors: all free text — no vocabulary, no
        // mark, even though none of these words are in the vocabulary.
        for line in [
            "let zzz = det",
            "save qq = det",
            "svc log ghostname",
            "hello --zzz freeform",
            "hello plainarg",
            "only eo9:zzz $ hello",
            "rename aa bb $ hello",
            "hello # zzz comment",
            "hello \"zzz text\"",
            "with hello as zz $ det",
        ] {
            let mut ed = editor();
            type_text(&mut ed, line);
            let out = ed.take_output();
            assert!(!out.contains("\u{1b}[31m"), "{line:?} marked: {out:?}");
        }
    }

    #[test]
    fn keyword_only_positions_do_not_arm_the_mark() {
        // After `with hello `, the grammar offers only the keyword `as` — but a free
        // positional word is also viable there (`with hello az as x $ y` parses, `az`
        // is an argument), so the mark must not arm on keyword-only evidence.
        let mut ed = editor();
        type_text(&mut ed, "with hello az");
        let out = ed.take_output();
        assert!(!out.contains("\u{1b}[31m"), "{out:?}");
    }

    #[test]
    fn marking_is_per_prompt_vocabulary_not_hardcoded() {
        // An empty vocabulary still tracks at the head (builtins are alive there):
        // a word that is no builtin prefix marks.
        let mut ed = Editor::new("eosh> ", Vocab::default(), Vec::new(), Marker::RED, None);
        type_text(&mut ed, "zq");
        let out = ed.take_output();
        assert!(out.contains("\u{1b}[31m"), "{out:?}");
        // `h` extends to builtins (help, history): green until it dies.
        let mut ed = Editor::new("eosh> ", Vocab::default(), Vec::new(), Marker::RED, None);
        type_text(&mut ed, "help");
        assert_eq!(ed.take_output(), "help");
    }

    #[test]
    fn enter_hands_over_the_accumulated_parse() {
        use eosh_core::ast::{Arg, ArgValue, Expr};
        // A green line: Enter's Submit carries the Command the keystroke states
        // accumulated — the same value `parse_command` builds, with no second parse.
        let mut ed = editor();
        type_text(&mut ed, "hello --name eo9");
        let (line, parsed) = submit_parsed(&mut ed);
        assert_eq!(line, "hello --name eo9");
        let expected = eosh_core::parse_command(&line).expect("parses");
        assert_eq!(parsed, Some(expected.clone()));
        assert_eq!(
            parsed,
            Some(Command::Run(Expr::App {
                callee: Box::new(Expr::Name("hello".into())),
                args: vec![Arg::Flag {
                    name: "name".into(),
                    value: ArgValue::Word("eo9".into()),
                }],
            }))
        );

        // A parse-dead red line withholds it (the session's parse is the verdict).
        let mut ed = editor();
        type_text(&mut ed, "help x");
        let (_, parsed) = submit_parsed(&mut ed);
        assert_eq!(parsed, None);

        // A green-but-incomplete line (viable prefix, cannot finish) withholds it too.
        let mut ed = editor();
        type_text(&mut ed, "let x = ");
        let (_, parsed) = submit_parsed(&mut ed);
        assert_eq!(parsed, None);

        // Non-ASCII words round-trip exactly (the Text-input path: real bytes, not
        // substitutes, reach the captured value).
        let mut ed = editor();
        type_text(&mut ed, "hello --name niño");
        let (line, parsed) = submit_parsed(&mut ed);
        assert_eq!(
            parsed,
            Some(eosh_core::parse_command(&line).expect("parses"))
        );
        match parsed {
            Some(Command::Run(Expr::App { args, .. })) => {
                assert_eq!(
                    args,
                    vec![Arg::Flag {
                        name: "name".into(),
                        value: ArgValue::Word("niño".into()),
                    }]
                );
            }
            other => panic!("unexpected parse: {other:?}"),
        }
    }

    #[test]
    fn describe_completes_and_keeps_carded_words_green() {
        // The owner's report, leg 1: `describe d…` must offer (and complete) the
        // builtins' own cards from the same table acceptance routes on. `descr` is
        // unique → TAB finishes the word plus the separating space.
        let mut ed = editor();
        type_text(&mut ed, "describe descr");
        ed.take_output();
        ed.handle(Key::Tab);
        assert_eq!(ed.take_output(), "ibe ");
        assert_eq!(submit(&mut ed), "describe describe ");

        // And the fully typed word never marks: `describe describe` is a card.
        for line in [
            "describe describe",
            "describe help",
            "describe compose",
            "describe eo9:fs/fs",
            "man describe",
            "man let",
            "man eo9:fs/fs",
        ] {
            let mut ed = editor();
            type_text(&mut ed, line);
            let out = ed.take_output();
            assert!(!out.contains("\u{1b}[31m"), "{line:?} marked: {out:?}");
        }
    }

    #[test]
    fn man_argument_completes_programs_and_cards() {
        // `man hell` → the /bin program `hello` (the cards' `help` already diverged).
        let mut ed = editor();
        type_text(&mut ed, "man hell");
        ed.take_output();
        ed.handle(Key::Tab);
        assert_eq!(ed.take_output(), "o ");
        assert_eq!(submit(&mut ed), "man hello ");
    }

    #[test]
    fn escribe_marks_as_name_dead_in_both_positions() {
        // The owner's report, leg 2: `escribe` (the d-less typo) paints red — and that
        // is CORRECT under the documented red semantics ("this line will not execute
        // successfully"): no card, no builtin, no /bin name, no binding can ever
        // prefix-extend from `es`, so resolution is guaranteed to fail at run time.
        // Head position: `e` still extends (env, exit), `s` is name-dead.
        let mut ed = editor();
        type_text(&mut ed, "escribe");
        assert_eq!(ed.take_output(), "e\u{1b}[31mscribe");
        assert_eq!(submit(&mut ed), "escribe");

        // Argument position: `describe escribe` would parse (an expression), but
        // `escribe` resolves nowhere — name-dead, same honest red.
        let mut ed = editor();
        type_text(&mut ed, "describe escribe");
        let out = ed.take_output();
        assert!(out.contains("\u{1b}[31m"), "{out:?}");
    }

    // -- argument completion (M3 deliverable 2) ----------------------------------------

    /// The flagship program args: net.l4.over-l2's signature dressed with its manual
    /// (docs/design/component-manuals.md §2's example, the v2 acceptance case).
    fn l4_args() -> ProgramArgs {
        ProgramArgs {
            flags: vec![
                FlagSpec {
                    name: "address".to_string(),
                    ty: "string".to_string(),
                    doc: Some("IPv4 acquisition mode".to_string()),
                    values: vec!["dhcp".to_string()],
                    kind: None,
                },
                FlagSpec {
                    name: "prefix-length".to_string(),
                    ty: "option<u8>".to_string(),
                    doc: Some("subnet prefix length".to_string()),
                    values: Vec::new(),
                    kind: None,
                },
                FlagSpec {
                    name: "gateway".to_string(),
                    ty: "option<string>".to_string(),
                    doc: None,
                    values: Vec::new(),
                    kind: None,
                },
            ],
        }
    }

    #[test]
    fn flag_completion_from_the_provided_signature() {
        // `net.l4.over-l2 --a<TAB>` → `--address` (the flagship).
        let mut ed = editor();
        ed.provide_args("net.l4.over-l2", l4_args());
        type_text(&mut ed, "net.l4.over-l2 --a");
        ed.take_output();
        ed.handle(Key::Tab);
        assert_eq!(ed.take_output(), "ddress ");
        // `--address <TAB>` → the manual's `dhcp`, LISTED (values stay free-form —
        // a unique hint is never auto-typed), then the prompt+line repaint.
        ed.handle(Key::Tab);
        assert_eq!(
            ed.take_output(),
            "\r\ndhcp\r\neosh> net.l4.over-l2 --address "
        );
        // Typing a prefix of the hint completes normally.
        type_text(&mut ed, "dh");
        ed.take_output();
        ed.handle(Key::Tab);
        assert_eq!(ed.take_output(), "cp ");
        assert_eq!(submit(&mut ed), "net.l4.over-l2 --address dhcp ");
    }

    #[test]
    fn flag_list_shows_descriptions_from_the_manual() {
        let mut ed = editor();
        ed.provide_args("net.l4.over-l2", l4_args());
        type_text(&mut ed, "net.l4.over-l2 --");
        ed.take_output();
        ed.handle(Key::Tab);
        let out = ed.take_output();
        assert!(out.contains("--address"), "{out:?}");
        assert!(out.contains("IPv4 acquisition mode"), "{out:?}");
        assert!(out.contains("--prefix-length"), "{out:?}");
        // `gateway` has no doc line: shown without a description.
        assert!(out.contains("--gateway"), "{out:?}");
        assert!(out.ends_with("eosh> net.l4.over-l2 --"), "{out:?}");
    }

    #[test]
    fn bool_flags_complete_true_false() {
        let mut ed = editor();
        ed.provide_args(
            "hello",
            ProgramArgs {
                flags: vec![FlagSpec {
                    name: "verbose".to_string(),
                    ty: "option<bool>".to_string(),
                    doc: None,
                    values: Vec::new(),
                    kind: None,
                }],
            },
        );
        type_text(&mut ed, "hello --verbose ");
        ed.take_output();
        // Nothing typed: the typed candidates list (never auto-filled).
        ed.handle(Key::Tab);
        assert_eq!(
            ed.take_output(),
            "\r\nfalse  true\r\neosh> hello --verbose "
        );
        // A typed prefix completes.
        type_text(&mut ed, "t");
        ed.take_output();
        ed.handle(Key::Tab);
        assert_eq!(ed.take_output(), "rue ");
        assert_eq!(submit(&mut ed), "hello --verbose true ");
    }

    #[test]
    fn kind_url_offers_the_canned_prefix_without_a_trailing_space() {
        let mut ed = editor();
        ed.provide_args(
            "browser",
            ProgramArgs {
                flags: vec![FlagSpec {
                    name: "url".to_string(),
                    ty: "string".to_string(),
                    doc: None,
                    values: Vec::new(),
                    kind: Some("url".to_string()),
                }],
            },
        );
        type_text(&mut ed, "browser --url ");
        ed.take_output();
        // Listed with its kind label…
        ed.handle(Key::Tab);
        let out = ed.take_output();
        assert!(out.contains("http://  url"), "{out:?}");
        // …and a typed prefix completes WITHOUT the trailing space (glue): the URL
        // continues right after.
        type_text(&mut ed, "h");
        ed.take_output();
        ed.handle(Key::Tab);
        assert_eq!(ed.take_output(), "ttp://");
        type_text(&mut ed, "x");
        assert_eq!(ed.take_output(), "x");
        assert_eq!(submit(&mut ed), "browser --url http://x");
    }

    #[test]
    fn an_unprovided_name_keeps_the_generic_argument_grammar() {
        // `hello` is in the vocabulary but no argument data was provided (unresolved /
        // still resolving): TAB after `--a` has nothing to offer — the bell, exactly
        // the pre-M3 behavior.
        let mut ed = editor();
        type_text(&mut ed, "hello --a");
        ed.take_output();
        ed.handle(Key::Tab);
        assert_eq!(ed.take_output(), "\u{7}");
    }

    #[test]
    fn provided_argument_candidates_never_arm_the_name_mark() {
        // Flag and value positions carry Flag/Value-tagged candidates once provided —
        // the name-mark oracle must ignore them: unknown flags and free-form values
        // stay green (additive, never restrictive).
        let mut ed = editor();
        ed.provide_args("net.l4.over-l2", l4_args());
        type_text(&mut ed, "net.l4.over-l2 --bogus freeform --address static9");
        let out = ed.take_output();
        assert!(!out.contains("\u{1b}[31m"), "{out:?}");
    }

    #[test]
    fn wanted_args_reports_typed_vocabulary_programs_lacking_data() {
        let mut ed = editor();
        assert!(ed.wanted_args().is_empty());
        type_text(&mut ed, "net.l4.over-l2 ");
        assert_eq!(ed.wanted_args(), vec!["net.l4.over-l2".to_string()]);
        // Bindings and unknown words are never wanted (the memo is keyed by resolved
        // program name); providing data — even empty — retires the want.
        type_text(&mut ed, "det zzz ");
        assert_eq!(ed.wanted_args(), vec!["net.l4.over-l2".to_string()]);
        ed.provide_args("net.l4.over-l2", ProgramArgs::default());
        assert!(ed.wanted_args().is_empty());
    }

    #[test]
    fn provide_args_mid_line_rearms_completion_without_visible_change() {
        // The async edge: the name resolves only after `--a` was already typed; the
        // provide re-parses (no output) and the same TAB now completes.
        let mut ed = editor();
        type_text(&mut ed, "net.l4.over-l2 --a");
        ed.take_output();
        ed.provide_args("net.l4.over-l2", l4_args());
        assert_eq!(ed.take_output(), "");
        ed.handle(Key::Tab);
        assert_eq!(ed.take_output(), "ddress ");
    }

    // -- the O(1)-backspace snapshot stack ---------------------------------------------

    /// The state restored by a backspace pop must be indistinguishable from a
    /// from-scratch reparse: same dead point, same word tracker, same stack depth,
    /// and the same parser verdicts (admissibility, completions, finishability).
    fn assert_consistent(ed: &Editor, context: &str) {
        // The stack invariant: one snapshot per green character.
        let green_end = ed.red_from.unwrap_or(ed.line.len());
        let green_chars = ed.line[..green_end]
            .iter()
            .filter(|&&byte| !(0x80..=0xbf).contains(&byte))
            .count();
        assert_eq!(
            ed.stack.len(),
            green_chars,
            "stack depth != green chars at {context} (line {:?})",
            String::from_utf8_lossy(&ed.line)
        );
        // The differential: rebuild the same line from scratch and compare.
        let mut fresh = Editor::new(
            &ed.prompt,
            ed.vocab.clone(),
            Vec::new(),
            ed.marker,
            ed.width,
        );
        fresh.line = ed.line.clone();
        fresh.rebuild();
        let line = String::from_utf8_lossy(&ed.line);
        assert_eq!(
            ed.red_from, fresh.red_from,
            "red_from at {context} ({line:?})"
        );
        assert_eq!(ed.in_word, fresh.in_word, "in_word at {context} ({line:?})");
        assert_eq!(
            ed.tracking, fresh.tracking,
            "tracking at {context} ({line:?})"
        );
        assert_eq!(
            ed.stack.len(),
            fresh.stack.len(),
            "stack depth at {context} ({line:?})"
        );
        assert_eq!(
            ed.state.admissible(),
            fresh.state.admissible(),
            "admissibility at {context} ({line:?})"
        );
        let comps = |state: &BoxP<Command>| {
            let mut out = Vec::new();
            state.completions(&mut out);
            out.sort_by(|a: &crate::inc::Completion, b| {
                (&a.word, a.matched).cmp(&(&b.word, b.matched))
            });
            out
        };
        assert_eq!(
            comps(&ed.state),
            comps(&fresh.state),
            "completions at {context} ({line:?})"
        );
        let finishes =
            |state: &BoxP<Command>| state.step(Input::Eof).and_then(Step::value).is_some();
        assert_eq!(
            finishes(&ed.state),
            finishes(&fresh.state),
            "finishability at {context} ({line:?})"
        );
    }

    #[test]
    fn backspace_pop_equals_reparse_scripted() {
        // The shapes that exercise every stack path: green typing, parse-dead and
        // name-dead marks, the red→green transition, TAB-appended bytes, recall
        // replacement, provide_args rebuild, UTF-8 multi-byte characters.
        let mut ed = editor_with_history(&["net.l4.over-l2 --address dhcp", "help x"]);
        ed.provide_args("net.l4.over-l2", l4_args());
        let script: &[Key] = &[
            Key::Char(b'n'),
            Key::Char(b'e'),
            Key::Char(b't'),
            Key::Char(b'.'),
            Key::Char(b'x'), // name-dead
            Key::Char(b'y'), // deeper red
            Key::Backspace,  // still red
            Key::Backspace,  // red→green transition
            Key::Char(b'l'),
            Key::Tab, // completes net.l4.over-l2 (unique from `net.l`)
            Key::Char(b'-'),
            Key::Char(b'-'),
            Key::Char(b'a'),
            Key::Tab, // completes --address
            Key::Char(b'd'),
            Key::Backspace,
            Key::Backspace, // erases through the TAB-appended space
            Key::Up,        // recall: help x (red repaint)
            Key::Backspace, // red→green on the recalled line
            Key::Down,      // back to the stash
            Key::Char(b' '),
            Key::Char(b'('),
            Key::Char(b'h'),
            Key::Backspace,
            Key::Backspace,
            Key::Backspace,
        ];
        for (step, &key) in script.iter().enumerate() {
            ed.handle(key);
            ed.take_output();
            assert_consistent(&ed, &format!("script step {step} ({key:?})"));
        }
        // Multi-byte characters: é assembles, then erases as one column.
        let mut ed = editor();
        for &byte in "h\u{e9}llo \u{201c}q".as_bytes() {
            ed.handle(Key::Char(byte));
        }
        assert_consistent(&ed, "utf8 typed");
        for _ in 0..8 {
            ed.handle(Key::Backspace);
            assert_consistent(&ed, "utf8 backspace");
        }
    }

    #[test]
    fn backspace_pop_equals_reparse_fuzzed() {
        // Deterministic xorshift64* (no Date/rand), the grammar tests' generator.
        struct Rng(u64);
        impl Rng {
            fn next(&mut self) -> u64 {
                let mut x = self.0;
                x ^= x >> 12;
                x ^= x << 25;
                x ^= x >> 27;
                self.0 = x;
                x.wrapping_mul(0x2545_F491_4F6C_DD1D)
            }
            fn below(&mut self, n: usize) -> usize {
                (self.next() % n as u64) as usize
            }
        }
        const BYTES: &[u8] = b"net.l4ovrhlpsvco $&()=,\"#[]-x\t";
        let mut rng = Rng(0xDEAD_BEEF_CAFE_1234);
        for round in 0..60 {
            // A third of the rounds run width-aware (a small width so wraps are
            // frequent): the differential below pins that the wrap layer is output
            // only — parser state, stack, and marks are identical with and without.
            let width = if round % 3 == 0 { Some(7) } else { None };
            let history = ["net.l4.over-l2 --address dhcp", "zzz qq"]
                .iter()
                .map(|s| s.to_string())
                .collect();
            let mut ed = Editor::new("eosh> ", vocab(), history, Marker::RED, width);
            if round % 2 == 0 {
                ed.provide_args("net.l4.over-l2", l4_args());
            }
            for step in 0..40 {
                let key = match rng.below(10) {
                    0 => Key::Backspace,
                    1 => Key::Tab,
                    2 => Key::Up,
                    3 => Key::Down,
                    _ => Key::Char(BYTES[rng.below(BYTES.len())]),
                };
                ed.handle(key);
                ed.take_output();
                assert_consistent(&ed, &format!("round {round} step {step} ({key:?})"));
            }
        }
    }

    // -- the width-aware output layer (the board-console wrap fix) ----------------------
    //
    // The fake-terminal harness: every test asserts the EXACT emitted byte sequence
    // against the module-docs contract (\r, \r\n, \b \b, CSI K, CSI A, CSI <n>G, SGR).
    // Width 8 with the 6-column prompt keeps the boundaries readable: the first row
    // holds `eosh> ` plus two characters.

    /// A width-aware editor over the same vocabulary and prompt.
    fn editor_with_width(width: usize, history: &[&str]) -> Editor {
        Editor::new(
            "eosh> ",
            vocab(),
            history.iter().map(|s| s.to_string()).collect(),
            Marker::RED,
            Some(width),
        )
    }

    #[test]
    fn typing_across_the_boundary_advances_the_row_explicitly() {
        let mut ed = editor_with_width(8, &[]);
        // `h` lands at column 7, `e` fills column 8 → explicit `\r\n`, never the
        // terminal's deferred auto-wrap.
        type_text(&mut ed, "hello");
        assert_eq!(ed.take_output(), "he\r\nllo");
        assert_eq!(submit(&mut ed), "hello");
        assert_eq!(ed.take_output(), "\r\n");
    }

    #[test]
    fn backspace_across_the_wrap_boundary_climbs_a_row() {
        let mut ed = editor_with_width(8, &[]);
        type_text(&mut ed, "hel");
        ed.take_output();
        // Erasing `l` (cursor at column 1 of row 2): plain same-row erase.
        ed.handle(Key::Backspace);
        assert_eq!(ed.take_output(), "\u{8} \u{8}");
        // Erasing `e` (cursor at column 0 of row 2, the character on the row above):
        // up, to the last column (1-based CHA), clear it — never `\b` at column 0.
        ed.handle(Key::Backspace);
        assert_eq!(ed.take_output(), "\u{1b}[A\u{1b}[8G\u{1b}[K");
        // And the next erase is back to a plain same-row one.
        ed.handle(Key::Backspace);
        assert_eq!(ed.take_output(), "\u{8} \u{8}");
        assert_eq!(submit(&mut ed), "");
    }

    #[test]
    fn recall_of_a_line_longer_than_the_width_wraps_the_repaint() {
        // `eosh> hello --name one` is 22 cells: rows `eosh> he` / `llo --na` / `me one`.
        // ↑ from the empty line: clear the one row, repaint prompt + entry wrapped.
        let mut ed = editor_with_width(8, &["hello --name one"]);
        ed.handle(Key::Up);
        assert_eq!(ed.take_output(), "\r\u{1b}[Keosh> he\r\nllo --na\r\nme one");
        assert_eq!(submit(&mut ed), "hello --name one");
    }

    #[test]
    fn recall_replacing_a_longer_line_with_a_shorter_one_clears_every_row() {
        // ↑↑ shows the 22-cell entry (3 rows), then ↓ replaces it with the 9-cell
        // `eosh> det` (2 rows): each repaint must clear every row the OLD content
        // occupied — `\b` cannot do that across rows (the board bench's bug).
        let mut ed = editor_with_width(8, &["hello --name one", "det"]);
        ed.handle(Key::Up);
        assert_eq!(ed.take_output(), "\r\u{1b}[Keosh> de\r\nt");
        ed.handle(Key::Up);
        // 9 cells span 2 rows: clear the cursor row + 1 above, repaint 3 wrapped rows.
        assert_eq!(
            ed.take_output(),
            "\r\u{1b}[K\u{1b}[A\u{1b}[Keosh> he\r\nllo --na\r\nme one"
        );
        ed.handle(Key::Down);
        // 22 cells span 3 rows (cursor on the third): clear all three, repaint 2 rows.
        assert_eq!(
            ed.take_output(),
            "\r\u{1b}[K\u{1b}[A\u{1b}[K\u{1b}[A\u{1b}[Keosh> de\r\nt"
        );
        assert_eq!(submit(&mut ed), "det");
    }

    #[test]
    fn recalling_a_red_entry_wraps_with_the_marker_interleaved() {
        // `help xx`: green `help ` + red `xx`; the SGRs are zero-width — the column
        // count runs straight through them.
        let mut ed = editor_with_width(8, &["help xx"]);
        ed.handle(Key::Up);
        assert_eq!(ed.take_output(), "\r\u{1b}[Keosh> he\r\nlp \u{1b}[31mxx");
        // Backspacing the red tail: same-row erases, then the red→green marker close.
        ed.handle(Key::Backspace);
        assert_eq!(ed.take_output(), "\u{8} \u{8}");
        ed.handle(Key::Backspace);
        assert_eq!(ed.take_output(), "\u{8} \u{8}\u{1b}[0m");
        assert_eq!(submit(&mut ed), "help ");
    }

    #[test]
    fn tab_completion_and_list_repaint_wrap_too() {
        // Width 10: `eosh> ti` + TAB appends `me.f`, whose `e` fills column 10.
        let mut ed = Editor::new("eosh> ", vocab(), Vec::new(), Marker::RED, Some(10));
        type_text(&mut ed, "ti");
        ed.take_output();
        ed.handle(Key::Tab);
        assert_eq!(ed.take_output(), "me\r\n.f");
        // No further progress: the list, then the wrapped prompt+line repaint
        // (`eosh> time` fills the first row exactly).
        ed.handle(Key::Tab);
        assert_eq!(
            ed.take_output(),
            "\r\ntime.frozen  time.fuzzy\r\neosh> time\r\n.f"
        );
        assert_eq!(submit(&mut ed), "time.f");
    }

    #[test]
    fn width_none_preserves_the_exact_historical_bytes() {
        // The regression pin: a width-less editor (every transport that cannot say —
        // the browser line transport, usermode today) emits byte-for-byte the
        // pre-width stream: no CSI outside the SGR marker, `\b \b` only.
        let mut ed = editor_with_history(&["hello --name one"]);
        // ↑ on the empty line: no erases, just the entry text.
        ed.handle(Key::Up);
        assert_eq!(ed.take_output(), "hello --name one");
        // ↓ back to the (empty) fresh line: one `\b \b` per character, nothing else.
        ed.handle(Key::Down);
        assert_eq!(ed.take_output(), "\u{8} \u{8}".repeat(16));
        // Typing past any width and the backspace stay plain.
        type_text(&mut ed, "hello --name eo9");
        assert_eq!(ed.take_output(), "hello --name eo9");
        ed.handle(Key::Backspace);
        assert_eq!(ed.take_output(), "\u{8} \u{8}");
    }

    #[test]
    fn inverse_marker_swaps_the_sequences() {
        let mut ed = Editor::new("eosh> ", vocab(), vec![], Marker::INVERSE, None);
        type_text(&mut ed, "help x");
        assert_eq!(ed.take_output(), "help \u{1b}[7mx");
        ed.handle(Key::Backspace);
        assert_eq!(ed.take_output(), "\u{8} \u{8}\u{1b}[27m");
    }
}
