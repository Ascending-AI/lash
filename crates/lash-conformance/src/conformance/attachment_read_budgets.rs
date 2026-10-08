//! DELIVERY-BOUNDS: actual content, allocation and every encoded occurrence are bounded.
use crate::*;
use lash_core::attachments::{ProviderFileDelivery, ProviderFileUploader, UploadedProviderFile};
use lash_core::provider::{AttachmentDeliveryError, SlotDeliveries};
use lash_sansio::llm::attachment_delivery::ProviderFileScope;
use lash_sansio::llm::attachment_delivery::{
    Delivery, DeliveryContext, DeliveryLimits, DeliverySecret, ProviderAccepts,
};
use lash_sansio::llm::types::{AttachmentSlot, SlotCodec};
use std::sync::atomic::{AtomicUsize, Ordering};

struct FileProbe {
    scope: ProviderFileScope,
    uploads: AtomicUsize,
    failure: Option<AttachmentStoreFailureClass>,
    valid_until_ms: Option<u64>,
}
#[async_trait::async_trait]
impl ProviderFileUploader for FileProbe {
    fn scope(&self) -> &ProviderFileScope {
        &self.scope
    }
    async fn upload(
        &self,
        reference: &AttachmentRef,
        bytes: &[u8],
    ) -> Result<UploadedProviderFile, AttachmentStoreError> {
        assert_eq!(bytes.len() as u64, reference.byte_len);
        self.uploads.fetch_add(1, Ordering::SeqCst);
        if let Some(class) = self.failure {
            return Err(AttachmentStoreError::Backend {
                operation: "upload",
                class,
                source: "upload-secret-sentinel".into(),
            });
        }
        Ok(UploadedProviderFile {
            id: DeliverySecret::new("overlong-file-id".repeat(8192)),
            valid_until_ms: self.valid_until_ms,
        })
    }
}

