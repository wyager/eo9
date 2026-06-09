# Parser step cost (area/33-step-cost-study) — 2026-06-09

## Verdict

**The parser does not cost 50 ms a keystroke — it costs about a tenth of a
millisecond under our own wasm stack at native speed, and the measured layers
account for at most ~1% of the board's ~53 ms/char.** Per keystroke on the 87-byte
demo line with the real 67-name /bin vocabulary:

* the `step()` successor-tree construction itself is **~30 allocs / ~1 KiB / 0.2–0.6 µs**
  (host native, release) — the live state tree is **9–25 heap blocks (~0.3–0.8 KiB)**,
  i.e. tiny;
* what dominates the parser-side cost is not `step` but the **oracle walks** the
  editor layers on top of it: `completions()` is ~18× and `admissible()` ~9× the
  whole-line cost of `step`, and the M3 name-mark oracle runs up to two
  `completions()` walks per typed word character (**~820 allocs / ~24 KiB / ~14 µs**
  per tracked keystroke, ~95% of the keystroke's heap churn);
* the same component bytes under **our kernel + on-target cranelift + in-wasm
  dlmalloc on QEMU aarch64 with HVF** (native ISA speed) cost **~98 µs net** per
  tracked keystroke — a **wasm tax of ~7× per keystroke (~10× whole-line)** over
  host-native rustc;
* scaling that to the A76 (2–4× slower core) predicts **~0.2–0.4 ms** per tracked
  keystroke on the board — versus the observed ~53 ms/char. The observed cost is
  also **flat across work mixes** on the board (O(1) backspace with *zero* parser
  work measures ~54 ms, same as forward typing with step + two oracle walks), which
  parser compute cannot produce. The GAPS entry "Editor parser step costs ~50 ms on
  target" is therefore **re-attributed**: the residue lives in a board-only layer
  (event delivery / per-key wake / echo path / a board-side execution-speed anomaly),
  and the discriminating probes belong to a board lane (§6).

The one genuinely user-perceptible parser-lane cost found: the **word-end
`provide_args` full-line rebuild** — 1.4 → 2.8 → 3.0 ms (net, HVF) at the demo
line's three word ends, growing with line length (O(N) replay + argument
resolution). Board-scaled that is a 5–10 ms hiccup at the first boundary after each
program name.

Everything below is measured, not modeled. Harness: `guest/eosh/eosh-inc/tests/step_cost.rs`
(host census + phase timing + width sweep; `#[ignore]`d, zero cost in ci and in
shipped builds — integration tests never enter the component build) and
`tests/step-cost/echolat.py` (QEMU echo-latency probe with built-in transport
controls). Repro commands in §7.

Machine: Apple Silicon host (aarch64-apple-darwin, release builds);
QEMU `virt` GICv2 `highmem=off`, 512 MiB, single core; HVF (`-cpu host`) for native
execution, TCG (`-cpu max`) as a slow-CPU proxy. Workload: the board demo line

```
net.rtl8125 --advertise-max 1000 $ net.l4.over-l2 --address dhcp $ curl http://yager.io
```

(87 bytes), vocabulary = the kernel store's real /bin listing (67 names from
`KERNEL_STORE_COMPONENTS`, exactly what `snapshot_vocab` hands the editor on a fresh
boot).

## 1. Tree-shape census (host)

Counting global allocator around the real code paths. Keystroke classes (the line
has four):

| class | positions (example) | step allocs | step bytes | live tree after (blocks / bytes) |
|---|---|---|---|---|
| tracked name-word byte | `n e t . r t l …` | 13–17 | ~0.4–0.6 KiB | 12–16 / 0.35–0.5 KiB |
| flag/value byte | `a d v e r …`, `h t t p …` | 9–12 | ~0.3 KiB | 9–11 / ~0.3 KiB |
| word-boundary space | after each word | 10–83 | up to 2.6 KiB | 9–16 |
| grammar expansion (`' '` after `$`) | pos 34, 66 | **518–520** | **~20 KiB** | 5–7 / ~0.2 KiB |

* Whole line, `step` only: **2,576 allocs / 90.6 KiB** (mean 30 allocs / 1.0 KiB per
  keystroke).
