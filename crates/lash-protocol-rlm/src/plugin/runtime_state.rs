use std::collections::BTreeSet;
use std::sync::Arc;

use lash_core::plugin::{CodeExecutorPlugin, ProtocolSessionContext};
use lash_core::{SessionError, SessionHistoryRecord};
use lash_rlm_types::{RlmGlobalsPatchPluginBody, RlmProtocolEvent};

use crate::dialect::{RlmDialect, RlmDialectRegistry, RlmDialectSession};
use crate::projection::{RlmProjectedBindings, RlmProjectionExtension, decode_rlm_protocol_event};
use crate::rlm_support::{
    BoundVariableRenderCache, SharedBoundVariablesPrompt, render_bound_variables,
};

pub(super) struct RlmRuntimeState {
    dialect_registry: RlmDialectRegistry,
    dialect: Arc<dyn RlmDialect>,
    session_projected_bindings: tokio::sync::Mutex<RlmProjectedBindings>,
    execution: tokio::sync::Mutex<Option<Box<dyn RlmDialectSession>>>,
    active_agent_frame_id: tokio::sync::Mutex<Option<String>>,
    bound_variables_prompt: SharedBoundVariablesPrompt,
}

impl RlmRuntimeState {
    pub(super) fn new(
        dialect_registry: RlmDialectRegistry,
        dialect: Arc<dyn RlmDialect>,
    ) -> Result<Self, SessionError> {
        let execution = dialect.create_session()?;
        let bound_variables_prompt = Arc::new(std::sync::RwLock::new(
            execution
                .prepare_bound_variables_prompt(&BTreeSet::new())?
                .render(),
        ));
        Ok(Self {
            execution: tokio::sync::Mutex::new(Some(execution)),
            dialect_registry,
            dialect,
            session_projected_bindings: tokio::sync::Mutex::new(RlmProjectedBindings::new()),
            active_agent_frame_id: tokio::sync::Mutex::new(None),
            bound_variables_prompt,
        })
    }

    #[cfg(test)]
    pub(super) fn new_lashlang_for_tests() -> Result<Self, SessionError> {
        Self::new_for_tests("lashlang")
    }

    #[cfg(test)]
    fn new_for_tests(active_language: &str) -> Result<Self, SessionError> {
        let services = crate::dialect::LashlangDialectServices {
            projection_resolver: Arc::new(crate::projection::ProjectionRegistry::new()),
            artifact_store: lashlang::global_in_memory_lashlang_artifact_store(),
            deferred_tool_resolver: None,
            execution_trace_config: crate::executor::RlmLashlangExecutionTraceConfig::default(),
            execution_bounds: crate::plugin::ExecutionBounds::unbounded(),
        };
        let dialect: Arc<dyn RlmDialect> = Arc::new(crate::dialect::LashlangDialect::new(
            lash_lashlang_runtime::LashlangSurface::default(),
            services.clone(),
        ));
        let typescript: Arc<dyn RlmDialect> = Arc::new(crate::dialect::TypescriptDialect::new(
            lash_lashlang_runtime::LashlangSurface::default(),
            services,
        ));
        let active = match active_language {
            "lashlang" => Arc::clone(&dialect),
            "typescript" => Arc::clone(&typescript),
            other => panic!("unknown test dialect `{other}`"),
        };
        Self::new(RlmDialectRegistry::new([dialect, typescript]), active)
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

    async fn refresh_bound_variables_prompt(&self) -> Result<(), SessionError> {
        let exclude = self.protected_projected_binding_names().await;
        let prepared = self
            .execution
            .lock()
            .await
            .as_ref()
            .map(|execution| execution.prepare_bound_variables_prompt(&exclude))
            .transpose()?;
        let rendered = prepared.map_or_else(
            || {
                let mut cache = BoundVariableRenderCache::default();
                render_bound_variables(&mut cache, &[])
            },
            crate::dialect::BoundVariablesPromptRender::render,
        );
        *self
            .bound_variables_prompt
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = rendered;
        Ok(())
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
        self.refresh_bound_variables_prompt().await?;
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
            *execution = self.dialect.create_session()?;
            *self.session_projected_bindings.lock().await = RlmProjectedBindings::new();
            *active_agent_frame_id = state.current_frame_node_id.clone();
        }
        let protected_names = self.protected_projected_binding_names().await;
        if let Some(snapshot) =
            state
                .execution_state_hydration()
                .map_err(|error| SessionError::Store {
                    context: "failed to hydrate RLM execution-state components".to_string(),
                    source: error,
                })?
        {
            execution.restore_execution_state(&snapshot)?;
            execution.prune_protected_globals(&protected_names)?;
        }
        for event in state.read_view().active_events() {
            if let SessionHistoryRecord::Protocol(event) = event
                && let Some(event) = decode_rlm_protocol_event(event)
            {
                self.apply_seed_or_globals_event(execution.as_mut(), event, &protected_names)
                    .await?;
            }
        }
        drop(execution_guard);
        drop(active_agent_frame_id);
        self.refresh_bound_variables_prompt().await?;
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
        execution.prune_protected_globals(&protected_names)?;
        for node in nodes {
            if let lash_core::SessionAppendNode::ProtocolEvent { event, .. } = node
                && let Some(event) = decode_rlm_protocol_event(event)
            {
                self.apply_seed_or_globals_event(execution.as_mut(), event, &protected_names)
                    .await?;
            }
        }
        drop(execution_guard);
        self.refresh_bound_variables_prompt().await?;
        Ok(())
    }