#[expect(clippy::unwrap_used, reason = "fixture MIME is valid")]
fn meta() -> AttachmentCreateMeta {
    AttachmentCreateMeta::new(MediaType::parse("image/png").unwrap(), None, None)
}
fn slot(reference: AttachmentRef, accepts: ProviderAccepts) -> AttachmentSlot {
    AttachmentSlot {
        reference,
        accepts,
        position: lash_sansio::llm::attachment_delivery::AttachmentPosition::Message,
        codec: SlotCodec {
            name: "lash.canonical".into(),
            revision: 1,
        },
    }
}
#[derive(Clone, Copy)]
enum Fault {
    None,
    Length,
    Digest,
    Capacity,
    Secret,
    Expired,
    Unaccepted,
}
struct DeliveryProbe {
    inner: Arc<dyn AttachmentStore>,
    calls: AtomicUsize,
    fault: Fault,
}
#[async_trait::async_trait]
impl AttachmentStore for DeliveryProbe {
    fn persistence(&self) -> AttachmentStorePersistence {
        self.inner.persistence()
    }
    async fn put(
        &self,
        bytes: Vec<u8>,
        meta: AttachmentCreateMeta,
    ) -> Result<AttachmentRef, AttachmentStoreError> {
        self.inner.put(bytes, meta).await
    }
    async fn get(
        &self,
        id: &AttachmentId,
        max: u64,
    ) -> Result<StoredAttachment, AttachmentStoreError> {
        self.inner.get(id, max).await
    }
    async fn delete(&self, id: &AttachmentId) -> Result<(), AttachmentStoreError> {
        self.inner.delete(id).await
    }
    async fn list(&self) -> Result<Vec<StoredBlobRef>, AttachmentStoreError> {
        self.inner.list().await
    }
    async fn head(&self, id: &AttachmentId) -> Result<Option<StoredBlobRef>, AttachmentStoreError> {
        self.inner.head(id).await
    }
    async fn deliver(
        &self,
        reference: &AttachmentRef,
        accepts: &ProviderAccepts,
        limits: &DeliveryLimits,
    ) -> Result<Delivery, AttachmentStoreError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        match self.fault {
            Fault::Length => Ok(Delivery::Bytes(vec![1; reference.byte_len as usize + 1])),
            Fault::Digest => Ok(Delivery::Bytes(vec![0; reference.byte_len as usize])),
            Fault::Capacity => {
                let mut bytes = Vec::with_capacity(limits.max_bytes as usize + 1);
                bytes.extend_from_slice(
                    &self.inner.get(&reference.id, limits.max_bytes).await?.bytes,
                );
                Ok(Delivery::Bytes(bytes))
            }
            Fault::Secret => Ok(Delivery::Url {
                url: DeliverySecret::new("https://immutable.invalid/".repeat(8192)),
                valid_until_ms: None,
            }),
            Fault::Expired => Ok(Delivery::Url {
                url: DeliverySecret::new("https://immutable.invalid/content".into()),
                valid_until_ms: Some(limits.valid_through_ms - 1),
            }),
            Fault::Unaccepted => Ok(Delivery::Url {
                url: DeliverySecret::new("https://immutable.invalid/content".into()),
                valid_until_ms: None,
            }),
            Fault::None if accepts.url => {
                self.inner
                    .deliver(
                        reference,
                        &ProviderAccepts {
                            bytes: true,
                            ..ProviderAccepts::NONE
                        },
                        limits,
                    )
                    .await?;
                Ok(Delivery::Url {
                    url: DeliverySecret::new("https://immutable.invalid/content".into()),
                    valid_until_ms: None,
                })
            }
            Fault::None => self.inner.deliver(reference, accepts, limits).await,
        }
    }
}
/// A byte-only acceptance forces bounded bytes, deduplicated by content and
/// effective acceptance; encoded work is charged for every occurrence.
#[expect(clippy::expect_used, reason = "conformance fixture setup must succeed")]
pub async fn attachment_delivery_read_budgets(backend: Arc<dyn AttachmentStore>) {
    let first = backend
        .put(vec![1; 4], meta())
        .await
        .expect("put legal blob");
    let second = backend
        .put(vec![2; 4], meta())
        .await
        .expect("put second blob");
    let mut large = backend
        .put(vec![3; 5], meta())
        .await
        .expect("put oversized blob");
    large.byte_len = 0;
    let accepts = ProviderAccepts {
        bytes: true,
        ..ProviderAccepts::NONE
    };
    let ctx = DeliveryContext {
        valid_through_ms: 1000,
        live_file_scope: None,
    };
    let probe = Arc::new(DeliveryProbe {
        inner: backend.clone(),
        calls: AtomicUsize::new(0),
        fault: Fault::None,
    });
    let store =
        RuntimeAttachmentStore::ephemeral(probe.clone()).with_read_policy(AttachmentReadPolicy {
            max_blob_bytes: 4,
            max_request_bytes: 8192,
        });
    assert!(matches!(
        backend.get(&large.id, 4).await,
        Err(AttachmentStoreError::ReadLimitExceeded { .. })
    ));
    assert!(
        store.read(&large).await.is_err(),
        "explicit reads cannot trust understated length"
    );
    let mut understated = first.clone();
    understated.byte_len = 0;
    assert!(
        store.read(&understated).await.is_err(),
        "explicit reads check the reference claim"
    );
    let too_large = slot(large, accepts.clone());
    assert!(store.deliver(&[&too_large], &ctx).await.is_err());
    let first = slot(first, accepts.clone());
    let second = slot(second, accepts);
    let bounded = store.reconfigured_read_policy(AttachmentReadPolicy {
        max_blob_bytes: 4,
        max_request_bytes: 2548,
    });
    assert!(bounded.deliver(&[&first, &second], &ctx).await.is_err());
    let exact = store.reconfigured_read_policy(AttachmentReadPolicy {
        max_blob_bytes: 4,
        max_request_bytes: 4 + 2 * (32 + 1024 + 24 * 9),
    });
    let before = probe.calls.load(Ordering::SeqCst);
    let values = exact
        .deliver(&[&first, &first], &ctx)
        .await
        .expect("one buffer and two encodings fit exactly");
    assert_eq!(probe.calls.load(Ordering::SeqCst) - before, 1);
    assert!(Arc::ptr_eq(&values[0], &values[1]));
    assert!(matches!(&*values[0], Delivery::Bytes(bytes) if bytes == &[1;4]));
    let no_room = store.reconfigured_read_policy(AttachmentReadPolicy {
        max_blob_bytes: 4,
        max_request_bytes: 1,
    });
    let before = probe.calls.load(Ordering::SeqCst);
    assert!(no_room.deliver(&[&first], &ctx).await.is_err());
    assert_eq!(
        probe.calls.load(Ordering::SeqCst),
        before,
        "reserve envelopes before backend work"
    );
    for fault in [
        Fault::Length,
        Fault::Digest,
        Fault::Capacity,
        Fault::Secret,
        Fault::Expired,
        Fault::Unaccepted,
    ] {
        let probe = Arc::new(DeliveryProbe {
            inner: backend.clone(),
            calls: AtomicUsize::new(0),
            fault,
        });
        let store =
            RuntimeAttachmentStore::ephemeral(probe).with_read_policy(AttachmentReadPolicy {
                max_blob_bytes: 4,
                max_request_bytes: 8192,
            });
        let accepts = ProviderAccepts {
            bytes: true,
            url: !matches!(fault, Fault::Unaccepted),
            provider_file: None,
        };
        let tested = slot(first.reference.clone(), accepts);
        assert!(
            store.deliver(&[&tested], &ctx).await.is_err(),
            "bad content or delivery must refuse before send"
        );
    }
    let scope = ProviderFileScope {
        provider: "fixture".into(),
        endpoint: "https://files.invalid".into(),
        credential_scope: "account".into(),
    };
    let uploader = Arc::new(FileProbe {
        scope: scope.clone(),
        uploads: AtomicUsize::new(0),
        failure: None,
        valid_until_ms: None,
    });
    let files = Arc::new(ProviderFileDelivery::new(
        backend.clone(),
        vec![uploader.clone()],
        Default::default(),
    ));
    let store = RuntimeAttachmentStore::ephemeral(files).with_read_policy(AttachmentReadPolicy {
        max_blob_bytes: 4,
        max_request_bytes: 8192,
    });
    let file_slot = slot(
        first.reference.clone(),
        ProviderAccepts {
            bytes: false,
            url: false,
            provider_file: Some(scope.clone()),
        },
    );
    let scoped = DeliveryContext {
        live_file_scope: Some(scope),
        ..ctx
    };
    assert!(
        store.deliver(&[&file_slot], &scoped).await.is_err(),
        "provider file ids share the serialization bound"
    );
    assert_eq!(uploader.uploads.load(Ordering::SeqCst), 1);
    let too_small = store.reconfigured_read_policy(AttachmentReadPolicy {
        max_blob_bytes: 3,
        max_request_bytes: 8192,
    });
    assert!(
        too_small.deliver(&[&file_slot], &scoped).await.is_err(),
        "a cached file cannot bypass the upload scratch reservation"
    );
    for class in [
        AttachmentStoreFailureClass::Transient,
        AttachmentStoreFailureClass::Terminal,
        AttachmentStoreFailureClass::Credentials,
    ] {
        let uploader = Arc::new(FileProbe {
            scope: scoped.live_file_scope.clone().expect("live scope"),
            uploads: AtomicUsize::new(0),
            failure: Some(class),
            valid_until_ms: None,
        });
        let files = Arc::new(ProviderFileDelivery::new(
            backend.clone(),
            vec![uploader],
            Default::default(),
        ));
        let store =
            RuntimeAttachmentStore::ephemeral(files).with_read_policy(AttachmentReadPolicy {
                max_blob_bytes: 4,
                max_request_bytes: 8192,
            });
        let failure = store
            .deliver(&[&file_slot], &scoped)
            .await
            .expect_err("file-only upload preserves its fault");
        assert!(
            matches!(&failure, AttachmentDeliveryError::Unavailable { retryable, .. } if *retryable == class.is_retryable())
        );
        assert!(!failure.to_string().contains("upload-secret-sentinel"));
        let mut fallback = file_slot.clone();
        fallback.accepts.bytes = true;
        let fallback = store.deliver(&[&fallback], &scoped).await;
        if class.is_operator_actionable() {
            assert!(fallback.is_err());
        } else {
            assert!(
                matches!(&*fallback.expect("non-auth upload falls back")[0], Delivery::Bytes(bytes) if bytes == &[1;4])
            );
        }
    }
    // An installed uploader is optional: a blob over the upload scratch
    // bound is never read for upload, and still delivers by the URL the
    // backend can mint without reading it.
    let scope = scoped.live_file_scope.clone().expect("live scope");
    let uploader = Arc::new(FileProbe {
        scope: scope.clone(),
        uploads: AtomicUsize::new(0),
        failure: None,
        valid_until_ms: None,
    });
    let files = ProviderFileDelivery::new(
        Arc::new(DeliveryProbe {
            inner: backend.clone(),
            calls: AtomicUsize::new(0),
            fault: Fault::Unaccepted,
        }),
        vec![uploader.clone()],
        Default::default(),
    );
    let over_scratch = DeliveryLimits {
        max_bytes: 3,
        valid_through_ms: scoped.valid_through_ms,
    };
    let url_or_file = ProviderAccepts {
        bytes: false,
        url: true,
        provider_file: Some(scope.clone()),
    };
    assert!(matches!(
        files
            .deliver(&first.reference, &url_or_file, &over_scratch)
            .await,
        Ok(Delivery::Url { .. })
    ));
    assert_eq!(uploader.uploads.load(Ordering::SeqCst), 0);
    // A file that cannot outlive the call is a typed terminal upload
    // failure, never a silent change of form.
    let short_lived = ProviderFileDelivery::new(
        backend.clone(),
        vec![Arc::new(FileProbe {
            scope: scope.clone(),
            uploads: AtomicUsize::new(0),
            failure: None,
            valid_until_ms: Some(1),
        })],
        Default::default(),
    );
    let fits = DeliveryLimits {
        max_bytes: 4,
        valid_through_ms: u64::MAX / 2,
    };
    for bytes in [false, true] {
        let accepts = ProviderAccepts {
            bytes,
            url: false,
            provider_file: Some(scope.clone()),
        };
        assert!(matches!(
            short_lived.deliver(&first.reference, &accepts, &fits).await,
            Err(AttachmentStoreError::Backend {
                operation: "upload",
                class: AttachmentStoreFailureClass::Terminal,
                ..
            })
        ));
    }
    assert_eq!(
        AttachmentReadPolicy::DEFAULT.max_blob_bytes,
        32 * 1024 * 1024
    );
    assert_eq!(
        AttachmentReadPolicy::DEFAULT.max_request_bytes,
        128 * 1024 * 1024
    );
}
