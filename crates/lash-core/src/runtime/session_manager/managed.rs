use super::create_plan::{SessionCreatePlan, resolve_session_create_plan};
use super::materialize::{MaterializedSession, materialize_session_create_plan};
use super::*;

impl ManagedSessionCapability {
    async fn register_materialized_session(
        &self,
        current: &CurrentSessionCapability,
        usage: &UsageCapability,
        plan: SessionCreatePlan,
        mut materialized: MaterializedSession,
    ) -> Result<SessionHandle, crate::PluginError> {
        if let Some(store) = &materialized.store_binding {
            let mut persisted_state = materialized
                .runtime
                .export_persisted_state()
                .await
                .map_err(|err| crate::PluginError::Session(err.to_string()))?;
            let operation = super::super::state::boundary_operation(
                &persisted_state.session_id,
                &plan.session_id,
                "create-session",
            );
            let (commit, persisted_node_ids) =
                crate::store::RuntimeCommit::persisted_state_with_operation(
                    &mut persisted_state,
                    &[],
                    operation,
                )
                .map_err(|err| crate::PluginError::Session(err.to_string()))?;
            // Lane-less by construction: the child is being created before it
            // owns an execution lane. A parent guard, if present, names a
            // different session and cannot authorize this child commit.
            let result = commit_runtime_state_with_fresh_session_execution_lease(
                Arc::clone(store),
                commit,
                &materialized.runtime.runtime_lease_owner,
                materialized.runtime.host.core.control.lease_timings,
                Arc::clone(&materialized.runtime.host.core.clock),
            )
            .await
            .map_err(|err| crate::PluginError::Session(err.to_string()))?;
            persisted_state.apply_persisted_commit_result(result);
            persisted_state.mark_node_ids_persisted(persisted_node_ids);
            materialized.runtime.state = persisted_state;
        }
        let observer_intent_source = match materialized.store_binding.as_deref() {
            Some(store) => crate::runtime::SessionObserverIntentSource::Persisted(store),
            None => crate::runtime::SessionObserverIntentSource::Unstored(plan.relation.clone()),
        };
        let observed_processes = crate::runtime::reconcile_session_process_observer_intents(
            current.host.process_registry.as_deref(),
            &plan.session_id,
            observer_intent_source,
        )
        .await
        .map_err(|error| {
            crate::PluginError::Session(format!(
                "failed to settle session-create observer intents: {error}"
            ))
        })?;
        self.registry.lock().await.insert(
            plan.session_id.clone(),
            RuntimeHandle::new(materialized.runtime),
        );
        if let Some(source) = &plan.usage_source {
            usage
                .child_sources
                .lock()
                .expect("child usage sources lock")
                .insert(plan.session_id.clone(), source.clone());
        }
        Ok(SessionHandle {
            session_id: plan.session_id,
            parent_session_id: plan.parent_session_id,
            policy: plan.policy,
            observed_processes,
        })
    }

    pub(in crate::runtime::session_manager) async fn create_session(
        &self,
        current: &CurrentSessionCapability,
        usage: &UsageCapability,
        request: SessionCreateRequest,
    ) -> Result<SessionHandle, crate::PluginError> {
        let plan = resolve_session_create_plan(self, current, request).await?;
        let materialized = materialize_session_create_plan(current, &plan).await?;
        Box::pin(self.register_materialized_session(current, usage, plan, materialized)).await
    }

    pub(in crate::runtime::session_manager) async fn close_session(
        &self,
        current: &CurrentSessionCapability,
        usage: &UsageCapability,
        session_id: &str,
    ) -> Result<(), crate::PluginError> {
        if session_id == current.session_id {
            return Err(crate::PluginError::Session(
                "cannot close the current session".to_string(),
            ));
        }
        let running_turn = {
            let turns = super::turns::lock_turns(&self.turns);
            turns
                .iter()
                .find(|(_, turn)| turn.session_id == session_id)
                .map(|(turn_id, turn)| (turn_id.clone(), turn.registration, turns.len()))
        };
        if let Some((turn_id, registration, registered_turns)) = running_turn {
            tracing::debug!(
                session_id,
                registered_turns,
                holder_turn_id = %turn_id,
                holder_registration = registration,
                consulted = "managed_turn_registry",
                outcome = "denied",
                event = "managed_session.close",
                "managed session close denied: a turn is still running"
            );
            return Err(crate::PluginError::Session(format!(
                "cannot close session `{session_id}` while a turn is running"
            )));
        }
        self.registry.lock().await.remove(session_id);
        usage
            .child_sources
            .lock()
            .expect("child usage sources lock")
            .remove(session_id);
        current.plugins.host().unregister_session(session_id)?;
        Ok(())
    }
}
