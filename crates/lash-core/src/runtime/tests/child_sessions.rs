use super::*;
use crate::AttachmentStore as _;
use crate::ToolProvider as _;
use crate::facade_support::ToolStateFacadeOps;
use lash_sansio::sync::MutexExt;

struct AttachmentWritingTool;

struct FirstTurnProcessTool;

impl FirstTurnProcessTool {
    /// Starting a durable process is journal-capable work, so this test tool
    /// registers in the runtime-owned orchestrating lane.
    fn orchestrating() -> crate::tool_provider::orchestration::OrchestratingToolDef {
        let implementation: Arc<
            dyn crate::tool_provider::orchestration::OrchestratingToolImplementation,
        > = Arc::new(Self);
        // SAFETY: lash-core owns this test-only tool contract and its body.
        unsafe {
            crate::tool_provider::orchestration::OrchestratingToolDef::from_first_party(
                implementation,
            )
        }
    }
}

#[async_trait::async_trait]
impl crate::tool_provider::orchestration::OrchestratingToolImplementation for FirstTurnProcessTool {
    fn manifest(&self) -> crate::ToolManifest {
        first_turn_process_tool_definition().manifest()
    }

    fn contract(&self) -> Arc<crate::ToolContract> {
        Arc::new(first_turn_process_tool_definition().contract())
    }

    async fn execute(
        &self,
        _args: &serde_json::Value,
        context: &crate::tool_provider::orchestration::OrchestrationContext<'_>,
    ) -> crate::ToolOutcome {
        match context
            .start_process(crate::ProcessStartRequest::external(
                "child-first-turn-process",
                crate::ProcessOriginator::host(),
                serde_json::json!({ "source": "first child turn" }),
            ))
            .await
        {
            Ok(process) => crate::ToolOutcome::ok(serde_json::json!({ "process": process.id })),
            Err(err) => crate::ToolOutcome::err_fmt(err),
        }
    }
}

fn first_turn_process_tool_definition() -> crate::ToolDefinition {
    crate::ToolDefinition::raw(
        "tool:start_first_turn_process",
        "start_first_turn_process",
        "register an externally owned process during the first child turn",
        crate::ToolDefinition::default_input_schema(),
        serde_json::json!({ "type": "object", "additionalProperties": false }),
    )
}

#[derive(Clone)]
struct NestedChildSessionTool {
    parents: Arc<std::sync::Mutex<Vec<String>>>,
}

impl NestedChildSessionTool {
    /// Nested managed child turns are journal-capable session work, so this
    /// test tool registers in the runtime-owned orchestrating lane.
    fn orchestrating(
        parents: Arc<std::sync::Mutex<Vec<String>>>,
    ) -> crate::tool_provider::orchestration::OrchestratingToolDef {
        let implementation: Arc<
            dyn crate::tool_provider::orchestration::OrchestratingToolImplementation,
        > = Arc::new(Self { parents });
        // SAFETY: lash-core owns this test-only tool contract and its body.
        unsafe {
            crate::tool_provider::orchestration::OrchestratingToolDef::from_first_party(
                implementation,
            )
        }
    }
}

#[async_trait::async_trait]
impl crate::tool_provider::orchestration::OrchestratingToolImplementation
    for NestedChildSessionTool
{
    fn manifest(&self) -> crate::ToolManifest {
        nested_child_session_tool_definition().manifest()
    }

    fn contract(&self) -> Arc<crate::ToolContract> {
        Arc::new(nested_child_session_tool_definition().contract())
    }

    async fn execute(
        &self,
        _args: &serde_json::Value,
        context: &crate::tool_provider::orchestration::OrchestrationContext<'_>,
    ) -> crate::ToolOutcome {
        let parent_id = context.session_id().to_string();
        self.parents.lock_recover().push(parent_id.clone());
        let (child_id, turn_id) = match parent_id.as_str() {
            "root" => ("nested-child", "nested-child-turn"),
            "nested-child" => ("nested-grandchild", "nested-grandchild-turn"),
            other => {
                return crate::ToolOutcome::err_fmt(format_args!(
                    "unexpected nested child parent `{other}`"
                ));
            }
        };
        let child = match context
            .sessions()
            .create_session(
                crate::SessionCreateRequest::child_session(
                    &parent_id,
                    crate::SessionStartPoint::Empty,
                    crate::PluginOptions::default(),
                )
                .with_session_id(child_id)
                .with_plugin_source(crate::SessionPluginSource::CurrentSessionFork),
            )
            .await
        {
            Ok(child) => child,
            Err(err) => return crate::ToolOutcome::err_fmt(format_args!("{err}")),
        };
        let result = context
            .sessions()
            .start_turn(
                &child.session_id,
                turn_id,
                TurnInput::text("run nested child"),
            )
            .await;
        let _ = context.sessions().close_session(&child.session_id).await;
        match result {
            Ok(_) => crate::ToolOutcome::ok(json!({ "status": "ok" })),
            Err(err) => crate::ToolOutcome::err_fmt(format_args!("{err}")),
        }
    }
}

fn nested_child_session_tool_definition() -> crate::ToolDefinition {
    crate::ToolDefinition::raw(
        "tool:spawn_nested_child",
        "spawn_nested_child",
        "spawn a nested child session",
        crate::ToolDefinition::default_input_schema(),
        serde_json::json!({ "type": "object", "additionalProperties": true }),
    )
}

#[async_trait::async_trait]
impl crate::ToolProvider for AttachmentWritingTool {
    fn tool_manifests(&self) -> Vec<crate::ToolManifest> {
        vec![attachment_writing_tool_definition().manifest()]
    }

