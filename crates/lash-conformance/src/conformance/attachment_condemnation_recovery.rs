//! ADR 0067 §6: a sweep adopts and finishes the condemnations a crashed
//! predecessor left before it condemns anything new, two sweepers adopt each
//! row once, and a delete that keeps failing stalls typed.
use super::attachment_adoption::{
    AttachmentBytesFactory, FaultingAttachmentStore, create, image_meta, open_pass,
    record_completed_write, write_intent,
};
use crate::conformance::DeploymentViewExt as _;
use lash_core::facade_support::reclaim_unreferenced_attachments;
use lash_core::testing::store_fixtures::session_store_request;
use lash_core::*;
use pretty_assertions::assert_eq;
use std::future::Future;
use std::sync::Arc;

/// Where an interrupted sweep stops for good, standing in for a sweeper that
/// crashed there.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SweepCrashPoint {
    /// After condemning, before arming: `Condemned`, bytes present.
    AfterCondemn,
    /// After arming, before the physical delete: `Deleting`, bytes present.
    AfterArm,
    /// After the physical delete, before retiring: `Deleting`, bytes gone.
    BeforeRetire,
}

/// A root authority that forwards to a durable factory until its sweep
/// reaches `crash_at`, then never returns: the pass stays live until the
/// sweep task is aborted, exactly as a sweeper that hangs and then dies.
struct InterruptedSweepRoot {
    inner: Arc<dyn DeploymentStore>,
    crash_at: SweepCrashPoint,
    reached: tokio::sync::Notify,
}

impl InterruptedSweepRoot {
    fn new(inner: Arc<dyn DeploymentStore>, crash_at: SweepCrashPoint) -> Self {
        Self {
            inner,
            crash_at,
            reached: tokio::sync::Notify::new(),
        }
    }

    async fn stop_if(&self, point: SweepCrashPoint) {
        if self.crash_at == point {
            self.reached.notify_one();
            std::future::pending::<()>().await;
        }
    }
}

#[async_trait::async_trait]
impl AttachmentRootSet for InterruptedSweepRoot {
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

    async fn begin_attachment_sweep(&self) -> Result<AttachmentSweepGeneration, StoreError> {
        self.inner.begin_attachment_sweep().await
    }

    async fn adopt_attachment_condemnations(
        &self,
        generation: &AttachmentSweepGeneration,
    ) -> Result<AttachmentCondemnationAdoption, StoreError> {
        self.inner.adopt_attachment_condemnations(generation).await
    }

    async fn condemn_attachment(
        &self,
        id: &AttachmentId,
        cutoff: u64,
        generation: &AttachmentSweepGeneration,
    ) -> Result<AttachmentCondemnation, StoreError> {
        let outcome = self
            .inner
            .condemn_attachment(id, cutoff, generation)
            .await?;
        if outcome == AttachmentCondemnation::Condemned {
            self.stop_if(SweepCrashPoint::AfterCondemn).await;
        }
        Ok(outcome)
    }

    async fn arm_attachment_delete(
        &self,
        id: &AttachmentId,
        generation: &AttachmentSweepGeneration,
    ) -> Result<AttachmentDeleteArming, StoreError> {
        let outcome = self.inner.arm_attachment_delete(id, generation).await?;
        if outcome == AttachmentDeleteArming::Armed {
            self.stop_if(SweepCrashPoint::AfterArm).await;
        }
        Ok(outcome)
    }

    async fn settle_attachment_condemnation(
        &self,
        id: &AttachmentId,
        generation: &AttachmentSweepGeneration,
        settlement: AttachmentCondemnationSettlement,
    ) -> Result<AttachmentSettlementOutcome, StoreError> {
        if settlement == AttachmentCondemnationSettlement::Deleted {
            self.stop_if(SweepCrashPoint::BeforeRetire).await;
        }
        self.inner
            .settle_attachment_condemnation(id, generation, settlement)
            .await
    }
}

/// An attachment store that logs every physical delete it is asked for, in
/// order, over a shared inner store.
struct DeleteLog {
    inner: Arc<dyn AttachmentStore>,
    deleted: std::sync::Mutex<Vec<AttachmentId>>,
}

