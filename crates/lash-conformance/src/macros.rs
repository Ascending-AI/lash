//! Named backend test registration. Every generated test owns a fresh fixture.

mod declared_start;
mod obligation_relay;
mod session_ingress;
mod tool_batch;
mod tool_call_identity;
mod turn_runner;

/// Expansion machinery for the runtime-persistence registration macros.
#[macro_export]
macro_rules! __runtime_persistence_register {
    (reopenable $fixture:block;
        stores [$(( $store_law:ident, $store_label:literal )),* $(,)?]
        hosted_stores [$(( $hosted_law:ident, $hosted_label:literal )),* $(,)?]
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
            }
        )*
        $(
            #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
            async fn $hosted_law() {
                let (_fixture_guard, make, _lease_timing) = $fixture;
                let handles = make($hosted_label);
                $crate::runtime_persistence_macro_support::$hosted_law(
                    handles.open,
                    handles.effect_host,
                )
                .await;
            }
        )*
        $(
            #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
            async fn $store_ref_law() {
                let (_fixture_guard, make, _lease_timing) = $fixture;
                let store = make($store_ref_label).open;
                $crate::runtime_persistence_macro_support::$store_ref_law(store.as_ref()).await;
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
            (receipt_replay_rehydrates_recorded_node_clocks_and_ids, "root"),
            (concurrent_head_revision_cas_applies_exactly_once, "concurrent-head-cas"),
            (serves_each_admitted_session_and_refuses_an_unknown_one, "alpha"),
            (commit_rejects_carried_nondefault_node_budget, "root"),
            (commit_rejects_carried_nondefault_byte_budget, "root"),
            (commit_rejects_follow_on_bytes_over_budget, "root"),
            (commit_rejects_turn_result_bytes_over_budget, "root"),
            (load_hydrates_checkpoint, "hydrated"),
            (checkpoint_restore_rejects_turn_index_without_increment_headroom, "root"),
            (checkpoint_restore_rejects_token_usage_whose_prompt_subtotal_overflows, "root"),
            (execution_state_replace_then_clear_removes_the_live_checkpoint_ref, "execution-state-replace-then-clear"),
            (checkpoint_rejects_unknown_component_ref, "checkpoint-unknown-ref"),
            (session_read_loads_persisted_history, "branchy"),
            (session_plugin_config_round_trips_through_the_committed_head, "session-plugin-config"),
            (session_metadata_round_trips, "root"),
            (head_and_window_reads_agree_for_each_named_session, "read-agreement"),
            (observer_settlement_requires_admission, "root"),
            (observer_settlement_preserves_creation_facts, "root"),
            (attachment_writes_keep_independent_referrers, "root"),
            (attachment_acquisition_preserves_receiving_referrer, "root"),
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
            (commit_rejects_non_derived_append_node_ids, "root"),
            (append_rejects_duplicate_batch_node_ids, "root"),
            (append_rejects_existing_node_id_collision, "root"),
            (head_retirement_gate_distinguishes_leaf_change_from_same_leaf, "root"),
            (committed_leaf_is_derived_from_the_terminal_appended_node, "root"),
            (preserve_head_commit_reports_the_resident_leaf, "root"),
            (empty_append_cannot_move_the_head, "empty-append-head-move"),
            (commit_rejects_leaf_without_frame_open_ancestor, "missing-frame-run"),
            (queued_work_source_keys_are_idempotent_and_list_ordered, "queued-work-source-keys"),
            (concurrent_queued_work_source_key_enqueues_report_one_inserted_and_one_existing, "concurrent-queued-work-source-key"),
            (decorated_queued_work_source_key_replay_reports_absorbed, "decorated-queued-work-source-key"),
            (pending_session_work_ordering_agrees_across_ingress_families, "pending-work-ordering"),
            (host_cancelled_wake_is_not_redelivered, "root"),
            (delete_then_enqueue_never_reuses_ingress_sequences, "root"),
            (pending_turn_inputs_source_keys_order_cancel_and_cross_session, "root"),
            (pending_turn_input_duplicate_input_id, "root"),
            (run_specs_join_the_submission_digest_and_intern_once, "run-specs"),
            (a_turn_input_batch_enqueues_new_ids_contiguously_in_request_order, "turn-input-batches"),
            (a_resent_turn_input_batch_answers_its_existing_ids_and_enqueues_the_rest, "turn-input-batch-retries"),
            (a_conflict_or_a_repeated_id_refuses_the_whole_turn_input_batch, "turn-input-batch-refusals"),
            (a_changed_resubmission_is_a_typed_conflict_for_every_kind, "ingress-content-conflict"),
            (a_recorded_queued_batch_refuses_multiple_payloads, "ingress-one-payload"),
            (a_settled_command_resubmitted_under_its_key_is_not_a_new_command, "ingress-command-resubmission"),
            ]
            hosted_stores [
            ]
            store_refs [
            ]
            factories [
            (plugin_state_boundary, "plugin-state"),
            ]
            timed_stores [
            ]
            timed_factories [
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
            ]
        );
    };
    (@reopen_laws $fixture:block; [$(( $law:ident, $label:literal )),* $(,)?]) => {
        $(
            #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
            async fn $law() {
                let (_fixture_guard, make, _lease_timing) = $fixture;
                $crate::runtime_persistence_macro_support::$law(make($label)).await;
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
                (a_process_event_batch_is_one_commit, "process-event-batch"),
                (a_boundary_commits_its_prelude_in_its_own_transaction, "process-event-batch-boundary"),
                (count_events_through_counts_every_event_at_any_top_bound, "count-events-through-top-bound"),
                (releasing_an_event_prefix_keeps_sequences_ordinals_and_replay_identity, "release-event-prefix"),
                (signal_admission_retains_its_identity_and_selected_wait, "signal-admission"),
                (raw_signal_appends_are_refused, "raw-signal-refusal"),
                (long_cancellation_requester_replay_is_backend_safe, "long-cancellation-replay"),
                (wake_subscription_is_indexed_and_retargetable, "wake-subscription"),
                (lifecycle_status_and_outcome_fold, "lifecycle-fold"),
                (producer_terminal_status_must_match_materialized_outcome, "terminal-status-outcome"),
                (list_filters_match_extracted_and_json_fields, "list-filters"),
                (process_registry_pagination, "pagination"),
                (non_terminal_process_pages_visit_every_row_across_the_page_bound, "bounded-pagination"),
                (process_event_pages_reject_out_of_range_sequences, "process-event-page-sql-cursor-range"),
                (waiting_processes_remain_in_the_non_terminal_scan, "waiting-non-terminal-scan"),
                (list_processes_filters_by_enriched_fields, "enriched-filters"),
                (list_processes_bounds_retired_rows_without_hiding_live_rows, "retired-bounds"),
                (list_processes_filters_by_until_scope_and_pending_cancel, "until-scope-cancel-filters"),
                (process_change_feed_never_misses_concurrent_terminal_writers, "concurrent-terminal-feed"),
                (session_delete_preserves_process_bytes, "session-delete-bytes"),
                (record_fold_and_retention_hold_for_every_registry_writer, "registry-writer-fold-retention"),
                (signals_refuse_undeclared_invalid_and_terminal_sends, "signal-refusals"),
                (work_wait_seam_covers_unknown_pruned_and_backend_owned_processes, "work-wait-matrix"),
                (tombstones_make_pruned_processes_distinguishable, "tombstones"),
                (a_start_key_after_prune_starts_a_new_process, "start-key-after-prune"),
                (watched_process_registry_start_key_after_prune_starts_a_new_process, "watched-start-key-after-prune"),
                (lifecycle_transition_refusals_are_backend_invariant, "transition-refusals"),
                (lifecycle_event_timestamps_follow_the_registry_clock, "lifecycle-clock"),
                (a_resume_event_cannot_return_an_ended_process_to_running, "resume-after-terminal"),
                (external_ref_is_written_compare_and_set_by_segment_ordinal, "external-ref-compare-and-set"),
                (a_start_key_reports_created_then_existing_and_is_trusted, "start-key-disposition"),
                (a_host_start_key_is_global_and_fences_its_originator, "host-start-key-global"),
                (a_start_key_conflict_names_no_retained_process, "start-key-conflict-content-free"),
                (a_host_retry_with_another_wake_target_conflicts, "host-start-key-wake-target"),
                (a_host_start_key_after_prune_starts_new_for_any_originator, "host-start-key-after-prune"),
                (scope_replay_cancel_and_trace_ignore_environment_rebinding, "scope-environment-rebinding"),
                (remote_start_replay_preserves_recorded_id_key_and_disposition, "remote-start-replay"),
                (retired_process_shapes_refuse_before_registration_or_effects, "retired-process-shapes"),
                (keyless_starts_are_always_new, "keyless-starts"),
                (concurrent_starts_under_one_key_register_one_process, "concurrent-start-key"),
                (terminal_completion_atomically_retains_parent_end_plan, "parent-end-plan"),
                (parent_end_plans_are_reclaimed_by_retention, "parent-end-plan-reclaim"),
                (a_completion_authority_commits_and_records_its_evidence, "completion-authority-granted"),
                (terminal_completion_replay_keeps_original_authority_and_writes_nothing, "terminal-completion-authority-replay"),
                (a_session_scope_closes_only_through_its_close_row, "session-scope-close"),
                (a_turn_scope_ends_through_its_recorded_ledger_row, "turn-parent-end"),
                (an_abandoned_consumer_hold_fences_registration, "abandoned-consumer-hold"),
                (consumer_hold_prevents_destructive_prune_until_settlement, "consumer-hold-retention"),
                (later_segment_recovery_refuses_without_terminal_mutation, "later-segment-recovery"),
                (every_execution_write_refuses_a_superseded_invocation_without_mutation, "invocation-write-matrix"),
                (scopes_that_collide_in_rendering_share_no_ledger_key, "colliding-scope-keys"),
                (a_session_close_fences_the_turn_scopes_that_never_became_runs, "never-run-turn-scopes"),
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
            ]
            plain [
                (store_recovery_fresh_instances, "store-recovery-fresh-instance-probe"),
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
            }
        )*
        $(
            #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
            async fn $plain_law() {
                let (_fixture_guard, make, _lease_timing) = $fixture;
                $crate::registration_macro_support::$plain_law(&make, $plain_label).await;
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
            ]
            bytes [
                (empty_root_set_refusal_returns_its_partial_report, "maintenance-refusal"),
            ]
        );
    };
    (@catalogue $fixture:block;
        sync [$(( $sync_law:ident, $sync_label:literal )),* $(,)?]
        async [$(( $async_law:ident, $async_label:literal )),* $(,)?]
        bytes [$(( $bytes_law:ident, $bytes_label:literal )),* $(,)?]
    ) => {
        $(
            #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
            async fn $sync_law() {
                let (_fixture_guard, backend, _make, _make_bytes) = $fixture;
                let _ = $sync_label;
                $crate::registration_macro_support::$sync_law(backend);
            }
        )*
        $(
            #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
            async fn $async_law() {
                let (_fixture_guard, backend, make, _make_bytes) = $fixture;
                let _ = $async_label;
                $crate::registration_macro_support::$async_law(backend, make()).await;
            }
        )*
        $(
            #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
            async fn $bytes_law() {
                let (_fixture_guard, backend, make, make_bytes) = $fixture;
                let _ = $bytes_label;
                $crate::registration_macro_support::$bytes_law(backend, make(), make_bytes())
                    .await;
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
            }
        )*
    };
}

