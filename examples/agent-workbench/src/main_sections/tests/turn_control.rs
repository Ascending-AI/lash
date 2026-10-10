use super::*;

#[test]
fn turn_cancel_query_maps_stop_and_abort_onto_lash_modes() {
    let parse = |query: &str| {
        let uri: axum::http::Uri = format!("/api/turn/cancel?{query}")
            .parse()
            .expect("cancel route uri");
        axum::extract::Query::<TurnCancelQuery>::try_from_uri(&uri)
            .expect("cancel query parses")
            .0
    };
    let stop = parse("session_id=s-1&mode=stop");
    assert_eq!(stop.session.session_id.as_deref(), Some("s-1"));
    assert_eq!(stop.mode, WorkbenchTurnCancelMode::Stop);
    assert_eq!(stop.mode.lash_mode(), lash::TurnCancelMode::AfterStep);
    let abort = parse("session_id=s-1&mode=abort");
    assert_eq!(abort.mode, WorkbenchTurnCancelMode::Abort);
    assert_eq!(abort.mode.lash_mode(), lash::TurnCancelMode::Immediate);
    let legacy = parse("session_id=s-1");
    assert_eq!(
        legacy.mode,
        WorkbenchTurnCancelMode::Abort,
        "an unqualified Stop control keeps today's immediate abort"
    );
    assert!(ui::INDEX_HTML.contains("id=\"abort\""));
    assert!(ui::INDEX_HTML.contains("stop after step"));
    assert!(ui::INDEX_HTML.contains("\"/api/turn/cancel?mode=\" + mode"));
    assert!(ui::INDEX_HTML.contains("stopTurn(\"stop\")"));
    assert!(ui::INDEX_HTML.contains("stopTurn(\"abort\")"));
    assert!(ui::INDEX_HTML.contains("STOP_ESCALATION_MS"));
    assert!(ui::INDEX_HTML.contains("escalated"));
}

use super::tests::{
    GatedProvider, RecordingTrace, Workbench, send_text, silent_provider, started_turn_id,
    wait_for_turn_released,
};
use lash::SessionId;
use lash::TurnId;

/// The attach deadline a law hands the cancel route when the turn cannot end
/// before it: long enough for the cancellation to be recorded first, short
/// enough to keep the law fast.
const EXPIRING_ATTACH_TIMEOUT: Duration = Duration::from_millis(300);

fn cancel_query(session_id: &SessionId, mode: WorkbenchTurnCancelMode) -> TurnCancelQuery {
    TurnCancelQuery {
        session: SessionQuery {
            session_id: Some(session_id.clone()),
        },
        mode,
    }
}

/// Send one turn through the chat route and wait for its provider call.
async fn running_turn(state: &AppState, gate: &mut GatedProvider) -> TurnId {
    let accepted = send_text(state, None, "hold the admitted turn")
        .await
        .expect("the send is admitted");
    let turn_id = started_turn_id(&accepted);
    gate.next_call().await;
    turn_id
}

fn done_items(events: &mut tokio::sync::broadcast::Receiver<ProductEvent>) -> usize {
    let mut done = 0;
    while let Ok(event) = events.try_recv() {
        if matches!(event.item, StreamItem::Done { .. }) {
            done += 1;
        }
    }
    done
}

