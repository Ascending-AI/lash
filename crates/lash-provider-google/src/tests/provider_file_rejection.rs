//! DELIVERY-REJECTION (ADR 0135 §4, FIG-5514): the API's definite refusal
//! of a file marks the slots this attempt delivered as files, so the handle
//! forgets those cached uploads and the next attempt uploads afresh; a
//! refusal of a body that delivered bytes is left to the classifier.
use async_trait::async_trait;
use lash_core::attachments::{
    AttachmentStore, ProviderFileCacheLimits, ProviderFileDelivery, RuntimeAttachmentStore,
};
use lash_core::facade_support::LlmTransportError;
use lash_core::llm::types::{AdmittedSend, LlmContentBlock, LlmMessage, LlmRole};
use lash_core::provider::{
    Provider, ProviderHandle, ProviderOptions, ProviderReliability, ProviderToken,
};
use lash_llm_transport::{LlmHttpBody, LlmHttpRequest, LlmHttpResponse, LlmHttpTransport};
use lash_sansio::{AttachmentCreateMeta, MediaType};
use serde_json::json;
use std::sync::{Arc, Mutex};

/// Serves the Files upload protocol, minting `files/upload-<n>`, and answers
/// each generation request with the next scripted `(status, body)`.
#[derive(Debug)]
struct Scripted {
    answers: Mutex<Vec<(u16, String)>>,
    wires: Mutex<Vec<String>>,
    uploads: Mutex<usize>,
}
#[async_trait]
impl LlmHttpTransport for Scripted {
    async fn send(
        &self,
        request: LlmHttpRequest,
        _: Option<std::time::Duration>,
    ) -> Result<LlmHttpResponse, LlmTransportError> {
        let upload = request
            .headers
            .iter()
            .find(|(key, _)| key == "X-Goog-Upload-Command")
            .map(|(_, value)| value.as_str().to_owned());
        let (status, headers, body) = match upload.as_deref() {
            Some("start") => (
                200,
                vec![(
                    "x-goog-upload-url".into(),
                    "https://generativelanguage.googleapis.com/upload/session".into(),
                )],
                String::new(),
            ),
            Some(_) => {
                let mut uploads = self.uploads.lock().unwrap();
                *uploads += 1;
                (
                    200,
                    vec![("x-goog-upload-status".into(), "final".into())],
                    json!({"file": {"uri": format!("files/upload-{uploads}")}}).to_string(),
                )
            }
            None => {
                self.wires
                    .lock()
                    .unwrap()
                    .push(String::from_utf8(request.body.to_vec()).unwrap());
                let (status, body) = self.answers.lock().unwrap().remove(0);
                (
                    status,
                    vec![("content-type".into(), "application/json".into())],
                    body,
                )
            }
        };
        Ok(LlmHttpResponse {
            status,
            headers,
            body: LlmHttpBody::buffered(body),
        })
    }
}

fn answered() -> String {
    json!({"response":{"candidates":[{"finishReason":"STOP","content":{"parts":[{"text":"seen"}]}}]}})
        .to_string()
}

