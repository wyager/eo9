//! Run the embedded, host-precompiled seed component (the spike-step-1 canary).
//!
//! The seed (kernel/seed/hello.wat) imports nothing, so a successful run proves the
//! platform layer — engine, code publishing, canonical ABI string/typed-call paths — is
//! healthy independently of the eo9 provider linking exercised by [`super::hello`].

use alloc::string::String;

use wasmtime::Store;
use wasmtime::component::{Component, Linker};

/// The host-precompiled seed component, injected by `cargo xtask build-kernel aarch64`.
static SEED_CWASM: &[u8] = include_bytes!(env!("EO9_SEED_CWASM"));

/// Deserialize, instantiate, and call the seed component, reporting over serial.
pub fn run() {
    crate::kprintln!(
        "wasm seed: {} byte precompiled component embedded in the image",
        SEED_CWASM.len()
    );
    let start_us = crate::timer::uptime_us();
    match try_run() {
        Ok((greeting, sum)) => {
            let elapsed_us = crate::timer::uptime_us() - start_us;
            crate::kprintln!("wasm seed: hello() -> \"{greeting}\"");
            crate::kprintln!("wasm seed: add(17, 25) -> {sum}");
            crate::kprintln!("wasm seed: deserialize + instantiate + 2 calls took {elapsed_us} us");
        }
        Err(error) => crate::kprintln!("wasm seed: FAILED: {error:?}"),
    }
}

fn try_run() -> Result<(String, u32), wasmtime::Error> {
    let engine = super::new_engine()?;

    // SAFETY: the artifact was produced by `cargo xtask build-kernel aarch64` with the
    // same wasmtime version, targeting exactly this machine and engine configuration, and
    // is embedded read-only in the kernel image.
    let component = unsafe { Component::deserialize(&engine, SEED_CWASM)? };

    let linker: Linker<()> = Linker::new(&engine);
    let mut store = Store::new(&engine, ());
    // The engine meters fuel (see `new_engine`); the demo gets an effectively-unlimited pool.
    store.set_fuel(u64::MAX)?;
    let instance = super::block_on(
        "seed instantiation",
        linker.instantiate_async(&mut store, &component),
    )??;

    let hello = instance.get_typed_func::<(), (String,)>(&mut store, "hello")?;
    let (greeting,) = super::block_on("seed hello()", hello.call_async(&mut store, ()))??;

    let add = instance.get_typed_func::<(u32, u32), (u32,)>(&mut store, "add")?;
    let (sum,) = super::block_on("seed add()", add.call_async(&mut store, (17, 25)))??;

    Ok((greeting, sum))
}
