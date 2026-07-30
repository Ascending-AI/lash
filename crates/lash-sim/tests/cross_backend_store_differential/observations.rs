use super::*;

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
