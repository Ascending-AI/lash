//! The rebind checklist, one negative test per line.
//!
//! Every case here makes the **lent** opener context deliberately wrong and
//! asserts the child followed its recorded request instead. A child running
//! under its opener's authority instead of its own recorded authority is the
//! failure [`rebind_child_dispatch`] exists to make impossible, and a test that
//! only asserted the happy path would pass just as well against a rebind that
//! forgot a field.

use crate::plugin::PluginSessionRequest;
use std::sync::Arc;

use lash_sansio::sync::MutexExt;

use super::*;
use crate::runtime::{ToolChildAdmission, ToolChildCompletionRouting, ToolChildScope};
use crate::tool_dispatch::ToolAttemptLineage;
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

pub(super) fn spec(turns: usize) -> ProcessExecutionEnvSpec {
    ProcessExecutionEnvSpec::new(
        crate::AdmittedPluginConfig::default(),
        crate::SessionPolicy::new(
            crate::TurnBudget::bounded(turns),
            crate::MaxToolCalls::new(1024),
        ),
    )
}

pub(super) fn manifest(id: &str) -> ToolManifest {
    let mut manifest = crate::ToolDefinition::raw(
        id,
        id,
        "a rebind fixture tool",
        crate::ToolDefinition::default_input_schema(),
        serde_json::json!({ "type": "object", "additionalProperties": true }),
    )
    .expect("valid declared tool schemas")
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
pub(super) fn request() -> ToolChildRequest {
    request_with_identity(ToolAttemptLineage::from_parent(Some(invocation(
        "recorded-parent",
    ))))
}

fn request_with_identity(identity: ToolAttemptLineage) -> ToolChildRequest {
    ToolChildRequest::new(
        PreparedToolCall {
            call_id: crate::ToolCallId::fixture("call-1"),
            provider_call_id: None,
            tool_id: ToolId::from("search"),
            tool_name: "search".into(),
            args: serde_json::json!({ "q": "lash" }),
            replay: None,
            prepared_payload: serde_json::Value::Null,
        },
        ToolChildAdmission::Catalog {
            owner: crate::plugin::PluginRevision::new("mock", crate::plugin::BehaviorRevision::ONE),
            manifest: Box::new(manifest("search")),
        },
        identity,
        ToolChildScope {
            opener: crate::EffectOpener::turn("child-session", "turn"),
            owner: crate::ExecutionOwner::SessionFrame {
                session_id: SessionId::from("child-session"),
                agent_frame_id: FrameNodeId::new("child-frame").expect("a valid frame id"),
            },
        },
        crate::TurnControlBindingId::new("recorded-authority").expect("a valid binding id"),
        ProcessExecutionEnvRef::new("env-ref"),
        ToolChildCompletionRouting::Inline,
        crate::runtime::effect::ToolChildSessionFacts {
            tool_surface: vec![crate::ToolDefinition {
                manifest: manifest("recorded-tool"),
                contract: crate::ToolContract::default(),
            }],
            ..Default::default()
        },
    )
}

/// The opener's own context, every recorded field set to something the child
/// must not inherit.
pub(super) fn lent() -> ToolDispatchContext<'static> {
    lent_with_direct_completions(crate::DirectCompletionClient::unavailable(
        "direct completions are unavailable in this test context",
    ))
}

