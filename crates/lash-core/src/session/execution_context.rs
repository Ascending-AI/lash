use std::sync::Arc;

use crate::facade_support::ScopedEffectControllerFacadeOps;
use tokio::sync::mpsc::Sender;
use tokio_util::sync::CancellationToken;

use crate::tool_dispatch::ToolDispatchContext;
use crate::{TurnActivity, TurnActivityId, TurnEvent};

#[derive(Clone)]
pub struct RuntimeExecutionContext<'run> {
    pub(super) session_id: String,
    pub(super) dispatch: Arc<ToolDispatchContext<'run>>,
    process_env_store: Arc<dyn crate::ProcessExecutionEnvStore>,
    attachment_store: Arc<crate::SessionAttachmentStore>,
    chronological_projection: Arc<crate::ChronologicalProjection>,
    protocol_extension: Option<crate::ProtocolTurnExtensionHandle>,
    turn_context: crate::TurnContext,
    execution_env_spec: crate::ProcessExecutionEnvSpec,
    process_originator: Option<crate::ProcessOriginator>,
    pub(super) runtime_process_id: Option<String>,
    pub(super) process_event_context: Option<RuntimeExecutionProcessEventContext>,
    process_env_ref: Option<crate::ProcessExecutionEnvRef>,
    process_wake_session_id: Option<String>,
    pub(super) parent_invocation: Option<crate::RuntimeInvocation>,
    turn_phase_probe: Option<Arc<dyn crate::runtime::RuntimeTurnPhaseProbe>>,
    pub(super) turn_event_tx: Option<Sender<TurnActivity>>,
    pub(super) cancellation_token: Option<CancellationToken>,
    pub(super) observe_turn_cancel: bool,
    /// Per-tool trace emission handle for this execution. Present only when the
    /// host installed a trace sink; `None` keeps every trace call a no-op.
    tracing: Option<RuntimeExecutionTracing>,
    /// Graph key of the enclosing code block, stamped onto the per-tool
    /// `TurnEvent`s emitted from this context so consumers can attribute a tool
    /// call to its code block without ordering heuristics. `None` when the
    /// context is not executing a code block.
    code_block_graph_key: Option<String>,
    /// Call id of the parent `batch` tool call when this context runs the
    /// children of a batch dispatch, stamped onto child `TurnEvent`s. `None`
    /// for top-level tool execution.
    batch_parent_call_id: Option<String>,
    /// Work-driver handle for this execution's process wiring, when the
    /// deployment provides one. Threaded through so in-run process
    /// operations (e.g. signalling another process) that build their own
    /// `RuntimeEffectLocalExecutor::processes(..)` call can hand it along
    /// instead of falling back to hub-less backoff polling.
    process_work_driver: Option<crate::ProcessWorkDriver>,
    /// Process ids started by THIS execution context. Possession of a handle
    /// the run itself created is sufficient capability to await/cancel it —
    /// run-local children do not require session observer edges (the ephemeral
    /// execution scope must never appear in durable grant state).
    started_process_ids: Arc<std::sync::Mutex<std::collections::HashSet<String>>>,
    /// Nested durable-controller failure captured while a language runtime
    /// owns the stack. Its fixed host-reply API must unwind before the
    /// enclosing code-execution effect can abort.
    nested_effect_error: Arc<std::sync::Mutex<Option<crate::RuntimeEffectControllerError>>>,
}

#[derive(Clone)]
pub(super) struct RuntimeExecutionProcessEventContext {
    pub process_id: String,
    pub execution_write_authority: crate::ProcessExecutionWriteAuthority,
    pub registry: Arc<dyn crate::ProcessRegistry>,
    pub awaiter: crate::ProcessAwaiter,
    pub store: Option<Arc<dyn crate::RuntimePersistence>>,
    pub session_store_factory: Option<Arc<dyn crate::SessionStoreFactory>>,
    pub queued_work_driver: Option<crate::QueuedWorkDriver>,
    pub clock: Arc<dyn crate::Clock>,
    pub wake_turn_policy: crate::WakeTurnPolicy,
}

