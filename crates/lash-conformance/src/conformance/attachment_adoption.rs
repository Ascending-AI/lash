//! Cross-session stored references acquire receiver roots in the boundary commit.
//! Root reconciliation is the layer-1 operation also used by terminal evidence
//! reclamation, so this witness can run at every head in the stack.
use lash_core::facade_support::{
    InMemoryAttachmentStore, SessionAttachmentStore, reclaim_unreferenced_attachments,
};
use lash_core::testing::store_fixtures::session_store_request;
use lash_core::*;
use pretty_assertions::assert_eq;
use std::future::Future;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

#[derive(Default)]
struct FaultingAttachmentStore {
    inner: InMemoryAttachmentStore,
    fail_put: AtomicBool,
    fail_delete: AtomicBool,
    vanish_after_list: AtomicBool,
}

impl FaultingAttachmentStore {
    fn fail_put(&self, fail: bool) {
        self.fail_put.store(fail, Ordering::SeqCst);
    }

    fn fail_delete(&self, fail: bool) {
        self.fail_delete.store(fail, Ordering::SeqCst);
    }

    fn vanish_after_next_list(&self) {
        self.vanish_after_list.store(true, Ordering::SeqCst);
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
        let listed = self.inner.list().await?;
        if self.vanish_after_list.swap(false, Ordering::SeqCst) {
            for blob in &listed {
                self.inner.delete(&blob.id).await?;
            }
        }
        Ok(listed)
    }

    async fn head(&self, id: &AttachmentId) -> Result<Option<StoredBlobRef>, AttachmentStoreError> {
        self.inner.head(id).await
    }
}

struct CoordinatedFailingPutStore {
    inner: InMemoryAttachmentStore,
    fail_first_put: AtomicBool,
    first_put_started: tokio::sync::Notify,
    release_first_put: tokio::sync::Notify,
}

impl Default for CoordinatedFailingPutStore {
    fn default() -> Self {
        Self {
            inner: InMemoryAttachmentStore::new(),
            fail_first_put: AtomicBool::new(true),
            first_put_started: tokio::sync::Notify::new(),
            release_first_put: tokio::sync::Notify::new(),
        }
    }
}

#[async_trait::async_trait]
impl AttachmentStore for CoordinatedFailingPutStore {
    async fn put(
        &self,
        bytes: Vec<u8>,
        meta: AttachmentCreateMeta,
    ) -> Result<AttachmentRef, AttachmentStoreError> {
        if self.fail_first_put.swap(false, Ordering::SeqCst) {
            self.first_put_started.notify_one();
            self.release_first_put.notified().await;
            return Err(AttachmentStoreError::Backend {
                operation: "put",
                class: AttachmentStoreFailureClass::Transient,
                source: "scripted first attachment put failure".into(),
            });
        }
        self.inner.put(bytes, meta).await
    }

    async fn get(&self, id: &AttachmentId) -> Result<StoredAttachment, AttachmentStoreError> {
        self.inner.get(id).await
    }

    async fn delete(&self, id: &AttachmentId) -> Result<(), AttachmentStoreError> {
        self.inner.delete(id).await
    }

    async fn list(&self) -> Result<Vec<StoredBlobRef>, AttachmentStoreError> {
        self.inner.list().await
    }

    async fn head(&self, id: &AttachmentId) -> Result<Option<StoredBlobRef>, AttachmentStoreError> {
        self.inner.head(id).await
    }
}

struct PausedCondemnationRoot {
    inner: Arc<dyn SessionStoreFactory>,
    condemned: tokio::sync::Notify,
    resume: tokio::sync::Notify,
}

impl PausedCondemnationRoot {
    fn new(inner: Arc<dyn SessionStoreFactory>) -> Self {
        Self {
            inner,
            condemned: tokio::sync::Notify::new(),
            resume: tokio::sync::Notify::new(),
        }
    }
}

#[async_trait::async_trait]
impl AttachmentRootSet for PausedCondemnationRoot {
    async fn live_attachment_refs(
        &self,
        intent_grace_cutoff_epoch_ms: u64,
    ) -> Result<std::collections::BTreeSet<AttachmentId>, StoreError> {
        self.inner
            .live_attachment_refs(intent_grace_cutoff_epoch_ms)
            .await
    }

    fn fence(&self) -> AttachmentGcFence {
        self.inner.fence()
    }

    async fn condemn_attachment(
        &self,
        id: &AttachmentId,
        intent_grace_cutoff_epoch_ms: u64,
    ) -> Result<AttachmentCondemnation, StoreError> {
        let outcome = self
            .inner
            .condemn_attachment(id, intent_grace_cutoff_epoch_ms)
            .await?;
        if outcome == AttachmentCondemnation::Condemned {
            self.condemned.notify_one();
            self.resume.notified().await;
        }
        Ok(outcome)
    }

    async fn arm_attachment_delete(
        &self,
        id: &AttachmentId,
    ) -> Result<AttachmentDeleteArming, StoreError> {
        self.inner.arm_attachment_delete(id).await
    }

    async fn release_attachment_condemnation(&self, id: &AttachmentId) -> Result<(), StoreError> {
        self.inner.release_attachment_condemnation(id).await
    }

    async fn retire_attachment_condemnation(&self, id: &AttachmentId) -> Result<(), StoreError> {
        self.inner.retire_attachment_condemnation(id).await
    }

    async fn has_live_attachment_ref(
        &self,
        id: &AttachmentId,
        intent_grace_cutoff_epoch_ms: u64,
    ) -> Result<bool, StoreError> {
        self.inner
            .has_live_attachment_ref(id, intent_grace_cutoff_epoch_ms)
            .await
    }
}

fn state(id: &str) -> RuntimeSessionState {
    let req = session_store_request(&SessionId::from(id), "probe", SessionRelation::Root);
    let mut state = RuntimeSessionState {
        session_id: id.into(),
        ..RuntimeSessionState::new(req.policy)
    };
    state.ensure_agent_frame_initialized();
    state
}
fn with_image(state: &mut RuntimeSessionState, reference: &AttachmentRef) {
    state.session_graph.append_message(Message {
        id: format!("image-{}", state.session_graph.nodes.len()),
        role: MessageRole::User,
        origin: None,
        parts: Arc::new(vec![Part::attachment_part(
            "image-part".into(),
            String::new(),
            Some(lash_sansio::PartAttachment {
                source: AttachmentSource::Stored {
                    attachment_ref: reference.clone(),
                },
            }),
        )]),
    });
}
async fn create(f: &Arc<dyn SessionStoreFactory>, id: &str) -> Arc<dyn RuntimePersistence> {
    f.create_store(&session_store_request(
        &SessionId::from(id),
        "probe",
        SessionRelation::Root,
    ))
    .await
    .unwrap()
}

