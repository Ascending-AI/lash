//! Attachment-layer tests.
//!
//! The attachment layer moved to `lash-core-store`; these cases shift it
//! through a SQLite memory backend's session catalog and attachment store.

use crate::SessionId;
use lash_core_store::attachments::*;
use lash_sansio::sync::MutexExt;
use lash_sansio::{AttachmentCreateMeta, AttachmentId, AttachmentRef};
use std::collections::{BTreeSet, HashMap};
use std::sync::{Arc, Mutex};

fn now_epoch_ms() -> u64 {
    <crate::SystemClock as crate::ClockWallTime>::timestamp_ms(&crate::SystemClock)
}

use crate::store::{AttachmentReferrers, AttachmentWriteFence};
use crate::{ArtifactReferrer, AttachmentWrite, ReferrerClaim, RuntimeOwner};
use lash_sansio::{AttachmentTypeMetadata, MediaType};

fn session_owner(session_id: &str) -> RuntimeOwner {
    RuntimeOwner::Session(SessionId::fixture(session_id))
}

fn session_claim(session_id: &str) -> ReferrerClaim {
    ReferrerClaim::unguarded(ArtifactReferrer::Session(SessionId::fixture(session_id)))
        .expect("a session claim is unguarded")
}

/// In-memory referrer edges and upload evidence: each digest's live
/// referrers, a pending write's claim included.
#[derive(Default)]
struct RecordingReferrers {
    edges: Mutex<HashMap<AttachmentId, Vec<ArtifactReferrer>>>,
    evidence: Mutex<BTreeSet<AttachmentId>>,
}

fn hold(referrers: &mut Vec<ArtifactReferrer>, referrer: &ArtifactReferrer) {
    if !referrers.contains(referrer) {
        referrers.push(referrer.clone());
    }
}

impl RecordingReferrers {
    fn live_ids(&self) -> BTreeSet<AttachmentId> {
        self.edges
            .lock_recover()
            .iter()
            .filter(|(_, referrers)| !referrers.is_empty())
            .map(|(id, _)| id.clone())
            .collect()
    }

    /// Ends every upload edge, as the cleanup executor does once an upload's
    /// expiry passes.
    fn end_uploads(&self) {
        for referrers in self.edges.lock_recover().values_mut() {
            referrers.retain(|referrer| !matches!(referrer, ArtifactReferrer::Upload(_)));
        }
    }
}

#[async_trait::async_trait]
impl AttachmentReferrers for RecordingReferrers {
    async fn begin_attachment_write(
        &self,
        write: &AttachmentWrite,
    ) -> Result<crate::AttachmentWriteFence, crate::StoreError> {
        hold(
            self.edges
                .lock_recover()
                .entry(write.attachment_id.clone())
                .or_default(),
            &write.claim.referrer(),
        );
        Ok(crate::AttachmentWriteFence::Granted(
            crate::AttachmentWritePermit::new(crate::AttachmentWriteToken::new()),
        ))
    }

    async fn complete_attachment_write(
        &self,
        write: &AttachmentWrite,
        _permit: crate::AttachmentWritePermit,
    ) -> Result<(), crate::StoreError> {
        self.evidence
            .lock_recover()
            .insert(write.attachment_id.clone());
        Ok(())
    }

    async fn abort_attachment_write(
        &self,
        write: &AttachmentWrite,
        _permit: crate::AttachmentWritePermit,
    ) -> Result<(), crate::StoreError> {
        if let Some(referrers) = self.edges.lock_recover().get_mut(&write.attachment_id) {
            referrers.retain(|held| held != &write.claim.referrer());
        }
        Ok(())
    }

    async fn acquire_attachment_refs(
        &self,
        claim: &ReferrerClaim,
        attachment_ids: &[AttachmentId],
    ) -> Result<(), crate::StoreError> {
        let evidence = self.evidence.lock_recover();
        if let Some(digest) = attachment_ids.iter().find(|id| !evidence.contains(*id)) {
            return Err(crate::StoreError::UnknownAttachment {
                digest: digest.clone(),
            });
        }
        let mut edges = self.edges.lock_recover();
        for attachment_id in attachment_ids {
            hold(
                edges.entry(attachment_id.clone()).or_default(),
                &claim.referrer(),
            );
        }
        Ok(())
    }

    async fn forget_attachment_ref(
        &self,
        referrer: &ArtifactReferrer,
        attachment_id: &AttachmentId,
    ) -> Result<(), crate::StoreError> {
        if let Some(referrers) = self.edges.lock_recover().get_mut(attachment_id) {
            referrers.retain(|held| held != referrer);
        }
        Ok(())
    }

    async fn end_attachment_referrer(
        &self,
        referrer: &ArtifactReferrer,
    ) -> Result<(), crate::StoreError> {
        for referrers in self.edges.lock_recover().values_mut() {
            referrers.retain(|held| held != referrer);
        }
        Ok(())
    }

    async fn session_referrer_state(
        &self,
        _session_id: &SessionId,
    ) -> Result<crate::SessionReferrerState, crate::StoreError> {
        Ok(crate::SessionReferrerState::Live)
    }

    async fn attachment_referrers(
        &self,
        attachment_id: &AttachmentId,
    ) -> Result<Vec<ArtifactReferrer>, crate::StoreError> {
        Ok(self
            .edges
            .lock_recover()
            .get(attachment_id)
            .cloned()
            .unwrap_or_default())
    }
}

/// Root set backed by a set of [`RecordingReferrers`]: the in-memory analogue
/// of a durable core's attachment edges.
struct RecordingRootSet {
    manifests: Vec<Arc<RecordingReferrers>>,
}

struct UnavailableRootSet;

#[async_trait::async_trait]
impl AttachmentRootSet for UnavailableRootSet {
    async fn live_attachment_refs(
        &self,
    ) -> Result<crate::attachments::CompleteAttachmentRoots, crate::StoreError> {
        Err(crate::StoreError::Backend(
            "root enumeration unavailable".to_string(),
        ))
    }

    async fn has_live_attachment_ref(&self, _id: &AttachmentId) -> Result<bool, crate::StoreError> {
        Err(crate::StoreError::Backend(
            "targeted root probe unavailable".to_string(),
        ))
    }
}

#[async_trait::async_trait]
impl AttachmentRootSet for RecordingRootSet {
    async fn attachment_root_page(
        &self,
        source: crate::attachments::AttachmentRootSource,
        after: Option<&AttachmentId>,
    ) -> Result<crate::attachments::AttachmentRootPage, crate::StoreError> {
        use crate::attachments::{AttachmentRootPage, AttachmentRootSource};
        let roots: BTreeSet<_> = self
            .manifests
            .iter()
            .flat_map(|manifest| manifest.live_ids())
            .collect();
        let ids = if source == AttachmentRootSource::Referrer(crate::ArtifactReferrerKind::Session)
        {
            roots
                .into_iter()
                .filter(|id| after.is_none_or(|after| id > after))
                .take(AttachmentRootPage::QUERY_LIMIT)
                .collect()
        } else {
            Vec::new()
        };
        AttachmentRootPage::from_rows(ids)
    }

    async fn has_live_attachment_ref(&self, id: &AttachmentId) -> Result<bool, crate::StoreError> {
        Ok(self
            .manifests
            .iter()
            .any(|manifest| manifest.live_ids().contains(id)))
    }
}

fn meta() -> AttachmentCreateMeta {
    AttachmentCreateMeta::new(
        MediaType::parse("image/png").unwrap(),
        Some(AttachmentTypeMetadata::image(Some(1), Some(1))),
        Some("pixel".to_string()),
    )
}

