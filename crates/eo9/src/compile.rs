//! Compilation through the store's compile cache: the canonical compile-opts text, the
//! plan-06 cache key, cache warming, and the `eo9 compile` subcommand.
//!
//! The cache key follows plan/06-store.md exactly: the ordered module hashes (today a
//! single unfused component), the configure constants (none yet — composition is later
//! work), the canonical compile-opts text, the host target triple, the pinned wasmtime
//! version string, and `compiler_deterministic = false` until plan 04 verifies
//! bit-identical codegen.
//!
//! The cached artifact is the engine's serialized compilation output
//! (`Engine::precompile_component`). `eo9-runtime` does not yet expose a way to build a
//! runnable [`Image`](eo9_runtime::Image) back from those bytes (image serialization is a
//! deferred item in plan/04-runtime.md), so a cache hit currently skips re-producing the
//! artifact while `run` still compiles its in-memory image from the component bytes; see
//! plan/11-usermode.md Decisions for the escalation.

use eo9_runtime::{EngineOptions, new_engine};
use eo9_store::{CacheKey, CacheKeyParams, Store};

use crate::cli::{Config, EXIT_SUCCESS, vlog};
use crate::source::{self, ProgramSource};

/// The target triple images are compiled for: the host triple (see `build.rs`).
pub const TARGET_TRIPLE: &str = env!("EO9_TARGET_TRIPLE");

/// The compiler version string for cache keys: the pinned wasmtime (Cranelift) version,
/// read from the workspace lockfile at build time (see `build.rs`).
pub const COMPILER_VERSION: &str = concat!("wasmtime-", env!("EO9_WASMTIME_VERSION"), " cranelift");

/// Whether codegen has been *verified* deterministic. The runtime's engine profile is
/// configured for determinism but bit-identical output has not been verified yet
/// (plan/04-runtime.md), so entries are keyed as non-deterministic per plan 06.
pub const COMPILER_DETERMINISTIC: bool = false;

/// Engine options derived from the CLI configuration.
pub fn engine_options(cfg: &Config) -> EngineOptions {
    EngineOptions {
        debug_info: cfg.debug_info,
    }
}

/// Canonical text rendering of the compile options, for the cache key. Everything not
/// listed here is pinned by the runtime's engine profile and therefore covered by the
/// compiler version string.
pub fn compile_opts_text(cfg: &Config) -> String {
    format!(
        "eo9-compile-opts 1\nengine-profile eo9-pinned\ndebug-info {}\n",
        cfg.debug_info
    )
}

/// The plan-06 cache key parameters for compiling one (unfused) component.
pub fn cache_key_params(cfg: &Config, source: &ProgramSource) -> CacheKeyParams {
    CacheKeyParams {
        module_hashes: vec![source.hash],
        configure_constants: Vec::new(),
        compile_opts: compile_opts_text(cfg),
        target_triple: TARGET_TRIPLE.to_string(),
        compiler_version: COMPILER_VERSION.to_string(),
        compiler_deterministic: COMPILER_DETERMINISTIC,
    }
}

/// The result of [`ensure_cached`]: the entry's key and whether the lookup hit.
pub struct CacheOutcome {
    pub key: CacheKey,
    pub hit: bool,
}

/// Make sure the compile cache holds an image for `source`, producing and inserting one
/// via `precompile` on a miss. A hit bumps the entry's usage metadata.
pub fn ensure_cached(
    cfg: &Config,
    store: &Store,
    source: &ProgramSource,
    precompile: impl FnOnce(&[u8]) -> Result<Vec<u8>, String>,
) -> Result<CacheOutcome, String> {
    let params = cache_key_params(cfg, source);
    let key = params.key();
    let cached = store
        .lookup_image(&key)
        .map_err(|err| format!("compile-cache lookup failed: {err}"))?;
    match cached {
        Some(entry) => {
            vlog!(
                cfg,
                "compile cache hit: key {key}, image {} bytes, use-count {}",
                entry.metadata.image_size,
                entry.metadata.use_count
            );
            Ok(CacheOutcome { key, hit: true })
        }
        None => {
            vlog!(cfg, "compile cache miss: key {key}; compiling");
            let image = precompile(&source.bytes)?;
            store
                .insert_image(&params, &image)
                .map_err(|err| format!("compile-cache insert failed: {err}"))?;
            vlog!(
                cfg,
                "compile cache filled: key {key}, image {} bytes",
                image.len()
            );
            Ok(CacheOutcome { key, hit: false })
        }
    }
}

/// `eo9 compile <name-or-path>`: warm the compile cache for a program.
pub fn cmd_compile(cfg: &Config, reference: &str) -> Result<u8, String> {
    let source = source::resolve_program(cfg, reference)?;
    let store = cfg.open_store()?;
    let engine = new_engine(&engine_options(cfg))
        .map_err(|err| format!("cannot create the engine: {err:#}"))?;
    let outcome = ensure_cached(cfg, &store, &source, |bytes| {
        engine
            .precompile_component(bytes)
            .map_err(|err| format!("compilation of {} failed: {err:#}", source.origin))
    })?;
    if outcome.hit {
        println!("compile cache hit: {}", outcome.key);
    } else {
        println!("compiled and cached: {}", outcome.key);
    }
    Ok(EXIT_SUCCESS)
}

#[cfg(test)]
mod tests {
    use super::*;
    use eo9_store::ObjectHash;

    fn source_of(bytes: &[u8]) -> ProgramSource {
        ProgramSource {
            bytes: bytes.to_vec(),
            hash: ObjectHash::of(bytes),
            origin: "test".to_string(),
        }
    }

    #[test]
    fn build_facts_are_present() {
        assert!(!TARGET_TRIPLE.is_empty());
        assert!(TARGET_TRIPLE.contains('-'), "not a triple: {TARGET_TRIPLE}");
        assert!(
            COMPILER_VERSION.starts_with("wasmtime-") && !COMPILER_VERSION.contains("unknown"),
            "compiler version not resolved from the lockfile: {COMPILER_VERSION}"
        );
    }

    #[test]
    fn cache_key_tracks_its_inputs() {
        let cfg = Config::default();
        let source = source_of(b"component bytes");

        let base = cache_key_params(&cfg, &source).key();
        assert_eq!(cache_key_params(&cfg, &source).key(), base);

        // Different module bytes -> different key.
        let other = source_of(b"different component bytes");
        assert_ne!(cache_key_params(&cfg, &other).key(), base);

        // Different compile options -> different key.
        let debug_cfg = Config {
            debug_info: true,
            ..Config::default()
        };
        assert_ne!(cache_key_params(&debug_cfg, &source).key(), base);
    }

    #[test]
    fn compile_opts_text_mentions_every_variable_option() {
        let cfg = Config::default();
        let text = compile_opts_text(&cfg);
        assert!(text.contains("debug-info false"), "unexpected text: {text}");
        let debug_cfg = Config {
            debug_info: true,
            ..Config::default()
        };
        assert!(compile_opts_text(&debug_cfg).contains("debug-info true"));
    }
}
