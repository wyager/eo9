//! The enumeration state machine: port reset (OHCI 1.0a port-reset timing), SET_ADDRESS,
//! and the GET_DESCRIPTOR chain, as a pure action/event machine the shells drive and the
//! host tests pin.
//!
//! The machine owns *what happens next and why* (the protocol); the driver owns *how*
//! (issuing port-register writes, building TDs, counting frames). Each step yields an
//! [`Action`]; the driver performs it and feeds back the matching [`Event`]. Timing
//! constants are in milliseconds because the OHCI frame counter (HcFmNumber) ticks once
//! per millisecond — the drivers' only clock.
//!
//! Sequence (USB 2.0 §9.1.2 / §5.5.3):
//! 1. Port reset — the controller times the ~10 ms reset itself and reports PRSC
//!    (OHCI §7.4.4); then 10 ms reset recovery before the device must answer
//!    (USB 2.0 §7.1.7.5 / §9.2.6.2).
//! 2. GET_DESCRIPTOR(device, 8) to the default address — only the descriptor head may
//!    be read before bMaxPacketSize0 is known (§5.5.3).
//! 3. SET_ADDRESS, then 2 ms address-settle (§9.2.6.3: the device gets up to 2 ms to
//!    complete the request after the status stage).
//! 4. GET_DESCRIPTOR(device, 18), GET_DESCRIPTOR(configuration, 9) for wTotalLength,
//!    GET_DESCRIPTOR(configuration, wTotalLength).

use crate::descriptor::{ConfigurationDescriptor, DeviceDescriptor};
use crate::setup::{self, SetupPacket};

/// Reset recovery: 10 ms after reset completes before the device must accept its first
/// setup (USB 2.0 §7.1.7.5).
pub const RESET_RECOVERY_MS: u32 = 10;
/// Address settle: 2 ms after SET_ADDRESS's status stage (USB 2.0 §9.2.6.3).
pub const ADDRESS_SETTLE_MS: u32 = 2;
/// The largest configuration blob the machine accepts (one page-shared buffer; a
/// boot keyboard's is 34 bytes — see the arena's control buffer).
pub const MAX_CONFIG_BYTES: usize = 256;

/// What the driver must do next.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Action {
    /// Write SetPortReset and wait (bounded) for PortResetStatusChange; the controller
    /// times the reset pulse itself.
    ResetPort,
    /// Count `0` field milliseconds on HcFmNumber.
    WaitMs(u32),
    /// Run a control transfer to `address` (endpoint 0, `max_packet` bytes per data
    /// packet) and feed back `ControlDone` with the IN data (empty for OUT/no-data).
    Control {
        address: u8,
        max_packet: u8,
        setup: SetupPacket,
    },
    /// Enumeration finished; read the result off the machine.
    Done,
}

/// What happened (the answer to the last action).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Event<'a> {
    /// The port reset completed (PRSC observed and cleared).
    PortResetComplete,
    /// The requested milliseconds elapsed.
    Waited,
    /// The control transfer completed; `data` is the IN payload (empty for OUT).
    ControlDone { data: &'a [u8] },
}

/// Why enumeration failed (the driver maps transfer failures to its own typed errors
/// before this machine ever sees them — these are protocol-level refusals).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EnumerationError {
    /// A descriptor read returned bytes that do not parse as the expected descriptor.
    MalformedDescriptor,
    /// The configuration blob claims more than [`MAX_CONFIG_BYTES`].
    ConfigurationTooLarge,
    /// An event that does not answer the pending action (a driver bug, surfaced as a
    /// typed error rather than a panic).
    ProtocolMismatch,
}

/// The machine. Drive with [`next_action`](Self::next_action) / [`event`](Self::event).
#[derive(Debug)]
pub struct Enumeration {
    address: u8,
    state: State,
    max_packet_ep0: u8,
    device: [u8; 18],
    config: [u8; MAX_CONFIG_BYTES],
    config_len: usize,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum State {
    ResetIssued,
    ResetRecovery,
    /// GET_DESCRIPTOR(device, 8) to address 0 with the default 8-byte max packet.
    DescriptorHead,
    SetAddress,
    AddressSettle,
    DeviceDescriptor,
    ConfigurationHead,
    ConfigurationFull { total: u16 },
    Done,
}

/// The result: everything usbcheck prints and hidcheck configures from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Enumerated {
    pub address: u8,
    pub max_packet_ep0: u8,
    pub device: DeviceDescriptor,
}

impl Enumeration {
    /// Start enumerating the device on a just-connected port, to be assigned `address`
    /// (1..=127).
    pub fn new(address: u8) -> Enumeration {
        Enumeration {
            address,
            state: State::ResetIssued,
            max_packet_ep0: 8,
            device: [0; 18],
            config: [0; MAX_CONFIG_BYTES],
            config_len: 0,
        }
    }