async fn committed_factory_attachment() -> (
    Arc<dyn crate::DeploymentStore>,
    Arc<dyn AttachmentStore>,
    AttachmentId,
) {
    let substrate = crate::testing::sqlite_memory_store_set().await;
    let factory: Arc<dyn crate::DeploymentStore> = substrate.session_store_factory();
    let request = crate::SessionStoreCreateRequest {
        owning_process_id: None,
        pending_observer_intents: Vec::new(),
        session_id: SessionId::from("explicit-root-factory"),
        relation: crate::SessionRelation::Root,
        config: crate::PersistedSessionConfig::new(
            crate::TurnBudget::Unbounded,
            crate::MaxToolCalls::new(1024),
            crate::NoProgressBudget::bounded(12),
            crate::SessionToolAccess::ambient(),
        ),
        head: crate::SessionCreationHead::Config,
        retention: crate::Retention::UntilGc,
    };
    let store = crate::testing::runtime_helpers::create_session_store(&factory, &request)
        .await
        .expect("create attachment-aware store");
    let backend = substrate.attachment_store();
    let session = RuntimeAttachmentStore::new(
        backend.clone(),
        Arc::new(PersistenceReferrersAdapter(Arc::clone(store.store()))),
        RuntimeOwner::Session(request.session_id.clone()),
        AttachmentPolicy::standard(),
    );
    let reference = session
        .put(vec![8, 8, 1], meta())
        .await
        .expect("put factory attachment");
    store
        .store()
        .acquire_attachment_refs(
            &session_claim("explicit-root-factory"),
            std::slice::from_ref(&reference.id),
        )
        .await
        .expect("commit factory attachment ref");
    (factory, backend, reference.id)
}

/// Deliberately faulty snapshot projection over a factory that really does hold
/// a committed ref, and whose targeted probe misses too. Every read-shaped guard
/// is therefore blind; only the factory's condemn CAS — the authority the
/// writer's intent lives in — can still see the root.
struct EmptySnapshotFactoryRoots<'a> {
    factory: &'a dyn crate::DeploymentStore,
}

#[async_trait::async_trait]
impl AttachmentRootSet for EmptySnapshotFactoryRoots<'_> {
    async fn attachment_root_page(
        &self,
        _source: crate::attachments::AttachmentRootSource,
        _after: Option<&AttachmentId>,
    ) -> Result<crate::attachments::AttachmentRootPage, crate::StoreError> {
        crate::attachments::AttachmentRootPage::from_rows(Vec::new())
    }

    async fn has_live_attachment_ref(&self, _id: &AttachmentId) -> Result<bool, crate::StoreError> {
        Ok(false)
    }

    fn fence(&self) -> crate::AttachmentGcFence {
        AttachmentRootSet::fence(self.factory)
    }

    async fn begin_attachment_sweep(
        &self,
    ) -> Result<crate::AttachmentSweepGeneration, crate::StoreError> {
        AttachmentRootSet::begin_attachment_sweep(self.factory).await
    }

    async fn adopt_attachment_condemnations(
        &self,
        generation: &crate::AttachmentSweepGeneration,
    ) -> Result<crate::AttachmentCondemnationAdoption, crate::StoreError> {
        AttachmentRootSet::adopt_attachment_condemnations(self.factory, generation).await
    }

    async fn condemn_attachment(
        &self,
        id: &AttachmentId,
        generation: &crate::AttachmentSweepGeneration,
    ) -> Result<crate::AttachmentCondemnation, crate::StoreError> {
        AttachmentRootSet::condemn_attachment(self.factory, id, generation).await
    }

    async fn arm_attachment_delete(
        &self,
        id: &AttachmentId,
        generation: &crate::AttachmentSweepGeneration,
    ) -> Result<crate::AttachmentDeleteArming, crate::StoreError> {
        AttachmentRootSet::arm_attachment_delete(self.factory, id, generation).await
    }

    async fn settle_attachment_condemnation(
        &self,
        id: &AttachmentId,
        generation: &crate::AttachmentSweepGeneration,
        settlement: crate::AttachmentCondemnationSettlement,
    ) -> Result<crate::AttachmentSettlementOutcome, crate::StoreError> {
        AttachmentRootSet::settle_attachment_condemnation(self.factory, id, generation, settlement)
            .await
    }

    async fn recover_abandoned_attachment_write(
        &self,
        id: &AttachmentId,
    ) -> Result<(), crate::StoreError> {
        AttachmentRootSet::recover_abandoned_attachment_write(self.factory, id).await
    }
}

/// Survival proof: the condemn CAS is a *conditional mutation* in the authority
/// that owns the roots, not another read. A snapshot and a targeted probe that
/// both miss a committed ref no longer cost the blob its bytes.
#[tokio::test]
async fn condemn_cas_spares_a_live_blob_every_read_shaped_guard_missed() {
    let (factory, backend, id) = committed_factory_attachment().await;
    let roots = EmptySnapshotFactoryRoots {
        factory: factory.as_ref(),
    };

    let report = reclaim_unreferenced_attachments(
        &roots,
        &*backend,
        AttachmentReclamationPolicy::new(0, EmptyRootSetPolicy::AuthorizeDeleteAll),
    )
    .await
    .expect("sweep with deliberately empty snapshot");

    assert_eq!(report.fence, crate::AttachmentGcFence::Fenced);
    assert_eq!(
        report.reclaimed_count, 0,
        "the condemn CAS must refuse a digest whose committed ref the reads missed"
    );
    assert!(report.deleted_while_referenced.is_empty());
    backend
        .get(&id, 32 * 1024 * 1024)
        .await
        .expect("the committed blob survives a blind snapshot and a blind probe");
}

struct DeleteFailingAttachmentStore {
    inner: Arc<dyn AttachmentStore>,
}

impl DeleteFailingAttachmentStore {
    async fn new() -> Self {
        Self {
            inner: crate::testing::sqlite_memory_store_set()
                .await
                .attachment_store(),
        }
    }
}

#[async_trait::async_trait]
impl AttachmentStore for DeleteFailingAttachmentStore {
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
        Err(AttachmentStoreError::Backend {
            operation: "delete",
            class: AttachmentStoreFailureClass::Transient,
            source: format!("scripted delete failure for {id}").into(),
        })
    }

    async fn list(&self) -> Result<Vec<StoredBlobRef>, AttachmentStoreError> {
        self.inner.list().await
    }

    async fn head(&self, id: &AttachmentId) -> Result<Option<StoredBlobRef>, AttachmentStoreError> {
        self.inner.head(id).await
    }
}

#[tokio::test]
async fn gc_all_deletes_failed_is_incomplete() {
    let backend = DeleteFailingAttachmentStore::new().await;
    let first = backend
        .put(vec![1, 3, 3, 7], meta())
        .await
        .expect("put first deletion candidate");
    let second = backend
        .put(vec![2, 4, 4, 8], meta())
        .await
        .expect("put second deletion candidate");
    let roots = RecordingRootSet { manifests: vec![] };

    let report = reclaim_unreferenced_attachments(
        &roots,
        &backend,
        AttachmentReclamationPolicy::new(0, EmptyRootSetPolicy::AuthorizeDeleteAll),
    )
    .await
    .expect("per-item failures complete an incomplete sweep");

    assert_eq!(report.reclaimed_count, 0);
    assert_eq!(
        report.failed_ids.iter().cloned().collect::<BTreeSet<_>>(),
        [first.id, second.id].into_iter().collect::<BTreeSet<_>>()
    );
    assert!(report.condemn_deferred_ids.is_empty());
    assert_eq!(
        crate::store::MaintenanceReport::sweep(&report),
        crate::store::MaintenanceSweep::Incomplete,
        "a pass whose every destructive step failed is incomplete, not empty"
    );
}

