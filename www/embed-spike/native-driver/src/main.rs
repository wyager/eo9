//! Host-side driver for the wasm32 embed spike.
//!
//! `preaot` precompiles real Eo9 components to Pulley (`pulley32`) artifacts with the same
//! compile-relevant configuration the wasm32 probe uses at load time (mirroring how xtask's
//! `precompile_for_kernel` pairs with the kernel engine). `run` executes the probe blob —
//! a wasm32 cdylib that itself embeds wasmtime — under the repository's pinned wasmtime,
//! providing only an `env.host_log` import so the probe can report what happened.

use std::path::{Path, PathBuf};
use std::time::Instant;

use wasmtime::{Caller, Config, Engine, Error, Linker, Module, Result, Store};

fn msg(message: impl Into<String>) -> Error {
    Error::msg(message.into())
}

fn repo_root() -> PathBuf {
    // native-driver -> embed-spike -> www -> repository root.
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(3)
        .expect("repository root")
        .to_path_buf()
}

fn artifacts_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../artifacts")
}

/// The compile-relevant configuration shared by every artifact and every probe engine.
/// Keep in sync with `wasm-host-probe/src/lib.rs::base_config` (and compare both against
/// xtask's `precompile_for_kernel`, which this mirrors apart from the target).
fn preaot_config(consume_fuel: bool) -> Result<Config> {
    let mut config = Config::new();
    config.target("pulley32")?;
    config.wasm_component_model(true);
    config.wasm_component_model_async(true);
    config.wasm_component_model_async_stackful(true);
    config.wasm_component_model_more_async_builtins(true);
    config.signals_based_traps(false);
    config.memory_reservation(0);
    config.memory_reservation_for_growth(1 << 20);
    config.memory_guard_size(0);
    config.memory_init_cow(false);
    config.concurrency_support(true);
    config.gc_support(false);
    config.wasm_threads(false);
    config.consume_fuel(consume_fuel);
    // The vendored wasmtime family (used via [patch] so the probe gets the CM-async/no_std
    // relaxation) carries the kernel's single-core compile-context lock, which panics on
    // contention; compile single-threaded here, it's a handful of small artifacts.
    config.parallel_compilation(false);
    Ok(config)
}

fn preaot_one(input: &Path, output_name: &str, consume_fuel: bool) -> Result<()> {
    let bytes = std::fs::read(input).map_err(|error| {
        msg(format!(
            "reading {} failed ({error}); run `cargo xtask build-guest` first for guest components",
            input.display()
        ))
    })?;
    let engine = Engine::new(&preaot_config(consume_fuel)?)?;
    let artifact = engine.precompile_component(&bytes).map_err(|error| {
        msg(format!(
            "precompiling {} for pulley32 failed: {error:?}",
            input.display()
        ))
    })?;
    let out_dir = artifacts_dir();
    std::fs::create_dir_all(&out_dir).map_err(|error| msg(format!("mkdir artifacts: {error}")))?;
    let out_path = out_dir.join(output_name);
    std::fs::write(&out_path, &artifact)
        .map_err(|error| msg(format!("writing {}: {error}", out_path.display())))?;
    println!(
        "preaot: {} -> {} ({} bytes, target pulley32, consume_fuel = {consume_fuel})",
        input.display(),
        out_path.display(),
        artifact.len()
    );
    Ok(())
}

fn preaot() -> Result<()> {
    let root = repo_root();
    let seed_wat = root.join("kernel/seed/hello.wat");
    let entropy_seeded = root.join("guest/target/components/eo9-stub-entropy-seeded.wasm");
    preaot_one(&seed_wat, "seed.cwasm", false)?;
    preaot_one(&seed_wat, "seed-fuel.cwasm", true)?;
    preaot_one(&entropy_seeded, "entropy-seeded.cwasm", false)?;
    Ok(())
}

/// The vendored compile crates gate host-target inference (`cranelift-native`) off, so the
/// driver names its own host triple explicitly when compiling the probe module.
fn host_triple() -> &'static str {
    if cfg!(all(target_arch = "aarch64", target_os = "macos")) {
        "aarch64-apple-darwin"
    } else if cfg!(all(target_arch = "x86_64", target_os = "macos")) {
        "x86_64-apple-darwin"
    } else if cfg!(all(target_arch = "aarch64", target_os = "linux")) {
        "aarch64-unknown-linux-gnu"
    } else {
        "x86_64-unknown-linux-gnu"
    }
}

