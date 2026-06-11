//! MBR partition-table parsing and validation — the pure core of `disk.part`.
//!
//! `disk.part` (guest/stubs/disk-part) is partition-table middleware: it imports
//! `eo9:disk/disk` (a whole device), parses the MBR on first use, and re-exports
//! `eo9:disk/disk` as a strict window over one partition. This crate is the part of
//! that job with byte arithmetic worth pinning on the host: sector decoding, the
//! extended-partition (EBR) chain walk, and the validation ladder. It never does I/O —
//! the chain walk is a pull-driven state machine ([`Walker`]) that *asks* the caller
//! for sectors by LBA and is fed their bytes, so the same code runs under the async
//! component reads and under plain host tests with byte fixtures.
//!
//! The posture is refuse-don't-guess (the disk.part plan sections in
//! docs/board/usb-msd-plan.md §2 and docs/board/sdcard-plan.md §B.3):
//!
//! * a missing boot signature, an entry past the device end, a zero-length non-empty
//!   entry, an entry starting at LBA 0, overlapping entries, a malformed or cyclic
//!   EBR chain — each is a typed [`TableError`], and the whole table is refused (no
//!   partial salvage: a table that lies once is not trusted twice);
//! * a GPT disk (any protective/hybrid `0xEE` entry) answers [`TableError::Gpt`] —
//!   v1 does not read GPT, and refusing is strictly better than misreading the
//!   protective MBR as one giant partition;
//! * the extended *container* is not selectable ([`SelectError::ExtendedContainer`]):
//!   its window would cover the EBR sectors, and the partition table must stay
//!   read-only through `disk.part` by construction, not by check. The same rule
//!   refuses tables where an EBR sector lies inside a logical partition's claimed
//!   span ([`TableError::EbrInsideLogical`]).
//!
//! Numbering is fdisk's: primaries are their MBR slot, 1–4, empty slots keep their
//! number reserved; logicals are 5, 6, … in chain order. All LBA arithmetic is in
//! 512-byte sectors — the unit the MBR format itself is defined in.

#![no_std]

extern crate alloc;

use alloc::vec::Vec;
use core::fmt;

/// The sector size the MBR format is defined in. The eo9:disk surface is
/// byte-addressed; every LBA in this crate converts at this factor.
pub const SECTOR_SIZE: u64 = 512;

/// Upper bound on the EBR chain length (cycle-independent runaway protection). Real
/// tools rarely create more than a handful of logicals; 64 is far beyond any honest
/// table and small enough that a hostile chain costs nothing.
pub const MAX_EBRS: usize = 64;

/// Offset of the four primary partition entries in the MBR (and of the two
/// meaningful entries in an EBR).
const ENTRIES_OFFSET: usize = 446;

/// One selectable partition: its fdisk number, where it starts, and how long it is
/// (both in 512-byte sectors, absolute from the start of the device).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Partition {
    /// fdisk numbering: 1–4 for primaries (the MBR slot), 5+ for logicals in chain
    /// order.
    pub number: u32,
    /// First sector of the partition's data, absolute.
    pub start_lba: u64,
    /// Length in sectors.
    pub sectors: u64,
}

impl Partition {
    /// First byte of the partition, absolute on the device.
    pub fn start_bytes(&self) -> u64 {
        self.start_lba * SECTOR_SIZE
    }

    /// Length of the partition in bytes.
    pub fn len_bytes(&self) -> u64 {
        self.sectors * SECTOR_SIZE
    }

    /// Whether `lba` falls inside this partition's span.
    fn contains(&self, lba: u64) -> bool {
        lba >= self.start_lba && lba < self.start_lba + self.sectors
    }

    /// Whether two spans intersect.
    fn overlaps(&self, other: &Partition) -> bool {
        self.start_lba < other.start_lba + other.sectors
            && other.start_lba < self.start_lba + self.sectors
    }
}

