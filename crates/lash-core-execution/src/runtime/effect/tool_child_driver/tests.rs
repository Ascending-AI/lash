//! The rebind checklist, one negative test per line.
//!
//! Every case here makes the **lent** opener context deliberately wrong and
//! asserts the child followed its recorded request instead. A child running
//! under its opener's authority instead of its own recorded authority is the
//! failure [`rebind_child_dispatch`] exists to make impossible, and a test that
//! only asserted the happy path would pass just as well against a rebind that
//! forgot a field.

use std::sync::Arc;

use lash_sansio::sync::MutexExt;

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
    request_with_identity(ToolAttemptEffectIdentity::Scalar {
        parent: Some(invocation("recorded-parent")),
    })
}

fn request_with_identity(identity: ToolAttemptEffectIdentity) -> ToolChildRequest {
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
        identity,
        ToolChildScope {
            opener: crate::EffectOpener::turn("child-session", "turn"),
            admitted_scope: crate::AdmittedScope::turn("child-session", "turn"),
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
    lent_with_direct_completions(crate::DirectCompletionClient::unavailable(
        "direct completions are unavailable in this test context",
    ))
}

fn lent_with_direct_completions(
    direct_completions: crate::DirectCompletionClient<'static>,
) -> ToolDispatchContext<'static> {
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
        direct_completions,
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
        crate::AdmittedScope::turn("child-session", "turn"),
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
    .expect("the lent client's test service binds to any recorded authority")
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
/// Catalog *for the recorded call*. The lent catalog holds a *different* tool
/// with a *different* retry policy, so a rebind that kept it wholesale would
/// run the child under a policy it was never admitted with — or fail to
/// resolve it at all.
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
            .expect("the admitted manifest resolves at the child's own id");
    assert_eq!(resolved.retry_policy, ToolRetryPolicy::safe(4, 10, 100));
    // The ruling binds the recorded call's id; every other live entry is lent
    // unchanged, because a call the child's orchestrating body issues is a
    // fresh admission and consults the live catalog.
    assert!(
        crate::tool_dispatch::resolve_callable_manifest_by_id(&child, &ToolId::from("opener-tool"))
            .is_some(),
        "a fresh nested admission resolves through the lent live catalog"
    );
}

