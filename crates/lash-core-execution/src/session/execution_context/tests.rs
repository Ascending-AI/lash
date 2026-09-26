use super::*;
use crate::tool_dispatch::ToolDispatchContext;
use crate::{ToolCall, ToolOutcome, ToolProvider};

struct NoopTools;

#[test]
fn trigger_owner_scope_uses_root_session_or_explicit_host_binding() {
    assert_eq!(
        resolve_trigger_owner_scope(&SessionId::from("root-session"), None).unwrap(),
        crate::TriggerOwnerScope::session("root-session")
    );
    let root = crate::ProcessOriginator::session(crate::SessionScope::new("root-session"));
    assert_eq!(
        resolve_trigger_owner_scope(&SessionId::from("ignored"), Some(&root)).unwrap(),
        crate::TriggerOwnerScope::session("root-session")
    );
    let frame = crate::ProcessOriginator::session(crate::SessionScope::for_agent_frame(
        "root-session",
        crate::facade_support::frame_node_id(&SessionId::from("root-session"), "agent-frame"),
    ));
    assert_eq!(
        resolve_trigger_owner_scope(&SessionId::from("ignored"), Some(&frame)).unwrap(),
        crate::TriggerOwnerScope::session("root-session"),
        "agent frames inherit the root session namespace"
    );
    let named_host = crate::ProcessOriginator::host_scoped("automation-a");
    assert_eq!(
        resolve_trigger_owner_scope(&SessionId::from("ignored"), Some(&named_host)).unwrap(),
        crate::TriggerOwnerScope::host("automation-a").unwrap()
    );
    assert!(
        resolve_trigger_owner_scope(
            &SessionId::from("ignored"),
            Some(&crate::ProcessOriginator::host())
        )
        .unwrap_err()
        .to_string()
        .contains("bare host authority")
    );
}

#[async_trait::async_trait]
impl ToolProvider for NoopTools {
    fn tool_manifests(&self) -> Vec<crate::ToolManifest> {
        Vec::new()
    }

    fn resolve_contract(&self, _name: &str) -> Option<Arc<crate::ToolContract>> {
        None
    }

    async fn execute(&self, _call: ToolCall<'_>) -> crate::ToolAttemptOutcome {
        ToolOutcome::err_fmt("not used").into()
    }
}

#[test]
fn tool_argument_projection_policy_resolves_from_active_catalog_and_defaults_unknown() {
    let tool = crate::ToolDefinition::raw(
        "tool:seedy",
        "seedy",
        "Seed-aware",
        crate::ToolDefinition::default_input_schema(),
        serde_json::json!({ "type": "string" }),
    )
    .with_argument_projection(
        crate::ToolArgumentProjectionPolicy::preserve_projected_refs_in_field("seed"),
    );
    let plugins = crate::plugin::PluginHost::empty()
        .build_session("session")
        .expect("plugin session");
    let dispatch = Arc::new(ToolDispatchContext {
        plugins,
        tools: Arc::new(NoopTools),
        tool_registry: None,
        tool_catalog: Arc::new(crate::ToolCatalog::from_tool_definitions(vec![tool])),
        sessions: Arc::new(crate::testing::MockSessionManager::default()),
        session_lifecycle: Arc::new(crate::testing::MockSessionManager::default()),
        session_graph: Arc::new(crate::testing::MockSessionManager::default()),
        processes: Arc::new(crate::UnavailableProcessService),
        trigger_router: None,
        process_definitions: None,
        process_engines: crate::ProcessEngineRegistry::default(),
        effect_controller: crate::runtime::RuntimeEffectControllerHandle::shared(Arc::new(
            crate::testing::UnavailableEffectController,
        )),
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
        observer: std::sync::Arc::new(crate::engine::NullObservationSink),
        checkpoint_messages: crate::tool_dispatch::CheckpointMessageBuffer::default(),
        trigger_outcomes: crate::tool_dispatch::ToolTriggerOutcomeBuffer::default(),
        attachment_store: Arc::new(crate::SessionAttachmentStore::unavailable()),
        attachment_source_policy: Arc::new(crate::OpenAttachmentSourcePolicy),
        turn_context: crate::TurnContext::default(),
        clock: std::sync::Arc::new(crate::SystemClock),
        process_lineage: None,
    });
    let ctx = RuntimeExecutionContext::new(
        SessionId::from("session"),
        dispatch,
        Arc::new(crate::testing::UnavailableProcessExecutionEnvStore),
        Arc::new(crate::SessionAttachmentStore::unavailable()),
        Arc::new(crate::ChronologicalProjection::default()),
        None,
        crate::TurnContext::default(),
    );

    assert_eq!(
        ctx.tool_argument_projection_policy("seedy"),
        crate::ToolArgumentProjectionPolicy::preserve_projected_refs_in_field("seed")
    );
    assert_eq!(
        ctx.tool_argument_projection_policy("missing"),
        crate::ToolArgumentProjectionPolicy::MaterializeProjectedValues
    );
}