/// Prove abandoned-writer recovery through destruction and reconstruction of
/// the backend's factory authority. The caller's `reopen` must create a new
/// factory over the same durable catalog after the initial factory and session
/// handle have been dropped.
pub async fn abandoned_attachment_write_recovery_after_cold_reopen<R, Fut>(
    initial_factory: Arc<dyn SessionStoreFactory>,
    reopen: R,
) where
    R: FnOnce() -> Fut,
    Fut: Future<Output = Arc<dyn SessionStoreFactory>>,
{
    assert_eq!(
        initial_factory.fence(),
        AttachmentGcFence::Fenced,
        "cold recovery requires a fenced durable authority"
    );
    let namespace = uuid::Uuid::new_v4();
    let session_id = SessionId::from(format!("cold-condemned-recovery-writer-{namespace}"));
    let adopter_id = SessionId::from(format!("cold-recovery-adopter-{namespace}"));
    let request = session_store_request(&session_id, "probe", SessionRelation::Root);
    let store = initial_factory.create_store(&request).await.unwrap();
    let backend: Arc<dyn AttachmentStore> = Arc::new(InMemoryAttachmentStore::new());
    let payload = format!("cold-recovery-payload-{namespace}").into_bytes();
    let attachment_id = lash_core::attachments::content_id(&payload);
    let survivor_id =
        lash_core::attachments::content_id(format!("recovery-survivor-{namespace}").as_bytes());
    let committed_id =
        lash_core::attachments::content_id(format!("recovery-committed-{namespace}").as_bytes());
    let survivor_intent = write_intent(&session_id, &survivor_id);
    record_completed_write(&store, &survivor_intent);
    record_completed_write(&store, &write_intent(&session_id, &committed_id));
    store
        .commit_refs(&session_id, std::slice::from_ref(&committed_id))
        .expect("commit unrelated attachment root");

    assert_eq!(
        initial_factory
            .condemn_attachment(&attachment_id, 0)
            .await
            .unwrap(),
        AttachmentCondemnation::Condemned
    );
    backend.put(payload.clone(), image_meta()).await.unwrap();
    let abandoned_intent = write_intent(&session_id, &attachment_id);
    let stale_permit = match store
        .begin_attachment_write(abandoned_intent.clone())
        .expect("abandoned writer claims Condemned")
    {
        AttachmentWriteFence::Granted(permit) => permit,
        AttachmentWriteFence::ReclamationInFlight => panic!("first restoring writer must win"),
    };
    let before_reopen = store.list_uncommitted(u64::MAX).unwrap();
    assert!(
        before_reopen.iter().any(|entry| {
            entry.session_id == session_id && entry.attachment_id == attachment_id
        }),
        "the abandoned intent must be durable before factory closure"
    );
    assert!(
        before_reopen
            .iter()
            .any(|entry| { entry.session_id == session_id && entry.attachment_id == survivor_id }),
        "the unrelated intent must be durable before factory closure"
    );

    drop(store);
    drop(initial_factory);
    let reopened_factory = reopen().await;
    let reopened = reopened_factory
        .open_existing_store(&request)
        .await
        .expect("reopen session after factory reconstruction")
        .expect("cold-reopened session exists");
    let before_recovery = reopened.list_uncommitted(u64::MAX).unwrap();
    assert!(
        before_recovery.iter().any(|entry| {
            entry.session_id == session_id && entry.attachment_id == attachment_id
        }),
        "cold reopen must retain the claim-associated abandoned intent"
    );

    reopened_factory
        .recover_abandoned_attachment_write(&attachment_id)
        .await
        .expect("recover abandoned writer after cold reopen");
    let after_recovery = reopened.list_uncommitted(u64::MAX).unwrap();
    assert!(
        after_recovery.iter().all(|entry| {
            entry.session_id != session_id || entry.attachment_id != attachment_id
        }),
        "recovery must delete the abandoned attempt's uncommitted intent"
    );
    assert!(
        after_recovery
            .iter()
            .any(|entry| { entry.session_id == session_id && entry.attachment_id == survivor_id }),
        "recovery must retain unrelated uncommitted intents"
    );
    assert!(
        reopened_factory
            .live_attachment_refs(u64::MAX)
            .await
            .unwrap()
            .contains(&committed_id),
        "recovery must retain unrelated committed roots"
    );
    reopened
        .abort_attachment_write(&abandoned_intent, stale_permit)
        .expect("the pre-reopen permit is stale after recovery");

    let scoped =
        SessionAttachmentStore::new(Arc::clone(&backend), reopened.clone(), session_id.clone());
    let restored = scoped
        .put(payload.clone(), image_meta())
        .await
        .expect("fresh re-put succeeds after recovery");
    assert_eq!(restored.id, attachment_id);
    assert_eq!(
        scoped.get(&attachment_id).await.unwrap().bytes,
        payload,
        "the successful re-put is immediately readable"
    );

    let adopter = create(&reopened_factory, adopter_id.as_str()).await;
    adopter
        .commit_refs(&adopter_id, std::slice::from_ref(&attachment_id))
        .expect("another session adopts the restored bytes");
    assert!(
        reopened_factory
            .live_attachment_refs(u64::MAX)
            .await
            .unwrap()
            .contains(&attachment_id),
        "successful cross-session adoption is a live root"
    );
}
async fn put(
    store: Arc<dyn RuntimePersistence>,
    bytes: Arc<dyn AttachmentStore>,
    id: &str,
    n: u8,
) -> AttachmentRef {
    SessionAttachmentStore::new(bytes, store, id)
        .put(
            [id.as_bytes(), &[n]].concat(),
            AttachmentCreateMeta::new(MediaType::parse("image/png").unwrap(), None, None),
        )
        .await
        .unwrap()
}
async fn sweep(f: &Arc<dyn SessionStoreFactory>, bytes: &Arc<dyn AttachmentStore>) -> usize {
    reclaim_unreferenced_attachments(
        f.as_ref(),
        bytes.as_ref(),
        AttachmentReclamationPolicy {
            grace_period_ms: 0,
            empty_root_set: EmptyRootSetPolicy::AuthorizeDeleteAll,
        },
    )
    .await
    .unwrap()
    .reclaimed_count
}
pub async fn cross_owner_attachment_adoption_conformance(f: Arc<dyn SessionStoreFactory>) {
    let namespace = uuid::Uuid::new_v4();
    let owner_id = format!("adoption-owner-{namespace}");
    let receiver_id = format!("adoption-receiver-{namespace}");
    let bytes: Arc<dyn AttachmentStore> = Arc::new(InMemoryAttachmentStore::new());
    let owner = create(&f, &owner_id).await;
    let live = create(&f, &receiver_id).await;
    let r = put(owner.clone(), bytes.clone(), &owner_id, 99).await;
    let mut st = state(&owner_id);
    with_image(&mut st, &r);
    let mut c = RuntimeCommit::persisted_state_for_test(&st, &[]);
    c.committed_attachment_ids = vec![r.id.clone()];
    owner.commit_runtime_state(c).await.unwrap();
    let reader = SessionAttachmentStore::new(bytes.clone(), live.clone(), &receiver_id);
    assert!(reader.get(&r.id).await.is_ok());
    let mut live_state = state(&receiver_id);
    with_image(&mut live_state, &r);
    let mut c = RuntimeCommit::persisted_state_for_test(&live_state, &[]);
    c.committed_attachment_ids = vec![r.id.clone()];
    // The receiver adopts while the uploader's evidence still stands: adoption
    // copies that evidence onto the receiver's row, so the uploader's session
    // can then be deleted without making the digest unadoptable or collectable.
    let (commit, reclaim) = tokio::join!(
        live.commit_runtime_state(c),
        f.live_attachment_refs(u64::MAX)
    );
    commit.unwrap();
    reclaim.unwrap();
    f.delete_session(&SessionId::from(owner_id)).await.unwrap();
    let removed = sweep(&f, &bytes).await;
    assert_eq!(removed, 0, "receiver owns a committed attachment root");
    let loaded = live.load_session().await.unwrap().unwrap();
    assert!(
        serde_json::to_string(&loaded.graph)
            .unwrap()
            .contains(r.id.as_str())
    );
    assert!(
        reader.get(&r.id).await.is_ok(),
        "committed live graph references missing bytes; GC removed {removed}"
    );
    f.delete_session(&SessionId::from(receiver_id))
        .await
        .unwrap();
    assert_eq!(
        sweep(&f, &bytes).await,
        1,
        "last receiver deletion releases the root"
    );
    adoption_fence_and_rollback(f.clone()).await;
    adoption_after_full_gc_and_release_is_refused(f.clone()).await;
    reput_after_full_gc_allows_adoption(f.clone()).await;
    out_of_band_absence_leaves_no_adoptable_evidence(f.clone()).await;
    failed_delete_releases_the_digest_for_a_fresh_put(f.clone()).await;
    failed_reput_restores_prior_phase(f.clone()).await;
    competing_writer_survives_failed_reput(f.clone()).await;
    sweep_cannot_overwrite_failed_reput_rollback(f.clone()).await;
    stale_sweep_release_cannot_revoke_restoring_writer(f.clone()).await;
    abandoned_writer_recovery_preserves_phase_and_unstrands_reput(f.clone()).await;
    stale_writer_abort_cannot_clobber_a_newer_delete(f.clone()).await;
    committed_restoring_settlement_preserves_root(f.clone()).await;
    // FIG-2795: adoption is gated on positive upload evidence.
    failed_reput_leaves_the_intent_unstamped_and_unadoptable(f.clone()).await;
    stale_permit_cannot_certify_an_upload(f.clone()).await;
    evidence_survives_the_uploaders_forgotten_intent(f.clone()).await;
    abort_after_a_foreign_adoption_preserves_that_root(f.clone()).await;
    duplicate_put_preserves_stamp_and_commitment(f.clone()).await;
    batch_commit_with_one_unknown_digest_writes_nothing(f.clone()).await;
    sweep_adoption_race(f.clone()).await;
    sweep_reput_race(f).await;
}

/// Shared host-facing condemnation-enumeration law.
pub async fn attachment_condemnation_enumeration_conformance(f: Arc<dyn SessionStoreFactory>) {
    let namespace = uuid::Uuid::new_v4();
    let session_id = SessionId::from(format!("condemnation-list-owner-{namespace}"));
    let store = f
        .create_store(&session_store_request(
            &session_id,
            "condemnation-list",
            SessionRelation::Root,
        ))
        .await
        .expect("create condemnation-list session");
    let id = |suffix: &str| {
        AttachmentId::parse(format!("condemnation-list-{namespace}-{suffix}"))
            .expect("valid generated attachment id")
    };
    // These suffixes deliberately mix ASCII punctuation and case. Database
    // locale collation can order them differently from `AttachmentId::Ord`.
    let condemned = id("_condemned");
    let deleting = id("Z-deleting");
    let retired = id("!retired");
    let restoring_condemned = id("a-restoring-condemned");

    for digest in [&retired, &condemned, &deleting, &restoring_condemned] {
        assert_eq!(
            f.condemn_attachment(digest, u64::MAX).await.unwrap(),
            AttachmentCondemnation::Condemned
        );
    }
    assert_eq!(
        f.arm_attachment_delete(&deleting).await.unwrap(),
        AttachmentDeleteArming::Armed
    );
    assert_eq!(
        f.arm_attachment_delete(&retired).await.unwrap(),
        AttachmentDeleteArming::Armed
    );
    // A completed delete deletes the row: there is no terminal phase left for
    // enumeration to report.
    f.retire_attachment_condemnation(&retired).await.unwrap();
    let restoring_intent = |digest: &AttachmentId| AttachmentIntent {
        attachment_id: digest.clone(),
        session_id: session_id.clone(),
        canonical_uri: format!("lash-attachment://blake3/{digest}"),
        intent_at_epoch_ms: 1,
        owner: None,
    };
    assert!(matches!(
        store
            .begin_attachment_write(restoring_intent(&restoring_condemned))
            .expect("restoring writer claims condemnation"),
        AttachmentWriteFence::Granted(_)
    ));

    let mut expected = vec![
        AttachmentCondemnationRecord {
            digest: condemned.clone(),
            phase: AttachmentCondemnationPhase::Condemned,
            provenance: AttachmentCondemnationProvenance::SweepOwned,
        },
        AttachmentCondemnationRecord {
            digest: deleting.clone(),
            phase: AttachmentCondemnationPhase::Deleting,
            provenance: AttachmentCondemnationProvenance::SweepOwned,
        },
        AttachmentCondemnationRecord {
            digest: restoring_condemned.clone(),
            phase: AttachmentCondemnationPhase::Condemned,
            provenance: AttachmentCondemnationProvenance::RestoringWrite {
                session_id: session_id.clone(),
            },
        },
    ];
    expected.sort_by(|left, right| left.digest.cmp(&right.digest));
    assert_eq!(
        f.list_condemnations().await.unwrap(),
        expected,
        "a retired condemnation leaves no row behind"
    );

    for digest in [&condemned, &deleting, &retired, &restoring_condemned] {
        f.release_attachment_condemnation(digest).await.unwrap();
    }
    expected.retain(|row| {
        matches!(
            row.provenance,
            AttachmentCondemnationProvenance::RestoringWrite { .. }
        )
    });
    assert_eq!(
        f.list_condemnations().await.unwrap(),
        expected,
        "manual release clears only unclaimed Condemned/Deleting rows"
    );
}

