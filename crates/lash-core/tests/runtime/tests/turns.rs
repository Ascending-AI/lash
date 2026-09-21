use super::*;
use lash_core::facade_support::{RuntimeSessionStateFacadeOps, ToolStateFacadeOps};
use lash_sansio::core_support::*;
use lash_sansio::sync::MutexExt;
use std::collections::HashMap;
use std::sync::atomic::AtomicUsize;

type PluginErrorDiscriminant = std::mem::Discriminant<lash_core::PluginError>;

fn turn_persisted_borrowed_append_plugin(
    attempted: Arc<AtomicBool>,
    received_error: Arc<std::sync::Mutex<Option<PluginErrorDiscriminant>>>,
) -> Arc<dyn lash_core::facade_support::PluginFactory> {
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
                        let lash_core::facade_support::PluginLifecycleEvent::TurnPersisted(ctx) =
                            event
                        else {
                            return Ok(());
                        };
                        if attempted.swap(true, Ordering::SeqCst) {
                            return Ok(());
                        }
                        if let Err(error) = ctx
                            .session_graph
                            .append_session_nodes(
                                &ctx.session_id,
                                lash_core::AppendSessionNodesRequest {
                                    operation_id: "lapsed-borrow-probe".to_string(),
                                    nodes: vec![lash_core::SessionAppendNode::plugin(
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

fn turn_finalized_borrowed_append_plugin() -> Arc<dyn lash_core::facade_support::PluginFactory> {
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
                            lash_core::facade_support::PluginLifecycleEvent::TurnPersisted(ctx) => {
                                *retained.lock_recover() = Some(Arc::clone(&ctx.session_graph));
                                Ok(())
                            }
                            lash_core::facade_support::PluginLifecycleEvent::TurnFinalized(
                                turn,
                            ) => {
                                let graph: Option<Arc<dyn lash_core::plugin::SessionGraphService>> =
                                    retained.lock_recover().clone();
                                let Some(graph) = graph else {
                                    return Ok(());
                                };
                                graph
                                    .append_session_nodes(
                                        &turn.state.session_id,
                                        lash_core::AppendSessionNodesRequest {
                                            operation_id: "finalized-lapsed-borrow-probe"
                                                .to_string(),
                                            nodes: vec![lash_core::SessionAppendNode::plugin(
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
    retained: Arc<std::sync::Mutex<Option<Arc<dyn lash_core::plugin::SessionGraphService>>>>,
) -> Arc<dyn lash_core::facade_support::PluginFactory> {
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
                        if let lash_core::facade_support::PluginLifecycleEvent::TurnPersisted(ctx) =
                            event
                        {
                            *retained.lock_recover() = Some(Arc::clone(&ctx.session_graph));
                            return Err(lash_core::PluginError::Session(
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
impl lash_core::plugin::ProtocolSessionPlugin for FailNextProtocolRestore {
    async fn restore_session(
        &self,
        _ctx: lash_core::plugin::ProtocolSessionContext<'_>,
        _state: lash_core::plugin::ProtocolSessionRestoreView,
    ) -> Result<(), lash_core::SessionError> {
        self.restore_count.fetch_add(1, Ordering::SeqCst);
        if self.fail_next.swap(false, Ordering::SeqCst) {
            return Err(lash_core::SessionError::Protocol(
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

impl lash_core::runtime::RuntimeTurnPhaseProbe for FailCaptureAfterEffectLoop {
    fn begin(&self, _phase: lash_core::runtime::RuntimeTurnPhase) {}

    fn end(&self, phase: lash_core::runtime::RuntimeTurnPhase) {
        if phase == lash_core::runtime::RuntimeTurnPhase::EffectLoop {
            self.executor.dirty.store(true, Ordering::SeqCst);
            self.executor.fail_capture.store(true, Ordering::SeqCst);
        }
    }
}

impl lash_core::runtime::RuntimeTurnPhaseProbe for FailCaptureAfterFirstCommittedTurn {
    fn begin(&self, _phase: lash_core::runtime::RuntimeTurnPhase) {}

    fn end(&self, phase: lash_core::runtime::RuntimeTurnPhase) {
        if phase == lash_core::runtime::RuntimeTurnPhase::CommittedTurn
            && self.committed_turns.fetch_add(1, Ordering::SeqCst) == 0
        {
            self.executor.dirty.store(true, Ordering::SeqCst);
            self.executor.fail_capture.store(true, Ordering::SeqCst);
        }
    }
}

impl lash_core::runtime::RuntimeTurnPhaseProbe for FailCaptureAfterCommittedTurns {
    fn begin(&self, _phase: lash_core::runtime::RuntimeTurnPhase) {}

    fn end(&self, phase: lash_core::runtime::RuntimeTurnPhase) {
        if phase == lash_core::runtime::RuntimeTurnPhase::CommittedTurn
            && self.committed_turns.fetch_add(1, Ordering::SeqCst) + 1 == self.fail_after
        {
            self.executor.dirty.store(true, Ordering::SeqCst);
            self.executor.fail_capture.store(true, Ordering::SeqCst);
        }
    }
}

impl lash_core::runtime::RuntimeTurnPhaseProbe for RecordPostCommitDelivery {
    fn begin(&self, phase: lash_core::runtime::RuntimeTurnPhase) {
        if phase == lash_core::runtime::RuntimeTurnPhase::PostCommitDelivery {
            self.entered.store(true, Ordering::SeqCst);
        }
    }

    fn end(&self, _phase: lash_core::runtime::RuntimeTurnPhase) {}
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
impl lash_core::plugin::CodeExecutorPlugin for FailingCaptureExecutor {
    async fn execute_code(
        &self,
        _ctx: lash_core::RuntimeExecutionContext<'_>,
        _request: lash_core::ExecRequest,
    ) -> Result<lash_core::ExecResponse, lash_core::SessionError> {
        unreachable!("execution-state capture regression does not execute code")
    }

    fn execution_state_dirty(&self) -> bool {
        self.dirty.load(Ordering::SeqCst)
    }

    async fn snapshot_execution_state(
        &self,
        _ctx: lash_core::plugin::ProtocolSessionContext<'_>,
    ) -> Result<lash_core::plugin::ExecutionStateSnapshot, lash_core::SessionError> {
        if self.fail_capture.load(Ordering::SeqCst) {
            return Err(lash_core::SessionError::Protocol(
                "injected dirty execution-state capture failure".to_string(),
            ));
        }
        Ok(lash_core::plugin::ExecutionStateSnapshot::from_root(Some(
            self.snapshot.lock_recover().clone().into(),
        )))
    }

    /// Reports the same obstacle the capture itself would hit, and stages
    /// nothing — the runtime uses this to fail a turn before its provider call.
    async fn probe_execution_state_capture(
        &self,
        _ctx: lash_core::plugin::ProtocolSessionContext<'_>,
    ) -> Result<(), lash_core::SessionError> {
        if self.fail_capture.load(Ordering::SeqCst) {
            return Err(lash_core::SessionError::Protocol(
                "injected dirty execution-state capture failure".to_string(),
            ));
        }
        Ok(())
    }

    async fn restore_execution_state(
        &self,
        _ctx: lash_core::plugin::ProtocolSessionContext<'_>,
        state: &lash_core::plugin::HydratedExecutionState,
    ) -> Result<(), lash_core::SessionError> {
        self.restored.lock_recover().push(state.root.to_vec());
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
impl lash_core::plugin::ProtocolSessionPlugin for ResetExecutorOnSwitchProtocol {
    async fn restore_session(
        &self,
        ctx: lash_core::plugin::ProtocolSessionContext<'_>,
        state: lash_core::plugin::ProtocolSessionRestoreView,
    ) -> Result<(), lash_core::SessionError> {
        let snapshot = state
            .execution_state
            .map_err(|source| lash_core::SessionError::Store {
                context: "hydrate test execution state".to_string(),
                source,
            })?
            .unwrap_or_else(|| lash_core::plugin::HydratedExecutionState {
                root: b"fresh-frame-execution-state".as_slice().into(),
                components: std::collections::BTreeMap::new(),
            });
        lash_core::plugin::CodeExecutorPlugin::restore_execution_state(
            self.executor.as_ref(),
            ctx,
            &snapshot,
        )
        .await
    }

    async fn before_llm_call(
        &self,
        _ctx: lash_core::plugin::ProtocolBeforeLlmCallContext,
        _request: &lash_core::LlmRequest,
    ) -> Result<Option<lash_core::ProtocolLlmCallAction>, lash_core::PluginError> {
        if !self.switch_next.swap(false, Ordering::SeqCst) {
            return Ok(None);
        }
        Ok(Some(lash_core::ProtocolLlmCallAction::SwitchAgentFrame {
            frame_key: lash_core::FrameKey::from_caller_material(&self.frame_key_material)
                .expect("non-empty caller material"),
            task: "reset the resident executor".to_string(),
        }))
    }
}

#[async_trait::async_trait]
impl lash_core::plugin::ProtocolSessionPlugin for SwitchBeforeLlmProtocol {
    async fn restore_session(
        &self,
        ctx: lash_core::plugin::ProtocolSessionContext<'_>,
        state: lash_core::plugin::ProtocolSessionRestoreView,
    ) -> Result<(), lash_core::SessionError> {
        if let (Some(executor), Some(snapshot)) = (
            self.executor.as_ref(),
            state
                .execution_state
                .map_err(|source| lash_core::SessionError::Store {
                    context: "hydrate test execution state".to_string(),
                    source,
                })?,
        ) {
            lash_core::plugin::CodeExecutorPlugin::restore_execution_state(
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
        _ctx: lash_core::plugin::ProtocolBeforeLlmCallContext,
        _request: &lash_core::LlmRequest,
    ) -> Result<Option<lash_core::ProtocolLlmCallAction>, lash_core::PluginError> {
        if !self.switch_next.swap(false, Ordering::SeqCst) {
            return Ok(None);
        }
        Ok(Some(lash_core::ProtocolLlmCallAction::SwitchAgentFrame {
            frame_key: lash_core::FrameKey::from_caller_material(&self.frame_key_material)
                .expect("non-empty caller material"),
            task: "protocol-directed switch".to_string(),
        }))
    }
}

#[async_trait::async_trait]
impl lash_core::plugin::ProtocolSessionPlugin for RestoreExecutorFromRuntimeState {
    async fn restore_session(
        &self,
        ctx: lash_core::plugin::ProtocolSessionContext<'_>,
        state: lash_core::plugin::ProtocolSessionRestoreView,
    ) -> Result<(), lash_core::SessionError> {
        if let Some(snapshot) =
            state
                .execution_state
                .map_err(|source| lash_core::SessionError::Store {
                    context: "hydrate test execution state".to_string(),
                    source,
                })?
        {
            lash_core::plugin::CodeExecutorPlugin::restore_execution_state(
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
impl lash_core::facade_support::EventSink for SuspendingPostCommitSink {
    async fn emit(&self, event: lash_core::facade_support::SessionStreamEvent) {
        if matches!(
            event,
            lash_core::facade_support::SessionStreamEvent::PluginEvent { .. }
        ) {
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

fn attachment_put_tool_definition() -> lash_core::ToolDefinition {
    lash_core::ToolDefinition::raw(
        "tool:attachment_put",
        "attachment_put",
        "Write an attachment through the active runtime facade.",
        lash_core::ToolDefinition::default_input_schema(),
        serde_json::json!({ "type": "object", "additionalProperties": true }),
    )
}

#[async_trait::async_trait]
impl lash_core::ToolProvider for AttachmentPutTool {
    fn tool_manifests(&self) -> Vec<lash_core::ToolManifest> {
        vec![attachment_put_tool_definition().manifest()]
    }

    fn resolve_contract(&self, name: &str) -> Option<Arc<lash_core::ToolContract>> {
        (name == "attachment_put").then(|| Arc::new(attachment_put_tool_definition().contract()))
    }

    async fn execute(&self, call: lash_core::ToolCall<'_>) -> lash_core::ToolAttemptOutcome {
        (async {
            let reference = call
                .context
                .attachments()
                .put(
                    b"turn-owned-tool-attachment".to_vec(),
                    lash_core::AttachmentCreateMeta::new(
                        lash_core::MediaType::parse("image/png").unwrap(),
                        Some(lash_core::AttachmentTypeMetadata::image(Some(1), Some(1))),
                        Some("turn-owned.png".to_string()),
                    ),
                )
                .await
                .expect("tool attachment put");
            lash_core::ToolOutcome::from_output(lash_core::ToolCallOutput::success_tool_value(
                lash_core::ToolValue::Attachment(lash_core::AttachmentSource::stored(reference)),
            ))
        })
        .await
        .into()
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

fn assert_turn_owned_attachment(store: &RecordingStore, turn_id: &TurnId) {
    let entries = store.attachment_manifest_entries();
    assert_eq!(entries.len(), 1);
    assert_eq!(
        entries[0].owner,
        Some(lash_core::AttachmentOwner::Turn {
            id: turn_id.as_str().to_string()
        })
    );
}

fn lease_owner(owner_id: &str) -> lash_core::LeaseOwnerIdentity {
    lash_core::LeaseOwnerIdentity::opaque(owner_id, format!("{owner_id}:incarnation"))
}

#[derive(Debug)]
struct CancelWatchTestClock(lash_core::testing::TestClock);

#[async_trait::async_trait]
impl lash_core::Clock for CancelWatchTestClock {
    fn now(&self) -> std::time::Instant {
        self.0.now()
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

mod checkpoint_progress;
mod drain_and_recovery;
mod drop_cancel_owner_failure;
mod effects_and_queue;
mod lease_and_claims;
mod turn_lifecycle;

use effects_and_queue::*;
use turn_lifecycle::*;

#[path = "commit_placement.rs"]
mod commit_placement;

#[path = "turn_cancel_modes.rs"]
mod turn_cancel_modes;
