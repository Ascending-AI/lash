//! `Promise.race` and `Promise.any` on the product path (FIG-3397, ADR 0099
//! §0, §6, §7, §10, §11, §12).
//!
//! Each case runs one authored cell end to end on the journaled tier, with
//! leaves the *test* settles: a `hold` leaf blocks inside its attempt until
//! released, a `defer` leaf parks on a completion key the test resolves. A
//! race's winner is therefore decided by construction — the prelude trap is
//! avoided because no winner is released from inside a sibling attempt — and
//! the loser is still in flight when the aggregate resumes, which is what
//! "selection cancels nothing" and "opener close cancels an unfinished arm"
//! are about.
//!
//! The resume itself is observed through a leaf the cell calls *after* the
//! aggregate: that leaf starting while the loser has not settled is the
//! aggregate having resumed ahead of the loser — winner latency, observed as
//! order and never as wall time.

use super::*;

/// The events the intent target recorded for `event_type`, by leaf id.
async fn emitted_ids(run: &OracleRun, event_type: &str) -> Vec<String> {
    run.registry
        .recent_events(&lash_sansio::ProcessId::from(INTENT_PROCESS), 64)
        .await
        .expect("read the intent target's events")
        .into_iter()
        .filter(|event| event.event_type == event_type)
        .filter_map(|event| event.payload["id"].as_str().map(str::to_string))
        .collect()
}

/// Waits until the driven turn has ended, without consuming it: the turn's end
/// has then closed every group it held (ADR 0099 §7). The budget is a
/// deadlock bound, never an ordering device.
async fn await_turn_end(driven: &DrivenOracle) {
    let deadline = tokio::time::Instant::now() + RENDEZVOUS_BUDGET;
    while !driven.turn.is_finished() {
        assert!(
            tokio::time::Instant::now() < deadline,
            "the turn never ended"
        );
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    }
}

/// §11 clause 4: a timer admitted beside a held tool wins the race, and the
/// aggregate resumes while the tool is still inside its attempt.
///
/// The leaf after the race holds too. The timer can fire before the loser's
/// dispatch reaches the host, and a turn that ran on to `finish` would then
/// close the group and cancel the loser before it ever started; holding
/// `after` keeps the turn open until the loser has been observed inside its
/// attempt, so the case never depends on the loser beating the timer.
async fn a_race_resumes_on_its_timer_while_a_held_tool_runs(tier: &JournaledTier) -> Result<()> {
    let driven = drive_cells(
        tier,
        "aggregate-oracle-race-timeout",
        vec![typescript_block(
            r#"const winner = await Promise.race([
  oracle.step({ id: "slow", hold: true }),
  sleep(20)
]);
await oracle.step({ id: "after", hold: true });
finish({ timedOut: winner === undefined });"#,
        )],
    )
    .await?;

    driven.theatre.await_started("after").await;
    driven.theatre.await_started("slow").await;
    assert!(
        !driven.theatre.settled().contains(&"slow".to_string()),
        "{}: the race resumed on its timer while the held tool had not settled, saw {:?}",
        tier.name,
        driven.theatre.settled()
    );
    driven.theatre.release("slow");
    driven.theatre.release("after");
    let run = driven.finish().await?;
    assert_eq!(
        run.final_value(),
        &serde_json::json!({ "timedOut": true }),
        "{}: the timer's fulfilment value is undefined",
        tier.name
    );
    Ok(())
}

