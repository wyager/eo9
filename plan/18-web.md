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
7. **Milestone 2 (browser root providers, the HTTP program store, JSPI suspension) is in.**
   The blob now registers browser root providers mirroring the kernel's
   (`blob/src/providers.rs`): `eo9:text/text` → the page terminal (write sync; `read-line`
   suspends the whole VM on the input box via JSPI), `eo9:time/time` → `Date.now` /
   `performance.now` (with `sleep` parked on a real `setTimeout`), `eo9:entropy/entropy` →
   `crypto.getRandomValues`. `cargo xtask build-web-vm` additionally pre-AOTs the real
   example programs (hello, cruncher, outcomes) and the kernel's sleepy canary to pulley32
   under `www/site/vm/store/`, and the blob fetches them on demand over HTTP (a
   JSPI-suspending `fetch`), deserializes, links the providers, and runs `main` with typed
   arguments and a rendered outcome (`blob/src/store.rs`). New JS import surface and the
   page wiring (program picker, "park the VM", read-line input row) are in `vm.js`; the
   import calls that genuinely block are `WebAssembly.Suspending` functions and the exports
   that may suspend are wrapped with `WebAssembly.promising`. Without JSPI the page degrades
   honestly (those demos disabled with an explanation; hello/fuel/entropy still work).
