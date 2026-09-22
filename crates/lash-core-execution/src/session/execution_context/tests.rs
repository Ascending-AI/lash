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

/// The session path publishes nothing before the process-start effect is journaled.
///
/// This is the FIG-3028 / #1390 regression, re-pointed at the journaled publish (FIG-3050).
/// #1390 kept the pre-journal staging publish and taught it to tolerate the permanently retired
/// staging owner a replay revisits; the spec now travels in the command instead, so there is no
/// pre-journal artifact and no owner to revisit. The journaled publish keeps the tolerance, and
/// `process_start_transfers_environment_and_replays_after_staging_retirement`
/// (`runtime::effect::executor::process_local`) exercises it there.
#[tokio::test]
async fn a_session_path_process_start_publishes_no_environment_before_its_journal() {
    use crate::ProcessExecutionEnvStore;

    let env_store = Arc::new(crate::InMemoryProcessExecutionEnvStore::new());
    let context = test_execution_context_with_env_store(env_store.clone());
    let registration = crate::ProcessRegistration::new(
        "journaled-process",
        crate::ProcessInput::Engine {
            kind: "test-engine".to_string(),
            payload: serde_json::json!({"program": "probe"}),
        },
        crate::RecoveryContract::Rerunnable,
        crate::ProcessProvenance::host(),
        crate::ProcessLifecyclePolicy::new(crate::ParentScope::Host, crate::OnParentEnd::Abandon),
    );

    let (prepared, env_spec) = context.process_start_execution_env(registration);
    assert_eq!(
        prepared.env_ref, None,
        "a session-path start must not carry a reference its journal has not produced"
    );
    let env_spec = env_spec.expect("the captured spec rides the process-start command");
    let staged_ref = env_spec.stable_ref().expect("stable environment reference");
    assert_eq!(
        env_store
            .get_process_execution_env(&staged_ref)
            .await
            .expect("read the environment store"),
        None,
        "nothing is published before the process-start effect runs"
    );
}

/// A start made inside a process execution reuses the reference its own registration records.
///
/// Those bytes are already published under the parent's durable owner, so the child stages
/// nothing and hands the executor the recorded reference rather than a fresh spec.
#[tokio::test]
async fn a_start_inside_a_process_execution_inherits_the_recorded_env_ref() {
    let env_store = Arc::new(crate::InMemoryProcessExecutionEnvStore::new());
    let inherited = crate::ProcessExecutionEnvRef::new("process-env:inherited");
    let parent = crate::ProcessRegistration::new(
        "parent-process",
        crate::ProcessInput::Engine {
            kind: "test-engine".to_string(),
            payload: serde_json::json!({"program": "parent"}),
        },
        crate::RecoveryContract::Rerunnable,
        crate::ProcessProvenance::host(),
        crate::ProcessLifecyclePolicy::new(crate::ParentScope::Host, crate::OnParentEnd::Abandon),
    )
    .with_execution_env_ref(Some(inherited.clone()));
    let context =
        test_execution_context_with_env_store(env_store).with_process_execution(&parent, None);

    let child = crate::ProcessRegistration::new(
        "child-process",
        crate::ProcessInput::Engine {
            kind: "test-engine".to_string(),
            payload: serde_json::json!({"program": "child"}),
        },
        crate::RecoveryContract::Rerunnable,
        crate::ProcessProvenance::host(),
        crate::ProcessLifecyclePolicy::new(crate::ParentScope::Host, crate::OnParentEnd::Abandon),
    );
    let (prepared, env_spec) = context.process_start_execution_env(child);
    assert_eq!(prepared.env_ref, Some(inherited));
    assert!(
        env_spec.is_none(),
        "an inherited environment is already durable; the command carries no spec"
    );
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

// ---------------------------------------------------------------------------
// FIG-3417: the lifecycle parent a child start declares comes from ONE shared
// derivation — the admitted execution scope, which for a process already
// carries the incarnation the admission authority bound. Nothing on this path
// re-resolves the reusable process name against the registry.
// ---------------------------------------------------------------------------

fn registration_for_parent_scope(process_id: &str) -> crate::ProcessRegistration {
    crate::ProcessRegistration::new(
        ProcessId::from(process_id),
        crate::ProcessInput::External {
            metadata: serde_json::Value::Null,
        },
        crate::RecoveryContract::ExternallyOwned,
        crate::ProcessProvenance::host(),
        crate::ProcessLifecyclePolicy::new(crate::ParentScope::Host, crate::OnParentEnd::Abandon),
    )
}

fn scoped_context(
    session_id: &str,
    admitted: crate::AdmittedScope,
) -> RuntimeExecutionContext<'static> {
    let controller = crate::ScopedEffectController::shared(
        Arc::new(crate::NativeRuntimeEffectController::default()),
        admitted,
    )
    .expect("the test scope validates");
    crate::testing::TestExecutionContextBuilder::new()
        .session_id(session_id)
        .borrowed_effect_controller(controller)
        .plugin_factories(vec![])
        .build()
        .into_runtime()
}

