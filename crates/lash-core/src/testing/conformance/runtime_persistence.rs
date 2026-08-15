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
const REALTIME_SCAFFOLDING_LEASE_TTL_MS: u64 = 500;
// Real database operations can be descheduled between claiming a lease and
// observing it. This is a harness stall allowance, not the semantic expiry
// boundary: controlled-clock backends still prove the 50 ms contract exactly.
const REALTIME_LEASE_STALL_ALLOWANCE: std::time::Duration = std::time::Duration::from_secs(5);
const REALTIME_LEASE_OBSERVATION_ATTEMPTS: usize = 3;
const REALTIME_LEASE_EXPIRY_POLL: std::time::Duration = std::time::Duration::from_millis(10);
const REALTIME_DELAYED_QUEUE_ROW_GAP_MS: u64 = 500;
const REALTIME_DELAYED_QUEUE_ROW_CROSSING_MARGIN_MS: u64 = 50;

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

    pub(super) fn scaffolding_lease_ttl_ms(&self) -> u64 {
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

    /// Let one durable queued-drain wait slice pass in this backend's own time
    /// domain: real sleeping where the backend owns its clock, an injected-clock
    /// advance otherwise.
    pub(super) async fn pass_wait_slice(&self, slice_ms: u64) {
        match self {
            Self::Realtime => {
                tokio::time::sleep(std::time::Duration::from_millis(slice_ms)).await;
            }
            Self::Controlled(advance) => advance(slice_ms),
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

    fn delayed_queue_row_available_at_ms(&self) -> u64 {
        match self {
            Self::Realtime => {
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .expect("system clock is after Unix epoch")
                    .as_millis() as u64
                    + REALTIME_DELAYED_QUEUE_ROW_GAP_MS
            }
            Self::Controlled(_) => 4_102_444_800_000,
        }
    }

    async fn cross_delayed_queue_row_boundary(&self) {
        match self {
            Self::Realtime => {
                tokio::time::sleep(std::time::Duration::from_millis(
                    REALTIME_DELAYED_QUEUE_ROW_GAP_MS
                        + REALTIME_DELAYED_QUEUE_ROW_CROSSING_MARGIN_MS,
                ))
                .await
            }
            Self::Controlled(advance) => advance(4_102_444_800_000),
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
    F: Fn(&str) -> Arc<dyn RuntimePersistence>,
{
    let first = make("fresh-instance-probe");
    let second = make("fresh-instance-probe");
    assert_fresh_instances(&first, &second, "runtime_persistence");
    drop((first, second));
    runtime_persistence_suite(make, &lease_timing).await;
}

/// Run the full [`RuntimePersistence`] suite plus durable reopen checks.
pub async fn runtime_persistence_reopenable<F>(make: F, lease_timing: RuntimePersistenceLeaseTiming)
where
    F: Fn(&str) -> ReopenableRuntimePersistence,
{
    let probe = make("pending-turn-input-multi-store-mint");
    assert_fresh_instances(&probe.open, &probe.reopen, "runtime_persistence_reopenable");
    pending_turn_input_mint_is_unique_across_store_instances(
        probe.open.as_ref(),
        probe.reopen.as_ref(),
    )
    .await;
    drop(probe);
    runtime_persistence_suite(|session_id| make(session_id).open, &lease_timing).await;
    gc_reclaims_unreachable_checkpoint_blobs_and_preserves_live(make("gc-blobs").open).await;
    append_receipt_survives_reopen(make("root")).await;
    runtime_persistence_survives_reopen(make("root")).await;
}

fn assert_two_session_resolution_errors(full: StoreError, head: StoreError, expected: &str) {
    match (full, head) {
        (
            StoreError::SessionResolutionAmbiguous {
                session_count: full_count,
            },
            StoreError::SessionResolutionAmbiguous {
                session_count: head_count,
            },
        ) => {
            assert_eq!(
                full_count, 2,
                "{expected}: full read session candidate count: expected 2, got {full_count}"
            );
            assert_eq!(
                head_count, 2,
                "{expected}: head read session candidate count: expected 2, got {head_count}"
            );
        }
        (full, head) => panic!(
            "{expected}: full and head reads returned different typed errors: full={full:?}, head={head:?}"
        ),
    }
}

/// Whether a session candidate has only been durably admitted or also has a
/// committed runtime head.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UnboundSessionAdmissionState {
    AdmittedOnly,
    Committed,
}

impl UnboundSessionAdmissionState {
    fn label(self) -> &'static str {
        match self {
            Self::AdmittedOnly => "admitted-only",
            Self::Committed => "committed",
        }
    }
}

/// One isolated durable substrate used by the unbound-session resolution law.
#[derive(Clone)]
pub struct UnboundSessionResolutionHandles {
    pub backend_name: &'static str,
    pub factory: Arc<dyn crate::SessionStoreFactory>,
    pub open_unbound: Arc<dyn Fn() -> Arc<dyn RuntimePersistence> + Send + Sync>,
}

/// Prove that an unbound handle resolves the same session for both shared
/// session-read projections across the full durable candidate matrix:
/// `{0, 1, 2} sessions x {admitted-only, committed}`.
///
/// `make_axis` must return a fresh, initially empty durable substrate for each
/// admission state. Every `open_unbound` call must return a newly opened,
/// unbound handle over that axis's shared substrate. This law is instantiated
/// by SQLite. Neither the in-memory nor the PostgreSQL backend has a global
/// unbound multi-session handle — a PostgreSQL session store is constructed
/// with its session id — so none of these six cells is instantiated there.
pub async fn unbound_session_reads_resolve_the_same_session<MakeAxis, MakeAxisFuture>(
    make_axis: MakeAxis,
) where
    MakeAxis: Fn(UnboundSessionAdmissionState) -> MakeAxisFuture,
    MakeAxisFuture: std::future::Future<Output = UnboundSessionResolutionHandles>,
{
    #[derive(Debug, PartialEq, Eq)]
    enum ReadResolution {
        Absent,
        Present,
        Indeterminate,
    }

    async fn assert_reads_agree(
        handles: &UnboundSessionResolutionHandles,
        expected: &str,
    ) -> ReadResolution {
        let head = (handles.open_unbound)().load_session_head_meta().await;
        let full = (handles.open_unbound)().load_session().await;
        match (full, head) {
            (Ok(None), Ok(None)) => ReadResolution::Absent,
            (Ok(Some(full)), Ok(Some(head))) => {
                assert_eq!(head.session_id, full.session_id, "{expected}: session id");
                assert_eq!(
                    head.head_revision, full.head_revision,
                    "{expected}: head revision"
                );
                assert_eq!(
                    head.leaf_node_id, full.graph.leaf_node_id,
                    "{expected}: leaf node id"
                );
                assert_eq!(
                    head.checkpoint_ref, full.checkpoint_ref,
                    "{expected}: checkpoint reference"
                );
                ReadResolution::Present
            }
            (Err(full), Err(head)) => {
                assert_two_session_resolution_errors(full, head, expected);
                ReadResolution::Indeterminate
            }
            (full, head) => panic!(
                "{expected}: full and head reads disagreed about session resolution: full={full:?}, head={head:?}"
            ),
        }
    }

    fn request(session_id: &str) -> crate::SessionStoreCreateRequest {
        crate::SessionStoreCreateRequest {
            session_id: session_id.to_string(),
            relation: crate::SessionRelation::Root,
            policy: crate::SessionPolicy::new(crate::TurnBudget::Unbounded),
        }
    }

    async fn add_session(
        handles: &UnboundSessionResolutionHandles,
        admission_state: UnboundSessionAdmissionState,
        session_id: &str,
    ) {
        let store = handles
            .factory
            .create_store(&request(session_id))
            .await
            .unwrap_or_else(|error| {
                panic!(
                    "{} {admission_state:?}: admit `{session_id}`: {error}",
                    handles.backend_name
                )
            });
        if admission_state == UnboundSessionAdmissionState::Committed {
            let state = RuntimeSessionState {
                session_id: session_id.to_string(),
                ..RuntimeSessionState::new(crate::SessionPolicy::new(crate::TurnBudget::Unbounded))
            };
            commit_runtime_state_for_test(
                &store,
                RuntimeCommit::persisted_state_for_test(&state, &[]),
                session_id,
            )
            .await
            .unwrap_or_else(|error| {
                panic!("{}: commit `{session_id}`: {error}", handles.backend_name)
            });
        }
    }

    for admission_state in [
        UnboundSessionAdmissionState::AdmittedOnly,
        UnboundSessionAdmissionState::Committed,
    ] {
        let handles = make_axis(admission_state).await;
        let cell = |session_count| {
            format!(
                "{} backend, {} sessions, {}",
                handles.backend_name,
                session_count,
                admission_state.label()
            )
        };

        assert_eq!(
            assert_reads_agree(&handles, &cell(0)).await,
            ReadResolution::Absent,
            "{} must resolve as absent",
            cell(0)
        );

        add_session(&handles, admission_state, "unbound-resolution-a").await;
        let one_expected = match admission_state {
            UnboundSessionAdmissionState::AdmittedOnly => ReadResolution::Absent,
            UnboundSessionAdmissionState::Committed => ReadResolution::Present,
        };
        assert_eq!(
            assert_reads_agree(&handles, &cell(1)).await,
            one_expected,
            "{} must have the expected resolution",
            cell(1)
        );

        add_session(&handles, admission_state, "unbound-resolution-b").await;
        assert_eq!(
            assert_reads_agree(&handles, &cell(2)).await,
            ReadResolution::Indeterminate,
            "{} must report typed ambiguity",
            cell(2)
        );
    }
}

/// A newly minted turn-input identity is store-wide rather than handle-local.
///
/// Reopenable backends exercise this through two independently constructed
/// handles over one durable store. Their conformance clocks deliberately keep
/// both admissions in one millisecond so the nonce is the deciding fact.
async fn pending_turn_input_mint_is_unique_across_store_instances(
    first: &dyn RuntimePersistence,
    second: &dyn RuntimePersistence,
) {
    let session_id = "pending-turn-input-multi-store-mint";
    let first_input = first
        .enqueue_pending_turn_input(pending_next_turn_input_draft(
            session_id,
            "first independent-store input",
        ))
        .await
        .expect("first store instance mints a pending turn-input ID");
    let second_input = second
        .enqueue_pending_turn_input(pending_next_turn_input_draft(
            session_id,
            "second independent-store input",
        ))
        .await
        .expect("second store instance mints a pending turn-input ID");

    assert_ne!(
        first_input.input_id, second_input.input_id,
        "independent store instances must mint distinct pending turn-input IDs in one millisecond"
    );
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
        .try_claim_session_execution_lease(
            session_id,
            &stale_owner,
            "runtime-persistence-clock-expiry-executor",
            TTL_MS,
        )
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
            crate::testing::queued_work_claim_policy(64),
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
        .try_claim_session_execution_lease(
            session_id,
            &successor,
            "runtime-persistence-clock-expiry-executor-2",
            TTL_MS,
        )
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
            crate::testing::queued_work_claim_policy(64),
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
        ..RuntimeSessionState::new(crate::SessionPolicy::new(crate::TurnBudget::Unbounded))
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
    F: Fn(&str) -> Arc<dyn RuntimePersistence>,
{
    // [`SessionCommitStore`]: atomic head commits, reads, metadata, the
    // attachment write-ahead manifest, and turn-commit idempotency.
    commit_increments_head_and_round_trips_agent_frames(make("root")).await;
    concurrent_head_revision_cas_applies_exactly_once(make("concurrent-head-cas")).await;
    commit_rejects_a_different_session_id(make("alpha")).await;
    commit_rejects_carried_nondefault_node_budget(make("root")).await;
    commit_rejects_carried_nondefault_byte_budget(make("root")).await;
    load_hydrates_checkpoint_and_usage(make("hydrated")).await;
    load_retains_reasoning_only_usage(make("root")).await;
    checkpoint_restore_rejects_turn_index_without_increment_headroom(make("root")).await;
    checkpoint_restore_rejects_token_usage_whose_prompt_subtotal_overflows(make("root")).await;
    load_rejects_token_usage_overflow(make("root")).await;
    usage_delta_identity_is_idempotent_across_commits(make("root")).await;
    usage_ordinal_reuse_with_different_payload_survives_receipt_replay(make("root")).await;
    execution_state_replace_then_clear_removes_the_live_checkpoint_ref(make(
        "execution-state-replace-then-clear",
    ))
    .await;
    checkpoint_rejects_unknown_component_ref(make("checkpoint-unknown-ref")).await;
    session_read_loads_persisted_history(make("branchy")).await;
    session_prompt_layer_round_trips_through_the_committed_head(make("session-prompt-layer")).await;
    session_metadata_round_trips(make("root")).await;
    attachment_manifest_records_intent_and_commit_stamps(make("root")).await;
    attachment_manifest_keeps_same_content_ownership_per_session(make("root")).await;
    attachment_manifest_reference_tracking_and_gc_root_set(make("root")).await;
    final_commit_stamp_is_idempotent_and_conflicts_on_changed_hash(make("root")).await;
    append_request_receipt_replays_after_head_advance(make("root")).await;
    append_request_receipt_rejects_changed_content(make("root")).await;
    append_request_exact_hash_rejects_changed_ancestor(make("root")).await;
    append_request_receipt_rejects_corrupt_node_count(make("root")).await;
    concurrent_same_append_operation_applies_exactly_once(make("root")).await;
    legacy_append_receipt_keeps_exact_hash_semantics(make("root")).await;
    append_receipt_encoding_version_mismatch_keeps_exact_hash_semantics(make("root")).await;
    append_receipt_and_graph_append_are_atomic(make("root")).await;
    fresh_append_receipt_enforces_ancestor_precondition(make("root")).await;
    store_computed_hash_rejects_mutated_commit(make("root")).await;
    commit_rejects_non_derived_append_node_ids(make("root")).await;
    append_rejects_duplicate_batch_node_ids(make("root")).await;
    append_rejects_existing_node_id_collision(make("root")).await;
    head_retirement_gate_distinguishes_leaf_change_from_same_leaf(make("root")).await;
    commit_rejects_unresolvable_leaf(make("root")).await;
    commit_rejects_missing_leaf(make("root")).await;
    empty_append_cannot_move_the_head(make("empty-append-head-move")).await;
    commit_rejects_leaf_without_frame_open_ancestor(make("missing-frame-root")).await;
    // [`SessionExecutionLeaseStore`]: single-writer lane fencing.
    session_execution_lease_contract(make("root")).await;
    borrowed_session_execution_lease_commit_contract(make("borrowed-commit-fence")).await;
    same_incarnation_rotation_gates_claims_not_commits(make("root")).await;
    same_host_distinct_executors_are_lane_less_without_revoking_holder(make(
        "fig1133-same-host-session",
    ))
    .await;
    session_execution_lease_fence_authority(make("lease-fence-authority").as_ref()).await;
    concurrent_session_execution_lease_rotation_and_stale_renewal_are_linearizable(make(
        "concurrent-rotation-renewal",
    ))
    .await;
    session_execution_lease_expires_by_ttl_contract(&|| make("ttl-expiry"), lease_timing).await;
    super::durable_queued_drain_wait_contract(make("durable-queued-drain"), lease_timing).await;
    session_execution_lease_diagnostic_read_contract(make("lease-diagnostic")).await;
    session_execution_lease_displacement_contract(make("lease-displacement")).await;
    // [`QueuedWorkStore`]: durable queued-work ingress, ordering, and claim
    // leases, plus the commit-side completion atomicity it shares with
    // [`SessionCommitStore`].
    queued_work_source_keys_are_idempotent_and_list_ordered(make("queued-work-source-keys")).await;
    concurrent_queue_and_turn_input_claims_have_one_owner(make("concurrent-queue-input")).await;
    checkpoint_work_claims_both_families_once(make("checkpoint-work")).await;
    checkpoint_budget_refusal_preserves_active_turn_input(make("checkpoint-budget-refusal")).await;
    queued_work_cancel_removes_only_unclaimed_batches(make("queued-work-cancel")).await;
    queued_work_exact_claim_uses_selected_batch_ids(make("root")).await;
    queued_work_classes_gate_command_and_turn_claims(make("root")).await;
    queued_work_claims_respect_boundaries_abandon_and_stale_completion(make("root")).await;
    queued_work_claims_supersede_across_session_lease_generations_with_timing(
        make("root"),
        lease_timing,
    )
    .await;
    claim_liveness_for_lease_less_paths_tracks_session_generations(
        make("claim-liveness"),
        lease_timing,
    )
    .await;
    same_generation_claim_scans_reach_rows_beyond_the_scan_surplus(make("claim-scan")).await;
    queued_work_respects_membership_limits_exclusivity_reclaim_and_sessions(make(
        "queued-membership",
    ))
    .await;
    queued_work_join_groups_by_delivery_policy_and_merge_key(make("queued-join")).await;
    queued_work_redrive_preserves_interrupted_batch_composition(make("redrive-composition")).await;
    queued_work_redrive_selects_claim_identity_across_ready_gap(
        make("redrive-ready-gap"),
        lease_timing,
    )
    .await;
    queued_work_redrive_obeys_delivery_boundary_before_identity(make("redrive-boundary")).await;
    queued_work_redrive_ignores_successor_row_limit(make("redrive-row-limit")).await;
    queued_work_selected_multi_identity_validation_and_abandon_restore(make(
        "selected-multi-identity",
    ))
    .await;
    queued_work_exact_claim_preserves_physical_order_and_key_breaks(make("physical-order")).await;
    process_wakes_batch_by_default(make("wake-default-batch")).await;
    queued_work_completion_is_lease_guarded(make("root")).await;
    queued_wake_delivery_is_source_key_idempotent_and_claimed_once(make("root")).await;
    queue_completion_and_turn_commit_stamp_are_atomic(make("root")).await;
    // [`TurnInputStore`]: pending turn-input lifecycle.
    pending_turn_inputs_source_keys_order_cancel_and_cross_session(make("root")).await;
    pending_turn_input_bulk_and_suffix_cancellation(make("pending-bulk-cancel")).await;
    pending_turn_input_claims_reclaim_complete_and_fence(make("root")).await;
    turn_input_application_identity_survives_pending_tombstone_vacuum(make(
        "turn-input-application",
    ))
    .await;
    turn_input_claims_supersede_across_session_lease_generations_with_timing(
        make("root"),
        lease_timing,
    )
    .await;
    active_turn_input_claim_reacquires_after_unrecorded_checkpoint(make("fig905-active-reacquire"))
        .await;
    pending_turn_input_cancel_covers_active_and_deferred_states(make("root")).await;
    pending_active_turn_inputs_defer_unaccepted_once_on_interrupt(make("root")).await;
}

async fn session_prompt_layer_round_trips_through_the_committed_head(
    store: Arc<dyn RuntimePersistence>,
) {
    let expected_prompt =
        crate::PromptLayer::new().with_contribution(crate::PromptContribution::guidance(
            "Session policy",
            "Continue with the persisted session-specific instructions.",
        ));
    let mut policy = crate::SessionPolicy::new(crate::TurnBudget::Unbounded);
    policy.prompt = expected_prompt.clone();
    let state = RuntimeSessionState {
        session_id: "session-prompt-layer".to_string(),
        policy,
        ..RuntimeSessionState::new(crate::SessionPolicy::new(crate::TurnBudget::Unbounded))
    };

    commit_runtime_state_for_test(
        &store,
        RuntimeCommit::persisted_state_for_test(&state, &[]),
        "session-prompt-layer",
    )
    .await
    .expect("commit session prompt layer");

    let head = store
        .load_session_head_meta()
        .await
        .expect("load session head")
        .expect("committed session head");
    assert_eq!(head.config.prompt, expected_prompt);
    let restored = crate::store::load_persisted_session_state(store.as_ref())
        .await
        .expect("load persisted session state")
        .expect("committed session state");
    assert_eq!(restored.policy.prompt, expected_prompt);
}

async fn execution_state_replace_then_clear_removes_the_live_checkpoint_ref(
    store: Arc<dyn RuntimePersistence>,
) {
    let mut state =
        RuntimeSessionState::new(crate::SessionPolicy::new(crate::TurnBudget::Unbounded));
    state.session_id = "execution-state-replace-then-clear".to_string();
    state.set_execution_state_snapshot(Some(b"initial-execution-state".to_vec()));

    let initial = commit_runtime_state_for_test(
        &store,
        RuntimeCommit::persisted_state_for_test(&state, &[]),
        "execution-state-initial",
    )
    .await
    .expect("commit initial execution state");
    state.apply_persisted_commit_result(initial);

    state.set_execution_state_snapshot(Some(b"replacement-execution-state".to_vec()));
    let replacement = commit_runtime_state_for_test(
        &store,
        RuntimeCommit::persisted_state_for_test(&state, &[]),
        "execution-state-replacement",
    )
    .await
    .expect("replace execution state");
    assert!(
        replacement
            .manifest
            .component_ref(crate::store::EXECUTION_STATE_CHECKPOINT_COMPONENT)
            .is_some()
    );
    state.apply_persisted_commit_result(replacement);

    state.set_execution_state_snapshot(None);
    let cleared = commit_runtime_state_for_test(
        &store,
        RuntimeCommit::persisted_state_for_test(&state, &[]),
        "execution-state-clear",
    )
    .await
    .expect("clear replacement execution state");
    assert!(
        cleared
            .manifest
            .component_ref(crate::store::EXECUTION_STATE_CHECKPOINT_COMPONENT)
            .is_none()
    );

    let durable = store
        .load_session()
        .await
        .expect("load replace-then-clear session")
        .expect("replace-then-clear session is durable");
    let checkpoint = durable
        .checkpoint
        .expect("replace-then-clear session has a checkpoint");
    assert!(
        checkpoint
            .component_ref(crate::store::EXECUTION_STATE_CHECKPOINT_COMPONENT)
            .is_none()
    );
}

async fn commit_rejects_carried_nondefault_node_budget(store: Arc<dyn RuntimePersistence>) {
    const CONFIGURED_NODE_LIMIT: usize = 1;
    let state = RuntimeSessionState {
        session_id: "root".to_string(),
        ..RuntimeSessionState::new(crate::SessionPolicy::new(crate::TurnBudget::Unbounded))
    };
    let parent = sample_session_node("root", "budget-frame", None);
    let child = sample_session_node("root", "budget-child", Some(&parent.node_id));
    let budget = crate::CommitBudget::new(
        crate::CommitBudgetLimit::Unbounded,
        crate::CommitBudgetLimit::bounded(CONFIGURED_NODE_LIMIT),
    );
    let mut commit = RuntimeCommit::persisted_state_for_test_with_budget(&state, &[], budget);
    commit.graph = crate::GraphAppend {
        nodes: vec![parent, child],
        leaf_node_id: None,
    };

    let error = store
        .commit_runtime_state(commit)
        .await
        .expect_err("backend must enforce the carried non-default node budget");
    assert!(matches!(
        error,
        StoreError::CommitNodeBudgetExceeded {
            node_count: 2,
            max_nodes: CONFIGURED_NODE_LIMIT,
        }
    ));
}

async fn commit_rejects_carried_nondefault_byte_budget(store: Arc<dyn RuntimePersistence>) {
    const CONFIGURED_BYTE_LIMIT: usize = 64;
    let state = RuntimeSessionState {
        session_id: "root".to_string(),
        ..RuntimeSessionState::new(crate::SessionPolicy::new(crate::TurnBudget::Unbounded))
    };
    let budget = crate::CommitBudget::new(
        crate::CommitBudgetLimit::bounded(CONFIGURED_BYTE_LIMIT),
        crate::CommitBudgetLimit::Unbounded,
    );
    let mut commit = RuntimeCommit::persisted_state_for_test_with_budget(&state, &[], budget);
    commit.checkpoint.components.insert(
        crate::store::EXECUTION_STATE_CHECKPOINT_COMPONENT.to_string(),
        crate::HydratedCheckpointComponent::changed(vec![0; CONFIGURED_BYTE_LIMIT * 2]),
    );

    let error = store
        .commit_runtime_state(commit)
        .await
        .expect_err("backend must enforce the carried non-default byte budget");
    assert!(matches!(
        error,
        StoreError::CommitByteBudgetExceeded {
            max_bytes: CONFIGURED_BYTE_LIMIT,
            ..
        }
    ));
}

async fn head_retirement_gate_distinguishes_leaf_change_from_same_leaf(
    store: Arc<dyn RuntimePersistence>,
) {
    let state = seed_append_receipt_state(&store).await;
    let old_leaf = state.session_graph.leaf_node_id.clone().expect("seed leaf");

    let same_leaf_commit = RuntimeCommit::persisted_state_for_test(&state, &[]);
    let seed_frame_node_id = same_leaf_commit
        .current_frame_node_id
        .clone()
        .expect("seed frame");
    let same_leaf_planner = crate::store::RuntimeCommitPlanner::prepare(same_leaf_commit.clone())
        .expect("prepare same-leaf commit");
    let same_leaf_plan = same_leaf_planner
        .plan(crate::store::FreshRuntimeCommitFacts {
            actual_head_revision: same_leaf_commit.expected_head_revision,
            old_leaf_node_id: Some(old_leaf.clone()),
            requested_ancestor_is_active: true,
            occupied_node_ids: std::collections::HashSet::new(),
            selected_leaf_is_live: true,
            has_live_nodes: true,
            old_leaf_is_live: true,
            parent_node_facts: Some(crate::store::ParentNodeFacts {
                node_id: old_leaf.clone(),
                generation: state.session_graph.active_path_nodes().len() as u64 - 1,
                frame_node_id: seed_frame_node_id.clone(),
            }),
        })
        .expect("plan same-leaf commit");
    assert!(
        !same_leaf_plan.head_changed(),
        "a same-leaf commit must not prescribe ancestry retirement"
    );
    store
        .commit_runtime_state(same_leaf_commit)
        .await
        .expect("same-leaf commit");
    assert!(
        store
            .load_node(&old_leaf)
            .await
            .expect("load old leaf after same-leaf commit")
            .is_some(),
        "a same-leaf commit must tombstone nothing"
    );

    let mut changed_state = loaded_conformance_state(&store).await;
    let nodes = vec![crate::SessionAppendNode::plugin(
        "retirement-gate",
        serde_json::json!({"leaf": "replacement"}),
    )];
    let (changed_commit, _) =
        append_request_commit(&mut changed_state, "retirement-gate-change", &nodes, None);
    let changed_planner = crate::store::RuntimeCommitPlanner::prepare(changed_commit.clone())
        .expect("prepare leaf-changing commit");
    let changed_plan = changed_planner
        .plan(crate::store::FreshRuntimeCommitFacts {
            actual_head_revision: changed_commit.expected_head_revision,
            old_leaf_node_id: Some(old_leaf.clone()),
            requested_ancestor_is_active: true,
            occupied_node_ids: std::collections::HashSet::new(),
            selected_leaf_is_live: false,
            has_live_nodes: true,
            old_leaf_is_live: true,
            parent_node_facts: Some(crate::store::ParentNodeFacts {
                node_id: old_leaf.clone(),
                generation: state.session_graph.active_path_nodes().len() as u64 - 1,
                frame_node_id: seed_frame_node_id,
            }),
        })
        .expect("plan leaf-changing commit");
    assert!(
        changed_plan.head_changed(),
        "a leaf-changing commit must prescribe retirement of its abandoned old head"
    );
    assert_eq!(
        changed_plan.old_leaf_node_id(),
        Some(old_leaf.as_str()),
        "the retirement prescription must name the abandoned old head"
    );
    store
        .commit_runtime_state(changed_commit)
        .await
        .expect("leaf-changing commit");
}

async fn load_retains_reasoning_only_usage(store: Arc<dyn RuntimePersistence>) {
    let state = RuntimeSessionState {
        session_id: "root".to_string(),
        ..RuntimeSessionState::new(crate::SessionPolicy::new(crate::TurnBudget::Unbounded))
    };
    let usage = TokenLedgerEntry {
        source: "reasoning-only".to_string(),
        model: "usage-model".to_string(),
        usage: TokenUsage {
            reasoning_output_tokens: 9,
            ..TokenUsage::default()
        },
    };
    commit_runtime_state_for_test(
        &store,
        RuntimeCommit::persisted_state_for_test(&state, std::slice::from_ref(&usage)),
        "reasoning-only usage seed",
    )
    .await
    .expect("seed reasoning-only durable usage");

    let read = store
        .load_session()
        .await
        .expect("load reasoning-only usage")
        .expect("reasoning-only usage session exists");
    assert_eq!(read.token_ledger.len(), 1);
    assert_eq!(read.token_ledger[0].source, usage.source);
    assert_eq!(read.token_ledger[0].usage, usage.usage);
}

async fn load_rejects_token_usage_overflow(store: Arc<dyn RuntimePersistence>) {
    let state = RuntimeSessionState {
        session_id: "root".to_string(),
        ..RuntimeSessionState::new(crate::SessionPolicy::new(crate::TurnBudget::Unbounded))
    };
    let usage = [
        TokenLedgerEntry {
            source: "overflow".to_string(),
            model: "usage-model".to_string(),
            usage: TokenUsage {
                input_tokens: i64::MAX,
                ..TokenUsage::default()
            },
        },
        TokenLedgerEntry {
            source: "overflow".to_string(),
            model: "usage-model".to_string(),
            usage: TokenUsage {
                input_tokens: 1,
                ..TokenUsage::default()
            },
        },
    ];
    commit_runtime_state_for_test(
        &store,
        RuntimeCommit::persisted_state_for_test(&state, &usage),
        "usage overflow seed",
    )
    .await
    .expect("seed distinct durable usage deltas");

    let error = store
        .load_session()
        .await
        .expect_err("overflowing usage rows must fail load");
    assert!(matches!(
        error,
        StoreError::TokenUsageAccountingOverflow {
            usage_source,
            model,
            counter: "input_tokens",
        } if usage_source == "overflow" && model == "usage-model"
    ));
}

async fn checkpoint_restore_rejects_turn_index_without_increment_headroom(
    store: Arc<dyn RuntimePersistence>,
) {
    let turn_index = usize::MAX - 16;
    let state = RuntimeSessionState {
        session_id: "root".to_string(),
        turn_index,
        ..RuntimeSessionState::new(crate::SessionPolicy::new(crate::TurnBudget::Unbounded))
    };
    commit_runtime_state_for_test(
        &store,
        RuntimeCommit::persisted_state_for_test(&state, &[]),
        "turn index overflow seed",
    )
    .await
    .expect("seed corrupt checkpoint turn index");

    let error = crate::store::load_persisted_session_state(store.as_ref())
        .await
        .expect_err("checkpoint turn index without increment headroom must fail restore");
    assert!(matches!(
        error,
        StoreError::CheckpointTurnIndexOutOfRange {
            turn_index: actual,
            max_exclusive,
        } if actual == turn_index && max_exclusive == turn_index
    ));
}

/// The prompt-side subtotal is not covered by the canonical total: signed
/// counters let a negative `output_tokens` hold the canonical total in range
/// while the prompt-side counters alone overflow. Restore must reject that
/// checkpoint rather than hand a poisoned base to the next turn's merge and to
/// the bare `total()`/`input_total()` policy readers.
async fn checkpoint_restore_rejects_token_usage_whose_prompt_subtotal_overflows(
    store: Arc<dyn RuntimePersistence>,
) {
    let token_usage = crate::TokenUsage {
        input_tokens: i64::MAX,
        output_tokens: i64::MIN,
        cache_read_input_tokens: i64::MAX,
        cache_write_input_tokens: 0,
        reasoning_output_tokens: 0,
    };
    assert!(
        token_usage.checked_total().is_ok(),
        "the canonical total must stay in range so this pins the prompt subtotal check"
    );
    let state = RuntimeSessionState {
        session_id: "root".to_string(),
        token_usage,
        ..RuntimeSessionState::new(crate::SessionPolicy::new(crate::TurnBudget::Unbounded))
    };
    commit_runtime_state_for_test(
        &store,
        RuntimeCommit::persisted_state_for_test(&state, &[]),
        "prompt subtotal overflow seed",
    )
    .await
    .expect("seed corrupt checkpoint token usage");

    let error = crate::store::load_persisted_session_state(store.as_ref())
        .await
        .expect_err("checkpoint usage whose prompt subtotal overflows must fail restore");
    assert!(matches!(
        error,
        StoreError::CheckpointTokenUsageOutOfRange {
            counter: "input_total_tokens"
        }
    ));
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
        ..RuntimeSessionState::new(crate::SessionPolicy::new(crate::TurnBudget::Unbounded))
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
        ..RuntimeSessionState::new(crate::SessionPolicy::new(crate::TurnBudget::Unbounded))
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
        ..RuntimeSessionState::new(crate::SessionPolicy::new(crate::TurnBudget::Unbounded))
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
        ..RuntimeSessionState::new(crate::SessionPolicy::new(crate::TurnBudget::Unbounded))
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

/// Prove that an inactive append ancestor wins over a simultaneously stale
/// head revision.
///
/// `supersede` atomically moves the durable head to the supplied earlier node
/// and advances its revision, constructing both rejected conditions without
/// creating a receipt.
///
/// Integrator class (ADR 0051): **conformance-suite embedders**.
pub async fn inactive_append_ancestor_precedes_stale_head<F, Fut>(
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
        "append-precedence",
        serde_json::json!({"value": "stale-head-and-ancestor"}),
    )];
    let (fresh, _) = append_request_commit(
        &mut state,
        "stale-head-and-ancestor",
        &nodes,
        Some(&required),
    );

    supersede(superseding_leaf).await;
    let error = store
        .commit_runtime_state(fresh)
        .await
        .expect_err("inactive ancestor must reject even when the head is also stale");
    assert!(
        matches!(
            &error,
            StoreError::AppendAncestorNotActive { required_node_id }
                if required_node_id == &required
        ),
        "inactive ancestor must precede stale head, got {error:?}"
    );
}

/// Prove that a head pointing at a tombstoned leaf is rejected as graph
/// corruption instead of being treated as a valid same-leaf commit.
///
/// Integrator class (ADR 0051): **conformance-suite embedders**.
pub async fn tombstoned_old_leaf_is_rejected<F, Fut>(
    store: Arc<dyn RuntimePersistence>,
    tombstone: F,
) where
    F: FnOnce(String) -> Fut,
    Fut: std::future::Future<Output = ()>,
{
    let mut state = seed_append_receipt_state(&store).await;
    let old_leaf = state.session_graph.leaf_node_id.clone().expect("seed leaf");
    let nodes = vec![crate::SessionAppendNode::plugin(
        "tombstoned-old-leaf",
        serde_json::json!({"value": "must-not-append"}),
    )];
    let (append, _) = append_request_commit(&mut state, "tombstoned-old-leaf", &nodes, None);

    tombstone(old_leaf.clone()).await;
    let error = store
        .commit_runtime_state(append)
        .await
        .expect_err("a tombstoned published leaf must reject the commit");
    assert!(
        matches!(
            &error,
            StoreError::InvalidGraphLeaf {
                leaf_node_id: Some(leaf)
            } if leaf == &old_leaf
        ),
        "tombstoned old leaf must be a typed invalid leaf, got {error:?}"
    );
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
        ..RuntimeSessionState::new(crate::SessionPolicy::new(crate::TurnBudget::Unbounded))
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
pub async fn complete_runtime_checkpoint_component_set_survives_cold_reopens<F>(make: F)
where
    F: Fn() -> Arc<dyn RuntimePersistence>,
{
    let open = make();
    let open_identity = Arc::downgrade(&open);
    bind_conformance_session(&open, "checkpoint-component-refs").await;
    let mut state = RuntimeSessionState {
        session_id: "checkpoint-component-refs".to_string(),
        ..RuntimeSessionState::new(crate::SessionPolicy::new(crate::TurnBudget::Unbounded))
    };
    state.set_execution_state_snapshot(Some(b"known-execution-state".to_vec()));
    let mut first_commit = RuntimeCommit::persisted_state_for_test(&state, &[]);
    first_commit.checkpoint.components.extend([
        (
            "arbitrary/unchanged".to_string(),
            crate::HydratedCheckpointComponent::changed(b"stable-body".to_vec()),
        ),
        (
            "arbitrary/duplicate-ref".to_string(),
            crate::HydratedCheckpointComponent::changed(b"stable-body".to_vec()),
        ),
        (
            "arbitrary/deleted".to_string(),
            crate::HydratedCheckpointComponent::changed(b"delete-me".to_vec()),
        ),
        (
            "arbitrary/changed".to_string(),
            crate::HydratedCheckpointComponent::changed(b"before".to_vec()),
        ),
    ]);
    let first =
        commit_runtime_state_for_test(&open, first_commit, "checkpoint-component-refs-first")
            .await
            .expect("commit checkpoint component bodies");
    let unchanged_descriptor = first.manifest.components["arbitrary/unchanged"].clone();
    let changed_before = first.manifest.components["arbitrary/changed"].clone();
    assert_eq!(
        unchanged_descriptor.blob_ref.as_str(),
        crate::stable_hash::sha256_hex(b"stable-body")
    );

    drop(open);

    let reopen = make();
    assert!(
        !std::sync::Weak::ptr_eq(&open_identity, &Arc::downgrade(&reopen)),
        "checkpoint-component reopen factory reused the writer handle"
    );
    let reopen_identity = Arc::downgrade(&reopen);
    bind_conformance_session(&reopen, "checkpoint-component-refs").await;

    // Exercise the production hydration and ordinary commit boundary. No test
    // code re-inserts arbitrary keys: the runtime-owned complete set must carry
    // them as unchanged refs.
    state = crate::store::load_persisted_session_state(reopen.as_ref())
        .await
        .expect("hydrate resident checkpoint component set")
        .expect("seeded checkpoint state");
    let mut ordinary_turn_projection = state.to_snapshot();
    ordinary_turn_projection.turn_index += 1;
    state.apply_snapshot(&ordinary_turn_projection);
    let second_commit = RuntimeCommit::persisted_state_for_test(&state, &[]);
    let carried = second_commit
        .checkpoint
        .components
        .get("arbitrary/unchanged")
        .expect("ordinary commit carries unknown key");
    assert_eq!(carried.body(), None, "unknown component must ride ref-only");
    assert_eq!(carried.blob_ref(), Some(&unchanged_descriptor.blob_ref));
    let measured = second_commit
        .measure_budget()
        .expect("measure ordinary complete-set commit");
    let root_bytes = rmp_serde::to_vec_named(
        &second_commit
            .checkpoint
            .manifest()
            .expect("project ordinary checkpoint root"),
    )
    .expect("encode ordinary checkpoint root")
    .len();
    assert_eq!(
        measured.checkpoint_bytes, root_bytes,
        "unchanged carried refs must add no component-body bytes to the budget"
    );
    let second =
        commit_runtime_state_for_test(&reopen, second_commit, "checkpoint-component-refs-second")
            .await
            .expect("commit unchanged checkpoint component refs");
    assert_eq!(
        second.manifest.components["arbitrary/unchanged"], unchanged_descriptor,
        "an unchanged arbitrary component must commit ref-only and reuse its descriptor"
    );
    assert!(
        second.manifest.components.contains_key("arbitrary/deleted"),
        "ordinary commits must retain every unknown component"
    );
    state = crate::store::load_persisted_session_state(reopen.as_ref())
        .await
        .expect("hydrate state after ordinary complete-set commit")
        .expect("ordinary complete-set checkpoint state");

    // Explicit owner mutation still uses absence from the complete listing as
    // deletion. The arbitrary store-law mutations remain direct because the
    // runtime intentionally has no typed owner for those keys.
    state.set_execution_state_snapshot(None);
    let mut third_commit = RuntimeCommit::persisted_state_for_test(&state, &[]);
    third_commit
        .checkpoint
        .components
        .remove("arbitrary/deleted");
    third_commit.checkpoint.components.insert(
        "arbitrary/changed".to_string(),
        crate::HydratedCheckpointComponent::changed(b"after".to_vec()),
    );
    let third =
        commit_runtime_state_for_test(&reopen, third_commit, "checkpoint-component-refs-third")
            .await
            .expect("commit explicit known and arbitrary component mutations");
    assert_ne!(
        third.manifest.components["arbitrary/changed"].blob_ref, changed_before.blob_ref,
        "a changed arbitrary component must mint a new content ref"
    );
    assert!(
        !third.manifest.components.contains_key("arbitrary/deleted"),
        "absence from the complete key listing is deletion"
    );
    assert!(
        !third
            .manifest
            .components
            .contains_key(crate::store::EXECUTION_STATE_CHECKPOINT_COMPONENT),
        "explicit deletion of a known component must remove it"
    );
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
        checkpoint.component_body("arbitrary/unchanged"),
        Some(&b"stable-body"[..]),
        "hydrate -> ordinary commit -> hydrate must preserve unknown bytes exactly"
    );
    assert_eq!(
        checkpoint.component_body("arbitrary/duplicate-ref"),
        Some(&b"stable-body"[..]),
        "two component keys may resolve the same deduplicated body"
    );
    assert_eq!(
        checkpoint.components["arbitrary/duplicate-ref"].blob_ref(),
        checkpoint.components["arbitrary/unchanged"].blob_ref(),
        "duplicate refs must resolve once and hydrate every owning key"
    );
    assert_eq!(
        checkpoint.components["arbitrary/unchanged"].blob_ref(),
        Some(&unchanged_descriptor.blob_ref),
        "round-trip must preserve the unknown component's content hash"
    );
    assert_eq!(
        checkpoint.component_body("arbitrary/changed"),
        Some(&b"after"[..])
    );
    assert!(!checkpoint.components.contains_key("arbitrary/deleted"));
    assert!(
        !checkpoint
            .components
            .contains_key(crate::store::EXECUTION_STATE_CHECKPOINT_COMPONENT),
        "known component deletion must survive cold hydration"
    );

    state = crate::store::load_persisted_session_state(cold_reopen.as_ref())
        .await
        .expect("reload current state before rejection laws")
        .expect("current checkpoint state before rejection laws");
    let mut unknown = RuntimeCommit::persisted_state_for_test(&state, &[]);
    unknown.checkpoint.components.insert(
        "arbitrary/unknown-ref".to_string(),
        crate::HydratedCheckpointComponent::Unchanged {
            descriptor: crate::CheckpointComponentDescriptor {
                blob_ref: crate::BlobRef("never-stored".to_string()),
                encoding_version: crate::store::CHECKPOINT_COMPONENT_ENCODING_VERSION,
            },
        },
    );
    let rejection_lease = claim_session_execution_lease_for_test(
        &cold_reopen,
        "checkpoint-component-refs",
        "checkpoint-component-rejections",
    )
    .await;
    let unknown_error = cold_reopen
        .commit_runtime_state(
            unknown.releasing_session_execution_lease(rejection_lease.completion()),
        )
        .await
        .expect_err("arbitrary unknown ref must fail");
    assert!(matches!(
        unknown_error,
        StoreError::CheckpointComponentMissing { ref key, .. }
            if key == "arbitrary/unknown-ref"
    ));

    let mut mismatch = RuntimeCommit::persisted_state_for_test(&state, &[]);
    mismatch.checkpoint.components.insert(
        "arbitrary/versioned".to_string(),
        crate::HydratedCheckpointComponent::Changed {
            encoding_version: crate::store::CHECKPOINT_COMPONENT_ENCODING_VERSION + 1,
            body: b"unsupported".to_vec(),
        },
    );
    let mismatch_error = cold_reopen
        .commit_runtime_state(
            mismatch.releasing_session_execution_lease(rejection_lease.completion()),
        )
        .await
        .expect_err("arbitrary encoding-version mismatch must fail");
    assert!(matches!(
        &mismatch_error,
        StoreError::CheckpointComponentEncodingVersionMismatch { key, .. }
            if key == "arbitrary/versioned"
    ));
    assert!(
        mismatch_error
            .to_string()
            .contains("remedy: drain affected sessions and recreate the store"),
        "typed mismatch must name the operator remedy: {mismatch_error}"
    );
    release_session_execution_lease_for_test(&cold_reopen, &rejection_lease).await;
}

/// A ref-only checkpoint commit is valid only when every referenced component
/// already exists in the backend.
pub async fn checkpoint_rejects_unknown_component_ref(store: Arc<dyn RuntimePersistence>) {
    let state = RuntimeSessionState {
        session_id: "checkpoint-unknown-ref".to_string(),
        ..RuntimeSessionState::new(crate::SessionPolicy::new(crate::TurnBudget::Unbounded))
    };
    let mut commit = RuntimeCommit::persisted_state_for_test(&state, &[]);
    commit.checkpoint.components.insert(
        "arbitrary/unknown-ref".to_string(),
        crate::HydratedCheckpointComponent::Unchanged {
            descriptor: crate::CheckpointComponentDescriptor {
                blob_ref: crate::BlobRef("checkpoint-component-that-was-never-stored".to_string()),
                encoding_version: crate::store::CHECKPOINT_COMPONENT_ENCODING_VERSION,
            },
        },
    );

    let error = commit_runtime_state_for_test(&store, commit, "checkpoint-unknown-ref")
        .await
        .expect_err("a checkpoint must reject a ref whose body is absent");
    assert!(matches!(
        &error,
        StoreError::CheckpointComponentMissing { key, blob_ref }
            if key == "arbitrary/unknown-ref"
                && blob_ref.as_str() == "checkpoint-component-that-was-never-stored"
    ));
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
        ..RuntimeSessionState::new(crate::SessionPolicy::new(crate::TurnBudget::Unbounded))
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
        ..RuntimeSessionState::new(crate::SessionPolicy::new(crate::TurnBudget::Unbounded))
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
        ))
        .await
        .expect("enqueue checkpoint queued work");
    let lease = store
        .try_claim_session_execution_lease(
            session_id,
            &owner,
            "checkpoint-work-claims-both-families-once-executor",
            60_000,
        )
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
            crate::testing::queued_work_claim_policy(10),
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
            crate::testing::queued_work_claim_policy(10),
        )
        .await
        .expect("same-generation checkpoint re-claim");
    assert!(
        second.0.is_none() && second.1.is_none(),
        "checkpoint claims must be granted exactly once per lease generation"
    );
}

