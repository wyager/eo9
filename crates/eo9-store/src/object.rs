//! The content-addressed object store: `objects/<blake3-hex>`, immutable once written.

use std::fs::{self, File};
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

use crate::fsutil;
use crate::{ObjectHash, Store, StoreError};

impl Store {
    /// Add `bytes` to the object store and return their content hash.
    ///
    /// Adding is idempotent: if the object is already present nothing is written. New
    /// objects appear atomically (temporary file + rename) and are made read-only, so a
    /// stored object is immutable from the moment it becomes visible.
    pub fn add(&self, bytes: &[u8]) -> Result<ObjectHash, StoreError> {
        let hash = ObjectHash::of(bytes);
        let path = self.object_path(&hash);
        if path.exists() {
            return Ok(hash);
        }
        fsutil::write_atomic(&path, bytes, true)?;
        Ok(hash)
    }

    /// Read the file at `path` and add its contents to the object store.
    pub fn add_file(&self, path: impl AsRef<Path>) -> Result<ObjectHash, StoreError> {
        let path = path.as_ref();
        let bytes = fs::read(path).map_err(|source| StoreError::io(path, source))?;
        self.add(&bytes)
    }

    /// Whether the object with this hash is present in the store.
    pub fn contains(&self, hash: &ObjectHash) -> bool {
        self.object_path(hash).exists()
    }

    /// The path the object with this hash lives at (whether or not it is present).
    pub fn object_path(&self, hash: &ObjectHash) -> PathBuf {
        self.objects_dir().join(hash.to_hex())
    }

    /// Open the object with this hash, yielding an immutable [`ObjectHandle`].
    pub fn open_object(&self, hash: &ObjectHash) -> Result<ObjectHandle, StoreError> {
        let path = self.object_path(hash);
        let file = File::open(&path).map_err(|source| {
            if source.kind() == std::io::ErrorKind::NotFound {
                StoreError::MissingObject { hash: *hash }
            } else {
                StoreError::io(&path, source)
            }
        })?;
        Ok(ObjectHandle {
            hash: *hash,
            path,
            file,
        })
    }

    /// Read an object's bytes, verifying them against the hash they are stored under.
    pub fn read_object(&self, hash: &ObjectHash) -> Result<Vec<u8>, StoreError> {
        let handle = self.open_object(hash)?;
        let bytes = handle.bytes()?;
        handle.check(&bytes)?;
        Ok(bytes)
    }

    /// List every object hash currently in the store, in ascending hash order.
    pub fn objects(&self) -> Result<Vec<ObjectHash>, StoreError> {
        let dir = self.objects_dir();
        let entries = fs::read_dir(&dir).map_err(|source| StoreError::io(&dir, source))?;
        let mut hashes = Vec::new();
        for entry in entries {
            let entry = entry.map_err(|source| StoreError::io(&dir, source))?;
            let file_name = entry.file_name();
            let Some(name) = file_name.to_str() else {
                continue;
            };
            if name.starts_with(fsutil::TMP_PREFIX) {
                continue;
            }
            hashes.push(ObjectHash::from_hex(name).map_err(|_| StoreError::Corrupt {
                path: entry.path(),
                line: 0,
                reason: "object file name is not a blake3 hex digest".to_owned(),
            })?);
        }
        hashes.sort_unstable();
        Ok(hashes)
    }
}

/// An immutable handle to a stored object: its content hash, its path in the store, and
/// an open read-only file.
///
/// This is the usermode realization of the spec's "opening a file for execution yields an
/// immutable handle": the hash gives a stable content identity for compile caching and
/// signatures, and the open file refers to the store's write-once, read-only object, so
/// the bytes read through the handle are the bytes that were hashed.
#[derive(Debug)]
pub struct ObjectHandle {
    hash: ObjectHash,
    path: PathBuf,
    file: File,
}

impl ObjectHandle {
    /// The content hash this handle refers to.
    pub fn hash(&self) -> &ObjectHash {
        &self.hash
    }

    /// The object's path inside the store.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// The open read-only file.
    pub fn file(&self) -> &File {
        &self.file
    }

    /// The object's size in bytes.
    pub fn size(&self) -> Result<u64, StoreError> {
        let metadata = self
            .file
            .metadata()
            .map_err(|source| StoreError::io(&self.path, source))?;
        Ok(metadata.len())
    }

    /// Read the whole object through the open file.
    pub fn bytes(&self) -> Result<Vec<u8>, StoreError> {
        let err = |source| StoreError::io(&self.path, source);
        let mut file = self.file.try_clone().map_err(err)?;
        file.seek(SeekFrom::Start(0)).map_err(err)?;
        let mut bytes = Vec::new();
        file.read_to_end(&mut bytes).map_err(err)?;
        Ok(bytes)
    }

    /// Re-hash the object and verify it still matches the hash it is stored under.
    pub fn verify(&self) -> Result<(), StoreError> {
        let bytes = self.bytes()?;
        self.check(&bytes)
    }

    pub(crate) fn check(&self, bytes: &[u8]) -> Result<(), StoreError> {
        let actual = ObjectHash::of(bytes);
        if actual == self.hash {
            Ok(())
        } else {
            Err(StoreError::HashMismatch {
                path: self.path.clone(),
                expected: self.hash,
                actual,
            })
        }
    }
}
