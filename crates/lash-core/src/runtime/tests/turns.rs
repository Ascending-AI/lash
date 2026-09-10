use super::*;
use crate::facade_support::{RuntimeSessionStateFacadeOps, ToolStateFacadeOps};
use lash_sansio::core_support::*;
use lash_sansio::sync::MutexExt;
use std::collections::HashMap;
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
        _state: crate::plugin::ProtocolSessionRestoreView,
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
        state: crate::plugin::ProtocolSessionRestoreView,
    ) -> Result<(), crate::SessionError> {
        let snapshot = state
            .execution_state
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
        state: crate::plugin::ProtocolSessionRestoreView,
    ) -> Result<(), crate::SessionError> {
        if let (Some(executor), Some(snapshot)) = (
            self.executor.as_ref(),
            state
                .execution_state
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
        state: crate::plugin::ProtocolSessionRestoreView,
    ) -> Result<(), crate::SessionError> {
        if let Some(snapshot) =
            state
                .execution_state
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

fn assert_turn_owned_attachment(store: &RecordingStore, turn_id: &TurnId) {
    let entries = store.attachment_manifest_entries();
    assert_eq!(entries.len(), 1);
    assert_eq!(
        entries[0].owner_kind,
        Some(crate::AttachmentOwnerKind::Turn)
    );
    assert_eq!(entries[0].owner_id.as_deref(), Some(turn_id.as_str()));
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
