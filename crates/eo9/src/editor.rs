//! A small line editor for the interactive shell: emacs-style editing, in-memory
//! history recall, and tab completion (candidates from `complete.rs`).
//!
//! The editor is written against generic `Read`/`Write` streams so its behaviour can be
//! unit-tested by feeding byte scripts; the raw-mode terminal plumbing that wires it to
//! the real stdin/stdout lives in `interactive.rs`. It implements only what a shell
//! needs — insertion, deletion, cursor movement, history, completion — and ignores
//! anything it does not understand (unknown control bytes and escape sequences) rather
//! than echoing it.
//!
//! Completion behaviour is the usual readline shape: a unique candidate completes the
//! word (directories keep their trailing `/` so the user can continue into them), an
//! ambiguous one first extends to the longest common prefix, and a tab that cannot make
//! progress lists the alternatives and repaints the line.

use std::io::{self, Read, Write};

use crate::complete::{Completion, ShellCompleter, longest_common_prefix};

/// Edit one line. `prompt` is whatever is already sitting at the start of the screen
/// line (the editor repaints it on every change); `history` is read-only here — the
/// caller decides what to add to it. Returns `Ok(None)` at end of input (Ctrl-D on an
/// empty line, or the stream closing with nothing typed).
pub fn edit_line(
    input: &mut dyn Read,
    output: &mut dyn Write,
    prompt: &str,
    history: &[String],
    completer: &ShellCompleter,
) -> io::Result<Option<String>> {
    let mut editor = Editor {
        output,
        prompt,
        buffer: Vec::new(),
        cursor: 0,
        history,
        history_pos: history.len(),
        saved: Vec::new(),
        completer,
    };
    editor.run(input)
}

struct Editor<'a> {
    output: &'a mut dyn Write,
    prompt: &'a str,
    /// The line being edited (chars, so editing positions are simple).
    buffer: Vec<char>,
    /// Cursor position in `buffer` (0..=len).
    cursor: usize,
    history: &'a [String],
    /// Position in `history`; `history.len()` means "the line being typed".
    history_pos: usize,
    /// The in-progress line, saved while browsing history.
    saved: Vec<char>,
    completer: &'a ShellCompleter,
}

