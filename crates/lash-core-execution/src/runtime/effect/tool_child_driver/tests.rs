//! The rebind checklist, one negative test per line.
//!
//! Every case here makes the **lent** opener context deliberately wrong and
//! asserts the child followed its recorded request instead. A child running
//! under its opener's authority instead of its own recorded authority is the
//! failure [`rebind_child_dispatch`] exists to make impossible, and a test that
//! only asserted the happy path would pass just as well against a rebind that
//! forgot a field.

use std::sync::Arc;

use super::*;
use crate::runtime::{ToolChildAdmission, ToolChildCompletionRouting, ToolChildScope};
use crate::tool_dispatch::ToolAttemptEffectIdentity;
use crate::{
    ExecutionScope, FrameNodeId, PreparedToolCall, ProcessExecutionEnvRef, ProcessExecutionEnvSpec,
    SessionId, ToolId, ToolManifest, ToolRetryPolicy,
};

struct NoopTools;

#[async_trait::async_trait]
impl crate::ToolProvider for NoopTools {
    fn tool_manifests(&self) -> Vec<ToolManifest> {
        Vec::new()
    }

    fn resolve_contract(&self, _name: &str) -> Option<Arc<crate::ToolContract>> {
        None
    }

    async fn execute(&self, _call: crate::ToolCall<'_>) -> crate::ToolAttemptOutcome {
        crate::ToolOutcome::err_fmt("the rebind tests never run a tool").into()
    }
}

fn spec(turns: usize) -> ProcessExecutionEnvSpec {
    ProcessExecutionEnvSpec::new(
        crate::PluginOptions::default(),
        crate::SessionPolicy::new(crate::TurnBudget::bounded(turns)),
    )
}

fn manifest(id: &str) -> ToolManifest {
    let mut manifest = crate::ToolDefinition::raw(
        id,
        id,
        "a rebind fixture tool",
        crate::ToolDefinition::default_input_schema(),
        serde_json::json!({ "type": "object", "additionalProperties": true }),
    )
    .manifest;
    manifest.retry_policy = ToolRetryPolicy::safe(4, 10, 100);
    manifest
}

fn invocation(effect_id: &str) -> crate::RuntimeInvocation {
    crate::RuntimeInvocation::effect(
        crate::EffectAddress::new(ExecutionScope::turn("child-session", "turn"), effect_id)
            .expect("a valid effect address"),
        crate::RuntimeAttribution::for_session("child-session"),
        effect_id,
    )
}

/// The request the child was admitted under. Every field here deliberately
/// disagrees with [`lent`]'s, so a rebind that dropped a line shows up as the
/// opener's value surviving.
fn request() -> ToolChildRequest {
    ToolChildRequest::new(
        PreparedToolCall::from_parts(
            "call-1",
            ToolId::from("search"),
            "search",
            serde_json::json!({ "q": "lash" }),
            None,
            serde_json::Value::Null,
        ),
        ToolChildAdmission::Catalog {
            manifest: Box::new(manifest("search")),
        },
        ToolAttemptEffectIdentity::Scalar {
            parent: Some(invocation("recorded-parent")),
        },
        ToolChildScope {
            opener: crate::EffectOpener::turn("child-session", "turn"),
            admitted_scope: ExecutionScope::turn("child-session", "turn"),
            session_id: SessionId::from("child-session"),
            agent_frame_id: FrameNodeId::new("child-frame").expect("a valid frame id"),
        },
        ProcessExecutionEnvRef::new("env-ref"),
        ToolChildCompletionRouting::Inline,
    )
}