/// A checkpoint claim spans pending inputs and queued work atomically. If the
/// queued head cannot fit the context window, the active-turn input must remain
/// pending and visible rather than being left accepted under a discarded claim.
async fn checkpoint_budget_refusal_preserves_active_turn_input(store: Arc<dyn RuntimePersistence>) {
    let session_id = "checkpoint-budget-atomicity";
    let turn_id = crate::TurnId::from("checkpoint-budget-atomicity:turn");
    let owner = lease_owner("checkpoint-budget-atomicity-owner");
    let input = store
        .enqueue_pending_turn_input(pending_active_turn_input_draft(
            session_id,
            turn_id.as_str(),
            crate::TurnInputCheckpointBoundary::AfterWork,
            "input that must survive a queue budget refusal",
        ))
        .await
        .expect("enqueue active-turn input for atomic checkpoint claim");
    let oversized_text = "oversized queued work".repeat(64);
    store
        .enqueue_queued_work(queued_draft(
            session_id,
            &oversized_text,
            DeliveryPolicy::EarliestSafeBoundary,
        ))
        .await
        .expect("enqueue oversized checkpoint queued work");
    let lease = store
        .try_claim_session_execution_lease(
            session_id,
            &owner,
            "checkpoint-budget-refusal-executor",
            60_000,
        )
        .await
        .expect("claim checkpoint atomicity session lease")
        .acquired()
        .expect("checkpoint atomicity session lease acquired");
    let error = store
        .claim_checkpoint_work(
            session_id,
            &lease.fence(),
            &owner,
            &turn_id,
            crate::CheckpointKind::AfterWork,
            10,
            crate::QueuedWorkClaimPolicy {
                max_context_tokens: 64,
                action_token_reserve: 1,
                max_rows: 10,
                max_pending_age_ms: 30_000,
            },
        )
        .await
        .expect_err("oversized queued row must refuse the combined checkpoint claim");
    assert!(matches!(
        error,
        StoreError::QueuedWorkRowExceedsContextWindow { .. }
    ));

    let pending = store
        .list_pending_turn_inputs(session_id)
        .await
        .expect("list active input after checkpoint budget refusal");
    assert_eq!(
        pending
            .iter()
            .map(|row| row.input_id.as_str())
            .collect::<Vec<_>>(),
        vec![input.input_id.as_str()],
        "the input claim must roll back with the refused queued-work claim"
    );
    assert_eq!(pending[0].state, crate::TurnInputState::PendingActive);
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
        .try_claim_session_execution_lease(
            session_id,
            &owner,
            "checkpoint-claim-probe-transaction-counts-executor",
            60_000,
        )
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
            crate::testing::queued_work_claim_policy(64),
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
            crate::testing::queued_work_claim_policy(64),
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
            crate::testing::queued_work_claim_policy(64),
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
            crate::testing::queued_work_claim_policy(64),
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
) -> QueuedWorkBatchDraft {
    let wake = ProcessWakeDelivery {
        version: crate::PROCESS_WAKE_DELIVERY_FORMAT_VERSION,
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
        authority: crate::QueuedWorkAuthority::default(),
        input: text.to_string(),
        created_at_ms: 1,
    };
    QueuedWorkBatchDraft::new(
        session_id,
        delivery_policy,
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
) -> QueuedWorkBatchDraft {
    QueuedWorkBatchDraft::new(
        session_id,
        delivery_policy,
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
        vec![QueuedWorkPayload::session_command(
            crate::SessionCommand::RefreshToolCatalog {
                reason: reason.to_string(),
            },
        )],
    )
    .with_kind(crate::QueuedWorkKind::Control)
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
        .try_claim_session_execution_lease(
            session_id,
            &owner,
            "claim-session-execution-lease-for-test-executor",
            60_000,
        )
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
                assignment: crate::AgentFrameAssignment::from_policy(crate::SessionPolicy::new(
                    crate::TurnBudget::Unbounded,
                )),
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
            model: ModelSpec::builder("gpt-5.4-mini")
                .context_window_tokens(200_000)
                .build()
                .expect("valid model spec"),
            ..SessionPolicy::new(crate::TurnBudget::Unbounded)
        },
        ..RuntimeSessionState::new(crate::SessionPolicy::new(crate::TurnBudget::Unbounded))
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
        read.checkpoint.as_ref().and_then(|checkpoint| {
            checkpoint.component_body(crate::store::EXECUTION_STATE_CHECKPOINT_COMPONENT)
        }),
        Some(&b"frame-vm"[..])
    );
}