    fn resolve_contract(&self, name: &str) -> Option<Arc<crate::ToolContract>> {
        (name == "write_attachment")
            .then(|| Arc::new(attachment_writing_tool_definition().contract()))
    }

    async fn execute(&self, call: crate::ToolCall<'_>) -> crate::ToolOutcome {
        let reference = match call
            .context
            .attachments()
            .put(
                vec![4, 2, 4, 2],
                crate::AttachmentCreateMeta::new(
                    crate::MediaType::parse("image/png").unwrap(),
                    Some(crate::AttachmentTypeMetadata::image(Some(2), Some(2))),
                    Some("child.png".to_string()),
                ),
            )
            .await
        {
            Ok(reference) => reference,
            Err(err) => return crate::ToolOutcome::err_fmt(err),
        };
        crate::ToolOutcome::ok(json!({ "attachment_id": reference.id }))
    }
}

fn attachment_writing_tool_definition() -> crate::ToolDefinition {
    crate::ToolDefinition::raw(
        "tool:write_attachment",
        "write_attachment",
        "write a test attachment",
        crate::ToolDefinition::default_input_schema(),
        serde_json::json!({ "type": "object", "additionalProperties": true }),
    )
}

#[tokio::test]
async fn session_manager_create_session_accepts_custom_context_overlay() {
    let runtime = runtime_with_plugins(Vec::new(), mock_provider(Vec::new())).await;
    let manager = runtime.session_state_service().expect("session manager");
    let lifecycle = runtime
        .session_lifecycle_service()
        .expect("session lifecycle");
    let handle = lifecycle
        .create_session(
            crate::SessionCreateRequest::root(
                crate::SessionStartPoint::Empty,
                crate::PluginOptions::default(),
            )
            .with_session_id("memory-child")
            .with_plugin_source(crate::SessionPluginSource::CurrentHostFresh)
            .with_context_overlay(crate::SessionContextOverlay {
                include_base_tools: false,
                tool_providers: vec![Arc::new(MemoryProbeTool)],
                prompt_contributions: vec![crate::PromptContribution::guidance(
                    "Memory Context",
                    "memory child",
                )],
            }),
        )
        .await
        .expect("child session");

    let catalog = manager
        .tool_catalog(&handle.session_id)
        .await
        .expect("tool catalog");
    let tool_names = catalog
        .iter()
        .filter_map(|tool| tool.get("name").and_then(|value| value.as_str()))
        .collect::<Vec<_>>();
    assert_eq!(tool_names, vec!["memory_probe"]);
}

#[tokio::test]
async fn inherited_child_session_carries_parent_tool_state() {
    let plugin_host = crate::PluginHost::new(vec![Arc::new(StaticPluginFactory::new(
        "memory_probe",
        crate::PluginSpec::new().with_tool_provider(Arc::new(MemoryProbeTool)),
    ))]);
    let plugin_session = plugin_host.build_session("root").expect("plugins");
    let mut runtime = LashRuntime::from_embedded_state(
        standard_test_policy(),
        test_host_config(),
        crate::RuntimeServices::new(plugin_session),
        RuntimeSessionState::new(crate::SessionPolicy::new(crate::TurnBudget::Unbounded)),
        crate::testing::runtime_lease_owner(),
    )
    .await
    .expect("runtime");
    set_runtime_provider(&mut runtime, mock_provider(Vec::new()).into_handle());
    let manager = runtime.session_state_service().expect("session manager");
    let lifecycle = runtime
        .session_lifecycle_service()
        .expect("session lifecycle");
    let mut snapshot = manager.tool_state("root").await.expect("tool state");
    snapshot
        .set_membership(&crate::ToolId::from("tool:memory_probe"), false)
        .expect("opt out of parent tool");
    manager
        .apply_tool_state("root", snapshot)
        .await
        .expect("apply dynamic state");

    let handle = lifecycle
        .create_session(
            crate::SessionCreateRequest::child_session(
                "root",
                crate::SessionStartPoint::Empty,
                crate::PluginOptions::default(),
            )
            .with_session_id("dynamic-child")
            .with_plugin_source(crate::SessionPluginSource::CurrentSessionFork),
        )
        .await
        .expect("child session");

    let catalog = manager
        .tool_catalog(&handle.session_id)
        .await
        .expect("tool catalog");
    let tool_names = catalog
        .iter()
        .filter_map(|tool| tool.get("name").and_then(|value| value.as_str()))
        .collect::<Vec<_>>();
    assert!(
        !tool_names.contains(&"memory_probe"),
        "inherited child should receive the parent's membership policy, got {tool_names:?}"
    );
}

