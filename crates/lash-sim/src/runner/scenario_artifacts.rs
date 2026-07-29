use super::*;

pub(super) fn scenario_contract_manifests() -> Vec<ScenarioContractManifest> {
    vec![
        scenario_contract_manifest(RUNTIME_SCENARIO_CONTRACTS),
        scenario_contract_manifest(STANDARD_PROTOCOL_SCENARIO_CONTRACTS),
        scenario_contract_manifest(RLM_PROTOCOL_SCENARIO_CONTRACTS),
        scenario_contract_manifest(AGENT_SCENARIO_CONTRACTS),
    ]
}

fn scenario_contract_manifest(
    contracts: &'static [ScenarioContractSpec],
) -> ScenarioContractManifest {
    let first = contracts
        .first()
        .expect("scenario contract manifest must not be empty");
    ScenarioContractManifest {
        suite: first.suite,
        oracle_id: first.oracle_id,
        contract_count: contracts.len(),
        required_sim_evidence: first.required_sim_evidence.to_vec(),
        contracts: contracts.to_vec(),
    }
}

fn all_scenario_contracts() -> impl Iterator<Item = &'static ScenarioContractSpec> {
    [
        RUNTIME_SCENARIO_CONTRACTS,
        STANDARD_PROTOCOL_SCENARIO_CONTRACTS,
        RLM_PROTOCOL_SCENARIO_CONTRACTS,
        AGENT_SCENARIO_CONTRACTS,
    ]
    .into_iter()
    .flat_map(|contracts| contracts.iter())
}

fn scenario_contract_oracle_id(contract: &ScenarioContractSpec) -> String {
    format!("{}:{}", contract.oracle_id, contract.test_name)
}

fn scenario_contract_backing_oracle_id(contract: &ScenarioContractSpec) -> String {
    scenario_contract_oracle_id(contract)
}

fn scenario_contract_classification(_contract: &ScenarioContractSpec) -> &'static str {
    "per_contract_oracle"
}

pub(super) fn write_scenario_contract_slices(
    artifact_root: &Path,
    event_lines: &[TraceEventLine],
    oracle_verdicts: &[OracleVerdict],
) -> Result<Vec<ScenarioContractSliceManifest>, FixedScriptRunnerError> {
    let slice_root = artifact_root.join(GENERATED_SIM_SCENARIO_SLICES);
    std::fs::create_dir_all(&slice_root)?;
    let mut manifests = Vec::new();
    for contract in all_scenario_contracts() {
        let oracle_id = scenario_contract_backing_oracle_id(contract);
        let classification = scenario_contract_classification(contract);
        let verdicts = oracle_verdicts
            .iter()
            .filter(|verdict| verdict.oracle_id == oracle_id)
            .cloned()
            .collect::<Vec<_>>();
        if verdicts.is_empty() {
            return Err(FixedScriptRunnerError::Assertion(format!(
                "scenario contract `{}` had no generated backing oracle verdict ({oracle_id})",
                contract.test_name
            )));
        }
        let selected_evidence = select_scenario_contract_evidence(contract, event_lines)?;
        let mut selected_keys = BTreeSet::new();
        let mut events = Vec::new();
        for evidence in &selected_evidence {
            if selected_keys.insert((evidence.trace_alias.clone(), evidence.boundary_id.clone())) {
                let event_line = event_lines
                    .iter()
                    .find(|line| {
                        line.trace_alias == evidence.trace_alias
                            && line.event.boundary_id == evidence.boundary_id
                    })
                    .ok_or_else(|| {
                        FixedScriptRunnerError::Assertion(format!(
                            "scenario contract `{}` selected missing event `{}`",
                            contract.test_name, evidence.boundary_id
                        ))
                    })?;
                events.push(event_line.clone());
            }
        }
        let generated_shape = scenario_generated_shape(contract, &selected_evidence, &events)?;
        let suite_dir = slice_root.join(contract.suite);
        std::fs::create_dir_all(&suite_dir)?;
        let artifact_path = suite_dir.join(format!("{}.json", contract.test_name));
        let artifact = ScenarioContractSliceArtifact {
            schema: "lash.sim.scenario-contract-slice.v1",
            contract: *contract,
            oracle_id: oracle_id.clone(),
            classification,
            semantic_oracle: contract.semantic_oracle,
            generated_shape: generated_shape.clone(),
            selected_evidence: selected_evidence.clone(),
            events,
            verdicts,
        };
        std::fs::write(&artifact_path, serde_json::to_vec_pretty(&artifact)?)?;
        manifests.push(ScenarioContractSliceManifest {
            schema: "lash.sim.scenario-contract-slice-manifest.v1",
            suite: contract.suite,
            test_name: contract.test_name,
            semantic_oracle: contract.semantic_oracle,
            oracle_id,
            classification,
            status: "generated_trace_slice_written",
            artifact_path: relative_path(artifact_root, &artifact_path),
            generated_shape,
            selected_event_count: artifact.selected_evidence.len(),
            selected_evidence,
        });
    }
    Ok(manifests)
}

