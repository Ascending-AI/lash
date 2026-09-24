//! Named backend test registration. Every generated test owns a fresh fixture.

mod session_ingress;
mod tool_child;
mod turn_crash;
mod turn_ingress;

/// Expansion machinery for the runtime-persistence registration macros.
#[macro_export]
macro_rules! __runtime_persistence_register {
    (reopenable $fixture:block;
        stores [$(( $store_law:ident, $store_label:literal )),* $(,)?]
        store_refs [$(( $store_ref_law:ident, $store_ref_label:literal )),* $(,)?]
        factories [$(( $factory_law:ident, $factory_label:literal )),* $(,)?]
        timed_stores [$(( $timed_law:ident, $timed_label:literal )),* $(,)?]
        timed_factories [$(( $timed_factory_law:ident, $timed_factory_label:literal )),* $(,)?]
    ) => {
        $(
            #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
            async fn $store_law() {
                let (_fixture_guard, make, _lease_timing) = $fixture;
                $crate::runtime_persistence_macro_support::$store_law(make($store_label).open)
                    .await;
                $crate::law_receipt::record(module_path!(), stringify!($store_law), $store_label);
            }
        )*
        $(
            #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
            async fn $store_ref_law() {
                let (_fixture_guard, make, _lease_timing) = $fixture;
                let store = make($store_ref_label).open;
                $crate::runtime_persistence_macro_support::$store_ref_law(store.as_ref()).await;
                $crate::law_receipt::record(module_path!(), stringify!($store_ref_law), $store_ref_label);
            }
        )*
        $(
            #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
            async fn $factory_law() {
                let (_fixture_guard, make, _lease_timing) = $fixture;
                $crate::runtime_persistence_macro_support::$factory_law(
                    |label| make(label).open,
                    $factory_label,
                )
                .await;
                $crate::law_receipt::record(module_path!(), stringify!($factory_law), $factory_label);
            }
        )*
        $(
            #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
            async fn $timed_law() {
                let (_fixture_guard, make, lease_timing) = $fixture;
                $crate::runtime_persistence_macro_support::$timed_law(
                    make($timed_label).open,
                    &lease_timing,
                )
                .await;
                $crate::law_receipt::record(module_path!(), stringify!($timed_law), $timed_label);
            }
        )*
        $(
            #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
            async fn $timed_factory_law() {
                let (_fixture_guard, make, lease_timing) = $fixture;
                $crate::runtime_persistence_macro_support::$timed_factory_law(
                    &|| make($timed_factory_label).open,
                    &lease_timing,
                )
                .await;
                $crate::law_receipt::record(module_path!(), stringify!($timed_factory_law), $timed_factory_label);
            }
        )*
    };
}

/// The runtime-persistence law catalogue, registered through
/// `runtime_persistence_reopenable_tests!`.
#[macro_export]
macro_rules! runtime_persistence_tests {
    (@catalogue $mode:ident $fixture:block) => {
        $crate::__runtime_persistence_register! {
            $mode $fixture;
            stores [
            (commit_increments_head_and_round_trips_agent_frames, "root"),
            (concurrent_head_revision_cas_applies_exactly_once, "concurrent-head-cas"),
            (queued_run_advance_rejects_unassigned_members_but_keeps_checkpoint_claims, "queued-run-provenance"),
            (queued_run_active_turn_member_reclaims_after_lane_rotation, "queued-run-active-reclaim"),
            (queued_run_repaired_checkpoint_input_remains_deferred_after_settle, "queued-run-repaired-input"),
            (queued_run_advance_repair_survives_later_terminal_settlement, "queued-run-advance-repair-runtime"),
            (queued_run_advance_repair_survives_host_terminal_settlement, "queued-run-advance-repair-host"),
            (queued_run_continuation_commits_outbox_and_retains_receipts, "queued-run-continuation"),
            (queued_run_exact_selection_never_commits_a_partial_claim, "queued-run-exact"),
            (queued_run_terminal_disposition_preserves_unassigned_work, "queued-run-disposition"),
            (queued_run_cancelled_follow_on_receipt_retains_withheld_members, "queued-run-cancelled-follow-on"),
            (queued_run_checkpoint_assignment_survives_lane_rotation, "queued-run-checkpoint-assignment"),
            (queued_run_resume_retakes_its_open_checkpoint_assignments, "queued-run-resume-retakes-assignments"),
            (queued_run_frozen_batches_survive_takeover_and_changed_limits, "queued-run-batches"),
            (queued_run_selected_excludes_pending_input, "queued-run-selected-composition"),
            (queued_run_automatic_prefers_pending_input, "queued-run-automatic-composition"),
            (queued_run_refused_selection_can_settle_empty, "queued-run-refused-selection"),
            (queued_run_members_survive_host_cancellation_after_lane_rotation, "queued-run-host-cancel-fence"),
            (queued_run_identity_survives_lane_rotation, "queued-run-identity"),
            (queued_run_commit_receipt_precedes_revisions_but_not_lane_fence, "queued-run-receipt"),
            (queued_run_selection_excludes_later_input_after_takeover, "queued-run-selection"),
            (commit_rejects_a_different_session_id, "alpha"),
            (commit_rejects_carried_nondefault_node_budget, "root"),
            (commit_rejects_carried_nondefault_byte_budget, "root"),
            (commit_rejects_queue_batch_bytes_over_budget, "root"),
            (commit_rejects_agent_frame_bytes_over_budget, "root"),
            (commit_rejects_usage_delta_bytes_over_budget, "root"),
            (commit_rejects_turn_result_bytes_over_budget, "root"),
            (commit_with_every_payload_family_inside_budget_succeeds, "root"),
            (load_hydrates_checkpoint_and_usage, "hydrated"),
            (load_retains_reasoning_only_usage, "root"),
            (load_retains_usage_dispositions_and_rebuilds_outstanding_attempts, "root"),
            (checkpoint_restore_rejects_turn_index_without_increment_headroom, "root"),
            (checkpoint_restore_rejects_token_usage_whose_prompt_subtotal_overflows, "root"),
            (load_rejects_token_usage_overflow, "root"),
            (usage_delta_identity_is_idempotent_across_commits, "root"),
            (usage_ordinal_reuse_with_different_payload_survives_receipt_replay, "root"),
            (execution_state_replace_then_clear_removes_the_live_checkpoint_ref, "execution-state-replace-then-clear"),
            (checkpoint_rejects_unknown_component_ref, "checkpoint-unknown-ref"),
            (session_read_loads_persisted_history, "branchy"),
            (session_prompt_layer_round_trips_through_the_committed_head, "session-prompt-layer"),
            (session_protocol_turn_options_round_trip_through_the_committed_head, "session-protocol-turn-options"),
            (session_metadata_round_trips, "root"),
            (session_metadata_relation_is_write_once, "root"),
            (attachment_manifest_records_intent_and_commit_stamps, "root"),
            (attachment_manifest_keeps_same_content_ownership_per_session, "root"),
            (attachment_manifest_reference_tracking_and_gc_root_set, "root"),
            (final_commit_stamp_is_idempotent_and_conflicts_on_changed_hash, "root"),
            (append_request_receipt_replays_after_head_advance, "root"),
            (append_request_receipt_rejects_changed_content, "root"),
            (append_request_exact_hash_rejects_changed_ancestor, "root"),
            (append_request_receipt_rejects_corrupt_node_count, "root"),
            (semantic_boundary_receipt_replays_after_head_advance, "root"),
            (semantic_boundary_receipt_rejects_changed_content, "root"),
            (semantic_boundary_receipt_rejects_mislabeled_identity, "root"),
            (concurrent_same_append_operation_applies_exactly_once, "root"),
            (legacy_append_receipt_keeps_exact_hash_semantics, "root"),
            (append_receipt_encoding_version_mismatch_keeps_exact_hash_semantics, "root"),
            (append_receipt_and_graph_append_are_atomic, "root"),
            (fresh_append_receipt_enforces_ancestor_precondition, "root"),
            (store_computed_hash_rejects_mutated_commit, "root"),
            (commit_rejects_non_derived_append_node_ids, "root"),
            (append_rejects_duplicate_batch_node_ids, "root"),
            (append_rejects_existing_node_id_collision, "root"),
            (head_retirement_gate_distinguishes_leaf_change_from_same_leaf, "root"),
            (committed_leaf_is_derived_from_the_terminal_appended_node, "root"),
            (preserve_head_commit_reports_the_resident_leaf, "root"),
            (empty_append_cannot_move_the_head, "empty-append-head-move"),
            (commit_rejects_leaf_without_frame_open_ancestor, "missing-frame-root"),
            (session_execution_lease_contract, "root"),
            (borrowed_session_execution_lease_commit_contract, "borrowed-commit-fence"),
            (same_incarnation_rotation_gates_claims_not_commits, "root"),
            (same_host_distinct_executors_are_lane_less_without_revoking_holder, "fig1133-same-host-session"),
            (concurrent_session_execution_lease_rotation_and_stale_renewal_are_linearizable, "concurrent-rotation-renewal"),
            (session_execution_lease_diagnostic_read_contract, "lease-diagnostic"),
            (session_execution_lease_displacement_contract, "lease-displacement"),
            (queued_work_source_keys_are_idempotent_and_list_ordered, "queued-work-source-keys"),
            (concurrent_queued_work_source_key_enqueues_report_one_inserted_and_one_existing, "concurrent-queued-work-source-key"),
            (decorated_queued_work_source_key_replay_reports_absorbed, "decorated-queued-work-source-key"),
            (pending_session_work_ordering_agrees_across_ingress_families, "pending-work-ordering"),
            (concurrent_queue_and_turn_input_claims_have_one_owner, "concurrent-queue-input"),
            (checkpoint_work_claims_both_families_once, "checkpoint-work"),
            (checkpoint_budget_refusal_preserves_active_turn_input, "checkpoint-budget-refusal"),
            (checkpoint_claims_honor_min_boundary_at_every_checkpoint, "checkpoint-min-boundary"),
            (queued_work_cancel_removes_only_unclaimed_batches, "queued-work-cancel"),
            (queued_work_exact_claim_uses_selected_batch_ids, "root"),
            (queued_work_classes_gate_command_and_turn_claims, "root"),
            (queued_work_claims_respect_boundaries_abandon_and_stale_completion, "root"),
            (same_generation_claim_scans_reach_rows_beyond_the_scan_surplus, "claim-scan"),
            (queued_work_respects_membership_limits_exclusivity_reclaim_and_sessions, "queued-membership"),
            (queued_work_join_groups_by_delivery_policy_and_merge_key, "queued-join"),
            (abandoned_predecessor_claim_pair_is_only_reclaimable_across_lease_generations, "abandoned-predecessor-generation"),
            (queued_work_redrive_preserves_interrupted_batch_composition, "redrive-composition"),
            (queued_work_redrive_obeys_delivery_boundary_before_identity, "redrive-boundary"),
            (queued_work_redrive_ignores_successor_row_limit, "redrive-row-limit"),
            (queued_work_redrive_ignores_a_changed_drain_policy, "redrive-drain-policy"),
            (queued_work_selected_multi_identity_validation_and_abandon_restore, "selected-multi-identity"),
            (queued_work_exact_claim_preserves_physical_order_and_key_breaks, "physical-order"),
            (process_wakes_batch_by_default, "wake-default-batch"),
            (queued_work_completion_is_lease_guarded, "root"),
            (queued_wake_delivery_is_source_key_idempotent_and_claimed_once, "root"),
            (host_cancelled_wake_is_not_redelivered, "root"),
            (queue_completion_and_turn_commit_stamp_are_atomic, "root"),
            (pending_turn_inputs_source_keys_order_cancel_and_cross_session, "root"),
            (pending_turn_input_duplicate_input_id, "root"),
            (identical_retry_after_defer_is_existing_not_conflict, "root"),
            (changed_retry_is_typed_conflict, "root"),
            (pending_turn_input_bulk_and_suffix_cancellation, "pending-bulk-cancel"),
            (pending_turn_input_claims_reclaim_complete_and_fence, "root"),
            (bound_turn_input_claim_is_excluded_until_its_turn_retakes_it, "turn-bound-claims"),
            (bound_claim_skips_a_pending_queued_runs_inputs, "turn-bound-queued-run"),
            (turn_park_lives_while_its_turn_holds_work, "turn-parks"),
            (turn_input_application_identity_survives_pending_tombstone_vacuum, "turn-input-application"),
            (active_turn_input_claim_reacquires_after_unrecorded_checkpoint, "fig905-active-reacquire"),
            (pending_turn_input_cancel_covers_active_and_deferred_states, "root"),
            (pending_active_turn_inputs_defer_unaccepted_once_on_interrupt, "root"),
            (a_turn_that_cannot_commit_leaves_no_input_pinned_to_it, "root"),
            (committed_turn_receipt_answers_the_parent_end_recovery_read, "root"),
            ]
            store_refs [
            (session_execution_lease_fence_authority, "lease-fence-authority"),
            ]
            factories [
            (plugin_state_boundary, "plugin-state"),
            ]
            timed_stores [
            (durable_queued_drain_wait_store_laws, "durable-queued-drain"),
            (queued_work_claims_supersede_across_session_lease_generations_with_timing, "root"),
            (claim_liveness_for_lease_less_paths_tracks_session_generations, "claim-liveness"),
            (accepted_turn_input_with_dead_lease_is_cancelled_and_vacuumed, "fig1511-orphaned-accepted"),
            (queued_work_names_a_deferred_lane_apart_from_an_exhausted_one, "deferred-versus-exhausted"),
            (queued_work_redrive_selects_claim_identity_across_ready_gap, "redrive-ready-gap"),
            (turn_input_claims_supersede_across_session_lease_generations_with_timing, "root"),
            ]
            timed_factories [
            (session_execution_lease_expires_by_ttl_contract, "ttl-expiry"),
            ]
        }
    };
}

