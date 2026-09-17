use std::sync::Arc;

use lash_sansio::sync::MutexExt;
use serde_json::json;

use super::{MockTool, test_tool, tool_id};
use crate::tool_registry::{ToolProviderSource, ToolRegistry};
use crate::{
    PreparedToolCall, ToolCall, ToolContract, ToolId, ToolManifest, ToolOutcome, ToolPrepareCall,
    ToolProvider,
};

pub(super) struct GrantBindingProvider {
    pub(super) prepared_bindings: Arc<std::sync::Mutex<Vec<serde_json::Value>>>,
    pub(super) executed_bindings: Arc<std::sync::Mutex<Vec<serde_json::Value>>>,
}

struct GrantDeferralProvider {
    may_defer: bool,
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

    async fn execute(&self, call: ToolCall<'_>) -> ToolOutcome {
        self.executed_bindings
            .lock_recover()
            .push(call.context.tool_execution_binding().clone());
        ToolOutcome::ok(json!(call.name))
    }
}

#[async_trait::async_trait]
impl ToolProvider for GrantDeferralProvider {
    fn tool_manifests(&self) -> Vec<ToolManifest> {
        Vec::new()
    }

    fn resolve_manifest_by_id(&self, id: &ToolId) -> Option<ToolManifest> {
        (id == &tool_id("host_only")).then(|| test_tool("host_only", "host-only").manifest())
    }

    fn resolve_contract(&self, _name: &str) -> Option<Arc<ToolContract>> {
        None
    }

    fn attempt_may_defer(&self, id: &ToolId) -> bool {
        self.may_defer && id == &tool_id("host_only")
    }

    async fn execute(&self, _call: ToolCall<'_>) -> ToolOutcome {
        ToolOutcome::ok(json!("host_only"))
    }
}

pub(super) fn grant_deferral_registry(may_defer: bool) -> ToolRegistry {
    let registry = ToolRegistry::from_tool_provider(Arc::new(MockTool)).expect("registry");
    registry
        .upsert_source(Arc::new(ToolProviderSource::new(
            "grant-source",
            vec![Arc::new(GrantDeferralProvider { may_defer })],
        )))
        .expect("grant source registered");
    registry
        .compose_session_catalog(true, Vec::new())
        .expect("resident catalog with live grant sources")
}
