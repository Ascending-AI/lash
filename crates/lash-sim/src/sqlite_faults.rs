use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use lash_core::{
    OperationId, RuntimeCommit, RuntimePersistence, RuntimeSessionState, SessionPolicy,
    SessionRelation, SessionStoreCreateRequest, SessionStoreFactory, StoreError,
};
use lash_sqlite_store::testing::{SqliteFaultInjector, SqliteFaultObservation, SqliteFaultPoint};
use serde::Serialize;
use serde_json::{Value, json};

pub const DEFAULT_SQLITE_FAULT_SEED_BASE: u64 = 0x0000_0000_0859_0000;

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SqliteFaultScenarioKind {
    AbortAfterBegin,
    AbortBeforeCommit,
    CommitIo,
    ReopenMidSequence,
}

impl SqliteFaultScenarioKind {
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

    fn fault_point(self) -> Option<SqliteFaultPoint> {
        match self {
            Self::AbortAfterBegin => Some(SqliteFaultPoint::AfterBegin),
            Self::AbortBeforeCommit => Some(SqliteFaultPoint::BeforeCommit),
            Self::CommitIo => Some(SqliteFaultPoint::CommitIo),
            Self::ReopenMidSequence => None,
        }
    }

    fn oracle_id(self) -> &'static str {
        match self {
            Self::AbortAfterBegin => "sim.oracle.sqlite-abort-after-begin.v1",
            Self::AbortBeforeCommit => "sim.oracle.sqlite-abort-before-commit.v1",
            Self::CommitIo => "sim.oracle.sqlite-commit-io.v1",
            Self::ReopenMidSequence => "sim.oracle.sqlite-reopen-mid-sequence.v1",
        }
    }
}

#[derive(Debug, Serialize)]
pub struct SqliteFaultProfileReport {
    pub schema: &'static str,
    pub status: &'static str,
    pub configured_seeds: Vec<u64>,
    pub scenarios: Vec<SqliteFaultScenarioReport>,
    pub coverage: SqliteFaultCoverage,
    #[serde(skip)]
    pub report_path: PathBuf,
}

#[derive(Debug, Serialize)]
pub struct SqliteFaultCoverage {
    pub complete_scenario_set: Vec<SqliteFaultScenarioKind>,
    pub exercised_scenarios: Vec<SqliteFaultScenarioKind>,
    pub dropped_scenarios: Vec<SqliteFaultScenarioKind>,
    pub bounded_prefix_commits_per_seed: &'static str,
}

#[derive(Debug, Serialize)]
pub struct SqliteFaultScenarioReport {
    pub seed: u64,
    pub scenario: SqliteFaultScenarioKind,
    pub prefix_commits: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub injected_fault: Option<SqliteFaultPoint>,
    pub injection_observations: Vec<SqliteFaultObservation>,
    pub oracles: Vec<SqliteFaultOracle>,
    pub replay_command: String,
}

#[derive(Debug, Serialize)]
pub struct SqliteFaultOracle {
    pub oracle_id: &'static str,
    pub status: &'static str,
    pub assertion: &'static str,
    pub evidence: Value,
}

#[derive(Debug, Serialize)]
struct SqliteFaultFailurePackage<'a> {
    schema: &'static str,
    seed: u64,
    scenario: SqliteFaultScenarioKind,
    oracle_id: &'a str,
    reason: &'a str,
    database_root: String,
    prefix_commits: usize,
    injected_fault: Option<SqliteFaultPoint>,
    exact_replay_command: String,
}

#[derive(Debug)]
struct ScenarioFailure {
    oracle_id: &'static str,
    reason: String,
}

impl ScenarioFailure {
    fn harness(reason: impl Into<String>) -> Self {
        Self {
            oracle_id: "sim.oracle.sqlite-fault-harness.v1",
            reason: reason.into(),
        }
    }

    fn oracle(kind: SqliteFaultScenarioKind, reason: impl Into<String>) -> Self {
        Self {
            oracle_id: kind.oracle_id(),
            reason: reason.into(),
        }
    }
}

