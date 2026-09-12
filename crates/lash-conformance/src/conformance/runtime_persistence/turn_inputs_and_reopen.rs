use super::*;
use pretty_assertions::assert_eq;

pub(super) async fn pending_turn_inputs_source_keys_order_cancel_and_cross_session(
    store: Arc<dyn RuntimePersistence>,
) {
    let first = store
        .enqueue_pending_turn_input(
            pending_next_turn_input_draft(&SessionId::from("root"), "first")
                .with_source_key("source:first"),
        )
        .await
        .expect("enqueue first pending input");
    let replay = store
        .enqueue_pending_turn_input(
            pending_next_turn_input_draft(&SessionId::from("root"), "first")
                .with_source_key("source:first"),
        )
        .await
        .expect("replay first pending input");
    let conflict = store
        .enqueue_pending_turn_input(
            pending_next_turn_input_draft(&SessionId::from("root"), "different replay payload")
                .with_source_key("source:first"),
        )
        .await
        .expect_err("same source key with changed content must conflict");
    assert!(matches!(
        conflict,
        StoreError::PendingTurnInputSourceKeyConflict {
            session_id,
            source_key,
            existing_input_id,
        } if session_id == "root"
            && source_key == "source:first"
            && existing_input_id == first.input_id
    ));
    let second = store
        .enqueue_pending_turn_input(pending_next_turn_input_draft(
            &SessionId::from("root"),
            "second",
        ))
        .await
        .expect("enqueue second pending input");
    store
        .enqueue_pending_turn_input(pending_next_turn_input_draft(
            &SessionId::from("other"),
            "other session",
        ))
        .await
        .expect("enqueue other session pending input");

    assert_eq!(
        first.input_id, replay.input_id,
        "replaying a source key must return the original pending input"
    );
    assert_eq!(
        pending_input_text(&replay),
        Some("first"),
        "source-key replay must return the original stored payload, not the replay attempt"
    );
    let listed = store
        .list_pending_turn_inputs(&SessionId::from("root"))
        .await
        .expect("list pending turn inputs");
    assert_eq!(
        listed
            .iter()
            .map(|input| input.input_id.as_str())
            .collect::<Vec<_>>(),
        vec![first.input_id.as_str(), second.input_id.as_str()]
    );
    assert!(listed[0].enqueue_seq < listed[1].enqueue_seq);
    assert!(listed.iter().all(|input| input.session_id == "root"));

    let cancelled = store
        .cancel_pending_turn_input(&SessionId::from("root"), &second.input_id)
        .await
        .expect("cancel pending turn input");
    expect_cancelled_pending_input(cancelled, &second.input_id);
    assert!(matches!(
        store
            .cancel_pending_turn_input(&SessionId::from("root"), &second.input_id)
            .await
            .expect("cancel pending turn input replay"),
        crate::PendingTurnInputCancelOutcome::AlreadyCancelled(input)
            if input.input_id == second.input_id
    ));
    assert_eq!(
        store
            .list_pending_turn_inputs(&SessionId::from("root"))
            .await
            .expect("list after cancel")
            .iter()
            .map(|input| input.input_id.as_str())
            .collect::<Vec<_>>(),
        vec![first.input_id.as_str()]
    );

    let cancelled_first = store
        .cancel_pending_turn_input(&SessionId::from("root"), &first.input_id)
        .await
        .expect("cancel source-keyed pending turn input");
    expect_cancelled_pending_input(cancelled_first, &first.input_id);
    let terminal_replay = store
        .enqueue_pending_turn_input(
            pending_next_turn_input_draft(&SessionId::from("root"), "first")
                .with_source_key("source:first"),
        )
        .await
        .expect("exact replay after cancellation");
    assert_eq!(terminal_replay.input_id, first.input_id);
    assert_eq!(terminal_replay.state, crate::TurnInputState::Cancelled);
    assert!(
        store
            .list_pending_turn_inputs(&SessionId::from("root"))
            .await
            .expect("list after terminal replay")
            .is_empty()
    );
    let vacuum = store
        .vacuum()
        .await
        .expect("vacuum pending input tombstones");
    assert_eq!(vacuum.removed_node_count, 0);
    assert_eq!(vacuum.removed_pending_turn_input_tombstone_count, 2);
    assert!(matches!(
        store
            .cancel_pending_turn_input(&SessionId::from("root"), &second.input_id)
            .await
            .expect("cancel pruned tombstone"),
        crate::PendingTurnInputCancelOutcome::NotFound
    ));
    assert_eq!(
        store
            .list_pending_turn_inputs(&SessionId::from("other"))
            .await
            .expect("list other session after tombstone vacuum")
            .len(),
        1,
        "vacuum must prune terminal evidence without removing live pending input"
    );
}

pub(super) async fn pending_turn_input_bulk_and_suffix_cancellation(
    store: Arc<dyn RuntimePersistence>,
) {
    let first = store
        .enqueue_pending_turn_input(
            pending_next_turn_input_draft(&SessionId::from("root"), "bulk first")
                .with_source_key("bulk:first"),
        )
        .await
        .expect("enqueue first bulk input");
    let second = store
        .enqueue_pending_turn_input(
            pending_next_turn_input_draft(&SessionId::from("root"), "bulk second")
                .with_source_key("bulk:second"),
        )
        .await
        .expect("enqueue second bulk input");
    let third = store
        .enqueue_pending_turn_input(pending_next_turn_input_draft(
            &SessionId::from("root"),
            "bulk third",
        ))
        .await
        .expect("enqueue third bulk input");
    let bulk = store
        .cancel_pending_turn_inputs(
            &SessionId::from("root"),
            &[
                crate::PendingTurnInputCancelTarget::source_key("bulk:first"),
                crate::PendingTurnInputCancelTarget::input_id(&third.input_id),
                crate::PendingTurnInputCancelTarget::source_key("bulk:missing"),
                crate::PendingTurnInputCancelTarget::source_key("bulk:first"),
            ],
        )
        .await
        .expect("bulk cancel pending turn inputs");
    assert_eq!(bulk.len(), 4);
    expect_cancelled_pending_input(bulk[0].outcome.clone(), &first.input_id);
    expect_cancelled_pending_input(bulk[1].outcome.clone(), &third.input_id);
    assert!(matches!(
        bulk[2].outcome,
        crate::PendingTurnInputCancelOutcome::NotFound
    ));
    assert!(matches!(
        &bulk[3].outcome,
        crate::PendingTurnInputCancelOutcome::AlreadyCancelled(input)
            if input.input_id == first.input_id
    ));
    assert_eq!(
        store
            .list_pending_turn_inputs(&SessionId::from("root"))
            .await
            .expect("list after bulk cancellation")
            .iter()
            .map(|input| input.input_id.as_str())
            .collect::<Vec<_>>(),
        vec![second.input_id.as_str()]
    );

    let suffix_anchor = store
        .enqueue_pending_turn_input(
            pending_next_turn_input_draft(&SessionId::from("root"), "suffix anchor")
                .with_source_key("suffix:anchor"),
        )
        .await
        .expect("enqueue suffix anchor");
    let active_claimed = store
        .enqueue_pending_turn_input(
            pending_active_turn_input_draft(
                &SessionId::from("root"),
                &TurnId::from("suffix-active-turn"),
                crate::TurnInputCheckpointBoundary::AfterWork,
                "suffix accepted active",
            )
            .with_source_key("suffix:claimed"),
        )
        .await
        .expect("enqueue suffix claimed input");
    let suffix_later = store
        .enqueue_pending_turn_input(
            pending_next_turn_input_draft(&SessionId::from("root"), "suffix later")
                .with_source_key("suffix:later"),
        )
        .await
        .expect("enqueue suffix later");
    let lease = claim_session_execution_lease_for_test(
        &store,
        &SessionId::from("root"),
        "suffix-cancel-owner",
    )
    .await;
    let active_claim = store
        .claim_active_turn_inputs(
            &SessionId::from("root"),
            &lease.fence(),
            &lease_owner("suffix-cancel-owner"),
            &crate::TurnId::from("suffix-active-turn"),
            crate::CheckpointKind::AfterWork,
            10,
        )
        .await
        .expect("claim suffix active input")
        .expect("suffix active input claim");

    let suffix = store
        .cancel_pending_turn_input_suffix(
            &SessionId::from("root"),
            &crate::PendingTurnInputCancelTarget::source_key("suffix:anchor"),
        )
        .await
        .expect("suffix cancel by source key");
    let crate::PendingTurnInputSuffixCancelOutcome::Outcomes { outcomes, .. } = suffix else {
        panic!("expected suffix outcomes, got {suffix:?}");
    };
    assert_eq!(outcomes.len(), 3);
    expect_cancelled_pending_input(outcomes[0].clone(), &suffix_anchor.input_id);
    match &outcomes[1] {
        crate::PendingTurnInputCancelOutcome::AlreadyClaimed { input, claim } => {
            assert_eq!(input.input_id, active_claimed.input_id);
            assert_eq!(
                claim.as_ref().and_then(|claim| claim.claim_id.as_deref()),
                Some(active_claim.claim_id.as_str())
            );
        }
        other => panic!("expected already-claimed suffix outcome, got {other:?}"),
    }
    expect_cancelled_pending_input(outcomes[2].clone(), &suffix_later.input_id);

    let suffix_by_id_anchor = store
        .enqueue_pending_turn_input(pending_next_turn_input_draft(
            &SessionId::from("root"),
            "suffix by id anchor",
        ))
        .await
        .expect("enqueue suffix by id anchor");
    let suffix_by_id_later = store
        .enqueue_pending_turn_input(
            pending_next_turn_input_draft(&SessionId::from("root"), "suffix by id later")
                .with_source_key("suffix:id-later"),
        )
        .await
        .expect("enqueue suffix by id later");
    let suffix_by_id = store
        .cancel_pending_turn_input_suffix(
            &SessionId::from("root"),
            &crate::PendingTurnInputCancelTarget::input_id(&suffix_by_id_anchor.input_id),
        )
        .await
        .expect("suffix cancel by input id");
    let crate::PendingTurnInputSuffixCancelOutcome::Outcomes { outcomes, .. } = suffix_by_id else {
        panic!("expected input-id suffix outcomes, got {suffix_by_id:?}");
    };
    assert_eq!(outcomes.len(), 2);
    expect_cancelled_pending_input(outcomes[0].clone(), &suffix_by_id_anchor.input_id);
    expect_cancelled_pending_input(outcomes[1].clone(), &suffix_by_id_later.input_id);

    assert!(matches!(
        store
            .cancel_pending_turn_input_suffix(
                &SessionId::from("root"),
                &crate::PendingTurnInputCancelTarget::source_key("suffix:missing"),
            )
            .await
            .expect("missing suffix anchor"),
        crate::PendingTurnInputSuffixCancelOutcome::AnchorNotFound { .. }
    ));
    assert_eq!(
        store
            .list_pending_turn_inputs(&SessionId::from("root"))
            .await
            .expect("list after suffix cancellation")
            .iter()
            .map(|input| input.input_id.as_str())
            .collect::<Vec<_>>(),
        vec![second.input_id.as_str()]
    );
}

