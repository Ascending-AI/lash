use super::*;
use crate::ToolProvider as _;
use crate::facade_support::{RuntimeSessionStateFacadeOps, ToolStateFacadeOps};
use lash_sansio::core_support::*;
use lash_sansio::sync::MutexExt;
use std::sync::atomic::AtomicUsize;

type PluginErrorDiscriminant = std::mem::Discriminant<crate::PluginError>;

fn turn_persisted_borrowed_append_plugin(
    attempted: Arc<AtomicBool>,
    received_error: Arc<std::sync::Mutex<Option<PluginErrorDiscriminant>>>,
) -> Arc<dyn crate::PluginFactory> {
    Arc::new(RuntimeTestPluginFactory {
        build: Arc::new(move |_| {
            let attempted = Arc::clone(&attempted);
            let received_error = Arc::clone(&received_error);
            Ok(Arc::new(RuntimeTestPlugin {
                before_turn: None,
                checkpoint: None,
                tool_result_projector: None,
                runtime_event: Some(Arc::new(move |event| {
                    let attempted = Arc::clone(&attempted);
                    let received_error = Arc::clone(&received_error);
                    Box::pin(async move {
                        let crate::PluginLifecycleEvent::TurnPersisted(ctx) = event else {
                            return Ok(());
                        };
                        if attempted.swap(true, Ordering::SeqCst) {
                            return Ok(());
                        }
                        if let Err(error) = ctx
                            .session_graph
                            .append_session_nodes(
                                &ctx.session_id,
                                crate::AppendSessionNodesRequest {
                                    operation_id: "lapsed-borrow-probe".to_string(),
                                    nodes: vec![crate::SessionAppendNode::plugin(
                                        "test.lapsed-borrow",
                                        serde_json::json!({"attempted": true}),
                                    )],
                                    requires_ancestor_node_id: None,
                                },
                            )
                            .await
                        {
                            *received_error.lock_recover() = Some(std::mem::discriminant(&error));
                            return Err(error);
                        }
                        Ok(())
                    })
                })),
                external_registrar: None,
            }))
        }),
    })
}

fn turn_finalized_borrowed_append_plugin() -> Arc<dyn crate::PluginFactory> {
    Arc::new(RuntimeTestPluginFactory {
        build: Arc::new(move |_| {
            let retained = Arc::new(std::sync::Mutex::new(None));
            Ok(Arc::new(RuntimeTestPlugin {
                before_turn: None,
                checkpoint: None,
                tool_result_projector: None,
                runtime_event: Some(Arc::new(move |event| {
                    let retained = Arc::clone(&retained);
                    Box::pin(async move {
                        match event {
                            crate::PluginLifecycleEvent::TurnPersisted(ctx) => {
                                *retained.lock_recover() = Some(Arc::clone(&ctx.session_graph));
                                Ok(())
                            }
                            crate::PluginLifecycleEvent::TurnFinalized(turn) => {
                                let graph: Option<Arc<dyn crate::plugin::SessionGraphService>> =
                                    retained.lock_recover().clone();
                                let Some(graph) = graph else {
                                    return Ok(());
                                };
                                graph
                                    .append_session_nodes(
                                        &turn.state.session_id,
                                        crate::AppendSessionNodesRequest {
                                            operation_id: "finalized-lapsed-borrow-probe"
                                                .to_string(),
                                            nodes: vec![crate::SessionAppendNode::plugin(
                                                "test.finalized-lapsed-borrow",
                                                serde_json::json!({"attempted": true}),
                                            )],
                                            requires_ancestor_node_id: None,
                                        },
                                    )
                                    .await?;
                                Ok(())
                            }
                            _ => Ok(()),
                        }
                    })
                })),
                external_registrar: None,
            }))
        }),
    })
}

fn retain_turn_persisted_graph_service_plugin(
    retained: Arc<std::sync::Mutex<Option<Arc<dyn crate::plugin::SessionGraphService>>>>,
) -> Arc<dyn crate::PluginFactory> {
    Arc::new(RuntimeTestPluginFactory {
        build: Arc::new(move |_| {
            let retained = Arc::clone(&retained);
            Ok(Arc::new(RuntimeTestPlugin {
                before_turn: None,
                checkpoint: None,
                tool_result_projector: None,
                runtime_event: Some(Arc::new(move |event| {
                    let retained = Arc::clone(&retained);
                    Box::pin(async move {
                        if let crate::PluginLifecycleEvent::TurnPersisted(ctx) = event {
                            *retained.lock_recover() = Some(Arc::clone(&ctx.session_graph));
                            return Err(crate::PluginError::Session(
                                "stop after retaining the turn-scoped graph service".to_string(),
                            ));
                        }
                        Ok(())
                    })
                })),
                external_registrar: None,
            }))
        }),
    })
}

struct FailNextProtocolRestore {
    fail_next: AtomicBool,
    restore_count: AtomicUsize,
}

#[async_trait::async_trait]
impl crate::plugin::ProtocolSessionPlugin for FailNextProtocolRestore {
    async fn restore_session(
        &self,
        _ctx: crate::plugin::ProtocolSessionContext<'_>,
        _state: &crate::RuntimeSessionState,
    ) -> Result<(), crate::SessionError> {
        self.restore_count.fetch_add(1, Ordering::SeqCst);
        if self.fail_next.swap(false, Ordering::SeqCst) {
            return Err(crate::SessionError::Protocol(
                "injected post-commit restore failure".to_string(),
            ));
        }
        Ok(())
    }
}

struct RecordPostCommitDelivery {
    entered: Arc<AtomicBool>,
}

struct FailCaptureAfterFirstCommittedTurn {
    executor: Arc<FailingCaptureExecutor>,
    committed_turns: AtomicUsize,
}

struct FailCaptureAfterCommittedTurns {
    executor: Arc<FailingCaptureExecutor>,
    committed_turns: AtomicUsize,
    fail_after: usize,
}

struct FailCaptureAfterEffectLoop {
    executor: Arc<FailingCaptureExecutor>,
}

impl crate::runtime::RuntimeTurnPhaseProbe for FailCaptureAfterEffectLoop {
    fn begin(&self, _phase: crate::runtime::RuntimeTurnPhase) {}

    fn end(&self, phase: crate::runtime::RuntimeTurnPhase) {
        if phase == crate::runtime::RuntimeTurnPhase::EffectLoop {
            self.executor.dirty.store(true, Ordering::SeqCst);
            self.executor.fail_capture.store(true, Ordering::SeqCst);
        }
    }
}

impl crate::runtime::RuntimeTurnPhaseProbe for FailCaptureAfterFirstCommittedTurn {
    fn begin(&self, _phase: crate::runtime::RuntimeTurnPhase) {}

    fn end(&self, phase: crate::runtime::RuntimeTurnPhase) {
        if phase == crate::runtime::RuntimeTurnPhase::CommittedTurn
            && self.committed_turns.fetch_add(1, Ordering::SeqCst) == 0
        {
            self.executor.dirty.store(true, Ordering::SeqCst);
            self.executor.fail_capture.store(true, Ordering::SeqCst);
        }
    }
}

impl crate::runtime::RuntimeTurnPhaseProbe for FailCaptureAfterCommittedTurns {
    fn begin(&self, _phase: crate::runtime::RuntimeTurnPhase) {}

    fn end(&self, phase: crate::runtime::RuntimeTurnPhase) {
        if phase == crate::runtime::RuntimeTurnPhase::CommittedTurn
            && self.committed_turns.fetch_add(1, Ordering::SeqCst) + 1 == self.fail_after
        {
            self.executor.dirty.store(true, Ordering::SeqCst);
            self.executor.fail_capture.store(true, Ordering::SeqCst);
        }
    }
}

impl crate::runtime::RuntimeTurnPhaseProbe for RecordPostCommitDelivery {
    fn begin(&self, phase: crate::runtime::RuntimeTurnPhase) {
        if phase == crate::runtime::RuntimeTurnPhase::PostCommitDelivery {
            self.entered.store(true, Ordering::SeqCst);
        }
    }

    fn end(&self, _phase: crate::runtime::RuntimeTurnPhase) {}
}

struct SuspendingPostCommitSink {
    adopted: Arc<AtomicBool>,
    entered: tokio::sync::mpsc::Sender<()>,
    release: Arc<tokio::sync::Notify>,
}

struct FailingCaptureExecutor {
    dirty: AtomicBool,
    fail_capture: AtomicBool,
    snapshot: std::sync::Mutex<Vec<u8>>,
    restored: std::sync::Mutex<Vec<Vec<u8>>>,
}

#[async_trait::async_trait]
impl crate::plugin::CodeExecutorPlugin for FailingCaptureExecutor {
    async fn execute_code(
        &self,
        _ctx: crate::RuntimeExecutionContext<'_>,
        _request: crate::ExecRequest,
    ) -> Result<crate::ExecResponse, crate::SessionError> {
        unreachable!("execution-state capture regression does not execute code")
    }

    fn execution_state_dirty(&self) -> bool {
        self.dirty.load(Ordering::SeqCst)
    }

    async fn snapshot_execution_state(
        &self,
        _ctx: crate::plugin::ProtocolSessionContext<'_>,
    ) -> Result<crate::plugin::ExecutionStateSnapshot, crate::SessionError> {
        if self.fail_capture.load(Ordering::SeqCst) {
            return Err(crate::SessionError::Protocol(
                "injected dirty execution-state capture failure".to_string(),
            ));
        }
        Ok(crate::plugin::ExecutionStateSnapshot::from_root(Some(
            self.snapshot.lock_recover().clone(),
        )))
    }

    /// Reports the same obstacle the capture itself would hit, and stages
    /// nothing — the runtime uses this to fail a turn before its provider call.
    async fn probe_execution_state_capture(
        &self,
        _ctx: crate::plugin::ProtocolSessionContext<'_>,
    ) -> Result<(), crate::SessionError> {
        if self.fail_capture.load(Ordering::SeqCst) {
            return Err(crate::SessionError::Protocol(
                "injected dirty execution-state capture failure".to_string(),
            ));
        }
        Ok(())
    }

    async fn restore_execution_state(
        &self,
        _ctx: crate::plugin::ProtocolSessionContext<'_>,
        state: &crate::plugin::HydratedExecutionState,
    ) -> Result<(), crate::SessionError> {
        self.restored.lock_recover().push(state.root.clone());
        Ok(())
    }
}

struct RestoreExecutorFromRuntimeState {
    executor: Arc<FailingCaptureExecutor>,
}

struct SwitchBeforeLlmProtocol {
    executor: Option<Arc<FailingCaptureExecutor>>,
    frame_key_material: String,
    switch_next: AtomicBool,
}

struct ResetExecutorOnSwitchProtocol {
    executor: Arc<FailingCaptureExecutor>,
    frame_key_material: String,
    switch_next: AtomicBool,
}

#[async_trait::async_trait]
impl crate::plugin::ProtocolSessionPlugin for ResetExecutorOnSwitchProtocol {
    async fn restore_session(
        &self,
        ctx: crate::plugin::ProtocolSessionContext<'_>,
        state: &crate::RuntimeSessionState,
    ) -> Result<(), crate::SessionError> {
        let snapshot = state
            .execution_state_hydration()
            .map_err(|source| crate::SessionError::Store {
                context: "hydrate test execution state".to_string(),
                source,
            })?
            .unwrap_or_else(|| crate::plugin::HydratedExecutionState {
                root: b"fresh-frame-execution-state".to_vec(),
                components: std::collections::BTreeMap::new(),
            });
        crate::plugin::CodeExecutorPlugin::restore_execution_state(
            self.executor.as_ref(),
            ctx,
            &snapshot,
        )
        .await
    }

    async fn before_llm_call(
        &self,
        _ctx: crate::plugin::ProtocolBeforeLlmCallContext,
        _request: &crate::LlmRequest,
    ) -> Result<Option<crate::ProtocolLlmCallAction>, crate::PluginError> {
        if !self.switch_next.swap(false, Ordering::SeqCst) {
            return Ok(None);
        }
        Ok(Some(crate::ProtocolLlmCallAction::SwitchAgentFrame {
            frame_key: crate::FrameKey::from_caller_material(&self.frame_key_material)
                .expect("non-empty caller material"),
            task: "reset the resident executor".to_string(),
        }))
    }
}

#[async_trait::async_trait]
impl crate::plugin::ProtocolSessionPlugin for SwitchBeforeLlmProtocol {
    async fn restore_session(
        &self,
        ctx: crate::plugin::ProtocolSessionContext<'_>,
        state: &crate::RuntimeSessionState,
    ) -> Result<(), crate::SessionError> {
        if let (Some(executor), Some(snapshot)) = (
            self.executor.as_ref(),
            state
                .execution_state_hydration()
                .map_err(|source| crate::SessionError::Store {
                    context: "hydrate test execution state".to_string(),
                    source,
                })?,
        ) {
            crate::plugin::CodeExecutorPlugin::restore_execution_state(
                executor.as_ref(),
                ctx,
                &snapshot,
            )
            .await?;
        }
        Ok(())
    }

    async fn before_llm_call(
        &self,
        _ctx: crate::plugin::ProtocolBeforeLlmCallContext,
        _request: &crate::LlmRequest,
    ) -> Result<Option<crate::ProtocolLlmCallAction>, crate::PluginError> {
        if !self.switch_next.swap(false, Ordering::SeqCst) {
            return Ok(None);
        }
        Ok(Some(crate::ProtocolLlmCallAction::SwitchAgentFrame {
            frame_key: crate::FrameKey::from_caller_material(&self.frame_key_material)
                .expect("non-empty caller material"),
            task: "protocol-directed switch".to_string(),
        }))
    }
}

#[async_trait::async_trait]
impl crate::plugin::ProtocolSessionPlugin for RestoreExecutorFromRuntimeState {
    async fn restore_session(
        &self,
        ctx: crate::plugin::ProtocolSessionContext<'_>,
        state: &crate::RuntimeSessionState,
    ) -> Result<(), crate::SessionError> {
        if let Some(snapshot) =
            state
                .execution_state_hydration()
                .map_err(|source| crate::SessionError::Store {
                    context: "hydrate test execution state".to_string(),
                    source,
                })?
        {
            crate::plugin::CodeExecutorPlugin::restore_execution_state(
                self.executor.as_ref(),
                ctx,
                &snapshot,
            )
            .await?;
        }
        Ok(())
    }
}

