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
// Restate double only. Reconciling an unacknowledged close in process needs
// the scope owner's own closed-scope record (FIG-3607 PR-2).
lash_conformance::drive_admission_tests!(@laws [] {
    let (backend, prefix, effect_host, stores, _process_work, turn_runner, _after_law) =
        sqlite_turn_runner_fixture().await;
    (backend, prefix, effect_host, stores, turn_runner)
}; [
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
]);
