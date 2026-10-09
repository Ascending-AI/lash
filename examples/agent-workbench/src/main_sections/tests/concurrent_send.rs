use super::*;
use lash::SessionId;
use lash::TurnId;

#[test]
fn active_turn_idle_claim_is_atomic_per_session() {
    let active_turns = ActiveTurns::default();
    let start = Arc::new(std::sync::Barrier::new(3));
    let claims = std::thread::scope(|scope| {
        let left = scope.spawn({
            let active_turns = active_turns.clone();
            let start = Arc::clone(&start);
            move || {
                start.wait();
                active_turns
                    .try_insert_for_idle_session(
                        &SessionId::from("race-session"),
                        &TurnId::from("left"),
                        WorkbenchTurnKind::User,
                    )
                    .is_claimed()
            }
        });
        let right = scope.spawn({
            let active_turns = active_turns.clone();
            let start = Arc::clone(&start);
            move || {
                start.wait();
                active_turns
                    .try_insert_for_idle_session(
                        &SessionId::from("race-session"),
                        &TurnId::from("right"),
                        WorkbenchTurnKind::User,
                    )
                    .is_claimed()
            }
        });
        start.wait();
        [
            left.join().expect("left claim"),
            right.join().expect("right claim"),
        ]
    });
    assert_eq!(claims.into_iter().filter(|claimed| *claimed).count(), 1);
    // One winner, and the slot is a lookup rather than a scan: the ledger is
    // keyed by session, so a second claim cannot land beside the first.
    assert!(
        active_turns
            .for_session(&SessionId::from("race-session"))
            .is_some()
    );
}

/// A host append is a session command applied at a turn boundary, so two
/// live writers' appends and a keyed append made at the same moment never
/// race a head CAS: each is applied and settled, none surfaces a conflict
/// for its host to repair, and the keyed append's stable message id lands
/// exactly once (FIG-4202).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn concurrent_host_appends_settle_through_the_command_lane_exactly_once() {
    let workbench = Workbench::silent().await;
    let state = &workbench.state;
    let session_id = state.current_session_id();
    let mut writers = Vec::new();
    for _ in 0..3 {
        writers.push(
            state
                .open_session(&session_id, "test")
                .await
                .expect("open an appending writer"),
        );
    }
    let reply_writer = writers.pop().expect("reply writer");
    let right = writers.pop().expect("right writer");
    let left = writers.pop().expect("left writer");
    let append = |writer: lash::LashSession, text: &'static str| {
        tokio::spawn(async move {
            writer
                .admin()
                .state()
                .append_messages(
                    vec![lash::plugins::PluginMessage::text(
                        lash::messages::MessageRole::Assistant,
                        text,
                    )],
                    format!("concurrent-host-append:{text}"),
                )
                .await?
                .settle_with(
                    &writer.admin().commands(),
                    lash::testing::admin_fixture_outcome,
                )
                .await
        })
    };
    let left_task = append(left, "fig4202-concurrent-left");
    let right_task = append(right, "fig4202-concurrent-right");
    let reply_id = "fig4202-keyed-append".to_string();
    let reply_task = {
        let reply_id = reply_id.clone();
        tokio::spawn(async move {
            reply_writer
                .admin()
                .state()
                .append_session_nodes(lash::plugins::AppendSessionNodesRequest {
                    operation_id: reply_id.clone(),
                    nodes: vec![lash::plugins::SessionAppendNode::message(
                        lash::plugins::PluginMessage::text(
                            lash::messages::MessageRole::Assistant,
                            "the keyed host append",
                        )
                        .with_id(reply_id),
                    )],
                    requires_ancestor_node_id: None,
                })
                .await?
                .settle_with(
                    &reply_writer.admin().commands(),
                    lash::testing::admin_fixture_outcome,
                )
                .await
        })
    };
    let (left_result, right_result, reply_result) = tokio::join!(left_task, right_task, reply_task);
    left_result
        .expect("left append task")
        .expect("the left append settles applied");
    right_result
        .expect("right append task")
        .expect("the right append settles applied");
    reply_result
        .expect("keyed append task")
        .expect("the keyed append settles applied");

    let fresh = state
        .open_session(&session_id, "test")
        .await
        .expect("reopen the durable session");
    let messages = fresh.read_view();
    let appended = messages
        .messages()
        .iter()
        .map(message_text)
        .filter(|text| text.starts_with("fig4202-concurrent-"))
        .collect::<Vec<_>>();
    assert!(
        appended == vec!["fig4202-concurrent-left", "fig4202-concurrent-right"]
            || appended == vec!["fig4202-concurrent-right", "fig4202-concurrent-left"],
        "both appends appear exactly once in durable graph order: {appended:?}"
    );
    assert_eq!(
        messages
            .messages()
            .iter()
            .filter(|message| message.id == reply_id)
            .count(),
        1,
        "the keyed append lands exactly once"
    );
    drop(fresh);
    workbench.shutdown().await;
}

