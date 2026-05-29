//! The persistent store disk: a disk-backed cache of on-target compile results
//! (plan/12: store-on-eofs, first rung).
//!
//! When the kernel boots with the bare `storedisk` command-line token, it brings up the
//! in-kernel virtio-blk driver (`crate::virtio_blk`), mounts the eofs filesystem found on
//! the disk (formatting a **blank** disk in place; anything unrecognized is left alone),
//! and from then on the shell's `compile` operation consults `/cache/<blake3>.cwasm` before
//! invoking Cranelift: a composition compiled once keeps its artifact across reboots, so the
//! second boot runs it without recompiling. Without the token (or without a disk) nothing
//! changes — every call here degrades to "no cache".
//!
//! Trust model: the cache holds native-code artifacts and is loaded with
//! `Component::deserialize`, which trusts its input — so disk-loaded bytes are **never**
//! handed to it unverified. Every cache entry is written with a small header carrying a
//! keyed-blake3 tag (the key is baked into the kernel image at build time by xtask and never
//! stored on the disk), and `lookup` recomputes and compares the tag before returning the
//! artifact; any mismatch is a typed refusal that falls back to recompiling, which
//! overwrites the bad entry. The layering: eofs's (unkeyed) block checksums catch
//! corruption, the keyed tag catches an adversary who can rewrite disk blocks *and* fix up
//! those checksums but does not hold the kernel image's key, and `Component::deserialize`'s
//! own compatibility checks catch artifacts from a different wasmtime build. The baked-in
//! store image and the kernel's own in-memory compiles stay untagged — they are the same
//! trust domain as the kernel image itself. Durability note: the in-kernel driver does not
//! negotiate VIRTIO_BLK_F_FLUSH yet (same as the wasm driver); eofs commits are ordered but
//! a host power cut may lose the most recent ones.

use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;
use core::cell::UnsafeCell;
use core::sync::atomic::{AtomicBool, Ordering};

use eofs_core::{BlockDevice, DeviceError, Eofs, FormatOptions};

use crate::virtio_blk::VirtioBlk;

/// The eofs directory that holds cached compile artifacts.
const CACHE_DIR: &str = "/cache";

/// Upper bound on a single cached artifact (a fused composition's serialized native code).
/// Larger results are simply not cached; they keep compiling on every boot.
const MAX_ARTIFACT_BYTES: u64 = 8 * 1024 * 1024;

/// The MAC key baked into this kernel image by `cargo xtask build-kernel aarch64` (a
/// 32-byte /dev/urandom value generated once per checkout; see `ensure_storedisk_mac_key`
/// in xtask). Cached artifacts are deserialized only after their keyed-blake3 tag verifies
/// under this key, so bytes written to the disk cannot become native code in this kernel
/// without it. Deleting the key file and rebuilding rotates the key, which simply makes
/// every previously cached artifact fail verification and recompile.
static MAC_KEY: &[u8; 32] = include_bytes!(env!("EO9_STOREDISK_MAC_KEY"));

/// Magic introducing one cache entry on disk. Entry layout: magic, artifact length
/// (u64 little-endian), keyed-blake3 tag over `cache-key || length || artifact`, then the
/// artifact.
const ENTRY_MAGIC: &[u8; 8] = b"EO9CACH1";
const ENTRY_HEADER_BYTES: usize = 8 + 8 + 32;

/// The keyed tag for one artifact. The cache key (the entry's own name) is folded in so a
/// valid entry cannot be copied over a *different* entry's path — composition X can never
/// be served composition Y's artifact, even though both tags verify under the same key.
/// The length is folded in so a tag computed over a longer artifact cannot be reused for a
/// truncated rewrite of the same entry. (The key is a fixed-length blake3 hex string, so
/// the concatenation is unambiguous.)
fn entry_tag(key: &str, artifact: &[u8]) -> [u8; 32] {
    let mut hasher = blake3::Hasher::new_keyed(MAC_KEY);
    hasher.update(key.as_bytes());
    hasher.update(&(artifact.len() as u64).to_le_bytes());
    hasher.update(artifact);
    *hasher.finalize().as_bytes()
}

/// `eofs_core::BlockDevice` over the in-kernel virtio-blk driver. The trait takes `&self`
/// for reads, so the driver (whose ring indices advance on every request) sits in an
/// `UnsafeCell`; every access goes through the module-level lock below, which restores the
/// exclusive access the cell needs.
struct StoreDevice {
    capacity: u64,
    driver: UnsafeCell<VirtioBlk>,
}

// SAFETY: all access is serialized by `STORE` (the module-level lock); the kernel is
// single-core and the lock is never re-entered (eofs never calls back into the cache).
unsafe impl Send for StoreDevice {}
unsafe impl Sync for StoreDevice {}

