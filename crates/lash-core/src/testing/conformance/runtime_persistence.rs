//! [`RuntimePersistence`] conformance, organized by capability segment:
//! [`SessionCommitStore`](crate::SessionCommitStore) (head CAS, checkpoint
//! hydration, metadata, attachment manifest, turn-commit stamps),
//! [`SessionExecutionLeaseStore`](crate::SessionExecutionLeaseStore),
//! [`QueuedWorkStore`](crate::QueuedWorkStore) (claim fencing),
//! [`TurnInputStore`](crate::TurnInputStore), and
//! [`StoreMaintenance`](crate::StoreMaintenance).

use super::*;
use crate::facade_support::{SessionGraphFacadeOps, ToolStateFacadeOps};

const CONTROLLED_LEASE_TTL_MS: u64 = 50;
const REALTIME_SCAFFOLDING_LEASE_TTL_MS: u64 = 50;
// Real database operations can be descheduled between claiming a lease and
// observing it. This is a harness stall allowance, not the semantic expiry
// boundary: controlled-clock backends still prove the 50 ms contract exactly.
const REALTIME_LEASE_STALL_ALLOWANCE: std::time::Duration = std::time::Duration::from_secs(5);
const REALTIME_LEASE_OBSERVATION_ATTEMPTS: usize = 3;
const REALTIME_LEASE_EXPIRY_POLL: std::time::Duration = std::time::Duration::from_millis(10);

/// How runtime-persistence conformance drives session-lease expiry.
#[derive(Clone)]
pub enum RuntimePersistenceLeaseTiming {
    /// The backend owns its clock (for example PostgreSQL transaction time).
    Realtime,
    /// The backend reads an injected clock advanced by the supplied callback.
    Controlled(std::sync::Arc<dyn Fn(u64) + Send + Sync>),
}

impl RuntimePersistenceLeaseTiming {
    pub fn controlled(advance: impl Fn(u64) + Send + Sync + 'static) -> Self {
        Self::Controlled(std::sync::Arc::new(advance))
    }

    fn scaffolding_lease_ttl_ms(&self) -> u64 {
        match self {
            Self::Realtime => REALTIME_SCAFFOLDING_LEASE_TTL_MS,
            Self::Controlled(_) => CONTROLLED_LEASE_TTL_MS,
        }
    }

    fn advance_to_just_before_semantic_expiry(&self) {
        if let Self::Controlled(advance) = self {
            advance(CONTROLLED_LEASE_TTL_MS - 1);
        }
    }

    fn advance_to_semantic_expiry(&self) {
        if let Self::Controlled(advance) = self {
            advance(1);
        }
    }

    async fn wait_until_expired(&self) {
        match self {
            Self::Realtime => {
                tokio::time::sleep(std::time::Duration::from_millis(
                    REALTIME_SCAFFOLDING_LEASE_TTL_MS,
                ))
                .await;
            }
            Self::Controlled(advance) => advance(CONTROLLED_LEASE_TTL_MS),
        }
    }
}

/// Run the [`RuntimePersistence`] durability conformance suite against the
/// backend produced by `make`. `make` must return a fresh, empty,
/// single-session store on each call.
///
/// Covers the durability crown jewels owned by the store, grouped by
/// capability segment: optimistic head CAS, session binding, checkpoint/usage
/// hydration, session metadata, attachment manifest intent/commit/GC
/// reconciliation, and idempotent final turn commit stamps
/// ([`SessionCommitStore`](crate::SessionCommitStore)); execution-lane fencing
/// ([`SessionExecutionLeaseStore`](crate::SessionExecutionLeaseStore));
/// queued-work ingress and claim fencing
/// ([`QueuedWorkStore`](crate::QueuedWorkStore)); the pending turn-input
/// lifecycle ([`TurnInputStore`](crate::TurnInputStore)); and tombstone/GC
/// behavior ([`StoreMaintenance`](crate::StoreMaintenance)).
/// Effect-host workflow history is deliberately outside this suite.
pub async fn runtime_persistence<F>(make: F, lease_timing: RuntimePersistenceLeaseTiming)
where
    F: Fn() -> Arc<dyn RuntimePersistence>,
{
    let first = make();
    let second = make();
    assert_fresh_instances(&first, &second, "runtime_persistence");
    drop((first, second));
    runtime_persistence_suite(make, &lease_timing).await;
}

/// Run the full [`RuntimePersistence`] suite plus durable reopen checks.
pub async fn runtime_persistence_reopenable<F>(make: F, lease_timing: RuntimePersistenceLeaseTiming)
where
    F: Fn() -> ReopenableRuntimePersistence,
{
    let probe = make();
    assert_fresh_instances(&probe.open, &probe.reopen, "runtime_persistence_reopenable");
    drop(probe);
    runtime_persistence_suite(|| make().open, &lease_timing).await;
    gc_reclaims_unreachable_checkpoint_blobs_and_preserves_live(make().open).await;
    append_receipt_survives_reopen(make()).await;
    runtime_persistence_survives_reopen(make()).await;
}

/// Prove lease and claim expiry using an injected embedded-backend clock.
///
/// This focused vector proves an embedded store consults its injected
/// [`Clock`](crate::Clock) across session leases and both claim families. Full
/// conformance suites state their timing mode explicitly; the `Realtime` mode
/// keeps its expired-to-reclaimable direction on the production backend clock
/// with bounded polling.
pub async fn runtime_persistence_clock_expiry(
    store: Arc<dyn RuntimePersistence>,
    advance: impl FnOnce(u64),
) {
    const TTL_MS: u64 = 1_000;
    let session_id = "clock-expiry";
    let stale_owner = lease_owner("clock-expiry-stale");
    let successor = lease_owner("clock-expiry-successor");
    let batch = store
        .enqueue_queued_work(queued_draft(
            session_id,
            "clock expiry queued work",
            DeliveryPolicy::EarliestSafeBoundary,
            SlotPolicy::Exclusive,
        ))
        .await
        .expect("enqueue clock-expiry queued work");
    let input = store
        .enqueue_pending_turn_input(pending_next_turn_input_draft(
            session_id,
            "clock expiry turn input",
        ))
        .await
        .expect("enqueue clock-expiry turn input");
    let stale_lease = store
        .try_claim_session_execution_lease(session_id, &stale_owner, TTL_MS)
        .await
        .expect("claim clock-expiry stale lease")
        .acquired()
        .expect("clock-expiry stale lease acquired");
    let stale_queue_claim = store
        .claim_ready_queued_work_by_batch_ids(
            session_id,
            &stale_lease.fence(),
            &stale_owner,
            QueuedWorkClaimBoundary::Idle,
            std::slice::from_ref(&batch.batch_id),
        )
        .await
        .expect("claim clock-expiry queued work")
        .expect("clock-expiry queued work claim exists");
    let stale_input_claim = store
        .claim_next_turn_inputs(session_id, &stale_lease.fence(), &stale_owner, 1)
        .await
        .expect("claim clock-expiry turn input")
        .expect("clock-expiry turn input claim exists");

    advance(TTL_MS);

    let successor_lease = store
        .try_claim_session_execution_lease(session_id, &successor, TTL_MS)
        .await
        .expect("claim clock-expiry successor lease")
        .acquired()
        .expect("expired lease is claimable through injected time");
    assert!(successor_lease.fencing_token > stale_lease.fencing_token);
    let successor_queue_claim = store
        .claim_ready_queued_work_by_batch_ids(
            session_id,
            &successor_lease.fence(),
            &successor,
            QueuedWorkClaimBoundary::Idle,
            std::slice::from_ref(&batch.batch_id),
        )
        .await
        .expect("reclaim clock-expiry queued work")
        .expect("dead-generation queued work is reclaimable");
    let successor_input_claim = store
        .claim_next_turn_inputs(session_id, &successor_lease.fence(), &successor, 1)
        .await
        .expect("reclaim clock-expiry turn input")
        .expect("dead-generation turn input is reclaimable");
    assert!(successor_queue_claim.fencing_token > stale_queue_claim.fencing_token);
    assert!(successor_input_claim.fencing_token > stale_input_claim.fencing_token);

    let stale_state = RuntimeSessionState {
        session_id: session_id.to_string(),
        ..RuntimeSessionState::default()
    };
    let stale_commit = store
        .commit_runtime_state(
            RuntimeCommit::persisted_state_for_test(&stale_state, &[])
                .completing_queue_claim(stale_queue_claim.completion())
                .completing_turn_input_claim(stale_input_claim.completion()),
        )
        .await;
    assert!(matches!(
        stale_commit,
        Err(StoreError::QueuedWorkClaimSuperseded { .. })
    ));

    store
        .commit_runtime_state(
            RuntimeCommit::persisted_state_for_test(&stale_state, &[])
                .releasing_session_execution_lease(successor_lease.completion())
                .completing_queue_claim(successor_queue_claim.completion())
                .completing_turn_input_claim(successor_input_claim.completion()),
        )
        .await
        .expect("successor settles reclaimed clock-expiry claims");
    assert_eq!(stale_queue_claim.batches[0].batch_id, batch.batch_id);
    assert_eq!(stale_input_claim.inputs[0].input_id, input.input_id);
}

async fn runtime_persistence_suite<F>(make: F, lease_timing: &RuntimePersistenceLeaseTiming)
where
    F: Fn() -> Arc<dyn RuntimePersistence>,
{
    // [`SessionCommitStore`]: atomic head commits, reads, metadata, the
    // attachment write-ahead manifest, and turn-commit idempotency.
    commit_increments_head_and_round_trips_agent_frames(make()).await;
    concurrent_head_revision_cas_applies_exactly_once(make()).await;
    commit_rejects_a_different_session_id(make()).await;
    load_hydrates_checkpoint_and_usage(make()).await;
    usage_delta_identity_is_idempotent_across_commits(make()).await;
    usage_ordinal_reuse_with_different_payload_survives_receipt_replay(make()).await;
    checkpoint_rejects_unknown_component_ref(make()).await;
    session_read_loads_persisted_history(make()).await;
    session_metadata_round_trips(make()).await;
    attachment_manifest_records_intent_and_commit_stamps(make()).await;
    attachment_manifest_keeps_same_content_ownership_per_session(make()).await;
    attachment_manifest_reference_tracking_and_gc_root_set(make()).await;
    final_commit_stamp_is_idempotent_and_conflicts_on_changed_hash(make()).await;
    append_request_receipt_replays_after_head_advance(make()).await;
    append_request_receipt_rejects_changed_content(make()).await;
    append_request_exact_hash_rejects_changed_ancestor(make()).await;
    append_request_receipt_rejects_corrupt_node_count(make()).await;
    concurrent_same_append_operation_applies_exactly_once(make()).await;
    legacy_append_receipt_keeps_exact_hash_semantics(make()).await;
    append_receipt_encoding_version_mismatch_keeps_exact_hash_semantics(make()).await;
    append_receipt_and_graph_append_are_atomic(make()).await;
    fresh_append_receipt_enforces_ancestor_precondition(make()).await;
    store_computed_hash_rejects_mutated_commit(make()).await;
    commit_rejects_non_derived_append_node_ids(make()).await;
    append_rejects_duplicate_batch_node_ids(make()).await;
    append_rejects_existing_node_id_collision(make()).await;
    commit_rejects_unresolvable_leaf(make()).await;
    commit_rejects_missing_leaf(make()).await;
    empty_append_cannot_move_the_head(make()).await;
    commit_rejects_leaf_without_frame_open_ancestor(make()).await;
    // [`SessionExecutionLeaseStore`]: single-writer lane fencing.
    session_execution_lease_contract(make()).await;
    session_execution_lease_expires_by_ttl_contract(&make, lease_timing).await;
    session_execution_lease_diagnostic_read_contract(make()).await;
    session_execution_lease_displacement_contract(make()).await;
    // [`QueuedWorkStore`]: durable queued-work ingress, ordering, and claim
    // leases, plus the commit-side completion atomicity it shares with
    // [`SessionCommitStore`].
    queued_work_source_keys_are_idempotent_and_list_ordered(make()).await;
    concurrent_queue_and_turn_input_claims_have_one_owner(make()).await;
    checkpoint_work_claims_both_families_once(make()).await;
    queued_work_cancel_removes_only_unclaimed_batches(make()).await;
    queued_work_exact_claim_uses_selected_batch_ids(make()).await;
    queued_work_classes_gate_command_and_turn_claims(make()).await;
    queued_work_claims_respect_boundaries_abandon_and_stale_completion(make()).await;
    queued_work_claims_supersede_across_session_lease_generations_with_timing(make(), lease_timing)
        .await;
    claim_liveness_for_lease_less_paths_tracks_session_generations(make(), lease_timing).await;
    same_generation_claim_scans_reach_rows_beyond_the_scan_surplus(make()).await;
    queued_work_respects_membership_limits_exclusivity_reclaim_and_sessions(make()).await;
    queued_work_join_groups_by_delivery_policy_and_merge_key(make()).await;
    wake_turn_policy_controls_coalescing(make()).await;
    queued_work_completion_is_lease_guarded(make()).await;
    queued_wake_delivery_is_source_key_idempotent_and_claimed_once(make()).await;
    queue_completion_and_turn_commit_stamp_are_atomic(make()).await;
    // [`TurnInputStore`]: pending turn-input lifecycle.
    pending_turn_inputs_source_keys_order_cancel_and_cross_session(make()).await;
    pending_turn_input_bulk_and_suffix_cancellation(make()).await;
    pending_turn_input_claims_reclaim_complete_and_fence(make()).await;
    turn_input_application_identity_survives_pending_tombstone_vacuum(make()).await;
    turn_input_claims_supersede_across_session_lease_generations_with_timing(make(), lease_timing)
        .await;
    active_turn_input_claim_reacquires_after_unrecorded_checkpoint(make()).await;
    pending_turn_input_cancel_covers_active_and_deferred_states(make()).await;
    pending_active_turn_inputs_defer_unaccepted_once_on_interrupt(make()).await;
}

async fn usage_delta_identity_is_idempotent_across_commits(store: Arc<dyn RuntimePersistence>) {
    let usage = TokenLedgerEntry {
        source: "idempotent-republish".to_string(),
        model: "usage-model".to_string(),
        usage: crate::TokenUsage {
            input_tokens: 11,
            output_tokens: 7,
            cache_read_input_tokens: 5,
            cache_write_input_tokens: 3,
            reasoning_output_tokens: 2,
        },
    };
    let state = RuntimeSessionState {
        session_id: "root".to_string(),
        ..RuntimeSessionState::default()
    };
    let first = RuntimeCommit::persisted_state_for_test(&state, std::slice::from_ref(&usage));
    let durable_identity = first.usage_deltas[0].identity.clone();
    let first_result = commit_runtime_state_for_test(&store, first, "usage identity first")
        .await
        .expect("publish first usage identity");

    let mut next_state = loaded_conformance_state(&store).await;
    next_state.head_revision = first_result.head_revision;
    let mut republish = RuntimeCommit::persisted_state_for_test(&next_state, &[]);
    republish.usage_deltas = vec![crate::store::RuntimeUsageDelta {
        identity: durable_identity.clone(),
        entry: usage.clone(),
    }];
    let republished = commit_runtime_state_for_test(&store, republish, "usage identity retry")
        .await
        .expect("republish existing usage identity");
    assert_eq!(
        republished.committed_usage_delta_identities,
        vec![durable_identity]
    );

    let read = store
        .load_session()
        .await
        .expect("load idempotent usage")
        .expect("usage session exists");
    let matching = read
        .token_ledger
        .iter()
        .filter(|entry| entry.source == usage.source && entry.model == usage.model)
        .collect::<Vec<_>>();
    assert_eq!(matching.len(), 1);
    assert_eq!(matching[0].usage, usage.usage);
}

async fn usage_ordinal_reuse_with_different_payload_survives_receipt_replay(
    store: Arc<dyn RuntimePersistence>,
) {
    let usage = |input_tokens| TokenLedgerEntry {
        source: "ordinal-reuse".to_string(),
        model: "usage-model".to_string(),
        usage: crate::TokenUsage {
            input_tokens,
            output_tokens: 0,
            cache_read_input_tokens: 0,
            cache_write_input_tokens: 0,
            reasoning_output_tokens: 0,
        },
    };
    let first_usage = usage(11);
    let later_usage = usage(29);
    let nodes = vec![crate::SessionAppendNode::plugin(
        "usage-ordinal-reuse",
        serde_json::json!({"append": "A"}),
    )];
    let mut initial_state = RuntimeSessionState {
        session_id: "root".to_string(),
        ..RuntimeSessionState::default()
    };

    // U1 is confirmed under append operation A at ordinal zero.
    let (mut first_append, _) =
        append_request_commit(&mut initial_state, "usage-ordinal-reuse-a", &nodes, None);
    first_append.usage_deltas = crate::store::RuntimeUsageDelta::for_operation(
        &first_append.turn_commit.operation,
        std::slice::from_ref(&first_usage),
    )
    .expect("identify first usage row");
    let first_identity = first_append.usage_deltas[0].identity.clone();
    let first_result =
        commit_runtime_state_for_test(&store, first_append, "usage-ordinal-reuse-first")
            .await
            .expect("commit append A with U1");
    assert_eq!(
        first_result.committed_usage_delta_identities,
        vec![first_identity.clone()]
    );

    // U2 is recorded after U1 confirmation. Replaying A reuses ordinal zero,
    // but its content-bound full identity is distinct.
    let mut retry_state = loaded_conformance_state(&store).await;
    let (mut replay_append, _) =
        append_request_commit(&mut retry_state, "usage-ordinal-reuse-a", &nodes, None);
    replay_append.usage_deltas = crate::store::RuntimeUsageDelta::for_operation(
        &replay_append.turn_commit.operation,
        std::slice::from_ref(&later_usage),
    )
    .expect("identify later usage row");
    let later_delta = replay_append.usage_deltas[0].clone();
    assert_eq!(
        later_delta.identity.operation_storage_key,
        first_identity.operation_storage_key
    );
    assert_eq!(
        later_delta.identity.entry_ordinal,
        first_identity.entry_ordinal
    );
    assert_ne!(
        later_delta.identity.payload_hash,
        first_identity.payload_hash
    );

    let replay = commit_runtime_state_for_test(&store, replay_append, "usage-ordinal-reuse-replay")
        .await
        .expect("replay append A with U2 staged");
    assert!(replay.receipt_replayed);
    assert_eq!(
        replay.committed_usage_delta_identities,
        vec![first_identity]
    );
    assert!(
        !replay
            .committed_usage_delta_identities
            .contains(&later_delta.identity),
        "receipt replay must not confirm a different payload at the reused ordinal"
    );

    // The caller therefore retains U2 and publishes it on the next natural
    // commit. Both full identities must be durable exactly once.
    let mut natural_state = loaded_conformance_state(&store).await;
    let mut natural_commit = RuntimeCommit::persisted_state_for_test(&natural_state, &[]);
    natural_commit.usage_deltas = vec![later_delta.clone()];
    let natural =
        commit_runtime_state_for_test(&store, natural_commit, "usage-ordinal-reuse-natural")
            .await
            .expect("publish U2 on next natural commit");
    assert_eq!(
        natural.committed_usage_delta_identities,
        vec![later_delta.identity]
    );

    natural_state = loaded_conformance_state(&store).await;
    let durable = natural_state
        .token_ledger
        .iter()
        .find(|entry| entry.source == "ordinal-reuse" && entry.model == "usage-model")
        .expect("merged U1 and U2 are durable");
    assert_eq!(durable.usage.input_tokens, 40);
}

fn append_request_commit(
    state: &mut RuntimeSessionState,
    operation_id: &str,
    nodes: &[crate::SessionAppendNode],
    requested_ancestor_node_id: Option<&str>,
) -> (RuntimeCommit, Vec<String>) {
    let operation = crate::runtime::state::boundary_operation(
        &state.session_id,
        operation_id,
        "append-session-nodes",
    );
    let stamp = RuntimeTurnCommitStamp::append_session_nodes(
        operation.clone(),
        requested_ancestor_node_id,
        nodes,
    )
    .expect("append request identity");
    let draft_namespace = operation
        .storage_key()
        .expect("append operation storage key");
    let requested_node_count = crate::runtime::state::append_session_nodes_to_state_with_clock(
        state,
        nodes,
        &draft_namespace,
        &crate::SystemClock,
    )
    .len();
    let mut graph = state.pending_graph_commit();
    let mapping = graph
        .derive_node_ids(&state.session_id, &operation)
        .expect("derive append node ids");
    let persisted = mapping
        .iter()
        .map(|(_, derived)| derived.clone())
        .collect::<Vec<_>>();
    let requested_ids = persisted[persisted.len().saturating_sub(requested_node_count)..].to_vec();
    let mut commit = RuntimeCommit::persisted_state_with_graph_commit_and_operation(
        state,
        graph,
        &[],
        operation,
    )
    .expect("build append request commit");
    commit.turn_commit = stamp;
    (commit, requested_ids)
}

async fn loaded_conformance_state(store: &Arc<dyn RuntimePersistence>) -> RuntimeSessionState {
    crate::store::load_persisted_session_state(store.as_ref())
        .await
        .expect("load conformance append state")
        .expect("conformance append state exists")
}

async fn seed_append_receipt_state(store: &Arc<dyn RuntimePersistence>) -> RuntimeSessionState {
    let mut state = RuntimeSessionState {
        session_id: "root".to_string(),
        ..RuntimeSessionState::default()
    };
    let nodes = vec![crate::SessionAppendNode::plugin(
        "append-receipt-seed",
        serde_json::json!({"seed": true}),
    )];
    let (commit, _) = append_request_commit(&mut state, "append-receipt-seed", &nodes, None);
    commit_runtime_state_for_test(store, commit, "append-receipt-seed")
        .await
        .expect("seed append receipt state");
    loaded_conformance_state(store).await
}

async fn append_request_receipt_replays_after_head_advance(store: Arc<dyn RuntimePersistence>) {
    let mut state = seed_append_receipt_state(&store).await;
    let required = state.session_graph.leaf_node_id.clone().expect("seed leaf");
    let nodes = vec![crate::SessionAppendNode::plugin(
        "append-receipt",
        serde_json::json!({"value": 1}),
    )];
    let (first_commit, first_node_ids) =
        append_request_commit(&mut state, "head-advanced-retry", &nodes, Some(&required));
    let first_hash = first_commit.turn_commit_hash().expect("first append hash");
    let first = commit_runtime_state_for_test(&store, first_commit, "head-advanced-first")
        .await
        .expect("first append receipt commit");

    let mut advanced = loaded_conformance_state(&store).await;
    let advance_nodes = vec![crate::SessionAppendNode::plugin(
        "append-receipt",
        serde_json::json!({"value": 2}),
    )];
    let (advance_commit, _) =
        append_request_commit(&mut advanced, "head-advanced-other", &advance_nodes, None);
    commit_runtime_state_for_test(&store, advance_commit, "head-advanced-other")
        .await
        .expect("advance append receipt head");

    let mut retry_state = loaded_conformance_state(&store).await;
    let (retry_commit, retry_node_ids) = append_request_commit(
        &mut retry_state,
        "head-advanced-retry",
        &nodes,
        Some(&required),
    );
    assert_ne!(
        retry_commit.turn_commit_hash().expect("retry hash"),
        first_hash,
        "head movement must change the whole-commit hash used by the legacy receipt arm"
    );
    let retry = store
        .commit_runtime_state(retry_commit)
        .await
        .expect("head-advanced append retry replays");
    assert!(retry.receipt_replayed);
    assert_eq!(retry_node_ids, first_node_ids);
    assert_eq!(retry.head_revision, first.head_revision);
    assert_eq!(retry.checkpoint_ref, first.checkpoint_ref);
    assert_eq!(retry.committed_leaf_node_id, first.committed_leaf_node_id);
    assert_eq!(
        retry.realized_node_timestamps,
        first.realized_node_timestamps
    );
    let read = store
        .load_session()
        .await
        .expect("load exactly-once append")
        .expect("append session");
    for node_id in first_node_ids {
        assert_eq!(
            read.graph
                .nodes
                .iter()
                .filter(|node| node.node_id == node_id)
                .count(),
            1,
            "the retried append node must exist exactly once"
        );
    }
}

async fn append_request_receipt_rejects_changed_content(store: Arc<dyn RuntimePersistence>) {
    let mut state = seed_append_receipt_state(&store).await;
    let original_nodes = vec![crate::SessionAppendNode::plugin(
        "append-receipt",
        serde_json::json!({"value": "original"}),
    )];
    let (first_commit, first_ids) =
        append_request_commit(&mut state, "changed-content", &original_nodes, None);
    commit_runtime_state_for_test(&store, first_commit, "changed-content-first")
        .await
        .expect("first changed-content append");
    let before = store
        .load_session()
        .await
        .expect("load before conflict")
        .unwrap();

    let mut retry_state = loaded_conformance_state(&store).await;
    let changed_nodes = vec![crate::SessionAppendNode::plugin(
        "append-receipt",
        serde_json::json!({"value": "changed"}),
    )];
    let (changed_commit, _) =
        append_request_commit(&mut retry_state, "changed-content", &changed_nodes, None);
    let error = store
        .commit_runtime_state(changed_commit)
        .await
        .expect_err("operation id reuse with changed content must conflict");
    assert!(matches!(
        error,
        StoreError::AppendOperationIdentityConflict { ref session_id, .. }
            if session_id == "root"
    ));
    let after = store
        .load_session()
        .await
        .expect("load after conflict")
        .unwrap();
    assert_eq!(after.head_revision, before.head_revision);
    assert_eq!(after.graph.leaf_node_id, before.graph.leaf_node_id);
    assert_eq!(after.graph.nodes.len(), before.graph.nodes.len());
    assert!(
        first_ids
            .iter()
            .all(|id| after.graph.find_node(id).is_some())
    );
}

async fn append_request_exact_hash_rejects_changed_ancestor(store: Arc<dyn RuntimePersistence>) {
    let mut state = seed_append_receipt_state(&store).await;
    let required = state.session_graph.leaf_node_id.clone().expect("seed leaf");
    let nodes = vec![crate::SessionAppendNode::plugin(
        "append-receipt",
        serde_json::json!({"value": "changed-ancestor"}),
    )];
    let (first, _) = append_request_commit(
        &mut state,
        "changed-ancestor-exact-hash",
        &nodes,
        Some(&required),
    );
    let mut changed_ancestor = first.clone();
    changed_ancestor.turn_commit = RuntimeTurnCommitStamp::append_session_nodes(
        first.turn_commit.operation.clone(),
        None,
        &nodes,
    )
    .expect("changed ancestor identity");
    assert_eq!(
        first.turn_commit_hash().expect("first hash"),
        changed_ancestor.turn_commit_hash().expect("retry hash"),
        "ancestor metadata is intentionally outside the whole-commit hash"
    );
    commit_runtime_state_for_test(&store, first, "changed-ancestor-first")
        .await
        .expect("first changed-ancestor append");

    let error = store
        .commit_runtime_state(changed_ancestor)
        .await
        .expect_err("changed requested ancestor must conflict despite an exact commit hash");
    assert!(matches!(
        error,
        StoreError::AppendOperationIdentityConflict { .. }
    ));
}

