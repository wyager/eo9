//! Step-cost instrumentation for the per-keystroke editor (study 33,
//! docs/study/parser-step-cost.md): where do the ~125M cycles/keystroke measured on
//! the board go?
//!
//! Everything heavy is `#[ignore]`d so `cargo xtask ci` stays fast; the smoke test
//! alone runs in the gate and keeps the harness compiling. Run the measurements with:
//!
//! ```text
//! cd guest && cargo test -p eosh-inc --target aarch64-apple-darwin --release \
//!     --test step_cost -- --ignored --nocapture --test-threads 1
//! ```
//!
//! (`--test-threads 1` because the counting allocator and the timing loops are
//! process-global; release because the question is real per-keystroke cost.)
//!
//! The harness measures, per keystroke position across the 87-byte demo line:
//!
//! * **census** — heap traffic per editor keystroke (a counting global allocator
//!   around the real [`eosh_inc::editor::Editor`]), plus the live-tree size via a
//!   clone census: `clone_box` re-boxes every node, so allocations during a clone ≈
//!   heap blocks in the state tree (boxes + vecs + strings; `Rc` clones don't
//!   allocate, so shared vocab/closure nodes count once at build, zero per clone).
//! * **phases** — wall time of `step` / `admissible` / `completions` / `clone_box`
//!   separately at every position (states are immutable, so each can be re-run on
//!   the same state and medianed). `clone_box` is the retroactive cost of M2's
//!   removed defensive per-keystroke clone.
//! * **width** — the same line over vocabularies of 4/16/38/67/100 names (67 = the
//!   kernel store's real /bin listing, snapshotted below).
//!
//! Nothing here ships: integration tests never enter the wasm component build.

use std::alloc::{GlobalAlloc, Layout, System};
use std::eprintln;
use std::string::String;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;
use std::vec::Vec;

use eosh_inc::editor::{Editor, Key, Marker};
use eosh_inc::grammar::{Vocab, command_line};
use eosh_inc::inc::{BoxP, Completion, Step, Tag};
use eosh_inc::input::Input;

// ---------------------------------------------------------------------------------
// Counting allocator
// ---------------------------------------------------------------------------------

static ALLOCS: AtomicU64 = AtomicU64::new(0);
static ALLOC_BYTES: AtomicU64 = AtomicU64::new(0);
static FREES: AtomicU64 = AtomicU64::new(0);
static FREE_BYTES: AtomicU64 = AtomicU64::new(0);

struct Counting;

unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        ALLOCS.fetch_add(1, Ordering::Relaxed);
        ALLOC_BYTES.fetch_add(layout.size() as u64, Ordering::Relaxed);
        unsafe { System.alloc(layout) }
    }
    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        FREES.fetch_add(1, Ordering::Relaxed);
        FREE_BYTES.fetch_add(layout.size() as u64, Ordering::Relaxed);
        unsafe { System.dealloc(ptr, layout) }
    }
    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        // Counted as one alloc of the new size + one free of the old (what a
        // grow-by-copy allocator like dlmalloc's fallback path does).
        ALLOCS.fetch_add(1, Ordering::Relaxed);
        ALLOC_BYTES.fetch_add(new_size as u64, Ordering::Relaxed);
        FREES.fetch_add(1, Ordering::Relaxed);
        FREE_BYTES.fetch_add(layout.size() as u64, Ordering::Relaxed);
        unsafe { System.realloc(ptr, layout, new_size) }
    }
}

#[global_allocator]
static COUNTING: Counting = Counting;

/// Heap traffic during `f`: (allocs, alloc_bytes, frees, free_bytes).
fn heap_traffic<R>(f: impl FnOnce() -> R) -> (u64, u64, u64, u64, R) {
    let a0 = ALLOCS.load(Ordering::Relaxed);
    let ab0 = ALLOC_BYTES.load(Ordering::Relaxed);
    let f0 = FREES.load(Ordering::Relaxed);
    let fb0 = FREE_BYTES.load(Ordering::Relaxed);
    let result = f();
    (
        ALLOCS.load(Ordering::Relaxed) - a0,
        ALLOC_BYTES.load(Ordering::Relaxed) - ab0,
        FREES.load(Ordering::Relaxed) - f0,
        FREE_BYTES.load(Ordering::Relaxed) - fb0,
        result,
    )
}

// ---------------------------------------------------------------------------------
// The workload
// ---------------------------------------------------------------------------------

