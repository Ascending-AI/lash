use std::sync::Arc;

use lash_sansio::sync::MutexExt;
use serde_json::json;

use super::{test_tool, tool_id};
use crate::{
    PreparedToolCall, ToolCall, ToolContract, ToolId, ToolManifest, ToolOutcome, ToolPrepareCall,
    ToolProvider,
};

pub(super) struct GrantBindingProvider {
    pub(super) prepared_bindings: Arc<std::sync::Mutex<Vec<serde_json::Value>>>,
    pub(super) executed_bindings: Arc<std::sync::Mutex<Vec<serde_json::Value>>>,
}

#[async_trait::async_trait]
impl ToolProvider for GrantBindingProvider {
    fn tool_manifests(&self) -> Vec<ToolManifest> {
        Vec::new()
    }

    fn resolve_manifest_by_id(&self, id: &ToolId) -> Option<ToolManifest> {
        (id == &tool_id("host_only")).then(|| test_tool("host_only", "host-only").manifest())
    }

    fn resolve_contract(&self, _name: &str) -> Option<Arc<ToolContract>> {
        None
    }

    async fn prepare_tool_call(
        &self,
        call: ToolPrepareCall<'_>,
    ) -> Result<PreparedToolCall, ToolOutcome> {
        self.prepared_bindings
            .lock_recover()
            .push(call.context.tool_execution_binding().clone());
        Ok(PreparedToolCall::identity(call.tool_id, call.pending))
    }

    async fn execute(&self, call: ToolCall<'_>) -> crate::ToolAttemptOutcome {
        self.executed_bindings
            .lock_recover()
            .push(call.context.tool_execution_binding().clone());
        ToolOutcome::ok(json!(call.name())).into()
    }
}