#[async_trait::async_trait]
impl crate::EventSink for SuspendingPostCommitSink {
    async fn emit(&self, event: crate::SessionStreamEvent) {
        if matches!(event, crate::SessionStreamEvent::PluginEvent { .. }) {
            assert!(
                self.adopted.load(Ordering::SeqCst),
                "resident state must be adopted before host event delivery"
            );
            let _ = self.entered.send(()).await;
            self.release.notified().await;
        }
    }
}

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

    async fn execute(&self, call: crate::ToolCall<'_>) -> crate::ToolOutcome {
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
        crate::ToolOutcome::from_output(crate::ToolCallOutput::success_tool_value(
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
struct CancelWatchTestClock(crate::testing::TestClock);

#[async_trait::async_trait]
impl crate::Clock for CancelWatchTestClock {
    fn now(&self) -> std::time::Instant {
        self.0.now()
    }

    fn timestamp_ms(&self) -> u64 {
        self.0.timestamp_ms()
    }

    fn timestamp_rfc3339(&self) -> String {
        self.0.timestamp_rfc3339()
    }

    fn timestamp_datetime(&self) -> chrono::DateTime<chrono::Utc> {
        self.0.timestamp_datetime()
    }

    async fn sleep(&self, _duration: std::time::Duration) {
        tokio::task::yield_now().await;
    }

    async fn sleep_until(&self, deadline: std::time::Instant) {
        self.0.sleep_until(deadline).await;
    }
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

#[tokio::test]
async fn dropping_suspended_host_delivery_keeps_committed_state_adopted() {
    let post_commit_entered = Arc::new(AtomicBool::new(false));
    let plugin: Arc<dyn crate::PluginFactory> = Arc::new(RuntimeTestPluginFactory {
        build: Arc::new(|_| {
            Ok(Arc::new(RuntimeTestPlugin {
                before_turn: None,
                checkpoint: None,
                tool_result_projector: None,
                runtime_event: None,
                external_registrar: Some(Arc::new(|reg| {
                    reg.turn().after(Arc::new(|_| {
                        Box::pin(async {
                            Ok(vec![crate::AfterTurnPluginDirective::from(
                                crate::PluginDirective::emit_runtime_events(vec![
                                    crate::PluginRuntimeEvent::Custom {
                                        name: "post_commit_suspend".to_string(),
                                        payload: serde_json::json!({"test": true}),
                                    },
                                ]),
                            )])
                        })
                    }));
                    Ok(())
                })),
            }))
        }),
    });
    let store = Arc::new(RecordingStore::default());
    let mut runtime = runtime_with_plugins_and_tools_and_host_and_store(
        vec![plugin],
        Arc::new(EmptyTools),
        mock_provider(vec![
            MockCall {
                stream_events: Vec::new(),
                response: Ok(LlmResponse {
                    parts: vec![LlmOutputPart::Text {
                        text: "committed before delivery".to_string(),
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
                        text: "resident commit survived drop".to_string(),
                        response_meta: None,
                    }],
                    response_metadata: Default::default(),
                    ..LlmResponse::default()
                }),
            },
        ]),
        test_host_config(),
        store.clone() as Arc<dyn crate::RuntimePersistence>,
    )
    .await;
    runtime.set_turn_phase_probe(Arc::new(RecordPostCommitDelivery {
        entered: Arc::clone(&post_commit_entered),
    }));
    let (entered_tx, mut entered_rx) = tokio::sync::mpsc::channel(1);
    let release = Arc::new(tokio::sync::Notify::new());
    let sink = SuspendingPostCommitSink {
        adopted: Arc::clone(&post_commit_entered),
        entered: entered_tx,
        release: Arc::clone(&release),
    };

    let mut turn = Box::pin(
        runtime.stream_turn(
            TurnInput::text("commit before delivering"),
            TurnOptions::new(
                CancellationToken::new(),
                named_turn_scope("root", "commit-before-delivery"),
            )
            .with_events(&sink),
        ),
    );
    tokio::select! {
        entered = entered_rx.recv() => assert!(entered.is_some(), "sink entered"),
        result = turn.as_mut() => panic!("turn must suspend in host delivery: {result:?}"),
    }
    let durable = crate::store::SessionCommitStore::load_session(store.as_ref())
        .await
        .expect("load committed head")
        .expect("committed session");
    assert_eq!(durable.head_revision, 1);
    drop(turn);
    assert_eq!(runtime.state.turn_index, 1);
    assert_eq!(runtime.resident_session_state, ResidentSessionState::Valid);
    let recovered = runtime
        .run_turn_assembled(
            TurnInput::text("continue after dropped host delivery"),
            CancellationToken::new(),
            named_turn_scope("root", "after-dropped-host-delivery"),
        )
        .await
        .expect("the adopted resident state remains usable");
    assert_eq!(
        recovered.assistant_output.safe_text,
        "resident commit survived drop"
    );
}

#[tokio::test]
async fn post_commit_restore_failure_is_a_diagnostic_and_forces_reload() {
    let protocol = Arc::new(FailNextProtocolRestore {
        fail_next: AtomicBool::new(false),
        restore_count: AtomicUsize::new(0),
    });
    let protocol_factory =
        crate::testing::test_standard_protocol_factory_with_runtime_state(protocol.clone(), None);
    let store = Arc::new(RecordingStore::default());
    let call_index = Arc::new(AtomicUsize::new(0));
    let transport = TestProvider::builder()
        .kind("mock")
        .requires_streaming(true)
        .complete(move |_| {
            let call_index = Arc::clone(&call_index);
            async move {
                Ok(match call_index.fetch_add(1, Ordering::SeqCst) {
                    0 => LlmResponse {
                        parts: vec![LlmOutputPart::ToolCall {
                            call_id: "switch".to_string(),
                            tool_name: "terminal_tool_0".to_string(),
                            input_json: "{}".to_string(),
                            replay: None,
                        }],
                        response_metadata: Default::default(),
                        ..LlmResponse::default()
                    },
                    1 => LlmResponse {
                        parts: vec![LlmOutputPart::Text {
                            text: "resident state reloaded".to_string(),
                            response_meta: None,
                        }],
                        response_metadata: Default::default(),
                        ..LlmResponse::default()
                    },
                    index => panic!("unexpected provider call {index}"),
                })
            }
        })
        .build();
    let mut runtime = runtime_with_plugins_and_tools_and_host_and_store(
        vec![protocol_factory],
        Arc::new(TerminalControlTool {
            controls: vec![crate::ToolControl::SwitchAgentFrame {
                frame_key: crate::FrameKey::from_caller_material("restore-failure-frame")
                    .expect("non-empty caller material"),
                initial_nodes: Vec::new(),
                task: Some("continue after restore".to_string()),
            }],
        }),
        transport,
        test_host_config(),
        store.clone() as Arc<dyn crate::RuntimePersistence>,
    )
    .await;
    protocol.fail_next.store(true, Ordering::SeqCst);

    let committed = runtime
        .run_turn_assembled(
            TurnInput::text("switch frames"),
            CancellationToken::new(),
            named_turn_scope("root", "post-commit-restore-failure"),
        )
        .await
        .expect("a published commit must not become a whole-turn error");
    assert!(matches!(
        committed.outcome,
        TurnOutcome::AgentFrameSwitch { .. }
    ));
    assert!(committed.errors.iter().any(|issue| {
        issue.code.as_deref() == Some("protocol_restore_session") && issue.retryable == Some(false)
    }));
    assert!(matches!(
        runtime.resident_session_state,
        ResidentSessionState::Invalidated { .. }
    ));
    let durable = crate::store::SessionCommitStore::load_session(store.as_ref())
        .await
        .expect("load committed frame switch")
        .expect("committed session");
    assert_eq!(durable.head_revision, 1);

    let ((refusal, reload_error, exported), capture) = super::trace_capture::capturing(|| async {
        let refusal = runtime
            .tool_state()
            .expect_err("a synchronous accessor refuses invalidated resident state");
        protocol.fail_next.store(true, Ordering::SeqCst);
        let reload_error = runtime
            .export_persisted_state()
            .await
            .expect_err("the injected protocol restore fault denies reload");
        let exported = runtime
            .export_persisted_state()
            .await
            .expect("persisted export retries reload from the durable head");
        (refusal, reload_error, exported)
    })
    .await;
    assert!(refusal.to_string().contains("durable reload is required"));
    assert_eq!(
        reload_error.code,
        crate::RuntimeErrorCode::ResidentSessionReloadFailed
    );
    assert_eq!(exported.head_revision, 1);
    assert_eq!(runtime.resident_session_state, ResidentSessionState::Valid);
    assert_eq!(protocol.restore_count.load(Ordering::SeqCst), 4);

    let refusal_event = capture.exactly_one("resident_session_state.sync_refusal");
    assert_eq!(refusal_event.field("consumer"), "tool_state");
    assert_eq!(refusal_event.field("consulted_validity"), "false");
    assert_eq!(refusal_event.field("outcome"), "refused");
    assert_eq!(
        refusal_event.field("error_classification"),
        "resident_session_reload_failed"
    );
    let reload_decisions = capture.named("resident_session_state.reload_decision");
    assert_eq!(
        reload_decisions.len(),
        2,
        "one decision event is required for each reload attempt"
    );
    let denied = &reload_decisions[0];
    assert_eq!(denied.field("consulted_validity"), "false");
    assert_eq!(denied.field("durable_source"), "history_store");
    assert_eq!(
        denied.field("durable_head_freshness"),
        "reloaded_from_store"
    );
    assert_eq!(denied.field("resident_head_revision"), "1");
    assert_eq!(denied.field("durable_head_revision"), "1");
    assert_eq!(
        denied.field("failing_restore_stage"),
        "protocol_session_restore"
    );
    assert_eq!(denied.field("outcome"), "denied");
    assert_eq!(
        denied.field("error_classification"),
        "resident_session_reload_failed"
    );
    let restored = &reload_decisions[1];
    assert_eq!(restored.field("failing_restore_stage"), "none");
    assert_eq!(restored.field("outcome"), "restored");
    assert_eq!(restored.field("error_classification"), "none");
    assert_eq!(
        refusal_event.field("decision_id"),
        denied.field("decision_id"),
        "synchronous refusal must reference the reload decision identity"
    );
    assert_eq!(
        denied.field("decision_id"),
        restored.field("decision_id"),
        "retrying one invalidation incident preserves its decision identity"
    );

    let recovered = runtime
        .run_turn_assembled(
            TurnInput::text("use the reloaded state"),
            CancellationToken::new(),
            named_turn_scope("root", "after-post-commit-restore-failure"),
        )
        .await
        .expect("next use reloads durable resident state");
    assert_eq!(
        recovered.assistant_output.safe_text,
        "resident state reloaded"
    );
    assert_eq!(runtime.resident_session_state, ResidentSessionState::Valid);
    assert_eq!(protocol.restore_count.load(Ordering::SeqCst), 4);
}

#[tokio::test]
async fn double_invalidation_preserves_first_decision_id() {
    let mut runtime =
        runtime_with_plugins_and_tools(Vec::new(), Arc::new(EmptyTools), mock_provider(Vec::new()))
            .await;
    assert_eq!(runtime.resident_session_state, ResidentSessionState::Valid);

    runtime.invalidate_resident_session_state();
    let initial_decision_id = match &runtime.resident_session_state {
        ResidentSessionState::Invalidated { decision_id } => decision_id.clone(),
        ResidentSessionState::Valid => panic!("expected invalidated resident state"),
    };
    assert!(!initial_decision_id.is_empty());

    // A second invalidation while already invalidated must preserve the first decision id
    runtime.invalidate_resident_session_state();
    match &runtime.resident_session_state {
        ResidentSessionState::Invalidated { decision_id } => {
            assert_eq!(
                decision_id, &initial_decision_id,
                "subsequent invalidation must not overwrite the initial decision identity"
            );
        }
        ResidentSessionState::Valid => panic!("expected invalidated resident state"),
    }
}

#[tokio::test]
async fn successful_reload_clears_invalidated_state_to_valid() {
    let store = Arc::new(RecordingStore::default());
    let mut runtime = runtime_with_plugins_and_tools_and_host_and_store(
        Vec::new(),
        Arc::new(EmptyTools),
        mock_provider(Vec::new()),
        test_host_config(),
        store.clone() as Arc<dyn crate::RuntimePersistence>,
    )
    .await;
    assert_eq!(runtime.resident_session_state, ResidentSessionState::Valid);

    runtime.invalidate_resident_session_state();
    assert!(matches!(
        runtime.resident_session_state,
        ResidentSessionState::Invalidated { .. }
    ));

    runtime
        .reload_invalidated_resident_session_state()
        .await
        .expect("successful reload from store/snapshot");

    assert_eq!(
        runtime.resident_session_state,
        ResidentSessionState::Valid,
        "successful reload must clear invalidated state back to Valid"
    );
}

/// FIG-1573: a turn that ends without committing must not leave an input
/// pinned to it - no crash required.
///
/// The host routed an input into the running turn, so the row is
/// `pending_active` and scoped to that turn's id. The commit-time re-defer
/// (`RuntimeCommit::deferring_interrupted_turn_inputs`) is the only writer that
/// moves such a row back to `deferred_next_turn`, and this turn never reaches
/// its commit: the store fences it, exactly as a claim fenced at a checkpoint
/// does in the field. The teardown owes the row the same repair, and this test
/// reads the durable row directly so it proves the teardown trigger and not the
/// drain-time backstop.
#[tokio::test]
async fn fig1573_input_pinned_to_a_turn_that_cannot_commit_is_re_deferred_at_teardown() {
    let session_id = "root";
    let live_turn_id = "fig1573-live-turn";
    let store = Arc::new(RecordingStore::default());
    let runtime_store: Arc<dyn crate::RuntimePersistence> = store.clone();
    let transport = TestProvider::builder()
        .kind("mock")
        .complete(|_| async {
            Ok(LlmResponse {
                parts: vec![LlmOutputPart::Text {
                    text: "answered".to_string(),
                    response_meta: None,
                }],
                response_metadata: Default::default(),
                ..LlmResponse::default()
            })
        })
        .build();
    let mut runtime = runtime_with_plugins_and_tools_and_host_and_store(
        Vec::new(),
        Arc::new(EmptyTools),
        transport,
        test_host_config(),
        Arc::clone(&runtime_store),
    )
    .await;

    crate::TurnInputStore::enqueue_pending_turn_input(
        store.as_ref(),
        crate::PendingTurnInputDraft::new(
            session_id,
            crate::TurnInputIngress::active_turn(
                live_turn_id.to_string(),
                crate::TurnInputCheckpointBoundary::AfterWork,
            ),
            crate::TurnInput::text("routed into the live turn"),
        ),
    )
    .await
    .expect("enqueue an input scoped to the live turn");

    store.fail_next_runtime_commit(crate::StoreError::SessionExecutionLeaseExpired {
        session_id: session_id.to_string(),
    });
    let error = runtime
        .run_turn_assembled(
            crate::TurnInput::text("run the turn that will be fenced at commit"),
            CancellationToken::new(),
            named_turn_scope(session_id, live_turn_id),
        )
        .await
        .expect_err("a fenced commit must fail the turn");
    assert_eq!(
        error.code,
        crate::RuntimeErrorCode::SessionExecutionLeaseLost
    );

    let pending = crate::TurnInputStore::list_pending_turn_inputs(store.as_ref(), session_id)
        .await
        .expect("list pending turn inputs");
    assert_eq!(
        pending.len(),
        2,
        "the routed input is still queued, and so is the fenced turn's own acceptance (ADR 0069)"
    );
    for row in &pending {
        assert_eq!(
            row.state,
            crate::TurnInputState::DeferredNextTurn,
            "the teardown of a turn that cannot commit must re-defer every input it held"
        );
        assert_eq!(
            row.ingress,
            crate::TurnInputIngress::NextTurn,
            "the repaired rows must be addressable by the next turn, not by the dead turn id"
        );
    }
}

#[tokio::test]
async fn dirty_execution_state_capture_failure_aborts_commit_and_cold_reopens_prior_state() {
    let executor = Arc::new(FailingCaptureExecutor {
        dirty: AtomicBool::new(true),
        fail_capture: AtomicBool::new(false),
        snapshot: std::sync::Mutex::new(b"committed-before-failure".to_vec()),
        restored: std::sync::Mutex::new(Vec::new()),
    });
    let protocol: Arc<dyn crate::plugin::ProtocolSessionPlugin> =
        Arc::new(RestoreExecutorFromRuntimeState {
            executor: Arc::clone(&executor),
        });
    let code_executor: Arc<dyn crate::plugin::CodeExecutorPlugin> = executor.clone();
    let protocol_factory = crate::testing::test_standard_protocol_factory_with_runtime_state(
        protocol,
        Some(code_executor),
    );
    let store = Arc::new(RecordingStore::default());
    let runtime_store: Arc<dyn crate::RuntimePersistence> = store.clone();
    let provider_executor = Arc::clone(&executor);
    let provider_call = Arc::new(AtomicUsize::new(0));
    let transport = TestProvider::builder()
        .kind("mock")
        .requires_streaming(true)
        .complete(move |_| {
            let executor = Arc::clone(&provider_executor);
            let call = provider_call.fetch_add(1, Ordering::SeqCst);
            async move {
                let text = match call {
                    0 => "first committed turn",
                    1 => {
                        *executor.snapshot.lock_recover() = b"dirty-after-baseline".to_vec();
                        executor.dirty.store(true, Ordering::SeqCst);
                        executor.fail_capture.store(true, Ordering::SeqCst);
                        "must not commit"
                    }
                    index => panic!("unexpected provider call {index}"),
                };
                Ok(LlmResponse {
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
    let mut runtime = runtime_with_plugins_and_tools_and_host_and_store(
        vec![protocol_factory],
        Arc::new(EmptyTools),
        transport,
        test_host_config(),
        Arc::clone(&runtime_store),
    )
    .await;

    runtime
        .run_turn_assembled(
            TurnInput::text("commit the baseline"),
            CancellationToken::new(),
            named_turn_scope("root", "execution-state-baseline"),
        )
        .await
        .expect("baseline turn commits its execution state");
    executor.dirty.store(false, Ordering::SeqCst);

    let error = runtime
        .run_turn_assembled(
            TurnInput::text("capture must fail"),
            CancellationToken::new(),
            named_turn_scope("root", "execution-state-capture-failure"),
        )
        .await
        .expect_err("dirty capture failure must abort before the turn commit");
    assert_eq!(
        error.code,
        crate::RuntimeErrorCode::ExecutionStateCaptureFailed
    );
    assert!(
        error
            .message
            .contains("failed to snapshot dirty execution state")
    );
    let durable = crate::store::load_persisted_session_state(store.as_ref())
        .await
        .expect("load baseline state")
        .expect("baseline state exists");
    assert_eq!(durable.head_revision, 1);
    assert_eq!(
        durable.execution_state_snapshot(),
        Some(b"committed-before-failure".as_slice())
    );

    drop(runtime);
    executor.fail_capture.store(false, Ordering::SeqCst);
    executor.dirty.store(false, Ordering::SeqCst);
    let reopen_protocol: Arc<dyn crate::plugin::ProtocolSessionPlugin> =
        Arc::new(RestoreExecutorFromRuntimeState {
            executor: Arc::clone(&executor),
        });
    let reopen_executor: Arc<dyn crate::plugin::CodeExecutorPlugin> = executor.clone();
    let reopen_factory = crate::testing::test_standard_protocol_factory_with_runtime_state(
        reopen_protocol,
        Some(reopen_executor),
    );
    let plugins = crate::PluginHost::new(vec![reopen_factory])
        .build_session("root")
        .expect("reopen plugins");
    let _reopened = LashRuntime::from_persistent_embedded_state(
        standard_test_policy(),
        test_host_config(),
        crate::PersistentRuntimeServices::new(plugins, runtime_store),
        durable,
        crate::testing::runtime_lease_owner(),
    )
    .await
    .expect("cold reopen restores the last committed execution state");
    assert_eq!(
        executor.restored.lock_recover().last().map(Vec::as_slice),
        Some(b"committed-before-failure".as_slice())
    );
}

#[tokio::test]
async fn caller_supplied_key_colliding_with_existing_frame_preserves_execution_state() {
    let executor = Arc::new(FailingCaptureExecutor {
        dirty: AtomicBool::new(true),
        fail_capture: AtomicBool::new(false),
        snapshot: std::sync::Mutex::new(b"live-frame-execution-state".to_vec()),
        restored: std::sync::Mutex::new(Vec::new()),
    });
    let protocol: Arc<dyn crate::plugin::ProtocolSessionPlugin> =
        Arc::new(SwitchBeforeLlmProtocol {
            executor: Some(Arc::clone(&executor)),
            frame_key_material: "caller-named-existing-frame".to_string(),
            switch_next: AtomicBool::new(true),
        });
    let code_executor: Arc<dyn crate::plugin::CodeExecutorPlugin> = executor.clone();
    let protocol_factory = crate::testing::test_standard_protocol_factory_with_runtime_state(
        protocol,
        Some(code_executor),
    );
    let store = Arc::new(RecordingStore::default());
    let runtime_store: Arc<dyn crate::RuntimePersistence> = store.clone();
    let transport = TestProvider::builder()
        .kind("mock")
        .requires_streaming(true)
        .complete(|_| async move {
            Ok(LlmResponse {
                parts: vec![LlmOutputPart::Text {
                    text: "follow-on must fail before commit".to_string(),
                    response_meta: None,
                }],
                response_metadata: Default::default(),
                ..LlmResponse::default()
            })
        })
        .build();
    let mut runtime = runtime_with_plugins_and_tools_and_host_and_store(
        vec![protocol_factory],
        Arc::new(EmptyTools),
        transport,
        test_host_config(),
        Arc::clone(&runtime_store),
    )
    .await;
    let colliding_frame_key = crate::FrameKey::from_caller_material("caller-named-existing-frame")
        .expect("non-empty caller material");
    let opened = runtime
        .open_agent_frame(crate::OpenAgentFrameRequest::new(
            colliding_frame_key.as_str(),
            crate::AgentFrameReason::initial(),
        ))
        .await
        .expect("pre-open caller-named frame");
    assert!(opened.opened, "caller-named collision target must exist");
    runtime.set_turn_phase_probe(Arc::new(FailCaptureAfterFirstCommittedTurn {
        executor: Arc::clone(&executor),
        committed_turns: AtomicUsize::new(0),
    }));
    let initial_frame_node_id = runtime
        .state
        .current_frame_node_id
        .clone()
        .expect("runtime initializes the current frame");

    let switched = runtime
        .run_turn_assembled(
            TurnInput::text("redrive an already materialized frame switch"),
            CancellationToken::new(),
            named_turn_scope("root", "already-current-frame-switch"),
        )
        .await
        .expect("an already-current frame switch remains an idempotent no-op");
    assert!(
        matches!(switched.outcome, TurnOutcome::AgentFrameSwitch { .. }),
        "unexpected no-op switch outcome: {switched:?}"
    );
    assert_eq!(
        runtime.state.current_frame_node_id.as_deref(),
        Some(initial_frame_node_id.as_str())
    );

    let durable = crate::store::load_persisted_session_state(store.as_ref())
        .await
        .expect("load no-op switch state")
        .expect("no-op switch state is durable");
    assert_eq!(
        durable.execution_state_snapshot(),
        Some(b"live-frame-execution-state".as_slice())
    );
    drop(runtime);

    let reopened_executor = Arc::new(FailingCaptureExecutor {
        dirty: AtomicBool::new(false),
        fail_capture: AtomicBool::new(false),
        snapshot: std::sync::Mutex::new(Vec::new()),
        restored: std::sync::Mutex::new(Vec::new()),
    });
    let reopen_protocol: Arc<dyn crate::plugin::ProtocolSessionPlugin> =
        Arc::new(SwitchBeforeLlmProtocol {
            executor: Some(Arc::clone(&reopened_executor)),
            frame_key_material: "caller-named-existing-frame".to_string(),
            switch_next: AtomicBool::new(true),
        });
    let reopen_code_executor: Arc<dyn crate::plugin::CodeExecutorPlugin> =
        reopened_executor.clone();
    let reopen_factory = crate::testing::test_standard_protocol_factory_with_runtime_state(
        reopen_protocol,
        Some(reopen_code_executor),
    );
    let plugins = crate::PluginHost::new(vec![reopen_factory])
        .build_session("root")
        .expect("cold-reopen plugins");
    let _reopened = LashRuntime::from_persistent_embedded_state(
        standard_test_policy(),
        test_host_config(),
        crate::PersistentRuntimeServices::new(plugins, runtime_store),
        durable,
        crate::testing::runtime_lease_owner(),
    )
    .await
    .expect("cold reopen restores the still-live frame execution state");
    assert_eq!(
        reopened_executor
            .restored
            .lock_recover()
            .last()
            .map(Vec::as_slice),
        Some(b"live-frame-execution-state".as_slice())
    );
}

#[tokio::test]
async fn materialized_frame_switch_clears_checkpoint_and_resets_resident_executor() {
    let executor = Arc::new(FailingCaptureExecutor {
        dirty: AtomicBool::new(true),
        fail_capture: AtomicBool::new(false),
        snapshot: std::sync::Mutex::new(b"abandoned-frame-execution-state".to_vec()),
        restored: std::sync::Mutex::new(Vec::new()),
    });
    let protocol: Arc<dyn crate::plugin::ProtocolSessionPlugin> =
        Arc::new(ResetExecutorOnSwitchProtocol {
            executor: Arc::clone(&executor),
            frame_key_material: "materialized-next-frame".to_string(),
            switch_next: AtomicBool::new(true),
        });
    let code_executor: Arc<dyn crate::plugin::CodeExecutorPlugin> = executor.clone();
    let protocol_factory = crate::testing::test_standard_protocol_factory_with_runtime_state(
        protocol,
        Some(code_executor),
    );
    let store = Arc::new(RecordingStore::default());
    let transport = TestProvider::builder()
        .kind("mock")
        .requires_streaming(true)
        .complete(|_| async move {
            panic!("a protocol-directed frame switch must finish before provider execution")
        })
        .build();
    let mut runtime = runtime_with_plugins_and_tools_and_host_and_store(
        vec![protocol_factory],
        Arc::new(EmptyTools),
        transport,
        test_host_config(),
        store.clone() as Arc<dyn crate::RuntimePersistence>,
    )
    .await;
    runtime.set_turn_phase_probe(Arc::new(FailCaptureAfterFirstCommittedTurn {
        executor: Arc::clone(&executor),
        committed_turns: AtomicUsize::new(0),
    }));
    executor.restored.lock_recover().clear();

    let switched = runtime
        .run_turn_assembled(
            TurnInput::text("switch to a distinct frame"),
            CancellationToken::new(),
            named_turn_scope("root", "materialized-frame-switch"),
        )
        .await
        .expect("materialized frame switch commits");
    assert!(matches!(
        switched.outcome,
        TurnOutcome::AgentFrameSwitch { .. }
    ));

    let durable = crate::store::load_persisted_session_state(store.as_ref())
        .await
        .expect("load materialized switch state")
        .expect("materialized switch state is durable");
    assert!(
        durable.execution_state_ref().is_none()
            && durable.execution_state_snapshot().is_none()
            && executor.restored.lock_recover().last().map(Vec::as_slice)
                == Some(b"fresh-frame-execution-state".as_slice()),
        "one committed switch must durably clear the checkpoint and reset the resident executor"
    );
}

#[tokio::test]
async fn capture_abort_releases_lease_and_claim_for_prompt_peer_reclaim() {
    let executor = Arc::new(FailingCaptureExecutor {
        dirty: AtomicBool::new(false),
        fail_capture: AtomicBool::new(false),
        snapshot: std::sync::Mutex::new(Vec::new()),
        restored: std::sync::Mutex::new(Vec::new()),
    });
    let protocol: Arc<dyn crate::plugin::ProtocolSessionPlugin> =
        Arc::new(RestoreExecutorFromRuntimeState {
            executor: Arc::clone(&executor),
        });
    let code_executor: Arc<dyn crate::plugin::CodeExecutorPlugin> = executor.clone();
    let protocol_factory = crate::testing::test_standard_protocol_factory_with_runtime_state(
        protocol,
        Some(code_executor),
    );
    let store = Arc::new(RecordingStore::default());
    let failing_transport = TestProvider::builder()
        .kind("mock")
        .requires_streaming(true)
        .complete(move |_| async move {
            Ok(LlmResponse {
                parts: vec![LlmOutputPart::Text {
                    text: "must not commit".to_string(),
                    response_meta: None,
                }],
                response_metadata: Default::default(),
                ..LlmResponse::default()
            })
        })
        .build();
    let mut first = runtime_with_plugins_and_tools_and_host_and_store(
        vec![Arc::clone(&protocol_factory)],
        Arc::new(EmptyTools),
        failing_transport,
        test_host_config(),
        store.clone() as Arc<dyn crate::RuntimePersistence>,
    )
    .await;
    first.set_turn_phase_probe(Arc::new(FailCaptureAfterEffectLoop {
        executor: Arc::clone(&executor),
    }));
    enqueue_idle_turn_input(store.as_ref(), "root", "peer must reclaim this input").await;

    let error = first
        .stream_next_queued_work(TurnOptions::new(
            CancellationToken::new(),
            named_turn_scope("root", "capture-abort-owner"),
        ))
        .await
        .expect_err("dirty capture aborts before commit");
    assert_eq!(
        error.code,
        crate::RuntimeErrorCode::ExecutionStateCaptureFailed
    );

    executor.fail_capture.store(false, Ordering::SeqCst);
    executor.dirty.store(false, Ordering::SeqCst);
    let mut peer = runtime_with_plugins_and_tools_and_host_and_store(
        vec![protocol_factory],
        Arc::new(EmptyTools),
        mock_provider(vec![MockCall {
            stream_events: Vec::new(),
            response: Ok(LlmResponse {
                parts: vec![LlmOutputPart::Text {
                    text: "peer reclaimed".to_string(),
                    response_meta: None,
                }],
                response_metadata: Default::default(),
                ..LlmResponse::default()
            }),
        }]),
        test_host_config(),
        store.clone() as Arc<dyn crate::RuntimePersistence>,
    )
    .await;

    let reclaimed = peer
        .stream_next_queued_work(TurnOptions::new(
            CancellationToken::new(),
            named_turn_scope("root", "capture-abort-peer"),
        ))
        .await
        .expect("peer reclaim must not wait for the lease TTL")
        .ran()
        .expect("peer immediately receives the abandoned input");
    assert_eq!(reclaimed.assistant_output.safe_text, "peer reclaimed");
}

#[tokio::test]
async fn follow_on_capture_failure_returns_the_committed_frame_and_handoff_is_retry_safe() {
    let executor = Arc::new(FailingCaptureExecutor {
        dirty: AtomicBool::new(false),
        fail_capture: AtomicBool::new(false),
        snapshot: std::sync::Mutex::new(Vec::new()),
        restored: std::sync::Mutex::new(Vec::new()),
    });
    let protocol: Arc<dyn crate::plugin::ProtocolSessionPlugin> =
        Arc::new(RestoreExecutorFromRuntimeState {
            executor: Arc::clone(&executor),
        });
    let code_executor: Arc<dyn crate::plugin::CodeExecutorPlugin> = executor.clone();
    let protocol_factory = crate::testing::test_standard_protocol_factory_with_runtime_state(
        protocol,
        Some(code_executor),
    );
    let store = Arc::new(RecordingStore::default());
    let call_index = Arc::new(AtomicUsize::new(0));
    let transport = TestProvider::builder()
        .kind("mock")
        .requires_streaming(true)
        .complete(move |_| {
            let call_index = Arc::clone(&call_index);
            async move {
                Ok(match call_index.fetch_add(1, Ordering::SeqCst) {
                    0 => LlmResponse {
                        parts: vec![LlmOutputPart::ToolCall {
                            call_id: "switch".to_string(),
                            tool_name: "terminal_tool_0".to_string(),
                            input_json: "{}".to_string(),
                            replay: None,
                        }],
                        response_metadata: Default::default(),
                        ..LlmResponse::default()
                    },
                    1 => LlmResponse {
                        parts: vec![LlmOutputPart::Text {
                            text: "recovered follow-on".to_string(),
                            response_meta: None,
                        }],
                        response_metadata: Default::default(),
                        ..LlmResponse::default()
                    },
                    index => panic!("unexpected provider call {index}"),
                })
            }
        })
        .build();
    let mut runtime = runtime_with_plugins_and_tools_and_host_and_store(
        vec![protocol_factory],
        Arc::new(TerminalControlTool {
            controls: vec![crate::ToolControl::SwitchAgentFrame {
                frame_key: crate::FrameKey::from_caller_material("capture-failure-frame")
                    .expect("non-empty caller material"),
                initial_nodes: Vec::new(),
                task: Some("recover committed handoff".to_string()),
            }],
        }),
        transport,
        test_host_config(),
        store.clone() as Arc<dyn crate::RuntimePersistence>,
    )
    .await;
    runtime.set_turn_phase_probe(Arc::new(FailCaptureAfterFirstCommittedTurn {
        executor: Arc::clone(&executor),
        committed_turns: AtomicUsize::new(0),
    }));
    enqueue_idle_turn_input(store.as_ref(), "root", "switch then fail capture").await;

    let committed = runtime
        .stream_next_queued_work(TurnOptions::new(
            CancellationToken::new(),
            named_turn_scope("root", "follow-on-capture-failure"),
        ))
        .await
        .expect("a follow-on pre-commit failure must not erase the committed frame")
        .ran()
        .expect("the committed frame is returned");
    assert!(matches!(
        committed.outcome,
        TurnOutcome::AgentFrameSwitch { .. }
    ));
    assert!(committed.errors.iter().any(|issue| {
        issue.code.as_deref() == Some("execution_state_capture_failed")
            && issue.retryable == Some(false)
    }));
    let durable = crate::store::SessionCommitStore::load_session(store.as_ref())
        .await
        .expect("load committed frame")
        .expect("committed frame exists");
    assert_eq!(durable.head_revision, 1);

    executor.fail_capture.store(false, Ordering::SeqCst);
    executor.dirty.store(false, Ordering::SeqCst);
    let recovered = runtime
        .stream_next_queued_work(TurnOptions::new(
            CancellationToken::new(),
            named_turn_scope("root", "retry-safe-committed-handoff"),
        ))
        .await
        .expect("retrying the logical queue call is safe")
        .ran()
        .expect("the durable handoff is reclaimed");
    assert_eq!(recovered.assistant_output.safe_text, "recovered follow-on");
    assert!(
        crate::store::QueuedWorkStore::list_queued_work(store.as_ref(), "root")
            .await
            .expect("queue after recovered handoff")
            .is_empty()
    );
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

    async fn execute(&self, call: crate::ToolCall<'_>) -> crate::ToolOutcome {
        match call.name {
            "rotate_surface" => {
                self.rotated.store(true, Ordering::SeqCst);
                crate::ToolOutcome::ok(json!({ "rotated": true })).with_control(
                    crate::ToolControl::SwitchAgentFrame {
                        frame_key: crate::FrameKey::from_caller_material("live-surface-frame")
                            .expect("non-empty caller material"),
                        initial_nodes: Vec::new(),
                        task: Some("call the newly available tool".to_string()),
                    },
                )
            }
            "new_after_rotation" => crate::ToolOutcome::ok(json!({ "called": call.name }))
                .with_control(crate::ToolControl::Finish {
                    value: crate::ToolValue::untrusted_json(json!("new tool executed")),
                }),
            "curated_before_rotation" | "hidden_after_rotation" => {
                crate::ToolOutcome::ok(json!({ "called": call.name }))
            }
            name => crate::ToolOutcome::err_fmt(format_args!("unknown rotating tool `{name}`")),
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
            crate::plugin::SessionCreationConfig {
                authority: crate::plugin::SessionAuthorityContext {
                    tool_access: crate::SessionToolAccess {
                        tools: Vec::new(),
                        hidden_tools: ["hidden_after_rotation".to_string()].into_iter().collect(),
                    },
                    ..crate::plugin::SessionAuthorityContext::default()
                },
                ..Default::default()
            },
        )
        .expect("frame child plugins");
    let mut runtime = LashRuntime::from_embedded_state(
        standard_test_policy(),
        test_host_config(),
        crate::RuntimeServices::new(plugins),
        RuntimeSessionState::new(crate::SessionPolicy::new(crate::TurnBudget::Unbounded)),
        crate::testing::runtime_lease_owner(),
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
            &crate::testing::mock_attempt_context(),
        )
        .await;
    assert!(
        !hidden_result.is_success(),
        "new hidden id must not execute after frame rotation: {hidden_result:?}"
    );
}

struct ExpireLeaseAtPreparedTurn {
    clock: Arc<ManualClock>,
    expired: AtomicBool,
}

impl ExpireLeaseAtPreparedTurn {
    fn new(clock: Arc<ManualClock>) -> Self {
        Self {
            clock,
            expired: AtomicBool::new(false),
        }
    }
}

impl crate::runtime::RuntimeTurnPhaseProbe for ExpireLeaseAtPreparedTurn {
    fn begin(&self, phase: crate::runtime::RuntimeTurnPhase) {
        if phase == crate::runtime::RuntimeTurnPhase::PreparedTurn
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

struct ExpireLeaseAtSecondTurnFinalizedHook {
    clock: Arc<ManualClock>,
    finalized_hooks: AtomicUsize,
}

impl ExpireLeaseAtSecondTurnFinalizedHook {
    fn new(clock: Arc<ManualClock>) -> Self {
        Self {
            clock,
            finalized_hooks: AtomicUsize::new(0),
        }
    }
}

impl crate::runtime::RuntimeTurnPhaseProbe for ExpireLeaseAtSecondTurnFinalizedHook {
    fn begin(&self, _phase: crate::runtime::RuntimeTurnPhase) {}

    fn end(&self, _phase: crate::runtime::RuntimeTurnPhase) {}

    fn begin_named(&self, phase: &str) {
        if phase.starts_with("plugin_hook.turn_finalized.")
            && self.finalized_hooks.fetch_add(1, Ordering::SeqCst) == 1
        {
            self.clock
                .advance_ms(crate::LeaseTimings::default().ttl_ms() + 1);
        }
    }
}

struct PauseAtPreparedTurn {
    entered: Arc<AtomicBool>,
    release: Arc<AtomicBool>,
}

impl crate::runtime::RuntimeTurnPhaseProbe for PauseAtPreparedTurn {
    fn begin(&self, phase: crate::runtime::RuntimeTurnPhase) {
        if phase != crate::runtime::RuntimeTurnPhase::PreparedTurn {
            return;
        }
        self.entered.store(true, Ordering::SeqCst);
        while !self.release.load(Ordering::SeqCst) {
            std::thread::yield_now();
        }
    }

    fn end(&self, _phase: crate::runtime::RuntimeTurnPhase) {}
}

struct PauseFirstProductCommitAttempt {
    entered: Arc<AtomicBool>,
    release: Arc<AtomicBool>,
    attempts: AtomicUsize,
}

impl crate::runtime::RuntimeTurnPhaseProbe for PauseFirstProductCommitAttempt {
    fn begin(&self, _phase: crate::runtime::RuntimeTurnPhase) {}

    fn end(&self, _phase: crate::runtime::RuntimeTurnPhase) {}

    fn begin_named(&self, phase: &str) {
        if phase == "commit_admission.product_attempt"
            && self.attempts.fetch_add(1, Ordering::SeqCst) == 0
        {
            self.entered.store(true, Ordering::SeqCst);
            while !self.release.load(Ordering::SeqCst) {
                std::thread::yield_now();
            }
        }
    }
}

struct PauseAfterEffectLoop {
    entered: Arc<AtomicBool>,
    release: Arc<AtomicBool>,
}

struct CasSurvivorIntentTools {
    calls: Arc<AtomicUsize>,
}

struct ParentEndFailureIntentTool;

fn parent_end_failure_intent_tool() -> crate::ToolDefinition {
    crate::ToolDefinition::raw(
        "tool:parent_end_failure_intent",
        "parent_end_failure_intent",
        "Start a process whose parent-end action exercises cancelled-turn teardown.",
        crate::ToolDefinition::default_input_schema(),
        serde_json::json!({"type": "object", "additionalProperties": true}),
    )
}

#[async_trait::async_trait]
impl crate::ToolProvider for ParentEndFailureIntentTool {
    fn tool_manifests(&self) -> Vec<crate::ToolManifest> {
        vec![parent_end_failure_intent_tool().manifest()]
    }

    fn resolve_contract(&self, name: &str) -> Option<Arc<crate::ToolContract>> {
        (name == "parent_end_failure_intent")
            .then(|| Arc::new(parent_end_failure_intent_tool().contract()))
    }

    async fn execute(&self, _call: crate::ToolCall<'_>) -> crate::ToolOutcome {
        panic!("the parent-end failure witness must use AttemptContext")
    }

    async fn execute_attempt(&self, call: crate::ToolCall<'_>) -> crate::ToolAttemptOutcome {
        crate::ToolAttemptOutcome::done(
            crate::ToolOutcomeDone::ok(serde_json::json!({"started": true})),
            crate::ToolIntents::v1(vec![crate::ToolIntent::StartProcess(Box::new(
                crate::StartProcessIntent {
                    session_id: call.context.session_id().to_string(),
                    request: crate::ProcessStartRequest::external(
                        "cancelled-turn-parent-end-child",
                        crate::ProcessOriginator::host_scoped("parent-end-failure-witness"),
                        serde_json::json!({"witness": true}),
                    ),
                    on_parent_end: crate::ProcessParentEndPolicy::Abandon,
                },
            ))]),
        )
    }
}

fn cas_survivor_intent_tool() -> crate::ToolDefinition {
    crate::ToolDefinition::raw(
        "tool:cas_survivor_intent",
        "cas_survivor_intent",
        "Emit durable evidence before the enclosing turn competes on head CAS.",
        crate::ToolDefinition::default_input_schema(),
        serde_json::json!({"type": "object", "additionalProperties": true}),
    )
}

#[async_trait::async_trait]
impl crate::ToolProvider for CasSurvivorIntentTools {
    fn tool_manifests(&self) -> Vec<crate::ToolManifest> {
        vec![cas_survivor_intent_tool().manifest()]
    }

    fn resolve_contract(&self, name: &str) -> Option<Arc<crate::ToolContract>> {
        (name == "cas_survivor_intent").then(|| Arc::new(cas_survivor_intent_tool().contract()))
    }

    async fn execute(&self, _call: crate::ToolCall<'_>) -> crate::ToolOutcome {
        panic!("the lease/CAS survivor law must use AttemptContext")
    }

    async fn execute_attempt(&self, call: crate::ToolCall<'_>) -> crate::ToolAttemptOutcome {
        self.calls.fetch_add(1, Ordering::SeqCst);
        crate::ToolAttemptOutcome::done(
            crate::ToolOutcomeDone::ok(serde_json::json!({"intent": "committed"})),
            crate::ToolIntents::v1(vec![crate::ToolIntent::EmitProcessEvent(
                crate::EmitProcessEventIntent {
                    session_id: call.context.session_id().to_string(),
                    process_id: "cas-survivor-intent-target".to_string(),
                    event_type: "intent.survivor.committed".to_string(),
                    payload: serde_json::json!({"survives": true}),
                },
            )]),
        )
    }
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
        if phase == crate::runtime::RuntimeTurnPhase::PostCommitDelivery
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
    let runtime = runtime_with_plugins_and_tools_and_host_and_store(
        Vec::new(),
        Arc::new(EmptyTools),
        transport,
        test_host_config(),
        runtime_store,
    )
    .await;
    (runtime, store)
}

async fn standard_runtime_with_transport_and_queue_store_for_session(
    transport: TestProvider,
    session_id: &str,
) -> (LashRuntime, Arc<RecordingStore>) {
    let store = Arc::new(RecordingStore::default());
    let runtime = TestRuntime::new(transport)
        .tools(Arc::new(EmptyTools))
        .host(test_host_config())
        .store(store.clone())
        .with_session_id(session_id)
        .build()
        .await;
    (runtime, store)
}

async fn standard_runtime_with_transport_and_queue_store_clock(
    transport: TestProvider,
    clock: Arc<dyn crate::Clock>,
) -> (LashRuntime, Arc<RecordingStore>) {
    let store = Arc::new(RecordingStore::with_clock(clock));
    let runtime_store: Arc<dyn crate::store::RuntimePersistence> = store.clone();
    let runtime = runtime_with_plugins_and_tools_and_host_and_store(
        Vec::new(),
        Arc::new(EmptyTools),
        transport,
        test_host_config(),
        runtime_store,
    )
    .await;
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

async fn enqueue_config_patch_command(
    store: &RecordingStore,
    session_id: &str,
    patch: crate::runtime::ApplyConfigPatch,
) -> crate::QueuedWorkBatch {
    crate::store::QueuedWorkStore::enqueue_queued_work(
        store,
        crate::QueuedWorkBatchDraft::new(
            session_id.to_string(),
            crate::DeliveryPolicy::AfterCurrentTurnCommit,
            vec![crate::QueuedWorkPayload::session_command(
                crate::SessionCommand::ApplyConfigPatch {
                    patch: Box::new(patch),
                },
            )],
        ),
    )
    .await
    .expect("enqueue config patch command")
}

#[tokio::test]
async fn queued_config_patches_coalesce_into_one_head_commit() {
    let (mut runtime, store) =
        standard_runtime_with_transport_and_queue_store(mock_provider(Vec::new())).await;
    let models = ["queued-model-a", "queued-model-b", "queued-model-c"];
    for model in models {
        enqueue_config_patch_command(
            store.as_ref(),
            "root",
            crate::runtime::ApplyConfigPatch {
                model: Some(
                    crate::ModelSpec::builder(model)
                        .context_window_tokens(32_000)
                        .build()
                        .expect("model"),
                ),
                ..crate::runtime::ApplyConfigPatch::default()
            },
        )
        .await;
    }
    let commits_before = *store.runtime_commit_count.lock_recover();
    let owner = lease_owner("config-patch-coalescing");
    let lease = crate::store::SessionExecutionLeaseStore::try_claim_session_execution_lease(
        store.as_ref(),
        "root",
        &owner,
        "config-patch-coalescing-executor",
        crate::LeaseTimings::default().ttl_ms(),
    )
    .await
    .expect("claim session execution lease")
    .acquired()
    .expect("session execution lease");

    runtime
        .drain_next_session_command(&lease.fence())
        .await
        .expect("drain coalesced config patches")
        .expect("one receipt from the coalesced claim");

    assert_eq!(
        *store.runtime_commit_count.lock_recover(),
        commits_before + 1,
        "N config commands must share exactly one head commit"
    );
    assert!(
        crate::store::QueuedWorkStore::list_queued_work(store.as_ref(), "root")
            .await
            .expect("list settled config commands")
            .is_empty(),
        "every independently accepted command must settle its own batch"
    );
    assert_eq!(runtime.session_policy().model.id, "queued-model-c");
}

#[tokio::test]
async fn config_settlement_distinguishes_enqueue_rejection_from_durable_completion() {
    let mut runtime = runtime_with_plugins(Vec::new(), mock_provider(Vec::new())).await;
    let original_model = runtime.session_policy().model.clone();
    let outcome = runtime
        .submit_apply_config_patch_with_idempotency_key(
            crate::runtime::ApplyConfigPatch {
                model: Some(
                    crate::ModelSpec::builder("must-not-publish")
                        .context_window_tokens(32_000)
                        .build()
                        .expect("model"),
                ),
                ..crate::runtime::ApplyConfigPatch::default()
            },
            "",
        )
        .await
        .expect("typed submission outcome");

    let crate::runtime::SessionCommandSettlement::Rejected(rejection) = outcome else {
        panic!("empty idempotency key must be rejected before durable acceptance");
    };
    assert_eq!(
        rejection.code,
        crate::RuntimeErrorCode::SessionCommandIdempotencyKey
    );
    assert_eq!(runtime.session_policy().model, original_model);

    let durable = runtime
        .submit_apply_config_patch_with_idempotency_key(
            crate::runtime::ApplyConfigPatch {
                model: Some(
                    crate::ModelSpec::builder("durable-inline")
                        .context_window_tokens(32_000)
                        .build()
                        .expect("model"),
                ),
                ..crate::runtime::ApplyConfigPatch::default()
            },
            "durable-inline",
        )
        .await
        .expect("durable settlement");
    assert!(matches!(
        durable,
        crate::runtime::SessionCommandSettlement::Durable(_)
    ));
    assert_eq!(runtime.session_policy().model.id, "durable-inline");
}

fn turn_budget_config_mutator(turn_budget: crate::TurnBudget) -> Arc<dyn crate::PluginFactory> {
    Arc::new(RuntimeTestPluginFactory {
        build: Arc::new(move |_| {
            Ok(Arc::new(RuntimeTestPlugin {
                before_turn: None,
                checkpoint: None,
                tool_result_projector: None,
                runtime_event: None,
                external_registrar: Some(Arc::new(move |reg| {
                    reg.session()
                        .config_mutator(Arc::new(move |_ctx, mut policy| {
                            Box::pin(async move {
                                policy.turn_budget = turn_budget;
                                Ok(policy)
                            })
                        }));
                    Ok(())
                })),
            }))
        }),
    })
}

#[tokio::test]
async fn plugin_turn_budget_mutation_survives_park_and_reload() {
    let store = Arc::new(RecordingStore::default());
    let runtime_store: Arc<dyn crate::RuntimePersistence> = store.clone();
    let persisted_budget = crate::TurnBudget::bounded(7);
    let mut runtime = runtime_with_plugins_and_tools_and_host_and_store(
        vec![turn_budget_config_mutator(persisted_budget)],
        Arc::new(EmptyTools),
        mock_provider(Vec::new()),
        test_host_config(),
        Arc::clone(&runtime_store),
    )
    .await;

    runtime
        .update_session_config(crate::SessionConfigPatch {
            model: Some(
                crate::ModelSpec::builder("turn-budget-mutation-trigger")
                    .context_window_tokens(32_000)
                    .build()
                    .expect("model"),
            ),
            ..crate::SessionConfigPatch::default()
        })
        .await
        .expect("plugin turn-budget mutation settles");
    assert_eq!(runtime.session_policy().turn_budget, persisted_budget);
    drop(runtime.park().await.expect("park mutated session"));

    let reloaded_state = crate::load_persisted_session_state(runtime_store.as_ref())
        .await
        .expect("load parked session")
        .expect("parked session exists");
    let plugin_host = crate::PluginHost::new(vec![turn_budget_config_mutator(persisted_budget)]);
    let plugins = match reloaded_state.plugin_snapshot() {
        Some(snapshot) => plugin_host.rematerialize_session(
            "root",
            snapshot,
            crate::plugin::RecordedSessionConfig::new(reloaded_state.protocol_turn_options.clone()),
        ),
        None => plugin_host.build_session("root"),
    }
    .expect("reloaded plugins");
    let reloaded = crate::LashRuntime::from_persistent_embedded_state(
        standard_test_policy(),
        test_host_config(),
        crate::PersistentRuntimeServices::new(plugins, runtime_store),
        reloaded_state,
        crate::testing::runtime_lease_owner(),
    )
    .await
    .expect("reload parked runtime");
    assert_eq!(
        reloaded.session_policy().turn_budget,
        persisted_budget,
        "plugin-mutated durable budget must survive cold reload"
    );
}

#[tokio::test]
async fn every_session_config_patch_emits_a_lifecycle_event() {
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
        .build()
        .into_handle();
    let alt_model = crate::ModelSpec::builder("alt-model")
        .context_window_tokens(123_456)
        .build()
        .expect("valid model spec");
    runtime
        .update_session_config(crate::SessionConfigPatch {
            model: Some(alt_model.clone()),
            ..Default::default()
        })
        .await
        .expect("update model config");
    runtime
        .update_session_config(crate::SessionConfigPatch {
            provider: Some(alt_provider),
            ..Default::default()
        })
        .await
        .expect("update provider config");

    assert_eq!(observed.lock().await.len(), 2);

    let combined_provider = TestProvider::builder()
        .kind("combined")
        .complete_error("combined provider not wired")
        .build()
        .into_handle();
    let combined_model = crate::ModelSpec::builder("combined-model")
        .context_window_tokens(234_567)
        .build()
        .expect("valid combined model spec");
    runtime
        .update_session_config(crate::SessionConfigPatch {
            provider: Some(combined_provider),
            model: Some(combined_model.clone()),
            ..Default::default()
        })
        .await
        .expect("update combined config");

    assert_eq!(observed.lock().await.len(), 3);

    let prompt = crate::PromptLayer::new().with_contribution(crate::PromptContribution::guidance(
        "Patch",
        "prompt-only session config",
    ));
    runtime
        .update_session_config(crate::SessionConfigPatch::with_prompt(prompt.clone()))
        .await
        .expect("update prompt config");

    assert_eq!(observed.lock().await.len(), 4);

    let generation = crate::GenerationOptions {
        seed: Some(42),
        ..Default::default()
    };
    runtime
        .update_session_config(crate::SessionConfigPatch {
            generation: Some(crate::GenerationOverlay::Replace(generation.clone())),
            ..Default::default()
        })
        .await
        .expect("update generation config");

    assert_eq!(observed.lock().await.len(), 5);

    let helper_template =
        crate::PromptTemplate::new(vec![crate::PromptTemplateSection::untitled(vec![
            crate::PromptTemplateEntry::text("prompt helper template"),
        ])]);
    runtime
        .set_prompt_template(helper_template.clone())
        .await
        .expect("set prompt template");

    let changes = observed.lock().await;
    assert_eq!(changes.len(), 6);
    let (previous, current) = &changes[0];
    assert_eq!(previous.provider_id, "mock");
    assert_eq!(current.provider_id, "mock");
    assert_eq!(current.model.id, "alt-model");
    assert_ne!(
        previous.context_window_tokens(),
        current.context_window_tokens()
    );
    let (previous, current) = &changes[1];
    assert_eq!(previous.provider_id, "mock");
    assert_eq!(previous.model.id, "alt-model");
    assert_eq!(current.provider_id, "alt");
    assert_eq!(current.model.id, "alt-model");
    let (previous, current) = &changes[2];
    assert_eq!(previous.provider_id, "alt");
    assert_eq!(previous.model.id, "alt-model");
    assert_eq!(current.provider_id, "combined");
    assert_eq!(current.model, combined_model);
    let (previous, current) = &changes[3];
    assert_eq!(previous.model.id, "combined-model");
    assert_eq!(current.prompt, prompt);
    let (previous, current) = &changes[4];
    assert_eq!(previous.prompt, prompt);
    assert_eq!(current.generation, generation);
    let (previous, current) = &changes[5];
    assert_eq!(previous.generation, generation);
    assert_eq!(
        current.prompt.template,
        Some(helper_template),
        "prompt helper changes emit SessionConfigChanged"
    );
}

#[tokio::test]
async fn turn_provider_override_does_not_persist_into_session_policy_or_agent_frame() {
    let mut runtime = runtime_with_plugins(Vec::new(), mock_provider(Vec::new())).await;
    let alt_provider = TestProvider::builder()
        .kind("alt")
        .complete(|_| async {
            Ok(LlmResponse {
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
    assert_eq!(
        runtime.state.effective_policy().recorded_provider_id(),
        "mock"
    );
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
                            crate::TurnPluginDirective::EnqueueMessages(
                                crate::EnqueueMessagesDirective {
                                    messages: vec![crate::PluginMessage::text(
                                        crate::MessageRole::System,
                                        "plugin preface",
                                    )],
                                },
                            ),
                            crate::TurnPluginDirective::AbortTurn(crate::AbortTurnDirective {
                                code: "blocked".to_string(),
                                message: "plugin stopped the turn".to_string(),
                            }),
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
    // The committed turn input carries typed provenance so a host that rendered
    // its own row for this turn recognizes this copy without parsing the
    // runtime-minted message id (FIG-972). The direct path has no durable turn
    // input behind it, so `input_id` is absent.
    assert_eq!(
        user_message.origin,
        Some(crate::MessageOrigin::TurnInput {
            turn_id: "skill-command-visibility-turn".to_string(),
            input_id: None,
        })
    );
}

#[tokio::test]
async fn retryable_llm_failures_exhaust_and_fail_turn() {
    let transport = mock_provider(vec![
        MockCall {
            stream_events: Vec::new(),
            response: Err(
                crate::llm::transport::LlmTransportError::new("provider unavailable")
                    .with_retry_verdict(
                        crate::llm::transport::TransportRetryVerdict::RetryableTransient,
                    )
                    .with_code("http_500"),
            ),
        },
        MockCall {
            stream_events: Vec::new(),
            response: Err(
                crate::llm::transport::LlmTransportError::new("provider unavailable")
                    .with_retry_verdict(
                        crate::llm::transport::TransportRetryVerdict::RetryableTransient,
                    )
                    .with_code("http_500"),
            ),
        },
        MockCall {
            stream_events: Vec::new(),
            response: Err(
                crate::llm::transport::LlmTransportError::new("provider unavailable")
                    .with_retry_verdict(
                        crate::llm::transport::TransportRetryVerdict::RetryableTransient,
                    )
                    .with_code("http_500"),
            ),
        },
        MockCall {
            stream_events: Vec::new(),
            response: Err(
                crate::llm::transport::LlmTransportError::new("provider unavailable")
                    .with_retry_verdict(
                        crate::llm::transport::TransportRetryVerdict::RetryableTransient,
                    )
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
    // A normal user message that records which turn absorbed it, not a plugin or
    // process injection (FIG-972).
    assert!(matches!(
        admitted[0].origin.as_ref(),
        Some(crate::MessageOrigin::TurnInput { turn_id, input_id })
            if turn_id == "queued-checkpoint-turn" && input_id.is_some()
    ));
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
                captured_requests.lock_recover().push(request);
                let call = captured_calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                let text = if call == 0 {
                    "First answer."
                } else {
                    "Second answer."
                };
                Ok(LlmResponse {
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

    let requests = requests.lock_recover().clone();
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
                            Ok(vec![crate::TurnPluginDirective::EnqueueMessages(
                                crate::EnqueueMessagesDirective {
                                    messages: vec![crate::PluginMessage::text(
                                        crate::MessageRole::System,
                                        "checkpoint injected",
                                    )],
                                },
                            )])
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
                        Ok(vec![crate::TurnPluginDirective::AbortTurn(
                            crate::AbortTurnDirective {
                                code: "checkpoint_rejected".to_string(),
                                message: "reject checkpoint delivery".to_string(),
                            },
                        )])
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
    // The turn's own input is an acceptance too (ADR 0069), so it is applied
    // and reported; what must not appear is application evidence for the input
    // this checkpoint failed to admit.
    assert!(
        turn_events
            .snapshot()
            .iter()
            .all(|activity| match &activity.event {
                crate::TurnEvent::QueuedInputAccepted { applications } => applications
                    .iter()
                    .all(|application| application.input_id != admitted.input_id),
                _ => true,
            }),
        "a rejected checkpoint must not emit live application evidence"
    );
    assert!(
        crate::store::TurnInputStore::list_turn_input_applications(store.as_ref(), "root")
            .await
            .expect("list rejected checkpoint applications")
            .iter()
            .all(|application| application.input_id != admitted.input_id),
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
        ) -> Result<(), crate::test_support::AttachmentSourcePolicyError> {
            if matches!(producer, crate::AttachmentProducer::Host) {
                return Err(crate::test_support::AttachmentSourcePolicyError {
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
                        Ok(vec![crate::TurnPluginDirective::EnqueueMessages(
                            crate::EnqueueMessagesDirective {
                                messages: vec![message],
                            },
                        )])
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
    // The turn's own input is an acceptance too (ADR 0069), so it is applied
    // and reported; what must not appear is application evidence for the input
    // this checkpoint failed to admit.
    assert!(
        turn_events
            .snapshot()
            .iter()
            .all(|activity| match &activity.event {
                crate::TurnEvent::QueuedInputAccepted { applications } => applications
                    .iter()
                    .all(|application| application.input_id != admitted.input_id),
                _ => true,
            }),
        "a failed checkpoint attachment must not emit live application evidence"
    );
    assert!(
        crate::store::TurnInputStore::list_turn_input_applications(store.as_ref(), "root")
            .await
            .expect("list attachment-failed checkpoint applications")
            .iter()
            .all(|application| application.input_id != admitted.input_id),
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
    // The injected input keeps the normal user-message representation — no
    // plugin or process origin — and records which turn absorbed it and which
    // durable input it came from (FIG-972).
    let crate::MessageOrigin::TurnInput { turn_id, input_id } = follow_up
        .origin
        .as_ref()
        .expect("committed injected input carries turn-input provenance")
    else {
        panic!("injected input must use the normal user-message representation");
    };
    assert_eq!(turn_id, "injection-accepted-turn");
    let input_id = input_id
        .as_deref()
        .expect("queued ingress records the durable input id");
    assert_eq!(follow_up.id, crate::runtime::ingress_message_id(input_id));
    let opening = projected
        .iter()
        .find(|message| {
            message.role == crate::MessageRole::User
                && message.parts.iter().any(|part| part.content == "hello")
        })
        .expect("committed opening input");
    // The opening input entered the same way (ADR 0069): its own acceptance,
    // its own durable id, distinct from the one injected at the checkpoint.
    let crate::MessageOrigin::TurnInput {
        turn_id: opening_turn_id,
        input_id: opening_input_id,
    } = opening
        .origin
        .as_ref()
        .expect("committed opening input carries turn-input provenance")
    else {
        panic!("the opening input must use the normal user-message representation");
    };
    assert_eq!(opening_turn_id, "injection-accepted-turn");
    let opening_input_id = opening_input_id
        .as_deref()
        .expect("a direct turn is admitted durably before it drives");
    assert_ne!(opening_input_id, input_id);
    assert_eq!(
        opening.id,
        crate::runtime::ingress_message_id(opening_input_id)
    );
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
                    .lock_recover()
                    .push(request.messages.clone());
                let text = match captured_calls.fetch_add(1, Ordering::SeqCst) {
                    0 => "first turn complete",
                    1 => "deferred input complete",
                    other => panic!("unexpected provider call {other}"),
                };
                Ok(LlmResponse {
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
    runtime.set_turn_phase_probe(Arc::new(PauseAtPreparedTurn {
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
        .ran()
        .expect("deferred input starts a turn");

    let requests = requests.lock_recover();
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
        .expect("command-only drain succeeds")
        .ran();

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

#[tokio::test]
async fn no_queued_work_submit_defers_without_refreshing_resident_state() {
    let (mut runtime, store) =
        standard_runtime_with_transport_and_queue_store(mock_provider(Vec::new())).await;
    let full_loads_before = store.load_session_count();
    let head_reads_before = store.load_session_head_meta_count();

    let receipt = runtime
        .submit_session_command(
            crate::SessionCommand::RefreshToolCatalog {
                reason: "deferred queued lane".to_string(),
            },
            "deferred-queued-command",
        )
        .await
        .expect("NoQueuedWork leaves the durable command pending");

    assert_eq!(store.load_session_count(), full_loads_before);
    assert_eq!(store.load_session_head_meta_count(), head_reads_before);
    let pending = crate::store::QueuedWorkStore::list_queued_work(store.as_ref(), "root")
        .await
        .expect("inspect deferred durable command");
    assert_eq!(pending.len(), 1);
    assert_eq!(pending[0].batch_id, receipt.batch_id);
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
                captured_requests.lock_recover().push(req);
                let call = captured_calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                let text = if call == 0 {
                    "turn input response"
                } else {
                    "wake checkpoint response"
                };
                Ok(LlmResponse {
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
        .process_registry()
        .cloned()
        .expect("process registry");
    let target_scope = crate::SessionScope::new("root");
    registry
        .register_process(
            crate::ProcessRegistration::new(
                "wake-after-user-input",
                crate::ProcessInput::External {
                    metadata: serde_json::Value::Null,
                },
                crate::RecoveryContract::ExternallyOwned,
                crate::ProcessProvenance::session(target_scope.clone()),
            )
            .with_extra_event_types([process_wake_event_type()])
            .with_wake_session_id(Some(target_scope.session_id.clone())),
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
        ),
    )
    .await;

    let drained = runtime
        .stream_next_queued_work(TurnOptions::new(
            CancellationToken::new(),
            named_turn_scope("root", "next-input-before-wake-drain"),
        ))
        .await
        .expect("queued drain succeeds")
        .ran()
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

    let requests = requests.lock_recover().clone();
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
        .process_registry()
        .cloned()
        .expect("process registry");
    let target_scope = crate::SessionScope::new("root");
    registry
        .register_process(
            crate::ProcessRegistration::new(
                "selected-wake",
                crate::ProcessInput::External {
                    metadata: serde_json::Value::Null,
                },
                crate::RecoveryContract::ExternallyOwned,
                crate::ProcessProvenance::session(target_scope.clone()),
            )
            .with_extra_event_types([process_wake_event_type()])
            .with_wake_session_id(Some(target_scope.session_id.clone())),
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
        ),
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
    // Commit admission is process-wide and keyed by session id. Keep this
    // cancellation rendezvous out of the shared `root` lane so unrelated
    // libtest cases cannot make its final commit contend with their turn.
    const SESSION_ID: &str = "process-wake-cancelled";

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
                captured_requests.lock_recover().push(req);
                let call = captured_calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                if call == 0 {
                    return Ok(LlmResponse {
                        parts: vec![LlmOutputPart::Text {
                            text: "initial queued input response".to_string(),
                            response_meta: None,
                        }],
                        response_metadata: Default::default(),
                        ..LlmResponse::default()
                    });
                }
                if let Some(tx) = captured_wake_started_tx.lock_recover().take() {
                    let _ = tx.send(());
                }
                std::future::pending::<Result<LlmResponse, _>>().await
            }
        })
        .build();
    let (mut runtime, store) =
        standard_runtime_with_transport_and_queue_store_for_session(transport, SESSION_ID).await;
    let queued_input =
        enqueue_idle_turn_input(store.as_ref(), SESSION_ID, "cancel with wake pending").await;
    let registry = runtime
        .host
        .process_registry()
        .cloned()
        .expect("process registry");
    let target_scope = crate::SessionScope::new(SESSION_ID);
    registry
        .register_process(
            crate::ProcessRegistration::new(
                "cancel-claimed-wake",
                crate::ProcessInput::External {
                    metadata: serde_json::Value::Null,
                },
                crate::RecoveryContract::ExternallyOwned,
                crate::ProcessProvenance::session(target_scope.clone()),
            )
            .with_extra_event_types([process_wake_event_type()])
            .with_wake_session_id(Some(target_scope.session_id.clone())),
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
        ),
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
            named_turn_scope(SESSION_ID, "cancel-claimed-wake-drain"),
        )),
    )
    .await
    .expect("cancelled wake drain should finish")
    .expect("cancelled wake drain should not error")
    .ran()
    .expect("cancelled queued input turn should still assemble");
    canceller.await.expect("canceller task");

    assert!(matches!(
        drained.outcome,
        TurnOutcome::Stopped(TurnStop::Cancelled { .. })
    ));
    assert!(
        crate::store::TurnInputStore::list_pending_turn_inputs(store.as_ref(), SESSION_ID)
            .await
            .expect("pending inputs after cancellation")
            .is_empty(),
        "queued input `{}` should be completed by the cancelled turn",
        queued_input.input_id
    );
    assert!(
        crate::store::QueuedWorkStore::list_queued_work(store.as_ref(), SESSION_ID)
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
                named_turn_scope(SESSION_ID, "after-cancel-claimed-wake-drain"),
            ))
            .await
            .expect("post-cancel drain should succeed")
            .ran()
            .is_none(),
        "neither the cancelled input nor the claimed wake should replay"
    );
    let requests = requests.lock_recover().clone();
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
// execution lease's generation; the claim carries no TTL of its own. So a turn
// that claims a batch at one checkpoint, stalls past the (tiny) lease TTL --
// here a slow provider call, while the session lease keeps renewing on its
// background cadence and preserves its generation -- then crosses another
// checkpoint re-runs `claim_ready_queued_work` under the *same* live generation,
// which can never self-steal its own rows. At finalization the original claim
// still owns its rows and the commit succeeds. Before generation fencing this
// failed with `QueuedWorkClaimExpired` because the claim expired under the
// stalled owner.
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
    let mut config = crate::RuntimeHostConfig::in_memory(
        crate::CommitBudget::bounded(1024 * 1024, 512),
        crate::QueuedWorkBatchingConfig::new(1),
    )
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

    // The wake batch is the queued work the turn claims mid-flight at an
    // active-turn checkpoint.
    let registry = runtime
        .host
        .process_registry()
        .cloned()
        .expect("process registry");
    let target_scope = crate::SessionScope::new("root");
    registry
        .register_process(
            crate::ProcessRegistration::new(
                "stalled-turn-wake",
                crate::ProcessInput::External {
                    metadata: serde_json::Value::Null,
                },
                crate::RecoveryContract::ExternallyOwned,
                crate::ProcessProvenance::session(target_scope.clone()),
            )
            .with_extra_event_types([process_wake_event_type()])
            .with_wake_session_id(Some(target_scope.session_id.clone())),
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
        ),
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
            .ran()
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
                requests.lock_recover().push(request);
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
                        parts: vec![LlmOutputPart::Text {
                            text: "follow-on complete".to_string(),
                            response_meta: None,
                        }],
                        response_metadata: Default::default(),
                        ..LlmResponse::default()
                    }),
                    2 => Ok(LlmResponse {
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
                frame_key: crate::FrameKey::from_caller_material("queued-follow-frame")
                    .expect("non-empty caller material"),
                initial_nodes: Vec::new(),
                task: Some("run follow-on task".to_string()),
            }],
        }),
        transport,
        test_host_config(),
        runtime_store,
    )
    .await;
    let first = enqueue_idle_turn_input(store.as_ref(), "root", "first queued turn").await;

    let first_result = runtime
        .stream_next_queued_work(TurnOptions::new(
            CancellationToken::new(),
            named_turn_scope("root", "queued-frame-chain"),
        ))
        .await
        .expect("queued frame chain succeeds")
        .ran()
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
    let requests_after_follow = requests.lock_recover().clone();
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
        .ran()
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
    let requests = requests.lock_recover();
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
                frame_key: crate::FrameKey::from_caller_material("recovery-frame")
                    .expect("non-empty caller material"),
                initial_nodes: Vec::new(),
                task: Some("recover this handoff".to_string()),
            }],
        }),
        transport,
        test_host_config(),
        runtime_store,
    )
    .await;
    let inbound = enqueue_idle_turn_input(store.as_ref(), "root", "start switch").await;
    store.fail_next_exact_queue_claim();

    let first = runtime
        .stream_next_queued_work(TurnOptions::new(
            CancellationToken::new(),
            named_turn_scope("root", "handoff-crash-window"),
        ))
        .await
        .expect("the committed frame switch remains a successful public call")
        .ran()
        .expect("the committed frame switch is returned");
    assert!(matches!(
        first.outcome,
        TurnOutcome::AgentFrameSwitch { .. }
    ));
    assert!(first.errors.iter().any(|issue| {
        issue.code.as_deref() == Some("store_commit_failed") && issue.retryable == Some(false)
    }));
    assert!(matches!(
        runtime.resident_session_state,
        ResidentSessionState::Invalidated { .. }
    ));

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
    let expected_frame_id = crate::session_graph::frame_node_id(
        "root",
        crate::FrameKey::from_caller_material("recovery-frame")
            .expect("non-empty caller material")
            .as_str(),
    );
    assert!(matches!(
        &queued[0].items[0].payload,
        crate::QueuedWorkPayload::AgentFrameTask { frame_id, task, .. }
            if frame_id == &expected_frame_id && task == "recover this handoff"
    ));

    let recovered = runtime
        .stream_next_queued_work(TurnOptions::new(
            CancellationToken::new(),
            named_turn_scope("root", "handoff-pump-recovery"),
        ))
        .await
        .expect("pump recovery succeeds")
        .ran()
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
    const SESSION_ID: &str = "mid-chain-cancellation";

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
    let mut runtime = TestRuntime::new(transport)
        .tools(Arc::new(TerminalControlTool {
            controls: vec![crate::ToolControl::SwitchAgentFrame {
                frame_key: crate::FrameKey::from_caller_material("cancelled-frame")
                    .expect("non-empty caller material"),
                initial_nodes: Vec::new(),
                task: Some("cancel before running".to_string()),
            }],
        }))
        .host(test_host_config())
        .store(runtime_store)
        .with_session_id(SESSION_ID)
        .build()
        .await;
    enqueue_idle_turn_input(store.as_ref(), SESSION_ID, "start cancellable switch").await;

    let terminal = runtime
        .stream_next_queued_work(TurnOptions::new(
            cancel,
            named_turn_scope(SESSION_ID, "mid-chain-cancel"),
        ))
        .await
        .expect("cancelled chain assembles")
        .ran()
        .expect("cancelled terminal turn");
    assert!(matches!(
        terminal.outcome,
        TurnOutcome::Stopped(TurnStop::Cancelled { .. })
    ));
    assert!(
        crate::store::QueuedWorkStore::list_queued_work(store.as_ref(), SESSION_ID)
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
        ) -> Result<(), crate::test_support::AttachmentSourcePolicyError> {
            Err(crate::test_support::AttachmentSourcePolicyError {
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
        .ran()
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
                        Ok(vec![crate::TurnPluginDirective::AbortTurn(
                            crate::AbortTurnDirective {
                                code: "blocked".to_string(),
                                message: "plugin stopped claimed turn".to_string(),
                            },
                        )])
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
    let inbound = enqueue_idle_turn_input(store.as_ref(), "root", "abort this input").await;

    let terminal = runtime
        .stream_next_queued_work(TurnOptions::new(
            CancellationToken::new(),
            named_turn_scope("root", "claimed-plugin-abort"),
        ))
        .await
        .expect("plugin abort assembles")
        .ran()
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
        parts: vec![Part::text(
            "prepared-attachment-user.p0".to_string(),
            "store an attachment".to_string(),
            None,
        )]
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
                frame_key: crate::FrameKey::from_caller_material("prepared-follow-frame")
                    .expect("non-empty caller material"),
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
        parts: vec![Part::text(
            "prepared-user.p0".to_string(),
            "prepared input".to_string(),
            None,
        )]
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
async fn turn_finalized_borrowed_append_lane_loss_keeps_typed_issue() {
    let call_index = Arc::new(AtomicUsize::new(0));
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
                            call_id: "finalized-lapsed-switch".to_string(),
                            tool_name: "terminal_tool_0".to_string(),
                            input_json: "{}".to_string(),
                            replay: None,
                        }],
                        response_metadata: Default::default(),
                        ..LlmResponse::default()
                    }),
                    1 => Ok(LlmResponse {
                        parts: vec![LlmOutputPart::Text {
                            text: "final turn still commits".to_string(),
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
    let clock = Arc::new(ManualClock::new(1_000));
    let store_clock: Arc<dyn crate::Clock> = clock.clone();
    let store = Arc::new(RecordingStore::with_clock(store_clock));
    let runtime_store: Arc<dyn crate::store::RuntimePersistence> = store;
    let host_clock: Arc<dyn crate::Clock> = clock.clone();
    let mut config = crate::RuntimeHostConfig::in_memory(
        crate::CommitBudget::bounded(1024 * 1024, 512),
        crate::QueuedWorkBatchingConfig::new(1),
    )
    .with_clock(host_clock);
    config.providers.provider_resolver = Arc::new(crate::SingleProviderResolver::new(
        transport.clone().into_handle(),
    ));
    let mut runtime = runtime_with_plugins_and_tools_and_host_and_store(
        vec![turn_finalized_borrowed_append_plugin()],
        Arc::new(TerminalControlTool {
            controls: vec![crate::ToolControl::SwitchAgentFrame {
                frame_key: crate::FrameKey::from_caller_material("finalized-lapsed-follow-frame")
                    .expect("non-empty caller material"),
                initial_nodes: Vec::new(),
                task: Some("exercise the retained finalize observer".to_string()),
            }],
        }),
        transport,
        crate::EmbeddedRuntimeHost::new(config),
        runtime_store,
    )
    .await;
    runtime.set_turn_phase_probe(Arc::new(ExpireLeaseAtSecondTurnFinalizedHook::new(
        Arc::clone(&clock),
    )));

    let run = runtime
        .stream_turn_with_agent_frames(
            TurnInput::text("start finalized borrowed append probe"),
            TurnOptions::new(
                CancellationToken::new(),
                named_turn_scope("root", "finalized-lapsed-borrow"),
            ),
        )
        .await
        .expect("the final current-head commit survives the observer's borrowed-lane failure");

    assert_eq!(run.turns.len(), 2);
    let issue = run.turns[1]
        .errors
        .iter()
        .find(|issue| {
            issue.code.as_deref()
                == Some(crate::RuntimeErrorCode::SessionExecutionLeaseLost.as_str())
        })
        .unwrap_or_else(|| {
            panic!(
                "TurnFinalized must preserve typed lane loss: {:?}",
                run.turns[1].errors
            )
        });
    assert_eq!(issue.kind, "runtime");
    assert_eq!(issue.retryable, Some(false));
    assert_eq!(call_index.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn retained_turn_graph_service_does_not_extend_the_execution_lane() {
    let transport = TestProvider::builder()
        .kind("mock")
        .requires_streaming(true)
        .complete(|_| async {
            Ok(LlmResponse {
                parts: vec![LlmOutputPart::ToolCall {
                    call_id: "retained-service-switch".to_string(),
                    tool_name: "terminal_tool_0".to_string(),
                    input_json: "{}".to_string(),
                    replay: None,
                }],
                response_metadata: Default::default(),
                ..LlmResponse::default()
            })
        })
        .build();
    let store = Arc::new(RecordingStore::default());
    let runtime_store: Arc<dyn crate::store::RuntimePersistence> = store.clone();
    let retained = Arc::new(std::sync::Mutex::new(None));
    let mut runtime = runtime_with_plugins_and_tools_and_host_and_store(
        vec![retain_turn_persisted_graph_service_plugin(Arc::clone(
            &retained,
        ))],
        Arc::new(TerminalControlTool {
            controls: vec![crate::ToolControl::SwitchAgentFrame {
                frame_key: crate::FrameKey::from_caller_material("retained-service-follow-frame")
                    .expect("non-empty caller material"),
                initial_nodes: Vec::new(),
                task: Some("leave this follow-on queued".to_string()),
            }],
        }),
        transport,
        test_host_config(),
        runtime_store,
    )
    .await;
    enqueue_idle_turn_input(store.as_ref(), "root", "stash the graph service").await;

    let output = runtime
        .stream_next_queued_work(TurnOptions::new(
            CancellationToken::new(),
            named_turn_scope("root", "retained-service"),
        ))
        .await
        .expect("queued switch succeeds")
        .ran()
        .expect("queued switch returns a turn");
    assert!(matches!(
        output.outcome,
        TurnOutcome::AgentFrameSwitch { .. }
    ));

    let graph = retained
        .lock_recover()
        .clone()
        .expect("TurnPersisted retained its graph service");
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            if crate::store::SessionExecutionLeaseStore::get_session_execution_lease(
                store.as_ref(),
                "root",
            )
            .await
            .expect("read released lane")
            .is_none()
            {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("the unique turn-driver guard releases while the service is retained");

    let error = graph
        .append_session_nodes(
            "root",
            crate::AppendSessionNodesRequest {
                operation_id: "stale-retained-service".to_string(),
                nodes: vec![crate::SessionAppendNode::plugin(
                    "test.stale-retained-service",
                    serde_json::json!({"attempted": true}),
                )],
                requires_ancestor_node_id: None,
            },
        )
        .await
        .expect_err("a retained service can only present its stale borrowed fence");
    assert!(matches!(
        error,
        crate::PluginError::SessionExecutionLeaseLost { ref session_id }
            if session_id == "root"
    ));
}

#[tokio::test]
async fn durable_queued_lapsed_lane_stays_loud_at_agent_frame_handoff() {
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
                            call_id: "queued-lapsed-switch".to_string(),
                            tool_name: "terminal_tool_0".to_string(),
                            input_json: "{}".to_string(),
                            replay: None,
                        }],
                        response_metadata: Default::default(),
                        ..LlmResponse::default()
                    }),
                    1 => Ok(LlmResponse {
                        parts: vec![LlmOutputPart::Text {
                            text: "must not silently reacquire".to_string(),
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
    let clock = Arc::new(ManualClock::new(1_000));
    let store_clock: Arc<dyn crate::Clock> = clock.clone();
    let store = Arc::new(RecordingStore::with_clock(store_clock));
    let runtime_store: Arc<dyn crate::store::RuntimePersistence> = store.clone();
    let host_clock: Arc<dyn crate::Clock> = clock.clone();
    let borrowed_append_attempted = Arc::new(AtomicBool::new(false));
    let borrowed_append_error = Arc::new(std::sync::Mutex::new(None));
    let mut config = crate::RuntimeHostConfig::in_memory(
        crate::CommitBudget::bounded(1024 * 1024, 512),
        crate::QueuedWorkBatchingConfig::new(1),
    )
    .with_clock(host_clock);
    config.providers.provider_resolver = Arc::new(crate::SingleProviderResolver::new(
        transport.clone().into_handle(),
    ));
    let mut runtime = runtime_with_plugins_and_tools_and_host_and_store(
        vec![turn_persisted_borrowed_append_plugin(
            Arc::clone(&borrowed_append_attempted),
            Arc::clone(&borrowed_append_error),
        )],
        Arc::new(TerminalControlTool {
            controls: vec![crate::ToolControl::SwitchAgentFrame {
                frame_key: crate::FrameKey::from_caller_material("queued-lapsed-follow-frame")
                    .expect("non-empty caller material"),
                initial_nodes: Vec::new(),
                task: Some("must retain the loud lease failure".to_string()),
            }],
        }),
        transport,
        crate::EmbeddedRuntimeHost::new(config),
        runtime_store,
    )
    .await;
    runtime.set_turn_phase_probe(Arc::new(ExpireLeaseAfterRetainedCommit::new(Arc::clone(
        &clock,
    ))));
    enqueue_idle_turn_input(store.as_ref(), "root", "start lapsed queued handoff").await;

    let output = runtime
        .stream_next_queued_work(TurnOptions::new(
            CancellationToken::new(),
            named_turn_scope("root", "queued-lapsed-handoff"),
        ))
        .await
        .expect("the committed switch is returned with a loud follow-on failure")
        .ran()
        .expect("queued turn should run");

    assert!(matches!(
        output.outcome,
        TurnOutcome::AgentFrameSwitch { .. }
    ));
    let issue = output
        .errors
        .iter()
        .find(|issue| {
            issue.code.as_deref()
                == Some(crate::RuntimeErrorCode::SessionExecutionLeaseLost.as_str())
        })
        .unwrap_or_else(|| {
            panic!(
                "the durable handoff reports the lapsed session lane: {:?}",
                output.errors
            )
        });
    assert_eq!(issue.retryable, Some(false));
    assert_eq!(
        *borrowed_append_error.lock_recover(),
        Some(std::mem::discriminant(
            &crate::PluginError::SessionExecutionLeaseLost {
                session_id: "root".to_string(),
            }
        )),
        "the plugin must receive the typed borrowed-lane failure"
    );
    assert!(
        borrowed_append_attempted.load(Ordering::SeqCst),
        "the lapsed retained lane must be presented by the borrowed nested commit"
    );
    assert_eq!(
        call_index.load(Ordering::SeqCst),
        1,
        "a lapsed retained lane must not be silently reacquired for the follow-on turn"
    );
    let pending = crate::store::QueuedWorkStore::list_queued_work(store.as_ref(), "root")
        .await
        .expect("list committed handoff batch");
    assert!(
        pending
            .iter()
            .any(|batch| batch.items.iter().any(|item| matches!(
                item.payload,
                crate::QueuedWorkPayload::AgentFrameTask { .. }
            ))),
        "the loud claim failure must leave the committed handoff batch claimable"
    );
    let final_lease = crate::store::SessionExecutionLeaseStore::get_session_execution_lease(
        store.as_ref(),
        "root",
    )
    .await
    .expect("read final session lane state");
    assert!(
        final_lease.is_none(),
        "settling the loud durable failure must clear the expired owner row"
    );
}

#[tokio::test]
async fn inprocess_lapsed_lane_stays_loud_after_agent_frame_handoff() {
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
                            call_id: "inprocess-lapsed-switch".to_string(),
                            tool_name: "terminal_tool_0".to_string(),
                            input_json: "{}".to_string(),
                            replay: None,
                        }],
                        response_metadata: Default::default(),
                        ..LlmResponse::default()
                    }),
                    1 => Ok(LlmResponse {
                        parts: vec![LlmOutputPart::Text {
                            text: "must not reach the follow-on".to_string(),
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
    let clock = Arc::new(ManualClock::new(1_000));
    let store_clock: Arc<dyn crate::Clock> = clock.clone();
    let store = Arc::new(RecordingStore::with_clock(store_clock));
    let runtime_store: Arc<dyn crate::store::RuntimePersistence> = store.clone();
    let host_clock: Arc<dyn crate::Clock> = clock.clone();
    let borrowed_append_attempted = Arc::new(AtomicBool::new(false));
    let borrowed_append_error = Arc::new(std::sync::Mutex::new(None));
    let mut config = crate::RuntimeHostConfig::in_memory(
        crate::CommitBudget::bounded(1024 * 1024, 512),
        crate::QueuedWorkBatchingConfig::new(1),
    )
    .with_clock(host_clock);
    config.providers.provider_resolver = Arc::new(crate::SingleProviderResolver::new(
        transport.clone().into_handle(),
    ));
    let mut runtime = runtime_with_plugins_and_tools_and_host_and_store(
        vec![turn_persisted_borrowed_append_plugin(
            Arc::clone(&borrowed_append_attempted),
            Arc::clone(&borrowed_append_error),
        )],
        Arc::new(TerminalControlTool {
            controls: vec![crate::ToolControl::SwitchAgentFrame {
                frame_key: crate::FrameKey::from_caller_material("inprocess-lapsed-follow-frame")
                    .expect("non-empty caller material"),
                initial_nodes: Vec::new(),
                task: Some("must retain the loud lease failure".to_string()),
            }],
        }),
        transport,
        crate::EmbeddedRuntimeHost::new(config),
        runtime_store,
    )
    .await;
    runtime.set_turn_phase_probe(Arc::new(ExpireLeaseAfterRetainedCommit::new(Arc::clone(
        &clock,
    ))));

    let run = runtime
        .stream_turn_with_agent_frames(
            TurnInput::text("start lapsed in-process handoff"),
            TurnOptions::new(
                CancellationToken::new(),
                named_turn_scope("root", "inprocess-lapsed-handoff"),
            ),
        )
        .await
        .expect("the committed switch is returned with a loud follow-on failure");

    assert_eq!(run.turns.len(), 1);
    assert!(matches!(
        run.turns[0].outcome,
        TurnOutcome::AgentFrameSwitch { .. }
    ));
    let issue = run.turns[0]
        .errors
        .iter()
        .find(|issue| {
            issue.code.as_deref()
                == Some(crate::RuntimeErrorCode::SessionExecutionLeaseLost.as_str())
        })
        .unwrap_or_else(|| {
            panic!(
                "the in-process follow-on reports the lapsed session lane: {:?}",
                run.turns[0].errors
            )
        });
    assert_eq!(issue.retryable, Some(false));
    assert_eq!(
        *borrowed_append_error.lock_recover(),
        Some(std::mem::discriminant(
            &crate::PluginError::SessionExecutionLeaseLost {
                session_id: "root".to_string(),
            }
        )),
        "the plugin must receive the typed borrowed-lane failure"
    );
    assert!(
        borrowed_append_attempted.load(Ordering::SeqCst),
        "the lapsed retained lane must be presented by the borrowed nested commit"
    );
    assert_eq!(
        call_index.load(Ordering::SeqCst),
        1,
        "the follow-on provider call must not start under an expired lane"
    );
    let final_lease = crate::store::SessionExecutionLeaseStore::get_session_execution_lease(
        store.as_ref(),
        "root",
    )
    .await
    .expect("read final session lane state");
    assert!(
        final_lease.is_none(),
        "settling the loud in-process failure must clear the expired owner row"
    );
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
                        parts: vec![LlmOutputPart::Text {
                            text: "retained lease follow-on".to_string(),
                            response_meta: None,
                        }],
                        response_metadata: Default::default(),
                        ..LlmResponse::default()
                    }),
                    2 => Ok(LlmResponse {
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
                frame_key: crate::FrameKey::from_caller_material("resident-follow-frame")
                    .expect("non-empty caller material"),
                initial_nodes: Vec::new(),
                task: Some("continue on retained lease".to_string()),
            }],
        }),
        transport,
        test_host_config(),
        runtime_store,
    )
    .await;

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
    // ADR 0069: one acceptance admitted this run, and it admitted exactly the
    // physical turn it was accepted for. The follow-on frames of the same run
    // were never separately admitted, so they must not restate the identity.
    assert!(
        run.turns[0].turn_input_acceptance.is_some(),
        "the admitted turn carries the acceptance it was admitted under"
    );
    assert!(
        run.turns[1].turn_input_acceptance.is_none(),
        "a follow-on frame of the same run was not separately admitted"
    );
    assert_eq!(
        store.load_session_count(),
        0,
        "the initial head probe and retained-lease follow-on must not hydrate an unchanged graph"
    );
    assert_eq!(
        store.load_session_head_meta_count(),
        1,
        "the first physical turn must establish durable head freshness exactly once"
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
        0,
        "reacquiring an unchanged durable head must not hydrate its graph"
    );
    assert_eq!(
        store.load_session_head_meta_count(),
        2,
        "a released and reacquired lease generation must force a durable head recheck"
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
    let mut config = crate::RuntimeHostConfig::in_memory(
        crate::CommitBudget::bounded(1024 * 1024, 512),
        crate::QueuedWorkBatchingConfig::new(1),
    )
    .with_clock(host_clock);
    config.providers.provider_resolver = Arc::new(crate::SingleProviderResolver::new(
        transport.clone().into_handle(),
    ));
    let mut runtime = runtime_with_plugins_and_tools_and_host_and_store(
        Vec::new(),
        Arc::new(TerminalControlTool {
            controls: vec![crate::ToolControl::SwitchAgentFrame {
                frame_key: crate::FrameKey::from_caller_material("lost-lease-follow-frame")
                    .expect("non-empty caller material"),
                initial_nodes: Vec::new(),
                task: Some("continue after retained commit".to_string()),
            }],
        }),
        transport,
        crate::EmbeddedRuntimeHost::new(config),
        runtime_store,
    )
    .await;
    runtime.set_turn_phase_probe(Arc::new(ExpireLeaseAfterRetainedCommit::new(Arc::clone(
        &clock,
    ))));

    let frame_run = runtime
        .stream_turn_with_agent_frames(
            TurnInput::text("lose the retained lease"),
            TurnOptions::new(
                CancellationToken::new(),
                named_turn_scope("root", "lost-retained-lease"),
            ),
        )
        .await
        .expect("the committed frame must survive the follow-on lease loss");
    assert_eq!(frame_run.turns.len(), 1);
    assert!(matches!(
        frame_run.turns[0].outcome,
        TurnOutcome::AgentFrameSwitch { .. }
    ));
    let issue = frame_run.turns[0]
        .errors
        .iter()
        .find(|issue| {
            issue.code.as_deref()
                == Some(crate::RuntimeErrorCode::SessionExecutionLeaseLost.as_str())
        })
        .expect("the committed frame reports the follow-on lease loss");
    assert_eq!(issue.retryable, Some(false));
    assert_eq!(
        store.load_session_count(),
        0,
        "the fenced handoff claim must reject the lost lease before a full reload"
    );
    assert_eq!(
        store.load_session_head_meta_count(),
        1,
        "the first turn must establish durable head freshness exactly once"
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
        1,
        "a lease acquired after loss must reload invalidated resident state exactly once"
    );
    assert_eq!(
        store.load_session_head_meta_count(),
        2,
        "the rebuilt turn must recheck the durable head without a second graph hydration"
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
            frame_key: crate::FrameKey::from_caller_material(&format!("bounded-frame-{index}"))
                .expect("non-empty caller material"),
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
    let inbound = enqueue_idle_turn_input(store.as_ref(), "root", "start bounded chain").await;

    let terminal = runtime
        .stream_next_queued_work(TurnOptions::new(
            CancellationToken::new(),
            named_turn_scope("root", "bounded-frame-chain"),
        ))
        .await
        .expect("bounded chain terminalizes")
        .ran()
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
async fn frame_switch_limit_capture_abort_abandons_prompt_claim_before_returning_diagnostic() {
    let switch_count = crate::runtime::logical_turn::MAX_AGENT_FRAME_SWITCHES;
    let executor = Arc::new(FailingCaptureExecutor {
        dirty: AtomicBool::new(false),
        fail_capture: AtomicBool::new(false),
        snapshot: std::sync::Mutex::new(Vec::new()),
        restored: std::sync::Mutex::new(Vec::new()),
    });
    let protocol: Arc<dyn crate::plugin::ProtocolSessionPlugin> =
        Arc::new(RestoreExecutorFromRuntimeState {
            executor: Arc::clone(&executor),
        });
    let code_executor: Arc<dyn crate::plugin::CodeExecutorPlugin> = executor.clone();
    let protocol_factory = crate::testing::test_standard_protocol_factory_with_runtime_state(
        protocol,
        Some(code_executor),
    );
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
            frame_key: crate::FrameKey::from_caller_material(&format!(
                "capture-abort-frame-{index}"
            ))
            .expect("non-empty caller material"),
            initial_nodes: Vec::new(),
            task: Some(format!("continue capture-abort chain {index}")),
        })
        .collect();
    let store = Arc::new(RecordingStore::default());
    let mut runtime = runtime_with_plugins_and_tools_and_host_and_store(
        vec![protocol_factory],
        Arc::new(TerminalControlTool { controls }),
        transport,
        test_host_config(),
        store.clone() as Arc<dyn crate::RuntimePersistence>,
    )
    .await;
    runtime.set_turn_phase_probe(Arc::new(FailCaptureAfterCommittedTurns {
        executor,
        committed_turns: AtomicUsize::new(0),
        fail_after: switch_count,
    }));
    enqueue_idle_turn_input(store.as_ref(), "root", "start capture-abort chain").await;

    let committed = runtime
        .stream_next_queued_work(TurnOptions::new(
            CancellationToken::new(),
            named_turn_scope("root", "bounded-frame-capture-abort"),
        ))
        .await
        .expect("a failed terminal capture preserves the last committed frame")
        .ran()
        .expect("the last committed frame is returned");

    assert!(matches!(
        committed.outcome,
        TurnOutcome::AgentFrameSwitch { .. }
    ));
    assert!(committed.errors.iter().any(|issue| {
        issue.code.as_deref() == Some("execution_state_capture_failed")
            && issue.retryable == Some(false)
    }));
    assert_eq!(
        store.abandoned_claim_counts(),
        (1, 0),
        "the claimed handoff must pass through ordinary local-abort cleanup"
    );
    let queued = store.raw_queued_work_for_testing();
    assert_eq!(queued.len(), 1, "only the uncommitted handoff remains");
    assert!(
        queued[0].1.is_none() && !queued[0].3,
        "the remaining handoff must have no claim identity or token: {queued:?}"
    );
}

#[tokio::test]
async fn leading_session_command_drains_before_queued_turn() {
    let transport = mock_provider(vec![MockCall {
        stream_events: Vec::new(),
        response: Ok(LlmResponse {
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
        .ran()
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
async fn idle_ordering_read_is_independent_of_pending_command_depth() {
    for backlog_depth in [1, 256] {
        let transport = mock_provider(vec![MockCall {
            stream_events: Vec::new(),
            response: Ok(LlmResponse {
                parts: vec![LlmOutputPart::Text {
                    text: format!("answer after {backlog_depth} commands"),
                    response_meta: None,
                }],
                response_metadata: Default::default(),
                ..LlmResponse::default()
            }),
        }]);
        let clock = Arc::new(ManualClock::new(1_500));
        let store_clock: Arc<dyn crate::Clock> = clock.clone();
        let (mut runtime, store) =
            standard_runtime_with_transport_and_queue_store_clock(transport, store_clock).await;
        for index in 0..backlog_depth {
            enqueue_session_command(
                store.as_ref(),
                "root",
                &format!("depth-invariance command {index}"),
            )
            .await;
        }
        clock.advance_ms(1);
        enqueue_idle_turn_input(store.as_ref(), "root", "user turn after commands").await;

        let drained = runtime
            .stream_next_queued_work(TurnOptions::new(
                CancellationToken::new(),
                named_turn_scope("root", &format!("depth-invariance-{backlog_depth}")),
            ))
            .await
            .expect("depth-invariance drain succeeds")
            .ran()
            .expect("queued turn runs after commands");

        assert_eq!(
            drained.assistant_output.safe_text,
            format!("answer after {backlog_depth} commands")
        );
        assert_eq!(
            store.list_pending_queued_work_count(),
            0,
            "idle ordering must not invoke the payload-hydrating full-list read at depth {backlog_depth}"
        );
    }
}

#[tokio::test]
async fn later_session_command_does_not_jump_earlier_queued_turn() {
    let transport = mock_provider(vec![MockCall {
        stream_events: Vec::new(),
        response: Ok(LlmResponse {
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
        .ran()
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
        .expect("later command drain succeeds")
        .ran();
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
                captured_requests.lock_recover().push(req);
                Ok(LlmResponse {
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
        .process_registry()
        .cloned()
        .expect("process registry");
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
                crate::RecoveryContract::ExternallyOwned,
                crate::ProcessProvenance::session(target_scope.clone())
                    .with_caused_by(Some(process_caused_by.clone())),
            )
            .with_extra_event_types([process_wake_event_type()])
            .with_wake_session_id(Some(target_scope.session_id.clone())),
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
        ),
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
        .ran()
        .expect("queued turn");

    let events = turn_events.snapshot();
    let crate::TurnEvent::TurnStarted { turn_id } = &events
        .first()
        .expect("queued turn emitted no activity")
        .event
    else {
        panic!("queued turn must begin with TurnStarted");
    };
    assert_eq!(turn_id, "queued-work-started-turn");
    let queued_started = events
        .iter()
        .position(|activity| matches!(&activity.event, crate::TurnEvent::QueuedWorkStarted { .. }))
        .expect("queued work started event");
    assert_eq!(queued_started, 1, "claim facts must follow turn identity");
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
        let guard = requests.lock_recover();
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

#[derive(Clone)]
struct CountingEchoTool {
    executions: Arc<AtomicUsize>,
}

#[derive(Clone)]
struct CancellationGatedTurnEvents {
    events: RecordingTurnEvents,
    cancellation: CancellationToken,
    entered: Arc<Mutex<Option<tokio::sync::oneshot::Sender<()>>>>,
}

impl CancellationGatedTurnEvents {
    fn new(cancellation: CancellationToken) -> (Self, tokio::sync::oneshot::Receiver<()>) {
        let (entered_tx, entered_rx) = tokio::sync::oneshot::channel();
        (
            Self {
                events: RecordingTurnEvents::default(),
                cancellation,
                entered: Arc::new(Mutex::new(Some(entered_tx))),
            },
            entered_rx,
        )
    }

    fn snapshot(&self) -> Vec<TurnActivity> {
        self.events.snapshot()
    }
}

#[async_trait::async_trait]
impl crate::TurnActivitySink for CancellationGatedTurnEvents {
    async fn emit(&self, activity: TurnActivity) {
        if matches!(
            &activity.event,
            TurnEvent::AssistantProseDelta { text }
                if text.as_ref() == "drained before effect abort"
        ) {
            if let Some(entered) = self.entered.lock_recover().take() {
                let _ = entered.send(());
            }
            self.cancellation.cancelled().await;
        }
        self.events.events.lock_recover().push(activity);
    }
}

#[async_trait::async_trait]
impl crate::ToolProvider for CountingEchoTool {
    fn tool_manifests(&self) -> Vec<crate::ToolManifest> {
        EchoTool.tool_manifests()
    }

    fn resolve_contract(&self, name: &str) -> Option<Arc<crate::ToolContract>> {
        EchoTool.resolve_contract(name)
    }

    async fn execute(&self, call: crate::ToolCall<'_>) -> crate::ToolOutcome {
        self.executions.fetch_add(1, Ordering::SeqCst);
        EchoTool.execute(call).await
    }
}

#[tokio::test]
async fn cancellation_watch_exhaustion_tears_down_committed_cancel_and_settles_turn() {
    let controller = Arc::new(
        super::effect::RecordingEffectController::default().with_always_failing_cancel_watch(),
    );
    let controller_for_provider = Arc::clone(&controller);
    let provider_calls = Arc::new(AtomicUsize::new(0));
    let observed_provider_calls = Arc::clone(&provider_calls);
    let tool_executions = Arc::new(AtomicUsize::new(0));
    let (provider_started_tx, provider_started_rx) = tokio::sync::oneshot::channel::<()>();
    let provider_started_tx = Arc::new(Mutex::new(Some(provider_started_tx)));
    let transport =
        TestProvider::builder()
            .kind("mock")
            .requires_streaming(true)
            .complete(move |request| {
                let controller = Arc::clone(&controller_for_provider);
                let observed_provider_calls = Arc::clone(&observed_provider_calls);
                let provider_started_tx = Arc::clone(&provider_started_tx);
                async move {
                    let call = observed_provider_calls.fetch_add(1, Ordering::SeqCst);
                    match call {
                        0 => {
                            request.stream_events.expect("stream events").send(
                                LlmStreamEvent::Delta("drained before effect abort".to_string()),
                            );
                            if let Some(started) = provider_started_tx.lock_recover().take() {
                                let _ = started.send(());
                            }
                            controller.wait_for_cancel_watch_exhaustion().await;
                            for _ in 0..32 {
                                tokio::task::yield_now().await;
                            }
                            Ok(LlmResponse {
                                parts: vec![LlmOutputPart::ToolCall {
                                    call_id: "post-exhaustion-tool".to_string(),
                                    tool_name: "echo_tool".to_string(),
                                    input_json: serde_json::json!({"value": "zombie"}).to_string(),
                                    replay: None,
                                }],
                                response_metadata: Default::default(),
                                ..LlmResponse::default()
                            })
                        }
                        1 => Ok(LlmResponse {
                            parts: vec![LlmOutputPart::Text {
                                text: "zombie turn completed".to_string(),
                                response_meta: None,
                            }],
                            response_metadata: Default::default(),
                            ..LlmResponse::default()
                        }),
                        _ => panic!("unexpected provider call {call}"),
                    }
                }
            })
            .build();
    let clock = Arc::new(CancelWatchTestClock(crate::testing::TestClock::new(0)));
    let host_clock: Arc<dyn crate::Clock> = clock.clone();
    let config = super::effect::runtime_host_config_with_native_controller(controller.clone())
        .with_clock(host_clock);
    let turn_driver = crate::TurnWorkDriver::new(Arc::clone(&config.control.effect_host));
    let host = EmbeddedRuntimeHost::new(config);
    let mut runtime = runtime_with_plugins_and_tools_and_host(
        Vec::new(),
        Arc::new(CountingEchoTool {
            executions: Arc::clone(&tool_executions),
        }),
        transport,
        host,
    )
    .await;
    let turn_id = "bounded-cancel-watch";
    let turn_address = crate::TurnAddress::new("root", turn_id);
    let turn_cancel = CancellationToken::new();
    let observed_turn_cancel = turn_cancel.clone();
    let (turn_events, stream_event_entered_rx) =
        CancellationGatedTurnEvents::new(turn_cancel.clone());
    let turn_events_for_task = turn_events.clone();
    let turn = crate::task::spawn(async move {
        runtime
            .stream_turn(
                TurnInput::text("tear down after the cancellation watcher gives up"),
                TurnOptions::new(turn_cancel, named_turn_scope("root", turn_id))
                    .with_turn_events(&turn_events_for_task),
            )
            .await
    });

    provider_started_rx
        .await
        .expect("provider must start before cancellation is committed");
    stream_event_entered_rx
        .await
        .expect("the buffered stream event must reach the gated sink");
    let receipt = turn_driver
        .request_cancel(crate::TurnCancelRequest::new(
            turn_address.clone(),
            "watch-exhaustion-cancel",
            Some("test-user".to_string()),
        ))
        .await
        .expect("commit cancellation receipt");
    assert!(matches!(
        receipt.outcome,
        crate::TurnCancelOutcome::Requested(ref evidence)
            if evidence.request_id == "watch-exhaustion-cancel"
    ));
    controller.release_cancel_watch_failures();

    let turn = tokio::time::timeout(std::time::Duration::from_secs(5), turn)
        .await
        .expect("watch exhaustion must tear down and settle the turn")
        .expect("turn task")
        .expect("committed cancellation remains a successful turn terminal");
    assert_eq!(
        controller.cancel_watch_attempts(),
        crate::runtime::turn_loop::TURN_CANCEL_WATCH_MAX_ATTEMPTS
    );
    assert_eq!(
        provider_calls.load(Ordering::SeqCst),
        1,
        "watch exhaustion must abort the in-flight provider call before another can start"
    );
    assert_eq!(
        tool_executions.load(Ordering::SeqCst),
        0,
        "provider output produced after watcher exhaustion must never reach an executor"
    );
    assert!(
        observed_turn_cancel.is_cancelled(),
        "watch exhaustion must cancel the active turn token after {} attempts",
        controller.cancel_watch_attempts()
    );
    assert!(matches!(
        turn.outcome,
        TurnOutcome::Stopped(TurnStop::Cancelled { ref evidence })
            if evidence.request_id == "watch-exhaustion-cancel"
    ));
    assert!(
        turn_events.snapshot().iter().any(|activity| matches!(
            &activity.event,
            TurnEvent::AssistantProseDelta { text }
                if text.as_ref() == "drained before effect abort"
        )),
        "cooperative teardown must drain the buffered stream event; provider_calls={}, tool_executions={}",
        provider_calls.load(Ordering::SeqCst),
        tool_executions.load(Ordering::SeqCst)
    );

    let terminal = tokio::time::timeout(
        std::time::Duration::from_secs(1),
        turn_driver.await_terminal(&turn_address),
    )
    .await
    .expect("the cancelled turn must settle its terminal")
    .expect("read settled turn terminal");
    assert!(matches!(
        terminal,
        crate::TurnTerminal::Committed {
            outcome: TurnOutcome::Stopped(TurnStop::Cancelled { ref evidence }),
            ..
        } if evidence.request_id == "watch-exhaustion-cancel"
    ));
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
                    if let Some(tx) = delta_sent_tx.lock_recover().take() {
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
        TurnOutcome::Stopped(TurnStop::Cancelled { .. })
    ));
    assert!(
        assembled.errors.is_empty(),
        "requested cancellation must not become an llm_provider TurnIssue: {:?}",
        assembled.errors
    );
    assert!(assembled.assistant_output.safe_text.is_empty());
    assert!(assembled.assistant_output.raw_text.is_empty());
    assert!(
        turn_events
            .snapshot()
            .iter()
            .all(|activity| !matches!(&activity.event, TurnEvent::Error { message } if message == "LLM error: cancelled")),
        "requested cancellation must not emit a user-visible LLM error"
    );
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
async fn parent_end_failure_after_effect_loop_cancellation_returns_session_for_next_turn() {
    let calls = Arc::new(AtomicUsize::new(0));
    let observed_calls = Arc::clone(&calls);
    let (second_call_started_tx, second_call_started_rx) = tokio::sync::oneshot::channel::<()>();
    let second_call_started_tx = Arc::new(Mutex::new(Some(second_call_started_tx)));
    let transport = TestProvider::builder()
        .kind("mock")
        .requires_streaming(true)
        .complete(move |_| {
            let call = observed_calls.fetch_add(1, Ordering::SeqCst);
            let second_call_started_tx = Arc::clone(&second_call_started_tx);
            async move {
                match call {
                    0 => Ok(LlmResponse {
                        parts: vec![LlmOutputPart::ToolCall {
                            call_id: "parent-end-failure-call".to_string(),
                            tool_name: "parent_end_failure_intent".to_string(),
                            input_json: "{}".to_string(),
                            replay: None,
                        }],
                        response_metadata: Default::default(),
                        ..LlmResponse::default()
                    }),
                    1 => {
                        if let Some(tx) = second_call_started_tx.lock_recover().take() {
                            let _ = tx.send(());
                        }
                        std::future::pending::<Result<LlmResponse, LlmTransportError>>().await
                    }
                    2 => Ok(LlmResponse {
                        parts: vec![LlmOutputPart::Text {
                            text: "second turn started".to_string(),
                            response_meta: None,
                        }],
                        response_metadata: Default::default(),
                        ..LlmResponse::default()
                    }),
                    _ => panic!("unexpected provider call {call}"),
                }
            }
        })
        .build();
    let mut runtime =
        runtime_with_plugins_and_tools(Vec::new(), Arc::new(ParentEndFailureIntentTool), transport)
            .await;
    let effect_controller = Arc::new(
        super::effect::RecordingEffectController::default()
            .with_local_llm_execution()
            .with_next_tool_parent_end_failure(),
    );
    let cancel = CancellationToken::new();
    let cancel_after_second_call_starts = cancel.clone();
    let canceller = crate::task::spawn(async move {
        second_call_started_rx
            .await
            .expect("the first turn should enter its second provider call");
        cancel_after_second_call_starts.cancel();
    });

    let first_error = runtime
        .run_turn_assembled(
            TurnInput::text("start the parent-end failure witness"),
            cancel,
            crate::ScopedEffectController::shared(
                effect_controller.clone(),
                crate::ExecutionScope::turn("root", "parent-end-failure-first-turn"),
            )
            .expect("first turn scope"),
        )
        .await
        .expect_err("the forced parent-end failure should fail the cancelled turn");
    canceller.await.expect("canceller task");
    assert_eq!(
        first_error.code,
        crate::RuntimeErrorCode::PluginSessionManager
    );
    assert!(first_error.message.contains("forced parent-end failure"));

    let second_turn = runtime
        .run_turn_assembled(
            TurnInput::text("prove the runtime can start another turn"),
            CancellationToken::new(),
            crate::ScopedEffectController::shared(
                effect_controller,
                crate::ExecutionScope::turn("root", "parent-end-failure-second-turn"),
            )
            .expect("second turn scope"),
        )
        .await
        .expect("the second turn should start on the same runtime");

    assert_eq!(
        second_turn.assistant_output.safe_text,
        "second turn started"
    );
    assert_eq!(calls.load(Ordering::SeqCst), 3);
}

#[tokio::test]
async fn truncated_retry_resets_partial_tool_calls_and_retains_failed_attempt_usage() {
    let attempts = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let transport = TestProvider::builder()
        .kind("openai-compatible")
        .requires_streaming(true)
        .generation_retry_guarantee(crate::provider::GenerationRetryGuarantee::Idempotent)
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
                            .with_retry_verdict(
                                crate::llm::transport::TransportRetryVerdict::RetryableTransient,
                            )
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

#[tokio::test]
async fn counted_provider_regeneration_emits_one_host_visible_attempt_reset() {
    let provider_calls = Arc::new(AtomicUsize::new(0));
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
            let provider_calls = Arc::clone(&provider_calls);
            move |request| {
                let call = provider_calls.fetch_add(1, Ordering::SeqCst);
                async move {
                    if call == 0 {
                        return Err(LlmTransportError::new("connection failed before response")
                            .with_kind(crate::ProviderFailureKind::Transport)
                            .with_retry_verdict(
                                crate::llm::transport::TransportRetryVerdict::RetryableTransient,
                            ));
                    }

                    request
                        .stream_events
                        .expect("stream events")
                        .send(LlmStreamEvent::Delta("success".to_string()));
                    Ok(LlmResponse {
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
    let turn_events = RecordingTurnEvents::default();

    let assembled = runtime
        .stream_turn(
            TurnInput::text("retry a pre-response transport failure"),
            TurnOptions::new(
                CancellationToken::new(),
                named_turn_scope("root", "counted-regeneration-reset"),
            )
            .with_turn_events(&turn_events),
        )
        .await
        .expect("counted retry succeeds");

    assert_eq!(provider_calls.load(Ordering::SeqCst), 2);
    assert_eq!(assembled.llm_calls[0].attempts.len(), 2);
    assert_eq!(
        assembled.llm_calls[0]
            .attempts
            .iter()
            .filter(|attempt| attempt.retry_budget_consumed)
            .count(),
        2
    );
    let turn_events = turn_events.snapshot();
    let resets = turn_events
        .iter()
        .filter_map(|activity| match &activity.event {
            TurnEvent::ModelAttemptReset {
                assistant_prose_correlation_ids,
                reasoning_correlation_ids,
            } => Some((assistant_prose_correlation_ids, reasoning_correlation_ids)),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(resets.len(), 1);
    assert_eq!(resets[0].0, &Vec::<crate::runtime::TurnActivityId>::new());
    assert_eq!(resets[0].1, &Vec::<crate::runtime::TurnActivityId>::new());
}

#[tokio::test(start_paused = true)]
async fn courtesy_retry_after_regeneration_emits_one_host_visible_attempt_reset() {
    let provider_calls = Arc::new(AtomicUsize::new(0));
    let transport = TestProvider::builder()
        .kind("openai-compatible")
        .requires_streaming(true)
        .options(crate::ProviderOptions {
            reliability: crate::provider::ProviderReliability::default()
                .max_attempts(1)
                .base_delay_ms(0)
                .max_delay_ms(0),
            ..crate::ProviderOptions::default()
        })
        .complete({
            let provider_calls = Arc::clone(&provider_calls);
            move |request| {
                let call = provider_calls.fetch_add(1, Ordering::SeqCst);
                async move {
                    if call == 0 {
                        return Err(LlmTransportError::new("provider requested a retry delay")
                            .with_status(429)
                            .with_retry_verdict(
                                crate::llm::transport::TransportRetryVerdict::RetryableThrottle {
                                    retry_after: Some(std::time::Duration::from_secs(1)),
                                },
                            ));
                    }

                    request
                        .stream_events
                        .expect("stream events")
                        .send(LlmStreamEvent::Delta("success".to_string()));
                    Ok(LlmResponse {
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
    let turn_events = RecordingTurnEvents::default();

    let assembled = runtime
        .stream_turn(
            TurnInput::text("defer to a provider retry-after"),
            TurnOptions::new(
                CancellationToken::new(),
                named_turn_scope("root", "courtesy-regeneration-reset"),
            )
            .with_turn_events(&turn_events),
        )
        .await
        .expect("courtesy retry succeeds");

    assert_eq!(provider_calls.load(Ordering::SeqCst), 2);
    assert_eq!(assembled.llm_calls[0].attempts.len(), 2);
    assert_eq!(
        assembled.llm_calls[0]
            .attempts
            .iter()
            .filter(|attempt| attempt.retry_budget_consumed)
            .count(),
        1
    );
    assert_eq!(
        turn_events
            .snapshot()
            .iter()
            .filter(|activity| { matches!(activity.event, TurnEvent::ModelAttemptReset { .. }) })
            .count(),
        1
    );
}

#[tokio::test]
async fn retryable_mid_stream_failure_preserves_durable_charge_safety_evidence() {
    let provider_calls = Arc::new(AtomicUsize::new(0));
    let requests = Arc::new(std::sync::Mutex::new(Vec::new()));
    let lost_text = std::iter::repeat_n("discarded", 256)
        .collect::<Vec<_>>()
        .join(" ");
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
            let provider_calls = Arc::clone(&provider_calls);
            let requests = Arc::clone(&requests);
            let lost_text = lost_text.clone();
            move |request| {
                let call = provider_calls.fetch_add(1, Ordering::SeqCst);
                requests.lock_recover().push(request.messages.clone());
                let lost_text = lost_text.clone();
                async move {
                    let stream = request.stream_events.expect("stream events");
                    if call == 0 {
                        stream.send(LlmStreamEvent::Delta(lost_text.clone()));
                        let usage = LlmUsage {
                            input_tokens: 32,
                            output_tokens: 256,
                            ..LlmUsage::default()
                        };
                        stream.send(LlmStreamEvent::Usage(usage.clone()));
                        return Err(LlmTransportError::new(
                            "stream ended before terminal evidence",
                        )
                        .with_kind(crate::ProviderFailureKind::Stream)
                        .with_code("stream_ended_before_terminal_response")
                        .with_retry_verdict(
                            crate::llm::transport::TransportRetryVerdict::RetryableTransient,
                        )
                        .with_partial_response(LlmResponse {
                            parts: vec![LlmOutputPart::Text {
                                text: lost_text,
                                response_meta: None,
                            }],
                            usage,
                            provider_usage: Some(serde_json::json!({
                                "prompt_tokens": 32,
                                "completion_tokens": 256
                            })),
                            response_metadata: Default::default(),
                            ..LlmResponse::default()
                        }));
                    }

                    stream.send(LlmStreamEvent::Delta("replacement".to_string()));
                    Ok(LlmResponse {
                        parts: vec![LlmOutputPart::Text {
                            text: "replacement".to_string(),
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
    let store = Arc::new(crate::InMemorySessionStore::new());
    let runtime_store: Arc<dyn crate::RuntimePersistence> = store.clone();
    let mut runtime = TestRuntime::new(transport)
        .store(runtime_store)
        .without_process_registry()
        .build()
        .await;
    let turn_events = RecordingTurnEvents::default();

    let assembled = runtime
        .stream_turn(
            TurnInput::text("retry after paid output"),
            TurnOptions::new(
                CancellationToken::new(),
                named_turn_scope("root", "paid-output-retry"),
            )
            .with_turn_events(&turn_events),
        )
        .await
        .expect("provider failure is returned as an assembled turn");

    assert_eq!(provider_calls.load(Ordering::SeqCst), 1);
    assert!(matches!(
        assembled.outcome,
        TurnOutcome::Stopped(TurnStop::ProviderError)
    ));
    assert!(assembled.assistant_output.safe_text.is_empty());
    assert!(assembled.assistant_output.raw_text.is_empty());
    let activities = turn_events.snapshot();
    assert!(activities.iter().any(|activity| matches!(
        &activity.event,
        TurnEvent::AssistantProseDelta { text } if text.as_ref() == lost_text
    )));
    assert!(
        activities
            .iter()
            .all(|activity| !matches!(activity.event, TurnEvent::ModelAttemptReset { .. }))
    );
    assert!(
        active_conversation_messages(&assembled.state)
            .iter()
            .flat_map(|message| message.parts.iter())
            .all(|part| !part.content.contains("discarded")),
        "a failed partial response remains preview output, not committed history"
    );
    let calls = &assembled.llm_calls;
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].attempts.len(), 1);
    let preserved_attempt = &calls[0].attempts[0];
    assert_eq!(
        preserved_attempt.protocol_position,
        crate::ProtocolPosition::OutputStarted
    );
    assert_eq!(
        preserved_attempt
            .usage
            .as_ref()
            .map(|usage| usage.output_tokens),
        Some(256)
    );
    assert_eq!(
        preserved_attempt
            .retry_decision
            .as_ref()
            .map(|decision| decision.scheduled),
        Some(false)
    );
    assert_eq!(
        preserved_attempt
            .retry_decision
            .as_ref()
            .and_then(|decision| decision.reason.as_deref()),
        Some("output_started_without_retry_guarantee")
    );
    let issue = assembled.errors.first().expect("typed provider issue");
    assert_eq!(
        issue.code.as_deref(),
        Some("unsafe_retry_after_output_started")
    );
    assert_eq!(issue.retryable, Some(false));
    assert!(
        issue.message.contains("already paid for")
            && issue.message.contains("cannot be safely regenerated")
    );
    assert_eq!(assembled.failure_evidence.len(), 1);
    assert!(
        assembled.failure_evidence.len()
            <= assembled
                .llm_calls
                .iter()
                .map(|call| call.attempts.len())
                .sum::<usize>(),
        "durable failure evidence is cardinality-bounded by sealed provider attempts"
    );
    let failure = &assembled.failure_evidence[0];
    assert_eq!(
        failure.partial_output.as_ref().map(|output| output.text()),
        Some(lost_text.as_str())
    );
    assert_eq!(failure.billed_usage.output_tokens, 256);
    assert_eq!(
        failure.refusal.denial_reason,
        crate::ChargeSafetyDenialReason::GuaranteeRequired
    );
    assert_eq!(
        failure.refusal.protocol_position,
        crate::ProtocolPosition::OutputStarted
    );
    assert!(
        serde_json::to_value(failure)
            .expect("serialize durable failure evidence")
            .get("refusal")
            .and_then(|refusal| refusal.get("retry_guarantee"))
            .is_none(),
        "durable refusal evidence must not persist an inferred constant"
    );
    assert_eq!(
        (
            failure.refusal.attempt_number,
            failure.refusal.attempt_count
        ),
        (1, 1)
    );

    runtime
        .stream_turn(
            TurnInput::text("follow up after the failed generation"),
            TurnOptions::new(
                CancellationToken::new(),
                named_turn_scope("root", "paid-output-follow-up"),
            ),
        )
        .await
        .expect("a later turn can continue without replaying failure evidence");
    {
        let requests = requests.lock_recover();
        assert_eq!(requests.len(), 2);
        assert!(
            !serde_json::to_string(&requests[1])
                .expect("serialize the follow-up provider request")
                .contains("discarded"),
            "the next provider prompt has no constructional path from turn settlement evidence"
        );
    }

    drop(runtime);
    let reopened = crate::store::load_persisted_session_read_view(store.as_ref())
        .await
        .expect("reopen the failed turn's session")
        .expect("failed turn left a durable session");
    assert_eq!(
        reopened.turn_failure_settlements().len(),
        1,
        "mid-stream failure evidence must survive runtime teardown and reopen"
    );
    assert_eq!(
        reopened.turn_failure_settlements()[0].evidence,
        assembled.failure_evidence
    );
    assert!(
        reopened
            .messages()
            .iter()
            .flat_map(|message| message.parts.iter())
            .all(|part| !part.content.contains("discarded")),
        "durable failure evidence remains outside model context"
    );
}

// Boundary: execution-lease tests stay in `turns.rs` because they exercise live
// `LashRuntime` lease acquisition, public scheduling, turn phase probes, and
// provider suspension. Runtime Scenarios own persistence-level head-CAS and
// queue/input claim invariants; these tests own the facade scheduler response.
/// A foreground turn is refused while a foreign executor holds the session
/// lane, before provider work or durable input acceptance (ADR 0077).
#[tokio::test]
async fn foreground_turn_is_refused_when_session_lane_is_held() {
    let transport = mock_provider(vec![MockCall {
        stream_events: Vec::new(),
        response: Ok(LlmResponse {
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
        "foreground-turn-is-refused-when-session-lane-is-held-executor",
        60_000,
    )
    .await
    .expect("claim session execution lease")
    .acquired()
    .expect("session execution lease");

    let error = runtime
        .run_turn_assembled(
            TurnInput::text("foreground must wait"),
            CancellationToken::new(),
            named_turn_scope("root", "foreground-busy-lane-turn"),
        )
        .await
        .expect_err("a foreign lease holder refuses the foreground turn");

    assert_eq!(
        error.code,
        crate::RuntimeErrorCode::SessionExecutionLaneBusy
    );
    assert!(
        crate::TurnInputStore::list_pending_turn_inputs(store.as_ref(), "root")
            .await
            .expect("read pending turn inputs after refusal")
            .is_empty(),
        "lease refusal must precede durable input acceptance"
    );
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
        "idle-queued-work-noops-without-claiming-when-session-lane-is-held-executor",
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
        .expect("busy queued drain should not error")
        .ran();

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
        .ran()
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
async fn durable_controller_waits_for_busy_session_lane_before_draining_queued_input() {
    let clock = Arc::new(ManualClock::new(1_000));
    let store_clock: Arc<dyn crate::Clock> = clock.clone();
    let (mut runtime, store) = standard_runtime_with_transport_and_queue_store_clock(
        mock_provider(Vec::new()),
        store_clock,
    )
    .await;
    runtime.host.core.clock = clock.clone();
    enqueue_idle_turn_input(store.as_ref(), "root", "queued during failover").await;
    let held_lease = crate::store::SessionExecutionLeaseStore::try_claim_session_execution_lease(
        store.as_ref(),
        "root",
        &lease_owner("crashed-worker"),
        "durable-controller-waits-for-busy-session-lane-before-draining-queued-input-executor",
        50,
    )
    .await
    .expect("claim crashed worker session execution lease")
    .acquired()
    .expect("crashed worker holds session execution lease");
    assert_eq!(held_lease.expires_at_epoch_ms, 1_050);

    let controller = Arc::new(
        super::effect::RecordingEffectController::default()
            .with_controller_owned_replay()
            .with_engine_paced_lane(),
    );
    let scope = crate::ScopedEffectController::shared(
        controller,
        crate::ExecutionScope::turn("root", "queued-failover-wake"),
    )
    .expect("durable queued-turn scope");
    let mut drain = crate::task::spawn(async move {
        runtime
            .stream_next_queued_work(TurnOptions::new(CancellationToken::new(), scope))
            .await
            .map(crate::facade_support::QueuedTurnDrain::ran)
    });

    assert!(
        tokio::time::timeout(std::time::Duration::from_millis(5), &mut drain)
            .await
            .is_err(),
        "durable queued drain must remain pending while the foreign lease is live"
    );
    clock.advance_ms(51);
    let drained = drain
        .await
        .expect("join durable queued drain")
        .expect("durable queued drain succeeds")
        .expect("durable queued drain consumes the pending input");
    assert_eq!(drained.assistant_output.safe_text, "finished");
    assert!(
        crate::store::TurnInputStore::list_pending_turn_inputs(store.as_ref(), "root")
            .await
            .expect("list pending input after durable drain")
            .is_empty(),
        "durable queued drain must settle the literal pending input"
    );
}

/// The give-up half of the same policy: a holder that keeps renewing is alive,
/// so no amount of in-process waiting can free the lane inside this invocation.
/// The drain reports the typed retryable error naming the live holder, and
/// leaves both the holder row and the queued row exactly as it found them.
#[tokio::test]
async fn durable_controller_reports_a_retryable_busy_lane_when_the_holder_is_alive() {
    let clock = Arc::new(ManualClock::new(1_000));
    let store_clock: Arc<dyn crate::Clock> = clock.clone();
    let (mut runtime, store) = standard_runtime_with_transport_and_queue_store_clock(
        mock_provider(Vec::new()),
        store_clock,
    )
    .await;
    runtime.host.core.clock = clock.clone();
    enqueue_idle_turn_input(store.as_ref(), "root", "queued behind a live holder").await;
    let held_lease = crate::store::SessionExecutionLeaseStore::try_claim_session_execution_lease(
        store.as_ref(),
        "root",
        &lease_owner("live-worker"),
        "live-holder-executor",
        100,
    )
    .await
    .expect("claim live worker session execution lease")
    .acquired()
    .expect("live worker holds session execution lease");
    assert_eq!(held_lease.expires_at_epoch_ms, 1_100);

    let controller = Arc::new(
        super::effect::RecordingEffectController::default()
            .with_controller_owned_replay()
            .with_engine_paced_lane(),
    );
    let scope = crate::ScopedEffectController::shared(
        controller,
        crate::ExecutionScope::turn("root", "queued-live-holder"),
    )
    .expect("durable queued-turn scope");
    let mut drain = crate::task::spawn(async move {
        runtime
            .stream_next_queued_work(TurnOptions::new(CancellationToken::new(), scope))
            .await
            .map(crate::facade_support::QueuedTurnDrain::ran)
    });
    assert!(
        tokio::time::timeout(std::time::Duration::from_millis(5), &mut drain)
            .await
            .is_err(),
        "the drain must still be waiting when the holder renews"
    );

    clock.advance_ms(10);
    let renewed = crate::store::SessionExecutionLeaseStore::renew_session_execution_lease(
        store.as_ref(),
        &held_lease.fence(),
        100,
    )
    .await
    .expect("live worker renews its session execution lease");
    assert_eq!(renewed.expires_at_epoch_ms, 1_110);

    let error = drain
        .await
        .expect("join durable queued drain")
        .expect_err("a live holder must end the wait with a typed error");
    assert_eq!(
        error.code,
        crate::RuntimeErrorCode::SessionExecutionLaneBusy
    );
    assert!(error.is_retryable());
    assert!(!error.is_terminal());
    assert_eq!(
        error.message,
        "session execution lane for session `root` is held by owner `live-worker` \
         incarnation `live-worker:incarnation` executor `live-holder-executor` \
         (fencing generation 1, expires at 1110); stopped waiting after 25ms \
         because the holder renewed its lease"
    );

    let holder_after = crate::store::SessionExecutionLeaseStore::get_session_execution_lease(
        store.as_ref(),
        "root",
    )
    .await
    .expect("read the holder row after the drain gave up")
    .expect("the live holder still holds the lane");
    assert_eq!(holder_after, renewed);
    assert_eq!(
        crate::store::TurnInputStore::list_pending_turn_inputs(store.as_ref(), "root")
            .await
            .expect("list pending input after the drain gave up")
            .len(),
        1,
        "a drain that gave up must leave the queued row pending"
    );
}

/// Cancellation cannot report an empty queue while a durable queued row is
/// still pending. It returns the same typed retryable lane signal so teardown
/// and redrive leave settlement to the engine.
#[tokio::test]
async fn cancelling_a_durable_busy_lane_wait_keeps_the_queued_row_pending() {
    let clock = Arc::new(ManualClock::new(1_000));
    let store_clock: Arc<dyn crate::Clock> = clock.clone();
    let (mut runtime, store) = standard_runtime_with_transport_and_queue_store_clock(
        mock_provider(Vec::new()),
        store_clock,
    )
    .await;
    runtime.host.core.clock = clock;
    enqueue_idle_turn_input(store.as_ref(), "root", "queued during cancellation").await;
    let held_lease = crate::store::SessionExecutionLeaseStore::try_claim_session_execution_lease(
        store.as_ref(),
        "root",
        &lease_owner("cancelled-wait-holder"),
        "cancelled-wait-holder-executor",
        100,
    )
    .await
    .expect("claim cancellation test holder lease")
    .acquired()
    .expect("cancellation test holder owns the lane");

    let controller = Arc::new(
        super::effect::RecordingEffectController::default()
            .with_controller_owned_replay()
            .with_engine_paced_lane(),
    );
    let scope = crate::ScopedEffectController::shared(
        controller,
        crate::ExecutionScope::turn("root", "queued-cancelled-wait"),
    )
    .expect("durable queued cancellation scope");
    let cancel = CancellationToken::new();
    let drain_cancel = cancel.clone();
    let drain = crate::task::spawn(async move {
        runtime
            .stream_next_queued_work(TurnOptions::new(drain_cancel, scope))
            .await
            .map(crate::facade_support::QueuedTurnDrain::ran)
    });
    tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    cancel.cancel();

    let error = drain
        .await
        .expect("join cancelled durable queued drain")
        .expect_err("cancellation while waiting must remain retryable");
    assert_eq!(
        error.code,
        crate::RuntimeErrorCode::SessionExecutionLaneBusy
    );
    assert_eq!(
        error.message,
        "session execution lane for session `root` is held by owner `cancelled-wait-holder` \
         incarnation `cancelled-wait-holder:incarnation` executor \
         `cancelled-wait-holder-executor` (fencing generation 1, expires at 1100); \
         stopped waiting after 25ms because the queued drain was cancelled while waiting"
    );
    assert!(error.is_retryable());
    assert!(!error.is_terminal());
    assert_eq!(
        crate::store::SessionExecutionLeaseStore::get_session_execution_lease(
            store.as_ref(),
            "root",
        )
        .await
        .expect("read holder after cancellation")
        .expect("holder remains installed"),
        held_lease
    );
    assert_eq!(
        crate::store::TurnInputStore::list_pending_turn_inputs(store.as_ref(), "root")
            .await
            .expect("list pending input after cancellation")
            .len(),
        1
    );
}

/// The backstop: a holder whose expiry never moves and never lapses (a frozen
/// clock) must not become an unbounded block. Waiting stops at twice the
/// observed TTL with the same typed retryable error.
#[tokio::test]
async fn durable_controller_stops_waiting_for_a_busy_lane_at_the_wait_budget() {
    let clock = Arc::new(ManualClock::new(1_000));
    let store_clock: Arc<dyn crate::Clock> = clock.clone();
    let (mut runtime, store) = standard_runtime_with_transport_and_queue_store_clock(
        mock_provider(Vec::new()),
        store_clock,
    )
    .await;
    runtime.host.core.clock = clock.clone();
    enqueue_idle_turn_input(store.as_ref(), "root", "queued behind a frozen holder").await;
    let held_lease = crate::store::SessionExecutionLeaseStore::try_claim_session_execution_lease(
        store.as_ref(),
        "root",
        &lease_owner("frozen-worker"),
        "frozen-holder-executor",
        100,
    )
    .await
    .expect("claim frozen worker session execution lease")
    .acquired()
    .expect("frozen worker holds session execution lease");

    let controller = Arc::new(
        super::effect::RecordingEffectController::default()
            .with_controller_owned_replay()
            .with_engine_paced_lane(),
    );
    let scope = crate::ScopedEffectController::shared(
        controller,
        crate::ExecutionScope::turn("root", "queued-frozen-holder"),
    )
    .expect("durable queued-turn scope");
    let error = runtime
        .stream_next_queued_work(TurnOptions::new(CancellationToken::new(), scope))
        .await
        .expect_err("the wait budget must end the drain with a typed error");
    assert_eq!(
        error.code,
        crate::RuntimeErrorCode::SessionExecutionLaneBusy
    );
    assert!(error.is_retryable());
    assert_eq!(
        error.message,
        "session execution lane for session `root` is held by owner `frozen-worker` \
         incarnation `frozen-worker:incarnation` executor `frozen-holder-executor` \
         (fencing generation 1, expires at 1100); stopped waiting after 200ms \
         because the in-process wait budget elapsed"
    );

    let holder_after = crate::store::SessionExecutionLeaseStore::get_session_execution_lease(
        store.as_ref(),
        "root",
    )
    .await
    .expect("read the holder row after the wait budget elapsed")
    .expect("the frozen holder still holds the lane");
    assert_eq!(holder_after, held_lease);
    assert_eq!(
        crate::store::TurnInputStore::list_pending_turn_inputs(store.as_ref(), "root")
            .await
            .expect("list pending input after the wait budget elapsed")
            .len(),
        1,
        "a drain that hit the wait budget must leave the queued row pending"
    );
}

/// The capability gate, not effect-replay ownership, is what selects the busy
/// wait. A controller that owns effect replay but is not a durable workflow
/// controller - every store-backed durable effect host - keeps the ordinary
/// one-shot `Busy -> None` drain contract.
#[tokio::test]
async fn controller_owned_replay_alone_keeps_the_one_shot_busy_drain_contract() {
    let (mut runtime, store) =
        standard_runtime_with_transport_and_queue_store(mock_provider(Vec::new())).await;
    enqueue_idle_turn_input(store.as_ref(), "root", "queued behind a replay-owning host").await;
    let held_lease = crate::store::SessionExecutionLeaseStore::try_claim_session_execution_lease(
        store.as_ref(),
        "root",
        &lease_owner("foreground-runtime"),
        "controller-owned-replay-alone-executor",
        60_000,
    )
    .await
    .expect("claim session execution lease")
    .acquired()
    .expect("session execution lease");

    let controller = Arc::new(
        super::effect::RecordingEffectController::default().with_controller_owned_replay(),
    );
    let scope = crate::ScopedEffectController::shared(
        controller,
        crate::ExecutionScope::turn("root", "queued-replay-owner"),
    )
    .expect("controller-owned replay queued-turn scope");
    let busy_result = runtime
        .stream_next_queued_work(TurnOptions::new(CancellationToken::new(), scope))
        .await
        .expect("a replay-owning non-workflow controller must not error on Busy")
        .ran();

    assert!(
        busy_result.is_none(),
        "controller-owned effect replay alone must keep the one-shot Busy no-op"
    );
    assert_eq!(
        crate::store::TurnInputStore::list_pending_turn_inputs(store.as_ref(), "root")
            .await
            .expect("queued turn input after the one-shot no-op")
            .len(),
        1
    );
    let holder_after = crate::store::SessionExecutionLeaseStore::get_session_execution_lease(
        store.as_ref(),
        "root",
    )
    .await
    .expect("read the holder row after the one-shot no-op")
    .expect("the holder still holds the lane");
    assert_eq!(holder_after.lease_token, held_lease.lease_token);
    assert_eq!(holder_after.fencing_token, held_lease.fencing_token);
}

#[tokio::test]
async fn session_command_waits_in_durable_queue_until_session_lease_ttl_expires() {
    let clock = Arc::new(ManualClock::new(1_000));
    let store_clock: Arc<dyn crate::Clock> = clock.clone();
    let (mut runtime, store) = standard_runtime_with_transport_and_queue_store_clock(
        mock_provider(Vec::new()),
        store_clock,
    )
    .await;
    let command = enqueue_session_command(store.as_ref(), "root", "wait for stale lease").await;
    let owner = lease_owner("stale-session-command-owner");
    crate::store::SessionExecutionLeaseStore::try_claim_session_execution_lease(
        store.as_ref(),
        "root",
        &owner,
        "session-command-waits-in-durable-queue-until-session-lease-ttl-expires-executor",
        50,
    )
    .await
    .expect("claim stale session execution lease")
    .acquired()
    .expect("session execution lease");

    let busy_result = runtime
        .stream_next_queued_work(TurnOptions::new(
            CancellationToken::new(),
            named_turn_scope("root", "command-before-lease-ttl"),
        ))
        .await
        .expect("busy command drain should not error")
        .ran();

    assert!(busy_result.is_none());
    assert_eq!(
        crate::store::QueuedWorkStore::list_queued_work(store.as_ref(), "root")
            .await
            .expect("list command while lease is live")
            .iter()
            .map(|batch| batch.batch_id.as_str())
            .collect::<Vec<_>>(),
        vec![command.batch_id.as_str()],
        "the command must remain durable while another owner holds the live lease"
    );

    clock.advance_ms(51);
    let after_ttl = runtime
        .stream_next_queued_work(TurnOptions::new(
            CancellationToken::new(),
            named_turn_scope("root", "command-after-lease-ttl"),
        ))
        .await
        .expect("command drain after TTL should succeed")
        .ran();

    assert!(after_ttl.is_none(), "a command-only drain returns no turn");
    assert!(
        crate::store::QueuedWorkStore::list_queued_work(store.as_ref(), "root")
            .await
            .expect("list command after TTL drain")
            .is_empty(),
        "the durable command should drain after the stale lease expires"
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
        "session-command-claim-lease-expiry-surfaces-session-execution-lease-lost-executor",
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

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_real_turn_commits_record_product_admission_waits() {
    let session_id = "root";
    let _ =
        crate::runtime::commit_admission::take_product_commit_admission_observations(session_id);
    let clock = Arc::new(ManualClock::new(1_000));
    let store_clock: Arc<dyn crate::Clock> = clock.clone();
    let store = Arc::new(RecordingStore::with_clock(store_clock));
    let build_runtime = |answer: &'static str| {
        let transport = mock_provider(vec![MockCall {
            stream_events: Vec::new(),
            response: Ok(LlmResponse {
                parts: vec![LlmOutputPart::Text {
                    text: answer.to_string(),
                    response_meta: None,
                }],
                response_metadata: Default::default(),
                ..LlmResponse::default()
            }),
        }]);
        let runtime_store: Arc<dyn crate::RuntimePersistence> = store.clone();
        let host_clock: Arc<dyn crate::Clock> = clock.clone();
        async move {
            runtime_with_plugins_and_tools_and_host_and_store(
                Vec::new(),
                Arc::new(EmptyTools),
                transport,
                crate::EmbeddedRuntimeHost::new(
                    crate::RuntimeHostConfig::in_memory(
                        crate::CommitBudget::bounded(1024 * 1024, 512),
                        crate::QueuedWorkBatchingConfig::new(1),
                    )
                    .with_clock(host_clock),
                ),
                runtime_store,
            )
            .await
        }
    };
    let mut first_runtime = build_runtime("first committed turn").await;
    let mut second_runtime = build_runtime("second stale turn").await;
    let entered = Arc::new(AtomicBool::new(false));
    let release = Arc::new(AtomicBool::new(false));
    let probe = Arc::new(PauseFirstProductCommitAttempt {
        entered: Arc::clone(&entered),
        release: Arc::clone(&release),
        attempts: AtomicUsize::new(0),
    });
    first_runtime.set_turn_phase_probe(probe.clone());
    second_runtime.set_turn_phase_probe(probe);

    let first = crate::task::spawn(async move {
        first_runtime
            .run_turn_assembled(
                TurnInput::text("first concurrent commit"),
                CancellationToken::new(),
                named_turn_scope(session_id, "product-admission-first"),
            )
            .await
    });
    tokio::time::timeout(std::time::Duration::from_secs(2), async {
        while !entered.load(Ordering::SeqCst) {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("first real turn entered the admitted product commit attempt");

    // Let a second same-process runtime take over the advisory execution lease
    // while the first remains current at the store head. This reaches two real
    // final-commit attempts without weakening the store CAS authority.
    clock.advance_ms(crate::LeaseTimings::default().ttl_ms() + 1);
    let second = crate::task::spawn(async move {
        second_runtime
            .run_turn_assembled(
                TurnInput::text("second concurrent commit"),
                CancellationToken::new(),
                named_turn_scope(session_id, "product-admission-second"),
            )
            .await
    });
    tokio::time::timeout(std::time::Duration::from_secs(2), async {
        while crate::runtime::commit_admission::process_commit_admission_queue_depth(session_id)
            == 0
        {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("second real turn queued behind product commit admission");
    release.store(true, Ordering::SeqCst);

    let first_result = first.await.expect("first product turn task");
    let second_result = second.await.expect("second product turn task");
    assert!(
        first_result.is_ok() || second_result.is_ok(),
        "one admitted real turn must advance the store head: first={first_result:?}, second={second_result:?}"
    );
    assert!(
        first_result.is_err() || second_result.is_err(),
        "the stale/superseded real turn must still be refused by durable authority"
    );

    let observations =
        crate::runtime::commit_admission::take_product_commit_admission_observations(session_id);
    assert!(
        observations.iter().any(|observation| {
            observation.path == "turn_final_commit"
                && observation.work_identity == "product-admission-second"
                && observation.queue_depth > 0
                && !observation.waited.is_zero()
        }),
        "real runtime turn commits must record a nonzero product admission wait: {observations:?}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn committed_intent_survives_takeover_and_head_cas_loss_in_the_same_runtime_turn() {
    let clock = Arc::new(ManualClock::new(1_000));
    let store_clock: Arc<dyn crate::Clock> = clock.clone();
    let store = Arc::new(RecordingStore::with_clock(store_clock));
    let runtime_store: Arc<dyn crate::RuntimePersistence> = store.clone();
    let registry = Arc::new(crate::TestLocalProcessRegistry::default());
    registry
        .register_process_with_observers(
            crate::ProcessRegistration::new(
                "cas-survivor-intent-target",
                crate::ProcessInput::External {
                    metadata: serde_json::Value::Null,
                },
                crate::RecoveryContract::ExternallyOwned,
                crate::ProcessProvenance::host(),
            )
            .with_extra_event_types([crate::ProcessEventType {
                name: "intent.survivor.committed".to_string(),
                payload_schema: crate::LashSchema::any(),
                semantics: crate::ProcessEventSemanticsSpec::default(),
            }]),
            &["root".to_string()],
        )
        .await
        .expect("register same-turn CAS survivor target");
    let tool_calls = Arc::new(AtomicUsize::new(0));
    let tools: Arc<dyn crate::ToolProvider> = Arc::new(CasSurvivorIntentTools {
        calls: Arc::clone(&tool_calls),
    });
    let model_calls = Arc::new(AtomicUsize::new(0));
    let transport = TestProvider::builder()
        .kind("mock")
        .complete({
            let model_calls = Arc::clone(&model_calls);
            move |_| {
                let model_calls = Arc::clone(&model_calls);
                async move {
                    Ok(match model_calls.fetch_add(1, Ordering::SeqCst) {
                        0 => LlmResponse {
                            parts: vec![LlmOutputPart::ToolCall {
                                call_id: "cas-survivor-call".to_string(),
                                tool_name: "cas_survivor_intent".to_string(),
                                input_json: "{}".to_string(),
                                replay: None,
                            }],
                            response_metadata: Default::default(),
                            ..LlmResponse::default()
                        },
                        1 => LlmResponse {
                            parts: vec![LlmOutputPart::Text {
                                text: "stale conversational tail".to_string(),
                                response_meta: None,
                            }],
                            response_metadata: Default::default(),
                            ..LlmResponse::default()
                        },
                        index => panic!("unexpected CAS survivor model call {index}"),
                    })
                }
            }
        })
        .build();
    let host_clock: Arc<dyn crate::Clock> = clock.clone();
    let config = crate::RuntimeHostConfig::in_memory(
        crate::CommitBudget::bounded(1024 * 1024, 512),
        crate::QueuedWorkBatchingConfig::new(1),
    )
    .with_clock(host_clock);
    let mut runtime = TestRuntime::new(transport)
        .plugins(Vec::new())
        .tools(tools)
        .host(crate::EmbeddedRuntimeHost::new(config))
        .store(runtime_store)
        .process_registry(registry.clone())
        .build()
        .await;
    let effect_loop_ended = Arc::new(AtomicBool::new(false));
    let release_effect_loop = Arc::new(AtomicBool::new(false));
    runtime.set_turn_phase_probe(Arc::new(PauseAfterEffectLoop {
        entered: Arc::clone(&effect_loop_ended),
        release: Arc::clone(&release_effect_loop),
    }));
    let first = crate::task::spawn(async move {
        runtime
            .run_turn_assembled(
                TurnInput::text("emit evidence before losing CAS"),
                CancellationToken::new(),
                named_turn_scope("root", "cas-survivor-stale-turn"),
            )
            .await
    });
    tokio::time::timeout(std::time::Duration::from_secs(2), async {
        while !effect_loop_ended.load(Ordering::SeqCst) {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("the intent-owning runtime turn reaches the pre-CAS boundary");
    assert_eq!(tool_calls.load(Ordering::SeqCst), 1);
    assert_eq!(
        registry
            .events_after("cas-survivor-intent-target", 0)
            .await
            .expect("read committed pre-CAS intent")
            .iter()
            .filter(|event| event.event_type == "intent.survivor.committed")
            .count(),
        1,
        "the same runtime turn executes the intent before its head CAS"
    );

    clock.advance_ms(crate::LeaseTimings::default().ttl_ms() + 1);
    let successor_transport = mock_provider(vec![MockCall {
        stream_events: Vec::new(),
        response: Ok(LlmResponse {
            parts: vec![LlmOutputPart::Text {
                text: "successor wins".to_string(),
                response_meta: None,
            }],
            response_metadata: Default::default(),
            ..LlmResponse::default()
        }),
    }]);
    let successor_store: Arc<dyn crate::RuntimePersistence> = store.clone();
    let successor_clock: Arc<dyn crate::Clock> = clock.clone();
    let successor_config = crate::RuntimeHostConfig::in_memory(
        crate::CommitBudget::bounded(1024 * 1024, 512),
        crate::QueuedWorkBatchingConfig::new(1),
    )
    .with_clock(successor_clock);
    let mut successor = TestRuntime::new(successor_transport)
        .plugins(Vec::new())
        .host(crate::EmbeddedRuntimeHost::new(successor_config))
        .store(successor_store)
        .process_registry(registry.clone())
        .build()
        .await;
    successor
        .run_turn_assembled(
            TurnInput::text("take over and win the head"),
            CancellationToken::new(),
            named_turn_scope("root", "cas-survivor-successor-turn"),
        )
        .await
        .expect("successor wins the shared store head CAS");
    release_effect_loop.store(true, Ordering::SeqCst);
    let error = first
        .await
        .expect("stale runtime task joins")
        .expect_err("the intent-owning stale conversational tail loses head CAS");
    assert_eq!(error.code, crate::RuntimeErrorCode::StoreCommitFailed);
    assert!(
        error.message.contains("head revision conflict"),
        "the same-turn loser must retain typed CAS diagnostics: {error:?}"
    );
    assert_eq!(
        registry
            .events_after("cas-survivor-intent-target", 0)
            .await
            .expect("read intent after CAS loss")
            .iter()
            .filter(|event| event.event_type == "intent.survivor.committed")
            .count(),
        1,
        "the intent survives the enclosing turn's failing CAS without duplication"
    );
}

// Regression (FIG-862): a foreground turn that has not observed takeover may
// still publish its current-head tail afterward.
// ManualClock advances store time only, while its sleep uses real Tokio time, so
// the default 10s renewal never fires in this millisecond-scale test. The
// observed-loss path is covered by
// `renewal_failure_mid_turn_does_not_select_a_durable_branch`.
#[tokio::test]
async fn unobserved_lease_loss_does_not_stop_foreground_turn_before_final_commit() {
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
                    if let Some(tx) = provider_started_tx.lock_recover().take() {
                        let _ = tx.send(());
                    }
                    let rx = provider_continue_rx
                        .lock_recover()
                        .take()
                        .expect("provider continue receiver available");
                    let _ = rx.await;
                    Ok(LlmResponse {
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
    let mut config = crate::RuntimeHostConfig::in_memory(
        crate::CommitBudget::bounded(1024 * 1024, 512),
        crate::QueuedWorkBatchingConfig::new(1),
    )
    .with_clock(host_clock);
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

    clock.advance_ms(crate::LeaseTimings::default().ttl_ms() + 1);
    let successor_transport = mock_provider(vec![MockCall {
        stream_events: Vec::new(),
        response: Ok(LlmResponse {
            parts: vec![LlmOutputPart::Text {
                text: "successor continued from landed tail".to_string(),
                response_meta: None,
            }],
            response_metadata: Default::default(),
            ..LlmResponse::default()
        }),
    }]);
    let successor_store: Arc<dyn crate::store::RuntimePersistence> = store.clone();
    let successor_host_clock: Arc<dyn crate::Clock> = clock.clone();
    let successor_config = crate::RuntimeHostConfig::in_memory(
        crate::CommitBudget::bounded(1024 * 1024, 512),
        crate::QueuedWorkBatchingConfig::new(1),
    )
    .with_clock(successor_host_clock);
    let mut successor_runtime = runtime_with_plugins_and_tools_and_host_and_store(
        Vec::new(),
        Arc::new(EmptyTools),
        successor_transport,
        crate::EmbeddedRuntimeHost::new(successor_config),
        successor_store,
    )
    .await;
    let successor_owner = successor_runtime.runtime_lease_owner.clone();
    let stolen = crate::store::SessionExecutionLeaseStore::try_claim_session_execution_lease(
        store.as_ref(),
        "root",
        &successor_owner,
        &successor_runtime.runtime_lease_executor_id,
        60_000,
    )
    .await
    .expect("steal expired session execution lease")
    .acquired()
    .expect("expired session execution lease should be claimable");
    let commits_before_lease_loss = *store.runtime_commit_count.lock_recover();
    provider_continue_tx
        .send(())
        .expect("provider should still be waiting");

    let assembled = turn
        .await
        .expect("foreground turn task")
        .expect("unobserved lease loss must not reject the turn");
    assert_eq!(
        assembled.assistant_output.safe_text,
        "committed under head CAS"
    );
    assert!(
        *store.runtime_commit_count.lock_recover() > commits_before_lease_loss,
        "the current-head turn must checkpoint and commit despite advisory lease loss"
    );
    let still_owned =
        crate::store::SessionExecutionLeaseStore::try_claim_session_execution_lease_with_token(
            store.as_ref(),
            "root",
            &successor_owner,
            &stolen.executor_id,
            &crate::LeaseClaimNonce::for_testing("successor-reentry-token"),
            60_000,
        )
        .await
        .expect("reclaim successor lease with the same owner")
        .acquired()
        .expect("the predecessor commit must leave the successor lease live");
    assert_eq!(
        still_owned.fencing_token, stolen.fencing_token,
        "the predecessor's final commit must not release the successor lease"
    );

    let successor_turn = successor_runtime
        .run_turn_assembled(
            TurnInput::text("continue after predecessor tail"),
            CancellationToken::new(),
            named_turn_scope("root", "successor-after-landed-tail"),
        )
        .await
        .expect("the successor should continue from the newly committed head");
    assert_eq!(
        active_conversation_messages(&successor_turn.state)
            .iter()
            .flat_map(|message| message.parts.iter())
            .filter(|part| part.content == "committed under head CAS")
            .count(),
        1,
        "the successor must reload exactly one predecessor tail that landed after takeover"
    );
    assert_eq!(
        successor_turn.assistant_output.safe_text,
        "successor continued from landed tail"
    );
    assert!(
        successor_turn.state.turn_index > assembled.state.turn_index,
        "the successor must advance from the predecessor's landed head"
    );
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
                        parts: vec![LlmOutputPart::Text {
                            text: "reach the active-turn checkpoint".to_string(),
                            response_meta: None,
                        }],
                        response_metadata: Default::default(),
                        ..LlmResponse::default()
                    });
                }
                if let Some(tx) = captured_provider_stalled_tx.lock_recover().take() {
                    let _ = tx.send(());
                }
                let rx = captured_provider_continue_rx
                    .lock_recover()
                    .take()
                    .expect("provider continue receiver available");
                let _ = rx.await;
                Ok(LlmResponse {
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
    let mut config = crate::RuntimeHostConfig::in_memory(
        crate::CommitBudget::bounded(1024 * 1024, 512),
        crate::QueuedWorkBatchingConfig::new(1),
    )
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

    enqueue_idle_turn_input(store.as_ref(), "root", "input held when the lease is lost").await;
    let registry = runtime
        .host
        .process_registry()
        .cloned()
        .expect("process registry");
    let target_scope = crate::SessionScope::new("root");
    registry
        .register_process(
            crate::ProcessRegistration::new(
                "lease-loss-claimed-wake",
                crate::ProcessInput::External {
                    metadata: serde_json::Value::Null,
                },
                crate::RecoveryContract::ExternallyOwned,
                crate::ProcessProvenance::session(target_scope.clone()),
            )
            .with_extra_event_types([process_wake_event_type()])
            .with_wake_session_id(Some(target_scope.session_id.clone())),
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
        ),
    )
    .await;

    let turn = crate::task::spawn(async move {
        runtime
            .stream_next_queued_work(TurnOptions::new(
                CancellationToken::new(),
                named_turn_scope("root", "renewal-failure-mid-turn"),
            ))
            .await
            .map(crate::facade_support::QueuedTurnDrain::ran)
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
            "renewal-failure-mid-turn-does-not-select-a-durable-branch-executor",
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
    const SESSION_ID: &str = "cancellation-sealed-renewal";

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
                if let Some(tx) = captured_provider_started_tx.lock_recover().take() {
                    let _ = tx.send(());
                }
                std::future::pending::<Result<LlmResponse, _>>().await
            }
        })
        .build();
    let mut config = crate::RuntimeHostConfig::in_memory(
        crate::CommitBudget::bounded(1024 * 1024, 512),
        crate::QueuedWorkBatchingConfig::new(1),
    )
    .with_lease_timings(crate::LeaseTimings::from_ttl(lease_ttl).expect("valid timings"));
    config.providers.provider_resolver = Arc::new(crate::SingleProviderResolver::new(
        transport.clone().into_handle(),
    ));
    let turn_driver = crate::TurnWorkDriver::new(Arc::clone(&config.control.effect_host));
    let mut runtime = TestRuntime::new(transport)
        .tools(Arc::new(EmptyTools))
        .host(crate::EmbeddedRuntimeHost::new(config))
        .store(runtime_store)
        .with_session_id(SESSION_ID)
        .build()
        .await;
    let effect_loop_ended = Arc::new(AtomicBool::new(false));
    let release_effect_loop = Arc::new(AtomicBool::new(false));
    runtime.set_turn_phase_probe(Arc::new(PauseAfterEffectLoop {
        entered: Arc::clone(&effect_loop_ended),
        release: Arc::clone(&release_effect_loop),
    }));

    let turn_id = "cancel-before-renewal-failure";
    let persisted_state = runtime.export_persistence_state();
    let turn_scope = native_scope(persisted_state.turn_scope(turn_id));
    let turn_address = crate::TurnAddress::new(&persisted_state.session_id, turn_id);
    let turn = crate::task::spawn(async move {
        runtime
            .run_turn_assembled(
                TurnInput::text("cancel before the lease renewal fails"),
                CancellationToken::new(),
                turn_scope,
            )
            .await
    });
    provider_started_rx
        .await
        .expect("provider should start after lease acquisition");
    let undelivered = crate::store::TurnInputStore::enqueue_pending_turn_input(
        store.as_ref(),
        crate::PendingTurnInputDraft::new(
            &persisted_state.session_id,
            crate::TurnInputIngress::active_turn(
                turn_id,
                crate::TurnInputCheckpointBoundary::AfterWork,
            ),
            crate::TurnInput::text("unsent steer restored by host"),
        ),
    )
    .await
    .expect("enqueue an undelivered active-turn input");
    let receipt = turn_driver
        .request_cancel(
            crate::TurnCancelRequest::new(
                turn_address,
                "cancel-before-loss-request",
                Some("test-user".to_string()),
            )
            .with_reason("user stopped the turn")
            .undelivered(crate::TurnCancelDisposition::Drop),
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
    assert_eq!(
        assembled.outcome,
        TurnOutcome::Stopped(TurnStop::Cancelled {
            evidence: crate::TurnCancellationEvidence {
                request_id: "cancel-before-loss-request".to_string(),
                origin: Some("test-user".to_string()),
                reason: Some("user stopped the turn".to_string()),
                undelivered: crate::TurnCancelDisposition::Drop,
            }
        })
    );
    assert_eq!(assembled.turn_cancel_input_outcome.affected_inputs.len(), 1);
    assert_eq!(
        assembled.turn_cancel_input_outcome.affected_inputs[0].input_id,
        undelivered.input_id
    );
    assert_eq!(
        assembled.turn_cancel_input_outcome.affected_inputs[0].disposition,
        crate::TurnCancelDisposition::Drop
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
            parts: vec![LlmOutputPart::Text {
                text: "committed after lease expiry".to_string(),
                response_meta: None,
            }],
            response_metadata: Default::default(),
            ..LlmResponse::default()
        }),
    }]);
    let host_clock: Arc<dyn crate::Clock> = clock.clone();
    let mut config = crate::RuntimeHostConfig::in_memory(
        crate::CommitBudget::bounded(1024 * 1024, 512),
        crate::QueuedWorkBatchingConfig::new(1),
    )
    .with_clock(host_clock);
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
    runtime.set_turn_phase_probe(Arc::new(ExpireLeaseAtPreparedTurn::new(Arc::clone(&clock))));

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
            parts: vec![LlmOutputPart::Text {
                text: "provider reached".to_string(),
                response_meta: None,
            }],
            response_metadata: Default::default(),
            ..LlmResponse::default()
        }),
    }]);
    let host_clock: Arc<dyn crate::Clock> = clock.clone();
    let mut config = crate::RuntimeHostConfig::in_memory(
        crate::CommitBudget::bounded(1024 * 1024, 512),
        crate::QueuedWorkBatchingConfig::new(1),
    )
    .with_clock(host_clock);
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
        .process_registry()
        .cloned()
        .expect("process registry");
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
                crate::RecoveryContract::ExternallyOwned,
                crate::ProcessProvenance::session(target_scope.clone())
                    .with_caused_by(Some(process_caused_by.clone())),
            )
            .with_extra_event_types([process_wake_event_type()])
            .with_wake_session_id(Some(target_scope.session_id.clone())),
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
        ),
    )
    .await;
    let expected_wake_id = wake.wake_id.clone();
    let expected_sequence = wake.sequence;
    let expected_text = format!(
        "Background process wake\nProcess: wake-proc\nEvent: process.wake #{expected_sequence}\nWake input:\ndeploy complete"
    );

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
                    sequence,
                    wake_id,
                    caused_by,
                } if process_id == "wake-proc"
                    && event_type == "process.wake"
                    && *sequence == expected_sequence
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
            && sequence == expected_sequence
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

/// FIG-1313 red-side anchor (a): the small-window wedge.
///
/// A 1,000-token model with roughly 900 tokens of retained history used to
/// refuse every selected queued drain outright — the complete projected
/// request (prompt + history + wake + action reserve) could not fit, so the
/// queue could never drain even one short wake, while ordinary turns on the
/// same fixture kept succeeding. Drain size is host policy now, and the shipped
/// one-at-a-time default leaves the provider as the authority on fit.
#[tokio::test]
async fn a_selected_queued_wake_drains_under_a_small_window_with_retained_history() {
    let provider_calls = Arc::new(AtomicUsize::new(0));
    let captured_provider_calls = Arc::clone(&provider_calls);
    let transport = TestProvider::builder()
        .kind("mock")
        .requires_streaming(true)
        .complete(move |_| {
            let provider_calls = Arc::clone(&captured_provider_calls);
            async move {
                provider_calls.fetch_add(1, Ordering::SeqCst);
                Ok(LlmResponse {
                    parts: vec![LlmOutputPart::Text {
                        text: "retained answer".to_string(),
                        response_meta: None,
                    }],
                    response_metadata: Default::default(),
                    ..LlmResponse::default()
                })
            }
        })
        .build();
    let (mut runtime, store) = standard_runtime_with_transport_and_queue_store(transport).await;
    runtime.host.core.durability.queued_work_batching = crate::QueuedWorkBatchingConfig::new(100);
    runtime
        .update_session_config(crate::SessionConfigPatch {
            model: Some(
                crate::ModelSpec::builder("mock-model")
                    .context_window_tokens(1_000)
                    .build()
                    .expect("valid constrained model"),
            ),
            ..Default::default()
        })
        .await
        .expect("constrain context window");

    runtime
        .run_turn_assembled(
            TurnInput::text("r".repeat(900)),
            CancellationToken::new(),
            named_turn_scope("root", "seed-retained-history"),
        )
        .await
        .expect("seed retained history without queued work");

    let registry = runtime
        .host
        .process_registry()
        .cloned()
        .expect("process registry");
    registry
        .register_process(
            crate::ProcessRegistration::new(
                "reserve-proc",
                crate::ProcessInput::External {
                    metadata: serde_json::Value::Null,
                },
                crate::RecoveryContract::ExternallyOwned,
                crate::ProcessProvenance::session(crate::SessionScope::new("root")),
            )
            .with_extra_event_types([process_wake_event_type()])
            .with_wake_session_id(Some("root".to_string())),
        )
        .await
        .expect("register wake process");
    append_process_wake_to_queue(
        registry.as_ref(),
        store.as_ref(),
        "reserve-proc",
        crate::ProcessEventAppendRequest::new(
            "process.wake",
            json!({"text": "short wake", "value": {"status": "done"}}),
        ),
    )
    .await;

    let batch_id = crate::store::QueuedWorkStore::list_pending_queued_work(store.as_ref(), "root")
        .await
        .expect("list wake for selected drain")
        .into_iter()
        .next()
        .expect("queued wake")
        .batch_id;

    runtime
        .stream_selected_queued_work(
            TurnOptions::new(
                CancellationToken::new(),
                named_turn_scope("root", "small-window-drain"),
            ),
            &[batch_id],
        )
        .await
        .expect("a short wake drains under a small window");
    assert_eq!(provider_calls.load(Ordering::SeqCst), 2);
    let pending = crate::store::QueuedWorkStore::list_pending_queued_work(store.as_ref(), "root")
        .await
        .expect("list drained queue");
    assert!(
        pending.is_empty(),
        "the drained wake must not remain pending: {pending:?}"
    );
}

/// FIG-1313: an exact host selection is not resized by the automatic policy.
///
/// The host named this composition, so the drain policy — which answers only
/// "how much of the pending queue should this wake take?" — is not consulted.
/// Under the shipped one-at-a-time default a policy-sized exact claim would
/// take one of the two requested rows, and the caller would abandon the partial
/// claim as unclaimable: `stream_selected_queued_work` on two mergeable wakes
/// could then never succeed, permanently and deterministically.
#[tokio::test]
async fn an_exact_two_row_selection_drains_under_the_one_at_a_time_default() {
    let requests = Arc::new(Mutex::new(Vec::new()));
    let captured_requests = Arc::clone(&requests);
    let transport = TestProvider::builder()
        .kind("mock")
        .requires_streaming(true)
        .complete(move |req| {
            let captured_requests = Arc::clone(&captured_requests);
            async move {
                captured_requests.lock_recover().push(req);
                Ok(LlmResponse {
                    parts: vec![LlmOutputPart::Text {
                        text: "both wakes answered".to_string(),
                        response_meta: None,
                    }],
                    response_metadata: Default::default(),
                    ..LlmResponse::default()
                })
            }
        })
        .build();
    let (mut runtime, store) = standard_runtime_with_transport_and_queue_store(transport).await;
    // The shipped default: no `with_drain_mode`, so `DrainMode::OneAtATime`.
    runtime.host.core.durability.queued_work_batching = crate::QueuedWorkBatchingConfig::new(100);
    assert_eq!(
        runtime
            .host
            .core
            .durability
            .queued_work_batching
            .drain_policy()
            .name(),
        "one_at_a_time"
    );
    let registry = runtime
        .host
        .process_registry()
        .cloned()
        .expect("process registry");
    registry
        .register_process(
            crate::ProcessRegistration::new(
                "paired-wake-proc",
                crate::ProcessInput::External {
                    metadata: serde_json::Value::Null,
                },
                crate::RecoveryContract::ExternallyOwned,
                crate::ProcessProvenance::session(crate::SessionScope::new("root")),
            )
            .with_extra_event_types([process_wake_event_type()])
            .with_wake_session_id(Some("root".to_string())),
        )
        .await
        .expect("register wake process");
    for text in ["first paired wake", "second paired wake"] {
        append_process_wake_to_queue(
            registry.as_ref(),
            store.as_ref(),
            "paired-wake-proc",
            crate::ProcessEventAppendRequest::new(
                "process.wake",
                json!({"text": text, "value": {"status": "done"}}),
            ),
        )
        .await;
    }

    // Both rows share `PROCESS_WAKE_MERGE_KEY`, so they are mergeable and the
    // automatic policy would have a choice to make here.
    let batch_ids = crate::store::QueuedWorkStore::list_pending_queued_work(store.as_ref(), "root")
        .await
        .expect("list queued wakes")
        .into_iter()
        .map(|batch| batch.batch_id)
        .collect::<Vec<_>>();
    assert_eq!(batch_ids.len(), 2);

    runtime
        .stream_selected_queued_work(
            TurnOptions::new(
                CancellationToken::new(),
                named_turn_scope("root", "paired-exact-drain"),
            ),
            &batch_ids,
        )
        .await
        .expect("an exact two-row selection is claimable")
        .expect("the exact selection produces a turn");

    let pending = crate::store::QueuedWorkStore::list_pending_queued_work(store.as_ref(), "root")
        .await
        .expect("list queue after exact drain");
    assert!(
        pending.is_empty(),
        "both selected rows must drain together: {pending:?}"
    );
    let requests = requests.lock_recover().clone();
    let last = requests.last().expect("a provider call");
    assert!(request_contains_text(last, "first paired wake"));
    assert!(
        request_contains_text(last, "second paired wake"),
        "the exact composition, not a policy-sized prefix, must reach the model"
    );
}

/// FIG-1313 red-side anchor (b): the irreducible residue stays typed.
///
/// A single queued row larger than the whole context window can never be
/// drained by any policy. It must name itself and the window it needs, not
/// wedge the queue silently.
#[tokio::test]
async fn an_irreducibly_oversized_queued_row_is_refused_by_name() {
    let transport = TestProvider::builder()
        .kind("mock")
        .requires_streaming(true)
        .complete(move |_| async move {
            panic!("an irreducibly oversized row must never reach the provider");
        })
        .build();
    let (mut runtime, store) = standard_runtime_with_transport_and_queue_store(transport).await;
    runtime.host.core.durability.queued_work_batching = crate::QueuedWorkBatchingConfig::new(100);
    runtime
        .update_session_config(crate::SessionConfigPatch {
            model: Some(
                crate::ModelSpec::builder("mock-model")
                    .context_window_tokens(1_000)
                    .build()
                    .expect("valid constrained model"),
            ),
            ..Default::default()
        })
        .await
        .expect("constrain context window");

    let registry = runtime
        .host
        .process_registry()
        .cloned()
        .expect("process registry");
    registry
        .register_process(
            crate::ProcessRegistration::new(
                "oversized-proc",
                crate::ProcessInput::External {
                    metadata: serde_json::Value::Null,
                },
                crate::RecoveryContract::ExternallyOwned,
                crate::ProcessProvenance::session(crate::SessionScope::new("root")),
            )
            .with_extra_event_types([process_wake_event_type()])
            .with_wake_session_id(Some("root".to_string())),
        )
        .await
        .expect("register wake process");
    let wake = append_process_wake_to_queue(
        registry.as_ref(),
        store.as_ref(),
        "oversized-proc",
        crate::ProcessEventAppendRequest::new(
            "process.wake",
            json!({"text": "w".repeat(4_000), "value": {"status": "done"}}),
        ),
    )
    .await;

    let batch_id = crate::store::QueuedWorkStore::list_pending_queued_work(store.as_ref(), "root")
        .await
        .expect("list oversized wake")
        .into_iter()
        .next()
        .expect("queued wake")
        .batch_id;

    let err = runtime
        .stream_selected_queued_work(
            TurnOptions::new(
                CancellationToken::new(),
                named_turn_scope("root", "oversized-row"),
            ),
            std::slice::from_ref(&batch_id),
        )
        .await
        .expect_err("a row larger than the window must be refused");
    let super::super::turn_loop::SelectedQueuedWorkDrainError::Refused { cause } = err else {
        panic!("an oversized row must surface as a typed refusal, not a bare runtime error");
    };
    let super::super::turn_loop::SelectedQueuedWorkDrainRefusalCause::
        QueuedItemExceedsContextWindow {
            batch_id: refused_batch_id,
            required_context_tokens,
            max_context_tokens,
            ..
        } = cause
    else {
        panic!("expected the oversized-row refusal, got {cause:?}");
    };
    assert_eq!(refused_batch_id, batch_id);
    assert_eq!(max_context_tokens, 1_000);
    assert!(required_context_tokens > max_context_tokens);
    let pending = crate::store::QueuedWorkStore::list_pending_queued_work(store.as_ref(), "root")
        .await
        .expect("list refused wake");
    assert_eq!(pending.len(), 1);
    assert!(matches!(
        &pending[0].items[0].payload,
        crate::QueuedWorkPayload::ProcessWake { wake: pending_wake }
            if pending_wake.wake_id == wake.wake_id
    ));
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
                        crate::plugin::PluginOperationSpec {
                            name: "test.spawn".to_string(),
                            description: "spawn".to_string(),
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
                                        crate::test_support::PluginOperationFailure::new(err.to_string())
                                    });
                                match handle {
                                    Ok(handle) => {
                                        let snapshot = ctx
                                            .sessions
                                            .snapshot_session(&handle.session_id)
                                            .await
                                            .map_err(|err| {
                                                crate::test_support::PluginOperationFailure::new(err.to_string())
                                            });
                                        match snapshot {
                                            Ok(snapshot) => Ok(crate::plugin::ErasedPluginOperationOutcome {
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
            parts: vec![Part::text(
                "m0.p0".to_string(),
                "root msg".to_string(),
                None,
            )]
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
                        crate::plugin::PluginOperationSpec {
                            name: "test.emit".to_string(),
                            description: "emit one durable event".to_string(),
                            session_param: crate::SessionParam::Optional,
                            input_schema: json!({}),
                            output_schema: json!({}),
                        },
                        Arc::new(|_, _| {
                            Box::pin(async move {
                                Ok(crate::plugin::ErasedPluginOperationOutcome {
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
    let operation_scope =
        crate::ExecutionScope::runtime_operation("root:plugin-command:stable-request");

    first
        .run_plugin_command("test.emit", json!({}), None, operation_scope.clone())
        .await
        .expect("first command attempt");
    let committed_after_first = *store.runtime_commit_count.lock_recover();
    let mut retry = runtime_with_plugins_and_tools_and_host_and_store(
        vec![plugin],
        Arc::new(EmptyTools),
        mock_provider(Vec::new()),
        test_host_config(),
        store_trait,
    )
    .await;

    retry
        .run_plugin_command("test.emit", json!({}), None, operation_scope)
        .await
        .expect("lost-response retry");

    assert_eq!(committed_after_first, 2);
    assert_eq!(
        *store.runtime_commit_count.lock_recover(),
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
        Arc::new(crate::NativeRuntimeEffectController::default()),
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
async fn session_manager_preserves_runtime_error_from_child_session_turn() {
    let factory = RecordingSessionStoreFactory::default();
    let host = test_host_config().with_session_store_factory(Arc::new(factory.clone()));
    let runtime = runtime_with_plugins_and_tools_and_host(
        Vec::new(),
        Arc::new(EmptyTools),
        mock_provider(Vec::new()),
        host,
    )
    .await;
    let lifecycle = runtime
        .session_lifecycle_service()
        .expect("session lifecycle");
    let handle = lifecycle
        .create_session(
            crate::SessionCreateRequest::root(
                crate::SessionStartPoint::Empty,
                crate::PluginOptions::default(),
            )
            .with_session_id("busy-child")
            .with_plugin_source(crate::SessionPluginSource::CurrentSessionFork),
        )
        .await
        .expect("child session");
    let store = factory
        .stores()
        .into_iter()
        .find(|store| {
            store
                .session_meta
                .lock_recover()
                .as_ref()
                .is_some_and(|meta| meta.session_id == handle.session_id)
        })
        .expect("child session store");
    let held_lease = crate::store::SessionExecutionLeaseStore::try_claim_session_execution_lease(
        store.as_ref(),
        &handle.session_id,
        &lease_owner("other-child-runtime"),
        "session-manager-runtime-error-boundary-executor",
        60_000,
    )
    .await
    .expect("claim child session execution lease")
    .acquired()
    .expect("child session execution lease");
    let turn_id = "busy-child-turn";
    let controller = crate::ScopedEffectController::shared(
        Arc::new(crate::NativeRuntimeEffectController::default()),
        crate::ExecutionScope::turn(&handle.session_id, turn_id),
    )
    .expect("child turn controller");

    let error = lifecycle
        .start_turn(
            crate::SessionTurnRequest::new(
                &handle.session_id,
                turn_id,
                TurnInput::text("preserve the runtime error"),
                controller,
            )
            .expect("child turn request"),
        )
        .await
        .expect_err("the held child session lane must refuse the turn");

    assert!(
        matches!(
            error,
            crate::PluginError::Runtime(ref runtime_error)
                if runtime_error.code == crate::RuntimeErrorCode::SessionExecutionLaneBusy
        ),
        "managed turn boundary must preserve the typed runtime error, got {error:?}"
    );
    crate::store::SessionExecutionLeaseStore::release_session_execution_lease(
        store.as_ref(),
        &held_lease.completion(),
    )
    .await
    .expect("release child session execution lease");
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
            parts: vec![Part::text(
                "u1.p0".to_string(),
                "parent hello".to_string(),
                None,
            )]
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
    let read = crate::store::SessionCommitStore::load_session(stores[0].as_ref())
        .await
        .expect("load session")
        .expect("session read");
    let graph = read.graph;
    let child_frame_node_id =
        crate::session_graph::frame_node_id(&meta.session_id, "initial-frame");
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
                runtime.state.effective_policy().clone(),
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
            Arc::new(crate::NativeRuntimeEffectController::default()),
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

/// A failed activation must not consume the managed-session handle.
/// `try_into_runtime` returns the intact handle in `Err`; discarding it removed
/// the child from the registry for good, so it could never be activated again
/// without a cold reopen.
#[tokio::test]
async fn failed_managed_session_activation_leaves_the_child_activatable() {
    let mut runtime = runtime_with_plugins(Vec::new(), mock_provider(Vec::new())).await;
    let lifecycle = runtime
        .session_lifecycle_service()
        .expect("session lifecycle");
    lifecycle
        .create_session(
            crate::SessionCreateRequest::child(
                runtime.session_id(),
                crate::SessionStartPoint::Empty,
                runtime.state.effective_policy().clone(),
                crate::PluginOptions::default(),
                "test",
            )
            .with_session_id("child")
            .with_plugin_source(crate::SessionPluginSource::CurrentSessionFork),
        )
        .await
        .expect("child session");

    // A second reference to the child runtime — what an in-flight observation or
    // child turn holds — makes the extraction fail.
    let in_use = runtime
        .managed_sessions
        .lock()
        .await
        .get("child")
        .cloned()
        .expect("managed child handle");
    let err = runtime
        .activate_managed_session("child")
        .await
        .expect_err("activation of an in-use child must fail");
    assert!(err.to_string().contains("still in use"));
    assert!(
        runtime.managed_sessions.lock().await.contains_key("child"),
        "a failed activation must leave the child in the registry"
    );

    drop(in_use);
    runtime
        .activate_managed_session("child")
        .await
        .expect("activation is retryable once the child is no longer in use");
    assert_eq!(runtime.session_id(), "child");
}

#[test]
fn queued_work_payload_cannot_encode_persisted_turn_input() {
    // This exhaustive match is the type-level ingress proof: generic queued
    // work has no model-visible TurnInput representation. Persisted user input
    // therefore has to cross the dedicated PendingTurnInputDraft/
    // TurnInputStore seam used by `LashRuntime::enqueue_turn_input`.
    fn work_class(payload: &crate::QueuedWorkPayload) -> crate::store::QueuedWorkClass {
        match payload {
            crate::QueuedWorkPayload::ProcessWake { .. }
            | crate::QueuedWorkPayload::AgentFrameTask { .. } => {
                crate::store::QueuedWorkClass::TurnWork
            }
            crate::QueuedWorkPayload::SessionCommand { .. } => {
                crate::store::QueuedWorkClass::SessionCommand
            }
        }
    }

    let payload =
        crate::QueuedWorkPayload::session_command(crate::SessionCommand::RefreshToolCatalog {
            reason: "type-level ingress proof".to_string(),
        });
    assert_eq!(work_class(&payload), payload.work_class());
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
                *captured.lock_recover() = Some(req.model_variant.clone());
                Ok(LlmResponse {
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
    let model = crate::ModelSpec::builder("mock-model")
        .variant(crate::ReasoningSelection::Effort("xhigh".to_string()))
        .context_window_tokens(200_000)
        .build()
        .expect("valid model spec")
        .with_capability(capability);

    let mut runtime = runtime_with_plugins(Vec::new(), mock_provider(Vec::new())).await;
    runtime
        .update_session_config(crate::SessionConfigPatch {
            provider: Some(provider),
            model: Some(model),
            ..Default::default()
        })
        .await
        .expect("update session config");

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
        .lock_recover()
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
    let model = crate::ModelSpec::builder("mock-model")
        .variant(crate::ReasoningSelection::Effort("turbo".to_string()))
        .context_window_tokens(200_000)
        .build()
        .expect("valid model spec")
        .with_capability(capability);

    let mut runtime = runtime_with_plugins(Vec::new(), mock_provider(Vec::new())).await;
    runtime
        .update_session_config(crate::SessionConfigPatch {
            provider: Some(provider),
            model: Some(model),
            ..Default::default()
        })
        .await
        .expect("update session config");

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

#[tokio::test]
async fn session_generation_options_reach_every_provider_request() {
    use std::num::NonZeroUsize;
    use std::sync::{Arc, Mutex};

    let captured: Arc<Mutex<Vec<crate::GenerationOptions>>> = Arc::new(Mutex::new(Vec::new()));
    let captured_for_provider = Arc::clone(&captured);
    let provider = TestProvider::builder()
        .kind("generation-capture")
        // A provider-level output cap is provider configuration, not request
        // intent: it must not appear on the request the turn driver builds.
        // The adapter layers it under the request in `resolve_generation_policy`.
        .options(crate::ProviderOptions {
            max_output_tokens: Some(1_024),
            ..Default::default()
        })
        .complete(move |req| {
            let captured = Arc::clone(&captured_for_provider);
            async move {
                captured.lock_recover().push(req.generation.clone());
                Ok(LlmResponse {
                    parts: vec![LlmOutputPart::Text {
                        text: "ok".to_string(),
                        response_meta: None,
                    }],
                    ..LlmResponse::default()
                })
            }
        })
        .build()
        .into_handle();

    let mut runtime = runtime_with_plugins(Vec::new(), mock_provider(Vec::new())).await;
    runtime
        .update_session_config(crate::SessionConfigPatch {
            provider: Some(provider),
            ..Default::default()
        })
        .await
        .expect("update session config");

    let run_turn = async |runtime: &mut LashRuntime, turn_id: &'static str| {
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
                named_turn_scope("root", turn_id),
            )
            .await
            .expect("turn");
    };

    run_turn(&mut runtime, "generation-default-turn").await;

    let requested = crate::GenerationOptions {
        output_token_cap: NonZeroUsize::new(64),
        temperature: Some(crate::NonNegativeFiniteF64::new(0.0).expect("finite temperature")),
        seed: Some(1234),
        stop_sequences: Vec::new(),
        ..Default::default()
    };
    runtime
        .update_session_config(crate::SessionConfigPatch {
            generation: Some(crate::GenerationOverlay::Replace(requested.clone())),
            ..Default::default()
        })
        .await
        .expect("update session config");
    run_turn(&mut runtime, "generation-requested-turn").await;

    let seen = captured.lock_recover().clone();
    assert_eq!(seen.len(), 2, "each turn issues one provider call");
    assert_eq!(
        seen[0],
        crate::GenerationOptions::default(),
        "a session that requested nothing must not have provider config echoed back as request intent"
    );
    assert_eq!(
        seen[1], requested,
        "the session's generation options must reach the provider request verbatim"
    );
    assert_eq!(
        runtime.session_policy().generation,
        requested,
        "the requested options are durable session policy, not per-turn state"
    );
}

#[tokio::test]
async fn omitted_generation_options_are_reported_on_the_turn_llm_call_record() {
    use std::num::NonZeroUsize;

    // The adapter's silent omission (a model that pins sampling, a wire with
    // no seed field) stays silent so one session-wide setting works across
    // mixed models — but the turn record says what actually reached the wire,
    // so a host asserting repeatability learns it was not honored.
    let dropped_sampling = crate::GenerationReceipt {
        output_token_cap: crate::GenerationOptionOutcome::Applied,
        temperature: crate::GenerationOptionOutcome::OmittedSamplingPinned,
        seed: crate::GenerationOptionOutcome::OmittedUnsupported,
        stop_sequences: crate::GenerationOptionOutcome::NotRequested,
        cache: crate::GenerationOptionOutcome::NotRequested,
    };
    let provider = TestProvider::builder()
        .kind("disposition-reporting")
        .complete(move |_req| async move {
            Ok(LlmResponse {
                parts: vec![LlmOutputPart::Text {
                    text: "ok".to_string(),
                    response_meta: None,
                }],
                generation_disposition: Some(dropped_sampling),
                ..LlmResponse::default()
            })
        })
        .build()
        .into_handle();

    let mut runtime = runtime_with_plugins(Vec::new(), mock_provider(Vec::new())).await;
    runtime
        .update_session_config(crate::SessionConfigPatch {
            provider: Some(provider),
            generation: Some(crate::GenerationOverlay::Replace(
                crate::GenerationOptions {
                    output_token_cap: NonZeroUsize::new(128),
                    temperature: Some(
                        crate::NonNegativeFiniteF64::new(0.2).expect("finite temperature"),
                    ),
                    seed: Some(99),
                    stop_sequences: Vec::new(),
                    ..Default::default()
                },
            )),
            ..Default::default()
        })
        .await
        .expect("update session config");

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
            named_turn_scope("root", "generation-disposition-turn"),
        )
        .await
        .expect("turn");

    let attempt = turn
        .llm_calls
        .first()
        .expect("one provider call")
        .attempts
        .first()
        .expect("one attempt");
    let reported = attempt
        .generation_disposition
        .expect("the adapter reported what it sent");
    assert_eq!(reported, dropped_sampling);
    assert!(
        !reported.nothing_omitted(),
        "a host asserting repeatability must be able to see the omission"
    );
}

#[tokio::test]
async fn an_output_token_cap_above_the_model_clamps_and_says_so() {
    use std::num::NonZeroUsize;
    use std::sync::{Arc, Mutex};

    // The cap is a bound, not a demand, and it is durable session policy: a
    // `update_session_config` selecting a smaller model must not leave the
    // session failing every remaining turn. It sends what the model can
    // produce, and the disposition says the number was reduced.
    let captured: Arc<Mutex<Vec<crate::GenerationOptions>>> = Arc::new(Mutex::new(Vec::new()));
    let captured_for_provider = Arc::clone(&captured);
    let provider = TestProvider::builder()
        .kind("clamping-capture")
        .complete(move |req| {
            let captured = Arc::clone(&captured_for_provider);
            async move {
                captured.lock_recover().push(req.generation.clone());
                Ok(LlmResponse {
                    parts: vec![LlmOutputPart::Text {
                        text: "ok".to_string(),
                        response_meta: None,
                    }],
                    // The adapter reports the cap it was handed as applied; it
                    // has no idea a larger one was asked for.
                    generation_disposition: Some(crate::GenerationReceipt {
                        output_token_cap: crate::GenerationOptionOutcome::Applied,
                        temperature: crate::GenerationOptionOutcome::Applied,
                        seed: crate::GenerationOptionOutcome::NotRequested,
                        stop_sequences: crate::GenerationOptionOutcome::NotRequested,
                        cache: crate::GenerationOptionOutcome::NotRequested,
                    }),
                    ..LlmResponse::default()
                })
            }
        })
        .build()
        .into_handle();

    let mut runtime = runtime_with_plugins(Vec::new(), mock_provider(Vec::new())).await;
    runtime
        .update_session_config(crate::SessionConfigPatch {
            provider: Some(provider),
            model: Some(
                crate::ModelSpec::builder("small-output-model")
                    .context_window_tokens(200_000)
                    .output_token_capacity(2_048)
                    .build()
                    .expect("valid test model"),
            ),
            generation: Some(crate::GenerationOverlay::Replace(
                crate::GenerationOptions {
                    output_token_cap: NonZeroUsize::new(32_000),
                    temperature: Some(
                        crate::NonNegativeFiniteF64::new(0.0).expect("finite temperature"),
                    ),
                    seed: None,
                    stop_sequences: Vec::new(),
                    ..Default::default()
                },
            )),
            ..Default::default()
        })
        .await
        .expect("update session config");

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
            named_turn_scope("root", "clamped-cap-turn"),
        )
        .await
        .expect("a cap above the model's capacity must not fail the turn");

    let seen = captured.lock_recover().clone();
    assert_eq!(
        seen.first().expect("one provider call").output_token_cap,
        NonZeroUsize::new(2_048),
        "the request carries the model's capacity, not the larger cap asked for"
    );
    assert_eq!(
        runtime.session_policy().generation.output_token_cap,
        NonZeroUsize::new(32_000),
        "clamping is per request against the current model; the session's intent is unchanged"
    );

    let reported = turn
        .llm_calls
        .first()
        .expect("one provider call")
        .attempts
        .first()
        .expect("one attempt")
        .generation_disposition
        .expect("the adapter reported what it sent");
    assert_eq!(
        reported.output_token_cap,
        crate::GenerationOptionOutcome::ClampedToCapacity
    );
    assert!(
        reported.nothing_omitted(),
        "a clamped cap reached the wire; it was not dropped"
    );
    assert!(
        !reported.fully_honored(),
        "a host that needs the number it asked for must be able to see the reduction"
    );
}

#[tokio::test]
async fn a_mid_run_generation_patch_merges_like_the_spec_overlay_does() {
    use std::num::NonZeroUsize;

    // Both surfaces that set generation options speak one vocabulary. A patch
    // naming only a cap must not drop a temperature and seed the session
    // pinned — the loss `SessionSpec`'s overlay exists to prevent, one API
    // over — and replacing stays available for a host that means it.
    let mut runtime = runtime_with_plugins(Vec::new(), mock_provider(Vec::new())).await;
    let pinned = crate::GenerationOptions {
        output_token_cap: None,
        temperature: Some(crate::NonNegativeFiniteF64::new(0.0).expect("finite temperature")),
        seed: Some(42),
        stop_sequences: Vec::new(),
        ..Default::default()
    };
    runtime
        .update_session_config(crate::SessionConfigPatch {
            generation: Some(crate::GenerationOverlay::Replace(pinned.clone())),
            ..Default::default()
        })
        .await
        .expect("update session config");

    runtime
        .update_session_config(crate::SessionConfigPatch {
            generation: Some(crate::GenerationOverlay::Merge(crate::GenerationOptions {
                output_token_cap: NonZeroUsize::new(4_096),
                ..Default::default()
            })),
            ..Default::default()
        })
        .await
        .expect("update session config");
    assert_eq!(
        runtime.session_policy().generation,
        crate::GenerationOptions {
            output_token_cap: NonZeroUsize::new(4_096),
            temperature: pinned.temperature.clone(),
            seed: Some(42),
            stop_sequences: Vec::new(),
            ..Default::default()
        },
        "a patch that names only a cap keeps the sampling the session pinned"
    );

    runtime
        .update_session_config(crate::SessionConfigPatch {
            generation: Some(crate::GenerationOverlay::Replace(
                crate::GenerationOptions::default(),
            )),
            ..Default::default()
        })
        .await
        .expect("update session config");
    assert_eq!(
        runtime.session_policy().generation,
        crate::GenerationOptions::default(),
        "an explicit replace still clears every option"
    );
}
