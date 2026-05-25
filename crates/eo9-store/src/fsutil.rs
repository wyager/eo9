//! Small filesystem helpers shared by the object store and the compile cache.
//!
//! Everything the store writes is made visible atomically: bytes go to a `.tmp-*` sibling
//! first and are `rename`d into place, so readers never observe a partially written
//! object, manifest, or cache entry.

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::StoreError;

/// Prefix used for all temporary files and directories inside the store.
pub(crate) const TMP_PREFIX: &str = ".tmp-";

/// Seconds since the Unix epoch, saturating at zero if the clock is before the epoch.
pub(crate) fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or_default()
}

/// A sibling path `<dir>/.tmp-<stem>-<unique>` that no other writer in this or any other
/// process will pick: uniqueness comes from the pid plus a process-global counter.
pub(crate) fn tmp_sibling(path: &Path) -> PathBuf {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let count = COUNTER.fetch_add(1, Ordering::Relaxed);
    let stem = path
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default();
    let unique = format!("{TMP_PREFIX}{stem}-{}-{count}", std::process::id());
    path.with_file_name(unique)
}

/// Atomically write `bytes` to `path` (write to a temporary sibling, then rename).
/// With `readonly`, the file's write permission bits are cleared before the rename, so
/// the final file is immutable from the moment it appears.
pub(crate) fn write_atomic(path: &Path, bytes: &[u8], readonly: bool) -> Result<(), StoreError> {
    let tmp = tmp_sibling(path);
    let result = write_atomic_inner(path, &tmp, bytes, readonly);
    if result.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    result
}

fn write_atomic_inner(
    path: &Path,
    tmp: &Path,
    bytes: &[u8],
    readonly: bool,
) -> Result<(), StoreError> {
    let err = |source| StoreError::io(path, source);
    let mut file = fs::File::create(tmp).map_err(err)?;
    file.write_all(bytes).map_err(err)?;
    file.sync_all().map_err(err)?;
    if readonly {
        let mut permissions = file.metadata().map_err(err)?.permissions();
        permissions.set_readonly(true);
        file.set_permissions(permissions).map_err(err)?;
    }
    drop(file);
    fs::rename(tmp, path).map_err(err)
}
