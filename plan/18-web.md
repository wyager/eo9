# 18 — Web-based Eo9 (the real stack in the browser)

## Scope

A web-based implementation of Eo9 a visitor can try in the browser (owner request: important
for demos). Architecture (owner-preferred, spiked in plan/15 "wasm32 embed spike"): the real
usermode stack — wasmtime with the Pulley interpreter backend inside — compiled to one wasm32
blob, with thin JS shims for the true roots; **not** a JavaScript re-implementation of Eo9
semantics. The /try page (plan/15, jco-transpiled components on the browser's own engine)
stays as-is; this area builds the successor at `/vm/`.

Pieces: `www/web-eo9/` (the blob workspace), `www/site/vm/` (the page), `cargo xtask
build-web-vm` (pre-AOT + build + install), and the vendored wasmtime's opt-in
`component-model-async-fiberless` feature (kernel/vendor/README.md) that makes the async ABI
runnable on a host with no fiber backend.

## Decisions

1. **The fiberless path works (milestone 1's crux, retired).** wasm32 has no wasmtime-fiber
   backend, and wasmtime 45 routes every call into an async-lifted export through a worker
   fiber — previously the single blocker for real Eo9 guests in a wasm32 host (plan/15 D17).
   The vendored wasmtime now has an **opt-in** `component-model-async-fiberless` feature
   (off by default; kernel and host builds unchanged): `run_on_worker` executes the guest
   call directly on the current stack instead of creating a worker fiber. Callback-ABI
   ("stackless") guests — which is what every Eo9 guest is — return to the host with a
   status code instead of blocking mid-frame, so they don't need the fiber; code that
   genuinely must block mid-guest-frame already checks `can_block()` and fails cleanly.
   Verified two ways: `www/embed-spike` with `--features fiberless` (the previously-failing
   cm-async steps now pass: the unmodified `entropy.seeded` runs through both `call_async`
   and `run_concurrent`/`call_concurrent`, producing the exact SplitMix64 sequence the
   kernel/native embeddings produce), and the shipped `/vm` blob (below). What is *proven*
   is the immediate-completion case (the guest never genuinely suspends); guests that await
   host futures which complete later are expected to work (the suspension lives in host
   futures polled by the event loop, with the callback delivered as another fiberless guest
   call) but are not yet exercised — that is milestone 2's first verification item.
2. **The `/vm` page (work-in-progress label) runs the real stack.** `www/web-eo9/blob` is a
   ~1.0 MiB cdylib for `wasm32-unknown-unknown`: the vendored wasmtime (custom platform
   layer, exactly like the bare-metal kernel: embedder TLS symbols, no-op CustomCodeMemory,
   `signals_based_traps(false)`, Pulley target) plus embedded pulley32 pre-AOT artifacts of
   the kernel seed component and the unmodified `entropy.seeded` stub. Its only import is
   `env.host_write` (terminal output); exports are `boot`, `run_hello`, `run_fuel`,
   `run_entropy(seed_lo, seed_hi, count)`. The page (`www/site/vm/`) is hand-written
   HTML/CSS/JS (no third-party code, same policy as /try): buttons → blob exports → output
   pane. Honest labeling of what is and is not there.
3. **Build/install via `cargo xtask build-web-vm`** (mirrors `build-web-demo`): build-guest →
   pre-AOT seed/seed-fuel/entropy.seeded to pulley32 (same compile-relevant config as the
   blob's engine; helper `preaot_for_web`) → cargo build the blob in its own workspace →
   copy to `www/site/vm/web-eo9.wasm`. The installed blob (1,069,022 bytes) is committed so
   the site deploys without tooling, like /try's bundle; `ci` does not build or need any of
   it. The blob workspace patches in the whole vendored wasmtime family (same note as
   www/embed-spike) so the fiberless feature and the CM-async relaxation are available.
4. **Verification state.** Served assets verified over the real `eo9-www` server
   (`/vm/`, `/vm/vm.js` and `/vm/web-eo9.wasm` with correct content types; `/try/` still
   200). The exact blob bytes installed at `www/site/vm/web-eo9.wasm` execute correctly
   under the embed-spike driver's new `verify-blob` mode (boot/hello/fuel/entropy all 0,
   entropy sequence matches native). The page DOM/JS load correctly in the local automation
   browser, but that webview ships **without WebAssembly enabled**, so the final
   in-retail-browser run was not captured this session — first thing to do next session in a
   normal browser (expected to just work; the page degrades to a clear error message if
   WebAssembly is unavailable, which is exactly what the automation webview showed).
5. **Pre-existing breakage fixed in passing:** the embed-spike workspace patched only
   `wasmtime`, so its std (native-driver) build broke against the registry
   `wasmtime-environ` once the on-target-codegen fork changed the vendored debug surface
   (`clif_dir` is `&str` there); the spike now patches the whole vendored family, compiles
   single-threaded (the vendored compile glue's lock is the kernel's single-core spinlock),
   and names its host triple explicitly (vendored builds gate `cranelift-native` off). One
   stray `&Path` call site in the vendored `wasmtime/src/runtime/module.rs` (std-only,
   `from_trusted_file`) was aligned with the string-path surface.
6. **Milestone 2 plan (the real shell in the browser):**
   - Verify the fiberless path for guests that genuinely await (a host import whose future
     completes later — e.g. a time.sleep wired to a JS timer): expected to work because the
     waiting happens in host futures, not guest frames; if a guest-side `waitable-set.wait`
     path turns out to demand a true block, that specific case needs JSPI.
   - Browser root providers: text (page terminal read-line via JSPI `WebAssembly.Suspending`
     or a queued-input poll), time (`performance.now`), entropy (`crypto.getRandomValues`),
     fs (memfs inside the blob) — as host functions on the blob's linker, mirroring the
     kernel's `add_providers`.
   - Run real Eo9 *programs* (hello/cruncher/outcomes with WAVE args/outcomes), then eosh
     itself with an exec/store surface over HTTP-fetched, pre-AOT'd pulley images —
     `eo9-embed`'s `ProviderSource` seam is the natural shape for this.
   - Unknowns to retire next: JSPI-vs-poll for blocking read-line; blob size growth once
     eo9-runtime/eo9-component are linked in (the spike's 1 MiB is wasmtime alone); whether
     `instantiateStreaming` needs the server to skip gzip for ranges (it doesn't — size is
     fine).
