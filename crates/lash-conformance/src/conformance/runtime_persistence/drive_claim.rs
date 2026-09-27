//! Claim fencing and orphan repair after a newer drive admission.

use super::*;
use lash_core::store::{AdmissionId, ClaimAuthority, DriveEpochSeal, DriveFence, RootStartNonce};

#[expect(clippy::expect_used, reason = "conformance fixture setup")]
async fn seeded(store: &Arc<dyn RuntimePersistence>, session_id: &SessionId) {
    let mut state = RuntimeSessionState {
        session_id: session_id.clone(),
        ..RuntimeSessionState::new(crate::SessionPolicy::new(crate::TurnBudget::Unbounded))
    };
    state.ensure_agent_frame_initialized();
    lash_core::testing::store_fixtures::commit_conformance_state(store, &mut state)
        .await
        .expect("seed claim-fence session");
}

#[expect(clippy::expect_used, reason = "conformance fixture setup")]
async fn seal(
    store: &Arc<dyn RuntimePersistence>,
    session_id: &SessionId,
    admission: &str,
    observed_epoch: u64,
) -> DriveFence {
    match store
        .seal_drive_epoch(
            session_id,
            &AdmissionId::new(admission),
            observed_epoch,
            &RootStartNonce::new(admission),
        )
        .await
        .expect("seal claim drive")
    {
        DriveEpochSeal::Sealed(fence) => fence,
        other => panic!("claim drive did not seal: {other:?}"),
    }
}

#[expect(clippy::expect_used, reason = "conformance law assertions")]
pub async fn a_stale_drive_epoch_refuses_a_claim(store: Arc<dyn RuntimePersistence>) {
    let session_id = SessionId::from("claim-stale-drive");
    seeded(&store, &session_id).await;
    store
        .enqueue_pending_turn_input(pending_next_turn_input_draft(&session_id, "pending"))
        .await
        .expect("enqueue pending input");
    let prior = seal(&store, &session_id, "first", 0).await;
    let current = seal(&store, &session_id, "second", prior.epoch()).await;
    let stale = ClaimAuthority::from_drive_fence(&prior);
    assert!(matches!(
        store
            .claim_next_turn_inputs(&session_id, &stale, &stale.owner, 1)
            .await,
        Err(StoreError::StaleDriveFence {
            fence_epoch: 1,
            current_epoch: 2,
            ..
        })
    ));
    let current = ClaimAuthority::from_drive_fence(&current);
    assert!(
        store
            .claim_next_turn_inputs(&session_id, &current, &current.owner, 1)
            .await
            .expect("current drive claims")
            .is_some()
    );
}

#[expect(clippy::expect_used, reason = "conformance law assertions")]
pub async fn a_new_drive_repairs_an_older_claim_without_a_ttl(store: Arc<dyn RuntimePersistence>) {
    let session_id = SessionId::from("claim-orphan-drive");
    let turn_id = TurnId::from("orphaned-turn");
    seeded(&store, &session_id).await;
    let input = store
        .enqueue_pending_turn_input(pending_active_turn_input_draft(
            &session_id,
            &turn_id,
            crate::TurnInputCheckpointBoundary::AfterWork,
            "orphaned input",
        ))
        .await
        .expect("enqueue active input");
    let prior = seal(&store, &session_id, "first", 0).await;
    let prior_authority = ClaimAuthority::from_drive_fence(&prior);
    assert!(
        store
            .claim_active_turn_inputs(
                &session_id,
                &prior_authority,
                &prior_authority.owner,
                &turn_id,
                crate::CheckpointKind::AfterWork,
                1,
            )
            .await
            .expect("claim active input")
            .is_some()
    );
    let current = seal(&store, &session_id, "second", prior.epoch()).await;
    let current_authority = ClaimAuthority::from_drive_fence(&current);
    assert_eq!(
        store
            .orphaned_active_turn_ids(
                &session_id,
                &current_authority,
                crate::OrphanedTurnInputScope::LaneGeneration {
                    resumable_turn_id: None,
                },
            )
            .await
            .expect("find older-epoch claim"),
        vec![turn_id.clone()]
    );
    let repaired = store
        .repair_orphaned_active_turn_inputs(
            &session_id,
            &current_authority,
            &turn_id,
            &crate::TurnCancelIntentSnapshot::Absent,
            None,
        )
        .await
        .expect("repair older-epoch claim")
        .into_applied()
        .expect("absent cancel intent stayed absent");
    assert_eq!(repaired.len(), 1);
    assert_eq!(repaired.affected_inputs[0].input_id, input.input_id);
}

