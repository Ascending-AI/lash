//! Token usage tracking surfaces.
//!
//! Four channels, finest granularity to coarsest:
//!
//! - **`TraceSink`**: every provider call across every session in the
//!   runtime. Right for billing, audit, off-line analysis. Heavier than
//!   necessary if you only want totals. See [`crate::tracing`].
//! - **[`TurnEvent::Usage`]**: live during a turn, one event per LLM
//!   iteration. Right for live counters.
//! - **[`TurnReport::usage`]**: per-turn snapshot at completion, the session's
//!   own LLM tokens. Right for "what did this message cost."
//! - **[`OwnerUsage`]** (`session.usage().await`, `core.owner_usage(..)`):
//!   the durable ledger of one owner (a session or a process), across every
//!   call made for it, broken down by `source` × `model`. Right for billing,
//!   dashboards and "session so far." [`OwnerUsage::report`] renders it as a
//!   [`SessionUsageReport`].
//!
//! Model usage is engine-owned accounting (ADR 0125). Each spending effect
//! (a turn's model call, a direct completion, a tool attempt) is one usage
//! run: admitted to storage before its first provider attempt, and settled
//! with the facts of every attempt it dispatched once its outcome is
//! journaled. Delivery survives the turn ending, forks, parks, deletion and
//! a lost runtime. [`UsageCompleteness`] says how far the ledger can be
//! trusted: an open run is delivery still pending, an unknown run dispatched
//! and its amount will never be known.
//!
//! Absence is not zero (ADR 0031). A provider call the runtime aborted at an
//! RLM cell boundary, or that failed mid-stream, may end before the provider
//! reports usage; it was still billed. Such attempts are recorded as
//! unreported facts, so [`UsageTotals::unreported_attempts`] tells a host how
//! many calls the counters do not cover. `session.reconcile_unreported_usage()`
//! asks the provider after the fact and appends one correction per recovered
//! attempt, which the totals sum.
//!
//! Usage buckets are provider-normalized before they reach these surfaces:
//! `input_tokens` is uncached ordinary input, `cache_read_input_tokens` is
//! cached prompt input read from the provider cache, `cache_write_input_tokens`
//! is prompt input written to the provider cache, and `output_tokens` is total
//! generated output. `reasoning_output_tokens` is a subset of output tokens,
//! not an extra total component. `TokenUsage::total()` therefore sums ordinary
//! input, output, cache reads, and cache writes.
//!
//! [`TurnEvent::Usage`]: lash_core::TurnEvent::Usage
//! [`TurnReport::usage`]: crate::TurnReport::usage

pub use lash_core::{
    OutstandingUsageAttempt, OwnerUsage, OwnerUsageRow, TokenUsage, TokenUsageOverflow,
    UsageCompleteness, UsageFactCursor, UsageFactPage, UsageFactRecord, UsageOwnerRetired,
    UsageReporting, UsageRunCursor, UsageRunFilter, UsageRunPage, UsageRunRecord, UsageRunState,
    UsageUnknownReason, facade_support::ReconciledUsageAttempt, facade_support::SessionUsageReport,
    facade_support::UsageAttributionKey, facade_support::UsageReconciliationReport,
    facade_support::UsageReportRow, facade_support::UsageTotals,
    facade_support::diff_usage_reports,
};

/// Well-known source labels used by the runtime and first-party plugins.
///
/// The `source` field on a usage fact is a free-form string; the
/// runtime does not interpret the value. Plugins may use additional labels of
/// their own.
pub mod sources {
    /// Parent's own LLM calls.
    pub const TURN: &str = "turn";
    /// Standard-compaction compaction passes.
    pub const COMPACTION: &str = "compaction";
}

// The vocabulary this module's signatures name (the facade-completeness rule).
pub use lash_core::usage_accounting::{
    UsageEffectKey, UsageFactIdentity, UsageFactKind, UsageRunId,
};
