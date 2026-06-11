//! Minimal FAT32 walking and same-size overwrite planning: the pure core of the stick
//! flasher (docs/board/usb-msd-plan.md §3.1).
//!
//! What it does: parse a FAT32 boot sector ([`Volume::open`]), find a file by 8.3
//! name in the root directory ([`Volume::find_root_file`]), walk its cluster chain
//! once — read-only, first FAT only, cycle-bounded ([`Volume::locate`]) — map byte
//! ranges to partition-relative LBAs ([`Volume::runs`]), and plan a **same-size**
//! in-place content overwrite ([`Volume::write_plan`]): the list of LBA+data writes
//! that replaces the file's bytes through its existing clusters, in chain order
//! (fragmentation is handled by construction — order comes from the chain, not from
//! contiguity assumptions).
//!
//! What it refuses to do, by design: allocate clusters, write the FAT (either copy),
//! write directory entries, or touch anything when the layout is not the one the
//! xtask-built stick guarantees (FAT32, 512-byte sectors, 8.3 names). Every such case
//! is a typed [`FatError`], never a write — the fixed-slot discipline (`cargo xtask
//! build-stick` pads `EO9.IMG` to a fixed slot and renders `BOOT.SCR` with
//! fixed-width fields) makes every legitimate rewrite same-size, so nothing else is
//! needed.
//!
//! All LBAs are relative to the start of the FAT32 partition (sector 0 = the boot
//! sector): the caller windows them — on the board that window is `disk.part`, in the
//! host tests it is the fixture image itself. I/O is the caller's: reads come through
//! the [`SectorRead`] callback, and the write side only *plans* ([`WriteOp`] lists) —
//! this crate never performs a write.

#![cfg_attr(not(test), no_std)]

extern crate alloc;

use alloc::vec::Vec;

/// The only sector size this crate speaks (what the stick, the BPB, and eo9:disk's
/// 512-byte world all use; anything else in a BPB is a typed refusal).
pub const SECTOR: usize = 512;

/// Device read callback: `lba` is partition-relative. The crate never asks for a
/// sector past the BPB's `total_sectors`.
pub trait SectorRead {
    type Error;
    fn read_sector(&mut self, lba: u64, out: &mut [u8; SECTOR]) -> Result<(), Self::Error>;
}

/// Why the filesystem (or the request) was refused. Typed, never a write — a foreign
/// layout must fail loudly before any byte moves (usb-msd-plan §8 risk 6).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FatError {
    /// The boot sector is not the FAT32 shape this crate supports; says which check.
    NotFat32 { why: &'static str },
    /// The requested name cannot be encoded as an 8.3 directory name.
    BadName,
    /// No root-directory entry carries that name.
    NotFound,
    /// The name matched a directory (or volume label), not a plain file.
    NotAFile,
    /// The chain ran into a free FAT entry — the directory and the FAT disagree.
    FreeClusterInChain { cluster: u32 },
    /// The chain ran into a bad-cluster mark or an out-of-range cluster number.
    BadClusterInChain { cluster: u32 },
    /// The chain is longer than the volume has clusters: a cycle (bounded walk —
    /// the loop-safe-exit discipline; a cyclic FAT must not hang the flasher).
    ChainCycle,
    /// The chain's capacity does not match the directory entry's file size.
    ChainSizeMismatch { chain_clusters: u32, file_size: u32 },
    /// A byte range reached past the end of the file.
    OutOfRange,
    /// `write_plan` was given content of a different size than the file: this crate
    /// only does same-size overwrites (the fixed-slot discipline).
    SizeMismatch { data: u64, file: u32 },
    /// `write_plan` on a file whose size is not a whole number of sectors: the tail
    /// write would need a read-modify-write this planner cannot do. The xtask-built
    /// slot files are MiB-sized; anything else is foreign.
    UnalignedSize { size: u32 },
}

/// A device error or a filesystem refusal.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Error<E> {
    Device(E),
    Fat(FatError),
}

impl<E> From<FatError> for Error<E> {
    fn from(err: FatError) -> Self {
        Error::Fat(err)
    }
}

/// A parsed FAT32 volume: the BPB fields the walk needs, validated.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Volume {
    sectors_per_cluster: u32,
    reserved_sectors: u32,
    fat_count: u32,
    fat_sectors: u32,
    root_cluster: u32,
    total_sectors: u32,
    cluster_count: u32,
}

