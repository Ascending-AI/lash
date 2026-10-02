use lash_sansio::SessionId;
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use crate::backend_fault::{
    BackendFault, BackendFaultArm, BackendFaultKind, BackendFaultLane, BackendFaultObservation,
    BackendFaultScript,
};
use lash_core::{
    DeploymentStore, OperationId, RuntimeCommit, RuntimeSessionState, RuntimeStore,
    SessionCreationHead, SessionPolicy, SessionRelation, SessionStoreCreateRequest, StoreError,
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

pub const DEFAULT_SQLITE_FAULT_SEED_BASE: u64 = 0x0000_0000_0859_0000;

/// The report schemas and fixed oracle ids of one backend's fault profile.
///
/// Every id a profile emits names the backend it judged, so a PostgreSQL
/// failure is never filed under a SQLite oracle and the declared inventory in
/// each declaration carries its observation class. SQLite keeps the
/// exact strings the confidence gate and `crates/lash-sim/README.md` already
/// document.
struct BackendFaultNaming {
    profile_schema: &'static str,
    plan_schema: &'static str,
    composition_schema: &'static str,
    failure_schema: &'static str,
    harness_oracle: crate::trace::OracleId<'static>,
    typed_error_oracle: crate::trace::OracleId<'static>,
    preserves_committed_work_oracle: crate::trace::OracleId<'static>,
    reopen_preserves_committed_work_oracle: crate::trace::OracleId<'static>,
    no_duplicate_effect_oracle: crate::trace::OracleId<'static>,
    multi_arm_composition_oracle: crate::trace::OracleId<'static>,
    refused_commit_oracle: crate::trace::OracleId<'static>,
    lost_commit_reply_oracle: crate::trace::OracleId<'static>,
    reopen_mid_sequence_oracle: crate::trace::OracleId<'static>,
}

const SQLITE_NAMING: BackendFaultNaming = BackendFaultNaming {
    profile_schema: "lash.sim.sqlite-substrate-faults.v2",
    plan_schema: "lash.sim.sqlite-fault-plan.v1",
    composition_schema: "lash.sim.sqlite-fault-composition.v1",
    failure_schema: "lash.sim.sqlite-substrate-fault-failure.v1",
    harness_oracle: crate::trace::OracleId::real("sim.oracle.sqlite-fault-harness.v1"),
    typed_error_oracle: crate::trace::OracleId::real("sim.oracle.sqlite-fault-typed-error.v1"),
    preserves_committed_work_oracle: crate::trace::OracleId::real(
        "sim.oracle.sqlite-fault-preserves-committed-work.v1",
    ),
    reopen_preserves_committed_work_oracle: crate::trace::OracleId::real(
        "sim.oracle.sqlite-reopen-preserves-committed-work.v1",
    ),
    no_duplicate_effect_oracle: crate::trace::OracleId::real(
        "sim.oracle.sqlite-fault-no-duplicate-effect.v1",
    ),
    multi_arm_composition_oracle: crate::trace::OracleId::real(
        "sim.oracle.sqlite-multi-arm-composition.v1",
    ),
    refused_commit_oracle: crate::trace::OracleId::real("sim.oracle.sqlite-refused-commit.v1"),
    lost_commit_reply_oracle: crate::trace::OracleId::real(
        "sim.oracle.sqlite-lost-commit-reply.v1",
    ),
    reopen_mid_sequence_oracle: crate::trace::OracleId::real(
        "sim.oracle.sqlite-reopen-mid-sequence.v1",
    ),
};

const POSTGRES_NAMING: BackendFaultNaming = BackendFaultNaming {
    profile_schema: "lash.sim.postgres-substrate-faults.v2",
    plan_schema: "lash.sim.postgres-fault-plan.v1",
    composition_schema: "lash.sim.postgres-fault-composition.v1",
    failure_schema: "lash.sim.postgres-substrate-fault-failure.v1",
    harness_oracle: crate::trace::OracleId::real("sim.oracle.postgres-fault-harness.v1"),
    typed_error_oracle: crate::trace::OracleId::real("sim.oracle.postgres-fault-typed-error.v1"),
    preserves_committed_work_oracle: crate::trace::OracleId::real(
        "sim.oracle.postgres-fault-preserves-committed-work.v1",
    ),
    reopen_preserves_committed_work_oracle: crate::trace::OracleId::real(
        "sim.oracle.postgres-reopen-preserves-committed-work.v1",
    ),
    no_duplicate_effect_oracle: crate::trace::OracleId::real(
        "sim.oracle.postgres-fault-no-duplicate-effect.v1",
    ),
    multi_arm_composition_oracle: crate::trace::OracleId::real(
        "sim.oracle.postgres-multi-arm-composition.v1",
    ),
    refused_commit_oracle: crate::trace::OracleId::real("sim.oracle.postgres-refused-commit.v1"),
    lost_commit_reply_oracle: crate::trace::OracleId::real(
        "sim.oracle.postgres-lost-commit-reply.v1",
    ),
    reopen_mid_sequence_oracle: crate::trace::OracleId::real(
        "sim.oracle.postgres-reopen-mid-sequence.v1",
    ),
};

const fn naming(backend: BackendFaultKind) -> &'static BackendFaultNaming {
    match backend {
        BackendFaultKind::SqliteMemory | BackendFaultKind::Sqlite => &SQLITE_NAMING,
        BackendFaultKind::Postgres => &POSTGRES_NAMING,
    }
}

/// The exact `lash-sim` invocation that replays `seed` on `backend`.
fn exact_replay_command(backend: BackendFaultKind, replay_root: &Path, seed: u64) -> String {
    format!(
        "cargo run -p lash-sim -- backend-faults {} --out {} --seed {seed}",
        backend.replay_backend_argument(),
        replay_root.display()
    )
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum BackendFaultScenarioKind {
    RefusedCommit,
    LostCommitReply,
    ReopenMidSequence,
}

impl BackendFaultScenarioKind {
    const ALL: [Self; 3] = [
        Self::RefusedCommit,
        Self::LostCommitReply,
        Self::ReopenMidSequence,
    ];

    fn for_seed(seed: u64) -> Self {
        match seed % 3 {
            0 => Self::RefusedCommit,
            1 => Self::LostCommitReply,
            _ => Self::ReopenMidSequence,
        }
    }

    fn fault(self) -> Option<BackendFault> {
        match self {
            Self::RefusedCommit => Some(BackendFault::Refused),
            Self::LostCommitReply => Some(BackendFault::ReplyLost),
            Self::ReopenMidSequence => None,
        }
    }

    fn oracle_id(self, backend: BackendFaultKind) -> crate::trace::OracleId<'static> {
        let names = naming(backend);
        match self {
            Self::RefusedCommit => names.refused_commit_oracle,
            Self::LostCommitReply => names.lost_commit_reply_oracle,
            Self::ReopenMidSequence => names.reopen_mid_sequence_oracle,
        }
    }
}

