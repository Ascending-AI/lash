use crate::plugin::{DirectCompletion, PluginError};

#[derive(Clone)]
pub struct ToolDirectCompletionClient<'run> {
    pub(super) owner: crate::RuntimeOwner,
    pub(super) call_id: lash_sansio::ToolCallId,
    pub(super) direct_completions: crate::DirectCompletionClient<'run>,
    pub(super) parent_invocation: Option<crate::RuntimeInvocation>,
}

impl ToolDirectCompletionClient<'_> {
    /// # Integrator class
    ///
    /// Tool implementors use this capability for provider calls that must
    /// retain the owner and causal attribution supplied by the runtime. A
    /// call inside a process is made for the process and names it as its
    /// cause: a process has no session to attribute the call to.
    pub async fn complete(
        &self,
        mut request: crate::DirectRequest,
        usage_source: &str,
    ) -> Result<DirectCompletion, PluginError> {
        match &self.owner {
            crate::RuntimeOwner::Session(session_id) => {
                if request.owner.is_none() {
                    request.owner = Some(crate::LlmRequestOwner::Session {
                        session_id: session_id.clone(),
                    });
                }
                if request.caused_by.is_none() {
                    request.caused_by = Some(crate::CausalRef::ToolCall {
                        session_id: session_id.clone(),
                        call_id: self.call_id.clone(),
                    });
                }
            }
            crate::RuntimeOwner::Process(process_id) => {
                if request.owner.is_none() {
                    request.owner = Some(crate::LlmRequestOwner::Process {
                        process_id: process_id.clone(),
                    });
                }
                if request.caused_by.is_none() {
                    request.caused_by = Some(crate::CausalRef::Process {
                        process_id: process_id.clone(),
                    });
                }
            }
        }
        self.direct_completions
            .direct_completion_for_tool(request, usage_source, self.parent_invocation.as_ref())
            .await
    }
}