/// A root-directory match: the file's first cluster and byte size.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RootFile {
    pub first_cluster: u32,
    pub size: u32,
}

/// A located file: its size and its full cluster chain, in file order. Produced by
/// [`Volume::locate`]; consumed by [`Volume::runs`] and [`Volume::write_plan`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileMap {
    pub size: u32,
    pub chain: Vec<u32>,
}

/// A contiguous run of file sectors on the device: `sectors` sectors at `lba`,
/// holding the file bytes starting at `file_offset` (sector-aligned).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Run {
    pub lba: u64,
    pub sectors: u32,
    pub file_offset: u64,
}

/// One planned write: `data` (a whole number of sectors) at `lba`. The caller
/// performs the I/O; in chain order the ops replace the file's content in place.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WriteOp<'d> {
    pub lba: u64,
    pub data: &'d [u8],
}

const FAT32_EOC: u32 = 0x0FFF_FFF8; // entries >= this end the chain
const FAT32_BAD: u32 = 0x0FFF_FFF7;
const FAT32_MASK: u32 = 0x0FFF_FFFF;
const DIR_ENTRY: usize = 32;
const ATTR_LONG_NAME: u8 = 0x0F;
const ATTR_DIRECTORY: u8 = 0x10;
const ATTR_VOLUME_ID: u8 = 0x08;

fn read_u16(bytes: &[u8], at: usize) -> u32 {
    u32::from(u16::from_le_bytes([bytes[at], bytes[at + 1]]))
}

fn read_u32(bytes: &[u8], at: usize) -> u32 {
    u32::from_le_bytes([bytes[at], bytes[at + 1], bytes[at + 2], bytes[at + 3]])
}

/// Encode `name` as the 11-byte 8.3 directory form (upcased, space-padded), e.g.
/// `"EO9.IMG"` → `b"EO9     IMG"`. Long names are deliberately unsupported: the
/// xtask-built stick guarantees 8.3.
pub fn name_to_83(name: &str) -> Result<[u8; 11], FatError> {
    let mut out = [b' '; 11];
    let mut dot = None;
    let bytes = name.as_bytes();
    for (i, &b) in bytes.iter().enumerate() {
        if b == b'.' {
            if dot.is_some() {
                return Err(FatError::BadName);
            }
            dot = Some(i);
        }
    }
    let (base, ext) = match dot {
        Some(at) => (&bytes[..at], &bytes[at + 1..]),
        None => (bytes, &[][..]),
    };
    if base.is_empty() || base.len() > 8 || ext.len() > 3 {
        return Err(FatError::BadName);
    }
    for (i, part) in [(0usize, base), (8usize, ext)] {
        for (j, &b) in part.iter().enumerate() {
            let up = b.to_ascii_uppercase();
            // The conservative 8.3 charset: letters, digits, and the punctuation FAT
            // allows that these sticks actually use. Anything exotic is a refusal.
            let ok = up.is_ascii_uppercase()
                || up.is_ascii_digit()
                || matches!(
                    up,
                    b'_' | b'-' | b'~' | b'!' | b'#' | b'$' | b'%' | b'&' | b'@'
                );
            if !ok {
                return Err(FatError::BadName);
            }
            out[i + j] = up;
        }
    }
    Ok(out)
}

