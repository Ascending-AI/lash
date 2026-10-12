use std::sync::Arc;

use crate::attachments::AttachmentPutReceipt;
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

    /// Store attachment bytes and report their independent byte-delivery budget.
    ///
    /// A successful put may exceed the delivery budget; the receipt advises the
    /// tool without changing put admission. URL delivery has no blob-byte cap.
    ///
    /// # Integrator class
    ///
    /// Tool implementors use this capability to publish attachment bytes and
    /// metadata without depending on the runtime's storage implementation.
    pub async fn put(
        &self,
        data: Vec<u8>,
        meta: AttachmentCreateMeta,
    ) -> Result<AttachmentPutReceipt, AttachmentStoreError> {
        self.store.put_receipt(data, meta).await
    }
}