struct StopBeforeCondemnationReclaim {
    inner: Arc<dyn SessionStoreFactory>,
    reclaim_reached: tokio::sync::Notify,
}

#[async_trait::async_trait]
impl AttachmentRootSet for StopBeforeCondemnationReclaim {
    fn can_prove_process_owner_death(&self) -> bool {
        self.inner.can_prove_process_owner_death()
    }

    async fn live_attachment_refs(
        &self,
        cutoff: u64,
    ) -> Result<std::collections::BTreeSet<AttachmentId>, StoreError> {
        self.inner.live_attachment_refs(cutoff).await
    }

    async fn list_condemnations(&self) -> Result<Vec<AttachmentCondemnationRecord>, StoreError> {
        self.inner.list_condemnations().await
    }

    async fn has_live_attachment_ref(
        &self,
        id: &AttachmentId,
        cutoff: u64,
    ) -> Result<bool, StoreError> {
        self.inner.has_live_attachment_ref(id, cutoff).await
    }

    fn fence(&self) -> AttachmentGcFence {
        self.inner.fence()
    }

    async fn condemn_attachment(
        &self,
        id: &AttachmentId,
        cutoff: u64,
    ) -> Result<AttachmentCondemnation, StoreError> {
        self.inner.condemn_attachment(id, cutoff).await
    }

    async fn arm_attachment_delete(
        &self,
        id: &AttachmentId,
    ) -> Result<AttachmentDeleteArming, StoreError> {
        self.inner.arm_attachment_delete(id).await
    }

    async fn release_attachment_condemnation(&self, id: &AttachmentId) -> Result<(), StoreError> {
        self.inner.release_attachment_condemnation(id).await
    }

    async fn retire_attachment_condemnation(&self, _id: &AttachmentId) -> Result<(), StoreError> {
        self.reclaim_reached.notify_one();
        std::future::pending().await
    }
}

/// Real GC regression for the crash window after bytes are deleted but before
/// the `Deleting` condemnation row is retired.
pub async fn attachment_condemnation_delete_crash_survives_cold_reopen<Reopen, ReopenFuture>(
    factory: Arc<dyn SessionStoreFactory>,
    reopen: Reopen,
) where
    Reopen: FnOnce() -> ReopenFuture,
    ReopenFuture: Future<Output = Arc<dyn SessionStoreFactory>>,
{
    let namespace = uuid::Uuid::new_v4();
    let session_id = SessionId::from(format!("condemnation-crash-{namespace}"));
    factory
        .create_store(&session_store_request(
            &session_id,
            "condemnation-crash",
            SessionRelation::Root,
        ))
        .await
        .expect("materialize durable condemnation catalog");
    let backend = Arc::new(InMemoryAttachmentStore::new());
    let reference = backend
        .put(
            format!("condemnation-crash-{namespace}").into_bytes(),
            AttachmentCreateMeta::new(
                MediaType::parse("application/octet-stream").unwrap(),
                None,
                None,
            ),
        )
        .await
        .expect("seed unreferenced physical blob");
    let interrupted = Arc::new(StopBeforeCondemnationReclaim {
        inner: Arc::clone(&factory),
        reclaim_reached: tokio::sync::Notify::new(),
    });
    let sweep_root = Arc::clone(&interrupted);
    let sweep_backend = Arc::clone(&backend);
    let sweep = tokio::spawn(async move {
        reclaim_unreferenced_attachments(
            sweep_root.as_ref(),
            sweep_backend.as_ref(),
            AttachmentReclamationPolicy {
                grace_period_ms: 0,
                empty_root_set: EmptyRootSetPolicy::AuthorizeDeleteAll,
            },
        )
        .await
    });
    tokio::time::timeout(
        std::time::Duration::from_secs(5),
        interrupted.reclaim_reached.notified(),
    )
    .await
    .expect("GC reaches the post-delete, pre-reclaim stop point");
    let expected = vec![AttachmentCondemnationRecord {
        digest: reference.id.clone(),
        phase: AttachmentCondemnationPhase::Deleting,
        provenance: AttachmentCondemnationProvenance::SweepOwned,
    }];
    assert_eq!(
        factory.list_condemnations().await.unwrap(),
        expected,
        "precondition: durable authority remains Deleting while reclaim is blocked"
    );
    sweep.abort();
    let cancellation = tokio::time::timeout(std::time::Duration::from_secs(5), sweep)
        .await
        .expect("aborted GC sweep terminates promptly");
    assert!(
        matches!(cancellation, Err(ref error) if error.is_cancelled()),
        "aborted GC sweep must report cancellation: {cancellation:?}"
    );
    let physical = backend.list().await.expect("enumerate physical backend");
    assert!(
        physical.is_empty(),
        "physical delete completed before the scripted stop"
    );
    drop(interrupted);
    drop(factory);

    let reopened = reopen().await;
    assert_eq!(reopened.list_condemnations().await.unwrap(), expected);

    let backend_derived = physical.into_iter().map(|blob| blob.id).collect::<Vec<_>>();
    assert_ne!(
        backend_derived,
        vec![reference.id],
        "negative control: backend.list cannot discover an orphaned Deleting authority row once bytes are absent"
    );
}

async fn out_of_band_absence_leaves_no_adoptable_evidence(f: Arc<dyn SessionStoreFactory>) {
    let namespace = uuid::Uuid::new_v4();
    let session_id = SessionId::from(format!("absent-head-receiver-{namespace}"));
    let store = create(&f, &session_id).await;
    let backend = Arc::new(FaultingAttachmentStore::default());
    let payload = format!("absent-head-{namespace}").into_bytes();
    let reference = backend.put(payload, image_meta()).await.unwrap();
    backend.vanish_after_next_list();

    let report = sweep(&f, &(backend.clone() as Arc<dyn AttachmentStore>)).await;
    assert_eq!(report, 0, "already-absent bytes are not counted as deleted");
    assert!(matches!(
        backend.get(&reference.id).await,
        Err(AttachmentStoreError::NotFound(_))
    ));
    let error = store
        .commit_refs(&session_id, std::slice::from_ref(&reference.id))
        .expect_err("bytes nobody uploaded through this store are not adoptable");
    assert!(matches!(
        error,
        StoreError::UnknownAttachment { ref digest } if digest == &reference.id
    ));
}

async fn failed_delete_releases_the_digest_for_a_fresh_put(f: Arc<dyn SessionStoreFactory>) {
    let namespace = uuid::Uuid::new_v4();
    let session_id = SessionId::from(format!("failed-delete-receiver-{namespace}"));
    let store = create(&f, &session_id).await;
    let backend = Arc::new(FaultingAttachmentStore::default());
    let reference = backend
        .put(
            format!("failed-delete-{namespace}").into_bytes(),
            image_meta(),
        )
        .await
        .unwrap();
    record_completed_write(&store, &write_intent(&session_id, &reference.id));
    backend.fail_delete(true);
    let report = reclaim_unreferenced_attachments(
        f.as_ref(),
        backend.as_ref(),
        AttachmentReclamationPolicy {
            grace_period_ms: 0,
            empty_root_set: EmptyRootSetPolicy::AuthorizeDeleteAll,
        },
    )
    .await
    .unwrap();
    assert_eq!(report.failed_ids, vec![reference.id.clone()]);
    assert!(
        backend.get(&reference.id).await.is_ok(),
        "a failed delete leaves the bytes in place"
    );
    // Condemnation cleared the manifest evidence under the same fence, so the
    // surviving bytes are not adoptable until somebody puts them again.
    let error = store
        .commit_refs(&session_id, std::slice::from_ref(&reference.id))
        .expect_err("condemnation cleared this digest's upload evidence");
    assert!(matches!(
        error,
        StoreError::UnknownAttachment { ref digest } if digest == &reference.id
    ));
    record_completed_write(&store, &write_intent(&session_id, &reference.id));
    store
        .commit_refs(&session_id, std::slice::from_ref(&reference.id))
        .expect("a fresh completed write restores adoptability");
    assert!(
        f.has_live_attachment_ref(&reference.id, u64::MAX)
            .await
            .unwrap()
    );
}

async fn failed_reput_restores_prior_phase(f: Arc<dyn SessionStoreFactory>) {
    let namespace = uuid::Uuid::new_v4();
    let session_id = SessionId::from(format!("failed-reput-{namespace}"));
    let store = create(&f, &session_id).await;
    let backend = Arc::new(FaultingAttachmentStore::default());
    let scoped = SessionAttachmentStore::new(
        backend.clone() as Arc<dyn AttachmentStore>,
        store.clone(),
        session_id.clone(),
    );

    let condemned_payload = format!("condemned-reput-{namespace}").into_bytes();
    let condemned = backend
        .put(condemned_payload.clone(), image_meta())
        .await
        .unwrap();
    assert_eq!(
        f.condemn_attachment(&condemned.id, 0).await.unwrap(),
        AttachmentCondemnation::Condemned
    );
    backend.fail_put(true);
    scoped
        .put(condemned_payload.clone(), image_meta())
        .await
        .expect_err("scripted condemned re-put fails");
    assert_eq!(
        f.arm_attachment_delete(&condemned.id).await.unwrap(),
        AttachmentDeleteArming::Armed,
        "failed re-put restores Condemned rather than Free"
    );
    let error = store
        .commit_refs(&session_id, std::slice::from_ref(&condemned.id))
        .expect_err("a failed re-put certifies no upload");
    assert!(matches!(
        error,
        StoreError::UnknownAttachment { ref digest } if digest == &condemned.id
    ));
    f.release_attachment_condemnation(&condemned.id)
        .await
        .unwrap();

    backend.fail_put(false);
    let restored = scoped
        .put(condemned_payload, image_meta())
        .await
        .expect("the retried re-put succeeds");
    assert_eq!(restored.id, condemned.id);
    store
        .commit_refs(&session_id, std::slice::from_ref(&condemned.id))
        .expect("the successful re-put makes the digest adoptable again");
}