async fn concurrent_head_revision_cas_applies_exactly_once(store: Arc<dyn RuntimePersistence>) {
    let session_id = "concurrent-head-cas";
    let lease = claim_session_execution_lease_for_test(&store, session_id, "cas-owner").await;
    let make_commit = |node_id: &str| {
        let state = RuntimeSessionState {
            session_id: session_id.to_string(),
            ..RuntimeSessionState::new(crate::SessionPolicy::new(crate::TurnBudget::Unbounded))
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
        ..RuntimeSessionState::new(crate::SessionPolicy::new(crate::TurnBudget::Unbounded))
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
        ..RuntimeSessionState::new(crate::SessionPolicy::new(crate::TurnBudget::Unbounded))
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
    let mut state = RuntimeSessionState {
        session_id: "hydrated".to_string(),
        plugin_snapshot_revision: Some(12),
        ..RuntimeSessionState::new(crate::SessionPolicy::new(crate::TurnBudget::Unbounded))
    };
    state.set_tool_state_snapshot(Some(ToolState::default().with_generation(9)));
    state.set_plugin_snapshot(Some(PluginSessionSnapshot {
        plugins: Default::default(),
    }));
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
            .decode_component::<ToolState>(crate::store::TOOL_STATE_CHECKPOINT_COMPONENT)
            .expect("decode dynamic snapshot")
            .expect("dynamic snapshot")
            .generation(),
        9
    );
    assert_eq!(checkpoint.plugin_snapshot_revision, Some(12));
    assert_eq!(read.token_ledger.len(), 1);
    assert_eq!(read.token_ledger[0].usage.input_tokens, 11);
}

async fn session_execution_lease_contract(store: Arc<dyn RuntimePersistence>) {
    let fresh_retry_owner = lease_owner("fresh-retry-owner");
    let fresh_retry_nonce = crate::LeaseClaimNonce::new();
    let fresh_retry = store
        .try_claim_session_execution_lease_with_token(
            "fresh-retry",
            &fresh_retry_owner,
            "fresh-retry-executor",
            &fresh_retry_nonce,
            120_000,
        )
        .await
        .expect("fresh claim with retry nonce")
        .acquired()
        .expect("fresh retry claim acquired");
    assert_eq!(fresh_retry.lease_token, fresh_retry_nonce.as_str());
    assert_eq!(fresh_retry.lease_term_ms, 120_000);
    let fresh_retried = store
        .try_claim_session_execution_lease_with_token(
            "fresh-retry",
            &fresh_retry_owner,
            "fresh-retry-executor",
            &fresh_retry_nonce,
            120_000,
        )
        .await
        .expect("retry fresh claim")
        .acquired()
        .expect("fresh claim retry remains acquired");
    assert_eq!(fresh_retried.lease_token, fresh_retry.lease_token);
    assert_eq!(fresh_retried.fencing_token, fresh_retry.fencing_token);
    assert_eq!(
        fresh_retried.claimed_at_epoch_ms, fresh_retry.claimed_at_epoch_ms,
        "a retry after an ambiguous fresh acquire must not double-bump generation or claimed-at"
    );
    release_session_execution_lease_for_test(&store, &fresh_retried).await;

    let takeover_predecessor = store
        .try_claim_session_execution_lease(
            "takeover-retry",
            &lease_owner("takeover-old"),
            "session-execution-lease-contract-executor",
            0,
        )
        .await
        .expect("claim immediately-expiring takeover predecessor")
        .acquired()
        .expect("takeover predecessor acquired");
    let takeover_owner = lease_owner("takeover-new");
    let takeover_nonce = crate::LeaseClaimNonce::new();
    let takeover = store
        .try_claim_session_execution_lease_with_token(
            "takeover-retry",
            &takeover_owner,
            "takeover-new-executor",
            &takeover_nonce,
            120_000,
        )
        .await
        .expect("take over expired lease")
        .acquired()
        .expect("takeover acquired");
    assert!(takeover.fencing_token > takeover_predecessor.fencing_token);
    assert_eq!(takeover.lease_token, takeover_nonce.as_str());
    let takeover_retried = store
        .try_claim_session_execution_lease_with_token(
            "takeover-retry",
            &takeover_owner,
            "takeover-new-executor",
            &takeover_nonce,
            120_000,
        )
        .await
        .expect("retry takeover")
        .acquired()
        .expect("takeover retry remains acquired");
    assert_eq!(takeover_retried.lease_token, takeover.lease_token);
    assert_eq!(takeover_retried.fencing_token, takeover.fencing_token);
    assert_eq!(
        takeover_retried.claimed_at_epoch_ms, takeover.claimed_at_epoch_ms,
        "a retry after an ambiguous takeover must not double-bump generation or claimed-at"
    );
    release_session_execution_lease_for_test(&store, &takeover_retried).await;

    let owner_a = lease_owner("owner-a");
    let first_nonce = crate::LeaseClaimNonce::for_testing("owner-a-first-token");
    let first = store
        .try_claim_session_execution_lease_with_token(
            "root",
            &owner_a,
            "owner-a-executor",
            &first_nonce,
            120_000,
        )
        .await
        .expect("owner A first claim")
        .acquired()
        .expect("owner A first claim acquired");
    let owner_a_next = crate::LeaseOwnerIdentity::opaque("owner-a", "owner-a:next-incarnation");
    let owner_b = lease_owner("owner-b");
    let owner_c = lease_owner("owner-c");
    let owner_expired = lease_owner("owner-expired");
    let reentry_nonce = crate::LeaseClaimNonce::new();
    let reentered = store
        .try_claim_session_execution_lease_with_token(
            "root",
            &owner_a,
            "owner-a-executor",
            &reentry_nonce,
            130_000,
        )
        .await
        .expect("same incarnation may re-enter live session lease")
        .acquired()
        .expect("same incarnation receives existing session lease");
    assert_ne!(
        reentered.lease_token, first.lease_token,
        "every same-incarnation claim must rotate the lock-lifecycle token"
    );
    assert_eq!(reentered.fencing_token, first.fencing_token);
    assert_eq!(reentered.lease_token, reentry_nonce.as_str());
    assert_eq!(reentered.lease_term_ms, 130_000);
    assert_eq!(
        reentered.claimed_at_epoch_ms, first.claimed_at_epoch_ms,
        "same-incarnation rotation preserves when the lane was first acquired"
    );
    assert!(reentered.expires_at_epoch_ms >= first.expires_at_epoch_ms);
    let retried = store
        .try_claim_session_execution_lease_with_token(
            "root",
            &owner_a,
            "owner-a-executor",
            &reentry_nonce,
            140_000,
        )
        .await
        .expect("retry same claim attempt")
        .acquired()
        .expect("retry observes the claim it already rotated");
    assert_eq!(retried.lease_token, reentered.lease_token);
    assert_eq!(retried.fencing_token, reentered.fencing_token);
    assert_eq!(retried.claimed_at_epoch_ms, reentered.claimed_at_epoch_ms);
    assert_eq!(retried.lease_term_ms, 140_000);
    assert!(
        matches!(
            store
                .try_claim_session_execution_lease(
                    "root",
                    &owner_a_next,
                    "session-execution-lease-contract-executor-2",
                    60_000
                )
                .await
                .expect("try same owner next incarnation"),
            crate::SessionExecutionLeaseClaimOutcome::Busy { .. }
        ),
        "a live session execution lease must exclude the same owner in a different incarnation"
    );
    assert!(
        matches!(
            store
                .try_claim_session_execution_lease(
                    "root",
                    &owner_b,
                    "session-execution-lease-contract-executor-3",
                    60_000
                )
                .await
                .expect("try concurrent session lease"),
            crate::SessionExecutionLeaseClaimOutcome::Busy { .. }
        ),
        "a live session execution lease must exclude concurrent owners"
    );
    let renewed = store
        .renew_session_execution_lease(&reentered.fence(), 150_000)
        .await
        .expect("renew live session lease");
    assert_eq!(renewed.session_id, reentered.session_id);
    assert_eq!(renewed.owner, reentered.owner);
    assert_eq!(renewed.lease_token, reentered.lease_token);
    assert_eq!(renewed.fencing_token, reentered.fencing_token);
    assert_eq!(renewed.lease_term_ms, 150_000);
    assert!(renewed.expires_at_epoch_ms >= reentered.expires_at_epoch_ms);
    let mut lock_lifecycle_authority = reentered.fence();
    lock_lifecycle_authority.fencing_token = lock_lifecycle_authority
        .fencing_token
        .saturating_add(10_000);
    let renewed_by_owner_and_token = store
        .renew_session_execution_lease(&lock_lifecycle_authority, 120_000)
        .await
        .expect("renewal lock lifecycle is predicated only on owner and lease token");
    assert_eq!(
        renewed_by_owner_and_token.fencing_token, reentered.fencing_token,
        "renewal returns the durable generation rather than trusting caller input"
    );

    let mut wrong_owner_current_token = reentered.fence();
    wrong_owner_current_token.owner = owner_b.clone();
    let err = store
        .renew_session_execution_lease(&wrong_owner_current_token, 120_000)
        .await
        .expect_err("the current token alone must not authorize a wrong owner renewal");
    assert!(matches!(
        err,
        StoreError::SessionExecutionLeaseRenewalRefused { .. }
    ));
    let err = store
        .release_session_execution_lease(&wrong_owner_current_token)
        .await
        .expect_err("the current token alone must not authorize a wrong owner release");
    assert!(matches!(
        err,
        StoreError::SessionExecutionLeaseReleaseRefused { .. }
    ));

    let mut stale_fence = reentered.fence();
    stale_fence.lease_token.push_str(":stale");
    let err = store
        .renew_session_execution_lease(&stale_fence, 60_000)
        .await
        .expect_err("stale session lease renew must fail");
    assert!(matches!(
        err,
        StoreError::SessionExecutionLeaseRenewalRefused { .. }
    ));
    let err = store
        .release_session_execution_lease(&crate::SessionExecutionLeaseAuthority {
            session_id: first.session_id.clone(),
            owner: first.owner.clone(),
            executor_id: "owner-a-executor".to_string(),
            lease_token: format!("{}:stale", first.lease_token),
            fencing_token: first.fencing_token,
        })
        .await
        .expect_err("stale release must be refused by name");
    assert!(matches!(
        err,
        StoreError::SessionExecutionLeaseReleaseRefused { .. }
    ));
    assert!(
        matches!(
            store
                .try_claim_session_execution_lease(
                    "root",
                    &owner_b,
                    "session-execution-lease-contract-executor-4",
                    60_000
                )
                .await
                .expect("try after stale release"),
            crate::SessionExecutionLeaseClaimOutcome::Busy { .. }
        ),
        "stale release must not clear the live lease"
    );
    // The token scopes the lock lifecycle: a completion retained by the prior
    // holder no longer identifies the successor claim, even though the owner
    // incarnation and fencing generation are unchanged.
    let retained_stale_completion = first.completion();
    let err = store
        .release_session_execution_lease(&retained_stale_completion)
        .await
        .expect_err("a retained predecessor completion must be refused by name");
    assert!(matches!(
        err,
        StoreError::SessionExecutionLeaseReleaseRefused { .. }
    ));
    assert!(
        matches!(
            store
                .try_claim_session_execution_lease(
                    "root",
                    &owner_b,
                    "session-execution-lease-contract-executor-5",
                    60_000
                )
                .await
                .expect("claim after stale retained release"),
            crate::SessionExecutionLeaseClaimOutcome::Busy { .. }
        ),
        "a retained predecessor completion must not free the successor claim"
    );

    let mut current_completion = reentered.completion();
    current_completion.fencing_token = current_completion.fencing_token.saturating_add(10_000);
    store
        .release_session_execution_lease(&current_completion)
        .await
        .expect("release lock lifecycle is predicated only on owner and lease token");
    let err = store
        .release_session_execution_lease(&current_completion)
        .await
        .expect_err("repeating an acknowledged release must be refused by name");
    assert!(matches!(
        err,
        StoreError::SessionExecutionLeaseReleaseRefused { .. }
    ));
    let second = claim_session_execution_lease_for_test(&store, "root", "owner-b").await;
    assert!(
        second.fencing_token > first.fencing_token,
        "reclaimed session leases must advance the fencing token"
    );
    let err = store
        .release_session_execution_lease(&first.completion())
        .await
        .expect_err("old release must be refused by name");
    assert!(matches!(
        err,
        StoreError::SessionExecutionLeaseReleaseRefused { .. }
    ));
    assert!(
        matches!(
            store
                .try_claim_session_execution_lease(
                    "root",
                    &owner_c,
                    "session-execution-lease-contract-executor-6",
                    60_000
                )
                .await
                .expect("try after old release"),
            crate::SessionExecutionLeaseClaimOutcome::Busy { .. }
        ),
        "old release must not clear a newer lease"
    );
    release_session_execution_lease_for_test(&store, &second).await;

    let expired = store
        .try_claim_session_execution_lease(
            "root",
            &owner_expired,
            "session-execution-lease-contract-executor-7",
            0,
        )
        .await
        .expect("claim expiring lease")
        .acquired()
        .expect("expiring lease");
    let reclaimed = claim_session_execution_lease_for_test(&store, "root", "owner-reclaim").await;
    assert!(reclaimed.fencing_token > expired.fencing_token);
    release_session_execution_lease_for_test(&store, &reclaimed).await;

    let mut state = RuntimeSessionState {
        session_id: "root".to_string(),
        ..RuntimeSessionState::new(crate::SessionPolicy::new(crate::TurnBudget::Unbounded))
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
        ..RuntimeSessionState::new(crate::SessionPolicy::new(crate::TurnBudget::Unbounded))
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
        ))
        .await
        .expect("enqueue fenced queue work");
    let err = store
        .claim_ready_queued_work(
            "root",
            &commit_lease.fence(),
            &lease_owner("queue-owner"),
            QueuedWorkClaimBoundary::Idle,
            crate::testing::queued_work_claim_policy(1),
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
            crate::testing::queued_work_claim_policy(1),
        )
        .await
        .expect("claim fenced queue work")
        .expect("queue work claim");
    assert_eq!(claim.batches[0].batch_id, batch.batch_id);
    release_session_execution_lease_for_test(&store, &queue_lease).await;
}

