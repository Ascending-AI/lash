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

use crate::SessionId;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use lash_sansio::core_support::ModelToolReturnCoreSupport as _;

use crate::session::ToolInvocation;
use lash_sansio::sync::MutexExt as _;

const SEED: u64 = 0x5_f740;

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

    fn presentation_step(&self) -> crate::plugin::ToolPresentationStep {
        let signal = self.clone();
        Arc::new(move |input: crate::plugin::ToolPresentationInput| {
            if input.context.call_id == signal.call_id {
                signal.raised.send_replace(true);
            }
            let projected = crate::ModelToolReturn::text(
                input.context.call_id,
                input.context.tool_name,
                input.context.output.value_for_projection().to_string(),
            );
            Box::pin(async move { Ok(projected) })
        })
    }
}

/// Two deferred tools. Each parks on a completion key and arranges for that key
/// to be resolved after its own delay, so the completions genuinely race.
#[derive(Clone)]
struct LatencyProbeTools {
    controller: Arc<dyn crate::RuntimeEffectController>,
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

    async fn execute(&self, call: crate::ToolCall<'_>) -> crate::ToolAttemptOutcome {
        if call.name() == SLOW_SYNCHRONOUS_PROBE {
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
                ))
                .into();
            }
            return crate::ToolOutcome::failure(crate::ToolFailure::runtime(
                crate::ToolFailureClass::Internal,
                "probe_failed",
                format!("{SLOW_SYNCHRONOUS_PROBE} rejected"),
            ))
            .into();
        }
        let key = call
            .context
            .completion_key()
            .expect("probe tools run on a controller that issues completion keys");
        let controller = Arc::clone(&self.controller);
        let name = call.name().to_string();
        let delay = probe_delay(&name);
        crate::task::spawn(async move {
            tokio::time::sleep(delay).await;
            let resolution = crate::Resolution::Err(crate::runtime::ExternalCompletionError::new(
                crate::FailureCode::foreign(
                    lash_sansio::Namespace::host("probe").expect("valid namespace"),
                    "probe_failed",
                )
                .expect("a validated host namespace is foreign-mintable"),
                format!("{name} rejected"),
            ));
            let _ = crate::AwaitEventResolver::resolve_await_event(
                controller.as_ref(),
                &key,
                resolution,
            )
            .await;
        });
        crate::ToolAttemptOutcome::Pending(crate::PendingCompletion::new())
    }
}

fn probe_context<'run>(
    backend: &crate::Backend,
    provider: Arc<dyn crate::ToolProvider>,
    scoped: crate::ScopedEffectController<'run>,
) -> crate::RuntimeExecutionContext<'run> {
    probe_context_with(
        backend,
        provider,
        scoped,
        None,
        Arc::new(crate::UnavailableProcessService),
    )
}

fn probe_context_with_presentation_step<'run>(
    backend: &crate::Backend,
    provider: Arc<dyn crate::ToolProvider>,
    scoped: crate::ScopedEffectController<'run>,
    step: Option<crate::plugin::ToolPresentationStep>,
) -> crate::RuntimeExecutionContext<'run> {
    probe_context_with(
        backend,
        provider,
        scoped,
        step,
        Arc::new(crate::UnavailableProcessService),
    )
}

/// The backend host's own controller for the probe turn's out-of-band
/// completion resolutions: an ingress-backed resolver, never the lent
/// controller a borrowed handle carries.
fn probe_controller(backend: &crate::Backend) -> Arc<dyn crate::RuntimeEffectController> {
    backend
        .effect_host()
        .scoped_static(crate::AdmittedScope::turn(
            SessionId::from("session"),
            crate::TurnId::from("test-turn"),
        ))
        .expect("the backend host admits the scope")
        .expect("the backend host lends a static controller")
        .owned_controller()
        .expect("a static controller is shared")
}

