//! The turn park feed (FIG-3659): the durable transition ledger every park
//! write and every park clear appends to in the same transaction.
//!
//! One ledger per store factory, shared across sessions: a host lists parked
//! turns through `list_turn_parks`, follows their transitions through
//! `turn_park_feed`, and reclaims feed history through the host-gated
//! `compact_turn_park_feed` lever. `count_unsettled_turns` reports the live
//! park count, the oldest park's age, and the per-reason split the gauges
//! record.

use std::collections::BTreeSet;
use std::num::NonZeroUsize;

use lash_sansio::TurnId;

use super::*;

#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: a literal nonzero page size"
)]
fn limit(n: usize) -> NonZeroUsize {
    NonZeroUsize::new(n).expect("a nonzero page size")
}

fn park_write(
    session_id: &SessionId,
    turn_id: &str,
    reason: crate::store::ParkReason,
    at_ms: u64,
) -> crate::store::TurnParkWrite {
    crate::store::TurnParkWrite {
        session_id: session_id.clone(),
        turn_id: TurnId::from(turn_id),
        reason,
        at_ms,
    }
}

fn divergence(message: &str) -> crate::store::ParkReason {
    crate::store::ParkReason::ReplayDivergence {
        message: message.to_string(),
    }
}

fn drift(message: &str) -> crate::store::ParkReason {
    crate::store::ParkReason::BindingDrift {
        message: message.to_string(),
    }
}

#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: a fresh factory creates its session"
)]
async fn create_bound_store(
    factory: &Arc<dyn crate::store::ConformanceSessionStoreFactory>,
    session_id: &SessionId,
) -> Arc<dyn RuntimePersistence> {
    factory
        .create_store(&session_store_request(
            session_id,
            "park-feed-model",
            crate::SessionRelation::Root,
        ))
        .await
        .expect("create the session's store")
}

async fn commit_turn(
    store: &Arc<dyn RuntimePersistence>,
    session_id: &SessionId,
    turn_id: &str,
    head_revision: u64,
    owner_id: &str,
) -> Result<crate::store::RuntimeCommitReceipt, crate::StoreError> {
    let state = RuntimeSessionState {
        session_id: session_id.clone(),
        head_revision,
        ..RuntimeSessionState::new(crate::SessionPolicy::new(crate::TurnBudget::Unbounded))
    };
    let commit = RuntimeCommit::persisted_state_with_operation_for_testing(
        &state,
        &[],
        crate::store::OperationId::turn(session_id.clone(), TurnId::from(turn_id), "final"),
    );
    commit_runtime_state_for_test(store, commit, owner_id).await
}

