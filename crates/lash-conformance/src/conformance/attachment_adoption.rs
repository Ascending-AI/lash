//! Cross-session stored references acquire receiver roots in the boundary commit.
//! Root reconciliation is the layer-1 operation also used by terminal evidence
//! reclamation, so this witness can run at every head in the stack.
use lash_core::facade_support::{
    InMemoryAttachmentStore, SessionAttachmentStore, reclaim_unreferenced_attachments,
};
use lash_core::testing::store_fixtures::session_store_request;
use lash_core::*;
use pretty_assertions::assert_eq;
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
            return Err(AttachmentStoreError::Backend(
                "scripted attachment put failure".to_string(),
            ));
        }
        self.inner.put(bytes, meta).await
    }

    async fn get(&self, id: &AttachmentId) -> Result<StoredAttachment, AttachmentStoreError> {
        self.inner.get(id).await
    }

    async fn delete(&self, id: &AttachmentId) -> Result<(), AttachmentStoreError> {
        if self.fail_delete.load(Ordering::SeqCst) {
            return Err(AttachmentStoreError::Backend(
                "scripted attachment delete failure".to_string(),
            ));
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
            return Err(AttachmentStoreError::Backend(
                "scripted first attachment put failure".to_string(),
            ));
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
    f.delete_session(&SessionId::from(owner_id)).await.unwrap();
    let (commit, reclaim) = tokio::join!(
        live.commit_runtime_state(c),
        f.live_attachment_refs(u64::MAX)
    );
    commit.unwrap();
    reclaim.unwrap();
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
    out_of_band_absence_is_reclaimed(f.clone()).await;
    failed_delete_releases_free(f.clone()).await;
    failed_reput_restores_prior_phase(f.clone()).await;
    competing_writer_survives_failed_reput(f.clone()).await;
    sweep_cannot_overwrite_failed_reput_rollback(f.clone()).await;
    stale_sweep_release_cannot_revoke_restoring_writer(f.clone()).await;
    abandoned_writer_recovery_is_reputtable_after_reopen(f.clone()).await;
    stale_writer_abort_cannot_clobber_newer_reclamation(f.clone()).await;
    sweep_adoption_race(f.clone()).await;
    sweep_reput_race(f).await;
}

async fn out_of_band_absence_is_reclaimed(f: Arc<dyn SessionStoreFactory>) {
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
        .expect_err("head(None) must leave durable reclaimed evidence");
    assert!(matches!(
        error,
        StoreError::AttachmentBytesReclaimed { ref digest } if digest == &reference.id
    ));
}

async fn failed_delete_releases_free(f: Arc<dyn SessionStoreFactory>) {
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
    store
        .commit_refs(&session_id, std::slice::from_ref(&reference.id))
        .expect("failed delete releases Free rather than recording Reclaimed");
    assert!(backend.get(&reference.id).await.is_ok());
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
    let direct_error = store
        .record_intent(write_intent(&session_id, &condemned.id))
        .expect_err("the legacy manifest path must not clear Condemned");
    assert!(
        direct_error.to_string().contains("unfenced manifest path"),
        "unexpected direct-write refusal: {direct_error}"
    );
    assert_eq!(
        f.arm_attachment_delete(&condemned.id).await.unwrap(),
        AttachmentDeleteArming::Armed,
        "a refused direct manifest write leaves Condemned intact"
    );
    f.release_attachment_condemnation(&condemned.id)
        .await
        .unwrap();
    assert_eq!(
        f.condemn_attachment(&condemned.id, 0).await.unwrap(),
        AttachmentCondemnation::Condemned
    );
    backend.fail_put(true);
    scoped
        .put(condemned_payload, image_meta())
        .await
        .expect_err("scripted condemned re-put fails");
    assert_eq!(
        f.arm_attachment_delete(&condemned.id).await.unwrap(),
        AttachmentDeleteArming::Armed,
        "failed re-put restores Condemned rather than Free"
    );
    f.release_attachment_condemnation(&condemned.id)
        .await
        .unwrap();

    backend.fail_put(false);
    let reclaimed_payload = format!("reclaimed-reput-{namespace}").into_bytes();
    let reclaimed = backend
        .put(reclaimed_payload.clone(), image_meta())
        .await
        .unwrap();
    assert_eq!(
        f.condemn_attachment(&reclaimed.id, 0).await.unwrap(),
        AttachmentCondemnation::Condemned
    );
    assert_eq!(
        f.arm_attachment_delete(&reclaimed.id).await.unwrap(),
        AttachmentDeleteArming::Armed
    );
    backend.inner.delete(&reclaimed.id).await.unwrap();
    f.reclaim_attachment_condemnation(&reclaimed.id)
        .await
        .unwrap();
    let direct_error = store
        .record_intent(write_intent(&session_id, &reclaimed.id))
        .expect_err("the legacy manifest path must not clear Reclaimed");
    assert!(matches!(
        direct_error,
        StoreError::AttachmentBytesReclaimed { ref digest } if digest == &reclaimed.id
    ));
    backend.fail_put(true);
    scoped
        .put(reclaimed_payload, image_meta())
        .await
        .expect_err("scripted reclaimed re-put fails");
    let error = store
        .commit_refs(&session_id, std::slice::from_ref(&reclaimed.id))
        .expect_err("failed re-put restores Reclaimed");
    assert!(matches!(
        error,
        StoreError::AttachmentBytesReclaimed { ref digest } if digest == &reclaimed.id
    ));
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
    f.reclaim_attachment_condemnation(&attachment_id)
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

async fn abandoned_writer_recovery_is_reputtable_after_reopen(f: Arc<dyn SessionStoreFactory>) {
    for reclaimed in [false, true] {
        let namespace = uuid::Uuid::new_v4();
        let session_id = SessionId::from(format!(
            "abandoned-{}-writer-{namespace}",
            if reclaimed { "reclaimed" } else { "condemned" }
        ));
        let attachment_id = lash_core::attachments::content_id(session_id.as_str().as_bytes());
        assert_eq!(
            f.condemn_attachment(&attachment_id, 0).await.unwrap(),
            AttachmentCondemnation::Condemned
        );
        if reclaimed {
            assert_eq!(
                f.arm_attachment_delete(&attachment_id).await.unwrap(),
                AttachmentDeleteArming::Armed
            );
            f.reclaim_attachment_condemnation(&attachment_id)
                .await
                .unwrap();
        }

        let intent = write_intent(&session_id, &attachment_id);
        let crashed_store = create(&f, &session_id).await;
        let stale_permit = match crashed_store
            .begin_attachment_write(intent.clone())
            .expect("writer claims the prior phase")
        {
            AttachmentWriteFence::Granted(permit) => permit,
            AttachmentWriteFence::ReclamationInFlight => panic!("first writer must win"),
        };
        assert!(stale_permit.rollback_token().is_some());
        assert!(matches!(
            crashed_store
                .begin_attachment_write(intent.clone())
                .expect("same-session concurrent write observes the durable claim"),
            AttachmentWriteFence::ReclamationInFlight
        ));
        drop(crashed_store);

        // The host has established quiescence after crash/cancellation. Recovery
        // reopens the durable authority, clears the token-to-intent association,
        // and leaves the exact phase available for a fresh re-put.
        f.recover_abandoned_attachment_write(&attachment_id)
            .await
            .expect("recover abandoned restoring writer");
        let reopened = create(&f, &session_id).await;
        let fresh_permit = match reopened
            .begin_attachment_write(intent.clone())
            .expect("fresh re-put claims the recovered phase")
        {
            AttachmentWriteFence::Granted(permit) => permit,
            AttachmentWriteFence::ReclamationInFlight => {
                panic!("recovered writer token must not strand the re-put")
            }
        };
        assert!(fresh_permit.rollback_token().is_some());

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

        if reclaimed {
            let error = reopened
                .commit_refs(&session_id, std::slice::from_ref(&attachment_id))
                .expect_err("recovery must preserve Reclaimed");
            assert!(matches!(
                error,
                StoreError::AttachmentBytesReclaimed { ref digest }
                    if digest == &attachment_id
            ));
        } else {
            assert_eq!(
                f.arm_attachment_delete(&attachment_id).await.unwrap(),
                AttachmentDeleteArming::Armed,
                "recovery must preserve Condemned"
            );
            f.release_attachment_condemnation(&attachment_id)
                .await
                .unwrap();
        }
    }
}

async fn stale_writer_abort_cannot_clobber_newer_reclamation(f: Arc<dyn SessionStoreFactory>) {
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
    // A newer sweep then owns the phase through a completed reclamation.
    f.recover_abandoned_attachment_write(&attachment_id)
        .await
        .unwrap();
    assert_eq!(
        f.arm_attachment_delete(&attachment_id).await.unwrap(),
        AttachmentDeleteArming::Armed,
        "recovery clears only the abandoned token, preserving Condemned"
    );
    f.reclaim_attachment_condemnation(&attachment_id)
        .await
        .unwrap();

    store
        .abort_attachment_write(&intent, permit)
        .expect("stale writer abort is a no-op");
    let error = store
        .commit_refs(&session_id, std::slice::from_ref(&attachment_id))
        .expect_err("stale rollback must not clear a newer Reclaimed phase");
    assert!(matches!(
        error,
        StoreError::AttachmentBytesReclaimed { ref digest } if digest == &attachment_id
    ));
}

fn image_meta() -> AttachmentCreateMeta {
    AttachmentCreateMeta::new(MediaType::parse("image/png").unwrap(), None, None)
}

fn write_intent(session_id: &SessionId, attachment_id: &AttachmentId) -> AttachmentIntent {
    AttachmentIntent {
        attachment_id: attachment_id.clone(),
        session_id: session_id.clone(),
        canonical_uri: format!("lash-attachment://blake3/{attachment_id}"),
        intent_at_epoch_ms: 0,
        owner_kind: None,
        owner_id: None,
    }
}

async fn adoption_fence_and_rollback(f: Arc<dyn SessionStoreFactory>) {
    let session_id = SessionId::from(format!("fenced-adoption-{}", uuid::Uuid::new_v4()));
    let bytes: Arc<dyn AttachmentStore> = Arc::new(InMemoryAttachmentStore::new());
    let store = create(&f, &session_id).await;
    let mut st = state(&session_id);
    let mut ids = Vec::new();
    for byte in [17, 18] {
        let reference = bytes
            .put(
                [session_id.as_bytes(), &[byte]].concat(),
                AttachmentCreateMeta::new(MediaType::parse("image/png").unwrap(), None, None),
            )
            .await
            .unwrap();
        with_image(&mut st, &reference);
        ids.push(reference.id);
    }
    ids.sort();
    for id in &ids {
        assert_eq!(
            f.condemn_attachment(id, u64::MAX).await.unwrap(),
            AttachmentCondemnation::Condemned
        );
    }
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
        error.to_string().contains("physical deletion is in flight"),
        "{error}"
    );
    assert_eq!(
        snapshot(store.load_session().await.unwrap()),
        before,
        "failed adoption publishes no graph/head/checkpoint"
    );
    let roots = f.live_attachment_refs(u64::MAX).await.unwrap();
    assert!(
        ids.iter().all(|id| !roots.contains(id)),
        "failed batch leaves no attachment root"
    );
    let repeated = store
        .commit_runtime_state(commit.clone())
        .await
        .expect_err("boundary rollback must leave the armed delete in place");
    assert!(
        repeated
            .to_string()
            .contains("physical deletion is in flight"),
        "armed deletion phase was lost across rollback: {repeated}"
    );
    assert_eq!(
        f.arm_attachment_delete(&ids[0]).await.unwrap(),
        AttachmentDeleteArming::Armed,
        "failed batch rolls back an earlier revocation"
    );
    for id in &ids {
        f.release_attachment_condemnation(id).await.unwrap();
        assert_eq!(
            f.condemn_attachment(id, u64::MAX).await.unwrap(),
            AttachmentCondemnation::Condemned
        );
    }
    store.commit_runtime_state(commit.clone()).await.unwrap();
    assert!(
        store
            .commit_runtime_state(commit)
            .await
            .unwrap()
            .receipt_replayed
    );
    for id in &ids {
        assert_eq!(
            f.arm_attachment_delete(id).await.unwrap(),
            AttachmentDeleteArming::Revoked,
            "successful adoption revokes unarmed deletion"
        );
        assert!(f.has_live_attachment_ref(id, u64::MAX).await.unwrap());
        assert!(bytes.get(id).await.is_ok());
    }
    assert_eq!(sweep(&f, &bytes).await, 0);
    f.delete_session(&session_id).await.unwrap();
    assert_eq!(sweep(&f, &bytes).await, 2);
}

async fn adoption_after_full_gc_and_release_is_refused(f: Arc<dyn SessionStoreFactory>) {
    let namespace = uuid::Uuid::new_v4();
    let owner_id = format!("reclaimed-adoption-owner-{namespace}");
    let receiver_id = format!("reclaimed-adoption-receiver-{namespace}");
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
        .expect_err("reclaimed bytes must refuse adoption after release");
    assert!(matches!(
        error,
        StoreError::AttachmentBytesReclaimed { ref digest } if digest == &reference.id
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
        .expect("a fresh put clears the reclaimed fact");
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
            Err(StoreError::AttachmentBytesReclaimed { digest }) => {
                assert_eq!(digest, reference.id);
                assert!(!rooted && !present);
                assert_eq!(reclaimed, 1);
            }
            Err(error) => {
                assert!(
                    error.to_string().contains("physical deletion is in flight"),
                    "schedule {schedule}: unexpected adoption error: {error}"
                );
                assert!(!rooted);
                assert_eq!(reclaimed, 1);
            }
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