/// Why a partition table was refused. Every variant renders to the operator-facing
/// refusal text (the `disk.part: …` prefix is the component's job).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TableError {
    /// A fed sector was not exactly 512 bytes (an underlying short read).
    SectorSize { got: usize },
    /// Sector 0 does not end in the 0x55AA boot signature: not an MBR.
    MissingSignature,
    /// A protective/hybrid GPT entry (type 0xEE) is present: this is a GPT disk.
    Gpt,
    /// An EBR in the extended chain is missing its 0x55AA signature.
    EbrMissingSignature { lba: u64 },
    /// A non-empty entry claims zero sectors.
    ZeroLength { number: u32 },
    /// A non-empty entry starts at LBA 0 — its window would cover the MBR itself.
    StartsAtZero { number: u32 },
    /// An entry extends past the end of the device.
    OutOfBounds {
        number: u32,
        end_lba: u64,
        device_sectors: u64,
    },
    /// Two entries claim intersecting spans.
    Overlap { a: u32, b: u32 },
    /// More than one primary extended container (type 0x05/0x0F).
    TwoExtendedContainers,
    /// A logical partition's span leaves its extended container.
    LogicalOutsideContainer { number: u32 },
    /// A chain EBR lies outside the extended container.
    EbrOutsideContainer { lba: u64 },
    /// An EBR sector lies inside a logical partition's claimed span — the table
    /// would be writable through that partition's window.
    EbrInsideLogical { ebr_lba: u64, number: u32 },
    /// The EBR chain revisits a sector.
    ChainCycle { lba: u64 },
    /// The EBR chain exceeds [`MAX_EBRS`].
    ChainTooLong,
    /// An EBR's chain slot (entry 1) carries a type that is neither empty nor
    /// extended — a layout this parser does not understand and refuses to guess at.
    BadChainEntry { lba: u64, partition_type: u8 },
    /// [`Walker::feed`] was called when no sector was requested (caller bug).
    NotExpectingSector,
}

impl fmt::Display for TableError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            TableError::SectorSize { got } => {
                write!(f, "a table sector read returned {got} bytes instead of 512")
            }
            TableError::MissingSignature => {
                write!(f, "sector 0 has no 0x55AA boot signature (not an MBR)")
            }
            TableError::Gpt => write!(
                f,
                "GPT partition table not supported in v1 (protective 0xEE entry found); \
                 refusing rather than misreading"
            ),
            TableError::EbrMissingSignature { lba } => {
                write!(f, "the EBR at LBA {lba} has no 0x55AA signature")
            }
            TableError::ZeroLength { number } => {
                write!(f, "partition {number} is non-empty but zero sectors long")
            }
            TableError::StartsAtZero { number } => write!(
                f,
                "partition {number} starts at LBA 0 — its window would cover the MBR"
            ),
            TableError::OutOfBounds {
                number,
                end_lba,
                device_sectors,
            } => write!(
                f,
                "partition {number} ends at LBA {end_lba}, past the device's \
                 {device_sectors} sectors"
            ),
            TableError::Overlap { a, b } => {
                write!(f, "partitions {a} and {b} overlap")
            }
            TableError::TwoExtendedContainers => {
                write!(f, "more than one extended container entry")
            }
            TableError::LogicalOutsideContainer { number } => write!(
                f,
                "logical partition {number} leaves its extended container"
            ),
            TableError::EbrOutsideContainer { lba } => {
                write!(
                    f,
                    "the chained EBR at LBA {lba} lies outside the extended container"
                )
            }
            TableError::EbrInsideLogical { ebr_lba, number } => write!(
                f,
                "the EBR at LBA {ebr_lba} lies inside logical partition {number}'s span — \
                 the table would be writable through its window"
            ),
            TableError::ChainCycle { lba } => {
                write!(f, "the EBR chain revisits LBA {lba} (cycle)")
            }
            TableError::ChainTooLong => {
                write!(f, "the EBR chain exceeds {MAX_EBRS} links")
            }
            TableError::BadChainEntry {
                lba,
                partition_type,
            } => write!(
                f,
                "the EBR at LBA {lba} chains to a non-extended type {partition_type:#04x}; \
                 refusing rather than guessing"
            ),
            TableError::NotExpectingSector => {
                write!(f, "fed a sector when none was requested (walker misuse)")
            }
        }
    }
}

/// Why a partition number could not be selected from a parsed table.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SelectError {
    /// Partition numbers are 1-based (fdisk); 0 is never valid.
    Zero,
    /// The requested number is the extended container — its window would cover the
    /// EBR chain; logical partitions are the selectable ones.
    ExtendedContainer { number: u32 },
    /// The requested number is not in the table; carries the numbers that are.
    NotPresent { number: u32, present: Vec<u32> },
}

