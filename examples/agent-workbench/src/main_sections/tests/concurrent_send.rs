/// The lease owner a Restate turn workflow opens its session with, mirrored
/// here so a test-driven turn contends for the session exactly as the workflow
/// does.
fn test_turn_execution_owner(turn_id: &str) -> lash::persistence::LeaseOwnerIdentity {
    let owner_id = format!("WorkbenchTurnWorkflow/{turn_id}/run");
    lash::persistence::LeaseOwnerIdentity::opaque(
        owner_id.clone(),
        format!("{owner_id}/test-incarnation"),
    )
}

/// The body of `restate::run_user_turn`, minus the Restate effect controller the
/// in-process test host does not need: open the session under the turn's
/// execution owner, run the turn, and publish its outcome.
async fn run_workbench_turn_attempt(
    state: &AppState,
    session_id: &str,
    turn_id: &str,
    text: &str,
) -> Result<(), AppError> {
    let session = state
        .core
        .session(session_id.to_string())
        .session_execution_owner(test_turn_execution_owner(turn_id))
        .open()
        .await
        .map_err(AppError::session_open)?;
    let turn_state = Arc::new(Mutex::new(TurnStreamState::default()));
    let ui_events = ChannelTurnEvents {
        turn_state: Arc::clone(&turn_state),
    };
    let output = session
        .turn(lash::TurnInput::text(text))
        .turn_id(turn_id.to_string())
        .require_finish()
        .expect("require finish")
        .stream_to(&ui_events)
        .await
        .map_err(AppError::runtime)?;
    crate::restate::record_turn_output(
        state,
        &session,
        turn_id,
        output,
        turn_state,
        "test.workbench_turn.completed",
    )
    .await
}

fn product_user_rows(state: &AppState, session_id: &str) -> Vec<(String, String)> {
    state
        .event_tx
        .snapshot(session_id)
        .events
        .into_iter()
        .filter_map(|event| match event.item {
            StreamItem::Message { message } if message.role == "user" => {
                Some((message.id, message.text))
            }
            StreamItem::Message { .. } | StreamItem::TurnInput { .. } | StreamItem::Done { .. } => {
                None
            }
        })
        .collect()
}

fn product_event_rows(state: &AppState, session_id: &str) -> Vec<(String, String)> {
    state
        .event_tx
        .snapshot(session_id)
        .events
        .into_iter()
        .filter_map(|event| match event.item {
            StreamItem::Message { message } if message.role == "event" => {
                Some((message.id, message.text))
            }
            StreamItem::Message { .. } | StreamItem::TurnInput { .. } | StreamItem::Done { .. } => {
                None
            }
        })
        .collect()
}

fn product_ingress_receipts(state: &AppState, session_id: &str) -> Vec<TurnInputReceipt> {
    state
        .event_tx
        .snapshot(session_id)
        .events
        .into_iter()
        .filter_map(|event| match event.item {
            StreamItem::TurnInput { receipt } => Some(receipt),
            StreamItem::Message { .. } | StreamItem::Done { .. } => None,
        })
        .collect()
}

fn turn_input_text(input: &lash::TurnInput) -> String {
    input
        .items
        .iter()
        .filter_map(|item| match item {
            lash::InputItem::Text { text } => Some(text.clone()),
            lash::InputItem::Attachment { .. } => None,
        })
        .collect::<Vec<_>>()
        .join(" ")
}

fn state_rows(snapshot: &StateReadSnapshot) -> Vec<(String, String)> {
    snapshot
        .transcript
        .iter()
        .filter_map(|row| match row {
            TranscriptRow::Message { message } => {
                Some((message.role.clone(), message.text.clone()))
            }
            TranscriptRow::Reasoning { .. } | TranscriptRow::CodeBlock { .. } => None,
        })
        .collect()
}

/// A provider whose first call parks until released, so a turn can be held
/// mid-flight while the routes under test are exercised against a busy session.
fn gated_first_call_provider(
    kind: &'static str,
) -> (
    ProviderHandle,
    mpsc::UnboundedReceiver<usize>,
    Arc<tokio::sync::Notify>,
) {
    let (entered_tx, entered_rx) = mpsc::unbounded_channel();
    let release = Arc::new(tokio::sync::Notify::new());
    let release_for_provider = Arc::clone(&release);
    let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let provider = lash::testing::TestProvider::builder()
        .kind(kind)
        .complete(move |_| {
            let entered_tx = entered_tx.clone();
            let release = Arc::clone(&release_for_provider);
            let calls = Arc::clone(&calls);
            async move {
                let call = calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                let _ = entered_tx.send(call);
                if call == 0 {
                    release.notified().await;
                }
                Ok(text_response(&format!(
                    "<lashlang>\nfinish \"answer {call}\"\n</lashlang>"
                )))
            }
        })
        .build()
        .into_handle();
    (provider, entered_rx, release)
}

