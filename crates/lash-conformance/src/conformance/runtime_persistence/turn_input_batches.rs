//! Turn-input batches on the pending turn-input ledger (FIG-3842).
//!
//! A batch is one request, admitted in one transaction. Its new ids are
//! enqueued in request order at consecutive positions of the session's shared
//! ingress sequence, so no other producer's input or command lands inside the
//! block. An id a stored row answers with identical content returns that row
//! wherever it sits, even settled. A conflicting id, or one id named twice,
//! refuses the whole request: no row, no sequence position and no spec row
//! survives it.

use super::*;
use pretty_assertions::assert_eq;

/// Concurrent single-input and command producers racing the law's batches.
const RACING_PRODUCERS: usize = 4;
/// Items each racing producer enqueues.
const RACING_ITEMS: usize = 6;
/// Batches racing the producers.
const RACING_BATCHES: usize = 3;
/// Inputs per racing batch.
const BATCH_INPUTS: usize = 5;

fn batch_spec(guidance: &str) -> crate::RunSpec {
    crate::RunSpec::overrides(crate::RunOverrides {
        prompt: Some(crate::PromptLayer::new().with_contribution(
            crate::PromptContribution::guidance("Batch", guidance.to_string()),
        )),
        ..crate::RunOverrides::default()
    })
}

/// A keyed next-turn draft carrying `text` under `spec`.
fn keyed(
    session_id: &SessionId,
    key: &str,
    text: &str,
    spec: &crate::RunSpec,
) -> crate::PendingTurnInputDraft {
    pending_next_turn_input_draft(session_id, text)
        .with_source_key(key)
        .with_run_spec(spec.clone())
}

#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: the law's batches are well formed"
)]
fn batch(
    session_id: &SessionId,
    drafts: Vec<crate::PendingTurnInputDraft>,
) -> crate::PendingTurnInputBatch {
    crate::PendingTurnInputBatch::new(session_id.clone(), drafts).expect("a well-formed batch")
}

fn keys(rows: &[crate::PendingTurnInput]) -> Vec<Option<&str>> {
    rows.iter().map(|row| row.source_key.as_deref()).collect()
}

fn seqs(rows: &[crate::PendingTurnInput]) -> Vec<u64> {
    rows.iter().map(|row| row.enqueue_seq).collect()
}

/// `count` consecutive sequence positions from `first`.
fn consecutive(first: u64, count: usize) -> Vec<u64> {
    (first..).take(count).collect()
}

