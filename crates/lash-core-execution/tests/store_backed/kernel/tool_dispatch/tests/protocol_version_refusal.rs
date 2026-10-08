use super::*;
use crate::tool_dispatch::execute_final_tool_intents;

/// An empty intent batch recorded under a predecessor or unknown protocol
/// version is refused typed, before any intent is realized.
#[tokio::test]
async fn empty_batch_dispatches_predecessor_and_unknown_versions_to_a_typed_protocol_refusal() {
    let provider: Arc<dyn ToolProvider> = Arc::new(ExactDispatchTools {
        contracts_resolved: Arc::new(AtomicUsize::new(0)),
        executed: Arc::new(AtomicUsize::new(0)),
        contract_available: true,
        observed_execution_bindings: None,
    });
    let context = refusing_dispatch_context(provider_plugins(
        provider,
        crate::plugin::SessionAuthorityContext::ambient_fixture(),
    ))
    .await;
    for recorded in [0, 1, 2, 4] {
        let outcomes = execute_final_tool_intents(
            &context.intent_realization_context(),
            &crate::ToolCallId::fixture("empty-version-call"),
            &crate::ToolIntents {
                protocol_version: recorded,
                intents: Vec::new(),
            },
            None,
        )
        .await
        .expect("empty unsupported batch is refused")
        .receipt
        .outcomes;
        assert_eq!(
            outcomes,
            vec![crate::ToolIntentExecutionOutcome::ProtocolRefused {
                refusal: crate::ToolIntentRefusalReason::UnsupportedProtocolVersion { recorded },
            }]
        );
    }
}
