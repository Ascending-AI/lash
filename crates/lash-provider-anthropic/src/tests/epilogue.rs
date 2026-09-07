use super::*;
use lash_llm_transport::conformance::epilogue::{
    EpilogueScenario, EpilogueTransport, completion_epilogue_conformance,
};

#[tokio::test]
async fn anthropic_completion_epilogue_conformance() {
    completion_epilogue_conformance(|scenario| async move {
        let body = match scenario {
        EpilogueScenario::HttpFailure => r#"{"error":{"message":"upstream failed"}}"#,
            EpilogueScenario::EmptyTruncated => "data: {\"type\":\"message_start\",\"message\":{\"usage\":{\"input_tokens\":1}}}\n\n",
            EpilogueScenario::TextTruncated => concat!(
                "data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\n",
                "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"partial\"}}\n\n",
            ),
            EpilogueScenario::EmptyOutputLimit => concat!(
                "data: {\"type\":\"message_start\",\"message\":{\"usage\":{\"input_tokens\":1}}}\n\n",
                "data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"max_tokens\"},\"usage\":{\"output_tokens\":1}}\n\n",
                "data: {\"type\":\"message_stop\"}\n\n",
            ),
        };
        let mut req = request(vec![LlmMessage::text(LlmRole::User, "hello")]);
        req.model_capability.stream_termination = Some(StreamTermination::RequireTerminalEvidence);
        req.stream_events = Some(LlmEventSender::new(|_| {}));
        AnthropicProvider::new("key").with_transport(Arc::new(EpilogueTransport::new(body, true, scenario))).complete(req).await
    }).await;
}