fn test_execution_context() -> RuntimeExecutionContext<'static> {
    test_execution_context_with_env_store(Arc::new(
        crate::testing::UnavailableProcessExecutionEnvStore,
    ))
}

fn test_execution_context_with_env_store(
    env_store: Arc<dyn crate::ProcessExecutionEnvStore>,
) -> RuntimeExecutionContext<'static> {
    let plugins = crate::plugin::PluginHost::empty()
        .build_session("session")
        .expect("plugin session");
    let dispatch = Arc::new(ToolDispatchContext {
        plugins,
        tools: Arc::new(NoopTools),
        tool_registry: None,
        tool_catalog: Arc::new(crate::ToolCatalog::from_tool_definitions(Vec::new())),
        sessions: Arc::new(crate::testing::MockSessionManager::default()),
        session_lifecycle: Arc::new(crate::testing::MockSessionManager::default()),
        session_graph: Arc::new(crate::testing::MockSessionManager::default()),
        processes: Arc::new(crate::UnavailableProcessService),
        trigger_router: None,
        process_definitions: None,
        process_engines: crate::ProcessEngineRegistry::default(),
        effect_controller: crate::runtime::RuntimeEffectControllerHandle::shared(Arc::new(
            crate::testing::UnavailableEffectController,
        )),
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
        observer: std::sync::Arc::new(crate::engine::NullObservationSink),
        checkpoint_messages: crate::tool_dispatch::CheckpointMessageBuffer::default(),
        trigger_outcomes: crate::tool_dispatch::ToolTriggerOutcomeBuffer::default(),
        attachment_store: Arc::new(crate::SessionAttachmentStore::unavailable()),
        attachment_source_policy: Arc::new(crate::OpenAttachmentSourcePolicy),
        turn_context: crate::TurnContext::default(),
        clock: std::sync::Arc::new(crate::SystemClock),
        process_lineage: None,
    });
    RuntimeExecutionContext::new(
        SessionId::from("session"),
        dispatch,
        env_store,
        Arc::new(crate::SessionAttachmentStore::unavailable()),
        Arc::new(crate::ChronologicalProjection::default()),
        None,
        crate::TurnContext::default(),
    )
}

/// A start made inside a process execution reuses the reference its own registration records.
///
/// Those bytes are already published under the parent's durable owner, so the child stages
/// nothing and hands the executor the recorded reference rather than a fresh spec.
#[tokio::test]
async fn a_start_inside_a_process_execution_inherits_the_recorded_env_ref() {
    let env_store = Arc::new(crate::testing::UnavailableProcessExecutionEnvStore);
    let inherited = crate::ProcessExecutionEnvRef::new("process-env:inherited");
    let parent = crate::ProcessRegistration::new(
        crate::ProcessInput::Engine {
            kind: "test-engine".to_string(),
            payload: serde_json::json!({"program": "parent"}),
        },
        crate::RecoveryContract::Rerunnable,
        crate::ProcessProvenance::host(),
        crate::Lifetime::Detached,
    )
    .with_execution_env_ref(Some(inherited.clone()));
    let context = test_execution_context_with_env_store(env_store).with_process_execution(
        crate::ProcessId::fixture("parent"),
        &parent,
        None,
    );

    let child = crate::ProcessRegistration::new(
        crate::ProcessInput::Engine {
            kind: "test-engine".to_string(),
            payload: serde_json::json!({"program": "child"}),
        },
        crate::RecoveryContract::Rerunnable,
        crate::ProcessProvenance::host(),
        crate::Lifetime::Detached,
    );
    let (prepared, env_spec) = context.process_start_execution_env(child);
    assert_eq!(prepared.env_ref, Some(inherited));
    assert!(
        env_spec.is_none(),
        "an inherited environment is already durable; the command carries no spec"
    );
}