/// L1: `list_turn_parks` orders by `(since_ms, session_id)` and honors its
/// reason, session, age, keyset and limit clauses.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
pub async fn parked_turns_list_by_since_with_filters_and_keyset_pages(
    factory: Arc<dyn crate::store::ConformanceSessionStoreFactory>,
) {
    let session_a = SessionId::from("park-list-a");
    let session_b = SessionId::from("park-list-b");
    let session_c = SessionId::from("park-list-c");
    let session_d = SessionId::from("park-list-d");
    let store_a = create_bound_store(&factory, &session_a).await;
    let store_b = create_bound_store(&factory, &session_b).await;
    let store_c = create_bound_store(&factory, &session_c).await;
    let store_d = create_bound_store(&factory, &session_d).await;
    store_a
        .record_turn_park(&park_write(&session_a, "turn-a", drift("a drifted"), 100))
        .await
        .expect("park a");
    store_b
        .record_turn_park(&park_write(
            &session_b,
            "turn-b",
            divergence("b diverged"),
            200,
        ))
        .await
        .expect("park b");
    store_c
        .record_turn_park(&park_write(&session_c, "turn-c", drift("c drifted"), 300))
        .await
        .expect("park c");
    // `d` ties `b` on `since_ms`: the keyset's tie branch —
    // `since_ms = after_since AND session_id > after_session_id` — is what a
    // walk over the tie must take to visit both.
    store_d
        .record_turn_park(&park_write(&session_d, "turn-d", drift("d drifted"), 200))
        .await
        .expect("park d");

    let query = crate::store::TurnParkQuery {
        reasons: None,
        session: None,
        parked_at_or_before_ms: None,
        after: None,
        limit: limit(10),
    };
    let all = factory
        .list_turn_parks(&query)
        .await
        .expect("list all parks");
    assert_eq!(
        all.iter()
            .map(|park| park.session_id.clone())
            .collect::<Vec<_>>(),
        vec![
            session_a.clone(),
            session_b.clone(),
            session_d.clone(),
            session_c.clone()
        ],
        "parks list oldest-first by (since_ms, session_id), ties in session order"
    );

    let by_reason = factory
        .list_turn_parks(&crate::store::TurnParkQuery {
            reasons: Some(BTreeSet::from([crate::store::ParkReasonCode::BindingDrift])),
            ..query.clone()
        })
        .await
        .expect("list by reason");
    assert_eq!(
        by_reason
            .iter()
            .map(|park| park.session_id.clone())
            .collect::<Vec<_>>(),
        vec![session_a.clone(), session_d.clone(), session_c.clone()],
        "the reason filter keeps only matching parks"
    );
    let empty_reason = factory
        .list_turn_parks(&crate::store::TurnParkQuery {
            reasons: Some(BTreeSet::from([
                crate::store::ParkReasonCode::RetiredGeneration,
            ])),
            ..query.clone()
        })
        .await
        .expect("list an unused reason");
    assert!(
        empty_reason.is_empty(),
        "no park carries the cutover reason"
    );

    let by_session = factory
        .list_turn_parks(&crate::store::TurnParkQuery {
            session: Some(session_b.clone()),
            ..query.clone()
        })
        .await
        .expect("list one session");
    assert_eq!(
        by_session
            .iter()
            .map(|park| park.session_id.clone())
            .collect::<Vec<_>>(),
        vec![session_b.clone()],
        "the session filter keeps only its park"
    );

    let by_age = factory
        .list_turn_parks(&crate::store::TurnParkQuery {
            parked_at_or_before_ms: Some(200),
            ..query.clone()
        })
        .await
        .expect("list by age");
    assert_eq!(
        by_age
            .iter()
            .map(|park| park.session_id.clone())
            .collect::<Vec<_>>(),
        vec![session_a.clone(), session_b.clone(), session_d.clone()],
        "the age filter keeps parks at or before the cutoff"
    );

    // A limit-1 keyset walk visits every record once, in order — across the
    // `since_ms` tie between `b` and `d`.
    let mut visited: Vec<SessionId> = Vec::new();
    let mut after = None;
    loop {
        let page = factory
            .list_turn_parks(&crate::store::TurnParkQuery {
                after,
                limit: limit(1),
                ..query.clone()
            })
            .await
            .expect("walk the keyset");
        let Some(last) = page.last() else { break };
        assert_eq!(page.len(), 1, "the page honors its limit");
        visited.push(last.session_id.clone());
        after = Some((last.since_ms, last.session_id.clone()));
    }
    assert_eq!(
        visited,
        vec![
            session_a.clone(),
            session_b.clone(),
            session_d.clone(),
            session_c.clone()
        ],
        "the keyset walk visits each park once, in order, across the tie"
    );

    // Filters compose with the keyset: drifting parks strictly after a's row.
    let tail = factory
        .list_turn_parks(&crate::store::TurnParkQuery {
            reasons: Some(BTreeSet::from([crate::store::ParkReasonCode::BindingDrift])),
            after: Some((100, session_a.clone())),
            ..query.clone()
        })
        .await
        .expect("list filtered keyset tail");
    assert_eq!(
        tail.iter()
            .map(|park| park.session_id.clone())
            .collect::<Vec<_>>(),
        vec![session_d.clone(), session_c.clone()],
        "the keyset applies before the reason filter's remainder"
    );
}

