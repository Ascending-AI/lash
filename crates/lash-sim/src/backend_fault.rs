use std::collections::BTreeMap;
use std::sync::Arc;

use lash_core::{
    OperationId, RuntimeCommit, RuntimePersistence, RuntimeSessionState, SessionPolicy,
    SessionRelation, SessionStoreCreateRequest, SessionStoreFactory, StoreError,
};
use lash_sqlite_store::testing::{SqliteFaultInjector, SqliteFaultPoint};
use serde_json::{Value, json};

use crate::runner::FixedScriptRunnerError;
use crate::scheduler::BoundaryEvent;

pub(crate) struct GeneratedBackendFaultHarness {
    attempts_by_session_operation: BTreeMap<(String, String), usize>,
    _root: tempfile::TempDir,
    factory: Arc<dyn SessionStoreFactory>,
    injector: SqliteFaultInjector,
    injector_enabled: bool,
}

impl Default for GeneratedBackendFaultHarness {
    fn default() -> Self {
        Self::new(true)
    }
}

impl GeneratedBackendFaultHarness {
    fn new(injector_enabled: bool) -> Self {
        let root = tempfile::tempdir().expect("create generated SQLite fault root");
        let injector = SqliteFaultInjector::default();
        let factory: Arc<dyn SessionStoreFactory> = Arc::new(
            lash_sqlite_store::SqliteSessionStoreFactory::new(root.path().join("store"))
                .with_fault_injector(injector.clone()),
        );
        Self {
            attempts_by_session_operation: BTreeMap::new(),
            _root: root,
            factory,
            injector,
            injector_enabled,
        }
    }

    #[cfg(test)]
    pub(crate) fn with_injector_enabled(injector_enabled: bool) -> Self {
        Self::new(injector_enabled)
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
        let retryable = event
            .payload
            .get("retryable")
            .and_then(Value::as_bool)
            .unwrap_or(true);
        let point = if retryable {
            SqliteFaultPoint::AfterBegin
        } else {
            SqliteFaultPoint::CommitIo
        };
        let seed = event.at ^ ((attempt as u64) << 32) ^ 0x4649_4731_3135_3300;
        let session_id = format!(
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
        );
        let store = self.create_store(&session_id).await?;
        let state = RuntimeSessionState {
            session_id: session_id.clone(),
            ..RuntimeSessionState::new(SessionPolicy::new(lash_core::TurnBudget::Unbounded))
        };
        let (commit, _) = RuntimeCommit::persisted_state_for_test(&state, &[])
            .with_operation(OperationId::turn(
                &session_id,
                format!("generated-backend-fault-{attempt}"),
                "final",
            ))
            .map_err(|err| FixedScriptRunnerError::Runtime(err.to_string()))?;
        let observations_before = self.injector.observations().len();
        if self.injector_enabled {
            self.injector.arm(seed, point);
        }
        let result = store.commit_runtime_state(commit).await;
        let observations = self.injector.observations();
        let injected = observations.get(observations_before).cloned();

        if !self.injector_enabled {
            let result = result.map_err(|err| {
                FixedScriptRunnerError::Runtime(format!(
                    "uninjected SQLite backend probe failed unexpectedly: {err}"
                ))
            })?;
            return Ok(json!({
                "session": event.actor_alias,
                "backend_failure": false,
                "operation": operation,
                "commit_succeeded": true,
                "head_revision": result.head_revision,
                "fault_injector": {
                    "enabled": false,
                    "exercised": false,
                },
            }));
        }

        let error = match result {
            Err(error @ StoreError::StorageFailure { .. }) => error,
            Err(other) => {
                return Err(FixedScriptRunnerError::Assertion(format!(
                    "SQLite fault `{}` returned non-storage error {other:?}",
                    event.boundary_id
                )));
            }
            Ok(result) => {
                return Err(FixedScriptRunnerError::Assertion(format!(
                    "SQLite fault `{}` unexpectedly committed revision {}",
                    event.boundary_id, result.head_revision
                )));
            }
        };
        let injected = injected.ok_or_else(|| {
            FixedScriptRunnerError::Assertion(format!(
                "SQLite fault `{}` did not reach the armed injector",
                event.boundary_id
            ))
        })?;
        Ok(json!({
            "session": event.actor_alias,
            "backend_failure": true,
            "operation": operation,
            "attempt": attempt,
            "retryable": retryable,
            "store_error_class": if retryable { "retryable_conflict" } else { "terminal_backend_error" },
            "production_store_error": {
                "type": "lash_core::StoreError",
                "variant": error.variant_name(),
                "message": error.to_string(),
                "retryable_class": retryable,
            },
            "fault_injector": {
                "enabled": true,
                "exercised": true,
                "implementation": "lash_sqlite_store::testing::SqliteFaultInjector",
                "seed": injected.seed,
                "point": injected.point,
                "write_transaction_ordinal": injected.write_transaction_ordinal,
            },
        }))
    }

    async fn create_store(
        &self,
        session_id: &str,
    ) -> Result<Arc<dyn RuntimePersistence>, FixedScriptRunnerError> {
        self.factory
            .create_store(&SessionStoreCreateRequest {
                pending_observer_intents: Vec::new(),
                session_id: session_id.to_string(),
                relation: SessionRelation::Root,
                policy: SessionPolicy::new(lash_core::TurnBudget::Unbounded),
            })
            .await
            .map_err(|err| FixedScriptRunnerError::Runtime(err.to_string()))
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
                "retryable": retryable,
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
    async fn backend_failure_observation_depends_on_real_sqlite_injector() {
        let retry = event("session-001:backend-failure:001", true, 1);
        let terminal = event("session-001:backend-failure:002", false, 2);

        let mut enabled = GeneratedBackendFaultHarness::with_injector_enabled(true);
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
                .pointer("/fault_injector/exercised")
                .and_then(Value::as_bool)
                == Some(true)
        }));

        let mut disabled = GeneratedBackendFaultHarness::with_injector_enabled(false);
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
            "real injector observations must satisfy the backend oracle: {}",
            enabled_verdict.message
        );
        assert_eq!(enabled_verdict.oracle_id, BACKEND_FAILURE_ORACLE);

        let mut wrong_point_events = enabled_events.clone();
        wrong_point_events[0].observed["fault_injector"]["point"] = json!("commit_io");
        let mut wrong_point_model = ModelStore::default();
        wrong_point_model.open_session("session-001");
        for (event, delivered) in [&retry, &terminal].into_iter().zip(&wrong_point_events) {
            wrong_point_model.apply_observed_boundary(event, &delivered.observed);
        }
        let wrong_point =
            backend_failure_observed(&wrong_point_model.summary(), &wrong_point_events);
        assert!(
            !wrong_point.is_passed(),
            "retryable classification must be grounded in the observed injector point"
        );

        let mut disabled_model = ModelStore::default();
        disabled_model.open_session("session-001");
        for (event, delivered) in [&retry, &terminal].into_iter().zip(&disabled_events) {
            disabled_model.apply_observed_boundary(event, &delivered.observed);
        }
        let disabled_verdict =
            backend_failure_observed(&disabled_model.summary(), &disabled_events);
        assert!(
            !disabled_verdict.is_passed(),
            "disabling the injector must change the oracle result"
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
    async fn generated_backend_failure_seed_records_real_injector_evidence() {
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
                .pointer("/fault_injector/exercised")
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
            .expect("model replay carries the recorded real injector observation");
    }
}
