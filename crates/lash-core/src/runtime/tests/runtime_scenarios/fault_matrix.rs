use super::cases::RUNTIME_SCENARIO_COVERAGE;
use std::collections::BTreeSet;

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
            package: "lash-internal-core",
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
            package: "lash-internal-protocol-standard",
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
        id: "stale-lease-ttl",
        kind: DurableFaultKind::LeaseLoss,
        contract: "An unexpired stale lease stays busy until TTL, then a successor advances the fence.",
        evidence: FaultEvidence::RuntimeScenario {
            test_name: "runtime_scenario_waits_for_stale_session_lease_ttl",
        },
    },
    DurableFaultMatrixRow {
        id: "queued-work-claim-generation-supersession",
        kind: DurableFaultKind::LeaseLoss,
        contract: "After a successor generation re-claims queued work, the predecessor claim is rejected at commit without mutation.",
        evidence: FaultEvidence::CargoTest(CargoTestEvidence {
            package: "lash-internal-core",
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
            package: "lash-internal-core",
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
            package: "lash-internal-core",
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
            package: "lash-internal-core",
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
            package: "lash-internal-core",
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
            package: "lash-internal-sqlite-store",
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
    let scenario_commands = run_fast_gate_with_fake_cargo("scenario-harnesses");
    assert!(
        scenario_commands.iter().any(|command| {
            command
                == &[
                    "test",
                    "-p",
                    "lash-internal-core",
                    "--locked",
                    "runtime_scenario",
                ]
        }),
        "fast gate must execute RuntimeScenario evidence rows"
    );

    let fault_matrix_commands = run_fast_gate_with_fake_cargo("fault-matrix");
    for row in DURABLE_FAULT_MATRIX {
        match row.evidence {
            FaultEvidence::RuntimeScenario { .. } | FaultEvidence::Blocked { .. } => {}
            FaultEvidence::CargoTest(evidence) => {
                let command = fault_matrix_commands
                    .iter()
                    .find(|command| command_executes_evidence(command, evidence));
                assert!(
                    command.is_some(),
                    "{} is non-blocked CargoTest evidence but is not executed by scripts/confidence-gate.sh fast:fault-matrix; observed commands: {fault_matrix_commands:?}",
                    row.id
                );
            }
        }
    }
}

const REAL_CARGO_FILTER_CHUNKS: usize = 5;
// Raising the chunk count must add a matching test and bump this pin.
const _: () = assert!(REAL_CARGO_FILTER_CHUNKS == 5);

#[test]
fn durable_fault_matrix_real_cargo_filters_chunk_0() {
    assert_real_cargo_filter_chunk_selects_tests(0);
}

#[test]
fn durable_fault_matrix_real_cargo_filters_chunk_1() {
    assert_real_cargo_filter_chunk_selects_tests(1);
}

#[test]
fn durable_fault_matrix_real_cargo_filters_chunk_2() {
    assert_real_cargo_filter_chunk_selects_tests(2);
}

#[test]
fn durable_fault_matrix_real_cargo_filters_chunk_3() {
    assert_real_cargo_filter_chunk_selects_tests(3);
}

#[test]
fn durable_fault_matrix_real_cargo_filters_chunk_4() {
    assert_real_cargo_filter_chunk_selects_tests(4);
}

fn assert_real_cargo_filter_chunk_selects_tests(chunk_index: usize) {
    assert!(chunk_index < REAL_CARGO_FILTER_CHUNKS);
    let fault_matrix_commands = run_fast_gate_with_fake_cargo("fault-matrix");
    let mut cargo_evidence_index = 0;

    for row in DURABLE_FAULT_MATRIX {
        let FaultEvidence::CargoTest(evidence) = row.evidence else {
            continue;
        };
        let row_chunk = cargo_evidence_index % REAL_CARGO_FILTER_CHUNKS;
        cargo_evidence_index += 1;
        if row_chunk != chunk_index {
            continue;
        }

        let command = fault_matrix_commands
            .iter()
            .find(|command| command_executes_evidence(command, evidence))
            .unwrap_or_else(|| {
                panic!(
                    "{} is non-blocked CargoTest evidence but is not executed by scripts/confidence-gate.sh fast:fault-matrix; observed commands: {fault_matrix_commands:?}",
                    row.id
                )
            });
        assert_real_cargo_filter_selects_tests(command, row.id);
    }
}

fn assert_real_cargo_filter_selects_tests(command: &[String], row_id: &str) {
    let repo_root = repository_root();
    let output =
        std::process::Command::new(std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into()))
            .args(command)
            .args(["--", "--list"])
            .current_dir(repo_root)
            .output()
            .unwrap_or_else(|err| panic!("execute real cargo list probe for {row_id}: {err}"));
    assert!(
        output.status.success(),
        "real cargo list probe failed for {row_id}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let selected = String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter(|line| line.ends_with(": test"))
        .count();
    assert!(
        selected > 0,
        "confidence-gate command for {row_id} selected zero tests under real cargo: {command:?}"
    );
}

fn command_executes_evidence(command: &[String], evidence: CargoTestEvidence) -> bool {
    if command.first().map(String::as_str) != Some("test")
        || !command
            .windows(2)
            .any(|pair| pair[0] == "-p" && pair[1] == evidence.package)
        || !command.iter().any(|arg| arg == evidence.filter)
    {
        return false;
    }
    evidence.test_target.is_none_or(|target| {
        command
            .windows(2)
            .any(|pair| pair[0] == "--test" && pair[1] == target)
    })
}

fn run_fast_gate_with_fake_cargo(shard: &str) -> Vec<Vec<String>> {
    use std::os::unix::fs::PermissionsExt;

    let temp = tempfile::tempdir().expect("confidence-gate probe tempdir");
    let cargo_dir = temp.path().join(".cargo/bin");
    std::fs::create_dir_all(&cargo_dir).expect("create fake cargo directory");
    let cargo_path = cargo_dir.join("cargo");
    std::fs::write(
        &cargo_path,
        r#"#!/usr/bin/env bash
if [ "${1:-}" = "nextest" ] && [ "${2:-}" = "--version" ]; then
  exit 1
fi
{
  printf 'BEGIN\n'
  for arg in "$@"; do
    printf '%s\n' "$arg"
  done
  printf 'END\n'
} >> "$LASH_FAKE_CARGO_LOG"
"#,
    )
    .expect("write fake cargo");
    let mut permissions = std::fs::metadata(&cargo_path)
        .expect("stat fake cargo")
        .permissions();
    permissions.set_mode(0o755);
    std::fs::set_permissions(&cargo_path, permissions).expect("make fake cargo executable");
    let log_path = temp.path().join("cargo.log");
    let out_dir = temp.path().join("confidence");
    let repo_root = repository_root();

    let output = std::process::Command::new("bash")
        .arg(repo_root.join("scripts/confidence-gate.sh"))
        .arg(format!("fast:{shard}"))
        .current_dir(repo_root)
        .env("HOME", temp.path())
        .env("LASH_FAKE_CARGO_LOG", &log_path)
        .env("LASH_CONFIDENCE_OUT_DIR", &out_dir)
        .output()
        .expect("execute confidence gate with fake cargo");
    assert!(
        output.status.success(),
        "confidence gate routing probe failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let log = std::fs::read_to_string(log_path).expect("read fake cargo command log");
    let mut commands = Vec::new();
    let mut current = None;
    for line in log.lines() {
        match line {
            "BEGIN" => current = Some(Vec::new()),
            "END" => commands.push(current.take().expect("command begin before end")),
            arg => current
                .as_mut()
                .expect("command argument inside begin/end")
                .push(arg.to_string()),
        }
    }
    assert!(current.is_none(), "unterminated fake cargo command log");
    commands
}

fn repository_root() -> &'static std::path::Path {
    std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(2)
        .expect("lash-core has repository root two ancestors above")
}