pub(super) async fn pending_turn_input_claims_reclaim_complete_and_fence(
    store: Arc<dyn RuntimePersistence>,
) {
    let first = store
        .enqueue_pending_turn_input(
            crate::PendingTurnInputDraft::new(
                "root",
                crate::TurnInputIngress::NextTurn,
                crate::TurnInput::text("first next").with_attachment(inline_png(vec![1, 2, 3])),
            )
            .with_source_key("next:first"),
        )
        .await
        .expect("enqueue first next input");
    let second = store
        .enqueue_pending_turn_input(pending_next_turn_input_draft(
            &SessionId::from("root"),
            "second next",
        ))
        .await
        .expect("enqueue second next input");
    let lease = claim_session_execution_lease_for_test(
        &store,
        &SessionId::from("root"),
        "turn-input-owner",
    )
    .await;
    let claim = store
        .claim_next_turn_inputs(
            &SessionId::from("root"),
            &lease.fence(),
            &lease_owner("turn-input-owner"),
            10,
        )
        .await
        .expect("claim next inputs")
        .expect("next input claim");
    assert_eq!(
        claim
            .inputs
            .iter()
            .map(|input| input.input_id.as_str())
            .collect::<Vec<_>>(),
        vec![first.input_id.as_str(), second.input_id.as_str()]
    );
    assert!(matches!(
        claim
            .materialize_turn_input()
            .items
            .iter()
            .find(|item| matches!(item, crate::InputItem::Attachment { .. })),
        Some(crate::InputItem::Attachment {
            source: crate::AttachmentSource::Inline { bytes, .. }
        }) if bytes == &[1, 2, 3]
    ));
    match store
        .cancel_pending_turn_input(&SessionId::from("root"), &first.input_id)
        .await
        .expect("cancel claimed input")
    {
        crate::PendingTurnInputCancelOutcome::AlreadyClaimed {
            input,
            claim: diagnostics,
        } => {
            assert_eq!(input.input_id, first.input_id);
            assert_eq!(
                diagnostics
                    .as_ref()
                    .and_then(|diagnostics| diagnostics.claim_id.as_deref()),
                Some(claim.claim_id.as_str())
            );
        }
        other => panic!("live claimed pending input must not be cancellable, got {other:?}"),
    }
    assert!(
        store
            .list_pending_turn_inputs(&SessionId::from("root"))
            .await
            .expect("list claimed inputs")
            .is_empty(),
        "live claimed pending inputs must be hidden from queue previews"
    );

    store
        .abandon_turn_input_claim(&claim)
        .await
        .expect("abandon pending input claim");
    assert_eq!(
        store
            .list_pending_turn_inputs(&SessionId::from("root"))
            .await
            .expect("list after abandon")
            .len(),
        2
    );
    let reclaimed = store
        .claim_next_turn_inputs(
            &SessionId::from("root"),
            &lease.fence(),
            &lease_owner("turn-input-owner"),
            10,
        )
        .await
        .expect("reclaim next inputs")
        .expect("reclaimed next claim");
    assert!(
        reclaimed.fencing_token > claim.fencing_token,
        "reclaiming abandoned pending inputs must advance the fencing token"
    );

    let state = RuntimeSessionState {
        session_id: SessionId::from("root"),
        ..RuntimeSessionState::new(crate::SessionPolicy::new(crate::TurnBudget::Unbounded))
    };
    let err = store
        .commit_runtime_state(
            RuntimeCommit::persisted_state_for_test(&state, &[])
                .completing_turn_input_claim(claim.completion()),
        )
        .await
        .expect_err("stale turn-input completion must fail");
    assert!(matches!(err, StoreError::TurnInputClaimSuperseded { .. }));
    assert!(
        store
            .list_pending_turn_inputs(&SessionId::from("root"))
            .await
            .expect("list reclaimed live inputs")
            .is_empty(),
        "stale completion must not abandon the live reclaimed claim"
    );

    store
        .commit_runtime_state(
            RuntimeCommit::persisted_state_for_test(&state, &[])
                .releasing_session_execution_lease(lease.completion())
                .completing_turn_input_claim(reclaimed.completion()),
        )
        .await
        .expect("valid pending input completion commits");
    assert!(
        store
            .list_pending_turn_inputs(&SessionId::from("root"))
            .await
            .expect("list after valid completion")
            .is_empty()
    );
    assert!(matches!(
        store
            .cancel_pending_turn_input(&SessionId::from("root"), &first.input_id)
            .await
            .expect("cancel completed input"),
        crate::PendingTurnInputCancelOutcome::AlreadyCompleted(input)
            if input.input_id == first.input_id
    ));
    let completed_replay = store
        .enqueue_pending_turn_input(
            crate::PendingTurnInputDraft::new(
                "root",
                crate::TurnInputIngress::NextTurn,
                crate::TurnInput::text("first next").with_attachment(inline_png(vec![1, 2, 3])),
            )
            .with_source_key("next:first"),
        )
        .await
        .expect("exact replay after completion");
    assert_eq!(completed_replay.input_id, first.input_id);
    assert_eq!(completed_replay.state, crate::TurnInputState::Completed);
    let post_completion_lease = claim_session_execution_lease_for_test(
        &store,
        &SessionId::from("root"),
        "post-completion-owner",
    )
    .await;
    assert!(
        store
            .claim_next_turn_inputs(
                &SessionId::from("root"),
                &post_completion_lease.fence(),
                &lease_owner("post-completion-owner"),
                10,
            )
            .await
            .expect("claim after completing inputs")
            .is_none(),
        "completed pending input tombstones must not be claimable"
    );
}

pub async fn turn_input_claims_supersede_across_session_lease_generations(
    store: Arc<dyn RuntimePersistence>,
    lease_timing: RuntimePersistenceLeaseTiming,
) {
    turn_input_claims_supersede_across_session_lease_generations_with_timing(store, &lease_timing)
        .await;
}

pub(super) async fn turn_input_claims_supersede_across_session_lease_generations_with_timing(
    store: Arc<dyn RuntimePersistence>,
    lease_timing: &RuntimePersistenceLeaseTiming,
) {
    // The DeferredNextTurn idle-retry shape: a failed turn releases its lease
    // and the next idle acquisition re-claims the same next-turn input under a
    // fresh generation, while the stale claim's completion is rejected. This was
    // the latent unrenewed-claim bug (ADR 0029).
    let input = store
        .enqueue_pending_turn_input(pending_next_turn_input_draft(
            &SessionId::from("root"),
            "generation next input",
        ))
        .await
        .expect("enqueue next-turn input");

    // (a) Same generation: a live next-turn claim is not re-claimable.
    let lease_a =
        claim_session_execution_lease_for_test(&store, &SessionId::from("root"), "tin-owner-a")
            .await;
    let claim_a = store
        .claim_next_turn_inputs(
            &SessionId::from("root"),
            &lease_a.fence(),
            &lease_owner("tin-owner-a"),
            10,
        )
        .await
        .expect("first next-turn claim")
        .expect("first next-turn claim exists");
    assert_eq!(claim_a.inputs[0].input_id, input.input_id);
    assert_eq!(claim_a.session_lease_generation, lease_a.fencing_token);
    assert!(
        store
            .claim_next_turn_inputs(
                &SessionId::from("root"),
                &lease_a.fence(),
                &lease_owner("tin-owner-a"),
                10
            )
            .await
            .expect("same-generation re-claim")
            .is_none(),
        "a live next-turn claim must not be re-claimable under its own generation"
    );

    // (b) Idle retry after lease release + re-acquire: the same next-turn input
    // is re-claimable by the new generation and the stale completion is
    // superseded.
    release_session_execution_lease_for_test(&store, &lease_a).await;
    let lease_b =
        claim_session_execution_lease_for_test(&store, &SessionId::from("root"), "tin-owner-b")
            .await;
    let claim_b = store
        .claim_next_turn_inputs(
            &SessionId::from("root"),
            &lease_b.fence(),
            &lease_owner("tin-owner-b"),
            10,
        )
        .await
        .expect("idle-retry next-turn claim")
        .expect("idle-retry next-turn claim exists");
    assert_eq!(claim_b.inputs[0].input_id, input.input_id);
    assert!(claim_b.fencing_token > claim_a.fencing_token);

    let stale_state = RuntimeSessionState {
        session_id: SessionId::from("root"),
        ..RuntimeSessionState::new(crate::SessionPolicy::new(crate::TurnBudget::Unbounded))
    };
    let stale_err = store
        .commit_runtime_state(
            RuntimeCommit::persisted_state_for_test(&stale_state, &[])
                .completing_turn_input_claim(claim_a.completion()),
        )
        .await
        .expect_err("superseded next-turn completion must fail");
    assert!(matches!(
        stale_err,
        StoreError::TurnInputClaimSuperseded { .. }
    ));
    release_session_execution_lease_for_test(&store, &lease_b).await;

    // (c) TTL takeover mints a new generation without a release.
    let dead_owner = lease_owner("tin-stale");
    let (_dead_lease, claim_dead) = claim_turn_input_under_short_lease(
        &store,
        &SessionId::from("root"),
        &dead_owner,
        lease_timing,
    )
    .await;
    let taker = lease_owner("tin-taker");
    let taker_lease = claim_session_execution_lease_after_expiry(
        &store,
        &SessionId::from("root"),
        &taker,
        lease_timing,
        "stale turn-input owner TTL",
    )
    .await;
    let claim_taker = store
        .claim_next_turn_inputs(&SessionId::from("root"), &taker_lease.fence(), &taker, 10)
        .await
        .expect("post-takeover next-turn claim")
        .expect("post-takeover next-turn claim exists");
    assert_eq!(claim_taker.inputs[0].input_id, input.input_id);
    let takeover_err = store
        .commit_runtime_state(
            RuntimeCommit::persisted_state_for_test(&stale_state, &[])
                .completing_turn_input_claim(claim_dead.completion()),
        )
        .await
        .expect_err("pre-takeover next-turn completion must fail");
    assert!(matches!(
        takeover_err,
        StoreError::TurnInputClaimSuperseded { .. }
    ));
}

