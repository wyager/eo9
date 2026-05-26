//! The in-memory, deterministic provider backend.
//!
//! [`Sandbox`] grants capabilities backed entirely by the runtime's in-memory providers:
//! captured text, a frozen clock, a seeded PRNG, and an in-memory filesystem. Nothing
//! touches the host, runs are reproducible, and the backend is portable — it is the exact
//! shape the wasm32/Pulley embedding (plan/15) will reuse, swapping the in-memory shims
//! for browser-backed ones.
//!
//! A `Sandbox` is cheap to clone: the captured-text buffers and the in-memory filesystem
//! are shared, so a clone handed to [`crate::Builder::backend`] still lets the original be
//! inspected after a run (`stdout`, `file_contents`, …). The clock and seed are fixed
//! config; the seeded entropy is reset at the start of every run, so each run is
//! reproducible from the same seed.

use eo9_runtime::providers::{CaptureText, FrozenTime, MemFs, SeededEntropy};

use crate::{Grants, ProviderSource, Roots};

/// Default frozen wall-clock instant: 2024-01-01T00:00:00Z (seconds since the epoch).
const DEFAULT_NOW_SECONDS: i64 = 1_704_067_200;

/// Deterministic, in-memory provider backend. See the module docs.
#[derive(Clone)]
pub struct Sandbox {
    text: CaptureText,
    fs: MemFs,
    seed: u64,
    now_seconds: i64,
    monotonic_ns: u64,
}

impl Default for Sandbox {
    fn default() -> Self {
        Sandbox {
            text: CaptureText::new(),
            fs: MemFs::new(),
            seed: 0,
            now_seconds: DEFAULT_NOW_SECONDS,
            monotonic_ns: 0,
        }
    }
}

impl Sandbox {
    /// A fresh sandbox: empty captured text, empty filesystem, seed 0, a fixed clock.
    pub fn new() -> Self {
        Self::default()
    }

    /// Set the PRNG seed (the seeded entropy is reset to this at the start of each run).
    pub fn with_seed(mut self, seed: u64) -> Self {
        self.seed = seed;
        self
    }

    /// Set the frozen clock: wall-clock seconds since the epoch and the monotonic reading.
    pub fn with_clock(mut self, now_seconds: i64, monotonic_ns: u64) -> Self {
        self.now_seconds = now_seconds;
        self.monotonic_ns = monotonic_ns;
        self
    }

    /// Pre-populate a file in the in-memory filesystem.
    pub fn insert_file(&self, path: &str, contents: impl Into<Vec<u8>>) {
        self.fs.insert_file(path, contents);
    }

    /// Pre-create a directory in the in-memory filesystem.
    pub fn insert_dir(&self, path: &str) {
        self.fs.insert_dir(path);
    }

    /// The contents of a file in the in-memory filesystem, if it exists.
    pub fn file_contents(&self, path: &str) -> Option<Vec<u8>> {
        self.fs.file_contents(path)
    }

    /// Everything programs have written to standard output so far.
    pub fn stdout(&self) -> String {
        self.text.stdout()
    }

    /// Everything programs have written to standard error so far.
    pub fn stderr(&self) -> String {
        self.text.stderr()
    }
}

impl ProviderSource for Sandbox {
    fn roots(&self, grants: Grants) -> Result<Roots, crate::EmbedError> {
        Ok(Roots {
            text: grants
                .text
                .then(|| Box::new(self.text.clone()) as Box<dyn eo9_runtime::TextProvider>),
            time: grants.time.then(|| {
                Box::new(FrozenTime::new(self.now_seconds, self.monotonic_ns))
                    as Box<dyn eo9_runtime::TimeProvider>
            }),
            entropy: grants.entropy.then(|| {
                Box::new(SeededEntropy::new(self.seed)) as Box<dyn eo9_runtime::EntropyProvider>
            }),
            fs: grants
                .fs
                .then(|| Box::new(self.fs.clone()) as Box<dyn eo9_runtime::FsProvider>),
        })
    }
}