/// L2: a same-turn re-park keeps `park_id` and `since_ms`, counts the refusal
/// in `attempts`, and writes no feed event; a different turn's park supersedes
/// the record and the feed shows `Unparked{Superseded}` then `Parked` under a
/// new `park_id`.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
pub async fn re_park_keeps_since_and_counts_attempts_and_another_turn_supersedes(
    factory: Arc<dyn crate::store::ConformanceSessionStoreFactory>,
) {
    let session_id = SessionId::from("park-supersede");
    let store = create_bound_store(&factory, &session_id).await;
    let first = store
        .record_turn_park(&park_write(&session_id, "turn-a", divergence("first"), 100))
        .await
        .expect("park the first turn");
    let reparked = store
        .record_turn_park(&park_write(&session_id, "turn-a", drift("again"), 200))
        .await
        .expect("re-park the same turn");
    assert_eq!(
        reparked.park_id, first.park_id,
        "a same-turn re-park keeps the park identity"
    );
    assert_eq!(
        reparked.since_ms, first.since_ms,
        "a same-turn re-park keeps the first park's age"
    );
    assert_eq!(reparked.attempts, 2, "the re-park counts the refusal");
    assert_eq!(reparked.last_refused_ms, 200);
    assert_eq!(reparked.reason, drift("again"));

    let page = factory
        .turn_park_feed(crate::store::ParkFeedCursor::initial(), limit(10))
        .await
        .expect("read the feed");
    assert_eq!(
        page.events.len(),
        1,
        "a same-turn re-park writes no feed event"
    );
    assert!(matches!(
        page.events[0].kind,
        crate::store::ParkEventKind::Parked { .. }
    ));
    assert_eq!(page.events[0].park_id, first.park_id);

    let second = store
        .record_turn_park(&park_write(
            &session_id,
            "turn-b",
            crate::store::ParkReason::RetiredGeneration {
                generation: Some(crate::ExecutableGeneration::new("blake3:old")),
                message: "cut over".to_string(),
            },
            300,
        ))
        .await
        .expect("a different turn parks");
    assert_ne!(
        second.park_id, first.park_id,
        "a superseding park mints a new identity"
    );
    assert_eq!(
        store
            .load_turn_park(&session_id)
            .await
            .expect("read the superseding park"),
        Some(second.clone()),
        "the session's park is the superseding turn's"
    );

    let page = factory
        .turn_park_feed(crate::store::ParkFeedCursor::initial(), limit(10))
        .await
        .expect("read the feed after supersession");
    assert_eq!(
        page.events.len(),
        3,
        "supersession closes one park and opens another"
    );
    assert_eq!(
        page.events[1].kind,
        crate::store::ParkEventKind::Unparked {
            cause: crate::store::UnparkCause::Superseded,
        },
        "the superseded park closes as superseded"
    );
    assert_eq!(page.events[1].target.turn_id, TurnId::from("turn-a"));
    assert_eq!(
        page.events[1].park_id, first.park_id,
        "the close event names the park it closed"
    );
    assert!(
        matches!(
            &page.events[2].kind,
            crate::store::ParkEventKind::Parked { reason } if reason.code()
                == crate::store::ParkReasonCode::RetiredGeneration
        ),
        "the new park opens with its reason: {:?}",
        page.events[2]
    );
    assert_eq!(page.events[2].target.turn_id, TurnId::from("turn-b"));
    assert_eq!(page.events[2].park_id, second.park_id);
    assert_eq!(
        page.events[2].seq,
        second.park_id.feed_sequence(),
        "a park's id is the feed sequence of the event that opened it"
    );
}

