//! The session actor on the durable substrate (ADR 0132 §3, §4; S3 of I0,
//! FIG-5194).
//!
//! - [`session`]: the session activation, turn admission, restore from the
//!   `TurnCheckpoint` and the phase runner (V0, then L3), and turn cancel
//!   (L3).
//! - [`session_mail`]: everything the session's mailbox carries, drained
//!   and applied under the epoch on every claim (L3s).

pub mod session;
pub mod session_mail;
