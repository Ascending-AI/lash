//! In-crate test suite.
//!
//! Split by what each file pins: the platform's wire contract, the bot's event
//! semantics, the log's per-line atomicity, and recovery across a restart. The harness in [`support`] serves
//! the platform over a real ephemeral socket so the wire assertions are made
//! against bytes rather than against Rust structs, and drives the bot through
//! [`crate::bot::channel::ChannelBot::ingest`] with envelopes the platform
//! actually produced.

mod bot_events;
mod log_atomicity;
mod platform_wire;
mod restart_recovery;
mod support;