8. **Retail-browser verification is automated.** `www/site/vm/selftest.html` loads the same
   blob with the same import wiring and runs every non-interactive demo, writing per-check
   results and a PASS/FAIL verdict; verified green (19/19) in headless Google Chrome 148
   (`--headless=new --virtual-time-budget=60000 --dump-dom http://127.0.0.1:<port>/vm/selftest.html`,
   JSPI available): boot/hello/fuel/entropy with the exact SplitMix64 sequence, the three
   store programs end to end (`Hello, selftest!` + `success(greeted)`, the exact cruncher
   digest, the outcomes typed failure with stderr routed), and "park the VM" proving JSPI
   suspension/resumption (the page-side clock advances ≥ the requested timer). The
   embed-spike `verify-blob` mode grew the new import surface (plain stand-ins) so the
   committed blob bytes still verify natively. The interactive read-line round trip needs a
   human (it waits on the page's input box); it shares the exact code path the selftest's
   suspending imports exercise.
9. **One honest limitation found: the stackful async lift.** The kernel's `sleepy` canary
   lifts its export `async` *without* a callback (stackful), i.e. it blocks mid-guest-frame
   on a sync-lowered `sleep`; that shape needs a real fiber backend, which wasm32 does not
   have, so the fiberless path refuses it ("store configuration requires `*_async`…"). The
   page keeps the button and reports the limitation honestly instead of hiding it. Eo9's own
   SDK guests use the callback ABI and run here; a callback-ABI guest that awaits
   `time.sleep`/`read-line` will suspend on the real browser timer/input through the same
   provider path "park the VM" exercises. Remaining for milestone 3 (the in-browser shell):
   fs + io-buffer providers in the blob, the exec/store surface for eosh (algebra + compile
   are further out — composition needs the compile path), and a callback-ABI sleep/read
   demo guest once one exists in the tree.

10. **Milestone-3 start: committed-asset drift fixed + a drift guard (the rest of m3 is the
    next pass).** The /vm `web-eo9.wasm` blob and the `store/*.cwasm` programs are committed,
    and review of the web-hardening branch found them stale: the `hello`/`cruncher`/`outcomes`
    store artifacts (and the blob) predated the `fs-impl-in-interface` and `configure-defaults`
    merges that regenerated the guest components. Fixed by re-running `cargo xtask build-web-vm`
    and committing the regenerated assets (+ brotli/gzip siblings); `sleepy.cwasm` (from WAT) was
    unaffected.
    - **Determinism finding:** the web-VM build is **byte-deterministic** on the pinned toolchain
      — a second `build-web-vm` reproduced every asset identically (the m1 "1-byte drift" did not
      recur). So a byte-exact drift guard is sound (no codegen-noise false positives).
    - **Guard:** `cargo xtask check-web-vm` rebuilds the blob + store artifacts to a temp staging
      dir and byte-compares against the committed `www/site/vm/` files without overwriting them;
      non-zero exit on drift, naming the stale files. Not wired into `ci` (it needs the wasm32
      target + a full guest build); run it after touching guest sources. Verified both ways
      (clean → "up to date"; a mutated committed artifact → "Drifted: store/hello.cwasm").
    - **Verification of the regenerated assets:** a node v25 (JSPI) harness mirroring `vm.js`'s
      import glue loaded the committed blob and ran `boot`/`run_hello` (`add(17,25)->42`)/
      `run_fuel`/`run_entropy` (exact `0x505f147c387507b6`) and `run_program` for the three
      store programs via the JSPI fetch path: `hello` → greeting + success, `cruncher` →
      `digest(14341732361190694547)`, `outcomes` → `failure(requested-failure(...))`. All pass.

11. **Milestone 3 remaining (fs/io providers → eosh in the browser) — design, not yet built.**
    Deferred to the next pass because each piece needs real implementation + verification and
    must not ship as an unverified blob:
    - **`eo9:fs` (memfs) + `eo9:io` (owned buffers) host providers in the blob**, hand-mirrored
      against the raw component `Linker` the way `blob/src/providers.rs` mirrors text/time/entropy
      (eo9-runtime/eo9-providers-unix don't compile for wasm32). Shapes to copy: the kernel's
      `wasm/shellfs.rs` (memfs semantics, file/immutable-handle resources, async
      open/read/write/list-directory/stat/create-directory/remove) and the io buffer resource
      (alloc/from-bytes/read-into/transfer). Proof: `readwrite` runs on /vm (it currently can't —
      no fs/io providers). The fs-impl-in-interface convention is already on master, so the
      root-handle lives on `eo9:fs/fs` here too.
    - **Exec/store surface for eosh**: give the blob the `eo9:exec` capability (component-algebra
      load/describe + spawn/wait) and a `/bin` view backed by the HTTP store, mirroring
      `crates/eo9/src/shell.rs`. Open question to decide then: `compile` for a *fused* composition
      needs codegen, which the blob does not have (Pulley artifacts are pre-AOT'd) — so `$`/`&`
      should give a clean "composition needs the compiler, not available in the browser yet"
      refusal, while plain program runs and `only`/attenuation (no new codegen) should work.
      Decide whether attenuated-but-not-recompiled compositions are feasible via instantiate-time
      linking.
    - **Boot eosh on /vm**: a real `eosh>` prompt using the m2 JSPI `read-line` path; eosh is
      built unmodified from guest sources and must run through the fiberless host. Needs a
      callback-ABI read-line in eosh's await path (confirm eosh's read-line lift is callback-ABI,
      not stackful — the stackful lift is unsupported on this host per Decision 9).

12. **Milestone-3, layer 1 done: the in-blob `eo9:fs` + `eo9:io` providers (readwrite runs).**
    `www/web-eo9/blob/src/fs.rs` adds a **writable in-memory filesystem** (`MemFs`) and the
    owned-buffer `eo9:io/buffers` table, mirroring the kernel's `wasm/shellfs.rs` and
    `eo9-runtime::link` shapes (same WIT-shaped host types, same per-buffer/total byte caps)
    but with writable semantics (files as byte vectors, directories as a path set;
    open/read/write/stat/list-directory/create-directory/remove; `open-exec` pins a snapshot).
    `WebState` now carries the memfs + buffer table; `store::instantiate` registers them
    alongside the text/time/entropy roots. Verified: the unmodified `readwrite` example
    (`--path /scratch/note.txt --contents "hello disk"`) round-trips end to end on `/vm` →
    `success(round-tripped(10))` (node v25 / JSPI harness `www/web-eo9/verify-fs.mjs`, and a
    new check in `selftest.js`); the existing programs (hello/cruncher/outcomes/entropy) still
    pass (8/8 in the harness); `readwrite` is now a selectable program on the page. Blob grew
    1.24 → 1.42 MiB.
    - **Bug fixed in passing (the load-bearing finding):** programs whose imports *cross-`use`*
      a resource (here `eo9:fs/fs` uses `eo9:io/buffers.buffer`) fail the **synchronous**
      `linker.instantiate` under the component-model-async ABI with "resource implementation is
      missing"; they link correctly only through **`instantiate_async`** driven by the polling
      executor — exactly what the kernel runner (`runner.rs`) and usermode `spawn` (`task.rs`)
      use. `store::instantiate` now instantiates via `block_on(linker.instantiate_async(…))`.
      hello/cruncher/outcomes worked before only because they have no cross-interface `use`.
    - **Finding that de-risks layer 4 (eosh):** `readwrite` is an SDK guest with an `async`
      `main` that **awaits** async fs ops, and it runs fiberlessly here — so the SDK's
      async-main + async-await path is callback-ABI and works on this host (not the stackful
      lift of Decision 9). eosh uses the same `eo9_guest::main!`/`text::read_line().await`
      shape, so booting eosh is **not** blocked by the fiber limitation; what remains for it is
      the exec/store surface, not an ABI wall.

13. **Milestone-3, layers 2–4 still to build (the exec surface + eosh) — deliberately not in
    this pass.** Booting eosh needs the blob to host the whole `eo9:exec` surface eosh imports
    (component-algebra load/describe, task spawn/wait, the `/bin` store view, and a `compile`
    that is artifact-lookup with a clean "composition needs the compiler, not in the browser"
    refusal for `$`/`&`). In the kernel that surface is ~1,600 lines (`wasm/shellexec.rs`);
    hand-mirroring it against the raw `Linker` for wasm32 — and *verifying* it (AOT eosh into
    the store, JS wiring, a browser/node smoke of an `eosh>` prompt) — is a milestone-sized
    build that cannot be done and verified in one pass without shipping a large unverified
    blob, which the project's norms forbid. Layer 1 is the clean, verified, committed prefix;
    layer 4's ABI risk is now retired (above), so the remaining work is plumbing the exec
    surface. Next pass: build `eo9:exec` + `/bin` store view in the blob, AOT eosh, then boot
    the prompt.

14. **Layer-2 feasibility CONFIRMED: the algebra closure compiles for `wasm32-unknown-unknown`
    in the blob's dependency graph.** The gating unknown for hosting the `eo9:exec` surface in
    the blob was whether `eo9-component` (the algebra) and its wasm-tools closure build for the
    blob's target alongside the vendored wasmtime family. Probed directly and retired (no code
    committed — the probe deps were reverted): with `eo9-component = { path =
    "../../../crates/eo9-component", default-features = false }` added to `blob/Cargo.toml` and
    the same five algebra `[patch.crates-io]` entries the kernel uses added to the blob
    workspace (`wit-parser`, `wac-types`, `wac-graph`, `wit-component`, `wasm-wave` →
    `kernel/vendor/*`), `cargo build -p web-eo9-blob --target wasm32-unknown-unknown` compiled
    `wac-types`, `wit-parser`, `wac-graph`, `wasm-wave`, `wit-component`, and `eo9-component`
    cleanly (only unused-import warnings; no version conflict with the vendored wasmtime family
    in the unified graph). The only errors were the blob lib's own `include_bytes!` of the
    pre-AOT `artifacts/*.cwasm` (produced by `build-web-vm`, not run in the probe) — unrelated.
    So the no_std algebra runs on wasm32 exactly as it does on `aarch64-unknown-none`.
    - **Exact next-pass recipe (apply deliberately, not as a dead dep):** (a) add the dep line +
      the five patches above; (b) implement the `eo9:exec` host surface in a new
      `blob/src/exec.rs`, mirroring the SUBSET of `kernel/wasm/shellexec.rs` eosh actually
      imports — component-algebra `load`(bytes→component via `eo9_component::load`) / `describe`
      / `compose`/`restrict`/`rename`/`configure`, and `task` spawn/wait — but DROP everything
      the browser doesn't need (no child fuel/preemption, no drive-loop scheduler, no on-target
      codegen integration); the surface is far smaller than the kernel's 1,588 lines because the
      browser has no codegen and runs one child at a time. (c) `compile` = artifact-lookup: a
      fused/`$`/`&` composition has no pre-AOT'd Pulley image, so it returns a clean typed
      "composition needs the compiler, which isn't available in the browser yet" refusal; a bare
      `/bin/<name>` resolves to its committed `store/<name>.<hash>.cwasm`. (d) the bytes-vs-Pulley
      mismatch: eosh's `resolve` reads `/bin/<name>.wasm` (raw component bytes) to `load`+`describe`
      via the algebra, but the store ships pre-AOT'd Pulley `.cwasm`; ship BOTH per program (the
      raw `.wasm` for the algebra, the `.cwasm` for spawn) or have `spawn` of a store-resolved
      component fetch the matching `.cwasm` by content hash — decide in the next pass and record.
      (e) AOT eosh + the coreutils into the fingerprinted store (extend `build-web-vm`); (f) boot
      eosh against text/time/entropy + the layer-1 fs/io + this exec surface; verify an `eosh>`
      smoke (`hello`, `ls`, `cat`, a refused `$`) via the node/JSPI harness pattern, extend
      `selftest.js`, keep `check-web-vm` green.

