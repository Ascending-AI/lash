//! Explicit host policy for terminal-session evidence reclamation (FIG-653).

/// Host-selected exclusive horizon for commit evidence.
///
/// Only receipts in a durably deleted session and strictly before this bound
/// are eligible. No clock or live configuration is consulted by reclamation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RetentionBound {
    /// Exclusive commit timestamp horizon in milliseconds since Unix epoch.
    pub committed_before_epoch_ms: u64,
    /// Never remove a terminal beyond this acknowledged feed position.
    pub turn_watermark: super::TurnProjectionWatermark,
}

/// Committed counts from one atomic, factory-wide evidence sweep.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RetentionReport {
    /// Terminal-session receipts removed before the host's horizon.
    pub removed_receipt_count: usize,
    /// Retained session fault and deletion records acknowledged by the host.
    pub removed_session_terminal_count: usize,
    /// Host tool-intent submission ledger rows removed before the host
    /// horizon once their owner session is durably deleted; the owner keeps a
    /// fence that refuses every later submission (FIG-1509).
    pub removed_tool_intent_submission_count: usize,
    /// Deleted-owner manifest rows no surviving graph prefix needs.
    pub removed_attachment_root_count: usize,
    /// Session-free runtime-operation scopes retired by this sweep: their
    /// owning operation had recorded its receipt and nothing was live under
    /// them any more, so their effect journal, groups, and promises were
    /// deleted and the scope fenced (ADR 0049, ADR 0067).
    pub retired_effect_scope_count: usize,
}

impl super::MaintenanceReport for RetentionReport {
    fn reclaimed_count(&self) -> usize {
        self.removed_receipt_count
            + self.removed_session_terminal_count
            + self.removed_tool_intent_submission_count
            + self.removed_attachment_root_count
            + self.retired_effect_scope_count
    }
}