impl fmt::Display for SelectError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SelectError::Zero => {
                write!(
                    f,
                    "partition numbers are 1-based (fdisk numbering); 0 is not valid"
                )
            }
            SelectError::ExtendedContainer { number } => write!(
                f,
                "partition {number} is the extended container; select one of its logical \
                 partitions (numbered 5 and up)"
            ),
            SelectError::NotPresent { number, present } => {
                write!(
                    f,
                    "partition {number} is absent from the partition table (present:"
                )?;
                if present.is_empty() {
                    write!(f, " none")?;
                } else {
                    for (index, p) in present.iter().enumerate() {
                        if index > 0 {
                            write!(f, ",")?;
                        }
                        write!(f, " {p}")?;
                    }
                }
                write!(f, ")")
            }
        }
    }
}

/// A fully parsed and validated partition table.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PartitionTable {
    /// The selectable partitions: primaries in slot order, then logicals in chain
    /// order. The extended container is *not* here — see [`PartitionTable::select`].
    pub partitions: Vec<Partition>,
    /// The MBR slot number of the extended container, if the table has one.
    pub container: Option<u32>,
}

impl PartitionTable {
    /// Look up the partition with fdisk number `number`, with the typed refusals the
    /// component surfaces verbatim.
    pub fn select(&self, number: u32) -> Result<&Partition, SelectError> {
        if number == 0 {
            return Err(SelectError::Zero);
        }
        if self.container == Some(number) {
            return Err(SelectError::ExtendedContainer { number });
        }
        self.partitions
            .iter()
            .find(|p| p.number == number)
            .ok_or_else(|| SelectError::NotPresent {
                number,
                present: self.partitions.iter().map(|p| p.number).collect(),
            })
    }
}

/// What the walker needs next.
#[derive(Debug, PartialEq, Eq)]
pub enum Step {
    /// Read the 512-byte sector at this absolute LBA and pass it to [`Walker::feed`].
    Need { lba: u64 },
    /// The table is parsed and validated.
    Done(PartitionTable),
}

/// One raw 16-byte entry, decoded.
struct RawEntry {
    partition_type: u8,
    start_lba: u64,
    sectors: u64,
}

fn decode_entry(sector: &[u8], index: usize) -> RawEntry {
    let at = ENTRIES_OFFSET + index * 16;
    let entry = &sector[at..at + 16];
    RawEntry {
        partition_type: entry[4],
        start_lba: u64::from(u32::from_le_bytes([
            entry[8], entry[9], entry[10], entry[11],
        ])),
        sectors: u64::from(u32::from_le_bytes([
            entry[12], entry[13], entry[14], entry[15],
        ])),
    }
}

fn is_extended(partition_type: u8) -> bool {
    // 0x05 = CHS extended, 0x0F = LBA (Windows 95) extended. 0x85 (Linux extended)
    // is deliberately *not* followed: it is rare, tool support is inconsistent, and
    // refusing via BadChainEntry/unselected-slot beats guessing.
    partition_type == 0x05 || partition_type == 0x0F
}

fn has_signature(sector: &[u8]) -> bool {
    sector[510] == 0x55 && sector[511] == 0xAA
}

/// The extended container's span (absolute sectors).
#[derive(Clone, Copy)]
struct Container {
    start_lba: u64,
    sectors: u64,
}

/// The pull-driven MBR/EBR walk: construct with the device size, [`Walker::start`]
/// with sector 0's bytes, then service every [`Step::Need`] by reading that sector
/// and passing it to [`Walker::feed`], until [`Step::Done`].
pub struct Walker {
    device_sectors: u64,
    partitions: Vec<Partition>,
    container: Option<Container>,
    container_number: Option<u32>,
    /// EBR LBAs visited, in order (cycle detection + the EBR-inside-logical check).
    ebr_lbas: Vec<u64>,
    /// The LBA the last `Need` asked for (None = not expecting a sector).
    pending: Option<u64>,
    next_logical: u32,
}

impl Walker {
    /// A walker for a device of `device_size` **bytes** (the eo9:disk `size()` value);
    /// entries are validated against the device's whole-sector count.
    pub fn new(device_size: u64) -> Self {
        Walker {
            device_sectors: device_size / SECTOR_SIZE,
            partitions: Vec::new(),
            container: None,
            container_number: None,
            ebr_lbas: Vec::new(),
            pending: None,
            next_logical: 5,
        }
    }