#[test]
fn parentless_effect_envelopes_use_process_originator_not_ambient_session() {
    let envelope = |context: &RuntimeExecutionContext<'_>, effect_id: &str| {
        crate::RuntimeEffectEnvelope::new(
            context.language_runtime_invocation(effect_id),
            crate::RuntimeEffectCommand::LanguageRuntimeValue {
                operation: "sample".to_string(),
            },
        )
    };

    let foreground = test_execution_context();
    assert_eq!(
        envelope(&foreground, "foreground").invocation.attribution,
        crate::RuntimeAttribution::for_session("session")
    );

    let mut host_process = test_execution_context();
    host_process.process_execution = Some(RuntimeProcessExecution {
        process_id: crate::process_id_for_test("host-process"),
        originator: crate::ProcessOriginator::host_scoped("automation"),
        env_ref: None,
        wake_session_id: None,
        event_context: None,
    });
    assert_eq!(
        envelope(&host_process, "host").invocation.attribution,
        crate::RuntimeAttribution::none(),
        "ambient current-session capability is descriptive inside a host-owned process"
    );

    let mut session_process = test_execution_context();
    session_process.process_execution = Some(RuntimeProcessExecution {
        process_id: crate::process_id_for_test("session-process"),
        originator: crate::ProcessOriginator::session(crate::SessionScope::new("origin-session")),
        env_ref: None,
        wake_session_id: None,
        event_context: None,
    });
    assert_eq!(
        envelope(&session_process, "session-origin")
            .invocation
            .attribution,
        crate::RuntimeAttribution::for_session("origin-session")
    );
}

#[tokio::test]
async fn execution_context_without_process_execution_returns_typed_error_from_append_and_signal() {
    let ctx = test_execution_context();

    let append_err = ctx
        .append_process_events(vec![crate::ProcessEventAppendRequest::new(
            "test.event",
            serde_json::json!({}),
        )])
        .await
        .unwrap_err();

    let crate::PluginError::RuntimeEffectController(append_effect_err) = append_err else {
        panic!("expected PluginError::RuntimeEffectController, got {append_err:?}");
    };
    assert_eq!(
        append_effect_err.code,
        crate::RuntimeErrorCode::ProcessRegistryUnavailable
    );
    assert_eq!(
        append_effect_err.message,
        "process execution is unavailable outside a durable process execution"
    );

    let signal_err = ctx
        .signal_process_by_id(
            &crate::process_id_for_test("proc-1"),
            "sig-1",
            "sig-id-1".to_string(),
            serde_json::json!({}),
        )
        .await
        .unwrap_err();

    assert_eq!(
        signal_err.code,
        crate::RuntimeErrorCode::ProcessRegistryUnavailable
    );
    assert_eq!(
        signal_err.message,
        "process execution is unavailable outside a durable process execution"
    );
}

// ---------------------------------------------------------------------------
// FIG-3417: the lifecycle parent a child start declares comes from ONE shared
// derivation — the admitted execution scope, which for a process already
// carries the incarnation the admission authority bound. Nothing on this path
// re-resolves the reusable process name against the registry.
// ---------------------------------------------------------------------------

fn scoped_context(
    session_id: &str,
    admitted: crate::AdmittedScope,
) -> RuntimeExecutionContext<'static> {
    let controller = crate::ScopedEffectController::shared(
        Arc::new(crate::testing::UnavailableEffectController),
        admitted,
    )
    .expect("the test scope validates");
    crate::testing::TestExecutionContextBuilder::over_controller(controller)
        .session_id(session_id)
        .plugin_factories(vec![])
        .build()
        .into_runtime()
}

/// A child a turn starts is started by the turn, under the turn's session:
/// the start context names both, nearest first.
#[tokio::test]
async fn a_child_started_from_a_turn_is_started_by_the_turn() {
    let context = scoped_context(
        "session-1",
        crate::AdmittedScope::turn("session-1", "turn-7"),
    );
    let cx = context
        .start_cx()
        .expect("a turn scope materializes a start context");
    let turn = crate::ScopeId::turn(SessionId::from("session-1"), crate::TurnId::from("turn-7"));
    let session = crate::ScopeId::session(SessionId::from("session-1"));
    assert_eq!(cx.starter().id(), &turn);
    assert_eq!(
        cx.session().map(|scope| scope.id().clone()),
        Some(session.clone())
    );
    assert_eq!(cx.ancestry().scopes(), &[turn, session]);
}

