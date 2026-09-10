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
    pub(super) ids: Vec<String>,
    owner: LeaseOwnerIdentity,
    lease: SessionExecutionLease,
    entry: Entry,
}

pub(super) async fn prepare(store: Arc<dyn RuntimePersistence>, entry: Entry) -> Case {
    let mut ids = Vec::new();
    for task in ["first", "second"] {
        let payload: lash_core::runtime::QueuedWorkBatchPayloads = match entry {
            Entry::Leading => lash_core::runtime::SessionCommand::ApplyConfigPatch {
                patch: Box::default(),
            }
            .into(),
            _ => lash_core::runtime::TurnWorkPayload::agent_frame_task(
                lash_core::facade_support::frame_node_id(&SessionId::from("root"), "frame"),
                task,
                None,
            )
            .into(),
        };
        let batch = store
            .enqueue_queued_work(
                QueuedWorkBatchDraft::new("root", DeliveryPolicy::EarliestSafeBoundary, payload)
                    .with_merge_key("atomicity"),
            )
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