/// The 87-byte demo line the board measurement used (the backspace-reparse slope).
const DEMO_LINE: &str =
    "net.rtl8125 --advertise-max 1000 $ net.l4.over-l2 --address dhcp $ curl http://yager.io";

/// The kernel store's /bin listing — `KERNEL_STORE_COMPONENTS` shell names
/// (xtask/src/main.rs), which is exactly what `snapshot_vocab` hands the editor on
/// the board (plus session bindings, zero on a fresh boot). 67 names today.
const BIN_NAMES: &[&str] = &[
    "eosh",
    "init",
    "restart.never",
    "restart.always",
    "restart.backoff",
    "hello",
    "time",
    "outcomes",
    "cruncher",
    "readwrite",
    "lspci",
    "entropy.seeded",
    "time.frozen",
    "disk.virtio",
    "fs.eofs",
    "fs.filtered",
    "fs.policy-subtree",
    "pci.filtered",
    "pci.admit-address",
    "pci.admit-vendor",
    "pci.none",
    "pci.deny",
    "platform.none",
    "platform.deny",
    "usb.ohci",
    "usb.ohci-pci",
    "usbcheck",
    "hidcheck",
    "platcheck",
    "usb.kbd",
    "sinkcheck",
    "net.virtio",
    "net.rtl8125",
    "l2check",
    "net.l2.switch",
    "vnicheck",
    "net.l2.bridge",
    "net.l4.over-l2",
    "l4check",
    "curl",
    "net.text",
    "telnetd",
    "oskexec",
    "vnic4check",
    "cancelcheck",
    "net.l2.deny",
    "net.l2.none",
    "net.l3.deny",
    "net.l3.none",
    "net.l4.deny",
    "net.l4.none",
    "net.l4.loopback",
    "net.l4.filtered",
    "net.policy-ports",
    "sockcheck",
    "ls",
    "cat",
    "echo",
    "wc",
    "head",
    "stat",
    "rm",
    "gpu.virtio",
    "gfx.mem",
    "gfx.none",
    "gfx.deny",
    "draw",
];

/// A vocabulary of `n` names: the real /bin names first (the demo line's words always
/// among them), synthetic `prog.NN` fillers beyond the real 67.
fn vocab_of(n: usize) -> Vocab {
    let mut entries: Vec<(String, Tag)> = Vec::new();
    for name in BIN_NAMES.iter().take(n) {
        entries.push((String::from(*name), Tag::Program));
    }
    let mut filler = 0usize;
    while entries.len() < n {
        entries.push((std::format!("prog.{filler:02}"), Tag::Program));
        filler += 1;
    }
    // The demo line's heads must stay resolvable at every width (the smallest widths
    // would otherwise drop them and change the tracking pattern).
    for needed in ["net.rtl8125", "net.l4.over-l2", "curl"] {
        if !entries.iter().any(|(word, _)| word == needed) {
            entries.pop();
            entries.push((String::from(needed), Tag::Program));
        }
    }
    Vocab::new(entries)
}

fn board_vocab() -> Vocab {
    vocab_of(BIN_NAMES.len())
}

/// Feed `prefix` bytes through a fresh grammar, returning each intermediate state
/// (index i = the state BEFORE byte i is stepped). All demo bytes are ASCII.
fn states_along(vocab: &Vocab, line: &[u8]) -> Vec<BoxP<()>> {
    let mut states = Vec::with_capacity(line.len() + 1);
    let mut current = command_line(vocab);
    for &byte in line {
        let next = current
            .step(Input::byte(byte).expect("demo line is ASCII"))
            .and_then(Step::cont)
            .expect("demo line is green");
        states.push(core::mem::replace(&mut current, next));
    }
    states.push(current);
    states
}

fn median(mut samples: Vec<u64>) -> u64 {
    samples.sort_unstable();
    samples[samples.len() / 2]
}

/// Median wall-clock nanoseconds of `f` over `reps` runs.
fn time_ns(reps: usize, mut f: impl FnMut()) -> u64 {
    let mut samples = Vec::with_capacity(reps);
    for _ in 0..reps {
        let t0 = Instant::now();
        f();
        samples.push(t0.elapsed().as_nanos() as u64);
    }
    median(samples)
}

// ---------------------------------------------------------------------------------
// Smoke (runs in ci): the counters count, the line is the line, the walk is green.
// ---------------------------------------------------------------------------------

