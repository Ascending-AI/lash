//! Raw diagnostics exposed only to tests and the explicit `testing` feature.

use super::InMemorySessionStore;

impl InMemorySessionStore {
    /// This diagnostic accessor deliberately does not take `write_transaction`.
    /// It also holds `tombstoned_node_ids` while acquiring `session_graph`;
    /// callers must not use it concurrently with writes or introduce the
    /// inverse lock order.
    pub fn raw_graph_nodes_for_testing(&self) -> Vec<crate::SessionNodeRecord> {
        let tombstoned = self
            .tombstoned_node_ids
            .lock()
            .expect("lock tombstoned nodes");
        self.global_session_graph
            .lock()
            .expect("lock global graph")
            .nodes
            .iter()
            .filter(|node| !tombstoned.contains(&node.node_id))
            .cloned()
            .collect()
    }

    /// Return the durable leaf-node id without loading a session read model.
    pub fn raw_leaf_node_id_for_testing(&self) -> Option<String> {
        self.session_head_meta
            .lock()
            .expect("lock store")
            .as_ref()
            .and_then(|meta| meta.leaf_node_id.clone())
    }

    /// Return the durable head revision without loading a session read model.
    pub fn raw_head_revision_for_testing(&self) -> Option<u64> {
        self.session_head_meta
            .lock()
            .expect("lock store")
            .as_ref()
            .map(|meta| meta.head_revision)
    }

    /// Return the durable checkpoint ref without constructing a session read model.
    pub fn raw_checkpoint_ref_for_testing(&self) -> Option<crate::BlobRef> {
        self.session_head_meta
            .lock()
            .expect("lock store")
            .as_ref()
            .and_then(|meta| meta.checkpoint_ref.clone())
    }

    /// Return raw pending-input lifecycle state for differential tests.
    pub fn raw_pending_turn_inputs_for_testing(
        &self,
    ) -> Vec<(String, crate::TurnInputState, Option<u64>)> {
        self.pending_turn_inputs
            .lock()
            .expect("lock pending turn inputs")
            .iter()
            .map(|entry| {
                (
                    entry.input.input_id.clone(),
                    entry.input.state,
                    entry
                        .claim_token
                        .as_ref()
                        .map(|_| entry.claim_session_lease_generation),
                )
            })
            .collect()
    }

    /// Return raw queued-work batches and their claim state for differential
    /// tests. The sequence is backend-local, so callers normalize ordering.
    pub fn raw_queued_work_for_testing(&self) -> Vec<super::RawQueuedWorkForTesting> {
        let session_id = self
            .session_meta
            .lock()
            .expect("lock session meta")
            .as_ref()
            .map(|meta| meta.session_id.clone());
        self.queued_work
            .lock()
            .expect("lock queued work")
            .iter()
            .filter(|entry| {
                session_id
                    .as_ref()
                    .is_none_or(|session_id| &entry.batch.session_id == session_id)
            })
            .map(|entry| {
                (
                    entry.batch.clone(),
                    entry.claim_id.is_some(),
                    entry.claim_owner.clone(),
                    entry.claim_token.is_some(),
                    entry.claim_fencing_token,
                    entry
                        .claim_token
                        .as_ref()
                        .map(|_| entry.claim_session_lease_generation),
                )
            })
            .collect()
    }

    /// Return the monotone receiver-side process-wake evidence directly from
    /// the in-memory durable map.
    pub fn raw_consumed_wake_high_water_for_testing(&self) -> Vec<(String, String, u64)> {
        let mut rows = self
            .consumed_wake_high_water
            .lock()
            .expect("lock consumed wake high water")
            .iter()
            .map(|((session_id, process_id), sequence)| {
                (session_id.clone(), process_id.clone(), *sequence)
            })
            .collect::<Vec<_>>();
        rows.sort();
        rows
    }

    /// Return the current checkpoint exactly as held by the in-memory durable
    /// implementation, including both content refs and resolved bodies.
    pub fn raw_checkpoint_for_testing(&self) -> Option<crate::HydratedSessionCheckpoint> {
        self.checkpoint.lock().expect("lock checkpoint").clone()
    }

