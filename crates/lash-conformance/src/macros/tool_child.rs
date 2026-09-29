//! Registration macros for the tool-child and turn-runner laws (FIG-3397,
//! ADR 0099): tool calls running as effect-group children on every tier.
//! Split from `macros.rs` to keep each catalogue file inside the support-file
//! line budget.

/// Register the laws that drive a real turn through the tier's
/// [`ConformanceTurnRunner`](crate::ConformanceTurnRunner): the public
/// signal-intent wake, the turn-cancel laws for tool calls running as
/// effect-group children, and the presentation-divergence park law (FIG-3679).
///
/// The fixture hands back a guard, a session prefix, the tier's effect host,
/// the store set under test (whose session catalog and process registry the
/// law's runtime uses), the process-work substrate, the tier's turn runner and
/// a post-law verification handed the law's name. Restate runs each turn inside a live handler
/// (`#[ignore]`d, deferred to `effect-group-conformance-e2e`).
#[macro_export]
macro_rules! turn_runner_tests {
    ($(#[$attr:meta])* $fixture:block) => {
        $crate::__turn_runner_register!([$(#[$attr])*] $fixture;
            (public_signal_intent_wakes_parked_process, "public-signal-intent-wake"));
        $crate::__turn_runner_register!([$(#[$attr])*] $fixture;
            (an_after_step_stop_during_a_child_retry_sleep_finishes_the_iteration, "tool-child-after-step-retry-sleep"));
        $crate::__turn_runner_register!([$(#[$attr])*] $fixture;
            (a_follow_on_pending_child_waits_under_the_follow_on_turn_cancel_gate, "tool-child-follow-on-cancel-gate"));
        $crate::__turn_runner_register!([$(#[$attr])*] $fixture;
            (a_diverged_tool_presentation_parks_the_turn, "presentation-divergence-park"));
    };
}

/// Register the turn-cancel law for a tool child that ignores cancellation,
/// with [`turn_runner_tests!`]'s fixture. The in-process tiers own their
/// children's tasks, so dropping one is theirs to prove.
#[macro_export]
macro_rules! tool_child_turn_cancel_tests {
    ($(#[$attr:meta])* $fixture:block) => {
        $crate::__turn_runner_register!([$(#[$attr])*] $fixture;
            (cancel_dispositions_survive_group_child_teardown_and_redrive, "tool-child-turn-cancel-drops-child"));
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

/// Register the turn-config laws (FIG-3600 S6, D3 §5.2; FIG-3838, FIG-3842): a root
/// resolves its run spec against its session config once, as a recorded
/// step, and every replay of the root runs under that record. The fixture is the admitted-head one: a guard, a
/// prefix, the tier's effect host, the store set under test and its
/// [`ConformanceTurnRunner`](crate::ConformanceTurnRunner).
#[macro_export]
macro_rules! turn_config_tests {
    ($(#[$attr:meta])* $fixture:block) => {
        $crate::turn_config_tests!(@law [$(#[$attr])*] $fixture;
            (a_committed_root_redriven_after_a_model_change_refuses_its_stale_epoch, "turn-config-stale-redrive"));
        $crate::turn_config_tests!(@law [$(#[$attr])*] $fixture;
            (an_input_sent_after_a_config_command_runs_on_the_new_model, "turn-config-after-command"));
        $crate::turn_config_tests!(@law [$(#[$attr])*] $fixture;
            (one_config_resolution_per_root, "turn-config-one-resolution"));
        $crate::turn_config_tests!(@law [$(#[$attr])*] $fixture;
            (an_unbindable_route_retries_and_never_fails_the_turn, "turn-config-unbindable-retries"));
        $crate::turn_config_tests!(@law [$(#[$attr])*] $fixture;
            (a_bad_route_is_refused_at_send_with_nothing_enqueued, "turn-config-bad-route-send"));
        $crate::turn_config_tests!(@law [$(#[$attr])*] $fixture;
            (a_route_refused_at_apply_leaves_the_route_unchanged, "turn-config-refused-at-apply"));
        $crate::turn_config_tests!(@law [$(#[$attr])*] $fixture;
            (run_specs_split_roots_in_admission_order, "run-spec-selector"));
        $crate::turn_config_tests!(@law [$(#[$attr])*] $fixture;
            (the_default_spec_is_the_snapshot_after_the_command_drain, "run-spec-default"));
        $crate::turn_config_tests!(@law [$(#[$attr])*] $fixture;
            (a_root_resolves_its_spec_once_across_a_crash, "run-spec-once"));
        $crate::turn_config_tests!(@law [$(#[$attr])*] $fixture;
            (a_missing_definition_retries_unrecorded_until_it_is_deployed, "run-spec-missing"));
        $crate::turn_config_tests!(@law [$(#[$attr])*] $fixture;
            (a_batch_shares_one_spec_that_each_root_resolves_once, "run-spec-batch"));
        $crate::turn_config_tests!(@law [$(#[$attr])*] $fixture;
            (a_batch_keeps_its_turn_lane_place_behind_the_command_lane, "run-spec-batch-order"));
        $crate::turn_config_tests!(@law [$(#[$attr])*] $fixture;
            (a_recovered_follow_on_inherits_its_roots_recorded_run, "run-spec-follow-on-inherit"));
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

/// Register the session drive's admission laws (FIG-3600, ADR 0105 §2): a
/// drive admits and seals every root before its first effect, one admission
/// at a time holds the session, and a replay mints no ownership. The fixture
/// is the admitted-head one: a guard, a prefix, the tier's effect host, the
/// store set under test and its
/// [`ConformanceTurnRunner`](crate::ConformanceTurnRunner).
///
/// A tier that must quarantine a law registers the laws by name and marks the
/// quarantined ones:
/// `drive_admission_tests!(@laws [] { fixture }; [(law, "label"), #[ignore = "why"] (other, "label"), ...])`.
#[macro_export]
macro_rules! drive_admission_tests {
    ($(#[$attr:meta])* $fixture:block) => {
        $crate::drive_admission_tests!(@laws [$(#[$attr])*] $fixture; [
    (a_terminal_root_never_reparks, "s7b-0"),
    (a_diverged_root_parks_once_holds_its_admitted_rows_blocks_admission_and_completes_after_restore, "s7b-15"),
    (an_exhausted_root_parks_engine_retry_exhausted_via_reconcile_idempotently_with_no_evidence, "s7b-13"),
    (a_parked_roots_fence_stays_current_until_a_verb, "s7b-14"),

    (sends_behind_a_parked_root_commit_but_are_not_admitted, "s7b-8"),
    (redrive_under_a_restored_build_completes_once_and_clears_the_park, "s7b-9"),
    (a_stale_redrive_is_fenced_by_a_later_cancel, "s7b-10"),
    (root_scope_close_runs_after_terminal_evidence_at_least_once_never_for_parked, "s7b-11"),
    (a_root_crashed_at_its_report_handover_still_closes_its_scope, "s7b-11b"),
    (cancel_fork_and_close_raise_the_drive_epoch_and_redrive_does_not, "s7b-12"),

    (cancel_of_a_parked_root_writes_cancelled_settles_its_input_and_drains_the_next, "s7b-1"),
    (no_row_stays_bound_after_a_roots_verb_close_or_lost_end, "root-verb-unbinds"),
    (a_refused_root_ends_once_and_its_next_input_admits_a_new_root, "refused-root-end"),
    (an_obsolete_executor_never_ends_its_successors_root, "obsolete-executor"),
    (inconsistent_divergence_still_parks_on_a_lower_revision, "inconsistent-lower-revision"),
    (inconsistent_divergence_still_parks_on_another_leaf, "inconsistent-other-leaf"),
    (inconsistent_divergence_still_parks_on_another_checkpoint, "inconsistent-other-checkpoint"),
    (fork_releases_the_old_owner_before_the_new_root_drives_in_original_order_on_a_fresh_journal, "s7b-2"),
    (verbs_are_park_id_cas, "s7b-3"),
    (redrive_under_the_same_build_reparks_the_same_park_with_attempts_plus_one, "s7b-4"),
    (cancel_or_fork_of_a_redriving_root_is_refused, "s7b-5"),
    (an_intent_survives_a_crash_at_every_gap_and_reconcile_completes_it, "s7b-6"),
    (engine_refusals_are_retained_and_listed, "s7b-7"),
    (a_root_parked_on_a_later_physical_turn_is_cleared_by_its_commit, "s7b-16"),
    (a_redrive_the_root_ran_past_is_never_applied_again, "s7b-17"),
    (a_stale_paused_listing_never_reparks_a_resumed_root, "s7b-18"),
    (a_parked_session_is_asked_to_drive_only_through_its_ingress_obligation, "s7b-19"),
    (a_send_racing_an_unsettled_redrive_is_refused_until_the_redrive_settles, "l2-1"),
    (a_lost_redrive_ack_is_settled_by_reconcile_and_the_queued_send_is_admitted, "l2-2"),
    (a_failing_child_cancel_never_wedges_its_roots_cancel_or_fork, "s8c-1"),
    (a_delivery_whose_claim_was_retaken_never_settles_its_intent, "s8c-2"),
    (an_intent_whose_engine_half_keeps_failing_stalls_at_its_ceiling_and_unwedges_its_session, "s8c-3"),
    (a_refused_follow_on_drive_keeps_the_intents_obligation_due, "s8c-4"),
            (one_authorized_drive_per_session, "drive-one-authorized"),
            (one_unfinished_root_per_session, "root-one-unfinished"),
            (admission_delivers_every_row_it_binds, "root-admission-delivers"),
            (a_root_admission_is_idempotent_across_new_rows_and_fences, "root-admission-idempotent"),
            (one_drive_admits_many_items, "drive-many-items"),
            (replay_cannot_mint_ownership, "drive-replay-ownership"),
            (admission_precedes_first_effect, "drive-admission-first"),
            (reset_before_admission_admits_fresh, "drive-reset-admission"),
            (parked_root_blocks_admission, "drive-parked-root"),
            (fence_is_not_in_the_envelope_hash, "drive-fence-envelope"),
            (every_driver_turn_is_owned_by_its_root, "drive-owned-root"),
            (a_store_fault_at_the_root_admission_is_retried_not_recorded, "drive-admission-fault-retried"),
            (a_command_enqueued_after_an_input_roots_admission_waits_for_the_next_boundary, "drive-command-after-admission"),
            (a_root_admission_survives_a_worker_crash_without_widening, "drive-admission-commit-crash"),
            (a_committed_root_replays_its_recorded_repair, "drive-recorded-repair"),
            (a_committed_root_answers_its_terminal_by_root, "drive-root-answered"),
            (a_host_id_naming_a_terminal_root_is_answered_not_rerun, "drive-root-adopted"),
            (a_root_whose_admission_a_successor_sealed_commits_nothing, "drive-root-superseded"),
            (a_command_roots_redrive_replays_its_recorded_outcome, "drive-command-root-redrive"),
            (a_root_end_closes_its_turn_scope_in_the_process_registry, "drive-root-registry-close"),
            (a_joined_inputs_turn_scope_closes_with_its_admitting_root, "drive-joined-scope-close"),
            (an_idle_session_admits_its_turn_lane_in_enqueue_order_whatever_the_kind, "drive-idle-turn-lane-order"),
            (a_turn_never_takes_an_item_past_an_earlier_unconsumed_item_of_the_other_kind, "drive-turn-lane-contiguous"),
        ]);
    };
    (@laws $attrs:tt $fixture:block; [$($(#[$law_attr:meta])* ( $law:ident, $label:literal )),* $(,)?]) => {
        $(
            $crate::drive_admission_tests!(@law $attrs [$(#[$law_attr])*] $fixture; ($law, $label));
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

/// Register the session-close laws L-D1..L-D6 (FIG-3600 S7, FIG-3607 item
/// 7): a deletion's refusals come before its recorded `BeginSessionClose`
/// step, the close ends every open root `SessionDeleted` and raises the drive
/// epoch, its engine half is retained on failure, and its `CloseSession`
/// intent outlives the session as the tombstone its roots are answered from,
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
            (session_delete_closes_active_and_parked_roots_as_session_deleted, "session-close-roots"),
            (a_refused_deletion_closes_nothing, "session-close-refused"),
            (the_close_intent_is_idempotent_retained_on_failure_and_survives_deletion, "session-close-retained"),
            (a_root_commit_racing_a_close_is_refused_stale_fence, "session-close-fence"),
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

/// Register the segment re-drive law (FIG-3547): a re-drive never
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

/// Register the model-call drift park law (FIG-3587): a model call replays
/// from the journaled prompt, a recorded model call whose envelope drifted
/// parks its turn, and restoring the surface finishes it. The fixture hands
/// back a guard, a prefix, the tier's effect host, the store set under test,
/// its [`ConformanceTurnRunner`](crate::ConformanceTurnRunner) and the RLM
/// protocol plugin factories from the crates above this one.
#[macro_export]
macro_rules! model_call_drift_park_tests {
    ($(#[$attr:meta])* $fixture:block) => {
        $crate::model_call_drift_park_tests!(@law [$(#[$attr])*] $fixture;
            (model_call_drift_parks_then_completes_once_restored, "model-call-drift-park"));
        $crate::model_call_drift_park_tests!(@law [$(#[$attr])*] $fixture;
            (runtime_drive_cold_replay_ignores_live_input_and_hook_drift, "runtime-cold-drive-replay"));
    };
    (@law [$($attr:tt)*] $fixture:block; ($law:ident, $label:literal)) => {
        $($attr)*
        #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
        async fn $law() {
            let (_guard, prefix, host, stores, runner, protocol) = $fixture;
            $crate::registration_macro_support::$law(&prefix, host, stores, runner, protocol).await;
        }
    };
}

/// Register the cell binding-drift law (FIG-3587): a redriven RLM cell links
/// against its journaled binding set, completing from the journal when the
/// drifted tool's result was recorded and parking when it would reach the
/// tool live; and the tool-child drift law (FIG-3725): a group tool child —
/// a model-issued call, an aggregate's leaf — judges its own tool the same
/// way. The fixture hands back a guard, a prefix, the tier's effect
/// host, the store set under test, its
/// [`ConformanceTurnRunner`](crate::ConformanceTurnRunner) — which must read
/// its journal's replay keys and cut a turn at a
/// [`JournalCut`](crate::JournalCut) — and the RLM protocol plugin factories
/// from the crates above this one.
#[macro_export]
macro_rules! cell_binding_drift_tests {
    ($(#[$attr:meta])* $fixture:block) => {
        $crate::cell_binding_drift_tests!(@law [$(#[$attr])*] $fixture;
            (redriven_cell_links_against_its_journaled_binding_set, "cell-binding-drift"));
        $crate::cell_binding_drift_tests!(@law [$(#[$attr])*] $fixture;
            (a_group_tool_child_judges_its_own_drifted_tool, "tool-child-drift"));
    };
    (@law [$($attr:tt)*] $fixture:block; ($law:ident, $label:literal)) => {
        $($attr)*
        #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
        async fn $law() {
            let (_guard, prefix, host, stores, runner, rlm) = $fixture;
            $crate::registration_macro_support::$law(prefix, host, stores, runner, rlm).await;
        }
    };
}

/// Register the served-process-start laws (FIG-3779): an RLM cell that
/// called `agents.spawn` is cut at one point of its process start — after
/// the start was issued, before its frontier marker, after the marker and
/// before its registration, after its registration and before its workflow
/// send — and redriven under a drifted `agents.spawn` binding. A recorded
/// start is served and the turn completes; one needed live parks with no
/// process started. The fixture hands back a guard, a prefix, the tier's
/// effect host, the store set under test, its
/// [`ConformanceTurnRunner`](crate::ConformanceTurnRunner) — which must read
/// its journal's replay keys, cut a turn at a
/// [`JournalCut`](crate::JournalCut) and run process segments — the RLM
/// protocol plugin factories, and the [`SubagentFactories`](crate::SubagentFactories)
/// from the crates above this one.
#[macro_export]
macro_rules! served_process_start_tests {
    ($(#[$attr:meta])* $fixture:block) => {
        $crate::served_process_start_tests!(@law [$(#[$attr])*] $fixture;
            (a_drifted_spawn_whose_start_was_issued_is_served, "served-process-start-issued"));
        $crate::served_process_start_tests!(@law [$(#[$attr])*] $fixture;
            (a_drifted_spawn_cut_before_its_start_marker_parks, "served-process-start-before-marker"));
        $crate::served_process_start_tests!(@law [$(#[$attr])*] $fixture;
            (a_drifted_spawn_cut_before_its_registration_parks, "served-process-start-before-registration"));
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

/// Register the batch crash-redrive law (FIG-4064): a turn crashed while its
/// batch holds one settled and one in-flight member recovers without running
/// the settled member again.
///
/// The fixture hands back what [`tool_batch_parallelism_tests!`] takes: a
/// guard, a session prefix, the tier's effect host, its store set, the
/// product producers that spell a batch on the tier, and its
/// [`ConformanceTurnRunner`](crate::ConformanceTurnRunner).
#[macro_export]
macro_rules! tool_batch_crash_redrive_tests {
    ($(#[$attr:meta])* $fixture:block) => {
        $(#[$attr])*
        #[tokio::test(flavor = "multi_thread", worker_threads = 8)]
        async fn a_settled_batch_member_runs_once_across_a_turn_crash() {
            let (_guard, prefix, host, stores, producers, runner) = $fixture;
            assert!(
                !producers.is_empty(),
                "a tier registers at least one product producer, or the law runs on nothing"
            );
            for producer in producers {
                $crate::registration_macro_support::a_settled_batch_member_runs_once_across_a_turn_crash(
                    prefix,
                    std::sync::Arc::clone(&host),
                    std::sync::Arc::clone(&stores),
                    std::sync::Arc::clone(&runner),
                    producer,
                )
                .await;
            }
        }
    };
}

/// Register the handler-level tool-child invocation laws (FIG-2266, ADR 0099).
///
/// The fixture hands back a guard, a session prefix and a
/// [`ToolChildLawFixture`]: a world factory over one substrate (a law calls it
/// from a runtime it is about to destroy), a process-registry factory, and the
/// completion routing the tier records for a deferrable child. Restate
/// registers through the live e2e recipe (`conformance_and_poison.rs`) and its
/// world has no Lash-owned drain — Restate redrives the child invocation
/// itself — so the laws take their open-time shape there.
#[macro_export]
macro_rules! tool_child_invocation_tests {
    ($fixture:block) => {
        $crate::tool_child_invocation_tests!(@catalogue [] $fixture);
    };
    ($(#[$attr:meta])+ $fixture:block) => {
        $crate::tool_child_invocation_tests!(@catalogue [$(#[$attr])*] $fixture);
    };
    (@catalogue [$($attr:tt)*] $fixture:block) => {
        $crate::tool_child_invocation_tests!(@expand [$($attr)*] $fixture; [
            (
                declared_intent_replay_preserves_manifest_order_and_capabilities,
                "tool-child-invocation-driver"
            ),
            (
                an_unregistered_opener_leaves_the_child_accepted,
                "tool-child-unregistered-opener"
            ),
            (
                a_foreign_opener_cannot_drive_another_openers_child,
                "tool-child-foreign-opener"
            ),
            (
                another_process_is_not_the_recorded_opener,
                "tool-child-process-incarnation"
            ),
            (
                a_committed_childs_final_is_protected_and_its_drain_is_finished,
                "tool-child-commit-boundary-crash"
            ),
            (
                drains_are_admitted_in_recorded_commit_order,
                "tool-child-commit-order"
            ),
            (
                a_drain_held_at_the_barrier_parks_under_a_frozen_dispatch_clock,
                "tool-child-commit-order-frozen-clock"
            ),
            (
                a_cancel_decided_before_a_sink_is_refused_at_the_sink,
                "tool-child-admission-fence"
            ),
            (
                a_late_completion_after_a_cancel_decision_is_refused,
                "tool-child-late-completion-refused"
            ),
            (
                a_deferred_childs_commit_point_is_its_resolution,
                "tool-child-deferred-commit-point"
            ),
            (
                group_accounting_conserves_each_incorporated_rank,
                "tool-child-group-prefix-incorporation"
            ),
            (
                two_presentation_steps_compose_deterministically_on_first_run_and_replay,
                "tool-child-presentation-composition"
            ),
            (
                a_changed_presentation_environment_on_replay_does_not_change_the_recorded_presentation,
                "tool-child-presentation-recorded-env"
            ),
            (
                the_oracle_and_a_bounded_step_coexist,
                "tool-child-presentation-coexistence"
            ),
            (
                a_retained_full_output_is_a_durable_artifact_not_a_path,
                "tool-child-presentation-artifact"
            ),
            (
                timer_and_durable_wait_children_are_admitted_beside_a_tool_child,
                "tool-child-timer-and-wait-siblings"
            ),
        ]);
    };
    (@expand $attrs:tt $fixture:block; [$(( $law:ident, $label:literal )),* $(,)?]) => {
        $(
            $crate::__tool_child_invocation_register!($attrs $fixture; ($law, $label));
        )*
    };
}

/// Register the live-fault laws (FIG-3575): a store fault while a tool child
/// resolves its environment is never its recorded outcome.
///
/// The fixture is [`tool_child_invocation_tests!`]'s. Registered by an
/// engine that re-runs a child whose run hit a live fault.
#[macro_export]
macro_rules! tool_child_live_fault_tests {
    ($(#[$attr:meta])* $fixture:block) => {
        $crate::__tool_child_invocation_register!([$(#[$attr])*] $fixture;
            (a_process_env_store_fault_is_never_the_childs_recorded_outcome,
             "tool-child-env-store-fault"));
    };
}

/// Register one shared tool-child invocation law.
#[macro_export]
macro_rules! __tool_child_invocation_register {
    ([$($attr:tt)*] $fixture:block; ($law:ident, $label:literal)) => {
        $($attr)*
        #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
        async fn $law() {
            let (_guard, prefix, fixture) = $fixture;
            $crate::registration_macro_support::$law(&fixture, prefix).await;
        }
    };
}

/// Register the consumer-driven group laws (FIG-3397, ADR 0099 §5, §6, §7,
/// §10, §13): an `All` group of tool children answers every admission shape
/// with its own reply, keyed by input index, and a settlement order its
/// preparation prefix leads; and the opener's end finishes and incorporates
/// every group its aggregates formed — one a cancel handed back, one a failed
/// end left `closing`.
///
/// The fixture hands back a guard, a session prefix and a
/// [`ToolChildLawFixture`], the same shape `tool_child_invocation_tests!`
/// takes — the law needs a world factory over the tier's substrate and a
/// process-registry factory, nothing more. Restate is deliberately absent:
/// its deployment host executes no effects, so there is no batch consumer to
/// run.
#[macro_export]
macro_rules! tool_batch_group_tests {
    ($(#[$attr:meta])* $fixture:block) => {
        $crate::__tool_child_invocation_register!([$(#[$attr])*] $fixture;
            (an_all_group_of_tool_children_yields_the_batch_replies, "tool-batch-group-replies"));
        $crate::__tool_child_invocation_register!([$(#[$attr])*] $fixture;
            (a_cancelled_aggregates_committed_loser_is_incorporated_by_its_openers_end,
             "opener-end-cancelled-aggregate"));
        $crate::__tool_child_invocation_register!([$(#[$attr])*] $fixture;
            (a_retried_openers_end_finishes_the_closing_group_its_first_end_left,
             "opener-end-retried-end"));
    };
}
