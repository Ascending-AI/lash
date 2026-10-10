use crate::ProcessId;
use crate::SessionId;
use lash_sansio::sync::MutexExt;
use std::sync::Arc;

mod definition_publication;
mod observations;
pub use observations::RuntimeExecutionTracing;
pub(crate) use observations::ToolObservationAttribution;
mod referrers;
mod tool_completion;
pub(crate) use referrers::execution_claim_of;
pub(crate) use tool_completion::ToolCallStart;

use tokio_util::sync::CancellationToken;

use crate::tool_dispatch::ToolDispatchContext;

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
    /// Cooperative cancellation for recorded tool bodies, fired with the turn fact.
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
    tool_fault_retry: crate::runtime::PollPacing,
    pub(super) dispatch: Arc<ToolDispatchContext<'run>>,
    tool_material_store: Option<Arc<dyn crate::store::ToolMaterialStore>>,

    /// The catalog the live registry resolves to, when the dispatch catalog
    /// is a turn's recorded surface: what a code cell's journaled binding set
    /// is judged against, tool by tool (FIG-3587). `None` means the dispatch
    /// catalog is live.
    live_tool_catalog: Option<Arc<crate::ToolCatalog>>,
    pub(super) process_env_store: Arc<dyn crate::ProcessExecutionEnvStore>,
    /// `F` this execution's durable writers emit — recorded by the bound
    /// session's store, so a stamped surface version is the fleet's, not the
    /// build's newest (FIG-3796). Contexts with no store default to the
    /// build's own generation.
    fleet_format: crate::FleetFormat,
    attachment_store: Arc<crate::RuntimeAttachmentStore>,
    chronological_projection: Arc<crate::ChronologicalProjection>,
    turn_context: crate::TurnContext,
    /// The admitted logical Run owning a foreground cell, independent of
    /// the effect scope of a process-owned session shift.
    logical_run: Option<crate::TurnAddress>,
    /// The capability refs the logical Run's recorded shape names, by slot
    /// (`RunSpec::capabilities`): empty outside a run, or for a run whose
    /// spec names none.
    run_capabilities: Arc<std::collections::BTreeMap<crate::SlotId, crate::CapabilityRef>>,
    execution_env_spec: crate::ProcessExecutionEnvSpec,
    process_execution: Option<RuntimeProcessExecution>,
    pub(super) parent_invocation: Option<crate::RuntimeInvocation>,
    turn_phase_probe: Option<Arc<dyn crate::runtime::RuntimeTurnPhaseProbe>>,
    pub(super) cancellation_token: Option<CancellationToken>,
    /// Whether `cancellation_token` is only a stop lent to this execution's
    /// step bodies (a process drive's, FIG-3673): then no shift decision reads
    /// it, and [`is_cancelled`](Self::is_cancelled) answers from recorded facts
    /// alone.
    token_is_lent_stop: bool,
    turn_cancel: RecordedTurnCancel,
    pub(super) observe_turn_cancel: bool,
    /// Set when a transferable wait this context issued was handed over,
    /// shared with every context derived from this one.
    wait_handed_over: Arc<std::sync::atomic::AtomicBool>,
    /// Durable cancellation authority for waits issued by this execution.
    /// A follow-on physical turn keeps its admitted effect scope but observes
    /// the cancellation gate addressed to its own turn identity.
    turn_cancel_scope: Option<crate::ExecutionScope>,
    /// Per-tool trace emission handle for this execution. Present only when the
    /// host installed a trace sink; `None` keeps every trace call a no-op.
    pub(super) tracing: Option<RuntimeExecutionTracing>,
    /// The live step of the recorded body this context runs in, when it runs
    /// in one: bound by the engine's step wrapper when the body really runs.
    live_step: Option<Arc<crate::trace::LiveStep>>,
    /// A standing a fixture placed this context at
    /// ([`Self::with_trace_standing`]).
    #[cfg(any(test, feature = "testing"))]
    fixture_standing: Option<crate::trace::TraceStanding>,
    /// Graph key of the enclosing code block, stamped onto the per-tool
    /// `TurnEvent`s emitted from this context so consumers can attribute a tool
    /// call to its code block without ordering heuristics. `None` when the
    /// context is not executing a code block.
    code_block_graph_key: Option<String>,
    /// Workflow node that issued tool calls through this context.
    pub(super) issuing_language_node_id: Option<Arc<str>>,
    /// Passive graph attribution, shared with the logical Run's process starts.
    pub(super) language_calls: crate::runtime::process::LanguageCallAttributions,
    /// Work-driver handle for this execution's process wiring, when the
    /// deployment provides one. Threaded through so in-run process
    /// operations (e.g. cancelling another process) that build their own
    /// `RuntimeEffectLocalExecutor::processes(..)` call can hand it along
    /// instead of falling back to hub-less backoff polling.
    process_work: Option<crate::ProcessWorkWiring>,
    /// Process ids started by THIS execution context. Possession of a handle
    /// the run itself created is sufficient capability to await/cancel it —
    /// run-local children do not require session observer edges (the ephemeral
    /// execution scope must never appear in durable grant state).
    started_process_ids: Arc<std::sync::Mutex<std::collections::BTreeSet<ProcessId>>>,
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
    /// The trace scope each call this execution traced the start of opened,
    /// for its completion.
    pub(crate) tool_requests: Arc<
        std::sync::Mutex<
            std::collections::BTreeMap<crate::ToolCallId, tool_completion::TracedToolCall>,
        >,
    >,
    /// The latest `max_tool_calls` refusal this execution met, kept typed for
    /// the language runtime that reports the failed cell or process: its own
    /// error channel carries the refusal's class, code and message only.
    pub(crate) tool_call_limit_refusal: Arc<std::sync::Mutex<Option<crate::ToolCallLimitExceeded>>>,
}

