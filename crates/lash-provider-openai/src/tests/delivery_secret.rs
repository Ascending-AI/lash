use async_trait::async_trait;
use lash_core::facade_support::LlmTransportError;
use lash_core::llm::types::{
    LiveRequestBody, LlmContentBlock, LlmEventSender, LlmMessage, LlmOutputPart,
    LlmProviderTraceSender, LlmResponse, LlmRole,
};
use lash_core::provider::{Provider, ProviderToken};
use lash_llm_transport::{LlmHttpBody, LlmHttpRequest, LlmHttpResponse, LlmHttpTransport};
use lash_sansio::llm::attachment_delivery::{Delivery, DeliverySecret};
use lash_sansio::{AttachmentId, AttachmentRef, MediaType};
use serde_json::json;
use std::sync::{Arc, Mutex};
const SECRET: &str = "https://files.example.invalid/delivered-secret-7a5e?signature=private";
#[derive(Debug)]
struct EchoTransport {
    mode: u8,
    seen: Mutex<usize>,
}
#[async_trait]
impl LlmHttpTransport for EchoTransport {
    async fn send(
        &self,
        request: LlmHttpRequest,
        _: Option<std::time::Duration>,
    ) -> Result<LlmHttpResponse, LlmTransportError> {
        let wire = String::from_utf8(request.body.to_vec()).unwrap();
        assert!(wire.contains(SECRET), "delivery must reach the live wire");
        assert!(!format!("{request:?}").contains(SECRET));
        let evidence = request.body_for_error.as_ref().expect("safe HTTP evidence");
        assert!(!evidence.contains(SECRET));
        assert!(evidence.contains("$lash_attachment"));
        assert_eq!(
            request.delivery_redactor.as_ref().unwrap().scrub(SECRET),
            "[redacted attachment delivery]"
        );
        *self.seen.lock().unwrap() += 1;
        if self.mode == 2 {
            return Err(LlmTransportError::new(SECRET)
                .with_http_status(503)
                .with_raw(SECRET)
                .with_partial_response(LlmResponse {
                    parts: vec![LlmOutputPart::Text {
                        text: SECRET.into(),
                        response_meta: None,
                    }],
                    terminal_diagnostic: Some(SECRET.into()),
                    provider_usage: Some(json!({"echo": SECRET})),
                    ..Default::default()
                }));
        }
        let body = if self.mode == 3 {
            [json!({"type":"response.created","sequence_number":1,"response":{"id":SECRET,"status":"in_progress","output":[]}}),
             json!({"type":"response.output_item.added","sequence_number":2,"output_index":0,"item":{"id":SECRET,"type":"message","role":"assistant","content":[]}}),
             json!({"type":"response.output_text.delta","sequence_number":3,"output_index":0,"content_index":0,"item_id":SECRET,"delta":SECRET})]
                .iter().map(|event| format!("data: {event}\n\n")).collect()
        } else if self.mode == 1 {
            json!({"error": {"message": SECRET}}).to_string()
        } else {
            success_body(&request)
        };
        Ok(LlmHttpResponse {
            status: if self.mode == 1 { 500 } else { 200 },
            headers: vec![
                ("content-type".into(), "text/event-stream".into()),
                ("x-request-id".into(), SECRET.into()),
            ],
            body: LlmHttpBody::buffered(body),
        })
    }
}
fn reference() -> AttachmentRef {
    AttachmentRef {
        id: AttachmentId::parse("a".repeat(64)).unwrap(),
        media_type: MediaType::parse("image/png").unwrap(),
        byte_len: 1,
        label: None,
        type_metadata: None,
    }
}
fn success_body(request: &LlmHttpRequest) -> String {
    let body: serde_json::Value = serde_json::from_slice(&request.body).unwrap();
    if body.get("messages").is_some() {
        format!(
            "data: {}\n\ndata: {}\n\ndata: [DONE]\n\n",
            json!({"choices":[{"index":0,"delta":{"content":SECRET},"finish_reason":null}]}),
            json!({"choices":[{"index":0,"delta":{},"finish_reason":"stop"}]})
        )
    } else {
        let item = json!({"id":SECRET,"type":"message","role":"assistant","status":"completed","content":[{"type":"output_text","text":SECRET}]});
        let events = [
            json!({"type":"response.output_item.added","output_index":0,"item":{"id":SECRET,"type":"message","role":"assistant","content":[]}}),
            json!({"type":"response.output_text.delta","item_id":SECRET,"output_index":0,"content_index":0,"delta":SECRET}),
            json!({"type":"response.output_text.done","item_id":SECRET,"output_index":0,"content_index":0,"text":SECRET}),
            json!({"type":"response.output_item.done","output_index":0,"item":item}),
            json!({"type":"response.completed","response":{"id":SECRET,"status":"completed","output":[item],"usage":{"input_tokens":1,"output_tokens":1,"total_tokens":2}}}),
        ];
        events.iter().map(|e| format!("data: {e}\n\n")).collect()
    }
}
// DELIVERY-SECRET: network writes alone may observe the delivered operand.
#[tokio::test]
async fn delivered_secrets_never_leave_send() {
    for adapter in 0..3 {
        for mode in 0..4 {
            if mode == 3 && adapter != 1 {
                continue;
            }
            let transport = Arc::new(EchoTransport {
                mode,
                seen: Mutex::new(0),
            });
            let mut provider: Box<dyn Provider> = match adapter {
                0 => Box::new(
                    crate::OpenAiCompatibleProvider::new("key", "https://example.invalid/v1")
                        .with_transport(transport.clone()),
                ),
                1 => Box::new(crate::OpenAiProvider::new("key").with_transport(transport.clone())),
                _ => Box::new(
                    crate::CodexProvider::new(Arc::new(ProviderToken::new("key")))
                        .force_sse_transport()
                        .with_http_transport(transport.clone()),
                ),
            };
            let mut request = super::request(vec![]);
            request.messages = vec![LlmMessage::new(
                LlmRole::User,
                vec![LlmContentBlock::Attachment {
                    reference: Box::new(reference()),
                }],
            )];
            if mode == 3 {
                request.model.metadata_mut().capability.stream_termination =
                    Some(lash_core::provider::StreamTermination::RequireTerminalEvidence);
            }
            let traces = Arc::new(Mutex::new(Vec::new()));
            let sink = traces.clone();
            request.provider_trace = Some(LlmProviderTraceSender::new(move |event| {
                sink.lock().unwrap().push(format!("{event:?}"))
            }));
            let events = Arc::new(Mutex::new(Vec::new()));
            let sink = events.clone();
            request.stream_events = Some(LlmEventSender::new(move |event| {
                sink.lock().unwrap().push(format!("{event:?}"))
            }));
            let template = Arc::new(provider.lower(&request).await.unwrap());
            assert_eq!(template.slots().count(), 1);
            let delivery = Delivery::Url {
                url: DeliverySecret::new(SECRET.into()),
                valid_until_ms: None,
            };
            let encoded = provider
                .encode_slot(template.slots().next().unwrap(), &delivery)
                .unwrap();
            let live = LiveRequestBody::fill(Arc::clone(&template), vec![encoded]).unwrap();
            let guarantee_request = request.clone();
            let result = provider.send(request, &live).await;
            if mode == 3 {
                assert!(result.is_err());
                assert_eq!(
                    provider.generation_retry_guarantee(&guarantee_request, &template),
                    lash_core::provider::GenerationRetryGuarantee::None,
                    "a checkpoint must not retain the previous delivery"
                );
            }
            assert_eq!(*transport.seen.lock().unwrap(), 1);
            if mode == 0 {
                assert!(result.is_ok(), "{result:?}");
            } else {
                let error = result.as_ref().unwrap_err();
                if mode != 3 {
                    assert_eq!(error.http_status, Some(if mode == 1 { 500 } else { 503 }));
                }
                if mode == 2 {
                    assert!(error.partial_response.is_some());
                }
            }
            assert!(!format!("{result:?}").contains(SECRET));
            let traces = traces.lock().unwrap();
            assert!(!traces.is_empty());
            assert!(!traces.join(" ").contains(SECRET));
            assert!(!events.lock().unwrap().join(" ").contains(SECRET));
        }
    }
}