fn probe_context_with<'run>(
    backend: &crate::Backend,
    provider: Arc<dyn crate::ToolProvider>,
    scoped: crate::ScopedEffectController<'run>,
    step: Option<crate::plugin::ToolPresentationStep>,
    processes: Arc<dyn crate::ProcessService>,
) -> crate::RuntimeExecutionContext<'run> {
    let spec = crate::PluginSpec::new().with_tool_provider(Arc::clone(&provider));
    let spec = match step {
        Some(step) => spec.with_presentation_step(step),
        None => spec,
    };
    let plugins = crate::support::plugin_host(vec![Arc::new(
        crate::plugin::StaticPluginFactory::new("probe_tools", spec),
    )])
    .build_session("root")
    .expect("plugin session");
    let tools = plugins.tools();
    let tool_catalog = plugins
        .resolved_tool_catalog(&SessionId::from("session"))
        .expect("tool catalog");
    let attachment_store: Arc<crate::SessionAttachmentStore> =
        Arc::new(crate::SessionAttachmentStore::unavailable());
    let dispatch = crate::tool_dispatch::ToolDispatchContext {
        plugins,
        tools,
        tool_catalog,
        sessions: Arc::new(crate::testing::MockSessionManager::default()),
        session_lifecycle: Arc::new(crate::testing::MockSessionManager::default()),
        session_graph: Arc::new(crate::testing::MockSessionManager::default()),
        processes,
        trigger_router: None,
        process_definitions: None,
        process_engines: Default::default(),
        effect_controller: crate::runtime::RuntimeEffectControllerHandle::borrowed(scoped),
        direct_completions: crate::DirectCompletionClient::unavailable(
            "direct completions are unavailable in this test context",
        ),
        parent_invocation: None,
        observation_call_key: None,
        execution_env_spec: crate::ProcessExecutionEnvSpec::new(
            crate::PluginOptions::default(),
            crate::SessionPolicy::new(crate::TurnBudget::Unbounded),
        ),
        session_id: SessionId::from("session"),
        agent_frame_id: crate::FrameNodeId::new("test-frame").unwrap(),
        observer: crate::engine::NullObservationSink::arc(),
        checkpoint_messages: crate::tool_dispatch::CheckpointMessageBuffer::default(),
        trigger_outcomes: crate::tool_dispatch::ToolTriggerOutcomeBuffer::default(),
        attachment_store: Arc::clone(&attachment_store),
        attachment_source_policy: Arc::new(crate::OpenAttachmentSourcePolicy),
        turn_context: crate::TurnContext::default(),
        clock: Arc::new(crate::SystemClock),
        tool_registry: None,
        process_lineage: None,
    };
    let process_env_store: Arc<dyn crate::ProcessExecutionEnvStore> = backend.process_env_store();
    let dispatch = Arc::new(dispatch);
    let host: Arc<dyn crate::EffectHost> = backend.effect_host();
    let wiring = crate::testing::wire_test_tool_children(&dispatch, &process_env_store, &host);
    let mut context = crate::RuntimeExecutionContext::new(
        SessionId::from("session"),
        dispatch,
        process_env_store,
        attachment_store,
        Arc::new(crate::ChronologicalProjection::default()),
        None,
        crate::TurnContext::default(),
    );
    context = context.with_tool_child_host(host);
    if let Some(guard) = wiring {
        context = context.with_live_opener_guard(Arc::new(guard));
    }
    context
}

async fn latency_probe_context<'run>(
    backend: &crate::Backend,
    scoped: crate::ScopedEffectController<'run>,
) -> crate::RuntimeExecutionContext<'run> {
    let resolver = probe_controller(backend);
    let provider: Arc<dyn crate::ToolProvider> = Arc::new(LatencyProbeTools {
        controller: resolver,
        awaited_leaf: None,
    });
    probe_context(backend, provider, scoped)
}

