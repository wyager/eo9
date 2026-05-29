//! On-target codegen: compile a WebAssembly component to native code **on the machine**
//! and run it (plan/12-kernel.md Decisions 26-29).
//!
//! The rest of the kernel runs *precompiled* artifacts produced on the host by
//! `cargo xtask build-kernel`. This module instead takes the raw (un-precompiled) seed
//! component bytes and feeds them to `Component::new`, which drives the vendored Cranelift
//! + wasmtime compile layers (built `no_std` under the `wasm-codegen` feature) to emit
//! native code into a heap allocation, publish it through the cache-maintenance code
//! publisher (`super::BareMetalCodeMemory`), and execute it — host-side AOT is then only a
//! bootstrap convenience, not a requirement.
//!
//! Success here (the seed's `hello()`/`add()` returning correct results) is the milestone
//! that retires the plan's single riskiest assumption: that Cranelift can run under the
//! kernel's `no_std + alloc` environment.

use alloc::string::String;

use wasmtime::Store;
use wasmtime::component::{Component, Linker};

/// The raw, un-precompiled seed component (assembled from WAT by `cargo xtask build-kernel`).
static SEED_WASM: &[u8] = include_bytes!(env!("EO9_SEED_WASM"));

/// Compile the seed component on-target, instantiate it, and call its exports.
pub fn run() {
    crate::kprintln!(
        "wasm codegen: compiling a {} byte component on-target with Cranelift…",
        SEED_WASM.len()
    );
    let start_us = crate::timer::uptime_us();
    match try_run() {
        Ok((greeting, sum, compile_us)) => {
            let total_us = crate::timer::uptime_us() - start_us;
            crate::kprintln!("wasm codegen: compiled on-target in {compile_us} us");
            crate::kprintln!("wasm codegen: hello() -> \"{greeting}\"");
            crate::kprintln!("wasm codegen: add(17, 25) -> {sum}");
            crate::kprintln!("wasm codegen: compile + instantiate + 2 calls took {total_us} us");
        }
        Err(error) => crate::kprintln!("wasm codegen: FAILED: {error:?}"),
    }
}

fn try_run() -> Result<(String, u32, u64), wasmtime::Error> {
    // The engine config (target + OS-less tunables + code publisher) is identical to the
    // one used to deserialize artifacts; the only difference here is that we hand it raw
    // wasm via `Component::new`, which compiles on-target instead of deserializing.
    let engine = super::new_engine()?;

    // The on-target compile: Cranelift turns the wasm bytes into native code (for this
    // machine's architecture) in a
    // heap allocation, and the code publisher does the I-/D-cache maintenance.
    let compile_start = crate::timer::uptime_us();
    let component = Component::new(&engine, SEED_WASM)?;
    let compile_us = crate::timer::uptime_us() - compile_start;

    let linker: Linker<()> = Linker::new(&engine);
    let mut store = Store::new(&engine, ());
    // The engine meters fuel (see `new_engine`); the demo gets an effectively-unlimited pool.
    store.set_fuel(u64::MAX)?;
    let instance = super::block_on(
        "codegen instantiation",
        linker.instantiate_async(&mut store, &component),
    )??;

    let hello = instance.get_typed_func::<(), (String,)>(&mut store, "hello")?;
    let (greeting,) = super::block_on("codegen hello()", hello.call_async(&mut store, ()))??;

    let add = instance.get_typed_func::<(u32, u32), (u32,)>(&mut store, "add")?;
    let (sum,) = super::block_on("codegen add()", add.call_async(&mut store, (17, 25)))??;

    Ok((greeting, sum, compile_us))
}