impl Editor<'_> {
    fn run(&mut self, input: &mut dyn Read) -> io::Result<Option<String>> {
        self.redraw()?;
        loop {
            let Some(byte) = read_byte(input)? else {
                // The input closed mid-line: hand back what was typed, or signal end of
                // input if there was nothing.
                self.finish_line()?;
                return Ok(if self.buffer.is_empty() {
                    None
                } else {
                    Some(self.line())
                });
            };
            match byte {
                b'\r' | b'\n' => {
                    self.finish_line()?;
                    return Ok(Some(self.line()));
                }
                // Ctrl-C: drop the line; the shell will print a fresh prompt.
                0x03 => {
                    self.output.write_all(b"^C\r\n")?;
                    self.output.flush()?;
                    return Ok(Some(String::new()));
                }
                // Ctrl-D: end of input on an empty line, delete-at-cursor otherwise.
                0x04 => {
                    if self.buffer.is_empty() {
                        self.finish_line()?;
                        return Ok(None);
                    }
                    if self.cursor < self.buffer.len() {
                        self.buffer.remove(self.cursor);
                        self.redraw()?;
                    }
                }
                // Backspace.
                0x7f | 0x08 => {
                    if self.cursor > 0 {
                        self.cursor -= 1;
                        self.buffer.remove(self.cursor);
                        self.redraw()?;
                    }
                }
                b'\t' => self.complete()?,
                // Ctrl-A / Ctrl-E: start / end of line.
                0x01 => {
                    self.cursor = 0;
                    self.redraw()?;
                }
                0x05 => {
                    self.cursor = self.buffer.len();
                    self.redraw()?;
                }
                // Ctrl-B / Ctrl-F: one character left / right.
                0x02 => {
                    if self.cursor > 0 {
                        self.cursor -= 1;
                        self.redraw()?;
                    }
                }
                0x06 => {
                    if self.cursor < self.buffer.len() {
                        self.cursor += 1;
                        self.redraw()?;
                    }
                }
                // Ctrl-K / Ctrl-U: kill to end / to start.
                0x0b => {
                    self.buffer.truncate(self.cursor);
                    self.redraw()?;
                }
                0x15 => {
                    self.buffer.drain(..self.cursor);
                    self.cursor = 0;
                    self.redraw()?;
                }
                // Ctrl-W: delete the word before the cursor.
                0x17 => self.delete_word_before_cursor()?,
                // Ctrl-L: clear the screen and repaint the line.
                0x0c => {
                    self.output.write_all(b"\x1b[2J\x1b[H")?;
                    self.redraw()?;
                }
                0x1b => self.escape_sequence(input)?,
                byte if byte >= 0x20 => {
                    if let Some(c) = read_utf8(byte, input)? {
                        self.buffer.insert(self.cursor, c);
                        self.cursor += 1;
                        self.redraw()?;
                    }
                }
                // Other control bytes: ignore.
                _ => {}
            }
        }
    }

    /// Tab: complete the word under the cursor.
    fn complete(&mut self) -> io::Result<()> {
        let line = self.line();
        let byte_cursor: usize = self.buffer[..self.cursor]
            .iter()
            .map(|c| c.len_utf8())
            .sum();
        let Completion { start, candidates } = self.completer.complete(&line, byte_cursor);
        if candidates.is_empty() {
            self.output.write_all(b"\x07")?; // bell
            self.output.flush()?;
            return Ok(());
        }
        let word_chars = line[start..byte_cursor].chars().count();
        let word_start = self.cursor - word_chars;

        if candidates.len() == 1 {
            let mut replacement = candidates[0].clone();
            if !replacement.ends_with('/') {
                replacement.push(' ');
            }
            self.replace_word(word_start, &replacement);
            return self.redraw();
        }

        let prefix = longest_common_prefix(&candidates);
        if prefix.chars().count() > word_chars {
            self.replace_word(word_start, &prefix);
            return self.redraw();
        }

        // No further progress: list the alternatives, then repaint the line.
        self.output.write_all(b"\r\n")?;
        self.output.write_all(candidates.join("  ").as_bytes())?;
        self.output.write_all(b"\r\n")?;
        self.redraw()
    }

    fn replace_word(&mut self, word_start: usize, replacement: &str) {
        self.buffer
            .splice(word_start..self.cursor, replacement.chars());
        self.cursor = word_start + replacement.chars().count();
    }

    fn delete_word_before_cursor(&mut self) -> io::Result<()> {
        let mut start = self.cursor;
        while start > 0 && self.buffer[start - 1].is_whitespace() {
            start -= 1;
        }
        while start > 0 && !self.buffer[start - 1].is_whitespace() {
            start -= 1;
        }
        self.buffer.drain(start..self.cursor);
        self.cursor = start;
        self.redraw()
    }

    /// An escape sequence: arrows, home/end, delete. Anything else is swallowed.
    fn escape_sequence(&mut self, input: &mut dyn Read) -> io::Result<()> {
        let Some(next) = read_byte(input)? else {
            return Ok(());
        };
        if next != b'[' && next != b'O' {
            return Ok(());
        }
        let mut params = Vec::new();
        loop {
            let Some(byte) = read_byte(input)? else {
                return Ok(());
            };
            if (0x40..=0x7e).contains(&byte) {
                return self.dispatch_escape(byte, &params);
            }
            params.push(byte);
            if params.len() > 16 {
                // A runaway sequence; give up quietly.
                return Ok(());
            }
        }
    }

    fn dispatch_escape(&mut self, final_byte: u8, params: &[u8]) -> io::Result<()> {
        match final_byte {
            b'A' => self.history_step(true),
            b'B' => self.history_step(false),
            b'C' => {
                if self.cursor < self.buffer.len() {
                    self.cursor += 1;
                }
                self.redraw()
            }
            b'D' => {
                if self.cursor > 0 {
                    self.cursor -= 1;
                }
                self.redraw()
            }
            b'H' => {
                self.cursor = 0;
                self.redraw()
            }
            b'F' => {
                self.cursor = self.buffer.len();
                self.redraw()
            }
            b'~' => match params {
                b"1" | b"7" => {
                    self.cursor = 0;
                    self.redraw()
                }
                b"4" | b"8" => {
                    self.cursor = self.buffer.len();
                    self.redraw()
                }
                b"3" => {
                    if self.cursor < self.buffer.len() {
                        self.buffer.remove(self.cursor);
                    }
                    self.redraw()
                }
                _ => Ok(()),
            },
            _ => Ok(()),
        }
    }

    /// ↑ (`back == true`) and ↓ through the history; the in-progress line is kept and
    /// restored when stepping past the newest entry.
    fn history_step(&mut self, back: bool) -> io::Result<()> {
        if self.history.is_empty() {
            return Ok(());
        }
        if back {
            if self.history_pos == 0 {
                return Ok(());
            }
            if self.history_pos == self.history.len() {
                self.saved = self.buffer.clone();
            }
            self.history_pos -= 1;
            self.buffer = self.history[self.history_pos].chars().collect();
        } else {
            if self.history_pos >= self.history.len() {
                return Ok(());
            }
            self.history_pos += 1;
            self.buffer = if self.history_pos == self.history.len() {
                self.saved.clone()
            } else {
                self.history[self.history_pos].chars().collect()
            };
        }
        self.cursor = self.buffer.len();
        self.redraw()
    }

    /// Repaint the line: column 0, clear, prompt, buffer, cursor back where it belongs.
    fn redraw(&mut self) -> io::Result<()> {
        self.output.write_all(b"\r\x1b[2K")?;
        self.output.write_all(self.prompt.as_bytes())?;
        self.output.write_all(self.line().as_bytes())?;
        let behind = self.buffer.len() - self.cursor;
        if behind > 0 {
            self.output.write_all(format!("\x1b[{behind}D").as_bytes())?;
        }
        self.output.flush()
    }

    fn finish_line(&mut self) -> io::Result<()> {
        self.output.write_all(b"\r\n")?;
        self.output.flush()
    }

    fn line(&self) -> String {
        self.buffer.iter().collect()
    }
}