/// A probe context whose synchronous leaf waits for `awaited_leaf` to settle,
/// and whose presentation step raises that signal.
async fn handshake_probe_context<'run>(
    awaited_leaf: LeafSettledSignal,
    backend: &crate::Backend,
    scoped: crate::ScopedEffectController<'run>,
) -> crate::RuntimeExecutionContext<'run> {
    let resolver = probe_controller(backend);
    let step = awaited_leaf.presentation_step();
    let provider: Arc<dyn crate::ToolProvider> = Arc::new(LatencyProbeTools {
        controller: resolver,
        awaited_leaf: Some(awaited_leaf),
    });
    probe_context_with_presentation_step(backend, provider, scoped, Some(step))
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

    async fn execute(&self, _call: crate::ToolCall<'_>) -> crate::ToolAttemptOutcome {
        self.attempts.fetch_add(1, Ordering::SeqCst);
        crate::ToolOutcome::retryable_failure(
            crate::ToolFailureClass::External,
            "retry_probe",
            "retry probe failure",
            Some(0),
        )
        .into()
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
    let double =
        crate::support::kernel_double(SEED, lash_restate_test::ServerConfig::default()).await;
    let backend = double.lash_backend();
    let handler = double
        .open_handler(crate::AdmittedScope::turn(
            SessionId::from("session"),
            crate::TurnId::from("test-turn"),
        ))
        .await
        .expect("open the probe handler");
    let context = probe_context(&backend, provider, handler.scoped());

    let scalar = context
        .call_command_tool(
            &crate::CommandReplayKey::new("scalar"),
            crate::session::ToolInvocation::new(
                "scalar",
                crate::ToolId::from("tool:granted_retry_probe"),
                serde_json::json!({}),
            )
            .with_execution_grant(grant.clone()),
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
    drop(context);
    handler.close().await.expect("close the probe handler");
}

/// The headline claim: a batch whose leaves both park reports the order their
/// completions actually arrived, not the order they were launched in.
///
/// Slow-fail is launched first and fast-fail second, so an input-order await
/// yields `[0, 1]` — which is exactly the answer that made `Promise.all` surface
/// the wrong rejection. The true order is `[1, 0]`.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn deferred_leaves_settle_in_completion_order_not_launch_order() {
    let double =
        crate::support::kernel_double(SEED, lash_restate_test::ServerConfig::default()).await;
    let backend = double.lash_backend();
    let _gate = double.server().outside_gates().enter();
    let handler = double
        .open_handler(crate::AdmittedScope::turn(
            SessionId::from("session"),
            crate::TurnId::from("test-turn"),
        ))
        .await
        .expect("open the probe handler");
    let context = latency_probe_context(&backend, handler.scoped()).await;
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
        "the fast rejection settled first, so it leads the order; replies: {:?}",
        replies
            .replies
            .iter()
            .map(|reply| reply.output.value_for_projection())
            .collect::<Vec<_>>()
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
    drop(context);
    handler.close().await.expect("close the probe handler");
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
    let replies = drain_slot_handshake_batch().await;
    for reply in &replies.replies {
        assert_eq!(
            reply.output.status(),
            lash_sansio::ToolCallStatus::Failure,
            "both probes reject"
        );
    }
}

/// The leaf that settled first leads the settlement order: in the handshake
/// above, the deferred leaf settles before the synchronous one can finish.
///
/// Settlement order is the durable final-commit order (ADR 0099 §5). The
/// deferred leaf's final record must commit when its completion resolves —
/// at the same §4 boundary an inline terminal crosses — and not at the
/// child's finalize: the handshake releases the synchronous leaf from the
/// deferred leaf's presentation step, which runs between the two, so a
/// finalize-time commit races the synchronous leaf's boundary commit and
/// loses it in about two runs of three (FIG-3609).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_later_leaf_that_settles_first_leads_the_settlement_order() {
    let replies = drain_slot_handshake_batch().await;
    assert_eq!(
        replies.settlement_order,
        vec![1, 0],
        "the deferred leaf settled first, so it leads the order"
    );
}

