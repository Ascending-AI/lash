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
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use lash_sansio::core_support::ModelToolReturnCoreSupport as _;

use crate::session::tool_execution::ToolInvocation;

/// A one-shot signal raised when a named leaf's reply has been projected, which
/// happens only once that leaf's child future has run to completion.
#[derive(Clone)]
struct LeafSettledSignal {
    call_id: &'static str,
    raised: Arc<tokio::sync::watch::Sender<bool>>,
}

impl LeafSettledSignal {
    fn new(call_id: &'static str) -> Self {
        Self {
            call_id,
            raised: Arc::new(tokio::sync::watch::channel(false).0),
        }
    }

    /// Waits for the signal, giving up after a budget so a batch that cannot
    /// raise it fails as an assertion rather than hanging the suite.
    async fn wait(&self) -> bool {
        let mut receiver = self.raised.subscribe();
        tokio::time::timeout(Duration::from_secs(5), async move {
            receiver.wait_for(|raised| *raised).await.is_ok()
        })
        .await
        .unwrap_or(false)
    }

    fn projector(&self) -> crate::plugin::ToolResultProjector {
        let signal = self.clone();
        Arc::new(move |context: crate::plugin::ToolResultProjectionContext| {
            if context.call_id == signal.call_id {
                signal.raised.send_replace(true);
            }
            let projected = crate::ModelToolReturn::text(
                context.call_id,
                context.tool_name,
                context.output.value_for_projection().to_string(),
            );
            Box::pin(async move { Ok(projected) })
        })
    }
}

/// Two deferred tools. Each parks on a completion key and arranges for that key
/// to be resolved after its own delay, so the completions genuinely race.
#[derive(Clone)]
struct LatencyProbeTools {
    controller: Arc<crate::InlineRuntimeEffectController>,
    /// When set, the synchronous probe blocks until this signal is raised
    /// instead of sleeping out a fixed delay.
    awaited_leaf: Option<LeafSettledSignal>,
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

/// A leaf that never parks: it blocks inside its own attempt and then returns a
/// terminal failure. It exists to hold a batch's earliest drain slot open until
/// a later leaf has settled.
const SLOW_SYNCHRONOUS_PROBE: &str = "slow_sync_fail";

fn probe_tools() -> Vec<crate::ToolDefinition> {
    vec![
        probe_tool("slow_fail"),
        probe_tool("fast_fail"),
        probe_tool(SLOW_SYNCHRONOUS_PROBE).with_retry_policy(crate::ToolRetryPolicy::Never),
    ]
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

    /// Every probe but the synchronous one parks on an out-of-band completion,
    /// so the runtime pre-derives the key those attempt bodies read.
    fn attempt_may_defer(&self, tool_id: &crate::ToolId) -> bool {
        probe_tools()
            .iter()
            .any(|tool| tool.id() == tool_id && tool.name() != SLOW_SYNCHRONOUS_PROBE)
    }

    async fn execute(&self, call: crate::ToolCall<'_>) -> crate::ToolOutcome {
        if call.name == SLOW_SYNCHRONOUS_PROBE {
            let settled = match &self.awaited_leaf {
                Some(signal) => signal.wait().await,
                // The synchronous probe exists only for the handshake case; a
                // context without a signal has nothing for it to wait on.
                None => false,
            };
            if !settled {
                return crate::ToolOutcome::failure(crate::ToolFailure::runtime(
                    crate::ToolFailureClass::Internal,
                    "awaited_leaf_never_settled",
                    "the later leaf never settled while this leaf held its drain slot",
                ));
            }
            return crate::ToolOutcome::failure(crate::ToolFailure::runtime(
                crate::ToolFailureClass::Internal,
                "probe_failed",
                format!("{SLOW_SYNCHRONOUS_PROBE} rejected"),
            ));
        }
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
        crate::ToolOutcome::pending(crate::PendingCompletion::new())
    }
}

fn probe_context(
    provider: Arc<dyn crate::ToolProvider>,
    controller: Arc<crate::InlineRuntimeEffectController>,
) -> crate::RuntimeExecutionContext<'static> {
    probe_context_with_projector(provider, controller, None)
}

fn probe_context_with_projector(
    provider: Arc<dyn crate::ToolProvider>,
    controller: Arc<crate::InlineRuntimeEffectController>,
    projector: Option<crate::plugin::ToolResultProjector>,
) -> crate::RuntimeExecutionContext<'static> {
    let spec = crate::PluginSpec::new().with_tool_provider(Arc::clone(&provider));
    let spec = match projector {
        Some(projector) => spec.with_tool_result_projector(projector),
        None => spec,
    };
    let plugins = crate::plugin::PluginHost::new(vec![Arc::new(
        crate::plugin::StaticPluginFactory::new("probe_tools", spec),
    )])
    .build_session("root")
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
        agent_frame_id: crate::FrameNodeId::default(),
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

fn latency_probe_context() -> crate::RuntimeExecutionContext<'static> {
    let controller = Arc::new(
        crate::InlineRuntimeEffectController::default().allow_process_lifetime_completion_keys(),
    );
    let provider: Arc<dyn crate::ToolProvider> = Arc::new(LatencyProbeTools {
        controller: Arc::clone(&controller),
        awaited_leaf: None,
    });
    probe_context(provider, controller)
}