impl DeleteLog {
    fn over(inner: Arc<dyn AttachmentStore>) -> Self {
        Self {
            inner,
            deleted: std::sync::Mutex::new(Vec::new()),
        }
    }

    #[expect(
        clippy::expect_used,
        reason = "conformance-law fixture: the log is never poisoned"
    )]
    fn deleted(&self) -> Vec<AttachmentId> {
        self.deleted.lock().expect("delete log").clone()
    }
}

#[async_trait::async_trait]
impl AttachmentStore for DeleteLog {
    async fn put(
        &self,
        bytes: Vec<u8>,
        meta: AttachmentCreateMeta,
    ) -> Result<AttachmentRef, AttachmentStoreError> {
        self.inner.put(bytes, meta).await
    }

    async fn get(&self, id: &AttachmentId) -> Result<StoredAttachment, AttachmentStoreError> {
        self.inner.get(id).await
    }

    #[expect(
        clippy::expect_used,
        reason = "conformance-law fixture: the log is never poisoned"
    )]
    async fn delete(&self, id: &AttachmentId) -> Result<(), AttachmentStoreError> {
        self.inner.delete(id).await?;
        self.deleted.lock().expect("delete log").push(id.clone());
        Ok(())
    }

    async fn list(&self) -> Result<Vec<StoredBlobRef>, AttachmentStoreError> {
        self.inner.list().await
    }

    async fn head(&self, id: &AttachmentId) -> Result<Option<StoredBlobRef>, AttachmentStoreError> {
        self.inner.head(id).await
    }
}

fn authorize_all() -> AttachmentReclamationPolicy {
    AttachmentReclamationPolicy {
        grace_period_ms: 0,
        empty_root_set: EmptyRootSetPolicy::AuthorizeDeleteAll,
    }
}

/// ADR 0067 §6 across a cold reopen, with two sweepers.
///
/// Three sweeps crash — after condemning, after arming, and after deleting but
/// before retiring — and the factory is closed with their rows stranded: two
/// digests' bytes remain, and one row outlives its bytes where no blob listing
/// can find it. After reopen two sweepers run at once. Between them they adopt
/// each stranded row exactly once and delete each surviving blob exactly once,
/// and each sweeper finishes the rows it adopted before it deletes anything
/// new. Nothing is left: no row, no bytes.
#[expect(
    clippy::expect_used,
    clippy::unwrap_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