pub fn sqlite_fault_seeds(count: usize) -> Vec<u64> {
    (0..count)
        .map(|index| DEFAULT_SQLITE_FAULT_SEED_BASE.wrapping_add(index as u64))
        .collect()
}

pub async fn run_sqlite_fault_profile(
    artifact_root: impl AsRef<Path>,
    seeds: &[u64],
) -> Result<SqliteFaultProfileReport, String> {
    if seeds.is_empty() {
        return Err("SQLite fault profile requires at least one seed".to_string());
    }
    let artifact_root = artifact_root.as_ref();
    std::fs::create_dir_all(artifact_root).map_err(|err| err.to_string())?;
    let mut scenarios = Vec::with_capacity(seeds.len());
    for &seed in seeds {
        match run_seed(artifact_root, seed).await {
            Ok(report) => scenarios.push(report),
            Err(failure) => {
                let failure_path = persist_failure(artifact_root, seed, &failure)?;
                return Err(format!(
                    "SQLite substrate fault seed {seed} failed oracle `{}`: {}; reproduction: {}; replay with `cargo run -p lash-sim -- sqlite-faults --out {} --seed {seed}`",
                    failure.oracle_id,
                    failure.reason,
                    failure_path.display(),
                    artifact_root.join("replay").display(),
                ));
            }
        }
    }

    let exercised = scenarios
        .iter()
        .map(|scenario| scenario.scenario)
        .collect::<BTreeSet<_>>();
    let dropped = SqliteFaultScenarioKind::ALL
        .iter()
        .copied()
        .filter(|scenario| !exercised.contains(scenario))
        .collect::<Vec<_>>();
    if !dropped.is_empty() {
        eprintln!("SQLite substrate fault coverage dropped by configured seed bound: {dropped:?}");
    }
    let report_path = artifact_root.join("sqlite-faults.json");
    let report = SqliteFaultProfileReport {
        schema: "lash.sim.sqlite-substrate-faults.v1",
        status: "passed",
        configured_seeds: seeds.to_vec(),
        scenarios,
        coverage: SqliteFaultCoverage {
            complete_scenario_set: SqliteFaultScenarioKind::ALL.to_vec(),
            exercised_scenarios: exercised.into_iter().collect(),
            dropped_scenarios: dropped,
            bounded_prefix_commits_per_seed: "1..=8 selected deterministically by seed",
        },
        report_path: report_path.clone(),
    };
    write_json(&report_path, &report)?;
    Ok(report)
}

