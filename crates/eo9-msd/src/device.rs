//! The Bulk-Only Transport command engine: the three-phase ladder (CBW out, data,
//! CSW in) over an abstract bulk-endpoint pair, with the BOT error-recovery rules
//! and the six-command SCSI surface the `usb.msd` component exports a disk from.
//!
//! The [`Transport`] trait is the seam: the wasm component implements it over
//! `eo9:usb` (bulk-read/bulk-write plus the three control requests), the host tests
//! implement it as a strict scripted mock — every call must match the script's next
//! step exactly, so the ladder's *sequence* (who clears which halt when, when the
//! reset fires) is pinned, not just its outcomes.
//!
//! The error ladder, per BOT 1.0 (§5.3, §6.6–6.7, figure 2):
//!
//! * **CBW stall** → reset recovery, typed protocol error (§6.6.1: a CBW the device
//!   STALLs is unrecoverable short of the reset).
//! * **Data-stage stall** → CLEAR_FEATURE(ENDPOINT_HALT) on the stalled direction —
//!   the provider already recovered ITS half per the eo9:usb halt contract — then
//!   proceed to the CSW, which names the outcome (§6.7.2/§6.7.3).
//! * **CSW stall** → clear halt, retry the CSW once; a second stall → reset
//!   recovery (§6.7.1 figure 2's two-try rule).
//! * **CSW status 1 (command failed)** → REQUEST SENSE, surface the key/ASC/ASCQ
//!   as [`MsdError::CommandFailed`].
//! * **CSW status 2 (phase error) or an undecodable CSW** → reset recovery
//!   (§5.3.4: Bulk-Only Mass Storage Reset, then clear both halts, in that order).
//! * **A command abandoned mid-conversation** (the component's cancel-on-drop) →
//!   the next command runs reset recovery first, so a torn BOT exchange can never
//!   make a later command consume the earlier one's data or status.

use alloc::vec::Vec;

use crate::bot::{self, Csw, CswError, Direction};
use crate::scsi::{self, Capacity, Inquiry, Sense};

/// The first command tag; subsequent commands increment (wrapping). The value is
/// arbitrary by spec (the device only echoes it); the e09 prefix makes transcripts
/// greppable and matches the L1 probe fixture.
pub const FIRST_TAG: u32 = 0xe090_0001;

/// What the engine asks of the endpoint pair. Implemented over `eo9:usb` by the
/// component and as a scripted mock by the host tests.
#[allow(async_fn_in_trait)]
pub trait Transport {
    /// One bulk-OUT transfer, whole (the provider loops its grain internally).
    async fn bulk_out(&mut self, data: &[u8]) -> Result<(), TransportError>;
    /// One bulk-IN transfer of up to `length` bytes; shorter answers are normal
    /// (the provider's transfer-window grain, or the device's short packet — the
    /// engine loops and lets the CSW arbitrate). An empty answer is a zero-length
    /// packet: the device's end-of-data marker.
    async fn bulk_in(&mut self, length: u32) -> Result<Vec<u8>, TransportError>;
    /// CLEAR_FEATURE(ENDPOINT_HALT) on the bulk-IN endpoint (the consumer's half of
    /// the eo9:usb halt-recovery contract; resets the device-side toggle).
    async fn clear_halt_in(&mut self) -> Result<(), TransportError>;
    /// CLEAR_FEATURE(ENDPOINT_HALT) on the bulk-OUT endpoint.
    async fn clear_halt_out(&mut self) -> Result<(), TransportError>;
    /// Bulk-Only Mass Storage Reset (class request 0xFF to the interface).
    async fn mass_storage_reset(&mut self) -> Result<(), TransportError>;
}

/// Transport-level failures, as the engine distinguishes them. The component maps
/// `eo9:usb`'s error variants into these; everything the ladder does not handle
/// structurally rides through as [`TransportError::Other`] text.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TransportError {
    /// The endpoint answered STALL (the device halting it — BOT error signalling).
    Stall,
    /// A bounded wait expired (the device NAKs forever: dead or wedged).
    Timeout,
    /// Anything else, labelled by the mapper.
    Other(alloc::string::String),
}

/// Why a BOT conversation broke (reset recovery has already run when one of these
/// is returned — the device is in a known state again, the command's outcome is not).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ProtocolError {
    /// The device STALLed the CBW itself.
    CbwStalled,
    /// The CSW stalled twice (the §6.7.1 two-try rule exhausted).
    CswStalledTwice,
    /// The CSW arrived but did not decode (length/signature/tag/reserved status).
    BadCsw(CswError),
    /// bCSWStatus 2: the device lost phase agreement.
    PhaseError,
}