/// The handshake batch: leaf 0 holds the earliest drain slot until leaf 1's
/// reply has been projected, and the batch asserts leaf 0 saw it.
async fn drain_slot_handshake_batch() -> crate::session::ToolBatchReplies {
    let double =
        crate::support::kernel_double(SEED, lash_restate_test::ServerConfig::default()).await;
    let backend = double.lash_backend();
    let _gate = double.server().outside_gates().enter();
    let handler = double
        .open_handler(crate::AdmittedScope::turn(
            SessionId::from("session"),
            crate::TurnId::from("test-turn"),
        ))
        .await
        .expect("open the probe handler");
    let context =
        handshake_probe_context(LeafSettledSignal::new("fast"), &backend, handler.scoped()).await;
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
    drop(context);
    handler.close().await.expect("close the probe handler");
    replies
}

/// The same batch with the delays swapped must produce the mirrored order.
///
/// Without this, a test that only ever sees `[1, 0]` is also passed by an
/// implementation that reverses the launch order, which would be just as wrong.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn completion_order_follows_the_delays_in_both_directions() {
    let double =
        crate::support::kernel_double(SEED, lash_restate_test::ServerConfig::default()).await;
    let backend = double.lash_backend();
    let _gate = double.server().outside_gates().enter();
    let handler = double
        .open_handler(crate::AdmittedScope::turn(
            SessionId::from("session"),
            crate::TurnId::from("test-turn"),
        ))
        .await
        .expect("open the probe handler");
    let context = latency_probe_context(&backend, handler.scoped()).await;
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
    drop(context);
    handler.close().await.expect("close the probe handler");
}

// ---------------------------------------------------------------------------
// One recorded order for a mixed tool/durable-wait batch (ADR 0095, FIG-2996).
//
// `Promise.all([tools.x.op(), processes.await(h)])` is the shape ADR 0087 could
// not settle in one order: a process wait was not a batch child, so it was
// awaited in a second phase after the tool batch and a tool rejection therefore
// always beat a process failure. `processes.await` is a leaf tool now and parks
// on a Durable Wait the process terminal resolves, so both leaves are in one
// batch and the recorded order is the order their completions arrived. These
// cases drive that batch under every permutation of launch order and
// completion order, because a single ordering is also produced by an
// implementation that returns launch order, or its reverse, by luck.
// ---------------------------------------------------------------------------

/// The ordinary tool leaf: parks on its own completion key and is resolved out
/// of band after its delay, as any deferred tool is.
const TOOL_LEAF: &str = "tool_leaf";

/// The durable-wait leaf, shaped as the process-controls plugin ships
/// `processes.await`: it parks and names the process terminal as its resolver,
/// and resolves nothing itself. The terminal is the test.
const PROCESS_AWAIT_LEAF: &str = "await_process_leaf";

/// The process the durable wait is taken against.
fn awaited_process_ref() -> crate::ProcessId {
    crate::ProcessId::fixture("child-process")
}

struct MixedBatchProbeTools {
    controller: Arc<dyn crate::RuntimeEffectController>,
    tool_delay: Duration,
    tool_fails: bool,
}

fn mixed_batch_tools() -> Vec<crate::ToolDefinition> {
    vec![probe_tool(TOOL_LEAF), probe_tool(PROCESS_AWAIT_LEAF)]
}

#[async_trait::async_trait]
impl crate::ToolProvider for MixedBatchProbeTools {
    fn tool_manifests(&self) -> Vec<crate::ToolManifest> {
        mixed_batch_tools()
            .into_iter()
            .map(|tool| tool.manifest())
            .collect()
    }

    fn resolve_contract(&self, name: &str) -> Option<Arc<crate::ToolContract>> {
        mixed_batch_tools()
            .into_iter()
            .find(|tool| tool.name() == name)
            .map(|tool| Arc::new(tool.contract()))
    }

    fn attempt_may_defer(&self, tool_id: &crate::ToolId) -> bool {
        mixed_batch_tools().iter().any(|tool| tool.id() == tool_id)
    }

