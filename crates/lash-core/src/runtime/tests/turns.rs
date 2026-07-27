use super::*;
use crate::ToolProvider as _;
use std::sync::atomic::AtomicUsize;

struct AttachmentPutTool;

fn attachment_put_tool_definition() -> crate::ToolDefinition {
    crate::ToolDefinition::raw(
        "tool:attachment_put",
        "attachment_put",
        "Write an attachment through the active runtime facade.",
        crate::ToolDefinition::default_input_schema(),
        serde_json::json!({ "type": "object", "additionalProperties": true }),
    )
}

#[async_trait::async_trait]
impl crate::ToolProvider for AttachmentPutTool {
    fn tool_manifests(&self) -> Vec<crate::ToolManifest> {
        vec![attachment_put_tool_definition().manifest()]
    }

    fn resolve_contract(&self, name: &str) -> Option<Arc<crate::ToolContract>> {
        (name == "attachment_put").then(|| Arc::new(attachment_put_tool_definition().contract()))
    }

    async fn execute(&self, call: crate::ToolCall<'_>) -> crate::ToolResult {
        let reference = call
            .context
            .attachments()
            .put(
                b"turn-owned-tool-attachment".to_vec(),
                crate::AttachmentCreateMeta::new(
                    crate::MediaType::parse("image/png").unwrap(),
                    Some(crate::AttachmentTypeMetadata::image(Some(1), Some(1))),
                    Some("turn-owned.png".to_string()),
                ),
            )
            .await
            .expect("tool attachment put");
        crate::ToolResult::from_output(crate::ToolCallOutput::success(
            crate::ToolValue::Attachment(crate::AttachmentSource::stored(reference)),
        ))
    }
}

fn attachment_put_transport() -> TestProvider {
    let call_index = Arc::new(AtomicUsize::new(0));
    TestProvider::builder()
        .kind("mock")
        .requires_streaming(true)
        .complete(move |_| {
            let call_index = Arc::clone(&call_index);
            async move {
                Ok(match call_index.fetch_add(1, Ordering::SeqCst) {
                    0 => LlmResponse {
                        parts: vec![LlmOutputPart::ToolCall {
                            call_id: "attachment-put-call".to_string(),
                            tool_name: "attachment_put".to_string(),
                            input_json: "{}".to_string(),
                            replay: None,
                        }],
                        response_metadata: Default::default(),
                        ..LlmResponse::default()
                    },
                    1 => LlmResponse {
                        full_text: "attachment stored".to_string(),
                        parts: vec![LlmOutputPart::Text {
                            text: "attachment stored".to_string(),
                            response_meta: None,
                        }],
                        response_metadata: Default::default(),
                        ..LlmResponse::default()
                    },
                    index => panic!("unexpected attachment provider call {index}"),
                })
            }
        })
        .build()
}

fn assert_turn_owned_attachment(store: &RecordingStore, turn_id: &str) {
    let entries = store.attachment_manifest_entries();
    assert_eq!(entries.len(), 1);
    assert_eq!(
        entries[0].owner_kind,
        Some(crate::AttachmentOwnerKind::Turn)
    );
    assert_eq!(entries[0].owner_id.as_deref(), Some(turn_id));
}

fn lease_owner(owner_id: &str) -> crate::LeaseOwnerIdentity {
    crate::LeaseOwnerIdentity::opaque(owner_id, format!("{owner_id}:incarnation"))
}

#[derive(Debug)]
struct ManualClock {
    epoch_ms: std::sync::atomic::AtomicU64,
}

impl ManualClock {
    fn new(epoch_ms: u64) -> Self {
        Self {
            epoch_ms: std::sync::atomic::AtomicU64::new(epoch_ms),
        }
    }

    fn advance_ms(&self, delta_ms: u64) {
        self.epoch_ms
            .fetch_add(delta_ms, std::sync::atomic::Ordering::SeqCst);
    }
}

#[async_trait::async_trait]
impl crate::Clock for ManualClock {
    fn now(&self) -> std::time::Instant {
        std::time::Instant::now()
    }

    fn timestamp_ms(&self) -> u64 {
        self.epoch_ms.load(std::sync::atomic::Ordering::SeqCst)
    }

    fn timestamp_rfc3339(&self) -> String {
        self.timestamp_datetime().to_rfc3339()
    }

    fn timestamp_datetime(&self) -> chrono::DateTime<chrono::Utc> {
        let system_time =
            std::time::UNIX_EPOCH + std::time::Duration::from_millis(self.timestamp_ms());
        chrono::DateTime::<chrono::Utc>::from(system_time)
    }

    async fn sleep(&self, duration: std::time::Duration) {
        tokio::time::sleep(duration).await;
    }

    async fn sleep_until(&self, deadline: std::time::Instant) {
        tokio::time::sleep_until(tokio::time::Instant::from_std(deadline)).await;
    }
}

#[derive(Debug)]
struct StepExpiryClock {
    epoch_ms: u64,
    live_timestamp_calls: std::sync::atomic::AtomicU64,
    timestamp_calls: std::sync::atomic::AtomicU64,
    armed: AtomicBool,
}

impl StepExpiryClock {
    fn new(epoch_ms: u64) -> Self {
        Self {
            epoch_ms,
            live_timestamp_calls: std::sync::atomic::AtomicU64::new(u64::MAX),
            timestamp_calls: std::sync::atomic::AtomicU64::new(0),
            armed: AtomicBool::new(false),
        }
    }

    fn expire_after_timestamp_calls(&self, live_calls: u64) {
        self.timestamp_calls
            .store(0, std::sync::atomic::Ordering::SeqCst);
        self.live_timestamp_calls
            .store(live_calls, std::sync::atomic::Ordering::SeqCst);
        self.armed.store(true, Ordering::SeqCst);
    }
}

#[async_trait::async_trait]
impl crate::Clock for StepExpiryClock {
    fn now(&self) -> std::time::Instant {
        std::time::Instant::now()
    }

    fn timestamp_ms(&self) -> u64 {
        if !self.armed.load(Ordering::SeqCst) {
            return self.epoch_ms;
        }
        let call = self
            .timestamp_calls
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        if call
            < self
                .live_timestamp_calls
                .load(std::sync::atomic::Ordering::SeqCst)
        {
            self.epoch_ms
        } else {
            self.epoch_ms
                .saturating_add(crate::LeaseTimings::default().ttl_ms())
                .saturating_add(1)
        }
    }

    fn timestamp_rfc3339(&self) -> String {
        self.timestamp_datetime().to_rfc3339()
    }

    fn timestamp_datetime(&self) -> chrono::DateTime<chrono::Utc> {
        let system_time =
            std::time::UNIX_EPOCH + std::time::Duration::from_millis(self.timestamp_ms());
        chrono::DateTime::<chrono::Utc>::from(system_time)
    }

    async fn sleep(&self, duration: std::time::Duration) {
        tokio::time::sleep(duration).await;
    }

    async fn sleep_until(&self, deadline: std::time::Instant) {
        tokio::time::sleep_until(tokio::time::Instant::from_std(deadline)).await;
    }
}

struct FrameRotatingDynamicTool {
    rotated: Arc<AtomicBool>,
}

fn rotating_tool_definition(name: &str) -> crate::ToolDefinition {
    crate::ToolDefinition::raw(
        format!("tool:{name}"),
        name,
        "Exercise live tool discovery across an AgentFrame rotation",
        crate::ToolDefinition::default_input_schema(),
        json!({ "type": "object", "additionalProperties": true }),
    )
}

#[async_trait::async_trait]
impl crate::ToolProvider for FrameRotatingDynamicTool {
    fn tool_manifests(&self) -> Vec<crate::ToolManifest> {
        let mut manifests = vec![
            rotating_tool_definition("rotate_surface").manifest(),
            rotating_tool_definition("curated_before_rotation").manifest(),
        ];
        if self.rotated.load(Ordering::SeqCst) {
            manifests.push(rotating_tool_definition("new_after_rotation").manifest());
            manifests.push(rotating_tool_definition("hidden_after_rotation").manifest());
        }
        manifests
    }

    fn resolve_contract(&self, name: &str) -> Option<Arc<crate::ToolContract>> {
        self.tool_manifests()
            .into_iter()
            .any(|manifest| manifest.name == name)
            .then(|| Arc::new(rotating_tool_definition(name).contract()))
    }

    async fn execute(&self, call: crate::ToolCall<'_>) -> crate::ToolResult {
        match call.name {
            "rotate_surface" => {
                self.rotated.store(true, Ordering::SeqCst);
                crate::ToolResult::ok(json!({ "rotated": true })).with_control(
                    crate::ToolControl::SwitchAgentFrame {
                        frame_id: "live-surface-frame".to_string(),
                        initial_nodes: Vec::new(),
                        task: Some("call the newly available tool".to_string()),
                    },
                )
            }
            "new_after_rotation" => crate::ToolResult::ok(json!({ "called": call.name }))
                .with_control(crate::ToolControl::Finish {
                    value: json!("new tool executed").into(),
                }),
            "curated_before_rotation" | "hidden_after_rotation" => {
                crate::ToolResult::ok(json!({ "called": call.name }))
            }
            name => crate::ToolResult::err_fmt(format_args!("unknown rotating tool `{name}`")),
        }
    }
}

#[tokio::test]
async fn continue_as_frame_rotation_reconciles_newly_advertised_tool() {
    let transport = mock_provider(vec![
        MockCall {
            stream_events: Vec::new(),
            response: Ok(LlmResponse {
                parts: vec![LlmOutputPart::ToolCall {
                    call_id: "rotate-call".to_string(),
                    tool_name: "rotate_surface".to_string(),
                    input_json: "{}".to_string(),
                    replay: None,
                }],
                response_metadata: Default::default(),
                ..LlmResponse::default()
            }),
        },
        MockCall {
            stream_events: Vec::new(),
            response: Ok(LlmResponse {
                parts: vec![LlmOutputPart::ToolCall {
                    call_id: "new-tool-call".to_string(),
                    tool_name: "new_after_rotation".to_string(),
                    input_json: "{}".to_string(),
                    replay: None,
                }],
                response_metadata: Default::default(),
                ..LlmResponse::default()
            }),
        },
    ]);
    let tools: Arc<dyn crate::ToolProvider> = Arc::new(FrameRotatingDynamicTool {
        rotated: Arc::new(AtomicBool::new(false)),
    });
    let mut factories = crate::testing::test_standard_protocol_factories();
    factories.push(Arc::new(StaticPluginFactory::new(
        "frame_rotating_tools",
        crate::PluginSpec::new().with_tool_provider(tools),
    )));
    let plugins = crate::PluginHost::new(factories)
        .build_session_with_parent(
            "root",
            Some("parent".to_string()),
            None,
            crate::plugin::SessionAuthorityContext {
                tool_access: crate::SessionToolAccess {
                    tools: Vec::new(),
                    hidden_tools: ["hidden_after_rotation".to_string()].into_iter().collect(),
                },
                ..crate::plugin::SessionAuthorityContext::default()
            },
        )
        .expect("frame child plugins");
    let mut runtime = LashRuntime::from_embedded_state(
        standard_test_policy(),
        test_host_config(),
        crate::RuntimeServices::new(plugins),
        RuntimeSessionState::default(),
    )
    .await
    .expect("frame child runtime");
    set_runtime_provider(&mut runtime, transport.into_handle());
    let mut curated = runtime.tool_state().expect("pre-rotation tool state");
    curated
        .set_membership(&crate::ToolId::from("tool:curated_before_rotation"), false)
        .expect("opt out before rotation");
    runtime
        .apply_tool_state(curated)
        .await
        .expect("apply pre-rotation curation");

    let run = runtime
        .stream_turn_with_agent_frames(
            TurnInput::text("rotate the frame"),
            TurnOptions::new(
                CancellationToken::new(),
                named_turn_scope("root", "live-surface-frame-rotation"),
            ),
        )
        .await
        .expect("AgentFrame run");

    assert_eq!(run.frame_switch_count(), 1);
    let final_turn = run.final_turn().expect("final frame");
    assert!(
        matches!(
            &final_turn.outcome,
            TurnOutcome::Finished(TurnFinish::ToolValue { tool_name, value })
                if tool_name == "new_after_rotation" && *value == json!("new tool executed")
        ),
        "new tool must be callable in the follow frame: {:?}",
        final_turn.outcome
    );
    let catalog_names = runtime
        .active_tool_catalog_shared()
        .expect("post-rotation model catalog")
        .iter()
        .filter_map(|entry| entry["name"].as_str().map(ToOwned::to_owned))
        .collect::<Vec<_>>();
    assert!(catalog_names.contains(&"new_after_rotation".to_string()));
    assert!(!catalog_names.contains(&"hidden_after_rotation".to_string()));
    assert!(!catalog_names.contains(&"curated_before_rotation".to_string()));

    let registry = runtime
        .session
        .as_ref()
        .expect("frame child session")
        .plugins()
        .tool_registry();
    let post_rotation_state = registry.export_state();
    assert!(
        !post_rotation_state
            .get(&crate::ToolId::from("tool:curated_before_rotation"))
            .expect("curated entry survives rotation")
            .is_member()
    );
    assert!(
        !post_rotation_state
            .get(&crate::ToolId::from("tool:hidden_after_rotation"))
            .expect("new hidden entry is retained as denied policy")
            .is_member()
    );
    let hidden_result = registry
        .execute_by_id(
            &crate::ToolId::from("tool:hidden_after_rotation"),
            &json!({}),
            &crate::testing::mock_tool_context(),
            None,
        )
        .await;
    assert!(
        !hidden_result.is_success(),
        "new hidden id must not execute after frame rotation: {hidden_result:?}"
    );
}

struct ExpireLeaseAtFinalCommit {
    clock: Arc<ManualClock>,
    expired: AtomicBool,
}

impl ExpireLeaseAtFinalCommit {
    fn new(clock: Arc<ManualClock>) -> Self {
        Self {
            clock,
            expired: AtomicBool::new(false),
        }
    }
}

impl crate::runtime::RuntimeTurnPhaseProbe for ExpireLeaseAtFinalCommit {
    fn begin(&self, phase: crate::runtime::RuntimeTurnPhase) {
        if phase == crate::runtime::RuntimeTurnPhase::FinalCommit
            && !self.expired.swap(true, Ordering::SeqCst)
        {
            self.clock
                .advance_ms(crate::LeaseTimings::default().ttl_ms() + 1);
        }
    }

    fn end(&self, _phase: crate::runtime::RuntimeTurnPhase) {}
}

struct ExpireLeaseAfterPromptBuild {
    clock: Arc<ManualClock>,
    expired: AtomicBool,
}

impl ExpireLeaseAfterPromptBuild {
    fn new(clock: Arc<ManualClock>) -> Self {
        Self {
            clock,
            expired: AtomicBool::new(false),
        }
    }
}

impl crate::runtime::RuntimeTurnPhaseProbe for ExpireLeaseAfterPromptBuild {
    fn begin(&self, _phase: crate::runtime::RuntimeTurnPhase) {}

    fn end(&self, phase: crate::runtime::RuntimeTurnPhase) {
        if phase == crate::runtime::RuntimeTurnPhase::PromptBuild
            && !self.expired.swap(true, Ordering::SeqCst)
        {
            self.clock
                .advance_ms(crate::LeaseTimings::default().ttl_ms() + 1);
        }
    }
}

struct ExpireLeaseAfterRetainedCommit {
    clock: Arc<ManualClock>,
    expired: AtomicBool,
}

struct PauseAtFinalizeTurn {
    entered: Arc<AtomicBool>,
    release: Arc<AtomicBool>,
}

impl crate::runtime::RuntimeTurnPhaseProbe for PauseAtFinalizeTurn {
    fn begin(&self, phase: crate::runtime::RuntimeTurnPhase) {
        if phase != crate::runtime::RuntimeTurnPhase::FinalizeTurn {
            return;
        }
        self.entered.store(true, Ordering::SeqCst);
        while !self.release.load(Ordering::SeqCst) {
            std::thread::yield_now();
        }
    }

    fn end(&self, _phase: crate::runtime::RuntimeTurnPhase) {}
}

struct PauseAfterEffectLoop {
    entered: Arc<AtomicBool>,
    release: Arc<AtomicBool>,
}

impl crate::runtime::RuntimeTurnPhaseProbe for PauseAfterEffectLoop {
    fn begin(&self, _phase: crate::runtime::RuntimeTurnPhase) {}

    fn end(&self, phase: crate::runtime::RuntimeTurnPhase) {
        if phase != crate::runtime::RuntimeTurnPhase::EffectLoop {
            return;
        }
        self.entered.store(true, Ordering::SeqCst);
        while !self.release.load(Ordering::SeqCst) {
            std::thread::yield_now();
        }
    }
}

impl ExpireLeaseAfterRetainedCommit {
    fn new(clock: Arc<ManualClock>) -> Self {
        Self {
            clock,
            expired: AtomicBool::new(false),
        }
    }
}

impl crate::runtime::RuntimeTurnPhaseProbe for ExpireLeaseAfterRetainedCommit {
    fn begin(&self, phase: crate::runtime::RuntimeTurnPhase) {
        if phase == crate::runtime::RuntimeTurnPhase::PostPersistHooks
            && !self.expired.swap(true, Ordering::SeqCst)
        {
            self.clock
                .advance_ms(crate::LeaseTimings::default().ttl_ms() + 1);
        }
    }

    fn end(&self, _phase: crate::runtime::RuntimeTurnPhase) {}
}

async fn standard_runtime_with_transport_and_queue_store(
    transport: TestProvider,
) -> (LashRuntime, Arc<RecordingStore>) {
    let store = Arc::new(RecordingStore::default());
    let runtime_store: Arc<dyn crate::store::RuntimePersistence> = store.clone();
    let mut runtime = runtime_with_plugins_and_tools_and_host_and_store(
        Vec::new(),
        Arc::new(EmptyTools),
        transport,
        test_host_config(),
        runtime_store,
    )
    .await;
    runtime.host.process_registry = Some(Arc::new(crate::TestLocalProcessRegistry::default()));
    (runtime, store)
}

async fn standard_runtime_with_transport_and_queue_store_clock(
    transport: TestProvider,
    clock: Arc<dyn crate::Clock>,
) -> (LashRuntime, Arc<RecordingStore>) {
    let store = Arc::new(RecordingStore::with_clock(clock));
    let runtime_store: Arc<dyn crate::store::RuntimePersistence> = store.clone();
    let mut runtime = runtime_with_plugins_and_tools_and_host_and_store(
        Vec::new(),
        Arc::new(EmptyTools),
        transport,
        test_host_config(),
        runtime_store,
    )
    .await;
    runtime.host.process_registry = Some(Arc::new(crate::TestLocalProcessRegistry::default()));
    (runtime, store)
}

async fn append_process_wake_to_queue(
    registry: &dyn crate::ProcessRegistry,
    store: &RecordingStore,
    process_id: &str,
    request: crate::ProcessEventAppendRequest,
) -> crate::ProcessWakeDelivery {
    let appended = registry
        .append_event(process_id, request)
        .await
        .expect("append wake");
    let wake = appended.wake_delivery.expect("wake delivery");
    crate::store::QueuedWorkStore::enqueue_queued_work(
        store,
        crate::process_wake_batch_draft(wake.clone()),
    )
    .await
    .expect("enqueue wake");
    wake
}

fn process_wake_event_type() -> crate::ProcessEventType {
    crate::ProcessEventType {
        name: "process.wake".to_string(),
        payload_schema: crate::LashSchema::any(),
        semantics: crate::ProcessEventSemanticsSpec {
            wake: Some(crate::ProcessWakeSpec {
                when: None,
                input: crate::ProcessValueSelector::Pointer("/text".to_string()),
                dedupe_key: crate::ProcessWakeDedupeKey::EventIdentity,
            }),
            ..crate::ProcessEventSemanticsSpec::default()
        },
    }
}

fn request_contains_text(request: &crate::llm::types::LlmRequest, needle: &str) -> bool {
    request.messages.iter().any(|message| {
        message.blocks.iter().any(|block| match block {
            crate::llm::types::LlmContentBlock::Text { text, .. } => text.contains(needle),
            _ => false,
        })
    })
}

async fn enqueue_turn_input_for_checkpoint(
    store: &RecordingStore,
    session_id: &str,
    turn_id: &str,
    source_key: Option<String>,
    input: TurnInput,
) -> crate::PendingTurnInput {
    let mut draft = crate::PendingTurnInputDraft::new(
        session_id.to_string(),
        crate::TurnInputIngress::active_turn(
            turn_id.to_string(),
            crate::TurnInputCheckpointBoundary::AfterWork,
        ),
        input,
    );
    draft.source_key = source_key;
    crate::store::TurnInputStore::enqueue_pending_turn_input(store, draft)
        .await
        .expect("enqueue turn input")
}

