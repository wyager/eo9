//! Root provider for `eo9:text` — standard text streams backed by std{in,out,err}.
//!
//! `write` is synchronous (matching the WIT signature); `read-line` and `read-key` are
//! potentially blocking and complete asynchronously through a [`Completer`].
//!
//! The provider owns a dedicated, detached reader thread rather than using the shared
//! blocking pool: a read on an interactive stdin can block indefinitely, and it must not
//! be able to wedge a pool that fs/disk operations depend on, nor delay provider
//! shutdown.
//!
//! **Per-key input** (`read-key`): only the [`TextProvider::stdio_interactive`]
//! constructor supports it — its reader thread owns the real stdin descriptor, puts the
//! terminal into raw mode (no echo, no canonical buffering, no signal characters) and
//! decodes escape sequences into semantic [`KeyEvent`]s. Raw mode is entered on the
//! first `read-key`, spans consecutive key reads, and is restored to the saved settings
//! whenever a key ends the edit (Enter, Ctrl-C, end of input) or a `read-line` arrives —
//! so child programs reading lines between prompts see a cooked terminal, the same
//! scope the retired host-side line editor gave it. The stream-based constructors
//! answer `read-key` with [`TextError::Unsupported`] (the typed "fall back to
//! read-line" signal): a line-buffered pipe cannot deliver keystrokes.
//!
//! Kill behavior: an in-flight `read-line`/`read-key` runs until the underlying
//! blocking read returns; the consumed input is handed to the completer, and if the
//! issuing task is dead the runtime drops it — the input is lost, not pushed back.
//! `write` never spans a kill (it is synchronous).

use std::io::{self, BufRead, BufReader, Write};
use std::sync::Mutex;
use std::sync::mpsc::{self, Sender};
use std::thread;

use crate::completion::Completer;

/// Which output stream to write to (WIT `output-stream`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OutputStream {
    /// Standard output.
    Out,
    /// Standard error.
    Err,
}

/// Errors reported by the text API (WIT `text-error`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TextError {
    /// The stream is closed (e.g. output was detached).
    Closed,
    /// The operation is not supported by this transport (`read-key` on a line-only
    /// stream). A typed refusal so consumers can fall back to `read-line`.
    Unsupported,
    /// Any other host I/O failure.
    Io(String),
}

/// One decoded keystroke (WIT `key`): the provider owns the escape-sequence decoding
/// and hands the consumer semantics. `Char` is one byte of printable input (multi-byte
/// UTF-8 arrives byte by byte); `Ctrl` carries the raw control byte (3 = Ctrl-C,
/// 4 = Ctrl-D); `Eof` is reserved for transports that decode an explicit end-of-input
/// keystroke (this provider reports end of input as `Ok(None)` instead, mirroring
/// `read-line`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeyEvent {
    Char(u8),
    Enter,
    Backspace,
    Tab,
    Up,
    Down,
    Left,
    Right,
    Ctrl(u8),
    Eof,
}

/// Completion payload of `read-line`: `Ok(None)` at end of input.
pub type ReadLineCompletion = Result<Option<String>, TextError>;

/// Completion payload of `read-key`: `Ok(None)` at end of input,
/// `Err(TextError::Unsupported)` from transports that cannot deliver keystrokes.
pub type ReadKeyCompletion = Result<Option<KeyEvent>, TextError>;

/// The host trait mirroring the WIT `eo9:text/text` interface (minus `default`, which is
/// the runtime's resource-table business).
pub trait TextHost: Send + Sync {
    /// Write UTF-8 text to stdout or stderr. Synchronous: the data has been handed to
    /// (and flushed into) the host stream when this returns.
    fn write(&self, to: OutputStream, text: &str) -> Result<(), TextError>;

    /// Read one line from stdin, without the trailing newline; completes with
    /// `Ok(None)` at end of input.
    fn read_line(&self, complete: Completer<ReadLineCompletion>);

    /// Read one decoded keystroke (no echo — the consumer owns the line image);
    /// completes with `Ok(None)` at end of input. The default answers
    /// [`TextError::Unsupported`], the contract for line-only transports.
    fn read_key(&self, complete: Completer<ReadKeyCompletion>) {
        complete(Err(TextError::Unsupported));
    }
}

/// One request for the reader thread.
enum Request {
    Line(Completer<ReadLineCompletion>),
    Key(Completer<ReadKeyCompletion>),
}

