//! The deterministic, hash-keyed compilation cache.
//!
//! A compiled image is cached under a [`CacheKey`]: the blake3 hash of everything the
//! artifact depends on — the ordered content hashes of every module in the fused
//! composition, the `configure` constants baked in at compose time, the compile options,
//! the target triple, the compiler version string, and whether the compiler has been
//! verified deterministic (see SPEC.md "The module store and compilation cache" and the
//! determinism note in `plan/06-store.md`).
//!
//! # On-disk format (version 1)
//!
//! Each entry is a directory `cache/<key-hex>/` holding exactly two files:
//!
//! * `image` — the compiled image bytes, as handed to [`Store::insert_image`].
//! * `meta` — line-oriented usage and provenance metadata:
//!
//! ```text
//! eo9-image-meta 1
//! created 1748102400
//! last-used 1748102400
//! use-count 3
//! image-size 123456
//! target aarch64-apple-darwin
//! compiler wasmtime-45.0.0 cranelift
//! deterministic false
//! module 5b3c…64-hex…9a
//! module 77aa…64-hex…01
//! ```
//!
//! Keys are single tokens; the value is the rest of the line (so `target` and `compiler`
//! may contain spaces, but never newlines). `module` lines repeat, in composition order.
//! Entries are created atomically (a temporary directory renamed into place);
//! [`Store::lookup_image`] bumps `use-count` and `last-used` by rewriting `meta`
//! atomically.
//!
//! # Eviction
//!
//! [`Store::gc`] enforces a size budget over the summed `image` sizes. When over budget,
//! entries are evicted in ascending `(use-count, last-used, key)` order — the least
//! frequently used go first, least recently used among equals — until the cache fits.
//! This is the LRU/MFU blend the spec asks for, in its simplest deterministic form.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::str::FromStr;

use crate::{ObjectHash, Store, StoreError, fsutil, hash};

const META_HEADER: &str = "eo9-image-meta 1";
const META_FILE: &str = "meta";
const IMAGE_FILE: &str = "image";

/// Domain-separation context for cache-key derivation. Changing the key encoding must
/// change this string, so old and new keys can never collide.
const KEY_CONTEXT: &str = "eo9-store compile-cache key v1";

/// Everything a compiled image depends on; hashing these yields the [`CacheKey`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CacheKeyParams {
    /// Content hashes of every module in the fused composition, in composition order.
    /// Order matters: re-association changes meaning, so it changes the key.
    pub module_hashes: Vec<ObjectHash>,
    /// `configure` constants baked in at compose time, as `(name, WAVE-encoded value)`
    /// pairs. They are canonicalized by a stable sort on name before hashing, so callers
    /// need not agree on an order.
    pub configure_constants: Vec<(String, String)>,
    /// Canonical text encoding of the compile options (e.g. the WAVE rendering of
    /// `eo9:exec/compile.compile-opts`).
    pub compile_opts: String,
    /// The target triple the image was compiled for.
    pub target_triple: String,
    /// The compiler version string (e.g. `wasmtime-45.0.0 cranelift`).
    pub compiler_version: String,
    /// Whether the compiler has been *verified* deterministic (plan 04). Until then this
    /// is `false`, which keys such entries separately so nothing is silently wrong when
    /// determinism is later verified and the flag flips.
    pub compiler_deterministic: bool,
}

impl CacheKeyParams {
    /// Derive the cache key for these parameters.
    pub fn key(&self) -> CacheKey {
        let mut hasher = blake3::Hasher::new_derive_key(KEY_CONTEXT);

        hash_u64(&mut hasher, self.module_hashes.len() as u64);
        for module in &self.module_hashes {
            hasher.update(module.as_bytes());
        }

        let mut constants: Vec<&(String, String)> = self.configure_constants.iter().collect();
        constants.sort_by_key(|(name, _)| name);
        hash_u64(&mut hasher, constants.len() as u64);
        for (name, value) in constants {
            hash_str(&mut hasher, name);
            hash_str(&mut hasher, value);
        }

        hash_str(&mut hasher, &self.compile_opts);
        hash_str(&mut hasher, &self.target_triple);
        hash_str(&mut hasher, &self.compiler_version);
        hasher.update(&[u8::from(self.compiler_deterministic)]);

        CacheKey(*hasher.finalize().as_bytes())
    }
}

fn hash_u64(hasher: &mut blake3::Hasher, value: u64) {
    hasher.update(&value.to_le_bytes());
}

