//! Root provider for `eo9:text` — standard text streams backed by std{in,out,err}.
//!
//! `write` is synchronous (matching the WIT signature); `read-line` is potentially
//! blocking and completes asynchronously through a [`Completer`].
//!
//! The provider owns a dedicated, detached reader thread rather than using the shared
//! blocking pool: a read on an interactive stdin can block indefinitely, and it must not
//! be able to wedge a pool that fs/disk operations depend on, nor delay provider
//! shutdown.
//!
//! Kill behavior: an in-flight `read-line` runs until the underlying blocking read
//! returns; the consumed line is handed to the completer, and if the issuing task is
//! dead the runtime drops it — the line is lost, not pushed back. `write` never spans a
//! kill (it is synchronous).

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
    /// Any other host I/O failure.
    Io(String),
}

/// Completion payload of `read-line`: `Ok(None)` at end of input.
pub type ReadLineCompletion = Result<Option<String>, TextError>;

/// The host trait mirroring the WIT `eo9:text/text` interface (minus `default`, which is
/// the runtime's resource-table business).
pub trait TextHost: Send + Sync {
    /// Write UTF-8 text to stdout or stderr. Synchronous: the data has been handed to
    /// (and flushed into) the host stream when this returns.
    fn write(&self, to: OutputStream, text: &str) -> Result<(), TextError>;

    /// Read one line from stdin, without the trailing newline; completes with
    /// `Ok(None)` at end of input.
    fn read_line(&self, complete: Completer<ReadLineCompletion>);
}

/// The unix text provider. Corresponds to the WIT `text-impl` root handle.
pub struct TextProvider {
    out: Mutex<Box<dyn Write + Send>>,
    err: Mutex<Box<dyn Write + Send>>,
    /// Requests for the dedicated reader thread. Dropping the sender lets the thread
    /// exit once it has served everything it already accepted.
    reader: Sender<Completer<ReadLineCompletion>>,
}

impl TextProvider {
    /// A provider wired to the process's real standard streams.
    pub fn stdio() -> Self {
        Self::from_streams(io::stdout(), io::stderr(), BufReader::new(io::stdin()))
    }

    /// A provider over arbitrary streams (used by tests, and by hosts that want to
    /// redirect the program's text I/O).
    pub fn from_streams(
        out: impl Write + Send + 'static,
        err: impl Write + Send + 'static,
        input: impl BufRead + Send + 'static,
    ) -> Self {
        let (sender, receiver) = mpsc::channel::<Completer<ReadLineCompletion>>();
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
        if let Err(mpsc::SendError(complete)) = self.reader.send(complete) {
            // The reader thread is gone (it cannot outlive the provider unless it was
            // never spawned); report the stream as closed rather than losing the op.
            complete(Err(TextError::Closed));
        }
    }
}

fn reader_loop(mut input: impl BufRead, requests: &mpsc::Receiver<Completer<ReadLineCompletion>>) {
    while let Ok(complete) = requests.recv() {
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
}
