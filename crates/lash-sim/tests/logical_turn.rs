use lash_sansio::SessionId;
use lash_sansio::TurnId;
use lash_sansio::sync::{LockResultExt, MutexExt};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex};

use async_trait::async_trait;
use lash_core::facade_support::SessionGraphFacadeOps;
use lash_core::runtime::{RuntimeTurnPhase, RuntimeTurnPhaseProbe};
use lash_core::{
    InputItem, LlmOutputPart, LlmResponse, SessionAppendNode, SessionNodePayload,
    ToolAttemptOutcome, ToolCall, ToolContract, ToolControl, ToolDefinition, ToolManifest,
    ToolOutcome, ToolProvider, TurnInput, facade_support::TraceRecord, facade_support::TraceSink,
    facade_support::TraceSinkError, facade_support::TurnStop,
};
use lash_sim::oracles::{
    FrameSwitchCommitObservation, FrameSwitchSeedObservation, frame_switch_follow_on_is_atomic,
    frame_switch_follow_on_precedes_pending, frame_switch_seeds,
    logical_turn_claims_settle_exactly_once,
};
use serde_json::{Value, json};

#[derive(Default)]
struct RecordingTraceSink {
    records: Mutex<Vec<TraceRecord>>,
}

impl RecordingTraceSink {
    fn snapshot(&self) -> Vec<TraceRecord> {
        self.records.lock_recover().clone()
    }
}

impl TraceSink for RecordingTraceSink {
    fn append(&self, record: &TraceRecord) -> Result<(), TraceSinkError> {
        self.records.lock_recover().push(record.clone());
        Ok(())
    }
}

struct PauseAfterFirstCommittedTurn {
    reached: Mutex<Option<std::sync::mpsc::Sender<()>>>,
    released: Mutex<bool>,
    release: Condvar,
    used: AtomicBool,
}

impl PauseAfterFirstCommittedTurn {
    fn new(reached: std::sync::mpsc::Sender<()>) -> Self {
        Self {
            reached: Mutex::new(Some(reached)),
            released: Mutex::new(false),
            release: Condvar::new(),
            used: AtomicBool::new(false),
        }
    }

    fn resume(&self) {
        *self.released.lock_recover() = true;
        self.release.notify_all();
    }
}

impl RuntimeTurnPhaseProbe for PauseAfterFirstCommittedTurn {
    fn begin(&self, _phase: RuntimeTurnPhase) {}

    #[expect(
        clippy::expect_used,
        reason = "test support: the surrounding harness code establishes this value; a refusal panics the harness with its case name by design"
    )]
    fn end(&self, phase: RuntimeTurnPhase) {
        if phase != RuntimeTurnPhase::CommittedTurn || self.used.swap(true, Ordering::SeqCst) {
            return;
        }
        if let Some(reached) = self.reached.lock_recover().take() {
            reached.send(()).expect("commit observer remains live");
        }
        let mut released = self.released.lock_recover();
        while !*released {
            released = self.release.wait(released).recover();
        }
    }
}

struct SeedSwitchTool {
    initial_nodes: Vec<SessionAppendNode>,
}

struct NoTools;

#[async_trait]
impl ToolProvider for NoTools {
    fn tool_manifests(&self) -> Vec<ToolManifest> {
        Vec::new()
    }

    fn resolve_contract(&self, _name: &str) -> Option<Arc<ToolContract>> {
        None
    }

    async fn execute(&self, _call: ToolCall<'_>) -> ToolAttemptOutcome {
        (async { ToolOutcome::err(json!("unknown tool")) })
            .await
            .into()
    }
}

#[async_trait]
impl ToolProvider for SeedSwitchTool {
    fn tool_manifests(&self) -> Vec<ToolManifest> {
        vec![switch_tool_definition().manifest()]
    }

    fn resolve_contract(&self, name: &str) -> Option<Arc<ToolContract>> {
        (name == "switch_frame").then(|| Arc::new(switch_tool_definition().contract()))
    }

    #[expect(
        clippy::expect_used,
        reason = "test support: the surrounding harness code establishes this value; a refusal panics the harness with its case name by design"
    )]
    async fn execute(&self, call: ToolCall<'_>) -> ToolAttemptOutcome {
        (async {
            assert_eq!(call.name(), "switch_frame");
            ToolOutcome::ok(json!({"switched": true})).with_control(ToolControl::SwitchAgentFrame {
                frame_key: lash_core::FrameKey::from_caller_material("sim-seeded-follow-frame")
                    .expect("non-empty caller material"),
                initial_nodes: self.initial_nodes.clone(),
                task: Some("run seeded follow-on".to_string()),
            })
        })
        .await
        .into()
    }
}

fn switch_tool_definition() -> ToolDefinition {
    ToolDefinition::raw(
        "tool:switch_frame",
        "switch_frame",
        "Switch to the seeded follow-on frame.",
        ToolDefinition::default_input_schema(),
        json!({"type": "object"}),
    )
}