#[tokio::test]
async fn gc_empty_backend_reports_nothing_to_do_with_root_diagnostic() {
    let backend = crate::testing::sqlite_memory_store_set()
        .await
        .attachment_store();

    let report = reclaim_unreferenced_attachments(
        &UnavailableRootSet,
        backend.as_ref(),
        AttachmentReclamationPolicy::new(0, EmptyRootSetPolicy::Refuse),
    )
    .await
    .expect("an enumerated empty backend has no destructive scope to refuse");

    assert_eq!(report.scanned_blob_count, 0);
    assert_eq!(report.reclaimed_count, 0);
    assert!(report.failed_ids.is_empty());
    assert!(report.condemn_deferred_ids.is_empty());
    // Nothing was scanned, so nothing was left undone: the unavailable root
    // set is reported as a diagnostic, not as an incomplete sweep.
    assert_eq!(
        crate::store::MaintenanceReport::sweep(&report),
        crate::store::MaintenanceSweep::NothingToDo
    );
    assert_eq!(
        report.root_enumeration_failure.as_deref(),
        Some(
            "failed to enumerate live attachment refs: store backend error: root enumeration unavailable"
        )
    );
}

#[tokio::test]
async fn gc_empty_root_set_does_not_refuse_when_every_blob_is_fresh() {
    let backend = crate::testing::sqlite_memory_store_set()
        .await
        .attachment_store();
    let attachment = backend
        .put(vec![4, 2, 4, 8], meta())
        .await
        .expect("put fresh blob");
    let roots = RecordingRootSet { manifests: vec![] };

    let report = reclaim_unreferenced_attachments(
        &roots,
        backend.as_ref(),
        AttachmentReclamationPolicy::new(60 * 60 * 1000, EmptyRootSetPolicy::Refuse),
    )
    .await
    .expect("fresh-only backend has no deletion candidate");

    assert_eq!(report.reclaimed_count, 0);
    backend
        .get(&attachment.id, 32 * 1024 * 1024)
        .await
        .expect("fresh blob survives");
}

#[tokio::test]
async fn gc_refuses_when_roots_are_unenumerable_and_blobs_are_only_grace_protected() {
    let backend = crate::testing::sqlite_memory_store_set()
        .await
        .attachment_store();
    let attachment = backend
        .put(vec![4, 2, 4, 9], meta())
        .await
        .expect("put fresh blob");

    let report = reclaim_unreferenced_attachments(
        &UnavailableRootSet,
        backend.as_ref(),
        AttachmentReclamationPolicy::new(60 * 60 * 1000, EmptyRootSetPolicy::Refuse),
    )
    .await
    .expect_err("an unenumerable root set cannot be reported as a healthy sweep");

    assert_eq!(
        report.refusal(),
        Some(&crate::store::MaintenanceRefusal::UnwitnessedScope {
            scope: "live attachment root set"
        })
    );
    let report = report.partial;
    assert_eq!(report.scanned_blob_count, 1);
    assert_eq!(report.reclaimed_count, 0);
    assert_eq!(
        report.root_enumeration_failure.as_deref(),
        Some(
            "failed to enumerate live attachment refs: store backend error: root enumeration unavailable"
        )
    );
    backend
        .get(&attachment.id, 32 * 1024 * 1024)
        .await
        .expect("fresh blob survives degraded sweep");
}

#[tokio::test]
async fn gc_non_empty_root_set_still_reclaims_an_unreferenced_blob() {
    let backend: Arc<dyn AttachmentStore> = crate::testing::sqlite_memory_store_set()
        .await
        .attachment_store();
    let manifest = Arc::new(RecordingReferrers::default());
    let session = RuntimeAttachmentStore::new(
        Arc::clone(&backend),
        manifest.clone() as Arc<dyn AttachmentReferrers>,
        session_owner("healthy-sweep"),
        AttachmentPolicy::standard(),
    );
    let live = session
        .put(vec![4, 2, 4, 9], meta())
        .await
        .expect("put live blob");
    manifest
        .acquire_attachment_refs(
            &session_claim("healthy-sweep"),
            std::slice::from_ref(&live.id),
        )
        .await
        .expect("commit live ref");
    manifest.end_uploads();
    let orphan = backend
        .put(vec![4, 2, 5, 0], meta())
        .await
        .expect("put orphan blob");
    let roots = RecordingRootSet {
        manifests: vec![manifest],
    };

    let report = reclaim_unreferenced_attachments(
        &roots,
        &*backend,
        AttachmentReclamationPolicy::new(0, EmptyRootSetPolicy::Refuse),
    )
    .await
    .expect("healthy non-empty-root sweep");

    assert_eq!(report.reclaimed_count, 1);
    backend
        .get(&live.id, 32 * 1024 * 1024)
        .await
        .expect("live blob survives");
    assert!(matches!(
        backend.get(&orphan.id, 32 * 1024 * 1024).await,
        Err(AttachmentStoreError::NotFound(_))
    ));
}

#[tokio::test]
async fn shared_bytes_survive_until_all_refs_released_then_gc_collects() {
    let backend: Arc<dyn AttachmentStore> = crate::testing::sqlite_memory_store_set()
        .await
        .attachment_store();
    let manifest_a = Arc::new(RecordingReferrers::default());
    let manifest_b = Arc::new(RecordingReferrers::default());
    let session_a = RuntimeAttachmentStore::new(
        backend.clone(),
        manifest_a.clone() as Arc<dyn AttachmentReferrers>,
        session_owner("session-a"),
        AttachmentPolicy::standard(),
    );
    let session_b = RuntimeAttachmentStore::new(
        backend.clone(),
        manifest_b.clone() as Arc<dyn AttachmentReferrers>,
        session_owner("session-b"),
        AttachmentPolicy::standard(),
    );

    // Two sessions put identical bytes: ONE physical blob. Each session
    // acquires its own edge and its upload ends, so the session edges are the
    // only roots and this test exercises edge-driven collection alone.
    let ref_a = session_a.put(vec![5, 5, 5], meta()).await.expect("put a");
    let ref_b = session_b.put(vec![5, 5, 5], meta()).await.expect("put b");
    assert_eq!(ref_a.id, ref_b.id);
    manifest_a
        .acquire_attachment_refs(&session_claim("session-a"), std::slice::from_ref(&ref_a.id))
        .await
        .expect("commit a");
    manifest_b
        .acquire_attachment_refs(&session_claim("session-b"), std::slice::from_ref(&ref_b.id))
        .await
        .expect("commit b");
    manifest_a.end_uploads();
    manifest_b.end_uploads();
    assert_eq!(backend.list().await.expect("list").len(), 1);

    let root_set = RecordingRootSet {
        manifests: vec![manifest_a.clone(), manifest_b.clone()],
    };

    // Session A releases its ref. Blob is still referenced by B: spared.
    session_a.delete(&ref_a.id).await.expect("a releases");
    let report = reclaim_unreferenced_attachments(
        &root_set,
        &*backend,
        AttachmentReclamationPolicy::new(0, EmptyRootSetPolicy::Refuse),
    )
    .await
    .expect("sweep with b holding a ref");
    assert_eq!(report.reclaimed_count, 0, "b still references the blob");
    assert_eq!(
        backend
            .get(&ref_b.id, 32 * 1024 * 1024)
            .await
            .expect("blob alive")
            .bytes,
        vec![5, 5, 5]
    );

    // Session B releases too. Now unreferenced: GC collects it.
    session_b.delete(&ref_b.id).await.expect("b releases");
    let report = reclaim_unreferenced_attachments(
        &root_set,
        &*backend,
        AttachmentReclamationPolicy::new(0, EmptyRootSetPolicy::AuthorizeDeleteAll),
    )
    .await
    .expect("sweep with no refs");
    assert_eq!(report.reclaimed_count, 1);
    assert!(matches!(
        backend.get(&ref_b.id, 32 * 1024 * 1024).await,
        Err(AttachmentStoreError::NotFound(_))
    ));
}

