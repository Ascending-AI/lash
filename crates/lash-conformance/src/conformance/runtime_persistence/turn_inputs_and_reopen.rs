use super::*;
use lash_core::store::IngressSettlement;
use pretty_assertions::assert_eq;

#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
pub async fn pending_turn_inputs_source_keys_order_cancel_and_cross_session(
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
            .map(|read| read.input.input_id.as_str())
            .collect::<Vec<_>>(),
        vec![first.input_id.as_str(), second.input_id.as_str()]
    );
    assert!(listed[0].input.enqueue_seq < listed[1].input.enqueue_seq);
    assert!(listed.iter().all(|read| read.input.session_id == "root"));

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
            .map(|read| read.input.input_id.as_str())
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
    assert_eq!(
        terminal_replay.state.kind(),
        crate::TurnInputStateKind::Cancelled
    );
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

#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
pub async fn pending_turn_input_bulk_and_suffix_cancellation(store: Arc<dyn RuntimePersistence>) {
    let first = store
        .enqueue_pending_turn_input(
            pending_next_turn_input_draft(&SessionId::from("pending-bulk-cancel"), "bulk first")
                .with_source_key("bulk:first"),
        )
        .await
        .expect("enqueue first bulk input");
    let second = store
        .enqueue_pending_turn_input(
            pending_next_turn_input_draft(&SessionId::from("pending-bulk-cancel"), "bulk second")
                .with_source_key("bulk:second"),
        )
        .await
        .expect("enqueue second bulk input");
    let third = store
        .enqueue_pending_turn_input(pending_next_turn_input_draft(
            &SessionId::from("pending-bulk-cancel"),
            "bulk third",
        ))
        .await
        .expect("enqueue third bulk input");
    let bulk = store
        .cancel_pending_turn_inputs(
            &SessionId::from("pending-bulk-cancel"),
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
            .list_pending_turn_inputs(&SessionId::from("pending-bulk-cancel"))
            .await
            .expect("list after bulk cancellation")
            .iter()
            .map(|read| read.input.input_id.as_str())
            .collect::<Vec<_>>(),
        vec![second.input_id.as_str()]
    );

    let suffix_anchor = store
        .enqueue_pending_turn_input(
            pending_next_turn_input_draft(&SessionId::from("pending-bulk-cancel"), "suffix anchor")
                .with_source_key("suffix:anchor"),
        )
        .await
        .expect("enqueue suffix anchor");
    let active_claimed = store
        .enqueue_pending_turn_input(
            pending_active_turn_input_draft(
                &SessionId::from("pending-bulk-cancel"),
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
            pending_next_turn_input_draft(&SessionId::from("pending-bulk-cancel"), "suffix later")
                .with_source_key("suffix:later"),
        )
        .await
        .expect("enqueue suffix later");
    let fence = seal_drive_fence_for_test(
        &store,
        &SessionId::from("pending-bulk-cancel"),
        "suffix-cancel-owner",
    )
    .await;
    let active_admission = admit_at_checkpoint_for_test(
        &store,
        &fence,
        &TurnId::from("suffix-active-turn"),
        &TurnId::from("suffix-active-turn"),
        crate::CheckpointKind::AfterWork,
        "suffix-active-turn:step",
        10,
        crate::testing::queued_work_admission_policy(10),
    )
    .await
    .expect("admit the suffix active input");
    assert_eq!(
        active_admission
            .inputs
            .as_ref()
            .map(|inputs| inputs.input_ids())
            .unwrap_or_default(),
        vec![active_claimed.input_id.clone()]
    );

    let suffix = store
        .cancel_pending_turn_input_suffix(
            &SessionId::from("pending-bulk-cancel"),
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
        crate::PendingTurnInputCancelOutcome::AlreadyAdmitted { input, root } => {
            assert_eq!(input.input_id, active_claimed.input_id);
            assert_eq!(root.as_str(), "suffix-active-turn");
        }
        other => panic!("expected already-admitted suffix outcome, got {other:?}"),
    }
    expect_cancelled_pending_input(outcomes[2].clone(), &suffix_later.input_id);

    let suffix_by_id_anchor = store
        .enqueue_pending_turn_input(pending_next_turn_input_draft(
            &SessionId::from("pending-bulk-cancel"),
            "suffix by id anchor",
        ))
        .await
        .expect("enqueue suffix by id anchor");
    let suffix_by_id_later = store
        .enqueue_pending_turn_input(
            pending_next_turn_input_draft(
                &SessionId::from("pending-bulk-cancel"),
                "suffix by id later",
            )
            .with_source_key("suffix:id-later"),
        )
        .await
        .expect("enqueue suffix by id later");
    let suffix_by_id = store
        .cancel_pending_turn_input_suffix(
            &SessionId::from("pending-bulk-cancel"),
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
                &SessionId::from("pending-bulk-cancel"),
                &crate::PendingTurnInputCancelTarget::source_key("suffix:missing"),
            )
            .await
            .expect("missing suffix anchor"),
        crate::PendingTurnInputSuffixCancelOutcome::AnchorNotFound { .. }
    ));
    assert_eq!(
        store
            .list_pending_turn_inputs(&SessionId::from("pending-bulk-cancel"))
            .await
            .expect("list after suffix cancellation")
            .iter()
            .map(|read| read.input.input_id.as_str())
            .collect::<Vec<_>>(),
        vec![second.input_id.as_str()]
    );
}

