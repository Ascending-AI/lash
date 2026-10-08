//! A session's queued work on the workbench: the page lists and cancels
//! individual batches, and a process wake runs as a turn whose reply shows
//! once beside the replies before it.

use super::*;
use lash::SessionId;

/// A process wake from `source_key` queued for `session_id`, each source its
/// own batch.
fn queued_wake_draft(
    session_id: &SessionId,
    source_key: &str,
) -> lash::persistence::QueuedWorkBatchDraft {
    lash::persistence::process_wake_batch_draft_with_delivery_policy(
        lash::process::ProcessWakeDelivery {
            version: lash::formats::PROCESS_WAKE_DELIVERY_FORMAT_VERSION,
            target_session_id: session_id.clone(),
            process_id: lash::ProcessId::fixture(source_key),
            sequence: 1,
            event_type: "process.wake".to_string(),
            process_caused_by: None,
            authority: lash::persistence::QueuedWorkAuthority::default(),
            input: source_key.to_string(),
            created_at_ms: 1,
            trace_cause: Default::default(),
        },
        lash::persistence::DeliveryPolicy::EarliestSafeBoundary,
    )
}

/// The page lists a session's pending batches in order and cancels one of
/// them: the cancelled batch leaves the queue, its cancellation is a queue
/// event a recent cursor replays, and the other batch stays. The engine
/// executes every pending batch; the page offers no run-one control. A
/// foreground turn holds the session while the page acts.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn workbench_lists_and_controls_individual_queued_batches() {
    let mut gate = GatedProvider::new();
    let workbench = Workbench::builder(gate.provider.clone()).build().await;
    let state = &workbench.state;
    let session_id = state.current_session_id();
    let foreground = send_text(state, None, "hold the session")
        .await
        .expect("the foreground send is admitted");
    let foreground = started_turn_id(&foreground);
    assert_eq!(gate.next_call().await, 0);
    let session = state
        .open_session_for_observation(&session_id)
        .await
        .expect("observe the session");
    let cursor = session
        .observe()
        .snapshot()
        .await
        .expect("a durable snapshot")
        .cursor;
    let first = state
        .session_store_factory
        .enqueue_queued_work(queued_wake_draft(&session_id, "workbench-control-first"))
        .await
        .expect("enqueue the first batch");
    let second = state
        .session_store_factory
        .enqueue_queued_work(queued_wake_draft(&session_id, "workbench-control-second"))
        .await
        .expect("enqueue the second batch");

    let Json(listed) = list_queued_work(State(state.clone()), Query(SessionQuery::default()))
        .await
        .expect("list the queued work");
    assert_eq!(
        listed
            .iter()
            .map(|batch| batch.batch_id.as_str())
            .collect::<Vec<_>>(),
        vec![first.batch_id.as_str(), second.batch_id.as_str()]
    );
    let Json(cancelled) = cancel_queued_work_batch(
        AxumPath(first.batch_id.to_string()),
        State(state.clone()),
        Query(SessionQuery::default()),
    )
    .await
    .expect("cancel the first batch");
    assert!(cancelled.accepted);
    assert_eq!(cancelled.batch_id, first.batch_id.as_str());
    let Json(remaining) = list_queued_work(State(state.clone()), Query(SessionQuery::default()))
        .await
        .expect("list after the cancel");
    assert_eq!(
        remaining
            .iter()
            .map(|batch| batch.batch_id.as_str())
            .collect::<Vec<_>>(),
        vec![second.batch_id.as_str()]
    );
    let lash::observe::SessionResume::Replayed { events } = session
        .observe()
        .resume_from_cursor(&cursor)
        .await
        .expect("resume the queue events")
    else {
        panic!("a recent cursor replays the queue events");
    };
    assert!(events.iter().any(|event| matches!(
        &event.payload,
        lash::observe::SessionObservationEventPayload::QueueChanged { kind, batch_ids }
            if *kind == lash::observe::SessionQueueEventKind::Cancelled
                && batch_ids.as_slice() == std::slice::from_ref(&first.batch_id)
    )));
    let Json(_) = cancel_queued_work_batch(
        AxumPath(second.batch_id.to_string()),
        State(state.clone()),
        Query(SessionQuery::default()),
    )
    .await
    .expect("cancel the second batch");
    drop(session);

    let timeline = include_str!("../../../assets/timeline.js");
    assert!(!timeline.contains("Run only this queued-work batch now"));
    assert!(timeline.contains("Cancel this pending queued-work batch"));
    gate.release(1);
    wait_for_turn_released(state, &session_id, &foreground, Duration::from_secs(30)).await;
    workbench.shutdown().await;
}

/// The committed turn-reply rows of `snapshot` that contain `text`.
fn reply_rows(snapshot: &StateReadSnapshot, text: &str) -> Vec<String> {
    snapshot
        .transcript
        .iter()
        .filter(|row| {
            row.suppressed.is_none()
                && row.provenance.is_turn_reply
                && row.content.text.contains(text)
        })
        .map(|row| serde_json::to_string(&row.row_id).expect("encode the row id"))
        .collect()
}

