//! Round-trips of every milestone-1 operation: create, write, read, mkdir, list, stat,
//! remove, commit, remount — plus determinism of the produced image bytes.

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

/// Deterministic patterned bytes for multi-block files.
fn pattern(len: usize, seed: u8) -> Vec<u8> {
    (0..len)
        .map(|i| (i as u64 * 131 + seed as u64 * 7 + i as u64 / 4096) as u8)
        .collect()
}

#[test]
fn empty_filesystem() {
    let fs = fresh_fs();
    assert_eq!(fs.txg(), 1);
    assert!(!fs.is_dirty());
    assert_eq!(fs.block_size(), 4096);
    assert!(fs.compression());
    assert_eq!(fs.list("/").unwrap(), Vec::<String>::new());
    let root = fs.stat("/").unwrap();
    assert_eq!(root.kind, NodeKind::Directory);
    assert_eq!(root.size, 0);
    assert_eq!(root.hash, [0u8; 32]);
    let report = fs.verify().unwrap();
    assert_eq!(report.blocks, 0);
    assert_eq!(report.directories, 1);

    let fs = Eofs::mount(fs.unmount()).unwrap();
    assert_eq!(fs.txg(), 1);
    assert_eq!(fs.list("/").unwrap(), Vec::<String>::new());
}

#[test]
fn files_and_directories_roundtrip() {
    let mut fs = fresh_fs();
    fs.mkdir("/etc").unwrap();
    fs.mkdir("/etc/app").unwrap();
    fs.create_file("/etc/app/config").unwrap();
    fs.write("/etc/app/config", 0, b"key = value\n").unwrap();
    fs.create_file("/readme").unwrap();
    fs.write("/readme", 0, b"hello eofs").unwrap();
    assert_eq!(fs.commit().unwrap(), 2);
    assert!(!fs.is_dirty());

    assert_eq!(fs.list("/").unwrap(), vec!["etc", "readme"]);
    assert_eq!(fs.list("/etc").unwrap(), vec!["app"]);
    assert_eq!(fs.list("/etc/app").unwrap(), vec!["config"]);
    assert_eq!(read_all(&fs, "/etc/app/config"), b"key = value\n");
    assert_eq!(read_all(&fs, "/readme"), b"hello eofs");

    let stat = fs.stat("/etc/app/config").unwrap();
    assert_eq!(stat.kind, NodeKind::File);
    assert_eq!(stat.size, 12);
    assert_ne!(stat.hash, [0u8; 32]);
    assert_eq!(fs.stat("/etc").unwrap().kind, NodeKind::Directory);

    fs.verify().unwrap();

    // Everything survives a remount from the raw image bytes.
    let fs = Eofs::mount(fs.unmount()).unwrap();
    assert_eq!(fs.txg(), 2);
    assert_eq!(fs.list("/").unwrap(), vec!["etc", "readme"]);
    assert_eq!(read_all(&fs, "/etc/app/config"), b"key = value\n");
    assert_eq!(read_all(&fs, "/readme"), b"hello eofs");
    fs.verify().unwrap();
}

#[test]
fn multi_block_files_and_indirect_trees() {
    let mut fs = fresh_fs();
    // 80 blocks + a partial tail: forces two levels of indirect blocks (fanout is 73).
    let data = pattern(80 * 4096 + 123, 3);
    fs.create_file("/big").unwrap();
    fs.write("/big", 0, &data).unwrap();
    fs.commit().unwrap();

    assert_eq!(fs.stat("/big").unwrap().size, data.len() as u64);
    assert_eq!(read_all(&fs, "/big"), data);

    // Unaligned reads across block boundaries.
    let mut buf = vec![0u8; 10_000];
    assert_eq!(fs.read("/big", 4000, &mut buf).unwrap(), 10_000);
    assert_eq!(buf, data[4000..14_000]);
    // Reads at and past the end are short or empty.
    assert_eq!(fs.read("/big", data.len() as u64 - 5, &mut buf).unwrap(), 5);
    assert_eq!(fs.read("/big", data.len() as u64 + 7, &mut buf).unwrap(), 0);

    let fs = Eofs::mount(fs.unmount()).unwrap();
    assert_eq!(read_all(&fs, "/big"), data);
    fs.verify().unwrap();
}

#[test]
fn overwrite_extend_and_sparse_gap() {
    let mut fs = fresh_fs();
    fs.create_file("/f").unwrap();
    fs.write("/f", 0, b"hello world").unwrap();
    fs.write("/f", 6, b"eofs!").unwrap();
    assert_eq!(read_all(&fs, "/f"), b"hello eofs!");

    // Writing past the end zero-fills the gap.
    fs.write("/f", 10_000, b"tail").unwrap();
    let content = read_all(&fs, "/f");
    assert_eq!(content.len(), 10_004);
    assert_eq!(&content[..11], b"hello eofs!");
    assert!(content[11..10_000].iter().all(|&b| b == 0));
    assert_eq!(&content[10_000..], b"tail");

    fs.commit().unwrap();
    let fs = Eofs::mount(fs.unmount()).unwrap();
    assert_eq!(read_all(&fs, "/f"), content);
    fs.verify().unwrap();
}

