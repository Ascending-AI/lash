//! A button trigger's lifecycle through the workbench's routes, beside a
//! running foreground turn.

use super::*;

/// The registrations the trigger route lists for the current session.
async fn listed_triggers(state: &AppState) -> Vec<WorkbenchTriggerRegistration> {
    list_triggers(State(state.clone()), Query(SessionQuery::default()))
        .await
        .expect("list the session's triggers")
        .0
}

/// Press the red button and answer the processes the press started.
async fn press_red(state: &AppState) -> Vec<lash::ProcessId> {
    let before = state.messages_snapshot().len();
    let Json(accepted) = button_trigger(
        State(state.clone()),
        Query(SessionQuery::default()),
        Json(ButtonEventRequest {
            button: ButtonChoice::Red,
            model: None,
            model_variant: None,
        }),
    )
    .await
    .expect("press the button");
    assert!(accepted.accepted);
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

async fn set_enabled(state: &AppState, subscription_key: &str, enabled: bool) {
    let Json(response) = set_trigger_enabled(
        AxumPath(subscription_key.to_string()),
        State(state.clone()),
        Query(SessionQuery::default()),
        Json(TriggerEnabledRequest { enabled }),
    )
    .await
    .expect("toggle the trigger");
    assert!(response.changed);
    assert_eq!(
        response
            .registration
            .map(|registration| registration.enabled),
        Some(enabled)
    );
}

/// A button trigger registered by a turn stays listed while disabled and
/// re-enabled, and leaves the listing once deleted; only an enabled trigger
/// starts work on a press. The wakes its work delivers while a foreground
/// turn runs wait as the session's queued work, each carrying its press, and
/// the work rail lists every run completed with its wake-carrying emission.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn button_trigger_lifecycle_stays_visible_and_queues_wakes_during_active_turn() {
    let mut gate = GatedProvider::replying(|call| match call {
        0 => super::reset_chat_tests::BUTTON_TRIGGER_REGISTRATION.to_string(),
        call => finish_cell(&format!("answer {call}")),
    });
    let workbench = Workbench::builder(gate.provider.clone()).build().await;
    let state = &workbench.state;
    let session_id = state.current_session_id();
    let registered = send_text(state, None, "register the button trigger")
        .await
        .expect("the registration send is admitted");
    assert_eq!(gate.next_call().await, 0);
    gate.release(1);
    wait_for_turn_released(
        state,
        &session_id,
        &started_turn_id(&registered),
        Duration::from_secs(30),
    )
    .await;
    let [registration] = listed_triggers(state)
        .await
        .try_into()
        .expect("the turn registered one trigger");
    let key = registration.registration.subscription_key.clone();
    assert!(registration.registration.enabled);

    // A foreground turn holds the session from here on.
    let foreground = send_text(state, None, "hold the session")
        .await
        .expect("the foreground send is admitted");
    let foreground = started_turn_id(&foreground);
    assert_eq!(gate.next_call().await, 1);

    let mut started = press_red(state).await;
    started.extend(press_red(state).await);
    assert_eq!(started.len(), 2, "each enabled press starts one run");

    set_enabled(state, &key, false).await;
    assert_eq!(
        listed_triggers(state).await.len(),
        1,
        "a disabled trigger stays listed"
    );
    assert!(
        press_red(state).await.is_empty(),
        "a disabled trigger starts nothing"
    );
    set_enabled(state, &key, true).await;
    let reenabled = press_red(state).await;
    assert_eq!(reenabled.len(), 1, "a re-enabled trigger starts its run");
    started.extend(reenabled);
    let Json(deleted) = delete_trigger(
        AxumPath(key.clone()),
        State(state.clone()),
        Query(SessionQuery::default()),
    )
    .await
    .expect("delete the trigger");
    assert!(deleted.changed);
    assert!(listed_triggers(state).await.is_empty());
    assert!(
        press_red(state).await.is_empty(),
        "a deleted trigger starts nothing"
    );

    for process_id in &started {
        tokio::time::timeout(
            Duration::from_secs(30),
            state.core.processes().await_output(process_id),
        )
        .await
        .expect("the trigger's run finishes in time")
        .expect("the trigger's run finishes");
    }
    let Json(queued) = list_queued_work(State(state.clone()), Query(SessionQuery::default()))
        .await
        .expect("list the session's queued work");
    assert!(!queued.is_empty(), "the runs' wakes wait for the turn");
    for batch in &queued {
        let lash::persistence::QueuedWorkPayload::ProcessWake { wake } = &batch.payload else {
            panic!("a run delivers a process wake: {batch:?}");
        };
        assert_eq!(wake.target_session_id, session_id);
        assert!(
            wake.input.contains("button_pressed") && wake.input.contains("Red"),
            "the wake carries its press: {}",
            wake.input
        );
    }
    let Json(work) = list_work(
        State(state.clone()),
        Query(SessionQuery {
            session_id: Some(session_id.clone()),
        }),
    )
    .await
    .expect("list the session's work");
    assert_eq!(
        work.iter()
            .map(|item| item.process.process_id.clone())
            .collect::<BTreeSet<_>>(),
        started.iter().cloned().collect::<BTreeSet<_>>()
    );
    for item in &work {
        assert_eq!(item.process.status_label, "completed", "{item:?}");
        let events = item
            .events
            .iter()
            .map(|event| event.event_type.as_str())
            .collect::<Vec<_>>();
        assert!(events.contains(&"process.yield"), "{events:?}");
        assert!(events.contains(&"process.completed"), "{events:?}");
    }

    // The foreground turn and the wake turns behind it run once released.
    gate.release(16);
    wait_for_turn_released(state, &session_id, &foreground, Duration::from_secs(30)).await;
    workbench.shutdown().await;
}
