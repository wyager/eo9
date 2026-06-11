//! Endpoint descriptors, transfer descriptors, the HCCA, and the done queue —
//! the in-memory half of the OHCI conversation (OHCI 1.0a §4).
//!
//! Everything the controller reads or writes lives in one DMA arena the shell
//! allocates; this module owns the *layout* of that arena ([`arena`]) and the
//! encode/decode of every structure in it, so the shells never do bit arithmetic.
//! All structures are little-endian 32-bit words (§4.1) and the driver moves them
//! as byte slices through the `dma-read`/`dma-write` accessors.

use crate::ConditionCode;

/// An endpoint descriptor (OHCI 1.0a §4.2, figure 4-3): four little-endian dwords.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub struct EndpointDescriptor {
    /// FunctionAddress [6:0].
    pub function_address: u8,
    /// EndpointNumber [10:7].
    pub endpoint_number: u8,
    /// Direction [12:11]: 00/11 get direction from the TD, 01 OUT, 10 IN.
    pub direction: EdDirection,
    /// Speed (bit 13): true = low speed.
    pub low_speed: bool,
    /// sKip (bit 14): the controller skips this ED without accessing TDs.
    pub skip: bool,
    /// Format (bit 15): true = isochronous TDs (unused in v1).
    pub isochronous: bool,
    /// MaximumPacketSize [26:16].
    pub max_packet_size: u16,
    /// TDQueueTailPointer (dword 1), 16-byte aligned.
    pub tail: u32,
    /// TDQueueHeadPointer (dword 2) [31:4]; bit 0 Halted, bit 1 toggleCarry ride along.
    pub head: u32,
    /// Halted (head bit 0): the controller stopped processing this queue.
    pub halted: bool,
    /// toggleCarry (head bit 1): the data toggle when the next TD's T field is 0b0x.
    pub toggle_carry: bool,
    /// NextED (dword 3), 16-byte aligned; 0 ends the list.
    pub next: u32,
}

/// ED Direction field values (§4.2.2).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum EdDirection {
    /// 00: direction comes from each TD (control endpoints).
    #[default]
    FromTd,
    /// 01: OUT.
    Out,
    /// 10: IN.
    In,
}

impl EndpointDescriptor {
    /// Encode to the 16 bytes the controller reads.
    pub fn encode(&self) -> [u8; 16] {
        let direction = match self.direction {
            EdDirection::FromTd => 0b00,
            EdDirection::Out => 0b01,
            EdDirection::In => 0b10,
        };
        let dword0: u32 = u32::from(self.function_address & 0x7f)
            | (u32::from(self.endpoint_number & 0xf) << 7)
            | (direction << 11)
            | (u32::from(self.low_speed) << 13)
            | (u32::from(self.skip) << 14)
            | (u32::from(self.isochronous) << 15)
            | (u32::from(self.max_packet_size & 0x7ff) << 16);
        let head =
            (self.head & !0xf) | u32::from(self.halted) | (u32::from(self.toggle_carry) << 1);
        encode_dwords([dword0, self.tail & !0xf, head, self.next & !0xf])
    }

    /// Decode the controller's view back (head pointer, halted, toggle carry are what
    /// the controller updates).
    pub fn decode(bytes: &[u8; 16]) -> EndpointDescriptor {
        let [dword0, tail, head, next] = decode_dwords(bytes);
        EndpointDescriptor {
            function_address: (dword0 & 0x7f) as u8,
            endpoint_number: ((dword0 >> 7) & 0xf) as u8,
            direction: match (dword0 >> 11) & 0b11 {
                0b01 => EdDirection::Out,
                0b10 => EdDirection::In,
                _ => EdDirection::FromTd,
            },
            low_speed: dword0 & (1 << 13) != 0,
            skip: dword0 & (1 << 14) != 0,
            isochronous: dword0 & (1 << 15) != 0,
            max_packet_size: ((dword0 >> 16) & 0x7ff) as u16,
            tail,
            head: head & !0xf,
            halted: head & 0b01 != 0,
            toggle_carry: head & 0b10 != 0,
            next,
        }
    }
}

