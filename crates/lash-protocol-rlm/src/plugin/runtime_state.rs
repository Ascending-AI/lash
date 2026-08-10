use std::collections::BTreeSet;
use std::sync::Arc;

use lash_core::plugin::{CodeExecutorPlugin, ProtocolSessionContext};
use lash_core::{SessionError, SessionHistoryRecord};
use lash_lashlang_runtime::{LashlangArtifactStore, LashlangSurface, SharedDeferredToolResolver};
use lash_rlm_types::{RlmGlobalsPatchPluginBody, RlmProtocolEvent};

use crate::executor::{
    RlmExecutionState, RlmLashlangExecutionTraceConfig, execute_code_with_bounds,
};
use crate::projection::{
    ProjectionResolver, RlmProjectedBindings, RlmProjectionExtension, decode_rlm_protocol_event,
};
use crate::rlm_support::{
    BoundVariableRenderCache, SharedBoundVariablesPrompt, render_bound_variables,
};

pub(super) struct RlmRuntimeState {
    projection_resolver: Arc<dyn ProjectionResolver>,
    artifact_store: Arc<dyn lashlang::LashlangArtifactStore>,
    lashlang_surface: LashlangSurface,
    deferred_tool_resolver: Option<SharedDeferredToolResolver>,
    lashlang_execution_trace_config: RlmLashlangExecutionTraceConfig,
    execution_bounds: lashlang::ExecutionBounds,
    session_projected_bindings: tokio::sync::Mutex<RlmProjectedBindings>,
    execution: tokio::sync::Mutex<Option<RlmExecutionState>>,
    active_agent_frame_id: tokio::sync::Mutex<Option<String>>,
    bound_variable_render_cache: tokio::sync::Mutex<BoundVariableRenderCache>,
    bound_variables_prompt: SharedBoundVariablesPrompt,
}

impl RlmRuntimeState {
    pub(super) fn new(
        projection_resolver: Arc<dyn ProjectionResolver>,
        artifact_store: Arc<dyn LashlangArtifactStore>,
        lashlang_surface: LashlangSurface,
        deferred_tool_resolver: Option<SharedDeferredToolResolver>,
        lashlang_execution_trace_config: RlmLashlangExecutionTraceConfig,
        execution_bounds: lashlang::ExecutionBounds,
    ) -> Result<Self, SessionError> {
        let mut bound_variable_render_cache = BoundVariableRenderCache::default();
        let bound_variables_prompt = Arc::new(std::sync::RwLock::new(render_bound_variables(
            &mut bound_variable_render_cache,
            &[],
        )));
        Ok(Self {
            execution: tokio::sync::Mutex::new(Some(RlmExecutionState::new()?)),
            projection_resolver,
            artifact_store,
            lashlang_surface,
            deferred_tool_resolver,
            lashlang_execution_trace_config,
            execution_bounds,
            session_projected_bindings: tokio::sync::Mutex::new(RlmProjectedBindings::new()),
            active_agent_frame_id: tokio::sync::Mutex::new(None),
            bound_variable_render_cache: tokio::sync::Mutex::new(bound_variable_render_cache),
            bound_variables_prompt,
        })
    }

    pub(super) async fn projected_binding_prompt_contributions(
        &self,
    ) -> Vec<lash_core::PromptContribution> {
        let bindings = self.session_projected_bindings.lock().await;
        RlmProjectionExtension::prompt_contributions_for(&bindings)
    }

    pub(super) fn shared_bound_variables_prompt(&self) -> SharedBoundVariablesPrompt {
        Arc::clone(&self.bound_variables_prompt)
    }

    async fn refresh_bound_variables_prompt(&self) {
        let globals = self.bound_variable_values().await;
        let mut cache = self.bound_variable_render_cache.lock().await;
        let rendered = render_bound_variables(&mut cache, &globals);
        *self
            .bound_variables_prompt
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = rendered;
    }