/// The unix text provider. Corresponds to the WIT `text-impl` root handle.
pub struct TextProvider {
    out: Mutex<Box<dyn Write + Send>>,
    err: Mutex<Box<dyn Write + Send>>,
    /// Requests for the dedicated reader thread. Dropping the sender lets the thread
    /// exit once it has served everything it already accepted.
    reader: Sender<Request>,
}

impl TextProvider {
    /// A provider wired to the process's real standard streams, line-only: `read-key`
    /// answers `Unsupported`. The right provider for children, pipes, and one-shot
    /// runs — transcripts behave exactly as before per-key input existed.
    pub fn stdio() -> Self {
        Self::from_streams(io::stdout(), io::stderr(), BufReader::new(io::stdin()))
    }

    /// A provider wired to the process's real standard streams *with per-key support*:
    /// `read-key` puts the controlling terminal into raw mode (restored at every edit
    /// boundary — see the module docs) and serves decoded keystrokes. Use only when
    /// stdin/stdout are the interactive terminal; if raw mode cannot be entered after
    /// all (not a tty), `read-key` answers `Unsupported` and the consumer falls back
    /// to `read-line`.
    pub fn stdio_interactive() -> Self {
        let (sender, receiver) = mpsc::channel::<Request>();
        // Detached like the stream reader; additionally restores the terminal's cooked
        // settings when the request channel closes (provider drop).
        thread::Builder::new()
            .name("eo9-text-stdin".to_owned())
            .spawn(move || interactive_reader_loop(&receiver))
            .expect("failed to spawn text reader thread");
        Self {
            out: Mutex::new(Box::new(io::stdout())),
            err: Mutex::new(Box::new(io::stderr())),
            reader: sender,
        }
    }

    /// A provider over arbitrary streams (used by tests, and by hosts that want to
    /// redirect the program's text I/O). Line-only: `read-key` answers `Unsupported`.
    pub fn from_streams(
        out: impl Write + Send + 'static,
        err: impl Write + Send + 'static,
        input: impl BufRead + Send + 'static,
    ) -> Self {
        let (sender, receiver) = mpsc::channel::<Request>();
        // Detached on purpose: if the input is an interactive terminal the thread may sit
        // in a blocking read long after the provider is gone; it exits once the request
        // channel is closed *and* its current read returns.
        thread::Builder::new()
            .name("eo9-text-stdin".to_owned())
            .spawn(move || reader_loop(input, &receiver))
            .expect("failed to spawn text reader thread");
        Self {
            out: Mutex::new(Box::new(out)),
            err: Mutex::new(Box::new(err)),
            reader: sender,
        }
    }
}

impl TextHost for TextProvider {
    fn write(&self, to: OutputStream, text: &str) -> Result<(), TextError> {
        let sink = match to {
            OutputStream::Out => &self.out,
            OutputStream::Err => &self.err,
        };
        let mut sink = sink.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        sink.write_all(text.as_bytes()).map_err(io_to_text)?;
        sink.flush().map_err(io_to_text)
    }

    fn read_line(&self, complete: Completer<ReadLineCompletion>) {
        if let Err(mpsc::SendError(Request::Line(complete))) =
            self.reader.send(Request::Line(complete))
        {
            // The reader thread is gone (it cannot outlive the provider unless it was
            // never spawned); report the stream as closed rather than losing the op.
            complete(Err(TextError::Closed));
        }
    }

    fn read_key(&self, complete: Completer<ReadKeyCompletion>) {
        if let Err(mpsc::SendError(Request::Key(complete))) =
            self.reader.send(Request::Key(complete))
        {
            complete(Err(TextError::Closed));
        }
    }
}

fn reader_loop(mut input: impl BufRead, requests: &mpsc::Receiver<Request>) {
    while let Ok(request) = requests.recv() {
        let complete = match request {
            Request::Line(complete) => complete,
            Request::Key(complete) => {
                // Stream-backed input is line-buffered by nature; the typed refusal
                // tells the consumer to use read-line.
                complete(Err(TextError::Unsupported));
                continue;
            }
        };
        let mut line = String::new();
        let completion = match input.read_line(&mut line) {
            Ok(0) => Ok(None),
            Ok(_) => {
                if line.ends_with('\n') {
                    line.pop();
                    if line.ends_with('\r') {
                        line.pop();
                    }
                }
                Ok(Some(line))
            }
            Err(err) => Err(io_to_text(err)),
        };
        complete(completion);
    }
}