// Fix C: the GC delete-time re-check. A blob looks unreferenced and stale in
// the `list` snapshot, but a new intent + `put` of the same content id landed
// after the snapshot, refreshing the blob. The sweep must re-stat the blob via
// `head` before deleting and spare it. Modelled sequentially with a backend
// whose `list` reports a stale mtime and whose `head` reports a fresh one.
#[tokio::test]
async fn gc_delete_recheck_spares_blob_refreshed_after_snapshot() {
    struct StaleSnapshotStore {
        id: AttachmentId,
        list_mtime: u64,
        head_mtime: u64,
        deleted: Mutex<bool>,
    }

    #[async_trait::async_trait]
    impl AttachmentStore for StaleSnapshotStore {
        async fn put(
            &self,
            _bytes: Vec<u8>,
            _meta: AttachmentCreateMeta,
        ) -> Result<AttachmentRef, AttachmentStoreError> {
            unreachable!("test does not put through this store")
        }
        async fn get(
            &self,
            id: &AttachmentId,
            _max_bytes: u64,
        ) -> Result<StoredAttachment, AttachmentStoreError> {
            Err(AttachmentStoreError::NotFound(id.clone()))
        }
        async fn delete(&self, _id: &AttachmentId) -> Result<(), AttachmentStoreError> {
            *self.deleted.lock_recover() = true;
            Ok(())
        }
        async fn list(&self) -> Result<Vec<StoredBlobRef>, AttachmentStoreError> {
            // Stale snapshot: the blob looks old and unreferenced.
            Ok(vec![StoredBlobRef {
                id: self.id.clone(),
                last_modified_epoch_ms: Some(self.list_mtime),
            }])
        }
        async fn head(
            &self,
            id: &AttachmentId,
        ) -> Result<Option<StoredBlobRef>, AttachmentStoreError> {
            // Fresh re-stat: the new intent's `put` refreshed the blob.
            Ok((id == &self.id).then(|| StoredBlobRef {
                id: id.clone(),
                last_modified_epoch_ms: Some(self.head_mtime),
            }))
        }
    }

    let now = now_epoch_ms();
    const GRACE_MS: u64 = 60 * 60 * 1000;
    let backend = StaleSnapshotStore {
        id: lash_core_store::attachments::content_id(b"recheck"),
        // Stale mtime well past the grace window: the first check would delete.
        list_mtime: now.saturating_sub(GRACE_MS * 2),
        // Fresh mtime inside the window: the re-check must spare it.
        head_mtime: now,
        deleted: Mutex::new(false),
    };
    // Empty root set: the snapshot did not see the new intent.
    let root_set = RecordingRootSet { manifests: vec![] };
    let report = reclaim_unreferenced_attachments(
        &root_set,
        &backend,
        AttachmentReclamationPolicy::new(GRACE_MS, EmptyRootSetPolicy::Refuse),
    )
    .await
    .expect("sweep");
    assert_eq!(
        report.reclaimed_count, 0,
        "the delete-time re-check must spare a blob refreshed after the snapshot"
    );
    assert!(
        !*backend.deleted.lock_recover(),
        "the freshly-refreshed blob must not be deleted"
    );
}

// Fix C, checks (b) and (d): the delete-window root re-check. A candidate blob
// is stale in both the `list` snapshot AND the `head` re-stat (so the freshness
// gate does not spare it), but a session records a fresh intent for the same
// content id in the delete window. A backend whose `head` reports a stale mtime
// and a root set scripted to answer the single-id probe shift the two branches:
// (b) a ref present before delete spares the blob; (d) a ref that appears only
// after delete is detected and alarmed via `deleted_while_referenced`.
struct StaleHeadStore {
    id: AttachmentId,
    mtime: u64,
    deleted: Mutex<bool>,
}

#[async_trait::async_trait]
impl AttachmentStore for StaleHeadStore {
    async fn put(
        &self,
        _bytes: Vec<u8>,
        _meta: AttachmentCreateMeta,
    ) -> Result<AttachmentRef, AttachmentStoreError> {
        unreachable!("test does not put through this store")
    }
    async fn get(
        &self,
        id: &AttachmentId,
        _max_bytes: u64,
    ) -> Result<StoredAttachment, AttachmentStoreError> {
        Err(AttachmentStoreError::NotFound(id.clone()))
    }
    async fn delete(&self, _id: &AttachmentId) -> Result<(), AttachmentStoreError> {
        *self.deleted.lock_recover() = true;
        Ok(())
    }
    async fn list(&self) -> Result<Vec<StoredBlobRef>, AttachmentStoreError> {
        Ok(vec![StoredBlobRef {
            id: self.id.clone(),
            last_modified_epoch_ms: Some(self.mtime),
        }])
    }
    async fn head(&self, id: &AttachmentId) -> Result<Option<StoredBlobRef>, AttachmentStoreError> {
        // Stale re-stat: the freshness gate does not spare the blob, so the
        // targeted root re-check is what must decide its fate.
        Ok((id == &self.id).then(|| StoredBlobRef {
            id: id.clone(),
            last_modified_epoch_ms: Some(self.mtime),
        }))
    }
}

/// Root set whose single-id probe returns scripted answers in order — models
/// a ref appearing at a chosen point in the delete window. `live_attachment_refs`
/// is empty (the snapshot never saw the late ref).
struct ScriptedRootSet {
    answers: Mutex<std::collections::VecDeque<bool>>,
}

#[async_trait::async_trait]
impl AttachmentRootSet for ScriptedRootSet {
    async fn attachment_root_page(
        &self,
        _source: crate::attachments::AttachmentRootSource,
        _after: Option<&AttachmentId>,
    ) -> Result<crate::attachments::AttachmentRootPage, crate::StoreError> {
        crate::attachments::AttachmentRootPage::from_rows(Vec::new())
    }

    async fn has_live_attachment_ref(&self, _id: &AttachmentId) -> Result<bool, crate::StoreError> {
        Ok(self.answers.lock_recover().pop_front().unwrap_or(false))
    }
}

#[tokio::test]
async fn gc_pre_delete_root_recheck_spares_reappeared_ref() {
    let now = now_epoch_ms();
    const GRACE_MS: u64 = 60 * 60 * 1000;
    let backend = StaleHeadStore {
        id: lash_core_store::attachments::content_id(b"reappeared"),
        // Well past the grace window: neither the snapshot nor the head re-stat
        // spares it, so the single-id root re-check is the only guard left.
        mtime: now.saturating_sub(GRACE_MS * 2),
        deleted: Mutex::new(false),
    };
    // (b) sees a live ref: the probe answers true before the delete.
    let root_set = ScriptedRootSet {
        answers: Mutex::new([true].into_iter().collect()),
    };
    let report = reclaim_unreferenced_attachments(
        &root_set,
        &backend,
        AttachmentReclamationPolicy::new(GRACE_MS, EmptyRootSetPolicy::Refuse),
    )
    .await
    .expect("sweep");
    assert_eq!(
        report.reclaimed_count, 0,
        "pre-delete root re-check must spare the blob"
    );
    assert!(report.deleted_while_referenced.is_empty());
    assert!(
        !*backend.deleted.lock_recover(),
        "a blob re-referenced before delete must not be deleted"
    );
}

