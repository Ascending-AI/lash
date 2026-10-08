//! DELIVERY-REJECTION (ADR 0135 §4, FIG-5514): Responses reports its
//! definite missing-file refusal against the slots it delivered as provider
//! files, so the handle forgets those cached files and the next attempt
//! uploads afresh.
use async_trait::async_trait;
use lash_core::attachments::{
    AttachmentStore, AttachmentStoreError, ProviderFileCacheLimits, ProviderFileDelivery,
    ProviderFileUploader, RuntimeAttachmentStore, UploadedProviderFile,
};
use lash_core::facade_support::LlmTransportError;
use lash_core::llm::types::{AdmittedSend, LlmContentBlock, LlmMessage, LlmRole};
use lash_core::provider::{Provider, ProviderHandle, ProviderOptions, ProviderReliability};
use lash_llm_transport::{LlmHttpBody, LlmHttpRequest, LlmHttpResponse, LlmHttpTransport};
use lash_sansio::llm::attachment_delivery::{DeliverySecret, ProviderFileScope};
use lash_sansio::{AttachmentCreateMeta, AttachmentRef, MediaType};
use serde_json::json;
use std::sync::{Arc, Mutex};

/// Answers each request with the next scripted `(status, body)` and records
/// every wire it was sent.
#[derive(Debug)]
struct Scripted {
    answers: Mutex<Vec<(u16, String)>>,
    wires: Mutex<Vec<String>>,
}
#[async_trait]
impl LlmHttpTransport for Scripted {
    async fn send(
        &self,
        request: LlmHttpRequest,
        _: Option<std::time::Duration>,
    ) -> Result<LlmHttpResponse, LlmTransportError> {
        self.wires
            .lock()
            .unwrap()
            .push(String::from_utf8(request.body.to_vec()).unwrap());
        let (status, body) = self.answers.lock().unwrap().remove(0);
        Ok(LlmHttpResponse {
            status,
            headers: vec![("content-type".into(), "text/event-stream".into())],
            body: LlmHttpBody::buffered(body),
        })
    }
}

/// Mints `file-<n>` for its n-th upload.
struct CountingUploader {
    scope: ProviderFileScope,
    uploads: Mutex<usize>,
}
#[async_trait]
impl ProviderFileUploader for CountingUploader {
    fn scope(&self) -> &ProviderFileScope {
        &self.scope
    }
    async fn upload(
        &self,
        _: &AttachmentRef,
        _: &[u8],
    ) -> Result<UploadedProviderFile, AttachmentStoreError> {
        let mut uploads = self.uploads.lock().unwrap();
        *uploads += 1;
        Ok(UploadedProviderFile {
            id: DeliverySecret::new(format!("file-{uploads}")),
            valid_until_ms: None,
        })
    }
}

fn answered() -> String {
    let item = json!({"id":"msg_1","type":"message","role":"assistant","status":"completed",
        "content":[{"type":"output_text","text":"read"}]});
    [
        json!({"type":"response.output_item.added","output_index":0,"item":{"id":"msg_1","type":"message","role":"assistant","content":[]}}),
        json!({"type":"response.output_text.delta","item_id":"msg_1","output_index":0,"content_index":0,"delta":"read"}),
        json!({"type":"response.output_text.done","item_id":"msg_1","output_index":0,"content_index":0,"text":"read"}),
        json!({"type":"response.output_item.done","output_index":0,"item":item}),
        json!({"type":"response.completed","response":{"id":"resp_1","status":"completed","output":[item],
            "usage":{"input_tokens":1,"output_tokens":1,"total_tokens":2}}}),
    ]
    .iter()
    .map(|event| format!("data: {event}\n\n"))
    .collect()
}

/// One call naming one PDF, sent through the handle's attempt loop over a
/// SQLite store, with `uploader` installed when `upload` is set.
async fn call(
    answers: Vec<(u16, String)>,
    upload: bool,
) -> (
    Result<(), lash_core::provider::ProviderCompletionError>,
    Vec<String>,
    usize,
) {
    let transport = Arc::new(Scripted {
        answers: Mutex::new(answers),
        wires: Mutex::new(Vec::new()),
    });
    let mut provider = crate::OpenAiProvider::new("key")
        .with_attachment_credential_scope("account")
        .with_transport(transport.clone())
        .with_options(ProviderOptions {
            reliability: ProviderReliability::default()
                .max_attempts(Some(3))
                .base_delay_ms(0)
                .max_delay_ms(0),
            ..ProviderOptions::default()
        });
    let uploader = Arc::new(CountingUploader {
        scope: provider.attachment_file_scope().expect("a file scope"),
        uploads: Mutex::new(0),
    });
    let stores = lash::sqlite::SqliteStoreSet::memory().await.unwrap();
    let original = stores.attachment_store();
    let reference = original
        .put(
            b"%PDF-1.4 a document".to_vec(),
            AttachmentCreateMeta {
                media_type: MediaType::parse("application/pdf").unwrap(),
                label: None,
                type_metadata: None,
            },
        )
        .await
        .unwrap();
    let backend: Arc<dyn AttachmentStore> = if upload {
        Arc::new(ProviderFileDelivery::new(
            original,
            vec![uploader.clone()],
            ProviderFileCacheLimits::default(),
        ))
    } else {
        original
    };
    let deliveries = RuntimeAttachmentStore::ephemeral(
        backend,
        lash_core::facade_support::AttachmentPolicy::standard(),
    );
    let request = super::request(vec![LlmMessage::new(
        LlmRole::User,
        vec![LlmContentBlock::Attachment {
            reference: Box::new(reference),
        }],
    )]);
    let template = Arc::new(provider.lower(&request).await.unwrap());
    assert!(
        template
            .slots()
            .next()
            .expect("one slot")
            .accepts
            .provider_file
            .is_some()
    );
    let admitted = AdmittedSend::of_request(&request, template);
    let mut handle = ProviderHandle::new(provider.into_components());
    let result = lash_core::testing::runtime_helpers::send_admitted(
        &mut handle,
        request,
        &admitted,
        &deliveries,
    )
    .await
    .map(|_| ());
    let wires = transport.wires.lock().unwrap().clone();
    let uploads = *uploader.uploads.lock().unwrap();
    (result, wires, uploads)
}

#[tokio::test]
async fn a_missing_file_id_is_forgotten_and_uploaded_afresh() {
    let missing = json!({"error": {
        "message": "File with id 'file-1' not found.",
        "type": "invalid_request_error", "param": "input", "code": null}})
    .to_string();
    let (result, wires, uploads) = call(vec![(404, missing), (200, answered())], true).await;
    result.expect("the second attempt answers");
    assert_eq!(wires.len(), 2);
    assert!(wires[0].contains(r#""file_id":"file-1""#), "{}", wires[0]);
    assert!(
        wires[1].contains(r#""file_id":"file-2""#),
        "the rejected file is forgotten and uploaded afresh: {}",
        wires[1]
    );
    assert_eq!(uploads, 2);
}

#[tokio::test]
async fn an_unrelated_refusal_of_a_byte_delivery_is_not_retried() {
    let unrelated = json!({"error": {
        "message": "The file was not found to be a valid PDF: file expired.",
        "type": "invalid_request_error", "param": "input", "code": null}})
    .to_string();
    let (result, wires, uploads) = call(vec![(400, unrelated), (200, answered())], false).await;
    assert!(result.is_err());
    assert_eq!(wires.len(), 1, "a byte delivery has nothing to invalidate");
    assert!(wires[0].contains(r#""file_data""#));
    assert_eq!(uploads, 0);
}