#[derive(Clone)]
pub(crate) struct RuntimeProcessExecution {
    pub process_id: ProcessId,
    pub originator: crate::ProcessOriginator,
    pub env_ref: Option<crate::ProcessExecutionEnvRef>,
    pub event_context: Option<RuntimeExecutionProcessEventContext>,
}

#[derive(Clone)]
pub struct RuntimeExecutionProcessEventContext {
    pub execution_write_authority: crate::ProcessExecutionWriteAuthority,
    pub process_work: crate::ProcessWorkWiring,
    pub store: Option<Arc<dyn crate::RuntimeStore>>,
    pub session_store_factory: Option<Arc<dyn crate::DeploymentStore>>,

    pub clock: Arc<dyn crate::Clock>,
}

impl<'run> RuntimeExecutionContext<'run> {
    /// Configure tool-fault retries for this execution and its derived contexts.
    pub fn with_tool_fault_retry(mut self, pacing: crate::runtime::PollPacing) -> Self {
        self.tool_fault_retry = pacing;
        self
    }

    pub fn tool_fault_retry(&self) -> crate::runtime::PollPacing {
        self.tool_fault_retry
    }

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
            None => self
                .dispatch
                .owner
                .session_id()
                .map(|session_id| crate::RuntimeAttribution::for_session(session_id.clone()))
                .unwrap_or_else(crate::RuntimeAttribution::none),
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
        let execution_scope = self.dispatch.effect_controller.execution_scope().clone();
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
        let execution_scope = self.dispatch.effect_controller.execution_scope().clone();
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

    /// The owned actor's context this execution runs under: what its VM
    /// snapshots and their operations' admissions commit through.
    pub fn actor_context(&self) -> &crate::ActorContext {
        &self.dispatch.effect_controller
    }