/// Register the attachment-condemnation cold-reopen adoption law (ADR 0067 §6).
#[macro_export]
macro_rules! attachment_condemnation_recovery_tests {
    ($fixture:block) => {
        $crate::attachment_condemnation_recovery_tests!(@catalogue $fixture; [
            (cold_reopen_adopts_old_generation_before_new_deletes, "attachment-condemnation-cold-reopen-adoption"),
        ]);
    };
    (@catalogue $fixture:block; [$(( $law:ident, $label:literal )),* $(,)?]) => {
        $(
            #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
            async fn $law() {
                let (_fixture_guard, factory, make_bytes, reopen) = $fixture;
                let _ = $label;
                $crate::registration_macro_support::$law(factory, make_bytes, reopen).await;
            }
        )*
    };
}

/// Register the stalled attachment-delete retry laws with a shared store clock.
#[macro_export]
macro_rules! attachment_stalled_retry_tests {
    ($fixture:block) => {
        $crate::attachment_stalled_retry_tests!(@catalogue $fixture; [
            (persistently_failing_delete_stalls_typed, "attachment-condemnation-delete-stall"),
            (stalled_delete_recovers_after_backoff, "attachment-stalled-delete-recovery"),
            (delete_retry_backoff_is_capped_and_never_early, "attachment-delete-retry-backoff"),
            (concurrent_stalled_retry_deletes_once, "attachment-stalled-retry-fence"),
        ]);
    };
    (@catalogue $fixture:block; [$(( $law:ident, $label:literal )),* $(,)?]) => {
        $(
            #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
            async fn $law() {
                let (_guard, factory, make_bytes, clock) = $fixture;
                let _ = $label;
                $crate::registration_macro_support::$law(factory, make_bytes, clock).await;
            }
        )*
    };
}

