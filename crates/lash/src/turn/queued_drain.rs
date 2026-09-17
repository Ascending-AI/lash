//! What an automatic queued-turn drain returns to a host.
//!
//! A drain that ran no turn is not self-explanatory, so the empty arm
//! carries the reason the runtime already computed instead of a bare
//! `None` the host has to guess at. The vocabulary is core's own — the
//! facade re-exports it rather than restating it, matching the rest of the
//! queue types at the crate root.

pub use lash_core::facade_support::{EmptyQueuedDrainReason, QueuedTurnDrain};