async fn enqueue_idle_turn_input(
    store: &RecordingStore,
    session_id: &str,
    text: &str,
) -> crate::PendingTurnInput {
    crate::store::TurnInputStore::enqueue_pending_turn_input(
        store,
        crate::PendingTurnInputDraft::new(
            session_id.to_string(),
            crate::TurnInputIngress::NextTurn,
            TurnInput::text(text),
        ),
    )
    .await
    .expect("enqueue idle turn input")
}

async fn enqueue_session_command(
    store: &RecordingStore,
    session_id: &str,
    reason: &str,
) -> crate::QueuedWorkBatch {
    crate::store::QueuedWorkStore::enqueue_queued_work(
        store,
        crate::QueuedWorkBatchDraft::new(
            session_id.to_string(),
            crate::DeliveryPolicy::EarliestSafeBoundary,
            crate::SlotPolicy::Exclusive,
            vec![crate::QueuedWorkPayload::session_command(
                crate::SessionCommand::RefreshToolCatalog {
                    reason: reason.to_string(),
                },
            )],
        ),
    )
    .await
    .expect("enqueue session command")
}

#[tokio::test]
async fn session_config_change_hook_receives_context_window_updates() {
    let observed = Arc::new(tokio::sync::Mutex::new(Vec::new()));
    let observed_hook = Arc::clone(&observed);
    let plugin = Arc::new(RuntimeTestPluginFactory {
        build: Arc::new(move |_| {
            let observed = Arc::clone(&observed_hook);
            Ok(Arc::new(RuntimeTestPlugin {
                before_turn: None,
                checkpoint: None,
                tool_result_projector: None,
                runtime_event: Some(Arc::new(move |event| {
                    let observed = Arc::clone(&observed);
                    Box::pin(async move {
                        if let crate::plugin::PluginLifecycleEvent::SessionConfigChanged(ctx) =
                            event
                        {
                            observed.lock().await.push((ctx.previous, ctx.current));
                        }
                        Ok(())
                    })
                })),
                external_registrar: None,
            }))
        }),
    });
    let transport = mock_provider(Vec::new());
    let mut runtime = runtime_with_plugins(vec![plugin], transport).await;

    let alt_provider = TestProvider::builder()
        .kind("alt")
        .complete_error("alt provider not wired")
        .build();
    let alt_model =
        crate::ModelSpec::from_token_limits("alt-model", Default::default(), 123_456, None)
            .expect("valid model spec");
    runtime
        .update_session_config(
            Some(alt_provider.into_handle()),
            Some(alt_model.clone()),
            None,
        )
        .await;

    let changes = observed.lock().await;
    assert_eq!(changes.len(), 1);
    let (previous, current) = &changes[0];
    assert_eq!(previous.provider_id, "mock");
    assert_eq!(current.provider_id, "alt");
    assert_eq!(current.model.id, "alt-model");
    assert_ne!(
        previous.context_window_tokens(),
        current.context_window_tokens()
    );
}

#[tokio::test]
async fn turn_provider_override_does_not_persist_into_session_policy_or_agent_frame() {
    let mut runtime = runtime_with_plugins(Vec::new(), mock_provider(Vec::new())).await;
    let alt_provider = TestProvider::builder()
        .kind("alt")
        .complete(|_| async {
            Ok(LlmResponse {
                full_text: "alt response".to_string(),
                parts: vec![LlmOutputPart::Text {
                    text: "alt response".to_string(),
                    response_meta: None,
                }],
                response_metadata: Default::default(),
                ..LlmResponse::default()
            })
        })
        .build()
        .into_handle();
    let mut turn_context = crate::TurnContext::default();
    turn_context.set_provider(alt_provider);

    let turn = runtime
        .run_turn_assembled(
            TurnInput {
                items: vec![InputItem::Text {
                    text: "use override".to_string(),
                }],
                protocol_turn_options: None,
                trace_turn_id: None,
                protocol_extension: None,
                turn_context,
            },
            CancellationToken::new(),
            named_turn_scope("root", "provider-override-turn"),
        )
        .await
        .expect("turn");

    assert_eq!(turn.assistant_output.safe_text, "alt response");
    assert_eq!(turn.state.policy.recorded_provider_id(), "mock");
    assert_eq!(runtime.policy.recorded_provider_id(), "mock");
    assert_eq!(runtime.state.policy.recorded_provider_id(), "mock");
    assert!(
        runtime.state.agent_frames.iter().all(|frame| frame
            .assignment
            .policy
            .recorded_provider_id()
            == "mock")
    );
}

#[tokio::test]
async fn plugin_before_turn_can_abort_and_inject_messages() {
    let plugin = Arc::new(RuntimeTestPluginFactory {
        build: Arc::new(|_| {
            Ok(Arc::new(RuntimeTestPlugin {
                before_turn: Some(Arc::new(|_| {
                    Box::pin(async {
                        Ok(vec![
                            crate::PluginDirective::EnqueueMessages {
                                messages: vec![crate::PluginMessage::text(
                                    crate::MessageRole::System,
                                    "plugin preface",
                                )],
                            },
                            crate::PluginDirective::AbortTurn {
                                code: "blocked".to_string(),
                                message: "plugin stopped the turn".to_string(),
                            },
                        ])
                    })
                })),
                checkpoint: None,
                tool_result_projector: None,
                runtime_event: None,
                external_registrar: None,
            }))
        }),
    });
    let transport = mock_provider(Vec::new());
    let mut runtime = runtime_with_plugins(vec![plugin], transport).await;

    let turn = runtime
        .run_turn_assembled(
            TurnInput {
                items: vec![InputItem::Text {
                    text: "hello".to_string(),
                }],
                protocol_turn_options: None,
                trace_turn_id: None,
                protocol_extension: None,
                turn_context: crate::TurnContext::default(),
            },
            CancellationToken::new(),
            named_turn_scope("root", "plugin-extension-turn"),
        )
        .await
        .expect("turn");

    assert!(matches!(&turn.outcome, TurnOutcome::Stopped(_)));
    assert!(matches!(
        &turn.outcome,
        TurnOutcome::Stopped(TurnStop::PluginAbort)
    ));
    assert!(turn.errors.iter().any(|issue| issue.kind == "plugin"));
    assert!(
        active_conversation_messages(&turn.state)
            .iter()
            .any(|message| {
                message
                    .parts
                    .iter()
                    .any(|part| part.content.contains("plugin preface"))
            })
    );
}

#[tokio::test]
async fn normal_turn_stores_effective_user_text_in_state() {
    let transport = mock_provider(vec![MockCall {
        stream_events: Vec::new(),
        response: Ok(LlmResponse {
            full_text: "Done".to_string(),
            parts: vec![LlmOutputPart::Text {
                text: "Done".to_string(),
                response_meta: None,
            }],
            response_metadata: Default::default(),
            ..LlmResponse::default()
        }),
    }]);
    let mut runtime = runtime_with_plugins(Vec::new(), transport).await;

    let turn = runtime
        .run_turn_assembled(
            TurnInput {
                items: vec![InputItem::Text {
                    text: "/yolopush\n\n<skill>\nbody\n</skill>".to_string(),
                }],
                protocol_turn_options: None,
                trace_turn_id: None,
                protocol_extension: None,
                turn_context: crate::TurnContext::default(),
            },
            CancellationToken::new(),
            named_turn_scope("root", "skill-command-visibility-turn"),
        )
        .await
        .expect("turn");

    let read_model = turn.state.read_model();
    let user_message = read_model
        .messages
        .iter()
        .find(|message| message.role == MessageRole::User)
        .expect("user message");
    assert_eq!(
        user_message.parts.first().map(|part| part.content.as_str()),
        Some("/yolopush\n\n<skill>\nbody\n</skill>")
    );
}

#[tokio::test]
async fn retryable_llm_failures_exhaust_and_fail_turn() {
    let transport = mock_provider(vec![
        MockCall {
            stream_events: Vec::new(),
            response: Err(
                crate::llm::transport::LlmTransportError::new("provider unavailable")
                    .retryable(true)
                    .with_code("http_500"),
            ),
        },
        MockCall {
            stream_events: Vec::new(),
            response: Err(
                crate::llm::transport::LlmTransportError::new("provider unavailable")
                    .retryable(true)
                    .with_code("http_500"),
            ),
        },
        MockCall {
            stream_events: Vec::new(),
            response: Err(
                crate::llm::transport::LlmTransportError::new("provider unavailable")
                    .retryable(true)
                    .with_code("http_500"),
            ),
        },
        MockCall {
            stream_events: Vec::new(),
            response: Err(
                crate::llm::transport::LlmTransportError::new("provider unavailable")
                    .retryable(true)
                    .with_code("http_500"),
            ),
        },
    ]);
    let mut runtime = runtime_with_plugins(Vec::new(), transport).await;

    let turn = runtime
        .run_turn_assembled(
            TurnInput {
                items: vec![InputItem::Text {
                    text: "hello".to_string(),
                }],
                protocol_turn_options: None,
                trace_turn_id: None,
                protocol_extension: None,
                turn_context: crate::TurnContext::default(),
            },
            CancellationToken::new(),
            named_turn_scope("root", "retryable-error-turn"),
        )
        .await
        .expect("turn");

    assert!(matches!(&turn.outcome, TurnOutcome::Stopped(_)));
    assert!(matches!(
        &turn.outcome,
        TurnOutcome::Stopped(TurnStop::ProviderError)
    ));
    assert!(turn.errors.iter().any(|issue| issue.kind == "llm_provider"));
    assert!(
        turn.errors
            .iter()
            .any(|issue| issue.message.contains("provider unavailable"))
    );
    // The transport's typed retryable signal survives into the host-facing
    // issue instead of living only in trace records.
    assert!(
        turn.errors
            .iter()
            .any(|issue| issue.kind == "llm_provider" && issue.retryable == Some(true))
    );
    assert_eq!(turn.llm_calls.len(), 1);
    assert_eq!(turn.llm_calls[0].attempts.len(), 4);
}

#[tokio::test]
async fn provider_failure_surfaces_typed_kind_and_retryability_on_turn_issue() {
    // A 400 classifies as a non-retryable Validation failure, so the turn
    // fails on the first attempt with fully typed failure signals.
    let transport = mock_provider(vec![MockCall {
        stream_events: Vec::new(),
        response: Err(crate::llm::transport::LlmTransportError::new("bad request").with_code("400")),
    }]);
    let mut runtime = runtime_with_plugins(Vec::new(), transport).await;

    let turn = runtime
        .run_turn_assembled(
            TurnInput {
                items: vec![InputItem::Text {
                    text: "hello".to_string(),
                }],
                protocol_turn_options: None,
                trace_turn_id: None,
                protocol_extension: None,
                turn_context: crate::TurnContext::default(),
            },
            CancellationToken::new(),
            named_turn_scope("root", "typed-provider-failure-turn"),
        )
        .await
        .expect("turn");

    assert!(matches!(
        &turn.outcome,
        TurnOutcome::Stopped(TurnStop::ProviderError)
    ));
    let issue = turn
        .errors
        .iter()
        .find(|issue| issue.kind == "llm_provider")
        .expect("llm_provider issue");
    assert_eq!(issue.retryable, Some(false));
    assert_eq!(
        issue.provider_failure_kind,
        Some(crate::ProviderFailureKind::Validation)
    );
    assert_eq!(issue.code.as_deref(), Some("400"));
    assert_eq!(turn.llm_calls.len(), 1);
    assert_eq!(turn.llm_calls[0].attempts.len(), 1);
}

#[tokio::test]
async fn assembled_turn_reports_turn_timing_from_injected_clock() {
    let transport = mock_provider(vec![MockCall {
        stream_events: Vec::new(),
        response: Ok(LlmResponse {
            full_text: "Done".to_string(),
            parts: vec![LlmOutputPart::Text {
                text: "Done".to_string(),
                response_meta: None,
            }],
            response_metadata: Default::default(),
            ..LlmResponse::default()
        }),
    }]);
    let mut runtime = runtime_with_plugins(Vec::new(), transport).await;
    runtime.host.core.clock = Arc::new(ManualClock::new(4_242));

    let turn = runtime
        .run_turn_assembled(
            TurnInput {
                items: vec![InputItem::Text {
                    text: "hello".to_string(),
                }],
                protocol_turn_options: None,
                trace_turn_id: None,
                protocol_extension: None,
                turn_context: crate::TurnContext::default(),
            },
            CancellationToken::new(),
            named_turn_scope("root", "turn-timing-turn"),
        )
        .await
        .expect("turn");

    // `started_at_ms` is read from the injected wall clock, so a
    // deterministic clock yields a deterministic timestamp (the OS clock
    // would report the current epoch here).
    assert_eq!(turn.execution.started_at_ms, 4_242);
}

#[tokio::test]
async fn queued_checkpoint_input_commits_before_continuing_standard_turn() {
    let transport = mock_provider(vec![
        MockCall {
            stream_events: Vec::new(),
            response: Ok(LlmResponse {
                full_text: "First answer.".to_string(),
                parts: vec![LlmOutputPart::Text {
                    text: "First answer.".to_string(),
                    response_meta: None,
                }],
                response_metadata: Default::default(),
                ..LlmResponse::default()
            }),
        },
        MockCall {
            stream_events: Vec::new(),
            response: Ok(LlmResponse {
                full_text: "Second answer.".to_string(),
                parts: vec![LlmOutputPart::Text {
                    text: "Second answer.".to_string(),
                    response_meta: None,
                }],
                response_metadata: Default::default(),
                ..LlmResponse::default()
            }),
        },
    ]);
    let (mut runtime, store) = standard_runtime_with_transport_and_queue_store(transport).await;
    enqueue_turn_input_for_checkpoint(
        store.as_ref(),
        "root",
        "queued-checkpoint-turn",
        None,
        TurnInput::text("one more thing"),
    )
    .await;

    let turn = runtime
        .run_turn_assembled(
            TurnInput {
                items: vec![InputItem::Text {
                    text: "hello".to_string(),
                }],
                protocol_turn_options: None,
                trace_turn_id: None,
                protocol_extension: None,
                turn_context: crate::TurnContext::default(),
            },
            CancellationToken::new(),
            named_turn_scope("root", "queued-checkpoint-turn"),
        )
        .await
        .expect("turn");

    assert!(
        active_conversation_messages(&turn.state)
            .iter()
            .any(|message| {
                message.role == MessageRole::Assistant
                    && message
                        .parts
                        .iter()
                        .any(|part| part.content.contains("Second answer."))
            })
    );
    let admitted = active_conversation_messages(&turn.state)
        .into_iter()
        .filter(|message| {
            message.role == MessageRole::User
                && message
                    .parts
                    .iter()
                    .any(|part| part.content == "one more thing")
        })
        .collect::<Vec<_>>();
    assert_eq!(admitted.len(), 1);
    assert!(admitted[0].origin.is_none());
}

#[tokio::test]
async fn queued_checkpoint_input_preserves_images() {
    let requests = Arc::new(Mutex::new(Vec::new()));
    let captured_requests = Arc::clone(&requests);
    let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let captured_calls = Arc::clone(&calls);
    let transport = TestProvider::builder()
        .kind("mock")
        .complete(move |request| {
            let captured_requests = Arc::clone(&captured_requests);
            let captured_calls = Arc::clone(&captured_calls);
            async move {
                captured_requests
                    .lock()
                    .expect("request capture lock")
                    .push(request);
                let call = captured_calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                let text = if call == 0 {
                    "First answer."
                } else {
                    "Second answer."
                };
                Ok(LlmResponse {
                    full_text: text.to_string(),
                    parts: vec![LlmOutputPart::Text {
                        text: text.to_string(),
                        response_meta: None,
                    }],
                    response_metadata: Default::default(),
                    ..LlmResponse::default()
                })
            }
        })
        .build();
    let (mut runtime, store) = standard_runtime_with_transport_and_queue_store(transport).await;
    enqueue_turn_input_for_checkpoint(
        store.as_ref(),
        "root",
        "image-attachment-turn",
        None,
        TurnInput::text("see image").with_attachment(crate::AttachmentSource::inline(
            crate::MediaType::parse("image/png").unwrap(),
            vec![1, 2, 3],
        )),
    )
    .await;

    runtime
        .run_turn_assembled(
            TurnInput {
                items: vec![InputItem::Text {
                    text: "hello".to_string(),
                }],
                protocol_turn_options: None,
                trace_turn_id: None,
                protocol_extension: None,
                turn_context: crate::TurnContext::default(),
            },
            CancellationToken::new(),
            named_turn_scope("root", "image-attachment-turn"),
        )
        .await
        .expect("turn");

    let requests = requests.lock().expect("request capture lock").clone();
    assert_eq!(requests.len(), 2);
    assert!(requests[1].messages.iter().any(|message| {
        message.role == crate::llm::types::LlmRole::User
            && message
                .blocks
                .iter()
                .any(|block| matches!(block, crate::llm::types::LlmContentBlock::Attachment { .. }))
    }));
}

// Boundary: active-turn checkpoint input tests stay in `turns.rs` when they
// assert model prompt replay, plugin checkpoint hooks, injected-input stream
// events, image materialization, or persisted conversation projection. Runtime
// Scenarios own the host-level active-input redrive/cancel/queue invariants.
#[tokio::test]
async fn checkpoint_hook_can_inject_messages() {
    let plugin = Arc::new(RuntimeTestPluginFactory {
        build: Arc::new(|_| {
            Ok(Arc::new(RuntimeTestPlugin {
                before_turn: None,
                checkpoint: Some(Arc::new(|ctx| {
                    Box::pin(async move {
                        if ctx.checkpoint == crate::CheckpointKind::BeforeCompletion {
                            Ok(vec![crate::PluginDirective::EnqueueMessages {
                                messages: vec![crate::PluginMessage::text(
                                    crate::MessageRole::System,
                                    "checkpoint injected",
                                )],
                            }])
                        } else {
                            Ok(Vec::new())
                        }
                    })
                })),
                tool_result_projector: None,
                runtime_event: None,
                external_registrar: None,
            }))
        }),
    });
    let transport = mock_provider(vec![
        MockCall {
            stream_events: Vec::new(),
            response: Ok(LlmResponse {
                full_text: "First answer.".to_string(),
                parts: vec![LlmOutputPart::Text {
                    text: "First answer.".to_string(),
                    response_meta: None,
                }],
                response_metadata: Default::default(),
                ..LlmResponse::default()
            }),
        },
        MockCall {
            stream_events: Vec::new(),
            response: Ok(LlmResponse {
                full_text: "Second answer.".to_string(),
                parts: vec![LlmOutputPart::Text {
                    text: "Second answer.".to_string(),
                    response_meta: None,
                }],
                response_metadata: Default::default(),
                ..LlmResponse::default()
            }),
        },
    ]);
    let mut runtime = runtime_with_plugins(vec![plugin], transport).await;

    let turn = runtime
        .run_turn_assembled(
            TurnInput {
                items: vec![InputItem::Text {
                    text: "hello".to_string(),
                }],
                protocol_turn_options: None,
                trace_turn_id: None,
                protocol_extension: None,
                turn_context: crate::TurnContext::default(),
            },
            CancellationToken::new(),
            named_turn_scope("root", "plugin-action-turn"),
        )
        .await
        .expect("turn");

    assert!(
        active_conversation_messages(&turn.state)
            .iter()
            .any(|message| {
                message.role == MessageRole::System
                    && message
                        .parts
                        .iter()
                        .any(|part| part.content == "checkpoint injected")
            })
    );
}