15. **Milestone-3, the component algebra runs in the browser (committed, verified).** The
    real `eo9-component` crate (`default-features = false` = its no_std build) is now a
    dependency of the blob, with the same five algebra `[patch.crates-io]` entries the kernel
    uses added to `www/web-eo9/Cargo.toml` — confirming D14's probe in a *committed* build:
    the algebra closure compiles for `wasm32-unknown-unknown` alongside the vendored wasmtime
    family. A new `blob/src/exec.rs` + the `algebra_demo()` export exercise it end to end:
    `eo9_component::load` a raw component → `describe` (kind/imports/exports) → `restrict`
    (`only eo9:text/text, eo9:time/time`) → then execute the *same* component via Pulley
    against the browser root providers. `cargo xtask build-web-vm` now also emits the hello
    example as raw `blob/artifacts/example-hello.wasm` (for the algebra) and pre-AOT'd
    `example-hello.cwasm` (for execution). Verified: `node www/web-eo9/verify-exec.mjs` →
    describe = binary, imports text+time, `only` seals a 35 KB component, execution →
    `success(greeted)`; the fs regression harness still passes; `selftest.js` gains the same
    four checks; the page gains a "Run the component algebra" button. Blob grew 1.42 → 4.05
    MiB (829 KiB brotli) — the algebra closure (wit-component/wac-graph/wit-parser/wasm-wave +
    eo9-component) adds ~2.7 MiB; a size-trim pass (or shipping the algebra only on the eosh
    page) is worth a follow-up.
    - **What this is and is not:** this proves the algebra + execution run in the blob and is
      the foundation for eosh. It does NOT yet register the guest-facing `eo9:exec` Linker
      surface (component-algebra/compile/task as host imports an *eosh component* links
      against) — that, plus the `/bin` store view feeding eosh's `resolve` and AOT'ing eosh
      into the store, is the remaining work to a real `eosh>` prompt. The recipe in D14 stands;
      D14(d)'s bytes-vs-Pulley duality is now concretely set up (the build emits both the raw
      `.wasm` and the `.cwasm` for the demo program, the pattern eosh's store needs).

16. **Milestone-3, coreutils run in the browser (committed, verified).** The twelve coreutils
    (`cat ls echo rng wc head cp mkdir rm touch stat find`) are now AOT'd to pulley32 into the
    fingerprinted `/vm` store and runnable on the page against the blob's in-memory `eo9:fs`
    (`fs::MemFs::seeded` pre-populates a small sample tree — `/welcome.txt`, `/docs/*` — so the
    fs-backed tools have content; the fs is writable, so programs may change it). Each imports
    only what it needs (echo → text; rng → entropy + text; the rest → fs + text + io) and runs
    through the existing `store::run_program` path (no exec surface needed — these are leaf
    programs, not the shell). Verified: `node www/web-eo9/verify-coreutils.mjs` → **12/12 PASS**
    (`echo` → `success(done)`, `rng 5` → `success(generated(5))`, `cat /welcome.txt` →
    `success(printed(150))`, `ls /` → `success(listed(2))`, `wc`/`stat`/`head`/`cp`/`mkdir`/
    `touch`/`rm`/`find` all succeed against the seeded fs); `verify-fs`/`verify-exec` still pass;
    `check-web-vm` ok (18 fingerprinted assets); featureless `cargo xtask ci` green; the page
    gains a coreutils optgroup + arg placeholders, and `selftest.js` gains echo/rng/cat/ls/find
    checks. Blob unchanged at 4.13 MiB (the coreutils are separate store artifacts, fetched on
    demand). This expands "what runs in the browser" to 16 real Eo9 programs and stages the
    coreutils for the eosh prompt.

17. **The eosh prompt (`eosh>` on /vm) — still the remaining work; the blocker is the exec
    surface's spawn/wait, not the ABI.** Booting eosh needs the blob to host the guest-facing
    `eo9:exec` *Linker* surface eosh imports — `component-algebra` (load/describe/compose/
    restrict/rename/configure), `compile` (artifact-lookup with a clean "$/& needs the compiler"
    refusal), and `task` (spawn/wait) with the `component`/`image`/`task` resource tables — which
    is only exercisable by a guest (eosh), so it cannot be verified without also AOT'ing eosh and
    booting the prompt. The genuine integration risk is **`task.spawn`/`task.wait`**: in the
    kernel (`wasm/shellexec.rs`) children run on the kernel's drive loop via a `CHILDREN`
    registry and `wait` polls it; the blob has no such drive loop, so a child must be run from
    inside eosh's `task.wait` host call — i.e. nested concurrent execution of a child store while
    the parent guest (eosh) is suspended in a `func_wrap_concurrent` host call. Getting that
    wrong deadlocks the executor ("cannot block a synchronous task" / re-entrancy). This is a
    design-and-iterate piece, not a one-pass plumbing job — it is the reason this and the two
    prior passes stopped short of the prompt. The foundations are all in place and proven:
    the algebra runs in the blob (D15), the fs/io providers run (D12), the SDK async-main/await
    runs fiberlessly (D12), and the leaf programs (examples + coreutils) run via the store path
    (D16). The next pass builds `eo9:exec` against the raw `Linker` (mirroring the eosh-imported
    subset of shellexec.rs, dropping fuel/preemption/codegen) with a run-to-completion spawn/wait
    designed for the single-child, no-drive-loop browser executor, AOTs eosh + makes resolve read
    `/bin/<name>.wasm` (ship the raw component bytes alongside the `.cwasm`, content-addressed per
    D14(d)), then boots and verifies an `eosh>` smoke. A server-side `/vm/compile` endpoint (the
    standalone server has the full toolchain) is the path to making `$`/`&` composition actually
    compile-and-run in the browser, since in-blob codegen is std/mmap-blocked.

## Decision 17 — eosh boots in the browser (the in-blob eo9:exec surface)

`blob/src/execsurface.rs` registers the `eo9:exec` surface eosh imports (component-algebra,
images, compile, task) on the raw component `Linker`, hand-rolled like `providers.rs`/`fs.rs`
(not `bindgen!` — the SDK path is finicky with the custom-platform + fiberless config). Design,
as built and verified:

- **component-algebra** is backed by the real `eo9-component` crate (load/save/describe/
  compose/extend/restrict/rename/configure); `component` is a host resource over a table.
- **compile** = artifact lookup: a loaded plain program's raw bytes are content-hashed to its
  pre-AOT'd `.cwasm` (embedded). A binary is "closed" if its required imports are all served by
  the browser root environment (text/time/entropy/fs/io) — exactly as a bare `hello` runs on the
  kernel. `compose`/`extend`/`configure` results have no artifact → `compile` returns a clean
  "composition needs the compiler, not available in the browser yet" (in-blob codegen is
  std/mmap-blocked; the server-side `/vm/compile` endpoint is the path to running `$`/`&`).
