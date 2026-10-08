//! DELIVERY-DERIVATIVES: a provider file is the derivative of one content as
//! one media type, and the request budget charges it as a file, not as bytes
//! encoded inline (ADR 0135 §5).
use crate::*;
use lash_core::attachments::{ProviderFileDelivery, ProviderFileUploader, UploadedProviderFile};
use lash_core::provider::SlotDeliveries;
use lash_sansio::llm::attachment_delivery::{
    AttachmentPosition, Delivery, DeliveryContext, DeliverySecret, ProviderAccepts,
    ProviderFileScope,
};
use lash_sansio::llm::types::{AttachmentSlot, SlotCodec};
use std::sync::Mutex;

/// An uploader that names each file after the media type it was uploaded as
/// and records every upload.
struct Uploads {
    scope: ProviderFileScope,
    media_types: Mutex<Vec<String>>,
}
#[async_trait::async_trait]
impl ProviderFileUploader for Uploads {
    fn scope(&self) -> &ProviderFileScope {
        &self.scope
    }
    async fn upload(
        &self,
        reference: &AttachmentRef,
        bytes: &[u8],
    ) -> Result<UploadedProviderFile, AttachmentStoreError> {
        assert_eq!(bytes.len() as u64, reference.byte_len);
        let media_type = reference.media_type.as_str().to_owned();
        let id = format!("files/{media_type}");
        #[expect(clippy::unwrap_used, reason = "no holder of the fixture lock panics")]
        self.media_types.lock().unwrap().push(media_type);
        Ok(UploadedProviderFile {
            id: DeliverySecret::new(id),
            valid_until_ms: None,
        })
    }
}
impl Uploads {
    #[expect(clippy::unwrap_used, reason = "no holder of the fixture lock panics")]
    fn seen(&self) -> Vec<String> {
        self.media_types.lock().unwrap().clone()
    }
}

struct Fixture {
    uploads: Arc<Uploads>,
    store: RuntimeAttachmentStore,
    ctx: DeliveryContext,
}

fn fixture(backend: Arc<dyn AttachmentStore>, policy: AttachmentReadPolicy) -> Fixture {
    let scope = ProviderFileScope {
        provider: "fixture".into(),
        endpoint: "https://files.invalid".into(),
        credential_scope: "account".into(),
    };
    let uploads = Arc::new(Uploads {
        scope: scope.clone(),
        media_types: Mutex::new(Vec::new()),
    });
    let files = Arc::new(ProviderFileDelivery::new(
        backend,
        vec![uploads.clone()],
        Default::default(),
    ));
    Fixture {
        uploads,
        store: RuntimeAttachmentStore::ephemeral(files).with_read_policy(policy),
        ctx: DeliveryContext {
            valid_through_ms: 1000,
            live_file_scope: Some(scope),
        },
    }
}

/// A slot that accepts `reference` only as a file of the fixture's scope.
fn file_slot(reference: AttachmentRef, ctx: &DeliveryContext) -> AttachmentSlot {
    AttachmentSlot {
        reference,
        accepts: ProviderAccepts {
            bytes: false,
            url: false,
            provider_file: ctx.live_file_scope.clone(),
        },
        position: AttachmentPosition::Message,
        codec: SlotCodec {
            name: "lash.canonical".into(),
            revision: 1,
        },
    }
}

fn file_id(delivery: &Delivery) -> &str {
    match delivery {
        Delivery::ProviderFile { id, .. } => id.expose(),
        other => panic!("expected a provider file, got {other:?}"),
    }
}

