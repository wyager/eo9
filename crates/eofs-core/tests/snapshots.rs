//! Snapshots are retained roots: isolated from later writes, listable, readable, and never
//! reclaimed by GC while they exist.

use eofs_core::{Eofs, FormatOptions, FsError, MemDevice, NodeKind};

const DEV_SIZE: u64 = 8 * 1024 * 1024;

fn fresh_fs() -> Eofs<MemDevice> {
    Eofs::format(MemDevice::new(DEV_SIZE), &FormatOptions::default()).unwrap()
}

fn read_all(fs: &Eofs<MemDevice>, path: &str) -> Vec<u8> {
    let size = fs.stat(path).unwrap().size as usize;
    let mut buf = vec![0u8; size];
    assert_eq!(fs.read(path, 0, &mut buf).unwrap(), size);
    buf
}

/// Deterministic, poorly compressible filler.
fn noise(len: usize) -> Vec<u8> {
    let mut state = 0x2545_f491_4f6c_dd1du64;
    let mut out = Vec::with_capacity(len);
    while out.len() < len {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        out.extend_from_slice(&state.to_le_bytes());
    }
    out.truncate(len);
    out
}

#[test]
fn snapshots_are_isolated_from_later_writes() {
    let mut fs = fresh_fs();
    fs.mkdir("/dir").unwrap();
    fs.create_file("/dir/f").unwrap();
    fs.write("/dir/f", 0, b"version one").unwrap();
    fs.snapshot_create("s1").unwrap();

    fs.write("/dir/f", 0, b"VERSION TWO").unwrap();
    fs.create_file("/later").unwrap();
    fs.remove("/later").unwrap();
    fs.create_file("/dir/g").unwrap();
    fs.commit().unwrap();

    let list = fs.snapshot_list().unwrap();
    assert_eq!(list.len(), 1);
    assert_eq!(list[0].name, "s1");
    assert_eq!(list[0].txg, 2);

    // The live tree moved on; the snapshot did not.
    assert_eq!(read_all(&fs, "/dir/f"), b"VERSION TWO");
    let snap = fs.snapshot("s1").unwrap();
    let mut buf = vec![0u8; 64];
    let n = snap.read("/dir/f", 0, &mut buf).unwrap();
    assert_eq!(&buf[..n], b"version one");
    assert_eq!(snap.list("/dir").unwrap(), vec!["f"]);
    assert_eq!(snap.stat("/dir/g"), Err(FsError::NotFound));
    assert_eq!(snap.stat("/dir").unwrap().kind, NodeKind::Directory);

    // Still true after a remount, and verify() covers the snapshot tree too.
    let fs = Eofs::mount(fs.unmount()).unwrap();
    let report = fs.verify().unwrap();
    assert_eq!(report.snapshots, 1);
    let snap = fs.snapshot("s1").unwrap();
    let n = snap.read("/dir/f", 0, &mut buf).unwrap();
    assert_eq!(&buf[..n], b"version one");
    assert_eq!(read_all(&fs, "/dir/f"), b"VERSION TWO");
}

#[test]
fn snapshot_names_are_unique_and_looked_up() {
    let mut fs = fresh_fs();
    fs.snapshot_create("s1").unwrap();
    assert_eq!(fs.snapshot_create("s1"), Err(FsError::AlreadyExists));
    assert_eq!(fs.snapshot_create("bad/name"), Err(FsError::InvalidPath));
    assert!(fs.snapshot("missing").is_err());
    fs.snapshot_create("s2").unwrap();
    fs.commit().unwrap();
    let names: Vec<String> = fs
        .snapshot_list()
        .unwrap()
        .into_iter()
        .map(|snap| snap.name)
        .collect();
    assert_eq!(names, vec!["s1", "s2"]);
}

#[test]
fn uncommitted_snapshots_vanish_on_remount() {
    let mut fs = fresh_fs();
    fs.snapshot_create("volatile").unwrap();
    let fs = Eofs::mount(fs.unmount()).unwrap();
    assert!(fs.snapshot_list().unwrap().is_empty());
}

#[test]
fn gc_never_reclaims_snapshot_data() {
    let mut fs = fresh_fs();
    let big = noise(16 * 4096);
    fs.create_file("/big").unwrap();
    fs.write("/big", 0, &big).unwrap();
    fs.snapshot_create("keep").unwrap();
    fs.commit().unwrap();

    fs.remove("/big").unwrap();
    fs.commit().unwrap();
    let report = fs.gc().unwrap();
    // The file's blocks are still pinned by the snapshot; only superseded directory and
    // snapshot-table versions can be reclaimed.
    assert!(report.reclaimed_bytes < 8 * 4096);

    // Fill reusable space with new data; the snapshot must be untouched.
    fs.create_file("/new").unwrap();
    fs.write("/new", 0, &noise(8 * 4096)).unwrap();
    fs.commit().unwrap();

    let snap = fs.snapshot("keep").unwrap();
    let mut buf = vec![0u8; big.len()];
    assert_eq!(snap.read("/big", 0, &mut buf).unwrap(), big.len());
    assert_eq!(buf, big);
    fs.verify().unwrap();
    assert_eq!(fs.stat("/big"), Err(FsError::NotFound));
}

#[test]
fn gc_reclaims_unreferenced_space_for_reuse() {
    let mut fs = fresh_fs();
    let data = noise(16 * 4096);
    fs.create_file("/scratch").unwrap();
    fs.write("/scratch", 0, &data).unwrap();
    fs.commit().unwrap();

    fs.remove("/scratch").unwrap();
    fs.commit().unwrap();

    let before = fs.space();
    let report = fs.gc().unwrap();
    assert!(report.reclaimed_bytes >= data.len() as u64);
    assert_eq!(fs.space().free_bytes, report.reclaimed_bytes);

    // A same-sized file now fits entirely into reclaimed space: the frontier stays put.
    fs.create_file("/again").unwrap();
    fs.write("/again", 0, &data).unwrap();
    fs.commit().unwrap();
    assert_eq!(fs.space().frontier, before.frontier);
    assert_eq!(read_all(&fs, "/again"), data);
    fs.verify().unwrap();

    // And the filesystem still remounts cleanly with reused extents in place.
    let fs = Eofs::mount(fs.unmount()).unwrap();
    assert_eq!(read_all(&fs, "/again"), data);
    fs.verify().unwrap();
}
