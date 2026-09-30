//! Cross-session stored references acquire receiver roots in the boundary commit.
//! Root reconciliation is the layer-1 operation also used by terminal evidence
//! reclamation, so this witness can run at every head in the stack.
use super::attachment_referrers::{claim, permit, write};
use lash_core::facade_support::{SessionAttachmentStore, reclaim_unreferenced_attachments};
use lash_core::testing::store_fixtures::session_store_request;
use lash_core::*;
use pretty_assertions::assert_eq;
use std::future::Future;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

/// Builds a fresh, empty attachment byte store, supplied by the tier: the
/// root-set laws hold a factory's roots against the bytes of one store each,
/// so every law and sub-law starts from no blobs at all.
pub type AttachmentBytesFactory = Arc<dyn Fn() -> Arc<dyn AttachmentStore> + Send + Sync>;

pub(super) struct FaultingAttachmentStore {
    inner: Arc<dyn AttachmentStore>,
    fail_put: AtomicBool,
    fail_delete: AtomicBool,
}

impl FaultingAttachmentStore {
    pub(super) fn over(inner: Arc<dyn AttachmentStore>) -> Self {
        Self {
            inner,
            fail_put: AtomicBool::new(false),
            fail_delete: AtomicBool::new(false),
        }
    }

    pub(super) fn fail_put(&self, fail: bool) {
        self.fail_put.store(fail, Ordering::SeqCst);
    }

    pub(super) fn fail_delete(&self, fail: bool) {
        self.fail_delete.store(fail, Ordering::SeqCst);
    }
}

#[async_trait::async_trait]
impl AttachmentStore for FaultingAttachmentStore {
    async fn put(
        &self,
        bytes: Vec<u8>,
        meta: AttachmentCreateMeta,
    ) -> Result<AttachmentRef, AttachmentStoreError> {
        if self.fail_put.load(Ordering::SeqCst) {
            return Err(AttachmentStoreError::Backend {
                operation: "put",
                class: AttachmentStoreFailureClass::Transient,
                source: "scripted attachment put failure".into(),
            });
        }
        self.inner.put(bytes, meta).await
    }

    async fn get(&self, id: &AttachmentId) -> Result<StoredAttachment, AttachmentStoreError> {
        self.inner.get(id).await
    }

    async fn delete(&self, id: &AttachmentId) -> Result<(), AttachmentStoreError> {
        if self.fail_delete.load(Ordering::SeqCst) {
            return Err(AttachmentStoreError::Backend {
                operation: "delete",
                class: AttachmentStoreFailureClass::Transient,
                source: "scripted attachment delete failure".into(),
            });
        }
        self.inner.delete(id).await
    }

    async fn list(&self) -> Result<Vec<StoredBlobRef>, AttachmentStoreError> {
        self.inner.list().await
    }

    async fn head(&self, id: &AttachmentId) -> Result<Option<StoredBlobRef>, AttachmentStoreError> {
        self.inner.head(id).await
    }
}

pub(super) async fn create(f: &Arc<dyn DeploymentStore>, id: &str) -> Arc<dyn RuntimeStore> {
    f.admit_session(&session_store_request(
        &SessionId::from(id),
        "probe",
        SessionRelation::Root,
    ))
    .await
    .unwrap();
    Arc::clone(f) as Arc<dyn RuntimeStore>
}

#[expect(
    clippy::expect_used,
    clippy::unwrap_used,
    reason = "conformance fixtures establish the setup"
)]
pub(super) async fn open_pass(f: &Arc<dyn DeploymentStore>) -> AttachmentSweepGeneration {
    f.begin_attachment_sweep().await.unwrap()
}
#[expect(clippy::unwrap_used, reason = "fixture media type is valid")]
pub(super) fn image_meta() -> AttachmentCreateMeta {
    AttachmentCreateMeta::new(MediaType::parse("image/png").unwrap(), None, None)
}
#[expect(
    clippy::expect_used,
    reason = "conformance fixture records a successful upload"
)]
pub(super) async fn record_completed_write(store: &Arc<dyn RuntimeStore>, write: &AttachmentWrite) {
    let permit = permit(store.as_ref(), write).await;
    store
        .complete_attachment_write(write, permit)
        .await
        .expect("complete upload");
}
#[expect(
    clippy::unwrap_used,
    reason = "fixture claims are unguarded session referrers"
)]
pub(super) fn session_write(session: &SessionId, id: &AttachmentId) -> AttachmentWrite {
    write(id, ArtifactReferrer::Session(session.clone()))
}