    async fn execute(&self, call: crate::ToolCall<'_>) -> crate::ToolAttemptOutcome {
        if call.name() == PROCESS_AWAIT_LEAF {
            // No out-of-band actor and no timer: the runtime arms the declared
            // resolver at the park site, and the process terminal is what
            // delivers the outcome.
            return crate::ToolAttemptOutcome::Pending(
                crate::PendingCompletion::new().resolved_by_process_terminal(awaited_process_ref()),
            );
        }
        let key = call
            .context
            .completion_key()
            .expect("probe tools run on a controller that issues completion keys");
        let controller = Arc::clone(&self.controller);
        let delay = self.tool_delay;
        let fails = self.tool_fails;
        crate::task::spawn(async move {
            tokio::time::sleep(delay).await;
            let resolution = if fails {
                crate::Resolution::Err(crate::runtime::ExternalCompletionError::new(
                    crate::FailureCode::foreign(
                        lash_sansio::Namespace::host("probe").expect("valid namespace"),
                        "probe_failed",
                    )
                    .expect("a validated host namespace is foreign-mintable"),
                    format!("{TOOL_LEAF} rejected"),
                ))
            } else {
                crate::Resolution::Ok(serde_json::json!(TOOL_LEAF))
            };
            let _ = crate::AwaitEventResolver::resolve_await_event(
                controller.as_ref(),
                &key,
                resolution,
            )
            .await;
        });
        crate::ToolAttemptOutcome::Pending(crate::PendingCompletion::new())
    }
}

/// How each leaf of the mixed batch behaves: when it completes and whether it
/// rejects.
#[derive(Clone, Copy)]
struct LeafSchedule {
    delay: Duration,
    fails: bool,
}

/// `tool_first` is the order the two leaves are written in, which is the order
/// they are launched in and the order the replies come back in — never, by
/// itself, the settlement order.
async fn mixed_batch(
    tool: LeafSchedule,
    process: LeafSchedule,
    tool_first: bool,
) -> crate::session::ToolBatchReplies {
    let double =
        crate::support::kernel_double(SEED, lash_restate_test::ServerConfig::default()).await;
    let backend = double.lash_backend();
    let _gate = double.server().outside_gates().enter();
    let controller = probe_controller(&backend);
    let handler = double
        .open_handler(crate::AdmittedScope::turn(
            SessionId::from("session"),
            crate::TurnId::from("test-turn"),
        ))
        .await
        .expect("open the probe handler");
    let processes = Arc::new(crate::testing::MockSessionManager::default());
    let provider: Arc<dyn crate::ToolProvider> = Arc::new(MixedBatchProbeTools {
        controller: Arc::clone(&controller),
        tool_delay: tool.delay,
        tool_fails: tool.fails,
    });
    let context = probe_context_with(
        &backend,
        provider,
        handler.scoped(),
        None,
        Arc::clone(&processes) as Arc<dyn crate::ProcessService>,
    );

    // Stand in for the process terminal: wait until the runtime armed it, then
    // resolve the wait it was armed for. Arming happens at the park site, so
    // the key cannot be known before the leaf parks.
    let terminal = crate::task::spawn({
        let processes = Arc::clone(&processes);
        let controller = Arc::clone(&controller);
        async move {
            let key = loop {
                if let Some((process_id, key)) = processes
                    .terminal_attachments
                    .lock_recover()
                    .first()
                    .cloned()
                {
                    assert_eq!(
                        process_id,
                        awaited_process_ref(),
                        "the terminal armed must be the process the wait names"
                    );
                    break key;
                }
                tokio::time::sleep(Duration::from_millis(2)).await;
            };
            tokio::time::sleep(process.delay).await;
            let resolution = if process.fails {
                crate::Resolution::Err(crate::runtime::ExternalCompletionError::new(
                    crate::FailureCode::foreign(
                        lash_sansio::Namespace::host("probe").expect("valid namespace"),
                        "process_failed",
                    )
                    .expect("a validated host namespace is foreign-mintable"),
                    "the awaited process failed",
                ))
            } else {
                crate::Resolution::Ok(serde_json::json!("process ok"))
            };
            let _ = crate::AwaitEventResolver::resolve_await_event(
                controller.as_ref(),
                &key,
                resolution,
            )
            .await;
        }
    });

    let tool_call = ToolInvocation::new(
        "tool-leaf",
        crate::ToolId::from(format!("tool:{TOOL_LEAF}")),
        serde_json::json!({}),
    );
    let process_call = ToolInvocation::new(
        "process-await-leaf",
        crate::ToolId::from(format!("tool:{PROCESS_AWAIT_LEAF}")),
        serde_json::json!({}),
    );
    let calls = if tool_first {
        vec![tool_call, process_call]
    } else {
        vec![process_call, tool_call]
    };
    let replies = context.call_tool_batch(calls).await;
    terminal.abort();
    drop(context);
    handler.close().await.expect("close the probe handler");
    replies
}

