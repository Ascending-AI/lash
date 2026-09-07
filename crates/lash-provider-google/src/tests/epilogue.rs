use super::*;
use lash_llm_transport::conformance::epilogue::{
    EpilogueScenario, EpilogueTransport, completion_epilogue_conformance,
};

#[tokio::test]
async fn google_completion_epilogue_conformance() {
    completion_epilogue_conformance(|scenario| async move {
        let body = match scenario {
        EpilogueScenario::HttpFailure => r#"{"error":{"message":"upstream failed"}}"#,
            EpilogueScenario::EmptyTruncated => "data: {\"response\":{\"candidates\":[{\"content\":{\"parts\":[]}}]}}\n\n",
            EpilogueScenario::TextTruncated => "data: {\"response\":{\"candidates\":[{\"content\":{\"parts\":[{\"text\":\"partial\"}]}}]}}\n\n",
            EpilogueScenario::EmptyOutputLimit => "data: {\"response\":{\"candidates\":[{\"content\":{\"parts\":[]},\"finishReason\":\"MAX_TOKENS\"}]}}\n\n",
        };
        GoogleOAuthProvider::new("access", "refresh", 0, crate::GoogleOAuthClient { id: "id".into(), secret: "secret".into() })
            .with_transport(Arc::new(EpilogueTransport::new(body, true, scenario)))
            .execute_request("access", json!({"model":"gemini-test"}), Some(LlmEventSender::new(|_| {})), None, StreamTermination::RequireTerminalEvidence, None).await
    }).await;
}