#[tokio::test]
async fn existing_session_start_propagates_unknown_checkpoint_component_into_child_first_root() {
    let runtime = runtime_with_plugins(Vec::new(), mock_provider(Vec::new())).await;
    let lifecycle = runtime
        .session_lifecycle_service()
        .expect("session lifecycle");
    let source = lifecycle
        .create_session(
            crate::SessionCreateRequest::root(
                crate::SessionStartPoint::Empty,
                crate::PluginOptions::default(),
            )
            .with_session_id("checkpoint-source")
            .with_plugin_source(crate::SessionPluginSource::CurrentHostFresh),
        )
        .await
        .expect("source session");
    let source_handle = runtime
        .managed_sessions
        .lock()
        .await
        .get(&source.session_id)
        .cloned()
        .expect("managed source runtime");
    let unknown_ref = crate::BlobRef("future-component-ref".to_string());
    {
        let mut source_runtime = source_handle.runtime.lock().await;
        source_runtime.state.checkpoint_components =
            crate::runtime::state::RuntimeCheckpointComponents::complete_refs_for_testing([(
                "extension/future-component".to_string(),
                unknown_ref.clone(),
            )]);
        source_handle.publish_from(&source_runtime);
    }

    let child = lifecycle
        .create_session(
            crate::SessionCreateRequest::root(
                crate::SessionStartPoint::ExistingSession {
                    session_id: source.session_id,
                },
                crate::PluginOptions::default(),
            )
            .with_session_id("checkpoint-child")
            .with_plugin_source(crate::SessionPluginSource::CurrentHostFresh),
        )
        .await
        .expect("child inherits the complete resident component set");
    let child_handle = runtime
        .managed_sessions
        .lock()
        .await
        .get(&child.session_id)
        .cloned()
        .expect("managed child runtime");
    let child_state = child_handle.observe().persisted_state.clone();
    let first_root = child_state
        .checkpoint_components
        .build_checkpoint(crate::PersistedTurnState::default(), None)
        .expect("child first checkpoint root");
    let carried = first_root
        .components
        .get("extension/future-component")
        .expect("unknown component survives ExistingSession inheritance");

    assert_eq!(carried.blob_ref(), Some(&unknown_ref));
    assert_eq!(carried.body(), None, "unknown component remains ref-only");
}

#[tokio::test]
async fn durable_managed_child_writes_to_its_own_attachment_namespace() {
    let transport = mock_provider(vec![
        MockCall {
            stream_events: vec![LlmStreamEvent::Part(LlmOutputPart::ToolCall {
                call_id: "child-attachment-call".to_string(),
                tool_name: "write_attachment".to_string(),
                input_json: "{}".to_string(),
                replay: None,
            })],
            response: Ok(LlmResponse::default()),
        },
        MockCall {
            stream_events: Vec::new(),
            response: Ok(LlmResponse {
                parts: vec![LlmOutputPart::Text {
                    text: "done".to_string(),
                    response_meta: None,
                }],
                response_metadata: Default::default(),
                ..LlmResponse::default()
            }),
        },
    ]);
    let child_factory = RecordingSessionStoreFactory::default().deferring_metadata_to_admission();
    let root_store = Arc::new(RecordingStore::default());
    *root_store.session_meta.lock_recover() = Some(crate::SessionMeta {
        pending_observer_intents: Vec::new(),
        session_id: "root".to_string(),
        relation: crate::SessionRelation::Root,
    });
    let bytes = Arc::new(crate::InMemoryAttachmentStore::new());
    let mut host_config = crate::RuntimeHostConfig::in_memory(
        crate::CommitBudget::bounded(1024 * 1024, 512),
        crate::QueuedWorkBatchingConfig::new(1),
    );
    host_config.durability.attachment_store =
        Arc::new(crate::SessionAttachmentStore::ephemeral(bytes.clone()));
    let host = crate::EmbeddedRuntimeHost::new(host_config)
        .with_session_store_factory(Arc::new(child_factory.clone()));
    let state = RuntimeSessionState {
        session_id: "root".to_string(),
        ..RuntimeSessionState::new(crate::SessionPolicy::new(crate::TurnBudget::Unbounded))
    };
    let mut runtime = LashRuntime::from_persistent_embedded_state(
        standard_test_policy(),
        host,
        crate::PersistentRuntimeServices::new(
            plugin_session_with_tools("root", Arc::new(AttachmentWritingTool)),
            Arc::clone(&root_store) as Arc<dyn crate::store::RuntimePersistence>,
        ),
        state,
        crate::testing::runtime_lease_owner(),
    )
    .await
    .expect("durable root runtime");
    set_runtime_provider(&mut runtime, transport.into_handle());

    let lifecycle = runtime
        .session_lifecycle_service()
        .expect("session lifecycle");
    let child = lifecycle
        .create_session(
            crate::SessionCreateRequest::child_session(
                "root",
                crate::SessionStartPoint::Empty,
                crate::PluginOptions::default(),
            )
            .with_session_id("attachment-child")
            .with_plugin_source(crate::SessionPluginSource::CurrentSessionFork),
        )
        .await
        .expect("durable child session");
    let turn_id = "attachment-child-turn";
    let controller = crate::ScopedEffectController::shared(
        Arc::new(crate::InlineRuntimeEffectController::default()),
        crate::ExecutionScope::turn(&child.session_id, turn_id),
    )
    .expect("child effect controller");
    let request = crate::SessionTurnRequest::new(
        &child.session_id,
        turn_id,
        TurnInput::text("write the attachment"),
        controller,
    )
    .expect("child turn request");
    lifecycle.start_turn(request).await.expect("child turn");

    let id = crate::attachments::content_id(&[4, 2, 4, 2]);
    // The blob lives exactly once in the shared, flat backend...
    assert_eq!(
        bytes.get(&id).await.expect("child attachment bytes").bytes,
        vec![4, 2, 4, 2]
    );
    // ...but reference isolation is now manifest-based: the child session holds
    // the ref, the root session never does. A managed child starts with its own
    // empty manifest, so it cannot resolve a blob the root put and vice versa.
    let child_store = child_factory
        .stores()
        .into_iter()
        .find(|store| {
            store
                .session_meta
                .lock_recover()
                .as_ref()
                .is_some_and(|meta| meta.session_id == "attachment-child")
        })
        .expect("child store");
    assert!(
        crate::AttachmentManifest::holds_ref(&*child_store, "attachment-child", &id)
            .expect("child manifest lookup"),
        "child session must hold the ref it wrote"
    );
    assert!(
        !crate::AttachmentManifest::holds_ref(&*root_store, "root", &id)
            .expect("root manifest lookup"),
        "root session must not hold a ref for the child's attachment"
    );
}

