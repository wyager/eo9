//! `disk.part` — the partition-table middleware (docs/board/usb-msd-plan.md §2 /
//! sdcard-plan.md §B.3: the component the USB-MSD and SD-card storage lanes share).
//!
//! The MBR byte arithmetic and the validation ladder (signature, GPT, bounds, overlap,
//! extended chains and their cycles) are pinned by `crates/eo9-partwalk`'s own host
//! tests against adversarial fixtures. These tests cover the provider layer: the
//! component has the middleware shape (imports `eo9:disk/disk`, re-exports it), the
//! chain composes and seals, and the *behavior* over a real partitioned device — the
//! window's offset translation, its typed out-of-range edges, the typed table
//! refusals, and a full `disk.part --partition 2 $ fs.eofs $ readwrite` stack with the
//! "boot partition" untouched — runs end to end. The partitioned device is the host
//! root disk provider over fixture bytes (the usermode `--disk <image>` grant path),
//! since `disk.mem` is deliberately blank and a window needs a table to parse.

use std::pin::Pin;
use std::sync::{Arc, Mutex};

use eo9_component::{Component, compose, configure};
use eo9_integration::{guest, run};
use eo9_runtime::providers::{BoxOp, CaptureText};
use eo9_runtime::{
    DiskError, DiskProvider, NamedArg, Outcome, Providers, SpawnError, SpawnLimits, Task,
};

fn disk_part() -> Component {
    guest::ensure_components(&["eo9-stub-disk-part"]);
    guest::load_stub("disk.part")
}

// ---------------------------------------------------------------------------------------
// The fixture device: an in-memory DiskProvider over prepared bytes (the same role the
// host's file-backed provider plays for `eo9 run --disk <image>`).
// ---------------------------------------------------------------------------------------

/// In-memory root disk provider over fixture bytes. Cloning shares the bytes, so a
/// test can inspect (or pre-partition) the device around a run.
#[derive(Clone)]
struct FixtureDisk {
    bytes: Arc<Mutex<Vec<u8>>>,
}

impl FixtureDisk {
    fn new(bytes: Vec<u8>) -> FixtureDisk {
        FixtureDisk {
            bytes: Arc::new(Mutex::new(bytes)),
        }
    }

    fn snapshot(&self, offset: usize, len: usize) -> Vec<u8> {
        self.bytes.lock().unwrap()[offset..offset + len].to_vec()
    }
}

impl DiskProvider for FixtureDisk {
    fn size(&mut self) -> u64 {
        self.bytes.lock().unwrap().len() as u64
    }

    fn read(&mut self, offset: u64, mut dst: Vec<u8>) -> BoxOp<(Vec<u8>, Result<u64, DiskError>)> {
        let bytes = self.bytes.clone();
        Box::pin(async move {
            let bytes = bytes.lock().unwrap();
            let len = dst.len() as u64;
            let Some(end) = offset.checked_add(len) else {
                return (dst, Err(DiskError::OutOfRange));
            };
            if end > bytes.len() as u64 {
                return (dst, Err(DiskError::OutOfRange));
            }
            dst.copy_from_slice(&bytes[offset as usize..end as usize]);
            (dst, Ok(len))
        }) as Pin<Box<_>>
    }

    fn write(&mut self, offset: u64, src: Vec<u8>) -> BoxOp<(Vec<u8>, Result<u64, DiskError>)> {
        let bytes = self.bytes.clone();
        Box::pin(async move {
            let mut bytes = bytes.lock().unwrap();
            let len = src.len() as u64;
            let Some(end) = offset.checked_add(len) else {
                return (src, Err(DiskError::OutOfRange));
            };
            if end > bytes.len() as u64 {
                return (src, Err(DiskError::OutOfRange));
            }
            bytes[offset as usize..end as usize].copy_from_slice(&src);
            (src, Ok(len))
        }) as Pin<Box<_>>
    }

    fn flush(&mut self) -> BoxOp<Result<(), DiskError>> {
        Box::pin(async { Ok(()) })
    }
}

fn with_disk(disk: FixtureDisk) -> Providers {
    let mut providers = Providers::none();
    providers.disk = Some(Box::new(disk));
    // partcheck narrates each probe over eo9:text.
    providers.text = Some(Box::new(CaptureText::new()));
    providers
}

// ---------------------------------------------------------------------------------------
// Fixture images (independent byte construction — the load-as-test-input discipline).
// ---------------------------------------------------------------------------------------

const SECTOR: usize = 512;
const P1_MAGIC: &str = "EO9-PART-FIXTURE-P1";
const P2_MAGIC: &str = "EO9-PART-FIXTURE-P2";

