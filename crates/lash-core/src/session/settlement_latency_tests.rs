//! Latency-ordered settlement across the deferred-tool phase.
//!
//! A leaf that parks on a completion key finishes out of band, long after the
//! batch was scheduled, and the order those completions arrive in is the one a
//! caller selecting the first rejection is asking about. Reported settlement
//! order has to be that order and not the order the calls were written in.
//!
//! The batch path gets this right by awaiting each parked completion inside the
//! unordered scheduler rather than after it, so a deferred leaf takes its place
//! in the order at the moment it actually completes. That is a property of how
//! the phases are arranged, which is exactly the kind of thing a later
//! refactor can quietly undo, so these tests hold it down from the outside:
//! two real deferred tools, completions raced against each other, asserted in
//! both launch orders so neither input order nor its reverse can pass.

use std::sync::Arc;
use std::time::Duration;

use crate::session::tool_execution::ToolInvocation;

/// Two deferred tools. Each parks on a completion key and arranges for that key
/// to be resolved after its own delay, so the completions genuinely race.
#[derive(Clone)]
struct LatencyProbeTools {
    controller: Arc<crate::InlineRuntimeEffectController>,
}

fn probe_tool(name: &str) -> crate::ToolDefinition {
    crate::ToolDefinition::raw(
        format!("tool:{name}"),
        name,
        "",
        crate::ToolDefinition::default_input_schema(),
        serde_json::json!({ "type": "string" }),
    )
}

fn probe_tools() -> Vec<crate::ToolDefinition> {
    vec![probe_tool("slow_fail"), probe_tool("fast_fail")]
}

/// How long each tool waits before its completion is delivered. The gap is wide
/// enough that a scheduler which honours completion order cannot produce the
/// launch order by luck.
fn probe_delay(name: &str) -> Duration {
    match name {
        "slow_fail" => Duration::from_millis(400),
        _ => Duration::from_millis(20),
    }
}

#[async_trait::async_trait]
impl crate::ToolProvider for LatencyProbeTools {
    fn tool_manifests(&self) -> Vec<crate::ToolManifest> {
        probe_tools()
            .into_iter()
            .map(|tool| tool.manifest())
            .collect()
    }

    fn resolve_contract(&self, name: &str) -> Option<Arc<crate::ToolContract>> {
        probe_tools()
            .into_iter()
            .find(|tool| tool.name() == name)
            .map(|tool| Arc::new(tool.contract()))
    }

    /// Every probe parks on an out-of-band completion, so the runtime
    /// pre-derives the key each attempt body reads.
    fn attempt_may_defer(&self, tool_id: &crate::ToolId) -> bool {
        probe_tools().iter().any(|tool| tool.id() == tool_id)
    }

    async fn execute(&self, call: crate::ToolCall<'_>) -> crate::ToolResult {
        let key = call
            .context
            .completion_key()
            .expect("probe tools run on a controller that issues completion keys");
        let controller = Arc::clone(&self.controller);
        let name = call.name.to_string();
        let delay = probe_delay(&name);
        crate::task::spawn(async move {
            tokio::time::sleep(delay).await;
            let resolution = crate::Resolution::Err(crate::runtime::ExternalCompletionError::new(
                "probe_failed",
                format!("{name} rejected"),
            ));
            let _ = crate::AwaitEventResolver::resolve_await_event(
                controller.as_ref(),
                &key,
                resolution,
            )
            .await;
        });
        crate::ToolResult::pending(crate::PendingCompletion::new())
    }
}

