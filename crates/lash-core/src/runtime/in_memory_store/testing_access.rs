//! Raw diagnostics exposed only to tests and the explicit `testing` feature.

use super::InMemorySessionStore;
use lash_sansio::sync::MutexExt;

impl InMemorySessionStore {
    #[cfg(test)]
    pub(crate) fn inject_graph_corruption_for_testing(
        &self,
        target: &crate::testing::conformance::GraphIntegrityTarget,
    ) {
        use crate::testing::conformance::{GraphIntegrityCorruption, GraphIntegrityRead};

        if target.corruption == GraphIntegrityCorruption::DanglingLeafId {
            self.session_head_meta
                .lock()
                .expect("lock session head")
                .as_mut()
                .expect("graph-integrity fixture has a session head")
                .leaf_node_id = Some(target.missing_node_id.clone());
            return;
        }
        let mut graph = self.global_session_graph.lock().expect("lock global graph");
        match target.corruption {
            GraphIntegrityCorruption::OrphanLeaf => {
                graph
                    .data_mut()
                    .nodes
                    .iter_mut()
                    .find(|node| node.node_id == target.leaf_node_id)
                    .expect("graph-integrity fixture leaf is durable")
                    .parent_node_id = Some(target.missing_node_id.clone());
            }
            GraphIntegrityCorruption::DuplicateNodeId => {
                let duplicate = graph
                    .nodes
                    .iter()
                    .find(|node| node.node_id == target.leaf_node_id)
                    .expect("graph-integrity fixture leaf is durable")
                    .clone();
                graph.data_mut().nodes.push(duplicate);
            }
            GraphIntegrityCorruption::DanglingLeafId => unreachable!(),
            GraphIntegrityCorruption::ParentCycle => {
                if target.read == GraphIntegrityRead::ActivePath {
                    graph
                        .data_mut()
                        .nodes
                        .iter_mut()
                        .find(|node| node.node_id == target.root_node_id)
                        .expect("graph-integrity fixture root is durable")
                        .parent_node_id = Some(target.leaf_node_id.clone());
                } else {
                    let template = graph
                        .nodes
                        .iter()
                        .find(|node| node.node_id == target.leaf_node_id)
                        .expect("graph-integrity fixture leaf is durable")
                        .clone();
                    let node_a_id = format!("{}-a", target.missing_node_id);
                    let node_b_id = format!("{}-b", target.missing_node_id);
                    let mut node_a = template.clone();
                    node_a.node_id = node_a_id.clone();
                    node_a.parent_node_id = Some(node_b_id.clone());
                    let mut node_b = template;
                    node_b.node_id = node_b_id;
                    node_b.parent_node_id = Some(node_a_id);
                    graph.data_mut().nodes.extend([node_a, node_b]);
                }
            }
        }
    }

    #[cfg(test)]
    pub(crate) fn load_whole_graph_for_testing(
        &self,
    ) -> Result<crate::SessionGraph, crate::StoreError> {
        let leaf_node_id = self
            .session_head_meta
            .lock()
            .expect("lock session head")
            .as_ref()
            .and_then(|meta| meta.leaf_node_id.clone());
        crate::SessionGraph::from_nodes(
            self.global_session_graph
                .lock()
                .expect("lock global graph")
                .nodes
                .clone(),
            leaf_node_id,
        )
        .map_err(|error| crate::StoreError::StoredDataCorrupt {
            record_kind: "SessionGraph",
            message: error.to_string(),
        })
    }

    /// This diagnostic accessor deliberately does not take `write_transaction`.
    /// It also holds `tombstoned_node_ids` while acquiring `session_graph`;
    /// callers must not use it concurrently with writes or introduce the
    /// inverse lock order.
    pub fn raw_graph_nodes_for_testing(&self) -> Vec<crate::SessionNodeRecord> {
        let tombstoned = self.tombstoned_node_ids.lock_recover();
        self.global_session_graph
            .lock_recover()
            .nodes
            .iter()
            .filter(|node| !tombstoned.contains(&node.node_id))
            .cloned()
            .collect()
    }

    /// Return the durable leaf-node id without loading a session read model.
    pub fn raw_leaf_node_id_for_testing(&self) -> Option<String> {
        self.session_head_meta
            .lock_recover()
            .as_ref()
            .and_then(|meta| meta.leaf_node_id.clone())
    }

    /// Return the durable head revision without loading a session read model.
    pub fn raw_head_revision_for_testing(&self) -> Option<u64> {
        self.session_head_meta
            .lock_recover()
            .as_ref()
            .map(|meta| meta.head_revision)
    }