/// A general transfer descriptor (OHCI 1.0a §4.3.1, figure 4-6): four little-endian
/// dwords. Isochronous TDs are out of v1 scope.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub struct TransferDescriptor {
    /// bufferRounding (bit 18): a short packet is NOT an error when set.
    pub buffer_rounding: bool,
    /// Direction/PID [20:19]: 00 SETUP, 01 OUT, 10 IN.
    pub pid: TdPid,
    /// DelayInterrupt [23:21]: 0b111 = no interrupt for this TD.
    pub delay_interrupt: u8,
    /// DataToggle [25:24]: 0b00/0b01 = carry from the ED, 0b10 = DATA0, 0b11 = DATA1.
    pub data_toggle: TdToggle,
    /// ErrorCount [27:26] (controller-written).
    pub error_count: u8,
    /// ConditionCode [31:28] (controller-written; 0b111x = not accessed).
    pub condition_code: ConditionCode,
    /// CurrentBufferPointer (dword 1): first byte of the remaining buffer; 0 when the
    /// buffer was consumed exactly.
    pub current_buffer: u32,
    /// NextTD (dword 2), 16-byte aligned.
    pub next: u32,
    /// BufferEnd (dword 3): address of the LAST byte of the buffer (inclusive).
    pub buffer_end: u32,
}

/// TD Direction/PID values (§4.3.1.2).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum TdPid {
    #[default]
    Setup,
    Out,
    In,
}

/// TD DataToggle values (§4.3.1.2: MSB set = the TD carries its own toggle).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum TdToggle {
    /// 0b00: toggle comes from the ED's toggleCarry.
    #[default]
    FromEd,
    /// 0b10: DATA0.
    Data0,
    /// 0b11: DATA1.
    Data1,
}

/// "No interrupt" DelayInterrupt value (§4.3.1.2) — the polled driver's default.
/// The controller still retires the TD onto its internal done queue, but the
/// HccaDoneHead writeback's interrupt-delay counter never arms — fine for the polled
/// paths, which detect retirement by the ED's head moving, not by the writeback.
pub const DI_NONE: u8 = 0b111;

/// "Interrupt at the next frame boundary" DelayInterrupt value (§4.3.1.2, DI = 0):
/// the controller writes HccaDoneHead back and raises WDH within a frame of the TD
/// retiring. Set on interrupt-endpoint TDs when the driver's event paths are live —
/// a TD left at [`DI_NONE`] never generates the WDH edge `read_report` parks on (the
/// area/37 lesson: the first event-mode build waited on an interrupt the TDs
/// themselves had suppressed, and every report rode the bounded-wait-expiry drain).
pub const DI_IMMEDIATE: u8 = 0;

impl TransferDescriptor {
    /// A fresh TD for submission: not yet accessed, no errors.
    pub fn new(pid: TdPid, toggle: TdToggle, buffer: Option<(u32, u32)>) -> TransferDescriptor {
        let (current_buffer, buffer_end) = match buffer {
            // CurrentBufferPointer = first byte, BufferEnd = last byte (inclusive).
            Some((start, len)) => (start, start + len - 1),
            // A zero-length packet has both pointers 0 (§4.3.1.2).
            None => (0, 0),
        };
        TransferDescriptor {
            buffer_rounding: true,
            pid,
            delay_interrupt: DI_NONE,
            data_toggle: toggle,
            error_count: 0,
            condition_code: ConditionCode::NotAccessed,
            current_buffer,
            next: 0,
            buffer_end,
        }
    }

    /// Encode to the 16 bytes the controller reads. A fresh TD encodes condition code
    /// 0b1111 (not accessed).
    pub fn encode(&self) -> [u8; 16] {
        let pid = match self.pid {
            TdPid::Setup => 0b00,
            TdPid::Out => 0b01,
            TdPid::In => 0b10,
        };
        let toggle = match self.data_toggle {
            TdToggle::FromEd => 0b00,
            TdToggle::Data0 => 0b10,
            TdToggle::Data1 => 0b11,
        };
        let condition = match self.condition_code {
            ConditionCode::NotAccessed => 0xf,
            ConditionCode::NoError => 0x0,
            other => condition_bits(other),
        };
        let dword0: u32 = (u32::from(self.buffer_rounding) << 18)
            | (pid << 19)
            | (u32::from(self.delay_interrupt & 0b111) << 21)
            | (toggle << 24)
            | (u32::from(self.error_count & 0b11) << 26)
            | (condition << 28);
        encode_dwords([
            dword0,
            self.current_buffer,
            self.next & !0xf,
            self.buffer_end,
        ])
    }

