use std::collections::BTreeSet;
use std::sync::Arc;

use lash_core::{ExecRequest, ExecResponse, RuntimeExecutionContext, SessionError};
use lash_lashlang_runtime::LashlangSurface;
use lash_rlm_types::RlmGlobalsPatchPluginBody;

use super::{
    BoundVariablesPromptRender, CellTags, LashlangDialectServices, RlmDialect, RlmDialectSession,
};
use crate::executor::{RlmExecutionState, execute_typescript_code_with_bounds};
use crate::projection::RlmProjectedBindings;
use crate::rlm_support::{BoundVariableRenderCache, render_bound_variables};

pub(crate) const LANGUAGE_ID: &str = "typescript";

pub(crate) struct TypescriptDialect {
    surface: LashlangSurface,
    services: LashlangDialectServices,
}

impl TypescriptDialect {
    pub(crate) fn new(surface: LashlangSurface, services: LashlangDialectServices) -> Self {
        Self { surface, services }
    }
}

impl RlmDialect for TypescriptDialect {
    fn language_id(&self) -> &'static str {
        LANGUAGE_ID
    }

    fn snapshot_engine_id(&self) -> &'static str {
        LANGUAGE_ID
    }

    fn cell_tags(&self) -> CellTags {
        CellTags {
            open: "<typescript>",
            close: "</typescript>",
        }
    }

    fn create_session(&self) -> Result<Box<dyn RlmDialectSession>, SessionError> {
        Ok(Box::new(TypescriptDialectSession {
            state: Some(RlmExecutionState::for_engine(LANGUAGE_ID)?),
            surface: self.surface.clone(),
            services: self.services.clone(),
            bound_variable_render_cache: Arc::new(std::sync::Mutex::new(
                BoundVariableRenderCache::default(),
            )),
        }))
    }

    fn render_execution_section(
        &self,
        _features: crate::protocol::RlmPromptFeatures,
        _tool_catalog: &lash_core::ToolCatalog,
    ) -> Result<String, SessionError> {
        Ok("## TypeScript execution\n\nUse exactly one paired `<typescript>...</typescript>` block. The accepted TypeScript subset executes on the durable Lash VM. Call `finish(value)` to terminate and `print(value)` for observations. Unsupported JavaScript or TypeScript constructs fail before execution with a named diagnostic.".to_string())
    }

    fn finalization_copy(&self, termination: &lash_rlm_types::RlmTermination) -> &'static str {
        match termination {
            lash_rlm_types::RlmTermination::FinishRequired { .. } => {
                "This turn requires a final value. Reply with one paired `<typescript>...</typescript>` block that calls `finish(value)`."
            }
            lash_rlm_types::RlmTermination::Natural => {
                "Continue with one paired `<typescript>...</typescript>` block, or finish with prose and no block. A call to `finish(value)` returns a computed final value."
            }
        }
    }

    fn cell_error_message(&self, error: crate::protocol::CellExtractionError) -> String {
        match error {
            crate::protocol::CellExtractionError::UnclosedCell => {
                "Model response started a `<typescript>` block but did not close it. Retry with one complete paired block.".to_string()
            }
            crate::protocol::CellExtractionError::MultipleCells => {
                "Model response contained multiple `<typescript>...</typescript>` blocks. Retry with exactly one paired block.".to_string()
            }
        }
    }

    fn turn_limit_final_copy(&self, max_turns: usize) -> String {
        format!(
            "Turn limit reached ({max_turns}). Reply in plain prose with accomplishments, remaining work, and next steps; do not emit a TypeScript block."
        )
    }

    fn finish_required_copy(&self, requires_schema: bool) -> String {
        if requires_schema {
            "Call `finish(value)` from one paired `<typescript>...</typescript>` block with a value matching the required output schema.".to_string()
        } else {
            "Call `finish(value)` from one paired `<typescript>...</typescript>` block.".to_string()
        }
    }

    fn finish_schema_mismatch_copy(&self) -> String {
        "The `finish` value did not match the required output schema. Correct it and call `finish(value)` again.".to_string()
    }

    fn invalid_cell_retry_copy(&self, error_text: &str) -> String {
        format!(
            "{error_text}\n\nReply again using exactly one paired `<typescript>...</typescript>` block."
        )
    }

    fn output_limit_cell_copy(&self, output_token_cap: Option<usize>) -> String {
        let cap = output_token_cap
            .map(|cap| format!(" The request cap was {cap} tokens."))
            .unwrap_or_default();
        format!(
            "Model output truncated the `<typescript>` block before `</typescript>`.{cap} Retry with a shorter block."
        )
    }

    fn code_stream_kind(&self) -> &'static str {
        "typescript_code"
    }

    fn execution_diagnostic_name(&self) -> &'static str {
        "execute_typescript"
    }

    fn stream_cell_start_event_name(&self) -> &'static str {
        "rlm_typescript_cell_start"
    }

    fn stream_cell_end_event_name(&self) -> &'static str {
        "rlm_typescript_cell_end"
    }
}

