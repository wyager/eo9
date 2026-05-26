//! Compilation through the store's compile cache: the canonical compile-opts text, the
//! plan-06 cache key, loading an image from the cache (or compiling and caching one),
//! and the `eo9 compile` subcommand.
//!
//! The cache key follows plan/06-store.md: the ordered module hashes (today a single
//! unfused component), the configure constants (none yet — composition is later work),
//! the canonical compile-opts text, the host target triple, the engine identity (the
//! pinned wasmtime version from the workspace lockfile plus the engine's runtime
//! compatibility fingerprint), and `compiler_deterministic = false` until plan 04
//! verifies bit-identical codegen.
//!
//! The cached artifact is [`Image::serialize`] output wrapped in a small envelope that
//! records its own blake3, so the bytes are integrity-checked on the way out of the
//! cache before they are trusted as native code (the `Image::deserialize` safety
//! contract). A hit therefore launches without any codegen; a miss compiles exactly
//! once, serializes that same image into the cache, and runs it.

use eo9_runtime::{EngineOptions, Image, compatibility_hash, new_engine};
use eo9_store::{CacheKey, CacheKeyParams, ObjectHash, Store};

use crate::cli::{Config, EXIT_SUCCESS, vlog};
use crate::source::{self, ProgramSource};

/// The target triple images are compiled for: the host triple (see `build.rs`).
pub const TARGET_TRIPLE: &str = env!("EO9_TARGET_TRIPLE");

/// The human-readable compiler pin for cache keys and cache metadata: the wasmtime
/// (Cranelift) version read from the workspace lockfile at build time (see `build.rs`).
/// The engine's runtime compatibility fingerprint is appended per engine in
/// [`compiler_version_for`].
pub const COMPILER_VERSION: &str = concat!("wasmtime-", env!("EO9_WASMTIME_VERSION"), " cranelift");

/// Whether codegen has been *verified* deterministic. The runtime's engine profile is
/// configured for determinism but bit-identical output has not been verified yet
/// (plan/04-runtime.md), so entries are keyed as non-deterministic per plan 06.
pub const COMPILER_DETERMINISTIC: bool = false;

/// Header line of a cached-image envelope: `eo9-cached-image 1 <blake3-of-payload>`.
const ARTIFACT_HEADER: &str = "eo9-cached-image 1";

/// Engine options derived from the CLI configuration.
pub fn engine_options(cfg: &Config) -> EngineOptions {
    EngineOptions {
        debug_info: cfg.debug_info,
    }
}

/// Canonical text rendering of the compile options, for the cache key. Everything not
/// listed here is pinned by the runtime's engine profile and therefore covered by the
/// engine identity string ([`compiler_version_for`]).
pub fn compile_opts_text(cfg: &Config) -> String {
    format!(
        "eo9-compile-opts 1\nengine-profile eo9-pinned\ndebug-info {}\n",
        cfg.debug_info
    )
}

/// The engine-identity half of the cache key: the human-readable wasmtime pin plus the
/// engine's compatibility fingerprint (`eo9_runtime::compatibility_hash`), which covers
/// the wasmtime build, the host target, and every compile-relevant engine setting. The
/// fingerprint is stable for a given toolchain build but not across Rust/wasmtime
/// upgrades — exactly the invalidation the cache wants, at the cost of spurious misses
/// after an upgrade that did not actually change codegen.
pub fn compiler_version_for(engine_fingerprint: u64) -> String {
    format!("{COMPILER_VERSION} compat-{engine_fingerprint:016x}")
}

/// The plan-06 cache key parameters for compiling one (unfused) component under an
/// engine with the given compatibility fingerprint.
pub fn cache_key_params(
    cfg: &Config,
    source: &ProgramSource,
    engine_fingerprint: u64,
) -> CacheKeyParams {
    CacheKeyParams {
        module_hashes: vec![source.hash],
        configure_constants: Vec::new(),
        compile_opts: compile_opts_text(cfg),
        target_triple: TARGET_TRIPLE.to_string(),
        compiler_version: compiler_version_for(engine_fingerprint),
        compiler_deterministic: COMPILER_DETERMINISTIC,
    }
}