fn latency_probe_context() -> crate::RuntimeExecutionContext<'static> {
    let controller = Arc::new(
        crate::InlineRuntimeEffectController::default().allow_process_lifetime_completion_keys(),
    );
    let provider: Arc<dyn crate::ToolProvider> = Arc::new(LatencyProbeTools {
        controller: Arc::clone(&controller),
    });
    let spec = crate::PluginSpec::new().with_tool_provider(Arc::clone(&provider));
    let plugins = crate::plugin::PluginHost::new(vec![Arc::new(
        crate::plugin::StaticPluginFactory::new("latency_probe_tools", spec),
    )])
    .build_session("root", None)
    .expect("plugin session");
    let tools = plugins.tools();
    let tool_catalog = plugins
        .resolved_tool_catalog("session")
        .expect("tool catalog");
    let (event_tx, _event_rx) = tokio::sync::mpsc::channel(8);
    let attachment_store: Arc<crate::SessionAttachmentStore> =
        Arc::new(crate::SessionAttachmentStore::in_memory());
    let dispatch = crate::tool_dispatch::ToolDispatchContext {
        plugins,
        tools,
        tool_catalog,
        sessions: Arc::new(crate::testing::MockSessionManager::default()),
        session_lifecycle: Arc::new(crate::testing::MockSessionManager::default()),
        session_graph: Arc::new(crate::testing::MockSessionManager::default()),
        processes: Arc::new(crate::UnavailableProcessService),
        trigger_router: None,
        effect_controller: crate::runtime::RuntimeEffectControllerHandle::shared(controller),
        direct_completions: crate::DirectCompletionClient::unavailable(
            "direct completions are unavailable in this test context",
        ),
        parent_invocation: None,
        execution_env_spec: crate::ProcessExecutionEnvSpec::new(
            crate::PluginOptions::default(),
            crate::SessionPolicy::new(crate::TurnBudget::Unbounded),
        ),
        session_id: "session".to_string(),
        agent_frame_id: String::new(),
        event_tx,
        checkpoint_messages: crate::tool_dispatch::CheckpointMessageBuffer::default(),
        trigger_outcomes: crate::tool_dispatch::ToolTriggerOutcomeBuffer::default(),
        attachment_store: Arc::clone(&attachment_store),
        attachment_source_policy: Arc::new(crate::OpenAttachmentSourcePolicy),
        turn_context: crate::TurnContext::default(),
        clock: Arc::new(crate::SystemClock),
        recorded_intent_outcomes: crate::tool_dispatch::RecordedToolIntentOutcomeBuffer::default(),
        tool_registry: None,
    };
    crate::RuntimeExecutionContext::new(
        "session".to_string(),
        Arc::new(dispatch),
        Arc::new(crate::InMemoryProcessExecutionEnvStore::new()),
        attachment_store,
        Arc::new(crate::ChronologicalProjection::default()),
        None,
        crate::TurnContext::default(),
    )
}

/// The headline claim: a batch whose leaves both park reports the order their
/// completions actually arrived, not the order they were launched in.
///
/// Slow-fail is launched first and fast-fail second, so an input-order await
/// yields `[0, 1]` — which is exactly the answer that made `Promise.all` surface
/// the wrong rejection. The true order is `[1, 0]`.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn deferred_leaves_settle_in_completion_order_not_launch_order() {
    let context = latency_probe_context();
    let replies = context
        .call_tool_batch(vec![
            ToolInvocation::new(
                "slow",
                crate::ToolId::from("tool:slow_fail"),
                serde_json::json!({}),
            ),
            ToolInvocation::new(
                "fast",
                crate::ToolId::from("tool:fast_fail"),
                serde_json::json!({}),
            ),
        ])
        .await;

    assert_eq!(replies.replies.len(), 2, "one reply per call");
    assert_eq!(
        replies.settlement_order,
        vec![1, 0],
        "the fast rejection settled first, so it leads the order"
    );
    for reply in &replies.replies {
        assert_eq!(
            reply.output.status(),
            lash_sansio::ToolCallStatus::Failure,
            "both probes reject"
        );
    }
    let first_settled = &replies.replies[replies.settlement_order[0]];
    assert!(
        first_settled
            .output
            .value_for_projection()
            .to_string()
            .contains("fast_fail"),
        "the leading position carries the fast tool's rejection: {}",
        first_settled.output.value_for_projection()
    );
}

/// The same batch with the delays swapped must produce the mirrored order.
///
/// Without this, a test that only ever sees `[1, 0]` is also passed by an
/// implementation that reverses the launch order, which would be just as wrong.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn completion_order_follows_the_delays_in_both_directions() {
    let context = latency_probe_context();
    let replies = context
        .call_tool_batch(vec![
            ToolInvocation::new(
                "fast",
                crate::ToolId::from("tool:fast_fail"),
                serde_json::json!({}),
            ),
            ToolInvocation::new(
                "slow",
                crate::ToolId::from("tool:slow_fail"),
                serde_json::json!({}),
            ),
        ])
        .await;

    assert_eq!(
        replies.settlement_order,
        vec![0, 1],
        "launching the fast tool first puts it first for the honest reason"
    );
}