    /// Live top-level variables for the "Bound Variables" prompt section: the
    /// model's own scratch variables and any seeded computed globals, read from
    /// the live execution namespace (not reconstructed from events). Excludes
    /// read-only values; those render type-only in their own section.
    async fn bound_variable_values(&self) -> Vec<(String, lashlang::Value)> {
        let exclude = self.protected_projected_binding_names().await;
        self.execution
            .lock()
            .await
            .as_ref()
            .map(|execution| execution.bound_variable_values(&exclude))
            .unwrap_or_default()
    }

    async fn protected_projected_binding_names(&self) -> BTreeSet<String> {
        self.session_projected_bindings
            .lock()
            .await
            .names()
            .collect()
    }

    pub(super) async fn apply_session_extension(
        &self,
        extension: lash_core::ProtocolSessionExtensionHandle,
    ) -> Result<(), SessionError> {
        let extension = extension
            .as_any()
            .downcast_ref::<RlmProjectionExtension>()
            .ok_or_else(|| {
                SessionError::Protocol(
                    "RLM protocol received an unsupported session extension".to_string(),
                )
            })?;
        reject_reserved_projected_binding_names(&extension.bindings)?;
        let mut guard = self.session_projected_bindings.lock().await;
        let merged = guard
            .clone()
            .merge(extension.bindings.clone())
            .map_err(|err| SessionError::Protocol(err.to_string()))?;
        *guard = merged;
        drop(guard);
        self.refresh_bound_variables_prompt().await;
        Ok(())
    }

    pub(super) async fn validate_turn_extension(
        &self,
        extension: &lash_core::ProtocolTurnExtensionHandle,
    ) -> Result<(), SessionError> {
        let extension = extension
            .as_any()
            .downcast_ref::<RlmProjectionExtension>()
            .ok_or_else(|| {
                SessionError::Protocol(
                    "RLM protocol received an unsupported turn extension".to_string(),
                )
            })?;
        reject_reserved_projected_binding_names(&extension.bindings)?;
        self.session_projected_bindings
            .lock()
            .await
            .clone()
            .merge(extension.bindings.clone())
            .map(|_| ())
            .map_err(|err| SessionError::Protocol(err.to_string()))
    }

    pub(super) async fn restore_runtime_session_state(
        &self,
        state: &lash_core::runtime::RuntimeSessionState,
    ) -> Result<(), SessionError> {
        let mut active_agent_frame_id = self.active_agent_frame_id.lock().await;
        let mut execution_guard = self.execution.lock().await;
        let execution = execution_guard
            .as_mut()
            .ok_or_else(|| SessionError::Protocol("RLM execution state is busy".to_string()))?;
        if *active_agent_frame_id != state.current_frame_node_id {
            *execution = RlmExecutionState::new()?;
            *self.session_projected_bindings.lock().await = RlmProjectedBindings::new();
            *self.bound_variable_render_cache.lock().await = BoundVariableRenderCache::default();
            *active_agent_frame_id = state.current_frame_node_id.clone();
        }
        let protected_names = self.protected_projected_binding_names().await;
        if let Some(snapshot) = state.execution_state_snapshot().map(|bytes| bytes.to_vec()) {
            execution
                .restore_execution_state(&snapshot)
                .map_err(|error| SessionError::Protocol(error.to_string()))?;
            execution.prune_protected_globals(&protected_names);
        }
        for event in state.read_view().active_events() {
            if let SessionHistoryRecord::Protocol(event) = event
                && let Some(event) = decode_rlm_protocol_event(event)
            {
                self.apply_seed_or_globals_event(execution, event, &protected_names)
                    .await?;
            }
        }
        drop(execution_guard);
        drop(active_agent_frame_id);
        self.refresh_bound_variables_prompt().await;
        Ok(())
    }

