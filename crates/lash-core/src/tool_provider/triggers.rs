use crate::{PluginError, TriggerEmitReport, TriggerOccurrenceRequest};

use super::ToolContext;

#[derive(Clone)]
pub struct ToolTriggerClient<'run> {
    pub(super) context: ToolContext<'run>,
}

impl<'run> ToolTriggerClient<'run> {
    pub async fn emit(
        &self,
        request: TriggerOccurrenceRequest,
    ) -> Result<TriggerEmitReport, PluginError> {
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
                .journal_addressing()
                == crate::EffectJournalAddressing::OrdinalAddressed
        {
            return Err(PluginError::Session(
                "ToolContext::triggers().emit() is unavailable inside an atomic tool attempt on ordinal-addressed journal tiers; return a typed tool intent for supported follow-on work or emit the trigger from a process step"
                    .to_string(),
            ));
        }
        let Some(dispatch) = self.context.runtime_dispatch.clone() else {
            return Err(PluginError::Session(
                "trigger emission is unavailable outside runtime tool execution".to_string(),
            ));
        };
        let router = dispatch.trigger_router.as_ref().ok_or_else(|| {
            PluginError::Session("trigger store is unavailable in this runtime".to_string())
        })?;
        let outcome = request.clone();
        let report = router
            .emit(request, dispatch.effect_controller.controller())
            .await?;
        dispatch
            .trigger_outcomes
            .enqueue(crate::tool_dispatch::ToolTriggerEffectOutcome {
                source_type: outcome.source_type,
                source_key: outcome.source_key,
                occurrence_id: report.occurrence_id.clone(),
                payload: outcome.payload,
                idempotency_key: outcome.idempotency_key,
                source: outcome.source,
                deliveries: report.deliveries.clone(),
            });
        Ok(report)
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

        fn journal_addressing(&self) -> crate::EffectJournalAddressing {
            crate::EffectJournalAddressing::OrdinalAddressed
        }
    }

    #[async_trait::async_trait]
    impl crate::RuntimeEffectController for ControllerOwnedReplay {
        async fn execute_effect(
            &self,
            _envelope: crate::RuntimeEffectEnvelope,
            _local_executor: crate::RuntimeEffectLocalExecutor<'_>,
        ) -> Result<crate::RuntimeEffectOutcome, crate::RuntimeEffectControllerError> {
            panic!("nested trigger guard must reject before effect execution")
        }
    }

    #[tokio::test]
    async fn trigger_emit_is_rejected_inside_controller_owned_atomic_tool_attempt() {
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
            crate::DirectCompletionClient::unavailable("not used by nested trigger guard"),
        )
        .parent_invocation(Some(crate::RuntimeInvocation::effect(
            crate::RuntimeScope::new("session"),
            "parent-tool-attempt",
            crate::RuntimeEffectKind::ToolAttempt,
            "parent-tool-attempt",
        )))
        .build();

        let error = ToolTriggerClient { context }
            .emit(crate::TriggerOccurrenceRequest::new(
                "test.trigger",
                "source",
                serde_json::Value::Null,
                "occurrence",
            ))
            .await
            .expect_err("nested trigger emission must be rejected");

        assert_eq!(
            error.to_string(),
            "plugin session error: ToolContext::triggers().emit() is unavailable inside an atomic tool attempt on ordinal-addressed journal tiers; return a typed tool intent for supported follow-on work or emit the trigger from a process step"
        );
    }
}