/// The input route records the exact ingress it was asked for: an
/// active-turn input targets the running turn at its next work boundary and
/// a next-turn input is deferred. Both are durably pending in order behind
/// the running turn's own input, the snapshot shows them, the turn's host
/// settlement withdraws only its active-turn input, and an active-turn input
/// whose turn settled between the route's check and its admission is
/// withdrawn and refused.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn turn_input_route_records_exact_active_and_next_turn_ingress() {
    let mut gate = GatedProvider::new();
    let workbench = Workbench::builder(gate.provider.clone()).build().await;
    let state = &workbench.state;
    let session_id = state.current_session_id();
    let no_active = enqueue_turn_input(
        State(state.clone()),
        Query(SessionQuery::default()),
        Json(TurnInputRequest {
            text: "too early".to_string(),
            ingress: TurnInputIngressRequest::ActiveTurn,
        }),
    )
    .await
    .expect_err("active-turn ingress without a running turn must fail");
    assert_eq!(no_active.status, StatusCode::CONFLICT);

    let turn_id = running_turn(state, &mut gate).await;
    let session = state
        .open_session(&session_id, "test")
        .await
        .expect("open the session");
    assert_eq!(
        session
            .durable()
            .unfinished_run()
            .await
            .expect("read durable turn admission")
            .expect("the turn is durably running")
            .run,
        turn_id
    );
    let running_input = session
        .durable()
        .pending_turn_inputs()
        .await
        .expect("list pending inputs")
        .first()
        .expect("the running turn's input")
        .input
        .input_id
        .to_string();

    let Json(injected) = enqueue_turn_input(
        State(state.clone()),
        Query(SessionQuery::default()),
        Json(TurnInputRequest {
            text: "inject exactly once".to_string(),
            ingress: TurnInputIngressRequest::ActiveTurn,
        }),
    )
    .await
    .expect("enqueue active-turn input");
    assert!(matches!(
        injected.ingress,
        lash::persistence::TurnInputIngress::ActiveTurn {
            turn_id: ref injected_into,
            min_boundary: lash::persistence::TurnInputCheckpointBoundary::AfterWork,
        } if *injected_into == turn_id
    ));
    assert_eq!(
        injected.state.kind(),
        lash::persistence::TurnInputStateKind::PendingActive
    );
    let Json(queued) = enqueue_turn_input(
        State(state.clone()),
        Query(SessionQuery::default()),
        Json(TurnInputRequest {
            text: "run after settle".to_string(),
            ingress: TurnInputIngressRequest::NextTurn,
        }),
    )
    .await
    .expect("enqueue next-turn input");
    assert!(matches!(
        queued.ingress,
        lash::persistence::TurnInputIngress::NextTurn
    ));
    assert_eq!(
        queued.state,
        lash::persistence::TurnInputState::DeferredNextTurn
    );

    let pending = session
        .durable()
        .pending_turn_inputs()
        .await
        .expect("list pending inputs");
    assert_eq!(
        pending
            .iter()
            .map(|read| read.input.input_id.to_string())
            .collect::<Vec<_>>(),
        vec![
            running_input.clone(),
            injected.input_id.clone(),
            queued.input_id.clone()
        ]
    );
    assert!(matches!(
        &pending[0].status,
        lash::PendingTurnInputReadStatus::Admitted { run } if *run == turn_id
    ));
    let snapshot = super::tests::read_state(state, None)
        .await
        .expect("load state snapshot");
    assert_eq!(
        snapshot
            .pending_turn_inputs
            .iter()
            .map(|read| read.input.input_id.to_string())
            .collect::<Vec<_>>(),
        vec![
            running_input.clone(),
            injected.input_id.clone(),
            queued.input_id.clone()
        ]
    );

    crate::turns::settle_workbench_turn(state, &session_id, &turn_id)
        .await
        .expect("settle the running turn");
    let after_settle = session
        .durable()
        .pending_turn_inputs()
        .await
        .expect("list pending inputs after turn settle");
    assert_eq!(
        after_settle
            .iter()
            .map(|read| read.input.input_id.to_string())
            .collect::<Vec<_>>(),
        vec![running_input.clone(), queued.input_id.clone()],
        "the host settlement withdraws only the settled turn's active-turn input"
    );

    // The route checked the turn, then it settled before the input landed.
    let raced = session
        .durable()
        .send(lash::TurnInput::text("must not be stranded"))
        .ingress(lash::persistence::TurnInputIngress::active_turn(
            &turn_id,
            lash::persistence::TurnInputCheckpointBoundary::AfterWork,
        ))
        .await
        .expect("send after the checked turn settled")
        .receipt()
        .clone();
    let race_error = reject_if_active_turn_settled(state, &raced)
        .await
        .expect_err("settled active-turn input must be rejected");
    assert_eq!(race_error.status, StatusCode::CONFLICT);
    let after_race = session
        .durable()
        .pending_turn_inputs()
        .await
        .expect("list pending inputs after settle race");
    assert_eq!(
        after_race
            .iter()
            .map(|read| read.input.input_id.to_string())
            .collect::<Vec<_>>(),
        vec![running_input, queued.input_id.clone()]
    );
    gate.release(2);
    drop(session);
    workbench.shutdown().await;
}

