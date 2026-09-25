use crate::ProcessId;
use crate::SessionId;
use lash_sansio::sync::MutexExt;
use std::sync::Arc;

use tokio::sync::mpsc::Sender;
use tokio_util::sync::CancellationToken;

use crate::tool_dispatch::ToolDispatchContext;
use crate::{TurnActivity, TurnActivityId, TurnEvent};

/// What an execution knows about its turn's cancellation, from recorded facts
/// only (FIG-3672 P9).
///
/// It starts as the turn's own recorded fact when the execution is built, and
/// only two things advance it: a recorded outcome that says the turn was
/// cancelled (a wait that lost to the turn's gate), and a journaled cancel
/// checkpoint a code cell issues. A replay meets the same outcomes and
/// checkpoints in the same order, so it reaches the same answer at the same
/// point. No live watch writes it. Clones share it, so the cell's host and the
/// context it issues through agree.
#[derive(Clone, Default)]
pub(crate) struct RecordedTurnCancel {
    observed: Arc<std::sync::atomic::AtomicBool>,
    control: Option<Arc<crate::runtime::turn_control::ActiveTurnControl>>,
    /// The deployment host a recorded step body watches the gate pair over.
    host: Option<Arc<dyn crate::EffectHost>>,
    /// The stop the turn lends its tool children, fired with the fact.
    lent: Option<CancellationToken>,
}

impl RecordedTurnCancel {
    fn is_observed(&self) -> bool {
        self.observed.load(std::sync::atomic::Ordering::SeqCst)
    }

    fn note(&self) {
        self.observed
            .store(true, std::sync::atomic::Ordering::SeqCst);
        if let Some(lent) = &self.lent {
            lent.cancel();
        }
    }
}

#[derive(Clone)]
pub struct RuntimeExecutionContext<'run> {
    pub(super) session_id: SessionId,
    pub(super) dispatch: Arc<ToolDispatchContext<'run>>,
    /// The catalog the live registry resolves to, when the dispatch catalog
    /// is a turn's recorded surface: what a code cell's journaled binding set
    /// is judged against, tool by tool (FIG-3587). `None` means the dispatch
    /// catalog is live.
    live_tool_catalog: Option<Arc<crate::ToolCatalog>>,
    process_env_store: Arc<dyn crate::ProcessExecutionEnvStore>,
    attachment_store: Arc<crate::SessionAttachmentStore>,
    chronological_projection: Arc<crate::ChronologicalProjection>,
    protocol_extension: Option<crate::ProtocolTurnExtensionHandle>,
    turn_context: crate::TurnContext,
    execution_env_spec: crate::ProcessExecutionEnvSpec,
    /// Attempt bound this execution stamps onto children a script engine starts
    /// on the model's behalf. Resolved from the host config when the enclosing
    /// execution is wired; the engine bridges pin the resolved value in their
    /// own durable segment state so a redrive re-registers what was recorded.
    engine_child_max_attempts: std::num::NonZeroU32,
    process_execution: Option<RuntimeProcessExecution>,
    pub(super) parent_invocation: Option<crate::RuntimeInvocation>,
    turn_phase_probe: Option<Arc<dyn crate::runtime::RuntimeTurnPhaseProbe>>,
    pub(super) turn_event_tx: Option<Sender<TurnActivity>>,
    pub(super) cancellation_token: Option<CancellationToken>,
    turn_cancel: RecordedTurnCancel,
    pub(super) observe_turn_cancel: bool,
    /// Durable cancellation authority for waits issued by this execution.
    /// A follow-on physical turn keeps its admitted effect scope but observes
    /// the cancellation gate addressed to its own turn identity.
    turn_cancel_scope: Option<crate::ExecutionScope>,
    /// Per-tool trace emission handle for this execution. Present only when the
    /// host installed a trace sink; `None` keeps every trace call a no-op.
    tracing: Option<RuntimeExecutionTracing>,
    /// Graph key of the enclosing code block, stamped onto the per-tool
    /// `TurnEvent`s emitted from this context so consumers can attribute a tool
    /// call to its code block without ordering heuristics. `None` when the
    /// context is not executing a code block.
    code_block_graph_key: Option<String>,
    /// Workflow node that issued tool calls through this context.
    issuing_language_node_id: Option<Arc<str>>,
    /// `None` for top-level tool execution.
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
    started_process_ids: Arc<std::sync::Mutex<std::collections::HashSet<ProcessId>>>,
    /// Nested durable-controller failure captured while a language runtime
    /// owns the stack. Its fixed host-reply API must unwind before the
    /// enclosing code-execution effect can abort.
    nested_effect_error: Arc<std::sync::Mutex<Option<crate::RuntimeEffectControllerError>>>,
    /// The once-only settlement incorporation ledger (ADR 0099 §6/§13,
    /// FIG-3411): which recorded settlements this context has applied and
    /// which usage deltas it has already charged. Travels wherever
    /// `started_process_ids` travels — the `Arc` is shared by clones and
    /// `to_static`, so a rebound or handed-over context incorporates against
    /// the same set.
    pub(crate) incorporation_ledger: Arc<std::sync::Mutex<crate::session::IncorporationLedger>>,
    /// The groups this context's opener still holds after a consumer stopped
    /// before exhaustion (ADR 0099 §7). Shared with the ledger through
    /// [`OpenerState`](crate::session::OpenerState): the opener's owner hands
    /// one state to every phase context it builds.
    pub(crate) opener_groups: Arc<std::sync::Mutex<crate::session::OpenerGroupRegistry>>,
    /// The host's durable closing seam (ADR 0099 §7), which the opener's end
    /// finalizes through. `None` on Restate, whose engine-side group index is
    /// the twin, and wherever no host wired one.
    pub(crate) group_closing: Option<Arc<dyn crate::StoreEffectGroupClosing>>,
    /// Keeps this context's live-opener registration alive for the context's
    /// lifetime: a `LiveOpenerGuard` deregisters on drop, and a test context
    /// that opened a tool-child group while its guard was already dropped
    /// would route children to a dead opener.
    #[cfg(any(test, feature = "testing"))]
    live_opener_guard: Option<Arc<crate::runtime::effect::LiveOpenerGuard>>,
    /// Keeps the effect host that routed this context's tool children alive:
    /// `ToolChildHost` holds only a `Weak` back to its host (a strong one
    /// would make host → controller → resolver a cycle), so a test context
    /// whose host was dropped after wiring would find "the effect host that
    /// routed this tool child is gone" on every child.
    #[cfg(any(test, feature = "testing"))]
    tool_child_host: Option<Arc<dyn crate::EffectHost>>,
}