impl Volume {
    /// Parse and validate a FAT32 boot sector. Every shortcut this crate takes is
    /// checked here, so a foreign layout dies as a typed refusal at open, not as a
    /// misread later.
    pub fn parse(boot: &[u8; SECTOR]) -> Result<Volume, FatError> {
        if boot[510] != 0x55 || boot[511] != 0xAA {
            return Err(FatError::NotFat32 {
                why: "missing 0x55AA boot signature",
            });
        }
        if read_u16(boot, 11) as usize != SECTOR {
            return Err(FatError::NotFat32 {
                why: "bytes per sector is not 512",
            });
        }
        let sectors_per_cluster = u32::from(boot[13]);
        if sectors_per_cluster == 0 || !sectors_per_cluster.is_power_of_two() {
            return Err(FatError::NotFat32 {
                why: "sectors per cluster is not a power of two",
            });
        }
        // The FAT32 discriminators (Microsoft FAT spec): FAT16's fields must be zero
        // and FAT32's must not.
        if read_u16(boot, 17) != 0 {
            return Err(FatError::NotFat32 {
                why: "root entry count is nonzero (FAT12/16 layout)",
            });
        }
        if read_u16(boot, 22) != 0 {
            return Err(FatError::NotFat32 {
                why: "16-bit FAT size is nonzero (FAT12/16 layout)",
            });
        }
        let fat_sectors = read_u32(boot, 36);
        if fat_sectors == 0 {
            return Err(FatError::NotFat32 {
                why: "32-bit FAT size is zero",
            });
        }
        let fat_count = u32::from(boot[16]);
        if fat_count == 0 || fat_count > 2 {
            return Err(FatError::NotFat32 {
                why: "FAT count is not 1 or 2",
            });
        }
        let reserved_sectors = read_u16(boot, 14);
        if reserved_sectors == 0 {
            return Err(FatError::NotFat32 {
                why: "zero reserved sectors",
            });
        }
        let total_sectors = match read_u16(boot, 19) {
            0 => read_u32(boot, 32),
            small => small,
        };
        let data_start = reserved_sectors + fat_count * fat_sectors;
        if total_sectors <= data_start {
            return Err(FatError::NotFat32 {
                why: "no data region after the FATs",
            });
        }
        let cluster_count = (total_sectors - data_start) / sectors_per_cluster;
        // Every chain entry must fit the first FAT (4 bytes per cluster): a FAT too
        // small for the data region would let the walk read past it.
        if (cluster_count + 2) > fat_sectors * (SECTOR as u32 / 4) {
            return Err(FatError::NotFat32 {
                why: "FAT too small for the data region",
            });
        }
        let root_cluster = read_u32(boot, 44) & FAT32_MASK;
        if root_cluster < 2 || root_cluster - 2 >= cluster_count {
            return Err(FatError::NotFat32 {
                why: "root cluster out of range",
            });
        }
        Ok(Volume {
            sectors_per_cluster,
            reserved_sectors,
            fat_count,
            fat_sectors,
            root_cluster,
            total_sectors,
            cluster_count,
        })
    }

    /// Read sector 0 through the callback and parse it.
    pub fn open<D: SectorRead>(dev: &mut D) -> Result<Volume, Error<D::Error>> {
        let mut boot = [0u8; SECTOR];
        dev.read_sector(0, &mut boot).map_err(Error::Device)?;
        Ok(Volume::parse(&boot)?)
    }

    /// Bytes per cluster.
    pub fn cluster_bytes(&self) -> u32 {
        self.sectors_per_cluster * SECTOR as u32
    }

    /// Clusters in the data region.
    pub fn cluster_count(&self) -> u32 {
        self.cluster_count
    }

    /// The first LBA of `cluster`'s data (partition-relative).
    pub fn cluster_lba(&self, cluster: u32) -> u64 {
        let data_start = self.reserved_sectors + self.fat_count * self.fat_sectors;
        u64::from(data_start) + u64::from(cluster - 2) * u64::from(self.sectors_per_cluster)
    }

    fn valid_cluster(&self, cluster: u32) -> bool {
        cluster >= 2 && cluster - 2 < self.cluster_count
    }

    /// The FAT entry for `cluster`, read from the FIRST FAT only (mirror FATs are
    /// never read and — like everything else here — never written). `cache` holds
    /// one FAT sector between calls so a chain walk reads each FAT sector once.
    fn fat_entry<D: SectorRead>(
        &self,
        dev: &mut D,
        cache: &mut FatCache,
        cluster: u32,
    ) -> Result<u32, Error<D::Error>> {
        let entries_per_sector = SECTOR as u32 / 4;
        let sector = u64::from(self.reserved_sectors) + u64::from(cluster / entries_per_sector);
        if cache.lba != Some(sector) {
            dev.read_sector(sector, &mut cache.bytes)
                .map_err(Error::Device)?;
            cache.lba = Some(sector);
        }
        let at = (cluster % entries_per_sector) as usize * 4;
        Ok(read_u32(&cache.bytes, at) & FAT32_MASK)
    }