// ------------------------------------------------------------------------------------
// The interactive (raw-mode) reader: real stdin, termios, escape decoding
// ------------------------------------------------------------------------------------

/// The interactive reader thread: owns the stdin descriptor and the terminal mode.
/// Line requests read in cooked mode (the tty's own line discipline does the editing);
/// key requests read raw, one decoded keystroke at a time. Raw mode spans consecutive
/// key requests and is restored at edit boundaries (Enter/Ctrl-C/end of input) and
/// before any line request, so programs the shell runs between prompts see a cooked
/// terminal.
fn interactive_reader_loop(requests: &mpsc::Receiver<Request>) {
    let mut terminal = TerminalMode::default();
    let mut decoder = KeyDecoder::default();
    while let Ok(request) = requests.recv() {
        match request {
            Request::Line(complete) => {
                terminal.ensure_cooked();
                complete(read_cooked_line());
            }
            Request::Key(complete) => {
                if terminal.ensure_raw().is_err() {
                    // Not a terminal after all (or termios refused): the typed
                    // refusal, every time — the consumer probes once and falls back.
                    complete(Err(TextError::Unsupported));
                    continue;
                }
                let result = read_one_key(&mut decoder);
                // Edit boundaries: the consumer is about to run a command (Enter,
                // Ctrl-C) or the session is over — give the terminal back cooked.
                if matches!(
                    result,
                    Ok(None) | Ok(Some(KeyEvent::Enter)) | Ok(Some(KeyEvent::Ctrl(3))) | Err(_)
                ) {
                    terminal.ensure_cooked();
                }
                complete(result);
            }
        }
    }
    // Provider dropped: leave the terminal as we found it.
    terminal.ensure_cooked();
}

/// Read one line from the stdin descriptor in cooked mode (the tty's line discipline
/// hands the whole line over after Enter; byte-wise reads keep no userspace buffer
/// that a later raw read could lose).
fn read_cooked_line() -> ReadLineCompletion {
    let mut line: Vec<u8> = Vec::new();
    loop {
        match read_stdin_byte() {
            Ok(None) => {
                if line.is_empty() {
                    return Ok(None);
                }
                break;
            }
            Ok(Some(b'\n')) => break,
            Ok(Some(byte)) => line.push(byte),
            Err(err) => return Err(io_to_text(err)),
        }
    }
    if line.last() == Some(&b'\r') {
        line.pop();
    }
    Ok(Some(String::from_utf8_lossy(&line).into_owned()))
}

/// Read raw bytes until the decoder produces one keystroke; `Ok(None)` at end of input.
fn read_one_key(decoder: &mut KeyDecoder) -> ReadKeyCompletion {
    loop {
        match read_stdin_byte() {
            Ok(None) => return Ok(None),
            Ok(Some(byte)) => {
                if let Some(event) = decoder.push(byte) {
                    return Ok(Some(event));
                }
            }
            Err(err) => return Err(io_to_text(err)),
        }
    }
}

/// One blocking byte from the real stdin descriptor; `None` at end of input.
fn read_stdin_byte() -> io::Result<Option<u8>> {
    let mut byte = [0u8; 1];
    loop {
        // SAFETY: a plain read(2) on the process's own stdin into a stack buffer.
        let n = unsafe { libc::read(libc::STDIN_FILENO, byte.as_mut_ptr().cast(), 1) };
        match n {
            0 => return Ok(None),
            1 => return Ok(Some(byte[0])),
            _ => {
                let err = io::Error::last_os_error();
                if err.kind() == io::ErrorKind::Interrupted {
                    continue;
                }
                return Err(err);
            }
        }
    }
}

/// The terminal-mode keeper: saves the cooked settings once, toggles raw on demand,
/// always restores what it saved.
#[derive(Default)]
struct TerminalMode {
    saved: Option<libc::termios>,
    raw: bool,
}