/// An image ready to spawn, plus where it came from.
pub struct LoadedImage {
    pub image: Image,
    pub key: CacheKey,
    /// True when the image was deserialized from the compile cache (no codegen ran).
    pub from_cache: bool,
    /// True when the cache now holds this image (it was already there, or the insert on
    /// a miss succeeded).
    pub stored: bool,
}

/// Obtain a runnable [`Image`] for `source`, preferring the compile cache.
///
/// * **Hit:** the cached envelope is integrity-checked against its recorded content hash
///   and deserialized — no codegen runs. An entry that fails the check (or the engine's
///   compatibility check) is ignored with a warning and the source is compiled instead.
/// * **Miss:** the component is compiled exactly once; that same image is serialized,
///   sealed, inserted into the cache, and returned.
///
/// The cache is an optimization only: a broken, unreadable, or unwritable cache never
/// fails this function — every cache error degrades to a warning and the component is
/// simply compiled from source. The only genuine errors are engine creation and
/// compilation of the component itself.
pub fn load_image(
    cfg: &Config,
    store: &Store,
    source: &ProgramSource,
) -> Result<LoadedImage, String> {
    let engine = new_engine(&engine_options(cfg))
        .map_err(|err| format!("cannot create the engine: {err:#}"))?;
    let params = cache_key_params(cfg, source, compatibility_hash(&engine));
    let key = params.key();

    // A lookup failure (unreadable entry, corrupt metadata, or a read-only cache that
    // cannot take the use-count bump) is treated as a miss, never as a fatal error.
    let cached = match store.lookup_image(&key) {
        Ok(cached) => cached,
        Err(err) => {
            eprintln!(
                "eo9: warning: compile-cache lookup failed for key {key}: {err}; compiling from source"
            );
            None
        }
    };
    match cached {
        Some(entry) => {
            let launch = unseal_artifact(&entry.image).and_then(|artifact| {
                // SAFETY: the bytes were produced by `Image::serialize` on insert and have
                // just been verified against the content hash recorded alongside them in
                // the store (the deserialize trust contract).
                unsafe { Image::deserialize(&engine, artifact) }.map_err(|err| err.to_string())
            });
            match launch {
                Ok(image) => {
                    vlog!(
                        cfg,
                        "launched from cached image: key {key}, {} bytes, use-count {}",
                        entry.metadata.image_size,
                        entry.metadata.use_count
                    );
                    return Ok(LoadedImage {
                        image,
                        key,
                        from_cache: true,
                        stored: true,
                    });
                }
                Err(reason) => {
                    // Never trust a questionable entry with native code; fall back to
                    // compiling from the component bytes. (The entry stays in place — the
                    // store has no single-entry eviction — so warn unconditionally.)
                    eprintln!("eo9: warning: ignoring compile-cache entry {key}: {reason}");
                }
            }
        }
        None => vlog!(cfg, "compile cache miss: key {key}; compiling"),
    }

    // Compile once, then try to cache the very image we are about to run. A run must
    // not fail just because the cache could not be written (read-only store, full disk,
    // serialization trouble): those paths warn and carry on with the compiled image.
    let image = Image::compile(&engine, &source.bytes)
        .map_err(|err| format!("{}: {err}", source.origin))?;
    let stored = match image.serialize() {
        Ok(artifact) => match store.insert_image(&params, &seal_artifact(&artifact)) {
            Ok(_) => {
                vlog!(
                    cfg,
                    "compiled {}: cached image ({} bytes) under key {key}",
                    source.origin,
                    artifact.len()
                );
                true
            }
            Err(err) => {
                eprintln!(
                    "eo9: warning: compiled image could not be cached under key {key}: {err}"
                );
                false
            }
        },
        Err(err) => {
            eprintln!("eo9: warning: compiled image could not be serialized for caching: {err}");
            false
        }
    };
    Ok(LoadedImage {
        image,
        key,
        from_cache: false,
        stored,
    })
}

