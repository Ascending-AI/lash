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
