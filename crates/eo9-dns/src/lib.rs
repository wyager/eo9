//! Minimal DNS wire encoding and parsing: one A/IN question out, the first A record
//! back. The pure core shared by the example programs — `l4check` (the transport-layer
//! probe's resolver round-trip) and `curl` (resolving the URL host) — factored out of
//! the wasm components so the byte arithmetic is host-tested (`cargo test -p eo9-dns`;
//! the eo9-rtl8125 / eo9-eofs precedent).
//!
//! Scope is deliberately exactly what those programs need: encode a single-question
//! recursion-desired A/IN query, and pull the first A record out of a reply (walking
//! compression pointers, RFC 1035 §4.1.4). Anything else of DNS — other record types,
//! multiple questions, EDNS, truncation retry over TCP — is unimplemented and
//! documented as such.

#![cfg_attr(not(test), no_std)]

extern crate alloc;

use alloc::vec::Vec;

/// Why a name cannot be encoded into a DNS question (RFC 1035 §2.3.4 wire limits).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NameError {
    /// A label was empty: a leading, trailing, or doubled dot — or no labels at all.
    EmptyLabel,
    /// A label exceeded the 63-byte wire limit.
    LabelTooLong,
    /// The encoded name would exceed the 255-byte wire limit.
    NameTooLong,
}

/// What a reply carried, once it parses as an answer to our query.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reply {
    /// The first A record in the answer section.
    A(u8, u8, u8, u8),
    /// An answer arrived but no A record could be extracted from it; carries the
    /// header's answer count.
    Answered(u16),
}

/// A reply that is an answer to our query, but a failed one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReplyError {
    /// The resolver answered with a non-zero response code.
    Rcode(u8),
    /// The resolver answered cleanly (rcode 0) but with zero answer records.
    NoRecords,
}

/// A DNS query: header asking for recursion, one A/IN question for the dot-split
/// `labels`, carrying `id` (the reply must echo it back).
///
/// The bytes are exactly what `l4check` has always sent — the header literals are the
/// pinned wire encoding (see the crate tests).
pub fn query<'a>(id: u16, labels: impl IntoIterator<Item = &'a str>) -> Result<Vec<u8>, NameError> {
    let mut packet = Vec::with_capacity(32);
    packet.extend_from_slice(&id.to_be_bytes()); // id
    packet.extend_from_slice(&[0x01, 0x00]); // flags: recursion desired
    packet.extend_from_slice(&[0x00, 0x01]); // one question
    packet.extend_from_slice(&[0x00, 0x00, 0x00, 0x00, 0x00, 0x00]); // no other records

    let name_starts = packet.len();
    let mut label_count = 0usize;
    for label in labels {
        if label.is_empty() {
            return Err(NameError::EmptyLabel);
        }
        if label.len() > 63 {
            return Err(NameError::LabelTooLong);
        }
        packet.push(label.len() as u8);
        packet.extend_from_slice(label.as_bytes());
        label_count += 1;
    }
    if label_count == 0 {
        return Err(NameError::EmptyLabel);
    }
    packet.push(0); // end of name
    if packet.len() - name_starts > 255 {
        return Err(NameError::NameTooLong);
    }

    packet.extend_from_slice(&[0x00, 0x01]); // type A
    packet.extend_from_slice(&[0x00, 0x01]); // class IN
    Ok(packet)
}

/// The end of a (possibly compression-pointed) DNS name starting at `at`.
fn skip_name(packet: &[u8], mut at: usize) -> Option<usize> {
    loop {
        let len = *packet.get(at)? as usize;
        if len == 0 {
            return Some(at + 1);
        }
        if len & 0xc0 == 0xc0 {
            return Some(at + 2); // compression pointer: two bytes, then done
        }
        at += 1 + len;
    }
}

