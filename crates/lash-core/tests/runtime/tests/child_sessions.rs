use super::*;
use lash_core::AttachmentStore as _;
use lash_core::facade_support::ToolStateFacadeOps;
use lash_sansio::sync::MutexExt;

struct AttachmentWritingTool;

struct FirstTurnProcessTool;

impl FirstTurnProcessTool {
    /// Starting a durable process is journal-capable work, so this test tool
    /// registers in the runtime-owned orchestrating lane.
    #[expect(
        unsafe_code,
        reason = "OrchestratingToolDef::from_first_party is lash-core's unsafe capability boundary, and this crate owns the tool contract it registers"
    )]
    fn orchestrating() -> lash_core::facade_support::OrchestratingToolDef {
        let implementation: Arc<dyn lash_core::facade_support::OrchestratingToolImplementation> =
            Arc::new(Self);
        // SAFETY: lash-core owns this test-only tool contract and its body.
        unsafe { lash_core::facade_support::OrchestratingToolDef::from_first_party(implementation) }
    }
}

#[async_trait::async_trait]
impl lash_core::facade_support::OrchestratingToolImplementation for FirstTurnProcessTool {
    fn manifest(&self) -> lash_core::ToolManifest {
        first_turn_process_tool_definition().manifest()
    }

    fn contract(&self) -> Arc<lash_core::ToolContract> {
        Arc::new(first_turn_process_tool_definition().contract())
    }

    async fn execute(
        &self,
        _args: &serde_json::Value,
        context: &lash_core::facade_support::OrchestrationContext<'_>,
    ) -> lash_core::ToolOutcome {
        match context
            .start_process(lash_core::ProcessStartRequest::external(
                "child-first-turn-process",
                lash_core::ProcessOriginator::host(),
                serde_json::json!({ "source": "first child turn" }),
                lash_core::ProcessLifecyclePolicy::new(
                    lash_core::ParentScope::Host,
                    lash_core::OnParentEnd::Abandon,
                ),
            ))
            .await
        {
            Ok(process) => lash_core::ToolOutcome::ok(serde_json::json!({ "process": process.id })),
            Err(err) => lash_core::ToolOutcome::err_fmt(err),
        }
    }
}

fn first_turn_process_tool_definition() -> lash_core::ToolDefinition {
    lash_core::ToolDefinition::raw(
        "tool:start_first_turn_process",
        "start_first_turn_process",
        "register an externally owned process during the first child turn",
        lash_core::ToolDefinition::default_input_schema(),
        serde_json::json!({ "type": "object", "additionalProperties": false }),
    )
}

#[async_trait::async_trait]
impl lash_core::ToolProvider for AttachmentWritingTool {
    fn tool_manifests(&self) -> Vec<lash_core::ToolManifest> {
        vec![attachment_writing_tool_definition().manifest()]
    }

    fn resolve_contract(&self, name: &str) -> Option<Arc<lash_core::ToolContract>> {
        (name == "write_attachment")
            .then(|| Arc::new(attachment_writing_tool_definition().contract()))
    }

    async fn execute(&self, call: lash_core::ToolCall<'_>) -> lash_core::ToolAttemptOutcome {
        let reference = match call
            .context
            .attachments()
            .put(
                vec![4, 2, 4, 2],
                lash_core::AttachmentCreateMeta::new(
                    lash_core::MediaType::parse("image/png").unwrap(),
                    Some(lash_core::AttachmentTypeMetadata::image(Some(2), Some(2))),
                    Some("child.png".to_string()),
                ),
            )
            .await
        {
            Ok(reference) => reference,
            Err(err) => return lash_core::ToolOutcome::err_fmt(err).into(),
        };
        lash_core::ToolOutcome::ok(json!({ "attachment_id": reference.id })).into()
    }
}

fn attachment_writing_tool_definition() -> lash_core::ToolDefinition {
    lash_core::ToolDefinition::raw(
        "tool:write_attachment",
        "write_attachment",
        "write a test attachment",
        lash_core::ToolDefinition::default_input_schema(),
        serde_json::json!({ "type": "object", "additionalProperties": true }),
    )
}

