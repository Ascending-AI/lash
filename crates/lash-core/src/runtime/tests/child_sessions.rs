use super::*;
use crate::AttachmentStore as _;
use crate::ToolProvider as _;

struct AttachmentWritingTool;

struct FirstTurnProcessTool;

#[async_trait::async_trait]
impl crate::ToolProvider for FirstTurnProcessTool {
    fn tool_manifests(&self) -> Vec<crate::ToolManifest> {
        vec![first_turn_process_tool_definition().manifest()]
    }

    fn resolve_contract(&self, name: &str) -> Option<Arc<crate::ToolContract>> {
        (name == "start_first_turn_process")
            .then(|| Arc::new(first_turn_process_tool_definition().contract()))
    }

    async fn execute(&self, call: crate::ToolCall<'_>) -> crate::ToolResult {
        match call
            .context
            .processes()
            .start(crate::ProcessStartRequest::external(
                "child-first-turn-process",
                crate::ProcessOriginator::host(),
                serde_json::json!({ "source": "first child turn" }),
            ))
            .await
        {
            Ok(process) => crate::ToolResult::ok(serde_json::json!({ "process": process.id })),
            Err(err) => crate::ToolResult::err_fmt(err),
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

#[async_trait::async_trait]
impl crate::ToolProvider for NestedChildSessionTool {
    fn tool_manifests(&self) -> Vec<crate::ToolManifest> {
        vec![nested_child_session_tool_definition().manifest()]
    }

    fn resolve_contract(&self, name: &str) -> Option<Arc<crate::ToolContract>> {
        (name == "spawn_nested_child")
            .then(|| Arc::new(nested_child_session_tool_definition().contract()))
    }

    async fn execute(&self, call: crate::ToolCall<'_>) -> crate::ToolResult {
        let context = call.context;
        let parent_id = context.session_id().to_string();
        self.parents
            .lock()
            .expect("nested child parents lock")
            .push(parent_id.clone());
        let (child_id, turn_id) = match parent_id.as_str() {
            "root" => ("nested-child", "nested-child-turn"),
            "nested-child" => ("nested-grandchild", "nested-grandchild-turn"),
            other => {
                return crate::ToolResult::err_fmt(format_args!(
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
            Err(err) => return crate::ToolResult::err_fmt(format_args!("{err}")),
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
            Ok(_) => crate::ToolResult::ok(json!({ "status": "ok" })),
            Err(err) => crate::ToolResult::err_fmt(format_args!("{err}")),
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

    async fn execute(&self, call: crate::ToolCall<'_>) -> crate::ToolResult {
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
            Err(err) => return crate::ToolResult::err_fmt(err),
        };
        crate::ToolResult::ok(json!({ "attachment_id": reference.id }))
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
    let plugin_session = plugin_host.build_session("root", None).expect("plugins");
    let mut runtime = LashRuntime::from_embedded_state(
        standard_test_policy(),
        test_host_config(),
        crate::RuntimeServices::new(plugin_session),
        RuntimeSessionState::default(),
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
                full_text: "done".to_string(),
                parts: vec![LlmOutputPart::Text {
                    text: "done".to_string(),
                    response_meta: None,
                }],
                response_metadata: Default::default(),
                ..LlmResponse::default()
            }),
        },
    ]);
    let child_factory = RecordingSessionStoreFactory::default();
    let root_store = Arc::new(RecordingStore::default());
    let bytes = Arc::new(crate::InMemoryAttachmentStore::new());
    let mut host_config = crate::RuntimeHostConfig::in_memory();
    host_config.durability.attachment_store =
        Arc::new(crate::SessionAttachmentStore::ephemeral(bytes.clone()));
    let host = crate::EmbeddedRuntimeHost::new(host_config)
        .with_session_store_factory(Arc::new(child_factory.clone()));
    let state = RuntimeSessionState {
        session_id: "root".to_string(),
        ..RuntimeSessionState::default()
    };
    let mut runtime = LashRuntime::from_persistent_embedded_state(
        standard_test_policy(),
        host,
        crate::PersistentRuntimeServices::new(
            plugin_session_with_tools("root", Arc::new(AttachmentWritingTool)),
            Arc::clone(&root_store) as Arc<dyn crate::store::RuntimePersistence>,
        ),
        state,
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
                .lock()
                .expect("lock session meta")
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
                full_text: "process registered".to_string(),
                parts: vec![LlmOutputPart::Text {
                    text: "process registered".to_string(),
                    response_meta: None,
                }],
                response_metadata: Default::default(),
                ..LlmResponse::default()
            }),
        },
    ]);
    let child_factory = RecordingSessionStoreFactory::default();
    let root_store = Arc::new(RecordingStore::default());
    let registry = Arc::new(crate::TestLocalProcessRegistry::default());
    let embedded = crate::EmbeddedRuntimeHost::new(crate::RuntimeHostConfig::in_memory())
        .with_session_store_factory(Arc::new(child_factory.clone()));
    let host = crate::ProcessRuntimeHost::new(embedded, registry);
    let mut runtime = LashRuntime::from_persistent_background_state(
        standard_test_policy(),
        host,
        crate::PersistentRuntimeServices::new(
            plugin_session_with_tools("root", Arc::new(FirstTurnProcessTool)),
            root_store as Arc<dyn crate::store::RuntimePersistence>,
        ),
        RuntimeSessionState {
            session_id: "root".to_string(),
            ..RuntimeSessionState::default()
        },
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
    let child_incarnation = child_factory
        .stores()
        .into_iter()
        .find_map(|store| {
            store
                .session_meta
                .lock()
                .expect("lock child metadata")
                .as_ref()
                .filter(|meta| meta.session_id == child.session_id)
                .map(|meta| meta.incarnation_id.clone())
        })
        .expect("child store incarnation");
    let turn_id = "process-child-first-turn";
    let controller = crate::ScopedEffectController::shared(
        Arc::new(crate::InlineRuntimeEffectController::default()),
        crate::ExecutionScope::turn_incarnation(&child.session_id, child_incarnation, turn_id),
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
        "the process grant must remain reachable from the durable child frame after commit: {handles:?}"
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
    let plugin_session = plugin_host.build_session("root", None).expect("plugins");
    let mut runtime = LashRuntime::from_embedded_state(
        standard_test_policy(),
        test_host_config(),
        crate::RuntimeServices::new(plugin_session),
        RuntimeSessionState::default(),
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
                    &crate::testing::mock_tool_context(),
                    None,
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
                full_text: "child session".to_string(),
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
                full_text: "done".to_string(),
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
    let tools: Arc<dyn crate::ToolProvider> = Arc::new(ChildSessionTool);
    let mut runtime = runtime_with_plugins_and_tools(Vec::new(), tools, transport).await;
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
            full_text: value.to_string(),
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
    let tools: Arc<dyn crate::ToolProvider> = Arc::new(NestedChildSessionTool {
        parents: Arc::clone(&parents),
    });
    let mut runtime = runtime_with_plugins_and_tools(Vec::new(), tools, transport).await;

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
        *parents.lock().expect("nested child parents lock"),
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
                full_text: "cached child".to_string(),
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
                full_text: "done".to_string(),
                parts: vec![LlmOutputPart::Text {
                    text: "done".to_string(),
                    response_meta: None,
                }],
                response_metadata: Default::default(),
                ..LlmResponse::default()
            }),
        },
    ]);
    let tools: Arc<dyn crate::ToolProvider> = Arc::new(ChildSessionTool);
    let mut runtime = runtime_with_plugins_and_tools(Vec::new(), tools, transport).await;
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
