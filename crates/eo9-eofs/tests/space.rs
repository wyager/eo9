//! Space reclamation (study 07, S7-3): the provider's gc-on-NoSpace discipline keeps an
//! image usable forever — rewrites reuse the space of the copies they replace, and
//! removed files' space comes back. Without reclamation, an image's append frontier
//! reaches the end of the device after a finite number of writes and the image bricks.

use eo9_eofs::{Eofs, FormatOptions, FsError, MemDevice};

/// The provider's `mutate()` pattern: try, gc on NoSpace, retry once.
fn with_gc_retry<R>(
    fs: &mut Eofs<MemDevice>,
    op: impl Fn(&mut Eofs<MemDevice>) -> Result<R, FsError>,
) -> Result<R, FsError> {
    match op(fs) {
        Err(FsError::NoSpace) => {
            fs.gc().expect("gc walks a healthy image");
            op(fs)
        }
        other => other,
    }
}

/// Pseudo-random (incompressible) bytes, deterministic per seed.
fn incompressible(len: usize, seed: u64) -> Vec<u8> {
    let mut state = 0x9E3779B97F4A7C15u64 ^ seed.wrapping_mul(0xD1B54A32D192ED03);
    (0..len)
        .map(|_| {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state as u8
        })
        .collect()
}

/// The provider's atomic rewrite: remove + recreate + write + commit, every step guarded
/// so the closure can be re-run from any partial state.
fn rewrite(fs: &mut Eofs<MemDevice>, path: &str, data: &[u8]) -> Result<(), FsError> {
    if fs.stat(path).is_ok() {
        fs.remove(path)?;
    }
    fs.create_file(path)?;
    fs.write(path, 0, data)?;
    fs.commit()?;
    Ok(())
}

#[test]
fn rewrites_with_gc_never_brick_the_image() {
    // 500 rewrites of a ~4 KiB incompressible file on a 256 KiB image. Without
    // reclamation the frontier hits the end after a few dozen; with the gc retry the
    // image stays usable indefinitely and always holds the last write.
    let mut fs = Eofs::format(MemDevice::new(256 * 1024), &FormatOptions::default()).unwrap();
    for i in 0..500u64 {
        let data = incompressible(4096, i);
        with_gc_retry(&mut fs, |fs| rewrite(fs, "/same.txt", &data))
            .unwrap_or_else(|err| panic!("rewrite {i} failed: {err:?}"));
    }
    let mut buf = vec![0u8; 4096];
    assert_eq!(fs.read("/same.txt", 0, &mut buf).unwrap(), 4096);
    assert_eq!(buf, incompressible(4096, 499));

    // And the image still mounts and verifies cleanly after all that churn.
    let fs = Eofs::mount(fs.unmount()).unwrap();
    assert!(fs.verify().unwrap().blocks > 0);
}

#[test]
fn without_gc_the_same_workload_bricks() {
    // The control for the test above: the same workload WITHOUT the gc retry runs out of
    // space — the study's "write-budget brick". (If this test ever starts failing, the
    // engine has grown automatic reclamation and the provider's retry can simplify.)
    let mut fs = Eofs::format(MemDevice::new(256 * 1024), &FormatOptions::default()).unwrap();
    let mut bricked = false;
    for i in 0..500u64 {
        let data = incompressible(4096, i);
        match rewrite(&mut fs, "/same.txt", &data) {
            Err(FsError::NoSpace) => {
                bricked = true;
                break;
            }
            other => other.unwrap(),
        }
    }
    assert!(
        bricked,
        "the no-gc control was expected to run out of space"
    );
}

#[test]
fn removed_files_space_is_reclaimed() {
    // Fill most of the image with one file, remove it, and the space is usable again for
    // a different file ("rm frees space").
    let mut fs = Eofs::format(MemDevice::new(256 * 1024), &FormatOptions::default()).unwrap();
    let big = incompressible(150 * 1024, 1);
    with_gc_retry(&mut fs, |fs| rewrite(fs, "/a", &big)).unwrap();

    // A second file of the same size cannot fit alongside the first, even with gc — the
    // space genuinely is occupied.
    let second = incompressible(150 * 1024, 2);
    assert_eq!(
        with_gc_retry(&mut fs, |fs| rewrite(fs, "/b", &second)),
        Err(FsError::NoSpace)
    );
    fs.rollback();

    // After removing the first file, the same write succeeds.
    fs.remove("/a").unwrap();
    fs.commit().unwrap();
    with_gc_retry(&mut fs, |fs| rewrite(fs, "/b", &second)).unwrap();

    let mut buf = vec![0u8; second.len()];
    assert_eq!(fs.read("/b", 0, &mut buf).unwrap(), second.len());
    assert_eq!(buf, second);
}