#[tokio::test]
async fn process_registered_during_first_durable_child_turn_remains_listable_after_commit() {
    let transport = mock_provider(vec![
        MockCall {
            stream_events: vec![LlmStreamEvent::Part(LlmOutputPart::ToolCall {
                call_id: "child-process-call".to_string(),
                tool_name: "start_first_turn_process".to_string(),
                input_json: "{}".to_string(),
                replay: None,
            })],
            response: Ok(LlmResponse::default()),
        },
        MockCall {
            stream_events: Vec::new(),
            response: Ok(LlmResponse {
                parts: vec![LlmOutputPart::Text {
                    text: "process registered".to_string(),
                    response_meta: None,
                }],
                response_metadata: Default::default(),
                ..LlmResponse::default()
            }),
        },
    ]);
    let child_factory = RecordingSessionStoreFactory::default().deferring_metadata_to_admission();
    let root_store = Arc::new(RecordingStore::default());
    let registry = Arc::new(crate::TestLocalProcessRegistry::default());
    let embedded = crate::EmbeddedRuntimeHost::new(crate::RuntimeHostConfig::in_memory(
        crate::CommitBudget::bounded(1024 * 1024, 512),
        crate::QueuedWorkBatchingConfig::new(1),
    ))
    .with_session_store_factory(Arc::new(child_factory.clone()));
    let registry: Arc<dyn crate::ProcessRegistry> = registry;
    let host = crate::ProcessRuntimeHost::with_ports(
        embedded,
        crate::testing::process_work_wiring_for_registry(Arc::clone(&registry)),
        Arc::new(crate::NoQueuedWork::new()),
    );
    let mut runtime = LashRuntime::from_persistent_background_state(
        standard_test_policy(),
        host,
        crate::PersistentRuntimeServices::new(
            plugin_session_with_orchestrating_tool("root", FirstTurnProcessTool::orchestrating()),
            root_store as Arc<dyn crate::store::RuntimePersistence>,
        ),
        RuntimeSessionState {
            session_id: "root".to_string(),
            ..RuntimeSessionState::new(crate::SessionPolicy::new(crate::TurnBudget::Unbounded))
        },
        crate::testing::runtime_lease_owner(),
    )
    .await
    .expect("durable root runtime");
    set_runtime_provider(&mut runtime, transport.into_handle());

    let lifecycle = runtime
        .session_lifecycle_service()
        .expect("session lifecycle");
    let child = lifecycle
        .create_session(
            crate::SessionCreateRequest::child_session(
                "root",
                crate::SessionStartPoint::Empty,
                crate::PluginOptions::default(),
            )
            .with_session_id("process-child")
            .with_plugin_source(crate::SessionPluginSource::CurrentSessionFork),
        )
        .await
        .expect("durable child session");
    let child_is_bound = child_factory.stores().into_iter().any(|store| {
        store
            .session_meta
            .lock_recover()
            .as_ref()
            .is_some_and(|meta| meta.session_id == child.session_id)
    });
    assert!(child_is_bound, "managed child must bind its store");
    let turn_id = "process-child-first-turn";
    let controller = crate::ScopedEffectController::shared(
        Arc::new(crate::InlineRuntimeEffectController::default()),
        crate::ExecutionScope::turn(&child.session_id, turn_id),
    )
    .expect("child effect controller");
    lifecycle
        .start_turn(
            crate::SessionTurnRequest::new(
                &child.session_id,
                turn_id,
                TurnInput::text("register the process"),
                controller,
            )
            .expect("child turn request"),
        )
        .await
        .expect("first child turn");

    let child_handle = runtime
        .managed_sessions
        .lock()
        .await
        .get(&child.session_id)
        .cloned()
        .expect("managed child runtime");
    let handles = child_handle.observe().list_all_process_handles().await;
    assert!(
        handles
            .iter()
            .any(|handle| handle.id == "child-first-turn-process"),
        "the observed process must remain reachable from the durable child frame after commit: {handles:?}"
    );
}

struct MemoryProbeFactory;

impl crate::plugin::PluginFactory for MemoryProbeFactory {
    fn id(&self) -> &'static str {
        "root_only_memory_probe"
    }

    fn build(
        &self,
        _ctx: &crate::plugin::PluginSessionContext,
    ) -> Result<Arc<dyn crate::plugin::SessionPlugin>, crate::PluginError> {
        Ok(Arc::new(MemoryProbePlugin))
    }
}

struct MemoryProbePlugin;

impl crate::plugin::SessionPlugin for MemoryProbePlugin {
    fn id(&self) -> &'static str {
        "root_only_memory_probe"
    }

    fn register(&self, reg: &mut crate::plugin::PluginRegistrar) -> Result<(), crate::PluginError> {
        reg.tools().provider(Arc::new(MemoryProbeTool))?;
        Ok(())
    }
}