#[tokio::test]
async fn inherited_child_session_carries_parent_tool_state() {
    let backend = memory_backend().await;
    let plugin_host =
        lash_core::testing::test_plugin_host(vec![Arc::new(StaticPluginFactory::new(
            "memory_probe",
            lash_core::facade_support::PluginSpec::new()
                .with_tool_provider(Arc::new(MemoryProbeTool)),
        ))]);
    let plugin_session = plugin_host.build_session("root").expect("plugins");
    let runtime_host = test_host_config(&backend);
    let runtime_services = lash_core::testing::runtime_internals::RuntimeServices::new(
        plugin_session,
        std::sync::Arc::clone(&runtime_host.core.durability.attachment_store),
        std::sync::Arc::clone(&runtime_host.core.durability.process_env_store),
    );
    let mut runtime = LashRuntime::from_embedded_state(
        standard_test_policy(),
        runtime_host,
        runtime_services,
        RuntimeSessionState::new(lash_core::SessionPolicy::new(
            lash_core::TurnBudget::Unbounded,
        )),
        lash_core::testing::runtime_lease_owner(),
    )
    .await
    .expect("runtime");
    set_runtime_provider(&mut runtime, mock_provider(Vec::new()).into_handle());
    let manager = runtime.session_state_service().expect("session manager");
    let lifecycle = runtime
        .session_lifecycle_service()
        .expect("session lifecycle");
    let mut snapshot = manager
        .tool_state(&SessionId::from("root"))
        .await
        .expect("tool state");
    snapshot
        .set_membership(&lash_core::ToolId::from("tool:memory_probe"), false)
        .expect("opt out of parent tool");
    manager
        .apply_tool_state(&SessionId::from("root"), snapshot)
        .await
        .expect("apply dynamic state");

    let plugin_init = manager
        .session_plugin_init(&SessionId::from("root"))
        .await
        .expect("plugin init");
    let handle = lifecycle
        .create_session(
            lash_core::SessionCreateRequest::child_session(
                "root",
                lash_core::SessionStartPoint::Empty,
                lash_core::PluginOptions::default(),
            )
            .with_session_id("dynamic-child")
            .with_plugin_source(lash_core::SessionPluginSource::ParentFork)
            .with_plugin_init(plugin_init),
        )
        .await
        .expect("child session");

    let child = reopen_session_runtime(&runtime, &handle.session_id).await;
    let catalog = child
        .session_state_service()
        .expect("child session state")
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
async fn parent_fork_without_plugin_init_is_refused() {
    let backend = memory_backend().await;
    let runtime = TestRuntime::new(&backend, mock_provider(Vec::new()))
        .build()
        .await;
    let lifecycle = runtime
        .session_lifecycle_service()
        .expect("session lifecycle");

    let err = lifecycle
        .create_session(
            lash_core::SessionCreateRequest::child_session(
                "root",
                lash_core::SessionStartPoint::Empty,
                lash_core::PluginOptions::default(),
            )
            .with_session_id("fork-without-init")
            .with_plugin_source(lash_core::SessionPluginSource::ParentFork),
        )
        .await
        .expect_err("a ParentFork without the captured payload must be refused");

    assert!(
        format!("{err}").contains("captured plugin init"),
        "expected a missing-capture refusal, got {err}"
    );
}

#[tokio::test]
async fn captured_plugin_init_is_immune_to_post_spawn_parent_mutation() {
    let backend = memory_backend().await;
    let plugin_host =
        lash_core::testing::test_plugin_host(vec![Arc::new(StaticPluginFactory::new(
            "memory_probe",
            lash_core::facade_support::PluginSpec::new()
                .with_tool_provider(Arc::new(MemoryProbeTool)),
        ))]);
    let plugin_session = plugin_host.build_session("root").expect("plugins");
    let runtime_host = test_host_config(&backend);
    let runtime_services = lash_core::testing::runtime_internals::RuntimeServices::new(
        plugin_session,
        std::sync::Arc::clone(&runtime_host.core.durability.attachment_store),
        std::sync::Arc::clone(&runtime_host.core.durability.process_env_store),
    );
    let mut runtime = LashRuntime::from_embedded_state(
        standard_test_policy(),
        runtime_host,
        runtime_services,
        RuntimeSessionState::new(lash_core::SessionPolicy::new(
            lash_core::TurnBudget::Unbounded,
        )),
        lash_core::testing::runtime_lease_owner(),
    )
    .await
    .expect("runtime");
    set_runtime_provider(&mut runtime, mock_provider(Vec::new()).into_handle());
    let manager = runtime.session_state_service().expect("session manager");
    let lifecycle = runtime
        .session_lifecycle_service()
        .expect("session lifecycle");

    // Capture before the parent mutates its tool state.
    let plugin_init = manager
        .session_plugin_init(&SessionId::from("root"))
        .await
        .expect("plugin init");

    let mut snapshot = manager
        .tool_state(&SessionId::from("root"))
        .await
        .expect("tool state");
    snapshot
        .set_membership(&lash_core::ToolId::from("tool:memory_probe"), false)
        .expect("opt out of parent tool");
    manager
        .apply_tool_state(&SessionId::from("root"), snapshot)
        .await
        .expect("apply dynamic state");

    let handle = lifecycle
        .create_session(
            lash_core::SessionCreateRequest::child_session(
                "root",
                lash_core::SessionStartPoint::Empty,
                lash_core::PluginOptions::default(),
            )
            .with_session_id("spawn-time-child")
            .with_plugin_source(lash_core::SessionPluginSource::ParentFork)
            .with_plugin_init(plugin_init),
        )
        .await
        .expect("child session");

    let child = reopen_session_runtime(&runtime, &handle.session_id).await;
    let catalog = child
        .session_state_service()
        .expect("child session state")
        .tool_catalog(&handle.session_id)
        .await
        .expect("tool catalog");
    let tool_names = catalog
        .iter()
        .filter_map(|tool| tool.get("name").and_then(|value| value.as_str()))
        .collect::<Vec<_>>();
    assert!(
        tool_names.contains(&"memory_probe"),
        "the peer must initialize from the spawn-time capture, not the mutated parent: {tool_names:?}"
    );
}

#[tokio::test]
async fn durable_child_writes_to_its_own_attachment_namespace() {
    let backend = memory_backend().await;
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
    let child_factory = RecordingSessionStoreFactory::over(backend.session_store_factory());
    let root_store = unbound_recording_store(&backend).await;
    let bytes = backend.attachment_store();
    let backend = LayeredBackend::over(backend)
        .map_session_store_factory(|_| Arc::new(child_factory.clone()))
        .into_backend();
    let host_config = lash_core::facade_support::RuntimeHostConfig::new(
        backend.clone(),
        lash_core::CommitBudget::bounded(1024 * 1024, 512),
        lash_core::QueuedWorkBatchingConfig::new(1),
    );
    let host = lash_core::facade_support::EmbeddedRuntimeHost::new(host_config);
    let state = RuntimeSessionState {
        session_id: SessionId::from("root"),
        ..RuntimeSessionState::new(lash_core::SessionPolicy::new(
            lash_core::TurnBudget::Unbounded,
        ))
    };
    let runtime_host = host;
    let runtime_services = lash_core::facade_support::PersistentRuntimeServices::new(
        plugin_session_with_tools(&SessionId::from("root"), Arc::new(AttachmentWritingTool)),
        Arc::clone(&root_store) as Arc<dyn lash_core::store::RuntimePersistence>,
        std::sync::Arc::clone(&runtime_host.core.durability.attachment_store),
        std::sync::Arc::clone(&runtime_host.core.durability.process_env_store),
    );
    let runtime = LashRuntime::from_persistent_embedded_state(
        standard_test_policy(),
        runtime_host,
        runtime_services,
        state,
        lash_core::testing::runtime_lease_owner(),
    )
    .await
    .expect("durable root runtime");

    let lifecycle = runtime
        .session_lifecycle_service()
        .expect("session lifecycle");
    let plugin_init = runtime
        .session_state_service()
        .expect("session state")
        .session_plugin_init(&SessionId::from("root"))
        .await
        .expect("plugin init");
    let child = lifecycle
        .create_session(
            lash_core::SessionCreateRequest::child_session(
                "root",
                lash_core::SessionStartPoint::Empty,
                lash_core::PluginOptions::default(),
            )
            .with_session_id("attachment-child")
            .with_plugin_source(lash_core::SessionPluginSource::ParentFork)
            .with_plugin_init(plugin_init),
        )
        .await
        .expect("durable child session");
    let mut child_runtime = reopen_session_runtime(&runtime, &child.session_id).await;
    set_runtime_provider(&mut child_runtime, transport.into_handle());
    let turn_id = "attachment-child-turn";
    child_runtime
        .run_turn_assembled(
            TurnInput::text("write the attachment"),
            CancellationToken::new(),
            host_turn_scope(
                &child_runtime.host.core,
                &child.session_id,
                &TurnId::from(turn_id),
            ),
        )
        .await
        .expect("child turn");

    let id = lash_core::attachments::content_id(&[4, 2, 4, 2]);
    // The blob lives exactly once in the shared, flat backend...
    assert_eq!(
        bytes.get(&id).await.expect("child attachment bytes").bytes,
        vec![4, 2, 4, 2]
    );
    // Manifest ownership attributes liveness to the child (FIG-653), while
    // reads resolve content addresses across sessions.
    let child_store = child_factory
        .store_for(&SessionId::from("attachment-child"))
        .expect("child store");
    assert!(
        lash_core::AttachmentManifest::list_all_refs(&*child_store)
            .await
            .map(|refs| refs.contains(&id))
            .expect("child manifest lookup"),
        "child session must hold the ref it wrote"
    );
    // The ref is the child's alone: once the child session is deleted, no
    // session roots the blob.
    backend
        .session_store_factory()
        .delete_session(&SessionId::from("attachment-child"))
        .await
        .expect("delete the child session");
    assert!(
        !lash_core::AttachmentManifest::list_all_refs(&*root_store)
            .await
            .map(|refs| refs.contains(&id))
            .expect("root manifest lookup"),
        "root session must not hold a ref for the child's attachment"
    );
}

#[tokio::test]
async fn process_registered_during_first_durable_child_turn_remains_listable_after_commit() {
    let backend = memory_backend().await;
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
    let child_factory = RecordingSessionStoreFactory::over(backend.session_store_factory());
    let root_store = unbound_recording_store(&backend).await;
    let registry = backend.process_registry();
    let backend = LayeredBackend::over(backend)
        .map_session_store_factory(|_| Arc::new(child_factory.clone()))
        .into_backend();
    let embedded = lash_core::facade_support::EmbeddedRuntimeHost::new(
        lash_core::facade_support::RuntimeHostConfig::new(
            backend.clone(),
            lash_core::CommitBudget::bounded(1024 * 1024, 512),
            lash_core::QueuedWorkBatchingConfig::new(1),
        ),
    );
    let registry: Arc<dyn lash_core::ProcessRegistry> = registry;
    let host = lash_core::facade_support::ProcessRuntimeHost::with_ports(
        embedded,
        lash_core::testing::process_work_wiring_for_registry(Arc::clone(&registry)),
        Arc::new(lash_core::NoQueuedWork::new()),
    );
    let runtime_host = host;
    let runtime_services = lash_core::facade_support::PersistentRuntimeServices::new(
        plugin_session_with_orchestrating_tool(
            &SessionId::from("root"),
            FirstTurnProcessTool::orchestrating(),
        ),
        root_store as Arc<dyn lash_core::store::RuntimePersistence>,
        std::sync::Arc::clone(&runtime_host.embedded().core.durability.attachment_store),
        std::sync::Arc::clone(&runtime_host.embedded().core.durability.process_env_store),
    );
    let runtime = LashRuntime::from_persistent_background_state(
        standard_test_policy(),
        runtime_host,
        runtime_services,
        RuntimeSessionState {
            session_id: SessionId::from("root"),
            ..RuntimeSessionState::new(lash_core::SessionPolicy::new(
                lash_core::TurnBudget::Unbounded,
            ))
        },
        lash_core::testing::runtime_lease_owner(),
    )
    .await
    .expect("durable root runtime");

    let lifecycle = runtime
        .session_lifecycle_service()
        .expect("session lifecycle");
    let plugin_init = runtime
        .session_state_service()
        .expect("session state")
        .session_plugin_init(&SessionId::from("root"))
        .await
        .expect("plugin init");
    let child = lifecycle
        .create_session(
            lash_core::SessionCreateRequest::child_session(
                "root",
                lash_core::SessionStartPoint::Empty,
                lash_core::PluginOptions::default(),
            )
            .with_session_id("process-child")
            .with_plugin_source(lash_core::SessionPluginSource::ParentFork)
            .with_plugin_init(plugin_init),
        )
        .await
        .expect("durable child session");
    let child_is_bound = match child_factory.store_for(&child.session_id) {
        Some(store) => lash_core::SessionCommitStore::load_session_meta(store.as_ref())
            .await
            .expect("load child session meta")
            .is_some_and(|meta| meta.session_id == child.session_id),
        None => false,
    };
    assert!(child_is_bound, "initialized child must bind its store");
    let mut child_runtime = reopen_session_runtime(&runtime, &child.session_id).await;
    set_runtime_provider(&mut child_runtime, transport.into_handle());
    let turn_id = "process-child-first-turn";
    child_runtime
        .run_turn_assembled(
            TurnInput::text("register the process"),
            CancellationToken::new(),
            host_turn_scope(
                &child_runtime.host.core,
                &child.session_id,
                &TurnId::from(turn_id),
            ),
        )
        .await
        .expect("first child turn");

    let child_handle = RuntimeHandle::new(child_runtime);
    let handles = child_handle.observe().list_all_process_handles().await;
    assert!(
        handles
            .iter()
            .any(|handle| handle.process_id == "child-first-turn-process"),
        "the observed process must remain reachable from the durable child frame after commit: {handles:?}"
    );
}

struct MemoryProbeFactory;

impl lash_core::plugin::PluginFactory for MemoryProbeFactory {
    fn id(&self) -> &'static str {
        "root_only_memory_probe"
    }

    fn build(
        &self,
        _ctx: &lash_core::plugin::PluginSessionContext,
    ) -> Result<Arc<dyn lash_core::plugin::SessionPlugin>, lash_core::PluginError> {
        Ok(Arc::new(MemoryProbePlugin))
    }
}

