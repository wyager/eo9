//! The Eo9 module store and compilation cache.
//!
//! This crate implements the Nix-inspired, content-addressed module store described in
//! SPEC.md ("The module store and compilation cache"):
//!
//! * **Object store** — immutable objects keyed by their blake3 content hash, stored as
//!   read-only files under `<root>/objects/<hash>`. A module's identity *is* its content
//!   hash; adding the same bytes twice is a no-op.
//! * **Name resolution** — `manifests/` map bare dotted names (`browser`,
//!   `virtualfs.create`, `fs.memfs`) to object hashes; `profiles/` stack manifests, with
//!   later manifests shadowing earlier ones (the same override direction as the `&`
//!   operator). [`Store::resolve`] hands back the hash plus an immutable [`ObjectHandle`]
//!   that loading and compilation key on.
//! * **Compile cache** — compiled images keyed by a [`CacheKey`] derived from the ordered
//!   module hashes of the fused composition, the `configure` constants, the compile
//!   options, the target triple, the compiler version string, and a compiler-determinism
//!   flag. Entries live under `<root>/cache/<key>/` with usage metadata and are evicted
//!   by [`Store::gc`] under a size budget.
//!
//! The store root defaults to `~/.eo9/store` and is overridden by the `EO9_STORE`
//! environment variable; see [`Store::open_default`]. All on-disk formats are documented
//! in the modules that own them ([`manifest`] and [`cache`]) and in
//! `plan/06-store.md` § Decisions.

mod cache;
mod fsutil;
mod hash;
mod manifest;
mod name;
mod object;

use std::env;
use std::ffi::OsString;
use std::fmt;
use std::io;
use std::path::{Path, PathBuf};

pub use cache::{
    CacheEntry, CacheKey, CacheKeyParams, CachePolicy, CachedImage, GcReport, ImageMetadata,
};
pub use hash::ObjectHash;
pub use manifest::{DEFAULT_MANIFEST, DEFAULT_PROFILE, Manifest, Profile, Resolved};
pub use name::Name;
pub use object::ObjectHandle;

/// Name of the environment variable that overrides the default store root.
pub const STORE_ENV_VAR: &str = "EO9_STORE";

/// First line of the `version` marker file at the store root.
const STORE_VERSION_LINE: &str = "eo9-store 1";

/// A module store rooted at a directory on the host filesystem.
///
/// The layout under the root is:
///
/// ```text
/// <root>/
///   version                      # layout marker: "eo9-store 1"
///   objects/<blake3-hex>         # immutable, read-only content-addressed objects
///   manifests/<name>.manifest    # name -> hash maps (see `manifest`)
///   profiles/<name>.profile      # ordered manifest stacks (see `manifest`)
///   cache/<key-hex>/{image,meta} # compile-cache entries (see `cache`)
/// ```
#[derive(Debug, Clone)]
pub struct Store {
    root: PathBuf,
}

impl Store {
    /// Open (creating if necessary) the store rooted at `root`.
    ///
    /// Creates the layout directories and the `version` marker on first use and verifies
    /// the marker on subsequent opens, so a future layout change can never silently
    /// misread an old store.
    pub fn open(root: impl Into<PathBuf>) -> Result<Store, StoreError> {
        let root = root.into();
        let store = Store { root };
        for dir in [
            store.root.as_path(),
            &store.objects_dir(),
            &store.manifests_dir(),
            &store.profiles_dir(),
            &store.cache_dir(),
        ] {
            std::fs::create_dir_all(dir).map_err(|source| StoreError::io(dir, source))?;
        }
        store.check_or_write_version()?;
        Ok(store)
    }

    /// Open the default store: `$EO9_STORE` if set, otherwise `~/.eo9/store`.
    pub fn open_default() -> Result<Store, StoreError> {
        Store::open(default_root()?)
    }

    /// The directory this store is rooted at.
    pub fn root(&self) -> &Path {
        &self.root
    }

    pub(crate) fn objects_dir(&self) -> PathBuf {
        self.root.join("objects")
    }

    pub(crate) fn manifests_dir(&self) -> PathBuf {
        self.root.join("manifests")
    }

    pub(crate) fn profiles_dir(&self) -> PathBuf {
        self.root.join("profiles")
    }

    pub(crate) fn cache_dir(&self) -> PathBuf {
        self.root.join("cache")
    }

    fn check_or_write_version(&self) -> Result<(), StoreError> {
        let path = self.root.join("version");
        match std::fs::read_to_string(&path) {
            Ok(text) => {
                let found = text.lines().next().unwrap_or("").trim();
                if found == STORE_VERSION_LINE {
                    Ok(())
                } else {
                    Err(StoreError::Corrupt {
                        path,
                        line: 1,
                        reason: format!(
                            "unsupported store version marker {found:?} (expected {STORE_VERSION_LINE:?})"
                        ),
                    })
                }
            }
            Err(err) if err.kind() == io::ErrorKind::NotFound => {
                fsutil::write_atomic(&path, format!("{STORE_VERSION_LINE}\n").as_bytes(), false)
            }
            Err(source) => Err(StoreError::io(&path, source)),
        }
    }
}

