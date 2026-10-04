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
mod trigger_scope;
pub(crate) use referrers::execution_claim_of;
use trigger_scope::missing_process_execution_error;
pub use trigger_scope::resolve_trigger_owner_scope;

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
    control: Option<Arc<crate::runtime::turn_control::ActiveTurnControl>>,
    /// The deployment host a recorded step body watches the gate pair over.
    host: Option<Arc<dyn crate::EffectHost>>,
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
    pub(super) dispatch: Arc<ToolDispatchContext<'run>>,
    tool_material_store: Option<Arc<dyn crate::store::ToolMaterialStore>>,
    pub(super) tool_run: Option<super::tool_run::ToolRunChannel>,

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
    /// Whether a durable wait this context issues may be handed to the Run's
    /// successor segment (FIG-4739): set by a code cell for an operation its
    /// run is parked on, whose state is captured, and never otherwise.
    transferable_waits: bool,
    /// Whether the turn this context executes for may end at a segment
    /// boundary inside the execution (FIG-4739).
    turn_hands_over: bool,
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
    pub(crate) opener_groups: Arc<std::sync::Mutex<crate::session::OpenerGroupRegistry>>,
    /// Retained request receipts used to commit each call's logical terminal.
    pub(crate) tool_requests: Arc<
        std::sync::Mutex<
            std::collections::BTreeMap<crate::ToolCallId, crate::store::ToolRequestReceipt>,
        >,
    >,
    /// The tool calls this context's cell has made, per group key, counted
    /// against the session's recorded `max_tool_calls` (FIG-4546). A turn
    /// builds a fresh context per cell, so this is the cell's own total; a
    /// redrive re-executes the cell and forms the same groups in the same
    /// order, so it refuses the same call. Shared by clones and `to_static`.
    /// A process counts what it holds instead, in `opener_groups`.
    pub(crate) cell_tool_calls: Arc<std::sync::Mutex<std::collections::BTreeMap<String, usize>>>,
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
    pub wake_session_id: Option<SessionId>,
    pub event_context: Option<RuntimeExecutionProcessEventContext>,
}

#[derive(Clone)]
pub struct RuntimeExecutionProcessEventContext {
    pub execution_write_authority: crate::ProcessExecutionWriteAuthority,
    pub process_work: crate::ProcessWorkWiring,
    pub store: Option<Arc<dyn crate::RuntimeStore>>,
    pub session_store_factory: Option<Arc<dyn crate::DeploymentStore>>,
    pub queued_work: Arc<dyn crate::SessionWorkEngine>,
    pub process_wake_delivery_policy: crate::DeliveryPolicy,
    pub clock: Arc<dyn crate::Clock>,
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