async fn competing_writer_survives_failed_reput(f: Arc<dyn SessionStoreFactory>) {
    let namespace = uuid::Uuid::new_v4();
    let first_id = SessionId::from(format!("failed-writer-{namespace}"));
    let second_id = SessionId::from(format!("successful-writer-{namespace}"));
    let first_store = create(&f, &first_id).await;
    let second_store = create(&f, &second_id).await;
    let backend = Arc::new(CoordinatedFailingPutStore::default());
    let payload = format!("competing-reput-{namespace}").into_bytes();
    let attachment_id = lash_core::attachments::content_id(&payload);
    assert_eq!(
        f.condemn_attachment(&attachment_id, 0).await.unwrap(),
        AttachmentCondemnation::Condemned
    );
    assert_eq!(
        f.arm_attachment_delete(&attachment_id).await.unwrap(),
        AttachmentDeleteArming::Armed
    );
    f.retire_attachment_condemnation(&attachment_id)
        .await
        .unwrap();

    let first = Arc::new(SessionAttachmentStore::new(
        backend.clone() as Arc<dyn AttachmentStore>,
        first_store,
        first_id,
    ));
    let second = Arc::new(SessionAttachmentStore::new(
        backend.clone() as Arc<dyn AttachmentStore>,
        second_store.clone(),
        second_id.clone(),
    ));
    let first_task = {
        let first = Arc::clone(&first);
        let payload = payload.clone();
        tokio::spawn(async move { first.put(payload, image_meta()).await })
    };
    backend.first_put_started.notified().await;
    let second_task = {
        let second = Arc::clone(&second);
        let payload = payload.clone();
        tokio::spawn(async move { second.put(payload, image_meta()).await })
    };
    tokio::task::yield_now().await;
    backend.release_first_put.notify_one();
    first_task
        .await
        .unwrap()
        .expect_err("first restoring writer fails");
    let restored = second_task
        .await
        .unwrap()
        .expect("competing writer retries and restores bytes");
    assert_eq!(restored.id, attachment_id);
    second_store
        .commit_refs(&second_id, std::slice::from_ref(&attachment_id))
        .expect("failed writer rollback does not clobber successful writer");
    assert!(backend.get(&attachment_id).await.is_ok());
}

async fn sweep_cannot_overwrite_failed_reput_rollback(f: Arc<dyn SessionStoreFactory>) {
    let namespace = uuid::Uuid::new_v4();
    let session_id = SessionId::from(format!("sweep-reput-race-{namespace}"));
    let store = create(&f, &session_id).await;
    let backend = Arc::new(CoordinatedFailingPutStore::default());
    let payload = format!("sweep-reput-race-{namespace}").into_bytes();
    let reference = backend
        .inner
        .put(payload.clone(), image_meta())
        .await
        .unwrap();
    assert_eq!(
        f.condemn_attachment(&reference.id, 0).await.unwrap(),
        AttachmentCondemnation::Condemned
    );
    let scoped = Arc::new(SessionAttachmentStore::new(
        backend.clone() as Arc<dyn AttachmentStore>,
        store,
        session_id,
    ));
    let writer = {
        let scoped = Arc::clone(&scoped);
        tokio::spawn(async move { scoped.put(payload, image_meta()).await })
    };
    backend.first_put_started.notified().await;
    let report = reclaim_unreferenced_attachments(
        f.as_ref(),
        backend.as_ref(),
        AttachmentReclamationPolicy {
            grace_period_ms: 0,
            empty_root_set: EmptyRootSetPolicy::AuthorizeDeleteAll,
        },
    )
    .await
    .unwrap();
    assert_eq!(report.condemn_deferred_ids, vec![reference.id.clone()]);
    backend.release_first_put.notify_one();
    writer
        .await
        .unwrap()
        .expect_err("scripted restoring writer fails");
    assert_eq!(
        f.arm_attachment_delete(&reference.id).await.unwrap(),
        AttachmentDeleteArming::Armed,
        "the stale sweep did not overwrite the writer's Condemned rollback"
    );
}

async fn stale_sweep_release_cannot_revoke_restoring_writer(f: Arc<dyn SessionStoreFactory>) {
    let namespace = uuid::Uuid::new_v4();
    let session_id = SessionId::from(format!("stale-sweep-release-{namespace}"));
    let store = create(&f, &session_id).await;
    let attachment_id =
        lash_core::attachments::content_id(format!("stale-sweep-release-{namespace}").as_bytes());
    assert_eq!(
        f.condemn_attachment(&attachment_id, 0).await.unwrap(),
        AttachmentCondemnation::Condemned
    );
    let intent = write_intent(&session_id, &attachment_id);
    let permit = match store
        .begin_attachment_write(intent.clone())
        .expect("restoring writer claims Condemned")
    {
        AttachmentWriteFence::Granted(permit) => permit,
        AttachmentWriteFence::ReclamationInFlight => panic!("first restoring writer must win"),
    };

    // Model an older sweep abandoning the condemnation after the writer won.
    // Ordinary sweep release has no writer-recovery authority and must be a
    // no-op against the token-owned phase.
    f.release_attachment_condemnation(&attachment_id)
        .await
        .expect("stale sweep release");
    assert_eq!(
        f.arm_attachment_delete(&attachment_id).await.unwrap(),
        AttachmentDeleteArming::Revoked,
        "stale sweep release must not make an active writer's Condemned phase armable"
    );
    assert!(matches!(
        store
            .begin_attachment_write(intent.clone())
            .expect("same-owner competing write observes the active claim"),
        AttachmentWriteFence::ReclamationInFlight
    ));

    store
        .abort_attachment_write(&intent, permit)
        .expect("active writer abort restores Condemned");
    assert_eq!(
        f.arm_attachment_delete(&attachment_id).await.unwrap(),
        AttachmentDeleteArming::Armed,
        "the owning writer, not a stale sweep, settles its token"
    );
    f.release_attachment_condemnation(&attachment_id)
        .await
        .unwrap();
}

async fn abandoned_writer_recovery_preserves_phase_and_unstrands_reput(
    f: Arc<dyn SessionStoreFactory>,
) {
    let namespace = uuid::Uuid::new_v4();
    let session_id = SessionId::from(format!("abandoned-condemned-writer-{namespace}"));
    let adopter_id = SessionId::from(format!("recovery-adopter-{namespace}"));
    let payload = format!("recovery-payload-{namespace}").into_bytes();
    let backend: Arc<dyn AttachmentStore> = Arc::new(InMemoryAttachmentStore::new());
    let attachment_id = backend.put(payload.clone(), image_meta()).await.unwrap().id;
    assert_eq!(
        f.condemn_attachment(&attachment_id, 0).await.unwrap(),
        AttachmentCondemnation::Condemned
    );

    let intent = write_intent(&session_id, &attachment_id);
    let crashed_store = create(&f, &session_id).await;
    let stale_permit = match crashed_store
        .begin_attachment_write(intent.clone())
        .expect("writer claims the prior phase")
    {
        AttachmentWriteFence::Granted(permit) => permit,
        AttachmentWriteFence::ReclamationInFlight => panic!("first writer must win"),
    };
    assert!(
        crashed_store
            .list_uncommitted(u64::MAX)
            .unwrap()
            .iter()
            .any(|entry| entry.session_id == session_id && entry.attachment_id == attachment_id),
        "the abandoned attempt must have an associated uncommitted intent"
    );
    assert!(matches!(
        crashed_store
            .begin_attachment_write(intent.clone())
            .expect("same-session concurrent write observes the durable claim"),
        AttachmentWriteFence::ReclamationInFlight
    ));
    drop(crashed_store);

    // The host has established quiescence after crash/cancellation. This
    // shared case reopens the session handle; the backend-specific cold
    // witness reconstructs the whole durable factory.
    f.recover_abandoned_attachment_write(&attachment_id)
        .await
        .expect("recover abandoned restoring writer");
    let reopened = create(&f, &session_id).await;
    assert!(
        reopened
            .list_uncommitted(u64::MAX)
            .unwrap()
            .iter()
            .all(|entry| entry.session_id != session_id || entry.attachment_id != attachment_id),
        "recovery removes the abandoned attempt's uncommitted intent"
    );
    let fresh_permit = match reopened
        .begin_attachment_write(intent.clone())
        .expect("fresh re-put claims the recovered phase")
    {
        AttachmentWriteFence::Granted(permit) => permit,
        AttachmentWriteFence::ReclamationInFlight => {
            panic!("recovered writer claim must not strand the re-put")
        }
    };

    reopened
        .abort_attachment_write(&intent, stale_permit)
        .expect("stale pre-recovery abort is a no-op");
    assert!(matches!(
        reopened
            .begin_attachment_write(intent.clone())
            .expect("stale abort cannot revoke the fresh claim"),
        AttachmentWriteFence::ReclamationInFlight
    ));
    reopened
        .abort_attachment_write(&intent, fresh_permit)
        .expect("fresh abort restores the recovered phase");
    assert_eq!(
        f.arm_attachment_delete(&attachment_id).await.unwrap(),
        AttachmentDeleteArming::Armed,
        "recovery must preserve Condemned"
    );
    f.release_attachment_condemnation(&attachment_id)
        .await
        .unwrap();
    assert_eq!(
        f.condemn_attachment(&attachment_id, 0).await.unwrap(),
        AttachmentCondemnation::Condemned,
        "re-establish Condemned for the successful restoring put"
    );

    let scoped =
        SessionAttachmentStore::new(Arc::clone(&backend), reopened.clone(), session_id.clone());
    let restored = scoped
        .put(payload.clone(), image_meta())
        .await
        .expect("successful re-put after abandoned-writer recovery");
    assert_eq!(restored.id, attachment_id);
    assert_eq!(scoped.get(&attachment_id).await.unwrap().bytes, payload);
    let adopter = create(&f, adopter_id.as_str()).await;
    adopter
        .commit_refs(&adopter_id, std::slice::from_ref(&attachment_id))
        .expect("another session adopts the restored attachment");
}

