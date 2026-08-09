use super::*;
use crate::trace::value_digest;

/// Anti-vacuity: across the whole generated seed set, the interleaving,
/// suspend/resume, and transport-mutation boundary classes must EACH appear at
/// least once. Several class oracles pass when their class is absent (you cannot
/// interleave one session, a run with no suspend boundary has nothing to check,
/// etc.); this lane-level guard fails loudly if a generator regression silently
/// drops a class so those oracles can never pass vacuously across the run.
fn assert_generated_class_coverage(
    event_lines: &[TraceEventLine],
    interleaving_depth_max: usize,
) -> Result<(), FixedScriptRunnerError> {
    if interleaving_depth_max < 2 {
        return Err(FixedScriptRunnerError::Assertion(format!(
            "no generated seed interleaved >= 2 live provider turns (peak {interleaving_depth_max}); the interleaving class is absent across the seed set"
        )));
    }
    let suspend_resume = event_lines.iter().any(|line| {
        line.event.observed.get("runtime_suspend").is_some()
            || line
                .event
                .payload
                .get("suspend_resume")
                .and_then(Value::as_bool)
                == Some(true)
    });
    if !suspend_resume {
        return Err(FixedScriptRunnerError::Assertion(
            "no suspend-resume boundary appeared across the generated seed set; the suspend/resume class is absent".to_string(),
        ));
    }
    let transport_mutation = event_lines.iter().any(|line| {
        line.event.kind == BoundaryKind::ProviderMutation
            && line
                .event
                .observed
                .get("mutation")
                .or_else(|| line.event.payload.get("mutation"))
                .and_then(Value::as_str)
                .is_some_and(is_transport_provider_mutation)
    });
    if !transport_mutation {
        return Err(FixedScriptRunnerError::Assertion(
            "no transport provider mutation appeared across the generated seed set; the transport-mutation class is absent".to_string(),
        ));
    }
    Ok(())
}

/// How a count-based generated run treats per-seed artifacts.
///
/// `Evidence` writes the full per-seed artifact set (trace, replay report,
/// minimize package, cross-backend SQLite re-run) for every seed. `Search`
/// runs every seed live with the full oracle set plus an in-memory
/// determinism replay, persists a complete reproducibility package under
/// `failures/seed-<hex>/` only when a seed fails, and fails the run with the
/// exact replay command.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SimRunMode {
    Evidence,
    Search,
}

pub const WEEKLY_REGRESSION_CORPUS: &str = "weekly-fixed-v1";

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SimSeedSource {
    Exploration { salt: String },
    RegressionCorpus,
    Explicit,
}

impl SimSeedSource {
    pub fn exploration(salt: Option<String>) -> Self {
        Self::Exploration {
            salt: salt.unwrap_or_else(random_run_salt),
        }
    }

    pub fn regression_corpus(name: &str) -> Result<Self, String> {
        if name == WEEKLY_REGRESSION_CORPUS {
            Ok(Self::RegressionCorpus)
        } else {
            Err(format!(
                "unknown simulation seed corpus `{name}`; expected `{WEEKLY_REGRESSION_CORPUS}`"
            ))
        }
    }

    fn seed(&self, profile: &str, seed_index: usize) -> u64 {
        match self {
            Self::Exploration { salt } => generated_seed(profile, salt, seed_index),
            Self::RegressionCorpus => regression_corpus_seed(profile, seed_index),
            Self::Explicit => unreachable!("explicit seeds bypass count-based derivation"),
        }
    }

    fn source_name(&self) -> &'static str {
        match self {
            Self::Exploration { .. } => "salted_exploration",
            Self::RegressionCorpus => "named_regression_corpus",
            Self::Explicit => "explicit_seeds",
        }
    }

    fn salt(&self) -> Option<&str> {
        match self {
            Self::Exploration { salt } => Some(salt),
            Self::RegressionCorpus => None,
            Self::Explicit => None,
        }
    }

    fn corpus(&self) -> Option<&'static str> {
        match self {
            Self::Exploration { .. } => None,
            Self::RegressionCorpus => Some(WEEKLY_REGRESSION_CORPUS),
            Self::Explicit => None,
        }
    }
}

fn random_run_salt() -> String {
    format!("{:016x}{:016x}", fastrand::u64(..), fastrand::u64(..))
}

#[derive(Clone, Debug)]
pub struct SimRunModeError(String);

impl fmt::Display for SimRunModeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "invalid sim run mode `{}`: expected `evidence` or `search`",
            self.0
        )
    }
}

impl std::error::Error for SimRunModeError {}

impl SimRunMode {
    pub fn parse(raw: &str) -> Result<Self, SimRunModeError> {
        match raw {
            "evidence" => Ok(Self::Evidence),
            "search" => Ok(Self::Search),
            other => Err(SimRunModeError(other.to_string())),
        }
    }

    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Evidence => "evidence",
            Self::Search => "search",
        }
    }
}

/// Run identity recorded in the profile summary: which shard of which
/// configured seed count ran, in which mode.
struct GeneratedRunLabels {
    shard: String,
    configured_seeds: usize,
    mode: SimRunMode,
    seed_source: SimSeedSource,
}

pub async fn run_generated_sim_profile(
    artifact_root: impl AsRef<Path>,
    profile: &str,
    seeds: usize,
    max_boundaries: usize,
    shard: SimShard,
    mode: SimRunMode,
    seed_source: SimSeedSource,
) -> Result<GeneratedSimProfileReport, FixedScriptRunnerError> {
    lash_core::panic_containment::set_loud(true);
    let configured_seeds = seeds.max(1);
    let indexed_seeds = (0..configured_seeds)
        .filter(|seed_index| shard.selects(*seed_index))
        .map(|seed_index| (seed_index, seed_source.seed(profile, seed_index)))
        .collect::<Vec<_>>();
    if indexed_seeds.is_empty() {
        return Err(FixedScriptRunnerError::Assertion(format!(
            "shard {} selects no seeds from a configured count of {configured_seeds}",
            shard.label()
        )));
    }
    let labels = GeneratedRunLabels {
        shard: shard.label(),
        configured_seeds,
        mode,
        seed_source,
    };
    match mode {
        SimRunMode::Evidence => {
            Box::pin(run_generated_evidence_profile(
                artifact_root.as_ref(),
                profile,
                &indexed_seeds
                    .iter()
                    .map(|(_, seed)| *seed)
                    .collect::<Vec<_>>(),
                max_boundaries,
                labels,
            ))
            .await
        }
        SimRunMode::Search => {
            Box::pin(run_generated_search_profile(
                artifact_root.as_ref(),
                profile,
                &indexed_seeds,
                max_boundaries,
                labels,
            ))
            .await
        }
    }
}