#[derive(Debug, Serialize)]
pub struct BackendFaultProfileReport {
    pub schema: &'static str,
    pub backend: BackendFaultKind,
    pub status: &'static str,
    pub configured_seeds: Vec<u64>,
    pub scenarios: Vec<BackendFaultScenarioReport>,
    pub composition_witness: BackendFaultCompositionWitness,
    pub coverage: BackendFaultCoverage,
    #[serde(skip)]
    pub report_path: PathBuf,
}

#[derive(Debug, Serialize)]
pub struct BackendFaultCoverage {
    pub complete_scenario_set: Vec<BackendFaultScenarioKind>,
    pub exercised_scenarios: Vec<BackendFaultScenarioKind>,
    pub dropped_scenarios: Vec<BackendFaultScenarioKind>,
    pub bounded_prefix_commits_per_seed: &'static str,
    pub bounded_composition_policy: &'static str,
}

/// Explicit multi-arm plan selected from one generated workload.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct BackendFaultCompositionPlan {
    pub schema: String,
    pub workload_seed: u64,
    pub workload_profile: String,
    pub workload_max_boundaries: usize,
    pub workload_id: String,
    pub selection_policy: String,
    pub max_attempts: usize,
    pub arms: Vec<GeneratedBackendFaultArm>,
}

/// One script arm and the generated boundary that selected it.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct GeneratedBackendFaultArm {
    pub source_boundary_id: String,
    pub arm: BackendFaultArm,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct BackendFaultCompositionAttempt {
    pub attempt: usize,
    pub outcome: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub store_error_variant: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub committed_head_revision: Option<u64>,
    pub fired_observations: Vec<BackendFaultObservation>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct BackendFaultCompositionRun {
    pub label: String,
    pub selected_arm_indices: Vec<usize>,
    pub attempts: Vec<BackendFaultCompositionAttempt>,
    pub operation_failed: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub failure_class: Option<String>,
    pub durable_prefix_revision: u64,
    pub final_reopened_head_revision: u64,
    pub injection_observations: Vec<BackendFaultObservation>,
}

#[derive(Clone, Debug, Serialize)]
pub struct BackendFaultCompositionWitness {
    pub schema: &'static str,
    pub plan: BackendFaultCompositionPlan,
    pub zero_arm_control: BackendFaultCompositionRun,
    pub single_arm_controls: Vec<BackendFaultCompositionRun>,
    pub paired: BackendFaultCompositionRun,
    pub repeated_paired: BackendFaultCompositionRun,
    pub repeat_matches: bool,
    pub oracle: BackendFaultOracle,
    pub replay_command: String,
}

#[derive(Debug, Serialize)]
pub struct BackendFaultScenarioReport {
    pub seed: u64,
    pub scenario: BackendFaultScenarioKind,
    pub prefix_commits: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub injected_fault: Option<BackendFault>,
    pub injection_observations: Vec<BackendFaultObservation>,
    pub oracles: Vec<BackendFaultOracle>,
    pub replay_command: String,
}

#[derive(Clone, Debug, Serialize)]
pub struct BackendFaultOracle {
    pub oracle_id: crate::trace::OracleId<'static>,
    pub status: &'static str,
    pub assertion: &'static str,
    pub evidence: Value,
}

#[derive(Debug, Serialize)]
struct BackendFaultFailurePackage<'a> {
    schema: &'static str,
    backend: BackendFaultKind,
    seed: u64,
    scenario: BackendFaultScenarioKind,
    oracle_id: &'a str,
    reason: &'a str,
    database_root: String,
    prefix_commits: usize,
    injected_fault: Option<BackendFault>,
    exact_replay_command: String,
}

#[derive(Debug)]
struct ScenarioFailure {
    oracle_id: crate::trace::OracleId<'static>,
    reason: String,
}

impl ScenarioFailure {
    fn harness(backend: BackendFaultKind, reason: impl Into<String>) -> Self {
        Self {
            oracle_id: naming(backend).harness_oracle,
            reason: reason.into(),
        }
    }

    fn oracle(
        backend: BackendFaultKind,
        kind: BackendFaultScenarioKind,
        reason: impl Into<String>,
    ) -> Self {
        Self {
            oracle_id: kind.oracle_id(backend),
            reason: reason.into(),
        }
    }
}

pub fn sqlite_fault_seeds(count: usize) -> Vec<u64> {
    (0..count)
        .map(|index| DEFAULT_SQLITE_FAULT_SEED_BASE.wrapping_add(index as u64))
        .collect()
}

pub async fn run_backend_fault_profile(
    backend: BackendFaultKind,
    artifact_root: impl AsRef<Path>,
    seeds: &[u64],
) -> Result<Option<BackendFaultProfileReport>, String> {
    if seeds.is_empty() {
        return Err(format!(
            "{} fault profile requires at least one seed",
            backend.name()
        ));
    }
    let artifact_root = artifact_root.as_ref();
    std::fs::create_dir_all(artifact_root).map_err(|err| err.to_string())?;
    let Some(lane) = BackendFaultLane::open(backend).await? else {
        return Ok(None);
    };
    let mut scenarios = Vec::with_capacity(seeds.len());
    for &seed in seeds {
        match run_seed(&lane, artifact_root, seed).await {
            Ok(report) => scenarios.push(report),
            Err(failure) => {
                let failure_path = persist_failure(backend, artifact_root, seed, &failure)?;
                return Err(format!(
                    "{} substrate fault seed {seed} failed oracle `{}`: {}; reproduction: {}; replay with `{}`",
                    backend.name(),
                    failure.oracle_id,
                    failure.reason,
                    failure_path.display(),
                    exact_replay_command(backend, &artifact_root.join("replay"), seed),
                ));
            }
        }
    }

    let exercised = scenarios
        .iter()
        .map(|scenario| scenario.scenario)
        .collect::<BTreeSet<_>>();
    let dropped = BackendFaultScenarioKind::ALL
        .iter()
        .copied()
        .filter(|scenario| !exercised.contains(scenario))
        .collect::<Vec<_>>();
    if !dropped.is_empty() {
        eprintln!(
            "{} substrate fault coverage dropped by configured seed bound: {dropped:?}",
            backend.name()
        );
    }
    let composition_witness = run_composition_witness(&lane, artifact_root, seeds[0]).await?;
    let report_path = artifact_root.join(backend.report_file_name());
    let report = BackendFaultProfileReport {
        schema: naming(backend).profile_schema,
        backend,
        status: "passed",
        configured_seeds: seeds.to_vec(),
        scenarios,
        composition_witness,
        coverage: BackendFaultCoverage {
            complete_scenario_set: BackendFaultScenarioKind::ALL.to_vec(),
            exercised_scenarios: exercised.into_iter().collect(),
            dropped_scenarios: dropped,
            bounded_prefix_commits_per_seed: "1..=8 selected deterministically by seed",
            bounded_composition_policy: "zero arms, each single arm, both arms, and repeated both arms; at most two commit attempts per run, the kth arm faulting the kth attempt",
        },
        report_path: report_path.clone(),
    };
    write_json(&report_path, &report)?;
    Ok(Some(report))
}

