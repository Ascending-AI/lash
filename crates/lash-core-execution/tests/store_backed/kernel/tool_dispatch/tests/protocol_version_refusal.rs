use super::*;

const SEED: u64 = 0x5_2d26;

#[tokio::test]
async fn empty_batch_dispatches_predecessor_and_unknown_versions_to_a_typed_protocol_refusal() {
    let (double, handler) = crate::support::open_dispatch_handler(SEED).await;
    let context = dispatch_context(crate::support::double_dispatch_ports(&double, &handler)).await;
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
    drop(context);
    handler.close().await.expect("close the dispatch handler");
}
