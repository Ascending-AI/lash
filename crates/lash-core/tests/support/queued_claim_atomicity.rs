use lash_core::runtime::{
    DeliveryPolicy, QueuedWorkBatchDraft, QueuedWorkClaim, QueuedWorkClaimBoundary,
};
use lash_core::{LeaseOwnerIdentity, RuntimePersistence, SessionExecutionLease};
use lash_sansio::SessionId;
use std::sync::Arc;

#[derive(Clone, Copy, Debug)]
pub(super) enum Entry {
    Leading,
    Automatic,
    Exact,
    Checkpoint,
}
pub(super) const ENTRIES: [Entry; 4] = [
    Entry::Leading,
    Entry::Automatic,
    Entry::Exact,
    Entry::Checkpoint,
];

pub(super) struct Case {
    pub(super) store: Arc<dyn RuntimePersistence>,
    pub(super) ids: Vec<lash_core::BatchId>,
    owner: LeaseOwnerIdentity,
    lease: SessionExecutionLease,
    entry: Entry,
}

pub(super) async fn prepare(store: Arc<dyn RuntimePersistence>, entry: Entry) -> Case {
    let mut ids = Vec::new();
    for (sequence, task) in [(1, "first"), (2, "second")] {
        // Both turn-work rows are wakes from one process, so they share the
        // process-wake merge key and coalesce like the command pair does.
        let draft = match entry {
            Entry::Leading => QueuedWorkBatchDraft::new(
                "root",
                DeliveryPolicy::EarliestSafeBoundary,
                lash_core::runtime::SessionCommand::ApplyConfigPatch {
                    patch: Box::default(),
                },
            )
            .with_merge_key("atomicity"),
            _ => lash_core::runtime::process_wake_batch_draft(wake(sequence, task)),
        };
        let batch = store
            .enqueue_queued_work(draft)
            .await
            .expect("enqueue claim row");
        ids.push(batch.batch_id);
    }
    let owner = LeaseOwnerIdentity::opaque("claims", "claims-incarnation");
    let lease = store
        .try_claim_session_execution_lease(
            &SessionId::from("root"),
            &owner,
            "claims-executor",
            60_000,
        )
        .await
        .expect("claim execution lease")
        .acquired()
        .expect("execution lease available");
    Case {
        store,
        ids,
        owner,
        lease,
        entry,
    }
}

fn wake(sequence: u64, text: &str) -> lash_core::runtime::ProcessWakeDelivery {
    let process_id = || lash_core::runtime::ProcessId::from("atomicity-process");
    lash_core::runtime::ProcessWakeDelivery {
        version: lash_core::runtime::PROCESS_WAKE_DELIVERY_FORMAT_VERSION,
        wake_id: format!("atomicity-process-wake-{sequence}"),
        target_session_id: SessionId::from("root"),
        process_id: process_id(),
        process_incarnation: lash_core::runtime::ProcessIncarnation::from_registration_sequence(1),
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
    pub(super) async fn claim(&self) -> Option<QueuedWorkClaim> {
        let policy = lash_core::testing::queued_work_claim_policy(10);
        match self.entry {
            Entry::Leading => self
                .store
                .claim_leading_ready_session_command(
                    &SessionId::from("root"),
                    &self.lease.fence(),
                    &self.owner,
                )
                .await
                .expect("leading claim"),
            Entry::Automatic => self
                .store
                .claim_ready_queued_work(
                    &SessionId::from("root"),
                    &self.lease.fence(),
                    &self.owner,
                    QueuedWorkClaimBoundary::Idle,
                    policy,
                )
                .await
                .expect("automatic claim")
                .claim(),
            Entry::Exact => {
                self.store
                    .claim_ready_queued_work_by_batch_ids(
                        &SessionId::from("root"),
                        &self.lease.fence(),
                        &self.owner,
                        QueuedWorkClaimBoundary::Idle,
                        &self.ids,
                        policy,
                    )
                    .await
                    .expect("exact claim")
                    .claim
            }
            Entry::Checkpoint => {
                self.store
                    .claim_checkpoint_work(
                        &SessionId::from("root"),
                        &self.lease.fence(),
                        &self.owner,
                        &lash_core::TurnId::from("turn"),
                        lash_core::CheckpointKind::AfterWork,
                        10,
                        policy,
                    )
                    .await
                    .expect("checkpoint claim")
                    .1
            }
        }
    }
}

/// The claimability verdict's two answers, over one row, on a real backend.
///
/// `queued_work_batch_claimability` (FIG-3381, called by both stores since
/// FIG-3383) says a row is claimable when it is unclaimed **or** claimed under
/// a superseded session-execution-lease generation, and refuses it when the
/// claiming generation already holds it. Both halves are safety properties:
/// the first is how a crashed runner's work is recovered, and the second is
/// what stops one generation holding two claims over one row (ADR 0029).
///
/// This runs on every backend, because the whole point of moving the decision
/// into shared code is that the two cannot answer differently. The SQL
/// predicate is still on each statement as the backstop, so a store that
/// dropped the verdict call would still pass the first half — the second half
/// is the one that fails, and it fails identically on both.
pub(super) async fn claimability_verdict_holds_over_a_displaced_generation(
    store: Arc<dyn RuntimePersistence>,
    backend: &str,
) {
    let case = prepare(store, Entry::Automatic).await;
    let first = case
        .claim()
        .await
        .unwrap_or_else(|| panic!("{backend}: the first generation claims the ready run"));

    // The same generation must not take the rows it already holds.
    assert!(
        case.claim().await.is_none(),
        "{backend}: a generation that already holds these rows must not claim them again",
    );

    // Displace the lane. The new holder's generation is a different one, so
    // the same rows become claimable again without anything releasing them.
    let successor = case.displace_lease().await;
    let second = successor
        .claim()
        .await
        .unwrap_or_else(|| panic!("{backend}: a displacing generation reclaims the held rows"));
    assert_ne!(
        first.claim_id, second.claim_id,
        "{backend}: the successor must take its own claim over the same rows",
    );
    assert_eq!(
        second.data.batches.len(),
        first.data.batches.len(),
        "{backend}: the successor recovers the whole interrupted run",
    );
    assert!(
        successor.claim().await.is_none(),
        "{backend}: the successor must not claim its own rows twice either",
    );
}

impl Case {
    /// Take this session's lane for a fresh owner, advancing the generation.
    ///
    /// The incumbent hands the lane back first, which is what a runner does
    /// when it stands down; the claims it left behind keep pointing at the
    /// generation that is now gone, which is exactly the state a successor has
    /// to recover from.
    async fn displace_lease(&self) -> Case {
        self.store
            .release_session_execution_lease(&self.lease.fence())
            .await
            .expect("the incumbent hands its lane back");
        let owner = LeaseOwnerIdentity::opaque("claims-successor", "claims-successor-incarnation");
        let lease = self
            .store
            .try_claim_session_execution_lease(
                &SessionId::from("root"),
                &owner,
                "claims-successor-executor",
                60_000,
            )
            .await
            .expect("claim the successor execution lease")
            .acquired()
            .expect("the successor execution lease is available");
        assert_ne!(
            lease.fencing_token, self.lease.fencing_token,
            "a displacing claim must advance the generation",
        );
        Case {
            store: Arc::clone(&self.store),
            ids: self.ids.clone(),
            owner,
            lease,
            entry: self.entry,
        }
    }
}
