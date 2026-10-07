//! What an opener shares across the phase contexts it builds.

use std::sync::Arc;

use lash_sansio::sync::MutexExt;

use super::execution_context::RuntimeExecutionContext;

/// What an opener shares across the phase contexts it builds: the once-only
/// incorporation ledger.
///
/// Cloning shares it; [`OpenerState::default`] starts a fresh opener.
#[derive(Clone, Debug, Default)]
pub struct OpenerState {
    pub(crate) ledger: Arc<std::sync::Mutex<super::IncorporationLedger>>,
}

impl OpenerState {
    /// What the opener has incorporated so far.
    #[must_use]
    pub fn ledger_snapshot(&self) -> super::IncorporationLedger {
        self.ledger.lock_recover().clone()
    }

    /// Adopt a ledger a replayed boundary carried: the incorporations a
    /// journal-served phase made before its worker died (ADR 0099 §6).
    pub fn absorb_ledger(&self, ledger: super::IncorporationLedger) {
        self.ledger.lock_recover().absorb(ledger);
    }
}

impl RuntimeExecutionContext<'_> {
    /// The latest `max_tool_calls` refusal this execution met, typed. A
    /// language runtime whose run failed on the refusal reads it here to
    /// report the failure with its typed cause.
    #[must_use]
    pub fn tool_call_limit_refusal(&self) -> Option<crate::ToolCallLimitExceeded> {
        *self.tool_call_limit_refusal.lock_recover()
    }

    /// Record `exceeded`, the `max_tool_calls` refusal this execution met.
    pub fn record_tool_call_limit_refusal(&self, exceeded: crate::ToolCallLimitExceeded) {
        *self.tool_call_limit_refusal.lock_recover() = Some(exceeded);
    }
}
