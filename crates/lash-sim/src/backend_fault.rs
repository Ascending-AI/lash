//! Backend faults for the simulator, armed as a [`Script`] over the store
//! (ADR 0044 §Simulation).
//!
//! An arm names one fault of `commit_runtime_state`: the call is refused
//! before it enters the store, or the store commits and the reply is lost.
//! The script sits at the store trait, so the same arm faults the same call
//! of the same operation on every backend.

use lash_sansio::SessionId;
use std::collections::BTreeMap;
use std::sync::Arc;

use lash_core::testing::{Outcome, Phase, Script, StoreOp};
use lash_core::{
    DeploymentStore, OperationId, RuntimeCommit, RuntimeSessionState, SessionCreationHead,
    SessionPolicy, SessionRelation, SessionStoreCreateRequest, StoreError,
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::runner::FixedScriptRunnerError;
use crate::scheduler::BoundaryEvent;

/// The operation every backend fault arms.
pub const FAULTED_OPERATION: StoreOp = StoreOp::commit_runtime_state;

/// What the script that injects every backend fault is.
pub const FAULT_SCRIPT_IMPLEMENTATION: &str = "lash_core::testing::Script";

/// The actor the faulted commits run as in the script's trace.
const FAULTED_ACTOR: &str = "lash-sim";

/// Which real store backend a fault plan is driving.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum BackendFaultKind {
    SqliteMemory,
    Sqlite,
    Postgres,
}

impl BackendFaultKind {
    pub const ALL: [Self; 3] = [Self::SqliteMemory, Self::Sqlite, Self::Postgres];

    pub const fn name(self) -> &'static str {
        match self {
            Self::SqliteMemory => "sqlite-memory",
            Self::Sqlite => "sqlite",
            Self::Postgres => "postgres",
        }
    }

    /// Name of the JSON report this backend's profile writes.
    pub const fn report_file_name(self) -> &'static str {
        match self {
            Self::SqliteMemory | Self::Sqlite => "sqlite-faults.json",
            Self::Postgres => "postgres-faults.json",
        }
    }

    /// The `lash-sim backend-faults` argument that selects this backend.
    pub const fn replay_backend_argument(self) -> &'static str {
        match self {
            Self::SqliteMemory => "--backend sqlite-memory",
            Self::Sqlite => "--backend sqlite",
            Self::Postgres => "--backend postgres",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|kind| kind.name() == value)
    }
}

/// One fault of a `commit_runtime_state` call, as the script injects it.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum BackendFault {
    /// The caller is answered a storage failure before the call enters the
    /// store: nothing the call carried is durable.
    Refused,
    /// The store commits and the caller is answered a storage failure in
    /// place of the reply: the commit stands.
    ReplyLost,
}

impl BackendFault {
    pub const fn name(self) -> &'static str {
        match self {
            Self::Refused => "refused",
            Self::ReplyLost => "reply_lost",
        }
    }

    /// Whether the faulted call's work is durable.
    pub const fn commit_stands(self) -> bool {
        matches!(self, Self::ReplyLost)
    }

    const fn of_phase(phase: Phase) -> Self {
        match phase {
            Phase::Before => Self::Refused,
            Phase::After => Self::ReplyLost,
        }
    }
}

/// One deterministic one-shot arm in a backend fault plan. `seed` is the
/// arm's identity in reports and in the refusal it answers.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct BackendFaultArm {
    pub seed: u64,
    pub fault: BackendFault,
}

impl BackendFaultArm {
    pub const fn new(seed: u64, fault: BackendFault) -> Self {
        Self { seed, fault }
    }
}

/// Evidence that an arm faulted a call: the call as the script's trace
/// recorded it.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct BackendFaultObservation {
    pub arm_index: usize,
    pub seed: u64,
    pub fault: BackendFault,
    pub operation: String,
    /// The faulted call's one-based position among the script's calls of
    /// `operation`.
    pub call: u64,
}

/// A store under a plan of arms: the kth arm faults the kth
/// `commit_runtime_state` made through [`Self::store`].
///
/// Dropping it fails the run when an arm never fired, as a [`Script`] does.
pub struct BackendFaultScript {
    script: Script,
    arms: Vec<BackendFaultArm>,
    store: Arc<dyn DeploymentStore>,
}

