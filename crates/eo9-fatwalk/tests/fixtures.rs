//! Host tests against mtools-built FAT32 fixture images (the load-as-test-input
//! discipline: the filesystems under test are built by a FOREIGN implementation —
//! GNU mtools — not by this crate, so agreement is evidence).
//!
//! Needs mformat/mcopy/mdel/mdir on PATH (`brew install mtools`); a missing binary
//! fails with that instruction, deliberately — the plan pins fatwalk to mtools-built
//! fixtures, so silently skipping would gut the battery.
//!
//! Coverage per the usb-msd-plan L4 contract: multiple cluster sizes, a fragmented
//! chain (mcopy after a deletion), a chain whose FAT entries span FAT-sector
//! boundaries, byte-range mapping, the same-size write plan (applied bytes verified
//! back through mtools, FAT and directory regions pinned untouched), and the typed
//! refusals (FAT16, missing file, size mismatches).

use std::path::{Path, PathBuf};
use std::process::Command;

use eo9_fatwalk::{Error, FatError, SECTOR, SectorRead, Volume};

/// The whole fixture image in memory, doubling as the device.
struct ImageDisk {
    bytes: Vec<u8>,
}

impl SectorRead for ImageDisk {
    type Error = String;
    fn read_sector(&mut self, lba: u64, out: &mut [u8; SECTOR]) -> Result<(), String> {
        let at = lba as usize * SECTOR;
        let end = at + SECTOR;
        if end > self.bytes.len() {
            return Err(format!("read past the image: lba {lba}"));
        }
        out.copy_from_slice(&self.bytes[at..end]);
        Ok(())
    }
}

fn mtools(tool: &str, args: &[&str]) -> Vec<u8> {
    let output = Command::new(tool)
        .args(args)
        .output()
        .unwrap_or_else(|err| {
            panic!(
                "could not run `{tool}` ({err}) — the fatwalk fixtures are mtools-built; \
             install mtools (`brew install mtools`) and re-run"
            )
        });
    assert!(
        output.status.success(),
        "`{tool} {}` failed:\n{}",
        args.join(" "),
        String::from_utf8_lossy(&output.stderr)
    );
    output.stdout
}

/// A fresh FAT32 image at `path`: `total_sectors` sectors, `spc` sectors per
/// cluster. Geometry h=16 s=32; `-F` forces FAT32 (mtools allows it below the spec's
/// cluster minimum, which keeps the fixtures small).
fn mkfat32(path: &Path, total_sectors: u32, spc: u32) {
    let _ = std::fs::remove_file(path);
    let image = path.to_str().unwrap();
    mtools(
        "mformat",
        &[
            "-C",
            "-i",
            image,
            "-T",
            &total_sectors.to_string(),
            "-h",
            "16",
            "-s",
            "32",
            "-c",
            &spc.to_string(),
            "-F",
            "::",
        ],
    );
}

fn put(image: &Path, source: &Path, name: &str) {
    mtools(
        "mcopy",
        &[
            "-i",
            image.to_str().unwrap(),
            source.to_str().unwrap(),
            &format!("::{name}"),
        ],
    );
}

fn del(image: &Path, name: &str) {
    mtools(
        "mdel",
        &["-i", image.to_str().unwrap(), &format!("::{name}")],
    );
}

/// Deterministic patterned bytes (seeded; no clock, no rng dependency).
fn pattern(seed: u32, len: usize) -> Vec<u8> {
    let mut state = seed.wrapping_mul(2654435761).wrapping_add(1);
    (0..len)
        .map(|_| {
            state = state.wrapping_mul(1664525).wrapping_add(1013904223);
            (state >> 24) as u8
        })
        .collect()
}