// An unfenced/legacy root authority — one that implements neither the
// condemnation transitions nor the writer's fenced intent insert — cannot close
// the delete window. The operation must say so rather than imply a guarantee:
// it reports `BestEffort`, and a ref that appears in the window is recorded as
// detection telemetry (the bytes are gone; lash cannot restore them, which is
// why such deployments belong on a backend with recoverable deletion).
#[tokio::test]
async fn unfenced_root_authority_reports_best_effort_and_detects_the_window_loss() {
    let now = now_epoch_ms();
    const GRACE_MS: u64 = 60 * 60 * 1000;
    let id = lash_core_store::attachments::content_id(b"window-ref");
    let backend = StaleHeadStore {
        id: id.clone(),
        mtime: now.saturating_sub(GRACE_MS * 2),
        deleted: Mutex::new(false),
    };
    // The pre-delete probe sees no ref (delete proceeds); the post-delete probe
    // sees a ref that appeared in the window.
    let root_set = ScriptedRootSet {
        answers: Mutex::new([false, true].into_iter().collect()),
    };
    let report = reclaim_unreferenced_attachments(
        &root_set,
        &backend,
        AttachmentReclamationPolicy::new(GRACE_MS, EmptyRootSetPolicy::AuthorizeDeleteAll),
    )
    .await
    .expect("sweep");
    assert_eq!(
        report.fence,
        crate::AttachmentGcFence::BestEffort,
        "an authority with no condemnation CAS must report itself best-effort"
    );
    assert_eq!(report.reclaimed_count, 1, "the blob is deleted");
    assert!(*backend.deleted.lock_recover(), "delete happened");
    assert_eq!(
        report.deleted_while_referenced,
        vec![id],
        "the unfenced path detects the window loss it cannot prevent"
    );
}

// ---------------------------------------------------------------------------
// The GC fence: a real writer against a real fenced root authority.
//
// The in-memory factory owns both halves the fence needs — the manifest the
// writer records intents in and the per-digest condemnation state the sweep
// CASes — so these exercise the same protocol a durable deployment runs, with
// the sweep's own backend calls as the interleaving points.
// ---------------------------------------------------------------------------

type WindowHook =
    Arc<dyn Fn() -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send>> + Send + Sync>;

type WindowWriterHandle = tokio::task::JoinHandle<Result<AttachmentRef, String>>;
/// Slot a window hook drops its writer's handle into for the test to await.
type WindowWriterSlot = Arc<Mutex<Option<WindowWriterHandle>>>;

/// Backend wrapper that runs a hook the first time the sweep calls `head` (the
/// digest is condemned but no delete is issued) or `delete` (the delete is in
/// flight) — the two instants a concurrent same-content write can land in.
struct WindowHookedStore {
    inner: Arc<dyn AttachmentStore>,
    on_head: Mutex<Option<WindowHook>>,
    on_delete: Mutex<Option<WindowHook>>,
    delete_calls: Mutex<usize>,
}

impl WindowHookedStore {
    fn new(inner: Arc<dyn AttachmentStore>) -> Self {
        Self {
            inner,
            on_head: Mutex::new(None),
            on_delete: Mutex::new(None),
            delete_calls: Mutex::new(0),
        }
    }

    async fn fire(hook: &Mutex<Option<WindowHook>>) {
        let taken = hook.lock_recover().take();
        if let Some(hook) = taken {
            hook().await;
        }
    }
}

#[async_trait::async_trait]
impl AttachmentStore for WindowHookedStore {
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
        *self.delete_calls.lock_recover() += 1;
        Self::fire(&self.on_delete).await;
        self.inner.delete(id).await
    }
    async fn list(&self) -> Result<Vec<StoredBlobRef>, AttachmentStoreError> {
        self.inner.list().await
    }
    async fn head(&self, id: &AttachmentId) -> Result<Option<StoredBlobRef>, AttachmentStoreError> {
        Self::fire(&self.on_head).await;
        self.inner.head(id).await
    }
}

struct FencedFixture {
    factory: Arc<dyn crate::DeploymentStore>,
    store: crate::store::SessionStore,
    backend: Arc<dyn AttachmentStore>,
    session: Arc<RuntimeAttachmentStore>,
    /// Every fence outcome the facade observed, in order.
    fence_attempts:
        Arc<tokio::sync::Mutex<tokio::sync::mpsc::UnboundedReceiver<AttachmentWriteFence>>>,
}

async fn fenced_fixture(session_id: &SessionId) -> FencedFixture {
    let substrate = crate::testing::sqlite_memory_store_set().await;
    let factory: Arc<dyn crate::DeploymentStore> = substrate.session_store_factory();
    let request = crate::SessionStoreCreateRequest {
        owning_process_id: None,
        pending_observer_intents: Vec::new(),
        session_id: session_id.clone(),
        relation: crate::SessionRelation::Root,
        config: crate::PersistedSessionConfig::new(
            crate::TurnBudget::Unbounded,
            crate::MaxToolCalls::new(1024),
            crate::NoProgressBudget::bounded(12),
            crate::SessionToolAccess::ambient(),
        ),
        head: crate::SessionCreationHead::Config,
        retention: crate::Retention::UntilGc,
    };
    let store = crate::testing::runtime_helpers::create_session_store(&factory, &request)
        .await
        .expect("create attachment-aware store");
    let backend = substrate.attachment_store();
    let (attempts, fence_attempts) = tokio::sync::mpsc::unbounded_channel();
    let session = Arc::new(RuntimeAttachmentStore::new(
        Arc::clone(&backend) as Arc<dyn AttachmentStore>,
        Arc::new(SignalingManifest {
            inner: Arc::new(PersistenceReferrersAdapter(Arc::clone(store.store()))),
            attempts,
        }),
        RuntimeOwner::Session(session_id.clone()),
        AttachmentPolicy::standard(),
    ));
    FencedFixture {
        factory,
        store,
        backend,
        session,
        fence_attempts: Arc::new(tokio::sync::Mutex::new(fence_attempts)),
    }
}

fn collecting_policy() -> AttachmentReclamationPolicy {
    AttachmentReclamationPolicy::new(0, EmptyRootSetPolicy::AuthorizeDeleteAll)
}

/// Spawn a facade `put` from inside a sweep hook and hand control back only when
/// the writer has actually reached the point the window is about to test: it
/// waits for the writer's first fence outcome, and — when that outcome is a
/// grant — for the bytes to land. No sleeps, no scheduling luck.
fn window_writer(
    session: Arc<RuntimeAttachmentStore>,
    bytes: Vec<u8>,
    attempts: Arc<tokio::sync::Mutex<tokio::sync::mpsc::UnboundedReceiver<AttachmentWriteFence>>>,
    handle_slot: WindowWriterSlot,
) -> WindowHook {
    Arc::new(move || {
        let session = Arc::clone(&session);
        let bytes = bytes.clone();
        let attempts = Arc::clone(&attempts);
        let handle_slot = Arc::clone(&handle_slot);
        Box::pin(async move {
            let (put_done, put_done_rx) = tokio::sync::oneshot::channel();
            let handle = crate::task::spawn(async move {
                let outcome = session
                    .put(bytes, meta())
                    .await
                    .map_err(|err| err.to_string());
                let _ = put_done.send(());
                outcome
            });
            *handle_slot.lock_recover() = Some(handle);
            let mut attempts = attempts.lock().await;
            match attempts.recv().await {
                // The writer is inside the window with an intent recorded: give
                // its bytes time to land before the window closes.
                Some(AttachmentWriteFence::Granted(_)) => {
                    let _ = put_done_rx.await;
                }
                // The writer is parked on the fence. Nothing more will happen
                // until this window closes, which is the point.
                Some(AttachmentWriteFence::ReclamationInFlight) | None => {}
            }
        })
    })
}

/// Manifest wrapper that reports each fence acquisition the facade makes, so a
/// window hook can wait for the writer instead of racing it.
struct SignalingManifest {
    inner: Arc<dyn AttachmentReferrers>,
    attempts: tokio::sync::mpsc::UnboundedSender<AttachmentWriteFence>,
}