    pub(super) async fn execute_code(
        &self,
        ctx: lash_core::RuntimeExecutionContext<'_>,
        request: lash_core::ExecRequest,
    ) -> Result<lash_core::ExecResponse, SessionError> {
        self.dialect_registry
            .resolve_active(&request.language, self.dialect.language_id())
            .map_err(|error| SessionError::Protocol(error.to_string()))?;
        let session_projected_bindings = self.session_projected_bindings.lock().await.clone();
        let mut guard = self.execution.lock().await;
        let mut state = guard
            .take()
            .ok_or_else(|| SessionError::Protocol("RLM execution state is busy".to_string()))?;

        let result = state
            .execute(ctx, request, session_projected_bindings)
            .await;
        *guard = Some(state);
        drop(guard);
        self.refresh_bound_variables_prompt().await?;
        result
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

    pub(super) async fn snapshot_execution_state(
        &self,
    ) -> Result<lash_core::plugin::ExecutionStateSnapshot, SessionError> {
        self.execution
            .lock()
            .await
            .as_mut()
            .ok_or_else(|| SessionError::Protocol("RLM execution state is busy".to_string()))?
            .snapshot_execution_state()
    }

    pub(super) async fn probe_execution_state_capture(&self) -> Result<(), SessionError> {
        self.execution
            .lock()
            .await
            .as_mut()
            .ok_or_else(|| SessionError::Protocol("RLM execution state is busy".to_string()))?
            .probe_execution_state_capture()
    }

    pub(super) async fn hydrated_execution_state(
        &self,
    ) -> Result<Option<lash_core::plugin::HydratedExecutionState>, SessionError> {
        self.execution
            .lock()
            .await
            .as_ref()
            .ok_or_else(|| SessionError::Protocol("RLM execution state is busy".to_string()))?
            .hydrated_execution_state()
            .map(Some)
    }

    pub(super) async fn acknowledge_execution_state_capture(&self) {
        if let Some(execution) = self.execution.lock().await.as_mut() {
            let _ = execution.acknowledge_execution_state_capture();
        }
    }

    pub(super) async fn abort_execution_state_capture(&self) {
        if let Some(execution) = self.execution.lock().await.as_mut() {
            let _ = execution.abort_execution_state_capture();
        }
    }

    pub(super) async fn restore_execution_state(
        &self,
        state: &lash_core::plugin::HydratedExecutionState,
    ) -> Result<(), SessionError> {
        let mut execution = self.execution.lock().await;
        execution
            .as_mut()
            .ok_or_else(|| SessionError::Protocol("RLM execution state is busy".to_string()))?
            .restore_execution_state(state)?;
        drop(execution);
        self.refresh_bound_variables_prompt().await?;
        Ok(())
    }

    async fn apply_seed_or_globals_event(
        &self,
        execution: &mut dyn RlmDialectSession,
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
    ) -> Result<lash_core::plugin::ExecutionStateSnapshot, SessionError> {
        self.state.snapshot_execution_state().await
    }

    async fn probe_execution_state_capture(
        &self,
        _ctx: ProtocolSessionContext<'_>,
    ) -> Result<(), SessionError> {
        self.state.probe_execution_state_capture().await
    }

    async fn hydrated_execution_state(
        &self,
        _ctx: ProtocolSessionContext<'_>,
    ) -> Result<Option<lash_core::plugin::HydratedExecutionState>, SessionError> {
        self.state.hydrated_execution_state().await
    }

    async fn restore_execution_state(
        &self,
        _ctx: ProtocolSessionContext<'_>,
        state: &lash_core::plugin::HydratedExecutionState,
    ) -> Result<(), SessionError> {
        self.state.restore_execution_state(state).await
    }

    async fn acknowledge_execution_state_capture(&self) {
        self.state.acknowledge_execution_state_capture().await;
    }

    async fn abort_execution_state_capture(&self) {
        self.state.abort_execution_state_capture().await;
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

    #[test]
    fn missing_dialect_session_renders_canonical_empty_bound_variables_prompt() {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime")
            .block_on(async {
                let state = RlmRuntimeState::new_lashlang_for_tests().expect("runtime state");
                *state.execution.lock().await = None;

                state
                    .refresh_bound_variables_prompt()
                    .await
                    .expect("refresh empty prompt");

                let prompt = state.bound_variables_prompt.read().unwrap().clone();
                assert!(prompt.starts_with("These variables are already bound in lashlang."));
                assert!(
                    prompt.contains(
                        "Available variables:\n- `history`: `list[HistoryItem]`, read-only"
                    )
                );
            });
    }

    #[test]
    fn executing_code_refreshes_the_driver_bound_variables_snapshot() {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime")
            .block_on(async {
                let state = RlmRuntimeState::new_lashlang_for_tests().expect("runtime state");
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

    #[test]
    fn execute_code_uses_the_selected_typescript_dialect_and_rejects_unknown_languages() {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime")
            .block_on(async {
                let state = RlmRuntimeState::new_for_tests("typescript").expect("runtime state");
                let response = state
                    .execute_code(
                        lash_core::testing::code_execution_context(),
                        lash_core::ExecRequest {
                            language: "typescript".to_string(),
                            code: "const answer: number = 40 + 2; finish(answer);".to_string(),
                            accept_finish: true,
                        },
                    )
                    .await
                    .expect("selected TypeScript dialect must execute");
                assert_eq!(response.error, None);
                assert_eq!(response.terminal_finish, Some(serde_json::json!(42)));

                let error = state
                    .execute_code(
                        lash_core::testing::code_execution_context(),
                        lash_core::ExecRequest {
                            language: "python".to_string(),
                            code: "finish(42)".to_string(),
                            accept_finish: true,
                        },
                    )
                    .await
                    .expect_err("unregistered language must be rejected");

                assert!(matches!(
                    error,
                    SessionError::Protocol(message)
                        if message == "RLM language `python` is not registered"
                ));

                // The registered-but-inactive case is the one that matters for
                // dialect integrity: `lashlang` is a real, registered dialect,
                // and this session is pinned to `typescript`. Without this
                // fence a TypeScript session would execute a `<lashlang>` cell
                // — the cross-dialect violation `runbooks/RULES.md` treats as
                // an abort-and-RCA event. An earlier revision of this test
                // covered only the unregistered language, and deleting the
                // fence left the whole package green.
                let inactive = state
                    .execute_code(
                        lash_core::testing::code_execution_context(),
                        lash_core::ExecRequest {
                            language: "lashlang".to_string(),
                            code: "finish(42)".to_string(),
                            accept_finish: true,
                        },
                    )
                    .await
                    .expect_err("a registered but inactive dialect must be rejected");

                assert!(
                    matches!(
                        &inactive,
                        SessionError::Protocol(message)
                            if message
                                == "RLM language `lashlang` is registered but session \
                                    language `typescript` is pinned"
                    ),
                    "{inactive:?}"
                );
            });
    }
}