/// Length-prefixed string framing, so adjacent fields can never be confused.
fn hash_str(hasher: &mut blake3::Hasher, value: &str) {
    hash_u64(hasher, value.len() as u64);
    hasher.update(value.as_bytes());
}

/// A compile-cache key: the blake3 digest of a [`CacheKeyParams`].
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct CacheKey([u8; 32]);

impl CacheKey {
    /// The raw 32-byte digest.
    pub fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }

    /// Lowercase hex encoding (64 characters); this is the on-disk entry directory name.
    pub fn to_hex(self) -> String {
        hash::encode_hex(&self.0)
    }

    /// Parse a 64-character lowercase hex key.
    pub fn from_hex(input: &str) -> Result<CacheKey, StoreError> {
        hash::decode_hex(input)
            .map(CacheKey)
            .map_err(|reason| StoreError::InvalidHash {
                input: input.to_owned(),
                reason,
            })
    }
}

impl std::fmt::Display for CacheKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.to_hex())
    }
}

impl std::fmt::Debug for CacheKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "CacheKey({self})")
    }
}

impl FromStr for CacheKey {
    type Err = StoreError;

    fn from_str(s: &str) -> Result<CacheKey, StoreError> {
        CacheKey::from_hex(s)
    }
}

/// Usage and provenance metadata stored alongside a cached image.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImageMetadata {
    /// When the entry was inserted (seconds since the Unix epoch).
    pub created: u64,
    /// When the entry was last returned by a lookup (seconds since the Unix epoch).
    pub last_used: u64,
    /// How many times the entry has been returned by a lookup (1 at insertion).
    pub use_count: u64,
    /// Size of the image file in bytes.
    pub image_size: u64,
    /// The target triple the image was compiled for.
    pub target_triple: String,
    /// The compiler version string.
    pub compiler_version: String,
    /// Whether the compiler was verified deterministic when the entry was inserted.
    pub compiler_deterministic: bool,
    /// Content hashes of the composition's modules, in composition order.
    pub module_hashes: Vec<ObjectHash>,
}

impl ImageMetadata {
    fn parse(text: &str, path: &Path) -> Result<ImageMetadata, StoreError> {
        let corrupt = |line: usize, reason: String| StoreError::Corrupt {
            path: path.to_owned(),
            line,
            reason,
        };
        let mut lines = text.lines().enumerate().map(|(i, l)| (i + 1, l.trim()));
        match lines.next() {
            Some((_, line)) if line == META_HEADER => {}
            other => {
                return Err(corrupt(
                    other.map_or(1, |(n, _)| n),
                    format!("expected header {META_HEADER:?}"),
                ));
            }
        }

        let mut created = None;
        let mut last_used = None;
        let mut use_count = None;
        let mut image_size = None;
        let mut target_triple = None;
        let mut compiler_version = None;
        let mut compiler_deterministic = None;
        let mut module_hashes = Vec::new();

        for (line_number, line) in lines {
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let (key, value) = line.split_once(char::is_whitespace).unwrap_or((line, ""));
            let value = value.trim();
            let parse_u64 = |value: &str| {
                value
                    .parse::<u64>()
                    .map_err(|err| corrupt(line_number, format!("expected an integer: {err}")))
            };
            match key {
                "created" => created = Some(parse_u64(value)?),
                "last-used" => last_used = Some(parse_u64(value)?),
                "use-count" => use_count = Some(parse_u64(value)?),
                "image-size" => image_size = Some(parse_u64(value)?),
                "target" => target_triple = Some(value.to_owned()),
                "compiler" => compiler_version = Some(value.to_owned()),
                "deterministic" => {
                    compiler_deterministic = Some(match value {
                        "true" => true,
                        "false" => false,
                        other => {
                            return Err(corrupt(
                                line_number,
                                format!("expected `true` or `false`, found {other:?}"),
                            ));
                        }
                    });
                }
                "module" => module_hashes.push(
                    ObjectHash::from_hex(value)
                        .map_err(|err| corrupt(line_number, err.to_string()))?,
                ),
                other => {
                    return Err(corrupt(line_number, format!("unknown field {other:?}")));
                }
            }
        }

        let missing = |field: &str| corrupt(0, format!("missing field {field:?}"));
        Ok(ImageMetadata {
            created: created.ok_or_else(|| missing("created"))?,
            last_used: last_used.ok_or_else(|| missing("last-used"))?,
            use_count: use_count.ok_or_else(|| missing("use-count"))?,
            image_size: image_size.ok_or_else(|| missing("image-size"))?,
            target_triple: target_triple.ok_or_else(|| missing("target"))?,
            compiler_version: compiler_version.ok_or_else(|| missing("compiler"))?,
            compiler_deterministic: compiler_deterministic
                .ok_or_else(|| missing("deterministic"))?,
            module_hashes,
        })
    }