struct MemoryProbePlugin;

impl lash_core::plugin::SessionPlugin for MemoryProbePlugin {
    fn id(&self) -> &'static str {
        "root_only_memory_probe"
    }

    fn register(
        &self,
        reg: &mut lash_core::plugin::PluginRegistrar,
    ) -> Result<(), lash_core::PluginError> {
        reg.tools().provider(Arc::new(MemoryProbeTool))?;
        Ok(())
    }
}

#[tokio::test]
async fn forked_child_session_keeps_hidden_live_tool_out_of_catalog_across_rebuild() {
    let backend = memory_backend().await;
    let plugin_host = lash_core::testing::test_plugin_host(vec![Arc::new(MemoryProbeFactory)]);
    let plugin_session = plugin_host.build_session("root").expect("plugins");
    let runtime_host = test_host_config(&backend);
    let runtime_services = lash_core::testing::runtime_internals::RuntimeServices::new(
        plugin_session,
        std::sync::Arc::clone(&runtime_host.core.durability.attachment_store),
        std::sync::Arc::clone(&runtime_host.core.durability.process_env_store),
    );
    let mut runtime = LashRuntime::from_embedded_state(
        standard_test_policy(),
        runtime_host,
        runtime_services,
        RuntimeSessionState::new(lash_core::SessionPolicy::new(
            lash_core::TurnBudget::Unbounded,
        )),
        lash_core::testing::runtime_lease_owner(),
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
            .tool_state(&SessionId::from("root"))
            .await
            .expect("tool state")
            .contains(&lash_core::ToolId::from("tool:memory_probe"))
    );

    let plugin_init = manager
        .session_plugin_init(&SessionId::from("root"))
        .await
        .expect("plugin init");
    let handle = lifecycle
        .create_session(
            lash_core::SessionCreateRequest::child_session(
                "root",
                lash_core::SessionStartPoint::Empty,
                lash_core::PluginOptions::default(),
            )
            .with_session_id("filtered-child")
            .with_plugin_source(lash_core::SessionPluginSource::ParentFork)
            .with_plugin_init(plugin_init)
            .with_tool_access(
                lash_core::SessionToolAccess::ambient()
                    .with_hidden_tools(["memory_probe"])
                    .expect("valid hidden name"),
            ),
        )
        .await
        .expect("hidden tool policy should survive fork");

    let mut child_runtime = reopen_session_runtime(&runtime, &handle.session_id).await;
    let registry = child_runtime
        .session
        .as_ref()
        .expect("child session")
        .plugins()
        .tool_registry();
    assert!(
        registry
            .export_state()
            .get(&lash_core::ToolId::from("tool:memory_probe"))
            .expect("authority-hidden entry retained with curation")
            .is_member(),
        "fork authority must not latch into the child's membership bit"
    );

    let child_manager = child_runtime
        .session_state_service()
        .expect("child session state");
    let catalog = child_manager
        .tool_catalog(&handle.session_id)
        .await
        .expect("tool catalog");
    let tool_names = catalog
        .iter()
        .filter_map(|tool| tool.get("name").and_then(|value| value.as_str()))
        .collect::<Vec<_>>();
    assert!(!tool_names.contains(&"memory_probe"));

    child_runtime
        .refresh_session_tool_catalog()
        .await
        .expect("rebuild child catalog from live sources");
    let rebuilt_catalog = child_manager
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
async fn child_usage_stays_on_the_child_sessions_own_ledger() {
    let backend = memory_backend().await;
    let transport = mock_openai_compatible_provider(vec![
        // The parent's own turn reports the parent's usage.
        MockCall {
            stream_events: vec![LlmStreamEvent::Usage(LlmUsage {
                input_tokens: 11,
                output_tokens: 3,
                cache_read_input_tokens: 0,
                cache_write_input_tokens: 0,
                reasoning_output_tokens: 0,
            })],
            response: Ok(LlmResponse {
                parts: vec![LlmOutputPart::Text {
                    text: "parent first".to_string(),
                    response_meta: None,
                }],
                execution_evidence: Some(lash_core::ExecutionEvidence {
                    served_model: Some("parent-first".to_string()),
                    reasoning_output_tokens: Some(0),
                    ..Default::default()
                }),
                response_metadata: Default::default(),
                ..LlmResponse::default()
            }),
        },
        // The child turn reports usage on the child's own session.
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
                execution_evidence: Some(lash_core::ExecutionEvidence {
                    served_model: Some("child-only".to_string()),
                    reasoning_output_tokens: Some(99),
                    ..Default::default()
                }),
                response_metadata: Default::default(),
                ..LlmResponse::default()
            }),
        },
        // A second parent turn after the child session has closed.
        MockCall {
            stream_events: Vec::new(),
            response: Ok(LlmResponse {
                parts: vec![LlmOutputPart::Text {
                    text: "done".to_string(),
                    response_meta: None,
                }],
                execution_evidence: Some(lash_core::ExecutionEvidence {
                    served_model: Some("parent-second".to_string()),
                    reasoning_output_tokens: Some(7),
                    ..Default::default()
                }),
                ..LlmResponse::default()
            }),
        },
    ]);
    let tools: Arc<dyn lash_core::ToolProvider> = Arc::new(EmptyTools);
    let mut runtime = runtime_with_plugins_and_tools(&backend, Vec::new(), tools, transport).await;

    let first_parent = runtime
        .stream_turn(
            TurnInput::text("run child"),
            TurnOptions::new(
                CancellationToken::new(),
                backend_turn_scope(
                    &backend,
                    &SessionId::from("root"),
                    &TurnId::from("usage-parent-1"),
                ),
            ),
        )
        .await
        .expect("first parent turn");
    assert!(matches!(
        &first_parent.outcome,
        TurnOutcome::Finished(_) | TurnOutcome::AgentFrameSwitch { .. }
    ));

    let lifecycle = runtime
        .session_lifecycle_service()
        .expect("session lifecycle");
    let plugin_init = runtime
        .session_state_service()
        .expect("session state")
        .session_plugin_init(&lash_core::SessionId::from(runtime.session_id()))
        .await
        .expect("plugin init");
    lifecycle
        .create_session(
            lash_core::SessionCreateRequest::child_session(
                runtime.session_id(),
                lash_core::SessionStartPoint::Empty,
                lash_core::PluginOptions::default(),
            )
            .with_session_id("subagent-child")
            .with_plugin_source(lash_core::SessionPluginSource::ParentFork)
            .with_plugin_init(plugin_init),
        )
        .await
        .expect("child session");
    let child_session_id = SessionId::from("subagent-child");
    let child_turn_id = TurnId::from("subagent-child-turn");
    let mut child_runtime = reopen_session_runtime(&runtime, &child_session_id).await;
    let child_turn = child_runtime
        .run_turn_assembled(
            TurnInput::text("run the child turn"),
            CancellationToken::new(),
            backend_turn_scope(&backend, &child_session_id, &child_turn_id),
        )
        .await
        .expect("child turn");
    assert!(matches!(
        &child_turn.outcome,
        TurnOutcome::Finished(_) | TurnOutcome::AgentFrameSwitch { .. }
    ));
    drop(child_runtime);

    let second_parent = runtime
        .stream_turn(
            TurnInput::text("finish up"),
            TurnOptions::new(
                CancellationToken::new(),
                backend_turn_scope(
                    &backend,
                    &SessionId::from("root"),
                    &TurnId::from("usage-parent-2"),
                ),
            ),
        )
        .await
        .expect("second parent turn");

    // Child usage is not folded into the parent's report: it holds only the
    // parent's own calls, and no source carries the child's tokens.
    let usage = runtime.usage_report();
    assert_eq!(usage.by_source["turn"].usage.input_tokens, 11);
    assert_eq!(usage.by_source["turn"].usage.output_tokens, 3);
    assert!(
        !usage.by_source.contains_key("subagent"),
        "child usage must not fold into the parent report: {usage:?}"
    );

    let parent_evidence = first_parent
        .llm_calls
        .iter()
        .chain(second_parent.llm_calls.iter())
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

    // The child's usage survives on its own durable ledger after the child
    // session has closed — a cold reopen of the child store reads it back
    // with no parent involvement.
    let child_ledger = durable_token_ledger(&runtime, "subagent-child").await;
    let child_usage = lash_core::facade_support::SessionUsageReport::from_entries(&child_ledger);
    let child_totals = child_usage
        .by_source
        .values()
        .map(|row| &row.usage)
        .fold(lash_core::TokenUsage::default(), |acc, usage| {
            acc.saturating_add(usage).0
        });
    assert_eq!(child_totals.input_tokens, 7);
    assert_eq!(child_totals.output_tokens, 2);
    assert_eq!(child_totals.cache_read_input_tokens, 4);
    assert_eq!(child_totals.reasoning_output_tokens, 1);
}

