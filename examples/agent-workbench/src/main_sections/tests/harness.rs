use super::*;
use lash::SessionId;
use lash::TurnId;

// The in-process workbench (FIG-5316): the `AppState` the binary serves from,
// built by the binary's own boot functions (`build_workbench_core`,
// `workbench_app_state`, `start_workbench`) over a SQLite memory store set.
// The engine is the durable backend's own; nothing here reaches into it.

/// The model every harness session runs, served by the test's provider
/// through the workbench's open model catalog.
pub(crate) const TEST_MODEL: &str = "test-model";

/// A running in-process workbench and the store set it serves from.
pub(crate) struct Workbench {
    pub(crate) state: AppState,
    pub(crate) stores: Arc<dyn lash::StoreSet>,
}

/// What a law may vary about a workbench before it boots; everything else is
/// the binary's own composition.
pub(crate) struct WorkbenchBuilder {
    provider: ProviderHandle,
    session_delete_faults: Option<Arc<super::session_delete_faults::SessionDeleteFaults>>,
    stores: Option<Arc<dyn lash::StoreSet>>,
    live_replay: Option<Arc<dyn lash::observe::LiveReplayStore>>,
    trace_sink: Option<Arc<dyn TraceSink>>,
    mail_world: mail::MailWorld,
    sessions: Option<WorkbenchSessions>,
    event_tx: Option<SessionEventRegistry>,
    active_turns: Option<ActiveTurns>,
    tool_provider: Option<Arc<dyn lash::tools::ToolProvider>>,
}

impl Workbench {
    pub(crate) fn builder(provider: ProviderHandle) -> WorkbenchBuilder {
        WorkbenchBuilder {
            provider,
            session_delete_faults: None,
            stores: None,
            live_replay: None,
            trace_sink: None,
            mail_world: mail::MailWorld::new(),
            sessions: None,
            event_tx: None,
            active_turns: None,
            tool_provider: None,
        }
    }

    /// A workbench whose provider answers every call with `reply`.
    pub(crate) async fn replying(reply: &'static str) -> Self {
        Self::builder(replying_provider(reply)).build().await
    }

    /// A workbench whose provider must not be called.
    pub(crate) async fn silent() -> Self {
        Self::builder(silent_provider()).build().await
    }

    pub(crate) async fn shutdown(self) {
        self.state
            .core
            .shutdown()
            .await
            .expect("the core shuts down");
    }
}

impl WorkbenchBuilder {
    /// Decorate the real store ports before building the engine.
    pub(crate) fn session_delete_faults(
        mut self,
        faults: Arc<super::session_delete_faults::SessionDeleteFaults>,
    ) -> Self {
        self.session_delete_faults = Some(faults);
        self
    }

    /// Serve from `stores`, e.g. the store set an earlier workbench served
    /// from: a restart of the web process over the same database.
    pub(crate) fn stores(mut self, stores: Arc<dyn lash::StoreSet>) -> Self {
        self.stores = Some(stores);
        self
    }

    pub(crate) fn live_replay(mut self, store: Arc<dyn lash::observe::LiveReplayStore>) -> Self {
        self.live_replay = Some(store);
        self
    }

    pub(crate) fn trace_sink(mut self, sink: Arc<dyn TraceSink>) -> Self {
        self.trace_sink = Some(sink);
        self
    }

    pub(crate) fn mail_world(mut self, mail_world: mail::MailWorld) -> Self {
        self.mail_world = mail_world;
        self
    }

    pub(crate) fn sessions(mut self, sessions: WorkbenchSessions) -> Self {
        self.sessions = Some(sessions);
        self
    }

    pub(crate) fn event_tx(mut self, event_tx: SessionEventRegistry) -> Self {
        self.event_tx = Some(event_tx);
        self
    }

    pub(crate) fn active_turns(mut self, active_turns: ActiveTurns) -> Self {
        self.active_turns = Some(active_turns);
        self
    }

    /// Serve host tools from `tools`, as a deployment's tool fixture does.
    pub(crate) fn tool_provider(mut self, tools: Arc<dyn lash::tools::ToolProvider>) -> Self {
        self.tool_provider = Some(tools);
        self
    }

