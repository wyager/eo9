//! The interactive text provider for `eo9 shell`: the same `eo9:text` capability the
//! plain stdio provider grants, except that `read-line` on a real terminal goes through
//! the line editor (`editor.rs`: history and tab completion) instead of a bare blocking
//! read.
//!
//! Used only when the shell is interactive — no `-c` command and both stdin and stdout
//! are terminals. Piped sessions, one-shot commands, and every child program keep the
//! plain provider, so scripted transcripts behave exactly as before. This changes how
//! the shell's input line is *typed*, not what the shell or its children may do.

use std::io::{self, BufRead, Write};
use std::sync::{Arc, Mutex};

use eo9_runtime::providers::BoxOp;
use eo9_runtime::{OutputStream, TextError, TextProvider};

use crate::complete::ShellCompleter;
use crate::editor;
use crate::providers::oneshot;

/// `eo9:text` for the interactive shell session.
pub struct InteractiveText {
    completer: Arc<ShellCompleter>,
    shared: Arc<Mutex<Shared>>,
}

struct Shared {
    /// Whatever was last written to stdout after its final newline — visually, the
    /// prompt the cursor is sitting after — used to repaint the line while editing.
    pending_prompt: String,
    /// Lines entered this session (oldest first), for ↑/↓ recall.
    history: Vec<String>,
}

impl InteractiveText {
    pub fn new(completer: ShellCompleter) -> Self {
        InteractiveText {
            completer: Arc::new(completer),
            shared: Arc::new(Mutex::new(Shared {
                pending_prompt: String::new(),
                history: Vec::new(),
            })),
        }
    }
}

impl TextProvider for InteractiveText {
    fn write(&mut self, to: OutputStream, text: &str) -> Result<(), TextError> {
        let result = match to {
            OutputStream::Out => {
                // Track the trailing partial line so the editor knows what to repaint.
                let mut shared = self
                    .shared
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
                match text.rfind('\n') {
                    Some(newline) => {
                        shared.pending_prompt.clear();
                        shared.pending_prompt.push_str(&text[newline + 1..]);
                    }
                    None => shared.pending_prompt.push_str(text),
                }
                let mut out = io::stdout();
                out.write_all(text.as_bytes()).and_then(|()| out.flush())
            }
            OutputStream::Err => {
                let mut err = io::stderr();
                err.write_all(text.as_bytes()).and_then(|()| err.flush())
            }
        };
        result.map_err(|err| TextError::Io(err.to_string()))
    }

    fn read_line(&mut self) -> BoxOp<Result<Option<String>, TextError>> {
        let (op, complete) = oneshot();
        // The completion closure must fire exactly once; park it where both the editor
        // thread and the spawn-failure path below can take it.
        let complete = Arc::new(Mutex::new(Some(complete)));
        let complete_in_thread = Arc::clone(&complete);
        let shared = Arc::clone(&self.shared);
        let completer = Arc::clone(&self.completer);

        let spawned = std::thread::Builder::new()
            .name("eo9-shell-editor".to_string())
            .spawn(move || {
                let (prompt, history) = {
                    let shared = shared
                        .lock()
                        .unwrap_or_else(|poisoned| poisoned.into_inner());
                    (shared.pending_prompt.clone(), shared.history.clone())
                };
                let result = read_one_line(&prompt, &history, &completer);
                if let Ok(Some(line)) = &result {
                    let trimmed = line.trim();
                    if !trimmed.is_empty() {
                        let mut shared = shared
                            .lock()
                            .unwrap_or_else(|poisoned| poisoned.into_inner());
                        if shared.history.last().map(String::as_str) != Some(trimmed) {
                            shared.history.push(trimmed.to_string());
                        }
                    }
                }
                if let Some(complete) = complete_in_thread
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .take()
                {
                    complete(result.map_err(|err| TextError::Io(err.to_string())));
                }
            });

        if spawned.is_err() {
            // The editor thread could not start at all; fail the read instead of leaving
            // the shell waiting forever.
            if let Some(complete) = complete
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .take()
            {
                complete(Err(TextError::Io(
                    "cannot start the line-editor thread".to_string(),
                )));
            }
        }
        op
    }
}

/// Read one line from the real terminal: raw mode plus the line editor. If the terminal
/// cannot be put into raw mode after all, fall back to a plain buffered read so the
/// shell still works (just without editing).
fn read_one_line(
    prompt: &str,
    history: &[String],
    completer: &ShellCompleter,
) -> io::Result<Option<String>> {
    match RawMode::enable() {
        Ok(_guard) => {
            let mut input = io::stdin();
            let mut output = io::stdout();
            editor::edit_line(&mut input, &mut output, prompt, history, completer)
        }
        Err(_) => {
            let mut line = String::new();
            match io::stdin().lock().read_line(&mut line)? {
                0 => Ok(None),
                _ => {
                    while line.ends_with('\n') || line.ends_with('\r') {
                        line.pop();
                    }
                    Ok(Some(line))
                }
            }
        }
    }
}

/// Puts the controlling terminal (stdin) into the editor's raw mode — no echo, no
/// canonical line buffering, no signal characters (the editor handles ^C/^D itself) —
/// and restores the previous settings on drop, so the terminal is sane again however
/// the edit ends.
struct RawMode {
    original: libc::termios,
}

impl RawMode {
    fn enable() -> io::Result<RawMode> {
        // SAFETY: plain libc calls on the process's own stdin descriptor; the termios
        // structs live on the stack and nothing outlives the calls.
        unsafe {
            let mut original: libc::termios = std::mem::zeroed();
            if libc::tcgetattr(libc::STDIN_FILENO, &mut original) != 0 {
                return Err(io::Error::last_os_error());
            }
            let mut attrs = original;
            attrs.c_lflag &= !(libc::ICANON | libc::ECHO | libc::ISIG);
            attrs.c_iflag &= !(libc::IXON | libc::ICRNL);
            attrs.c_cc[libc::VMIN] = 1;
            attrs.c_cc[libc::VTIME] = 0;
            if libc::tcsetattr(libc::STDIN_FILENO, libc::TCSANOW, &attrs) != 0 {
                return Err(io::Error::last_os_error());
            }
            Ok(RawMode { original })
        }
    }
}

impl Drop for RawMode {
    fn drop(&mut self) {
        // SAFETY: restores the attributes saved by `enable`; a failure here changes
        // nothing about memory safety, the terminal just stays raw.
        unsafe {
            let _ = libc::tcsetattr(libc::STDIN_FILENO, libc::TCSANOW, &self.original);
        }
    }
}
