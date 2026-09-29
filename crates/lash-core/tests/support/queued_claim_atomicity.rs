use lash_core::store::{AdmittedHead, DriveFence, RootAdmission};
use lash_core::testing::RuntimePersistenceTestDriveExt as _;
use lash_core::{LeaseOwnerIdentity, RuntimePersistence, StoreError, TurnId};
use lash_sansio::SessionId;
use std::sync::Arc;

/// The two writes that bind queued work to a root.
#[derive(Clone, Copy, Debug)]
pub(super) enum Entry {
    /// `admit_root` headed by the first batch.
    Root,
    /// `admit_at_checkpoint` of the running root.
    Checkpoint,
}
pub(super) const ENTRIES: [Entry; 2] = [Entry::Root, Entry::Checkpoint];

/// The root every case admits.
const ROOT: &str = "atomicity-root";

pub(super) struct Case {
    pub(super) store: Arc<dyn RuntimePersistence>,
    pub(super) ids: Vec<lash_core::BatchId>,
    fence: DriveFence,
    entry: Entry,
}

pub(super) async fn prepare(store: Arc<dyn RuntimePersistence>, entry: Entry) -> Case {
    let mut ids = Vec::new();
    for (sequence, task) in [(1, "first"), (2, "second")] {
        // Both rows are wakes from one process, so they share the
        // process-wake merge key and one admission takes both.
        let batch = store
            .enqueue_queued_work(lash_core::runtime::process_wake_batch_draft(wake(
                sequence, task,
            )))
            .await
            .expect("enqueue the admission row");
        ids.push(batch.batch_id);
    }
    let owner = LeaseOwnerIdentity::opaque("admissions", "admissions-incarnation");
    let fence = store
        .seal_drive_epoch_for_test(
            &SessionId::from("root"),
            &owner,
            "admissions-executor",
            60_000,
        )
        .await
        .expect("seal the drive epoch")
        .acquired()
        .expect("the drive epoch is sealed");
    Case {
        store,
        ids,
        fence,
        entry,
    }
}

fn wake(sequence: u64, text: &str) -> lash_core::runtime::ProcessWakeDelivery {
    let process_id = || lash_core::runtime::ProcessId::fixture("atomicity-process");
    lash_core::runtime::ProcessWakeDelivery {
        version: lash_core::runtime::PROCESS_WAKE_DELIVERY_FORMAT_VERSION,
        wake_id: format!("atomicity-process-wake-{sequence}"),
        target_session_id: SessionId::from("root"),
        process_id: process_id(),
        sequence,
        event_type: "process.wake".to_string(),
        event_invocation: lash_core::runtime::RuntimeInvocation {
            attribution: lash_core::runtime::RuntimeAttribution::for_session("root"),
            subject: lash_core::runtime::RuntimeSubject::ProcessEvent {
                process_id: process_id(),
                sequence,
                event_type: "process.wake".to_string(),
            },
            caused_by: None,
            replay: None,
        },
        process_caused_by: None,
        authority: lash_core::runtime::QueuedWorkAuthority::default(),
        input: text.to_string(),
        created_at_ms: 1,
    }
}

impl Case {
    /// Admit the case's rows through its entry point; the batch ids bound.
    pub(super) async fn admit(&self) -> Result<Vec<lash_core::BatchId>, StoreError> {
        let policy = lash_core::testing::queued_work_admission_policy(10);
        let root = TurnId::from(ROOT);
        match self.entry {
            Entry::Root => {
                let mut request = lash_core::testing::store_fixtures::admit_root_request_for_test(
                    &self.fence,
                    &root,
                    AdmittedHead::Batch(self.ids[0].clone()),
                );
                request.policy = policy;
                Ok(self
                    .store
                    .admit_root(&request)
                    .await?
                    .map(|admission| admission.batch_ids())
                    .unwrap_or_default())
            }
            Entry::Checkpoint => Ok(
                lash_core::testing::store_fixtures::admit_at_checkpoint_for_test(
                    &self.store,
                    &self.fence,
                    &root,
                    &root,
                    lash_core::CheckpointKind::AfterWork,
                    "atomicity-checkpoint",
                    10,
                    policy,
                )
                .await?
                .queued
                .map(|queued| queued.batch_ids())
                .unwrap_or_default(),
            ),
        }
    }

    async fn admit_root(&self, root: &str) -> Result<Option<RootAdmission>, StoreError> {
        let mut request = lash_core::testing::store_fixtures::admit_root_request_for_test(
            &self.fence,
            &TurnId::from(root),
            AdmittedHead::Batch(self.ids[0].clone()),
        );
        request.policy = lash_core::testing::queued_work_admission_policy(10);
        self.store.admit_root(&request).await
    }
}

/// An admission holds its rows across a displaced fence, on a real backend.
///
/// The session's one unfinished root owns the rows it admitted: the same
/// root admitted again, under its own fence or a successor's, reads back the
/// recorded admission, and no other root takes them while it is unfinished
/// (FIG-3927). Both halves run on every backend so the two cannot answer
/// differently.
pub(super) async fn an_admission_holds_its_rows_across_a_displaced_fence(
    store: Arc<dyn RuntimePersistence>,
    backend: &str,
) {
    let case = prepare(store, Entry::Root).await;
    let first = case
        .admit_root(ROOT)
        .await
        .unwrap_or_else(|error| panic!("{backend}: the first admission: {error}"))
        .unwrap_or_else(|| panic!("{backend}: the first fence admits the ready run"));
    assert_eq!(
        first.batch_ids(),
        case.ids,
        "{backend}: the admission takes both rows"
    );
    let again = case
        .admit_root(ROOT)
        .await
        .unwrap_or_else(|error| panic!("{backend}: the repeated admission: {error}"));
    assert_eq!(
        again.map(|admission| admission.batch_ids()),
        Some(first.batch_ids()),
        "{backend}: the same root reads back its recorded admission",
    );
    assert!(
        matches!(
            case.admit_root("atomicity-other-root").await,
            Err(StoreError::UnfinishedRootConflict { .. })
        ),
        "{backend}: no other root is admitted while the first is unfinished",
    );

    let successor = case.displace_fence().await;
    let resumed = successor
        .admit_root(ROOT)
        .await
        .unwrap_or_else(|error| panic!("{backend}: the successor's resume: {error}"));
    assert_eq!(
        resumed.map(|admission| admission.batch_ids()),
        Some(first.batch_ids()),
        "{backend}: the successor resumes the recorded admission, not a new one",
    );
    assert!(
        matches!(
            successor.admit_root("atomicity-other-root").await,
            Err(StoreError::UnfinishedRootConflict { .. })
        ),
        "{backend}: a successor fence does not free the admitted rows for another root",
    );
}

impl Case {
    /// Seal a successor drive epoch over this case's session.
    async fn displace_fence(&self) -> Case {
        self.store
            .supersede_drive_epoch_for_test(&self.fence)
            .await
            .expect("the incumbent's epoch is superseded");
        let owner =
            LeaseOwnerIdentity::opaque("admissions-successor", "admissions-successor-incarnation");
        let fence = self
            .store
            .seal_drive_epoch_for_test(
                &SessionId::from("root"),
                &owner,
                "admissions-successor-executor",
                60_000,
            )
            .await
            .expect("seal the successor epoch")
            .acquired()
            .expect("the successor epoch is sealed");
        assert!(
            fence.epoch() > self.fence.epoch(),
            "a successor seal advances the epoch",
        );
        Case {
            store: Arc::clone(&self.store),
            ids: self.ids.clone(),
            fence,
            entry: self.entry,
        }
    }
}
