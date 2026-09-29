//! A new frame starts without a reload (ADR 0112 §14.6): explicit compaction
//! (6a) and `continue_as` (6c) each commit a frame switch that leaves only the
//! new frame resident, adopted from the commit itself rather than read back
//! from the store.

use super::*;
use lash_core::testing::TestTurnDrive as _;
use std::collections::HashSet;

const SEED: u64 = 0x5_f4a0;
const SESSION: &str = "root";

fn text_call(text: &str) -> MockCall {
    MockCall {
        stream_events: Vec::new(),
        response: Ok(LlmResponse {
            parts: vec![LlmOutputPart::Text {
                text: text.to_string(),
                response_meta: None,
            }],
            response_metadata: Default::default(),
            ..LlmResponse::default()
        }),
    }
}

fn tool_call(call_id: &str, tool_name: &str) -> MockCall {
    MockCall {
        stream_events: Vec::new(),
        response: Ok(LlmResponse {
            parts: vec![LlmOutputPart::ToolCall {
                call_id: call_id.to_string(),
                tool_name: tool_name.to_string(),
                input_json: "{}".to_string(),
                replay: None,
            }],
            response_metadata: Default::default(),
            ..LlmResponse::default()
        }),
    }
}

async fn drive_text_turn(
    runtime: &mut LashRuntime,
    double: &lash_restate_test::RestateTestBackend,
    turn_id: &str,
    text: &str,
) {
    let handler = double
        .open_handler(AdmittedScope::turn(
            SessionId::from(SESSION),
            TurnId::from(turn_id),
        ))
        .await
        .expect("open the turn's handler");
    runtime
        .drive_turn_frames(
            TurnInput::text(text),
            TurnOptions::new(CancellationToken::new(), handler.scoped()),
        )
        .await
        .expect("the turn runs");
    handler.close().await.expect("close the turn's handler");
}

/// The §14.6 facts after a frame-switching commit: the head, the window base
/// and the resident state all name the new frame, the resident graph is that
/// frame alone, and nothing read the window back since `window_loads_before`.
async fn assert_new_frame_resident_without_reload(
    runtime: &LashRuntime,
    store: &Arc<lash_core::testing::runtime_helpers::RecordingStore>,
    old_frame: &lash_core::FrameNodeId,
    window_loads_before: usize,
) {
    assert_eq!(
        store.load_session_count(),
        window_loads_before,
        "the frame switch adopts its own commit; nothing reloads the window"
    );
    let state = runtime.state();
    let new_frame = state
        .current_frame_node_id
        .clone()
        .expect("the switched state names its frame");
    assert_ne!(&new_frame, old_frame, "the commit starts a new frame");

    let window = durable_window(store.clone(), SESSION).await;
    assert_eq!(window.current_frame_node_id.as_ref(), Some(&new_frame));
    let anchor = window
        .window
        .anchor()
        .expect("a durable window is anchored");
    assert_eq!(
        anchor.frame_node_id, new_frame,
        "the window base is the new FrameOpen"
    );
    assert_eq!(anchor.previous_frame_node_id.as_ref(), Some(old_frame));
    assert_eq!(
        window
            .window
            .nodes
            .first()
            .map(|node| node.node_id.as_str()),
        Some(new_frame.as_str()),
        "the window starts at the new frame's FrameOpen"
    );

    let resident = state
        .session_graph
        .nodes
        .iter()
        .map(|node| node.node_id.clone())
        .collect::<HashSet<_>>();
    let durable = window
        .window
        .nodes
        .iter()
        .map(|node| node.node_id.clone())
        .collect::<HashSet<_>>();
    assert_eq!(resident, durable, "the resident nodes are the new frame's");
    assert!(
        state
            .persisted_node_ids
            .iter()
            .all(|node_id| resident.contains(node_id)),
        "every persisted id is resident"
    );
    assert_eq!(state.agent_frames.len(), 1, "one frame record is resident");
    assert_eq!(state.agent_frames[0].frame_node_id, new_frame);
    assert_eq!(
        state.agent_frames[0].previous_frame_node_id.as_ref(),
        Some(old_frame)
    );
}

struct SummaryCompactor;

#[async_trait::async_trait]
impl lash_core::facade_support::ContextCompactor for SummaryCompactor {
    fn id(&self) -> &'static str {
        "test.frame_residency_compactor"
    }

    async fn compact(
        &self,
        _ctx: &lash_core::facade_support::CompactionContext<'_>,
    ) -> Result<
        Option<lash_core::facade_support::ContextCompaction>,
        lash_core::facade_support::ContextError,
    > {
        Ok(Some(lash_core::facade_support::ContextCompaction::new(
            vec![lash_core::SessionAppendNode::message(
                lash_core::PluginMessage::text(
                    lash_core::MessageRole::Assistant,
                    "Compaction summary: the earlier frame",
                ),
            )],
        )))
    }
}