#[async_trait::async_trait]
impl AttachmentReferrers for SignalingManifest {
    async fn begin_attachment_write(
        &self,
        write: &AttachmentWrite,
    ) -> Result<AttachmentWriteFence, crate::StoreError> {
        let fence = self.inner.begin_attachment_write(write).await?;
        let _ = self.attempts.send(fence);
        Ok(fence)
    }

    async fn complete_attachment_write(
        &self,
        write: &AttachmentWrite,
        permit: crate::AttachmentWritePermit,
    ) -> Result<(), crate::StoreError> {
        self.inner.complete_attachment_write(write, permit).await
    }

    async fn abort_attachment_write(
        &self,
        write: &AttachmentWrite,
        permit: crate::AttachmentWritePermit,
    ) -> Result<(), crate::StoreError> {
        self.inner.abort_attachment_write(write, permit).await
    }

    async fn acquire_attachment_refs(
        &self,
        claim: &ReferrerClaim,
        attachment_ids: &[AttachmentId],
    ) -> Result<(), crate::StoreError> {
        self.inner
            .acquire_attachment_refs(claim, attachment_ids)
            .await
    }

    async fn forget_attachment_ref(
        &self,
        referrer: &ArtifactReferrer,
        attachment_id: &AttachmentId,
    ) -> Result<(), crate::StoreError> {
        self.inner
            .forget_attachment_ref(referrer, attachment_id)
            .await
    }

    async fn end_attachment_referrer(
        &self,
        referrer: &ArtifactReferrer,
    ) -> Result<(), crate::StoreError> {
        self.inner.end_attachment_referrer(referrer).await
    }

    async fn session_referrer_state(
        &self,
        session_id: &SessionId,
    ) -> Result<crate::SessionReferrerState, crate::StoreError> {
        self.inner.session_referrer_state(session_id).await
    }

    async fn attachment_referrers(
        &self,
        attachment_id: &AttachmentId,
    ) -> Result<Vec<ArtifactReferrer>, crate::StoreError> {
        self.inner.attachment_referrers(attachment_id).await
    }
}

/// SURVIVAL PROOF (the delete window). A session writes the same content while
/// the sweep's physical delete is already in flight — the exact interleaving the
/// pre-fence sweep reported as `deleted_while_referenced` after the bytes were
/// gone. The writer cannot record an intent for an armed digest, so it parks,
/// and the release that follows the delete lets it record and re-put: the
/// content is present when the sweep returns.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn same_content_put_inside_the_delete_window_survives() {
    let fixture = fenced_fixture(&SessionId::from("delete-window-writer")).await;
    let bytes = vec![7, 1, 7];
    let id = content_id(&bytes);
    // Genuine garbage: identical bytes with no live root, so the sweep is right
    // to collect them.
    fixture
        .backend
        .put(bytes.clone(), meta())
        .await
        .expect("seed the unreferenced blob");

    let handle_slot = Arc::new(Mutex::new(None));
    let backend = WindowHookedStore::new(Arc::clone(&fixture.backend));
    *backend.on_delete.lock_recover() = Some(window_writer(
        Arc::clone(&fixture.session),
        bytes.clone(),
        Arc::clone(&fixture.fence_attempts),
        Arc::clone(&handle_slot),
    ));

    let report =
        reclaim_unreferenced_attachments(fixture.factory.as_ref(), &backend, collecting_policy())
            .await
            .expect("sweep");

    let writer = handle_slot
        .lock_recover()
        .take()
        .expect("the window writer was started");
    let reference = writer
        .await
        .expect("writer task")
        .expect("the write completes rather than failing on the fence");
    assert_eq!(reference.id, id);
    assert_eq!(
        fixture
            .backend
            .get(&id, 32 * 1024 * 1024)
            .await
            .expect("the write that landed in the delete window survives")
            .bytes,
        bytes
    );
    assert!(
        !crate::AttachmentReferrers::attachment_referrers(fixture.store.store().as_ref(), &id)
            .await
            .expect("referrer probe")
            .is_empty(),
        "the surviving bytes are rooted by the writer's upload"
    );
    assert!(
        report.deleted_while_referenced.is_empty(),
        "no root exists for an armed digest, so the detector must stay silent"
    );
    assert_eq!(report.fence, crate::AttachmentGcFence::Fenced);
}

/// CONTENTION after arming and before final `HEAD`. Arming must precede the
/// absence observation, so a writer arriving in this window parks until the
/// sweep retires the condemnation, then restores the bytes under a fresh
/// write of its own.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn writer_after_delete_arming_restores_the_deleted_digest() {
    let fixture = fenced_fixture(&SessionId::from("condemned-window-writer")).await;
    let bytes = vec![2, 7, 1, 8];
    let id = content_id(&bytes);
    fixture
        .backend
        .put(bytes.clone(), meta())
        .await
        .expect("seed the unreferenced blob");

    let handle_slot = Arc::new(Mutex::new(None));
    let backend = WindowHookedStore::new(Arc::clone(&fixture.backend));
    *backend.on_head.lock_recover() = Some(window_writer(
        Arc::clone(&fixture.session),
        bytes.clone(),
        Arc::clone(&fixture.fence_attempts),
        Arc::clone(&handle_slot),
    ));

    let report =
        reclaim_unreferenced_attachments(fixture.factory.as_ref(), &backend, collecting_policy())
            .await
            .expect("sweep");

    let writer = handle_slot
        .lock_recover()
        .take()
        .expect("the window writer was started");
    writer
        .await
        .expect("writer task")
        .expect("a writer after arming retries and restores the digest");
    assert_eq!(
        report.reclaimed_count, 1,
        "the armed sweep must record its completed reclamation"
    );
    assert_eq!(
        *backend.delete_calls.lock_recover(),
        1,
        "the physical delete is only ever issued for an armed digest"
    );
    assert!(report.condemn_deferred_ids.is_empty());
    assert!(
        report.deleted_while_referenced.is_empty(),
        "a fenced sweep must never delete a referenced blob: {:?}",
        report.deleted_while_referenced
    );
    assert_eq!(
        fixture
            .backend
            .get(&id, 32 * 1024 * 1024)
            .await
            .expect("survives")
            .bytes,
        bytes
    );
}

/// SKIP-ON-CONTENTION between sweepers: a digest a live peer sweeper holds is
/// deferred, not waited on, and no delete is issued for it. Once that peer is
/// dead, the next sweep adopts its condemnation and finishes the delete
/// (ADR 0067 §6); no host lever is involved.
#[tokio::test]
async fn a_live_peers_condemnation_defers_and_a_dead_peers_is_adopted() {
    let fixture = fenced_fixture(&SessionId::from("peer-sweeper")).await;
    let bytes = vec![3, 3, 3];
    let id = content_id(&bytes);
    fixture
        .backend
        .put(bytes.clone(), meta())
        .await
        .expect("seed the unreferenced blob");

    // A peer sweeper's condemnation, held while the peer runs.
    let peer = AttachmentRootSet::begin_attachment_sweep(fixture.factory.as_ref())
        .await
        .expect("peer pass");
    assert_eq!(
        AttachmentRootSet::condemn_attachment(fixture.factory.as_ref(), &id, &peer)
            .await
            .expect("first condemn"),
        crate::AttachmentCondemnation::Condemned
    );
    assert_eq!(
        AttachmentRootSet::condemn_attachment(fixture.factory.as_ref(), &id, &peer)
            .await
            .expect("second condemn"),
        crate::AttachmentCondemnation::AlreadyCondemned,
        "a condemned digest is never contended for"
    );

    let backend = WindowHookedStore::new(Arc::clone(&fixture.backend));
    let report =
        reclaim_unreferenced_attachments(fixture.factory.as_ref(), &backend, collecting_policy())
            .await
            .expect("sweep");
    assert_eq!(report.condemn_deferred_ids, vec![id.clone()]);
    assert_eq!(report.adopted_count, 0, "a live peer's row is not adopted");
    assert!(
        report.deleted_while_referenced.is_empty(),
        "a fenced sweep must never delete a referenced blob: {:?}",
        report.deleted_while_referenced
    );
    assert_eq!(report.reclaimed_count, 0);
    assert_eq!(*backend.delete_calls.lock_recover(), 0);
    fixture
        .backend
        .get(&id, 32 * 1024 * 1024)
        .await
        .expect("a deferred digest keeps its bytes");

    // The peer dies without settling its condemnation.
    drop(peer);
    let report =
        reclaim_unreferenced_attachments(fixture.factory.as_ref(), &backend, collecting_policy())
            .await
            .expect("second sweep");
    assert_eq!(report.adopted_count, 1);
    assert_eq!(report.reclaimed_count, 1);
    assert!(report.condemn_deferred_ids.is_empty());
    assert!(
        AttachmentRootSet::list_condemnations(fixture.factory.as_ref())
            .await
            .expect("list condemnations")
            .is_empty(),
        "the adopted condemnation is retired"
    );
}

