use lash_sansio::sync::MutexExt;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use tokio::sync::Semaphore;
use tokio_util::sync::CancellationToken;

use super::RuntimeExecutionContext;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum RecordedProcessCommand {
    Await,
    Cancel,
}

struct FirstAwaitDropSignal {
    state_changed: Arc<Semaphore>,
}

impl Drop for FirstAwaitDropSignal {
    fn drop(&mut self) {
        self.state_changed.add_permits(1);
    }
}

struct RecordingProcessEffectController {
    commands: Mutex<Vec<RecordedProcessCommand>>,
    await_count: AtomicUsize,
    first_await_started: Arc<Semaphore>,
    first_await_state_changed: Arc<Semaphore>,
    release_first_await: Arc<Semaphore>,
}

struct DeletedSessionProcessEffectController;

impl crate::AwaitEventResolver for DeletedSessionProcessEffectController {}

#[async_trait::async_trait]
impl crate::RuntimeEffectController for DeletedSessionProcessEffectController {
    async fn execute_effect(
        &self,
        envelope: crate::RuntimeEffectEnvelope,
        _local_executor: crate::RuntimeEffectLocalExecutor<'_>,
    ) -> Result<crate::RuntimeEffectOutcome, crate::RuntimeEffectControllerError> {
        assert!(
            matches!(
                envelope.command,
                crate::RuntimeEffectCommand::Process { .. }
            ),
            "deleted-session fixture only accepts process commands"
        );
        Err(crate::RuntimeEffectControllerError::from(
            crate::StoreError::SessionDeleted {
                session_id: "fig790-session".to_string(),
            },
        ))
    }
}

impl Default for RecordingProcessEffectController {
    fn default() -> Self {
        Self {
            commands: Mutex::default(),
            await_count: AtomicUsize::default(),
            first_await_started: Arc::new(Semaphore::new(0)),
            first_await_state_changed: Arc::new(Semaphore::new(0)),
            release_first_await: Arc::new(Semaphore::new(0)),
        }
    }
}

impl RecordingProcessEffectController {
    fn commands(&self) -> Vec<RecordedProcessCommand> {
        self.commands.lock_recover().clone()
    }
}

impl crate::AwaitEventResolver for RecordingProcessEffectController {}

#[async_trait::async_trait]
impl crate::RuntimeEffectController for RecordingProcessEffectController {
    async fn execute_effect(
        &self,
        envelope: crate::RuntimeEffectEnvelope,
        local_executor: crate::RuntimeEffectLocalExecutor<'_>,
    ) -> Result<crate::RuntimeEffectOutcome, crate::RuntimeEffectControllerError> {
        let crate::RuntimeEffectCommand::Process { command } = envelope.command else {
            return Err(crate::RuntimeEffectControllerError::foreign(
                "fig790_test_command",
                "recording controller only accepts process commands",
            ));
        };
        match *command {
            crate::ProcessCommand::Await { process_id } => {
                self.commands
                    .lock_recover()
                    .push(RecordedProcessCommand::Await);
                let execution = local_executor.into_process()?;
                if self.await_count.fetch_add(1, Ordering::SeqCst) == 0 {
                    let _drop_signal = FirstAwaitDropSignal {
                        state_changed: Arc::clone(&self.first_await_state_changed),
                    };
                    self.first_await_started.add_permits(1);
                    if let Some(turn_cancellation) = execution.turn_cancellation {
                        turn_cancellation.cancellation.cancelled().await;
                        self.first_await_state_changed.add_permits(1);
                    }
                    self.release_first_await
                        .acquire()
                        .await
                        .expect("release first process await")
                        .forget();
                }
                Ok(crate::RuntimeEffectOutcome::Process {
                    result: crate::ProcessEffectOutcome::Await {
                        output: Box::new(crate::ProcessAwaitOutput::from_tool_output(
                            crate::ToolCallOutput::success(
                                serde_json::json!({ "process_id": process_id }),
                            ),
                        )),
                    },
                })
            }
            crate::ProcessCommand::Cancel { process_id, .. } => {
                self.commands
                    .lock_recover()
                    .push(RecordedProcessCommand::Cancel);
                let record =
                    crate::ProcessRecord::from_registration(crate::ProcessRegistration::new(
                        process_id,
                        crate::ProcessInput::External {
                            metadata: serde_json::Value::Null,
                        },
                        crate::RecoveryContract::ExternallyOwned,
                        crate::ProcessProvenance::host(),
                    ));
                Ok(crate::RuntimeEffectOutcome::Process {
                    result: crate::ProcessEffectOutcome::Cancel {
                        record: Box::new(record),
                    },
                })
            }
            command => Err(crate::RuntimeEffectControllerError::foreign(
                "fig790_test_command",
                format!("unexpected process command: {command:?}"),
            )),
        }
    }
}