/// Register the shared runtime-persistence laws plus durable reopen laws.
#[macro_export]
macro_rules! runtime_persistence_reopenable_tests {
    ($fixture:block) => {
        $crate::runtime_persistence_tests!(@catalogue reopenable $fixture);
        $crate::runtime_persistence_reopenable_tests!(@reopen_laws $fixture;
            [
                (reopen_mint_identity, "pending-turn-input-multi-store-mint"),
                (gc_blobs, "gc-blobs"),
                (append_receipt_reopen, "root"),
                (runtime_reopen, "root"),
            ]
        );
    };
    (@reopen_laws $fixture:block; [$(( $law:ident, $label:literal )),* $(,)?]) => {
        $(
            #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
            async fn $law() {
                let (_fixture_guard, make, _lease_timing) = $fixture;
                $crate::runtime_persistence_macro_support::$law(make($label)).await;
                $crate::law_receipt::record(module_path!(), stringify!($law), $label);
            }
        )*
    };
}

/// Expansion machinery for the process-registry registration macros.
#[macro_export]
macro_rules! __process_registry_register {
    (reopenable $fixture:block;
        probe [$(( $probe_law:ident, $probe_label:literal )),* $(,)?]
        conformance [$(( $conformance_law:ident, $conformance_label:literal )),* $(,)?]
        cancellation [$(( $cancellation_law:ident, $cancellation_label:literal )),* $(,)?]
        registry [$(( $registry_law:ident, $registry_label:literal )),* $(,)?]
    ) => {
        $(
            #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
            async fn $probe_law() {
                let (_fixture_guard, make) = $fixture;
                let _ = $probe_label;
                let open = |label: &str| make(label).open;
                $crate::registration_macro_support::$probe_law(&open).await;
                $crate::law_receipt::record(module_path!(), stringify!($probe_law), $probe_label);
            }
        )*
        $(
            #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
            async fn $cancellation_law() {
                let (_fixture_guard, make) = $fixture;
                $crate::registration_macro_support::process_registry_cancellation_reopen_contract(
                    make($cancellation_label),
                )
                .await;
                $crate::law_receipt::record(module_path!(), stringify!($cancellation_law), $cancellation_label);
            }
        )*
        $(
            #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
            async fn $conformance_law() {
                let (_fixture_guard, make) = $fixture;
                $crate::registration_macro_support::$conformance_law(
                    make($conformance_label).open,
                )
                .await;
                $crate::law_receipt::record(module_path!(), stringify!($conformance_law), $conformance_label);
            }
        )*
        $(
            #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
            async fn $registry_law() {
                let (_fixture_guard, make) = $fixture;
                let registry: std::sync::Arc<
                    dyn $crate::registration_macro_support::ProcessRegistry,
                > =
                    make($registry_label).open;
                $crate::registration_macro_support::$registry_law(registry).await;
                $crate::law_receipt::record(module_path!(), stringify!($registry_law), $registry_label);
            }
        )*
    };
}

/// The process-registry law catalogue, registered through
/// `process_registry_reopenable_tests!`.
#[macro_export]
macro_rules! process_registry_tests {
    (@catalogue $mode:ident $fixture:block) => {
        $crate::__process_registry_register! {
            $mode $fixture;
            probe [
                (process_registry_fresh_instances, "process-registry-fresh-instance-probe"),
            ]
            conformance [
                (process_registry_registration_contract, "process-registry-registration"),
                (empty_tool_call_identifiers_leave_no_row, "empty-tool-call-identifiers"),
                (process_namespace, "process-namespace"),
                (process_event_append_arms_are_ordered, "process-event-append-arms"),
                (a_failed_observer_transfer_leaves_no_partial_mutation, "observer-transfer-rollback"),
            ]
            cancellation [
                (process_registry_cancellation_contract, "process-registry-cancellation"),
            ]
            registry [
                (live_reference_summary_tracks_non_terminal_reference_counts, "live-reference-summary"),
                (registration_and_observers_are_atomic, "registration-observers"),
                (observer_events_are_auditable_and_transfer_is_atomic, "observer-audit-transfer"),
                (generic_append_rejects_reserved_edge_audit_events, "reserved-edge-events"),
                (canonical_process_event_payload_replay, "canonical-event-replay"),
                (count_events_through_counts_every_event_at_any_top_bound, "count-events-through-top-bound"),
                (long_cancellation_requester_replay_is_backend_safe, "long-cancellation-replay"),
                (wake_subscription_is_indexed_and_retargetable, "wake-subscription"),
                (lifecycle_status_and_outcome_fold, "lifecycle-fold"),
                (producer_terminal_status_must_match_materialized_outcome, "terminal-status-outcome"),
                (list_filters_match_extracted_and_json_fields, "list-filters"),
                (process_registry_pagination, "pagination"),
                (process_event_pages_reject_out_of_range_sequences, "process-event-page-sql-cursor-range"),
                (waiting_processes_remain_in_the_recovery_worklist, "waiting-worklist"),
                (list_processes_filters_by_enriched_fields, "enriched-filters"),
                (list_processes_bounds_retired_rows_without_hiding_live_rows, "retired-bounds"),
                (list_processes_filters_by_parent_scope_and_pending_cancel, "parent-scope-cancel-filters"),
                (process_change_feed_never_misses_concurrent_terminal_writers, "concurrent-terminal-feed"),
                (process_lease_fencing_contract, "lease-fencing"),
                (superseded_process_lease_cannot_release_or_complete, "lease-takeover-release"),
                (process_lease_batch_read_matches_point_reads, "lease-batch-read"),
                (session_delete_preserves_process_bytes, "session-delete-bytes"),
                (refolded_process_record_matches_hot_projection, "hot-refold"),
                (process_attempt_budget_is_typed, "attempt-budget"),
                (redriven_child_reregisters_with_the_recorded_attempt_bound, "redriven-attempt-bound"),
                (tombstones_make_pruned_processes_distinguishable, "tombstones"),
                (reused_process_ids_refuse_superseded_incarnations, "incarnation-reuse"),
                (watched_process_registry_reused_process_ids_refuse_superseded_incarnations, "watched-incarnation-reuse"),
                (lifecycle_transition_refusals_are_backend_invariant, "transition-refusals"),
                (external_ref_is_written_compare_and_set_by_segment_ordinal, "external-ref-compare-and-set"),
                (registration_reports_created_then_existing, "registration-disposition"),
                (concurrent_identical_registrations_are_idempotent, "concurrent-registration-repeat"),
                (caller_departure_state_machine, "caller-departure"),
                (caller_departed_rows_are_reclaimed_by_retention, "caller-departed-retention"),
                (terminal_completion_atomically_retains_parent_end_plan, "parent-end-plan"),
                (settled_parent_end_plans_are_reclaimed_by_retention, "parent-end-plan-reclaim"),
                (a_turn_scope_ends_through_its_recorded_ledger_row, "turn-parent-end"),
                (scopes_that_collide_in_rendering_share_no_ledger_key, "colliding-scope-keys"),
                (an_unrecorded_turn_parent_is_reported_until_its_row_is_written, "unrecorded-turn-parents"),
                (process_prune_scoped_by_originator, "scoped-prune"),
                (process_prune_batch_tombstones, "batch-prune"),
            ]
        }
    };
}

/// Register the process-registry laws plus the durable reopen law.
#[macro_export]
macro_rules! process_registry_reopenable_tests {
    ($fixture:block) => {
        $crate::process_registry_tests!(@catalogue reopenable $fixture);
        $crate::process_registry_reopenable_tests!(@reopen $fixture;
            [
                (process_registry_reopen_conformance, "process-registry-reopen"),
            ]
        );
    };
    (@reopen $fixture:block; [$(( $law:ident, $label:literal )),* $(,)?]) => {
        $(
            #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
            async fn $law() {
                let (_fixture_guard, make) = $fixture;
                $crate::registration_macro_support::$law(make($label)).await;
                $crate::law_receipt::record(module_path!(), stringify!($law), $label);
            }
        )*
    };
}