/// A stuck execution is a root: a put under a turn whose journal never settles
/// keeps the blob, and the condemn CAS is what refuses — no age, no clock.
#[tokio::test]
async fn a_stuck_execution_retains_the_blob() {
    let fixture = fenced_fixture(&SessionId::from("stuck-intent")).await;
    let binding = fixture
        .session
        .bind_execution_scoped(
            crate::ExecutionScope::turn("stuck-intent", "turn-that-never-commits")
                .journal_identity()
                .expect("a valid turn scope"),
        )
        .expect("a session runtime binds its execution");
    let reference = fixture
        .session
        .put(vec![4, 4], meta())
        .await
        .expect("put under a turn owner");
    drop(binding);

    let pass = AttachmentRootSet::begin_attachment_sweep(fixture.factory.as_ref())
        .await
        .expect("pass");
    assert_eq!(
        AttachmentRootSet::condemn_attachment(fixture.factory.as_ref(), &reference.id, &pass)
            .await
            .expect("condemn"),
        crate::AttachmentCondemnation::RootPresent,
        "an execution edge whose journal never settled is a live root"
    );

    let backend = WindowHookedStore::new(Arc::clone(&fixture.backend));
    let report =
        reclaim_unreferenced_attachments(fixture.factory.as_ref(), &backend, collecting_policy())
            .await
            .expect("sweep");
    assert_eq!(report.reclaimed_count, 0);
    assert_eq!(*backend.delete_calls.lock_recover(), 0);
    assert!(
        report.deleted_while_referenced.is_empty(),
        "a fenced sweep must never delete a referenced blob: {:?}",
        report.deleted_while_referenced
    );
    assert_eq!(report.fence, crate::AttachmentGcFence::Fenced);
    fixture
        .backend
        .get(&reference.id, 32 * 1024 * 1024)
        .await
        .expect("the blob a stuck execution roots survives");
}

#[tokio::test]
async fn a_bound_execution_holds_its_puts_and_an_unbound_put_its_upload() {
    let manifest = Arc::new(RecordingReferrers::default());
    let store = Arc::new(RuntimeAttachmentStore::new(
        crate::testing::sqlite_memory_store_set()
            .await
            .attachment_store(),
        manifest.clone(),
        session_owner("session-1"),
        AttachmentPolicy::standard(),
    ));
    let journal = crate::ExecutionScope::turn("session-1", "turn-1")
        .journal_identity()
        .expect("a valid turn scope");
    let binding = store
        .bind_execution_scoped(journal.clone())
        .expect("a session runtime binds its execution");

    let reference = store.put(vec![8, 9, 10], meta()).await.expect("put");
    assert_eq!(
        manifest.attachment_referrers(&reference.id).await.unwrap(),
        vec![ArtifactReferrer::Execution(journal.clone())]
    );
    assert_eq!(
        store.recorded_execution_puts(&journal),
        [reference.id.clone()].into_iter().collect()
    );

    drop(binding);
    let host_reference = store.put(vec![11, 12], meta()).await.expect("host put");
    let referrers = manifest
        .attachment_referrers(&host_reference.id)
        .await
        .unwrap();
    assert!(
        matches!(referrers.as_slice(), [ArtifactReferrer::Upload(upload)]
            if upload.session_id().as_str() == "session-1"),
        "an unbound put is held by a fresh upload of its session: {referrers:?}"
    );
}

#[tokio::test]
async fn a_process_runtime_put_is_held_by_its_record() {
    let manifest = Arc::new(RecordingReferrers::default());
    let process_id = crate::ProcessId::fixture("process-1");
    let store = Arc::new(RuntimeAttachmentStore::new(
        crate::testing::sqlite_memory_store_set()
            .await
            .attachment_store(),
        manifest.clone(),
        RuntimeOwner::Process(process_id.clone()),
        AttachmentPolicy::standard(),
    ));
    assert!(
        store
            .bind_execution_scoped(
                crate::ExecutionScope::turn("session-1", "turn-1")
                    .journal_identity()
                    .expect("a valid turn scope"),
            )
            .is_err(),
        "only a session runtime binds an execution"
    );

    let reference = store.put(vec![2], meta()).await.expect("process put");
    assert_eq!(
        manifest.attachment_referrers(&reference.id).await.unwrap(),
        vec![ArtifactReferrer::ProcessRecord(process_id)]
    );
}

fn attachment_request(
    attachments: Vec<crate::AttachmentRef>,
) -> Arc<crate::llm::types::LlmRequest> {
    let blocks = attachments
        .into_iter()
        .map(|reference| crate::llm::types::LlmContentBlock::Attachment {
            reference: Box::new(reference),
        })
        .collect();
    Arc::new(crate::llm::types::LlmRequest {
        instructions: None,
        model: lash_sansio::llm_profile::LlmProfileConfig::new(
            lash_sansio::llm_profile::RecordedLlmProfile::mint(
                lash_sansio::llm_profile::LlmProfileKey::new("request-fixture"),
                lash_sansio::llm_profile::LlmProfileMetadata::builder(
                    "attachment-model".to_string(),
                )
                .context_window_tokens(128_000)
                .capability(Default::default())
                .extra_body(Default::default())
                .cache_retention(lash_sansio::llm::capability::CacheRetention::Short)
                .build()
                .expect("valid profile"),
            ),
        )
        .with_reasoning(crate::ReasoningSelection::ProviderDefault),
        messages: vec![crate::llm::types::LlmMessage::new(
            crate::llm::types::LlmRole::User,
            blocks,
        )],
        tools: Arc::new(Vec::new()),
        tool_choice: crate::llm::types::LlmToolChoice::None,
        attachment_acceptance: lash_core_store::attachments::attachment_test_acceptance(),
        generation: crate::llm::types::GenerationOptions::default(),
        scope: crate::llm::types::LlmRequestScope::new(
            "attachment-session",
            "attachment-frame",
            "attachment-request",
        ),
        output_spec: None,
        stream_events: None,
        provider_trace: None,
    })
}