async fn stale_writer_abort_cannot_clobber_a_newer_delete(f: Arc<dyn SessionStoreFactory>) {
    let namespace = uuid::Uuid::new_v4();
    let session_id = SessionId::from(format!("stale-writer-sweep-{namespace}"));
    let store = create(&f, &session_id).await;
    let attachment_id =
        lash_core::attachments::content_id(format!("stale-writer-sweep-{namespace}").as_bytes());
    assert_eq!(
        f.condemn_attachment(&attachment_id, 0).await.unwrap(),
        AttachmentCondemnation::Condemned
    );
    let intent = write_intent(&session_id, &attachment_id);
    let permit = match store
        .begin_attachment_write(intent.clone())
        .expect("first writer claims Condemned")
    {
        AttachmentWriteFence::Granted(permit) => permit,
        AttachmentWriteFence::ReclamationInFlight => {
            panic!("unowned Condemned must grant the first writer")
        }
    };

    // Host recovery makes the first permit stale while preserving its phase.
    // A newer sweep then owns the phase through an armed physical delete.
    f.recover_abandoned_attachment_write(&attachment_id)
        .await
        .unwrap();
    assert_eq!(
        f.arm_attachment_delete(&attachment_id).await.unwrap(),
        AttachmentDeleteArming::Armed,
        "recovery clears only the abandoned claim, preserving Condemned"
    );

    store
        .abort_attachment_write(&intent, permit)
        .expect("stale writer abort is a no-op");
    assert!(
        f.list_condemnations()
            .await
            .unwrap()
            .contains(&AttachmentCondemnationRecord {
                digest: attachment_id.clone(),
                phase: AttachmentCondemnationPhase::Deleting,
                provenance: AttachmentCondemnationProvenance::SweepOwned,
            }),
        "a stale rollback must not revoke a newer armed delete"
    );
    let error = store
        .commit_refs(&session_id, std::slice::from_ref(&attachment_id))
        .expect_err("a digest whose delete is in flight is not adoptable");
    assert!(matches!(
        error,
        StoreError::UnknownAttachment { ref digest } if digest == &attachment_id
    ));
    f.release_attachment_condemnation(&attachment_id)
        .await
        .unwrap();
}

#[derive(Clone, Copy, Debug)]
enum CommittedRestoringSettlement {
    Abort,
    Recover,
}

/// A restoring write can outlive the turn commit that stamps its associated
/// intent. Abort and explicit recovery must retain that root and retire the old
/// unarmed `Condemned` phase before its sweeper can arm.
async fn committed_restoring_settlement_preserves_root(factory: Arc<dyn SessionStoreFactory>) {
    for settlement in [
        CommittedRestoringSettlement::Abort,
        CommittedRestoringSettlement::Recover,
    ] {
        let namespace = uuid::Uuid::new_v4();
        let session_id = SessionId::from(format!(
            "attachment-committed-restoring-{settlement:?}-{namespace}"
        ));
        let turn_id = TurnId::from(format!("attachment-restoring-turn-{namespace}"));
        let request = session_store_request(&session_id, "probe", SessionRelation::Root);
        let store = factory.create_store(&request).await.unwrap();
        let attachment_id = lash_core::attachments::content_id(
            format!("committed restoring {settlement:?} {namespace}").as_bytes(),
        );
        assert_eq!(
            factory.condemn_attachment(&attachment_id, 0).await.unwrap(),
            AttachmentCondemnation::Condemned
        );
        let intent = AttachmentIntent {
            attachment_id: attachment_id.clone(),
            session_id: session_id.clone(),
            canonical_uri: format!("lash-attachment://blake3/{attachment_id}"),
            intent_at_epoch_ms: 0,
            owner: Some(AttachmentOwner::Turn {
                id: turn_id.to_string(),
            }),
        };
        let permit = match store
            .begin_attachment_write(intent.clone())
            .expect("claim the prior phase for a turn-owned restoring write")
        {
            AttachmentWriteFence::Granted(permit) => permit,
            AttachmentWriteFence::ReclamationInFlight => {
                panic!("the first restoring writer must acquire the digest")
            }
        };
        commit_turn_owned_intent(&store, &request, &turn_id, &attachment_id).await;

        match settlement {
            CommittedRestoringSettlement::Abort => store
                .abort_attachment_write(&intent, permit)
                .expect("settle the late matching abort"),
            CommittedRestoringSettlement::Recover => {
                factory
                    .recover_abandoned_attachment_write(&attachment_id)
                    .await
                    .expect("recover the quiescent committed restoring writer");
                store
                    .abort_attachment_write(&intent, permit)
                    .expect("the recovered permit is stale");
            }
        }
        assert!(
            factory
                .live_attachment_refs(u64::MAX)
                .await
                .unwrap()
                .contains(&attachment_id),
            "{settlement:?} must retain the associated committed root"
        );
        assert_eq!(
            factory.arm_attachment_delete(&attachment_id).await.unwrap(),
            AttachmentDeleteArming::Revoked,
            "{settlement:?} must retire Condemned when its intent became committed"
        );
    }

    committed_restoring_abort_survives_the_older_sweep(factory).await;
}

async fn commit_turn_owned_intent(
    store: &Arc<dyn RuntimePersistence>,
    request: &SessionStoreCreateRequest,
    turn_id: &TurnId,
    attachment_id: &AttachmentId,
) {
    let mut state = RuntimeSessionState {
        session_id: request.session_id.clone(),
        ..RuntimeSessionState::new(request.policy.clone())
    };
    state.ensure_agent_frame_initialized();
    let mut commit = RuntimeCommit::persisted_state_for_test(&state, &[]);
    commit.turn_commit = lash_core::store::RuntimeTurnCommitStamp::new(OperationId::turn(
        &request.session_id,
        turn_id,
        "final",
    ));
    let lease = store
        .try_claim_session_execution_lease(
            &request.session_id,
            &LeaseOwnerIdentity::opaque(
                "attachment-conformance",
                format!("committed-restoring-{attachment_id}"),
            ),
            "attachment conformance",
            60_000,
        )
        .await
        .unwrap()
        .acquired()
        .expect("fresh session lease is acquired");
    store
        .commit_runtime_state(commit.releasing_session_execution_lease(lease.completion()))
        .await
        .expect("commit the turn-owned attachment intent");
    assert!(
        store
            .list_uncommitted(u64::MAX)
            .unwrap()
            .iter()
            .all(|entry| entry.attachment_id != *attachment_id),
        "the turn commit must stamp the restoring intent"
    );
}

async fn committed_restoring_abort_survives_the_older_sweep(factory: Arc<dyn SessionStoreFactory>) {
    let namespace = uuid::Uuid::new_v4();
    let session_id = SessionId::from(format!("attachment-committed-sweep-{namespace}"));
    let turn_id = TurnId::from(format!("attachment-committed-sweep-turn-{namespace}"));
    let request = session_store_request(&session_id, "probe", SessionRelation::Root);
    let store = factory.create_store(&request).await.unwrap();
    let backend: Arc<dyn AttachmentStore> = Arc::new(InMemoryAttachmentStore::new());
    let reference = backend
        .put(
            format!("committed restoring sweep {namespace}").into_bytes(),
            image_meta(),
        )
        .await
        .unwrap();
    let attachment_id = reference.id;
    let paused_root = Arc::new(PausedCondemnationRoot::new(factory.clone()));
    let sweep_root = paused_root.clone();
    let sweep_backend = backend.clone();
    let sweep = tokio::spawn(async move {
        reclaim_unreferenced_attachments(
            &*sweep_root,
            &*sweep_backend,
            AttachmentReclamationPolicy {
                grace_period_ms: 0,
                empty_root_set: EmptyRootSetPolicy::AuthorizeDeleteAll,
            },
        )
        .await
    });
    paused_root.condemned.notified().await;

    let intent = AttachmentIntent {
        attachment_id: attachment_id.clone(),
        session_id: session_id.clone(),
        canonical_uri: format!("lash-attachment://blake3/{attachment_id}"),
        intent_at_epoch_ms: 0,
        owner: Some(AttachmentOwner::Turn {
            id: turn_id.to_string(),
        }),
    };
    let permit = match store
        .begin_attachment_write(intent.clone())
        .expect("claim the older sweep's condemnation")
    {
        AttachmentWriteFence::Granted(permit) => permit,
        AttachmentWriteFence::ReclamationInFlight => panic!("restoring writer must win"),
    };
    commit_turn_owned_intent(&store, &request, &turn_id, &attachment_id).await;
    store
        .abort_attachment_write(&intent, permit)
        .expect("abort after the intent became committed");
    paused_root.resume.notify_one();

    let report = sweep.await.expect("join the older sweep").expect("sweep");
    assert_eq!(
        report.reclaimed_count, 0,
        "the rooted blob must not be deleted"
    );
    assert!(
        report.deleted_while_referenced.is_empty(),
        "the fence must prevent rather than merely detect root loss"
    );
    assert!(
        report.condemn_deferred_ids.contains(&attachment_id),
        "the older sweep must defer the digest whose condemnation was superseded"
    );
    assert!(
        factory
            .live_attachment_refs(u64::MAX)
            .await
            .unwrap()
            .contains(&attachment_id),
        "the turn-owned root remains live"
    );
    assert_eq!(
        backend.get(&attachment_id).await.unwrap().bytes,
        format!("committed restoring sweep {namespace}").into_bytes(),
        "the older sweep must leave the rooted bytes readable"
    );
}

