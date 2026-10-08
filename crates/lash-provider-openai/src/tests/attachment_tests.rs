use super::*;
use lash_core::llm::types::{AttachmentSlot, SlotCodec};
use lash_sansio::llm::attachment_delivery::{
    AttachmentPosition, Delivery, DeliverySecret, ProviderFileScope,
};

// DELIVERY-SCOPE: Chat has no Files namespace, even when the host permits one.
#[test]
fn chat_never_accepts_provider_files() {
    let provider = OpenAiCompatibleProvider::new("key", "https://example.invalid/v1");
    let mime = lash_sansio::MediaType::parse("image/png").unwrap();
    let scope = ProviderFileScope {
        provider: provider.kind().into(),
        endpoint: provider.route_identity("").endpoint.into(),
        credential_scope: "account".into(),
    };
    for position in [AttachmentPosition::Message, AttachmentPosition::ToolResult] {
        let accepts = provider.attachment_accepts("model", &mime, position);
        assert!(accepts.provider_file.is_none());
        if position == AttachmentPosition::ToolResult {
            assert!(accepts.is_empty());
        }
        let reference = lash_sansio::AttachmentRef {
            id: lash_sansio::AttachmentId::parse("a".repeat(64)).unwrap(),
            media_type: mime.clone(),
            byte_len: 1,
            label: None,
            type_metadata: None,
        };
        // A forged pinned acceptance cannot make the Chat codec encode a file id.
        let slot = AttachmentSlot {
            reference,
            position,
            accepts: lash_sansio::llm::attachment_delivery::ProviderAccepts {
                bytes: true,
                url: true,
                provider_file: Some(scope.clone()),
            },
            codec: SlotCodec {
                name: crate::attachment_delivery::CHAT_CODEC.into(),
                revision: 1,
            },
        };
        let delivered = Delivery::ProviderFile {
            scope: scope.clone(),
            id: DeliverySecret::new("file-secret".into()),
            valid_until_ms: None,
        };
        let error = provider.encode_slot(&slot, &delivered).unwrap_err();
        assert_eq!(error.retry_verdict, TransportRetryVerdict::Forbidden);
        assert_eq!(
            error.code.as_ref().unwrap().spelling(),
            "admitted_request_unavailable"
        );
        assert!(!format!("{error:?}").contains("file-secret"));
    }
}