/// Trace-sink handle threaded into tool execution so per-tool trace events are
/// emitted from the single shared seam, whichever protocol drives the turn.
///
/// `scope_context` carries the turn-scoped identity (session / turn / iteration)
/// so [`crate::trace::assign_span_identity`] stamps `tool:<call_id>` under the
/// right turn; `base_context` carries the host's run-level trace context.
#[derive(Clone)]
pub(crate) struct RuntimeExecutionTracing {
    sink: Arc<dyn lash_trace::TraceSink>,
    level: lash_trace::TraceLevel,
    base_context: lash_trace::TraceContext,
    scope_context: lash_trace::TraceContext,
}

impl RuntimeExecutionTracing {
    pub(crate) fn new(
        sink: Arc<dyn lash_trace::TraceSink>,
        level: lash_trace::TraceLevel,
        base_context: lash_trace::TraceContext,
        scope_context: lash_trace::TraceContext,
    ) -> Self {
        Self {
            sink,
            level,
            base_context,
            scope_context,
        }
    }

    fn emit(&self, event: lash_trace::TraceEvent, clock: &dyn crate::Clock) {
        crate::trace::emit_trace(
            &Some(Arc::clone(&self.sink)),
            &self.base_context,
            self.scope_context.clone(),
            event,
            clock,
        );
    }
}