/// One call naming one image, sent through the handle's attempt loop over
/// a SQLite store, with the provider's own uploader installed when `upload`
/// is set: its outcome, each generation wire, and the upload count.
async fn call(
    answers: Vec<(u16, String)>,
    upload: bool,
) -> (Result<(), String>, Vec<String>, usize) {
    let transport = Arc::new(Scripted {
        answers: Mutex::new(answers),
        wires: Mutex::new(Vec::new()),
        uploads: Mutex::new(0),
    });
    let mut provider = crate::GoogleOAuthProvider::new(Arc::new(ProviderToken::new("key")))
        .with_project_id(Some("project".into()))
        .with_attachment_credential_scope("host-account")
        .with_transport(transport.clone())
        .with_options(ProviderOptions {
            reliability: ProviderReliability::default()
                .max_attempts(Some(3))
                .base_delay_ms(0)
                .max_delay_ms(0),
            ..ProviderOptions::default()
        });
    let stores = lash::sqlite::SqliteStoreSet::memory().await.unwrap();
    let original = stores.attachment_store();
    let reference = original
        .put(
            vec![1, 2, 3],
            AttachmentCreateMeta {
                media_type: MediaType::parse("image/png").unwrap(),
                label: None,
                type_metadata: None,
            },
        )
        .await
        .unwrap();
    let backend: Arc<dyn AttachmentStore> = if upload {
        Arc::new(ProviderFileDelivery::new(
            original,
            vec![Arc::new(provider.file_uploader().expect("a file scope"))],
            ProviderFileCacheLimits::default(),
        ))
    } else {
        original
    };
    let deliveries = RuntimeAttachmentStore::ephemeral(
        backend,
        lash_core::facade_support::AttachmentPolicy::standard(),
    );
    let request = super::request(None);
    let request = lash_core::llm::types::LlmRequest {
        messages: vec![LlmMessage::new(
            LlmRole::User,
            vec![LlmContentBlock::Attachment {
                reference: Box::new(reference),
            }],
        )],
        ..request
    };
    let template = Arc::new(provider.lower(&request).await.unwrap());
    assert!(
        template
            .slots()
            .next()
            .expect("one slot")
            .accepts
            .provider_file
            .is_some(),
        "the slot's pinned acceptance allows a file either way"
    );
    let admitted = AdmittedSend::of_request(&request, template);
    let mut handle = ProviderHandle::new(provider.into_components());
    let answered = lash_core::testing::runtime_helpers::send_admitted(
        &mut handle,
        request,
        &admitted,
        &deliveries,
    )
    .await
    .map(|_| ())
    .map_err(|error| format!("{error:?}"));
    let wires = transport.wires.lock().unwrap().clone();
    let uploads = *transport.uploads.lock().unwrap();
    (answered, wires, uploads)
}

#[tokio::test]
async fn a_missing_file_is_forgotten_and_uploaded_afresh() {
    let missing = json!({"error": {"code": 404, "status": "NOT_FOUND",
        "message": "File files/upload-1 not found or has expired."}})
    .to_string();
    let (answered, wires, uploads) =
        call(vec![(404, missing), (200, self::answered())], true).await;
    answered.expect("the second attempt answers");
    assert_eq!(wires.len(), 2);
    assert!(wires[0].contains("files/upload-1"), "{}", wires[0]);
    assert!(
        wires[1].contains("files/upload-2"),
        "the rejected file is forgotten and uploaded afresh: {}",
        wires[1]
    );
    assert_eq!(uploads, 2);
}

#[tokio::test]
async fn an_unrelated_refusal_of_a_byte_delivery_is_not_retried() {
    let unrelated = json!({"error": {"code": 400, "status": "INVALID_ARGUMENT",
        "message": "A file part was not found to be valid: the inline data expired."}})
    .to_string();
    let (answered, wires, uploads) =
        call(vec![(400, unrelated), (200, self::answered())], false).await;
    assert!(answered.is_err());
    assert_eq!(wires.len(), 1, "a byte delivery has nothing to invalidate");
    assert!(wires[0].contains("inlineData"), "{}", wires[0]);
    assert_eq!(uploads, 0);
}