/// A checkpoint executor can durably move an active input to `accepted` and
/// crash before its effect outcome journals the claim. The successor must be
/// able to reacquire that same turn/input pair under its newer generation.
pub async fn active_turn_input_claim_reacquires_after_unrecorded_checkpoint(
    store: Arc<dyn RuntimePersistence>,
) {
    const SESSION_ID: &str = "fig905-active-reacquire";
    const TURN_ID: &str = "fig905-active-reacquire:turn";
    let input = store
        .enqueue_pending_turn_input(pending_active_turn_input_draft(
            &SessionId::from(SESSION_ID),
            &crate::TurnId::from(TURN_ID),
            crate::TurnInputCheckpointBoundary::AfterWork,
            "accepted before checkpoint outcome",
        ))
        .await
        .expect("enqueue active input");

    let predecessor = claim_session_execution_lease_for_test(
        &store,
        &SessionId::from(SESSION_ID),
        "fig905-active-predecessor",
    )
    .await;
    let predecessor_claim = store
        .claim_active_turn_inputs(
            &SessionId::from(SESSION_ID),
            &predecessor.fence(),
            &lease_owner("fig905-active-predecessor"),
            &crate::TurnId::from(TURN_ID),
            crate::CheckpointKind::AfterWork,
            10,
        )
        .await
        .expect("claim active input before simulated crash")
        .expect("active input claim exists");
    assert_eq!(
        predecessor_claim.inputs[0].state,
        crate::TurnInputState::Accepted
    );
    release_session_execution_lease_for_test(&store, &predecessor).await;

    let successor = claim_session_execution_lease_for_test(
        &store,
        &SessionId::from(SESSION_ID),
        "fig905-active-successor",
    )
    .await;
    let (successor_claim, queued_claim) = store
        .claim_checkpoint_work(
            &SessionId::from(SESSION_ID),
            &successor.fence(),
            &lease_owner("fig905-active-successor"),
            &crate::TurnId::from(TURN_ID),
            crate::CheckpointKind::AfterWork,
            10,
            crate::testing::queued_work_claim_policy(10),
        )
        .await
        .expect("reacquire accepted input after unrecorded checkpoint");
    let successor_claim = successor_claim.expect("successor reacquires accepted input");
    assert!(
        queued_claim.is_none(),
        "accepted-only checkpoint fixture must not rely on queued work to open the claim path"
    );
    assert_eq!(successor_claim.inputs[0].input_id, input.input_id);
    assert!(successor_claim.session_lease_generation > predecessor_claim.session_lease_generation);
    assert!(successor_claim.fencing_token > predecessor_claim.fencing_token);

    let stale_state = RuntimeSessionState {
        session_id: SessionId::from(SESSION_ID.to_string()),
        ..RuntimeSessionState::new(crate::SessionPolicy::new(crate::TurnBudget::Unbounded))
    };
    let stale_error = store
        .commit_runtime_state(
            RuntimeCommit::persisted_state_for_test(&stale_state, &[])
                .completing_turn_input_claim(predecessor_claim.completion()),
        )
        .await
        .expect_err("reacquisition supersedes the unjournaled predecessor claim");
    assert!(matches!(
        stale_error,
        StoreError::TurnInputClaimSuperseded { .. }
    ));

    store
        .commit_runtime_state(
            RuntimeCommit::persisted_state_for_test(&stale_state, &[])
                .releasing_session_execution_lease(successor.completion())
                .completing_turn_input_claim(successor_claim.completion()),
        )
        .await
        .expect("successor settles reacquired active input");
}

pub(super) async fn pending_turn_input_cancel_covers_active_and_deferred_states(
    store: Arc<dyn RuntimePersistence>,
) {
    let turn_id = "cancel-active-turn";
    let active_keep = store
        .enqueue_pending_turn_input(pending_active_turn_input_draft(
            &SessionId::from("root"),
            &TurnId::from(turn_id),
            crate::TurnInputCheckpointBoundary::AfterWork,
            "active that defers",
        ))
        .await
        .expect("enqueue active input to defer");
    let active_cancel = store
        .enqueue_pending_turn_input(pending_active_turn_input_draft(
            &SessionId::from("root"),
            &TurnId::from(turn_id),
            crate::TurnInputCheckpointBoundary::AfterWork,
            "active cancelled before interrupt",
        ))
        .await
        .expect("enqueue active input to cancel");
    let next_cancel = store
        .enqueue_pending_turn_input(pending_next_turn_input_draft(
            &SessionId::from("root"),
            "next cancelled before claim",
        ))
        .await
        .expect("enqueue next input to cancel");

    let cancelled_active = store
        .cancel_pending_turn_input(&SessionId::from("root"), &active_cancel.input_id)
        .await
        .expect("cancel active input");
    let cancelled_active =
        expect_cancelled_pending_input(cancelled_active, &active_cancel.input_id);
    assert!(matches!(
        cancelled_active.ingress,
        crate::TurnInputIngress::ActiveTurn { .. }
    ));
    let cancelled_next = store
        .cancel_pending_turn_input(&SessionId::from("root"), &next_cancel.input_id)
        .await
        .expect("cancel next input");
    expect_cancelled_pending_input(cancelled_next, &next_cancel.input_id);

    let lease = claim_session_execution_lease_for_test(
        &store,
        &SessionId::from("root"),
        "cancel-input-owner",
    )
    .await;
    let state = RuntimeSessionState {
        session_id: SessionId::from("root"),
        ..RuntimeSessionState::new(crate::SessionPolicy::new(crate::TurnBudget::Unbounded))
    };
    store
        .commit_runtime_state(
            lash_core::testing::store_fixtures::authorize_completion_deferral_for_test(
                store.as_ref(),
                &lease.fence(),
                RuntimeCommit::persisted_state_for_test(&state, &[])
                    .deferring_interrupted_turn_inputs(turn_id, None),
            )
            .await
            .expect("authorize interrupt deferral"),
        )
        .await
        .expect("interrupt commit defers uncancelled active input");

    let pending_after_interrupt = store
        .list_pending_turn_inputs(&SessionId::from("root"))
        .await
        .expect("list after interrupt");
    assert_eq!(
        pending_after_interrupt
            .iter()
            .map(|input| input.input_id.as_str())
            .collect::<Vec<_>>(),
        vec![active_keep.input_id.as_str()],
        "cancelled active and next-turn inputs must not be resurrected by interrupt deferral"
    );
    assert!(matches!(
        pending_after_interrupt[0].ingress,
        crate::TurnInputIngress::NextTurn
    ));
    assert_eq!(
        pending_after_interrupt[0].state,
        crate::TurnInputState::DeferredNextTurn
    );

    let cancelled_deferred = store
        .cancel_pending_turn_input(&SessionId::from("root"), &active_keep.input_id)
        .await
        .expect("cancel deferred input");
    expect_cancelled_pending_input(cancelled_deferred, &active_keep.input_id);
    assert!(
        store
            .claim_next_turn_inputs(
                &SessionId::from("root"),
                &lease.fence(),
                &lease_owner("cancel-input-owner"),
                10,
            )
            .await
            .expect("claim after cancelling deferred input")
            .is_none(),
        "cancelled deferred input must not be claimable"
    );
}

