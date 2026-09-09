//! Explicit host policy for terminal-session evidence reclamation (FIG-653).

/// Host-selected exclusive horizon for commit evidence.
///
/// Only receipts in a durably deleted session and strictly before this bound
/// are eligible. No clock or live configuration is consulted by reclamation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RetentionBound {
    /// Exclusive commit timestamp horizon in milliseconds since Unix epoch.
    pub committed_before_epoch_ms: u64,
}

/// Committed counts from one atomic, factory-wide evidence sweep.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RetentionReport {
    /// Terminal-session receipts removed before the host's horizon.
    pub removed_receipt_count: usize,
    /// Terminal-session usage rows whose owning receipt no longer exists.
    pub removed_usage_delta_count: usize,
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
            + self.removed_usage_delta_count
            + self.removed_attachment_root_count
            + self.retired_effect_scope_count
    }
}

/// The commit key under which a plugin operation records its receipt: the
/// durable proof the reclaim sweep reads before retiring the operation's
/// effect scope.
pub const PLUGIN_OPERATION_STATE_RECEIPT_KEY: &str = "plugin-operation-state";

/// The `runtime_turn_commits` storage key of the receipt a plugin operation
/// under `scope` records when it completes. A scope with effect rows and no
/// such receipt is still owned by a live operation (or one that failed before
/// recording anything) and is not the sweep's to retire.
pub fn plugin_operation_receipt_storage_key(
    scope: &crate::ExecutionScope,
) -> Result<String, crate::StoreError> {
    crate::OperationId::new(scope.clone(), PLUGIN_OPERATION_STATE_RECEIPT_KEY).storage_key()
}