- **task** is single-child run-to-completion: `spawn` runs the child to completion immediately
  via the existing run_program path (a fresh Store, instantiate_async, run `main` under
  `run_concurrent`/`block_on`), stores the `program-outcome`; `wait`/`kill`/`resume` return it;
  `runnable` is immediate. **Nested `run_concurrent` works** (eosh's `main` → `task.spawn` →
  child `run_concurrent`) — the integration risk that stalled the prior passes is retired.
- Args are bound with a WAVE-lite scalar parser keyed on the arg-spec type text (string/bool/
  integers/char/option) — enough for the demo programs without pulling wasmtime's `wave` feature.
- `/bin/<name>.wasm` is seeded into the blob's MemFs with raw component bytes so eosh's `resolve`
  (fs `open-exec` + `load`) finds programs; today seeded with `hello` (more follow).

**Verified** (`www/web-eo9/verify-eosh.mjs`, node v25/JSPI, and the committed fingerprinted blob):
`eosh_instantiate` → eosh links against the in-blob exec/text/fs surface; `eosh_command("hello
--name web --excited true")` → the program prints `Hello, web!`, eosh renders `ok: greeted`, and
eosh's session outcome is `success(exited)`. featureless `cargo xtask ci` green; `check-web-vm`
ok. Blob 5.20 MiB raw / 1.07 MiB brotli.

**Remaining**: seed `/bin` with more programs (coreutils — embed/fetch their raw+`.cwasm`); wire
the interactive `eosh>` prompt on the page (main(none) reading the terminal via JSPI read-line);
`only`-attenuation via a linker restricted to the admitted interfaces (today the base artifact
runs with full root providers); the server-side `/vm/compile` endpoint for `$`/`&`.

## Decision 18 — `only`-attenuation enforced by a restricted linker on the run path

`spawn` runs the *base* artifact (compile = artifact lookup; an `only`/`rename` of a binary
keeps the base artifact, recording the admitted allow-list on the component), so a capability
the `only` gate sealed must be withheld at run time by the linker, not the bytes. `run_child`
now threads the recorded `allow` into the linker: `providers::add_providers_for(linker, allow)`
adds only the admitted root families (each family registers its own authority-free `types`
alongside its authority interface, so a program never needs a family's `types` unless it imports
that family), and fs/io is added only when `eo9:fs/fs` is admitted. `allow == None` is
unrestricted. An entry admits an interface by exact match or as the bare package (`only eo9:text`
matches `eo9:text/text`), version-insensitive — the same package-shorthand the usermode `only`
accepts.

Verified (`verify-eosh.mjs`): `only eo9:text/text,eo9:time/time $ hello` runs against a
text+time-only linker; `only eo9:text/text $ echo` runs text-only; `only eo9:text/text $ hello`
is refused (`restrict` rejects the required-but-unadmitted `eo9:time/time` before spawn). Note:
with the current demo programs (all imports required, none optional) the restricted linker is
defense-in-depth — a program can't use a capability it doesn't import, and required-outside-allow
is refused at `restrict`; the restriction becomes load-bearing for a program with an *optional*
import that a successful `only` seals as absent (none in the demo set yet).

## Decision 19 — a provider in `/bin` so `$`/`&` is exercisable through eosh

`entropy.seeded` (the unmodified `eo9-stub-entropy-seeded`) is now seeded into the blob's
`/bin` as raw component bytes (for the algebra's `load`) plus a pulley `.cwasm` (the `bin!`
macro embeds both), via the same xtask `/bin` build loop as the example/coreutil binaries.
A visitor can now type `entropy.seeded $ rng --count 3` at the prompt: eosh resolves both
from `/bin`, composes with the real algebra, and `compile` of the fused result returns the
clean "composition needs the compiler" refusal (no precompiled artifact) — reached through
eosh, not a crash (verify-eosh.mjs). The server-side `/vm/compile` endpoint (Decision 20,
pending) is the path to actually compiling+running such a composition in the browser. Cost:
the blob grows by entropy.seeded's raw+cwasm (~6.05 MiB raw / ~1.19 MiB brotli) — a lazy-fetch
trim (serve `/bin` raw+cwasm from the HTTP store instead of embedding) is the recorded
blob-size follow-up. Nit: eosh renders the refusal as `CompileError::Codegen(...)` (raw enum
prefix) — an eosh-side rendering follow-up.

## Decision 20 — server-side `/vm/compile` for in-browser `$`/`&` (DESIGN — implemented in Decision 21)