    fn to_text(&self) -> Result<String, StoreError> {
        for (field, value) in [
            ("target", &self.target_triple),
            ("compiler", &self.compiler_version),
        ] {
            if value.contains('\n') || value.contains('\r') {
                return Err(StoreError::InvalidMetadata {
                    reason: format!("the {field} string must not contain newlines: {value:?}"),
                });
            }
        }
        let mut out = format!(
            "{META_HEADER}\n\
             created {}\n\
             last-used {}\n\
             use-count {}\n\
             image-size {}\n\
             target {}\n\
             compiler {}\n\
             deterministic {}\n",
            self.created,
            self.last_used,
            self.use_count,
            self.image_size,
            self.target_triple,
            self.compiler_version,
            self.compiler_deterministic,
        );
        for module in &self.module_hashes {
            out.push_str("module ");
            out.push_str(&module.to_hex());
            out.push('\n');
        }
        Ok(out)
    }
}

/// A cache entry as listed by [`Store::cache_entries`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CacheEntry {
    /// The entry's key.
    pub key: CacheKey,
    /// The entry's metadata.
    pub metadata: ImageMetadata,
}

/// A cache hit: the image bytes plus the entry's metadata (after the lookup's bump).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CachedImage {
    /// The entry's key.
    pub key: CacheKey,
    /// The compiled image bytes.
    pub image: Vec<u8>,
    /// The entry's metadata.
    pub metadata: ImageMetadata,
}

/// The eviction policy for [`Store::gc`]: a size budget over summed image bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CachePolicy {
    /// Maximum total size of cached image bytes; entries are evicted until the cache
    /// fits. Metadata files are a few hundred bytes each and are not counted.
    pub max_bytes: u64,
}

impl CachePolicy {
    /// The provisional default budget: 4 GiB of compiled images.
    pub const DEFAULT_MAX_BYTES: u64 = 4 * 1024 * 1024 * 1024;

    /// Plan an eviction: given the current entries, return the keys to evict, in eviction
    /// order, so that the remaining entries fit the budget. Pure — this is the whole
    /// policy, and [`Store::gc`] is just "plan, then delete".
    ///
    /// Eviction order is ascending `(use-count, last-used, key)`: least frequently used
    /// first, least recently used among equals, key as a deterministic tiebreak.
    pub fn plan(&self, entries: &[CacheEntry]) -> Vec<CacheKey> {
        let mut total: u64 = entries.iter().map(|e| e.metadata.image_size).sum();
        if total <= self.max_bytes {
            return Vec::new();
        }
        let mut candidates: Vec<&CacheEntry> = entries.iter().collect();
        candidates.sort_by_key(|e| (e.metadata.use_count, e.metadata.last_used, e.key));
        let mut evict = Vec::new();
        for entry in candidates {
            if total <= self.max_bytes {
                break;
            }
            total = total.saturating_sub(entry.metadata.image_size);
            evict.push(entry.key);
        }
        evict
    }
}

impl Default for CachePolicy {
    fn default() -> CachePolicy {
        CachePolicy {
            max_bytes: CachePolicy::DEFAULT_MAX_BYTES,
        }
    }
}

/// What a [`Store::gc`] run did.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct GcReport {
    /// Number of cache entries before the run.
    pub entries_before: usize,
    /// Total image bytes before the run.
    pub bytes_before: u64,
    /// Keys evicted, in eviction order.
    pub evicted: Vec<CacheKey>,
    /// Image bytes freed by eviction.
    pub bytes_evicted: u64,
    /// Leftover temporary files/directories (from interrupted writes) that were removed.
    pub stale_tmp_removed: usize,
}