struct EffectBackedProcessService {
    registry: Arc<dyn crate::ProcessRegistry>,
}

impl EffectBackedProcessService {
    async fn execute(
        &self,
        scope: crate::ProcessOpScope<'_>,
        command: crate::ProcessCommand,
    ) -> Result<crate::ProcessEffectOutcome, crate::PluginError> {
        let mut local_executor =
            crate::RuntimeEffectLocalExecutor::processes(Arc::clone(&self.registry), None);
        if let Some(turn_cancellation) = scope.turn_cancellation.clone() {
            local_executor = local_executor.with_process_turn_cancellation(turn_cancellation);
        }
        let effect_id = command.effect_id();
        let envelope = crate::RuntimeEffectEnvelope::new(
            crate::RuntimeInvocation::effect(
                crate::RuntimeScope::new("fig790-test"),
                effect_id.clone(),
                crate::RuntimeEffectKind::Process,
                effect_id,
            ),
            crate::RuntimeEffectCommand::process(command),
        );
        scope
            .controller()
            .execute_effect(envelope, local_executor)
            .await?
            .into_process()
            .map_err(crate::PluginError::from)
    }
}

#[async_trait::async_trait]
impl crate::ProcessService for EffectBackedProcessService {
    async fn start_from_recorded_intent(
        &self,
        _session_id: &str,
        _request: crate::ProcessStartRequest,
        _scope: crate::ProcessOpScope<'_>,
    ) -> Result<crate::ProcessHandleView, crate::PluginError> {
        Err(crate::PluginError::Session(
            "recorded intent start is not used by the FIG-790 fixture".to_string(),
        ))
    }

    async fn finish_recorded_intent_parent(
        &self,
        _session_id: &str,
        identity: crate::ToolIntentIdentity,
        process_id: String,
        policy: crate::ProcessParentEndPolicy,
        reason: String,
        scope: crate::ProcessOpScope<'_>,
    ) -> Result<crate::ToolIntentParentEndOutcome, crate::PluginError> {
        match self
            .execute(
                scope,
                crate::ProcessCommand::ParentEnd {
                    identity,
                    process_id,
                    policy,
                    reason,
                },
            )
            .await?
        {
            crate::ProcessEffectOutcome::ParentEnd { outcome } => Ok(*outcome),
            _ => unreachable!("parent-end command returns parent-end outcome"),
        }
    }

    async fn start(
        &self,
        _session_id: &str,
        _registration: crate::ProcessRegistration,
        _options: crate::ProcessStartOptions,
        _scope: crate::ProcessOpScope<'_>,
    ) -> Result<crate::ProcessRecord, crate::PluginError> {
        Err(crate::PluginError::Session(
            "start is not used by the FIG-790 fixture".to_string(),
        ))
    }

    async fn await_process(
        &self,
        process_id: &str,
        scope: crate::ProcessOpScope<'_>,
    ) -> Result<crate::ProcessAwaitOutput, crate::PluginError> {
        match self
            .execute(
                scope,
                crate::ProcessCommand::Await {
                    process_id: process_id.to_string(),
                },
            )
            .await?
        {
            crate::ProcessEffectOutcome::Await { output } => Ok(*output),
            _ => unreachable!("await command returns await outcome"),
        }
    }

    async fn list_visible(
        &self,
        _session_id: &str,
        _mode: crate::ProcessListMode,
        _scope: crate::ProcessOpScope<'_>,
    ) -> Result<Vec<crate::ProcessRecord>, crate::PluginError> {
        Err(crate::PluginError::Session(
            "list is not used by the FIG-790 fixture".to_string(),
        ))
    }