impl<'run> RuntimeExecutionContext<'run> {
    pub(super) fn process_scope(
        &self,
        parent_invocation: Option<crate::RuntimeInvocation>,
    ) -> crate::ProcessOpScope<'_> {
        crate::ProcessOpScope::new(self.dispatch.effect_controller.scoped())
            .with_parent_invocation(parent_invocation)
            .with_agent_frame_id(Some(self.dispatch.agent_frame_id.clone()))
    }

    pub(super) fn record_started_process(&self, process_id: &str) {
        self.started_process_ids
            .lock()
            .expect("started process ids lock")
            .insert(process_id.to_string());
    }

    pub(crate) fn session_graph_service(&self) -> &dyn crate::plugin::SessionGraphService {
        self.dispatch.session_graph.as_ref()
    }

    #[cfg(test)]
    pub(crate) fn tool_argument_projection_policy(
        &self,
        name: &str,
    ) -> crate::ToolArgumentProjectionPolicy {
        crate::tool_dispatch::resolve_tool_argument_projection_policy(&self.dispatch, name)
    }

    #[allow(
        clippy::too_many_arguments,
        reason = "code execution bridge carries explicit per-turn runtime dependencies"
    )]
    pub(crate) fn new(
        session_id: String,
        dispatch: Arc<ToolDispatchContext<'run>>,
        process_env_store: Arc<dyn crate::ProcessExecutionEnvStore>,
        attachment_store: Arc<crate::SessionAttachmentStore>,
        chronological_projection: Arc<crate::ChronologicalProjection>,
        protocol_extension: Option<crate::ProtocolTurnExtensionHandle>,
        turn_context: crate::TurnContext,
    ) -> Self {
        Self {
            session_id,
            dispatch,
            process_env_store,
            attachment_store,
            chronological_projection,
            protocol_extension,
            turn_context,
            execution_env_spec: crate::ProcessExecutionEnvSpec::new(
                crate::PluginOptions::default(),
                crate::SessionPolicy::default(),
            ),
            process_originator: None,
            runtime_process_id: None,
            process_event_context: None,
            started_process_ids: Arc::default(),
            nested_effect_error: Arc::default(),
            process_env_ref: None,
            process_wake_session_id: None,
            parent_invocation: None,
            turn_phase_probe: None,
            turn_event_tx: None,
            cancellation_token: None,
            observe_turn_cancel: true,
            tracing: None,
            code_block_graph_key: None,
            batch_parent_call_id: None,
            process_work_driver: None,
        }
    }

    pub(crate) fn to_static(&self) -> Option<RuntimeExecutionContext<'static>> {
        Some(RuntimeExecutionContext {
            session_id: self.session_id.clone(),
            dispatch: Arc::new(self.dispatch.to_static()?),
            process_env_store: Arc::clone(&self.process_env_store),
            attachment_store: Arc::clone(&self.attachment_store),
            chronological_projection: Arc::clone(&self.chronological_projection),
            protocol_extension: self.protocol_extension.clone(),
            turn_context: self.turn_context.clone(),
            execution_env_spec: self.execution_env_spec.clone(),
            process_originator: self.process_originator.clone(),
            runtime_process_id: self.runtime_process_id.clone(),
            process_event_context: self.process_event_context.clone(),
            process_env_ref: self.process_env_ref.clone(),
            process_wake_session_id: self.process_wake_session_id.clone(),
            parent_invocation: self.parent_invocation.clone(),
            turn_phase_probe: self.turn_phase_probe.clone(),
            turn_event_tx: self.turn_event_tx.clone(),
            cancellation_token: self.cancellation_token.clone(),
            observe_turn_cancel: self.observe_turn_cancel,
            tracing: self.tracing.clone(),
            code_block_graph_key: self.code_block_graph_key.clone(),
            batch_parent_call_id: self.batch_parent_call_id.clone(),
            process_work_driver: self.process_work_driver.clone(),
            started_process_ids: Arc::clone(&self.started_process_ids),
            nested_effect_error: Arc::clone(&self.nested_effect_error),
        })
    }

    pub(crate) fn take_nested_effect_error(&self) -> Option<crate::RuntimeEffectControllerError> {
        self.nested_effect_error
            .lock()
            .expect("nested runtime effect error lock poisoned")
            .take()
    }

    /// Exposes execution scope id to protocol and process-engine implementors while executing code
    /// against the session runtime.
    pub fn execution_scope_id(&self) -> String {
        self.dispatch
            .effect_controller
            .scoped()
            .scope_id()
            .to_string()
    }

    /// Exposes session scope to protocol and process-engine implementors while executing code
    /// against the session runtime.
    pub fn session_scope(&self) -> crate::SessionScope {
        if self.dispatch.agent_frame_id.is_empty() {
            crate::SessionScope::new(self.session_id.clone())
        } else {
            crate::SessionScope::for_agent_frame(
                self.session_id.clone(),
                self.dispatch.agent_frame_id.clone(),
            )
        }
    }

    pub fn trigger_store(&self) -> Option<Arc<dyn crate::TriggerStore>> {
        self.dispatch
            .trigger_router
            .as_ref()
            .map(crate::TriggerRouter::store)
    }

    pub(super) async fn emit_turn_activity(
        &self,
        correlation_id: TurnActivityId,
        event: TurnEvent,
    ) {
        if let Some(tx) = &self.turn_event_tx {
            let _ = tx.send(TurnActivity::new(correlation_id, event)).await;
        }
    }

    pub(crate) fn with_turn_event_sender(mut self, turn_event_tx: Sender<TurnActivity>) -> Self {
        self.turn_event_tx = Some(turn_event_tx);
        self
    }

    pub(crate) fn with_tracing(mut self, tracing: Option<RuntimeExecutionTracing>) -> Self {
        self.tracing = tracing;
        self
    }

    pub(crate) fn replay_validation_trace(&self) -> Option<crate::RuntimeEffectReplayTrace> {
        let tracing = self.tracing.as_ref()?;
        crate::RuntimeEffectReplayTrace::gated(
            tracing.level,
            Some(&tracing.sink),
            tracing.base_context.clone(),
            tracing.scope_context.clone(),
            Arc::clone(&self.dispatch.clock),
        )
    }

    pub(crate) fn with_code_block_graph_key(mut self, graph_key: Option<String>) -> Self {
        self.code_block_graph_key = graph_key;
        self
    }

    pub(crate) fn with_batch_parent_call_id(mut self, parent_call_id: Option<String>) -> Self {
        self.batch_parent_call_id = parent_call_id;
        self
    }

    /// Graph key of the enclosing code block for tool calls run from this
    /// context, or `None` when no code block is executing.
    pub(super) fn code_block_graph_key(&self) -> Option<String> {
        self.code_block_graph_key.clone()
    }

    /// Parent batch call id for tool calls run from this context, or `None`
    /// when this context is not executing batch children.
    pub(super) fn batch_parent_call_id(&self) -> Option<String> {
        self.batch_parent_call_id.clone()
    }

    /// Emit a `ToolCallStarted` trace event for a tool run from this context.
    /// No-op when the host installed no trace sink.
    pub(super) fn emit_tool_call_started_trace(
        &self,
        call_id: &str,
        name: &str,
        args: &serde_json::Value,
    ) {
        if let Some(tracing) = self.tracing.as_ref() {
            tracing.emit(
                lash_trace::TraceEvent::ToolCallStarted {
                    call_id: Some(call_id.to_string()),
                    name: name.to_string(),
                    args: args.clone(),
                },
                self.dispatch.clock.as_ref(),
            );
        }
    }

    /// Emit a `ToolCallCompleted` trace event for a tool run from this context.
    /// No-op when the host installed no trace sink.
    pub(super) fn emit_tool_call_completed_trace(&self, record: &crate::ToolCallRecord) {
        if let Some(tracing) = self.tracing.as_ref() {
            tracing.emit(
                lash_trace::TraceEvent::ToolCallCompleted {
                    call_id: record.call_id.clone(),
                    name: record.tool.clone(),
                    args: record.args.clone(),
                    output: crate::trace::trace_tool_call_output(&record.output),
                    duration_ms: record.duration_ms,
                },
                self.dispatch.clock.as_ref(),
            );
        }
    }

    pub(crate) fn with_parent_invocation(mut self, metadata: crate::RuntimeInvocation) -> Self {
        self.parent_invocation = Some(metadata);
        self
    }

    pub(crate) fn with_execution_env_spec(
        mut self,
        execution_env_spec: crate::ProcessExecutionEnvSpec,
    ) -> Self {
        self.execution_env_spec = execution_env_spec;
        self
    }

    pub(crate) fn with_process_registration_context(
        mut self,
        registration: &crate::ProcessRegistration,
    ) -> Self {
        self.process_originator = Some(registration.provenance.originator.clone());
        self.runtime_process_id = Some(registration.id.clone());
        self.process_env_ref = registration.env_ref.clone();
        self.process_wake_session_id = registration.wake_session_id.clone();
        self
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) fn with_process_event_context(
        mut self,
        process_id: impl Into<String>,
        execution_write_authority: crate::ProcessExecutionWriteAuthority,
        registry: Arc<dyn crate::ProcessRegistry>,
        awaiter: crate::ProcessAwaiter,
        store: Option<Arc<dyn crate::RuntimePersistence>>,
        session_store_factory: Option<Arc<dyn crate::SessionStoreFactory>>,
        queued_work_driver: Option<crate::QueuedWorkDriver>,
        clock: Arc<dyn crate::Clock>,
        wake_turn_policy: crate::WakeTurnPolicy,
    ) -> Self {
        self.process_event_context = Some(RuntimeExecutionProcessEventContext {
            process_id: process_id.into(),
            execution_write_authority,
            registry,
            awaiter,
            store,
            session_store_factory,
            queued_work_driver,
            clock,
            wake_turn_policy,
        });
        self
    }

    pub(crate) fn with_turn_phase_probe(
        mut self,
        probe: Option<Arc<dyn crate::runtime::RuntimeTurnPhaseProbe>>,
    ) -> Self {
        self.turn_phase_probe = probe;
        self
    }

    #[doc(hidden)]
    pub fn named_phase(&self, phase: &'static str) -> crate::runtime::RuntimeNamedPhase {
        crate::runtime::RuntimeNamedPhase::begin(self.turn_phase_probe.clone(), phase)
    }

    pub(crate) fn with_cancellation_token(mut self, cancellation_token: CancellationToken) -> Self {
        self.cancellation_token = Some(cancellation_token);
        self
    }

    pub(crate) fn without_turn_cancel_observation(mut self) -> Self {
        self.observe_turn_cancel = false;
        self
    }

    pub(crate) fn with_process_work_driver(
        mut self,
        process_work_driver: Option<crate::ProcessWorkDriver>,
    ) -> Self {
        self.process_work_driver = process_work_driver;
        self
    }

    pub(crate) fn record_nested_effect_error(&self, error: crate::RuntimeEffectControllerError) {
        let mut pending = self
            .nested_effect_error
            .lock()
            .expect("nested runtime effect error lock poisoned");
        pending.get_or_insert(error);
    }

    pub fn attachment_store(&self) -> Arc<crate::SessionAttachmentStore> {
        Arc::clone(&self.attachment_store)
    }

    pub(crate) fn is_run_local_process(&self, process_id: &str) -> bool {
        self.started_process_ids
            .lock()
            .expect("started process ids lock")
            .contains(process_id)
    }

    pub(crate) fn process_spawn_provenance(&self) -> Option<crate::ProcessSpawnProvenance> {
        self.process_originator
            .clone()
            .map(|originator| crate::ProcessSpawnProvenance {
                originator,
                wake_session_id: self.process_wake_session_id.clone(),
            })
    }

    pub(crate) async fn attach_captured_process_execution_env(
        &self,
        registration: crate::ProcessRegistration,
    ) -> Result<crate::ProcessRegistration, crate::PluginError> {
        if registration.env_ref.is_some() {
            return Ok(registration);
        }
        match registration.input.as_ref() {
            crate::ProcessInput::ToolCall { .. } | crate::ProcessInput::Engine { .. } => {
                let env_ref = self.captured_process_execution_env_ref().await?;
                Ok(registration.with_execution_env_ref(Some(env_ref)))
            }
            crate::ProcessInput::External { .. } | crate::ProcessInput::SessionTurn { .. } => {
                Ok(registration)
            }
        }
    }

    pub async fn captured_process_execution_env_ref(
        &self,
    ) -> Result<crate::ProcessExecutionEnvRef, crate::PluginError> {
        if let Some(env_ref) = self.process_env_ref.clone() {
            return Ok(env_ref);
        }
        crate::persist_process_execution_env(
            self.process_env_store.as_ref(),
            &self.execution_env_spec,
        )
        .await
    }

    pub async fn start_child_process(
        &self,
        registration: crate::ProcessRegistration,
        _kind: impl Into<String>,
        _label: Option<String>,
    ) -> crate::ToolInvocationReply {
        let _phase = self.named_phase("process.start_child");
        let registration = match self
            .attach_captured_process_execution_env(registration)
            .await
        {
            Ok(registration) => registration,
            Err(err) => {
                return crate::ToolInvocationReply::error(serde_json::json!(err.to_string()));
            }
        };
        let process_id = registration.id.clone();
        let mut options =
            crate::ProcessStartOptions::new().with_initial_observer(self.session_id.clone());
        if let Some(spawn) = self.process_spawn_provenance() {
            options = options.with_spawn_provenance(spawn);
        }
        match self
            .dispatch
            .processes
            .start(
                &self.session_id,
                registration,
                options,
                self.process_scope(self.parent_invocation.clone()),
            )
            .await
        {
            Ok(_) => {
                self.record_started_process(&process_id);
                crate::ToolInvocationReply::success(Self::process_handle_json(&process_id))
            }
            Err(err) => crate::ToolInvocationReply::error(serde_json::json!(err.to_string())),
        }
    }

    pub async fn append_process_event(
        &self,
        request: crate::ProcessEventAppendRequest,
    ) -> Result<crate::ProcessEvent, crate::PluginError> {
        let context = self.process_event_context.as_ref().ok_or_else(|| {
            crate::PluginError::Session(
                "process event emission is unavailable outside a durable process execution"
                    .to_string(),
            )
        })?;
        let result = context
            .registry
            .append_event_with_authority(
                &context.process_id,
                request,
                &context.execution_write_authority,
            )
            .await?;
        crate::tool_provider::process_events::enqueue_wake_delivery(
            std::sync::Arc::clone(&context.registry),
            context.store.clone(),
            context.session_store_factory.as_ref(),
            result.wake_delivery,
            Some(self.session_graph_service()),
            context.queued_work_driver.as_ref(),
            Arc::clone(&context.clock),
            &context.wake_turn_policy,
        )
        .await?;
        Ok(result.event)
    }

    pub async fn await_process_signal_event(
        &self,
        process_id: &str,
        signal_name: &str,
        event_ordinal: u64,
    ) -> Result<serde_json::Value, crate::RuntimeEffectControllerError> {
        let cancellation = self.cancellation_token.clone().unwrap_or_default();
        let key = self
            .dispatch
            .effect_controller
            .controller()
            .await_event_key(
                &crate::ExecutionScope::process(process_id),
                crate::AwaitEventWaitIdentity::process_signal(
                    process_id,
                    signal_name,
                    event_ordinal,
                ),
            )
            .await?;
        let invocation = crate::runtime::causal::process_await_event_invocation(
            &self.session_id,
            self.parent_invocation.as_ref(),
            process_id,
            signal_name,
            event_ordinal,
        );
        let outcome = self
            .dispatch
            .effect_controller
            .controller()
            .execute_effect(
                crate::RuntimeEffectEnvelope::new(
                    invocation,
                    crate::RuntimeEffectCommand::AwaitEvent { key },
                ),
                crate::RuntimeEffectLocalExecutor::await_event_with_clock(
                    cancellation,
                    None,
                    std::sync::Arc::clone(&self.dispatch.clock),
                )
                .with_turn_cancel_observation(self.observe_turn_cancel)
                .with_turn_cancel_scope(
                    self.dispatch
                        .effect_controller
                        .scoped()
                        .execution_scope()
                        .clone(),
                ),
            )
            .await?;
        match outcome.into_await_event()? {
            crate::Resolution::Ok(value) => Ok(value),
            crate::Resolution::Err(err) => Err(crate::RuntimeEffectControllerError::new(
                err.code,
                err.message,
            )),
            crate::Resolution::Timeout => Err(crate::RuntimeEffectControllerError::new(
                "process_signal_wait_timeout",
                "process signal wait timed out",
            )),
            crate::Resolution::Cancelled => Err(crate::RuntimeEffectControllerError::new(
                "process_signal_wait_cancelled",
                "process signal wait was cancelled",
            )),
        }
    }

    /// Exposes callable tool manifest by id to protocol and process-engine implementors while
    /// executing code against the session runtime. Returns `None` when no callable tool manifest by
    /// id is present.
    pub fn callable_tool_manifest_by_id(&self, id: &crate::ToolId) -> Option<crate::ToolManifest> {
        crate::tool_dispatch::resolve_callable_manifest_by_id(&self.dispatch, id)
    }

    pub async fn signal_process_by_id(
        &self,
        process_id: &str,
        signal_name: &str,
        signal_id: String,
        payload: serde_json::Value,
    ) -> Result<crate::ProcessEvent, crate::RuntimeEffectControllerError> {
        let registry = self
            .process_event_context
            .as_ref()
            .map(|context| Arc::clone(&context.registry))
            .ok_or_else(|| {
                crate::RuntimeEffectControllerError::new(
                    "process_registry_unavailable",
                    "process signalling is unavailable outside a durable process execution",
                )
            })?;
        let event_type = crate::process_signal_event_type(signal_name)?;
        let replay_key = format!("process:{process_id}:signal.{signal_name}:{signal_id}");
        let signal_payload = payload.clone();
        let command = crate::ProcessCommand::Signal {
            process_id: process_id.to_string(),
            signal_name: signal_name.to_string(),
            signal_id,
            request: crate::ProcessEventAppendRequest::new(event_type.clone(), payload)
                .with_replay_key(replay_key),
        };
        let effect_id = command.effect_id();
        let invocation = crate::runtime::causal::process_effect_invocation(
            &self.session_id,
            self.parent_invocation.clone(),
            &effect_id,
        );
        let outcome = self
            .dispatch
            .effect_controller
            .controller()
            .execute_effect(
                crate::RuntimeEffectEnvelope::new(
                    invocation,
                    crate::RuntimeEffectCommand::process(command),
                ),
                crate::RuntimeEffectLocalExecutor::processes(
                    Arc::clone(&registry),
                    self.process_work_driver.clone(),
                ),
            )
            .await?;
        match outcome.into_process()? {
            crate::ProcessEffectOutcome::Signal { event } => {
                let waiting_ordinal =
                    registry
                        .get_process(process_id)
                        .await?
                        .and_then(|record| match record.wait {
                            Some(crate::WaitState {
                                kind:
                                    crate::WaitKind::Signal {
                                        name,
                                        event_type: wait_event_type,
                                        ordinal,
                                        ..
                                    },
                                ..
                            }) if name == signal_name && wait_event_type == event_type => {
                                Some(ordinal)
                            }
                            _ => None,
                        });
                let ordinal = match waiting_ordinal {
                    Some(ordinal) => ordinal,
                    None => {
                        registry
                            .count_events_through(process_id, &event_type, event.sequence)
                            .await?
                    }
                };
                if ordinal > 0 {
                    let key = self
                        .dispatch
                        .effect_controller
                        .controller()
                        .await_event_key(
                            &crate::ExecutionScope::process(process_id),
                            crate::AwaitEventWaitIdentity::process_signal(
                                process_id,
                                signal_name,
                                ordinal,
                            ),
                        )
                        .await?;
                    let _ = self
                        .dispatch
                        .effect_controller
                        .controller()
                        .resolve_await_event(&key, crate::Resolution::Ok(signal_payload))
                        .await?;
                }
                Ok(*event)
            }
            other => Err(crate::RuntimeEffectControllerError::new(
                "runtime_effect_wrong_outcome",
                format!("expected signal outcome, got {other:?}"),
            )),
        }
    }

    pub async fn sleep_process(
        &self,
        scope: &str,
        sequence: u64,
        duration_ms: u64,
    ) -> Result<(), crate::RuntimeEffectControllerError> {
        let cancellation = self.cancellation_token.clone().unwrap_or_default();
        let invocation = crate::runtime::causal::process_sleep_invocation(
            &self.session_id,
            self.parent_invocation.as_ref(),
            scope,
            sequence,
        );
        let outcome = self
            .dispatch
            .effect_controller
            .controller()
            .execute_effect(
                crate::RuntimeEffectEnvelope::new(
                    invocation,
                    crate::RuntimeEffectCommand::Sleep { duration_ms },
                ),
                crate::RuntimeEffectLocalExecutor::sleep_with_clock(
                    cancellation,
                    std::sync::Arc::clone(&self.dispatch.clock),
                )
                .with_turn_cancel_observation(self.observe_turn_cancel)
                .with_turn_cancel_scope(
                    self.dispatch
                        .effect_controller
                        .scoped()
                        .execution_scope()
                        .clone(),
                ),
            )
            .await?;
        match outcome {
            crate::RuntimeEffectOutcome::Sleep => Ok(()),
            other => Err(crate::RuntimeEffectControllerError::new(
                "runtime_effect_wrong_outcome",
                format!("expected sleep outcome, got {}", other.kind().as_str()),
            )),
        }
    }

    /// Exposes chronological projection to protocol and process-engine implementors while executing
    /// code against the session runtime.
    pub fn chronological_projection(&self) -> Arc<crate::ChronologicalProjection> {
        Arc::clone(&self.chronological_projection)
    }

    pub async fn execute_trigger_effect(
        &self,
        effect_id: String,
        command: crate::TriggerCommand,
    ) -> Result<crate::TriggerEffectResult, crate::RuntimeEffectControllerError> {
        let store = self.trigger_store().ok_or_else(|| {
            crate::RuntimeEffectControllerError::new(
                "trigger_store_unavailable",
                "trigger store is unavailable in this runtime",
            )
        })?;
        let scope = self
            .parent_invocation
            .as_ref()
            .map(|invocation| invocation.scope.clone())
            .unwrap_or_else(|| crate::RuntimeScope::new(self.session_id.clone()));
        let invocation = crate::RuntimeInvocation::effect(
            scope,
            effect_id.clone(),
            crate::RuntimeEffectKind::Trigger,
            effect_id,
        )
        .with_caused_by(
            self.parent_invocation
                .as_ref()
                .and_then(crate::RuntimeInvocation::causal_ref),
        );
        self.dispatch
            .effect_controller
            .controller()
            .execute_effect(
                crate::RuntimeEffectEnvelope::new(
                    invocation,
                    crate::RuntimeEffectCommand::Trigger {
                        command: Box::new(command),
                    },
                ),
                crate::RuntimeEffectLocalExecutor::triggers(store),
            )
            .await?
            .into_trigger()
    }

    /// Exposes parent invocation to protocol and process-engine implementors while executing code
    /// against the session runtime. Returns `None` when no parent invocation is present.
    pub fn parent_invocation(&self) -> Option<&crate::RuntimeInvocation> {
        self.parent_invocation.as_ref()
    }

    /// Exposes session id to protocol and process-engine implementors while executing code against
    /// the session runtime.
    pub fn session_id(&self) -> &str {
        &self.session_id
    }

    /// Exposes tool catalog to protocol and process-engine implementors while executing code
    /// against the session runtime.
    pub fn tool_catalog(&self) -> Arc<crate::ToolCatalog> {
        Arc::clone(&self.dispatch.tool_catalog)
    }

    pub fn trigger_actor(&self) -> crate::ProcessOriginator {
        self.process_originator
            .clone()
            .unwrap_or_else(|| crate::ProcessOriginator::session(self.session_scope()))
    }

    pub fn trigger_owner_scope(&self) -> Result<crate::TriggerOwnerScope, crate::PluginError> {
        resolve_trigger_owner_scope(&self.session_id, self.process_originator.as_ref())
    }

    pub fn trigger_registration_wake_target(&self) -> Option<crate::SessionScope> {
        self.process_wake_session_id
            .as_ref()
            .map(crate::SessionScope::new)
            .or_else(|| Some(self.session_scope()))
    }

    /// Exposes turn context to protocol and process-engine implementors while executing code
    /// against the session runtime.
    pub fn turn_context(&self) -> &crate::TurnContext {
        &self.turn_context
    }
}