    pub async fn journaled_language_runtime_value(
        &self,
        effect_id: String,
        operation: String,
    ) -> Result<serde_json::Value, crate::RuntimeEffectControllerError> {
        let invocation = self.language_runtime_invocation(&effect_id);
        self.dispatch
            .effect_controller
            .vm_effect(
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

    /// Records a language-owned value once under a cell-scoped key. Replay
    /// serves the stored result without invoking `run` again, so whatever
    /// `run` does — an attachment put included — happens on the first
    /// execution only, and its recorded answer is what every replay reads.
    pub async fn journaled_language_value_with<F, Fut>(
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
        // The cell replay key and causal parent identify this record. Live
        // turn indices may differ on redrive and must not change its envelope.
        let invocation = self.deferred_resolution_invocation(&effect_id);
        self.dispatch
            .effect_controller
            .vm_effect(
                crate::RuntimeEffectEnvelope::new(
                    invocation,
                    crate::RuntimeEffectCommand::LanguageRuntimeValue { operation },
                ),
                crate::RuntimeEffectLocalExecutor::language_runtime_value_with(
                    move |_| async move {
                        Ok(crate::RuntimeEffectOutcome::LanguageRuntimeValue {
                            value: run().await?,
                        })
                    },
                ),
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
            .vm_effect(
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
    pub(crate) fn process_scope(
        &self,
        parent_invocation: Option<crate::RuntimeInvocation>,
    ) -> crate::ProcessOpScope<'_> {
        crate::ProcessOpScope::new(self.dispatch.effect_controller.clone())
            .with_parent_invocation(parent_invocation)
            .with_agent_frame_id(self.dispatch.owner.agent_frame_id().cloned())
            .with_process_lineage(self.dispatch.process_lineage.clone())
    }

    pub(crate) fn record_started_process(&self, process_id: &ProcessId) {
        self.started_process_ids
            .lock_recover()
            .insert(process_id.clone());
    }

    pub(crate) fn to_static(&self) -> Option<RuntimeExecutionContext<'static>> {
        Some(RuntimeExecutionContext {
            tool_fault_retry: self.tool_fault_retry,
            dispatch: Arc::new(self.dispatch.to_static()?),
            tool_material_store: self.tool_material_store.clone(),

            live_tool_catalog: self.live_tool_catalog.clone(),
            process_env_store: Arc::clone(&self.process_env_store),
            fleet_format: self.fleet_format,
            attachment_store: Arc::clone(&self.attachment_store),
            chronological_projection: Arc::clone(&self.chronological_projection),
            turn_context: self.turn_context.clone(),
            logical_run: self.logical_run.clone(),
            run_capabilities: Arc::clone(&self.run_capabilities),
            execution_env_spec: self.execution_env_spec.clone(),
            process_execution: self.process_execution.clone(),
            parent_invocation: self.parent_invocation.clone(),
            turn_phase_probe: self.turn_phase_probe.clone(),
            cancellation_token: self.cancellation_token.clone(),
            token_is_lent_stop: self.token_is_lent_stop,
            turn_cancel: self.turn_cancel.clone(),
            observe_turn_cancel: self.observe_turn_cancel,
            wait_handed_over: Arc::clone(&self.wait_handed_over),
            turn_cancel_scope: self.turn_cancel_scope.clone(),
            tracing: self.tracing.clone(),
            live_step: self.live_step.clone(),
            #[cfg(any(test, feature = "testing"))]
            fixture_standing: self.fixture_standing.clone(),
            code_block_graph_key: self.code_block_graph_key.clone(),
            issuing_language_node_id: self.issuing_language_node_id.clone(),
            language_calls: Arc::clone(&self.language_calls),

            process_work: self.process_work.clone(),
            started_process_ids: Arc::clone(&self.started_process_ids),
            nested_effect_error: Arc::clone(&self.nested_effect_error),
            incorporation_ledger: Arc::clone(&self.incorporation_ledger),
            tool_requests: Arc::clone(&self.tool_requests),
            tool_call_limit_refusal: Arc::clone(&self.tool_call_limit_refusal),
        })
    }

    pub fn with_tool_material_store(
        mut self,
        store: Arc<dyn crate::store::ToolMaterialStore>,
    ) -> Self {
        self.tool_material_store = Some(store);
        self
    }
    pub fn tool_material_store(&self) -> Option<Arc<dyn crate::store::ToolMaterialStore>> {
        self.tool_material_store.clone()
    }

    pub fn take_nested_effect_error(&self) -> Option<crate::RuntimeEffectControllerError> {
        self.nested_effect_error.lock_recover().take()
    }

    pub fn execution_scope_id(&self) -> String {
        self.dispatch.effect_controller.scope_id().to_string()
    }

    /// The admitted scope this execution's controller carries: the execution
    /// scope plus, when it is a process, the incarnation it was admitted
    /// under. This is the checked pair — consumers that need an opener derive
    /// it from this value rather than re-pairing scope and pin themselves.
    pub fn admitted_scope(&self) -> crate::AdmittedScope {
        self.dispatch.effect_controller.admitted_scope().clone()
    }

    /// The process incarnation this execution's scope was admitted under, when
    /// it runs under one.
    ///
    /// A process-backed session turn — the shape every `agents.spawn` child
    /// takes — runs under `ExecutionScope::Process`, which carries the reusable
    /// process name and no incarnation. The admitted scope the controller was
    /// built with carries the exact pair, and this is where an execution that
    /// must name its opener (ADR 0099 §1) reads it back.
    pub fn admitted_process(&self) -> Option<crate::ProcessId> {
        self.dispatch.effect_controller.admitted_process().cloned()
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
        let correlation = correlation::process_invocation_of(&self.turn_context)?;
        correlation.authority.attempt_for(&correlation.process_id)
    }

    /// The session scope of the frame this execution was admitted on, or
    /// [`crate::PluginError::NotASessionRuntime`] inside a process.
    pub fn session_scope(&self) -> Result<crate::SessionScope, crate::PluginError> {
        match &self.dispatch.owner {
            crate::ExecutionOwner::SessionFrame {
                session_id,
                agent_frame_id,
            } => Ok(crate::SessionScope::for_agent_frame(
                session_id.clone(),
                agent_frame_id.clone(),
            )),
            crate::ExecutionOwner::Process { process_id } => Err(
                crate::runtime::not_a_session_runtime("session_scope", process_id),
            ),
        }
    }

    /// The fleet-format generation this context's durable writers emit —
    /// `F` as the bound store recorded it, or this build's own generation
    /// where the context holds no store (FIG-3796).
    pub fn fleet_format(&self) -> crate::FleetFormat {
        self.fleet_format
    }

    /// Binds the fleet format the bound session's store recorded. Every
    /// durable stamp produced inside this context — effect summaries, parent
    /// scopes, leases — routes through `writer_version` on this value.
    #[must_use]
    pub fn with_fleet_format(mut self, fleet_format: crate::FleetFormat) -> Self {
        self.fleet_format = fleet_format;
        self
    }

    pub fn with_parent_invocation(mut self, metadata: crate::RuntimeInvocation) -> Self {
        self.parent_invocation = Some(metadata);
        self
    }

    pub(crate) fn attachment_acceptance(&self) -> &crate::provider::AttachmentCapabilitySnapshot {
        &self.execution_env_spec.policy.attachment_acceptance
    }

    /// The session's recorded tool-call limit, as this execution runs under
    /// it: a run's snapshot for a turn, the recorded environment for a
    /// process.
    /// The `max_tool_calls` the execution runs under.
    #[must_use]
    pub fn max_tool_calls(&self) -> crate::MaxToolCalls {
        self.execution_env_spec.policy.max_tool_calls
    }

    /// The protocol turn options the execution runs under: the protocol
    /// plugin's namespace of the configuration it was admitted under, a
    /// run's own options applied. For a turn's cell they are the options
    /// its driver reads as the turn's termination.
    #[must_use]
    pub fn protocol_turn_options(&self) -> crate::ProtocolTurnOptions {
        self.execution_env_spec
            .plugin_config
            .config
            .protocol_turn_options()
    }

    pub fn with_execution_env_spec(
        mut self,
        execution_env_spec: crate::ProcessExecutionEnvSpec,
    ) -> Self {
        self.execution_env_spec = execution_env_spec;
        self
    }

    pub(crate) async fn recorded_tool_run_env_spec(
        &self,
        reference: &crate::ProcessExecutionEnvRef,
    ) -> Result<crate::ProcessExecutionEnvSpec, crate::PluginError> {
        crate::load_process_execution_env(self.process_env_store.as_ref(), reference)
            .await
            .map_err(Into::into)
    }

    pub(crate) fn tool_run_env_spec(&self) -> crate::ProcessExecutionEnvSpec {
        self.execution_env_spec.clone()
    }

    pub fn recorded_render(&self) -> Option<&crate::RecordedRender> {
        self.execution_env_spec.render.as_ref()
    }

    pub fn with_recorded_render(mut self, recorded: crate::RecordedRender) -> Self {
        self.execution_env_spec.render = Some(recorded);
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

    /// The stop this execution observes: what cancels the work it runs on
    /// its turn's behalf. A context with none never stops.
    #[must_use]
    pub fn cancellation(&self) -> CancellationToken {
        self.cancellation_token.clone().unwrap_or_default()
    }

    pub fn with_cancellation_token(mut self, cancellation_token: CancellationToken) -> Self {
        self.cancellation_token = Some(cancellation_token);
        self
    }

    /// A process drive's execution (FIG-3673): `stop` is lent to the step
    /// bodies it runs, which record what it did to them, and is never a shift
    /// input. The shift observes its process's cancellation only through
    /// recorded outcomes, recorded wait races and
    /// [`process_cancel_checkpoint`](Self::process_cancel_checkpoint).
    pub fn with_lent_process_stop(mut self, stop: CancellationToken) -> Self {
        self.cancellation_token = Some(stop);
        self.token_is_lent_stop = true;
        self
    }

    /// Run one registry step of a process body as a step the effect
    /// controller records under `name` (FIG-3673).
    pub async fn record_process_drive_step(
        &self,
        name: String,
        step: crate::ProcessDriveStep<'_>,
    ) -> Result<(), crate::RuntimeEffectControllerError> {
        self.dispatch
            .effect_controller
            .record_process_drive_step(name, step)
            .await
    }

    /// A process body's cancel checkpoint (FIG-3673): the effect controller's
    /// recorded observation of whether the process has a committed
    /// cancellation. A replay reaches the same checkpoints and reads the same
    /// answers.
    pub async fn process_cancel_checkpoint(
        &self,
    ) -> Result<bool, crate::RuntimeEffectControllerError> {
        self.dispatch
            .effect_controller
            .observe_process_cancel(&self.cancellation_token.clone().unwrap_or_default())
            .await
    }

    /// Records that a recorded outcome this execution received cancelled its
    /// turn: a wait that lost to the turn's cancellation gate. Only outcomes
    /// the engine recorded may call this, so a replay reaches it at the same
    /// point.
    pub fn note_turn_cancelled(&self) {
        self.turn_cancel.note();
    }

    /// A code cell's cancel checkpoint: whether this execution's recorded
    /// fact says the turn is cancelled. A cancel requested while a cell runs
    /// is the session's mail: the phase runner stops the cell at its next
    /// commit and ends the turn `Cancelled` (L3, FIG-5172).
    pub async fn turn_cancel_checkpoint(
        &self,
        _checkpoint: u64,
    ) -> Result<bool, crate::RuntimeEffectControllerError> {
        Ok(self.turn_cancel.is_observed())
    }

    pub fn without_turn_cancel_observation(mut self) -> Self {
        self.observe_turn_cancel = false;
        self
    }

    pub fn with_turn_cancel_scope(mut self, scope: crate::ExecutionScope) -> Self {
        self.turn_cancel_scope = Some(scope);
        self
    }

    /// Bind the admitted logical Run whose cell may cross segment boundaries.
    pub fn with_logical_run(mut self, run: crate::TurnAddress) -> Self {
        self.logical_run = Some(run);
        self
    }

    /// The admitted logical Run authorized to resume this cell's continuation.
    pub fn logical_run(&self) -> Option<&crate::TurnAddress> {
        self.logical_run.as_ref()
    }

    /// Bind the capability refs the logical Run's recorded shape names.
    pub fn with_run_capabilities(
        mut self,
        capabilities: std::collections::BTreeMap<crate::SlotId, crate::CapabilityRef>,
    ) -> Self {
        self.run_capabilities = Arc::new(capabilities);
        self
    }

    /// The capability refs the logical Run's recorded shape names, by slot.
    pub fn run_capabilities(
        &self,
    ) -> &std::collections::BTreeMap<crate::SlotId, crate::CapabilityRef> {
        &self.run_capabilities
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
                    .turn_cancel_wait(cancellation),
            }
        } else {
            crate::runtime::TurnCancelWait::unobserved(cancellation)
        }
    }

    /// Records that a transferable wait this context issued was handed to
    /// the Run's successor segment.
    pub(crate) fn record_wait_handed_over(&self) {
        self.wait_handed_over
            .store(true, std::sync::atomic::Ordering::SeqCst);
    }

    /// Whether a wait this context issued was handed to the Run's successor
    /// segment since the last call; the answer is taken.
    pub fn take_wait_handed_over(&self) -> bool {
        self.wait_handed_over
            .swap(false, std::sync::atomic::Ordering::SeqCst)
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
            self.dispatch.effect_controller.execution_scope(),
            self.effect_attribution(),
            self.parent_invocation.as_ref(),
            command,
        )
        .into_runtime_invocation();
        self.clone().with_parent_invocation(invocation)
    }

    /// This context serving one replayed language command (FIG-3586): every
    /// journal write the command makes — a tool attempt, a runtime value, a
    /// sleep, a group open, a process command — asks
    /// `guard` first.
    pub fn with_command_journal_guard(&self, guard: Arc<crate::CommandJournalGuard>) -> Self {
        let mut dispatch = (*self.dispatch).clone();
        dispatch.effect_controller = dispatch.effect_controller.with_journal_guard(guard);
        let mut context = self.clone();
        context.dispatch = Arc::new(dispatch);
        context
    }

    /// Shares the session-scoped attachment store with code-executor implementors so code-produced
    /// artifacts follow the same durable ownership contract as turn input.
    pub fn attachment_store(&self) -> Arc<crate::RuntimeAttachmentStore> {
        Arc::clone(&self.attachment_store)
    }

    /// Reports whether the execution scope has been cancelled by its host.
    ///
    /// Language-runtime bridges use this cooperative probe to turn a host Stop
    /// into their typed terminal instead of reporting it as a guest-program
    /// failure.
    pub fn is_cancelled(&self) -> bool {
        self.turn_cancel.is_observed()
            || (!self.token_is_lent_stop
                && self
                    .cancellation_token
                    .as_ref()
                    .is_some_and(CancellationToken::is_cancelled))
    }

    pub(crate) fn process_id(&self) -> Option<&ProcessId> {
        self.process_execution.as_ref().map(|exec| &exec.process_id)
    }

    /// Engine execution that owns the enclosing process execution, when this
    /// context runs inside an attempt-bound engine process.
    pub fn engine_execution_id(&self) -> Option<&str> {
        if let Some(execution) = self.process_execution.as_ref() {
            return execution
                .event_context
                .as_ref()?
                .execution_write_authority
                .engine_execution_id(&execution.process_id);
        }
        let correlation = correlation::process_invocation_of(&self.turn_context)?;
        correlation
            .authority
            .engine_execution_id(&correlation.process_id)
    }

    pub(crate) fn is_run_local_process(&self, process_id: &ProcessId) -> bool {
        self.started_process_ids.lock_recover().contains(process_id)
    }

    /// The lineage of the process this context runs inside, when it runs
    /// inside one: what a start made here records above its starter.
    pub(crate) fn process_lineage(&self) -> Option<crate::ProcessLineage> {
        self.dispatch.process_lineage.clone()
    }

    pub(crate) fn process_spawn_provenance(&self) -> Option<crate::ProcessSpawnProvenance> {
        self.process_execution
            .as_ref()
            .map(|exec| crate::ProcessSpawnProvenance {
                originator: exec.originator.clone(),
            })
    }

    fn child_process_observers(&self) -> Vec<crate::SessionId> {
        match self.process_execution.as_ref() {
            Some(exec) => match &exec.originator {
                crate::ProcessOriginator::Host { .. } => Vec::new(),
                crate::ProcessOriginator::Session { session_id, .. } => vec![session_id.clone()],
            },
            None => self
                .dispatch
                .owner
                .session_id()
                .cloned()
                .into_iter()
                .collect(),
        }
    }

    /// Capture a start's environment under this execution before its command
    /// is journaled. The command carries only the published digest.
    pub(crate) async fn process_start_execution_env(
        &self,
        registration: crate::ProcessStartRegistration,
    ) -> Result<crate::ProcessStartRegistration, crate::PluginError> {
        if registration.env_ref.is_some() {
            return Ok(registration);
        }
        let claim = execution_claim_of(self.dispatch.effect_controller.execution_scope())?;
        let env_ref = self.capture_execution_env(&claim).await?;
        Ok(registration.with_execution_env_ref(Some(env_ref)))
    }

    /// Publish or acquire this execution's environment under `claim` before
    /// persisting its digest in a start or a tool run. An ended referrer
    /// refuses acquisition; it cannot resurrect a reclaimed environment.
    pub(crate) async fn capture_execution_env(
        &self,
        claim: &crate::ReferrerClaim,
    ) -> Result<crate::ProcessExecutionEnvRef, crate::PluginError> {
        if let Some(env_ref) = self.inherited_process_execution_env_ref() {
            self.process_env_store
                .acquire_process_execution_env(claim, &env_ref)
                .await
                .map_err(crate::PluginError::from)?;
            return Ok(env_ref);
        }
        crate::publish_process_execution_env(
            self.process_env_store.as_ref(),
            claim,
            &self.execution_env_spec,
        )
        .await
    }

    pub(crate) fn inherited_process_execution_env_ref(
        &self,
    ) -> Option<crate::ProcessExecutionEnvRef> {
        self.process_execution
            .as_ref()
            .and_then(|exec| exec.env_ref.clone())
    }

    /// The start context a code-executor's child start draws its lifetime
    /// from: the admitted scope the controller was built with, and the
    /// lineage of the process this context runs inside. There is no registry
    /// access here by design (FIG-3607 R2).
    pub fn start_cx(&self) -> Result<crate::StartCx, crate::PluginError> {
        let scoped = self.dispatch.effect_controller.clone();
        crate::StartCx::materialize(scoped.admitted_scope(), self.process_lineage().as_ref())
            .map_err(|error| crate::PluginError::Session(error.to_string()))
    }

    pub async fn start_child_process(
        &self,
        request: crate::ProcessStartRequest,
        _kind: impl Into<String>,
        _label: Option<String>,
    ) -> crate::ToolInvocationReply {
        let _phase = self.named_phase("process.start_child");
        let registration = request.into_registration();
        let registration = match self.process_start_execution_env(registration).await {
            Ok(registration) => registration,
            Err(error) => {
                return crate::ToolInvocationReply::error(serde_json::json!(error.to_string()));
            }
        };
        // A redrive presents the same start key and gets the retained process
        // back untouched (ADR 0107), so the attempt bound it recorded stands
        // whatever this attempt resolved.
        let mut options = crate::ProcessStartOptions::new()
            .with_initial_observers(self.child_process_observers());
        if let Some(spawn) = self.process_spawn_provenance() {
            options = options.with_spawn_provenance(spawn);
        }
        let session_id = match self.session_id() {
            Ok(session_id) => session_id.clone(),
            Err(err) => {
                return crate::ToolInvocationReply::error(serde_json::json!(err.to_string()));
            }
        };
        match self
            .dispatch
            .processes
            .start(
                &session_id,
                registration,
                options,
                self.process_scope(self.parent_invocation.clone()),
            )
            .await
        {
            Ok(record) => {
                self.record_started_process(&record.process_id);
                crate::ToolInvocationReply::success(Self::process_handle_json(&record.process_id))
            }
            Err(err) => crate::ToolInvocationReply::error(serde_json::json!(err.to_string())),
        }
    }

    pub fn callable_tool_manifest_by_id(&self, id: &crate::ToolId) -> Option<crate::ToolManifest> {
        crate::tool_dispatch::resolve_callable_manifest_by_id(&self.dispatch, id)
    }

    /// The projection providers of the backend this context's actor runs
    /// over, which answer the reads of its VM runs on this node (ADR 0132
    /// §9). `None` without a backend.
    pub fn projection_providers(
        &self,
    ) -> Option<Arc<dyn crate::runtime::actor::projection::ProjectionProviders>> {
        self.dispatch
            .effect_controller
            .projection_providers()
            .cloned()
    }

    pub fn chronological_projection(&self) -> Arc<crate::ChronologicalProjection> {
        Arc::clone(&self.chronological_projection)
    }

    pub fn parent_invocation(&self) -> Option<&crate::RuntimeInvocation> {
        self.parent_invocation.as_ref()
    }

    /// Who this execution runs for.
    pub fn owner(&self) -> &crate::ExecutionOwner {
        &self.dispatch.owner
    }

    /// The session this execution runs in, or
    /// [`crate::PluginError::NotASessionRuntime`] inside a process.
    pub fn session_id(&self) -> Result<&SessionId, crate::PluginError> {
        self.dispatch.owner.require_session("execution_session_id")
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

    pub fn turn_context(&self) -> &crate::TurnContext {
        &self.turn_context
    }
}

mod correlation;
pub(crate) use correlation::{
    attach_process_invocation_correlation, attach_process_lineage,
    clear_process_invocation_correlation, process_lineage_of,
};

#[cfg(test)]
mod tests;

/// Runtime-only construction and wiring of a [`RuntimeExecutionContext`].
///
/// The turn driver and the process runner in `lash-core` build every
/// execution context; a code executor only consumes one. These members are
/// that cross-crate construction seam: `lash_core::core_internal` re-exports
/// the trait, the `lash` facade does not, and the impl is hidden from docs
/// because it is support plumbing rather than code-executor surface (ADR 0051).
pub mod runtime_ops;
