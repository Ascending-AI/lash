//! Two sessions of one workbench run side by side and share nothing: each
//! transcript holds only its own turn, each session owns its own trigger
//! registration, and a press in one starts work only in that one.

use super::*;
use lash::SessionId;

/// The text of every user row `session_id` renders.
async fn user_rows(state: &AppState, session_id: &SessionId) -> Vec<String> {
    state_rows(
        &read_state(state, Some(session_id))
            .await
            .expect("project the session"),
    )
    .into_iter()
    .filter(|(role, _)| role == "user")
    .map(|(_, text)| text)
    .collect()
}

/// The process ids `session_id`'s work rail lists.
async fn session_work(state: &AppState, session_id: &SessionId) -> Vec<lash::ProcessId> {
    let Json(work) = list_work(
        State(state.clone()),
        Query(SessionQuery {
            session_id: Some(session_id.clone()),
        }),
    )
    .await
    .expect("list the session's work");
    work.into_iter()
        .map(|item| item.process.process_id)
        .collect()
}

/// The processes a press of the button in `session_id` started.
async fn press_in(state: &AppState, session_id: &SessionId) -> Vec<lash::ProcessId> {
    let before = state.messages_snapshot().len();
    let Json(accepted) = button_trigger(
        State(state.clone()),
        Query(SessionQuery {
            session_id: Some(session_id.clone()),
        }),
        Json(ButtonEventRequest {
            button: ButtonChoice::Blue,
            model: None,
            model_variant: None,
        }),
    )
    .await
    .expect("press the button");
    assert!(accepted.accepted);
    // A wake turn the other session's press caused may publish beside it.
    let rows = state
        .messages_snapshot()
        .split_off(before)
        .into_iter()
        .filter(|row| row.role == "event")
        .collect::<Vec<_>>();
    let [press] = rows.as_slice() else {
        panic!("one press is one row: {rows:?}");
    };
    let Some(ChatMessageProvenance::TriggerOccurrence { process_ids, .. }) = &press.provenance
    else {
        panic!("the press row names its occurrence: {press:?}");
    };
    process_ids.clone()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_sessions_isolate_transcripts_triggers_and_processes() {
    let workbench = Workbench::replying(super::reset_chat_tests::BUTTON_TRIGGER_REGISTRATION).await;
    let state = &workbench.state;
    let session_a = state.current_session_id();
    let Json(created) = create_session(
        State(state.clone()),
        Json(SessionCreateRequest {
            name: Some("session b".to_string()),
        }),
    )
    .await
    .expect("create session B");
    let session_b = created.session_id;

    let (turn_a, turn_b) = tokio::join!(
        send_text(state, Some(&session_a), "isolation-marker-A"),
        send_text(state, Some(&session_b), "isolation-marker-B"),
    );
    let turn_a = started_turn_id(&turn_a.expect("session A's send"));
    let turn_b = started_turn_id(&turn_b.expect("session B's send"));
    wait_for_turn_released(state, &session_a, &turn_a, Duration::from_secs(30)).await;
    wait_for_turn_released(state, &session_b, &turn_b, Duration::from_secs(30)).await;
    assert_eq!(
        user_rows(state, &session_a).await,
        vec!["isolation-marker-A".to_string()]
    );
    assert_eq!(
        user_rows(state, &session_b).await,
        vec!["isolation-marker-B".to_string()]
    );

    let Json(registrations_a) = list_triggers(
        State(state.clone()),
        Query(SessionQuery {
            session_id: Some(session_a.clone()),
        }),
    )
    .await
    .expect("session A's triggers");
    let Json(registrations_b) = list_triggers(
        State(state.clone()),
        Query(SessionQuery {
            session_id: Some(session_b.clone()),
        }),
    )
    .await
    .expect("session B's triggers");
    assert_eq!(registrations_a.len(), 1, "{registrations_a:?}");
    assert_eq!(registrations_b.len(), 1, "{registrations_b:?}");

    let started_a = press_in(state, &session_a).await;
    let started_b = press_in(state, &session_b).await;
    assert_eq!(started_a.len(), 1, "A's press starts only A's trigger");
    assert_eq!(started_b.len(), 1, "B's press starts only B's trigger");
    assert_ne!(started_a, started_b);
    for process_id in started_a.iter().chain(&started_b) {
        tokio::time::timeout(
            Duration::from_secs(30),
            state.core.processes().await_output(process_id),
        )
        .await
        .expect("the trigger's process finishes in time")
        .expect("the trigger's process finishes");
    }
    assert_eq!(session_work(state, &session_a).await, started_a);
    assert_eq!(session_work(state, &session_b).await, started_b);
    workbench.shutdown().await;
}