/// A Stop on a routed turn that no run ever opened returns at once, prunes
/// the route (also from the persisted routing), and publishes no terminal:
/// pruning a route is not terminal evidence.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn dangling_routed_turn_does_not_hang_stop_and_is_pruned() {
    let data_dir = tempfile::tempdir().expect("temp workbench dir");
    let active_turns_path = data_dir.path().join("active-turns.json");
    let workbench = Workbench::builder(silent_provider())
        .active_turns(ActiveTurns::persistent(active_turns_path.clone()).expect("active turns"))
        .build()
        .await;
    let state = &workbench.state;
    let session_id = state.current_session_id();
    let mut events = state.event_tx.subscribe(&session_id);
    state.track_turn(&session_id, &TurnId::from("dangling-turn"));

    let driver = state.core.turn_work_driver();
    let receipts = tokio::time::timeout(
        Duration::from_secs(5),
        state.cancel_turns_for_session_with_driver(
            &session_id,
            &driver,
            WorkbenchTurnCancelMode::Abort,
            EXPIRING_ATTACH_TIMEOUT,
        ),
    )
    .await
    .expect("Stop must not hang on a dangling routed turn")
    .expect("cancel dangling turn");

    assert!(
        matches!(
            receipts.as_slice(),
            [TurnCancelReceipt::UnknownOrRevoked { address }]
                if address.turn_id == "dangling-turn"
        ),
        "{receipts:?}"
    );
    assert!(state.active_turns.for_session(&session_id).is_none());
    assert_eq!(
        done_items(&mut events),
        0,
        "pruning a route is not terminal evidence"
    );
    let recovered = ActiveTurns::persistent(active_turns_path).expect("reopen active turns");
    assert!(recovered.for_session(&session_id).is_none());
    workbench.shutdown().await;
}

