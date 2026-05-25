//! The pinned Wasmtime engine configuration for Eo9.
//!
//! Everything that executes guest code in usermode goes through an [`wasmtime::Engine`]
//! built here, so the knobs that matter for Eo9's guarantees are set in exactly one place:
//!
//! * **Components + component-model async.** Eo9 programs are components and all Eo9 I/O is
//!   expressed with the Component Model's async vocabulary (`future`/`stream`,
//!   waitable-sets), so `component-model` and `component-model-async` are always on.
//!   The stackful-async (🚟) and "more async builtins" (🚝) sub-features are also enabled:
//!   the guest-side `waitable-set.wait` path the Eo9 guest SDK uses needs them (see the
//!   spike notes in plan/04-runtime.md § Decisions).
//! * **Fuel metering on.** Codegen inserts fuel checks; fuel is the donate-and-run CPU
//!   budget of `eo9:exec/task.resume` and the deterministic yield mechanism (SPEC
//!   "Performance" / "Execution APIs"). Epoch interruption stays off (non-deterministic).
//! * **Determinism.** NaN canonicalization and deterministic relaxed-SIMD lowering are
//!   enabled so single-context execution is bit-deterministic. Shared-memory threads stay
//!   disabled (parallelism is a capability, not a default — SPEC "Execution APIs").
//! * **Codegen determinism for the compile cache** (plan 06) is *not* asserted here beyond
//!   choosing a fixed opt level; what Wasmtime guarantees about bit-identical artifacts is
//!   recorded in plan/04-runtime.md § Decisions and escalated where it falls short.

use wasmtime::{Config, Engine, OptLevel, Result};

/// Options for building an Eo9 engine. Everything not exposed here is pinned.
#[derive(Debug, Clone, Default)]
pub struct EngineOptions {
    /// Emit full debug info (DWARF) into compiled images so native tasks are inspectable
    /// by a host debugger. Maps to `compile-opts.debug-info` in `eo9:exec/compile`.
    pub debug_info: bool,
}

/// Build the pinned Eo9 engine configuration.
pub fn config(opts: &EngineOptions) -> Config {
    let mut config = Config::new();

    // Components and the component-model async ABI.
    config.wasm_component_model(true);
    config.wasm_component_model_async(true);
    config.wasm_component_model_async_stackful(true);
    config.wasm_component_model_more_async_builtins(true);

    // CPU is fuel (SPEC: "codegen inserts yield points (fuel checks)"). Epochs stay off.
    config.consume_fuel(true);
    config.epoch_interruption(false);

    // Determinism of execution: canonical NaNs, deterministic relaxed SIMD, no threads.
    config.cranelift_nan_canonicalization(true);
    config.relaxed_simd_deterministic(true);
    config.wasm_threads(false);

    // Fixed codegen options so compile output is a function of (input, options, version).
    config.cranelift_opt_level(OptLevel::Speed);
    config.parallel_compilation(false);

    // Guest traps are mapped into outcomes; wasm backtraces stay at their default (on) for
    // trap diagnostics.
    config.debug_info(opts.debug_info);

    config
}

/// Build an [`Engine`] with the pinned Eo9 configuration.
pub fn new_engine(opts: &EngineOptions) -> Result<Engine> {
    Engine::new(&config(opts))
}

/// A fingerprint of everything that determines whether a serialized
/// [`Image`](crate::Image) is loadable by the given engine: the wasmtime version, the
/// host target, and every compile-relevant configuration flag (for Eo9 that reduces to
/// the [`EngineOptions`] used, since the rest is pinned in [`config`]).
///
/// Two engines with equal fingerprints accept each other's serialized images; anything
/// else is rejected by [`Image::deserialize`](crate::Image::deserialize). Intended as the
/// engine component of a compilation-cache key (areas 06/11), alongside the content hash
/// of the component being compiled. The value is stable for a given toolchain build but
/// not across Rust/wasmtime upgrades — exactly the invalidation a cache wants.
pub fn compatibility_hash(engine: &Engine) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    engine.precompile_compatibility_hash().hash(&mut hasher);
    hasher.finish()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn engine_builds() {
        new_engine(&EngineOptions::default()).expect("pinned engine config must be valid");
    }

    #[test]
    fn engine_builds_with_debug_info() {
        new_engine(&EngineOptions { debug_info: true }).expect("debug-info config must be valid");
    }

    #[test]
    fn compatibility_hash_is_stable_for_equal_options() {
        let a = new_engine(&EngineOptions::default()).unwrap();
        let b = new_engine(&EngineOptions::default()).unwrap();
        assert_eq!(compatibility_hash(&a), compatibility_hash(&b));
    }
}
