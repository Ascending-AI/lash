use super::*;
use lash_core::plugin::PluginSessionRequest;
use lash_core::testing::TestTurnExecution as _;

const SEED: u64 = 0x5_f410;
/// A clock jump past the 30s session-lease TTL these probes expire.
const EXPIRED_LEASE_MS: u64 = 30_001;

#[derive(Debug)]
pub(super) struct ManualClock {
    epoch_ms: std::sync::atomic::AtomicU64,
}

impl ManualClock {
    pub(super) fn new(epoch_ms: u64) -> Self {
        Self {
            epoch_ms: std::sync::atomic::AtomicU64::new(epoch_ms),
        }
    }

    pub(super) fn advance_ms(&self, delta_ms: u64) {
        self.epoch_ms
            .fetch_add(delta_ms, std::sync::atomic::Ordering::SeqCst);
    }
}

#[async_trait::async_trait]
impl lash_core::Clock for ManualClock {
    fn now(&self) -> std::time::Instant {
        std::time::Instant::now()
    }

    fn timestamp_datetime(&self) -> chrono::DateTime<chrono::Utc> {
        let timestamp_ms = self.epoch_ms.load(std::sync::atomic::Ordering::SeqCst);
        chrono::DateTime::from(
            std::time::UNIX_EPOCH + std::time::Duration::from_millis(timestamp_ms),
        )
    }

    async fn sleep(&self, duration: std::time::Duration) {
        tokio::time::sleep(duration).await;
    }

    async fn sleep_until(&self, deadline: std::time::Instant) {
        tokio::time::sleep_until(tokio::time::Instant::from_std(deadline)).await;
    }
}

#[tokio::test(flavor = "multi_thread")]
pub(super) async fn double_invalidation_preserves_first_decision_id() {
    let backend = sqlite_memory_store_backend().await;
    let mut runtime = runtime_with_plugins_and_tools(
        &backend,
        Vec::new(),
        Arc::new(EmptyTools),
        mock_provider(Vec::new()),
    )
    .await;
    assert_eq!(
        *runtime.resident_session.validity(),
        ResidentSessionState::Valid
    );

    runtime.invalidate_resident_session_state();
    let initial_decision_id = match runtime.resident_session.validity() {
        ResidentSessionState::Invalidated { decision_id } => decision_id.clone(),
        ResidentSessionState::Valid => panic!("expected invalidated resident state"),
    };
    assert!(!initial_decision_id.is_empty());

    // A second invalidation while already invalidated must preserve the first decision id
    runtime.invalidate_resident_session_state();
    match runtime.resident_session.validity() {
        ResidentSessionState::Invalidated { decision_id } => {
            assert_eq!(
                decision_id, &initial_decision_id,
                "subsequent invalidation must not overwrite the initial decision identity"
            );
        }
        ResidentSessionState::Valid => panic!("expected invalidated resident state"),
    }
}

pub(super) struct FrameRotatingDynamicTool {
    rotated: Arc<AtomicBool>,
}

pub(super) fn rotating_tool_definition(name: &str) -> lash_core::ToolDefinition {
    lash_core::ToolDefinition::raw(
        format!("tool:{name}"),
        name,
        "Exercise live tool discovery across an AgentFrame rotation",
        lash_core::ToolDefinition::default_input_schema(),
        json!({ "type": "object", "additionalProperties": true }),
    )
    .expect("valid declared tool schemas")
}

#[async_trait::async_trait]
impl lash_core::ToolProvider for FrameRotatingDynamicTool {
    fn tool_manifests(&self) -> Vec<lash_core::ToolManifest> {
        let mut manifests = vec![
            rotating_tool_definition("rotate_surface").manifest(),
            rotating_tool_definition("curated_before_rotation").manifest(),
        ];
        if self.rotated.load(Ordering::SeqCst) {
            manifests.push(rotating_tool_definition("new_after_rotation").manifest());
            manifests.push(rotating_tool_definition("hidden_after_rotation").manifest());
        }
        manifests
    }

    fn resolve_contract(&self, name: &str) -> Option<Arc<lash_core::ToolContract>> {
        self.tool_manifests()
            .into_iter()
            .any(|manifest| manifest.name == name)
            .then(|| Arc::new(rotating_tool_definition(name).contract()))
    }