* Live state tree (clone census — `clone_box` re-boxes every node, so allocations
  during a clone ≈ heap blocks in the tree): **9–25 blocks, 240–780 bytes**. The
  snapshot stack (one state per green char, M3) therefore holds ~30 KiB for the whole
  87-char line. Memory is a non-issue.
* **Editor composite** (the real `Editor::handle` per key, i.e. tracker arming +
  step + the M3 name-mark oracle): mean **308 allocs / 9.5 KiB per keystroke**;
  tracked name-word keys are **816–836 allocs / 23–24 KiB** — of which the
  `completions()` walk is 763–818 allocs. **~95% of per-keystroke heap churn is the
  oracle walks, not the step.**

The expansion-key spike (518 allocs) is the `Bind` right-hand side being built when
`$ ` commits to a new `expr`: every lazy/bind expansion reconstructs the alternation
spine, including the per-position `hint_words` vocabulary filter. It is also the
single most expensive `step` (16.5 µs host) — visible as the priciest non-word-end
keys in the QEMU run too.

## 2. Per-phase host timing

Median-of-25, release, per call, on the state before each keystroke; line totals:

| phase | typical per call | worst per call | Σ over the 87-byte line |
|---|---|---|---|
| `step` | 0.2–0.6 µs | 16.5 µs (`' '` after `$`) | **68 µs** |
| `admissible()` | 1–15 µs | 22.7 µs | **613 µs** (9× step) |
| `completions()` | 2–30 µs | 39.7 µs | **1,226 µs** (18× step) |
| `clone_box()` | 0.15–0.5 µs | 0.7 µs | **22 µs** |

Editor composite (what actually runs per key): **553 µs / line = 6.4 µs/key mean**;
by class: tracked name-word key **~14 µs**, word-start key 2.3–2.9 µs, expansion key
6.6–7.0 µs, flag/value key **~0.1 µs**.

Why the oracles dwarf the step: `Bind::admissible`/`Bind::completions` do the
Eof-peek — `p1.step(Eof)`, then **construct the right-hand side** (`f(value)`, which
builds grammar subtrees, vocabulary filters included) and recurse into it, per call,
per keystroke, and the construction cascades through nested binds. The step only
advances the live spine; the oracles repeatedly rebuild and discard the grammar's
future.

**The removed M2 clone, retroactively:** `clone_box` totals 22 µs against the 553 µs
composite — **~4% per keystroke (~0.3 µs of 14 µs on tracked keys)**. M3's removal
was correct hygiene but could never have moved a 53 ms number; the board bench
indeed observed "typing echo unchanged by M3's clone removal".

## 3. Vocabulary-width scaling

Same line, vocabulary 4 → 100 names (real /bin names first, the demo line's three
heads always present, synthetic `prog.NN` fillers beyond 67):

| vocab | line composite | per key | `net.`-prefixed entries alive while typing |
|---|---|---|---|
| 4 | 43.8 µs | 0.50 µs | 2 |
| 16 | 45.0 µs | 0.52 µs | 2 |
| 38 | 339.6 µs | 3.90 µs | 5 |
| 67 | 541.8 µs | 6.23 µs | 15 |
| 100 | 517.4 µs | 5.95 µs | 15 |

**Cost is linear in the number of vocabulary entries sharing the typed word's
prefix, and flat in total width beyond them** (100 ≈ 67 because the fillers share no
prefix with `net.*`). A log-log fit over the sweep gives ~width^0.8, but the
mechanism is the alive-set size, not raw width: `Words` is already a single shared
node with an alive-index vector, so dead entries cost nothing per step. There is no
superlinear blowup to fix at realistic vocabulary sizes.

## 4. The codegen/runtime tax (our wasm stack on QEMU aarch64)

`tests/step-cost/echolat.py` boots the real kernel image, types the demo line at the
real eosh prompt, and times each byte's write→echo round trip. The guest echoes
*after* the keystroke's parser work, so echo latency = transport + editor cost.
Two in-band controls isolate transport: a **red region** (after a dead char the
editor stops stepping the parser entirely — pure transport + O(1) bookkeeping) and a
**comment body** (real step over a near-empty tree).

