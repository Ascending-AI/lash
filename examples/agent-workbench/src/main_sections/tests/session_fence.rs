use super::*;
use lash::SessionId;

// Session fencing (FIG-2358): a delete and a turn submit order through one
// fence, and a delete settles the turn it finds running.

#[test]
fn replacing_a_non_current_session_does_not_rotate_the_selected_session() {
    let ids = WorkbenchSessions::fresh();
    let retired = ids.current();
    ids.ensure(&retired);
    let selected = "workbench-selected-during-delete";
    ids.record(
        SessionId::fixture(selected.to_string()),
        "selected".to_string(),
    );
    ids.select(&SessionId::from(selected))
        .expect("select competing session");

    let (replacement, replaced_current) = ids.replace(&retired);

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

    let (replacement, replaced_current) = ids.replace(&SessionId::from(retired));

    assert!(!replaced_current);
    let entry = ids
        .entry(&replacement)
        .expect("replacement joins the roster");
    assert_eq!(entry.name, retired);
}

#[test]
fn a_retiring_mark_refuses_the_claim_until_the_delete_is_abandoned() {
    let active_turns = ActiveTurns::default();
    assert!(active_turns.begin_retirement(&SessionId::from("fenced")));
    assert!(!active_turns.begin_retirement(&SessionId::from("fenced")));
    assert_eq!(
        active_turns.try_insert_for_idle_session(
            &SessionId::from("fenced"),
            &TurnId::from("late-turn"),
            WorkbenchTurnKind::User,
        ),
        ActiveTurnClaim::Refused(SessionRetirement::Retiring)
    );
    assert!(
        active_turns
            .for_session(&SessionId::from("fenced"))
            .is_none()
    );

    active_turns.abandon_retirement(&SessionId::from("fenced"));
    assert_eq!(active_turns.retirement(&SessionId::from("fenced")), None);
    assert_eq!(
        active_turns.try_insert_for_idle_session(
            &SessionId::from("fenced"),
            &TurnId::from("after-abandon"),
            WorkbenchTurnKind::User,
        ),
        ActiveTurnClaim::Claimed
    );
    assert_eq!(
        active_turns.try_insert_for_idle_session(
            &SessionId::from("fenced"),
            &TurnId::from("second"),
            WorkbenchTurnKind::User,
        ),
        ActiveTurnClaim::Busy
    );
}

#[test]
fn a_confirmed_retirement_is_never_lifted() {
    let active_turns = ActiveTurns::default();
    active_turns.begin_retirement(&SessionId::from("gone"));
    active_turns.confirm_retirement(&SessionId::from("gone"));
    active_turns.abandon_retirement(&SessionId::from("gone"));
    assert_eq!(
        active_turns.retirement(&SessionId::from("gone")),
        Some(SessionRetirement::Retired)
    );
    assert_eq!(
        active_turns.try_insert_for_idle_session(
            &SessionId::from("gone"),
            &TurnId::from("late-turn"),
            WorkbenchTurnKind::User,
        ),
        ActiveTurnClaim::Refused(SessionRetirement::Retired)
    );
    // Confirming straight from the durable fact needs no prior mark.
    active_turns.confirm_retirement(&SessionId::from("tombstoned"));
    assert_eq!(
        active_turns.retirement(&SessionId::from("tombstoned")),
        Some(SessionRetirement::Retired)
    );
}

/// A send that passed its admission read and is held at the claim while a
/// delete runs to completion is refused at the claim: the typed deleted
/// conflict, no claim left behind, no user row, no provider call.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_delete_that_lands_between_admission_and_claim_refuses_the_send() {
    let (gate, entered_rx) = super::concurrent_send_tests::TurnAdmissionGate::new(
        "agent_workbench.api.turn.claim_ready",
    );
    let workbench = Workbench::builder(silent_provider())
        .trace_sink(Arc::clone(&gate) as Arc<dyn TraceSink>)
        .build()
        .await;
    let state = &workbench.state;
    let old_session_id = state.current_session_id();
    let send = tokio::spawn({
        let state = state.clone();
        let old_session_id = old_session_id.clone();
        async move {
            send_text(
                &state,
                Some(&old_session_id),
                "send that loses to the delete",
            )
            .await
        }
    });
    super::concurrent_send_tests::entered(entered_rx).await;

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

    gate.open();
    let error = send
        .await
        .expect("send task")
        .expect_err("the send released after the delete is refused at the claim");
    super::recoverable_chat_tests::assert_deleted_session_conflict(&error, &old_session_id);
    assert!(
        state.active_turns.for_session(&old_session_id).is_none(),
        "a refused send leaves no active-turn claim behind"
    );
    assert!(
        product_rows(state, &old_session_id, "user").is_empty(),
        "a refused send commits no user row"
    );
    workbench.shutdown().await;
}