fn generated_multi_arm_plan(
    backend: BackendFaultKind,
    seed: u64,
) -> Result<BackendFaultCompositionPlan, String> {
    const PROFILE: &str = "fast-random";
    const MAX_BOUNDARIES: usize = 24;

    let workload = crate::generator::generate_workload(seed, PROFILE, MAX_BOUNDARIES)
        .map_err(|error| error.to_string())?;
    let retryable = workload
        .boundaries
        .iter()
        .find(|boundary| {
            boundary.kind == crate::scheduler::BoundaryKind::BackendFailure
                && boundary.payload.get("fault").and_then(Value::as_str)
                    == Some(BackendFault::Refused.name())
        })
        .ok_or_else(|| "generated workload has no refused backend-failure boundary".to_string())?;
    let terminal = workload
        .boundaries
        .iter()
        .find(|boundary| {
            boundary.kind == crate::scheduler::BoundaryKind::BackendFailure
                && boundary.payload.get("fault").and_then(Value::as_str)
                    == Some(BackendFault::ReplyLost.name())
        })
        .ok_or_else(|| {
            "generated workload has no lost-reply backend-failure boundary".to_string()
        })?;
    let faults = match workload.seed % 4 {
        0 => [BackendFault::Refused, BackendFault::ReplyLost],
        1 => [BackendFault::ReplyLost, BackendFault::Refused],
        2 => [BackendFault::Refused, BackendFault::Refused],
        _ => [BackendFault::ReplyLost, BackendFault::ReplyLost],
    };
    let arms = vec![
        GeneratedBackendFaultArm {
            source_boundary_id: retryable.boundary_id.clone(),
            arm: BackendFaultArm::new(
                seed ^ retryable.at.rotate_left(17) ^ 0x4649_4731_3135_3501,
                faults[0],
            ),
        },
        GeneratedBackendFaultArm {
            source_boundary_id: terminal.boundary_id.clone(),
            arm: BackendFaultArm::new(
                seed ^ terminal.at.rotate_left(17) ^ 0x4649_4731_3135_3502,
                faults[1],
            ),
        },
    ];
    Ok(BackendFaultCompositionPlan {
        schema: naming(backend).plan_schema.to_string(),
        workload_seed: seed,
        workload_profile: PROFILE.to_string(),
        workload_max_boundaries: MAX_BOUNDARIES,
        workload_id: workload.workload_id,
        selection_policy: "the generated workload seed selects one of the four ordered pairs of commit faults; its first refused and first lost-reply backend boundaries supply arm identities; the kth selected arm faults the kth commit attempt".to_string(),
        max_attempts: 2,
        arms,
    })
}

async fn run_composition_witness(
    lane: &BackendFaultLane,
    artifact_root: &Path,
    seed: u64,
) -> Result<BackendFaultCompositionWitness, String> {
    let backend = lane.kind();
    let plan = generated_multi_arm_plan(backend, seed)?;
    let zero_arm_control =
        run_composition_case(lane, artifact_root, &plan, "zero-arms", Vec::new()).await?;
    let mut single_arm_controls = Vec::with_capacity(plan.arms.len());
    for arm_index in 0..plan.arms.len() {
        single_arm_controls.push(
            run_composition_case(
                lane,
                artifact_root,
                &plan,
                &format!("single-arm-{arm_index}"),
                vec![arm_index],
            )
            .await?,
        );
    }
    let paired = run_composition_case(lane, artifact_root, &plan, "paired", vec![0, 1]).await?;
    let repeated_paired =
        run_composition_case(lane, artifact_root, &plan, "paired-repeat", vec![0, 1]).await?;
    let repeat_matches = paired.selected_arm_indices == repeated_paired.selected_arm_indices
        && paired.attempts == repeated_paired.attempts
        && paired.operation_failed == repeated_paired.operation_failed
        && paired.failure_class == repeated_paired.failure_class
        && paired.durable_prefix_revision == repeated_paired.durable_prefix_revision
        && paired.final_reopened_head_revision == repeated_paired.final_reopened_head_revision
        && paired.injection_observations == repeated_paired.injection_observations;
    let replay_command = exact_replay_command(backend, &artifact_root.join("replay"), seed);
    let mut witness = BackendFaultCompositionWitness {
        schema: naming(backend).composition_schema,
        plan,
        zero_arm_control,
        single_arm_controls,
        paired,
        repeated_paired,
        repeat_matches,
        oracle: BackendFaultOracle {
            oracle_id: naming(backend).multi_arm_composition_oracle,
            status: "passed",
            assertion: "the generated two-arm plan answers both attempts of a two-attempt operation a storage failure, publishing it once exactly when a reply was lost, while zero-arm and either single-arm controls commit, and repeating the seed reproduces fired identities, order, and storage outcome",
            evidence: Value::Null,
        },
        replay_command,
    };
    validate_composition_witness(&witness)?;
    witness.oracle.evidence = json!({
        "workload_seed": witness.plan.workload_seed,
        "workload_id": witness.plan.workload_id,
        "max_attempts": witness.plan.max_attempts,
        "paired_failure_class": witness.paired.failure_class,
        "paired_fired": witness.paired.injection_observations,
        "paired_final_head_revision": witness.paired.final_reopened_head_revision,
        "single_arm_final_head_revisions": witness
            .single_arm_controls
            .iter()
            .map(|control| control.final_reopened_head_revision)
            .collect::<Vec<_>>(),
        "zero_arm_final_head_revision": witness.zero_arm_control.final_reopened_head_revision,
        "repeat_matches": witness.repeat_matches,
    });
    Ok(witness)
}

