use super::create_plan::{SessionCreatePlan, resolve_session_create_plan};
use super::materialize::{MaterializedSession, materialize_session_create_plan};
use super::*;

impl ManagedSessionCapability {
    async fn register_materialized_session(
        &self,
        usage: &UsageCapability,
        plan: SessionCreatePlan,
        materialized: MaterializedSession,
    ) -> Result<SessionHandle, crate::PluginError> {
        if let Some(store) = &materialized.store_binding {
            let mut persisted_state = materialized.runtime.export_persisted_state();
            super::normalize_session_graph(&mut persisted_state);
            persisted_state.graph_replace_required = true;
            let commit = crate::store::RuntimeCommit::persisted_state(&persisted_state, &[])
                .with_operation(super::super::state::revision_operation(
                    &persisted_state,
                    "create-session",
                ))
                .map_err(|err| crate::PluginError::Session(err.to_string()))?;
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
        }
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
        self.register_materialized_session(usage, plan, materialized)
            .await
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
        if self
            .turns
            .lock()
            .await
            .values()
            .any(|turn| turn.session_id == session_id)
        {
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
