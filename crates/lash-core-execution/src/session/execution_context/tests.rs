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

    async fn execute(&self, _call: ToolCall<'_>) -> ToolOutcome {
        ToolOutcome::err_fmt("not used")
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
    let (event_tx, _event_rx) = tokio::sync::mpsc::channel(1);
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
            crate::NativeRuntimeEffectController::default(),
        )),
        direct_completions: crate::DirectCompletionClient::unavailable(
            "direct completions are unavailable in this test context",
        ),
        parent_invocation: None,
        execution_env_spec: crate::ProcessExecutionEnvSpec::new(
            crate::PluginOptions::default(),
            crate::SessionPolicy::new(crate::TurnBudget::Unbounded),
        ),
        session_id: SessionId::from("session"),
        agent_frame_id: crate::FrameNodeId::new("test-frame").unwrap(),
        event_tx,
        checkpoint_messages: crate::tool_dispatch::CheckpointMessageBuffer::default(),
        trigger_outcomes: crate::tool_dispatch::ToolTriggerOutcomeBuffer::default(),
        attachment_store: Arc::new(crate::SessionAttachmentStore::in_memory()),
        attachment_source_policy: Arc::new(crate::OpenAttachmentSourcePolicy),
        turn_context: crate::TurnContext::default(),
        clock: std::sync::Arc::new(crate::SystemClock),
    });
    let ctx = RuntimeExecutionContext::new(
        SessionId::from("session"),
        dispatch,
        Arc::new(crate::InMemoryProcessExecutionEnvStore::new()),
        Arc::new(crate::SessionAttachmentStore::in_memory()),
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
    test_execution_context_with_env_store(Arc::new(crate::InMemoryProcessExecutionEnvStore::new()))
}

fn test_execution_context_with_env_store(
    env_store: Arc<dyn crate::ProcessExecutionEnvStore>,
) -> RuntimeExecutionContext<'static> {
    let plugins = crate::plugin::PluginHost::empty()
        .build_session("session")
        .expect("plugin session");
    let (event_tx, _event_rx) = tokio::sync::mpsc::channel(1);
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
            crate::NativeRuntimeEffectController::default(),
        )),
        direct_completions: crate::DirectCompletionClient::unavailable(
            "direct completions are unavailable in this test context",
        ),
        parent_invocation: None,
        execution_env_spec: crate::ProcessExecutionEnvSpec::new(
            crate::PluginOptions::default(),
            crate::SessionPolicy::new(crate::TurnBudget::Unbounded),
        ),
        session_id: SessionId::from("session"),
        agent_frame_id: crate::FrameNodeId::new("test-frame").unwrap(),
        event_tx,
        checkpoint_messages: crate::tool_dispatch::CheckpointMessageBuffer::default(),
        trigger_outcomes: crate::tool_dispatch::ToolTriggerOutcomeBuffer::default(),
        attachment_store: Arc::new(crate::SessionAttachmentStore::in_memory()),
        attachment_source_policy: Arc::new(crate::OpenAttachmentSourcePolicy),
        turn_context: crate::TurnContext::default(),
        clock: std::sync::Arc::new(crate::SystemClock),
    });
    RuntimeExecutionContext::new(
        SessionId::from("session"),
        dispatch,
        env_store,
        Arc::new(crate::SessionAttachmentStore::in_memory()),
        Arc::new(crate::ChronologicalProjection::default()),
        None,
        crate::TurnContext::default(),
    )
}

/// A process start is staged before the process-start effect is journaled, so a replayed turn
/// repeats the staging publish after the first attempt already transferred the environment to
/// the process owner and permanently retired the staging owner. The replay must still reach the
/// journaled start command, so the retired staging owner resolves to the same content-addressed
/// reference instead of failing the start.
#[tokio::test]
async fn process_start_staging_survives_a_retired_staging_owner_on_replay() {
    use crate::ProcessExecutionEnvStore;

    let env_store = Arc::new(crate::InMemoryProcessExecutionEnvStore::new());
    let context = test_execution_context_with_env_store(env_store.clone());
    let registration = crate::ProcessRegistration::new(
        "replayed-process",
        crate::ProcessInput::Engine {
            kind: "test-engine".to_string(),
            payload: serde_json::json!({"program": "probe"}),
        },
        crate::RecoveryContract::Rerunnable,
        crate::ProcessProvenance::host(),
        crate::ProcessLifecyclePolicy::new(crate::ParentScope::Host, crate::OnParentEnd::Abandon),
    );
    let staging_owner = crate::ArtifactOwner::process_start(&registration.id);

    let first = context
        .attach_captured_process_execution_env(registration.clone())
        .await
        .expect("first attempt stages the execution environment");
    let staged_ref = first.env_ref.clone().expect("staged env ref");

    // The process-start effect transfers the staged artifact and fences the staging owner.
    env_store
        .transfer_process_execution_env(
            &staging_owner,
            &crate::ArtifactOwner::process(crate::ProcessRef::new(
                registration.id.clone(),
                crate::ProcessIncarnation::from_registration_sequence(1),
            )),
            &staged_ref,
        )
        .await
        .expect("transfer to the process owner");
    env_store
        .retire_process_execution_env_owner(&staging_owner)
        .await
        .expect("retire the staging owner");

    let replayed = context
        .attach_captured_process_execution_env(registration)
        .await
        .expect("replay still reaches the journaled process start");
    assert_eq!(replayed.env_ref, Some(staged_ref));
}

/// The replay tolerance is scoped to process-start staging. A durable owner (trigger
/// registration publishes under the execution's own artifact owner and then persists the
/// reference) must still fail at publish time once that owner is fenced, rather than record a
/// reference to bytes the retirement reclaimed.
#[tokio::test]
async fn a_retired_durable_owner_still_fails_the_public_env_ref_publish() {
    use crate::ProcessExecutionEnvStore;

    let env_store = Arc::new(crate::InMemoryProcessExecutionEnvStore::new());
    let context = test_execution_context_with_env_store(env_store.clone());
    let owner =
        crate::ArtifactOwner::Execution(crate::ExecutionScope::runtime_operation("durable-owner"));

    context
        .captured_process_execution_env_ref(&owner)
        .await
        .expect("first publish under a live owner");

    env_store
        .retire_process_execution_env_owner(&owner)
        .await
        .expect("retire the durable owner");

    let error = context
        .captured_process_execution_env_ref(&owner)
        .await
        .expect_err("a fenced durable owner must not resolve to a reclaimed reference");
    assert!(
        crate::artifact_owner_is_permanently_retired(&error),
        "unexpected error: {error}"
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
        process_id: ProcessId::from("host-process"),
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
        process_id: ProcessId::from("session-process"),
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
        .append_process_event(crate::ProcessEventAppendRequest::new(
            "test.event",
            serde_json::json!({}),
        ))
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
            &ProcessId::from("proc-1"),
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