fn lent_with_direct_completions(
    direct_completions: crate::DirectCompletionClient<'static>,
) -> ToolDispatchContext<'static> {
    let mut other_tool = manifest("opener-tool");
    other_tool.retry_policy = ToolRetryPolicy::Never;
    ToolDispatchContext {
        tool_receipts: None,
        plugins: crate::plugin::PluginHost::empty()
            .build_session(PluginSessionRequest::creation(
                "opener-session",
                Default::default(),
            ))
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
        process_engines: crate::ProcessEngineRegistry::default(),
        effect_controller: crate::runtime::ScopedEffectController::shared(
            Arc::new(crate::testing::UnavailableEffectController),
            crate::AdmittedScope::runtime_operation("test-runtime-effect-controller"),
        )
        .expect("valid test runtime scope"),
        direct_completions,
        parent_invocation: Some(invocation("opener-parent")),
        observation_call_key: None,
        execution_env_spec: spec(9),
        owner: crate::ExecutionOwner::SessionFrame {
            session_id: SessionId::from("opener-session"),
            agent_frame_id: FrameNodeId::new("opener-frame").expect("a valid frame id"),
        },
        observer: Arc::new(crate::engine::NullObservationSink),
        checkpoint_messages: crate::tool_dispatch::CheckpointMessageBuffer::default(),
        trigger_outcomes: crate::tool_dispatch::ToolTriggerOutcomeBuffer::default(),
        attachment_store: Arc::new(crate::RuntimeAttachmentStore::unavailable()),
        attachment_source_policy: Arc::new(crate::OpenAttachmentSourcePolicy),
        turn_context: crate::TurnContext::default(),
        clock: Arc::new(crate::SystemClock),
        process_lineage: None,
        process_originator: None,
    }
}

/// The child's own admitted controller, bound to the child's claim scope and
/// not the opener's.
pub(super) fn child_controller() -> ScopedEffectController<'static> {
    ScopedEffectController::shared(
        Arc::new(crate::testing::UnavailableEffectController),
        crate::AdmittedScope::turn("child-session", "turn"),
    )
    .expect("a valid child scope")
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
    let child = rebind_child_dispatch(&lent, &request(), child_controller(), spec(3))
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
        origin: None,
        parts: vec![crate::Part::text(
            String::new(),
            "the opener's".to_string(),
            None,
        )],
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
    let child = rebind_child_dispatch(&lent, &request(), child_controller(), spec(3))
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

/// A `DirectCompletionService` probe on the real `Runtime` completion
/// source, so a rebind exercises `DirectCompletionService::bind_tool_child`
/// the way a managed-LLM transport's would — a test-fn source would bypass
/// it entirely. Everything the client must rebind is captured where the
/// service receives it: the recorded session and environment at bind, and
/// the admitted controller's scope, the recorded turn, and whether an attempt fault latch was bound at
/// call.
#[derive(Default)]
struct CompletionProbe {
    binds: std::sync::Mutex<Vec<(crate::RuntimeOwner, ProcessExecutionEnvSpec)>>,
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
        effect_attempt: Option<&crate::EffectAttempt>,
    ) -> Result<crate::DirectCompletion, crate::PluginError> {
        self.probe.completes.lock_recover().push((
            effect_controller.execution_scope().clone(),
            turn_id.cloned(),
            effect_attempt.is_some(),
        ));
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
        _effect_attempt: Option<&crate::EffectAttempt>,
    ) -> Result<crate::DirectLlmCompletion, crate::PluginError> {
        Err(crate::PluginError::Session(
            "the rebind probe answers text completions only".to_string(),
        ))
    }

    fn bind_tool_child(
        self: Arc<Self>,
        owner: &crate::RuntimeOwner,
        execution_env_spec: &crate::ProcessExecutionEnvSpec,
    ) -> Option<Arc<dyn crate::direct_completion_client::DirectCompletionService>> {
        self.probe
            .binds
            .lock_recover()
            .push((owner.clone(), execution_env_spec.clone()));
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
            usage_disposition: crate::AttemptUsageOutcome::default(),
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
        crate::runtime::ScopedEffectController::shared(
            Arc::new(crate::testing::UnavailableEffectController),
            crate::AdmittedScope::runtime_operation("test-runtime-effect-controller"),
        )
        .expect("valid test runtime scope"),
        Some(crate::TurnId::from("opener-turn")),
    ))
}