impl Store {
    /// Insert a compiled image. The key and provenance metadata are both derived from
    /// `params`, so they can never disagree. Returns the key the image is stored under.
    ///
    /// Inserting an already-present key leaves the existing entry in place (with
    /// deterministic compilation both images are identical by construction).
    pub fn insert_image(
        &self,
        params: &CacheKeyParams,
        image: &[u8],
    ) -> Result<CacheKey, StoreError> {
        let key = params.key();
        let entry_dir = self.cache_entry_dir(&key);
        if entry_dir.exists() {
            return Ok(key);
        }

        let now = fsutil::unix_now();
        let metadata = ImageMetadata {
            created: now,
            last_used: now,
            use_count: 1,
            image_size: image.len() as u64,
            target_triple: params.target_triple.clone(),
            compiler_version: params.compiler_version.clone(),
            compiler_deterministic: params.compiler_deterministic,
            module_hashes: params.module_hashes.clone(),
        };
        let meta_text = metadata.to_text()?;

        // Build the entry in a temporary directory, then rename it into place so a
        // half-written entry is never visible under its key.
        let tmp_dir = fsutil::tmp_sibling(&entry_dir);
        let build = build_entry(&tmp_dir, &entry_dir, image, &meta_text);
        if build.is_err() || tmp_dir.exists() {
            let _ = fs::remove_dir_all(&tmp_dir);
        }
        build?;
        Ok(key)
    }

    /// Look up a compiled image by key. A hit bumps the entry's `use-count` and
    /// `last-used` and returns the image bytes with the updated metadata.
    pub fn lookup_image(&self, key: &CacheKey) -> Result<Option<CachedImage>, StoreError> {
        let entry_dir = self.cache_entry_dir(key);
        let Some(mut metadata) = self.read_entry_metadata(&entry_dir)? else {
            return Ok(None);
        };
        let image_path = entry_dir.join(IMAGE_FILE);
        let image = fs::read(&image_path).map_err(|e| StoreError::io(&image_path, e))?;

        metadata.use_count = metadata.use_count.saturating_add(1);
        metadata.last_used = fsutil::unix_now();
        fsutil::write_atomic(
            &entry_dir.join(META_FILE),
            metadata.to_text()?.as_bytes(),
            false,
        )?;

        Ok(Some(CachedImage {
            key: *key,
            image,
            metadata,
        }))
    }

    /// List every cache entry (key + metadata), in ascending key order.
    pub fn cache_entries(&self) -> Result<Vec<CacheEntry>, StoreError> {
        let cache_dir = self.cache_dir();
        let entries = fs::read_dir(&cache_dir).map_err(|e| StoreError::io(&cache_dir, e))?;
        let mut out = Vec::new();
        for entry in entries {
            let entry = entry.map_err(|e| StoreError::io(&cache_dir, e))?;
            let file_name = entry.file_name();
            let Some(name) = file_name.to_str() else {
                continue;
            };
            if name.starts_with(fsutil::TMP_PREFIX) {
                continue;
            }
            let key = CacheKey::from_hex(name).map_err(|_| StoreError::Corrupt {
                path: entry.path(),
                line: 0,
                reason: "cache entry directory name is not a blake3 hex digest".to_owned(),
            })?;
            let Some(metadata) = self.read_entry_metadata(&entry.path())? else {
                continue;
            };
            out.push(CacheEntry { key, metadata });
        }
        out.sort_by_key(|entry| entry.key);
        Ok(out)
    }

    /// Total size of all cached image bytes.
    pub fn cache_size(&self) -> Result<u64, StoreError> {
        Ok(self
            .cache_entries()?
            .iter()
            .map(|entry| entry.metadata.image_size)
            .sum())
    }

    /// Enforce the eviction policy: evict entries (per [`CachePolicy::plan`]) until the
    /// cache fits the budget, and sweep stale temporary files left behind by interrupted
    /// writes (only ones older than an hour, so in-flight writes are never disturbed).
    pub fn gc(&self, policy: &CachePolicy) -> Result<GcReport, StoreError> {
        let entries = self.cache_entries()?;
        let mut report = GcReport {
            entries_before: entries.len(),
            bytes_before: entries.iter().map(|e| e.metadata.image_size).sum(),
            ..GcReport::default()
        };

        for key in policy.plan(&entries) {
            let entry_dir = self.cache_entry_dir(&key);
            let size = entries
                .iter()
                .find(|e| e.key == key)
                .map(|e| e.metadata.image_size)
                .unwrap_or_default();
            fs::remove_dir_all(&entry_dir).map_err(|e| StoreError::io(&entry_dir, e))?;
            report.bytes_evicted += size;
            report.evicted.push(key);
        }

        report.stale_tmp_removed = self.sweep_stale_tmp()?;
        Ok(report)
    }