    pub async fn journaled_language_runtime_value(
        &self,
        effect_id: String,
        operation: String,
    ) -> Result<serde_json::Value, crate::RuntimeEffectControllerError> {
        let invocation = self.language_runtime_invocation(&effect_id);
        self.dispatch
            .effect_controller
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
            .execute_effect(
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
        // A redrive may carry different turn metadata. The cell address and
        // causal parent identify its seal, just as they identify its outputs.
        let invocation = self.deferred_resolution_invocation(&key);
        self.dispatch
            .effect_controller
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

    pub(crate) fn session_graph_service(&self) -> &dyn crate::plugin::SessionGraphService {
        self.dispatch.session_graph.as_ref()
    }

    pub(crate) fn to_static(&self) -> Option<RuntimeExecutionContext<'static>> {
        Some(RuntimeExecutionContext {
            dispatch: Arc::new(self.dispatch.to_static()?),
            tool_material_store: self.tool_material_store.clone(),
            tool_run: self.tool_run.clone(),

            live_tool_catalog: self.live_tool_catalog.clone(),
            process_env_store: Arc::clone(&self.process_env_store),
            fleet_format: self.fleet_format,
            attachment_store: Arc::clone(&self.attachment_store),
            chronological_projection: Arc::clone(&self.chronological_projection),
            turn_context: self.turn_context.clone(),
            logical_run: self.logical_run.clone(),
            execution_env_spec: self.execution_env_spec.clone(),
            process_execution: self.process_execution.clone(),
            parent_invocation: self.parent_invocation.clone(),
            turn_phase_probe: self.turn_phase_probe.clone(),
            cancellation_token: self.cancellation_token.clone(),
            token_is_lent_stop: self.token_is_lent_stop,
            turn_cancel: self.turn_cancel.clone(),
            observe_turn_cancel: self.observe_turn_cancel,
            transferable_waits: self.transferable_waits,
            turn_hands_over: self.turn_hands_over,
            wait_handed_over: Arc::clone(&self.wait_handed_over),
            turn_cancel_scope: self.turn_cancel_scope.clone(),
            tracing: self.tracing.clone(),
            live_step: self.live_step.clone(),
            #[cfg(any(test, feature = "testing"))]
            fixture_standing: self.fixture_standing.clone(),
            code_block_graph_key: self.code_block_graph_key.clone(),
            issuing_language_node_id: self.issuing_language_node_id.clone(),

            process_work: self.process_work.clone(),
            started_process_ids: Arc::clone(&self.started_process_ids),
            nested_effect_error: Arc::clone(&self.nested_effect_error),
            incorporation_ledger: Arc::clone(&self.incorporation_ledger),
            opener_groups: Arc::clone(&self.opener_groups),
            tool_requests: Arc::clone(&self.tool_requests),
            cell_tool_calls: Arc::clone(&self.cell_tool_calls),
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

    pub fn trigger_store(&self) -> Option<Arc<dyn crate::TriggerStore>> {
        self.dispatch
            .trigger_router
            .as_ref()
            .map(crate::TriggerRouter::store)
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
    pub(crate) fn max_tool_calls(&self) -> crate::MaxToolCalls {
        self.execution_env_spec.policy.max_tool_calls
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
            .controller()
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
            .controller()
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
            .and_then(crate::RuntimeInvocation::effect_replay_key)
            .map(str::to_string)
        else {
            return Ok(false);
        };
        let observed = control
            .observe_pending_cancel(
                &self.dispatch.effect_controller,
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

    /// Bind the admitted logical Run whose cell may cross segment boundaries.
    pub fn with_logical_run(mut self, run: crate::TurnAddress) -> Self {
        self.logical_run = Some(run);
        self
    }

    /// The admitted logical Run authorized to resume this cell's continuation.
    pub fn logical_run(&self) -> Option<&crate::TurnAddress> {
        self.logical_run.as_ref()
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
            .transferable(self.transferable_waits)
        } else {
            crate::runtime::TurnCancelWait::unobserved(cancellation)
        }
    }

    /// Marks the durable waits this context issues as ones the Run's
    /// successor segment may take over (FIG-4739). A wait handed over answers
    /// [`TurnWaitHandedOver`](crate::RuntimeErrorCode::TurnWaitHandedOver)
    /// and stays open: the caller must hold captured state that issues the
    /// wait again.
    pub fn with_transferable_waits(mut self, transferable: bool) -> Self {
        self.transferable_waits = transferable;
        self
    }

    /// Says whether the turn this context executes for may end at a segment
    /// boundary inside the execution (FIG-4739): its engine moves turns off a
    /// draining build, and the turn has a shift to recover its continuation.
    /// Only such an execution marks a wait transferable.
    pub fn with_turn_hand_over(mut self, hands_over: bool) -> Self {
        self.turn_hands_over = hands_over;
        self
    }

    /// Whether the turn this context executes for may end at a segment
    /// boundary inside the execution.
    pub fn turn_hands_over(&self) -> bool {
        self.turn_hands_over
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

    pub(crate) fn process_event_context(&self) -> Option<&RuntimeExecutionProcessEventContext> {
        self.process_execution
            .as_ref()
            .and_then(|exec| exec.event_context.as_ref())
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
                wake_session_id: exec.wake_session_id.clone(),
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
        if registration.env_ref.is_some()
            || matches!(
                registration.input.as_ref(),
                crate::ProcessStartTarget::Input(crate::ProcessInput::External { .. })
            )
        {
            return Ok(registration);
        }
        let claim = execution_claim_of(self.dispatch.effect_controller.execution_scope())?;
        let env_ref = self.captured_process_execution_env_ref(&claim).await?;
        Ok(registration.with_execution_env_ref(Some(env_ref)))
    }

    /// Publish or acquire this execution's captured environment under `claim`
    /// before persisting its digest in a declaration. An ended referrer refuses
    /// acquisition; it cannot resurrect a reclaimed environment.
    pub async fn captured_process_execution_env_ref(
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
                self.record_started_process(&record.id);
                crate::ToolInvocationReply::success(Self::process_handle_json(&record.id))
            }
            Err(err) => crate::ToolInvocationReply::error(serde_json::json!(err.to_string())),
        }
    }

    /// Appends replay-scoped process events for code-executor implementors as
    /// one atomic batch (FIG-3571), in order, and returns the stored events.
    /// Each coordinated wake delivery the batch produced is enqueued after it
    /// commits.
    pub async fn append_process_events(
        &self,
        requests: Vec<crate::ProcessEventAppendRequest>,
    ) -> Result<Vec<crate::ProcessEvent>, crate::PluginError> {
        let exec = self
            .process_execution
            .as_ref()
            .ok_or_else(missing_process_execution_error)?;
        let context = exec
            .event_context
            .as_ref()
            .ok_or_else(missing_process_execution_error)?;
        let receipts = context
            .process_work
            .registry()
            .append_events(
                &exec.process_id,
                requests,
                &context.execution_write_authority,
            )
            .await?;
        let mut events = Vec::with_capacity(receipts.len());
        for receipt in receipts {
            crate::tool_provider::process_events::enqueue_wake_delivery(
                std::sync::Arc::clone(context.process_work.registry()),
                context.store.clone(),
                context.session_store_factory.as_ref(),
                receipt.wake_delivery,
                Some(self.session_graph_service()),
                Arc::clone(&context.queued_work),
                context.process_wake_delivery_policy,
                Arc::clone(&context.clock),
            )
            .await?;
            events.push(receipt.event);
        }
        Ok(events)
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
            self.dispatch.effect_controller.execution_scope(),
            &crate::runtime::command_invocation(
                self.dispatch.effect_controller.execution_scope(),
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
            .execute_effect(
                crate::RuntimeEffectEnvelope::new(
                    invocation,
                    crate::RuntimeEffectCommand::AwaitEvent { key },
                ),
                crate::RuntimeEffectLocalExecutor::await_event_under(
                    &self.turn_cancel_wait(cancellation),
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
            )
            .into_journaled()),
            crate::Resolution::Cancelled => Err(crate::RuntimeEffectControllerError::new(
                crate::RuntimeErrorCode::ProcessSignalWaitCancelled,
                "process signal wait was cancelled",
            )
            .into_journaled()),
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
        self.process_execution
            .as_ref()
            .and_then(|exec| exec.event_context.as_ref())
            .ok_or_else(missing_process_execution_error)?;
        let identity =
            crate::ProcessSignalIdentity::new(process_id.clone(), signal_name, signal_id)?;
        let command = crate::ProcessCommand::Signal {
            signal: crate::ProcessSignal::new(identity, payload),
        };
        let effect_id = command.effect_id();
        let invocation = crate::runtime::causal::process_effect_invocation(
            self.dispatch.effect_controller.execution_scope(),
            self.parent_invocation
                .as_ref()
                .map(|parent| parent.attribution.clone())
                .unwrap_or_else(crate::RuntimeAttribution::none),
            self.parent_invocation.clone(),
            &effect_id,
        );
        let controller = self.dispatch.effect_controller.controller();
        let scoped = self.dispatch.effect_controller.clone();
        #[expect(
            clippy::expect_used,
            reason = "`EffectTaskController::scoped` returns a proxy that owns the controller it was just built around"
        )]
        let (owned_controller, task_requests): (
            Arc<dyn crate::RuntimeEffectController>,
            Option<crate::runtime::effect::EffectControllerTaskRequests>,
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
        let registry = self
            .process_execution
            .as_ref()
            .and_then(|exec| exec.event_context.as_ref())
            .map(|context| Arc::clone(context.process_work.registry()))
            .ok_or_else(missing_process_execution_error)?;
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
            self.dispatch.process_engines.clone(),
            crate::runtime::HostStartAdmission::default(),
        )
        .with_process_attachments(Arc::clone(self.attachment_store.referrers()))
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
                self.dispatch.effect_controller.execution_scope().clone(),
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
                // A process's cancellation reaches its sleep only as the
                // engine's recorded race of the timer against the process's
                // cancel fact, reported as `RuntimeEffectSleepCancelled`
                // (FIG-3673); a live read here could answer differently on
                // redrive.
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
        let store = self.revision_referrer_trigger_store(store)?;
        #[expect(
            clippy::expect_used,
            reason = "the scope comes from the caller's own live effect controller, which is admitted by construction"
        )]
        let invocation = crate::RuntimeEffectInvocation::new(
            crate::EffectAddress::new(
                self.dispatch.effect_controller.execution_scope().clone(),
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
        if !matches!(
            draft.target,
            crate::ProcessStartTarget::Input(crate::ProcessInput::Engine { .. })
                | crate::ProcessStartTarget::Definition { .. }
        ) {
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
            if matches!(draft.target, crate::ProcessStartTarget::Definition { .. }) {
                return Err(crate::RuntimeEffectControllerError::foreign(
                    "process_definition_store_unavailable",
                    crate::TurnFailureCause::Outcome,
                    "trigger definition admission requires a process-engine registry",
                ));
            }
            return Ok(());
        };
        crate::admit_trigger_registration_target(registry, draft)
            .await
            .map_err(crate::RuntimeEffectControllerError::from)
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

    /// The originator a trigger command issued here acts as: the process's
    /// recorded originator, or the session frame itself.
    pub fn trigger_actor(&self) -> Result<crate::ProcessOriginator, crate::PluginError> {
        match self.process_execution.as_ref() {
            Some(exec) => Ok(exec.originator.clone()),
            None => Ok(crate::ProcessOriginator::session(self.session_scope()?)),
        }
    }

    pub fn trigger_owner_scope(&self) -> Result<crate::TriggerOwnerScope, crate::PluginError> {
        resolve_trigger_owner_scope(
            &self.dispatch.owner.runtime_owner(),
            self.process_execution.as_ref().map(|exec| &exec.originator),
        )
    }

    /// Where a registration's deliveries wake: the process's recorded wake
    /// target, else the session frame. A process with no wake target wakes
    /// nothing.
    pub fn trigger_registration_wake_target(&self) -> Option<crate::SessionScope> {
        match self.process_execution.as_ref() {
            Some(exec) => exec.wake_session_id.as_ref().map(crate::SessionScope::new),
            None => self.session_scope().ok(),
        }
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