impl StoreDevice {
    fn new(driver: VirtioBlk) -> StoreDevice {
        StoreDevice {
            capacity: driver.capacity_bytes(),
            driver: UnsafeCell::new(driver),
        }
    }

    #[allow(clippy::mut_from_ref)]
    fn driver(&self) -> &mut VirtioBlk {
        // SAFETY: see the `Send`/`Sync` justification above — the outer lock gives this
        // call exclusive access for its whole duration.
        unsafe { &mut *self.driver.get() }
    }
}

impl BlockDevice for StoreDevice {
    fn size(&self) -> u64 {
        self.capacity
    }

    fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<(), DeviceError> {
        self.driver()
            .read_bytes(offset, buf)
            .map_err(|_| DeviceError::Io)
    }

    fn write_at(&mut self, offset: u64, data: &[u8]) -> Result<(), DeviceError> {
        self.driver()
            .write_bytes(offset, data)
            .map_err(|_| DeviceError::Io)
    }

    fn flush(&mut self) -> Result<(), DeviceError> {
        // No VIRTIO_BLK_F_FLUSH yet (see the module docs); writes have completed by the
        // time each request returns, so there is nothing further to order here.
        Ok(())
    }
}

/// The mounted store disk, if this boot has one.
struct Mounted {
    fs: Eofs<StoreDevice>,
}

/// A tiny spinlock, mirroring `shellexec::KLock` (which is private to that module): the
/// kernel is single-core and none of the paths that take this lock can re-enter it.
struct CacheLock {
    locked: AtomicBool,
    value: UnsafeCell<Option<Mounted>>,
}

// SAFETY: access to `value` is serialized by `locked`.
unsafe impl Sync for CacheLock {}

impl CacheLock {
    const fn new() -> CacheLock {
        CacheLock {
            locked: AtomicBool::new(false),
            value: UnsafeCell::new(None),
        }
    }

    fn with<R>(&self, f: impl FnOnce(&mut Option<Mounted>) -> R) -> R {
        while self.locked.swap(true, Ordering::Acquire) {
            core::hint::spin_loop();
        }
        // SAFETY: the flag gives exclusive access; the closure never re-enters the lock.
        let result = f(unsafe { &mut *self.value.get() });
        self.locked.store(false, Ordering::Release);
        result
    }
}

static STORE: CacheLock = CacheLock::new();

/// Whether this boot has a usable store disk.
pub fn enabled() -> bool {
    STORE.with(|slot| slot.is_some())
}

/// Bring up the store disk: probe the virtio-blk function, mount (or format, blank disks
/// only) the eofs filesystem on it, and report what was found. Called once from the runner
/// when the `storedisk` boot token is present; every failure degrades to "no cache" with a
/// printed reason.
pub fn init() {
    let driver = match VirtioBlk::probe_and_start() {
        Ok(driver) => driver,
        Err(error) => {
            crate::kprintln!("storedisk: unavailable: {error}");
            return;
        }
    };
    let capacity = driver.capacity_bytes();
    crate::kprintln!(
        "storedisk: virtio-blk {} sectors ({} MiB) claimed for the kernel store",
        capacity / 512,
        capacity / (1024 * 1024),
    );

    let device = StoreDevice::new(driver);

    // Decide between mount and format without ever clobbering foreign data: a disk whose
    // first filesystem block is all zero is treated as blank and formatted; anything else
    // must mount as eofs or the cache stays off.
    let mut probe = alloc::vec![0u8; 4096];
    if device.read_at(0, &mut probe).is_err() {
        crate::kprintln!("storedisk: unavailable: the first block could not be read");
        return;
    }
    let blank = probe.iter().all(|byte| *byte == 0);

    let fs = if blank {
        match Eofs::format(device, &FormatOptions::default()) {
            Ok(fs) => {
                crate::kprintln!("storedisk: blank disk formatted with eofs (block 4096, lz4 on)");
                fs
            }
            Err(error) => {
                crate::kprintln!("storedisk: formatting the blank disk failed: {error:?}");
                return;
            }
        }
    } else {
        match Eofs::mount(device) {
            Ok(fs) => fs,
            Err(error) => {
                crate::kprintln!(
                    "storedisk: the disk holds something that is not a mountable eofs \
                     filesystem ({error:?}); leaving it untouched, compile cache disabled"
                );
                return;
            }
        }
    };

    let cached = fs.list(CACHE_DIR).map(|names| names.len()).unwrap_or(0);
    crate::kprintln!(
        "storedisk: eofs mounted (txg {}), {} cached compile artifact(s)",
        fs.txg(),
        cached
    );
    STORE.with(|slot| *slot = Some(Mounted { fs }));
}

