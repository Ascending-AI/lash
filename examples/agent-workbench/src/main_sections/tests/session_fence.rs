use super::*;

// Session fencing (FIG-2358): a delete and a turn submit order through one
// fence, and a delete settles the turn it finds running.

use super::concurrent_send_tests::TurnAdmissionGate;
use super::recoverable_chat_tests::{
    assert_deleted_session_conflict, recoverable_chat_test_state_with_provider,
    retire_workbench_session,
};

#[test]
fn reset_session_rotation_replaces_workbench_session_id() {
    let ids = WorkbenchSessions::fresh();
    let original = ids.current();
    let (new, replaced_current) = ids.replace(&original, lash::rlm::RlmDialect::Lashlang);
    assert!(replaced_current);
    assert_eq!(ids.current(), new);
    assert_ne!(original, new);
    assert!(original.starts_with(SESSION_ID_PREFIX));
    assert!(new.starts_with(SESSION_ID_PREFIX));
}

#[test]
fn replacing_a_non_current_session_does_not_rotate_the_selected_session() {
    let ids = WorkbenchSessions::fresh();
    let retired = ids.current();
    ids.ensure(&retired, lash::rlm::RlmDialect::Lashlang);
    let selected = "workbench-selected-during-delete";
    ids.record(
        selected.to_string(),
        "selected".to_string(),
        lash::rlm::RlmDialect::Lashlang,
    );
    ids.select(selected).expect("select competing session");

    let (replacement, replaced_current) = ids.replace(&retired, lash::rlm::RlmDialect::Lashlang);

    assert!(!replaced_current);
    assert_eq!(ids.current(), selected);
    assert!(ids.entry(&retired).is_none());
    assert_eq!(
        ids.entry(&replacement)
            .expect("replacement keeps retired roster slot")
            .name,
        retired
    );
}

#[test]
fn replacing_an_unrostered_session_records_its_replacement() {
    let ids = WorkbenchSessions::fresh();
    let retired = "workbench-external-session";

    let (replacement, replaced_current) = ids.replace(retired, lash::rlm::RlmDialect::Typescript);

    assert!(!replaced_current);
    let entry = ids
        .entry(&replacement)
        .expect("replacement joins the roster");
    assert_eq!(entry.name, retired);
    assert_eq!(entry.dialect, lash::rlm::RlmDialect::Typescript);
}

#[test]
fn a_retiring_mark_refuses_the_claim_until_the_delete_is_abandoned() {
    let active_turns = ActiveTurns::default();
    assert!(active_turns.begin_retirement("fenced"));
    assert!(!active_turns.begin_retirement("fenced"));
    assert_eq!(
        active_turns.try_insert_for_idle_session("fenced", "late-turn"),
        ActiveTurnClaim::Refused(SessionRetirement::Retiring)
    );
    assert!(active_turns.for_session("fenced").is_empty());

    active_turns.abandon_retirement("fenced");
    assert_eq!(active_turns.retirement("fenced"), None);
    assert_eq!(
        active_turns.try_insert_for_idle_session("fenced", "after-abandon"),
        ActiveTurnClaim::Claimed
    );
    assert_eq!(
        active_turns.try_insert_for_idle_session("fenced", "second"),
        ActiveTurnClaim::Busy
    );
}

#[test]
fn a_confirmed_retirement_is_never_lifted() {
    let active_turns = ActiveTurns::default();
    active_turns.begin_retirement("gone");
    active_turns.confirm_retirement("gone");
    active_turns.abandon_retirement("gone");
    assert_eq!(
        active_turns.retirement("gone"),
        Some(SessionRetirement::Retired)
    );
    assert_eq!(
        active_turns.try_insert_for_idle_session("gone", "late-turn"),
        ActiveTurnClaim::Refused(SessionRetirement::Retired)
    );
    // Confirming straight from the durable fact needs no prior mark.
    active_turns.confirm_retirement("tombstoned");
    assert_eq!(
        active_turns.retirement("tombstoned"),
        Some(SessionRetirement::Retired)
    );
}

