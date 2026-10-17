//! LLM call surface of the Lash runtime kernel.
//!
//! This crate owns the provider handle and its retry/charge-safety machinery,
//! model specification and clamping, the LLM transport seam, and the trace
//! projections of the records those produce. It sits directly above
//! `lash-core-ids` and below everything in `lash-core` that reaches for a
//! store, a plugin or the runtime, so `lash-core` re-exports every module and
//! item below at its original path.

#![deny(clippy::future_not_send)]

pub mod llm;
pub use lash_protocol_types::llm_profile;
pub mod provider;
pub mod session_model;
pub mod stream_assembly;
pub mod trace;
pub mod turn_vocabulary;

/// Items `lash-core` reaches for across the crate boundary that are not part
/// of a module's published surface. Nothing outside the kernel uses them and
/// `lash-core` re-exports each one crate-internally.
pub mod core_internal {
    pub use crate::provider::handle::{
        ModelCallBounds, ProviderCompletionSideband, call_id_for_scope, complete_prepared,
        prepare_completion, synthetic_terminal_call_record,
    };
}

// Crate-root vocabulary. These re-exports exist so the modules above keep the
// `crate::Item` paths they carried inside `lash-core`; they are deliberately
// crate-internal, so this crate's public surface is the modules alone.
pub(crate) use lash_core_ids::clock::{Clock, SystemClock};
pub(crate) use lash_core_ids::{operational_metrics, panic_containment};
pub(crate) use lash_protocol_types::{
    llm::types::GenerationReceipt, llm::types::LlmCallRecord, llm::types::LlmTerminalReason,
    llm::types::NonNegativeFiniteF64, llm::types::ProviderFailureKind,
    llm::types::ProviderReplayDrop,
};
pub(crate) use lash_protocol_types::{session_model::FailureCode, session_model::TurnFailureKind};
pub(crate) use session_model::ChargeSafetyPolicy;

// Names the moved modules reach for only from their own test suites.
#[cfg(test)]
pub(crate) use lash_core_ids::clock::ClockWallTime;
#[cfg(test)]
pub(crate) use lash_protocol_types::{
    llm::types::AttemptOutcome, llm::types::AttemptRecord, llm::types::ChargeSafetyDecision,
    llm::types::ChargeSafetyDenialReason, llm::types::GenerationOptionOutcome,
    llm::types::GenerationOptions, llm::types::LlmCallId, llm::types::LlmRequestScope,
    llm::types::ProtocolPosition, llm::types::ProviderReplayDropReason,
};
#[cfg(test)]
pub(crate) use provider::LlmProfileCapability;