/// Read the page until its transcript shows a committed reply containing
/// `text`, and answer that snapshot.
async fn await_committed_reply(state: &AppState, text: &str) -> StateReadSnapshot {
    tokio::time::timeout(Duration::from_secs(30), async {
        loop {
            let snapshot = read_state(state, None).await.expect("read the state");
            if !reply_rows(&snapshot, text).is_empty() {
                return snapshot;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("the wake turn's reply is committed and rendered")
}

/// Press the button the session registered a trigger on: the run the press
/// starts emits, and its emission wakes the session.
async fn wake_the_session(state: &AppState) {
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
    let press = state
        .messages_snapshot()
        .into_iter()
        .find(|message| message.role == "event")
        .expect("the press is one row");
    let Some(ChatMessageProvenance::TriggerOccurrence { process_ids, .. }) = press.provenance
    else {
        panic!("the press row names its occurrence");
    };
    assert_eq!(process_ids.len(), 1, "the press started one run");
}

/// A wake turn ends on its prose reply, which the runtime commits itself:
/// the reply is committed once and the page renders that committed copy
/// once (FIG-984).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn wake_turn_leaves_exactly_one_agent_reply_committed_and_rendered() {
    const WAKE_REPLY: &str = "You pressed the Red button!";
    let workbench = Workbench::builder(scripted_cells_provider(vec![
        super::reset_chat_tests::BUTTON_TRIGGER_REGISTRATION.to_string(),
        WAKE_REPLY.to_string(),
    ]))
    .build()
    .await;
    let state = &workbench.state;
    let session_id = state.current_session_id();
    run_turn(state, "register the button trigger").await;
    wake_the_session(state).await;
    let snapshot = await_committed_reply(state, WAKE_REPLY).await;

    let committed = state
        .core
        .session(session_id.clone())
        .durable()
        .await
        .expect("bind the durable session")
        .read()
        .await
        .expect("read the committed session")
        .expect("the session has committed state");
    let committed_replies = committed
        .transcript()
        .expect("a valid committed history")
        .visible()
        .filter(|row| row.provenance.is_turn_reply && row.content.text.contains(WAKE_REPLY))
        .map(|row| serde_json::to_string(&row.row_id).expect("encode the row id"))
        .collect::<Vec<_>>();
    assert_eq!(
        committed_replies.len(),
        1,
        "a wake turn commits its reply once: {committed_replies:?}"
    );
    assert_eq!(
        reply_rows(&snapshot, WAKE_REPLY),
        committed_replies,
        "the page renders the committed copy once"
    );
    workbench.shutdown().await;
}

/// A wake turn does not retract the answer before it: its cause commits as
/// an event message, and the previous turn's reasoned reply, which the RLM
/// protocol committed itself, stays rendered beside the wake's (FIG-1406).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_wake_turn_leaves_the_previous_reasoned_reply_rendered() {
    const REASONED_REPLY: &str = "FIG-1406 reasoned send answer";
    const WAKE_REPLY: &str = "FIG-1406 wake answer";
    let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let provider = lash::testing::TestProvider::builder()
        .kind("workbench-harness")
        .complete(move |_| {
            let call = calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            let response = match call {
                0 => text_response(super::reset_chat_tests::BUTTON_TRIGGER_REGISTRATION),
                1 => {
                    let mut response = text_response(REASONED_REPLY);
                    response.parts.insert(
                        0,
                        lash::direct::LlmOutputPart::Reasoning {
                            text: "FIG-1406 send reasoning".to_string(),
                            replay: None,
                        },
                    );
                    response
                }
                _ => text_response(WAKE_REPLY),
            };
            async move { Ok(response) }
        })
        .build()
        .into_handle();
    let workbench = Workbench::builder(provider).build().await;
    let state = &workbench.state;
    let session_id = state.current_session_id();
    run_turn(state, "register the button trigger").await;
    run_turn(state, "answer with reasoning").await;
    wake_the_session(state).await;
    let snapshot = await_committed_reply(state, WAKE_REPLY).await;

    let committed = state
        .core
        .session(session_id)
        .durable()
        .await
        .expect("bind the durable session")
        .read()
        .await
        .expect("read the committed session")
        .expect("the session has committed state");
    assert!(
        committed
            .messages()
            .iter()
            .any(|message| lash::message_role(message) == "event"),
        "the wake turn commits its cause as an event message"
    );
    let replies = snapshot
        .transcript
        .iter()
        .filter(|row| row.suppressed.is_none() && row.provenance.is_turn_reply)
        .map(|row| row.content.text.clone())
        .collect::<Vec<_>>();
    assert_eq!(
        replies,
        vec![
            "registered".to_string(),
            REASONED_REPLY.to_string(),
            WAKE_REPLY.to_string()
        ],
        "the send's reasoned answer survives the wake that followed it"
    );
    workbench.shutdown().await;
}
