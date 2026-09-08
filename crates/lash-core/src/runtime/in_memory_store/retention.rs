//! Factory-global retention of terminal-session receipts and usage (FIG-2502).
use super::factory::InMemorySessionStoreFactory;
use lash_sansio::sync::MutexExt;

impl InMemorySessionStoreFactory {
    pub(super) fn reclaim_retained_evidence_in_memory(
        &self,
        bound: crate::store::RetentionBound,
    ) -> crate::store::RetentionReport {
        let _transaction = self.write_transaction.lock_recover();
        // Only successful session deletion populates this map. The permanent
        // deleted_session_ids set is never pruned (FIG-754 / FIG-748).
        let mut retired = self.retired_stores.lock_recover();
        let mut report = crate::store::RetentionReport::default();
        retired.retain(|session_id, store| {
            if !self.deleted_session_ids.lock_recover().contains(session_id) {
                return true;
            }
            let mut receipts = store.runtime_turn_commits.lock_recover();
            let before = receipts.len();
            receipts
                .retain(|_, receipt| receipt.committed_at_ms >= bound.committed_before_epoch_ms);
            report.removed_receipt_count += before - receipts.len();
            // Phase 2: terminal usage is owned by the matching receipt. Prune
            // only after phase 1, by anti-join against remaining receipt roots.
            let mut usage = store.usage_deltas.lock_recover();
            let before = usage.len();
            usage.retain(|delta| {
                receipts.contains_key(&(
                    session_id.clone(),
                    delta.identity.operation_storage_key.clone(),
                ))
            });
            report.removed_usage_delta_count += before - usage.len();
            !receipts.is_empty() || !usage.is_empty()
        });
        let before = self.attachment_manifest.lock_recover().len();
        self.reclaim_deleted_attachment_roots();
        report.removed_attachment_root_count =
            before - self.attachment_manifest.lock_recover().len();
        report
    }
}