#[tokio::test]
async fn forked_child_session_keeps_hidden_live_tool_non_executable_across_rebuild() {
    let plugin_host = crate::PluginHost::new(vec![Arc::new(MemoryProbeFactory)]);
    let plugin_session = plugin_host.build_session("root").expect("plugins");
    let mut runtime = LashRuntime::from_embedded_state(
        standard_test_policy(),
        test_host_config(),
        crate::RuntimeServices::new(plugin_session),
        RuntimeSessionState::new(crate::SessionPolicy::new(crate::TurnBudget::Unbounded)),
        crate::testing::runtime_lease_owner(),
    )
    .await
    .expect("runtime");
    set_runtime_provider(&mut runtime, mock_provider(Vec::new()).into_handle());
    let manager = runtime.session_state_service().expect("session manager");
    let lifecycle = runtime
        .session_lifecycle_service()
        .expect("session lifecycle");
    assert!(
        manager
            .tool_state("root")
            .await
            .expect("tool state")
            .contains(&crate::ToolId::from("tool:memory_probe"))
    );

    let handle = lifecycle
        .create_session(
            crate::SessionCreateRequest::child_session(
                "root",
                crate::SessionStartPoint::Empty,
                crate::PluginOptions::default(),
            )
            .with_session_id("filtered-child")
            .with_plugin_source(crate::SessionPluginSource::CurrentSessionFork)
            .with_tool_access(crate::SessionToolAccess {
                tools: Vec::new(),
                hidden_tools: ["memory_probe".to_string()].into_iter().collect(),
            }),
        )
        .await
        .expect("hidden tool policy should survive fork");

    let child_handle = runtime
        .managed_sessions
        .lock()
        .await
        .get(&handle.session_id)
        .cloned()
        .expect("managed child runtime");
    let tool_id = crate::ToolId::from("tool:memory_probe");
    let execute_hidden = |registry: Arc<crate::ToolRegistry>| {
        let tool_id = tool_id.clone();
        async move {
            registry
                .execute_by_id(
                    &tool_id,
                    &json!({}),
                    &crate::testing::mock_attempt_context(),
                )
                .await
        }
    };

    let registry = {
        let child = child_handle.runtime.lock().await;
        child
            .session
            .as_ref()
            .expect("child session")
            .plugins()
            .tool_registry()
    };
    assert!(
        !registry
            .export_state()
            .get(&crate::ToolId::from("tool:memory_probe"))
            .expect("hidden entry retained as policy")
            .is_member()
    );
    let result = execute_hidden(Arc::clone(&registry)).await;
    assert!(
        !result.is_success(),
        "hidden id must not execute: {result:?}"
    );

    let catalog = manager
        .tool_catalog(&handle.session_id)
        .await
        .expect("tool catalog");
    let tool_names = catalog
        .iter()
        .filter_map(|tool| tool.get("name").and_then(|value| value.as_str()))
        .collect::<Vec<_>>();
    assert!(!tool_names.contains(&"memory_probe"));

    {
        let mut child = child_handle.runtime.lock().await;
        child
            .refresh_session_tool_catalog()
            .await
            .expect("rebuild child catalog from live sources");
        child_handle.publish_from(&child);
    }
    let result = execute_hidden(registry).await;
    assert!(
        !result.is_success(),
        "hidden id must remain non-executable after rebuild: {result:?}"
    );
    let rebuilt_catalog = manager
        .tool_catalog(&handle.session_id)
        .await
        .expect("rebuilt tool catalog");
    assert!(
        rebuilt_catalog
            .iter()
            .all(|tool| tool["name"] != json!("memory_probe")),
        "hidden tool must remain absent after live re-enumeration"
    );
}

