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

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    struct ControllerOwnedReplay;

    impl crate::AwaitEventResolver for ControllerOwnedReplay {
        fn replay_ownership(&self) -> crate::EffectReplayOwnership {
            crate::EffectReplayOwnership::Controller
        }
    }

    #[async_trait::async_trait]
    impl crate::RuntimeEffectController for ControllerOwnedReplay {
        async fn execute_effect(
            &self,
            _envelope: crate::RuntimeEffectEnvelope,
            _local_executor: crate::RuntimeEffectLocalExecutor<'_>,
        ) -> Result<crate::RuntimeEffectOutcome, crate::RuntimeEffectControllerError> {
            panic!("nested batch guard must reject before effect execution")
        }
    }

    #[tokio::test]
    async fn nested_batch_is_rejected_inside_controller_owned_atomic_tool_attempt() {
        let manager = Arc::new(crate::testing::MockSessionManager::default());
        let sessions: Arc<dyn crate::plugin::SessionStateService> = manager.clone();
        let lifecycle: Arc<dyn crate::plugin::SessionLifecycleService> = manager.clone();
        let graph: Arc<dyn crate::plugin::SessionGraphService> = manager;
        let context = crate::ToolContext::builder(
            "session".to_string(),
            sessions,
            lifecycle,
            graph,
            Arc::new(crate::UnavailableProcessService),
            crate::runtime::RuntimeEffectControllerHandle::shared(Arc::new(ControllerOwnedReplay)),
            Arc::new(crate::SessionAttachmentStore::in_memory()),
            crate::DirectCompletionClient::unavailable("not used by nested batch guard"),
        )
        .parent_invocation(Some(crate::RuntimeInvocation::effect(
            crate::RuntimeScope::new("session"),
            "parent-tool-attempt",
            crate::RuntimeEffectKind::ToolAttempt,
            "parent-tool-attempt",
        )))
        .build();
        let calls = vec![
            ToolInvocation::new(
                "one",
                crate::ToolId::from("tool:one"),
                serde_json::json!({}),
            ),
            ToolInvocation::new(
                "two",
                crate::ToolId::from("tool:two"),
                serde_json::json!({}),
            ),
        ];

        let replies = ToolDispatchClient { context }.batch(calls).await;

        assert_eq!(replies.len(), 2);
        for reply in replies {
            assert_eq!(reply.output.status(), lash_sansio::ToolCallStatus::Failure);
            assert!(reply.output.value_for_projection().to_string().contains(
                "nested tool batch dispatch is unavailable inside an atomic tool attempt"
            ));
            assert!(reply.record.is_none());
        }
    }
}