async fn run_seed(
    artifact_root: &Path,
    seed: u64,
) -> Result<SqliteFaultScenarioReport, ScenarioFailure> {
    let scenario = SqliteFaultScenarioKind::for_seed(seed);
    let prefix_commits = 1 + ((seed >> 2) as usize % 8);
    let seed_root = artifact_root.join(format!("seed-{seed:016x}"));
    if seed_root.exists() {
        std::fs::remove_dir_all(&seed_root)
            .map_err(|err| ScenarioFailure::harness(format!("reset seed root: {err}")))?;
    }
    std::fs::create_dir_all(&seed_root)
        .map_err(|err| ScenarioFailure::harness(format!("create seed root: {err}")))?;
    let injector = SqliteFaultInjector::default();
    let factory: Arc<dyn SessionStoreFactory> = Arc::new(
        lash_sqlite_store::SqliteSessionStoreFactory::new(seed_root.join("sqlite-store"))
            .with_fault_injector(injector.clone()),
    );
    let session_id = format!("lash-sim-sqlite-fault-{seed:016x}");
    let mut store = create_store(Arc::clone(&factory), &session_id).await?;
    let mut state = RuntimeSessionState {
        session_id: session_id.clone(),
        ..RuntimeSessionState::new(lash_core::SessionPolicy::new(
            lash_core::TurnBudget::Unbounded,
        ))
    };

    for index in 0..prefix_commits {
        state.turn_index = index;
        let commit = stamped_commit(&state, &format!("prefix-{index:03}"))?;
        let result = store
            .commit_runtime_state(commit)
            .await
            .map_err(|err| ScenarioFailure::harness(format!("prefix commit {index}: {err}")))?;
        state.apply_persisted_commit_result(result);
    }
    let durable_prefix_revision = state.head_revision;
    let mut target_state = state.clone();
    target_state.turn_index = prefix_commits;
    let target_commit = stamped_commit(&target_state, "target")?;
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
        .map_err(|_| ScenarioFailure::oracle(scenario, "injected commit hung for five seconds"))?;
        let error_message = match injected {
            Err(StoreError::StorageFailure { message, .. }) => message,
            Err(other) => {
                return Err(ScenarioFailure::oracle(
                    scenario,
                    format!("injected commit returned non-storage-failure error {other:?}"),
                ));
            }
            Ok(result) => {
                return Err(ScenarioFailure::oracle(
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
                scenario,
                format!("fault did not fire exactly once as armed: {observations:?}"),
            ));
        }
        oracles.push(SqliteFaultOracle {
            oracle_id: "sim.oracle.sqlite-fault-typed-error.v1",
            status: "passed",
            assertion: "the substrate fault returns StoreError::StorageFailure within the timeout rather than hanging or panicking",
            evidence: json!({
                "store_error_variant": "StorageFailure",
                "message": error_message,
                "timeout_seconds": 5,
            }),
        });

        drop(store);
        store = open_store(Arc::clone(&factory), &session_id).await?;
        let after_fault = store
            .load_session()
            .await
            .map_err(|err| ScenarioFailure::harness(format!("load after fault: {err}")))?
            .ok_or_else(|| ScenarioFailure::oracle(scenario, "committed prefix disappeared"))?;
        if after_fault.head_revision != durable_prefix_revision {
            return Err(ScenarioFailure::oracle(
                scenario,
                format!(
                    "fault changed durable head from {durable_prefix_revision} to {}",
                    after_fault.head_revision
                ),
            ));
        }
        oracles.push(SqliteFaultOracle {
            oracle_id: "sim.oracle.sqlite-fault-preserves-committed-work.v1",
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
            open_store(Arc::clone(&factory), &session_id),
        )
        .await
        .map_err(|_| ScenarioFailure::oracle(scenario, "reopen hung for five seconds"))??;
        let reopened = store
            .load_session()
            .await
            .map_err(|err| ScenarioFailure::harness(format!("load after reopen: {err}")))?
            .ok_or_else(|| ScenarioFailure::oracle(scenario, "committed prefix disappeared"))?;
        if reopened.head_revision != durable_prefix_revision {
            return Err(ScenarioFailure::oracle(
                scenario,
                format!(
                    "reopen changed durable head from {durable_prefix_revision} to {}",
                    reopened.head_revision
                ),
            ));
        }
        oracles.push(SqliteFaultOracle {
            oracle_id: "sim.oracle.sqlite-reopen-preserves-committed-work.v1",
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
        .map_err(|err| ScenarioFailure::oracle(scenario, format!("target retry failed: {err}")))?;
    let duplicate = store
        .commit_runtime_state(target_commit)
        .await
        .map_err(|err| {
            ScenarioFailure::oracle(scenario, format!("duplicate retry failed: {err}"))
        })?;
    if first.head_revision != durable_prefix_revision + 1
        || duplicate.head_revision != first.head_revision
        || duplicate.checkpoint_ref != first.checkpoint_ref
    {
        return Err(ScenarioFailure::oracle(
            scenario,
            format!(
                "idempotent retry diverged: prefix={durable_prefix_revision}, first={}, duplicate={}",
                first.head_revision, duplicate.head_revision
            ),
        ));
    }
    drop(store);
    let final_store = open_store(factory, &session_id).await?;
    let final_read = final_store
        .load_session()
        .await
        .map_err(|err| ScenarioFailure::harness(format!("final load: {err}")))?
        .ok_or_else(|| ScenarioFailure::oracle(scenario, "final committed state disappeared"))?;
    if final_read.head_revision != first.head_revision {
        return Err(ScenarioFailure::oracle(
            scenario,
            format!(
                "final reopen changed head from {} to {}",
                first.head_revision, final_read.head_revision
            ),
        ));
    }
    oracles.push(SqliteFaultOracle {
        oracle_id: "sim.oracle.sqlite-fault-no-duplicate-effect.v1",
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
    oracles.push(SqliteFaultOracle {
        oracle_id: scenario.oracle_id(),
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

    Ok(SqliteFaultScenarioReport {
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
    state: &RuntimeSessionState,
    operation_suffix: &str,
) -> Result<RuntimeCommit, ScenarioFailure> {
    RuntimeCommit::persisted_state_for_test(state, &[])
        .with_operation(OperationId::turn(
            &state.session_id,
            format!("sqlite-fault-{operation_suffix}"),
            "final",
        ))
        .map(|(commit, _)| commit)
        .map_err(|err| ScenarioFailure::harness(format!("stamp runtime commit: {err}")))
}

fn request(session_id: &str) -> SessionStoreCreateRequest {
    SessionStoreCreateRequest {
        pending_observer_intents: Vec::new(),
        session_id: session_id.to_string(),
        relation: SessionRelation::Root,
        policy: SessionPolicy::new(lash_core::TurnBudget::Unbounded),
    }
}

async fn create_store(
    factory: Arc<dyn SessionStoreFactory>,
    session_id: &str,
) -> Result<Arc<dyn RuntimePersistence>, ScenarioFailure> {
    factory
        .create_store(&request(session_id))
        .await
        .map_err(|err| ScenarioFailure::harness(format!("create store: {err}")))
}

async fn open_store(
    factory: Arc<dyn SessionStoreFactory>,
    session_id: &str,
) -> Result<Arc<dyn RuntimePersistence>, ScenarioFailure> {
    factory
        .open_existing_store(&request(session_id))
        .await
        .map_err(|err| ScenarioFailure::harness(format!("open store: {err}")))?
        .ok_or_else(|| ScenarioFailure::harness("reopened store did not exist"))
}

fn persist_failure(
    artifact_root: &Path,
    seed: u64,
    failure: &ScenarioFailure,
) -> Result<PathBuf, String> {
    let failure_root = artifact_root
        .join("failures")
        .join(format!("seed-{seed:016x}"));
    std::fs::create_dir_all(&failure_root).map_err(|err| err.to_string())?;
    let path = failure_root.join("reproduction.json");
    let package = SqliteFaultFailurePackage {
        schema: "lash.sim.sqlite-substrate-fault-failure.v1",
        seed,
        scenario: SqliteFaultScenarioKind::for_seed(seed),
        oracle_id: failure.oracle_id,
        reason: &failure.reason,
        database_root: artifact_root
            .join(format!("seed-{seed:016x}"))
            .join("sqlite-store")
            .display()
            .to_string(),
        prefix_commits: 1 + ((seed >> 2) as usize % 8),
        injected_fault: SqliteFaultScenarioKind::for_seed(seed).fault_point(),
        exact_replay_command: format!(
            "cargo run -p lash-sim -- sqlite-faults --out {} --seed {seed}",
            artifact_root.join("replay").display()
        ),
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

    #[test]
    fn oracle_failure_is_persisted_with_exact_seed_replay() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let failure = ScenarioFailure::oracle(
            SqliteFaultScenarioKind::CommitIo,
            "deliberate oracle failure",
        );
        let path = persist_failure(tmp.path(), DEFAULT_SQLITE_FAULT_SEED_BASE + 2, &failure)
            .expect("persist failure");
        let body = std::fs::read_to_string(path).expect("failure package");
        assert!(body.contains("deliberate oracle failure"));
        assert!(body.contains("--seed 140050434"));
    }
}
