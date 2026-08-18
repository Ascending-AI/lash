//! An `AttachmentStore` that does not state a freshness answer must not compile.
//!
//! `head` is the GC's delete-time freshness re-check. A silently inherited
//! implementation is a behaviour the implementor never chose, so the method is
//! required: omitting it is a compile error, not a default.

use lash::attachments::{AttachmentCreateMeta, AttachmentId, AttachmentRef};
use lash::persistence::{AttachmentStore, AttachmentStoreError, StoredAttachment, StoredBlobRef};

struct HeadlessStore;

#[async_trait::async_trait]
impl AttachmentStore for HeadlessStore {
    async fn put(
        &self,
        _bytes: Vec<u8>,
        _meta: AttachmentCreateMeta,
    ) -> Result<AttachmentRef, AttachmentStoreError> {
        Err(AttachmentStoreError::NotFound(AttachmentId::from("absent")))
    }

    async fn get(&self, _id: &AttachmentId) -> Result<StoredAttachment, AttachmentStoreError> {
        Err(AttachmentStoreError::NotFound(AttachmentId::from("absent")))
    }

    async fn delete(&self, _id: &AttachmentId) -> Result<(), AttachmentStoreError> {
        Ok(())
    }

    async fn list(&self) -> Result<Vec<StoredBlobRef>, AttachmentStoreError> {
        Ok(Vec::new())
    }
}

fn main() {}
