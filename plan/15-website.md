# 15 — Website (`www/`) and the in-browser demo

## Scope
The public eo9.org site and its standalone server (already on master; design and operational notes live in
`www/README.md`), plus the **/try page**: a page where a visitor runs the real Eo9 guest components — the
eosh shell, the standard stubs, the example programs — in the browser's own wasm engine, against a small
hand-written JS "browser host" that plays the role the usermode runtime plays natively. Everything runs
client-side; the server only serves static files. Out of scope: anything server-side-executed, and any
"terminal" that merely imitates eosh — if the real thing cannot run, this page does not ship.

## Spec references
"Eo9-as-program", "WASM runtime", "Composition and the `$` operator", "The capability algebra",
Arguments-and-outcomes (WAVE), "Shell".

## Deliverables (browser demo)
- `cargo xtask build-web-demo`: build the guest components (plan 07/09/10), transpile them to
  browser-runnable ES modules + core wasm, emit the static `/try` assets under `www/site/try/`.
- The browser host: text → in-page terminal, time/entropy → Web APIs, and (for eosh) the `eo9:exec`
  surface — component-algebra/compile/task implemented over the transpiled-component graph, plus a small
  read-only "store" of prebuilt components fetched over HTTP.
- The `/try` page itself: terminal, an honest statement of what is real and what is absent, suggested
  commands.
- CI stays green without node and without the built demo assets.

## Milestones
1. Feasibility spike: transpile the real components, run them in a JS engine against a hand-written host
   (**done** — see Decisions 2–5).
2. `/try` v1: the example binaries running on the page via a minimal launcher + browser host; honest
   framing; no eosh claim (**done** — see Decisions 8–13; ships without stub providers, see Decision 10).
3. `/try` v2: the real eosh REPL — requires the JS exec host (component-algebra over the transpiled-module
   graph, spawn/wait/kill, WAVE argument checking) and the store-backed name resolution eosh expects;
   stub-provider composition rides on the same work (see Decision 14).
4. Polish: bundle-size reduction (shared intrinsics), browser-support matrix, suggested-command tour.

## Decisions

1. **Transpilation tooling comes from crates.io, not npm.** The natural tool is jco, but the
   `@bytecodealliance` npm scope does not resolve through the npm registry configured on the build machine
   (404: not found / not permitted), and overriding the registry was declined at permission review. jco's
   transpiler core, however, is the Rust crate **`js-component-bindgen`** (1.19.x on crates.io, same
   wasm-tools/wasmtime-environ family the repo already uses), so the plan is a small build-time helper crate
   (e.g. `www/try-build`, outside the root workspace like `www/` itself) that calls
   `js_component_bindgen::transpile()` directly — no node/npm in the transpile path at all. npm is then
   needed only for the terminal widget; `@xterm/xterm` 6.0.0 *does* resolve through the configured registry,
   and vendoring it as two pinned static files is an acceptable alternative. New dependency needs planner
   approval before milestone 2 starts.
2. **Feasibility: verified end to end for the sync ABI.** The real `eo9-example-hello` component (the exact
   artifact `xtask build-guest` produces), transpiled with js-component-bindgen 1.19.3 (instantiation mode)
   and run in a JS engine (node 25 / V8) against a ~25-line hand-written host providing `eo9:text` and
   `eo9:time`, prints through the text capability and returns `greeted`. `outcomes` and `cruncher` are the
   same shape (cruncher imports nothing).
3. **Feasibility: the CM-async ABI works in the transpiled output.** `eo9-example-readwrite`
   (`main: async func`, async-lowered `eo9:fs` imports, owned-buffer round-trip) runs end to end with the
   host's fs provided as plain JS `async` functions and a host-side `buffer` resource class — returns
   `round-tripped(35)`. Async-lifted exports also work: `entropy.seeded`'s `configure: async func(seed)`
   returns the exported `entropy-impl` handle and the PRNG sequence is deterministic and repeatable across
   instances. The generated async path uses JSPI (`WebAssembly.promising`/`Suspending`), so the browser
   needs JSPI: shipped in Chromium-based browsers, available in current node; Firefox/Safari support must be
   checked before calling the page generally available (fallback: feature-detect and say so on the page).
4. **One upstream bug found (blocks async-lifted exports that return exported resources).**
   js-component-bindgen 1.19.3's instantiation-mode output declares an exported resource's JS class *after*
   the task-return trampoline that references it, so instantiation throws
   `Cannot access 'EntropyImpl' before initialization` (TDZ). Hoisting the class declaration above the
   trampolines (one-line reorder, verified on a scratch copy) makes everything work, which shows the rest of
   the machinery is sound. Action: file/confirm the issue upstream and pick up the fixed release; we do not
   ship a post-processing edit of generated code.
5. **eosh itself transpiles cleanly; what remains is host surface, not feasibility.** The transpiled eosh
   asks for exactly `eo9:exec/{component-algebra, images, compile, task}`, `eo9:fs/{types,fs}`,
   `eo9:text/{types,text}`, `eo9:io/buffers`. The browser host therefore has to implement the component
   algebra over a graph of *pre-transpiled* components (compose/extend/restrict/rename/configure as wiring
   decisions, `compile` as graph resolution, `spawn` as instantiating the transpiled modules with the chosen
   imports, WAVE argument checking against signatures extracted at build time) and a read-only store view
   (`/bin/<name>.wasm`) backed by HTTP fetches. That is milestone 3's work; milestones can ship in the order
   above so the page is honest at every step (the v1 launcher is presented as a launcher, never as eosh).
