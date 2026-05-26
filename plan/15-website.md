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
   (done — see Decisions 2–5).
2. `/try` v1: sync-ABI examples (`hello`, `outcomes`, `cruncher`) + invoker-configured stubs
   (`entropy.seeded`, `time.frozen`) running on the page via a minimal launcher host; honest framing; no
   eosh claim yet.
3. `/try` v2: the real eosh REPL — requires the JS exec host (component-algebra over the transpiled-module
   graph, spawn/wait/kill, WAVE argument checking) and the store-backed name resolution eosh expects.
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
   investigation.