pub(super) async fn pending_active_turn_inputs_defer_unaccepted_once_on_interrupt(
    store: Arc<dyn RuntimePersistence>,
) {
    let turn_id = "active-turn-1";
    let accepted = store
        .enqueue_pending_turn_input(
            crate::PendingTurnInputDraft::new(
                "root",
                crate::TurnInputIngress::active_turn(
                    turn_id,
                    crate::TurnInputCheckpointBoundary::AfterWork,
                ),
                crate::TurnInput::text("accepted active")
                    .with_attachment(inline_png(vec![9, 8, 7])),
            )
            .with_source_key("active:accepted"),
        )
        .await
        .expect("enqueue accepted active input");
    let unaccepted = store
        .enqueue_pending_turn_input(pending_active_turn_input_draft(
            &SessionId::from("root"),
            &TurnId::from(turn_id),
            crate::TurnInputCheckpointBoundary::AfterWork,
            "unaccepted active",
        ))
        .await
        .expect("enqueue unaccepted active input");
    let before_completion = store
        .enqueue_pending_turn_input(pending_active_turn_input_draft(
            &SessionId::from("root"),
            &TurnId::from(turn_id),
            crate::TurnInputCheckpointBoundary::BeforeCompletion,
            "before-completion active",
        ))
        .await
        .expect("enqueue before-completion active input");
    let other_active = store
        .enqueue_pending_turn_input(pending_active_turn_input_draft(
            &SessionId::from("root"),
            &TurnId::from("other-turn"),
            crate::TurnInputCheckpointBoundary::AfterWork,
            "other active",
        ))
        .await
        .expect("enqueue other active input");

    let lease = claim_session_execution_lease_for_test(
        &store,
        &SessionId::from("root"),
        "active-input-owner",
    )
    .await;
    let claim_turn_id = crate::TurnId::from(turn_id);
    let claim = store
        .claim_active_turn_inputs(
            &SessionId::from("root"),
            &lease.fence(),
            &lease_owner("active-input-owner"),
            &claim_turn_id,
            crate::CheckpointKind::AfterWork,
            1,
        )
        .await
        .expect("claim active inputs")
        .expect("active input claim");
    assert_eq!(
        claim
            .inputs
            .iter()
            .map(|input| input.input_id.as_str())
            .collect::<Vec<_>>(),
        vec![accepted.input_id.as_str()],
        "AfterWork claims must include matching active inputs admitted at that boundary in order"
    );
    assert!(matches!(
        claim.materialize_turn_input().items.last(),
        Some(crate::InputItem::Attachment {
            source: crate::AttachmentSource::Inline { bytes, .. }
        }) if bytes == &[9, 8, 7]
    ));

    let state = RuntimeSessionState {
        session_id: SessionId::from("root"),
        ..RuntimeSessionState::new(crate::SessionPolicy::new(crate::TurnBudget::Unbounded))
    };
    let interrupt_result = store
        .commit_runtime_state(
            lash_core::testing::store_fixtures::authorize_completion_deferral_for_test(
                store.as_ref(),
                &lease.fence(),
                RuntimeCommit::persisted_state_for_test(&state, &[])
                    .completing_turn_input_claim(claim.completion())
                    .deferring_interrupted_turn_inputs(turn_id, None),
            )
            .await
            .expect("authorize active input deferral"),
        )
        .await
        .expect("interrupt commit completes accepted inputs and defers unaccepted inputs");
    let mut state = state;
    state.head_revision = interrupt_result.head_revision;
    let pending_after_interrupt = store
        .list_pending_turn_inputs(&SessionId::from("root"))
        .await
        .expect("list after interrupt deferral");
    assert_eq!(
        pending_after_interrupt
            .iter()
            .map(|input| input.input_id.as_str())
            .collect::<Vec<_>>(),
        vec![
            unaccepted.input_id.as_str(),
            before_completion.input_id.as_str(),
            other_active.input_id.as_str(),
        ],
        "interrupt must complete accepted input, defer matching unaccepted inputs, and retain other-turn active input"
    );
    let deferred_after_interrupt = pending_after_interrupt
        .iter()
        .filter(|input| input.ingress.active_turn_id().is_none())
        .collect::<Vec<_>>();
    assert_eq!(
        deferred_after_interrupt
            .iter()
            .map(|input| input.input_id.as_str())
            .collect::<Vec<_>>(),
        vec![
            unaccepted.input_id.as_str(),
            before_completion.input_id.as_str()
        ],
        "accepted active inputs must be completed and only unaccepted matching active inputs become next-turn work"
    );
    assert!(deferred_after_interrupt.iter().all(|input| {
        matches!(input.ingress, crate::TurnInputIngress::NextTurn)
            && input.state == crate::TurnInputState::DeferredNextTurn
    }));
    assert!(
        pending_after_interrupt
            .iter()
            .any(|input| input.ingress.active_turn_id() == Some(&TurnId::from("other-turn"))),
        "inputs for other active turns must not be deferred by this interrupt"
    );

    let next_claim = store
        .claim_next_turn_inputs(
            &SessionId::from("root"),
            &lease.fence(),
            &lease_owner("active-input-owner"),
            10,
        )
        .await
        .expect("claim deferred next inputs")
        .expect("deferred next input claim");
    assert_eq!(
        next_claim
            .inputs
            .iter()
            .map(|input| input.input_id.as_str())
            .collect::<Vec<_>>(),
        vec![
            unaccepted.input_id.as_str(),
            before_completion.input_id.as_str()
        ]
    );
    store
        .commit_runtime_state(
            RuntimeCommit::persisted_state_for_test(&state, &[])
                .releasing_session_execution_lease(lease.completion())
                .completing_turn_input_claim(next_claim.completion()),
        )
        .await
        .expect("complete deferred next input");
    assert!(
        store
            .list_pending_turn_inputs(&SessionId::from("root"))
            .await
            .expect("list after completing deferred input")
            .iter()
            .all(|input| input.ingress.active_turn_id() == Some(&TurnId::from("other-turn"))),
        "inputs for other active turns must not be deferred by this interrupt"
    );
}

/// A turn that cannot commit leaves no input pinned to it (FIG-1573).
///
/// The commit-time re-defer
/// ([`RuntimeCommit::deferring_interrupted_turn_inputs`]) is the repair a turn
/// carries for the active-turn-scoped inputs it did not deliver. A turn that
/// never reaches its commit - killed, aborted, or fenced - owes the same repair,
/// and only the store can perform it once the turn is gone. Both scopes are
/// laws: naming the dead turn repairs exactly its rows, and the lane-generation
/// scope repairs every row no live claim protects, except those pinned to a turn
/// the caller names as still resumable. Nothing else may move, so a row pinned
/// to a turn that can still deliver stays put, and a caller whose lane has been
/// superseded repairs nothing at all.
pub async fn a_turn_that_cannot_commit_leaves_no_input_pinned_to_it(
    store: Arc<dyn RuntimePersistence>,
) {
    let dead_turn_id = "fig1573-dead-turn";
    let other_turn_id = "fig1573-other-turn";
    let lease =
        claim_session_execution_lease_for_test(&store, &SessionId::from("root"), "fig1573-owner")
            .await;
    let orphaned = store
        .enqueue_pending_turn_input(pending_active_turn_input_draft(
            &SessionId::from("root"),
            &TurnId::from(dead_turn_id),
            crate::TurnInputCheckpointBoundary::AfterWork,
            "pinned to a turn that cannot commit",
        ))
        .await
        .expect("enqueue the orphaned input");
    let other = store
        .enqueue_pending_turn_input(pending_active_turn_input_draft(
            &SessionId::from("root"),
            &TurnId::from(other_turn_id),
            crate::TurnInputCheckpointBoundary::AfterWork,
            "pinned to a turn that can still deliver",
        ))
        .await
        .expect("enqueue the untouched input");

    let repaired = store
        .repair_orphaned_active_turn_inputs(
            &SessionId::from("root"),
            &lease.fence(),
            &TurnId::from(dead_turn_id),
            &crate::TurnCancelIntentSnapshot::Absent,
            None,
        )
        .await
        .expect("re-defer inputs pinned to the dead turn")
        .into_applied()
        .expect("unchanged absent intent");
    assert_eq!(
        repaired.len(),
        1,
        "naming a dead turn must repair exactly the rows pinned to it"
    );
    let pending = store
        .list_pending_turn_inputs(&SessionId::from("root"))
        .await
        .expect("list pending inputs after the turn-scoped repair");
    let repaired_row = pending
        .iter()
        .find(|input| input.input_id == orphaned.input_id)
        .expect("the repaired input is still queued");
    assert_eq!(repaired_row.state, crate::TurnInputState::DeferredNextTurn);
    assert_eq!(repaired_row.ingress, crate::TurnInputIngress::NextTurn);
    let untouched_row = pending
        .iter()
        .find(|input| input.input_id == other.input_id)
        .expect("the other turn's input is still queued");
    assert_eq!(untouched_row.state, crate::TurnInputState::PendingActive);
    assert_eq!(
        untouched_row.ingress.active_turn_id(),
        Some(&crate::TurnId::from(other_turn_id)),
        "a row pinned to a turn that can still deliver must never be swept"
    );
    assert_eq!(
        store
            .repair_orphaned_active_turn_inputs(
                &SessionId::from("root"),
                &lease.fence(),
                &TurnId::from(dead_turn_id),
                &crate::TurnCancelIntentSnapshot::Absent,
                None,
            )
            .await
            .expect("repeat the turn-scoped repair")
            .into_applied()
            .expect("unchanged absent intent")
            .len(),
        0,
        "the repair is idempotent: a repaired row is no longer pinned to any turn"
    );

    // The repaired row is next-turn work again, which is the whole point.
    let claim = store
        .claim_next_turn_inputs(
            &SessionId::from("root"),
            &lease.fence(),
            &lease_owner("fig1573-owner"),
            10,
        )
        .await
        .expect("claim the repaired input as next-turn work")
        .expect("the repaired input is claimable");
    assert_eq!(
        claim
            .inputs
            .iter()
            .map(|input| input.input_id.as_str())
            .collect::<Vec<_>>(),
        vec![orphaned.input_id.as_str()]
    );
    // A turn the sweeping caller can still resume owns its pinned rows even
    // though no live claim protects them: cold recovery replays that turn under
    // the same turn id at a new generation, and its agent-frame follow-ons are
    // the same execution continuing.
    let follow_on = store
        .enqueue_pending_turn_input(pending_active_turn_input_draft(
            &SessionId::from("root"),
            &crate::TurnId::from(format!("{other_turn_id}:agent-frame:2")),
            crate::TurnInputCheckpointBoundary::AfterWork,
            "pinned to a follow-on frame of the resumable turn",
        ))
        .await
        .expect("enqueue the follow-on frame's input");
    assert_eq!(
        store
            .orphaned_active_turn_ids(
                &SessionId::from("root"),
                &lease.fence(),
                crate::OrphanedTurnInputScope::LaneGeneration {
                    resumable_turn_id: Some(&TurnId::from(other_turn_id)),
                },
            )
            .await
            .expect("discover while naming a turn the caller can still resume")
            .len(),
        0,
        "a row pinned to a resumable turn, or to one of its agent frames, must survive the sweep"
    );
    let after_exclusion = store
        .list_pending_turn_inputs(&SessionId::from("root"))
        .await
        .expect("list pending inputs after the excluded sweep");
    for input_id in [other.input_id.as_str(), follow_on.input_id.as_str()] {
        let row = after_exclusion
            .iter()
            .find(|input| input.input_id == input_id)
            .expect("the excluded row is still queued");
        assert_eq!(row.state, crate::TurnInputState::PendingActive);
        assert!(row.ingress.active_turn_id().is_some());
    }
    // A row this caller's own live generation holds is never an orphan, even
    // while the lane-generation scope is sweeping around it.
    let repairable_turn_ids = store
        .orphaned_active_turn_ids(
            &SessionId::from("root"),
            &lease.fence(),
            crate::OrphanedTurnInputScope::LaneGeneration {
                resumable_turn_id: None,
            },
        )
        .await
        .expect("discover around the caller's own live claim");
    assert_eq!(
        repairable_turn_ids.len(),
        2,
        "discovery identifies the unclaimed pinned turns and leaves the live claim alone"
    );
    for turn_id in repairable_turn_ids {
        store
            .repair_orphaned_active_turn_inputs(
                &SessionId::from("root"),
                &lease.fence(),
                &turn_id,
                &crate::TurnCancelIntentSnapshot::Absent,
                None,
            )
            .await
            .expect("repair one discovered orphan");
    }
    // Abandoning a next-turn claim restores the next-turn state, not the
    // active-turn one: a plural abandon that restored `pending_active` would
    // re-strand the row behind a turn id that no longer exists.
    store
        .abandon_turn_input_claims(std::slice::from_ref(&claim))
        .await
        .expect("abandon the next-turn claim");
    let after_abandon = store
        .list_pending_turn_inputs(&SessionId::from("root"))
        .await
        .expect("list pending inputs after abandoning the claim");
    let restored_row = after_abandon
        .iter()
        .find(|input| input.input_id == orphaned.input_id)
        .expect("the abandoned input is still queued");
    assert_eq!(restored_row.state, crate::TurnInputState::DeferredNextTurn);
    assert_eq!(restored_row.ingress, crate::TurnInputIngress::NextTurn);
    assert!(
        after_abandon
            .iter()
            .all(|input| input.ingress.active_turn_id().is_none()
                && input.state == crate::TurnInputState::DeferredNextTurn),
        "no input may stay pinned to a turn once no live claim protects it"
    );

    // A superseded caller repairs nothing: between its own check and its write
    // the lane may have moved on, and the holder that displaced it owns those
    // rows now.
    let stale_fence = lease.fence();
    let stranded = store
        .enqueue_pending_turn_input(pending_active_turn_input_draft(
            &SessionId::from("root"),
            &TurnId::from("fig1573-superseded-turn"),
            crate::TurnInputCheckpointBoundary::AfterWork,
            "pinned while the lane changes hands",
        ))
        .await
        .expect("enqueue the input a superseded caller must not touch");
    let discovered = store
        .orphaned_active_turn_ids(
            &SessionId::from("root"),
            &stale_fence,
            crate::OrphanedTurnInputScope::LaneGeneration {
                resumable_turn_id: None,
            },
        )
        .await
        .expect("stale owner discovers the orphan before takeover");
    assert_eq!(discovered, vec![TurnId::from("fig1573-superseded-turn")]);
    release_session_execution_lease_for_test(&store, &lease).await;
    let successor = claim_session_execution_lease_for_test(
        &store,
        &SessionId::from("root"),
        "fig1573-successor",
    )
    .await;
    assert!(
        successor.fencing_token > stale_fence.fencing_token,
        "a reclaimed lane must advance the generation"
    );
    let refusal = store
        .repair_orphaned_active_turn_inputs(
            &SessionId::from("root"),
            &stale_fence,
            &TurnId::from("fig1573-superseded-turn"),
            &crate::TurnCancelIntentSnapshot::Absent,
            None,
        )
        .await
        .expect_err("a superseded fence must be refused inside the repair");
    assert!(
        matches!(refusal, StoreError::SessionExecutionLeaseExpired { .. }),
        "a superseded repair must be refused as a lost lease, not silently applied: {refusal:?}"
    );
    let after_refusal = store
        .list_pending_turn_inputs(&SessionId::from("root"))
        .await
        .expect("list pending inputs after the refused repair");
    let untouched = after_refusal
        .iter()
        .find(|input| input.input_id == stranded.input_id)
        .expect("the refused row is still queued");
    assert_eq!(untouched.state, crate::TurnInputState::PendingActive);
    assert_eq!(
        untouched.ingress.active_turn_id(),
        Some(&crate::TurnId::from("fig1573-superseded-turn")),
        "a refused repair must leave the row exactly as it found it"
    );
    // The successor's own fence repairs it.
    assert_eq!(
        store
            .repair_orphaned_active_turn_inputs(
                &SessionId::from("root"),
                &successor.fence(),
                &TurnId::from("fig1573-superseded-turn"),
                &crate::TurnCancelIntentSnapshot::Absent,
                None,
            )
            .await
            .expect("the live holder repairs the row the superseded caller could not")
            .into_applied()
            .expect("unchanged absent intent")
            .len(),
        1,
    );
    release_session_execution_lease_for_test(&store, &successor).await;
}