    fn cache_entry_dir(&self, key: &CacheKey) -> PathBuf {
        self.cache_dir().join(key.to_hex())
    }

    /// Read an entry's metadata; `Ok(None)` if the entry does not exist.
    fn read_entry_metadata(&self, entry_dir: &Path) -> Result<Option<ImageMetadata>, StoreError> {
        let meta_path = entry_dir.join(META_FILE);
        match fs::read_to_string(&meta_path) {
            Ok(text) => Ok(Some(ImageMetadata::parse(&text, &meta_path)?)),
            Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(source) => Err(StoreError::io(&meta_path, source)),
        }
    }

    /// Remove stale `.tmp-*` leftovers (interrupted writes) older than an hour from the
    /// objects, manifests, profiles, and cache directories. Returns how many were removed.
    fn sweep_stale_tmp(&self) -> Result<usize, StoreError> {
        const STALE_AFTER_SECS: u64 = 60 * 60;
        let now = fsutil::unix_now();
        let mut removed = 0;
        for dir in [
            self.objects_dir(),
            self.manifests_dir(),
            self.profiles_dir(),
            self.cache_dir(),
        ] {
            let entries = fs::read_dir(&dir).map_err(|e| StoreError::io(&dir, e))?;
            for entry in entries {
                let entry = entry.map_err(|e| StoreError::io(&dir, e))?;
                if !entry
                    .file_name()
                    .to_string_lossy()
                    .starts_with(fsutil::TMP_PREFIX)
                {
                    continue;
                }
                let path = entry.path();
                let Ok(metadata) = fs::symlink_metadata(&path) else {
                    continue;
                };
                let age = metadata
                    .modified()
                    .ok()
                    .and_then(|m| m.duration_since(std::time::UNIX_EPOCH).ok())
                    .map(|m| now.saturating_sub(m.as_secs()));
                if age.is_some_and(|age| age >= STALE_AFTER_SECS) {
                    let result = if metadata.is_dir() {
                        fs::remove_dir_all(&path)
                    } else {
                        fs::remove_file(&path)
                    };
                    if result.is_ok() {
                        removed += 1;
                    }
                }
            }
        }
        Ok(removed)
    }
}

