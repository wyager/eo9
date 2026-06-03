//! cancelcheck — the executable cancel-mid-flight disk probe (plan/09 D34's recorded
//! follow-up), exercised in usermode over `disk.mem`.
//!
//! In-memory reads complete within the callee's first executor entry, so the
//! mid-flight window does not open here: every attempt classifies as a miss and the
//! probe must report that honestly. (The same program is in the kernel store for the
//! metal run; hits there additionally wait on the kernel's `pci.wait` suspending the
//! calling task instead of blocking host-side — plan/09 D37.) What this suite pins is the probe
//! machinery itself: the manual select, the cancel-on-drop of queued/completed
//! subtasks (no trap, no hang — the canonical ABI's two cancel traps are avoided by
//! the SDK), and the byte-exact verification of both regions after every attempt.

use eo9_component::{compose, configure};
use eo9_integration::{guest, run};
use eo9_runtime::providers::CaptureText;
use eo9_runtime::{NamedArg, Outcome, Providers};

const COMPONENTS: &[&str] = &["eo9-stub-disk-mem", "eo9-example-cancelcheck"];

/// 2 MiB: comfortably more than the probe's two regions (1 MiB + 64 KiB).
const DISK_SIZE: &str = "2097152";

#[test]
fn the_cancel_probe_runs_clean_over_an_in_memory_disk() {
    guest::ensure_components(COMPONENTS);
    let disk = configure(&guest::load_stub("disk.mem"), &[("size", DISK_SIZE)])
        .expect("baking the disk size succeeds");
    let program =
        compose(&disk, &guest::load_example("cancelcheck")).expect("disk.mem $ cancelcheck");

    let capture = CaptureText::new();
    let providers = Providers {
        text: Some(Box::new(capture.clone())),
        ..Providers::none()
    };
    let outcome = run::run_component(&program, &[NamedArg::new("attempts", "4")], providers);
    match outcome {
        Outcome::Success(success) => {
            // The classification line is the program's own typed report; all four
            // attempts must be accounted for, and the verification (which would have
            // failed the run with a `corruption` value) passed byte-for-byte.
            assert!(
                success.value.contains("attempts=4"),
                "the report must carry the attempt count: {}",
                success.value
            );
            println!("cancelcheck (usermode/disk.mem): {}", success.value);
        }
        other => panic!("expected the probe to verify cleanly, got {other:?}"),
    }
}