/// Register one independently reported test per durable store-recovery law.
#[macro_export]
macro_rules! store_recovery_tests {
    ($fixture:block) => {
        $crate::store_recovery_tests!(@catalogue $fixture;
            timed [
                (expired_claim_is_recoverable_once, "store-recovery-expired-claim"),
                (checkpoint_survives_before_claim_settlement, "store-recovery-checkpoint"),
            ]
            plain [
                (store_recovery_fresh_instances, "store-recovery-fresh-instance-probe"),
                (atomic_commit_settles_claim_once, "store-recovery-atomic-commit"),
                (recorded_commit_replay_is_idempotent, "store-recovery-replay"),
            ]
        );
    };
    (@catalogue $fixture:block;
        timed [$(( $timed_law:ident, $timed_label:literal )),* $(,)?]
        plain [$(( $plain_law:ident, $plain_label:literal )),* $(,)?]
    ) => {
        $(
            #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
            async fn $timed_law() {
                let (_fixture_guard, make, lease_timing) = $fixture;
                $crate::registration_macro_support::$timed_law(
                    &make,
                    $timed_label,
                    &lease_timing,
                )
                .await;
                $crate::law_receipt::record(module_path!(), stringify!($timed_law), $timed_label);
            }
        )*
        $(
            #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
            async fn $plain_law() {
                let (_fixture_guard, make, _lease_timing) = $fixture;
                $crate::registration_macro_support::$plain_law(&make, $plain_label).await;
                $crate::law_receipt::record(module_path!(), stringify!($plain_law), $plain_label);
            }
        )*
    };
}

/// Register the maintenance laws that require no backend fault injector.
#[macro_export]
macro_rules! store_maintenance_tests {
    ($fixture:block) => {
        $crate::store_maintenance_tests!(@catalogue $fixture;
            sync [
                (report_failure_channels_are_incomplete, "maintenance-report-failures"),
            ]
            async [
                (idle_store_reports_witnessed_nothing_to_do, "maintenance-idle"),
                (superseded_checkpoint_is_a_witnessed_sweep, "maintenance-sweep"),
                (empty_root_set_refusal_returns_its_partial_report, "maintenance-refusal"),
            ]
        );
    };
    (@catalogue $fixture:block;
        sync [$(( $sync_law:ident, $sync_label:literal )),* $(,)?]
        async [$(( $async_law:ident, $async_label:literal )),* $(,)?]
    ) => {
        $(
            #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
            async fn $sync_law() {
                let (_fixture_guard, backend, _make) = $fixture;
                let _ = $sync_label;
                $crate::registration_macro_support::$sync_law(backend);
                $crate::law_receipt::record(module_path!(), stringify!($sync_law), $sync_label);
            }
        )*
        $(
            #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
            async fn $async_law() {
                let (_fixture_guard, backend, make) = $fixture;
                let _ = $async_label;
                $crate::registration_macro_support::$async_law(backend, make()).await;
                $crate::law_receipt::record(module_path!(), stringify!($async_law), $async_label);
            }
        )*
    };
}

#[macro_export]
macro_rules! store_maintenance_fault_tests {
    ($fixture:block) => {
        $crate::store_maintenance_fault_tests!(@catalogue $fixture; [
            (sweep_failure_is_not_an_empty_report, "maintenance-failure"),
        ]);
    };
    (@catalogue $fixture:block; [$(( $law:ident, $label:literal )),* $(,)?]) => {
        $(
            #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
            async fn $law() {
                let (_fixture_guard, backend, make, fault) = $fixture;
                let _ = $label;
                $crate::registration_macro_support::$law(backend, make(), fault.as_ref()).await;
                $crate::law_receipt::record(module_path!(), stringify!($law), $label);
            }
        )*
    };
}

/// Expansion machinery for effect-group host registration.
#[macro_export]
macro_rules! __effect_group_host_register {
    ([$($attr:tt)*] $fixture:block; $law:ident, $label:literal, wired) => {
        $($attr)*
        #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
        async fn $law() {
            let (_fixture_guard, factory) = $fixture;
            let make = || {
                factory(Some(
                    $crate::registration_macro_support::effect_group_suite_executors(),
                ))
            };
            let prefix = $crate::registration_macro_support::effect_group_test_prefix($label);
            $crate::registration_macro_support::$law(&make, &prefix).await;
            $crate::law_receipt::record(module_path!(), stringify!($law), $label);
        }
    };
    ([$($attr:tt)*] $fixture:block; $law:ident, $label:literal, unwired) => {
        $($attr)*
        #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
        async fn $law() {
            let (_fixture_guard, factory) = $fixture;
            let make = || factory(None);
            let prefix = $crate::registration_macro_support::effect_group_test_prefix($label);
            $crate::registration_macro_support::$law(&make, &prefix).await;
            $crate::law_receipt::record(module_path!(), stringify!($law), $label);
        }
    };
    ([$($attr:tt)*] $fixture:block; $law:ident, $label:literal, mixed) => {
        $($attr)*
        #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
        async fn $law() {
            let (_fixture_guard, factory) = $fixture;
            let unwired = || factory(None);
            let make = || {
                factory(Some(
                    $crate::registration_macro_support::effect_group_suite_executors(),
                ))
            };
            let prefix = $crate::registration_macro_support::effect_group_test_prefix($label);
            $crate::registration_macro_support::$law(&unwired, &make, &prefix).await;
            $crate::law_receipt::record(module_path!(), stringify!($law), $label);
        }
    };
}