fn tool_call_response() -> LlmResponse {
    LlmResponse {
        parts: vec![LlmOutputPart::ToolCall {
            call_id: "sim-switch-call".to_string(),
            tool_name: "switch_frame".to_string(),
            input_json: "{}".to_string(),
            replay: None,
        }],
        response_metadata: Default::default(),
        ..LlmResponse::default()
    }
}

fn text_response(text: &str) -> LlmResponse {
    LlmResponse {
        parts: vec![LlmOutputPart::Text {
            text: text.to_string(),
            response_meta: None,
        }],
        response_metadata: Default::default(),
        ..LlmResponse::default()
    }
}

#[expect(
    clippy::expect_used,
    reason = "test support: the surrounding harness code establishes this value; a refusal panics the harness with its case name by design"
)]
fn model() -> lash_core::ModelSpec {
    lash_core::ModelSpec::builder("logical-turn-sim")
        .context_window_tokens(200_000)
        .build()
        .expect("valid sim model")
}

#[expect(
    clippy::expect_used,
    reason = "test support: a server double that refuses to start panics the harness with its case name by design"
)]
async fn sim_engine() -> lash_sim::backend::SimEngine {
    lash_sim::backend::SimEngine::new(0x5eed_7010)
        .await
        .expect("sim engine")
}

/// The global invariants (FIG-4086) over what a scenario left on `engine`:
/// its final store, judged at the scenario's end.
#[expect(
    clippy::expect_used,
    reason = "test support: a store the checkers cannot read panics the harness with its case name by design"
)]
async fn assert_global_invariants(engine: &lash_sim::backend::SimEngine, scenario: &str) {
    let report = lash_sim::invariants::check_engine(
        format!("logical-turn/{scenario}"),
        0x5eed_7010,
        &lash_sim::invariants::HistoryRecorder::default(),
        engine,
    )
    .await
    .expect("capture the history");
    report.print_quarantined();
    assert!(report.passed(), "{}", report.failure());
}

async fn standard_core(
    provider: lash_core::facade_support::ProviderHandle,
    tools: Arc<dyn ToolProvider>,
    trace: Arc<RecordingTraceSink>,
) -> (lash::LashCore, lash_sim::backend::SimEngine) {
    standard_core_with_attachment_limit(provider, tools, trace, None).await
}

async fn standard_core_with_attachment_limit(
    provider: lash_core::facade_support::ProviderHandle,
    tools: Arc<dyn ToolProvider>,
    trace: Arc<RecordingTraceSink>,
    max_attachment_bytes: Option<u64>,
) -> (lash::LashCore, lash_sim::backend::SimEngine) {
    standard_core_on(
        sim_engine().await,
        provider,
        tools,
        trace,
        max_attachment_bytes,
    )
}

#[expect(
    clippy::expect_used,
    reason = "test support: the surrounding harness code establishes this value; a refusal panics the harness with its case name by design"
)]
fn standard_core_on(
    engine: lash_sim::backend::SimEngine,
    provider: lash_core::facade_support::ProviderHandle,
    tools: Arc<dyn ToolProvider>,
    trace: Arc<RecordingTraceSink>,
    max_attachment_bytes: Option<u64>,
) -> (lash::LashCore, lash_sim::backend::SimEngine) {
    let provider_id = provider.kind().to_string();
    let core = lash::LashCore::standard_builder(engine.backend(), lash::TurnBudget::Unbounded)
        .session_spec(
            lash::SessionSpec::new()
                .provider_id(provider_id)
                .turn_budget(lash::TurnBudget::Unbounded),
        )
        .provider(provider)
        .model(model())
        .tools(tools)
        .commit_budget(lash_core::CommitBudget::bounded(1024 * 1024, 512))
        .queued_work_batching(lash_core::QueuedWorkBatchingConfig::new(1))
        .max_attachment_bytes(max_attachment_bytes)
        .trace_sink(trace)
        .build(lash::persistence::LeaseOwnerIdentity::opaque(
            "logical-turn-test",
            "logical-turn-test-boot",
        ))
        .expect("build logical-turn sim core");
    (core, engine)
}