pub(super) async fn session_metadata_round_trips(store: Arc<dyn RuntimePersistence>) {
    let meta = SessionMeta {
        pending_observer_intents: Vec::new(),
        session_id: SessionId::from("root"),
        relation: SessionRelation::Child {
            parent_session_id: SessionId::from("parent-session"),
            caused_by: None,
        },
    };
    store
        .save_session_meta(meta.clone())
        .await
        .expect("save session meta");
    let loaded = store
        .load_session_meta()
        .await
        .expect("load session meta")
        .expect("session meta present");
    assert_eq!(loaded, meta);
}

/// Blob-backed backends must physically reclaim the checkpoint blob a superseding
/// commit orphaned, while preserving the live one. Generalizes the SQLite-only
/// `gc_unreachable_keeps_rooted_checkpoint_blobs` test to every reclaiming
/// backend via the [`GcReport`](crate::GcReport) counters plus a post-GC load.
pub(super) async fn gc_reclaims_unreachable_checkpoint_blobs_and_preserves_live(
    store: Arc<dyn RuntimePersistence>,
) {
    // First commit writes a live checkpoint blob.
    let mut v1 = RuntimeSessionState {
        session_id: SessionId::from("gc-blobs"),
        ..RuntimeSessionState::new(crate::SessionPolicy::new(crate::TurnBudget::Unbounded))
    };
    v1.set_tool_state_snapshot(Some(
        ToolState::default().with_generation_for_conformance(1),
    ));
    let v1_result = commit_runtime_state_for_test(
        &store,
        RuntimeCommit::persisted_state_for_test(&v1, &[]),
        "gc-blobs-v1",
    )
    .await
    .expect("commit v1");
    // Second commit supersedes it with different content, so the v1 checkpoint
    // blob is now unreachable from every session head.
    let mut v2 = RuntimeSessionState {
        session_id: SessionId::from("gc-blobs"),
        head_revision: v1_result.head_revision,
        ..RuntimeSessionState::new(crate::SessionPolicy::new(crate::TurnBudget::Unbounded))
    };
    v2.set_tool_state_snapshot(Some(
        ToolState::default().with_generation_for_conformance(2),
    ));
    commit_runtime_state_for_test(
        &store,
        RuntimeCommit::persisted_state_for_test(&v2, &[]),
        "gc-blobs-v2",
    )
    .await
    .expect("commit v2");

    let report = store
        .gc_unreachable()
        .await
        .expect("gc reclaims unreachable checkpoint blobs");
    assert!(
        report.root_count >= 1,
        "a live checkpoint must be rooted, got {report:?}"
    );
    assert!(
        report.retained_blob_count >= 1,
        "the live checkpoint blob must be retained, got {report:?}"
    );
    assert!(
        report.deleted_blob_count >= 1,
        "the superseded checkpoint blob must be reclaimed, got {report:?}"
    );

    // The reachable checkpoint survived: the session still loads at generation 2.
    let read = store
        .load_session()
        .await
        .expect("load after gc")
        .expect("session after gc");
    assert_eq!(
        read.checkpoint
            .and_then(|checkpoint| {
                checkpoint
                    .decode_component::<ToolState>(crate::store::TOOL_STATE_CHECKPOINT_COMPONENT)
                    .expect("decode reachable tool state")
            })
            .map(|tool_state| tool_state.generation()),
        Some(2),
        "gc must preserve the reachable checkpoint's snapshots"
    );

    // Idempotent: with nothing newly unreachable, a second sweep deletes nothing.
    let second = store.gc_unreachable().await.expect("second gc");
    assert_eq!(
        second.deleted_blob_count, 0,
        "gc must never reclaim reachable blobs, got {second:?}"
    );
}