    pub(crate) async fn build(self) -> Workbench {
        let stores: Arc<dyn lash::StoreSet> = match self.stores {
            Some(stores) => stores,
            None => Arc::new(
                lash::sqlite::SqliteStoreSet::memory()
                    .await
                    .expect("open a SQLite memory store set"),
            ),
        };
        lash::testing::process_execution_env_fixture(stores.process_env_store().as_ref()).await;
        let stores = match self.session_delete_faults {
            Some(faults) => faults.install(stores),
            None => stores,
        };
        let selection = LlmProfileSelection {
            model: TEST_MODEL.to_string(),
            model_variant: None,
        };
        let session_defaults = workbench_session_defaults(&selection, None);
        let approvals =
            approvals::WorkbenchApprovals::in_memory().expect("open the approval ledger");
        let live_replay = match self.live_replay {
            Some(store) => store,
            None => WorkbenchLiveReplay::from_environment()
                .expect("the default live replay")
                .store()
                .await
                .expect("the default live replay store"),
        };
        let mcp = lash::mcp::McpPluginFactory::builder(BTreeMap::new())
            .build()
            .await
            .expect("an MCP factory with no servers");
        let plugins = WorkbenchCorePlugins {
            rlm_workers: workbench_rlm_workers().expect("the VM worker deployment"),
            tool_provider: self.tool_provider,
            mail_world: self.mail_world.clone(),
            child_spec: session_defaults.clone(),
            deferred_tools: deferred_tools::WorkbenchDeferredTools::in_memory()
                .expect("open the deferred-tool grants"),
            approvals: approvals.clone(),
            mcp: Arc::new(mcp),
            live_replay,
            #[cfg(feature = "e2e-tools")]
            operation: Arc::default(),
        };
        let lashlang_execution = Arc::new(TraceLashlangGraphStore::default());
        let trace_sink = self
            .trace_sink
            .unwrap_or_else(|| Arc::new(TeeTraceSink::new([])) as Arc<dyn TraceSink>);
        let core = build_workbench_core(
            &stores,
            workbench_rlm_channel().expect("the RLM channel"),
            workbench_context_window_tokens(),
            plugins,
            WorkbenchTracing {
                trace_sink: Arc::clone(&trace_sink),
                lashlang_execution_sink: Arc::clone(&lashlang_execution) as Arc<dyn TraceSink>,
            },
            self.provider,
            lash::persistence::LeaseOwnerIdentity::opaque(
                "agent-workbench-test",
                uuid::Uuid::new_v4().to_string(),
            ),
        )
        .await
        .expect("build the workbench core");
        let state = workbench_app_state(
            core,
            stores.as_ref(),
            WorkbenchHost {
                session_defaults,
                sessions: self.sessions.unwrap_or_else(WorkbenchSessions::fresh),
                event_tx: self
                    .event_tx
                    .unwrap_or_else(|| SessionEventRegistry::new(64)),
                active_turns: self.active_turns.unwrap_or_default(),
                mail_world: self.mail_world,
                approvals,
                selected_llm_profile: selection,
                trace_sink: Some(trace_sink),
                lashlang_execution,
            },
        )
        .expect("assemble the workbench state");
        start_workbench(&state)
            .await
            .expect("the workbench starts serving");
        Workbench { state, stores }
    }
}

/// An RLM reply: one TypeScript cell.
pub(crate) fn text_response(text: &str) -> lash::provider::LlmResponse {
    lash::provider::LlmResponse {
        parts: vec![lash::direct::LlmOutputPart::Text {
            text: text.to_string(),
            response_meta: None,
        }],
        ..lash::provider::LlmResponse::default()
    }
}

/// The cell that finishes a turn with `value` as its final string.
pub(crate) fn finish_cell(value: &str) -> String {
    format!(
        "<typescript>\nfinish({});\n</typescript>",
        serde_json::to_string(value).expect("a string encodes")
    )
}

pub(crate) fn replying_provider(reply: &'static str) -> ProviderHandle {
    lash::testing::TestProvider::builder()
        .kind("workbench-harness")
        .complete(move |_| async move { Ok(text_response(reply)) })
        .build()
        .into_handle()
}

/// A provider whose call `n` answers `cells[n]`.
pub(crate) fn scripted_cells_provider(cells: Vec<String>) -> ProviderHandle {
    let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    lash::testing::TestProvider::builder()
        .kind("workbench-harness")
        .complete(move |_| {
            let call = calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            let cell = cells
                .get(call)
                .cloned()
                .unwrap_or_else(|| panic!("the scripted provider ran out of cells at {call}"));
            async move { Ok(text_response(&cell)) }
        })
        .build()
        .into_handle()
}

pub(crate) fn silent_provider() -> ProviderHandle {
    lash::testing::TestProvider::builder()
        .kind("workbench-harness")
        .complete_error("this law must not call the provider")
        .build()
        .into_handle()
}