    async fn validate_visible(
        &self,
        _session_id: &str,
        _process_ids: &[String],
        _scope: crate::ProcessOpScope<'_>,
    ) -> Result<(), crate::PluginError> {
        Err(crate::PluginError::Session(
            "validation is not used by the FIG-790 fixture".to_string(),
        ))
    }

    async fn cancel(
        &self,
        _session_id: &str,
        process_id: &str,
        scope: crate::ProcessOpScope<'_>,
    ) -> Result<crate::ProcessRecord, crate::PluginError> {
        match self
            .execute(
                scope,
                crate::ProcessCommand::Cancel {
                    process_id: process_id.to_string(),
                    reason: Some("turn cancelled while awaiting process".to_string()),
                    replay: None,
                },
            )
            .await?
        {
            crate::ProcessEffectOutcome::Cancel { record } => Ok(*record),
            _ => unreachable!("cancel command returns cancel outcome"),
        }
    }

    async fn cancel_recorded_intent(
        &self,
        session_id: &str,
        process_id: &str,
        _reason: Option<String>,
        scope: crate::ProcessOpScope<'_>,
    ) -> Result<crate::ProcessRecord, crate::PluginError> {
        self.cancel(session_id, process_id, scope).await
    }

    async fn signal_possessed(
        &self,
        _session_id: &str,
        _process_id: &str,
        _signal_name: String,
        _signal_id: String,
        _payload: serde_json::Value,
        _scope: crate::ProcessOpScope<'_>,
    ) -> Result<crate::ProcessEvent, crate::PluginError> {
        Err(crate::PluginError::Session(
            "signal is not used by the FIG-790 fixture".to_string(),
        ))
    }

    async fn signal_recorded_intent(
        &self,
        _session_id: &str,
        _process_id: &str,
        _signal_name: String,
        _signal_id: String,
        _payload: serde_json::Value,
        _scope: crate::ProcessOpScope<'_>,
    ) -> Result<crate::ProcessEvent, crate::PluginError> {
        Err(crate::PluginError::Session(
            "recorded intent signal is not used by the FIG-790 fixture".to_string(),
        ))
    }

    async fn emit_event_recorded_intent(
        &self,
        _session_id: &str,
        _process_id: &str,
        _event_type: String,
        _replay_key: String,
        _payload: serde_json::Value,
        _scope: crate::ProcessOpScope<'_>,
    ) -> Result<crate::ProcessEvent, crate::PluginError> {
        Err(crate::PluginError::Session(
            "recorded intent event is not used by the FIG-790 fixture".to_string(),
        ))
    }

    async fn transfer(
        &self,
        _from_session_id: &str,
        _to_session_id: &str,
        _process_ids: Vec<String>,
        _scope: crate::ProcessOpScope<'_>,
    ) -> Result<(), crate::PluginError> {
        Err(crate::PluginError::Session(
            "transfer is not used by the FIG-790 fixture".to_string(),
        ))
    }
}

struct NoopTools;

#[async_trait::async_trait]
impl crate::ToolProvider for NoopTools {
    fn tool_manifests(&self) -> Vec<crate::ToolManifest> {
        Vec::new()
    }

    fn resolve_contract(&self, _name: &str) -> Option<Arc<crate::ToolContract>> {
        None
    }

    async fn execute(&self, _call: crate::ToolCall<'_>) -> crate::ToolOutcome {
        crate::ToolOutcome::err_fmt("not used by the FIG-790 fixture")
    }
}

