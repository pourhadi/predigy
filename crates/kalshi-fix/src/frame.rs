//! FIX 4.4 wire framing: SOH-delimited `tag=value` pairs with a
//! 3-digit checksum trailer.
//!
//! ## The shape
//!
//! Every message is `8=FIX.4.4 | 9=<bodylen> | 35=<type> | … | 10=<sum>`,
//! pipe-separated by SOH (`0x01`) bytes:
//!
//! ```text
//! 8=FIX.4.4\x019=NN\x0135=A\x01...\x0110=NNN\x01
//! ```
//!
//! - `9` (BodyLength) = byte count from the start of `35=…` up to but
//!   not including `10=…\x01`.
//! - `10` (Checksum) = sum of all bytes preceding the checksum field
//!   (including the SOH before it), `mod 256`, formatted to three
//!   ASCII digits.
//!
//! ## What this module provides
//!
//! - [`FieldList`]: a `Vec<(u32, String)>` newtype for building message
//!   bodies in tag order, with header (`8`, `9`, `35`) and trailer
//!   (`10`) computed automatically by [`encode`].
//! - [`decode_message`]: scans bytes for one complete framed message,
//!   verifies the checksum, returns the parsed fields.
//!
//! Higher-level "build a NewOrderSingle from a `predigy_core::Order`"
//! lives in `crate::messages`; this module is the byte-level layer
//! and is the most security-relevant part of the crate (a corrupt
//! message slipping past the checker could produce a duplicate
//! order). Tested via round-trip + corruption-detection cases.

use crate::error::Error;
use crate::tags::{BEGIN_STRING, BEGIN_STRING_VALUE, BODY_LENGTH, CHECKSUM, MSG_TYPE, SOH};
use std::fmt;

/// Ordered tag/value list. Order matters on the wire — FIX requires
/// tags 8, 9, 35 first and 10 last (handled by [`encode`]); within
/// the body, ordering is application-defined but stable within a
/// message type.
#[derive(Debug, Default, Clone)]
pub struct FieldList(pub Vec<(u32, String)>);

impl FieldList {
    #[must_use]
    pub fn new() -> Self {
        Self(Vec::new())
    }

    pub fn push(&mut self, tag: u32, value: impl Into<String>) -> &mut Self {
        self.0.push((tag, value.into()));
        self
    }

    /// Find the first value for `tag`, or `None`.
    #[must_use]
    pub fn get(&self, tag: u32) -> Option<&str> {
        self.0
            .iter()
            .find_map(|(t, v)| if *t == tag { Some(v.as_str()) } else { None })
    }

    /// Required-tag accessor with structured error.
    pub fn require(&self, tag: u32, msg_type: &str) -> Result<&str, Error> {
        self.get(tag).ok_or_else(|| Error::MissingTag {
            tag,
            msg_type: msg_type.to_string(),
        })
    }
}

impl fmt::Display for FieldList {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for (i, (t, v)) in self.0.iter().enumerate() {
            if i > 0 {
                write!(f, "|")?;
            }
            write!(f, "{t}={v}")?;
        }
        Ok(())
    }
}

/// Encode `body_fields` (must include tag 35 first) into a complete
/// FIX message with header (`8=`, `9=`) and checksum trailer (`10=`).
/// `body_fields` must NOT contain tags 8, 9, or 10 — those are added
/// here.
pub fn encode(body_fields: &[(u32, String)]) -> Vec<u8> {
    // Build the body bytes (everything from tag 35 onward, before
    // the checksum trailer). Each field is `tag=value\x01`.
    let mut body = Vec::with_capacity(256);
    for (tag, value) in body_fields {
        debug_assert!(
            *tag != BEGIN_STRING && *tag != BODY_LENGTH && *tag != CHECKSUM,
            "encode: caller must not include tags 8/9/10 in body"
        );
        body.extend(tag.to_string().as_bytes());
        body.push(b'=');
        body.extend(value.as_bytes());
        body.push(SOH);
    }
    let body_len = body.len();

    // Header: 8=FIX.4.4\x019=<bodylen>\x01
    let mut out = Vec::with_capacity(body.len() + 32);
    out.extend(format!("{BEGIN_STRING}=").as_bytes());
    out.extend(BEGIN_STRING_VALUE.as_bytes());
    out.push(SOH);
    out.extend(format!("{BODY_LENGTH}={body_len}").as_bytes());
    out.push(SOH);
    out.extend(&body);

    // Checksum: sum of all bytes so far mod 256, three-digit decimal.
    let sum: u32 = out.iter().map(|&b| u32::from(b)).sum();
    let checksum = (sum % 256) as u8;
    out.extend(format!("{CHECKSUM}={checksum:03}").as_bytes());
    out.push(SOH);
    out
}