fn write_entry(image: &mut [u8], slot: usize, partition_type: u8, start: u32, count: u32) {
    let at = 446 + slot * 16;
    image[at + 4] = partition_type;
    image[at + 8..at + 12].copy_from_slice(&start.to_le_bytes());
    image[at + 12..at + 16].copy_from_slice(&count.to_le_bytes());
}

/// 8 MiB, MBR: p1 (FAT-typed) at LBA 2048 for 2048 sectors, p2 (0xDA) at LBA 4096 for
/// 4096 sectors, a magic string at each partition's first sector.
fn mbr_fixture() -> Vec<u8> {
    let mut image = vec![0u8; 16384 * SECTOR];
    image[510] = 0x55;
    image[511] = 0xAA;
    write_entry(&mut image, 0, 0x0C, 2048, 2048);
    write_entry(&mut image, 1, 0xDA, 4096, 4096);
    image[2048 * SECTOR..2048 * SECTOR + P1_MAGIC.len()].copy_from_slice(P1_MAGIC.as_bytes());
    image[4096 * SECTOR..4096 * SECTOR + P2_MAGIC.len()].copy_from_slice(P2_MAGIC.as_bytes());
    image
}

/// 4 MiB, a protective-MBR GPT disk.
fn gpt_fixture() -> Vec<u8> {
    let mut image = vec![0u8; 8192 * SECTOR];
    image[510] = 0x55;
    image[511] = 0xAA;
    write_entry(&mut image, 0, 0xEE, 1, 8191);
    image[SECTOR..SECTOR + 8].copy_from_slice(b"EFI PART");
    image
}

/// The composed probe chain `disk.part [--partition N] $ partcheck`, leaving the disk
/// import for the root provider.
fn probe_chain(partition: Option<u32>) -> Component {
    guest::ensure_components(&["eo9-stub-disk-part", "eo9-example-partcheck"]);
    let part = match partition {
        Some(partition) => configure(&disk_part(), &[("partition", &partition.to_string())])
            .expect("disk.part --partition N should configure"),
        None => disk_part(),
    };
    compose(&part, &guest::load_example("partcheck")).expect("disk.part $ partcheck must compose")
}

fn partcheck_args(mode: &str, magic: Option<&str>, needle: Option<&str>) -> Vec<NamedArg> {
    let opt = |value: Option<&str>| match value {
        Some(value) => format!("some(\"{value}\")"),
        None => String::from("none"),
    };
    vec![
        NamedArg::new("mode", format!("\"{mode}\"")),
        NamedArg::new("magic", opt(magic)),
        NamedArg::new("needle", opt(needle)),
    ]
}

// ---------------------------------------------------------------------------------------
// Shape and composition.
// ---------------------------------------------------------------------------------------

/// `disk.part` has the middleware shape: it asks for a raw device and re-exports the
/// same surface (plus its config interface for `--partition`).
#[test]
fn disk_part_has_the_middleware_shape() {
    let info = disk_part().describe();
    let imports: Vec<&str> = info.imports.iter().map(|n| n.interface.as_str()).collect();
    assert!(
        imports.contains(&"eo9:disk/disk"),
        "disk.part must import the whole device: {imports:?}"
    );
    let exports: Vec<&str> = info.exports.iter().map(|e| e.interface.as_str()).collect();
    assert!(
        exports.contains(&"eo9:disk/disk"),
        "disk.part must re-export the windowed device: {exports:?}"
    );
    assert!(
        exports.contains(&"eo9:disk/part-config"),
        "disk.part must export its config interface: {exports:?}"
    );
}

/// `disk.mem $ disk.part` seals the device need and still offers `eo9:disk/disk`.
#[test]
fn disk_mem_seals_the_part_device_need() {
    guest::ensure_components(&["eo9-stub-disk-mem", "eo9-stub-disk-part"]);
    let stack = compose(&guest::load_stub("disk.mem"), &disk_part())
        .expect("disk.mem $ disk.part must compose");
    let info = stack.describe();
    assert!(
        info.imports
            .iter()
            .all(|need| !need.interface.starts_with("eo9:disk/")),
        "the device need must be sealed by disk.mem: {:?}",
        info.imports
    );
    assert!(
        info.exports
            .iter()
            .any(|export| export.interface == "eo9:disk/disk"),
        "the stack must still export eo9:disk/disk: {:?}",
        info.exports
    );
}