/// Two refs to the same bytes under different media types are two
/// derivatives: one request that names both uploads each as its own type and
/// hands each slot the file of its type, and a later request finds each in
/// the cache by its type.
#[expect(
    clippy::expect_used,
    clippy::unwrap_used,
    reason = "conformance fixture setup must succeed"
)]
pub async fn attachment_delivery_derives_per_media_type(backend: Arc<dyn AttachmentStore>) {
    let fixture = fixture(backend.clone(), AttachmentReadPolicy::DEFAULT);
    let put = |media_type: &'static str| {
        let backend = backend.clone();
        async move {
            backend
                .put(
                    b"one content, two types".to_vec(),
                    AttachmentCreateMeta::new(MediaType::parse(media_type).unwrap(), None, None),
                )
                .await
                .expect("put the content")
        }
    };
    let plain = put("text/plain").await;
    let markdown = put("text/markdown").await;
    assert_eq!(plain.id, markdown.id, "one content has one id");
    let plain = file_slot(plain, &fixture.ctx);
    let markdown = file_slot(markdown, &fixture.ctx);

    let first = fixture
        .store
        .deliver(&[&plain, &markdown, &plain], &fixture.ctx)
        .await
        .expect("both derivatives are delivered");
    assert_eq!(
        fixture.uploads.seen(),
        ["text/plain", "text/markdown"],
        "each media type is uploaded once, as itself"
    );
    assert_eq!(file_id(&first[0]), "files/text/plain");
    assert_eq!(file_id(&first[1]), "files/text/markdown");
    assert!(
        Arc::ptr_eq(&first[0], &first[2]),
        "occurrences of one derivative share its delivery"
    );

    let again = fixture
        .store
        .deliver(&[&markdown, &plain], &fixture.ctx)
        .await
        .expect("both derivatives are reused");
    assert_eq!(
        fixture.uploads.seen().len(),
        2,
        "a cache hit uploads nothing"
    );
    assert_eq!(file_id(&again[0]), "files/text/markdown");
    assert_eq!(file_id(&again[1]), "files/text/plain");
}

/// A provider file is charged its upload scratch on a cache miss and its
/// escaped id on a hit, never the inline encoding: a file too large to send
/// as base64 under the request budget is uploaded, and then reused by a
/// request whose budget could not even hold its bytes.
///
/// The policy is the default (32 MiB a blob, 128 MiB a request) at one
/// 1024th scale, so the blob sits where a 24 MiB file does at the default:
/// above the ceiling an inline encoding leaves (about 20 MiB), below the
/// blob bound.
#[expect(clippy::expect_used, reason = "conformance fixture setup must succeed")]
pub async fn attachment_delivery_charges_a_provider_file_as_a_file(
    backend: Arc<dyn AttachmentStore>,
) {
    let scaled = AttachmentReadPolicy {
        max_blob_bytes: AttachmentReadPolicy::DEFAULT.max_blob_bytes / 1024,
        max_request_bytes: AttachmentReadPolicy::DEFAULT.max_request_bytes / 1024,
    };
    let fixture = fixture(backend.clone(), scaled);
    let reference = backend
        .put(
            vec![7; 24 * 1024],
            AttachmentCreateMeta::new(
                MediaType::parse("application/pdf").expect("fixture MIME is valid"),
                None,
                None,
            ),
        )
        .await
        .expect("put the file");
    let mut inline = file_slot(reference.clone(), &fixture.ctx);
    inline.accepts = ProviderAccepts {
        bytes: true,
        ..ProviderAccepts::NONE
    };
    assert!(
        fixture
            .store
            .deliver(&[&inline], &fixture.ctx)
            .await
            .is_err(),
        "the fixture file does not fit the request budget encoded inline"
    );

    let file = file_slot(reference, &fixture.ctx);
    let uploaded = fixture
        .store
        .deliver(&[&file], &fixture.ctx)
        .await
        .expect("a cache miss is charged its upload scratch, which fits");
    assert!(matches!(
        &*uploaded[0],
        Delivery::ProviderFile { uploaded: true, .. }
    ));
    assert_eq!(fixture.uploads.seen().len(), 1);

    // A request budget below the file's size: only a charge that reads
    // nothing admits it.
    let tight = fixture
        .store
        .reconfigured_read_policy(AttachmentReadPolicy {
            max_blob_bytes: scaled.max_blob_bytes,
            max_request_bytes: 8 * 1024,
        });
    let reused = tight
        .deliver(&[&file, &file], &fixture.ctx)
        .await
        .expect("a cache hit is charged its file id per occurrence");
    assert!(matches!(
        &*reused[0],
        Delivery::ProviderFile {
            uploaded: false,
            ..
        }
    ));
    assert_eq!(fixture.uploads.seen().len(), 1, "a cache hit reads nothing");

    // The same tight budget refuses the upload a miss would need.
    let cold = self::fixture(backend, scaled);
    let tight = cold.store.reconfigured_read_policy(AttachmentReadPolicy {
        max_blob_bytes: scaled.max_blob_bytes,
        max_request_bytes: 8 * 1024,
    });
    assert!(
        tight.deliver(&[&file], &cold.ctx).await.is_err(),
        "a cache miss cannot read past the request budget"
    );
    assert!(cold.uploads.seen().is_empty());
}