/// §3's managed-LLM line of the checklist: the lent client is rebound to the
/// child's recorded authority, so the service is asked to bind the recorded
/// session and environment — and the call it then receives arrives on the
/// child's admitted controller under the recorded turn, never the opener's
/// current facts.
#[tokio::test]
async fn the_lent_completion_client_is_rebound_to_the_recorded_authority() {
    let probe = Arc::new(CompletionProbe::default());
    let lent = probed_lent(&probe, true);
    // A recorded parent that carries the child's turn, so the rebound
    // client's attribution observably comes from the journal and not from
    // the opener's minted `opener-turn`.
    let request = request_with_identity(ToolAttemptLineage::from_parent(Some(
        crate::RuntimeInvocation::effect(
            crate::EffectAddress::new(
                ExecutionScope::turn("child-session", "turn"),
                "recorded-parent",
            )
            .expect("a valid effect address"),
            crate::RuntimeAttribution::for_turn("child-session", "child-turn", 0, 0),
            "recorded-parent",
        ),
    )));
    let child = rebind_child_dispatch(&lent, &request, child_controller(), spec(3))
        .expect("the probe service binds to the recorded authority");

    {
        let binds = probe.binds.lock_recover();
        assert_eq!(
            binds.as_slice(),
            &[(
                crate::RuntimeOwner::Session(SessionId::from("child-session")),
                spec(3)
            )],
            "the service binds the recorded session and environment"
        );
    }

    let completion = child
        .direct_completions
        .direct_completion(crate::DirectRequest::text("a managed call"), "law-source")
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
                false
            )],
            "the call arrived on the child's admitted controller under the \
             recorded turn; the rebind binds no attempt fault latch, which only the \
             child's `ToolAttempt` body supplies (ADR 0127)"
        );
    }

    // The opener's own client is untouched: a call on it still arrives under
    // the turn it was minted with — the rebind cloned, it did not mutate.
    lent.direct_completions
        .direct_completion(crate::DirectRequest::text("the opener's"), "law")
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
    let error = rebind_child_dispatch(&lent, &request(), child_controller(), spec(3))
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

/// Records the turn-cancel shape of every journaled sleep, the way the
/// retry-gate suite's recorder does: the observable a signalled host gate
/// would act on is `observe_turn_cancel`, so asserting the shape is asserting
/// the gate was never attached.
/// A process-execution-env store that fails every read with `error`.
struct FailingEnvStore(fn() -> Result<Option<Vec<u8>>, crate::ArtifactStoreError>);

#[async_trait::async_trait]
impl crate::ProcessExecutionEnvStore for FailingEnvStore {
    async fn publish_process_execution_env(
        &self,
        _claim: &crate::ReferrerClaim,
        _env_ref: &ProcessExecutionEnvRef,
        _bytes: &[u8],
    ) -> Result<(), crate::ArtifactStoreError> {
        Ok(())
    }

    async fn acquire_process_execution_env(
        &self,
        _claim: &crate::ReferrerClaim,
        _env_ref: &ProcessExecutionEnvRef,
    ) -> Result<(), crate::ArtifactStoreError> {
        Ok(())
    }

    async fn end_process_env_referrer(
        &self,
        _cleanup: &crate::ResolvedArtifactCleanup,
    ) -> Result<(), crate::ArtifactStoreError> {
        Ok(())
    }

    async fn get_process_execution_env(
        &self,
        _env_ref: &ProcessExecutionEnvRef,
    ) -> Result<Option<Vec<u8>>, crate::ArtifactStoreError> {
        (self.0)()
    }
}

