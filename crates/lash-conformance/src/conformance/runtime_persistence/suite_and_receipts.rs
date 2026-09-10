use super::*;
use pretty_assertions::assert_eq;

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
pub async fn runtime_persistence<F>(
    make: F,
    lease_timing: RuntimePersistenceLeaseTiming,
    law: RuntimePersistenceLaw,
) where
    F: Fn(&str) -> Arc<dyn RuntimePersistence>,
{
    if matches!(law, RuntimePersistenceLaw::fresh_instances) {
        let first = make("fresh-instance-probe");
        let second = make("fresh-instance-probe");
        assert_fresh_instances(&first, &second, "runtime_persistence");
    } else {
        runtime_persistence_suite(make, &lease_timing, law).await;
    }
}

/// Run one independent durable reopen or runtime persistence vector.
pub async fn runtime_persistence_reopenable<F>(
    make: F,
    lease_timing: RuntimePersistenceLeaseTiming,
    law: RuntimePersistenceLaw,
) where
    F: Fn(&str) -> ReopenableRuntimePersistence,
{
    match law {
        RuntimePersistenceLaw::reopen_mint_identity => {
            let probe = make("pending-turn-input-multi-store-mint");
            assert_fresh_instances(&probe.open, &probe.reopen, "runtime_persistence_reopenable");
            pending_turn_input_mint_is_unique_across_store_instances(
                probe.open.as_ref(),
                probe.reopen.as_ref(),
            )
            .await;
        }
        RuntimePersistenceLaw::gc_blobs => {
            gc_reclaims_unreachable_checkpoint_blobs_and_preserves_live(make("gc-blobs").open).await
        }
        RuntimePersistenceLaw::append_receipt_reopen => {
            append_receipt_survives_reopen(make("root")).await
        }
        RuntimePersistenceLaw::runtime_reopen => {
            runtime_persistence_survives_reopen(make("root")).await
        }
        _ => {
            runtime_persistence_suite(|session_id| make(session_id).open, &lease_timing, law).await
        }
    }
}

