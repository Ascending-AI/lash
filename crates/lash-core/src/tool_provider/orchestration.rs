use super::ToolContext;
use crate::ToolManifest;
use crate::plugin::PluginError;

/// Per-call inputs handed to an orchestrating provider body.
///
/// Unlike a leaf attempt call, this is executed by the enclosing process
/// replay code and is never wrapped in a `ToolAttempt` effect. Journaled
/// operations issued through [`OrchestrationContext`] are therefore ordinary,
/// ordinal-safe children of that process replay.
///
/// This is public for **tool-provider integrators** implementing a model-facing
/// operation whose result depends on durable child work.
pub struct OrchestratingToolCall<'a> {
    pub name: &'a str,
    pub args: &'a serde_json::Value,
    pub context: &'a OrchestrationContext<'a>,
}

/// Sealed process-replay environment for the rare tool body that must await
/// durable work before it can return.
///
/// The body is deterministic workflow code: it must not consult wall clock or
/// randomness, drive commands from unordered iteration, perform unjournaled
/// I/O, or leave a journaled action un-awaited.
///
/// This is public for **tool-provider integrators**; runtime hosts construct it
/// and provider bodies receive it through [`OrchestratingToolCall`].
#[derive(Clone)]
pub struct OrchestrationContext<'run> {
    context: ToolContext<'run>,
}

impl<'run> OrchestrationContext<'run> {
    pub(crate) fn from_tool_context(context: ToolContext<'run>) -> Self {
        Self { context }
    }

    #[cfg(any(test, feature = "testing"))]
    #[doc(hidden)]
    pub fn __for_testing(context: ToolContext<'run>) -> Self {
        Self::from_tool_context(context)
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