/// Find the next complete FIX message in `buf` starting at the
/// beginning. Returns `(message_fields, bytes_consumed)` if a complete
/// message is present; `Ok(None)` if more bytes are needed.
///
/// Verifies the checksum. A frame whose checksum doesn't match is
/// returned as an `Err(Checksum)` — the caller should drop the
/// connection rather than skip past, since a bad checksum likely
/// indicates desync.
pub fn decode_message(buf: &[u8]) -> Result<Option<(FieldList, usize)>, Error> {
    // Look for the header start tag `8=FIX.4.4`.
    let Some(start_idx) = find_subslice(buf, b"8=FIX.4.4\x01") else {
        // We might just not have enough bytes yet. Wait.
        return Ok(None);
    };
    if start_idx > 0 {
        return Err(Error::Frame(format!(
            "garbage at start of buffer ({start_idx} bytes before 8=FIX.4.4)"
        )));
    }
    // Parse `9=<bodylen>\x01` immediately after the header. If the
    // buffer ends exactly at the header (or has only a partial body
    // length field), bail to "need more bytes" rather than erroring
    // — the caller will re-call when more arrives.
    let header_end = b"8=FIX.4.4\x01".len();
    if buf.len() < header_end + 4 {
        // Smallest possible `9=<digit>\x01` is 4 bytes.
        return Ok(None);
    }
    if !buf[header_end..].contains(&SOH) {
        return Ok(None);
    }
    let (body_len_value, body_len_consumed) = parse_field(&buf[header_end..])?;
    if body_len_value.0 != BODY_LENGTH {
        return Err(Error::Frame(format!(
            "expected tag 9 after 8=FIX.4.4, got {}",
            body_len_value.0
        )));
    }
    let body_len: usize = body_len_value
        .1
        .parse()
        .map_err(|_| Error::Frame(format!("body length {:?} not a number", body_len_value.1)))?;
    let body_start = header_end + body_len_consumed;
    let body_end = body_start + body_len;
    if buf.len() < body_end + 7 {
        // Need at least `10=NNN\x01` (7 bytes) after the body.
        return Ok(None);
    }
    let trailer = &buf[body_end..body_end + 7];
    if !trailer.starts_with(b"10=") || trailer[6] != SOH {
        return Err(Error::Frame(format!(
            "expected `10=NNN\\x01` trailer, got {:?}",
            std::str::from_utf8(trailer).unwrap_or("<non-utf8>")
        )));
    }
    let cksum_str = std::str::from_utf8(&trailer[3..6])
        .map_err(|_| Error::Frame("checksum bytes not utf8".into()))?;
    let claimed: u8 = cksum_str
        .parse()
        .map_err(|_| Error::Frame(format!("checksum {cksum_str:?} not a number")))?;
    let computed_sum: u32 = buf[..body_end].iter().map(|&b| u32::from(b)).sum();
    let computed = (computed_sum % 256) as u8;
    if claimed != computed {
        return Err(Error::Checksum {
            expected: computed,
            got: claimed,
        });
    }
    // Parse all fields (8, 9, 35, ..., excluding 10).
    let mut fields = FieldList::new();
    let mut cursor = 0usize;
    while cursor < body_end {
        let (kv, consumed) = parse_field(&buf[cursor..])?;
        // Skip 8 and 9 — they're framing, not application fields.
        if kv.0 != BEGIN_STRING && kv.0 != BODY_LENGTH {
            fields.0.push(kv);
        }
        cursor += consumed;
    }
    Ok(Some((fields, body_end + 7)))
}

/// Parse one `tag=value\x01` field from the start of `buf`. Returns
/// `((tag, value), bytes_consumed)`.
fn parse_field(buf: &[u8]) -> Result<((u32, String), usize), Error> {
    let eq_pos = buf
        .iter()
        .position(|&b| b == b'=')
        .ok_or_else(|| Error::Frame("no `=` in field".into()))?;
    let soh_pos = buf
        .iter()
        .position(|&b| b == SOH)
        .ok_or_else(|| Error::Frame("no SOH terminator in field".into()))?;
    if eq_pos > soh_pos {
        return Err(Error::Frame("`=` after SOH".into()));
    }
    let tag_str = std::str::from_utf8(&buf[..eq_pos])
        .map_err(|_| Error::Frame("tag bytes not utf8".into()))?;
    let tag: u32 = tag_str
        .parse()
        .map_err(|_| Error::Frame(format!("tag {tag_str:?} not a number")))?;
    let value = std::str::from_utf8(&buf[eq_pos + 1..soh_pos])
        .map_err(|_| Error::Frame("value bytes not utf8".into()))?
        .to_string();
    Ok(((tag, value), soh_pos + 1))
}

fn find_subslice(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() || haystack.len() < needle.len() {
        return None;
    }
    haystack.windows(needle.len()).position(|w| w == needle)
}

/// Pretty-print a FIX message with `|` instead of SOH. Useful for
/// log lines and panics.
#[must_use]
pub fn pretty(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len());
    for &b in bytes {
        if b == SOH {
            out.push('|');
        } else {
            out.push(b as char);
        }
    }
    out
}

