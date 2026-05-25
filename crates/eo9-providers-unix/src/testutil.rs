//! Shared test support: a dependency-free temporary-directory helper.
//!
//! Test directories live under the workspace `target/` directory (not the system temp
//! dir) so tests stay inside the repository tree and are removed by `cargo clean` even
//! if a test aborts before `Drop` runs.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

static NEXT_ID: AtomicU64 = AtomicU64::new(0);

/// A uniquely named directory, removed (recursively) on drop.
pub(crate) struct TempDir {
    path: PathBuf,
}

impl TempDir {
    pub(crate) fn new() -> Self {
        let base = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("..")
            .join("..")
            .join("target")
            .join("eo9-providers-unix-tests");
        std::fs::create_dir_all(&base).expect("failed to create test temp base directory");
        loop {
            let name = format!(
                "tmp-{}-{}",
                std::process::id(),
                NEXT_ID.fetch_add(1, Ordering::Relaxed)
            );
            let path = base.join(name);
            match std::fs::create_dir(&path) {
                Ok(()) => return Self { path },
                Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => {}
                Err(err) => panic!("failed to create test temp directory: {err}"),
            }
        }
    }

    pub(crate) fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn temp_dirs_are_distinct_and_cleaned_up() {
        let first = TempDir::new();
        let second = TempDir::new();
        assert_ne!(first.path(), second.path());
        assert!(first.path().is_dir());
        let kept = first.path().to_path_buf();
        drop(first);
        assert!(!kept.exists());
        assert!(second.path().is_dir());
    }
}