/// What the resolver said: the first A record if one can be extracted, otherwise a
/// summary of the answer header. `None` if this datagram is not an answer to a query
/// carrying `id` (wrong id, or not a response) — callers keep receiving.
pub fn parse_reply(packet: &[u8], id: u16) -> Option<Result<Reply, ReplyError>> {
    if packet.len() < 12 {
        return None;
    }
    if packet[0..2] != id.to_be_bytes() {
        return None; // not our query
    }
    if packet[2] & 0x80 == 0 {
        return None; // not a response
    }
    let rcode = packet[3] & 0x0f;
    if rcode != 0 {
        return Some(Err(ReplyError::Rcode(rcode)));
    }
    let questions = u16::from_be_bytes([packet[4], packet[5]]) as usize;
    let answer_count = u16::from_be_bytes([packet[6], packet[7]]);
    let answers = answer_count as usize;
    if answers == 0 {
        return Some(Err(ReplyError::NoRecords));
    }

    // Walk past the question section, then look for the first A/IN answer.
    let mut at = 12;
    for _ in 0..questions {
        at = match skip_name(packet, at) {
            Some(next) => next + 4, // qtype + qclass
            None => return Some(Ok(Reply::Answered(answer_count))),
        };
    }
    for _ in 0..answers {
        let after_name = match skip_name(packet, at) {
            Some(next) => next,
            None => break,
        };
        if packet.len() < after_name + 10 {
            break;
        }
        let rtype = u16::from_be_bytes([packet[after_name], packet[after_name + 1]]);
        let rdlength =
            u16::from_be_bytes([packet[after_name + 8], packet[after_name + 9]]) as usize;
        let rdata = after_name + 10;
        if rtype == 1 && rdlength == 4 && packet.len() >= rdata + 4 {
            return Some(Ok(Reply::A(
                packet[rdata],
                packet[rdata + 1],
                packet[rdata + 2],
                packet[rdata + 3],
            )));
        }
        at = rdata + rdlength;
    }
    Some(Ok(Reply::Answered(answer_count)))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The id l4check has always used; the encoder output is pinned byte-for-byte
    /// against the bytes the example built by hand before the factor-out.
    const ID: u16 = 0xe09;

    #[test]
    fn query_bytes_are_the_pinned_l4check_encoding() {
        let packet = query(ID, ["example", "com"]).expect("a valid name");
        #[rustfmt::skip]
        let expected: &[u8] = &[
            0x0e, 0x09,             // id
            0x01, 0x00,             // flags: recursion desired
            0x00, 0x01,             // one question
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, // no other records
            7, b'e', b'x', b'a', b'm', b'p', b'l', b'e',
            3, b'c', b'o', b'm',
            0,                      // end of name
            0x00, 0x01,             // type A
            0x00, 0x01,             // class IN
        ];
        assert_eq!(packet, expected);
    }

    #[test]
    fn query_refuses_bad_names() {
        assert_eq!(query(ID, []), Err(NameError::EmptyLabel));
        assert_eq!(
            query(ID, ["example", "", "com"]),
            Err(NameError::EmptyLabel)
        );
        let long_label = "x".repeat(64);
        assert_eq!(
            query(ID, [long_label.as_str()]),
            Err(NameError::LabelTooLong)
        );
        let label = "y".repeat(63);
        let labels = [label.as_str(); 4]; // 4 * (1 + 63) + 1 = 257 > 255
        assert_eq!(query(ID, labels), Err(NameError::NameTooLong));
        // 63 + 63 + 63 + 61 encodes to exactly 255 with the length bytes and the
        // terminator: the limit itself is fine.
        let last = "z".repeat(61);
        let ok = [
            label.as_str(),
            label.as_str(),
            label.as_str(),
            last.as_str(),
        ];
        assert!(query(ID, ok).is_ok());
    }

    /// A reply to our query: header, the echoed question, then the given answer
    /// section bytes.
    fn reply(flags: [u8; 2], answer_count: u16, answers: &[u8]) -> Vec<u8> {
        let mut packet = Vec::new();
        packet.extend_from_slice(&ID.to_be_bytes());
        packet.extend_from_slice(&flags);
        packet.extend_from_slice(&[0x00, 0x01]); // one question
        packet.extend_from_slice(&answer_count.to_be_bytes());
        packet.extend_from_slice(&[0x00, 0x00, 0x00, 0x00]); // no authority/additional
        // The echoed question: example.com A/IN.
        packet.extend_from_slice(&[7]);
        packet.extend_from_slice(b"example");
        packet.extend_from_slice(&[3]);
        packet.extend_from_slice(b"com");
        packet.extend_from_slice(&[0, 0x00, 0x01, 0x00, 0x01]);
        packet.extend_from_slice(answers);
        packet
    }

    /// One answer record with a compression-pointer name (to the question at 12).
    fn a_record(address: [u8; 4]) -> Vec<u8> {
        let mut record = Vec::new();
        record.extend_from_slice(&[0xc0, 0x0c]); // name: pointer to offset 12
        record.extend_from_slice(&[0x00, 0x01]); // type A
        record.extend_from_slice(&[0x00, 0x01]); // class IN
        record.extend_from_slice(&[0x00, 0x00, 0x0e, 0x10]); // ttl
        record.extend_from_slice(&[0x00, 0x04]); // rdlength
        record.extend_from_slice(&address);
        record
    }

    #[test]
    fn parse_extracts_the_first_a_record() {
        let packet = reply([0x81, 0x80], 1, &a_record([93, 184, 215, 14]));
        assert_eq!(
            parse_reply(&packet, ID),
            Some(Ok(Reply::A(93, 184, 215, 14)))
        );
    }

    #[test]
    fn parse_skips_a_leading_cname_to_reach_the_a_record() {
        // A CNAME (type 5) first, then the A record — the first-A walk must step over
        // the CNAME's rdata by its rdlength.
        let mut answers = Vec::new();
        answers.extend_from_slice(&[0xc0, 0x0c, 0x00, 0x05, 0x00, 0x01]); // name, CNAME, IN
        answers.extend_from_slice(&[0x00, 0x00, 0x0e, 0x10]); // ttl
        answers.extend_from_slice(&[0x00, 0x02, 0xc0, 0x0c]); // rdlength 2: a pointer
        answers.extend_from_slice(&a_record([10, 0, 0, 7]));
        let packet = reply([0x81, 0x80], 2, &answers);
        assert_eq!(parse_reply(&packet, ID), Some(Ok(Reply::A(10, 0, 0, 7))));
    }

    #[test]
    fn parse_summarizes_answers_without_an_a_record() {
        // One AAAA-typed (28) record: answered, but nothing extractable.
        let mut record = Vec::new();
        record.extend_from_slice(&[0xc0, 0x0c, 0x00, 0x1c, 0x00, 0x01]);
        record.extend_from_slice(&[0x00, 0x00, 0x0e, 0x10]);
        record.extend_from_slice(&[0x00, 0x10]);
        record.extend_from_slice(&[0u8; 16]);
        let packet = reply([0x81, 0x80], 1, &record);
        assert_eq!(parse_reply(&packet, ID), Some(Ok(Reply::Answered(1))));
    }

    #[test]
    fn parse_reports_rcode_and_empty_answers() {
        let nxdomain = reply([0x81, 0x83], 0, &[]);
        assert_eq!(parse_reply(&nxdomain, ID), Some(Err(ReplyError::Rcode(3))));
        let empty = reply([0x81, 0x80], 0, &[]);
        assert_eq!(parse_reply(&empty, ID), Some(Err(ReplyError::NoRecords)));
    }

    #[test]
    fn parse_ignores_what_is_not_our_answer() {
        // Wrong id.
        let other = {
            let mut packet = reply([0x81, 0x80], 1, &a_record([1, 2, 3, 4]));
            packet[0] = 0xff;
            packet
        };
        assert_eq!(parse_reply(&other, ID), None);
        // Our id, but a query (QR clear), not a response.
        let echo = query(ID, ["example", "com"]).expect("a valid name");
        assert_eq!(parse_reply(&echo, ID), None);
        // Too short to carry a header.
        assert_eq!(parse_reply(&[0x0e, 0x09, 0x80], ID), None);
    }

    #[test]
    fn parse_survives_truncated_answer_sections() {
        // The header promises an answer the packet does not carry: summary, not panic.
        let packet = reply([0x81, 0x80], 1, &[0xc0, 0x0c, 0x00]);
        assert_eq!(parse_reply(&packet, ID), Some(Ok(Reply::Answered(1))));
    }
}