    /// Decode the controller's writeback.
    pub fn decode(bytes: &[u8; 16]) -> TransferDescriptor {
        let [dword0, current_buffer, next, buffer_end] = decode_dwords(bytes);
        TransferDescriptor {
            buffer_rounding: dword0 & (1 << 18) != 0,
            pid: match (dword0 >> 19) & 0b11 {
                0b01 => TdPid::Out,
                0b10 => TdPid::In,
                _ => TdPid::Setup,
            },
            delay_interrupt: ((dword0 >> 21) & 0b111) as u8,
            data_toggle: match (dword0 >> 24) & 0b11 {
                0b10 => TdToggle::Data0,
                0b11 => TdToggle::Data1,
                _ => TdToggle::FromEd,
            },
            error_count: ((dword0 >> 26) & 0b11) as u8,
            condition_code: ConditionCode::from_bits((dword0 >> 28) as u8),
            current_buffer,
            next,
            buffer_end,
        }
    }

    /// How many bytes of an IN transfer's buffer were actually filled, given the
    /// original buffer span. CurrentBufferPointer == 0 means the buffer was consumed
    /// exactly (§4.3.1.3.5); otherwise it points at the next unfilled byte.
    pub fn bytes_transferred(&self, buffer_start: u32, buffer_len: u32) -> u32 {
        if self.current_buffer == 0 {
            buffer_len
        } else {
            self.current_buffer.saturating_sub(buffer_start)
        }
    }
}

fn condition_bits(code: ConditionCode) -> u32 {
    match code {
        ConditionCode::NoError => 0x0,
        ConditionCode::Crc => 0x1,
        ConditionCode::BitStuffing => 0x2,
        ConditionCode::DataToggleMismatch => 0x3,
        ConditionCode::Stall => 0x4,
        ConditionCode::DeviceNotResponding => 0x5,
        ConditionCode::PidCheckFailure => 0x6,
        ConditionCode::UnexpectedPid => 0x7,
        ConditionCode::DataOverrun => 0x8,
        ConditionCode::DataUnderrun => 0x9,
        ConditionCode::BufferOverrun => 0xc,
        ConditionCode::BufferUnderrun => 0xd,
        ConditionCode::NotAccessed => 0xf,
        ConditionCode::Reserved(bits) => u32::from(bits & 0xf),
    }
}

fn encode_dwords(dwords: [u32; 4]) -> [u8; 16] {
    let mut bytes = [0u8; 16];
    for (slot, dword) in dwords.iter().enumerate() {
        bytes[slot * 4..slot * 4 + 4].copy_from_slice(&dword.to_le_bytes());
    }
    bytes
}

fn decode_dwords(bytes: &[u8; 16]) -> [u32; 4] {
    let mut dwords = [0u32; 4];
    for (slot, dword) in dwords.iter_mut().enumerate() {
        *dword = u32::from_le_bytes(bytes[slot * 4..slot * 4 + 4].try_into().unwrap());
    }
    dwords
}

// --------------------------------------------------------------------------------------
// HCCA
// --------------------------------------------------------------------------------------

/// The Host Controller Communications Area (OHCI 1.0a §4.4, table 4-9): 256 bytes,
/// 256-byte aligned.
pub mod hcca {
    /// Total size (and required alignment) of the HCCA.
    pub const SIZE: u64 = 256;
    /// HccaInterruptTable: 32 dword head pointers at offset 0.
    pub const INTERRUPT_TABLE: u64 = 0x00;
    pub const INTERRUPT_TABLE_ENTRIES: u64 = 32;
    /// HccaFrameNumber: u16 at 0x80, updated each frame.
    pub const FRAME_NUMBER: u64 = 0x80;
    /// HccaDoneHead: u32 at 0x84. Written at the WDH event; bit 0 set means other
    /// interrupt events are also pending. 0 = nothing retired since the last take.
    pub const DONE_HEAD: u64 = 0x84;

    /// Offset of interrupt-table slot `index` (the periodic schedule entry for frames
    /// where `frame % 32 == index`).
    pub const fn interrupt_table_entry(index: u64) -> u64 {
        INTERRUPT_TABLE + 4 * (index % INTERRUPT_TABLE_ENTRIES)
    }
}

/// Walk a retired done queue. `done_head` is the HccaDoneHead value (bit 0 masked off
/// by the caller via [`done_head_pointer`]); `read_td` maps a TD physical address to
/// its decoded form. The controller links retired TDs through NextTD in **reverse**
/// order of completion (§4.4.1.1: the most recently retired TD is at the head), so
/// the addresses are yielded head-first = newest-first; callers that need completion
/// order reverse. Bounded by `limit` so a corrupt pointer chain cannot loop forever.
pub fn walk_done_queue(
    done_head: u32,
    limit: usize,
    mut read_td: impl FnMut(u32) -> TransferDescriptor,
    mut visit: impl FnMut(u32, TransferDescriptor),
) -> Result<(), DoneQueueOverrun> {
    let mut current = done_head & !0xf;
    let mut remaining = limit;
    while current != 0 {
        if remaining == 0 {
            return Err(DoneQueueOverrun);
        }
        remaining -= 1;
        let td = read_td(current);
        visit(current, td);
        current = td.next & !0xf;
    }
    Ok(())
}