pub async fn cold_reopen_adopts_old_generation_before_new_deletes<Reopen, ReopenFuture>(
    factory: Arc<dyn DeploymentStore>,
    make_bytes: AttachmentBytesFactory,
    reopen: Reopen,
) where
    Reopen: FnOnce() -> ReopenFuture,
    ReopenFuture: Future<Output = Arc<dyn DeploymentStore>>,
{
    assert_eq!(
        factory.fence(),
        AttachmentGcFence::Fenced,
        "adoption requires a fenced durable authority"
    );
    let namespace = uuid::Uuid::new_v4();
    let session_id = SessionId::from(format!("condemnation-crash-{namespace}"));
    factory
        .admit_view(&session_store_request(
            &session_id,
            "condemnation-crash",
            SessionRelation::Root,
        ))
        .await
        .expect("materialize durable condemnation catalog");
    let backend = make_bytes();
    let blob = |label: &str| format!("condemnation-crash-{label}-{namespace}").into_bytes();

    let mut crashed = Vec::new();
    let mut stranded = Vec::new();
    for crash_at in [
        SweepCrashPoint::AfterCondemn,
        SweepCrashPoint::AfterArm,
        SweepCrashPoint::BeforeRetire,
    ] {
        let reference = backend
            .put(blob(&format!("{crash_at:?}")), image_meta())
            .await
            .expect("seed an unreferenced physical blob");
        let root = Arc::new(InterruptedSweepRoot::new(Arc::clone(&factory), crash_at));
        let sweep_root = Arc::clone(&root);
        let sweep_backend = Arc::clone(&backend);
        let sweep = tokio::spawn(async move {
            reclaim_unreferenced_attachments(
                sweep_root.as_ref(),
                sweep_backend.as_ref(),
                authorize_all(),
            )
            .await
        });
        tokio::time::timeout(std::time::Duration::from_secs(10), root.reached.notified())
            .await
            .unwrap_or_else(|_| panic!("the sweep reaches its {crash_at:?} crash point"));
        crashed.push((sweep, root));
        stranded.push((crash_at, reference.id));
    }
    let phase_of = |crash_at| match crash_at {
        SweepCrashPoint::AfterCondemn => AttachmentCondemnationPhase::Condemned,
        SweepCrashPoint::AfterArm | SweepCrashPoint::BeforeRetire => {
            AttachmentCondemnationPhase::Deleting
        }
    };
    let mut expected = stranded
        .iter()
        .map(|(crash_at, digest)| AttachmentCondemnationRecord {
            digest: digest.clone(),
            phase: phase_of(*crash_at),
            provenance: AttachmentCondemnationProvenance::SweepOwned,
            delete_attempts: 0,
            last_delete_error: None,
            stalled: None,
        })
        .collect::<Vec<_>>();
    expected.sort_by(|left, right| left.digest.cmp(&right.digest));
    assert_eq!(
        factory.list_condemnations().await.unwrap(),
        expected,
        "precondition: each crashed sweep stranded its row"
    );
    for (sweep, root) in crashed {
        sweep.abort();
        let cancellation = tokio::time::timeout(std::time::Duration::from_secs(5), sweep)
            .await
            .expect("aborted GC sweep terminates promptly");
        assert!(
            matches!(cancellation, Err(ref error) if error.is_cancelled()),
            "aborted GC sweep must report cancellation: {cancellation:?}"
        );
        drop(root);
    }
    let before_retire = &stranded[2].1;
    let surviving = stranded[..2]
        .iter()
        .map(|(_, digest)| digest.clone())
        .collect::<Vec<_>>();
    let mut listed = backend
        .list()
        .await
        .expect("enumerate physical backend")
        .into_iter()
        .map(|blob| blob.id)
        .collect::<Vec<_>>();
    listed.sort();
    let mut expected_listed = surviving.clone();
    expected_listed.sort();
    assert_eq!(
        listed, expected_listed,
        "precondition: the crash before retiring deleted its bytes, the others did not, \
         so no blob listing can rediscover the orphaned Deleting row"
    );
    drop(factory);

    let reopened = reopen().await;
    assert_eq!(
        reopened.list_condemnations().await.unwrap(),
        expected,
        "the stranded rows survive the cold reopen"
    );
    let fresh = backend
        .put(blob("fresh"), image_meta())
        .await
        .expect("seed a new unreferenced blob");
    let first_log = Arc::new(DeleteLog::over(Arc::clone(&backend)));
    let second_log = Arc::new(DeleteLog::over(Arc::clone(&backend)));
    let (first, second) = tokio::join!(
        reclaim_unreferenced_attachments(reopened.as_ref(), first_log.as_ref(), authorize_all()),
        reclaim_unreferenced_attachments(reopened.as_ref(), second_log.as_ref(), authorize_all()),
    );
    let (first, second) = (
        first.expect("first sweeper completes"),
        second.expect("second sweeper completes"),
    );
    assert_eq!(
        first.adopted_count + second.adopted_count,
        stranded.len(),
        "the two sweepers adopt each stranded row exactly once between them"
    );
    for log in [&first_log, &second_log] {
        let deleted = log.deleted();
        if let Some(new_at) = deleted.iter().position(|digest| digest == &fresh.id) {
            assert!(
                deleted[new_at..]
                    .iter()
                    .all(|digest| !surviving.contains(digest)),
                "a sweeper finishes the rows it adopted before it deletes new candidates: \
                 {deleted:?}"
            );
        }
    }
    let mut deleted = first_log.deleted();
    deleted.extend(second_log.deleted());
    for digest in surviving.iter().chain(std::iter::once(&fresh.id)) {
        assert_eq!(
            deleted.iter().filter(|deleted| *deleted == digest).count(),
            1,
            "`{digest}` is deleted exactly once: {deleted:?}"
        );
    }
    assert!(
        !deleted.contains(before_retire),
        "bytes a crashed sweeper already deleted are not deleted again"
    );
    assert_eq!(
        reopened.list_condemnations().await.unwrap(),
        Vec::new(),
        "every stranded row is retired, including the one no blob listing can find"
    );
    assert!(
        backend.list().await.unwrap().is_empty(),
        "every stranded digest's bytes are deleted"
    );
}