#[expect(clippy::expect_used, reason = "conformance law assertions")]
pub async fn a_new_incarnation_reclaims_within_the_same_drive_epoch(
    store: Arc<dyn RuntimePersistence>,
) {
    let input_session = SessionId::from("claim-new-incarnation");
    seeded(&store, &input_session).await;
    store
        .enqueue_pending_turn_input(pending_next_turn_input_draft(&input_session, "pending"))
        .await
        .expect("enqueue input");
    let fence = seal(&store, &input_session, "input-drive", 0).await;
    let mut authority = ClaimAuthority::from_drive_fence(&fence);
    let old_owner = crate::LeaseOwnerIdentity::opaque("worker", "old-incarnation");
    let new_owner = crate::LeaseOwnerIdentity::opaque("worker", "new-incarnation");
    authority.owner = old_owner.clone();
    let old = store
        .claim_next_turn_inputs(&input_session, &authority, &old_owner, 1)
        .await
        .expect("first incarnation claims input")
        .expect("input claim exists");
    assert!(
        store
            .claim_next_turn_inputs(&input_session, &authority, &old_owner, 1)
            .await
            .expect("same incarnation rechecks input")
            .is_none(),
        "a repeat by the same incarnation remains held"
    );
    authority.owner = new_owner.clone();
    let fresh = store
        .claim_next_turn_inputs(&input_session, &authority, &new_owner, 1)
        .await
        .expect("new incarnation reclaims input")
        .expect("new incarnation takes over input");
    assert!(fresh.fencing_token > old.fencing_token);
    let state = crate::load_persisted_session_state(store.as_ref())
        .await
        .expect("read state before input settlement")
        .expect("session state remains present");
    let error = store
        .commit_runtime_state(
            RuntimeCommit::persisted_state_for_test(&state, &[])
                .completing_turn_input_claim(old.completion()),
        )
        .await
        .expect_err("old incarnation cannot settle the replacement claim");
    assert!(
        matches!(error, StoreError::TurnInputClaimSuperseded { .. }),
        "stale input settlement returned {error:?}"
    );
    store
        .commit_runtime_state(
            RuntimeCommit::persisted_state_for_test(&state, &[])
                .completing_turn_input_claim(fresh.completion()),
        )
        .await
        .expect("new incarnation settles input");

    let queue_session = input_session;
    store
        .enqueue_queued_work(queued_draft(
            &queue_session,
            "queued",
            crate::DeliveryPolicy::EarliestSafeBoundary,
        ))
        .await
        .expect("enqueue work");
    authority.owner = old_owner.clone();
    let old = store
        .claim_ready_queued_work(
            &queue_session,
            &authority,
            &old_owner,
            crate::QueuedWorkClaimBoundary::Idle,
            crate::testing::queued_work_claim_policy(1),
        )
        .await
        .expect("first incarnation claims work")
        .claim()
        .expect("work claim exists");
    assert!(
        store
            .claim_ready_queued_work(
                &queue_session,
                &authority,
                &old_owner,
                crate::QueuedWorkClaimBoundary::Idle,
                crate::testing::queued_work_claim_policy(1),
            )
            .await
            .expect("same incarnation rechecks work")
            .claim()
            .is_none(),
        "a repeat by the same incarnation remains held"
    );
    authority.owner = new_owner.clone();
    let fresh = store
        .claim_ready_queued_work(
            &queue_session,
            &authority,
            &new_owner,
            crate::QueuedWorkClaimBoundary::Idle,
            crate::testing::queued_work_claim_policy(1),
        )
        .await
        .expect("new incarnation reclaims work")
        .claim()
        .expect("new incarnation takes over work");
    assert!(fresh.fencing_token > old.fencing_token);
    let state = crate::load_persisted_session_state(store.as_ref())
        .await
        .expect("read state after input settlement")
        .expect("session state remains present");
    let error = store
        .commit_runtime_state(
            RuntimeCommit::persisted_state_for_test(&state, &[])
                .completing_queue_claim(old.completion()),
        )
        .await
        .expect_err("old incarnation cannot settle the replacement work claim");
    assert!(
        matches!(error, StoreError::QueuedWorkClaimSuperseded { .. }),
        "stale work settlement returned {error:?}"
    );
    store
        .commit_runtime_state(
            RuntimeCommit::persisted_state_for_test(&state, &[])
                .completing_queue_claim(fresh.completion()),
        )
        .await
        .expect("new incarnation settles work");
}
