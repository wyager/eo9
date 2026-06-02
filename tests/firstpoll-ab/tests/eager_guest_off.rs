//! Arm A only: the seven-row eager-guest matrix with today's pins (the wall and its
//! variants), verbatim from tests/eo9-integration/tests/eager_guest.rs. Under
//! `first-poll-inline` three of the rows intentionally flip to RETURNED — those
//! expectations live in eager_guest_on.rs.
#![cfg(not(feature = "first-poll-inline"))]

#[path = "../../eo9-integration/tests/eager_guest.rs"]
mod eager_guest;
