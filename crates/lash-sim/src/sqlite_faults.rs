use lash_sansio::SessionId;
use std::collections::BTreeSet;
use std::num::NonZeroU64;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use crate::backend_fault::{
    BackendFaultArm, BackendFaultKind, BackendFaultLane, BackendFaultObservation, BackendFaultPoint,
};
use lash_core::{
    OperationId, RuntimeCommit, RuntimePersistence, RuntimeSessionState, SessionPolicy,
    SessionRelation, SessionStoreCreateRequest, SessionStoreFactory, StoreError,
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

pub const DEFAULT_SQLITE_FAULT_SEED_BASE: u64 = 0x0000_0000_0859_0000;

/// The report schemas and fixed oracle ids of one backend's fault profile.
///
/// Every id a profile emits names the backend it judged, so a PostgreSQL
/// failure is never filed under a SQLite oracle and the declared inventory in
/// `crate::trace::oracle_observation_class` can list both. SQLite keeps the
/// exact strings the confidence gate and `crates/lash-sim/README.md` already
/// document.
struct BackendFaultNaming {
    profile_schema: &'static str,
    plan_schema: &'static str,
    composition_schema: &'static str,
    failure_schema: &'static str,
    harness_oracle: &'static str,
    typed_error_oracle: &'static str,
    preserves_committed_work_oracle: &'static str,
    reopen_preserves_committed_work_oracle: &'static str,
    no_duplicate_effect_oracle: &'static str,
    multi_arm_composition_oracle: &'static str,
}

const SQLITE_NAMING: BackendFaultNaming = BackendFaultNaming {
    profile_schema: "lash.sim.sqlite-substrate-faults.v2",
    plan_schema: "lash.sim.sqlite-fault-plan.v1",
    composition_schema: "lash.sim.sqlite-fault-composition.v1",
    failure_schema: "lash.sim.sqlite-substrate-fault-failure.v1",
    harness_oracle: "sim.oracle.sqlite-fault-harness.v1",
    typed_error_oracle: "sim.oracle.sqlite-fault-typed-error.v1",
    preserves_committed_work_oracle: "sim.oracle.sqlite-fault-preserves-committed-work.v1",
    reopen_preserves_committed_work_oracle: "sim.oracle.sqlite-reopen-preserves-committed-work.v1",
    no_duplicate_effect_oracle: "sim.oracle.sqlite-fault-no-duplicate-effect.v1",
    multi_arm_composition_oracle: "sim.oracle.sqlite-multi-arm-composition.v1",
};

const POSTGRES_NAMING: BackendFaultNaming = BackendFaultNaming {
    profile_schema: "lash.sim.postgres-substrate-faults.v2",
    plan_schema: "lash.sim.postgres-fault-plan.v1",
    composition_schema: "lash.sim.postgres-fault-composition.v1",
    failure_schema: "lash.sim.postgres-substrate-fault-failure.v1",
    harness_oracle: "sim.oracle.postgres-fault-harness.v1",
    typed_error_oracle: "sim.oracle.postgres-fault-typed-error.v1",
    preserves_committed_work_oracle: "sim.oracle.postgres-fault-preserves-committed-work.v1",
    reopen_preserves_committed_work_oracle: "sim.oracle.postgres-reopen-preserves-committed-work.v1",
    no_duplicate_effect_oracle: "sim.oracle.postgres-fault-no-duplicate-effect.v1",
    multi_arm_composition_oracle: "sim.oracle.postgres-multi-arm-composition.v1",
};

const fn naming(backend: BackendFaultKind) -> &'static BackendFaultNaming {
    match backend {
        BackendFaultKind::Sqlite => &SQLITE_NAMING,
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
    AbortAfterBegin,
    AbortBeforeCommit,
    CommitIo,
    ReopenMidSequence,
}

impl BackendFaultScenarioKind {
    const ALL: [Self; 4] = [
        Self::AbortAfterBegin,
        Self::AbortBeforeCommit,
        Self::CommitIo,
        Self::ReopenMidSequence,
    ];

    fn for_seed(seed: u64) -> Self {
        match seed % 4 {
            0 => Self::AbortAfterBegin,
            1 => Self::AbortBeforeCommit,
            2 => Self::CommitIo,
            _ => Self::ReopenMidSequence,
        }
    }

    fn fault_point(self) -> Option<BackendFaultPoint> {
        match self {
            Self::AbortAfterBegin => Some(BackendFaultPoint::AfterBegin),
            Self::AbortBeforeCommit => Some(BackendFaultPoint::BeforeCommit),
            Self::CommitIo => Some(BackendFaultPoint::CommitIo),
            Self::ReopenMidSequence => None,
        }
    }

    fn oracle_id(self, backend: BackendFaultKind) -> &'static str {
        match (backend, self) {
            (BackendFaultKind::Sqlite, Self::AbortAfterBegin) => {
                "sim.oracle.sqlite-abort-after-begin.v1"
            }
            (BackendFaultKind::Sqlite, Self::AbortBeforeCommit) => {
                "sim.oracle.sqlite-abort-before-commit.v1"
            }
            (BackendFaultKind::Sqlite, Self::CommitIo) => "sim.oracle.sqlite-commit-io.v1",
            (BackendFaultKind::Sqlite, Self::ReopenMidSequence) => {
                "sim.oracle.sqlite-reopen-mid-sequence.v1"
            }
            (BackendFaultKind::Postgres, Self::AbortAfterBegin) => {
                "sim.oracle.postgres-abort-after-begin.v1"
            }
            (BackendFaultKind::Postgres, Self::AbortBeforeCommit) => {
                "sim.oracle.postgres-abort-before-commit.v1"
            }
            (BackendFaultKind::Postgres, Self::CommitIo) => "sim.oracle.postgres-commit-io.v1",
            (BackendFaultKind::Postgres, Self::ReopenMidSequence) => {
                "sim.oracle.postgres-reopen-mid-sequence.v1"
            }
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

/// One injector arm and the generated boundary that selected it.
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
    pub injected_fault: Option<BackendFaultPoint>,
    pub injection_observations: Vec<BackendFaultObservation>,
    pub oracles: Vec<BackendFaultOracle>,
    pub replay_command: String,
}

#[derive(Clone, Debug, Serialize)]
pub struct BackendFaultOracle {
    pub oracle_id: &'static str,
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
    injected_fault: Option<BackendFaultPoint>,
    exact_replay_command: String,
}

#[derive(Debug)]
struct ScenarioFailure {
    oracle_id: &'static str,
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

/// Runs the commit-boundary fault scenarios against the real SQLite substrate.
#[expect(
    clippy::expect_used,
    reason = "test support: the surrounding harness code establishes this value; a refusal panics the harness with its case name by design"
)]
pub async fn run_sqlite_fault_profile(
    artifact_root: impl AsRef<Path>,
    seeds: &[u64],
) -> Result<BackendFaultProfileReport, String> {
    Ok(
        run_backend_fault_profile(BackendFaultKind::Sqlite, artifact_root, seeds)
            .await?
            .expect("the SQLite lane is always configured"),
    )
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
            bounded_composition_policy: "zero arms, each single arm, both arms, and repeated both arms; at most two commit attempts per run",
        },
        report_path: report_path.clone(),
    };
    write_json(&report_path, &report)?;
    Ok(Some(report))
}

