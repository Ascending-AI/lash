//! Provider-reported token usage on turn results and live events.
//!
//! Hosts meter spend at the `Provider` seam (ADR 0127). Response usage and
//! sealed attempt history are journaled with the model call result. Traces
//! and turn summaries report observations, not billing evidence.

pub use lash_core::{LlmUsage, TokenUsageOverflow};