async fn append_request_receipt_rejects_corrupt_node_count(store: Arc<dyn RuntimePersistence>) {
    let mut state = seed_append_receipt_state(&store).await;
    let nodes = vec![crate::SessionAppendNode::plugin(
        "append-receipt",
        serde_json::json!({"value": "count-cross-check"}),
    )];
    let (first, _) = append_request_commit(&mut state, "count-cross-check", &nodes, None);
    let mut corrupt_retry = first.clone();
    corrupt_retry.turn_commit.requested_node_count = Some(nodes.len() + 1);
    commit_runtime_state_for_test(&store, first, "count-cross-check-first")
        .await
        .expect("first count-cross-check append");

    let error = store
        .commit_runtime_state(corrupt_retry)
        .await
        .expect_err("matching receipt hashes with a different node count are corruption");
    assert!(matches!(
        error,
        StoreError::AppendReceiptRequestedNodeCountCorrupt {
            stored: Some(1),
            attempted: Some(2),
            ..
        }
    ));
}

async fn concurrent_same_append_operation_applies_exactly_once(store: Arc<dyn RuntimePersistence>) {
    let state = RuntimeSessionState {
        session_id: "root".to_string(),
        ..RuntimeSessionState::default()
    };
    let nodes = vec![crate::SessionAppendNode::plugin(
        "append-receipt-race",
        serde_json::json!({"value": "same-operation"}),
    )];
    let (left, node_ids) =
        append_request_commit(&mut state.clone(), "same-operation-race", &nodes, None);
    let (right, right_node_ids) =
        append_request_commit(&mut state.clone(), "same-operation-race", &nodes, None);
    assert_eq!(node_ids, right_node_ids);
    assert_eq!(
        left.turn_commit_hash().expect("left race hash"),
        right.turn_commit_hash().expect("right race hash")
    );

    let (left_result, right_result) = tokio::join!(
        store.commit_runtime_state(left),
        store.commit_runtime_state(right)
    );
    let left_result = left_result.expect("left same-operation race result");
    let right_result = right_result.expect("right same-operation race result");
    assert_ne!(
        left_result.receipt_replayed, right_result.receipt_replayed,
        "one concurrent attempt must publish and the other must replay"
    );
    let read = store
        .load_session()
        .await
        .expect("load same-operation race")
        .expect("same-operation race session");
    for node_id in node_ids {
        assert_eq!(
            read.graph
                .nodes
                .iter()
                .filter(|node| node.node_id == node_id)
                .count(),
            1,
            "the concurrent same-operation node must be durable exactly once"
        );
    }
}

/// Prove that a durable append receipt wins after a branch switch removes the
/// request's ancestor from the active path.
///
/// `supersede` is a backend test hook that atomically moves the durable leaf to
/// the supplied earlier node and advances the head revision. Conformance-suite
/// embedders use their backend's raw test access for that single mutation.
///
/// Integrator class (ADR 0051): **conformance-suite embedders**.
pub async fn append_request_receipt_replays_after_ancestor_superseded<F, Fut>(
    store: Arc<dyn RuntimePersistence>,
    supersede: F,
) where
    F: FnOnce(String) -> Fut,
    Fut: std::future::Future<Output = ()>,
{
    let mut state = seed_append_receipt_state(&store).await;
    let required = state.session_graph.leaf_node_id.clone().expect("seed leaf");
    let superseding_leaf = state
        .session_graph
        .find_node(&required)
        .and_then(|node| node.parent_node_id.clone())
        .expect("seed append has the initial frame as parent");
    let nodes = vec![crate::SessionAppendNode::plugin(
        "append-receipt",
        serde_json::json!({"value": "ancestor"}),
    )];
    let (first_commit, _) =
        append_request_commit(&mut state, "ancestor-replay", &nodes, Some(&required));
    let first = commit_runtime_state_for_test(&store, first_commit, "ancestor-replay-first")
        .await
        .expect("first ancestor append");

    supersede(superseding_leaf).await;
    let mut retry_state = loaded_conformance_state(&store).await;
    assert!(
        !retry_state.session_graph.active_path_contains(&required),
        "the backend hook must move the requested ancestor off the active path"
    );
    let (retry, _) =
        append_request_commit(&mut retry_state, "ancestor-replay", &nodes, Some(&required));
    let replay = store
        .commit_runtime_state(retry)
        .await
        .expect("receipt replay must precede the fresh ancestor fence");
    assert!(replay.receipt_replayed);
    assert_eq!(replay.head_revision, first.head_revision);
}

async fn legacy_append_receipt_keeps_exact_hash_semantics(store: Arc<dyn RuntimePersistence>) {
    let mut state = seed_append_receipt_state(&store).await;
    let nodes = vec![crate::SessionAppendNode::plugin(
        "append-receipt",
        serde_json::json!({"value": "legacy"}),
    )];
    let (mut legacy, _) = append_request_commit(&mut state, "legacy-receipt", &nodes, None);
    legacy.turn_commit = RuntimeTurnCommitStamp::new(legacy.turn_commit.operation.clone());
    let exact_retry = legacy.clone();
    commit_runtime_state_for_test(&store, legacy, "legacy-receipt-first")
        .await
        .expect("first legacy receipt");
    let replay = store
        .commit_runtime_state(exact_retry.clone())
        .await
        .expect("legacy exact-hash retry replays");
    assert!(replay.receipt_replayed);

    let mut changed = exact_retry;
    changed.checkpoint.turn_state.turn_index += 1;
    let error = store
        .commit_runtime_state(changed)
        .await
        .expect_err("legacy changed-hash retry conflicts");
    assert!(matches!(
        error,
        StoreError::RuntimeTurnCommitConflict { .. }
    ));
}

async fn append_receipt_encoding_version_mismatch_keeps_exact_hash_semantics(
    store: Arc<dyn RuntimePersistence>,
) {
    let mut state = seed_append_receipt_state(&store).await;
    let nodes = vec![crate::SessionAppendNode::plugin(
        "append-receipt",
        serde_json::json!({"value": "versioned"}),
    )];
    let (mut future_version, _) =
        append_request_commit(&mut state, "version-mismatch", &nodes, None);
    future_version.turn_commit.identity_encoding_version = future_version
        .turn_commit
        .identity_encoding_version
        .map(|version| version + 1);
    let mut exact_retry = future_version.clone();
    exact_retry.turn_commit.identity_encoding_version = Some(1);
    commit_runtime_state_for_test(&store, future_version, "version-mismatch-first")
        .await
        .expect("first future-version receipt");
    let exact = store
        .commit_runtime_state(exact_retry.clone())
        .await
        .expect("version mismatch exact-hash retry replays");
    assert!(exact.receipt_replayed);

    let mut changed = exact_retry;
    changed.checkpoint.turn_state.turn_index += 1;
    let error = store
        .commit_runtime_state(changed)
        .await
        .expect_err("version mismatch changed-hash retry uses legacy conflict");
    assert!(matches!(
        error,
        StoreError::RuntimeTurnCommitConflict { .. }
    ));
}

async fn append_receipt_and_graph_append_are_atomic(store: Arc<dyn RuntimePersistence>) {
    let mut state = RuntimeSessionState {
        session_id: "root".to_string(),
        ..RuntimeSessionState::default()
    };
    let nodes = vec![crate::SessionAppendNode::plugin(
        "append-receipt",
        serde_json::json!({"value": "atomic"}),
    )];
    let (clean, ids) = append_request_commit(&mut state, "atomic-append", &nodes, None);
    let mut failing = clean.clone();
    failing
        .enqueued_queue_batches
        .push(QueuedWorkBatchDraft::new(
            "different-session",
            DeliveryPolicy::AfterCurrentTurnCommit,
            SlotPolicy::Exclusive,
            vec![QueuedWorkPayload::agent_frame_task(
                "atomic-frame",
                "must roll back",
                None,
            )],
        ));
    let failing_lease =
        claim_session_execution_lease_for_test(&store, "root", "atomic-append-failing").await;
    let error = store
        .commit_runtime_state(failing.releasing_session_execution_lease(failing_lease.completion()))
        .await
        .expect_err("mid-commit outbox failure rolls back append and receipt");
    assert!(matches!(error, StoreError::SessionBindingMismatch { .. }));
    assert!(
        store
            .load_session()
            .await
            .expect("load failed append")
            .is_none()
    );
    release_session_execution_lease_for_test(&store, &failing_lease).await;

    commit_runtime_state_for_test(&store, clean, "atomic-append-retry")
        .await
        .expect("fresh retry after rollback succeeds");
    let read = store.load_session().await.expect("load retry").unwrap();
    assert!(ids.iter().all(|id| read.graph.find_node(id).is_some()));
}

async fn fresh_append_receipt_enforces_ancestor_precondition(store: Arc<dyn RuntimePersistence>) {
    let mut state = seed_append_receipt_state(&store).await;
    let before = store
        .load_session()
        .await
        .expect("load before stale")
        .unwrap();
    let nodes = vec![crate::SessionAppendNode::plugin(
        "append-receipt",
        serde_json::json!({"value": "stale"}),
    )];
    let (fresh, _) = append_request_commit(
        &mut state,
        "fresh-stale-ancestor",
        &nodes,
        Some("not-on-the-active-path"),
    );
    let error = store
        .commit_runtime_state(fresh)
        .await
        .expect_err("fresh append must enforce ancestor precondition");
    assert!(matches!(
        error,
        StoreError::AppendAncestorNotActive { ref required_node_id }
            if required_node_id == "not-on-the-active-path"
    ));
    let after = store
        .load_session()
        .await
        .expect("load after stale")
        .unwrap();
    assert_eq!(after.head_revision, before.head_revision);
    assert_eq!(after.graph.leaf_node_id, before.graph.leaf_node_id);
    assert_eq!(after.graph.nodes.len(), before.graph.nodes.len());
}

/// A backend must mint refs for checkpoint bodies and resolve those refs after
/// both the ref-only successor write and the final read reopen the substrate.
///
/// This is the standing regression for the checkpoint-component failure
/// shape: write bodies, drop the writer, write only their refs through an
/// independently constructed handle, drop that writer, then construct a third
/// handle and hydrate. The helper owns construction order so a caller cannot
/// prebuild nominally cold handles before the writes they verify.
pub async fn checkpoint_component_refs_survive_cold_reopens<F>(make: F)
where
    F: Fn() -> Arc<dyn RuntimePersistence>,
{
    let open = make();
    let open_identity = Arc::downgrade(&open);
    bind_conformance_session(&open, "checkpoint-component-refs").await;
    let mut state = RuntimeSessionState {
        session_id: "checkpoint-component-refs".to_string(),
        tool_state_snapshot: Some(ToolState::default().with_generation(91)),
        plugin_snapshot_revision: Some(37),
        plugin_snapshot: Some(PluginSessionSnapshot::default()),
        execution_state_snapshot: Some(b"opaque-execution-state-before-clean-commit".to_vec()),
        ..RuntimeSessionState::default()
    };

    let first = commit_runtime_state_for_test(
        &open,
        RuntimeCommit::persisted_state_for_test(&state, &[]),
        "checkpoint-component-refs-first",
    )
    .await
    .expect("commit checkpoint component bodies");
    assert!(
        first.manifest.tool_state_ref.is_some(),
        "a stored tool-state body must return its content ref"
    );
    assert!(
        first.manifest.plugin_snapshot_ref.is_some(),
        "a stored plugin-snapshot body must return its content ref"
    );
    assert!(
        first.manifest.execution_state_ref.is_some(),
        "a stored execution-state body must return its content ref"
    );

    state.apply_persisted_commit_result(first);
    assert!(state.tool_state_snapshot.is_none());
    assert!(state.plugin_snapshot.is_none());
    assert!(state.execution_state_snapshot.is_none());
    drop(open);

    let reopen = make();
    assert!(
        !std::sync::Weak::ptr_eq(&open_identity, &Arc::downgrade(&reopen)),
        "checkpoint-component reopen factory reused the writer handle"
    );
    let reopen_identity = Arc::downgrade(&reopen);
    bind_conformance_session(&reopen, "checkpoint-component-refs").await;

    let second = commit_runtime_state_for_test(
        &reopen,
        RuntimeCommit::persisted_state_for_test(&state, &[]),
        "checkpoint-component-refs-second",
    )
    .await
    .expect("commit unchanged checkpoint component refs");
    state.apply_persisted_commit_result(second);
    drop(reopen);

    let cold_reopen = make();
    assert!(
        !std::sync::Weak::ptr_eq(&open_identity, &Arc::downgrade(&cold_reopen))
            && !std::sync::Weak::ptr_eq(&reopen_identity, &Arc::downgrade(&cold_reopen)),
        "checkpoint-component cold reader reused a writer handle"
    );
    bind_conformance_session(&cold_reopen, "checkpoint-component-refs").await;
    let read = cold_reopen
        .load_session()
        .await
        .expect("cold-load refs-only checkpoint")
        .expect("refs-only checkpoint session");
    let checkpoint = read.checkpoint.expect("hydrated refs-only checkpoint");
    assert_eq!(
        checkpoint.tool_state.as_ref().map(ToolState::generation),
        Some(91)
    );
    assert!(
        checkpoint.plugin_snapshot.is_some(),
        "refs-only commit must preserve the plugin snapshot body"
    );
    assert_eq!(checkpoint.plugin_snapshot_revision, Some(37));
    assert_eq!(
        checkpoint.execution_state.as_deref(),
        Some(&b"opaque-execution-state-before-clean-commit"[..]),
        "refs-only commit must preserve execution state across a cold load"
    );
}

/// A ref-only checkpoint commit is valid only when every referenced component
/// already exists in the backend.
pub async fn checkpoint_rejects_unknown_component_ref(store: Arc<dyn RuntimePersistence>) {
    let state = RuntimeSessionState {
        session_id: "checkpoint-unknown-ref".to_string(),
        execution_state_ref: Some(crate::BlobRef(
            "checkpoint-component-that-was-never-stored".to_string(),
        )),
        ..RuntimeSessionState::default()
    };

    let error = commit_runtime_state_for_test(
        &store,
        RuntimeCommit::persisted_state_for_test(&state, &[]),
        "checkpoint-unknown-ref",
    )
    .await
    .expect_err("a checkpoint must reject a ref whose body is absent");
    assert!(
        error
            .to_string()
            .contains("checkpoint-component-that-was-never-stored"),
        "missing-component error must identify the unresolved ref: {error}"
    );
}

async fn commit_rejects_leaf_without_frame_open_ancestor(store: Arc<dyn RuntimePersistence>) {
    let state = RuntimeSessionState {
        session_id: "missing-frame-root".to_string(),
        ..RuntimeSessionState::default()
    };
    let node = SessionNodeRecord {
        node_id: "unframed-root".to_string(),
        parent_node_id: None,
        timestamp: "2026-07-27T00:00:00Z".to_string(),
        payload: SessionNodePayload::Event {
            event: crate::SessionHistoryRecord::Protocol(
                ProtocolEvent::typed("unframed", serde_json::Value::Null).expect("protocol event"),
            ),
        },
    };
    let commit = RuntimeCommit::persisted_state_with_graph_commit(
        &state,
        crate::GraphAppend {
            nodes: vec![node],
            leaf_node_id: Some("unframed-root".to_string()),
        },
        &[],
    );
    let expected_leaf_node_id = commit
        .graph
        .leaf_node_id
        .clone()
        .expect("derived unframed leaf");

    let error = store
        .commit_runtime_state(commit)
        .await
        .expect_err("every root graph must open with FrameOpen");

    assert!(matches!(
        error,
        StoreError::MissingFrameOpenAncestor { leaf_node_id }
            if leaf_node_id == expected_leaf_node_id
    ));
}

async fn turn_input_application_identity_survives_pending_tombstone_vacuum(
    store: Arc<dyn RuntimePersistence>,
) {
    let session_id = "turn-input-application";
    let owner_id = "turn-input-application-owner";
    let lease = claim_session_execution_lease_for_test(&store, session_id, owner_id).await;
    let mut state = RuntimeSessionState {
        session_id: session_id.to_string(),
        ..RuntimeSessionState::default()
    };
    let mut expected = Vec::new();
    let mut replay = None;

    for (turn_index, turn_id) in ["z-first-application-turn", "a-second-application-turn"]
        .into_iter()
        .enumerate()
    {
        let admitted = futures_util::future::try_join_all((0..2).map(|input_index| {
            store.enqueue_pending_turn_input(
                pending_next_turn_input_draft(
                    session_id,
                    &format!("canonical application {turn_index}:{input_index}"),
                )
                .with_source_key(format!(
                    "host:application-source-{turn_index}-{input_index}"
                )),
            )
        }))
        .await
        .expect("enqueue application inputs");
        let mut claim = store
            .claim_next_turn_inputs(session_id, &lease.fence(), &lease_owner(owner_id), 10)
            .await
            .expect("claim application inputs")
            .expect("application input claim");
        let committed_message_id = format!("application-message-{turn_index}");
        claim.record_initial_turn_application(&crate::TurnId::from(turn_id), &committed_message_id);
        let turn_expected = claim.applications.clone();
        assert_eq!(admitted.len(), turn_expected.len());

        let mut commit = RuntimeCommit::persisted_state_for_test(&state, &[])
            .completing_turn_input_claim(claim.completion());
        if turn_index == 1 {
            commit = commit.releasing_session_execution_lease(lease.completion());
        }
        commit.turn_commit = crate::RuntimeTurnCommitStamp::new(crate::OperationId::turn(
            session_id, turn_id, "final",
        ));
        if turn_index == 1 {
            replay = Some(commit.clone());
        }
        let result = store
            .commit_runtime_state(commit)
            .await
            .expect("commit application identity");
        state.head_revision = result.head_revision;

        assert_eq!(result.turn_input_applications, turn_expected);
        expected.extend(turn_expected);
    }

    let replayed = store
        .commit_runtime_state(replay.expect("second turn commit replay"))
        .await
        .expect("replay application turn commit");
    assert_eq!(
        replayed.turn_input_applications,
        expected[2..],
        "an exact turn-commit replay must retain its applications"
    );
    assert_eq!(
        store
            .list_turn_input_applications(session_id)
            .await
            .expect("read durable application identity"),
        expected,
        "applications must follow monotonic turn-commit order and must not double-count a replay"
    );

    store.vacuum().await.expect("vacuum application tombstone");
    assert_eq!(
        store
            .list_turn_input_applications(session_id)
            .await
            .expect("read application identity after tombstone vacuum"),
        expected,
        "application reconciliation must come from the committed turn, not a pending snapshot"
    );
}

async fn checkpoint_work_claims_both_families_once(store: Arc<dyn RuntimePersistence>) {
    let session_id = "checkpoint-work";
    let turn_id = crate::TurnId::from("checkpoint-turn");
    let owner = lease_owner("checkpoint-owner");
    let input = store
        .enqueue_pending_turn_input(pending_active_turn_input_draft(
            session_id,
            turn_id.as_str(),
            crate::TurnInputCheckpointBoundary::AfterWork,
            "checkpoint input",
        ))
        .await
        .expect("enqueue checkpoint input");
    let batch = store
        .enqueue_queued_work(queued_draft(
            session_id,
            "checkpoint queued work",
            DeliveryPolicy::EarliestSafeBoundary,
            SlotPolicy::Exclusive,
        ))
        .await
        .expect("enqueue checkpoint queued work");
    let lease = store
        .try_claim_session_execution_lease(session_id, &owner, 60_000)
        .await
        .expect("claim checkpoint session lease")
        .acquired()
        .expect("checkpoint session lease acquired");

    let (input_claim, queue_claim) = store
        .claim_checkpoint_work(
            session_id,
            &lease.fence(),
            &owner,
            &turn_id,
            crate::CheckpointKind::AfterWork,
            10,
            10,
        )
        .await
        .expect("claim both checkpoint work families");
    let input_claim = input_claim.expect("checkpoint input claim exists");
    let queue_claim = queue_claim.expect("checkpoint queue claim exists");
    assert_eq!(input_claim.inputs[0].input_id, input.input_id);
    assert_eq!(queue_claim.batches[0].batch_id, batch.batch_id);
    assert_eq!(input_claim.session_lease_generation, lease.fencing_token);
    assert_eq!(queue_claim.session_lease_generation, lease.fencing_token);

    let second = store
        .claim_checkpoint_work(
            session_id,
            &lease.fence(),
            &owner,
            &turn_id,
            crate::CheckpointKind::AfterWork,
            10,
            10,
        )
        .await
        .expect("same-generation checkpoint re-claim");
    assert!(
        second.0.is_none() && second.1.is_none(),
        "checkpoint claims must be granted exactly once per lease generation"
    );
}

/// Prove checkpoint admission probes stay read-only for empty queues and for
/// deferred queue heads, while real checkpoint work still shares one write
/// transaction and deferred work remains claimable at the idle boundary.
pub async fn checkpoint_claim_probe_transaction_counts(
    store: Arc<dyn RuntimePersistence>,
    session_id: &str,
    counts: impl Fn() -> (usize, usize),
) {
    let turn_id = crate::TurnId::from(format!("{session_id}:counter-turn"));
    let owner = lease_owner(&format!("{session_id}:checkpoint-counter-owner"));
    let lease = store
        .try_claim_session_execution_lease(session_id, &owner, 60_000)
        .await
        .expect("claim checkpoint counter lease")
        .acquired()
        .expect("checkpoint counter lease acquired");

    let empty = store
        .claim_checkpoint_work(
            session_id,
            &lease.fence(),
            &owner,
            &turn_id,
            crate::CheckpointKind::AfterWork,
            64,
            64,
        )
        .await
        .expect("probe quiescent checkpoint");
    assert!(empty.0.is_none() && empty.1.is_none());
    assert_eq!(counts(), (1, 0));

    let deferred = store
        .enqueue_queued_work(queued_process_wake_draft(
            session_id,
            "deferred checkpoint head",
            DeliveryPolicy::AfterCurrentTurnCommit,
            SlotPolicy::Exclusive,
        ))
        .await
        .expect("enqueue deferred checkpoint head");
    let deferred_checkpoint = store
        .claim_checkpoint_work(
            session_id,
            &lease.fence(),
            &owner,
            &turn_id,
            crate::CheckpointKind::AfterWork,
            64,
            64,
        )
        .await
        .expect("probe deferred checkpoint head");
    assert!(
        deferred_checkpoint.0.is_none() && deferred_checkpoint.1.is_none(),
        "after-current-turn-commit work must not claim at an active checkpoint"
    );
    assert_eq!(
        counts(),
        (2, 0),
        "a deferred queue head must not open a checkpoint write transaction"
    );

    let deferred_claim = store
        .claim_ready_queued_work(
            session_id,
            &lease.fence(),
            &owner,
            QueuedWorkClaimBoundary::Idle,
            64,
        )
        .await
        .expect("claim deferred work at idle boundary")
        .expect("deferred work remains claimable at idle boundary");
    assert_eq!(deferred_claim.batches[0].batch_id, deferred.batch_id);

    store
        .enqueue_pending_turn_input(crate::PendingTurnInputDraft::new(
            session_id,
            crate::TurnInputIngress::active_turn(
                turn_id.to_string(),
                crate::TurnInputCheckpointBoundary::AfterWork,
            ),
            crate::TurnInput::text("pending checkpoint input"),
        ))
        .await
        .expect("enqueue counter input");
    store
        .enqueue_queued_work(queued_process_wake_draft(
            session_id,
            "pending checkpoint work",
            DeliveryPolicy::EarliestSafeBoundary,
            SlotPolicy::Exclusive,
        ))
        .await
        .expect("enqueue counter work");
    let pending = store
        .claim_checkpoint_work(
            session_id,
            &lease.fence(),
            &owner,
            &turn_id,
            crate::CheckpointKind::AfterWork,
            64,
            64,
        )
        .await
        .expect("claim pending checkpoint work");
    assert!(pending.0.is_some() && pending.1.is_some());
    assert_eq!(counts(), (3, 1));
}

/// Build a queued process-wake draft for backend conformance tests.
pub fn queued_process_wake_draft(
    session_id: &str,
    text: &str,
    delivery_policy: DeliveryPolicy,
    slot_policy: SlotPolicy,
) -> QueuedWorkBatchDraft {
    let wake = ProcessWakeDelivery {
        wake_id: format!("wake:{session_id}:{text}"),
        target_session_id: session_id.to_string(),
        process_id: format!("process:{text}"),
        sequence: 1,
        event_type: "process.wake".to_string(),
        event_invocation: RuntimeInvocation {
            scope: RuntimeScope::new(session_id),
            subject: RuntimeSubject::ProcessEvent {
                process_id: format!("process:{text}"),
                sequence: 1,
                event_type: "process.wake".to_string(),
            },
            caused_by: None,
            replay: None,
        },
        process_caused_by: None,
        input: text.to_string(),
        created_at_ms: 1,
    };
    QueuedWorkBatchDraft::new(
        session_id,
        delivery_policy,
        slot_policy,
        vec![QueuedWorkPayload::process_wake(wake)],
    )
    .with_source_key(crate::process_wake_source_key(
        &format!("process:{text}"),
        1,
    ))
    .with_process_wake_source(format!("process:{text}"), 1)
}

fn queued_draft(
    session_id: &str,
    text: &str,
    delivery_policy: DeliveryPolicy,
    slot_policy: SlotPolicy,
) -> QueuedWorkBatchDraft {
    QueuedWorkBatchDraft::new(
        session_id,
        delivery_policy,
        slot_policy,
        vec![QueuedWorkPayload::agent_frame_task(
            format!("frame:{text}"),
            text,
            None,
        )],
    )
}

fn queued_session_command_draft(session_id: &str, reason: &str) -> QueuedWorkBatchDraft {
    QueuedWorkBatchDraft::new(
        session_id,
        DeliveryPolicy::EarliestSafeBoundary,
        SlotPolicy::Exclusive,
        vec![QueuedWorkPayload::session_command(
            crate::SessionCommand::RefreshToolCatalog {
                reason: reason.to_string(),
            },
        )],
    )
}

fn queued_batch_text(batch: &QueuedWorkBatch) -> Option<&str> {
    let payload = batch.items.first().map(|item| &item.payload)?;
    match payload {
        QueuedWorkPayload::ProcessWake { wake } => Some(wake.input.as_str()),
        QueuedWorkPayload::AgentFrameTask { task, .. } => Some(task.as_str()),
        QueuedWorkPayload::SessionCommand { .. } => None,
    }
}

