//! Hostile-image robustness: deliberately corrupted or adversarial images must fail with a
//! clean `Corrupt`-style error — never an out-of-memory, a stack overflow, or a hang.
//!
//! The forging helpers below build on-disk structures by hand following `FORMAT.md`
//! (uberblock layout, block pointers, object references, directory entries), which also
//! doubles as a check that the documented layout matches the implementation.

use eofs_core::{Eofs, FormatOptions, FsError, MemDevice};

const SLOT_SIZE: usize = 4096;
const DATA_START: u64 = 8192;
const ALLOC_UNIT: u64 = 512;

// --- forging helpers (layout per FORMAT.md) --------------------------------------------

/// Offset of the uberblock slot with the highest transaction number.
fn newest_slot(image: &[u8]) -> usize {
    let txg = |slot: usize| -> u64 {
        if &image[slot..slot + 8] != b"EOFS-UB\0" {
            return 0;
        }
        u64::from_le_bytes(image[slot + 24..slot + 32].try_into().unwrap())
    };
    if txg(0) >= txg(SLOT_SIZE) {
        0
    } else {
        SLOT_SIZE
    }
}

/// Recompute the uberblock checksum after patching its fields.
fn fix_checksum(image: &mut [u8], slot: usize) {
    let checksum = blake3::hash(&image[slot..slot + 192]);
    image[slot + 192..slot + 224].copy_from_slice(checksum.as_bytes());
}

/// A 56-byte block pointer.
fn block_ptr(addr: u64, len: u32, hash: [u8; 32]) -> [u8; 56] {
    let mut ptr = [0u8; 56];
    ptr[0..8].copy_from_slice(&addr.to_le_bytes());
    ptr[8..12].copy_from_slice(&len.to_le_bytes()); // logical size
    ptr[12..16].copy_from_slice(&len.to_le_bytes()); // physical size (raw codec)
    ptr[16] = 0; // codec: raw
    ptr[24..56].copy_from_slice(&hash);
    ptr
}

/// A 72-byte object reference.
fn obj_ref(size: u64, level: u8, ptr: [u8; 56]) -> [u8; 72] {
    let mut obj = [0u8; 72];
    obj[0..8].copy_from_slice(&size.to_le_bytes());
    obj[8] = level;
    obj[16..72].copy_from_slice(&ptr);
    obj
}

/// A serialized directory entry.
fn dir_entry(name: &str, kind: u8, obj: [u8; 72]) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(&(name.len() as u16).to_le_bytes());
    out.push(kind);
    out.push(0);
    out.extend_from_slice(&obj);
    out.extend_from_slice(name.as_bytes());
    out
}

/// Write a raw (uncompressed) block into the image at `addr` and return an object reference
/// describing it as a single-block object.
fn forge_block(image: &mut [u8], addr: u64, bytes: &[u8]) -> [u8; 72] {
    image[addr as usize..addr as usize + bytes.len()].copy_from_slice(bytes);
    let ptr = block_ptr(addr, bytes.len() as u32, *blake3::hash(bytes).as_bytes());
    obj_ref(bytes.len() as u64, 0, ptr)
}

/// Point the newest uberblock's live root at `obj` (72 bytes at slot offset 48).
fn set_live_root(image: &mut [u8], obj: [u8; 72]) {
    let slot = newest_slot(image);
    image[slot + 48..slot + 120].copy_from_slice(&obj);
    fix_checksum(image, slot);
}

/// Patch only the size and level of the newest uberblock's live root.
fn patch_live_root(image: &mut [u8], size: u64, level: u8) {
    let slot = newest_slot(image);
    image[slot + 48..slot + 56].copy_from_slice(&size.to_le_bytes());
    image[slot + 56] = level;
    fix_checksum(image, slot);
}

// --- base images -------------------------------------------------------------------------

/// A normal little filesystem: a directory and two files under the root.
fn small_image() -> Vec<u8> {
    let mut fs = Eofs::format(MemDevice::new(8 * 1024 * 1024), &FormatOptions::default()).unwrap();
    fs.mkdir("/dir").unwrap();
    fs.create_file("/dir/inner").unwrap();
    fs.write("/dir/inner", 0, b"inner file contents").unwrap();
    fs.create_file("/top").unwrap();
    fs.write("/top", 0, b"top-level file").unwrap();
    fs.commit().unwrap();
    fs.unmount().into_vec()
}