#[test]
fn a_retiring_session_refuses_use_but_admits_the_delete_retry() {
    run_async_test_on_stack_budget("session-fence-retiring-admission-test", || async {
        let data_dir = tempfile::tempdir().expect("tempdir");
        let state = recoverable_chat_test_state(data_dir.path(), 16).await;
        let session_id = state.current_session_id();
        let query = SessionQuery {
            session_id: Some(session_id.clone()),
        };
        state
            .admit_session(&query, "api.state")
            .await
            .expect("live");
        state.active_turns.begin_retirement(&session_id);

        let error = state
            .admit_session(&query, "api.state")
            .await
            .expect_err("a retiring session refuses use");
        assert_eq!(error.status, StatusCode::CONFLICT);
        assert_eq!(error.verdict, AppErrorVerdict::Terminal);
        assert_eq!(error.message, retiring_session_message(&session_id));
        assert_eq!(
            state
                .admit_session_for_delete(&query, "api.session.delete")
                .await
                .expect("a retiring session admits the delete retry"),
            session_id
        );

        // An ambiguous outcome keeps the mark; a definitive failure follows the
        // durable fact, which says the session is live.
        state
            .settle_retirement_mark(&session_id, &Err(AppError::internal("ambiguous")))
            .await;
        assert_eq!(
            state.active_turns.retirement(&session_id),
            Some(SessionRetirement::Retiring)
        );
        state
            .settle_retirement_mark(&session_id, &Err(AppError::conflict("remains live")))
            .await;
        assert_eq!(state.active_turns.retirement(&session_id), None);
        state
            .admit_session(&query, "api.state")
            .await
            .expect("live again");

        // Once the durable tombstone exists, a delete retry is refused like any
        // other use.
        retire_workbench_session(&state, &session_id).await;
        let error = state
            .admit_session_for_delete(&query, "api.session.delete")
            .await
            .expect_err("a tombstoned session refuses the delete");
        assert_deleted_session_conflict(&error, &session_id);
    });
}

#[test]
fn a_delete_that_lands_between_admission_and_claim_refuses_the_send() {
    run_async_test_on_stack_budget_multi_thread("session-fence-delete-vs-submit-race", 2, || {
        a_delete_that_lands_between_admission_and_claim_refuses_the_send_inner()
    });
}

async fn a_delete_that_lands_between_admission_and_claim_refuses_the_send_inner() {
    let data_dir = tempfile::tempdir().expect("delete-vs-submit race tempdir");
    let provider = lash::testing::TestProvider::builder()
        .kind("workbench-delete-vs-submit-race")
        .complete_error("a send that loses to the delete must not call the provider")
        .build()
        .into_handle();
    let mut state = queued_send_test_state(data_dir.path(), provider).await;
    let (restate_ingress_url, mut restate_requests) = spawn_restate_ingress_capture().await;
    state.restate_ingress_url = restate_ingress_url;
    // The rendezvous: the send passes its admission read and stops at the
    // claim boundary; the delete runs to completion while it is stopped; then
    // the send is released into the claim. Losing the schedule is forced, not
    // waited for.
    let (entered_tx, entered_rx) = std::sync::mpsc::sync_channel(1);
    let release = Arc::new((Mutex::new(false), std::sync::Condvar::new()));
    state.trace_sink = Some(Arc::new(TurnAdmissionGate {
        event_name: "agent_workbench.api.turn.claim_ready",
        entered: entered_tx,
        release: Arc::clone(&release),
    }));
    let old_session_id = state.current_session_id();
    let send = tokio::spawn({
        let state = state.clone();
        let old_session_id = old_session_id.clone();
        async move {
            send_turn(
                State(state),
                Query(SessionQuery {
                    session_id: Some(old_session_id),
                }),
                Json(TurnRequest {
                    text: "send that loses to the delete".to_string(),
                    model: Some("test-model".to_string()),
                    model_variant: None,
                    attachment_id: None,
                }),
            )
            .await
        }
    });
    entered_rx
        .recv_timeout(Duration::from_secs(5))
        .expect("the send reaches the claim boundary");

    let Json(snapshot) = Box::pin(reset_chat(
        State(state.clone()),
        Query(SessionQuery {
            session_id: Some(old_session_id.clone()),
        }),
    ))
    .await
    .expect("the delete completes while the send is held at the claim");
    assert_ne!(snapshot.settings.session_id, old_session_id);
    assert_eq!(
        state.active_turns.retirement(&old_session_id),
        Some(SessionRetirement::Retired)
    );
    let delete_request = restate_requests
        .recv()
        .await
        .expect("the delete workflow call is captured");
    assert!(
        delete_request
            .get("path")
            .and_then(Value::as_str)
            .is_some_and(|path| path.starts_with("WorkbenchSessionDeleteWorkflow/")),
        "unexpected Restate request before the send was released: {delete_request}"
    );

    let (released, condition) = &*release;
    *released.lock().unwrap_or_else(|error| error.into_inner()) = true;
    condition.notify_all();
    let error = send
        .await
        .expect("send task")
        .expect_err("the send released after the delete is refused at the claim");
    assert_deleted_session_conflict(&error, &old_session_id);
    assert!(
        state.active_turns.for_session(&old_session_id).is_empty(),
        "a refused send leaves no active-turn claim behind"
    );
    assert!(
        matches!(
            restate_requests.try_recv(),
            Err(mpsc::error::TryRecvError::Empty)
        ),
        "a refused send submits no turn workflow"
    );
    assert!(
        state.messages_snapshot().is_empty(),
        "a refused send commits no user row"
    );
}

