use super::*;

#[derive(Clone, PartialEq, Eq)]
pub(super) struct DurableNode {
    // SQL rows are read by `seq`; the in-memory vector uses its native index.
    // Comparing this normalized replay ordinal keeps transcript order
    // contract-visible without comparing backend-local sequence counters.
    pub(super) ordinal: usize,
    pub(super) node_id: String,
    // Compared as its own field rather than inside `bytes`: SQL keeps parent
    // topology in an indexed column while the in-memory record carries it in
    // the struct, so byte comparison alone would report a physical layout
    // choice as drift while leaving the edge itself uncompared.
    pub(super) parent_node_id: Option<String>,
    // Both SQL backends currently store node_json as TEXT. A future jsonb
    // migration would reserialize values and make every byte comparison red
    // for a reason outside the persistence contract.
    pub(super) bytes: Vec<u8>,
}

impl std::fmt::Debug for DurableNode {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("DurableNode")
            .field("ordinal", &self.ordinal)
            .field("node_id", &self.node_id)
            .field("parent_node_id", &self.parent_node_id)
            .field("bytes", &String::from_utf8_lossy(&self.bytes))
            .finish()
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct CheckpointObservation {
    pub(super) checkpoint_ref: Option<BlobRef>,
    pub(super) turn_state: serde_json::Value,
    pub(super) tool_state_ref: Option<BlobRef>,
    pub(super) tool_state: Option<serde_json::Value>,
    pub(super) plugin_snapshot_ref: Option<BlobRef>,
    pub(super) plugin_snapshot: Option<serde_json::Value>,
    pub(super) plugin_snapshot_revision: Option<u64>,
    pub(super) execution_state_ref: Option<BlobRef>,
    pub(super) execution_state: Option<Vec<u8>>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct RuntimeTurnCommitObservation {
    pub(super) operation: String,
    pub(super) turn_commit_hash: String,
    pub(super) result: serde_json::Value,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct AttachmentManifestObservation {
    pub(super) attachment_id: AttachmentId,
    pub(super) canonical_uri: String,
    pub(super) intent_at_epoch_ms: u64,
    // Commit time is store-authoritative (database time in PostgreSQL, injected
    // host time locally). The logical lifecycle fact is compared explicitly.
    pub(super) committed: bool,
    pub(super) owner_kind: Option<AttachmentOwnerKind>,
    pub(super) owner_id: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct NodeAnchorObservation {
    pub(super) node_id: String,
    pub(super) checkpoint_ref: BlobRef,
    pub(super) source_session_id: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct UsageDeltaObservation {
    pub(super) source: String,
    pub(super) model: String,
    pub(super) usage: TokenUsage,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct SessionMetaObservation {
    pub(super) session_name: String,
    pub(super) created_at: String,
    pub(super) model: String,
    pub(super) cwd: Option<String>,
    pub(super) relation: SessionRelation,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct SessionExecutionLeaseObservation {
    pub(super) owner: Option<LeaseOwnerIdentity>,
    // Lease tokens are backend-generated CAS capabilities. Their bytes are a
    // physical implementation detail; token presence is the logical row state
    // and is compared explicitly alongside owner, generation, and times.
    pub(super) lease_token_present: bool,
    pub(super) fencing_token: u64,
    // PostgreSQL uses database-authoritative wall time while local stores use
    // the injected clock. Compare the durable temporal contract (claimed and
    // TTL) rather than incomparable clock-domain epoch values.
    pub(super) claimed: bool,
    pub(super) ttl_ms: Option<u64>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct ComparableRuntimeCommitResult {
    pub(super) head_revision: u64,
    pub(super) turn_input_applications: Vec<TurnInputApplication>,
}

impl From<RuntimeCommitResult> for ComparableRuntimeCommitResult {
    fn from(result: RuntimeCommitResult) -> Self {
        Self {
            head_revision: result.head_revision,
            turn_input_applications: result.turn_input_applications,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct RawDurableState {
    pub(super) head_revision: Option<u64>,
    pub(super) leaf_node_id: Option<String>,
    pub(super) checkpoint: Option<CheckpointObservation>,
    pub(super) durable_nodes: Vec<DurableNode>,
    pub(super) runtime_turn_commits: Vec<RuntimeTurnCommitObservation>,
    pub(super) attachment_manifest: Vec<AttachmentManifestObservation>,
    pub(super) node_anchors: Vec<NodeAnchorObservation>,
    pub(super) usage_deltas: Vec<UsageDeltaObservation>,
    pub(super) session_meta: Option<SessionMetaObservation>,
    pub(super) session_execution_leases: Vec<SessionExecutionLeaseObservation>,
    pub(super) pending_turn_inputs: Vec<PendingTurnInputObservation>,
    pub(super) queued_work: Vec<QueuedWorkObservation>,
    pub(super) session_owned_artifact_refs: Vec<SessionOwnedArtifactRefObservation>,
    // `process_*` and `trigger_*` are deliberately excluded: they are separate
    // subsystems with dedicated conformance suites, while this harness and its
    // operation vocabulary are scoped to one runtime session. Effect/await
    // state is likewise owned by the separate EffectHost contract.
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct StepObservation {
    pub(super) store_error: Option<String>,
    pub(super) runtime_commit_result: Option<ComparableRuntimeCommitResult>,
    pub(super) durable_state: RawDurableState,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct PendingTurnInputObservation {
    pub(super) input_id: String,
    pub(super) state: TurnInputState,
    pub(super) claim_session_lease_generation: Option<u64>,
}

// Backend-generated batch/item ids, physical enqueue sequences, and enqueue
// timestamps are excluded. Logical order survives as `ordinal`; PostgreSQL's
// wall-clock enqueue time is not comparable with the injected local clocks.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct QueuedWorkObservation {
    ordinal: usize,
    source_key: Option<String>,
    delivery_policy: DeliveryPolicy,
    slot_policy: SlotPolicy,
    merge_key: MergeKey,
    available_at_ms: u64,
    payloads: Vec<serde_json::Value>,
    claim_id_present: bool,
    claim_owner: Option<LeaseOwnerIdentity>,
    claim_token_present: bool,
    claim_fencing_token: u64,
    claim_session_lease_generation: Option<u64>,
}

// SQLite stores a content-addressed blob pointer while PostgreSQL stores the
// manifest bytes inline. Only namespace plus owner ref form the shared logical
// identity; pointer/body representation is deliberately normalized away.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct SessionOwnedArtifactRefObservation {
    namespace: String,
    artifact_ref: String,
}

pub(super) fn session_owned_artifact_ref_observations(
    rows: Vec<(String, String)>,
) -> Vec<SessionOwnedArtifactRefObservation> {
    rows.into_iter()
        .map(
            |(namespace, artifact_ref)| SessionOwnedArtifactRefObservation {
                namespace,
                artifact_ref,
            },
        )
        .collect()
}

pub(super) fn queued_work_observation(
    ordinal: usize,
    batch: QueuedWorkBatch,
    claim_id_present: bool,
    claim_owner: Option<LeaseOwnerIdentity>,
    claim_token_present: bool,
    claim_fencing_token: u64,
    claim_session_lease_generation: Option<u64>,
) -> QueuedWorkObservation {
    QueuedWorkObservation {
        ordinal,
        source_key: batch.source_key,
        delivery_policy: batch.delivery_policy,
        slot_policy: batch.slot_policy,
        merge_key: batch.merge_key,
        available_at_ms: batch.available_at_ms,
        payloads: batch
            .items
            .into_iter()
            .map(|item| serde_json::to_value(item.payload).expect("encode queued-work payload"))
            .collect(),
        claim_id_present,
        claim_owner,
        claim_token_present,
        claim_fencing_token,
        claim_session_lease_generation,
    }
}

pub(super) fn queued_work_observations_from_sql_rows(
    batches: Vec<QueuedWorkBatchRow>,
    items: Vec<QueuedWorkItemRow>,
) -> Vec<QueuedWorkObservation> {
    let mut payloads_by_batch = BTreeMap::<String, Vec<(i64, serde_json::Value)>>::new();
    for (batch_id, item_index, payload_json) in items {
        payloads_by_batch.entry(batch_id).or_default().push((
            item_index,
            serde_json::from_str(&payload_json).expect("decode queued-work payload"),
        ));
    }

    batches
        .into_iter()
        .enumerate()
        .map(
            |(
                ordinal,
                (
                    _enqueue_seq,
                    batch_id,
                    source_key,
                    delivery_policy,
                    slot_policy,
                    merge_key_json,
                    available_at_ms,
                    claim_id,
                    claim_owner_id,
                    claim_owner_incarnation_id,
                    claim_owner_liveness_json,
                    claim_token,
                    claim_fencing_token,
                    claim_session_lease_generation,
                ),
            )| {
                let mut payloads = payloads_by_batch.remove(&batch_id).unwrap_or_default();
                payloads.sort_by_key(|(item_index, _)| *item_index);
                QueuedWorkObservation {
                    ordinal,
                    source_key,
                    delivery_policy: DeliveryPolicy::from_wire_str(&delivery_policy)
                        .expect("decode queued-work delivery policy"),
                    slot_policy: SlotPolicy::from_wire_str(&slot_policy)
                        .expect("decode queued-work slot policy"),
                    merge_key: serde_json::from_str(&merge_key_json)
                        .expect("decode queued-work merge key"),
                    available_at_ms: available_at_ms as u64,
                    payloads: payloads
                        .into_iter()
                        .map(|(_item_index, payload)| payload)
                        .collect(),
                    claim_id_present: claim_id.is_some(),
                    claim_owner: decode_lease_owner(
                        claim_owner_id,
                        claim_owner_incarnation_id,
                        claim_owner_liveness_json,
                    ),
                    claim_token_present: claim_token.is_some(),
                    claim_fencing_token: claim_fencing_token as u64,
                    claim_session_lease_generation: claim_token
                        .as_ref()
                        .map(|_| claim_session_lease_generation as u64),
                }
            },
        )
        .collect()
}