/// Manifest rows are GC roots, not read authorization (FIG-653).
pub(super) async fn attachment_manifest_reference_tracking_and_gc_root_set(
    store: Arc<dyn RuntimePersistence>,
) {
    let intent_id = AttachmentId::parse(format!("{:x}", sha256_of(b"intent-only")))
        .expect("valid attachment id");
    let committed_id =
        AttachmentId::parse(format!("{:x}", sha256_of(b"committed"))).expect("valid attachment id");
    let intent = |id: &AttachmentId, at: u64| AttachmentIntent {
        attachment_id: id.clone(),
        session_id: SessionId::from("root"),
        canonical_uri: format!("lash-attachment://blake3/{id}"),
        intent_at_epoch_ms: at,
        owner_kind: None,
        owner_id: None,
    };
    store
        .record_intent(intent(&intent_id, 100))
        .expect("record intent-only");
    store
        .record_intent(intent(&committed_id, 100))
        .expect("record committed intent");
    store
        .commit_refs(
            &SessionId::from("root"),
            std::slice::from_ref(&committed_id),
        )
        .expect("commit attachment ref");

    // Root set: every live ref, intent or committed.
    let refs = store.list_all_refs().expect("list all refs");
    assert!(refs.contains(&intent_id), "intents feed the GC root set");
    assert!(refs.contains(&committed_id), "commits feed the GC root set");

    // Uncommitted listing still distinguishes intents from commits.
    let uncommitted = store.list_uncommitted(1_000_000).expect("list uncommitted");
    assert!(
        uncommitted
            .iter()
            .any(|entry| entry.attachment_id == intent_id),
        "an uncommitted intent is listed as uncommitted"
    );
    assert!(
        !uncommitted
            .iter()
            .any(|entry| entry.attachment_id == committed_id),
        "a committed attachment is not listed as uncommitted"
    );

    // Forget drops the ref from the root set.
    store
        .forget(&SessionId::from("root"), &intent_id)
        .expect("forget intent ref");
    assert!(
        !store
            .list_all_refs()
            .map(|refs| refs.contains(&intent_id))
            .expect("ref dropped"),
        "a forgotten ref is no longer held"
    );
    assert!(
        !store
            .list_all_refs()
            .expect("list after forget")
            .contains(&intent_id),
        "a forgotten ref leaves the root set"
    );
}

pub(super) fn sha256_of(bytes: &[u8]) -> impl std::fmt::LowerHex {
    use sha2::{Digest, Sha256};
    Sha256::digest(bytes)
}

pub(super) async fn append_receipt_survives_reopen(factory: ReopenableRuntimePersistence) {
    let mut state = RuntimeSessionState {
        session_id: SessionId::from("root"),
        ..RuntimeSessionState::new(crate::SessionPolicy::new(crate::TurnBudget::Unbounded))
    };
    let nodes = vec![crate::SessionAppendNode::plugin(
        "append-receipt-reopen",
        serde_json::json!({"value": "reopen"}),
    )];
    let (first_commit, _) =
        append_request_commit(&mut state, "append-receipt-reopen", &nodes, None);
    let first =
        commit_runtime_state_for_test(&factory.open, first_commit, "append-receipt-reopen-first")
            .await
            .expect("commit append receipt before reopen");

    let mut reopened_state = loaded_conformance_state(&factory.reopen).await;
    let (retry_commit, _) =
        append_request_commit(&mut reopened_state, "append-receipt-reopen", &nodes, None);
    let replay = factory
        .reopen
        .commit_runtime_state(retry_commit)
        .await
        .expect("reopened store replays append receipt");
    assert!(replay.receipt_replayed);
    assert_eq!(replay.head_revision, first.head_revision);
    assert_eq!(replay.checkpoint_ref, first.checkpoint_ref);
    assert_eq!(replay.committed_leaf_node_id, first.committed_leaf_node_id);
    assert_eq!(
        replay.realized_node_timestamps,
        first.realized_node_timestamps
    );
}

pub(super) async fn runtime_persistence_survives_reopen(factory: ReopenableRuntimePersistence) {
    session_execution_lease_first_claim_excludes_concurrent_reopen_handles(&factory).await;

    let meta = SessionMeta {
        pending_observer_intents: Vec::new(),
        session_id: SessionId::from("root"),
        relation: SessionRelation::Root,
    };
    factory
        .open
        .save_session_meta(meta.clone())
        .await
        .expect("save meta");
    let mut state = RuntimeSessionState {
        session_id: SessionId::from("root"),
        ..RuntimeSessionState::new(crate::SessionPolicy::new(crate::TurnBudget::Unbounded))
    };
    state.set_tool_state_snapshot(Some(
        ToolState::default().with_generation_for_conformance(77),
    ));
    let initial_commit = commit_runtime_state_for_test(
        &factory.open,
        RuntimeCommit::persisted_state_for_test(&state, &[]),
        "reopen",
    )
    .await
    .expect("commit state");
    state.head_revision = initial_commit.head_revision;

    let application_lease = claim_session_execution_lease_for_test(
        &factory.open,
        &SessionId::from("root"),
        "reopen-applications",
    )
    .await;
    let mut expected_applications = Vec::new();
    for (turn_index, turn_id) in ["z-reopen-application", "a-reopen-application"]
        .into_iter()
        .enumerate()
    {
        factory
            .open
            .enqueue_pending_turn_input(
                pending_next_turn_input_draft(
                    &SessionId::from("root"),
                    &format!("reopen application {turn_index}"),
                )
                .with_source_key(format!("host:reopen-application-{turn_index}")),
            )
            .await
            .expect("enqueue reopen application");
        let mut claim = factory
            .open
            .claim_next_turn_inputs(
                &SessionId::from("root"),
                &application_lease.fence(),
                &lease_owner("reopen-applications"),
                1,
            )
            .await
            .expect("claim reopen application")
            .expect("reopen application claim");
        claim.record_initial_turn_application(
            &crate::TurnId::from(turn_id),
            &format!("reopen-application-message-{turn_index}"),
        );
        expected_applications.extend(claim.applications.clone());

        let mut commit = RuntimeCommit::persisted_state_for_test(&state, &[])
            .completing_turn_input_claim(claim.completion());
        if turn_index == 1 {
            commit = commit.releasing_session_execution_lease(application_lease.completion());
        }
        commit.turn_commit =
            RuntimeTurnCommitStamp::new(crate::OperationId::turn("root", turn_id, "final"));
        let result = factory
            .open
            .commit_runtime_state(commit)
            .await
            .expect("commit reopen application");
        state.head_revision = result.head_revision;
    }
    let queued = factory
        .open
        .enqueue_queued_work(
            queued_draft(
                &SessionId::from("root"),
                "survives reopen",
                DeliveryPolicy::EarliestSafeBoundary,
            )
            .with_source_key("reopen:queued"),
        )
        .await
        .expect("enqueue queued work");
    let attachment = AttachmentId::parse("reopen-attachment").expect("valid attachment id");
    factory
        .open
        .record_intent(AttachmentIntent {
            attachment_id: attachment.clone(),
            session_id: SessionId::from("root"),
            canonical_uri: "sha256:reopen-attachment".to_string(),
            intent_at_epoch_ms: 100,
            owner_kind: None,
            owner_id: None,
        })
        .expect("record attachment intent");

    let reopened_meta = factory
        .reopen
        .load_session_meta()
        .await
        .expect("load reopened meta")
        .expect("reopened meta");
    assert_eq!(reopened_meta, meta);
    let reopened = factory
        .reopen
        .load_session()
        .await
        .expect("load reopened state")
        .expect("reopened state");
    assert_eq!(reopened.session_id, "root");
    assert_eq!(
        reopened
            .checkpoint
            .as_ref()
            .and_then(|checkpoint| {
                checkpoint
                    .decode_component::<ToolState>(crate::store::TOOL_STATE_CHECKPOINT_COMPONENT)
                    .expect("decode reopened tool state")
            })
            .map(|tool_state| tool_state.generation()),
        Some(77)
    );
    assert_eq!(
        factory
            .reopen
            .list_turn_input_applications(&SessionId::from("root"))
            .await
            .expect("list applications from reopened handle"),
        expected_applications,
        "a fresh durable handle must reconcile applications in turn-commit order"
    );
    let reopened_queue = factory
        .reopen
        .list_queued_work(&SessionId::from("root"))
        .await
        .expect("list reopened queue");
    assert_eq!(reopened_queue.len(), 1);
    assert_eq!(reopened_queue[0].batch_id, queued.batch_id);
    assert_eq!(
        queued_batch_text(&reopened_queue[0]),
        Some("survives reopen")
    );
    let reopened_intents = factory
        .reopen
        .list_uncommitted(200)
        .expect("list reopened attachment intents");
    assert!(
        reopened_intents
            .iter()
            .any(|intent| intent.attachment_id == attachment),
        "attachment intent rows must survive reopening a durable store"
    );
}

pub(super) async fn session_execution_lease_first_claim_excludes_concurrent_reopen_handles(
    factory: &ReopenableRuntimePersistence,
) {
    let barrier = Arc::new(tokio::sync::Barrier::new(3));
    let open = Arc::clone(&factory.open);
    let reopen = Arc::clone(&factory.reopen);
    let open_barrier = Arc::clone(&barrier);
    let reopen_barrier = Arc::clone(&barrier);
    let open_owner = lease_owner("owner-a");
    let reopen_owner = lease_owner("owner-b");

    let open_claim = crate::task::spawn(async move {
        open_barrier.wait().await;
        open.try_claim_session_execution_lease(
            &SessionId::from("first-claim-race"),
            &open_owner,
            "session-execution-lease-first-claim-excludes-concurrent-reopen-handles-executor",
            60_000,
        )
        .await
    });
    let reopen_claim = crate::task::spawn(async move {
        reopen_barrier.wait().await;
        reopen
            .try_claim_session_execution_lease(
                &SessionId::from("first-claim-race"),
                &reopen_owner,
                "session-execution-lease-first-claim-excludes-concurrent-reopen-handles-executor-2",
                60_000,
            )
            .await
    });

    barrier.wait().await;
    let open_claim = open_claim
        .await
        .expect("join open first-claim race")
        .expect("open first-claim race");
    let reopen_claim = reopen_claim
        .await
        .expect("join reopen first-claim race")
        .expect("reopen first-claim race");
    let open_lease = open_claim.acquired();
    let reopen_lease = reopen_claim.acquired();
    let claim_count = usize::from(open_lease.is_some()) + usize::from(reopen_lease.is_some());
    assert_eq!(
        claim_count, 1,
        "exactly one concurrent first claim may acquire a session execution lease"
    );
    if let Some(lease) = open_lease.as_ref().or(reopen_lease.as_ref()) {
        factory
            .open
            .release_session_execution_lease(&lease.completion())
            .await
            .expect("release first-claim race winner");
    }
}