/// `configure` refuses partition 0 (1-based, fdisk numbering): baking the constant
/// succeeds, and the provider's own refusal text surfaces as the typed
/// `SpawnError::ConfigurationRefused` when the chain is bound at spawn.
#[test]
fn configure_refuses_partition_zero_at_bind() {
    let zero = configure(&disk_part(), &[("partition", "0")])
        .expect("baking the constant itself succeeds; the provider refuses at bind");
    let program =
        compose(&zero, &guest::load_example("partcheck")).expect("the chain still composes");
    let image = run::compile_component(&program);
    let err = Task::spawn(
        &image,
        &partcheck_args("window", None, None),
        SpawnLimits::default(),
        with_disk(FixtureDisk::new(mbr_fixture())),
    )
    .expect_err("--partition 0 must refuse the spawn");
    match err {
        SpawnError::ConfigurationRefused(reason) => assert!(
            reason.contains("1-based"),
            "the refusal must carry the provider's own message: {reason}"
        ),
        other => panic!("expected ConfigurationRefused, got: {other}"),
    }
}

// ---------------------------------------------------------------------------------------
// Behavior over a partitioned device.
// ---------------------------------------------------------------------------------------

/// The unconfigured default (partition 1): the window starts at p1's first sector
/// (the magic), reports p1's size, round-trips a write, and refuses typed at its
/// edges — partcheck pins all of it from inside the guest.
#[test]
fn default_partition_window_serves_and_enforces() {
    let outcome = run::run_component(
        &probe_chain(None),
        &partcheck_args("window", Some(P1_MAGIC), None),
        with_disk(FixtureDisk::new(mbr_fixture())),
    );
    match &outcome {
        Outcome::Success(_) => {}
        other => panic!("expected the p1 window to serve, got {other:?}"),
    }
    let value = run::success_value(&outcome);
    assert!(
        value.contains("size=1048576"),
        "p1 is 2048 sectors = 1 MiB: {value}"
    );
}

/// `--partition 2` moves the window: p2's magic, p2's size.
#[test]
fn partition_2_window_is_a_different_span() {
    let outcome = run::run_component(
        &probe_chain(Some(2)),
        &partcheck_args("window", Some(P2_MAGIC), None),
        with_disk(FixtureDisk::new(mbr_fixture())),
    );
    match &outcome {
        Outcome::Success(_) => {}
        other => panic!("expected the p2 window to serve, got {other:?}"),
    }
    let value = run::success_value(&outcome);
    assert!(
        value.contains("size=2097152"),
        "p2 is 4096 sectors = 2 MiB: {value}"
    );
}

/// A write through the window lands inside the partition — and the partition table
/// plus everything before the partition start is byte-identical afterwards (the
/// read-only-by-construction guarantee, observed from outside the chain).
#[test]
fn window_writes_never_touch_the_table_or_other_spans() {
    let disk = FixtureDisk::new(mbr_fixture());
    // Everything before p1's start: the MBR sector and the gap (boot-loader land).
    let before = disk.snapshot(0, 2048 * SECTOR);
    // p2's whole span must also be untouched by a p1-window run.
    let p2_before = disk.snapshot(4096 * SECTOR, 4096 * SECTOR);

    let outcome = run::run_component(
        &probe_chain(Some(1)),
        &partcheck_args("window", Some(P1_MAGIC), None),
        with_disk(disk.clone()),
    );
    assert!(
        matches!(outcome, Outcome::Success(_)),
        "the p1 window must serve: {outcome:?}"
    );

    assert_eq!(
        disk.snapshot(0, 2048 * SECTOR),
        before,
        "the partition table and the pre-partition gap must be byte-identical"
    );
    assert_eq!(
        disk.snapshot(4096 * SECTOR, 4096 * SECTOR),
        p2_before,
        "p2's span must be untouched by a p1-window run"
    );
}

/// A GPT disk refuses typed — the protective MBR is never misread as one partition.
#[test]
fn gpt_disk_refuses_typed() {
    let outcome = run::run_component(
        &probe_chain(None),
        &partcheck_args("refusal", None, Some("GPT")),
        with_disk(FixtureDisk::new(gpt_fixture())),
    );
    match &outcome {
        Outcome::Success(_) => {}
        other => panic!("expected the typed GPT refusal, got {other:?}"),
    }
    assert!(
        run::success_value(&outcome).contains("not supported in v1"),
        "the refusal must name the v1 limitation: {}",
        run::success_value(&outcome)
    );
}

