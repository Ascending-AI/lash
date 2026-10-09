use lash_sansio::SessionId;
use lash_sansio::sync::MutexExt;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use lash_core::facade_support::SessionGraphFacadeOps;
use lash_core::{
    LlmOutputPart, LlmResponse, SessionAppendNode, SessionNodePayload, ToolAttemptOutcome,
    ToolCall, ToolContract, ToolControl, ToolDefinition, ToolManifest, ToolOutcome, ToolProvider,
    TurnInput, facade_support::TraceRecord, facade_support::TraceSink,
    facade_support::TraceSinkError, facade_support::TurnStop,
};
use serde_json::{Value, json};

#[derive(Default)]
struct RecordingTraceSink {
    records: Mutex<Vec<TraceRecord>>,
}

impl TraceSink for RecordingTraceSink {
    fn append(&self, record: &TraceRecord) -> Result<(), TraceSinkError> {
        self.records.lock_recover().push(record.clone());
        Ok(())
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

#[expect(clippy::expect_used, reason = "this fixture declares valid schemas")]
fn switch_tool_definition() -> ToolDefinition {
    ToolDefinition::raw(
        "tool:switch_frame",
        "switch_frame",
        "Switch to the seeded follow-on frame.",
        ToolDefinition::default_input_schema(),
        json!({"type": "object"}),
    )
    .expect("valid declared tool schemas")
    .with_execution(std::time::Duration::from_secs(120))
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
fn model() -> lash_core::LlmProfileMetadata {
    lash_core::LlmProfileMetadata::builder("logical-turn-sim")
        .cache_retention(lash_core::provider::CacheRetention::Short)
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
    let core = lash::LashCore::standard_builder(engine.backend())
        .serve_test_llm_profile(provider, model())
        .tools(tools)
        .commit_budget(lash_core::CommitBudget::bounded(1024 * 1024, 512))
        .queued_work_batching(lash_core::QueuedWorkBatchingConfig::new(1))
        .tool_source_policy(lash_core::ToolSourcePolicy::Tolerate)
        .execution_budgets(lash::ExecutionBudgets::recommended())
        .delta_coalescing(lash::DeltaCoalescing::recommended())
        .data_retention(lash::DataRetention {
            attachments: lash::persistence::AttachmentPolicy {
                max_attachment_bytes,
                ..lash::persistence::AttachmentPolicy::standard()
            },
            ..lash::DataRetention::standard()
        })
        .trace_sink(trace)
        .build(lash::persistence::LeaseOwnerIdentity::opaque(
            lash::persistence::LeaseOwnerId::new("logical-turn-test"),
            lash::persistence::LeaseIncarnationId::new("logical-turn-test-boot"),
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
async fn admitted_switch_is_seeded_queued_after_earlier_work_and_exactly_once() {
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
            // order: the engine executes every input as soon as it is accepted.
            move |request| {
                let provider_call = Arc::clone(&provider_call);
                let completions = Arc::clone(&completions);
                let first_provider_started_tx = Arc::clone(&first_provider_started_tx);
                let release_first_provider_rx = Arc::clone(&release_first_provider_rx);
                let request = serde_json::to_string(&request).unwrap_or_default();
                async move {
                    provider_call.fetch_add(1, Ordering::SeqCst);
                    // The follow-on runs after the queued input, so its
                    // request also carries that input's history.
                    Ok(if request.contains("run seeded follow-on") {
                        completions.lock_recover().push("follow-on".to_string());
                        text_response("seeded follow-on complete")
                    } else if request.contains("second queued turn") {
                        completions.lock_recover().push("pending-next".to_string());
                        text_response("pending next complete")
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
    let engine = lash_sim::backend::SimEngine::new(0x5eed_7010)
        .await
        .expect("concurrent sim engine");
    let backend = engine.backend();
    let core = lash::LashCore::standard_builder(backend)
        .commit_budget(lash::CommitBudget::bounded(1024 * 1024, 512))
        .data_retention(lash::DataRetention::standard())
        .queued_work_batching(lash::QueuedWorkBatchingConfig::new(1024))
        .tool_source_policy(lash_core::ToolSourcePolicy::Tolerate)
        .execution_budgets(lash::ExecutionBudgets::recommended())
        .delta_coalescing(lash::DeltaCoalescing::recommended())
        .serve_test_llm_profile(provider, model())
        .tools(Arc::new(SeedSwitchTool { initial_nodes }))
        .trace_sink(trace.clone())
        .build(lash::persistence::LeaseOwnerIdentity::opaque(
            lash::persistence::LeaseOwnerId::new("logical-turn-test"),
            lash::persistence::LeaseIncarnationId::new("logical-turn-test-boot"),
        ))
        .expect("build logical-turn sim core");
    let session = created_session(&core, "logical-turn-sim")
        .await
        .durable()
        .await
        .expect("open sim session");
    let first = session
        .send(TurnInput::text("first queued turn"))
        .id(lash::TurnId::parse("first").expect("nonblank host identity"))
        .await
        .expect("send first turn");
    first_provider_started_rx
        .await
        .expect("first provider call started");
    let second = session
        .send(TurnInput::text("second queued turn"))
        .id(lash::TurnId::parse("second").expect("nonblank host identity"))
        .await
        .expect("send second turn while chain is active");
    release_first_provider_tx
        .send(())
        .expect("release first provider call");

    let switched = first.output().await.expect("the switching run settles");
    let lash_core::facade_support::TurnOutcome::AgentFrameSwitch {
        frame_key, task, ..
    } = &switched.result.outcome
    else {
        panic!("the switch answers its send: {:?}", switched.result.outcome);
    };
    assert_eq!(task.as_str(), "run seeded follow-on");
    let second_output = second.output().await.expect("the second input settles");
    assert_eq!(
        second_output.assistant_message(),
        Some("pending next complete")
    );
    // The follow-on is mail the switch's commit sent: it takes the session's
    // next ingress position, so the input queued before the switch committed
    // runs first (ADR 0101 §3), and the follow-on runs once, after it.
    let follow_on = session
        .attach_id(lash_core::runtime::durable::session_mail::frame_task_run(
            frame_key,
        ))
        .output()
        .await
        .expect("the follow-on answers");
    assert_eq!(
        follow_on.assistant_message(),
        Some("seeded follow-on complete")
    );
    assert_eq!(
        *completions.lock_recover(),
        ["pending-next", "follow-on"],
        "the earlier queued input runs first and the follow-on runs once"
    );
    let frame_node_id = lash_core::facade_support::frame_node_id(
        &SessionId::from("logical-turn-sim"),
        frame_key.as_str(),
    );
    for output in [&second_output, &follow_on] {
        assert_eq!(
            output.result.state.current_frame_node_id.as_deref(),
            Some(frame_node_id.as_str()),
            "every run after the switch runs on the switched frame"
        );
    }
    assert_eq!(
        canonical_seed_nodes(&follow_on.result.state, frame_node_id.as_str()),
        expected_seed_nodes,
        "the switch's seed nodes open the new frame"
    );
    assert!(
        session
            .pending_turn_inputs()
            .await
            .expect("final inputs")
            .is_empty()
    );
    assert!(session.queued_work().await.expect("final queue").is_empty());
    assert_global_invariants(&engine, "admitted-switch").await;
}

struct BoundedSwitchTools {
    switch_count: usize,
}

impl BoundedSwitchTools {
    #[expect(clippy::expect_used, reason = "this fixture declares valid schemas")]
    fn definition(index: usize) -> ToolDefinition {
        ToolDefinition::raw(
            format!("tool:terminal_tool_{index}"),
            format!("terminal_tool_{index}"),
            "Switch to the next frame in the bounded chain.",
            ToolDefinition::default_input_schema(),
            json!({"type": "object"}),
        )
        .expect("valid declared tool schemas")
        .with_execution(std::time::Duration::from_secs(120))
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
async fn admissions_settle_for_finish_cancel_and_error() {
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
        .durable()
        .await
        .expect("open finish session");
    finish_session
        .send(TurnInput::text("finish admitted input"))
        .output()
        .await
        .expect("finish input runs");
    assert_global_invariants(&finish_engine, "admissions-settle-finish").await;
    drop(finish_engine);

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
        .durable()
        .await
        .expect("open cancel session");
    let cancelled = cancel_session
        .send(TurnInput::text("cancel admitted input"))
        .await
        .expect("send cancel input");
    provider_started_rx.await.expect("cancel provider started");
    cancelled
        .cancel()
        .await
        .expect("cancel the running input's run");
    let cancelled = cancelled.output().await.expect("cancel input runs");
    assert_global_invariants(&cancel_engine, "admissions-settle-cancel").await;
    drop(cancel_engine);
    assert!(matches!(
        cancelled.result.outcome,
        lash_core::facade_support::TurnOutcome::Stopped(TurnStop::Cancelled { .. })
    ));
}

/// The host's configured bound governs the durable chain, rather than a
/// constant or an in-memory count tied to one turn.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_frame_switch_chain_stops_at_the_configured_bound() {
    assert_switch_chain(3, 3, false).await;
}

/// A chain below the configured bound completes without a bound failure.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_frame_switch_chain_under_the_bound_completes() {
    assert_switch_chain(3, 2, true).await;
}

#[expect(
    clippy::expect_used,
    clippy::unwrap_used,
    reason = "test support: a refused fixture or unreadable terminal fails the chain law"
)]
async fn assert_switch_chain(limit: usize, switches: usize, finishes: bool) {
    let call_index = Arc::new(AtomicUsize::new(0));
    let provider = lash_core::testing::TestProvider::builder()
        .kind("logical-turn-bound")
        .complete({
            let call_index = Arc::clone(&call_index);
            move |_| {
                let call_index = Arc::clone(&call_index);
                async move {
                    let index = call_index.fetch_add(1, Ordering::SeqCst);
                    // The mutant runs one more switch at the bound, so its
                    // failure is immediate rather than an unknown-tool loop.
                    if (finishes && index >= switches) || index > limit {
                        return Ok(text_response("the chain completed"));
                    }
                    Ok(LlmResponse {
                        parts: vec![LlmOutputPart::ToolCall {
                            call_id: format!("switch-{index}"),
                            tool_name: format!("terminal_tool_{index}"),
                            input_json: "{}".to_string(),
                            replay: None,
                        }],
                        ..LlmResponse::default()
                    })
                }
            }
        })
        .build()
        .into_handle();
    let engine = sim_engine().await;
    let core = lash::LashCore::standard_builder(engine.backend())
        .serve_test_llm_profile(provider, model())
        .tools(Arc::new(BoundedSwitchTools {
            switch_count: limit + 1,
        }))
        .commit_budget(lash::CommitBudget::bounded(1024 * 1024, 512))
        .data_retention(lash::DataRetention::standard())
        .queued_work_batching(lash::QueuedWorkBatchingConfig::new(1))
        .tool_source_policy(lash_core::ToolSourcePolicy::Tolerate)
        .delta_coalescing(lash::DeltaCoalescing::recommended())
        .execution_budgets(
            lash::ExecutionBudgets::new(lash_core::ExecutionBudgetsConfig {
                agent_frame_switch_limit: std::num::NonZeroU32::new(limit as u32).unwrap(),
                ..lash_core::ExecutionBudgetsConfig::recommended()
            })
            .expect("valid execution budgets"),
        )
        .build(lash::persistence::LeaseOwnerIdentity::opaque(
            lash::persistence::LeaseOwnerId::new("logical-turn-chain"),
            lash::persistence::LeaseIncarnationId::new("chain-boot"),
        ))
        .expect("build chain core");
    let session = created_session(&core, "logical-turn-bound")
        .await
        .durable()
        .await
        .expect("open bound session");
    let mut output = session
        .send(TurnInput::text("run the frame switch chain"))
        .output()
        .await
        .expect("the first switch answers its send");
    for _ in 0..switches {
        let lash_core::facade_support::TurnOutcome::AgentFrameSwitch { frame_key, .. } =
            &output.result.outcome
        else {
            panic!(
                "the chain must switch until its final follow-on: {:?}",
                output.result.outcome
            );
        };
        // A switch's answer proves that its follow-on mail committed. Attach
        // only after that answer, so an unaccepted future id cannot end the law.
        output = tokio::time::timeout(
            std::time::Duration::from_secs(60),
            session
                .attach_id(lash_core::runtime::durable::session_mail::frame_task_run(
                    frame_key,
                ))
                .output(),
        )
        .await
        .expect("the chain reaches its final follow-on")
        .expect("the follow-on terminalizes");
    }
    if finishes {
        assert_eq!(output.assistant_message(), Some("the chain completed"));
        assert_eq!(call_index.load(Ordering::SeqCst), switches + 1);
    } else {
        assert!(
            matches!(
                output.result.outcome,
                lash_core::facade_support::TurnOutcome::Stopped(TurnStop::AgentFrameSwitchLimit)
            ),
            "the follow-on at the bound stops typed: {:?}",
            output.result.outcome
        );
        assert_eq!(call_index.load(Ordering::SeqCst), limit);
        let bounded_run = lash_core::TurnId::parse(
            output
                .result
                .acceptance
                .as_ref()
                .unwrap()
                .source_key
                .as_ref()
                .unwrap(),
        )
        .unwrap();
        // The terminal is durable: attaching again requires no live error
        // event to distinguish the chain bound from another runtime failure.
        let resumed = session
            .attach_id(bounded_run)
            .output()
            .await
            .expect("reattach bounded run");
        assert!(matches!(
            resumed.result.outcome,
            lash_core::facade_support::TurnOutcome::Stopped(TurnStop::AgentFrameSwitchLimit)
        ));
        // The bound belongs to this chain, not to the session's frame history.
        let fresh = session
            .send(TurnInput::text("start a fresh chain"))
            .output()
            .await
            .expect("fresh host input runs");
        let lash_core::facade_support::TurnOutcome::AgentFrameSwitch { frame_key, .. } =
            &fresh.result.outcome
        else {
            panic!("fresh input must switch once");
        };
        let fresh = session
            .attach_id(lash_core::runtime::durable::session_mail::frame_task_run(
                frame_key,
            ))
            .output()
            .await
            .expect("fresh follow-on completes");
        assert_eq!(fresh.assistant_message(), Some("the chain completed"));
    }
    assert!(
        session
            .queued_work()
            .await
            .expect("settled queue")
            .is_empty()
    );
    assert!(
        session
            .pending_turn_inputs()
            .await
            .expect("settled inputs")
            .is_empty()
    );
    assert_global_invariants(&engine, "frame-switch-chain").await;
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
        lash_protocol_rlm::CellDialect::typescript(),
    );
    let core = lash::LashCore::rlm_builder(backend, factory)
        .commit_budget(lash::CommitBudget::bounded(1024 * 1024, 512))
        .data_retention(lash::DataRetention::standard())
        .queued_work_batching(lash::QueuedWorkBatchingConfig::new(1024))
        .tool_source_policy(lash_core::ToolSourcePolicy::Tolerate)
        .execution_budgets(lash::ExecutionBudgets::recommended())
        .delta_coalescing(lash::DeltaCoalescing::recommended())
        .serve_test_llm_profile(provider, model())
        .trace_sink(trace.clone())
        .build(lash::persistence::LeaseOwnerIdentity::opaque(
            lash::persistence::LeaseOwnerId::new("logical-turn-test"),
            lash::persistence::LeaseIncarnationId::new("logical-turn-test-boot"),
        ))
        .expect("build RLM seed sim core");
    let session = created_session(&core, "logical-turn-rlm-seed")
        .await
        .durable()
        .await
        .expect("open RLM seed session");
    let switched = session
        .send(TurnInput::text("switch with an RLM seed"))
        .output()
        .await
        .expect("RLM seed turn runs");
    // The send answers with the switch; the task runs as the follow-on under
    // its own run (ADR 0101 §3).
    let lash_core::facade_support::TurnOutcome::AgentFrameSwitch { frame_key, .. } =
        &switched.result.outcome
    else {
        panic!("the switch answers its send: {:?}", switched.result.outcome);
    };
    let terminal = session
        .attach_id(lash_core::runtime::durable::session_mail::frame_task_run(
            frame_key,
        ))
        .output()
        .await
        .expect("the follow-on answers");
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
            } => lash_protocol_rlm::decode_rlm_protocol_event(event)
                .expect("recorded RLM protocol event decodes"),
            _ => None,
        })
        .filter_map(|event| match event {
            lash_rlm_types::RlmProtocolEvent::RlmSeed(seed) => serde_json::to_value(seed).ok(),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(
        observed_nodes,
        vec![json!({"globals": {"baton": "rlm-sim-seed"}})],
        "the RLM frame seed is unchanged"
    );
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
        lash_protocol_rlm::CellDialect::typescript(),
    );
    let core = lash::LashCore::rlm_builder(backend, factory)
        .commit_budget(lash::CommitBudget::bounded(1024 * 1024, 512))
        .data_retention(lash::DataRetention::standard())
        .queued_work_batching(lash::QueuedWorkBatchingConfig::new(1024))
        .tool_source_policy(lash_core::ToolSourcePolicy::Tolerate)
        .execution_budgets(lash::ExecutionBudgets::recommended())
        .delta_coalescing(lash::DeltaCoalescing::recommended())
        .serve_test_llm_profile(provider, model())
        .build(lash::persistence::LeaseOwnerIdentity::opaque(
            lash::persistence::LeaseOwnerId::new("logical-turn-test"),
            lash::persistence::LeaseIncarnationId::new("logical-turn-test-boot"),
        ))
        .expect("build RLM shadowed-control sim core");
    let session = created_session(&core, "logical-turn-rlm-shadowed-control")
        .await
        .durable()
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
/// This test crate's one path to a session that may not exist yet
/// (FIG-4112): only `create` creates, so this creates `session_id` to run
/// [`model`], unbounded, unless the catalog already holds it, then hands back the
/// builder for the verb under test. An existing or deleted id is left for
/// that verb to report.
async fn created_session(
    core: &lash::LashCore,
    session_id: impl Into<lash::SessionId>,
) -> lash::SessionBuilder {
    let session_id = session_id.into();
    match core
        .session(session_id.clone())
        .create(lash::SessionCreation::root(
            lash::plugins::SessionToolAccess::ambient(),
            lash::SessionSpec::new(
                model().wire_model,
                lash::TurnBudget::Unbounded,
                lash::MaxToolCalls::new(1024),
            )
            .no_progress_budget(lash_core::NoProgressBudget::bounded(12)),
        ))
        .await
    {
        Ok(_)
        | Err(lash::EmbedError::SessionAlreadyExists { .. })
        | Err(lash::EmbedError::Store(lash::persistence::StoreError::SessionDeleted { .. })) => {}
        Err(error) => panic!("create session `{session_id}`: {error:?}"),
    }
    core.session(session_id)
}