6. **Transpiled sizes (uncompressed, 1.19.3 defaults):** hello ≈ 141 KB JS + 33 KB wasm, stubs ≈ 140–150 KB
   JS + 40 KB wasm each, eosh ≈ 317 KB JS + 188 KB wasm. Most of the JS is repeated per-component intrinsics,
   so it compresses and dedupes well; fine for a demo page, revisit before calling it "the" distribution
   channel.
7. **Build-machine note for reproducing the spike.** Cargo build scripts executed from `/tmp` are killed by
   the machine's execution policy; the spike crate builds normally under the repository tree. The spike
   lives outside the website sources and is not part of the site; `www/` itself is unchanged by this
   investigation. (Standing rule since adopted: keep every build inside the repository tree.)
8. **v1 shipped.** `/try/` is a static page: a hand-written terminal + launcher (`site/try/try.js`,
   ~500 lines), the browser host (`site/try/host.js`, ~180 lines: eo9:text → terminal, eo9:time → browser
   clock, eo9:fs → page-session memfs, eo9:io buffers), and the committed generated bundle
   (`site/try/components/`, four programs — hello, outcomes, cruncher, readwrite — ~750 KiB uncompressed,
   15 files). `www/try-build` (its own workspace; js-component-bindgen `=1.19.3`, wit-parser/wit-component
   0.250 for the manifest) generates the bundle; `cargo xtask build-web-demo` = build-guest + try-build.
   `cargo xtask ci` does not depend on any of it, and the eo9-www server needed no code changes (its
   content-type table already covered .js/.wasm/.json).
9. **No third-party JavaScript: the terminal is hand-rolled.** The approved xterm.js vendoring turned out to
   be unnecessary — the launcher is line-oriented, so a ~100-line scrollback-plus-input widget does the job,
   keeps the "no external assets" property of the site, and avoids carrying a vendored copy of someone
   else's minified bundle. xterm remains an option for v2 if eosh wants real line editing.
10. **v1 ships without stub providers (owner's option (c)).** Composing a stub at run time requires calling
    its `configure` export from JS, and async-lifted exports that return an owned exported resource trip the
    upstream TDZ bug (Decision 4); no fixed release exists (1.19.3 is the latest as of 2026-05-26). The
    suggested avoidance (a) — binding `configure` at build time with the native algebra and transpiling the
    fused result — is plausible but was deliberately not gambled on for v1: the algebra's bind-on-first-use
    binder leans on wasmtime's CM-async subtask-status encoding (plan/03 D12), and whether the transpiled JS
    runtime reproduces that behavior is exactly the kind of thing that needs its own verification pass. The
    capability story v1 demonstrates instead is the loader rule (grant/revoke + refusal before execution),
    which needs no stubs. Stub composition lands with v2.
11. **Upstream issue draft (for the owner to file against bytecodealliance/jco):** *Title:* "transpile
    (instantiation mode): exported-resource class is declared after the task-return trampoline that
    references it, causing a TDZ ReferenceError for async-lifted exports returning owned resources."
    *Body:* Transpiling a component whose async-lifted export returns an `own<R>` of an exported resource R
    (e.g. `configure: async func(seed: u64) -> result<r, string>` in an exported config interface), with
    `instantiation_mode: Async`, produces output where the `liftFns` array for the task-return trampoline
    captures `className: R` before `class R { … }` is declared later in the same instantiation body;
    instantiation then throws `ReferenceError: Cannot access 'R' before initialization`. Observed with
    js-component-bindgen 1.19.3 (Rust API); hoisting the class declaration above the trampolines makes the
    component work correctly, so the fix is an ordering change in the generated output. A minimal reproducer
    is any component exporting a resource plus an async function returning it.
12. **What was verified in a real browser** (local eo9-www serving the worktree, WebKit-based webview with
    JSPI): hello prints through eo9:text and returns `success(greeted)`; cruncher returns the same digest on
    repeated runs (pure compute, no imports); readwrite (async main, async fs imports via JSPI) returns
    `success(round-tripped(n))` and `files` shows what it wrote to the page memfs; outcomes' failure variant
    renders as `failure(requested-failure("…"))`; `revoke time` makes hello be refused before instantiation
    with the loader-rule message and `grant time` restores it; no console errors.
13. **Browser support statement.** Sync-ABI programs run in any modern browser. Async-main programs need
    JSPI (`WebAssembly.Suspending`); the page feature-detects it, marks affected programs in `list`, and
    explains when a run is attempted without it. Recent Chromium-based browsers ship JSPI; Safari/Firefox
    status should be re-checked when v2 (eosh, which has an async main) is attempted, since v2 cannot fall
    back the way v1 does.
14. **v2 sketch (the real eosh REPL), not started.** Transpile eosh + the stubs into the bundle; implement
    the `eo9:exec` surface in the browser host: `component-algebra` over a graph of pre-transpiled
    components (compose/extend/restrict/rename as wiring decisions, `configure` as recorded constants,
    `describe` from build-time metadata), `compile` as graph resolution, `task.spawn` as instantiating the
    transpiled modules with the chosen wiring, WAVE argument encoding/checking, and a read-only store view
    (`/bin/<name>.wasm`) backed by HTTP fetches of the component bytes. Prerequisites: the upstream fix from
    Decision 11 (stub `configure`), a JSPI story for non-Chromium browsers, and a planner call on how
    faithful the JS `compile`/fuel semantics must be before the page may call the thing it runs "eosh".