/// `eo9 compile <name-or-path>`: warm the compile cache for a program (a binary; the
/// cache holds closed binaries, so providers are rejected as not-a-binary).
pub fn cmd_compile(cfg: &Config, reference: &str) -> Result<u8, String> {
    let source = source::resolve_program(cfg, reference)?;
    let store = cfg.open_store()?;
    let loaded = load_image(cfg, &store, &source)?;
    if loaded.from_cache {
        println!("compile cache hit: {}", loaded.key);
    } else if loaded.stored {
        println!("compiled and cached: {}", loaded.key);
    } else {
        println!("compiled (not cached — see warning above): {}", loaded.key);
    }
    Ok(EXIT_SUCCESS)
}

/// Wrap a serialized image in an envelope recording its own blake3, so the cache can be
/// integrity-checked before its bytes are handed to `Image::deserialize`.
fn seal_artifact(artifact: &[u8]) -> Vec<u8> {
    let mut sealed = format!("{ARTIFACT_HEADER} {}\n", ObjectHash::of(artifact)).into_bytes();
    sealed.extend_from_slice(artifact);
    sealed
}

/// Check and strip a cached-image envelope, returning the serialized image bytes. The
/// error explains why the entry cannot be trusted.
fn unseal_artifact(sealed: &[u8]) -> Result<&[u8], String> {
    let newline = sealed
        .iter()
        .position(|byte| *byte == b'\n')
        .ok_or_else(|| "entry has no envelope header".to_string())?;
    let header = std::str::from_utf8(&sealed[..newline])
        .map_err(|_| "entry envelope header is not UTF-8".to_string())?;
    let recorded = header
        .strip_prefix(ARTIFACT_HEADER)
        .map(str::trim)
        .ok_or_else(|| format!("entry envelope header {header:?} is not {ARTIFACT_HEADER:?}"))?;
    let expected = ObjectHash::from_hex(recorded)
        .map_err(|err| format!("entry envelope carries an invalid content hash: {err}"))?;

    let payload = &sealed[newline + 1..];
    let actual = ObjectHash::of(payload);
    if actual != expected {
        return Err(format!(
            "image bytes do not match their recorded content hash (expected {expected}, found {actual})"
        ));
    }
    Ok(payload)
}

#[cfg(test)]
mod tests {
    use super::*;

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
        assert!(compiler_version_for(0xabcd).ends_with("compat-000000000000abcd"));
    }

    #[test]
    fn cache_key_tracks_its_inputs() {
        let cfg = Config::default();
        let source = source_of(b"component bytes");

        let base = cache_key_params(&cfg, &source, 1).key();
        assert_eq!(cache_key_params(&cfg, &source, 1).key(), base);

        // Different module bytes -> different key.
        let other = source_of(b"different component bytes");
        assert_ne!(cache_key_params(&cfg, &other, 1).key(), base);

        // Different compile options -> different key.
        let debug_cfg = Config {
            debug_info: true,
            ..Config::default()
        };
        assert_ne!(cache_key_params(&debug_cfg, &source, 1).key(), base);

        // Different engine fingerprint (wasmtime build / config) -> different key.
        assert_ne!(cache_key_params(&cfg, &source, 2).key(), base);
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

    #[test]
    fn sealed_artifacts_round_trip_and_detect_corruption() {
        let artifact = b"not really native code, but bytes all the same".to_vec();
        let sealed = seal_artifact(&artifact);
        assert_eq!(unseal_artifact(&sealed).unwrap(), artifact.as_slice());

        // Flipping a payload byte is caught by the recorded content hash.
        let mut corrupt = sealed.clone();
        let last = corrupt.len() - 1;
        corrupt[last] ^= 0x01;
        let err = unseal_artifact(&corrupt).unwrap_err();
        assert!(err.contains("content hash"), "unexpected error: {err}");

        // A foreign or truncated entry is rejected by the envelope header.
        assert!(unseal_artifact(b"").is_err());
        assert!(unseal_artifact(b"some other format\npayload").is_err());
        assert!(unseal_artifact(b"eo9-cached-image 1 nothex\npayload").is_err());
    }
}