/// A borrowed commit validates the ordinary current-token fence without
/// participating in the lease lifecycle. Run through the shared conformance
/// suite so in-memory, SQLite, PostgreSQL, and perf backends cannot drift.
pub async fn borrowed_session_execution_lease_commit_contract(store: Arc<dyn RuntimePersistence>) {
    let session_id = "borrowed-commit-fence";
    let owner = lease_owner("borrowed-commit-owner");
    let first_nonce = crate::LeaseClaimNonce::for_testing("borrowed-commit-first-token");
    let held = store
        .try_claim_session_execution_lease_with_token(
            session_id,
            &owner,
            "borrowed-commit-executor",
            &first_nonce,
            120_000,
        )
        .await
        .expect("claim borrowed-commit lane")
        .acquired()
        .expect("borrowed-commit lane acquired");
    let mut state = RuntimeSessionState {
        session_id: session_id.to_string(),
        ..RuntimeSessionState::new(crate::SessionPolicy::new(crate::TurnBudget::Unbounded))
    };
    let operation = crate::OperationId::new(
        crate::ExecutionScope::runtime_operation("borrowed-commit-replay"),
        "commit",
    );
    let same_operation =
        RuntimeCommit::persisted_state_with_operation_for_testing(&state, &[], operation);
    let committed = store
        .commit_runtime_state(
            same_operation
                .clone()
                .borrowing_session_execution_lease(held.fence()),
        )
        .await
        .expect("borrowed commit accepts the current held authority");
    state.head_revision = committed.head_revision;

    let renewed = store
        .renew_session_execution_lease(&held.fence(), 120_000)
        .await
        .expect("borrowed commit leaves the outer guard fence-valid");
    assert_eq!(renewed.lease_token, held.lease_token);
    assert_eq!(renewed.fencing_token, held.fencing_token);

    let replay_successor = store
        .try_claim_session_execution_lease_with_token(
            session_id,
            &owner,
            "borrowed-commit-executor",
            &crate::LeaseClaimNonce::for_testing("borrowed-commit-replay-token"),
            120_000,
        )
        .await
        .expect("rotate the fence before replaying the same operation")
        .acquired()
        .expect("same-incarnation replay rotation acquired");
    let error = store
        .commit_runtime_state(same_operation.borrowing_session_execution_lease(held.fence()))
        .await
        .expect_err("a stale borrowed fence must veto receipt replay");
    assert!(matches!(
        error,
        StoreError::SessionExecutionLeaseExpired { .. }
    ));
    assert_ne!(replay_successor.lease_token, held.lease_token);

    let lapsed = store
        .try_claim_session_execution_lease_with_token(
            session_id,
            &owner,
            "borrowed-commit-executor",
            &crate::LeaseClaimNonce::for_testing("borrowed-commit-lapsed-token"),
            0,
        )
        .await
        .expect("rotate to an immediately lapsed borrowed-commit lane")
        .acquired()
        .expect("same-incarnation lapsed lane acquired");
    let error = store
        .commit_runtime_state(
            RuntimeCommit::persisted_state_for_test(&state, &[])
                .borrowing_session_execution_lease(lapsed.fence()),
        )
        .await
        .expect_err("a lapsed guard cannot authorize a borrowed commit");
    assert!(matches!(
        error,
        StoreError::SessionExecutionLeaseExpired { .. }
    ));

    let rotated = store
        .try_claim_session_execution_lease_with_token(
            session_id,
            &owner,
            "borrowed-commit-executor",
            &crate::LeaseClaimNonce::for_testing("borrowed-commit-rotated-token"),
            120_000,
        )
        .await
        .expect("rotate borrowed-commit lane")
        .acquired()
        .expect("same-incarnation rotation acquired");
    let error = store
        .commit_runtime_state(
            RuntimeCommit::persisted_state_for_test(&state, &[])
                .borrowing_session_execution_lease(held.fence()),
        )
        .await
        .expect_err("a stale outer guard cannot authorize a borrowed commit");
    assert!(matches!(
        error,
        StoreError::SessionExecutionLeaseExpired { .. }
    ));
    let after_rejection = store
        .get_session_execution_lease(session_id)
        .await
        .expect("read lane after stale borrow rejection")
        .expect("stale borrow rejection leaves successor live");
    assert_eq!(after_rejection.lease_token, rotated.lease_token);
    release_session_execution_lease_for_test(&store, &rotated).await;
}