impl BackendFaultScript {
    pub fn arm(
        inner: Arc<dyn DeploymentStore>,
        arms: impl IntoIterator<Item = BackendFaultArm>,
    ) -> Self {
        let script = Script::new();
        let arms = arms.into_iter().collect::<Vec<_>>();
        for (index, arm) in arms.iter().enumerate() {
            let rule = script.on(FAULTED_OPERATION).nth(index + 1);
            match arm.fault {
                BackendFault::Refused => {
                    let seed = arm.seed;
                    rule.before().fail(move || StoreError::StorageFailure {
                        backend: "lash-sim",
                        message: format!(
                            "arm {seed} refused the commit before it entered the store"
                        ),
                    });
                }
                BackendFault::ReplyLost => rule.after().lose_reply(),
            }
        }
        let store = script.wrap(FAULTED_ACTOR, inner) as Arc<dyn DeploymentStore>;
        Self {
            script,
            arms,
            store,
        }
    }

    /// The store whose commits the arms fault.
    pub fn store(&self) -> Arc<dyn DeploymentStore> {
        Arc::clone(&self.store)
    }

    /// Every phase of every call made through [`Self::store`], rendered as
    /// the script's trace prints it.
    pub fn trace(&self) -> Vec<String> {
        self.script
            .trace()
            .iter()
            .map(ToString::to_string)
            .collect()
    }

    /// Every call an arm faulted so far, in the order they happened.
    pub fn observations(&self) -> Vec<BackendFaultObservation> {
        self.script
            .trace()
            .into_iter()
            .filter(|call| matches!(call.outcome, Outcome::Failed(_)))
            .filter_map(|call| {
                let arm_index = call.nth.checked_sub(1)?;
                let arm = self.arms.get(arm_index)?;
                Some(BackendFaultObservation {
                    arm_index,
                    seed: arm.seed,
                    fault: BackendFault::of_phase(call.phase),
                    operation: call.op.name().to_string(),
                    call: call.nth as u64,
                })
            })
            .collect()
    }
}

/// One backend lane a fault profile runs against.
///
/// The SQLite lanes need nothing beyond a store per case. The Postgres lane
/// owns one throwaway database created from `LASH_POSTGRES_DATABASE_URL` and
/// dropped with the lane, so concurrent suites that truncate the shared
/// database cannot delete a scenario's durable prefix mid-run.
pub struct BackendFaultLane {
    kind: BackendFaultKind,
    postgres: Option<PostgresFaultLane>,
}

struct PostgresFaultLane {
    storage: lash_postgres_store::PostgresStorage,
    // Dropped with the lane, which drops the database.
    _database: lash_postgres_store::testing::IsolatedDatabase,
}

impl BackendFaultLane {
    /// An explicitly selected PostgreSQL lane requires a non-empty database URL.
    pub async fn open(kind: BackendFaultKind) -> Result<Option<Self>, String> {
        match kind {
            BackendFaultKind::SqliteMemory | BackendFaultKind::Sqlite => Ok(Some(Self {
                kind,
                postgres: None,
            })),
            BackendFaultKind::Postgres => {
                let database = lash_postgres_store::testing::IsolatedDatabase::create(
                    &lash_postgres_store::testing::required_database_url(),
                )
                .await;
                let storage = lash_postgres_store::PostgresStorage::connect(database.url())
                    .await
                    .map_err(|error| format!("connect postgres fault store: {error}"))?;
                Ok(Some(Self {
                    kind,
                    postgres: Some(PostgresFaultLane {
                        storage,
                        _database: database,
                    }),
                }))
            }
        }
    }

    pub fn kind(&self) -> BackendFaultKind {
        self.kind
    }

    /// A fresh store of this lane's backend, as production opens it.
    ///
    /// `case_root` is only used by the SQLite file lane.
    #[expect(
        clippy::expect_used,
        reason = "test support: the surrounding harness code establishes this value; a refusal panics the harness with its case name by design"
    )]
    pub async fn store(
        &self,
        case_root: &std::path::Path,
    ) -> Result<Arc<dyn DeploymentStore>, String> {
        match self.kind {
            BackendFaultKind::SqliteMemory => Ok(lash_sqlite_store::SqliteStoreSet::memory()
                .await
                .map_err(|error| format!("open SQLite memory fault store: {error}"))?
                .session_store_factory()),
            BackendFaultKind::Sqlite => Ok(Arc::new(
                lash_sqlite_store::SqliteStore::open(&case_root.join("store"))
                    .await
                    .map_err(|error| format!("open SQLite fault store: {error}"))?,
            )),
            BackendFaultKind::Postgres => {
                let lane = self
                    .postgres
                    .as_ref()
                    .expect("a PostgreSQL lane always opens its database");
                Ok(Arc::new(lash_postgres_store::PostgresStore::new(
                    &lane.storage,
                )))
            }
        }
    }
}