pub async fn run_generated_sim_profile_for_seeds(
    artifact_root: impl AsRef<Path>,
    profile: &str,
    seed_values: &[u64],
    max_boundaries: usize,
) -> Result<GeneratedSimProfileReport, FixedScriptRunnerError> {
    lash_core::panic_containment::set_loud(true);
    let labels = GeneratedRunLabels {
        shard: SimShard::FULL.label(),
        configured_seeds: seed_values.len(),
        mode: SimRunMode::Evidence,
        seed_source: SimSeedSource::Explicit,
    };
    Box::pin(run_generated_evidence_profile(
        artifact_root.as_ref(),
        profile,
        seed_values,
        max_boundaries,
        labels,
    ))
    .await
}

async fn run_generated_evidence_profile(
    artifact_root: &Path,
    profile: &str,
    seed_values: &[u64],
    max_boundaries: usize,
    labels: GeneratedRunLabels,
) -> Result<GeneratedSimProfileReport, FixedScriptRunnerError> {
    validate_workload_profile(profile)?;
    if seed_values.is_empty() {
        return Err(FixedScriptRunnerError::Assertion(
            "generated simulation requires at least one seed".to_string(),
        ));
    }
    std::fs::create_dir_all(artifact_root)?;

    let provider_dir = artifact_root.join("provider-corpus");
    let fixed_manifest = Box::pin(run_fixed_script_profile(&provider_dir)).await?;
    write_provider_script_manifest(artifact_root, &fixed_manifest)?;

    let runtime_proof = prove_runtime_facade_turn().await?;
    let seed_count = seed_values.len();
    let boundary_limit = max_boundaries.max(1);
    let replay_dir = artifact_root.join("replays");
    std::fs::create_dir_all(&replay_dir)?;

    let mut event_lines = Vec::new();
    let mut replay_reports = Vec::new();
    let mut oracle_verdicts = Vec::new();
    let mut model_store_sessions = 0;
    let mut boundary_events = 0;
    let mut runtime_turn_proofs = 0;
    let mut interleaving_depth_max = 0;
    let mut interleaving_depth_min = usize::MAX;

    for seed in seed_values.iter().copied() {
        let workload = generate_workload(seed, profile, boundary_limit)?;
        let trace_path = replay_dir.join(format!("seed-{seed:016x}.trace.json"));
        let trace = run_generated_workload(
            workload,
            &fixed_manifest.script_bundle_hash,
            &labels.shard,
            &trace_path,
        )
        .await?;
        if !trace.oracle.is_passed() {
            return Err(FixedScriptRunnerError::Assertion(
                trace.oracle.message.clone(),
            ));
        }
        boundary_events += trace.events.len();
        runtime_turn_proofs += trace
            .events
            .iter()
            .filter(|event| event.kind == BoundaryKind::Provider)
            .count();
        let seed_interleaving_depth = peak_concurrent_live_turns(&trace.events);
        interleaving_depth_max = interleaving_depth_max.max(seed_interleaving_depth);
        interleaving_depth_min = interleaving_depth_min.min(seed_interleaving_depth);
        model_store_sessions += trace.final_summary.session_count;
        oracle_verdicts.extend(trace.oracles.iter().cloned());
        for event in trace.events.iter().cloned() {
            event_lines.push(TraceEventLine::new(
                format!("seed-{seed:016x}"),
                seed,
                profile,
                event,
            ));
        }
        write_trace(&trace_path, &trace)?;
        let transcript_path = trace_path.with_extension("txt");
        let transcript = trace.render_transcript();
        let transcript_artifact =
            write_generated_transcript_best_effort(artifact_root, &transcript_path, &transcript);
        let trace_sha256 = file_sha256(&trace_path)?;
        let replay = replay_trace(&trace_path, &trace)?;
        oracle_verdicts.push(replay.terminal_verdict.clone());
        let replay_report_path = replay_dir.join(format!("seed-{seed:016x}.replay.json"));
        let replay_report_sha256 = write_replay_report(&replay_report_path, &replay)?;
        let minimize_dir = replay_dir.join(format!("seed-{seed:016x}.minimize"));
        let minimize = minimize_trace(&trace_path, &trace, &minimize_dir)?;
        let minimize_report_path = minimize_dir.join("minimize.json");
        std::fs::write(&minimize_report_path, serde_json::to_vec_pretty(&minimize)?)?;
        let minimize_report_sha256 = file_sha256(&minimize_report_path)?;
        let minimized_trace_sha256 = file_sha256(&minimize.minimized_trace_path)?;
        let sqlite_database_path = replay_dir.join(format!("seed-{seed:016x}.sqlite-store"));
        let sqlite_replay_report_path =
            replay_dir.join(format!("seed-{seed:016x}.sqlite-replay.json"));
        // Cross-backend check: re-drive the SAME generated workload through the
        // SAME dynamic, concurrency-faithful runtime driver, backed by the real
        // `lash-sqlite-store` (session store + SQLite durable-effect controller),
        // and require the resulting observable Lash STATE (the abstract world
        // summary) to match the in-memory reference run exactly. Because both runs
        // share one driver and one scheduling discipline and differ ONLY in the
        // store, the comparison is apples-to-apples by construction: there is no
        // separate fixed-order, provider-event-gated re-drive that can deadlock or
        // spuriously diverge on active-turn / next-turn ingress timing. The
        // workload is regenerated deterministically from the seed (the original
        // was consumed by the reference run).
        let sqlite_workload = generate_workload(seed, profile, boundary_limit)?;
        // The cross-backend equivalence reference is a SERIALIZED in-memory run of
        // the same workload: it shares the durable re-run's serialize-provider-turn
        // discipline and differs ONLY in the backend store, so equality is a
        // well-posed durable-state check. (The concurrency-preserving search-lane
        // summary in `trace.final_summary` runs a different scheduling discipline
        // and is not directly comparable to the serialized durable re-run.)
        let serialized_reference = replay_workload_serialized_reference(&sqlite_workload).await?;
        let sqlite_summary =
            replay_workload_on_sqlite(&sqlite_workload, &sqlite_database_path).await?;
        let backend_verdict = replay_determinism(&serialized_reference, &sqlite_summary);
        oracle_verdicts.push(backend_verdict.clone());
        let sqlite_report = if backend_verdict.is_passed() {
            serde_json::json!({
                "schema": "lash.sim.sqlite-cross-backend-rerun.v1",
                "seed": seed,
                "profile": profile,
                "backend": "lash_sqlite_store",
                "driver": "unified_generated_runtime_world",
                "matches_reference": true,
                "reference_digest": serialized_reference.digest.clone(),
                "actual_digest": sqlite_summary.digest.clone(),
                "verdict": backend_verdict.clone(),
                "final_summary": sqlite_summary,
            })
        } else {
            serde_json::json!({
                "schema": "lash.sim.sqlite-cross-backend-rerun.v1",
                "seed": seed,
                "profile": profile,
                "backend": "lash_sqlite_store",
                "driver": "unified_generated_runtime_world",
                "matches_reference": false,
                "reference_digest": serialized_reference.digest.clone(),
                "actual_digest": sqlite_summary.digest.clone(),
                "verdict": backend_verdict.clone(),
                "reference_summary": serialized_reference,
                "actual_summary": sqlite_summary,
            })
        };
        std::fs::write(
            &sqlite_replay_report_path,
            serde_json::to_vec_pretty(&sqlite_report)?,
        )?;
        if !backend_verdict.is_passed() {
            return Err(FixedScriptRunnerError::Assertion(format!(
                "cross-backend SQLite re-run for seed {seed} ({profile}) diverged from the serialized in-memory reference: {}; wrote {}",
                backend_verdict.message,
                sqlite_replay_report_path.display()
            )));
        }
        let sqlite_replay_report_sha256 = file_sha256(&sqlite_replay_report_path)?;
        replay_reports.push(GeneratedReplayArtifact {
            seed,
            trace_path: relative_path(artifact_root, &trace_path),
            trace_sha256,
            transcript_path: transcript_artifact
                .as_ref()
                .map(|artifact| artifact.0.clone()),
            transcript_sha256: transcript_artifact.map(|artifact| artifact.1),
            replay_report_path: relative_path(artifact_root, &replay_report_path),
            replay_report_sha256,
            minimized_trace_path: relative_path(artifact_root, &minimize.minimized_trace_path),
            minimized_trace_sha256,
            failure_package_path: relative_path(artifact_root, &minimize.failure_package_path),
            minimize_report_path: relative_path(artifact_root, &minimize_report_path),
            minimize_report_sha256,
            sqlite_database_path: relative_path(artifact_root, &sqlite_database_path),
            sqlite_replay_report_path: relative_path(artifact_root, &sqlite_replay_report_path),
            sqlite_replay_report_sha256,
            replay_command: trace.replay_command,
            sqlite_replay_command: format!(
                "cargo run -p lash-sim --locked -- replay-sqlite {} --out {}",
                trace_path.display(),
                replay_dir.display()
            ),
        });
    }

    oracle_verdicts.push(runtime_proof.runtime_invariant.clone());
    oracle_verdicts.push(runtime_proof.provider_output_invariant.clone());
    oracle_verdicts.push(
        runtime_proof
            .pending_tool_completion
            .turn_suspension_invariant
            .clone(),
    );
    oracle_verdicts.push(
        runtime_proof
            .pending_tool_completion
            .scheduler_resolution_invariant
            .clone(),
    );
    oracle_verdicts.push(
        runtime_proof
            .pending_tool_completion
            .final_result_invariant
            .clone(),
    );
    oracle_verdicts.push(
        runtime_proof
            .final_value_semantic_channel
            .semantic_channel_invariant
            .clone(),
    );

    assert_generated_class_coverage(&event_lines, interleaving_depth_max)?;
    let events_sha256 = write_event_lines(&artifact_root.join(GENERATED_SIM_EVENTS), &event_lines)?;
    write_failure_artifact_shape(artifact_root)?;

    let oracle_passes = oracle_verdicts
        .iter()
        .filter(|verdict| verdict.status == OracleStatus::Passed)
        .count();
    let oracle_failures = oracle_verdicts.len() - oracle_passes;
    let scheduler_controlled_boundaries = event_lines
        .iter()
        .filter(|line| line.event.scheduler.scheduler_controlled)
        .count();
    let scheduler_owned_runtime_completions = event_lines
        .iter()
        .filter(|line| line.event.payload.get("runtime_completion").is_some())
        .count();
    let scenario_contract_oracles = oracle_verdicts
        .iter()
        .filter(|verdict| verdict.oracle_id.starts_with("sim.oracle.scenario."))
        .count();
    let scenario_contract_mini_oracles = oracle_verdicts
        .iter()
        .filter(|verdict| verdict.oracle_id.starts_with("sim.oracle.scenario-mini."))
        .count();
    let scenario_contract_slices =
        write_scenario_contract_slices(artifact_root, &event_lines, &oracle_verdicts)?;
    let scenario_contract_slice_count = scenario_contract_slices.len();
    let scenario_contract_packages = write_scenario_contract_packages(
        artifact_root,
        &event_lines,
        &oracle_verdicts,
        &replay_reports,
    )?;
    let scenario_contract_package_count = scenario_contract_packages.len();
    let generated_backend_regression_fixtures =
        write_generated_backend_regression_fixtures(artifact_root, &event_lines, &replay_reports)?;
    let generated_backend_regression_fixture_count = generated_backend_regression_fixtures.len();
    let generated_runtime_provider_matrix = generated_runtime_provider_matrix(&event_lines);
    let summary_path = artifact_root.join(GENERATED_SIM_SUMMARY);
    let report = GeneratedSimProfileReport {
        schema: "lash.sim.profile-summary.v1",
        profile: profile.to_string(),
        shard: labels.shard,
        configured_seeds: labels.configured_seeds,
        mode: labels.mode.as_str(),
        seed_source: labels.seed_source.source_name(),
        seed_salt: labels.seed_source.salt().map(ToString::to_string),
        seed_corpus: labels.seed_source.corpus(),
        determinism_sample: GeneratedDeterminismSample {
            policy: "not_applicable_to_evidence_mode",
            selected_seed_indices: Vec::new(),
            checked_seeds: 0,
            reproduced_identically: 0,
        },
        generator_version: GENERATOR_VERSION,
        script_bundle_hash: fixed_manifest.script_bundle_hash.clone(),
        provider_manifest_path: GENERATED_SIM_PROVIDER_MANIFEST,
        provider_matrix: fixed_manifest.provider_matrix.clone(),
        generated_runtime_provider_matrix,
        events_path: Some(GENERATED_SIM_EVENTS),
        events_sha256: Some(events_sha256),
        replay_reports,
        runtime_proof,
        scenario_contracts: scenario_contract_manifests(),
        scenario_contract_slices,
        scenario_contract_packages,
        generated_backend_regression_fixtures,
        model_only_boundary_reviews: model_only_boundary_reviews(),
        provider_transport_exclusions: fixed_manifest.provider_transport_exclusions.clone(),
        counts: GeneratedSimCounts {
            generated_seeds: seed_count,
            boundary_events,
            scheduler_controlled_boundaries,
            runtime_completion_registrations: scheduler_owned_runtime_completions,
            scheduler_owned_runtime_completions,
            fixed_provider_proofs: fixed_manifest.summary.total_proofs,
            runtime_proofs: runtime_turn_proofs + 3,
            replay_reports: seed_count,
            minimized_replays: seed_count,
            backend_replays: seed_count,
            scenario_contract_oracles,
            scenario_contract_mini_oracles,
            scenario_contract_slices: scenario_contract_slice_count,
            scenario_contract_packages: scenario_contract_package_count,
            generated_backend_regression_fixtures: generated_backend_regression_fixture_count,
            oracle_passes,
            oracle_failures,
            model_store_sessions,
            interleaving_depth_max,
            interleaving_depth_min: if interleaving_depth_min == usize::MAX {
                0
            } else {
                interleaving_depth_min
            },
        },
        oracle_verdicts,
        failure_artifact_shape: GENERATED_SIM_FAILURE_SHAPE,
        summary_path: summary_path.clone(),
    };
    std::fs::write(&summary_path, serde_json::to_vec_pretty(&report)?)?;
    Ok(report)
}