#[macro_export]
macro_rules! attachment_adoption_tests {
    ($fixture:block) => {
        $crate::attachment_adoption_tests!(@catalogue $fixture;
            bytes [
                (cross_session_attachment_adoption_conformance, "cross-owner-attachment-adoption"),
                (concurrent_adoption_deletes_once, "attachment-condemnation-concurrent-adoption"),
            ]
            runs [
                (attachment_condemnation_enumeration_conformance, "attachment-condemnation-enumeration"),
            ]
        );
    };
    (@catalogue $fixture:block;
        bytes [$(( $bytes_law:ident, $bytes_label:literal )),* $(,)?]
        runs [$(( $law:ident, $label:literal )),* $(,)?]
    ) => {
        $(
            #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
            async fn $bytes_law() {
                let (_fixture_guard, factory, make_bytes) = $fixture;
                let _ = $bytes_label;
                $crate::registration_macro_support::$bytes_law(factory, make_bytes).await;
            }
        )*
        $(
            #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
            async fn $law() {
                let (_fixture_guard, factory, _make_bytes) = $fixture;
                let _ = $label;
                $crate::registration_macro_support::$law(factory).await;
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
            }
        )*
    };
}

/// Register the completion projection-repair law.
#[macro_export]
macro_rules! process_projection_repair_tests {
    ($fixture:block) => {
        $crate::process_projection_repair_tests!(@catalogue $fixture; [
            (completion_replay_repairs_projection, "completion-projection-repair"),
        ]);
    };
    (@catalogue $fixture:block; [$(( $law:ident, $label:literal )),* $(,)?]) => {
        $(
            #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
            async fn $law() {
                let (_fixture_guard, registry, corrupt_projection) = $fixture;
                let _ = $label;
                $crate::registration_macro_support::$law(registry, corrupt_projection).await;
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
                let (_fixture_guard, backend) = $fixture;
                let _ = $label;
                $crate::registration_macro_support::$law(backend).await;
            }
        )*
    };
}