    /// What the driver should do now.
    pub fn next_action(&self) -> Action {
        match self.state {
            State::ResetIssued => Action::ResetPort,
            State::ResetRecovery => Action::WaitMs(RESET_RECOVERY_MS),
            State::DescriptorHead => Action::Control {
                address: 0,
                max_packet: 8,
                setup: setup::get_descriptor(setup::descriptor_type::DEVICE, 0, 8),
            },
            State::SetAddress => Action::Control {
                address: 0,
                max_packet: self.max_packet_ep0,
                setup: setup::set_address(self.address),
            },
            State::AddressSettle => Action::WaitMs(ADDRESS_SETTLE_MS),
            State::DeviceDescriptor => Action::Control {
                address: self.address,
                max_packet: self.max_packet_ep0,
                setup: setup::get_descriptor(setup::descriptor_type::DEVICE, 0, 18),
            },
            State::ConfigurationHead => Action::Control {
                address: self.address,
                max_packet: self.max_packet_ep0,
                setup: setup::get_descriptor(setup::descriptor_type::CONFIGURATION, 0, 9),
            },
            State::ConfigurationFull { total } => Action::Control {
                address: self.address,
                max_packet: self.max_packet_ep0,
                setup: setup::get_descriptor(setup::descriptor_type::CONFIGURATION, 0, total),
            },
            State::Done => Action::Done,
        }
    }

    /// Feed back the outcome of the last action; the next call to `next_action` says
    /// what to do next.
    pub fn event(&mut self, event: Event<'_>) -> Result<(), EnumerationError> {
        match (self.state, event) {
            (State::ResetIssued, Event::PortResetComplete) => {
                self.state = State::ResetRecovery;
            }
            (State::ResetRecovery, Event::Waited) => {
                self.state = State::DescriptorHead;
            }
            (State::DescriptorHead, Event::ControlDone { data }) => {
                self.max_packet_ep0 = DeviceDescriptor::max_packet_size_from_head(data)
                    .ok_or(EnumerationError::MalformedDescriptor)?;
                self.state = State::SetAddress;
            }
            (State::SetAddress, Event::ControlDone { .. }) => {
                self.state = State::AddressSettle;
            }
            (State::AddressSettle, Event::Waited) => {
                self.state = State::DeviceDescriptor;
            }
            (State::DeviceDescriptor, Event::ControlDone { data }) => {
                if DeviceDescriptor::parse(data).is_none() {
                    return Err(EnumerationError::MalformedDescriptor);
                }
                self.device[..18].copy_from_slice(&data[..18]);
                self.state = State::ConfigurationHead;
            }
            (State::ConfigurationHead, Event::ControlDone { data }) => {
                let configuration = ConfigurationDescriptor::parse(data)
                    .ok_or(EnumerationError::MalformedDescriptor)?;
                if configuration.total_length as usize > MAX_CONFIG_BYTES {
                    return Err(EnumerationError::ConfigurationTooLarge);
                }
                self.state = State::ConfigurationFull {
                    total: configuration.total_length,
                };
            }
            (State::ConfigurationFull { .. }, Event::ControlDone { data }) => {
                // A device may legally answer short; keep what arrived.
                let length = data.len().min(MAX_CONFIG_BYTES);
                self.config[..length].copy_from_slice(&data[..length]);
                self.config_len = length;
                self.state = State::Done;
            }
            _ => return Err(EnumerationError::ProtocolMismatch),
        }
        Ok(())
    }

    /// The enumerated identity, once `next_action` answers `Done`.
    pub fn result(&self) -> Option<Enumerated> {
        if self.state != State::Done {
            return None;
        }
        Some(Enumerated {
            address: self.address,
            max_packet_ep0: self.max_packet_ep0,
            device: DeviceDescriptor::parse(&self.device)?,
        })
    }

    /// The raw device descriptor bytes (valid once done).
    pub fn device_bytes(&self) -> &[u8] {
        &self.device
    }

