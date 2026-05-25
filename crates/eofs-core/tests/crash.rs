//! Simulated power-cut crash consistency.
//!
//! A fixed scenario of five transactions runs over a `CutDevice` that loses power at every
//! possible write boundary (and with several torn-write lengths). After each cut the image
//! is remounted and must (a) mount, (b) pass `verify()`, and (c) contain exactly the state
//! of some committed transaction — at least every transaction whose `commit()` returned
//! success before the cut, and never a partial one.

use std::collections::{BTreeMap, BTreeSet};

use eofs_core::{CutDevice, Eofs, FormatOptions, FsError, MemDevice, NodeKind};

const DEV_SIZE: u64 = 4 * 1024 * 1024;
const TOTAL_TXS: usize = 5;

fn content_a1() -> Vec<u8> {
    b"alpha, first version".to_vec()
}

fn content_a2() -> Vec<u8> {
    b"alpha rewritten with rather more text than before -- still one block".to_vec()
}

fn content_b() -> Vec<u8> {
    (0..10_240u64).map(|i| (i * 31 % 251) as u8).collect()
}

fn content_c() -> Vec<u8> {
    b"carol".to_vec()
}

fn content_a_tail() -> Vec<u8> {
    b"tail written past the first block".to_vec()
}

const A_TAIL_OFFSET: u64 = 4096 + 10;

fn content_d() -> Vec<u8> {
    b"delta".to_vec()
}

/// The scenario: five transactions. Counts a transaction as reported-committed only when
/// its `commit()` returns `Ok`. Stops at the first error (the power is out).
fn scenario<D: eofs_core::BlockDevice>(
    fs: &mut Eofs<D>,
    reported: &mut usize,
) -> Result<(), FsError> {
    // T1
    fs.mkdir("/dir")?;
    fs.create_file("/dir/a")?;
    fs.write("/dir/a", 0, &content_a1())?;
    fs.commit()?;
    *reported += 1;
    // T2
    fs.write("/dir/a", 0, &content_a2())?;
    fs.create_file("/b")?;
    fs.write("/b", 0, &content_b())?;
    fs.commit()?;
    *reported += 1;
    // T3
    fs.snapshot_create("snap1")?;
    fs.mkdir("/dir/sub")?;
    fs.create_file("/dir/sub/c")?;
    fs.write("/dir/sub/c", 0, &content_c())?;
    fs.commit()?;
    *reported += 1;
    // T4
    fs.remove("/b")?;
    fs.write("/dir/a", A_TAIL_OFFSET, &content_a_tail())?;
    fs.commit()?;
    *reported += 1;
    // T5 — gc() first, so these writes land in reclaimed extents.
    fs.gc()?;
    fs.create_file("/d")?;
    fs.write("/d", 0, &content_d())?;
    fs.commit()?;
    *reported += 1;
    Ok(())
}

/// The expected filesystem contents after `txs` committed transactions.
#[derive(Debug, Default, PartialEq, Eq)]
struct Model {
    files: BTreeMap<String, Vec<u8>>,
    dirs: BTreeSet<String>,
    snapshots: BTreeSet<String>,
}

fn write_into(content: &mut Vec<u8>, offset: u64, data: &[u8]) {
    let end = offset as usize + data.len();
    if content.len() < end {
        content.resize(end, 0);
    }
    content[offset as usize..end].copy_from_slice(data);
}

fn model_after(txs: usize) -> Model {
    let mut model = Model::default();
    if txs >= 1 {
        model.dirs.insert("/dir".into());
        model.files.insert("/dir/a".into(), content_a1());
    }
    if txs >= 2 {
        model.files.insert("/dir/a".into(), content_a2());
        model.files.insert("/b".into(), content_b());
    }
    if txs >= 3 {
        model.snapshots.insert("snap1".into());
        model.dirs.insert("/dir/sub".into());
        model.files.insert("/dir/sub/c".into(), content_c());
    }
    if txs >= 4 {
        model.files.remove("/b");
        let content = model.files.get_mut("/dir/a").unwrap();
        write_into(content, A_TAIL_OFFSET, &content_a_tail());
    }
    if txs >= 5 {
        model.files.insert("/d".into(), content_d());
    }
    model
}

