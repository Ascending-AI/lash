//! Token usage tracking surfaces.
//!
//! Four channels, finest granularity to coarsest:
//!
//! - **`TraceSink`**: every provider call across every session in the
//!   runtime. Right for billing, audit, off-line analysis. Heavier than
//!   necessary if you only want totals. See [`crate::tracing`].
//! - **[`TurnEvent::Usage`] / [`TurnEvent::ChildUsage`]**: live during a
//!   turn, one event per LLM iteration. `Usage` is the parent's own
//!   model call; `ChildUsage` carries `session_id` + `source` so a UI can
//!   group child traffic (e.g. by subagent). Right for live counters.
//! - **[`TurnResult::usage`] / [`TurnResult::children_usage`]**: per-turn
//!   snapshot at completion. `usage` is parent-only; `children_usage` retains
//!   one row per `(session, source, model)`. [`TurnResult::total_usage`] sums
//!   both.
//!   Right for "what did this message cost."
//! - **[`SessionUsageReport`]** (`session.usage_report()`): aggregate
//!   across the whole session, broken down by `source` × `model`. Right for
//!   dashboards and "session so far."
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
//! [`TurnEvent::ChildUsage`]: lash_core::TurnEvent::ChildUsage
//! [`TurnResult::usage`]: crate::TurnResult::usage
//! [`TurnResult::children_usage`]: crate::TurnResult::children_usage
//! [`TurnResult::total_usage`]: crate::TurnResult::total_usage

pub use lash_core::{
    TokenLedgerEntry, TokenUsage, TokenUsageOverflow, facade_support::SessionUsageReport,
    facade_support::UsageReportRow, facade_support::UsageTotals, facade_support::diff_token_ledger,
    facade_support::diff_usage_reports,
};

/// Well-known source labels used by the runtime and first-party plugins.
///
/// The `source` field on [`TokenLedgerEntry`] and `ChildUsage` events is a
/// free-form string; the runtime does not interpret the value. Plugins may
/// use additional labels of their own.
pub mod sources {
    /// Parent's own LLM calls.
    pub const TURN: &str = "turn";
    /// Spawned subagent sessions.
    pub const SUBAGENT: &str = "subagent";
    /// Rolling-history compaction passes.
    pub const COMPACTION: &str = "compaction";
    /// Default fallback when no `usage_source` is set on a child session.
    pub const CHILD: &str = "child";
}
