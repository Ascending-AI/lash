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
}

impl super::MaintenanceReport for RetentionReport {
    fn reclaimed_count(&self) -> usize {
        self.removed_receipt_count
            + self.removed_usage_delta_count
            + self.removed_attachment_root_count
    }
}