/// High-volume seed search over the generated DST world. Every seed runs live
/// with the full oracle set plus an in-memory determinism replay; passing
/// seeds retain no per-seed artifacts, and the first failing seed persists a
/// complete reproducibility package under `failures/seed-<hex>/` (trace,
/// replay evidence, failing oracles, final summary, minimized regression
/// package) before failing the run with the exact replay command. Per-seed
/// evidence artifacts (events log, replay reports, cross-backend re-runs,
/// scenario slices/packages) stay in the bounded evidence lane.
async fn run_generated_search_profile(
    artifact_root: &Path,
    profile: &str,
    indexed_seeds: &[(usize, u64)],
    max_boundaries: usize,
    labels: GeneratedRunLabels,
) -> Result<GeneratedSimProfileReport, FixedScriptRunnerError> {
    validate_workload_profile(profile)?;
    std::fs::create_dir_all(artifact_root)?;

    let provider_dir = artifact_root.join("provider-corpus");
    let fixed_manifest = Box::pin(run_fixed_script_profile(&provider_dir)).await?;
    write_provider_script_manifest(artifact_root, &fixed_manifest)?;

    let runtime_proof = prove_runtime_facade_turn().await?;
    let boundary_limit = max_boundaries.max(1);
    let failures_dir = artifact_root.join("failures");

    // Search keeps the summary lean: the verdicts retained are the runtime
    // facade proof plus any failing seed verdicts; passing per-seed verdicts
    // are counted but not recorded.
    let mut recorded_verdicts: Vec<OracleVerdict> = vec![
        runtime_proof.runtime_invariant.clone(),
        runtime_proof.provider_output_invariant.clone(),
        runtime_proof
            .pending_tool_completion
            .turn_suspension_invariant
            .clone(),
        runtime_proof
            .pending_tool_completion
            .scheduler_resolution_invariant
            .clone(),
        runtime_proof
            .pending_tool_completion
            .final_result_invariant
            .clone(),
        runtime_proof
            .final_value_semantic_channel
            .semantic_channel_invariant
            .clone(),
    ];
    let mut oracle_passes = recorded_verdicts
        .iter()
        .filter(|verdict| verdict.status == OracleStatus::Passed)
        .count();
    let mut oracle_failures = recorded_verdicts.len() - oracle_passes;

    let mut provider_matrix_by_kind: BTreeMap<String, GeneratedRuntimeProviderMatrixRow> =
        BTreeMap::new();
    let mut boundary_events = 0usize;
    let mut runtime_turn_proofs = 0usize;
    let mut scheduler_controlled_boundaries = 0usize;
    let mut scheduler_owned_runtime_completions = 0usize;
    let mut scenario_contract_oracles = 0usize;
    let mut scenario_contract_mini_oracles = 0usize;
    let mut model_store_sessions = 0usize;
    let mut interleaving_depth_max = 0usize;
    let mut interleaving_depth_min = usize::MAX;

    let selected_determinism_indices = indexed_seeds
        .iter()
        .filter_map(|(index, _)| (index % 20 == 0).then_some(*index))
        .collect::<Vec<_>>();
    let mut reproduced_identically = 0usize;

    for (seed_index, seed) in indexed_seeds.iter().copied() {
        let workload = generate_workload(seed, profile, boundary_limit)?;
        let seed_dir = failures_dir.join(format!("seed-{seed:016x}"));
        let trace_path = seed_dir.join("trace.json");
        let trace = run_generated_workload(
            workload,
            &fixed_manifest.script_bundle_hash,
            &labels.shard,
            &trace_path,
        )
        .await?;

        boundary_events += trace.events.len();
        runtime_turn_proofs += trace
            .events
            .iter()
            .filter(|event| event.kind == BoundaryKind::Provider)
            .count();
        scheduler_controlled_boundaries += trace
            .events
            .iter()
            .filter(|event| event.scheduler.scheduler_controlled)
            .count();
        scheduler_owned_runtime_completions += trace
            .events
            .iter()
            .filter(|event| event.payload.get("runtime_completion").is_some())
            .count();
        observe_runtime_provider_matrix(&mut provider_matrix_by_kind, trace.events.iter());
        let seed_interleaving_depth = peak_concurrent_live_turns(&trace.events);
        interleaving_depth_max = interleaving_depth_max.max(seed_interleaving_depth);
        interleaving_depth_min = interleaving_depth_min.min(seed_interleaving_depth);
        model_store_sessions += trace.final_summary.session_count;
        for verdict in &trace.oracles {
            if verdict.oracle_id.starts_with("sim.oracle.scenario-mini.") {
                scenario_contract_mini_oracles += 1;
            } else if verdict.oracle_id.starts_with("sim.oracle.scenario.") {
                scenario_contract_oracles += 1;
            }
            if verdict.status == OracleStatus::Passed {
                oracle_passes += 1;
            } else {
                oracle_failures += 1;
                recorded_verdicts.push(verdict.clone());
            }
        }

        // In-memory determinism replay for every search seed.
        let replay_outcome = replay_trace(&trace_path, &trace);
        match &replay_outcome {
            Ok(_) => oracle_passes += 1,
            Err(_) => oracle_failures += 1,
        }
        let determinism_failure = if seed_index % 20 == 0 {
            let rerun = run_generated_workload(
                generate_workload(seed, profile, boundary_limit)?,
                &fixed_manifest.script_bundle_hash,
                &labels.shard,
                &trace_path,
            )
            .await?;
            match require_identical_simulation_rerun(seed_index, &trace, &rerun) {
                Ok(()) => {
                    reproduced_identically += 1;
                    oracle_passes += 1;
                    None
                }
                Err(err) => {
                    oracle_failures += 1;
                    Some(err.to_string())
                }
            }
        } else {
            None
        };
        if trace.oracle.is_passed() && replay_outcome.is_ok() && determinism_failure.is_none() {
            continue;
        }

        // Failing seed: persist the complete reproducibility package, then
        // fail the run with the exact replay command.
        std::fs::create_dir_all(&seed_dir)?;
        write_trace(&trace_path, &trace)?;
        let transcript_path = seed_dir.join("transcript.txt");
        if let Err(err) = std::fs::write(&transcript_path, trace.render_transcript()) {
            eprintln!(
                "warning: generated transcript `{}` was not written: {err}",
                transcript_path.display()
            );
        }
        match &replay_outcome {
            Ok(replay) => {
                write_replay_report(&seed_dir.join("replay.json"), replay)?;
            }
            Err(err) => {
                let divergence = json!({
                    "schema": "lash.sim.search-replay-divergence.v1",
                    "seed": seed,
                    "profile": profile,
                    "shard": labels.shard,
                    "error": err.to_string(),
                });
                std::fs::write(
                    seed_dir.join("replay-divergence.json"),
                    serde_json::to_vec_pretty(&divergence)?,
                )?;
            }
        }
        if let Some(error) = &determinism_failure {
            let divergence = json!({
                "schema": "lash.sim.search-determinism-divergence.v1",
                "seed": seed,
                "seed_index": seed_index,
                "profile": profile,
                "shard": labels.shard,
                "seed_source": labels.seed_source.source_name(),
                "seed_salt": labels.seed_source.salt(),
                "seed_corpus": labels.seed_source.corpus(),
                "error": error,
            });
            std::fs::write(
                seed_dir.join("determinism-divergence.json"),
                serde_json::to_vec_pretty(&divergence)?,
            )?;
        }
        let failing_oracles = trace
            .oracles
            .iter()
            .filter(|verdict| verdict.status != OracleStatus::Passed)
            .collect::<Vec<_>>();
        std::fs::write(
            seed_dir.join("failing-oracles.json"),
            serde_json::to_vec_pretty(&failing_oracles)?,
        )?;
        std::fs::write(
            seed_dir.join("final-summary.json"),
            serde_json::to_vec_pretty(&trace.final_summary)?,
        )?;
        // Oracle-preserving minimization targets the combined trace oracle; a
        // replay-only divergence has no failing oracle to preserve.
        let minimize_summary = if trace.oracle.is_passed() {
            json!({
                "skipped":
                    "in-memory determinism replay diverged; minimization targets a failing oracle"
            })
        } else {
            let minimize_dir = seed_dir.join("minimize");
            match minimize_trace(&trace_path, &trace, &minimize_dir) {
                Ok(minimize) => {
                    let minimize_report_path = minimize_dir.join("minimize.json");
                    std::fs::write(&minimize_report_path, serde_json::to_vec_pretty(&minimize)?)?;
                    json!({
                        "minimize_report_path": relative_path(artifact_root, &minimize_report_path),
                        "minimized_trace_path":
                            relative_path(artifact_root, &minimize.minimized_trace_path),
                        "failure_package_path":
                            relative_path(artifact_root, &minimize.failure_package_path),
                    })
                }
                Err(err) => json!({ "error": err.to_string() }),
            }
        };
        let failure_reason = if let Some(error) = determinism_failure {
            error
        } else if trace.oracle.is_passed() {
            match &replay_outcome {
                Err(err) => err.to_string(),
                Ok(_) => unreachable!("failing search seed with passing oracle and passing replay"),
            }
        } else {
            trace.oracle.message.clone()
        };
        let package = json!({
            "schema": "lash.sim.search-failure-package.v1",
            "seed": seed,
            "seed_index": seed_index,
            "profile": profile,
            "shard": labels.shard,
            "mode": labels.mode.as_str(),
            "seed_source": labels.seed_source.source_name(),
            "seed_salt": labels.seed_source.salt(),
            "seed_corpus": labels.seed_source.corpus(),
            "reason": failure_reason,
            "replay_command": trace.replay_command,
            "regenerate_command": format!(
                "cargo run -p lash-sim --locked -- run --out <artifact-root> --profile {profile} --seed {seed} --max-boundaries {boundary_limit}"
            ),
            "minimize": minimize_summary,
        });
        std::fs::write(
            seed_dir.join("package.json"),
            serde_json::to_vec_pretty(&package)?,
        )?;
        let failure_summary = json!({
            "schema": "lash.sim.search-failure-summary.v1",
            "status": "failed",
            "profile": profile,
            "shard": labels.shard,
            "configured_seeds": labels.configured_seeds,
            "mode": labels.mode.as_str(),
            "seed_source": labels.seed_source.source_name(),
            "seed_salt": labels.seed_source.salt(),
            "seed_corpus": labels.seed_source.corpus(),
            "failing_seed_index": seed_index,
            "failing_seed": seed,
            "reason": failure_reason,
            "failure_package": relative_path(artifact_root, &seed_dir.join("package.json")),
        });
        std::fs::write(
            artifact_root.join(GENERATED_SIM_SUMMARY),
            serde_json::to_vec_pretty(&failure_summary)?,
        )?;
        return Err(FixedScriptRunnerError::Assertion(format!(
            "search seed index {seed_index}, seed-{seed:016x} ({profile}, shard {}) failed: {failure_reason}; \
             reproducibility package at {}; reproduce with: {}",
            labels.shard,
            seed_dir.display(),
            trace.replay_command
        )));
    }

    write_failure_artifact_shape(artifact_root)?;
    let generated_runtime_provider_matrix = finish_runtime_provider_matrix(provider_matrix_by_kind);
    let summary_path = artifact_root.join(GENERATED_SIM_SUMMARY);
    let report = GeneratedSimProfileReport {
        schema: "lash.sim.profile-summary.v1",
        profile: profile.to_string(),
        shard: labels.shard,
        configured_seeds: labels.configured_seeds,
        mode: labels.mode.as_str(),
        seed_source: labels.seed_source.source_name(),
        seed_salt: labels.seed_source.salt().map(ToString::to_string),
        seed_corpus: labels.seed_source.corpus(),
        determinism_sample: GeneratedDeterminismSample {
            policy: "global_seed_index_mod_20_equals_zero",
            selected_seed_indices: selected_determinism_indices,
            checked_seeds: reproduced_identically,
            reproduced_identically,
        },
        generator_version: GENERATOR_VERSION,
        script_bundle_hash: fixed_manifest.script_bundle_hash.clone(),
        provider_manifest_path: GENERATED_SIM_PROVIDER_MANIFEST,
        provider_matrix: fixed_manifest.provider_matrix.clone(),
        generated_runtime_provider_matrix,
        events_path: None,
        events_sha256: None,
        replay_reports: Vec::new(),
        runtime_proof,
        scenario_contracts: scenario_contract_manifests(),
        scenario_contract_slices: Vec::new(),
        scenario_contract_packages: Vec::new(),
        generated_backend_regression_fixtures: Vec::new(),
        model_only_boundary_reviews: model_only_boundary_reviews(),
        provider_transport_exclusions: fixed_manifest.provider_transport_exclusions.clone(),
        counts: GeneratedSimCounts {
            generated_seeds: indexed_seeds.len(),
            boundary_events,
            scheduler_controlled_boundaries,
            runtime_completion_registrations: scheduler_owned_runtime_completions,
            scheduler_owned_runtime_completions,
            fixed_provider_proofs: fixed_manifest.summary.total_proofs,
            runtime_proofs: runtime_turn_proofs + 3,
            replay_reports: 0,
            minimized_replays: 0,
            backend_replays: 0,
            scenario_contract_oracles,
            scenario_contract_mini_oracles,
            scenario_contract_slices: 0,
            scenario_contract_packages: 0,
            generated_backend_regression_fixtures: 0,
            oracle_passes,
            oracle_failures,
            model_store_sessions,
            interleaving_depth_max,
            interleaving_depth_min: if interleaving_depth_min == usize::MAX {
                0
            } else {
                interleaving_depth_min
            },
        },
        oracle_verdicts: recorded_verdicts,
        failure_artifact_shape: GENERATED_SIM_FAILURE_SHAPE,
        summary_path: summary_path.clone(),
    };
    std::fs::write(&summary_path, serde_json::to_vec_pretty(&report)?)?;
    Ok(report)
}

