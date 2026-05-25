//! eofs-core — Eo9's native filesystem, as a target-independent library.
//!
//! This crate implements the on-disk format and the read/write engine of `eofs`
//! (SPEC.md, "Filesystem API"): a copy-on-write, Merkle-hashed, snapshotting, block-
//! compressed filesystem over an abstract byte-addressed [`BlockDevice`]. It is
//! `no_std` + `alloc` and free of wasm, wasmtime, and OS types, so the same code serves
//! host-side tests and tooling, the milestone-2 guest provider component (which will bridge
//! `eo9:disk` to [`BlockDevice`] and `eo9:fs` to [`Eofs`]), and the bare-metal kernel.
//!
//! # Shape of the design
//!
//! * **Copy-on-write, never overwrite in place.** Every change writes new blocks; the old
//!   tree stays intact until a [`commit`](Eofs::commit) flips the root by writing the next
//!   uberblock into the slot the previous commit did not use. Crash consistency is by
//!   construction: a mount adopts the newest uberblock whose checksum is valid, so a torn
//!   or missing commit simply falls back to the previous transaction. There is no journal
//!   and no fsck; [`verify`](Eofs::verify) exists to check integrity, not to repair.
//! * **A Merkle tree throughout.** Every block pointer carries the blake3 hash of the block
//!   it points to, and indirect blocks / directories are themselves blocks full of pointers,
//!   so every node's hash covers its entire subtree, up to the uberblock. Reads check the
//!   hash of every block they touch; [`stat`](Eofs::stat) exposes each node's Merkle root.
//! * **Snapshots are retained roots.** [`snapshot_create`](Eofs::snapshot_create) records
//!   the current root in the snapshot table; old blocks stay reachable and readable through
//!   a [`SnapshotView`] no matter what happens to the live tree afterwards.
//! * **Compression on by default.** Data and metadata blocks are lz4-compressed unless that
//!   would not save space, in which case they are stored raw; every pointer carries a codec
//!   tag, so further codecs can be added without a format change.
//! * **Append-style allocation, deferred reclamation.** New blocks go at the allocation
//!   frontier (or into holes found by the manual [`gc`](Eofs::gc) walk); nothing reachable
//!   from any retained root is ever reused.
//! * **Deterministic.** No clocks, no randomness: the same operation sequence over the same
//!   configuration produces bit-identical images.
//!
//! The exact byte layout lives in `FORMAT.md` next to this crate; [`format`](mod@format) is
//! its code twin.
//!
//! # Example
//!
//! ```
//! use eofs_core::{Eofs, FormatOptions, MemDevice};
//!
//! let dev = MemDevice::new(1 << 20);
//! let mut fs = Eofs::format(dev, &FormatOptions::default()).unwrap();
//! fs.mkdir("/etc").unwrap();
//! fs.create_file("/etc/motd").unwrap();
//! fs.write("/etc/motd", 0, b"hello from eofs").unwrap();
//! fs.snapshot_create("clean").unwrap();
//! fs.commit().unwrap();
//!
//! let mut buf = [0u8; 15];
//! assert_eq!(fs.read("/etc/motd", 0, &mut buf).unwrap(), 15);
//! assert_eq!(&buf, b"hello from eofs");
//! assert!(fs.verify().unwrap().blocks > 0);
//!
//! // Remount from the raw image and read it back.
//! let dev = fs.unmount();
//! let fs = Eofs::mount(dev).unwrap();
//! assert_eq!(fs.list("/etc").unwrap(), vec!["motd".to_string()]);
//! ```

#![no_std]
#![forbid(unsafe_code)]

extern crate alloc;

mod device;
mod error;
pub mod format;
mod fs;
mod space;
mod tree;

pub use device::{BlockDevice, CutDevice, DeviceError, MemDevice};
pub use error::FsError;
pub use format::{Codec, NodeKind};
pub use fs::{
    Eofs, FormatOptions, GcReport, NodeStat, SnapshotInfo, SnapshotView, SpaceReport, VerifyReport,
};