/// Deleting a session other than the selected one leaves the selected
/// session's rows, Lash VM graphs and mail accounts as they were.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn deleting_a_non_current_session_preserves_selected_session_buffers() {
    let workbench = Workbench::silent().await;
    let state = &workbench.state;
    let retired_session_id = state.current_session_id();
    let selected_session_id = SessionId::from("workbench-selected-during-delete");
    state
        .sessions
        .record(selected_session_id.clone(), "selected".to_string());
    state
        .sessions
        .select(&selected_session_id)
        .expect("select the competing session");
    state.messages.lock_recover().push(ChatMessage {
        id: "selected-message".to_string(),
        role: "user".to_string(),
        text: "keep the selected view".to_string(),
        at: "2026-08-30T00:00:00Z".to_string(),
        attachments: Vec::new(),
        provenance: None,
        client_nonce: None,
    });
    state
        .mail_world
        .add_account("Selected Inbox")
        .expect("add selected-session mail account");
    let messages_before = serde_json::to_value(state.messages_snapshot())
        .expect("serialize selected-session messages");
    let graphs_before = state.lash_vm_execution.graphs();
    let mail_before = serde_json::to_value(state.mail_world.account_summaries())
        .expect("serialize selected-session mail accounts");

    let Json(replacement) = Box::pin(reset_chat(
        State(state.clone()),
        Query(SessionQuery {
            session_id: Some(retired_session_id.clone()),
        }),
    ))
    .await
    .expect("delete the non-current session");

    assert_ne!(replacement.settings.session_id, retired_session_id);
    assert_eq!(state.current_session_id(), selected_session_id);
    assert_eq!(
        serde_json::to_value(state.messages_snapshot()).expect("serialize messages after delete"),
        messages_before
    );
    assert_eq!(state.lash_vm_execution.graphs(), graphs_before);
    assert_eq!(
        serde_json::to_value(state.mail_world.account_summaries())
            .expect("serialize mail accounts after delete"),
        mail_before
    );
    super::reset_chat_tests::assert_tombstoned(state, &retired_session_id).await;
    workbench.shutdown().await;
}

/// Blocks the route at one of its own trace events until released.
pub(crate) struct TurnAdmissionGate {
    event_name: &'static str,
    entered: std::sync::mpsc::SyncSender<()>,
    release: Arc<(Mutex<bool>, std::sync::Condvar)>,
}

impl TurnAdmissionGate {
    pub(crate) fn new(event_name: &'static str) -> (Arc<Self>, std::sync::mpsc::Receiver<()>) {
        let (entered, entered_rx) = std::sync::mpsc::sync_channel(1);
        (
            Arc::new(Self {
                event_name,
                entered,
                release: Arc::new((Mutex::new(false), std::sync::Condvar::new())),
            }),
            entered_rx,
        )
    }

    pub(crate) fn open(&self) {
        let (released, condition) = &*self.release;
        *released.lock().unwrap_or_else(|error| error.into_inner()) = true;
        condition.notify_all();
    }
}

impl TraceSink for TurnAdmissionGate {
    fn append(
        &self,
        record: &TraceRecord,
    ) -> std::result::Result<(), lash::tracing::TraceSinkError> {
        if matches!(
            &record.event,
            TraceEvent::Custom { name, .. } if name == self.event_name
        ) {
            let _ = self.entered.send(());
            let (released, condition) = &*self.release;
            let mut released = released.lock().unwrap_or_else(|error| error.into_inner());
            while !*released {
                released = condition
                    .wait(released)
                    .unwrap_or_else(|error| error.into_inner());
            }
        }
        Ok(())
    }
}

pub(crate) async fn entered(gate: std::sync::mpsc::Receiver<()>) {
    tokio::task::spawn_blocking(move || gate.recv_timeout(Duration::from_secs(10)))
        .await
        .expect("gate wait")
        .expect("the send reaches its gated boundary");
}