fn fig790_process_await_context(
    controller: Arc<dyn crate::RuntimeEffectController>,
) -> RuntimeExecutionContext<'static> {
    let host = Arc::new(crate::testing::MockSessionManager::default());
    let registry: Arc<dyn crate::ProcessRegistry> = host.process_registry.clone();
    let processes: Arc<dyn crate::ProcessService> =
        Arc::new(EffectBackedProcessService { registry });
    let plugins = crate::plugin::PluginHost::empty()
        .build_session("fig790-session")
        .expect("FIG-790 plugin session");
    let (event_tx, _event_rx) = tokio::sync::mpsc::channel(1);
    let attachment_store = Arc::new(crate::SessionAttachmentStore::in_memory());
    let dispatch = Arc::new(crate::tool_dispatch::ToolDispatchContext {
        plugins,
        tools: Arc::new(NoopTools),
        tool_registry: None,
        tool_catalog: Arc::new(crate::ToolCatalog::from_tool_definitions(Vec::new())),
        sessions: host.clone(),
        session_lifecycle: host.clone(),
        session_graph: host,
        processes,
        trigger_router: None,
        effect_controller: crate::runtime::RuntimeEffectControllerHandle::shared(controller),
        direct_completions: crate::DirectCompletionClient::unavailable(
            "direct completions are unavailable in this test context",
        ),
        parent_invocation: None,
        execution_env_spec: crate::ProcessExecutionEnvSpec::new(
            crate::PluginOptions::default(),
            crate::SessionPolicy::new(crate::TurnBudget::Unbounded),
        ),
        session_id: "fig790-session".to_string(),
        agent_frame_id: String::new(),
        event_tx,
        checkpoint_messages: crate::tool_dispatch::CheckpointMessageBuffer::default(),
        trigger_outcomes: crate::tool_dispatch::ToolTriggerOutcomeBuffer::default(),
        recorded_intent_outcomes: crate::tool_dispatch::RecordedToolIntentOutcomeBuffer::default(),
        attachment_store: Arc::clone(&attachment_store),
        attachment_source_policy: Arc::new(crate::OpenAttachmentSourcePolicy),
        turn_context: crate::TurnContext::default(),
        clock: Arc::new(crate::SystemClock),
    });
    RuntimeExecutionContext::new(
        "fig790-session".to_string(),
        dispatch,
        Arc::new(crate::InMemoryProcessExecutionEnvStore::new()),
        attachment_store,
        Arc::new(crate::ChronologicalProjection::default()),
        None,
        crate::TurnContext::default(),
    )
}

async fn assert_cancelled_process_await_emits_one_await(already_cancelled: bool) {
    let controller = Arc::new(RecordingProcessEffectController::default());
    let context = fig790_process_await_context(controller.clone());
    let cancellation = CancellationToken::new();
    if already_cancelled {
        cancellation.cancel();
    }
    let cancellation_for_call = cancellation.clone();
    let await_task = crate::task::spawn(async move {
        context
            .await_process_with_cancellation("fig790-process", None, Some(cancellation_for_call))
            .await
    });

    controller
        .first_await_started
        .acquire()
        .await
        .expect("first await starts")
        .forget();
    if !already_cancelled {
        cancellation.cancel();
    }
    if controller.commands().first() != Some(&RecordedProcessCommand::Cancel) {
        controller
            .first_await_state_changed
            .acquire()
            .await
            .expect("first await observes cancellation or is dropped")
            .forget();
    }
    controller.release_first_await.add_permits(1);

    await_task
        .await
        .expect("process-await task joins")
        .expect("process-await path completes");
    assert_eq!(
        controller.commands(),
        vec![RecordedProcessCommand::Await],
        "cancelled process awaits must emit exactly one journal command"
    );
}

#[tokio::test]
async fn already_cancelled_process_await_emits_exactly_one_await_effect() {
    assert_cancelled_process_await_emits_one_await(true).await;
}

#[tokio::test]
async fn mid_await_process_cancellation_emits_exactly_one_await_effect() {
    assert_cancelled_process_await_emits_one_await(false).await;
}

#[tokio::test]
async fn deleted_session_process_await_latches_typed_enclosing_effect_abort() {
    let context = fig790_process_await_context(Arc::new(DeletedSessionProcessEffectController));
    context.record_started_process("fig790-process");

    let reply = context
        .await_process_handle(
            "await-deleted-session-process".to_string(),
            RuntimeExecutionContext::process_handle_json("fig790-process"),
        )
        .await;

    assert!(!reply.output.is_success());
    let error = context
        .take_nested_effect_error()
        .expect("controller error must escape the fixed language-host reply boundary");
    assert_eq!(
        error.cause,
        Some(crate::RuntimeErrorCause::SessionDeleted {
            session_id: "fig790-session".to_string(),
        })
    );
}