/// Two sweepers that start together over one dead pass's rows adopt each row
/// once and delete its bytes once.
#[expect(
    clippy::unwrap_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
pub async fn concurrent_adoption_deletes_once(
    f: Arc<dyn DeploymentStore>,
    make_bytes: AttachmentBytesFactory,
) {
    let namespace = uuid::Uuid::new_v4();
    create(&f, &format!("concurrent-adoption-{namespace}")).await;
    let backend = make_bytes();
    let dead = open_pass(&f).await;
    let mut stranded = Vec::new();
    for index in 0..4_u8 {
        let reference = backend
            .put(
                format!("concurrent-adoption-{index}-{namespace}").into_bytes(),
                image_meta(),
            )
            .await
            .unwrap();
        assert_eq!(
            f.condemn_attachment(&reference.id, u64::MAX, &dead)
                .await
                .unwrap(),
            AttachmentCondemnation::Condemned
        );
        if index % 2 == 1 {
            assert_eq!(
                f.arm_attachment_delete(&reference.id, &dead).await.unwrap(),
                AttachmentDeleteArming::Armed
            );
        }
        stranded.push(reference.id);
    }
    let before_death = f.begin_attachment_sweep().await.unwrap();
    assert_eq!(
        f.adopt_attachment_condemnations(&before_death)
            .await
            .unwrap()
            .held_by_live_pass
            .len(),
        stranded.len(),
        "a live pass's rows are held, never adopted"
    );
    drop(before_death);
    // The pass dies without settling anything, as a crashed sweeper does.
    drop(dead);

    let first_log = Arc::new(DeleteLog::over(Arc::clone(&backend)));
    let second_log = Arc::new(DeleteLog::over(Arc::clone(&backend)));
    let (first, second) = tokio::join!(
        reclaim_unreferenced_attachments(f.as_ref(), first_log.as_ref(), authorize_all()),
        reclaim_unreferenced_attachments(f.as_ref(), second_log.as_ref(), authorize_all()),
    );
    let (first, second) = (first.unwrap(), second.unwrap());
    assert_eq!(first.adopted_count + second.adopted_count, stranded.len());
    let mut deleted = first_log.deleted();
    deleted.extend(second_log.deleted());
    deleted.sort();
    let mut expected = stranded.clone();
    expected.sort();
    assert_eq!(
        deleted, expected,
        "each adopted digest is deleted exactly once"
    );
    assert!(f.list_condemnations().await.unwrap().is_empty());
    assert!(backend.list().await.unwrap().is_empty());
}

/// A delete that keeps failing is retried by later sweeps, one attempt each,
/// then stalls with a typed reason in the condemnation listing. It never
/// drops the row while the bytes remain, and a stalled row is not retried. A
/// failure retrying cannot change stalls at once.
#[expect(
    clippy::expect_used,
    clippy::unwrap_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