fn pending_next_turn_input_draft(session_id: &str, text: &str) -> crate::PendingTurnInputDraft {
    crate::PendingTurnInputDraft::new(
        session_id,
        crate::TurnInputIngress::NextTurn,
        crate::TurnInput::text(text),
    )
}

fn inline_png(bytes: Vec<u8>) -> crate::AttachmentSource {
    crate::AttachmentSource::inline(crate::MediaType::parse("image/png").unwrap(), bytes)
}

fn pending_active_turn_input_draft(
    session_id: &str,
    turn_id: &str,
    min_boundary: crate::TurnInputCheckpointBoundary,
    text: &str,
) -> crate::PendingTurnInputDraft {
    crate::PendingTurnInputDraft::new(
        session_id,
        crate::TurnInputIngress::active_turn(turn_id, min_boundary),
        crate::TurnInput::text(text),
    )
}

fn pending_input_text(input: &crate::PendingTurnInput) -> Option<&str> {
    match input.input.items.first()? {
        crate::InputItem::Text { text } => Some(text.as_str()),
        crate::InputItem::Attachment { .. } => None,
    }
}

fn expect_cancelled_pending_input(
    outcome: crate::PendingTurnInputCancelOutcome,
    input_id: &str,
) -> crate::PendingTurnInput {
    match outcome {
        crate::PendingTurnInputCancelOutcome::Cancelled(input) => {
            assert_eq!(input.input_id, input_id);
            assert_eq!(input.state, crate::TurnInputState::Cancelled);
            input
        }
        other => panic!("expected cancelled pending turn input `{input_id}`, got {other:?}"),
    }
}

fn lease_owner(owner_id: &str) -> crate::LeaseOwnerIdentity {
    crate::LeaseOwnerIdentity::opaque(owner_id, format!("{owner_id}:incarnation"))
}

async fn claim_session_execution_lease_for_test(
    store: &Arc<dyn RuntimePersistence>,
    session_id: &str,
    owner_id: &str,
) -> crate::SessionExecutionLease {
    let owner = lease_owner(owner_id);
    store
        .try_claim_session_execution_lease(session_id, &owner, 60_000)
        .await
        .expect("claim session execution lease")
        .acquired()
        .expect("session execution lease is free")
}

async fn release_session_execution_lease_for_test(
    store: &Arc<dyn RuntimePersistence>,
    lease: &crate::SessionExecutionLease,
) {
    store
        .release_session_execution_lease(&lease.completion())
        .await
        .expect("release session execution lease");
}

async fn commit_runtime_state_for_test(
    store: &Arc<dyn RuntimePersistence>,
    commit: RuntimeCommit,
    owner_id: &str,
) -> Result<crate::store::RuntimeCommitResult, StoreError> {
    let session_id = commit.session_id.clone();
    let lease = claim_session_execution_lease_for_test(store, &session_id, owner_id).await;
    store
        .commit_runtime_state(commit.releasing_session_execution_lease(lease.completion()))
        .await
}

fn sample_session_node(session_id: &str, id: &str, parent: Option<&str>) -> SessionNodeRecord {
    let node_id = parent.map_or_else(|| crate::frame_node_id(session_id, id), |_| id.to_string());
    SessionNodeRecord {
        node_id,
        parent_node_id: parent.map(ToOwned::to_owned),
        timestamp: "1970-01-01T00:00:00Z".to_string(),
        payload: if parent.is_none() {
            SessionNodePayload::FrameOpen {
                frame_key: id.to_string(),
                reason: AgentFrameReason::initial(),
                assignment: crate::AgentFrameAssignment::from_policy(
                    crate::SessionPolicy::default(),
                ),
                protocol_turn_options: ProtocolTurnOptions::default(),
            }
        } else {
            SessionNodePayload::Event {
                event: crate::SessionHistoryRecord::Protocol(
                    ProtocolEvent::typed("conformance", serde_json::json!({ "node": id }))
                        .expect("protocol event"),
                ),
            }
        },
    }
}

fn attachment_intent(id: &str) -> AttachmentIntent {
    AttachmentIntent {
        attachment_id: AttachmentId::new(id.to_string()),
        session_id: "root".to_string(),
        canonical_uri: format!("sha256:{id}"),
        intent_at_epoch_ms: 100,
        owner_kind: None,
        owner_id: None,
    }
}

async fn commit_increments_head_and_round_trips_agent_frames(store: Arc<dyn RuntimePersistence>) {
    let mut state = RuntimeSessionState {
        session_id: "root".to_string(),
        policy: SessionPolicy {
            model: ModelSpec::from_token_limits("gpt-5.4-mini", Default::default(), 200_000, None)
                .expect("valid model spec"),
            ..SessionPolicy::default()
        },
        ..RuntimeSessionState::default()
    };
    state.ensure_agent_frame_initialized();
    let assignment = state
        .current_agent_frame()
        .expect("initial frame")
        .assignment
        .clone();
    let custom_reason = AgentFrameReason::new("plan_mode");
    let second_frame_node_id = crate::session_graph::frame_node_id(&state.session_id, "frame-2");
    assert!(state.session_graph.append_frame_open_with_id_at(
        second_frame_node_id.clone(),
        "frame-2".to_string(),
        custom_reason.clone(),
        assignment,
        ProtocolTurnOptions::default(),
        "2026-07-27T00:00:00Z".to_string(),
    ));
    state.current_frame_node_id = Some(second_frame_node_id.clone());
    state.agent_frames = state.session_graph.agent_frame_records("root");
    state.set_execution_state_snapshot(Some(b"frame-vm".to_vec()));

    commit_runtime_state_for_test(
        &store,
        RuntimeCommit::persisted_state_for_test(&state, &[]),
        "commit-round-trip",
    )
    .await
    .expect("commit runtime state");
    let read = store
        .load_session()
        .await
        .expect("load session")
        .expect("session read");

    assert_eq!(
        read.current_frame_node_id.as_deref(),
        Some(second_frame_node_id.as_str())
    );
    let frames = read.graph.agent_frame_records("root");
    assert_eq!(frames.len(), 2);
    let current = frames
        .iter()
        .find(|frame| frame.frame_node_id == second_frame_node_id)
        .expect("current frame");
    assert_eq!(current.reason, custom_reason);
    assert_eq!(
        read.checkpoint
            .as_ref()
            .and_then(|checkpoint| checkpoint.execution_state.as_deref()),
        Some(&b"frame-vm"[..])
    );
}

async fn concurrent_head_revision_cas_applies_exactly_once(store: Arc<dyn RuntimePersistence>) {
    let session_id = "concurrent-head-cas";
    let lease = claim_session_execution_lease_for_test(&store, session_id, "cas-owner").await;
    let make_commit = |node_id: &str| {
        let state = RuntimeSessionState {
            session_id: session_id.to_string(),
            ..RuntimeSessionState::default()
        };
        let node = sample_session_node(session_id, node_id, None);
        let derived_node_id = node.node_id.clone();
        let commit = RuntimeCommit {
            expected_head_revision: 0,
            current_frame_node_id: Some(derived_node_id.clone()),
            graph: crate::GraphAppend {
                nodes: vec![node],
                leaf_node_id: Some(derived_node_id),
            },
            ..RuntimeCommit::persisted_state_for_test(&state, &[])
        };
        commit
            .with_operation(crate::OperationId::new(
                crate::ExecutionScope::runtime_operation(format!("head-cas:{node_id}")),
                "commit",
            ))
            .expect("build distinct head-CAS operation")
            .0
    };

    let barrier = Arc::new(tokio::sync::Barrier::new(3));
    let left_store = Arc::clone(&store);
    let right_store = Arc::clone(&store);
    let left_barrier = Arc::clone(&barrier);
    let right_barrier = Arc::clone(&barrier);
    let left_commit = make_commit("cas-left");
    let right_commit = make_commit("cas-right");
    let left = crate::task::spawn(async move {
        left_barrier.wait().await;
        left_store.commit_runtime_state(left_commit).await
    });
    let right = crate::task::spawn(async move {
        right_barrier.wait().await;
        right_store.commit_runtime_state(right_commit).await
    });

    barrier.wait().await;
    let left = left.await.expect("join left head-CAS writer");
    let right = right.await.expect("join right head-CAS writer");
    let winners = [&left, &right]
        .into_iter()
        .filter(|result| result.is_ok())
        .count();
    let conflicts = [&left, &right]
        .into_iter()
        .filter(|result| matches!(result, Err(StoreError::HeadRevisionConflict { .. })))
        .count();
    assert_eq!(
        winners, 1,
        "exactly one concurrent writer must win head CAS, got left={left:?} right={right:?}"
    );
    assert_eq!(
        conflicts, 1,
        "the losing writer must receive HeadRevisionConflict, got left={left:?} right={right:?}"
    );

    let persisted = store
        .load_session()
        .await
        .expect("load state after concurrent head CAS")
        .expect("concurrent head-CAS winner persisted a session");
    assert_eq!(persisted.head_revision, 1, "exactly one commit applied");
    assert_eq!(persisted.graph.nodes.len(), 1, "exactly one graph applied");
    let left_node_id = crate::frame_node_id(session_id, "cas-left");
    let right_node_id = crate::frame_node_id(session_id, "cas-right");
    assert!(
        persisted.graph.nodes[0].node_id == left_node_id
            || persisted.graph.nodes[0].node_id == right_node_id,
        "the persisted graph must come from one of the two writers"
    );
    release_session_execution_lease_for_test(&store, &lease).await;
}

async fn commit_rejects_a_different_session_id(store: Arc<dyn RuntimePersistence>) {
    let alpha = RuntimeSessionState {
        session_id: "alpha".to_string(),
        ..RuntimeSessionState::default()
    };
    commit_runtime_state_for_test(
        &store,
        RuntimeCommit::persisted_state_for_test(&alpha, &[]),
        "bind-alpha",
    )
    .await
    .expect("first commit binds the session");
    let beta = RuntimeSessionState {
        session_id: "beta".to_string(),
        ..RuntimeSessionState::default()
    };
    let result = commit_runtime_state_for_test(
        &store,
        RuntimeCommit::persisted_state_for_test(&beta, &[]),
        "bind-beta",
    )
    .await;
    assert!(
        result.is_err(),
        "a single-session store must reject a commit for a different session id"
    );
}

async fn load_hydrates_checkpoint_and_usage(store: Arc<dyn RuntimePersistence>) {
    let state = RuntimeSessionState {
        session_id: "hydrated".to_string(),
        tool_state_snapshot: Some(ToolState::default().with_generation(9)),
        plugin_snapshot_revision: Some(12),
        plugin_snapshot: Some(PluginSessionSnapshot {
            plugins: Default::default(),
        }),
        ..RuntimeSessionState::default()
    };
    let usage = TokenLedgerEntry {
        source: "turn".to_string(),
        model: "mock-model".to_string(),
        usage: TokenUsage {
            input_tokens: 11,
            output_tokens: 7,
            cache_read_input_tokens: 3,
            cache_write_input_tokens: 0,
            reasoning_output_tokens: 5,
        },
    };

    commit_runtime_state_for_test(
        &store,
        RuntimeCommit::persisted_state_for_test(&state, &[usage]),
        "hydrate",
    )
    .await
    .expect("commit");

    let read = store.load_session().await.expect("load").expect("session");
    let checkpoint = read.checkpoint.expect("checkpoint");
    assert_eq!(read.session_id, "hydrated");
    assert_eq!(
        checkpoint
            .tool_state
            .expect("dynamic snapshot")
            .generation(),
        9
    );
    assert_eq!(checkpoint.plugin_snapshot_revision, Some(12));
    assert_eq!(read.token_ledger.len(), 1);
    assert_eq!(read.token_ledger[0].usage.input_tokens, 11);
}

async fn session_execution_lease_contract(store: Arc<dyn RuntimePersistence>) {
    let first = claim_session_execution_lease_for_test(&store, "root", "owner-a").await;
    let owner_a = lease_owner("owner-a");
    let owner_a_next = crate::LeaseOwnerIdentity::opaque("owner-a", "owner-a:next-incarnation");
    let owner_b = lease_owner("owner-b");
    let owner_c = lease_owner("owner-c");
    let owner_expired = lease_owner("owner-expired");
    let reentered = store
        .try_claim_session_execution_lease("root", &owner_a, 120_000)
        .await
        .expect("same incarnation may re-enter live session lease")
        .acquired()
        .expect("same incarnation receives existing session lease");
    assert_eq!(reentered.lease_token, first.lease_token);
    assert_eq!(reentered.fencing_token, first.fencing_token);
    assert!(reentered.expires_at_epoch_ms >= first.expires_at_epoch_ms);
    assert!(
        matches!(
            store
                .try_claim_session_execution_lease("root", &owner_a_next, 60_000)
                .await
                .expect("try same owner next incarnation"),
            crate::SessionExecutionLeaseClaimOutcome::Busy { .. }
        ),
        "a live session execution lease must exclude the same owner in a different incarnation"
    );
    assert!(
        matches!(
            store
                .try_claim_session_execution_lease("root", &owner_b, 60_000)
                .await
                .expect("try concurrent session lease"),
            crate::SessionExecutionLeaseClaimOutcome::Busy { .. }
        ),
        "a live session execution lease must exclude concurrent owners"
    );
    let renewed = store
        .renew_session_execution_lease(&reentered.fence(), 120_000)
        .await
        .expect("renew live session lease");
    assert_eq!(renewed.lease_token, first.lease_token);
    assert!(renewed.expires_at_epoch_ms >= reentered.expires_at_epoch_ms);

    let mut stale_fence = reentered.fence();
    stale_fence.lease_token.push_str(":stale");
    let err = store
        .renew_session_execution_lease(&stale_fence, 60_000)
        .await
        .expect_err("stale session lease renew must fail");
    assert!(matches!(
        err,
        StoreError::SessionExecutionLeaseExpired { .. }
    ));
    store
        .release_session_execution_lease(&crate::SessionExecutionLeaseAuthority {
            session_id: first.session_id.clone(),
            owner: first.owner.clone(),
            lease_token: format!("{}:stale", first.lease_token),
            fencing_token: first.fencing_token,
        })
        .await
        .expect("stale release is fenced and idempotent");
    assert!(
        matches!(
            store
                .try_claim_session_execution_lease("root", &owner_b, 60_000)
                .await
                .expect("try after stale release"),
            crate::SessionExecutionLeaseClaimOutcome::Busy { .. }
        ),
        "stale release must not clear the live lease"
    );
    // A completion identifies the lease *slot*, not one grant: the
    // same-incarnation re-entry above returned the identical owner, token and
    // fence, so releasing a completion retained from before that re-entry
    // clears the *successor's* live lease and no backend predicate can tell the
    // two apart. lash-core relies on this: a `SessionExecutionLeaseGuard` never
    // releases out of band (a dropped guard leaves the lease to TTL), because
    // only a guard that still tracks the lease knows the release is its own.
    // Rotating the lease token on every claim would make stale completions
    // distinguishable and is the change to make here if that behavior is ever
    // wanted — it must be a deliberate, suite-wide decision.
    //
    // Paired enforcement: this law pins the backend *fact* (a retained
    // completion frees the refreshed lease). The *prohibition* it implies —
    // that a `SessionExecutionLeaseGuard` must never release out of band — is
    // enforced by
    // `runtime::session_execution_lease::tests::guard_dropped_mid_release_never_releases_a_successors_lease`,
    // which fails if a dropped guard performs any release. Changing either side
    // requires revisiting the other.
    let retained_stale_completion = first.completion();
    store
        .release_session_execution_lease(&retained_stale_completion)
        .await
        .expect("release with a completion retained across a same-incarnation re-claim");
    let stolen = store
        .try_claim_session_execution_lease("root", &owner_b, 60_000)
        .await
        .expect("claim after releasing a retained completion")
        .acquired()
        .expect(
            "releasing a completion retained across a same-incarnation re-claim frees the \
             successor's live lease; owners must never release a completion they stopped tracking",
        );
    assert!(stolen.fencing_token > first.fencing_token);
    release_session_execution_lease_for_test(&store, &stolen).await;
    // Restore the pre-law state for the assertions below: `owner-a` holds a
    // live lease again, as it did before this block ran.
    let owner_a_relaid = claim_session_execution_lease_for_test(&store, "root", "owner-a").await;

    release_session_execution_lease_for_test(&store, &owner_a_relaid).await;
    let second = claim_session_execution_lease_for_test(&store, "root", "owner-b").await;
    assert!(
        second.fencing_token > first.fencing_token,
        "reclaimed session leases must advance the fencing token"
    );
    store
        .release_session_execution_lease(&first.completion())
        .await
        .expect("old release is idempotent");
    assert!(
        matches!(
            store
                .try_claim_session_execution_lease("root", &owner_c, 60_000)
                .await
                .expect("try after old release"),
            crate::SessionExecutionLeaseClaimOutcome::Busy { .. }
        ),
        "old release must not clear a newer lease"
    );
    release_session_execution_lease_for_test(&store, &second).await;

    let expired = store
        .try_claim_session_execution_lease("root", &owner_expired, 0)
        .await
        .expect("claim expiring lease")
        .acquired()
        .expect("expiring lease");
    let reclaimed = claim_session_execution_lease_for_test(&store, "root", "owner-reclaim").await;
    assert!(reclaimed.fencing_token > expired.fencing_token);
    release_session_execution_lease_for_test(&store, &reclaimed).await;

    let mut state = RuntimeSessionState {
        session_id: "root".to_string(),
        ..RuntimeSessionState::default()
    };
    let lease_free_commit = store
        .commit_runtime_state(RuntimeCommit::persisted_state_for_test(&state, &[]))
        .await
        .expect("head CAS, not the advisory lease, authorizes commit");
    state.head_revision = lease_free_commit.head_revision;

    let commit_lease = claim_session_execution_lease_for_test(&store, "root", "commit-owner").await;
    let lease_commit = store
        .commit_runtime_state(
            RuntimeCommit::persisted_state_for_test(&state, &[])
                .releasing_session_execution_lease(commit_lease.completion()),
        )
        .await
        .expect("advisory lease-bearing commit");
    state.head_revision = lease_commit.head_revision;
    let after_commit = claim_session_execution_lease_for_test(&store, "root", "after-commit").await;
    release_session_execution_lease_for_test(&store, &after_commit).await;

    let turn_state = RuntimeSessionState {
        session_id: "root".to_string(),
        turn_index: 1,
        head_revision: state.head_revision,
        ..RuntimeSessionState::default()
    };
    let turn_commit = RuntimeCommit::persisted_state_for_test(&turn_state, &[]);
    let mut turn_commit = turn_commit;
    turn_commit.turn_commit = RuntimeTurnCommitStamp::new(crate::OperationId::turn(
        "root",
        "lease-replay-turn",
        "final",
    ));
    let turn_lease = claim_session_execution_lease_for_test(&store, "root", "turn-owner").await;
    let first_result = store
        .commit_runtime_state(
            turn_commit
                .clone()
                .releasing_session_execution_lease(turn_lease.completion()),
        )
        .await
        .expect("first final commit under session lease");
    let replay = store
        .commit_runtime_state(turn_commit)
        .await
        .expect("idempotent replay returns without live session lease");
    assert_eq!(replay.head_revision, first_result.head_revision);

    let batch = store
        .enqueue_queued_work(queued_draft(
            "root",
            "fenced queue",
            DeliveryPolicy::EarliestSafeBoundary,
            SlotPolicy::Exclusive,
        ))
        .await
        .expect("enqueue fenced queue work");
    let err = store
        .claim_ready_queued_work(
            "root",
            &commit_lease.fence(),
            &lease_owner("queue-owner"),
            QueuedWorkClaimBoundary::Idle,
            1,
        )
        .await
        .expect_err("queued-work claims require a live session lease");
    assert!(matches!(
        err,
        StoreError::SessionExecutionLeaseExpired { .. }
    ));
    let queue_lease = claim_session_execution_lease_for_test(&store, "root", "queue-owner").await;
    let claim = store
        .claim_ready_queued_work(
            "root",
            &queue_lease.fence(),
            &lease_owner("queue-owner"),
            QueuedWorkClaimBoundary::Idle,
            1,
        )
        .await
        .expect("claim fenced queue work")
        .expect("queue work claim");
    assert_eq!(claim.batches[0].batch_id, batch.batch_id);
    release_session_execution_lease_for_test(&store, &queue_lease).await;
}

async fn session_execution_lease_expires_by_ttl_contract<F>(
    make: &F,
    lease_timing: &RuntimePersistenceLeaseTiming,
) where
    F: Fn() -> Arc<dyn RuntimePersistence>,
{
    // Realtime deliberately cannot pin the exact `>` versus `>=` database-millisecond edge;
    // the Controlled vectors own that boundary.
    // Its verdict trusts backend-reported claim and expiry timestamps, gated by the Postgres
    // clock-contract vector and the injected-clock vectors for embedded stores.
    for attempt in 0..REALTIME_LEASE_OBSERVATION_ATTEMPTS {
        let store = make();
        let session_id = format!("ttl-expiry-{attempt}");
        let holder_owner = lease_owner("stale-holder");
        let claimant = lease_owner("ttl-claimant");
        let holder = store
            .try_claim_session_execution_lease(&session_id, &holder_owner, CONTROLLED_LEASE_TTL_MS)
            .await
            .expect("claim stale-holder lease")
            .acquired()
            .expect("stale-holder lease acquired");

        lease_timing.advance_to_just_before_semantic_expiry();
        let outcome = store
            .try_claim_session_execution_lease(&session_id, &claimant, 60_000)
            .await
            .expect("claimant observes stale-holder lease");
        match outcome {
            crate::SessionExecutionLeaseClaimOutcome::Busy {
                holder: busy_holder,
            } => {
                assert_eq!(
                    busy_holder.lease_token, holder.lease_token,
                    "the busy observation must name the stale-holder lease"
                );
            }
            crate::SessionExecutionLeaseClaimOutcome::Acquired(acquired)
                if acquired.lease.claimed_at_epoch_ms < holder.expires_at_epoch_ms =>
            {
                panic!(
                    "an unexpired stale lease must remain busy rather than being reclaimed: \
                     successor claimed at {} before holder expiry {}",
                    acquired.lease.claimed_at_epoch_ms, holder.expires_at_epoch_ms
                );
            }
            crate::SessionExecutionLeaseClaimOutcome::Acquired(lapsed_successor) => {
                release_session_execution_lease_for_test(&store, &lapsed_successor.lease).await;
                continue;
            }
        }

        lease_timing.advance_to_semantic_expiry();
        let acquired = claim_session_execution_lease_until_acquired(
            &store,
            &session_id,
            &claimant,
            lease_timing,
            "stale-holder TTL",
        )
        .await;
        assert!(
            acquired.fencing_token > holder.fencing_token,
            "TTL takeover must advance the fencing token"
        );
        release_session_execution_lease_for_test(&store, &acquired).await;
        return;
    }
    panic!(
        "could not observe the stale-holder lease within its {} ms semantic TTL after {} attempts",
        CONTROLLED_LEASE_TTL_MS, REALTIME_LEASE_OBSERVATION_ATTEMPTS
    );
}

async fn claim_session_execution_lease_after_expiry(
    store: &Arc<dyn RuntimePersistence>,
    session_id: &str,
    claimant: &crate::LeaseOwnerIdentity,
    lease_timing: &RuntimePersistenceLeaseTiming,
    context: &str,
) -> crate::SessionExecutionLease {
    lease_timing.wait_until_expired().await;
    claim_session_execution_lease_until_acquired(store, session_id, claimant, lease_timing, context)
        .await
}

async fn claim_session_execution_lease_until_acquired(
    store: &Arc<dyn RuntimePersistence>,
    session_id: &str,
    claimant: &crate::LeaseOwnerIdentity,
    lease_timing: &RuntimePersistenceLeaseTiming,
    context: &str,
) -> crate::SessionExecutionLease {
    let deadline = std::time::Instant::now() + REALTIME_LEASE_STALL_ALLOWANCE;
    loop {
        match store
            .try_claim_session_execution_lease(session_id, claimant, 60_000)
            .await
            .unwrap_or_else(|error| panic!("claim after {context}: {error}"))
        {
            crate::SessionExecutionLeaseClaimOutcome::Acquired(acquisition) => {
                return acquisition.lease;
            }
            crate::SessionExecutionLeaseClaimOutcome::Busy { holder: _ }
                if matches!(lease_timing, RuntimePersistenceLeaseTiming::Realtime)
                    && std::time::Instant::now() < deadline =>
            {
                tokio::time::sleep(REALTIME_LEASE_EXPIRY_POLL).await;
            }
            crate::SessionExecutionLeaseClaimOutcome::Busy { holder } => {
                panic!("lease remained busy after {context}: {holder:?}")
            }
        }
    }
}

/// The diagnostic read reports the durable lease row as a raw fact and never
/// mutates it: unknown sessions and released rows read as absent, a held row
/// reports the exact holder facts, a lapsed row is still reported (expiry is not
/// filtered), and a takeover is visible as a strictly higher generation under a
/// different holder.
async fn session_execution_lease_diagnostic_read_contract(store: Arc<dyn RuntimePersistence>) {
    assert!(
        store
            .get_session_execution_lease("lease-diagnostics-unknown")
            .await
            .expect("diagnostic read of an unknown session succeeds")
            .is_none(),
        "an unknown session id must read as no lease rather than erroring"
    );

    let held = claim_session_execution_lease_for_test(&store, "lease-diagnostics", "diag-a").await;
    let observed = store
        .get_session_execution_lease("lease-diagnostics")
        .await
        .expect("diagnostic read of a held lease")
        .expect("a held lease must be reported");
    assert_eq!(observed.session_id, held.session_id);
    assert_eq!(observed.owner, held.owner);
    assert_eq!(observed.fencing_token, held.fencing_token);
    assert_eq!(observed.lease_token, held.lease_token);
    assert_eq!(observed.claimed_at_epoch_ms, held.claimed_at_epoch_ms);
    assert_eq!(observed.expires_at_epoch_ms, held.expires_at_epoch_ms);

    // Reading must not renew, expire, or re-fence anything: the holder's own
    // renewal still succeeds against the fence it presented before the read.
    store
        .renew_session_execution_lease(&held.fence(), 120_000)
        .await
        .expect("a diagnostic read must not invalidate the holder's fence");

    release_session_execution_lease_for_test(&store, &held).await;
    assert!(
        store
            .get_session_execution_lease("lease-diagnostics")
            .await
            .expect("diagnostic read after release")
            .is_none(),
        "a released row must read as no holder even though its generation persists"
    );

    // A lapsed holder is the ambiguous case triage must see, so expiry is
    // reported rather than filtered out.
    let lapsing = store
        .try_claim_session_execution_lease("lease-diagnostics", &lease_owner("diag-lapsed"), 0)
        .await
        .expect("claim an immediately expiring lease")
        .acquired()
        .expect("expiring lease acquired");
    let lapsed = store
        .get_session_execution_lease("lease-diagnostics")
        .await
        .expect("diagnostic read of a lapsed lease")
        .expect("a lapsed holder must still be reported");
    assert_eq!(lapsed.owner, lapsing.owner);
    assert_eq!(lapsed.fencing_token, lapsing.fencing_token);

    let successor =
        claim_session_execution_lease_for_test(&store, "lease-diagnostics", "diag-b").await;
    let after_takeover = store
        .get_session_execution_lease("lease-diagnostics")
        .await
        .expect("diagnostic read after takeover")
        .expect("the successor holds the row");
    assert_eq!(after_takeover.owner, successor.owner);
    assert!(
        after_takeover.fencing_token > lapsing.fencing_token,
        "takeover must be visible read-side as a strictly higher generation"
    );
    release_session_execution_lease_for_test(&store, &successor).await;
}