/// L3: every park transition writes exactly one feed event — the first park,
/// the parked turn's own commit, an input withdrawal that releases its held
/// work, a queued-run settlement, and the session's deletion — while a
/// different turn's commit writes none. Sequences are strictly increasing and
/// a mid-feed cursor resumes with exactly the suffix.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
pub async fn every_park_transition_writes_exactly_one_feed_event(
    factory: Arc<dyn crate::store::ConformanceSessionStoreFactory>,
) {
    // The parked turn's own commit settles the park.
    let commit_session = SessionId::from("park-feed-own-commit");
    let commit_store = create_bound_store(&factory, &commit_session).await;
    commit_store
        .record_turn_park(&park_write(
            &commit_session,
            "turn-1",
            divergence("one"),
            10,
        ))
        .await
        .expect("park turn-1");
    commit_turn(&commit_store, &commit_session, "turn-1", 0, "commit-owner")
        .await
        .expect("the parked turn's commit lands");

    // Another turn's commit leaves the park — and the feed — untouched.
    let other_session = SessionId::from("park-feed-other-commit");
    let other_store = create_bound_store(&factory, &other_session).await;
    other_store
        .record_turn_park(&park_write(&other_session, "turn-2", divergence("two"), 20))
        .await
        .expect("park turn-2");
    let feed_before = factory
        .turn_park_feed(crate::store::ParkFeedCursor::initial(), limit(100))
        .await
        .expect("read the feed before the other commit");
    commit_turn(&other_store, &other_session, "turn-other", 0, "other-owner")
        .await
        .expect("another turn's commit lands");
    let feed_after = factory
        .turn_park_feed(crate::store::ParkFeedCursor::initial(), limit(100))
        .await
        .expect("read the feed after the other commit");
    assert_eq!(
        feed_after.events.len(),
        feed_before.events.len(),
        "another turn's commit writes no park event"
    );
    assert!(
        other_store
            .load_turn_park(&other_session)
            .await
            .expect("read the surviving park")
            .is_some(),
        "another turn's commit leaves the park"
    );

    // A cancel that withdraws the parked turn's held input cancels the park.
    let withdraw_session = SessionId::from("park-feed-withdraw");
    let withdraw_store = create_bound_store(&factory, &withdraw_session).await;
    let input = withdraw_store
        .enqueue_pending_turn_input(crate::PendingTurnInputDraft::new(
            &withdraw_session,
            crate::TurnInputIngress::NextTurn,
            crate::TurnInput::text("the parked turn's input"),
        ))
        .await
        .expect("enqueue the held input");
    let lease = crate::testing::store_fixtures::claim_session_execution_lease_for_test(
        &withdraw_store,
        &withdraw_session,
        "withdraw-owner",
    )
    .await;
    let drive = withdraw_store
        .claim_next_turn_inputs(&withdraw_session, &lease.fence(), &lease.owner, 1)
        .await
        .expect("claim the drive")
        .expect("the input is claimable");
    withdraw_store
        .bind_turn_input_claim(&drive, &TurnId::from("turn-3"), &input.input_id)
        .await
        .expect("bind the held input to the parked turn");
    withdraw_store
        .record_turn_park(&park_write(
            &withdraw_session,
            "turn-3",
            divergence("three"),
            30,
        ))
        .await
        .expect("park turn-3");
    // A parked turn's claim is live only while the lease generation it pins
    // still holds the session; the turn aborted, so its lease releases and
    // the held input's cancel withdraws it.
    withdraw_store
        .release_session_execution_lease(&lease.completion())
        .await
        .expect("release the aborted turn's lease");
    let cancelled = withdraw_store
        .cancel_pending_turn_input(&withdraw_session, &input.input_id)
        .await
        .expect("cancel the held input");
    assert!(
        matches!(
            cancelled,
            crate::PendingTurnInputCancelOutcome::Cancelled(_)
        ),
        "the held input cancels: {cancelled:?}"
    );

    // A suffix cancel that reaches the parked turn's held input cancels the
    // park the same way — the suffix clear site is its own write path.
    let suffix_session = SessionId::from("park-feed-suffix");
    let suffix_store = create_bound_store(&factory, &suffix_session).await;
    let held = suffix_store
        .enqueue_pending_turn_input(crate::PendingTurnInputDraft::new(
            &suffix_session,
            crate::TurnInputIngress::NextTurn,
            crate::TurnInput::text("the parked turn's held input"),
        ))
        .await
        .expect("enqueue the held input");
    let suffix_lease = crate::testing::store_fixtures::claim_session_execution_lease_for_test(
        &suffix_store,
        &suffix_session,
        "suffix-owner",
    )
    .await;
    let suffix_drive = suffix_store
        .claim_next_turn_inputs(
            &suffix_session,
            &suffix_lease.fence(),
            &suffix_lease.owner,
            1,
        )
        .await
        .expect("claim the drive")
        .expect("the held input is claimable");
    suffix_store
        .bind_turn_input_claim(&suffix_drive, &TurnId::from("turn-3s"), &held.input_id)
        .await
        .expect("bind the held input to the parked turn");
    suffix_store
        .record_turn_park(&park_write(
            &suffix_session,
            "turn-3s",
            divergence("three-suffix"),
            35,
        ))
        .await
        .expect("park turn-3s");
    suffix_store
        .release_session_execution_lease(&suffix_lease.completion())
        .await
        .expect("release the aborted turn's lease");
    let suffix_outcome = suffix_store
        .cancel_pending_turn_input_suffix(
            &suffix_session,
            &crate::PendingTurnInputCancelTarget::input_id(&held.input_id),
        )
        .await
        .expect("cancel the suffix from the held input");
    assert!(
        matches!(
            suffix_outcome,
            crate::PendingTurnInputSuffixCancelOutcome::Outcomes { .. }
        ),
        "the suffix cancel runs: {suffix_outcome:?}"
    );

    // A queued-run settlement unparks the turn it held.
    let run_session = SessionId::from("park-feed-run-settled");
    let run_store = create_bound_store(&factory, &run_session).await;
    run_store
        .record_turn_park(&park_write(&run_session, "turn-4", divergence("four"), 40))
        .await
        .expect("park turn-4");
    let run_lease = crate::testing::store_fixtures::claim_session_execution_lease_for_test(
        &run_store,
        &run_session,
        "run-owner",
    )
    .await;
    let state = RuntimeSessionState {
        session_id: run_session.clone(),
        ..RuntimeSessionState::new(crate::SessionPolicy::new(crate::TurnBudget::Unbounded))
    };
    let admission = run_store
        .begin_or_resume_queued_run(
            &run_lease.authority(),
            crate::store::BeginQueuedRun {
                session_id: run_session.clone(),
                identity: Some(crate::ExecutionScope::queue_drain(
                    &run_session,
                    "park-feed-run",
                )),
                request: crate::store::QueuedRunRequest::Automatic,
                configuration: RuntimeCommit::persisted_state_for_test(&state, &[]).config,
                expected_head_revision: 0,
                initial_turn_index: 1,
                generation: None,
            },
        )
        .await
        .expect("admit the queued run");
    run_store
        .settle_queued_run(
            &run_lease.authority(),
            crate::store::QueuedRunCommit {
                scope: admission.scope.clone(),
                expected_revision: admission.revision,
                progress: crate::store::QueuedRunProgress::Settle {
                    terminal: crate::store::QueuedRunTerminal::Failed {
                        code: crate::RuntimeErrorCode::QueuedWork,
                        message: "host abandoned the submission".to_string(),
                    },
                },
            },
        )
        .await
        .expect("settle the queued run");

    // The session's deletion cancels its park; the ledger row survives it.
    let delete_session = SessionId::from("park-feed-deleted");
    let delete_store = create_bound_store(&factory, &delete_session).await;
    delete_store
        .record_turn_park(&park_write(
            &delete_session,
            "turn-5",
            divergence("five"),
            50,
        ))
        .await
        .expect("park turn-5");
    factory
        .delete_session(&delete_session)
        .await
        .expect("delete the parked session");

    let page = factory
        .turn_park_feed(crate::store::ParkFeedCursor::initial(), limit(100))
        .await
        .expect("read the whole feed");
    let kinds: Vec<&crate::store::ParkEventKind> =
        page.events.iter().map(|event| &event.kind).collect();
    use crate::store::ParkCancelCause as Cancel;
    use crate::store::ParkEventKind as Kind;
    use crate::store::UnparkCause as Unpark;
    let expected = [
        Kind::Parked {
            reason: divergence("one"),
        },
        Kind::Unparked {
            cause: Unpark::TurnCommitted,
        },
        Kind::Parked {
            reason: divergence("two"),
        },
        Kind::Parked {
            reason: divergence("three"),
        },
        Kind::Cancelled {
            cause: Cancel::InputWithdrawn,
        },
        Kind::Parked {
            reason: divergence("three-suffix"),
        },
        Kind::Cancelled {
            cause: Cancel::InputWithdrawn,
        },
        Kind::Parked {
            reason: divergence("four"),
        },
        Kind::Unparked {
            cause: Unpark::RunSettled,
        },
        Kind::Parked {
            reason: divergence("five"),
        },
        Kind::Cancelled {
            cause: Cancel::SessionDeleted,
        },
    ];
    assert_eq!(
        kinds,
        expected.iter().collect::<Vec<_>>(),
        "every park transition writes exactly one feed event, in commit order"
    );
    assert!(
        page.events.windows(2).all(|pair| pair[0].seq < pair[1].seq),
        "feed sequences are strictly increasing: {:?}",
        page.events
            .iter()
            .map(|event| event.seq)
            .collect::<Vec<_>>()
    );
    let named = |index: usize| {
        let event = &page.events[index];
        (
            event.target.session_id.clone(),
            event.target.turn_id.clone(),
        )
    };
    assert_eq!(named(0), (commit_session.clone(), TurnId::from("turn-1")));
    assert_eq!(named(1), (commit_session.clone(), TurnId::from("turn-1")));
    assert_eq!(named(4), (withdraw_session.clone(), TurnId::from("turn-3")));
    assert_eq!(named(6), (suffix_session.clone(), TurnId::from("turn-3s")));
    assert_eq!(named(8), (run_session.clone(), TurnId::from("turn-4")));
    assert_eq!(named(10), (delete_session.clone(), TurnId::from("turn-5")));

    // A resume from a mid-feed cursor returns exactly the suffix.
    let mid = crate::store::ParkFeedCursor::from_store_sequence(page.events[4].seq);
    let suffix = factory
        .turn_park_feed(mid, limit(100))
        .await
        .expect("resume from the middle of the feed");
    assert_eq!(
        suffix.events,
        page.events[5..],
        "a resumed cursor returns exactly the remaining events"
    );
    assert_eq!(
        suffix.next,
        crate::store::ParkFeedCursor::from_store_sequence(
            page.events.last().expect("the feed is not empty").seq
        ),
        "the resumed page advances the cursor to the last event"
    );
}