pub(super) fn write_generated_transcript_best_effort(
    artifact_root: &Path,
    transcript_path: &Path,
    transcript: &str,
) -> Option<(String, String)> {
    if let Err(err) = std::fs::write(transcript_path, transcript) {
        eprintln!(
            "warning: generated transcript `{}` was not written: {err}",
            transcript_path.display()
        );
        return None;
    }
    match file_sha256(transcript_path) {
        Ok(sha256) => Some((relative_path(artifact_root, transcript_path), sha256)),
        Err(err) => {
            eprintln!(
                "warning: generated transcript `{}` was written but not hashed: {err}",
                transcript_path.display()
            );
            None
        }
    }
}

fn write_provider_script_manifest(
    artifact_root: &Path,
    fixed_manifest: &FixedScriptManifest,
) -> Result<(), FixedScriptRunnerError> {
    let manifest = ProviderScriptProfileManifest {
        schema: "lash.sim.provider-script-manifest.v1",
        fixed_script_manifest: "provider-corpus/fixed-script-manifest.json".to_string(),
        fixed_script_summary: "provider-corpus/summary.json".to_string(),
        script_bundle_hash: fixed_manifest.script_bundle_hash.clone(),
        scripts: fixed_manifest.scripts.clone(),
        provider_matrix: fixed_manifest.provider_matrix.clone(),
        provider_transport_exclusions: fixed_manifest.provider_transport_exclusions.clone(),
    };
    std::fs::write(
        artifact_root.join(GENERATED_SIM_PROVIDER_MANIFEST),
        serde_json::to_vec_pretty(&manifest)?,
    )?;
    Ok(())
}