/// 14.6a: explicit compaction starts a new frame without a reload.
#[tokio::test(flavor = "multi_thread")]
pub(super) async fn explicit_compaction_starts_a_frame_without_a_reload() {
    let double = kernel_double(SEED, lash_restate_test::ServerConfig::default()).await;
    let backend = double.lash_backend();
    let store = double_unbound_recording_store(&double).await;
    let mut runtime = runtime_with_plugins_and_tools_and_host_and_store(
        vec![Arc::new(StaticPluginFactory::new(
            "frame-residency-compactor",
            lash_core::facade_support::PluginSpec::new()
                .with_context_compactor(100, Arc::new(SummaryCompactor)),
        ))],
        Arc::new(EmptyTools),
        mock_provider(vec![text_call("first answer"), text_call("second answer")]),
        test_host_config(&backend),
        store.clone() as Arc<dyn lash_core::RuntimeStore>,
    )
    .await;
    drive_text_turn(
        &mut runtime,
        &double,
        "frame-residency-first",
        "first request",
    )
    .await;
    drive_text_turn(
        &mut runtime,
        &double,
        "frame-residency-second",
        "second request",
    )
    .await;
    let old_frame = runtime
        .state()
        .current_frame_node_id
        .clone()
        .expect("the first frame");
    let probe = store.load_session_count();
    durable_window(store.clone(), SESSION).await;
    assert_eq!(
        store.load_session_count(),
        probe + 1,
        "the counter counts window reads on the runtime's store"
    );
    let window_loads_before = store.load_session_count();

    let handler = double
        .open_handler(AdmittedScope::runtime_operation(
            "frame-residency-compaction",
        ))
        .await
        .expect("open the compaction's handler");
    let compacted = Box::pin(runtime.compact_context(None, handler.scoped()))
        .await
        .expect("compaction runs");
    handler
        .close()
        .await
        .expect("close the compaction's handler");
    assert!(compacted, "the compactor answered a summary");

    assert_new_frame_resident_without_reload(&runtime, &store, &old_frame, window_loads_before)
        .await;
}

struct ContinueAsTool;

#[async_trait::async_trait]
impl lash_core::ToolProvider for ContinueAsTool {
    fn tool_manifests(&self) -> Vec<lash_core::ToolManifest> {
        vec![rotating_tool_definition("continue_elsewhere").manifest()]
    }

    fn resolve_contract(&self, name: &str) -> Option<Arc<lash_core::ToolContract>> {
        (name == "continue_elsewhere").then(|| Arc::new(rotating_tool_definition(name).contract()))
    }

    async fn execute(&self, _call: lash_core::ToolCall<'_>) -> lash_core::ToolAttemptOutcome {
        lash_core::ToolOutcome::ok(json!({ "continued": true }))
            .with_control(lash_core::ToolControl::SwitchAgentFrame {
                frame_key: lash_core::FrameKey::from_caller_material("frame-residency-next")
                    .expect("non-empty caller material"),
                initial_nodes: Vec::new(),
                task: Some("finish in the new frame".to_string()),
            })
            .into()
    }
}

/// 14.6c: `continue_as` starts a new frame without a reload.
#[tokio::test(flavor = "multi_thread")]
pub(super) async fn continue_as_starts_a_frame_without_a_reload() {
    let double = kernel_double(SEED + 1, lash_restate_test::ServerConfig::default()).await;
    let backend = double.lash_backend();
    let store = double_unbound_recording_store(&double).await;
    let mut factories = lash_core::testing::test_standard_protocol_factories();
    factories.push(Arc::new(StaticPluginFactory::new(
        "frame-residency-continue-as",
        lash_core::facade_support::PluginSpec::new().with_tool_provider(Arc::new(ContinueAsTool)),
    )));
    let mut runtime = runtime_with_plugins_and_tools_and_host_and_store(
        factories,
        Arc::new(EmptyTools),
        mock_provider(vec![
            text_call("first answer"),
            tool_call("continue-call", "continue_elsewhere"),
            text_call("finished in the new frame"),
        ]),
        test_host_config(&backend),
        store.clone() as Arc<dyn lash_core::RuntimeStore>,
    )
    .await;
    drive_text_turn(
        &mut runtime,
        &double,
        "frame-residency-first",
        "first request",
    )
    .await;
    let old_frame = runtime
        .state()
        .current_frame_node_id
        .clone()
        .expect("the first frame");
    let probe = store.load_session_count();
    durable_window(store.clone(), SESSION).await;
    assert_eq!(
        store.load_session_count(),
        probe + 1,
        "the counter counts window reads on the runtime's store"
    );
    let window_loads_before = store.load_session_count();

    drive_text_turn(
        &mut runtime,
        &double,
        "frame-residency-continue",
        "continue in a new frame",
    )
    .await;

    assert_new_frame_resident_without_reload(&runtime, &store, &old_frame, window_loads_before)
        .await;
}