#[test]
fn smoke_demo_line_is_green_and_counters_count() {
    assert_eq!(DEMO_LINE.len(), 87, "the board measurement's 87-byte line");
    assert_eq!(BIN_NAMES.len(), 67);
    let vocab = board_vocab();
    let (allocs, bytes, _, _, states) = heap_traffic(|| states_along(&vocab, DEMO_LINE.as_bytes()));
    assert_eq!(states.len(), 88);
    assert!(allocs > 0 && bytes > 0, "the allocator hook is live");
    // And the editor agrees the line is green.
    let mut editor = Editor::new("eosh> ", board_vocab(), Vec::new(), Marker::RED, None);
    for &byte in DEMO_LINE.as_bytes() {
        editor.handle(Key::Char(byte));
    }
    assert!(
        !editor.take_output().contains("\u{1b}[31m"),
        "line went red"
    );
}

// ---------------------------------------------------------------------------------
// 1. Census: heap traffic per keystroke + live-tree size
// ---------------------------------------------------------------------------------

#[test]
#[ignore = "measurement harness — run explicitly with --ignored --nocapture"]
fn census_allocs_per_keystroke() {
    let vocab = board_vocab();
    let states = states_along(&vocab, DEMO_LINE.as_bytes());

    eprintln!("\n== census: per-position heap traffic (vocab = 67 real names) ==");
    eprintln!("pos\tbyte\tstep_allocs\tstep_bytes\tcomp_allocs\tlive_nodes\tlive_bytes");
    let mut total_step_allocs = 0u64;
    let mut total_step_bytes = 0u64;
    for (i, &byte) in DEMO_LINE.as_bytes().iter().enumerate() {
        let state = &states[i];
        let input = Input::byte(byte).expect("ascii");
        let (sa, sb, _, _, _) = heap_traffic(|| {
            let _ = state.step(input);
        });
        let (ca, _, _, _, _) = heap_traffic(|| {
            let mut out: Vec<Completion> = Vec::new();
            state.completions(&mut out);
        });
        // Clone census: allocations during clone_box ≈ live heap blocks in the tree.
        let (la, lb, _, _, _) = heap_traffic(|| {
            let _ = state.clone_box();
        });
        total_step_allocs += sa;
        total_step_bytes += sb;
        eprintln!("{i}\t{:?}\t{sa}\t{sb}\t{ca}\t{la}\t{lb}", char::from(byte));
    }
    eprintln!(
        "TOTAL step traffic over the line: {total_step_allocs} allocs, {total_step_bytes} bytes \
         (mean {:.0} allocs / {:.0} bytes per keystroke)",
        total_step_allocs as f64 / 87.0,
        total_step_bytes as f64 / 87.0
    );

    // The editor-composite per keystroke (what the board actually runs per key:
    // tracker arming, step, the tracking completions walk, the snapshot push).
    eprintln!("\n== census: editor-composite heap traffic per keystroke ==");
    eprintln!("pos\tbyte\tallocs\talloc_bytes\tfrees\tfree_bytes");
    let mut editor = Editor::new("eosh> ", board_vocab(), Vec::new(), Marker::RED, None);
    let mut total = (0u64, 0u64);
    for (i, &byte) in DEMO_LINE.as_bytes().iter().enumerate() {
        let (a, ab, f, fb, _) = heap_traffic(|| {
            editor.handle(Key::Char(byte));
            editor.take_output();
        });
        total.0 += a;
        total.1 += ab;
        eprintln!("{i}\t{:?}\t{a}\t{ab}\t{f}\t{fb}", char::from(byte));
    }
    eprintln!(
        "TOTAL editor traffic: {} allocs, {} bytes (mean {:.0} allocs / {:.0} bytes per key)",
        total.0,
        total.1,
        total.0 as f64 / 87.0,
        total.1 as f64 / 87.0
    );
}

// ---------------------------------------------------------------------------------
// 2. Phase timing: step vs admissible vs completions vs clone_box per position
// ---------------------------------------------------------------------------------