/// Collect the actual filesystem contents (files, directories, snapshot names).
fn collect(fs: &Eofs<MemDevice>) -> Model {
    fn walk(fs: &Eofs<MemDevice>, dir: &str, model: &mut Model) {
        for name in fs.list(dir).unwrap() {
            let path = if dir == "/" {
                format!("/{name}")
            } else {
                format!("{dir}/{name}")
            };
            let stat = fs.stat(&path).unwrap();
            match stat.kind {
                NodeKind::Directory => {
                    model.dirs.insert(path.clone());
                    walk(fs, &path, model);
                }
                NodeKind::File => {
                    let mut buf = vec![0u8; stat.size as usize];
                    assert_eq!(fs.read(&path, 0, &mut buf).unwrap(), buf.len());
                    model.files.insert(path, buf);
                }
            }
        }
    }
    let mut model = Model::default();
    walk(fs, "/", &mut model);
    for snap in fs.snapshot_list().unwrap() {
        model.snapshots.insert(snap.name);
    }
    model
}

/// A freshly formatted, committed, empty image (cuts are applied to the transactions, not
/// to mkfs).
fn base_image() -> Vec<u8> {
    let fs = Eofs::format(MemDevice::new(DEV_SIZE), &FormatOptions::default()).unwrap();
    fs.unmount().into_vec()
}

#[test]
fn power_cut_at_every_write_boundary() {
    let base = base_image();

    // Dry run: no cut, count the writes, and sanity-check the scenario itself.
    let device = CutDevice::unlimited(MemDevice::from_vec(base.clone()));
    let mut fs = Eofs::mount(device).unwrap();
    let mut reported = 0;
    scenario(&mut fs, &mut reported).unwrap();
    assert_eq!(reported, TOTAL_TXS);
    let device = fs.unmount();
    let total_writes = device.writes();
    assert!(total_writes > 20, "scenario is suspiciously small");
    let full_fs = Eofs::mount(MemDevice::from_vec(device.into_inner().into_vec())).unwrap();
    assert_eq!(collect(&full_fs), model_after(TOTAL_TXS));

    // Cut the power at every write boundary, with the interrupting write dropped entirely
    // (tear 0) or partially applied (torn writes of 97 and 1000 bytes).
    for cut in 0..=total_writes {
        for tear in [0usize, 97, 1000] {
            let device = CutDevice::cut_after(MemDevice::from_vec(base.clone()), cut, tear);
            let mut fs = Eofs::mount(device).unwrap();
            let mut reported = 0;
            let _ = scenario(&mut fs, &mut reported);
            let image = fs.unmount().into_inner().into_vec();

            // Power comes back: remount and check.
            let fs = Eofs::mount(MemDevice::from_vec(image)).unwrap_or_else(|err| {
                panic!("remount failed after cut at write {cut} (tear {tear}): {err:?}")
            });
            fs.verify().unwrap_or_else(|err| {
                panic!("verify failed after cut at write {cut} (tear {tear}): {err:?}")
            });

            let durable = (fs.txg() - 1) as usize;
            assert!(
                durable >= reported,
                "cut at write {cut} (tear {tear}): {reported} transactions were reported \
                 committed but only {durable} survived"
            );
            assert!(
                durable <= reported + 1,
                "cut at write {cut} (tear {tear}): more transactions than were ever issued"
            );
            assert_eq!(
                collect(&fs),
                model_after(durable),
                "cut at write {cut} (tear {tear}): state does not match transaction {durable}"
            );

            // The snapshot, once durable, keeps showing the T2 state.
            if durable >= 3 {
                let snap = fs.snapshot("snap1").unwrap();
                let mut buf = vec![0u8; 256];
                let n = snap.read("/dir/a", 0, &mut buf).unwrap();
                assert_eq!(&buf[..n], &content_a2()[..]);
                assert!(snap.stat("/dir/sub").is_err());
            }
        }
    }
}