/// A provider every call of which parks until the law releases it: each call
/// `n` reports itself entered, waits for one permit, then answers (by default
/// `finish("answer n")`).
pub(crate) struct GatedProvider {
    pub(crate) provider: ProviderHandle,
    pub(crate) entered: mpsc::UnboundedReceiver<usize>,
    permits: Arc<tokio::sync::Semaphore>,
}

impl GatedProvider {
    pub(crate) fn new() -> Self {
        Self::replying(|call| finish_cell(&format!("answer {call}")))
    }

    /// A gated provider whose call `n` answers `reply(n)` once released.
    pub(crate) fn replying(reply: impl Fn(usize) -> String + Send + Sync + 'static) -> Self {
        let reply = Arc::new(reply);
        let (entered_tx, entered) = mpsc::unbounded_channel();
        let permits = Arc::new(tokio::sync::Semaphore::new(0));
        let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let provider = lash::testing::TestProvider::builder()
            .kind("workbench-harness-gated")
            .complete({
                let permits = Arc::clone(&permits);
                move |_| {
                    let entered_tx = entered_tx.clone();
                    let permits = Arc::clone(&permits);
                    let call = calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    let reply = reply(call);
                    async move {
                        let _ = entered_tx.send(call);
                        permits
                            .acquire()
                            .await
                            .expect("the gate stays open")
                            .forget();
                        Ok(text_response(&reply))
                    }
                }
            })
            .build()
            .into_handle();
        Self {
            provider,
            entered,
            permits,
        }
    }

    /// Wait until the next call has entered the provider, and answer its
    /// index.
    pub(crate) async fn next_call(&mut self) -> usize {
        tokio::time::timeout(Duration::from_secs(30), self.entered.recv())
            .await
            .expect("a call reaches the provider")
            .expect("the provider stays alive")
    }

    /// Let `count` parked or later calls answer.
    pub(crate) fn release(&self, count: usize) {
        self.permits.add_permits(count);
    }
}

/// The headers a remote client opens `/api/observations` with: a protocol
/// Hello for every version this build speaks.
pub(crate) fn remote_hello_headers() -> axum::http::HeaderMap {
    let mut headers = axum::http::HeaderMap::new();
    headers.insert(
        "x-lash-protocol-hello",
        serde_json::to_string(&lash::remote::Negotiation::Hello {
            supported: lash::remote::REMOTE_PROTOCOL,
        })
        .expect("encode the protocol Hello")
        .parse()
        .expect("a Hello is a header value"),
    );
    headers
}

/// The chat route's request for `text` on the selected model.
pub(crate) fn turn_request(text: &str) -> TurnRequest {
    TurnRequest {
        text: text.to_string(),
        model: Some(TEST_MODEL.to_string()),
        model_variant: None,
        attachment_id: None,
        client_nonce: None,
    }
}

/// Send `text` to `session` through the chat route.
pub(crate) async fn send_text(
    state: &AppState,
    session: Option<&SessionId>,
    text: &str,
) -> Result<TurnAccepted, AppError> {
    send_turn(
        State(state.clone()),
        Query(SessionQuery {
            session_id: session.cloned(),
        }),
        Json(turn_request(text)),
    )
    .await
    .map(|Json(accepted)| accepted)
}

pub(crate) fn started_turn_id(accepted: &TurnAccepted) -> TurnId {
    assert!(
        !accepted.queued,
        "the send was queued, not started: {accepted:?}"
    );
    accepted
        .turn_id
        .clone()
        .expect("a started send names its turn")
}

