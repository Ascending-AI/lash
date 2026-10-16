//! The session actor on the durable substrate (ADR 0132 §3, §4; S3 of I0,
//! FIG-5194).
//!
//! - [`session`]: the session activation, turn admission, restore from the
//!   `TurnCheckpoint` and the turn seam (V0, then L3), and turn cancel
//!   (L3); its phase runner is `phases`.
//! - [`node`]: the node runtime that serves a backend (L3).
//! - [`ProcessActivation`]: the process actor a node serves processes with
//!   (L6).
//! - [`session_mail`]: everything the session's mailbox carries, drained
//!   and applied under the epoch on every claim (L3s).
//! - [`session_close`]: the session's closing state, one fenced step at a
//!   time (L6b).
//! - `head_commit`: the session's head commits outside `turn.commit`, a
//!   session command's (`session.command`, FIG-5230) and a context-pressure
//!   frame's open (`pressure.frame`, FIG-5355), on its fenced transaction.
//! - `session_publication`: every commit that may publish the session's
//!   execution root, its heap pages as the commit's candidate (FIG-6418).
//! - [`turn_scope`]: a turn's scope ending with its commit or cancel, the
//!   cascade's cursor work and the bounded wait for its children (L6b).

pub(in crate::runtime) mod commit_publication;
pub mod head;
pub(crate) mod head_commit;
mod model_call;
pub mod node;
mod phase_root;
pub use phase_root::{RecordedPluginPhase, read_plugin_phase, read_protocol_events};
pub mod phases;
mod publication_window;
pub mod run_config;
pub mod services;
pub mod session;
pub mod session_close;
pub mod session_mail;
pub(crate) mod session_publication;
mod tool_round;
mod turn_cancel;
pub mod turn_scope;
mod turn_trace;

pub use lash_core_execution::runtime::actor::process::{
    ProcessActivation, engine_status, stored_engine_state,
};

#[cfg(test)]
#[path = "scope_end_tests.rs"]
mod scope_end_tests;
