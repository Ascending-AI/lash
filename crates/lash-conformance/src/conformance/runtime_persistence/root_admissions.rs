//! A root's admission as store laws (FIG-3927 §2.2): the rows an unfinished
//! root was admitted with stay its own across a lane rotation, whichever
//! turn-lane family heads it.

use super::*;
use lash_core::store::{AdmitRootRequest, AdmittedHead, RootAdmission, SessionHeadRef};
use lash_core::testing::RuntimePersistenceTestClaimExt as _;
use pretty_assertions::assert_eq;

/// An admission request for `root` headed by `head` on an empty session.
fn admit_request(
    authority: &crate::ClaimAuthority,
    session_id: &SessionId,
    root: &str,
    head: AdmittedHead,
) -> AdmitRootRequest {
    AdmitRootRequest {
        session_id: session_id.clone(),
        lease: authority.fence(),
        owner: authority.owner.clone(),
        root: TurnId::from(root),
        head,
        max_inputs: 64,
        policy: lash_core::testing::queued_work_claim_policy(64),
        base: SessionHeadRef {
            generation: 0,
            revision: 0,
            leaf: None,
            checkpoint: None,
        },
        turn_index: 1,
        generation: None,
        admitted_generation: lash_core::engine::BuildGeneration::for_test("conformance"),
    }
}

/// Admit `root` on `head` under `authority`, which must reach its head.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: the admission is established by the setup"
)]
pub(super) async fn admitted_on(
    store: &Arc<dyn RuntimePersistence>,
    authority: &crate::ClaimAuthority,
    session_id: &SessionId,
    root: &str,
    head: AdmittedHead,
) -> RootAdmission {
    store
        .admit_root(&admit_request(authority, session_id, root, head))
        .await
        .expect("admit the root")
        .expect("the root reaches its head")
}

/// A lane timeout rotates physical claim authority, not an unfinished root's
/// ownership of the input it was admitted with. Host cancellation refuses
/// it, and the successor's admission retry answers the same input.
#[expect(
    clippy::expect_used,
    reason = "conformance fixture fails at the violated durable ownership invariant"
)]
pub async fn an_unfinished_roots_input_survives_host_cancellation_after_lane_rotation(
    store: Arc<dyn RuntimePersistence>,
) {
    let input_session = SessionId::from("root-admission-input-cancel-fence");
    let input = store
        .enqueue_pending_turn_input(pending_next_turn_input_draft(
            &input_session,
            "root-owned input",
        ))
        .await
        .expect("enqueue input");
    let first = seal_claim_authority_for_test(&store, &input_session, "input-first").await;
    let head = AdmittedHead::Input(input.input_id.clone());
    let admitted = admitted_on(&store, &first, &input_session, "input-root", head.clone()).await;
    assert_eq!(admitted.input_ids(), vec![input.input_id.clone()]);
    store
        .supersede_claim_epoch_for_test(&first.authority())
        .await
        .expect("old lane expires");
    let cancelled = store
        .cancel_pending_turn_inputs(
            &input_session,
            &[crate::PendingTurnInputCancelTarget::input_id(
                &input.input_id,
            )],
        )
        .await
        .expect("host cancellation returns a refusal");
    assert!(matches!(
        &cancelled[0].outcome,
        crate::PendingTurnInputCancelOutcome::AlreadyClaimed { input: held, .. }
            if held.input_id == input.input_id
    ));
    let suffix = store
        .cancel_pending_turn_input_suffix(
            &input_session,
            &crate::PendingTurnInputCancelTarget::input_id(&input.input_id),
        )
        .await
        .expect("suffix cancellation also refuses root ownership");
    let crate::PendingTurnInputSuffixCancelOutcome::Outcomes { outcomes, .. } = suffix else {
        panic!("expected suffix cancellation outcomes, got {suffix:?}");
    };
    assert!(matches!(
        outcomes.as_slice(),
        [crate::PendingTurnInputCancelOutcome::AlreadyClaimed { input: held, .. }]
            if held.input_id == input.input_id
    ));
    let next = seal_claim_authority_for_test(&store, &input_session, "input-next").await;
    let resumed = admitted_on(&store, &next, &input_session, "input-root", head).await;
    assert_eq!(
        serde_json::to_value(&resumed).expect("encode resumed admission"),
        serde_json::to_value(&admitted).expect("encode recorded admission"),
        "the successor drives the recorded admission"
    );
}

/// The batch half of the input law above: a lane timeout never releases the
/// batch an unfinished root was admitted with.
#[expect(
    clippy::expect_used,
    reason = "conformance fixture fails at the violated durable ownership invariant"
)]
pub async fn an_unfinished_roots_batch_survives_host_cancellation_after_lane_rotation(
    store: Arc<dyn RuntimePersistence>,
) {
    let batch_session = SessionId::from("root-admission-batch-cancel-fence");
    let batch = store
        .enqueue_queued_work(checkpoint_claims::queued_draft(
            &batch_session,
            "root-owned batch",
            DeliveryPolicy::EarliestSafeBoundary,
        ))
        .await
        .expect("enqueue batch");
    let first = seal_claim_authority_for_test(&store, &batch_session, "batch-first").await;
    let head = AdmittedHead::Batch(batch.batch_id.clone());
    let admitted = admitted_on(&store, &first, &batch_session, "batch-root", head.clone()).await;
    assert_eq!(
        admitted
            .queued
            .iter()
            .flat_map(|claim| claim.batches.iter().map(|batch| batch.batch_id.clone()))
            .collect::<Vec<_>>(),
        vec![batch.batch_id.clone()]
    );
    store
        .supersede_claim_epoch_for_test(&first.authority())
        .await
        .expect("old batch lane expires");
    assert!(
        store
            .cancel_queued_work_batch(&batch_session, &batch.batch_id)
            .await
            .expect("host batch cancellation returns a refusal")
            .is_none()
    );
    let next = seal_claim_authority_for_test(&store, &batch_session, "batch-next").await;
    let resumed = admitted_on(&store, &next, &batch_session, "batch-root", head).await;
    assert_eq!(
        serde_json::to_value(&resumed).expect("encode resumed admission"),
        serde_json::to_value(&admitted).expect("encode recorded admission"),
        "the successor drives the recorded admission"
    );
}
