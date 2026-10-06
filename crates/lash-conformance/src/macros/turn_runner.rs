//! Registration macros for surviving turn-runner and binding laws.
//! Split from `macros.rs` to keep each catalogue file inside the support-file
//! line budget.

/// Register the laws that execute a real turn through the tier's
/// [`ConformanceTurnRunner`](crate::ConformanceTurnRunner): the public
/// signal-intent wake.
///
/// The fixture hands back a guard, a session prefix, the tier's effect host,
/// the store set under test (whose session catalog and process registry the
/// law's runtime uses), the process-work substrate, the tier's turn runner and
/// a post-law verification handed the law's name. Restate runs each turn inside a live handler
/// (`#[ignore]`d, deferred to `run-conformance-e2e`).
#[macro_export]
macro_rules! turn_runner_tests {
    ($(#[$attr:meta])* $fixture:block) => {
        $crate::__turn_runner_register!([$(#[$attr])*] $fixture;
            (public_signal_intent_wakes_parked_process, "public-signal-intent-wake"));
    };
}

/// Register the FIG-1293 migrated-tools crash-redrive law. The fixture hands
/// back a guard, a prefix, the effect host, the store set under test, the tier's
/// turn runner and the plugin factories (the standard protocol, `spawn_agent`,
/// `cancel_process`) from the crates above this one.
#[macro_export]
macro_rules! migrated_tools_redrive_tests {
    ($(#[$attr:meta])* $fixture:block) => {
        $crate::migrated_tools_redrive_tests!(@law [$(#[$attr])*] $fixture;
            (public_migrated_tools_redrive_to_literal_outcomes, "migrated-tools-redrive"));
    };
    (@law [$($attr:tt)*] $fixture:block; ($law:ident, $label:literal)) => {
        $($attr)*
        #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
        async fn $law() {
            let (_guard, prefix, host, stores, runner, plugins) = $fixture;
            $crate::registration_macro_support::$law(prefix, host, stores, runner, plugins).await;
        }
    };
}

/// Register the admitted-head redrive law (FIG-3682): a direct turn crashed
/// after its own commit and redriven replays at the head it was admitted on,
/// under the turn index its admission recorded. The fixture hands back a
/// guard, a prefix, the tier's effect host, the store set under test and its
/// [`ConformanceTurnRunner`](crate::ConformanceTurnRunner). The turn calls no
/// tool, so it runs wherever a direct turn runs.
#[macro_export]
macro_rules! admitted_head_redrive_tests {
    ($(#[$attr:meta])* $fixture:block) => {
        $crate::admitted_head_redrive_tests!(@law [$(#[$attr])*] $fixture;
            (a_turn_redriven_after_its_commit_replays_at_its_admitted_head, "admitted-head-redrive"));
    };
    (@law [$($attr:tt)*] $fixture:block; ($law:ident, $label:literal)) => {
        $($attr)*
        #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
        async fn $law() {
            let (_guard, prefix, host, stores, runner) = $fixture;
            $crate::registration_macro_support::$law(prefix, host, stores, runner).await;
        }
    };
}

/// Register the turn-config laws (FIG-3600 S6, D3 §5.2; FIG-3838, FIG-3842,
/// FIG-5093): a run resolves its run spec against its session config once, as a recorded
/// step, and every replay of the run executes under that record. The fixture is the admitted-head one: a guard, a
/// prefix, the tier's effect host, the store set under test and its
/// [`ConformanceTurnRunner`](crate::ConformanceTurnRunner).
#[macro_export]
macro_rules! turn_config_tests {
    ($(#[$attr:meta])* $fixture:block) => {
        $crate::turn_config_tests!(@law [$(#[$attr])*] $fixture;
            (a_committed_run_redriven_after_a_profile_change_answers_from_its_receipt, "turn-config-stale-redrive"));
        $crate::turn_config_tests!(@law [$(#[$attr])*] $fixture;
            (an_older_admission_redriven_after_a_profile_change_is_fenced_out, "turn-config-stale-fenced-out"));
        $crate::turn_config_tests!(@law [$(#[$attr])*] $fixture;
            (an_input_sent_after_a_config_command_runs_on_the_new_profile, "turn-config-after-command"));
        $crate::turn_config_tests!(@law [$(#[$attr])*] $fixture;
            (a_config_transaction_waits_while_a_run_owns_the_head, "turn-config-pending-while-run"));
        $crate::turn_config_tests!(@law [$(#[$attr])*] $fixture;
            (one_config_resolution_per_run, "turn-config-one-resolution"));
        $crate::turn_config_tests!(@law [$(#[$attr])*] $fixture;
            (an_unbindable_llm_profile_retries_and_never_fails_the_turn, "turn-config-unbindable-retries"));
        $crate::turn_config_tests!(@law [$(#[$attr])*] $fixture;
            (an_unknown_profile_key_is_refused_typed_and_publishes_nothing, "turn-config-unknown-key"));
        $crate::turn_config_tests!(@law [$(#[$attr])*] $fixture;
            (a_corrupt_recorded_namespace_is_corruption_and_never_a_recorded_refusal, "turn-config-corrupt-namespace"));
        $crate::turn_config_tests!(@law [$(#[$attr])*] $fixture;
            (a_profile_change_records_the_binding_minted_where_it_resolves, "turn-config-minted-at-resolution"));
        $crate::turn_config_tests!(@law [$(#[$attr])*] $fixture;
            (a_reasoning_change_is_judged_against_the_final_recorded_llm_profile, "turn-config-reasoning"));
        $crate::turn_config_tests!(@law [$(#[$attr])*] $fixture;
            (run_specs_split_runs_in_admission_order, "run-spec-selector"));
        $crate::turn_config_tests!(@law [$(#[$attr])*] $fixture;
            (the_default_spec_is_the_snapshot_after_the_command_drain, "run-spec-default"));
        $crate::turn_config_tests!(@law [$(#[$attr])*] $fixture;
            (a_config_command_after_a_pinned_run_resolves_over_the_sticky_config, "run-spec-sticky-command"));
        $crate::turn_config_tests!(@law [$(#[$attr])*] $fixture;
            (a_run_resolves_its_spec_once_across_a_crash, "run-spec-once"));
        $crate::turn_config_tests!(@law [$(#[$attr])*] $fixture;
            (a_missing_definition_retries_unrecorded_until_it_is_deployed, "run-spec-missing"));
        $crate::turn_config_tests!(@law [$(#[$attr])*] $fixture;
            (a_batch_shares_one_spec_that_each_run_resolves_once, "run-spec-batch"));
        $crate::turn_config_tests!(@law [$(#[$attr])*] $fixture;
            (a_batch_keeps_its_turn_lane_place_behind_the_command_lane, "run-spec-batch-order"));
        $crate::turn_config_tests!(@law [$(#[$attr])*] $fixture;
            (runs_stating_different_tool_grants_each_run_under_their_own, "run-grants-own"));
        $crate::turn_config_tests!(@law [$(#[$attr])*] $fixture;
            (a_runs_tool_grants_survive_its_crash_and_a_cold_reopen, "run-grants-reopen"));
        $crate::turn_config_tests!(@law [$(#[$attr])*] $fixture;
            (a_tool_access_command_still_shapes_later_runs_that_state_no_grants, "run-grants-command"));
        $crate::turn_config_tests!(@law [$(#[$attr])*] $fixture;
            (a_recovered_follow_on_inherits_its_runs_recorded_execution, "run-spec-follow-on-inherit"));
        $crate::turn_config_tests!(@law [$(#[$attr])*] $fixture;
            (a_redriven_switch_owes_its_follow_on_under_the_bound_its_run_resolved, "run-spec-follow-on-bound"));
        $crate::turn_config_tests!(@law [$(#[$attr])*] $fixture;
            (a_redrive_runs_under_the_execution_controls_its_run_recorded, "turn-config-recorded-controls-redrive"));
        $crate::turn_config_tests!(@law [$(#[$attr])*] $fixture;
            (a_redrive_assembles_the_terminal_its_run_recorded_termination_decides, "turn-config-recorded-termination-redrive"));
        $crate::turn_config_tests!(@law [$(#[$attr])*] $fixture;
            (a_missing_recorded_termination_is_a_typed_terminal_refusal, "turn-config-missing-recorded-termination"));
        $crate::turn_config_tests!(@law [$(#[$attr])*] $fixture;
            (a_redrive_calls_the_model_with_the_request_defaults_its_run_recorded, "turn-config-recorded-request-defaults-redrive"));
    };
    (@law [$($attr:tt)*] $fixture:block; ($law:ident, $label:literal)) => {
        $($attr)*
        #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
        async fn $law() {
            let (_guard, prefix, host, stores, runner) = $fixture;
            $crate::registration_macro_support::$law(prefix, host, stores, runner).await;
        }
    };
}

/// Register the session shift's admission laws (FIG-3600, ADR 0105 §2): a
/// shift admits and seals every run before its first effect, one admission
/// at a time holds the session, and a replay mints no ownership. The fixture
/// is the admitted-head one: a guard, a prefix, the tier's effect host, the
/// store set under test and its
/// [`ConformanceTurnRunner`](crate::ConformanceTurnRunner).
///
/// A tier that must quarantine a law registers the laws by name and marks the
/// quarantined ones:
/// `shift_admission_tests!(@laws [] { fixture }; [(law, "label"), #[ignore = "why"] (other, "label"), ...])`.
#[macro_export]
macro_rules! shift_admission_tests {
    ($(#[$attr:meta])* $fixture:block) => {
        $crate::shift_admission_tests!(@laws [$(#[$attr])*] $fixture; [
    (a_terminal_run_never_reparks, "s7b-0"),
    (a_diverged_run_parks_once_holds_its_admitted_rows_blocks_admission_and_completes_after_restore, "s7b-15"),
    (an_exhausted_run_parks_engine_retry_exhausted_via_reconcile_idempotently_with_no_evidence, "s7b-13"),
    (a_parked_runs_fence_stays_current_until_a_verb, "s7b-14"),

    (sends_behind_a_parked_run_commit_but_are_not_admitted, "s7b-8"),
    (redrive_under_a_restored_build_completes_once_and_clears_the_park, "s7b-9"),
    (a_stale_redrive_is_fenced_by_a_later_cancel, "s7b-10"),
    (run_scope_close_runs_after_terminal_evidence_at_least_once_never_for_parked, "s7b-11"),
    (a_run_crashed_at_its_report_handover_still_closes_its_scope, "s7b-11b"),
    (cancel_fork_and_close_raise_the_shift_epoch_and_redrive_does_not, "s7b-12"),

    (cancel_of_a_parked_run_writes_cancelled_settles_its_input_and_drains_the_next, "s7b-1"),
    (no_row_stays_bound_after_a_runs_verb_close_or_lost_end, "run-verb-unbinds"),
    (a_refused_run_ends_once_and_its_next_input_admits_a_new_run, "refused-run-end"),
    (a_run_with_no_engine_execution_ends_only_once_it_started, "lost-run-no-run"),
    (an_obsolete_executor_never_ends_its_successors_run, "obsolete-executor"),
    (fork_releases_the_old_owner_before_the_new_run_executes_in_original_order_on_a_fresh_journal, "s7b-2"),
    (verbs_are_park_id_cas, "s7b-3"),
    (redrive_under_the_same_build_reparks_the_same_park_with_attempts_plus_one, "s7b-4"),
    (cancel_or_fork_of_a_redriving_run_is_refused, "s7b-5"),
    (an_intent_survives_a_crash_at_every_gap_and_reconcile_completes_it, "s7b-6"),
    (engine_refusals_are_retained_and_listed, "s7b-7"),
    (a_run_parked_on_a_later_physical_turn_is_cleared_by_its_commit, "s7b-16"),
    (a_redrive_the_run_ran_past_is_never_applied_again, "s7b-17"),
    (a_stale_paused_listing_never_reparks_a_resumed_run, "s7b-18"),
    (a_parked_session_is_asked_to_work_only_through_its_ingress_obligation, "s7b-19"),
    (a_send_racing_an_unsettled_redrive_is_refused_until_the_redrive_settles, "l2-1"),
    (every_order_of_a_send_and_a_redrives_settle_admits_nothing_ahead_of_the_redrive, "l2-1b"),
    (a_lost_resume_ack_is_reconciled_before_queued_work_is_admitted, "l2-2"),
    (a_failing_child_cancel_never_wedges_its_runs_cancel_or_fork, "s8c-1"),
    (a_delivery_whose_claim_was_retaken_never_settles_its_intent, "s8c-2"),
    (an_intent_whose_engine_half_keeps_failing_stalls_at_its_ceiling_and_unwedges_its_session, "s8c-3"),
    (a_store_fault_in_an_intents_delivery_stalls_its_obligation_and_never_wedges_its_session, "f09-1"),
    (re_arming_a_refused_intent_makes_it_owed_again_and_its_delivery_completes_it, "f09-2"),
    (a_refused_follow_on_shift_keeps_the_intents_obligation_due, "s8c-4"),
            (one_unfinished_run_per_session, "run-one-unfinished"),
            (admission_delivers_every_row_it_binds, "run-admission-delivers"),
    (run_admission_binds_cancellation_authority_with_its_rows, "run-admission-cancel-binding"),
    (preparing_or_refusing_admission_leaves_cancellation_authority_unbound, "run-admission-unbound-proposal"),
            (a_run_admission_is_idempotent_across_new_rows_and_fences, "run-admission-idempotent"),
            (one_shift_admits_many_items, "shift-many-items"),
            (replay_cannot_mint_ownership, "shift-replay-ownership"),
            (admission_precedes_first_effect, "shift-admission-first"),
            (parked_run_blocks_admission, "shift-parked-run"),
            (a_command_enqueued_after_an_input_runs_admission_waits_for_the_next_boundary, "shift-command-after-admission"),
            (a_committed_run_answers_its_terminal_by_run, "shift-run-answered"),
            (a_host_id_naming_a_terminal_run_is_answered_not_rerun, "shift-run-adopted"),
            (a_run_whose_admission_a_successor_sealed_commits_nothing, "shift-run-superseded"),
            (a_run_recorded_under_one_executor_is_never_admitted_by_another, "shift-run-one-executor"),
            (a_lost_acceptors_run_is_executed_once_by_the_sessions_shift, "shift-run-lost-acceptor"),
            (admit_run_refuses_another_engine_held_executor, "run-admission-executor"),
            (first_admission_wins_without_changing_business_identity, "trace-first-writer"),
            (a_refused_acceptor_adopts_the_outcome_its_runs_executor_recorded, "shift-run-acceptor-adopts"),
            (a_parent_turn_acceptors_run_is_closed_to_a_later_drive, "shift-run-acceptor-recorded"),
            (a_command_runs_redrive_replays_its_recorded_outcome, "shift-command-run-redrive"),
            (a_host_task_is_admitted_as_its_own_operation_run, "shift-operation-run"),
            (a_run_end_closes_its_turn_scope_in_the_process_registry, "shift-run-registry-close"),
            (a_joined_inputs_turn_scope_closes_with_its_admitting_run, "shift-joined-scope-close"),
            (an_idle_session_admits_its_turn_lane_in_enqueue_order_whatever_the_kind, "shift-idle-turn-lane-order"),
            (a_turn_never_takes_an_item_past_an_earlier_unconsumed_item_of_the_other_kind, "shift-turn-lane-contiguous"),
        ]);
    };
    (@laws $attrs:tt $fixture:block; [$($(#[$law_attr:meta])* ( $law:ident, $label:literal )),* $(,)?]) => {
        $(
            $crate::shift_admission_tests!(@law $attrs [$(#[$law_attr])*] $fixture; ($law, $label));
        )*
    };
    (@law [$($attr:tt)*] [$($law_attr:tt)*] $fixture:block; ($law:ident, $label:literal)) => {
        $($attr)*
        $($law_attr)*
        #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
        async fn $law() {
            let (_guard, prefix, host, stores, runner) = $fixture;
            $crate::registration_macro_support::$law(prefix, host, stores, runner).await;
        }
    };
}

/// Register the driver-turn ownership law (FIG-3607 contract 4, FIG-4489):
/// every logical turn a shift runs — a host-input run's, a wake run's, a
/// frame or terminal-checkpoint follow-on's, and a follow-on a later shift
/// recovered — is owned by `Turn(logical run)`, writes positive terminal
/// evidence, and closes that run's scope exactly once; a parked run keeps
/// its scope open until a cancel ends it. The fixture is the admitted-head
/// one: a guard, a prefix, the tier's effect host, the store set under test
/// and its [`ConformanceTurnRunner`](crate::ConformanceTurnRunner), which
/// must crash a turn from outside its attempt.
#[macro_export]
macro_rules! driver_turn_ownership_tests {
    ($(#[$attr:meta])* $fixture:block) => {
        $(#[$attr])*
        #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
        async fn every_driver_turn_is_owned_by_its_run() {
            let (_guard, prefix, host, stores, runner) = $fixture;
            $crate::registration_macro_support::every_driver_turn_is_owned_by_its_run(
                prefix, host, stores, runner,
            )
            .await;
        }
    };
}

/// Register the session-close laws L-D1..L-D6 (FIG-3600 S7, FIG-3607 item
/// 7): a deletion's refusals come before its recorded `BeginSessionClose`
/// step, the close ends every open run `SessionDeleted` and raises the shift
/// epoch, its engine half is retained on failure, and its `CloseSession`
/// intent outlives the session as the tombstone its runs are answered from,
/// and is the one writer of the session scope's close row (D11); and the
/// two-phase delete's laws L-D7..L-D12 (ADR 0109 §4): the close's
/// acknowledgement arms the `SessionDelete` obligation, which waits on
/// exactly the session's undelivered cleanup and then deletes it; a deletion
/// retried after its close is not refused by a closure pin the close
/// superseded, and the physical delete retires that pin; the frame cleanup
/// the delete arms, whose claimant dies inside it, is retaken at its lapse
/// and settled.
/// The fixture is the admitted-head one; a tier with a
/// [`ConformanceTurnRunner`](crate::ConformanceTurnRunner) runs the close
/// inside the engine's `SessionDelete` handler.
#[macro_export]
macro_rules! session_close_tests {
    ($fixture:block) => {
        $crate::session_close_tests!(@laws $fixture; [
            (session_delete_closes_active_and_parked_runs_as_session_deleted, "session-close-runs"),
            (a_refused_deletion_closes_nothing, "session-close-refused"),
            (the_close_intent_is_idempotent_retained_on_failure_and_survives_deletion, "session-close-retained"),
            (a_run_commit_racing_a_close_is_refused_stale_fence, "session-close-fence"),
            (session_delete_writes_exactly_one_close_row_via_its_intent, "session-close-one-row"),
            (a_close_interrupted_before_its_acknowledgement_is_finished_and_its_tombstone_kept, "session-close-crash"),
            (a_close_acknowledgement_arms_the_session_delete_obligation, "session-delete-arm"),
            (session_delete_counts_only_the_sessions_undelivered_cleanup, "session-delete-cleanup"),
            (the_physical_delete_waits_for_cleanup_then_deletes_the_session, "session-delete-finalizer"),
            (a_deletion_retried_after_its_close_is_not_refused_by_a_pin_the_close_superseded, "session-close-superseded-pin"),
            (the_physical_delete_retires_the_closure_pins_its_close_superseded, "session-delete-superseded-pin"),
            (a_frame_cleanup_whose_claimant_died_is_retaken_at_its_lapse_and_settled, "session-delete-frame-cleanup-lapse"),
        ]);
    };
    (@laws $fixture:block; [$(($law:ident, $label:literal)),* $(,)?]) => {
        $($crate::session_close_tests!(@one $fixture; ($law, $label));)*
    };
    (@one $fixture:block; ($law:ident, $label:literal)) => {
        #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
        async fn $law() {
            let (_guard, prefix, host, stores, runner) = $fixture;
            $crate::registration_macro_support::$law(prefix, host, stores, runner).await;
        }
    };
}

/// Register the segment redrive law (FIG-3547): a redrive never
/// re-executes a recorded effect, an unrecorded one runs once more under the
/// same identity, and a segment whose engine lost its record ends
/// `Abandoned(SubstrateLost)` with no further effect. The fixture hands back a
/// guard, a prefix, the store set whose process registry the tier's engine
/// reads, and the tier's [`ConformanceTurnRunner`](crate::ConformanceTurnRunner),
/// which must run, crash and recover process segments.
#[macro_export]
macro_rules! segment_redrive_tests {
    ($(#[$attr:meta])* $fixture:block) => {
        $crate::segment_redrive_tests!(@law [$(#[$attr])*] $fixture;
            (segment_redrive_never_reexecutes_a_recorded_effect, "segment-redrive"));
    };
    (@law [$($attr:tt)*] $fixture:block; ($law:ident, $label:literal)) => {
        $($attr)*
        #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
        async fn $law() {
            let (_guard, prefix, stores, runner) = $fixture;
            $crate::registration_macro_support::$law(prefix, stores, runner).await;
        }
    };
}

/// Register the served-process-start laws (FIG-3779): an RLM cell that
/// called `agents.spawn` is cut at one point of its declared process start —
/// after the start was issued, or after its registration and before its
/// workflow send — and redriven under a drifted `agents.spawn` binding. The
/// recorded start is served and the turn completes. The fixture hands back a
/// guard, a prefix, the tier's effect host, the store set under test, its
/// [`ConformanceTurnRunner`](crate::ConformanceTurnRunner) — which must run
/// process segments — the RLM protocol plugin factories, and the
/// [`SubagentFactories`](crate::SubagentFactories) from the crates above this
/// one.
#[macro_export]
macro_rules! served_process_start_tests {
    ($(#[$attr:meta])* $fixture:block) => {
        $crate::served_process_start_tests!(@law [$(#[$attr])*] $fixture;
            (a_drifted_spawn_whose_start_was_issued_is_served, "served-process-start-issued"));
        $crate::served_process_start_tests!(@law [$(#[$attr])*] $fixture;
            (a_drifted_spawn_cut_before_its_send_is_served, "served-process-start-before-send"));
    };
    (@law [$($attr:tt)*] $fixture:block; ($law:ident, $label:literal)) => {
        $($attr)*
        #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
        async fn $law() {
            let (_guard, prefix, host, stores, runner, rlm, subagents) = $fixture;
            $crate::registration_macro_support::$law(prefix, host, stores, runner, rlm, subagents)
                .await;
        }
    };
}

/// Register one turn-runner law.
#[macro_export]
macro_rules! __turn_runner_register {
    ([$($attr:tt)*] $fixture:block; ($law:ident, $label:literal)) => {
        $($attr)*
        #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
        async fn $law() {
            let (_guard, prefix, host, stores, work, runner, verify) = $fixture;
            $crate::registration_macro_support::$law(prefix, host, stores, work, runner).await;
            verify(stringify!($law)).await;
        }
    };
}

#[macro_export]
macro_rules! session_command_replay_tests {
    ($fixture:block) => {
        $crate::turn_config_tests!(@law [] $fixture;
            (session_command_resubmission_after_advance_returns_first_receipt, "command-receipt-replay"));
        $crate::turn_config_tests!(@law [] $fixture;
            (settled_config_transaction_applies_once_after_advance, "command-config-once"));
        $crate::turn_config_tests!(@law [] $fixture;
            (stale_config_transaction_replay_returns_its_recorded_revision, "command-stale-outcome"));
        $crate::turn_config_tests!(@law [] $fixture;
            (settled_command_changed_content_is_a_typed_conflict, "command-content-conflict"));
    };
}