/// New ids stay contiguous and in request order while other producers race
/// them: every batch's rows come back in request order, at consecutive
/// positions of the session's one ingress sequence, and no single input or
/// command enqueued concurrently holds a position inside any batch's block.
/// The shared spec is interned once and every row names it.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
pub async fn a_turn_input_batch_enqueues_new_ids_contiguously_in_request_order(
    store: Arc<dyn RuntimeStore>,
) {
    let session_id = SessionId::from("turn-input-batches");
    let spec = batch_spec("batched shape");
    let hash = spec
        .hash()
        .expect("hash the batch spec")
        .expect("a non-default spec has a hash");
    let mut producers = tokio::task::JoinSet::new();
    for producer in 0..RACING_PRODUCERS {
        let store = Arc::clone(&store);
        let session_id = session_id.clone();
        producers.spawn(async move {
            let mut foreign = Vec::new();
            for item in 0..RACING_ITEMS {
                let seq = if item % 2 == 0 {
                    store
                        .enqueue_pending_turn_input(
                            pending_next_turn_input_draft(
                                &session_id,
                                &format!("single {producer}.{item}"),
                            )
                            .with_source_key(format!("single-{producer}-{item}")),
                        )
                        .await
                        .expect("a racing single input is admitted")
                        .enqueue_seq
                } else {
                    store
                        .enqueue_queued_work(queued_session_command_draft(
                            &session_id,
                            &format!("command {producer}.{item}"),
                        ))
                        .await
                        .expect("a racing command is admitted")
                        .enqueue_seq
                };
                foreign.push(seq);
                tokio::task::yield_now().await;
            }
            foreign
        });
    }
    let mut batches = tokio::task::JoinSet::new();
    for index in 0..RACING_BATCHES {
        let store = Arc::clone(&store);
        let session_id = session_id.clone();
        let spec = spec.clone();
        batches.spawn(async move {
            let drafts = (0..BATCH_INPUTS)
                .map(|position| {
                    keyed(
                        &session_id,
                        &format!("batch-{index}-{position}"),
                        &format!("batch {index} input {position}"),
                        &spec,
                    )
                })
                .collect();
            let rows = store
                .enqueue_pending_turn_inputs(batch(&session_id, drafts))
                .await
                .expect("the batch is admitted");
            (index, rows)
        });
    }
    let mut foreign = Vec::new();
    while let Some(joined) = producers.join_next().await {
        foreign.extend(joined.expect("a racing producer finished"));
    }
    let mut admitted = Vec::new();
    while let Some(joined) = batches.join_next().await {
        admitted.push(joined.expect("a racing batch finished"));
    }
    assert_eq!(admitted.len(), RACING_BATCHES);
    for (index, rows) in &admitted {
        let expected_keys = (0..BATCH_INPUTS)
            .map(|position| format!("batch-{index}-{position}"))
            .collect::<Vec<_>>();
        assert_eq!(
            keys(rows),
            expected_keys
                .iter()
                .map(|key| Some(key.as_str()))
                .collect::<Vec<_>>(),
            "batch {index} answers its rows in request order"
        );
        let first = rows[0].enqueue_seq;
        assert_eq!(
            seqs(rows),
            consecutive(first, BATCH_INPUTS),
            "batch {index} holds one contiguous block of the ingress sequence"
        );
        let block = first..first + BATCH_INPUTS as u64;
        assert!(
            foreign.iter().all(|seq| !block.contains(seq)),
            "no racing input or command landed inside batch {index}'s block {block:?}: {foreign:?}"
        );
        assert!(
            rows.iter().all(|row| row.run_spec.as_ref() == Some(&hash)),
            "every row of batch {index} names the shared spec"
        );
    }
    let pending = store
        .list_pending_turn_inputs(&session_id)
        .await
        .expect("list the session's inputs");
    assert_eq!(
        pending.len(),
        RACING_BATCHES * BATCH_INPUTS + RACING_PRODUCERS * RACING_ITEMS.div_ceil(2),
        "every batch input and racing single input is pending once"
    );
    assert_eq!(
        store
            .load_run_spec(&session_id, &hash)
            .await
            .expect("read the interned spec"),
        Some(spec),
        "the shared spec is interned and reads back exactly"
    );
}

/// A resent batch answers every id a stored row already holds with that row,
/// wherever it sits and whatever became of it, and enqueues only the new ids,
/// in request order, as one contiguous block after everything already
/// admitted. Resending the whole batch again, as after a lost reply, answers
/// every position with the same row and enqueues nothing.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
pub async fn a_resent_turn_input_batch_answers_its_existing_ids_and_enqueues_the_rest(
    store: Arc<dyn RuntimeStore>,
) {
    let session_id = SessionId::from("turn-input-batch-retries");
    let spec = batch_spec("retried shape");
    let draft = |key: &str| keyed(&session_id, key, &format!("input {key}"), &spec);

    let first = store
        .enqueue_pending_turn_inputs(batch(&session_id, vec![draft("a"), draft("b")]))
        .await
        .expect("admit the first batch");
    let between = store
        .enqueue_pending_turn_input(
            pending_next_turn_input_draft(&session_id, "a single send in between")
                .with_source_key("x"),
        )
        .await
        .expect("admit a single input after the first batch");
    // `a` settles before the batch is resent: its evidence still answers.
    store
        .cancel_pending_turn_input(&session_id, &first[0].input_id)
        .await
        .expect("withdraw `a`");

    let resent = batch(
        &session_id,
        vec![draft("c"), draft("a"), draft("d"), draft("b"), draft("e")],
    );
    let second = store
        .enqueue_pending_turn_inputs(resent.clone())
        .await
        .expect("admit the resent batch");
    assert_eq!(
        keys(&second),
        vec![Some("c"), Some("a"), Some("d"), Some("b"), Some("e")],
        "the rows answer in request order"
    );
    for (position, original) in [(1, &first[0]), (3, &first[1])] {
        assert_eq!(
            (&second[position].input_id, second[position].enqueue_seq),
            (&original.input_id, original.enqueue_seq),
            "a resent id answers its existing row where it sits"
        );
    }
    let new_seqs = [&second[0], &second[2], &second[4]]
        .map(|row| row.enqueue_seq)
        .to_vec();
    assert_eq!(
        new_seqs,
        consecutive(between.enqueue_seq + 1, 3),
        "the new ids are one contiguous block, in request order, after everything admitted"
    );

    let again = store
        .enqueue_pending_turn_inputs(resent)
        .await
        .expect("a lost reply is safe to resend");
    assert_eq!(
        again
            .iter()
            .map(|row| (&row.input_id, row.enqueue_seq))
            .collect::<Vec<_>>(),
        second
            .iter()
            .map(|row| (&row.input_id, row.enqueue_seq))
            .collect::<Vec<_>>(),
        "an identical resend answers every position with the same row"
    );
    assert_eq!(
        store
            .list_pending_turn_inputs(&session_id)
            .await
            .expect("list the session's inputs")
            .len(),
        5,
        "b, x, c, d and e are pending once each; the resends enqueued nothing"
    );
}