/// The done-queue walk exceeded its bound — a corrupt NextTD chain.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DoneQueueOverrun;

/// Split an HccaDoneHead value into the list pointer and the "other interrupts
/// pending" flag (bit 0, §4.4.1).
pub fn done_head_pointer(hcca_done_head: u32) -> (u32, bool) {
    (hcca_done_head & !0xf, hcca_done_head & 1 != 0)
}

// --------------------------------------------------------------------------------------
// DMA arena layout
// --------------------------------------------------------------------------------------

/// Layout of the one DMA arena both shells allocate (`alloc-dma`, page-aligned, which
/// over-satisfies the HCCA's 256-byte and the ED/TD 16-byte alignment requirements).
/// Offsets are from the arena base; the device-visible addresses are `dma-address +
/// offset`.
pub mod arena {
    /// HCCA at the (page-aligned) base.
    pub const HCCA: u64 = 0;
    /// The control ED (endpoint 0 of the device under enumeration).
    pub const CONTROL_ED: u64 = 0x100;
    /// The interrupt ED (the HID interrupt-IN endpoint).
    pub const INTERRUPT_ED: u64 = 0x110;
    /// TD pool: slots of 16 bytes. Slot ownership: 0-3 the control path, 4-5 the
    /// interrupt endpoint's ping-pong pair, 6-7 the bulk-IN pair, 8-9 the bulk-OUT
    /// pair (each pair is pending TD + dummy tail, the same shape everywhere).
    pub const TD_POOL: u64 = 0x120;
    pub const TD_SLOTS: u64 = 10;
    /// The two resident bulk EDs (IN then OUT on the bulk list —
    /// docs/board/usb-msd-plan.md §1.1; resident so the controller-owned toggleCarry
    /// survives across transfers, which is exactly what `TdToggle::FromEd` is for).
    pub const BULK_IN_ED: u64 = 0x1c0;
    pub const BULK_OUT_ED: u64 = 0x1d0;
    /// Control-transfer data buffer (descriptor reads fit comfortably).
    pub const CONTROL_BUFFER: u64 = 0x200;
    pub const CONTROL_BUFFER_LEN: u64 = 0x200;
    /// Setup-packet buffer (8 bytes).
    pub const SETUP_BUFFER: u64 = 0x400;
    /// Interrupt-IN report buffer.
    pub const INTERRUPT_BUFFER: u64 = 0x440;
    pub const INTERRUPT_BUFFER_LEN: u64 = 0x40;
    /// Bulk data buffer: one general TD's reach is at most two 4 KiB pages
    /// (OHCI 1.0a §4.3.1.1), and this window starts page-aligned within the
    /// page-aligned arena, so a full-window transfer is exactly one TD — the v1
    /// one-TD-at-a-time grain (8 KiB per round against a ~1 MB/s full-speed bus;
    /// TD chaining is the recorded follow-up, usb-msd-plan §1.1).
    pub const BULK_BUFFER: u64 = 0x1000;
    pub const BULK_BUFFER_LEN: u64 = 0x2000;
    /// Total arena size to allocate.
    pub const SIZE: u64 = 0x3000;

