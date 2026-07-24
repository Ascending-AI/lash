use super::cases::RUNTIME_SCENARIO_COVERAGE;
use std::collections::BTreeSet;

const CONFIDENCE_GATE_SH: &str = include_str!("../../../../../../scripts/confidence-gate.sh");

#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd)]
enum DurableFaultKind {
    CrashReopen,
    DuplicateInputs,
    ProviderFailureRetry,
    Cancellation,
    LeaseLoss,
    TriggerDeliveryRecovery,
    BackendPermutation,
}

#[derive(Clone, Copy, Debug)]
struct CargoTestEvidence {
    package: &'static str,
    test_target: Option<&'static str>,
    filter: &'static str,
    required_env: Option<&'static str>,
}

#[derive(Clone, Copy, Debug)]
enum FaultEvidence {
    RuntimeScenario {
        test_name: &'static str,
    },
    CargoTest(CargoTestEvidence),
    #[allow(dead_code)]
    Blocked {
        rationale: &'static str,
    },
}

#[derive(Clone, Copy, Debug)]
struct DurableFaultMatrixRow {
    id: &'static str,
    kind: DurableFaultKind,
    contract: &'static str,
    evidence: FaultEvidence,
}

const DURABLE_FAULT_MATRIX: &[DurableFaultMatrixRow] = &[
    DurableFaultMatrixRow {
        id: "crash-reopen-runtime-rebuild",
        kind: DurableFaultKind::CrashReopen,
        contract: "Cold runtime/session rebuild and worker recovery preserve durable graph and process state.",
        evidence: FaultEvidence::CargoTest(CargoTestEvidence {
            package: "lash-runtime",
            test_target: None,
            filter: "runtime_rebuild_and_worker_recovery_with_durable_stores",
            required_env: None,
        }),
    },
    DurableFaultMatrixRow {
        id: "duplicate-turn-input-source-key",
        kind: DurableFaultKind::DuplicateInputs,
        contract: "Duplicate source-key turn input returns the original pending input and payload.",
        evidence: FaultEvidence::RuntimeScenario {
            test_name: "runtime_scenario_observation_replay_keeps_original_turn_input",
        },
    },
    DurableFaultMatrixRow {
        id: "provider-retry-exhaustion",
        kind: DurableFaultKind::ProviderFailureRetry,
        contract: "Retryable LLM provider failures are retried deterministically and fail the turn only after exhaustion.",
        evidence: FaultEvidence::CargoTest(CargoTestEvidence {
            package: "lash-core",
            test_target: None,
            filter: "retryable_llm_failures_exhaust_and_fail_turn",
            required_env: None,
        }),
    },
    DurableFaultMatrixRow {
        id: "protocol-provider-failure",
        kind: DurableFaultKind::ProviderFailureRetry,
        contract: "Protocol-level provider failure stops without manufacturing a checkpoint.",
        evidence: FaultEvidence::CargoTest(CargoTestEvidence {
            package: "lash-protocol-standard",
            test_target: Some("protocol_scenarios"),
            filter: "standard_protocol_scenario_provider_error_stops_without_checkpoint",
            required_env: None,
        }),
    },
    DurableFaultMatrixRow {
        id: "checkpoint-redrive-cancel",
        kind: DurableFaultKind::Cancellation,
        contract: "Active-turn and next-turn cancellation prevents later redrive after checkpoint deferral.",
        evidence: FaultEvidence::RuntimeScenario {
            test_name: "runtime_scenario_defers_checkpoint_turn_input_and_respects_cancel",
        },
    },
    DurableFaultMatrixRow {
        id: "lease-release-advisory",
        kind: DurableFaultKind::LeaseLoss,
        contract: "A released advisory lease does not block a current-head commit, while the head-revision CAS still rejects stale follow-up state.",
        evidence: FaultEvidence::RuntimeScenario {
            test_name: "runtime_scenario_commits_after_advisory_session_lease_release",
        },
    },
    DurableFaultMatrixRow {
        id: "dead-lease-reclaim",
        kind: DurableFaultKind::LeaseLoss,
        contract: "Dead local lease holders are reclaimable while stale observed-holder reclaim cannot clear a newer fence.",
        evidence: FaultEvidence::RuntimeScenario {
            test_name: "runtime_scenario_reclaims_dead_session_lease_and_rejects_stale_observation",
        },
    },
    DurableFaultMatrixRow {
        id: "queued-work-claim-generation-supersession",
        kind: DurableFaultKind::LeaseLoss,
        contract: "A queued-work claim superseded by a new session-lease generation is rejected at commit.",
        evidence: FaultEvidence::CargoTest(CargoTestEvidence {
            package: "lash-core",
            test_target: None,
            filter: "queued_work_claims_supersede_across_session_lease_generations",
            required_env: None,
        }),
    },
    DurableFaultMatrixRow {
        id: "deferred-next-turn-generation-reclaim",
        kind: DurableFaultKind::LeaseLoss,
        contract: "A failed turn's DeferredNextTurn claim is reclaimed by idle retry under a new session-lease generation while its stale completion is rejected.",
        evidence: FaultEvidence::CargoTest(CargoTestEvidence {
            package: "lash-core",
            test_target: None,
            filter: "turn_input_claims_supersede_across_session_lease_generations",
            required_env: None,
        }),
    },
    DurableFaultMatrixRow {
        id: "same-generation-claim-bounded-scan",
        kind: DurableFaultKind::LeaseLoss,
        contract: "More than 32 same-generation claims cannot hide a later unclaimed queued-work, session-command, or turn-input row from bounded scans.",
        evidence: FaultEvidence::CargoTest(CargoTestEvidence {
            package: "lash-core",
            test_target: None,
            filter: "same_generation_claim_scans_reach_rows_beyond_the_scan_surplus",
            required_env: None,
        }),
    },
    DurableFaultMatrixRow {
        id: "trigger-delivery-reserve-start-crash-window",
        kind: DurableFaultKind::TriggerDeliveryRecovery,
        contract: "A trigger delivery reserved before a crash but missing its process row is reconciled into exactly one deterministic process start.",
        evidence: FaultEvidence::CargoTest(CargoTestEvidence {
            package: "lash-core",
            test_target: None,
            filter: "sweep_reconciles_reserved_trigger_delivery_without_process",
            required_env: None,
        }),
    },
    DurableFaultMatrixRow {
        id: "trigger-delivery-prune-orphan-retention",
        kind: DurableFaultKind::TriggerDeliveryRecovery,
        contract: "Retention prunes trigger delivery rows with their terminal process rows so recovery does not resurrect completed trigger work.",
        evidence: FaultEvidence::CargoTest(CargoTestEvidence {
            package: "lash-core",
            test_target: None,
            filter: "sweep_does_not_reconcile_trigger_delivery_pruned_with_terminal_process",
            required_env: None,
        }),
    },
    DurableFaultMatrixRow {
        id: "sqlite-backend-conformance",
        kind: DurableFaultKind::BackendPermutation,
        contract: "Sqlite runs the backend conformance contract, including reopen, source-key, claim, lease, process change-feed ordering, process_change_feed_never_misses_concurrent_terminal_writers, drainage, watermark-bounded prune, and effect replay cases.",
        evidence: FaultEvidence::CargoTest(CargoTestEvidence {
            package: "lash-sqlite-store",
            test_target: Some("conformance"),
            filter: "conformance",
            required_env: None,
        }),
    },
    DurableFaultMatrixRow {
        id: "postgres-backend-conformance",
        kind: DurableFaultKind::BackendPermutation,
        contract: "When the env-gated Postgres lane is configured, Postgres runs the same backend conformance contract, including process_change_feed_never_misses_concurrent_terminal_writers, drainage, and watermark-bounded prune, against a durable service backend.",
        evidence: FaultEvidence::Blocked {
            rationale: "Fast confidence cannot require an external Postgres service; Postgres conformance remains blocked in fast and runs only when LASH_POSTGRES_DATABASE_URL or Docker is available.",
        },
    },
];