    async fn execute(&self, call: lash_core::ToolCall<'_>) -> lash_core::ToolAttemptOutcome {
        (async {
            match call.name() {
                "rotate_surface" => {
                    self.rotated.store(true, Ordering::SeqCst);
                    lash_core::ToolOutcome::ok(json!({ "rotated": true })).with_control(
                        lash_core::ToolControl::SwitchAgentFrame {
                            frame_key: lash_core::FrameKey::from_caller_material(
                                "live-surface-frame",
                            )
                            .expect("non-empty caller material"),
                            initial_nodes: Vec::new(),
                            task: Some("call the newly available tool".to_string()),
                        },
                    )
                }
                "new_after_rotation" => lash_core::ToolOutcome::ok(
                    json!({ "called": call.name() }),
                )
                .with_control(lash_core::ToolControl::Finish {
                    value: lash_core::ToolValue::untrusted_json(json!("new tool executed")),
                }),
                "curated_before_rotation" | "hidden_after_rotation" => {
                    lash_core::ToolOutcome::ok(json!({ "called": call.name() }))
                }
                name => {
                    lash_core::ToolOutcome::err_fmt(format_args!("unknown rotating tool `{name}`"))
                }
            }
        })
        .await
        .into()
    }
}

pub(super) struct ExpireLeaseAtPreparedTurn {
    clock: Arc<lash_core::testing::TestClock>,
    expired: AtomicBool,
}

impl ExpireLeaseAtPreparedTurn {
    pub(super) fn new(clock: Arc<lash_core::testing::TestClock>) -> Self {
        Self {
            clock,
            expired: AtomicBool::new(false),
        }
    }
}

impl lash_core::runtime::RuntimeTurnPhaseProbe for ExpireLeaseAtPreparedTurn {
    fn begin(&self, phase: lash_core::runtime::RuntimeTurnPhase) {
        if phase == lash_core::runtime::RuntimeTurnPhase::PreparedTurn
            && !self.expired.swap(true, Ordering::SeqCst)
        {
            self.clock.advance(EXPIRED_LEASE_MS);
        }
    }

    fn end(&self, _phase: lash_core::runtime::RuntimeTurnPhase) {}
}

pub(super) struct ExpireLeaseAfterPromptBuild {
    clock: Arc<lash_core::testing::TestClock>,
    expired: AtomicBool,
}

impl ExpireLeaseAfterPromptBuild {
    pub(super) fn new(clock: Arc<lash_core::testing::TestClock>) -> Self {
        Self {
            clock,
            expired: AtomicBool::new(false),
        }
    }
}

impl lash_core::runtime::RuntimeTurnPhaseProbe for ExpireLeaseAfterPromptBuild {
    fn begin(&self, _phase: lash_core::runtime::RuntimeTurnPhase) {}

    fn end(&self, phase: lash_core::runtime::RuntimeTurnPhase) {
        if phase == lash_core::runtime::RuntimeTurnPhase::PromptBuild
            && !self.expired.swap(true, Ordering::SeqCst)
        {
            self.clock.advance(EXPIRED_LEASE_MS);
        }
    }
}

pub(super) struct ExpireLeaseAtSecondTurnFinalizedHook {
    clock: Arc<lash_core::testing::TestClock>,
    finalized_hooks: AtomicUsize,
}

impl ExpireLeaseAtSecondTurnFinalizedHook {
    pub(super) fn new(clock: Arc<lash_core::testing::TestClock>) -> Self {
        Self {
            clock,
            finalized_hooks: AtomicUsize::new(0),
        }
    }
}

impl lash_core::runtime::RuntimeTurnPhaseProbe for ExpireLeaseAtSecondTurnFinalizedHook {
    fn begin(&self, _phase: lash_core::runtime::RuntimeTurnPhase) {}

    fn end(&self, _phase: lash_core::runtime::RuntimeTurnPhase) {}

    fn begin_named(&self, phase: &str) {
        if phase.starts_with("plugin_hook.turn_finalized.")
            && self.finalized_hooks.fetch_add(1, Ordering::SeqCst) == 1
        {
            self.clock.advance(EXPIRED_LEASE_MS);
        }
    }
}

pub(super) struct PauseAtPreparedTurn {
    pub(super) entered: Arc<AtomicBool>,
    pub(super) release: Arc<AtomicBool>,
}

impl lash_core::runtime::RuntimeTurnPhaseProbe for PauseAtPreparedTurn {
    fn begin(&self, phase: lash_core::runtime::RuntimeTurnPhase) {
        if phase != lash_core::runtime::RuntimeTurnPhase::PreparedTurn {
            return;
        }
        self.entered.store(true, Ordering::SeqCst);
        while !self.release.load(Ordering::SeqCst) {
            std::thread::yield_now();
        }
    }

    fn end(&self, _phase: lash_core::runtime::RuntimeTurnPhase) {}
}

pub(super) struct PauseAfterEffectLoop {
    pub(super) entered: Arc<AtomicBool>,
    pub(super) release: Arc<AtomicBool>,
}

