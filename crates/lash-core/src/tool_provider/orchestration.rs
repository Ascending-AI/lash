use super::{ToolContext, ToolPrepareCall, ToolResult};
use crate::plugin::PluginError;
use crate::{PreparedToolCall, ToolContract, ToolManifest};
use std::sync::Arc;

/// Sealed process-replay environment for the rare tool body that must await
/// durable work before it can return.
///
/// The body is deterministic workflow code: it must not consult wall clock or
/// randomness, drive commands from unordered iteration, perform unjournaled
/// I/O, or leave a journaled action un-awaited.
///
/// This is a doc-hidden first-party facade-support seam. Runtime dispatch
/// constructs it only for a typed orchestrating registration and passes it
/// directly to that registration's implementation.
#[derive(Clone)]
#[doc(hidden)]
pub struct OrchestrationContext<'run> {
    context: ToolContext<'run>,
}

impl<'run> OrchestrationContext<'run> {
    pub(crate) fn new(context: ToolContext<'run>) -> Self {
        Self { context }
    }

    pub fn session_id(&self) -> &str {
        self.context.session_id()
    }

    pub fn tool_call_id(&self) -> Option<&str> {
        self.context.tool_call_id()
    }

    pub fn prepared_payload(&self) -> &serde_json::Value {
        self.context.prepared_payload()
    }

    pub fn decode_prepared_payload<T>(&self) -> Result<T, serde_json::Error>
    where
        T: serde::de::DeserializeOwned,
    {
        self.context.decode_prepared_payload()
    }

    pub fn callable_tool_manifest(&self, name: &str) -> Option<ToolManifest> {
        let dispatch = self.context.runtime_dispatch.as_ref()?;
        crate::tool_dispatch::resolve_callable_manifest(dispatch, name)
    }

    pub async fn start_process(
        &self,
        request: crate::ProcessStartRequest,
    ) -> Result<crate::ProcessHandleSummary, PluginError> {
        self.context.processes().start(request).await
    }

    pub async fn await_process(
        &self,
        process_id: &str,
    ) -> Result<crate::ProcessAwaitOutput, PluginError> {
        self.context.processes().await_process(process_id).await
    }

    pub async fn cancel_process(
        &self,
        process_id: &str,
    ) -> Result<crate::ProcessCancelSummary, PluginError> {
        self.context.processes().cancel(process_id).await
    }

    /// Emit one journaled process signal using an author-supplied stable id.
    /// Requiring the id keeps replay identity independent of randomness.
    pub async fn signal_process(
        &self,
        process_id: &str,
        signal_name: &str,
        signal_id: impl Into<String>,
        payload: serde_json::Value,
    ) -> Result<crate::ProcessEvent, PluginError> {
        self.context
            .processes()
            .signal_with_id(process_id, signal_name, signal_id.into(), payload)
            .await
    }

    pub fn emit_child_process_started(
        &self,
        process_id: impl Into<String>,
        child_entry_name: Option<String>,
    ) {
        self.context
            .emit_child_process_started(process_id, child_entry_name);
    }

    pub async fn call_tool_batch(
        &self,
        calls: Vec<crate::ToolInvocation>,
    ) -> Vec<crate::ToolInvocationReply> {
        let Some(runtime) = self.context.runtime_execution_context.clone() else {
            return calls
                .into_iter()
                .map(|_| {
                    crate::ToolInvocationReply::error(serde_json::json!(
                        "tool batch orchestration is unavailable outside process replay"
                    ))
                })
                .collect();
        };
        runtime
            .with_batch_parent_call_id(self.context.tool_call_id.clone())
            .call_tool_batch(calls)
            .await
    }
}

/// Implementation contract carried by an [`OrchestratingToolDef`].
///
/// First-party crates keep their concrete implementation types private and
/// expose only the completed definition. Hosts can enable such a definition,
/// but leaf providers cannot be upgraded into this lane.
#[async_trait::async_trait]
#[doc(hidden)]
pub trait OrchestratingToolImplementation: Send + Sync + 'static {
    fn manifest(&self) -> ToolManifest;

    fn contract(&self) -> Arc<ToolContract>;

    async fn prepare_tool_call(
        &self,
        call: ToolPrepareCall<'_>,
    ) -> Result<PreparedToolCall, ToolResult> {
        Ok(PreparedToolCall::identity(call.tool_id, call.pending))
    }

    async fn execute(
        &self,
        args: &serde_json::Value,
        context: &OrchestrationContext<'_>,
    ) -> ToolResult;
}

/// Opaque definition for a first-party orchestrating registration.
///
/// The registry accepts this completed definition as a distinct registration
/// kind. It never recognizes or upgrades a leaf provider by id or source name.
#[derive(Clone)]
#[doc(hidden)]
pub struct OrchestratingToolDef {
    implementation: Arc<dyn OrchestratingToolImplementation>,
}

impl OrchestratingToolDef {
    /// Package an implementation supplied by an owning first-party crate.
    ///
    /// # Safety
    ///
    /// The caller must be the crate that owns the registered tool contract.
    /// This is an unsafe capability boundary so ordinary downstream Rust code
    /// cannot mint an orchestrating registration from a leaf provider. This is
    /// a provenance convention, not a memory-safety invariant: violating it is
    /// an unsupported capability escalation, but does not by itself cause
    /// undefined behavior.
    #[doc(hidden)]
    pub unsafe fn from_first_party(
        implementation: Arc<dyn OrchestratingToolImplementation>,
    ) -> Self {
        Self { implementation }
    }

    #[cfg(test)]
    pub(crate) fn new(implementation: Arc<dyn OrchestratingToolImplementation>) -> Self {
        Self { implementation }
    }

    pub(crate) fn manifest(&self) -> ToolManifest {
        self.implementation.manifest()
    }

    pub(crate) fn contract(&self) -> Arc<ToolContract> {
        self.implementation.contract()
    }

    pub(crate) async fn prepare_tool_call(
        &self,
        call: ToolPrepareCall<'_>,
    ) -> Result<PreparedToolCall, ToolResult> {
        self.implementation.prepare_tool_call(call).await
    }

    pub(crate) async fn execute(
        &self,
        args: &serde_json::Value,
        context: &OrchestrationContext<'_>,
    ) -> ToolResult {
        self.implementation.execute(args, context).await
    }
}