#[expect(
    clippy::expect_used,
    reason = "test support: the surrounding harness code establishes this value; a refusal panics the harness with its case name by design"
)]
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
                && boundary.payload.get("retryable").and_then(Value::as_bool) == Some(true)
        })
        .ok_or_else(|| {
            "generated workload has no retryable backend-failure boundary".to_string()
        })?;
    let terminal = workload
        .boundaries
        .iter()
        .find(|boundary| {
            boundary.kind == crate::scheduler::BoundaryKind::BackendFailure
                && boundary.payload.get("retryable").and_then(Value::as_bool) == Some(false)
        })
        .ok_or_else(|| "generated workload has no terminal backend-failure boundary".to_string())?;
    let points = match workload.seed % 3 {
        0 => [
            BackendFaultPoint::AfterBegin,
            BackendFaultPoint::BeforeCommit,
        ],
        1 => [BackendFaultPoint::AfterBegin, BackendFaultPoint::CommitIo],
        _ => [BackendFaultPoint::BeforeCommit, BackendFaultPoint::CommitIo],
    };
    let occurrence = NonZeroU64::new(1).expect("one is non-zero");
    let arms = vec![
        GeneratedBackendFaultArm {
            source_boundary_id: retryable.boundary_id.clone(),
            arm: BackendFaultArm::new(
                seed ^ retryable.at.rotate_left(17) ^ 0x4649_4731_3135_3501,
                points[0],
                occurrence,
            ),
        },
        GeneratedBackendFaultArm {
            source_boundary_id: terminal.boundary_id.clone(),
            arm: BackendFaultArm::new(
                seed ^ terminal.at.rotate_left(17) ^ 0x4649_4731_3135_3502,
                points[1],
                occurrence,
            ),
        },
    ];
    Ok(BackendFaultCompositionPlan {
        schema: naming(backend).plan_schema.to_string(),
        workload_seed: seed,
        workload_profile: PROFILE.to_string(),
        workload_max_boundaries: MAX_BOUNDARIES,
        workload_id: workload.workload_id,
        selection_policy: "the generated workload seed selects one of the three ordered pairs of distinct transaction points; its first retryable and first terminal backend boundaries supply arm identities; both target their first reached occurrence".to_string(),
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
            assertion: "the generated two-arm plan exhausts a two-attempt operation while zero-arm and either single-arm controls commit, and repeating the seed reproduces fired identities, order, and storage outcome",
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
    let (factory, injector) = lane.armed_factory(&case_root);
    let session_id = SessionId::from(format!(
        "lash-sim-{}-composition-{:016x}-{label}",
        lane.kind().name(),
        plan.workload_seed
    ));

    // Creation and the durable prefix happen before arming so setup writes
    // cannot consume an operation arm.
    let store = create_store(backend, Arc::clone(&factory), &session_id)
        .await
        .map_err(|failure| failure.reason)?;
    let mut state = RuntimeSessionState {
        session_id: session_id.clone(),
        ..RuntimeSessionState::new(SessionPolicy::new(lash_core::TurnBudget::Unbounded))
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
    injector.arm_many(selected_arms);

    let mut attempts = Vec::with_capacity(plan.max_attempts);
    let mut operation_failed = true;
    for attempt in 1..=plan.max_attempts {
        let observations_before = injector.observations().len();
        let result = tokio::time::timeout(
            Duration::from_secs(5),
            store.commit_runtime_state(target.clone()),
        )
        .await
        .map_err(|_| {
            format!("composition case `{label}` attempt {attempt} hung for five seconds")
        })?;
        let observations = injector.observations();
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
    if !injector.remaining_arms().is_empty() {
        return Err(format!(
            "composition case `{label}` did not consume every selected arm: {:?}",
            injector.remaining_arms()
        ));
    }

    drop(store);
    let reopened = open_store(backend, factory, &session_id)
        .await
        .map_err(|failure| failure.reason)?;
    let final_state = reopened
        .load_session()
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
        injection_observations: injector.observations(),
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
        || witness.paired.final_reopened_head_revision != witness.paired.durable_prefix_revision
    {
        return Err(
            "paired arms must exhaust both attempts in plan order without publishing the operation"
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
        && observation.point == arm.point
        && observation.point_occurrence == arm.occurrence.get()
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
    let (factory, injector) = lane.armed_factory(&seed_root);
    let session_id = SessionId::from(format!("lash-sim-{}-fault-{seed:016x}", backend.name()));
    let mut store = create_store(backend, Arc::clone(&factory), &session_id).await?;
    let mut state = RuntimeSessionState {
        session_id: session_id.clone(),
        ..RuntimeSessionState::new(lash_core::SessionPolicy::new(
            lash_core::TurnBudget::Unbounded,
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
    let replay_command = format!(
        "cargo run -p lash-sim -- sqlite-faults --out {} --seed {seed}",
        artifact_root.join("replay").display()
    );

    let mut oracles = Vec::new();
    let injected_fault = scenario.fault_point();
    if let Some(point) = injected_fault {
        injector.arm(seed, point);
        let injected = tokio::time::timeout(
            Duration::from_secs(5),
            store.commit_runtime_state(target_commit.clone()),
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
        let observations = injector.observations();
        if observations.len() != 1 || observations[0].seed != seed || observations[0].point != point
        {
            return Err(ScenarioFailure::oracle(
                backend,
                scenario,
                format!("fault did not fire exactly once as armed: {observations:?}"),
            ));
        }
        oracles.push(BackendFaultOracle {
            oracle_id: naming(backend).typed_error_oracle,
            status: "passed",
            assertion: "the substrate fault returns StoreError::StorageFailure within the timeout rather than hanging or panicking",
            evidence: json!({
                "store_error_variant": "StorageFailure",
                "message": error_message,
                "timeout_seconds": 5,
            }),
        });

        drop(store);
        store = open_store(backend, Arc::clone(&factory), &session_id).await?;
        let after_fault = store
            .load_session()
            .await
            .map_err(|err| ScenarioFailure::harness(backend, format!("load after fault: {err}")))?
            .ok_or_else(|| {
                ScenarioFailure::oracle(backend, scenario, "committed prefix disappeared")
            })?;
        if after_fault.head_revision != durable_prefix_revision {
            return Err(ScenarioFailure::oracle(
                backend,
                scenario,
                format!(
                    "fault changed durable head from {durable_prefix_revision} to {}",
                    after_fault.head_revision
                ),
            ));
        }
        oracles.push(BackendFaultOracle {
            oracle_id: naming(backend).preserves_committed_work_oracle,
            status: "passed",
            assertion: "reopen retains every commit preceding the fault and publishes none of the failed transaction",
            evidence: json!({
                "prefix_head_revision": durable_prefix_revision,
                "reopened_head_revision": after_fault.head_revision,
                "failed_transaction_published": false,
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
            .load_session()
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
        .load_session()
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
        assertion: "retrying the same durable operation returns its receipt and advances the head exactly once",
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
        assertion: "the seed-selected substrate fault preserves committed work, advances the retried operation exactly once, and returns any injected failure as a typed error",
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
        injection_observations: injector.observations(),
        oracles,
        replay_command,
    })
}

fn stamped_commit(
    backend: BackendFaultKind,
    state: &RuntimeSessionState,
    operation_suffix: &str,
) -> Result<RuntimeCommit, ScenarioFailure> {
    RuntimeCommit::persisted_state_for_test(state, &[])
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
        pending_observer_intents: Vec::new(),
        session_id: SessionId::from(session_id.to_string()),
        relation: SessionRelation::Root,
        policy: SessionPolicy::new(lash_core::TurnBudget::Unbounded),
    }
}

async fn create_store(
    backend: BackendFaultKind,
    factory: Arc<dyn SessionStoreFactory>,
    session_id: &SessionId,
) -> Result<Arc<dyn RuntimePersistence>, ScenarioFailure> {
    factory
        .create_store(&request(session_id))
        .await
        .map_err(|err| ScenarioFailure::harness(backend, format!("create store: {err}")))
}

async fn open_store(
    backend: BackendFaultKind,
    factory: Arc<dyn SessionStoreFactory>,
    session_id: &SessionId,
) -> Result<Arc<dyn RuntimePersistence>, ScenarioFailure> {
    factory
        .open_existing_store(&request(session_id))
        .await
        .map_err(|err| ScenarioFailure::harness(backend, format!("open store: {err}")))?
        .ok_or_else(|| ScenarioFailure::harness(backend, "reopened store did not exist"))
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
        oracle_id: failure.oracle_id,
        reason: &failure.reason,
        database_root: artifact_root
            .join(format!("seed-{seed:016x}"))
            .join("store")
            .display()
            .to_string(),
        prefix_commits: 1 + ((seed >> 2) as usize % 8),
        injected_fault: BackendFaultScenarioKind::for_seed(seed).fault_point(),
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
        assert_eq!(plan.arms[0].arm.point, BackendFaultPoint::AfterBegin);
        assert_eq!(plan.arms[1].arm.point, BackendFaultPoint::CommitIo);
        assert_ne!(
            plan.arms[0].source_boundary_id,
            plan.arms[1].source_boundary_id
        );

        let schedules = (0..3)
            .map(|offset| {
                generated_multi_arm_plan(
                    BackendFaultKind::Sqlite,
                    DEFAULT_SQLITE_FAULT_SEED_BASE + offset,
                )
                .expect("generated multi-arm schedule")
                .arms
                .into_iter()
                .map(|planned| planned.arm.point)
                .collect::<Vec<_>>()
            })
            .collect::<Vec<_>>();
        assert_eq!(
            schedules,
            vec![
                vec![BackendFaultPoint::AfterBegin, BackendFaultPoint::CommitIo],
                vec![BackendFaultPoint::BeforeCommit, BackendFaultPoint::CommitIo],
                vec![
                    BackendFaultPoint::AfterBegin,
                    BackendFaultPoint::BeforeCommit,
                ],
            ]
        );
    }

    #[tokio::test]
    async fn generated_two_arm_witness_requires_both_arms_to_exhaust_retry_budget() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let lane = BackendFaultLane::open(BackendFaultKind::Sqlite)
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
        .expect("real SQLite run with omitted second injector arm");
        let error = validate_composition_witness(&omitted_arm_witness)
            .expect_err("the oracle must reject a real run missing its second injector arm");
        assert!(error.contains("paired arms must exhaust"), "{error}");
    }

    #[tokio::test]
    async fn bounded_seed_set_covers_every_sqlite_fault_and_oracle() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let report = run_sqlite_fault_profile(tmp.path(), &sqlite_fault_seeds(4))
            .await
            .expect("SQLite fault profile");
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

    /// The same bounded seed set on a real PostgreSQL, proving the Postgres
    /// injector reaches the production write transaction and that every
    /// commit-boundary oracle holds there too.
    ///
    /// Skips when `LASH_POSTGRES_DATABASE_URL` is unset; `LASH_REQUIRE_POSTGRES=1`
    /// makes that a panic, so the CI lane cannot silently pass.
    #[tokio::test]
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

    #[test]
    fn oracle_failure_is_persisted_with_exact_seed_replay() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let failure = ScenarioFailure::oracle(
            BackendFaultKind::Sqlite,
            BackendFaultScenarioKind::CommitIo,
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
        assert!(body.contains("sim.oracle.sqlite-commit-io.v1"));
        assert!(body.contains("lash.sim.sqlite-substrate-fault-failure.v1"));
        assert!(body.contains("backend-faults --backend sqlite"));
    }

    #[test]
    fn a_postgres_failure_package_replays_on_postgres() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let failure = ScenarioFailure::oracle(
            BackendFaultKind::Postgres,
            BackendFaultScenarioKind::CommitIo,
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
        assert!(body.contains("sim.oracle.postgres-commit-io.v1"));
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
                    crate::trace::oracle_observation_class(id).is_some(),
                    "`{id}` is missing from the declared oracle inventory"
                );
            }
        }
    }
}