/// A conflict anywhere in a batch refuses the whole request: at every
/// position, a stored id resent with other content, or under another spec,
/// is a typed conflict, and the refused batch leaves no row, no sequence
/// position and no spec row behind. One id named twice in a request, by
/// source key or by input id, is refused before any store is asked.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
pub async fn a_conflict_or_a_repeated_id_refuses_the_whole_turn_input_batch(
    store: Arc<dyn RuntimeStore>,
) {
    let session_id = SessionId::from("turn-input-batch-refusals");
    let spec = batch_spec("stored shape");
    let stored = store
        .enqueue_pending_turn_inputs(batch(
            &session_id,
            ["p", "q", "r"]
                .map(|key| keyed(&session_id, key, &format!("input {key}"), &spec))
                .to_vec(),
        ))
        .await
        .expect("admit the stored batch");
    let before = store
        .list_pending_turn_inputs(&session_id)
        .await
        .expect("list the session's inputs")
        .into_iter()
        .map(|read| read.input.input_id)
        .collect::<Vec<_>>();
    let fresh_spec = batch_spec("a shape only the refused batches carry");
    let fresh_hash = fresh_spec
        .hash()
        .expect("hash the fresh spec")
        .expect("a non-default spec has a hash");

    for position in 0..3 {
        for (conflict, what) in [
            (
                keyed(&session_id, "q", "other content", &fresh_spec),
                "other content",
            ),
            (
                keyed(&session_id, "q", "input q", &fresh_spec),
                "another spec",
            ),
        ] {
            let mut drafts = (0..3)
                .map(|slot| {
                    keyed(
                        &session_id,
                        &format!("new-{position}-{slot}"),
                        "new input",
                        &fresh_spec,
                    )
                })
                .collect::<Vec<_>>();
            drafts[position] = conflict;
            let refused = store
                .enqueue_pending_turn_inputs(batch(&session_id, drafts))
                .await;
            assert!(
                matches!(
                    &refused,
                    Err(StoreError::PendingTurnInputSourceKeyConflict { source_key, existing_input_id, .. })
                        if source_key == "q" && *existing_input_id == stored[1].input_id
                ),
                "{what} at position {position} refuses the whole batch: {refused:?}"
            );
        }
    }
    let after = store
        .list_pending_turn_inputs(&session_id)
        .await
        .expect("list the session's inputs")
        .into_iter()
        .map(|read| read.input.input_id)
        .collect::<Vec<_>>();
    assert_eq!(after, before, "no refused batch enqueued anything");
    assert_eq!(
        store
            .load_run_spec(&session_id, &fresh_hash)
            .await
            .expect("read the refused batches' spec"),
        None,
        "no refused batch left its spec interned"
    );
    let next = store
        .enqueue_pending_turn_input(
            pending_next_turn_input_draft(&session_id, "after the refusals").with_source_key("s"),
        )
        .await
        .expect("admit an input after the refusals");
    assert_eq!(
        next.enqueue_seq,
        stored[2].enqueue_seq + 1,
        "no refused batch kept a sequence position"
    );

    for (drafts, what) in [
        (
            vec![
                keyed(&session_id, "twice", "one", &spec),
                keyed(&session_id, "once", "two", &spec),
                keyed(&session_id, "twice", "three", &spec),
            ],
            "a source key named twice",
        ),
        (
            vec![
                pending_next_turn_input_draft(&session_id, "one").with_input_id("ti:twice"),
                pending_next_turn_input_draft(&session_id, "two").with_input_id("ti:twice"),
            ],
            "an input id named twice",
        ),
    ] {
        let refused = crate::PendingTurnInputBatch::new(session_id.clone(), drafts);
        assert!(
            matches!(
                &refused,
                Err(StoreError::PendingTurnInputBatchDuplicate { session_id: refused_session, .. })
                    if *refused_session == session_id
            ),
            "{what} refuses the request: {refused:?}"
        );
    }
}