/// The generated runner's backend-failure boundary: one fresh SQLite memory
/// session per boundary, whose one commit a one-arm script faults.
pub(crate) struct GeneratedBackendFaultHarness {
    attempts_by_session_operation: BTreeMap<(String, String), usize>,
    factory: tokio::sync::OnceCell<Arc<dyn DeploymentStore>>,
    script_armed: bool,
}

impl Default for GeneratedBackendFaultHarness {
    fn default() -> Self {
        Self::new(true)
    }
}

impl GeneratedBackendFaultHarness {
    fn new(script_armed: bool) -> Self {
        Self {
            attempts_by_session_operation: BTreeMap::new(),
            factory: tokio::sync::OnceCell::new(),
            script_armed,
        }
    }

    #[cfg(test)]
    pub(crate) fn with_script_armed(script_armed: bool) -> Self {
        Self::new(script_armed)
    }

    pub(crate) async fn inject(
        &mut self,
        event: &BoundaryEvent,
    ) -> Result<Value, FixedScriptRunnerError> {
        let operation = event
            .payload
            .get("operation")
            .and_then(Value::as_str)
            .unwrap_or("backend_operation")
            .to_string();
        let attempts = self
            .attempts_by_session_operation
            .entry((event.actor_alias.clone(), operation.clone()))
            .or_insert(0);
        *attempts += 1;
        let attempt = *attempts;
        let fault: BackendFault =
            serde_json::from_value(event.payload.get("fault").cloned().unwrap_or(Value::Null))
                .map_err(|error| {
                    FixedScriptRunnerError::Assertion(format!("backend fault: {error}"))
                })?;
        let seed = event.at ^ ((attempt as u64) << 32) ^ 0x4649_4731_3135_3300;
        let session_id = SessionId::fixture(format!(
            "sim-fault-{}",
            event
                .boundary_id
                .chars()
                .map(|character| {
                    if character.is_ascii_alphanumeric() {
                        character
                    } else {
                        '-'
                    }
                })
                .collect::<String>()
        ));
        let factory = self.create_session(&session_id).await?;
        let state = RuntimeSessionState {
            session_id: session_id.clone(),
            ..RuntimeSessionState::new(SessionPolicy::new(
                lash_core::TurnBudget::Unbounded,
                lash_core::MaxToolCalls::new(1024),
            ))
        };
        let (commit, _) = RuntimeCommit::persisted_state_for_test(&state)
            .with_operation(OperationId::turn(
                &session_id,
                lash_core::TurnId::fixture(format!("generated-backend-fault-{attempt}")),
                "final",
            ))
            .map_err(|err| FixedScriptRunnerError::Runtime(err.to_string()))?;
        let arms = self
            .script_armed
            .then_some(BackendFaultArm::new(seed, fault));
        let script = BackendFaultScript::arm(factory, arms);
        let result = script.store().commit_runtime_state(commit).await;
        let injected = script.observations().into_iter().next();

        if !self.script_armed {
            let result = result.map_err(|err| {
                FixedScriptRunnerError::Runtime(format!(
                    "unscripted SQLite backend probe failed unexpectedly: {err}"
                ))
            })?;
            return Ok(json!({
                "session": event.actor_alias,
                "backend_failure": false,
                "operation": operation,
                "commit_succeeded": true,
                "head_revision": result.head_revision,
                "fault_script": {
                    "armed": false,
                    "fired": false,
                },
            }));
        }

        let error = match result {
            Err(error @ StoreError::StorageFailure { .. }) => error,
            Err(other) => {
                return Err(FixedScriptRunnerError::Assertion(format!(
                    "backend fault `{}` returned non-storage error {other:?}",
                    event.boundary_id
                )));
            }
            Ok(result) => {
                return Err(FixedScriptRunnerError::Assertion(format!(
                    "backend fault `{}` unexpectedly answered revision {}",
                    event.boundary_id, result.head_revision
                )));
            }
        };
        let injected = injected.ok_or_else(|| {
            FixedScriptRunnerError::Assertion(format!(
                "backend fault `{}` did not reach the armed script",
                event.boundary_id
            ))
        })?;
        let mut observation = crate::store::backend_fault_observation(
            json!(event.actor_alias),
            operation,
            attempt,
            &error,
        );
        observation["fault_script"] = json!({
            "armed": true,
            "fired": true,
            "implementation": FAULT_SCRIPT_IMPLEMENTATION,
            "seed": injected.seed,
            "fault": injected.fault,
            "store_operation": injected.operation,
            "call": injected.call,
        });
        Ok(observation)
    }