/// §7 and worked examples 3–4: `finish` right after a race. The loser that is
/// still parked when the turn ends is cancel-decided by the close — the turn
/// ends without it ever settling — while the loser whose final record
/// committed first keeps its authority and realizes its declared intent.
async fn finishing_right_after_a_race_cancels_the_unfinished_loser(
    tier: &JournaledTier,
) -> Result<()> {
    let driven = drive_cells(
        tier,
        "aggregate-oracle-race-finish",
        vec![typescript_block(
            r#"const winner = await Promise.race([
  oracle.step({ id: "parked", defer: true }),
  oracle.step({ id: "committed", hold: true, intent: true }),
  oracle.step({ id: "winner" })
]);
await oracle.step({ id: "gate", hold: true });
finish(winner);"#,
        )],
    )
    .await?;

    driven.theatre.await_started("parked").await;
    driven.theatre.await_started("gate").await;
    // The committed loser finishes while the opener is still live.
    driven.theatre.release("committed");
    driven.theatre.await_settled("committed").await;
    driven.theatre.release("gate");
    // Nothing resolves `parked`: the turn's end must cancel it for the turn to
    // finish at all.
    let run = driven.finish().await?;
    assert_eq!(
        run.final_value(),
        &serde_json::json!({ "id": "winner" }),
        "{}: the race resolves with the settlement that ranked first",
        tier.name
    );
    assert!(
        !run.theatre.settled().contains(&"parked".to_string()),
        "{}: the parked loser never settled",
        tier.name
    );
    assert_eq!(
        emitted_ids(&run, INTENT_EVENT).await,
        vec!["committed".to_string()],
        "{}: the committed loser's intent is realized before the turn ends",
        tier.name
    );
    Ok(())
}

/// §0 *live* and §10 L5: a plain value decides a race, yet every pending
/// operand is admitted first, and the losing tool runs on under the live
/// opener — its declared intent is realized before the turn ends.
async fn a_plain_value_decides_a_race_whose_loser_still_runs(tier: &JournaledTier) -> Result<()> {
    let driven = drive_cells(
        tier,
        "aggregate-oracle-race-plain",
        vec![typescript_block(
            r#"const winner = await Promise.race([
  oracle.step({ id: "loser", hold: true, intent: true }),
  7
]);
await oracle.step({ id: "gate", hold: true });
finish({ winner });"#,
        )],
    )
    .await?;

    // The loser was admitted and dispatched even though a plain value won,
    // and the aggregate resumed while it was still inside its attempt.
    driven.theatre.await_started("loser").await;
    driven.theatre.await_started("gate").await;
    driven.theatre.release("loser");
    driven.theatre.await_settled("loser").await;
    driven.theatre.release("gate");
    let run = driven.finish().await?;
    assert_eq!(run.final_value(), &serde_json::json!({ "winner": 7 }));
    assert_eq!(
        emitted_ids(&run, INTENT_EVENT).await,
        vec!["loser".to_string()],
        "{}: a loser that settles while the opener lives realizes its intent",
        tier.name
    );
    Ok(())
}

/// §11 clause 1: a literal array and an array held in a binding race the same
/// operands the same way.
async fn literal_and_bound_arrays_race_alike(tier: &JournaledTier) -> Result<()> {
    for (label, cell) in [
        (
            "a literal array",
            r#"finish(await Promise.race([
  oracle.step({ id: "held", hold: true }),
  oracle.step({ id: "quick" })
]));"#,
        ),
        (
            "an array bound to a name",
            r#"const leaves = [oracle.step({ id: "held", hold: true }), oracle.step({ id: "quick" })];
finish(await Promise.race(leaves));"#,
        ),
    ] {
        let driven = drive_cells(
            tier,
            "aggregate-oracle-race-arrays",
            vec![typescript_block(cell)],
        )
        .await?;
        driven.theatre.await_consumed(1).await;
        driven.theatre.release("held");
        let run = driven.finish().await?;
        assert_eq!(
            run.final_value(),
            &serde_json::json!({ "id": "quick" }),
            "{}/{label}: the race resolves with the leaf that settled",
            tier.name
        );
    }
    Ok(())
}

/// §10 L2 / L4 and §11 clause 8: an exhausted `any` rejects with an
/// `AggregateError` whose `errors` hold one rejection per input position, in
/// input order — a handle written twice is one execution and two errors.
async fn an_exhausted_any_reports_every_position_in_input_order(
    tier: &JournaledTier,
) -> Result<()> {
    let run = run_cell(
        tier,
        "aggregate-oracle-any-exhausted",
        r#"const repeated = oracle.step({ id: "twice", fail: true });
try {
  await Promise.any([repeated, oracle.step({ id: "once", fail: true }), repeated]);
  finish({ resolved: true });
} catch (error) {
  finish({
    name: error.name,
    reasons: error.errors.map((reason) => reason.message)
  });
}"#,
    )
    .await?;
    let value = run.final_value();
    assert_eq!(value["name"], serde_json::json!("AggregateError"));
    let reasons = value["reasons"]
        .as_array()
        .unwrap_or_else(|| panic!("the aggregate error carries its errors, got {value}"));
    assert_eq!(
        reasons.len(),
        3,
        "{}: one rejection per input position",
        tier.name
    );
    for (position, id) in ["twice", "once", "twice"].iter().enumerate() {
        assert!(
            reasons[position]
                .as_str()
                .is_some_and(|reason| reason.contains(&format!("step {id} rejected"))),
            "{}: position {position} holds {id}'s rejection, got {}",
            tier.name,
            reasons[position]
        );
    }
    let mut started = run.theatre.started();
    started.sort();
    assert_eq!(
        started,
        vec!["once".to_string(), "twice".to_string()],
        "{}: a handle written twice executes once",
        tier.name
    );
    Ok(())
}

