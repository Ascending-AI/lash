use super::*;
use lash_core::AttachmentStore as _;
use lash_core::facade_support::ToolStateFacadeOps;
use lash_core::testing::TestTurnDrive as _;
use lash_sansio::sync::MutexExt;

const SEED: u64 = 0x5_c401;

struct AttachmentWritingTool;

struct FirstTurnProcessTool;

/// Starting a durable process is a declared effect: the tool declares the
/// start and lash realizes it after the attempt commits.
#[async_trait::async_trait]
impl lash_core::ToolProvider for FirstTurnProcessTool {
    fn tool_manifests(&self) -> Vec<lash_core::ToolManifest> {
        vec![first_turn_process_tool_definition().manifest()]
    }

    fn resolve_contract(&self, name: &str) -> Option<Arc<lash_core::ToolContract>> {
        (name == "start_first_turn_process")
            .then(|| Arc::new(first_turn_process_tool_definition().contract()))
    }

    async fn execute(&self, call: lash_core::ToolCall<'_>) -> lash_core::ToolAttemptOutcome {
        let session_id = lash_core::SessionId::from(call.context.session_id());
        lash_core::ToolAttemptOutcome::done(
            lash_core::ToolOutcomeDone::ok(serde_json::json!({ "declared": "start" })),
            lash_core::ToolIntents::v3(vec![lash_core::ToolIntent::StartProcess(Box::new(
                lash_core::StartProcessIntent {
                    session_id: session_id.clone(),
                    declaration: lash_core::ProcessStartDeclaration::external(
                        lash_core::ProcessOriginator::host(),
                        serde_json::json!({ "source": "first child turn" }),
                        lash_core::Lifetime::Detached,
                    )
                    .with_observers([session_id]),
                },
            ))]),
        )
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

#[tokio::test(flavor = "multi_thread")]
async fn inherited_child_session_carries_parent_tool_state() {
    let double = kernel_double(SEED, lash_restate_test::ServerConfig::default()).await;
    let backend = double.lash_backend();
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

#[tokio::test(flavor = "multi_thread")]
async fn parent_fork_without_plugin_init_is_refused() {
    let double = kernel_double(SEED + 1, lash_restate_test::ServerConfig::default()).await;
    let backend = double.lash_backend();
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

#[tokio::test(flavor = "multi_thread")]
async fn captured_plugin_init_is_immune_to_post_spawn_parent_mutation() {
    let double = kernel_double(SEED + 2, lash_restate_test::ServerConfig::default()).await;
    let backend = double.lash_backend();
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

#[tokio::test(flavor = "multi_thread")]
async fn durable_child_writes_to_its_own_attachment_namespace() {
    let double = kernel_double(SEED + 3, lash_restate_test::ServerConfig::default()).await;
    let backend = double.lash_backend();
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
    let root_store = double_unbound_recording_store(&double).await;
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
    let handler = double
        .open_handler(AdmittedScope::turn(
            child.session_id.clone(),
            TurnId::from(turn_id).clone(),
        ))
        .await
        .expect("open the scope's handler");
    child_runtime
        .drive_turn(
            TurnInput::text("write the attachment"),
            lash_core::facade_support::TurnOptions::new(CancellationToken::new(), handler.scoped()),
        )
        .await
        .expect("child turn");
    handler.close().await.expect("close the scope's handler");

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

#[tokio::test(flavor = "multi_thread")]
async fn process_registered_during_first_durable_child_turn_remains_listable_after_commit() {
    let double = kernel_double(SEED + 4, lash_restate_test::ServerConfig::default()).await;
    let backend = double.lash_backend();
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
    let root_store = double_unbound_recording_store(&double).await;
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
        Arc::new(lash_core::NoSessionWork::new()),
    );
    let runtime_host = host;
    let runtime_services = lash_core::facade_support::PersistentRuntimeServices::new(
        plugin_session_with_tools(&SessionId::from("root"), Arc::new(FirstTurnProcessTool)),
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
    let handler = double
        .open_handler(AdmittedScope::turn(
            child.session_id.clone(),
            TurnId::from(turn_id).clone(),
        ))
        .await
        .expect("open the scope's handler");
    child_runtime
        .drive_turn(
            TurnInput::text("register the process"),
            lash_core::facade_support::TurnOptions::new(CancellationToken::new(), handler.scoped()),
        )
        .await
        .expect("first child turn");
    handler.close().await.expect("close the scope's handler");

    let child_handle = RuntimeHandle::new(child_runtime);
    let handles = child_handle.observe().list_all_process_handles().await;
    let registered = lash_core::ProcessQuery::list_processes(
        registry.as_ref(),
        &lash_core::ProcessListFilter {
            status: lash_core::ProcessStatusFilter::Any,
            ..lash_core::ProcessListFilter::default()
        },
    )
    .await
    .expect("list the child's processes");
    assert_eq!(
        registered.len(),
        1,
        "the first child turn registered one process"
    );
    assert!(
        handles
            .iter()
            .any(|handle| handle.process_id == registered[0].id),
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

#[tokio::test(flavor = "multi_thread")]
async fn forked_child_session_keeps_hidden_live_tool_out_of_catalog_across_rebuild() {
    let double = kernel_double(SEED + 5, lash_restate_test::ServerConfig::default()).await;
    let backend = double.lash_backend();
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

#[tokio::test(flavor = "multi_thread")]
async fn child_usage_stays_on_the_child_sessions_own_ledger() {
    let double = kernel_double(SEED + 6, lash_restate_test::ServerConfig::default()).await;
    let backend = double.lash_backend();
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
    let handler = double
        .open_handler(AdmittedScope::turn(
            SessionId::from("root").clone(),
            TurnId::from("usage-parent-1").clone(),
        ))
        .await
        .expect("open the scope's handler");
    let first_parent = runtime
        .drive_turn(
            TurnInput::text("run child"),
            TurnOptions::new(CancellationToken::new(), handler.scoped()),
        )
        .await
        .expect("first parent turn");
    handler.close().await.expect("close the scope's handler");
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
    let handler = double
        .open_handler(AdmittedScope::turn(
            child_session_id.clone(),
            child_turn_id.clone(),
        ))
        .await
        .expect("open the scope's handler");
    let child_turn = child_runtime
        .drive_turn(
            TurnInput::text("run the child turn"),
            lash_core::facade_support::TurnOptions::new(CancellationToken::new(), handler.scoped()),
        )
        .await
        .expect("child turn");
    handler.close().await.expect("close the scope's handler");
    assert!(matches!(
        &child_turn.outcome,
        TurnOutcome::Finished(_) | TurnOutcome::AgentFrameSwitch { .. }
    ));
    drop(child_runtime);
    let handler = double
        .open_handler(AdmittedScope::turn(
            SessionId::from("root").clone(),
            TurnId::from("usage-parent-2").clone(),
        ))
        .await
        .expect("open the scope's handler");
    let second_parent = runtime
        .drive_turn(
            TurnInput::text("finish up"),
            TurnOptions::new(CancellationToken::new(), handler.scoped()),
        )
        .await
        .expect("second parent turn");
    handler.close().await.expect("close the scope's handler");

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

#[tokio::test(flavor = "multi_thread")]
async fn cached_only_child_usage_stays_on_the_child_ledger() {
    let double = kernel_double(SEED + 7, lash_restate_test::ServerConfig::default()).await;
    let backend = double.lash_backend();
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
    let handler = double
        .open_handler(AdmittedScope::turn(
            SessionId::from("root").clone(),
            TurnId::from("child-session-event-parent").clone(),
        ))
        .await
        .expect("open the scope's handler");
    runtime
        .drive_turn(
            TurnInput::text("run parent"),
            TurnOptions::new(CancellationToken::new(), handler.scoped()),
        )
        .await
        .expect("parent turn");
    handler.close().await.expect("close the scope's handler");

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
    let handler = double
        .open_handler(AdmittedScope::turn(
            child_session_id.clone(),
            child_turn_id.clone(),
        ))
        .await
        .expect("open the scope's handler");
    child_runtime
        .drive_turn(
            TurnInput::text("run the child turn"),
            lash_core::facade_support::TurnOptions::new(CancellationToken::new(), handler.scoped()),
        )
        .await
        .expect("child turn");
    handler.close().await.expect("close the scope's handler");
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