/// The opener's own context, every recorded field set to something the child
/// must not inherit.
fn lent() -> ToolDispatchContext<'static> {
    let (event_tx, event_rx) = tokio::sync::mpsc::channel(1);
    // Held for the test's lifetime, so the sender never reports a closed
    // channel for a reason unrelated to what is asserted.
    std::mem::forget(event_rx);
    let mut other_tool = manifest("opener-tool");
    other_tool.retry_policy = ToolRetryPolicy::Never;
    ToolDispatchContext {
        plugins: crate::plugin::PluginHost::empty()
            .build_session("opener-session")
            .expect("plugin session"),
        tools: Arc::new(NoopTools),
        tool_registry: None,
        tool_catalog: Arc::new(crate::ToolCatalog::from_tool_definitions(vec![
            crate::ToolDefinition {
                manifest: other_tool,
                contract: crate::ToolContract::default(),
            },
        ])),
        sessions: Arc::new(crate::testing::MockSessionManager::default()),
        session_lifecycle: Arc::new(crate::testing::MockSessionManager::default()),
        session_graph: Arc::new(crate::testing::MockSessionManager::default()),
        processes: Arc::new(crate::UnavailableProcessService),
        trigger_router: None,
        process_definitions: None,
        process_engines: crate::ProcessEngineRegistry::default(),
        effect_controller: crate::runtime::RuntimeEffectControllerHandle::shared(Arc::new(
            crate::NativeRuntimeEffectController::default(),
        )),
        direct_completions: crate::DirectCompletionClient::unavailable(
            "direct completions are unavailable in this test context",
        ),
        parent_invocation: Some(invocation("opener-parent")),
        execution_env_spec: spec(9),
        session_id: SessionId::from("opener-session"),
        agent_frame_id: FrameNodeId::new("opener-frame").expect("a valid frame id"),
        event_tx,
        checkpoint_messages: crate::tool_dispatch::CheckpointMessageBuffer::default(),
        trigger_outcomes: crate::tool_dispatch::ToolTriggerOutcomeBuffer::default(),
        attachment_store: Arc::new(crate::SessionAttachmentStore::in_memory()),
        attachment_source_policy: Arc::new(crate::OpenAttachmentSourcePolicy),
        turn_context: crate::TurnContext::default(),
        clock: Arc::new(crate::SystemClock),
    }
}

/// The child's own admitted controller, bound to the child's claim scope and
/// not the opener's.
fn child_controller() -> ScopedEffectController<'static> {
    ScopedEffectController::shared(
        Arc::new(crate::NativeRuntimeEffectController::default()),
        ExecutionScope::turn("child-session", "turn"),
    )
    .expect("a valid child scope")
}

fn rebound(request: &ToolChildRequest) -> ToolDispatchContext<'static> {
    rebind_child_dispatch(
        &lent(),
        request,
        child_controller(),
        spec(3),
        &ToolUsageLedger::new(),
    )
}

/// A child may be attributed to a session the lending opener is not: a process
/// opener has no session of its own (ADR 0094) and still does tool work that
/// belongs to one.
#[test]
fn the_child_is_attributed_to_its_recorded_session_not_the_opener_s() {
    assert_eq!(
        lent().session_id,
        SessionId::from("opener-session"),
        "the lent value must be wrong for this test to prove anything"
    );
    assert_eq!(
        rebound(&request()).session_id,
        SessionId::from("child-session")
    );
}

/// One session holds many frames (ADR 0092). A child that inherited the
/// opener's current frame would attribute its work to the wrong one.
#[test]
fn the_child_keeps_its_recorded_agent_frame() {
    assert_eq!(lent().agent_frame_id.as_str(), "opener-frame");
    assert_eq!(rebound(&request()).agent_frame_id.as_str(), "child-frame");
}

/// Ruling 1 (ADR 0099 §3 amendment 1): a reopen may not consult the live Tool
/// Catalog. The lent catalog holds a *different* tool with a *different* retry
/// policy, so a rebind that kept it would run the child under a policy it was
/// never admitted with — or fail to resolve it at all.
#[test]
fn the_child_is_dispatched_against_its_admitted_manifest_not_the_live_catalog() {
    let lent = lent();
    assert!(
        crate::tool_dispatch::resolve_callable_manifest_by_id(&lent, &ToolId::from("search"))
            .is_none(),
        "the lent catalog must not contain the child's tool for this test to prove anything"
    );
    let child = rebound(&request());
    let resolved =
        crate::tool_dispatch::resolve_callable_manifest_by_id(&child, &ToolId::from("search"))
            .expect("the admitted manifest is the child's whole catalog");
    assert_eq!(resolved.retry_policy, ToolRetryPolicy::safe(4, 10, 100));
    assert_eq!(
        child.tool_catalog.tools.len(),
        1,
        "the child sees exactly its admitted manifest, never the opener's catalog"
    );
}