    /// The full configuration blob (valid once done).
    pub fn configuration(&self) -> &[u8] {
        &self.config[..self.config_len]
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::setup::descriptor_type;

    const DEVICE: [u8; 18] = [
        18, 1, 0x00, 0x02, 0, 0, 0, 8, 0x27, 0x06, 0x01, 0x00, 0x00, 0x00, 1, 4, 5, 1,
    ];
    const CONFIG_HEAD: [u8; 9] = [9, 2, 34, 0, 1, 1, 0, 0xe0, 25];

    fn config_full() -> [u8; 34] {
        let mut blob = [0u8; 34];
        blob[..9].copy_from_slice(&CONFIG_HEAD);
        blob[9..18].copy_from_slice(&[9, 4, 0, 0, 1, 3, 1, 1, 0]);
        blob[18..27].copy_from_slice(&[9, 0x21, 0x11, 0x01, 0, 1, 0x22, 63, 0]);
        blob[27..34].copy_from_slice(&[7, 5, 0x81, 3, 8, 0, 10]);
        blob
    }

    #[test]
    fn the_happy_path_walks_the_whole_chain() {
        let mut machine = Enumeration::new(2);

        // 1. Port reset, then the 10 ms recovery.
        assert_eq!(machine.next_action(), Action::ResetPort);
        machine.event(Event::PortResetComplete).unwrap();
        assert_eq!(machine.next_action(), Action::WaitMs(RESET_RECOVERY_MS));
        machine.event(Event::Waited).unwrap();

        // 2. The 8-byte descriptor head to address 0 with the default max packet.
        let Action::Control {
            address: 0,
            max_packet: 8,
            setup,
        } = machine.next_action()
        else {
            panic!("expected the head read");
        };
        assert_eq!(
            setup,
            crate::setup::get_descriptor(descriptor_type::DEVICE, 0, 8)
        );
        machine
            .event(Event::ControlDone {
                data: &DEVICE[..8],
            })
            .unwrap();

        // 3. SET_ADDRESS still goes to address 0; then the 2 ms settle.
        let Action::Control {
            address: 0, setup, ..
        } = machine.next_action()
        else {
            panic!("expected SET_ADDRESS");
        };
        assert_eq!(setup, crate::setup::set_address(2));
        machine.event(Event::ControlDone { data: &[] }).unwrap();
        assert_eq!(machine.next_action(), Action::WaitMs(ADDRESS_SETTLE_MS));
        machine.event(Event::Waited).unwrap();

        // 4. Full device descriptor, configuration head, full configuration — all on
        // the new address.
        for (expected, answer) in [
            (
                crate::setup::get_descriptor(descriptor_type::DEVICE, 0, 18),
                &DEVICE[..],
            ),
            (
                crate::setup::get_descriptor(descriptor_type::CONFIGURATION, 0, 9),
                &CONFIG_HEAD[..],
            ),
            (
                crate::setup::get_descriptor(descriptor_type::CONFIGURATION, 0, 34),
                &config_full()[..],
            ),
        ] {
            let Action::Control {
                address: 2, setup, ..
            } = machine.next_action()
            else {
                panic!("expected a control transfer on the assigned address");
            };
            assert_eq!(setup, expected);
            machine.event(Event::ControlDone { data: answer }).unwrap();
        }

        assert_eq!(machine.next_action(), Action::Done);
        let result = machine.result().unwrap();
        assert_eq!(result.address, 2);
        assert_eq!(result.max_packet_ep0, 8);
        assert_eq!(result.device.vendor_id, 0x0627);
        assert_eq!(machine.configuration(), &config_full());
        // The blob parses down to the boot keyboard endpoint.
        let boot = crate::descriptor::find_boot_interface(machine.configuration()).unwrap();
        assert_eq!(boot.endpoint.address, 0x81);
    }

    #[test]
    fn max_packet_from_the_head_redirects_later_reads() {
        // A 64-byte-ep0 device: the head read learns 64 and every later control
        // action carries it.
        let mut machine = Enumeration::new(1);
        machine.event(Event::PortResetComplete).unwrap();
        machine.event(Event::Waited).unwrap();
        let mut head = DEVICE;
        head[7] = 64;
        machine
            .event(Event::ControlDone { data: &head[..8] })
            .unwrap();
        let Action::Control { max_packet, .. } = machine.next_action() else {
            panic!("expected SET_ADDRESS");
        };
        assert_eq!(max_packet, 64);
    }

    #[test]
    fn refusals_are_typed_never_panics() {
        // A wrong event for the state.
        let mut machine = Enumeration::new(1);
        assert_eq!(
            machine.event(Event::Waited),
            Err(EnumerationError::ProtocolMismatch)
        );

        // A garbage descriptor head.
        let mut machine = Enumeration::new(1);
        machine.event(Event::PortResetComplete).unwrap();
        machine.event(Event::Waited).unwrap();
        assert_eq!(
            machine.event(Event::ControlDone {
                data: &[0u8; 8]
            }),
            Err(EnumerationError::MalformedDescriptor)
        );

        // A configuration that claims more than the buffer.
        let mut machine = Enumeration::new(1);
        machine.event(Event::PortResetComplete).unwrap();
        machine.event(Event::Waited).unwrap();
        machine
            .event(Event::ControlDone {
                data: &DEVICE[..8],
            })
            .unwrap();
        machine.event(Event::ControlDone { data: &[] }).unwrap();
        machine.event(Event::Waited).unwrap();
        machine.event(Event::ControlDone { data: &DEVICE }).unwrap();
        let mut huge = CONFIG_HEAD;
        huge[2] = 0xff;
        huge[3] = 0x01;
        assert_eq!(
            machine.event(Event::ControlDone { data: &huge }),
            Err(EnumerationError::ConfigurationTooLarge)
        );

        // No result before done.
        assert_eq!(Enumeration::new(1).result(), None);
    }
}
