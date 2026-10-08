//! Derivative provider-file reuse over immutable original attachment bytes.
use super::{
    AttachmentStore, AttachmentStoreError, AttachmentStorePersistence, StoredAttachment,
    StoredBlobRef, validate_attachment_bytes,
};
use lash_sansio::llm::attachment_delivery::{
    Delivery, DeliveryLimits, DeliverySecret, ProviderAccepts, ProviderFileScope,
};
use lash_sansio::sync::MutexExt;
use lash_sansio::{AttachmentCreateMeta, AttachmentId, AttachmentRef, MediaType};
use std::sync::{Arc, Mutex};

#[async_trait::async_trait]
pub trait ProviderFileUploader: Send + Sync {
    fn scope(&self) -> &ProviderFileScope;
    async fn upload(
        &self,
        reference: &AttachmentRef,
        bytes: &[u8],
    ) -> Result<UploadedProviderFile, AttachmentStoreError>;
}
#[derive(Debug)]
pub struct UploadedProviderFile {
    pub id: DeliverySecret,
    pub valid_until_ms: Option<u64>,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ProviderFileCacheLimits {
    pub capacity: usize,
    pub ttl_ms: u64,
}
impl Default for ProviderFileCacheLimits {
    fn default() -> Self {
        Self {
            capacity: 1024,
            ttl_ms: 86_400_000,
        }
    }
}
struct CacheEntry {
    reference: AttachmentId,
    media_type: MediaType,
    byte_len: u64,
    scope: ProviderFileScope,
    id: DeliverySecret,
    valid_until_ms: u64,
}
/// An optional host component. Cache misses may upload again after a crash;
/// this cache never holds a referrer or removes original content.
pub struct ProviderFileDelivery {
    inner: Arc<dyn AttachmentStore>,
    uploaders: Vec<Arc<dyn ProviderFileUploader>>,
    limits: ProviderFileCacheLimits,
    cache: Mutex<Vec<CacheEntry>>,
}
impl ProviderFileDelivery {
    pub fn new(
        inner: Arc<dyn AttachmentStore>,
        uploaders: Vec<Arc<dyn ProviderFileUploader>>,
        cache: ProviderFileCacheLimits,
    ) -> Self {
        Self {
            inner,
            uploaders,
            limits: cache,
            cache: Mutex::new(Vec::new()),
        }
    }
    fn cached(
        &self,
        reference: &AttachmentRef,
        scope: &ProviderFileScope,
        horizon: u64,
    ) -> Result<Option<Delivery>, AttachmentStoreError> {
        let cache = self.cache.lock_recover();
        let Some(entry) = cache.iter().find(|entry| {
            entry.reference == reference.id
                && entry.media_type == reference.media_type
                && &entry.scope == scope
                && entry.valid_until_ms >= horizon
                && entry.valid_until_ms > super::now_epoch_ms()
        }) else {
            return Ok(None);
        };
        if entry.byte_len != reference.byte_len {
            return Err(AttachmentStoreError::ContentMismatch {
                id: reference.id.clone(),
                detail: super::ContentMismatchDetail::Length {
                    expected: reference.byte_len,
                    actual: entry.byte_len,
                },
            });
        }
        Ok(Some(Delivery::ProviderFile {
            scope: scope.clone(),
            id: DeliverySecret::new(entry.id.expose().to_owned()),
            valid_until_ms: Some(entry.valid_until_ms),
        }))
    }
}
#[async_trait::async_trait]
impl AttachmentStore for ProviderFileDelivery {
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
        max_bytes: u64,
    ) -> Result<StoredAttachment, AttachmentStoreError> {
        self.inner.get(id, max_bytes).await
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
        if let Some(scope) = &accepts.provider_file
            && let Some(uploader) = self
                .uploaders
                .iter()
                .find(|uploader| uploader.scope() == scope)
        {
            // A cache hit still needs an extant original: it cannot resurrect
            // content reclaimed after its final Lash referrer ended.
            if self.inner.head(&reference.id).await?.is_none() {
                return Err(AttachmentStoreError::NotFound(reference.id.clone()));
            }
            if let Some(delivery) = self.cached(reference, scope, limits.valid_through_ms)? {
                return Ok(delivery);
            }
            let read_limit = limits.max_bytes.min(reference.byte_len);
            let stored = self.inner.get(&reference.id, read_limit).await?;
            validate_attachment_bytes(
                reference,
                &stored.bytes,
                stored.bytes.capacity() as u64,
                read_limit,
            )?;
            match uploader.upload(reference, &stored.bytes).await {
                Ok(file) => {
                    let now = super::now_epoch_ms();
                    let expiry = file
                        .valid_until_ms
                        .unwrap_or(u64::MAX)
                        .min(now.saturating_add(self.limits.ttl_ms));
                    if expiry >= limits.valid_through_ms && expiry > now {
                        if self.limits.capacity > 0 {
                            let mut cache = self.cache.lock_recover();
                            cache.retain(|entry| {
                                entry.valid_until_ms > now
                                    && !(entry.reference == reference.id
                                        && entry.media_type == reference.media_type
                                        && &entry.scope == scope)
                            });
                            if cache.len() >= self.limits.capacity {
                                cache.remove(0);
                            }
                            cache.push(CacheEntry {
                                reference: reference.id.clone(),
                                media_type: reference.media_type.clone(),
                                byte_len: reference.byte_len,
                                scope: scope.clone(),
                                id: DeliverySecret::new(file.id.expose().to_owned()),
                                valid_until_ms: expiry,
                            });
                        }
                        return Ok(Delivery::ProviderFile {
                            scope: scope.clone(),
                            id: file.id,
                            valid_until_ms: Some(expiry),
                        });
                    }
                }
                Err(error) if error.is_operator_actionable() => {
                    // Uploader diagnostics can contain a remote id or URL.
                    return Err(AttachmentStoreError::Backend {
                        operation: "upload",
                        class: super::AttachmentStoreFailureClass::Credentials,
                        source: "provider-file upload authorization failed".into(),
                    });
                }
                Err(error) if !accepts.bytes && !accepts.url => {
                    return Err(redacted_upload_error(error));
                }
                Err(_) => {}
            }
        }
        self.inner
            .deliver(
                reference,
                &ProviderAccepts {
                    bytes: accepts.bytes,
                    url: accepts.url,
                    provider_file: None,
                },
                limits,
            )
            .await
    }
    async fn invalidate_delivery(
        &self,
        reference: &AttachmentRef,
        rejected: &Delivery,
    ) -> Result<(), AttachmentStoreError> {
        if let Delivery::ProviderFile { scope, id, .. } = rejected {
            self.cache.lock_recover().retain(|entry| {
                !(entry.reference == reference.id
                    && entry.media_type == reference.media_type
                    && &entry.scope == scope
                    && entry.id.expose() == id.expose())
            });
        }
        self.inner.invalidate_delivery(reference, rejected).await
    }
}

/// Upload services may include live delivery material in their source text.
/// Preserve safe typed refusals and rebuild backend failures from their class.
fn redacted_upload_error(error: AttachmentStoreError) -> AttachmentStoreError {
    let class = error.failure_class().unwrap_or_else(|| {
        if error.is_retryable() {
            super::AttachmentStoreFailureClass::Transient
        } else {
            super::AttachmentStoreFailureClass::Terminal
        }
    });
    match error {
        safe @ (AttachmentStoreError::NotFound(_)
        | AttachmentStoreError::DeliveryUnsupported { .. }
        | AttachmentStoreError::ContentMismatch { .. }
        | AttachmentStoreError::SizeLimitExceeded { .. }
        | AttachmentStoreError::ReadLimitExceeded { .. }
        | AttachmentStoreError::RequestBudgetExceeded { .. }
        | AttachmentStoreError::ReclamationInFlight { .. }) => safe,
        _ => AttachmentStoreError::Backend {
            operation: "upload",
            class,
            source: "provider-file upload failed".into(),
        },
    }
}