/// A probe context whose synchronous leaf waits for `awaited_leaf` to settle,
/// and whose result projector raises that signal.
fn handshake_probe_context(
    awaited_leaf: LeafSettledSignal,
) -> crate::RuntimeExecutionContext<'static> {
    let controller = Arc::new(
        crate::InlineRuntimeEffectController::default().allow_process_lifetime_completion_keys(),
    );
    let projector = awaited_leaf.projector();
    let provider: Arc<dyn crate::ToolProvider> = Arc::new(LatencyProbeTools {
        controller: Arc::clone(&controller),
        awaited_leaf: Some(awaited_leaf),
    });
    probe_context_with_projector(provider, controller, Some(projector))
}

struct GrantedRetryProbeTools {
    catalog_definition: crate::ToolDefinition,
    attempts: Arc<AtomicUsize>,
}

#[async_trait::async_trait]
impl crate::ToolProvider for GrantedRetryProbeTools {
    fn tool_manifests(&self) -> Vec<crate::ToolManifest> {
        vec![self.catalog_definition.manifest()]
    }

    fn resolve_contract(&self, name: &str) -> Option<Arc<crate::ToolContract>> {
        (name == self.catalog_definition.name())
            .then(|| Arc::new(self.catalog_definition.contract()))
    }

    async fn execute(&self, _call: crate::ToolCall<'_>) -> crate::ToolOutcome {
        self.attempts.fetch_add(1, Ordering::SeqCst);
        crate::ToolOutcome::retryable_failure(
            crate::ToolFailureClass::External,
            "retry_probe",
            "retry probe failure",
            Some(0),
        )
    }
}

#[tokio::test]
async fn granted_in_catalog_call_uses_same_manifest_retry_policy_scalar_and_batch() {
    let attempts = Arc::new(AtomicUsize::new(0));
    let catalog_definition =
        probe_tool("granted_retry_probe").with_retry_policy(crate::ToolRetryPolicy::Never);
    let granted_definition =
        probe_tool("granted_retry_probe").with_retry_policy(crate::ToolRetryPolicy::safe(2, 0, 0));
    let grant = crate::ToolExecutionGrant::from_definition(granted_definition)
        .with_source_id(crate::PLUGIN_TOOL_SOURCE_ID);
    let provider: Arc<dyn crate::ToolProvider> = Arc::new(GrantedRetryProbeTools {
        catalog_definition,
        attempts: Arc::clone(&attempts),
    });
    let controller = Arc::new(crate::InlineRuntimeEffectController::default());
    let context = probe_context(provider, controller);

    let scalar = context
        .call_tool_with_execution_grant(
            "scalar".to_string(),
            grant.clone(),
            serde_json::json!({}),
            0,
        )
        .await;
    let scalar_attempts = attempts.load(Ordering::SeqCst);

    let batch = context
        .call_tool_batch(vec![
            ToolInvocation::new(
                "batch",
                crate::ToolId::from("tool:granted_retry_probe"),
                serde_json::json!({}),
            )
            .with_execution_grant(grant),
        ])
        .await;
    let batch_attempts = attempts.load(Ordering::SeqCst) - scalar_attempts;

    assert_eq!(
        scalar_attempts, 2,
        "the grant's safe policy wins for scalar"
    );
    assert_eq!(batch_attempts, scalar_attempts, "batch matches scalar");
    assert!(!scalar.output.is_success());
    assert!(!batch.replies[0].output.is_success());
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

/// A later leaf must be able to settle while an earlier leaf still holds its
/// intent-drain slot.
///
/// Both probes above park immediately, so neither ever waited on the batch's
/// intent-drain gate for its turn, which left this case uncovered. Here leaf 0
/// runs a synchronous attempt and so holds the earliest drain slot until that
/// attempt returns, while leaf 1 parks at once and its completion lands in
/// 20 ms.
///
/// The two leaves are wired into a handshake rather than a race, because a race
/// between them proves nothing: leaf 0 refuses to finish until leaf 1's reply
/// has been projected, which happens only after leaf 1's child future has run
/// to completion. So the batch can only make progress at all if leaf 1 settles
/// first.
///
/// Every non-draining exit — leaf 1's parked launch among them — used to
/// discharge by calling `finish`, which waited for the slot's turn *before*
/// releasing it. Leaf 1 could therefore not return its parked launch, let alone
/// await its completion, until leaf 0 had drained; against that code this test
/// deadlocks both leaves and leaf 0 reports `awaited_leaf_never_settled`.
/// Discharge is now the guard's drop, which takes no turn, so leaf 1 settles
/// when it actually settles and leads the order.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_later_leaf_settles_while_an_earlier_leaf_holds_its_drain_slot() {
    let context = handshake_probe_context(LeafSettledSignal::new("fast"));
    let replies = context
        .call_tool_batch(vec![
            ToolInvocation::new(
                "slow-sync",
                crate::ToolId::from(format!("tool:{SLOW_SYNCHRONOUS_PROBE}")),
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
    let synchronous_leaf = replies.replies[0].output.value_for_projection().to_string();
    assert!(
        !synchronous_leaf.contains("awaited_leaf_never_settled"),
        "the deferred leaf must settle while the synchronous leaf holds slot 0: {synchronous_leaf}"
    );
    assert_eq!(
        replies.settlement_order,
        vec![1, 0],
        "the deferred leaf settled first, so it leads the order"
    );
    for reply in &replies.replies {
        assert_eq!(
            reply.output.status(),
            lash_sansio::ToolCallStatus::Failure,
            "both probes reject"
        );
    }
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