impl lash_core::runtime::RuntimeTurnPhaseProbe for PauseAfterEffectLoop {
    fn begin(&self, _phase: lash_core::runtime::RuntimeTurnPhase) {}

    fn end(&self, phase: lash_core::runtime::RuntimeTurnPhase) {
        if phase != lash_core::runtime::RuntimeTurnPhase::EffectLoop {
            return;
        }
        self.entered.store(true, Ordering::SeqCst);
        while !self.release.load(Ordering::SeqCst) {
            std::thread::yield_now();
        }
    }
}

pub(super) async fn standard_runtime_with_transport_and_queue_store_for_session(
    backend: &lash_core::Backend,
    transport: TestProvider,
    session_id: &SessionId,
) -> (LashRuntime, Arc<RecordingStore>) {
    let store = unbound_recording_store(backend).await;
    let runtime = TestRuntime::new(backend, transport)
        .tools(Arc::new(EmptyTools))
        .host(test_host_config(backend))
        .store(store.clone())
        .with_session_id(session_id)
        .build()
        .await;
    (runtime, store)
}

/// `unbound_recording_store`'s store-only-backend twin (D1 F3): an unbound
/// store on `backend`'s session catalog under the recording decorator, for
/// tests that run no effect.
pub(super) async fn recording_unbound_store_on(
    backend: &lash_core::Backend,
) -> Arc<RecordingStore> {
    Arc::new(RecordingStore::over(backend.session_store_factory()))
}

/// Input a test sends to a turn while the turn runs (ADR 0101 §5.1: a turn is
/// addressable only while it runs or once it has ended). The test queues the
/// input before it executes the turn; the provider sends it, addressed to that
/// turn's checkpoints, when it takes its first call, and the test reads back
/// what the store admitted.
#[derive(Clone, Default)]
pub(super) struct SteerWhileRunning {
    store: Arc<std::sync::OnceLock<Arc<RecordingStore>>>,
    queued: Arc<Mutex<Vec<lash_core::PendingTurnInputDraft>>>,
    sent: Arc<Mutex<Vec<lash_core::PendingTurnInput>>>,
}

impl SteerWhileRunning {
    /// The store the queued input is sent to.
    #[expect(
        clippy::expect_used,
        reason = "test fixture: one fixture steers one store"
    )]
    pub(super) fn bind(&self, store: &Arc<RecordingStore>) {
        self.store
            .set(Arc::clone(store))
            .ok()
            .expect("a steering fixture binds its store once");
    }

    /// Queue `input` for `turn_id`'s checkpoints, filed under `source_key`.
    pub(super) fn queue(
        &self,
        session_id: &SessionId,
        turn_id: &TurnId,
        source_key: Option<String>,
        input: TurnInput,
    ) {
        let mut draft = lash_core::PendingTurnInputDraft::new(
            session_id.clone(),
            lash_core::TurnInputIngress::active_turn(
                turn_id.clone(),
                lash_core::TurnInputCheckpointBoundary::AfterWork,
            ),
            input,
        );
        draft.source_key = source_key;
        self.queued.lock_recover().push(draft);
    }

    /// Send every queued input: the provider calls this while the turn runs.
    #[expect(
        clippy::expect_used,
        reason = "test fixture: the running turn accepts its own steering input"
    )]
    pub(super) async fn send_queued(&self) {
        let drafts = std::mem::take(&mut *self.queued.lock_recover());
        if drafts.is_empty() {
            return;
        }
        let store = Arc::clone(
            self.store
                .get()
                .expect("the steering fixture is bound before the turn runs"),
        );
        for draft in drafts {
            let input =
                lash_core::store::TurnInputStore::enqueue_pending_turn_input(store.as_ref(), draft)
                    .await
                    .expect("enqueue input addressed to the running turn");
            self.sent.lock_recover().push(input);
        }
    }

    /// The inputs the store admitted, in the order they were sent.
    pub(super) fn sent(&self) -> Vec<lash_core::PendingTurnInput> {
        self.sent.lock_recover().clone()
    }

    /// A phase probe that sends the queued input as the running turn begins
    /// its before-turn hooks: for a turn that never reaches its provider. It
    /// blocks its worker on the send, so the runtime must be multi-threaded.
    pub(super) fn at_before_turn_hooks(
        &self,
    ) -> Arc<dyn lash_core::runtime::RuntimeTurnPhaseProbe> {
        Arc::new(SteerAtBeforeTurnHooks(self.clone()))
    }

    /// A mock provider answering `calls` in order that sends the queued
    /// input before it answers the first.
    pub(super) fn provider(&self, calls: Vec<MockCall>) -> TestProvider {
        let calls = Arc::new(Mutex::new(calls));
        let steer = self.clone();
        TestProvider::builder()
            .kind("mock")
            .requires_streaming(true)
            .complete(move |req| {
                let calls = Arc::clone(&calls);
                let steer = steer.clone();
                async move {
                    steer.send_queued().await;
                    let call = calls.lock_recover().remove(0);
                    if let Some(tx) = req.stream_events.as_ref() {
                        for event in &call.stream_events {
                            tx.send(event.clone());
                        }
                    }
                    call.response
                }
            })
            .build()
    }
}

