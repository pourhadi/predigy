//! Session-layer bookkeeping: monotonic outbound and inbound sequence
//! numbers, plus helpers for verifying incoming messages.
//!
//! This module is intentionally minimal — it's the part of FIX 4.4
//! that needs the most operational care, so we keep the surface
//! small and let the binary drive the higher-level state machine
//! (logon → working → logout). When we add ResendRequest gap fill
//! the additional state belongs here.

use crate::error::Error;
use crate::frame::FieldList;
use crate::tags::{MSG_SEQ_NUM, SENDER_COMP_ID, TARGET_COMP_ID};

#[derive(Debug, Clone)]
pub struct Session {
    pub sender_comp_id: String,
    pub target_comp_id: String,
    /// Next outbound sequence number to use. Bumped after each
    /// successful write.
    pub out_seq: u64,
    /// Next inbound sequence number we expect.
    pub in_seq: u64,
    /// Heartbeat interval the venue agreed to in its Logon response.
    /// Until then it's the value we asked for in our outbound Logon.
    pub heartbeat_secs: u32,
}

impl Session {
    #[must_use]
    pub fn new(
        sender_comp_id: impl Into<String>,
        target_comp_id: impl Into<String>,
        start_out_seq: u64,
        start_in_seq: u64,
        heartbeat_secs: u32,
    ) -> Self {
        Self {
            sender_comp_id: sender_comp_id.into(),
            target_comp_id: target_comp_id.into(),
            out_seq: start_out_seq,
            in_seq: start_in_seq,
            heartbeat_secs,
        }
    }

    /// Get and bump the next outbound sequence number. Called once
    /// per outbound message, atomic with respect to that message's
    /// `MsgSeqNum` (tag 34) field.
    pub fn next_out_seq(&mut self) -> u64 {
        let n = self.out_seq;
        self.out_seq = self.out_seq.checked_add(1).expect("seq overflow");
        n
    }

    /// Validate an inbound message:
    /// - `SenderCompID` (tag 49) must match the venue (our `target`)
    /// - `TargetCompID` (tag 56) must match us (our `sender`)
    /// - `MsgSeqNum` (tag 34) must equal `in_seq` (no gaps)
    ///
    /// Returns `Ok(seq_seen)` on success and bumps `in_seq`. On any
    /// mismatch returns `Err(Session)` — the caller is expected to
    /// drop the connection (until ResendRequest support lands).
    pub fn validate_inbound(&mut self, fields: &FieldList) -> Result<u64, Error> {
        let sender = fields.require(SENDER_COMP_ID, "?")?;
        let target = fields.require(TARGET_COMP_ID, "?")?;
        if sender != self.target_comp_id {
            return Err(Error::Session("inbound SenderCompID mismatch"));
        }
        if target != self.sender_comp_id {
            return Err(Error::Session("inbound TargetCompID mismatch"));
        }
        let seq_str = fields.require(MSG_SEQ_NUM, "?")?;
        let seq: u64 = seq_str.parse().map_err(|_| Error::MalformedTag {
            tag: MSG_SEQ_NUM,
            got: seq_str.to_string(),
        })?;
        if seq != self.in_seq {
            return Err(Error::Session(
                "inbound MsgSeqNum gap (resend not yet supported)",
            ));
        }
        self.in_seq = self.in_seq.checked_add(1).expect("seq overflow");
        Ok(seq)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_inbound(sender: &str, target: &str, seq: u64) -> FieldList {
        let mut f = FieldList::new();
        f.push(SENDER_COMP_ID, sender);
        f.push(TARGET_COMP_ID, target);
        f.push(MSG_SEQ_NUM, seq.to_string());
        f
    }

    #[test]
    fn next_out_seq_is_monotonic() {
        let mut s = Session::new("S", "T", 1, 1, 30);
        assert_eq!(s.next_out_seq(), 1);
        assert_eq!(s.next_out_seq(), 2);
        assert_eq!(s.out_seq, 3);
    }

    #[test]
    fn validate_inbound_happy_path_advances_in_seq() {
        let mut s = Session::new("S", "T", 1, 5, 30);
        let f = make_inbound("T", "S", 5);
        assert_eq!(s.validate_inbound(&f).unwrap(), 5);
        assert_eq!(s.in_seq, 6);
    }

    #[test]
    fn validate_inbound_rejects_compid_mismatch() {
        let mut s = Session::new("S", "T", 1, 1, 30);
        let f = make_inbound("WRONG", "S", 1);
        assert!(matches!(s.validate_inbound(&f), Err(Error::Session(_))));
        assert_eq!(s.in_seq, 1, "in_seq must not advance on rejection");
    }

    #[test]
    fn validate_inbound_rejects_seq_gap() {
        let mut s = Session::new("S", "T", 1, 5, 30);
        let f = make_inbound("T", "S", 7);
        assert!(matches!(s.validate_inbound(&f), Err(Error::Session(_))));
        assert_eq!(s.in_seq, 5);
    }

    #[test]
    fn validate_inbound_rejects_seq_replay() {
        let mut s = Session::new("S", "T", 1, 5, 30);
        let f = make_inbound("T", "S", 4);
        assert!(matches!(s.validate_inbound(&f), Err(Error::Session(_))));
    }
}
