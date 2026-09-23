//! A turn-input claim bound to the aborted direct turn that held it
//! (FIG-3589, ADR 0069 §7), at the store seam.
//!
//! The runtime binds a direct turn's drive claim when the turn aborts with
//! `Err`. From then on the rows stop lapsing with the claim's lease
//! generation: no claim under any generation takes them, and they are not
//! claimable work, until the aborted turn's redrive re-takes them or a cancel
//! of one of them returns the rest to the queue.

use super::*;

#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
async fn statuses(
    store: &Arc<dyn RuntimePersistence>,
    session_id: &SessionId,
) -> Vec<(crate::InputId, crate::PendingTurnInputReadStatus)> {
    store
        .list_pending_turn_inputs(session_id)
        .await
        .expect("list pending inputs")
        .into_iter()
        .map(|read| (read.input.input_id, read.status))
        .collect()
}

#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
pub async fn bound_turn_input_claim_is_excluded_until_its_turn_retakes_it(
    store: Arc<dyn RuntimePersistence>,
) {
    let session_id = SessionId::from("turn-bound-claims");
    let aborted_turn = TurnId::from("aborted-direct-turn");
    let first = store
        .enqueue_pending_turn_input(pending_next_turn_input_draft(&session_id, "first"))
        .await
        .expect("enqueue the first input");
    let second = store
        .enqueue_pending_turn_input(pending_next_turn_input_draft(&session_id, "second"))
        .await
        .expect("enqueue the second input");

    let aborted_lease =
        claim_session_execution_lease_for_test(&store, &session_id, "aborted-owner").await;
    let drive = store
        .claim_next_turn_inputs(
            &session_id,
            &aborted_lease.fence(),
            &lease_owner("aborted-owner"),
            1,
        )
        .await
        .expect("claim the aborted turn's drive")
        .expect("the first input is claimable");
    assert_eq!(drive.inputs.len(), 1);
    store
        .bind_turn_input_claim(&drive, &aborted_turn, &first.input_id)
        .await
        .expect("bind the drive claim to the aborted turn");
    release_session_execution_lease_for_test(&store, &aborted_lease).await;
    let bound = crate::PendingTurnInputReadStatus::TurnBound {
        turn_id: aborted_turn.clone(),
        receipt_input_id: first.input_id.clone(),
    };
    assert_eq!(
        statuses(&store, &session_id).await,
        vec![
            (first.input_id.clone(), bound.clone()),
            (
                second.input_id.clone(),
                crate::PendingTurnInputReadStatus::Pending
            ),
        ],
        "a released lease does not free a bound claim"
    );

    // A successor generation takes only the unbound row, and another turn's
    // redrive takes nothing.
    let successor =
        claim_session_execution_lease_for_test(&store, &session_id, "successor-owner").await;
    let successor_claim = store
        .claim_next_turn_inputs(
            &session_id,
            &successor.fence(),
            &lease_owner("successor-owner"),
            10,
        )
        .await
        .expect("claim under the successor generation")
        .expect("the unbound input is claimable");
    assert_eq!(
        successor_claim
            .inputs
            .iter()
            .map(|input| input.input_id.clone())
            .collect::<Vec<_>>(),
        vec![second.input_id.clone()],
        "no generation reclaims a bound row"
    );
    assert!(
        store
            .reclaim_turn_bound_inputs(
                &session_id,
                &successor.fence(),
                &lease_owner("successor-owner"),
                &TurnId::from("some-other-turn"),
            )
            .await
            .expect("another turn's redrive probe")
            .is_none(),
        "only the bound turn's redrive re-takes its rows"
    );
    store
        .abandon_turn_input_claim(&successor_claim)
        .await
        .expect("hand the successor's claim back");

    // The aborted turn's redrive re-takes exactly its bound rows under its own
    // generation, and the binding is gone.
    let redrive = store
        .reclaim_turn_bound_inputs(
            &session_id,
            &successor.fence(),
            &lease_owner("successor-owner"),
            &aborted_turn,
        )
        .await
        .expect("the aborted turn's redrive re-takes its rows")
        .expect("the bound row is re-taken");
    assert_eq!(
        redrive
            .inputs
            .iter()
            .map(|input| input.input_id.clone())
            .collect::<Vec<_>>(),
        vec![first.input_id.clone()]
    );
    assert!(
        matches!(
            statuses(&store, &session_id).await[0].1,
            crate::PendingTurnInputReadStatus::Held { .. }
        ),
        "the re-taken row is held by the redrive's live generation"
    );

    // Binding a claim that no longer holds the row is a no-op.
    store
        .bind_turn_input_claim(&drive, &aborted_turn, &first.input_id)
        .await
        .expect("bind a superseded claim");
    assert!(
        matches!(
            statuses(&store, &session_id).await[0].1,
            crate::PendingTurnInputReadStatus::Held { .. }
        ),
        "a superseded claim binds nothing"
    );
    release_session_execution_lease_for_test(&store, &successor).await;

    // A cancel of a bound row other than the receipt's is refused; a cancel of
    // the receipt's input returns the claim's other rows to the queue, unbound.
    let third_lease =
        claim_session_execution_lease_for_test(&store, &session_id, "third-owner").await;
    let absorbing = store
        .claim_next_turn_inputs(
            &session_id,
            &third_lease.fence(),
            &lease_owner("third-owner"),
            10,
        )
        .await
        .expect("claim both rows")
        .expect("both rows are claimable");
    assert_eq!(absorbing.inputs.len(), 2);
    store
        .bind_turn_input_claim(&absorbing, &aborted_turn, &second.input_id)
        .await
        .expect("bind the absorbing claim");
    release_session_execution_lease_for_test(&store, &third_lease).await;
    let refused = store
        .cancel_pending_turn_input(&session_id, &first.input_id)
        .await
        .expect("cancel a bound row other than the receipt's");
    assert!(
        matches!(
            &refused,
            crate::PendingTurnInputCancelOutcome::TurnBound {
                turn_id,
                receipt_input_id,
                ..
            } if *turn_id == aborted_turn && *receipt_input_id == second.input_id
        ),
        "a cancel of a bound row other than the receipt's is refused: {refused:?}"
    );
    assert_eq!(
        statuses(&store, &session_id).await.len(),
        2,
        "the refused cancel changed nothing"
    );
    assert!(
        store
            .cancel_pending_turn_input(&session_id, &second.input_id)
            .await
            .expect("cancel the receipt's input")
            .is_cancelled(),
        "the receipt's input is cancellable"
    );
    assert_eq!(
        statuses(&store, &session_id).await,
        vec![(
            first.input_id.clone(),
            crate::PendingTurnInputReadStatus::Pending
        )],
        "the cancel returns the bound claim's other row to the queue"
    );

    // A turn whose drive outcome was lost binds by its receipt's row, fenced
    // by the generation the drive ran under.
    let third = store
        .enqueue_pending_turn_input(pending_next_turn_input_draft(&session_id, "third"))
        .await
        .expect("enqueue the third input");
    let lost_drive_lease =
        claim_session_execution_lease_for_test(&store, &session_id, "lost-drive-owner").await;
    let lost_drive = store
        .claim_next_turn_inputs(
            &session_id,
            &lost_drive_lease.fence(),
            &lease_owner("lost-drive-owner"),
            10,
        )
        .await
        .expect("claim the lost drive")
        .expect("both open rows are claimable");
    assert_eq!(lost_drive.inputs.len(), 2);
    let generation = lost_drive_lease.fence().fencing_token;
    let lost_turn = TurnId::from("lost-drive-turn");
    store
        .bind_turn_input_claim_of_receipt(&session_id, &third.input_id, generation + 1, &lost_turn)
        .await
        .expect("bind under another generation");
    assert!(
        statuses(&store, &session_id)
            .await
            .iter()
            .all(|(_, status)| matches!(status, crate::PendingTurnInputReadStatus::Held { .. })),
        "a claim taken under another generation binds nothing"
    );
    store
        .bind_turn_input_claim_of_receipt(&session_id, &third.input_id, generation, &lost_turn)
        .await
        .expect("bind by the receipt's row");
    let lost_bound = crate::PendingTurnInputReadStatus::TurnBound {
        turn_id: lost_turn,
        receipt_input_id: third.input_id.clone(),
    };
    assert_eq!(
        statuses(&store, &session_id).await,
        vec![
            (first.input_id.clone(), lost_bound.clone()),
            (third.input_id.clone(), lost_bound),
        ],
        "the whole claim the receipt's row carries is bound"
    );
    release_session_execution_lease_for_test(&store, &lost_drive_lease).await;
}

