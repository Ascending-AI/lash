use crate::plugin::{DirectCompletion, PluginError};

#[derive(Clone)]
pub struct ToolDirectCompletionClient<'run> {
    pub(super) session_id: String,
    pub(super) tool_call_id: Option<String>,
    pub(super) direct_completions: crate::DirectCompletionClient<'run>,
    pub(super) parent_invocation: Option<crate::RuntimeInvocation>,
}

impl ToolDirectCompletionClient<'_> {
    /// Run a direct completion attributed to the current tool invocation.
    ///
    /// # Integrator class
    ///
    /// Tool implementors use this capability for provider calls that must
    /// retain the session and causal attribution supplied by the runtime.
    pub async fn complete(
        &self,
        mut request: crate::DirectRequest,
        usage_source: &str,
    ) -> Result<DirectCompletion, PluginError> {
        if request.session_id.is_none() {
            request.session_id = Some(self.session_id.clone());
        }
        if request.caused_by.is_none()
            && let Some(call_id) = self.tool_call_id.clone()
        {
            request.caused_by = Some(crate::CausalRef::ToolCall {
                session_id: self.session_id.clone(),
                call_id,
            });
        }
        self.direct_completions
            .direct_completion_for_tool(request, usage_source, self.parent_invocation.as_ref())
            .await
    }
}