/// A filesystem whose root directory spans three blocks (so its object has an indirect
/// block with three children).
fn wide_root_image() -> Vec<u8> {
    let mut fs = Eofs::format(MemDevice::new(8 * 1024 * 1024), &FormatOptions::default()).unwrap();
    for i in 0..140 {
        fs.create_file(&format!("/file-{i:04}-padding-name"))
            .unwrap();
    }
    fs.commit().unwrap();
    assert!(
        fs.stat("/").unwrap().size > 2 * 4096,
        "root directory is not wide enough"
    );
    fs.unmount().into_vec()
}

// --- 1: oversized object references -------------------------------------------------------

#[test]
fn oversized_object_size_is_rejected() {
    let mut image = small_image();
    patch_live_root(&mut image, u64::MAX / 2, 0);

    let fs = Eofs::mount(MemDevice::from_vec(image)).unwrap();
    assert!(matches!(fs.list("/"), Err(FsError::Corrupt(_))));
    assert!(matches!(fs.verify(), Err(FsError::Corrupt(_))));
}

#[test]
fn metadata_object_over_the_cap_is_rejected() {
    let mut image = small_image();
    // 20 MiB is structurally plausible for the device, but far beyond the metadata cap.
    patch_live_root(&mut image, 20 * 1024 * 1024, 2);

    let fs = Eofs::mount(MemDevice::from_vec(image)).unwrap();
    assert!(matches!(fs.list("/"), Err(FsError::Corrupt(_))));
    assert!(fs.verify().is_err());
}

// --- 2: inflated level / fan-out -----------------------------------------------------------

#[test]
fn inflated_object_level_is_rejected() {
    let mut image = small_image();
    let slot = newest_slot(&image);
    let size = u64::from_le_bytes(image[slot + 48..slot + 56].try_into().unwrap());
    patch_live_root(&mut image, size, 200);

    let fs = Eofs::mount(MemDevice::from_vec(image)).unwrap();
    assert!(matches!(fs.list("/"), Err(FsError::Corrupt(_))));
    assert!(matches!(fs.verify(), Err(FsError::Corrupt(_))));
}

#[test]
fn fanout_beyond_the_declared_size_is_rejected() {
    let mut image = wide_root_image();
    // Shrink the root directory's declared size from three blocks to two: the walk must
    // stop as soon as it meets the third data block instead of trusting the tree's fan-out.
    patch_live_root(&mut image, 2 * 4096, 1);

    let fs = Eofs::mount(MemDevice::from_vec(image)).unwrap();
    assert!(matches!(fs.list("/"), Err(FsError::Corrupt(_))));
    assert!(matches!(fs.verify(), Err(FsError::Corrupt(_))));
}

// --- 3: deep and cyclic directory structures ----------------------------------------------

#[test]
fn very_deep_directory_chains_do_not_overflow_the_stack() {
    const DEPTH: u64 = 50_000;
    let device_size = DATA_START + (DEPTH + 16) * ALLOC_UNIT;
    let fs = Eofs::format(MemDevice::new(device_size), &FormatOptions::default()).unwrap();
    let mut image = fs.unmount().into_vec();

    // Forge a 50,000-deep chain of single-entry directories, innermost first.
    let mut child = [0u8; 72]; // the empty object: an empty directory
    for i in 0..DEPTH {
        let entry = dir_entry("d", 2, child);
        child = forge_block(&mut image, DATA_START + i * ALLOC_UNIT, &entry);
    }
    set_live_root(&mut image, child);

    let mut fs = Eofs::mount(MemDevice::from_vec(image)).unwrap();
    let report = fs.verify().unwrap();
    assert_eq!(report.directories, DEPTH + 1);
    assert_eq!(fs.list("/").unwrap(), vec!["d"]);
    fs.gc().unwrap();
}

#[test]
fn repeated_directory_references_are_rejected() {
    // Each forged directory holds two entries pointing at the *same* child: without a
    // visited set this walk would visit 2^40 directories. It must fail fast instead.
    const DEPTH: u64 = 40;
    let fs = Eofs::format(MemDevice::new(1024 * 1024), &FormatOptions::default()).unwrap();
    let mut image = fs.unmount().into_vec();

    let leaf = dir_entry("f", 1, [0u8; 72]);
    let mut child = forge_block(&mut image, DATA_START, &leaf);
    for i in 1..=DEPTH {
        let mut twins = dir_entry("a", 2, child);
        twins.extend_from_slice(&dir_entry("b", 2, child));
        child = forge_block(&mut image, DATA_START + i * ALLOC_UNIT, &twins);
    }
    set_live_root(&mut image, child);

    let mut fs = Eofs::mount(MemDevice::from_vec(image)).unwrap();
    assert!(matches!(fs.verify(), Err(FsError::Corrupt(_))));
    assert!(matches!(fs.gc(), Err(FsError::Corrupt(_))));
}