    /// Return turn-commit receipt identity, intent hash, and replay payload.
    pub fn raw_runtime_turn_commits_for_testing(
        &self,
    ) -> Vec<(String, String, crate::RuntimeCommitResult)> {
        let mut rows = self
            .runtime_turn_commits
            .lock()
            .expect("lock runtime turn commits")
            .iter()
            .map(
                |((_session_id, operation), (hash, result, _committed_at_ms))| {
                    (operation.clone(), hash.clone(), result.clone())
                },
            )
            .collect::<Vec<_>>();
        rows.sort_by(|left, right| left.0.cmp(&right.0));
        rows
    }

    /// Return every attachment-manifest row owned by this bound session.
    pub fn raw_attachment_manifest_for_testing(&self) -> Vec<crate::AttachmentManifestEntry> {
        let session_id = self
            .session_meta
            .lock()
            .expect("lock session meta")
            .as_ref()
            .map(|meta| meta.session_id.clone());
        let mut rows = self
            .attachment_manifest
            .lock()
            .expect("lock attachment manifest")
            .values()
            .filter(|entry| Some(entry.session_id.as_str()) == session_id.as_deref())
            .cloned()
            .collect::<Vec<_>>();
        rows.sort_by(|left, right| left.attachment_id.cmp(&right.attachment_id));
        rows
    }

    pub fn raw_usage_deltas_for_testing(&self) -> Vec<crate::TokenLedgerEntry> {
        self.usage_deltas.lock().expect("lock usage deltas").clone()
    }

    pub fn raw_session_meta_for_testing(&self) -> Option<crate::SessionMeta> {
        self.session_meta.lock().expect("lock session meta").clone()
    }

    /// Install deterministic metadata before a cross-backend differential run.
    pub fn replace_session_meta_for_testing(&self, meta: crate::SessionMeta) {
        let _transaction = self
            .write_transaction
            .lock()
            .expect("lock in-memory write transaction");
        *self.session_meta.lock().expect("lock session meta") = Some(meta);
    }

    pub fn raw_session_execution_leases_for_testing(
        &self,
    ) -> Vec<(
        String,
        Option<crate::LeaseOwnerIdentity>,
        bool,
        u64,
        u64,
        u64,
    )> {
        let mut rows = self
            .session_execution_leases
            .lock()
            .expect("lock session execution leases")
            .iter()
            .map(|(session_id, lease)| {
                (
                    session_id.clone(),
                    lease.owner.clone(),
                    lease.lease_token.is_some(),
                    lease.fencing_token,
                    lease.claimed_at_epoch_ms,
                    lease.expires_at_epoch_ms,
                )
            })
            .collect::<Vec<_>>();
        rows.sort_by(|left, right| left.0.cmp(&right.0));
        rows
    }
}

impl super::InMemorySessionStoreFactory {
    /// Return the concrete testing store after `SessionStoreFactory` created it.
    pub fn raw_store_for_testing(
        &self,
        session_id: &str,
    ) -> Option<std::sync::Arc<InMemorySessionStore>> {
        self.stores
            .lock()
            .expect("lock in-memory stores")
            .get(session_id)
            .cloned()
    }

    /// Return explicit node-anchor rows without mixing in implicit live tips.
    pub fn raw_node_anchors_for_testing(&self) -> Vec<(String, crate::BlobRef, String)> {
        let mut rows = self
            .node_anchors
            .lock()
            .expect("lock node anchors")
            .iter()
            .map(
                |(node_id, (checkpoint_ref, _checkpoint, source_session_id))| {
                    (
                        node_id.clone(),
                        checkpoint_ref.clone(),
                        source_session_id.clone(),
                    )
                },
            )
            .collect::<Vec<_>>();
        rows.sort_by(|left, right| left.0.cmp(&right.0));
        rows
    }
}

#[cfg(test)]
mod tests {
    use crate::{
        DeliveryPolicy, QueuedWorkBatchDraft, QueuedWorkPayload, QueuedWorkStore, SlotPolicy,
    };

    #[tokio::test]
    async fn queued_work_diagnostic_is_unfiltered_without_session_meta() {
        let store = super::InMemorySessionStore::default();
        store
            .enqueue_queued_work(QueuedWorkBatchDraft::new(
                "deleted-session",
                DeliveryPolicy::EarliestSafeBoundary,
                SlotPolicy::Exclusive,
                vec![QueuedWorkPayload::session_command(
                    crate::SessionCommand::RefreshToolCatalog {
                        reason: "prove post-delete diagnostics are non-vacuous".to_string(),
                    },
                )],
            ))
            .await
            .expect("seed queued work without session metadata");

        let rows = store.raw_queued_work_for_testing();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].0.session_id, "deleted-session");
    }
}
