use lash_core_execution::*;
use lash_core_store::attachments::*;
use lash_protocol_types::MediaType;

struct UnsupportedAttachmentRoots;

#[async_trait::async_trait]
impl AttachmentRootSet for UnsupportedAttachmentRoots {
    async fn begin_attachment_sweep(
        &self,
    ) -> Result<lash_core_execution::AttachmentSweepGeneration, lash_core_execution::StoreError>
    {
        Err(lash_core_execution::StoreError::UnsupportedStoreOperation {
            operation: "begin_attachment_sweep",
        })
    }
    async fn adopt_attachment_condemnations(
        &self,
        _: &lash_core_execution::AttachmentSweepGeneration,
    ) -> Result<lash_core_execution::AttachmentCondemnationAdoption, lash_core_execution::StoreError>
    {
        Err(lash_core_execution::StoreError::UnsupportedStoreOperation {
            operation: "adopt_attachment_condemnations",
        })
    }
    async fn condemn_attachment(
        &self,
        _: &AttachmentId,
        _: &lash_core_execution::AttachmentSweepGeneration,
    ) -> Result<lash_core_execution::AttachmentCondemnation, lash_core_execution::StoreError> {
        Err(lash_core_execution::StoreError::UnsupportedStoreOperation {
            operation: "condemn_attachment",
        })
    }
    async fn arm_attachment_delete(
        &self,
        _: &AttachmentId,
        _: &lash_core_execution::AttachmentSweepGeneration,
    ) -> Result<lash_core_execution::AttachmentDeleteArming, lash_core_execution::StoreError> {
        Err(lash_core_execution::StoreError::UnsupportedStoreOperation {
            operation: "arm_attachment_delete",
        })
    }
    async fn settle_attachment_condemnation(
        &self,
        _: &AttachmentId,
        _: &lash_core_execution::AttachmentSweepGeneration,
        _: lash_core_execution::AttachmentCondemnationSettlement,
    ) -> Result<lash_core_execution::AttachmentSettlementOutcome, lash_core_execution::StoreError>
    {
        Err(lash_core_execution::StoreError::UnsupportedStoreOperation {
            operation: "settle_attachment_condemnation",
        })
    }
    async fn recover_abandoned_attachment_write(
        &self,
        _: &AttachmentId,
    ) -> Result<(), lash_core_execution::StoreError> {
        Err(lash_core_execution::StoreError::UnsupportedStoreOperation {
            operation: "recover_abandoned_attachment_write",
        })
    }

    async fn live_attachment_refs(
        &self,
    ) -> Result<
        lash_core_store::attachments::CompleteAttachmentRoots,
        lash_core_store::store::StoreError,
    > {
        Err(
            lash_core_store::store::StoreError::UnsupportedStoreOperation {
                operation: "live_attachment_refs",
            },
        )
    }

    async fn has_live_attachment_ref(
        &self,
        _id: &AttachmentId,
    ) -> Result<bool, lash_core_store::store::StoreError> {
        Err(
            lash_core_store::store::StoreError::UnsupportedStoreOperation {
                operation: "has_live_attachment_ref",
            },
        )
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
        lash_core_store::store::MaintenanceStop::Failed(
            AttachmentStoreError::RootSetEnumerationFailed { source }
        ) if source.to_string().contains("live_attachment_refs")
    ));
    backend
        .get(&reference.id, 32 * 1024 * 1024)
        .await
        .expect("aborted sweep leaves blob intact");
}