pub(super) async fn queued_wake_delivery_is_source_key_idempotent_and_claimed_once(
    store: Arc<dyn RuntimePersistence>,
) {
    let wake = ProcessWakeDelivery {
        version: crate::PROCESS_WAKE_DELIVERY_FORMAT_VERSION,
        wake_id: "wake-1".to_string(),
        target_session_id: SessionId::from("root"),
        process_id: ProcessId::from("process-1"),
        process_incarnation: crate::ProcessIncarnation::from_registration_sequence(1),
        sequence: 7,
        event_type: "process.wake".to_string(),
        event_invocation: RuntimeInvocation {
            attribution: RuntimeAttribution::for_session("root"),
            subject: RuntimeSubject::ProcessEvent {
                process_id: ProcessId::from("process-1"),
                sequence: 7,
                event_type: "process.wake".to_string(),
            },
            caused_by: None,
            replay: None,
        },
        process_caused_by: None,
        authority: crate::QueuedWorkAuthority::default(),
        input: "wake payload".to_string(),
        created_at_ms: 1,
    };
    let malformed = QueuedWorkBatchDraft::new(
        wake.target_session_id.clone(),
        DeliveryPolicy::EarliestSafeBoundary,
        crate::TurnWorkPayload::process_wake(wake.clone()),
    )
    .with_source_key(crate::process_wake_source_key(
        &wake.process_id,
        wake.sequence,
    ));
    store
        .enqueue_queued_work(malformed)
        .await
        .expect_err("process-wake enqueue must require structural producer identity");

    let first = store
        .enqueue_queued_work(crate::process_wake_batch_draft(wake.clone()))
        .await
        .expect("enqueue wake");
    let replay = store
        .enqueue_queued_work(crate::process_wake_batch_draft(wake.clone()))
        .await
        .expect("replay wake enqueue");
    assert_eq!(
        first.batch_id, replay.batch_id,
        "wake source-key replay must return the original queued batch"
    );
    assert_eq!(
        store
            .list_queued_work(&SessionId::from("root"))
            .await
            .expect("list queued wakes")
            .len(),
        1,
        "replayed wake must not create a second queued delivery"
    );

    let session_lease =
        claim_session_execution_lease_for_test(&store, &SessionId::from("root"), "wake-owner")
            .await;
    let claim = store
        .claim_ready_queued_work(
            &SessionId::from("root"),
            &session_lease.fence(),
            &lease_owner("wake-owner"),
            QueuedWorkClaimBoundary::Idle,
            crate::testing::queued_work_claim_policy(10),
        )
        .await
        .expect("claim wake")
        .claim()
        .expect("wake claim");
    assert_eq!(claim.batches.len(), 1);
    assert_eq!(claim.batches[0].items.len(), 1);
    assert!(matches!(
        claim.batches[0].items[0].payload,
        QueuedWorkPayload::ProcessWake { .. }
    ));
    let state = RuntimeSessionState {
        session_id: SessionId::from("root"),
        ..RuntimeSessionState::new(crate::SessionPolicy::new(crate::TurnBudget::Unbounded))
    };
    store
        .commit_runtime_state(
            RuntimeCommit::persisted_state_for_test(&state, &[])
                .releasing_session_execution_lease(session_lease.completion())
                .completing_queue_claim(claim.completion()),
        )
        .await
        .expect("wake delivery completion commits");
    assert!(
        store
            .list_queued_work(&SessionId::from("root"))
            .await
            .expect("list after wake completion")
            .is_empty(),
        "completed wake delivery must be removed exactly once"
    );
    let consumed_replay = store
        .enqueue_queued_work(crate::process_wake_batch_draft(wake))
        .await
        .expect_err("late no-live-row wake must trip the receiver floor");
    assert!(matches!(
        consumed_replay,
        StoreError::ProcessWakeSequenceRewound { .. }
    ));
    assert!(
        store
            .list_queued_work(&SessionId::from("root"))
            .await
            .expect("list after consumed wake redelivery")
            .is_empty(),
        "receiver evidence must prevent a late redelivery from recreating queued work"
    );
}

pub(super) async fn final_commit_stamp_is_idempotent_and_conflicts_on_changed_hash(
    store: Arc<dyn RuntimePersistence>,
) {
    let mut state = RuntimeSessionState {
        session_id: SessionId::from("root"),
        ..RuntimeSessionState::new(crate::SessionPolicy::new(crate::TurnBudget::Unbounded))
    };
    state.ensure_agent_frame_initialized();
    state.session_graph.data_mut().nodes[0].timestamp = "2026-07-26T10:00:00Z".to_string();
    state.set_execution_state_snapshot(Some(vec![7; 1_024]));
    let operation = crate::OperationId::turn("root", "provider-turn", "final");
    let (stamped_commit, _) = RuntimeCommit::persisted_state_for_test(&state, &[])
        .with_operation(operation.clone())
        .expect("derive and stamp first commit");
    let turn_commit_hash = stamped_commit
        .turn_commit_hash()
        .expect("first commit hash");

    let session_lease =
        claim_session_execution_lease_for_test(&store, &SessionId::from("root"), "provider-turn")
            .await;
    let first = store
        .commit_runtime_state(
            stamped_commit
                .clone()
                .releasing_session_execution_lease(session_lease.completion()),
        )
        .await
        .expect("first final commit requires a live session execution lease");
    let mut replay_state = state.clone();
    replay_state.session_graph.data_mut().nodes[0].timestamp = "2026-07-26T10:00:09Z".to_string();
    let (replay_commit, _) = RuntimeCommit::persisted_state_for_test(&replay_state, &[])
        .with_operation(operation.clone())
        .expect("derive and stamp replay");
    let replay_hash = replay_commit
        .turn_commit_hash()
        .expect("replay commit hash");
    assert_eq!(replay_hash, turn_commit_hash);
    let retry = store
        .commit_runtime_state(replay_commit)
        .await
        .expect("same final commit retries idempotently without a live lease");
    assert_eq!(retry.head_revision, first.head_revision);
    assert_eq!(retry.checkpoint_ref, first.checkpoint_ref);
    let receipt_json = serde_json::to_string(&first).expect("serialize commit receipt");
    assert!(
        !receipt_json.contains("execution_state_snapshot"),
        "commit receipts must retain frame references and timestamps, never snapshot bytes"
    );
    replay_state.apply_persisted_commit_result(retry.clone());

    let mut retry_from_new_head = RuntimeCommit::persisted_state_for_test(&state, &[])
        .with_operation(operation.clone())
        .expect("stamp retry from advanced head")
        .0;
    retry_from_new_head.expected_head_revision = first.head_revision;
    let retry_hash = retry_from_new_head
        .turn_commit_hash()
        .expect("retry commit hash");
    assert_eq!(
        retry_hash, turn_commit_hash,
        "turn commit identity must not depend on the optimistic CAS revision"
    );

    let changed_state = RuntimeSessionState {
        session_id: SessionId::from("root"),
        turn_index: 1,
        ..RuntimeSessionState::new(crate::SessionPolicy::new(crate::TurnBudget::Unbounded))
    };
    let mut changed = RuntimeCommit::persisted_state_for_test(&changed_state, &[]);
    changed.turn_commit =
        RuntimeTurnCommitStamp::new(crate::OperationId::turn("root", "provider-turn", "final"));
    let err = store
        .commit_runtime_state(changed)
        .await
        .expect_err("same provider turn id with a different commit hash must conflict");
    assert!(
        matches!(&err, StoreError::RuntimeTurnCommitConflict { .. }),
        "unexpected changed-hash error: {err:?}"
    );
}

pub(super) async fn store_computed_hash_rejects_mutated_commit(store: Arc<dyn RuntimePersistence>) {
    let mut state = RuntimeSessionState {
        session_id: SessionId::from("root"),
        ..RuntimeSessionState::new(crate::SessionPolicy::new(crate::TurnBudget::Unbounded))
    };
    state.ensure_agent_frame_initialized();
    let operation = crate::OperationId::turn("root", "realization-guard", "final");
    let frame_key = crate::FrameKey::from_caller_material("realization-guard-frame")
        .expect("non-empty frame material");
    let node_id = crate::session_graph::frame_node_id(&state.session_id, frame_key.as_str());
    let graph = crate::GraphAppend {
        nodes: vec![crate::SessionNodeRecord {
            node_id: node_id.to_string(),
            parent_node_id: None,
            timestamp: "2026-07-26T10:00:00Z".to_string(),
            payload: crate::SessionNodePayload::FrameOpen {
                frame_key,
                reason: AgentFrameReason::initial(),
                assignment: crate::AgentFrameAssignment::from_policy(crate::SessionPolicy::new(
                    crate::TurnBudget::Unbounded,
                )),
                protocol_turn_options: ProtocolTurnOptions::default(),
            },
        }],
        leaf_node_id: Some(node_id.to_string()),
    };
    let (first, node_id_mapping) =
        RuntimeCommit::persisted_state_with_graph_commit(&state, graph, &[])
            .with_operation(operation)
            .expect("stamp guarded commit");
    assert_eq!(
        node_id_mapping,
        vec![(node_id.to_string(), node_id.to_string())],
        "operation stamping must return the append-id mapping"
    );
    commit_runtime_state_for_test(&store, first.clone(), "realization-guard")
        .await
        .expect("first guarded commit");

    let first_hash = first.turn_commit_hash().expect("first store-computed hash");
    let mut divergent_replay = first;
    let crate::GraphAppend { nodes, .. } = &mut divergent_replay.graph;
    nodes[0].parent_node_id = Some("proposal-only-parent".to_string());
    let divergent_hash = divergent_replay
        .turn_commit_hash()
        .expect("mutated store-computed hash");
    assert_ne!(
        divergent_hash, first_hash,
        "the receipt identity must cover mutated topology"
    );
    let err = crate::store::commit_runtime_state_verified(store.as_ref(), divergent_replay)
        .await
        .expect_err("the store must reject a mutated commit reusing an operation id");
    assert!(
        matches!(&err, StoreError::RuntimeTurnCommitConflict { .. }),
        "unexpected mutated-commit error: {err:?}"
    );
    let stored = store
        .load_node(&node_id)
        .await
        .expect("load guarded node")
        .expect("guarded node remains stored");
    assert_eq!(
        stored.parent_node_id, None,
        "a rejected receipt replay must not adopt or persist proposal topology"
    );
}

