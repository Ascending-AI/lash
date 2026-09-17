use crate::{ToolInvocation, ToolInvocationReply, ToolManifest};

use super::ToolContext;

#[derive(Clone)]
pub struct ToolDispatchClient<'run> {
    pub(super) context: ToolContext<'run>,
}

impl<'run> ToolDispatchClient<'run> {
    /// Resolve the callable manifest for a tool name in the current runtime.
    ///
    /// # Integrator class
    ///
    /// Tool implementors inspect this manifest before composing nested tool
    /// dispatch without reaching into the runtime registry.
    pub fn callable_tool_manifest(&self, name: &str) -> Option<ToolManifest> {
        let dispatch = self.context.runtime_dispatch.as_ref()?;
        crate::tool_dispatch::resolve_callable_manifest(dispatch, name)
    }

    /// Dispatch a batch of nested tool invocations through the current runtime.
    ///
    /// # Integrator class
    ///
    /// Tool implementors use this capability to compose tools while retaining
    /// runtime ownership of dispatch, attribution, and reply ordering.
    pub async fn batch(&self, calls: Vec<ToolInvocation>) -> Vec<ToolInvocationReply> {
        let Some(runtime) = self.context.runtime_execution_context.clone() else {
            return calls
                .into_iter()
                .map(|_| {
                    ToolInvocationReply::error(serde_json::json!(
                        "tool batch dispatch is unavailable outside runtime execution"
                    ))
                })
                .collect();
        };
        // Children of a batch dispatch carry the batch call's id so consumers
        // can attribute them to their parent without re-parsing batch args.
        // A nested provider batch is not an aggregate await, so it needs the
        // replies only; settlement order matters where Promise.all selects.
        runtime
            .with_batch_parent_call_id(self.context.tool_call_id.clone())
            .call_tool_batch(calls)
            .await
            .replies
    }
}