struct SteerAtBeforeTurnHooks(SteerWhileRunning);

impl lash_core::runtime::RuntimeTurnPhaseProbe for SteerAtBeforeTurnHooks {
    fn begin(&self, phase: lash_core::runtime::RuntimeTurnPhase) {
        if phase != lash_core::runtime::RuntimeTurnPhase::BeforeTurnHooks {
            return;
        }
        let steer = self.0.clone();
        // The probe runs inside the engine handler's poll. Re-entering the
        // test's runtime there (`block_in_place` + `Handle::block_on`) hands
        // the worker's core away mid-poll and intermittently left the turn's
        // attempt unpolled for good, so the send runs on a thread and runtime
        // of its own while this worker waits for it.
        std::thread::scope(|scope| {
            scope
                .spawn(|| {
                    tokio::runtime::Builder::new_current_thread()
                        .enable_all()
                        .build()
                        .expect("steer runtime")
                        .block_on(steer.send_queued());
                })
                .join()
                .expect("steer thread");
        });
    }

    fn end(&self, _phase: lash_core::runtime::RuntimeTurnPhase) {}
}

pub(super) async fn enqueue_turn_input_for_checkpoint(
    store: &RecordingStore,
    session_id: &SessionId,
    turn_id: &TurnId,
    source_key: Option<String>,
    input: TurnInput,
) -> lash_core::PendingTurnInput {
    let mut draft = lash_core::PendingTurnInputDraft::new(
        session_id.clone(),
        lash_core::TurnInputIngress::active_turn(
            turn_id.clone(),
            lash_core::TurnInputCheckpointBoundary::AfterWork,
        ),
        input,
    );
    draft.source_key = source_key;
    lash_core::store::TurnInputStore::enqueue_pending_turn_input(store, draft)
        .await
        .expect("enqueue turn input")
}

pub(super) async fn enqueue_idle_turn_input(
    store: &RecordingStore,
    session_id: &SessionId,
    text: &str,
) -> lash_core::PendingTurnInput {
    lash_core::store::TurnInputStore::enqueue_pending_turn_input(
        store,
        lash_core::PendingTurnInputDraft::new(
            session_id.clone(),
            lash_core::TurnInputIngress::NextTurn,
            TurnInput::text(text),
        ),
    )
    .await
    .expect("enqueue idle turn input")
}

pub(super) async fn enqueue_session_command(
    store: &RecordingStore,
    session_id: &SessionId,
    reason: &str,
) -> lash_core::testing::runtime_internals::QueuedWorkBatch {
    lash_core::store::QueuedWorkStore::enqueue_queued_work(
        store,
        lash_core::testing::runtime_internals::QueuedWorkBatchDraft::new(
            session_id.clone(),
            lash_core::DeliveryPolicy::EarliestSafeBoundary,
            lash_core::facade_support::SessionCommand::RefreshToolCatalog {
                reason: reason.to_string(),
            },
        ),
    )
    .await
    .expect("enqueue session command")
}

/// Enqueue `transaction` on `store`'s command lane as `runtime`'s session
/// would admit it, written against the runtime's config revision.
pub(super) async fn enqueue_config_transaction(
    store: &RecordingStore,
    runtime: &lash_core::runtime::LashRuntime,
    id: &str,
    transaction: lash_core::ConfigTransaction,
) -> lash_core::testing::runtime_internals::QueuedWorkBatch {
    let registry = runtime.config_registry().expect("config registry");
    let record = registry
        .admit(
            id,
            runtime.config_revision(),
            registry.entries(&transaction).expect("entries"),
        )
        .expect("admitted");
    lash_core::store::QueuedWorkStore::enqueue_queued_work(
        store,
        lash_core::testing::runtime_internals::QueuedWorkBatchDraft::new(
            SessionId::fixture(runtime.session_id().to_string()),
            lash_core::DeliveryPolicy::AfterCurrentTurnCommit,
            lash_core::facade_support::SessionCommand::ApplyConfigTransaction {
                transaction: Box::new(record),
            },
        ),
    )
    .await
    .expect("enqueue config transaction")
}