pub(super) fn write_scenario_contract_packages(
    artifact_root: &Path,
    event_lines: &[TraceEventLine],
    oracle_verdicts: &[OracleVerdict],
    replay_reports: &[GeneratedReplayArtifact],
) -> Result<Vec<ScenarioContractPackageManifest>, FixedScriptRunnerError> {
    let package_root = artifact_root.join(GENERATED_SIM_SCENARIO_PACKAGES);
    std::fs::create_dir_all(&package_root)?;
    let replay_lookup = replay_artifact_lookup(replay_reports);
    let mut manifests = Vec::new();
    let mut backing_oracle_claims = BTreeMap::new();
    let mut package_fact_graph_claims = BTreeMap::new();
    for contract in all_scenario_contracts() {
        let oracle_id = scenario_contract_backing_oracle_id(contract);
        let classification = scenario_contract_classification(contract);
        if let Some(previous) = backing_oracle_claims.insert(oracle_id.clone(), contract.test_name)
        {
            return Err(FixedScriptRunnerError::Assertion(format!(
                "scenario contracts `{previous}` and `{}` share backing oracle `{oracle_id}` despite claiming per-contract generated semantics",
                contract.test_name
            )));
        }
        let verdicts = oracle_verdicts
            .iter()
            .filter(|verdict| verdict.oracle_id == oracle_id)
            .cloned()
            .collect::<Vec<_>>();
        if verdicts.is_empty() {
            return Err(FixedScriptRunnerError::Assertion(format!(
                "scenario contract `{}` had no generated backing oracle verdict for package ({oracle_id})",
                contract.test_name
            )));
        }
        let selected_evidence = select_scenario_contract_evidence(contract, event_lines)?;
        let events = selected_events(contract, event_lines, &selected_evidence)?;
        let generated_shape = scenario_generated_shape(contract, &selected_evidence, &events)?;
        assert_distinct_package_fact_graph(
            contract,
            &generated_shape,
            &mut package_fact_graph_claims,
        )?;
        let positive =
            scenario_positive_evidence(contract, &selected_evidence, &verdicts, &replay_lookup)?;
        let negative = scenario_negative_evidence(&generated_shape.negative_fixture);
        let operational_cases = scenario_operational_cases(contract, &selected_evidence);
        let package_id = format!("{}.{}", contract.suite, contract.test_name);
        let suite_dir = package_root.join(contract.suite).join(contract.test_name);
        std::fs::create_dir_all(&suite_dir)?;
        let package_path = suite_dir.join("package.json");
        let artifact = ScenarioContractPackageArtifact {
            schema: "lash.sim.scenario-contract-package.v1",
            package_id: package_id.clone(),
            contract: *contract,
            oracle_id: oracle_id.clone(),
            classification,
            semantic_oracle: contract.semantic_oracle,
            operational_cases: operational_cases.clone(),
            generated_shape: generated_shape.clone(),
            positive: positive.clone(),
            negative: negative.clone(),
            selected_evidence: selected_evidence.clone(),
            events,
            verdicts,
        };
        std::fs::write(&package_path, serde_json::to_vec_pretty(&artifact)?)?;
        manifests.push(ScenarioContractPackageManifest {
            schema: "lash.sim.scenario-contract-package-manifest.v1",
            package_id,
            suite: contract.suite,
            test_name: contract.test_name,
            semantic_oracle: contract.semantic_oracle,
            transition_kind: generated_shape.transition_kind,
            oracle_id,
            classification,
            status: "generated_replay_package_written",
            package_path: relative_path(artifact_root, &package_path),
            operational_cases,
            positive,
            negative,
        });
    }
    Ok(manifests)
}

fn assert_distinct_package_fact_graph(
    contract: &ScenarioContractSpec,
    generated_shape: &ScenarioGeneratedShape,
    seen: &mut BTreeMap<Vec<(String, Vec<String>)>, &'static str>,
) -> Result<(), FixedScriptRunnerError> {
    let fingerprint = transition_fact_graph_fingerprint(&generated_shape.transition_facts);
    if let Some(previous) = seen.insert(fingerprint, contract.test_name) {
        return Err(FixedScriptRunnerError::Assertion(format!(
            "scenario packages `{previous}` and `{}` produced identical generated transition fact graphs despite claiming distinct per-contract semantics",
            contract.test_name
        )));
    }
    Ok(())
}