#[derive(Clone)]
struct ProcessInvocationCorrelation {
    process_id: ProcessId,
    authority: crate::ProcessExecutionWriteAuthority,
}

/// Carries attempt-bound process invocation authority into one live child turn.
///
/// The private input type prevents an ordinary [`crate::TurnContext`] caller
/// from fabricating the correlation with a string. The runtime revalidates the
/// process and attempt when it reads the value.
pub(crate) fn attach_process_invocation_correlation(
    turn_context: &mut crate::TurnContext,
    process_id: &ProcessId,
    authority: &crate::ProcessExecutionWriteAuthority,
) {
    if authority.attempt_for(process_id).is_some() {
        turn_context.set_runtime_correlation(ProcessInvocationCorrelation {
            process_id: process_id.clone(),
            authority: authority.clone(),
        });
    } else {
        turn_context.clear_runtime_correlation::<ProcessInvocationCorrelation>();
    }
}

pub(crate) fn clear_process_invocation_correlation(turn_context: &mut crate::TurnContext) {
    turn_context.clear_runtime_correlation::<ProcessInvocationCorrelation>();
}

#[derive(Clone)]
pub(crate) struct RuntimeProcessExecution {
    pub process_id: ProcessId,
    pub originator: crate::ProcessOriginator,
    pub env_ref: Option<crate::ProcessExecutionEnvRef>,
    pub wake_session_id: Option<SessionId>,
    pub event_context: Option<RuntimeExecutionProcessEventContext>,
}

#[derive(Clone)]
pub struct RuntimeExecutionProcessEventContext {
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
pub struct RuntimeExecutionTracing {
    sink: Arc<dyn lash_trace::TraceSink>,
    base_context: lash_trace::TraceContext,
    scope_context: lash_trace::TraceContext,
}

impl RuntimeExecutionTracing {
    pub fn new(
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
        issuing_node_id: Option<&str>,
        clock: &dyn crate::Clock,
    ) {
        self.emit(
            lash_trace::TraceEvent::ToolCallCompleted {
                call_id: record.call_id.clone(),
                name: record.tool.clone(),
                args: record.args.clone(),
                output: crate::trace::trace_tool_call_output(&record.output),
                duration_ms: record.duration_ms,
                issuing_node_id: issuing_node_id.map(str::to_string),
                attempts: (!attempts.is_empty()).then(|| attempts.to_vec()),
            },
            clock,
        );
    }
}

impl<'run> RuntimeExecutionContext<'run> {
    /// Restore run-local child possession for a resumed process-engine segment.
    pub fn restore_started_process_ids(&self, process_ids: &[ProcessId]) {
        self.started_process_ids
            .lock_recover()
            .extend(process_ids.iter().cloned());
    }

    /// Snapshot run-local child possession before a process-engine segment handover.
    pub fn started_process_ids(&self) -> Vec<ProcessId> {
        let mut process_ids = self
            .started_process_ids
            .lock_recover()
            .iter()
            .cloned()
            .collect::<Vec<_>>();
        process_ids.sort_unstable();
        process_ids
    }

    /// Restore the once-only incorporation ledger a segment handover carried
    /// (FIG-3411): without it a successor context would incorporate the same
    /// settlement a second time and double-charge its usage.
    pub fn restore_incorporation_ledger(&self, ledger: crate::session::IncorporationLedger) {
        *self.incorporation_ledger.lock_recover() = ledger;
    }

    /// Snapshot the once-only incorporation ledger for a segment handover.
    pub fn incorporation_ledger_snapshot(&self) -> crate::session::IncorporationLedger {
        self.incorporation_ledger.lock_recover().clone()
    }

    pub(super) fn effect_attribution(&self) -> crate::RuntimeAttribution {
        if let Some(parent) = self.parent_invocation.as_ref() {
            return parent.attribution.clone();
        }
        match self
            .process_execution
            .as_ref()
            .map(|execution| &execution.originator)
        {
            Some(crate::ProcessOriginator::Host { .. }) => crate::RuntimeAttribution::none(),
            Some(crate::ProcessOriginator::Session { session_id, .. }) => {
                crate::RuntimeAttribution::for_session(session_id.clone())
            }
            None => crate::RuntimeAttribution::for_session(self.session_id.clone()),
        }
    }