fn image_meta() -> AttachmentCreateMeta {
    AttachmentCreateMeta::new(MediaType::parse("image/png").unwrap(), None, None)
}

/// One completed write against the manifest: acquire the fence, then stamp the
/// upload evidence. This is the only way a manifest row is created by a writer,
/// and the only thing that makes a digest adoptable.
fn record_completed_write(store: &Arc<dyn RuntimePersistence>, intent: &AttachmentIntent) {
    let AttachmentWriteFence::Granted(permit) = store
        .begin_attachment_write(intent.clone())
        .expect("begin attachment write")
    else {
        panic!(
            "expected a granted write fence for `{}`",
            intent.attachment_id
        );
    };
    store
        .complete_attachment_write(intent, permit)
        .expect("stamp upload evidence");
}

fn write_intent(session_id: &SessionId, attachment_id: &AttachmentId) -> AttachmentIntent {
    AttachmentIntent {
        attachment_id: attachment_id.clone(),
        session_id: session_id.clone(),
        canonical_uri: format!("lash-attachment://blake3/{attachment_id}"),
        intent_at_epoch_ms: 0,
        owner: None,
    }
}

/// A begun-but-never-completed write leaves an unstamped row, and an unstamped
/// row certifies nothing: the digest stays unadoptable until a put completes.
async fn failed_reput_leaves_the_intent_unstamped_and_unadoptable(f: Arc<dyn SessionStoreFactory>) {
    let namespace = uuid::Uuid::new_v4();
    let session_id = SessionId::from(format!("unstamped-intent-{namespace}"));
    let store = create(&f, &session_id).await;
    let attachment_id =
        lash_core::attachments::content_id(format!("unstamped-intent-{namespace}").as_bytes());
    let intent = write_intent(&session_id, &attachment_id);
    let AttachmentWriteFence::Granted(permit) = store
        .begin_attachment_write(intent.clone())
        .expect("begin the attempt whose upload will fail")
    else {
        panic!("an unclaimed digest must grant the fence");
    };
    let unstamped = store
        .list_uncommitted(u64::MAX)
        .unwrap()
        .into_iter()
        .find(|entry| entry.session_id == session_id && entry.attachment_id == attachment_id)
        .expect("begin records the attempt");
    assert_eq!(
        unstamped.written_at_epoch_ms, None,
        "begin must not stamp upload evidence"
    );
    let error = store
        .commit_refs(&session_id, std::slice::from_ref(&attachment_id))
        .expect_err("an unstamped row is not upload evidence");
    assert!(matches!(
        error,
        StoreError::UnknownAttachment { ref digest } if digest == &attachment_id
    ));
    assert!(
        !f.has_live_attachment_ref(&attachment_id, u64::MAX)
            .await
            .unwrap(),
        "the refused adoption published no root"
    );

    store
        .abort_attachment_write(&intent, permit)
        .expect("the failed upload rolls its own attempt back");
    assert!(
        store
            .list_uncommitted(u64::MAX)
            .unwrap()
            .iter()
            .all(|entry| entry.session_id != session_id || entry.attachment_id != attachment_id),
        "abort removes the unstamped, uncommitted row"
    );
    assert!(matches!(
        store
            .commit_refs(&session_id, std::slice::from_ref(&attachment_id))
            .expect_err("the rolled-back digest is still unadoptable"),
        StoreError::UnknownAttachment { .. }
    ));
}

/// Only the attempt that currently owns the manifest row can certify an upload.
async fn stale_permit_cannot_certify_an_upload(f: Arc<dyn SessionStoreFactory>) {
    let namespace = uuid::Uuid::new_v4();
    let session_id = SessionId::from(format!("stale-permit-{namespace}"));
    let store = create(&f, &session_id).await;
    let attachment_id =
        lash_core::attachments::content_id(format!("stale-permit-{namespace}").as_bytes());
    let intent = write_intent(&session_id, &attachment_id);
    let AttachmentWriteFence::Granted(first) = store
        .begin_attachment_write(intent.clone())
        .expect("first attempt begins")
    else {
        panic!("an unclaimed digest must grant the fence");
    };
    let AttachmentWriteFence::Granted(second) = store
        .begin_attachment_write(intent.clone())
        .expect("a retry supersedes the first attempt")
    else {
        panic!("an unclaimed digest must grant the retry");
    };
    let error = store
        .complete_attachment_write(&intent, first)
        .expect_err("the superseded attempt cannot stamp the row");
    assert!(matches!(
        error,
        StoreError::StaleWritePermit { ref digest } if digest == &attachment_id
    ));
    assert!(matches!(
        store
            .commit_refs(&session_id, std::slice::from_ref(&attachment_id))
            .expect_err("a stale completion leaves the row unstamped"),
        StoreError::UnknownAttachment { .. }
    ));

    store
        .complete_attachment_write(&intent, second)
        .expect("the owning attempt stamps the row");
    store
        .commit_refs(&session_id, std::slice::from_ref(&attachment_id))
        .expect("the stamped row is upload evidence");
    assert!(
        f.has_live_attachment_ref(&attachment_id, u64::MAX)
            .await
            .unwrap()
    );
}

/// Upload evidence is a property of the digest, not of the uploader's row: an
/// adopter's copied stamp keeps the digest adoptable after the uploader is gone.
async fn evidence_survives_the_uploaders_forgotten_intent(f: Arc<dyn SessionStoreFactory>) {
    let namespace = uuid::Uuid::new_v4();
    let uploader_id = SessionId::from(format!("evidence-uploader-{namespace}"));
    let adopter_id = SessionId::from(format!("evidence-adopter-{namespace}"));
    let late_id = SessionId::from(format!("evidence-late-adopter-{namespace}"));
    let uploader = create(&f, uploader_id.as_str()).await;
    let adopter = create(&f, adopter_id.as_str()).await;
    let late = create(&f, late_id.as_str()).await;
    let attachment_id =
        lash_core::attachments::content_id(format!("evidence-payload-{namespace}").as_bytes());

    record_completed_write(&uploader, &write_intent(&uploader_id, &attachment_id));
    adopter
        .commit_refs(&adopter_id, std::slice::from_ref(&attachment_id))
        .expect("the second session adopts the uploaded digest");

    drop(uploader);
    f.delete_session(&uploader_id)
        .await
        .expect("the uploader's session ages out");
    assert!(
        f.has_live_attachment_ref(&attachment_id, u64::MAX)
            .await
            .unwrap(),
        "the adopter still roots the digest"
    );
    late.commit_refs(&late_id, std::slice::from_ref(&attachment_id))
        .expect("a third session adopts through the adopter's copied evidence");
    assert!(
        f.has_live_attachment_ref(&attachment_id, u64::MAX)
            .await
            .unwrap()
    );
}

/// An uploader's own rollback settles its own row only; it can neither unroot
/// nor unstamp a digest another session already adopted.
async fn abort_after_a_foreign_adoption_preserves_that_root(f: Arc<dyn SessionStoreFactory>) {
    let namespace = uuid::Uuid::new_v4();
    let uploader_id = SessionId::from(format!("abort-uploader-{namespace}"));
    let adopter_id = SessionId::from(format!("abort-adopter-{namespace}"));
    let uploader = create(&f, uploader_id.as_str()).await;
    let adopter = create(&f, adopter_id.as_str()).await;
    let bytes: Arc<dyn AttachmentStore> = Arc::new(InMemoryAttachmentStore::new());
    let payload = format!("abort-after-adoption-{namespace}").into_bytes();
    let attachment_id = bytes.put(payload, image_meta()).await.unwrap().id;

    let intent = write_intent(&uploader_id, &attachment_id);
    let AttachmentWriteFence::Granted(permit) = uploader
        .begin_attachment_write(intent.clone())
        .expect("uploader begins")
    else {
        panic!("an unclaimed digest must grant the fence");
    };
    uploader
        .complete_attachment_write(&intent, permit)
        .expect("uploader stamps its evidence");
    adopter
        .commit_refs(&adopter_id, std::slice::from_ref(&attachment_id))
        .expect("another session adopts the uploaded digest");

    uploader
        .abort_attachment_write(&intent, permit)
        .expect("a late abort of the uploader's own attempt");
    assert!(
        f.has_live_attachment_ref(&attachment_id, u64::MAX)
            .await
            .unwrap(),
        "the foreign root survives the uploader's rollback"
    );
    assert_eq!(
        sweep(&f, &bytes).await,
        0,
        "the rooted bytes are not reclaimed"
    );
    assert!(bytes.get(&attachment_id).await.is_ok());
}

/// Re-putting a digest that is already committed is a fresh attempt over the
/// same row: neither the upload stamp nor the commitment is lost.
async fn duplicate_put_preserves_stamp_and_commitment(f: Arc<dyn SessionStoreFactory>) {
    let namespace = uuid::Uuid::new_v4();
    let session_id = SessionId::from(format!("duplicate-put-{namespace}"));
    let adopter_id = SessionId::from(format!("duplicate-put-adopter-{namespace}"));
    let store = create(&f, &session_id).await;
    let adopter = create(&f, adopter_id.as_str()).await;
    let attachment_id =
        lash_core::attachments::content_id(format!("duplicate-put-{namespace}").as_bytes());
    let intent = write_intent(&session_id, &attachment_id);

    record_completed_write(&store, &intent);
    store
        .commit_refs(&session_id, std::slice::from_ref(&attachment_id))
        .expect("the uploader commits its own reference");
    assert!(
        f.has_live_attachment_ref(&attachment_id, u64::MAX)
            .await
            .unwrap()
    );

    record_completed_write(&store, &intent);
    assert!(
        store
            .list_uncommitted(u64::MAX)
            .unwrap()
            .iter()
            .all(|entry| entry.session_id != session_id || entry.attachment_id != attachment_id),
        "a duplicate put must not unstamp the committed row"
    );
    assert!(
        f.has_live_attachment_ref(&attachment_id, u64::MAX)
            .await
            .unwrap(),
        "a duplicate put must preserve the commitment"
    );
    adopter
        .commit_refs(&adopter_id, std::slice::from_ref(&attachment_id))
        .expect("a duplicate put preserves the upload evidence");
}

