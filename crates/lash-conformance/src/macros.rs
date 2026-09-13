//! Named backend test registration. Every generated test owns a fresh fixture.

/// Expansion machinery for the runtime-persistence registration macros.
#[doc(hidden)]
#[macro_export]
macro_rules! __runtime_persistence_register {
    (plain $fixture:block;
        stores [$(( $store_law:ident, $store_label:literal )),* $(,)?]
        store_refs [$(( $store_ref_law:ident, $store_ref_label:literal )),* $(,)?]
        factories [$(( $factory_law:ident, $factory_label:literal )),* $(,)?]
        timed_stores [$(( $timed_law:ident, $timed_label:literal )),* $(,)?]
        timed_factories [$(( $timed_factory_law:ident, $timed_factory_label:literal )),* $(,)?]
        plain_factories [$(( $plain_factory_law:ident, $plain_factory_label:literal )),* $(,)?]
    ) => {
        $(
            #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
            async fn $store_law() {
                let (make, _lease_timing) = $fixture;
                $crate::runtime_persistence_macro_support::$store_law(make($store_label)).await;
            }
        )*
        $(
            #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
            async fn $store_ref_law() {
                let (make, _lease_timing) = $fixture;
                let store = make($store_ref_label);
                $crate::runtime_persistence_macro_support::$store_ref_law(store.as_ref()).await;
            }
        )*
        $(
            #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
            async fn $factory_law() {
                let (make, _lease_timing) = $fixture;
                $crate::runtime_persistence_macro_support::$factory_law(make, $factory_label)
                    .await;
            }
        )*
        $(
            #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
            async fn $timed_law() {
                let (make, lease_timing) = $fixture;
                $crate::runtime_persistence_macro_support::$timed_law(
                    make($timed_label),
                    &lease_timing,
                )
                .await;
            }
        )*
        $(
            #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
            async fn $timed_factory_law() {
                let (make, lease_timing) = $fixture;
                $crate::runtime_persistence_macro_support::$timed_factory_law(
                    &|| make($timed_factory_label),
                    &lease_timing,
                )
                .await;
            }
        )*
        $(
            #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
            async fn $plain_factory_law() {
                let (make, _lease_timing) = $fixture;
                $crate::runtime_persistence_macro_support::$plain_factory_law(
                    make,
                    $plain_factory_label,
                )
                .await;
            }
        )*
    };
    (reopenable $fixture:block;
        stores [$(( $store_law:ident, $store_label:literal )),* $(,)?]
        store_refs [$(( $store_ref_law:ident, $store_ref_label:literal )),* $(,)?]
        factories [$(( $factory_law:ident, $factory_label:literal )),* $(,)?]
        timed_stores [$(( $timed_law:ident, $timed_label:literal )),* $(,)?]
        timed_factories [$(( $timed_factory_law:ident, $timed_factory_label:literal )),* $(,)?]
        plain_factories [$(( $plain_factory_law:ident, $plain_factory_label:literal )),* $(,)?]
    ) => {
        $(
            #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
            async fn $store_law() {
                let (make, _lease_timing) = $fixture;
                $crate::runtime_persistence_macro_support::$store_law(make($store_label).open)
                    .await;
            }
        )*
        $(
            #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
            async fn $store_ref_law() {
                let (make, _lease_timing) = $fixture;
                let store = make($store_ref_label).open;
                $crate::runtime_persistence_macro_support::$store_ref_law(store.as_ref()).await;
            }
        )*
        $(
            #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
            async fn $factory_law() {
                let (make, _lease_timing) = $fixture;
                $crate::runtime_persistence_macro_support::$factory_law(
                    |label| make(label).open,
                    $factory_label,
                )
                .await;
            }
        )*
        $(
            #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
            async fn $timed_law() {
                let (make, lease_timing) = $fixture;
                $crate::runtime_persistence_macro_support::$timed_law(
                    make($timed_label).open,
                    &lease_timing,
                )
                .await;
            }
        )*
        $(
            #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
            async fn $timed_factory_law() {
                let (make, lease_timing) = $fixture;
                $crate::runtime_persistence_macro_support::$timed_factory_law(
                    &|| make($timed_factory_label).open,
                    &lease_timing,
                )
                .await;
            }
        )*
    };
}

/// Register one independently reported test per plain runtime-persistence law.
#[macro_export]
macro_rules! runtime_persistence_tests {
    ($fixture:block) => {
        $crate::runtime_persistence_tests!(@catalogue plain $fixture);
    };
    (@catalogue $mode:ident $fixture:block) => {
        $crate::__runtime_persistence_register! {
            $mode $fixture;
            stores [
            (commit_increments_head_and_round_trips_agent_frames, "root"),
            (concurrent_head_revision_cas_applies_exactly_once, "concurrent-head-cas"),
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
            (commit_rejects_unresolvable_leaf, "root"),
            (commit_rejects_missing_leaf, "root"),
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
            (queue_completion_and_turn_commit_stamp_are_atomic, "root"),
            (pending_turn_inputs_source_keys_order_cancel_and_cross_session, "root"),
            (pending_turn_input_bulk_and_suffix_cancellation, "pending-bulk-cancel"),
            (pending_turn_input_claims_reclaim_complete_and_fence, "root"),
            (turn_input_application_identity_survives_pending_tombstone_vacuum, "turn-input-application"),
            (active_turn_input_claim_reacquires_after_unrecorded_checkpoint, "fig905-active-reacquire"),
            (pending_turn_input_cancel_covers_active_and_deferred_states, "root"),
            (pending_active_turn_inputs_defer_unaccepted_once_on_interrupt, "root"),
            (a_turn_that_cannot_commit_leaves_no_input_pinned_to_it, "root"),
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
            plain_factories [
            (fresh_instances, "fresh-instance-probe"),
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
                let (make, _lease_timing) = $fixture;
                $crate::runtime_persistence_macro_support::$law(make($label)).await;
            }
        )*
    };
}
