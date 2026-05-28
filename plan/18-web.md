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