/// A cancellation recorded on a running turn whose terminal does not attach
/// within the route's deadline answers `202` with the pending receipt, and
/// the turn stays routed, in memory and on disk, with no terminal published:
/// the turn may still commit its own terminal, and its follower releases the
/// route when it does.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_pending_cancel_terminal_retains_the_turns_routing() {
    let data_dir = tempfile::tempdir().expect("temp workbench dir");
    let active_turns_path = data_dir.path().join("active-turns.json");
    let mut gate = GatedProvider::new();
    let workbench = Workbench::builder(gate.provider.clone())
        .active_turns(ActiveTurns::persistent(active_turns_path.clone()).expect("active turns"))
        .build()
        .await;
    let state = &workbench.state;
    let session_id = state.current_session_id();
    let turn_id = running_turn(state, &mut gate).await;
    let mut events = state.event_tx.subscribe(&session_id);

    let response = cancel_turn_with_driver(
        state.clone(),
        cancel_query(&session_id, WorkbenchTurnCancelMode::Stop),
        &state.core.turn_work_driver(),
        EXPIRING_ATTACH_TIMEOUT,
    )
    .await
    .expect("cancel the live turn")
    .into_response();
    assert_eq!(response.status(), StatusCode::ACCEPTED);
    let body = axum::body::to_bytes(response.into_body(), 64 * 1024)
        .await
        .expect("read pending Stop receipt");
    let body = serde_json::from_slice::<Value>(&body).expect("decode pending Stop receipt");
    let cancellation = &body["cancellations"][0];
    assert_eq!(
        cancellation["status"],
        "cancellation_recorded_terminal_pending"
    );
    assert_eq!(cancellation["address"]["session_id"], session_id.as_str());
    assert_eq!(cancellation["address"]["turn_id"], turn_id.as_str());
    assert_eq!(cancellation["cancellation"]["outcome"], "requested");
    assert!(
        cancellation["cancellation"]["cancellation"]["request_id"]
            .as_str()
            .is_some_and(|request_id| request_id.starts_with("workbench-stop-"))
    );
    assert_eq!(
        cancellation["cancellation"]["cancellation"]["origin"],
        "user"
    );
    assert_eq!(
        cancellation["cancellation"]["cancellation"]["reason"],
        "workbench Stop control"
    );
    assert!(cancellation.get("terminal").is_none());
    let routed = Some(lash::TurnAddress::new(&session_id, &turn_id));
    assert_eq!(
        state
            .active_turns
            .for_session(&session_id)
            .map(|active_turn| active_turn.address),
        routed,
        "a running turn stays routable while its cancellation is pending"
    );
    let recovered = ActiveTurns::persistent(active_turns_path).expect("reopen active turns");
    assert_eq!(
        recovered
            .for_session(&session_id)
            .map(|active_turn| active_turn.address),
        routed
    );
    assert_eq!(
        done_items(&mut events),
        0,
        "a still-active turn must not receive a terminal Done item"
    );
    gate.release(1);
    wait_for_turn_released(state, &session_id, &turn_id).await;
    workbench.shutdown().await;
}

