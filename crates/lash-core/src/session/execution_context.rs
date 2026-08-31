use lash_sansio::sync::MutexExt;
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
    process_execution: Option<RuntimeProcessExecution>,
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
    process_work: Option<crate::ProcessWorkWiring>,
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
pub(crate) struct RuntimeProcessExecution {
    pub process_id: String,
    pub originator: crate::ProcessOriginator,
    pub env_ref: Option<crate::ProcessExecutionEnvRef>,
    pub wake_session_id: Option<String>,
    pub event_context: Option<RuntimeExecutionProcessEventContext>,
}

#[derive(Clone)]
pub(crate) struct RuntimeExecutionProcessEventContext {
    pub execution_write_authority: crate::ProcessExecutionWriteAuthority,
    pub process_work: crate::ProcessWorkWiring,
    pub store: Option<Arc<dyn crate::RuntimePersistence>>,
    pub session_store_factory: Option<Arc<dyn crate::SessionStoreFactory>>,
    pub queued_work: Arc<dyn crate::QueuedWorkSubstrate>,
    pub process_wake_delivery_policy: crate::DeliveryPolicy,
    pub clock: Arc<dyn crate::Clock>,
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
    base_context: lash_trace::TraceContext,
    scope_context: lash_trace::TraceContext,
}