/// Typed failures of one SCSI command over BOT.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum MsdError {
    /// CSW status 1: the device executed the exchange but refused the command.
    /// `sense` carries REQUEST SENSE's key/ASC/ASCQ (`None` when the sense fetch
    /// itself failed or answered an unparseable format).
    CommandFailed { sense: Option<Sense> },
    /// The BOT conversation broke; reset recovery was performed.
    Protocol(ProtocolError),
    /// The transport failed outside the ladder's structural cases.
    Transport(TransportError),
    /// The device moved fewer data bytes than the command required, with a passing
    /// CSW (short data and/or reported residue).
    ShortData { expected: u32, got: u32 },
}

/// The data stage of one command.
enum DataPhase<'a> {
    None,
    In { expected: u32 },
    Out { data: &'a [u8] },
}

/// The BOT command engine over one bulk-endpoint pair. LUN 0 unconditionally
/// (docs/board/usb-msd-plan.md §1.2 — the component refuses multi-LUN devices
/// before this engine ever runs).
pub struct Bot<T: Transport> {
    transport: T,
    tag: u32,
    /// A command was abandoned between CBW and CSW (the calling future dropped at
    /// an await): the device may still be mid-exchange, so the next command must
    /// re-synchronize with reset recovery before touching the wire.
    mid_command: bool,
}

impl<T: Transport> Bot<T> {
    pub fn new(transport: T) -> Bot<T> {
        Bot {
            transport,
            tag: FIRST_TAG,
            mid_command: false,
        }
    }

    /// INQUIRY: the device's identity strings (and the type check's input).
    pub async fn inquiry(&mut self) -> Result<Inquiry, MsdError> {
        let cdb = scsi::inquiry(scsi::INQUIRY_LEN);
        let expected = u32::from(scsi::INQUIRY_LEN);
        let (data, _residue) = self.command(&cdb, DataPhase::In { expected }).await?;
        Inquiry::parse(&data).ok_or(MsdError::ShortData {
            expected,
            got: data.len() as u32,
        })
    }

    /// TEST UNIT READY: succeeds when the unit is ready; a failure carries the
    /// sense triple (the post-reset UNIT ATTENTION shows up here, and one TUR
    /// retry after its sense fetch is the documented way to consume it).
    pub async fn test_unit_ready(&mut self) -> Result<(), MsdError> {
        let cdb = scsi::test_unit_ready();
        self.command(&cdb, DataPhase::None).await.map(|_| ())
    }

    /// READ CAPACITY(10): last LBA + block size.
    pub async fn read_capacity(&mut self) -> Result<Capacity, MsdError> {
        let cdb = scsi::read_capacity10();
        let expected = scsi::READ_CAPACITY_LEN as u32;
        let (data, _residue) = self.command(&cdb, DataPhase::In { expected }).await?;
        Capacity::parse(&data).ok_or(MsdError::ShortData {
            expected,
            got: data.len() as u32,
        })
    }

    /// READ(10): `blocks` blocks at `lba`, all bytes or a typed error (a passing
    /// CSW with short data is [`MsdError::ShortData`] — block reads are exact).
    pub async fn read10(
        &mut self,
        lba: u32,
        blocks: u16,
        block_size: u32,
    ) -> Result<Vec<u8>, MsdError> {
        let cdb = scsi::read10(lba, blocks);
        let expected = u32::from(blocks) * block_size;
        let (data, _residue) = self.command(&cdb, DataPhase::In { expected }).await?;
        if (data.len() as u32) < expected {
            return Err(MsdError::ShortData {
                expected,
                got: data.len() as u32,
            });
        }
        Ok(data)
    }

    /// WRITE(10): `data` (exactly `blocks * block_size` bytes) at `lba`; a CSW
    /// residue means the device did not accept everything — typed, never silent.
    pub async fn write10(
        &mut self,
        lba: u32,
        blocks: u16,
        block_size: u32,
        data: &[u8],
    ) -> Result<(), MsdError> {
        let expected = u32::from(blocks) * block_size;
        debug_assert_eq!(data.len() as u32, expected, "write10 payload size");
        let cdb = scsi::write10(lba, blocks);
        let (_data, residue) = self.command(&cdb, DataPhase::Out { data }).await?;
        if residue != 0 {
            return Err(MsdError::ShortData {
                expected,
                got: expected.saturating_sub(residue),
            });
        }
        Ok(())
    }