/// Register the fork observer-intent recovery law.
#[macro_export]
macro_rules! observer_intent_tests {
    ($fixture:block) => {
        $crate::observer_intent_tests!(@catalogue $fixture; [
            (fork_observer_transient_failure_retains_intent_until_publication, "fork-observer-intent"),
        ]);
    };
    (@catalogue $fixture:block; [$(( $law:ident, $label:literal )),* $(,)?]) => {
        $(
            #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
            async fn $law() {
                let (_fixture_guard, factory) = $fixture;
                let _ = $label;
                $crate::registration_macro_support::$law(factory).await;
            }
        )*
    };
}

#[macro_export]
macro_rules! process_trigger_retention_tests {
    ($fixture:block) => {
        $crate::process_trigger_retention_tests!(@catalogue $fixture; [
            (trigger_capture_route_and_compaction_refusal_matrix, "trigger-capture-compaction-matrix"),
            (trigger_occurrence_redelivery_after_reclaim, "trigger-occurrence-redelivery-after-reclaim"),
            (trigger_redelivery_after_forget_starts_again, "trigger-redelivery-after-forget"),
        ]);
    };
    (@catalogue $fixture:block; [$(( $law:ident, $label:literal )),* $(,)?]) => {
        $(
            #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
            async fn $law() {
                let (_fixture_guard, make) = $fixture;
                let _ = $label;
                $crate::registration_macro_support::$law(make).await;
            }
        )*
    };
}

/// Register the host tool-intent submission ledger's retention law
/// (FIG-1509). The fixture yields a guard and a constructor of a fresh
/// backend with a way to reopen it.
#[macro_export]
macro_rules! tool_intent_retention_tests {
    ($fixture:block) => {
        $crate::tool_intent_retention_tests!(@catalogue $fixture; [
            (tool_intent_submissions_reclaim_only_after_owner_death, "tool-intent-reclaim"),
        ]);
    };
    (@catalogue $fixture:block; [$(( $law:ident, $label:literal )),* $(,)?]) => {
        $(
            #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
            async fn $law() {
                let (_fixture_guard, make) = $fixture;
                let _ = $label;
                $crate::registration_macro_support::$law(make).await;
            }
        )*
    };
}

/// Register the trigger-occurrence tombstone retention and forget laws. The
/// fixture yields a guard and a constructor from a clock to a trigger store.
#[macro_export]
macro_rules! trigger_occurrence_tombstone_retention_tests {
    ($fixture:block) => {
        $crate::trigger_occurrence_tombstone_retention_tests!(@catalogue $fixture; [
            (trigger_occurrence_tombstones_survive_every_reclaim, "trigger-occurrence-tombstone-retention"),
            (trigger_tombstone_forget_has_an_exclusive_write_time_cutoff, "trigger-tombstone-forget-cutoff"),
        ]);
    };
    (@catalogue $fixture:block; [$(( $law:ident, $label:literal )),* $(,)?]) => {
        $(
            #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
            async fn $law() {
                let (_fixture_guard, make) = $fixture;
                let _ = $label;
                $crate::registration_macro_support::$law(make).await;
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
            }
        )*
    };
}