/// A granted claim must name the lapsed holder it displaced, read inside the same
/// atomic operation.
///
/// This is the only truthful report of a takeover. The displaced runner is
/// usually *why* the lease lapsed, so it is frequently dead, frozen, or already
/// replaced; a takeover inferred from its own renewal-failure path is missing in
/// exactly that case, and can name whichever holder happens to be current by the
/// time it wakes rather than the one that displaced it.
///
/// The same vector carries the generation law the displacement is measured
/// against: [`crate::store::SessionExecutionLeaseStore`] is a fencing trait, and
/// ADR 0029 requires every fresh acquisition after release or TTL expiry to mint
/// `previous + 1`. Both halves are checked against the generations this run
/// actually observed, never against a constant, so a store frozen at one
/// generation cannot pass.
///
/// Every implementation answers this, in-process doubles included. A double that
/// reports no displacement silently disables the takeover event for whatever it
/// stands in for, and one that restarts the fence after release reissues a
/// generation that stale claims still pin, which stops fencing working at all.
/// Callers pass a session id they own, because a claim mutates the lane.
pub async fn session_execution_lease_displacement(
    store: &(dyn crate::store::SessionExecutionLeaseStore + '_),
    session_id: &str,
) {
    let first = lease_owner("displacement-first");
    let second = lease_owner("displacement-second");

    // A first claim on a row nobody ever held displaces nobody.
    let opening = store
        .try_claim_session_execution_lease(session_id, &first, 0)
        .await
        .expect("first claim")
        .acquisition()
        .expect("an unheld lane is acquirable");
    assert!(
        opening.displaced.is_none(),
        "a first claim must not report displacing anyone: {:?}",
        opening.displaced
    );

    // Taking over a lapsed holder must name that exact holder and generation.
    let takeover = store
        .try_claim_session_execution_lease(session_id, &second, 60_000)
        .await
        .expect("claim the lapsed lane")
        .acquisition()
        .expect("a lapsed lane is claimable");
    let displaced = takeover.displaced.as_ref().unwrap_or_else(|| {
        panic!(
            "displacing a lapsed holder must be reported on the claim; \
             this store reported nothing, which disables the takeover event"
        )
    });
    assert_eq!(
        displaced.owner, opening.lease.owner,
        "the displacement must name the holder actually displaced"
    );
    assert_eq!(
        displaced.fencing_token, opening.lease.fencing_token,
        "the displacement must name the generation actually displaced"
    );
    assert_eq!(
        takeover.lease.fencing_token,
        opening.lease.fencing_token + 1,
        "a claim over an expired lease must mint exactly the previous generation plus one \
         (ADR 0029): displaced {}, acquired {}",
        opening.lease.fencing_token,
        takeover.lease.fencing_token
    );
    assert_eq!(
        displaced.expired_at_epoch_ms, opening.lease.expires_at_epoch_ms,
        "the displacement must report the lapsed holder's own expiry"
    );

    // Same-incarnation reentry advances nothing, so it displaces nobody.
    let reentry = store
        .try_claim_session_execution_lease(session_id, &second, 60_000)
        .await
        .expect("reenter the live lane")
        .acquisition()
        .expect("the same incarnation reenters its own lease");
    assert_eq!(reentry.lease.fencing_token, takeover.lease.fencing_token);
    assert!(
        reentry.displaced.is_none(),
        "reentry must not report a displacement: {:?}",
        reentry.displaced
    );

    // A holder that released its lane hands it over; the next claimant took
    // nothing from anyone and must not report a takeover.
    store
        .release_session_execution_lease(&reentry.lease.completion())
        .await
        .expect("release the lane");
    let after_release = store
        .try_claim_session_execution_lease(session_id, &first, 60_000)
        .await
        .expect("claim a released lane")
        .acquisition()
        .expect("a released lane is acquirable");
    assert!(
        after_release.displaced.is_none(),
        "claiming a cleanly released lane displaces nobody: {:?}",
        after_release.displaced
    );
    assert_eq!(
        after_release.lease.fencing_token,
        reentry.lease.fencing_token + 1,
        "a claim after a release must mint exactly the released generation plus one \
         (ADR 0029): released {}, acquired {}. Restarting or repeating the generation here \
         reissues one that stale claims still pin, so fencing stops working",
        reentry.lease.fencing_token,
        after_release.lease.fencing_token
    );
    store
        .release_session_execution_lease(&after_release.lease.completion())
        .await
        .expect("release the reclaimed lane");
}

/// The durable-backend entry point for the shared lease-acquisition contract.
///
/// The contract itself lives in [`session_execution_lease_displacement`] because
/// it binds every implementation of the fencing trait, doubles included. There is
/// nothing extra a durable backend owes here: the displacement report and the
/// `previous + 1` generation law are both trait-level obligations.
async fn session_execution_lease_displacement_contract(store: Arc<dyn RuntimePersistence>) {
    session_execution_lease_displacement(store.as_ref(), "lease-displacement").await;
}

async fn session_read_loads_persisted_history(store: Arc<dyn RuntimePersistence>) {
    let root = sample_session_node("branchy", "root-node", None);
    let root_node_id = root.node_id.clone();
    let graph = crate::SessionGraph::from_nodes(
        vec![
            root,
            sample_session_node("branchy", "left-node", Some(&root_node_id)),
            sample_session_node("branchy", "left-leaf", Some("left-node")),
        ],
        Some("left-leaf".to_string()),
    );
    let state = RuntimeSessionState {
        session_id: "branchy".to_string(),
        current_frame_node_id: Some(root_node_id.clone()),
        session_graph: graph,
        ..RuntimeSessionState::default()
    };
    let commit = RuntimeCommit::persisted_state_for_test(&state, &[]);
    let expected_node_ids = commit
        .graph
        .nodes
        .iter()
        .map(|node| node.node_id.clone())
        .collect::<Vec<_>>();
    let expected_leaf_node_id = commit.graph.leaf_node_id.clone();
    commit_runtime_state_for_test(&store, commit, "active-path")
        .await
        .expect("commit linear graph");

    let read = store
        .load_session()
        .await
        .expect("load session history")
        .expect("session history exists");
    assert_eq!(
        read.graph
            .nodes
            .iter()
            .map(|node| node.node_id.as_str())
            .collect::<Vec<_>>(),
        expected_node_ids
            .iter()
            .map(String::as_str)
            .collect::<Vec<_>>(),
        "session reads must return the persisted leaf-to-root history"
    );
    assert_eq!(read.graph.leaf_node_id, expected_leaf_node_id);
}

async fn attachment_manifest_records_intent_and_commit_stamps(store: Arc<dyn RuntimePersistence>) {
    let committed_by_runtime = AttachmentId::new("runtime-commit".to_string());
    let committed_out_of_band = AttachmentId::new("manual-commit".to_string());
    let orphan = AttachmentId::new("orphan".to_string());
    for id in [&committed_by_runtime, &committed_out_of_band, &orphan] {
        store
            .record_intent(attachment_intent(id.as_str()))
            .expect("record attachment intent");
    }

    let mut uncommitted = store
        .list_uncommitted(200)
        .expect("list uncommitted attachment intents");
    uncommitted.sort_by(|left, right| left.attachment_id.cmp(&right.attachment_id));
    assert_eq!(uncommitted.len(), 3);

    store
        .commit_refs("root", std::slice::from_ref(&committed_out_of_band))
        .expect("commit attachment ref out of band");
    let state = RuntimeSessionState {
        session_id: "root".to_string(),
        ..RuntimeSessionState::default()
    };
    commit_runtime_state_for_test(
        &store,
        RuntimeCommit::persisted_state_for_test(&state, &[])
            .with_committed_attachments([committed_by_runtime.clone()]),
        "attachment-manifest",
    )
    .await
    .expect("runtime commit stamps attachment manifest");

    let still_uncommitted = store
        .list_uncommitted(200)
        .expect("list remaining uncommitted attachments");
    assert_eq!(still_uncommitted.len(), 1);
    assert_eq!(still_uncommitted[0].attachment_id, orphan);
    assert!(still_uncommitted[0].committed_at_epoch_ms.is_none());

    store
        .forget("root", &orphan)
        .expect("forget orphan attachment");
    assert!(
        store
            .list_uncommitted(200)
            .expect("list after forget")
            .is_empty()
    );
}

async fn attachment_manifest_keeps_same_content_ownership_per_session(
    store: Arc<dyn RuntimePersistence>,
) {
    let attachment = AttachmentId::new("same-content");
    for session_id in ["committed-owner", "orphan-owner"] {
        store
            .record_intent(AttachmentIntent {
                attachment_id: attachment.clone(),
                session_id: session_id.to_string(),
                canonical_uri: format!("session:{session_id}:sha256:{attachment}"),
                intent_at_epoch_ms: 100,
                owner_kind: None,
                owner_id: None,
            })
            .expect("record independent owner intent");
    }
    store
        .commit_refs("committed-owner", std::slice::from_ref(&attachment))
        .expect("commit first owner");

    let uncommitted = store.list_uncommitted(200).expect("list owner orphan");
    assert!(
        uncommitted.iter().any(|entry| {
            entry.session_id == "orphan-owner" && entry.attachment_id == attachment
        })
    );
    assert!(!uncommitted.iter().any(|entry| {
        entry.session_id == "committed-owner" && entry.attachment_id == attachment
    }));

    store
        .forget("orphan-owner", &attachment)
        .expect("forget only orphan owner");
    store
        .record_intent(AttachmentIntent {
            attachment_id: attachment.clone(),
            session_id: "committed-owner".to_string(),
            canonical_uri: format!("session:committed-owner:sha256:{attachment}"),
            intent_at_epoch_ms: 150,
            owner_kind: None,
            owner_id: None,
        })
        .expect("repeat committed owner intent");
    assert!(
        !store
            .list_uncommitted(200)
            .expect("committed ownership remains stamped")
            .iter()
            .any(|entry| entry.session_id == "committed-owner"
                && entry.attachment_id == attachment),
        "a colliding owner or repeated put must not erase another session's commit stamp"
    );
}

async fn queued_work_source_keys_are_idempotent_and_list_ordered(
    store: Arc<dyn RuntimePersistence>,
) {
    let first = store
        .enqueue_queued_work(
            queued_draft(
                "root",
                "first",
                DeliveryPolicy::EarliestSafeBoundary,
                SlotPolicy::Join,
            )
            .with_source_key("source:first"),
        )
        .await
        .expect("enqueue first batch");
    let replay = store
        .enqueue_queued_work(
            queued_draft(
                "root",
                "different replay payload",
                DeliveryPolicy::EarliestSafeBoundary,
                SlotPolicy::Join,
            )
            .with_source_key("source:first"),
        )
        .await
        .expect("replay first batch");
    let second = store
        .enqueue_queued_work(queued_draft(
            "root",
            "second",
            DeliveryPolicy::EarliestSafeBoundary,
            SlotPolicy::Exclusive,
        ))
        .await
        .expect("enqueue second batch");
    store
        .enqueue_queued_work(queued_draft(
            "other",
            "other session",
            DeliveryPolicy::EarliestSafeBoundary,
            SlotPolicy::Exclusive,
        ))
        .await
        .expect("enqueue other session");

    assert_eq!(
        first.batch_id, replay.batch_id,
        "replaying a source key must return the original batch"
    );
    assert_eq!(first.items[0].item_id, replay.items[0].item_id);
    assert_eq!(
        queued_batch_text(&replay),
        Some("first"),
        "source-key replay must return the original stored payload, not the replay attempt"
    );
    let listed = store
        .list_queued_work("root")
        .await
        .expect("list queued work");
    assert_eq!(
        listed
            .iter()
            .map(|batch| batch.batch_id.as_str())
            .collect::<Vec<_>>(),
        vec![first.batch_id.as_str(), second.batch_id.as_str()]
    );
    assert!(listed[0].enqueue_seq < listed[1].enqueue_seq);
}

async fn concurrent_queue_and_turn_input_claims_have_one_owner(store: Arc<dyn RuntimePersistence>) {
    let session_id = "concurrent-claim-races";
    let batch = store
        .enqueue_queued_work(queued_draft(
            session_id,
            "single-owner queue batch",
            DeliveryPolicy::EarliestSafeBoundary,
            SlotPolicy::Exclusive,
        ))
        .await
        .expect("enqueue queue batch for claim race");
    let input = store
        .enqueue_pending_turn_input(pending_next_turn_input_draft(
            session_id,
            "single-owner turn input",
        ))
        .await
        .expect("enqueue turn input for claim race");
    let lease =
        claim_session_execution_lease_for_test(&store, session_id, "claim-race-lease").await;

    let queue_barrier = Arc::new(tokio::sync::Barrier::new(3));
    let left_store = Arc::clone(&store);
    let right_store = Arc::clone(&store);
    let left_barrier = Arc::clone(&queue_barrier);
    let right_barrier = Arc::clone(&queue_barrier);
    let left_fence = lease.fence();
    let right_fence = lease.fence();
    let left_queue = crate::task::spawn(async move {
        left_barrier.wait().await;
        left_store
            .claim_ready_queued_work(
                session_id,
                &left_fence,
                &lease_owner("queue-left"),
                QueuedWorkClaimBoundary::Idle,
                1,
            )
            .await
    });
    let right_queue = crate::task::spawn(async move {
        right_barrier.wait().await;
        right_store
            .claim_ready_queued_work(
                session_id,
                &right_fence,
                &lease_owner("queue-right"),
                QueuedWorkClaimBoundary::Idle,
                1,
            )
            .await
    });
    queue_barrier.wait().await;
    let left_queue = left_queue
        .await
        .expect("join left queue claimant")
        .expect("left queue claim race resolves cleanly");
    let right_queue = right_queue
        .await
        .expect("join right queue claimant")
        .expect("right queue claim race resolves cleanly");
    let queue_winners = [left_queue.as_ref(), right_queue.as_ref()]
        .into_iter()
        .flatten()
        .collect::<Vec<_>>();
    assert_eq!(
        queue_winners.len(),
        1,
        "exactly one owner may claim the same queue batch"
    );
    assert_eq!(queue_winners[0].batches[0].batch_id, batch.batch_id);
    assert!(
        store
            .list_pending_queued_work(session_id)
            .await
            .expect("list queue after claim race")
            .is_empty(),
        "the winning queue claim must exclusively hide its batch"
    );

    let input_barrier = Arc::new(tokio::sync::Barrier::new(3));
    let left_store = Arc::clone(&store);
    let right_store = Arc::clone(&store);
    let left_barrier = Arc::clone(&input_barrier);
    let right_barrier = Arc::clone(&input_barrier);
    let left_fence = lease.fence();
    let right_fence = lease.fence();
    let left_input = crate::task::spawn(async move {
        left_barrier.wait().await;
        left_store
            .claim_next_turn_inputs(session_id, &left_fence, &lease_owner("input-left"), 1)
            .await
    });
    let right_input = crate::task::spawn(async move {
        right_barrier.wait().await;
        right_store
            .claim_next_turn_inputs(session_id, &right_fence, &lease_owner("input-right"), 1)
            .await
    });
    input_barrier.wait().await;
    let left_input = left_input
        .await
        .expect("join left turn-input claimant")
        .expect("left turn-input claim race resolves cleanly");
    let right_input = right_input
        .await
        .expect("join right turn-input claimant")
        .expect("right turn-input claim race resolves cleanly");
    let input_winners = [left_input.as_ref(), right_input.as_ref()]
        .into_iter()
        .flatten()
        .collect::<Vec<_>>();
    assert_eq!(
        input_winners.len(),
        1,
        "exactly one owner may claim the same turn input"
    );
    assert_eq!(input_winners[0].inputs[0].input_id, input.input_id);
    assert_ne!(
        queue_winners[0].owner.owner_id, input_winners[0].owner.owner_id,
        "the queue and turn-input races use independent logical owners"
    );

    release_session_execution_lease_for_test(&store, &lease).await;
}

async fn queued_work_cancel_removes_only_unclaimed_batches(store: Arc<dyn RuntimePersistence>) {
    let cancellable = store
        .enqueue_queued_work(queued_draft(
            "root",
            "cancel me",
            DeliveryPolicy::AfterCurrentTurnCommit,
            SlotPolicy::Exclusive,
        ))
        .await
        .expect("enqueue cancellable batch");
    let cancelled = store
        .cancel_queued_work_batch("root", &cancellable.batch_id)
        .await
        .expect("cancel unclaimed batch")
        .expect("unclaimed batch is returned");
    assert_eq!(cancelled.batch_id, cancellable.batch_id);
    assert_eq!(queued_batch_text(&cancelled), Some("cancel me"));
    assert!(
        store
            .list_queued_work("root")
            .await
            .expect("list after cancellation")
            .is_empty(),
        "cancelled batches must be removed from the durable queue"
    );

    let claimed = store
        .enqueue_queued_work(queued_draft(
            "root",
            "claimed",
            DeliveryPolicy::AfterCurrentTurnCommit,
            SlotPolicy::Exclusive,
        ))
        .await
        .expect("enqueue claimed batch");
    let session_lease = claim_session_execution_lease_for_test(&store, "root", "owner").await;
    let claim = store
        .claim_ready_queued_work(
            "root",
            &session_lease.fence(),
            &lease_owner("owner"),
            QueuedWorkClaimBoundary::Idle,
            1,
        )
        .await
        .expect("claim batch")
        .expect("claim exists");
    assert_eq!(claim.batches[0].batch_id, claimed.batch_id);
    // The session lease stays live here: a claim is live for lease-less host
    // callers exactly while the generation it pins still holds the session lease
    // (ADR 0029), so the hiding/cancel guards below must observe a live lease.
    assert!(
        store
            .list_pending_queued_work("root")
            .await
            .expect("list pending during active claim")
            .is_empty(),
        "active claims must disappear from user-editable queue snapshots"
    );
    assert_eq!(
        store
            .list_queued_work("root")
            .await
            .expect("raw durable list during active claim")
            .len(),
        1,
        "claimed batches remain durable until their claim is completed"
    );
    assert!(
        store
            .cancel_queued_work_batch("root", &claimed.batch_id)
            .await
            .expect("cancel active claim")
            .is_none(),
        "actively claimed batches must not be cancelled"
    );
    store
        .abandon_queued_work_claim(&claim)
        .await
        .expect("abandon claim");
    release_session_execution_lease_for_test(&store, &session_lease).await;
    assert_eq!(
        store
            .list_pending_queued_work("root")
            .await
            .expect("list pending after abandoned claim")
            .len(),
        1,
        "abandoned claims become user-editable queue work again"
    );
    assert!(
        store
            .cancel_queued_work_batch("root", &claimed.batch_id)
            .await
            .expect("cancel abandoned claim")
            .is_some(),
        "abandoned batches become cancellable again"
    );
}

async fn queued_work_exact_claim_uses_selected_batch_ids(store: Arc<dyn RuntimePersistence>) {
    let first = store
        .enqueue_queued_work(queued_draft(
            "root",
            "first",
            DeliveryPolicy::AfterCurrentTurnCommit,
            SlotPolicy::Exclusive,
        ))
        .await
        .expect("enqueue first batch");
    let second = store
        .enqueue_queued_work(queued_draft(
            "root",
            "second",
            DeliveryPolicy::AfterCurrentTurnCommit,
            SlotPolicy::Exclusive,
        ))
        .await
        .expect("enqueue second batch");

    let selected_session_lease =
        claim_session_execution_lease_for_test(&store, "root", "owner").await;
    assert!(
        store
            .claim_ready_queued_work_by_batch_ids(
                "root",
                &selected_session_lease.fence(),
                &lease_owner("owner"),
                QueuedWorkClaimBoundary::ActiveTurnCheckpoint,
                std::slice::from_ref(&second.batch_id),
            )
            .await
            .expect("boundary-gated exact claim")
            .is_none(),
        "exact selection must preserve the delivery boundary gate"
    );
    assert!(
        store
            .claim_ready_queued_work_by_batch_ids(
                "root",
                &selected_session_lease.fence(),
                &lease_owner("owner"),
                QueuedWorkClaimBoundary::Idle,
                &[first.batch_id.clone(), second.batch_id.clone()],
            )
            .await
            .expect("slot-policy-gated exact claim")
            .is_none(),
        "exact selection must not combine exclusive batches into one claim"
    );
    let selected = store
        .claim_ready_queued_work_by_batch_ids(
            "root",
            &selected_session_lease.fence(),
            &lease_owner("owner"),
            QueuedWorkClaimBoundary::Idle,
            std::slice::from_ref(&second.batch_id),
        )
        .await
        .expect("claim out-of-order exact batch")
        .expect("selected exact batch exists");
    assert_eq!(selected.batches[0].batch_id, second.batch_id);
    assert_eq!(
        store
            .list_pending_queued_work("root")
            .await
            .expect("list after out-of-order exact claim")
            .iter()
            .map(|batch| batch.batch_id.as_str())
            .collect::<Vec<_>>(),
        vec![first.batch_id.as_str()]
    );
    let state = RuntimeSessionState {
        session_id: "root".to_string(),
        ..RuntimeSessionState::default()
    };
    store
        .commit_runtime_state(
            RuntimeCommit::persisted_state_for_test(&state, &[])
                .releasing_session_execution_lease(selected_session_lease.completion())
                .completing_queue_claim(selected.completion()),
        )
        .await
        .expect("complete out-of-order exact batch");

    let accepted_session_lease =
        claim_session_execution_lease_for_test(&store, "root", "owner").await;
    let claim = store
        .claim_ready_queued_work_by_batch_ids(
            "root",
            &accepted_session_lease.fence(),
            &lease_owner("owner"),
            QueuedWorkClaimBoundary::Idle,
            std::slice::from_ref(&first.batch_id),
        )
        .await
        .expect("claim first exact batch")
        .expect("first exact claim exists");
    assert_eq!(
        claim
            .batches
            .iter()
            .map(|batch| batch.batch_id.as_str())
            .collect::<Vec<_>>(),
        vec![first.batch_id.as_str()]
    );
    // Both selected batches are hidden now: `second` was atomically completed
    // above, while `first` remains held by this live claim.
    assert!(
        store
            .list_pending_queued_work("root")
            .await
            .expect("list pending after exact claim")
            .is_empty()
    );
    release_session_execution_lease_for_test(&store, &accepted_session_lease).await;
}

async fn queued_work_classes_gate_command_and_turn_claims(store: Arc<dyn RuntimePersistence>) {
    let command = store
        .enqueue_queued_work(queued_session_command_draft("root", "refresh before turn"))
        .await
        .expect("enqueue command");
    let turn = store
        .enqueue_queued_work(queued_draft(
            "root",
            "user turn",
            DeliveryPolicy::AfterCurrentTurnCommit,
            SlotPolicy::Exclusive,
        ))
        .await
        .expect("enqueue turn");

    let rejected_turn_lease =
        claim_session_execution_lease_for_test(&store, "root", "turn-owner").await;
    assert!(
        store
            .claim_ready_queued_work(
                "root",
                &rejected_turn_lease.fence(),
                &lease_owner("turn-owner"),
                QueuedWorkClaimBoundary::Idle,
                10,
            )
            .await
            .expect("turn claim with leading command")
            .is_none(),
        "turn claims must not skip a leading session command"
    );
    release_session_execution_lease_for_test(&store, &rejected_turn_lease).await;

    let command_lease =
        claim_session_execution_lease_for_test(&store, "root", "command-owner").await;
    let command_claim = store
        .claim_leading_ready_session_command(
            "root",
            &command_lease.fence(),
            &lease_owner("command-owner"),
        )
        .await
        .expect("claim leading command")
        .expect("leading command claim exists");
    assert_eq!(
        command_claim
            .batches
            .iter()
            .map(|batch| batch.batch_id.as_str())
            .collect::<Vec<_>>(),
        vec![command.batch_id.as_str()]
    );
    let state = RuntimeSessionState {
        session_id: "root".to_string(),
        ..RuntimeSessionState::default()
    };
    store
        .commit_runtime_state(
            RuntimeCommit::persisted_state_for_test(&state, &[])
                .releasing_session_execution_lease(command_lease.completion())
                .completing_queue_claim(command_claim.completion()),
        )
        .await
        .expect("complete command claim");

    let selected_turn_lease =
        claim_session_execution_lease_for_test(&store, "root", "turn-owner").await;
    let selected_turn = store
        .claim_ready_queued_work_by_batch_ids(
            "root",
            &selected_turn_lease.fence(),
            &lease_owner("turn-owner"),
            QueuedWorkClaimBoundary::Idle,
            std::slice::from_ref(&turn.batch_id),
        )
        .await
        .expect("claim selected turn after command")
        .expect("selected turn claim exists");
    release_session_execution_lease_for_test(&store, &selected_turn_lease).await;
    assert_eq!(selected_turn.batches[0].batch_id, turn.batch_id);

    let first_turn = store
        .enqueue_queued_work(queued_draft(
            "turn-first",
            "first turn",
            DeliveryPolicy::AfterCurrentTurnCommit,
            SlotPolicy::Exclusive,
        ))
        .await
        .expect("enqueue first turn");
    let second_command = store
        .enqueue_queued_work(queued_session_command_draft("turn-first", "later refresh"))
        .await
        .expect("enqueue later command");
    let rejected_command_lease =
        claim_session_execution_lease_for_test(&store, "turn-first", "command-owner").await;
    assert!(
        store
            .claim_leading_ready_session_command(
                "turn-first",
                &rejected_command_lease.fence(),
                &lease_owner("command-owner"),
            )
            .await
            .expect("claim command behind turn")
            .is_none(),
        "session commands must not jump ahead of earlier turn work"
    );
    let turn_claim = store
        .claim_ready_queued_work(
            "turn-first",
            &rejected_command_lease.fence(),
            &lease_owner("command-owner"),
            QueuedWorkClaimBoundary::Idle,
            10,
        )
        .await
        .expect("claim turn before later command")
        .expect("turn claim exists");
    assert_eq!(turn_claim.batches[0].batch_id, first_turn.batch_id);
    store
        .abandon_queued_work_claim(&turn_claim)
        .await
        .expect("abandon turn claim");
    release_session_execution_lease_for_test(&store, &rejected_command_lease).await;
    assert_eq!(
        store
            .list_queued_work("turn-first")
            .await
            .expect("list turn-first queue")
            .iter()
            .map(|batch| batch.batch_id.as_str())
            .collect::<Vec<_>>(),
        vec![
            first_turn.batch_id.as_str(),
            second_command.batch_id.as_str()
        ]
    );
}

async fn queued_work_claims_respect_boundaries_abandon_and_stale_completion(
    store: Arc<dyn RuntimePersistence>,
) {
    let after_commit = store
        .enqueue_queued_work(queued_draft(
            "root",
            "after current commit",
            DeliveryPolicy::AfterCurrentTurnCommit,
            SlotPolicy::Exclusive,
        ))
        .await
        .expect("enqueue after-commit work");
    let earliest = store
        .enqueue_queued_work(queued_draft(
            "root",
            "earliest",
            DeliveryPolicy::EarliestSafeBoundary,
            SlotPolicy::Exclusive,
        ))
        .await
        .expect("enqueue earliest work");

    // A single live session lease governs the whole flow: a queued-work claim
    // blocks the checkpoint boundary only while its own generation still holds
    // the session lease (ADR 0029).
    let session_lease = claim_session_execution_lease_for_test(&store, "root", "owner-a").await;
    assert!(
        store
            .claim_ready_queued_work(
                "root",
                &session_lease.fence(),
                &lease_owner("owner-a"),
                QueuedWorkClaimBoundary::ActiveTurnCheckpoint,
                10,
            )
            .await
            .expect("checkpoint claim")
            .is_none(),
        "after-current-commit work at the queue head must wait for the idle boundary"
    );

    let idle_claim = store
        .claim_ready_queued_work(
            "root",
            &session_lease.fence(),
            &lease_owner("owner-a"),
            QueuedWorkClaimBoundary::Idle,
            10,
        )
        .await
        .expect("idle claim")
        .expect("idle claim exists");
    assert_eq!(idle_claim.batches.len(), 1);
    assert_eq!(idle_claim.batches[0].batch_id, after_commit.batch_id);

    // With the after-commit head held by this generation's own live claim, the
    // checkpoint boundary skips past it to the earliest-safe-boundary batch.
    let checkpoint_claim = store
        .claim_ready_queued_work(
            "root",
            &session_lease.fence(),
            &lease_owner("owner-a"),
            QueuedWorkClaimBoundary::ActiveTurnCheckpoint,
            10,
        )
        .await
        .expect("checkpoint claim after head is leased")
        .expect("checkpoint claim exists");
    assert_eq!(checkpoint_claim.batches[0].batch_id, earliest.batch_id);

    // Abandoning the idle claim frees the after-commit batch; reclaiming it under
    // the same live lease advances the fencing token.
    store
        .abandon_queued_work_claim(&idle_claim)
        .await
        .expect("abandon idle claim");
    let reclaimed = store
        .claim_ready_queued_work(
            "root",
            &session_lease.fence(),
            &lease_owner("owner-a"),
            QueuedWorkClaimBoundary::Idle,
            10,
        )
        .await
        .expect("reclaim abandoned work")
        .expect("reclaimed work exists");
    assert_eq!(reclaimed.batches[0].batch_id, after_commit.batch_id);
    assert!(
        reclaimed.fencing_token > idle_claim.fencing_token,
        "reclaiming abandoned work must advance the fencing token"
    );
    release_session_execution_lease_for_test(&store, &session_lease).await;

    // The pre-abandon claim's completion no longer owns any row: the reclaim
    // rewrote the batch's claim id + lease token, so committing the stale
    // completion is rejected as superseded (ADR 0029 keeps completion validation
    // by claim id + lease token; the abandon+reclaim is what supersedes it).
    let stale_state = RuntimeSessionState {
        session_id: "root".to_string(),
        ..RuntimeSessionState::default()
    };
    let stale_err = commit_runtime_state_for_test(
        &store,
        RuntimeCommit::persisted_state_for_test(&stale_state, &[])
            .completing_queue_claim(idle_claim.completion()),
        "owner-d",
    )
    .await
    .expect_err("stale pre-abandon completion must be rejected");
    assert!(
        matches!(stale_err, StoreError::QueuedWorkClaimSuperseded { .. }),
        "stale completion produced the wrong error: {stale_err:?}"
    );
    assert_eq!(
        store
            .list_queued_work("root")
            .await
            .expect("rejected stale completion preserves queued work")
            .len(),
        2,
        "rejected stale completion must not delete the reclaimed batch"
    );
}

pub async fn queued_work_claims_supersede_across_session_lease_generations(
    store: Arc<dyn RuntimePersistence>,
    lease_timing: RuntimePersistenceLeaseTiming,
) {
    queued_work_claims_supersede_across_session_lease_generations_with_timing(store, &lease_timing)
        .await;
}

async fn queued_work_claims_supersede_across_session_lease_generations_with_timing(
    store: Arc<dyn RuntimePersistence>,
    lease_timing: &RuntimePersistenceLeaseTiming,
) {
    let batch = store
        .enqueue_queued_work(queued_draft(
            "root",
            "generation work",
            DeliveryPolicy::EarliestSafeBoundary,
            SlotPolicy::Exclusive,
        ))
        .await
        .expect("enqueue generation work");

    // (a) Same generation: a live claim cannot re-claim its own row. The
    // caller's validated-live fence generation matches the row's pinned
    // generation, so self-steal is unrepresentable (ADR 0029).
    let lease_a = claim_session_execution_lease_for_test(&store, "root", "gen-owner-a").await;
    let claim_a = store
        .claim_ready_queued_work(
            "root",
            &lease_a.fence(),
            &lease_owner("gen-owner-a"),
            QueuedWorkClaimBoundary::Idle,
            10,
        )
        .await
        .expect("first-generation claim")
        .expect("first-generation claim exists");
    assert_eq!(claim_a.batches[0].batch_id, batch.batch_id);
    assert_eq!(claim_a.session_lease_generation, lease_a.fencing_token);
    assert!(
        store
            .claim_ready_queued_work(
                "root",
                &lease_a.fence(),
                &lease_owner("gen-owner-a"),
                QueuedWorkClaimBoundary::Idle,
                10,
            )
            .await
            .expect("same-generation re-claim")
            .is_none(),
        "a live claim must not be re-claimable under its own session-lease generation"
    );

    // (b) Release + re-acquire mints a new generation. Re-claiming the batch
    // replaces its ownership and supersedes the old generation's completion.
    release_session_execution_lease_for_test(&store, &lease_a).await;
    let lease_b = claim_session_execution_lease_for_test(&store, "root", "gen-owner-b").await;
    assert!(
        lease_b.fencing_token > lease_a.fencing_token,
        "re-acquisition must mint a fresh generation"
    );
    let claim_b = store
        .claim_ready_queued_work(
            "root",
            &lease_b.fence(),
            &lease_owner("gen-owner-b"),
            QueuedWorkClaimBoundary::Idle,
            10,
        )
        .await
        .expect("next-generation reclaim")
        .expect("next-generation reclaim exists");
    assert_eq!(claim_b.batches[0].batch_id, batch.batch_id);
    assert!(claim_b.fencing_token > claim_a.fencing_token);

    let stale_state = RuntimeSessionState {
        session_id: "root".to_string(),
        ..RuntimeSessionState::default()
    };
    let head_before_stale = store
        .load_session()
        .await
        .expect("load head before stale completion");
    let queue_before_stale = store
        .list_queued_work("root")
        .await
        .expect("load queue before stale completion");
    let stale_err = store
        .commit_runtime_state(
            RuntimeCommit::persisted_state_for_test(&stale_state, &[])
                .completing_queue_claim(claim_a.completion()),
        )
        .await
        .expect_err("superseded-generation completion must fail");
    assert!(
        matches!(
            stale_err,
            StoreError::QueuedWorkClaimSuperseded {
                ref row_id,
                ref superseding_claim_id,
                ref superseding_session_lease_generation,
                ..
            } if row_id.as_deref() == Some(batch.batch_id.as_str())
                && superseding_claim_id.as_deref() == Some(claim_b.claim_id.as_str())
                && superseding_session_lease_generation.as_deref()
                    == Some(&claim_b.session_lease_generation)
        ),
        "a superseded queued-work completion must report the row and current authority: {stale_err:?}"
    );
    assert_eq!(
        persisted_session_read_snapshot(
            store
                .load_session()
                .await
                .expect("load head after stale completion")
        ),
        persisted_session_read_snapshot(head_before_stale),
        "superseded completion must not mutate the durable head"
    );
    assert_eq!(
        serde_json::to_value(
            store
                .list_queued_work("root")
                .await
                .expect("load queue after stale completion")
        )
        .expect("serialize queue after stale completion"),
        serde_json::to_value(queue_before_stale).expect("serialize queue before stale completion"),
        "superseded completion must not mutate queued work"
    );
    release_session_execution_lease_for_test(&store, &lease_b).await;

    // (c) A TTL takeover mints a new generation without any release. The
    // successor's re-claim below is what supersedes the pre-takeover claim.
    let dead_owner = lease_owner("gen-stale");
    let dead_lease = store
        .try_claim_session_execution_lease(
            "root",
            &dead_owner,
            lease_timing.scaffolding_lease_ttl_ms(),
        )
        .await
        .expect("claim dead-owner lease")
        .acquired()
        .expect("dead-owner lease acquired");
    let claim_dead = store
        .claim_ready_queued_work(
            "root",
            &dead_lease.fence(),
            &dead_owner,
            QueuedWorkClaimBoundary::Idle,
            10,
        )
        .await
        .expect("dead-owner claim")
        .expect("dead-owner claim exists");
    let taker = lease_owner("gen-taker");
    let taker_lease = claim_session_execution_lease_after_expiry(
        &store,
        "root",
        &taker,
        lease_timing,
        "stale queued-work owner TTL",
    )
    .await;
    assert!(taker_lease.fencing_token > dead_lease.fencing_token);
    let claim_taker = store
        .claim_ready_queued_work(
            "root",
            &taker_lease.fence(),
            &taker,
            QueuedWorkClaimBoundary::Idle,
            10,
        )
        .await
        .expect("post-takeover claim")
        .expect("post-takeover claim exists");
    assert_eq!(claim_taker.batches[0].batch_id, batch.batch_id);
    let takeover_err = store
        .commit_runtime_state(
            RuntimeCommit::persisted_state_for_test(&stale_state, &[])
                .completing_queue_claim(claim_dead.completion()),
        )
        .await
        .expect_err("pre-takeover completion must fail");
    assert!(matches!(
        takeover_err,
        StoreError::QueuedWorkClaimSuperseded { .. }
    ));
}

fn persisted_session_read_snapshot(
    loaded: Option<crate::store::PersistedSessionRead>,
) -> serde_json::Value {
    loaded.map_or(serde_json::Value::Null, |loaded| {
        let checkpoint = loaded.checkpoint.map(|checkpoint| {
            serde_json::json!({
                "tool_state_ref": checkpoint.tool_state_ref,
                "tool_state": checkpoint.tool_state,
                "plugin_snapshot_ref": checkpoint.plugin_snapshot_ref,
                "plugin_snapshot": checkpoint.plugin_snapshot,
                "plugin_snapshot_revision": checkpoint.plugin_snapshot_revision,
                "execution_state_ref": checkpoint.execution_state_ref,
                "execution_state": checkpoint.execution_state,
            })
        });
        serde_json::json!({
            "head_revision": loaded.head_revision,
            "current_frame_node_id": loaded.current_frame_node_id,
            "graph": loaded.graph,
            "checkpoint_ref": loaded.checkpoint_ref,
            "checkpoint": checkpoint,
            "token_ledger": loaded.token_ledger,
        })
    })
}

async fn claim_both_generation_fenced_lanes(
    store: &Arc<dyn RuntimePersistence>,
    session_id: &str,
    owner: &crate::LeaseOwnerIdentity,
    lease_ttl_ms: u64,
) -> (
    QueuedWorkBatch,
    crate::PendingTurnInput,
    crate::SessionExecutionLease,
    crate::QueuedWorkClaim,
    crate::TurnInputClaim,
) {
    let batch = store
        .enqueue_queued_work(queued_draft(
            session_id,
            "lease-less liveness work",
            DeliveryPolicy::EarliestSafeBoundary,
            SlotPolicy::Exclusive,
        ))
        .await
        .expect("enqueue generation-fenced queued work");
    let input = store
        .enqueue_pending_turn_input(pending_next_turn_input_draft(
            session_id,
            "lease-less liveness input",
        ))
        .await
        .expect("enqueue generation-fenced turn input");
    let lease = store
        .try_claim_session_execution_lease(session_id, owner, lease_ttl_ms)
        .await
        .expect("claim session lease for both claim lanes")
        .acquired()
        .expect("session lease for both claim lanes is free");
    let queue_claim = store
        .claim_ready_queued_work(
            session_id,
            &lease.fence(),
            owner,
            QueuedWorkClaimBoundary::Idle,
            1,
        )
        .await
        .expect("claim generation-fenced queued work")
        .expect("generation-fenced queued work claim exists");
    let input_claim = store
        .claim_next_turn_inputs(session_id, &lease.fence(), owner, 1)
        .await
        .expect("claim generation-fenced turn input")
        .expect("generation-fenced turn input claim exists");
    (batch, input, lease, queue_claim, input_claim)
}

async fn assert_both_retained_claims_are_visible_and_cancellable(
    store: &Arc<dyn RuntimePersistence>,
    session_id: &str,
    batch: &QueuedWorkBatch,
    input: &crate::PendingTurnInput,
) {
    assert_eq!(
        store
            .list_pending_queued_work(session_id)
            .await
            .expect("list queued work after claim generation stopped being live")
            .iter()
            .map(|batch| batch.batch_id.as_str())
            .collect::<Vec<_>>(),
        vec![batch.batch_id.as_str()],
        "a queued-work claim whose generation is no longer live must be visible"
    );
    assert_eq!(
        store
            .list_pending_turn_inputs(session_id)
            .await
            .expect("list turn inputs after claim generation stopped being live")
            .iter()
            .map(|input| input.input_id.as_str())
            .collect::<Vec<_>>(),
        vec![input.input_id.as_str()],
        "a turn-input claim whose generation is no longer live must be visible"
    );
    let cancelled_batch = store
        .cancel_queued_work_batch(session_id, &batch.batch_id)
        .await
        .expect("cancel queued work after claim generation stopped being live")
        .expect("queued work with a non-live claim generation is cancellable");
    assert_eq!(cancelled_batch.batch_id, batch.batch_id);
    let cancelled_input = store
        .cancel_pending_turn_input(session_id, &input.input_id)
        .await
        .expect("cancel turn input after claim generation stopped being live");
    expect_cancelled_pending_input(cancelled_input, &input.input_id);
}

async fn claim_liveness_for_lease_less_paths_tracks_session_generations(
    store: Arc<dyn RuntimePersistence>,
    lease_timing: &RuntimePersistenceLeaseTiming,
) {
    // Release: retain both claim rows, then clear the lease token without
    // abandoning either claim. Lease-less paths must immediately treat both
    // rows as pending again.
    let release_owner = lease_owner("lease-less-release-owner");
    let (batch, input, lease, _queue_claim, _input_claim) =
        claim_both_generation_fenced_lanes(&store, "lease-less-release", &release_owner, 60_000)
            .await;
    release_session_execution_lease_for_test(&store, &lease).await;
    assert_both_retained_claims_are_visible_and_cancellable(
        &store,
        "lease-less-release",
        &batch,
        &input,
    )
    .await;

    // Expiry: the lease row still carries the generation, but its token is no
    // longer live once the TTL elapses. The correlated SQL predicates must not
    // mistake generation equality alone for a live claim.
    let expiry_owner = lease_owner("lease-less-expiry-owner");
    let (batch, input, _lease, _queue_claim, _input_claim) = claim_both_generation_fenced_lanes(
        &store,
        "lease-less-expiry",
        &expiry_owner,
        lease_timing.scaffolding_lease_ttl_ms(),
    )
    .await;
    lease_timing.wait_until_expired().await;
    assert_both_retained_claims_are_visible_and_cancellable(
        &store,
        "lease-less-expiry",
        &batch,
        &input,
    )
    .await;

    // TTL takeover advances to a different generation. Claims retained from
    // the expired generation are no longer live for lease-less callers.
    let dead_owner = lease_owner("lease-less-stale");
    let (batch, input, _dead_lease, _queue_claim, _input_claim) =
        claim_both_generation_fenced_lanes(
            &store,
            "lease-less-takeover",
            &dead_owner,
            lease_timing.scaffolding_lease_ttl_ms(),
        )
        .await;
    let taker = lease_owner("lease-less-taker");
    let taker_lease = claim_session_execution_lease_after_expiry(
        &store,
        "lease-less-takeover",
        &taker,
        lease_timing,
        "lease-less owner TTL",
    )
    .await;
    assert_both_retained_claims_are_visible_and_cancellable(
        &store,
        "lease-less-takeover",
        &batch,
        &input,
    )
    .await;
    release_session_execution_lease_for_test(&store, &taker_lease).await;
}

pub async fn same_generation_claim_scans_reach_rows_beyond_the_scan_surplus(
    store: Arc<dyn RuntimePersistence>,
) {
    const ROW_COUNT: usize = 34;

    let queue_session = "bounded-scan-queue";
    let queue_owner = lease_owner("bounded-scan-queue-owner");
    let mut queue_batches = Vec::with_capacity(ROW_COUNT);
    for index in 0..ROW_COUNT {
        queue_batches.push(
            store
                .enqueue_queued_work(queued_draft(
                    queue_session,
                    &format!("bounded queue {index}"),
                    DeliveryPolicy::EarliestSafeBoundary,
                    SlotPolicy::Exclusive,
                ))
                .await
                .expect("enqueue bounded-scan queued work"),
        );
    }
    let queue_lease = store
        .try_claim_session_execution_lease(queue_session, &queue_owner, 60_000)
        .await
        .expect("claim bounded-scan queue session lease")
        .acquired()
        .expect("bounded-scan queue session lease is free");
    for expected in &queue_batches {
        let claim = store
            .claim_ready_queued_work(
                queue_session,
                &queue_lease.fence(),
                &queue_owner,
                QueuedWorkClaimBoundary::Idle,
                1,
            )
            .await
            .expect("claim bounded-scan queued work")
            .expect("bounded-scan queued work remains reachable");
        assert_eq!(claim.batches[0].batch_id, expected.batch_id);
    }
    release_session_execution_lease_for_test(&store, &queue_lease).await;

    let command_session = "bounded-scan-command";
    let command_owner = lease_owner("bounded-scan-command-owner");
    let mut command_batches = Vec::with_capacity(ROW_COUNT);
    for index in 0..ROW_COUNT {
        command_batches.push(
            store
                .enqueue_queued_work(queued_session_command_draft(
                    command_session,
                    &format!("bounded command {index}"),
                ))
                .await
                .expect("enqueue bounded-scan session command"),
        );
    }
    let command_lease = store
        .try_claim_session_execution_lease(command_session, &command_owner, 60_000)
        .await
        .expect("claim bounded-scan command session lease")
        .acquired()
        .expect("bounded-scan command session lease is free");
    for expected in &command_batches {
        let claim = store
            .claim_leading_ready_session_command(
                command_session,
                &command_lease.fence(),
                &command_owner,
            )
            .await
            .expect("claim bounded-scan session command")
            .expect("bounded-scan session command remains reachable");
        assert_eq!(claim.batches[0].batch_id, expected.batch_id);
    }
    release_session_execution_lease_for_test(&store, &command_lease).await;

    let input_session = "bounded-scan-turn-input";
    let input_owner = lease_owner("bounded-scan-turn-input-owner");
    let mut inputs = Vec::with_capacity(ROW_COUNT);
    for index in 0..ROW_COUNT {
        inputs.push(
            store
                .enqueue_pending_turn_input(pending_next_turn_input_draft(
                    input_session,
                    &format!("bounded turn input {index}"),
                ))
                .await
                .expect("enqueue bounded-scan turn input"),
        );
    }
    let input_lease = store
        .try_claim_session_execution_lease(input_session, &input_owner, 60_000)
        .await
        .expect("claim bounded-scan turn-input session lease")
        .acquired()
        .expect("bounded-scan turn-input session lease is free");
    for expected in &inputs {
        let claim = store
            .claim_next_turn_inputs(input_session, &input_lease.fence(), &input_owner, 1)
            .await
            .expect("claim bounded-scan turn input")
            .expect("bounded-scan turn input remains reachable");
        assert_eq!(claim.inputs[0].input_id, expected.input_id);
    }
    release_session_execution_lease_for_test(&store, &input_lease).await;
}

async fn queued_work_respects_membership_limits_exclusivity_reclaim_and_sessions(
    store: Arc<dyn RuntimePersistence>,
) {
    store
        .enqueue_queued_work(
            queued_draft(
                "root",
                "not ready",
                DeliveryPolicy::EarliestSafeBoundary,
                SlotPolicy::Exclusive,
            )
            .with_available_at_ms(4_102_444_800_000),
        )
        .await
        .expect("enqueue unavailable work");
    let exclusive = store
        .enqueue_queued_work(queued_draft(
            "root",
            "exclusive",
            DeliveryPolicy::EarliestSafeBoundary,
            SlotPolicy::Exclusive,
        ))
        .await
        .expect("enqueue exclusive work");
    let joined = store
        .enqueue_queued_work(
            queued_draft(
                "root",
                "joined",
                DeliveryPolicy::EarliestSafeBoundary,
                SlotPolicy::Join,
            )
            .with_merge_key(MergeKey::Group("root".to_string())),
        )
        .await
        .expect("enqueue joined work");
    let other = store
        .enqueue_queued_work(queued_draft(
            "other",
            "other session",
            DeliveryPolicy::EarliestSafeBoundary,
            SlotPolicy::Exclusive,
        ))
        .await
        .expect("enqueue other session work");

    // Both root claims run under one live session lease: advancing through the
    // queue relies on each claimed batch staying held by the current generation,
    // so a same-generation follow-up claim skips it (ADR 0029).
    let root_session_lease =
        claim_session_execution_lease_for_test(&store, "root", "owner-a").await;
    let claim = store
        .claim_ready_queued_work(
            "root",
            &root_session_lease.fence(),
            &lease_owner("owner-a"),
            QueuedWorkClaimBoundary::Idle,
            10,
        )
        .await
        .expect("claim root")
        .expect("root claim");
    assert_eq!(
        claim
            .batches
            .iter()
            .map(|batch| batch.batch_id.as_str())
            .collect::<Vec<_>>(),
        vec![exclusive.batch_id.as_str()],
        "an exclusive batch must claim alone and unavailable earlier work must be skipped"
    );
    let next_root = store
        .claim_ready_queued_work(
            "root",
            &root_session_lease.fence(),
            &lease_owner("owner-a"),
            QueuedWorkClaimBoundary::Idle,
            10,
        )
        .await
        .expect("claim joined")
        .expect("joined claim");
    release_session_execution_lease_for_test(&store, &root_session_lease).await;
    assert_eq!(next_root.batches[0].batch_id, joined.batch_id);
    let other_session_lease =
        claim_session_execution_lease_for_test(&store, "other", "owner-c").await;
    let other_claim = store
        .claim_ready_queued_work(
            "other",
            &other_session_lease.fence(),
            &lease_owner("owner-c"),
            QueuedWorkClaimBoundary::Idle,
            10,
        )
        .await
        .expect("claim other")
        .expect("other claim");
    release_session_execution_lease_for_test(&store, &other_session_lease).await;
    assert_eq!(
        other_claim.batches[0].batch_id, other.batch_id,
        "claiming one session must not consume queued work from another session"
    );

    let reclaimed_source = store
        .enqueue_queued_work(queued_draft(
            "reclaim",
            "superseded claim",
            DeliveryPolicy::EarliestSafeBoundary,
            SlotPolicy::Exclusive,
        ))
        .await
        .expect("enqueue reclaim work");
    let first_generation_lease =
        claim_session_execution_lease_for_test(&store, "reclaim", "owner-a").await;
    let first_generation_claim = store
        .claim_ready_queued_work(
            "reclaim",
            &first_generation_lease.fence(),
            &lease_owner("owner-a"),
            QueuedWorkClaimBoundary::Idle,
            1,
        )
        .await
        .expect("claim under the first generation")
        .expect("first-generation claim");
    release_session_execution_lease_for_test(&store, &first_generation_lease).await;
    let reclaim_session_lease =
        claim_session_execution_lease_for_test(&store, "reclaim", "owner-b").await;
    let reclaimed = store
        .claim_ready_queued_work(
            "reclaim",
            &reclaim_session_lease.fence(),
            &lease_owner("owner-b"),
            QueuedWorkClaimBoundary::Idle,
            1,
        )
        .await
        .expect("reclaim under a new generation")
        .expect("reclaimed superseded claim");
    release_session_execution_lease_for_test(&store, &reclaim_session_lease).await;
    assert_eq!(reclaimed.batches[0].batch_id, reclaimed_source.batch_id);
    assert!(
        reclaimed.fencing_token > first_generation_claim.fencing_token,
        "reclaiming a claim across a session-lease generation must bump the fencing token"
    );

    let limited_first = store
        .enqueue_queued_work(
            queued_draft(
                "limited",
                "one",
                DeliveryPolicy::EarliestSafeBoundary,
                SlotPolicy::Join,
            )
            .with_merge_key(MergeKey::Group("limited".to_string())),
        )
        .await
        .expect("enqueue limited one");
    let limited_second = store
        .enqueue_queued_work(
            queued_draft(
                "limited",
                "two",
                DeliveryPolicy::EarliestSafeBoundary,
                SlotPolicy::Join,
            )
            .with_merge_key(MergeKey::Group("limited".to_string())),
        )
        .await
        .expect("enqueue limited two");
    let limited_third = store
        .enqueue_queued_work(
            queued_draft(
                "limited",
                "three",
                DeliveryPolicy::EarliestSafeBoundary,
                SlotPolicy::Join,
            )
            .with_merge_key(MergeKey::Group("limited".to_string())),
        )
        .await
        .expect("enqueue limited three");
    // One live lease: the capped claim keeps the first two batches held, so the
    // same-generation follow-up claim only sees the third (ADR 0029).
    let limited_session_lease =
        claim_session_execution_lease_for_test(&store, "limited", "owner").await;
    let limited = store
        .claim_ready_queued_work(
            "limited",
            &limited_session_lease.fence(),
            &lease_owner("owner"),
            QueuedWorkClaimBoundary::Idle,
            2,
        )
        .await
        .expect("limited claim")
        .expect("limited claim exists");
    assert_eq!(
        limited
            .batches
            .iter()
            .map(|batch| batch.batch_id.as_str())
            .collect::<Vec<_>>(),
        vec![
            limited_first.batch_id.as_str(),
            limited_second.batch_id.as_str()
        ],
        "max_batches must cap a join claim"
    );
    let remaining = store
        .claim_ready_queued_work(
            "limited",
            &limited_session_lease.fence(),
            &lease_owner("owner"),
            QueuedWorkClaimBoundary::Idle,
            10,
        )
        .await
        .expect("remaining claim")
        .expect("remaining claim exists");
    release_session_execution_lease_for_test(&store, &limited_session_lease).await;
    assert_eq!(remaining.batches[0].batch_id, limited_third.batch_id);
}

async fn queued_work_join_groups_by_delivery_policy_and_merge_key(
    store: Arc<dyn RuntimePersistence>,
) {
    let first = store
        .enqueue_queued_work(
            queued_draft(
                "root",
                "group a one",
                DeliveryPolicy::EarliestSafeBoundary,
                SlotPolicy::Join,
            )
            .with_merge_key(MergeKey::Group("a".to_string())),
        )
        .await
        .expect("enqueue group a one");
    let second = store
        .enqueue_queued_work(
            queued_draft(
                "root",
                "group a two",
                DeliveryPolicy::EarliestSafeBoundary,
                SlotPolicy::Join,
            )
            .with_merge_key(MergeKey::Group("a".to_string())),
        )
        .await
        .expect("enqueue group a two");
    let different_merge = store
        .enqueue_queued_work(
            queued_draft(
                "root",
                "group b",
                DeliveryPolicy::EarliestSafeBoundary,
                SlotPolicy::Join,
            )
            .with_merge_key(MergeKey::Group("b".to_string())),
        )
        .await
        .expect("enqueue group b");
    let different_delivery = store
        .enqueue_queued_work(
            queued_draft(
                "root",
                "after commit",
                DeliveryPolicy::AfterCurrentTurnCommit,
                SlotPolicy::Join,
            )
            .with_merge_key(MergeKey::Group("a".to_string())),
        )
        .await
        .expect("enqueue after-commit");

    // All three group claims run under one live session lease so each claimed
    // group stays held by the current generation and the next same-generation
    // claim advances to the following group (ADR 0029).
    let session_lease = claim_session_execution_lease_for_test(&store, "root", "owner-a").await;
    let first_claim = store
        .claim_ready_queued_work(
            "root",
            &session_lease.fence(),
            &lease_owner("owner-a"),
            QueuedWorkClaimBoundary::Idle,
            10,
        )
        .await
        .expect("claim first group")
        .expect("first group claim");
    assert_eq!(
        first_claim
            .batches
            .iter()
            .map(|batch| batch.batch_id.as_str())
            .collect::<Vec<_>>(),
        vec![first.batch_id.as_str(), second.batch_id.as_str()],
        "join claims must group only adjacent batches with the same delivery policy and merge key"
    );
    let second_claim = store
        .claim_ready_queued_work(
            "root",
            &session_lease.fence(),
            &lease_owner("owner-a"),
            QueuedWorkClaimBoundary::Idle,
            10,
        )
        .await
        .expect("claim second group")
        .expect("second group claim");
    assert_eq!(second_claim.batches[0].batch_id, different_merge.batch_id);
    let third_claim = store
        .claim_ready_queued_work(
            "root",
            &session_lease.fence(),
            &lease_owner("owner-a"),
            QueuedWorkClaimBoundary::Idle,
            10,
        )
        .await
        .expect("claim third group")
        .expect("third group claim");
    release_session_execution_lease_for_test(&store, &session_lease).await;
    assert_eq!(third_claim.batches[0].batch_id, different_delivery.batch_id);
}

async fn wake_turn_policy_controls_coalescing(store: Arc<dyn RuntimePersistence>) {
    let default_policy = crate::WakeTurnPolicy::default();
    assert_eq!(
        default_policy,
        crate::WakeTurnPolicy::each_wake(
            DeliveryPolicy::EarliestSafeBoundary,
            SlotPolicy::Exclusive,
        ),
        "the host policy default must preserve the pre-configuration behavior"
    );
    for process_id in ["exclusive-a", "exclusive-b"] {
        store
            .enqueue_queued_work(crate::process_wake_batch_draft_with_policy(
                policy_test_wake("wake-policy-exclusive", process_id, 1),
                &default_policy,
            ))
            .await
            .expect("enqueue default-policy wake");
    }
    let exclusive_lease =
        claim_session_execution_lease_for_test(&store, "wake-policy-exclusive", "exclusive-owner")
            .await;
    let first_exclusive = store
        .claim_ready_queued_work(
            "wake-policy-exclusive",
            &exclusive_lease.fence(),
            &lease_owner("exclusive-owner"),
            QueuedWorkClaimBoundary::Idle,
            10,
        )
        .await
        .expect("claim first default-policy wake")
        .expect("first default-policy wake exists");
    let second_exclusive = store
        .claim_ready_queued_work(
            "wake-policy-exclusive",
            &exclusive_lease.fence(),
            &lease_owner("exclusive-owner"),
            QueuedWorkClaimBoundary::Idle,
            10,
        )
        .await
        .expect("claim second default-policy wake")
        .expect("second default-policy wake exists");
    release_session_execution_lease_for_test(&store, &exclusive_lease).await;
    assert_eq!(first_exclusive.batches.len(), 1);
    assert_eq!(
        second_exclusive.batches.len(),
        1,
        "exclusive wakes must remain separate turns by default"
    );

    let join_each_policy =
        crate::WakeTurnPolicy::each_wake(DeliveryPolicy::EarliestSafeBoundary, SlotPolicy::Join);
    for process_id in ["join-each-a", "join-each-b"] {
        store
            .enqueue_queued_work(crate::process_wake_batch_draft_with_policy(
                policy_test_wake("wake-policy-join-each", process_id, 1),
                &join_each_policy,
            ))
            .await
            .expect("enqueue join-each wake");
    }
    let join_each_lease =
        claim_session_execution_lease_for_test(&store, "wake-policy-join-each", "join-each-owner")
            .await;
    for claim_number in 1..=2 {
        let claim = store
            .claim_ready_queued_work(
                "wake-policy-join-each",
                &join_each_lease.fence(),
                &lease_owner("join-each-owner"),
                QueuedWorkClaimBoundary::Idle,
                10,
            )
            .await
            .expect("claim join-each wake")
            .expect("join-each wake exists");
        assert_eq!(
            claim.batches.len(),
            1,
            "each-wake mode with a join slot must not merge claim {claim_number}"
        );
    }
    release_session_execution_lease_for_test(&store, &join_each_lease).await;

    let merge_policy = crate::WakeTurnPolicy::coalesce(
        DeliveryPolicy::EarliestSafeBoundary,
        crate::WakeCoalescingKey::Group("process-wakes".to_string()),
    );
    let merged_wakes = [
        policy_test_wake("wake-policy-merge", "merge-same-process", 1),
        policy_test_wake("wake-policy-merge", "merge-same-process", 2),
    ];
    for wake in &merged_wakes {
        store
            .enqueue_queued_work(crate::process_wake_batch_draft_with_policy(
                wake.clone(),
                &merge_policy,
            ))
            .await
            .expect("enqueue merge-policy wake");
    }
    let merge_lease =
        claim_session_execution_lease_for_test(&store, "wake-policy-merge", "merge-owner").await;
    let merged = store
        .claim_ready_queued_work(
            "wake-policy-merge",
            &merge_lease.fence(),
            &lease_owner("merge-owner"),
            QueuedWorkClaimBoundary::Idle,
            10,
        )
        .await
        .expect("claim merge-policy wakes")
        .expect("merge-policy wakes exist");
    assert_eq!(
        merged.batches.len(),
        2,
        "merge-enabled wake policy must coalesce two sequences from one process"
    );
    let state = RuntimeSessionState {
        session_id: "wake-policy-merge".to_string(),
        ..RuntimeSessionState::default()
    };
    store
        .commit_runtime_state(
            RuntimeCommit::persisted_state_for_test(&state, &[])
                .releasing_session_execution_lease(merge_lease.completion())
                .completing_queue_claim(merged.completion()),
        )
        .await
        .expect("settle every batch in merged wake claim");
    assert!(
        store
            .list_queued_work("wake-policy-merge")
            .await
            .expect("list merged queue after settlement")
            .is_empty(),
        "merged settlement must delete every claimed receiver row"
    );
    for wake in merged_wakes {
        let error = store
            .enqueue_queued_work(crate::process_wake_batch_draft_with_policy(
                wake,
                &merge_policy,
            ))
            .await
            .expect_err("settled wake without a live row must trip the receiver floor");
        assert!(matches!(
            error,
            StoreError::ProcessWakeSequenceRewound { .. }
        ));
    }
}

fn policy_test_wake(session_id: &str, process_id: &str, sequence: u64) -> ProcessWakeDelivery {
    ProcessWakeDelivery {
        wake_id: format!("wake:{process_id}:{sequence}"),
        target_session_id: session_id.to_string(),
        process_id: process_id.to_string(),
        sequence,
        event_type: "process.wake".to_string(),
        event_invocation: RuntimeInvocation {
            scope: RuntimeScope::new(session_id),
            subject: RuntimeSubject::ProcessEvent {
                process_id: process_id.to_string(),
                sequence,
                event_type: "process.wake".to_string(),
            },
            caused_by: None,
            replay: None,
        },
        process_caused_by: None,
        input: process_id.to_string(),
        created_at_ms: 1,
    }
}

async fn queued_work_completion_is_lease_guarded(store: Arc<dyn RuntimePersistence>) {
    let first = store
        .enqueue_queued_work(
            queued_draft(
                "root",
                "join one",
                DeliveryPolicy::EarliestSafeBoundary,
                SlotPolicy::Join,
            )
            .with_merge_key(MergeKey::Group("joined".to_string())),
        )
        .await
        .expect("enqueue first joined batch");
    let second = store
        .enqueue_queued_work(
            queued_draft(
                "root",
                "join two",
                DeliveryPolicy::EarliestSafeBoundary,
                SlotPolicy::Join,
            )
            .with_merge_key(MergeKey::Group("joined".to_string())),
        )
        .await
        .expect("enqueue second joined batch");
    let claim_session_lease =
        claim_session_execution_lease_for_test(&store, "root", "owner-a").await;
    let claim = store
        .claim_ready_queued_work(
            "root",
            &claim_session_lease.fence(),
            &lease_owner("owner-a"),
            QueuedWorkClaimBoundary::Idle,
            10,
        )
        .await
        .expect("claim joined batches")
        .expect("joined claim exists");
    assert_eq!(
        claim
            .batches
            .iter()
            .map(|batch| batch.batch_id.as_str())
            .collect::<Vec<_>>(),
        vec![first.batch_id.as_str(), second.batch_id.as_str()]
    );

    let mut stale_completion = claim.completion();
    stale_completion.lease_token.push_str(":stale");
    let state = RuntimeSessionState {
        session_id: "root".to_string(),
        ..RuntimeSessionState::default()
    };
    let err = store
        .commit_runtime_state(
            RuntimeCommit::persisted_state_for_test(&state, &[])
                .completing_queue_claim(stale_completion),
        )
        .await
        .expect_err("stale queued-work completion must fail");
    assert!(matches!(err, StoreError::QueuedWorkClaimSuperseded { .. }));
    assert_eq!(
        store
            .list_queued_work("root")
            .await
            .expect("stale completion preserves queued work")
            .len(),
        2
    );

    store
        .commit_runtime_state(
            RuntimeCommit::persisted_state_for_test(&state, &[])
                .releasing_session_execution_lease(claim_session_lease.completion())
                .completing_queue_claim(claim.completion()),
        )
        .await
        .expect("valid queued-work completion commits");
    assert!(
        store
            .list_queued_work("root")
            .await
            .expect("valid completion clears queued work")
            .is_empty()
    );
}

async fn queue_completion_and_turn_commit_stamp_are_atomic(store: Arc<dyn RuntimePersistence>) {
    let batch = store
        .enqueue_queued_work(queued_draft(
            "root",
            "atomic queue",
            DeliveryPolicy::EarliestSafeBoundary,
            SlotPolicy::Exclusive,
        ))
        .await
        .expect("enqueue queue batch");
    let session_lease = claim_session_execution_lease_for_test(&store, "root", "queue-owner").await;
    let claim = store
        .claim_ready_queued_work(
            "root",
            &session_lease.fence(),
            &lease_owner("queue-owner"),
            QueuedWorkClaimBoundary::Idle,
            1,
        )
        .await
        .expect("claim queue")
        .expect("queue claim");
    assert_eq!(claim.batches[0].batch_id, batch.batch_id);
    let input = store
        .enqueue_pending_turn_input(pending_next_turn_input_draft(
            "root",
            "atomic pending input",
        ))
        .await
        .expect("enqueue atomic pending input");
    let input_claim = store
        .claim_next_turn_inputs(
            "root",
            &session_lease.fence(),
            &lease_owner("queue-owner"),
            1,
        )
        .await
        .expect("claim atomic pending input")
        .expect("atomic pending input claim");
    assert_eq!(input_claim.inputs[0].input_id, input.input_id);
    let state = RuntimeSessionState {
        session_id: "root".to_string(),
        turn_index: 41,
        ..RuntimeSessionState::default()
    };
    let mut base_commit = RuntimeCommit::persisted_state_for_test(&state, &[]);
    base_commit.enqueued_queue_batches = vec![
        QueuedWorkBatchDraft::new(
            "root",
            DeliveryPolicy::AfterCurrentTurnCommit,
            SlotPolicy::Exclusive,
            vec![QueuedWorkPayload::agent_frame_task(
                "follow-frame",
                "follow-on task",
                None,
            )],
        )
        .with_source_key("agent-frame-handoff:turn-atomic"),
    ];
    let turn_commit =
        RuntimeTurnCommitStamp::new(crate::OperationId::turn("root", "turn-atomic", "final"));
    base_commit.turn_commit = turn_commit.clone();
    let mut stale_queue_completion = claim.completion();
    stale_queue_completion.lease_token.push_str(":stale");
    let err = store
        .commit_runtime_state(
            base_commit
                .clone()
                .completing_turn_input_claim(input_claim.completion())
                .completing_queue_claim(stale_queue_completion),
        )
        .await
        .expect_err("stale queue completion must reject the whole final commit");
    assert!(matches!(err, StoreError::QueuedWorkClaimSuperseded { .. }));
    assert!(
        store
            .load_session()
            .await
            .expect("load after rejected atomic commit")
            .is_none(),
        "rejected queue completion must not persist session state"
    );
    assert_eq!(
        store
            .list_queued_work("root")
            .await
            .expect("list after rejected atomic commit")
            .len(),
        1,
        "rejected queue completion must preserve queued work"
    );

    let mut cross_session_outbox = base_commit.clone();
    cross_session_outbox.enqueued_queue_batches[0].session_id = "other-session".to_string();
    let err = store
        .commit_runtime_state(
            cross_session_outbox
                .completing_queue_claim(claim.completion())
                .completing_turn_input_claim(input_claim.completion()),
        )
        .await
        .expect_err("outbox enqueue failure must reject the whole final commit");
    assert!(matches!(err, StoreError::SessionBindingMismatch { .. }));
    assert!(
        store
            .load_session()
            .await
            .expect("load after rejected outbox enqueue")
            .is_none(),
        "rejected outbox enqueue must roll back session state"
    );
    assert_eq!(
        store
            .list_queued_work("root")
            .await
            .expect("list after rejected outbox enqueue")
            .len(),
        1,
        "rejected outbox enqueue must roll back inbound queue completion"
    );

    let first = store
        .commit_runtime_state(
            base_commit
                .clone()
                .releasing_session_execution_lease(session_lease.completion())
                .completing_turn_input_claim(input_claim.completion())
                .completing_queue_claim(claim.completion()),
        )
        .await
        .expect("valid final commit clears queue and records the turn stamp atomically");
    let retry = store
        .commit_runtime_state({
            let mut retry = base_commit;
            retry.turn_commit = RuntimeTurnCommitStamp::new(crate::OperationId::turn(
                "root",
                "turn-atomic",
                "final",
            ));
            retry
                .releasing_session_execution_lease(session_lease.completion())
                .completing_turn_input_claim(input_claim.completion())
                .completing_queue_claim(claim.completion())
        })
        .await
        .expect("same final turn commit stamp retries idempotently");
    assert_eq!(retry.head_revision, first.head_revision);
    assert_eq!(retry.checkpoint_ref, first.checkpoint_ref);
    assert_eq!(
        retry.realized_node_timestamps,
        first.realized_node_timestamps
    );
    assert_eq!(first.enqueued_queue_batches.len(), 1);
    assert_eq!(retry.enqueued_queue_batches.len(), 1);
    assert_eq!(
        retry.enqueued_queue_batches[0].batch_id, first.enqueued_queue_batches[0].batch_id,
        "idempotent commit retry must return the original outbox identity"
    );
    assert!(
        store
            .load_session()
            .await
            .expect("load after accepted atomic commit")
            .is_some()
    );
    assert!(
        store
            .list_queued_work("root")
            .await
            .expect("list after accepted atomic commit")
            .iter()
            .map(|batch| batch.batch_id.as_str())
            .eq([first.enqueued_queue_batches[0].batch_id.as_str()])
    );
    assert!(
        store
            .list_pending_turn_inputs("root")
            .await
            .expect("list inputs after accepted atomic commit")
            .is_empty(),
        "accepted switch commit must complete inbound input with the outbox enqueue"
    );
}

async fn pending_turn_inputs_source_keys_order_cancel_and_cross_session(
    store: Arc<dyn RuntimePersistence>,
) {
    let first = store
        .enqueue_pending_turn_input(
            pending_next_turn_input_draft("root", "first").with_source_key("source:first"),
        )
        .await
        .expect("enqueue first pending input");
    let replay = store
        .enqueue_pending_turn_input(
            pending_next_turn_input_draft("root", "first").with_source_key("source:first"),
        )
        .await
        .expect("replay first pending input");
    let conflict = store
        .enqueue_pending_turn_input(
            pending_next_turn_input_draft("root", "different replay payload")
                .with_source_key("source:first"),
        )
        .await
        .expect_err("same source key with changed content must conflict");
    assert!(matches!(
        conflict,
        StoreError::PendingTurnInputSourceKeyConflict {
            session_id,
            source_key,
            existing_input_id,
        } if session_id == "root"
            && source_key == "source:first"
            && existing_input_id == first.input_id
    ));
    let second = store
        .enqueue_pending_turn_input(pending_next_turn_input_draft("root", "second"))
        .await
        .expect("enqueue second pending input");
    store
        .enqueue_pending_turn_input(pending_next_turn_input_draft("other", "other session"))
        .await
        .expect("enqueue other session pending input");

    assert_eq!(
        first.input_id, replay.input_id,
        "replaying a source key must return the original pending input"
    );
    assert_eq!(
        pending_input_text(&replay),
        Some("first"),
        "source-key replay must return the original stored payload, not the replay attempt"
    );
    let listed = store
        .list_pending_turn_inputs("root")
        .await
        .expect("list pending turn inputs");
    assert_eq!(
        listed
            .iter()
            .map(|input| input.input_id.as_str())
            .collect::<Vec<_>>(),
        vec![first.input_id.as_str(), second.input_id.as_str()]
    );
    assert!(listed[0].enqueue_seq < listed[1].enqueue_seq);
    assert!(listed.iter().all(|input| input.session_id == "root"));

    let cancelled = store
        .cancel_pending_turn_input("root", &second.input_id)
        .await
        .expect("cancel pending turn input");
    expect_cancelled_pending_input(cancelled, &second.input_id);
    assert!(matches!(
        store
            .cancel_pending_turn_input("root", &second.input_id)
            .await
            .expect("cancel pending turn input replay"),
        crate::PendingTurnInputCancelOutcome::AlreadyCancelled(input)
            if input.input_id == second.input_id
    ));
    assert_eq!(
        store
            .list_pending_turn_inputs("root")
            .await
            .expect("list after cancel")
            .iter()
            .map(|input| input.input_id.as_str())
            .collect::<Vec<_>>(),
        vec![first.input_id.as_str()]
    );

    let cancelled_first = store
        .cancel_pending_turn_input("root", &first.input_id)
        .await
        .expect("cancel source-keyed pending turn input");
    expect_cancelled_pending_input(cancelled_first, &first.input_id);
    let terminal_replay = store
        .enqueue_pending_turn_input(
            pending_next_turn_input_draft("root", "first").with_source_key("source:first"),
        )
        .await
        .expect("exact replay after cancellation");
    assert_eq!(terminal_replay.input_id, first.input_id);
    assert_eq!(terminal_replay.state, crate::TurnInputState::Cancelled);
    assert!(
        store
            .list_pending_turn_inputs("root")
            .await
            .expect("list after terminal replay")
            .is_empty()
    );
    let vacuum = store
        .vacuum()
        .await
        .expect("vacuum pending input tombstones");
    assert_eq!(vacuum.removed_node_count, 0);
    assert_eq!(vacuum.removed_pending_turn_input_tombstone_count, 2);
    assert!(matches!(
        store
            .cancel_pending_turn_input("root", &second.input_id)
            .await
            .expect("cancel pruned tombstone"),
        crate::PendingTurnInputCancelOutcome::NotFound
    ));
    assert_eq!(
        store
            .list_pending_turn_inputs("other")
            .await
            .expect("list other session after tombstone vacuum")
            .len(),
        1,
        "vacuum must prune terminal evidence without removing live pending input"
    );
}

async fn pending_turn_input_bulk_and_suffix_cancellation(store: Arc<dyn RuntimePersistence>) {
    let first = store
        .enqueue_pending_turn_input(
            pending_next_turn_input_draft("root", "bulk first").with_source_key("bulk:first"),
        )
        .await
        .expect("enqueue first bulk input");
    let second = store
        .enqueue_pending_turn_input(
            pending_next_turn_input_draft("root", "bulk second").with_source_key("bulk:second"),
        )
        .await
        .expect("enqueue second bulk input");
    let third = store
        .enqueue_pending_turn_input(pending_next_turn_input_draft("root", "bulk third"))
        .await
        .expect("enqueue third bulk input");
    let bulk = store
        .cancel_pending_turn_inputs(
            "root",
            &[
                crate::PendingTurnInputCancelTarget::source_key("bulk:first"),
                crate::PendingTurnInputCancelTarget::input_id(&third.input_id),
                crate::PendingTurnInputCancelTarget::source_key("bulk:missing"),
                crate::PendingTurnInputCancelTarget::source_key("bulk:first"),
            ],
        )
        .await
        .expect("bulk cancel pending turn inputs");
    assert_eq!(bulk.len(), 4);
    expect_cancelled_pending_input(bulk[0].outcome.clone(), &first.input_id);
    expect_cancelled_pending_input(bulk[1].outcome.clone(), &third.input_id);
    assert!(matches!(
        bulk[2].outcome,
        crate::PendingTurnInputCancelOutcome::NotFound
    ));
    assert!(matches!(
        &bulk[3].outcome,
        crate::PendingTurnInputCancelOutcome::AlreadyCancelled(input)
            if input.input_id == first.input_id
    ));
    assert_eq!(
        store
            .list_pending_turn_inputs("root")
            .await
            .expect("list after bulk cancellation")
            .iter()
            .map(|input| input.input_id.as_str())
            .collect::<Vec<_>>(),
        vec![second.input_id.as_str()]
    );

    let suffix_anchor = store
        .enqueue_pending_turn_input(
            pending_next_turn_input_draft("root", "suffix anchor").with_source_key("suffix:anchor"),
        )
        .await
        .expect("enqueue suffix anchor");
    let active_claimed = store
        .enqueue_pending_turn_input(
            pending_active_turn_input_draft(
                "root",
                "suffix-active-turn",
                crate::TurnInputCheckpointBoundary::AfterWork,
                "suffix accepted active",
            )
            .with_source_key("suffix:claimed"),
        )
        .await
        .expect("enqueue suffix claimed input");
    let suffix_later = store
        .enqueue_pending_turn_input(
            pending_next_turn_input_draft("root", "suffix later").with_source_key("suffix:later"),
        )
        .await
        .expect("enqueue suffix later");
    let lease = claim_session_execution_lease_for_test(&store, "root", "suffix-cancel-owner").await;
    let active_claim = store
        .claim_active_turn_inputs(
            "root",
            &lease.fence(),
            &lease_owner("suffix-cancel-owner"),
            &crate::TurnId::from("suffix-active-turn"),
            crate::CheckpointKind::AfterWork,
            10,
        )
        .await
        .expect("claim suffix active input")
        .expect("suffix active input claim");

    let suffix = store
        .cancel_pending_turn_input_suffix(
            "root",
            &crate::PendingTurnInputCancelTarget::source_key("suffix:anchor"),
        )
        .await
        .expect("suffix cancel by source key");
    let crate::PendingTurnInputSuffixCancelOutcome::Outcomes { outcomes, .. } = suffix else {
        panic!("expected suffix outcomes, got {suffix:?}");
    };
    assert_eq!(outcomes.len(), 3);
    expect_cancelled_pending_input(outcomes[0].clone(), &suffix_anchor.input_id);
    match &outcomes[1] {
        crate::PendingTurnInputCancelOutcome::AlreadyClaimed { input, claim } => {
            assert_eq!(input.input_id, active_claimed.input_id);
            assert_eq!(
                claim.as_ref().and_then(|claim| claim.claim_id.as_deref()),
                Some(active_claim.claim_id.as_str())
            );
        }
        other => panic!("expected already-claimed suffix outcome, got {other:?}"),
    }
    expect_cancelled_pending_input(outcomes[2].clone(), &suffix_later.input_id);

    let suffix_by_id_anchor = store
        .enqueue_pending_turn_input(pending_next_turn_input_draft("root", "suffix by id anchor"))
        .await
        .expect("enqueue suffix by id anchor");
    let suffix_by_id_later = store
        .enqueue_pending_turn_input(
            pending_next_turn_input_draft("root", "suffix by id later")
                .with_source_key("suffix:id-later"),
        )
        .await
        .expect("enqueue suffix by id later");
    let suffix_by_id = store
        .cancel_pending_turn_input_suffix(
            "root",
            &crate::PendingTurnInputCancelTarget::input_id(&suffix_by_id_anchor.input_id),
        )
        .await
        .expect("suffix cancel by input id");
    let crate::PendingTurnInputSuffixCancelOutcome::Outcomes { outcomes, .. } = suffix_by_id else {
        panic!("expected input-id suffix outcomes, got {suffix_by_id:?}");
    };
    assert_eq!(outcomes.len(), 2);
    expect_cancelled_pending_input(outcomes[0].clone(), &suffix_by_id_anchor.input_id);
    expect_cancelled_pending_input(outcomes[1].clone(), &suffix_by_id_later.input_id);

    assert!(matches!(
        store
            .cancel_pending_turn_input_suffix(
                "root",
                &crate::PendingTurnInputCancelTarget::source_key("suffix:missing"),
            )
            .await
            .expect("missing suffix anchor"),
        crate::PendingTurnInputSuffixCancelOutcome::AnchorNotFound { .. }
    ));
    assert_eq!(
        store
            .list_pending_turn_inputs("root")
            .await
            .expect("list after suffix cancellation")
            .iter()
            .map(|input| input.input_id.as_str())
            .collect::<Vec<_>>(),
        vec![second.input_id.as_str()]
    );
}

async fn pending_turn_input_claims_reclaim_complete_and_fence(store: Arc<dyn RuntimePersistence>) {
    let first = store
        .enqueue_pending_turn_input(
            crate::PendingTurnInputDraft::new(
                "root",
                crate::TurnInputIngress::NextTurn,
                crate::TurnInput::text("first next").with_attachment(inline_png(vec![1, 2, 3])),
            )
            .with_source_key("next:first"),
        )
        .await
        .expect("enqueue first next input");
    let second = store
        .enqueue_pending_turn_input(pending_next_turn_input_draft("root", "second next"))
        .await
        .expect("enqueue second next input");
    let lease = claim_session_execution_lease_for_test(&store, "root", "turn-input-owner").await;
    let claim = store
        .claim_next_turn_inputs("root", &lease.fence(), &lease_owner("turn-input-owner"), 10)
        .await
        .expect("claim next inputs")
        .expect("next input claim");
    assert_eq!(
        claim
            .inputs
            .iter()
            .map(|input| input.input_id.as_str())
            .collect::<Vec<_>>(),
        vec![first.input_id.as_str(), second.input_id.as_str()]
    );
    assert!(matches!(
        claim
            .materialize_turn_input()
            .items
            .iter()
            .find(|item| matches!(item, crate::InputItem::Attachment { .. })),
        Some(crate::InputItem::Attachment {
            source: crate::AttachmentSource::Inline { bytes, .. }
        }) if bytes == &[1, 2, 3]
    ));
    match store
        .cancel_pending_turn_input("root", &first.input_id)
        .await
        .expect("cancel claimed input")
    {
        crate::PendingTurnInputCancelOutcome::AlreadyClaimed {
            input,
            claim: diagnostics,
        } => {
            assert_eq!(input.input_id, first.input_id);
            assert_eq!(
                diagnostics
                    .as_ref()
                    .and_then(|diagnostics| diagnostics.claim_id.as_deref()),
                Some(claim.claim_id.as_str())
            );
        }
        other => panic!("live claimed pending input must not be cancellable, got {other:?}"),
    }
    assert!(
        store
            .list_pending_turn_inputs("root")
            .await
            .expect("list claimed inputs")
            .is_empty(),
        "live claimed pending inputs must be hidden from queue previews"
    );

    store
        .abandon_turn_input_claim(&claim)
        .await
        .expect("abandon pending input claim");
    assert_eq!(
        store
            .list_pending_turn_inputs("root")
            .await
            .expect("list after abandon")
            .len(),
        2
    );
    let reclaimed = store
        .claim_next_turn_inputs("root", &lease.fence(), &lease_owner("turn-input-owner"), 10)
        .await
        .expect("reclaim next inputs")
        .expect("reclaimed next claim");
    assert!(
        reclaimed.fencing_token > claim.fencing_token,
        "reclaiming abandoned pending inputs must advance the fencing token"
    );

    let state = RuntimeSessionState {
        session_id: "root".to_string(),
        ..RuntimeSessionState::default()
    };
    let err = store
        .commit_runtime_state(
            RuntimeCommit::persisted_state_for_test(&state, &[])
                .completing_turn_input_claim(claim.completion()),
        )
        .await
        .expect_err("stale turn-input completion must fail");
    assert!(matches!(err, StoreError::TurnInputClaimSuperseded { .. }));
    assert!(
        store
            .list_pending_turn_inputs("root")
            .await
            .expect("list reclaimed live inputs")
            .is_empty(),
        "stale completion must not abandon the live reclaimed claim"
    );

    store
        .commit_runtime_state(
            RuntimeCommit::persisted_state_for_test(&state, &[])
                .releasing_session_execution_lease(lease.completion())
                .completing_turn_input_claim(reclaimed.completion()),
        )
        .await
        .expect("valid pending input completion commits");
    assert!(
        store
            .list_pending_turn_inputs("root")
            .await
            .expect("list after valid completion")
            .is_empty()
    );
    assert!(matches!(
        store
            .cancel_pending_turn_input("root", &first.input_id)
            .await
            .expect("cancel completed input"),
        crate::PendingTurnInputCancelOutcome::AlreadyCompleted(input)
            if input.input_id == first.input_id
    ));
    let completed_replay = store
        .enqueue_pending_turn_input(
            crate::PendingTurnInputDraft::new(
                "root",
                crate::TurnInputIngress::NextTurn,
                crate::TurnInput::text("first next").with_attachment(inline_png(vec![1, 2, 3])),
            )
            .with_source_key("next:first"),
        )
        .await
        .expect("exact replay after completion");
    assert_eq!(completed_replay.input_id, first.input_id);
    assert_eq!(completed_replay.state, crate::TurnInputState::Completed);
    let post_completion_lease =
        claim_session_execution_lease_for_test(&store, "root", "post-completion-owner").await;
    assert!(
        store
            .claim_next_turn_inputs(
                "root",
                &post_completion_lease.fence(),
                &lease_owner("post-completion-owner"),
                10,
            )
            .await
            .expect("claim after completing inputs")
            .is_none(),
        "completed pending input tombstones must not be claimable"
    );
}

pub async fn turn_input_claims_supersede_across_session_lease_generations(
    store: Arc<dyn RuntimePersistence>,
    lease_timing: RuntimePersistenceLeaseTiming,
) {
    turn_input_claims_supersede_across_session_lease_generations_with_timing(store, &lease_timing)
        .await;
}

async fn turn_input_claims_supersede_across_session_lease_generations_with_timing(
    store: Arc<dyn RuntimePersistence>,
    lease_timing: &RuntimePersistenceLeaseTiming,
) {
    // The DeferredNextTurn idle-retry shape: a failed turn releases its lease
    // and the next idle acquisition re-claims the same next-turn input under a
    // fresh generation, while the stale claim's completion is rejected. This was
    // the latent unrenewed-claim bug (ADR 0029).
    let input = store
        .enqueue_pending_turn_input(pending_next_turn_input_draft(
            "root",
            "generation next input",
        ))
        .await
        .expect("enqueue next-turn input");

    // (a) Same generation: a live next-turn claim is not re-claimable.
    let lease_a = claim_session_execution_lease_for_test(&store, "root", "tin-owner-a").await;
    let claim_a = store
        .claim_next_turn_inputs("root", &lease_a.fence(), &lease_owner("tin-owner-a"), 10)
        .await
        .expect("first next-turn claim")
        .expect("first next-turn claim exists");
    assert_eq!(claim_a.inputs[0].input_id, input.input_id);
    assert_eq!(claim_a.session_lease_generation, lease_a.fencing_token);
    assert!(
        store
            .claim_next_turn_inputs("root", &lease_a.fence(), &lease_owner("tin-owner-a"), 10)
            .await
            .expect("same-generation re-claim")
            .is_none(),
        "a live next-turn claim must not be re-claimable under its own generation"
    );

    // (b) Idle retry after lease release + re-acquire: the same next-turn input
    // is re-claimable by the new generation and the stale completion is
    // superseded.
    release_session_execution_lease_for_test(&store, &lease_a).await;
    let lease_b = claim_session_execution_lease_for_test(&store, "root", "tin-owner-b").await;
    let claim_b = store
        .claim_next_turn_inputs("root", &lease_b.fence(), &lease_owner("tin-owner-b"), 10)
        .await
        .expect("idle-retry next-turn claim")
        .expect("idle-retry next-turn claim exists");
    assert_eq!(claim_b.inputs[0].input_id, input.input_id);
    assert!(claim_b.fencing_token > claim_a.fencing_token);

    let stale_state = RuntimeSessionState {
        session_id: "root".to_string(),
        ..RuntimeSessionState::default()
    };
    let stale_err = store
        .commit_runtime_state(
            RuntimeCommit::persisted_state_for_test(&stale_state, &[])
                .completing_turn_input_claim(claim_a.completion()),
        )
        .await
        .expect_err("superseded next-turn completion must fail");
    assert!(matches!(
        stale_err,
        StoreError::TurnInputClaimSuperseded { .. }
    ));
    release_session_execution_lease_for_test(&store, &lease_b).await;

    // (c) TTL takeover mints a new generation without a release.
    let dead_owner = lease_owner("tin-stale");
    let dead_lease = store
        .try_claim_session_execution_lease(
            "root",
            &dead_owner,
            lease_timing.scaffolding_lease_ttl_ms(),
        )
        .await
        .expect("claim dead-owner lease")
        .acquired()
        .expect("dead-owner lease acquired");
    let claim_dead = store
        .claim_next_turn_inputs("root", &dead_lease.fence(), &dead_owner, 10)
        .await
        .expect("dead-owner next-turn claim")
        .expect("dead-owner next-turn claim exists");
    let taker = lease_owner("tin-taker");
    let taker_lease = claim_session_execution_lease_after_expiry(
        &store,
        "root",
        &taker,
        lease_timing,
        "stale turn-input owner TTL",
    )
    .await;
    let claim_taker = store
        .claim_next_turn_inputs("root", &taker_lease.fence(), &taker, 10)
        .await
        .expect("post-takeover next-turn claim")
        .expect("post-takeover next-turn claim exists");
    assert_eq!(claim_taker.inputs[0].input_id, input.input_id);
    let takeover_err = store
        .commit_runtime_state(
            RuntimeCommit::persisted_state_for_test(&stale_state, &[])
                .completing_turn_input_claim(claim_dead.completion()),
        )
        .await
        .expect_err("pre-takeover next-turn completion must fail");
    assert!(matches!(
        takeover_err,
        StoreError::TurnInputClaimSuperseded { .. }
    ));
}

/// A checkpoint executor can durably move an active input to `accepted` and
/// crash before its effect outcome journals the claim. The successor must be
/// able to reacquire that same turn/input pair under its newer generation.
pub async fn active_turn_input_claim_reacquires_after_unrecorded_checkpoint(
    store: Arc<dyn RuntimePersistence>,
) {
    const SESSION_ID: &str = "fig905-active-reacquire";
    const TURN_ID: &str = "fig905-active-reacquire:turn";
    let input = store
        .enqueue_pending_turn_input(pending_active_turn_input_draft(
            SESSION_ID,
            &crate::TurnId::from(TURN_ID),
            crate::TurnInputCheckpointBoundary::AfterWork,
            "accepted before checkpoint outcome",
        ))
        .await
        .expect("enqueue active input");

    let predecessor =
        claim_session_execution_lease_for_test(&store, SESSION_ID, "fig905-active-predecessor")
            .await;
    let predecessor_claim = store
        .claim_active_turn_inputs(
            SESSION_ID,
            &predecessor.fence(),
            &lease_owner("fig905-active-predecessor"),
            &crate::TurnId::from(TURN_ID),
            crate::CheckpointKind::AfterWork,
            10,
        )
        .await
        .expect("claim active input before simulated crash")
        .expect("active input claim exists");
    assert_eq!(
        predecessor_claim.inputs[0].state,
        crate::TurnInputState::Accepted
    );
    release_session_execution_lease_for_test(&store, &predecessor).await;

    let successor =
        claim_session_execution_lease_for_test(&store, SESSION_ID, "fig905-active-successor").await;
    let (successor_claim, queued_claim) = store
        .claim_checkpoint_work(
            SESSION_ID,
            &successor.fence(),
            &lease_owner("fig905-active-successor"),
            &crate::TurnId::from(TURN_ID),
            crate::CheckpointKind::AfterWork,
            10,
            10,
        )
        .await
        .expect("reacquire accepted input after unrecorded checkpoint");
    let successor_claim = successor_claim.expect("successor reacquires accepted input");
    assert!(
        queued_claim.is_none(),
        "accepted-only checkpoint fixture must not rely on queued work to open the claim path"
    );
    assert_eq!(successor_claim.inputs[0].input_id, input.input_id);
    assert!(successor_claim.session_lease_generation > predecessor_claim.session_lease_generation);
    assert!(successor_claim.fencing_token > predecessor_claim.fencing_token);

    let stale_state = RuntimeSessionState {
        session_id: SESSION_ID.to_string(),
        ..RuntimeSessionState::default()
    };
    let stale_error = store
        .commit_runtime_state(
            RuntimeCommit::persisted_state_for_test(&stale_state, &[])
                .completing_turn_input_claim(predecessor_claim.completion()),
        )
        .await
        .expect_err("reacquisition supersedes the unjournaled predecessor claim");
    assert!(matches!(
        stale_error,
        StoreError::TurnInputClaimSuperseded { .. }
    ));

    store
        .commit_runtime_state(
            RuntimeCommit::persisted_state_for_test(&stale_state, &[])
                .releasing_session_execution_lease(successor.completion())
                .completing_turn_input_claim(successor_claim.completion()),
        )
        .await
        .expect("successor settles reacquired active input");
}

async fn pending_turn_input_cancel_covers_active_and_deferred_states(
    store: Arc<dyn RuntimePersistence>,
) {
    let turn_id = "cancel-active-turn";
    let active_keep = store
        .enqueue_pending_turn_input(pending_active_turn_input_draft(
            "root",
            turn_id,
            crate::TurnInputCheckpointBoundary::AfterWork,
            "active that defers",
        ))
        .await
        .expect("enqueue active input to defer");
    let active_cancel = store
        .enqueue_pending_turn_input(pending_active_turn_input_draft(
            "root",
            turn_id,
            crate::TurnInputCheckpointBoundary::AfterWork,
            "active cancelled before interrupt",
        ))
        .await
        .expect("enqueue active input to cancel");
    let next_cancel = store
        .enqueue_pending_turn_input(pending_next_turn_input_draft(
            "root",
            "next cancelled before claim",
        ))
        .await
        .expect("enqueue next input to cancel");

    let cancelled_active = store
        .cancel_pending_turn_input("root", &active_cancel.input_id)
        .await
        .expect("cancel active input");
    let cancelled_active =
        expect_cancelled_pending_input(cancelled_active, &active_cancel.input_id);
    assert!(matches!(
        cancelled_active.ingress,
        crate::TurnInputIngress::ActiveTurn { .. }
    ));
    let cancelled_next = store
        .cancel_pending_turn_input("root", &next_cancel.input_id)
        .await
        .expect("cancel next input");
    expect_cancelled_pending_input(cancelled_next, &next_cancel.input_id);

    let lease = claim_session_execution_lease_for_test(&store, "root", "cancel-input-owner").await;
    let state = RuntimeSessionState {
        session_id: "root".to_string(),
        ..RuntimeSessionState::default()
    };
    store
        .commit_runtime_state(
            RuntimeCommit::persisted_state_for_test(&state, &[])
                .deferring_interrupted_turn_inputs(turn_id),
        )
        .await
        .expect("interrupt commit defers uncancelled active input");

    let pending_after_interrupt = store
        .list_pending_turn_inputs("root")
        .await
        .expect("list after interrupt");
    assert_eq!(
        pending_after_interrupt
            .iter()
            .map(|input| input.input_id.as_str())
            .collect::<Vec<_>>(),
        vec![active_keep.input_id.as_str()],
        "cancelled active and next-turn inputs must not be resurrected by interrupt deferral"
    );
    assert!(matches!(
        pending_after_interrupt[0].ingress,
        crate::TurnInputIngress::NextTurn
    ));
    assert_eq!(
        pending_after_interrupt[0].state,
        crate::TurnInputState::DeferredNextTurn
    );

    let cancelled_deferred = store
        .cancel_pending_turn_input("root", &active_keep.input_id)
        .await
        .expect("cancel deferred input");
    expect_cancelled_pending_input(cancelled_deferred, &active_keep.input_id);
    assert!(
        store
            .claim_next_turn_inputs(
                "root",
                &lease.fence(),
                &lease_owner("cancel-input-owner"),
                10,
            )
            .await
            .expect("claim after cancelling deferred input")
            .is_none(),
        "cancelled deferred input must not be claimable"
    );
}

async fn pending_active_turn_inputs_defer_unaccepted_once_on_interrupt(
    store: Arc<dyn RuntimePersistence>,
) {
    let turn_id = "active-turn-1";
    let accepted = store
        .enqueue_pending_turn_input(
            crate::PendingTurnInputDraft::new(
                "root",
                crate::TurnInputIngress::active_turn(
                    turn_id,
                    crate::TurnInputCheckpointBoundary::AfterWork,
                ),
                crate::TurnInput::text("accepted active")
                    .with_attachment(inline_png(vec![9, 8, 7])),
            )
            .with_source_key("active:accepted"),
        )
        .await
        .expect("enqueue accepted active input");
    let unaccepted = store
        .enqueue_pending_turn_input(pending_active_turn_input_draft(
            "root",
            turn_id,
            crate::TurnInputCheckpointBoundary::AfterWork,
            "unaccepted active",
        ))
        .await
        .expect("enqueue unaccepted active input");
    let before_completion = store
        .enqueue_pending_turn_input(pending_active_turn_input_draft(
            "root",
            turn_id,
            crate::TurnInputCheckpointBoundary::BeforeCompletion,
            "before-completion active",
        ))
        .await
        .expect("enqueue before-completion active input");
    let other_active = store
        .enqueue_pending_turn_input(pending_active_turn_input_draft(
            "root",
            "other-turn",
            crate::TurnInputCheckpointBoundary::AfterWork,
            "other active",
        ))
        .await
        .expect("enqueue other active input");

    let lease = claim_session_execution_lease_for_test(&store, "root", "active-input-owner").await;
    let claim_turn_id = crate::TurnId::from(turn_id);
    let claim = store
        .claim_active_turn_inputs(
            "root",
            &lease.fence(),
            &lease_owner("active-input-owner"),
            &claim_turn_id,
            crate::CheckpointKind::AfterWork,
            1,
        )
        .await
        .expect("claim active inputs")
        .expect("active input claim");
    assert_eq!(
        claim
            .inputs
            .iter()
            .map(|input| input.input_id.as_str())
            .collect::<Vec<_>>(),
        vec![accepted.input_id.as_str()],
        "AfterWork claims must include matching active inputs admitted at that boundary in order"
    );
    assert!(matches!(
        claim.materialize_turn_input().items.last(),
        Some(crate::InputItem::Attachment {
            source: crate::AttachmentSource::Inline { bytes, .. }
        }) if bytes == &[9, 8, 7]
    ));

    let state = RuntimeSessionState {
        session_id: "root".to_string(),
        ..RuntimeSessionState::default()
    };
    let interrupt_result = store
        .commit_runtime_state(
            RuntimeCommit::persisted_state_for_test(&state, &[])
                .completing_turn_input_claim(claim.completion())
                .deferring_interrupted_turn_inputs(turn_id),
        )
        .await
        .expect("interrupt commit completes accepted inputs and defers unaccepted inputs");
    let mut state = state;
    state.head_revision = interrupt_result.head_revision;
    let pending_after_interrupt = store
        .list_pending_turn_inputs("root")
        .await
        .expect("list after interrupt deferral");
    assert_eq!(
        pending_after_interrupt
            .iter()
            .map(|input| input.input_id.as_str())
            .collect::<Vec<_>>(),
        vec![
            unaccepted.input_id.as_str(),
            before_completion.input_id.as_str(),
            other_active.input_id.as_str(),
        ],
        "interrupt must complete accepted input, defer matching unaccepted inputs, and retain other-turn active input"
    );
    let deferred_after_interrupt = pending_after_interrupt
        .iter()
        .filter(|input| input.ingress.active_turn_id().is_none())
        .collect::<Vec<_>>();
    assert_eq!(
        deferred_after_interrupt
            .iter()
            .map(|input| input.input_id.as_str())
            .collect::<Vec<_>>(),
        vec![
            unaccepted.input_id.as_str(),
            before_completion.input_id.as_str()
        ],
        "accepted active inputs must be completed and only unaccepted matching active inputs become next-turn work"
    );
    assert!(deferred_after_interrupt.iter().all(|input| {
        matches!(input.ingress, crate::TurnInputIngress::NextTurn)
            && input.state == crate::TurnInputState::DeferredNextTurn
    }));
    assert!(
        pending_after_interrupt
            .iter()
            .any(|input| input.ingress.active_turn_id() == Some("other-turn")),
        "inputs for other active turns must not be deferred by this interrupt"
    );

    let next_claim = store
        .claim_next_turn_inputs(
            "root",
            &lease.fence(),
            &lease_owner("active-input-owner"),
            10,
        )
        .await
        .expect("claim deferred next inputs")
        .expect("deferred next input claim");
    assert_eq!(
        next_claim
            .inputs
            .iter()
            .map(|input| input.input_id.as_str())
            .collect::<Vec<_>>(),
        vec![
            unaccepted.input_id.as_str(),
            before_completion.input_id.as_str()
        ]
    );
    store
        .commit_runtime_state(
            RuntimeCommit::persisted_state_for_test(&state, &[])
                .releasing_session_execution_lease(lease.completion())
                .completing_turn_input_claim(next_claim.completion()),
        )
        .await
        .expect("complete deferred next input");
    assert!(
        store
            .list_pending_turn_inputs("root")
            .await
            .expect("list after completing deferred input")
            .iter()
            .all(|input| input.ingress.active_turn_id() == Some("other-turn")),
        "inputs for other active turns must not be deferred by this interrupt"
    );
}

async fn session_metadata_round_trips(store: Arc<dyn RuntimePersistence>) {
    let meta = SessionMeta {
        session_id: "root".to_string(),
        session_name: "Conformance Root".to_string(),
        created_at: "2026-06-02T00:00:00Z".to_string(),
        model: "gpt-5.4-mini".to_string(),
        cwd: Some("/tmp/lash-conformance".to_string()),
        relation: SessionRelation::Root,
    };
    store
        .save_session_meta(meta.clone())
        .await
        .expect("save session meta");
    let loaded = store
        .load_session_meta()
        .await
        .expect("load session meta")
        .expect("session meta present");
    assert_eq!(loaded.session_id, meta.session_id);
    assert_eq!(loaded.session_name, meta.session_name);
    assert_eq!(loaded.created_at, meta.created_at);
    assert_eq!(loaded.model, meta.model);
    assert_eq!(loaded.cwd, meta.cwd);
    assert_eq!(loaded.relation, meta.relation);
}

/// Blob-backed backends must physically reclaim the checkpoint blob a superseding
/// commit orphaned, while preserving the live one. Generalizes the SQLite-only
/// `gc_unreachable_keeps_rooted_checkpoint_blobs` test to every reclaiming
/// backend via the [`GcReport`](crate::GcReport) counters plus a post-GC load.
async fn gc_reclaims_unreachable_checkpoint_blobs_and_preserves_live(
    store: Arc<dyn RuntimePersistence>,
) {
    // First commit writes a live checkpoint blob.
    let v1 = RuntimeSessionState {
        session_id: "gc-blobs".to_string(),
        tool_state_snapshot: Some(ToolState::default().with_generation(1)),
        ..RuntimeSessionState::default()
    };
    let v1_result = commit_runtime_state_for_test(
        &store,
        RuntimeCommit::persisted_state_for_test(&v1, &[]),
        "gc-blobs-v1",
    )
    .await
    .expect("commit v1");
    // Second commit supersedes it with different content, so the v1 checkpoint
    // blob is now unreachable from every session head.
    let v2 = RuntimeSessionState {
        session_id: "gc-blobs".to_string(),
        tool_state_snapshot: Some(ToolState::default().with_generation(2)),
        head_revision: v1_result.head_revision,
        ..RuntimeSessionState::default()
    };
    commit_runtime_state_for_test(
        &store,
        RuntimeCommit::persisted_state_for_test(&v2, &[]),
        "gc-blobs-v2",
    )
    .await
    .expect("commit v2");

    let report = store
        .gc_unreachable()
        .await
        .expect("gc reclaims unreachable checkpoint blobs");
    assert!(
        report.root_count >= 1,
        "a live checkpoint must be rooted, got {report:?}"
    );
    assert!(
        report.retained_blob_count >= 1,
        "the live checkpoint blob must be retained, got {report:?}"
    );
    assert!(
        report.deleted_blob_count >= 1,
        "the superseded checkpoint blob must be reclaimed, got {report:?}"
    );

    // The reachable checkpoint survived: the session still loads at generation 2.
    let read = store
        .load_session()
        .await
        .expect("load after gc")
        .expect("session after gc");
    assert_eq!(
        read.checkpoint
            .and_then(|checkpoint| checkpoint.tool_state)
            .map(|tool_state| tool_state.generation()),
        Some(2),
        "gc must preserve the reachable checkpoint's snapshots"
    );

    // Idempotent: with nothing newly unreachable, a second sweep deletes nothing.
    let second = store.gc_unreachable().await.expect("second gc");
    assert_eq!(
        second.deleted_blob_count, 0,
        "gc must never reclaim reachable blobs, got {second:?}"
    );
}

/// The durable manifest is the reference layer: it answers `holds_ref` for the
/// session-boundary guard, exposes every live ref (intent or committed) for the
/// GC root set, and drops refs on `forget`. Both intents and commits count as
/// live refs; a forgotten ref disappears from both queries.
async fn attachment_manifest_reference_tracking_and_gc_root_set(
    store: Arc<dyn RuntimePersistence>,
) {
    let intent_id = AttachmentId::new(format!("{:x}", sha256_of(b"intent-only")));
    let committed_id = AttachmentId::new(format!("{:x}", sha256_of(b"committed")));
    let intent = |id: &AttachmentId, at: u64| AttachmentIntent {
        attachment_id: id.clone(),
        session_id: "root".to_string(),
        canonical_uri: format!("lash-attachment://sha256/{id}"),
        intent_at_epoch_ms: at,
        owner_kind: None,
        owner_id: None,
    };
    store
        .record_intent(intent(&intent_id, 100))
        .expect("record intent-only");
    store
        .record_intent(intent(&committed_id, 100))
        .expect("record committed intent");
    store
        .commit_refs("root", std::slice::from_ref(&committed_id))
        .expect("commit attachment ref");

    // Boundary guard: both an intent and a commit are live refs for their
    // session; another session (or an unknown id) holds no ref.
    assert!(
        store.holds_ref("root", &intent_id).expect("holds intent"),
        "an uncommitted intent is a live ref"
    );
    assert!(
        store
            .holds_ref("root", &committed_id)
            .expect("holds commit"),
        "a committed attachment is a live ref"
    );
    assert!(
        !store
            .holds_ref("other-session", &committed_id)
            .expect("no cross-session ref"),
        "a ref belongs only to its own session"
    );
    assert!(
        !store
            .holds_ref("root", &AttachmentId::new("sha256:never-referenced"))
            .expect("no ref for unknown id"),
        "an id never referenced holds no ref"
    );

    // Root set: every live ref, intent or committed.
    let refs = store.list_all_refs().expect("list all refs");
    assert!(refs.contains(&intent_id), "intents feed the GC root set");
    assert!(refs.contains(&committed_id), "commits feed the GC root set");

    // Uncommitted listing still distinguishes intents from commits.
    let uncommitted = store.list_uncommitted(1_000_000).expect("list uncommitted");
    assert!(
        uncommitted
            .iter()
            .any(|entry| entry.attachment_id == intent_id),
        "an uncommitted intent is listed as uncommitted"
    );
    assert!(
        !uncommitted
            .iter()
            .any(|entry| entry.attachment_id == committed_id),
        "a committed attachment is not listed as uncommitted"
    );

    // Forget drops the ref from both the boundary guard and the root set.
    store.forget("root", &intent_id).expect("forget intent ref");
    assert!(
        !store.holds_ref("root", &intent_id).expect("ref dropped"),
        "a forgotten ref is no longer held"
    );
    assert!(
        !store
            .list_all_refs()
            .expect("list after forget")
            .contains(&intent_id),
        "a forgotten ref leaves the root set"
    );
}

fn sha256_of(bytes: &[u8]) -> impl std::fmt::LowerHex {
    use sha2::{Digest, Sha256};
    Sha256::digest(bytes)
}

async fn append_receipt_survives_reopen(factory: ReopenableRuntimePersistence) {
    let mut state = RuntimeSessionState {
        session_id: "root".to_string(),
        ..RuntimeSessionState::default()
    };
    let nodes = vec![crate::SessionAppendNode::plugin(
        "append-receipt-reopen",
        serde_json::json!({"value": "reopen"}),
    )];
    let (first_commit, _) =
        append_request_commit(&mut state, "append-receipt-reopen", &nodes, None);
    let first =
        commit_runtime_state_for_test(&factory.open, first_commit, "append-receipt-reopen-first")
            .await
            .expect("commit append receipt before reopen");

    let mut reopened_state = loaded_conformance_state(&factory.reopen).await;
    let (retry_commit, _) =
        append_request_commit(&mut reopened_state, "append-receipt-reopen", &nodes, None);
    let replay = factory
        .reopen
        .commit_runtime_state(retry_commit)
        .await
        .expect("reopened store replays append receipt");
    assert!(replay.receipt_replayed);
    assert_eq!(replay.head_revision, first.head_revision);
    assert_eq!(replay.checkpoint_ref, first.checkpoint_ref);
    assert_eq!(replay.committed_leaf_node_id, first.committed_leaf_node_id);
    assert_eq!(
        replay.realized_node_timestamps,
        first.realized_node_timestamps
    );
}

async fn runtime_persistence_survives_reopen(factory: ReopenableRuntimePersistence) {
    session_execution_lease_first_claim_excludes_concurrent_reopen_handles(&factory).await;

    let meta = SessionMeta {
        session_id: "root".to_string(),
        session_name: "Durable Root".to_string(),
        created_at: "2026-06-02T00:00:00Z".to_string(),
        model: "gpt-5.4-mini".to_string(),
        cwd: Some("/tmp/lash-reopen".to_string()),
        relation: SessionRelation::Root,
    };
    factory
        .open
        .save_session_meta(meta.clone())
        .await
        .expect("save meta");
    let mut state = RuntimeSessionState {
        session_id: "root".to_string(),
        tool_state_snapshot: Some(ToolState::default().with_generation(77)),
        ..RuntimeSessionState::default()
    };
    let initial_commit = commit_runtime_state_for_test(
        &factory.open,
        RuntimeCommit::persisted_state_for_test(&state, &[]),
        "reopen",
    )
    .await
    .expect("commit state");
    state.head_revision = initial_commit.head_revision;

    let application_lease =
        claim_session_execution_lease_for_test(&factory.open, "root", "reopen-applications").await;
    let mut expected_applications = Vec::new();
    for (turn_index, turn_id) in ["z-reopen-application", "a-reopen-application"]
        .into_iter()
        .enumerate()
    {
        factory
            .open
            .enqueue_pending_turn_input(
                pending_next_turn_input_draft("root", &format!("reopen application {turn_index}"))
                    .with_source_key(format!("host:reopen-application-{turn_index}")),
            )
            .await
            .expect("enqueue reopen application");
        let mut claim = factory
            .open
            .claim_next_turn_inputs(
                "root",
                &application_lease.fence(),
                &lease_owner("reopen-applications"),
                1,
            )
            .await
            .expect("claim reopen application")
            .expect("reopen application claim");
        claim.record_initial_turn_application(
            &crate::TurnId::from(turn_id),
            &format!("reopen-application-message-{turn_index}"),
        );
        expected_applications.extend(claim.applications.clone());

        let mut commit = RuntimeCommit::persisted_state_for_test(&state, &[])
            .completing_turn_input_claim(claim.completion());
        if turn_index == 1 {
            commit = commit.releasing_session_execution_lease(application_lease.completion());
        }
        commit.turn_commit =
            RuntimeTurnCommitStamp::new(crate::OperationId::turn("root", turn_id, "final"));
        let result = factory
            .open
            .commit_runtime_state(commit)
            .await
            .expect("commit reopen application");
        state.head_revision = result.head_revision;
    }
    let queued = factory
        .open
        .enqueue_queued_work(
            queued_draft(
                "root",
                "survives reopen",
                DeliveryPolicy::EarliestSafeBoundary,
                SlotPolicy::Exclusive,
            )
            .with_source_key("reopen:queued"),
        )
        .await
        .expect("enqueue queued work");
    let attachment = AttachmentId::new("reopen-attachment".to_string());
    factory
        .open
        .record_intent(AttachmentIntent {
            attachment_id: attachment.clone(),
            session_id: "root".to_string(),
            canonical_uri: "sha256:reopen-attachment".to_string(),
            intent_at_epoch_ms: 100,
            owner_kind: None,
            owner_id: None,
        })
        .expect("record attachment intent");

    let reopened_meta = factory
        .reopen
        .load_session_meta()
        .await
        .expect("load reopened meta")
        .expect("reopened meta");
    assert_eq!(reopened_meta.session_name, meta.session_name);
    let reopened = factory
        .reopen
        .load_session()
        .await
        .expect("load reopened state")
        .expect("reopened state");
    assert_eq!(reopened.session_id, "root");
    assert_eq!(
        reopened
            .checkpoint
            .as_ref()
            .and_then(|checkpoint| checkpoint.tool_state.as_ref())
            .map(|tool_state| tool_state.generation()),
        Some(77)
    );
    assert_eq!(
        factory
            .reopen
            .list_turn_input_applications("root")
            .await
            .expect("list applications from reopened handle"),
        expected_applications,
        "a fresh durable handle must reconcile applications in turn-commit order"
    );
    let reopened_queue = factory
        .reopen
        .list_queued_work("root")
        .await
        .expect("list reopened queue");
    assert_eq!(reopened_queue.len(), 1);
    assert_eq!(reopened_queue[0].batch_id, queued.batch_id);
    assert_eq!(
        queued_batch_text(&reopened_queue[0]),
        Some("survives reopen")
    );
    let reopened_intents = factory
        .reopen
        .list_uncommitted(200)
        .expect("list reopened attachment intents");
    assert!(
        reopened_intents
            .iter()
            .any(|intent| intent.attachment_id == attachment),
        "attachment intent rows must survive reopening a durable store"
    );
}

async fn session_execution_lease_first_claim_excludes_concurrent_reopen_handles(
    factory: &ReopenableRuntimePersistence,
) {
    let barrier = Arc::new(tokio::sync::Barrier::new(3));
    let open = Arc::clone(&factory.open);
    let reopen = Arc::clone(&factory.reopen);
    let open_barrier = Arc::clone(&barrier);
    let reopen_barrier = Arc::clone(&barrier);
    let open_owner = lease_owner("owner-a");
    let reopen_owner = lease_owner("owner-b");

    let open_claim = crate::task::spawn(async move {
        open_barrier.wait().await;
        open.try_claim_session_execution_lease("first-claim-race", &open_owner, 60_000)
            .await
    });
    let reopen_claim = crate::task::spawn(async move {
        reopen_barrier.wait().await;
        reopen
            .try_claim_session_execution_lease("first-claim-race", &reopen_owner, 60_000)
            .await
    });

    barrier.wait().await;
    let open_claim = open_claim
        .await
        .expect("join open first-claim race")
        .expect("open first-claim race");
    let reopen_claim = reopen_claim
        .await
        .expect("join reopen first-claim race")
        .expect("reopen first-claim race");
    let open_lease = open_claim.acquired();
    let reopen_lease = reopen_claim.acquired();
    let claim_count = usize::from(open_lease.is_some()) + usize::from(reopen_lease.is_some());
    assert_eq!(
        claim_count, 1,
        "exactly one concurrent first claim may acquire a session execution lease"
    );
    if let Some(lease) = open_lease.as_ref().or(reopen_lease.as_ref()) {
        factory
            .open
            .release_session_execution_lease(&lease.completion())
            .await
            .expect("release first-claim race winner");
    }
}

async fn queued_wake_delivery_is_source_key_idempotent_and_claimed_once(
    store: Arc<dyn RuntimePersistence>,
) {
    let wake = ProcessWakeDelivery {
        wake_id: "wake-1".to_string(),
        target_session_id: "root".to_string(),
        process_id: "process-1".to_string(),
        sequence: 7,
        event_type: "process.wake".to_string(),
        event_invocation: RuntimeInvocation {
            scope: RuntimeScope::new("root"),
            subject: RuntimeSubject::ProcessEvent {
                process_id: "process-1".to_string(),
                sequence: 7,
                event_type: "process.wake".to_string(),
            },
            caused_by: None,
            replay: None,
        },
        process_caused_by: None,
        input: "wake payload".to_string(),
        created_at_ms: 1,
    };
    let malformed = QueuedWorkBatchDraft::new(
        wake.target_session_id.clone(),
        DeliveryPolicy::EarliestSafeBoundary,
        SlotPolicy::Exclusive,
        vec![QueuedWorkPayload::process_wake(wake.clone())],
    )
    .with_source_key(crate::process_wake_source_key(
        &wake.process_id,
        wake.sequence,
    ));
    store
        .enqueue_queued_work(malformed)
        .await
        .expect_err("process-wake enqueue must require structural producer identity");

    let first = store
        .enqueue_queued_work(crate::process_wake_batch_draft(wake.clone()))
        .await
        .expect("enqueue wake");
    let replay = store
        .enqueue_queued_work(crate::process_wake_batch_draft(wake.clone()))
        .await
        .expect("replay wake enqueue");
    assert_eq!(
        first.batch_id, replay.batch_id,
        "wake source-key replay must return the original queued batch"
    );
    assert_eq!(
        store
            .list_queued_work("root")
            .await
            .expect("list queued wakes")
            .len(),
        1,
        "replayed wake must not create a second queued delivery"
    );

    let session_lease = claim_session_execution_lease_for_test(&store, "root", "wake-owner").await;
    let claim = store
        .claim_ready_queued_work(
            "root",
            &session_lease.fence(),
            &lease_owner("wake-owner"),
            QueuedWorkClaimBoundary::Idle,
            10,
        )
        .await
        .expect("claim wake")
        .expect("wake claim");
    assert_eq!(claim.batches.len(), 1);
    assert_eq!(claim.batches[0].items.len(), 1);
    assert!(matches!(
        claim.batches[0].items[0].payload,
        QueuedWorkPayload::ProcessWake { .. }
    ));
    let state = RuntimeSessionState {
        session_id: "root".to_string(),
        ..RuntimeSessionState::default()
    };
    store
        .commit_runtime_state(
            RuntimeCommit::persisted_state_for_test(&state, &[])
                .releasing_session_execution_lease(session_lease.completion())
                .completing_queue_claim(claim.completion()),
        )
        .await
        .expect("wake delivery completion commits");
    assert!(
        store
            .list_queued_work("root")
            .await
            .expect("list after wake completion")
            .is_empty(),
        "completed wake delivery must be removed exactly once"
    );
    let consumed_replay = store
        .enqueue_queued_work(crate::process_wake_batch_draft(wake))
        .await
        .expect_err("late no-live-row wake must trip the receiver floor");
    assert!(matches!(
        consumed_replay,
        StoreError::ProcessWakeSequenceRewound { .. }
    ));
    assert!(
        store
            .list_queued_work("root")
            .await
            .expect("list after consumed wake redelivery")
            .is_empty(),
        "receiver evidence must prevent a late redelivery from recreating queued work"
    );
}

async fn final_commit_stamp_is_idempotent_and_conflicts_on_changed_hash(
    store: Arc<dyn RuntimePersistence>,
) {
    let mut state = RuntimeSessionState {
        session_id: "root".to_string(),
        ..RuntimeSessionState::default()
    };
    state.ensure_agent_frame_initialized();
    state.session_graph.data_mut().nodes[0].timestamp = "2026-07-26T10:00:00Z".to_string();
    state.execution_state_snapshot = Some(vec![7; 1_024]);
    let operation = crate::OperationId::turn("root", "provider-turn", "final");
    let (stamped_commit, _) = RuntimeCommit::persisted_state_for_test(&state, &[])
        .with_operation(operation.clone())
        .expect("derive and stamp first commit");
    let turn_commit_hash = stamped_commit
        .turn_commit_hash()
        .expect("first commit hash");

    let session_lease =
        claim_session_execution_lease_for_test(&store, "root", "provider-turn").await;
    let first = store
        .commit_runtime_state(
            stamped_commit
                .clone()
                .releasing_session_execution_lease(session_lease.completion()),
        )
        .await
        .expect("first final commit requires a live session execution lease");
    let mut replay_state = state.clone();
    replay_state.session_graph.data_mut().nodes[0].timestamp = "2026-07-26T10:00:09Z".to_string();
    let (replay_commit, _) = RuntimeCommit::persisted_state_for_test(&replay_state, &[])
        .with_operation(operation.clone())
        .expect("derive and stamp replay");
    let replay_hash = replay_commit
        .turn_commit_hash()
        .expect("replay commit hash");
    assert_eq!(replay_hash, turn_commit_hash);
    let retry = store
        .commit_runtime_state(replay_commit)
        .await
        .expect("same final commit retries idempotently without a live lease");
    assert_eq!(retry.head_revision, first.head_revision);
    assert_eq!(retry.checkpoint_ref, first.checkpoint_ref);
    let receipt_json = serde_json::to_string(&first).expect("serialize commit receipt");
    assert!(
        !receipt_json.contains("execution_state_snapshot"),
        "commit receipts must retain frame references and timestamps, never snapshot bytes"
    );
    replay_state.apply_persisted_commit_result(retry.clone());

    let mut retry_from_new_head = RuntimeCommit::persisted_state_for_test(&state, &[])
        .with_operation(operation.clone())
        .expect("stamp retry from advanced head")
        .0;
    retry_from_new_head.expected_head_revision = first.head_revision;
    let retry_hash = retry_from_new_head
        .turn_commit_hash()
        .expect("retry commit hash");
    assert_eq!(
        retry_hash, turn_commit_hash,
        "turn commit identity must not depend on the optimistic CAS revision"
    );

    let changed_state = RuntimeSessionState {
        session_id: "root".to_string(),
        turn_index: 1,
        ..RuntimeSessionState::default()
    };
    let mut changed = RuntimeCommit::persisted_state_for_test(&changed_state, &[]);
    changed.turn_commit =
        RuntimeTurnCommitStamp::new(crate::OperationId::turn("root", "provider-turn", "final"));
    let err = store
        .commit_runtime_state(changed)
        .await
        .expect_err("same provider turn id with a different commit hash must conflict");
    assert!(matches!(err, StoreError::RuntimeTurnCommitConflict { .. }));
}

async fn store_computed_hash_rejects_mutated_commit(store: Arc<dyn RuntimePersistence>) {
    let mut state = RuntimeSessionState {
        session_id: "root".to_string(),
        ..RuntimeSessionState::default()
    };
    state.ensure_agent_frame_initialized();
    let operation = crate::OperationId::turn("root", "realization-guard", "final");
    let frame_key = "realization-guard-frame";
    let node_id = crate::session_graph::frame_node_id(&state.session_id, frame_key);
    let graph = crate::GraphAppend {
        nodes: vec![crate::SessionNodeRecord {
            node_id: node_id.clone(),
            parent_node_id: None,
            timestamp: "2026-07-26T10:00:00Z".to_string(),
            payload: crate::SessionNodePayload::FrameOpen {
                frame_key: frame_key.to_string(),
                reason: AgentFrameReason::initial(),
                assignment: crate::AgentFrameAssignment::from_policy(
                    crate::SessionPolicy::default(),
                ),
                protocol_turn_options: ProtocolTurnOptions::default(),
            },
        }],
        leaf_node_id: Some(node_id.clone()),
    };
    let (first, node_id_mapping) =
        RuntimeCommit::persisted_state_with_graph_commit(&state, graph, &[])
            .with_operation(operation)
            .expect("stamp guarded commit");
    assert_eq!(
        node_id_mapping,
        vec![(node_id.clone(), node_id.clone())],
        "operation stamping must return the append-id mapping"
    );
    commit_runtime_state_for_test(&store, first.clone(), "realization-guard")
        .await
        .expect("first guarded commit");

    let first_hash = first.turn_commit_hash().expect("first store-computed hash");
    let mut divergent_replay = first;
    let crate::GraphAppend { nodes, .. } = &mut divergent_replay.graph;
    nodes[0].parent_node_id = Some("proposal-only-parent".to_string());
    let divergent_hash = divergent_replay
        .turn_commit_hash()
        .expect("mutated store-computed hash");
    assert_ne!(
        divergent_hash, first_hash,
        "the receipt identity must cover mutated topology"
    );
    let err = crate::store::commit_runtime_state_verified(store.as_ref(), divergent_replay)
        .await
        .expect_err("the store must reject a mutated commit reusing an operation id");
    assert!(matches!(err, StoreError::RuntimeTurnCommitConflict { .. }));
    let stored = store
        .load_node(&node_id)
        .await
        .expect("load guarded node")
        .expect("guarded node remains stored");
    assert_eq!(
        stored.parent_node_id, None,
        "a rejected receipt replay must not adopt or persist proposal topology"
    );
}

async fn commit_rejects_non_derived_append_node_ids(store: Arc<dyn RuntimePersistence>) {
    let mut state = RuntimeSessionState {
        session_id: "root".to_string(),
        ..RuntimeSessionState::default()
    };
    state.ensure_agent_frame_initialized();
    let operation = crate::OperationId::turn("root", "guard-turn", "final");
    let graph = crate::GraphAppend {
        nodes: vec![crate::SessionNodeRecord {
            node_id: "rogue-node-id".to_string(),
            parent_node_id: None,
            timestamp: "2026-07-26T10:00:00Z".to_string(),
            payload: crate::SessionNodePayload::Plugin {
                plugin_type: "guard".to_string(),
                body: crate::session_graph::SharedJsonValue::new(serde_json::json!({"ok": true})),
            },
        }],
        leaf_node_id: Some("rogue-node-id".to_string()),
    };
    let mut commit = RuntimeCommit::persisted_state_with_graph_commit(&state, graph, &[]);
    commit.turn_commit = RuntimeTurnCommitStamp::new(operation);
    let err = commit_runtime_state_for_test(&store, commit, "node-guard")
        .await
        .expect_err("store must rederive append node ids before writing");
    assert!(matches!(err, StoreError::NodeIdDerivationMismatch { .. }));
    assert!(
        store
            .load_session()
            .await
            .expect("load after guard rejection")
            .is_none(),
        "guard rejection must happen before any durable write"
    );
}

async fn append_rejects_existing_node_id_collision(store: Arc<dyn RuntimePersistence>) {
    let mut state = RuntimeSessionState {
        session_id: "root".to_string(),
        ..RuntimeSessionState::default()
    };
    state.ensure_agent_frame_initialized();
    let frame_key = "collision-frame";
    let colliding_id = crate::session_graph::frame_node_id(&state.session_id, frame_key);
    let original = crate::SessionNodeRecord {
        node_id: colliding_id.clone(),
        parent_node_id: None,
        timestamp: "2026-07-26T10:00:00Z".to_string(),
        payload: crate::SessionNodePayload::FrameOpen {
            frame_key: frame_key.to_string(),
            reason: AgentFrameReason::new("original"),
            assignment: crate::AgentFrameAssignment::from_policy(crate::SessionPolicy::default()),
            protocol_turn_options: ProtocolTurnOptions::default(),
        },
    };
    state.session_graph =
        crate::SessionGraph::from_nodes(vec![original.clone()], Some(colliding_id.clone()));
    let initial = RuntimeCommit::persisted_state_for_test(&state, &[]);
    let first = commit_runtime_state_for_test(&store, initial, "collision-seed")
        .await
        .expect("seed colliding durable node");

    let replacement = crate::SessionNodeRecord {
        payload: crate::SessionNodePayload::FrameOpen {
            frame_key: frame_key.to_string(),
            reason: AgentFrameReason::new("replacement"),
            assignment: crate::AgentFrameAssignment::from_policy(crate::SessionPolicy::default()),
            protocol_turn_options: ProtocolTurnOptions::default(),
        },
        ..original
    };
    let mut append = RuntimeCommit::persisted_state_with_graph_commit(
        &state,
        crate::GraphAppend {
            nodes: vec![replacement],
            leaf_node_id: Some(colliding_id.clone()),
        },
        &[],
    );
    append.expected_head_revision = first.head_revision;
    let err = commit_runtime_state_for_test(&store, append, "collision-append")
        .await
        .expect_err("append must reject an id already present in durable history");
    assert!(matches!(
        err,
        StoreError::NodeIdCollision { ref node_id } if node_id == &colliding_id
    ));
    let stored = store
        .load_node(&colliding_id)
        .await
        .expect("load original node")
        .expect("original node remains");
    let (reason, _, _) = stored.frame_open().expect("stored frame");
    assert_eq!(reason.as_str(), "original");
}

async fn append_rejects_duplicate_batch_node_ids(store: Arc<dyn RuntimePersistence>) {
    let state = RuntimeSessionState {
        session_id: "root".to_string(),
        ..RuntimeSessionState::default()
    };
    let duplicate_node_id = crate::frame_node_id("root", "duplicate");
    let commit = RuntimeCommit::persisted_state_with_graph_commit(
        &state,
        crate::GraphAppend {
            nodes: vec![
                sample_session_node("root", "duplicate", None),
                sample_session_node("root", "duplicate", None),
            ],
            leaf_node_id: Some(duplicate_node_id.clone()),
        },
        &[],
    );
    let err = commit_runtime_state_for_test(&store, commit, "duplicate-batch")
        .await
        .expect_err("a duplicate id in one append must abort the whole commit");
    assert!(matches!(
        err,
        StoreError::NodeIdCollision { ref node_id } if node_id == &duplicate_node_id
    ));
    assert!(
        store
            .load_session()
            .await
            .expect("load after duplicate rejection")
            .is_none(),
        "duplicate rejection must happen before any durable write"
    );
}

async fn commit_rejects_unresolvable_leaf(store: Arc<dyn RuntimePersistence>) {
    let state = RuntimeSessionState {
        session_id: "root".to_string(),
        ..RuntimeSessionState::default()
    };
    let commit = RuntimeCommit::persisted_state_with_graph_commit(
        &state,
        crate::GraphAppend {
            nodes: vec![sample_session_node("root", "valid-node", None)],
            leaf_node_id: Some("missing-leaf".to_string()),
        },
        &[],
    );
    let err = commit_runtime_state_for_test(&store, commit, "invalid-leaf")
        .await
        .expect_err("commit leaf must resolve in the post-commit live graph");
    assert!(matches!(
        err,
        StoreError::InvalidGraphLeaf {
            leaf_node_id: Some(ref leaf)
        } if leaf == "missing-leaf"
    ));
    let valid_node_id = crate::frame_node_id("root", "valid-node");
    assert!(
        store
            .load_node(&valid_node_id)
            .await
            .expect("load after leaf rejection")
            .is_none(),
        "leaf rejection must abort the whole commit"
    );
}

async fn commit_rejects_missing_leaf(store: Arc<dyn RuntimePersistence>) {
    let state = RuntimeSessionState {
        session_id: "root".to_string(),
        ..RuntimeSessionState::default()
    };
    let missing = RuntimeCommit::persisted_state_with_graph_commit(
        &state,
        crate::GraphAppend {
            nodes: vec![sample_session_node("root", "node-without-leaf", None)],
            leaf_node_id: None,
        },
        &[],
    );
    let err = commit_runtime_state_for_test(&store, missing, "missing-leaf")
        .await
        .expect_err("a non-empty graph commit requires a resolving leaf");
    assert!(matches!(
        err,
        StoreError::InvalidGraphLeaf { leaf_node_id: None }
    ));
    let node_without_leaf_id = crate::frame_node_id("root", "node-without-leaf");
    assert!(
        store
            .load_node(&node_without_leaf_id)
            .await
            .expect("load after missing leaf rejection")
            .is_none(),
        "missing leaf rejection must abort the whole commit"
    );
}

async fn empty_append_cannot_move_the_head(store: Arc<dyn RuntimePersistence>) {
    let mut state = RuntimeSessionState {
        session_id: "empty-append-head-move".to_string(),
        ..RuntimeSessionState::default()
    };
    state.ensure_agent_frame_initialized();
    let first = store
        .commit_runtime_state(RuntimeCommit::persisted_state_for_test(&state, &[]))
        .await
        .expect("seed the live head");
    let old_leaf = state.session_graph.leaf_node_id.clone();
    state.apply_persisted_commit_result(first);
    let mut move_attempt = RuntimeCommit::persisted_state_with_graph_commit(
        &state,
        crate::GraphAppend {
            nodes: Vec::new(),
            leaf_node_id: None,
        },
        &[],
    );
    move_attempt.current_frame_node_id = old_leaf.clone();
    let error = store
        .commit_runtime_state(move_attempt)
        .await
        .expect_err("an empty append must not move the head");
    assert!(matches!(
        error,
        StoreError::InvalidGraphLeaf { leaf_node_id: None }
    ));
    let loaded = store
        .load_session()
        .await
        .expect("load after rejected empty append")
        .expect("seeded session remains");
    assert_eq!(loaded.graph.leaf_node_id, old_leaf);
}