    /// Offset of TD-pool slot `slot`.
    pub const fn td_slot(slot: u64) -> u64 {
        TD_POOL + 16 * (slot % TD_SLOTS)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ed_encode_pins_every_field() {
        // A low-speed control endpoint, function 2, endpoint 0, MPS 8, with queue
        // pointers — cross-checked by hand against OHCI 1.0a figure 4-3.
        let ed = EndpointDescriptor {
            function_address: 2,
            endpoint_number: 0,
            direction: EdDirection::FromTd,
            low_speed: true,
            skip: false,
            isochronous: false,
            max_packet_size: 8,
            tail: 0x1000_0120,
            head: 0x1000_0130,
            halted: false,
            toggle_carry: false,
            next: 0,
        };
        let bytes = ed.encode();
        // dword0 = FA=2 | EN=0 | D=00 | S=1<<13 | MPS=8<<16 = 0x0008_2002.
        assert_eq!(&bytes[0..4], &0x0008_2002u32.to_le_bytes());
        assert_eq!(&bytes[4..8], &0x1000_0120u32.to_le_bytes());
        assert_eq!(&bytes[8..12], &0x1000_0130u32.to_le_bytes());
        assert_eq!(&bytes[12..16], &0u32.to_le_bytes());
        assert_eq!(EndpointDescriptor::decode(&bytes), ed);
    }

    #[test]
    fn ed_skip_and_interrupt_in_shape() {
        // A full-speed interrupt-IN endpoint 1 on function 2, MPS 8, skipped.
        let ed = EndpointDescriptor {
            function_address: 2,
            endpoint_number: 1,
            direction: EdDirection::In,
            skip: true,
            max_packet_size: 8,
            ..EndpointDescriptor::default()
        };
        let bytes = ed.encode();
        // dword0 = FA=2 | EN=1<<7 | D=10<<11 | K=1<<14 | MPS=8<<16 = 0x0008_5082.
        assert_eq!(&bytes[0..4], &0x0008_5082u32.to_le_bytes());
    }

    #[test]
    fn ed_bulk_out_shape() {
        // A full-speed bulk-OUT endpoint 2 on function 1, MPS 64 — the usb-storage
        // shape (direction encoded in the ED, TDs carry FromEd toggles).
        let ed = EndpointDescriptor {
            function_address: 1,
            endpoint_number: 2,
            direction: EdDirection::Out,
            max_packet_size: 64,
            ..EndpointDescriptor::default()
        };
        let bytes = ed.encode();
        // dword0 = FA=1 | EN=2<<7 | D=01<<11 | MPS=64<<16 = 0x0040_0901.
        assert_eq!(&bytes[0..4], &0x0040_0901u32.to_le_bytes());
        assert_eq!(EndpointDescriptor::decode(&bytes), ed);
    }

    #[test]
    fn ed_halted_and_toggle_ride_the_head_pointer() {
        let mut ed = EndpointDescriptor {
            head: 0x2000_0040,
            halted: true,
            toggle_carry: true,
            ..EndpointDescriptor::default()
        };
        let bytes = ed.encode();
        assert_eq!(&bytes[8..12], &0x2000_0043u32.to_le_bytes());
        let decoded = EndpointDescriptor::decode(&bytes);
        assert!(decoded.halted && decoded.toggle_carry);
        assert_eq!(decoded.head, 0x2000_0040);
        // And the clean state encodes cleanly.
        ed.halted = false;
        ed.toggle_carry = false;
        assert_eq!(&ed.encode()[8..12], &0x2000_0040u32.to_le_bytes());
    }

    #[test]
    fn setup_td_encodes_data0_and_not_accessed() {
        // The SETUP stage: 8-byte buffer at 0x3000_0400, DATA0, no interrupt.
        let td = TransferDescriptor::new(TdPid::Setup, TdToggle::Data0, Some((0x3000_0400, 8)));
        let bytes = td.encode();
        // dword0 = R(1<<18) | DP=00 | DI=111<<21 | T=10<<24 | CC=1111<<28 = 0xf2e4_0000.
        assert_eq!(&bytes[0..4], &0xf2e4_0000u32.to_le_bytes());
        assert_eq!(&bytes[4..8], &0x3000_0400u32.to_le_bytes());
        // BufferEnd is the address of the LAST byte (inclusive).
        assert_eq!(&bytes[12..16], &0x3000_0407u32.to_le_bytes());
    }

    #[test]
    fn status_stage_td_is_zero_length_data1() {
        // An IN status stage: zero-length, DATA1, both buffer pointers 0.
        let td = TransferDescriptor::new(TdPid::In, TdToggle::Data1, None);
        let bytes = td.encode();
        // dword0 = R | DP=10<<19 | DI=111<<21 | T=11<<24 | CC=1111 = 0xf3f4_0000.
        assert_eq!(&bytes[0..4], &0xf3f4_0000u32.to_le_bytes());
        assert_eq!(&bytes[4..8], &0u32.to_le_bytes());
        assert_eq!(&bytes[12..16], &0u32.to_le_bytes());
    }

    #[test]
    fn td_decode_reads_the_controller_writeback() {
        // A retired IN TD: CC = NoError, toggle updated to DATA1, buffer consumed
        // exactly (CBP = 0).
        let dword0: u32 = (1 << 18) | (0b10 << 19) | (0b111 << 21) | (0b11 << 24);
        let mut bytes = [0u8; 16];
        bytes[0..4].copy_from_slice(&dword0.to_le_bytes());
        bytes[8..12].copy_from_slice(&0x3000_0120u32.to_le_bytes());
        bytes[12..16].copy_from_slice(&0x3000_0211u32.to_le_bytes());
        let td = TransferDescriptor::decode(&bytes);
        assert_eq!(td.condition_code, ConditionCode::NoError);
        assert_eq!(td.pid, TdPid::In);
        assert_eq!(td.bytes_transferred(0x3000_0200, 18), 18);
        // A short read leaves CBP pointing at the next unfilled byte.
        let short = TransferDescriptor {
            current_buffer: 0x3000_0208,
            ..td
        };
        assert_eq!(short.bytes_transferred(0x3000_0200, 18), 8);
    }

    #[test]
    fn hcca_layout_matches_the_spec() {
        assert_eq!(hcca::SIZE, 256);
        assert_eq!(hcca::FRAME_NUMBER, 0x80);
        assert_eq!(hcca::DONE_HEAD, 0x84);
        assert_eq!(hcca::interrupt_table_entry(0), 0);
        assert_eq!(hcca::interrupt_table_entry(31), 0x7c);
        assert_eq!(hcca::interrupt_table_entry(33), 0x04);
    }

    #[test]
    fn done_queue_walks_newest_first_and_bounds_corruption() {
        // Three retired TDs linked newest-first: 0x340 -> 0x330 -> 0x320 -> 0.
        let td_at = |address: u32| -> TransferDescriptor {
            let next = match address {
                0x340 => 0x330,
                0x330 => 0x320,
                _ => 0,
            };
            TransferDescriptor {
                next,
                ..TransferDescriptor::default()
            }
        };
        let mut seen = Vec::new();
        walk_done_queue(0x341, 8, td_at, |address, _| seen.push(address)).unwrap();
        // Bit 0 of HccaDoneHead is the "other interrupts" flag, masked by the walk.
        assert_eq!(seen, vec![0x340, 0x330, 0x320]);
        assert_eq!(done_head_pointer(0x341), (0x340, true));

        // A self-linked TD must hit the bound, not hang.
        let looped = |_: u32| TransferDescriptor {
            next: 0x340,
            ..TransferDescriptor::default()
        };
        assert_eq!(
            walk_done_queue(0x340, 8, looped, |_, _| {}),
            Err(DoneQueueOverrun)
        );
    }

    #[test]
    fn arena_layout_is_aligned_and_disjoint() {
        // HCCA needs 256-byte alignment (the arena base is page-aligned), EDs/TDs
        // 16-byte alignment, and no two windows overlap.
        assert_eq!(arena::HCCA % 256, 0);
        assert_eq!(arena::CONTROL_ED % 16, 0);
        assert_eq!(arena::INTERRUPT_ED % 16, 0);
        assert_eq!(arena::BULK_IN_ED % 16, 0);
        assert_eq!(arena::BULK_OUT_ED % 16, 0);
        for slot in 0..arena::TD_SLOTS {
            assert_eq!(arena::td_slot(slot) % 16, 0);
        }
        // The bulk buffer starts page-aligned so its full window spans exactly the
        // two physical pages one general TD can address (§4.3.1.1).
        assert_eq!(arena::BULK_BUFFER % 0x1000, 0);
        assert_eq!(arena::BULK_BUFFER_LEN, 0x2000);
        let windows = [
            (arena::HCCA, super::hcca::SIZE),
            (arena::CONTROL_ED, 16),
            (arena::INTERRUPT_ED, 16),
            (arena::BULK_IN_ED, 16),
            (arena::BULK_OUT_ED, 16),
            (arena::TD_POOL, 16 * arena::TD_SLOTS),
            (arena::CONTROL_BUFFER, arena::CONTROL_BUFFER_LEN),
            (arena::SETUP_BUFFER, 8),
            (arena::INTERRUPT_BUFFER, arena::INTERRUPT_BUFFER_LEN),
            (arena::BULK_BUFFER, arena::BULK_BUFFER_LEN),
        ];
        for (i, &(start_a, len_a)) in windows.iter().enumerate() {
            assert!(start_a + len_a <= arena::SIZE);
            for &(start_b, len_b) in &windows[i + 1..] {
                assert!(
                    start_a + len_a <= start_b || start_b + len_b <= start_a,
                    "arena windows overlap: {start_a:#x}+{len_a:#x} vs {start_b:#x}+{len_b:#x}"
                );
            }
        }
    }
}
