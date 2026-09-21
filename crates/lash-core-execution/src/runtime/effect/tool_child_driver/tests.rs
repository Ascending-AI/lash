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
    )
    .expect("the lent client's test service binds to any recorded authority");
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

    let live = LiveOpenerContext::capture(&lent(), tokio_util::sync::CancellationToken::new())
        .expect("a shared controller lends a context");
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

struct ProbedCompletionService {
    probe: Arc<CompletionProbe>,
    /// What `bind_tool_child` answers. `false` models a service that cannot
    /// prove it executes under the recorded authority — the production
    /// transport's answer to a foreign session.
    bindable: bool,
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
        // does — a billed provider attempt is a usage fact of the call.
        if let Some(sink) = usage_sink {
            sink.record(&probed_call_record());
        }
        Ok(probed_completion())
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

fn probed_lent(probe: &Arc<CompletionProbe>, bindable: bool) -> ToolDispatchContext<'static> {
    lent_with_direct_completions(crate::DirectCompletionClient::runtime(
        Arc::new(ProbedCompletionService {
            probe: Arc::clone(probe),
            bindable,
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
    let lent = probed_lent(&probe, true);
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
    let lent = probed_lent(&probe, false);
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
