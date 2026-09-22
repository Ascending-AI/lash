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
//! - **[`SessionUsageReport`]** (`session.usage_report()`): aggregate
//!   across the whole session, broken down by `source` × `model`. Right for
//!   dashboards and "session so far."
//!
//! Absence is not zero (ADR 0031). A provider call the runtime aborted at an
//! RLM cell boundary, or that failed mid-stream, may end before the provider
//! reports usage; it was still billed. Such attempts carry a typed
//! `usage_disposition` on their attempt record and the ledger gets an
//! `Unreported` row for them even at zero usage, so
//! [`UsageTotals::unreported_attempts`] tells a host how many calls the
//! counters do not cover. `session.reconcile_unreported_usage()` asks the
//! provider after the fact and appends `Reconciled` correction rows
//! ([`LedgerUsageDisposition`]) that the totals sum.
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
    LedgerUsageDisposition, TokenLedgerEntry, TokenUsage, TokenUsageOverflow,
    facade_support::ReconciledUsageAttempt, facade_support::SessionUsageReport,
    facade_support::UnreportedUsageAttempt, facade_support::UsageReconciliationReport,
    facade_support::UsageReportRow, facade_support::UsageTotals, facade_support::diff_token_ledger,
    facade_support::diff_usage_reports,
};

/// Well-known source labels used by the runtime and first-party plugins.
///
/// The `source` field on [`TokenLedgerEntry`] is a free-form string; the
/// runtime does not interpret the value. Plugins may use additional labels of
/// their own.
pub mod sources {
    /// Parent's own LLM calls.
    pub const TURN: &str = "turn";
    /// Standard-compaction compaction passes.
    pub const COMPACTION: &str = "compaction";
}