async fn run_composition_case(
    lane: &BackendFaultLane,
    artifact_root: &Path,
    plan: &BackendFaultCompositionPlan,
    label: &str,
    selected_arm_indices: Vec<usize>,
) -> Result<BackendFaultCompositionRun, String> {
    let backend = lane.kind();
    let case_root = artifact_root.join("composition").join(label);
    if case_root.exists() {
        std::fs::remove_dir_all(&case_root).map_err(|error| error.to_string())?;
    }
    std::fs::create_dir_all(&case_root).map_err(|error| error.to_string())?;
    let factory = lane.store(&case_root).await?;
    let session_id = SessionId::from(format!(
        "lash-sim-{}-composition-{:016x}-{label}",
        lane.kind().name(),
        plan.workload_seed
    ));

    // Creation and the durable prefix go to the store itself; only the target
    // operation's attempts run under the script.
    let store = create_store(backend, Arc::clone(&factory), &session_id)
        .await
        .map_err(|failure| failure.reason)?;
    let mut state = RuntimeSessionState {
        session_id: session_id.clone(),
        ..RuntimeSessionState::new(SessionPolicy::new(
            lash_core::TurnBudget::Unbounded,
            lash_core::MaxToolCalls::new(1024),
        ))
    };
    let prefix =
        stamped_commit(backend, &state, "composition-prefix").map_err(|failure| failure.reason)?;
    let prefix_result = store
        .commit_runtime_state(prefix)
        .await
        .map_err(|error| error.to_string())?;
    state.apply_persisted_commit_result(prefix_result);
    let durable_prefix_revision = state.head_revision;
    state.turn_index += 1;
    let target =
        stamped_commit(backend, &state, "composition-target").map_err(|failure| failure.reason)?;

    let selected_arms = selected_arm_indices
        .iter()
        .map(|&index| {
            plan.arms
                .get(index)
                .map(|planned| planned.arm)
                .ok_or_else(|| format!("composition selected missing arm index {index}"))
        })
        .collect::<Result<Vec<_>, _>>()?;
    let script = BackendFaultScript::arm(Arc::clone(&factory), selected_arms);
    let scripted = script.store();

    let mut attempts = Vec::with_capacity(plan.max_attempts);
    let mut operation_failed = true;
    for attempt in 1..=plan.max_attempts {
        let observations_before = script.observations().len();
        let result = tokio::time::timeout(
            Duration::from_secs(5),
            scripted.commit_runtime_state(target.clone()),
        )
        .await
        .map_err(|_| {
            format!("composition case `{label}` attempt {attempt} hung for five seconds")
        })?;
        let observations = script.observations();
        let fired_observations = observations[observations_before..].to_vec();
        match result {
            Ok(result) => {
                attempts.push(BackendFaultCompositionAttempt {
                    attempt,
                    outcome: "committed".to_string(),
                    store_error_variant: None,
                    message: None,
                    committed_head_revision: Some(result.head_revision),
                    fired_observations,
                });
                operation_failed = false;
                break;
            }
            Err(error @ StoreError::StorageFailure { .. }) => {
                attempts.push(BackendFaultCompositionAttempt {
                    attempt,
                    outcome: "storage_failure".to_string(),
                    store_error_variant: Some(error.variant_name().to_string()),
                    message: Some(error.to_string()),
                    committed_head_revision: None,
                    fired_observations,
                });
            }
            Err(other) => {
                return Err(format!(
                    "composition case `{label}` attempt {attempt} returned non-storage error {other:?}"
                ));
            }
        }
    }
    let injection_observations = script.observations();
    // Dropping the script fails the run when a selected arm never fired.
    drop((scripted, script));

    drop(store);
    let reopened = open_store(backend, factory, &session_id)
        .await
        .map_err(|failure| failure.reason)?;
    let final_state = reopened
        .load_session_head_meta(&session_id)
        .await
        .map_err(|error| error.to_string())?
        .ok_or_else(|| format!("composition case `{label}` lost the durable prefix"))?;
    Ok(BackendFaultCompositionRun {
        label: label.to_string(),
        selected_arm_indices,
        attempts,
        operation_failed,
        failure_class: operation_failed.then(|| "retry_budget_exhausted".to_string()),
        durable_prefix_revision,
        final_reopened_head_revision: final_state.head_revision,
        injection_observations,
    })
}

fn validate_composition_witness(witness: &BackendFaultCompositionWitness) -> Result<(), String> {
    if witness.plan.arms.len() != 2 || witness.plan.max_attempts != 2 {
        return Err("composition witness requires exactly two arms and two attempts".to_string());
    }
    if witness.zero_arm_control.operation_failed
        || witness.zero_arm_control.attempts.len() != 1
        || witness.zero_arm_control.attempts[0].outcome != "committed"
        || !witness.zero_arm_control.injection_observations.is_empty()
        || witness.zero_arm_control.final_reopened_head_revision
            != witness.zero_arm_control.durable_prefix_revision + 1
    {
        return Err("zero-arm control must commit on its first attempt".to_string());
    }
    if witness.single_arm_controls.len() != witness.plan.arms.len()
        || witness
            .single_arm_controls
            .iter()
            .enumerate()
            .any(|(arm_index, control)| {
                control.operation_failed
                    || control.selected_arm_indices != [arm_index]
                    || control.attempts.len() != 2
                    || control.attempts[0].outcome != "storage_failure"
                    || control.attempts[1].outcome != "committed"
                    || control.injection_observations.len() != 1
                    || !observation_matches_arm(
                        &control.injection_observations[0],
                        &witness.plan.arms[arm_index].arm,
                    )
                    || control.final_reopened_head_revision != control.durable_prefix_revision + 1
            })
    {
        return Err("each single arm must fail once and commit on retry".to_string());
    }
    if !witness.paired.operation_failed
        || witness.paired.failure_class.as_deref() != Some("retry_budget_exhausted")
        || witness.paired.attempts.len() != witness.plan.max_attempts
        || witness
            .paired
            .attempts
            .iter()
            .any(|attempt| attempt.outcome != "storage_failure")
        || witness.paired.injection_observations.len() != witness.plan.arms.len()
        || witness
            .paired
            .injection_observations
            .iter()
            .zip(&witness.plan.arms)
            .enumerate()
            .any(|(arm_index, (observation, planned))| {
                observation.arm_index != arm_index
                    || !observation_matches_arm(observation, &planned.arm)
            })
        || witness.paired.final_reopened_head_revision
            != witness.paired.durable_prefix_revision
                + u64::from(
                    witness
                        .paired
                        .injection_observations
                        .iter()
                        .any(|observation| observation.fault.commit_stands()),
                )
    {
        return Err(
            "paired arms must exhaust both attempts in plan order, publishing the operation once exactly when a reply was lost"
                .to_string(),
        );
    }
    if !witness.repeat_matches {
        return Err(
            "repeating the same generated seed must reproduce arm order and storage outcome"
                .to_string(),
        );
    }
    Ok(())
}

