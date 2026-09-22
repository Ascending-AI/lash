use super::*;

#[tokio::test]
async fn empty_batch_dispatches_predecessor_and_unknown_versions_to_a_typed_protocol_refusal() {
    let context = dispatch_context();
    for recorded in [0, 1, 2, 4] {
        let outcomes = execute_final_tool_intents(
            &context,
            Some("empty-version-call"),
            &crate::ToolIntents {
                protocol_version: recorded,
                intents: Vec::new(),
            },
            None,
        )
        .await
        .expect("empty unsupported batch is refused");
        assert_eq!(
            outcomes,
            vec![crate::ToolIntentExecutionOutcome::ProtocolRefused {
                refusal: crate::ToolIntentRefusalReason::UnsupportedProtocolVersion { recorded },
            }]
        );
    }
}
