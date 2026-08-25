use std::sync::Arc;

use crate::{AttachmentCreateMeta, AttachmentRef, AttachmentStoreError};

#[derive(Clone)]
pub struct ToolAttachmentClient {
    pub(super) store: Arc<crate::SessionAttachmentStore>,
}

impl ToolAttachmentClient {
    /// Store one attachment through the session-bound attachment service.
    ///
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