In-blob codegen is std/mmap-blocked, so a fused composition can't be compiled client-side
(Decision 19's clean refusal). The path to actually running `entropy.seeded $ rng` in the
browser is a bounded compile endpoint on the standalone `www` server (which has the full host
toolchain). Design for the next focused pass (NOT done here — it's a server + blob + page
feature that can't be implemented *and* verified within one fork's budget without shipping
unverified code; #1 (only-narrowing) and #2 (provider in /bin) are done and verified instead):

- **Server (`www/src`)**: a POST `/vm/compile` route. Body is a composition expressed over
  **store program names + algebra ops** (e.g. `entropy.seeded $ rng`), NOT uploaded bytes.
  The server: parses the expression with a small host-side parser (a minimal `name [--flag v]
  { ($|&) name … }` / `only … $` reader — the eosh grammar is no_std guest code, so a tiny
  host reader is cleaner than reusing it), resolves each name against a fixed allow-set of the
  `/vm` store programs (reject anything not in the set), fuses with `eo9-component` (compose/
  extend/restrict/configure), and compiles to a pulley32 image with the same web-compatible
  engine config `xtask::preaot_for_web` uses (new dep: eo9-component + eo9-runtime, host-side).
  Returns the `.cwasm` bytes. **Security (required)**: names restricted to the known store set;
  request-size cap; a compile timeout; a small concurrency limit (semaphore) — so it can't be a
  compile-bomb/DoS. Keep the existing security headers; add `connect-src 'self'` to the CSP for
  the POST and verify the page still loads.
- **Blob (`execsurface.rs` compile)**: on a fused (artifact-None) component, instead of the
  flat refusal, POST the composition to `/vm/compile` via a JSPI `Suspending` host import
  (the blob already fetches the store over HTTP), receive the pulley image, and run it through
  the existing run-to-completion `spawn` path — honestly labelled "compiled on the server".
- **Verify**: `entropy.seeded $ rng --count 3` at the browser prompt → server compiles → eosh
  runs it → typed outcome, deterministic across runs (extend verify-eosh.mjs with a stub/real
  `/vm/compile` responder, and a www server integration test for the route + its bounds).

## Decision 21 — `/vm/compile` implemented: the in-browser composition round-trip works

Decision 20's design, built and verified end-to-end. Typing `entropy.seeded $ rng --count 3`
at the browser `eosh>` prompt now genuinely compiles the fused composition on the server and
runs it in the blob — verified by `verify-eosh.mjs` (which spawns the real `eo9-www` server and
points the blob's compile host-import at its `/vm/compile`): 3 SplitMix64 numbers print,
deterministic across two runs, and the composition no longer hits the codegen refusal.

What was built (all under `www/`, `xtask`; eosh untouched):

- **Server compile core (`www/src/compile.rs`)**: `compile_expression(expr, raw_dir, allow)`
  parses `[only <csv> $] name ($ name)*` (consumer `--flags` stripped — bound at spawn, not
  part of the fused component), resolves each name against the allow-set, fuses with the real
  `eo9-component` algebra (right-assoc `$`, leading `only` → `restrict`), and precompiles to a
  pulley32 image with the exact engine config `xtask::preaot_for_web` uses (shared helper
  `pulley_engine`). `&`/`rename`/`configure` are rejected with a clear message (the kernel/
  native run the full algebra). New host deps: `eo9-component` (path) + `wasmtime` 45. Unit-
  tested: fuses `entropy.seeded $ rng` to a real artifact; allow-set rejection enforced.
- **HTTP route (`www/src/server.rs`)**: `POST /vm/compile`, dispatched in the site connection's
  service before the static-file path. **Security bounds**: 2 KiB request-body cap
  (`Limited`), a 20 s compile timeout (`spawn_blocking` + `timeout`), and a concurrency gate
  (2 permits, wait up to 10 s then 503). The allow-set is exactly the stems of
  `site/vm/raw/*.wasm` shipped with the site — never the client's word. Typed responses:
  `application/octet-stream` image on success, `text/plain` 4xx/5xx with the reason. Carries
  the standard security headers. The CSP already had `connect-src 'self'`, so no CSP change was
  needed (the design's one open question resolved itself). Integration-tested over real HTTP
  (`www/tests/vm_compile.rs`): compiles `entropy.seeded $ rng`, rejects unknown programs (400),
  unsupported ops (400), empty (400), and oversized bodies (413).
- **Raw components (`xtask build-web-vm`)**: emits each `/bin` program's raw bytes to
  `site/vm/raw/<name>.wasm` (the same set, by name). Not fingerprinted — the server resolves
  them by fixed name. `check-web-vm` still passes (it checks only the fingerprinted assets);
  `build-web-vm` reproduces them deterministically (straight copy of the guest components).
- **Blob (`host.rs`, `execsurface.rs`)**: a `host_compile_len`/`host_compile_copy` JSPI import
  pair mirroring the existing `host_fetch_*` (async POST stashes the image + returns its length;
  sync copy into blob memory). The key wrinkle the design under-specified: **eosh `load`s raw
  bytes, never names**, and `ComponentEntry` tracked no provenance — so the compile op had the
  fused bytes but not the names+ops expression the endpoint requires. Resolved by recovering
  names **by content hash** against the embedded `/bin` set (`name_for`, reusing the existing
  `artifact_for` hash) and threading a small `Prov` enum (`Program`/`Compose`/`Only`) through
  `load`/`compose`/`restrict`; `extend`/`rename`/`configure` set provenance `None` (not in the
  endpoint grammar → the existing clean refusal, unchanged). On a fused (artifact-`None`)
  component, `compile` renders the provenance to the endpoint expression, POSTs via the host
  import, and runs the returned image through the unchanged run-to-completion `spawn` path.
- **vm.js**: the page shim — `hostCompileLen` POSTs to `/vm/compile`, `hostCompileCopy` copies
  the image; both registered (`Suspending` under JSPI, an `unavailable` stub otherwise). The
  other blob harnesses (`verify-{coreutils,fs,exec}.mjs`) get `-1` stubs so the blob still
  instantiates (the two imports are now mandatory).

Security recap (all required bounds present): names-and-ops only (no uploaded bytes); allow-set
= shipped store programs (anything else 4xx); request-size cap; compile timeout; concurrency
limit. Blob size after the round-trip wiring: ~6.05 MiB raw / ~1.19 MiB brotli (the lazy-fetch
`/bin` trim from Decision 19 remains the recorded blob-size follow-up). The Decision 19 nit
(eosh renders an actual refusal as `CompileError::Codegen(...)`) is now rarely reached for `$`
compositions (they compile) but still applies to `&`/`rename`/`configure` — unchanged.

## Decision 22 — in-blob codegen: the browser VM is fully self-hosted (compose → compile → run, client-side)

Owner-approved goal: stop outsourcing composition codegen to the server and reuse the kernel's
no_std compile fork *inside the wasm32 blob*, with cranelift's **Pulley backends** as the
emission target — the same compose → compile → run story as native Eo9 and the bare-metal
kernel, just interpreted.

**What changed**
- `web-eo9-blob` gains an `inblob-codegen` cargo feature (default **on**) = `wasmtime/cranelift`
  on the already-vendored wasmtime. No vendored crate needed any change: the kernel's no_std
  port of wasmtime-environ/wasmtime-cranelift compiles for `wasm32-unknown-unknown` as-is, and
  cranelift-codegen's `host-arch` feature is a silent no-op on wasm32 (no native backend exists),
  leaving exactly the Pulley32/Pulley64 backends the blob needs. The blob's existing executing
  engine config (`target("pulley32")`, OS-less tunables, no-op code memory) doubles as the
  compiling engine — `Component::new` on it drives Cranelift → Pulley bytecode, and Pulley
  bytecode needs no executable pages, so the publisher stays a no-op.
- `exec.rs::compile_demo()` (a new `compile_demo` export + page/selftest checks): compiles the
  raw hello component and an algebra-fused `entropy.seeded $ rng` in-blob and runs both —
  the measured demo of the self-hosted pipeline.
- `execsurface.rs`: the `eo9:exec` `compile` op now compiles a fused composition's
  `executable_bytes()` **in-blob** (`compile_in_blob` → `Component::new` → `serialize()` → the
  same deserialize-and-run image path as a pre-AOT'd program). The server-POST path and its
  provenance machinery (`Prov`, `name_for`, `host_compile_len/copy`, vm.js glue) are removed —
  eosh `load`s raw bytes and the blob compiles whatever fused bytes the algebra produced, so
  `&`/`rename`/`configure` results compile too (no longer limited to the endpoint's
  `[only $] name ($ name)*` grammar). With the feature off (the size-measurement build) a fused
  composition gets the honest "compiler not built into this blob" refusal.
- `verify-eosh.mjs` is now fully offline (no server spawn, no compile import): a passing run is
  direct proof that `entropy.seeded $ rng --count 3` typed at the eosh prompt is fused and
  compiled with **zero server involvement**. verify-exec gained the compile_demo checks;
  selftest.html exercises compile_demo in the browser.

**Measured (node v25 JSPI harness, this machine)**
- In-blob compile latency: hello (35,265-byte component) ≈ **112 ms**; the fused
  `entropy.seeded $ rng` (57,274 bytes) ≈ **58 ms**; outputs are byte-identical to the
  server-compiled and native results (the seeded stream matches value-for-value).
- Blob size: 6.05 MiB raw / 1.19 MiB brotli (runtime-only, prior) → **9.50 MiB raw / 1.73 MiB
  brotli** with the compiler in (+3.4 MiB raw / +0.54 MiB on the wire). Per the size call in the
  directive (≤ ~3 MiB brotli), this ships as a single blob; a lazy-loaded compiler module and the
  Decision 19 lazy-fetch `/bin` trim remain available follow-ups if the wire size ever matters.

**The server `/vm/compile` endpoint** (Decisions 20–21) stays in place — it is tested, bounded,
and useful as a reference/fallback — but nothing in the blob or the page calls it any more.
Removing it (and its `site/vm/raw/` inputs) is a planner call for a later pass.

## Decision 23 — the page terminals must survive a hostile browser and behave like terminals

Owner report from a real-Chrome walkthrough: the in-page console only took focus when the exact
prompt line was clicked, and Enter appeared to do nothing. Investigation against the committed
site reproduced a way to get exactly that dead-terminal state: both `try.js` and `vm.js` (and
`selftest.js`) computed `hasJSPI` at module top level via `typeof WebAssembly.Suspending`, which
**throws `ReferenceError` when the `WebAssembly` global itself is absent** (locked-down or
policy-managed browsers, or a degraded renderer) — killing the whole module before any click or
keydown handler is registered, leaving an empty, unresponsive console whose only live element is
the bare `<input>` row.

**What changed**
- All three scripts guard the global first (`typeof WebAssembly === "object" && …`); a missing
  engine now degrades to an explained limitation instead of a dead page. `/vm` reaches its
  existing "no WebAssembly support" reporting; `/try` prints a note and keeps the launcher
  commands working.
- `try.js` loads `host.js` dynamically inside `start()` and reports a load failure in the
  terminal; input wiring and `error`/`unhandledrejection` reporting are registered before
  anything that can fail, so no asset failure can silence the prompt again.
- Terminal ergonomics on `/vm` (parity with `/try`, which already had click-to-focus): clicking
  anywhere in the console focuses the input when a program is reading; keystrokes typed while
  focus is elsewhere are routed into the input; a stray Enter when nothing is reading prints a
  hint instead of doing nothing; `cursor: text` over the console.
- Verified in real headless Chrome 148 with CDP-driven mouse/keyboard events (not the node
  harness): full eosh session on `/vm` (boot → click mid-console → `hello --name chrome
  --excited true` → routed-keystroke `ls /` → `exit`), `/try` click-anywhere + Enter, and both
  pages with `WebAssembly` deleted from the page (the failure mode above) staying interactive
  and self-explaining. node verify-{eosh,exec,fs,coreutils} and `cargo xtask check-web-vm`
  unchanged and green.

**Known pre-existing gap (not addressed here):** `/vm/selftest.html` currently reports
`FAIL sleepy reports the stackful-lift limitation honestly` on master too — `run_sleepy` now
*succeeds* fiberlessly (`sleepy.run() measured ~52 ms across its await`), so the check's
expectation (non-zero rc + a "stackful" line) is stale. Whether the canary's purpose changed or
the check should now assert success is the blob owner's call.

## Decision 24 — variadic coreutil arguments in the blob; the sleepy expectation flips to success

The positional/variadic argument merge re-signatured the path-taking coreutils to a trailing
`paths: list<string>` (and `head` to `(lines: u64, paths: list<string>)`), which the committed /vm
store assets and the blob's two argument paths did not yet understand.

**What changed**
- The blob's WAVE-lite codec (`execsurface.rs`) now parses `list<T>` values (top-level-comma split,
  quotes/brackets respected) and `bind_args` defaults a *missing* `list<…>` argument to the empty
  list, mirroring the host binder's empty-tail rule — so a bare `ls` works at the browser eosh
  prompt exactly like it does natively.
- The page program-row table (`store.rs`) gained `ArgKind::TextList`: a trailing variadic field that
  collects zero or more values into a `list<string>` (`cat`, `ls`, `wc`, `stat`, `rm`, `touch`, and
  `head`'s new `lines`-then-`paths` order).
- `cargo xtask build-web-vm` regenerated the store (new fingerprints for the seven re-signatured
  coreutils) and the blob (which also picks up the rebuilt eosh with positional/variadic binding);
  `check-web-vm` is green against the committed assets.
- Harnesses: `verify-coreutils.mjs` covers multi-path `cat`, a bare `ls`, and `head 2 <path>`;
  `verify-eosh.mjs` drives `cat /welcome.txt /docs/about.txt`, a bare `ls`, and the `only eo9:text`
  package shorthand at the interactive prompt. All four node/JSPI harnesses pass (coreutils 14/14,
  eosh 16/16, fs, exec).
- The stale selftest expectation from D23 is resolved the way the observed behaviour dictates:
  `run_sleepy` on the current blob *succeeds* (`sleepy.run() measured ~52 ms across its await`,
  rc 0), so `selftest.js` now asserts success-and-measured rather than the old refusal. The /vm page
  copy describing the canary as a reported limitation was updated to match. (The `store.rs`
  `run_sleepy` doc comment still describes the old refusal framing — comment-only, left for a later
  blob rebuild to avoid churning the fingerprint for a no-code change.)

**Noted, not addressed here:** the kernel's scalar WAVE arg codec still lacks the empty-tail
default for a missing trailing `list<string>` (bare `ls` on metal), and the pre-existing clippy
findings in `web-eo9-blob` (`not_unsafe_ptr_arg_deref` on the exported `extern "C"` entry points,
one `collapsible_if`) remain — the web-eo9 workspace is not in any clippy gate.

## Decision 25 — the blob workspace gets a lint gate inside `build-web-vm`

The `www/web-eo9` workspace stays out of `cargo xtask ci` (wasm32 target, heavy vendored closure),
so its lint debt was invisible. `cargo xtask build-web-vm` now runs `cargo fmt --all --check` and
`cargo clippy --workspace --release --target wasm32-unknown-unknown -- -D warnings` for the blob
workspace right after building it — anyone refreshing the committed /vm assets gets the lint pass
for free, and the gate cannot slow `ci` down. The pre-existing findings were fixed to make the
gate start green: `eosh_command` / `run_program` are now `unsafe extern "C"` with their pointer
contract documented (they dereference page-supplied pointers; the wasm export ABI is unchanged,
the JS callers are unaffected), and the `TRUNCATE` branch of `MemFs::open` uses a let-chain.

## Decision 26 — blob path-independence: remap flags in, a small cargo-metadata residue remains

The committed blob used to embed the absolute checkout path 184 times (panic-location strings for
the vendored path dependencies), so rebuilding the same sources from a different worktree changed
the blob's bytes and its fingerprinted URL. `build-web-vm` now passes
`--remap-path-prefix=<repo-root>=/eo9` (plus cargo-home and rustup-home prefixes) via `RUSTFLAGS`
for the blob build: the rebuilt blob contains **zero** repository paths, and a clean rebuild at the
same path is bit-identical.

What the flags do *not* fix: building from a second checkout path still produces a blob that
differs by ~410 bytes (~0.005%) with no path strings in either binary. The residue is cargo's
per-unit metadata hash: the blob workspace's `[patch.crates-io]` path dependencies live *outside*
its workspace root (`../../kernel/vendor/*`, `../../crates/eo9-component`), so their package ids —
and therefore `-Cmetadata`, symbol hashes and the resulting code layout — incorporate the absolute
checkout path (verified: the `libwasmtime-<hash>.rlib` unit hashes differ between two worktrees
while registry crates' match). Fixing that needs either cargo-side support or restructuring where
the vendored crates live relative to the blob workspace; not worth it now. Practical consequence:
regenerate the committed /vm assets from one canonical checkout per change (as we already do), and
expect a different-but-equivalent fingerprint if a different worktree regenerates them.

## Decision 27 — blob size: smaller release profile kept, lazy `/bin` fetch designed but skipped

`opt-level = "z"` + `panic = "abort"` (panics can only abort on wasm32-unknown-unknown anyway) on
top of the existing fat-LTO/1-CGU/strip profile takes the blob from 9,991,322 to 8,582,558 bytes
raw and 1,823,808 to 1,693,561 bytes brotli (−14% / −7%) with all four node/JSPI harnesses and the
in-blob compile path still passing.

The next real win would be lazily fetching the `/bin` raw+cwasm pairs and the embedded demo
artifacts from the page's HTTP store instead of `include_bytes!` (roughly 2 MB of the raw blob):
the store fetch path (`host_fetch_len`/`host_fetch_copy`) already exists, so the design is to seed
`/bin` entries as *names* resolved through the store on first use rather than bytes baked into the
blob. Skipped here because it changes the offline story (the node harnesses and the no-network
boot currently exercise a fully self-contained blob) and touches the exec-surface seeding path;
revisit if the blob needs to shrink further.

## Decision 28 — the `&` form is exercised end-to-end through the browser eosh prompt

`verify-eosh.mjs` now drives `entropy.seeded & entropy.seeded --seed 7 $ rng --count 2` at the
interactive prompt: a configure + extend + compose fusion with no pre-AOT'd artifact, compiled
in-blob and run. The checks assert the run produced output (no codegen refusal) and that `&` is
right-biased — the `--seed 7` layer shadows the default-seeded left layer, so the stream rng sees
differs from the plain `entropy.seeded $ rng` runs. 17/17 eosh harness checks pass.

## Decision 29 — the blob registers `wiring`; `time.frozen` joins `/bin` (the frozen-clock example ships)

Two follow-ups from other lanes, closed together because both force a blob/asset rebuild:

- **`wiring` on the exec surface.** The eosh on master now imports
  `eo9:exec/component-algebra.wiring` (plan/02 D18), so any future asset rebuild would have produced
  an eosh the blob cannot instantiate. `execsurface.rs` registers `wiring` exactly like the usermode
  runtime: it returns the algebra value's in-memory `wiring_tree()` — a composed value renders its
  full tree, a freshly loaded one a single leaf. Verified at the browser prompt:
  `describe entropy.seeded $ rng` ends with a `wiring:` section showing the `$ compose` node with the
  provider and consumer layers (the interposed provider is visible in-browser, same as
  `describe --wiring` natively).
- **`time.frozen` in `/bin`.** The stub is built into the blob's `/bin` (raw + pulley32, via the same
  xtask `/bin` loop as entropy.seeded — a one-line addition to that list), so the virtualized-clock
  composition is formable at the prompt. The try-it page gains the previously-dropped frozen-clock
  example (`time.frozen --now-seconds 0 --monotonic-ns 0 $ hello --name frozen --excited true`),
  verified before shipping the copy: the harness asserts the output line is exactly
  `[0.000000000] Hello, frozen!`.

Verification: `verify-eosh.mjs` grew the two checks (19/19 pass, still fully offline);
verify-coreutils 14/14, verify-fs, verify-exec pass; `check-web-vm` ok (18 assets);
full `cargo xtask ci` (incl. the www gate) green; a headless-Chrome smoke against the served
worktree site auto-boots to `eosh>` with no clicks and runs `hello` through the page's input
handler. Blob: 8,756,332 bytes raw / 1,714,796 brotli (the new eosh + time.frozen raw+cwasm add
~168 KB raw / ~21 KB wire over Decision 27's figures). Only the blob asset changed fingerprint —
the store `.cwasm` set reproduced byte-identically.

**Follow-up (2026-05-28, `eo9:rt/diagnostics`):** the guest SDK's panic handler now reports panic messages
through a new `eo9:rt/diagnostics.report-panic` import that every SDK-built component carries. Before the
next `/vm` asset rebuild (`build-web-vm`), the blob's exec/provider surface must register that import —
alongside the already-recorded `wiring` registration — or newly built components (including eosh and every
/bin program) will fail to instantiate in the browser. The committed assets predate the import and keep
working until then. Registering it as a per-child write-once slot surfaced in the child's trapped outcome
matches the usermode/kernel behavior; accepting and ignoring it is the minimum.

## Decision 30 — the blob registers `eo9:rt/diagnostics`; guest panic messages reach the browser's trapped reasons (2026-05-29)

The SDK's panic handler now reports the panic message and source location through a
`eo9:rt/diagnostics.report-panic` import that every SDK-built component carries (plan/02 D19, plan/07 D13),
so the blob had to serve that import before any `/vm` asset rebuild. Implemented as the full usermode mirror
rather than the accept-and-ignore minimum:

- `providers.rs`: `WebState` gains a write-once, 1 KiB-bounded `panic_message` slot (char-boundary
  truncation, mirroring `eo9-runtime`); `add_diagnostics` registers `report-panic` into it and is called from
  both `add_providers` and `add_providers_for` — never gated by an `only` allow-list, matching
  `eo9-component::restrict`'s always-admit rule (the sink grants no authority and is required for any
  SDK-built child to instantiate). `trapped_reason(error, panic_message)` renders
  `guest panicked: <message> at <file>:<line> — <wasmtime error>` when a message was reported.
- `execsurface.rs`: `run_child_inner` no longer `?`s the `main` error away — it flattens the three result
  layers with the store still in scope and folds the slot into the child's `trapped(...)` outcome (the reason
  eosh prints). `is_root_provided` adds `eo9:rt/diagnostics`, since the executor itself serves it — without
  that, `compile` refused every freshly built program with `NotClosed(["eo9:rt/diagnostics"])`.
- `store.rs` (`run_program`): same fold for the page/JS path. `lib.rs`'s `entropy.seeded` demo (a bare
  `Linker<()>`) registers an accept-and-drop sink so the rebuilt stub still instantiates.
- The demo `Linker<()>` paths for the hand-written seed component are untouched (it is not SDK-built).

Assets rebuilt once (`build-web-vm`): every store/`/bin` program and eosh now carry the import; blob
8,830,026 B raw / 1,725,392 B brotli (+74 KB raw / +11 KB wire over D29 — the import set grew in every
embedded program). Verification: verify-eosh 19/19 (offline), verify-coreutils **15/15** including a new
check that `outcomes` in trap mode reports
`guest panicked: outcomes: trapping as requested at examples/outcomes/src/lib.rs:<line>` through
`run_program`; verify-fs and verify-exec pass; `check-web-vm` ok; a headless-Chrome `--dump-dom` smoke
against the served worktree site auto-boots to the `eosh>` prompt with no interaction. The eosh-spawn fold
(`run_child`) shares the same slot/render code but is not separately driven by the harness — `/bin` ships no
panicking program; if one is ever added, assert the message at the prompt too.

## Decision 31 — try-it page: type into the terminal, an "Explore the sandbox" section, and bare-default examples (2026-05-29, owner feedback)

Owner feedback on the simplified try-it page, all four items implemented and verified at the browser
prompt (verify-eosh.mjs drives every command shown on the page; a headless-Chrome CDP smoke types into
the page with real key events):

- **No separate input box.** Keystrokes go straight into the terminal: while the shell's read-line is
  pending, vm.js renders a live `> …` line with a blinking block cursor inside `#vm-output` itself;
  printable keys, Backspace, Enter, and paste (newline submits) are captured at the document level, the
  same place the previous routing already lived. The `#vm-input-row` element and its CSS are gone; the
  finished line freezes into the ordinary echoed command line. Form fields and modifier chords are left
  alone, so text selection/copy still works.
- **`hello` takes optional arguments** (the cross-cutting part is recorded here because the page is why
  it happened): `name`/`excited` are now `option<…>` with in-program defaults ("world", false), so a bare
  `hello` works at every prompt. eosh already completed missing optional arguments to `none`; the CLI's
  flag binder now wraps supplied values for `option<…>` parameters as `some(…)` and the runtime binds an
  unsupplied `option<…>` parameter to `none` (both documented in their doc comments) — without those two
  pieces the direct `eo9 hello --name user` path would have regressed. The blob's `algebra_demo`/
  `compile_demo` exports pass `Val::Option`-wrapped arguments to match.
- **"Explore the sandbox"** section before the examples teaches the discovery loop — `help`, `ls /bin`,
  `describe hello` / `describe entropy.seeded`, then a first composition. **`env` is deliberately not on
  the page**: the browser exec surface has no session manifest, so the builtin only prints "no session
  capability information available" — surfacing the session manifest in the blob is the recorded gap.
- **Examples simplified to defaults**: "Run a program" is now `describe hello` then `hello --name user`
  (typed-arguments/typed-outcome caption kept); the `only` pass/refuse pair and the frozen-clock example
  drop hello's arguments entirely.

The /vm assets were rebuilt once for the new hello + blob (blob 8,829,574 B raw / 1,725,277 B brotli);
all four node/JSPI harnesses pass (verify-eosh now 27 checks), check-web-vm is green, and the full
`cargo xtask ci` gate passes.

## Decision 32 — the blob seeds a session manifest, so `env` works in the browser (2026-05-29)

The D31 gap is closed: `WebState::new()` now seeds `/session` (the `eo9-session 1` format from eosh-core's
`envinfo`, the same file the usermode and kernel embedders write) into the in-memory filesystem, describing
exactly what the page grants — shell: the page terminal, the browser clock, crypto entropy, the in-memory
fs, and exec (algebra + in-browser compiler + spawn); children: the same minus exec, with a fresh fs per
run; notes that nothing leaves the page and that `only` restricts a command. The text is informational
(the linker registrations in `providers.rs`/`execsurface.rs` remain the authority) and lives next to the
manifest function so the two stay in sync. eosh's `env` builtin therefore reports real capabilities in the
browser; verify-eosh drives `env` (asserting the granted/receive sections) and now also asserts that
`describe entropy.seeded` shows the provider's configure argument. The try-it page's "Explore the sandbox"
copy can now add an `env` line — a one-line follow-up deliberately not made here (the page was left
untouched in this change).

## Decision 33 — try-it terminal polish: full-width terminal, type on the prompt line (2026-05-29, owner feedback)

Three owner fixes to the /vm page, all presentation-side (no blob or fingerprinted-asset changes):

- **The terminal breaks out of the prose column.** The shell's own output (`help`, `env`, `describe`) is
  laid out for ~100 monospace columns, but `#vm-output` inherited `main`'s 44rem prose width (~77 columns
  at 0.85rem), so help lines wrapped. The terminal now sizes itself to `min(58rem, 100vw - 2.5rem)` and
  centers itself in the viewport with a negative-margin breakout (`margin-left: calc((100% - W) / 2)`),
  giving ~109 columns on desktop while collapsing to exactly the old gutters on narrow screens; long lines
  still `pre-wrap` rather than scroll sideways. Verified in headless Chrome at 1280px: the longest help
  line (100 chars, the `env <expr>` row) renders as a single client rect, 20px tall.
- **Typed input renders on the prompt line itself.** `armReadLine` previously opened a *new* line starting
  with a green `> `, which read as a duplicate prompt under the shell's own `eosh>` line. The live input
  (text span + block cursor) is now appended to the last line the shell printed — the prompt line — and the
  finished command freezes there, so the transcript reads `eosh> hello` exactly like a real terminal. No
  `> ` marker is rendered anywhere anymore (verified: zero such lines after `help` and a run).
- **Copy**: the "there is no separate input box" sentence is gone — typing into the terminal is
  self-evident; the note now only says `exit` ends the session and reload gives a fresh one.

Verification: node/JSPI verify-eosh (unchanged blob) still passes; www workspace tests green; check-web-vm
still ok (only vm.css/vm.js/index.html and their precompressed siblings changed); headless-Chrome CDP run
with real key events confirms the width, the absence of stray `> ` lines, `eosh> hello` rendering on one
line while typing (cursor on the same line), and the bare `hello` command running to `ok: greeted`.
The "Explore the sandbox" `env` line from D32 remains the deliberate follow-up.