/// The workbench's own queued-work wiring, which the default test state leaves
/// out: production supplies `WorkbenchQueuedWorkSubmitter`, so lash installs no
/// inline queued-work runner and the drain of a deferred next-turn input is the
/// workbench's Restate queued-turn workflow. A state built without a driver gets
/// lash's `InlineQueuedWorkRunHandle` instead, which drains the input itself
/// without the workbench in the loop — a shape the workbench never runs in.
async fn queued_send_test_state(
    data_dir: &std::path::Path,
    provider: ProviderHandle,
) -> AppState {
    let store_factory: Arc<dyn lash::persistence::SessionStoreFactory> = Arc::new(
        lash_sqlite_store::SqliteSessionStoreFactory::new(data_dir.join("lash-sessions")),
    );
    recoverable_chat_test_state_with_dependencies(
        data_dir,
        64,
        provider,
        in_memory_trigger_store(),
        store_factory,
        Some(inert_queued_work_driver()),
    )
    .await
}

/// FIG-1000: a second client's send while a turn is running must get an honest
/// admission outcome. `/api/turn` used to answer `200 {"accepted":true}`, start
/// a second concurrent turn, and broadcast an optimistic user row for it — after
/// which the durable fence refused one of the two turns and the surface reported
/// nothing. The send is now admitted as the next turn's input, so the message is
/// kept, rendered as a queued receipt, and answered by the drained turn.
#[tokio::test]
async fn a_send_to_a_busy_session_is_admitted_as_a_queued_next_turn_input() {
    let data_dir = tempfile::tempdir().expect("queued send tempdir");
    let (provider, mut provider_entered, release) =
        gated_first_call_provider("workbench-queued-concurrent-send");
    let mut state = queued_send_test_state(data_dir.path(), provider).await;
    let (restate_ingress_url, mut restate_requests) = spawn_restate_ingress_capture().await;
    state.restate_ingress_url = restate_ingress_url;
    let session_id = state.current_session_id();

    let Json(first) = send_turn(
        State(state.clone()),
        Query(SessionQuery::default()),
        Json(TurnRequest {
            text: "first send".to_string(),
            model: Some("test-model".to_string()),
            model_variant: None,
            attachment_id: None,
        }),
    )
    .await
    .expect("first send admitted");
    assert!(first.accepted, "the first send starts a turn");
    assert!(
        !first.queued,
        "an idle session runs the send now: {first:?}"
    );
    let submitted = tokio::time::timeout(Duration::from_secs(5), restate_requests.recv())
        .await
        .expect("first send reaches Restate")
        .expect("first send payload");
    let first_turn_id = submitted
        .pointer("/body/turn_id")
        .and_then(Value::as_str)
        .expect("first turn id")
        .to_string();

    let running = tokio::spawn({
        let state = state.clone();
        let session_id = session_id.clone();
        let first_turn_id = first_turn_id.clone();
        async move {
            let result =
                run_workbench_turn_attempt(&state, &session_id, &first_turn_id, "first send").await;
            crate::restate::terminalize_turn_execution(
                &state,
                &session_id,
                &first_turn_id,
                "test.workbench_turn.failed",
                Ok(result),
            )
            .await
        }
    });
    assert_eq!(
        tokio::time::timeout(Duration::from_secs(5), provider_entered.recv())
            .await
            .expect("the first turn reaches the provider"),
        Some(0),
        "the first turn must be parked in the provider while the second send arrives"
    );

    let Json(second) = send_turn(
        State(state.clone()),
        Query(SessionQuery::default()),
        Json(TurnRequest {
            text: "second send".to_string(),
            model: Some("test-model".to_string()),
            model_variant: None,
            attachment_id: None,
        }),
    )
    .await
    .expect("a send to a busy session is admitted, not dropped");
    assert!(second.accepted, "the queued send is still accepted");
    assert!(
        second.queued,
        "a send to a session with a running turn must report that it was queued: {second:?}"
    );
    let receipt = second
        .queued_input
        .as_ref()
        .expect("a queued send carries its ingress receipt");
    assert_eq!(receipt.text, "second send");
    assert_eq!(
        receipt.ingress,
        lash::persistence::TurnInputIngress::NextTurn
    );
    assert_eq!(
        receipt.state,
        lash::persistence::TurnInputState::DeferredNextTurn
    );
    assert!(
        matches!(restate_requests.try_recv(), Err(mpsc::error::TryRecvError::Empty)),
        "a queued send must not submit a second concurrent turn workflow"
    );
    assert_eq!(
        state.active_turns.for_session(&session_id).len(),
        1,
        "a queued send must not register a second active turn"
    );

    assert_eq!(
        product_user_rows(&state, &session_id)
            .into_iter()
            .map(|(_, text)| text)
            .collect::<Vec<_>>(),
        vec!["first send".to_string()],
        "a queued send must not broadcast an optimistic user row it never committed"
    );
    let receipts = product_ingress_receipts(&state, &session_id);
    assert_eq!(
        receipts
            .iter()
            .map(|receipt| receipt.text.clone())
            .collect::<Vec<_>>(),
        vec!["second send".to_string()],
        "every viewer must see the queued send as an ingress receipt"
    );

    let Json(mid_turn) = app_state(State(state.clone()), Query(SessionQuery::default()))
        .await
        .expect("mid-turn snapshot");
    assert_eq!(
        mid_turn
            .pending_turn_inputs
            .iter()
            .map(|input| turn_input_text(&input.input))
            .collect::<Vec<_>>(),
        vec!["second send".to_string()],
        "the queued send must be durably pending, not held in browser memory"
    );
    assert_eq!(
        state_rows(&mid_turn)
            .into_iter()
            .filter(|(role, _)| role == "user")
            .map(|(_, text)| text)
            .collect::<Vec<_>>(),
        vec!["first send".to_string()],
        "the mid-turn projection must carry exactly the running turn's user row"
    );

    release.notify_one();
    running
        .await
        .expect("running turn task")
        .expect("the first turn completes");

    let Json(settled) = app_state(State(state.clone()), Query(SessionQuery::default()))
        .await
        .expect("settled snapshot");
    assert_eq!(
        state_rows(&settled),
        vec![
            ("user".to_string(), "first send".to_string()),
            ("assistant".to_string(), "answer 0".to_string()),
        ],
        "the first turn settles to exactly its own committed pair"
    );
    assert_eq!(
        settled.pending_turn_inputs.len(),
        1,
        "the queued send survives the running turn's settlement"
    );

    // What `WorkbenchQueuedTurnWorkflow` does when the submitter's drain fires
    // after terminalization: run the queued input as its own turn and publish
    // its outcome. The queued send becomes a committed user message and gets an
    // answer, so queueing it lost nothing.
    let session = state
        .core
        .session(session_id.clone())
        .open()
        .await
        .expect("open drain session");
    let drained = session
        .queued_turn()
        .drain_id("test-drained-queued-turn")
        .run()
        .await
        .expect("drain the queued send")
        .expect("the queued send runs as a turn");
    state.track_turn(&session_id, "test-drained-queued-turn");
    crate::restate::record_turn_output(
        &state,
        &session,
        "test-drained-queued-turn",
        drained.result,
        Arc::new(Mutex::new(TurnStreamState::default())),
        "test.drained_queued_turn.completed",
    )
    .await
    .expect("record the drained turn");
    crate::restate::settle_workbench_turn(&state, &session_id, "test-drained-queued-turn")
        .await
        .expect("settle the drained turn");
    drop(session);

    let Json(drained_state) = app_state(State(state.clone()), Query(SessionQuery::default()))
        .await
        .expect("drained snapshot");
    assert_eq!(
        state_rows(&drained_state),
        vec![
            ("user".to_string(), "first send".to_string()),
            ("assistant".to_string(), "answer 0".to_string()),
            ("user".to_string(), "second send".to_string()),
            ("assistant".to_string(), "answer 1".to_string()),
        ],
        "the queued send is answered as its own turn, once, in order"
    );
    assert!(
        drained_state.pending_turn_inputs.is_empty(),
        "the drained input leaves the pending lane"
    );
}