fn canonical_seed_nodes(state: &lash_core::SessionSnapshot, frame_id: &str) -> Vec<Value> {
    state
        .session_graph
        .nodes
        .iter()
        .filter(|node| {
            state
                .session_graph
                .nearest_frame_node_id(Some(&node.node_id))
                .map(lash_core::NodeId::as_str)
                == Some(frame_id)
        })
        .filter_map(|node| match &node.payload {
            SessionNodePayload::Plugin { plugin_type, body } => Some(json!({
                "kind": "plugin",
                "plugin_type": plugin_type,
                "body": body.as_ref(),
            })),
            _ => None,
        })
        .collect()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn claimed_switch_is_seeded_atomic_ordered_and_exactly_once() {
    let expected_seed_nodes = vec![
        json!({"kind": "plugin", "plugin_type": "sim.seed.alpha", "body": {"value": 1}}),
        json!({"kind": "plugin", "plugin_type": "sim.seed.beta", "body": {"value": 2}}),
    ];
    let initial_nodes = vec![
        SessionAppendNode::plugin("sim.seed.alpha", json!({"value": 1})),
        SessionAppendNode::plugin("sim.seed.beta", json!({"value": 2})),
    ];
    let trace = Arc::new(RecordingTraceSink::default());
    let completions = Arc::new(Mutex::new(Vec::<String>::new()));
    let provider_call = Arc::new(AtomicUsize::new(0));
    let (first_provider_started_tx, first_provider_started_rx) = tokio::sync::oneshot::channel();
    let first_provider_started_tx = Arc::new(Mutex::new(Some(first_provider_started_tx)));
    let (release_first_provider_tx, release_first_provider_rx) = tokio::sync::oneshot::channel();
    let release_first_provider_rx =
        Arc::new(tokio::sync::Mutex::new(Some(release_first_provider_rx)));
    let provider = lash_core::testing::TestProvider::builder()
        .kind("logical-turn-sim")
        .complete({
            let provider_call = Arc::clone(&provider_call);
            let completions = Arc::clone(&completions);
            let first_provider_started_tx = Arc::clone(&first_provider_started_tx);
            let release_first_provider_rx = Arc::clone(&release_first_provider_rx);
            // Each exchange is scripted by the input it answers, not by call
            // order: the engine drives every input as soon as it is accepted.
            move |request| {
                let provider_call = Arc::clone(&provider_call);
                let completions = Arc::clone(&completions);
                let first_provider_started_tx = Arc::clone(&first_provider_started_tx);
                let release_first_provider_rx = Arc::clone(&release_first_provider_rx);
                let request = serde_json::to_string(&request).unwrap_or_default();
                async move {
                    provider_call.fetch_add(1, Ordering::SeqCst);
                    Ok(if request.contains("second queued turn") {
                        completions.lock_recover().push("pending-next".to_string());
                        text_response("pending next complete")
                    } else if request.contains("run seeded follow-on") {
                        completions.lock_recover().push("follow-on".to_string());
                        text_response("seeded follow-on complete")
                    } else {
                        if let Some(started) = first_provider_started_tx.lock_recover().take() {
                            let _ = started.send(());
                        }
                        if let Some(release) = release_first_provider_rx.lock().await.take() {
                            let _ = release.await;
                        }
                        tool_call_response()
                    })
                }
            }
        })
        .build()
        .into_handle();
    // The law holds the engine's drive while a root runs; the hold reaches
    // the running drive at its next step, which needs a server whose attempts
    // run concurrently.
    let engine = lash_sim::backend::SimEngine::new(0x5eed_7010)
        .await
        .expect("concurrent sim engine");
    let backend = engine.backend();
    let core = lash::LashCore::standard_builder(backend, lash::TurnBudget::Unbounded)
        .commit_budget(lash::CommitBudget::bounded(1024 * 1024, 512))
        .queued_work_batching(lash::QueuedWorkBatchingConfig::new(1024))
        .provider(provider)
        .model(model())
        .tools(Arc::new(SeedSwitchTool { initial_nodes }))
        .trace_sink(trace.clone())
        .build(lash::persistence::LeaseOwnerIdentity::opaque(
            "logical-turn-test",
            "logical-turn-test-boot",
        ))
        .expect("build logical-turn sim core");
    let session = created_session(&core, "logical-turn-sim")
        .await
        .open()
        .await
        .expect("open sim session");
    let (reached_tx, reached_rx) = std::sync::mpsc::channel();
    let pause = Arc::new(PauseAfterFirstCommittedTurn::new(reached_tx));
    session.set_turn_phase_probe(pause.clone()).await;
    let first = session
        .send(TurnInput::text("first queued turn"))
        .id("first")
        .await
        .expect("send first turn");
    first_provider_started_rx
        .await
        .expect("first provider call started");
    let second = session
        .send(TurnInput::text("second queued turn"))
        .id("second")
        .await
        .expect("send second turn while chain is active");
    release_first_provider_tx
        .send(())
        .expect("release first provider call");
    tokio::task::spawn_blocking(move || reached_rx.recv())
        .await
        .expect("join commit observer")
        .expect("switch commit reached observable boundary");

    let pending_at_commit = session
        .durable()
        .pending_turn_inputs()
        .await
        .expect("pending inputs at switch commit");
    let catalog = lash::Backend::session_store_factory(&engine.backend());
    let owed_at_commit = lash_core::store::SessionCommitStore::load_session_head_meta(
        catalog.as_ref(),
        &SessionId::from("logical-turn-sim"),
    )
    .await
    .expect("head at switch commit")
    .expect("the switch committed a head")
    .pending_follow_on;
    let inbound_completed = pending_at_commit
        .iter()
        .all(|input| input.input.input_id != *first.input_id());
    let second_still_pending = pending_at_commit
        .iter()
        .any(|input| input.input.input_id == *second.input_id());
    let expected_frame_id = lash_core::facade_support::frame_node_id(
        &SessionId::from("logical-turn-sim"),
        lash_core::FrameKey::from_caller_material("sim-seeded-follow-frame")
            .expect("non-empty caller material")
            .as_str(),
    );
    let follow_on_owed = owed_at_commit.is_some_and(|owed| {
        owed.frame_id.as_str() == expected_frame_id.as_str() && owed.task == "run seeded follow-on"
    });
    assert!(
        second_still_pending,
        "unrelated queued input must remain pending"
    );
    assert!(
        frame_switch_follow_on_is_atomic(&[FrameSwitchCommitObservation {
            turn_id: TurnId::from("first"),
            inbound_claim_completed: inbound_completed,
            follow_on_owed,
        }])
        .is_passed()
    );
    // Hold the engine's drive before the root goes on: the root runs its
    // follow-on to its answer, and the held drive admits nothing after it, so
    // the unrelated input is still pending once the root has settled.
    let hold = engine.hold_session_drive(&session).await;
    pause.resume();

    let first_output = first.output().await.expect("the first root settles");
    assert_eq!(
        first_output.assistant_message(),
        Some("seeded follow-on complete")
    );
    let frame_node_id = first_output
        .result
        .state
        .current_frame_node_id
        .as_deref()
        .expect("seeded follow-on frame");
    let observed_seed_nodes = canonical_seed_nodes(&first_output.result.state, frame_node_id);
    assert!(
        frame_switch_seeds(&[FrameSwitchSeedObservation {
            protocol: "standard".to_string(),
            expected_nodes: expected_seed_nodes,
            observed_nodes: observed_seed_nodes,
        }])
        .is_passed()
    );
    assert_eq!(
        session
            .durable()
            .pending_turn_inputs()
            .await
            .expect("pending after follow-on")
            .iter()
            .map(|input| input.input.input_id.as_str())
            .collect::<Vec<_>>(),
        vec![second.input_id().as_str()]
    );

    hold.release();
    let second_output = second.output().await.expect("the second input settles");
    assert_eq!(
        second_output.assistant_message(),
        Some("pending next complete")
    );
    assert!(
        frame_switch_follow_on_precedes_pending(
            &completions.lock_recover(),
            "follow-on",
            &["pending-next".to_string()],
        )
        .is_passed()
    );
    let claim_verdict = logical_turn_claims_settle_exactly_once(&trace.snapshot());
    assert!(
        claim_verdict.is_passed(),
        "all input and handoff claims must settle once: {claim_verdict:?}"
    );
    assert!(
        session
            .durable()
            .pending_turn_inputs()
            .await
            .expect("final inputs")
            .is_empty()
    );
    assert!(
        session
            .durable()
            .queued_work()
            .await
            .expect("final queue")
            .is_empty()
    );
    assert_global_invariants(&engine, "claimed-switch").await;
}

struct BoundedSwitchTools {
    switch_count: usize,
}

impl BoundedSwitchTools {
    fn definition(index: usize) -> ToolDefinition {
        ToolDefinition::raw(
            format!("tool:terminal_tool_{index}"),
            format!("terminal_tool_{index}"),
            "Switch to the next frame in the bounded chain.",
            ToolDefinition::default_input_schema(),
            json!({"type": "object"}),
        )
    }
}

#[async_trait]
impl ToolProvider for BoundedSwitchTools {
    fn tool_manifests(&self) -> Vec<ToolManifest> {
        (0..self.switch_count)
            .map(|index| Self::definition(index).manifest())
            .collect()
    }

    fn resolve_contract(&self, name: &str) -> Option<Arc<ToolContract>> {
        let index = name.strip_prefix("terminal_tool_")?.parse::<usize>().ok()?;
        (index < self.switch_count).then(|| Arc::new(Self::definition(index).contract()))
    }

    #[expect(
        clippy::expect_used,
        reason = "test support: the surrounding harness code establishes this value; a refusal panics the harness with its case name by design"
    )]
    async fn execute(&self, call: ToolCall<'_>) -> ToolAttemptOutcome {
        (async {
            let index = call
                .name()
                .strip_prefix("terminal_tool_")
                .and_then(|value| value.parse::<usize>().ok())
                .expect("bounded switch tool name");
            ToolOutcome::ok(json!({"switch": index})).with_control(ToolControl::SwitchAgentFrame {
                frame_key: lash_core::FrameKey::from_caller_material(&format!(
                    "bounded-frame-{index}"
                ))
                .expect("non-empty caller material"),
                initial_nodes: vec![SessionAppendNode::plugin(
                    "sim.bounded.seed",
                    json!({"index": index}),
                )],
                task: Some(format!("continue bounded chain {index}")),
            })
        })
        .await
        .into()
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn claims_settle_for_finish_cancel_error_and_chain_bound() {
    let finish_trace = Arc::new(RecordingTraceSink::default());
    let finish_provider = lash_core::testing::TestProvider::builder()
        .kind("logical-turn-finish")
        .complete(|_| async { Ok(text_response("finished")) })
        .build()
        .into_handle();
    let (finish_core, finish_engine) =
        standard_core(finish_provider, Arc::new(NoTools), finish_trace.clone()).await;
    let finish_session = created_session(&finish_core, "logical-turn-finish")
        .await
        .open()
        .await
        .expect("open finish session");
    finish_session
        .send(TurnInput::text("finish claimed input"))
        .output()
        .await
        .expect("finish input runs");
    assert_global_invariants(&finish_engine, "claims-settle-finish").await;
    drop(finish_engine);
    let finish_verdict = logical_turn_claims_settle_exactly_once(&finish_trace.snapshot());
    assert!(finish_verdict.is_passed(), "{finish_verdict:?}");

    let cancel_trace = Arc::new(RecordingTraceSink::default());
    let (provider_started_tx, provider_started_rx) = tokio::sync::oneshot::channel();
    let provider_started_tx = Arc::new(Mutex::new(Some(provider_started_tx)));
    let cancel_provider = lash_core::testing::TestProvider::builder()
        .kind("logical-turn-cancel")
        .complete({
            let provider_started_tx = Arc::clone(&provider_started_tx);
            move |_| {
                let provider_started_tx = Arc::clone(&provider_started_tx);
                async move {
                    if let Some(started) = provider_started_tx.lock_recover().take() {
                        let _ = started.send(());
                    }
                    std::future::pending().await
                }
            }
        })
        .build()
        .into_handle();
    // The stop reaches the turn as a durable request over ingress while the
    // turn's attempt is running, so the stop request must be able to proceed.
    let (cancel_core, cancel_engine) = standard_core_on(
        lash_sim::backend::SimEngine::new(0x5eed_7010)
            .await
            .expect("concurrent sim engine"),
        cancel_provider,
        Arc::new(NoTools),
        cancel_trace.clone(),
        None,
    );
    let cancel_session = created_session(&cancel_core, "logical-turn-cancel")
        .await
        .open()
        .await
        .expect("open cancel session");
    let cancelled = cancel_session
        .send(TurnInput::text("cancel claimed input"))
        .await
        .expect("send cancel input");
    provider_started_rx.await.expect("cancel provider started");
    cancelled
        .cancel()
        .await
        .expect("cancel the running input's root");
    let cancelled = cancelled.output().await.expect("cancel input runs");
    assert_global_invariants(&cancel_engine, "claims-settle-cancel").await;
    drop(cancel_engine);
    assert!(matches!(
        cancelled.result.outcome,
        lash_core::facade_support::TurnOutcome::Stopped(TurnStop::Cancelled { .. })
    ));
    let cancel_verdict = logical_turn_claims_settle_exactly_once(&cancel_trace.snapshot());
    assert!(cancel_verdict.is_passed(), "{cancel_verdict:?}");

    let error_trace = Arc::new(RecordingTraceSink::default());
    let error_provider = lash_core::testing::TestProvider::builder()
        .kind("logical-turn-error")
        .complete(|_| async { panic!("normalization errors must not call the provider") })
        .build()
        .into_handle();
    // An inline attachment over the core's attachment limit fails input
    // normalization before any provider call.
    let (error_core, error_engine) = standard_core_with_attachment_limit(
        error_provider,
        Arc::new(NoTools),
        error_trace.clone(),
        Some(8),
    )
    .await;
    let error_session = created_session(&error_core, "logical-turn-error")
        .await
        .open()
        .await
        .expect("open error session");
    let invalid = error_session
        .send(TurnInput::items([InputItem::attachment(
            lash_core::AttachmentSource::inline(
                lash_core::MediaType::parse("application/pdf").unwrap(),
                vec![0_u8; 64],
            ),
        )]))
        .output()
        .await
        .expect("invalid input terminalizes");
    assert_global_invariants(&error_engine, "claims-settle-error").await;
    drop(error_engine);
    assert!(matches!(
        invalid.result.outcome,
        lash_core::facade_support::TurnOutcome::Stopped(TurnStop::InvalidInput)
    ));
    let error_verdict = logical_turn_claims_settle_exactly_once(&error_trace.snapshot());
    assert!(error_verdict.is_passed(), "{error_verdict:?}");

    const SWITCH_BOUND: usize = 16;
    let bound_trace = Arc::new(RecordingTraceSink::default());
    let call_index = Arc::new(AtomicUsize::new(0));
    let bound_provider = lash_core::testing::TestProvider::builder()
        .kind("logical-turn-bound")
        .complete({
            let call_index = Arc::clone(&call_index);
            move |_| {
                let call_index = Arc::clone(&call_index);
                async move {
                    let index = call_index.fetch_add(1, Ordering::SeqCst);
                    Ok(LlmResponse {
                        parts: vec![LlmOutputPart::ToolCall {
                            call_id: format!("switch-{index}"),
                            tool_name: format!("terminal_tool_{index}"),
                            input_json: "{}".to_string(),
                            replay: None,
                        }],
                        response_metadata: Default::default(),
                        ..LlmResponse::default()
                    })
                }
            }
        })
        .build()
        .into_handle();
    let (bound_core, bound_engine) = standard_core(
        bound_provider,
        Arc::new(BoundedSwitchTools {
            switch_count: SWITCH_BOUND,
        }),
        bound_trace.clone(),
    )
    .await;
    let bound_session = created_session(&bound_core, "logical-turn-bound")
        .await
        .open()
        .await
        .expect("open bound session");
    let bounded = bound_session
        .send(TurnInput::text("run beyond the frame switch bound"))
        .output()
        .await
        .expect("bounded chain terminalizes");
    assert_global_invariants(&bound_engine, "claims-settle-bound").await;
    drop(bound_engine);
    assert!(matches!(
        bounded.result.outcome,
        lash_core::facade_support::TurnOutcome::Stopped(TurnStop::RuntimeError)
    ));
    assert!(
        bounded
            .result
            .errors
            .iter()
            .any(|error| error.message.contains("exceeded the limit of"))
    );
    assert_eq!(call_index.load(Ordering::SeqCst), SWITCH_BOUND);
    assert!(
        bound_session
            .durable()
            .queued_work()
            .await
            .expect("bounded queue")
            .is_empty()
    );
    assert!(
        bound_session
            .durable()
            .pending_turn_inputs()
            .await
            .expect("bounded inputs")
            .is_empty()
    );
    let bound_verdict = logical_turn_claims_settle_exactly_once(&bound_trace.snapshot());
    assert!(bound_verdict.is_passed(), "{bound_verdict:?}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn rlm_continue_as_seed_materializes_in_the_new_frame() {
    let trace = Arc::new(RecordingTraceSink::default());
    let call_index = Arc::new(AtomicUsize::new(0));
    let provider = lash_core::testing::TestProvider::builder()
        .kind("logical-turn-rlm-seed")
        .complete({
            let call_index = Arc::clone(&call_index);
            move |_| {
                let call_index = Arc::clone(&call_index);
                async move {
                    let text = match call_index.fetch_add(1, Ordering::SeqCst) {
                        0 => {
                            r#"
<typescript>
await control.continue_as({
  task: "finish with the carried baton",
  seed: { baton: "rlm-sim-seed" }
});
</typescript>
"#
                        }
                        1 => {
                            r#"
<typescript>
finish({ baton: baton });
</typescript>
"#
                        }
                        index => panic!("unexpected RLM provider call {index}"),
                    };
                    Ok(text_response(text))
                }
            }
        })
        .build()
        .into_handle();
    let engine = sim_engine().await;
    let backend = engine.backend();
    let factory = lash_protocol_rlm::RlmProtocolPluginFactory::new(
        lash_protocol_rlm::RlmProtocolPluginConfig::builder()
            .channel(lash_protocol_rlm::RlmChannel::Cell)
            .instruction_limit(lash_protocol_rlm::InstructionBound::instructions(1_000_000))
            .memory_limit(lash_protocol_rlm::MemoryBound::mebibytes(64))
            .build(),
        &backend,
    );
    let core = lash::LashCore::rlm_builder(backend, lash::TurnBudget::Unbounded, factory)
        .commit_budget(lash::CommitBudget::bounded(1024 * 1024, 512))
        .queued_work_batching(lash::QueuedWorkBatchingConfig::new(1024))
        .provider(provider)
        .model(model())
        .trace_sink(trace.clone())
        .build(lash::persistence::LeaseOwnerIdentity::opaque(
            "logical-turn-test",
            "logical-turn-test-boot",
        ))
        .expect("build RLM seed sim core");
    let session = created_session(&core, "logical-turn-rlm-seed")
        .await
        .open()
        .await
        .expect("open RLM seed session");
    let terminal = session
        .send(TurnInput::text("switch with an RLM seed"))
        .output()
        .await
        .expect("RLM seed turn runs");
    assert_eq!(
        terminal
            .final_value()
            .and_then(|value| value.get("baton"))
            .and_then(Value::as_str),
        Some("rlm-sim-seed")
    );
    let observed_nodes = terminal
        .result
        .state
        .session_graph
        .nodes
        .iter()
        .filter_map(|node| match &node.payload {
            SessionNodePayload::Event {
                event: lash_core::SessionHistoryRecord::Protocol(event),
            } => lash_protocol_rlm::decode_rlm_protocol_event(event),
            _ => None,
        })
        .filter_map(|event| match event {
            lash_rlm_types::RlmProtocolEvent::RlmSeed(seed) => serde_json::to_value(seed).ok(),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert!(
        frame_switch_seeds(&[FrameSwitchSeedObservation {
            protocol: "rlm".to_string(),
            expected_nodes: vec![json!({"globals": {"baton": "rlm-sim-seed"}})],
            observed_nodes,
        }])
        .is_passed()
    );
    let claim_verdict = logical_turn_claims_settle_exactly_once(&trace.snapshot());
    assert!(claim_verdict.is_passed(), "{claim_verdict:?}");
    assert_eq!(call_index.load(Ordering::SeqCst), 2);
    assert_global_invariants(&engine, "rlm-continue-as").await;
}

/// FIG-3085: a top-level binding that shadows the `control` module disarms
/// `control.continue_as` for every later turn of the session. The frame switch
/// never happens, the driver refuses the same cell until its no-progress budget
/// is spent, and the turn commits `Stopped(MaxTurns)` with no final value --
/// which is how the distributed workers E2E surfaced it as
/// `[500] queued frame-switch follow-on produced no final value`. The budget is
/// unbounded here, so `MaxTurns` can only come from the no-progress path.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_shadowed_control_module_stops_the_frame_switch_turn_without_a_final_value() {
    let call_index = Arc::new(AtomicUsize::new(0));
    let requests = Arc::new(Mutex::new(Vec::<String>::new()));
    let provider = lash_core::testing::TestProvider::builder()
        .kind("logical-turn-rlm-shadowed-control")
        .complete({
            let call_index = Arc::clone(&call_index);
            let requests = Arc::clone(&requests);
            move |request| {
                let call_index = Arc::clone(&call_index);
                let requests = Arc::clone(&requests);
                async move {
                    requests
                        .lock_recover()
                        .push(serde_json::to_string(&request).unwrap_or_default());
                    let text = if call_index.fetch_add(1, Ordering::SeqCst) == 0 {
                        r#"
<typescript>
const control = "local shadow";
finish({ bound: control });
</typescript>
"#
                    } else {
                        r#"
<typescript>
await control.continue_as({
  task: "finish with the carried baton",
  seed: { baton: "rlm-sim-seed" }
});
</typescript>
"#
                    };
                    Ok(text_response(text))
                }
            }
        })
        .build()
        .into_handle();
    let engine = sim_engine().await;
    let backend = engine.backend();
    let factory = lash_protocol_rlm::RlmProtocolPluginFactory::new(
        lash_protocol_rlm::RlmProtocolPluginConfig::builder()
            .channel(lash_protocol_rlm::RlmChannel::Cell)
            .instruction_limit(lash_protocol_rlm::InstructionBound::instructions(1_000_000))
            .memory_limit(lash_protocol_rlm::MemoryBound::mebibytes(64))
            .build(),
        &backend,
    );
    let core = lash::LashCore::rlm_builder(backend, lash::TurnBudget::Unbounded, factory)
        .commit_budget(lash::CommitBudget::bounded(1024 * 1024, 512))
        .queued_work_batching(lash::QueuedWorkBatchingConfig::new(1024))
        .provider(provider)
        .model(model())
        .build(lash::persistence::LeaseOwnerIdentity::opaque(
            "logical-turn-test",
            "logical-turn-test-boot",
        ))
        .expect("build RLM shadowed-control sim core");
    let session = created_session(&core, "logical-turn-rlm-shadowed-control")
        .await
        .open()
        .await
        .expect("open shadowed-control session");
    let bound = session
        .send(TurnInput::text("bind a local `control`"))
        .output()
        .await
        .expect("binding turn runs");
    assert_eq!(
        bound
            .final_value()
            .and_then(|value| value.get("bound"))
            .and_then(Value::as_str),
        Some("local shadow")
    );
    let calls_after_binding = call_index.load(Ordering::SeqCst);

    let switched = session
        .send(TurnInput::text("switch with an RLM seed"))
        .output()
        .await
        .expect("frame-switch turn runs");
    assert!(
        matches!(
            switched.result.outcome,
            lash_core::facade_support::TurnOutcome::Stopped(TurnStop::MaxTurns)
        ),
        "expected the shadowed frame switch to stop on the no-progress budget, got {:?}",
        switched.result.outcome
    );
    assert!(
        switched.final_value().is_none(),
        "a stopped turn must not report a final value"
    );
    let refusals = requests
        .lock_recover()
        .iter()
        .filter(|request| request.contains("shadows module"))
        .count();
    assert!(
        refusals > 0,
        "expected the refusal to name the shadowed module authority"
    );
    assert_eq!(
        call_index.load(Ordering::SeqCst) - calls_after_binding,
        12,
        "the stopped turn must spend exactly the default no-progress budget"
    );
    assert_global_invariants(&engine, "shadowed-control-module").await;
}

/// Queued turn work a terminal checkpoint withholds: one process wake.
fn withheld_wake(session_id: &SessionId) -> lash_core::runtime::QueuedWorkBatchDraft {
    let process_id = lash_core::runtime::ProcessId::fixture("withheld-at-terminal");
    lash_core::runtime::process_wake_batch_draft(lash_core::runtime::ProcessWakeDelivery {
        version: lash_core::runtime::PROCESS_WAKE_DELIVERY_FORMAT_VERSION,
        wake_id: "withheld-at-terminal-wake".to_string(),
        target_session_id: session_id.clone(),
        process_id: process_id.clone(),
        sequence: 1,
        event_type: "process.wake".to_string(),
        event_invocation: lash_core::runtime::RuntimeInvocation {
            attribution: lash_core::runtime::RuntimeAttribution::for_session(session_id.as_str()),
            subject: lash_core::runtime::RuntimeSubject::ProcessEvent {
                process_id,
                sequence: 1,
                event_type: "process.wake".to_string(),
            },
            caused_by: None,
            replay: None,
        },
        process_caused_by: None,
        authority: lash_core::runtime::QueuedWorkAuthority::default(),
        input: "work withheld at terminal checkpoint".to_string(),
        created_at_ms: 1,
    })
}

#[tokio::test]
async fn terminal_checkpoint_withheld_claim_is_traced_once() {
    let engine = sim_engine().await;
    let backend = engine.backend();
    let factory: Arc<dyn lash_core::DeploymentStore> =
        lash::Backend::session_store_factory(&backend);
    let trace = Arc::new(RecordingTraceSink::default());
    let calls = Arc::new(AtomicUsize::new(0));
    let session_id = SessionId::from("logical-turn-withheld-trace");
    let provider = lash_core::testing::TestProvider::builder()
        .kind("logical-turn-withheld-trace")
        .complete({
            let factory = Arc::clone(&factory);
            let calls = Arc::clone(&calls);
            let session_id = session_id.clone();
            move |_| {
                let factory = Arc::clone(&factory);
                let calls = Arc::clone(&calls);
                let session_id = session_id.clone();
                async move {
                    if calls.fetch_add(1, Ordering::SeqCst) == 0 {
                        factory
                            .enqueue_queued_work(withheld_wake(&session_id))
                            .await
                            .unwrap();
                    }
                    Ok(text_response("physical turn finished"))
                }
            }
        })
        .build()
        .into_handle();
    let core = lash::LashCore::standard_builder(backend, lash::TurnBudget::Unbounded)
        .commit_budget(lash::CommitBudget::bounded(1024 * 1024, 512))
        .queued_work_batching(lash::QueuedWorkBatchingConfig::new(1))
        .provider(provider)
        .model(model())
        .tools(Arc::new(NoTools))
        .trace_sink(trace.clone())
        .build(lash::persistence::LeaseOwnerIdentity::opaque(
            "withheld-trace-test",
            "withheld-trace-test-boot",
        ))
        .unwrap();
    let session = created_session(&core, session_id.as_str())
        .await
        .open()
        .await
        .unwrap();
    session
        .send(TurnInput::text("start the logical run"))
        .output()
        .await
        .expect("the logical run settles");
    assert_eq!(
        calls.load(Ordering::SeqCst),
        2,
        "withheld work ran in its own physical turn"
    );
    let verdict = logical_turn_claims_settle_exactly_once(&trace.snapshot());
    assert!(verdict.is_passed(), "{verdict:?}");
    assert_global_invariants(&engine, "withheld-claim").await;
}

/// This test crate's one path to a session that may not exist yet
/// (FIG-4112): only `create` creates, so this creates `session_id` with the
/// core's config unless the catalog already holds it, then hands back the
/// builder for the verb under test. An existing or deleted id is left for
/// that verb to report.
async fn created_session(
    core: &lash::LashCore,
    session_id: impl Into<lash::SessionId>,
) -> lash::SessionBuilder {
    let session_id = session_id.into();
    match core
        .session(session_id.clone())
        .create(lash::SessionCreation::default())
        .await
    {
        Ok(_)
        | Err(lash::EmbedError::SessionAlreadyExists { .. })
        | Err(lash::EmbedError::Store(lash::persistence::StoreError::SessionDeleted { .. })) => {}
        Err(error) => panic!("create session `{session_id}`: {error:?}"),
    }
    core.session(session_id)
}