/// A direct turn that absorbs a pending queued run's frozen inputs and aborts
/// binds none of them: they lapse to the run, whose retry re-claims them
/// (FIG-3589).
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
pub async fn bound_claim_skips_a_pending_queued_runs_inputs(store: Arc<dyn RuntimePersistence>) {
    let session_id = SessionId::from("turn-bound-queued-run");
    let state = RuntimeSessionState {
        session_id: session_id.clone(),
        ..RuntimeSessionState::new(crate::SessionPolicy::new(crate::TurnBudget::Unbounded))
    };
    let member = store
        .enqueue_pending_turn_input(pending_next_turn_input_draft(&session_id, "run member"))
        .await
        .expect("enqueue the run's input");
    let run_lease = claim_session_execution_lease_for_test(&store, &session_id, "run").await;
    let admission = store
        .begin_or_resume_queued_run(
            &run_lease.authority(),
            lash_core::store::BeginQueuedRun {
                session_id: session_id.clone(),
                identity: None,
                request: lash_core::store::QueuedRunRequest::Automatic,
                configuration: RuntimeCommit::persisted_state_for_test(&state, &[]).config,
                expected_head_revision: 0,
                initial_turn_index: 1,
            },
        )
        .await
        .expect("admit the queued run");
    let selected = store
        .select_queued_run(
            &run_lease.authority(),
            &admission.scope,
            &run_lease.owner,
            64,
            &admission.configuration,
            lash_core::testing::queued_work_claim_policy(64),
        )
        .await
        .expect("select the run's members");
    assert_eq!(selected.inputs.len(), 1);
    release_session_execution_lease_for_test(&store, &run_lease).await;

    // Under the next generation a direct turn's drive absorbs the run's
    // lapsed member, then aborts and binds its claim.
    let direct_lease = claim_session_execution_lease_for_test(&store, &session_id, "direct").await;
    let absorbed = store
        .claim_next_turn_inputs(
            &session_id,
            &direct_lease.fence(),
            &lease_owner("direct"),
            64,
        )
        .await
        .expect("claim under the direct turn's generation")
        .expect("the run's lapsed member is claimable");
    assert_eq!(absorbed.inputs[0].input_id, member.input_id);
    store
        .bind_turn_input_claim(&absorbed, &TurnId::from("aborted-direct"), &member.input_id)
        .await
        .expect("bind the aborted drive");
    release_session_execution_lease_for_test(&store, &direct_lease).await;
    assert!(
        statuses(&store, &session_id)
            .await
            .iter()
            .all(|(_, status)| !matches!(
                status,
                crate::PendingTurnInputReadStatus::TurnBound { .. }
            )),
        "a pending queued run's input is never bound"
    );

    let retry_lease = claim_session_execution_lease_for_test(&store, &session_id, "retry").await;
    let resumed = store
        .select_queued_run(
            &retry_lease.authority(),
            &admission.scope,
            &retry_lease.owner,
            64,
            &admission.configuration,
            lash_core::testing::queued_work_claim_policy(64),
        )
        .await
        .expect("the run's retry re-claims its member");
    assert_eq!(
        resumed
            .inputs
            .iter()
            .flat_map(|claim| claim.inputs.iter().map(|input| input.input_id.clone()))
            .collect::<Vec<_>>(),
        vec![member.input_id.clone()],
        "the run's retry drives its own member"
    );
    release_session_execution_lease_for_test(&store, &retry_lease).await;
}