/// A turn awaiting a real process it started is stopped by the Abort
/// control: the turn commits its cancelled terminal, the route attaches it,
/// and the awaited process settles as cancelled after it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn stop_over_real_process_await_commits_cancelled_terminal() {
    let workbench = Workbench::replying(
        r#"<typescript>
const hold_for_stop = async () => {
  await sleep(600000);
  return "unreachable";
};
const handle = await processes.start({ definition: hold_for_stop });
await chat.reply(String(await handle));
</typescript>"#,
    )
    .await;
    let state = &workbench.state;
    let session_id = state.current_session_id();
    let accepted = send_text(state, None, "start and await the held process")
        .await
        .expect("send the process-await turn");
    let turn_id = started_turn_id(&accepted);
    let registry = workbench.stores.process_registry();
    let process_id = tokio::time::timeout(Duration::from_secs(30), async {
        loop {
            let live = registry
                .list_non_terminal_processes_page(
                    std::num::NonZeroUsize::new(16).expect("non-zero test page size"),
                    None,
                )
                .await
                .expect("list live process while turn awaits")
                .records;
            if let [process] = live.as_slice() {
                break process.id.clone();
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("turn must reach a real process await");

    let (status, Json(receipt)) = Box::pin(cancel_turn(
        State(state.clone()),
        Query(cancel_query(&session_id, WorkbenchTurnCancelMode::Abort)),
    ))
    .await
    .expect("production Stop handler must cancel the process-await turn");
    assert_eq!(status, StatusCode::OK);
    assert!(receipt.accepted);
    assert!(
        matches!(
            receipt.cancellations.as_slice(),
            [TurnCancelReceipt::TerminalAttached {
                terminal: lash::TurnTerminal::Committed {
                    stop: Some(lash::TurnStop::Cancelled { .. }),
                },
                ..
            }]
        ),
        "{receipt:?}"
    );
    let process_outcome = tokio::time::timeout(
        Duration::from_secs(30),
        state.core.processes().await_output(&process_id),
    )
    .await
    .expect("the awaited process settles after Stop")
    .expect("process outcome after Stop");
    assert_eq!(
        process_outcome.terminal_status(),
        Some(lash::process::TerminalProcessStatus::Cancelled),
        "{process_outcome:?}"
    );
    wait_for_turn_released(state, &session_id, &turn_id).await;
    assert!(state.active_turns.for_session(&session_id).is_none());
    workbench.shutdown().await;
}

/// Two Stops racing on one turn record one request: one answer says
/// requested and the other already requested, both name the winning request
/// in the receipt and the trace, and the turn publishes one terminal.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_stops_publish_one_done_and_trace_winning_request() {
    let mut gate = GatedProvider::new();
    let trace = Arc::new(RecordingTrace::default());
    let workbench = Workbench::builder(gate.provider.clone())
        .trace_sink(Arc::clone(&trace) as Arc<dyn TraceSink>)
        .build()
        .await;
    let state = &workbench.state;
    let session_id = state.current_session_id();
    let turn_id = running_turn(state, &mut gate).await;
    let mut events = state.event_tx.subscribe(&session_id);
    let driver = state.core.turn_work_driver();
    let cancel = || {
        cancel_turn_with_driver(
            state.clone(),
            cancel_query(&session_id, WorkbenchTurnCancelMode::Stop),
            &driver,
            EXPIRING_ATTACH_TIMEOUT,
        )
    };
    let (first, second) = tokio::join!(cancel(), cancel());
    let responses = [
        first.expect("first stop").1.0,
        second.expect("second stop").1.0,
    ];
    let cancellations = responses
        .iter()
        .map(|response| match response.cancellations.as_slice() {
            [TurnCancelReceipt::CancellationRecordedTerminalPending { cancellation, .. }] => {
                cancellation
            }
            other => panic!("expected a recorded cancellation: {other:?}"),
        })
        .collect::<Vec<_>>();
    assert_eq!(
        cancellations
            .iter()
            .filter(|c| matches!(c, RecordedTurnCancellation::Requested(_)))
            .count(),
        1
    );
    assert_eq!(
        cancellations
            .iter()
            .filter(|c| matches!(c, RecordedTurnCancellation::AlreadyRequested(_)))
            .count(),
        1
    );
    let winner = cancellations[0].evidence().request_id.clone();
    assert_eq!(winner, cancellations[1].evidence().request_id);
    let traces = trace.custom("turn.cancel_requested");
    assert_eq!(traces.len(), 2);
    for (_, payload) in traces {
        assert_eq!(
            payload["request_id"].as_str(),
            Some(winner.as_str()),
            "both traces must identify the winning request"
        );
    }
    gate.release(1);
    wait_for_turn_released(state, &session_id, &turn_id).await;
    assert_eq!(
        done_items(&mut events),
        1,
        "concurrent stops must publish one live terminal"
    );
    workbench.shutdown().await;
}

/// The Stop control asks for an after-step cancellation and the turn's
/// terminal records it; an Abort on a turn already holding an after-step
/// request escalates it to immediate.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn stop_control_requests_after_step_and_abort_escalates_the_durable_record() {
    // The first step runs a cell that does not finish, so the turn has a
    // step boundary to stop at; every later turn finishes.
    let mut gate = GatedProvider::replying(|call| match call {
        0 => "<typescript>\nconst step = 1;\n</typescript>".to_string(),
        call => super::tests::finish_cell(&format!("answer {call}")),
    });
    let workbench = Workbench::builder(gate.provider.clone()).build().await;
    let state = &workbench.state;
    let session_id = state.current_session_id();
    let driver = state.core.turn_work_driver();

    let stopped_turn = running_turn(state, &mut gate).await;
    let (_, Json(stopped)) = cancel_turn_with_driver(
        state.clone(),
        cancel_query(&session_id, WorkbenchTurnCancelMode::Stop),
        &driver,
        EXPIRING_ATTACH_TIMEOUT,
    )
    .await
    .expect("stop route");
    match stopped.cancellations.as_slice() {
        [
            TurnCancelReceipt::CancellationRecordedTerminalPending {
                cancellation: RecordedTurnCancellation::Requested(evidence),
                ..
            },
        ] => {
            assert_eq!(evidence.origin.as_deref(), Some("user"));
            assert_eq!(evidence.reason.as_deref(), Some("workbench Stop control"));
            assert_eq!(evidence.mode, lash::TurnCancelMode::AfterStep);
        }
        other => panic!("stop route must record an after-step cancellation: {other:?}"),
    }
    gate.release(1);
    let terminal = tokio::time::timeout(
        Duration::from_secs(30),
        driver.await_terminal(&lash::TurnAddress::new(&session_id, &stopped_turn)),
    )
    .await
    .expect("the stopped turn ends")
    .expect("the stopped turn's terminal");
    assert!(
        matches!(
            &terminal,
            lash::TurnTerminal::Committed {
                stop: Some(lash::TurnStop::Cancelled { evidence }),
            } if evidence.mode == lash::TurnCancelMode::AfterStep
                && evidence.reason.as_deref() == Some("workbench Stop control")
        ),
        "the terminal records the after-step stop: {terminal:?}"
    );
    wait_for_turn_released(state, &session_id, &stopped_turn).await;

    // Escalate: a routed turn already holding an after-step request is
    // upgraded by the Abort control.
    let escalated_turn = running_turn(state, &mut gate).await;
    let seeded = driver
        .request_cancel(
            lash::TurnCancelRequest::new(
                lash::TurnAddress::new(&session_id, &escalated_turn),
                "host-stop",
                Some("user".to_string()),
            )
            .mode(lash::TurnCancelMode::AfterStep),
        )
        .await
        .expect("seed the after-step request");
    assert!(matches!(
        seeded.outcome,
        lash::TurnCancelOutcome::Requested(_)
    ));
    let receipts = state
        .cancel_turns_for_session_with_driver(
            &session_id,
            &driver,
            WorkbenchTurnCancelMode::Abort,
            Duration::from_secs(30),
        )
        .await
        .expect("escalate routed turn");
    match receipts.as_slice() {
        [
            TurnCancelReceipt::TerminalAttached {
                cancellation: RecordedTurnCancellation::Escalated(evidence),
                terminal:
                    lash::TurnTerminal::Committed {
                        stop:
                            Some(lash::TurnStop::Cancelled {
                                evidence: committed,
                            }),
                    },
                ..
            },
        ] => {
            assert_eq!(evidence.mode, lash::TurnCancelMode::Immediate);
            assert_eq!(evidence.reason.as_deref(), Some("workbench Abort control"));
            assert_eq!(committed.mode, lash::TurnCancelMode::Immediate);
        }
        other => panic!("Abort after Stop must report an escalation: {other:?}"),
    }
    gate.release(1);
    workbench.shutdown().await;
}

/// Register a process `turn_id` started, `Detached` from it: the turn's own
/// end leaves it alone, so only the turn control's cancel reaches it.
async fn register_turn_child(
    registry: &Arc<dyn lash::persistence::ProcessRegistry>,
    session_id: &SessionId,
    turn_id: &TurnId,
) -> lash::ProcessId {
    let mut registration = lash::testing::held_engine_registration(
        json!({ "awaited": true }),
        lash::process::ProcessProvenance::session(lash::process::SessionScope::new(
            session_id.clone(),
        )),
        lash::process::Lifetime::Detached,
    );
    registration.ancestry = lash::process::Ancestry::from_scopes([lash::process::ScopeId::turn(
        session_id.clone(),
        turn_id.clone(),
    )]);
    registry
        .register_process(registration)
        .await
        .expect("register the awaited process")
        .id
}

/// FIG-3155: an API turn cancellation over a foreground process await used to
/// commit the turn terminal and leave the process running. Both modes request
/// the cancellation of the processes the turn started, and of no other
/// turn's, from the turn-cancel path itself.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn both_cancel_modes_request_cancellation_of_the_turns_awaited_process() {
    let mut gate = GatedProvider::new();
    let trace = Arc::new(RecordingTrace::default());
    let workbench = Workbench::builder(gate.provider.clone())
        .trace_sink(Arc::clone(&trace) as Arc<dyn TraceSink>)
        .build()
        .await;
    let state = &workbench.state;
    let session_id = state.current_session_id();
    let registry = workbench.stores.process_registry();
    let other_turn = TurnId::from("other-turn");
    for mode in [
        WorkbenchTurnCancelMode::Stop,
        WorkbenchTurnCancelMode::Abort,
    ] {
        let turn_id = running_turn(state, &mut gate).await;
        let child = register_turn_child(&registry, &session_id, &turn_id).await;
        let foreign = register_turn_child(&registry, &session_id, &other_turn).await;
        let before = trace.custom("process.cancel_requested").len();

        let (_, Json(cancelled)) = cancel_turn_with_driver(
            state.clone(),
            cancel_query(&session_id, mode),
            &state.core.turn_work_driver(),
            EXPIRING_ATTACH_TIMEOUT,
        )
        .await
        .expect("cancel the turn awaiting a process");
        assert!(cancelled.accepted, "the {mode:?} cancel names the turn");

        let requested = trace.custom("process.cancel_requested");
        let cancelled = requested[before..]
            .iter()
            .map(|(session, payload)| {
                (
                    session.clone(),
                    payload["process_id"].as_str().map(str::to_string),
                )
            })
            .collect::<Vec<_>>();
        assert_eq!(
            cancelled,
            vec![(Some(session_id.clone()), Some(child.to_string()))],
            "{mode:?} must cancel the process its own turn started, and only it (not {foreign})"
        );
        gate.release(1);
        wait_for_turn_released(state, &session_id, &turn_id).await;
    }
    workbench.shutdown().await;
}

