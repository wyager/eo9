//! Compression behaviour: on by default, raw fallback for incompressible blocks, and
//! logical content (and therefore Merkle hashes) independent of the codec.

use eofs_core::{Eofs, FormatOptions, MemDevice};

const DEV_SIZE: u64 = 8 * 1024 * 1024;

fn format_with(compression: bool) -> Eofs<MemDevice> {
    let opts = FormatOptions {
        compression,
        ..FormatOptions::default()
    };
    Eofs::format(MemDevice::new(DEV_SIZE), &opts).unwrap()
}

fn read_all(fs: &Eofs<MemDevice>, path: &str) -> Vec<u8> {
    let size = fs.stat(path).unwrap().size as usize;
    let mut buf = vec![0u8; size];
    assert_eq!(fs.read(path, 0, &mut buf).unwrap(), size);
    buf
}

/// Highly compressible: repeated text.
fn compressible(len: usize) -> Vec<u8> {
    b"all work and no play makes eofs a dull filesystem. "
        .iter()
        .cycle()
        .copied()
        .take(len)
        .collect()
}

/// Incompressible: a deterministic xorshift byte stream.
fn incompressible(len: usize) -> Vec<u8> {
    let mut state = 0x9e37_79b9_7f4a_7c15u64;
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
fn compressible_data_is_stored_compressed() {
    let mut fs = format_with(true);
    let data = compressible(64 * 4096);
    fs.create_file("/text").unwrap();
    fs.write("/text", 0, &data).unwrap();
    fs.commit().unwrap();

    let report = fs.verify().unwrap();
    assert!(report.compressed_blocks > 0, "no block was compressed");
    assert!(
        report.physical_bytes < report.logical_bytes / 2,
        "compression saved too little: {} physical vs {} logical",
        report.physical_bytes,
        report.logical_bytes
    );
    assert_eq!(read_all(&fs, "/text"), data);

    let fs = Eofs::mount(fs.unmount()).unwrap();
    assert_eq!(read_all(&fs, "/text"), data);
    fs.verify().unwrap();
}

#[test]
fn compression_off_stores_everything_raw() {
    let mut fs = format_with(false);
    assert!(!fs.compression());
    let data = compressible(16 * 4096);
    fs.create_file("/text").unwrap();
    fs.write("/text", 0, &data).unwrap();
    fs.commit().unwrap();

    let report = fs.verify().unwrap();
    assert_eq!(report.compressed_blocks, 0);
    assert_eq!(read_all(&fs, "/text"), data);
}

#[test]
fn incompressible_data_falls_back_to_raw_storage() {
    let data = incompressible(32 * 4096);

    let mut on = format_with(true);
    on.create_file("/noise").unwrap();
    on.write("/noise", 0, &data).unwrap();
    on.commit().unwrap();
    assert_eq!(read_all(&on, "/noise"), data);

    let mut off = format_with(false);
    off.create_file("/noise").unwrap();
    off.write("/noise", 0, &data).unwrap();
    off.commit().unwrap();

    // With the raw fallback, incompressible data costs (essentially) the same space whether
    // compression is enabled or not: only metadata blocks can still shrink a little.
    let frontier_on = on.space().frontier;
    let frontier_off = off.space().frontier;
    assert!(frontier_on <= frontier_off);
    assert!(
        frontier_off - frontier_on <= 4096,
        "incompressible data should not be stored compressed ({frontier_on} vs {frontier_off})"
    );
    on.verify().unwrap();
}

#[test]
fn compressible_data_uses_less_space_than_raw() {
    let data = compressible(64 * 4096);

    let mut on = format_with(true);
    on.create_file("/text").unwrap();
    on.write("/text", 0, &data).unwrap();
    on.commit().unwrap();

    let mut off = format_with(false);
    off.create_file("/text").unwrap();
    off.write("/text", 0, &data).unwrap();
    off.commit().unwrap();

    assert!(
        on.space().frontier < off.space().frontier / 2,
        "compression should at least halve this file: {} vs {}",
        on.space().frontier,
        off.space().frontier
    );
}

#[test]
fn single_block_hashes_are_codec_independent() {
    // Block-pointer hashes cover logical (uncompressed) bytes, so a single-block file's
    // Merkle root is exactly blake3(content) no matter how the block was stored. (Hashes of
    // multi-block nodes cover their indirect blocks — see FORMAT.md "Hashing".)
    let data = compressible(1000);

    let mut on = format_with(true);
    on.create_file("/f").unwrap();
    on.write("/f", 0, &data).unwrap();

    let mut off = format_with(false);
    off.create_file("/f").unwrap();
    off.write("/f", 0, &data).unwrap();

    let expected = *blake3::hash(&data).as_bytes();
    assert_eq!(on.stat("/f").unwrap().hash, expected);
    assert_eq!(off.stat("/f").unwrap().hash, expected);
}