/// Body wrapper that owns the fields a message carries between the
/// header (8/9/35) and trailer (10). Used by `crate::messages` to
/// build typed messages and feed them through `encode`.
///
/// (The 35 tag IS included in `body_with_msg_type` so callers can
/// pass a single Vec directly to `encode`. `encode` itself doesn't
/// add the 35 tag.)
#[must_use]
pub fn body_with_msg_type(msg_type: &str, fields: &mut Vec<(u32, String)>) -> Vec<(u32, String)> {
    let mut out = Vec::with_capacity(fields.len() + 1);
    out.push((MSG_TYPE, msg_type.to_string()));
    out.append(fields);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip_minimal_logon() {
        // Build a minimal Logon-shaped message body, encode it, decode
        // it, expect the same fields back (minus 8/9 framing).
        let body = body_with_msg_type(
            "A",
            &mut vec![
                (49, "SENDER".to_string()),
                (56, "TARGET".to_string()),
                (34, "1".to_string()),
                (52, "20260504-12:34:56".to_string()),
                (98, "0".to_string()),
                (108, "30".to_string()),
            ],
        );
        let bytes = encode(&body);
        let (decoded, consumed) = decode_message(&bytes).unwrap().expect("complete frame");
        assert_eq!(consumed, bytes.len());
        // Decoded should contain 35, 49, 56, 34, 52, 98, 108 (8/9 stripped).
        assert_eq!(decoded.get(35).unwrap(), "A");
        assert_eq!(decoded.get(49).unwrap(), "SENDER");
        assert_eq!(decoded.get(108).unwrap(), "30");
    }

    #[test]
    fn checksum_is_three_digits_mod_256() {
        let body = body_with_msg_type(
            "0",
            &mut vec![
                (49, "S".to_string()),
                (56, "T".to_string()),
                (34, "1".to_string()),
            ],
        );
        let bytes = encode(&body);
        // Trailing SOH; just before that is `10=NNN`.
        assert_eq!(bytes.last(), Some(&SOH));
        let trailer_start = bytes.len() - 7;
        assert_eq!(&bytes[trailer_start..trailer_start + 3], b"10=");
        // The 3 chars must be ASCII digits.
        for &c in &bytes[trailer_start + 3..trailer_start + 6] {
            assert!(c.is_ascii_digit(), "checksum digit {c:?} not 0-9");
        }
    }

    #[test]
    fn corrupt_checksum_is_rejected() {
        let body = body_with_msg_type(
            "0",
            &mut vec![
                (49, "S".to_string()),
                (56, "T".to_string()),
                (34, "1".to_string()),
            ],
        );
        let mut bytes = encode(&body);
        // Flip a checksum digit.
        let cksum_pos = bytes.len() - 4;
        bytes[cksum_pos] = if bytes[cksum_pos] == b'0' { b'1' } else { b'0' };
        let err = decode_message(&bytes).unwrap_err();
        assert!(matches!(err, Error::Checksum { .. }));
    }

    #[test]
    fn decode_returns_none_on_partial_frame() {
        let body = body_with_msg_type(
            "0",
            &mut vec![
                (49, "S".to_string()),
                (56, "T".to_string()),
                (34, "1".to_string()),
            ],
        );
        let bytes = encode(&body);
        assert!(decode_message(&bytes[..bytes.len() - 1]).unwrap().is_none());
        assert!(decode_message(&bytes[..10]).unwrap().is_none());
    }

    #[test]
    fn decode_garbage_before_header_errors() {
        let body = body_with_msg_type(
            "0",
            &mut vec![
                (49, "S".to_string()),
                (56, "T".to_string()),
                (34, "1".to_string()),
            ],
        );
        let mut bytes = b"GARBAGE".to_vec();
        bytes.extend(encode(&body));
        let err = decode_message(&bytes).unwrap_err();
        assert!(matches!(err, Error::Frame(_)));
    }

    #[test]
    fn pretty_replaces_soh_with_pipe() {
        let body = body_with_msg_type(
            "0",
            &mut vec![
                (49, "S".to_string()),
                (56, "T".to_string()),
                (34, "1".to_string()),
            ],
        );
        let bytes = encode(&body);
        let s = pretty(&bytes);
        assert!(s.starts_with("8=FIX.4.4|9="));
        assert!(s.contains("|35=0|"));
        assert!(s.ends_with('|'));
    }

    #[test]
    fn require_returns_missing_tag_on_absent() {
        let mut f = FieldList::new();
        f.push(35, "A");
        let err = f.require(11, "A").unwrap_err();
        match err {
            Error::MissingTag { tag, msg_type } => {
                assert_eq!(tag, 11);
                assert_eq!(msg_type, "A");
            }
            other => panic!("expected MissingTag, got {other:?}"),
        }
    }
}
