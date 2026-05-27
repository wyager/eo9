//! Resolving a program reference — a bare dotted store name or a host path — into
//! component bytes plus the content hash the compile cache keys on.
//!
//! Both routes go through an immutable handle (SPEC.md "Loading is immutability-first"):
//!
//! * a **store name** resolves to the store's write-once object and is read through the
//!   store's [`ObjectHandle`](eo9_store::ObjectHandle), then re-hashed to confirm it
//!   still matches the hash it was resolved to;
//! * a **path** is opened through the unix fs provider's `open-exec`, which takes a
//!   private copy-on-write snapshot (refusing, under the default `clone-or-refuse`
//!   policy, if the backing filesystem cannot clone), so the bytes we hash, cache, and
//!   compile are exactly the bytes of one immutable snapshot — no TOCTOU window.

use std::path::Path;
use std::sync::Arc;
use std::sync::mpsc;

use eo9_providers_unix::fs::{FsError, FsHost, FsProvider, ImmutableHost};
use eo9_providers_unix::{BlockingPool, OwnedBuffer, completer};
use eo9_store::{Name, ObjectHash};

use crate::cli::{Config, vlog};

/// A resolved program: its bytes, their content hash, and where they came from.
pub struct ProgramSource {
    /// The component bytes, read through an immutable handle.
    pub bytes: Vec<u8>,
    /// blake3 hash of `bytes` — the module hash the compile cache keys on.
    pub hash: ObjectHash,
    /// Human-readable provenance, for messages and `-v` diagnostics.
    pub origin: String,
}

/// Whether a program reference is a host path rather than a bare dotted store name.
///
/// The rule is purely syntactic so it never depends on what happens to exist on disk: a
/// reference containing `/`, starting with `.`, or ending in `.wasm` is a path;
/// everything else must parse as a store [`Name`]. A file in the current directory can
/// always be forced to the path route by prefixing `./`.
pub fn is_path(reference: &str) -> bool {
    reference.contains('/') || reference.starts_with('.') || reference.ends_with(".wasm")
}

/// Resolve a program reference for execution.
pub fn resolve_program(cfg: &Config, reference: &str) -> Result<ProgramSource, String> {
    if is_path(reference) {
        resolve_path(cfg, Path::new(reference))
    } else {
        resolve_store_name(cfg, reference)
    }
}

/// Read a component's bytes for inspection only (`eo9 describe`): no execution handle is
/// taken, so a plain read is enough. Returns the bytes and a provenance string.
pub fn read_component(cfg: &Config, reference: &str) -> Result<(Vec<u8>, String), String> {
    if is_path(reference) {
        let bytes =
            std::fs::read(reference).map_err(|err| format!("cannot read {reference}: {err}"))?;
        Ok((bytes, reference.to_string()))
    } else {
        let name = parse_name(reference)?;
        let store = cfg.open_store()?;
        if let Err(err) = crate::seed::seed_store_if_empty(cfg, &store) {
            vlog!(cfg, "could not seed the empty store: {err}");
        }
        let resolved = store.resolve(&name).map_err(|err| err.to_string())?;
        let bytes = store
            .read_object(&resolved.hash)
            .map_err(|err| err.to_string())?;
        Ok((bytes, format!("{name} (store object {})", resolved.hash)))
    }
}

fn parse_name(reference: &str) -> Result<Name, String> {
    Name::parse(reference).map_err(|err| {
        format!(
            "{err} (to run a file instead, use a path containing `/`, starting with `.`, \
             or ending in `.wasm`)"
        )
    })
}

fn resolve_store_name(cfg: &Config, reference: &str) -> Result<ProgramSource, String> {
    let name = parse_name(reference)?;
    let store = cfg.open_store()?;
    // First run against a brand-new store: seed it from the embedded components, exactly
    // like the shell path does, so `eo9 hello ...` works out of the box (a non-empty
    // store is left untouched). A seeding problem only matters if resolution then fails.
    if let Err(err) = crate::seed::seed_store_if_empty(cfg, &store) {
        vlog!(cfg, "could not seed the empty store: {err}");
    }
    let resolved = store.resolve(&name).map_err(|err| err.to_string())?;
    let bytes = resolved.handle.bytes().map_err(|err| err.to_string())?;
    let hash = ObjectHash::of(&bytes);
    if hash != resolved.hash {
        return Err(format!(
            "store object for {name} no longer matches its content hash (expected {}, found {hash})",
            resolved.hash
        ));
    }
    vlog!(
        cfg,
        "resolved {name} -> {hash} ({} bytes) via the store",
        bytes.len()
    );
    Ok(ProgramSource {
        bytes,
        hash,
        origin: format!("{name} (store object {hash})"),
    })
}