struct ProcessAwaitEntered {
    entered: tokio::sync::Notify,
}

impl lash::runtime::RuntimeTurnPhaseProbe for ProcessAwaitEntered {
    fn begin(&self, _phase: lash::runtime::RuntimeTurnPhase) {}

    fn end(&self, _phase: lash::runtime::RuntimeTurnPhase) {}

    fn begin_named(&self, phase: &str) {
        if phase == "process.await_handle" {
            self.entered.notify_one();
        }
    }
}

#[test]
fn deleting_a_session_with_a_running_turn_cancels_it_before_retiring() {
    run_async_test_on_stack_budget("session-fence-running-turn-delete", || {
        deleting_a_session_with_a_running_turn_cancels_it_before_retiring_inner()
    });
}

async fn deleting_a_session_with_a_running_turn_cancels_it_before_retiring_inner() {
    let data_dir = tempfile::tempdir().expect("running-turn delete tempdir");
    let provider = lash::testing::TestProvider::builder()
        .kind("workbench-running-turn-delete")
        .complete(|_| async {
            Ok(text_response(
                r#"<lashlang>
process hold_for_delete() {
  sleep for "10m"
  finish "unreachable"
}
handle = start hold_for_delete()
finish (await handle)?
</lashlang>"#,
            ))
        })
        .build()
        .into_handle();
    let mut state = recoverable_chat_test_state_with_provider(data_dir.path(), 16, provider).await;
    let (restate_ingress_url, mut restate_requests) = spawn_restate_ingress_capture().await;
    state.restate_ingress_url = restate_ingress_url;
    let old_session_id = state.current_session_id();
    let turn_text = "start and await the held process";
    let Json(accepted) = send_turn(
        State(state.clone()),
        Query(SessionQuery::default()),
        Json(TurnRequest {
            text: turn_text.to_string(),
            model: Some("test-model".to_string()),
            model_variant: None,
            attachment_id: None,
        }),
    )
    .await
    .expect("send the process-await turn through the production handler");
    assert!(accepted.accepted);
    let submitted = restate_requests
        .recv()
        .await
        .expect("capture the submitted turn");
    let turn_id = submitted
        .pointer("/body/turn_id")
        .and_then(Value::as_str)
        .expect("submitted turn id")
        .to_string();
    assert!(state.active_turns.contains(&old_session_id, &turn_id));
    let session = state
        .core
        .session(old_session_id.clone())
        .open()
        .await
        .expect("open the submitted session");
    let await_entered = Arc::new(ProcessAwaitEntered {
        entered: tokio::sync::Notify::new(),
    });
    session
        .set_turn_phase_probe(
            Arc::clone(&await_entered) as Arc<dyn lash::runtime::RuntimeTurnPhaseProbe>
        )
        .await;
    let run_turn_id = turn_id.clone();
    let turn = tokio::spawn(async move {
        session
            .turn(lash::TurnInput::text(turn_text))
            .turn_id(run_turn_id)
            .run()
            .await
    });
    tokio::time::timeout(Duration::from_secs(10), await_entered.entered.notified())
        .await
        .expect("the turn reaches a real process await");

    let Json(snapshot) = Box::pin(reset_chat(
        State(state.clone()),
        Query(SessionQuery {
            session_id: Some(old_session_id.clone()),
        }),
    ))
    .await
    .expect("the delete cancels the running turn and retires the session");
    let turn = turn
        .await
        .expect("the cancelled turn joins")
        .expect("the cancelled turn commits its terminal");
    assert!(
        matches!(
            turn.result.outcome,
            lash::TurnOutcome::Stopped(lash::TurnStop::Cancelled { .. })
        ),
        "the delete must settle the running turn as cancelled, got {:?}",
        turn.result.outcome
    );
    assert_ne!(snapshot.settings.session_id, old_session_id);
    assert_eq!(state.current_session_id(), snapshot.settings.session_id);
    assert!(state.active_turns.for_session(&old_session_id).is_empty());
    assert_eq!(
        state.active_turns.retirement(&old_session_id),
        Some(SessionRetirement::Retired)
    );
    let delete_request = restate_requests
        .recv()
        .await
        .expect("the delete workflow call is captured");
    assert!(
        delete_request
            .get("path")
            .and_then(Value::as_str)
            .is_some_and(|path| path.starts_with("WorkbenchSessionDeleteWorkflow/")),
        "the delete follows the cancel: {delete_request}"
    );
    let error = send_turn(
        State(state.clone()),
        Query(SessionQuery {
            session_id: Some(old_session_id.clone()),
        }),
        Json(TurnRequest {
            text: "must not be admitted".to_string(),
            model: Some("test-model".to_string()),
            model_variant: None,
            attachment_id: None,
        }),
    )
    .await
    .expect_err("the retired id is refused after the delete");
    assert_deleted_session_conflict(&error, &old_session_id);
}