pub(super) fn assert_two_session_resolution_errors(
    full: StoreError,
    head: StoreError,
    expected: &str,
) {
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

/// Prove that an unbound session-metadata lookup refuses to choose between
/// multiple durable session candidates.
///
/// The backend fixture must contain more than one session before constructing
/// `load`. SQLite reaches this seam through its unbound store handle;
/// PostgreSQL exercises the matching backend-support lookup directly because
/// its runtime-persistence handles are always session-bound.
pub async fn unbound_session_meta_refuses_ambiguous_resolution(
    backend_name: &str,
    load: impl std::future::Future<Output = Result<Option<SessionMeta>, crate::StoreError>>,
) {
    assert_eq!(
        load.await.unwrap_or_else(|error| panic!(
            "{backend_name}: load unbound session metadata: {error}"
        )),
        None,
        "{backend_name} must refuse to choose one session metadata row when multiple rows match"
    );
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

    fn request(session_id: &SessionId) -> crate::SessionStoreCreateRequest {
        crate::SessionStoreCreateRequest {
            pending_observer_intents: Vec::new(),
            session_id: SessionId::from(session_id.to_string()),
            relation: crate::SessionRelation::Root,
            policy: crate::SessionPolicy::new(crate::TurnBudget::Unbounded),
        }
    }

    async fn add_session(
        handles: &UnboundSessionResolutionHandles,
        admission_state: UnboundSessionAdmissionState,
        session_id: &SessionId,
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
                session_id: SessionId::from(session_id.to_string()),
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

        add_session(
            &handles,
            admission_state,
            &SessionId::from("unbound-resolution-a"),
        )
        .await;
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

        add_session(
            &handles,
            admission_state,
            &SessionId::from("unbound-resolution-b"),
        )
        .await;
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
pub(super) async fn pending_turn_input_mint_is_unique_across_store_instances(
    first: &dyn RuntimePersistence,
    second: &dyn RuntimePersistence,
) {
    let session_id = "pending-turn-input-multi-store-mint";
    let first_input = first
        .enqueue_pending_turn_input(pending_next_turn_input_draft(
            &SessionId::from(session_id),
            "first independent-store input",
        ))
        .await
        .expect("first store instance mints a pending turn-input ID");
    let second_input = second
        .enqueue_pending_turn_input(pending_next_turn_input_draft(
            &SessionId::from(session_id),
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
            &SessionId::from(session_id),
            "clock expiry queued work",
            DeliveryPolicy::EarliestSafeBoundary,
        ))
        .await
        .expect("enqueue clock-expiry queued work");
    let input = store
        .enqueue_pending_turn_input(pending_next_turn_input_draft(
            &SessionId::from(session_id),
            "clock expiry turn input",
        ))
        .await
        .expect("enqueue clock-expiry turn input");
    let stale_lease = store
        .try_claim_session_execution_lease(
            &SessionId::from(session_id),
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
            &SessionId::from(session_id),
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
        .claim_next_turn_inputs(
            &SessionId::from(session_id),
            &stale_lease.fence(),
            &stale_owner,
            1,
        )
        .await
        .expect("claim clock-expiry turn input")
        .expect("clock-expiry turn input claim exists");

    advance(TTL_MS);

    let successor_lease = store
        .try_claim_session_execution_lease(
            &SessionId::from(session_id),
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
            &SessionId::from(session_id),
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
        .claim_next_turn_inputs(
            &SessionId::from(session_id),
            &successor_lease.fence(),
            &successor,
            1,
        )
        .await
        .expect("reclaim clock-expiry turn input")
        .expect("dead-generation turn input is reclaimable");
    assert!(successor_queue_claim.fencing_token > stale_queue_claim.fencing_token);
    assert!(successor_input_claim.fencing_token > stale_input_claim.fencing_token);

    let stale_state = RuntimeSessionState {
        session_id: SessionId::from(session_id.to_string()),
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

/// Independently runnable runtime persistence contract vectors.
#[allow(non_camel_case_types)]
#[derive(Clone, Copy, Debug)]
pub enum RuntimePersistenceLaw {
    plugin_state_boundary,
    commit_increments_head_and_round_trips_agent_frames,
    concurrent_head_revision_cas_applies_exactly_once,
    commit_rejects_a_different_session_id,
    commit_rejects_carried_nondefault_node_budget,
    commit_rejects_carried_nondefault_byte_budget,
    commit_rejects_queue_batch_bytes_over_budget,
    commit_rejects_agent_frame_bytes_over_budget,
    commit_rejects_usage_delta_bytes_over_budget,
    commit_rejects_turn_result_bytes_over_budget,
    commit_with_every_payload_family_inside_budget_succeeds,
    load_hydrates_checkpoint_and_usage,
    load_retains_reasoning_only_usage,
    load_retains_usage_dispositions_and_rebuilds_outstanding_attempts,
    checkpoint_restore_rejects_turn_index_without_increment_headroom,
    checkpoint_restore_rejects_token_usage_whose_prompt_subtotal_overflows,
    load_rejects_token_usage_overflow,
    usage_delta_identity_is_idempotent_across_commits,
    usage_ordinal_reuse_with_different_payload_survives_receipt_replay,
    execution_state_replace_then_clear_removes_the_live_checkpoint_ref,
    checkpoint_rejects_unknown_component_ref,
    session_read_loads_persisted_history,
    session_prompt_layer_round_trips_through_the_committed_head,
    session_protocol_turn_options_round_trip_through_the_committed_head,
    session_metadata_round_trips,
    attachment_manifest_records_intent_and_commit_stamps,
    attachment_manifest_keeps_same_content_ownership_per_session,
    attachment_manifest_reference_tracking_and_gc_root_set,
    final_commit_stamp_is_idempotent_and_conflicts_on_changed_hash,
    append_request_receipt_replays_after_head_advance,
    append_request_receipt_rejects_changed_content,
    append_request_exact_hash_rejects_changed_ancestor,
    append_request_receipt_rejects_corrupt_node_count,
    semantic_boundary_receipt_replays_after_head_advance,
    semantic_boundary_receipt_rejects_changed_content,
    semantic_boundary_receipt_rejects_mislabeled_identity,
    concurrent_same_append_operation_applies_exactly_once,
    legacy_append_receipt_keeps_exact_hash_semantics,
    append_receipt_encoding_version_mismatch_keeps_exact_hash_semantics,
    append_receipt_and_graph_append_are_atomic,
    fresh_append_receipt_enforces_ancestor_precondition,
    store_computed_hash_rejects_mutated_commit,
    commit_rejects_non_derived_append_node_ids,
    append_rejects_duplicate_batch_node_ids,
    append_rejects_existing_node_id_collision,
    head_retirement_gate_distinguishes_leaf_change_from_same_leaf,
    commit_rejects_unresolvable_leaf,
    commit_rejects_missing_leaf,
    empty_append_cannot_move_the_head,
    commit_rejects_leaf_without_frame_open_ancestor,
    session_execution_lease_contract,
    borrowed_session_execution_lease_commit_contract,
    same_incarnation_rotation_gates_claims_not_commits,
    same_host_distinct_executors_are_lane_less_without_revoking_holder,
    session_execution_lease_fence_authority,
    concurrent_session_execution_lease_rotation_and_stale_renewal_are_linearizable,
    session_execution_lease_expires_by_ttl_contract,
    durable_queued_drain_wait_store_laws,
    session_execution_lease_diagnostic_read_contract,
    session_execution_lease_displacement_contract,
    queued_work_source_keys_are_idempotent_and_list_ordered,
    concurrent_queued_work_source_key_enqueues_report_one_inserted_and_one_existing,
    decorated_queued_work_source_key_replay_reports_absorbed,
    pending_session_work_ordering_agrees_across_ingress_families,
    concurrent_queue_and_turn_input_claims_have_one_owner,
    checkpoint_work_claims_both_families_once,
    checkpoint_budget_refusal_preserves_active_turn_input,
    checkpoint_claims_honor_min_boundary_at_every_checkpoint,
    queued_work_cancel_removes_only_unclaimed_batches,
    queued_work_exact_claim_uses_selected_batch_ids,
    queued_work_classes_gate_command_and_turn_claims,
    queued_work_claims_respect_boundaries_abandon_and_stale_completion,
    queued_work_claims_supersede_across_session_lease_generations_with_timing,
    claim_liveness_for_lease_less_paths_tracks_session_generations,
    same_generation_claim_scans_reach_rows_beyond_the_scan_surplus,
    queued_work_respects_membership_limits_exclusivity_reclaim_and_sessions,
    queued_work_join_groups_by_delivery_policy_and_merge_key,
    abandoned_predecessor_claim_pair_is_only_reclaimable_across_lease_generations,
    queued_work_redrive_preserves_interrupted_batch_composition,
    queued_work_names_a_deferred_lane_apart_from_an_exhausted_one,
    queued_work_redrive_selects_claim_identity_across_ready_gap,
    queued_work_redrive_obeys_delivery_boundary_before_identity,
    queued_work_redrive_ignores_successor_row_limit,
    queued_work_redrive_ignores_a_changed_drain_policy,
    queued_work_selected_multi_identity_validation_and_abandon_restore,
    queued_work_exact_claim_preserves_physical_order_and_key_breaks,
    process_wakes_batch_by_default,
    queued_work_completion_is_lease_guarded,
    queued_wake_delivery_is_source_key_idempotent_and_claimed_once,
    queue_completion_and_turn_commit_stamp_are_atomic,
    pending_turn_inputs_source_keys_order_cancel_and_cross_session,
    pending_turn_input_bulk_and_suffix_cancellation,
    pending_turn_input_claims_reclaim_complete_and_fence,
    turn_input_application_identity_survives_pending_tombstone_vacuum,
    turn_input_claims_supersede_across_session_lease_generations_with_timing,
    active_turn_input_claim_reacquires_after_unrecorded_checkpoint,
    pending_turn_input_cancel_covers_active_and_deferred_states,
    pending_active_turn_inputs_defer_unaccepted_once_on_interrupt,
    a_turn_that_cannot_commit_leaves_no_input_pinned_to_it,
    fresh_instances,
    reopen_mint_identity,
    gc_blobs,
    append_receipt_reopen,
    runtime_reopen,
}

pub(super) async fn runtime_persistence_suite<F>(
    make: F,
    lease_timing: &RuntimePersistenceLeaseTiming,
    law: RuntimePersistenceLaw,
) where
    F: Fn(&str) -> Arc<dyn RuntimePersistence>,
{
    match law {
        RuntimePersistenceLaw::plugin_state_boundary => { super::plugin_state::plugin_state_boundary_law(make).await; },
        RuntimePersistenceLaw::commit_increments_head_and_round_trips_agent_frames => { commit_increments_head_and_round_trips_agent_frames(make("root")).await; },
        RuntimePersistenceLaw::concurrent_head_revision_cas_applies_exactly_once => { concurrent_head_revision_cas_applies_exactly_once(make("concurrent-head-cas")).await; },
        RuntimePersistenceLaw::commit_rejects_a_different_session_id => { commit_rejects_a_different_session_id(make("alpha")).await; },
        RuntimePersistenceLaw::commit_rejects_carried_nondefault_node_budget => { commit_rejects_carried_nondefault_node_budget(make("root")).await; },
        RuntimePersistenceLaw::commit_rejects_carried_nondefault_byte_budget => { commit_rejects_carried_nondefault_byte_budget(make("root")).await; },
        RuntimePersistenceLaw::commit_rejects_queue_batch_bytes_over_budget => { commit_rejects_queue_batch_bytes_over_budget(make("root")).await; },
        RuntimePersistenceLaw::commit_rejects_agent_frame_bytes_over_budget => { commit_rejects_agent_frame_bytes_over_budget(make("root")).await; },
        RuntimePersistenceLaw::commit_rejects_usage_delta_bytes_over_budget => { commit_rejects_usage_delta_bytes_over_budget(make("root")).await; },
        RuntimePersistenceLaw::commit_rejects_turn_result_bytes_over_budget => { commit_rejects_turn_result_bytes_over_budget(make("root")).await; },
        RuntimePersistenceLaw::commit_with_every_payload_family_inside_budget_succeeds => { commit_with_every_payload_family_inside_budget_succeeds(make("root")).await; },
        RuntimePersistenceLaw::load_hydrates_checkpoint_and_usage => { load_hydrates_checkpoint_and_usage(make("hydrated")).await; },
        RuntimePersistenceLaw::load_retains_reasoning_only_usage => { load_retains_reasoning_only_usage(make("root")).await; },
        RuntimePersistenceLaw::load_retains_usage_dispositions_and_rebuilds_outstanding_attempts => { load_retains_usage_dispositions_and_rebuilds_outstanding_attempts(make("root")).await; },
        RuntimePersistenceLaw::checkpoint_restore_rejects_turn_index_without_increment_headroom => { checkpoint_restore_rejects_turn_index_without_increment_headroom(make("root")).await; },
        RuntimePersistenceLaw::checkpoint_restore_rejects_token_usage_whose_prompt_subtotal_overflows => { checkpoint_restore_rejects_token_usage_whose_prompt_subtotal_overflows(make("root")).await; },
        RuntimePersistenceLaw::load_rejects_token_usage_overflow => { load_rejects_token_usage_overflow(make("root")).await; },
        RuntimePersistenceLaw::usage_delta_identity_is_idempotent_across_commits => { usage_delta_identity_is_idempotent_across_commits(make("root")).await; },
        RuntimePersistenceLaw::usage_ordinal_reuse_with_different_payload_survives_receipt_replay => { usage_ordinal_reuse_with_different_payload_survives_receipt_replay(make("root")).await; },
        RuntimePersistenceLaw::execution_state_replace_then_clear_removes_the_live_checkpoint_ref => { execution_state_replace_then_clear_removes_the_live_checkpoint_ref(make(
        "execution-state-replace-then-clear",
    ))
    .await; },
        RuntimePersistenceLaw::checkpoint_rejects_unknown_component_ref => { checkpoint_rejects_unknown_component_ref(make("checkpoint-unknown-ref")).await; },
        RuntimePersistenceLaw::session_read_loads_persisted_history => { session_read_loads_persisted_history(make("branchy")).await; },
        RuntimePersistenceLaw::session_prompt_layer_round_trips_through_the_committed_head => { session_prompt_layer_round_trips_through_the_committed_head(make("session-prompt-layer")).await; },
        RuntimePersistenceLaw::session_protocol_turn_options_round_trip_through_the_committed_head => { session_protocol_turn_options_round_trip_through_the_committed_head(make("session-protocol-turn-options")).await; },
        RuntimePersistenceLaw::session_metadata_round_trips => { session_metadata_round_trips(make("root")).await; },
        RuntimePersistenceLaw::attachment_manifest_records_intent_and_commit_stamps => { attachment_manifest_records_intent_and_commit_stamps(make("root")).await; },
        RuntimePersistenceLaw::attachment_manifest_keeps_same_content_ownership_per_session => { attachment_manifest_keeps_same_content_ownership_per_session(make("root")).await; },
        RuntimePersistenceLaw::attachment_manifest_reference_tracking_and_gc_root_set => { attachment_manifest_reference_tracking_and_gc_root_set(make("root")).await; },
        RuntimePersistenceLaw::final_commit_stamp_is_idempotent_and_conflicts_on_changed_hash => { final_commit_stamp_is_idempotent_and_conflicts_on_changed_hash(make("root")).await; },
        RuntimePersistenceLaw::append_request_receipt_replays_after_head_advance => { append_request_receipt_replays_after_head_advance(make("root")).await; },
        RuntimePersistenceLaw::append_request_receipt_rejects_changed_content => { append_request_receipt_rejects_changed_content(make("root")).await; },
        RuntimePersistenceLaw::append_request_exact_hash_rejects_changed_ancestor => { append_request_exact_hash_rejects_changed_ancestor(make("root")).await; },
        RuntimePersistenceLaw::append_request_receipt_rejects_corrupt_node_count => { append_request_receipt_rejects_corrupt_node_count(make("root")).await; },
        RuntimePersistenceLaw::semantic_boundary_receipt_replays_after_head_advance => { semantic_boundary_receipt_replays_after_head_advance(make("root")).await; },
        RuntimePersistenceLaw::semantic_boundary_receipt_rejects_changed_content => { semantic_boundary_receipt_rejects_changed_content(make("root")).await; },
        RuntimePersistenceLaw::semantic_boundary_receipt_rejects_mislabeled_identity => { semantic_boundary_receipt_rejects_mislabeled_identity(make("root")).await; },
        RuntimePersistenceLaw::concurrent_same_append_operation_applies_exactly_once => { concurrent_same_append_operation_applies_exactly_once(make("root")).await; },
        RuntimePersistenceLaw::legacy_append_receipt_keeps_exact_hash_semantics => { legacy_append_receipt_keeps_exact_hash_semantics(make("root")).await; },
        RuntimePersistenceLaw::append_receipt_encoding_version_mismatch_keeps_exact_hash_semantics => { append_receipt_encoding_version_mismatch_keeps_exact_hash_semantics(make("root")).await; },
        RuntimePersistenceLaw::append_receipt_and_graph_append_are_atomic => { append_receipt_and_graph_append_are_atomic(make("root")).await; },
        RuntimePersistenceLaw::fresh_append_receipt_enforces_ancestor_precondition => { fresh_append_receipt_enforces_ancestor_precondition(make("root")).await; },
        RuntimePersistenceLaw::store_computed_hash_rejects_mutated_commit => { store_computed_hash_rejects_mutated_commit(make("root")).await; },
        RuntimePersistenceLaw::commit_rejects_non_derived_append_node_ids => { commit_rejects_non_derived_append_node_ids(make("root")).await; },
        RuntimePersistenceLaw::append_rejects_duplicate_batch_node_ids => { append_rejects_duplicate_batch_node_ids(make("root")).await; },
        RuntimePersistenceLaw::append_rejects_existing_node_id_collision => { append_rejects_existing_node_id_collision(make("root")).await; },
        RuntimePersistenceLaw::head_retirement_gate_distinguishes_leaf_change_from_same_leaf => { head_retirement_gate_distinguishes_leaf_change_from_same_leaf(make("root")).await; },
        RuntimePersistenceLaw::commit_rejects_unresolvable_leaf => { commit_rejects_unresolvable_leaf(make("root")).await; },
        RuntimePersistenceLaw::commit_rejects_missing_leaf => { commit_rejects_missing_leaf(make("root")).await; },
        RuntimePersistenceLaw::empty_append_cannot_move_the_head => { empty_append_cannot_move_the_head(make("empty-append-head-move")).await; },
        RuntimePersistenceLaw::commit_rejects_leaf_without_frame_open_ancestor => { commit_rejects_leaf_without_frame_open_ancestor(make("missing-frame-root")).await; },
        RuntimePersistenceLaw::session_execution_lease_contract => { session_execution_lease_contract(make("root")).await; },
        RuntimePersistenceLaw::borrowed_session_execution_lease_commit_contract => { crate::conformance::borrowed_session_execution_lease_commit_contract(make(
        "borrowed-commit-fence",
    ))
    .await; },
        RuntimePersistenceLaw::same_incarnation_rotation_gates_claims_not_commits => { same_incarnation_rotation_gates_claims_not_commits(make("root")).await; },
        RuntimePersistenceLaw::same_host_distinct_executors_are_lane_less_without_revoking_holder => { crate::conformance::same_host_distinct_executors_are_lane_less_without_revoking_holder(
        make("fig1133-same-host-session"),
    )
    .await; },
        RuntimePersistenceLaw::session_execution_lease_fence_authority => { session_execution_lease_fence_authority(make("lease-fence-authority").as_ref()).await; },
        RuntimePersistenceLaw::concurrent_session_execution_lease_rotation_and_stale_renewal_are_linearizable => { concurrent_session_execution_lease_rotation_and_stale_renewal_are_linearizable(make(
        "concurrent-rotation-renewal",
    ))
    .await; },
        RuntimePersistenceLaw::session_execution_lease_expires_by_ttl_contract => { session_execution_lease_expires_by_ttl_contract(&|| make("ttl-expiry"), lease_timing).await; },
        RuntimePersistenceLaw::durable_queued_drain_wait_store_laws => { super::durable_queued_drain_wait::durable_queued_drain_wait_store_laws(
        make("durable-queued-drain"),
        lease_timing,
    )
    .await; },
        RuntimePersistenceLaw::session_execution_lease_diagnostic_read_contract => { session_execution_lease_diagnostic_read_contract(make("lease-diagnostic")).await; },
        RuntimePersistenceLaw::session_execution_lease_displacement_contract => { session_execution_lease_displacement_contract(make("lease-displacement")).await; },
        RuntimePersistenceLaw::queued_work_source_keys_are_idempotent_and_list_ordered => { queued_work_source_keys_are_idempotent_and_list_ordered(make("queued-work-source-keys")).await; },
        RuntimePersistenceLaw::concurrent_queued_work_source_key_enqueues_report_one_inserted_and_one_existing => { concurrent_queued_work_source_key_enqueues_report_one_inserted_and_one_existing(make(
        "concurrent-queued-work-source-key",
    ))
    .await; },
        RuntimePersistenceLaw::decorated_queued_work_source_key_replay_reports_absorbed => { decorated_queued_work_source_key_replay_reports_absorbed(make(
        "decorated-queued-work-source-key",
    ))
    .await; },
        RuntimePersistenceLaw::pending_session_work_ordering_agrees_across_ingress_families => { pending_session_work_ordering_agrees_across_ingress_families(make("pending-work-ordering"))
        .await; },
        RuntimePersistenceLaw::concurrent_queue_and_turn_input_claims_have_one_owner => { concurrent_queue_and_turn_input_claims_have_one_owner(make("concurrent-queue-input")).await; },
        RuntimePersistenceLaw::checkpoint_work_claims_both_families_once => { checkpoint_work_claims_both_families_once(make("checkpoint-work")).await; },
        RuntimePersistenceLaw::checkpoint_budget_refusal_preserves_active_turn_input => { checkpoint_budget_refusal_preserves_active_turn_input(make("checkpoint-budget-refusal")).await; },
        RuntimePersistenceLaw::checkpoint_claims_honor_min_boundary_at_every_checkpoint => { checkpoint_claims_honor_min_boundary_at_every_checkpoint(make("checkpoint-min-boundary")).await; },
        RuntimePersistenceLaw::queued_work_cancel_removes_only_unclaimed_batches => { queued_work_cancel_removes_only_unclaimed_batches(make("queued-work-cancel")).await; },
        RuntimePersistenceLaw::queued_work_exact_claim_uses_selected_batch_ids => { queued_work_exact_claim_uses_selected_batch_ids(make("root")).await; },
        RuntimePersistenceLaw::queued_work_classes_gate_command_and_turn_claims => { queued_work_classes_gate_command_and_turn_claims(make("root")).await; },
        RuntimePersistenceLaw::queued_work_claims_respect_boundaries_abandon_and_stale_completion => { queued_work_claims_respect_boundaries_abandon_and_stale_completion(make("root")).await; },
        RuntimePersistenceLaw::queued_work_claims_supersede_across_session_lease_generations_with_timing => { queued_work_claims_supersede_across_session_lease_generations_with_timing(
        make("root"),
        lease_timing,
    )
    .await; },
        RuntimePersistenceLaw::claim_liveness_for_lease_less_paths_tracks_session_generations => { claim_liveness_for_lease_less_paths_tracks_session_generations(
        make("claim-liveness"),
        lease_timing,
    )
    .await; },
        RuntimePersistenceLaw::same_generation_claim_scans_reach_rows_beyond_the_scan_surplus => { same_generation_claim_scans_reach_rows_beyond_the_scan_surplus(make("claim-scan")).await; },
        RuntimePersistenceLaw::queued_work_respects_membership_limits_exclusivity_reclaim_and_sessions => { queued_work_respects_membership_limits_exclusivity_reclaim_and_sessions(make(
        "queued-membership",
    ))
    .await; },
        RuntimePersistenceLaw::queued_work_join_groups_by_delivery_policy_and_merge_key => { queued_work_join_groups_by_delivery_policy_and_merge_key(make("queued-join")).await; },
        RuntimePersistenceLaw::abandoned_predecessor_claim_pair_is_only_reclaimable_across_lease_generations => { abandoned_predecessor_claim_pair_is_only_reclaimable_across_lease_generations(make(
        "abandoned-predecessor-generation",
    ))
    .await; },
        RuntimePersistenceLaw::queued_work_redrive_preserves_interrupted_batch_composition => { queued_work_redrive_preserves_interrupted_batch_composition(make("redrive-composition")).await; },
        RuntimePersistenceLaw::queued_work_names_a_deferred_lane_apart_from_an_exhausted_one => { queued_work_names_a_deferred_lane_apart_from_an_exhausted_one(
        make("deferred-versus-exhausted"),
        lease_timing,
    )
    .await; },
        RuntimePersistenceLaw::queued_work_redrive_selects_claim_identity_across_ready_gap => { queued_work_redrive_selects_claim_identity_across_ready_gap(
        make("redrive-ready-gap"),
        lease_timing,
    )
    .await; },
        RuntimePersistenceLaw::queued_work_redrive_obeys_delivery_boundary_before_identity => { queued_work_redrive_obeys_delivery_boundary_before_identity(make("redrive-boundary")).await; },
        RuntimePersistenceLaw::queued_work_redrive_ignores_successor_row_limit => { queued_work_redrive_ignores_successor_row_limit(make("redrive-row-limit")).await; },
        RuntimePersistenceLaw::queued_work_redrive_ignores_a_changed_drain_policy => { queued_work_redrive_ignores_a_changed_drain_policy(make("redrive-drain-policy")).await; },
        RuntimePersistenceLaw::queued_work_selected_multi_identity_validation_and_abandon_restore => { queued_work_selected_multi_identity_validation_and_abandon_restore(make(
        "selected-multi-identity",
    ))
    .await; },
        RuntimePersistenceLaw::queued_work_exact_claim_preserves_physical_order_and_key_breaks => { crate::conformance::queued_work_exact_claim_preserves_physical_order_and_key_breaks(
        make("physical-order"),
    )
    .await; },
        RuntimePersistenceLaw::process_wakes_batch_by_default => { process_wakes_batch_by_default(make("wake-default-batch")).await; },
        RuntimePersistenceLaw::queued_work_completion_is_lease_guarded => { queued_work_completion_is_lease_guarded(make("root")).await; },
        RuntimePersistenceLaw::queued_wake_delivery_is_source_key_idempotent_and_claimed_once => { queued_wake_delivery_is_source_key_idempotent_and_claimed_once(make("root")).await; },
        RuntimePersistenceLaw::queue_completion_and_turn_commit_stamp_are_atomic => { queue_completion_and_turn_commit_stamp_are_atomic(make("root")).await; },
        RuntimePersistenceLaw::pending_turn_inputs_source_keys_order_cancel_and_cross_session => { pending_turn_inputs_source_keys_order_cancel_and_cross_session(make("root")).await; },
        RuntimePersistenceLaw::pending_turn_input_bulk_and_suffix_cancellation => { pending_turn_input_bulk_and_suffix_cancellation(make("pending-bulk-cancel")).await; },
        RuntimePersistenceLaw::pending_turn_input_claims_reclaim_complete_and_fence => { pending_turn_input_claims_reclaim_complete_and_fence(make("root")).await; },
        RuntimePersistenceLaw::turn_input_application_identity_survives_pending_tombstone_vacuum => { turn_input_application_identity_survives_pending_tombstone_vacuum(make(
        "turn-input-application",
    ))
    .await; },
        RuntimePersistenceLaw::turn_input_claims_supersede_across_session_lease_generations_with_timing => { turn_input_claims_supersede_across_session_lease_generations_with_timing(
        make("root"),
        lease_timing,
    )
    .await; },
        RuntimePersistenceLaw::active_turn_input_claim_reacquires_after_unrecorded_checkpoint => { active_turn_input_claim_reacquires_after_unrecorded_checkpoint(make("fig905-active-reacquire"))
        .await; },
        RuntimePersistenceLaw::pending_turn_input_cancel_covers_active_and_deferred_states => { pending_turn_input_cancel_covers_active_and_deferred_states(make("root")).await; },
        RuntimePersistenceLaw::pending_active_turn_inputs_defer_unaccepted_once_on_interrupt => { pending_active_turn_inputs_defer_unaccepted_once_on_interrupt(make("root")).await; },
        RuntimePersistenceLaw::a_turn_that_cannot_commit_leaves_no_input_pinned_to_it => { crate::conformance::a_turn_that_cannot_commit_leaves_no_input_pinned_to_it(make(
        "root",
    ))
    .await; },
        _ => unreachable!("reopen vector requires the reopenable runner"),
    }
}

pub(super) async fn session_prompt_layer_round_trips_through_the_committed_head(
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
        session_id: SessionId::from("session-prompt-layer"),
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
    assert_eq!(head.config.prompt, Some(expected_prompt.clone()));
    let restored = crate::store::load_persisted_session_state(store.as_ref())
        .await
        .expect("load persisted session state")
        .expect("committed session state");
    assert_eq!(restored.policy.prompt, expected_prompt);
}

/// FIG-2479: the commanded protocol-turn-options fact round-trips resident
/// state → committed head row (SESSION_HEAD_META v6) → cold load, and the head
/// value is what the loaded state carries.
pub(super) async fn session_protocol_turn_options_round_trip_through_the_committed_head(
    store: Arc<dyn RuntimePersistence>,
) {
    let expected = crate::ProtocolTurnOptions {
        payload: serde_json::json!({
            "dialect": "conformance-dialect",
            "termination": {"kind": "conformance-termination"},
        }),
    };
    let mut state = RuntimeSessionState {
        session_id: SessionId::from("session-protocol-turn-options"),
        ..RuntimeSessionState::new(crate::SessionPolicy::new(crate::TurnBudget::Unbounded))
    };
    state.protocol_turn_options = expected.clone();

    commit_runtime_state_for_test(
        &store,
        RuntimeCommit::persisted_state_for_test(&state, &[]),
        "session-protocol-turn-options",
    )
    .await
    .expect("commit session protocol turn options");

    let head = store
        .load_session_head_meta()
        .await
        .expect("load session head")
        .expect("committed session head");
    assert_eq!(
        head.config.protocol_turn_options,
        Some(expected.clone()),
        "the committed head row must carry the settled protocol turn options"
    );
    let restored = crate::store::load_persisted_session_state(store.as_ref())
        .await
        .expect("load persisted session state")
        .expect("committed session state");
    assert_eq!(
        restored.protocol_turn_options, expected,
        "cold load must restore the protocol turn options from the head"
    );
}

pub(super) async fn execution_state_replace_then_clear_removes_the_live_checkpoint_ref(
    store: Arc<dyn RuntimePersistence>,
) {
    let mut state =
        RuntimeSessionState::new(crate::SessionPolicy::new(crate::TurnBudget::Unbounded));
    state.session_id = SessionId::from("execution-state-replace-then-clear".to_string());
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

pub(super) async fn commit_rejects_carried_nondefault_node_budget(
    store: Arc<dyn RuntimePersistence>,
) {
    const CONFIGURED_NODE_LIMIT: usize = 1;
    let state = RuntimeSessionState {
        session_id: SessionId::from("root"),
        ..RuntimeSessionState::new(crate::SessionPolicy::new(crate::TurnBudget::Unbounded))
    };
    let parent = sample_session_node(&SessionId::from("root"), "budget-frame", None);
    let child = sample_session_node(
        &SessionId::from("root"),
        "budget-child",
        Some(&parent.node_id),
    );
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

pub(super) async fn commit_rejects_carried_nondefault_byte_budget(
    store: Arc<dyn RuntimePersistence>,
) {
    const CONFIGURED_BYTE_LIMIT: usize = 64;
    let state = RuntimeSessionState {
        session_id: SessionId::from("root"),
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

pub(super) fn commit_budget_conformance_fixture(byte_limit: usize) -> RuntimeCommit {
    let state = RuntimeSessionState {
        session_id: SessionId::from("root"),
        ..RuntimeSessionState::new(crate::SessionPolicy::new(crate::TurnBudget::Unbounded))
    };
    RuntimeCommit::persisted_state_for_test_with_budget(
        &state,
        &[],
        crate::CommitBudget::new(
            crate::CommitBudgetLimit::bounded(byte_limit),
            crate::CommitBudgetLimit::Unbounded,
        ),
    )
}

pub(super) async fn commit_rejects_queue_batch_bytes_over_budget(
    store: Arc<dyn RuntimePersistence>,
) {
    const BYTE_LIMIT: usize = 2_048;
    let mut commit = commit_budget_conformance_fixture(BYTE_LIMIT);
    commit
        .validate_budget()
        .expect("the commit without a queue batch must fit");
    commit.enqueued_queue_batches = vec![QueuedWorkBatchDraft::new(
        "root",
        DeliveryPolicy::AfterCurrentTurnCommit,
        crate::TurnWorkPayload::agent_frame_task(
            crate::session_graph::frame_node_id(&SessionId::from("root"), "oversized-queue-batch"),
            "q".repeat(BYTE_LIMIT * 2),
            None,
        ),
    )];

    let error = store
        .commit_runtime_state(commit)
        .await
        .expect_err("queue batch bytes alone must trip the commit budget");
    assert!(matches!(
        error,
        StoreError::CommitByteBudgetExceeded {
            queue_batch_bytes,
            max_bytes: BYTE_LIMIT,
            ..
        } if queue_batch_bytes > BYTE_LIMIT
    ));
}

pub(super) async fn commit_rejects_agent_frame_bytes_over_budget(
    store: Arc<dyn RuntimePersistence>,
) {
    const BYTE_LIMIT: usize = 2_048;
    let mut commit = commit_budget_conformance_fixture(BYTE_LIMIT);
    commit
        .validate_budget()
        .expect("the commit without an agent frame must fit");
    commit.current_frame_node_id = Some(crate::FrameNodeId::new("f".repeat(BYTE_LIMIT * 2)));

    let error = store
        .commit_runtime_state(commit)
        .await
        .expect_err("agent frame bytes alone must trip the commit budget");
    assert!(matches!(
        error,
        StoreError::CommitByteBudgetExceeded {
            agent_frame_bytes,
            max_bytes: BYTE_LIMIT,
            ..
        } if agent_frame_bytes > BYTE_LIMIT
    ));
}

pub(super) async fn commit_rejects_usage_delta_bytes_over_budget(
    store: Arc<dyn RuntimePersistence>,
) {
    const BYTE_LIMIT: usize = 2_048;
    let mut commit = commit_budget_conformance_fixture(BYTE_LIMIT);
    commit
        .validate_budget()
        .expect("the commit without a usage delta must fit");
    commit.usage_deltas = crate::store::RuntimeUsageDelta::for_operation(
        &commit.turn_commit.operation,
        &[TokenLedgerEntry {
            source: "u".repeat(BYTE_LIMIT * 2),
            model: "budget-model".to_string(),
            usage: TokenUsage::default(),
            usage_disposition: Default::default(),
        }],
    )
    .expect("identify the oversized usage delta");

    let error = store
        .commit_runtime_state(commit)
        .await
        .expect_err("usage delta bytes alone must trip the commit budget");
    assert!(matches!(
        error,
        StoreError::CommitByteBudgetExceeded {
            usage_delta_bytes,
            max_bytes: BYTE_LIMIT,
            ..
        } if usage_delta_bytes > BYTE_LIMIT
    ));
}

pub(super) async fn commit_rejects_turn_result_bytes_over_budget(
    store: Arc<dyn RuntimePersistence>,
) {
    const BYTE_LIMIT: usize = 2_048;
    let mut commit = commit_budget_conformance_fixture(BYTE_LIMIT);
    commit
        .validate_budget()
        .expect("the commit with its ordinary turn result must fit");
    commit.turn_commit = RuntimeTurnCommitStamp::new(crate::OperationId::new(
        crate::ExecutionScope::runtime_operation("t".repeat(BYTE_LIMIT * 2)),
        "commit",
    ));

    let error = store
        .commit_runtime_state(commit)
        .await
        .expect_err("turn result bytes alone must trip the commit budget");
    assert!(matches!(
        error,
        StoreError::CommitByteBudgetExceeded {
            turn_result_bytes,
            max_bytes: BYTE_LIMIT,
            ..
        } if turn_result_bytes > BYTE_LIMIT
    ));
}

pub(super) async fn commit_with_every_payload_family_inside_budget_succeeds(
    store: Arc<dyn RuntimePersistence>,
) {
    const BYTE_LIMIT: usize = 64 * 1024;
    let mut state = RuntimeSessionState {
        session_id: SessionId::from("root"),
        ..RuntimeSessionState::new(crate::SessionPolicy::new(crate::TurnBudget::Unbounded))
    };
    state.ensure_agent_frame_initialized();
    let usage = TokenLedgerEntry {
        source: "all-families".to_string(),
        model: "budget-model".to_string(),
        usage: TokenUsage {
            input_tokens: 1,
            output_tokens: 2,
            ..TokenUsage::default()
        },
        usage_disposition: Default::default(),
    };
    let mut commit = RuntimeCommit::persisted_state_for_test_with_budget(
        &state,
        &[usage],
        crate::CommitBudget::new(
            crate::CommitBudgetLimit::bounded(BYTE_LIMIT),
            crate::CommitBudgetLimit::Unbounded,
        ),
    );
    commit.committed_attachment_ids =
        vec![AttachmentId::parse("all-families-attachment").expect("valid attachment id")];
    commit.enqueued_queue_batches = vec![QueuedWorkBatchDraft::new(
        "root",
        DeliveryPolicy::AfterCurrentTurnCommit,
        crate::TurnWorkPayload::agent_frame_task(
            crate::session_graph::frame_node_id(&SessionId::from("root"), "all-families-follow-up"),
            "follow-up",
            None,
        ),
    )];

    store
        .commit_runtime_state(commit)
        .await
        .expect("a commit with every payload family inside the limit must succeed");
}

pub(super) async fn head_retirement_gate_distinguishes_leaf_change_from_same_leaf(
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
            requested_ancestor_is_active: true,
            occupied_node_ids: std::collections::HashSet::new(),
            selected_leaf_is_live: true,
            has_live_nodes: true,
            published_leaf: crate::store::PublishedLeafFacts::Live(crate::store::ParentNodeFacts {
                node_id: old_leaf.clone(),
                generation: state.session_graph.active_path_nodes().len() as u64 - 1,
                frame_node_id: seed_frame_node_id.to_string(),
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
            requested_ancestor_is_active: true,
            occupied_node_ids: std::collections::HashSet::new(),
            selected_leaf_is_live: false,
            has_live_nodes: true,
            published_leaf: crate::store::PublishedLeafFacts::Live(crate::store::ParentNodeFacts {
                node_id: old_leaf.clone(),
                generation: state.session_graph.active_path_nodes().len() as u64 - 1,
                frame_node_id: seed_frame_node_id.into_inner(),
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

pub(super) async fn load_retains_reasoning_only_usage(store: Arc<dyn RuntimePersistence>) {
    let state = RuntimeSessionState {
        session_id: SessionId::from("root"),
        ..RuntimeSessionState::new(crate::SessionPolicy::new(crate::TurnBudget::Unbounded))
    };
    let usage = TokenLedgerEntry {
        source: "reasoning-only".to_string(),
        model: "usage-model".to_string(),
        usage: TokenUsage {
            reasoning_output_tokens: 9,
            ..TokenUsage::default()
        },
        usage_disposition: Default::default(),
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

/// FIG-2765: the durable row, not process memory, is what says which calls were
/// billed but never counted. Every disposition — reported, hole, correction, and
/// an explicit zero-valued correction — must survive a store round trip with its
/// hole identities intact, and the outstanding set must be rebuildable from the
/// rows alone.
pub(super) async fn load_retains_usage_dispositions_and_rebuilds_outstanding_attempts(
    store: Arc<dyn RuntimePersistence>,
) {
    let state = RuntimeSessionState {
        session_id: SessionId::from("root"),
        ..RuntimeSessionState::new(crate::SessionPolicy::new(crate::TurnBudget::Unbounded))
    };
    let hole =
        |call_id: &str, ordinal: u32, generation: Option<&str>| crate::UnreportedLedgerAttempt {
            call_id: call_id.to_string(),
            attempt_ordinal: ordinal,
            generation_id: generation.map(str::to_string),
        };
    let rows = [
        TokenLedgerEntry::reported(
            "turn",
            "openrouter/model",
            TokenUsage {
                input_tokens: 12,
                ..TokenUsage::default()
            },
        ),
        TokenLedgerEntry {
            source: "turn".to_string(),
            model: "openrouter/model".to_string(),
            usage: TokenUsage::default(),
            usage_disposition: crate::LedgerUsageDisposition::unreported([
                hole("call-a", 0, Some("gen-a")),
                hole("call-b", 2, None),
                hole("call-c", 1, Some("gen-c")),
            ]),
        },
        TokenLedgerEntry {
            source: "turn".to_string(),
            model: "openrouter/model".to_string(),
            usage: TokenUsage {
                input_tokens: 334,
                ..TokenUsage::default()
            },
            usage_disposition: crate::LedgerUsageDisposition::Reconciled {
                call_id: "call-a".to_string(),
                attempt_ordinal: 0,
            },
        },
        // An explicit zero correction is information: the provider answered and
        // the charge really was nothing. It must not be mistaken for an empty
        // row and dropped.
        TokenLedgerEntry {
            source: "turn".to_string(),
            model: "openrouter/model".to_string(),
            usage: TokenUsage::default(),
            usage_disposition: crate::LedgerUsageDisposition::Reconciled {
                call_id: "call-c".to_string(),
                attempt_ordinal: 1,
            },
        },
    ];
    commit_runtime_state_for_test(
        &store,
        RuntimeCommit::persisted_state_for_test(&state, &rows),
        "usage disposition seed",
    )
    .await
    .expect("seed durable usage dispositions");

    let read = store
        .load_session()
        .await
        .expect("load usage dispositions")
        .expect("usage disposition session exists");
    assert_eq!(read.token_ledger.len(), 4, "one row per disposition");
    let dispositions = read
        .token_ledger
        .iter()
        .map(|entry| entry.usage_disposition.clone())
        .collect::<Vec<_>>();
    for expected in rows.iter().map(|row| &row.usage_disposition) {
        assert!(
            dispositions.contains(expected),
            "durable read lost a usage disposition: {expected:?} not in {dispositions:?}"
        );
    }

    let outstanding = crate::runtime::outstanding_unreported_attempts(&read.token_ledger);
    let keys = outstanding
        .iter()
        .map(|attempt| (attempt.call_id.as_str(), attempt.attempt_ordinal))
        .collect::<Vec<_>>();
    assert_eq!(
        keys,
        vec![("call-b", 2)],
        "corrected attempts are filled; the uncorrected hole survives the reload"
    );
    assert_eq!(
        outstanding[0].generation_id, None,
        "a hole with no generation id survives as a hole, not as missing data"
    );
    assert_eq!(outstanding[0].source, "turn");
    assert_eq!(outstanding[0].model, "openrouter/model");

    let report = crate::SessionUsageReport::from_entries(&read.token_ledger);
    assert_eq!(report.usage.usage.input_tokens, 346);
    assert_eq!(report.usage.unreported_attempts, 1);
    assert_eq!(report.usage.reconciled_attempts, 2);
}

pub(super) async fn load_rejects_token_usage_overflow(store: Arc<dyn RuntimePersistence>) {
    let state = RuntimeSessionState {
        session_id: SessionId::from("root"),
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
            usage_disposition: Default::default(),
        },
        TokenLedgerEntry {
            source: "overflow".to_string(),
            model: "usage-model".to_string(),
            usage: TokenUsage {
                input_tokens: 1,
                ..TokenUsage::default()
            },
            usage_disposition: Default::default(),
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

pub(super) async fn checkpoint_restore_rejects_turn_index_without_increment_headroom(
    store: Arc<dyn RuntimePersistence>,
) {
    let turn_index = usize::MAX - 16;
    let state = RuntimeSessionState {
        session_id: SessionId::from("root"),
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
pub(super) async fn checkpoint_restore_rejects_token_usage_whose_prompt_subtotal_overflows(
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
        session_id: SessionId::from("root"),
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

pub(super) async fn usage_delta_identity_is_idempotent_across_commits(
    store: Arc<dyn RuntimePersistence>,
) {
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
        usage_disposition: Default::default(),
    };
    let state = RuntimeSessionState {
        session_id: SessionId::from("root"),
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