pub async fn persistently_failing_delete_stalls_typed(
    f: Arc<dyn DeploymentStore>,
    make_bytes: AttachmentBytesFactory,
) {
    let namespace = uuid::Uuid::new_v4();
    let session_id = SessionId::from(format!("failing-delete-{namespace}"));
    let store = create(&f, session_id.as_str()).await;
    let backend = Arc::new(FaultingAttachmentStore::over(make_bytes()));
    let reference = backend
        .put(
            format!("failing-delete-{namespace}").into_bytes(),
            image_meta(),
        )
        .await
        .unwrap();
    backend.fail_delete(true);
    for attempt in 1..=MAX_ATTACHMENT_DELETE_ATTEMPTS {
        let report =
            reclaim_unreferenced_attachments(f.as_ref(), backend.as_ref(), authorize_all())
                .await
                .expect("a failed delete completes an incomplete sweep");
        assert_eq!(report.failed_ids, vec![reference.id.clone()]);
        let stalled = attempt == MAX_ATTACHMENT_DELETE_ATTEMPTS;
        assert_eq!(
            report.stalled_ids,
            if stalled {
                vec![reference.id.clone()]
            } else {
                Vec::new()
            },
            "attempt {attempt}"
        );
        let listed = f.list_condemnations().await.unwrap();
        assert_eq!(listed.len(), 1, "attempt {attempt}: the row is kept");
        assert_eq!(listed[0].digest, reference.id);
        assert_eq!(listed[0].phase, AttachmentCondemnationPhase::Condemned);
        assert_eq!(listed[0].delete_attempts, attempt);
        assert!(
            listed[0]
                .last_delete_error
                .as_deref()
                .is_some_and(|error| error.contains("scripted attachment delete failure")),
            "attempt {attempt}: {listed:?}"
        );
        assert_eq!(
            listed[0].stalled,
            stalled.then_some(AttachmentDeleteStallReason::AttemptsExhausted),
            "attempt {attempt}"
        );
        assert!(backend.get(&reference.id).await.is_ok(), "the bytes remain");
    }
    // A stalled row is listed and reported, never retried: the sweep that
    // follows issues no delete and records no attempt, even once the backend
    // would accept it.
    backend.fail_delete(false);
    let log = DeleteLog::over(backend.clone() as Arc<dyn AttachmentStore>);
    let report = reclaim_unreferenced_attachments(f.as_ref(), &log, authorize_all())
        .await
        .unwrap();
    assert_eq!(report.stalled_ids, vec![reference.id.clone()]);
    assert!(report.failed_ids.is_empty() && report.condemn_deferred_ids.is_empty());
    assert_eq!(
        MaintenanceReport::sweep(&report),
        MaintenanceSweep::Incomplete,
        "a stalled delete keeps the sweep incomplete"
    );
    assert!(log.deleted().is_empty(), "a stalled row is never retried");
    assert_eq!(
        f.list_condemnations().await.unwrap()[0].delete_attempts,
        MAX_ATTACHMENT_DELETE_ATTEMPTS
    );
    // A writer can still restore the digest: its successful put clears the
    // stalled condemnation, and the digest is adoptable again.
    record_completed_write(&store, &write_intent(&session_id, &reference.id)).await;
    assert!(f.list_condemnations().await.unwrap().is_empty());

    // A failure retrying cannot change stalls on the first attempt.
    let refused = Arc::new(RefusingDeleteStore {
        inner: make_bytes(),
    });
    let refused_reference = refused
        .put(
            format!("refused-delete-{namespace}").into_bytes(),
            image_meta(),
        )
        .await
        .unwrap();
    let report = reclaim_unreferenced_attachments(f.as_ref(), refused.as_ref(), authorize_all())
        .await
        .expect("a refused delete completes an incomplete sweep");
    assert_eq!(report.stalled_ids, vec![refused_reference.id.clone()]);
    let listed = f.list_condemnations().await.unwrap();
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].digest, refused_reference.id);
    assert_eq!(listed[0].delete_attempts, 1);
    assert_eq!(
        listed[0].stalled,
        Some(AttachmentDeleteStallReason::Refused)
    );
}

/// A backend whose deletes fail for want of authorization.
struct RefusingDeleteStore {
    inner: Arc<dyn AttachmentStore>,
}

#[async_trait::async_trait]
impl AttachmentStore for RefusingDeleteStore {
    async fn put(
        &self,
        bytes: Vec<u8>,
        meta: AttachmentCreateMeta,
    ) -> Result<AttachmentRef, AttachmentStoreError> {
        self.inner.put(bytes, meta).await
    }

    async fn get(&self, id: &AttachmentId) -> Result<StoredAttachment, AttachmentStoreError> {
        self.inner.get(id).await
    }

    async fn delete(&self, _id: &AttachmentId) -> Result<(), AttachmentStoreError> {
        Err(AttachmentStoreError::Backend {
            operation: "delete",
            class: AttachmentStoreFailureClass::Credentials,
            source: "scripted delete authorization failure".into(),
        })
    }

    async fn list(&self) -> Result<Vec<StoredBlobRef>, AttachmentStoreError> {
        self.inner.list().await
    }

    async fn head(&self, id: &AttachmentId) -> Result<Option<StoredBlobRef>, AttachmentStoreError> {
        self.inner.head(id).await
    }
}