fn write_failure_artifact_shape(artifact_root: &Path) -> Result<(), FixedScriptRunnerError> {
    let path = artifact_root.join(GENERATED_SIM_FAILURE_SHAPE);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let shape = FailureArtifactShape {
        schema: "lash.sim.failure-artifact-shape.v1",
        directory: "failures/<failure-id>/",
        trace: "trace.json",
        replay_report: "replay.json",
        oracle: "oracle.json",
        final_summary: "final-summary.json",
    };
    std::fs::write(path, serde_json::to_vec_pretty(&shape)?)?;
    Ok(())
}

pub(super) fn generated_seed(profile: &str, salt: &str, seed_index: usize) -> u64 {
    let mut hasher = Sha256::new();
    hasher.update((profile.len() as u64).to_le_bytes());
    hasher.update(profile.as_bytes());
    hasher.update((salt.len() as u64).to_le_bytes());
    hasher.update(salt.as_bytes());
    hasher.update(seed_index.to_le_bytes());
    let digest = hasher.finalize();
    let mut bytes = [0_u8; 8];
    bytes.copy_from_slice(&digest[..8]);
    u64::from_le_bytes(bytes)
}

pub(super) fn regression_corpus_seed(profile: &str, seed_index: usize) -> u64 {
    let mut hasher = Sha256::new();
    hasher.update(profile.as_bytes());
    hasher.update(seed_index.to_le_bytes());
    let digest = hasher.finalize();
    let mut bytes = [0_u8; 8];
    bytes.copy_from_slice(&digest[..8]);
    u64::from_le_bytes(bytes)
}