/// How a child whose recorded environment does not load settles, per cause
/// (FIG-3575): a store that did not answer is the attempt's live fault, and
/// an environment the store holds but this build cannot use is the request's
/// outcome. Only the live fault carries the retry authority that keeps it out
/// of the load step's journal (FIG-3683).
#[tokio::test]
async fn an_unresolved_environment_settles_by_whose_fact_it_is() {
    let request = request();
    async fn settle(
        request: &ToolChildRequest,
        store: Arc<dyn crate::ProcessExecutionEnvStore>,
    ) -> RuntimeEffectControllerError {
        let address = crate::EffectAddress::new(
            request.scope.claim_scope().into_scope(),
            format!("{}:env", request.call.call_id),
        )
        .expect("a valid load address");
        RuntimeEffectLocalExecutor::execution_env_load(store, "tool child")
            .execute(RuntimeEffectEnvelope::new(
                crate::RuntimeEffectInvocation::new(
                    address,
                    crate::RuntimeAttribution::none(),
                    "load",
                ),
                RuntimeEffectCommand::LoadExecutionEnv {
                    env: request.execution_env.clone(),
                },
            ))
            .await
            .expect_err("the environment does not load")
    }
    let retried = |error: &RuntimeEffectControllerError| {
        error
            .journal_disposition(crate::RuntimeEffectKind::LoadExecutionEnv)
            .is_retryable_derivation()
    };

    // Store I/O: a store that did not answer is a live fault.
    let timed_out = settle(
        &request,
        Arc::new(FailingEnvStore(|| {
            Err(crate::ArtifactStoreError::Backend(
                "pool timed out while waiting for an open connection".into(),
            ))
        })),
    )
    .await;
    assert_eq!(
        timed_out.turn_failure_cause(),
        crate::TurnFailureCause::LiveFault,
        "a store that did not answer is this attempt's fault: {timed_out}"
    );
    assert!(timed_out.message.contains("pool timed out"));
    assert!(
        retried(&timed_out),
        "a live fault is never the load's record"
    );

    // A typed store refusal is the request's outcome: bytes no referrer
    // holds any more are absent on every redrive, so the load records it.
    let missing_bytes = settle(
        &request,
        Arc::new(FailingEnvStore(|| {
            Err(crate::ArtifactStoreError::ArtifactMissing {
                artifact_ref: "env".into(),
            })
        })),
    )
    .await;
    assert_eq!(
        missing_bytes.code,
        crate::RuntimeErrorCode::RuntimeEffectToolChildRequestVersion
    );
    assert!(missing_bytes.message.contains("`env` is not stored"));
    assert_eq!(
        missing_bytes.turn_failure_cause(),
        crate::TurnFailureCause::Outcome
    );
    assert!(!retried(&missing_bytes), "a typed refusal is recorded");

    // A refusal the store answers with is the request's outcome.
    let invalid = settle(
        &request,
        Arc::new(FailingEnvStore(|| {
            Err(crate::ArtifactStoreError::StoredDataCorrupt {
                source: crate::ModuleArtifactCorruption::InvalidReference {
                    record_kind: "process execution environment".into(),
                    reference: "invalid".into(),
                },
            })
        })),
    )
    .await;
    assert_eq!(invalid.code, crate::RuntimeErrorCode::RuntimeStoreCorrupt);
    assert_eq!(
        invalid.turn_failure_cause(),
        crate::TurnFailureCause::Outcome
    );

    // Nothing stored under the reference: the request's outcome.
    let missing = settle(&request, Arc::new(FailingEnvStore(|| Ok(None)))).await;
    assert!(!retried(&missing), "a missing environment is recorded");
    assert_eq!(
        missing.code,
        crate::RuntimeErrorCode::RuntimeEffectToolChildRequestVersion
    );
    assert_eq!(
        missing.turn_failure_cause(),
        crate::TurnFailureCause::Outcome
    );
    assert!(missing.message.contains("missing process execution env"));

    let unavailable = settle(
        &request,
        Arc::new(crate::testing::UnavailableProcessExecutionEnvStore),
    )
    .await;
    assert_eq!(
        unavailable.turn_failure_cause(),
        crate::TurnFailureCause::LiveFault
    );
    assert!(
        retried(&unavailable),
        "a failed acquisition is retried before journaling"
    );
}
