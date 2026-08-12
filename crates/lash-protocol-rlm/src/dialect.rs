mod lashlang;

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use lash_core::{ExecRequest, ExecResponse, RuntimeExecutionContext, SessionError};
use lash_rlm_types::RlmGlobalsPatchPluginBody;

pub(crate) use lashlang::{LashlangDialect, LashlangDialectServices};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct CellTags {
    pub(crate) open: &'static str,
    pub(crate) close: &'static str,
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub(crate) enum DialectRegistryError {
    #[error("RLM language `{language}` is not registered")]
    Unregistered { language: String },
    #[error("RLM language `{language}` is registered but session language `{active}` is pinned")]
    Inactive { language: String, active: String },
}

#[async_trait::async_trait]
pub(crate) trait RlmDialectSession: Send {
    async fn execute(
        &mut self,
        ctx: RuntimeExecutionContext<'_>,
        request: ExecRequest,
        session_projected_bindings: crate::projection::RlmProjectedBindings,
    ) -> Result<ExecResponse, SessionError>;

    fn execution_state_dirty(&self) -> bool;

    fn snapshot_execution_state(
        &mut self,
    ) -> Result<lash_core::plugin::ExecutionStateSnapshot, SessionError>;

    fn probe_execution_state_capture(&mut self) -> Result<(), SessionError>;

    fn hydrated_execution_state(
        &self,
    ) -> Result<lash_core::plugin::HydratedExecutionState, SessionError>;

    fn acknowledge_execution_state_capture(&mut self);

    fn abort_execution_state_capture(&mut self);

    fn restore_execution_state(
        &mut self,
        state: &lash_core::plugin::HydratedExecutionState,
    ) -> Result<(), SessionError>;

    fn prune_protected_globals(&mut self, protected_names: &BTreeSet<String>);

    fn patch_globals(
        &mut self,
        patch: &RlmGlobalsPatchPluginBody,
        protected_names: &BTreeSet<String>,
    ) -> Result<(), SessionError>;

    fn render_bound_variables(&mut self, exclude: &BTreeSet<String>) -> Arc<str>;
}

pub(crate) trait RlmDialect: Send + Sync {
    fn language_id(&self) -> &'static str;

    fn snapshot_engine_id(&self) -> &'static str;

    fn cell_tags(&self) -> CellTags;

    fn create_session(&self) -> Result<Box<dyn RlmDialectSession>, SessionError>;

    fn render_execution_section(
        &self,
        features: crate::protocol::RlmPromptFeatures,
        tool_catalog: &lash_core::ToolCatalog,
    ) -> Result<String, SessionError>;

    fn render_history_cell(&self, prose: &str, code: &str) -> String {
        crate::cell_scan::render_cell_text(self.cell_tags(), prose, code)
    }

    fn finalization_copy(&self, termination: &lash_rlm_types::RlmTermination) -> &'static str;

    fn cell_error_message(&self, error: crate::protocol::CellExtractionError) -> String;

    fn turn_limit_final_copy(&self, max_turns: usize) -> String;

    fn finish_required_copy(&self, requires_schema: bool) -> String;

    fn finish_schema_mismatch_copy(&self) -> String;

    fn invalid_cell_retry_copy(&self, error_text: &str) -> String;

    fn output_limit_cell_copy(&self, output_token_cap: Option<usize>) -> String;

    fn code_stream_kind(&self) -> &'static str;

    fn execution_diagnostic_name(&self) -> &'static str;

    fn stream_cell_start_event_name(&self) -> &'static str;

    fn stream_cell_end_event_name(&self) -> &'static str;
}

#[derive(Clone)]
pub(crate) struct RlmDialectRegistry {
    dialects: Arc<BTreeMap<&'static str, Arc<dyn RlmDialect>>>,
}

impl RlmDialectRegistry {
    pub(crate) fn new(dialects: impl IntoIterator<Item = Arc<dyn RlmDialect>>) -> Self {
        let dialects = dialects
            .into_iter()
            .map(|dialect| (dialect.language_id(), dialect))
            .collect();
        Self {
            dialects: Arc::new(dialects),
        }
    }

    pub(crate) fn resolve(
        &self,
        language: &str,
    ) -> Result<Arc<dyn RlmDialect>, DialectRegistryError> {
        self.dialects
            .get(language)
            .cloned()
            .ok_or_else(|| DialectRegistryError::Unregistered {
                language: language.to_string(),
            })
    }

    pub(crate) fn resolve_active(
        &self,
        language: &str,
        active: &str,
    ) -> Result<Arc<dyn RlmDialect>, DialectRegistryError> {
        let dialect = self.resolve(language)?;
        if language != active {
            return Err(DialectRegistryError::Inactive {
                language: language.to_string(),
                active: active.to_string(),
            });
        }
        Ok(dialect)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn registry_returns_typed_error_for_unregistered_language() {
        let registry = RlmDialectRegistry::new(Vec::new());
        let error = match registry.resolve("typescript") {
            Ok(_) => panic!("unregistered language resolved"),
            Err(error) => error,
        };
        assert_eq!(
            error,
            DialectRegistryError::Unregistered {
                language: "typescript".to_string()
            }
        );
    }
}
