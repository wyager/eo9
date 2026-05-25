//! Hash verification: corrupting stored bytes is caught by `verify()` and by reads, and a
//! damaged uberblock slot falls back to the previous transaction (or fails the mount when
//! both slots are gone).

use eofs_core::{Eofs, FormatOptions, FsError, MemDevice};

const DEV_SIZE: u64 = 4 * 1024 * 1024;
const MARKER: &[u8] = b"EOFS-CORRUPTION-MARKER-";

fn marker_content() -> Vec<u8> {
    MARKER.iter().cycle().copied().take(3 * 4096).collect()
}

/// A formatted image (compression off, so file content appears verbatim on the device)
/// containing `/victim` full of the marker pattern.
fn image_with_victim() -> Vec<u8> {
    let opts = FormatOptions {
        compression: false,
        ..FormatOptions::default()
    };
    let mut fs = Eofs::format(MemDevice::new(DEV_SIZE), &opts).unwrap();
    fs.create_file("/victim").unwrap();
    fs.write("/victim", 0, &marker_content()).unwrap();
    fs.commit().unwrap();
    fs.verify().unwrap();
    fs.unmount().into_vec()
}

#[test]
fn corrupted_file_content_is_detected() {
    let mut image = image_with_victim();

    // Flip one byte in the middle of the stored file content.
    let at = image
        .windows(MARKER.len())
        .position(|window| window == MARKER)
        .expect("marker not found in the image")
        + 2048;
    image[at] ^= 0x40;

    let fs = Eofs::mount(MemDevice::from_vec(image)).unwrap();
    assert_eq!(fs.verify(), Err(FsError::ChecksumMismatch));

    let mut buf = vec![0u8; 3 * 4096];
    assert_eq!(
        fs.read("/victim", 0, &mut buf),
        Err(FsError::ChecksumMismatch)
    );
}

#[test]
fn corrupted_metadata_is_detected() {
    let mut image = image_with_victim();

    // The engine allocates a file's indirect block immediately after its data blocks, so
    // the bytes right after the three marker blocks are reachable metadata. Damage them.
    let first_marker = image
        .windows(MARKER.len())
        .position(|window| window == MARKER)
        .unwrap();
    let at = first_marker + 3 * 4096 + 10;
    image[at] ^= 0x01;

    let fs = Eofs::mount(MemDevice::from_vec(image)).unwrap();
    assert!(fs.verify().is_err(), "corrupted metadata went unnoticed");
}

#[test]
fn damaged_newest_uberblock_falls_back_to_the_previous_commit() {
    let opts = FormatOptions::default();
    let mut fs = Eofs::format(MemDevice::new(DEV_SIZE), &opts).unwrap();
    fs.create_file("/a").unwrap();
    fs.write("/a", 0, b"first transaction").unwrap();
    assert_eq!(fs.commit().unwrap(), 2); // slot 0
    fs.write("/a", 0, b"second transaction").unwrap();
    assert_eq!(fs.commit().unwrap(), 3); // slot 1
    let mut image = fs.unmount().into_vec();

    // Damage the newest uberblock (transaction 3 lives in slot 1 = bytes 4096..8192).
    image[4096 + 100] ^= 0xff;

    let fs = Eofs::mount(MemDevice::from_vec(image)).unwrap();
    assert_eq!(fs.txg(), 2);
    let mut buf = vec![0u8; 32];
    let n = fs.read("/a", 0, &mut buf).unwrap();
    assert_eq!(&buf[..n], b"first transaction");
    fs.verify().unwrap();
}

#[test]
fn damaging_both_uberblocks_fails_the_mount() {
    let mut image = image_with_victim();
    image[100] ^= 0xff;
    image[4096 + 100] ^= 0xff;
    assert!(matches!(
        Eofs::mount(MemDevice::from_vec(image)),
        Err(FsError::Corrupt(_))
    ));
}

#[test]
fn a_blank_device_does_not_mount() {
    assert!(matches!(
        Eofs::mount(MemDevice::new(DEV_SIZE)),
        Err(FsError::Corrupt(_))
    ));
}
