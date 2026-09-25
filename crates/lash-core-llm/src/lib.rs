//! LLM call surface of the Lash runtime kernel.
//!
//! This crate owns the provider handle and its retry/charge-safety machinery,
//! model specification and clamping, the LLM transport seam, and the trace
//! projections of the records those produce. It sits directly above
//! `lash-core-ids` and below everything in `lash-core` that reaches for a
//! store, a plugin or the runtime, so `lash-core` re-exports every module and
//! item below at its original path.

pub mod llm;
pub mod model;
pub mod provider;
pub mod session_model;
pub mod trace;
pub mod turn_vocabulary;

/// Items `lash-core` reaches for across the crate boundary that are not part
/// of a module's published surface. Nothing outside the kernel uses them and
/// `lash-core` re-exports each one crate-internally.
pub mod core_internal {
    pub use crate::provider::handle::{
        ProviderCompletionSideband, call_id_for_scope, synthetic_terminal_call_record,
    };
}

// Crate-root vocabulary. These re-exports exist so the modules above keep the
// `crate::Item` paths they carried inside `lash-core`; they are deliberately
// crate-internal, so this crate's public surface is the modules alone.
pub(crate) use lash_core_ids::clock::{Clock, SystemClock};
pub(crate) use lash_core_ids::{operational_metrics, panic_containment};
pub(crate) use lash_sansio::llm::types::{
    AttemptOutcome, AttemptRecord, AttemptUsageDisposition, ChargeSafetyDecision,
    ChargeSafetyDenialReason, ExecutionEvidence, ExecutionEvidenceCollectionInterruption,
    GenerationOptions, GenerationReceipt, LlmCallRecord, LlmTerminalReason, NonNegativeFiniteF64,
    ProviderFailureKind, ProviderReplayDrop,
};
pub(crate) use lash_sansio::session_model::{FailureCode, TurnFailureKind};
pub(crate) use session_model::ChargeSafetyPolicy;

// Names the moved modules reach for only from their own test suites.
#[cfg(test)]
pub(crate) use lash_core_ids::clock::ClockWallTime;
#[cfg(test)]
pub(crate) use lash_sansio::llm::types::{
    GenerationOptionOutcome, LlmCallId, LlmRequestScope, ProtocolPosition, ProviderReplayDropReason,
};
#[cfg(test)]
pub(crate) use provider::ModelCapability;
