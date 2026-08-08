use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex};

use async_trait::async_trait;
use lash_core::facade_support::SessionGraphFacadeOps;
use lash_core::runtime::{RuntimeTurnPhase, RuntimeTurnPhaseProbe};
use lash_core::{
    InputItem, LlmOutputPart, LlmResponse, SessionAppendNode, SessionNodePayload, ToolCall,
    ToolContract, ToolControl, ToolDefinition, ToolManifest, ToolProvider, ToolResult, TurnInput,
    facade_support::TraceRecord, facade_support::TraceSink, facade_support::TraceSinkError,
    facade_support::TurnStop,
};
use lash_sim::oracles::{
    FrameSwitchCommitObservation, FrameSwitchSeedObservation,
    frame_switch_follow_on_precedes_pending, frame_switch_outbox_is_atomic, frame_switch_seeds,
    logical_turn_claims_settle_exactly_once,
};
use serde_json::{Value, json};

#[derive(Default)]
struct RecordingTraceSink {
    records: Mutex<Vec<TraceRecord>>,
}

impl RecordingTraceSink {
    fn snapshot(&self) -> Vec<TraceRecord> {
        self.records.lock().expect("trace records").clone()
    }
}

impl TraceSink for RecordingTraceSink {
    fn append(&self, record: &TraceRecord) -> Result<(), TraceSinkError> {
        self.records
            .lock()
            .expect("trace records")
            .push(record.clone());
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
        *self.released.lock().expect("commit pause") = true;
        self.release.notify_all();
    }
}

impl RuntimeTurnPhaseProbe for PauseAfterFirstCommittedTurn {
    fn begin(&self, _phase: RuntimeTurnPhase) {}