/// The cache key for a fused composition: blake3 of the exact bytes handed to the compiler.
pub fn key(executable_bytes: &[u8]) -> String {
    blake3::hash(executable_bytes).to_hex().as_str().into()
}

/// Look up a cached artifact. Returns the artifact bytes only after the entry's keyed tag
/// verifies; `None` on any miss, parse failure, or verification failure (each is printed,
/// never fatal — the caller falls back to compiling, which overwrites the entry).
pub fn lookup(key: &str) -> Option<Vec<u8>> {
    STORE.with(|slot| {
        let mounted = slot.as_mut()?;
        let path = format!("{CACHE_DIR}/{key}.cwasm");
        let stat = mounted.fs.stat(&path).ok()?;
        if stat.size < ENTRY_HEADER_BYTES as u64
            || stat.size > MAX_ARTIFACT_BYTES + ENTRY_HEADER_BYTES as u64
        {
            crate::kprintln!("storedisk: cached entry has an implausible size; ignoring it");
            return None;
        }
        let mut bytes = alloc::vec![0u8; stat.size as usize];
        match mounted.fs.read(&path, 0, &mut bytes) {
            Ok(read) if read as u64 == stat.size => {}
            Ok(_) => return None,
            Err(error) => {
                crate::kprintln!("storedisk: reading a cached artifact failed: {error:?}");
                return None;
            }
        }

        // Parse and verify the entry header before anything else looks at the bytes.
        if &bytes[0..8] != ENTRY_MAGIC {
            crate::kprintln!("storedisk: cached entry has a bad header; ignoring it");
            return None;
        }
        let mut length = [0u8; 8];
        length.copy_from_slice(&bytes[8..16]);
        let length = u64::from_le_bytes(length) as usize;
        if length != bytes.len() - ENTRY_HEADER_BYTES {
            crate::kprintln!(
                "storedisk: cached entry failed integrity verification (length mismatch); \
                 it will be recompiled"
            );
            return None;
        }
        let mut stored_tag = [0u8; 32];
        stored_tag.copy_from_slice(&bytes[16..48]);
        let artifact = &bytes[ENTRY_HEADER_BYTES..];
        // blake3::Hash's equality is constant-time; compare through it.
        if blake3::Hash::from(stored_tag) != blake3::Hash::from(entry_tag(key, artifact)) {
            crate::kprintln!(
                "storedisk: cached entry failed integrity verification (keyed tag mismatch); \
                 it will be recompiled"
            );
            return None;
        }
        Some(artifact.to_vec())
    })
}

/// Store a freshly compiled artifact (header + keyed tag + bytes). Failures are printed and
/// otherwise ignored — the composition still runs from the in-memory result.
pub fn store(key: &str, artifact: &[u8]) {
    if artifact.len() as u64 > MAX_ARTIFACT_BYTES {
        crate::kprintln!(
            "storedisk: not caching a {} byte artifact (over the {} MiB cap)",
            artifact.len(),
            MAX_ARTIFACT_BYTES / (1024 * 1024)
        );
        return;
    }
    STORE.with(|slot| {
        let Some(mounted) = slot.as_mut() else {
            return;
        };
        let path = format!("{CACHE_DIR}/{key}.cwasm");
        let mut entry = Vec::with_capacity(ENTRY_HEADER_BYTES + artifact.len());
        entry.extend_from_slice(ENTRY_MAGIC);
        entry.extend_from_slice(&(artifact.len() as u64).to_le_bytes());
        entry.extend_from_slice(&entry_tag(key, artifact));
        entry.extend_from_slice(artifact);
        let result = (|| -> Result<(), eofs_core::FsError> {
            if mounted.fs.stat(CACHE_DIR).is_err() {
                mounted.fs.mkdir(CACHE_DIR)?;
            }
            // Replace any existing entry wholesale so a shorter rewrite can never leave
            // stale tail bytes behind the verified region.
            if mounted.fs.stat(&path).is_ok() {
                mounted.fs.remove(&path)?;
            }
            mounted.fs.create_file(&path)?;
            mounted.fs.write(&path, 0, &entry)?;
            mounted.fs.commit()?;
            Ok(())
        })();
        match result {
            Ok(()) => crate::kprintln!(
                "storedisk: cached {} KiB of compiled code as {}…",
                artifact.len() / 1024,
                &key[..16.min(key.len())]
            ),
            Err(error) => {
                crate::kprintln!("storedisk: caching the compiled artifact failed: {error:?}");
            }
        }
    });
}