#[macro_export]
macro_rules! session_delete_blob_reclaim_tests {
    ($fixture:block) => {
        $crate::session_delete_blob_reclaim_tests!(@catalogue $fixture; [
            (session_delete_blob_reclaim_conformance, "session-delete-blob-reclaim"),
            (session_delete_reclaims_after_head_advances, "session-delete-advanced-head"),
            (session_delete_preserves_admission_base_blobs, "session-delete-admission-base"),
        ]);
    };
    (@catalogue $fixture:block; [$(( $law:ident, $label:literal )),* $(,)?]) => {
        $(
            #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
            async fn $law() {
                let (_fixture_guard, backend, make) = $fixture;
                let _ = $label;
                $crate::registration_macro_support::$law(backend, make).await;
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
            (trigger_subscription_change_cursor_law, "trigger-subscription-change-cursor"),
            (host_scope_filters_list_cancel_and_deactivate_uniformly, "host-scope-filters"),
        ]);
    };
    (@catalogue $fixture:block; [$(( $law:ident, $label:literal )),* $(,)?]) => {
        $(
            #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
            async fn $law() {
                let (_fixture_guard, make) = $fixture;
                let _ = $label;
                $crate::registration_macro_support::$law(make).await;
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
        }
    };
    ($fixture:block; $law:ident, $label:literal, blob) => {
        #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
        async fn $law() {
            let (_fixture_guard, backend, factory, registry, probe) = $fixture;
            let _ = $label;
            $crate::registration_macro_support::$law(backend, factory, registry, probe).await;
        }
    };
}

/// Register one independently reported test per process-prune reclamation law.
#[macro_export]
macro_rules! process_prune_reclaim_tests {
    ($fixture:block) => {
        $crate::process_prune_reclaim_tests!(@catalogue $fixture; [
            (process_prune_preserves_independent_session_checkpoint_roots, "process-prune-independent-checkpoints", blob),
        ]);
    };
    (@catalogue $fixture:block; [$(( $law:ident, $label:literal, $mode:ident )),* $(,)?]) => {
        $(
            $crate::__process_prune_reclaim_register!($fixture; $law, $label, $mode);
        )*
    };
}

/// Register the process-prune referrer law. The fixture yields
/// `(guard, process registry, process-environment store)`.
#[macro_export]
macro_rules! process_prune_start_staging_tests {
    ($fixture:block) => {
        $crate::process_prune_start_staging_tests!(@catalogue $fixture; [
            (prune_and_late_transfer_fences, "process-prune-referrer-fence"),
            (two_starts_share_one_captured_environment, "shared-captured-environment"),
        ]);
    };
    (@catalogue $fixture:block; [$(( $law:ident, $label:literal )),* $(,)?]) => {
        $(
            #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
            async fn $law() {
                let (_fixture_guard, registry, env_store) = $fixture;
                let _ = $label;
                $crate::registration_macro_support::$law(registry, env_store).await;
            }
        )*
    };
}

/// Register the start-staging laws a global start key needs (FIG-4111). The
/// fixture yields `(guard, process registry, artifact referrer ports)`.
#[macro_export]
macro_rules! process_start_staging_tests {
    ($fixture:block) => {
        $crate::process_start_staging_tests!(@catalogue $fixture; [
            (a_refused_start_never_strands_a_concurrent_start_under_its_key, "refused-start-concurrent-stager"),
            (a_start_key_end_applied_before_the_rescue_keeps_the_concurrent_start_held, "start-end-applied-before-rescue"),
        ]);
    };
    (@catalogue $fixture:block; [$(( $law:ident, $label:literal )),* $(,)?]) => {
        $(
            #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
            async fn $law() {
                let (_fixture_guard, registry, ports) = $fixture;
                let _ = $label;
                $crate::registration_macro_support::$law(registry, ports).await;
            }
        )*
    };
}

/// Register the immutable process-definition laws (ADR 0113 §3.6): no
/// reclamation while a referrer holds a definition, eventual reclamation after
/// the last, and exact start-by-id replay at every crash boundary. The fixture
/// yields `(guard, process registry, artifact referrer ports)`.
#[macro_export]
macro_rules! process_definition_tests {
    ($fixture:block) => {
        $crate::process_definition_tests!(@catalogue $fixture; [
            (definition_is_not_reclaimed_while_any_referrer_holds_it, "definition-held-by-every-referrer"),
            (definition_is_eventually_reclaimed_after_its_last_referrer, "definition-reclaimed-after-last-referrer"),
            (definition_publication_verifies_bytes_on_an_existing_id, "definition-publication-immutable"),
            (start_by_id_replays_exactly_at_before_create_attempt_commit, "start-by-id-before-create-commit"),
            (start_by_id_replays_exactly_at_after_attempt_commit_before_publication, "start-by-id-before-publication"),
            (start_by_id_replays_exactly_at_after_publication_before_frame_commit, "start-by-id-before-frame-commit"),
            (start_by_id_replays_exactly_at_before_start_admission, "start-by-id-before-admission"),
            (start_by_id_replays_exactly_at_after_registration_before_result, "start-by-id-after-registration"),
            (start_by_id_replays_exactly_at_after_recorded_start, "start-by-id-after-recorded-start"),
        ]);
    };
    (@catalogue $fixture:block; [$(( $law:ident, $label:literal )),* $(,)?]) => {
        $(
            #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
            async fn $law() {
                let (_fixture_guard, registry, ports) = $fixture;
                let _ = $label;
                $crate::registration_macro_support::$law(registry, ports).await;
            }
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
            (lashlang_last_referrer_reclaims_module, "lashlang-artifact-last-referrer"),
            (lashlang_abandoned_start_reclaims_module, "lashlang-artifact-abandoned-start"),
            (lashlang_carry_preserves_module, "lashlang-artifact-carry"),
            (lashlang_ended_referrer_fences_late_publication, "lashlang-artifact-referrer-fence"),
            (lashlang_hostile_module_references_are_rejected, "lashlang-artifact-hostile-reference"),
            (lashlang_alpha_variants_publish_distinct_refs, "lashlang-artifact-alpha-variants"),
            (lashlang_artifact_survives_reopen, "lashlang-artifact-reopen"),
            (process_execution_env_store_fresh_instances, "process-env-fresh-instances"),
            (process_environment_namespace, "process-env-hostile-reference"),
            (process_env_last_referrer_reclaims_bytes, "process-env-last-referrer"),
            (process_env_carry_precedes_reclamation, "process-env-carry"),
            (slow_process_env_writer_is_fenced, "process-env-slow-writer"),
            (process_env_survives_reopen, "process-env-reopen"),
            (artifact_store_cross_namespace_isolation, "artifact-store-cross-namespace"),
            (turn_prelude_reads_back_by_digest, "turn-prelude-read-by-digest"),
            (turn_prelude_is_released_with_its_journal, "turn-prelude-journal-release"),
        ]);
    };
    (@catalogue $fixture:block; [$(( $law:ident, $label:literal )),* $(,)?]) => {
        $(
            #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
            async fn $law() {
                let (_fixture_guard, make) = $fixture;
                let _ = $label;
                $crate::fused_artifact_store::$law(make).await;
            }
        )*
    };
}

