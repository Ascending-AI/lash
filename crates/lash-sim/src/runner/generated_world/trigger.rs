//! Trigger boundaries: a subscription registered for the boundary's source,
//! and one occurrence emitted through the production trigger router. The
//! router commits the occurrence with its delivery bound to the process it
//! starts in one `trigger.start` transaction (ADR 0132 §12). The world's
//! engine runs no node, so the started process stays ready, as a
//! deployment's process actor would find it.

use super::*;
use lash_core_execution::facade_support::TriggerRouter;
use lash_core_execution::{
    ActorContext, ProcessEngineRegistration, ProcessEngineRegistry, ProcessWorkWiring,
};

/// The source every simulated trigger subscribes to and emits on.
const SOURCE_TYPE: &str = "sim.trigger";

pub(super) struct SimTriggerHarness {
    backend: lash::Backend,
    registered_source_keys: BTreeSet<String>,
    env_ref: Option<lash_core::ProcessExecutionEnvRef>,
}

impl SimTriggerHarness {
    pub(super) fn over(backend: lash::Backend) -> Self {
        Self {
            backend,
            registered_source_keys: BTreeSet::new(),
            env_ref: None,
        }
    }

    pub(super) async fn deliver(
        &mut self,
        event: &BoundaryEvent,
    ) -> Result<Value, FixedScriptRunnerError> {
        let session = event
            .payload
            .get("session")
            .and_then(Value::as_str)
            .unwrap_or(&event.actor_alias)
            .to_string();
        let source_key = event
            .payload
            .get("source_key")
            .and_then(Value::as_str)
            .unwrap_or(&event.boundary_id)
            .to_string();
        let runtime =
            |err: &dyn std::fmt::Display| FixedScriptRunnerError::Runtime(err.to_string());
        if self.registered_source_keys.insert(source_key.clone()) {
            let env_ref = match &self.env_ref {
                Some(env_ref) => env_ref.clone(),
                None => {
                    let env_ref = lash_core_execution::testing::process_execution_env_fixture(
                        &*self.backend.process_env_store(),
                    )
                    .await;
                    self.env_ref = Some(env_ref.clone());
                    env_ref
                }
            };
            let draft = lash_core::TriggerSubscriptionDraft::for_process(
                format!("sim/{}", event.boundary_id),
                env_ref,
                SOURCE_TYPE,
                source_key.clone(),
                lash_core::ProcessInput::Engine {
                    kind: crate::crash_matrix::engine::KIND.to_owned(),
                    payload: crate::crash_matrix::engine::ends_at_once(&event.boundary_id),
                },
                lash_core::ProcessIdentity::labelled(
                    crate::crash_matrix::engine::KIND,
                    Some("sim trigger"),
                ),
            )
            .with_payload_schema(lash_core::JsonSchema::any())
            .with_wake_target(lash_core::SessionScope::new(SessionId::fixture(
                session.clone(),
            )));
            self.backend
                .trigger_store()
                .execute_command(
                    &format!("sim-trigger-register:{}", event.boundary_id),
                    lash_core::TriggerCommand::Register {
                        owner_scope: lash_core::TriggerOwnerScope::session(SessionId::fixture(
                            session.clone(),
                        )),
                        actor: lash_core::ProcessOriginator::session(lash_core::SessionScope::new(
                            SessionId::fixture(session.clone()),
                        )),
                        draft,
                    },
                )
                .await
                .map_err(|err| runtime(&err))?
                .map_err(|err| runtime(&err))?;
        }
        let request = lash_core::TriggerOccurrenceRequest::new(
            SOURCE_TYPE,
            source_key.clone(),
            json!({
                "boundary_id": event.boundary_id,
                "session": session,
            }),
            format!("sim-trigger:{}", event.boundary_id),
        )
        .with_source(json!({"sim": true}));
        let engines =
            ProcessEngineRegistry::new().with_registration(ProcessEngineRegistration::accepting(
                Arc::new(crate::crash_matrix::engine::SimProcessEngine),
            ));
        let router = TriggerRouter::new(
            self.backend.trigger_store(),
            ProcessWorkWiring::without_process_work(self.backend.process_registry()),
        )
        .with_process_artifacts(self.backend.process_env_store(), engines);
        let report = router
            .emit(request, &ActorContext::detached(self.backend.clone()))
            .await
            .map_err(|err| runtime(&err))?;
        Ok(json!({
            "session": session,
            "trigger_delivered": true,
            "source_key": source_key,
            "occurrence_id": report.occurrence_id,
            "reservation_count": report.deliveries.len(),
            "started_process": !report.deliveries.is_empty(),
        }))
    }
}