    #[expect(
        clippy::expect_used,
        reason = "the scope comes from the caller's own live effect controller, which is admitted by construction"
    )]
    pub(crate) fn language_runtime_invocation(
        &self,
        effect_id: &str,
    ) -> crate::RuntimeEffectInvocation {
        let execution_scope = self
            .dispatch
            .effect_controller
            .scoped()
            .execution_scope()
            .clone();
        crate::RuntimeEffectInvocation::new(
            crate::EffectAddress::new(execution_scope, effect_id)
                .expect("runtime context carries an admitted effect scope"),
            self.effect_attribution(),
            effect_id,
        )
        .with_caused_by(
            self.parent_invocation
                .as_ref()
                .and_then(crate::RuntimeInvocation::causal_ref),
        )
    }

    #[expect(
        clippy::expect_used,
        reason = "the scope comes from the caller's own live effect controller, which is admitted by construction"
    )]
    fn deferred_resolution_invocation(&self, effect_id: &str) -> crate::RuntimeEffectInvocation {
        let execution_scope = self
            .dispatch
            .effect_controller
            .scoped()
            .execution_scope()
            .clone();
        crate::RuntimeEffectInvocation::new(
            crate::EffectAddress::new(execution_scope, effect_id)
                .expect("runtime context carries an admitted effect scope"),
            crate::RuntimeAttribution::none(),
            effect_id,
        )
        .with_caused_by(
            self.parent_invocation
                .as_ref()
                .and_then(crate::RuntimeInvocation::causal_ref),
        )
    }

    /// Executes a nondeterministic language-runtime operation behind the
    /// durable effect controller so replay returns the recorded sample.
    pub async fn journaled_language_runtime_value(
        &self,
        effect_id: String,
        operation: String,
    ) -> Result<serde_json::Value, crate::RuntimeEffectControllerError> {
        let invocation = self.language_runtime_invocation(&effect_id);
        self.dispatch
            .effect_controller
            .scoped()
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

    /// Journals a replayed language run's seal at `key` (FIG-3586): the
    /// run's facts ride in `facts` and so in the envelope, and `producer` is
    /// the outcome — served back on replay, so the answer names who wrote the
    /// journal, never who is replaying it.
    pub async fn journal_run_seal(
        &self,
        key: String,
        facts: String,
        producer: serde_json::Value,
    ) -> Result<serde_json::Value, crate::RuntimeEffectControllerError> {
        let invocation = self.language_runtime_invocation(&key);
        self.dispatch
            .effect_controller
            .scoped()
            .execute_effect(
                crate::RuntimeEffectEnvelope::new(
                    invocation,
                    crate::RuntimeEffectCommand::LanguageRuntimeValue {
                        operation: format!(
                            "{}:{facts}",
                            crate::runtime::effect::RUN_SEAL_OPERATION
                        ),
                    },
                ),
                crate::RuntimeEffectLocalExecutor::run_seal(producer),
            )
            .await?
            .into_language_runtime_value()
    }

    /// Journals the link-scoped deferred-resolution decision without inheriting
    /// the live caller attribution. The admitted parent address supplies the
    /// durable identity; attribution and descriptive parent labels are not part
    /// of that decision and must not make recovery hash a different envelope.
    pub async fn journaled_deferred_resolution_with<F, Fut>(
        &self,
        effect_id: String,
        operation: String,
        run: F,
    ) -> Result<serde_json::Value, crate::RuntimeEffectControllerError>
    where
        F: FnOnce() -> Fut + Send + 'run,
        Fut: std::future::Future<
                Output = Result<serde_json::Value, crate::RuntimeEffectControllerError>,
            > + Send
            + 'run,
    {
        let invocation = self.deferred_resolution_invocation(&effect_id);
        let expected_operation = operation.clone();
        self.dispatch
            .effect_controller
            .scoped()
            .execute_effect(
                crate::RuntimeEffectEnvelope::new(
                    invocation,
                    crate::RuntimeEffectCommand::LanguageRuntimeValue { operation },
                ),
                crate::RuntimeEffectLocalExecutor::language_runtime_value_with(
                    move |envelope| async move {
                        let crate::RuntimeEffectCommand::LanguageRuntimeValue { operation } =
                            envelope.command
                        else {
                            return Err(crate::RuntimeEffectControllerError::new(
                                crate::RuntimeErrorCode::RuntimeEffectLocalExecutorMismatch,
                                "deferred-resolution executor requires a language_runtime_value command",
                            ));
                        };
                        if operation != expected_operation {
                            return Err(crate::RuntimeEffectControllerError::new(
                                crate::RuntimeErrorCode::RuntimeEffectLocalExecutorMismatch,
                                format!(
                                    "deferred-resolution operation `{operation}` does not match `{expected_operation}`"
                                ),
                            ));
                        }
                        Ok(crate::RuntimeEffectOutcome::LanguageRuntimeValue {
                            value: run().await?,
                        })
                    },
                ),
            )
            .await?
            .into_language_runtime_value()
    }

    /// Records a classified nested runtime-effect failure so the enclosing
    /// `ExecCode` effect aborts instead of journaling a model-visible response.
    pub fn record_nested_runtime_effect_error(&self, error: crate::RuntimeEffectControllerError) {
        self.record_nested_effect_error(error);
    }
    pub(super) fn process_scope(
        &self,
        parent_invocation: Option<crate::RuntimeInvocation>,
    ) -> crate::ProcessOpScope<'_> {
        crate::ProcessOpScope::new(self.dispatch.effect_controller.scoped())
            .with_parent_invocation(parent_invocation)
            .with_agent_frame_id(Some(self.dispatch.agent_frame_id.clone()))
    }

    pub(crate) fn record_started_process(&self, process_id: &ProcessId) {
        self.started_process_ids
            .lock_recover()
            .insert(ProcessId::from(process_id.to_string()));
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
    pub fn new(
        session_id: SessionId,
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
            live_tool_catalog: None,
            execution_env_spec: crate::ProcessExecutionEnvSpec::new(
                crate::PluginOptions::default(),
                crate::SessionPolicy::new(crate::TurnBudget::Unbounded),
            ),
            engine_child_max_attempts: crate::runtime::DEFAULT_ENGINE_CHILD_MAX_ATTEMPTS,
            process_execution: None,
            started_process_ids: Arc::default(),
            nested_effect_error: Arc::default(),
            incorporation_ledger: Arc::default(),
            opener_groups: Arc::default(),
            group_closing: None,
            parent_invocation: None,
            turn_phase_probe: None,
            turn_event_tx: None,
            cancellation_token: None,
            turn_cancel: RecordedTurnCancel::default(),
            observe_turn_cancel: true,
            turn_cancel_scope: None,
            tracing: None,
            code_block_graph_key: None,
            issuing_language_node_id: None,
            batch_parent_call_id: None,
            process_work: None,
            #[cfg(any(test, feature = "testing"))]
            live_opener_guard: None,
            #[cfg(any(test, feature = "testing"))]
            tool_child_host: None,
        }
    }

    pub(crate) fn to_static(&self) -> Option<RuntimeExecutionContext<'static>> {
        Some(RuntimeExecutionContext {
            session_id: self.session_id.clone(),
            dispatch: Arc::new(self.dispatch.to_static()?),
            live_tool_catalog: self.live_tool_catalog.clone(),
            process_env_store: Arc::clone(&self.process_env_store),
            attachment_store: Arc::clone(&self.attachment_store),
            chronological_projection: Arc::clone(&self.chronological_projection),
            protocol_extension: self.protocol_extension.clone(),
            turn_context: self.turn_context.clone(),
            execution_env_spec: self.execution_env_spec.clone(),
            engine_child_max_attempts: self.engine_child_max_attempts,
            process_execution: self.process_execution.clone(),
            parent_invocation: self.parent_invocation.clone(),
            turn_phase_probe: self.turn_phase_probe.clone(),
            turn_event_tx: self.turn_event_tx.clone(),
            cancellation_token: self.cancellation_token.clone(),
            turn_cancel: self.turn_cancel.clone(),
            observe_turn_cancel: self.observe_turn_cancel,
            turn_cancel_scope: self.turn_cancel_scope.clone(),
            tracing: self.tracing.clone(),
            code_block_graph_key: self.code_block_graph_key.clone(),
            issuing_language_node_id: self.issuing_language_node_id.clone(),
            batch_parent_call_id: self.batch_parent_call_id.clone(),
            process_work: self.process_work.clone(),
            started_process_ids: Arc::clone(&self.started_process_ids),
            nested_effect_error: Arc::clone(&self.nested_effect_error),
            incorporation_ledger: Arc::clone(&self.incorporation_ledger),
            opener_groups: Arc::clone(&self.opener_groups),
            group_closing: self.group_closing.clone(),
            #[cfg(any(test, feature = "testing"))]
            live_opener_guard: self.live_opener_guard.clone(),
            #[cfg(any(test, feature = "testing"))]
            tool_child_host: self.tool_child_host.clone(),
        })
    }

    pub fn take_nested_effect_error(&self) -> Option<crate::RuntimeEffectControllerError> {
        self.nested_effect_error.lock_recover().take()
    }

    pub fn execution_scope_id(&self) -> String {
        self.dispatch
            .effect_controller
            .scoped()
            .scope_id()
            .to_string()
    }

    /// The admitted scope this execution's controller carries: the execution
    /// scope plus, when it is a process, the incarnation it was admitted
    /// under. This is the checked pair — consumers that need an opener derive
    /// it from this value rather than re-pairing scope and pin themselves.
    pub fn admitted_scope(&self) -> crate::AdmittedScope {
        self.dispatch
            .effect_controller
            .scoped()
            .admitted_scope()
            .clone()
    }

    /// The process incarnation this execution's scope was admitted under, when
    /// it runs under one.
    ///
    /// A process-backed session turn — the shape every `agents.spawn` child
    /// takes — runs under `ExecutionScope::Process`, which carries the reusable
    /// process name and no incarnation. The admitted scope the controller was
    /// built with carries the exact pair, and this is where an execution that
    /// must name its opener (ADR 0099 §1) reads it back.
    pub fn admitted_process(&self) -> Option<crate::ProcessRef> {
        self.dispatch
            .effect_controller
            .scoped()
            .admitted_process()
            .cloned()
    }

    /// The durable process attempt admitted for this execution, when it runs
    /// under a process whose engine installed execution authority.
    pub fn admitted_process_attempt(&self) -> Option<u32> {
        if let Some(execution) = self.process_execution.as_ref() {
            return execution
                .event_context
                .as_ref()?
                .execution_write_authority
                .attempt_for(&execution.process_id);
        }
        let correlation = self
            .turn_context
            .runtime_correlation::<ProcessInvocationCorrelation>()?;
        correlation.authority.attempt_for(&correlation.process_id)
    }

    /// Returns the exact owner used to stage artifacts produced by this
    /// replayable execution.
    pub fn artifact_owner(&self) -> crate::ArtifactOwner {
        crate::ArtifactOwner::execution(
            self.dispatch
                .effect_controller
                .scoped()
                .execution_scope()
                .clone(),
        )
    }

    pub fn session_scope(&self) -> crate::SessionScope {
        crate::SessionScope::for_agent_frame(
            self.session_id.clone(),
            self.dispatch.agent_frame_id.clone(),
        )
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

    pub fn with_turn_event_sender(mut self, turn_event_tx: Sender<TurnActivity>) -> Self {
        self.turn_event_tx = Some(turn_event_tx);
        self
    }

    pub fn with_tracing(mut self, tracing: Option<RuntimeExecutionTracing>) -> Self {
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

    pub fn with_code_block_graph_key(mut self, graph_key: Option<String>) -> Self {
        self.code_block_graph_key = graph_key;
        self
    }

    pub fn with_issuing_language_node_id(mut self, node_id: impl Into<String>) -> Self {
        self.issuing_language_node_id = Some(Arc::from(node_id.into()));
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
                    issuing_node_id: self.issuing_language_node_id.as_deref().map(str::to_string),
                },
                self.dispatch.clock.as_ref(),
            );
        }
    }

    /// No-op when the host installed no trace sink.
    pub(super) fn emit_tool_call_completed_trace(
        &self,
        record: &crate::ToolCallRecord,
        attempts: &[lash_trace::TraceRetryAttempt],
    ) {
        if let Some(tracing) = self.tracing.as_ref() {
            tracing.emit_tool_call_completed(
                record,
                attempts,
                self.issuing_language_node_id.as_deref(),
                self.dispatch.clock.as_ref(),
            );
        }
    }

    pub fn with_parent_invocation(mut self, metadata: crate::RuntimeInvocation) -> Self {
        self.parent_invocation = Some(metadata);
        self
    }

    /// Retains a live-opener registration for this context's lifetime (test
    /// and conformance contexts only).
    #[cfg(any(test, feature = "testing"))]
    pub fn with_live_opener_guard(
        mut self,
        guard: Arc<crate::runtime::effect::LiveOpenerGuard>,
    ) -> Self {
        self.live_opener_guard = Some(guard);
        self
    }

    /// Retains the effect host this context's tool children route through —
    /// the `ToolChildHost` resolver holds it weakly (test and conformance
    /// contexts only).
    #[cfg(any(test, feature = "testing"))]
    pub fn with_tool_child_host(mut self, host: Arc<dyn crate::EffectHost>) -> Self {
        self.tool_child_host = Some(host);
        self
    }

    pub(super) fn attachment_acceptance(&self) -> &crate::provider::AttachmentCapabilitySnapshot {
        &self
            .execution_env_spec
            .policy
            .model
            .capability
            .attachment_acceptance
    }

    /// The attempt bound a script engine stamps onto a child it starts for the
    /// model. Engine bridges read this once when an execution segment begins
    /// and record the resolved value in their durable segment state, so the
    /// registration fingerprint stays stable across a host config change.
    pub fn engine_child_max_attempts(&self) -> std::num::NonZeroU32 {
        self.engine_child_max_attempts
    }

    pub fn with_engine_child_max_attempts(
        mut self,
        engine_child_max_attempts: std::num::NonZeroU32,
    ) -> Self {
        self.engine_child_max_attempts = engine_child_max_attempts;
        self
    }

    pub fn with_execution_env_spec(
        mut self,
        execution_env_spec: crate::ProcessExecutionEnvSpec,
    ) -> Self {
        self.execution_env_spec = execution_env_spec;
        self
    }

    pub fn with_process_execution(
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

    pub fn with_turn_phase_probe(
        mut self,
        probe: Option<Arc<dyn crate::runtime::RuntimeTurnPhaseProbe>>,
    ) -> Self {
        self.turn_phase_probe = probe;
        self
    }

    pub fn named_phase(&self, phase: &'static str) -> crate::runtime::RuntimeNamedPhase {
        crate::runtime::RuntimeNamedPhase::begin(self.turn_phase_probe.clone(), phase)
    }

    pub(crate) fn attempt_phase_probe(
        &self,
    ) -> Option<Arc<dyn crate::runtime::RuntimeTurnPhaseProbe>> {
        self.turn_phase_probe.clone()
    }

    pub fn with_cancellation_token(mut self, cancellation_token: CancellationToken) -> Self {
        self.cancellation_token = Some(cancellation_token);
        self
    }

    /// Starts this execution's recorded turn-cancel fact: `honoured` is
    /// whether the turn had already recorded a cancellation when it built
    /// this execution, `control` is the gate pair a code cell's cancel
    /// checkpoints peek, and `lent` is the stop the turn lends its tool
    /// children, fired when the fact advances. The turn driver is the only
    /// caller.
    pub fn with_recorded_turn_cancel(
        mut self,
        honoured: bool,
        control: Arc<crate::runtime::turn_control::ActiveTurnControl>,
        host: Arc<dyn crate::EffectHost>,
        lent: CancellationToken,
    ) -> Self {
        // A tool this execution runs in process cooperates through the same
        // lent stop its group children get: it fires only when the recorded
        // fact advances, so a tool's cancel is never a live read of the gate.
        if self.cancellation_token.is_none() {
            self.cancellation_token = Some(lent.clone());
        }
        let turn_cancel = RecordedTurnCancel {
            observed: Arc::default(),
            control: Some(control),
            host: Some(host),
            lent: Some(lent),
        };
        if honoured {
            turn_cancel.note();
        }
        self.turn_cancel = turn_cancel;
        self
    }

    /// Records that a recorded outcome this execution received cancelled its
    /// turn: a wait that lost to the turn's cancellation gate. Only outcomes
    /// the engine recorded may call this, so a replay reaches it at the same
    /// point.
    pub fn note_turn_cancelled(&self) {
        self.turn_cancel.note();
    }

    /// Execution-side only: run one recorded step body that this execution
    /// issues in process (a tool attempt) under a cooperative stop that fires
    /// when the turn's gate pair asks it to stop now (FIG-3672 P9). The body
    /// gets the stop; what it returns is the step's recorded outcome. A watch
    /// that gives up leaves the body running to its end (the engine records
    /// every tool outcome, so the fault must not become one). An execution
    /// with no gate control runs the body under its own token.
    pub(crate) async fn run_turn_step_body<T, F, Fut>(&self, body: F) -> T
    where
        F: FnOnce(Option<CancellationToken>) -> Fut,
        Fut: std::future::Future<Output = T>,
    {
        let (Some(control), Some(host)) = (
            self.turn_cancel.control.as_ref(),
            self.turn_cancel.host.as_ref(),
        ) else {
            return body(self.cancellation_token.clone()).await;
        };
        control
            .run_recorded_step_body(host, self.is_cancelled(), |stop| body(Some(stop)))
            .await
    }

    /// A code cell's cancel checkpoint (FIG-3672 P9): a journaled peek of the
    /// turn's gate pair under the checkpoint's own identity, which advances
    /// this execution's recorded fact when the turn must stop now. A replay
    /// issues the same checkpoints at the same instruction counts and is
    /// served the same answers. Answers whether the turn is cancelled.
    ///
    /// An execution with no gate control (a process body, a test context)
    /// has no checkpoint and answers from its fact alone.
    pub async fn turn_cancel_checkpoint(
        &self,
        checkpoint: u64,
    ) -> Result<bool, crate::RuntimeEffectControllerError> {
        if self.turn_cancel.is_observed() {
            return Ok(true);
        }
        let Some(control) = self.turn_cancel.control.as_ref() else {
            return Ok(false);
        };
        let Some(cell) = self
            .parent_invocation
            .as_ref()
            .and_then(crate::RuntimeInvocation::replay_key)
            .map(str::to_string)
        else {
            return Ok(false);
        };
        let observed = control
            .observe_pending_cancel(
                &self.dispatch.effect_controller.scoped(),
                crate::runtime::turn_control::TurnCancelPeekIdentity::CellCheckpoint {
                    cell,
                    checkpoint,
                },
            )
            .await
            .map_err(crate::RuntimeEffectControllerError::from)?;
        if observed.is_some() {
            self.turn_cancel.note();
        }
        Ok(observed.is_some())
    }

    pub fn without_turn_cancel_observation(mut self) -> Self {
        self.observe_turn_cancel = false;
        self
    }

    pub fn with_turn_cancel_scope(mut self, scope: crate::ExecutionScope) -> Self {
        self.turn_cancel_scope = Some(scope);
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
            match self.turn_cancel_scope.clone() {
                Some(scope) => crate::runtime::TurnCancelWait::observing(cancellation, scope),
                None => self
                    .dispatch
                    .effect_controller
                    .scoped()
                    .turn_cancel_wait(cancellation),
            }
        } else {
            crate::runtime::TurnCancelWait::unobserved(cancellation)
        }
    }

    pub fn with_process_work(mut self, process_work: Option<crate::ProcessWorkWiring>) -> Self {
        self.process_work = process_work;
        self
    }

    pub fn record_nested_effect_error(&self, error: crate::RuntimeEffectControllerError) {
        let mut pending = self.nested_effect_error.lock_recover();
        pending.get_or_insert(error);
    }

    /// The recorded nested effect error, when it reports a replay divergence
    /// (FIG-3586): a language runtime inspects it after each command it
    /// issued, so a mismatch at a recorded entry stops the run there instead
    /// of reaching the program as a catchable failure.
    pub fn nested_replay_mismatch(&self) -> Option<crate::RuntimeEffectControllerError> {
        self.nested_effect_error
            .lock_recover()
            .as_ref()
            .filter(|error| error.code.is_replay_mismatch())
            .cloned()
    }

    /// Records `error` as the nested effect error, replacing whatever was
    /// recorded before: a language runtime re-types the replay mismatch its
    /// command met into the run's own divergence, which carries the ordinal
    /// and attribution the substrate's mismatch cannot.
    pub fn replace_nested_effect_error(&self, error: crate::RuntimeEffectControllerError) {
        *self.nested_effect_error.lock_recover() = Some(error);
    }

    /// Whether a nested effect error is recorded: the enclosing execution
    /// aborts, so it writes nothing further of its own.
    pub fn has_nested_effect_error(&self) -> bool {
        self.nested_effect_error.lock_recover().is_some()
    }

    /// The recorded nested effect error, left in place.
    pub(crate) fn peek_nested_effect_error(&self) -> Option<crate::RuntimeEffectControllerError> {
        self.nested_effect_error.lock_recover().clone()
    }

    /// This context with `command`'s invocation as the parent every nested
    /// effect it issues descends from (FIG-3586): a process command journals
    /// at `{command}:{effect id}`, under the command's own key.
    pub fn under_command(&self, command: &crate::CommandReplayKey) -> Self {
        let invocation = crate::runtime::command_invocation(
            self.dispatch.effect_controller.scoped().execution_scope(),
            self.effect_attribution(),
            self.parent_invocation.as_ref(),
            command,
        )
        .into_runtime_invocation();
        self.clone().with_parent_invocation(invocation)
    }

    /// This context serving one replayed language command (FIG-3586): every
    /// journal write the command makes — a tool attempt, a runtime value, a
    /// trigger operation, a sleep, a group open, a process command — asks
    /// `guard` first.
    pub fn with_command_journal_guard(&self, guard: Arc<crate::CommandJournalGuard>) -> Self {
        let mut dispatch = (*self.dispatch).clone();
        dispatch.effect_controller = dispatch.effect_controller.with_journal_guard(guard);
        let mut context = self.clone();
        context.dispatch = Arc::new(dispatch);
        context
    }

    /// The recorded-frontier read of this execution's scope (FIG-3586): the
    /// journal rows its controller holds in `range`, with this opener's group
    /// keys read back as the commands that formed them.
    pub async fn read_recorded_journal(
        &self,
        range: &crate::RecordedKeyRange,
    ) -> Result<crate::RecordedJournal, crate::RuntimeEffectControllerError> {
        let range = crate::RecordedKeyRange {
            group_key_prefix: self.own_group_key_prefix(),
            ..range.clone()
        };
        self.dispatch
            .effect_controller
            .controller()
            .read_recorded_journal(&range)
            .await
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
        self.turn_cancel.is_observed()
            || self
                .cancellation_token
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

    /// Restate invocation that owns the enclosing process execution, when this
    /// context runs inside an attempt-bound Restate process.
    pub fn restate_invocation_id(&self) -> Option<&str> {
        if let Some(execution) = self.process_execution.as_ref() {
            return execution
                .event_context
                .as_ref()?
                .execution_write_authority
                .restate_invocation_id(&execution.process_id);
        }
        let correlation = self
            .turn_context
            .runtime_correlation::<ProcessInvocationCorrelation>()?;
        correlation
            .authority
            .restate_invocation_id(&correlation.process_id)
    }

    pub(crate) fn is_run_local_process(&self, process_id: &ProcessId) -> bool {
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

    /// Resolves the execution environment a session-path process start hands
    /// the journaled process-start command, publishing nothing.
    ///
    /// Publication belongs inside the replayable process effect (FIG-3050).
    /// Publishing here would stage the artifact under
    /// [`ArtifactOwner::process_start`](crate::ArtifactOwner::process_start)
    /// *before* the start is journaled, and a replay of the same turn would
    /// revisit that staging owner after the first attempt's start effect
    /// transferred the artifact and permanently retired it — the divergence
    /// FIG-3028 had to absorb with a retirement tolerance at this call site.
    /// The spec instead rides
    /// [`ProcessStartOptions::env_spec`](crate::ProcessStartOptions::env_spec)
    /// into the command, and the executor publishes it under the journal.
    ///
    /// A start made *inside* a process execution inherits the reference its own
    /// registration carries: those bytes are already published under a durable
    /// owner, so that start stages nothing either and the executor protects the
    /// recorded reference instead.
    pub(crate) fn process_start_execution_env(
        &self,
        registration: crate::ProcessRegistration,
    ) -> (
        crate::ProcessRegistration,
        Option<crate::ProcessExecutionEnvSpec>,
    ) {
        if registration.env_ref.is_some() {
            return (registration, None);
        }
        match registration.input.as_ref() {
            crate::ProcessInput::ToolCall { .. } | crate::ProcessInput::Engine { .. } => {
                match self.inherited_process_execution_env_ref() {
                    Some(env_ref) => (registration.with_execution_env_ref(Some(env_ref)), None),
                    None => (registration, Some(self.execution_env_spec.clone())),
                }
            }
            crate::ProcessInput::External { .. } | crate::ProcessInput::SessionTurn { .. } => {
                (registration, None)
            }
        }
    }

    /// A retired owner fails here. Every caller publishes under a durable owner and then persists
    /// the reference (trigger registration keeps it in `TriggerSubscriptionDraft::env_ref`), so a
    /// retirement must surface at publish time rather than hand back a reference to bytes the
    /// fence already reclaimed. Process starts do not publish at all before their journal: they go
    /// through [`Self::process_start_execution_env`].
    pub async fn captured_process_execution_env_ref(
        &self,
        owner: &crate::ArtifactOwner,
    ) -> Result<crate::ProcessExecutionEnvRef, crate::PluginError> {
        if let Some(env_ref) = self.inherited_process_execution_env_ref() {
            return Ok(env_ref);
        }
        crate::publish_process_execution_env(
            self.process_env_store.as_ref(),
            owner,
            &self.execution_env_spec,
        )
        .await
    }

    fn inherited_process_execution_env_ref(&self) -> Option<crate::ProcessExecutionEnvRef> {
        self.process_execution
            .as_ref()
            .and_then(|exec| exec.env_ref.clone())
    }

    /// The tool-execution context this run lends its tool calls.
    ///
    /// Exposed so the turn path can publish it to the live-opener registry
    /// (ADR 0099 §3): a tool child of a group this turn opens borrows the live
    /// half of exactly this context.
    pub fn dispatch(&self) -> &Arc<ToolDispatchContext<'run>> {
        &self.dispatch
    }

    /// The enclosing durable parent for a code-executor's child start.
    ///
    /// Derived through the one owner derivation — the admitted scope the
    /// controller was built with — so a same-name successor in the registry
    /// cannot rebind a child this execution's opener still owns (FIG-3417).
    /// There is no registry access here by design: `resolve_process_ref` is a
    /// name lookup, and a name is not an owner.
    pub fn child_process_parent_scope(&self) -> Result<crate::ParentScope, crate::PluginError> {
        let scoped = self.dispatch.effect_controller.scoped();
        let opener = crate::EffectOpener::for_scope(scoped.admitted_scope())
            .map_err(|error| crate::PluginError::Session(error.to_string()))?;
        Ok(crate::ParentScope::from_owner(&opener))
    }

    pub async fn start_child_process(
        &self,
        request: crate::ProcessStartRequest,
        _kind: impl Into<String>,
        _label: Option<String>,
    ) -> crate::ToolInvocationReply {
        let _phase = self.named_phase("process.start_child");
        let registration = request.into_registration(None);
        let (registration, env_spec) = self.process_start_execution_env(registration);
        let process_id = registration.id.clone();
        // The registry row, not the caller's pin, is the durable truth for a
        // child's attempt bound: a redrive that re-registers the same
        // deterministic child id after the host default moved must re-register
        // with the recorded value or the registration fingerprint conflicts
        // forever. Only a child with no row yet takes the caller's resolution.
        let registration = match self
            .dispatch
            .processes
            .recorded_max_attempts(&self.session_id, &process_id)
            .await
        {
            Ok(Some(recorded)) => registration.with_max_attempts(Some(recorded)),
            Ok(None) => registration,
            Err(err) => {
                return crate::ToolInvocationReply::error(serde_json::json!(err.to_string()));
            }
        };
        let mut options = crate::ProcessStartOptions::new()
            .with_initial_observers(self.child_process_observers())
            .with_env_spec(env_spec);
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
            Ok(record) => {
                self.record_started_process(&process_id);
                crate::ToolInvocationReply::success(Self::process_handle_json(
                    &crate::ProcessRef::from_record(&record),
                ))
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
        command: &crate::CommandReplayKey,
        process_id: &ProcessId,
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
        // The wait is addressed by name and per-name ordinal, because outside
        // signallers address it; the journaled await is addressed by the
        // command's issue ordinal, like every other effect of the run
        // (FIG-3586).
        let invocation = crate::runtime::causal::child_effect_invocation(
            self.dispatch.effect_controller.scoped().execution_scope(),
            &crate::runtime::command_invocation(
                self.dispatch.effect_controller.scoped().execution_scope(),
                self.effect_attribution(),
                self.parent_invocation.as_ref(),
                command,
            )
            .into_runtime_invocation(),
            command.signal(),
            "signal",
        );
        let outcome = self
            .dispatch
            .effect_controller
            .scoped()
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
            crate::Resolution::Err(err) => Err(crate::RuntimeEffectControllerError::foreign(
                // A host's completion code is host-authored vocabulary: it
                // lands in `ForeignCode` verbatim (namespace included) and is
                // never re-parsed into a Lash `RuntimeErrorCode` arm.
                err.code.namespaced(),
                crate::TurnFailureCause::Outcome,
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

    pub fn callable_tool_manifest_by_id(&self, id: &crate::ToolId) -> Option<crate::ToolManifest> {
        crate::tool_dispatch::resolve_callable_manifest_by_id(&self.dispatch, id)
    }

    /// Appends one named, replay-scoped signal to a process for code-executor implementors.
    pub async fn signal_process_by_id(
        &self,
        process_id: &ProcessId,
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
        let process_ref = registry
            .resolve_process_ref(process_id)
            .await
            .map_err(crate::RuntimeEffectControllerError::from)?;
        let replay_key = crate::process_signal_wait_key(process_id, signal_name, &signal_id);
        let command = crate::ProcessCommand::Signal {
            process_ref,
            signal_name: signal_name.to_string(),
            signal_id,
            request: crate::ProcessEventAppendRequest::new(event_type.clone(), payload)
                .with_replay_key(replay_key),
        };
        let effect_id = command.effect_id();
        let invocation = crate::runtime::causal::process_effect_invocation(
            self.dispatch.effect_controller.scoped().execution_scope(),
            self.parent_invocation
                .as_ref()
                .map(|parent| parent.attribution.clone())
                .unwrap_or_else(crate::RuntimeAttribution::none),
            self.parent_invocation.clone(),
            &effect_id,
        );
        let controller = self.dispatch.effect_controller.controller();
        let scoped = self.dispatch.effect_controller.scoped();
        #[expect(
            clippy::expect_used,
            reason = "`EffectTaskController::scoped` returns a proxy that owns the controller it was just built around"
        )]
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
                scoped.admitted_scope().clone(),
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
                        crate::TurnFailureCause::Outcome,
                        "process execution has no process-work port",
                    )
                })?,
        )
        .with_process_effect_controller(owned_controller);
        let outcome = if let Some(task_requests) = task_requests {
            crate::runtime::effect::drive_effect_controller_task(
                controller,
                scoped.execution_scope().clone(),
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

    #[expect(
        clippy::expect_used,
        reason = "the scope comes from the caller's own live effect controller, which is admitted by construction"
    )]
    fn command_sleep_invocation(
        &self,
        command: &crate::CommandReplayKey,
    ) -> crate::RuntimeEffectInvocation {
        crate::RuntimeEffectInvocation::new(
            crate::EffectAddress::new(
                self.dispatch
                    .effect_controller
                    .scoped()
                    .execution_scope()
                    .clone(),
                command.sleep(),
            )
            .expect("a command sleep uses the already admitted controller scope"),
            self.effect_attribution(),
            command.sleep(),
        )
        .with_caused_by(
            self.parent_invocation
                .as_ref()
                .and_then(crate::RuntimeInvocation::causal_ref),
        )
    }

    /// Sleeps under the command key a replayed language program issued the
    /// sleep at (FIG-3586), through the effect-host seam so cancellation and
    /// replay semantics remain durable. The intent is journaled at
    /// [`CommandReplayKey::sleep`](crate::CommandReplayKey::sleep).
    pub async fn sleep_command(
        &self,
        command: &crate::CommandReplayKey,
        spec: crate::SleepSpec,
    ) -> Result<(), crate::RuntimeEffectControllerError> {
        let cancellation = self.cancellation_token.clone().unwrap_or_default();
        let invocation = self.command_sleep_invocation(command);
        let command = crate::RuntimeEffectCommand::Sleep { spec };
        let outcome = self
            .dispatch
            .effect_controller
            .scoped()
            .execute_effect(
                crate::RuntimeEffectEnvelope::new(invocation, command),
                crate::RuntimeEffectLocalExecutor::sleep_under(
                    &self.turn_cancel_wait(cancellation.clone()),
                    std::sync::Arc::clone(&self.dispatch.clock),
                ),
            )
            .await;
        let outcome = match outcome {
            Ok(outcome) => outcome,
            Err(error) => {
                // A retryable sleep failure is the host asking for redelivery,
                // not a guest-visible sleep result. Raise it at the handler
                // boundary so the guest cannot swallow it into a terminal
                // process failure (FIG-3149).
                if error.code.is_retryable() {
                    self.record_nested_effect_error(error.clone());
                }
                // A sleep that lost to the turn's cancellation gate is a
                // recorded outcome: the turn is cancelled from here on.
                if error.code == crate::RuntimeErrorCode::RuntimeEffectSleepCancelled
                    && self
                        .turn_cancel_wait(CancellationToken::new())
                        .observes_turn_cancel()
                {
                    self.note_turn_cancelled();
                }
                return Err(error);
            }
        };
        match outcome {
            crate::RuntimeEffectOutcome::Sleep => {
                // Process sleeps remain uninterruptible, and the wake is where a
                // committed cancellation takes ownership of settlement. The
                // verdict itself is the effect host's to record: a live read at
                // this boundary can answer differently on redrive, so the
                // durable host journals the wake verdict and reports it as
                // `RuntimeEffectSleepCancelled` instead (FIG-3149).
                Ok(())
            }
            other => Err(crate::RuntimeEffectControllerError::new(
                crate::RuntimeErrorCode::RuntimeEffectWrongOutcome,
                format!("expected sleep outcome, got {}", other.kind().as_str()),
            )),
        }
    }

    pub fn chronological_projection(&self) -> Arc<crate::ChronologicalProjection> {
        Arc::clone(&self.chronological_projection)
    }

    pub async fn execute_trigger_effect(
        &self,
        effect_id: String,
        mut command: crate::TriggerCommand,
    ) -> Result<crate::TriggerEffectResult, crate::RuntimeEffectControllerError> {
        self.admit_trigger_command_target(&mut command).await?;
        let store = self.trigger_store().ok_or_else(|| {
            crate::RuntimeEffectControllerError::new(
                crate::RuntimeErrorCode::TriggerStoreUnavailable,
                "trigger store is unavailable in this runtime",
            )
        })?;
        #[expect(
            clippy::expect_used,
            reason = "the scope comes from the caller's own live effect controller, which is admitted by construction"
        )]
        let invocation = crate::RuntimeEffectInvocation::new(
            crate::EffectAddress::new(
                self.dispatch
                    .effect_controller
                    .scoped()
                    .execution_scope()
                    .clone(),
                effect_id.clone(),
            )
            .expect("runtime context carries an admitted effect scope"),
            self.effect_attribution(),
            effect_id.clone(),
        )
        .with_caused_by(
            self.parent_invocation
                .as_ref()
                .and_then(crate::RuntimeInvocation::causal_ref),
        );
        self.dispatch
            .effect_controller
            .scoped()
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

    /// Runs the engine registry's admission on every registration-shaped
    /// trigger command before it reaches the store (FIG-1522).
    ///
    /// A subscription's target is admitted exactly once, here: delivery replays
    /// the recorded target without re-gating it, so an unregistered engine kind
    /// that got past this point would produce starts admitted nowhere. A
    /// registration whose target names an engine therefore requires a wired
    /// engine registry; there is no unchecked path.
    async fn admit_trigger_command_target(
        &self,
        command: &mut crate::TriggerCommand,
    ) -> Result<(), crate::RuntimeEffectControllerError> {
        let draft = match command {
            crate::TriggerCommand::Register { draft, .. }
            | crate::TriggerCommand::Update { draft, .. }
            | crate::TriggerCommand::Revive { draft, .. } => draft,
            _ => return Ok(()),
        };
        if !matches!(draft.target, crate::ProcessInput::Engine { .. }) {
            return Ok(());
        }
        // A runtime that wired no process-engine registry has no authority to
        // consult: there is nothing here that could say whether the kind is
        // known, and the start itself still fails closed when the engine is
        // missing. Refusing here instead would break every embedder that
        // registers triggers without an engine registry, which is not the hole
        // FIG-1522 names — that hole is a *configured* registry never being
        // asked.
        let Some(registry) = self
            .dispatch
            .trigger_router
            .as_ref()
            .and_then(crate::TriggerRouter::process_engines)
        else {
            return Ok(());
        };
        crate::admit_trigger_registration_target(registry, draft)
            .await
            .map_err(crate::RuntimeEffectControllerError::from)
    }

    pub fn parent_invocation(&self) -> Option<&crate::RuntimeInvocation> {
        self.parent_invocation.as_ref()
    }

    pub fn session_id(&self) -> &str {
        &self.session_id
    }

    pub fn tool_catalog(&self) -> Arc<crate::ToolCatalog> {
        Arc::clone(&self.dispatch.tool_catalog)
    }

    /// The catalog the live registry resolves to now. It is
    /// [`Self::tool_catalog`] unless that is a turn's recorded surface.
    pub fn live_tool_catalog(&self) -> Arc<crate::ToolCatalog> {
        self.live_tool_catalog
            .clone()
            .unwrap_or_else(|| Arc::clone(&self.dispatch.tool_catalog))
    }

    #[must_use]
    pub(crate) fn with_live_tool_catalog(mut self, live: Arc<crate::ToolCatalog>) -> Self {
        self.live_tool_catalog = Some(live);
        self
    }

    pub fn trigger_actor(&self) -> crate::ProcessOriginator {
        self.process_execution
            .as_ref()
            .map(|exec| exec.originator.clone())
            .unwrap_or_else(|| crate::ProcessOriginator::session(self.session_scope()))
    }

    pub fn trigger_owner_scope(&self) -> Result<crate::TriggerOwnerScope, crate::PluginError> {
        resolve_trigger_owner_scope(
            &self.session_id,
            self.process_execution.as_ref().map(|exec| &exec.originator),
        )
    }

    pub fn trigger_registration_wake_target(&self) -> Option<crate::SessionScope> {
        self.process_execution
            .as_ref()
            .and_then(|exec| exec.wake_session_id.as_ref())
            .map(crate::SessionScope::new)
            .or_else(|| Some(self.session_scope()))
    }

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
    root_session_id: &SessionId,
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
mod tests;