/// Register the referrer laws on a fresh fused artifact-store fixture.
/// The fixture has the same shape as `artifact_store_reopenable_tests!`.
#[macro_export]
macro_rules! artifact_referrer_tests {
    ($fixture:block) => {
        $crate::artifact_referrer_tests!(@catalogue $fixture; [
            (publication_racing_frame_end_is_fenced, "artifact-referrer-publication-race"),
            (host_pins_reclaim_and_fence, "artifact-referrer-host-pins"),
            (attachment_only_referrers_cannot_acquire_artifacts, "artifact-referrer-kind-refusal"),
            (captured_environments_are_shared_until_the_last_referrer_ends, "captured-environment-referrers"),
            (every_referrer_kind_has_one_canonical_id, "artifact-referrer-canonical-id"),
            (retry_idempotency_after_destination_ends, "artifact-referrer-retry-idempotency"),
        ]);
    };
    (@catalogue $fixture:block; [$(( $law:ident, $label:literal )),* $(,)?]) => {
        $(
            #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
            async fn $law() {
                let (_fixture_guard, make) = $fixture;
                let _ = $label;
                $crate::$law(make).await;
            }
        )*
    };
}

/// Register the retained tool-material laws (FIG-4889) on a fixture that
/// returns `(guard, make)`, where `make()` yields a
/// `ReopenableToolMaterialStore` over a fresh durable catalog.
#[macro_export]
macro_rules! tool_material_tests {
    ($fixture:block) => {
        $crate::tool_material_tests!(@catalogue $fixture; [
            source_material_reads_refuse_typed_without_a_fresh_body,
        ]);
    };
    (@catalogue $fixture:block; [$($law:ident),* $(,)?]) => {
        $(
            #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
            async fn $law() {
                let (_fixture_guard, make) = $fixture;
                $crate::material_retention::$law(make).await;
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
            }
        )*
    };
}

