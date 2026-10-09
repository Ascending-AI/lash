//! The runbook has no attachment workload; its host refuses byte writes.

use lash::attachments::{AttachmentCreateMeta, AttachmentId, AttachmentRef};
use lash::persistence::{
    AttachmentStore, AttachmentStoreError, AttachmentStoreFailureClass, StoredAttachment,
    StoredBlobRef,
};

pub(crate) struct NoAttachments;

#[async_trait::async_trait]
impl AttachmentStore for NoAttachments {
    async fn put(
        &self,
        _bytes: Vec<u8>,
        _meta: AttachmentCreateMeta,
    ) -> Result<AttachmentRef, AttachmentStoreError> {
        Err(AttachmentStoreError::Backend {
            operation: "put",
            class: AttachmentStoreFailureClass::Terminal,
            source: "this context has no attachment port".into(),
        })
    }

    async fn get(
        &self,
        id: &AttachmentId,
        _max_bytes: u64,
    ) -> Result<StoredAttachment, AttachmentStoreError> {
        Err(AttachmentStoreError::NotFound(id.clone()))
    }

    async fn delete(&self, _id: &AttachmentId) -> Result<(), AttachmentStoreError> {
        Ok(())
    }

    async fn list(&self) -> Result<Vec<StoredBlobRef>, AttachmentStoreError> {
        Ok(Vec::new())
    }

    async fn head(
        &self,
        _id: &AttachmentId,
    ) -> Result<Option<StoredBlobRef>, AttachmentStoreError> {
        Ok(None)
    }
}
