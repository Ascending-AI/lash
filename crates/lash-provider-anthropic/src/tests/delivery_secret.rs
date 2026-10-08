use async_trait::async_trait;
use lash_core::facade_support::LlmTransportError;
use lash_core::llm::types::{
    LiveRequestBody, LlmContentBlock, LlmEventSender, LlmMessage, LlmOutputPart,
    LlmProviderTraceSender, LlmResponse, LlmRole, ResponseContext,
};
use lash_core::provider::Provider;
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
        let body = if self.mode == 1 {
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
fn success_body(_: &LlmHttpRequest) -> String {
    let half = SECRET.len() / 2;
    let events = [
        json!({"type":"message_start","message":{"id":SECRET,"usage":{"input_tokens":1}}}),
        json!({"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}),
        json!({"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":&SECRET[..half]}}),
        json!({"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":&SECRET[half..]}}),
        json!({"type":"content_block_stop","index":0}),
        json!({"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":1}}),
        json!({"type":"message_stop"}),
    ];
    events.iter().map(|e| format!("data: {e}\n\n")).collect()
}
// DELIVERY-SECRET: network writes alone may observe the delivered operand.
#[tokio::test]
async fn delivered_secrets_never_leave_send() {
    for _adapter in 0..1 {
        for mode in 0..3 {
            let transport = Arc::new(EchoTransport {
                mode,
                seen: Mutex::new(0),
            });
            let mut provider: Box<dyn Provider> =
                Box::new(crate::AnthropicProvider::new("key").with_transport(transport.clone()));
            let mut request = super::request(vec![]);
            request.messages = vec![LlmMessage::new(
                LlmRole::User,
                vec![LlmContentBlock::Attachment {
                    reference: Box::new(reference()),
                }],
            )];
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
            let live = LiveRequestBody::fill(template, vec![encoded]).unwrap();
            let result = provider
                .send(&live, ResponseContext::of_request(&request))
                .await;
            assert_eq!(*transport.seen.lock().unwrap(), 1);
            if mode == 0 {
                assert!(result.is_ok(), "{result:?}");
            } else {
                let error = result.as_ref().unwrap_err();
                assert_eq!(error.http_status, Some(if mode == 1 { 500 } else { 503 }));
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