#[tokio::test]
async fn parent_turn_receives_live_child_token_usage_events() {
    let transport = mock_openai_compatible_provider(vec![
        MockCall {
            stream_events: vec![
                LlmStreamEvent::Part(LlmOutputPart::ToolCall {
                    call_id: "tool-1".to_string(),
                    tool_name: "spawn_child".to_string(),
                    input_json: "{}".to_string(),
                    replay: None,
                }),
                LlmStreamEvent::Usage(LlmUsage {
                    input_tokens: 11,
                    output_tokens: 3,
                    cache_read_input_tokens: 0,
                    cache_write_input_tokens: 0,
                    reasoning_output_tokens: 0,
                }),
            ],
            response: Ok(LlmResponse {
                execution_evidence: Some(crate::ExecutionEvidence {
                    served_model: Some("parent-first".to_string()),
                    reasoning_output_tokens: Some(0),
                    ..Default::default()
                }),
                response_metadata: Default::default(),
                ..LlmResponse::default()
            }),
        },
        MockCall {
            stream_events: vec![LlmStreamEvent::Usage(LlmUsage {
                input_tokens: 7,
                output_tokens: 2,
                cache_read_input_tokens: 4,
                cache_write_input_tokens: 0,
                reasoning_output_tokens: 1,
            })],
            response: Ok(LlmResponse {
                parts: vec![LlmOutputPart::Text {
                    text: "child session".to_string(),
                    response_meta: None,
                }],
                execution_evidence: Some(crate::ExecutionEvidence {
                    served_model: Some("child-only".to_string()),
                    reasoning_output_tokens: Some(99),
                    ..Default::default()
                }),
                response_metadata: Default::default(),
                ..LlmResponse::default()
            }),
        },
        MockCall {
            stream_events: Vec::new(),
            response: Ok(LlmResponse {
                parts: vec![LlmOutputPart::Text {
                    text: "done".to_string(),
                    response_meta: None,
                }],
                execution_evidence: Some(crate::ExecutionEvidence {
                    served_model: Some("parent-second".to_string()),
                    reasoning_output_tokens: Some(7),
                    ..Default::default()
                }),
                ..LlmResponse::default()
            }),
        },
    ]);
    let tools: Arc<dyn crate::ToolProvider> = Arc::new(EmptyTools);
    let mut runtime = runtime_with_plugins_and_tools(
        vec![Arc::new(StaticPluginFactory::new(
            "child-session-tool",
            crate::PluginSpec::new().with_orchestrating_tool(ChildSessionTool::orchestrating()),
        ))],
        tools,
        transport,
    )
    .await;
    let sink = RecordingSink::default();
    let turn_events = RecordingTurnEvents::default();

    let turn = runtime
        .stream_turn(
            TurnInput {
                items: vec![InputItem::Text {
                    text: "run child".to_string(),
                }],
                protocol_turn_options: None,
                trace_turn_id: None,
                protocol_extension: None,
                turn_context: crate::TurnContext::default(),
            },
            TurnOptions::new(
                CancellationToken::new(),
                named_turn_scope("root", "child-session-usage-parent"),
            )
            .with_events(&sink)
            .with_turn_events(&turn_events),
        )
        .await
        .expect("parent turn");

    assert!(matches!(
        &turn.outcome,
        TurnOutcome::Finished(_) | TurnOutcome::AgentFrameSwitch { .. }
    ));
    let events = sink.snapshot();
    let child_usage_event = events
        .clone()
        .into_iter()
        .find_map(|event| match event {
            SessionStreamEvent::ChildTokenUsage {
                session_id,
                source,
                model,
                usage,
                cumulative,
                ..
            } => Some((session_id, source, model, usage, cumulative)),
            _ => None,
        })
        .unwrap_or_else(|| panic!("child token usage event missing from {events:?}"));
    assert_eq!(child_usage_event.0, "subagent-child");
    assert_eq!(child_usage_event.1, "subagent");
    assert_eq!(child_usage_event.2, "mock-model");
    assert_eq!(child_usage_event.3.input_tokens, 7);
    assert_eq!(child_usage_event.3.output_tokens, 2);
    assert_eq!(child_usage_event.3.cache_read_input_tokens, 4);
    assert_eq!(child_usage_event.3.reasoning_output_tokens, 1);
    assert_eq!(child_usage_event.4.cache_read_input_tokens, 4);

    // The session-event projection should also surface a TurnEvent::ChildUsage
    // on the embed-facing TurnActivity stream.
    let activities = turn_events.snapshot();
    let projected = activities
        .iter()
        .find_map(|activity| match &activity.event {
            crate::TurnEvent::ChildUsage {
                session_id,
                source,
                model,
                usage,
                cumulative,
                ..
            } => Some((
                session_id.clone(),
                source.clone(),
                model.clone(),
                usage.clone(),
                cumulative.clone(),
            )),
            _ => None,
        })
        .unwrap_or_else(|| panic!("TurnEvent::ChildUsage missing from {activities:?}"));
    assert_eq!(projected.0, "subagent-child");
    assert_eq!(projected.1, "subagent");
    assert_eq!(projected.2, "mock-model");
    assert_eq!(projected.3.input_tokens, 7);
    assert_eq!(projected.4.cache_read_input_tokens, 4);

    // AssembledTurn carries per-(source, model) child entries so embed
    // consumers can compute per-turn breakdowns without diffing reports.
    let child_entry = turn
        .children_usage
        .iter()
        .find(|entry| entry.source == "subagent" && entry.model == "mock-model")
        .unwrap_or_else(|| panic!("missing subagent ledger entry: {:?}", turn.children_usage));
    assert_eq!(child_entry.usage.input_tokens, 7);
    assert_eq!(child_entry.usage.output_tokens, 2);
    assert_eq!(child_entry.usage.cache_read_input_tokens, 4);
    assert_eq!(child_entry.usage.reasoning_output_tokens, 1);

    assert_eq!(turn.llm_calls.len(), 2);
    let parent_evidence = turn
        .llm_calls
        .iter()
        .map(|call| {
            call.attempts[0]
                .evidence
                .as_ref()
                .expect("parent attempt evidence")
        })
        .collect::<Vec<_>>();
    assert_eq!(
        parent_evidence[0].served_model.as_deref(),
        Some("parent-first")
    );
    assert_eq!(parent_evidence[0].reasoning_output_tokens, Some(0));
    assert_eq!(
        parent_evidence[1].served_model.as_deref(),
        Some("parent-second")
    );
    assert_eq!(parent_evidence[1].reasoning_output_tokens, Some(7));
    assert!(
        parent_evidence
            .iter()
            .all(|evidence| evidence.served_model.as_deref() != Some("child-only"))
    );

    let usage = runtime.usage_report();
    assert_eq!(usage.by_source["subagent"].usage.input_tokens, 7);
    assert_eq!(usage.by_source["subagent"].usage.output_tokens, 2);
    assert_eq!(usage.by_source["subagent"].usage.cache_read_input_tokens, 4);
    assert_eq!(usage.by_source["subagent"].usage.reasoning_output_tokens, 1);
}

#[tokio::test]
async fn nested_child_turns_use_independent_default_task_stacks() {
    let tool_call = |call_id: &str| MockCall {
        stream_events: vec![LlmStreamEvent::Part(LlmOutputPart::ToolCall {
            call_id: call_id.to_string(),
            tool_name: "spawn_nested_child".to_string(),
            input_json: "{}".to_string(),
            replay: None,
        })],
        response: Ok(LlmResponse::default()),
    };
    let text = |value: &str| MockCall {
        stream_events: Vec::new(),
        response: Ok(LlmResponse {
            parts: vec![LlmOutputPart::Text {
                text: value.to_string(),
                response_meta: None,
            }],
            response_metadata: Default::default(),
            ..LlmResponse::default()
        }),
    };
    let transport = mock_provider(vec![
        tool_call("parent-spawn"),
        tool_call("child-spawn"),
        text("grandchild done"),
        text("child done"),
        text("parent done"),
    ]);
    let parents = Arc::new(std::sync::Mutex::new(Vec::new()));
    let tools: Arc<dyn crate::ToolProvider> = Arc::new(EmptyTools);
    let mut runtime = runtime_with_plugins_and_tools(
        vec![Arc::new(StaticPluginFactory::new(
            "nested-child-session-tool",
            crate::PluginSpec::new().with_orchestrating_tool(
                NestedChildSessionTool::orchestrating(Arc::clone(&parents)),
            ),
        ))],
        tools,
        transport,
    )
    .await;

    let turn = runtime
        .stream_turn(
            TurnInput::text("run three levels"),
            TurnOptions::new(
                CancellationToken::new(),
                named_turn_scope("root", "nested-parent-turn"),
            ),
        )
        .await
        .expect("three-level nested turn");

    assert!(matches!(turn.outcome, TurnOutcome::Finished(_)));
    assert_eq!(
        *parents.lock_recover(),
        vec!["root".to_string(), "nested-child".to_string()]
    );
}

