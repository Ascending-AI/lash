use lash_core_execution::attachments::*;
use lash_core_execution::*;
use lash_sansio::MediaType;

struct UnsupportedAttachmentRoots;

#[async_trait::async_trait]
impl AttachmentRootSet for UnsupportedAttachmentRoots {
    async fn live_attachment_refs(
        &self,
    ) -> Result<
        lash_core_execution::attachments::CompleteAttachmentRoots,
        lash_core_execution::StoreError,
    > {
        Err(lash_core_execution::StoreError::UnsupportedStoreOperation {
            operation: "live_attachment_refs",
        })
    }

    async fn has_live_attachment_ref(
        &self,
        _id: &AttachmentId,
    ) -> Result<bool, lash_core_execution::StoreError> {
        Err(lash_core_execution::StoreError::UnsupportedStoreOperation {
            operation: "has_live_attachment_ref",
        })
    }
}

#[tokio::test]
async fn unsupported_root_enumeration_aborts_sweep_and_preserves_blob() {
    let temp = tempfile::tempdir().expect("tempdir");
    let backend = lash_sqlite_store::SqliteStoreSet::open(
        (temp.path()).join("attachments.db"),
        lash_sqlite_store::SqliteSynchronous::Normal,
    )
    .await
    .expect("SQLite attachment store")
    .attachment_store();
    let reference = backend
        .put(
            b"fail-closed-live-blob".to_vec(),
            AttachmentCreateMeta::new(MediaType::parse("image/png").unwrap(), None, None),
        )
        .await
        .expect("put attachment");

    let error = reclaim_unreferenced_attachments(
        &UnsupportedAttachmentRoots,
        backend.as_ref(),
        AttachmentReclamationPolicy::new(0, EmptyRootSetPolicy::Refuse),
    )
    .await
    .expect_err("unsupported root enumeration must abort the sweep");

    assert!(matches!(
        &error.stop,
        lash_core_execution::store::MaintenanceStop::Failed(
            AttachmentStoreError::RootSetEnumerationFailed { source }
        ) if source.to_string().contains("live_attachment_refs")
    ));
    backend
        .get(&reference.id, 32 * 1024 * 1024)
        .await
        .expect("aborted sweep leaves blob intact");
}