/// A scratch dir unique to this test process.
fn scratch(label: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("eo9-fatwalk-{}-{label}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("create scratch dir");
    dir
}

fn write_file(dir: &Path, name: &str, bytes: &[u8]) -> PathBuf {
    let path = dir.join(name);
    std::fs::write(&path, bytes).expect("write fixture file");
    path
}

fn load(path: &Path) -> ImageDisk {
    ImageDisk {
        bytes: std::fs::read(path).expect("read fixture image"),
    }
}

/// Read a located file's byte range out of the image via the crate's run mapping.
fn read_range(
    volume: &Volume,
    disk: &ImageDisk,
    map: &eo9_fatwalk::FileMap,
    offset: u64,
    len: u64,
) -> Vec<u8> {
    let runs = volume.runs(map, offset, len).expect("runs");
    let mut covered = Vec::new();
    for run in &runs {
        let at = run.lba as usize * SECTOR;
        covered.extend_from_slice(&disk.bytes[at..at + run.sectors as usize * SECTOR]);
    }
    // The runs cover whole sectors starting at the first run's (sector-aligned)
    // file_offset; slice the requested bytes out.
    let skip = (offset - runs[0].file_offset) as usize;
    covered[skip..skip + len as usize].to_vec()
}

/// Locate + read a whole file and compare against the source bytes, for one cluster
/// size. Exercised at 512 B and 4 KiB clusters — the byte→cluster→LBA arithmetic
/// must not depend on the cluster size.
fn roundtrip_at_cluster_size(spc: u32) {
    let dir = scratch(&format!("roundtrip-c{spc}"));
    let image = dir.join("fat32.img");
    mkfat32(&image, 131072, spc);

    // 300_000 bytes: not cluster-aligned (a tail-slack file), > 1 cluster at every
    // tested size.
    let content = pattern(7 + spc, 300_000);
    let source = write_file(&dir, "eo9.img", &content);
    put(&image, &source, "EO9.IMG");

    let mut disk = load(&image);
    let volume = Volume::open(&mut disk).expect("FAT32 volume");
    assert_eq!(volume.cluster_bytes(), spc * SECTOR as u32);
    let map = volume.locate(&mut disk, "EO9.IMG").expect("locate EO9.IMG");
    assert_eq!(map.size as usize, content.len());
    assert_eq!(
        map.chain.len() as u64,
        (content.len() as u64).div_ceil(u64::from(volume.cluster_bytes()))
    );

    // Whole file.
    assert_eq!(
        read_range(&volume, &disk, &map, 0, map.size as u64),
        content
    );
    // An unaligned interior range (crosses sector and cluster boundaries).
    assert_eq!(
        read_range(&volume, &disk, &map, 1234, 70_000),
        &content[1234..1234 + 70_000]
    );
    // The unaligned tail.
    let tail = map.size as u64 - 999;
    assert_eq!(
        read_range(&volume, &disk, &map, tail, 999),
        &content[tail as usize..]
    );
}

#[test]
fn roundtrip_512_byte_clusters() {
    roundtrip_at_cluster_size(1);
}

#[test]
fn roundtrip_4_kib_clusters() {
    roundtrip_at_cluster_size(8);
}

/// Force fragmentation by leaving NO contiguous run big enough: nearly fill a small
/// volume with four files, delete two non-adjacent ones, then copy a file larger
/// than any single hole — wherever mtools' allocator starts, the chain must split
/// across holes. (A naive copy-after-one-deletion fixture came out contiguous:
/// mtools does not first-fit from the lowest free cluster.) The test asserts the
/// split actually happened (a fixture that silently came out contiguous would prove
/// nothing) and that chain-order reads still reassemble the file.
#[test]
fn fragmented_chain_reads_in_chain_order() {
    let dir = scratch("fragmented");
    let image = dir.join("fat32.img");
    // 8 MiB volume, 512 B clusters: data region ~7.8 MiB.
    mkfat32(&image, 16384, 1);

    let f1 = write_file(&dir, "f1.bin", &pattern(1, 2048 * 1024));
    let f2 = write_file(&dir, "f2.bin", &pattern(2, 2048 * 1024));
    let f3 = write_file(&dir, "f3.bin", &pattern(5, 2048 * 1024));
    let f4 = write_file(&dir, "f4.bin", &pattern(6, 1536 * 1024));
    put(&image, &f1, "F1.BIN");
    put(&image, &f2, "F2.BIN");
    put(&image, &f3, "F3.BIN");
    put(&image, &f4, "F4.BIN");
    del(&image, "F1.BIN");
    del(&image, "F3.BIN");
    // 3.5 MiB: bigger than either 2 MiB hole and bigger than the tail slack.
    let content = pattern(3, 3584 * 1024);
    let c = write_file(&dir, "c.bin", &content);
    put(&image, &c, "C.BIN");

    let mut disk = load(&image);
    let volume = Volume::open(&mut disk).expect("FAT32 volume");
    let map = volume.locate(&mut disk, "C.BIN").expect("locate C.BIN");
    let breaks = map
        .chain
        .windows(2)
        .filter(|pair| pair[1] != pair[0] + 1)
        .count();
    assert!(
        breaks >= 1,
        "fixture failed to fragment: C.BIN's chain is contiguous ({} clusters)",
        map.chain.len()
    );
    assert_eq!(
        read_range(&volume, &disk, &map, 0, map.size as u64),
        content
    );

    // The write plan respects the fragmentation: more than one op, ops in chain
    // order, all inside the file's own clusters.
    let new_content = pattern(4, 3584 * 1024);
    let plan = volume.write_plan(&map, &new_content).expect("write plan");
    assert!(
        plan.len() > 1,
        "a fragmented chain must yield multiple write ops"
    );
    let file_lbas: std::collections::BTreeSet<u64> = map
        .chain
        .iter()
        .flat_map(|&cluster| {
            let first = volume.cluster_lba(cluster);
            (0..u64::from(volume.cluster_bytes()) / SECTOR as u64).map(move |s| first + s)
        })
        .collect();
    for op in &plan {
        for s in 0..(op.data.len() / SECTOR) as u64 {
            assert!(
                file_lbas.contains(&(op.lba + s)),
                "write op at lba {} strays outside the file's clusters",
                op.lba + s
            );
        }
    }
}

/// A chain long enough that its FAT entries span several FAT sectors (128 entries
/// per sector at 4 bytes each): 400 clusters at 512 B/cluster crosses two FAT-sector
/// boundaries; the walk's one-sector cache must follow.
#[test]
fn chain_spanning_fat_sector_boundaries() {
    let dir = scratch("fatspan");
    let image = dir.join("fat32.img");
    mkfat32(&image, 131072, 1);

    let content = pattern(9, 400 * 512);
    let source = write_file(&dir, "span.bin", &content);
    put(&image, &source, "SPAN.BIN");

    let mut disk = load(&image);
    let volume = Volume::open(&mut disk).expect("FAT32 volume");
    let map = volume
        .locate(&mut disk, "SPAN.BIN")
        .expect("locate SPAN.BIN");
    assert_eq!(map.chain.len(), 400);
    let entries_per_fat_sector = 128u32;
    let first_fat_sector = map.chain.first().unwrap() / entries_per_fat_sector;
    let last_fat_sector = map.chain.last().unwrap() / entries_per_fat_sector;
    assert!(
        last_fat_sector > first_fat_sector,
        "fixture failed: the chain's FAT entries all sit in one FAT sector"
    );
    assert_eq!(
        read_range(&volume, &disk, &map, 0, map.size as u64),
        content
    );
}

/// The flasher's whole move: same-size overwrite through the existing chain, FAT and
/// directory bytes pinned untouched, and the result read back by mtools (the foreign
/// implementation agrees the file now carries the new bytes).
#[test]
fn same_size_overwrite_via_write_plan() {
    let dir = scratch("overwrite");
    let image = dir.join("fat32.img");
    mkfat32(&image, 131072, 8);

    // A slot-shaped file: 2 MiB, sector- and cluster-aligned (the fixed-slot
    // discipline in miniature).
    let slot = 2 * 1024 * 1024;
    let old = pattern(20, slot);
    let source = write_file(&dir, "eo9.img", &old);
    put(&image, &source, "EO9.IMG");
    // A bystander file that must survive untouched.
    let other = pattern(21, 50_000);
    let other_src = write_file(&dir, "boot.scr", &other);
    put(&image, &other_src, "BOOT.SCR");

    let mut disk = load(&image);
    let volume = Volume::open(&mut disk).expect("FAT32 volume");
    let map = volume.locate(&mut disk, "EO9.IMG").expect("locate EO9.IMG");

    // The non-data region (reserved + both FATs) plus the root directory cluster
    // must not change; snapshot before.
    let data_start_lba = volume.cluster_lba(2) as usize * SECTOR;
    let metadata_before = disk.bytes[..data_start_lba].to_vec();

    let new = pattern(22, slot);
    let plan = volume.write_plan(&map, &new).expect("write plan");
    for op in &plan {
        let at = op.lba as usize * SECTOR;
        disk.bytes[at..at + op.data.len()].copy_from_slice(op.data);
    }
    std::fs::write(&image, &disk.bytes).expect("write image back");

    // Nothing before the data region changed: no FAT writes, no reserved-area
    // writes, by construction.
    assert_eq!(
        metadata_before,
        &disk.bytes[..data_start_lba],
        "the write plan touched the reserved/FAT region"
    );

    // mtools reads the new content back and still lists both files.
    let out = dir.join("readback.bin");
    mtools(
        "mcopy",
        &[
            "-i",
            image.to_str().unwrap(),
            "::EO9.IMG",
            out.to_str().unwrap(),
        ],
    );
    assert_eq!(std::fs::read(&out).expect("readback"), new);
    let listing = String::from_utf8_lossy(&mtools("mdir", &["-i", image.to_str().unwrap(), "::"]))
        .to_string();
    assert!(
        listing.contains("EO9      IMG"),
        "mdir lost EO9.IMG:\n{listing}"
    );
    assert!(
        listing.contains("BOOT     SCR"),
        "mdir lost BOOT.SCR:\n{listing}"
    );
    let other_out = dir.join("other.bin");
    mtools(
        "mcopy",
        &[
            "-i",
            image.to_str().unwrap(),
            "::BOOT.SCR",
            other_out.to_str().unwrap(),
        ],
    );
    assert_eq!(
        std::fs::read(&other_out).expect("bystander readback"),
        other
    );
}

/// Typed refusals: a FAT16 image, a missing file, a directory-shaped name, and the
/// write plan's same-size/alignment discipline.
#[test]
fn typed_refusals() {
    let dir = scratch("refusals");

    // FAT16 (no -F on a small image): typed NotFat32, never a walk.
    let fat16 = dir.join("fat16.img");
    let _ = std::fs::remove_file(&fat16);
    mtools(
        "mformat",
        &[
            "-C",
            "-i",
            fat16.to_str().unwrap(),
            "-T",
            "8192",
            "-h",
            "4",
            "-s",
            "32",
            "-c",
            "4",
            "::",
        ],
    );
    let mut disk16 = load(&fat16);
    match Volume::open(&mut disk16) {
        Err(Error::Fat(FatError::NotFat32 { .. })) => {}
        other => panic!("FAT16 image must be a typed NotFat32 refusal, got {other:?}"),
    }

    // FAT32 with one file for the lookup/write refusals.
    let image = dir.join("fat32.img");
    mkfat32(&image, 131072, 1);
    let content = pattern(30, 1000); // deliberately NOT sector-aligned
    let source = write_file(&dir, "odd.bin", &content);
    put(&image, &source, "ODD.BIN");
    let mut disk = load(&image);
    let volume = Volume::open(&mut disk).expect("FAT32 volume");

    match volume.locate(&mut disk, "MISSING.IMG") {
        Err(Error::Fat(FatError::NotFound)) => {}
        other => panic!("expected NotFound, got {other:?}"),
    }
    match volume.locate(&mut disk, "not/a.name") {
        Err(Error::Fat(FatError::BadName)) => {}
        other => panic!("expected BadName, got {other:?}"),
    }

    let map = volume.locate(&mut disk, "ODD.BIN").expect("locate ODD.BIN");
    // Wrong size: refused before alignment is even considered.
    assert_eq!(
        volume.write_plan(&map, &[0u8; 512]),
        Err(FatError::SizeMismatch {
            data: 512,
            file: 1000
        })
    );
    // Same size but not sector-aligned: refused (the tail would need RMW).
    assert_eq!(
        volume.write_plan(&map, &content),
        Err(FatError::UnalignedSize { size: 1000 })
    );
    // Byte ranges past the end: refused.
    assert_eq!(volume.runs(&map, 990, 11), Err(FatError::OutOfRange));
    assert_eq!(volume.runs(&map, 0, 0), Err(FatError::OutOfRange));
}