// ADR 0135 §4: delivered slots preserve ordinary streaming and provider text.
#[tokio::test]
async fn url_attachment_calls_stream_deltas_and_provider_traces() {
    use lash_core::llm::types::{
        LiveRequestBody, LlmEventSender, LlmProviderTraceSender, ResponseContext,
    };
    use lash_core::provider::ProviderToken;
    use std::sync::Mutex;
    const URL: &str = "https://files.example.invalid/image?signature=live";
    #[derive(Debug)]
    struct StreamingTransport;
    #[async_trait]
    impl LlmHttpTransport for StreamingTransport {
        async fn send(
            &self,
            request: LlmHttpRequest,
            _: Option<std::time::Duration>,
        ) -> Result<lash_llm_transport::LlmHttpResponse, LlmTransportError> {
            let wire: Value = serde_json::from_slice(&request.body).unwrap();
            assert!(wire.to_string().contains(URL));
            assert!(!request.body_for_error.as_ref().unwrap().contains(URL));
            let body = if wire.get("messages").is_some() {
                format!(
                    "data: {}\n\ndata: {}\n\ndata: [DONE]\n\n",
                    json!({"choices":[{"index":0,"delta":{"content":URL,"reasoning_content":"thinking"},"finish_reason":null}]}),
                    json!({"choices":[{"index":0,"delta":{},"finish_reason":"stop"}]})
                )
            } else {
                let item = json!({"id":"msg1","type":"message","role":"assistant","status":"completed","content":[{"type":"output_text","text":URL}]});
                [
                    json!({"type":"response.output_item.added","output_index":0,"item":{"id":"msg1","type":"message","role":"assistant","content":[]}}),
                    json!({"type":"response.output_text.delta","item_id":"msg1","output_index":0,"content_index":0,"delta":URL}),
                    json!({"type":"response.output_text.done","item_id":"msg1","output_index":0,"content_index":0,"text":URL}),
                    json!({"type":"response.output_item.done","output_index":0,"item":item}),
                    json!({"type":"response.completed","response":{"id":"resp1","status":"completed","output":[item],"usage":{"input_tokens":1,"output_tokens":1,"total_tokens":2}}}),
                ].iter().map(|e| format!("data: {e}\n\n")).collect()
            };
            Ok(lash_llm_transport::LlmHttpResponse {
                status: 200,
                headers: vec![("content-type".into(), "text/event-stream".into())],
                body: LlmHttpBody::buffered(body),
            })
        }
    }
    for adapter in 0..3 {
        let transport = Arc::new(StreamingTransport);
        let mut provider: Box<dyn Provider> = match adapter {
            0 => Box::new(
                OpenAiCompatibleProvider::new("key", "https://example.invalid/v1")
                    .with_transport(transport),
            ),
            1 => Box::new(crate::OpenAiProvider::new("key").with_transport(transport)),
            _ => Box::new(
                crate::CodexProvider::new(Arc::new(ProviderToken::new("key")))
                    .force_sse_transport()
                    .with_http_transport(transport),
            ),
        };
        let mut req = request(vec![]);
        req.model.metadata_mut().request_defaults.expose_thinking = true;
        req.messages = vec![LlmMessage::new(
            LlmRole::User,
            vec![LlmContentBlock::Attachment {
                reference: Box::new(lash_sansio::AttachmentRef {
                    id: lash_sansio::AttachmentId::parse("a".repeat(64)).unwrap(),
                    media_type: lash_sansio::MediaType::parse("image/png").unwrap(),
                    byte_len: 1,
                    label: None,
                    type_metadata: None,
                }),
            }],
        )];
        let events = Arc::new(Mutex::new(Vec::new()));
        let sink = events.clone();
        req.stream_events = Some(LlmEventSender::new(move |event| {
            sink.lock().unwrap().push(event)
        }));
        let traces = Arc::new(Mutex::new(Vec::new()));
        let sink = traces.clone();
        req.provider_trace = Some(LlmProviderTraceSender::new(move |event| {
            sink.lock().unwrap().push(event)
        }));
        let template = Arc::new(provider.lower(&req).await.unwrap());
        let delivery = Delivery::Url {
            url: DeliverySecret::new(URL.into()),
            valid_until_ms: None,
        };
        let encoded = provider
            .encode_slot(template.slots().next().unwrap(), &delivery)
            .unwrap();
        let live = LiveRequestBody::fill(template, vec![encoded]).unwrap();
        let response = provider
            .send(&live, ResponseContext::of_request(&req))
            .await
            .unwrap();
        let events = events.lock().unwrap();
        assert!(
            events
                .iter()
                .any(|e| matches!(e, LlmStreamEvent::Delta { text, .. } if text == URL)),
            "adapter {adapter} lost its delta: {events:?}"
        );
        if adapter == 0 {
            assert!(events.iter().any(
                |e| matches!(e, LlmStreamEvent::ReasoningDelta { text, .. } if text == "thinking")
            ));
        }
        assert!(
            response
                .parts
                .iter()
                .any(|p| matches!(p, LlmOutputPart::Text { text, .. } if text == URL))
        );
        let traces = traces.lock().unwrap();
        assert!(
            traces
                .iter()
                .any(|e| e.request_endpoint().is_none() && e.raw.contains(URL)),
            "adapter {adapter} lost its response traces: {traces:?}"
        );
    }
}

/// A host's immutable URL delivery for the resume and continuation laws.
pub(crate) struct UrlDelivery;
#[async_trait]
impl lash_core::provider::SlotDeliveries for UrlDelivery {
    async fn deliver(
        &self,
        slots: &[&AttachmentSlot],
        _: &lash_sansio::llm::attachment_delivery::DeliveryContext,
    ) -> Result<Vec<Arc<Delivery>>, lash_core::provider::AttachmentDeliveryError> {
        Ok(slots
            .iter()
            .map(|_| {
                Arc::new(Delivery::Url {
                    url: DeliverySecret::new(
                        "https://immutable.example.test/image?signature=live".into(),
                    ),
                    valid_until_ms: None,
                })
            })
            .collect())
    }
    async fn invalidate(
        &self,
        _: &lash_sansio::AttachmentRef,
        _: &Delivery,
    ) -> Result<(), lash_core::provider::AttachmentDeliveryError> {
        Ok(())
    }
}

pub(crate) fn url_attachment() -> LlmContentBlock {
    LlmContentBlock::Attachment {
        reference: Box::new(lash_sansio::AttachmentRef {
            id: lash_sansio::AttachmentId::parse("a".repeat(64)).unwrap(),
            media_type: lash_sansio::MediaType::parse("image/png").unwrap(),
            byte_len: 1,
            label: None,
            type_metadata: None,
        }),
    }
}