/// Lineage is recorded, not the opener's current one: the child's attempts
/// derive their replay keys and causal parent from the identity the request
/// carries.
#[test]
fn lineage_comes_from_the_recorded_attempt_identity() {
    assert_eq!(
        lent()
            .parent_invocation
            .and_then(|parent| parent.effect_id().map(str::to_string)),
        Some("opener-parent".to_string())
    );
    assert_eq!(
        rebound(&request())
            .parent_invocation
            .and_then(|parent| parent.effect_id().map(str::to_string)),
        Some("recorded-parent".to_string())
    );
}

/// The environment the child was admitted under, resolved from its recorded
/// reference — never whatever the opener happens to be running now.
#[test]
fn the_child_runs_under_its_recorded_execution_environment() {
    assert_eq!(
        lent().execution_env_spec.policy.turn_budget,
        crate::TurnBudget::bounded(9)
    );
    assert_eq!(
        rebound(&request()).execution_env_spec.policy.turn_budget,
        crate::TurnBudget::bounded(3)
    );
}

/// ADR 0099 §2: the child's **own** admitted controller, never the lent one.
/// This is the authority boundary; everything else on the checklist is
/// attribution.
#[test]
fn the_child_runs_on_its_own_admitted_controller() {
    assert_ne!(
        lent().effect_controller.scoped().execution_scope(),
        &ExecutionScope::turn("child-session", "turn"),
        "the lent controller must be admitted under a scope that is not the child's"
    );
    assert_eq!(
        rebound(&request())
            .effect_controller
            .scoped()
            .execution_scope(),
        &ExecutionScope::turn("child-session", "turn")
    );
}

/// Child-local buffers. Their contents ride the child's outcome (§6, §13), so a
/// child that wrote into the opener's buffers would put its facts somewhere its
/// settlement cannot carry them from — and would smuggle the opener's pending
/// facts into its own settlement.
#[test]
fn the_child_gets_fresh_checkpoint_and_trigger_buffers() {
    let lent = lent();
    lent.checkpoint_messages.enqueue(vec![crate::PluginMessage {
        id: None,
        role: crate::MessageRole::Assistant,
        content: "the opener's".to_string(),
        origin: None,
        parts: Vec::new(),
        attachments: Vec::new(),
    }]);
    let child = rebind_child_dispatch(
        &lent,
        &request(),
        child_controller(),
        spec(3),
        &ToolUsageLedger::new(),
    );
    assert!(
        child.checkpoint_messages.drain().is_empty(),
        "a child must not inherit the opener's committed messages"
    );
    assert!(child.trigger_outcomes.drain().is_empty());
    assert_eq!(
        lent.checkpoint_messages.drain().len(),
        1,
        "and must not drain them out from under the opener either"
    );
}

/// Everything not on the checklist is deployment wiring and live channels, and
/// §3 puts both on the lent side of the split. Asserted by identity, so a
/// rebind that quietly rebuilt one of them fails here.
#[test]
fn everything_not_on_the_checklist_is_the_lent_value() {
    let lent = lent();
    let child = rebind_child_dispatch(
        &lent,
        &request(),
        child_controller(),
        spec(3),
        &ToolUsageLedger::new(),
    );
    assert!(Arc::ptr_eq(&lent.plugins, &child.plugins));
    assert!(Arc::ptr_eq(&lent.tools, &child.tools));
    assert!(Arc::ptr_eq(&lent.processes, &child.processes));
    assert!(Arc::ptr_eq(&lent.sessions, &child.sessions));
    assert!(Arc::ptr_eq(
        &lent.session_lifecycle,
        &child.session_lifecycle
    ));
    assert!(Arc::ptr_eq(&lent.session_graph, &child.session_graph));
    assert!(Arc::ptr_eq(&lent.attachment_store, &child.attachment_store));
    assert!(Arc::ptr_eq(
        &lent.attachment_source_policy,
        &child.attachment_source_policy
    ));
    assert!(Arc::ptr_eq(&lent.clock, &child.clock));
    assert!(lent.event_tx.same_channel(&child.event_tx));
}