/// A send that read the session idle, then lost the atomic claim to queued
/// work, is admitted as the next turn's input: no user row, one ingress
/// receipt, and the queued work keeps the claim.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_send_queues_if_queued_work_claims_after_its_idle_read() {
    let (gate, entered_rx) = TurnAdmissionGate::new("agent_workbench.api.turn.claim_ready");
    let workbench = Workbench::builder(silent_provider())
        .trace_sink(Arc::clone(&gate) as Arc<dyn TraceSink>)
        .build()
        .await;
    let state = &workbench.state;
    let session_id = state.current_session_id();
    let send = tokio::spawn({
        let state = state.clone();
        async move { send_text(&state, None, "send that loses the queued claim race").await }
    });
    entered(entered_rx).await;
    assert!(
        state
            .active_turns
            .try_insert_for_idle_session(
                &session_id,
                &TurnId::from("queued-race-owner"),
                WorkbenchTurnKind::Queued,
            )
            .is_claimed()
    );
    gate.open();

    let accepted = send
        .await
        .expect("send task")
        .expect("the losing send is queued");
    assert!(accepted.queued);
    assert!(state.active_turns.for_session(&session_id).is_some());
    assert!(product_rows(state, &session_id, "user").is_empty());
    assert_eq!(product_ingress_receipts(state, &session_id).len(), 1);
    state
        .active_turns
        .remove(&session_id, &TurnId::from("queued-race-owner"));
    workbench.shutdown().await;
}

struct PanickingTurnAdmissionTrace;

impl TraceSink for PanickingTurnAdmissionTrace {
    fn append(
        &self,
        record: &TraceRecord,
    ) -> std::result::Result<(), lash::tracing::TraceSinkError> {
        if matches!(
            &record.event,
            TraceEvent::Custom { name, .. }
                if name == "agent_workbench.api.turn.admission_committed"
        ) {
            panic!("injected turn-admission panic");
        }
        Ok(())
    }
}

/// A send whose admission panics after its user row committed answers `500`,
/// releases the session's claim, retires the optimistic user row and
/// publishes one failure row; no turn reaches the provider.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_panicked_turn_submission_cleans_up_and_publishes_failure() {
    let workbench = Workbench::builder(silent_provider())
        .trace_sink(Arc::new(PanickingTurnAdmissionTrace))
        .build()
        .await;
    let state = &workbench.state;
    let session_id = state.current_session_id();

    let error = send_text(state, None, "panic after commit")
        .await
        .expect_err("the panicked admission is an API failure");
    assert_eq!(error.status, StatusCode::INTERNAL_SERVER_ERROR);
    assert!(
        state.active_turns.for_session(&session_id).is_none(),
        "the panic guard must release the active turn"
    );
    assert!(
        product_rows(state, &session_id, "user").is_empty(),
        "the panic guard must retire the optimistic user row"
    );
    let failures = product_rows(state, &session_id, "event");
    assert_eq!(failures.len(), 1);
    assert_eq!(failures[0].1, PUBLIC_TURN_FAILURE_MESSAGE);
    workbench.shutdown().await;
}

/// FIG-2324: dropping the HTTP request after the user row and active-turn
/// prompt commit must not cancel submission and permanently divert later
/// sends into a queue that can never drain.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_dropped_send_request_cannot_wedge_a_committed_turn() {
    let (gate, entered_rx) = TurnAdmissionGate::new("agent_workbench.api.turn.admission_committed");
    let workbench = Workbench::builder(replying_provider(
        "<typescript>\nfinish(\"request completed\");\n</typescript>",
    ))
    .trace_sink(Arc::clone(&gate) as Arc<dyn TraceSink>)
    .build()
    .await;
    let state = &workbench.state;
    let session_id = state.current_session_id();

    let request = tokio::spawn({
        let state = state.clone();
        async move { send_text(&state, None, "committed before disconnect").await }
    });
    entered(entered_rx).await;
    let turn_id = state
        .active_turns
        .for_session(&session_id)
        .expect("the committed send holds the session's claim")
        .address
        .turn_id;
    request.abort();
    gate.open();
    assert!(
        request
            .await
            .expect_err("the request task was aborted")
            .is_cancelled(),
        "the harness must drop the request future"
    );

    // The admission task outlives the dropped request: the engine runs the
    // committed turn and its follower settles it.
    wait_for_turn_released(state, &session_id, &turn_id, Duration::from_secs(30)).await;
    assert!(
        state.active_turns.for_session(&session_id).is_none(),
        "settlement must retire the detached turn"
    );

    let follow_up = send_text(state, None, "send after disconnect")
        .await
        .expect("a later send is admitted normally");
    assert!(!follow_up.queued, "the later send must not remain wedged");
    let follow_up_turn = started_turn_id(&follow_up);
    wait_for_turn_released(state, &session_id, &follow_up_turn, Duration::from_secs(30)).await;
    workbench.shutdown().await;
}

