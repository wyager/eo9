//! Host-side provider traits for the root capabilities the runtime can wire into a task.
//!
//! These traits are the seam between the runtime (this crate) and whoever implements the
//! root capabilities — area 08's unix providers on the usermode path, the kernel's drivers
//! on bare metal, or the in-memory test providers below. They are deliberately small,
//! synchronous where the WIT is synchronous, and use plain `core::future::Future` (no
//! executor, no runtime types) where the WIT returns a `future<T>`: the runtime polls the
//! returned operation from the task's event loop, and the waker it passes *is* the task's
//! doorbell — completing the operation from another thread and waking that waker is all a
//! provider has to do.
//!
//! The shapes mirror `wit/text`, `wit/time`, and `wit/entropy` directly; see
//! plan/04-runtime.md § Decisions for the trait-surface rationale.

use std::future::Future;
use std::pin::Pin;

/// A pending provider operation: a plain boxed future, polled from the task's event loop.
///
/// The waker passed to `poll` is the owning task's doorbell; wake it (from any thread) when
/// the operation can make progress. If the task is killed the operation is dropped — the
/// provider's `Drop` impl is the place to abort or complete any underlying work.
pub type BoxOp<T> = Pin<Box<dyn Future<Output = T> + Send + 'static>>;

/// Which output stream a [`TextProvider::write`] targets (`eo9:text/text.output-stream`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OutputStream {
    /// Standard output.
    Out,
    /// Standard error.
    Err,
}

/// Error type for text operations (`eo9:text/text.text-error`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TextError {
    /// The stream is closed (output detached, or stdin hit end of input).
    Closed,
    /// Any other I/O failure.
    Io(String),
}

/// Wall-clock time (`eo9:time/time.datetime`): seconds and nanoseconds since the Unix epoch.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Datetime {
    pub seconds: i64,
    pub nanoseconds: u32,
}

/// Root provider for `eo9:text/text`.
pub trait TextProvider: Send + 'static {
    /// Write UTF-8 text to stdout or stderr.
    fn write(&mut self, to: OutputStream, text: &str) -> Result<(), TextError>;

    /// Read one line from stdin (without the trailing newline); `None` at end of input.
    fn read_line(&mut self) -> BoxOp<Result<Option<String>, TextError>>;
}

/// Root provider for `eo9:time/time`.
pub trait TimeProvider: Send + 'static {
    /// Current wall-clock time.
    fn now(&mut self) -> Datetime;

    /// Current monotonic time in nanoseconds since an arbitrary (per-boot) epoch.
    fn monotonic_now(&mut self) -> u64;

    /// The granularity of this clock in nanoseconds.
    fn resolution(&mut self) -> u64;

    /// Resolves once at least `duration_ns` nanoseconds of monotonic time have elapsed.
    fn sleep(&mut self, duration_ns: u64) -> BoxOp<()>;
}

/// Root provider for `eo9:entropy/entropy`.
pub trait EntropyProvider: Send + 'static {
    /// Return `len` random bytes.
    fn get_bytes(&mut self, len: u64) -> Vec<u8>;

    /// Return a single random 64-bit value.
    fn get_u64(&mut self) -> u64;
}

/// The set of root providers wired into one task at spawn.
///
/// Every field is optional: a task's component is linked only against the interfaces it
/// imports, and an import with no corresponding provider is a spawn-time error (the
/// loader rule from SPEC.md "WASM runtime").
#[derive(Default)]
pub struct Providers {
    pub text: Option<Box<dyn TextProvider>>,
    pub time: Option<Box<dyn TimeProvider>>,
    pub entropy: Option<Box<dyn EntropyProvider>>,
}

impl Providers {
    /// No providers at all (a task with no capabilities).
    pub fn none() -> Self {
        Self::default()
    }
}

// ---------------------------------------------------------------------------------------
// In-memory providers, for tests and deterministic runs inside this crate. The real root
// providers on the usermode path are area 08's (`eo9-providers-unix`).
// ---------------------------------------------------------------------------------------

/// In-memory text provider: captures writes, serves scripted stdin lines.
///
/// Cloning shares the underlying buffers, so a test can keep a clone and read what the
/// task wrote after the provider itself has been moved into the task.
#[derive(Default, Clone)]
pub struct CaptureText {
    /// Everything written to `out`, concatenated.
    pub out: std::sync::Arc<std::sync::Mutex<String>>,
    /// Everything written to `err`, concatenated.
    pub err: std::sync::Arc<std::sync::Mutex<String>>,
    /// Lines `read_line` will serve, in order; end of input afterwards.
    stdin: std::sync::Arc<std::sync::Mutex<std::collections::VecDeque<String>>>,
}

impl CaptureText {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_stdin(lines: impl IntoIterator<Item = String>) -> Self {
        let capture = Self::default();
        capture.stdin.lock().unwrap().extend(lines);
        capture
    }

    /// Everything written to `out` so far.
    pub fn stdout(&self) -> String {
        self.out.lock().unwrap().clone()
    }

    /// Everything written to `err` so far.
    pub fn stderr(&self) -> String {
        self.err.lock().unwrap().clone()
    }
}

impl TextProvider for CaptureText {
    fn write(&mut self, to: OutputStream, text: &str) -> Result<(), TextError> {
        match to {
            OutputStream::Out => self.out.lock().unwrap().push_str(text),
            OutputStream::Err => self.err.lock().unwrap().push_str(text),
        }
        Ok(())
    }

    fn read_line(&mut self) -> BoxOp<Result<Option<String>, TextError>> {
        let line = self.stdin.lock().unwrap().pop_front();
        Box::pin(std::future::ready(Ok(line)))
    }
}

/// Frozen clock: both clocks report a fixed instant; `sleep` resolves immediately.
#[derive(Debug, Clone, Copy)]
pub struct FrozenTime {
    pub now: Datetime,
    pub monotonic_ns: u64,
}

impl FrozenTime {
    pub fn new(now_seconds: i64, monotonic_ns: u64) -> Self {
        Self {
            now: Datetime {
                seconds: now_seconds,
                nanoseconds: 0,
            },
            monotonic_ns,
        }
    }
}

impl TimeProvider for FrozenTime {
    fn now(&mut self) -> Datetime {
        self.now
    }

    fn monotonic_now(&mut self) -> u64 {
        self.monotonic_ns
    }

    fn resolution(&mut self) -> u64 {
        1
    }

    fn sleep(&mut self, _duration_ns: u64) -> BoxOp<()> {
        Box::pin(std::future::ready(()))
    }
}

/// Deterministic PRNG from a fixed seed (splitmix64), for reproducible tests.
#[derive(Debug, Clone, Copy)]
pub struct SeededEntropy {
    state: u64,
}

impl SeededEntropy {
    pub fn new(seed: u64) -> Self {
        Self { state: seed }
    }

    fn next(&mut self) -> u64 {
        // splitmix64: tiny, dependency-free, and good enough for deterministic tests.
        self.state = self.state.wrapping_add(0x9e3779b97f4a7c15);
        let mut z = self.state;
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58476d1ce4e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d049bb133111eb);
        z ^ (z >> 31)
    }
}

impl EntropyProvider for SeededEntropy {
    fn get_bytes(&mut self, len: u64) -> Vec<u8> {
        let len = usize::try_from(len).unwrap_or(usize::MAX);
        let mut out = Vec::with_capacity(len);
        while out.len() < len {
            let chunk = self.next().to_le_bytes();
            let take = usize::min(8, len - out.len());
            out.extend_from_slice(&chunk[..take]);
        }
        out
    }

    fn get_u64(&mut self) -> u64 {
        self.next()
    }
}