/// §1 and the registry's key rule, at the driver's door: a child whose opener
/// is not live in this process is **neither run nor failed**. The resolver
/// answers absence, the group leaves the child accepted, and the process whose
/// opener is live runs it.
#[test]
fn a_child_whose_opener_is_not_registered_here_is_not_routed() {
    let host: Arc<dyn EffectHost> = Arc::new(crate::NativeEffectHost::default());
    let tool_children = ToolChildHost::new(
        &host,
        Arc::new(crate::InMemoryProcessExecutionEnvStore::default()),
    );
    let envelope = crate::RuntimeEffectEnvelope::new(
        crate::RuntimeEffectInvocation::new(
            crate::EffectAddress::new(ExecutionScope::turn("child-session", "turn"), "child")
                .expect("a valid effect address"),
            crate::RuntimeAttribution::for_session("child-session"),
            "child",
        ),
        RuntimeEffectCommand::ToolInvocation {
            request: Box::new(request()),
        },
    );

    assert!(
        super::super::group_drain::GroupExecutors::executor_for(tool_children.as_ref(), &envelope)
            .is_none(),
        "an unregistered opener is a routing fact, not an executor and not a failure"
    );

    let live = LiveOpenerContext::capture(&lent()).expect("a shared controller lends a context");
    let guard = tool_children
        .openers()
        .register(crate::EffectOpener::turn("child-session", "turn"), live);
    assert!(
        super::super::group_drain::GroupExecutors::executor_for(tool_children.as_ref(), &envelope)
            .is_some(),
        "the same child routes once its opener is live here"
    );
    drop(guard);
    assert!(
        super::super::group_drain::GroupExecutors::executor_for(tool_children.as_ref(), &envelope)
            .is_none(),
        "and stops routing when the opener's registration ends"
    );
}

/// A command that is not a tool child is honestly not this resolver's, and it
/// says so rather than refusing the group on someone else's behalf.
#[test]
fn the_resolver_answers_only_for_tool_children() {
    let host: Arc<dyn EffectHost> = Arc::new(crate::NativeEffectHost::default());
    let tool_children = ToolChildHost::new(
        &host,
        Arc::new(crate::InMemoryProcessExecutionEnvStore::default()),
    );
    let envelope = crate::RuntimeEffectEnvelope::new(
        crate::RuntimeEffectInvocation::new(
            crate::EffectAddress::new(ExecutionScope::turn("child-session", "turn"), "sleep")
                .expect("a valid effect address"),
            crate::RuntimeAttribution::for_session("child-session"),
            "sleep",
        ),
        RuntimeEffectCommand::Sleep {
            spec: crate::SleepSpec::For { duration_ms: 1 },
        },
    );
    assert!(
        super::super::group_drain::GroupExecutors::executor_for(tool_children.as_ref(), &envelope)
            .is_none()
    );
}

/// One function derives an opener from an admitted scope, exhaustively, so a
/// scope arm added later is a compile error rather than a silently
/// unregistered opener. The derivation is the bridge's
/// (`cell_opener_for_scope`, FIG-3394): a turn and a queued drain name their
/// opener from the scope alone; a process scope names it only through the
/// pinned incarnation the runner bound, never from the reusable name.
#[test]
fn opener_derivation_names_every_admitted_opener_scope() {
    let turn = ExecutionScope::turn("session", "turn");
    assert_eq!(
        opener_for_execution_scope(&turn, None),
        Some(crate::EffectOpener::turn("session", "turn"))
    );
    let drain = ExecutionScope::queue_drain("session", "drain-1");
    assert_eq!(
        opener_for_execution_scope(&drain, None),
        Some(crate::EffectOpener::queue_drain("session", "drain-1"))
    );
    let process_ref = crate::ProcessRef::new(
        "process-1",
        crate::ProcessIncarnation::from_registration_sequence(7),
    );
    let process = ExecutionScope::process("process-1");
    assert_eq!(
        opener_for_execution_scope(&process, Some(&process_ref)),
        Some(crate::EffectOpener::process(process_ref.clone()))
    );
    // A process scope without its pinned incarnation — or with a foreign one —
    // names nothing rather than an opener minted from the reusable name.
    assert!(opener_for_execution_scope(&process, None).is_none());
    let foreign = crate::ProcessRef::new(
        "other",
        crate::ProcessIncarnation::from_registration_sequence(7),
    );
    assert!(opener_for_execution_scope(&process, Some(&foreign)).is_none());
    for scope in [
        ExecutionScope::session_delete("session"),
        ExecutionScope::runtime_operation("op-1"),
    ] {
        assert!(
            opener_for_execution_scope(&scope, None).is_none(),
            "{scope:?} names no opener"
        );
    }
}
