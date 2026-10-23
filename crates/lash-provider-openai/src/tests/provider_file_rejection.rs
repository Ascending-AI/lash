//! DELIVERY-REJECTION (ADR 0135 §4, FIG-5514): Responses reports its
//! structured parameter refusal identifies only the rejected file delivery;
//! message-only refusals identify nothing and do not trigger re-upload.
use async_trait::async_trait;
use lash_core::attachments::{
    AttachmentStore, AttachmentStoreError, ProviderFileCacheLimits, ProviderFileDelivery,
    ProviderFileUploader, RuntimeAttachmentStore, UploadedProviderFile,
};
use lash_core::llm::types::SegmentCache;
use lash_core::llm::types::{AdmittedSend, LlmContentBlock, LlmMessage, LlmRole};
use lash_core::provider::{Provider, ProviderHandle, ProviderOptions, ProviderReliability};
use lash_llm_transport::{LlmHttpBody, LlmHttpRequest, LlmHttpResponse, LlmHttpTransport};
use lash_protocol_types::{AttachmentCreateMeta, AttachmentRef, MediaType};

use lash_protocol_types::{
    llm::attachment_delivery::DeliverySecret, llm::attachment_delivery::ProviderFileScope,
};

use lash_transport_failure::LlmTransportError;
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

/// One call naming distinct PDFs, sent through the handle's attempt loop over a
/// SQLite store, with `uploader` installed when `upload` is set.
async fn call(
    answers: Vec<(u16, String)>,
    upload: bool,
    file_count: usize,
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
    let mut blocks = Vec::new();
    for index in 0..file_count {
        let reference = original
            .put(
                format!("%PDF-1.4 document {index}").into_bytes(),
                AttachmentCreateMeta {
                    media_type: MediaType::parse("application/pdf").unwrap(),
                    label: None,
                    type_metadata: None,
                },
            )
            .await
            .unwrap();
        blocks.push(LlmContentBlock::Attachment {
            reference: Box::new(reference),
        });
    }
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
    let request = super::request(vec![LlmMessage::new(LlmRole::User, blocks)]);
    let template = Arc::new(
        provider
            .lower(&request, &SegmentCache::disabled())
            .await
            .unwrap(),
    );
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
async fn an_identified_file_parameter_reuploads_only_that_delivery() {
    let refusal = json!({"error": {"message": "rejected",
        "type": "invalid_request_error", "param": "input[0].content[1].file_id", "code": null}})
    .to_string();
    let (result, wires, uploads) = call(vec![(404, refusal), (200, answered())], true, 2).await;
    result.expect("the identified delivery is uploaded afresh");
    let wires: Vec<serde_json::Value> = wires
        .iter()
        .map(|wire| serde_json::from_str(wire).unwrap())
        .collect();
    assert_eq!(wires.len(), 2);
    assert_eq!(wires[0]["input"][0]["content"][0]["file_id"], "file-1");
    assert_eq!(wires[0]["input"][0]["content"][1]["file_id"], "file-2");
    assert_eq!(wires[1]["input"][0]["content"][0]["file_id"], "file-1");
    assert_eq!(wires[1]["input"][0]["content"][1]["file_id"], "file-3");
    assert_eq!(uploads, 3, "the other delivery stays cached");
}

#[tokio::test]
async fn an_unrelated_refusal_of_a_byte_delivery_is_not_retried() {
    let unrelated = json!({"error": {
        "message": "The file was not found to be a valid PDF: file expired.",
        "type": "invalid_request_error", "param": "input", "code": null}})
    .to_string();
    let (result, wires, uploads) = call(vec![(400, unrelated), (200, answered())], false, 1).await;
    assert!(result.is_err());
    assert_eq!(wires.len(), 1, "a byte delivery has nothing to invalidate");
    assert!(wires[0].contains(r#""file_data""#));
    assert_eq!(uploads, 0);
}

#[tokio::test]
async fn unidentified_file_refusals_do_not_invalidate_deliveries() {
    for (message, param) in [
        ("profile does not exist", "input"),
        ("File with id 'file-1' not found.", "input"),
        ("File not found", "input[99].content[0].file_id"),
        ("File not found", "input[0].content[99].file_id"),
        ("File not found", "input[0].content[0]"),
    ] {
        let refusal = json!({"error": {
            "message": message, "type": "invalid_request_error", "param": param, "code": null
        }})
        .to_string();
        let (result, wires, uploads) = call(vec![(404, refusal), (200, answered())], true, 1).await;
        assert!(
            result.is_err(),
            "no structured resource identity authorizes invalidation"
        );
        assert_eq!(wires.len(), 1);
        assert_eq!(uploads, 1);
    }
}