/// Ruling 1 at the recorded id itself: a live entry that shares the child's
/// tool id but drifted since admission — a different retry policy here —
/// loses to the recorded manifest, because the reopen may not consult it for
/// this call.
#[test]
fn a_live_entry_at_the_childs_own_id_loses_to_the_recorded_manifest() {
    let mut lent = lent();
    let mut drifted = manifest("search");
    drifted.retry_policy = ToolRetryPolicy::Never;
    lent.tool_catalog = Arc::new(crate::ToolCatalog::from_tool_definitions(vec![
        crate::ToolDefinition {
            manifest: drifted,
            contract: crate::ToolContract::default(),
        },
    ]));
    let child = rebind_child_dispatch(
        &lent,
        &request(),
        child_controller(),
        spec(3),
        &ToolUsageLedger::new(),
    )
    .expect("the lent client's test service binds to any recorded authority");
    let resolved =
        crate::tool_dispatch::resolve_callable_manifest_by_id(&child, &ToolId::from("search"))
            .expect("the recorded manifest resolves at the child's own id");
    assert_eq!(
        resolved.retry_policy,
        ToolRetryPolicy::safe(4, 10, 100),
        "the recorded retry policy wins over a drifted live entry at the same id"
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

/// The claim pin is the recorded `AdmittedScope`, never `enclosing_process`.
/// A process opener legitimately encloses its own incarnation, so the pair
/// `opener = P#7, enclosing = P#7` validates — but when the admitted claim is a
/// turn scope, the controller must stay that turn's controller. The retired
/// post-admission pin block would have pinned P#7 onto it instead, making
/// the execution context the claim pin.
#[test]
fn a_process_openers_enclosing_incarnation_is_never_the_claim_pin() {
    let mut request = request();
    let opener_ref = crate::ProcessRef::new(
        "worker",
        crate::ProcessIncarnation::from_registration_sequence(7),
    );
    request.scope.opener = crate::EffectOpener::process(opener_ref.clone());
    request.enclosing_process = Some(opener_ref.clone());
    request
        .validate()
        .expect("a process opener enclosing its own incarnation is a legal request");

    let host: Arc<dyn EffectHost> = Arc::new(crate::NativeEffectHost::default());
    let tool_children = ToolChildHost::new(
        &host,
        Arc::new(crate::InMemoryProcessExecutionEnvStore::default()),
    );
    let binding = crate::GroupChildBinding {
        child: crate::EffectAddress::new(ExecutionScope::turn("child-session", "turn"), "child")
            .expect("a valid child address"),
        membership: crate::EffectGroupMembership {
            group_key: "group".to_string(),
            position: 0,
            wake: crate::GroupWakePolicy::All,
            loser_disposition: crate::LoserPolicy::RunToCompletion,
        },
    };
    let controller = tool_children
        .child_controller(&request.scope.admitted_scope, binding)
        .expect("the admitted pair constructs the child's controller");
    assert_eq!(
        controller.execution_scope(),
        &ExecutionScope::turn("child-session", "turn"),
        "the controller is the recorded claim's, a turn scope"
    );
    assert!(
        controller.admitted_process().is_none(),
        "the opener's incarnation never became the claim pin"
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
    lent.trigger_outcomes
        .enqueue(crate::tool_dispatch::ToolTriggerEffectOutcome {
            source_type: "watcher".to_string(),
            source_key: "the opener's".to_string(),
            occurrence_id: "occurrence-1".to_string(),
            payload: serde_json::json!({ "observed": true }),
            idempotency_key: "occurrence-1".to_string(),
            source: None,
            deliveries: Vec::new(),
        });
    let child = rebind_child_dispatch(
        &lent,
        &request(),
        child_controller(),
        spec(3),
        &ToolUsageLedger::new(),
    )
    .expect("the lent client's test service binds to any recorded authority");
    assert!(
        child.checkpoint_messages.drain().is_empty(),
        "a child must not inherit the opener's committed messages"
    );
    assert!(
        child.trigger_outcomes.drain().is_empty(),
        "a child must not inherit the opener's pending trigger receipts"
    );
    assert_eq!(
        lent.checkpoint_messages.drain().len(),
        1,
        "and must not drain them out from under the opener either"
    );
    let lent_triggers = lent.trigger_outcomes.drain();
    assert_eq!(
        lent_triggers.len(),
        1,
        "the opener's own trigger receipt stays on the lent buffer"
    );
    assert_eq!(lent_triggers[0].occurrence_id, "occurrence-1");
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
    )
    .expect("the lent client's test service binds to any recorded authority");
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

/// §3's ruling stated as a negative: a group child holds no runtime execution
/// context — the live context is exactly the authority a retained child must
/// not borrow, because the facts it would record are the child's buffers'
/// job to carry into the settlement. What the context *does* name is the
/// rebound dispatch itself, never the opener's live one.
#[test]
fn a_childs_tool_context_holds_no_runtime_execution_context() {
    let lent = lent();
    let rebound = Arc::new(
        rebind_child_dispatch(
            &lent,
            &request(),
            child_controller(),
            spec(3),
            &ToolUsageLedger::new(),
        )
        .expect("the lent client's test service binds to any recorded authority"),
    );
    let context = child_tool_context(
        &rebound,
        &request(),
        crate::runtime::TurnCancelWait::unobserved(tokio_util::sync::CancellationToken::new()),
        crate::tool_dispatch::OrchestratingStartsBuffer::default(),
    );
    assert!(
        context.runtime_execution_context.is_none(),
        "a group child is not lent a live execution context (§3)"
    );
    assert!(
        context
            .runtime_dispatch
            .as_ref()
            .is_some_and(|dispatch| Arc::ptr_eq(dispatch, &rebound)),
        "the context's dispatch is the child's rebound dispatch"
    );
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

    let lent_dispatch = lent();
    let lent_controller = lent_dispatch
        .effect_controller
        .scoped()
        .to_static()
        .expect("the lent dispatch's controller is 'static");
    let live = LiveOpenerContext::capture(
        &lent_dispatch,
        lent_controller,
        tokio_util::sync::CancellationToken::new(),
    );
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

/// Live-opener registration derives through the one owner derivation
/// (`EffectOpener::for_scope`, FIG-3417), exhaustively, so a scope arm added
/// later is a compile error rather than a silently unregistered opener: a
/// turn and a queued drain name their opener from the scope alone; a process
/// scope names it only through the pinned incarnation the runner bound,
/// never from the reusable name.
#[test]
fn opener_derivation_names_every_admitted_opener_scope() {
    let turn = crate::AdmittedScope::turn("session", "turn");
    assert_eq!(
        opener_for_execution_scope(&turn),
        Some(crate::EffectOpener::turn("session", "turn"))
    );
    let drain = crate::AdmittedScope::queue_drain("session", "drain-1");
    assert_eq!(
        opener_for_execution_scope(&drain),
        Some(crate::EffectOpener::queue_drain("session", "drain-1"))
    );
    let process_ref = crate::ProcessRef::new(
        "process-1",
        crate::ProcessIncarnation::from_registration_sequence(7),
    );
    let process = crate::AdmittedScope::process(process_ref.clone());
    assert_eq!(
        opener_for_execution_scope(&process),
        Some(crate::EffectOpener::process(process_ref.clone()))
    );
    // The half-admitted shapes — a process scope with no incarnation, or with
    // another process's — cannot reach the derivation at all: `AdmittedScope`
    // refuses them at construction, so there is no call site to test.
    for admitted in [
        crate::AdmittedScope::session_delete("session"),
        crate::AdmittedScope::runtime_operation("op-1"),
    ] {
        assert!(
            opener_for_execution_scope(&admitted).is_none(),
            "{admitted:?} names no opener"
        );
    }
}

/// A `DirectCompletionService` probe on the real `Runtime` completion
/// source, so a rebind exercises `DirectCompletionService::bind_tool_child`
/// the way a managed-LLM transport's would — a test-fn source would bypass
/// it entirely. Everything the client must rebind is captured where the
/// service receives it: the recorded session and environment at bind, and
/// the admitted controller's scope, the recorded turn, and the usage sink at
/// call.
#[derive(Default)]
struct CompletionProbe {
    binds: std::sync::Mutex<Vec<(SessionId, ProcessExecutionEnvSpec)>>,
    completes: std::sync::Mutex<Vec<(ExecutionScope, Option<crate::TurnId>, bool)>>,
}

/// What the service's `complete` does after it has received the call. The
/// billed-failure arm models `apply_direct_outcome`'s ordering: the sealed
/// provider record — a billed attempt that then failed — is a usage fact of
/// the call and feeds the sink *before* the error projects.
#[derive(Clone, Copy)]
enum ProbeCall {
    Succeed,
    FailAfterBilling,
}

struct ProbedCompletionService {
    probe: Arc<CompletionProbe>,
    /// What `bind_tool_child` answers. `false` models a service that cannot
    /// prove it executes under the recorded authority — the production
    /// transport's answer to a foreign session.
    bindable: bool,
    call: ProbeCall,
}

#[async_trait::async_trait]
impl crate::direct_completion_client::DirectCompletionService for ProbedCompletionService {
    async fn complete(
        &self,
        _request: crate::DirectRequest,
        _usage_source: &str,
        effect_controller: crate::ScopedEffectController<'_>,
        turn_id: Option<&crate::TurnId>,
        _position: crate::direct_completion_client::DirectExecutionPosition,
        usage_sink: Option<&crate::runtime::ToolUsageLedger>,
    ) -> Result<crate::DirectCompletion, crate::PluginError> {
        self.probe.completes.lock_recover().push((
            effect_controller.execution_scope().clone(),
            turn_id.cloned(),
            usage_sink.is_some(),
        ));
        // The sealed record feeds the bound sink the way the runtime service
        // does — a billed provider attempt is a usage fact of the call,
        // whatever the call then returns.
        if let Some(sink) = usage_sink {
            sink.record(
                &match self.call {
                    ProbeCall::Succeed => probed_call_record(),
                    ProbeCall::FailAfterBilling => failed_billed_call_record(),
                },
                "test-source",
                "test-model",
            );
        }
        match self.call {
            ProbeCall::Succeed => Ok(probed_completion()),
            ProbeCall::FailAfterBilling => Err(crate::PluginError::Session(
                "the provider attempt billed, then failed".to_string(),
            )),
        }
    }

    async fn complete_llm(
        &self,
        _request: crate::LlmRequest,
        _usage_source: &str,
        _effect_controller: crate::ScopedEffectController<'_>,
        _turn_id: Option<&crate::TurnId>,
        _position: crate::direct_completion_client::DirectExecutionPosition,
        _caused_by: Option<crate::CausalRef>,
        _usage_sink: Option<&crate::runtime::ToolUsageLedger>,
    ) -> Result<crate::DirectLlmCompletion, crate::PluginError> {
        Err(crate::PluginError::Session(
            "the rebind probe answers text completions only".to_string(),
        ))
    }

    fn bind_tool_child(
        self: Arc<Self>,
        session_id: &crate::SessionId,
        execution_env_spec: &crate::ProcessExecutionEnvSpec,
    ) -> Option<Arc<dyn crate::direct_completion_client::DirectCompletionService>> {
        self.probe
            .binds
            .lock_recover()
            .push((session_id.clone(), execution_env_spec.clone()));
        self.bindable.then(|| {
            Arc::new(ProbedCompletionService {
                probe: Arc::clone(&self.probe),
                bindable: self.bindable,
                call: self.call,
            }) as Arc<dyn crate::direct_completion_client::DirectCompletionService>
        })
    }
}

fn probed_call_record() -> crate::LlmCallRecord {
    crate::LlmCallRecord {
        call_id: crate::LlmCallId("probed-call".to_string()),
        label: None,
        replay_drops: Vec::new(),
        attempts: vec![crate::AttemptRecord {
            ordinal: 1,
            started_at: 0,
            duration: std::time::Duration::ZERO,
            outcome: crate::AttemptOutcome::Completed,
            protocol_position: crate::ProtocolPosition::ResponseObserved,
            retry_budget_consumed: false,
            retry_decision: None,
            error: None,
            evidence: None,
            generation_disposition: None,
            usage: Some(lash_sansio::llm::types::LlmUsage {
                input_tokens: 5,
                output_tokens: 3,
                cache_read_input_tokens: 0,
                cache_write_input_tokens: 0,
                reasoning_output_tokens: 0,
            }),
            usage_disposition: crate::AttemptUsageDisposition::default(),
        }],
    }
}

/// A sealed provider record whose only attempt billed, then failed — the
/// hostile case §13's usage line exists for: the spend is a fact even though
/// the call's terminal outcome is an error.
fn failed_billed_call_record() -> crate::LlmCallRecord {
    let mut record = probed_call_record();
    record.call_id = crate::LlmCallId("failed-billed-call".to_string());
    record.attempts[0].outcome = crate::AttemptOutcome::Failed;
    record
}

fn probed_completion() -> crate::DirectCompletion {
    crate::DirectCompletion {
        text: "probed completion".to_string(),
        usage: crate::TokenUsage {
            input_tokens: 5,
            output_tokens: 3,
            ..crate::TokenUsage::default()
        },
        llm_call: probed_call_record(),
    }
}

fn probed_lent(
    probe: &Arc<CompletionProbe>,
    bindable: bool,
    call: ProbeCall,
) -> ToolDispatchContext<'static> {
    lent_with_direct_completions(crate::DirectCompletionClient::runtime(
        Arc::new(ProbedCompletionService {
            probe: Arc::clone(probe),
            bindable,
            call,
        }),
        crate::runtime::RuntimeEffectControllerHandle::shared(Arc::new(
            crate::NativeRuntimeEffectController::default(),
        )),
        Some(crate::TurnId::from("opener-turn")),
    ))
}

/// §3's managed-LLM line of the checklist: the lent client is rebound to the
/// child's recorded authority, so the service is asked to bind the recorded
/// session and environment — and the call it then receives arrives on the
/// child's admitted controller under the recorded turn, with the child's
/// usage ledger bound, never the opener's current facts.
#[tokio::test]
async fn the_lent_completion_client_is_rebound_to_the_recorded_authority() {
    let probe = Arc::new(CompletionProbe::default());
    let lent = probed_lent(&probe, true, ProbeCall::Succeed);
    // A recorded parent that carries the child's turn, so the rebound
    // client's attribution observably comes from the journal and not from
    // the opener's minted `opener-turn`.
    let request = request_with_identity(ToolAttemptEffectIdentity::Scalar {
        parent: Some(crate::RuntimeInvocation::effect(
            crate::EffectAddress::new(
                ExecutionScope::turn("child-session", "turn"),
                "recorded-parent",
            )
            .expect("a valid effect address"),
            crate::RuntimeAttribution::for_turn("child-session", "child-turn", 0, 0),
            "recorded-parent",
        )),
    });
    let usage_ledger = ToolUsageLedger::new();
    let child = rebind_child_dispatch(&lent, &request, child_controller(), spec(3), &usage_ledger)
        .expect("the probe service binds to the recorded authority");

    {
        let binds = probe.binds.lock_recover();
        assert_eq!(
            binds.as_slice(),
            &[(SessionId::from("child-session"), spec(3))],
            "the service binds the recorded session and environment"
        );
    }

    let completion = child
        .direct_completions
        .direct_completion(
            crate::DirectRequest::text("law-model", "a managed call"),
            "law-source",
        )
        .await
        .expect("the rebound client completes");
    assert_eq!(completion.text, "probed completion");
    {
        let completes = probe.completes.lock_recover();
        assert_eq!(
            completes.as_slice(),
            &[(
                ExecutionScope::turn("child-session", "turn"),
                Some(crate::TurnId::from("child-turn")),
                true
            )],
            "the call arrived on the child's admitted controller under the \
             recorded turn, with the child's usage sink bound"
        );
    }
    let deltas = usage_ledger.take();
    assert_eq!(deltas.len(), 1, "the billed attempt lands on the child");
    assert_eq!(deltas[0].usage.input_tokens, 5);

    // The opener's own client is untouched: a call on it still arrives under
    // the turn it was minted with — the rebind cloned, it did not mutate.
    lent.direct_completions
        .direct_completion(
            crate::DirectRequest::text("law-model", "the opener's"),
            "law",
        )
        .await
        .expect("the lent client still completes");
    assert_eq!(
        probe
            .completes
            .lock_recover()
            .last()
            .map(|receipt| &receipt.1),
        Some(&Some(crate::TurnId::from("opener-turn"))),
        "the opener's client keeps the turn it was minted with"
    );
}

/// The other half of §3's managed-LLM line: a service that cannot prove it
/// executes under the recorded session and environment makes the rebind a
/// typed refusal, not a silent borrow of the opener's authority.
#[test]
fn a_completion_service_that_cannot_bind_the_recorded_authority_refuses() {
    let probe = Arc::new(CompletionProbe::default());
    let lent = probed_lent(&probe, false, ProbeCall::Succeed);
    let error = rebind_child_dispatch(
        &lent,
        &request(),
        child_controller(),
        spec(3),
        &ToolUsageLedger::new(),
    )
    .err()
    .expect("an unbindable service refuses the child");
    assert_eq!(
        error.code,
        crate::RuntimeErrorCode::RuntimeEffectToolChildRequestOpener
    );
    assert_eq!(
        probe.binds.lock_recover().len(),
        1,
        "the refusal came from asking, not from skipping the bind"
    );
}

/// §13's usage line: a provider attempt that billed and then failed is a
/// spend of the child even though the call returns an error. The service
/// feeds its sealed record to the bound sink before the error projects —
/// the same ordering `apply_direct_outcome` holds — so the ledger the
/// settlement drains already carries the failed attempt's usage.
#[tokio::test]
async fn a_billed_failed_completion_attempt_lands_on_the_child_usage() {
    let probe = Arc::new(CompletionProbe::default());
    let lent = probed_lent(&probe, true, ProbeCall::FailAfterBilling);
    let usage_ledger = ToolUsageLedger::new();
    let child = rebind_child_dispatch(
        &lent,
        &request(),
        child_controller(),
        spec(3),
        &usage_ledger,
    )
    .expect("the probe service binds to the recorded authority");

    child
        .direct_completions
        .direct_completion(
            crate::DirectRequest::text("law-model", "a call that bills then fails"),
            "law-source",
        )
        .await
        .expect_err("the failed call surfaces as an error");

    let deltas = usage_ledger.take();
    assert_eq!(
        deltas.len(),
        1,
        "the failed attempt's billed spend is retained, not dropped"
    );
    assert_eq!(
        deltas[0].llm_call_id,
        crate::LlmCallId("failed-billed-call".to_string())
    );
    assert_eq!(deltas[0].provider_attempt, 1);
    assert_eq!(deltas[0].usage.input_tokens, 5);
}

/// The tool-child host the recorded-authority checks run against.
fn tool_children(host: &Arc<dyn EffectHost>) -> Arc<ToolChildHost> {
    ToolChildHost::new(
        host,
        Arc::new(crate::InMemoryProcessExecutionEnvStore::default()),
    )
}

/// A controller that reports durable-journaled turn-control participation and
/// names the host's await-event authority, so the recorded-authority checks
/// see the durable arms rather than the native local ones.
struct DurableReplayController {
    authority_id: std::sync::OnceLock<String>,
}

impl crate::AwaitEventResolver for DurableReplayController {
    fn await_event_authority_binding_id(&self) -> Option<String> {
        self.authority_id.get().cloned()
    }
}

#[async_trait::async_trait]
impl crate::RuntimeEffectController for DurableReplayController {
    async fn turn_control_participation(
        &self,
    ) -> Result<crate::TurnControlParticipation, crate::RuntimeError> {
        Ok(crate::TurnControlParticipation::DurableJournaled)
    }

    async fn execute_effect(
        &self,
        _envelope: RuntimeEffectEnvelope,
        _local_executor: RuntimeEffectLocalExecutor<'_>,
    ) -> Result<RuntimeEffectOutcome, RuntimeEffectControllerError> {
        unreachable!("the authority-check tests execute no effects")
    }

    async fn open_effect_group(
        &self,
        _group: crate::RuntimeEffectGroup,
    ) -> Result<crate::EffectGroupHandle, RuntimeEffectControllerError> {
        Err(crate::effect_groups_unsupported("DurableReplayController"))
    }

    async fn await_next_settlement(
        &self,
        _handle: &mut crate::EffectGroupHandle,
        _cancel: crate::CancellationToken,
    ) -> Result<crate::GroupSettlement, RuntimeEffectControllerError> {
        Err(crate::effect_groups_unsupported("DurableReplayController"))
    }

    async fn close_effect_group(
        &self,
        _handle: crate::EffectGroupHandle,
        _disposition: crate::LoserPolicy,
    ) -> Result<(), RuntimeEffectControllerError> {
        Err(crate::effect_groups_unsupported("DurableReplayController"))
    }
}

/// A scoped durable-participant controller whose await-event authority is this
/// host's, so the host's binding derivation accepts it.
fn durable_child_controller(host: &Arc<dyn EffectHost>) -> ScopedEffectController<'static> {
    let controller = Arc::new(DurableReplayController {
        authority_id: std::sync::OnceLock::new(),
    });
    controller
        .authority_id
        .set(host.turn_control_binding_id())
        .expect("the authority id is set once");
    ScopedEffectController::shared(
        controller,
        crate::AdmittedScope::turn("child-session", "turn"),
    )
    .expect("a valid child scope")
}

/// A request whose recorded cancellation authority is exactly what `host`
/// derives for the child's admitted scope — the fixture every durable-side
/// check needs, because a durable participant always records `Some`.
fn durably_admitted_request(
    host: &Arc<dyn EffectHost>,
    routing: ToolChildCompletionRouting,
) -> ToolChildRequest {
    let derived = crate::runtime::effect::executor::turn_control_binding_id_for_scope(
        &host.turn_control_binding_id(),
        &ExecutionScope::turn("child-session", "turn"),
    )
    .expect("a scope-derived binding id");
    let mut request = request().with_cancellation_authority(
        crate::TurnControlBindingId::new(derived).expect("a valid binding id"),
    );
    request.completion_routing = routing;
    request
}

/// §3's cancellation line, wrong direction: a recorded binding this host did
/// not mint for the admitted scope is a foreign authority — the cooperative
/// signal it would honour is not the one this opener sends.
#[tokio::test]
async fn a_foreign_cancellation_binding_is_refused() {
    let host: Arc<dyn EffectHost> = Arc::new(crate::NativeEffectHost::default());
    let tool_children = tool_children(&host);
    let request = request().with_cancellation_authority(
        crate::TurnControlBindingId::new("a-binding-this-host-did-not-mint")
            .expect("a valid binding id"),
    );
    let error = validate_recorded_authorities(&tool_children, &child_controller(), &request)
        .await
        .expect_err("a binding this host did not mint for the scope is refused");
    assert_eq!(
        error.code,
        crate::RuntimeErrorCode::RuntimeEffectToolChildCancellationAuthority
    );
}

/// And the matching one is accepted: presence was never the check — the
/// recorded id must equal what this host derives for the admitted scope, and
/// it is only legal under the durable participation that minted it.
#[tokio::test]
async fn the_recorded_cancellation_binding_is_accepted() {
    let host: Arc<dyn EffectHost> = Arc::new(crate::NativeEffectHost::default());
    let tool_children = tool_children(&host);
    let controller = durable_child_controller(&host);
    let request = durably_admitted_request(&host, ToolChildCompletionRouting::Inline);
    validate_recorded_authorities(&tool_children, &controller, &request)
        .await
        .expect("the binding this host derives for the admitted scope is accepted");
}

/// A `None` record is legal only under local participation: reopened against
/// a durable-journaled controller it is an inconsistency, because the
/// cooperative authority exists and the record that omits it lies.
#[tokio::test]
async fn a_missing_cancellation_record_on_a_durable_participant_is_refused() {
    let host: Arc<dyn EffectHost> = Arc::new(crate::NativeEffectHost::default());
    let tool_children = tool_children(&host);
    let controller = durable_child_controller(&host);
    let error = validate_recorded_authorities(&tool_children, &controller, &request())
        .await
        .expect_err("a durable participant without a recorded binding is inconsistent");
    assert_eq!(
        error.code,
        crate::RuntimeErrorCode::RuntimeEffectToolChildCancellationAuthority
    );
}

/// The recorded cancellation authority is the scope the child's waits
/// observe: whether the child is waiting out a retry, parked on a deferred
/// completion, or running the attempt body, the cooperative signal that can
/// reach it is the recorded binding's — never a foreign opener's.
#[tokio::test]
async fn the_cancel_wait_observes_the_admitted_scope() {
    let with_authority = request().with_cancellation_authority(
        crate::TurnControlBindingId::new("recorded-authority").expect("a valid binding id"),
    );
    let dispatch = Arc::new(rebound(&with_authority));
    let wait = child_turn_cancel_wait(
        &dispatch,
        &with_authority,
        &tokio_util::sync::CancellationToken::new(),
    );
    let observed = wait
        .process_turn_cancellation()
        .expect("a recorded authority observes turn cancellation");
    assert_eq!(
        observed.scope,
        ExecutionScope::turn("child-session", "turn"),
        "the wait observes the child's admitted scope"
    );

    let request = request();
    let dispatch = Arc::new(rebound(&request));
    let wait = child_turn_cancel_wait(
        &dispatch,
        &request,
        &tokio_util::sync::CancellationToken::new(),
    );
    assert!(
        wait.process_turn_cancellation().is_none(),
        "no recorded authority, no turn observation"
    );
}

/// §14's routing line, wrong issuer: a process-lifetime key minted by another
/// registry is unresolvable here — refused, never re-minted into a second
/// dispatch.
#[tokio::test]
async fn a_process_lifetime_key_from_a_foreign_issuer_is_refused() {
    let host: Arc<dyn EffectHost> = Arc::new(crate::NativeEffectHost::default());
    let tool_children = tool_children(&host);
    let mut request = request();
    request.completion_routing = ToolChildCompletionRouting::ProcessLifetime {
        issuer: crate::TurnControlBindingId::new("registry-not-this-one")
            .expect("a valid binding id"),
    };
    let error = validate_recorded_authorities(&tool_children, &child_controller(), &request)
        .await
        .expect_err("a key issued by another registry is refused");
    assert_eq!(
        error.code,
        crate::RuntimeErrorCode::RuntimeEffectToolChildCompletionRouting
    );
}

/// The same routing bound to this host's registry identity is accepted.
#[tokio::test]
async fn a_process_lifetime_key_from_this_registry_is_accepted() {
    let host: Arc<dyn EffectHost> = Arc::new(crate::NativeEffectHost::default());
    let tool_children = tool_children(&host);
    let mut request = request();
    request.completion_routing = ToolChildCompletionRouting::ProcessLifetime {
        issuer: crate::TurnControlBindingId::new(host.turn_control_binding_id())
            .expect("a valid binding id"),
    };
    validate_recorded_authorities(&tool_children, &child_controller(), &request)
        .await
        .expect("a key this registry issued resolves here");
}

/// `Durable` routing needs a durable await-event authority behind the child's
/// controller — on a locally-participating one the child is refused rather
/// than parked on a key nothing resolves. The request is a consistent local
/// admission (`None` cancellation record), so the refusal it reaches is the
/// routing check's, not the cancellation matrix's.
#[tokio::test]
async fn durable_routing_without_a_durable_authority_is_refused() {
    let host: Arc<dyn EffectHost> = Arc::new(crate::NativeEffectHost::default());
    let tool_children = tool_children(&host);
    let mut request = request();
    request.completion_routing = ToolChildCompletionRouting::Durable;
    let error = validate_recorded_authorities(&tool_children, &child_controller(), &request)
        .await
        .expect_err("durable routing needs a durable await-event authority");
    assert_eq!(
        error.code,
        crate::RuntimeErrorCode::RuntimeEffectToolChildCompletionRouting
    );
}

/// And it is admitted where the authority exists.
#[tokio::test]
async fn durable_routing_with_a_durable_authority_is_accepted() {
    let host: Arc<dyn EffectHost> = Arc::new(crate::NativeEffectHost::default());
    let tool_children = tool_children(&host);
    let controller = durable_child_controller(&host);
    let request = durably_admitted_request(&host, ToolChildCompletionRouting::Durable);
    validate_recorded_authorities(&tool_children, &controller, &request)
        .await
        .expect("durable routing under a durable authority is admitted");
}

/// §2's lifetime line at the driver: the runner `executor_for` hands back owns
/// the `LiveOpenerContext` it resolved against. Dropping the opener's
/// registration guard between resolution and execution must neither stall the
/// child on a re-registration nothing promised nor rebind it to a successor —
/// the accepted child completes on the captured context.
#[tokio::test]
async fn a_resolved_child_executes_on_the_captured_opener_context() {
    let host: Arc<dyn EffectHost> = Arc::new(crate::NativeEffectHost::default());
    let env_store = Arc::new(crate::InMemoryProcessExecutionEnvStore::default());
    let tool_children = ToolChildHost::new(&host, env_store.clone());
    let mut request = request();
    request.execution_env = crate::publish_process_execution_env(
        env_store.as_ref(),
        &crate::ArtifactOwner::host("tool-child-driver-tests"),
        &spec(3),
    )
    .await
    .expect("the recorded environment publishes");
    let opener = request.scope.opener.clone();
    let envelope = crate::RuntimeEffectEnvelope::new(
        crate::RuntimeEffectInvocation::new(
            crate::EffectAddress::new(ExecutionScope::turn("child-session", "turn"), "child")
                .expect("a valid effect address"),
            crate::RuntimeAttribution::for_session("child-session"),
            "child",
        ),
        RuntimeEffectCommand::ToolInvocation {
            request: Box::new(request),
        },
    )
    .in_effect_group(
        "group",
        0,
        crate::GroupWakePolicy::All,
        crate::LoserPolicy::Cancel,
    );
    let lent_dispatch = lent();
    let lent_controller = lent_dispatch
        .effect_controller
        .scoped()
        .to_static()
        .expect("the lent dispatch's controller is 'static");
    let live = LiveOpenerContext::capture(
        &lent_dispatch,
        lent_controller,
        tokio_util::sync::CancellationToken::new(),
    );
    let guard = tool_children.openers().register(opener, live);
    let executor =
        super::super::group_drain::GroupExecutors::executor_for(tool_children.as_ref(), &envelope)
            .expect("the live opener routes the child");

    // The opener's registration ends between resolution and execution: the
    // runner must still complete on the context it captured rather than wait
    // for a re-registration nothing promises — the timeout is what makes a
    // wait-for-reregistration regression a failure and not a hang.
    drop(guard);
    let outcome = tokio::time::timeout(
        std::time::Duration::from_secs(5),
        executor.execute(envelope),
    )
    .await
    .expect("a resolved child never waits for the opener to re-register")
    .expect("the captured context executes the child");
    assert!(
        matches!(outcome, crate::RuntimeEffectOutcome::ToolInvocation { .. }),
        "the child settles its own recorded work"
    );
}

/// §14's routing line, right issuer wrong participation: a process-lifetime
/// key is a local-participation admission — a durable journal would resolve it
/// after the issuing process is gone. Even this registry's own issuer id is
/// therefore refused on a durable-journaled controller, before any key is
/// prepared.
#[tokio::test]
async fn a_process_lifetime_key_is_refused_under_durable_participation() {
    let host: Arc<dyn EffectHost> = Arc::new(crate::NativeEffectHost::default());
    let tool_children = tool_children(&host);
    let controller = durable_child_controller(&host);
    let mut request = durably_admitted_request(&host, ToolChildCompletionRouting::Inline);
    request.completion_routing = ToolChildCompletionRouting::ProcessLifetime {
        issuer: crate::TurnControlBindingId::new(host.turn_control_binding_id())
            .expect("a valid binding id"),
    };
    let error = validate_recorded_authorities(&tool_children, &controller, &request)
        .await
        .expect_err("a process-lifetime key cannot ride a durable journal");
    assert_eq!(
        error.code,
        crate::RuntimeErrorCode::RuntimeEffectToolChildCompletionRouting
    );
}

/// Records the turn-cancel shape of every journaled sleep, the way the
/// retry-gate suite's recorder does: the observable a signalled host gate
/// would act on is `observe_turn_cancel`, so asserting the shape is asserting
/// the gate was never attached.
#[derive(Default)]
struct SleepShapeRecorder {
    sleeps: std::sync::Mutex<Vec<(bool, Option<crate::ExecutionScope>)>>,
}

impl crate::AwaitEventResolver for SleepShapeRecorder {}

#[async_trait::async_trait]
impl crate::RuntimeEffectController for SleepShapeRecorder {
    async fn execute_effect(
        &self,
        envelope: RuntimeEffectEnvelope,
        local_executor: RuntimeEffectLocalExecutor<'_>,
    ) -> Result<RuntimeEffectOutcome, RuntimeEffectControllerError> {
        if matches!(&envelope.command, crate::RuntimeEffectCommand::Sleep { .. }) {
            let options = local_executor.into_sleep_options();
            self.sleeps
                .lock_recover()
                .push((options.observe_turn_cancel, options.turn_cancel_scope));
            Ok(crate::RuntimeEffectOutcome::Sleep)
        } else {
            local_executor.execute(envelope).await
        }
    }

    async fn open_effect_group(
        &self,
        _group: crate::RuntimeEffectGroup,
    ) -> Result<crate::EffectGroupHandle, RuntimeEffectControllerError> {
        Err(crate::effect_groups_unsupported("SleepShapeRecorder"))
    }

    async fn await_next_settlement(
        &self,
        _handle: &mut crate::EffectGroupHandle,
        _cancel: crate::CancellationToken,
    ) -> Result<crate::GroupSettlement, RuntimeEffectControllerError> {
        Err(crate::effect_groups_unsupported("SleepShapeRecorder"))
    }

    async fn close_effect_group(
        &self,
        _handle: crate::EffectGroupHandle,
        _disposition: crate::LoserPolicy,
    ) -> Result<(), RuntimeEffectControllerError> {
        Err(crate::effect_groups_unsupported("SleepShapeRecorder"))
    }

    async fn commit_group_child_final(
        &self,
        _commit: crate::runtime::effect::GroupChildFinalCommit,
    ) -> Result<crate::runtime::effect::EffectGroupChildCommitOutcome, RuntimeEffectControllerError>
    {
        // This recorder journals no durable group membership, so every row it
        // is asked about is ungrouped — the same cheap answer a real host
        // gives an ordinary attempt.
        Ok(crate::runtime::effect::EffectGroupChildCommitOutcome::Ungrouped)
    }
}

/// A leaf that fails retryably once, so a nested batch has one journaled retry
/// sleep to observe.
struct RetryOnceTools {
    attempts: std::sync::atomic::AtomicUsize,
}

#[async_trait::async_trait]
impl crate::ToolProvider for RetryOnceTools {
    fn tool_manifests(&self) -> Vec<ToolManifest> {
        vec![manifest("retry-leaf")]
    }

    fn resolve_contract(&self, name: &str) -> Option<Arc<crate::ToolContract>> {
        (name == "retry-leaf").then(|| Arc::new(crate::ToolContract::default()))
    }

    async fn execute(&self, _call: crate::ToolCall<'_>) -> crate::ToolAttemptOutcome {
        let attempt = self
            .attempts
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        if attempt == 0 {
            crate::ToolOutcome::retryable_failure(
                crate::ToolFailureClass::External,
                "transient",
                "transient failure",
                Some(1),
            )
            .into()
        } else {
            crate::ToolOutcome::ok(serde_json::json!("retried")).into()
        }
    }
}

/// §3's exact-wait line inside a nested batch: the driver computes the
/// cancellation trio once from the *recorded* authority and every nested
/// retry and deferred wait rides that exact value. A child admitted with no
/// cooperative authority must keep its nested waits unobserved even though
/// its admitted scope names a turn — a wait derived from the scope alone
/// (`ScopedEffectController::turn_cancel_wait` always yields an observing
/// trio) would attach the host's gate and let a signalled turn cancel reach a
/// child that never admitted the signal.
#[tokio::test]
async fn a_nested_retry_sleep_observes_no_host_turn_gate() {
    let recorder = Arc::new(SleepShapeRecorder::default());
    let controller = ScopedEffectController::shared(
        recorder.clone(),
        crate::AdmittedScope::turn("child-session", "turn"),
    )
    .expect("a valid child scope");
    let request = request();
    let mut lent = lent();
    lent.tools = Arc::new(RetryOnceTools {
        attempts: std::sync::atomic::AtomicUsize::new(0),
    });
    lent.tool_catalog = Arc::new(crate::ToolCatalog::from_tool_definitions(vec![
        crate::ToolDefinition {
            manifest: manifest("retry-leaf"),
            contract: crate::ToolContract::default(),
        },
    ]));
    let dispatch = Arc::new(
        rebind_child_dispatch(
            &lent,
            &request,
            controller,
            spec(3),
            &ToolUsageLedger::new(),
        )
        .expect("the lent client's test service binds to any recorded authority"),
    );
    let wait = child_turn_cancel_wait(
        &dispatch,
        &request,
        &tokio_util::sync::CancellationToken::new(),
    );
    assert!(
        wait.process_turn_cancellation().is_none(),
        "a child admitted without a cancellation authority waits unobserved"
    );
    let body_context = child_tool_context(
        &dispatch,
        &request,
        wait,
        crate::tool_dispatch::OrchestratingStartsBuffer::default(),
    );

    let replies = crate::OrchestrationContext::new(body_context)
        .call_tool_batch(vec![crate::ToolInvocation::new(
            "nested-1",
            manifest("retry-leaf").id,
            serde_json::json!({}),
        )])
        .await;

    assert_eq!(replies.len(), 1, "the nested call settles");
    assert!(
        replies[0].output.is_success(),
        "the retried call succeeds on its second attempt: {:?}",
        replies[0].output
    );
    let sleeps = recorder.sleeps.lock_recover().clone();
    assert_eq!(sleeps.len(), 1, "exactly one retry sleep is journaled");
    assert_eq!(
        sleeps[0],
        (false, None),
        "the nested retry sleep must not attach the host's turn-cancel gate"
    );
}