    pub(super) async fn append_session_nodes(
        &self,
        nodes: &[lash_core::SessionAppendNode],
    ) -> Result<(), SessionError> {
        let mut execution_guard = self.execution.lock().await;
        let execution = execution_guard
            .as_mut()
            .ok_or_else(|| SessionError::Protocol("RLM execution state is busy".to_string()))?;
        let protected_names = self.protected_projected_binding_names().await;
        execution.prune_protected_globals(&protected_names);
        for node in nodes {
            if let lash_core::SessionAppendNode::ProtocolEvent { event, .. } = node
                && let Some(event) = decode_rlm_protocol_event(event)
            {
                self.apply_seed_or_globals_event(execution, event, &protected_names)
                    .await?;
            }
        }
        drop(execution_guard);
        self.refresh_bound_variables_prompt().await;
        Ok(())
    }

    pub(super) async fn execute_code(
        &self,
        ctx: lash_core::RuntimeExecutionContext<'_>,
        request: lash_core::ExecRequest,
    ) -> Result<lash_core::ExecResponse, SessionError> {
        let session_projected_bindings = self.session_projected_bindings.lock().await.clone();
        let mut guard = self.execution.lock().await;
        let state = guard
            .take()
            .ok_or_else(|| SessionError::Protocol("RLM execution state is busy".to_string()))?;

        let result = execute_code_with_bounds(
            state,
            ctx,
            request,
            Arc::clone(&self.artifact_store),
            self.lashlang_surface.clone(),
            self.deferred_tool_resolver.clone(),
            session_projected_bindings,
            Arc::clone(&self.projection_resolver),
            self.lashlang_execution_trace_config.clone(),
            self.execution_bounds,
        )
        .await;
        match result {
            Ok((state, response)) => {
                *guard = Some(state);
                drop(guard);
                self.refresh_bound_variables_prompt().await;
                Ok(response)
            }
            Err(err) => {
                *guard = Some(RlmExecutionState::new()?);
                drop(guard);
                self.refresh_bound_variables_prompt().await;
                Err(err)
            }
        }
    }

    pub(super) fn execution_state_dirty(&self) -> bool {
        self.execution
            .try_lock()
            .map(|execution| {
                execution
                    .as_ref()
                    .map(|execution| execution.execution_state_dirty())
                    .unwrap_or(true)
            })
            .unwrap_or(true)
    }

    pub(super) async fn snapshot_execution_state(&self) -> Result<Option<Vec<u8>>, SessionError> {
        self.execution
            .lock()
            .await
            .as_mut()
            .ok_or_else(|| SessionError::Protocol("RLM execution state is busy".to_string()))?
            .snapshot_execution_state()
    }

    pub(super) async fn restore_execution_state(&self, data: &[u8]) -> Result<(), SessionError> {
        let mut execution = self.execution.lock().await;
        execution
            .as_mut()
            .ok_or_else(|| SessionError::Protocol("RLM execution state is busy".to_string()))?
            .restore_execution_state(data)
            .map_err(|error| SessionError::Protocol(error.to_string()))?;
        drop(execution);
        self.refresh_bound_variables_prompt().await;
        Ok(())
    }

    async fn apply_seed_or_globals_event(
        &self,
        execution: &mut RlmExecutionState,
        event: RlmProtocolEvent,
        protected_names: &BTreeSet<String>,
    ) -> Result<(), SessionError> {
        match event {
            RlmProtocolEvent::RlmGlobalsPatch(patch) => {
                execution.patch_globals(&patch, protected_names)?;
            }
            RlmProtocolEvent::RlmSeed(seed) => {
                let mut protected_names = protected_names.clone();
                if !seed.projected.is_empty() {
                    self.install_initial_projected_seed(seed.projected)?;
                    protected_names = self.protected_projected_binding_names().await;
                }
                if !seed.globals.is_empty() {
                    execution.patch_globals(
                        &RlmGlobalsPatchPluginBody {
                            set_default: seed.globals,
                        },
                        &protected_names,
                    )?;
                }
            }
            RlmProtocolEvent::RlmAssistantContent(_)
            | RlmProtocolEvent::RlmTrajectoryEntry(_)
            | RlmProtocolEvent::RlmDiagnostic(_) => {}
        }
        Ok(())
    }