/// The position of the first trace named `name` among `trace`'s records.
fn trace_position(trace: &RecordingTrace, name: &str) -> Option<usize> {
    let name = format!("agent_workbench.{name}");
    trace.records().iter().position(|record| {
        matches!(&record.event, TraceEvent::Custom { name: recorded, .. } if *recorded == name)
    })
}

/// Deleting a session whose turn awaits a real process cancels that turn
/// before the close is requested: the delete's cancel step precedes its
/// retirement, the awaited process ends cancelled, the session's claim is
/// released, and the retired id is refused afterwards.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn deleting_a_session_with_a_running_turn_cancels_it_before_retiring() {
    let trace = Arc::new(RecordingTrace::default());
    let workbench = Workbench::builder(replying_provider(
        r#"<typescript>
const hold_for_delete = await processes.create({ dialect: "typescript", source: `
const hold = async () => {
  await sleep(600000);
  return "unreachable";
};
` });
const handle = await processes.start({ definition: hold_for_delete });
finish(String(await handle));
</typescript>"#,
    ))
    .trace_sink(Arc::clone(&trace) as Arc<dyn TraceSink>)
    .build()
    .await;
    let state = &workbench.state;
    let old_session_id = state.current_session_id();
    send_text(state, None, "start and await the held process")
        .await
        .expect("send the process-await turn");
    let registry = workbench.stores.process_registry();
    let process_id = tokio::time::timeout(Duration::from_secs(30), async {
        loop {
            let live = registry
                .list_non_terminal_processes_page(
                    std::num::NonZeroUsize::new(16).expect("non-zero test page size"),
                    None,
                )
                .await
                .expect("list the live process while the turn awaits")
                .records;
            if let [process] = live.as_slice() {
                break process.id.clone();
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
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
    assert_ne!(snapshot.settings.session_id, old_session_id);
    assert_eq!(state.current_session_id(), snapshot.settings.session_id);
    assert_eq!(
        state.active_turns.retirement(&old_session_id),
        Some(SessionRetirement::Retired)
    );
    let cancelled = trace.custom("api.session.delete.turns_cancelled");
    let [(_, cancelled)] = cancelled.as_slice() else {
        panic!("the delete cancels the session's turns once: {cancelled:?}");
    };
    assert!(
        cancelled["cancellations"]
            .as_array()
            .is_some_and(|cancellations| cancellations.len() == 1),
        "the delete cancelled the running turn: {cancelled}"
    );
    assert!(
        trace_position(&trace, "api.session.delete.turns_cancelled")
            < trace_position(&trace, "session.retirement_settled"),
        "the running turn is cancelled before the session retires"
    );
    // The delete reclaims the finished work its session originated, so the
    // process may already be pruned: either way it ended cancelled.
    match tokio::time::timeout(
        Duration::from_secs(30),
        state.core.processes().await_output(&process_id),
    )
    .await
    .expect("the awaited process settles after the delete")
    {
        Ok(outcome) => assert_eq!(
            outcome.terminal_status(),
            Some(lash::process::TerminalProcessStatus::Cancelled),
            "{outcome:?}"
        ),
        Err(lash::EmbedError::Plugin(lash::plugins::PluginError::ProcessNoLongerRetained {
            terminal_label,
            ..
        })) => assert_eq!(
            terminal_label,
            lash::process::RetiredProcessStatus::Cancelled
        ),
        Err(error) => panic!("the awaited process outcome: {error:?}"),
    }
    assert!(state.active_turns.for_session(&old_session_id).is_none());
    let error = send_text(state, Some(&old_session_id), "must not be admitted")
        .await
        .expect_err("the retired id is refused after the delete");
    super::recoverable_chat_tests::assert_deleted_session_conflict(&error, &old_session_id);
    workbench.shutdown().await;
}

// FIG-2359 / FIG-5371: every session-bound use refuses a retired id with
// the same typed 409, side-effect ingress included. The delete retry is
// admitted so it can reconcile a durable tombstone idempotently (FIG-5347).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn every_session_bound_route_refuses_a_retired_id_with_the_same_conflict() {
    use futures_util::TryFutureExt as _;

    let workbench = Workbench::silent().await;
    let state = &workbench.state;
    let session_id = state.current_session_id();
    tombstone_session(state, &session_id).await;

    let query = || SessionQuery {
        session_id: Some(session_id.clone()),
    };
    type RouteCall<'a> =
        std::pin::Pin<Box<dyn std::future::Future<Output = Result<(), AppError>> + 'a>>;
    let routes: Vec<(&'static str, RouteCall<'_>)> = vec![
        (
            "enqueue_tool_catalog_refresh",
            Box::pin(enqueue_tool_catalog_refresh(state, "retired_session_test").map_ok(drop)),
        ),
        (
            "GET /api/state",
            Box::pin(app_state(State(state.clone()), Query(query())).map_ok(drop)),
        ),
        (
            "GET /api/events",
            Box::pin(
                session_events_with_shutdown(
                    State(state.clone()),
                    Query(ProductEventsQuery {
                        cursor: None,
                        session_id: Some(session_id.clone()),
                    }),
                    None,
                )
                .map_ok(drop),
            ),
        ),
        (
            "GET /api/observations",
            Box::pin(
                session_observations_with_shutdown(
                    State(state.clone()),
                    Query(EventsQuery {
                        cursor: None,
                        session_id: Some(session_id.clone()),
                    }),
                    None,
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
                    Json(turn_request("refused")),
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
            Box::pin(
                cancel_turn(
                    State(state.clone()),
                    Query(TurnCancelQuery {
                        session: query(),
                        mode: WorkbenchTurnCancelMode::Abort,
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
            "DELETE /api/triggers/{subscription_id}",
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
                        model: Some(TEST_MODEL.to_string()),
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
            "GET /api/lash-vm-graphs",
            Box::pin(list_lash_vm_graphs(State(state.clone()), Query(query())).map_ok(drop)),
        ),
        (
            "GET /api/lash-vm-graph/{key}",
            Box::pin(
                lash_vm_graph(
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
            "{route} must return the shared 409: {}",
            error.message
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

    let Json(settled) = Box::pin(reset_chat(State(state.clone()), Query(query())))
        .await
        .expect("DELETE /api/session admits a retry against the durable tombstone");
    assert_ne!(settled.settings.session_id, session_id);
    assert_eq!(state.current_session_id(), settled.settings.session_id);
    assert_eq!(
        state.active_turns.retirement(&session_id),
        Some(SessionRetirement::Retired),
        "the delete retry confirms retirement"
    );
    super::reset_chat_tests::assert_tombstoned(state, &session_id).await;
    let roster = serde_json::to_value(state.sessions.list()).expect("the roster encodes");

    let Json(retried) = Box::pin(reset_chat(State(state.clone()), Query(query())))
        .await
        .expect("DELETE /api/session settles an already confirmed delete idempotently");
    assert_eq!(retried.settings.session_id, settled.settings.session_id);
    assert_eq!(state.current_session_id(), settled.settings.session_id);
    assert_eq!(
        serde_json::to_value(state.sessions.list()).expect("the roster encodes"),
        roster,
        "the delete retry preserves the settled roster"
    );
    workbench.shutdown().await;
}

// FIG-3292: the ledger is keyed by session, so the states the old
// `BTreeSet<(SessionId, TurnId)>` beside a separate prompt map could represent
// are gone from memory. A file written by that build can still hold them, so
// the loader has to answer for each one.

fn write_active_turns_file(path: &std::path::Path, body: Value) {
    std::fs::write(
        path,
        serde_json::to_vec(&body).expect("encode active turns"),
    )
    .expect("write active turns fixture");
}

#[test]
fn a_persisted_prompt_for_an_absent_turn_is_dropped_rather_than_restored() {
    let temp = tempfile::tempdir().expect("orphan prompt tempdir");
    let path = temp.path().join("active-turns.json");
    write_active_turns_file(
        &path,
        json!({
            "turns": [["s1", "live-turn"]],
            "prompts": [
                { "session_id": "s1", "turn_id": "retired-turn", "prompt": "orphan",
                  "row_id": "orphan-ui-input", "at": "2026-10-03T00:00:00Z" }
            ],
        }),
    );

    let restored = ActiveTurns::persistent(path).expect("an orphan prompt must not fail the boot");

    let active = restored
        .for_session(&SessionId::from("s1"))
        .expect("the live turn is restored");
    assert_eq!(active.address.turn_id, TurnId::from("live-turn"));
    assert_eq!(
        active.prompt, None,
        "a prompt naming a turn the file does not carry is not that turn's prompt"
    );
}

#[test]
fn a_persisted_second_turn_for_one_session_is_dropped_on_load() {
    let temp = tempfile::tempdir().expect("double claim tempdir");
    let path = temp.path().join("active-turns.json");
    write_active_turns_file(
        &path,
        json!({
            "turns": [["s1", "alpha"], ["s1", "beta"], ["s2", "gamma"]],
            "prompts": [],
        }),
    );

    let restored =
        ActiveTurns::persistent(path).expect("a doubly claimed session must not fail the boot");

    assert_eq!(
        restored
            .for_session(&SessionId::from("s1"))
            .map(|active| active.address.turn_id),
        Some(TurnId::from("alpha")),
        "one session holds one turn; the first in key order survives"
    );
    assert_eq!(
        restored
            .for_session(&SessionId::from("s2"))
            .map(|active| active.address.turn_id),
        Some(TurnId::from("gamma")),
        "a second session is untouched by the first session's repair"
    );
}

#[test]
fn a_persisted_turn_without_a_kind_recovers_one_from_its_id() {
    let temp = tempfile::tempdir().expect("legacy kind tempdir");
    let path = temp.path().join("active-turns.json");
    // A file this build did not write: no `kinds` array at all.
    write_active_turns_file(
        &path,
        json!({
            "turns": [["s1", "workbench-queued-abc"], ["s2", "workbench-turn-def"]],
            "prompts": [],
        }),
    );

    let restored = ActiveTurns::persistent(path).expect("a pre-kind file must still load");

    assert_eq!(
        restored
            .for_session(&SessionId::from("s1"))
            .map(|active| active.kind),
        Some(WorkbenchTurnKind::Queued)
    );
    assert_eq!(
        restored
            .for_session(&SessionId::from("s2"))
            .map(|active| active.kind),
        Some(WorkbenchTurnKind::User)
    );
}

#[test]
fn a_claimed_kind_survives_a_restart_without_consulting_the_turn_id() {
    let temp = tempfile::tempdir().expect("kind round trip tempdir");
    let path = temp.path().join("active-turns.json");
    let session_id = SessionId::from("s1");
    // An id whose prefix says "user"; only the persisted kind says otherwise.
    let turn_id = TurnId::from("workbench-turn-mislabelled");
    let turns = ActiveTurns::persistent(path.clone()).expect("open active turns");
    assert_eq!(
        turns.try_insert_with_prompt_for_idle_session(
            &session_id,
            &turn_id,
            WorkbenchTurnKind::Queued,
            Some("held prompt".to_string()),
            None,
        ),
        ActiveTurnClaim::Claimed
    );
    drop(turns);

    let restored = ActiveTurns::persistent(path).expect("reopen active turns");

    let active = restored
        .for_session(&session_id)
        .expect("the claim is restored");
    assert_eq!(active.address.turn_id, turn_id);
    assert_eq!(
        active.kind,
        WorkbenchTurnKind::Queued,
        "the persisted kind outranks whatever the id prefix suggests"
    );
    assert_eq!(
        active.prompt.map(|prompt| prompt.text),
        Some("held prompt".to_string()),
        "the turn and its prompt are restored as one value"
    );
}

#[test]
fn two_sessions_claim_their_own_slots_without_interference() {
    let active_turns = ActiveTurns::default();
    assert_eq!(
        active_turns.try_insert_for_idle_session(
            &SessionId::from("left"),
            &TurnId::from("left-turn"),
            WorkbenchTurnKind::User,
        ),
        ActiveTurnClaim::Claimed
    );
    assert_eq!(
        active_turns.try_insert_for_idle_session(
            &SessionId::from("right"),
            &TurnId::from("right-turn"),
            WorkbenchTurnKind::Queued,
        ),
        ActiveTurnClaim::Claimed,
        "the claim is a lookup on this session, not a scan of every claim"
    );
    active_turns.remove(&SessionId::from("left"), &TurnId::from("left-turn"));
    assert!(active_turns.for_session(&SessionId::from("left")).is_none());
    assert_eq!(
        active_turns
            .for_session(&SessionId::from("right"))
            .map(|active| active.kind),
        Some(WorkbenchTurnKind::Queued)
    );
}

#[test]
fn releasing_a_claim_a_later_turn_already_replaced_leaves_that_turn_alone() {
    let active_turns = ActiveTurns::default();
    let session_id = SessionId::from("s1");
    active_turns.insert(&session_id, "first", WorkbenchTurnKind::User);
    active_turns.remove(&session_id, &TurnId::from("first"));
    active_turns.insert(&session_id, "second", WorkbenchTurnKind::Queued);

    // The first turn's submission guard runs late.
    active_turns.remove(&session_id, &TurnId::from("first"));

    assert_eq!(
        active_turns
            .for_session(&session_id)
            .map(|active| active.address.turn_id),
        Some(TurnId::from("second")),
        "release is addressed to a turn, not a session-wide clear"
    );
}