/// FIG-1000: the admission check in the handler is advisory — the session
/// execution lease and the commit CAS are the authority — so a send that wins
/// admission can still lose the commit race. When it does, the loss must be
/// visible: a failure row every viewer sees, and no optimistic user row left
/// standing for a turn that committed nothing.
#[tokio::test]
async fn a_turn_that_loses_the_commit_race_surfaces_its_failure_and_retires_its_row() {
    let data_dir = tempfile::tempdir().expect("losing race tempdir");
    let (provider, mut provider_entered, release) =
        gated_first_call_provider("workbench-losing-commit-race");
    let mut state = queued_send_test_state(data_dir.path(), provider).await;
    let (restate_ingress_url, mut restate_requests) = spawn_restate_ingress_capture().await;
    state.restate_ingress_url = restate_ingress_url;
    let session_id = state.current_session_id();

    let Json(_) = send_turn(
        State(state.clone()),
        Query(SessionQuery::default()),
        Json(TurnRequest {
            text: "admitted send".to_string(),
            model: Some("test-model".to_string()),
            model_variant: None,
            attachment_id: None,
        }),
    )
    .await
    .expect("send admitted");
    let submitted = tokio::time::timeout(Duration::from_secs(5), restate_requests.recv())
        .await
        .expect("send reaches Restate")
        .expect("send payload");
    let turn_id = submitted
        .pointer("/body/turn_id")
        .and_then(Value::as_str)
        .expect("turn id")
        .to_string();

    let losing = tokio::spawn({
        let state = state.clone();
        let session_id = session_id.clone();
        let turn_id = turn_id.clone();
        async move {
            let result =
                run_workbench_turn_attempt(&state, &session_id, &turn_id, "admitted send").await;
            crate::restate::terminalize_turn_execution(
                &state,
                &session_id,
                &turn_id,
                "restate_user_turn.failed",
                Ok(result),
            )
            .await
        }
    });
    assert_eq!(
        tokio::time::timeout(Duration::from_secs(5), provider_entered.recv())
            .await
            .expect("the admitted turn reaches the provider"),
        Some(0)
    );
    assert_eq!(
        product_user_rows(&state, &session_id)
            .into_iter()
            .map(|(_, text)| text)
            .collect::<Vec<_>>(),
        vec!["admitted send".to_string()],
        "the admitted send renders optimistically while it runs"
    );

    // A competing executor of the same session — a racing sibling the advisory
    // admission check could not see — commits first and takes the head revision.
    let competitor = state
        .core
        .session(session_id.clone())
        .session_execution_owner(test_turn_execution_owner("competing-turn"))
        .open()
        .await
        .expect("open competing session");
    competitor
        .turn(lash::TurnInput::text("competing send"))
        .turn_id("competing-turn")
        .require_finish()
        .expect("require finish")
        .run()
        .await
        .expect("the competing turn commits first");
    drop(competitor);

    release.notify_one();
    let terminalized = losing.await.expect("losing turn task");
    assert!(
        terminalized.is_err(),
        "a turn refused by the durable fence must terminalize as a failure"
    );

    let failure_rows = product_event_rows(&state, &session_id);
    assert_eq!(
        failure_rows
            .iter()
            .filter(|(id, _)| id == &format!("turn:{turn_id}:failed"))
            .map(|(_, text)| text.as_str())
            .collect::<Vec<_>>(),
        vec![PUBLIC_TURN_FAILURE_MESSAGE],
        "the refused turn must render a failure row in every viewer: {failure_rows:?}"
    );
    let done = state
        .event_tx
        .snapshot(&session_id)
        .events
        .into_iter()
        .filter_map(|event| match event.item {
            StreamItem::Done { turn_id, outcome } => Some((turn_id, outcome)),
            StreamItem::Message { .. } | StreamItem::TurnInput { .. } => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(
        done,
        vec![(Some(turn_id.clone()), TurnDoneOutcome::Failed)],
        "the turn's done event must name the failure so every viewer stops claiming it ran"
    );
    assert!(
        product_user_rows(&state, &session_id).is_empty(),
        "the refused turn's optimistic user row must retire: it committed nothing"
    );
    assert!(
        state.active_turns.for_session(&session_id).is_empty(),
        "the refused turn must not stay active"
    );

    let Json(settled) = app_state(State(state.clone()), Query(SessionQuery::default()))
        .await
        .expect("settled snapshot");
    assert_eq!(
        state_rows(&settled)
            .into_iter()
            .filter(|(role, _)| role == "user")
            .map(|(_, text)| text)
            .collect::<Vec<_>>(),
        vec!["competing send".to_string()],
        "the settled projection carries only the turn that actually committed"
    );
    assert!(
        state_rows(&settled)
            .iter()
            .all(|(_, text)| text != "admitted send"),
        "no phantom row may survive for a turn whose commit was refused"
    );
}

/// The browser contract for both halves of FIG-1000: a queued send renders its
/// receipt instead of a user row, and a failed turn's `done` rebuilds the
/// transcript from the authoritative snapshot so the retired row disappears from
/// every tab that already rendered it.
#[test]
fn workbench_ui_renders_queued_sends_and_failed_turn_reconciliation() {
    assert!(ui::INDEX_HTML.contains("accepted?.queued"));
    assert!(ui::INDEX_HTML.contains("renderIngressReceipt(accepted.queued_input)"));
    assert!(ui::INDEX_HTML.contains("event.outcome === \"failed\""));
    assert!(ui::INDEX_HTML.contains("queued next · waiting for the running turn"));
}