#[test]
fn native_authority_retains_attempt_correlation_without_restate_identity() {
    let process_id = crate::process_id_for_test("native-process");
    let authority = crate::ProcessExecutionWriteAuthority::invocation(
        process_id.clone(),
        "foreign-restate-invocation",
    )
    .bind_attempt(1);
    let mut turn_context = crate::TurnContext::default();
    super::attach_process_invocation_correlation(&mut turn_context, &process_id, &authority);
    let native_authority = crate::ProcessExecutionWriteAuthority::lease(crate::ProcessLease {
        schema_version: crate::PROCESS_LEASE_SCHEMA_VERSION,
        process_id: process_id.clone(),
        owner: crate::LeaseOwnerIdentity::opaque("native-worker", "attempt"),
        lease_token: "native-worker-lease".to_string(),
        fencing_token: 1,
        claimed_at_epoch_ms: 0,
        expires_at_epoch_ms: u64::MAX,
    })
    .bind_attempt(3);
    super::attach_process_invocation_correlation(&mut turn_context, &process_id, &native_authority);
    let context = crate::testing::TestExecutionContextBuilder::over_controller(Arc::new(
        crate::testing::UnavailableEffectController,
    )
        as Arc<dyn crate::RuntimeEffectController>)
    .turn_context(turn_context)
    .plugin_factories(vec![])
    .build()
    .into_runtime();

    assert_eq!(context.engine_execution_id(), None);
    assert_eq!(context.admitted_process_attempt(), Some(3));
}

/// A queued-work drain is a durable owner with an end protocol (FIG-3419), so
/// a child it starts is started by the drain itself — the derivation must not
/// silently borrow the session's current turn.
#[tokio::test]
async fn a_child_started_from_a_queued_drain_is_started_by_the_drain() {
    let context = scoped_context(
        "session-1",
        crate::AdmittedScope::queue_drain("session-1", "drain-3"),
    );
    assert_eq!(
        context
            .start_cx()
            .expect("a queued drain materializes a start context")
            .starter()
            .id(),
        &crate::ScopeId::queue_drain("session-1", "drain-3"),
    );
}

/// Records the `(key, ordinal)` every emitted observation lands under: the
/// identity the spec pins (ADR 0105 §1).
#[derive(Default)]
struct ObservationIds(std::sync::Mutex<Vec<String>>);

impl crate::engine::ObservationSink for ObservationIds {
    fn observe(&self, observation: crate::engine::DriveObservation) {
        self.0
            .lock()
            .expect("observation ids")
            .push(format!("{}#{}", observation.key, observation.ordinal));
    }
}

fn emit_started(context: &RuntimeExecutionContext<'_>, material: &str, sink: &ObservationIds) {
    let call = context.with_call_observation_key(context.call_observation_key(material));
    call.observation_cursor("directives:before").observe(
        sink,
        crate::engine::ObservedEvent::Activity {
            correlation_id: None,
            event: crate::TurnEvent::ToolCallStarted {
                call_id: Some(material.to_string()),
                name: "tool".to_string(),
                args: serde_json::json!({}),
                graph_key: None,
                parent_call_id: None,
            },
        },
    );
}

/// Two tool calls in one turn share the dispatch's observation base — on the
/// turn-dispatched protocol path there is no per-call invocation — so their
/// before-directive lanes only stay distinct because each call carries its
/// own qualified key (ADR 0105 §1).
#[test]
fn two_calls_on_one_base_mint_distinct_before_directive_ids() {
    let context = test_execution_context();
    let sink = ObservationIds::default();
    emit_started(&context, "0:0:lookup", &sink);
    emit_started(&context, "0:1:lookup", &sink);
    let ids = sink.0.lock().expect("observation ids").clone();
    assert_eq!(
        ids,
        [
            format!(
                "{}:call:0:0:lookup:directives:before#0",
                context.dispatch.observation_base_key()
            ),
            format!(
                "{}:call:0:1:lookup:directives:before#0",
                context.dispatch.observation_base_key()
            ),
        ],
        "each call's lane keys under its own material; a shared base would mint the same id twice"
    );
}

/// The model may repeat one `call_id` across protocol iterations: the
/// iteration is part of the lane material, so both calls mint distinct ids
/// (ADR 0105 §1).
#[test]
fn a_call_id_repeated_across_iterations_mints_distinct_ids() {
    let context = test_execution_context();
    let sink = ObservationIds::default();
    emit_started(&context, "0:0:dup", &sink);
    emit_started(&context, "1:0:dup", &sink);
    let ids = sink.0.lock().expect("observation ids").clone();
    assert_eq!(ids.len(), 2);
    assert_ne!(ids[0], ids[1], "iterations qualify the reused call id");
    assert!(ids[0].ends_with(":call:0:0:dup:directives:before#0"));
    assert!(ids[1].ends_with(":call:1:0:dup:directives:before#0"));
}