pub(super) async fn commit_rejects_non_derived_append_node_ids(store: Arc<dyn RuntimePersistence>) {
    let mut state = RuntimeSessionState {
        session_id: SessionId::from("root"),
        ..RuntimeSessionState::new(crate::SessionPolicy::new(crate::TurnBudget::Unbounded))
    };
    state.ensure_agent_frame_initialized();
    let operation = crate::OperationId::turn("root", "guard-turn", "final");
    let graph = crate::GraphAppend {
        nodes: vec![crate::SessionNodeRecord {
            node_id: "rogue-node-id".to_string(),
            parent_node_id: None,
            timestamp: "2026-07-26T10:00:00Z".to_string(),
            payload: crate::SessionNodePayload::Plugin {
                plugin_type: "guard".to_string(),
                body: crate::session_graph::SharedJsonValue::new(serde_json::json!({"ok": true})),
            },
        }],
        leaf_node_id: Some("rogue-node-id".to_string()),
    };
    let mut commit = RuntimeCommit::persisted_state_with_graph_commit(&state, graph, &[]);
    commit.turn_commit = RuntimeTurnCommitStamp::new(operation);
    let err = commit_runtime_state_for_test(&store, commit, "node-guard")
        .await
        .expect_err("store must rederive append node ids before writing");
    assert!(
        matches!(&err, StoreError::NodeIdDerivationMismatch { .. }),
        "unexpected node-derivation error: {err:?}"
    );
    assert!(
        store
            .load_session()
            .await
            .expect("load after guard rejection")
            .is_none(),
        "guard rejection must happen before any durable write"
    );
}

pub(super) async fn append_rejects_existing_node_id_collision(store: Arc<dyn RuntimePersistence>) {
    let mut state = RuntimeSessionState {
        session_id: SessionId::from("root"),
        ..RuntimeSessionState::new(crate::SessionPolicy::new(crate::TurnBudget::Unbounded))
    };
    state.ensure_agent_frame_initialized();
    let frame_key =
        crate::FrameKey::from_caller_material("collision-frame").expect("non-empty frame material");
    let colliding_id = crate::session_graph::frame_node_id(&state.session_id, frame_key.as_str());
    let original = crate::SessionNodeRecord {
        node_id: colliding_id.to_string(),
        parent_node_id: None,
        timestamp: "2026-07-26T10:00:00Z".to_string(),
        payload: crate::SessionNodePayload::FrameOpen {
            frame_key: frame_key.clone(),
            reason: AgentFrameReason::new("original"),
            assignment: crate::AgentFrameAssignment::from_policy(crate::SessionPolicy::new(
                crate::TurnBudget::Unbounded,
            )),
            protocol_turn_options: ProtocolTurnOptions::default(),
        },
    };
    state.session_graph =
        crate::SessionGraph::from_nodes(vec![original.clone()], Some(colliding_id.to_string()))
            .expect("collision fixture seed graph is valid");
    let initial = RuntimeCommit::persisted_state_for_test(&state, &[]);
    let first = commit_runtime_state_for_test(&store, initial, "collision-seed")
        .await
        .expect("seed colliding durable node");

    let replacement = crate::SessionNodeRecord {
        payload: crate::SessionNodePayload::FrameOpen {
            frame_key,
            reason: AgentFrameReason::new("replacement"),
            assignment: crate::AgentFrameAssignment::from_policy(crate::SessionPolicy::new(
                crate::TurnBudget::Unbounded,
            )),
            protocol_turn_options: ProtocolTurnOptions::default(),
        },
        ..original
    };
    let mut append = RuntimeCommit::persisted_state_with_graph_commit(
        &state,
        crate::GraphAppend {
            nodes: vec![replacement],
            leaf_node_id: Some(colliding_id.to_string()),
        },
        &[],
    );
    append.expected_head_revision = first.head_revision;
    let err = commit_runtime_state_for_test(&store, append, "collision-append")
        .await
        .expect_err("append must reject an id already present in durable history");
    assert!(
        matches!(
            &err,
            StoreError::NodeIdCollision { node_id } if node_id == colliding_id.as_str()
        ),
        "unexpected durable collision error: {err:?}"
    );
    let stored = store
        .load_node(&colliding_id)
        .await
        .expect("load original node")
        .expect("original node remains");
    let (reason, _, _) = stored.frame_open().expect("stored frame");
    assert_eq!(reason.as_str(), "original");
}

pub(super) async fn append_rejects_duplicate_batch_node_ids(store: Arc<dyn RuntimePersistence>) {
    let state = RuntimeSessionState {
        session_id: SessionId::from("root"),
        ..RuntimeSessionState::new(crate::SessionPolicy::new(crate::TurnBudget::Unbounded))
    };
    let duplicate_node_id = caller_frame_node_id(&SessionId::from("root"), "duplicate");
    let commit = RuntimeCommit::persisted_state_with_graph_commit(
        &state,
        crate::GraphAppend {
            nodes: vec![
                sample_session_node(&SessionId::from("root"), "duplicate", None),
                sample_session_node(&SessionId::from("root"), "duplicate", None),
            ],
            leaf_node_id: Some(duplicate_node_id.to_string()),
        },
        &[],
    );
    let err = commit_runtime_state_for_test(&store, commit, "duplicate-batch")
        .await
        .expect_err("a duplicate id in one append must abort the whole commit");
    assert!(
        matches!(
            &err,
            StoreError::NodeIdCollision { node_id } if node_id == duplicate_node_id.as_str()
        ),
        "unexpected duplicate-id error: {err:?}"
    );
    assert!(
        store
            .load_session()
            .await
            .expect("load after duplicate rejection")
            .is_none(),
        "duplicate rejection must happen before any durable write"
    );
}

pub(super) async fn commit_rejects_unresolvable_leaf(store: Arc<dyn RuntimePersistence>) {
    let state = RuntimeSessionState {
        session_id: SessionId::from("root"),
        ..RuntimeSessionState::new(crate::SessionPolicy::new(crate::TurnBudget::Unbounded))
    };
    let commit = RuntimeCommit::persisted_state_with_graph_commit(
        &state,
        crate::GraphAppend {
            nodes: vec![sample_session_node(
                &SessionId::from("root"),
                "valid-node",
                None,
            )],
            leaf_node_id: Some("missing-leaf".to_string()),
        },
        &[],
    );
    let err = commit_runtime_state_for_test(&store, commit, "invalid-leaf")
        .await
        .expect_err("commit leaf must resolve in the post-commit live graph");
    assert!(
        matches!(
            &err,
            StoreError::InvalidGraphLeaf {
                leaf_node_id: Some(leaf)
            } if leaf == "missing-leaf"
        ),
        "unexpected unresolved-leaf error: {err:?}"
    );
    let valid_node_id = caller_frame_node_id(&SessionId::from("root"), "valid-node");
    assert!(
        store
            .load_node(&valid_node_id)
            .await
            .expect("load after leaf rejection")
            .is_none(),
        "leaf rejection must abort the whole commit"
    );
}

pub(super) async fn commit_rejects_missing_leaf(store: Arc<dyn RuntimePersistence>) {
    let state = RuntimeSessionState {
        session_id: SessionId::from("root"),
        ..RuntimeSessionState::new(crate::SessionPolicy::new(crate::TurnBudget::Unbounded))
    };
    let missing = RuntimeCommit::persisted_state_with_graph_commit(
        &state,
        crate::GraphAppend {
            nodes: vec![sample_session_node(
                &SessionId::from("root"),
                "node-without-leaf",
                None,
            )],
            leaf_node_id: None,
        },
        &[],
    );
    let err = commit_runtime_state_for_test(&store, missing, "missing-leaf")
        .await
        .expect_err("a non-empty graph commit requires a resolving leaf");
    assert!(
        matches!(&err, StoreError::InvalidGraphLeaf { leaf_node_id: None }),
        "unexpected missing-leaf error: {err:?}"
    );
    let node_without_leaf_id = caller_frame_node_id(&SessionId::from("root"), "node-without-leaf");
    assert!(
        store
            .load_node(&node_without_leaf_id)
            .await
            .expect("load after missing leaf rejection")
            .is_none(),
        "missing leaf rejection must abort the whole commit"
    );
}

pub(super) async fn empty_append_cannot_move_the_head(store: Arc<dyn RuntimePersistence>) {
    let mut state = RuntimeSessionState {
        session_id: SessionId::from("empty-append-head-move"),
        ..RuntimeSessionState::new(crate::SessionPolicy::new(crate::TurnBudget::Unbounded))
    };
    state.ensure_agent_frame_initialized();
    let first = store
        .commit_runtime_state(RuntimeCommit::persisted_state_for_test(&state, &[]))
        .await
        .expect("seed the live head");
    let old_leaf = state.session_graph.leaf_node_id.clone();
    state.apply_persisted_commit_result(first);
    let mut move_attempt = RuntimeCommit::persisted_state_with_graph_commit(
        &state,
        crate::GraphAppend {
            nodes: Vec::new(),
            leaf_node_id: None,
        },
        &[],
    );
    move_attempt.current_frame_node_id = old_leaf.clone().map(|frame_node_id| {
        crate::FrameNodeId::new(frame_node_id).expect("test frame identity is non-empty")
    });
    let error = store
        .commit_runtime_state(move_attempt)
        .await
        .expect_err("an empty append must not move the head");
    assert!(
        matches!(&error, StoreError::InvalidGraphLeaf { leaf_node_id: None }),
        "unexpected empty-append error: {error:?}"
    );
    let loaded = store
        .load_session()
        .await
        .expect("load after rejected empty append")
        .expect("seeded session remains");
    assert_eq!(loaded.graph.leaf_node_id, old_leaf);
}