/// FIG-3018: a cancel whose terminal could not attach keeps the turn routed
/// on purpose; once the session's delete settles as a tombstone, the route
/// and its prompt go with it, in memory and on disk, so the next boot does
/// not resurrect a route for an id every surface refuses.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_confirmed_tombstone_retires_the_route_a_cancel_had_to_keep() {
    let data_dir = tempfile::tempdir().expect("temp workbench dir");
    let active_turns_path = data_dir.path().join("active-turns.json");
    let mut gate = GatedProvider::new();
    let workbench = Workbench::builder(gate.provider.clone())
        .active_turns(ActiveTurns::persistent(active_turns_path.clone()).expect("active turns"))
        .build()
        .await;
    let state = &workbench.state;
    let session_id = state.current_session_id();
    let turn_id = running_turn(state, &mut gate).await;

    let receipts = state
        .cancel_turns_for_session_with_driver(
            &session_id,
            &state.core.turn_work_driver(),
            WorkbenchTurnCancelMode::Stop,
            EXPIRING_ATTACH_TIMEOUT,
        )
        .await
        .expect("cancel the routed turn");
    assert!(
        matches!(
            receipts.as_slice(),
            [TurnCancelReceipt::CancellationRecordedTerminalPending { .. }]
        ),
        "the held turn leaves its terminal pending: {receipts:?}"
    );
    assert_eq!(
        state
            .active_turns
            .for_session(&session_id)
            .map(|active_turn| active_turn.address),
        Some(lash::TurnAddress::new(&session_id, &turn_id)),
        "a pending terminal keeps its route"
    );
    assert!(
        state
            .active_turns
            .prompt_for(&session_id, &turn_id)
            .is_some()
    );

    // The delete settles: the durable tombstone is a fact.
    state.settle_retirement_mark(&session_id, &Ok(())).await;

    assert_eq!(
        state.active_turns.retirement(&session_id),
        Some(SessionRetirement::Retired)
    );
    assert!(
        state.active_turns.for_session(&session_id).is_none(),
        "a tombstoned session keeps no routes"
    );
    assert!(
        state
            .active_turns
            .prompt_for(&session_id, &turn_id)
            .is_none(),
        "the route's prompt goes with it"
    );
    let persisted: Value = serde_json::from_slice(
        &std::fs::read(&active_turns_path).expect("read persisted active turns"),
    )
    .expect("decode persisted active turns");
    assert_eq!(
        persisted.pointer("/turns").and_then(Value::as_array),
        Some(&Vec::new()),
        "the persisted snapshot drops the retired session's route: {persisted:#}"
    );
    gate.release(1);
    workbench.shutdown().await;
}