fn require_identical_simulation_rerun(
    seed_index: usize,
    first: &SimulationTrace,
    second: &SimulationTrace,
) -> Result<(), FixedScriptRunnerError> {
    let first = determinism_projection(first)?;
    let second = determinism_projection(second)?;
    if first != second {
        let difference = first_json_difference("$", &first, &second)
            .unwrap_or_else(|| "unknown difference".to_string());
        return Err(FixedScriptRunnerError::Assertion(format!(
            "simulator nondeterminism: seed index {seed_index}, seed-{:016x} did not reproduce identically; {difference}; first={}; rerun={}",
            first["seed"].as_u64().unwrap_or_default(),
            value_digest(&first),
            value_digest(&second)
        )));
    }
    Ok(())
}

fn determinism_projection(trace: &SimulationTrace) -> Result<Value, serde_json::Error> {
    let mut value = serde_json::to_value(trace)?;
    if let Some(events) = value.get_mut("events").and_then(Value::as_array_mut) {
        for event in events {
            // Provider futures are harvested on the first host scheduler pass
            // that observes completion, so this diagnostic virtual timestamp is
            // not part of the deterministic simulator decision stream.
            if let Some(observed) = event.get_mut("observed").and_then(Value::as_object_mut) {
                observed.remove("sim_clock");
            }
        }
    }
    if let Some(writes) = value
        .get_mut("durable_writes")
        .and_then(Value::as_array_mut)
    {
        for write in writes {
            let attributed_session = write
                .get("attributed_session_id")
                .and_then(Value::as_str)
                .map(ToString::to_string);
            if let Some(attributed_session) = attributed_session {
                // Separately executed contract proofs allocate a fresh real
                // runtime session id on every execution, then attribute the
                // accepted write to a stable generated session. The raw UUID is
                // entropy by contract; its stable attribution is the simulator
                // identity checked here.
                write["session_id"] = Value::String(attributed_session);
            }
            if let Some(write) = write.as_object_mut() {
                // Checker state contains raw persistence identities derived from
                // host-time frame creation. Its internal graph/transcript/usage
                // consistency is checked independently on each run; the unseed
                // comparison retains the stable checkpoint seam and the full
                // delivered boundary trace, matching the pre-v3 trace surface.
                write.remove("state");
            }
        }
    }
    Ok(value)
}