    /// Return the durable checkpoint ref without constructing a session read model.
    pub fn raw_checkpoint_ref_for_testing(&self) -> Option<crate::BlobRef> {
        self.session_head_meta
            .lock_recover()
            .as_ref()
            .and_then(|meta| meta.checkpoint_ref.clone())
    }

    /// Return raw pending-input lifecycle state for differential tests.
    pub fn raw_pending_turn_inputs_for_testing(&self) -> Vec<super::RawPendingTurnInputForTesting> {
        self.pending_turn_inputs
            .lock_recover()
            .iter()
            .map(|entry| {
                (
                    entry.input.input_id.clone(),
                    entry.input.enqueue_seq,
                    entry.input.state,
                    entry.claim_id.clone(),
                    entry.claim_fencing_token,
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
            .lock_recover()
            .as_ref()
            .map(|meta| meta.session_id.clone());
        self.queued_work
            .lock_recover()
            .iter()
            .filter(|entry| {
                session_id
                    .as_ref()
                    .is_none_or(|session_id| &entry.batch.session_id == session_id)
            })
            .map(|entry| {
                (
                    entry.batch.clone(),
                    entry.claim_id.clone(),
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

    /// Return the receiver-side process-wake allocation fences directly from
    /// the in-memory durable map.
    pub fn raw_wake_redelivery_fences_for_testing(&self) -> Vec<(String, String, u64)> {
        let mut rows = self
            .wake_redelivery_fences
            .lock_recover()
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
        self.checkpoint.lock_recover().clone()
    }

    /// Return turn-commit receipt identity, intent hash, and replay payload.
    pub fn raw_runtime_turn_commits_for_testing(
        &self,
    ) -> Vec<(String, String, crate::RuntimeCommitResult)> {
        let mut rows = self
            .runtime_turn_commits
            .lock_recover()
            .iter()
            .map(|((_session_id, operation), record)| {
                (
                    operation.clone(),
                    record.turn_commit_hash.clone(),
                    record.result.clone(),
                )
            })
            .collect::<Vec<_>>();
        rows.sort_by(|left, right| left.0.cmp(&right.0));
        rows
    }

    /// Return every attachment-manifest row owned by this bound session.
    pub fn raw_attachment_manifest_for_testing(&self) -> Vec<crate::AttachmentManifestEntry> {
        let session_id = self
            .session_meta
            .lock_recover()
            .as_ref()
            .map(|meta| meta.session_id.clone());
        let mut rows = self
            .attachment_manifest
            .lock_recover()
            .values()
            .filter(|entry| Some(entry.session_id.as_str()) == session_id.as_deref())
            .cloned()
            .collect::<Vec<_>>();
        rows.sort_by(|left, right| left.attachment_id.cmp(&right.attachment_id));
        rows
    }

    pub fn raw_usage_deltas_for_testing(&self) -> Vec<crate::TokenLedgerEntry> {
        self.usage_deltas
            .lock_recover()
            .iter()
            .map(|delta| delta.entry.clone())
            .collect()
    }

    pub fn raw_session_meta_for_testing(&self) -> Option<crate::SessionMeta> {
        self.session_meta.lock_recover().clone()
    }

    /// Install deterministic metadata before a cross-backend differential run.
    pub fn replace_session_meta_for_testing(&self, meta: crate::SessionMeta) {
        let _transaction = self.write_transaction.lock_recover();
        *self.session_meta.lock_recover() = Some(meta);
    }

    pub fn raw_session_execution_leases_for_testing(
        &self,
    ) -> Vec<(
        String,
        Option<crate::LeaseOwnerIdentity>,
        Option<String>,
        bool,
        u64,
        u64,
        u64,
    )> {
        let mut rows = self
            .session_execution_leases
            .lock_recover()
            .iter()
            .map(|(session_id, lease)| {
                (
                    session_id.clone(),
                    lease.owner.clone(),
                    lease.executor_id.clone(),
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
        self.stores.lock_recover().get(session_id).cloned()
    }

    /// Return explicit node-anchor rows without mixing in implicit live tips.
    pub fn raw_node_anchors_for_testing(&self) -> Vec<(String, crate::BlobRef, String)> {
        let mut rows = self
            .node_anchors
            .lock_recover()
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
    use crate::{DeliveryPolicy, QueuedWorkBatchDraft, QueuedWorkPayload, QueuedWorkStore};

    #[tokio::test]
    async fn queued_work_diagnostic_is_unfiltered_without_session_meta() {
        let store = super::InMemorySessionStore::default();
        store
            .enqueue_queued_work(QueuedWorkBatchDraft::new(
                "deleted-session",
                DeliveryPolicy::EarliestSafeBoundary,
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