/// L4: a commit that rolls back — here a head-revision CAS conflict — leaves
/// both the park row and the feed untouched.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
pub async fn a_rolled_back_commit_leaves_park_and_feed_unchanged(
    factory: Arc<dyn crate::store::ConformanceSessionStoreFactory>,
) {
    let session_id = SessionId::from("park-feed-rolled-back");
    let store = create_bound_store(&factory, &session_id).await;
    let parked = store
        .record_turn_park(&park_write(&session_id, "turn-6", divergence("six"), 60))
        .await
        .expect("park turn-6");
    commit_turn(&store, &session_id, "turn-other", 0, "advancing-owner")
        .await
        .expect("an unrelated turn advances the head");
    let feed_before = factory
        .turn_park_feed(crate::store::ParkFeedCursor::initial(), limit(100))
        .await
        .expect("read the feed before the conflicted commit");

    let conflict = commit_turn(&store, &session_id, "turn-6", 0, "stale-owner").await;
    assert!(
        conflict.is_err(),
        "a stale head revision refuses the commit"
    );
    assert_eq!(
        store
            .load_turn_park(&session_id)
            .await
            .expect("read the park after the rollback"),
        Some(parked),
        "the rolled-back commit leaves the park row untouched"
    );
    let feed_after = factory
        .turn_park_feed(crate::store::ParkFeedCursor::initial(), limit(100))
        .await
        .expect("read the feed after the rollback");
    assert_eq!(
        feed_after.events, feed_before.events,
        "the rolled-back commit appends no feed event"
    );
}