/// FIG-995: queue management withdraws at the store boundary, edits only a
/// successfully cancelled anchor, and leaves earlier and admitted input alone.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn queued_inputs_can_be_single_cancelled_suffix_edited_and_resubmitted() {
    let mut gate = GatedProvider::new();
    let workbench = Workbench::builder(gate.provider.clone()).build().await;
    let state = &workbench.state;
    let session_id = state.current_session_id();
    let turn_id = running_turn(state, &mut gate).await;
    let app = turn_input_routes().with_state(state.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind routes");
    let address = listener.local_addr().expect("address");
    let server = tokio::spawn(async move { axum::serve(listener, app).await });
    let client = reqwest::Client::new();
    let base = format!("http://{address}/api/turn/input");
    let mut ids = Vec::new();
    for text in [
        "keep earlier",
        "edit anchor",
        "cancel later",
        "single cancel",
    ] {
        let receipt = client
            .post(&base)
            .json(&json!({"text": text, "ingress": "next_turn"}))
            .send()
            .await
            .expect("enqueue")
            .error_for_status()
            .expect("accepted")
            .json::<Value>()
            .await
            .expect("receipt");
        ids.push(receipt["input_id"].as_str().expect("id").to_owned());
    }
    let single = client
        .delete(format!("{base}/{}", ids[3]))
        .send()
        .await
        .expect("cancel");
    assert_eq!(
        single.status(),
        StatusCode::OK,
        "the host exposes single cancellation"
    );
    assert_eq!(
        single.json::<Value>().await.expect("single receipt")["outcome"]["outcome"],
        "cancelled"
    );
    let suffix = client
        .post(format!("{base}/{}/edit", ids[1]))
        .send()
        .await
        .expect("edit")
        .error_for_status()
        .expect("suffix cancelled")
        .json::<Value>()
        .await
        .expect("suffix receipt");
    assert_eq!(suffix["outcome"], "outcomes");
    let outcomes = suffix["data"]["outcomes"].as_array().expect("outcomes");
    assert_eq!(
        outcomes
            .iter()
            .filter(|o| o["outcome"] == "cancelled")
            .count(),
        2
    );
    let replacement = client
        .post(&base)
        .json(&json!({"text": "edited replacement", "ingress": "next_turn"}))
        .send()
        .await
        .expect("resubmit")
        .error_for_status()
        .expect("accepted")
        .json::<Value>()
        .await
        .expect("replacement");
    assert!(!ids.iter().any(|id| replacement["input_id"] == *id));
    let session = state
        .open_session(&session_id, "test")
        .await
        .expect("session");
    let pending = session
        .durable()
        .pending_turn_inputs()
        .await
        .expect("pending");
    assert!(pending.iter().any(|p| p.input.input_id == ids[0]));
    assert!(
        pending
            .iter()
            .any(|p| replacement["input_id"] == p.input.input_id.to_string())
    );
    assert!(
        !pending
            .iter()
            .any(|p| ids[1..].contains(&p.input.input_id.to_string()))
    );
    let admitted = pending.iter().find(|p| matches!(&p.status, lash::PendingTurnInputReadStatus::Admitted { run } if *run == turn_id)).expect("admitted input");
    let refusal = client
        .delete(format!("{base}/{}", admitted.input.input_id))
        .send()
        .await
        .expect("cancel admitted")
        .json::<Value>()
        .await
        .expect("typed refusal");
    assert_eq!(refusal["outcome"]["outcome"], "already_admitted");
    gate.release(1);
    server.abort();
    let _ = server.await;
    workbench.shutdown().await;
}