/// A busy session accepts the second browser send as durable next-turn
/// input: every viewer sees it as a queued receipt, the snapshot shows the
/// running input admitted and the queued one open, and once the first run
/// commits the queued send runs once as its own turn.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_send_to_a_busy_session_is_admitted_as_a_queued_next_turn_input() {
    let mut gate = GatedProvider::new();
    let workbench = Workbench::builder(gate.provider.clone()).build().await;
    let state = &workbench.state;
    let session_id = state.current_session_id();

    let first = send_text(state, None, "first send")
        .await
        .expect("first send admitted");
    assert!(first.accepted && !first.queued);
    let first_turn_id = started_turn_id(&first);
    assert_eq!(gate.next_call().await, 0);

    let second = send_text(state, None, "second send")
        .await
        .expect("busy send admitted");
    assert!(second.accepted && second.queued);
    let receipt = second
        .queued_input
        .expect("busy send has an ingress receipt");
    assert_eq!(receipt.text, "second send");
    assert_eq!(
        receipt.ingress,
        lash::persistence::TurnInputIngress::NextTurn
    );
    assert_eq!(
        receipt.state,
        lash::persistence::TurnInputState::DeferredNextTurn
    );
    assert_eq!(
        product_rows(state, &session_id, "user")
            .into_iter()
            .map(|(_, text)| text)
            .collect::<Vec<_>>(),
        vec!["first send".to_string()],
    );
    assert_eq!(
        product_ingress_receipts(state, &session_id)
            .iter()
            .map(|receipt| receipt.text.clone())
            .collect::<Vec<_>>(),
        vec!["second send".to_string()],
        "every viewer must see the queued send as an ingress receipt"
    );
    let mid_turn = read_state(state, None).await.expect("mid-turn snapshot");
    let (held, pending) =
        mid_turn
            .pending_turn_inputs
            .iter()
            .fold((0_usize, 0_usize), |(held, pending), input| {
                match input.status {
                    lash::PendingTurnInputReadStatus::Admitted { .. } => (held + 1, pending),
                    lash::PendingTurnInputReadStatus::Open => (held, pending + 1),
                    _ => (held, pending),
                }
            });
    assert_eq!(
        (held, pending),
        (1, 1),
        "the running input stays admitted; the queued send is durably pending: {:?}",
        mid_turn.pending_turn_inputs
    );
    assert!(mid_turn.pending_turn_inputs.iter().any(|input| {
        input.input.input_id.as_str() == receipt.input_id
            && matches!(input.status, lash::PendingTurnInputReadStatus::Open)
    }));

    gate.release(2);
    wait_for_turn_released(state, &session_id, &first_turn_id, Duration::from_secs(30)).await;
    tokio::time::timeout(Duration::from_secs(30), async {
        loop {
            let snapshot = read_state(state, None).await.expect("settled snapshot");
            let rows = state_rows(&snapshot);
            if snapshot.pending_turn_inputs.is_empty()
                && rows.iter().filter(|(role, _)| role == "assistant").count() == 2
            {
                let users = rows
                    .iter()
                    .filter(|(role, _)| role == "user")
                    .map(|(_, text)| text.as_str())
                    .collect::<Vec<_>>();
                assert_eq!(users, vec!["first send", "second send"], "{rows:?}");
                let answers = rows
                    .iter()
                    .filter(|(role, _)| role == "assistant")
                    .map(|(_, text)| text.as_str())
                    .collect::<Vec<_>>();
                assert_eq!(answers, vec!["answer 0", "answer 1"], "{rows:?}");
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("the queued send settles exactly once after the first run");
    workbench.shutdown().await;
}

/// Opening a session while its turn's provider call is stalled returns
/// promptly: a stalled turn pins no lease an open waits out.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_stalled_turn_does_not_block_competing_recovery_open() {
    let mut gate = GatedProvider::new();
    let workbench = Workbench::builder(gate.provider.clone()).build().await;
    let state = &workbench.state;
    let session_id = state.current_session_id();
    let accepted = send_text(state, None, "admitted send")
        .await
        .expect("send admitted");
    let turn_id = started_turn_id(&accepted);
    assert_eq!(gate.next_call().await, 0);
    tokio::time::timeout(
        Duration::from_secs(2),
        state.core.session(session_id.clone()).open(),
    )
    .await
    .expect("recovery open does not wait out a stalled turn")
    .expect("recovery open succeeds while the turn is stalled");
    gate.release(1);
    wait_for_turn_released(state, &session_id, &turn_id, Duration::from_secs(30)).await;
    assert!(state.active_turns.for_session(&session_id).is_none());
    workbench.shutdown().await;
}