/// Wait until the send's follower settles `turn_id` and releases the
/// session's claim on it.
pub(crate) async fn wait_for_turn_released(
    state: &AppState,
    session_id: &SessionId,
    turn_id: &TurnId,
    timeout: Duration,
) {
    let deadline = tokio::time::Instant::now() + timeout;
    while state
        .active_turns
        .for_session(session_id)
        .is_some_and(|active| active.address.turn_id == *turn_id)
    {
        assert!(
            tokio::time::Instant::now() < deadline,
            "turn {turn_id} was not settled within {timeout:?}"
        );
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}

/// Send `text` through the chat route and wait for its follower to settle it.
pub(crate) async fn run_turn(state: &AppState, text: &str) -> TurnId {
    let session_id = state.current_session_id();
    let accepted = send_text(state, None, text)
        .await
        .expect("the send is admitted");
    let turn_id = started_turn_id(&accepted);
    wait_for_turn_released(state, &session_id, &turn_id, Duration::from_secs(30)).await;
    turn_id
}

/// [`run_turn`] in `session_id`, named by the send's query.
pub(crate) async fn run_turn_in(state: &AppState, session_id: &SessionId, text: &str) -> TurnId {
    let accepted = send_text(state, Some(session_id), text)
        .await
        .expect("the send is admitted");
    let turn_id = started_turn_id(&accepted);
    wait_for_turn_released(state, session_id, &turn_id, Duration::from_secs(30)).await;
    turn_id
}

/// The `(role, text)` rows the page renders for `snapshot`.
pub(crate) fn state_rows(snapshot: &StateReadSnapshot) -> Vec<(String, String)> {
    snapshot
        .transcript
        .iter()
        .filter_map(|row| {
            chat_message_from_row(row)
                .expect("project canonical row")
                .map(|message| (message.role.clone(), message.text.clone()))
        })
        .collect()
}

/// The `/api/state` snapshot of `session`, or of the current session.
pub(crate) async fn read_state(
    state: &AppState,
    session: Option<&SessionId>,
) -> Result<StateReadSnapshot, AppError> {
    app_state(
        State(state.clone()),
        Query(SessionQuery {
            session_id: session.cloned(),
        }),
    )
    .await
    .map(|Json(snapshot)| snapshot)
}

/// The `(id, text)` product rows `session_id` published with `role`.
pub(crate) fn product_rows(
    state: &AppState,
    session_id: &SessionId,
    role: &str,
) -> Vec<(String, String)> {
    state
        .event_tx
        .snapshot(session_id)
        .events
        .into_iter()
        .filter_map(|event| match event.item {
            StreamItem::Message { message } if message.role == role => {
                Some((message.id, message.text))
            }
            _ => None,
        })
        .collect()
}

/// The queued-input receipts `session_id` published.
pub(crate) fn product_ingress_receipts(
    state: &AppState,
    session_id: &SessionId,
) -> Vec<TurnInputReceipt> {
    state
        .event_tx
        .snapshot(session_id)
        .events
        .into_iter()
        .filter_map(|event| match event.item {
            StreamItem::TurnInput { receipt } => Some(receipt),
            _ => None,
        })
        .collect()
}

/// Delete `session_id` through the core, as any host holding administrative
/// authority may, and wait for its tombstone. The workbench's own retirement
/// mark is never placed, so a later refusal is the store tombstone's.
pub(crate) async fn tombstone_session(state: &AppState, session_id: &SessionId) {
    let administration = state.core.session_administration().await;
    let context = administration
        .delete_context(session_id)
        .expect("a delete context for the session");
    lash::LashCore::delete_session(context)
        .await
        .expect("request the session's deletion");
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    while !matches!(
        state
            .session_store_factory
            .lookup_session(session_id)
            .await
            .expect("look up the deleted session"),
        lash::persistence::SessionLookup::Deleted
    ) {
        assert!(
            tokio::time::Instant::now() < deadline,
            "the session's close never wrote its tombstone"
        );
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}

impl AppState {
    /// Route `turn_id` as `session_id`'s running turn, as the send route
    /// claims it.
    pub(crate) fn track_turn(&self, session_id: &SessionId, turn_id: &TurnId) {
        self.active_turns
            .insert(session_id, turn_id, WorkbenchTurnKind::User);
    }

    /// The rows this process published, across sessions.
    pub(crate) fn messages_snapshot(&self) -> Vec<ChatMessage> {
        self.messages.lock_recover().clone()
    }
}

/// A trace sink that keeps every record, for a law about what the workbench
/// traced.
#[derive(Default)]
pub(crate) struct RecordingTrace {
    records: Mutex<Vec<TraceRecord>>,
}

impl RecordingTrace {
    /// Every record the workbench and its core traced, in order.
    pub(crate) fn records(&self) -> Vec<TraceRecord> {
        self.records.lock_recover().clone()
    }

    /// The `(session, payload)` of every workbench trace named `name`.
    pub(crate) fn custom(&self, name: &str) -> Vec<(Option<SessionId>, Value)> {
        let name = format!("agent_workbench.{name}");
        self.records
            .lock_recover()
            .iter()
            .filter_map(|record| match &record.event {
                TraceEvent::Custom {
                    name: recorded,
                    payload,
                } if *recorded == name => {
                    Some((record.context.session_id.clone(), payload.clone()))
                }
                _ => None,
            })
            .collect()
    }
}

impl TraceSink for RecordingTrace {
    fn append(
        &self,
        record: &TraceRecord,
    ) -> std::result::Result<(), lash::tracing::TraceSinkError> {
        self.records.lock_recover().push(record.clone());
        Ok(())
    }
}
