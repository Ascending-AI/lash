use super::*;
use crate::support::LlmStreamEvent;

/// FIG-6547: a WebSocket body capture is retained while the send is still
/// pending, so abandonment after a cost-bearing frame retains the receipt.
#[tokio::test]
async fn websocket_attempt_retains_declared_body_receipts_before_its_terminal() {
    let ws = spawn_scripted_websocket(vec![ScriptedWsAction::RecordedFrames {
        frames: vec![json!({"type":"response.created","response":{"id":"resp-partial","model":"gpt-5","usage":{"cost":0.04}}}).to_string()],
        close_after_frames: false,
    }]).await;
    let mut provider = websocket_test_provider(
        CodexTransport::Websocket,
        "http://127.0.0.1:9/unused".into(),
        ws.url.clone(),
    );
    let received = Arc::new(Notify::new());
    let mut req = request(vec![LlmMessage::text(LlmRole::User, "hello")]);
    req.model
        .metadata_mut()
        .request_defaults
        .response_metadata_body_paths = vec!["/response/usage/cost".into()];
    req.stream_events = Some(lash_core::llm::types::LlmEventSender::new({
        let received = received.clone();
        move |event| {
            if let LlmStreamEvent::Evidence(evidence) = event
                && evidence
                    .execution_evidence
                    .as_ref()
                    .and_then(|evidence| evidence.provider_response_id.as_deref())
                    == Some("resp-partial")
            {
                received.notify_one();
            }
        }
    }));
    let template = provider
        .lower(&req, &lash_core::llm::types::SegmentCache::disabled())
        .await
        .expect("lowers");
    let body =
        lash_core::llm::types::LiveRequestBody::fill(Arc::new(template), vec![]).expect("no slots");
    let context = lash_core::llm::types::ResponseContext::of_request(&req);
    let control = context.attempt_control.clone();
    let send = provider.send(&body, context);
    tokio::pin!(send);
    tokio::time::timeout(Duration::from_secs(5), async {
        tokio::select! {
            result = &mut send => panic!("send ended before its terminal: {result:?}"),
            _ = received.notified() => {},
        }
    })
    .await
    .expect("the cost-bearing frame was read");
    let partial = control.partial_response().expect("retained partial");
    assert_eq!(
        partial.response_metadata["body:/response/usage/cost"],
        json!(0.04)
    );
}

#[derive(Debug)]
struct FailedHeadersTransport(Arc<Notify>);

#[async_trait::async_trait]
impl lash_llm_transport::LlmHttpTransport for FailedHeadersTransport {
    async fn send(
        &self,
        _: lash_llm_transport::LlmHttpRequest,
        _: Option<Duration>,
    ) -> Result<lash_llm_transport::LlmHttpResponse, LlmTransportError> {
        Ok(lash_llm_transport::LlmHttpResponse {
            status: 400,
            headers: vec![("x-cost".into(), "0.03".into())],
            body: lash_llm_transport::LlmHttpBody::streamed(PendingErrorBody(self.0.clone())),
        })
    }
}

#[derive(Debug)]
struct PendingErrorBody(Arc<Notify>);

#[async_trait::async_trait]
impl lash_llm_transport::LlmByteStream for PendingErrorBody {
    async fn next_chunk(&mut self) -> Result<Option<bytes::Bytes>, LlmTransportError> {
        self.0.notify_one();
        std::future::pending().await
    }
}

/// FIG-6547: an error body still being read must not hide received cost headers.
#[tokio::test]
async fn http_attempt_retains_declared_headers_before_reading_a_failed_body() {
    for kind in ["chat", "responses", "oauth"] {
        let polled = Arc::new(Notify::new());
        let transport = Arc::new(FailedHeadersTransport(polled.clone()));
        let mut provider: Box<dyn Provider> = match kind {
            "chat" => Box::new(
                crate::OpenAiCompatibleProvider::new("key", "https://proxy.example/v1")
                    .with_transport(transport),
            ),
            "responses" => Box::new(crate::OpenAiProvider::new("key").with_transport(transport)),
            _ => Box::new(
                CodexProvider::new(Arc::new(lash_core::provider::ProviderToken::new("access")))
                    .force_sse_transport()
                    .with_http_transport(transport),
            ),
        };
        let mut req = request(vec![LlmMessage::text(LlmRole::User, "hello")]);
        req.model
            .metadata_mut()
            .request_defaults
            .response_metadata_headers = vec!["X-Cost".into()];
        let template = provider
            .lower(&req, &lash_core::llm::types::SegmentCache::disabled())
            .await
            .expect("lowers");
        let body = lash_core::llm::types::LiveRequestBody::fill(Arc::new(template), vec![])
            .expect("no slots");
        let context = lash_core::llm::types::ResponseContext::of_request(&req);
        let control = context.attempt_control.clone();
        let send = provider.send(&body, context);
        tokio::pin!(send);
        tokio::time::timeout(Duration::from_secs(5), async {
            tokio::select! {
                result = &mut send => panic!("send ended before its body arrived: {result:?}"),
                _ = polled.notified() => {},
            }
        })
        .await
        .expect("the error body is being read");
        let partial = control
            .partial_response()
            .expect("received headers were retained");
        assert_eq!(
            partial.response_metadata["header:x-cost"],
            json!("0.03"),
            "{kind}"
        );
        assert_eq!(
            partial.usage,
            lash_core::llm::types::LlmUsage::unknown_call()
        );
    }
}