/// §6 with §10: a leaf whose declared intent is refused settles rejected even
/// though its attempt returned a value, so it cannot win an `any`; the next
/// fulfilment does.
async fn an_intent_refusal_turns_a_would_be_any_winner_into_a_rejection(
    tier: &JournaledTier,
) -> Result<()> {
    let driven = drive_cells(
        tier,
        "aggregate-oracle-any-refused-intent",
        vec![typescript_block(
            r#"finish(await Promise.any([
  oracle.step({ id: "refused", refused_intent: true }),
  oracle.step({ id: "later", defer: true })
]));"#,
        )],
    )
    .await?;
    driven.theatre.await_consumed(1).await;
    driven
        .theatre
        .settle_deferred(
            &driven.core,
            "later",
            lash_core::Resolution::Ok(serde_json::json!({ "id": "later" })),
        )
        .await?;
    let run = driven.finish().await?;
    assert_eq!(
        run.final_value(),
        &serde_json::json!({ "id": "later" }),
        "{}: the refused leaf was a rejection, so the next fulfilment wins",
        tier.name
    );
    Ok(())
}

/// §4 and W17: a loser still inside its attempt when the turn ends is
/// cancel-decided by the close, and the turn ends without waiting for its
/// body. When that body finally returns — a stale writer behind a decision it
/// cannot see — it can deliver nothing: its declared intent is never realized
/// and the winner stays the answer.
async fn a_losers_final_after_the_close_is_refused(tier: &JournaledTier) -> Result<()> {
    let driven = drive_cells(
        tier,
        "aggregate-oracle-race-stale-writer",
        vec![typescript_block(
            r#"finish(await Promise.race([
  oracle.step({ id: "stale", hold: true, intent: true }),
  oracle.step({ id: "winner" })
]));"#,
        )],
    )
    .await?;
    driven.theatre.await_started("stale").await;
    driven.theatre.await_settled("winner").await;
    await_turn_end(&driven).await;
    driven.theatre.release("stale");
    let run = driven.finish().await?;
    assert_eq!(run.final_value(), &serde_json::json!({ "id": "winner" }));
    assert!(
        emitted_ids(&run, INTENT_EVENT).await.is_empty(),
        "{}: a final record behind the cancel decision realizes nothing",
        tier.name
    );
    Ok(())
}

/// §11 clause 5: `Promise.race([])` never settles, so the host ends the cell
/// with the typed unsettled-await error rather than parking it forever. It is
/// not catchable, so the cell's own `catch` never runs.
async fn racing_nothing_ends_the_cell_with_a_typed_host_error(tier: &JournaledTier) -> Result<()> {
    let run = run_cells(
        tier,
        "aggregate-oracle-race-empty",
        vec![
            typescript_block(
                r#"try {
  finish({ resolved: await Promise.race([]) });
} catch (error) {
  finish({ caught: error.message });
}"#,
            ),
            typescript_block(r#"finish("observed the host error");"#),
        ],
    )
    .await?;
    assert_eq!(
        run.final_value(),
        &serde_json::json!("observed the host error"),
        "{}: the unsettled await ended the cell uncaught",
        tier.name
    );
    assert!(
        run.requests
            .get(1)
            .is_some_and(|request| request.contains("aggregate_await_unsettled")),
        "{}: the model sees the typed host error: {:?}",
        tier.name,
        run.requests.get(1)
    );
    Ok(())
}