fn run_probe(probe_path: &Path) -> Result<()> {
    // Single-threaded compilation for the same reason as `preaot_config`.
    let mut config = Config::new();
    config.parallel_compilation(false);
    config.target(host_triple())?;
    let engine = Engine::new(&config)?;
    let module = Module::from_file(&engine, probe_path)
        .map_err(|error| msg(format!("loading probe {}: {error:?}", probe_path.display())))?;

    let mut linker: Linker<()> = Linker::new(&engine);
    linker.func_wrap(
        "env",
        "host_log",
        |mut caller: Caller<'_, ()>, ptr: u32, len: u32| -> Result<()> {
            let memory = caller
                .get_export("memory")
                .and_then(|export| export.into_memory())
                .ok_or_else(|| msg("probe has no exported memory"))?;
            let mut buffer = vec![0u8; len as usize];
            memory
                .read(&caller, ptr as usize, &mut buffer)
                .map_err(|error| msg(format!("reading host_log message: {error}")))?;
            println!("[probe] {}", String::from_utf8_lossy(&buffer));
            Ok(())
        },
    )?;

    let mut store = Store::new(&engine, ());
    let instance = linker.instantiate(&mut store, &module)?;
    let run = instance.get_typed_func::<(), i32>(&mut store, "run")?;

    let started = Instant::now();
    let failures = run.call(&mut store, ())?;
    let elapsed = started.elapsed();
    println!(
        "driver: probe finished in {elapsed:?} with {failures} failed step(s) \
         (probe blob: {} bytes)",
        std::fs::metadata(probe_path)
            .map(|metadata| metadata.len())
            .unwrap_or(0)
    );
    if failures != 0 {
        return Err(msg(format!("{failures} probe step(s) failed")));
    }
    Ok(())
}

/// Drive the web VM blob (www/web-eo9) the same way the /vm page's JavaScript does: provide
/// `env.host_write`, then call `boot`, `run_hello`, `run_fuel`, and
/// `run_entropy(seed, count)`. This is the manual-equivalent check for the page when no
/// scriptable browser with WebAssembly is at hand; the blob bytes are exactly the ones the
/// site serves.
fn verify_blob(blob_path: &Path) -> Result<()> {
    let mut config = Config::new();
    config.parallel_compilation(false);
    config.target(host_triple())?;
    let engine = Engine::new(&config)?;
    let module = Module::from_file(&engine, blob_path)
        .map_err(|error| msg(format!("loading blob {}: {error:?}", blob_path.display())))?;

    let mut linker: Linker<()> = Linker::new(&engine);
    linker.func_wrap(
        "env",
        "host_write",
        |mut caller: Caller<'_, ()>, ptr: u32, len: u32| -> Result<()> {
            let memory = caller
                .get_export("memory")
                .and_then(|export| export.into_memory())
                .ok_or_else(|| msg("blob has no exported memory"))?;
            let mut buffer = vec![0u8; len as usize];
            memory
                .read(&caller, ptr as usize, &mut buffer)
                .map_err(|error| msg(format!("reading host_write message: {error}")))?;
            println!("[blob] {}", String::from_utf8_lossy(&buffer));
            Ok(())
        },
    )?;

    let mut store = Store::new(&engine, ());
    let instance = linker.instantiate(&mut store, &module)?;
    let mut failures = 0i32;
    for (name, args) in [
        ("boot", vec![]),
        ("run_hello", vec![]),
        ("run_fuel", vec![]),
        // seed 0xe09 (lo, hi) and 4 draws — the page's defaults.
        ("run_entropy", vec![0xe09i32, 0, 4]),
    ] {
        let func = instance
            .get_func(&mut store, name)
            .ok_or_else(|| msg(format!("blob does not export `{name}`")))?;
        let params: Vec<wasmtime::Val> = args
            .iter()
            .map(|value| wasmtime::Val::I32(*value))
            .collect();
        let mut results = [wasmtime::Val::I32(-1)];
        func.call(&mut store, &params, &mut results)
            .map_err(|error| msg(format!("calling `{name}`: {error:?}")))?;
        let code = match results[0] {
            wasmtime::Val::I32(code) => code,
            _ => -1,
        };
        println!("driver: {name} -> {code}");
        if code != 0 {
            failures += 1;
        }
    }
    if failures != 0 {
        return Err(msg(format!("{failures} blob call(s) failed")));
    }
    Ok(())
}

fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        Some("preaot") => preaot(),
        Some("run") => {
            let probe = args
                .get(1)
                .ok_or_else(|| msg("usage: native-driver run <probe.wasm>"))?;
            run_probe(Path::new(probe))
        }
        Some("verify-blob") => {
            let blob = args
                .get(1)
                .ok_or_else(|| msg("usage: native-driver verify-blob <web-eo9.wasm>"))?;
            verify_blob(Path::new(blob))
        }
        _ => Err(msg(
            "usage: native-driver <preaot | run <probe.wasm> | verify-blob <web-eo9.wasm>>",
        )),
    }
}