/// Validation covers the whole batch before anything is written.
async fn batch_commit_with_one_unknown_digest_writes_nothing(f: Arc<dyn SessionStoreFactory>) {
    let namespace = uuid::Uuid::new_v4();
    let session_id = SessionId::from(format!("batch-unknown-{namespace}"));
    let store = create(&f, &session_id).await;
    let known = lash_core::attachments::content_id(format!("batch-known-{namespace}").as_bytes());
    let unknown =
        lash_core::attachments::content_id(format!("batch-unknown-{namespace}").as_bytes());
    record_completed_write(&store, &write_intent(&session_id, &known));

    let error = store
        .commit_refs(&session_id, &[known.clone(), unknown.clone()])
        .expect_err("one unknown digest refuses the whole batch");
    assert!(matches!(
        error,
        StoreError::UnknownAttachment { ref digest } if digest == &unknown
    ));
    assert!(
        !f.has_live_attachment_ref(&known, u64::MAX).await.unwrap(),
        "the evidenced member of a refused batch is not adopted"
    );
    assert!(
        store
            .list_uncommitted(u64::MAX)
            .unwrap()
            .iter()
            .any(|entry| entry.session_id == session_id && entry.attachment_id == known),
        "the refused batch left the evidenced row uncommitted"
    );
    assert!(
        !store.list_all_refs().unwrap().contains(&unknown),
        "the refused batch wrote no row for the unknown digest"
    );

    store
        .commit_refs(&session_id, std::slice::from_ref(&known))
        .expect("the evidenced digest alone adopts");
    assert!(f.has_live_attachment_ref(&known, u64::MAX).await.unwrap());
}

async fn adoption_fence_and_rollback(f: Arc<dyn SessionStoreFactory>) {
    let session_id = SessionId::from(format!("fenced-adoption-{}", uuid::Uuid::new_v4()));
    let bytes: Arc<dyn AttachmentStore> = Arc::new(InMemoryAttachmentStore::new());
    let store = create(&f, &session_id).await;
    let scoped = SessionAttachmentStore::new(bytes.clone(), store.clone(), session_id.clone());
    let mut st = state(&session_id);
    let mut ids = Vec::new();
    for byte in [17, 18] {
        let reference = scoped
            .put([session_id.as_bytes(), &[byte]].concat(), image_meta())
            .await
            .unwrap();
        with_image(&mut st, &reference);
        ids.push(reference.id);
    }
    ids.sort();
    // An armed delete owns the second digest: condemnation cleared its manifest
    // evidence under the fence, so the batch has one unadoptable member.
    assert_eq!(
        f.condemn_attachment(&ids[1], u64::MAX).await.unwrap(),
        AttachmentCondemnation::Condemned
    );
    assert_eq!(
        f.arm_attachment_delete(&ids[1]).await.unwrap(),
        AttachmentDeleteArming::Armed
    );
    let mut commit = RuntimeCommit::persisted_state_for_test(&st, &[]);
    commit.committed_attachment_ids = ids.clone();
    let snapshot = |loaded: Option<lash_core::store::PersistedSessionRead>| {
        loaded.map(|s| {
            (
                s.head_revision,
                s.checkpoint_ref,
                serde_json::to_value(s.graph).unwrap(),
            )
        })
    };
    let before = snapshot(store.load_session().await.unwrap());
    let error = store
        .commit_runtime_state(commit.clone())
        .await
        .expect_err("armed delete refuses adoption");
    assert!(
        matches!(error, StoreError::UnknownAttachment { ref digest } if digest == &ids[1]),
        "{error}"
    );
    assert_eq!(
        snapshot(store.load_session().await.unwrap()),
        before,
        "failed adoption publishes no graph/head/checkpoint"
    );
    // The refused batch adopted neither member: the evidenced digest is still
    // uncommitted and the condemned one still has no row at all. (Probing
    // `live_attachment_refs` here would instead age the surviving uncommitted
    // intent out, destroying the evidence the retry below needs.)
    assert!(
        store
            .list_uncommitted(u64::MAX)
            .unwrap()
            .iter()
            .any(|entry| entry.attachment_id == ids[0]),
        "a batch with one unadoptable digest adopts none of it"
    );
    assert!(
        !store.list_all_refs().unwrap().contains(&ids[1]),
        "the refused batch wrote no row for the condemned digest"
    );
    let repeated = store
        .commit_runtime_state(commit.clone())
        .await
        .expect_err("boundary rollback must leave the armed delete in place");
    assert!(
        matches!(repeated, StoreError::UnknownAttachment { ref digest } if digest == &ids[1]),
        "armed deletion phase was lost across rollback: {repeated}"
    );

    // Release the delete and re-establish the evidence the condemnation cleared.
    f.release_attachment_condemnation(&ids[1]).await.unwrap();
    record_completed_write(&store, &write_intent(&session_id, &ids[1]));
    store.commit_runtime_state(commit.clone()).await.unwrap();
    assert!(
        store
            .commit_runtime_state(commit)
            .await
            .unwrap()
            .receipt_replayed
    );
    for id in &ids {
        assert!(f.has_live_attachment_ref(id, u64::MAX).await.unwrap());
        assert!(bytes.get(id).await.is_ok());
    }
    assert_eq!(sweep(&f, &bytes).await, 0);
    f.delete_session(&session_id).await.unwrap();
    assert_eq!(sweep(&f, &bytes).await, 2);
}

async fn adoption_after_full_gc_and_release_is_refused(f: Arc<dyn SessionStoreFactory>) {
    let namespace = uuid::Uuid::new_v4();
    let owner_id = format!("swept-adoption-owner-{namespace}");
    let receiver_id = format!("swept-adoption-receiver-{namespace}");
    let payload = [owner_id.as_bytes(), &[251]].concat();
    let bytes: Arc<dyn AttachmentStore> = Arc::new(InMemoryAttachmentStore::new());
    let owner = create(&f, &owner_id).await;
    let receiver = create(&f, &receiver_id).await;
    let reference = SessionAttachmentStore::new(bytes.clone(), owner.clone(), &owner_id)
        .put(
            payload,
            AttachmentCreateMeta::new(MediaType::parse("image/png").unwrap(), None, None),
        )
        .await
        .unwrap();
    let mut owner_state = state(&owner_id);
    with_image(&mut owner_state, &reference);
    let mut owner_commit = RuntimeCommit::persisted_state_for_test(&owner_state, &[]);
    owner_commit.committed_attachment_ids = vec![reference.id.clone()];
    owner.commit_runtime_state(owner_commit).await.unwrap();

    let reader = SessionAttachmentStore::new(bytes.clone(), receiver.clone(), &receiver_id);
    assert!(reader.get(&reference.id).await.is_ok());
    f.delete_session(&SessionId::from(owner_id.as_str()))
        .await
        .unwrap();
    f.reclaim_retained_evidence(RetentionBound {
        committed_before_epoch_ms: u64::MAX,
    })
    .await
    .unwrap();
    assert_eq!(sweep(&f, &bytes).await, 1);
    assert!(matches!(
        reader.get(&reference.id).await,
        Err(AttachmentStoreError::NotFound(_))
    ));
    f.release_attachment_condemnation(&reference.id)
        .await
        .expect("release after successful delete");
    assert!(matches!(
        reader.get(&reference.id).await,
        Err(AttachmentStoreError::NotFound(_))
    ));

    let snapshot = |loaded: Option<lash_core::store::PersistedSessionRead>| {
        loaded.map(|session| {
            (
                session.head_revision,
                session.checkpoint_ref,
                serde_json::to_value(session.graph).unwrap(),
            )
        })
    };
    let before = snapshot(receiver.load_session().await.unwrap());
    let mut receiver_state = state(&receiver_id);
    with_image(&mut receiver_state, &reference);
    let mut receiver_commit = RuntimeCommit::persisted_state_for_test(&receiver_state, &[]);
    receiver_commit.committed_attachment_ids = vec![reference.id.clone()];
    let error = receiver
        .commit_runtime_state(receiver_commit)
        .await
        .expect_err("deleted bytes must refuse adoption after release");
    assert!(matches!(
        error,
        StoreError::UnknownAttachment { ref digest } if digest == &reference.id
    ));
    assert_eq!(
        snapshot(receiver.load_session().await.unwrap()),
        before,
        "typed refusal publishes no graph or head state"
    );
    assert!(
        !receiver.list_all_refs().unwrap().contains(&reference.id),
        "typed refusal publishes no manifest row"
    );
    assert!(
        !f.has_live_attachment_ref(&reference.id, u64::MAX)
            .await
            .unwrap()
    );
}