/// §11 clause 6: `Promise.any([])` rejects with an `AggregateError` whose
/// `errors` is empty, and needs no group.
async fn any_of_nothing_rejects_with_an_empty_aggregate_error(tier: &JournaledTier) -> Result<()> {
    let run = run_cell(
        tier,
        "aggregate-oracle-any-empty",
        r#"try {
  finish({ resolved: await Promise.any([]) });
} catch (error) {
  finish({ name: error.name, count: error.errors.length });
}"#,
    )
    .await?;
    assert_eq!(
        run.final_value(),
        &serde_json::json!({ "name": "AggregateError", "count": 0 })
    );
    Ok(())
}

/// §12 and worked example 7: a timer beats `processes.await` on a process that
/// is still running. The losing wait is released at the turn's end, and the
/// process is not cancelled by it.
async fn a_race_over_a_running_process_leaves_the_process_running(
    tier: &JournaledTier,
) -> Result<()> {
    let run = run_cell(
        tier,
        "aggregate-oracle-race-process",
        r#"const worker = async () => { await waitSignal("go"); return "done"; };
const job = await processes.start({ definition: worker });
const winner = await Promise.race([processes.await({ handle: job }), sleep(20)]);
finish({ timedOut: winner === undefined, job: job.process_id });"#,
    )
    .await?;
    let value = run.final_value();
    assert_eq!(value["timedOut"], serde_json::json!(true));
    let process_id = value["job"]
        .as_str()
        .unwrap_or_else(|| panic!("the cell reports the job, got {value}"));
    let record = run
        .registry
        .get_process(&lash_sansio::ProcessId::from(process_id))
        .await
        .expect("read the job")
        .expect("the job is registered");
    assert!(
        !record.status.is_terminal(),
        "{}: releasing the losing wait does not end the process, got {:?}",
        tier.name,
        record.status
    );
    Ok(())
}

/// W16: a session whose race still holds a live loser cannot be deleted, and
/// the refusal happens before anything is deleted — the session still opens.
async fn a_session_with_a_live_group_refuses_deletion_before_deleting_anything(
    tier: &JournaledTier,
) -> Result<()> {
    let session_id = "aggregate-oracle-delete-live";
    let driven = drive_cells(
        tier,
        session_id,
        vec![typescript_block(
            r#"finish(await Promise.all([
  oracle.step({ id: "held", hold: true }),
  oracle.step({ id: "quick" })
]));"#,
        )],
    )
    .await?;
    driven.theatre.await_started("held").await;
    let refused = delete_bound_session(&driven.core, session_id).await;
    let error = refused.expect_err("a live group pins its session");
    assert!(
        error.to_string().contains("effect_group_lifecycle_pinned")
            || error.to_string().contains("live or closing"),
        "{}: the refusal names the pinned group, got {error}",
        tier.name
    );
    driven.theatre.release("held");
    let catalog = Arc::clone(&driven.core.store_factory);
    let run = driven.finish().await?;
    assert_eq!(
        run.final_value(),
        &serde_json::json!([{ "id": "held" }, { "id": "quick" }]),
        "{}: the turn whose session deletion was refused commits normally",
        tier.name
    );
    let remaining = catalog
        .read_session(&SessionId::from(session_id))
        .await
        .expect("read the session catalog row");
    assert!(
        remaining.is_some(),
        "{}: nothing was deleted before the refusal",
        tier.name
    );
    Ok(())
}

