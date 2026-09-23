//! The process-lifetime completion-key refusal: a controller that journals
//! nothing, or a journaled one that cannot prepare keys, refuses a completion
//! key unless it opted into process-lifetime keys. The opt-in and the refusal
//! exist only for the native effect host (ADR 0102: every host journals), so
//! these tests go with them; FIG-3585 deletes the route and this file.

use std::sync::Arc;

use super::*;

struct DurableControllerWithoutCompletionKeySupport;

impl crate::AwaitEventResolver for DurableControllerWithoutCompletionKeySupport {}
#[async_trait::async_trait]
impl crate::RuntimeEffectController for DurableControllerWithoutCompletionKeySupport {
    fn effect_journaling(&self) -> crate::EffectJournaling {
        crate::EffectJournaling::Journaled
    }

    async fn execute_effect(
        &self,
        _envelope: crate::RuntimeEffectEnvelope,
        _local_executor: crate::RuntimeEffectLocalExecutor<'_>,
    ) -> Result<crate::RuntimeEffectOutcome, crate::RuntimeEffectControllerError> {
        unreachable!("completion-key capability is rejected before effect execution")
    }

    async fn open_effect_group(
        &self,
        _group: crate::RuntimeEffectGroup,
    ) -> Result<crate::EffectGroupHandle, crate::RuntimeEffectControllerError> {
        Err(crate::effect_groups_unsupported(
            "DurableControllerWithoutCompletionKeySupport",
        ))
    }

    async fn await_next_settlement(
        &self,
        _handle: &mut crate::EffectGroupHandle,
        _cancel: crate::CancellationToken,
    ) -> Result<crate::GroupSettlement, crate::RuntimeEffectControllerError> {
        Err(crate::effect_groups_unsupported(
            "DurableControllerWithoutCompletionKeySupport",
        ))
    }

    async fn close_effect_group(
        &self,
        _handle: crate::EffectGroupHandle,
        _disposition: crate::LoserPolicy,
    ) -> Result<(), crate::RuntimeEffectControllerError> {
        Err(crate::effect_groups_unsupported(
            "DurableControllerWithoutCompletionKeySupport",
        ))
    }
}

#[tokio::test]
async fn native_completion_key_requires_process_lifetime_opt_in() {
    let prepared = PreparedToolCall::from_parts(
        "call-native-risk",
        "tool:demo_tool",
        "demo_tool",
        serde_json::json!({}),
        None,
        serde_json::json!({}),
    );
    let context = ToolContext::builder(
        SessionId::from("session-native-risk"),
        Arc::new(crate::testing::MockSessionManager::default()),
        Arc::new(crate::testing::MockSessionManager::default()),
        Arc::new(crate::testing::MockSessionManager::default()),
        Arc::new(crate::UnavailableProcessService),
        crate::runtime::RuntimeEffectControllerHandle::shared(Arc::new(
            crate::testing::UnavailableEffectController,
        )),
        Arc::new(crate::SessionAttachmentStore::unavailable()),
        crate::DirectCompletionClient::unavailable(
            "direct completions are unavailable in this test context",
        ),
    )
    .prepared_call(&prepared)
    .build();

    let error = context
        .completion_key()
        .await
        .expect_err("Native completion keys must refuse by default");
    assert_eq!(error.code.as_str(), "tool_completion_key_process_lifetime");
    assert!(error.message.contains("process-loss-safe"));
    assert!(
        error
            .message
            .contains("allow_process_lifetime_completion_keys")
    );
}

#[tokio::test]
async fn durable_controller_does_not_bypass_completion_key_preparation() {
    let prepared = PreparedToolCall::from_parts(
        "call-controller-risk",
        "tool:demo_tool",
        "demo_tool",
        serde_json::json!({}),
        None,
        serde_json::json!({}),
    );
    let context = ToolContext::builder(
        SessionId::from("session-controller-risk"),
        Arc::new(crate::testing::MockSessionManager::default()),
        Arc::new(crate::testing::MockSessionManager::default()),
        Arc::new(crate::testing::MockSessionManager::default()),
        Arc::new(crate::UnavailableProcessService),
        crate::runtime::RuntimeEffectControllerHandle::shared(Arc::new(
            DurableControllerWithoutCompletionKeySupport,
        )),
        Arc::new(crate::SessionAttachmentStore::unavailable()),
        crate::DirectCompletionClient::unavailable(
            "direct completions are unavailable in this test context",
        ),
    )
    .prepared_call(&prepared)
    .build();

    let error = context
        .completion_key()
        .await
        .expect_err("controller ownership alone must not permit completion keys");
    assert_eq!(error.code.as_str(), "tool_completion_key_process_lifetime");
}