    fn end(&self, phase: RuntimeTurnPhase) {
        if phase != RuntimeTurnPhase::CommittedTurn || self.used.swap(true, Ordering::SeqCst) {
            return;
        }
        if let Some(reached) = self.reached.lock().expect("commit signal").take() {
            reached.send(()).expect("commit observer remains live");
        }
        let mut released = self.released.lock().expect("commit pause");
        while !*released {
            released = self.release.wait(released).expect("commit pause wait");
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

    async fn execute(&self, _call: ToolCall<'_>) -> ToolResult {
        ToolResult::err(json!("unknown tool"))
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

    async fn execute(&self, call: ToolCall<'_>) -> ToolResult {
        assert_eq!(call.name, "switch_frame");
        ToolResult::ok(json!({"switched": true})).with_control(ToolControl::SwitchAgentFrame {
            frame_id: "sim-seeded-follow-frame".to_string(),
            initial_nodes: self.initial_nodes.clone(),
            task: Some("run seeded follow-on".to_string()),
        })
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
        full_text: text.to_string(),
        parts: vec![LlmOutputPart::Text {
            text: text.to_string(),
            response_meta: None,
        }],
        response_metadata: Default::default(),
        ..LlmResponse::default()
    }
}

fn model() -> lash_core::ModelSpec {
    lash_core::ModelSpec::builder("logical-turn-sim")
        .context_window_tokens(200_000)
        .build()
        .expect("valid sim model")
}

fn standard_core(
    provider: lash_core::facade_support::ProviderHandle,
    tools: Arc<dyn ToolProvider>,
    trace: Arc<RecordingTraceSink>,
) -> lash::LashCore {
    standard_core_with_attachment_policy(
        provider,
        tools,
        trace,
        Arc::new(lash_core::facade_support::OpenAttachmentSourcePolicy),
    )
}

fn standard_core_with_attachment_policy(
    provider: lash_core::facade_support::ProviderHandle,
    tools: Arc<dyn ToolProvider>,
    trace: Arc<RecordingTraceSink>,
    attachment_source_policy: Arc<dyn lash_core::facade_support::AttachmentSourcePolicy>,
) -> lash::LashCore {
    lash::LashCore::standard_builder()
        .effect_host(Arc::new(lash::durability::InlineEffectHost::default()))
        .attachment_store(Arc::new(lash::persistence::InMemoryAttachmentStore::new()))
        .process_env_store(Arc::new(
            lash::persistence::InMemoryProcessExecutionEnvStore::new(),
        ))
        .store_factory(Arc::new(
            lash::persistence::InMemorySessionStoreFactory::new(),
        ))
        .provider(provider)
        .model(model())
        .tools(tools)
        .trace_sink(trace)
        .disable_queued_work_driver()
        .advanced()
        .runtime_host_config(
            lash_core::facade_support::RuntimeHostConfig::in_memory()
                .with_attachment_source_policy(attachment_source_policy),
        )
        .build()
        .expect("build logical-turn sim core")
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
            move |_| {
                let provider_call = Arc::clone(&provider_call);
                let completions = Arc::clone(&completions);
                let first_provider_started_tx = Arc::clone(&first_provider_started_tx);
                let release_first_provider_rx = Arc::clone(&release_first_provider_rx);
                async move {
                    Ok(match provider_call.fetch_add(1, Ordering::SeqCst) {
                        0 => {
                            if let Some(started) = first_provider_started_tx
                                .lock()
                                .expect("first provider signal")
                                .take()
                            {
                                let _ = started.send(());
                            }
                            if let Some(release) = release_first_provider_rx.lock().await.take() {
                                let _ = release.await;
                            }
                            tool_call_response()
                        }
                        1 => {
                            completions
                                .lock()
                                .expect("completion order")
                                .push("follow-on".to_string());
                            text_response("seeded follow-on complete")
                        }
                        2 => {
                            completions
                                .lock()
                                .expect("completion order")
                                .push("pending-next".to_string());
                            text_response("pending next complete")
                        }
                        index => panic!("unexpected provider call {index}"),
                    })
                }
            }
        })
        .build()
        .into_handle();
    let core = lash::LashCore::standard_builder()
        .effect_host(Arc::new(lash::durability::InlineEffectHost::default()))
        .attachment_store(Arc::new(lash::persistence::InMemoryAttachmentStore::new()))
        .process_env_store(Arc::new(
            lash::persistence::InMemoryProcessExecutionEnvStore::new(),
        ))
        .store_factory(Arc::new(
            lash::persistence::InMemorySessionStoreFactory::new(),
        ))
        .provider(provider)
        .model(model())
        .tools(Arc::new(SeedSwitchTool { initial_nodes }))
        .trace_sink(trace.clone())
        .disable_queued_work_driver()
        .build()
        .expect("build logical-turn sim core");
    let session = core
        .session("logical-turn-sim")
        .open()
        .await
        .expect("open sim session");
    let first = session
        .enqueue(TurnInput::text("first queued turn"))
        .id("first")
        .send()
        .await
        .expect("enqueue first turn");
    let (reached_tx, reached_rx) = std::sync::mpsc::channel();
    let pause = Arc::new(PauseAfterFirstCommittedTurn::new(reached_tx));
    session.set_turn_phase_probe(pause.clone()).await;
    let drain_session = session.clone();
    let first_drain = tokio::spawn(async move { drain_session.queued_turn().run().await });
    first_provider_started_rx
        .await
        .expect("first provider call started");
    let second = session
        .enqueue(TurnInput::text("second queued turn"))
        .id("second")
        .send()
        .await
        .expect("enqueue second turn while chain is active");
    release_first_provider_tx
        .send(())
        .expect("release first provider call");
    tokio::task::spawn_blocking(move || reached_rx.recv())
        .await
        .expect("join commit observer")
        .expect("switch commit reached observable boundary");

    let pending_at_commit = session
        .pending_turn_inputs()
        .await
        .expect("pending inputs at switch commit");
    let queued_at_commit = session
        .queued_work()
        .await
        .expect("outbox at switch commit");
    let inbound_completed = pending_at_commit
        .iter()
        .all(|input| input.input_id != first.input_id);
    let second_still_pending = pending_at_commit
        .iter()
        .any(|input| input.input_id == second.input_id);
    let follow_on_enqueued = queued_at_commit.iter().any(|batch| {
        batch.items.iter().any(|item| {
            matches!(
                &item.payload,
                lash_core::runtime::QueuedWorkPayload::AgentFrameTask { frame_id, task, .. }
                    if frame_id == "sim-seeded-follow-frame" && task == "run seeded follow-on"
            )
        })
    });
    assert!(
        second_still_pending,
        "unrelated queued input must remain pending"
    );
    assert!(
        frame_switch_outbox_is_atomic(&[FrameSwitchCommitObservation {
            turn_id: "first".to_string(),
            inbound_claim_completed: inbound_completed,
            follow_on_enqueued,
        }])
        .is_passed()
    );
    pause.resume();

    let first_output = first_drain
        .await
        .expect("join first drain")
        .expect("first drain succeeds")
        .expect("first drain ran a turn");
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
            .pending_turn_inputs()
            .await
            .expect("pending after follow-on")
            .iter()
            .map(|input| input.input_id.as_str())
            .collect::<Vec<_>>(),
        vec![second.input_id.as_str()]
    );