fn first_json_difference(path: &str, first: &Value, second: &Value) -> Option<String> {
    match (first, second) {
        (Value::Object(first), Value::Object(second)) => {
            let keys = first.keys().chain(second.keys()).collect::<BTreeSet<_>>();
            for key in keys {
                let child_path = format!("{path}.{key}");
                match (first.get(key), second.get(key)) {
                    (Some(first), Some(second)) => {
                        if let Some(difference) = first_json_difference(&child_path, first, second)
                        {
                            return Some(difference);
                        }
                    }
                    (first, second) => {
                        return Some(format!(
                            "first difference at {child_path}: first={first:?}; rerun={second:?}"
                        ));
                    }
                }
            }
            None
        }
        (Value::Array(first), Value::Array(second)) => {
            if first.len() != second.len() {
                return Some(format!(
                    "first difference at {path}.length: first={}; rerun={}",
                    first.len(),
                    second.len()
                ));
            }
            for (index, (first, second)) in first.iter().zip(second).enumerate() {
                if let Some(difference) =
                    first_json_difference(&format!("{path}[{index}]"), first, second)
                {
                    return Some(difference);
                }
            }
            None
        }
        _ if first != second => Some(format!(
            "first difference at {path}: first={first}; rerun={second}"
        )),
        _ => None,
    }
}