/// L5: a cursor below the compaction horizon is refused with the typed
/// `ParkFeedCursorCompacted` error, while a cursor at the horizon still reads.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
pub async fn a_compacted_feed_cursor_is_refused_typed(
    factory: Arc<dyn crate::store::ConformanceSessionStoreFactory>,
) {
    let session_id = SessionId::from("park-feed-compacted");
    let store = create_bound_store(&factory, &session_id).await;
    let parked = store
        .record_turn_park(&park_write(&session_id, "turn-7", divergence("seven"), 70))
        .await
        .expect("park turn-7");
    let parked_seq = parked.park_id.feed_sequence();
    factory
        .compact_turn_park_feed(crate::store::ParkFeedCursor::from_store_sequence(
            parked_seq,
        ))
        .await
        .expect("compact the feed through the first event");

    let compacted = factory
        .turn_park_feed(crate::store::ParkFeedCursor::initial(), limit(10))
        .await;
    let Err(crate::StoreError::ParkFeedCursorCompacted { horizon }) = compacted else {
        panic!("a cursor below the horizon is refused typed: {compacted:?}");
    };
    assert_eq!(
        horizon,
        crate::store::ParkFeedCursor::from_store_sequence(parked_seq),
        "the error carries the compaction horizon"
    );

    let live = factory
        .turn_park_feed(
            crate::store::ParkFeedCursor::from_store_sequence(parked_seq),
            limit(10),
        )
        .await
        .expect("a cursor at the horizon still reads");
    assert!(live.events.is_empty(), "no event follows the horizon yet");

    let other_session = SessionId::from("park-feed-compacted-other");
    let other_store = create_bound_store(&factory, &other_session).await;
    other_store
        .record_turn_park(&park_write(
            &other_session,
            "turn-8",
            divergence("eight"),
            80,
        ))
        .await
        .expect("park another session's turn");
    let tail = factory
        .turn_park_feed(
            crate::store::ParkFeedCursor::from_store_sequence(parked_seq),
            limit(10),
        )
        .await
        .expect("read past the horizon");
    assert_eq!(
        tail.events.len(),
        1,
        "events past the horizon remain readable"
    );
    assert!(
        matches!(
            tail.events[0].kind,
            crate::store::ParkEventKind::Parked { .. }
        ) && tail.events[0].target.session_id == other_session,
        "the surviving event is the later park: {:?}",
        tail.events[0]
    );

    // Compacting through a sequence beyond the clock's own must not raise the
    // horizon past the last allocated event: the next park's event would land
    // at or below the horizon and be unreachable from every cursor. The
    // horizon clamps to the head instead, so the later event stays readable.
    let head = tail
        .events
        .last()
        .expect("the feed has the second park's event")
        .seq;
    factory
        .compact_turn_park_feed(crate::store::ParkFeedCursor::from_store_sequence(head + 10))
        .await
        .expect("compact through a cursor past the clock");
    // The clamped horizon is recoverable from the typed refusal.
    let Err(crate::StoreError::ParkFeedCursorCompacted {
        horizon: clamped_horizon,
    }) = factory
        .turn_park_feed(crate::store::ParkFeedCursor::initial(), limit(10))
        .await
    else {
        panic!("a cursor below the clamped horizon is refused typed");
    };
    assert_eq!(
        clamped_horizon,
        crate::store::ParkFeedCursor::from_store_sequence(head),
        "compaction clamps the horizon to the allocated sequence"
    );
    let third_session = SessionId::from("park-feed-compacted-third");
    let third_store = create_bound_store(&factory, &third_session).await;
    third_store
        .record_turn_park(&park_write(
            &third_session,
            "turn-9",
            divergence("nine"),
            90,
        ))
        .await
        .expect("park a third session's turn");
    let after_clamp = factory
        .turn_park_feed(clamped_horizon, limit(10))
        .await
        .expect("read from the clamped horizon");
    assert_eq!(
        after_clamp.events.len(),
        1,
        "the event appended after over-current compaction is still reachable"
    );
    assert!(
        matches!(
            after_clamp.events[0].kind,
            crate::store::ParkEventKind::Parked { .. }
        ) && after_clamp.events[0].target.session_id == third_session,
        "the reachable event is the newest park: {:?}",
        after_clamp.events[0]
    );
}

