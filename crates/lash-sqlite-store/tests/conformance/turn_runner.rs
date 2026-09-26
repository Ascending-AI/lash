use super::*;

/// The turn-driving laws' fixture: a fresh backend, its effect host and store set, a
/// native process-work substrate over its process registry, and a runner that
/// scopes each turn on the same host.
type SqliteTurnRunnerFixture = (
    TestEngineBackend,
    &'static str,
    Arc<dyn EffectHost>,
    Arc<dyn lash_core_execution::StoreSet>,
    Arc<dyn lash_core_execution::ProcessWorkSubstrate>,
    Arc<dyn lash_conformance::ConformanceTurnRunner>,
    fn(&'static str) -> std::future::Ready<()>,
);

async fn sqlite_turn_runner_fixture() -> SqliteTurnRunnerFixture {
    let backend = TestEngineBackend::open(SUBSTRATE).await;
    let effect_host = backend.effect_host() as Arc<dyn EffectHost>;
    let registry = backend.process_registry() as Arc<dyn ProcessRegistry>;
    let process_work = Arc::new(lash_core_execution::NativeProcessWork::for_registry(
        Arc::clone(&registry),
    )) as Arc<dyn lash_core_execution::ProcessWorkSubstrate>;
    let turn_runner = lash_conformance::HostTurnRunner::shared(Arc::clone(&effect_host));
    let law_backend = backend.as_stores();
    (
        backend,
        "sqlite-turn-runner",
        effect_host,
        law_backend,
        process_work,
        turn_runner,
        // The SQLite host owns no post-law assertion beyond the shared checks.
        |_law| std::future::ready(()),
    )
}

lash_conformance::turn_runner_tests!({ sqlite_turn_runner_fixture().await });

lash_conformance::tool_child_turn_cancel_tests!({ sqlite_turn_runner_fixture().await });

lash_conformance::admitted_head_redrive_tests!({
    let (backend, prefix, effect_host, stores, _process_work, turn_runner, _after_law) =
        sqlite_turn_runner_fixture().await;
    (backend, prefix, effect_host, stores, turn_runner)
});

lash_conformance::turn_config_tests!({
    let (backend, prefix, effect_host, stores, _process_work, turn_runner, _after_law) =
        sqlite_turn_runner_fixture().await;
    (backend, prefix, effect_host, stores, turn_runner)
});

// The in-process tier redelivers no drive: a root whose scope close failed
// after its evidence committed leaves no work for a later drive to admit, so
// the close's at-least-once retry is an engine's, and
// `root_scope_close_runs_after_terminal_evidence_at_least_once` runs on the
// Restate double only. In process, the process worker's recovery pass
// reconciles an unacknowledged close: it re-derives the root's missing
// scope-close row from the root's terminal evidence (FIG-3607).
lash_conformance::drive_admission_tests!(@laws [] {
    let (backend, prefix, effect_host, stores, _process_work, turn_runner, _after_law) =
        sqlite_turn_runner_fixture().await;
    (backend, prefix, effect_host, stores, turn_runner)
}; [
    (a_terminal_root_never_reparks, "s7b-0"),
    (a_diverged_root_parks_once_holds_claims_blocks_admission_and_completes_after_restore, "s7b-15"),
    (an_exhausted_root_parks_engine_retry_exhausted_via_reconcile_idempotently_with_no_evidence, "s7b-13"),
    (a_parked_roots_fence_stays_current_until_a_verb, "s7b-14"),

    (sends_behind_a_parked_root_commit_but_are_not_admitted, "s7b-8"),
    (redrive_under_a_restored_build_completes_once_and_clears_the_park, "s7b-9"),
    (a_stale_redrive_is_fenced_by_a_later_cancel, "s7b-10"),
    (root_scope_close_runs_after_terminal_evidence_at_least_once_never_for_parked, "s7b-11"),
    (cancel_fork_and_close_raise_the_drive_epoch_and_redrive_does_not, "s7b-12"),

    (cancel_of_a_parked_root_writes_cancelled_settles_its_input_and_drains_the_next, "s7b-1"),
    (fork_releases_the_old_owner_before_the_new_root_drives_in_original_order_on_a_fresh_journal, "s7b-2"),
    (verbs_are_park_id_cas, "s7b-3"),
    (redrive_under_the_same_build_reparks_the_same_park_with_attempts_plus_one, "s7b-4"),
    (cancel_or_fork_of_a_redriving_root_is_refused, "s7b-5"),
    (an_intent_survives_a_crash_at_every_gap_and_reconcile_completes_it, "s7b-6"),
    (engine_refusals_are_retained_and_listed, "s7b-7"),
    (a_root_parked_on_a_later_physical_turn_is_cleared_by_its_commit, "s7b-16"),
    (a_redrive_the_root_ran_past_is_never_applied_again, "s7b-17"),
    (a_stale_paused_listing_never_reparks_a_resumed_root, "s7b-18"),
    (a_parked_session_is_asked_to_drive_only_once_its_park_resolves, "s7b-19"),
    (one_authorized_drive_per_session, "drive-one-authorized"),
    (one_drive_claims_many_items, "drive-many-items"),
    (claim_identity_is_idempotent_within_ownership, "drive-claim-idempotent"),
    (replay_cannot_mint_ownership, "drive-replay-ownership"),
    (admission_precedes_first_effect, "drive-admission-first"),
    (reset_before_admission_admits_fresh, "drive-reset-admission"),
    (parked_root_blocks_admission, "drive-parked-root"),
    (fence_is_not_in_the_envelope_hash, "drive-fence-envelope"),
    (every_driver_turn_is_owned_by_its_root, "drive-owned-root"),
    (a_store_fault_at_the_root_claim_is_retried_not_recorded, "drive-claim-fault-retried"),
    (a_committed_root_answers_its_terminal_by_root, "drive-root-answered"),
    (a_host_id_naming_a_terminal_root_is_answered_not_rerun, "drive-root-adopted"),
    (a_root_whose_admission_a_successor_sealed_commits_nothing, "drive-root-superseded"),
    (a_queued_root_settled_without_a_commit_closes_after_its_evidence, "drive-root-settled-close"),
    (a_root_end_closes_its_turn_scope_in_the_process_registry, "drive-root-registry-close"),
]);