/// Write `image` and `meta` into `tmp_dir`, then rename it to `entry_dir`.
fn build_entry(
    tmp_dir: &Path,
    entry_dir: &Path,
    image: &[u8],
    meta_text: &str,
) -> Result<(), StoreError> {
    fs::create_dir_all(tmp_dir).map_err(|e| StoreError::io(tmp_dir, e))?;
    let image_path = tmp_dir.join(IMAGE_FILE);
    fs::write(&image_path, image).map_err(|e| StoreError::io(&image_path, e))?;
    let meta_path = tmp_dir.join(META_FILE);
    fs::write(&meta_path, meta_text.as_bytes()).map_err(|e| StoreError::io(&meta_path, e))?;
    match fs::rename(tmp_dir, entry_dir) {
        Ok(()) => Ok(()),
        // Lost a race with another inserter: the entry now exists, which is fine.
        Err(_) if entry_dir.exists() => Ok(()),
        Err(e) => Err(StoreError::io(entry_dir, e)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn params() -> CacheKeyParams {
        CacheKeyParams {
            module_hashes: vec![ObjectHash::of(b"env"), ObjectHash::of(b"app")],
            configure_constants: vec![
                ("dir".to_owned(), "\"/tmp/sandbox\"".to_owned()),
                ("seed".to_owned(), "42".to_owned()),
            ],
            compile_opts: "{debug-info: false, safepoint-maps: false}".to_owned(),
            target_triple: "aarch64-apple-darwin".to_owned(),
            compiler_version: "wasmtime-45.0.0".to_owned(),
            compiler_deterministic: false,
        }
    }

    #[test]
    fn identical_params_give_identical_keys() {
        assert_eq!(params().key(), params().key());
    }

    #[test]
    fn every_field_is_significant() {
        let base = params().key();

        let mut p = params();
        p.module_hashes.push(ObjectHash::of(b"extra"));
        assert_ne!(p.key(), base, "module set");

        let mut p = params();
        p.module_hashes.reverse();
        assert_ne!(p.key(), base, "module order");

        let mut p = params();
        p.configure_constants[1].1 = "43".to_owned();
        assert_ne!(p.key(), base, "configure constants");

        let mut p = params();
        p.compile_opts = "{debug-info: true, safepoint-maps: false}".to_owned();
        assert_ne!(p.key(), base, "compile opts");

        let mut p = params();
        p.target_triple = "riscv64gc-unknown-none-elf".to_owned();
        assert_ne!(p.key(), base, "target triple");

        let mut p = params();
        p.compiler_version = "wasmtime-46.0.0".to_owned();
        assert_ne!(p.key(), base, "compiler version");

        let mut p = params();
        p.compiler_deterministic = true;
        assert_ne!(p.key(), base, "determinism flag");
    }

    #[test]
    fn configure_constant_order_does_not_matter() {
        let mut reordered = params();
        reordered.configure_constants.reverse();
        assert_eq!(reordered.key(), params().key());
    }

    #[test]
    fn field_framing_is_unambiguous() {
        // Moving a character across a field boundary must change the key.
        let mut a = params();
        a.target_triple = "ab".to_owned();
        a.compiler_version = "c".to_owned();
        let mut b = params();
        b.target_triple = "a".to_owned();
        b.compiler_version = "bc".to_owned();
        assert_ne!(a.key(), b.key());
    }

    #[test]
    fn metadata_text_round_trips() {
        let metadata = ImageMetadata {
            created: 1_748_102_400,
            last_used: 1_748_102_500,
            use_count: 7,
            image_size: 12_345,
            target_triple: "aarch64-apple-darwin".to_owned(),
            compiler_version: "wasmtime-45.0.0 cranelift".to_owned(),
            compiler_deterministic: false,
            module_hashes: vec![ObjectHash::of(b"a"), ObjectHash::of(b"b")],
        };
        let text = metadata.to_text().unwrap();
        let parsed = ImageMetadata::parse(&text, Path::new("meta")).unwrap();
        assert_eq!(parsed, metadata);
    }

    #[test]
    fn metadata_rejects_newlines_and_bad_fields() {
        let mut metadata = ImageMetadata {
            created: 0,
            last_used: 0,
            use_count: 0,
            image_size: 0,
            target_triple: "a\nb".to_owned(),
            compiler_version: "c".to_owned(),
            compiler_deterministic: true,
            module_hashes: Vec::new(),
        };
        assert!(metadata.to_text().is_err());
        metadata.target_triple = "ok".to_owned();
        assert!(metadata.to_text().is_ok());

        assert!(ImageMetadata::parse("eo9-image-meta 1\ncreated x\n", Path::new("meta")).is_err());
        assert!(ImageMetadata::parse("not-a-header\n", Path::new("meta")).is_err());
        assert!(ImageMetadata::parse("eo9-image-meta 1\ncreated 1\n", Path::new("meta")).is_err());
    }

    fn entry(key_seed: &[u8], use_count: u64, last_used: u64, image_size: u64) -> CacheEntry {
        let mut p = params();
        p.module_hashes = vec![ObjectHash::of(key_seed)];
        CacheEntry {
            key: p.key(),
            metadata: ImageMetadata {
                created: 0,
                last_used,
                use_count,
                image_size,
                target_triple: "t".to_owned(),
                compiler_version: "c".to_owned(),
                compiler_deterministic: false,
                module_hashes: p.module_hashes,
            },
        }
    }

    #[test]
    fn under_budget_evicts_nothing() {
        let entries = vec![entry(b"a", 1, 10, 100), entry(b"b", 1, 20, 100)];
        let policy = CachePolicy { max_bytes: 200 };
        assert!(policy.plan(&entries).is_empty());
    }

    #[test]
    fn eviction_prefers_rarely_then_least_recently_used() {
        let rarely_old = entry(b"rarely-old", 1, 10, 100);
        let rarely_new = entry(b"rarely-new", 1, 99, 100);
        let often_old = entry(b"often-old", 50, 5, 100);
        let entries = vec![often_old.clone(), rarely_new.clone(), rarely_old.clone()];

        // Budget forces exactly one eviction: the least-used, least-recently-used entry.
        let policy = CachePolicy { max_bytes: 200 };
        assert_eq!(policy.plan(&entries), vec![rarely_old.key]);

        // A tighter budget evicts both rarely-used entries, oldest first, sparing the
        // frequently-used one.
        let policy = CachePolicy { max_bytes: 100 };
        assert_eq!(policy.plan(&entries), vec![rarely_old.key, rarely_new.key]);
    }
}