/// L6: `count_unsettled_turns` reports exactly what the list implies — the
/// parked count, the per-reason split, and the oldest park's `since_ms`.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
pub async fn summary_agrees_with_list(
    factory: Arc<dyn crate::store::ConformanceSessionStoreFactory>,
) {
    let empty = factory
        .count_unsettled_turns()
        .await
        .expect("count an empty deployment");
    assert_eq!(empty.parked_turns, 0);
    assert_eq!(empty.oldest_parked_since_ms, None);
    assert!(empty.parked_by_reason.is_empty());

    let session_a = SessionId::from("park-summary-a");
    let session_b = SessionId::from("park-summary-b");
    let session_c = SessionId::from("park-summary-c");
    let store_a = create_bound_store(&factory, &session_a).await;
    let store_b = create_bound_store(&factory, &session_b).await;
    let store_c = create_bound_store(&factory, &session_c).await;
    store_a
        .record_turn_park(&park_write(&session_a, "turn-a", drift("a"), 300))
        .await
        .expect("park a");
    store_b
        .record_turn_park(&park_write(&session_b, "turn-b", divergence("b"), 100))
        .await
        .expect("park b");
    store_c
        .record_turn_park(&park_write(&session_c, "turn-c", drift("c"), 200))
        .await
        .expect("park c");

    let listed = factory
        .list_turn_parks(&crate::store::TurnParkQuery {
            reasons: None,
            session: None,
            parked_at_or_before_ms: None,
            after: None,
            limit: limit(100),
        })
        .await
        .expect("list the parks");
    let summary = factory
        .count_unsettled_turns()
        .await
        .expect("count the deployment");
    assert_eq!(
        summary.parked_turns,
        listed.len(),
        "the parked count is the listed count"
    );
    let mut by_reason: std::collections::BTreeMap<crate::store::ParkReasonCode, usize> =
        std::collections::BTreeMap::new();
    for park in &listed {
        *by_reason.entry(park.reason.code()).or_default() += 1;
    }
    assert_eq!(
        summary.parked_by_reason, by_reason,
        "the per-reason split matches the listed records"
    );
    assert_eq!(
        summary.oldest_parked_since_ms,
        listed.iter().map(|park| park.since_ms).min(),
        "the oldest park is the list's first"
    );
    assert!(
        summary.in_flight_turns >= summary.parked_turns,
        "parked turns are in-flight turns too"
    );
}
