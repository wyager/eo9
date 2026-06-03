# Can ownership enforce interrupt sequencing? A prototype and an honest verdict

Companion to `irq-audit.md`, which walked every interrupt-edge site and found 0 bugs, 4
SOUND-BUT-FRAGILE clusters, and a large SOUND majority whose soundness comes from three
structural families (masked-halt brackets, sticky counters + guaranteed re-polls, bounded
fallbacks). The question here: can Rust's ownership/typestate machinery turn the fragile
sites' load-bearing comments into compile errors, at zero runtime cost — and is the trade
worth taking?

## The prototype: the PLIC claim as a linear token

The riscv64 PLIC was the best candidate because the hardware itself already speaks the
protocol we want to enforce: `claim` hands the handler an interrupt source ID, and the
gateway withholds every further delivery of that source until the same ID is written back to
`complete`. A claim that is never completed is a **permanently stranded source** — for UART0
that is a console deaf forever, with no rescue path (audit entry B1). Before the prototype,
the obligation lived entirely in the shape of one loop body.

`plic::claim()` now returns `Option<Claim>` — a `#[must_use]` newtype over `NonZeroU32` —
and `plic::complete(claim)` consumes it by value. What each failure mode becomes:

| Failure | Before | After |
|---|---|---|
| Complete without claiming | possible (any `u32`) | **impossible** (only `claim()` constructs a `Claim`) |
| Complete twice | silent (PLIC ignores it) | **compile error** (use after move) |
| Forget to complete (early `continue`/`break`/`?` in the loop body) | silent permanent deafness of the source | `#[must_use]` lint at the call site; **debug-build panic at the drop site** naming the stranded source; release builds unaffected |
| Reorder service vs. complete | possible | still possible — see "what typestate cannot do" |

Zero-cost, verified three ways: `Option<Claim>` is const-asserted to be exactly the size of
the raw register read (the `NonZeroU32` niche encodes `None` as the hardware's own
0-sentinel, so the `while let Some(claim)` loop compiles to the same compare-with-zero as
the old `if source == 0 break`); the release build carries no `Drop` impl at all (the loud
drop is `#[cfg(debug_assertions)]`); and a clean A/B build of the riscv64 kernel shows
`ktrap` — the one function that touches the token — **byte-size identical (686 bytes both
sides)**, with the total `.text` delta (+56 bytes of 9.4 MB, 0.0006%) attributable to
codegen-unit repartitioning noise (the same A/B shows unrelated functions like `idle_wait`
changing size, which a 12-line PLIC edit cannot cause directly).

## What typestate CAN enforce here

* **Must-consume obligations** — the claim/EOI shape: a value created by acknowledge that
  must reach exactly one completion call. This is the strongest fit, because the obligation
  is *create → consume exactly once*, which is precisely what affine types + `#[must_use]` +
  a debug drop-bomb approximate. The two no-rescue audit entries (A6 GIC IAR→EOIR, B1 PLIC
  claim→complete) are both this shape.
* **Call-order within one scope** — ack-before-drain (A1): an `ack()` returning a ZST token
  that `drain(&Acked)` requires would make the paste-freeze reorder a compile error. Cheap,
  but see the honesty notes below.
* **Capability gating** — "this operation is only legal inside a masked bracket" (A3/B4
  scavenger exclusivity): a `MaskedSection` RAII guard whose existence is required by the
  ring-producer functions. Possible today; the current mask brackets are 10 lines and local,
  so the win is small.

## What typestate CANNOT enforce (the honesty section)

* **The choice of correct order.** The type system enforces the order you *encode*, not its
  correctness against the hardware model. The paste-freeze bug would only have been caught
  if someone had already understood that ack-must-precede-drain and encoded it — but the
  same understanding expressed as a comment is what we have now. Typestate protects the
  *next* engineer from silently breaking a *known* invariant; it discovers nothing.
* **Rust has affine types, not linear ones.** A value can always be dropped. `#[must_use]`
  only fires on an unbound result; a claim bound to a variable and then leaked via an early
  exit is caught only by the debug drop-bomb at runtime, not at compile time. The compile
  error covers double-complete and complete-without-claim; the *forget* direction — the
  dangerous one — is a lint plus a debug panic, which is much better than silence but is
  not the "compile error" the pitch implies.
* **Cross-context invariants.** "The handler masks the line, the wait-future unmasks it
  later, the drop path masks it again" (A8) spans an interrupt handler, a poll function,
  and a destructor across two modules. No ownership story connects those; the soundness
  there comes from the sticky-counter design, and a token would add ceremony without adding
  a guarantee.
* **Hardware-side state.** Whether the FIFO is empty, whether the level line has dropped,
  whether the device honors a suppression hint — invisible to types by nature.

## Cost, measured against the audit

The audit found exactly **two sites where the token shape adds a real guarantee that
nothing else provides** (A6, B1 — the no-rescue must-consume pairs) and one more where it
would re-encode a now-well-documented invariant with an existing rescue (A1, scavenger).
Everything else is already structurally sound, and several sites (A8, C4) are sound
*because of designs that typestate cannot express anyway*. The prototype cost ~60 lines
including documentation for one site, compiled to identical code, and required no `unsafe`
and no handler restructuring. The GIC equivalent (an `Ack(iar)` token consumed by
`end_of_interrupt`) would be the same size.

## Verdict: ADOPT-FOR-THE-TOKEN-SHAPE, NOT-WORTH-IT as a general program

* **Adopt** the linear-token pattern for the two must-consume hardware protocols — the PLIC
  claim (done, this branch) and the GIC IAR/EOIR pair (same shape, same ~60 lines, queued as
  a follow-up rather than done here so the prototype's verdict could be evaluated on one
  site first). These are the sites where a one-line refactor mistake produces an
  undebuggable permanently-deaf machine and nothing else catches it.
* **Do not** retrofit typestate across the remaining interrupt code. The audit's SOUND
  majority is sound for structural reasons (masked brackets, sticky counters, bounded
  fallbacks) that tokens neither strengthen nor express, and the FRAGILE-with-rescue site
  (A1) is better served by its existing scavenger backstop plus its now-extensive comments
  than by ceremony that cannot encode *why* the order is what it is.
* **For new interrupt code**, the priority order remains: (1) reach for one of the three
  structural soundness families first — they make order *non-load-bearing*, which beats
  enforcing it; (2) where a hardware protocol hands you a must-consume obligation, wrap it
  in a token from day one; (3) where neither applies, write the invariant down at the site
  in the audit's format (invariant / order / rescue / consequence).

The honest summary for the owner: this was worth one site, probably two, and the exercise's
main value was the audit itself — the inventory of which orderings are load-bearing and
what rescues exist is worth more than the types are.