impl TerminalMode {
    /// Enter raw mode (no echo, no canonical buffering, no signal characters, no flow
    /// control, CR not translated — the attribute set the retired host editor used).
    /// No-op if already raw.
    fn ensure_raw(&mut self) -> io::Result<()> {
        if self.raw {
            return Ok(());
        }
        // SAFETY: plain libc calls on the process's own stdin descriptor; the termios
        // structs live on the stack/in self and nothing outlives the calls.
        unsafe {
            let mut original: libc::termios = std::mem::zeroed();
            if libc::tcgetattr(libc::STDIN_FILENO, &mut original) != 0 {
                return Err(io::Error::last_os_error());
            }
            if self.saved.is_none() {
                self.saved = Some(original);
            }
            let mut attrs = original;
            attrs.c_lflag &= !(libc::ICANON | libc::ECHO | libc::ISIG);
            attrs.c_iflag &= !(libc::IXON | libc::ICRNL);
            attrs.c_cc[libc::VMIN] = 1;
            attrs.c_cc[libc::VTIME] = 0;
            if libc::tcsetattr(libc::STDIN_FILENO, libc::TCSANOW, &attrs) != 0 {
                return Err(io::Error::last_os_error());
            }
        }
        self.raw = true;
        Ok(())
    }

    /// Restore the saved cooked settings. No-op if not raw.
    fn ensure_cooked(&mut self) {
        if !self.raw {
            return;
        }
        if let Some(saved) = &self.saved {
            // SAFETY: restores the attributes saved by `ensure_raw`; a failure here
            // changes nothing about memory safety, the terminal just stays raw.
            unsafe {
                let _ = libc::tcsetattr(libc::STDIN_FILENO, libc::TCSANOW, saved);
            }
        }
        self.raw = false;
    }
}

/// Escape-sequence decoder: raw terminal bytes in, semantic [`KeyEvent`]s out. The
/// state machine is shared in spirit with the kernel console's
/// (`kernel/eo9-kernel/src/wasm/providers.rs` — that crate targets
/// `aarch64-unknown-none` and mirrors shapes rather than reusing them): arrows arrive
/// as `ESC [ <final>` / `ESC O <final>` with optional parameter bytes (0x20..=0x3f)
/// before the final; unknown finals are consumed silently; a lone ESC is dropped and
/// the byte after it decodes normally.
#[derive(Default)]
struct KeyDecoder {
    state: EscState,
}

#[derive(Default, Clone, Copy, PartialEq)]
enum EscState {
    #[default]
    Idle,
    Esc,
    Csi,
}

impl KeyDecoder {
    /// Feed one byte; `Some(event)` when a complete keystroke decodes.
    fn push(&mut self, byte: u8) -> Option<KeyEvent> {
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
                    // Home/End/Delete and the rest: consumed, ignored (v1).
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
            // Remaining control bytes, raw (3 = Ctrl-C, 4 = Ctrl-D, …).
            0x00..=0x1f => KeyEvent::Ctrl(byte),
            // Printable ASCII and UTF-8 lead/continuation bytes.
            _ => KeyEvent::Char(byte),
        })
    }
}