/// Position of the leaf that completed first, given the written order.
fn first_settled(replies: &crate::session::ToolBatchReplies) -> usize {
    *replies
        .settlement_order
        .first()
        .expect("a batch that settled records an order")
}

/// The headline law: one batch, one recorded order, and the durable wait takes
/// its place in it by when the process terminal fired — not by where it was
/// written. Every permutation of written order and completion order is driven,
/// so neither input order nor its reverse passes.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_durable_wait_takes_its_place_in_the_one_recorded_order() {
    let quick = Duration::from_millis(20);
    let slow = Duration::from_millis(300);
    for tool_first in [true, false] {
        for tool_is_quick in [true, false] {
            let tool = LeafSchedule {
                delay: if tool_is_quick { quick } else { slow },
                fails: false,
            };
            let process = LeafSchedule {
                delay: if tool_is_quick { slow } else { quick },
                fails: false,
            };
            let replies = mixed_batch(tool, process, tool_first).await;

            let tool_position = usize::from(!tool_first);
            let process_position = usize::from(tool_first);
            let expected_first = if tool_is_quick {
                tool_position
            } else {
                process_position
            };
            assert_eq!(
                first_settled(&replies),
                expected_first,
                "tool_first={tool_first} tool_is_quick={tool_is_quick}: the leaf whose \
                 completion arrived first leads the recorded order"
            );
            assert_eq!(
                replies.settlement_order.len(),
                2,
                "both leaves are in the one order"
            );
            // The cell observes both, in written order, whichever settled
            // first: the recorded order selects a rejection, it does not
            // reorder results.
            for reply in &replies.replies {
                assert_eq!(
                    reply.output.status(),
                    lash_sansio::ToolCallStatus::Success,
                    "tool_first={tool_first} tool_is_quick={tool_is_quick}: both leaves succeed"
                );
            }
        }
    }
}

/// A process that fails before the tool settles is the first failure the batch
/// recorded, and it is recorded as such however the aggregate was written.
///
/// Under ADR 0087 this was unreachable: the tool batch was phase one, so a tool
/// rejection always won and a process failure could only be reported when no
/// tool leaf had rejected.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_process_that_fails_first_is_the_batch_recorded_first_failure() {
    for tool_first in [true, false] {
        let replies = mixed_batch(
            LeafSchedule {
                delay: Duration::from_millis(300),
                fails: true,
            },
            LeafSchedule {
                delay: Duration::from_millis(20),
                fails: true,
            },
            tool_first,
        )
        .await;

        let process_position = usize::from(tool_first);
        assert_eq!(
            first_settled(&replies),
            process_position,
            "tool_first={tool_first}: the process failed first, so its rejection leads"
        );
        let leading = &replies.replies[first_settled(&replies)];
        assert!(
            leading
                .output
                .value_for_projection()
                .to_string()
                .contains("process_failed"),
            "tool_first={tool_first}: the leading position carries the process failure: {}",
            leading.output.value_for_projection()
        );
    }
}
