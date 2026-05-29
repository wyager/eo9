//! Root provider for `eo9:disk` — a file-backed raw block device.
//!
//! A disk is a flat span of bytes addressed by offset; no filesystem semantics live
//! here. The backing store is any host path that supports positioned reads and writes —
//! a plain regular file or a block-device node. Both operations use the owned-buffer
//! round-trip and complete asynchronously on the provider's blocking pool via
//! `pread`/`pwrite`, so any number of operations can be in flight on one device without
//! shared seek state.
//!
//! The device size is captured from the backing file's metadata when the provider is
//! opened (or taken from the caller for `create`) and is fixed for the provider's life;
//! every operation must fall entirely inside `[0, size)` or it fails with
//! `out-of-range` before touching the file.
//!
//! Kill behavior: in-flight reads and writes are never aborted — they run to completion
//! on a pool thread (a write issued before a kill may still reach the backing file), the
//! completer receives the buffer back, and a dead caller's runtime drops it. Dropping
//! the provider (and the pool) drains already-submitted operations first.

use std::fs::{File, OpenOptions};
use std::io;
use std::os::unix::fs::FileExt;
use std::path::Path;
use std::sync::Arc;

use crate::buffer::OwnedBuffer;
use crate::completion::Completer;
use crate::pool::BlockingPool;

/// Successful read (WIT `read-result`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReadResult {
    /// Number of bytes read into the buffer, starting at its beginning.
    pub bytes_read: u64,
}

/// Successful write (WIT `write-result`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WriteResult {
    /// Number of bytes written from the buffer.
    pub bytes_written: u64,
}

/// Read failure (WIT `read-error`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReadError {
    /// The backing device is gone.
    NotFound,
    /// Any other host I/O failure.
    Io(String),
    /// The requested range does not lie entirely inside the device.
    OutOfRange,
}

/// Write failure (WIT `write-error`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WriteError {
    /// Any other host I/O failure.
    Io(String),
    /// The requested range does not lie entirely inside the device.
    OutOfRange,
    /// The device was opened read-only.
    ReadOnly,
}

/// Completion payload of `read`: the buffer comes back on success and error alike.
pub type ReadCompletion = (OwnedBuffer, Result<ReadResult, ReadError>);
/// Completion payload of `write`: the buffer comes back on success and error alike.
pub type WriteCompletion = (OwnedBuffer, Result<WriteResult, WriteError>);

/// The host trait mirroring the WIT `eo9:disk/disk` interface (minus `default`).
pub trait DiskHost: Send + Sync {
    /// Read `dst.len()` bytes starting at `offset` into `dst`.
    fn read(&self, offset: u64, dst: OwnedBuffer, complete: Completer<ReadCompletion>);
    /// Write the whole of `src` starting at `offset`.
    fn write(&self, offset: u64, src: OwnedBuffer, complete: Completer<WriteCompletion>);
}

/// The unix disk provider: one file-backed block device. Corresponds to the WIT
/// `disk-impl` root handle.
pub struct DiskProvider {
    file: Arc<File>,
    size: u64,
    read_only: bool,
    pool: Arc<BlockingPool>,
}

impl DiskProvider {
    /// Opens an existing backing file (or block-device node). The device size is the
    /// file's current length; for block-device nodes whose metadata reports a zero
    /// length, pass the real size via [`DiskProvider::open_with_size`].
    pub fn open(
        path: impl AsRef<Path>,
        read_only: bool,
        pool: Arc<BlockingPool>,
    ) -> io::Result<Self> {
        let file = OpenOptions::new()
            .read(true)
            .write(!read_only)
            .open(path.as_ref())?;
        let size = file.metadata()?.len();
        Ok(Self {
            file: Arc::new(file),
            size,
            read_only,
            pool,
        })
    }

    /// Opens an existing backing file with an explicitly given device size.
    pub fn open_with_size(
        path: impl AsRef<Path>,
        size: u64,
        read_only: bool,
        pool: Arc<BlockingPool>,
    ) -> io::Result<Self> {
        let file = OpenOptions::new()
            .read(true)
            .write(!read_only)
            .open(path.as_ref())?;
        Ok(Self {
            file: Arc::new(file),
            size,
            read_only,
            pool,
        })
    }

    /// Creates a new backing file of `size` bytes (zero-filled, sparse where the host
    /// filesystem supports it) and opens it read-write. Fails if the path exists.
    pub fn create(path: impl AsRef<Path>, size: u64, pool: Arc<BlockingPool>) -> io::Result<Self> {
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .open(path.as_ref())?;
        file.set_len(size)?;
        Ok(Self {
            file: Arc::new(file),
            size,
            read_only: false,
            pool,
        })
    }

    /// Device size in bytes, fixed at open time.
    pub fn size(&self) -> u64 {
        self.size
    }

    /// Whether the device rejects writes.
    pub fn read_only(&self) -> bool {
        self.read_only
    }

