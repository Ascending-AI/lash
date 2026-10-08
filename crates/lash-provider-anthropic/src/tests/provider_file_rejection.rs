//! DELIVERY-REJECTION (ADR 0135 §4, FIG-5514): Messages reports its definite
//! missing-file refusal against the slots it delivered as provider files,
//! and asks for the Files beta exactly when it delivered one.
use async_trait::async_trait;
use lash_core::facade_support::LlmTransportError;
use lash_core::llm::types::{
    LiveRequestBody, LlmContentBlock, LlmMessage, LlmRole, ResponseContext,
};
use lash_core::provider::Provider;
use lash_llm_transport::{LlmHttpBody, LlmHttpRequest, LlmHttpResponse, LlmHttpTransport};
use lash_sansio::llm::attachment_delivery::{Delivery, DeliverySecret};
use lash_sansio::{AttachmentId, AttachmentRef, MediaType};
use serde_json::json;
use std::sync::{Arc, Mutex};

/// Answers every request `(status, body)` and records each `anthropic-beta`.
#[derive(Debug)]
struct Refusing {
    status: u16,
    body: String,
    betas: Mutex<Vec<String>>,
}
#[async_trait]
impl LlmHttpTransport for Refusing {
    async fn send(
        &self,
        request: LlmHttpRequest,
        _: Option<std::time::Duration>,
    ) -> Result<LlmHttpResponse, LlmTransportError> {
        let beta = request
            .headers
            .iter()
            .find(|(name, _)| name.eq_ignore_ascii_case("anthropic-beta"))
            .map(|(_, value)| value.as_str().to_owned())
            .unwrap_or_default();
        self.betas.lock().unwrap().push(beta);
        Ok(LlmHttpResponse {
            status: self.status,
            headers: Vec::new(),
            body: LlmHttpBody::buffered(self.body.clone()),
        })
    }
}

/// Send one PDF slot filled by `delivery` to a transport answering
/// `(status, error)`; the error and the beta header that went out.
async fn refused(status: u16, error: serde_json::Value, file: bool) -> (LlmTransportError, String) {
    let transport = Arc::new(Refusing {
        status,
        body: json!({"type": "error", "error": error}).to_string(),
        betas: Mutex::new(Vec::new()),
    });
    let mut provider = crate::AnthropicProvider::new("key")
        .with_attachment_credential_scope("workspace")
        .with_transport(transport.clone());
    let request = super::request(vec![LlmMessage::new(
        LlmRole::User,
        vec![LlmContentBlock::Attachment {
            reference: Box::new(AttachmentRef {
                id: AttachmentId::parse("a".repeat(64)).unwrap(),
                media_type: MediaType::parse("application/pdf").unwrap(),
                byte_len: 1,
                label: None,
                type_metadata: None,
            }),
        }],
    )]);
    let template = Arc::new(provider.lower(&request).await.unwrap());
    let slot = template.slots().next().expect("one slot");
    let delivery = if file {
        Delivery::ProviderFile {
            scope: provider.attachment_file_scope().expect("a file scope"),
            id: DeliverySecret::new("file_011".into()),
            valid_until_ms: None,
        }
    } else {
        Delivery::Bytes(vec![7])
    };
    let encoded = provider.encode_slot(slot, &delivery).unwrap();
    let live = LiveRequestBody::fill(Arc::clone(&template), vec![encoded]).unwrap();
    let error = provider
        .send(&live, ResponseContext::of_request(&request))
        .await
        .unwrap_err();
    let beta = transport.betas.lock().unwrap().join(";");
    (error, beta)
}

#[tokio::test]
async fn a_missing_file_id_marks_the_slot_delivered_as_a_file() {
    let missing = json!({"type": "not_found_error", "message": "File not found: file_011"});
    let (error, beta) = refused(404, missing.clone(), true).await;
    assert_eq!(error.rejected_slots(), [0]);
    assert!(beta.contains("files-api-2025-04-14"), "{beta}");

    // The same answer to a byte delivery names nothing this body sent.
    let (error, beta) = refused(404, missing, false).await;
    assert!(error.rejected_slots().is_empty());
    assert!(!beta.contains("files-api"), "{beta}");

    // Another missing resource is not a missing file.
    let model = json!({"type": "not_found_error", "message": "model: claude-none"});
    let (error, _) = refused(404, model, true).await;
    assert!(error.rejected_slots().is_empty());
}