/// Reads a closed session's durable token ledger straight from the session
/// store factory — a cold reopen with no resident runtime involved.
async fn durable_token_ledger(
    runtime: &LashRuntime,
    session_id: &str,
) -> Vec<lash_core::TokenLedgerEntry> {
    let store = runtime
        .host
        .core
        .session_store_factory()
        .open_existing_store_by_id(&SessionId::from(session_id))
        .await
        .expect("open child store")
        .expect("child store exists");
    store
        .load_session()
        .await
        .expect("load child session")
        .expect("persisted child session")
        .token_ledger
}

#[tokio::test]
async fn cached_only_child_usage_stays_on_the_child_ledger() {
    let backend = memory_backend().await;
    let transport = mock_provider(vec![
        MockCall {
            stream_events: vec![LlmStreamEvent::Usage(LlmUsage {
                input_tokens: 5,
                output_tokens: 1,
                cache_read_input_tokens: 0,
                cache_write_input_tokens: 0,
                reasoning_output_tokens: 0,
            })],
            response: Ok(LlmResponse {
                parts: vec![LlmOutputPart::Text {
                    text: "parent".to_string(),
                    response_meta: None,
                }],
                response_metadata: Default::default(),
                ..LlmResponse::default()
            }),
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
    ]);
    let tools: Arc<dyn lash_core::ToolProvider> = Arc::new(EmptyTools);
    let mut runtime = runtime_with_plugins_and_tools(&backend, Vec::new(), tools, transport).await;

    runtime
        .stream_turn(
            TurnInput::text("run parent"),
            TurnOptions::new(
                CancellationToken::new(),
                backend_turn_scope(
                    &backend,
                    &SessionId::from("root"),
                    &TurnId::from("child-session-event-parent"),
                ),
            ),
        )
        .await
        .expect("parent turn");

    let lifecycle = runtime
        .session_lifecycle_service()
        .expect("session lifecycle");
    let plugin_init = runtime
        .session_state_service()
        .expect("session state")
        .session_plugin_init(&lash_core::SessionId::from(runtime.session_id()))
        .await
        .expect("plugin init");
    lifecycle
        .create_session(
            lash_core::SessionCreateRequest::child_session(
                runtime.session_id(),
                lash_core::SessionStartPoint::Empty,
                lash_core::PluginOptions::default(),
            )
            .with_session_id("subagent-child")
            .with_plugin_source(lash_core::SessionPluginSource::ParentFork)
            .with_plugin_init(plugin_init),
        )
        .await
        .expect("child session");
    let child_session_id = SessionId::from("subagent-child");
    let child_turn_id = TurnId::from("subagent-child-turn");
    let mut child_runtime = reopen_session_runtime(&runtime, &child_session_id).await;
    child_runtime
        .run_turn_assembled(
            TurnInput::text("run the child turn"),
            CancellationToken::new(),
            backend_turn_scope(&backend, &child_session_id, &child_turn_id),
        )
        .await
        .expect("child turn");
    drop(child_runtime);

    let usage = runtime.usage_report();
    assert_eq!(usage.by_source["turn"].usage.input_tokens, 5);
    assert_eq!(usage.by_source["turn"].usage.output_tokens, 1);
    assert!(
        !usage.by_source.contains_key("subagent"),
        "child usage must not fold into the parent report: {usage:?}"
    );

    let child_ledger = durable_token_ledger(&runtime, "subagent-child").await;
    let child_usage = lash_core::facade_support::SessionUsageReport::from_entries(&child_ledger);
    let child_totals = child_usage
        .by_source
        .values()
        .map(|row| &row.usage)
        .fold(lash_core::TokenUsage::default(), |acc, usage| {
            acc.saturating_add(usage).0
        });
    assert_eq!(child_totals.input_tokens, 0);
    assert_eq!(child_totals.output_tokens, 0);
    assert_eq!(child_totals.cache_read_input_tokens, 9);
    assert_eq!(child_totals.reasoning_output_tokens, 0);
}

/// Tool that parks the turn that calls it: it reports that it started, then
/// never returns. It is the controlled await a dropped child turn is
/// dropped at.
struct ParkedTool {
    started: tokio::sync::mpsc::Sender<()>,
}

#[async_trait::async_trait]
impl lash_core::ToolProvider for ParkedTool {
    fn tool_manifests(&self) -> Vec<lash_core::ToolManifest> {
        vec![parked_tool_definition().manifest()]
    }

    fn resolve_contract(&self, name: &str) -> Option<Arc<lash_core::ToolContract>> {
        (name == "park_forever").then(|| Arc::new(parked_tool_definition().contract()))
    }

    async fn execute(&self, _call: lash_core::ToolCall<'_>) -> lash_core::ToolAttemptOutcome {
        let _ = self.started.send(()).await;
        std::future::pending::<lash_core::ToolAttemptOutcome>().await
    }
}

fn parked_tool_definition() -> lash_core::ToolDefinition {
    lash_core::ToolDefinition::raw(
        "tool:park_forever",
        "park_forever",
        "park the calling turn forever",
        lash_core::ToolDefinition::default_input_schema(),
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

fn session_input_tokens(runtime: &LashRuntime) -> i64 {
    runtime
        .usage_report()
        .by_source
        .values()
        .map(|row| row.usage.input_tokens)
        .sum()
}

/// Dropping a parked child-turn future mid-await — what a cancelled process
/// does to the run that owns the child — must leave the ordinary session
/// reusable: no turn registration outlives the future, so a later turn on the
/// same child runs to completion and reports its own usage.
#[tokio::test]
async fn dropped_child_turn_leaves_the_session_reusable() {
    let backend = memory_backend().await;
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
    let tools: Arc<dyn lash_core::ToolProvider> = Arc::new(ParkedTool {
        started: started_tx,
    });
    let runtime = runtime_with_plugins_and_tools(&backend, Vec::new(), tools, transport).await;
    let lifecycle = runtime
        .session_lifecycle_service()
        .expect("session lifecycle");
    let plugin_init = runtime
        .session_state_service()
        .expect("session state")
        .session_plugin_init(&lash_core::SessionId::from(runtime.session_id()))
        .await
        .expect("plugin init");
    lifecycle
        .create_session(
            lash_core::SessionCreateRequest::child_session(
                runtime.session_id(),
                lash_core::SessionStartPoint::Empty,
                lash_core::PluginOptions::default(),
            )
            .with_session_id("cancelled-child")
            .with_plugin_source(lash_core::SessionPluginSource::ParentFork)
            .with_plugin_init(plugin_init),
        )
        .await
        .expect("child session");

    let cancelled_child_session_id = SessionId::from("cancelled-child");
    let cancelled_child_turn_id = TurnId::from("cancelled-child-turn");
    let mut child = reopen_session_runtime(&runtime, &cancelled_child_session_id).await;
    let mut turn = Box::pin(child.run_turn_assembled(
        TurnInput::text("park the child turn"),
        CancellationToken::new(),
        host_turn_scope(
            &child.host.core,
            &cancelled_child_session_id,
            &cancelled_child_turn_id,
        ),
    ));
    tokio::select! {
        _ = started_rx.recv() => {}
        outcome = turn.as_mut() => panic!("parked child turn must not complete: {outcome:?}"),
    }

    // The cancellation: the owning process drops the child-turn future.
    drop(turn);

    assert_eq!(
        session_input_tokens(&child),
        0,
        "usage lands at turn finish, so a dropped child turn reports nothing"
    );
    let plugin_init = runtime
        .session_state_service()
        .expect("session state")
        .session_plugin_init(&lash_core::SessionId::from(runtime.session_id()))
        .await
        .expect("plugin init");
    lifecycle
        .create_session(
            lash_core::SessionCreateRequest::child_session(
                runtime.session_id(),
                lash_core::SessionStartPoint::Empty,
                lash_core::PluginOptions::default(),
            )
            .with_session_id("retry-child")
            .with_plugin_source(lash_core::SessionPluginSource::ParentFork)
            .with_plugin_init(plugin_init),
        )
        .await
        .expect("retry child session");
    let retry_child_session_id = SessionId::from("retry-child");
    let retry_child_turn_id = TurnId::from("cancelled-child-turn");
    let mut retry_child = reopen_session_runtime(&runtime, &retry_child_session_id).await;
    let retried = retry_child
        .run_turn_assembled(
            TurnInput::text("park the child turn"),
            CancellationToken::new(),
            host_turn_scope(
                &retry_child.host.core,
                &retry_child_session_id,
                &retry_child_turn_id,
            ),
        )
        .await
        .expect("retried child turn");
    assert!(matches!(
        retried.outcome,
        TurnOutcome::Finished(_) | TurnOutcome::AgentFrameSwitch { .. }
    ));
    assert_eq!(
        session_input_tokens(&retry_child),
        5,
        "the retried turn's usage lands on its own session's ledger"
    );
    assert_eq!(
        session_input_tokens(&child),
        0,
        "a different session's turn must not leak usage into the cancelled child's ledger"
    );

    let recovered_turn_id = TurnId::from("cancelled-child-turn-2");
    let recovered = child
        .run_turn_assembled(
            TurnInput::text("park the child turn"),
            CancellationToken::new(),
            host_turn_scope(
                &child.host.core,
                &cancelled_child_session_id,
                &recovered_turn_id,
            ),
        )
        .await
        .expect("the dropped turn future leaves the child session reusable");
    assert_eq!(
        recovered.assistant_output.safe_text,
        "cancelled child recovered"
    );
    assert_eq!(
        session_input_tokens(&child),
        5,
        "the recovered child's turn must report usage normally"
    );
}