    /// The requested range must lie entirely inside the device.
    fn check_range(&self, offset: u64, len: u64) -> bool {
        offset.checked_add(len).is_some_and(|end| end <= self.size)
    }
}

impl DiskProvider {
    /// Read synchronously on the calling thread, completing eagerly.
    ///
    /// The async [`DiskHost`] path completes on a pool thread, which a *synchronous*
    /// consumer of the disk capability cannot wait for: the eofs provider component
    /// drives its block device from a synchronous engine and requires every disk call to
    /// have completed by the time the call returns (plan/14-eofs.md D15). Embedders that
    /// hand a disk to such a consumer use this eager form instead of [`DiskHost::read`].
    pub fn read_blocking(&self, offset: u64, mut dst: OwnedBuffer) -> ReadCompletion {
        if !self.check_range(offset, dst.len()) {
            return (dst, Err(ReadError::OutOfRange));
        }
        let result = match self.file.read_exact_at(dst.as_mut_slice(), offset) {
            Ok(()) => Ok(ReadResult {
                bytes_read: dst.len(),
            }),
            Err(err) => Err(io_to_read_error(&err)),
        };
        (dst, result)
    }

    /// Write synchronously on the calling thread, completing eagerly (see
    /// [`DiskProvider::read_blocking`]).
    pub fn write_blocking(&self, offset: u64, src: OwnedBuffer) -> WriteCompletion {
        if self.read_only {
            return (src, Err(WriteError::ReadOnly));
        }
        if !self.check_range(offset, src.len()) {
            return (src, Err(WriteError::OutOfRange));
        }
        let result = match self.file.write_all_at(src.as_slice(), offset) {
            Ok(()) => Ok(WriteResult {
                bytes_written: src.len(),
            }),
            Err(err) => Err(WriteError::Io(err.to_string())),
        };
        (src, result)
    }

    /// Make every completed write durable on the backing file (fsync), synchronously on
    /// the calling thread. A read-only device has nothing volatile and succeeds
    /// immediately.
    pub fn flush_blocking(&self) -> Result<(), WriteError> {
        if self.read_only {
            return Ok(());
        }
        self.file
            .sync_all()
            .map_err(|err| WriteError::Io(err.to_string()))
    }
}

impl DiskHost for DiskProvider {
    fn read(&self, offset: u64, mut dst: OwnedBuffer, complete: Completer<ReadCompletion>) {
        if !self.check_range(offset, dst.len()) {
            complete((dst, Err(ReadError::OutOfRange)));
            return;
        }
        let file = Arc::clone(&self.file);
        self.pool.submit(move || {
            let result = match file.read_exact_at(dst.as_mut_slice(), offset) {
                Ok(()) => Ok(ReadResult {
                    bytes_read: dst.len(),
                }),
                Err(err) => Err(io_to_read_error(&err)),
            };
            complete((dst, result));
        });
    }

    fn write(&self, offset: u64, src: OwnedBuffer, complete: Completer<WriteCompletion>) {
        if self.read_only {
            complete((src, Err(WriteError::ReadOnly)));
            return;
        }
        if !self.check_range(offset, src.len()) {
            complete((src, Err(WriteError::OutOfRange)));
            return;
        }
        let file = Arc::clone(&self.file);
        self.pool.submit(move || {
            let result = match file.write_all_at(src.as_slice(), offset) {
                Ok(()) => Ok(WriteResult {
                    bytes_written: src.len(),
                }),
                Err(err) => Err(WriteError::Io(err.to_string())),
            };
            complete((src, result));
        });
    }
}

