#![expect(
    clippy::expect_used,
    reason = "test target: clippy's allow-unwrap-in-tests only exempts #[test] functions, and the setup helpers around them in this target are test code too"
)]

use async_trait::async_trait;
use lash_core::facade_support::LlmTransportError;
use lash_core::provider::{Provider, ProviderToken};
use lash_core_store::attachments::AttachmentStore;
use lash_core_store::attachments::provider_files::{
    ProviderFileCacheLimits, ProviderFileDelivery, ProviderFileUploader,
};
use lash_llm_transport::{LlmHttpBody, LlmHttpRequest, LlmHttpResponse, LlmHttpTransport};
use lash_provider_google::GoogleOAuthProvider;
use lash_sansio::llm::attachment_delivery::{Delivery, DeliveryLimits, ProviderAccepts};
use lash_sansio::{AttachmentCreateMeta, AttachmentRef, MediaType};
use serde_json::json;
use std::sync::{Arc, Mutex};

#[derive(Debug)]
struct FilesTransport {
    uploads: Mutex<usize>,
    expiry_ms: Mutex<u64>,
}
#[async_trait]
impl LlmHttpTransport for FilesTransport {
    async fn send(
        &self,
        request: LlmHttpRequest,
        _: Option<std::time::Duration>,
    ) -> Result<LlmHttpResponse, LlmTransportError> {
        if request
            .headers
            .iter()
            .any(|(key, value)| key == "X-Goog-Upload-Command" && value.as_str() == "start")
        {
            return Ok(LlmHttpResponse {
                status: 200,
                headers: vec![(
                    "x-goog-upload-url".into(),
                    "https://generativelanguage.googleapis.com/upload/private".into(),
                )],
                body: LlmHttpBody::buffered(""),
            });
        }
        let mut count = self.uploads.lock().expect("uploads lock");
        *count += 1;
        let expiry = chrono::DateTime::from_timestamp_millis(
            i64::try_from(*self.expiry_ms.lock().expect("expiry lock")).expect("expiry fits"),
        )
        .expect("a valid expiry")
        .to_rfc3339();
        Ok(LlmHttpResponse {
            status: 200,
            headers: vec![("x-goog-upload-status".into(), "final".into())],
            body: LlmHttpBody::buffered(
                json!({"file":{"uri":format!("files/private-{count}"), "expirationTime":expiry}})
                    .to_string(),
            ),
        })
    }
}
fn handle(delivery: &Delivery) -> String {
    match delivery {
        Delivery::ProviderFile { id, .. } => id.expose().into(),
        _ => panic!("file-only acceptance must upload"),
    }
}
fn accepts(provider: &GoogleOAuthProvider) -> ProviderAccepts {
    ProviderAccepts {
        bytes: false,
        url: false,
        provider_file: provider.attachment_file_scope(),
    }
}
async fn deliver(
    store: &ProviderFileDelivery,
    reference: &AttachmentRef,
    provider: &GoogleOAuthProvider,
    horizon: u64,
) -> Delivery {
    store
        .deliver(
            reference,
            &accepts(provider),
            &DeliveryLimits {
                max_bytes: 4096,
                max_upload_bytes: 4096,
                valid_through_ms: horizon,
            },
        )
        .await
        .expect("the provider file is delivered")
}

// DELIVERY-SCOPE: a cached derivative never changes or retains the original ref.
#[tokio::test]
async fn provider_files_stay_in_their_scope() {
    let now = u64::try_from(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis(),
    )
    .unwrap();
    let transport = Arc::new(FilesTransport {
        uploads: Mutex::new(0),
        expiry_ms: Mutex::new(now + 100_000),
    });
    let base = GoogleOAuthProvider::new(Arc::new(ProviderToken::new("key")))
        .with_project_id(Some("project".into()))
        .with_attachment_credential_scope("account-a")
        .with_transport(transport.clone());
    let credential = base.clone().with_attachment_credential_scope("account-b");
    let endpoint = base.clone().with_endpoint("https://other.example.invalid");
    let project = base.clone().with_project_id(Some("other-project".into()));
    let providers = [base, credential, endpoint, project];
    let uploaders: Vec<Arc<dyn ProviderFileUploader>> = providers
        .iter()
        .map(|p| Arc::new(p.file_uploader().unwrap()) as Arc<dyn ProviderFileUploader>)
        .collect();
    for (index, uploader) in uploaders.iter().enumerate() {
        assert_eq!(
            uploader.scope(),
            providers[index].attachment_file_scope().as_ref().unwrap()
        );
        for other in &uploaders[..index] {
            assert_ne!(uploader.scope(), other.scope());
        }
    }
    let stores = lash::sqlite::SqliteStoreSet::memory().await.unwrap();
    let original = stores.attachment_store();
    let meta = AttachmentCreateMeta {
        media_type: MediaType::parse("image/png").unwrap(),
        label: None,
        type_metadata: None,
    };
    let reference = original.put(vec![1, 2, 3], meta.clone()).await.unwrap();
    let other = original.put(vec![4, 5, 6], meta).await.unwrap();
    let store = ProviderFileDelivery::new(
        original.clone(),
        uploaders,
        ProviderFileCacheLimits {
            capacity: 1,
            ttl_ms: 86_400_000,
        },
    );
    let first = deliver(&store, &reference, &providers[0], now + 1000).await;
    let reused = deliver(&store, &reference, &providers[0], now + 1000).await;
    assert_eq!(handle(&first), handle(&reused));
    assert_eq!(*transport.uploads.lock().unwrap(), 1);
    store.invalidate_delivery(&reference, &first).await.unwrap();
    let after_rejection = deliver(&store, &reference, &providers[0], now + 1000).await;
    assert_ne!(handle(&first), handle(&after_rejection));
    *transport.expiry_ms.lock().unwrap() = now + 1_000_000;
    let after_expiry = deliver(&store, &reference, &providers[0], now + 200_000).await;
    assert_ne!(handle(&after_rejection), handle(&after_expiry));
    let _ = deliver(&store, &other, &providers[0], now + 1000).await;
    let after_eviction = deliver(&store, &reference, &providers[0], now + 1000).await;
    assert_ne!(handle(&after_expiry), handle(&after_eviction));
    let uploads_before_scope_changes = *transport.uploads.lock().unwrap();
    for provider in &providers[1..] {
        let changed = deliver(&store, &reference, provider, now + 1000).await;
        assert_ne!(handle(&after_eviction), handle(&changed));
    }
    assert_eq!(
        *transport.uploads.lock().unwrap(),
        uploads_before_scope_changes + 3
    );
    // Recreating the derivative can upload again after a crash.
    let restarted = ProviderFileDelivery::new(
        original.clone(),
        vec![Arc::new(providers[0].file_uploader().unwrap())],
        ProviderFileCacheLimits {
            capacity: 1,
            ttl_ms: 86_400_000,
        },
    );
    let after_restart = deliver(&restarted, &reference, &providers[0], now + 1000).await;
    assert_ne!(handle(&after_eviction), handle(&after_restart));
    assert_eq!(
        original.get(&reference.id, 4096).await.unwrap().bytes,
        vec![1, 2, 3]
    );
    assert_eq!(
        original
            .put(
                vec![1, 2, 3],
                AttachmentCreateMeta {
                    media_type: reference.media_type.clone(),
                    label: None,
                    type_metadata: None
                }
            )
            .await
            .unwrap(),
        reference
    );
}