    let second_output = session
        .queued_turn()
        .run()
        .await
        .expect("second drain succeeds")
        .expect("second queued turn ran");
    assert_eq!(
        second_output.assistant_message(),
        Some("pending next complete")
    );
    assert!(
        frame_switch_follow_on_precedes_pending(
            &completions.lock().expect("completion order"),
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
            .pending_turn_inputs()
            .await
            .expect("final inputs")
            .is_empty()
    );
    assert!(session.queued_work().await.expect("final queue").is_empty());
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

    async fn execute(&self, call: ToolCall<'_>) -> ToolResult {
        let index = call
            .name
            .strip_prefix("terminal_tool_")
            .and_then(|value| value.parse::<usize>().ok())
            .expect("bounded switch tool name");
        ToolResult::ok(json!({"switch": index})).with_control(ToolControl::SwitchAgentFrame {
            frame_id: format!("bounded-frame-{index}"),
            initial_nodes: vec![SessionAppendNode::plugin(
                "sim.bounded.seed",
                json!({"index": index}),
            )],
            task: Some(format!("continue bounded chain {index}")),
        })
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn claims_settle_for_finish_cancel_error_and_chain_bound() {
    #[derive(Debug)]
    struct DenyAttachments;

    impl lash_core::facade_support::AttachmentSourcePolicy for DenyAttachments {
        fn authorize(
            &self,
            producer: &lash_core::facade_support::AttachmentProducer,
            _source: &lash_core::AttachmentSource,
        ) -> Result<(), lash_core::facade_support::AttachmentSourcePolicyError> {
            Err(lash_core::facade_support::AttachmentSourcePolicyError {
                producer: producer.clone(),
                reason: "attachment denied for logical-turn settlement test".to_string(),
            })
        }
    }

    let finish_trace = Arc::new(RecordingTraceSink::default());
    let finish_provider = lash_core::testing::TestProvider::builder()
        .kind("logical-turn-finish")
        .complete(|_| async { Ok(text_response("finished")) })
        .build()
        .into_handle();
    let finish_core = standard_core(finish_provider, Arc::new(NoTools), finish_trace.clone());
    let finish_session = finish_core
        .session("logical-turn-finish")
        .open()
        .await
        .expect("open finish session");
    finish_session
        .enqueue(TurnInput::text("finish claimed input"))
        .send()
        .await
        .expect("enqueue finish input");
    finish_session
        .queued_turn()
        .run()
        .await
        .expect("finish drain succeeds")
        .expect("finish input runs");
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
                    if let Some(started) = provider_started_tx
                        .lock()
                        .expect("cancel provider signal")
                        .take()
                    {
                        let _ = started.send(());
                    }
                    std::future::pending().await
                }
            }
        })
        .build()
        .into_handle();
    let cancel_core = standard_core(cancel_provider, Arc::new(NoTools), cancel_trace.clone());
    let cancel_session = cancel_core
        .session("logical-turn-cancel")
        .open()
        .await
        .expect("open cancel session");
    cancel_session
        .enqueue(TurnInput::text("cancel claimed input"))
        .send()
        .await
        .expect("enqueue cancel input");
    let drain_session = cancel_session.clone();
    let cancelled = tokio::spawn(async move { drain_session.queued_turn().run().await });
    provider_started_rx.await.expect("cancel provider started");
    assert_eq!(cancel_session.cancel_running_turns(), 1);
    let cancelled = cancelled
        .await
        .expect("join cancelled turn")
        .expect("cancel drain succeeds")
        .expect("cancel input runs");
    assert!(matches!(
        cancelled.result.outcome,
        lash_core::facade_support::TurnOutcome::Stopped(TurnStop::Cancelled)
    ));
    let cancel_verdict = logical_turn_claims_settle_exactly_once(&cancel_trace.snapshot());
    assert!(cancel_verdict.is_passed(), "{cancel_verdict:?}");

    let error_trace = Arc::new(RecordingTraceSink::default());
    let error_provider = lash_core::testing::TestProvider::builder()
        .kind("logical-turn-error")
        .complete(|_| async { panic!("normalization errors must not call the provider") })
        .build()
        .into_handle();
    let error_core = standard_core_with_attachment_policy(
        error_provider,
        Arc::new(NoTools),
        error_trace.clone(),
        Arc::new(DenyAttachments),
    );
    let error_session = error_core
        .session("logical-turn-error")
        .open()
        .await
        .expect("open error session");
    error_session
        .enqueue(TurnInput::items([InputItem::attachment(
            lash_core::AttachmentSource::external_url(
                lash_core::MediaType::parse("application/pdf").unwrap(),
                "https://example.test/denied.pdf",
            ),
        )]))
        .send()
        .await
        .expect("enqueue invalid input");
    let invalid = error_session
        .queued_turn()
        .run()
        .await
        .expect("invalid drain succeeds")
        .expect("invalid input terminalizes");
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
    let bound_core = standard_core(
        bound_provider,
        Arc::new(BoundedSwitchTools {
            switch_count: SWITCH_BOUND,
        }),
        bound_trace.clone(),
    );
    let bound_session = bound_core
        .session("logical-turn-bound")
        .open()
        .await
        .expect("open bound session");
    bound_session
        .enqueue(TurnInput::text("run beyond the frame switch bound"))
        .send()
        .await
        .expect("enqueue bounded chain");
    let bounded = bound_session
        .queued_turn()
        .run()
        .await
        .expect("bounded chain drain succeeds")
        .expect("bounded chain terminalizes");
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
            .queued_work()
            .await
            .expect("bounded queue")
            .is_empty()
    );
    assert!(
        bound_session
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
<lashlang>
await control.continue_as({
  task: "finish with the carried baton",
  seed: { baton: "rlm-sim-seed" }
})?
</lashlang>
"#
                        }
                        1 => {
                            r#"
<lashlang>
finish { baton: baton }
</lashlang>
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
    let factory = lash_protocol_rlm::RlmProtocolPluginFactory::new(
        lash_protocol_rlm::RlmProtocolPluginConfig::default(),
        Arc::new(lash::persistence::InMemoryLashlangArtifactStore::new()),
    );
    let core = lash::LashCore::rlm_builder(factory)
        .effect_host(Arc::new(lash::durability::InlineEffectHost::default()))
        .attachment_store(Arc::new(lash::persistence::InMemoryAttachmentStore::new()))
        .process_env_store(Arc::new(
            lash::persistence::InMemoryProcessExecutionEnvStore::new(),
        ))
        .store_factory(Arc::new(
            lash::persistence::InMemorySessionStoreFactory::new(),
        ))
        .process_registry(Arc::new(lash_core::TestLocalProcessRegistry::default())
            as Arc<dyn lash_core::ProcessRegistry>)
        .provider(provider)
        .model(model())
        .trace_sink(trace.clone())
        .disable_queued_work_driver()
        .build()
        .expect("build RLM seed sim core");
    let session = core
        .session("logical-turn-rlm-seed")
        .open()
        .await
        .expect("open RLM seed session");
    session
        .enqueue(TurnInput::text("switch with an RLM seed"))
        .send()
        .await
        .expect("enqueue RLM seed turn");
    let terminal = session
        .queued_turn()
        .run()
        .await
        .expect("RLM seed drain succeeds")
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
}