    /// Find `name` (8.3) in the root directory. Deleted entries and long-name
    /// fragments are skipped; a directory or volume-label match is a typed refusal.
    pub fn find_root_file<D: SectorRead>(
        &self,
        dev: &mut D,
        name: &str,
    ) -> Result<RootFile, Error<D::Error>> {
        let want = name_to_83(name)?;
        let mut cache = FatCache::new();
        let mut cluster = self.root_cluster;
        let mut sector = [0u8; SECTOR];
        // The root directory is itself a cluster chain; bound it like any other.
        for _ in 0..=self.cluster_count {
            for s in 0..self.sectors_per_cluster {
                dev.read_sector(self.cluster_lba(cluster) + u64::from(s), &mut sector)
                    .map_err(Error::Device)?;
                for entry in sector.chunks_exact(DIR_ENTRY) {
                    match entry[0] {
                        0x00 => return Err(FatError::NotFound.into()), // end of directory
                        0xE5 => continue,                              // deleted
                        _ => {}
                    }
                    let attr = entry[11];
                    if attr & ATTR_LONG_NAME == ATTR_LONG_NAME {
                        continue; // long-name fragment; the 8.3 entry follows
                    }
                    if entry[..11] != want {
                        continue;
                    }
                    if attr & (ATTR_DIRECTORY | ATTR_VOLUME_ID) != 0 {
                        return Err(FatError::NotAFile.into());
                    }
                    let first_cluster = (read_u16(entry, 20) << 16) | read_u16(entry, 26);
                    return Ok(RootFile {
                        first_cluster: first_cluster & FAT32_MASK,
                        size: read_u32(entry, 28),
                    });
                }
            }
            let next = self.fat_entry(dev, &mut cache, cluster)?;
            if next >= FAT32_EOC {
                return Err(FatError::NotFound.into());
            }
            if next == 0 {
                return Err(FatError::FreeClusterInChain { cluster }.into());
            }
            if next == FAT32_BAD || !self.valid_cluster(next) {
                return Err(FatError::BadClusterInChain { cluster: next }.into());
            }
            cluster = next;
        }
        Err(FatError::ChainCycle.into())
    }

    /// Walk `file`'s cluster chain (first FAT only, cycle-bounded) and check it
    /// against the directory's size: the chain must hold exactly the clusters the
    /// size needs — anything else means the FAT and the directory disagree, and a
    /// flasher must not write through a filesystem that disagrees with itself.
    pub fn chain<D: SectorRead>(
        &self,
        dev: &mut D,
        file: RootFile,
    ) -> Result<FileMap, Error<D::Error>> {
        let cluster_bytes = u64::from(self.cluster_bytes());
        let needed = u64::from(file.size).div_ceil(cluster_bytes) as u32;
        let mut chain = Vec::with_capacity(needed as usize);
        if file.size == 0 || file.first_cluster == 0 {
            // A zero-size file legitimately has no chain; a zero first cluster with a
            // nonzero size is a disagreement.
            return if file.size == 0 && file.first_cluster == 0 {
                Ok(FileMap { size: 0, chain })
            } else {
                Err(FatError::ChainSizeMismatch {
                    chain_clusters: 0,
                    file_size: file.size,
                }
                .into())
            };
        }
        if !self.valid_cluster(file.first_cluster) {
            return Err(FatError::BadClusterInChain {
                cluster: file.first_cluster,
            }
            .into());
        }
        let mut cache = FatCache::new();
        let mut cluster = file.first_cluster;
        loop {
            chain.push(cluster);
            if chain.len() as u32 > self.cluster_count {
                return Err(FatError::ChainCycle.into());
            }
            let next = self.fat_entry(dev, &mut cache, cluster)?;
            if next >= FAT32_EOC {
                break;
            }
            if next == 0 {
                return Err(FatError::FreeClusterInChain { cluster }.into());
            }
            if next == FAT32_BAD || !self.valid_cluster(next) {
                return Err(FatError::BadClusterInChain { cluster: next }.into());
            }
            cluster = next;
        }
        if chain.len() as u32 != needed {
            return Err(FatError::ChainSizeMismatch {
                chain_clusters: chain.len() as u32,
                file_size: file.size,
            }
            .into());
        }
        Ok(FileMap {
            size: file.size,
            chain,
        })
    }