// FIG-2359: every session-bound route resolves its id through the one
// admission read, so a retired id gets the same typed 409 everywhere —
// side-effect ingress included — instead of a 200 for a dead session.
#[test]
fn every_session_bound_route_refuses_a_retired_id_with_the_same_conflict() {
    run_async_test_on_stack_budget("session-fence-route-sweep-test", || async {
        use futures_util::TryFutureExt as _;

        let data_dir = tempfile::tempdir().expect("route sweep tempdir");
        let state = recoverable_chat_test_state(data_dir.path(), 16).await;
        let session_id = state.current_session_id();
        retire_workbench_session(&state, &session_id).await;

        let query = || SessionQuery {
            session_id: Some(session_id.clone()),
        };
        type RouteCall<'a> =
            std::pin::Pin<Box<dyn std::future::Future<Output = Result<(), AppError>> + 'a>>;
        let routes: Vec<(&'static str, RouteCall<'_>)> = vec![
            (
                "GET /api/state",
                Box::pin(app_state(State(state.clone()), Query(query())).map_ok(drop)),
            ),
            (
                "GET /api/events",
                Box::pin(
                    session_events(
                        State(state.clone()),
                        Query(ProductEventsQuery {
                            cursor: None,
                            session_id: Some(session_id.clone()),
                        }),
                    )
                    .map_ok(drop),
                ),
            ),
            (
                "GET /api/observations",
                Box::pin(
                    session_observations(
                        State(state.clone()),
                        Query(EventsQuery {
                            cursor: None,
                            session_id: Some(session_id.clone()),
                        }),
                    )
                    .map_ok(drop),
                ),
            ),
            (
                "POST /api/turn",
                Box::pin(
                    send_turn(
                        State(state.clone()),
                        Query(query()),
                        Json(TurnRequest {
                            text: "refused".to_string(),
                            model: Some("test-model".to_string()),
                            model_variant: None,
                            attachment_id: None,
                        }),
                    )
                    .map_ok(drop),
                ),
            ),
            (
                "POST /api/turn/input",
                Box::pin(
                    enqueue_turn_input(
                        State(state.clone()),
                        Query(query()),
                        Json(TurnInputRequest {
                            text: "refused".to_string(),
                            ingress: TurnInputIngressRequest::NextTurn,
                        }),
                    )
                    .map_ok(drop),
                ),
            ),
            (
                "POST /api/turn/cancel",
                Box::pin(cancel_turn(State(state.clone()), Query(query())).map_ok(drop)),
            ),
            (
                "DELETE /api/session",
                Box::pin(Box::pin(reset_chat(State(state.clone()), Query(query()))).map_ok(drop)),
            ),
            (
                "POST /api/button-trigger",
                Box::pin(
                    button_trigger(
                        State(state.clone()),
                        Query(query()),
                        Json(ButtonEventRequest {
                            button: ButtonChoice::Red,
                            model: Some("test-model".to_string()),
                            model_variant: None,
                        }),
                    )
                    .map_ok(drop),
                ),
            ),
            (
                "GET /api/triggers",
                Box::pin(list_triggers(State(state.clone()), Query(query())).map_ok(drop)),
            ),
            (
                "PUT /api/triggers/{key}/enabled",
                Box::pin(
                    set_trigger_enabled(
                        AxumPath("any-subscription".to_string()),
                        State(state.clone()),
                        Query(query()),
                        Json(TriggerEnabledRequest { enabled: false }),
                    )
                    .map_ok(drop),
                ),
            ),
            (
                "DELETE /api/triggers/{key}",
                Box::pin(
                    delete_trigger(
                        AxumPath("any-subscription".to_string()),
                        State(state.clone()),
                        Query(query()),
                    )
                    .map_ok(drop),
                ),
            ),
            (
                "POST /api/accounts/{slug}/messages",
                Box::pin(
                    inject_message(
                        AxumPath("personal".to_string()),
                        State(state.clone()),
                        Query(query()),
                        Json(InjectMessageRequest {
                            title: "refused".to_string(),
                            text: "refused".to_string(),
                            model: Some("test-model".to_string()),
                            model_variant: None,
                        }),
                    )
                    .map_ok(drop),
                ),
            ),
            (
                "GET /api/work",
                Box::pin(list_work(State(state.clone()), Query(query())).map_ok(drop)),
            ),
            (
                "GET /api/queued-work",
                Box::pin(list_queued_work(State(state.clone()), Query(query())).map_ok(drop)),
            ),
            (
                "POST /api/queued-work/{batch}/run",
                Box::pin(
                    run_queued_work_batch(
                        AxumPath("any-batch".to_string()),
                        State(state.clone()),
                        Query(query()),
                    )
                    .map_ok(drop),
                ),
            ),
            (
                "POST /api/queued-work/{batch}/cancel",
                Box::pin(
                    cancel_queued_work_batch(
                        AxumPath("any-batch".to_string()),
                        State(state.clone()),
                        Query(query()),
                    )
                    .map_ok(drop),
                ),
            ),
            (
                "GET /api/lashlang-graphs",
                Box::pin(list_lashlang_graphs(State(state.clone()), Query(query())).map_ok(drop)),
            ),
            (
                "GET /api/lashlang-graph/{key}",
                Box::pin(
                    lashlang_graph(
                        AxumPath("any-graph".to_string()),
                        State(state.clone()),
                        Query(query()),
                    )
                    .map_ok(drop),
                ),
            ),
            (
                "POST /api/sessions/select",
                Box::pin(
                    select_session(
                        State(state.clone()),
                        Json(SessionSelectRequest {
                            session_id: session_id.clone(),
                        }),
                    )
                    .map_ok(drop),
                ),
            ),
        ];
        for (route, call) in routes {
            let error = match call.await {
                Ok(()) => panic!("{route} must refuse the retired session"),
                Err(error) => error,
            };
            assert_eq!(
                error.status,
                StatusCode::CONFLICT,
                "{route} must return the shared 409"
            );
            assert_eq!(
                error.message,
                deleted_session_message(&session_id),
                "{route} must return the shared refusal message"
            );
            assert_eq!(
                error.verdict,
                AppErrorVerdict::Terminal,
                "{route} must return the terminal verdict"
            );
        }
        assert!(
            state.messages_snapshot().is_empty(),
            "no side-effect ingress committed anything for the retired session"
        );
    });
}