fn observation_matches_arm(observation: &BackendFaultObservation, arm: &BackendFaultArm) -> bool {
    observation.seed == arm.seed
        && observation.fault == arm.fault
        && observation.operation == crate::backend_fault::FAULTED_OPERATION.name()
}

async fn run_seed(
    lane: &BackendFaultLane,
    artifact_root: &Path,
    seed: u64,
) -> Result<BackendFaultScenarioReport, ScenarioFailure> {
    let backend = lane.kind();
    let scenario = BackendFaultScenarioKind::for_seed(seed);
    let prefix_commits = 1 + ((seed >> 2) as usize % 8);
    let seed_root = artifact_root.join(format!("seed-{seed:016x}"));
    if seed_root.exists() {
        std::fs::remove_dir_all(&seed_root)
            .map_err(|err| ScenarioFailure::harness(backend, format!("reset seed root: {err}")))?;
    }
    std::fs::create_dir_all(&seed_root)
        .map_err(|err| ScenarioFailure::harness(backend, format!("create seed root: {err}")))?;
    let factory = lane
        .store(&seed_root)
        .await
        .map_err(|error| ScenarioFailure::harness(backend, error))?;
    let session_id = SessionId::from(format!("lash-sim-{}-fault-{seed:016x}", backend.name()));
    let mut store = create_store(backend, Arc::clone(&factory), &session_id).await?;
    let mut state = RuntimeSessionState {
        session_id: session_id.clone(),
        ..RuntimeSessionState::new(lash_core::SessionPolicy::new(
            lash_core::TurnBudget::Unbounded,
            lash_core::MaxToolCalls::new(1024),
        ))
    };

    for index in 0..prefix_commits {
        state.turn_index = index;
        let commit = stamped_commit(backend, &state, &format!("prefix-{index:03}"))?;
        let result = store.commit_runtime_state(commit).await.map_err(|err| {
            ScenarioFailure::harness(backend, format!("prefix commit {index}: {err}"))
        })?;
        state.apply_persisted_commit_result(result);
    }
    let durable_prefix_revision = state.head_revision;
    let mut target_state = state.clone();
    target_state.turn_index = prefix_commits;
    let target_commit = stamped_commit(backend, &target_state, "target")?;
    let replay_command = exact_replay_command(backend, &artifact_root.join("replay"), seed);

    let mut oracles = Vec::new();
    let mut injection_observations = Vec::new();
    let injected_fault = scenario.fault();
    if let Some(fault) = injected_fault {
        let script =
            BackendFaultScript::arm(Arc::clone(&factory), [BackendFaultArm::new(seed, fault)]);
        let injected = tokio::time::timeout(
            Duration::from_secs(5),
            script.store().commit_runtime_state(target_commit.clone()),
        )
        .await
        .map_err(|_| {
            ScenarioFailure::oracle(backend, scenario, "injected commit hung for five seconds")
        })?;
        let error_message = match injected {
            Err(StoreError::StorageFailure { message, .. }) => message,
            Err(other) => {
                return Err(ScenarioFailure::oracle(
                    backend,
                    scenario,
                    format!("injected commit returned non-storage-failure error {other:?}"),
                ));
            }
            Ok(result) => {
                return Err(ScenarioFailure::oracle(
                    backend,
                    scenario,
                    format!(
                        "injected commit unexpectedly succeeded at head revision {}",
                        result.head_revision
                    ),
                ));
            }
        };
        let observations = script.observations();
        if observations.len() != 1 || observations[0].seed != seed || observations[0].fault != fault
        {
            return Err(ScenarioFailure::oracle(
                backend,
                scenario,
                format!("fault did not fire exactly once as armed: {observations:?}"),
            ));
        }
        injection_observations = observations;
        oracles.push(BackendFaultOracle {
            oracle_id: naming(backend).typed_error_oracle,
            status: "passed",
            assertion: "the scripted fault answers StoreError::StorageFailure within the timeout rather than hanging or panicking",
            evidence: json!({
                "store_error_variant": "StorageFailure",
                "message": error_message,
                "timeout_seconds": 5,
            }),
        });

        drop(store);
        store = open_store(backend, Arc::clone(&factory), &session_id).await?;
        let after_fault = store
            .load_session_head_meta(&session_id)
            .await
            .map_err(|err| ScenarioFailure::harness(backend, format!("load after fault: {err}")))?
            .ok_or_else(|| {
                ScenarioFailure::oracle(backend, scenario, "committed prefix disappeared")
            })?;
        let expected_head = durable_prefix_revision + u64::from(fault.commit_stands());
        if after_fault.head_revision != expected_head {
            return Err(ScenarioFailure::oracle(
                backend,
                scenario,
                format!(
                    "a {} commit left the durable head at {}, expected {expected_head} over prefix {durable_prefix_revision}",
                    fault.name(),
                    after_fault.head_revision
                ),
            ));
        }
        oracles.push(BackendFaultOracle {
            oracle_id: naming(backend).preserves_committed_work_oracle,
            status: "passed",
            assertion: "reopen retains every commit preceding the fault; a refused commit publishes nothing and a commit whose reply was lost stands once",
            evidence: json!({
                "prefix_head_revision": durable_prefix_revision,
                "reopened_head_revision": after_fault.head_revision,
                "faulted_commit_published": fault.commit_stands(),
            }),
        });
    } else {
        drop(store);
        store = tokio::time::timeout(
            Duration::from_secs(5),
            open_store(backend, Arc::clone(&factory), &session_id),
        )
        .await
        .map_err(|_| {
            ScenarioFailure::oracle(backend, scenario, "reopen hung for five seconds")
        })??;
        let reopened = store
            .load_session_head_meta(&session_id)
            .await
            .map_err(|err| ScenarioFailure::harness(backend, format!("load after reopen: {err}")))?
            .ok_or_else(|| {
                ScenarioFailure::oracle(backend, scenario, "committed prefix disappeared")
            })?;
        if reopened.head_revision != durable_prefix_revision {
            return Err(ScenarioFailure::oracle(
                backend,
                scenario,
                format!(
                    "reopen changed durable head from {durable_prefix_revision} to {}",
                    reopened.head_revision
                ),
            ));
        }
        oracles.push(BackendFaultOracle {
            oracle_id: naming(backend).reopen_preserves_committed_work_oracle,
            status: "passed",
            assertion: "closing the live handle and reopening mid-sequence retains the exact committed head without a hang or panic",
            evidence: json!({
                "prefix_head_revision": durable_prefix_revision,
                "reopened_head_revision": reopened.head_revision,
                "timeout_seconds": 5,
            }),
        });
    }

    let first = store
        .commit_runtime_state(target_commit.clone())
        .await
        .map_err(|err| {
            ScenarioFailure::oracle(backend, scenario, format!("target retry failed: {err}"))
        })?;
    let duplicate = store
        .commit_runtime_state(target_commit)
        .await
        .map_err(|err| {
            ScenarioFailure::oracle(backend, scenario, format!("duplicate retry failed: {err}"))
        })?;
    if first.head_revision != durable_prefix_revision + 1
        || duplicate.head_revision != first.head_revision
        || duplicate.checkpoint_ref != first.checkpoint_ref
    {
        return Err(ScenarioFailure::oracle(
            backend,
            scenario,
            format!(
                "idempotent retry diverged: prefix={durable_prefix_revision}, first={}, duplicate={}",
                first.head_revision, duplicate.head_revision
            ),
        ));
    }
    drop(store);
    let final_store = open_store(backend, factory, &session_id).await?;
    let final_read = final_store
        .load_session_head_meta(&session_id)
        .await
        .map_err(|err| ScenarioFailure::harness(backend, format!("final load: {err}")))?
        .ok_or_else(|| {
            ScenarioFailure::oracle(backend, scenario, "final committed state disappeared")
        })?;
    if final_read.head_revision != first.head_revision {
        return Err(ScenarioFailure::oracle(
            backend,
            scenario,
            format!(
                "final reopen changed head from {} to {}",
                first.head_revision, final_read.head_revision
            ),
        ));
    }
    oracles.push(BackendFaultOracle {
        oracle_id: naming(backend).no_duplicate_effect_oracle,
        status: "passed",
        assertion: "retrying the same durable operation returns its receipt and advances the head exactly once, whether or not the faulted attempt committed",
        evidence: json!({
            "prefix_head_revision": durable_prefix_revision,
            "first_result_head_revision": first.head_revision,
            "duplicate_result_head_revision": duplicate.head_revision,
            "final_reopened_head_revision": final_read.head_revision,
            "same_checkpoint_ref": duplicate.checkpoint_ref == first.checkpoint_ref,
        }),
    });
    oracles.push(BackendFaultOracle {
        oracle_id: scenario.oracle_id(backend),
        status: "passed",
        assertion: "the seed-selected commit fault preserves committed work, advances the retried operation exactly once, and returns any injected failure as a typed error",
        evidence: json!({
            "scenario": scenario,
            "injected_fault": injected_fault,
            "prefix_commits": prefix_commits,
            "durable_prefix_revision": durable_prefix_revision,
            "final_head_revision": final_read.head_revision,
        }),
    });

    Ok(BackendFaultScenarioReport {
        seed,
        scenario,
        prefix_commits,
        injected_fault,
        injection_observations,
        oracles,
        replay_command,
    })
}

