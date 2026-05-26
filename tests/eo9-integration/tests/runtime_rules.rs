//! Runtime-rule suite (plan/13-tests.md milestone 2): the newly merged runtime behaviours
//! observed from outside the runtime crate — the io-buffer caps (a clean in-band error,
//! never host memory growth) and the loader rule's auto-sealing of optional imports at
//! spawn (complementing the composition-level absence tests in `capabilities.rs`).

use eo9_integration::{fixtures, run};
use eo9_runtime::providers::SeededEntropy;
use eo9_runtime::task::{MAX_BUFFER_BYTES, MAX_TOTAL_BUFFER_BYTES};
use eo9_runtime::{NamedArg, Outcome, Providers};

/// Run the buffer-hog fixture: construct `count` buffers of `len` bytes each.
fn run_buffer_hog(len: u64, count: u32) -> Outcome {
    let image = run::compile_wat(fixtures::buffer_hog_wat());
    run::run_image(
        &image,
        &[
            NamedArg::new("len", len.to_string()),
            NamedArg::new("count", count.to_string()),
        ],
        Providers::none(),
    )
}

#[test]
fn buffer_allocations_within_the_caps_succeed() {
    // A handful of small buffers is far below both caps.
    let outcome = run_buffer_hog(1024 * 1024, 4);
    assert_eq!(run::success_value(&outcome), "4");

    // Exactly the per-buffer cap is allowed.
    let outcome = run_buffer_hog(MAX_BUFFER_BYTES, 1);
    assert_eq!(run::success_value(&outcome), "1");
}

#[test]
fn an_over_sized_buffer_fails_with_a_clean_error_naming_the_per_buffer_cap() {
    let outcome = run_buffer_hog(MAX_BUFFER_BYTES + 1, 1);
    match &outcome {
        Outcome::Trapped(reason) => {
            assert!(
                reason.contains("per-buffer cap"),
                "the error must name the per-buffer cap, got: {reason}"
            );
        }
        other => panic!("an over-sized buffer must fail in-band, got {other:?}"),
    }
}

#[test]
fn exceeding_the_per_task_buffer_budget_fails_with_a_clean_error() {
    // Each buffer is within the per-buffer cap, but the fifth would push the task past
    // the per-task ceiling (5 * 16 MiB > 64 MiB).
    let per_buffer = MAX_BUFFER_BYTES;
    let count = (MAX_TOTAL_BUFFER_BYTES / per_buffer + 1) as u32;
    let outcome = run_buffer_hog(per_buffer, count);
    match &outcome {
        Outcome::Trapped(reason) => {
            assert!(
                reason.contains("buffer budget") || reason.contains("ceiling"),
                "the error must name the per-task budget, got: {reason}"
            );
        }
        other => panic!("exceeding the task buffer budget must fail in-band, got {other:?}"),
    }
}

#[test]
fn an_optional_import_is_auto_sealed_at_spawn_and_observes_the_grant() {
    let probe = fixtures::optional_entropy_probe();
    let image = run::compile_component(&probe);

    // Not granted: the spawn still succeeds (no loader-rule rejection) and the program
    // observes absence through the `-optional` import's own type.
    let outcome = run::run_image(&image, &[], Providers::none());
    assert_eq!(
        run::success_value(&outcome),
        "0",
        "an ungranted optional import must be auto-sealed as absent"
    );

    // Granted: the very same program observes presence.
    let outcome = run::run_image(
        &image,
        &[],
        Providers {
            entropy: Some(Box::new(SeededEntropy::new(1))),
            ..Providers::none()
        },
    );
    assert_eq!(
        run::success_value(&outcome),
        "1",
        "a granted optional import must be observed as present"
    );
}
