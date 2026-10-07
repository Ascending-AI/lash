//! The session actor on the durable substrate (ADR 0132 §3, §4; S3 of I0,
//! FIG-5194).
//!
//! - [`session`]: the session activation, turn admission, restore from the
//!   `TurnCheckpoint` and the turn seam (V0, then L3), and turn cancel
//!   (L3); its phase runner is `phases`.
//! - [`node`]: the node runtime that serves a backend (L3).
//! - [`session_mail`]: everything the session's mailbox carries, drained
//!   and applied under the epoch on every claim (L3s).

pub mod head;
mod model_call;
pub mod node;
pub mod phases;
pub mod session;
pub mod session_mail;
mod turn_cancel;