async fn same_incarnation_rotation_gates_claims_not_commits(store: Arc<dyn RuntimePersistence>) {
    let mut state = RuntimeSessionState {
        session_id: "root".to_string(),
        ..RuntimeSessionState::new(crate::SessionPolicy::new(crate::TurnBudget::Unbounded))
    };
    let overlap_owner = lease_owner("same-incarnation-overlap");
    let overlap_predecessor_nonce = crate::LeaseClaimNonce::new();
    let overlap_predecessor = store
        .try_claim_session_execution_lease_with_token(
            "root",
            &overlap_owner,
            "same-incarnation-executor",
            &overlap_predecessor_nonce,
            60_000,
        )
        .await
        .expect("claim overlap predecessor")
        .acquired()
        .expect("overlap predecessor acquired");
    let overlap_successor_nonce = crate::LeaseClaimNonce::new();
    let overlap_successor = store
        .try_claim_session_execution_lease_with_token(
            "root",
            &overlap_owner,
            "same-incarnation-executor",
            &overlap_successor_nonce,
            60_000,
        )
        .await
        .expect("claim overlap successor")
        .acquired()
        .expect("same incarnation overlap successor acquired");
    assert_eq!(
        overlap_successor.fencing_token, overlap_predecessor.fencing_token,
        "same-incarnation overlap must preserve the claim generation"
    );

    let overlap_win = store
        .commit_runtime_state(
            RuntimeCommit::persisted_state_for_test(&state, &[])
                .releasing_session_execution_lease(overlap_predecessor.completion()),
        )
        .await
        .expect("predecessor may win purely by the current-head CAS");
    state.head_revision = overlap_win.head_revision;
    let live_after_win = store
        .get_session_execution_lease("root")
        .await
        .expect("read overlap successor after predecessor win")
        .expect("stale predecessor release must leave successor live");
    assert_eq!(live_after_win.lease_token, overlap_successor.lease_token);

    let stale_state = RuntimeSessionState {
        session_id: "root".to_string(),
        ..RuntimeSessionState::new(crate::SessionPolicy::new(crate::TurnBudget::Unbounded))
    };
    let err = store
        .commit_runtime_state(
            RuntimeCommit::persisted_state_for_test(&stale_state, &[])
                .releasing_session_execution_lease(overlap_predecessor.completion()),
        )
        .await
        .expect_err("predecessor may lose only because the head CAS is stale");
    assert!(matches!(err, StoreError::HeadRevisionConflict { .. }));
    let live_after_loss = store
        .get_session_execution_lease("root")
        .await
        .expect("read overlap successor after predecessor loss")
        .expect("CAS-losing predecessor must leave successor live");
    assert_eq!(live_after_loss.lease_token, overlap_successor.lease_token);
    release_session_execution_lease_for_test(&store, &overlap_successor).await;
}

/// One host may open the same session more than once. The runtime-minted
/// executor discriminator keeps those opens out of the reentry arm while the
/// stable host owner remains shared. This law runs unchanged on in-memory,
/// SQLite, PostgreSQL, and the perf conformance backend.
pub async fn same_host_distinct_executors_are_lane_less_without_revoking_holder(
    store: Arc<dyn RuntimePersistence>,
) {
    let owner = crate::LeaseOwnerIdentity::opaque("fig1133-host", "fig1133-boot");
    let first_nonce = crate::LeaseClaimNonce::for_testing("fig1133-first-token");
    let first = store
        .try_claim_session_execution_lease_with_token(
            "fig1133-same-host-session",
            &owner,
            "fig1133-executor-a",
            &first_nonce,
            120_000,
        )
        .await
        .expect("first executor claim")
        .acquired()
        .expect("first executor acquires the lane");
    assert_eq!(first.session_id, "fig1133-same-host-session");
    assert_eq!(first.owner.owner_id, "fig1133-host");
    assert_eq!(first.owner.incarnation_id, "fig1133-boot");
    assert_eq!(first.executor_id, "fig1133-executor-a");
    assert_eq!(first.lease_token, "fig1133-first-token");
    assert_eq!(first.fencing_token, 1);

    let second_nonce = crate::LeaseClaimNonce::for_testing("fig1133-second-token");
    let holder = match store
        .try_claim_session_execution_lease_with_token(
            "fig1133-same-host-session",
            &owner,
            "fig1133-executor-b",
            &second_nonce,
            120_000,
        )
        .await
        .expect("second executor receives a typed claim outcome")
    {
        crate::SessionExecutionLeaseClaimOutcome::Busy { holder } => holder,
        crate::SessionExecutionLeaseClaimOutcome::Acquired(_) => {
            panic!("second same-host executor must be lane-less")
        }
    };
    assert_eq!(holder.session_id, "fig1133-same-host-session");
    assert_eq!(holder.owner.owner_id, "fig1133-host");
    assert_eq!(holder.owner.incarnation_id, "fig1133-boot");
    assert_eq!(holder.executor_id, "fig1133-executor-a");
    assert_eq!(holder.lease_token, "fig1133-first-token");
    assert_eq!(holder.fencing_token, 1);
    let holder_after_busy = store
        .get_session_execution_lease("fig1133-same-host-session")
        .await
        .expect("read holder after busy result")
        .expect("busy result leaves holder row present");
    assert_eq!(holder_after_busy, first);

    let renewed = store
        .renew_session_execution_lease(&first.fence(), 120_000)
        .await
        .expect("the first holder renews after the second executor is refused");
    assert_eq!(renewed.owner.owner_id, "fig1133-host");
    assert_eq!(renewed.owner.incarnation_id, "fig1133-boot");
    assert_eq!(renewed.executor_id, "fig1133-executor-a");
    assert_eq!(renewed.lease_token, "fig1133-first-token");
    assert_eq!(renewed.fencing_token, 1);

    let mut committed_state = RuntimeSessionState {
        session_id: "fig1133-same-host-session".to_string(),
        ..RuntimeSessionState::new(crate::SessionPolicy::new(crate::TurnBudget::Unbounded))
    };
    let fenced_commit = RuntimeCommit::persisted_state_with_operation_for_testing(
        &committed_state,
        &[],
        crate::OperationId::new(
            crate::ExecutionScope::runtime_operation("fig1133-holder-mid-turn"),
            "commit",
        ),
    );
    let fenced_result = store
        .commit_runtime_state(fenced_commit.borrowing_session_execution_lease(first.fence()))
        .await
        .expect("first holder's fenced mid-turn commit remains authorized");
    assert_eq!(fenced_result.head_revision, 1);
    committed_state.head_revision = 1;

    let make_lane_less_commit = |executor: &'static str| {
        RuntimeCommit::persisted_state_with_operation_for_testing(
            &committed_state,
            &[],
            crate::OperationId::new(
                crate::ExecutionScope::runtime_operation(format!("fig1133-lane-less-{executor}")),
                "commit",
            ),
        )
    };
    let barrier = Arc::new(tokio::sync::Barrier::new(3));
    let first_store = Arc::clone(&store);
    let second_store = Arc::clone(&store);
    let first_barrier = Arc::clone(&barrier);
    let second_barrier = Arc::clone(&barrier);
    let first_commit = make_lane_less_commit("a");
    let second_commit = make_lane_less_commit("b");
    let first_writer = crate::task::spawn(async move {
        first_barrier.wait().await;
        first_store.commit_runtime_state(first_commit).await
    });
    let second_writer = crate::task::spawn(async move {
        second_barrier.wait().await;
        second_store.commit_runtime_state(second_commit).await
    });
    barrier.wait().await;
    let first_result = first_writer.await.expect("join first lane-less writer");
    let second_result = second_writer.await.expect("join second lane-less writer");
    let results = [first_result, second_result];
    assert_eq!(results.iter().filter(|result| result.is_ok()).count(), 1);
    assert_eq!(
        results
            .iter()
            .filter(|result| matches!(
                result,
                Err(StoreError::HeadRevisionConflict {
                    expected: 1,
                    actual: 2
                })
            ))
            .count(),
        1
    );
}

