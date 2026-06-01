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

// --- probing and degraded-mount reporting (study 07, S7-1 & S7-2) -------------------------

#[test]
fn probe_distinguishes_eofs_blank_foreign_and_unmountable() {
    use eofs_core::{ImageState, probe};

    // A healthy image probes as Eofs at its committed transaction, not degraded.
    let image = image_with_victim();
    let dev = MemDevice::from_vec(image.clone());
    assert_eq!(
        probe(&dev).unwrap(),
        ImageState::Eofs {
            txg: 2,
            degraded: false
        }
    );

    // A zero-filled device is Blank.
    assert_eq!(probe(&MemDevice::new(DEV_SIZE)).unwrap(), ImageState::Blank);

    // A device full of someone else's data is Foreign — never auto-formatted.
    let mut foreign = vec![0u8; DEV_SIZE as usize];
    foreign[1024..2048].fill(0xA5); // an "ext4 superblock"
    assert_eq!(
        probe(&MemDevice::from_vec(foreign)).unwrap(),
        ImageState::Foreign
    );

    // Foreign data far into the device but zeros up front still counts as Foreign when it
    // is inside the probed span.
    let mut late = vec![0u8; DEV_SIZE as usize];
    late[60 * 1024] = 1;
    assert_eq!(
        probe(&MemDevice::from_vec(late)).unwrap(),
        ImageState::Foreign
    );

    // A btrfs-shaped victim: zeros everywhere except a superblock at exactly 64 KiB — the
    // boundary where the old 64 KiB probe span ended (owner ruling on study 07, S7-2).
    // This must read as Foreign, never Blank.
    let mut btrfs_like = vec![0u8; DEV_SIZE as usize];
    btrfs_like[64 * 1024..64 * 1024 + 8].copy_from_slice(b"_BHRfS_M");
    assert_eq!(
        probe(&MemDevice::from_vec(btrfs_like)).unwrap(),
        ImageState::Foreign
    );

    // Backup structures at the END of the device (a backup GPT header, ZFS end labels)
    // also make it Foreign: a wiped start with surviving backups is damaged data, not a
    // blank device.
    let mut tail_data = vec![0u8; DEV_SIZE as usize];
    let tail = DEV_SIZE as usize - 512;
    tail_data[tail..tail + 8].copy_from_slice(b"EFI PART");
    assert_eq!(
        probe(&MemDevice::from_vec(tail_data)).unwrap(),
        ImageState::Foreign
    );

    // Data hiding between the probed spans of a LARGE device is the accepted residual
    // risk: beyond the leading megabyte and before the trailing 64 KiB nothing common
    // lives, and probing entire multi-gigabyte devices on every mount is not worth it.
    // (Devices at or under 2 MiB are probed in full, so this gap only exists above that.)
    let mut between = vec![0u8; (3 * 1024 * 1024) as usize];
    between[2 * 1024 * 1024] = 1;
    assert_eq!(
        probe(&MemDevice::from_vec(between)).unwrap(),
        ImageState::Blank
    );

    // A small device (at or under 2 MiB) is probed in full: a non-zero byte anywhere makes
    // it Foreign.
    let mut small = vec![0u8; (2 * 1024 * 1024) as usize];
    small[1024 * 1024 + 512 * 1024] = 1; // past the 1 MiB prefix, before the 64 KiB suffix
    assert_eq!(
        probe(&MemDevice::from_vec(small)).unwrap(),
        ImageState::Foreign
    );

    // An image whose every uberblock slot is damaged is Unmountable, not Blank or Foreign.
    let mut dead = image;
    dead[100] ^= 0xff;
    dead[4096 + 100] ^= 0xff;
    assert_eq!(
        probe(&MemDevice::from_vec(dead)).unwrap(),
        ImageState::Unmountable
    );
}

#[test]
fn falling_back_past_a_damaged_uberblock_is_reported() {
    use eofs_core::{ImageState, probe};

    // Two transactions: txg 1 (format, slot 1) and txg 2 (the file, slot 0).
    let mut image = image_with_victim();

    // Damage the NEWEST uberblock (txg 2 lives in slot 0 = bytes 0..4096). The mount
    // falls back to txg 1 — correct for crash recovery, but it must SAY so, because if
    // txg 2 was an acknowledged commit this is silent data loss.
    image[100] ^= 0xff;

    let dev = MemDevice::from_vec(image);
    assert_eq!(
        probe(&dev).unwrap(),
        ImageState::Eofs {
            txg: 1,
            degraded: true
        }
    );

    let (fs, report) = Eofs::mount_with_report(dev).unwrap();
    assert_eq!(report.txg, 1);
    assert!(report.fell_back_past_invalid_slot);
    // The rolled-back state has no /victim (it was created in txg 2).
    assert!(matches!(fs.stat("/victim"), Err(FsError::NotFound)));

    // A healthy image, by contrast, reports no fallback.
    let (_fs, report) = Eofs::mount_with_report(MemDevice::from_vec(image_with_victim())).unwrap();
    assert_eq!(report.txg, 2);
    assert!(!report.fell_back_past_invalid_slot);
}