struct TypescriptDialectSession {
    state: Option<RlmExecutionState>,
    surface: LashlangSurface,
    services: LashlangDialectServices,
    bound_variable_render_cache: Arc<std::sync::Mutex<BoundVariableRenderCache>>,
}

impl TypescriptDialectSession {
    fn state(&self) -> Result<&RlmExecutionState, SessionError> {
        self.state
            .as_ref()
            .ok_or_else(|| SessionError::Protocol("RLM execution state is busy".to_string()))
    }

    fn state_mut(&mut self) -> Result<&mut RlmExecutionState, SessionError> {
        self.state
            .as_mut()
            .ok_or_else(|| SessionError::Protocol("RLM execution state is busy".to_string()))
    }
}

#[async_trait::async_trait]
impl RlmDialectSession for TypescriptDialectSession {
    async fn execute(
        &mut self,
        ctx: RuntimeExecutionContext<'_>,
        request: ExecRequest,
        session_projected_bindings: RlmProjectedBindings,
    ) -> Result<ExecResponse, SessionError> {
        if self.state.is_none() {
            return Err(SessionError::Protocol(
                "RLM execution state is busy".to_string(),
            ));
        }
        let reset_state = RlmExecutionState::for_engine(LANGUAGE_ID)?;
        let state = self
            .state
            .take()
            .ok_or_else(|| SessionError::Protocol("RLM execution state is busy".to_string()))?;
        let result = execute_typescript_code_with_bounds(
            state,
            ctx,
            request,
            Arc::clone(&self.services.artifact_store),
            self.surface.clone(),
            self.services.deferred_tool_resolver.clone(),
            session_projected_bindings,
            Arc::clone(&self.services.projection_resolver),
            self.services.execution_trace_config.clone(),
            self.services.execution_bounds.into_engine(),
        )
        .await;
        match result {
            Ok((state, response)) => {
                self.state = Some(state);
                Ok(response)
            }
            Err(error) => {
                self.state = Some(reset_state);
                Err(error)
            }
        }
    }

    fn execution_state_dirty(&self) -> bool {
        self.state
            .as_ref()
            .map(RlmExecutionState::execution_state_dirty)
            .unwrap_or(true)
    }

    fn snapshot_execution_state(
        &mut self,
    ) -> Result<lash_core::plugin::ExecutionStateSnapshot, SessionError> {
        self.state_mut()?.snapshot_execution_state()
    }

    fn probe_execution_state_capture(&mut self) -> Result<(), SessionError> {
        self.state_mut()?.probe_execution_state_capture()
    }

    fn hydrated_execution_state(
        &self,
    ) -> Result<lash_core::plugin::HydratedExecutionState, SessionError> {
        self.state()?.hydrated_execution_state()
    }