fn resolve_trigger_owner_scope(
    root_session_id: &str,
    originator: Option<&crate::ProcessOriginator>,
) -> Result<crate::TriggerOwnerScope, crate::PluginError> {
    match originator {
        Some(crate::ProcessOriginator::Host {
            scope: Some(binding_id),
        }) => crate::TriggerOwnerScope::host(binding_id.clone()),
        Some(crate::ProcessOriginator::Host { scope: None }) => Err(crate::PluginError::Session(
            "bare host authority cannot own user trigger subscriptions; use an explicit host binding"
                .to_string(),
        )),
        Some(crate::ProcessOriginator::Session { session_id }) => {
            Ok(crate::TriggerOwnerScope::session(session_id.clone()))
        }
        None => Ok(crate::TriggerOwnerScope::session(root_session_id)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tool_dispatch::ToolDispatchContext;
    use crate::{ToolCall, ToolProvider, ToolResult};

    struct NoopTools;

    #[test]
    fn trigger_owner_scope_uses_root_session_or_explicit_host_binding() {
        assert_eq!(
            resolve_trigger_owner_scope("root-session", None).unwrap(),
            crate::TriggerOwnerScope::session("root-session")
        );
        let root = crate::ProcessOriginator::session(crate::SessionScope::new("root-session"));
        assert_eq!(
            resolve_trigger_owner_scope("ignored", Some(&root)).unwrap(),
            crate::TriggerOwnerScope::session("root-session")
        );
        let frame = crate::ProcessOriginator::session(crate::SessionScope::for_agent_frame(
            "root-session",
            "agent-frame",
        ));
        assert_eq!(
            resolve_trigger_owner_scope("ignored", Some(&frame)).unwrap(),
            crate::TriggerOwnerScope::session("root-session"),
            "agent frames inherit the root session namespace"
        );
        let named_host = crate::ProcessOriginator::host_scoped("automation-a");
        assert_eq!(
            resolve_trigger_owner_scope("ignored", Some(&named_host)).unwrap(),
            crate::TriggerOwnerScope::host("automation-a").unwrap()
        );
        assert!(
            resolve_trigger_owner_scope("ignored", Some(&crate::ProcessOriginator::host()))
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

        async fn execute(&self, _call: ToolCall<'_>) -> ToolResult {
            ToolResult::err_fmt("not used")
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
            .build_session("session", None)
            .expect("plugin session");
        let (event_tx, _event_rx) = tokio::sync::mpsc::channel(1);
        let dispatch = Arc::new(ToolDispatchContext {
            plugins,
            tools: Arc::new(NoopTools),
            tool_catalog: Arc::new(crate::ToolCatalog::from_tools(
                vec![tool.manifest()],
                std::collections::BTreeMap::new(),
            )),
            sessions: Arc::new(crate::testing::MockSessionManager::default()),
            session_lifecycle: Arc::new(crate::testing::MockSessionManager::default()),
            session_graph: Arc::new(crate::testing::MockSessionManager::default()),
            processes: Arc::new(crate::UnavailableProcessService),
            trigger_router: None,
            effect_controller: crate::runtime::RuntimeEffectControllerHandle::shared(Arc::new(
                crate::InlineRuntimeEffectController::default(),
            )),
            direct_completions: crate::DirectCompletionClient::unavailable(
                "direct completions are unavailable in this test context",
            ),
            parent_invocation: None,
            execution_env_spec: crate::ProcessExecutionEnvSpec::new(
                crate::PluginOptions::default(),
                crate::SessionPolicy::default(),
            ),
            session_id: "session".to_string(),
            agent_frame_id: String::new(),
            event_tx,
            checkpoint_messages: crate::tool_dispatch::CheckpointMessageBuffer::default(),
            trigger_outcomes: crate::tool_dispatch::ToolTriggerOutcomeBuffer::default(),
            attachment_store: Arc::new(crate::SessionAttachmentStore::in_memory()),
            attachment_source_policy: Arc::new(crate::OpenAttachmentSourcePolicy),
            turn_context: crate::TurnContext::default(),
            clock: std::sync::Arc::new(crate::SystemClock),
        });
        let ctx = RuntimeExecutionContext::new(
            "session".to_string(),
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
}