#[test]
fn durable_fault_matrix_covers_required_fault_classes() {
    let observed = DURABLE_FAULT_MATRIX
        .iter()
        .map(|row| row.kind)
        .collect::<BTreeSet<_>>();
    let required = BTreeSet::from([
        DurableFaultKind::CrashReopen,
        DurableFaultKind::DuplicateInputs,
        DurableFaultKind::ProviderFailureRetry,
        DurableFaultKind::Cancellation,
        DurableFaultKind::LeaseLoss,
        DurableFaultKind::TriggerDeliveryRecovery,
        DurableFaultKind::BackendPermutation,
    ]);
    assert_eq!(observed, required);
}

#[test]
fn durable_fault_matrix_rows_have_executable_or_blocked_evidence() {
    let runtime_scenarios = RUNTIME_SCENARIO_COVERAGE
        .iter()
        .map(|coverage| coverage.test_name)
        .collect::<BTreeSet<_>>();
    let mut ids = BTreeSet::new();

    for row in DURABLE_FAULT_MATRIX {
        assert!(ids.insert(row.id), "duplicate durable fault row {}", row.id);
        assert!(
            !row.contract.trim().is_empty(),
            "{} has no contract",
            row.id
        );
        match row.evidence {
            FaultEvidence::RuntimeScenario { test_name } => {
                assert!(
                    runtime_scenarios.contains(test_name),
                    "{} points at unknown Runtime Scenario `{}`",
                    row.id,
                    test_name
                );
            }
            FaultEvidence::CargoTest(evidence) => {
                assert!(
                    !evidence.package.trim().is_empty(),
                    "{} has an empty package",
                    row.id
                );
                assert!(
                    !evidence.filter.trim().is_empty(),
                    "{} has an empty test filter",
                    row.id
                );
                if let Some(test_target) = evidence.test_target {
                    assert!(
                        !test_target.trim().is_empty(),
                        "{} has an empty test target",
                        row.id
                    );
                }
                if let Some(required_env) = evidence.required_env {
                    assert!(
                        !required_env.trim().is_empty(),
                        "{} has an empty required env var",
                        row.id
                    );
                }
            }
            FaultEvidence::Blocked { rationale } => {
                assert!(
                    rationale.split_whitespace().count() >= 5,
                    "{} blocked row needs a concrete rationale",
                    row.id
                );
            }
        }
    }
}

#[test]
fn durable_fault_matrix_fast_gate_executes_all_nonblocked_evidence() {
    assert!(
        CONFIDENCE_GATE_SH.contains("run_cargo_tests -p lash-core --locked runtime_scenario"),
        "fast gate must execute RuntimeScenario evidence rows"
    );

    for row in DURABLE_FAULT_MATRIX {
        match row.evidence {
            FaultEvidence::RuntimeScenario { .. } | FaultEvidence::Blocked { .. } => {}
            FaultEvidence::CargoTest(_) => {
                let marker = format!("durable-fault-matrix: {}", row.id);
                assert!(
                    CONFIDENCE_GATE_SH.contains(&marker),
                    "{} is non-blocked CargoTest evidence but is not executed by scripts/confidence-gate.sh fast",
                    row.id
                );
            }
        }
    }
}
