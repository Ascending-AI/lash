//! A root's admission as store laws (FIG-3927 §2.2): the rows an unfinished
//! root was admitted with stay its own across a lane rotation, whichever
//! turn-lane family heads it, and a host cancel cannot withdraw them until the
//! root releases them.

use super::*;
use lash_core::store::{AdmittedHead, DriveFence, RootAdmission};
use lash_core::testing::RuntimePersistenceTestDriveExt as _;
use pretty_assertions::assert_eq;

/// Admit `root` on `head` under `fence`, which must reach its head.
pub(super) async fn admitted_on(
    store: &Arc<dyn RuntimePersistence>,
    fence: &DriveFence,
    session_id: &SessionId,
    root: &str,
    head: AdmittedHead,
) -> RootAdmission {
    assert_eq!(fence.session(), session_id, "the fence is the session's");
    admitted_root(store, fence, root, head).await
}

fn refused_as_admitted(
    outcome: &crate::PendingTurnInputCancelOutcome,
    input: &crate::PendingTurnInput,
    root: &str,
) -> bool {
    matches!(
        outcome,
        crate::PendingTurnInputCancelOutcome::AlreadyAdmitted { input: held, root: holder }
            if held.input_id == input.input_id && holder.as_str() == root
    )
}

/// A lane timeout rotates the drive fence, not an unfinished root's
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
    let first = seal_drive_fence_for_test(&store, &input_session, "input-first").await;
    let head = AdmittedHead::Input(input.input_id.clone());
    let admitted = admitted_on(&store, &first, &input_session, "input-root", head.clone()).await;
    assert_eq!(admitted.input_ids(), vec![input.input_id.clone()]);
    store
        .supersede_drive_epoch_for_test(&first)
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
    assert!(refused_as_admitted(
        &cancelled[0].outcome,
        &input,
        "input-root"
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
        [outcome] if refused_as_admitted(outcome, &input, "input-root")
    ));
    let next = seal_drive_fence_for_test(&store, &input_session, "input-next").await;
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
        .enqueue_queued_work(checkpoint_admissions::queued_draft(
            &batch_session,
            "root-owned batch",
            DeliveryPolicy::EarliestSafeBoundary,
        ))
        .await
        .expect("enqueue batch");
    let first = seal_drive_fence_for_test(&store, &batch_session, "batch-first").await;
    let head = AdmittedHead::Batch(batch.batch_id.clone());
    let admitted = admitted_on(&store, &first, &batch_session, "batch-root", head.clone()).await;
    assert_eq!(admitted.batch_ids(), vec![batch.batch_id.clone()]);
    store
        .supersede_drive_epoch_for_test(&first)
        .await
        .expect("old batch lane expires");
    assert!(
        store
            .cancel_queued_work_batch(&batch_session, &batch.batch_id)
            .await
            .expect("host batch cancellation returns a refusal")
            .is_none()
    );
    let next = seal_drive_fence_for_test(&store, &batch_session, "batch-next").await;
    let resumed = admitted_on(&store, &next, &batch_session, "batch-root", head).await;
    assert_eq!(
        serde_json::to_value(&resumed).expect("encode resumed admission"),
        serde_json::to_value(&admitted).expect("encode recorded admission"),
        "the successor drives the recorded admission"
    );
}

/// FIG-3927 N5: an admitted row is not withdrawable. A host cancel of an
/// input or a batch a root admitted answers `AlreadyAdmitted{root}` (a
/// batch: nothing removed) and changes nothing; once the root's commit hands
/// the rows back, the same cancels succeed.
#[expect(
    clippy::expect_used,
    reason = "conformance fixture fails at the violated durable ownership invariant"
)]
pub async fn an_admitted_row_is_not_withdrawable_until_its_root_releases_it(
    store: Arc<dyn RuntimePersistence>,
) {
    let session = SessionId::from("admitted-row-withdrawal");
    let input = store
        .enqueue_pending_turn_input(pending_next_turn_input_draft(&session, "admitted input"))
        .await
        .expect("enqueue input");
    let fence = seal_drive_fence_for_test(&store, &session, "withdrawal-owner").await;
    let root = "withdrawal-root";
    let admission = admitted_on(
        &store,
        &fence,
        &session,
        root,
        AdmittedHead::Input(input.input_id.clone()),
    )
    .await;
    assert_eq!(admission.input_ids(), vec![input.input_id.clone()]);
    let batch = store
        .enqueue_queued_work(checkpoint_admissions::queued_draft(
            &session,
            "admitted batch",
            DeliveryPolicy::EarliestSafeBoundary,
        ))
        .await
        .expect("enqueue batch");
    let checkpoint = admit_at_checkpoint_for_test(
        &store,
        &fence,
        &TurnId::from(root),
        &TurnId::from(root),
        crate::CheckpointKind::AfterWork,
        "withdrawal-root:step",
        64,
        crate::testing::queued_work_claim_policy(64),
    )
    .await
    .expect("the checkpoint admits the batch");
    assert_eq!(
        checkpoint
            .queued
            .as_ref()
            .map(|queued| queued.batch_ids())
            .unwrap_or_default(),
        vec![batch.batch_id.clone()]
    );

    let before_inputs = store
        .list_pending_turn_inputs(&session)
        .await
        .expect("list inputs");
    let batch_ids = |batches: Vec<crate::QueuedWorkBatch>| {
        batches
            .into_iter()
            .map(|batch| batch.batch_id)
            .collect::<Vec<_>>()
    };
    let before_batches = batch_ids(
        store
            .list_queued_work(&session)
            .await
            .expect("list batches"),
    );
    let cancelled = store
        .cancel_pending_turn_inputs(
            &session,
            &[crate::PendingTurnInputCancelTarget::input_id(
                &input.input_id,
            )],
        )
        .await
        .expect("host cancellation answers");
    assert!(
        refused_as_admitted(&cancelled[0].outcome, &input, root),
        "an admitted input answers AlreadyAdmitted naming its root, got {:?}",
        cancelled[0].outcome
    );
    assert!(
        store
            .cancel_queued_work_batch(&session, &batch.batch_id)
            .await
            .expect("host batch cancellation answers")
            .is_none(),
        "an admitted batch is not removed"
    );
    assert_eq!(
        serde_json::to_value(
            store
                .list_pending_turn_inputs(&session)
                .await
                .expect("list inputs")
        )
        .expect("encode inputs"),
        serde_json::to_value(before_inputs).expect("encode inputs"),
        "a refused cancel changes nothing"
    );
    assert_eq!(
        batch_ids(
            store
                .list_queued_work(&session)
                .await
                .expect("list batches")
        ),
        before_batches,
        "a refused cancel changes nothing"
    );

    // The root hands both rows back open at their positions, then ends.
    end_root(
        &store,
        &fence,
        releasing(root, [input_row(&input), batch_row(&batch)]),
    )
    .await;
    let cancelled = store
        .cancel_pending_turn_inputs(
            &session,
            &[crate::PendingTurnInputCancelTarget::input_id(
                &input.input_id,
            )],
        )
        .await
        .expect("host cancellation after release");
    checkpoint_admissions::expect_cancelled_pending_input(
        cancelled[0].outcome.clone(),
        input.input_id.as_str(),
    );
    assert_eq!(
        store
            .cancel_queued_work_batch(&session, &batch.batch_id)
            .await
            .expect("host batch cancellation after release")
            .map(|removed| removed.batch_id),
        Some(batch.batch_id.clone()),
        "a released batch is withdrawable"
    );
}