impl RuntimeExecutionTracing {
    pub(crate) fn new(
        sink: Arc<dyn lash_trace::TraceSink>,
        base_context: lash_trace::TraceContext,
        scope_context: lash_trace::TraceContext,
    ) -> Self {
        Self {
            sink,
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

    pub(crate) fn emit_tool_call_completed(
        &self,
        record: &crate::ToolCallRecord,
        attempts: &[lash_trace::TraceRetryAttempt],
        clock: &dyn crate::Clock,
    ) {
        self.emit(
            lash_trace::TraceEvent::ToolCallCompleted {
                call_id: record.call_id.clone(),
                name: record.tool.clone(),
                args: record.args.clone(),
                output: crate::trace::trace_tool_call_output(&record.output),
                duration_ms: record.duration_ms,
                attempts: (!attempts.is_empty()).then(|| attempts.to_vec()),
            },
            clock,
        );
    }
}

impl<'run> RuntimeExecutionContext<'run> {
    pub(crate) async fn finish_parent_end_actions(&self) -> Result<(), crate::PluginError> {
        crate::tool_dispatch::execute_parent_end_actions(self.dispatch.as_ref()).await
    }

    /// Restore crash-retained teardown actions for protocol and process-engine implementors.
    pub fn restore_parent_end_actions(&self, actions: &[crate::ToolIntentParentEndAction]) {
        self.dispatch.recorded_intent_outcomes.restore(actions);
    }

    /// Snapshot teardown actions for protocol and process-engine implementors before persistence.
    pub fn parent_end_actions(&self) -> Vec<crate::ToolIntentParentEndAction> {
        self.dispatch.recorded_intent_outcomes.snapshot()
    }

    /// Restore run-local child possession for a resumed process-engine segment.
    pub fn restore_started_process_ids(&self, process_ids: &[String]) {
        self.started_process_ids
            .lock_recover()
            .extend(process_ids.iter().cloned());
    }

    /// Snapshot run-local child possession before a process-engine segment handover.
    pub fn started_process_ids(&self) -> Vec<String> {
        let mut process_ids = self
            .started_process_ids
            .lock_recover()
            .iter()
            .cloned()
            .collect::<Vec<_>>();
        process_ids.sort_unstable();
        process_ids
    }

    /// Executes a nondeterministic language-runtime operation behind the
    /// durable effect controller so replay returns the recorded sample.
    pub async fn journaled_language_runtime_value(
        &self,
        effect_id: String,
        operation: String,
    ) -> Result<serde_json::Value, crate::RuntimeEffectControllerError> {
        let scope = self
            .parent_invocation
            .as_ref()
            .map(|invocation| invocation.scope.clone())
            .unwrap_or_else(|| crate::RuntimeScope::new(self.session_id.clone()));
        let invocation = crate::RuntimeInvocation::effect(
            scope,
            effect_id.clone(),
            crate::RuntimeEffectKind::LanguageRuntimeValue,
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
                    crate::RuntimeEffectCommand::LanguageRuntimeValue { operation },
                ),
                crate::RuntimeEffectLocalExecutor::language_runtime_value(Arc::clone(
                    &self.dispatch.clock,
                )),
            )
            .await?
            .into_language_runtime_value()
    }
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
            .lock_recover()
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
                crate::SessionPolicy::new(crate::TurnBudget::Unbounded),
            ),
            process_execution: None,
            started_process_ids: Arc::default(),
            nested_effect_error: Arc::default(),
            parent_invocation: None,
            turn_phase_probe: None,
            turn_event_tx: None,
            cancellation_token: None,
            observe_turn_cancel: true,
            tracing: None,
            code_block_graph_key: None,
            batch_parent_call_id: None,
            process_work: None,
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
            process_execution: self.process_execution.clone(),
            parent_invocation: self.parent_invocation.clone(),
            turn_phase_probe: self.turn_phase_probe.clone(),
            turn_event_tx: self.turn_event_tx.clone(),
            cancellation_token: self.cancellation_token.clone(),
            observe_turn_cancel: self.observe_turn_cancel,
            tracing: self.tracing.clone(),
            code_block_graph_key: self.code_block_graph_key.clone(),
            batch_parent_call_id: self.batch_parent_call_id.clone(),
            process_work: self.process_work.clone(),
            started_process_ids: Arc::clone(&self.started_process_ids),
            nested_effect_error: Arc::clone(&self.nested_effect_error),
        })
    }

    pub(crate) fn take_nested_effect_error(&self) -> Option<crate::RuntimeEffectControllerError> {
        self.nested_effect_error.lock_recover().take()
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

    /// Exposes trigger store to protocol and process-engine implementors while executing code
    /// against the session runtime. Returns `None` when no trigger store is present.
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
        crate::RuntimeEffectReplayTrace::for_divergence(
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
    pub(super) fn emit_tool_call_completed_trace(
        &self,
        record: &crate::ToolCallRecord,
        attempts: &[lash_trace::TraceRetryAttempt],
    ) {
        if let Some(tracing) = self.tracing.as_ref() {
            tracing.emit_tool_call_completed(record, attempts, self.dispatch.clock.as_ref());
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

    pub(crate) fn with_process_execution(
        mut self,
        registration: &crate::ProcessRegistration,
        event_context: impl Into<Option<RuntimeExecutionProcessEventContext>>,
    ) -> Self {
        self.process_execution = Some(RuntimeProcessExecution {
            process_id: registration.id.clone(),
            originator: registration.provenance.originator.clone(),
            env_ref: registration.env_ref.clone(),
            wake_session_id: registration.wake_session_id.clone(),
            event_context: event_context.into(),
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

    pub(crate) fn attempt_phase_probe(
        &self,
    ) -> Option<Arc<dyn crate::runtime::RuntimeTurnPhaseProbe>> {
        self.turn_phase_probe.clone()
    }

    pub(crate) fn with_cancellation_token(mut self, cancellation_token: CancellationToken) -> Self {
        self.cancellation_token = Some(cancellation_token);
        self
    }

    pub(crate) fn without_turn_cancel_observation(mut self) -> Self {
        self.observe_turn_cancel = false;
        self
    }

    /// The complete turn-cancel trio for one wait built from this execution:
    /// the token the wait races against, whether this execution observes turn
    /// cancellation, and the execution scope its turn-cancel gate registers
    /// under.
    ///
    /// Every wait this execution builds takes the trio whole. A wait that took
    /// only the scope inherited the executor's observing default and attached
    /// the turn-cancel gate an execution built with
    /// `without_turn_cancel_observation` had switched off.
    pub(crate) fn turn_cancel_wait(
        &self,
        cancellation: CancellationToken,
    ) -> crate::runtime::TurnCancelWait {
        if self.observe_turn_cancel {
            self.dispatch
                .effect_controller
                .scoped()
                .turn_cancel_wait(cancellation)
        } else {
            crate::runtime::TurnCancelWait::unobserved(cancellation)
        }
    }

    pub(crate) fn with_process_work(
        mut self,
        process_work: Option<crate::ProcessWorkWiring>,
    ) -> Self {
        self.process_work = process_work;
        self
    }

    pub(crate) fn record_nested_effect_error(&self, error: crate::RuntimeEffectControllerError) {
        let mut pending = self.nested_effect_error.lock_recover();
        pending.get_or_insert(error);
    }

    /// Shares the session-scoped attachment store with code-executor implementors so code-produced
    /// artifacts follow the same durable ownership contract as turn input.
    pub fn attachment_store(&self) -> Arc<crate::SessionAttachmentStore> {
        Arc::clone(&self.attachment_store)
    }

    /// Reports whether the execution scope has been cancelled by its host.
    ///
    /// Language-runtime bridges use this cooperative probe to turn a host Stop
    /// into their typed terminal instead of reporting it as a guest-program
    /// failure.
    pub fn is_cancelled(&self) -> bool {
        self.cancellation_token
            .as_ref()
            .is_some_and(CancellationToken::is_cancelled)
    }

    pub(super) fn process_id(&self) -> Option<&str> {
        self.process_execution
            .as_ref()
            .map(|exec| exec.process_id.as_str())
    }

    pub(super) fn process_event_context(&self) -> Option<&RuntimeExecutionProcessEventContext> {
        self.process_execution
            .as_ref()
            .and_then(|exec| exec.event_context.as_ref())
    }

    pub(crate) fn is_run_local_process(&self, process_id: &str) -> bool {
        self.started_process_ids.lock_recover().contains(process_id)
    }

    pub(crate) fn process_spawn_provenance(&self) -> Option<crate::ProcessSpawnProvenance> {
        self.process_execution
            .as_ref()
            .map(|exec| crate::ProcessSpawnProvenance {
                originator: exec.originator.clone(),
                wake_session_id: exec.wake_session_id.clone(),
            })
    }

    fn child_process_observers(&self) -> Vec<crate::SessionId> {
        match self.process_execution.as_ref() {
            Some(exec) => match &exec.originator {
                crate::ProcessOriginator::Host { .. } => Vec::new(),
                crate::ProcessOriginator::Session { session_id, .. } => vec![session_id.clone()],
            },
            None => vec![self.session_id.clone()],
        }
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

    /// Exposes captured process execution env ref to protocol and process-engine implementors while
    /// executing code against the session runtime.
    pub async fn captured_process_execution_env_ref(
        &self,
    ) -> Result<crate::ProcessExecutionEnvRef, crate::PluginError> {
        if let Some(env_ref) = self
            .process_execution
            .as_ref()
            .and_then(|exec| exec.env_ref.clone())
        {
            return Ok(env_ref);
        }
        crate::persist_process_execution_env(
            self.process_env_store.as_ref(),
            &self.execution_env_spec,
        )
        .await
    }

    /// Starts a child process for code-executor implementors with the current execution context as
    /// causal provenance.
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
        let mut options = crate::ProcessStartOptions::new()
            .with_initial_observers(self.child_process_observers());
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

    /// Appends one replay-scoped process event for code-executor implementors and returns the
    /// store-assigned sequence and any coordinated wake delivery.
    pub async fn append_process_event(
        &self,
        request: crate::ProcessEventAppendRequest,
    ) -> Result<crate::ProcessEvent, crate::PluginError> {
        let exec = self
            .process_execution
            .as_ref()
            .ok_or_else(missing_process_execution_error)?;
        let context = exec
            .event_context
            .as_ref()
            .ok_or_else(missing_process_execution_error)?;
        let result = context
            .process_work
            .registry()
            .append_event_with_authority(
                &exec.process_id,
                request,
                &context.execution_write_authority,
            )
            .await?;
        crate::tool_provider::process_events::enqueue_wake_delivery(
            std::sync::Arc::clone(context.process_work.registry()),
            context.store.clone(),
            context.session_store_factory.as_ref(),
            result.wake_delivery,
            Some(self.session_graph_service()),
            Arc::clone(&context.queued_work),
            context.process_wake_delivery_policy,
            Arc::clone(&context.clock),
        )
        .await?;
        Ok(result.event)
    }

    /// Waits for one named process signal for code-executor implementors through the durable
    /// await-event seam rather than polling the registry.
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
                crate::RuntimeEffectLocalExecutor::await_event_under(
                    &self.turn_cancel_wait(cancellation),
                    None,
                    std::sync::Arc::clone(&self.dispatch.clock),
                ),
            )
            .await?;
        match outcome.into_await_event()? {
            crate::Resolution::Ok(value) => Ok(value),
            crate::Resolution::Err(err) => Err(crate::RuntimeEffectControllerError::new(
                crate::RuntimeErrorCode::from_wire_code(&err.code),
                err.message,
            )),
            crate::Resolution::Timeout => Err(crate::RuntimeEffectControllerError::new(
                crate::RuntimeErrorCode::ProcessSignalWaitTimeout,
                "process signal wait timed out",
            )),
            crate::Resolution::Cancelled => Err(crate::RuntimeEffectControllerError::new(
                crate::RuntimeErrorCode::ProcessSignalWaitCancelled,
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

    /// Appends one named, replay-scoped signal to a process for code-executor implementors.
    pub async fn signal_process_by_id(
        &self,
        process_id: &str,
        signal_name: &str,
        signal_id: String,
        payload: serde_json::Value,
    ) -> Result<crate::ProcessEvent, crate::RuntimeEffectControllerError> {
        let registry = self
            .process_execution
            .as_ref()
            .and_then(|exec| exec.event_context.as_ref())
            .map(|context| Arc::clone(context.process_work.registry()))
            .ok_or_else(missing_process_execution_error)?;
        let event_type = crate::process_signal_event_type(signal_name)?;
        let replay_key = format!("process:{process_id}:signal.{signal_name}:{signal_id}");
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
        let controller = self.dispatch.effect_controller.controller();
        let scoped = self.dispatch.effect_controller.scoped();
        let (owned_controller, task_requests): (
            Arc<dyn crate::RuntimeEffectController>,
            Option<
                tokio::sync::mpsc::UnboundedReceiver<
                    crate::runtime::effect::EffectControllerTaskRequest,
                >,
            >,
        ) = if let Some(owned) = scoped.owned_controller() {
            (owned, None)
        } else {
            let (proxy, requests) = crate::runtime::effect::EffectTaskController::scoped(
                controller,
                scoped.execution_scope().clone(),
            )?;
            (
                proxy
                    .owned_controller()
                    .expect("effect-task proxy owns its controller"),
                Some(requests),
            )
        };
        let envelope = crate::RuntimeEffectEnvelope::new(
            invocation,
            crate::RuntimeEffectCommand::process(command),
        );
        let local_executor = crate::RuntimeEffectLocalExecutor::processes(
            Arc::clone(&registry),
            self.process_work
                .as_ref()
                .map(|work| Arc::clone(work.port()))
                .ok_or_else(|| {
                    crate::RuntimeEffectControllerError::foreign(
                        "process_work_unavailable",
                        "process execution has no process-work port",
                    )
                })?,
        )
        .with_process_effect_controller(owned_controller);
        let outcome = if let Some(task_requests) = task_requests {
            crate::runtime::effect::drive_effect_controller_task(
                controller,
                envelope,
                local_executor,
                task_requests,
            )
            .await?
        } else {
            controller.execute_effect(envelope, local_executor).await?
        };
        match outcome.into_process()? {
            crate::ProcessEffectOutcome::Signal { event } => Ok(*event),
            other => Err(crate::RuntimeEffectControllerError::new(
                crate::RuntimeErrorCode::RuntimeEffectWrongOutcome,
                format!("expected signal outcome, got {other:?}"),
            )),
        }
    }

    /// Sleeps process execution through the effect-host seam for code-executor implementors so
    /// cancellation and replay semantics remain durable.
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
                crate::RuntimeEffectLocalExecutor::sleep_under(
                    &self.turn_cancel_wait(cancellation.clone()),
                    std::sync::Arc::clone(&self.dispatch.clock),
                ),
            )
            .await?;
        match outcome {
            crate::RuntimeEffectOutcome::Sleep => {
                // Process sleeps remain uninterruptible. The registry read is the
                // wake boundary: durable cancellation may be committed before
                // its live signal reaches this worker, but it must still win
                // before guest code resumes after the timer.
                if let Some(process) = self.process_execution.as_ref() {
                    let event_context = process
                        .event_context
                        .as_ref()
                        .ok_or_else(missing_process_execution_error)?;
                    let events = match event_context
                        .process_work
                        .registry()
                        .events_after(&process.process_id, 0)
                        .await
                    {
                        Ok(events) => events,
                        Err(error) => {
                            let error = match error {
                                crate::PluginError::Runtime(error) => {
                                    crate::RuntimeEffectControllerError::from(error)
                                }
                                error => crate::RuntimeEffectControllerError::from(error),
                            };
                            self.record_nested_effect_error(error.clone());
                            return Err(error);
                        }
                    };
                    let cancel_requested = events
                        .iter()
                        .any(|event| event.event_type == "process.cancel_requested");
                    if cancel_requested {
                        cancellation.cancel();
                        return Err(crate::RuntimeEffectControllerError::new(
                            crate::RuntimeErrorCode::RuntimeEffectSleepCancelled,
                            "runtime effect sleep observed process cancellation at wake",
                        ));
                    }
                }
                Ok(())
            }
            other => Err(crate::RuntimeEffectControllerError::new(
                crate::RuntimeErrorCode::RuntimeEffectWrongOutcome,
                format!("expected sleep outcome, got {}", other.kind().as_str()),
            )),
        }
    }

    /// Exposes chronological projection to protocol and process-engine implementors while executing
    /// code against the session runtime.
    pub fn chronological_projection(&self) -> Arc<crate::ChronologicalProjection> {
        Arc::clone(&self.chronological_projection)
    }

    /// Executes trigger effect work for protocol and process-engine implementors while executing
    /// code against the session runtime.
    pub async fn execute_trigger_effect(
        &self,
        effect_id: String,
        command: crate::TriggerCommand,
    ) -> Result<crate::TriggerEffectResult, crate::RuntimeEffectControllerError> {
        let store = self.trigger_store().ok_or_else(|| {
            crate::RuntimeEffectControllerError::new(
                crate::RuntimeErrorCode::TriggerStoreUnavailable,
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

    /// Exposes trigger actor to protocol and process-engine implementors while executing code
    /// against the session runtime.
    pub fn trigger_actor(&self) -> crate::ProcessOriginator {
        self.process_execution
            .as_ref()
            .map(|exec| exec.originator.clone())
            .unwrap_or_else(|| crate::ProcessOriginator::session(self.session_scope()))
    }

    /// Exposes trigger owner scope to protocol and process-engine implementors while executing code
    /// against the session runtime.
    pub fn trigger_owner_scope(&self) -> Result<crate::TriggerOwnerScope, crate::PluginError> {
        resolve_trigger_owner_scope(
            &self.session_id,
            self.process_execution.as_ref().map(|exec| &exec.originator),
        )
    }

    /// Exposes trigger registration wake target to protocol and process-engine implementors while
    /// executing code against the session runtime. Returns `None` when no trigger registration wake
    /// target is present.
    pub fn trigger_registration_wake_target(&self) -> Option<crate::SessionScope> {
        self.process_execution
            .as_ref()
            .and_then(|exec| exec.wake_session_id.as_ref())
            .map(crate::SessionScope::new)
            .or_else(|| Some(self.session_scope()))
    }

    /// Exposes turn context to protocol and process-engine implementors while executing code
    /// against the session runtime.
    pub fn turn_context(&self) -> &crate::TurnContext {
        &self.turn_context
    }
}

fn missing_process_execution_error() -> crate::RuntimeEffectControllerError {
    crate::RuntimeEffectControllerError::new(
        crate::RuntimeErrorCode::ProcessRegistryUnavailable,
        "process execution is unavailable outside a durable process execution",
    )
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
        Some(crate::ProcessOriginator::Session { session_id, .. }) => {
            Ok(crate::TriggerOwnerScope::session(session_id.clone()))
        }
        None => Ok(crate::TriggerOwnerScope::session(root_session_id)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tool_dispatch::ToolDispatchContext;
    use crate::{ToolCall, ToolOutcome, ToolProvider};

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
            crate::facade_support::frame_node_id("root-session", "agent-frame"),
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

        async fn execute(&self, _call: ToolCall<'_>) -> ToolOutcome {
            ToolOutcome::err_fmt("not used")
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
            .build_session("session")
            .expect("plugin session");
        let (event_tx, _event_rx) = tokio::sync::mpsc::channel(1);
        let dispatch = Arc::new(ToolDispatchContext {
            plugins,
            tools: Arc::new(NoopTools),
            tool_registry: None,
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
                crate::NativeRuntimeEffectController::default(),
            )),
            direct_completions: crate::DirectCompletionClient::unavailable(
                "direct completions are unavailable in this test context",
            ),
            parent_invocation: None,
            execution_env_spec: crate::ProcessExecutionEnvSpec::new(
                crate::PluginOptions::default(),
                crate::SessionPolicy::new(crate::TurnBudget::Unbounded),
            ),
            session_id: "session".to_string(),
            agent_frame_id: crate::FrameNodeId::default(),
            event_tx,
            checkpoint_messages: crate::tool_dispatch::CheckpointMessageBuffer::default(),
            trigger_outcomes: crate::tool_dispatch::ToolTriggerOutcomeBuffer::default(),
            recorded_intent_outcomes:
                crate::tool_dispatch::RecordedToolIntentOutcomeBuffer::default(),
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

    fn test_execution_context() -> RuntimeExecutionContext<'static> {
        let plugins = crate::plugin::PluginHost::empty()
            .build_session("session")
            .expect("plugin session");
        let (event_tx, _event_rx) = tokio::sync::mpsc::channel(1);
        let dispatch = Arc::new(ToolDispatchContext {
            plugins,
            tools: Arc::new(NoopTools),
            tool_registry: None,
            tool_catalog: Arc::new(crate::ToolCatalog::from_tools(
                Vec::new(),
                std::collections::BTreeMap::new(),
            )),
            sessions: Arc::new(crate::testing::MockSessionManager::default()),
            session_lifecycle: Arc::new(crate::testing::MockSessionManager::default()),
            session_graph: Arc::new(crate::testing::MockSessionManager::default()),
            processes: Arc::new(crate::UnavailableProcessService),
            trigger_router: None,
            effect_controller: crate::runtime::RuntimeEffectControllerHandle::shared(Arc::new(
                crate::NativeRuntimeEffectController::default(),
            )),
            direct_completions: crate::DirectCompletionClient::unavailable(
                "direct completions are unavailable in this test context",
            ),
            parent_invocation: None,
            execution_env_spec: crate::ProcessExecutionEnvSpec::new(
                crate::PluginOptions::default(),
                crate::SessionPolicy::new(crate::TurnBudget::Unbounded),
            ),
            session_id: "session".to_string(),
            agent_frame_id: crate::FrameNodeId::default(),
            event_tx,
            checkpoint_messages: crate::tool_dispatch::CheckpointMessageBuffer::default(),
            trigger_outcomes: crate::tool_dispatch::ToolTriggerOutcomeBuffer::default(),
            recorded_intent_outcomes:
                crate::tool_dispatch::RecordedToolIntentOutcomeBuffer::default(),
            attachment_store: Arc::new(crate::SessionAttachmentStore::in_memory()),
            attachment_source_policy: Arc::new(crate::OpenAttachmentSourcePolicy),
            turn_context: crate::TurnContext::default(),
            clock: std::sync::Arc::new(crate::SystemClock),
        });
        RuntimeExecutionContext::new(
            "session".to_string(),
            dispatch,
            Arc::new(crate::InMemoryProcessExecutionEnvStore::new()),
            Arc::new(crate::SessionAttachmentStore::in_memory()),
            Arc::new(crate::ChronologicalProjection::default()),
            None,
            crate::TurnContext::default(),
        )
    }

    #[tokio::test]
    async fn execution_context_without_process_execution_returns_typed_error_from_append_and_signal()
     {
        let ctx = test_execution_context();

        let append_err = ctx
            .append_process_event(crate::ProcessEventAppendRequest::new(
                "test.event",
                serde_json::json!({}),
            ))
            .await
            .unwrap_err();

        let crate::PluginError::RuntimeEffectController(append_effect_err) = append_err else {
            panic!("expected PluginError::RuntimeEffectController, got {append_err:?}");
        };
        assert_eq!(
            append_effect_err.code,
            crate::RuntimeErrorCode::ProcessRegistryUnavailable
        );
        assert_eq!(
            append_effect_err.message,
            "process execution is unavailable outside a durable process execution"
        );

        let signal_err = ctx
            .signal_process_by_id(
                "proc-1",
                "sig-1",
                "sig-id-1".to_string(),
                serde_json::json!({}),
            )
            .await
            .unwrap_err();

        assert_eq!(
            signal_err.code,
            crate::RuntimeErrorCode::ProcessRegistryUnavailable
        );
        assert_eq!(
            signal_err.message,
            "process execution is unavailable outside a durable process execution"
        );
    }
}
