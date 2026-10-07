use super::*;
use pretty_assertions::assert_eq;

#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
pub async fn pending_turn_inputs_source_keys_order_cancel_and_cross_session(
    store: Arc<dyn RuntimeStore>,
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
        .vacuum(&SessionId::from("root"))
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
pub async fn pending_turn_input_bulk_and_suffix_cancellation(store: Arc<dyn RuntimeStore>) {
    // The run whose running turn the suffix's active input addresses.
    let session_id = SessionId::from("pending-bulk-cancel");
    active_run(&store, &session_id, &TurnId::from("suffix-active-turn")).await;
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
    let active_admitted = store
        .enqueue_pending_turn_input(
            pending_active_turn_input_draft(
                &SessionId::from("pending-bulk-cancel"),
                &TurnId::from("suffix-active-turn"),
                crate::TurnInputCheckpointBoundary::AfterWork,
                "suffix accepted active",
            )
            .with_source_key("suffix:admitted"),
        )
        .await
        .expect("enqueue suffix admitted input");
    let suffix_later = store
        .enqueue_pending_turn_input(
            pending_next_turn_input_draft(&SessionId::from("pending-bulk-cancel"), "suffix later")
                .with_source_key("suffix:later"),
        )
        .await
        .expect("enqueue suffix later");
    let active_admission = admit_at_checkpoint_for_test(
        &store,
        &session_id,
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
        vec![active_admitted.input_id.clone()]
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
        crate::PendingTurnInputCancelOutcome::AlreadyAdmitted { input, run } => {
            assert_eq!(input.input_id, active_admitted.input_id);
            assert_eq!(run.as_str(), "suffix-active-turn");
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
        // The input the checkpoint accepted stays listed, bound to its run,
        // until that run settles or releases it (FIG-4044).
        vec![second.input_id.as_str(), active_admitted.input_id.as_str()]
    );
}

#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
pub async fn pending_turn_input_duplicate_input_id(store: Arc<dyn RuntimeStore>) {
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
