use super::*;
use lash_sansio::MediaType;

struct UnsupportedAttachmentRoots;

#[async_trait::async_trait]
impl AttachmentRootSet for UnsupportedAttachmentRoots {
    async fn live_attachment_refs(
        &self,
        _intent_grace_cutoff_epoch_ms: u64,
    ) -> Result<BTreeSet<AttachmentId>, crate::StoreError> {
        Err(crate::StoreError::UnsupportedStoreOperation {
            operation: "live_attachment_refs",
        })
    }

    async fn has_live_attachment_ref(
        &self,
        _id: &AttachmentId,
        _intent_grace_cutoff_epoch_ms: u64,
    ) -> Result<bool, crate::StoreError> {
        Err(crate::StoreError::UnsupportedStoreOperation {
            operation: "has_live_attachment_ref",
        })
    }
}

#[tokio::test]
async fn unsupported_root_enumeration_aborts_sweep_and_preserves_blob() {
    let backend = InMemoryAttachmentStore::new();
    let reference = backend
        .put(
            b"fail-closed-live-blob".to_vec(),
            AttachmentCreateMeta::new(MediaType::parse("image/png").unwrap(), None, None),
        )
        .await
        .expect("put attachment");

    let error = reclaim_unreferenced_attachments(&UnsupportedAttachmentRoots, &backend, 0)
        .await
        .expect_err("unsupported root enumeration must abort the sweep");

    assert!(matches!(
        error,
        AttachmentStoreError::Backend(message)
            if message.contains("failed to enumerate live attachment refs")
                && message.contains("live_attachment_refs")
    ));
    backend
        .get(&reference.id)
        .await
        .expect("aborted sweep leaves blob intact");
}