#[tokio::test]
async fn parent_turn_keeps_cached_only_child_usage_live() {
    let transport = mock_provider(vec![
        MockCall {
            stream_events: vec![
                LlmStreamEvent::Part(LlmOutputPart::ToolCall {
                    call_id: "tool-1".to_string(),
                    tool_name: "spawn_child".to_string(),
                    input_json: "{}".to_string(),
                    replay: None,
                }),
                LlmStreamEvent::Usage(LlmUsage {
                    input_tokens: 5,
                    output_tokens: 1,
                    cache_read_input_tokens: 0,
                    cache_write_input_tokens: 0,
                    reasoning_output_tokens: 0,
                }),
            ],
            response: Ok(LlmResponse::default()),
        },
        MockCall {
            stream_events: vec![LlmStreamEvent::Usage(LlmUsage {
                input_tokens: 0,
                output_tokens: 0,
                cache_read_input_tokens: 9,
                cache_write_input_tokens: 0,
                reasoning_output_tokens: 0,
            })],
            response: Ok(LlmResponse {
                parts: vec![LlmOutputPart::Text {
                    text: "cached child".to_string(),
                    response_meta: None,
                }],
                response_metadata: Default::default(),
                ..LlmResponse::default()
            }),
        },
        MockCall {
            stream_events: Vec::new(),
            response: Ok(LlmResponse {
                parts: vec![LlmOutputPart::Text {
                    text: "done".to_string(),
                    response_meta: None,
                }],
                response_metadata: Default::default(),
                ..LlmResponse::default()
            }),
        },
    ]);
    let tools: Arc<dyn crate::ToolProvider> = Arc::new(EmptyTools);
    let mut runtime = runtime_with_plugins_and_tools(
        vec![Arc::new(StaticPluginFactory::new(
            "child-session-tool",
            crate::PluginSpec::new().with_orchestrating_tool(ChildSessionTool::orchestrating()),
        ))],
        tools,
        transport,
    )
    .await;
    let sink = RecordingSink::default();

    runtime
        .stream_turn(
            TurnInput {
                items: vec![InputItem::Text {
                    text: "run child".to_string(),
                }],
                protocol_turn_options: None,
                trace_turn_id: None,
                protocol_extension: None,
                turn_context: crate::TurnContext::default(),
            },
            TurnOptions::new(
                CancellationToken::new(),
                named_turn_scope("root", "child-session-event-parent"),
            )
            .with_events(&sink),
        )
        .await
        .expect("parent turn");

    let events = sink.snapshot();
    let child_usage_event = events
        .clone()
        .into_iter()
        .find_map(|event| match event {
            SessionStreamEvent::ChildTokenUsage {
                usage, cumulative, ..
            } => Some((usage, cumulative)),
            _ => None,
        })
        .unwrap_or_else(|| panic!("child token usage event missing from {events:?}"));
    assert_eq!(child_usage_event.0.input_tokens, 0);
    assert_eq!(child_usage_event.0.output_tokens, 0);
    assert_eq!(child_usage_event.0.cache_read_input_tokens, 9);
    assert_eq!(child_usage_event.0.reasoning_output_tokens, 0);
    assert_eq!(child_usage_event.1.cache_read_input_tokens, 9);

    let usage = runtime.usage_report();
    assert_eq!(usage.by_source["subagent"].usage.input_tokens, 0);
    assert_eq!(usage.by_source["subagent"].usage.output_tokens, 0);
    assert_eq!(usage.by_source["subagent"].usage.cache_read_input_tokens, 9);
    assert_eq!(usage.by_source["subagent"].usage.reasoning_output_tokens, 0);
}

/// Tool that parks the turn that calls it: it reports that it started, then
/// never returns. It is the controlled await a cancelled managed child turn is
/// dropped at.
struct ParkedTool {
    started: tokio::sync::mpsc::Sender<()>,
}

#[async_trait::async_trait]
impl crate::ToolProvider for ParkedTool {
    fn tool_manifests(&self) -> Vec<crate::ToolManifest> {
        vec![parked_tool_definition().manifest()]
    }

    fn resolve_contract(&self, name: &str) -> Option<Arc<crate::ToolContract>> {
        (name == "park_forever").then(|| Arc::new(parked_tool_definition().contract()))
    }

    async fn execute(&self, _call: crate::ToolCall<'_>) -> crate::ToolOutcome {
        let _ = self.started.send(()).await;
        std::future::pending::<()>().await;
        unreachable!("the parked tool never completes")
    }
}

fn parked_tool_definition() -> crate::ToolDefinition {
    crate::ToolDefinition::raw(
        "tool:park_forever",
        "park_forever",
        "park the calling turn forever",
        crate::ToolDefinition::default_input_schema(),
        serde_json::json!({ "type": "object", "additionalProperties": false }),
    )
}

