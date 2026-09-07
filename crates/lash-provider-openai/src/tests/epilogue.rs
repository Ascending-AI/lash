use super::*;
use lash_llm_transport::conformance::epilogue::{
    EpilogueScenario, EpilogueTransport, completion_epilogue_conformance,
};

fn responses_epilogue_wire(scenario: EpilogueScenario) -> &'static str {
    match scenario {
        EpilogueScenario::HttpFailure => r#"{"error":{"message":"upstream failed"}}"#,
        EpilogueScenario::EmptyTruncated => {
            "data: {\"type\":\"response.created\",\"response\":{\"id\":\"resp_test\",\"status\":\"in_progress\",\"output\":[]}}\n\n"
        }
        EpilogueScenario::TextTruncated => {
            "data: {\"type\":\"response.output_text.delta\",\"output_index\":0,\"delta\":\"partial\"}\n\n"
        }
        EpilogueScenario::EmptyOutputLimit => {
            "data: {\"type\":\"response.incomplete\",\"response\":{\"id\":\"resp_test\",\"status\":\"incomplete\",\"incomplete_details\":{\"reason\":\"max_output_tokens\"},\"output\":[]}}\n\n"
        }
    }
}

#[tokio::test]
async fn chat_completion_epilogue_conformance() {
    for streamed in [false, true] {
        completion_epilogue_conformance(|scenario| async move {
            let body = match scenario {
                EpilogueScenario::HttpFailure => r#"{"error":{"message":"upstream failed"}}"#,
                EpilogueScenario::EmptyTruncated => "data: {\"choices\":[{\"delta\":{}}]}\n\n",
                EpilogueScenario::TextTruncated => {
                    "data: {\"choices\":[{\"delta\":{\"content\":\"partial\"}}]}\n\n"
                }
                EpilogueScenario::EmptyOutputLimit => {
                    "data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"length\"}]}\n\n"
                }
            };
            let mut provider = openrouter_provider()
                .with_transport(Arc::new(EpilogueTransport::new(body, streamed, scenario)));
            provider
                .complete(streamed_request(Arc::new(
                    std::sync::Mutex::new(Vec::new()),
                )))
                .await
        })
        .await;
    }
}

#[tokio::test]
async fn responses_completion_epilogue_conformance() {
    for streamed in [false, true] {
        completion_epilogue_conformance(|scenario| async move {
            let mut provider = OpenAiProvider::new("key").with_transport(Arc::new(
                EpilogueTransport::new(responses_epilogue_wire(scenario), streamed, scenario),
            ));
            provider
                .complete(streamed_request(Arc::new(
                    std::sync::Mutex::new(Vec::new()),
                )))
                .await
        })
        .await;
    }
}

#[tokio::test]
async fn codex_completion_epilogue_conformance() {
    completion_epilogue_conformance(|scenario| async move {
        let mut provider = crate::CodexProvider::new("access", "refresh", u64::MAX)
            .force_sse_transport()
            .with_http_transport(Arc::new(EpilogueTransport::new(
                responses_epilogue_wire(scenario),
                true,
                scenario,
            )));
        provider
            .complete(streamed_request(Arc::new(
                std::sync::Mutex::new(Vec::new()),
            )))
            .await
    })
    .await;
}