pub(super) fn transition_fact_graph_fingerprint(
    transition_facts: &[ScenarioTransitionFact],
) -> Vec<(String, Vec<String>)> {
    transition_facts
        .iter()
        .map(|fact| (fact.fact.clone(), fact.boundary_ids.clone()))
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect()
}

struct BackendRegressionSpec {
    fixture_id: &'static str,
    required_boundary_kinds: &'static [&'static str],
    semantic_oracles: &'static [&'static str],
    regression_contract: &'static str,
    predicate: fn(&[&TraceEventLine]) -> bool,
}

pub(super) fn write_generated_backend_regression_fixtures(
    artifact_root: &Path,
    event_lines: &[TraceEventLine],
    replay_reports: &[GeneratedReplayArtifact],
) -> Result<Vec<GeneratedBackendRegressionManifest>, FixedScriptRunnerError> {
    let fixture_root = artifact_root.join(GENERATED_SIM_BACKEND_REGRESSION_FIXTURES);
    std::fs::create_dir_all(&fixture_root)?;
    let replay_lookup = replay_artifact_lookup(replay_reports);
    let mut by_trace: BTreeMap<String, Vec<&TraceEventLine>> = BTreeMap::new();
    for line in event_lines {
        by_trace
            .entry(line.trace_alias.clone())
            .or_default()
            .push(line);
    }

    let specs = [
        BackendRegressionSpec {
            fixture_id: "queued-active-turn-cancel-race",
            required_boundary_kinds: &["queued_ingress", "cancellation", "provider"],
            semantic_oracles: &[
                "sim.oracle.state-machine-semantic-invariants.v1",
                "sim.oracle.scenario-mini.runtime.queued-input-hidden-while-live.v1",
                "sim.oracle.scenario-mini.runtime.cancellation-prevents-idle-claim.v1",
            ],
            regression_contract: "active-turn queued input stays hidden, then cancellation terminalizes the pending row before any later idle claim can surface it",
            predicate: trace_has_queued_cancel_race,
        },
        BackendRegressionSpec {
            fixture_id: "trigger-wakeup-routes-process",
            required_boundary_kinds: &["trigger"],
            semantic_oracles: &["sim.oracle.state-machine-semantic-invariants.v1"],
            regression_contract: "trigger occurrence records a stable source key, reserves a matching delivery, and starts process wake routing without live external input",
            predicate: trace_has_trigger_wakeup_route,
        },
        BackendRegressionSpec {
            fixture_id: "duplicate-process-wake-idempotency",
            required_boundary_kinds: &["process_wake"],
            semantic_oracles: &[
                "sim.oracle.state-machine-semantic-invariants.v1",
                "sim.oracle.process-wake-observed.v1",
            ],
            regression_contract: "duplicate process wake deliveries share structural process/event identity, claim queued work once, and keep receiver idempotency evidence backed by generated dynamic replay",
            predicate: trace_has_duplicate_process_wake_idempotency,
        },
        BackendRegressionSpec {
            fixture_id: "worker-stale-completion-fenced",
            required_boundary_kinds: &["worker"],
            semantic_oracles: &[
                "sim.oracle.state-machine-semantic-invariants.v1",
                "sim.oracle.scenario-mini.runtime.stale-lease-commit-rejected.v1",
            ],
            regression_contract: "worker A loses its process lease to worker B; A's stale terminal output is rejected and absent, while B terminalizes the process exactly once",
            predicate: trace_has_worker_stale_completion,
        },
        BackendRegressionSpec {
            fixture_id: "durable-effect-crash-reopen-replay",
            required_boundary_kinds: &["durable_effect"],
            semantic_oracles: &["sim.oracle.state-machine-semantic-invariants.v1"],
            regression_contract: "durable effect first execution calls the local executor once and crash/reopen-style replay returns stored history without re-executing",
            predicate: trace_has_durable_effect_replay,
        },
        BackendRegressionSpec {
            fixture_id: "backend-retry-terminalization",
            required_boundary_kinds: &["backend_failure"],
            semantic_oracles: &["sim.oracle.state-machine-semantic-invariants.v1"],
            regression_contract: "retryable backend conflicts advance attempts and terminate on a non-retryable production StoreError class",
            predicate: trace_has_backend_retry_terminalization,
        },
        BackendRegressionSpec {
            fixture_id: "provider-protocol-terminalization",
            required_boundary_kinds: &["provider_mutation"],
            semantic_oracles: &["sim.oracle.state-machine-semantic-invariants.v1"],
            regression_contract: "scripted provider mutation matrices classify retryable 429 and dropped-terminal parser failures through every migrated provider parser",
            predicate: trace_has_provider_protocol_terminalization,
        },
        BackendRegressionSpec {
            fixture_id: "rlm-standard-protocol-terminal-boundaries",
            required_boundary_kinds: &["provider", "provider_mutation", "exec_code"],
            semantic_oracles: &[
                "sim.oracle.state-machine-semantic-invariants.v1",
                "sim.oracle.scenario.standard-contract.v1:standard_protocol_scenario_provider_error_stops_without_checkpoint",
                "sim.oracle.scenario.rlm-contract.v1:rlm_protocol_scenario_exec_any_tool_control_fail_is_terminal_error",
                "sim.oracle.scenario.rlm-contract.v1:rlm_protocol_scenario_exec_any_tool_control_frame_switch_is_terminal",
            ],
            regression_contract: "standard provider-error terminalization and RLM exec terminal boundaries stay represented by generated transitions with dynamic backend evidence",
            predicate: trace_has_protocol_terminal_boundary_mix,
        },
    ];

    let mut manifests = Vec::new();
    for spec in specs {
        let Some((trace_alias, _lines)) = by_trace
            .iter()
            .find(|(_alias, lines)| (spec.predicate)(lines))
        else {
            return Err(FixedScriptRunnerError::Assertion(format!(
                "generated backend regression fixture `{}` could not select a generated trace",
                spec.fixture_id
            )));
        };
        let replay = replay_lookup.get(trace_alias).ok_or_else(|| {
            FixedScriptRunnerError::Assertion(format!(
                "generated backend regression fixture `{}` selected trace `{trace_alias}` without replay artifact",
                spec.fixture_id
            ))
        })?;
        let package_dir = fixture_root.join(spec.fixture_id);
        std::fs::create_dir_all(&package_dir)?;
        let source_trace_path = artifact_root.join(&replay.trace_path);
        let fixture_trace_path = package_dir.join("trace.json");
        std::fs::copy(&source_trace_path, &fixture_trace_path)?;
        let package_path = package_dir.join("package.json");
        let static_backend_replay_policy = "not_claimed_for_generated_scheduler_traces";
        let backend_equivalence_contract = "source seed passed the dynamic generated workload rerun against the serialized in-memory reference and lash-sqlite-store; static SQLite/Postgres replay is a different fixed-order trace contract and is not inferred from this generated trace";
        let package = GeneratedBackendRegressionPackage {
            schema: "lash.sim.generated-backend-regression-package.v1",
            fixture_id: spec.fixture_id,
            status: "generated_cross_backend_valid_trace",
            trace: "trace.json",
            source_trace_path: replay.trace_path.clone(),
            source_trace_sha256: replay.trace_sha256.clone(),
            source_sqlite_replay_report_path: replay.sqlite_replay_report_path.clone(),
            source_sqlite_replay_report_sha256: replay.sqlite_replay_report_sha256.clone(),
            required_boundary_kinds: spec.required_boundary_kinds.to_vec(),
            semantic_oracles: spec.semantic_oracles.to_vec(),
            replay_backends: vec!["model"],
            static_backend_replay_policy,
            backend_equivalence_contract,
            regression_contract: spec.regression_contract,
            replay_command: format!(
                "cargo run -p lash-sim --locked -- replay {}",
                fixture_trace_path.display()
            ),
        };
        std::fs::write(&package_path, serde_json::to_vec_pretty(&package)?)?;
        manifests.push(GeneratedBackendRegressionManifest {
            schema: "lash.sim.generated-backend-regression-manifest.v1",
            fixture_id: spec.fixture_id,
            status: "generated_cross_backend_valid_trace",
            package_path: relative_path(artifact_root, &package_path),
            trace_path: relative_path(artifact_root, &fixture_trace_path),
            source_trace_path: replay.trace_path.clone(),
            source_trace_sha256: replay.trace_sha256.clone(),
            source_sqlite_replay_report_path: replay.sqlite_replay_report_path.clone(),
            source_sqlite_replay_report_sha256: replay.sqlite_replay_report_sha256.clone(),
            required_boundary_kinds: spec.required_boundary_kinds.to_vec(),
            semantic_oracles: spec.semantic_oracles.to_vec(),
            replay_backends: vec!["model"],
            static_backend_replay_policy,
            backend_equivalence_contract,
            regression_contract: spec.regression_contract,
        });
    }
    Ok(manifests)
}
