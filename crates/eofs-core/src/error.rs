//! The filesystem error type.

use core::fmt;

use crate::device::DeviceError;

/// Errors returned by the eofs engine.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FsError {
    /// The underlying device failed.
    Device(DeviceError),
    /// The path (or snapshot name) does not exist.
    NotFound,
    /// The path (or snapshot name) already exists.
    AlreadyExists,
    /// A non-directory appeared where a directory was required.
    NotADirectory,
    /// A directory appeared where a file was required.
    IsADirectory,
    /// `remove` on a directory that still has entries.
    DirectoryNotEmpty,
    /// The path or name is not acceptable (empty component, `.`/`..`, embedded `/` or NUL,
    /// name too long, or an operation aimed at the root itself).
    InvalidPath,
    /// The device has no room for the allocation.
    NoSpace,
    /// A stored block's content does not match the blake3 hash in its block pointer.
    ChecksumMismatch,
    /// The on-disk structure is not a valid eofs image (bad magic, bad uberblock checksum,
    /// out-of-range pointer, malformed directory, ...). The string names the check that failed.
    Corrupt(&'static str),
    /// The format options are not acceptable. The string names the offending option.
    InvalidConfig(&'static str),
}

impl From<DeviceError> for FsError {
    fn from(err: DeviceError) -> FsError {
        FsError::Device(err)
    }
}

impl fmt::Display for FsError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            FsError::Device(err) => write!(f, "device error: {err}"),
            FsError::NotFound => write!(f, "not found"),
            FsError::AlreadyExists => write!(f, "already exists"),
            FsError::NotADirectory => write!(f, "not a directory"),
            FsError::IsADirectory => write!(f, "is a directory"),
            FsError::DirectoryNotEmpty => write!(f, "directory not empty"),
            FsError::InvalidPath => write!(f, "invalid path"),
            FsError::NoSpace => write!(f, "no space left on device"),
            FsError::ChecksumMismatch => write!(f, "block checksum mismatch"),
            FsError::Corrupt(what) => write!(f, "corrupt filesystem: {what}"),
            FsError::InvalidConfig(what) => write!(f, "invalid format options: {what}"),
        }
    }
}

impl core::error::Error for FsError {}