fn io_to_read_error(err: &io::Error) -> ReadError {
    match err.kind() {
        io::ErrorKind::NotFound => ReadError::NotFound,
        // The range check passed against the size captured at open, so hitting EOF means
        // the backing file shrank underneath us.
        io::ErrorKind::UnexpectedEof => {
            ReadError::Io("backing file is shorter than the device size".to_owned())
        }
        _ => ReadError::Io(err.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::completion::completer;
    use crate::testutil::TempDir;
    use std::sync::mpsc;
    use std::time::Duration;

    const TIMEOUT: Duration = Duration::from_secs(10);

    fn read_blocking(disk: &DiskProvider, offset: u64, len: u64) -> ReadCompletion {
        let (tx, rx) = mpsc::channel();
        disk.read(
            offset,
            OwnedBuffer::new(len),
            completer(move |done| tx.send(done).unwrap()),
        );
        rx.recv_timeout(TIMEOUT).unwrap()
    }

    fn write_blocking(disk: &DiskProvider, offset: u64, bytes: &[u8]) -> WriteCompletion {
        let (tx, rx) = mpsc::channel();
        disk.write(
            offset,
            OwnedBuffer::from_vec(bytes.to_vec()),
            completer(move |done| tx.send(done).unwrap()),
        );
        rx.recv_timeout(TIMEOUT).unwrap()
    }

    #[test]
    fn create_write_read_round_trip() {
        let dir = TempDir::new();
        let pool = Arc::new(BlockingPool::new(2));
        let disk = DiskProvider::create(dir.path().join("disk.img"), 4096, pool).unwrap();
        assert_eq!(disk.size(), 4096);
        assert!(!disk.read_only());

        let (_, result) = write_blocking(&disk, 1000, b"block device");
        assert_eq!(result.unwrap(), WriteResult { bytes_written: 12 });

        let (buf, result) = read_blocking(&disk, 1000, 12);
        assert_eq!(result.unwrap(), ReadResult { bytes_read: 12 });
        assert_eq!(buf.as_slice(), b"block device");

        // Untouched regions read back as zeroes.
        let (buf, result) = read_blocking(&disk, 0, 8);
        assert_eq!(result.unwrap(), ReadResult { bytes_read: 8 });
        assert_eq!(buf.as_slice(), &[0u8; 8]);
    }

    #[test]
    fn reopening_an_existing_image_sees_previous_writes() {
        let dir = TempDir::new();
        let path = dir.path().join("disk.img");
        let pool = Arc::new(BlockingPool::new(2));
        {
            let disk = DiskProvider::create(&path, 512, Arc::clone(&pool)).unwrap();
            write_blocking(&disk, 100, b"persistent").1.unwrap();
        }
        let disk = DiskProvider::open(&path, false, pool).unwrap();
        assert_eq!(disk.size(), 512);
        let (buf, result) = read_blocking(&disk, 100, 10);
        assert_eq!(result.unwrap().bytes_read, 10);
        assert_eq!(buf.as_slice(), b"persistent");
    }

    #[test]
    fn out_of_range_operations_are_rejected_and_return_the_buffer() {
        let dir = TempDir::new();
        let pool = Arc::new(BlockingPool::new(1));
        let disk = DiskProvider::create(dir.path().join("disk.img"), 64, pool).unwrap();

        let (buf, result) = read_blocking(&disk, 60, 8);
        assert_eq!(result.unwrap_err(), ReadError::OutOfRange);
        assert_eq!(buf.len(), 8);

        let (buf, result) = write_blocking(&disk, u64::MAX, b"x");
        assert_eq!(result.unwrap_err(), WriteError::OutOfRange);
        assert_eq!(buf.len(), 1);

        // A zero-length operation at the very end of the device is in range.
        let (_, result) = read_blocking(&disk, 64, 0);
        assert_eq!(result.unwrap().bytes_read, 0);
    }

    #[test]
    fn read_only_devices_reject_writes_but_serve_reads() {
        let dir = TempDir::new();
        let path = dir.path().join("disk.img");
        let pool = Arc::new(BlockingPool::new(1));
        {
            let disk = DiskProvider::create(&path, 128, Arc::clone(&pool)).unwrap();
            write_blocking(&disk, 0, b"frozen").1.unwrap();
        }
        let disk = DiskProvider::open(&path, true, pool).unwrap();
        assert!(disk.read_only());
        let (buf, result) = write_blocking(&disk, 0, b"nope");
        assert_eq!(result.unwrap_err(), WriteError::ReadOnly);
        assert_eq!(buf.as_slice(), b"nope");
        let (buf, _) = read_blocking(&disk, 0, 6);
        assert_eq!(buf.as_slice(), b"frozen");
    }

    #[test]
    fn many_concurrent_operations_all_complete_correctly() {
        let dir = TempDir::new();
        let pool = Arc::new(BlockingPool::new(4));
        let disk =
            Arc::new(DiskProvider::create(dir.path().join("disk.img"), 256 * 64, pool).unwrap());

        // 256 concurrent writes to disjoint 64-byte blocks.
        let (tx, rx) = mpsc::channel();
        for block in 0..256u64 {
            let tx = tx.clone();
            disk.write(
                block * 64,
                OwnedBuffer::from_vec(vec![block as u8; 64]),
                completer(move |(_, result)| tx.send(result).unwrap()),
            );
        }
        drop(tx);
        for result in rx.iter() {
            assert_eq!(result.unwrap().bytes_written, 64);
        }

        // 256 concurrent reads verify every block.
        let (tx, rx) = mpsc::channel();
        for block in 0..256u64 {
            let tx = tx.clone();
            disk.read(
                block * 64,
                OwnedBuffer::new(64),
                completer(move |(buf, result)| tx.send((block, buf, result)).unwrap()),
            );
        }
        drop(tx);
        let mut completions = 0;
        for (block, buf, result) in rx.iter() {
            assert_eq!(result.unwrap().bytes_read, 64);
            assert_eq!(buf.as_slice(), &[block as u8; 64][..]);
            completions += 1;
        }
        assert_eq!(completions, 256);
    }

    #[test]
    fn create_refuses_to_clobber_an_existing_file() {
        let dir = TempDir::new();
        let path = dir.path().join("disk.img");
        let pool = Arc::new(BlockingPool::new(1));
        DiskProvider::create(&path, 16, Arc::clone(&pool)).unwrap();
        assert!(DiskProvider::create(&path, 16, pool).is_err());
    }
}