/// Register one independently reported test per shared effect-group host law.
#[macro_export]
macro_rules! effect_group_host_tests {
    ($fixture:block) => {
        $crate::effect_group_host_tests!(@catalogue [] $fixture);
    };
    ($(#[$attr:meta])+ $fixture:block) => {
        $crate::effect_group_host_tests!(@catalogue [$(#[$attr])*] $fixture);
    };
    (@catalogue [$($attr:tt)*] $fixture:block) => {
        $crate::effect_group_host_tests!(@expand [$($attr)*] $fixture; [
            (cancel_stops_the_losers, "group-cancel", wired),
            (cancel_gives_every_unsettled_child_a_cancellation_terminal, "group-cancel-terminals", wired),
            (an_unregistered_host_refuses_all_three_from_wiring, "group-unwired", unwired),
            (a_refused_open_journals_nothing, "group-refused-open", mixed),
            (a_child_with_no_runner_refuses_the_open_and_refuses_the_retry, "group-no-runner", wired),
            (a_reopen_whose_runner_this_deployment_lost_is_not_an_open_refusal, "group-lost-runner", wired),
            (a_wired_host_serves_all_three_group_methods, "group-capability", wired),
            (a_proxied_controller_serves_all_three_group_methods, "group-proxied", wired),
            (wrong_scope_groups_are_refused_before_any_child_runs, "group-scope", wired),
            (duplicate_replay_keys_are_refused_before_a_host_sees_them, "group-duplicate-replay", wired),
            (the_first_settlement_wakes_the_caller_while_the_loser_still_runs, "group-first", wired),
            (a_closed_group_reopened_while_a_loser_still_runs_serves_its_settlements, "group-reopen-after-close", wired),
            (a_scope_with_a_live_group_child_is_not_quiescent, "group-live-child", wired),
            (a_closed_group_with_a_draining_loser_is_not_quiescent, "group-closed-draining-loser", wired),
            (settlement_n_is_stable_across_re_reads, "group-reread", wired),
            (every_child_is_delivered_once_in_rank_order, "group-order", wired),
            (siblings_settling_together_get_distinct_sequences, "group-concurrent-ranks", wired),
            (a_closed_group_serves_its_caller_no_further_settlements, "group-closed-caller", wired),
            (the_wake_rule_is_identity_and_the_host_filters_nothing, "group-wake-identity", wired),
            (awaiting_past_the_last_child_is_refused, "group-past-last", wired),
            (a_cancelled_await_leaves_the_rank_to_be_read_again, "group-cancelled-await", wired),
            (run_to_completion_losers_settle_after_the_caller_is_gone, "group-run-to-completion", wired),
            (a_close_may_narrow_but_never_widen, "group-close-narrowing", wired),
            (closing_twice_under_one_disposition_succeeds, "group-idempotent-close", wired),
            (a_reopen_is_fenced_on_shape_and_runs_no_child_twice, "group-reopen", wired),
            (a_second_host_instance_reads_the_ranks_the_first_recorded, "group-handoff", wired),
            (a_reopen_dispatches_the_retained_membership, "group-w1-membership", wired),
            (a_reopen_reissues_each_childs_original_identity, "group-w2-identity", wired),
            (a_losing_wait_stays_admitted_until_the_group_releases_it, "group-losing-wait", wired),
            (a_wait_cancelled_before_it_parks_is_still_released, "group-unparked-wait", wired),
        ]);
    };
    (@expand $attrs:tt $fixture:block; [$(( $law:ident, $label:literal, $mode:ident )),* $(,)?]) => {
        $(
            $crate::__effect_group_host_register!($attrs $fixture; $law, $label, $mode);
        )*
    };
}

/// Register the close-race law: a close racing its own children's settlements
/// seats exactly one terminal per child. Its own suite so a tier can park it
/// apart from the rest of the effect-group catalogue; the fixture is the
/// `effect_group_host_tests!` one.
#[macro_export]
macro_rules! effect_group_close_race_tests {
    ($fixture:block) => {
        $crate::effect_group_close_race_tests!(@catalogue [] $fixture);
    };
    ($(#[$attr:meta])+ $fixture:block) => {
        $crate::effect_group_close_race_tests!(@catalogue [$(#[$attr])*] $fixture);
    };
    (@catalogue [$($attr:tt)*] $fixture:block) => {
        $crate::effect_group_close_race_tests!(@expand [$($attr)*] $fixture; [
            (a_close_racing_its_children_seats_one_terminal_per_child, "group-close-race", wired),
        ]);
    };
    (@expand $attrs:tt $fixture:block; [$(( $law:ident, $label:literal, $mode:ident )),* $(,)?]) => {
        $(
            $crate::__effect_group_host_register!($attrs $fixture; $law, $label, $mode);
        )*
    };
}

/// Register the durable cancelled-child terminal law.
#[macro_export]
macro_rules! __effect_group_cancelled_child_terminal_register {
    ([$($attr:tt)*] $fixture:block; $law:ident, $label:literal) => {
        $($attr)*
        #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
        async fn $law() {
            let (_fixture_guard, factory) = $fixture;
            let _ = $label;
            $crate::registration_macro_support::$law(factory).await;
            $crate::law_receipt::record(module_path!(), stringify!($law), $label);
        }
    };
}

/// Register the durable cancelled-child terminal law.
#[macro_export]
macro_rules! effect_group_cancelled_child_terminal_tests {
    ($fixture:block) => {
        $crate::effect_group_cancelled_child_terminal_tests!(@catalogue [] $fixture);
    };
    ($(#[$attr:meta])+ $fixture:block) => {
        $crate::effect_group_cancelled_child_terminal_tests!(@catalogue [$(#[$attr])*] $fixture);
    };
    (@catalogue [$($attr:tt)*] $fixture:block) => {
        $crate::effect_group_cancelled_child_terminal_tests!(@expand [$($attr)*] $fixture; [
            (effect_group_cancelled_child_terminal_is_durable, "group-cancel-terminal"),
        ]);
    };
    (@expand $attrs:tt $fixture:block; [$(( $law:ident, $label:literal )),* $(,)?]) => {
        $(
            $crate::__effect_group_cancelled_child_terminal_register!(
                $attrs $fixture; $law, $label
            );
        )*
    };
}

/// The fixture yields `(guard, make, witness)`: the witness is how this host
/// proves its active-wait registration before the quiescence law asks for
/// retirement (`effect_host_journaled_wait_registration_witness` for a store
/// journal).
#[macro_export]
macro_rules! effect_host_await_event_tests {
    ($fixture:block) => {
        $crate::effect_host_await_event_tests!(@witnessed $fixture; [
            (effect_host_await_events_with_active_wait_witness, "effect-host-await-event"),
        ]);
        $crate::effect_host_await_event_tests!(@catalogue $fixture; [
            (
                completion_routing_pairwise_refusal,
                "completion-routing-pairwise"
            ),
        ]);
    };
    (@witnessed $fixture:block; [$(( $law:ident, $label:literal )),* $(,)?]) => {
        $(
            #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
            async fn $law() {
                let (_fixture_guard, make, witness) = $fixture;
                let _ = $label;
                $crate::registration_macro_support::$law(make, witness).await;
                $crate::law_receipt::record(module_path!(), stringify!($law), $label);
            }
        )*
    };
    (@catalogue $fixture:block; [$(( $law:ident, $label:literal )),* $(,)?]) => {
        $(
            #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
            async fn $law() {
                let (_fixture_guard, make, _witness) = $fixture;
                let _ = $label;
                $crate::registration_macro_support::$law(make).await;
                $crate::law_receipt::record(module_path!(), stringify!($law), $label);
            }
        )*
    };
}

#[macro_export]
macro_rules! effect_host_cold_await_event_tests {
    ($fixture:block) => {
        $crate::effect_host_cold_await_event_tests!(@catalogue $fixture; [
            (effect_host_await_events_cold_instance, "effect-host-cold-await-event"),
        ]);
    };
    (@catalogue $fixture:block; [$(( $law:ident, $label:literal )),* $(,)?]) => {
        $(
            #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
            async fn $law() {
                let (_fixture_guard, make) = $fixture;
                let _ = $label;
                $crate::registration_macro_support::$law(make).await;
                $crate::law_receipt::record(module_path!(), stringify!($law), $label);
            }
        )*
    };
}

/// Register the attachment-condemnation cold-reopen crash law.
#[macro_export]
macro_rules! attachment_condemnation_recovery_tests {
    ($fixture:block) => {
        $crate::attachment_condemnation_recovery_tests!(@catalogue $fixture; [
            (attachment_condemnation_delete_crash_survives_cold_reopen, "attachment-condemnation-delete-crash"),
        ]);
    };
    (@catalogue $fixture:block; [$(( $law:ident, $label:literal )),* $(,)?]) => {
        $(
            #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
            async fn $law() {
                let (_fixture_guard, factory, reopen) = $fixture;
                let _ = $label;
                $crate::registration_macro_support::$law(factory, reopen).await;
                $crate::law_receipt::record(module_path!(), stringify!($law), $label);
            }
        )*
    };
}

#[macro_export]
macro_rules! attachment_adoption_tests {
    ($fixture:block) => {
        $crate::attachment_adoption_tests!(@catalogue $fixture; [
            (cross_owner_attachment_adoption_conformance, "cross-owner-attachment-adoption"),
            (attachment_condemnation_enumeration_conformance, "attachment-condemnation-enumeration"),
            (attachment_owner_identity_round_trips_conformance, "attachment-owner-identity-round-trip"),
        ]);
    };
    (@catalogue $fixture:block; [$(( $law:ident, $label:literal )),* $(,)?]) => {
        $(
            #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
            async fn $law() {
                let (_fixture_guard, factory) = $fixture;
                let _ = $label;
                $crate::registration_macro_support::$law(factory).await;
                $crate::law_receipt::record(module_path!(), stringify!($law), $label);
            }
        )*
    };
}

/// Register one independently reported test per fork-lineage law.
#[macro_export]
macro_rules! lineage_tests {
    ($fixture:block) => {
        $crate::lineage_tests!(@catalogue $fixture; [
            (fork_lineage_conformance, "fork-lineage"),
            (fork_lineage_no_carrier_law, "fork-lineage-no-carrier"),
            (fork_plan_matches_edge_walk_law, "fork-plan-edge-walk"),
        ]);
    };
    (@catalogue $fixture:block; [$(( $law:ident, $label:literal )),* $(,)?]) => {
        $(
            #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
            async fn $law() {
                let (_fixture_guard, handles) = $fixture;
                let _ = $label;
                $crate::registration_macro_support::$law(handles).await;
                $crate::law_receipt::record(module_path!(), stringify!($law), $label);
            }
        )*
    };
}

#[macro_export]
macro_rules! process_change_horizon_tests {
    ($fixture:block) => {
        $crate::process_change_horizon_tests!(@catalogue $fixture; [
            (process_change_cursor_below_tombstone_compaction_horizon_is_refused, "process-change-horizon"),
        ]);
    };
    (@catalogue $fixture:block; [$(( $law:ident, $label:literal )),* $(,)?]) => {
        $(
            #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
            async fn $law() {
                let (_fixture_guard, registry) = $fixture;
                let _ = $label;
                $crate::registration_macro_support::$law(registry).await;
                $crate::law_receipt::record(module_path!(), stringify!($law), $label);
            }
        )*
    };
}

/// Register the laws every `Backend` implementation answers (ADR 0102).
///
/// The fixture yields `(guard, Arc<dyn Backend>)`.
#[macro_export]
macro_rules! backend_tests {
    ($fixture:block) => {
        $crate::backend_tests!(@catalogue $fixture; [
            (a_backend_binds_its_effect_host_to_its_identity, "backend-binding-identity"),
        ]);
    };
    (@catalogue $fixture:block; [$(( $law:ident, $label:literal )),* $(,)?]) => {
        $(
            #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
            async fn $law() {
                let (_fixture_guard, backend) = $fixture;
                let _ = $label;
                $crate::registration_macro_support::$law(backend).await;
                $crate::law_receipt::record(module_path!(), stringify!($law), $label);
            }
        )*
    };
}

/// Register the leased-completion projection-repair law.
#[macro_export]
macro_rules! process_projection_repair_tests {
    ($fixture:block) => {
        $crate::process_projection_repair_tests!(@catalogue $fixture; [
            (leased_completion_replay_repairs_projection, "leased-completion-projection-repair"),
        ]);
    };
    (@catalogue $fixture:block; [$(( $law:ident, $label:literal )),* $(,)?]) => {
        $(
            #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
            async fn $law() {
                let (_fixture_guard, registry, corrupt_projection) = $fixture;
                let _ = $label;
                $crate::registration_macro_support::$law(registry, corrupt_projection).await;
                $crate::law_receipt::record(module_path!(), stringify!($law), $label);
            }
        )*
    };
}

#[macro_export]
macro_rules! retention_tests {
    ($fixture:block) => {
        $crate::retention_tests!(@catalogue $fixture; [
            (retention_conformance, "retention"),
        ]);
    };
    (@catalogue $fixture:block; [$(( $law:ident, $label:literal )),* $(,)?]) => {
        $(
            #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
            async fn $law() {
                let (_fixture_guard, factory) = $fixture;
                let _ = $label;
                $crate::registration_macro_support::$law(factory).await;
                $crate::law_receipt::record(module_path!(), stringify!($law), $label);
            }
        )*
    };
}

/// Register the fork observer-intent recovery law.
#[macro_export]
macro_rules! observer_intent_tests {
    ($fixture:block) => {
        $crate::observer_intent_tests!(@catalogue $fixture; [
            (fork_observer_intent_transient_failure, "fork-observer-intent"),
        ]);
    };
    (@catalogue $fixture:block; [$(( $law:ident, $label:literal )),* $(,)?]) => {
        $(
            #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
            async fn $law() {
                let (_fixture_guard, factory) = $fixture;
                let _ = $label;
                $crate::registration_macro_support::$law(factory).await;
                $crate::law_receipt::record(module_path!(), stringify!($law), $label);
            }
        )*
    };
}

#[macro_export]
macro_rules! process_continuation_store_tests {
    ($fixture:block) => {
        $crate::process_continuation_store_tests!(@catalogue $fixture; [
            (process_continuation_store, "process-continuation-store"),
        ]);
    };
    (@catalogue $fixture:block; [$(( $law:ident, $label:literal )),* $(,)?]) => {
        $(
            #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
            async fn $law() {
                let (_fixture_guard, registry, store) = $fixture;
                let _ = $label;
                $crate::registration_macro_support::$law(registry, store).await;
                $crate::law_receipt::record(module_path!(), stringify!($law), $label);
            }
        )*
    };
}

#[macro_export]
macro_rules! process_trigger_retention_tests {
    ($fixture:block) => {
        $crate::process_trigger_retention_tests!(@catalogue $fixture; [
            (process_trigger_retention, "process-trigger-retention"),
        ]);
    };
    (@catalogue $fixture:block; [$(( $law:ident, $label:literal )),* $(,)?]) => {
        $(
            #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
            async fn $law() {
                let (_fixture_guard, make) = $fixture;
                let _ = $label;
                $crate::registration_macro_support::$law(make).await;
                $crate::law_receipt::record(module_path!(), stringify!($law), $label);
            }
        )*
    };
}

/// Register the store-contract state-machine law.
#[macro_export]
macro_rules! store_contract_state_machine_tests {
    ($fixture:block) => {
        $crate::store_contract_state_machine_tests!(@catalogue $fixture; [
            (store_contract_state_machine, "store-contract-state-machine"),
        ]);
    };
    (@catalogue $fixture:block; [$(( $law:ident, $label:literal )),* $(,)?]) => {
        $(
            #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
            async fn $law() {
                let (_fixture_guard, backend, make) = $fixture;
                let _ = $label;
                $crate::registration_macro_support::$law(backend, make).await;
                $crate::law_receipt::record(module_path!(), stringify!($law), $label);
            }
        )*
    };
}

#[macro_export]
macro_rules! runtime_persistence_state_machine_tests {
    ($fixture:block) => {
        $crate::runtime_persistence_state_machine_tests!(@catalogue $fixture; [
            (runtime_persistence_state_machine, "runtime-persistence-state-machine"),
        ]);
    };
    (@catalogue $fixture:block; [$(( $law:ident, $label:literal )),* $(,)?]) => {
        $(
            #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
            async fn $law() {
                let (_fixture_guard, backend, make) = $fixture;
                let _ = $label;
                $crate::registration_macro_support::$law(backend, make).await;
                $crate::law_receipt::record(module_path!(), stringify!($law), $label);
            }
        )*
    };
}

#[macro_export]
macro_rules! session_graph_state_machine_tests {
    ($fixture:block) => {
        $crate::session_graph_state_machine_tests!(@catalogue $fixture; [
            (session_graph_state_machine, "session-graph-state-machine"),
        ]);
    };
    (@catalogue $fixture:block; [$(( $law:ident, $label:literal )),* $(,)?]) => {
        $(
            #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
            async fn $law() {
                let (_fixture_guard, backend, make) = $fixture;
                let _ = $label;
                $crate::registration_macro_support::$law(backend, make).await;
                $crate::law_receipt::record(module_path!(), stringify!($law), $label);
            }
        )*
    };
}

#[macro_export]
macro_rules! session_delete_blob_reclaim_tests {
    ($fixture:block) => {
        $crate::session_delete_blob_reclaim_tests!(@catalogue $fixture; [
            (session_delete_blob_reclaim_conformance, "session-delete-blob-reclaim"),
        ]);
    };
    (@catalogue $fixture:block; [$(( $law:ident, $label:literal )),* $(,)?]) => {
        $(
            #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
            async fn $law() {
                let (_fixture_guard, backend, make) = $fixture;
                let _ = $label;
                $crate::registration_macro_support::$law(backend, make).await;
                $crate::law_receipt::record(module_path!(), stringify!($law), $label);
            }
        )*
    };
}

/// Register the zero-row session-lease renewal law.
#[macro_export]
macro_rules! session_execution_lease_renewal_tests {
    ($fixture:block) => {
        $crate::session_execution_lease_renewal_tests!(@catalogue $fixture; [
            (session_execution_lease_zero_row_renewal_is_refused, "session-lease-zero-row-renewal"),
        ]);
    };
    (@catalogue $fixture:block; [$(( $law:ident, $label:literal )),* $(,)?]) => {
        $(
            #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
            async fn $law() {
                let (_fixture_guard, handles) = $fixture;
                let _ = $label;
                $crate::registration_macro_support::$law(handles).await;
                $crate::law_receipt::record(module_path!(), stringify!($law), $label);
            }
        )*
    };
}

/// Register the durable tool-access recovery law.
#[macro_export]
macro_rules! tool_access_persistence_tests {
    ($fixture:block) => {
        $crate::tool_access_persistence_tests!(@catalogue $fixture; [
            (session_tool_access_durable_recovery, "tool-access-durable-recovery"),
        ]);
    };
    (@catalogue $fixture:block; [$(( $law:ident, $label:literal )),* $(,)?]) => {
        $(
            #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
            async fn $law() {
                let (_fixture_guard, persistence) = $fixture;
                let _ = $label;
                $crate::registration_macro_support::$law(persistence).await;
                $crate::law_receipt::record(module_path!(), stringify!($law), $label);
            }
        )*
    };
}

/// Register the durable reopenable trigger-store laws.
#[macro_export]
macro_rules! trigger_store_reopenable_tests {
    ($fixture:block) => {
        $crate::trigger_store_reopenable_tests!(@catalogue $fixture; [
            (trigger_store_reopenable, "trigger-store-reopenable"),
        ]);
    };
    (@catalogue $fixture:block; [$(( $law:ident, $label:literal )),* $(,)?]) => {
        $(
            #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
            async fn $law() {
                let (_fixture_guard, make) = $fixture;
                let _ = $label;
                $crate::registration_macro_support::$law(make).await;
                $crate::law_receipt::record(module_path!(), stringify!($law), $label);
            }
        )*
    };
}

/// Expansion machinery for trigger-retention fault laws.
#[macro_export]
macro_rules! __trigger_retention_fault_register {
    ($fixture:block; $law:ident, $label:literal, retention) => {
        #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
        async fn $law() {
            let (_fixture_guard, store, fault) = $fixture;
            let _ = $label;
            $crate::registration_macro_support::$law(store, fault.as_ref()).await;
            $crate::law_receipt::record(module_path!(), stringify!($law), $label);
        }
    };
}

/// Register the trigger-retention corruption and rollback laws.
#[macro_export]
macro_rules! trigger_retention_fault_tests {
    ($fixture:block) => {
        $crate::trigger_retention_fault_tests!(@catalogue $fixture; [
            (trigger_occurrence_retention_failure_law, "trigger-occurrence-retention-failure", retention),
            (trigger_retention_reconciliation_failure_law, "trigger-retention-reconciliation-failure", retention),
        ]);
    };
    (@catalogue $fixture:block; [$(( $law:ident, $label:literal, $mode:ident )),* $(,)?]) => {
        $(
            $crate::__trigger_retention_fault_register!($fixture; $law, $label, $mode);
        )*
    };
}

/// Register the persisted trigger-occurrence listing corruption law.
#[macro_export]
macro_rules! trigger_occurrence_listing_tests {
    ($fixture:block) => {
        $crate::trigger_occurrence_listing_tests!(@catalogue $fixture; [
            (trigger_occurrence_listing_corruption_law, "trigger-occurrence-listing-corruption"),
        ]);
    };
    (@catalogue $fixture:block; [$(( $law:ident, $label:literal )),* $(,)?]) => {
        $(
            #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
            async fn $law() {
                let (_fixture_guard, store, injector) = $fixture;
                let _ = $label;
                $crate::registration_macro_support::$law(store, injector.as_ref()).await;
                $crate::law_receipt::record(module_path!(), stringify!($law), $label);
            }
        )*
    };
}

/// Register the portable durable effect-controller replay laws.
#[macro_export]
macro_rules! effect_controller_replay_tests {
    ($fixture:block) => {
        $crate::effect_controller_replay_tests!(@catalogue $fixture; [
            (effect_controller_journaled_effect_replay, "effect-controller-journaled-replay"),
            (effect_controller_concurrent_replay_deterministic, "effect-controller-concurrent-replay"),
            (effect_controller_code_cell_replays_by_reexecution, "effect-controller-code-cell-reexecution"),
        ]);
    };
    ($fixture:block, $verify:expr) => {
        $crate::effect_controller_replay_tests!(@catalogue $fixture, $verify; [
            (effect_controller_journaled_effect_replay, "effect-controller-journaled-replay"),
            (effect_controller_concurrent_replay_deterministic, "effect-controller-concurrent-replay"),
            (effect_controller_code_cell_replays_by_reexecution, "effect-controller-code-cell-reexecution"),
        ]);
    };
    (@catalogue $fixture:block; [$(( $law:ident, $label:literal )),* $(,)?]) => {
        $(
            #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
            async fn $law() {
                let (_fixture_guard, make) = $fixture;
                $crate::registration_macro_support::$law(make).await;
                $crate::law_receipt::record(module_path!(), stringify!($law), $label);
            }
        )*
    };
    (@catalogue $fixture:block, $verify:expr; [$(( $law:ident, $label:literal )),* $(,)?]) => {
        $(
            #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
            async fn $law() {
                let (fixture_guard, make) = $fixture;
                $crate::registration_macro_support::$law(make).await;
                ($verify)($label, &fixture_guard);
                $crate::law_receipt::record(module_path!(), stringify!($law), $label);
            }
        )*
    };
}

/// Register effect-controller replay-mismatch diagnostics.
#[macro_export]
macro_rules! effect_controller_replay_mismatch_tests {
    ($fixture:block) => {
        $crate::effect_controller_replay_mismatch_tests!(@catalogue $fixture; [
            (effect_controller_replay_mismatch_diagnostics, "effect-controller-replay-mismatch"),
        ]);
    };
    (@catalogue $fixture:block; [$(( $law:ident, $label:literal )),* $(,)?]) => {
        $(
            #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
            async fn $law() {
                let (_fixture_guard, make, mismatch_code) = $fixture;
                let _ = $label;
                $crate::registration_macro_support::$law(make, mismatch_code).await;
                $crate::law_receipt::record(module_path!(), stringify!($law), $label);
            }
        )*
    };
}

/// Register the SQL effect-controller lease-fencing law.
#[macro_export]
macro_rules! effect_controller_lease_fencing_tests {
    ($fixture:block) => {
        $crate::effect_controller_lease_fencing_tests!(@catalogue $fixture; [
            (effect_controller_lease_fencing, "effect-controller-lease-fencing"),
            (effect_lease_renew_transient_error_keeps_tool_running, "effect-lease-renew-transient-error"),
            (effect_lease_renew_errors_past_budget_leave_row_reclaimable, "effect-lease-renew-budget-exhausted"),
            (effect_lease_renew_stall_is_abandoned_at_the_deadline, "effect-lease-renew-stalled"),
        ]);
    };
    (@catalogue $fixture:block; [$(( $law:ident, $label:literal )),* $(,)?]) => {
        $(
            #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
            async fn $law() {
                let (_fixture_guard, backend) = $fixture;
                let _ = $label;
                $crate::registration_macro_support::$law(backend).await;
                $crate::law_receipt::record(module_path!(), stringify!($law), $label);
            }
        )*
    };
}

/// Register exact journal-retirement laws for SQL effect hosts.
#[macro_export]
macro_rules! effect_host_retirement_tests {
    ($fixture:block) => {
        $crate::effect_host_retirement_tests!(@catalogue $fixture; [
            (effect_host_retires_session_journal, "effect-host-retire-session"),
            (effect_host_retires_process_journal, "effect-host-retire-process"),
            (effect_host_retires_runtime_operation_journal, "effect-host-retire-runtime-operation"),
        ]);
    };
    (@catalogue $fixture:block; [$(( $law:ident, $label:literal )),* $(,)?]) => {
        $(
            #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
            async fn $law() {
                let (_fixture_guard, host) = $fixture;
                let _ = $label;
                $crate::registration_macro_support::$law(host.as_ref()).await;
                $crate::law_receipt::record(module_path!(), stringify!($law), $label);
            }
        )*
    };
}

/// Expansion machinery for process-prune reclamation registration.
#[macro_export]
macro_rules! __process_prune_reclaim_register {
    ($fixture:block; $law:ident, $label:literal, registry) => {
        #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
        async fn $law() {
            let (_fixture_guard, _backend, factory, registry, _probe) = $fixture;
            let _ = $label;
            $crate::registration_macro_support::$law(factory, registry).await;
            $crate::law_receipt::record(module_path!(), stringify!($law), $label);
        }
    };
    ($fixture:block; $law:ident, $label:literal, blob) => {
        #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
        async fn $law() {
            let (_fixture_guard, backend, factory, registry, probe) = $fixture;
            let _ = $label;
            $crate::registration_macro_support::$law(backend, factory, registry, probe).await;
            $crate::law_receipt::record(module_path!(), stringify!($law), $label);
        }
    };
}

/// Register one independently reported test per process-prune reclamation law.
#[macro_export]
macro_rules! process_prune_reclaim_tests {
    ($fixture:block) => {
        $crate::process_prune_reclaim_tests!(@catalogue $fixture; [
            (process_prune_reclaims_tombstones_owned_by_deleted_sessions, "process-prune-tombstone-reclaim", registry),
            (process_prune_records_deletions_for_later_reclaim, "process-prune-records-deletions", registry),
            (process_prune_reclaims_checkpoint_blobs_and_propagates_failure, "process-prune-checkpoint-blob-reclaim", blob),
            (process_prune_reclaims_content_aliased_checkpoint_roots, "process-prune-content-alias", blob),
        ]);
    };
    (@catalogue $fixture:block; [$(( $law:ident, $label:literal, $mode:ident )),* $(,)?]) => {
        $(
            $crate::__process_prune_reclaim_register!($fixture; $law, $label, $mode);
        )*
    };
}

#[macro_export]
macro_rules! attachment_store_tests {
    ($fixture:block) => {
        $crate::attachment_store_tests!(@catalogue $fixture; [
            (attachment_store, "attachment-store"),
        ]);
    };
    (@catalogue $fixture:block; [$(( $law:ident, $label:literal )),* $(,)?]) => {
        $(
            #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
            async fn $law() {
                let (_fixture_guard, make, persistence) = $fixture;
                let _ = $label;
                $crate::registration_macro_support::$law(make, persistence).await;
                $crate::law_receipt::record(module_path!(), stringify!($law), $label);
            }
        )*
    };
}

/// Register the durable reopenable attachment-store laws.
#[macro_export]
macro_rules! attachment_store_reopenable_tests {
    ($fixture:block) => {
        $crate::attachment_store_reopenable_tests!(@catalogue $fixture; [
            (attachment_store_reopenable, "attachment-store-reopenable"),
        ]);
    };
    (@catalogue $fixture:block; [$(( $law:ident, $label:literal )),* $(,)?]) => {
        $(
            #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
            async fn $law() {
                let (_fixture_guard, make, persistence) = $fixture;
                let _ = $label;
                $crate::registration_macro_support::$law(make, persistence).await;
                $crate::law_receipt::record(module_path!(), stringify!($law), $label);
            }
        )*
    };
}

/// Register the durable fused artifact-store laws.
#[macro_export]
macro_rules! artifact_store_reopenable_tests {
    ($fixture:block) => {
        $crate::artifact_store_reopenable_tests!(@catalogue $fixture; [
            (lashlang_artifact_store_fresh_instances, "lashlang-artifact-fresh-instances"),
            (lashlang_artifact_store_reports_durable, "lashlang-artifact-durability"),
            (lashlang_artifact_owner_lifecycle, "lashlang-artifact-owner-lifecycle"),
            (lashlang_failed_registration_reclaims_staging_owner, "lashlang-artifact-failed-registration"),
            (lashlang_artifact_transfer_is_idempotent, "lashlang-artifact-transfer"),
            (lashlang_artifact_retirement_fences_late_publication, "lashlang-artifact-retirement-fence"),
            (lashlang_slow_writer_is_fenced_after_retirement, "lashlang-artifact-slow-writer"),
            (lashlang_hostile_module_references_are_rejected, "lashlang-artifact-hostile-reference"),
            (lashlang_alpha_variants_publish_distinct_refs, "lashlang-artifact-alpha-variants"),
            (lashlang_artifact_survives_reopen, "lashlang-artifact-reopen"),
            (process_execution_env_store_fresh_instances, "process-env-fresh-instances"),
            (process_environment_namespace, "process-env-hostile-reference"),
            (process_env_owner_lifecycle, "process-env-owner-lifecycle"),
            (failed_registration_reclaims_process_env, "process-env-failed-registration"),
            (process_env_transfer_and_fence, "process-env-transfer"),
            (slow_process_env_writer_is_fenced, "process-env-slow-writer"),
            (process_env_survives_reopen, "process-env-reopen"),
            (artifact_store_cross_namespace_isolation, "artifact-store-cross-namespace"),
        ]);
    };
    (@catalogue $fixture:block; [$(( $law:ident, $label:literal )),* $(,)?]) => {
        $(
            #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
            async fn $law() {
                let (_fixture_guard, make) = $fixture;
                let _ = $label;
                $crate::fused_artifact_store::$law(make).await;
                $crate::law_receipt::record(module_path!(), stringify!($law), $label);
            }
        )*
    };
}

/// Register the fence-integrity corruption law.
#[macro_export]
macro_rules! fence_integrity_tests {
    ($fixture:block) => {
        $crate::fence_integrity_tests!(@catalogue $fixture; [
            (fence_integrity_conformance, "fence-integrity"),
        ]);
    };
    (@catalogue $fixture:block; [$(( $law:ident, $label:literal )),* $(,)?]) => {
        $(
            #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
            async fn $law() {
                let (_fixture_guard, make) = $fixture;
                let _ = $label;
                $crate::registration_macro_support::$law(make).await;
                $crate::law_receipt::record(module_path!(), stringify!($law), $label);
            }
        )*
    };
}

/// Register the graph-integrity corruption law.
#[macro_export]
macro_rules! graph_integrity_tests {
    ($fixture:block) => {
        $crate::graph_integrity_tests!(@catalogue $fixture; [
            (graph_integrity_conformance, "graph-integrity"),
        ]);
    };
    (@catalogue $fixture:block; [$(( $law:ident, $label:literal )),* $(,)?]) => {
        $(
            #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
            async fn $law() {
                let (_fixture_guard, make) = $fixture;
                let _ = $label;
                $crate::registration_macro_support::$law(make).await;
                $crate::law_receipt::record(module_path!(), stringify!($law), $label);
            }
        )*
    };
}

#[macro_export]
macro_rules! signed_counter_write_domain_tests {
    ($fixture:block) => {
        $crate::signed_counter_write_domain_tests!(@catalogue $fixture; [
            (signed_counter_write_domain_conformance, "signed-counter-write-domain"),
        ]);
    };
    (@catalogue $fixture:block; [$(( $law:ident, $label:literal )),* $(,)?]) => {
        $(
            #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
            async fn $law() {
                let (_fixture_guard, store) = $fixture;
                let _ = $label;
                $crate::registration_macro_support::$law(store).await;
                $crate::law_receipt::record(module_path!(), stringify!($law), $label);
            }
        )*
    };
}

#[macro_export]
macro_rules! session_store_factory_tests {
    ($fixture:block) => {
        $crate::session_store_factory_tests!(@catalogue $fixture; [
            (session_store_factory, "session-store-factory"),
        ]);
        $crate::session_store_factory_tests!(@turn_cancel $fixture; [
            (session_store_factory_discovers_empty_pending_queued_run, "queued-run-empty-discovery"),
            (session_store_factory_retains_assigned_input_tombstone, "queued-run-assigned-retention"),
            (turn_cancel_exact_replay_preserves_different_pending_authorization, "turn-cancel-exact-replay"),
            (turn_cancel_closure_settlement_is_fenced_and_non_overwritable, "turn-cancel-closure-settlement"),
            (turn_cancel_scope_retirement_serializes_with_authorization, "turn-cancel-scope-retirement"),
            (turn_cancel_request_escalation_advances_intent_without_replacing_base, "turn-cancel-escalation"),
            (turn_cancel_repair_preserves_base_across_escalation_and_reopen, "turn-cancel-repair-reopen"),
            (turn_cancel_repair_orders_intent_and_ordinary_redefer, "turn-cancel-repair-redefer"),
            (turn_cancel_final_commit_intent_cas_is_atomic, "turn-cancel-final-commit-cas"),
            (turn_cancel_conflicting_repeat_leaves_no_durable_trace, "turn-cancel-conflicting-repeat"),
            (turn_cancel_concurrent_opposing_requests_converge, "turn-cancel-concurrent-opposing"),
            (turn_cancel_wrong_binding_is_refused_at_every_phase, "turn-cancel-wrong-binding"),
        ]);
    };
    (@catalogue $fixture:block; [$(( $law:ident, $label:literal )),* $(,)?]) => {
        $(
            #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
            async fn $law() {
                let (_fixture_guard, backend, unbound, make) = $fixture;
                let _ = $label;
                $crate::registration_macro_support::$law(backend, unbound, make).await;
                $crate::law_receipt::record(module_path!(), stringify!($law), $label);
            }
        )*
    };
    (@turn_cancel $fixture:block; [$(( $law:ident, $label:literal )),* $(,)?]) => {
        $(
            #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
            async fn $law() {
                let (_fixture_guard, _backend, _unbound, make) = $fixture;
                let _unbound: Option<
                    ::std::sync::Arc<dyn lash_core::store::StoreMaintenance>,
                > = _unbound;
                let _ = $label;
                $crate::registration_macro_support::$law(make()).await;
                $crate::law_receipt::record(module_path!(), stringify!($law), $label);
            }
        )*
    };
}

/// Register one session-config settlement law.
#[macro_export]
macro_rules! __session_config_settlement_register {
    ([$($attr:tt)*] $fixture:block; $law:ident, $label:literal) => {
        $($attr)*
        #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
        async fn $law() {
            let (_fixture_guard, make) = $fixture;
            let _ = $label;
            Box::pin($crate::registration_macro_support::$law(make)).await;
            $crate::law_receipt::record(module_path!(), stringify!($law), $label);
        }
    };
}

/// Register the session-config settlement laws. The fixture hands back a guard
/// and a maker of fresh backends; each law builds its runtime over one.
#[macro_export]
macro_rules! session_config_settlement_tests {
    ($(#[$attr:meta])* $fixture:block) => {
        $crate::session_config_settlement_tests!(@catalogue [$(#[$attr])*] $fixture; [
            (session_config_settlement_timeout_is_typed, "config-settlement-timeout"),
            (cancelled_session_config_settlement_is_typed, "config-settlement-cancelled"),
            (superseded_config_settlement_adopts_the_newer_head, "config-settlement-superseded"),
        ]);
    };
    (@catalogue $attrs:tt $fixture:block; [$(( $law:ident, $label:literal )),* $(,)?]) => {
        $(
            $crate::__session_config_settlement_register!($attrs $fixture; $law, $label);
        )*
    };
}

#[macro_export]
macro_rules! fresh_session_admission_tests {
    ($fixture:block) => {
        $crate::fresh_session_admission_tests!(@catalogue $fixture; [
            (fresh_session_admission_returns_created, "fresh-session-admission"),
        ]);
    };
    (@catalogue $fixture:block; [$(( $law:ident, $label:literal )),* $(,)?]) => {
        $(
            #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
            async fn $law() {
                let (_fixture_guard, make) = $fixture;
                let _ = $label;
                $crate::registration_macro_support::$law(make).await;
                $crate::law_receipt::record(module_path!(), stringify!($law), $label);
            }
        )*
    };
}

/// Expansion machinery for session read-view registration.
#[macro_export]
macro_rules! __session_read_view_register {
    ($fixture:block; $law:ident, $label:literal, failure) => {
        #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
        async fn $law() {
            let (_fixture_guard, factory, advance) = $fixture;
            let _ = $label;
            $crate::registration_macro_support::$law(factory, advance).await;
            $crate::law_receipt::record(module_path!(), stringify!($law), $label);
        }
    };
    ($fixture:block; $law:ident, $label:literal, read) => {
        #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
        async fn $law() {
            let (_fixture_guard, factory, _advance) = $fixture;
            let _ = $label;
            $crate::registration_macro_support::$law(factory).await;
            $crate::law_receipt::record(module_path!(), stringify!($law), $label);
        }
    };
}

/// Register one independently reported test per session read-view law.
#[macro_export]
macro_rules! session_read_view_tests {
    ($fixture:block) => {
        $crate::session_read_view_tests!(@catalogue $fixture; [
            (session_store_factory_mid_stream_failure_evidence, "session-read-mid-stream-failure", failure),
            (session_store_factory_read_session, "session-read-view", read),
        ]);
    };
    (@catalogue $fixture:block; [$(( $law:ident, $label:literal, $mode:ident )),* $(,)?]) => {
        $(
            $crate::__session_read_view_register!($fixture; $law, $label, $mode);
        )*
    };
}

#[macro_export]
macro_rules! process_prune_session_store_tests {
    ($fixture:block) => {
        $crate::process_prune_session_store_tests!(@catalogue $fixture; [
            (process_prune_deletes_owned_session_stores, "process-prune-session-store-cleanup"),
        ]);
    };
    (@catalogue $fixture:block; [$(( $law:ident, $label:literal )),* $(,)?]) => {
        $(
            #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
            async fn $law() {
                let (_fixture_guard, factory, registry) = $fixture;
                let _ = $label;
                $crate::registration_macro_support::$law(factory, registry).await;
                $crate::law_receipt::record(module_path!(), stringify!($law), $label);
            }
        )*
    };
}

/// Expansion machinery for live-replay registration.
#[macro_export]
macro_rules! __live_replay_register {
    ($fixture:block; $law:ident, $label:literal, plain) => {
        #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
        async fn $law() {
            let (_guard, make, _capacity, _ttl, _wait, _incarnations) = $fixture;
            let _ = $label;
            $crate::registration_macro_support::$law(make).await;
            $crate::law_receipt::record(module_path!(), stringify!($law), $label);
        }
    };
    ($fixture:block; $law:ident, $label:literal, capacity) => {
        #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
        async fn $law() {
            let (_guard, _make, capacity, _ttl, _wait, _incarnations) = $fixture;
            let _ = $label;
            $crate::registration_macro_support::$law(capacity).await;
            $crate::law_receipt::record(module_path!(), stringify!($law), $label);
        }
    };
    ($fixture:block; $law:ident, $label:literal, ttl) => {
        #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
        async fn $law() {
            let (_guard, _make, _capacity, ttl, wait, _incarnations) = $fixture;
            let _ = $label;
            $crate::registration_macro_support::$law(ttl, wait).await;
            $crate::law_receipt::record(module_path!(), stringify!($law), $label);
        }
    };
    ($fixture:block; $law:ident, $label:literal, incarnation) => {
        #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
        async fn $law() {
            let (_guard, _make, _capacity, _ttl, _wait, (original, fresh, preserved)) = $fixture;
            let _ = $label;
            $crate::registration_macro_support::$law(original, fresh, preserved).await;
            $crate::law_receipt::record(module_path!(), stringify!($law), $label);
        }
    };
}

/// Register one independently reported test per live-replay law.
#[macro_export]
macro_rules! live_replay_tests {
    ($fixture:block) => {
        $crate::live_replay_tests!(@catalogue $fixture; [
            (live_replay_store, "live-replay", plain),
            (live_replay_store_capacity_trim, "live-replay-capacity", capacity),
            (live_replay_store_ttl_trim, "live-replay-ttl", ttl),
            (incarnation_change_invalidates_cursor, "live-replay-incarnation", incarnation),
        ]);
    };
    (@catalogue $fixture:block; [$(( $law:ident, $label:literal, $mode:ident )),* $(,)?]) => {
        $(
            $crate::__live_replay_register!($fixture; $law, $label, $mode);
        )*
    };
}

/// Register checkpoint-component cold-reopen conformance.
#[macro_export]
macro_rules! checkpoint_component_reopen_tests {
    ($fixture:block) => {
        $crate::checkpoint_component_reopen_tests!(@catalogue $fixture; [
            (complete_runtime_checkpoint_component_set_survives_cold_reopens, "checkpoint-components-cold-reopen"),
        ]);
    };
    (@catalogue $fixture:block; [$(( $law:ident, $label:literal )),* $(,)?]) => {
        $(
            #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
            async fn $law() {
                let (_guard, make) = $fixture;
                let _ = $label;
                $crate::registration_macro_support::$law(make).await;
                $crate::law_receipt::record(module_path!(), stringify!($law), $label);
            }
        )*
    };
}

#[macro_export]
macro_rules! session_graph_append_tests {
    ($fixture:block) => {
        $crate::session_graph_append_tests!(@catalogue $fixture; [
            (session_graph_append_branch_liveness, "session-graph-append-branch-liveness"),
        ]);
    };
    (@catalogue $fixture:block; [$(( $law:ident, $label:literal )),* $(,)?]) => {
        $(
            #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
            async fn $law() {
                let (_guard, factory) = $fixture;
                let _ = $label;
                $crate::registration_macro_support::$law(factory).await;
                $crate::law_receipt::record(module_path!(), stringify!($law), $label);
            }
        )*
    };
}

#[macro_export]
macro_rules! runtime_persistence_clock_tests {
    ($fixture:block) => {
        $crate::runtime_persistence_clock_tests!(@catalogue $fixture; [
            (runtime_persistence_clock_expiry, "runtime-persistence-clock-expiry"),
        ]);
    };
    (@catalogue $fixture:block; [$(( $law:ident, $label:literal )),* $(,)?]) => {
        $(
            #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
            async fn $law() {
                let (_guard, store, advance, verify) = $fixture;
                let _ = $label;
                $crate::registration_macro_support::$law(::std::sync::Arc::clone(&store), advance).await;
                verify(store).await;
                $crate::law_receipt::record(module_path!(), stringify!($law), $label);
            }
        )*
    };
}

#[macro_export]
macro_rules! turn_work_driver_tests {
    ($fixture:block) => {
        $crate::turn_work_driver_tests!(@catalogue $fixture; [
            (turn_work_driver, "turn-work-driver"),
        ]);
    };
    (@catalogue $fixture:block; [$(( $law:ident, $label:literal )),* $(,)?]) => {
        $(
            #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
            async fn $law() {
                let (_guard, host, registration_barrier) = $fixture;
                let _ = $label;
                $crate::registration_macro_support::$law(host, registration_barrier).await;
                $crate::law_receipt::record(module_path!(), stringify!($law), $label);
            }
        )*
    };
}

/// Register wake-delivery crash conformance.
#[macro_export]
macro_rules! wake_delivery_crash_tests {
    ($fixture:block) => {
        $crate::wake_delivery_crash_tests!(@catalogue $fixture; [
            (wake_delivery_crash_matrix, "wake-delivery-crash-matrix"),
        ]);
    };
    (@catalogue $fixture:block; [$(( $law:ident, $label:literal )),* $(,)?]) => {
        $(
            #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
            async fn $law() {
                let (_guard, factory, registry, clock, work, witness, before_terminal, verify) = $fixture;
                let _ = $label;
                $crate::registration_macro_support::$law(
                    factory,
                    registry,
                    clock,
                    work,
                    witness,
                    before_terminal,
                )
                .await;
                verify().await;
                $crate::law_receipt::record(module_path!(), stringify!($law), $label);
            }
        )*
    };
}

/// Register wake-delivery ordering-group conformance.
#[macro_export]
macro_rules! wake_delivery_ordering_tests {
    ($fixture:block) => {
        $crate::wake_delivery_ordering_tests!(@catalogue $fixture; [
            (wake_delivery_ordering_group_conformance, "wake-delivery-ordering-group"),
        ]);
    };
    (@catalogue $fixture:block; [$(( $law:ident, $label:literal )),* $(,)?]) => {
        $(
            #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
            async fn $law() {
                let (_guard, registry, injector, work, witness, before_terminal, verify) = $fixture;
                let _ = $label;
                $crate::registration_macro_support::$law(
                    registry,
                    injector,
                    work,
                    witness,
                    before_terminal,
                )
                .await;
                verify().await;
                $crate::law_receipt::record(module_path!(), stringify!($law), $label);
            }
        )*
    };
}

/// Register cold abandoned-attachment recovery.
#[macro_export]
macro_rules! abandoned_attachment_recovery_tests {
    ($fixture:block) => {
        $crate::abandoned_attachment_recovery_tests!(@catalogue $fixture; [
            (abandoned_attachment_write_recovery_after_cold_reopen, "abandoned-attachment-cold-reopen"),
        ]);
    };
    (@catalogue $fixture:block; [$(( $law:ident, $label:literal )),* $(,)?]) => {
        $(
            #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
            async fn $law() {
                let (_guard, make) = $fixture;
                let _ = $label;
                let (factory, reopen) = make().await;
                $crate::registration_macro_support::$law(factory, reopen).await;
                $crate::law_receipt::record(module_path!(), stringify!($law), $label);
            }
        )*
    };
}

/// Register cold attachment-owner replay.
#[macro_export]
macro_rules! attachment_owner_cold_replay_tests {
    ($fixture:block) => {
        $crate::attachment_owner_cold_replay_tests!(@catalogue $fixture; [
            (attachment_owner_cold_replay, "attachment-owner-cold-replay"),
        ]);
    };
    (@catalogue $fixture:block; [$(( $law:ident, $label:literal )),* $(,)?]) => {
        $(
            #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
            async fn $law() {
                let (_guard, backend) = $fixture;
                let _ = $label;
                $crate::registration_macro_support::$law(backend).await;
                $crate::law_receipt::record(module_path!(), stringify!($law), $label);
            }
        )*
    };
}

#[macro_export]
macro_rules! attachment_owner_degraded_tests {
    ($fixture:block) => {
        $crate::attachment_owner_degraded_tests!(@catalogue $fixture; [
            (attachment_owner_degraded_proof, "attachment-owner-degraded-proof"),
        ]);
    };
    (@catalogue $fixture:block; [$(( $law:ident, $label:literal )),* $(,)?]) => {
        $(
            #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
            async fn $law() {
                let (_guard, factory) = $fixture;
                let _ = $label;
                $crate::registration_macro_support::$law(factory).await;
                $crate::law_receipt::record(module_path!(), stringify!($law), $label);
            }
        )*
    };
}

#[macro_export]
macro_rules! store_effect_group_drain_tests {
    ($fixture:block) => {
        $crate::store_effect_group_drain_tests!(@catalogue $fixture; [
            (store_effect_group_drain_conformance, "store-effect-group-drain"),
            (
                store_effect_group_crash_matrix_conformance,
                "store-effect-group-crash-matrix"
            ),
        ]);
    };
    (@catalogue $fixture:block; [$(( $law:ident, $label:literal )),* $(,)?]) => {
        $(
            #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
            async fn $law() {
                let (_guard, make) = $fixture;
                let _ = $label;
                $crate::registration_macro_support::$law(make).await;
                $crate::law_receipt::record(module_path!(), stringify!($law), $label);
            }
        )*
    };
}

/// Register the durable-closing laws (ADR 0099 §7, FIG-3410) against the same
/// drain-world factory the drain suite uses: each law builds its own hosts
/// over the same journal and reaches the closing seam through
/// `EffectHost::effect_group_closing`.
#[macro_export]
macro_rules! store_effect_group_closing_tests {
    ($fixture:block) => {
        $crate::store_effect_group_closing_tests!(@catalogue $fixture; [
            (
                closing_is_recorded_before_any_cancel_is_issued,
                "group-closing-before-cancel"
            ),
            (
                a_crash_after_drain_resumes_at_outcome_commit,
                "group-crash-outcome"
            ),
            (
                a_crash_after_accounting_resumes_at_parent_end,
                "group-crash-parent-end"
            ),
            (
                a_drain_budget_expiry_leaves_closing_recorded,
                "group-closing-pending"
            ),
        ]);
    };
    (@catalogue $fixture:block; [$(( $law:ident, $label:literal )),* $(,)?]) => {
        $(
            #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
            async fn $law() {
                let (_guard, make) = $fixture;
                let _ = $label;
                $crate::registration_macro_support::$law(make).await;
                $crate::law_receipt::record(module_path!(), stringify!($law), $label);
            }
        )*
    };
}

/// Register atomic runtime-operation effect-group retirement.
#[macro_export]
macro_rules! effect_group_runtime_retirement_tests {
    ($fixture:block) => {
        $crate::effect_group_runtime_retirement_tests!(@catalogue $fixture; [
            (effect_group_runtime_operation_retirement_is_atomic, "effect-group-runtime-retirement"),
        ]);
    };
    (@catalogue $fixture:block; [$(( $law:ident, $label:literal )),* $(,)?]) => {
        $(
            #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
            async fn $law() {
                let (_guard, make, verify) = $fixture;
                let _ = $label;
                let observation = $crate::registration_macro_support::$law(make).await;
                verify(observation).await;
                $crate::law_receipt::record(module_path!(), stringify!($law), $label);
            }
        )*
    };
}

#[macro_export]
macro_rules! effect_group_quiescent_retirement_tests {
    ($fixture:block) => {
        $crate::effect_group_quiescent_retirement_tests!(@catalogue $fixture; [
            (effect_group_quiescent_retirement_waits_for_live_children, "effect-group-quiescent-retirement"),
        ]);
    };
    (@catalogue $fixture:block; [$(( $law:ident, $label:literal )),* $(,)?]) => {
        $(
            #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
            async fn $law() {
                let (_guard, make, verify) = $fixture;
                let _ = $label;
                let observation = $crate::registration_macro_support::$law(make).await;
                verify(observation).await;
                $crate::law_receipt::record(module_path!(), stringify!($law), $label);
            }
        )*
    };
}

/// The fixture supplies a store plus the backend's own "make this node the session head"
/// mutation, applied outside the runtime.
#[macro_export]
macro_rules! append_head_switch_tests {
    ($fixture:block) => {
        $crate::append_head_switch_tests!(@catalogue $fixture; [
            (append_request_receipt_replays_after_ancestor_superseded, "append-receipt-ancestor-superseded"),
            (inactive_append_ancestor_precedes_stale_head, "append-ancestor-precedes-stale-head"),
        ]);
    };
    (@catalogue $fixture:block; [$(( $law:ident, $label:literal )),* $(,)?]) => {
        $(
            #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
            async fn $law() {
                let (_guard, store, switch_head) = $fixture;
                let _ = $label;
                $crate::registration_macro_support::$law(store, switch_head).await;
                $crate::law_receipt::record(module_path!(), stringify!($law), $label);
            }
        )*
    };
}

/// Register the tombstoned-leaf append refusal. The fixture supplies a store
/// plus the backend's own out-of-band node tombstone.
#[macro_export]
macro_rules! append_tombstone_tests {
    ($fixture:block) => {
        $crate::append_tombstone_tests!(@catalogue $fixture; [
            (tombstoned_old_leaf_is_rejected, "append-tombstoned-old-leaf"),
        ]);
    };
    (@catalogue $fixture:block; [$(( $law:ident, $label:literal )),* $(,)?]) => {
        $(
            #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
            async fn $law() {
                let (_guard, store, tombstone) = $fixture;
                let _ = $label;
                $crate::registration_macro_support::$law(store, tombstone).await;
                $crate::law_receipt::record(module_path!(), stringify!($law), $label);
            }
        )*
    };
}

#[macro_export]
macro_rules! append_receipt_envelope_tests {
    ($fixture:block) => {
        $crate::append_receipt_envelope_tests!(@catalogue $fixture; [
            (append_receipt_mixed_usage_envelope, "append-receipt-mixed-usage"),
        ]);
    };
    (@catalogue $fixture:block; [$(( $law:ident, $label:literal )),* $(,)?]) => {
        $(
            #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
            async fn $law() {
                let (_guard, store) = $fixture;
                let _ = $label;
                $crate::registration_macro_support::$law(store).await;
                $crate::law_receipt::record(module_path!(), stringify!($law), $label);
            }
        )*
    };
}

/// The fixture supplies a store plus the backend's own persisted-receipt rewrite, applied
/// outside the runtime.
#[macro_export]
macro_rules! append_receipt_rewrite_tests {
    ($fixture:block) => {
        $crate::append_receipt_rewrite_tests!(@catalogue $fixture; [
            (old_format_append_receipt_returns_public_leaf, "append-receipt-old-format"),
        ]);
    };
    (@catalogue $fixture:block; [$(( $law:ident, $label:literal )),* $(,)?]) => {
        $(
            #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
            async fn $law() {
                let (_guard, store, rewrite) = $fixture;
                let _ = $label;
                $crate::registration_macro_support::$law(store, rewrite).await;
                $crate::law_receipt::record(module_path!(), stringify!($law), $label);
            }
        )*
    };
}

/// Register append-receipt identity-encoding corruption refusal. The fixture
/// supplies a store plus the backend's own corrupting write.
#[macro_export]
macro_rules! append_receipt_identity_corruption_tests {
    ($fixture:block) => {
        $crate::append_receipt_identity_corruption_tests!(@catalogue $fixture; [
            (append_receipt_corrupt_identity_encoding_version_is_refused, "append-receipt-corrupt-identity-version"),
        ]);
    };
    (@catalogue $fixture:block; [$(( $law:ident, $label:literal )),* $(,)?]) => {
        $(
            #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
            async fn $law() {
                let (_guard, store, corrupt) = $fixture;
                let _ = $label;
                $crate::registration_macro_support::$law(store, corrupt).await;
                $crate::law_receipt::record(module_path!(), stringify!($law), $label);
            }
        )*
    };
}

/// Register cancelled-append usage publication. The fixture supplies a store
/// plus the backend's own commit-seam pause.
#[macro_export]
macro_rules! append_usage_cancellation_tests {
    ($fixture:block) => {
        $crate::append_usage_cancellation_tests!(@catalogue $fixture; [
            (append_usage_cancellation_publishes_exactly_once, "append-usage-cancellation"),
        ]);
    };
    (@catalogue $fixture:block; [$(( $law:ident, $label:literal )),* $(,)?]) => {
        $(
            #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
            async fn $law() {
                let (_guard, store, arm_and_wait) = $fixture;
                let _ = $label;
                $crate::registration_macro_support::$law(store, arm_and_wait).await;
                $crate::law_receipt::record(module_path!(), stringify!($law), $label);
            }
        )*
    };
}

/// The fixture supplies the backend's per-admission-axis handle factory.
#[macro_export]
macro_rules! unbound_session_read_tests {
    ($fixture:block) => {
        $crate::unbound_session_read_tests!(@catalogue $fixture; [
            (unbound_session_reads_resolve_the_same_session, "unbound-session-reads"),
        ]);
    };
    (@catalogue $fixture:block; [$(( $law:ident, $label:literal )),* $(,)?]) => {
        $(
            #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
            async fn $law() {
                let (_guard, make_axis) = $fixture;
                let _ = $label;
                $crate::registration_macro_support::$law(make_axis).await;
                $crate::law_receipt::record(module_path!(), stringify!($law), $label);
            }
        )*
    };
}

/// Register unbound-session metadata ambiguity refusal.
#[macro_export]
macro_rules! unbound_session_meta_tests {
    ($fixture:block) => {
        $crate::unbound_session_meta_tests!(@catalogue $fixture; [
            (unbound_session_meta_refuses_ambiguous_resolution, "unbound-session-meta"),
        ]);
    };
    (@catalogue $fixture:block; [$(( $law:ident, $label:literal )),* $(,)?]) => {
        $(
            #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
            async fn $law() {
                let (_guard, backend_name, load) = $fixture;
                let _ = $label;
                $crate::registration_macro_support::$law(backend_name, load).await;
                $crate::law_receipt::record(module_path!(), stringify!($law), $label);
            }
        )*
    };
}

/// Register checkpoint-claim transaction accounting. The fixture supplies a
/// store, the session it probes and the backend's own transaction counter.
#[macro_export]
macro_rules! checkpoint_claim_probe_tests {
    ($fixture:block) => {
        $crate::checkpoint_claim_probe_tests!(@catalogue $fixture; [
            (checkpoint_claim_probe_transaction_counts, "checkpoint-claim-probe-counts"),
        ]);
    };
    (@catalogue $fixture:block; [$(( $law:ident, $label:literal )),* $(,)?]) => {
        $(
            #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
            async fn $law() {
                let (_guard, store, session_id, counts, teardown) = $fixture;
                let _ = $label;
                $crate::registration_macro_support::$law(store, &session_id, counts).await;
                teardown.await;
                $crate::law_receipt::record(module_path!(), stringify!($law), $label);
            }
        )*
    };
}

/// The fixture supplies the engine-paced and deployment-host resolver factories.
#[macro_export]
macro_rules! durable_queued_drain_wait_resolver_tests {
    ($fixture:block) => {
        $crate::durable_queued_drain_wait_resolver_tests!(@catalogue $fixture; [
            (durable_queued_drain_wait_resolver_laws, "durable-queued-drain-resolver"),
        ]);
    };
    (@catalogue $fixture:block; [$(( $law:ident, $label:literal )),* $(,)?]) => {
        $(
            #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
            async fn $law() {
                let (_fixture_guard, make_engine, make_deployment) = $fixture;
                let _ = $label;
                $crate::registration_macro_support::$law(make_engine, make_deployment).await;
                $crate::law_receipt::record(module_path!(), stringify!($law), $label);
            }
        )*
    };
}

/// Expansion machinery for await-event witness registration.
#[macro_export]
macro_rules! __effect_host_await_event_witness_register {
    ([$($attr:tt)*] $fixture:block; $law:ident, $label:literal) => {
        $($attr)*
        #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
        async fn $law() {
            let (_guard, deadline, make, witness, finish) = $fixture;
            ::tokio::time::timeout(
                deadline,
                $crate::registration_macro_support::$law(make, witness),
            )
            .await
            .unwrap_or_else(|_| panic!("{} exceeded {deadline:?}", $label));
            finish.await;
            $crate::law_receipt::record(module_path!(), stringify!($law), $label);
        }
    };
}

/// The fixture supplies the backend's host maker plus its own post-condition witness.
#[macro_export]
macro_rules! effect_host_await_event_witness_tests {
    ($fixture:block) => {
        $crate::effect_host_await_event_witness_tests!(@catalogue [] $fixture);
    };
    ($(#[$attr:meta])+ $fixture:block) => {
        $crate::effect_host_await_event_witness_tests!(@catalogue [$(#[$attr])*] $fixture);
    };
    (@catalogue $attrs:tt $fixture:block) => {
        $crate::effect_host_await_event_witness_tests!(@expand $attrs $fixture; [
            (effect_host_await_events_with_active_wait_witness, "await-event-warm-witness"),
            (effect_host_await_events_cold_instance_with_active_wait_witness, "await-event-cold-witness"),
        ]);
    };
    (@expand $attrs:tt $fixture:block; [$(( $law:ident, $label:literal )),* $(,)?]) => {
        $(
            $crate::__effect_host_await_event_witness_register!(
                $attrs $fixture; $law, $label
            );
        )*
    };
}

/// Register queued-work laws that must run against the backend's own clock.
/// The fixture supplies a store plus the lease timing that clock implies.
#[macro_export]
macro_rules! backend_clock_queued_work_tests {
    ($fixture:block) => {
        $crate::backend_clock_queued_work_tests!(@catalogue $fixture; [
            (queued_work_redrive_selects_claim_identity_across_ready_gap, "backend-clock-redrive-ready-gap"),
        ]);
    };
    (@catalogue $fixture:block; [$(( $law:ident, $label:literal )),* $(,)?]) => {
        $(
            #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
            async fn $law() {
                let (_guard, store, timing) = $fixture;
                let _ = $label;
                $crate::registration_macro_support::$law(store, &timing).await;
                $crate::law_receipt::record(module_path!(), stringify!($law), $label);
            }
        )*
    };
}