/// The default store root: `$EO9_STORE` if set, otherwise `~/.eo9/store`.
pub fn default_root() -> Result<PathBuf, StoreError> {
    default_root_from(env::var_os(STORE_ENV_VAR), env::home_dir())
}

/// Pure helper behind [`default_root`], split out so the precedence rule is testable
/// without mutating the process environment.
fn default_root_from(
    env_store: Option<OsString>,
    home: Option<PathBuf>,
) -> Result<PathBuf, StoreError> {
    match env_store {
        Some(dir) if !dir.is_empty() => Ok(PathBuf::from(dir)),
        _ => match home {
            Some(home) if !home.as_os_str().is_empty() => Ok(home.join(".eo9").join("store")),
            _ => Err(StoreError::NoDefaultRoot),
        },
    }
}

/// Errors produced by the store.
#[derive(Debug)]
pub enum StoreError {
    /// An I/O operation on `path` failed.
    Io { path: PathBuf, source: io::Error },
    /// Neither `$EO9_STORE` nor a home directory is available to locate the default root.
    NoDefaultRoot,
    /// A string is not a valid bare dotted module name.
    InvalidName { input: String, reason: String },
    /// A string is not a valid blake3 hex digest.
    InvalidHash { input: String, reason: String },
    /// A manifest or profile file name is not a valid identifier.
    InvalidFileStem { input: String, reason: String },
    /// A metadata value cannot be represented in the on-disk format.
    InvalidMetadata { reason: String },
    /// The name does not resolve in the given profile.
    UnknownName { name: Name, profile: String },
    /// A profile refers to a manifest that does not exist.
    MissingManifest { profile: String, manifest: String },
    /// A hash is referenced (by a manifest or a caller) but the object is not in the store.
    MissingObject { hash: ObjectHash },
    /// An object's bytes no longer match its hash (the store has been tampered with).
    HashMismatch {
        path: PathBuf,
        expected: ObjectHash,
        actual: ObjectHash,
    },
    /// A store file (version marker, manifest, profile, or cache metadata) failed to parse.
    Corrupt {
        path: PathBuf,
        line: usize,
        reason: String,
    },
}

impl StoreError {
    pub(crate) fn io(path: impl Into<PathBuf>, source: io::Error) -> StoreError {
        StoreError::Io {
            path: path.into(),
            source,
        }
    }
}

impl fmt::Display for StoreError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            StoreError::Io { path, source } => {
                write!(f, "i/o error on {}: {source}", path.display())
            }
            StoreError::NoDefaultRoot => write!(
                f,
                "cannot determine the default store root: neither ${STORE_ENV_VAR} nor a home directory is set"
            ),
            StoreError::InvalidName { input, reason } => {
                write!(f, "invalid module name {input:?}: {reason}")
            }
            StoreError::InvalidHash { input, reason } => {
                write!(f, "invalid object hash {input:?}: {reason}")
            }
            StoreError::InvalidFileStem { input, reason } => {
                write!(f, "invalid manifest/profile name {input:?}: {reason}")
            }
            StoreError::InvalidMetadata { reason } => {
                write!(f, "invalid cache metadata: {reason}")
            }
            StoreError::UnknownName { name, profile } => {
                write!(f, "name {name} does not resolve in profile {profile:?}")
            }
            StoreError::MissingManifest { profile, manifest } => write!(
                f,
                "profile {profile:?} refers to manifest {manifest:?}, which does not exist"
            ),
            StoreError::MissingObject { hash } => {
                write!(f, "object {hash} is not present in the store")
            }
            StoreError::HashMismatch {
                path,
                expected,
                actual,
            } => write!(
                f,
                "object {} does not match its hash: expected {expected}, found {actual}",
                path.display()
            ),
            StoreError::Corrupt { path, line, reason } => {
                write!(
                    f,
                    "corrupt store file {} (line {line}): {reason}",
                    path.display()
                )
            }
        }
    }
}

impl std::error::Error for StoreError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            StoreError::Io { source, .. } => Some(source),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn env_var_overrides_home() {
        let root = default_root_from(
            Some(OsString::from("/tmp/elsewhere")),
            Some(PathBuf::from("/home/u")),
        )
        .unwrap();
        assert_eq!(root, PathBuf::from("/tmp/elsewhere"));
    }

    #[test]
    fn home_is_the_fallback() {
        let root = default_root_from(None, Some(PathBuf::from("/home/u"))).unwrap();
        assert_eq!(root, PathBuf::from("/home/u/.eo9/store"));
    }

    #[test]
    fn empty_env_var_is_ignored() {
        let root =
            default_root_from(Some(OsString::new()), Some(PathBuf::from("/home/u"))).unwrap();
        assert_eq!(root, PathBuf::from("/home/u/.eo9/store"));
    }

    #[test]
    fn no_env_and_no_home_is_an_error() {
        assert!(matches!(
            default_root_from(None, None),
            Err(StoreError::NoDefaultRoot)
        ));
    }
}