#[tokio::test]
async fn checkpoint_plugin_abort_leaves_active_input_pending_without_application_evidence() {
    let plugin = Arc::new(RuntimeTestPluginFactory {
        build: Arc::new(|_| {
            Ok(Arc::new(RuntimeTestPlugin {
                before_turn: None,
                checkpoint: Some(Arc::new(|_| {
                    Box::pin(async {
                        Ok(vec![crate::PluginDirective::AbortTurn {
                            code: "checkpoint_rejected".to_string(),
                            message: "reject checkpoint delivery".to_string(),
                        }])
                    })
                })),
                tool_result_projector: None,
                runtime_event: None,
                external_registrar: None,
            }))
        }),
    });
    let transport = mock_provider(vec![MockCall {
        stream_events: Vec::new(),
        response: Ok(LlmResponse {
            full_text: "first".to_string(),
            parts: vec![LlmOutputPart::Text {
                text: "first".to_string(),
                response_meta: None,
            }],
            response_metadata: Default::default(),
            ..LlmResponse::default()
        }),
    }]);
    let store = Arc::new(RecordingStore::default());
    let runtime_store: Arc<dyn crate::store::RuntimePersistence> = store.clone();
    let mut runtime = runtime_with_plugins_and_tools_and_host_and_store(
        vec![plugin],
        Arc::new(EmptyTools),
        transport,
        test_host_config(),
        runtime_store,
    )
    .await;
    runtime.host.process_registry = Some(Arc::new(crate::TestLocalProcessRegistry::default()));
    let admitted = enqueue_turn_input_for_checkpoint(
        store.as_ref(),
        "root",
        "checkpoint-plugin-abort-turn",
        Some("host:checkpoint-plugin-abort".to_string()),
        TurnInput::text("must remain pending"),
    )
    .await;
    let turn_events = RecordingTurnEvents::default();

    let turn = runtime
        .stream_turn(
            TurnInput::text("hello"),
            TurnOptions::new(
                CancellationToken::new(),
                named_turn_scope("root", "checkpoint-plugin-abort-turn"),
            )
            .with_turn_events(&turn_events),
        )
        .await
        .expect("plugin-aborted turn assembles");

    assert!(
        matches!(turn.outcome, TurnOutcome::Stopped(_)),
        "checkpoint rejection must stop the turn: {:?}",
        turn.outcome
    );
    assert!(
        turn_events.snapshot().iter().all(|activity| !matches!(
            activity.event,
            crate::TurnEvent::QueuedInputAccepted { .. }
        )),
        "a rejected checkpoint must not emit live application evidence"
    );
    assert!(
        crate::store::TurnInputStore::list_turn_input_applications(store.as_ref(), "root")
            .await
            .expect("list rejected checkpoint applications")
            .is_empty(),
        "a rejected checkpoint must not persist application evidence"
    );
    assert!(
        crate::store::TurnInputStore::list_pending_turn_inputs(store.as_ref(), "root")
            .await
            .expect("list pending input after rejected checkpoint")
            .iter()
            .any(|input| input.input_id == admitted.input_id),
        "a rejected checkpoint input must remain claimable"
    );
    assert!(
        active_conversation_messages(&turn.state)
            .iter()
            .all(|message| message
                .parts
                .iter()
                .all(|part| part.content != "must remain pending")),
        "a rejected checkpoint input must not enter canonical history"
    );
}

#[tokio::test]
async fn checkpoint_attachment_failure_leaves_active_input_pending_without_application_evidence() {
    #[derive(Debug)]
    struct DenyHostCheckpointAttachments;

    impl crate::AttachmentSourcePolicy for DenyHostCheckpointAttachments {
        fn authorize(
            &self,
            producer: &crate::AttachmentProducer,
            _source: &crate::AttachmentSource,
        ) -> Result<(), crate::AttachmentSourcePolicyError> {
            if matches!(producer, crate::AttachmentProducer::Host) {
                return Err(crate::AttachmentSourcePolicyError {
                    producer: producer.clone(),
                    reason: "checkpoint attachment denied for test".to_string(),
                });
            }
            Ok(())
        }
    }

    let plugin = Arc::new(RuntimeTestPluginFactory {
        build: Arc::new(|_| {
            Ok(Arc::new(RuntimeTestPlugin {
                before_turn: None,
                checkpoint: Some(Arc::new(|_| {
                    Box::pin(async {
                        let mut message =
                            crate::PluginMessage::text(crate::MessageRole::System, "plugin upload");
                        message
                            .attachments
                            .push(crate::AttachmentSource::external_url(
                                crate::MediaType::parse("application/pdf")
                                    .expect("valid test media type"),
                                "https://example.test/checkpoint.pdf",
                            ));
                        Ok(vec![crate::PluginDirective::EnqueueMessages {
                            messages: vec![message],
                        }])
                    })
                })),
                tool_result_projector: None,
                runtime_event: None,
                external_registrar: None,
            }))
        }),
    });
    let transport = mock_provider(vec![MockCall {
        stream_events: Vec::new(),
        response: Ok(LlmResponse {
            full_text: "first".to_string(),
            parts: vec![LlmOutputPart::Text {
                text: "first".to_string(),
                response_meta: None,
            }],
            response_metadata: Default::default(),
            ..LlmResponse::default()
        }),
    }]);
    let store = Arc::new(RecordingStore::default());
    let runtime_store: Arc<dyn crate::store::RuntimePersistence> = store.clone();
    let mut runtime = runtime_with_plugins_and_tools_and_host_and_store(
        vec![plugin],
        Arc::new(EmptyTools),
        transport,
        test_host_config(),
        runtime_store,
    )
    .await;
    runtime.host.core.attachment_source_policy = Arc::new(DenyHostCheckpointAttachments);
    runtime.host.process_registry = Some(Arc::new(crate::TestLocalProcessRegistry::default()));
    let admitted = enqueue_turn_input_for_checkpoint(
        store.as_ref(),
        "root",
        "checkpoint-attachment-failure-turn",
        Some("host:checkpoint-attachment-failure".to_string()),
        TurnInput::text("must remain pending after attachment failure"),
    )
    .await;
    let turn_events = RecordingTurnEvents::default();

    let turn = runtime
        .stream_turn(
            TurnInput::text("hello"),
            TurnOptions::new(
                CancellationToken::new(),
                named_turn_scope("root", "checkpoint-attachment-failure-turn"),
            )
            .with_turn_events(&turn_events),
        )
        .await
        .expect("attachment-failed turn assembles");

    assert!(
        matches!(turn.outcome, TurnOutcome::Stopped(_)),
        "checkpoint attachment failure must stop the turn: {:?}",
        turn.outcome
    );
    assert!(
        turn_events.snapshot().iter().all(|activity| !matches!(
            activity.event,
            crate::TurnEvent::QueuedInputAccepted { .. }
        )),
        "a failed checkpoint attachment must not emit live application evidence"
    );
    assert!(
        crate::store::TurnInputStore::list_turn_input_applications(store.as_ref(), "root")
            .await
            .expect("list attachment-failed checkpoint applications")
            .is_empty(),
        "a failed checkpoint attachment must not persist application evidence"
    );
    assert!(
        crate::store::TurnInputStore::list_pending_turn_inputs(store.as_ref(), "root")
            .await
            .expect("list pending input after attachment failure")
            .iter()
            .any(|input| input.input_id == admitted.input_id),
        "an attachment-failed checkpoint input must remain claimable"
    );
    assert!(
        active_conversation_messages(&turn.state)
            .iter()
            .all(|message| message
                .parts
                .iter()
                .all(|part| part.content != "must remain pending after attachment failure")),
        "an attachment-failed checkpoint input must not enter canonical history"
    );
}