/// Open a host path for execution through the fs provider's immutable `open-exec` and
/// read the component bytes through the snapshot handle.
fn resolve_path(cfg: &Config, path: &Path) -> Result<ProgramSource, String> {
    let canonical = path
        .canonicalize()
        .map_err(|err| format!("cannot open {}: {err}", path.display()))?;
    if !canonical.is_file() {
        return Err(format!("{} is not a regular file", path.display()));
    }

    // The snapshot provider is rooted at the program file's own directory and the file is
    // addressed by name below it. (`--fs-root` is a different concern: it is the root of
    // the *program's* eo9:fs capability, not of where the program may be loaded from.)
    let root = canonical
        .parent()
        .ok_or_else(|| format!("{} has no parent directory", canonical.display()))?
        .to_path_buf();
    let relative = canonical.strip_prefix(&root).map_err(|_| {
        format!(
            "{} is not under the fs root {}",
            canonical.display(),
            root.display()
        )
    })?;
    let guest_path = relative
        .to_str()
        .ok_or_else(|| format!("{} is not valid UTF-8", relative.display()))?;

    let pool = Arc::new(BlockingPool::new(2));
    let provider = FsProvider::new(&root, pool)
        .map_err(|err| {
            format!(
                "cannot create the fs provider rooted at {}: {err}",
                root.display()
            )
        })?
        .with_exec_snapshot_policy(cfg.exec_snapshot);

    let (sender, receiver) = mpsc::channel();
    provider.open_exec(
        guest_path,
        completer(move |result| {
            let _ = sender.send(result);
        }),
    );
    let handle = receiver
        .recv()
        .map_err(|_| "the fs provider dropped the open-exec completion".to_string())?
        .map_err(|err| open_exec_error(path, &err))?;

    let bytes = read_all(handle.as_ref(), path)?;
    let hash = ObjectHash::of(&bytes);
    vlog!(
        cfg,
        "opened {} for execution via open-exec ({} bytes, blake3 {hash})",
        path.display(),
        bytes.len()
    );
    Ok(ProgramSource {
        bytes,
        hash,
        origin: path.display().to_string(),
    })
}

/// Read the whole immutable snapshot through its handle.
fn read_all(handle: &dyn ImmutableHost, path: &Path) -> Result<Vec<u8>, String> {
    let size = handle.size();
    let mut bytes = Vec::with_capacity(usize::try_from(size).unwrap_or(0));
    let mut offset = 0u64;
    while offset < size {
        let (sender, receiver) = mpsc::channel();
        handle.read(
            offset,
            OwnedBuffer::new(size - offset),
            completer(move |completion| {
                let _ = sender.send(completion);
            }),
        );
        let (buffer, result) = receiver
            .recv()
            .map_err(|_| "the fs provider dropped a read completion".to_string())?;
        let read = result.map_err(|err| {
            format!(
                "reading {} through its immutable handle failed: {err:?}",
                path.display()
            )
        })?;
        if read.bytes_read == 0 {
            break;
        }
        let chunk = buffer.copy_out(0, read.bytes_read).map_err(|err| {
            format!(
                "reading {} through its immutable handle failed: {err}",
                path.display()
            )
        })?;
        bytes.extend_from_slice(&chunk);
        offset += read.bytes_read;
    }
    Ok(bytes)
}

fn open_exec_error(path: &Path, err: &FsError) -> String {
    match err {
        FsError::NotImmutable => format!(
            "cannot open {} for execution: the filesystem cannot take a copy-on-write \
             snapshot of it, and the default `--exec-snapshot clone-or-refuse` policy only \
             accepts backends that can promise immutability. Re-run with `--exec-snapshot \
             clone-or-copy` to allow a byte-for-byte copy instead (the copy can observe a \
             torn write if something modifies the file at the same moment).",
            path.display()
        ),
        other => format!("cannot open {} for execution: {other:?}", path.display()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn path_detection_is_syntactic() {
        assert!(is_path("./hello"));
        assert!(is_path("guest/target/components/eo9-example-hello.wasm"));
        assert!(is_path("hello.wasm"));
        assert!(is_path("/abs/path"));
        assert!(!is_path("hello"));
        assert!(!is_path("virtualfs.create"));
        assert!(!is_path("fs.memfs"));
    }
}