fn child_turn_usage_event() -> LlmStreamEvent {
    LlmStreamEvent::Usage(LlmUsage {
        input_tokens: 5,
        output_tokens: 1,
        cache_read_input_tokens: 0,
        cache_write_input_tokens: 0,
        reasoning_output_tokens: 0,
    })
}

fn child_source_input_tokens(runtime: &LashRuntime) -> i64 {
    runtime
        .shared_token_ledger
        .lock_recover()
        .iter()
        .map(|entry| entry.usage.input_tokens)
        .sum()
}

/// Cancelling the process that drives a managed child turn drops the
/// `start_turn` future at whichever await it is parked on. That must release the
/// turn's registration and its live-usage entry: a ghost registration would
/// refuse `close_session` for the session's whole lifetime and reject every
/// later turn on it as "already has a running turn".
#[tokio::test]
async fn cancelled_managed_child_turn_releases_its_registration_and_live_usage() {
    let transport = mock_provider(vec![
        // Gated child turn: one provider round-trip reports usage, then the
        // tool call parks the turn.
        MockCall {
            stream_events: vec![
                LlmStreamEvent::Part(LlmOutputPart::ToolCall {
                    call_id: "park-1".to_string(),
                    tool_name: "park_forever".to_string(),
                    input_json: "{}".to_string(),
                    replay: None,
                }),
                child_turn_usage_event(),
            ],
            response: Ok(LlmResponse::default()),
        },
        // Retried child turn after the cancellation.
        MockCall {
            stream_events: vec![child_turn_usage_event()],
            response: Ok(LlmResponse {
                parts: vec![LlmOutputPart::Text {
                    text: "retried child turn".to_string(),
                    response_meta: None,
                }],
                response_metadata: Default::default(),
                ..LlmResponse::default()
            }),
        },
        // Follow-up on the same child whose first turn future was dropped.
        MockCall {
            stream_events: vec![child_turn_usage_event()],
            response: Ok(LlmResponse {
                parts: vec![LlmOutputPart::Text {
                    text: "cancelled child recovered".to_string(),
                    response_meta: None,
                }],
                response_metadata: Default::default(),
                ..LlmResponse::default()
            }),
        },
    ]);
    let (started_tx, mut started_rx) = tokio::sync::mpsc::channel::<()>(1);
    let tools: Arc<dyn crate::ToolProvider> = Arc::new(ParkedTool {
        started: started_tx,
    });
    let runtime = runtime_with_plugins_and_tools(Vec::new(), tools, transport).await;
    let lifecycle = runtime
        .session_lifecycle_service()
        .expect("session lifecycle");
    lifecycle
        .create_session(
            crate::SessionCreateRequest::child_session(
                runtime.session_id(),
                crate::SessionStartPoint::Empty,
                crate::PluginOptions::default(),
            )
            .with_session_id("cancelled-child")
            .with_plugin_source(crate::SessionPluginSource::CurrentSessionFork),
        )
        .await
        .expect("child session");

    let turn_id = "cancelled-child-turn";
    let request = |session_id: &'static str, turn_id: &str| {
        crate::SessionTurnRequest::new(
            session_id,
            turn_id,
            TurnInput::text("park the child turn"),
            named_turn_scope(session_id, turn_id),
        )
        .expect("child turn request")
    };
    let mut turn = Box::pin(lifecycle.start_turn(request("cancelled-child", turn_id)));
    tokio::select! {
        _ = started_rx.recv() => {}
        outcome = turn.as_mut() => panic!("parked child turn must not complete: {outcome:?}"),
    }
    assert!(
        runtime.managed_turns.lock_recover().contains_key(turn_id),
        "the parked child turn must be registered while it runs"
    );
    assert_eq!(
        child_source_input_tokens(&runtime),
        5,
        "the parked child turn must have reported live usage before cancellation"
    );

    // The cancellation: the owning process drops the child-turn future.
    drop(turn);

    assert!(
        runtime.managed_turns.lock_recover().is_empty(),
        "a cancelled child turn must not leave a ghost registration behind"
    );
    // The live-usage entry is keyed by turn id, so a stranded entry would
    // swallow this turn's usage as already reported.
    lifecycle
        .create_session(
            crate::SessionCreateRequest::child_session(
                runtime.session_id(),
                crate::SessionStartPoint::Empty,
                crate::PluginOptions::default(),
            )
            .with_session_id("retry-child")
            .with_plugin_source(crate::SessionPluginSource::CurrentSessionFork),
        )
        .await
        .expect("retry child session");
    let retried = lifecycle
        .start_turn(request("retry-child", turn_id))
        .await
        .expect("retried child turn");
    assert!(matches!(
        retried.outcome,
        TurnOutcome::Finished(_) | TurnOutcome::AgentFrameSwitch { .. }
    ));
    assert_eq!(
        child_source_input_tokens(&runtime),
        10,
        "the retried turn's live usage must be reported against a reclaimed entry"
    );

    let recovered = lifecycle
        .start_turn(request("cancelled-child", "cancelled-child-turn-2"))
        .await
        .expect("the dropped turn future returns the child runtime's session loan");
    assert_eq!(
        recovered.assistant_output.safe_text,
        "cancelled child recovered"
    );
    assert_eq!(
        child_source_input_tokens(&runtime),
        15,
        "the recovered child's turn must report usage normally"
    );

    lifecycle
        .close_session("cancelled-child")
        .await
        .expect("a cancelled child turn must not keep its session open forever");
}
