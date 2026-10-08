use std::sync::Arc;

use crate::{AttachmentCreateMeta, AttachmentRef, AttachmentStoreError};

#[derive(Clone)]
pub struct ToolAttachmentClient {
    pub(super) store: Arc<crate::RuntimeAttachmentStore>,
}

impl ToolAttachmentClient {
    /// Read stored bytes explicitly, subject to the host's attachment read policy.
    ///
    /// # Integrator class
    ///
    /// Tool implementors use this capability to resolve retained history values.
    pub async fn read(&self, reference: &AttachmentRef) -> Result<Vec<u8>, AttachmentStoreError> {
        self.store.read(reference).await
    }

    /// # Integrator class
    ///
    /// Tool implementors use this capability to publish attachment bytes and
    /// metadata without depending on the runtime's storage implementation.
    pub async fn put(
        &self,
        data: Vec<u8>,
        meta: AttachmentCreateMeta,
    ) -> Result<AttachmentRef, AttachmentStoreError> {
        self.store.put(data, meta).await
    }
}
