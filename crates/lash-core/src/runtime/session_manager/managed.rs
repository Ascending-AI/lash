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
            let mut persisted_state = materialized.runtime.export_persisted_state();
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
        let observed_processes = apply_requested_process_observers(current, &plan).await;
        if let Some(store) = &materialized.store_binding {
            let pending = observed_processes
                .iter()
                .filter(|result| {
                    matches!(
                        result.outcome,
                        crate::SessionObservedProcessOutcome::Unavailable { .. }
                    )
                })
                .map(|result| result.process_id.clone())
                .collect();
            let mut meta = store
                .load_session_meta()
                .await
                .map_err(|error| {
                    crate::PluginError::Session(format!(
                        "failed to load session-create observer intent: {error}"
                    ))
                })?
                .ok_or_else(|| {
                    crate::PluginError::Session(
                        "created session has no metadata for observer intent settlement"
                            .to_string(),
                    )
                })?;
            meta.relation.settle_observer_intent(pending);
            store.save_session_meta(meta).await.map_err(|error| {
                crate::PluginError::Session(format!(
                    "failed to settle session-create observer intent: {error}"
                ))
            })?;
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
        self.register_materialized_session(current, usage, plan, materialized)
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

async fn apply_requested_process_observers(
    current: &CurrentSessionCapability,
    plan: &SessionCreatePlan,
) -> Vec<crate::SessionObservedProcessResult> {
    let mut outcomes = Vec::with_capacity(plan.protocol_request.observed_processes.len());
    for process_id in &plan.protocol_request.observed_processes {
        let outcome = match current.host.process_registry.as_ref() {
            None => crate::SessionObservedProcessOutcome::Unavailable {
                message: "process registry is unavailable in this runtime".to_string(),
            },
            Some(registry) => match registry.get_process(process_id).await {
                Ok(Some(_)) => match registry
                    .add_observer(
                        &plan.session_id,
                        process_id,
                        crate::ProcessObserverBy::host(format!(
                            "session-create:{}",
                            plan.session_id
                        )),
                    )
                    .await
                {
                    Ok(()) => crate::SessionObservedProcessOutcome::Observed,
                    Err(crate::PluginError::ProcessNoLongerRetained {
                        terminal_label,
                        pruned_at_ms,
                    }) => crate::SessionObservedProcessOutcome::NoLongerRetained {
                        terminal_label,
                        pruned_at_ms,
                    },
                    Err(error) => crate::SessionObservedProcessOutcome::Unavailable {
                        message: error.to_string(),
                    },
                },
                Ok(None) => crate::SessionObservedProcessOutcome::NotFound,
                Err(crate::PluginError::ProcessNoLongerRetained {
                    terminal_label,
                    pruned_at_ms,
                }) => crate::SessionObservedProcessOutcome::NoLongerRetained {
                    terminal_label,
                    pruned_at_ms,
                },
                Err(error) => crate::SessionObservedProcessOutcome::Unavailable {
                    message: error.to_string(),
                },
            },
        };
        outcomes.push(crate::SessionObservedProcessResult {
            process_id: process_id.clone(),
            outcome,
        });
    }
    outcomes
}
