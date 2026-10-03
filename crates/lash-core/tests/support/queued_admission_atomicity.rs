use lash_core::store::{AdmittedHead, RunAdmission, ShiftFence};
use lash_core::testing::RuntimeStoreTestShiftExt as _;
use lash_core::{LeaseOwnerIdentity, RuntimeStore, StoreError, TurnId};
use lash_sansio::SessionId;
use std::sync::Arc;

/// The two writes that bind queued work to a run.
#[derive(Clone, Copy, Debug)]
pub(super) enum Entry {
    /// `admit_run` headed by the first batch.
    Run,
    /// `admit_at_checkpoint` of the running run.
    Checkpoint,
}
pub(super) const ENTRIES: [Entry; 2] = [Entry::Run, Entry::Checkpoint];

/// The run every case admits.
const RUN: &str = "atomicity-run";

pub(super) struct Case {
    pub(super) store: Arc<dyn RuntimeStore>,
    pub(super) ids: Vec<lash_core::BatchId>,
    fence: ShiftFence,
    entry: Entry,
}

/// Prepare a case on `store`, whose catalog has admitted the root session
/// `run`.
pub(super) async fn prepare(store: Arc<dyn RuntimeStore>, entry: Entry) -> Case {
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
        .seal_shift_epoch_for_test(
            &SessionId::from("root"),
            &owner,
            "admissions-executor",
            60_000,
        )
        .await
        .expect("seal the shift epoch")
        .acquired()
        .expect("the shift epoch is sealed");
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
        target_session_id: SessionId::from("root"),
        process_id: process_id(),
        sequence,
        event_type: "process.wake".to_string(),
        process_caused_by: None,
        authority: lash_core::runtime::QueuedWorkAuthority::default(),
        input: text.to_string(),
        created_at_ms: 1,
        trace_cause: Default::default(),
    }
}

impl Case {
    /// Admit the case's rows through its entry point; the batch ids bound.
    pub(super) async fn admit(&self) -> Result<Vec<lash_core::BatchId>, StoreError> {
        let policy = lash_core::testing::queued_work_admission_policy(10);
        let run = TurnId::from(RUN);
        match self.entry {
            Entry::Run => {
                let mut request = lash_core::testing::store_fixtures::admit_run_request_for_test(
                    &self.fence,
                    &run,
                    AdmittedHead::Batch(self.ids[0].clone()),
                );
                request.policy = policy;
                request.turn_cancellation = Some(lash_core::store::TurnCancellationBinding {
                    binding_id: "atomicity-authority".into(),
                    admitted_scope: lash_core::ExecutionScope::turn("root", &run),
                });
                Ok(self
                    .store
                    .admit_run(&request)
                    .await?
                    .map(|admission| admission.batch_ids())
                    .unwrap_or_default())
            }
            Entry::Checkpoint => Ok(
                lash_core::testing::store_fixtures::admit_at_checkpoint_for_test(
                    &self.store,
                    &self.fence,
                    &run,
                    &run,
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

    async fn admit_run(&self, run: &str) -> Result<Option<RunAdmission>, StoreError> {
        let mut request = lash_core::testing::store_fixtures::admit_run_request_for_test(
            &self.fence,
            &TurnId::fixture(run),
            AdmittedHead::Batch(self.ids[0].clone()),
        );
        request.policy = lash_core::testing::queued_work_admission_policy(10);
        self.store.admit_run(&request).await
    }
}

/// An admission holds its rows across a displaced fence, on a real backend.
///
/// The session's one unfinished run owns the rows it admitted: the same
/// run admitted again, under its own fence or a successor's, reads back the
/// recorded admission, and no other run takes them while it is unfinished
/// (FIG-3927). Both halves run on every backend so the two cannot answer
/// differently.
pub(super) async fn an_admission_holds_its_rows_across_a_displaced_fence(
    store: Arc<dyn RuntimeStore>,
    backend: &str,
) {
    let case = prepare(store, Entry::Run).await;
    let first = case
        .admit_run(RUN)
        .await
        .unwrap_or_else(|error| panic!("{backend}: the first admission: {error}"))
        .unwrap_or_else(|| panic!("{backend}: the first fence admits the ready run"));
    assert_eq!(
        first.batch_ids(),
        case.ids,
        "{backend}: the admission takes both rows"
    );
    let again = case
        .admit_run(RUN)
        .await
        .unwrap_or_else(|error| panic!("{backend}: the repeated admission: {error}"));
    assert_eq!(
        again.map(|admission| admission.batch_ids()),
        Some(first.batch_ids()),
        "{backend}: the same run reads back its recorded admission",
    );
    assert!(
        matches!(
            case.admit_run("atomicity-other-run").await,
            Err(StoreError::UnfinishedRunConflict { .. })
        ),
        "{backend}: no other run is admitted while the first is unfinished",
    );

    let successor = case.displace_fence().await;
    let resumed = successor
        .admit_run(RUN)
        .await
        .unwrap_or_else(|error| panic!("{backend}: the successor's resume: {error}"));
    assert_eq!(
        resumed.map(|admission| admission.batch_ids()),
        Some(first.batch_ids()),
        "{backend}: the successor resumes the recorded admission, not a new one",
    );
    assert!(
        matches!(
            successor.admit_run("atomicity-other-run").await,
            Err(StoreError::UnfinishedRunConflict { .. })
        ),
        "{backend}: a successor fence does not free the admitted rows for another run",
    );
}

impl Case {
    /// Seal a successor shift epoch over this case's session.
    async fn displace_fence(&self) -> Case {
        self.store
            .supersede_shift_epoch_for_test(&self.fence)
            .await
            .expect("the incumbent's epoch is superseded");
        let owner =
            LeaseOwnerIdentity::opaque("admissions-successor", "admissions-successor-incarnation");
        let fence = self
            .store
            .seal_shift_epoch_for_test(
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