    /// Parse sector 0 (the MBR). Returns the first [`Step`].
    pub fn start(&mut self, sector0: &[u8]) -> Result<Step, TableError> {
        if sector0.len() != SECTOR_SIZE as usize {
            return Err(TableError::SectorSize { got: sector0.len() });
        }
        if !has_signature(sector0) {
            return Err(TableError::MissingSignature);
        }

        // GPT first, before anything else is believed: any 0xEE entry (protective or
        // hybrid) means this is a GPT disk and the MBR is not the truth.
        for index in 0..4 {
            if decode_entry(sector0, index).partition_type == 0xEE {
                return Err(TableError::Gpt);
            }
        }

        for index in 0..4 {
            let entry = decode_entry(sector0, index);
            let number = (index + 1) as u32;
            if entry.partition_type == 0x00 {
                // Empty slot: the number stays reserved (fdisk numbering), nothing
                // selectable here.
                continue;
            }
            self.check_span(number, entry.start_lba, entry.sectors)?;
            if is_extended(entry.partition_type) {
                if self.container.is_some() {
                    return Err(TableError::TwoExtendedContainers);
                }
                self.container = Some(Container {
                    start_lba: entry.start_lba,
                    sectors: entry.sectors,
                });
                self.container_number = Some(number);
            } else {
                self.partitions.push(Partition {
                    number,
                    start_lba: entry.start_lba,
                    sectors: entry.sectors,
                });
            }
        }

        match self.container {
            Some(container) => {
                self.ebr_lbas.push(container.start_lba);
                self.pending = Some(container.start_lba);
                Ok(Step::Need {
                    lba: container.start_lba,
                })
            }
            None => self.finalize(),
        }
    }

    /// Feed the sector the previous [`Step::Need`] asked for (an EBR).
    pub fn feed(&mut self, sector: &[u8]) -> Result<Step, TableError> {
        let Some(ebr_lba) = self.pending.take() else {
            return Err(TableError::NotExpectingSector);
        };
        if sector.len() != SECTOR_SIZE as usize {
            return Err(TableError::SectorSize { got: sector.len() });
        }
        if !has_signature(sector) {
            return Err(TableError::EbrMissingSignature { lba: ebr_lba });
        }
        let container = self
            .container
            .expect("feed only runs after start found a container");

        // Entry 0: the logical partition this EBR describes. Its start is relative to
        // *this EBR's* sector. An empty slot describes no logical (a hole in the
        // chain) — tolerated, the chain entry still advances.
        let logical = decode_entry(sector, 0);
        if logical.partition_type == 0xEE {
            return Err(TableError::Gpt);
        }
        if logical.partition_type != 0x00 {
            if is_extended(logical.partition_type) {
                // A container inside a container in the data slot: a layout this
                // parser refuses to guess at.
                return Err(TableError::BadChainEntry {
                    lba: ebr_lba,
                    partition_type: logical.partition_type,
                });
            }
            let number = self.next_logical;
            self.next_logical += 1;
            let start_lba =
                ebr_lba
                    .checked_add(logical.start_lba)
                    .ok_or(TableError::OutOfBounds {
                        number,
                        end_lba: u64::MAX,
                        device_sectors: self.device_sectors,
                    })?;
            self.check_span(number, start_lba, logical.sectors)?;
            let end = start_lba + logical.sectors;
            if start_lba < container.start_lba || end > container.start_lba + container.sectors {
                return Err(TableError::LogicalOutsideContainer { number });
            }
            self.partitions.push(Partition {
                number,
                start_lba,
                sectors: logical.sectors,
            });
        }

        // Entry 1: the chain link. Empty = end of chain; extended = the next EBR,
        // relative to the *container's* start (the MBR extended-partition quirk);
        // anything else is refused, not guessed at.
        let chain = decode_entry(sector, 1);
        if chain.partition_type == 0x00 {
            return self.finalize();
        }
        if !is_extended(chain.partition_type) {
            return Err(TableError::BadChainEntry {
                lba: ebr_lba,
                partition_type: chain.partition_type,
            });
        }
        let next_lba = container
            .start_lba
            .checked_add(chain.start_lba)
            .ok_or(TableError::EbrOutsideContainer { lba: u64::MAX })?;
        if next_lba < container.start_lba || next_lba >= container.start_lba + container.sectors {
            return Err(TableError::EbrOutsideContainer { lba: next_lba });
        }
        if self.ebr_lbas.contains(&next_lba) {
            return Err(TableError::ChainCycle { lba: next_lba });
        }
        if self.ebr_lbas.len() >= MAX_EBRS {
            return Err(TableError::ChainTooLong);
        }
        self.ebr_lbas.push(next_lba);
        self.pending = Some(next_lba);
        Ok(Step::Need { lba: next_lba })
    }