    fn acknowledge_execution_state_capture(&mut self) -> Result<(), SessionError> {
        self.state_mut()?.acknowledge_execution_state_capture();
        Ok(())
    }

    fn abort_execution_state_capture(&mut self) -> Result<(), SessionError> {
        self.state_mut()?.abort_execution_state_capture();
        Ok(())
    }

    fn restore_execution_state(
        &mut self,
        state: &lash_core::plugin::HydratedExecutionState,
    ) -> Result<(), SessionError> {
        self.state_mut()?
            .restore_execution_state(state)
            .map_err(|error| SessionError::Protocol(error.to_string()))
    }

    fn prune_protected_globals(
        &mut self,
        protected_names: &BTreeSet<String>,
    ) -> Result<(), SessionError> {
        self.state_mut()?.prune_protected_globals(protected_names);
        Ok(())
    }

    fn patch_globals(
        &mut self,
        patch: &RlmGlobalsPatchPluginBody,
        protected_names: &BTreeSet<String>,
    ) -> Result<(), SessionError> {
        self.state_mut()?.patch_globals(patch, protected_names)
    }

    fn prepare_bound_variables_prompt(
        &self,
        exclude: &BTreeSet<String>,
    ) -> Result<BoundVariablesPromptRender, SessionError> {
        let mut globals = self.state()?.bound_variable_values(exclude);
        // A block-scoped binding that shadows an outer name is lowered to a
        // generated slot. It is the author's value under a name the author
        // never wrote, and it is dead by the time any turn boundary renders,
        // so it is never a bound variable the model should see.
        globals.retain(|(name, _)| !name.starts_with(lash_typescript::GENERATED_BINDING_PREFIX));
        let cache = Arc::clone(&self.bound_variable_render_cache);
        Ok(BoundVariablesPromptRender::new(move || {
            let mut cache = cache
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            render_bound_variables(&mut cache, &globals)
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identity_and_cell_tags_are_typescript() {
        let dialect = TypescriptDialect::new(
            LashlangSurface::default(),
            LashlangDialectServices {
                projection_resolver: Arc::new(crate::projection::ProjectionRegistry::new()),
                artifact_store: lashlang::global_in_memory_lashlang_artifact_store(),
                deferred_tool_resolver: None,
                execution_trace_config: crate::executor::RlmLashlangExecutionTraceConfig::default(),
                execution_bounds: crate::plugin::ExecutionBounds::unbounded(),
            },
        );
        assert_eq!(dialect.language_id(), "typescript");
        assert_eq!(dialect.snapshot_engine_id(), "typescript");
        assert_eq!(dialect.cell_tags().open, "<typescript>");
        assert_eq!(dialect.cell_tags().close, "</typescript>");
    }

    #[test]
    fn session_executes_a_typescript_request_end_to_end() {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime")
            .block_on(async {
                let dialect = TypescriptDialect::new(
                    LashlangSurface::default(),
                    LashlangDialectServices {
                        projection_resolver: Arc::new(crate::projection::ProjectionRegistry::new()),
                        artifact_store: lashlang::global_in_memory_lashlang_artifact_store(),
                        deferred_tool_resolver: None,
                        execution_trace_config:
                            crate::executor::RlmLashlangExecutionTraceConfig::default(),
                        execution_bounds: crate::plugin::ExecutionBounds::unbounded(),
                    },
                );
                let mut session = dialect.create_session().expect("typescript session");
                let response = session
                    .execute(
                        lash_core::testing::code_execution_context(),
                        ExecRequest {
                            language: "typescript".to_string(),
                            code: "const answer: number = 40 + 2; finish(answer);".to_string(),
                            accept_finish: true,
                        },
                        RlmProjectedBindings::new(),
                    )
                    .await
                    .expect("execute typescript");

                assert_eq!(response.error, None);
                assert_eq!(response.terminal_finish, Some(serde_json::json!(42)));
            });
    }
}