fn process_event_context(
    process_id: &ProcessId,
    registry: Arc<dyn crate::ProcessRegistry>,
) -> RuntimeExecutionProcessEventContext {
    RuntimeExecutionProcessEventContext {
        execution_write_authority: crate::ProcessExecutionWriteAuthority::invocation(
            process_id.clone(),
            "test-write-authority",
        ),
        process_work: crate::testing::process_work_wiring_for_registry(registry),
        store: None,
        session_store_factory: None,
        queued_work: Arc::new(crate::NoQueuedWork::new()),
        process_wake_delivery_policy: crate::DeliveryPolicy::EarliestSafeBoundary,
        clock: Arc::new(crate::SystemClock),
    }
}

/// A child a turn starts takes the turn as its lifecycle parent.
#[tokio::test]
async fn a_child_started_from_a_turn_parents_on_the_turn() {
    let context = scoped_context(
        "session-1",
        crate::AdmittedScope::turn("session-1", "turn-7"),
    );
    assert_eq!(
        context
            .child_process_parent_scope()
            .expect("a turn scope derives a turn parent"),
        crate::ParentScope::turn(SessionId::from("session-1"), crate::TurnId::from("turn-7")),
    );
}

/// A process scope nobody bound an admitted incarnation to cannot be built —
/// `AdmittedScope` refuses the unpinned pair at construction, so no execution
/// context can ever carry the reusable name as a fallback. The registry in
/// this fixture *could* resolve the name, which is what makes the construction
/// refusal prove the derivation never asks it.
#[tokio::test]
async fn a_process_scope_without_an_admitted_incarnation_is_unconstructible() {
    let registry: Arc<dyn crate::ProcessRegistry> =
        Arc::new(crate::TestLocalProcessRegistry::default());
    registry
        .register_process(registration_for_parent_scope("worker"))
        .await
        .expect("first registration");
    assert!(
        matches!(
            crate::AdmittedScope::new(crate::ExecutionScope::process("worker"), None),
            Err(crate::AdmittedScopeError::ProcessIncarnationMissing { .. })
        ),
        "the reusable name alone is never admitted"
    );
}

/// A same-name successor already retained in the registry does not rebind the
/// pinned parent: a child started by incarnation 1 of `worker` parents on
/// incarnation 1 even though the registry now holds incarnation 2.
#[tokio::test]
async fn a_child_started_from_a_process_incarnation_keeps_the_pinned_parent() {
    let registry: Arc<dyn crate::ProcessRegistry> =
        Arc::new(crate::TestLocalProcessRegistry::default());
    let retired = registry
        .register_process(registration_for_parent_scope("worker"))
        .await
        .expect("first registration");
    registry
        .complete_process(
            &retired.id,
            crate::ProcessAwaitOutput::from_tool_output(crate::ToolCallOutput::success(
                serde_json::json!("old"),
            )),
            crate::ProcessCompletionAuthority::external_owner(),
        )
        .await
        .expect("complete the first incarnation");
    registry
        .prune_terminal_processes(u64::MAX, None, crate::ProjectionWatermark::NoProjector)
        .await
        .expect("prune the retired incarnation");
    let successor = registry
        .register_process(registration_for_parent_scope("worker"))
        .await
        .expect("same-name successor registration");
    assert_ne!(
        successor.incarnation, retired.incarnation,
        "the fixture must hold a successor incarnation under the same name"
    );
    // Recovery validates a retained pair with get_process_ref — and the
    // superseded incarnation is refused there, not rebound.
    assert!(
        registry
            .get_process_ref(&crate::ProcessRef::new(
                retired.id.clone(),
                retired.incarnation,
            ))
            .await
            .is_err(),
        "get_process_ref must refuse the superseded incarnation"
    );

    let context = scoped_context(
        "session-1",
        crate::AdmittedScope::process(crate::ProcessRef::new(
            retired.id.clone(),
            retired.incarnation,
        )),
    )
    .with_process_execution(
        &registration_for_parent_scope("worker"),
        Some(process_event_context(&retired.id, Arc::clone(&registry))),
    );
    assert_eq!(
        context
            .child_process_parent_scope()
            .expect("the pinned incarnation is the parent"),
        crate::ParentScope::process(crate::ProcessRef::from_record(&retired)),
    );
}

#[test]
fn native_authority_clears_turn_invocation_correlation() {
    let process_id = crate::ProcessId::from("native-process");
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
    });
    super::attach_process_invocation_correlation(&mut turn_context, &process_id, &native_authority);
    let context = crate::testing::TestExecutionContextBuilder::new()
        .turn_context(turn_context)
        .plugin_factories(vec![])
        .build()
        .into_runtime();

    assert_eq!(context.restate_invocation_id(), None);
}

/// A queued-work drain admits a host lifecycle parent until FIG-3419 lands the
/// drain-end protocol that lets a drain own durable children — the derivation
/// must not silently borrow the session's current turn.
#[tokio::test]
async fn a_child_started_from_a_queued_drain_parents_on_the_host() {
    let context = scoped_context(
        "session-1",
        crate::AdmittedScope::queue_drain("session-1", "drain-3"),
    );
    assert_eq!(
        context
            .child_process_parent_scope()
            .expect("a queued drain admits a host parent until FIG-3419"),
        crate::ParentScope::Host,
    );
}