#[test]
#[ignore = "measurement harness — run explicitly with --ignored --nocapture"]
fn phase_timing_per_position() {
    const REPS: usize = 25;
    let vocab = board_vocab();
    let states = states_along(&vocab, DEMO_LINE.as_bytes());

    eprintln!("\n== phases: median ns per call, per position (vocab = 67, reps = {REPS}) ==");
    eprintln!("pos\tbyte\tstep_ns\tadmissible_ns\tcompletions_ns\tclone_ns");
    let mut totals = (0u64, 0u64, 0u64, 0u64);
    for (i, &byte) in DEMO_LINE.as_bytes().iter().enumerate() {
        let state = &states[i];
        let input = Input::byte(byte).expect("ascii");
        let step_ns = time_ns(REPS, || {
            let _ = state.step(input);
        });
        let adm_ns = time_ns(REPS, || {
            let _ = state.admissible();
        });
        let comp_ns = time_ns(REPS, || {
            let mut out: Vec<Completion> = Vec::new();
            state.completions(&mut out);
        });
        let clone_ns = time_ns(REPS, || {
            let _ = state.clone_box();
        });
        totals.0 += step_ns;
        totals.1 += adm_ns;
        totals.2 += comp_ns;
        totals.3 += clone_ns;
        eprintln!(
            "{i}\t{:?}\t{step_ns}\t{adm_ns}\t{comp_ns}\t{clone_ns}",
            char::from(byte)
        );
    }
    eprintln!(
        "TOTAL over the line (µs): step {:.1}, admissible {:.1}, completions {:.1}, clone {:.1}",
        totals.0 as f64 / 1e3,
        totals.1 as f64 / 1e3,
        totals.2 as f64 / 1e3,
        totals.3 as f64 / 1e3
    );

    // The editor-composite per keystroke, timed end to end (one fresh editor per rep
    // so the snapshot stack does not grow rep over rep).
    let comp_ns = time_ns(REPS, || {
        let mut editor = Editor::new("eosh> ", board_vocab(), Vec::new(), Marker::RED, None);
        for &byte in DEMO_LINE.as_bytes() {
            editor.handle(Key::Char(byte));
            editor.take_output();
        }
    });
    eprintln!(
        "editor composite, whole 87-byte line: {:.1} µs ({:.2} µs/key median-of-{REPS})",
        comp_ns as f64 / 1e3,
        comp_ns as f64 / 87.0 / 1e3
    );

    // Per-position editor-composite cost (the host twin of the QEMU echo-latency
    // probe): for each position, replay the prefix into a fresh editor (untimed),
    // then time the one keystroke. This is the number the guest-side per-key echo
    // latency should be compared against, class by class (tracked name words vs
    // flag/value bytes).
    eprintln!("\n== per-position editor-composite keystroke cost ==");
    eprintln!("pos\tbyte\tkey_ns");
    for (i, &byte) in DEMO_LINE.as_bytes().iter().enumerate() {
        let mut samples = Vec::with_capacity(REPS);
        for _ in 0..REPS {
            let mut editor = Editor::new("eosh> ", board_vocab(), Vec::new(), Marker::RED, None);
            for &prefix_byte in &DEMO_LINE.as_bytes()[..i] {
                editor.handle(Key::Char(prefix_byte));
            }
            editor.take_output();
            let t0 = Instant::now();
            editor.handle(Key::Char(byte));
            samples.push(t0.elapsed().as_nanos() as u64);
        }
        eprintln!("{i}\t{:?}\t{}", char::from(byte), median(samples));
    }
}

// ---------------------------------------------------------------------------------
// 3. Vocabulary-width scaling
// ---------------------------------------------------------------------------------

#[test]
#[ignore = "measurement harness — run explicitly with --ignored --nocapture"]
fn width_scaling() {
    const REPS: usize = 15;
    eprintln!("\n== width: whole-line editor composite vs vocabulary size (reps = {REPS}) ==");
    eprintln!("vocab\tline_us\tper_key_us\tstep_allocs_total\tlive_nodes_at_end");
    for width in [4usize, 16, 38, 67, 100] {
        let line_ns = time_ns(REPS, || {
            let mut editor = Editor::new("eosh> ", vocab_of(width), Vec::new(), Marker::RED, None);
            for &byte in DEMO_LINE.as_bytes() {
                editor.handle(Key::Char(byte));
                editor.take_output();
            }
        });
        let vocab = vocab_of(width);
        let (allocs, _, _, _, states) = heap_traffic(|| states_along(&vocab, DEMO_LINE.as_bytes()));
        let (live, _, _, _, _) = heap_traffic(|| {
            let _ = states.last().expect("nonempty").clone_box();
        });
        eprintln!(
            "{width}\t{:.1}\t{:.2}\t{allocs}\t{live}",
            line_ns as f64 / 1e3,
            line_ns as f64 / 87.0 / 1e3
        );
    }
}
