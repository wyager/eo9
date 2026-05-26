# 16 — Embeddable runtime (`eo9-embed`)

## Scope
A library crate, `crates/eo9-embed`, that lets any Rust program embed a usermode Eo9
instance: choose a capability environment and run components to a three-way outcome, with
the root providers supplied by a pluggable *backend*. It is the reusable core behind two
later deliverables:

- **Bundle milestone** (`eo9 bundle`, PLAN.md): ship an Eo9 program as a native executable
  for another OS — a launcher links `eo9-embed`, grants exactly the authority the program
  was given, and runs it.
- **/try v2** (plan/15 Decisions 15–20): the wasm32 + Pulley browser blob is `eo9-embed`
  compiled to wasm32 with a browser-backed provider backend.

Out of scope for this area: the `eo9 bundle` command itself, the wasm32/Pulley backend,
and the compile-cache integration (the `eo9` binary's `compile.rs`) — all recorded as
follow-ups below.

## Spec references
"Eo9-as-program", "Execution APIs" (the host side of `compile`/`task`, the loader rule,
three-way `program-outcome`), the capability algebra (`$`/`&`/`only`/`configure`), and the
Bundle target in PLAN.md.

## Public API (as built)
- `Eo9` / `Eo9::builder() -> Builder` — the instance. `run_bytes`, `run_path`,
  `run_component` (the last takes an already-composed `Component`), and `describe`.
- `Builder` — `grants(Grants)`, `grant_fs(bool)`, `grant_exec(bool)`, `limits`/`max_memory`,
  `debug_info`, `backend(impl ProviderSource)`, `build() -> Result<Eo9, EmbedError>`.
- `Grants { text, time, entropy, fs, exec }` — default text+time+entropy on, fs+exec off
  (the safe minimal useful set); `Grants::none()`.
- `ProviderSource` trait + `Roots { text, time, entropy, fs }` — the backend seam.
- `Sandbox` — deterministic in-memory backend (captured text, frozen clock, seeded RNG,
  in-memory fs); inspectable via `stdout`/`stderr`/`file_contents`, configurable seed/clock.
- `Host` (feature `host`, default on) — host-OS backend (stdio/wall-clock/OS-RNG, and a
  rooted host fs via `with_fs_root`); `ExecSnapshotPolicy` re-exported.
- `EmbedError`, `render_outcome`, and re-exports of `Outcome`, `WaveValue`, `NamedArg`,
  `Component`, `ComponentInfo`, and the algebra (`compose`/`extend`/`restrict`/`configure`).

## Decisions

1. **Composed directly; did not refactor the binary (yet).** The completion-callback →
   future provider bridge lives in the `eo9` binary (`crates/eo9/src/providers.rs`). Per
   PLAN.md ground rules and the area brief, refactoring the binary to share it was judged
   more than a modest change for this pass (it threads through `run.rs`/`shell.rs` and the
   session/exec wiring), so `eo9-embed` composes the lower-level crates directly and the
   host backend's bridge is a faithful copy of the binary's, kept deliberately identical.
   **Follow-up:** make the `eo9` binary depend on `eo9-embed` for its root providers and
   drive loop, deleting the duplicate bridge. The binary keeps the CLI-specific pieces
   (session manifest, the shell's `ChildPolicy`/`ExecProvider` wiring). No other crate was
   modified; the only out-of-crate change is one `[workspace.dependencies]` line.

2. **Two backends behind a `ProviderSource` trait.** `Sandbox` (in-memory, deterministic,
   no host access) is the default-portable backend and the exact shape the wasm32/Pulley
   path needs; `Host` (default feature `host`) is the real host-OS backend. Making the
   backend a trait object means the **wasm32 path is a new `ProviderSource` impl** —
   browser-backed text (a terminal element), time (`performance`/`Date`), entropy
   (`crypto.getRandomValues`), and a memory-backed fs — plus building `eo9-embed` with
   `--no-default-features` (verified to compile: drops the `eo9-providers-unix` dep, keeps
   only `Sandbox`). No API change is needed to add it.

3. **Capability model mirrors the CLI exactly.** Grants are opt-in for fs and exec; a
   program that *requires* `eo9:fs` without an fs grant is refused up front with a clear
   message (not the raw linker error); the runtime's loader rule still governs (only
   imported interfaces are linked, an unsatisfied import is a spawn error). Children of an
   exec-holding program receive the same roots **minus exec** (exec is never inherited) —
   implemented as the exec `ChildPolicy`'s provider factory calling the backend with the
   child grant set, degrading fs gracefully (drop fs, then no caps) since the factory
   cannot surface errors.

4. **`Sandbox` shares state across clones; entropy resets per run.** The captured-text
   buffers and in-memory fs are `Arc`-shared so a clone handed to `backend()` is still
   inspectable after a run; the seeded PRNG is re-seeded at the start of every run, so each
   run is reproducible. Text/fs state therefore accumulates across multiple runs of one
   `Sandbox` — fine for the common single-run embed; documented on the type.

5. **An engine is created per run; no compile cache yet.** `run_*` creates a fresh engine
   and compiles the component each call. This keeps `eo9-embed` free of `eo9-store` and
   avoids exposing the wasmtime `Engine` type in the public API. **Follow-ups:** (a) reuse
   one engine across runs (needs an `Engine` re-export or an opaque handle from
   eo9-runtime); (b) optional compile-cache integration (the binary's `compile.rs` logic
   over `eo9-store`) behind a builder option. Both are additive.

6. **Tests run real guest components.** The end-to-end suite builds the guest workspace on
   demand (same convention as `tests/eo9-integration`) and runs `hello` (captured
   greeting), `cruncher` (deterministic across runs), `readwrite` (full round-trip through
   the in-memory fs), the missing-fs refusal, and the Host-fs-without-root error. The exec
   capability is wired and compiles but is not yet exercised end to end here (it needs an
   exec-capable guest such as eosh); a follow-up test should run a child through `Host`.

## Milestones
1. **(done)** The crate: builder API, `Sandbox` + `Host` backends, example, end-to-end
   tests, CI green.
2. Consolidate the binary onto `eo9-embed` (Decision 1 follow-up).
3. Engine reuse + optional compile-cache integration (Decision 5).
4. The wasm32/Pulley backend (plan/15) once the fiber question is resolved; then `eo9
   bundle` on top.
