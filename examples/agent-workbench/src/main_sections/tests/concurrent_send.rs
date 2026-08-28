/// The body of `restate::run_user_turn`, minus the Restate effect controller the
/// in-process test host does not need.
async fn run_workbench_turn_attempt(
    state: &AppState,
    session_id: &str,
    turn_id: &str,
    text: &str,
) -> Result<(), AppError> {
    let session = state
        .core
        .session(session_id.to_string())
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

async fn run_workbench_turn_attempt_with_error_evidence(
    state: &AppState,
    session_id: &str,
    turn_id: &str,
    text: &str,
) -> (
    Result<(), AppError>,
    Option<(lash::runtime::RuntimeErrorCode, String)>,
) {
    let session = match state.core.session(session_id.to_string()).open().await {
        Ok(session) => session,
        Err(error) => return (Err(AppError::session_open(error)), None),
    };
    let turn_state = Arc::new(Mutex::new(TurnStreamState::default()));
    let ui_events = ChannelTurnEvents {
        turn_state: Arc::clone(&turn_state),
    };
    match session
        .turn(lash::TurnInput::text(text))
        .turn_id(turn_id.to_string())
        .require_finish()
        .expect("require finish")
        .stream_to(&ui_events)
        .await
    {
        Ok(output) => (
            crate::restate::record_turn_output(
                state,
                &session,
                turn_id,
                output,
                turn_state,
                "test.workbench_turn.completed",
            )
            .await,
            None,
        ),
        Err(lash::EmbedError::Runtime(error)) => {
            let evidence = Some((error.code.clone(), error.message.clone()));
            (Err(AppError::runtime(lash::EmbedError::Runtime(error))), evidence)
        }
        Err(error) => (Err(AppError::runtime(error)), None),
    }
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
            StreamItem::Message { .. }
            | StreamItem::TurnInput { .. }
            | StreamItem::ModelCallRecorded { .. }
            | StreamItem::Done { .. } => {
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
            StreamItem::Message { .. }
            | StreamItem::TurnInput { .. }
            | StreamItem::ModelCallRecorded { .. }
            | StreamItem::Done { .. } => {
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
            StreamItem::Message { .. }
            | StreamItem::ModelCallRecorded { .. }
            | StreamItem::Done { .. } => None,
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

/// ADR 0077: a replacement worker is refused while a dead holder's lease is
/// still live, then admits and completes after the TTL expires.
#[tokio::test]
async fn new_turn_waits_for_dead_lease_ttl_before_admission() {
    let data_dir = tempfile::tempdir().expect("successor persistence tempdir");
    let store_factory = Arc::new(lash_sqlite_store::SqliteSessionStoreFactory::new(
        data_dir.path().join("lash-sessions"),
    ));
    let provider = lash::testing::TestProvider::builder()
        .kind("workbench-successor-persistence")
        .complete(|_| async {
            Ok(text_response(
                "<lashlang>\nfinish \"replacement completed\"\n</lashlang>",
            ))
        })
        .build()
        .into_handle();
    let state = recoverable_chat_test_state_with_dependencies(
        data_dir.path(),
        64,
        provider,
        in_memory_trigger_store(),
        store_factory.clone(),
        Some(inert_queued_work_port()),
    )
    .await;
    let session_id = state.current_session_id();
    let turn_id = "fig1133-new-turn";
    let dead_incarnation = lash::persistence::LeaseOwnerIdentity::opaque(
        "legacy-workbench-turn-17/run",
        "legacy-workbench-turn-17/dead-boot",
    );

    // Materialize and park the durable session before incarnation A claims the
    // workflow lane. Dropping the acquisition value simulates process loss: it
    // performs no owner-side release, so the durable lease remains live.
    let parked = state
        .core
        .session(session_id.clone())
        .open()
        .await
        .expect("open the first incarnation")
        .park()
        .await
        .expect("park the materialized session");
    assert_eq!(parked.session_id(), session_id);
    drop(parked);
    let store = lash_sqlite_store::Store::open(&store_factory.catalog_path())
        .await
        .expect("open the durable session catalog");
    let dead_lease = lash::persistence::SessionExecutionLeaseStore::try_claim_session_execution_lease(
        &store,
        &session_id,
        &dead_incarnation,
        "new-turn-within-dead-lease-ttl-commits-under-head-cas-executor",
        100,
    )
    .await
    .expect("incarnation A claims the session lane")
    .acquired()
    .expect("the parked session lane is free for incarnation A");

    let before_expiry = state.core.session(session_id.clone()).open().await;
    assert!(
        matches!(before_expiry, Err(ref error) if error.to_string().contains("store commit is contended")),
        "replacement recovery must wait for the dead holder TTL"
    );
    tokio::time::sleep(Duration::from_millis(150)).await;

    let successor = state
        .core
        .session(session_id.clone())
        .open()
        .await
        .expect("open the replacement incarnation");
    let turn_state = Arc::new(Mutex::new(TurnStreamState::default()));
    let ui_events = ChannelTurnEvents {
        turn_state: Arc::clone(&turn_state),
    };
    let output = successor
        .turn(lash::TurnInput::text("complete after process loss"))
        .turn_id(turn_id)
        .require_finish()
        .expect("require finish")
        .stream_to(&ui_events)
        .await
        .expect("the replacement runtime completes after lease takeover");
    crate::restate::record_turn_output(
        &state,
        &successor,
        turn_id,
        output,
        turn_state,
        "test.fig1129.successor.completed",
    )
    .await
    .expect("the replacement persists the workbench-owned assistant projection");

    let committed = successor
        .read_view()
        .messages()
        .iter()
        .map(lash::message_text)
        .collect::<Vec<_>>();
    assert!(
        committed.iter().any(|text| text == "complete after process loss"),
        "the successor's user turn must be durable: {committed:?}"
    );
    assert!(
        committed.iter().any(|text| text == "replacement completed"),
        "the successor's final workbench projection must be durable: {committed:?}"
    );
    let Json(projected) = app_state(State(state.clone()), Query(SessionQuery::default()))
        .await
        .expect("project the completed successor turn");
    assert!(
        state_rows(&projected)
            .iter()
            .any(|(role, text)| role == "assistant" && text == "replacement completed"),
        "the user-facing projection must show the completed turn: {:?}",
        state_rows(&projected)
    );

    // After takeover and completion, ordinary administration can reopen and
    // append against the current generation.
    let contender = state
        .core
        .session(session_id.clone())
        .open()
        .await
        .expect("open the replacement worker runtime");
    contender
        .admin()
        .state()
        .append_messages(vec![lash::plugins::PluginMessage::text(
            lash::messages::MessageRole::Assistant,
            "append committed under head CAS",
        )])
        .await
        .expect("the post-takeover append commits");
    let durable_after_append = lash::persistence::load_persisted_session_state(&store)
        .await
        .expect("re-read durable session after lane-less append")
        .expect("the durable session remains present");
    assert!(
        durable_after_append
            .read_view()
            .messages()
            .iter()
            .any(|message| lash::message_text(message) == "append committed under head CAS"),
        "the post-takeover append must be fully durable"
    );

    // Keep the exact pre-TTL evidence live through the takeover assertions.
    assert_eq!(dead_lease.owner, dead_incarnation);
}

/// ADR 0077 restart arm: even the same stable worker owner must wait for the
/// dead boot incarnation's live lease to expire before recovery.
#[tokio::test]
async fn same_worker_successor_waits_for_dead_boot_ttl() {
    let data_dir = tempfile::tempdir().expect("same-turn successor tempdir");
    let store_factory = Arc::new(lash_sqlite_store::SqliteSessionStoreFactory::new(
        data_dir.path().join("lash-sessions"),
    ));
    let provider = lash::testing::TestProvider::builder()
        .kind("workbench-same-turn-successor")
        .complete_error("the append-only restart gate must not call the provider")
        .build()
        .into_handle();
    let state = recoverable_chat_test_state_with_dependencies(
        data_dir.path(),
        64,
        provider,
        in_memory_trigger_store(),
        store_factory.clone(),
        Some(inert_queued_work_port()),
    )
    .await;
    let session_id = state.current_session_id();
    let session = state
        .core
        .session(session_id.clone())
        .open()
        .await
        .expect("materialize restart-gate session");
    session
        .park()
        .await
        .expect("park restart-gate session before simulating process loss");

    let store = lash_sqlite_store::Store::open(&store_factory.catalog_path())
        .await
        .expect("open restart-gate store");
    let dead_boot = lash::persistence::LeaseOwnerIdentity::opaque(
        "agent-workbench-test-worker",
        "agent-workbench-dead-boot",
    );
    let dead_lease = lash::persistence::SessionExecutionLeaseStore::try_claim_session_execution_lease(
        &store,
        &session_id,
        &dead_boot,
        "same-turn-successor-within-dead-lease-ttl-commits-under-head-cas-executor",
        100,
    )
    .await
    .expect("dead boot claims the lane")
    .acquired()
    .expect("restart-gate lane starts free");

    let before_expiry = state.core.session(session_id.clone()).open().await;
    assert!(
        matches!(before_expiry, Err(ref error) if error.to_string().contains("store commit is contended")),
        "the same stable owner still needs expiry of the dead incarnation"
    );
    tokio::time::sleep(Duration::from_millis(150)).await;

    let successor = state
        .core
        .session(session_id.clone())
        .open()
        .await
        .expect("open same-worker successor boot");
    successor
        .admin()
        .state()
        .append_messages(vec![lash::plugins::PluginMessage::text(
            lash::messages::MessageRole::Assistant,
            "same-turn successor committed",
        )])
        .await
        .expect("same-turn successor commits after the dead lease TTL");
    let durable = lash::persistence::load_persisted_session_state(&store)
        .await
        .expect("read same-turn successor state")
        .expect("same-turn successor state exists");
    assert!(durable.read_view().messages().iter().any(|message| {
        lash::message_text(message) == "same-turn successor committed"
    }));
    assert_eq!(dead_lease.owner, dead_boot);
}

/// Holds the first two writers to reach `session_graph_append.pre_commit`
/// until both have arrived, so their head CAS attempts genuinely overlap.
///
/// `begin_named` is a synchronous callback on a tokio worker, so the rendezvous
/// is a bounded watchdog rather than a `std::sync::Barrier`: a barrier has no
/// timeout, and if both spawned appends were ever served by one worker the test
/// would hang CI forever instead of failing. Overshooting the deadline is a real
/// defect in the gate (the overlap it exists to prove did not happen), so it
/// panics and turns the test red.
struct AppendPreCommitBarrier {
    arrivals: std::sync::atomic::AtomicUsize,
}

impl AppendPreCommitBarrier {
    const OVERLAP_DEADLINE: std::time::Duration = std::time::Duration::from_secs(30);
    const OVERLAP_POLL: std::time::Duration = std::time::Duration::from_millis(1);

    fn new() -> Self {
        Self {
            arrivals: std::sync::atomic::AtomicUsize::new(0),
        }
    }
}

impl lash::runtime::RuntimeTurnPhaseProbe for AppendPreCommitBarrier {
    fn begin(&self, _phase: lash::runtime::RuntimeTurnPhase) {}

    fn end(&self, _phase: lash::runtime::RuntimeTurnPhase) {}

    fn begin_named(&self, phase: &str) {
        if phase != "session_graph_append.pre_commit"
            || self
                .arrivals
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst)
                >= 2
        {
            return;
        }
        let deadline = std::time::Instant::now() + Self::OVERLAP_DEADLINE;
        while self.arrivals.load(std::sync::atomic::Ordering::SeqCst) < 2 {
            assert!(
                std::time::Instant::now() < deadline,
                "only one writer reached session_graph_append.pre_commit within {:?}; the append \
                 race never overlapped",
                Self::OVERLAP_DEADLINE
            );
            std::thread::sleep(Self::OVERLAP_POLL);
        }
    }
}

/// FIG-1133 Phase 6 gate: both live writers stage from the same graph and are
/// held at the pre-commit boundary. One wins the first head CAS; the loser
/// observes that exact conflict, refreshes, and appends without loss or partial
/// publication.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn two_live_writers_rebase_appends_into_durable_graph_order() {
    let data_dir = tempfile::tempdir().expect("concurrent append tempdir");
    let provider = lash::testing::TestProvider::builder()
        .kind("workbench-concurrent-append")
        .complete_error("the append gate must not call the provider")
        .build()
        .into_handle();
    let state = queued_send_test_state(data_dir.path(), provider).await;
    let session_id = state.current_session_id();
    let left = state
        .core
        .session(session_id.clone())
        .open()
        .await
        .expect("open left append writer");
    let right = state
        .core
        .session(session_id.clone())
        .open()
        .await
        .expect("open right append writer");
    let barrier = Arc::new(AppendPreCommitBarrier::new());
    let probe: Arc<dyn lash::runtime::RuntimeTurnPhaseProbe> = barrier;
    left.set_turn_phase_probe(Arc::clone(&probe)).await;
    right.set_turn_phase_probe(probe).await;

    let left_task = tokio::spawn(async move {
        let first = left
            .admin()
            .state()
            .append_messages(vec![lash::plugins::PluginMessage::text(
                lash::messages::MessageRole::Assistant,
                "fig1133-concurrent-left",
            )])
            .await;
        (left, first)
    });
    let right_task = tokio::spawn(async move {
        let first = right
            .admin()
            .state()
            .append_messages(vec![lash::plugins::PluginMessage::text(
                lash::messages::MessageRole::Assistant,
                "fig1133-concurrent-right",
            )])
            .await;
        (right, first)
    });
    let (left_result, right_result) = tokio::join!(left_task, right_task);
    let (left, left_result) = left_result.expect("left append task");
    let (right, right_result) = right_result.expect("right append task");
    assert_eq!(
        [left_result.is_ok(), right_result.is_ok()]
            .into_iter()
            .filter(|won| *won)
            .count(),
        1
    );
    let left_lost = left_result.is_err();
    let conflict = if left_lost {
        left_result.expect_err("left writer loses the first CAS")
    } else {
        right_result.expect_err("right writer loses the first CAS")
    };
    // The loser must retain the *typed* conflict, not a rendered string: a host
    // is told to refresh and retry from this outcome, and string matching cannot
    // distinguish it from any other commit failure.
    let lash::EmbedError::Session(lash::SessionError::Store {
        source: lash::persistence::StoreError::HeadRevisionConflict { expected, actual },
        ..
    }) = &conflict
    else {
        panic!("the CAS loser must surface a typed HeadRevisionConflict, got {conflict:?}");
    };
    assert_eq!(*expected, 0);
    assert_eq!(*actual, 1);
    assert_eq!(
        conflict.to_string(),
        "runtime session error: failed to persist runtime state: store head revision conflict: expected 0, actual 1"
    );
    let (loser, missing_text) = if left_lost {
        (left, "fig1133-concurrent-left")
    } else {
        (right, "fig1133-concurrent-right")
    };
    loser
        .admin()
        .state()
        .append_messages(vec![lash::plugins::PluginMessage::text(
            lash::messages::MessageRole::Assistant,
            missing_text,
        )])
        .await
        .expect("CAS loser refreshes and commits its append");

    let fresh = state
        .core
        .session(session_id)
        .open()
        .await
        .expect("reopen durable append graph");
    let ordered = fresh
        .read_view()
        .messages()
        .iter()
        .map(lash::message_text)
        .filter(|text| text.starts_with("fig1133-concurrent-"))
        .collect::<Vec<_>>();
    assert!(
        ordered == vec!["fig1133-concurrent-left", "fig1133-concurrent-right"]
            || ordered == vec!["fig1133-concurrent-right", "fig1133-concurrent-left"],
        "both literal appends must appear exactly once in durable graph order: {ordered:?}"
    );
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
/// lash's `NativeQueuedWorkRunHandle` instead, which drains the input itself
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
        Some(inert_queued_work_port()),
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

/// ADR 0077: a busy execution lane refuses competing recovery before any
/// mutable session payload is hydrated. The current holder stays authoritative
/// and completes normally once its provider resumes.
#[tokio::test]
async fn a_busy_lane_refuses_competing_recovery_without_disturbing_its_holder() {
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
            let (result, error_evidence) = Box::pin(
                run_workbench_turn_attempt_with_error_evidence(
                    &state,
                    &session_id,
                    &turn_id,
                    "admitted send",
                ),
            )
            .await;
            let terminalized = crate::restate::terminalize_turn_execution(
                &state,
                &session_id,
                &turn_id,
                "restate_user_turn.failed",
                Ok(result),
            )
            .await;
            (terminalized, error_evidence)
        }
    });
    assert_eq!(
        tokio::time::timeout(Duration::from_secs(5), provider_entered.recv())
            .await
            .expect("the admitted turn reaches the provider"),
        Some(0)
    );
    let holder_before_race = state
        .core
        .session_lease_diagnostics(&session_id)
        .await
        .expect("read stalled holder")
        .expect("stalled turn materialized its lease")
        .holder
        .expect("stalled turn holds the lane");
    assert_eq!(holder_before_race.owner.owner_id, "agent-workbench-test-worker");
    assert_eq!(
        holder_before_race.owner.incarnation_id,
        "agent-workbench-test-boot"
    );
    assert_eq!(
        product_user_rows(&state, &session_id)
            .into_iter()
            .map(|(_, text)| text)
            .collect::<Vec<_>>(),
        vec!["admitted send".to_string()],
        "the admitted send renders optimistically while it runs"
    );

    let competitor_error = match state
        .core
        .session(session_id.clone())
        .open()
        .await
    {
        Ok(_) => panic!("a busy lane must refuse competing recovery"),
        Err(error) => error,
    };
    assert!(
        competitor_error.to_string().contains("store commit is contended"),
        "the refusal must identify the busy admission lane: {competitor_error}"
    );
    let holder_after_race = state
        .core
        .session_lease_diagnostics(&session_id)
        .await
        .expect("read holder after refused competitor")
        .expect("stalled holder row remains present")
        .holder
        .expect("stalled holder remains current");
    assert_eq!(holder_after_race, holder_before_race);

    release.notify_one();
    let (terminalized, error_evidence) = losing.await.expect("losing turn task");
    assert_eq!(error_evidence, None);
    assert!(terminalized.is_ok(), "the admitted holder must complete");

    let failure_rows = product_event_rows(&state, &session_id);
    assert!(
        failure_rows
            .iter()
            .all(|(id, _)| id != &format!("turn:{turn_id}:failed")),
        "refusing the competitor must not fail the admitted holder: {failure_rows:?}"
    );
    let done = state
        .event_tx
        .snapshot(&session_id)
        .events
        .into_iter()
        .filter_map(|event| match event.item {
            StreamItem::Done { turn_id, outcome } => Some((turn_id, outcome)),
            StreamItem::Message { .. }
            | StreamItem::TurnInput { .. }
            | StreamItem::ModelCallRecorded { .. } => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(
        done,
        vec![(Some(turn_id.clone()), TurnDoneOutcome::Completed)],
        "the admitted holder completes exactly once"
    );
    assert!(
        product_user_rows(&state, &session_id)
            .iter()
            .any(|(_, text)| text == "admitted send"),
        "the admitted holder's committed user row remains visible"
    );
    assert!(
        state.active_turns.for_session(&session_id).is_empty(),
        "the completed holder must not stay active"
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
        vec!["admitted send".to_string()],
        "the settled projection carries the admitted holder"
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