    async fn create_session(
        &self,
        session_id: &SessionId,
    ) -> Result<Arc<dyn DeploymentStore>, FixedScriptRunnerError> {
        let factory = self
            .factory
            .get_or_try_init(|| async {
                let stores = lash_sqlite_store::SqliteStoreSet::memory()
                    .await
                    .map_err(|error| FixedScriptRunnerError::Runtime(error.to_string()))?;
                let factory: Arc<dyn DeploymentStore> = stores.session_store_factory();
                Ok::<_, FixedScriptRunnerError>(factory)
            })
            .await?;
        factory
            .admit_session(&SessionStoreCreateRequest {
                owning_process_id: None,
                pending_observer_intents: Vec::new(),
                session_id: session_id.clone(),
                relation: SessionRelation::Root,
                config: SessionPolicy::new(
                    lash_core::TurnBudget::Unbounded,
                    lash_core::MaxToolCalls::new(1024),
                )
                .into(),
                head: SessionCreationHead::Config,
            })
            .await
            .map_err(|err| FixedScriptRunnerError::Runtime(err.to_string()))?;
        Ok(Arc::clone(factory))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::oracles::{BACKEND_FAILURE_ORACLE, backend_failure_observed};
    use crate::scheduler::{BoundaryKind, DeliveredBoundary};
    use crate::store::ModelStore;

    fn event(id: &str, retryable: bool, at: u64) -> BoundaryEvent {
        event_for_session("session-001", id, retryable, at)
    }

    fn event_for_session(session: &str, id: &str, retryable: bool, at: u64) -> BoundaryEvent {
        BoundaryEvent::new(
            id,
            session,
            BoundaryKind::BackendFailure,
            at,
            if retryable {
                "backend.failure.retryable"
            } else {
                "backend.failure.terminal"
            },
            json!({
                "session": session,
                "operation": "commit_runtime_state:001",
                "fault": if retryable { BackendFault::Refused } else { BackendFault::ReplyLost },
            }),
        )
    }

    fn delivered(event: &BoundaryEvent, sequence: usize, observed: Value) -> DeliveredBoundary {
        DeliveredBoundary {
            schema: "lash.sim.delivered-boundary.v1".to_string(),
            sequence,
            scheduler: Default::default(),
            boundary_id: event.boundary_id.clone(),
            actor_alias: event.actor_alias.clone(),
            kind: event.kind,
            at: event.at,
            label: event.label.clone(),
            payload: event.payload.clone(),
            observed,
        }
    }

    #[tokio::test]
    async fn fig_4679_a_lost_reply_uses_the_returned_store_error_class() {
        let mut harness = GeneratedBackendFaultHarness::default();
        let observed = harness
            .inject(&event("classification", false, 1))
            .await
            .expect("injected lost reply");
        assert_eq!(
            observed["production_store_error"]["variant"],
            "StorageFailure"
        );
        assert_eq!(
            observed["transient"], true,
            "production storage failures are transient"
        );
        assert_eq!(observed["transient"], true);
    }

    #[tokio::test]
    async fn backend_failure_observation_depends_on_the_armed_script() {
        let retry = event("session-001:backend-failure:001", true, 1);
        let terminal = event("session-001:backend-failure:002", false, 2);

        let mut enabled = GeneratedBackendFaultHarness::with_script_armed(true);
        let enabled_events = vec![
            delivered(
                &retry,
                1,
                enabled.inject(&retry).await.expect("injected retry fault"),
            ),
            delivered(
                &terminal,
                2,
                enabled
                    .inject(&terminal)
                    .await
                    .expect("injected terminal fault"),
            ),
        ];
        assert!(enabled_events.iter().all(|event| {
            event
                .observed
                .pointer("/fault_script/fired")
                .and_then(Value::as_bool)
                == Some(true)
        }));

        let mut disabled = GeneratedBackendFaultHarness::with_script_armed(false);
        let disabled_events = vec![
            delivered(
                &retry,
                1,
                disabled
                    .inject(&retry)
                    .await
                    .expect("uninjected retry commit"),
            ),
            delivered(
                &terminal,
                2,
                disabled
                    .inject(&terminal)
                    .await
                    .expect("uninjected terminal commit"),
            ),
        ];
        assert_ne!(enabled_events[0].observed, disabled_events[0].observed);

        let mut enabled_model = ModelStore::default();
        enabled_model.open_session("session-001");
        for (event, delivered) in [&retry, &terminal].into_iter().zip(&enabled_events) {
            enabled_model.apply_observed_boundary(event, &delivered.observed);
        }
        let enabled_verdict = backend_failure_observed(&enabled_model.summary(), &enabled_events);
        assert!(
            enabled_verdict.is_passed(),
            "scripted fault observations must satisfy the backend oracle: {}",
            enabled_verdict.message
        );
        assert_eq!(enabled_verdict.oracle_id, BACKEND_FAILURE_ORACLE);

        let mut disabled_model = ModelStore::default();
        disabled_model.open_session("session-001");
        for (event, delivered) in [&retry, &terminal].into_iter().zip(&disabled_events) {
            disabled_model.apply_observed_boundary(event, &delivered.observed);
        }
        let disabled_verdict =
            backend_failure_observed(&disabled_model.summary(), &disabled_events);
        assert!(
            !disabled_verdict.is_passed(),
            "leaving the script unarmed must change the oracle result"
        );
    }

    #[tokio::test]
    async fn backend_failure_attempts_are_scoped_by_session_and_operation() {
        let first_session =
            event_for_session("session-001", "session-001:backend-failure:001", true, 1);
        let second_session =
            event_for_session("session-002", "session-002:backend-failure:001", true, 2);
        let second_terminal =
            event_for_session("session-002", "session-002:backend-failure:002", false, 3);
        let mut harness = GeneratedBackendFaultHarness::default();
        let first_observed = harness
            .inject(&first_session)
            .await
            .expect("first session fault");
        let second_observed = harness
            .inject(&second_session)
            .await
            .expect("second session fault");
        let terminal_observed = harness
            .inject(&second_terminal)
            .await
            .expect("second session terminal fault");

        assert_eq!(first_observed["attempt"], 1);
        assert_eq!(second_observed["attempt"], 1);
        assert_eq!(terminal_observed["attempt"], 2);

        let events = vec![
            delivered(&first_session, 1, first_observed),
            delivered(&second_session, 2, second_observed),
            delivered(&second_terminal, 3, terminal_observed),
        ];
        let mut model = ModelStore::default();
        model.open_session("session-001");
        model.open_session("session-002");
        for (event, delivered) in [&first_session, &second_session, &second_terminal]
            .into_iter()
            .zip(&events)
        {
            model.apply_observed_boundary(event, &delivered.observed);
        }
        assert!(
            backend_failure_observed(&model.summary(), &events).is_passed(),
            "retry and terminal evidence from session-002 must satisfy the law without borrowing session-001"
        );
    }

    #[tokio::test]
    async fn generated_backend_failure_seed_records_script_evidence() {
        let workload = crate::generator::generate_workload(5, "fast-random", 24)
            .expect("seeded generated workload");
        let trace = crate::runner::run_generated_workload_for_fixture(workload, "bundle")
            .await
            .expect("generated trace");
        let backend_events = trace
            .events
            .iter()
            .filter(|event| event.kind == BoundaryKind::BackendFailure)
            .collect::<Vec<_>>();
        assert!(!backend_events.is_empty());
        assert!(backend_events.iter().all(|event| {
            event
                .observed
                .pointer("/fault_script/fired")
                .and_then(Value::as_bool)
                == Some(true)
        }));
        let verdict = trace
            .oracles
            .iter()
            .find(|verdict| verdict.oracle_id == BACKEND_FAILURE_ORACLE)
            .expect("backend failure verdict");
        assert!(verdict.is_passed(), "{}", verdict.message);
        assert_eq!(
            verdict.observation_class,
            crate::trace::OracleObservationClass::RealObservation
        );
        assert!(trace.oracle.is_passed(), "{}", trace.oracle.message);
        crate::replay::replay_trace(std::path::Path::new("generated-seed-5.json"), &trace)
            .expect("model replay carries the recorded script observation");
    }
}