fn stamped_commit(
    backend: BackendFaultKind,
    state: &RuntimeSessionState,
    operation_suffix: &str,
) -> Result<RuntimeCommit, ScenarioFailure> {
    RuntimeCommit::persisted_state_for_test(state)
        .with_operation(OperationId::turn(
            &state.session_id,
            format!("{}-fault-{operation_suffix}", backend.name()),
            "final",
        ))
        .map(|(commit, _)| commit)
        .map_err(|err| ScenarioFailure::harness(backend, format!("stamp runtime commit: {err}")))
}

fn request(session_id: &SessionId) -> SessionStoreCreateRequest {
    SessionStoreCreateRequest {
        owning_process_id: None,
        pending_observer_intents: Vec::new(),
        session_id: SessionId::from(session_id.to_string()),
        relation: SessionRelation::Root,
        config: SessionPolicy::new(
            lash_core::TurnBudget::Unbounded,
            lash_core::MaxToolCalls::new(1024),
        )
        .into(),
        head: SessionCreationHead::Config,
    }
}

async fn create_store(
    backend: BackendFaultKind,
    factory: Arc<dyn DeploymentStore>,
    session_id: &SessionId,
) -> Result<Arc<dyn RuntimeStore>, ScenarioFailure> {
    factory
        .admit_session(&request(session_id))
        .await
        .map_err(|err| ScenarioFailure::harness(backend, format!("create store: {err}")))?;
    let store: Arc<dyn RuntimeStore> = factory;
    Ok(store)
}

async fn open_store(
    backend: BackendFaultKind,
    factory: Arc<dyn DeploymentStore>,
    session_id: &SessionId,
) -> Result<Arc<dyn RuntimeStore>, ScenarioFailure> {
    match factory
        .lookup_session(session_id)
        .await
        .map_err(|err| ScenarioFailure::harness(backend, format!("open store: {err}")))?
    {
        lash_core::SessionLookup::Live(_) => {
            let store: Arc<dyn RuntimeStore> = factory;
            Ok(store)
        }
        lash_core::SessionLookup::Absent | lash_core::SessionLookup::Deleted => Err(
            ScenarioFailure::harness(backend, "reopened store did not exist"),
        ),
    }
}

fn persist_failure(
    backend: BackendFaultKind,
    artifact_root: &Path,
    seed: u64,
    failure: &ScenarioFailure,
) -> Result<PathBuf, String> {
    let failure_root = artifact_root
        .join("failures")
        .join(format!("seed-{seed:016x}"));
    std::fs::create_dir_all(&failure_root).map_err(|err| err.to_string())?;
    let path = failure_root.join("reproduction.json");
    let package = BackendFaultFailurePackage {
        schema: naming(backend).failure_schema,
        backend,
        seed,
        scenario: BackendFaultScenarioKind::for_seed(seed),
        oracle_id: failure.oracle_id.id,
        reason: &failure.reason,
        database_root: artifact_root
            .join(format!("seed-{seed:016x}"))
            .join("store")
            .display()
            .to_string(),
        prefix_commits: 1 + ((seed >> 2) as usize % 8),
        injected_fault: BackendFaultScenarioKind::for_seed(seed).fault(),
        exact_replay_command: exact_replay_command(backend, &artifact_root.join("replay"), seed),
    };
    write_json(&path, &package)?;
    Ok(path)
}