/// Probe the claim/renew race through the public API on every backend. Embedded
/// stores serialize the operations under one writer lock; PostgreSQL uses the
/// same per-session advisory lock. Either linearization is legal, but a renewal
/// that runs after rotation must return the named refusal rather than fabricate
/// success for the stale token.
async fn concurrent_session_execution_lease_rotation_and_stale_renewal_are_linearizable(
    store: Arc<dyn RuntimePersistence>,
) {
    let session_id = "concurrent-rotation-renewal";
    let owner = lease_owner("concurrent-rotation-owner");
    let predecessor_nonce = crate::LeaseClaimNonce::for_testing("concurrent-predecessor-token");
    let predecessor = store
        .try_claim_session_execution_lease_with_token(
            session_id,
            &owner,
            "concurrent-claim-executor",
            &predecessor_nonce,
            120_000,
        )
        .await
        .expect("claim concurrent predecessor")
        .acquired()
        .expect("concurrent predecessor acquired");
    let successor_nonce = crate::LeaseClaimNonce::new();
    let successor_token = successor_nonce.as_str().to_string();
    let barrier = std::sync::Arc::new(tokio::sync::Barrier::new(3));

    let claim_store = Arc::clone(&store);
    let claim_owner = owner.clone();
    let claim_barrier = Arc::clone(&barrier);
    let claim = crate::task::spawn(async move {
        claim_barrier.wait().await;
        claim_store
            .try_claim_session_execution_lease_with_token(
                session_id,
                &claim_owner,
                "concurrent-claim-executor",
                &successor_nonce,
                120_000,
            )
            .await
    });
    let renew_store = Arc::clone(&store);
    let predecessor_fence = predecessor.fence();
    let renew_barrier = Arc::clone(&barrier);
    let renewal = crate::task::spawn(async move {
        renew_barrier.wait().await;
        renew_store
            .renew_session_execution_lease(&predecessor_fence, 120_000)
            .await
    });
    barrier.wait().await;

    let successor = claim
        .await
        .expect("join concurrent rotating claim")
        .expect("concurrent rotating claim")
        .acquired()
        .expect("same-incarnation rotating claim acquired");
    assert_eq!(successor.lease_token, successor_token);
    match renewal.await.expect("join concurrent stale renewal") {
        Ok(renewed) => assert_eq!(
            renewed.lease_token, predecessor.lease_token,
            "a successful renewal must have linearized before token rotation"
        ),
        Err(StoreError::SessionExecutionLeaseRenewalRefused { .. }) => {}
        Err(error) => panic!("concurrent stale renewal returned the wrong error: {error}"),
    }
    let durable = store
        .get_session_execution_lease(session_id)
        .await
        .expect("read durable lease after concurrent probe")
        .expect("successor remains live after concurrent probe");
    assert_eq!(durable.lease_token, successor.lease_token);
    release_session_execution_lease_for_test(&store, &successor).await;
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
            .try_claim_session_execution_lease(
                &session_id,
                &holder_owner,
                "session-execution-lease-expires-by-ttl-contract-executor",
                lease_timing.scaffolding_lease_ttl_ms(),
            )
            .await
            .expect("claim stale-holder lease")
            .acquired()
            .expect("stale-holder lease acquired");

        lease_timing.advance_to_just_before_semantic_expiry();
        let outcome = store
            .try_claim_session_execution_lease(
                &session_id,
                &claimant,
                "session-execution-lease-expires-by-ttl-contract-executor-2",
                60_000,
            )
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
        lease_timing.scaffolding_lease_ttl_ms(),
        REALTIME_LEASE_OBSERVATION_ATTEMPTS
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
            .try_claim_session_execution_lease(
                session_id,
                claimant,
                "claim-session-execution-lease-until-acquired-executor",
                60_000,
            )
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

async fn claim_queued_work_under_short_lease(
    store: &Arc<dyn RuntimePersistence>,
    session_id: &str,
    owner: &crate::LeaseOwnerIdentity,
    lease_timing: &RuntimePersistenceLeaseTiming,
) -> (crate::SessionExecutionLease, crate::QueuedWorkClaim) {
    let deadline = std::time::Instant::now() + REALTIME_LEASE_STALL_ALLOWANCE;
    loop {
        let lease = store
            .try_claim_session_execution_lease(
                session_id,
                owner,
                "claim-queued-work-under-short-lease-executor",
                lease_timing.scaffolding_lease_ttl_ms(),
            )
            .await
            .expect("claim dead-owner lease")
            .acquired()
            .expect("dead-owner lease acquired");
        match store
            .claim_ready_queued_work(
                session_id,
                &lease.fence(),
                owner,
                QueuedWorkClaimBoundary::Idle,
                crate::testing::queued_work_claim_policy(10),
            )
            .await
        {
            Ok(Some(claim)) => return (lease, claim),
            Err(StoreError::SessionExecutionLeaseExpired { .. })
                if matches!(lease_timing, RuntimePersistenceLeaseTiming::Realtime)
                    && std::time::Instant::now() < deadline => {}
            Ok(None) => panic!("dead-owner queued-work claim must exist"),
            Err(error) => panic!("dead-owner queued-work claim: {error}"),
        }
    }
}

async fn claim_turn_input_under_short_lease(
    store: &Arc<dyn RuntimePersistence>,
    session_id: &str,
    owner: &crate::LeaseOwnerIdentity,
    lease_timing: &RuntimePersistenceLeaseTiming,
) -> (crate::SessionExecutionLease, crate::TurnInputClaim) {
    let deadline = std::time::Instant::now() + REALTIME_LEASE_STALL_ALLOWANCE;
    loop {
        let lease = store
            .try_claim_session_execution_lease(
                session_id,
                owner,
                "claim-turn-input-under-short-lease-executor",
                lease_timing.scaffolding_lease_ttl_ms(),
            )
            .await
            .expect("claim dead-owner lease")
            .acquired()
            .expect("dead-owner lease acquired");
        match store
            .claim_next_turn_inputs(session_id, &lease.fence(), owner, 10)
            .await
        {
            Ok(Some(claim)) => return (lease, claim),
            Err(StoreError::SessionExecutionLeaseExpired { .. })
                if matches!(lease_timing, RuntimePersistenceLeaseTiming::Realtime)
                    && std::time::Instant::now() < deadline => {}
            Ok(None) => panic!("dead-owner next-turn claim must exist"),
            Err(error) => panic!("dead-owner next-turn claim: {error}"),
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
        .try_claim_session_execution_lease(
            "lease-diagnostics",
            &lease_owner("diag-lapsed"),
            "session-execution-lease-diagnostic-read-contract-executor",
            0,
        )
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
        .try_claim_session_execution_lease(
            session_id,
            &first,
            "session-execution-lease-displacement-executor",
            0,
        )
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
        .try_claim_session_execution_lease(
            session_id,
            &second,
            "session-execution-lease-displacement-executor-2",
            60_000,
        )
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

    // Exact same-executor reentry advances nothing, so it displaces nobody.
    let reentry_nonce = crate::LeaseClaimNonce::for_testing("displacement-reentry-token");
    let reentry = store
        .try_claim_session_execution_lease_with_token(
            session_id,
            &second,
            &takeover.lease.executor_id,
            &reentry_nonce,
            60_000,
        )
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
        .try_claim_session_execution_lease(
            session_id,
            &first,
            "session-execution-lease-displacement-executor-3",
            60_000,
        )
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

/// Prove the core-owned execution-fence predicate through a backend's real
/// load/lock/claim path.
///
/// The same vector runs against in-memory, SQLite, PostgreSQL, and the perf
/// store so no implementation can weaken one term locally. Queued-work and
/// leading-command paths receive this coverage transitively through their
/// shared fence-ensure helper rather than duplicating the vector three times.
pub async fn session_execution_lease_fence_authority(store: &dyn RuntimePersistence) {
    let session_id = "lease-fence-authority";
    let owner = lease_owner("lease-fence-owner");
    store
        .enqueue_pending_turn_input(pending_next_turn_input_draft(
            session_id,
            "lease fence input",
        ))
        .await
        .expect("enqueue input behind the lease fence");
    let predecessor = store
        .try_claim_session_execution_lease_with_token(
            session_id,
            &owner,
            "lease-fence-executor",
            &crate::LeaseClaimNonce::for_testing("lease-fence-predecessor-token"),
            60_000,
        )
        .await
        .expect("claim fence predecessor")
        .acquired()
        .expect("fence predecessor acquired");
    let successor = store
        .try_claim_session_execution_lease_with_token(
            session_id,
            &owner,
            "lease-fence-executor",
            &crate::LeaseClaimNonce::for_testing("lease-fence-successor-token"),
            60_000,
        )
        .await
        .expect("rotate fence token for the same owner")
        .acquired()
        .expect("same-owner successor acquired");
    assert_eq!(predecessor.fencing_token, successor.fencing_token);
    assert_ne!(predecessor.lease_token, successor.lease_token);

    let stale_token = store
        .claim_next_turn_inputs(session_id, &predecessor.fence(), &owner, 1)
        .await
        .expect_err("a retained guard must be rejected after same-owner token rotation");
    assert!(matches!(
        stale_token,
        StoreError::SessionExecutionLeaseExpired { .. }
    ));

    let mut stale_incarnation = successor.fence();
    stale_incarnation.owner.incarnation_id.push_str(":stale");
    let stale_incarnation = store
        .claim_next_turn_inputs(session_id, &stale_incarnation, &owner, 1)
        .await
        .expect_err("a stale holder incarnation must be rejected");
    assert!(matches!(
        stale_incarnation,
        StoreError::SessionExecutionLeaseExpired { .. }
    ));

    store
        .release_session_execution_lease(&successor.completion())
        .await
        .expect("release live successor before expiry case");
    let expired = store
        .try_claim_session_execution_lease(
            session_id,
            &lease_owner("lease-fence-expired"),
            "session-execution-lease-fence-authority-executor",
            0,
        )
        .await
        .expect("claim immediately expired fence")
        .acquired()
        .expect("immediately expired fence acquired");
    let expired_error = store
        .claim_next_turn_inputs(session_id, &expired.fence(), &expired.owner, 1)
        .await
        .expect_err("an expired lease must be rejected");
    assert!(matches!(
        expired_error,
        StoreError::SessionExecutionLeaseExpired { .. }
    ));

    let current = store
        .try_claim_session_execution_lease(
            session_id,
            &lease_owner("lease-fence-current"),
            "session-execution-lease-fence-authority-executor-2",
            60_000,
        )
        .await
        .expect("claim current fence")
        .acquired()
        .expect("current fence acquired");
    let claim = store
        .claim_next_turn_inputs(session_id, &current.fence(), &current.owner, 1)
        .await
        .expect("the current-token holder must be accepted")
        .expect("the current-token holder claims the pending input");
    assert_eq!(claim.inputs.len(), 1);
    store
        .release_session_execution_lease(&current.completion())
        .await
        .expect("release current fence after acceptance case");
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
    )
    .expect("branch fixture graph is valid");
    let state = RuntimeSessionState {
        session_id: "branchy".to_string(),
        current_frame_node_id: Some(root_node_id.clone()),
        session_graph: graph,
        ..RuntimeSessionState::new(crate::SessionPolicy::new(crate::TurnBudget::Unbounded))
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
        ..RuntimeSessionState::new(crate::SessionPolicy::new(crate::TurnBudget::Unbounded))
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
            queued_draft("root", "first", DeliveryPolicy::EarliestSafeBoundary)
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
        ))
        .await
        .expect("enqueue second batch");
    store
        .enqueue_queued_work(queued_draft(
            "other",
            "other session",
            DeliveryPolicy::EarliestSafeBoundary,
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
                crate::testing::queued_work_claim_policy(1),
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
                crate::testing::queued_work_claim_policy(1),
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
            crate::testing::queued_work_claim_policy(1),
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
        ))
        .await
        .expect("enqueue first batch");
    let second = store
        .enqueue_queued_work(queued_draft(
            "root",
            "second",
            DeliveryPolicy::AfterCurrentTurnCommit,
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
                crate::testing::queued_work_claim_policy(64),
            )
            .await
            .expect("boundary-gated exact claim")
            .is_none(),
        "exact selection must preserve the delivery boundary gate"
    );
    let exclusive_prefix = store
        .claim_ready_queued_work_by_batch_ids(
            "root",
            &selected_session_lease.fence(),
            &lease_owner("owner"),
            QueuedWorkClaimBoundary::Idle,
            &[first.batch_id.clone(), second.batch_id.clone()],
            crate::testing::queued_work_claim_policy(64),
        )
        .await
        .expect("claim exclusive exact prefix")
        .expect("the first exclusive exact batch is claimable");
    assert_eq!(
        exclusive_prefix
            .batches
            .iter()
            .map(|batch| batch.enqueue_seq)
            .collect::<Vec<_>>(),
        vec![1],
        "exact selection must take only the maximal valid physical prefix"
    );
    store
        .abandon_queued_work_claim(&exclusive_prefix)
        .await
        .expect("abandon exclusive exact-prefix probe");
    let selected = store
        .claim_ready_queued_work_by_batch_ids(
            "root",
            &selected_session_lease.fence(),
            &lease_owner("owner"),
            QueuedWorkClaimBoundary::Idle,
            std::slice::from_ref(&second.batch_id),
            crate::testing::queued_work_claim_policy(64),
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
        ..RuntimeSessionState::new(crate::SessionPolicy::new(crate::TurnBudget::Unbounded))
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
    let already_settled = store
        .claim_ready_queued_work_by_batch_ids(
            "root",
            &accepted_session_lease.fence(),
            &lease_owner("owner"),
            QueuedWorkClaimBoundary::Idle,
            std::slice::from_ref(&second.batch_id),
            crate::testing::queued_work_claim_policy(64),
        )
        .await
        .expect("resolve already-settled exact batch");
    assert!(
        already_settled.claim.is_none(),
        "an already-settled selected ID must not acquire another claim"
    );
    assert_eq!(
        already_settled.already_satisfied_batch_ids,
        vec![second.batch_id.clone()],
        "an already-settled selected ID is idempotently satisfied"
    );
    let claim = store
        .claim_ready_queued_work_by_batch_ids(
            "root",
            &accepted_session_lease.fence(),
            &lease_owner("owner"),
            QueuedWorkClaimBoundary::Idle,
            std::slice::from_ref(&first.batch_id),
            crate::testing::queued_work_claim_policy(64),
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

#[doc(hidden)]
pub async fn queued_work_exact_claim_preserves_physical_order_and_key_breaks(
    store: Arc<dyn RuntimePersistence>,
) {
    let a1 = store
        .enqueue_queued_work(
            queued_draft(
                "exact-key-break",
                "a1",
                DeliveryPolicy::EarliestSafeBoundary,
            )
            .with_source_key("exact-a1")
            .with_merge_key("a"),
        )
        .await
        .expect("enqueue exact A1");
    let _b1 = store
        .enqueue_queued_work(
            queued_draft(
                "exact-key-break",
                "b1",
                DeliveryPolicy::EarliestSafeBoundary,
            )
            .with_source_key("exact-b1")
            .with_merge_key("b"),
        )
        .await
        .expect("enqueue exact B1");
    let a2 = store
        .enqueue_queued_work(
            queued_draft(
                "exact-key-break",
                "a2",
                DeliveryPolicy::EarliestSafeBoundary,
            )
            .with_source_key("exact-a2")
            .with_merge_key("a"),
        )
        .await
        .expect("enqueue exact A2");

    let owner = lease_owner("exact-key-break-owner");
    let lease =
        claim_session_execution_lease_for_test(&store, "exact-key-break", &owner.owner_id).await;
    let claim = store
        .claim_ready_queued_work_by_batch_ids(
            "exact-key-break",
            &lease.fence(),
            &owner,
            QueuedWorkClaimBoundary::Idle,
            &[a2.batch_id, a1.batch_id],
            crate::testing::queued_work_claim_policy(64),
        )
        .await
        .expect("claim reversed exact A rows")
        .expect("physical prefix contains exact A1");

    assert_eq!(
        claim
            .batches
            .iter()
            .map(|batch| (batch.source_key.as_deref(), batch.enqueue_seq))
            .collect::<Vec<_>>(),
        vec![(Some("exact-a1"), 1)],
        "an exact claim must preserve enqueue order and stop at the physical B key break"
    );
    assert_eq!(
        store
            .list_pending_queued_work("exact-key-break")
            .await
            .expect("list exact-key-break remainder")
            .iter()
            .map(|batch| (batch.source_key.as_deref(), batch.enqueue_seq))
            .collect::<Vec<_>>(),
        vec![(Some("exact-b1"), 2), (Some("exact-a2"), 3)],
        "the key-break row and later requested row must remain queued in physical order"
    );
    release_session_execution_lease_for_test(&store, &lease).await;
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
                crate::testing::queued_work_claim_policy(10),
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
        ..RuntimeSessionState::new(crate::SessionPolicy::new(crate::TurnBudget::Unbounded))
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
            &[command.batch_id.clone(), turn.batch_id.clone()],
            crate::testing::queued_work_claim_policy(64),
        )
        .await
        .expect("resolve mixed command and turn selection after command completion");
    assert_eq!(
        selected_turn.already_satisfied_batch_ids,
        vec![command.batch_id.clone()],
        "the leading command consumed before the selected turn is already satisfied"
    );
    let selected_turn = selected_turn.expect("selected turn claim exists");
    release_session_execution_lease_for_test(&store, &selected_turn_lease).await;
    assert_eq!(selected_turn.batches[0].batch_id, turn.batch_id);

    let first_turn = store
        .enqueue_queued_work(queued_draft(
            "turn-first",
            "first turn",
            DeliveryPolicy::AfterCurrentTurnCommit,
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
            crate::testing::queued_work_claim_policy(10),
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
        ))
        .await
        .expect("enqueue after-commit work");
    let earliest = store
        .enqueue_queued_work(queued_draft(
            "root",
            "earliest",
            DeliveryPolicy::EarliestSafeBoundary,
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
                crate::testing::queued_work_claim_policy(10),
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
            crate::testing::queued_work_claim_policy(10),
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
            crate::testing::queued_work_claim_policy(10),
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
            crate::testing::queued_work_claim_policy(10),
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
        ..RuntimeSessionState::new(crate::SessionPolicy::new(crate::TurnBudget::Unbounded))
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
            crate::testing::queued_work_claim_policy(10),
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
                crate::testing::queued_work_claim_policy(10),
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
            crate::testing::queued_work_claim_policy(10),
        )
        .await
        .expect("next-generation reclaim")
        .expect("next-generation reclaim exists");
    assert_eq!(claim_b.batches[0].batch_id, batch.batch_id);
    assert!(claim_b.fencing_token > claim_a.fencing_token);

    let stale_state = RuntimeSessionState {
        session_id: "root".to_string(),
        ..RuntimeSessionState::new(crate::SessionPolicy::new(crate::TurnBudget::Unbounded))
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
    let (dead_lease, claim_dead) =
        claim_queued_work_under_short_lease(&store, "root", &dead_owner, lease_timing).await;
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
            crate::testing::queued_work_claim_policy(10),
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
                "components": checkpoint.components,
                "plugin_snapshot_revision": checkpoint.plugin_snapshot_revision,
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
        .try_claim_session_execution_lease(
            session_id,
            owner,
            "claim-both-generation-fenced-lanes-executor",
            lease_ttl_ms,
        )
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
            crate::testing::queued_work_claim_policy(1),
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
                ))
                .await
                .expect("enqueue bounded-scan queued work"),
        );
    }
    let queue_lease = store
        .try_claim_session_execution_lease(
            queue_session,
            &queue_owner,
            "same-generation-claim-scans-reach-rows-beyond-the-scan-surplus-executor",
            60_000,
        )
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
                crate::testing::queued_work_claim_policy(1),
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
        .try_claim_session_execution_lease(
            command_session,
            &command_owner,
            "same-generation-claim-scans-reach-rows-beyond-the-scan-surplus-executor-2",
            60_000,
        )
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
        .try_claim_session_execution_lease(
            input_session,
            &input_owner,
            "same-generation-claim-scans-reach-rows-beyond-the-scan-surplus-executor-3",
            60_000,
        )
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
            queued_draft("root", "not ready", DeliveryPolicy::EarliestSafeBoundary)
                .with_available_at_ms(4_102_444_800_000),
        )
        .await
        .expect("enqueue unavailable work");
    let exclusive = store
        .enqueue_queued_work(queued_draft(
            "root",
            "exclusive",
            DeliveryPolicy::EarliestSafeBoundary,
        ))
        .await
        .expect("enqueue exclusive work");
    let joined = store
        .enqueue_queued_work(
            queued_draft("root", "joined", DeliveryPolicy::EarliestSafeBoundary)
                .with_merge_key("root"),
        )
        .await
        .expect("enqueue joined work");
    let other = store
        .enqueue_queued_work(queued_draft(
            "other",
            "other session",
            DeliveryPolicy::EarliestSafeBoundary,
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
            crate::testing::queued_work_claim_policy(10),
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
            crate::testing::queued_work_claim_policy(10),
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
            crate::testing::queued_work_claim_policy(10),
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
            crate::testing::queued_work_claim_policy(1),
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
            crate::testing::queued_work_claim_policy(1),
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
            queued_draft("limited", "one", DeliveryPolicy::EarliestSafeBoundary)
                .with_merge_key("limited"),
        )
        .await
        .expect("enqueue limited one");
    let limited_second = store
        .enqueue_queued_work(
            queued_draft("limited", "two", DeliveryPolicy::EarliestSafeBoundary)
                .with_merge_key("limited"),
        )
        .await
        .expect("enqueue limited two");
    let limited_third = store
        .enqueue_queued_work(
            queued_draft("limited", "three", DeliveryPolicy::EarliestSafeBoundary)
                .with_merge_key("limited"),
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
            crate::testing::queued_work_claim_policy(2),
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
            crate::testing::queued_work_claim_policy(10),
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
            queued_draft("root", "group a one", DeliveryPolicy::EarliestSafeBoundary)
                .with_merge_key("a"),
        )
        .await
        .expect("enqueue group a one");
    let second = store
        .enqueue_queued_work(
            queued_draft("root", "group a two", DeliveryPolicy::EarliestSafeBoundary)
                .with_merge_key("a"),
        )
        .await
        .expect("enqueue group a two");
    let different_merge = store
        .enqueue_queued_work(
            queued_draft("root", "group b", DeliveryPolicy::EarliestSafeBoundary)
                .with_merge_key("b"),
        )
        .await
        .expect("enqueue group b");
    let different_delivery = store
        .enqueue_queued_work(
            queued_draft(
                "root",
                "after commit",
                DeliveryPolicy::AfterCurrentTurnCommit,
            )
            .with_merge_key("a"),
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
            crate::testing::queued_work_claim_policy(10),
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
            crate::testing::queued_work_claim_policy(10),
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
            crate::testing::queued_work_claim_policy(10),
        )
        .await
        .expect("claim third group")
        .expect("third group claim");
    release_session_execution_lease_for_test(&store, &session_lease).await;
    assert_eq!(third_claim.batches[0].batch_id, different_delivery.batch_id);
}

async fn queued_work_redrive_preserves_interrupted_batch_composition(
    store: Arc<dyn RuntimePersistence>,
) {
    for (source_key, label) in [("redrive-w1", "w1"), ("redrive-w2", "w2")] {
        store
            .enqueue_queued_work(
                queued_draft(
                    "interrupted-batch-redrive",
                    label,
                    DeliveryPolicy::EarliestSafeBoundary,
                )
                .with_source_key(source_key)
                .with_merge_key("redrive-key"),
            )
            .await
            .expect("enqueue original redrive row");
    }

    let first_owner = lease_owner("redrive-owner-a");
    let first_lease = claim_session_execution_lease_for_test(
        &store,
        "interrupted-batch-redrive",
        &first_owner.owner_id,
    )
    .await;
    let first_claim = store
        .claim_ready_queued_work(
            "interrupted-batch-redrive",
            &first_lease.fence(),
            &first_owner,
            QueuedWorkClaimBoundary::Idle,
            crate::testing::queued_work_claim_policy(64),
        )
        .await
        .expect("claim original redrive batch")
        .expect("original redrive batch exists");
    assert_eq!(
        first_claim
            .batches
            .iter()
            .map(|batch| (batch.source_key.as_deref(), batch.enqueue_seq))
            .collect::<Vec<_>>(),
        vec![(Some("redrive-w1"), 1), (Some("redrive-w2"), 2)]
    );

    // Model an interruption after the claimed composition has escaped to a
    // journaled command, but before the queue completion commits. Releasing the
    // session lease makes the intact predecessor claim reclaimable without
    // abandoning or settling it.
    release_session_execution_lease_for_test(&store, &first_lease).await;
    store
        .enqueue_queued_work(
            queued_draft(
                "interrupted-batch-redrive",
                "w3",
                DeliveryPolicy::EarliestSafeBoundary,
            )
            .with_source_key("redrive-w3")
            .with_merge_key("redrive-key"),
        )
        .await
        .expect("enqueue post-interruption compatible row");

    let successor_owner = lease_owner("redrive-owner-b");
    let successor_lease = claim_session_execution_lease_for_test(
        &store,
        "interrupted-batch-redrive",
        &successor_owner.owner_id,
    )
    .await;
    let redriven = store
        .claim_ready_queued_work(
            "interrupted-batch-redrive",
            &successor_lease.fence(),
            &successor_owner,
            QueuedWorkClaimBoundary::Idle,
            crate::testing::queued_work_claim_policy(64),
        )
        .await
        .expect("redrive interrupted claim")
        .expect("interrupted claim remains reclaimable");
    assert_eq!(
        redriven
            .batches
            .iter()
            .map(|batch| (batch.source_key.as_deref(), batch.enqueue_seq))
            .collect::<Vec<_>>(),
        vec![(Some("redrive-w1"), 1), (Some("redrive-w2"), 2)],
        "redrive must retain the literal predecessor batch composition"
    );
    assert_ne!(first_claim.claim_id, redriven.claim_id);
    release_session_execution_lease_for_test(&store, &successor_lease).await;

    let third_owner = lease_owner("redrive-owner-c");
    let third_lease = claim_session_execution_lease_for_test(
        &store,
        "interrupted-batch-redrive",
        &third_owner.owner_id,
    )
    .await;
    let twice_redriven = store
        .claim_ready_queued_work(
            "interrupted-batch-redrive",
            &third_lease.fence(),
            &third_owner,
            QueuedWorkClaimBoundary::Idle,
            crate::testing::queued_work_claim_policy(64),
        )
        .await
        .expect("redrive second interrupted generation")
        .expect("second interrupted generation remains reclaimable");
    assert_eq!(
        twice_redriven
            .batches
            .iter()
            .map(|batch| (batch.source_key.as_deref(), batch.enqueue_seq))
            .collect::<Vec<_>>(),
        vec![(Some("redrive-w1"), 1), (Some("redrive-w2"), 2)],
        "a third generation must recover the second generation's literal composition"
    );
    assert_ne!(redriven.claim_id, twice_redriven.claim_id);

    let subsequent = store
        .claim_ready_queued_work(
            "interrupted-batch-redrive",
            &third_lease.fence(),
            &third_owner,
            QueuedWorkClaimBoundary::Idle,
            crate::testing::queued_work_claim_policy(64),
        )
        .await
        .expect("claim post-interruption row")
        .expect("post-interruption row remains separately claimable");
    assert_eq!(
        subsequent
            .batches
            .iter()
            .map(|batch| (batch.source_key.as_deref(), batch.enqueue_seq))
            .collect::<Vec<_>>(),
        vec![(Some("redrive-w3"), 3)],
        "new compatible work must wait for a separate successor claim"
    );
    release_session_execution_lease_for_test(&store, &third_lease).await;
}

#[doc(hidden)]
pub async fn queued_work_redrive_selects_claim_identity_across_ready_gap(
    store: Arc<dyn RuntimePersistence>,
    lease_timing: &RuntimePersistenceLeaseTiming,
) {
    let session_id = "interrupted-batch-ready-gap";
    store
        .enqueue_queued_work(
            queued_draft(session_id, "w1", DeliveryPolicy::EarliestSafeBoundary)
                .with_source_key("gap-w1")
                .with_merge_key("gap-key"),
        )
        .await
        .expect("enqueue ready gap W1");
    store
        .enqueue_queued_work(
            queued_draft(session_id, "w2", DeliveryPolicy::EarliestSafeBoundary)
                .with_source_key("gap-w2")
                .with_merge_key("gap-key")
                .with_available_at_ms(lease_timing.delayed_queue_row_available_at_ms()),
        )
        .await
        .expect("enqueue delayed gap W2");
    store
        .enqueue_queued_work(
            queued_draft(session_id, "w3", DeliveryPolicy::EarliestSafeBoundary)
                .with_source_key("gap-w3")
                .with_merge_key("gap-key"),
        )
        .await
        .expect("enqueue ready gap W3");

    let first_owner = lease_owner("gap-owner-a");
    let first_lease =
        claim_session_execution_lease_for_test(&store, session_id, &first_owner.owner_id).await;
    let first_claim = store
        .claim_ready_queued_work(
            session_id,
            &first_lease.fence(),
            &first_owner,
            QueuedWorkClaimBoundary::Idle,
            crate::testing::queued_work_claim_policy(64),
        )
        .await
        .expect("claim ready rows across delayed gap")
        .expect("ready W1 and W3 form the original claim");
    assert_eq!(
        first_claim
            .batches
            .iter()
            .map(|batch| (batch.source_key.as_deref(), batch.enqueue_seq))
            .collect::<Vec<_>>(),
        vec![(Some("gap-w1"), 1), (Some("gap-w3"), 3)]
    );
    release_session_execution_lease_for_test(&store, &first_lease).await;
    lease_timing.cross_delayed_queue_row_boundary().await;

    let successor = lease_owner("gap-owner-b");
    let successor_lease =
        claim_session_execution_lease_for_test(&store, session_id, &successor.owner_id).await;
    let redriven = store
        .claim_ready_queued_work(
            session_id,
            &successor_lease.fence(),
            &successor,
            QueuedWorkClaimBoundary::Idle,
            crate::testing::queued_work_claim_policy(64),
        )
        .await
        .expect("redrive ready-gap claim")
        .expect("interrupted identity remains reclaimable across gap");
    assert_eq!(
        redriven
            .batches
            .iter()
            .map(|batch| (batch.source_key.as_deref(), batch.enqueue_seq))
            .collect::<Vec<_>>(),
        vec![(Some("gap-w1"), 1), (Some("gap-w3"), 3)]
    );
    let delayed = store
        .claim_ready_queued_work(
            session_id,
            &successor_lease.fence(),
            &successor,
            QueuedWorkClaimBoundary::Idle,
            crate::testing::queued_work_claim_policy(64),
        )
        .await
        .expect("claim newly ready gap row")
        .expect("W2 remains a separate claim");
    assert_eq!(
        delayed
            .batches
            .iter()
            .map(|batch| (batch.source_key.as_deref(), batch.enqueue_seq))
            .collect::<Vec<_>>(),
        vec![(Some("gap-w2"), 2)]
    );
    release_session_execution_lease_for_test(&store, &successor_lease).await;
}

async fn queued_work_redrive_obeys_delivery_boundary_before_identity(
    store: Arc<dyn RuntimePersistence>,
) {
    let session_id = "interrupted-batch-delivery-gate";
    for (source_key, label) in [("gate-w1", "w1"), ("gate-w2", "w2")] {
        store
            .enqueue_queued_work(
                queued_draft(session_id, label, DeliveryPolicy::AfterCurrentTurnCommit)
                    .with_source_key(source_key)
                    .with_merge_key("gate-key"),
            )
            .await
            .expect("enqueue delivery-gated redrive row");
    }
    let first_owner = lease_owner("gate-owner-a");
    let first_lease =
        claim_session_execution_lease_for_test(&store, session_id, &first_owner.owner_id).await;
    let first_claim = store
        .claim_ready_queued_work(
            session_id,
            &first_lease.fence(),
            &first_owner,
            QueuedWorkClaimBoundary::Idle,
            crate::testing::queued_work_claim_policy(64),
        )
        .await
        .expect("claim delivery-gated work while idle")
        .expect("idle boundary admits after-commit work");
    assert_eq!(
        first_claim
            .batches
            .iter()
            .map(|batch| (batch.source_key.as_deref(), batch.enqueue_seq))
            .collect::<Vec<_>>(),
        vec![(Some("gate-w1"), 1), (Some("gate-w2"), 2)]
    );
    release_session_execution_lease_for_test(&store, &first_lease).await;

    let successor = lease_owner("gate-owner-b");
    let successor_lease =
        claim_session_execution_lease_for_test(&store, session_id, &successor.owner_id).await;
    assert!(
        store
            .claim_ready_queued_work(
                session_id,
                &successor_lease.fence(),
                &successor,
                QueuedWorkClaimBoundary::ActiveTurnCheckpoint,
                crate::testing::queued_work_claim_policy(64),
            )
            .await
            .expect("apply active checkpoint gate before identity redrive")
            .is_none(),
        "the active checkpoint boundary must produce a literal empty claim"
    );
    for (source_key, label) in [("gate-fresh-w1", "fresh-w1"), ("gate-fresh-w2", "fresh-w2")] {
        store
            .enqueue_queued_work(
                queued_draft(session_id, label, DeliveryPolicy::EarliestSafeBoundary)
                    .with_source_key(source_key)
                    .with_merge_key("gate-fresh-key"),
            )
            .await
            .expect("enqueue fresh checkpoint-deliverable work");
    }
    let fresh_checkpoint_claim = store
        .claim_ready_queued_work(
            session_id,
            &successor_lease.fence(),
            &successor,
            QueuedWorkClaimBoundary::ActiveTurnCheckpoint,
            crate::testing::queued_work_claim_policy(64),
        )
        .await
        .expect("claim fresh work while idle-only predecessor remains withheld")
        .expect("fresh checkpoint-deliverable work remains claimable");
    assert_eq!(
        fresh_checkpoint_claim
            .batches
            .iter()
            .map(|batch| (batch.source_key.as_deref(), batch.enqueue_seq))
            .collect::<Vec<_>>(),
        vec![(Some("gate-fresh-w1"), 3), (Some("gate-fresh-w2"), 4),]
    );
    store
        .abandon_queued_work_claim(&fresh_checkpoint_claim)
        .await
        .expect("return fresh checkpoint claim before idle redrive");
    let after_boundary = store
        .claim_ready_queued_work(
            session_id,
            &successor_lease.fence(),
            &successor,
            QueuedWorkClaimBoundary::Idle,
            crate::testing::queued_work_claim_policy(64),
        )
        .await
        .expect("redrive after delivery boundary clears")
        .expect("original composition remains intact");
    assert_eq!(
        after_boundary
            .batches
            .iter()
            .map(|batch| (batch.source_key.as_deref(), batch.enqueue_seq))
            .collect::<Vec<_>>(),
        vec![(Some("gate-w1"), 1), (Some("gate-w2"), 2)]
    );
    release_session_execution_lease_for_test(&store, &successor_lease).await;
}

async fn queued_work_redrive_ignores_successor_row_limit(store: Arc<dyn RuntimePersistence>) {
    let session_id = "interrupted-batch-row-limit";
    for (source_key, label) in [
        ("limit-w1", "w1"),
        ("limit-w2", "w2"),
        ("limit-w3", "w3"),
        ("limit-w4", "w4"),
        ("limit-w5", "w5"),
    ] {
        store
            .enqueue_queued_work(
                queued_draft(session_id, label, DeliveryPolicy::EarliestSafeBoundary)
                    .with_source_key(source_key)
                    .with_merge_key("limit-key"),
            )
            .await
            .expect("enqueue row-limit redrive row");
    }
    let first_owner = lease_owner("limit-owner-a");
    let first_lease =
        claim_session_execution_lease_for_test(&store, session_id, &first_owner.owner_id).await;
    let first_claim = store
        .claim_ready_queued_work(
            session_id,
            &first_lease.fence(),
            &first_owner,
            QueuedWorkClaimBoundary::Idle,
            crate::testing::queued_work_claim_policy(64),
        )
        .await
        .expect("claim five-row predecessor")
        .expect("five-row predecessor exists");
    assert_eq!(
        first_claim
            .batches
            .iter()
            .map(|batch| (batch.source_key.as_deref(), batch.enqueue_seq))
            .collect::<Vec<_>>(),
        vec![
            (Some("limit-w1"), 1),
            (Some("limit-w2"), 2),
            (Some("limit-w3"), 3),
            (Some("limit-w4"), 4),
            (Some("limit-w5"), 5),
        ]
    );
    release_session_execution_lease_for_test(&store, &first_lease).await;

    let successor = lease_owner("limit-owner-b");
    let successor_lease =
        claim_session_execution_lease_for_test(&store, session_id, &successor.owner_id).await;
    let redriven = store
        .claim_ready_queued_work(
            session_id,
            &successor_lease.fence(),
            &successor,
            QueuedWorkClaimBoundary::Idle,
            crate::testing::queued_work_claim_policy(2),
        )
        .await
        .expect("redrive under smaller successor row limit")
        .expect("predecessor composition ignores successor row limit");
    assert_eq!(
        redriven
            .batches
            .iter()
            .map(|batch| (batch.source_key.as_deref(), batch.enqueue_seq))
            .collect::<Vec<_>>(),
        vec![
            (Some("limit-w1"), 1),
            (Some("limit-w2"), 2),
            (Some("limit-w3"), 3),
            (Some("limit-w4"), 4),
            (Some("limit-w5"), 5),
        ]
    );
    release_session_execution_lease_for_test(&store, &successor_lease).await;

    let selected_owner = lease_owner("limit-owner-c");
    let selected_lease = claim_session_execution_lease_for_test(
        &store,
        "interrupted-batch-row-limit",
        &selected_owner.owner_id,
    )
    .await;
    let selected_redrive = store
        .claim_ready_queued_work_by_batch_ids(
            "interrupted-batch-row-limit",
            &selected_lease.fence(),
            &selected_owner,
            QueuedWorkClaimBoundary::Idle,
            &redriven
                .batches
                .iter()
                .map(|batch| batch.batch_id.clone())
                .collect::<Vec<_>>(),
            crate::testing::queued_work_claim_policy(2),
        )
        .await
        .expect("selected redrive under smaller successor row limit")
        .expect("selected predecessor composition ignores successor row limit");
    assert_eq!(
        selected_redrive
            .batches
            .iter()
            .map(|batch| (batch.source_key.as_deref(), batch.enqueue_seq))
            .collect::<Vec<_>>(),
        vec![
            (Some("limit-w1"), 1),
            (Some("limit-w2"), 2),
            (Some("limit-w3"), 3),
            (Some("limit-w4"), 4),
            (Some("limit-w5"), 5),
        ]
    );
    release_session_execution_lease_for_test(&store, &selected_lease).await;
}

async fn queued_work_selected_multi_identity_validation_and_abandon_restore(
    store: Arc<dyn RuntimePersistence>,
) {
    let session_id = "selected-multi-identity";
    let mut batches = Vec::new();
    for (source_key, label) in [
        ("selected-claim-a1", "a1"),
        ("selected-claim-a2", "a2"),
        ("selected-claim-b1", "b1"),
        ("selected-claim-b2", "b2"),
    ] {
        batches.push(
            store
                .enqueue_queued_work(
                    queued_draft(session_id, label, DeliveryPolicy::EarliestSafeBoundary)
                        .with_source_key(source_key)
                        .with_merge_key("selected-multi-identity-key"),
                )
                .await
                .expect("enqueue selected multi-identity row"),
        );
    }
    let predecessor_owner = lease_owner("selected-multi-predecessor");
    let predecessor_lease =
        claim_session_execution_lease_for_test(&store, session_id, &predecessor_owner.owner_id)
            .await;
    let claim_a = store
        .claim_ready_queued_work(
            session_id,
            &predecessor_lease.fence(),
            &predecessor_owner,
            QueuedWorkClaimBoundary::Idle,
            crate::testing::queued_work_claim_policy(2),
        )
        .await
        .expect("claim predecessor A")
        .expect("predecessor A exists");
    assert_eq!(
        claim_a
            .batches
            .iter()
            .map(|batch| batch.source_key.as_deref())
            .collect::<Vec<_>>(),
        vec![Some("selected-claim-a1"), Some("selected-claim-a2")]
    );
    let claim_b = store
        .claim_ready_queued_work(
            session_id,
            &predecessor_lease.fence(),
            &predecessor_owner,
            QueuedWorkClaimBoundary::Idle,
            crate::testing::queued_work_claim_policy(2),
        )
        .await
        .expect("claim predecessor B")
        .expect("predecessor B exists");
    assert_eq!(
        claim_b
            .batches
            .iter()
            .map(|batch| batch.source_key.as_deref())
            .collect::<Vec<_>>(),
        vec![Some("selected-claim-b1"), Some("selected-claim-b2")]
    );
    release_session_execution_lease_for_test(&store, &predecessor_lease).await;

    let successor_owner = lease_owner("selected-multi-successor");
    let successor_lease =
        claim_session_execution_lease_for_test(&store, session_id, &successor_owner.owner_id).await;
    let mixed = store
        .claim_ready_queued_work_by_batch_ids(
            session_id,
            &successor_lease.fence(),
            &successor_owner,
            QueuedWorkClaimBoundary::Idle,
            &[
                batches[0].batch_id.clone(),
                batches[1].batch_id.clone(),
                batches[2].batch_id.clone(),
            ],
            crate::testing::queued_work_claim_policy(64),
        )
        .await;
    assert!(
        matches!(
            &mixed,
            Err(StoreError::SelectedQueuedWorkRequiresInterruptedComposition {
                required_batch_ids,
            }) if required_batch_ids == &vec![
                batches[2].batch_id.clone(),
                batches[3].batch_id.clone(),
            ]
        ),
        "full A plus partial B must name B's literal complete composition: {mixed:?}"
    );

    let successor_claim = store
        .claim_ready_queued_work_by_batch_ids(
            session_id,
            &successor_lease.fence(),
            &successor_owner,
            QueuedWorkClaimBoundary::Idle,
            &batches
                .iter()
                .map(|batch| batch.batch_id.clone())
                .collect::<Vec<_>>(),
            crate::testing::queued_work_claim_policy(64),
        )
        .await
        .expect("select two complete interrupted identities")
        .expect("the physically earliest identity is reclaimed");
    assert_eq!(
        successor_claim
            .batches
            .iter()
            .map(|batch| batch.source_key.as_deref())
            .collect::<Vec<_>>(),
        vec![Some("selected-claim-a1"), Some("selected-claim-a2")]
    );
    assert_eq!(
        successor_claim.abandon_restore_claim_id.as_deref(),
        Some(claim_a.claim_id.as_str())
    );
    store
        .abandon_queued_work_claim(&successor_claim)
        .await
        .expect("abandon successor A claim");

    for (selected, required) in [
        (
            &batches[0],
            vec![batches[0].batch_id.clone(), batches[1].batch_id.clone()],
        ),
        (
            &batches[2],
            vec![batches[2].batch_id.clone(), batches[3].batch_id.clone()],
        ),
    ] {
        let partial = store
            .claim_ready_queued_work_by_batch_ids(
                session_id,
                &successor_lease.fence(),
                &successor_owner,
                QueuedWorkClaimBoundary::Idle,
                std::slice::from_ref(&selected.batch_id),
                crate::testing::queued_work_claim_policy(64),
            )
            .await;
        assert!(
            matches!(
                &partial,
                Err(StoreError::SelectedQueuedWorkRequiresInterruptedComposition {
                    required_batch_ids,
                }) if required_batch_ids == &required
            ),
            "abandon must restore both predecessor identities: {partial:?}"
        );
    }
    release_session_execution_lease_for_test(&store, &successor_lease).await;
}

async fn process_wakes_batch_by_default(store: Arc<dyn RuntimePersistence>) {
    let merged_wakes = [
        policy_test_wake("wake-default-batch", "process-a", 1),
        policy_test_wake("wake-default-batch", "process-b", 1),
    ];
    for wake in &merged_wakes {
        store
            .enqueue_queued_work(crate::process_wake_batch_draft(wake.clone()))
            .await
            .expect("enqueue default-key wake");
    }
    let merge_lease =
        claim_session_execution_lease_for_test(&store, "wake-default-batch", "merge-owner").await;
    let merged = store
        .claim_ready_queued_work(
            "wake-default-batch",
            &merge_lease.fence(),
            &lease_owner("merge-owner"),
            QueuedWorkClaimBoundary::Idle,
            crate::testing::queued_work_claim_policy(10),
        )
        .await
        .expect("claim default-key wakes")
        .expect("default-key wakes exist");
    assert_eq!(
        merged.batches.len(),
        2,
        "the constant wake merge key must batch compatible wakes across processes"
    );
    assert!(
        merged
            .batches
            .iter()
            .all(|batch| { batch.merge_key.as_deref() == Some(crate::PROCESS_WAKE_MERGE_KEY) })
    );
    let state = RuntimeSessionState {
        session_id: "wake-default-batch".to_string(),
        ..RuntimeSessionState::new(crate::SessionPolicy::new(crate::TurnBudget::Unbounded))
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
            .list_queued_work("wake-default-batch")
            .await
            .expect("list merged queue after settlement")
            .is_empty(),
        "merged settlement must delete every claimed receiver row"
    );
    for wake in merged_wakes {
        let error = store
            .enqueue_queued_work(crate::process_wake_batch_draft(wake))
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
        version: crate::PROCESS_WAKE_DELIVERY_FORMAT_VERSION,
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
        authority: crate::QueuedWorkAuthority::default(),
        input: process_id.to_string(),
        created_at_ms: 1,
    }
}

async fn queued_work_completion_is_lease_guarded(store: Arc<dyn RuntimePersistence>) {
    let first = store
        .enqueue_queued_work(
            queued_draft("root", "join one", DeliveryPolicy::EarliestSafeBoundary)
                .with_merge_key("joined"),
        )
        .await
        .expect("enqueue first joined batch");
    let second = store
        .enqueue_queued_work(
            queued_draft("root", "join two", DeliveryPolicy::EarliestSafeBoundary)
                .with_merge_key("joined"),
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
            crate::testing::queued_work_claim_policy(10),
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
        ..RuntimeSessionState::new(crate::SessionPolicy::new(crate::TurnBudget::Unbounded))
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
            crate::testing::queued_work_claim_policy(1),
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
        ..RuntimeSessionState::new(crate::SessionPolicy::new(crate::TurnBudget::Unbounded))
    };
    let mut base_commit = RuntimeCommit::persisted_state_for_test(&state, &[]);
    base_commit.enqueued_queue_batches = vec![
        QueuedWorkBatchDraft::new(
            "root",
            DeliveryPolicy::AfterCurrentTurnCommit,
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
        ..RuntimeSessionState::new(crate::SessionPolicy::new(crate::TurnBudget::Unbounded))
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
        ..RuntimeSessionState::new(crate::SessionPolicy::new(crate::TurnBudget::Unbounded))
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
    let (_dead_lease, claim_dead) =
        claim_turn_input_under_short_lease(&store, "root", &dead_owner, lease_timing).await;
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
            crate::testing::queued_work_claim_policy(10),
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
        ..RuntimeSessionState::new(crate::SessionPolicy::new(crate::TurnBudget::Unbounded))
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
        ..RuntimeSessionState::new(crate::SessionPolicy::new(crate::TurnBudget::Unbounded))
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
        ..RuntimeSessionState::new(crate::SessionPolicy::new(crate::TurnBudget::Unbounded))
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
        relation: SessionRelation::Child {
            parent_session_id: "parent-session".to_string(),
            caused_by: None,
        },
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
    assert_eq!(loaded, meta);
}

/// Blob-backed backends must physically reclaim the checkpoint blob a superseding
/// commit orphaned, while preserving the live one. Generalizes the SQLite-only
/// `gc_unreachable_keeps_rooted_checkpoint_blobs` test to every reclaiming
/// backend via the [`GcReport`](crate::GcReport) counters plus a post-GC load.
async fn gc_reclaims_unreachable_checkpoint_blobs_and_preserves_live(
    store: Arc<dyn RuntimePersistence>,
) {
    // First commit writes a live checkpoint blob.
    let mut v1 = RuntimeSessionState {
        session_id: "gc-blobs".to_string(),
        ..RuntimeSessionState::new(crate::SessionPolicy::new(crate::TurnBudget::Unbounded))
    };
    v1.set_tool_state_snapshot(Some(ToolState::default().with_generation(1)));
    let v1_result = commit_runtime_state_for_test(
        &store,
        RuntimeCommit::persisted_state_for_test(&v1, &[]),
        "gc-blobs-v1",
    )
    .await
    .expect("commit v1");
    // Second commit supersedes it with different content, so the v1 checkpoint
    // blob is now unreachable from every session head.
    let mut v2 = RuntimeSessionState {
        session_id: "gc-blobs".to_string(),
        head_revision: v1_result.head_revision,
        ..RuntimeSessionState::new(crate::SessionPolicy::new(crate::TurnBudget::Unbounded))
    };
    v2.set_tool_state_snapshot(Some(ToolState::default().with_generation(2)));
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
            .and_then(|checkpoint| {
                checkpoint
                    .decode_component::<ToolState>(crate::store::TOOL_STATE_CHECKPOINT_COMPONENT)
                    .expect("decode reachable tool state")
            })
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
        ..RuntimeSessionState::new(crate::SessionPolicy::new(crate::TurnBudget::Unbounded))
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
        relation: SessionRelation::Root,
    };
    factory
        .open
        .save_session_meta(meta.clone())
        .await
        .expect("save meta");
    let mut state = RuntimeSessionState {
        session_id: "root".to_string(),
        ..RuntimeSessionState::new(crate::SessionPolicy::new(crate::TurnBudget::Unbounded))
    };
    state.set_tool_state_snapshot(Some(ToolState::default().with_generation(77)));
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
    assert_eq!(reopened_meta, meta);
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
            .and_then(|checkpoint| {
                checkpoint
                    .decode_component::<ToolState>(crate::store::TOOL_STATE_CHECKPOINT_COMPONENT)
                    .expect("decode reopened tool state")
            })
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
        open.try_claim_session_execution_lease(
            "first-claim-race",
            &open_owner,
            "session-execution-lease-first-claim-excludes-concurrent-reopen-handles-executor",
            60_000,
        )
        .await
    });
    let reopen_claim = crate::task::spawn(async move {
        reopen_barrier.wait().await;
        reopen
            .try_claim_session_execution_lease(
                "first-claim-race",
                &reopen_owner,
                "session-execution-lease-first-claim-excludes-concurrent-reopen-handles-executor-2",
                60_000,
            )
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
        version: crate::PROCESS_WAKE_DELIVERY_FORMAT_VERSION,
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
        authority: crate::QueuedWorkAuthority::default(),
        input: "wake payload".to_string(),
        created_at_ms: 1,
    };
    let malformed = QueuedWorkBatchDraft::new(
        wake.target_session_id.clone(),
        DeliveryPolicy::EarliestSafeBoundary,
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
            crate::testing::queued_work_claim_policy(10),
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
        ..RuntimeSessionState::new(crate::SessionPolicy::new(crate::TurnBudget::Unbounded))
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
        ..RuntimeSessionState::new(crate::SessionPolicy::new(crate::TurnBudget::Unbounded))
    };
    state.ensure_agent_frame_initialized();
    state.session_graph.data_mut().nodes[0].timestamp = "2026-07-26T10:00:00Z".to_string();
    state.set_execution_state_snapshot(Some(vec![7; 1_024]));
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
        ..RuntimeSessionState::new(crate::SessionPolicy::new(crate::TurnBudget::Unbounded))
    };
    let mut changed = RuntimeCommit::persisted_state_for_test(&changed_state, &[]);
    changed.turn_commit =
        RuntimeTurnCommitStamp::new(crate::OperationId::turn("root", "provider-turn", "final"));
    let err = store
        .commit_runtime_state(changed)
        .await
        .expect_err("same provider turn id with a different commit hash must conflict");
    assert!(
        matches!(&err, StoreError::RuntimeTurnCommitConflict { .. }),
        "unexpected changed-hash error: {err:?}"
    );
}

async fn store_computed_hash_rejects_mutated_commit(store: Arc<dyn RuntimePersistence>) {
    let mut state = RuntimeSessionState {
        session_id: "root".to_string(),
        ..RuntimeSessionState::new(crate::SessionPolicy::new(crate::TurnBudget::Unbounded))
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
                assignment: crate::AgentFrameAssignment::from_policy(crate::SessionPolicy::new(
                    crate::TurnBudget::Unbounded,
                )),
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
    assert!(
        matches!(&err, StoreError::RuntimeTurnCommitConflict { .. }),
        "unexpected mutated-commit error: {err:?}"
    );
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
        ..RuntimeSessionState::new(crate::SessionPolicy::new(crate::TurnBudget::Unbounded))
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
    assert!(
        matches!(&err, StoreError::NodeIdDerivationMismatch { .. }),
        "unexpected node-derivation error: {err:?}"
    );
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
        ..RuntimeSessionState::new(crate::SessionPolicy::new(crate::TurnBudget::Unbounded))
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
            assignment: crate::AgentFrameAssignment::from_policy(crate::SessionPolicy::new(
                crate::TurnBudget::Unbounded,
            )),
            protocol_turn_options: ProtocolTurnOptions::default(),
        },
    };
    state.session_graph =
        crate::SessionGraph::from_nodes(vec![original.clone()], Some(colliding_id.clone()))
            .expect("collision fixture seed graph is valid");
    let initial = RuntimeCommit::persisted_state_for_test(&state, &[]);
    let first = commit_runtime_state_for_test(&store, initial, "collision-seed")
        .await
        .expect("seed colliding durable node");

    let replacement = crate::SessionNodeRecord {
        payload: crate::SessionNodePayload::FrameOpen {
            frame_key: frame_key.to_string(),
            reason: AgentFrameReason::new("replacement"),
            assignment: crate::AgentFrameAssignment::from_policy(crate::SessionPolicy::new(
                crate::TurnBudget::Unbounded,
            )),
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
    assert!(
        matches!(
            &err,
            StoreError::NodeIdCollision { node_id } if node_id == &colliding_id
        ),
        "unexpected durable collision error: {err:?}"
    );
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
        ..RuntimeSessionState::new(crate::SessionPolicy::new(crate::TurnBudget::Unbounded))
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
    assert!(
        matches!(
            &err,
            StoreError::NodeIdCollision { node_id } if node_id == &duplicate_node_id
        ),
        "unexpected duplicate-id error: {err:?}"
    );
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
        ..RuntimeSessionState::new(crate::SessionPolicy::new(crate::TurnBudget::Unbounded))
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
    assert!(
        matches!(
            &err,
            StoreError::InvalidGraphLeaf {
                leaf_node_id: Some(leaf)
            } if leaf == "missing-leaf"
        ),
        "unexpected unresolved-leaf error: {err:?}"
    );
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
        ..RuntimeSessionState::new(crate::SessionPolicy::new(crate::TurnBudget::Unbounded))
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
    assert!(
        matches!(&err, StoreError::InvalidGraphLeaf { leaf_node_id: None }),
        "unexpected missing-leaf error: {err:?}"
    );
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
        ..RuntimeSessionState::new(crate::SessionPolicy::new(crate::TurnBudget::Unbounded))
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
    assert!(
        matches!(&error, StoreError::InvalidGraphLeaf { leaf_node_id: None }),
        "unexpected empty-append error: {error:?}"
    );
    let loaded = store
        .load_session()
        .await
        .expect("load after rejected empty append")
        .expect("seeded session remains");
    assert_eq!(loaded.graph.leaf_node_id, old_leaf);
}