#[test]
fn remove_files_and_directories() {
    let mut fs = fresh_fs();
    fs.mkdir("/dir").unwrap();
    fs.create_file("/dir/f").unwrap();
    fs.write("/dir/f", 0, b"data").unwrap();
    fs.commit().unwrap();

    assert_eq!(fs.remove("/dir"), Err(FsError::DirectoryNotEmpty));
    fs.remove("/dir/f").unwrap();
    assert_eq!(fs.read("/dir/f", 0, &mut [0u8; 4]), Err(FsError::NotFound));
    fs.remove("/dir").unwrap();
    assert_eq!(fs.list("/").unwrap(), Vec::<String>::new());
    fs.commit().unwrap();

    let fs = Eofs::mount(fs.unmount()).unwrap();
    assert_eq!(fs.list("/").unwrap(), Vec::<String>::new());
    fs.verify().unwrap();
}

#[test]
fn operation_errors() {
    let mut fs = fresh_fs();
    fs.create_file("/f").unwrap();
    fs.mkdir("/d").unwrap();

    assert_eq!(fs.create_file("/f"), Err(FsError::AlreadyExists));
    assert_eq!(fs.mkdir("/f"), Err(FsError::AlreadyExists));
    assert_eq!(fs.create_file("/missing/x"), Err(FsError::NotFound));
    assert_eq!(fs.create_file("/f/x"), Err(FsError::NotADirectory));
    assert_eq!(fs.list("/f"), Err(FsError::NotADirectory));
    assert_eq!(fs.write("/d", 0, b"x"), Err(FsError::IsADirectory));
    assert_eq!(fs.read("/d", 0, &mut [0u8; 1]), Err(FsError::IsADirectory));
    assert_eq!(fs.write("/missing", 0, b"x"), Err(FsError::NotFound));
    assert_eq!(fs.stat("/missing"), Err(FsError::NotFound));
    assert_eq!(fs.remove("/missing"), Err(FsError::NotFound));

    assert_eq!(fs.create_file("/"), Err(FsError::InvalidPath));
    assert_eq!(fs.remove("/"), Err(FsError::InvalidPath));
    assert_eq!(fs.create_file("/.."), Err(FsError::InvalidPath));
    assert_eq!(fs.mkdir("/a/./b"), Err(FsError::InvalidPath));
    assert_eq!(
        fs.stat(&format!("/{}", "x".repeat(300))),
        Err(FsError::InvalidPath)
    );
}

#[test]
fn uncommitted_changes_are_dropped_on_remount() {
    let mut fs = fresh_fs();
    fs.create_file("/durable").unwrap();
    fs.write("/durable", 0, b"v1").unwrap();
    fs.commit().unwrap();

    fs.write("/durable", 0, b"v2").unwrap();
    fs.create_file("/ephemeral").unwrap();
    assert!(fs.is_dirty());

    // No commit: the remount sees the last committed transaction only.
    let fs = Eofs::mount(fs.unmount()).unwrap();
    assert_eq!(fs.txg(), 2);
    assert_eq!(read_all(&fs, "/durable"), b"v1");
    assert_eq!(fs.stat("/ephemeral"), Err(FsError::NotFound));
    fs.verify().unwrap();
}

#[test]
fn merkle_hashes_propagate_to_the_root() {
    let mut fs = fresh_fs();
    fs.mkdir("/a").unwrap();
    fs.mkdir("/b").unwrap();
    fs.create_file("/a/f").unwrap();
    fs.write("/a/f", 0, b"one").unwrap();
    fs.create_file("/b/g").unwrap();
    fs.write("/b/g", 0, b"untouched").unwrap();
    fs.commit().unwrap();

    let root_before = fs.stat("/").unwrap().hash;
    let a_before = fs.stat("/a").unwrap().hash;
    let b_before = fs.stat("/b").unwrap().hash;

    fs.write("/a/f", 0, b"two").unwrap();
    fs.commit().unwrap();

    // The change is visible in every ancestor's hash, and nowhere else.
    assert_ne!(fs.stat("/a/f").unwrap().hash, [0u8; 32]);
    assert_ne!(fs.stat("/a").unwrap().hash, a_before);
    assert_ne!(fs.stat("/").unwrap().hash, root_before);
    assert_eq!(fs.stat("/b").unwrap().hash, b_before);
}

#[test]
fn identical_operation_sequences_produce_identical_images() {
    let run = || {
        let mut fs = fresh_fs();
        fs.mkdir("/dir").unwrap();
        fs.create_file("/dir/a").unwrap();
        fs.write("/dir/a", 0, &pattern(10_000, 1)).unwrap();
        fs.commit().unwrap();
        fs.create_file("/b").unwrap();
        fs.write("/b", 100, b"offset write").unwrap();
        fs.snapshot_create("snap").unwrap();
        fs.remove("/dir/a").unwrap();
        fs.commit().unwrap();
        fs.unmount().into_vec()
    };
    assert_eq!(run(), run());
}

#[test]
fn no_op_commit_does_not_advance_the_transaction() {
    let mut fs = fresh_fs();
    assert_eq!(fs.commit().unwrap(), 1);
    fs.create_file("/f").unwrap();
    assert_eq!(fs.commit().unwrap(), 2);
    assert_eq!(fs.commit().unwrap(), 2);
}