fn write_json(path: &Path, value: &impl Serialize) -> Result<(), String> {
    let mut bytes = serde_json::to_vec_pretty(value).map_err(|err| err.to_string())?;
    bytes.push(b'\n');
    std::fs::write(path, bytes).map_err(|err| err.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generated_multi_arm_plan_round_trips_with_stable_identity_and_order() {
        let plan =
            generated_multi_arm_plan(BackendFaultKind::Sqlite, DEFAULT_SQLITE_FAULT_SEED_BASE)
                .expect("generated multi-arm plan");
        let repeated =
            generated_multi_arm_plan(BackendFaultKind::Sqlite, DEFAULT_SQLITE_FAULT_SEED_BASE)
                .expect("repeated generated multi-arm plan");
        let encoded = serde_json::to_value(&plan).expect("encode plan");
        let decoded: BackendFaultCompositionPlan =
            serde_json::from_value(encoded).expect("decode plan");

        assert_eq!(decoded, plan);
        assert_eq!(repeated, plan);
        assert_eq!(plan.max_attempts, 2);
        assert_eq!(plan.arms.len(), 2);
        assert_eq!(plan.arms[0].arm.fault, BackendFault::Refused);
        assert_eq!(plan.arms[1].arm.fault, BackendFault::ReplyLost);
        assert_ne!(
            plan.arms[0].source_boundary_id,
            plan.arms[1].source_boundary_id
        );

        let schedules = (0..4)
            .map(|offset| {
                generated_multi_arm_plan(
                    BackendFaultKind::Sqlite,
                    DEFAULT_SQLITE_FAULT_SEED_BASE + offset,
                )
                .expect("generated multi-arm schedule")
                .arms
                .into_iter()
                .map(|planned| planned.arm.fault)
                .collect::<Vec<_>>()
            })
            .collect::<Vec<_>>();
        assert_eq!(
            schedules,
            vec![
                vec![BackendFault::Refused, BackendFault::ReplyLost],
                vec![BackendFault::ReplyLost, BackendFault::Refused],
                vec![BackendFault::Refused, BackendFault::Refused],
                vec![BackendFault::ReplyLost, BackendFault::ReplyLost],
            ]
        );
    }

    #[tokio::test]
    async fn generated_two_arm_witness_requires_both_arms_to_exhaust_retry_budget() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let lane = BackendFaultLane::open(BackendFaultKind::SqliteMemory)
            .await
            .expect("open SQLite lane")
            .expect("the SQLite lane is always configured");
        let witness = run_composition_witness(&lane, tmp.path(), DEFAULT_SQLITE_FAULT_SEED_BASE)
            .await
            .expect("two-arm witness");

        validate_composition_witness(&witness).expect("valid witness");
        assert!(witness.paired.operation_failed);
        assert_eq!(witness.paired.attempts.len(), witness.plan.max_attempts);
        assert_eq!(witness.single_arm_controls.len(), witness.plan.arms.len());
        assert!(
            witness
                .single_arm_controls
                .iter()
                .all(|control| !control.operation_failed)
        );
        assert!(!witness.zero_arm_control.operation_failed);
        assert!(witness.repeat_matches);

        let mut omitted_arm_witness = witness.clone();
        omitted_arm_witness.paired = run_composition_case(
            &lane,
            tmp.path(),
            &witness.plan,
            "paired-omitted-second-arm",
            vec![0],
        )
        .await
        .expect("real SQLite run with omitted second arm");
        let error = validate_composition_witness(&omitted_arm_witness)
            .expect_err("the oracle must reject a real run missing its second arm");
        assert!(error.contains("paired arms must exhaust"), "{error}");
    }

    #[tokio::test]
    async fn bounded_seed_set_covers_every_sqlite_fault_and_oracle() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let report = run_backend_fault_profile(
            BackendFaultKind::SqliteMemory,
            tmp.path(),
            &sqlite_fault_seeds(4),
        )
        .await
        .expect("SQLite fault profile")
        .expect("the SQLite lane is always configured");
        assert_eq!(report.status, "passed");
        assert!(report.coverage.dropped_scenarios.is_empty());
        assert_eq!(report.scenarios.len(), 4);
        assert!(report.report_path.exists());
        for scenario in report.scenarios {
            assert!(
                scenario
                    .oracles
                    .iter()
                    .all(|oracle| oracle.status == "passed")
            );
            if scenario.injected_fault.is_some() {
                assert_eq!(scenario.injection_observations.len(), 1);
            }
        }
    }

    /// The same bounded seed set on a real PostgreSQL: every commit-fault
    /// oracle holds there too.
    ///
    /// Ignored without the service gate; an explicit run requires PostgreSQL.
    #[tokio::test]
    #[ignore = "requires PostgreSQL; run with --include-ignored inside a pg16 gate"]
    async fn postgres_backend_fault_seed_set_covers_every_fault_and_oracle() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let Some(report) = run_backend_fault_profile(
            BackendFaultKind::Postgres,
            tmp.path(),
            &sqlite_fault_seeds(4),
        )
        .await
        .expect("Postgres fault profile") else {
            eprintln!("skipping: LASH_POSTGRES_DATABASE_URL is not configured");
            return;
        };
        assert_eq!(report.backend, BackendFaultKind::Postgres);
        assert_eq!(report.status, "passed");
        assert!(report.coverage.dropped_scenarios.is_empty());
        assert_eq!(report.scenarios.len(), 4);
        assert!(report.report_path.exists());
        for scenario in report.scenarios {
            assert!(
                scenario
                    .oracles
                    .iter()
                    .all(|oracle| oracle.status == "passed")
            );
            if scenario.injected_fault.is_some() {
                assert_eq!(scenario.injection_observations.len(), 1);
            }
        }
    }

    /// What one arm did on one backend: every call the script saw, and the
    /// session head the store kept after each step.
    #[derive(Debug, Eq, PartialEq)]
    struct ArmEffect {
        calls: Vec<String>,
        head_after_admission: u64,
        head_after_fault: u64,
        head_after_retry: u64,
    }

    /// Admit a session and commit once, all through a store armed with one
    /// `fault`, then retry the commit.
    async fn arm_effect(backend: BackendFaultKind, fault: BackendFault) -> ArmEffect {
        let tmp = tempfile::tempdir().expect("tempdir");
        let lane = BackendFaultLane::open(backend)
            .await
            .expect("open the lane")
            .expect("the lane is configured");
        let inner = lane.store(tmp.path()).await.expect("open the store");
        let script = BackendFaultScript::arm(Arc::clone(&inner), [BackendFaultArm::new(7, fault)]);
        let session_id = SessionId::from(format!("arm-scope-{}", fault.name()));
        let head = || async {
            inner
                .load_session_head_meta(&session_id)
                .await
                .expect("read the session head")
                .map_or(0, |meta| meta.head_revision)
        };
        // The admission is a write of its own: an arm on the commit leaves it alone.
        let store = create_store(backend, script.store(), &session_id)
            .await
            .map_err(|failure| failure.reason)
            .expect("admitting a session is not the armed commit");
        let head_after_admission = head().await;
        let state = RuntimeSessionState {
            session_id: session_id.clone(),
            ..RuntimeSessionState::new(SessionPolicy::new(
                lash_core::TurnBudget::Unbounded,
                lash_core::MaxToolCalls::new(1024),
            ))
        };
        let commit = stamped_commit(backend, &state, "armed")
            .map_err(|failure| failure.reason)
            .expect("stamp the commit");
        let faulted = store
            .commit_runtime_state(commit.clone())
            .await
            .expect_err("the armed commit is faulted");
        assert!(
            matches!(faulted, StoreError::StorageFailure { .. }),
            "{faulted:?}"
        );
        let head_after_fault = head().await;
        let retried = store
            .commit_runtime_state(commit)
            .await
            .expect("the retried commit is answered");
        assert_eq!(retried.head_revision, head().await);
        ArmEffect {
            calls: script.trace(),
            head_after_admission,
            head_after_fault,
            head_after_retry: retried.head_revision,
        }
    }

    /// The calls a one-arm script records for [`arm_effect`]'s workload.
    fn expected_calls(fault: BackendFault) -> Vec<&'static str> {
        let faulted_commit: &[&str] = match fault {
            BackendFault::Refused => &["lash-sim:commit_runtime_state#1 before fail(transient)"],
            BackendFault::ReplyLost => &[
                "lash-sim:commit_runtime_state#1 before entered",
                "lash-sim:commit_runtime_state#1 after fail(transient)",
            ],
        };
        [
            &[
                "lash-sim:admit_session#1 before entered",
                "lash-sim:admit_session#1 after returned",
            ],
            faulted_commit,
            &[
                "lash-sim:commit_runtime_state#2 before entered",
                "lash-sim:commit_runtime_state#2 after returned",
            ],
        ]
        .concat()
    }

    fn assert_arm_effect(fault: BackendFault, effect: &ArmEffect) {
        assert_eq!(effect.calls, expected_calls(fault));
        assert_eq!(
            effect.head_after_fault,
            effect.head_after_admission + u64::from(fault.commit_stands()),
            "a {} commit is durable exactly when its reply was lost",
            fault.name()
        );
        assert_eq!(effect.head_after_retry, effect.head_after_admission + 1);
    }

    /// FIG-4793: an arm names a call of `commit_runtime_state`, so it faults
    /// that call and no other write, and the same one on every backend.
    #[tokio::test]
    async fn an_arm_faults_the_same_commit_on_sqlite_memory_and_file() {
        for fault in [BackendFault::Refused, BackendFault::ReplyLost] {
            let memory = arm_effect(BackendFaultKind::SqliteMemory, fault).await;
            assert_arm_effect(fault, &memory);
            let file = arm_effect(BackendFaultKind::Sqlite, fault).await;
            assert_eq!(file, memory, "SQLite file and memory diverge on {fault:?}");
        }
    }

    /// The PostgreSQL leg of the same law.
    #[tokio::test]
    #[ignore = "requires PostgreSQL; run with --include-ignored inside a pg16 gate"]
    async fn postgres_backend_fault_arm_faults_the_same_commit_as_sqlite() {
        for fault in [BackendFault::Refused, BackendFault::ReplyLost] {
            let postgres = arm_effect(BackendFaultKind::Postgres, fault).await;
            assert_arm_effect(fault, &postgres);
            let sqlite = arm_effect(BackendFaultKind::SqliteMemory, fault).await;
            assert_eq!(
                postgres, sqlite,
                "PostgreSQL and SQLite diverge on {fault:?}"
            );
        }
    }

    #[test]
    fn oracle_failure_is_persisted_with_exact_seed_replay() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let failure = ScenarioFailure::oracle(
            BackendFaultKind::Sqlite,
            BackendFaultScenarioKind::LostCommitReply,
            "deliberate oracle failure",
        );
        let path = persist_failure(
            BackendFaultKind::Sqlite,
            tmp.path(),
            DEFAULT_SQLITE_FAULT_SEED_BASE + 2,
            &failure,
        )
        .expect("persist failure");
        let body = std::fs::read_to_string(path).expect("failure package");
        assert!(body.contains("deliberate oracle failure"));
        assert!(body.contains("--seed 140050434"));
        assert!(body.contains("sim.oracle.sqlite-lost-commit-reply.v1"));
        assert!(body.contains("lash.sim.sqlite-substrate-fault-failure.v1"));
        assert!(body.contains("backend-faults --backend sqlite"));
    }

    #[test]
    fn a_postgres_failure_package_replays_on_postgres() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let failure = ScenarioFailure::oracle(
            BackendFaultKind::Postgres,
            BackendFaultScenarioKind::LostCommitReply,
            "deliberate oracle failure",
        );
        let path = persist_failure(
            BackendFaultKind::Postgres,
            tmp.path(),
            DEFAULT_SQLITE_FAULT_SEED_BASE + 2,
            &failure,
        )
        .expect("persist failure");
        let body = std::fs::read_to_string(path).expect("failure package");
        assert!(body.contains("backend-faults --backend postgres"));
        assert!(body.contains("--seed 140050434"));
        assert!(body.contains("sim.oracle.postgres-lost-commit-reply.v1"));
        assert!(body.contains("lash.sim.postgres-substrate-fault-failure.v1"));
        assert!(!body.contains("sqlite"));
    }

    #[test]
    fn every_backend_fault_oracle_id_is_declared_in_the_inventory() {
        for backend in BackendFaultKind::ALL {
            let names = naming(backend);
            let mut ids = vec![
                names.harness_oracle,
                names.typed_error_oracle,
                names.preserves_committed_work_oracle,
                names.reopen_preserves_committed_work_oracle,
                names.no_duplicate_effect_oracle,
                names.multi_arm_composition_oracle,
            ];
            ids.extend(
                BackendFaultScenarioKind::ALL
                    .iter()
                    .map(|scenario| scenario.oracle_id(backend)),
            );
            for id in ids {
                assert!(
                    id.class == crate::trace::OracleObservationClass::RealObservation,
                    "`{id}` is missing from the declared oracle inventory"
                );
            }
        }
    }
}
