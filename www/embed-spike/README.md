# wasm32 embed spike

Feasibility probe for running the **real Eo9 stack in the browser** by compiling the usermode
runtime (wasmtime included, Pulley interpreter backend) to a single wasm32 blob — the
direction chosen for `/try` v2 and the future `eo9-embed` / `eo9 bundle` milestone — instead
of re-implementing exec in JavaScript over transpiled components (the `/try` v1 path).

Findings are recorded in `plan/15-website.md` ("wasm32 embed spike"). Headlines:

- A real Eo9 component, pre-AOT'd to **`pulley32`** on the host, **deserializes, instantiates,
  and runs correctly inside wasmtime compiled for `wasm32-unknown-unknown`** (the kernel seed
  component's `hello()`/`add()` over the sync canonical ABI), with fuel metering active.
- wasmtime on wasm32 must be built **without its `std` feature** (the std platform layer
  assumes mmap), i.e. the same custom-platform embedding the bare-metal kernel uses, including
  the kernel's vendored `component-model-async` relaxation and the embedder TLS symbols.
- The **fiber gap**: every call into an async-lifted export (`call_async`, and
  `run_concurrent`/`call_concurrent` alike) requires a `wasmtime-fiber` fiber, and there is no
  wasm32 stack-switching backend — so CM-async guests (every real Eo9 program) do not run on a
  wasm32 host yet. Sync instantiation of CM-async components works; only the calls are blocked.
- In-blob compilation (cranelift) is additionally blocked on wasmtime's `std`/mmap assumptions —
  the same port the kernel's on-target-codegen rung needs — so v2 ships pre-AOT'd Pulley images.

This workspace is standalone: `cargo xtask ci` neither builds nor depends on it.

## Reproducing

```sh
# 1. Build the guest components (provides eo9-stub-entropy-seeded.wasm):
cargo xtask build-guest

# 2. Pre-AOT the demo components to pulley32 artifacts (writes ./artifacts/*.cwasm):
cd www/embed-spike
cargo run --release -p native-driver -- preaot

# 3. Build the wasm32 probe (embeds the artifacts) and run it under the native driver:
cargo build --release -p wasm-host-probe --target wasm32-unknown-unknown
cargo run --release -p native-driver -- run target/wasm32-unknown-unknown/release/wasm_host_probe.wasm
```

Expected output: the seed `hello()`/`add()` results and the fuel measurements succeed; the two
CM-async strategies report `fibers unsupported on this host architecture` — that error is the
finding, not a bug in the probe.

`artifacts/` and `target/` are generated and git-ignored; the probe cannot build before step 2.
