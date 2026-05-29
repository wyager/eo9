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
//! `Component::deserialize`, exactly like the baked-in store image. The disk is attached by
//! the operator and is granted to the kernel only via the explicit boot token, so it sits in
//! the same trust class as the kernel image itself; eofs's block checksums catch corruption,
//! and an artifact wasmtime cannot validate falls back to a fresh compile. Durability note:
//! the in-kernel driver does not negotiate VIRTIO_BLK_F_FLUSH yet (same as the wasm driver);
//! eofs commits are ordered but a host power cut may lose the most recent ones.

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

/// Look up a cached artifact. `None` on any miss or error (errors are printed, never fatal).
pub fn lookup(key: &str) -> Option<Vec<u8>> {
    STORE.with(|slot| {
        let mounted = slot.as_mut()?;
        let path = format!("{CACHE_DIR}/{key}.cwasm");
        let stat = mounted.fs.stat(&path).ok()?;
        if stat.size == 0 || stat.size > MAX_ARTIFACT_BYTES {
            return None;
        }
        let mut bytes = alloc::vec![0u8; stat.size as usize];
        match mounted.fs.read(&path, 0, &mut bytes) {
            Ok(read) if read as u64 == stat.size => Some(bytes),
            Ok(_) => None,
            Err(error) => {
                crate::kprintln!("storedisk: reading a cached artifact failed: {error:?}");
                None
            }
        }
    })
}

/// Store a freshly compiled artifact. Failures are printed and otherwise ignored — the
/// composition still runs from the in-memory result.
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
        let result = (|| -> Result<(), eofs_core::FsError> {
            if mounted.fs.stat(CACHE_DIR).is_err() {
                mounted.fs.mkdir(CACHE_DIR)?;
            }
            if mounted.fs.stat(&path).is_err() {
                mounted.fs.create_file(&path)?;
            }
            mounted.fs.write(&path, 0, artifact)?;
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