#[expect(
    clippy::expect_used,
    clippy::unwrap_used,
    reason = "conformance fixtures establish each result"
)]
pub async fn abandoned_attachment_write_recovery_after_cold_reopen<R, Fut>(
    initial_factory: Arc<dyn DeploymentStore>,
    _make_bytes: AttachmentBytesFactory,
    reopen: R,
) where
    R: FnOnce() -> Fut,
    Fut: Future<Output = Arc<dyn DeploymentStore>>,
{
    let store = create(&initial_factory, "cold-write").await;
    let id = AttachmentId::parse("cold-restoring-write").unwrap();
    let pass = open_pass(&initial_factory).await;
    assert_eq!(
        initial_factory
            .condemn_attachment(&id, &pass)
            .await
            .unwrap(),
        AttachmentCondemnation::Condemned
    );
    let attempt = write(
        &id,
        ArtifactReferrer::ProcessRecord(ProcessId::fixture("cold-writer")),
    );
    let pending = permit(store.as_ref(), &attempt).await;
    drop(store);
    drop(pass);
    drop(initial_factory);
    let reopened = reopen().await;
    reopened
        .recover_abandoned_attachment_write(&id)
        .await
        .unwrap();
    assert!(reopened.attachment_referrers(&id).await.unwrap().is_empty());
    assert!(matches!(
        reopened.complete_attachment_write(&attempt, pending).await,
        Err(StoreError::StaleWritePermit { .. })
    ));
    let pass = open_pass(&reopened).await;
    let adopted = reopened
        .adopt_attachment_condemnations(&pass)
        .await
        .unwrap();
    assert_eq!(adopted.adopted.len(), 1);
    assert_eq!(
        reopened.arm_attachment_delete(&id, &pass).await.unwrap(),
        AttachmentDeleteArming::Armed
    );
}

#[expect(
    clippy::expect_used,
    clippy::unwrap_used,
    reason = "conformance fixtures establish each result"
)]
pub async fn cross_session_attachment_adoption_conformance(
    f: Arc<dyn DeploymentStore>,
    make_bytes: AttachmentBytesFactory,
) {
    let store = create(&f, "reader-a").await;
    create(&f, "reader-b").await;
    let bytes = make_bytes();
    let faulting = Arc::new(FaultingAttachmentStore::over(bytes.clone()));
    let facade = SessionAttachmentStore::new(
        faulting.clone(),
        store.clone(),
        RuntimeOwner::Process(ProcessId::fixture("byte-writer")),
    );
    faulting.fail_put(true);
    let failed = facade.put(vec![8], image_meta()).await.unwrap_err();
    assert!(failed.is_retryable());
    let id = lash_core::attachments::content_id(&[8]);
    assert!(store.attachment_referrers(&id).await.unwrap().is_empty());
    let pass = open_pass(&f).await;
    assert_eq!(
        f.condemn_attachment(&id, &pass).await.unwrap(),
        AttachmentCondemnation::Condemned
    );
    faulting.fail_put(false);
    let reference = facade.put(vec![8], image_meta()).await.unwrap();
    for session in ["reader-a", "reader-b"] {
        store
            .acquire_attachment_refs(
                &claim(ArtifactReferrer::Session(session.into())),
                std::slice::from_ref(&reference.id),
            )
            .await
            .unwrap();
    }
    store
        .end_attachment_referrer(&ArtifactReferrer::ProcessRecord(ProcessId::fixture(
            "byte-writer",
        )))
        .await
        .unwrap();
    store
        .end_attachment_referrer(&ArtifactReferrer::Session("reader-a".into()))
        .await
        .unwrap();
    assert_eq!(
        store.attachment_referrers(&reference.id).await.unwrap(),
        vec![ArtifactReferrer::Session("reader-b".into())]
    );
    let report = reclaim_unreferenced_attachments(
        f.as_ref(),
        bytes.as_ref(),
        AttachmentReclamationPolicy {
            grace_period_ms: 0,
            empty_root_set: EmptyRootSetPolicy::AuthorizeDeleteAll,
        },
    )
    .await
    .unwrap();
    assert_eq!(report.reclaimed_count, 0);
    assert_eq!(bytes.get(&reference.id).await.unwrap().bytes, vec![8]);
    store
        .end_attachment_referrer(&ArtifactReferrer::Session("reader-b".into()))
        .await
        .unwrap();
    let report = reclaim_unreferenced_attachments(
        f.as_ref(),
        bytes.as_ref(),
        AttachmentReclamationPolicy {
            grace_period_ms: 0,
            empty_root_set: EmptyRootSetPolicy::AuthorizeDeleteAll,
        },
    )
    .await
    .unwrap();
    assert_eq!(report.reclaimed_count, 1);
}

#[expect(
    clippy::expect_used,
    clippy::unwrap_used,
    reason = "conformance fixtures establish each result"
)]
pub async fn attachment_condemnation_enumeration_conformance(f: Arc<dyn DeploymentStore>) {
    let store = create(&f, "enumeration").await;
    let id = AttachmentId::parse("enumerated-condemnation").unwrap();
    let pass = open_pass(&f).await;
    f.condemn_attachment(&id, &pass).await.unwrap();
    let rows = f.list_condemnations().await.unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].digest, id);
    assert_eq!(rows[0].phase, AttachmentCondemnationPhase::Condemned);
    let attempt = write(
        &id,
        ArtifactReferrer::ProcessRecord(ProcessId::fixture("enumerated-writer")),
    );
    let token = permit(store.as_ref(), &attempt).await;
    assert!(
        matches!(&f.list_condemnations().await.unwrap()[0].provenance, AttachmentCondemnationProvenance::RestoringWrite { referrer } if referrer == attempt.claim.referrer())
    );
    store.abort_attachment_write(&attempt, token).await.unwrap();
    assert!(matches!(
        f.list_condemnations().await.unwrap()[0].provenance,
        AttachmentCondemnationProvenance::SweepOwned
    ));
}