| | HVF (native speed) | TCG (~10× slower proxy) |
|---|---|---|
| transport baseline (red control, median) | 112 µs | 205 µs |
| comment control | 108 µs | 215 µs |
| tracked name-word key, net of baseline | **98 µs** | 941 µs |
| flag/value key, net | ~16 µs (noise-bounded) | 67 µs |
| expansion key (`' '` after `$`), net | ~102 µs | 753 µs |
| word-end `provide_args` keys, net | **1.4 / 2.8 / 3.0 ms** | 28 ms |

Compute scales ~10× from HVF to TCG while the controls stay flat — the
decomposition is sound (the deltas are guest compute, not transport).

**The tax** (HVF net ÷ host-native, same per-key work):

* tracked name-word key: 98 µs / 14 µs ≈ **7×**
* whole line excluding the three word-end keys: 4.7 ms / 0.46 ms ≈ **10×**
* expansion keys: ~15× (noisier; allocation-heaviest class, consistent with dlmalloc
  being the costliest part of the tax)

That ~7–10× bundles cranelift-vs-LLVM codegen, in-wasm dlmalloc vs the host
allocator, and `Box<dyn>`-indirection amplification. It is exactly the kind of
multiplier our stack should expect — and it is **three orders of magnitude short of
explaining the board**.

**Word-end spikes:** the first boundary after each typed program name runs
`wanted_args` → session resolution (memoized describe + manual) → `provide_args` →
**`Editor::rebuild()`, a full O(N) replay of the line** (including the per-char
oracle walks). The three spikes grow 1.4 → 2.8 → 3.0 ms with prefix length, i.e. the
replay dominates. This is the only parser-lane cost that projects to a perceptible
board hiccup (~5–10 ms).

## 5. Board reconciliation — where the GAPS entry lands

Putting the layers together for one tracked keystroke:

| layer | cost (cycles, normalized) |
|---|---|
| step successor construction | host 0.45 µs ≈ ~1.6k cycles |
| + the two M3 oracle walks (editor composite) | host 14 µs ≈ ~50k cycles |
| + our wasm stack (cranelift + dlmalloc), native speed | 98 µs ≈ ~340k cycles |
| × A76 scaling (2–4×, clock + width) | **0.2–0.4 ms predicted on board** |
| observed on board | **~53 ms ≈ 122M cycles** |

The measured parser stack explains **≤1%** of the board number. Three board-side
facts (bench notes, 2026-06-09) independently corroborate that the residue is not
parser compute:

1. **O(1) backspace costs the same as typing** (~54 ms vs ~53 ms). Post-M3 backspace
   pops a snapshot — no step, no oracle walk, no clone — yet measures identically.
   One latency across wildly different work mixes is the signature of a per-event
   floor, not compute.
2. **M3's clone removal changed nothing** (~53 ms before and after) — consistent
   with the clone's measured ~4% share, inconsistent with combinator churn being the
   driver.
3. **The pre-M3 backspace slope had a ~0.85 s fixed intercept per press**
   (1.3 s @ 10 chars → 4.75 s @ 87 chars ⇒ ~45 ms/char + ~0.85 s). The kernel's idle
   backstop is 1 s (`IDLE_BACKSTOP_NS`), and day-one board images delivered UART RX
   only through the idle-path `scavenge_rx` poll (the 64-byte-FIFO truncation era,
   GAPS 2026-06-08). A keystroke rescued by the backstop instead of an interrupt is
   exactly a ~0–1 s fixed wait — the timer-flush-is-a-bug rule applies: that
   intercept is a **liveness finding** about the event path at bench time, not a
   parser cost.

What this study **cannot** resolve off-board: the pre-M3 slope's ~45 ms per
*replayed* char happened inside one `rebuild()` call (no I/O per char), and forward
typing still measures ~53 ms/char on the M3 image with the RX interrupt armed. If
those numbers are real compute, the board executes this wasm ~300–500× slower than
HVF executes the same bytes — far beyond CPU class, and pointing at something
categorical (e.g. cache-ineffective memory attributes on the wasm heap, or the probe
measuring a pacing floor). Discriminating probes, all cheap, all board-lane:

* **In-guest timing**: a `cfg`-gated eosh bench builtin that times 87 ×
  `editor.handle(Key::Char(…))` with the monotonic clock and prints one number —
  separates guest compute from everything outside the component.
* **Probe-floor check**: measure isolated-keystroke echo latency vs a 2-byte burst —
  a per-event floor doubles the burst's first-byte latency only; compute doubles both.
* **Memory-attribute check**: an in-guest 1 MiB memcpy/checksum loop timed with the
  monotonic clock (~0.1 ms cached, ~10+ ms uncached — a one-line discriminator for
  the cache hypothesis).
* **FTDI/pacing audit of the bench harness**: the adapter's default 16 ms latency
  timer quantizes host-side echo timestamps; `eosh_cmd.py`-era pacing did too.

*Status since this study (bench, 2026-06-09): the burst probe has run — flag/value
keys drop to ~3 ms/char in bursts while name-position keys stay ~46 ms isolated AND
in bursts — and the residue's active hypothesis lane is `area/34-fuel-yield-latency`
(H1: fuel-yield quantization riding the kbd service's poll timer).*

## 6. Mitigation ladder, sized by the data

Ordered by measured leverage per unit of risk. (a) and the word-end fix attack the
~95% (oracle walks); arenas attack the remainder; (c)/(d) are not currently
justified by data.

| rung | what | sized by | expected multiplier |
|---|---|---|---|
| **(a) non-allocating, early-exit name oracle** | replace `name_completions`' two full `completions()` walks (Vec of cloned `String`s, cascading Bind RHS construction) with a boolean query that stops at the first name-tagged candidate and skips RHS construction once answered; cache the word-start arming answer | tracked key = 14 µs of which ~13.5 µs oracle; 820 → ~40 allocs/key | **5–7× on tracked keys, 3–4× whole line, ~95% of heap churn gone** |
| **(a′) incremental `provide_args` re-arm** | word-end resolution currently calls `rebuild()` — O(N) replay with oracle walks per char; only the in-progress application's argument layer actually changed. Re-arm from the predecessor snapshot (or defer the re-arm to TAB, where a list repaint already happens) | the 1.4–3.0 ms HVF spikes (≈5–10 ms board-scaled), growing with line length | **removes the only perceptible parser-lane hiccup** |
| **(b) bump-arena for walk/step scratch** | after (a): step still allocs 30/key (518 on expansion keys) into dlmalloc in-wasm. States persist on the snapshot stack, so the arena scopes to oracle/step temporaries, not states | expansion key 16.5 µs host / ~15× wasm tax (alloc-heaviest class measured) | **1.5–2× on step-heavy keys; shrinks the wasm tax's dlmalloc share** |
| **(c) trie the vocabulary** | width sweep says cost is already linear in prefix-sharing entries and flat in total width at ≤100 names; `Words` is already one shared node | 67 → 100 names: 542 → 517 µs (flat) | **≤1.2× today — defer until the vocabulary is ≫100 names** |
| **(d) fixed-size no_alloc states (audio2 shape)** | eliminates boxing, dyn dispatch and all allocation; host tracked key ~14 µs → ~1–2 µs by construction | bounded by what remains after (a)+(b): ~2–4 µs/key host, ~15–25 µs wasm | **3–5× more, at full-rewrite cost — only if the board probe pins the residue on the parser after all** |

Recommended order: **(a) → (a′) → (b)**, then stop and re-measure on the board with
the §5 probes before considering (d); (c) only on vocabulary growth. None of these
move the board's ~53 ms/char if §5's re-attribution holds — the board probes come
first in any keystroke-feel lane.

## 7. Reproduction

```sh
# host census + phases + width sweep (release; ~2 s total)
cd guest && cargo test -p eosh-inc --target aarch64-apple-darwin --release \
    --test step_cost -- --ignored --nocapture --test-threads 1

# wasm-under-our-stack echo latency (build the image first)
cargo xtask build-kernel aarch64
python3 tests/step-cost/echolat.py --accel hvf   # native-speed run
python3 tests/step-cost/echolat.py --accel tcg   # slow-CPU proxy run
```

The harness test file is `#[ignore]`d (a fast smoke test keeps it compiling in ci);
the python probe drives only QEMU and never touches a serial device.