/// Register the deployment-store (session catalog) laws.
///
/// The fixture yields `(guard, make, make_attached, effect_host)`: `make`
/// returns a fresh, empty `Arc<dyn ConformanceDeployment>`, and
/// `make_attached` a fresh one with the attachment byte store of its
/// substrate.
#[macro_export]
macro_rules! session_store_factory_tests {
    ($fixture:block) => {
        $crate::session_store_factory_tests!(@catalogue $fixture; [
            (session_store_factory, "session-store-factory"),
        ]);
        $crate::session_store_factory_tests!(@turn_cancel $fixture; [
            (session_meta_records_the_process_that_owns_it, "session-meta-owning-process"),
            (a_closing_session_lists_as_closing_never_as_live, "catalog-closing-entry"),
            (concurrent_session_admissions_preserve_one_relation, "concurrent-session-relation"),

            (a_session_that_never_ran_a_turn_forks_at_its_creation_revision, "fork-empty-session"),
        ]);
        $crate::session_store_factory_tests!(@turn_cancel_hosted $fixture; [
            (fork_inherits_history_without_execution_queues_waits_or_journals, "fork-execution-isolation"),
        ]);
    };
    (@catalogue $fixture:block; [$(( $law:ident, $label:literal )),* $(,)?]) => {
        $(
            #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
            async fn $law() {
                let (_fixture_guard, make, make_attached, _effect_host) = $fixture;
                let _ = $label;
                $crate::registration_macro_support::$law(make, make_attached).await;
            }
        )*
    };
    (@turn_cancel $fixture:block; [$(( $law:ident, $label:literal )),* $(,)?]) => {
        $(
            #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
            async fn $law() {
                let (_fixture_guard, make, _make_attached, _effect_host) = $fixture;
                let _ = $label;
                $crate::registration_macro_support::$law(make()).await;
            }
        )*
    };
    (@turn_cancel_hosted $fixture:block; [$(( $law:ident, $label:literal )),* $(,)?]) => {
        $(
            #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
            async fn $law() {
                let (_fixture_guard, make, _make_attached, effect_host) = $fixture;
                let _ = $label;
                $crate::registration_macro_support::$law(make(), effect_host).await;
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
        }
    };
}

/// Register the session-config settlement laws, and the creation-budget law
/// (FIG-4393). The fixture hands back a guard and a maker of fresh backends;
/// each law builds its runtime, or its creation, over one.
#[macro_export]
macro_rules! session_config_settlement_tests {
    ($(#[$attr:meta])* $fixture:block) => {
        $crate::session_config_settlement_tests!(@catalogue [$(#[$attr])*] $fixture; [
            (session_config_settlement_pending_returns_without_wait, "config-settlement-pending"),
            (cancelled_session_config_settlement_is_typed, "config-settlement-cancelled"),
            (superseded_config_settlement_adopts_the_newer_head, "config-settlement-superseded"),
            (session_creation_refuses_a_head_no_commit_fits, "creation-budget"),
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
            }
        )*
    };
}

/// Register the session read-view law: a storage law that reads through the
/// session-store factory.
///
/// The fixture yields `(guard, its session-store factory)`.
#[macro_export]
macro_rules! session_read_view_tests {
    ($fixture:block) => {
        $crate::session_read_view_tests!(@catalogue $fixture; [
            (session_store_factory_read_session, "session-read-view"),
        ]);
    };
    (@catalogue $fixture:block; [$(( $law:ident, $label:literal )),* $(,)?]) => {
        $(
            #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
            async fn $law() {
                let (_fixture_guard, factory) = $fixture;
                let _ = $label;
                $crate::registration_macro_support::$law(factory).await;
            }
        )*
    };
}

#[macro_export]
macro_rules! process_prune_session_store_tests {
    ($fixture:block) => {
        $crate::process_prune_session_store_tests!(@catalogue $fixture; [
            (ended_process_record_has_no_attachment_edges, "process-prune-session-store-cleanup"),
            (a_same_start_key_successor_after_prune_has_independent_attachment_referrers, "same-key-successor-after-prune"),
            (reclaim_races_fork_and_unpin_without_using_process_roots, "reclaim-fork-unpin-race"),
        ]);
    };
    (@catalogue $fixture:block; [$(( $law:ident, $label:literal )),* $(,)?]) => {
        $(
            #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
            async fn $law() {
                let (_fixture_guard, factory, registry, effect_host) = $fixture;
                let _ = $label;
                $crate::registration_macro_support::$law(factory, registry, effect_host).await;
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
        }
    };
    ($fixture:block; $law:ident, $label:literal, capacity) => {
        #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
        async fn $law() {
            let (_guard, _make, capacity, _ttl, _wait, _incarnations) = $fixture;
            let _ = $label;
            $crate::registration_macro_support::$law(capacity).await;
        }
    };
    ($fixture:block; $law:ident, $label:literal, ttl) => {
        #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
        async fn $law() {
            let (_guard, _make, _capacity, ttl, wait, _incarnations) = $fixture;
            let _ = $label;
            $crate::registration_macro_support::$law(ttl, wait).await;
        }
    };
    ($fixture:block; $law:ident, $label:literal, incarnation) => {
        #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
        async fn $law() {
            let (_guard, _make, _capacity, _ttl, _wait, (original, fresh, preserved)) = $fixture;
            let _ = $label;
            $crate::registration_macro_support::$law(original, fresh, preserved).await;
        }
    };
}

/// Register one independently reported test per live-replay law.
#[macro_export]
macro_rules! live_replay_tests {
    ($fixture:block) => {
        $crate::live_replay_tests!(@catalogue $fixture; [
            (live_replay_store, "live-replay", plain),
            (live_replay_store_burst, "live-replay-burst", plain),
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
                let (factory, make_bytes, reopen) = make().await;
                $crate::registration_macro_support::$law(factory, make_bytes, reopen).await;
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
            }
        )*
    };
}

/// Register checkpoint-admission transaction accounting. The fixture supplies a
/// store, the session it probes and the backend's own transaction counter.
#[macro_export]
macro_rules! checkpoint_admission_probe_tests {
    ($fixture:block) => {
        $crate::checkpoint_admission_probe_tests!(@catalogue $fixture; [
            (checkpoint_admission_probe_transaction_counts, "checkpoint-admission-probe-counts"),
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
            }
        )*
    };
}

/// Register each durable attachment-referrer law independently.
#[macro_export]
macro_rules! attachment_referrer_tests {
    ($fixture:block) => { $crate::attachment_referrer_tests!(@laws $fixture; [
        ended_process_record_refuses_attachment_writes_and_acquisitions,
        upload_staging_identities_are_distinct_guarded_and_fenced_independently,
        commit_and_enqueue_acquire_session_edges_all_or_nothing,
        retained_output_is_held_by_its_execution_until_a_commit_names_it,
        attachment_prefix_pin_keeps_the_session_edge_until_unpin,
        session_referrer_waits_for_graph_retirement,
        condemnation_needs_no_edge_and_no_pending_write,
        skipped_attachment_referrer_kind_cannot_authorize_delete,
        truncated_attachment_root_page_cannot_authorize_delete,
        attachment_root_sources_partition_start_inputs,
        complete_attachment_roots_cover_every_kind_and_exhaust_pages,
    ]); };
    (@laws $fixture:block; [$($law:ident),* $(,)?]) => { $(
        #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
        async fn $law() {
            let (_guard, handles) = $fixture;
            $crate::$law(handles).await;
        }
    )* };
}

/// Register the durable queue's post-mutation observation recovery law.
#[macro_export]
macro_rules! queue_observation_tests {
    ($fixture:block) => {
        $crate::queue_observation_tests!(@law $fixture; queue_head_read_failure_publishes_recoverable_gap);
        $crate::queue_observation_tests!(@law $fixture; queue_publication_failure_preserves_committed_mutation);
    };
    (@law $fixture:block; $law:ident) => {
        #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
        async fn $law() {
            let (_guard, backend) = $fixture;
            $crate::registration_macro_support::$law(backend).await;
        }
    };
}

#[macro_export]
macro_rules! checkpoint_profile_tests {
    ($fixture:block) => {
        $crate::checkpoint_profile_tests!(@law $fixture; checkpoint_identity_is_independent_of_compression_profile);
        $crate::checkpoint_profile_tests!(@law $fixture; checkpoint_profile_change_preserves_refs_budget_and_atomic_root_leaves);
    };
    (@law $fixture:block; $law:ident) => {
        #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
        async fn $law() {
            let (_guard, stores) = $fixture;
            $crate::registration_macro_support::$law(stores).await;
        }
    };
}
