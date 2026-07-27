use crate::{ToolInvocation, ToolInvocationReply, ToolManifest};

use super::ToolContext;

#[derive(Clone)]
pub struct ToolDispatchClient<'run> {
    pub(super) context: ToolContext<'run>,
}

impl<'run> ToolDispatchClient<'run> {
    pub fn callable_tool_manifest(&self, name: &str) -> Option<ToolManifest> {
        let dispatch = self.context.runtime_dispatch.as_ref()?;
        crate::tool_dispatch::resolve_callable_manifest(dispatch, name)
    }

    pub async fn batch(&self, calls: Vec<ToolInvocation>) -> Vec<ToolInvocationReply> {
        if self
            .context
            .parent_invocation
            .as_ref()
            .is_some_and(|invocation| {
                invocation.effect_kind() == Some(crate::RuntimeEffectKind::ToolAttempt)
            })
            && self
                .context
                .effect_controller
                .controller()
                .replay_ownership()
                == crate::EffectReplayOwnership::Controller
        {
            return calls
                .into_iter()
                .map(|_| {
                    ToolInvocationReply::error(serde_json::json!(
                        "nested tool batch dispatch is unavailable inside an atomic tool attempt; decompose the work into process steps"
                    ))
                })
                .collect();
        }
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
        runtime
            .with_batch_parent_call_id(self.context.tool_call_id.clone())
            .call_tool_batch(calls)
            .await
    }
}