    /// Find `name` in the root directory and walk its chain: the one-call form the
    /// flasher uses.
    pub fn locate<D: SectorRead>(
        &self,
        dev: &mut D,
        name: &str,
    ) -> Result<FileMap, Error<D::Error>> {
        let file = self.find_root_file(dev, name)?;
        self.chain(dev, file)
    }

    /// Map the file byte range `[offset, offset + len)` to device sectors: merged
    /// contiguous [`Run`]s in file order, covering every sector that holds a byte of
    /// the range (sector-granular — the caller slices partial first/last sectors
    /// using each run's `file_offset`).
    pub fn runs(&self, map: &FileMap, offset: u64, len: u64) -> Result<Vec<Run>, FatError> {
        let end = offset.checked_add(len).ok_or(FatError::OutOfRange)?;
        if len == 0 || end > u64::from(map.size) {
            return Err(FatError::OutOfRange);
        }
        let spc = u64::from(self.sectors_per_cluster);
        let first_sector = offset / SECTOR as u64;
        let last_sector = (end - 1) / SECTOR as u64;
        let mut runs: Vec<Run> = Vec::new();
        for file_sector in first_sector..=last_sector {
            let cluster_index = (file_sector / spc) as usize;
            let within = file_sector % spc;
            let lba = self.cluster_lba(map.chain[cluster_index]) + within;
            match runs.last_mut() {
                Some(run) if run.lba + u64::from(run.sectors) == lba => run.sectors += 1,
                _ => runs.push(Run {
                    lba,
                    sectors: 1,
                    file_offset: file_sector * SECTOR as u64,
                }),
            }
        }
        Ok(runs)
    }

    /// Plan a same-size in-place overwrite of the whole file with `data`: one
    /// [`WriteOp`] per contiguous run, in chain order. Refused unless `data` is
    /// exactly the file's size and that size is a whole number of sectors (the
    /// fixed-slot discipline guarantees both; anything else would need the FAT or
    /// directory writes this crate refuses to have).
    pub fn write_plan<'d>(
        &self,
        map: &FileMap,
        data: &'d [u8],
    ) -> Result<Vec<WriteOp<'d>>, FatError> {
        if data.len() as u64 != u64::from(map.size) {
            return Err(FatError::SizeMismatch {
                data: data.len() as u64,
                file: map.size,
            });
        }
        if !(map.size as usize).is_multiple_of(SECTOR) {
            return Err(FatError::UnalignedSize { size: map.size });
        }
        let runs = self.runs(map, 0, u64::from(map.size))?;
        Ok(runs
            .iter()
            .map(|run| WriteOp {
                lba: run.lba,
                data: &data[run.file_offset as usize
                    ..run.file_offset as usize + run.sectors as usize * SECTOR],
            })
            .collect())
    }
}

/// One cached FAT sector (the chain walk touches FAT sectors in order, so a single
/// sector of cache turns the walk into one read per FAT sector).
struct FatCache {
    lba: Option<u64>,
    bytes: [u8; SECTOR],
}

impl FatCache {
    fn new() -> Self {
        FatCache {
            lba: None,
            bytes: [0u8; SECTOR],
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn name_encoding() {
        assert_eq!(name_to_83("EO9.IMG").unwrap(), *b"EO9     IMG");
        assert_eq!(name_to_83("boot.scr").unwrap(), *b"BOOT    SCR");
        assert_eq!(name_to_83("BOOTARGS.TXT").unwrap(), *b"BOOTARGSTXT");
        assert_eq!(name_to_83("NOEXT").unwrap(), *b"NOEXT      ");
        assert_eq!(name_to_83("TOOLONGNAME.TXT"), Err(FatError::BadName));
        assert_eq!(name_to_83("A.LONG"), Err(FatError::BadName));
        assert_eq!(name_to_83("TWO.DOT.S"), Err(FatError::BadName));
        assert_eq!(name_to_83(".IMG"), Err(FatError::BadName));
        assert_eq!(name_to_83("SP ACE.TXT"), Err(FatError::BadName));
    }

    #[test]
    fn parse_refuses_garbage() {
        let zeros = [0u8; SECTOR];
        assert_eq!(
            Volume::parse(&zeros),
            Err(FatError::NotFat32 {
                why: "missing 0x55AA boot signature"
            })
        );
    }
}