/// §7, §10 L3: a turn cancelled while its `any` is parked on rank 2 — rank 1
/// consumed as a rejection — ends cancelled. The cancellation travels on the
/// host-control channel: the cell never sees a fabricated `AggregateError`, so
/// its `catch` never runs, and the children still pending are cancel-decided
/// by the close rather than left to settle under a turn that has ended.
async fn a_turn_cancelled_while_parked_on_rank_n_ends_cancelled(
    tier: &JournaledTier,
) -> Result<()> {
    let session_id = "aggregate-oracle-any-cancelled";
    let theatre = Arc::new(OracleTheatre::default());
    let backend = tier.backend().await;
    let registry: Arc<dyn ProcessRegistry> = backend.process_registry();
    register_intent_target(registry.as_ref(), session_id).await;
    let requests = Arc::new(StdMutex::new(Vec::<String>::new()));
    let core = oracle_core(
        backend,
        session_id,
        vec![typescript_block(
            r#"try {
  finish({ resolved: await Promise.any([
    oracle.step({ id: "rejected", defer: true }),
    oracle.step({ id: "parked", defer: true }),
    oracle.step({ id: "held", hold: true, intent: true })
  ]) });
} catch (error) {
  finish({ caught: error.name });
}"#,
        )],
        Arc::clone(&theatre),
        Arc::clone(&requests),
    )?;
    let session = core.session(session_id).open().await?;
    let cancel = CancellationToken::new();
    let streamed = Arc::clone(&theatre);
    let turn_cancel = cancel.clone();
    let turn = tokio::spawn(async move {
        session
            .turn(TurnInput::text("settle the aggregate"))
            .cancel(turn_cancel)
            .stream_to(streamed.as_ref())
            .await
    });

    theatre.await_started("held").await;
    theatre
        .settle_deferred(&core, "rejected", OracleTheatre::rejection("rejected"))
        .await?;
    // Rank 1 is consumed; the aggregate is parked on rank 2.
    theatre.await_consumed(1).await;
    cancel.cancel();
    let report = tokio::time::timeout(RENDEZVOUS_BUDGET, turn)
        .await
        .expect("the cancelled turn reports")
        .expect("turn task")?;
    theatre.release("held");

    assert!(
        matches!(
            report.outcome,
            TurnOutcome::Stopped(lash_core::facade_support::TurnStop::Cancelled { .. })
        ),
        "{}: the turn ends cancelled, got {:?}",
        tier.name,
        report.outcome
    );
    assert!(
        report.final_value().is_none(),
        "{}: no fabricated AggregateError reached the cell's catch, got {:?}",
        tier.name,
        report.final_value()
    );
    let settled = theatre.settled();
    assert!(
        !settled.contains(&"parked".to_string()),
        "{}: the parked child never settled after the cancel, saw {settled:?}",
        tier.name
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn sqlite_a_race_resumes_on_its_timer_while_a_held_tool_runs() -> Result<()> {
    a_race_resumes_on_its_timer_while_a_held_tool_runs(&sqlite()).await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn sqlite_finishing_right_after_a_race_cancels_the_unfinished_loser() -> Result<()> {
    finishing_right_after_a_race_cancels_the_unfinished_loser(&sqlite()).await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn sqlite_a_plain_value_decides_a_race_whose_loser_still_runs() -> Result<()> {
    a_plain_value_decides_a_race_whose_loser_still_runs(&sqlite()).await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn sqlite_literal_and_bound_arrays_race_alike() -> Result<()> {
    literal_and_bound_arrays_race_alike(&sqlite()).await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn sqlite_an_exhausted_any_reports_every_position_in_input_order() -> Result<()> {
    an_exhausted_any_reports_every_position_in_input_order(&sqlite()).await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn sqlite_an_intent_refusal_turns_a_would_be_any_winner_into_a_rejection() -> Result<()> {
    an_intent_refusal_turns_a_would_be_any_winner_into_a_rejection(&sqlite()).await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn sqlite_a_losers_final_after_the_close_is_refused() -> Result<()> {
    a_losers_final_after_the_close_is_refused(&sqlite()).await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn sqlite_racing_nothing_ends_the_cell_with_a_typed_host_error() -> Result<()> {
    racing_nothing_ends_the_cell_with_a_typed_host_error(&sqlite()).await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn sqlite_any_of_nothing_rejects_with_an_empty_aggregate_error() -> Result<()> {
    any_of_nothing_rejects_with_an_empty_aggregate_error(&sqlite()).await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn sqlite_a_race_over_a_running_process_leaves_the_process_running() -> Result<()> {
    a_race_over_a_running_process_leaves_the_process_running(&sqlite()).await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn sqlite_a_session_with_a_live_group_refuses_deletion_before_deleting_anything() -> Result<()>
{
    a_session_with_a_live_group_refuses_deletion_before_deleting_anything(&sqlite()).await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn sqlite_a_turn_cancelled_while_parked_on_rank_n_ends_cancelled() -> Result<()> {
    a_turn_cancelled_while_parked_on_rank_n_ends_cancelled(&sqlite()).await
}