    /// Per-entry span checks shared by primaries and logicals: non-zero length, not
    /// covering the MBR, inside the device.
    fn check_span(&self, number: u32, start_lba: u64, sectors: u64) -> Result<(), TableError> {
        if sectors == 0 {
            return Err(TableError::ZeroLength { number });
        }
        if start_lba == 0 {
            return Err(TableError::StartsAtZero { number });
        }
        let end_lba = start_lba
            .checked_add(sectors)
            .ok_or(TableError::OutOfBounds {
                number,
                end_lba: u64::MAX,
                device_sectors: self.device_sectors,
            })?;
        if end_lba > self.device_sectors {
            return Err(TableError::OutOfBounds {
                number,
                end_lba,
                device_sectors: self.device_sectors,
            });
        }
        Ok(())
    }

    /// Whole-table validation once every entry is in: pairwise overlap (the container
    /// counted against the primaries; logicals are inside it by construction) and the
    /// EBR-inside-logical refusal.
    fn finalize(&mut self) -> Result<Step, TableError> {
        for (index, a) in self.partitions.iter().enumerate() {
            for b in &self.partitions[index + 1..] {
                if a.overlaps(b) {
                    return Err(TableError::Overlap {
                        a: a.number,
                        b: b.number,
                    });
                }
            }
        }
        if let (Some(container), Some(container_number)) = (self.container, self.container_number) {
            let span = Partition {
                number: container_number,
                start_lba: container.start_lba,
                sectors: container.sectors,
            };
            // The container vs the primaries only: logicals live inside it by design.
            for p in self.partitions.iter().filter(|p| p.number <= 4) {
                if span.overlaps(p) {
                    return Err(TableError::Overlap {
                        a: container_number,
                        b: p.number,
                    });
                }
            }
        }
        for &ebr_lba in &self.ebr_lbas {
            for p in &self.partitions {
                if p.contains(ebr_lba) {
                    return Err(TableError::EbrInsideLogical {
                        ebr_lba,
                        number: p.number,
                    });
                }
            }
        }
        Ok(Step::Done(PartitionTable {
            partitions: core::mem::take(&mut self.partitions),
            container: self.container_number,
        }))
    }
}