#[tokio::test]
async fn queued_checkpoint_input_accepts_and_persists_one_normal_user_message() {
    let transport = mock_provider(vec![
        MockCall {
            stream_events: Vec::new(),
            response: Ok(LlmResponse {
                full_text: "first".to_string(),
                parts: vec![LlmOutputPart::Text {
                    text: "first".to_string(),
                    response_meta: None,
                }],
                response_metadata: Default::default(),
                ..LlmResponse::default()
            }),
        },
        MockCall {
            stream_events: Vec::new(),
            response: Ok(LlmResponse {
                full_text: "answer".to_string(),
                parts: vec![LlmOutputPart::Text {
                    text: "answer".to_string(),
                    response_meta: None,
                }],
                response_metadata: Default::default(),
                ..LlmResponse::default()
            }),
        },
    ]);
    let (mut runtime, store) = standard_runtime_with_transport_and_queue_store(transport).await;
    enqueue_turn_input_for_checkpoint(
        store.as_ref(),
        "root",
        "injection-accepted-turn",
        Some("host:follow-up-id".to_string()),
        TurnInput::text("follow up"),
    )
    .await;
    let sink = RecordingSink::default();
    let assembled = runtime
        .stream_turn(
            TurnInput {
                items: vec![InputItem::Text {
                    text: "hello".to_string(),
                }],
                protocol_turn_options: None,
                trace_turn_id: None,
                protocol_extension: None,
                turn_context: crate::TurnContext::default(),
            },
            TurnOptions::new(
                CancellationToken::new(),
                named_turn_scope("root", "injection-accepted-turn"),
            )
            .with_events(&sink),
        )
        .await
        .expect("turn");

    let mut saw_injected_accept = false;
    for event in sink.snapshot() {
        if let crate::SessionStreamEvent::InjectedTurnInputAccepted { inputs, .. } = event {
            saw_injected_accept = inputs.iter().any(|input| {
                input.id.as_deref() == Some("follow-up-id")
                    && input.message.role == crate::MessageRole::User
                    && input.message.content == "follow up"
            });
        }
    }
    assert!(
        saw_injected_accept,
        "expected injected turn input accepted event"
    );

    let projected = active_conversation_messages(&assembled.state);
    let follow_up_count = projected
        .iter()
        .filter(|message| {
            message.role == crate::MessageRole::User
                && message.parts.iter().any(|part| part.content == "follow up")
        })
        .count();
    assert_eq!(
        follow_up_count, 1,
        "injected active-turn input must persist exactly once in history"
    );
    let follow_up = projected
        .iter()
        .find(|message| {
            message.role == crate::MessageRole::User
                && message.parts.iter().any(|part| part.content == "follow up")
        })
        .expect("committed injected input");
    assert!(
        follow_up.origin.is_none(),
        "injected input must use the normal user-message representation"
    );
    assert!(projected.iter().any(|message| {
        message.role == crate::MessageRole::User
            && message.parts.iter().any(|part| part.content == "hello")
    }));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn active_input_after_last_call_is_first_admitted_on_next_turn() {
    let requests = Arc::new(Mutex::new(Vec::new()));
    let captured_requests = Arc::clone(&requests);
    let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let captured_calls = Arc::clone(&calls);
    let transport = TestProvider::builder()
        .kind("after-last-call-ingress")
        .complete(move |request| {
            let captured_requests = Arc::clone(&captured_requests);
            let captured_calls = Arc::clone(&captured_calls);
            async move {
                captured_requests
                    .lock()
                    .expect("request capture")
                    .push(request.messages.clone());
                let text = match captured_calls.fetch_add(1, Ordering::SeqCst) {
                    0 => "first turn complete",
                    1 => "deferred input complete",
                    other => panic!("unexpected provider call {other}"),
                };
                Ok(LlmResponse {
                    full_text: text.to_string(),
                    parts: vec![LlmOutputPart::Text {
                        text: text.to_string(),
                        response_meta: None,
                    }],
                    response_metadata: Default::default(),
                    ..LlmResponse::default()
                })
            }
        })
        .build();
    let (mut runtime, store) = standard_runtime_with_transport_and_queue_store(transport).await;
    let entered = Arc::new(AtomicBool::new(false));
    let release = Arc::new(AtomicBool::new(false));
    runtime.set_turn_phase_probe(Arc::new(PauseAtFinalizeTurn {
        entered: Arc::clone(&entered),
        release: Arc::clone(&release),
    }));

    let first_turn = crate::task::spawn(async move {
        runtime
            .run_turn_assembled(
                TurnInput::text("first turn input"),
                CancellationToken::new(),
                named_turn_scope("root", "after-last-call-turn"),
            )
            .await
            .expect("first turn");
        runtime
    });
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        while !entered.load(Ordering::SeqCst) {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("turn reaches finalization after its last call");
    enqueue_turn_input_for_checkpoint(
        store.as_ref(),
        "root",
        "after-last-call-turn",
        Some("host:late-active".to_string()),
        TurnInput::text("late active input"),
    )
    .await;
    release.store(true, Ordering::SeqCst);
    let mut runtime = first_turn.await.expect("first turn task");

    let pending = crate::store::TurnInputStore::list_pending_turn_inputs(store.as_ref(), "root")
        .await
        .expect("deferred late input");
    assert_eq!(pending.len(), 1);
    assert!(matches!(
        pending[0].ingress,
        crate::TurnInputIngress::NextTurn
    ));
    assert_eq!(pending[0].state, crate::TurnInputState::DeferredNextTurn);

    runtime
        .stream_next_queued_work(TurnOptions::new(
            CancellationToken::new(),
            named_turn_scope("root", "late-active-next-turn"),
        ))
        .await
        .expect("drain deferred input")
        .expect("deferred input starts a turn");

    let requests = requests.lock().expect("request capture");
    assert_eq!(requests.len(), 2);
    assert_eq!(
        serde_json::to_string(&requests[1][1..]).expect("serialize next-turn first-call messages"),
        r#"[{"role":"User","blocks":[{"Text":{"text":"first turn input","response_meta":null,"cache_breakpoint":false}}]},{"role":"Assistant","blocks":[{"Text":{"text":"first turn complete","response_meta":null,"cache_breakpoint":false}}]},{"role":"User","blocks":[{"Text":{"text":"late active input","response_meta":null,"cache_breakpoint":false}}]}]"#
    );
}

// Boundary: Runtime Scenarios own command-only queue completion at the store
// layer. This full runtime test stays here to assert the public scheduler API:
// command-only work returns `None` rather than fabricating a turn.
#[tokio::test]
async fn command_only_queued_work_drain_completes_without_turn() {
    let (mut runtime, store) =
        standard_runtime_with_transport_and_queue_store(mock_provider(Vec::new())).await;
    let command = enqueue_session_command(store.as_ref(), "root", "test refresh").await;

    let drained = runtime
        .stream_next_queued_work(TurnOptions::new(
            CancellationToken::new(),
            named_turn_scope("root", "command-only-queue-drain"),
        ))
        .await
        .expect("command-only drain succeeds");

    assert!(drained.is_none());
    assert!(
        crate::store::QueuedWorkStore::list_queued_work(store.as_ref(), "root")
            .await
            .expect("list queue after command-only drain")
            .is_empty(),
        "command batch `{}` should be completed",
        command.batch_id
    );
}

// Boundary: these process-wake and active-checkpoint steering tests stay in
// `turns.rs` because they verify the full `LashRuntime` scheduler, provider
// prompt contents, cancellation path, and selected queued-work APIs. Runtime
// Scenarios cover the overlapping store-level queue/input/lease invariants,
// including active-checkpoint process-wake claim eligibility and the selected
// queued-work invariant that pending next-turn input is not consumed. The
// selected-drain case remains here because the owned behavior is the public
// `stream_selected_queued_work` API running a turn while preserving unrelated
// pending input.
#[tokio::test]
async fn next_turn_input_turn_claims_process_wake_at_active_checkpoint() {
    let requests = Arc::new(Mutex::new(Vec::new()));
    let captured_requests = Arc::clone(&requests);
    let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let captured_calls = Arc::clone(&calls);
    let transport = TestProvider::builder()
        .kind("mock")
        .requires_streaming(true)
        .complete(move |req| {
            let captured_requests = Arc::clone(&captured_requests);
            let captured_calls = Arc::clone(&captured_calls);
            async move {
                captured_requests
                    .lock()
                    .expect("request capture lock")
                    .push(req);
                let call = captured_calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                let text = if call == 0 {
                    "turn input response"
                } else {
                    "wake checkpoint response"
                };
                Ok(LlmResponse {
                    full_text: text.to_string(),
                    parts: vec![LlmOutputPart::Text {
                        text: text.to_string(),
                        response_meta: None,
                    }],
                    response_metadata: Default::default(),
                    ..LlmResponse::default()
                })
            }
        })
        .build();
    let (mut runtime, store) = standard_runtime_with_transport_and_queue_store(transport).await;
    let queued_input = enqueue_idle_turn_input(store.as_ref(), "root", "queued user input").await;
    let registry = runtime
        .host
        .process_registry
        .as_ref()
        .expect("process registry")
        .clone();
    let target_scope = crate::SessionScope::new("root");
    registry
        .register_process(
            crate::ProcessRegistration::new(
                "wake-after-user-input",
                crate::ProcessInput::External {
                    metadata: serde_json::Value::Null,
                },
                crate::RecoveryDisposition::ExternallyOwned,
                crate::ProcessProvenance::session(target_scope.clone()),
            )
            .with_extra_event_types([process_wake_event_type()]),
        )
        .await
        .expect("register wake process");
    let wake = append_process_wake_to_queue(
        registry.as_ref(),
        store.as_ref(),
        "wake-after-user-input",
        crate::ProcessEventAppendRequest::new(
            "process.wake",
            json!({
                "text": "wake should wait",
                "value": {
                    "status": "wake should wait"
                }
            }),
        )
        .with_wake_target_scope(target_scope),
    )
    .await;

    let drained = runtime
        .stream_next_queued_work(TurnOptions::new(
            CancellationToken::new(),
            named_turn_scope("root", "next-input-before-wake-drain"),
        ))
        .await
        .expect("queued drain succeeds")
        .expect("pending turn input drains first");

    assert_eq!(
        drained.assistant_output.safe_text,
        "wake checkpoint response"
    );
    assert!(
        crate::store::TurnInputStore::list_pending_turn_inputs(store.as_ref(), "root")
            .await
            .expect("pending inputs after drain")
            .is_empty(),
        "turn input `{}` should be completed",
        queued_input.input_id
    );
    assert!(
        crate::store::QueuedWorkStore::list_queued_work(store.as_ref(), "root")
            .await
            .expect("queued work after pending input drain")
            .is_empty(),
        "process wake `{}` should be claimed at the user-input turn checkpoint",
        wake.wake_id
    );

    let requests = requests.lock().expect("request capture lock").clone();
    assert_eq!(requests.len(), 2);
    assert!(request_contains_text(&requests[0], "queued user input"));
    assert!(!request_contains_text(&requests[0], "wake should wait"));
    assert!(request_contains_text(&requests[1], "queued user input"));
    assert!(request_contains_text(&requests[1], "wake should wait"));
}

#[tokio::test]
async fn selected_process_wake_drain_does_not_claim_pending_next_turn_input() {
    let transport = mock_provider(vec![MockCall {
        stream_events: Vec::new(),
        response: Ok(LlmResponse {
            full_text: "selected wake response".to_string(),
            parts: vec![LlmOutputPart::Text {
                text: "selected wake response".to_string(),
                response_meta: None,
            }],
            response_metadata: Default::default(),
            ..LlmResponse::default()
        }),
    }]);
    let (mut runtime, store) = standard_runtime_with_transport_and_queue_store(transport).await;
    let queued_input = enqueue_idle_turn_input(store.as_ref(), "root", "still pending user").await;
    let registry = runtime
        .host
        .process_registry
        .as_ref()
        .expect("process registry")
        .clone();
    let target_scope = crate::SessionScope::new("root");
    registry
        .register_process(
            crate::ProcessRegistration::new(
                "selected-wake",
                crate::ProcessInput::External {
                    metadata: serde_json::Value::Null,
                },
                crate::RecoveryDisposition::ExternallyOwned,
                crate::ProcessProvenance::session(target_scope.clone()),
            )
            .with_extra_event_types([process_wake_event_type()]),
        )
        .await
        .expect("register wake process");
    let wake = append_process_wake_to_queue(
        registry.as_ref(),
        store.as_ref(),
        "selected-wake",
        crate::ProcessEventAppendRequest::new(
            "process.wake",
            json!({
                "text": "selected wake",
                "value": {
                    "status": "selected wake"
                }
            }),
        )
        .with_wake_target_scope(target_scope),
    )
    .await;
    let wake_batch = crate::store::QueuedWorkStore::list_queued_work(store.as_ref(), "root")
        .await
        .expect("queued work before selected drain")
        .into_iter()
        .find(|batch| {
            batch.items.iter().any(|item| {
                matches!(
                    &item.payload,
                    crate::QueuedWorkPayload::ProcessWake { wake: queued_wake }
                        if queued_wake.wake_id == wake.wake_id
                )
            })
        })
        .expect("wake batch");

    let drained = runtime
        .stream_selected_queued_work(
            TurnOptions::new(
                CancellationToken::new(),
                named_turn_scope("root", "selected-wake-drain"),
            ),
            std::slice::from_ref(&wake_batch.batch_id),
        )
        .await
        .expect("selected wake drain succeeds")
        .expect("selected wake produces a turn");

    assert_eq!(drained.assistant_output.safe_text, "selected wake response");
    let pending_inputs =
        crate::store::TurnInputStore::list_pending_turn_inputs(store.as_ref(), "root")
            .await
            .expect("pending inputs after selected wake drain");
    assert_eq!(
        pending_inputs
            .iter()
            .map(|input| input.input_id.as_str())
            .collect::<Vec<_>>(),
        vec![queued_input.input_id.as_str()],
        "selected queued-work drains must not also claim pending user input"
    );
    assert!(
        crate::store::QueuedWorkStore::list_queued_work(store.as_ref(), "root")
            .await
            .expect("queued work after selected wake drain")
            .is_empty(),
        "selected wake batch should be completed"
    );
}

#[tokio::test]
async fn process_wake_claimed_at_checkpoint_is_completed_when_turn_is_cancelled() {
    let requests = Arc::new(Mutex::new(Vec::new()));
    let captured_requests = Arc::clone(&requests);
    let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let captured_calls = Arc::clone(&calls);
    let (wake_started_tx, wake_started_rx) = tokio::sync::oneshot::channel::<()>();
    let wake_started_tx = Arc::new(Mutex::new(Some(wake_started_tx)));
    let captured_wake_started_tx = Arc::clone(&wake_started_tx);
    let transport = TestProvider::builder()
        .kind("mock")
        .requires_streaming(true)
        .complete(move |req| {
            let captured_requests = Arc::clone(&captured_requests);
            let captured_calls = Arc::clone(&captured_calls);
            let captured_wake_started_tx = Arc::clone(&captured_wake_started_tx);
            async move {
                captured_requests
                    .lock()
                    .expect("request capture lock")
                    .push(req);
                let call = captured_calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                if call == 0 {
                    return Ok(LlmResponse {
                        full_text: "initial queued input response".to_string(),
                        parts: vec![LlmOutputPart::Text {
                            text: "initial queued input response".to_string(),
                            response_meta: None,
                        }],
                        response_metadata: Default::default(),
                        ..LlmResponse::default()
                    });
                }
                if let Some(tx) = captured_wake_started_tx
                    .lock()
                    .expect("wake started signal")
                    .take()
                {
                    let _ = tx.send(());
                }
                std::future::pending::<Result<LlmResponse, _>>().await
            }
        })
        .build();
    let (mut runtime, store) = standard_runtime_with_transport_and_queue_store(transport).await;
    let queued_input =
        enqueue_idle_turn_input(store.as_ref(), "root", "cancel with wake pending").await;
    let registry = runtime
        .host
        .process_registry
        .as_ref()
        .expect("process registry")
        .clone();
    let target_scope = crate::SessionScope::new("root");
    registry
        .register_process(
            crate::ProcessRegistration::new(
                "cancel-claimed-wake",
                crate::ProcessInput::External {
                    metadata: serde_json::Value::Null,
                },
                crate::RecoveryDisposition::ExternallyOwned,
                crate::ProcessProvenance::session(target_scope.clone()),
            )
            .with_extra_event_types([process_wake_event_type()]),
        )
        .await
        .expect("register wake process");
    let wake = append_process_wake_to_queue(
        registry.as_ref(),
        store.as_ref(),
        "cancel-claimed-wake",
        crate::ProcessEventAppendRequest::new(
            "process.wake",
            json!({
                "text": "wake cancelled in checkpoint",
                "value": {
                    "status": "wake cancelled in checkpoint"
                }
            }),
        )
        .with_wake_target_scope(target_scope),
    )
    .await;
    let cancel = CancellationToken::new();
    let cancel_after_wake_started = cancel.clone();
    let canceller = crate::task::spawn(async move {
        wake_started_rx
            .await
            .expect("wake provider call should start");
        cancel_after_wake_started.cancel();
    });

    let drained = tokio::time::timeout(
        std::time::Duration::from_secs(5),
        runtime.stream_next_queued_work(TurnOptions::new(
            cancel,
            named_turn_scope("root", "cancel-claimed-wake-drain"),
        )),
    )
    .await
    .expect("cancelled wake drain should finish")
    .expect("cancelled wake drain should not error")
    .expect("cancelled queued input turn should still assemble");
    canceller.await.expect("canceller task");

    assert!(matches!(
        drained.outcome,
        TurnOutcome::Stopped(TurnStop::Cancelled)
    ));
    assert!(
        crate::store::TurnInputStore::list_pending_turn_inputs(store.as_ref(), "root")
            .await
            .expect("pending inputs after cancellation")
            .is_empty(),
        "queued input `{}` should be completed by the cancelled turn",
        queued_input.input_id
    );
    assert!(
        crate::store::QueuedWorkStore::list_queued_work(store.as_ref(), "root")
            .await
            .expect("queued work after cancellation")
            .is_empty(),
        "claimed wake `{}` should be completed by the cancelled turn",
        wake.wake_id
    );
    assert!(
        runtime
            .stream_next_queued_work(TurnOptions::new(
                CancellationToken::new(),
                named_turn_scope("root", "after-cancel-claimed-wake-drain"),
            ))
            .await
            .expect("post-cancel drain should succeed")
            .is_none(),
        "neither the cancelled input nor the claimed wake should replay"
    );
    let requests = requests.lock().expect("request capture lock").clone();
    assert_eq!(requests.len(), 2);
    assert!(request_contains_text(
        &requests[0],
        "cancel with wake pending"
    ));
    assert!(!request_contains_text(
        &requests[0],
        "wake cancelled in checkpoint"
    ));
    assert!(request_contains_text(
        &requests[1],
        "cancel with wake pending"
    ));
    assert!(request_contains_text(
        &requests[1],
        "wake cancelled in checkpoint"
    ));
}

// Regression (ADR 0029): a long-running turn must keep the queued-work claim it
// already holds alive across a stall, no matter how short the lease TTL is.
// Queued-work batches are claimed at active-turn checkpoints under the session
// execution lease's generation; the claim carries no TTL of its own and is live
// exactly while that generation still holds the session lease. So a turn that
// claims a batch at one checkpoint, stalls past the (tiny) lease TTL -- here a
// slow provider call, while the session lease keeps renewing on its background
// cadence and preserves its generation -- then crosses another checkpoint
// re-runs `claim_ready_queued_work` under the *same* live generation, which can
// never self-steal its own rows. At finalization the original claim still owns
// its rows and the commit succeeds. Before generation fencing this failed with
// `QueuedWorkClaimExpired` because the claim expired under the stalled owner.
//
// This test must FAIL if anyone reintroduces time- or renewal-based claim
// invalidation. The turn is driven with an in-process `TurnInput` (not a
// store-claimed pending input) so the queued-work claim is the store claim
// under scrutiny; the equally-unrenewed turn-input claim is covered by the
// conformance generation-supersession cases.
#[tokio::test]
async fn long_turn_keeps_claims_live_across_session_lease_renewals() {
    // A tiny TTL keeps the test sub-second: the session execution lease renews
    // every `renew_interval` and keeps its generation live, so the queued-work
    // claim pinned to that generation survives the stalled provider call by
    // construction.
    let lease_ttl = std::time::Duration::from_millis(120);
    let provider_stall = std::time::Duration::from_millis(500);
    let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let stall_calls = Arc::clone(&calls);
    let transport = TestProvider::builder()
        .kind("mock")
        .requires_streaming(true)
        .complete(move |_request| {
            let stall_calls = Arc::clone(&stall_calls);
            async move {
                // Call 0 leaves the turn at a checkpoint that claims the wake;
                // the claimed wake is injected into call 1, and stalling there
                // pushes the live claim past its TTL before the next checkpoint.
                if stall_calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst) == 1 {
                    tokio::time::sleep(provider_stall).await;
                }
                Ok(LlmResponse {
                    full_text: "stalled turn response".to_string(),
                    parts: vec![LlmOutputPart::Text {
                        text: "stalled turn response".to_string(),
                        response_meta: None,
                    }],
                    response_metadata: Default::default(),
                    ..LlmResponse::default()
                })
            }
        })
        .build();

    let store = Arc::new(RecordingStore::default());
    let runtime_store: Arc<dyn crate::store::RuntimePersistence> = store.clone();
    let mut config = crate::RuntimeHostConfig::in_memory()
        .with_lease_timings(crate::LeaseTimings::from_ttl(lease_ttl).expect("valid lease timings"));
    config.providers.provider_resolver = Arc::new(crate::SingleProviderResolver::new(
        transport.clone().into_handle(),
    ));
    let mut runtime = runtime_with_plugins_and_tools_and_host_and_store(
        Vec::new(),
        Arc::new(EmptyTools),
        transport,
        crate::EmbeddedRuntimeHost::new(config),
        runtime_store,
    )
    .await;
    runtime.host.process_registry = Some(Arc::new(crate::TestLocalProcessRegistry::default()));

    // The wake batch is the queued work the turn claims mid-flight at an
    // active-turn checkpoint.
    let registry = runtime
        .host
        .process_registry
        .as_ref()
        .expect("process registry")
        .clone();
    let target_scope = crate::SessionScope::new("root");
    registry
        .register_process(
            crate::ProcessRegistration::new(
                "stalled-turn-wake",
                crate::ProcessInput::External {
                    metadata: serde_json::Value::Null,
                },
                crate::RecoveryDisposition::ExternallyOwned,
                crate::ProcessProvenance::session(target_scope.clone()),
            )
            .with_extra_event_types([process_wake_event_type()]),
        )
        .await
        .expect("register wake process");
    let wake = append_process_wake_to_queue(
        registry.as_ref(),
        store.as_ref(),
        "stalled-turn-wake",
        crate::ProcessEventAppendRequest::new(
            "process.wake",
            json!({
                "text": "queued work claimed mid turn",
                "value": {
                    "status": "queued work claimed mid turn"
                }
            }),
        )
        .with_wake_target_scope(target_scope),
    )
    .await;

    // Correct behavior: the turn's claim stays live under its session-lease
    // generation and commits, so the wake is completed exactly once. The second
    // checkpoint re-runs `claim_ready_queued_work` under the same live
    // generation and cannot re-steal the turn's own rows.
    let turn = tokio::time::timeout(
        std::time::Duration::from_secs(5),
        runtime.run_turn_assembled(
            TurnInput::text("long running user turn"),
            CancellationToken::new(),
            named_turn_scope("root", "long-turn-queued-work-claim"),
        ),
    )
    .await
    .expect("stalled turn should finish")
    .expect("stalled turn must commit without losing its queued-work claim");

    assert_eq!(turn.assistant_output.safe_text, "stalled turn response");
    assert!(
        crate::store::QueuedWorkStore::list_queued_work(store.as_ref(), "root")
            .await
            .expect("queued work after stalled turn")
            .is_empty(),
        "wake `{}` should be completed exactly once by the committing turn",
        wake.wake_id
    );
    assert!(
        runtime
            .stream_next_queued_work(TurnOptions::new(
                CancellationToken::new(),
                named_turn_scope("root", "after-long-turn-queued-work-claim"),
            ))
            .await
            .expect("post-turn queue check should succeed")
            .is_none(),
        "the committed wake `{}` must not replay after the turn",
        wake.wake_id
    );
}

// Boundary: command ordering tests stay in `turns.rs` when they assert public
// queued-work scheduler behavior across `stream_next_queued_work` calls,
// provider execution, and the API distinction between "ran a turn" and
// command-only `None`. Runtime Scenarios own the store-level command-before
// turn-work gate and command-only drain invariants.
#[tokio::test]
async fn queued_frame_switch_finishes_follow_on_before_next_queued_turn() {
    let store = Arc::new(RecordingStore::default());
    let captured_store = Arc::clone(&store);
    let requests = Arc::new(Mutex::new(Vec::new()));
    let captured_requests = Arc::clone(&requests);
    let call_index = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let captured_call_index = Arc::clone(&call_index);
    let transport = TestProvider::builder()
        .kind("mock")
        .requires_streaming(true)
        .complete(move |request| {
            let store = Arc::clone(&captured_store);
            let requests = Arc::clone(&captured_requests);
            let call_index = Arc::clone(&captured_call_index);
            async move {
                requests.lock().expect("request capture lock").push(request);
                match call_index.fetch_add(1, std::sync::atomic::Ordering::SeqCst) {
                    0 => {
                        enqueue_idle_turn_input(store.as_ref(), "root", "second queued turn").await;
                        Ok(LlmResponse {
                            parts: vec![LlmOutputPart::ToolCall {
                                call_id: "switch-call".to_string(),
                                tool_name: "terminal_tool_0".to_string(),
                                input_json: serde_json::json!({}).to_string(),
                                replay: None,
                            }],
                            response_metadata: Default::default(),
                            ..LlmResponse::default()
                        })
                    }
                    1 => Ok(LlmResponse {
                        full_text: "follow-on complete".to_string(),
                        parts: vec![LlmOutputPart::Text {
                            text: "follow-on complete".to_string(),
                            response_meta: None,
                        }],
                        response_metadata: Default::default(),
                        ..LlmResponse::default()
                    }),
                    2 => Ok(LlmResponse {
                        full_text: "second queued complete".to_string(),
                        parts: vec![LlmOutputPart::Text {
                            text: "second queued complete".to_string(),
                            response_meta: None,
                        }],
                        response_metadata: Default::default(),
                        ..LlmResponse::default()
                    }),
                    index => panic!("unexpected provider call {index}"),
                }
            }
        })
        .build();
    let runtime_store: Arc<dyn crate::store::RuntimePersistence> = store.clone();
    let mut runtime = runtime_with_plugins_and_tools_and_host_and_store(
        Vec::new(),
        Arc::new(TerminalControlTool {
            controls: vec![crate::ToolControl::SwitchAgentFrame {
                frame_id: "queued-follow-frame".to_string(),
                initial_nodes: Vec::new(),
                task: Some("run follow-on task".to_string()),
            }],
        }),
        transport,
        test_host_config(),
        runtime_store,
    )
    .await;
    runtime.host.process_registry = Some(Arc::new(crate::TestLocalProcessRegistry::default()));
    let first = enqueue_idle_turn_input(store.as_ref(), "root", "first queued turn").await;

    let first_result = runtime
        .stream_next_queued_work(TurnOptions::new(
            CancellationToken::new(),
            named_turn_scope("root", "queued-frame-chain"),
        ))
        .await
        .expect("queued frame chain succeeds")
        .expect("queued frame chain returns its terminal turn");

    assert_eq!(
        first_result.assistant_output.safe_text,
        "follow-on complete"
    );
    let pending_after_follow =
        crate::store::TurnInputStore::list_pending_turn_inputs(store.as_ref(), "root")
            .await
            .expect("pending inputs after frame follow");
    assert_eq!(pending_after_follow.len(), 1);
    assert_ne!(pending_after_follow[0].input_id, first.input_id);
    let requests_after_follow = requests.lock().expect("request capture lock").clone();
    assert_eq!(requests_after_follow.len(), 2);
    assert!(request_contains_text(
        &requests_after_follow[1],
        "run follow-on task"
    ));
    assert!(!request_contains_text(
        &requests_after_follow[1],
        "second queued turn"
    ));

    let second_result = runtime
        .stream_next_queued_work(TurnOptions::new(
            CancellationToken::new(),
            named_turn_scope("root", "second-queued-after-frame-chain"),
        ))
        .await
        .expect("second queued turn succeeds")
        .expect("second queued turn runs after the frame chain");

    assert_eq!(
        second_result.assistant_output.safe_text,
        "second queued complete"
    );
    assert!(
        crate::store::TurnInputStore::list_pending_turn_inputs(store.as_ref(), "root")
            .await
            .expect("pending inputs after second turn")
            .is_empty()
    );
    let requests = requests.lock().expect("request capture lock");
    assert_eq!(requests.len(), 3);
    assert!(request_contains_text(&requests[2], "second queued turn"));
}

#[tokio::test]
async fn committed_frame_handoff_survives_before_inline_claim_and_pump_recovers_it() {
    let store = Arc::new(RecordingStore::default());
    let runtime_store: Arc<dyn crate::store::RuntimePersistence> = store.clone();
    let call_index = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let captured_call_index = Arc::clone(&call_index);
    let transport = TestProvider::builder()
        .kind("mock")
        .requires_streaming(true)
        .complete(move |_| {
            let call_index = Arc::clone(&captured_call_index);
            async move {
                match call_index.fetch_add(1, Ordering::SeqCst) {
                    0 => Ok(LlmResponse {
                        parts: vec![LlmOutputPart::ToolCall {
                            call_id: "switch-call".to_string(),
                            tool_name: "terminal_tool_0".to_string(),
                            input_json: "{}".to_string(),
                            replay: None,
                        }],
                        response_metadata: Default::default(),
                        ..LlmResponse::default()
                    }),
                    1 => Ok(LlmResponse {
                        full_text: "recovered follow-on".to_string(),
                        parts: vec![LlmOutputPart::Text {
                            text: "recovered follow-on".to_string(),
                            response_meta: None,
                        }],
                        response_metadata: Default::default(),
                        ..LlmResponse::default()
                    }),
                    index => panic!("unexpected provider call {index}"),
                }
            }
        })
        .build();
    let mut runtime = runtime_with_plugins_and_tools_and_host_and_store(
        Vec::new(),
        Arc::new(TerminalControlTool {
            controls: vec![crate::ToolControl::SwitchAgentFrame {
                frame_id: "recovery-frame".to_string(),
                initial_nodes: Vec::new(),
                task: Some("recover this handoff".to_string()),
            }],
        }),
        transport,
        test_host_config(),
        runtime_store,
    )
    .await;
    runtime.host.process_registry = Some(Arc::new(crate::TestLocalProcessRegistry::default()));
    let inbound = enqueue_idle_turn_input(store.as_ref(), "root", "start switch").await;
    store.fail_next_exact_queue_claim();

    let first = runtime
        .stream_next_queued_work(TurnOptions::new(
            CancellationToken::new(),
            named_turn_scope("root", "handoff-crash-window"),
        ))
        .await;
    assert!(
        first.is_err(),
        "simulated post-commit claim failure must surface"
    );

    let inputs = crate::store::TurnInputStore::list_pending_turn_inputs(store.as_ref(), "root")
        .await
        .expect("list inbound input after switch commit");
    assert!(
        inputs
            .iter()
            .all(|input| input.input_id != inbound.input_id)
    );
    let queued = crate::store::QueuedWorkStore::list_pending_queued_work(store.as_ref(), "root")
        .await
        .expect("list committed handoff");
    assert_eq!(queued.len(), 1);
    assert!(matches!(
        &queued[0].items[0].payload,
        crate::QueuedWorkPayload::AgentFrameTask { frame_id, task, .. }
            if frame_id == "recovery-frame" && task == "recover this handoff"
    ));

    let recovered = runtime
        .stream_next_queued_work(TurnOptions::new(
            CancellationToken::new(),
            named_turn_scope("root", "handoff-pump-recovery"),
        ))
        .await
        .expect("pump recovery succeeds")
        .expect("pump runs durable handoff");
    assert_eq!(recovered.assistant_output.safe_text, "recovered follow-on");
    assert!(
        crate::store::QueuedWorkStore::list_queued_work(store.as_ref(), "root")
            .await
            .expect("queue after recovery")
            .is_empty()
    );
}

#[tokio::test]
async fn mid_chain_cancellation_commits_one_cancelled_terminal_and_settles_handoff() {
    let store = Arc::new(RecordingStore::default());
    let captured_store = Arc::clone(&store);
    let cancel = CancellationToken::new();
    let cancel_after_switch = cancel.clone();
    let transport = TestProvider::builder()
        .kind("mock")
        .requires_streaming(true)
        .complete(move |_| {
            let store = Arc::clone(&captured_store);
            let cancel = cancel_after_switch.clone();
            async move {
                store.set_claim_after_lease_validation_hook(Arc::new(move || cancel.cancel()));
                Ok(LlmResponse {
                    parts: vec![LlmOutputPart::ToolCall {
                        call_id: "switch-call".to_string(),
                        tool_name: "terminal_tool_0".to_string(),
                        input_json: "{}".to_string(),
                        replay: None,
                    }],
                    response_metadata: Default::default(),
                    ..LlmResponse::default()
                })
            }
        })
        .build();
    let runtime_store: Arc<dyn crate::store::RuntimePersistence> = store.clone();
    let mut runtime = runtime_with_plugins_and_tools_and_host_and_store(
        Vec::new(),
        Arc::new(TerminalControlTool {
            controls: vec![crate::ToolControl::SwitchAgentFrame {
                frame_id: "cancelled-frame".to_string(),
                initial_nodes: Vec::new(),
                task: Some("cancel before running".to_string()),
            }],
        }),
        transport,
        test_host_config(),
        runtime_store,
    )
    .await;
    runtime.host.process_registry = Some(Arc::new(crate::TestLocalProcessRegistry::default()));
    enqueue_idle_turn_input(store.as_ref(), "root", "start cancellable switch").await;

    let terminal = runtime
        .stream_next_queued_work(TurnOptions::new(
            cancel,
            named_turn_scope("root", "mid-chain-cancel"),
        ))
        .await
        .expect("cancelled chain assembles")
        .expect("cancelled terminal turn");
    assert!(matches!(
        terminal.outcome,
        TurnOutcome::Stopped(TurnStop::Cancelled)
    ));
    assert!(
        crate::store::QueuedWorkStore::list_queued_work(store.as_ref(), "root")
            .await
            .expect("queue after cancellation")
            .is_empty()
    );
}

#[tokio::test]
async fn claimed_normalization_failure_commits_and_settles_input() {
    #[derive(Debug)]
    struct DenyClaimedAttachments;

    impl crate::AttachmentSourcePolicy for DenyClaimedAttachments {
        fn authorize(
            &self,
            producer: &crate::AttachmentProducer,
            _source: &crate::AttachmentSource,
        ) -> Result<(), crate::AttachmentSourcePolicyError> {
            Err(crate::AttachmentSourcePolicyError {
                producer: producer.clone(),
                reason: "claimed attachment denied for test".to_string(),
            })
        }
    }

    let (mut runtime, store) =
        standard_runtime_with_transport_and_queue_store(mock_provider(Vec::new())).await;
    runtime.host.core.attachment_source_policy = Arc::new(DenyClaimedAttachments);
    let inbound = crate::store::TurnInputStore::enqueue_pending_turn_input(
        store.as_ref(),
        crate::PendingTurnInputDraft::new(
            "root",
            crate::TurnInputIngress::NextTurn,
            TurnInput::items([InputItem::attachment(
                crate::AttachmentSource::external_url(
                    crate::MediaType::parse("application/pdf").unwrap(),
                    "https://example.test/denied.pdf",
                ),
            )]),
        ),
    )
    .await
    .expect("enqueue invalid input");

    let terminal = runtime
        .stream_next_queued_work(TurnOptions::new(
            CancellationToken::new(),
            named_turn_scope("root", "invalid-claimed-input"),
        ))
        .await
        .expect("invalid input assembles")
        .expect("invalid terminal turn");
    assert!(matches!(
        terminal.outcome,
        TurnOutcome::Stopped(TurnStop::InvalidInput)
    ));
    let inputs = crate::store::TurnInputStore::list_pending_turn_inputs(store.as_ref(), "root")
        .await
        .expect("list completed invalid input");
    assert!(
        inputs
            .iter()
            .all(|input| input.input_id != inbound.input_id)
    );
}

#[tokio::test]
async fn claimed_plugin_abort_commits_and_settles_input() {
    let plugin = Arc::new(RuntimeTestPluginFactory {
        build: Arc::new(|_| {
            Ok(Arc::new(RuntimeTestPlugin {
                before_turn: Some(Arc::new(|_| {
                    Box::pin(async {
                        Ok(vec![crate::PluginDirective::AbortTurn {
                            code: "blocked".to_string(),
                            message: "plugin stopped claimed turn".to_string(),
                        }])
                    })
                })),
                checkpoint: None,
                tool_result_projector: None,
                runtime_event: None,
                external_registrar: None,
            }))
        }),
    });
    let store = Arc::new(RecordingStore::default());
    let runtime_store: Arc<dyn crate::store::RuntimePersistence> = store.clone();
    let mut runtime = runtime_with_plugins_and_tools_and_host_and_store(
        vec![plugin],
        Arc::new(EmptyTools),
        mock_provider(Vec::new()),
        test_host_config(),
        runtime_store,
    )
    .await;
    runtime.host.process_registry = Some(Arc::new(crate::TestLocalProcessRegistry::default()));
    let inbound = enqueue_idle_turn_input(store.as_ref(), "root", "abort this input").await;

    let terminal = runtime
        .stream_next_queued_work(TurnOptions::new(
            CancellationToken::new(),
            named_turn_scope("root", "claimed-plugin-abort"),
        ))
        .await
        .expect("plugin abort assembles")
        .expect("plugin abort terminal turn");
    assert!(matches!(
        terminal.outcome,
        TurnOutcome::Stopped(TurnStop::PluginAbort)
    ));
    let inputs = crate::store::TurnInputStore::list_pending_turn_inputs(store.as_ref(), "root")
        .await
        .expect("list completed aborted input");
    assert!(
        inputs
            .iter()
            .all(|input| input.input_id != inbound.input_id)
    );
}

#[tokio::test]
async fn stream_turn_tool_put_is_bound_to_the_turn_id() {
    const TURN_ID: &str = "attachment-owner-stream-turn";
    let store = Arc::new(RecordingStore::default());
    let runtime_store: Arc<dyn crate::RuntimePersistence> = store.clone();
    let mut runtime = runtime_with_plugins_and_tools_and_host_and_store(
        Vec::new(),
        Arc::new(AttachmentPutTool),
        attachment_put_transport(),
        test_host_config(),
        runtime_store,
    )
    .await;

    runtime
        .stream_turn(
            TurnInput::text("store an attachment"),
            TurnOptions::new(CancellationToken::new(), named_turn_scope("root", TURN_ID)),
        )
        .await
        .expect("stream turn succeeds");

    assert_turn_owned_attachment(store.as_ref(), TURN_ID);
}

#[tokio::test]
async fn stream_prepared_turn_tool_put_is_bound_to_the_turn_id() {
    const TURN_ID: &str = "attachment-owner-prepared-turn";
    let store = Arc::new(RecordingStore::default());
    let runtime_store: Arc<dyn crate::RuntimePersistence> = store.clone();
    let mut runtime = runtime_with_plugins_and_tools_and_host_and_store(
        Vec::new(),
        Arc::new(AttachmentPutTool),
        attachment_put_transport(),
        test_host_config(),
        runtime_store,
    )
    .await;
    let messages = crate::MessageSequence::from_owned(vec![Message {
        id: "prepared-attachment-user".to_string(),
        role: MessageRole::User,
        parts: vec![Part {
            id: "prepared-attachment-user.p0".to_string(),
            kind: PartKind::Text,
            content: "store an attachment".to_string(),
            attachment: None,
            tool_call_id: None,
            tool_name: None,
            tool_replay: None,
            prune_state: PruneState::Intact,
            reasoning_meta: None,
            response_meta: None,
        }]
        .into(),
        origin: None,
    }]);

    runtime
        .stream_prepared_turn(
            messages,
            None,
            None,
            None,
            crate::TurnContext::default(),
            Vec::new(),
            TURN_ID.to_string(),
            1,
            &NoopEventSink,
            &NoopTurnActivitySink,
            named_turn_scope("root", TURN_ID),
            CancellationToken::new(),
            None,
            None,
        )
        .await
        .expect("prepared stream turn succeeds");

    assert_turn_owned_attachment(store.as_ref(), TURN_ID);
}

#[tokio::test]
async fn stream_prepared_turn_follows_agent_frame_switch() {
    let call_index = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let captured_call_index = Arc::clone(&call_index);
    let transport = TestProvider::builder()
        .kind("mock")
        .requires_streaming(true)
        .complete(move |_| {
            let call_index = Arc::clone(&captured_call_index);
            async move {
                match call_index.fetch_add(1, Ordering::SeqCst) {
                    0 => Ok(LlmResponse {
                        parts: vec![LlmOutputPart::ToolCall {
                            call_id: "prepared-switch".to_string(),
                            tool_name: "terminal_tool_0".to_string(),
                            input_json: "{}".to_string(),
                            replay: None,
                        }],
                        response_metadata: Default::default(),
                        ..LlmResponse::default()
                    }),
                    1 => Ok(LlmResponse {
                        full_text: "prepared follow-on complete".to_string(),
                        parts: vec![LlmOutputPart::Text {
                            text: "prepared follow-on complete".to_string(),
                            response_meta: None,
                        }],
                        response_metadata: Default::default(),
                        ..LlmResponse::default()
                    }),
                    index => panic!("unexpected provider call {index}"),
                }
            }
        })
        .build();
    let mut runtime = runtime_with_plugins_and_tools(
        Vec::new(),
        Arc::new(TerminalControlTool {
            controls: vec![crate::ToolControl::SwitchAgentFrame {
                frame_id: "prepared-follow-frame".to_string(),
                initial_nodes: Vec::new(),
                task: Some("finish prepared follow-on".to_string()),
            }],
        }),
        transport,
    )
    .await;
    let messages = crate::MessageSequence::from_owned(vec![Message {
        id: "prepared-user".to_string(),
        role: MessageRole::User,
        parts: vec![Part {
            id: "prepared-user.p0".to_string(),
            kind: PartKind::Text,
            content: "prepared input".to_string(),
            attachment: None,
            tool_call_id: None,
            tool_name: None,
            tool_replay: None,
            prune_state: PruneState::Intact,
            reasoning_meta: None,
            response_meta: None,
        }]
        .into(),
        origin: None,
    }]);

    let terminal = runtime
        .stream_prepared_turn(
            messages,
            None,
            None,
            None,
            crate::TurnContext::default(),
            Vec::new(),
            "prepared-chain".to_string(),
            1,
            &NoopEventSink,
            &NoopTurnActivitySink,
            named_turn_scope("root", "prepared-chain"),
            CancellationToken::new(),
            None,
            None,
        )
        .await
        .expect("prepared logical turn succeeds");
    assert_eq!(
        terminal.assistant_output.safe_text,
        "prepared follow-on complete"
    );
    assert_eq!(call_index.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn retained_lease_reuses_graph_and_reacquisition_reloads() {
    let call_index = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let captured_call_index = Arc::clone(&call_index);
    let transport = TestProvider::builder()
        .kind("mock")
        .requires_streaming(true)
        .complete(move |_| {
            let call_index = Arc::clone(&captured_call_index);
            async move {
                match call_index.fetch_add(1, Ordering::SeqCst) {
                    0 => Ok(LlmResponse {
                        parts: vec![LlmOutputPart::ToolCall {
                            call_id: "resident-switch".to_string(),
                            tool_name: "terminal_tool_0".to_string(),
                            input_json: "{}".to_string(),
                            replay: None,
                        }],
                        response_metadata: Default::default(),
                        ..LlmResponse::default()
                    }),
                    1 => Ok(LlmResponse {
                        full_text: "retained lease follow-on".to_string(),
                        parts: vec![LlmOutputPart::Text {
                            text: "retained lease follow-on".to_string(),
                            response_meta: None,
                        }],
                        response_metadata: Default::default(),
                        ..LlmResponse::default()
                    }),
                    2 => Ok(LlmResponse {
                        full_text: "reacquired lease turn".to_string(),
                        parts: vec![LlmOutputPart::Text {
                            text: "reacquired lease turn".to_string(),
                            response_meta: None,
                        }],
                        response_metadata: Default::default(),
                        ..LlmResponse::default()
                    }),
                    index => panic!("unexpected provider call {index}"),
                }
            }
        })
        .build();
    let store = Arc::new(RecordingStore::default());
    let runtime_store: Arc<dyn crate::store::RuntimePersistence> = store.clone();
    let mut runtime = runtime_with_plugins_and_tools_and_host_and_store(
        Vec::new(),
        Arc::new(TerminalControlTool {
            controls: vec![crate::ToolControl::SwitchAgentFrame {
                frame_id: "resident-follow-frame".to_string(),
                initial_nodes: Vec::new(),
                task: Some("continue on retained lease".to_string()),
            }],
        }),
        transport,
        test_host_config(),
        runtime_store,
    )
    .await;
    runtime.host.process_registry = Some(Arc::new(crate::TestLocalProcessRegistry::default()));

    let run = runtime
        .stream_turn_with_agent_frames(
            TurnInput::text("start retained lease chain"),
            TurnOptions::new(
                CancellationToken::new(),
                named_turn_scope("root", "resident-chain"),
            ),
        )
        .await
        .expect("retained lease chain succeeds");
    assert_eq!(run.turns.len(), 2);
    assert_eq!(
        store.load_session_count(),
        1,
        "the follow-on physical turn must reuse the graph committed under the retained lease"
    );
    for node in &run.turns[0].state.session_graph.nodes {
        assert!(
            run.turns[1]
                .state
                .session_graph
                .find_node(&node.node_id)
                .is_some(),
            "the skip path lost committed graph node {}",
            node.node_id
        );
    }

    runtime
        .stream_turn(
            TurnInput::text("turn after lease release"),
            TurnOptions::new(
                CancellationToken::new(),
                named_turn_scope("root", "reacquired-turn"),
            ),
        )
        .await
        .expect("turn after lease reacquisition succeeds");
    assert_eq!(
        store.load_session_count(),
        2,
        "a released and reacquired lease generation must force a graph reload"
    );
}

#[tokio::test]
async fn lost_lease_and_reacquisition_force_graph_reloads() {
    let call_index = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let captured_call_index = Arc::clone(&call_index);
    let transport = TestProvider::builder()
        .kind("mock")
        .requires_streaming(true)
        .complete(move |_| {
            let call_index = Arc::clone(&captured_call_index);
            async move {
                let index = call_index.fetch_add(1, Ordering::SeqCst);
                let response = match index {
                    0 => LlmResponse {
                        parts: vec![LlmOutputPart::ToolCall {
                            call_id: "lost-lease-switch".to_string(),
                            tool_name: "terminal_tool_0".to_string(),
                            input_json: "{}".to_string(),
                            replay: None,
                        }],
                        response_metadata: Default::default(),
                        ..LlmResponse::default()
                    },
                    1 => LlmResponse {
                        full_text: "reacquired lease turn".to_string(),
                        parts: vec![LlmOutputPart::Text {
                            text: "reacquired lease turn".to_string(),
                            response_meta: None,
                        }],
                        response_metadata: Default::default(),
                        ..LlmResponse::default()
                    },
                    index => panic!("unexpected provider call {index}"),
                };
                Ok(response)
            }
        })
        .build();
    let clock = Arc::new(ManualClock::new(1_000));
    let store_clock: Arc<dyn crate::Clock> = clock.clone();
    let store = Arc::new(RecordingStore::with_clock(store_clock));
    let runtime_store: Arc<dyn crate::store::RuntimePersistence> = store.clone();
    let host_clock: Arc<dyn crate::Clock> = clock.clone();
    let mut config = crate::RuntimeHostConfig::in_memory().with_clock(host_clock);
    config.providers.provider_resolver = Arc::new(crate::SingleProviderResolver::new(
        transport.clone().into_handle(),
    ));
    let mut runtime = runtime_with_plugins_and_tools_and_host_and_store(
        Vec::new(),
        Arc::new(TerminalControlTool {
            controls: vec![crate::ToolControl::SwitchAgentFrame {
                frame_id: "lost-lease-follow-frame".to_string(),
                initial_nodes: Vec::new(),
                task: Some("continue after retained commit".to_string()),
            }],
        }),
        transport,
        crate::EmbeddedRuntimeHost::new(config),
        runtime_store,
    )
    .await;
    runtime.host.process_registry = Some(Arc::new(crate::TestLocalProcessRegistry::default()));
    runtime.set_turn_phase_probe(Arc::new(ExpireLeaseAfterRetainedCommit::new(Arc::clone(
        &clock,
    ))));

    let err = runtime
        .stream_turn_with_agent_frames(
            TurnInput::text("lose the retained lease"),
            TurnOptions::new(
                CancellationToken::new(),
                named_turn_scope("root", "lost-retained-lease"),
            ),
        )
        .await
        .expect_err("the follow-on commit must observe the lost lease");
    assert_eq!(err.code, crate::RuntimeErrorCode::SessionExecutionLeaseLost);
    assert_eq!(
        store.load_session_count(),
        1,
        "the fenced handoff claim must reject the lost lease before a follow-on turn starts"
    );

    runtime
        .stream_turn(
            TurnInput::text("turn after lease loss"),
            TurnOptions::new(
                CancellationToken::new(),
                named_turn_scope("root", "turn-after-lease-loss"),
            ),
        )
        .await
        .expect("turn after lease loss and reacquisition succeeds");
    assert_eq!(
        store.load_session_count(),
        2,
        "a lease acquired after loss must reload the graph again"
    );
}

#[tokio::test]
async fn frame_switch_limit_commits_terminal_error_and_settles_claim() {
    let switch_count = crate::runtime::logical_turn::MAX_AGENT_FRAME_SWITCHES;
    let call_index = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let captured_call_index = Arc::clone(&call_index);
    let transport = TestProvider::builder()
        .kind("mock")
        .requires_streaming(true)
        .complete(move |_| {
            let call_index = Arc::clone(&captured_call_index);
            async move {
                let index = call_index.fetch_add(1, Ordering::SeqCst);
                Ok(LlmResponse {
                    parts: vec![LlmOutputPart::ToolCall {
                        call_id: format!("switch-{index}"),
                        tool_name: format!("terminal_tool_{index}"),
                        input_json: "{}".to_string(),
                        replay: None,
                    }],
                    response_metadata: Default::default(),
                    ..LlmResponse::default()
                })
            }
        })
        .build();
    let controls = (0..switch_count)
        .map(|index| crate::ToolControl::SwitchAgentFrame {
            frame_id: format!("bounded-frame-{index}"),
            initial_nodes: Vec::new(),
            task: Some(format!("continue bounded chain {index}")),
        })
        .collect();
    let store = Arc::new(RecordingStore::default());
    let runtime_store: Arc<dyn crate::store::RuntimePersistence> = store.clone();
    let mut runtime = runtime_with_plugins_and_tools_and_host_and_store(
        Vec::new(),
        Arc::new(TerminalControlTool { controls }),
        transport,
        test_host_config(),
        runtime_store,
    )
    .await;
    runtime.host.process_registry = Some(Arc::new(crate::TestLocalProcessRegistry::default()));
    let inbound = enqueue_idle_turn_input(store.as_ref(), "root", "start bounded chain").await;

    let terminal = runtime
        .stream_next_queued_work(TurnOptions::new(
            CancellationToken::new(),
            named_turn_scope("root", "bounded-frame-chain"),
        ))
        .await
        .expect("bounded chain terminalizes")
        .expect("bounded chain returns terminal turn");
    assert!(matches!(
        terminal.outcome,
        TurnOutcome::Stopped(TurnStop::RuntimeError)
    ));
    assert!(
        terminal
            .errors
            .iter()
            .any(|issue| { issue.message.contains("exceeded the limit of") })
    );
    assert_eq!(call_index.load(Ordering::SeqCst), switch_count);
    assert!(
        crate::store::QueuedWorkStore::list_queued_work(store.as_ref(), "root")
            .await
            .expect("queue after bounded chain")
            .is_empty()
    );
    let inputs = crate::store::TurnInputStore::list_pending_turn_inputs(store.as_ref(), "root")
        .await
        .expect("inputs after bounded chain");
    assert!(
        inputs
            .iter()
            .all(|input| input.input_id != inbound.input_id)
    );
}

#[tokio::test]
async fn leading_session_command_drains_before_queued_turn() {
    let transport = mock_provider(vec![MockCall {
        stream_events: Vec::new(),
        response: Ok(LlmResponse {
            full_text: "queued answer".to_string(),
            parts: vec![LlmOutputPart::Text {
                text: "queued answer".to_string(),
                response_meta: None,
            }],
            response_metadata: Default::default(),
            ..LlmResponse::default()
        }),
    }]);
    let clock = Arc::new(ManualClock::new(1_000));
    let store_clock: Arc<dyn crate::Clock> = clock.clone();
    let (mut runtime, store) =
        standard_runtime_with_transport_and_queue_store_clock(transport, store_clock).await;
    let command = enqueue_session_command(store.as_ref(), "root", "refresh before turn").await;
    clock.advance_ms(1);
    let turn = enqueue_idle_turn_input(store.as_ref(), "root", "user turn").await;
    let turn_events = RecordingTurnEvents::default();

    let drained = runtime
        .stream_next_queued_work(
            TurnOptions::new(
                CancellationToken::new(),
                named_turn_scope("root", "command-before-turn-drain"),
            )
            .with_turn_events(&turn_events),
        )
        .await
        .expect("queued drain succeeds")
        .expect("queued turn runs after command");

    assert_eq!(drained.assistant_output.safe_text, "queued answer");
    assert!(
        crate::store::QueuedWorkStore::list_queued_work(store.as_ref(), "root")
            .await
            .expect("list queue after command plus turn")
            .is_empty(),
        "command `{}` and turn input `{}` should both be completed",
        command.batch_id,
        turn.input_id
    );
}

#[tokio::test]
async fn later_session_command_does_not_jump_earlier_queued_turn() {
    let transport = mock_provider(vec![MockCall {
        stream_events: Vec::new(),
        response: Ok(LlmResponse {
            full_text: "first turn answer".to_string(),
            parts: vec![LlmOutputPart::Text {
                text: "first turn answer".to_string(),
                response_meta: None,
            }],
            response_metadata: Default::default(),
            ..LlmResponse::default()
        }),
    }]);
    let clock = Arc::new(ManualClock::new(2_000));
    let store_clock: Arc<dyn crate::Clock> = clock.clone();
    let (mut runtime, store) =
        standard_runtime_with_transport_and_queue_store_clock(transport, store_clock).await;
    let turn = enqueue_idle_turn_input(store.as_ref(), "root", "first user turn").await;
    clock.advance_ms(1);
    let command = enqueue_session_command(store.as_ref(), "root", "refresh after turn").await;

    let drained = runtime
        .stream_next_queued_work(TurnOptions::new(
            CancellationToken::new(),
            named_turn_scope("root", "turn-before-command-drain"),
        ))
        .await
        .expect("queued turn drain succeeds")
        .expect("first queued turn runs");

    assert_eq!(drained.assistant_output.safe_text, "first turn answer");
    assert_eq!(
        crate::store::QueuedWorkStore::list_queued_work(store.as_ref(), "root")
            .await
            .expect("list queue after first turn")
            .iter()
            .map(|batch| batch.batch_id.as_str())
            .collect::<Vec<_>>(),
        vec![command.batch_id.as_str()],
        "later command should remain after turn `{}` runs",
        turn.input_id
    );

    let command_only = runtime
        .stream_next_queued_work(TurnOptions::new(
            CancellationToken::new(),
            named_turn_scope("root", "later-command-drain"),
        ))
        .await
        .expect("later command drain succeeds");
    assert!(command_only.is_none());
    assert!(
        crate::store::QueuedWorkStore::list_queued_work(store.as_ref(), "root")
            .await
            .expect("list queue after later command")
            .is_empty()
    );
}

// Boundary: Runtime Scenarios own the idle queue claim and completion
// invariant. This full runtime test stays here because it verifies the
// app-facing queued-work turn event, prompt projection, and blank-history
// suppression produced by `stream_next_queued_work`.
#[tokio::test]
async fn pending_process_wake_drains_into_idle_queued_turn_as_turn_event() {
    let requests = Arc::new(Mutex::new(Vec::new()));
    let captured_requests = Arc::clone(&requests);
    let transport = TestProvider::builder()
        .kind("mock")
        .requires_streaming(true)
        .complete(move |req| {
            let captured_requests = Arc::clone(&captured_requests);
            async move {
                captured_requests
                    .lock()
                    .expect("request capture lock")
                    .push(req);
                Ok(LlmResponse {
                    full_text: "saw event".to_string(),
                    parts: vec![LlmOutputPart::Text {
                        text: "saw event".to_string(),
                        response_meta: None,
                    }],
                    response_metadata: Default::default(),
                    ..LlmResponse::default()
                })
            }
        })
        .build();
    let (mut runtime, store) = standard_runtime_with_transport_and_queue_store(transport).await;
    let registry = runtime
        .host
        .process_registry
        .as_ref()
        .expect("process registry")
        .clone();
    let target_scope = crate::SessionScope::new("root");
    let process_caused_by = crate::CausalRef::SessionNode {
        session_id: "root".to_string(),
        node_id: "trigger:button".to_string(),
    };
    registry
        .register_process(
            crate::ProcessRegistration::new(
                "wake-proc",
                crate::ProcessInput::External {
                    metadata: serde_json::Value::Null,
                },
                crate::RecoveryDisposition::ExternallyOwned,
                crate::ProcessProvenance::session(target_scope.clone())
                    .with_caused_by(Some(process_caused_by.clone())),
            )
            .with_extra_event_types([process_wake_event_type()]),
        )
        .await
        .expect("register wake process");
    let wake = append_process_wake_to_queue(
        registry.as_ref(),
        store.as_ref(),
        "wake-proc",
        crate::ProcessEventAppendRequest::new(
            "process.wake",
            json!({
                "text": "deploy complete",
                "value": {
                    "status": "deploy complete"
                }
            }),
        )
        .with_wake_target_scope(target_scope.clone()),
    )
    .await;

    let turn_events = RecordingTurnEvents::default();
    runtime
        .stream_next_queued_work(
            TurnOptions::new(
                CancellationToken::new(),
                named_turn_scope("root", "queued-work-started-turn"),
            )
            .with_turn_events(&turn_events),
        )
        .await
        .expect("turn")
        .expect("queued turn");

    let events = turn_events.snapshot();
    let queued_started = events
        .iter()
        .position(|activity| matches!(&activity.event, crate::TurnEvent::QueuedWorkStarted { .. }))
        .expect("queued work started event");
    let model_started = events
        .iter()
        .position(|activity| {
            matches!(
                &activity.event,
                crate::TurnEvent::ModelRequestStarted { .. }
            )
        })
        .expect("model request started event");
    assert!(
        queued_started < model_started,
        "queued work should be announced before model output starts"
    );
    let crate::TurnEvent::QueuedWorkStarted {
        boundary,
        batch_ids,
        causes,
    } = &events[queued_started].event
    else {
        panic!("expected queued work started event");
    };
    assert_eq!(*boundary, crate::QueuedWorkClaimBoundary::Idle);
    assert_eq!(batch_ids.len(), 1);
    assert!(causes.iter().any(|cause| {
        cause.event_type == "process.wake"
            && cause.id == wake.wake_id
            && cause.text.contains("deploy complete")
    }));

    let requests = {
        let guard = requests.lock().expect("request capture lock");
        guard.clone()
    };
    assert_eq!(requests.len(), 1);
    let request = &requests[0];
    let message_text = |message: &crate::llm::types::LlmMessage| {
        message
            .blocks
            .iter()
            .filter_map(|block| match block {
                crate::llm::types::LlmContentBlock::Text { text, .. } => Some(text.as_ref()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n")
    };
    let turn_event_user_messages = request
        .messages
        .iter()
        .filter(|message| {
            message.role == crate::llm::types::LlmRole::User
                && message_text(message).contains("=== TURN EVENTS ===")
        })
        .collect::<Vec<_>>();
    assert_eq!(turn_event_user_messages.len(), 1);
    let turn_event_text = message_text(turn_event_user_messages[0]);
    assert!(turn_event_text.contains("Background process wake"));
    assert!(turn_event_text.contains("deploy complete"));
    assert!(request.messages.iter().all(|message| {
        message.role != crate::llm::types::LlmRole::System
            || !message_text(message).contains("deploy complete")
    }));
    assert!(request.messages.iter().all(|message| {
        message.role != crate::llm::types::LlmRole::User || !message.is_blank()
    }));
    assert!(
        active_conversation_messages(&runtime.state)
            .iter()
            .all(|message| {
                !(message.role == crate::MessageRole::User
                    && message
                        .parts
                        .iter()
                        .all(|part| part.content.trim().is_empty()))
            }),
        "empty wake turns must not synthesize blank user history"
    );
    assert!(
        crate::store::QueuedWorkStore::list_queued_work(store.as_ref(), "root")
            .await
            .expect("queued work after commit")
            .is_empty()
    );
}

#[tokio::test]
async fn cancelled_provider_stream_does_not_commit_partial_output() {
    let (delta_sent_tx, delta_sent_rx) = tokio::sync::oneshot::channel::<()>();
    let delta_sent_tx = Arc::new(Mutex::new(Some(delta_sent_tx)));
    let transport = TestProvider::builder()
        .kind("mock")
        .requires_streaming(true)
        .complete({
            let delta_sent_tx = Arc::clone(&delta_sent_tx);
            move |request| {
                let delta_sent_tx = Arc::clone(&delta_sent_tx);
                async move {
                    let stream = request
                        .stream_events
                        .expect("streaming runtime should request provider stream events");
                    stream.send(LlmStreamEvent::Delta("partial provider text".to_string()));
                    if let Some(tx) = delta_sent_tx.lock().expect("delta signal").take() {
                        let _ = tx.send(());
                    }
                    std::future::pending::<Result<LlmResponse, LlmTransportError>>().await
                }
            }
        })
        .build();
    let mut runtime = standard_runtime_with_transport(transport).await;
    let cancel = CancellationToken::new();
    let turn_cancel = cancel.clone();
    let turn_events = RecordingTurnEvents::default();
    let turn_events_for_task = turn_events.clone();
    let turn = crate::task::spawn(async move {
        runtime
            .stream_turn(
                TurnInput::text("cancel after partial stream"),
                TurnOptions::new(
                    turn_cancel,
                    named_turn_scope("root", "cancel-partial-provider-stream"),
                )
                .with_turn_events(&turn_events_for_task),
            )
            .await
    });

    delta_sent_rx
        .await
        .expect("provider should emit the visible partial text");
    cancel.cancel();
    let assembled = turn
        .await
        .expect("turn task")
        .expect("cancelled turn should assemble");

    assert!(matches!(
        assembled.outcome,
        TurnOutcome::Stopped(TurnStop::Cancelled)
    ));
    assert!(assembled.assistant_output.safe_text.is_empty());
    assert!(assembled.assistant_output.raw_text.is_empty());
    assert!(
        turn_events.snapshot().iter().any(|activity| matches!(
            &activity.event,
            TurnEvent::AssistantProseDelta { text } if text.as_ref() == "partial provider text"
        )),
        "partial provider text should remain observable only as live turn activity"
    );
    assert!(
        active_conversation_messages(&assembled.state)
            .iter()
            .filter(|message| message.role == MessageRole::Assistant)
            .flat_map(|message| message.parts.iter())
            .all(|part| !part.content.contains("partial provider text")),
        "cancelled streamed partial must not be committed to read-view history"
    );
}

#[tokio::test]
async fn truncated_retry_resets_partial_tool_calls_and_retains_failed_attempt_usage() {
    let attempts = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let transport = TestProvider::builder()
        .kind("openai-compatible")
        .requires_streaming(true)
        .options(crate::ProviderOptions {
            reliability: crate::provider::ProviderReliability::default()
                .max_attempts(2)
                .base_delay_ms(0)
                .max_delay_ms(0),
            ..crate::ProviderOptions::default()
        })
        .complete({
            let attempts = Arc::clone(&attempts);
            move |request| {
                let attempt = attempts.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                async move {
                    let stream = request.stream_events.expect("stream events");
                    if attempt == 0 {
                        let usage = LlmUsage {
                            input_tokens: 11,
                            output_tokens: 2,
                            ..LlmUsage::default()
                        };
                        stream.send(LlmStreamEvent::Part(LlmOutputPart::ToolCall {
                            call_id: "partial-call".to_string(),
                            tool_name: "must_not_run".to_string(),
                            input_json: "{\"unfinished\":".to_string(),
                            replay: None,
                        }));
                        stream.send(LlmStreamEvent::Usage(usage.clone()));
                        return Err(LlmTransportError::new("Stream ended without finish_reason")
                            .with_kind(crate::ProviderFailureKind::Stream)
                            .with_code("stream_ended_before_finish_reason")
                            .retryable(true)
                            .with_partial_response(LlmResponse {
                                parts: vec![LlmOutputPart::ToolCall {
                                    call_id: "partial-call".to_string(),
                                    tool_name: "must_not_run".to_string(),
                                    input_json: "{\"unfinished\":".to_string(),
                                    replay: None,
                                }],
                                usage,
                                provider_usage: Some(serde_json::json!({
                                    "prompt_tokens": 11,
                                    "completion_tokens": 2
                                })),
                                response_metadata: Default::default(),
                                ..LlmResponse::default()
                            }));
                    }

                    stream.send(LlmStreamEvent::Delta("success".to_string()));
                    Ok(LlmResponse {
                        full_text: "success".to_string(),
                        parts: vec![LlmOutputPart::Text {
                            text: "success".to_string(),
                            response_meta: None,
                        }],
                        terminal_reason: crate::LlmTerminalReason::Stop,
                        response_metadata: Default::default(),
                        ..LlmResponse::default()
                    })
                }
            }
        })
        .build();
    let mut runtime = standard_runtime_with_transport(transport).await;

    let assembled = runtime
        .stream_turn(
            TurnInput::text("retry a truncated stream"),
            TurnOptions::new(
                CancellationToken::new(),
                named_turn_scope("root", "truncated-stream-retry"),
            ),
        )
        .await
        .expect("retry succeeds");

    assert_eq!(attempts.load(std::sync::atomic::Ordering::SeqCst), 2);
    assert_eq!(assembled.assistant_output.safe_text, "success");
    assert!(assembled.tool_calls.is_empty());
    assert!(
        active_conversation_messages(&assembled.state)
            .iter()
            .flat_map(|message| message.parts.iter())
            .all(|part| !part.content.contains("must_not_run"))
    );
    let failed_attempt = &assembled.llm_calls[0].attempts[0];
    assert_eq!(failed_attempt.outcome, crate::AttemptOutcome::Interrupted);
    assert_eq!(
        failed_attempt
            .usage
            .as_ref()
            .map(|usage| usage.input_tokens),
        Some(11)
    );
}

// Boundary: advisory-lease tests stay in `turns.rs` because they exercise live
// `LashRuntime` lease acquisition, public scheduling, turn phase probes, and
// provider suspension. Runtime Scenarios own persistence-level head-CAS and
// queue/input claim invariants; these tests own the facade scheduler response.
#[tokio::test]
async fn foreground_turn_proceeds_when_advisory_session_lane_is_held() {
    let transport = mock_provider(vec![MockCall {
        stream_events: Vec::new(),
        response: Ok(LlmResponse {
            full_text: "foreground proceeded".to_string(),
            parts: vec![LlmOutputPart::Text {
                text: "foreground proceeded".to_string(),
                response_meta: None,
            }],
            response_metadata: Default::default(),
            ..LlmResponse::default()
        }),
    }]);
    let (mut runtime, store) = standard_runtime_with_transport_and_queue_store(transport).await;
    let owner = lease_owner("other-runtime");
    let held_lease = crate::store::SessionExecutionLeaseStore::try_claim_session_execution_lease(
        store.as_ref(),
        "root",
        &owner,
        60_000,
    )
    .await
    .expect("claim session execution lease")
    .acquired()
    .expect("session execution lease");

    let assembled = runtime
        .run_turn_assembled(
            TurnInput::text("foreground may proceed"),
            CancellationToken::new(),
            named_turn_scope("root", "foreground-advisory-lane-turn"),
        )
        .await
        .expect("an advisory lease holder must not select a durable error branch");

    assert_eq!(assembled.assistant_output.safe_text, "foreground proceeded");
    crate::store::SessionExecutionLeaseStore::release_session_execution_lease(
        store.as_ref(),
        &held_lease.completion(),
    )
    .await
    .expect("release held session execution lease");
}

#[tokio::test]
async fn idle_queued_work_noops_without_claiming_when_session_lane_is_held() {
    let transport = mock_provider(vec![MockCall {
        stream_events: Vec::new(),
        response: Ok(LlmResponse {
            full_text: "queued answer".to_string(),
            parts: vec![LlmOutputPart::Text {
                text: "queued answer".to_string(),
                response_meta: None,
            }],
            response_metadata: Default::default(),
            ..LlmResponse::default()
        }),
    }]);
    let (mut runtime, store) = standard_runtime_with_transport_and_queue_store(transport).await;
    enqueue_idle_turn_input(store.as_ref(), "root", "queued while busy").await;
    let owner = lease_owner("foreground-runtime");
    let held_lease = crate::store::SessionExecutionLeaseStore::try_claim_session_execution_lease(
        store.as_ref(),
        "root",
        &owner,
        60_000,
    )
    .await
    .expect("claim session execution lease")
    .acquired()
    .expect("session execution lease");

    let busy_result = runtime
        .stream_next_queued_work(TurnOptions::new(
            CancellationToken::new(),
            named_turn_scope("root", "queued-busy-turn"),
        ))
        .await
        .expect("busy queued drain should not error");

    assert!(
        busy_result.is_none(),
        "idle queued drain must no-op while another owner holds the session lane"
    );
    assert_eq!(
        crate::store::TurnInputStore::list_pending_turn_inputs(store.as_ref(), "root")
            .await
            .expect("queued turn input while busy")
            .len(),
        1,
        "busy drain must not consume queued turn input"
    );

    crate::store::SessionExecutionLeaseStore::release_session_execution_lease(
        store.as_ref(),
        &held_lease.completion(),
    )
    .await
    .expect("release held session execution lease");
    let drained = runtime
        .stream_next_queued_work(TurnOptions::new(
            CancellationToken::new(),
            named_turn_scope("root", "queued-after-busy-turn"),
        ))
        .await
        .expect("queued drain after release should succeed")
        .expect("queued turn should still be pending after busy no-op");

    assert_eq!(drained.assistant_output.safe_text, "queued answer");
    assert!(
        crate::store::TurnInputStore::list_pending_turn_inputs(store.as_ref(), "root")
            .await
            .expect("queued turn input after drain")
            .is_empty()
    );
}

#[tokio::test]
async fn session_command_claim_lease_expiry_surfaces_session_execution_lease_lost() {
    let clock = Arc::new(StepExpiryClock::new(1_000));
    let store_clock: Arc<dyn crate::Clock> = clock.clone();
    let store = Arc::new(RecordingStore::with_clock(store_clock));
    let runtime_store: Arc<dyn crate::store::RuntimePersistence> = store.clone();
    let mut runtime = runtime_with_plugins_and_tools_and_host_and_store(
        Vec::new(),
        Arc::new(EmptyTools),
        mock_provider(Vec::new()),
        test_host_config(),
        runtime_store,
    )
    .await;
    let owner = lease_owner("session-command-drain-test");
    let lease = crate::store::SessionExecutionLeaseStore::try_claim_session_execution_lease(
        store.as_ref(),
        "root",
        &owner,
        crate::LeaseTimings::default().ttl_ms(),
    )
    .await
    .expect("claim session execution lease")
    .acquired()
    .expect("session execution lease");
    clock.expire_after_timestamp_calls(0);

    let err = runtime
        .drain_next_session_command(&lease.fence())
        .await
        .expect_err("expired session command claim lease must fail as lease lost");

    assert_eq!(err.code, crate::RuntimeErrorCode::SessionExecutionLeaseLost);
}

#[tokio::test]
async fn idle_queued_work_claim_lease_expiry_surfaces_session_execution_lease_lost() {
    let clock = Arc::new(StepExpiryClock::new(1_000));
    let store_clock: Arc<dyn crate::Clock> = clock.clone();
    let store = Arc::new(RecordingStore::with_clock(store_clock));
    let runtime_store: Arc<dyn crate::store::RuntimePersistence> = store.clone();
    let mut runtime = runtime_with_plugins_and_tools_and_host_and_store(
        Vec::new(),
        Arc::new(EmptyTools),
        mock_provider(Vec::new()),
        test_host_config(),
        runtime_store,
    )
    .await;
    clock.expire_after_timestamp_calls(3);

    let err = runtime
        .stream_next_queued_work(TurnOptions::new(
            CancellationToken::new(),
            named_turn_scope("root", "idle-claim-lease-expiry-turn"),
        ))
        .await
        .expect_err("expired idle queued-work claim lease must fail as lease lost");

    assert_eq!(err.code, crate::RuntimeErrorCode::SessionExecutionLeaseLost);
}

#[tokio::test]
async fn advisory_lease_loss_does_not_stop_foreground_turn_before_final_commit() {
    let clock = Arc::new(ManualClock::new(1_000));
    let store_clock: Arc<dyn crate::Clock> = clock.clone();
    let store = Arc::new(RecordingStore::with_clock(store_clock));
    let runtime_store: Arc<dyn crate::store::RuntimePersistence> = store.clone();
    let (provider_started_tx, provider_started_rx) = tokio::sync::oneshot::channel();
    let (provider_continue_tx, provider_continue_rx) = tokio::sync::oneshot::channel();
    let provider_started_tx = Arc::new(Mutex::new(Some(provider_started_tx)));
    let provider_continue_rx = Arc::new(Mutex::new(Some(provider_continue_rx)));
    let transport = TestProvider::builder()
        .kind("mock")
        .requires_streaming(true)
        .complete({
            let provider_started_tx = Arc::clone(&provider_started_tx);
            let provider_continue_rx = Arc::clone(&provider_continue_rx);
            move |_request| {
                let provider_started_tx = Arc::clone(&provider_started_tx);
                let provider_continue_rx = Arc::clone(&provider_continue_rx);
                async move {
                    if let Some(tx) = provider_started_tx
                        .lock()
                        .expect("provider started sender")
                        .take()
                    {
                        let _ = tx.send(());
                    }
                    let rx = provider_continue_rx
                        .lock()
                        .expect("provider continue receiver")
                        .take()
                        .expect("provider continue receiver available");
                    let _ = rx.await;
                    Ok(LlmResponse {
                        full_text: "committed under head CAS".to_string(),
                        parts: vec![LlmOutputPart::Text {
                            text: "committed under head CAS".to_string(),
                            response_meta: None,
                        }],
                        response_metadata: Default::default(),
                        ..LlmResponse::default()
                    })
                }
            }
        })
        .build();
    let host_clock: Arc<dyn crate::Clock> = clock.clone();
    let mut config = crate::RuntimeHostConfig::in_memory().with_clock(host_clock);
    config.providers.provider_resolver = Arc::new(crate::SingleProviderResolver::new(
        transport.clone().into_handle(),
    ));
    let mut runtime = runtime_with_plugins_and_tools_and_host_and_store(
        Vec::new(),
        Arc::new(EmptyTools),
        transport,
        crate::EmbeddedRuntimeHost::new(config),
        runtime_store,
    )
    .await;

    let turn = crate::task::spawn(async move {
        runtime
            .run_turn_assembled(
                TurnInput::text("lease can be lost"),
                CancellationToken::new(),
                named_turn_scope("root", "lease-loss-turn"),
            )
            .await
    });
    provider_started_rx
        .await
        .expect("provider should start after session lease acquisition");
    let commits_before_lease_loss = *store.runtime_commit_count.lock().expect("commit count");

    clock.advance_ms(crate::LeaseTimings::default().ttl_ms() + 1);
    let owner = lease_owner("stealing-runtime");
    let stolen = crate::store::SessionExecutionLeaseStore::try_claim_session_execution_lease(
        store.as_ref(),
        "root",
        &owner,
        60_000,
    )
    .await
    .expect("steal expired session execution lease")
    .acquired()
    .expect("expired session execution lease should be claimable");
    provider_continue_tx
        .send(())
        .expect("provider should still be waiting");

    let assembled = turn
        .await
        .expect("foreground turn task")
        .expect("advisory lease loss must not reject the turn");
    assert_eq!(
        assembled.assistant_output.safe_text,
        "committed under head CAS"
    );
    assert!(
        *store.runtime_commit_count.lock().expect("commit count") > commits_before_lease_loss,
        "the current-head turn must checkpoint and commit despite advisory lease loss"
    );
    crate::store::SessionExecutionLeaseStore::release_session_execution_lease(
        store.as_ref(),
        &stolen.completion(),
    )
    .await
    .expect("release stolen session execution lease");
}

#[tokio::test]
async fn renewal_failure_mid_turn_does_not_select_a_durable_branch() {
    let lease_ttl = std::time::Duration::from_millis(120);
    let clock = Arc::new(ManualClock::new(1_000));
    let store_clock: Arc<dyn crate::Clock> = clock.clone();
    let store = Arc::new(RecordingStore::with_clock(store_clock));
    let runtime_store: Arc<dyn crate::store::RuntimePersistence> = store.clone();
    let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let captured_calls = Arc::clone(&calls);
    let (provider_stalled_tx, provider_stalled_rx) = tokio::sync::oneshot::channel::<()>();
    let provider_stalled_tx = Arc::new(Mutex::new(Some(provider_stalled_tx)));
    let captured_provider_stalled_tx = Arc::clone(&provider_stalled_tx);
    let (provider_continue_tx, provider_continue_rx) = tokio::sync::oneshot::channel::<()>();
    let provider_continue_rx = Arc::new(Mutex::new(Some(provider_continue_rx)));
    let captured_provider_continue_rx = Arc::clone(&provider_continue_rx);
    let transport = TestProvider::builder()
        .kind("mock")
        .requires_streaming(true)
        .complete(move |_request| {
            let captured_calls = Arc::clone(&captured_calls);
            let captured_provider_stalled_tx = Arc::clone(&captured_provider_stalled_tx);
            let captured_provider_continue_rx = Arc::clone(&captured_provider_continue_rx);
            async move {
                if captured_calls.fetch_add(1, Ordering::SeqCst) == 0 {
                    return Ok(LlmResponse {
                        full_text: "reach the active-turn checkpoint".to_string(),
                        parts: vec![LlmOutputPart::Text {
                            text: "reach the active-turn checkpoint".to_string(),
                            response_meta: None,
                        }],
                        response_metadata: Default::default(),
                        ..LlmResponse::default()
                    });
                }
                if let Some(tx) = captured_provider_stalled_tx
                    .lock()
                    .expect("provider stalled sender")
                    .take()
                {
                    let _ = tx.send(());
                }
                let rx = captured_provider_continue_rx
                    .lock()
                    .expect("provider continue receiver")
                    .take()
                    .expect("provider continue receiver available");
                let _ = rx.await;
                Ok(LlmResponse {
                    full_text: "stale claim completion".to_string(),
                    parts: vec![LlmOutputPart::Text {
                        text: "stale claim completion".to_string(),
                        response_meta: None,
                    }],
                    response_metadata: Default::default(),
                    ..LlmResponse::default()
                })
            }
        })
        .build();
    let host_clock: Arc<dyn crate::Clock> = clock.clone();
    let mut config = crate::RuntimeHostConfig::in_memory()
        .with_clock(host_clock)
        .with_lease_timings(crate::LeaseTimings::from_ttl(lease_ttl).expect("valid timings"));
    config.providers.provider_resolver = Arc::new(crate::SingleProviderResolver::new(
        transport.clone().into_handle(),
    ));
    let mut runtime = runtime_with_plugins_and_tools_and_host_and_store(
        Vec::new(),
        Arc::new(EmptyTools),
        transport,
        crate::EmbeddedRuntimeHost::new(config),
        runtime_store,
    )
    .await;
    runtime.host.process_registry = Some(Arc::new(crate::TestLocalProcessRegistry::default()));

    enqueue_idle_turn_input(store.as_ref(), "root", "input held when the lease is lost").await;
    let registry = runtime
        .host
        .process_registry
        .as_ref()
        .expect("process registry")
        .clone();
    let target_scope = crate::SessionScope::new("root");
    registry
        .register_process(
            crate::ProcessRegistration::new(
                "lease-loss-claimed-wake",
                crate::ProcessInput::External {
                    metadata: serde_json::Value::Null,
                },
                crate::RecoveryDisposition::ExternallyOwned,
                crate::ProcessProvenance::session(target_scope.clone()),
            )
            .with_extra_event_types([process_wake_event_type()]),
        )
        .await
        .expect("register wake process");
    append_process_wake_to_queue(
        registry.as_ref(),
        store.as_ref(),
        "lease-loss-claimed-wake",
        crate::ProcessEventAppendRequest::new(
            "process.wake",
            json!({
                "text": "queued work held when the lease is lost",
                "value": { "status": "lease lost" }
            }),
        )
        .with_wake_target_scope(target_scope),
    )
    .await;

    let turn = crate::task::spawn(async move {
        runtime
            .stream_next_queued_work(TurnOptions::new(
                CancellationToken::new(),
                named_turn_scope("root", "renewal-failure-mid-turn"),
            ))
            .await
    });
    provider_stalled_rx
        .await
        .expect("provider should stall after both ingress claims are held");
    let renewals_before_loss = store.session_execution_lease_renewal_count();

    clock.advance_ms(lease_ttl.as_millis() as u64 + 1);
    let successor = lease_owner("renewal-failure-successor");
    let successor_lease =
        crate::store::SessionExecutionLeaseStore::try_claim_session_execution_lease(
            store.as_ref(),
            "root",
            &successor,
            60_000,
        )
        .await
        .expect("claim expired session execution lease")
        .acquired()
        .expect("expired lease should be acquired by successor");
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        while store.session_execution_lease_renewal_count() == renewals_before_loss {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("renewal task must observe the expired predecessor fence");
    provider_continue_tx
        .send(())
        .expect("provider should still be waiting");

    let assembled = tokio::time::timeout(std::time::Duration::from_secs(5), turn)
        .await
        .expect("turn should finish")
        .expect("turn task")
        .expect("advisory renewal failure must not select a durable error branch")
        .expect("claimed queued work must produce a turn");
    assert!(
        store.session_execution_lease_renewal_count() > renewals_before_loss,
        "the live renewal task must observe the expired predecessor fence"
    );
    assert_eq!(
        assembled.assistant_output.safe_text,
        "stale claim completion"
    );
    assert_eq!(
        store.abandoned_claim_counts(),
        (0, 0),
        "advisory lease loss must not mutate durable claim state out of band"
    );

    crate::store::SessionExecutionLeaseStore::release_session_execution_lease(
        store.as_ref(),
        &successor_lease.completion(),
    )
    .await
    .expect("release successor lease");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn cancellation_sealed_before_renewal_failure_remains_evidence_bearing_cancelled() {
    let lease_ttl = std::time::Duration::from_millis(120);
    let store = Arc::new(RecordingStore::default());
    let runtime_store: Arc<dyn crate::store::RuntimePersistence> = store.clone();
    let (provider_started_tx, provider_started_rx) = tokio::sync::oneshot::channel::<()>();
    let provider_started_tx = Arc::new(Mutex::new(Some(provider_started_tx)));
    let captured_provider_started_tx = Arc::clone(&provider_started_tx);
    let transport = TestProvider::builder()
        .kind("mock")
        .requires_streaming(true)
        .complete(move |_request| {
            let captured_provider_started_tx = Arc::clone(&captured_provider_started_tx);
            async move {
                if let Some(tx) = captured_provider_started_tx
                    .lock()
                    .expect("provider started sender")
                    .take()
                {
                    let _ = tx.send(());
                }
                std::future::pending::<Result<LlmResponse, _>>().await
            }
        })
        .build();
    let mut config = crate::RuntimeHostConfig::in_memory()
        .with_lease_timings(crate::LeaseTimings::from_ttl(lease_ttl).expect("valid timings"));
    config.providers.provider_resolver = Arc::new(crate::SingleProviderResolver::new(
        transport.clone().into_handle(),
    ));
    let turn_driver = crate::TurnWorkDriver::new(Arc::clone(&config.control.effect_host));
    let mut runtime = runtime_with_plugins_and_tools_and_host_and_store(
        Vec::new(),
        Arc::new(EmptyTools),
        transport,
        crate::EmbeddedRuntimeHost::new(config),
        runtime_store,
    )
    .await;
    let effect_loop_ended = Arc::new(AtomicBool::new(false));
    let release_effect_loop = Arc::new(AtomicBool::new(false));
    runtime.set_turn_phase_probe(Arc::new(PauseAfterEffectLoop {
        entered: Arc::clone(&effect_loop_ended),
        release: Arc::clone(&release_effect_loop),
    }));

    let turn_id = "cancel-before-renewal-failure";
    let turn = crate::task::spawn(async move {
        runtime
            .run_turn_assembled(
                TurnInput::text("cancel before the lease renewal fails"),
                CancellationToken::new(),
                named_turn_scope("root", turn_id),
            )
            .await
    });
    provider_started_rx
        .await
        .expect("provider should start after lease acquisition");
    let receipt = turn_driver
        .request_cancel(
            crate::TurnCancelRequest::new(
                crate::TurnAddress::new("root", turn_id),
                "cancel-before-loss-request",
                Some("test-user".to_string()),
            )
            .with_reason("user stopped the turn"),
        )
        .await
        .expect("seal user cancellation");
    assert!(matches!(
        receipt.outcome,
        crate::TurnCancelOutcome::Requested(ref evidence)
            if evidence.request_id == "cancel-before-loss-request"
    ));

    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        while !effect_loop_ended.load(Ordering::SeqCst) {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("turn should observe cancellation before finalization");
    let renewals_before_failure = store.session_execution_lease_renewal_count();
    store.fail_next_session_execution_lease_renewal();
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        while store.session_execution_lease_renewal_count() == renewals_before_failure {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("renewal task should receive the injected store rejection");
    release_effect_loop.store(true, Ordering::SeqCst);

    let assembled = tokio::time::timeout(std::time::Duration::from_secs(5), turn)
        .await
        .expect("cancelled turn should finish")
        .expect("cancelled turn task")
        .expect("sealed cancellation should commit despite later renewal rejection");
    assert!(matches!(
        assembled.outcome,
        TurnOutcome::Stopped(TurnStop::Cancelled)
    ));
    assert_eq!(
        assembled.cancellation,
        Some(crate::TurnCancellationEvidence {
            request_id: "cancel-before-loss-request".to_string(),
            origin: Some("test-user".to_string()),
            reason: Some("user stopped the turn".to_string()),
        })
    );
}

#[tokio::test]
async fn finish_turn_commit_uses_head_cas_after_advisory_lease_expiry() {
    let clock = Arc::new(ManualClock::new(1_000));
    let store_clock: Arc<dyn crate::Clock> = clock.clone();
    let store = Arc::new(RecordingStore::with_clock(store_clock));
    let runtime_store: Arc<dyn crate::store::RuntimePersistence> = store.clone();
    let transport = mock_provider(vec![MockCall {
        stream_events: Vec::new(),
        response: Ok(LlmResponse {
            full_text: "committed after lease expiry".to_string(),
            parts: vec![LlmOutputPart::Text {
                text: "committed after lease expiry".to_string(),
                response_meta: None,
            }],
            response_metadata: Default::default(),
            ..LlmResponse::default()
        }),
    }]);
    let host_clock: Arc<dyn crate::Clock> = clock.clone();
    let mut config = crate::RuntimeHostConfig::in_memory().with_clock(host_clock);
    config.providers.provider_resolver = Arc::new(crate::SingleProviderResolver::new(
        transport.clone().into_handle(),
    ));
    let mut runtime = runtime_with_plugins_and_tools_and_host_and_store(
        Vec::new(),
        Arc::new(EmptyTools),
        transport,
        crate::EmbeddedRuntimeHost::new(config),
        runtime_store,
    )
    .await;
    runtime.set_turn_phase_probe(Arc::new(ExpireLeaseAtFinalCommit::new(Arc::clone(&clock))));

    let assembled = runtime
        .run_turn_assembled(
            TurnInput::text("lease expires at commit"),
            CancellationToken::new(),
            named_turn_scope("root", "final-commit-lease-expiry-turn"),
        )
        .await
        .expect("head CAS must authorize final commit after advisory lease expiry");

    assert_eq!(
        assembled.assistant_output.safe_text,
        "committed after lease expiry"
    );
}

#[tokio::test]
async fn prepared_checkpoint_continues_after_advisory_lease_expiry() {
    let clock = Arc::new(ManualClock::new(1_000));
    let store_clock: Arc<dyn crate::Clock> = clock.clone();
    let store = Arc::new(RecordingStore::with_clock(store_clock));
    let runtime_store: Arc<dyn crate::store::RuntimePersistence> = store.clone();
    let transport = mock_provider(vec![MockCall {
        stream_events: Vec::new(),
        response: Ok(LlmResponse {
            full_text: "provider reached".to_string(),
            parts: vec![LlmOutputPart::Text {
                text: "provider reached".to_string(),
                response_meta: None,
            }],
            response_metadata: Default::default(),
            ..LlmResponse::default()
        }),
    }]);
    let host_clock: Arc<dyn crate::Clock> = clock.clone();
    let mut config = crate::RuntimeHostConfig::in_memory().with_clock(host_clock);
    config.providers.provider_resolver = Arc::new(crate::SingleProviderResolver::new(
        transport.clone().into_handle(),
    ));
    let mut runtime = runtime_with_plugins_and_tools_and_host_and_store(
        Vec::new(),
        Arc::new(EmptyTools),
        transport,
        crate::EmbeddedRuntimeHost::new(config),
        runtime_store,
    )
    .await;
    runtime.set_turn_phase_probe(Arc::new(ExpireLeaseAfterPromptBuild::new(Arc::clone(
        &clock,
    ))));

    let assembled = runtime
        .run_turn_assembled(
            TurnInput::text("lease expires at prepared checkpoint"),
            CancellationToken::new(),
            named_turn_scope("root", "prepared-checkpoint-lease-expiry-turn"),
        )
        .await
        .expect("prepared checkpoint must continue after advisory lease expiry");

    assert_eq!(assembled.assistant_output.safe_text, "provider reached");
}

// Boundary: this durable process-wake case stays in `turns.rs` because it
// asserts committed conversation history, streamed turn events, and process
// origin metadata across the full runtime, not only persistence ownership.
#[tokio::test]
async fn durable_process_wake_drains_as_committed_event_history_and_acknowledges() {
    let transport = mock_provider(vec![
        MockCall {
            stream_events: Vec::new(),
            response: Ok(LlmResponse {
                full_text: "first answer".to_string(),
                parts: vec![LlmOutputPart::Text {
                    text: "first answer".to_string(),
                    response_meta: None,
                }],
                response_metadata: Default::default(),
                ..LlmResponse::default()
            }),
        },
        MockCall {
            stream_events: Vec::new(),
            response: Ok(LlmResponse {
                full_text: "acknowledged".to_string(),
                parts: vec![LlmOutputPart::Text {
                    text: "acknowledged".to_string(),
                    response_meta: None,
                }],
                response_metadata: Default::default(),
                ..LlmResponse::default()
            }),
        },
    ]);
    let (mut runtime, store) = standard_runtime_with_transport_and_queue_store(transport).await;
    let registry = runtime
        .host
        .process_registry
        .as_ref()
        .expect("process registry")
        .clone();
    let target_scope = crate::SessionScope::new("root");
    let process_caused_by = crate::CausalRef::SessionNode {
        session_id: "root".to_string(),
        node_id: "trigger:button".to_string(),
    };
    registry
        .register_process(
            crate::ProcessRegistration::new(
                "wake-proc",
                crate::ProcessInput::External {
                    metadata: serde_json::Value::Null,
                },
                crate::RecoveryDisposition::ExternallyOwned,
                crate::ProcessProvenance::session(target_scope.clone())
                    .with_caused_by(Some(process_caused_by.clone())),
            )
            .with_extra_event_types([process_wake_event_type()]),
        )
        .await
        .expect("register wake process");
    let wake = append_process_wake_to_queue(
        registry.as_ref(),
        store.as_ref(),
        "wake-proc",
        crate::ProcessEventAppendRequest::new(
            "process.wake",
            json!({
                "text": "deploy complete",
                "value": {
                    "status": "deploy complete"
                }
            }),
        )
        .with_wake_target_scope(target_scope.clone()),
    )
    .await;
    let expected_wake_id = wake.wake_id.clone();
    let expected_text = "Background process wake\nProcess: wake-proc\nEvent: process.wake #1\nWake input:\ndeploy complete";

    let sink = RecordingSink::default();
    let turn_events = RecordingTurnEvents::default();
    runtime
        .stream_turn(
            TurnInput::text("hello"),
            TurnOptions::new(
                CancellationToken::new(),
                named_turn_scope("root", "process-wake-turn"),
            )
            .with_events(&sink)
            .with_turn_events(&turn_events),
        )
        .await
        .expect("turn");

    let turn_event_snapshot = turn_events.snapshot();
    let queued_started = turn_event_snapshot
        .iter()
        .find(|activity| matches!(&activity.event, crate::TurnEvent::QueuedWorkStarted { .. }))
        .expect("queued work started event");
    let crate::TurnEvent::QueuedWorkStarted {
        boundary, causes, ..
    } = &queued_started.event
    else {
        panic!("expected queued work started event");
    };
    assert_eq!(
        *boundary,
        crate::QueuedWorkClaimBoundary::ActiveTurnCheckpoint
    );
    assert!(causes.iter().any(|cause| {
        cause.event_type == "process.wake"
            && cause.id == expected_wake_id
            && cause.text == expected_text
            && matches!(
                &cause.origin,
                crate::MessageOrigin::Process {
                    process_id,
                    event_type,
                    sequence: 1,
                    wake_id,
                    caused_by,
                } if process_id == "wake-proc"
                    && event_type == "process.wake"
                    && wake_id.as_deref() == Some(expected_wake_id.as_str())
                    && caused_by.as_ref() == Some(&process_caused_by)
            )
    }));

    assert!(
        sink.snapshot().into_iter().all(|event| {
            !matches!(
                event,
                crate::SessionStreamEvent::InjectedMessagesCommitted { messages, .. }
                    if messages.iter().any(|message| message.content == expected_text)
            )
        }),
        "durable wake events must not be bridged as injected plugin messages"
    );
    assert!(
        crate::store::QueuedWorkStore::list_queued_work(store.as_ref(), "root")
            .await
            .expect("queued work after commit")
            .is_empty()
    );
    let wake_history = active_conversation_messages(&runtime.state)
        .into_iter()
        .find(|message| {
            message.role == crate::MessageRole::Event
                && message
                    .parts
                    .iter()
                    .any(|part| part.content == expected_text)
        })
        .expect("wake history message");
    assert!(matches!(
        wake_history.origin,
        Some(crate::MessageOrigin::Process {
            process_id,
            event_type,
            sequence,
            wake_id,
            caused_by,
        }) if process_id == "wake-proc"
            && event_type == "process.wake"
            && sequence == 1
            && wake_id.as_deref() == Some(expected_wake_id.as_str())
            && caused_by.as_ref() == Some(&process_caused_by)
    ));
    assert!(
        active_conversation_messages(&runtime.state)
            .iter()
            .all(|message| {
                !((message.role == crate::MessageRole::System
                    || message.role == crate::MessageRole::User)
                    && message
                        .parts
                        .iter()
                        .any(|part| part.content == expected_text))
            }),
        "durable wake must not enter history as provider system text"
    );
}

#[tokio::test]
async fn external_invoke_can_create_session_from_current_snapshot() {
    let plugin = Arc::new(RuntimeTestPluginFactory {
        build: Arc::new(|_| {
            Ok(Arc::new(RuntimeTestPlugin {
                before_turn: None,
                checkpoint: None,
                tool_result_projector: None,
                runtime_event: None,
                external_registrar: Some(Arc::new(|reg| {
                    reg.operations().command(
                        crate::PluginOperationDef {
                            name: "test.spawn".to_string(),
                            description: "spawn".to_string(),
                            kind: crate::PluginOperationKind::Command,
                            session_param: crate::SessionParam::Optional,
                            input_schema: json!({}),
                            output_schema: json!({}),
                        },
                        Arc::new(|ctx, _args| {
                            Box::pin(async move {
                                let handle = ctx
                                    .session_lifecycle
                                    .create_session(
                                        crate::SessionCreateRequest::root(
                                            crate::SessionStartPoint::CurrentSession,
                                            crate::PluginOptions::default(),
                                        )
                                        .with_session_id("branched")
                                        .with_plugin_source(
                                            crate::SessionPluginSource::CurrentSessionFork,
                                        )
                                        .with_initial_nodes(vec![crate::SessionAppendNode::message(
                                            crate::PluginMessage::text(
                                                crate::MessageRole::User,
                                                "branch seed",
                                            ),
                                        )]),
                                    )
                                    .await
                                    .map_err(|err| {
                                        crate::PluginOperationFailure::new(err.to_string())
                                    });
                                match handle {
                                    Ok(handle) => {
                                        let snapshot = ctx
                                            .sessions
                                            .snapshot_session(&handle.session_id)
                                            .await
                                            .map_err(|err| {
                                                crate::PluginOperationFailure::new(err.to_string())
                                            });
                                        match snapshot {
                                            Ok(snapshot) => Ok(crate::plugin::ErasedPluginCommandOutcome {
                                                output: json!({
                                                "session_id": handle.session_id,
                                                "message_count": snapshot.read_model().messages.len(),
                                                }),
                                                events: Vec::new(),
                                                directives: Vec::new(),
                                            }),
                                            Err(err) => Err(err),
                                        }
                                    }
                                    Err(err) => Err(err),
                                }
                            })
                        }),
                    )
                })),
            }))
        }),
    });
    let transport = mock_provider(Vec::new());
    let mut runtime = runtime_with_plugins(vec![plugin], transport).await;

    append_message(
        &mut runtime.state,
        Message {
            id: "m0".to_string(),
            role: MessageRole::User,
            parts: vec![Part {
                id: "m0.p0".to_string(),
                kind: PartKind::Text,
                content: "root msg".to_string(),
                attachment: None,
                tool_call_id: None,
                tool_name: None,
                tool_replay: None,
                prune_state: PruneState::Intact,
                reasoning_meta: None,
                response_meta: None,
            }]
            .into(),
            origin: None,
        },
    );

    let result = runtime
        .run_plugin_command(
            "test.spawn",
            json!({}),
            None,
            crate::ExecutionScope::runtime_operation("root:plugin-command:test-spawn"),
        )
        .await
        .expect("invoke");
    assert_eq!(
        result
            .output
            .get("session_id")
            .and_then(|value| value.as_str()),
        Some("branched")
    );
    assert_eq!(
        result
            .output
            .get("message_count")
            .and_then(|value| value.as_u64()),
        Some(2)
    );
}

#[tokio::test]
async fn plugin_command_reuses_caller_scope_on_lost_response_retry() {
    let plugin: Arc<dyn crate::PluginFactory> = Arc::new(RuntimeTestPluginFactory {
        build: Arc::new(|_| {
            Ok(Arc::new(RuntimeTestPlugin {
                before_turn: None,
                checkpoint: None,
                tool_result_projector: None,
                runtime_event: None,
                external_registrar: Some(Arc::new(|reg| {
                    reg.operations().command(
                        crate::PluginOperationDef {
                            name: "test.emit".to_string(),
                            description: "emit one durable event".to_string(),
                            kind: crate::PluginOperationKind::Command,
                            session_param: crate::SessionParam::Optional,
                            input_schema: json!({}),
                            output_schema: json!({}),
                        },
                        Arc::new(|_, _| {
                            Box::pin(async move {
                                Ok(crate::plugin::ErasedPluginCommandOutcome {
                                    output: json!({"ok": true}),
                                    events: vec![crate::PluginRuntimeEvent::Custom {
                                        name: "test.event".to_string(),
                                        payload: json!({"value": 1}),
                                    }],
                                    directives: Vec::new(),
                                })
                            })
                        }),
                    )
                })),
            }))
        }),
    });
    let store = Arc::new(RecordingStore::default());
    let store_trait = store.clone() as Arc<dyn crate::RuntimePersistence>;
    let mut first = runtime_with_plugins_and_tools_and_host_and_store(
        vec![Arc::clone(&plugin)],
        Arc::new(EmptyTools),
        mock_provider(Vec::new()),
        test_host_config(),
        Arc::clone(&store_trait),
    )
    .await;
    let mut retry = runtime_with_plugins_and_tools_and_host_and_store(
        vec![plugin],
        Arc::new(EmptyTools),
        mock_provider(Vec::new()),
        test_host_config(),
        store_trait,
    )
    .await;
    let operation_scope =
        crate::ExecutionScope::runtime_operation("root:plugin-command:stable-request");

    first
        .run_plugin_command("test.emit", json!({}), None, operation_scope.clone())
        .await
        .expect("first command attempt");
    let committed_after_first = *store
        .runtime_commit_count
        .lock()
        .expect("runtime commit count");

    retry
        .run_plugin_command("test.emit", json!({}), None, operation_scope)
        .await
        .expect("lost-response retry");

    assert_eq!(committed_after_first, 2);
    assert_eq!(
        *store
            .runtime_commit_count
            .lock()
            .expect("runtime commit count"),
        committed_after_first,
        "retrying one command scope must receipt-hit both durable effects"
    );
}

#[tokio::test]
async fn session_manager_can_run_child_session_turn() {
    let transport = mock_provider(vec![MockCall {
        stream_events: vec![
            LlmStreamEvent::Delta("child ".to_string()),
            LlmStreamEvent::Delta("session".to_string()),
            LlmStreamEvent::Usage(LlmUsage {
                input_tokens: 7,
                output_tokens: 2,
                cache_read_input_tokens: 0,
                cache_write_input_tokens: 0,
                reasoning_output_tokens: 1,
            }),
        ],
        response: Ok(LlmResponse {
            full_text: "child session".to_string(),
            parts: vec![LlmOutputPart::Text {
                text: "child session".to_string(),
                response_meta: None,
            }],
            response_metadata: Default::default(),
            ..LlmResponse::default()
        }),
    }]);
    let runtime = runtime_with_plugins(Vec::new(), transport).await;
    let lifecycle = runtime
        .session_lifecycle_service()
        .expect("session lifecycle");
    let handle = lifecycle
        .create_session(
            crate::SessionCreateRequest::root(
                crate::SessionStartPoint::Empty,
                crate::PluginOptions::default(),
            )
            .with_session_id("child")
            .with_plugin_source(crate::SessionPluginSource::CurrentSessionFork),
        )
        .await
        .expect("child session");
    let turn_id = "child-lifecycle-turn";
    let scoped_effect_controller = crate::ScopedEffectController::shared(
        Arc::new(crate::InlineRuntimeEffectController::default()),
        crate::ExecutionScope::turn(&handle.session_id, turn_id),
    )
    .expect("scoped child turn");
    let request = crate::SessionTurnRequest::new(
        &handle.session_id,
        turn_id,
        TurnInput {
            items: vec![InputItem::Text {
                text: "hello".to_string(),
            }],
            protocol_turn_options: None,
            trace_turn_id: None,
            protocol_extension: None,
            turn_context: crate::TurnContext::default(),
        },
        scoped_effect_controller,
    )
    .expect("child turn request");
    let assembled = lifecycle.start_turn(request).await.expect("child turn");
    assert_eq!(handle.session_id, "child");
    assert_eq!(handle.policy.model.id, "mock-model");
    assert_eq!(assembled.state.session_id, "child");
}

#[tokio::test]
async fn session_manager_persists_child_sessions_in_separate_store() {
    let factory = RecordingSessionStoreFactory::default();
    let host = test_host_config().with_session_store_factory(Arc::new(factory.clone()));
    let mut runtime = runtime_with_plugins_and_tools_and_host(
        Vec::new(),
        Arc::new(EmptyTools),
        mock_provider(Vec::new()),
        host,
    )
    .await;
    append_message(
        &mut runtime.state,
        Message {
            id: "u1".to_string(),
            role: MessageRole::User,
            parts: vec![Part {
                id: "u1.p0".to_string(),
                kind: PartKind::Text,
                content: "parent hello".to_string(),
                attachment: None,
                tool_call_id: None,
                tool_name: None,
                tool_replay: None,
                prune_state: PruneState::Intact,
                reasoning_meta: None,
                response_meta: None,
            }]
            .into(),
            origin: None,
        },
    );
    runtime.state.turn_index = 3;

    let lifecycle = runtime
        .session_lifecycle_service()
        .expect("session lifecycle");
    let handle = lifecycle
        .create_session(
            crate::SessionCreateRequest::child_session(
                "root",
                crate::SessionStartPoint::CurrentSession,
                crate::PluginOptions::default(),
            )
            .with_session_id("child-store")
            .with_plugin_source(crate::SessionPluginSource::CurrentSessionFork),
        )
        .await
        .expect("child session");

    assert_eq!(handle.session_id, "child-store");
    let stores = factory.stores();
    assert_eq!(stores.len(), 1);
    let meta = crate::store::SessionCommitStore::load_session_meta(stores[0].as_ref())
        .await
        .expect("load session meta")
        .expect("session meta");
    assert_eq!(meta.session_id, "child-store");
    assert_eq!(meta.parent_session_id(), Some("root"));
    let read = crate::store::SessionCommitStore::load_session(
        stores[0].as_ref(),
        crate::store::SessionReadScope::FullGraph,
    )
    .await
    .expect("load session")
    .expect("session read");
    let graph = read.graph;
    let child_frame_node_id =
        crate::session_graph::frame_node_id(&meta.incarnation_id, "initial-frame");
    assert_eq!(
        graph.nodes.first().map(|node| node.node_id.as_str()),
        Some(child_frame_node_id.as_str())
    );
    assert_eq!(
        graph
            .nodes
            .first()
            .and_then(|node| node.parent_node_id.as_deref()),
        None
    );
    assert_eq!(
        graph
            .nodes
            .iter()
            .filter(|node| matches!(node.payload, crate::SessionNodePayload::FrameOpen { .. }))
            .count(),
        1,
        "child history must not retain the parent frame root"
    );
    let read_model = graph.read_model();
    let messages = read_model.messages.as_slice();
    assert_eq!(messages.len(), 1);
    assert_eq!(messages[0].parts[0].content, "parent hello");
    let checkpoint = read.checkpoint.expect("checkpoint");
    let turn_state = checkpoint.turn_state;
    assert_eq!(turn_state.turn_index, 3);
}

#[tokio::test]
async fn child_relation_does_not_replace_active_session() {
    let mut runtime = runtime_with_plugins(Vec::new(), mock_provider(Vec::new())).await;
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
            .with_session_id("ordinary-child")
            .with_plugin_source(crate::SessionPluginSource::CurrentSessionFork),
        )
        .await
        .expect("child session");

    assert_eq!(runtime.session_id(), "root");
    let assembled = runtime
        .run_turn_assembled(
            TurnInput {
                items: vec![InputItem::Text {
                    text: "parent turn".to_string(),
                }],
                protocol_turn_options: None,
                trace_turn_id: None,
                protocol_extension: None,
                turn_context: crate::TurnContext::default(),
            },
            CancellationToken::new(),
            named_turn_scope("root", "ordinary-child-parent-turn"),
        )
        .await
        .expect("parent turn");

    assert_eq!(assembled.state.session_id, "root");
    assert_eq!(assembled.state.turn_index, 1);
}

#[tokio::test]
async fn session_manager_rejects_duplicate_child_session_ids() {
    let runtime = runtime_with_plugins(Vec::new(), mock_provider(Vec::new())).await;
    let lifecycle = runtime
        .session_lifecycle_service()
        .expect("session lifecycle");
    lifecycle
        .create_session(
            crate::SessionCreateRequest::root(
                crate::SessionStartPoint::Empty,
                crate::PluginOptions::default(),
            )
            .with_session_id("child")
            .with_plugin_source(crate::SessionPluginSource::CurrentSessionFork),
        )
        .await
        .expect("first child session");

    let err = lifecycle
        .create_session(
            crate::SessionCreateRequest::root(
                crate::SessionStartPoint::Empty,
                crate::PluginOptions::default(),
            )
            .with_session_id("child")
            .with_plugin_source(crate::SessionPluginSource::CurrentSessionFork),
        )
        .await
        .expect_err("duplicate child session should fail");
    assert!(err.to_string().contains("already exists"));
}

#[tokio::test]
async fn runtime_can_activate_managed_child_session() {
    let mut runtime = runtime_with_plugins(Vec::new(), mock_provider(Vec::new())).await;
    let lifecycle = runtime
        .session_lifecycle_service()
        .expect("session lifecycle");
    lifecycle
        .create_session(
            crate::SessionCreateRequest::child(
                runtime.session_id(),
                crate::SessionStartPoint::Empty,
                runtime.policy.clone(),
                crate::PluginOptions::default(),
                "test",
            )
            .with_session_id("child")
            .with_plugin_source(crate::SessionPluginSource::CurrentSessionFork),
        )
        .await
        .expect("child session");

    runtime
        .activate_managed_session("child")
        .await
        .expect("activate child");

    assert_eq!(runtime.session_id(), "child");
    let activated_child_request = crate::SessionTurnRequest::new(
        "child",
        "activated-child-turn",
        TurnInput {
            items: vec![InputItem::Text {
                text: "old manager should not own activated child".to_string(),
            }],
            protocol_turn_options: None,
            trace_turn_id: None,
            protocol_extension: None,
            turn_context: crate::TurnContext::default(),
        },
        crate::ScopedEffectController::shared(
            Arc::new(crate::InlineRuntimeEffectController::default()),
            crate::ExecutionScope::turn("child", "activated-child-turn"),
        )
        .expect("scoped activated child turn"),
    )
    .expect("activated child request");
    assert!(
        lifecycle.start_turn(activated_child_request).await.is_err(),
        "activated child runtime should leave the parent manager registry"
    );
}

#[test]
fn turn_input_queue_ingress_has_one_production_draft_persistence_path() {
    fn scan_dir(root: &std::path::Path, file: &mut dyn FnMut(&std::path::Path)) {
        let Ok(entries) = std::fs::read_dir(root) else {
            return;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path
                .file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name == "target" || name == ".git")
            {
                continue;
            }
            if path.is_dir() {
                scan_dir(&path, file);
            } else if path.extension().and_then(|ext| ext.to_str()) == Some("rs") {
                file(&path);
            }
        }
    }

    let workspace = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(std::path::Path::parent)
        .expect("workspace root");
    let allowed = workspace.join("crates/lash-core/src/runtime/session_api.rs");
    let mut offenders = Vec::new();
    for root in [workspace.join("crates"), workspace.join("examples")] {
        scan_dir(&root, &mut |path| {
            let normalized = path.to_string_lossy();
            if normalized.contains("/src/runtime/tests/")
                || normalized.contains("/src/testing/")
                || normalized.contains("/tests/")
            {
                return;
            }
            let Ok(source) = std::fs::read_to_string(path) else {
                return;
            };
            for (offset, _) in source.match_indices("QueuedWorkBatchDraft::new") {
                let snippet_end = source.len().min(offset + 800);
                let snippet = &source[offset..snippet_end];
                let constructs_turn_input_draft = snippet.contains("QueuedWorkPayload::turn_input")
                    || snippet.contains("QueuedWorkPayload::TurnInput");
                if constructs_turn_input_draft && path != allowed {
                    offenders.push(
                        path.strip_prefix(workspace)
                            .unwrap_or(path)
                            .display()
                            .to_string(),
                    );
                }
            }
        });
    }
    assert!(
        offenders.is_empty(),
        "turn-input queued work drafts must be persisted only through LashRuntime::enqueue_turn_input; offenders: {offenders:?}"
    );
}

#[tokio::test]
async fn turn_driver_normalizes_alias_effort_into_outgoing_request() {
    use std::sync::{Arc, Mutex};

    let captured: Arc<Mutex<Option<crate::ReasoningSelection>>> = Arc::new(Mutex::new(None));
    let captured_for_provider = Arc::clone(&captured);
    let provider = TestProvider::builder()
        .kind("capability-capture")
        .complete(move |req| {
            let captured = Arc::clone(&captured_for_provider);
            async move {
                *captured.lock().expect("capture lock") = Some(req.model_variant.clone());
                Ok(LlmResponse {
                    full_text: "ok".to_string(),
                    parts: vec![LlmOutputPart::Text {
                        text: "ok".to_string(),
                        response_meta: None,
                    }],
                    response_metadata: Default::default(),
                    ..LlmResponse::default()
                })
            }
        })
        .build()
        .into_handle();

    let capability = crate::ModelCapability {
        reasoning: Some(crate::ReasoningCapability {
            efforts: ["low", "medium", "high", "max"]
                .into_iter()
                .map(String::from)
                .collect(),
            aliases: std::collections::BTreeMap::from([("xhigh".to_string(), "max".to_string())]),
            ..Default::default()
        }),
        cache_control: None,
        stream_termination: None,
        sampling: crate::SamplingCapability::Configurable,
    };
    let model = crate::ModelSpec::from_token_limits(
        "mock-model",
        crate::ReasoningSelection::Effort("xhigh".to_string()),
        200_000,
        None,
    )
    .expect("valid model spec")
    .with_capability(capability);

    let mut runtime = runtime_with_plugins(Vec::new(), mock_provider(Vec::new())).await;
    runtime
        .update_session_config(Some(provider), Some(model), None)
        .await;

    let turn = runtime
        .run_turn_assembled(
            TurnInput {
                items: vec![InputItem::Text {
                    text: "hello".to_string(),
                }],
                protocol_turn_options: None,
                trace_turn_id: None,
                protocol_extension: None,
                turn_context: crate::TurnContext::default(),
            },
            CancellationToken::new(),
            named_turn_scope("root", "alias-normalize-turn"),
        )
        .await
        .expect("turn");

    assert_eq!(turn.assistant_output.safe_text, "ok");
    let seen = captured
        .lock()
        .expect("capture lock")
        .clone()
        .expect("provider must be called");
    assert_eq!(
        seen,
        crate::ReasoningSelection::Effort("max".to_string()),
        "alias `xhigh` must clamp to canonical `max` before the provider sees the request"
    );
}

#[tokio::test]
async fn turn_driver_rejects_unsupported_effort_before_provider_call() {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};

    let called = Arc::new(AtomicBool::new(false));
    let called_for_provider = Arc::clone(&called);
    let provider = TestProvider::builder()
        .kind("capability-reject")
        .complete(move |_req| {
            let called = Arc::clone(&called_for_provider);
            async move {
                called.store(true, Ordering::SeqCst);
                Ok(LlmResponse::default())
            }
        })
        .build()
        .into_handle();

    let capability = crate::ModelCapability {
        reasoning: Some(crate::ReasoningCapability {
            efforts: ["low", "medium", "high"]
                .into_iter()
                .map(String::from)
                .collect(),
            ..Default::default()
        }),
        cache_control: None,
        stream_termination: None,
        sampling: crate::SamplingCapability::Configurable,
    };
    let model = crate::ModelSpec::from_token_limits(
        "mock-model",
        crate::ReasoningSelection::Effort("turbo".to_string()),
        200_000,
        None,
    )
    .expect("valid model spec")
    .with_capability(capability);

    let mut runtime = runtime_with_plugins(Vec::new(), mock_provider(Vec::new())).await;
    runtime
        .update_session_config(Some(provider), Some(model), None)
        .await;

    let turn = runtime
        .run_turn_assembled(
            TurnInput {
                items: vec![InputItem::Text {
                    text: "hello".to_string(),
                }],
                protocol_turn_options: None,
                trace_turn_id: None,
                protocol_extension: None,
                turn_context: crate::TurnContext::default(),
            },
            CancellationToken::new(),
            named_turn_scope("root", "unsupported-effort-turn"),
        )
        .await
        .expect("turn");

    assert!(
        !called.load(Ordering::SeqCst),
        "an unsupported effort must be rejected before the provider is called"
    );
    let issue = turn
        .errors
        .iter()
        .find(|issue| issue.kind == "llm_provider")
        .expect("llm_provider issue");
    assert_eq!(issue.code.as_deref(), Some("unsupported_effort"));
    assert!(issue.message.contains("Unsupported effort `turbo`"));
}