fn io_to_text(err: io::Error) -> TextError {
    match err.kind() {
        io::ErrorKind::BrokenPipe | io::ErrorKind::UnexpectedEof => TextError::Closed,
        _ => TextError::Io(err.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::completion::completer;
    use std::io::Cursor;
    use std::sync::Arc;
    use std::time::Duration;

    /// A `Write` sink tests can inspect after handing it to the provider.
    #[derive(Clone, Default)]
    struct SharedSink(Arc<Mutex<Vec<u8>>>);

    impl SharedSink {
        fn contents(&self) -> String {
            String::from_utf8(self.0.lock().unwrap().clone()).unwrap()
        }
    }

    impl Write for SharedSink {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    fn provider_with(input: &str) -> (TextProvider, SharedSink, SharedSink) {
        let out = SharedSink::default();
        let err = SharedSink::default();
        let provider =
            TextProvider::from_streams(out.clone(), err.clone(), Cursor::new(input.to_owned()));
        (provider, out, err)
    }

    fn read_one_line(provider: &TextProvider) -> ReadLineCompletion {
        let (tx, rx) = mpsc::channel();
        provider.read_line(completer(move |line| tx.send(line).unwrap()));
        rx.recv_timeout(Duration::from_secs(10)).unwrap()
    }

    #[test]
    fn write_goes_to_the_selected_stream() {
        let (provider, out, err) = provider_with("");
        provider.write(OutputStream::Out, "to stdout\n").unwrap();
        provider.write(OutputStream::Err, "to stderr\n").unwrap();
        provider.write(OutputStream::Out, "more").unwrap();
        assert_eq!(out.contents(), "to stdout\nmore");
        assert_eq!(err.contents(), "to stderr\n");
    }

    #[test]
    fn read_line_strips_newlines_and_reports_eof() {
        let (provider, _out, _err) = provider_with("first\r\nsecond\nlast without newline");
        assert_eq!(read_one_line(&provider), Ok(Some("first".to_owned())));
        assert_eq!(read_one_line(&provider), Ok(Some("second".to_owned())));
        assert_eq!(
            read_one_line(&provider),
            Ok(Some("last without newline".to_owned()))
        );
        assert_eq!(read_one_line(&provider), Ok(None));
        // End of input is sticky.
        assert_eq!(read_one_line(&provider), Ok(None));
    }

    #[test]
    fn write_failures_are_mapped() {
        struct Broken;
        impl Write for Broken {
            fn write(&mut self, _buf: &[u8]) -> io::Result<usize> {
                Err(io::Error::new(io::ErrorKind::BrokenPipe, "pipe closed"))
            }
            fn flush(&mut self) -> io::Result<()> {
                Ok(())
            }
        }
        let provider = TextProvider::from_streams(Broken, io::sink(), Cursor::new(Vec::new()));
        assert_eq!(
            provider.write(OutputStream::Out, "x"),
            Err(TextError::Closed)
        );
    }

    #[test]
    fn pending_read_lines_complete_even_if_the_provider_is_dropped_first() {
        let (provider, _out, _err) = provider_with("late line\n");
        let (tx, rx) = mpsc::channel();
        provider.read_line(completer(move |line| tx.send(line).unwrap()));
        drop(provider);
        assert_eq!(
            rx.recv_timeout(Duration::from_secs(10)).unwrap(),
            Ok(Some("late line".to_owned()))
        );
    }

    #[test]
    fn stream_backed_read_key_answers_unsupported() {
        let (provider, _out, _err) = provider_with("typed input\n");
        let (tx, rx) = mpsc::channel();
        provider.read_key(completer(move |key| tx.send(key).unwrap()));
        assert_eq!(
            rx.recv_timeout(Duration::from_secs(10)).unwrap(),
            Err(TextError::Unsupported)
        );
        // The refusal consumed nothing: the line is still there for read-line.
        assert_eq!(read_one_line(&provider), Ok(Some("typed input".to_owned())));
    }

    /// Drive the decoder over a byte script, collecting the decoded keys.
    fn decode(script: &[u8]) -> Vec<KeyEvent> {
        let mut decoder = KeyDecoder::default();
        script.iter().filter_map(|&b| decoder.push(b)).collect()
    }

    #[test]
    fn decoder_maps_plain_bytes() {
        assert_eq!(
            decode(b"ab\r\n\t\x7f\x08"),
            vec![
                KeyEvent::Char(b'a'),
                KeyEvent::Char(b'b'),
                KeyEvent::Enter,
                KeyEvent::Enter,
                KeyEvent::Tab,
                KeyEvent::Backspace,
                KeyEvent::Backspace,
            ]
        );
        assert_eq!(
            decode(&[0x03, 0x04, 0x15]),
            vec![KeyEvent::Ctrl(3), KeyEvent::Ctrl(4), KeyEvent::Ctrl(0x15)]
        );
        // UTF-8 bytes pass through as char bytes.
        assert_eq!(
            decode("é".as_bytes()),
            vec![KeyEvent::Char(0xc3), KeyEvent::Char(0xa9)]
        );
    }

    #[test]
    fn decoder_maps_escape_sequences() {
        assert_eq!(decode(b"\x1b[A\x1b[B"), vec![KeyEvent::Up, KeyEvent::Down]);
        assert_eq!(
            decode(b"\x1b[C\x1bOD"),
            vec![KeyEvent::Right, KeyEvent::Left]
        );
        // Parameter bytes are consumed; unknown finals are swallowed (Delete: ESC[3~).
        assert_eq!(decode(b"\x1b[3~x"), vec![KeyEvent::Char(b'x')]);
        // A lone ESC is dropped and the following byte decodes normally.
        assert_eq!(decode(b"\x1bq"), vec![KeyEvent::Char(b'q')]);
        // ESC ESC stays armed: the sequence after the second ESC still decodes.
        assert_eq!(decode(b"\x1b\x1b[A"), vec![KeyEvent::Up]);
    }
}