    /// One command through the ladder; CSW status 1 is turned into
    /// [`MsdError::CommandFailed`] by fetching sense data here, so every caller
    /// gets the typed key/ASC/ASCQ for free.
    async fn command(
        &mut self,
        cdb: &[u8],
        phase: DataPhase<'_>,
    ) -> Result<(Vec<u8>, u32), MsdError> {
        let (data, csw) = self.execute(cdb, phase).await?;
        match csw.status {
            bot::CSW_PASSED => Ok((data, csw.residue)),
            // CSW_FAILED — the only other value `execute` lets through.
            _ => {
                let sense = self.fetch_sense().await;
                Err(MsdError::CommandFailed { sense })
            }
        }
    }

    /// REQUEST SENSE as its own BOT round trip; best-effort (`None` on any
    /// failure — the CommandFailed it decorates is the primary error).
    async fn fetch_sense(&mut self) -> Option<Sense> {
        let cdb = scsi::request_sense(scsi::SENSE_LEN);
        let expected = u32::from(scsi::SENSE_LEN);
        match self.execute(&cdb, DataPhase::In { expected }).await {
            Ok((data, csw)) if csw.status == bot::CSW_PASSED => Sense::parse(&data),
            _ => None,
        }
    }

    /// The three-phase ladder. Returns the IN data (empty for OUT/none) and a CSW
    /// whose status is PASSED or FAILED — every other outcome is an `Err`, with
    /// reset recovery already performed where the spec mandates it.
    async fn execute(
        &mut self,
        cdb: &[u8],
        phase: DataPhase<'_>,
    ) -> Result<(Vec<u8>, Csw), MsdError> {
        // Re-synchronize after an abandoned exchange before anything else.
        if self.mid_command {
            self.reset_recovery().await.map_err(MsdError::Transport)?;
            self.mid_command = false;
        }

        let (transfer_length, direction) = match &phase {
            DataPhase::None => (0, Direction::Out),
            DataPhase::In { expected } => (*expected, Direction::In),
            DataPhase::Out { data } => (data.len() as u32, Direction::Out),
        };
        self.tag = self.tag.wrapping_add(1);
        let tag = self.tag;
        let cbw = bot::encode_cbw(tag, transfer_length, direction, 0, cdb);

        // Phase 1: CBW out. A STALL here is unrecoverable short of the reset
        // (BOT §6.6.1); any other failure leaves `mid_command` set so the next
        // command re-synchronizes.
        self.mid_command = true;
        match self.transport.bulk_out(&cbw).await {
            Ok(()) => {}
            Err(TransportError::Stall) => {
                self.reset_recovery().await.map_err(MsdError::Transport)?;
                self.mid_command = false;
                return Err(MsdError::Protocol(ProtocolError::CbwStalled));
            }
            Err(other) => return Err(MsdError::Transport(other)),
        }

        // Phase 2: data. A stall ends the stage (the device truncating the
        // transfer); the consumer-owed clear-halt runs and the CSW arbitrates.
        let mut data = Vec::new();
        match phase {
            DataPhase::None => {}
            DataPhase::In { expected } => {
                while (data.len() as u32) < expected {
                    let remaining = expected - data.len() as u32;
                    match self.transport.bulk_in(remaining).await {
                        Ok(chunk) if chunk.is_empty() => break, // ZLP: end of data
                        Ok(chunk) => data.extend_from_slice(&chunk),
                        Err(TransportError::Stall) => {
                            self.transport
                                .clear_halt_in()
                                .await
                                .map_err(MsdError::Transport)?;
                            break;
                        }
                        Err(other) => return Err(MsdError::Transport(other)),
                    }
                }
            }
            DataPhase::Out { data: payload } => match self.transport.bulk_out(payload).await {
                Ok(()) => {}
                Err(TransportError::Stall) => {
                    self.transport
                        .clear_halt_out()
                        .await
                        .map_err(MsdError::Transport)?;
                }
                Err(other) => return Err(MsdError::Transport(other)),
            },
        }

        // Phase 3: CSW, with the two-try stall rule (§6.7.1 figure 2).
        let csw_bytes = match self.transport.bulk_in(bot::CSW_LEN as u32).await {
            Ok(bytes) => bytes,
            Err(TransportError::Stall) => {
                self.transport
                    .clear_halt_in()
                    .await
                    .map_err(MsdError::Transport)?;
                match self.transport.bulk_in(bot::CSW_LEN as u32).await {
                    Ok(bytes) => bytes,
                    Err(TransportError::Stall) => {
                        self.reset_recovery().await.map_err(MsdError::Transport)?;
                        self.mid_command = false;
                        return Err(MsdError::Protocol(ProtocolError::CswStalledTwice));
                    }
                    Err(other) => return Err(MsdError::Transport(other)),
                }
            }
            Err(other) => return Err(MsdError::Transport(other)),
        };

        let csw = match bot::decode_csw(&csw_bytes, tag) {
            Ok(csw) => csw,
            Err(error) => {
                // Not a valid CSW (BOT §6.3): reset recovery, typed error.
                self.reset_recovery().await.map_err(MsdError::Transport)?;
                self.mid_command = false;
                return Err(MsdError::Protocol(ProtocolError::BadCsw(error)));
            }
        };
        if csw.status == bot::CSW_PHASE_ERROR {
            self.reset_recovery().await.map_err(MsdError::Transport)?;
            self.mid_command = false;
            return Err(MsdError::Protocol(ProtocolError::PhaseError));
        }

        self.mid_command = false;
        Ok((data, csw))
    }