async fn reput_after_full_gc_allows_adoption(f: Arc<dyn SessionStoreFactory>) {
    let namespace = uuid::Uuid::new_v4();
    let owner_id = format!("reput-owner-{namespace}");
    let receiver_id = format!("reput-receiver-{namespace}");
    let payload = [owner_id.as_bytes(), &[252]].concat();
    let bytes: Arc<dyn AttachmentStore> = Arc::new(InMemoryAttachmentStore::new());
    let owner = create(&f, &owner_id).await;
    let receiver = create(&f, &receiver_id).await;
    let scoped_owner = SessionAttachmentStore::new(bytes.clone(), owner.clone(), &owner_id);
    let reference = scoped_owner
        .put(
            payload.clone(),
            AttachmentCreateMeta::new(MediaType::parse("image/png").unwrap(), None, None),
        )
        .await
        .unwrap();
    let mut owner_state = state(&owner_id);
    with_image(&mut owner_state, &reference);
    let mut owner_commit = RuntimeCommit::persisted_state_for_test(&owner_state, &[]);
    owner_commit.committed_attachment_ids = vec![reference.id.clone()];
    owner.commit_runtime_state(owner_commit).await.unwrap();
    f.delete_session(&SessionId::from(owner_id.as_str()))
        .await
        .unwrap();
    f.reclaim_retained_evidence(RetentionBound {
        committed_before_epoch_ms: u64::MAX,
    })
    .await
    .unwrap();
    assert_eq!(sweep(&f, &bytes).await, 1);

    let scoped_receiver =
        SessionAttachmentStore::new(bytes.clone(), receiver.clone(), &receiver_id);
    let restored = scoped_receiver
        .put(
            payload,
            AttachmentCreateMeta::new(MediaType::parse("image/png").unwrap(), None, None),
        )
        .await
        .expect("a fresh put records new upload evidence for the swept digest");
    assert_eq!(restored.id, reference.id);
    let mut receiver_state = state(&receiver_id);
    with_image(&mut receiver_state, &restored);
    let mut receiver_commit = RuntimeCommit::persisted_state_for_test(&receiver_state, &[]);
    receiver_commit.committed_attachment_ids = vec![restored.id.clone()];
    receiver
        .commit_runtime_state(receiver_commit)
        .await
        .expect("adoption succeeds after re-put");
    assert!(
        f.has_live_attachment_ref(&restored.id, u64::MAX)
            .await
            .unwrap()
    );
    assert!(scoped_receiver.get(&restored.id).await.is_ok());
    assert_eq!(sweep(&f, &bytes).await, 0);
}

const RACE_SCHEDULES: usize = 20;

async fn sweep_adoption_race(f: Arc<dyn SessionStoreFactory>) {
    for schedule in 0..RACE_SCHEDULES {
        let namespace = uuid::Uuid::new_v4();
        let owner_id = format!("adoption-race-owner-{schedule}-{namespace}");
        let receiver_id = format!("adoption-race-receiver-{schedule}-{namespace}");
        let bytes: Arc<dyn AttachmentStore> = Arc::new(InMemoryAttachmentStore::new());
        let owner = create(&f, &owner_id).await;
        let receiver = create(&f, &receiver_id).await;
        let reference = put(owner.clone(), bytes.clone(), &owner_id, schedule as u8).await;
        let mut owner_state = state(&owner_id);
        with_image(&mut owner_state, &reference);
        let mut owner_commit = RuntimeCommit::persisted_state_for_test(&owner_state, &[]);
        owner_commit.committed_attachment_ids = vec![reference.id.clone()];
        owner.commit_runtime_state(owner_commit).await.unwrap();
        f.delete_session(&SessionId::from(owner_id.as_str()))
            .await
            .unwrap();
        f.reclaim_retained_evidence(RetentionBound {
            committed_before_epoch_ms: u64::MAX,
        })
        .await
        .unwrap();

        let mut receiver_state = state(&receiver_id);
        with_image(&mut receiver_state, &reference);
        let mut receiver_commit = RuntimeCommit::persisted_state_for_test(&receiver_state, &[]);
        receiver_commit.committed_attachment_ids = vec![reference.id.clone()];
        let (reclaimed, adopted) = if schedule % 2 == 0 {
            tokio::join!(
                sweep(&f, &bytes),
                receiver.commit_runtime_state(receiver_commit)
            )
        } else {
            let (adopted, reclaimed) = tokio::join!(
                receiver.commit_runtime_state(receiver_commit),
                sweep(&f, &bytes)
            );
            (reclaimed, adopted)
        };
        let rooted = f
            .has_live_attachment_ref(&reference.id, u64::MAX)
            .await
            .unwrap();
        let present = bytes.get(&reference.id).await.is_ok();
        assert!(
            !rooted || present,
            "schedule {schedule}: sweep/adoption race rooted missing bytes"
        );
        match adopted {
            Ok(_) => {
                assert!(
                    rooted && present,
                    "schedule {schedule}: successful adoption lost bytes"
                );
                assert_eq!(reclaimed, 0);
            }
            Err(StoreError::UnknownAttachment { digest }) => {
                assert_eq!(digest, reference.id);
                assert!(
                    !rooted,
                    "schedule {schedule}: a refused adoption must publish no root"
                );
                assert_eq!(reclaimed, 1);
            }
            Err(error) => panic!("schedule {schedule}: unexpected adoption error: {error}"),
        }
    }
}

async fn sweep_reput_race(f: Arc<dyn SessionStoreFactory>) {
    for schedule in 0..RACE_SCHEDULES {
        let namespace = uuid::Uuid::new_v4();
        let owner_id = format!("reput-race-owner-{schedule}-{namespace}");
        let writer_id = format!("reput-race-writer-{schedule}-{namespace}");
        let payload = [owner_id.as_bytes(), &[schedule as u8]].concat();
        let bytes: Arc<dyn AttachmentStore> = Arc::new(InMemoryAttachmentStore::new());
        let owner = create(&f, &owner_id).await;
        let writer = create(&f, &writer_id).await;
        let reference = SessionAttachmentStore::new(bytes.clone(), owner.clone(), &owner_id)
            .put(
                payload.clone(),
                AttachmentCreateMeta::new(MediaType::parse("image/png").unwrap(), None, None),
            )
            .await
            .unwrap();
        let mut owner_state = state(&owner_id);
        with_image(&mut owner_state, &reference);
        let mut owner_commit = RuntimeCommit::persisted_state_for_test(&owner_state, &[]);
        owner_commit.committed_attachment_ids = vec![reference.id.clone()];
        owner.commit_runtime_state(owner_commit).await.unwrap();
        f.delete_session(&SessionId::from(owner_id.as_str()))
            .await
            .unwrap();
        f.reclaim_retained_evidence(RetentionBound {
            committed_before_epoch_ms: u64::MAX,
        })
        .await
        .unwrap();

        let scoped_writer = SessionAttachmentStore::new(bytes.clone(), writer, &writer_id);
        let put = scoped_writer.put(
            payload,
            AttachmentCreateMeta::new(MediaType::parse("image/png").unwrap(), None, None),
        );
        let (_reclaimed, restored) = if schedule % 2 == 0 {
            tokio::join!(sweep(&f, &bytes), put)
        } else {
            let (restored, reclaimed) = tokio::join!(put, sweep(&f, &bytes));
            (reclaimed, restored)
        };
        let restored = restored.expect("re-put wins or retries after the sweep");
        assert_eq!(restored.id, reference.id);
        let rooted = f.has_live_attachment_ref(&restored.id, 0).await.unwrap();
        let present = scoped_writer.get(&restored.id).await.is_ok();
        assert!(
            !rooted || present,
            "schedule {schedule}: sweep/re-put race rooted missing bytes; \
             rooted={rooted}, present={present}"
        );
    }
}

/// Every backend reads back the durable attachment owner it was given, as one
/// value (FIG-2850).
///
/// The owner used to ride the intent as three independently nullable fields.
/// Both SQL schemas enforce the pairing with `CHECK`
/// constraints; the in-memory store enforced nothing, so a half-populated pair
/// was accepted there and rejected by every production backend — a divergence
/// on a public API. The owner is now one [`AttachmentOwner`], so the malformed
/// input no longer exists to diverge on, and what remains to certify is that
/// the three owner shapes round-trip identically everywhere: a turn owner
/// carries no incarnation, a process owner always carries its own, and an
/// unowned direct host put reads back with no owner at all.
pub async fn attachment_owner_identity_round_trips_conformance(f: Arc<dyn SessionStoreFactory>) {
    let namespace = uuid::Uuid::new_v4();
    let session_id = format!("owner-identity-{namespace}");
    let store = create(&f, &session_id).await;
    let turn_owner = crate::AttachmentOwner::Turn {
        id: format!("owner-identity-turn-{namespace}"),
    };
    let process_owner = crate::AttachmentOwner::Process {
        id: format!("owner-identity-process-{namespace}"),
        incarnation: lash_core::ProcessIncarnation::from_registration_sequence(7),
    };
    let cases: [(&str, Option<crate::AttachmentOwner>); 3] = [
        ("owner-identity-turn", Some(turn_owner.clone())),
        ("owner-identity-process", Some(process_owner.clone())),
        ("owner-identity-unowned", None),
    ];
    for (digest, owner) in &cases {
        crate::conformance::helpers::record_completed_attachment_write(
            &store,
            crate::AttachmentIntent {
                attachment_id: AttachmentId::parse(*digest).expect("valid attachment id"),
                session_id: SessionId::from(session_id.clone()),
                canonical_uri: format!("lash-attachment://blake3/{digest}"),
                intent_at_epoch_ms: 1_000,
                owner: owner.clone(),
            },
        );
    }

    let entries = store
        .list_uncommitted(u64::MAX)
        .expect("list the manifest rows just written");
    for (digest, owner) in &cases {
        let entry = entries
            .iter()
            .find(|entry| entry.attachment_id.as_str() == *digest)
            .unwrap_or_else(|| panic!("manifest row for `{digest}` survived its write"));
        assert_eq!(
            entry.owner, *owner,
            "the durable owner of `{digest}` must read back exactly as written"
        );
    }

    // The pairing rules the SQL CHECK constraints enforce are properties of the
    // value, so every backend answers them identically without validating a row.
    assert_eq!(
        turn_owner.kind(),
        lash_core::AttachmentOwnerKind::Turn,
        "a turn owner projects the turn discriminant"
    );
    assert_eq!(
        turn_owner.incarnation(),
        None,
        "a turn owner never carries an incarnation"
    );
    assert_eq!(
        process_owner.kind(),
        lash_core::AttachmentOwnerKind::Process,
        "a process owner projects the process discriminant"
    );
    assert_eq!(
        process_owner.incarnation(),
        Some(lash_core::ProcessIncarnation::from_registration_sequence(7)),
        "a process owner always carries its incarnation"
    );

    f.delete_session(&SessionId::from(session_id))
        .await
        .unwrap();
}