/// A blank device (no signature — `disk.mem`'s zero-filled default, the whole chain in
/// components) refuses typed.
#[test]
fn blank_device_refuses_typed() {
    guest::ensure_components(&[
        "eo9-stub-disk-mem",
        "eo9-stub-disk-part",
        "eo9-example-partcheck",
    ]);
    let stack = compose(&guest::load_stub("disk.mem"), &disk_part())
        .expect("disk.mem $ disk.part must compose");
    let program = compose(&stack, &guest::load_example("partcheck"))
        .expect("disk.mem $ disk.part $ partcheck must compose");
    let mut providers = Providers::none();
    providers.text = Some(Box::new(CaptureText::new()));
    let outcome = run::run_component(
        &program,
        &partcheck_args("refusal", None, Some("signature")),
        providers,
    );
    match &outcome {
        Outcome::Success(_) => {}
        other => panic!("expected the missing-signature refusal, got {other:?}"),
    }
}

/// Selecting a partition the table does not have refuses typed, naming what is there.
#[test]
fn absent_partition_refuses_typed() {
    let outcome = run::run_component(
        &probe_chain(Some(3)),
        &partcheck_args("refusal", None, Some("absent")),
        with_disk(FixtureDisk::new(mbr_fixture())),
    );
    match &outcome {
        Outcome::Success(_) => {}
        other => panic!("expected the absent-partition refusal, got {other:?}"),
    }
    assert!(
        run::success_value(&outcome).contains("present: 1, 2"),
        "the refusal must name the partitions that exist: {}",
        run::success_value(&outcome)
    );
}

// ---------------------------------------------------------------------------------------
// The SD-plan stack: a filesystem on partition 2, the boot partition untouchable.
// ---------------------------------------------------------------------------------------

/// `disk.part --partition 2 $ fs.eofs $ readwrite`: eofs formats the blank partition-2
/// window on first mount and round-trips a file through it — while partition 1 and the
/// partition table stay byte-identical (sdcard-plan §B.3's one-card-does-both layout,
/// usermode).
#[test]
fn eofs_on_partition_2_round_trips_and_p1_is_untouched() {
    guest::ensure_components(&[
        "eo9-stub-disk-part",
        "eo9-stub-fs-eofs",
        "eo9-example-readwrite",
    ]);
    let part2 =
        configure(&disk_part(), &[("partition", "2")]).expect("--partition 2 should configure");
    let stack =
        compose(&part2, &guest::load_stub("fs.eofs")).expect("disk.part $ fs.eofs must compose");
    let program = compose(&stack, &guest::load_example("readwrite"))
        .expect("disk.part $ fs.eofs $ readwrite must compose");

    // p2 must be blank for this test: eofs's foreign-image refusal (correctly) refuses
    // to format over non-zero data, and the window fixture's P2 magic is exactly that.
    let mut image = mbr_fixture();
    image[4096 * SECTOR..4096 * SECTOR + P2_MAGIC.len()].fill(0);
    let disk = FixtureDisk::new(image);
    // p1's span AND everything before it: the boot partition plus the table.
    let head_before = disk.snapshot(0, 4096 * SECTOR);

    let outcome = run::run_component(
        &program,
        &[
            NamedArg::new("path", "\"cache.bin\""),
            NamedArg::new("contents", "\"survives on partition 2\""),
        ],
        with_disk(disk.clone()),
    );
    match &outcome {
        Outcome::Success(_) => {}
        other => panic!("expected a round-trip through eofs on p2, got {other:?}"),
    }
    assert!(
        run::success_value(&outcome).starts_with("round-tripped("),
        "unexpected success value: {}",
        run::success_value(&outcome)
    );

    assert_eq!(
        disk.snapshot(0, 4096 * SECTOR),
        head_before,
        "the table and the whole boot partition must be byte-identical after eofs \
         formatted and wrote partition 2"
    );
    // And eofs really did write inside p2 (the partition is no longer blank).
    assert!(
        disk.snapshot(4096 * SECTOR, 4096 * SECTOR)
            .iter()
            .any(|&b| b != 0),
        "eofs must have written into partition 2's span"
    );
}

/// Two identical runs over identical fixtures produce identical outcomes (no clock, no
/// entropy anywhere in the chain).
#[test]
fn window_probe_is_deterministic() {
    let args = partcheck_args("window", Some(P1_MAGIC), None);
    let first = run::run_component(
        &probe_chain(None),
        &args,
        with_disk(FixtureDisk::new(mbr_fixture())),
    );
    let second = run::run_component(
        &probe_chain(None),
        &args,
        with_disk(FixtureDisk::new(mbr_fixture())),
    );
    assert!(
        matches!(first, Outcome::Success(_)),
        "first run must succeed: {first:?}"
    );
    assert_eq!(
        run::success_value(&first),
        run::success_value(&second),
        "the window probe must be deterministic across runs"
    );
}