// -----------------------------------------------------------------------------------
// Host tests: adversarial fixtures, built byte-by-byte (the load-as-test-input
// discipline — the fixtures encode the format spec independently of the parser).
// -----------------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::vec;

    /// A 512-byte sector with the 0x55AA signature and the given 16-byte entries
    /// written into slots 0..4 at offset 446.
    fn sector(entries: &[(usize, u8, u32, u32)]) -> Vec<u8> {
        let mut sector = vec![0u8; 512];
        sector[510] = 0x55;
        sector[511] = 0xAA;
        for &(slot, partition_type, start, count) in entries {
            let at = 446 + slot * 16;
            sector[at + 4] = partition_type;
            sector[at + 8..at + 12].copy_from_slice(&start.to_le_bytes());
            sector[at + 12..at + 16].copy_from_slice(&count.to_le_bytes());
        }
        sector
    }

    /// Drive a walker over sector 0 plus a list of (lba, sector) it may ask for.
    fn walk(
        device_size: u64,
        sector0: &[u8],
        ebrs: &[(u64, Vec<u8>)],
    ) -> Result<PartitionTable, TableError> {
        let mut walker = Walker::new(device_size);
        let mut step = walker.start(sector0)?;
        loop {
            match step {
                Step::Done(table) => return Ok(table),
                Step::Need { lba } => {
                    let (_, bytes) = ebrs
                        .iter()
                        .find(|(at, _)| *at == lba)
                        .unwrap_or_else(|| panic!("walker asked for unexpected LBA {lba}"));
                    step = walker.feed(bytes)?;
                }
            }
        }
    }

    const MIB: u64 = 1024 * 1024;

    #[test]
    fn two_primaries_parse_with_fdisk_numbers_and_absolute_spans() {
        let sector0 = sector(&[(0, 0x0C, 2048, 2048), (1, 0xDA, 4096, 4096)]);
        let table = walk(8 * MIB, &sector0, &[]).unwrap();
        assert_eq!(
            table.partitions,
            vec![
                Partition {
                    number: 1,
                    start_lba: 2048,
                    sectors: 2048
                },
                Partition {
                    number: 2,
                    start_lba: 4096,
                    sectors: 4096
                },
            ]
        );
        assert_eq!(table.container, None);
        let p1 = table.select(1).unwrap();
        assert_eq!((p1.start_bytes(), p1.len_bytes()), (MIB, MIB));
    }

    #[test]
    fn empty_slots_keep_their_numbers_reserved() {
        // Only slot 3 is populated: it is partition 3, not partition 1.
        let sector0 = sector(&[(2, 0x83, 2048, 1024)]);
        let table = walk(4 * MIB, &sector0, &[]).unwrap();
        assert_eq!(table.partitions.len(), 1);
        assert_eq!(table.partitions[0].number, 3);
        assert!(matches!(
            table.select(1),
            Err(SelectError::NotPresent { number: 1, .. })
        ));
    }

    #[test]
    fn missing_signature_refuses() {
        let mut sector0 = sector(&[(0, 0x0C, 2048, 1024)]);
        sector0[511] = 0x00;
        assert_eq!(
            walk(4 * MIB, &sector0, &[]).unwrap_err(),
            TableError::MissingSignature
        );
    }

    #[test]
    fn protective_gpt_refuses_typed() {
        let sector0 = sector(&[(0, 0xEE, 1, 0xFFFF_FFFF)]);
        assert_eq!(walk(8 * MIB, &sector0, &[]).unwrap_err(), TableError::Gpt);
    }

    #[test]
    fn hybrid_gpt_refuses_even_with_a_plausible_first_entry() {
        // A hybrid MBR: slot 0 looks like an honest FAT partition, slot 1 is the 0xEE
        // protective entry. Believing slot 0 would be a misread; the whole disk is GPT.
        let sector0 = sector(&[(0, 0x0C, 2048, 2048), (1, 0xEE, 1, 100)]);
        assert_eq!(walk(8 * MIB, &sector0, &[]).unwrap_err(), TableError::Gpt);
    }

    #[test]
    fn zero_length_entry_refuses() {
        let sector0 = sector(&[(0, 0x83, 2048, 0)]);
        assert_eq!(
            walk(4 * MIB, &sector0, &[]).unwrap_err(),
            TableError::ZeroLength { number: 1 }
        );
    }

    #[test]
    fn entry_starting_at_lba_zero_refuses() {
        // The window would cover the MBR sector itself — the read-only-by-construction
        // guarantee would be gone.
        let sector0 = sector(&[(1, 0x83, 0, 1024)]);
        assert_eq!(
            walk(4 * MIB, &sector0, &[]).unwrap_err(),
            TableError::StartsAtZero { number: 2 }
        );
    }

    #[test]
    fn entry_past_the_device_end_refuses() {
        // Device is 4 MiB = 8192 sectors; the entry claims up to 9216.
        let sector0 = sector(&[(0, 0x83, 8192, 1024)]);
        assert_eq!(
            walk(4 * MIB, &sector0, &[]).unwrap_err(),
            TableError::OutOfBounds {
                number: 1,
                end_lba: 9216,
                device_sectors: 8192
            }
        );
    }

    #[test]
    fn entry_ending_exactly_at_the_device_end_is_accepted() {
        let sector0 = sector(&[(0, 0x83, 4096, 4096)]);
        let table = walk(4 * MIB, &sector0, &[]).unwrap();
        assert_eq!(table.partitions[0].sectors, 4096);
    }

    #[test]
    fn overlapping_primaries_refuse() {
        let sector0 = sector(&[(0, 0x0C, 2048, 4096), (1, 0xDA, 4096, 1024)]);
        assert_eq!(
            walk(8 * MIB, &sector0, &[]).unwrap_err(),
            TableError::Overlap { a: 1, b: 2 }
        );
    }

    #[test]
    fn touching_primaries_do_not_overlap() {
        let sector0 = sector(&[(0, 0x0C, 2048, 2048), (1, 0xDA, 4096, 1024)]);
        assert!(walk(8 * MIB, &sector0, &[]).is_ok());
    }

    #[test]
    fn extended_chain_walks_two_logicals_with_correct_arithmetic() {
        // Container: slot 1, LBA 4096, 8192 sectors. EBR1 at 4096: logical at +2048
        // (absolute 6144), 1024 long; chain to container-relative 4096 (absolute 8192).
        // EBR2 at 8192: logical at +1024 (absolute 9216), 2048 long; chain ends.
        let sector0 = sector(&[(0, 0x0C, 2048, 1024), (1, 0x05, 4096, 8192)]);
        let ebr1 = sector(&[(0, 0x83, 2048, 1024), (1, 0x05, 4096, 4096)]);
        let ebr2 = sector(&[(0, 0x83, 1024, 2048)]);
        let table = walk(8 * MIB, &sector0, &[(4096, ebr1), (8192, ebr2)]).unwrap();
        assert_eq!(
            table.partitions,
            vec![
                Partition {
                    number: 1,
                    start_lba: 2048,
                    sectors: 1024
                },
                Partition {
                    number: 5,
                    start_lba: 6144,
                    sectors: 1024
                },
                Partition {
                    number: 6,
                    start_lba: 9216,
                    sectors: 2048
                },
            ]
        );
        assert_eq!(table.container, Some(2));
    }

    #[test]
    fn selecting_the_extended_container_refuses() {
        let sector0 = sector(&[(1, 0x05, 4096, 4096)]);
        let ebr1 = sector(&[(0, 0x83, 2048, 1024)]);
        let table = walk(8 * MIB, &sector0, &[(4096, ebr1)]).unwrap();
        assert_eq!(
            table.select(2),
            Err(SelectError::ExtendedContainer { number: 2 })
        );
        assert!(table.select(5).is_ok());
    }

    #[test]
    fn select_zero_refuses() {
        let sector0 = sector(&[(0, 0x0C, 2048, 1024)]);
        let table = walk(4 * MIB, &sector0, &[]).unwrap();
        assert_eq!(table.select(0), Err(SelectError::Zero));
    }

    #[test]
    fn ebr_chain_cycle_refuses() {
        // EBR at 4096 chains to absolute 6144; the EBR there chains back to 4096
        // (container-relative 0).
        let sector0 = sector(&[(1, 0x05, 4096, 4096)]);
        let ebr1 = sector(&[(0, 0x83, 1024, 512), (1, 0x05, 2048, 2048)]);
        let ebr2 = sector(&[(0, 0x83, 512, 256), (1, 0x05, 0, 2048)]);
        assert_eq!(
            walk(8 * MIB, &sector0, &[(4096, ebr1), (6144, ebr2)]).unwrap_err(),
            TableError::ChainCycle { lba: 4096 }
        );
    }

    #[test]
    fn ebr_chain_self_link_refuses_immediately() {
        // The first EBR chains to itself (container-relative 0 = the container start).
        let sector0 = sector(&[(1, 0x05, 4096, 4096)]);
        let ebr1 = sector(&[(0, 0x83, 1024, 512), (1, 0x05, 0, 2048)]);
        assert_eq!(
            walk(8 * MIB, &sector0, &[(4096, ebr1)]).unwrap_err(),
            TableError::ChainCycle { lba: 4096 }
        );
    }

    #[test]
    fn ebr_missing_signature_refuses() {
        let sector0 = sector(&[(1, 0x05, 4096, 4096)]);
        let mut ebr1 = sector(&[(0, 0x83, 1024, 512)]);
        ebr1[510] = 0;
        assert_eq!(
            walk(8 * MIB, &sector0, &[(4096, ebr1)]).unwrap_err(),
            TableError::EbrMissingSignature { lba: 4096 }
        );
    }

    #[test]
    fn logical_leaving_the_container_refuses() {
        // Container 4096..8192; the logical claims +2048 for 4096 sectors → ends at
        // 10240, past the container end.
        let sector0 = sector(&[(1, 0x05, 4096, 4096)]);
        let ebr1 = sector(&[(0, 0x83, 2048, 4096)]);
        assert_eq!(
            walk(16 * MIB, &sector0, &[(4096, ebr1)]).unwrap_err(),
            TableError::LogicalOutsideContainer { number: 5 }
        );
    }

    #[test]
    fn chained_ebr_outside_the_container_refuses() {
        let sector0 = sector(&[(1, 0x05, 4096, 4096)]);
        // Chains to container-relative 8192 → absolute 12288, past the container end.
        let ebr1 = sector(&[(0, 0x83, 1024, 512), (1, 0x05, 8192, 1024)]);
        assert_eq!(
            walk(16 * MIB, &sector0, &[(4096, ebr1)]).unwrap_err(),
            TableError::EbrOutsideContainer { lba: 12288 }
        );
    }

    #[test]
    fn ebr_inside_a_logical_span_refuses() {
        // EBR1 at 4096: logical at +1024 covering 1024..3072 (absolute 5120..7168),
        // chain to relative 2048 → absolute 6144 — which lies INSIDE that logical's
        // span: the table would be writable through partition 5's window.
        let sector0 = sector(&[(1, 0x05, 4096, 4096)]);
        let ebr1 = sector(&[(0, 0x83, 1024, 2048), (1, 0x05, 2048, 1024)]);
        // The second EBR carries no logical of its own (so the only finding left is
        // the chain sector sitting inside partition 5's span).
        let ebr2 = sector(&[]);
        assert_eq!(
            walk(16 * MIB, &sector0, &[(4096, ebr1), (6144, ebr2)]).unwrap_err(),
            TableError::EbrInsideLogical {
                ebr_lba: 6144,
                number: 5
            }
        );
    }

    #[test]
    fn non_extended_chain_entry_refuses() {
        let sector0 = sector(&[(1, 0x05, 4096, 4096)]);
        let ebr1 = sector(&[(0, 0x83, 1024, 512), (1, 0x83, 2048, 512)]);
        assert_eq!(
            walk(16 * MIB, &sector0, &[(4096, ebr1)]).unwrap_err(),
            TableError::BadChainEntry {
                lba: 4096,
                partition_type: 0x83
            }
        );
    }

    #[test]
    fn two_extended_containers_refuse() {
        let sector0 = sector(&[(0, 0x05, 2048, 1024), (1, 0x0F, 4096, 1024)]);
        assert_eq!(
            walk(8 * MIB, &sector0, &[]).unwrap_err(),
            TableError::TwoExtendedContainers
        );
    }

    #[test]
    fn chain_longer_than_the_bound_refuses() {
        // A fresh EBR every 16 sectors, each describing a tiny logical and chaining
        // on — more than MAX_EBRS of them. No LBA repeats, so only the length bound
        // can stop it.
        let container_start = 4096u64;
        let sector0 = sector(&[(1, 0x05, container_start as u32, 8192)]);
        let mut ebrs = Vec::new();
        for link in 0..(MAX_EBRS as u32 + 2) {
            let at = container_start + u64::from(link) * 16;
            let ebr = sector(&[(0, 0x83, 8, 4), (1, 0x05, (link + 1) * 16, 16)]);
            ebrs.push((at, ebr));
        }
        assert_eq!(
            walk(16 * MIB, &sector0, &ebrs).unwrap_err(),
            TableError::ChainTooLong
        );
    }

    #[test]
    fn short_sector_refuses() {
        let mut walker = Walker::new(4 * MIB);
        assert_eq!(
            walker.start(&[0u8; 511]).unwrap_err(),
            TableError::SectorSize { got: 511 }
        );
    }

    #[test]
    fn feeding_without_a_request_refuses() {
        let mut walker = Walker::new(4 * MIB);
        let sector0 = sector(&[(0, 0x0C, 2048, 1024)]);
        assert!(matches!(walker.start(&sector0), Ok(Step::Done(_))));
        assert_eq!(
            walker.feed(&sector0).unwrap_err(),
            TableError::NotExpectingSector
        );
    }

    #[test]
    fn empty_table_parses_with_no_partitions() {
        // A signed sector with four empty slots is a valid, empty table; selection
        // then reports what is (not) present.
        let sector0 = sector(&[]);
        let table = walk(4 * MIB, &sector0, &[]).unwrap();
        assert!(table.partitions.is_empty());
        let err = table.select(1).unwrap_err();
        assert!(
            matches!(err, SelectError::NotPresent { number: 1, ref present } if present.is_empty())
        );
    }

    #[test]
    fn gpt_entry_inside_an_ebr_refuses() {
        let sector0 = sector(&[(1, 0x05, 4096, 4096)]);
        let ebr1 = sector(&[(0, 0xEE, 1024, 512)]);
        assert_eq!(
            walk(16 * MIB, &sector0, &[(4096, ebr1)]).unwrap_err(),
            TableError::Gpt
        );
    }

    #[test]
    fn error_text_is_operator_grade() {
        // The component surfaces these verbatim behind "disk.part: " — pin the two
        // the gates grep for.
        assert!(alloc::format!("{}", TableError::Gpt).contains("GPT"));
        let absent = SelectError::NotPresent {
            number: 3,
            present: vec![1, 2],
        };
        let text = alloc::format!("{absent}");
        assert!(text.contains("absent"), "{text}");
        assert!(text.contains("present: 1, 2"), "{text}");
    }
}