/// Read one byte; `None` at end of input.
fn read_byte(input: &mut dyn Read) -> io::Result<Option<u8>> {
    let mut byte = [0u8; 1];
    loop {
        match input.read(&mut byte) {
            Ok(0) => return Ok(None),
            Ok(_) => return Ok(Some(byte[0])),
            Err(err) if err.kind() == io::ErrorKind::Interrupted => continue,
            Err(err) => return Err(err),
        }
    }
}

/// Finish reading one UTF-8 scalar whose first byte is `first`. Invalid sequences are
/// dropped (`None`) rather than inserted.
fn read_utf8(first: u8, input: &mut dyn Read) -> io::Result<Option<char>> {
    let extra = match first {
        0x00..=0x7f => 0,
        0xc0..=0xdf => 1,
        0xe0..=0xef => 2,
        0xf0..=0xf7 => 3,
        _ => return Ok(None),
    };
    let mut bytes = vec![first];
    for _ in 0..extra {
        match read_byte(input)? {
            Some(byte) => bytes.push(byte),
            None => return Ok(None),
        }
    }
    Ok(std::str::from_utf8(&bytes)
        .ok()
        .and_then(|text| text.chars().next()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    fn completer() -> ShellCompleter {
        ShellCompleter::new(
            vec![
                "hello".to_string(),
                "outcomes".to_string(),
                "time.frozen".to_string(),
                "time.fuzzy".to_string(),
            ],
            None,
        )
    }

    /// Drive the editor with a byte script; returns the line and everything written to
    /// the output stream.
    fn edit(script: &[u8], history: &[String]) -> (Option<String>, String) {
        let mut input = Cursor::new(script.to_vec());
        let mut output = Vec::new();
        let result = edit_line(&mut input, &mut output, "eosh> ", history, &completer())
            .expect("editing never fails on in-memory streams");
        (result, String::from_utf8_lossy(&output).into_owned())
    }

    #[test]
    fn plain_typing_returns_the_line() {
        let (line, output) = edit(b"hello --name eo9\r", &[]);
        assert_eq!(line.as_deref(), Some("hello --name eo9"));
        assert!(output.contains("eosh> hello --name eo9"));
    }

    #[test]
    fn backspace_and_cursor_movement_edit_in_place() {
        // Type "helx", erase the x, finish the word.
        let (line, _) = edit(b"helx\x7flo\r", &[]);
        assert_eq!(line.as_deref(), Some("hello"));
        // Type "ac", go left once, insert "b".
        let (line, _) = edit(b"ac\x1b[Db\r", &[]);
        assert_eq!(line.as_deref(), Some("abc"));
        // Ctrl-A then Ctrl-K kills the whole line.
        let (line, _) = edit(b"junk\x01\x0bok\r", &[]);
        assert_eq!(line.as_deref(), Some("ok"));
    }

    #[test]
    fn end_of_input_and_control_characters() {
        // Ctrl-D on an empty line is end of input.
        let (line, _) = edit(b"\x04", &[]);
        assert_eq!(line, None);
        // Ctrl-C cancels the line but keeps the shell going.
        let (line, output) = edit(b"oops\x03", &[]);
        assert_eq!(line.as_deref(), Some(""));
        assert!(output.contains("^C"));
        // The stream closing mid-line hands back what was typed.
        let (line, _) = edit(b"partial", &[]);
        assert_eq!(line.as_deref(), Some("partial"));
    }

    #[test]
    fn unique_completion_fills_the_word() {
        // "hell" matches only the name `hello` (a bare "hel" would also match the
        // builtin `help`).
        let (line, _) = edit(b"hell\t\r", &[]);
        assert_eq!(line.as_deref(), Some("hello "));
        // Completion mid-command works on the word under the cursor only.
        let (line, _) = edit(b"time.frozen $ outc\t\r", &[]);
        assert_eq!(line.as_deref(), Some("time.frozen $ outcomes "));
    }

    #[test]
    fn ambiguous_completion_extends_then_lists() {
        // First tab extends to the longest common prefix...
        let (line, output) = edit(b"ti\t\r", &[]);
        assert_eq!(line.as_deref(), Some("time.f"));
        assert!(!output.contains("time.frozen  time.fuzzy"));
        // ...the next tab lists the alternatives and keeps the line.
        let (line, output) = edit(b"ti\t\t\r", &[]);
        assert_eq!(line.as_deref(), Some("time.f"));
        assert!(output.contains("time.frozen  time.fuzzy"), "{output}");
    }

    #[test]
    fn completion_with_no_candidates_rings_the_bell() {
        let (line, output) = edit(b"zzz\t\r", &[]);
        assert_eq!(line.as_deref(), Some("zzz"));
        assert!(output.contains('\u{7}'));
    }

    #[test]
    fn history_recall_steps_back_and_forward() {
        let history = vec!["hello --name one".to_string(), "outcomes".to_string()];
        // Up once: the newest entry.
        let (line, _) = edit(b"\x1b[A\r", &history);
        assert_eq!(line.as_deref(), Some("outcomes"));
        // Up twice: the older entry.
        let (line, _) = edit(b"\x1b[A\x1b[A\r", &history);
        assert_eq!(line.as_deref(), Some("hello --name one"));
        // Up then down restores the line that was being typed.
        let (line, _) = edit(b"dra\x1b[A\x1b[B ft\r", &history);
        assert_eq!(line.as_deref(), Some("dra ft"));
    }
}
