//! DELIVERY-REJECTION (ADR 0135 §4, FIG-7265): a message-only file
//! refusal identifies no delivery and reaches the host without re-upload.
use async_trait::async_trait;
use lash_core::llm::types::SegmentCache;
use lash_core::llm::types::{AdmittedSend, AttachmentSlot, LlmContentBlock, LlmMessage, LlmRole};
use lash_core::provider::{
    AttachmentDeliveryError, Provider, ProviderHandle, ProviderOptions, ProviderReliability,
    SlotDeliveries,
};
use lash_llm_transport::{LlmHttpBody, LlmHttpRequest, LlmHttpResponse, LlmHttpTransport};
use lash_protocol_types::{AttachmentRef, MediaType};

use lash_protocol_types::{
    llm::attachment_delivery::Delivery, llm::attachment_delivery::DeliveryContext,
    llm::attachment_delivery::DeliverySecret, llm::attachment_delivery::ProviderFileScope,
};

use lash_transport_failure::LlmTransportError;
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

struct FileDeliveries {
    file: bool,
    scope: ProviderFileScope,
    invalidations: std::sync::atomic::AtomicUsize,
}
#[async_trait]
impl SlotDeliveries for FileDeliveries {
    async fn deliver(
        &self,
        slots: &[&AttachmentSlot],
        _: &DeliveryContext,
    ) -> Result<Vec<Arc<Delivery>>, AttachmentDeliveryError> {
        Ok(slots
            .iter()
            .map(|_| {
                Arc::new(if self.file {
                    Delivery::ProviderFile {
                        scope: self.scope.clone(),
                        id: DeliverySecret::new("file_011".into()),
                        valid_until_ms: None,
                        uploaded: false,
                    }
                } else {
                    Delivery::Bytes(vec![7])
                })
            })
            .collect())
    }
    async fn invalidate(
        &self,
        _: &AttachmentRef,
        _: &Delivery,
    ) -> Result<(), AttachmentDeliveryError> {
        self.invalidations
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        Ok(())
    }
}

#[tokio::test]
async fn unidentified_file_refusal_reaches_the_host_without_invalidating_or_retrying() {
    for (file, message) in [
        (true, "File not found: file_011"),
        (true, "profile does not exist"),
        (false, "File not found: file_011"),
    ] {
        let transport = Arc::new(Refusing {
            status: 404,
            body:
                json!({"type": "error", "error": {"type": "not_found_error", "message": message}})
                    .to_string(),
            betas: Mutex::new(Vec::new()),
        });
        let mut provider = crate::AnthropicProvider::new("key")
            .with_attachment_credential_scope("workspace")
            .with_transport(transport.clone())
            .with_options(ProviderOptions {
                reliability: ProviderReliability::default()
                    .max_attempts(Some(3))
                    .base_delay_ms(0)
                    .max_delay_ms(0),
                ..ProviderOptions::default()
            });
        let request = super::request(vec![LlmMessage::new(
            LlmRole::User,
            vec![LlmContentBlock::Attachment {
                reference: Box::new(AttachmentRef {
                    id: lash_protocol_types::attachment::content_id(&[7]),
                    media_type: MediaType::parse("application/pdf").unwrap(),
                    byte_len: 1,
                    label: None,
                    type_metadata: None,
                }),
            }],
        )]);
        let deliveries = FileDeliveries {
            file,
            scope: provider.attachment_file_scope().expect("a file scope"),
            invalidations: std::sync::atomic::AtomicUsize::new(0),
        };
        let template = Arc::new(
            provider
                .lower(&request, &SegmentCache::disabled())
                .await
                .unwrap(),
        );
        let admitted = AdmittedSend::of_request(&request, template);
        let mut handle = ProviderHandle::new(provider.into_components());
        let error = lash_core::testing::runtime_helpers::send_admitted(
            &mut handle,
            request,
            &admitted,
            &deliveries,
        )
        .await
        .expect_err("a message-only refusal reaches the host");
        assert_eq!(error.http_status, Some(404));
        assert_eq!(
            error.kind,
            lash_core::llm::transport::ProviderFailureKind::Http
        );
        assert_eq!(
            error.retry_verdict,
            lash_transport_failure::TransportRetryVerdict::NotRetryable
        );
        assert!(error.rejected_slots().is_empty());
        assert_eq!(
            deliveries
                .invalidations
                .load(std::sync::atomic::Ordering::SeqCst),
            0
        );
        let betas = transport.betas.lock().unwrap();
        assert_eq!(
            betas.len(),
            1,
            "the host receives the original refusal without re-upload"
        );
        assert_eq!(betas[0].contains("files-api-2025-04-14"), file);
    }
}
