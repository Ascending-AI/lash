use lash_core::{RuntimeStore, StoreError, TurnId};
use lash_sansio::SessionId;
use std::sync::Arc;

/// The run every case admits at its checkpoint.
const RUN: &str = "atomicity-run";

pub(super) struct Case {
    pub(super) store: Arc<dyn RuntimeStore>,
    pub(super) ids: Vec<lash_core::BatchId>,
}

/// Prepare a case on `store`, whose catalog has admitted the root session.
pub(super) async fn prepare(store: Arc<dyn RuntimeStore>) -> Case {
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
    Case { store, ids }
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
    /// Admit the case's rows at the running run's checkpoint; the batch ids
    /// bound.
    pub(super) async fn admit(&self) -> Result<Vec<lash_core::BatchId>, StoreError> {
        let run = TurnId::from(RUN);
        Ok(
            lash_core::testing::store_fixtures::admit_at_checkpoint_for_test(
                &self.store,
                &SessionId::from("root"),
                &run,
                &run,
                lash_core::CheckpointKind::AfterWork,
                "atomicity-checkpoint",
                10,
                lash_core::testing::queued_work_admission_policy(10),
            )
            .await?
            .queued
            .map(|queued| queued.batch_ids())
            .unwrap_or_default(),
        )
    }
}