    fn install_initial_projected_seed(
        &self,
        snapshot: lash_rlm_types::RlmProjectedSeedSnapshot,
    ) -> Result<(), SessionError> {
        let bindings = match RlmProjectedBindings::from_snapshot(&snapshot) {
            Ok(bindings) => bindings,
            Err(err) => {
                return Err(SessionError::Protocol(format!(
                    "rlm projected seed snapshot rejected: {err}"
                )));
            }
        };
        reject_reserved_projected_binding_names(&bindings)?;
        let mut guard = match self.session_projected_bindings.try_lock() {
            Ok(guard) => guard,
            Err(_) => return Err(SessionError::Protocol(
                "rlm projected seed snapshot could not be installed because session bindings were contended".to_string(),
            )),
        };
        let merged = guard
            .clone()
            .merge(bindings)
            .map_err(|err| SessionError::Protocol(err.to_string()))?;
        *guard = merged;
        Ok(())
    }
}

pub(super) struct RlmCodeExecutor {
    state: Arc<RlmRuntimeState>,
}

impl RlmCodeExecutor {
    pub(super) fn new(state: Arc<RlmRuntimeState>) -> Self {
        Self { state }
    }
}

#[async_trait::async_trait]
impl CodeExecutorPlugin for RlmCodeExecutor {
    async fn execute_code(
        &self,
        ctx: lash_core::RuntimeExecutionContext<'_>,
        request: lash_core::ExecRequest,
    ) -> Result<lash_core::ExecResponse, SessionError> {
        self.state.execute_code(ctx, request).await
    }

    fn execution_state_dirty(&self) -> bool {
        self.state.execution_state_dirty()
    }

    async fn snapshot_execution_state(
        &self,
        _ctx: ProtocolSessionContext<'_>,
    ) -> Result<Option<Vec<u8>>, SessionError> {
        self.state.snapshot_execution_state().await
    }

    async fn restore_execution_state(
        &self,
        _ctx: ProtocolSessionContext<'_>,
        data: &[u8],
    ) -> Result<(), SessionError> {
        self.state.restore_execution_state(data).await
    }
}

pub(super) fn reject_reserved_projected_binding_names(
    bindings: &RlmProjectedBindings,
) -> Result<(), SessionError> {
    if bindings.names().any(|name| name == "history") {
        return Err(SessionError::Protocol(
            "`history` is reserved as an RLM built-in binding".to_string(),
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::projection::ProjectionRegistry;

    #[test]
    fn executing_code_refreshes_the_driver_bound_variables_snapshot() {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime")
            .block_on(async {
                let state = RlmRuntimeState::new(
                    Arc::new(ProjectionRegistry::new()),
                    lashlang::global_in_memory_lashlang_artifact_store(),
                    LashlangSurface::new(
                        lashlang::LashlangAbilities::default(),
                        lashlang::LashlangLanguageFeatures::default(),
                        lashlang::LashlangHostCatalog::new(),
                    ),
                    None,
                    RlmLashlangExecutionTraceConfig::default(),
                    lashlang::ExecutionBounds::unbounded(),
                )
                .expect("runtime state");
                let prompt = state.shared_bound_variables_prompt();
                assert!(!prompt.read().expect("prompt read").contains("scratch_note"));

                state
                    .execute_code(
                        lash_core::testing::code_execution_context(),
                        lash_core::ExecRequest {
                            language: "lashlang".to_string(),
                            code: "scratch_note = \"after execution\"".to_string(),
                            accept_finish: true,
                        },
                    )
                    .await
                    .expect("execute code");

                assert!(
                    prompt
                        .read()
                        .expect("prompt read")
                        .contains("- `scratch_note` = after execution")
                );
            });
    }
}