// FIG-5743: a resumable-upload reply cannot send bytes or bearer credentials
// outside the origin that accepted the upload start.
#[tokio::test]
async fn file_upload_refuses_a_foreign_origin_before_sending_and_finalizes_same_origin() {
    use lash_core_store::attachments::provider_files::ProviderFileUploader;
    use lash_core_store::attachments::{AttachmentStoreError, AttachmentStoreFailureClass};
    use std::io::{Read, Write};
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let foreign = format!("http://{}/session", listener.local_addr().unwrap());
    let requests = Arc::new(AtomicUsize::new(0));
    let stop = Arc::new(AtomicBool::new(false));
    let server = std::thread::spawn({
        let (requests, stop) = (requests.clone(), stop.clone());
        move || {
            while !stop.load(Ordering::SeqCst) {
                match listener.accept() {
                    Ok((mut stream, _)) => {
                        requests.fetch_add(1, Ordering::SeqCst);
                        stream
                            .set_read_timeout(Some(std::time::Duration::from_secs(1)))
                            .unwrap();
                        let mut buffer = [0; 4096];
                        let _ = stream.read(&mut buffer);
                        let body = r#"{"file":{"uri":"files/leaked"}}"#;
                        write!(stream, "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).unwrap();
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        std::thread::sleep(std::time::Duration::from_millis(1))
                    }
                    Err(error) => panic!("listener failed: {error}"),
                }
            }
        }
    });
    #[derive(Debug)]
    struct UploadTransport {
        url: String,
        finalized: AtomicUsize,
    }
    #[async_trait]
    impl LlmHttpTransport for UploadTransport {
        async fn send(
            &self,
            request: LlmHttpRequest,
            timeout: Option<std::time::Duration>,
        ) -> Result<LlmHttpResponse, LlmTransportError> {
            if request
                .headers
                .iter()
                .any(|(name, value)| name == "X-Goog-Upload-Command" && value.as_str() == "start")
            {
                assert_eq!(
                    request.url,
                    "https://generativelanguage.googleapis.com/upload/v1beta/files"
                );
                return Ok(LlmHttpResponse {
                    status: 200,
                    headers: vec![("x-goog-upload-url".into(), self.url.clone())],
                    body: LlmHttpBody::buffered(""),
                });
            }
            self.finalized.fetch_add(1, Ordering::SeqCst);
            if request.url.starts_with("http://127.0.0.1:") {
                return lash_llm_transport::ReqwestLlmHttpTransport::new()
                    .send(request, timeout)
                    .await;
            }
            assert_eq!(
                request.url,
                "https://generativelanguage.googleapis.com/upload/session"
            );
            assert_eq!(request.body.as_ref(), &[1, 2, 3]);
            assert!(
                request
                    .headers
                    .iter()
                    .any(|(name, value)| name == "Authorization"
                        && value.as_str() == "Bearer private-key")
            );
            Ok(LlmHttpResponse {
                status: 200,
                headers: vec![("x-goog-upload-status".into(), "final".into())],
                body: LlmHttpBody::buffered(r#"{"file":{"uri":"files/control"}}"#),
            })
        }
    }
    let reference = lash_sansio::AttachmentRef {
        id: lash_sansio::AttachmentId::parse("ab".repeat(32)).unwrap(),
        media_type: MediaType::parse("image/png").unwrap(),
        byte_len: 3,
        type_metadata: None,
        label: None,
    };
    let transport = Arc::new(UploadTransport {
        url: foreign,
        finalized: AtomicUsize::new(0),
    });
    let provider = crate::GoogleOAuthProvider::new(Arc::new(ProviderToken::new("private-key")))
        .with_project_id(Some("project".into()))
        .with_attachment_credential_scope("account")
        .with_transport(transport.clone());
    let result = provider
        .file_uploader()
        .unwrap()
        .upload(&reference, &[1, 2, 3])
        .await;
    stop.store(true, Ordering::SeqCst);
    server.join().unwrap();
    let error = result.unwrap_err();
    let AttachmentStoreError::Backend { class, source, .. } = error else {
        panic!("typed upload refusal required");
    };
    assert_eq!(class, AttachmentStoreFailureClass::Terminal);
    let cause = source
        .downcast_ref::<LlmTransportError>()
        .expect("transport cause is retained");
    assert_eq!(cause.kind, lash_core::ProviderFailureKind::Validation);
    assert_eq!(
        cause.code.as_ref().unwrap().spelling(),
        "invalid_provider_endpoint"
    );
    assert_eq!(
        requests.load(Ordering::SeqCst),
        0,
        "no body or credential reaches the foreign listener"
    );
    assert_eq!(transport.finalized.load(Ordering::SeqCst), 0);
    let transport = Arc::new(UploadTransport {
        url: "https://generativelanguage.googleapis.com/upload/session".into(),
        finalized: AtomicUsize::new(0),
    });
    let provider = crate::GoogleOAuthProvider::new(Arc::new(ProviderToken::new("private-key")))
        .with_project_id(Some("project".into()))
        .with_attachment_credential_scope("account")
        .with_transport(transport.clone());
    let file = provider
        .file_uploader()
        .unwrap()
        .upload(&reference, &[1, 2, 3])
        .await
        .unwrap();
    assert_eq!(file.id.expose(), "files/control");
    assert_eq!(transport.finalized.load(Ordering::SeqCst), 1);
}