    /// Reset recovery (BOT §5.3.4), in the spec's order: Bulk-Only Mass Storage
    /// Reset, clear the bulk-IN halt, clear the bulk-OUT halt.
    async fn reset_recovery(&mut self) -> Result<(), TransportError> {
        self.transport.mass_storage_reset().await?;
        self.transport.clear_halt_in().await?;
        self.transport.clear_halt_out().await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scsi::sense_key;
    use std::collections::VecDeque;
    use std::future::Future;

    /// One scripted step: what call the engine MUST make next, and its answer.
    /// The strict-mock rule: any divergence (wrong call, wrong bytes, wrong ask)
    /// panics with the step index, and a finished test asserts the script is
    /// exhausted — the ladder's sequence is the thing under test.
    #[derive(Debug)]
    enum Step {
        Out {
            expect: Vec<u8>,
            answer: Result<(), TransportError>,
        },
        In {
            expect_length: u32,
            answer: Result<Vec<u8>, TransportError>,
        },
        ClearHaltIn,
        ClearHaltOut,
        Reset,
    }

    struct Scripted {
        steps: VecDeque<Step>,
        consumed: usize,
    }

    impl Scripted {
        fn new(steps: Vec<Step>) -> Scripted {
            Scripted {
                steps: steps.into(),
                consumed: 0,
            }
        }

        fn next(&mut self, what: &str) -> Step {
            self.consumed += 1;
            self.steps
                .pop_front()
                .unwrap_or_else(|| panic!("step {}: unscripted call {what}", self.consumed))
        }

        fn assert_done(&self) {
            assert!(
                self.steps.is_empty(),
                "script not exhausted: {} step(s) left, first {:?}",
                self.steps.len(),
                self.steps.front()
            );
        }
    }

    impl Transport for Scripted {
        async fn bulk_out(&mut self, data: &[u8]) -> Result<(), TransportError> {
            let index = self.consumed + 1;
            match self.next("bulk_out") {
                Step::Out { expect, answer } => {
                    assert_eq!(data, expect.as_slice(), "step {index}: bulk_out bytes");
                    answer
                }
                other => panic!("step {index}: expected {other:?}, engine called bulk_out"),
            }
        }

        async fn bulk_in(&mut self, length: u32) -> Result<Vec<u8>, TransportError> {
            let index = self.consumed + 1;
            match self.next("bulk_in") {
                Step::In {
                    expect_length,
                    answer,
                } => {
                    assert_eq!(length, expect_length, "step {index}: bulk_in ask");
                    answer
                }
                other => panic!("step {index}: expected {other:?}, engine called bulk_in"),
            }
        }

        async fn clear_halt_in(&mut self) -> Result<(), TransportError> {
            let index = self.consumed + 1;
            match self.next("clear_halt_in") {
                Step::ClearHaltIn => Ok(()),
                other => panic!("step {index}: expected {other:?}, engine called clear_halt_in"),
            }
        }

        async fn clear_halt_out(&mut self) -> Result<(), TransportError> {
            let index = self.consumed + 1;
            match self.next("clear_halt_out") {
                Step::ClearHaltOut => Ok(()),
                other => panic!("step {index}: expected {other:?}, engine called clear_halt_out"),
            }
        }

        async fn mass_storage_reset(&mut self) -> Result<(), TransportError> {
            let index = self.consumed + 1;
            match self.next("mass_storage_reset") {
                Step::Reset => Ok(()),
                other => {
                    panic!("step {index}: expected {other:?}, engine called mass_storage_reset")
                }
            }
        }
    }

    /// Drive a ready-everywhere future to completion (every await is immediately
    /// ready against the scripted mock — the eo9-ohci test-runner shape).
    fn run<R>(future: impl Future<Output = R>) -> R {
        let mut pinned = std::pin::pin!(future);
        let waker = std::task::Waker::noop();
        let mut context = std::task::Context::from_waker(waker);
        for _ in 0..1_000_000 {
            if let std::task::Poll::Ready(value) = pinned.as_mut().poll(&mut context) {
                return value;
            }
        }
        panic!("the engine future did not complete against the always-ready mock");
    }

    /// The nth command's tag (1-based: the first command sent gets FIRST_TAG + 1).
    fn tag(n: u32) -> u32 {
        FIRST_TAG.wrapping_add(n)
    }

    fn csw(tag: u32, residue: u32, status: u8) -> Vec<u8> {
        let mut bytes = vec![0u8; 13];
        bytes[0..4].copy_from_slice(b"USBS");
        bytes[4..8].copy_from_slice(&tag.to_le_bytes());
        bytes[8..12].copy_from_slice(&residue.to_le_bytes());
        bytes[12] = status;
        bytes
    }

    fn fixed_sense(key: u8, asc: u8, ascq: u8) -> Vec<u8> {
        let mut bytes = vec![0u8; 18];
        bytes[0] = 0x70;
        bytes[2] = key;
        bytes[7] = 10; // additional length through the ASCQ
        bytes[12] = asc;
        bytes[13] = ascq;
        bytes
    }

    fn cbw(tag: u32, length: u32, direction: Direction, cdb: &[u8]) -> Vec<u8> {
        bot::encode_cbw(tag, length, direction, 0, cdb).to_vec()
    }

    #[test]
    fn read10_loops_the_provider_grain_and_passes() {
        let sectors: Vec<u8> = (0..1024u32).map(|i| i as u8).collect();
        let script = vec![
            Step::Out {
                expect: cbw(tag(1), 1024, Direction::In, &scsi::read10(2, 2)),
                answer: Ok(()),
            },
            // The provider answers in two grain chunks; the engine loops.
            Step::In {
                expect_length: 1024,
                answer: Ok(sectors[..512].to_vec()),
            },
            Step::In {
                expect_length: 512,
                answer: Ok(sectors[512..].to_vec()),
            },
            Step::In {
                expect_length: 13,
                answer: Ok(csw(tag(1), 0, bot::CSW_PASSED)),
            },
        ];
        let mut bot = Bot::new(Scripted::new(script));
        let data = run(bot.read10(2, 2, 512)).unwrap();
        assert_eq!(data, sectors);
        bot.transport.assert_done();
    }

    #[test]
    fn write10_sends_cbw_then_payload() {
        let payload = vec![0xa5u8; 512];
        let script = vec![
            Step::Out {
                expect: cbw(tag(1), 512, Direction::Out, &scsi::write10(7, 1)),
                answer: Ok(()),
            },
            Step::Out {
                expect: payload.clone(),
                answer: Ok(()),
            },
            Step::In {
                expect_length: 13,
                answer: Ok(csw(tag(1), 0, bot::CSW_PASSED)),
            },
        ];
        let mut bot = Bot::new(Scripted::new(script));
        run(bot.write10(7, 1, 512, &payload)).unwrap();
        bot.transport.assert_done();
    }

    #[test]
    fn inquiry_decodes_the_qemu_shape() {
        let mut answer = vec![0u8; 36];
        answer[1] = 0x80;
        answer[8..16].copy_from_slice(b"QEMU    ");
        answer[16..32].copy_from_slice(b"QEMU HARDDISK   ");
        answer[32..36].copy_from_slice(b"2.5+");
        let script = vec![
            Step::Out {
                expect: cbw(tag(1), 36, Direction::In, &scsi::inquiry(36)),
                answer: Ok(()),
            },
            Step::In {
                expect_length: 36,
                answer: Ok(answer),
            },
            Step::In {
                expect_length: 13,
                answer: Ok(csw(tag(1), 0, bot::CSW_PASSED)),
            },
        ];
        let mut bot = Bot::new(Scripted::new(script));
        let inquiry = run(bot.inquiry()).unwrap();
        assert_eq!(inquiry.vendor_str(), "QEMU");
        assert_eq!(inquiry.product_str(), "QEMU HARDDISK");
        assert_eq!(inquiry.device_type, 0);
        bot.transport.assert_done();
    }

    #[test]
    fn read_capacity_decodes() {
        let script = vec![
            Step::Out {
                expect: cbw(tag(1), 8, Direction::In, &scsi::read_capacity10()),
                answer: Ok(()),
            },
            Step::In {
                expect_length: 8,
                answer: Ok(vec![0x00, 0x00, 0x3f, 0xff, 0x00, 0x00, 0x02, 0x00]),
            },
            Step::In {
                expect_length: 13,
                answer: Ok(csw(tag(1), 0, bot::CSW_PASSED)),
            },
        ];
        let mut bot = Bot::new(Scripted::new(script));
        let capacity = run(bot.read_capacity()).unwrap();
        assert_eq!(capacity.last_lba, 16383);
        assert_eq!(capacity.block_size, 512);
        bot.transport.assert_done();
    }

    /// CSW status 1 → REQUEST SENSE → typed error: the post-reset UNIT ATTENTION
    /// rung (TEST UNIT READY is exactly where it shows up live).
    #[test]
    fn command_failure_fetches_sense() {
        let script = vec![
            Step::Out {
                expect: cbw(tag(1), 0, Direction::Out, &scsi::test_unit_ready()),
                answer: Ok(()),
            },
            Step::In {
                expect_length: 13,
                answer: Ok(csw(tag(1), 0, bot::CSW_FAILED)),
            },
            // The engine's own REQUEST SENSE round trip, next tag.
            Step::Out {
                expect: cbw(tag(2), 18, Direction::In, &scsi::request_sense(18)),
                answer: Ok(()),
            },
            Step::In {
                expect_length: 18,
                answer: Ok(fixed_sense(sense_key::UNIT_ATTENTION, 0x29, 0x00)),
            },
            Step::In {
                expect_length: 13,
                answer: Ok(csw(tag(2), 0, bot::CSW_PASSED)),
            },
        ];
        let mut bot = Bot::new(Scripted::new(script));
        let error = run(bot.test_unit_ready()).unwrap_err();
        assert_eq!(
            error,
            MsdError::CommandFailed {
                sense: Some(Sense {
                    key: sense_key::UNIT_ATTENTION,
                    asc: 0x29,
                    ascq: 0
                })
            }
        );
        bot.transport.assert_done();
    }

    /// Mid-data stall: clear the IN halt (the eo9:usb contract's consumer half),
    /// then the CSW names the failure and sense types it — the read-past-capacity
    /// shape on real silicon.
    #[test]
    fn data_in_stall_clears_halt_then_reads_csw() {
        let script = vec![
            Step::Out {
                expect: cbw(tag(1), 512, Direction::In, &scsi::read10(0xffff, 1)),
                answer: Ok(()),
            },
            Step::In {
                expect_length: 512,
                answer: Err(TransportError::Stall),
            },
            Step::ClearHaltIn,
            Step::In {
                expect_length: 13,
                answer: Ok(csw(tag(1), 512, bot::CSW_FAILED)),
            },
            Step::Out {
                expect: cbw(tag(2), 18, Direction::In, &scsi::request_sense(18)),
                answer: Ok(()),
            },
            Step::In {
                expect_length: 18,
                answer: Ok(fixed_sense(sense_key::ILLEGAL_REQUEST, 0x21, 0x00)),
            },
            Step::In {
                expect_length: 13,
                answer: Ok(csw(tag(2), 0, bot::CSW_PASSED)),
            },
        ];
        let mut bot = Bot::new(Scripted::new(script));
        let error = run(bot.read10(0xffff, 1, 512)).unwrap_err();
        assert_eq!(
            error,
            MsdError::CommandFailed {
                sense: Some(Sense {
                    key: sense_key::ILLEGAL_REQUEST,
                    asc: 0x21,
                    ascq: 0
                })
            }
        );
        bot.transport.assert_done();
    }

    /// An OUT-stage stall clears the OUT halt and still reads the CSW.
    #[test]
    fn data_out_stall_clears_halt_then_reads_csw() {
        let payload = vec![0x11u8; 512];
        let script = vec![
            Step::Out {
                expect: cbw(tag(1), 512, Direction::Out, &scsi::write10(3, 1)),
                answer: Ok(()),
            },
            Step::Out {
                expect: payload.clone(),
                answer: Err(TransportError::Stall),
            },
            Step::ClearHaltOut,
            Step::In {
                expect_length: 13,
                answer: Ok(csw(tag(1), 512, bot::CSW_FAILED)),
            },
            Step::Out {
                expect: cbw(tag(2), 18, Direction::In, &scsi::request_sense(18)),
                answer: Ok(()),
            },
            Step::In {
                expect_length: 18,
                answer: Ok(fixed_sense(sense_key::DATA_PROTECT, 0x27, 0x00)),
            },
            Step::In {
                expect_length: 13,
                answer: Ok(csw(tag(2), 0, bot::CSW_PASSED)),
            },
        ];
        let mut bot = Bot::new(Scripted::new(script));
        let error = run(bot.write10(3, 1, 512, &payload)).unwrap_err();
        assert!(matches!(error, MsdError::CommandFailed { sense: Some(s) }
            if s.key == sense_key::DATA_PROTECT));
        bot.transport.assert_done();
    }

    /// Phase error: reset recovery in the §5.3.4 order (reset, clear IN, clear
    /// OUT), typed error.
    #[test]
    fn phase_error_runs_reset_recovery() {
        let script = vec![
            Step::Out {
                expect: cbw(tag(1), 0, Direction::Out, &scsi::test_unit_ready()),
                answer: Ok(()),
            },
            Step::In {
                expect_length: 13,
                answer: Ok(csw(tag(1), 0, bot::CSW_PHASE_ERROR)),
            },
            Step::Reset,
            Step::ClearHaltIn,
            Step::ClearHaltOut,
        ];
        let mut bot = Bot::new(Scripted::new(script));
        let error = run(bot.test_unit_ready()).unwrap_err();
        assert_eq!(error, MsdError::Protocol(ProtocolError::PhaseError));
        bot.transport.assert_done();
    }

    #[test]
    fn bad_csw_signature_runs_reset_recovery() {
        let mut garbage = csw(tag(1), 0, bot::CSW_PASSED);
        garbage[0] = b'X';
        let script = vec![
            Step::Out {
                expect: cbw(tag(1), 0, Direction::Out, &scsi::test_unit_ready()),
                answer: Ok(()),
            },
            Step::In {
                expect_length: 13,
                answer: Ok(garbage),
            },
            Step::Reset,
            Step::ClearHaltIn,
            Step::ClearHaltOut,
        ];
        let mut bot = Bot::new(Scripted::new(script));
        let error = run(bot.test_unit_ready()).unwrap_err();
        assert_eq!(
            error,
            MsdError::Protocol(ProtocolError::BadCsw(CswError::Signature))
        );
        bot.transport.assert_done();
    }

    #[test]
    fn csw_tag_mismatch_runs_reset_recovery() {
        let script = vec![
            Step::Out {
                expect: cbw(tag(1), 0, Direction::Out, &scsi::test_unit_ready()),
                answer: Ok(()),
            },
            Step::In {
                expect_length: 13,
                answer: Ok(csw(0xdead_beef, 0, bot::CSW_PASSED)),
            },
            Step::Reset,
            Step::ClearHaltIn,
            Step::ClearHaltOut,
        ];
        let mut bot = Bot::new(Scripted::new(script));
        let error = run(bot.test_unit_ready()).unwrap_err();
        assert_eq!(
            error,
            MsdError::Protocol(ProtocolError::BadCsw(CswError::Tag {
                expected: tag(1),
                got: 0xdead_beef
            }))
        );
        bot.transport.assert_done();
    }

    /// The §6.7.1 two-try rule: one CSW stall clears the halt and retries; the
    /// retry succeeding completes the command normally.
    #[test]
    fn csw_stall_once_retries_and_passes() {
        let script = vec![
            Step::Out {
                expect: cbw(tag(1), 0, Direction::Out, &scsi::test_unit_ready()),
                answer: Ok(()),
            },
            Step::In {
                expect_length: 13,
                answer: Err(TransportError::Stall),
            },
            Step::ClearHaltIn,
            Step::In {
                expect_length: 13,
                answer: Ok(csw(tag(1), 0, bot::CSW_PASSED)),
            },
        ];
        let mut bot = Bot::new(Scripted::new(script));
        run(bot.test_unit_ready()).unwrap();
        bot.transport.assert_done();
    }

    #[test]
    fn csw_stall_twice_runs_reset_recovery() {
        let script = vec![
            Step::Out {
                expect: cbw(tag(1), 0, Direction::Out, &scsi::test_unit_ready()),
                answer: Ok(()),
            },
            Step::In {
                expect_length: 13,
                answer: Err(TransportError::Stall),
            },
            Step::ClearHaltIn,
            Step::In {
                expect_length: 13,
                answer: Err(TransportError::Stall),
            },
            Step::Reset,
            Step::ClearHaltIn,
            Step::ClearHaltOut,
        ];
        let mut bot = Bot::new(Scripted::new(script));
        let error = run(bot.test_unit_ready()).unwrap_err();
        assert_eq!(error, MsdError::Protocol(ProtocolError::CswStalledTwice));
        bot.transport.assert_done();
    }

    /// A STALLed CBW is unrecoverable short of the reset (BOT §6.6.1).
    #[test]
    fn cbw_stall_runs_reset_recovery() {
        let script = vec![
            Step::Out {
                expect: cbw(tag(1), 0, Direction::Out, &scsi::test_unit_ready()),
                answer: Err(TransportError::Stall),
            },
            Step::Reset,
            Step::ClearHaltIn,
            Step::ClearHaltOut,
        ];
        let mut bot = Bot::new(Scripted::new(script));
        let error = run(bot.test_unit_ready()).unwrap_err();
        assert_eq!(error, MsdError::Protocol(ProtocolError::CbwStalled));
        bot.transport.assert_done();
    }

    /// A NAK-forever device surfaces as the transport's bounded-wait expiry, typed
    /// — and the abandoned exchange forces reset recovery before the NEXT command
    /// touches the wire (the cancel-on-drop re-synchronization).
    #[test]
    fn timeout_propagates_and_next_command_resynchronizes() {
        let script = vec![
            Step::Out {
                expect: cbw(tag(1), 512, Direction::In, &scsi::read10(0, 1)),
                answer: Ok(()),
            },
            Step::In {
                expect_length: 512,
                answer: Err(TransportError::Timeout),
            },
        ];
        let mut bot = Bot::new(Scripted::new(script));
        let error = run(bot.read10(0, 1, 512)).unwrap_err();
        assert_eq!(error, MsdError::Transport(TransportError::Timeout));
        bot.transport.assert_done();

        // The next command re-synchronizes first (reset recovery), then runs.
        bot.transport = Scripted::new(vec![
            Step::Reset,
            Step::ClearHaltIn,
            Step::ClearHaltOut,
            Step::Out {
                expect: cbw(tag(2), 0, Direction::Out, &scsi::test_unit_ready()),
                answer: Ok(()),
            },
            Step::In {
                expect_length: 13,
                answer: Ok(csw(tag(2), 0, bot::CSW_PASSED)),
            },
        ]);
        run(bot.test_unit_ready()).unwrap();
        bot.transport.assert_done();
    }

    /// Short data with a passing CSW (residue route): block reads are exact, so
    /// the engine types the shortfall instead of returning a ragged buffer.
    #[test]
    fn short_data_with_passing_csw_is_typed() {
        let script = vec![
            Step::Out {
                expect: cbw(tag(1), 1024, Direction::In, &scsi::read10(0, 2)),
                answer: Ok(()),
            },
            Step::In {
                expect_length: 1024,
                answer: Ok(vec![0u8; 512]),
            },
            // A ZLP ends the data stage early.
            Step::In {
                expect_length: 512,
                answer: Ok(Vec::new()),
            },
            Step::In {
                expect_length: 13,
                answer: Ok(csw(tag(1), 512, bot::CSW_PASSED)),
            },
        ];
        let mut bot = Bot::new(Scripted::new(script));
        let error = run(bot.read10(0, 2, 512)).unwrap_err();
        assert_eq!(
            error,
            MsdError::ShortData {
                expected: 1024,
                got: 512
            }
        );
        bot.transport.assert_done();
    }

    /// A write the device under-accepted (CSW residue) is typed, never silent.
    #[test]
    fn write_residue_is_typed_short_data() {
        let payload = vec![0u8; 1024];
        let script = vec![
            Step::Out {
                expect: cbw(tag(1), 1024, Direction::Out, &scsi::write10(0, 2)),
                answer: Ok(()),
            },
            Step::Out {
                expect: payload.clone(),
                answer: Ok(()),
            },
            Step::In {
                expect_length: 13,
                answer: Ok(csw(tag(1), 100, bot::CSW_PASSED)),
            },
        ];
        let mut bot = Bot::new(Scripted::new(script));
        let error = run(bot.write10(0, 2, 512, &payload)).unwrap_err();
        assert_eq!(
            error,
            MsdError::ShortData {
                expected: 1024,
                got: 924
            }
        );
        bot.transport.assert_done();
    }

    /// A failed sense fetch decorates the CommandFailed with `None` instead of
    /// masking the primary error.
    #[test]
    fn sense_fetch_failure_keeps_the_primary_error() {
        let script = vec![
            Step::Out {
                expect: cbw(tag(1), 0, Direction::Out, &scsi::test_unit_ready()),
                answer: Ok(()),
            },
            Step::In {
                expect_length: 13,
                answer: Ok(csw(tag(1), 0, bot::CSW_FAILED)),
            },
            Step::Out {
                expect: cbw(tag(2), 18, Direction::In, &scsi::request_sense(18)),
                answer: Ok(()),
            },
            Step::In {
                expect_length: 18,
                answer: Err(TransportError::Timeout),
            },
        ];
        let mut bot = Bot::new(Scripted::new(script));
        let error = run(bot.test_unit_ready()).unwrap_err();
        assert_eq!(error, MsdError::CommandFailed { sense: None });
        bot.transport.assert_done();
    }
}