pub(super) fn provider_matrix(
    scripts: &[ScriptHashManifest],
    proof_runs: &[ProofRun],
) -> Vec<ProviderMatrixRow> {
    let mut by_provider: BTreeMap<String, ProviderMatrixRow> = BTreeMap::new();
    for script in scripts {
        let row = by_provider
            .entry(script.provider_kind.clone())
            .or_insert_with(|| ProviderMatrixRow {
                provider_kind: script.provider_kind.clone(),
                script_names: Vec::new(),
                proof_names: Vec::new(),
                endpoints: Vec::new(),
                success_proofs: 0,
                error_proofs: 0,
                cancelled_proofs: 0,
            });
        row.script_names.push(script.name.clone());
    }
    for proof in proof_runs {
        let row = by_provider
            .entry(proof.provider_kind.clone())
            .or_insert_with(|| ProviderMatrixRow {
                provider_kind: proof.provider_kind.clone(),
                script_names: Vec::new(),
                proof_names: Vec::new(),
                endpoints: Vec::new(),
                success_proofs: 0,
                error_proofs: 0,
                cancelled_proofs: 0,
            });
        row.proof_names.push(proof.name.clone());
        row.endpoints.push(proof.endpoint.clone());
        match proof.transcript.terminal.classification {
            "success" => row.success_proofs += 1,
            "error" => row.error_proofs += 1,
            "cancelled_before_response_start" => row.cancelled_proofs += 1,
            _ => {}
        }
    }
    by_provider
        .into_values()
        .map(|mut row| {
            row.script_names.sort();
            row.script_names.dedup();
            row.proof_names.sort();
            row.proof_names.dedup();
            row.endpoints.sort();
            row.endpoints.dedup();
            row
        })
        .collect()
}

fn observe_runtime_provider_matrix<'a>(
    by_provider: &mut BTreeMap<String, GeneratedRuntimeProviderMatrixRow>,
    events: impl Iterator<Item = &'a crate::scheduler::DeliveredBoundary>,
) {
    for event in events {
        match event.kind {
            BoundaryKind::Ingress => {
                let Some(provider_kind) =
                    event.payload.get("provider_kind").and_then(Value::as_str)
                else {
                    continue;
                };
                let script = event
                    .payload
                    .get("provider_script")
                    .and_then(Value::as_str)
                    .unwrap_or("unknown")
                    .to_string();
                let row = by_provider
                    .entry(provider_kind.to_string())
                    .or_insert_with(|| GeneratedRuntimeProviderMatrixRow {
                        provider_kind: provider_kind.to_string(),
                        script_names: Vec::new(),
                        runtime_session_count: 0,
                        runtime_provider_turn_count: 0,
                    });
                row.runtime_session_count += 1;
                row.script_names.push(script);
            }
            BoundaryKind::Provider => {
                let Some(provider_kind) =
                    event.payload.get("provider_kind").and_then(Value::as_str)
                else {
                    continue;
                };
                let script = event
                    .payload
                    .get("script")
                    .and_then(Value::as_str)
                    .unwrap_or("unknown")
                    .to_string();
                let row = by_provider
                    .entry(provider_kind.to_string())
                    .or_insert_with(|| GeneratedRuntimeProviderMatrixRow {
                        provider_kind: provider_kind.to_string(),
                        script_names: Vec::new(),
                        runtime_session_count: 0,
                        runtime_provider_turn_count: 0,
                    });
                row.runtime_provider_turn_count += 1;
                row.script_names.push(script);
            }
            _ => {}
        }
    }
}

fn finish_runtime_provider_matrix(
    by_provider: BTreeMap<String, GeneratedRuntimeProviderMatrixRow>,
) -> Vec<GeneratedRuntimeProviderMatrixRow> {
    by_provider
        .into_values()
        .map(|mut row| {
            row.script_names.sort();
            row.script_names.dedup();
            row
        })
        .collect()
}

fn generated_runtime_provider_matrix(
    event_lines: &[TraceEventLine],
) -> Vec<GeneratedRuntimeProviderMatrixRow> {
    let mut by_provider: BTreeMap<String, GeneratedRuntimeProviderMatrixRow> = BTreeMap::new();
    observe_runtime_provider_matrix(&mut by_provider, event_lines.iter().map(|line| &line.event));
    finish_runtime_provider_matrix(by_provider)
}

#[cfg(test)]
mod seed_tests {
    use std::collections::BTreeSet;

    use super::*;

    #[test]
    fn salted_seed_runs_explore_disjoint_seed_sets_and_reproduce_by_index() {
        let first = (0..1_000)
            .map(|index| generated_seed("full-random", "run-a", index))
            .collect::<BTreeSet<_>>();
        let second = (0..1_000)
            .map(|index| generated_seed("full-random", "run-b", index))
            .collect::<BTreeSet<_>>();

        assert!(first.is_disjoint(&second));
        assert_eq!(
            generated_seed("full-random", "recorded-salt", 317),
            generated_seed("full-random", "recorded-salt", 317)
        );
    }

    #[test]
    fn named_regression_corpus_retains_the_unsalted_seed_derivation() {
        let corpus = SimSeedSource::regression_corpus(WEEKLY_REGRESSION_CORPUS)
            .expect("named regression corpus");
        assert_eq!(
            corpus.seed("full-random", 12),
            regression_corpus_seed("full-random", 12)
        );
        assert_eq!(corpus.corpus(), Some(WEEKLY_REGRESSION_CORPUS));
    }

    #[tokio::test]
    async fn determinism_sample_rejects_an_injected_nondeterminism_source() {
        let workload = generate_workload(5, "fast-random", 24).expect("workload");
        let first = run_generated_workload_for_fixture(workload, "bundle")
            .await
            .expect("first run");
        let mut injected = first.clone();
        let event = injected
            .events
            .iter_mut()
            .find(|event| event.kind == BoundaryKind::Provider)
            .expect("provider event");
        event.observed["injected_os_entropy"] = json!(fastrand::u64(..));

        let error = require_identical_simulation_rerun(0, &first, &injected)
            .expect_err("injected nondeterminism must fail loudly");
        assert!(error.to_string().contains("simulator nondeterminism"));
    }
}