#[test]
fn pinned_session_attachment_acceptance_survives_model_catalogue_change() {
    let source = AttachmentRef::new(
        content_id(&[1, 2, 3]),
        MediaType::parse("image/png").unwrap(),
        3,
        None,
        None,
    );
    let recorded = |key: &str| {
        crate::testing::test_llm_profile_config(key, crate::testing::test_llm_profile_metadata(key))
    };
    let mut policy = crate::SessionPolicy::new(
        crate::TurnBudget::Unbounded,
        crate::MaxToolCalls::new(1024),
        crate::NoProgressBudget::bounded(12),
    );
    policy.model = Some(recorded("attachment-model"));
    policy.attachment_acceptance = lash_core_store::attachments::attachment_test_acceptance();
    let changed_host = Arc::new(crate::provider::AttachmentCapabilitySnapshot {
        revision: "test-host-revision-2".to_string(),
        acceptors: Vec::new(),
    });
    // A model command moves the session to another model; its acceptance is
    // its own recorded field and stays.
    let registry = crate::ConfigRegistry::build(&[]).expect("the core owner registers");
    let models = crate::testing::single_llm_profile_registry(
        crate::LlmProfileKey::new("upgraded-attachment-model"),
        crate::testing::test_llm_profile_metadata("upgraded-attachment-model"),
        crate::testing::TestProvider::builder()
            .kind("attachment-catalogue")
            .build()
            .into_handle(),
    );
    let entries = registry
        .entries(&crate::ConfigTransaction::of(
            crate::plugin::config::core::SetLlmProfile {
                model: crate::LlmProfileKey::new("upgraded-attachment-model"),
            },
        ))
        .expect("the core owner registers the model command");
    let transaction = registry
        .admit("upgrade-model", 0, entries)
        .expect("the model command is admitted");
    let mut config =
        crate::PersistedSessionConfig::from_policy(&policy, crate::SessionToolAccess::ambient());
    assert!(matches!(
        registry
            .resolve(
                &config,
                &transaction,
                models.as_ref(),
                &crate::store::plugin_writers::PluginAdmission::default(),
                &crate::plugin::prompt::PromptCatalog::default(),
            )
            .expect("the recorded config reads")
            .publish(&mut config),
        crate::ConfigTransactionOutcome::Applied { .. }
    ));
    let policy = config.session_policy();
    assert_eq!(policy.model, Some(recorded("upgraded-attachment-model")));
    let restored: crate::SessionPolicy =
        serde_json::from_slice(&serde_json::to_vec(&policy).unwrap()).unwrap();
    let mut historical = attachment_request(vec![source.clone()]);
    Arc::make_mut(&mut historical).attachment_acceptance = restored.attachment_acceptance;
    let notices = degrade_unmaterializable_request_attachments(&mut historical);
    assert!(
        notices.is_empty(),
        "the opening revision must preserve historical rendering"
    );
    assert_eq!(historical.attachments().collect::<Vec<_>>(), vec![&source]);
    assert_eq!(
        historical.attachment_acceptance.revision,
        "test-host-revision-1"
    );
    let mut unpinned = attachment_request(vec![source]);
    Arc::make_mut(&mut unpinned).attachment_acceptance = changed_host;
    assert_eq!(
        degrade_unmaterializable_request_attachments(&mut unpinned).len(),
        1
    );
    assert!(
        unpinned.attachments().next().is_none(),
        "the changed table must be a meaningful counterexample"
    );
}

/// A manifest whose durable work takes real time, standing in for a Postgres or
/// SQLite round trip.
struct SlowManifest {
    inner: NoopAttachmentReferrers,
    delay: std::time::Duration,
}

#[async_trait::async_trait]
impl AttachmentReferrers for SlowManifest {
    async fn begin_attachment_write(
        &self,
        write: &AttachmentWrite,
    ) -> Result<AttachmentWriteFence, crate::StoreError> {
        tokio::time::sleep(self.delay).await;
        self.inner.begin_attachment_write(write).await
    }

    async fn complete_attachment_write(
        &self,
        write: &AttachmentWrite,
        permit: crate::AttachmentWritePermit,
    ) -> Result<(), crate::StoreError> {
        self.inner.complete_attachment_write(write, permit).await
    }

    async fn abort_attachment_write(
        &self,
        write: &AttachmentWrite,
        permit: crate::AttachmentWritePermit,
    ) -> Result<(), crate::StoreError> {
        self.inner.abort_attachment_write(write, permit).await
    }

    async fn acquire_attachment_refs(
        &self,
        claim: &ReferrerClaim,
        attachment_ids: &[AttachmentId],
    ) -> Result<(), crate::StoreError> {
        self.inner
            .acquire_attachment_refs(claim, attachment_ids)
            .await
    }

    async fn forget_attachment_ref(
        &self,
        referrer: &ArtifactReferrer,
        attachment_id: &AttachmentId,
    ) -> Result<(), crate::StoreError> {
        self.inner
            .forget_attachment_ref(referrer, attachment_id)
            .await
    }

    async fn end_attachment_referrer(
        &self,
        referrer: &ArtifactReferrer,
    ) -> Result<(), crate::StoreError> {
        self.inner.end_attachment_referrer(referrer).await
    }

    async fn session_referrer_state(
        &self,
        session_id: &SessionId,
    ) -> Result<crate::SessionReferrerState, crate::StoreError> {
        self.inner.session_referrer_state(session_id).await
    }

    async fn attachment_referrers(
        &self,
        attachment_id: &AttachmentId,
    ) -> Result<Vec<ArtifactReferrer>, crate::StoreError> {
        self.inner.attachment_referrers(attachment_id).await
    }
}

/// FIG-3076 (succeeding FIG-3073's bridge regression): a manifest write must
/// not stop the caller's runtime.
///
/// The worker that wedged in the multi-worker E2E had one attachment write
/// in flight and nothing else could run — not the session-lease renewal, not
/// the h2 accept loop, not an unrelated `/health` listener on its own port.
/// This reproduces that shape at the async boundary the manifest now exposes:
/// a single-worker multi-thread runtime, one heartbeat task, and a manifest
/// whose durable half takes 300ms. With the write awaited rather than bridged
/// onto the worker thread, the heartbeat keeps ticking throughout; any
/// re-introduced `block_on`/`spawn_blocking`/throwaway-runtime bridge on the
/// sole worker leaves it at zero.
#[test]
fn a_manifest_write_leaves_the_caller_runtime_running() {
    use std::sync::atomic::{AtomicUsize, Ordering};

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(1)
        .enable_all()
        .build()
        .expect("build single-worker runtime");

    let ticks = Arc::new(AtomicUsize::new(0));
    let heartbeat = Arc::clone(&ticks);
    let observed = Arc::clone(&ticks);

    let (reference, before, after) = runtime.block_on(async move {
        crate::task::spawn(async move {
            loop {
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
                heartbeat.fetch_add(1, Ordering::SeqCst);
            }
        });
        // Let the heartbeat reach its first await point before the write starts.
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        let before = observed.load(Ordering::SeqCst);
        // The write has to run on a worker thread, which is where every real
        // manifest call runs. Executing it from the `block_on` thread instead
        // would leave the worker free and prove nothing.
        let worker = crate::task::spawn(async move {
            let session = RuntimeAttachmentStore::new(
                crate::testing::sqlite_memory_store_set()
                    .await
                    .attachment_store(),
                Arc::new(SlowManifest {
                    inner: NoopAttachmentReferrers,
                    delay: std::time::Duration::from_millis(300),
                }),
                session_owner("runtime-liveness"),
                AttachmentPolicy::standard(),
            );
            let reference = session
                .put(vec![4, 2], meta())
                .await
                .expect("the attachment write completes");
            (reference, observed.load(Ordering::SeqCst))
        });
        let (reference, after) = worker.await.expect("attachment write task");
        (reference, before, after)
    });

    assert_eq!(reference.id, content_id(&[4, 2]));
    assert!(
        after > before + 1,
        "the caller's runtime stopped while a manifest write was in flight: \
         heartbeat went {before} -> {after} across a 300ms write"
    );
}