/// A root's admission binds the next-turn prefix, a host cancel of a bound
/// row answers `AlreadyAdmitted{root}`, reads name the root, a completion
/// under a superseded fence is refused and changes nothing, and the live
/// fence's completion settles every row to a tombstone no admission takes.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
pub async fn pending_turn_inputs_admit_settle_and_fence(store: Arc<dyn RuntimePersistence>) {
    let session = SessionId::from("root");
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
        .enqueue_pending_turn_input(pending_next_turn_input_draft(&session, "second next"))
        .await
        .expect("enqueue second next input");
    let fence = seal_drive_fence_for_test(&store, &session, "turn-input-owner").await;
    let root = "input-root";
    let admission = admitted_root(
        &store,
        &fence,
        root,
        lash_core::store::AdmittedHead::Input(first.input_id.clone()),
    )
    .await;
    assert_eq!(
        admission.input_ids(),
        vec![first.input_id.clone(), second.input_id.clone()]
    );
    let admitted = admission.inputs.as_ref().expect("the root admits inputs");
    assert!(
        admitted
            .inputs
            .iter()
            .any(|input| input.input.items.iter().any(|item| matches!(
                item,
                crate::InputItem::Attachment {
                    source: crate::AttachmentSource::Inline { bytes, .. }
                } if bytes == &[1, 2, 3]
            )))
    );
    match store
        .cancel_pending_turn_input(&session, &first.input_id)
        .await
        .expect("cancel an admitted input")
    {
        crate::PendingTurnInputCancelOutcome::AlreadyAdmitted {
            input,
            root: holder,
        } => {
            assert_eq!(input.input_id, first.input_id);
            assert_eq!(holder.as_str(), root);
        }
        other => panic!("an admitted input must not be cancellable, got {other:?}"),
    }
    let admitted_reads = store
        .list_pending_turn_inputs(&session)
        .await
        .expect("list admitted inputs");
    assert_eq!(
        admitted_reads
            .iter()
            .map(|read| read.input.input_id.as_str())
            .collect::<Vec<_>>(),
        vec![first.input_id.as_str(), second.input_id.as_str()]
    );
    let bound = crate::PendingTurnInputReadStatus::Admitted {
        root: TurnId::from(root),
    };
    assert!(admitted_reads.iter().all(|read| read.status == bound));

    let successor = seal_drive_fence_for_test(&store, &session, "turn-input-successor").await;
    let err = try_end_root(&store, &fence, completing_admission(root, &admission))
        .await
        .expect_err("a completion under a superseded fence must fail");
    assert!(matches!(err, StoreError::StaleDriveFence { .. }));
    let reads_after_stale_completion = store
        .list_pending_turn_inputs(&session)
        .await
        .expect("list after the refused completion");
    assert_eq!(reads_after_stale_completion.len(), 2);
    assert!(
        reads_after_stale_completion
            .iter()
            .all(|read| read.status == bound),
        "a refused completion must leave the rows bound to their root"
    );

    end_root(&store, &successor, completing_admission(root, &admission)).await;
    assert!(
        store
            .list_pending_turn_inputs(&session)
            .await
            .expect("list after the live completion")
            .is_empty()
    );
    assert!(matches!(
        store
            .cancel_pending_turn_input(&session, &first.input_id)
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
    assert_eq!(
        completed_replay.state.kind(),
        crate::TurnInputStateKind::Completed
    );
    assert!(
        admit_root_for_test(
            &store,
            &successor,
            &TurnId::from("after-completion-root"),
            lash_core::store::AdmittedHead::Input(first.input_id.clone()),
        )
        .await
        .expect("admit after completing inputs")
        .is_none(),
        "completed pending input tombstones must not be admitted"
    );
}

/// FIG-905: a checkpoint executor can durably bind an active input and crash
/// before its effect outcome journals the admission. The successor reruns
/// the same checkpoint step under its newer fence and reads back exactly the
/// rows that step bound; the predecessor's completion is refused, and the
/// successor settles them.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
pub async fn a_checkpoint_admission_rerun_returns_its_own_rows(store: Arc<dyn RuntimePersistence>) {
    const SESSION_ID: &str = "fig905-active-reacquire";
    const TURN_ID: &str = "fig905-active-reacquire:turn";
    const STEP: &str = "fig905-active-reacquire:turn:checkpoint";
    let session_id = SessionId::from(SESSION_ID);
    let turn = crate::TurnId::from(TURN_ID);
    let input = store
        .enqueue_pending_turn_input(pending_active_turn_input_draft(
            &session_id,
            &turn,
            crate::TurnInputCheckpointBoundary::AfterWork,
            "accepted before checkpoint outcome",
        ))
        .await
        .expect("enqueue active input");

    let predecessor =
        seal_drive_fence_for_test(&store, &session_id, "fig905-active-predecessor").await;
    let admit = |fence: lash_core::store::DriveFence| {
        let store = Arc::clone(&store);
        let turn = turn.clone();
        async move {
            admit_at_checkpoint_for_test(
                &store,
                &fence,
                &turn,
                &turn,
                crate::CheckpointKind::AfterWork,
                STEP,
                10,
                crate::testing::queued_work_admission_policy(10),
            )
            .await
            .expect("admit at the checkpoint")
        }
    };
    let bound = admit(predecessor.clone()).await;
    let bound_inputs = bound
        .inputs
        .as_ref()
        .expect("the checkpoint binds the input");
    assert_eq!(
        bound_inputs.inputs[0].state.kind(),
        crate::TurnInputStateKind::Accepted
    );

    let successor = seal_drive_fence_for_test(&store, &session_id, "fig905-active-successor").await;
    let rerun = admit(successor.clone()).await;
    assert!(
        rerun.queued.is_none(),
        "an accepted-only checkpoint fixture must not rely on queued work"
    );
    assert_eq!(
        rerun
            .inputs
            .as_ref()
            .map(|inputs| inputs.input_ids())
            .unwrap_or_default(),
        vec![input.input_id.clone()],
        "the rerun reads back the rows its step bound"
    );

    let settlement = completing_checkpoint(IngressSettlement::new(turn.clone()), &rerun);
    let stale_error = try_end_root(&store, &predecessor, settlement.clone())
        .await
        .expect_err("the superseded predecessor's completion is refused");
    assert!(matches!(stale_error, StoreError::StaleDriveFence { .. }));
    end_root(&store, &successor, settlement).await;
}

/// FIG-1511, FIG-3927 §2.6: an input bound to a root stays bound across a
/// drive supersession, and host cancellation refuses it. The root's terminal
/// releases it: an accepted active-turn input names a turn that is over, so
/// it is re-deferred open, cancellable and vacuumed, and a settlement that
/// still names it is refused because no root holds it.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
pub async fn accepted_turn_input_released_by_its_root_terminal_is_cancelled_and_vacuumed(
    store: Arc<dyn RuntimePersistence>,
) {
    const SESSION_ID: &str = "fig1511-orphaned-accepted";
    const TURN_ID: &str = "fig1511-orphaned-accepted:turn";
    let session_id = SessionId::from(SESSION_ID);
    let turn = TurnId::from(TURN_ID);
    let input = store
        .enqueue_pending_turn_input(pending_active_turn_input_draft(
            &session_id,
            &turn,
            crate::TurnInputCheckpointBoundary::AfterWork,
            "accepted before the drive was superseded",
        ))
        .await
        .expect("enqueue active input");
    let fence = seal_drive_fence_for_test(&store, &session_id, "fig1511-accepted-owner").await;
    let admitted = admit_at_checkpoint_for_test(
        &store,
        &fence,
        &turn,
        &turn,
        crate::CheckpointKind::AfterWork,
        "fig1511:step",
        1,
        crate::testing::queued_work_admission_policy(1),
    )
    .await
    .expect("admit the active input");
    let inputs = admitted.inputs.clone().expect("the input is admitted");
    assert_eq!(inputs.inputs[0].input_id, input.input_id);
    assert_eq!(
        inputs.inputs[0].state.kind(),
        crate::TurnInputStateKind::Accepted
    );

    let successor = seal_drive_fence_for_test(&store, &session_id, "fig1511-successor").await;
    assert!(
        matches!(
            store
                .cancel_pending_turn_input(&session_id, &input.input_id)
                .await
                .expect("cancel the bound input after supersession"),
            crate::PendingTurnInputCancelOutcome::AlreadyAdmitted { ref root, .. }
                if *root == turn
        ),
        "a drive supersession does not release a root's rows"
    );

    // The root ends without settling the input: its terminal releases it.
    end_root(&store, &successor, IngressSettlement::new(turn.clone())).await;
    assert_eq!(
        store
            .root_of_input(&session_id, &input.input_id)
            .await
            .expect("read the released input's root"),
        None,
        "a released checkpoint input stays unbound"
    );
    let released = store
        .list_pending_turn_inputs(&session_id)
        .await
        .expect("list the released input");
    assert_eq!(released.len(), 1);
    assert_eq!(released[0].status, crate::PendingTurnInputReadStatus::Open);
    assert_eq!(
        released[0].input.state.kind(),
        crate::TurnInputStateKind::DeferredNextTurn
    );
    expect_cancelled_pending_input(
        store
            .cancel_pending_turn_input(&session_id, &input.input_id)
            .await
            .expect("cancel the released input"),
        &input.input_id,
    );
    let mut zombie = IngressSettlement::new(turn.clone());
    zombie.completed_inputs.push(inputs.completion());
    let stale_error = store
        .commit_runtime_state(settling_commit_for_test(
            head_commit(&store, &session_id).await,
            &successor,
            zombie,
        ))
        .await
        .expect_err("a cancelled input must refuse a settlement that still names it");
    assert!(matches!(
        stale_error,
        StoreError::IngressRowNotAdmitted {
            session_id: ref refused_session_id,
            ref root,
            ref row,
            admitted_root: None,
        } if *refused_session_id == session_id
            && *root == turn
            && **row == lash_core::store::IngressRowId::Input(input.input_id.clone())
    ));
    let vacuum = store
        .vacuum()
        .await
        .expect("vacuum cancelled accepted input");
    assert_eq!(vacuum.removed_node_count, 0);
    assert_eq!(vacuum.removed_pending_turn_input_tombstone_count, 1);
    assert!(matches!(
        store
            .cancel_pending_turn_input(&session_id, &input.input_id)
            .await
            .expect("read accepted input after vacuum"),
        crate::PendingTurnInputCancelOutcome::NotFound
    ));
}

#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
pub async fn pending_turn_input_cancel_covers_active_and_deferred_states(
    store: Arc<dyn RuntimePersistence>,
    effect_host: Arc<dyn crate::EffectHost>,
) {
    let authority = crate::TurnCancellationAuthority::new(
        effect_host.turn_control_binding_id(),
        effect_host as Arc<dyn crate::AwaitEventResolver>,
    );
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
        cancelled_active.ingress(),
        crate::TurnInputIngress::ActiveTurn { .. }
    ));
    let cancelled_next = store
        .cancel_pending_turn_input(&SessionId::from("root"), &next_cancel.input_id)
        .await
        .expect("cancel next input");
    expect_cancelled_pending_input(cancelled_next, &next_cancel.input_id);

    let lease =
        seal_drive_fence_for_test(&store, &SessionId::from("root"), "cancel-input-owner").await;
    let state = RuntimeSessionState {
        session_id: SessionId::from("root"),
        ..RuntimeSessionState::new(crate::SessionPolicy::new(crate::TurnBudget::Unbounded))
    };
    store
        .commit_runtime_state(
            lash_core::testing::store_fixtures::authorize_completion_deferral_for_test(
                store.as_ref(),
                &authority,
                &lease,
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
            .map(|read| read.input.input_id.as_str())
            .collect::<Vec<_>>(),
        vec![active_keep.input_id.as_str()],
        "cancelled active and next-turn inputs must not be resurrected by interrupt deferral"
    );
    assert!(matches!(
        pending_after_interrupt[0].input.ingress(),
        crate::TurnInputIngress::NextTurn
    ));
    assert_eq!(
        pending_after_interrupt[0].input.state.kind(),
        crate::TurnInputStateKind::DeferredNextTurn
    );

    let cancelled_deferred = store
        .cancel_pending_turn_input(&SessionId::from("root"), &active_keep.input_id)
        .await
        .expect("cancel deferred input");
    expect_cancelled_pending_input(cancelled_deferred, &active_keep.input_id);
    assert!(
        admit_root_for_test(
            &store,
            &lease,
            &TurnId::from("after-cancel-root"),
            lash_core::store::AdmittedHead::Input(active_keep.input_id.clone()),
        )
        .await
        .expect("admit after cancelling deferred input")
        .is_none(),
        "cancelled deferred input must not be admitted"
    );
}

#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
pub async fn pending_active_turn_inputs_defer_unaccepted_once_on_interrupt(
    store: Arc<dyn RuntimePersistence>,
    effect_host: Arc<dyn crate::EffectHost>,
) {
    let authority = crate::TurnCancellationAuthority::new(
        effect_host.turn_control_binding_id(),
        effect_host as Arc<dyn crate::AwaitEventResolver>,
    );
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

    let lease =
        seal_drive_fence_for_test(&store, &SessionId::from("root"), "active-input-owner").await;
    let claim_turn_id = crate::TurnId::from(turn_id);
    let admitted = admit_at_checkpoint_for_test(
        &store,
        &lease,
        &claim_turn_id,
        &claim_turn_id,
        crate::CheckpointKind::AfterWork,
        "active-turn-1:step",
        1,
        crate::testing::queued_work_admission_policy(1),
    )
    .await
    .expect("admit active inputs");
    let claim = admitted.inputs.expect("active input admission");
    assert_eq!(
        claim
            .inputs
            .iter()
            .map(|input| input.input_id.as_str())
            .collect::<Vec<_>>(),
        vec![accepted.input_id.as_str()],
        "AfterWork admissions must include matching active inputs admitted at that boundary in order"
    );
    assert!(matches!(
        claim.inputs[0].input.items.last(),
        Some(crate::InputItem::Attachment {
            source: crate::AttachmentSource::Inline { bytes, .. }
        }) if bytes == &[9, 8, 7]
    ));

    let state = RuntimeSessionState {
        session_id: SessionId::from("root"),
        ..RuntimeSessionState::new(crate::SessionPolicy::new(crate::TurnBudget::Unbounded))
    };
    store
        .commit_runtime_state(
            lash_core::testing::store_fixtures::authorize_completion_deferral_for_test(
                store.as_ref(),
                &authority,
                &lease,
                settling_commit_for_test(
                    RuntimeCommit::persisted_state_for_test(&state, &[]),
                    &lease,
                    {
                        let mut settlement = IngressSettlement::new(claim_turn_id.clone());
                        settlement.completed_inputs.push(claim.completion());
                        settlement
                    },
                )
                .deferring_interrupted_turn_inputs(turn_id, None),
            )
            .await
            .expect("authorize active input deferral"),
        )
        .await
        .expect("interrupt commit completes accepted inputs and defers unaccepted inputs");
    let pending_after_interrupt = store
        .list_pending_turn_inputs(&SessionId::from("root"))
        .await
        .expect("list after interrupt deferral");
    assert_eq!(
        pending_after_interrupt
            .iter()
            .map(|read| read.input.input_id.as_str())
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
        .filter(|read| read.input.ingress().active_turn_id().is_none())
        .collect::<Vec<_>>();
    assert_eq!(
        deferred_after_interrupt
            .iter()
            .map(|read| read.input.input_id.as_str())
            .collect::<Vec<_>>(),
        vec![
            unaccepted.input_id.as_str(),
            before_completion.input_id.as_str()
        ],
        "accepted active inputs must be completed and only unaccepted matching active inputs become next-turn work"
    );
    assert!(deferred_after_interrupt.iter().all(|read| {
        matches!(read.input.ingress(), crate::TurnInputIngress::NextTurn)
            && read.input.state.kind() == crate::TurnInputStateKind::DeferredNextTurn
    }));
    assert!(
        pending_after_interrupt
            .iter()
            .any(|read| read.input.ingress().active_turn_id() == Some(&TurnId::from("other-turn"))),
        "inputs for other active turns must not be deferred by this interrupt"
    );

    let next_claim = admitted_root(
        &store,
        &lease,
        "deferred-root",
        lash_core::store::AdmittedHead::Input(unaccepted.input_id.clone()),
    )
    .await;
    assert_eq!(
        next_claim.input_ids(),
        vec![
            unaccepted.input_id.clone(),
            before_completion.input_id.clone()
        ]
    );
    end_root(
        &store,
        &lease,
        completing_admission("deferred-root", &next_claim),
    )
    .await;
    assert!(
        store
            .list_pending_turn_inputs(&SessionId::from("root"))
            .await
            .expect("list after completing deferred input")
            .iter()
            .all(|read| {
                read.input.ingress().active_turn_id() == Some(&TurnId::from("other-turn"))
            }),
        "inputs for other active turns must not be deferred by this interrupt"
    );
}

/// A turn that cannot commit leaves no input pinned to it (FIG-1573,
/// FIG-3927 §2.6).
///
/// A turn that never reaches its commit - killed, aborted, or fenced - ends
/// its root with a terminal, and the terminal write is the repair: every
/// row still bound to the root is released, and every open active-turn input
/// addressed to any of the root's physical turns is re-deferred to the next
/// turn. Nothing else moves: a row pinned to another root's turn stays put,
/// and a terminal under a superseded fence writes nothing at all.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
pub async fn a_turn_that_cannot_commit_leaves_no_input_pinned_to_it(
    store: Arc<dyn RuntimePersistence>,
) {
    let session = SessionId::from("root");
    let dead_root = TurnId::from("fig1573-dead-root");
    let later_turn = lash_core::store::PhysicalTurn::derive_turn_id(&dead_root, 2);
    let other_turn_id = "fig1573-other-turn";
    let head = store
        .enqueue_pending_turn_input(pending_next_turn_input_draft(
            &session,
            "the dead root's head",
        ))
        .await
        .expect("enqueue the head");
    let stale = seal_drive_fence_for_test(&store, &session, "fig1573-owner").await;
    let admission = admitted_root(
        &store,
        &stale,
        dead_root.as_str(),
        lash_core::store::AdmittedHead::Input(head.input_id.clone()),
    )
    .await;
    assert_eq!(admission.input_ids(), vec![head.input_id.clone()]);
    let mut orphaned = Vec::new();
    for (turn, text) in [
        (&dead_root, "pinned to the root's first turn"),
        (&later_turn, "pinned to a later physical turn of the root"),
    ] {
        orphaned.push(
            store
                .enqueue_pending_turn_input(pending_active_turn_input_draft(
                    &session,
                    turn,
                    crate::TurnInputCheckpointBoundary::AfterWork,
                    text,
                ))
                .await
                .expect("enqueue an input pinned to the dead root"),
        );
    }
    let other = store
        .enqueue_pending_turn_input(pending_active_turn_input_draft(
            &session,
            &TurnId::from(other_turn_id),
            crate::TurnInputCheckpointBoundary::AfterWork,
            "pinned to a turn that can still deliver",
        ))
        .await
        .expect("enqueue the untouched input");

    // A superseded caller repairs nothing: its terminal is refused whole.
    let successor = seal_drive_fence_for_test(&store, &session, "fig1573-successor").await;
    let refusal = try_end_root(&store, &stale, IngressSettlement::new(dead_root.clone()))
        .await
        .expect_err("a superseded fence must be refused inside the terminal write");
    assert!(
        matches!(refusal, StoreError::StaleDriveFence { .. }),
        "a superseded terminal must be refused by the drive fence: {refusal:?}"
    );
    let untouched = store
        .list_pending_turn_inputs(&session)
        .await
        .expect("list pending inputs after the refused terminal");
    for input in &orphaned {
        let row = untouched
            .iter()
            .find(|read| read.input.input_id == input.input_id)
            .expect("the pinned row is still queued");
        assert_eq!(
            row.input.state.kind(),
            crate::TurnInputStateKind::PendingActive,
            "a refused terminal must leave the row exactly as it found it"
        );
    }

    // The live fence's terminal releases the root's rows and re-defers every
    // open input addressed to its turns.
    end_root(
        &store,
        &successor,
        IngressSettlement::new(dead_root.clone()),
    )
    .await;
    let pending = store
        .list_pending_turn_inputs(&session)
        .await
        .expect("list pending inputs after the terminal");
    for input in orphaned.iter().chain(std::iter::once(&head)) {
        let row = pending
            .iter()
            .find(|read| read.input.input_id == input.input_id)
            .expect("the repaired input is still queued");
        assert_eq!(row.status, crate::PendingTurnInputReadStatus::Open);
        assert_eq!(
            row.input.state.kind(),
            crate::TurnInputStateKind::DeferredNextTurn
        );
        assert_eq!(row.input.ingress(), crate::TurnInputIngress::NextTurn);
    }
    let untouched_row = pending
        .iter()
        .find(|read| read.input.input_id == other.input_id)
        .expect("the other turn's input is still queued");
    assert_eq!(
        untouched_row.input.state.kind(),
        crate::TurnInputStateKind::PendingActive
    );
    assert_eq!(
        untouched_row.input.ingress().active_turn_id(),
        Some(&crate::TurnId::from(other_turn_id)),
        "a row pinned to another root's turn must never be swept"
    );

    // The repaired rows are next-turn work again, which is the whole point.
    let next = admitted_root(
        &store,
        &successor,
        "fig1573-next-root",
        lash_core::store::AdmittedHead::Input(head.input_id.clone()),
    )
    .await;
    assert_eq!(
        next.input_ids(),
        vec![
            head.input_id.clone(),
            orphaned[0].input_id.clone(),
            orphaned[1].input_id.clone(),
        ]
    );
}

#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
pub async fn pending_turn_input_duplicate_input_id(store: Arc<dyn RuntimePersistence>) {
    let first = store
        .enqueue_pending_turn_input(
            pending_next_turn_input_draft(&SessionId::from("root"), "first")
                .with_input_id("dup:input"),
        )
        .await
        .expect("enqueue first pending input");
    // A draft reusing a stored `input_id` with different content, or from
    // another session, is refused: the SQL schemas declare the column
    // globally UNIQUE.
    let changed = store
        .enqueue_pending_turn_input(
            pending_next_turn_input_draft(&SessionId::from("root"), "changed")
                .with_input_id("dup:input"),
        )
        .await
        .expect_err("a second pending input reusing an input_id must be refused");
    assert!(
        matches!(
            changed,
            StoreError::PendingTurnInputIdConflict {
                ref session_id,
                ref input_id,
            } if session_id.as_str() == "root" && input_id.as_str() == "dup:input"
        ),
        "the refusal is the typed id-conflict error, got {changed:?}"
    );
    // An identical same-session re-submission is the same admission re-run:
    // it returns the stored row and files nothing. Only lash's own journaled
    // turn acceptance provisions explicit ids (ADR 0069 §6, FIG-3513), so this
    // is how a re-run acceptance body finds the row its first run wrote.
    let identical = store
        .enqueue_pending_turn_input(
            pending_next_turn_input_draft(&SessionId::from("root"), "first")
                .with_input_id("dup:input"),
        )
        .await
        .expect("an identical same-session re-submission adopts the stored row");
    assert_eq!(identical.input_id, first.input_id);
    assert_eq!(identical.enqueue_seq, first.enqueue_seq);
    let cross_session = store
        .enqueue_pending_turn_input(
            pending_next_turn_input_draft(&SessionId::from("other"), "other")
                .with_input_id("dup:input"),
        )
        .await
        .expect_err("input_id uniqueness spans sessions");
    assert!(
        matches!(cross_session, StoreError::PendingTurnInputIdConflict { .. }),
        "the cross-session refusal is the typed id-conflict error, got {cross_session:?}"
    );

    // The refused drafts filed nothing: the stored row is untouched and alone.
    let listed = store
        .list_pending_turn_inputs(&SessionId::from("root"))
        .await
        .expect("list pending inputs after the refused duplicates");
    assert_eq!(
        listed
            .iter()
            .map(|read| read.input.input_id.as_str())
            .collect::<Vec<_>>(),
        vec![first.input_id.as_str()]
    );
    assert_eq!(pending_input_text(&listed[0].input), Some("first"));
    assert!(
        store
            .list_pending_turn_inputs(&SessionId::from("other"))
            .await
            .expect("list other session after the refused duplicate")
            .is_empty()
    );
}

fn source_keyed_active_draft(
    turn_id: &str,
    min_boundary: crate::TurnInputCheckpointBoundary,
    text: &str,
    source_key: &str,
) -> crate::PendingTurnInputDraft {
    pending_active_turn_input_draft(
        &SessionId::from("root"),
        &TurnId::from(turn_id),
        min_boundary,
        text,
    )
    .with_source_key(source_key)
}

fn assert_source_key_conflict(
    result: Result<crate::PendingTurnInput, StoreError>,
    source_key: &str,
    existing: &crate::PendingTurnInput,
    context: &str,
) {
    match result {
        Err(StoreError::PendingTurnInputSourceKeyConflict {
            session_id,
            source_key: conflicting_key,
            existing_input_id,
        }) => {
            assert_eq!(session_id, "root", "{context}");
            assert_eq!(conflicting_key, source_key, "{context}");
            assert_eq!(existing_input_id, existing.input_id, "{context}");
        }
        other => panic!("{context}: expected a typed source-key conflict, got {other:?}"),
    }
}

/// An identical source-key retry is the same submission whatever happened to
/// the row after admission (FIG-3544).
///
/// Both Defer paths rewrite the row's current ingress to `next_turn`: the
/// final commit of the turn the row named, and orphan repair. The replay
/// verdict compares the digest written once at admission, so a byte-identical
/// `active_turn` retry after either rewrite — and after the deferred row later
/// settles into a tombstone — returns the existing row instead of a conflict.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
pub async fn identical_retry_after_defer_is_existing_not_conflict(
    store: Arc<dyn RuntimePersistence>,
    effect_host: Arc<dyn crate::EffectHost>,
) {
    let authority = crate::TurnCancellationAuthority::new(
        effect_host.turn_control_binding_id(),
        effect_host as Arc<dyn crate::AwaitEventResolver>,
    );
    let session_id = SessionId::from("root");
    let ended_turn = "fig3544-ended-turn";
    let dead_turn = "fig3544-dead-turn";
    let commit_deferred = || {
        source_keyed_active_draft(
            ended_turn,
            crate::TurnInputCheckpointBoundary::BeforeCompletion,
            "deferred by the turn's final commit",
            "host:fig3544-commit-defer",
        )
    };
    let repair_deferred = || {
        source_keyed_active_draft(
            dead_turn,
            crate::TurnInputCheckpointBoundary::AfterWork,
            "deferred by its root's terminal",
            "host:fig3544-repair-defer",
        )
    };
    let first_commit_deferred = store
        .enqueue_pending_turn_input(commit_deferred())
        .await
        .expect("admit the input the turn's final commit defers");
    let first_repair_deferred = store
        .enqueue_pending_turn_input(repair_deferred())
        .await
        .expect("admit the input the root terminal defers");

    let lease = seal_drive_fence_for_test(&store, &session_id, "fig3544-owner").await;
    let state = RuntimeSessionState {
        session_id: session_id.clone(),
        ..RuntimeSessionState::new(crate::SessionPolicy::new(crate::TurnBudget::Unbounded))
    };
    store
        .commit_runtime_state(
            lash_core::testing::store_fixtures::authorize_completion_deferral_for_test(
                store.as_ref(),
                &authority,
                &lease,
                RuntimeCommit::persisted_state_for_test(&state, &[])
                    .deferring_interrupted_turn_inputs(ended_turn, None),
            )
            .await
            .expect("authorize the final-commit deferral"),
        )
        .await
        .expect("the turn's final commit defers its undelivered input");
    // The dead turn's root ends: its terminal re-defers the input pinned to it.
    end_root(
        &store,
        &lease,
        IngressSettlement::new(TurnId::from(dead_turn)),
    )
    .await;

    let pending = store
        .list_pending_turn_inputs(&session_id)
        .await
        .expect("list the deferred inputs");
    for first in [&first_commit_deferred, &first_repair_deferred] {
        let row = pending
            .iter()
            .find(|read| read.input.input_id == first.input_id)
            .expect("the deferred input is still queued");
        assert_eq!(
            row.input.state,
            crate::TurnInputState::DeferredNextTurn,
            "the Defer rewrote the row's current ingress to next_turn"
        );
    }

    for (retry, first, path) in [
        (
            commit_deferred(),
            &first_commit_deferred,
            "final-commit defer",
        ),
        (
            repair_deferred(),
            &first_repair_deferred,
            "root-terminal defer",
        ),
    ] {
        let replayed = store
            .enqueue_pending_turn_input(retry)
            .await
            .unwrap_or_else(|err| {
                panic!("an identical retry after a {path} must replay, not conflict: {err}")
            });
        assert_eq!(
            replayed.input_id, first.input_id,
            "an identical retry after a {path} returns the existing row"
        );
        assert_eq!(
            replayed.state,
            crate::TurnInputState::DeferredNextTurn,
            "the replay reports the row's current state, which the retry does not change"
        );
    }

    // Settle both deferred rows into tombstones: replay still matches.
    let next = admitted_root(
        &store,
        &lease,
        "fig3544-next-root",
        lash_core::store::AdmittedHead::Input(first_commit_deferred.input_id.clone()),
    )
    .await;
    assert_eq!(next.input_ids().len(), 2);
    end_root(
        &store,
        &lease,
        completing_admission("fig3544-next-root", &next),
    )
    .await;
    for (retry, first) in [
        (commit_deferred(), &first_commit_deferred),
        (repair_deferred(), &first_repair_deferred),
    ] {
        let replayed = store
            .enqueue_pending_turn_input(retry)
            .await
            .expect("an identical retry against the completed tombstone replays");
        assert_eq!(replayed.input_id, first.input_id);
        assert_eq!(
            replayed.state.kind(),
            crate::TurnInputStateKind::Completed,
            "the tombstone answers the replay until vacuum"
        );
    }
}

/// A source key re-presented with a different submission is a typed conflict
/// naming the existing row (FIG-3544).
///
/// Every submitted field is identity: the input, the turn the ingress names,
/// its minimum boundary, and the scope. The comparison is against the
/// submission as admitted, never the row's current ingress: once a Defer has
/// rewritten that to `next_turn`, a retry that restates `next_turn` is still a
/// different submission and still conflicts.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
pub async fn changed_retry_is_typed_conflict(store: Arc<dyn RuntimePersistence>) {
    let session_id = SessionId::from("root");
    let turn = "fig3544-conflict-turn";
    let key = "host:fig3544-conflict";
    let original = || {
        source_keyed_active_draft(
            turn,
            crate::TurnInputCheckpointBoundary::AfterWork,
            "original submission",
            key,
        )
    };
    let first = store
        .enqueue_pending_turn_input(original())
        .await
        .expect("admit the original submission");

    for (changed, context) in [
        (
            source_keyed_active_draft(
                turn,
                crate::TurnInputCheckpointBoundary::AfterWork,
                "changed submission",
                key,
            ),
            "a changed input conflicts",
        ),
        (
            source_keyed_active_draft(
                turn,
                crate::TurnInputCheckpointBoundary::BeforeCompletion,
                "original submission",
                key,
            ),
            "a changed minimum boundary conflicts",
        ),
        (
            source_keyed_active_draft(
                "fig3544-another-turn",
                crate::TurnInputCheckpointBoundary::AfterWork,
                "original submission",
                key,
            ),
            "a changed target turn conflicts",
        ),
        (
            pending_next_turn_input_draft(&session_id, "original submission").with_source_key(key),
            "a changed scope conflicts",
        ),
    ] {
        assert_source_key_conflict(
            store.enqueue_pending_turn_input(changed).await,
            key,
            &first,
            context,
        );
    }

    let lease = seal_drive_fence_for_test(&store, &session_id, "fig3544-conflict-owner").await;
    // The turn's root ends: its terminal re-defers the input pinned to it.
    end_root(&store, &lease, IngressSettlement::new(TurnId::from(turn))).await;
    assert_source_key_conflict(
        store
            .enqueue_pending_turn_input(
                pending_next_turn_input_draft(&session_id, "original submission")
                    .with_source_key(key),
            )
            .await,
        key,
        &first,
        "restating the row's rewritten current ingress is not the admitted submission",
    );
    let replayed = store
        .enqueue_pending_turn_input(original())
        .await
        .expect("the admitted submission still replays after the Defer");
    assert_eq!(replayed.input_id, first.input_id);
}
