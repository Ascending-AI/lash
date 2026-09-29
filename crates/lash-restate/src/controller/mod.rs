//! Handler-scoped runtime-effect controller.
//!
//! One responsibility: map a Lash runtime effect onto the Restate journal
//! command that makes it durable — a `ctx.run` for journaled effects, a durable
//! timer for sleeps, a durable-wait call for await-events, and direct workflow
//! scheduling for process commands. The context seam those commands are issued
//! through lives in [`context`].

use lash_sansio::SessionId;
pub(crate) mod context;
pub(crate) mod effect_journal;
mod group_child_cancel;
mod group_commit;
use group_child_cancel::group_child_cancelled;
mod group_read;
pub(crate) mod journal_budget;
mod journaled_effect;
use journaled_effect::EngineFaults;
mod live_frontier;
mod scope_recording;
mod scoped;
mod turn_cancel_request;
mod turn_gate;
pub(crate) use turn_cancel_request::{
    restate_await_event_turn_cancel_wait_request, restate_timer_turn_cancel_wait_request,
};
use turn_cancel_request::{
    restate_group_turn_cancel_wait_request, restate_process_turn_cancel_wait_request,
};

use lash_core::facade_support::trace_context_for_runtime_effect_invocation;
use std::fmt;
use std::marker::PhantomData;
use std::sync::{Arc, Mutex, Weak};
use std::time::Duration;

use lash_core::{
    AwaitEventKey, AwaitEventResolver, AwaitEventWaitIdentity, CompletionKeyPreparation,
    EffectGroupHandle, EffectHost, ExecutionScope, GroupSettlement, LoserPolicy, PluginError,
    ProcessCommand, ProcessEffectOutcome, ProcessExternalRef, ProcessRecord, ProcessRegistry,
    RankedGroupSettlement, Resolution, ResolveOutcome, RuntimeEffectCommand,
    RuntimeEffectController, RuntimeEffectControllerError, RuntimeEffectEnvelope,
    RuntimeEffectGroup, RuntimeEffectInvocation, RuntimeEffectLocalExecutor, RuntimeEffectOutcome,
    RuntimeError, RuntimeErrorCode, ScopedEffectController, SleepSpec,
    facade_support::RuntimeAwaitEventOptions, facade_support::RuntimeSleepOptions,
    facade_support::refuse_unhonored_group_membership,
};
use restate_sdk::context::RunRetryPolicy;
use restate_sdk::errors::TerminalError;
use restate_sdk::serde::Json;

use crate::durable_wait::{
    RestateDurableWaitAddress, RestateDurableWaitResolveRequest, RestateTurnCancelRaceOutcome,
    restate_await_event_key_for_authority, restate_await_event_key_is_valid_for_authority,
    restate_unknown_or_revoked,
};
use crate::effect_group::{
    EffectGroupCloseDisposition, EffectGroupCloseRequest, EffectGroupCloseResponse,
    EffectGroupDispatchRequest, EffectGroupOpenRequest, EffectGroupOpenResponse,
    EffectGroupProbeResponse, EffectGroupShape, EffectGroupWaitResolution, decode_wait_resolution,
    group_shape_error, ready_wait_request,
};
use crate::ingress::RestateAuthorityId;
use crate::process::RestateProcessCancelRequest;
use context::journaled_restate_durable_wait_request;
pub(crate) use live_frontier::LiveFrontier;

pub use context::RestateControllerContext;

struct RestateTraceObserver {
    sink: Weak<dyn lash_trace::TraceSink>,
    base_context: lash_trace::TraceContext,
    current_context: Mutex<Option<lash_trace::TraceContext>>,
}

/// Configuration for [`RestateRuntimeEffectController`].
#[derive(Clone)]
pub struct RestateEffectControllerOptions {
    run_retry_policy: Option<RunRetryPolicy>,
    segment_effect_budget: u64,
    journaled_effect_byte_budget: Option<u64>,
    /// The §7 drain budget: on the SQL tiers it bounds how long group
    /// finalization waits on a cancel-decided child's attempt body after its
    /// decision commits. On Restate there is no such wait — the engine's own
    /// cancellation already abandons a cancelled child's drive, so the bound
    /// is vacuous here — but the option is held so the three tiers carry one
    /// construction-level vocabulary (FIG-3410).
    drain_budget: Duration,
    /// Whether this controller drives a process segment, whose waits that
    /// observe no turn race the segment's durable cancel promise and whose
    /// cancel peeks read it (FIG-3673).
    process_cancel: context::ProcessCancelRace,
    /// The generation that admitted the segment this controller drives: its
    /// signal waits also race the segment's hand-over promise, and hand over
    /// to a drain wake naming this generation (FIG-3799). Shared, so every
    /// controller future that holds the options stays a pointer wider.
    segment_generation: Option<Arc<lash_core::engine::BuildGeneration>>,
    /// The cancel fact of the effect-group child this controller drives
    /// (FIG-3904): a wait that observes no turn races it as a journaled arm,
    /// [`observe_group_child_cancel`](RuntimeEffectController::observe_group_child_cancel)
    /// is a journaled peek of it, and a recorded step body watches it live.
    /// Shared for the same pointer-width reason `segment_generation` is.
    group_child_cancel: Option<Arc<crate::effect_group::GroupChildCancel>>,
}

impl Default for RestateEffectControllerOptions {
    fn default() -> Self {
        Self {
            run_retry_policy: None,
            segment_effect_budget: 10_000,
            journaled_effect_byte_budget: None,
            drain_budget: lash_core::EffectGroupDrainBudget::DEFAULT.duration(),
            process_cancel: context::ProcessCancelRace::NotRaced,
            segment_generation: None,
            group_child_cancel: None,
        }
    }
}

impl RestateEffectControllerOptions {
    pub fn new() -> Self {
        Self::default()
    }

    /// Set a Restate retry policy for recorded `ctx.run` effects.
    ///
    /// Lash provider/tool errors are recorded as Lash data, so this policy is
    /// used only when the recorded closure itself fails before producing a
    /// serializable effect result.
    pub fn run_retry_policy(mut self, policy: RunRetryPolicy) -> Self {
        self.run_retry_policy = Some(policy);
        self
    }

    /// Set the deterministic maximum number of completed effects in one
    /// Restate invocation. Replay observes the same progress and cuts at the
    /// same post-effect point.
    pub fn segment_effect_budget(mut self, effects: u64) -> Self {
        self.segment_effect_budget = effects.max(1);
        self
    }

    /// Refuse to journal a recorded effect whose payload exceeds `bytes`.
    ///
    /// An effect outcome the engine will not accept fails the same way on every
    /// redrive, which leaves the turn uncommitted forever. Deciding the same
    /// verdict here instead turns that poison into a terminal effect failure the
    /// host can see. Set this at or below the deployment's Restate journal-entry
    /// limit; unset, only outcomes that cannot be serialized at all are refused.
    ///
    /// # Enabling or disabling this is a drain-only config change
    ///
    /// An effect that must run outside the run closure - a durable process
    /// command - journals its budget verdict in a slot of its own
    /// ahead of the effect, so that a redrive honours the recorded give-up
    /// instead of running the effect again. That slot exists only while a budget
    /// is configured, so turning the budget on or off changes the journal's slot
    /// sequence: drain in-flight invocations across such a change. An invocation
    /// that spans the toggle replays against a sequence it was not recorded
    /// with, which Restate reports as a journal mismatch - a loud terminal
    /// failure for that invocation, never a silent re-execution or a wrong
    /// result.
    ///
    /// Changing the *value* is safe at any time, in either direction: the
    /// verdict is decided from the budget in force when it was journaled and
    /// replays from the journal, so the slot sequence never depends on the
    /// number.
    pub fn journaled_effect_byte_budget(mut self, bytes: u64) -> Self {
        self.journaled_effect_byte_budget = Some(bytes);
        self
    }

    /// Set the group drain budget (ADR 0099 §7): the bound a group's
    /// finalization waits on a cancel-decided child's attempt body after its
    /// decision commits.
    ///
    /// On this tier the bound is **vacuous** and is held for cross-tier parity
    /// only: engine cancellation abandons the cancelled child's drive itself,
    /// so no host-side wait exists for it to bound. The value is journaled
    /// nowhere and changing it never changes a committed obligation.
    pub fn drain_budget(mut self, budget: lash_core::EffectGroupDrainBudget) -> Self {
        self.drain_budget = budget.duration();
        self
    }

    /// Mark the controller as a process segment's drive (FIG-3673): a wait
    /// it issues that observes no turn races the segment's durable cancel
    /// promise, and [`observe_process_cancel`](RuntimeEffectController::observe_process_cancel)
    /// is a journaled peek of that promise. Only the process workflow sets
    /// it, on the workflow context whose promise it is.
    pub(crate) fn process_segment_drive(mut self) -> Self {
        self.process_cancel = context::ProcessCancelRace::Raced;
        self
    }

    /// Mark a process segment's drive as admitted under `generation`
    /// (FIG-3799): its signal waits race the segment's hand-over promise as
    /// well as its cancel promise, and hand the wait to a successor when the
    /// drain wakes this generation. Only the process workflow sets it.
    pub(crate) fn segment_generation(
        mut self,
        generation: lash_core::engine::BuildGeneration,
    ) -> Self {
        self.segment_generation = Some(Arc::new(generation));
        self
    }
}

impl fmt::Debug for RestateEffectControllerOptions {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RestateEffectControllerOptions")
            .field("run_retry_policy", &self.run_retry_policy)
            .field("segment_effect_budget", &self.segment_effect_budget)
            .field(
                "journaled_effect_byte_budget",
                &self.journaled_effect_byte_budget,
            )
            .field("drain_budget", &self.drain_budget)
            .field("process_cancel", &self.process_cancel)
            .field("segment_generation", &self.segment_generation)
            .field("group_child_cancel", &self.group_child_cancel)
            .finish()
    }
}
pub use effect_journal::EFFECT_JOURNAL_VERSION;
pub(crate) use effect_journal::RecordedRuntimeEffect;

/// Error raised while bridging a Lash effect to Restate.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum RestateEffectError {
    #[error("Restate terminal error while running `{effect}`: {terminal}")]
    Terminal {
        effect: String,
        terminal: TerminalError,
    },
    /// The journal slot holds an entry this build refuses to replay, such as
    /// one another effect-journal generation wrote.
    #[error(transparent)]
    Refused(RuntimeEffectControllerError),
}

impl From<RestateEffectError> for RuntimeEffectControllerError {
    fn from(error: RestateEffectError) -> Self {
        match error {
            RestateEffectError::Terminal { .. } => {
                Self::new(RuntimeErrorCode::EngineEffectController, error.to_string())
            }
            RestateEffectError::Refused(refusal) => refusal,
        }
    }
}

async fn resolve_restate_await_event<'ctx, C>(
    context: &C,
    namespace: &crate::RestateNamespace,
    key: &AwaitEventKey,
    resolution: Resolution,
) -> Result<ResolveOutcome, RuntimeError>
where
    C: RestateControllerContext<'ctx> + ?Sized,
{
    context
        .resolve_event(
            namespace,
            RestateDurableWaitResolveRequest {
                key: key.clone(),
                resolution,
            },
        )
        .await
        .map_err(|err| {
            RuntimeError::new(
                lash_core::RuntimeErrorCode::EngineEffectController,
                err.to_string(),
            )
        })?
        .into_result()
}
/// Lash [`RuntimeEffectController`] and [`EffectHost`] backed by a Restate handler context.
///
/// This type is intentionally handler-scoped.
pub struct RestateRuntimeEffectController<'ctx, C> {
    context: C,
    authority_id: RestateAuthorityId,
    options: RestateEffectControllerOptions,
    trace: Option<RestateTraceObserver>,
    /// The drain generation of the build whose lash handler runs this
    /// controller (FIG-3795): an effect group it opens dispatches on that
    /// build's lane, so the group's children run on the build that opened
    /// it. A controller a host builds inside its own handler names none, and
    /// its groups dispatch on the stable lane.
    build_generation: Option<lash_core::engine::BuildGeneration>,
    /// The generation sentinel its first recorded entry carries (FIG-3980).
    folded_sentinel: Option<Arc<crate::sentinel::FoldedSentinel>>,
    /// The namespace of the deployment whose services this controller calls
    /// (FIG-3898): the durable waits, process workflow and effect groups it
    /// addresses are that namespace's.
    namespace: crate::RestateNamespace,
    /// The ranks this controller's run reads served (FIG-4088).
    read_ahead: group_read::GroupReadAhead,
    _ctx: PhantomData<&'ctx ()>,
}

impl<'ctx, C> RestateRuntimeEffectController<'ctx, C> {
    pub fn new(context: C, authority_id: RestateAuthorityId) -> Self {
        Self::with_options(
            context,
            authority_id,
            RestateEffectControllerOptions::default(),
        )
    }

    pub fn with_options(
        context: C,
        authority_id: RestateAuthorityId,
        options: RestateEffectControllerOptions,
    ) -> Self {
        Self {
            context,
            authority_id,
            options,
            trace: None,
            build_generation: None,
            folded_sentinel: None,
            namespace: crate::RestateNamespace::default(),
            read_ahead: group_read::GroupReadAhead::default(),
            _ctx: PhantomData,
        }
    }

    /// Call the lash services of the deployment in `namespace` (FIG-3898).
    /// A controller a host builds inside its own handler calls its own
    /// deployment's namespace; unset, the default namespace's.
    pub fn in_namespace(mut self, namespace: crate::RestateNamespace) -> Self {
        self.namespace = namespace;
        self
    }

    /// The namespace this controller's calls address.
    pub fn namespace(&self) -> &crate::RestateNamespace {
        &self.namespace
    }

    /// Run as a controller of the build of `generation`: the groups it
    /// opens dispatch on that build's `EffectGroupDispatch` lane and the
    /// processes it starts carry the generation as their sender (FIG-3795).
    /// Lash's own handlers set it; a host's never do.
    pub(crate) fn with_build_generation(
        mut self,
        generation: lash_core::engine::BuildGeneration,
    ) -> Self {
        self.build_generation = Some(generation);
        self
    }

    #[cfg(test)]
    pub(crate) fn new_for_test(context: C) -> Self {
        Self::new(
            context,
            RestateAuthorityId::new("lash-restate-tests").expect("valid test authority"),
        )
    }

    #[cfg(test)]
    pub(crate) fn with_options_for_test(
        context: C,
        options: RestateEffectControllerOptions,
    ) -> Self {
        Self::with_options(
            context,
            RestateAuthorityId::new("lash-restate-tests").expect("valid test authority"),
            options,
        )
    }

    /// Observe durable steps through a non-owning sink handle.
    ///
    /// Trace append is deliberately best-effort and never crosses the Restate
    /// context seam: the journal remains truth and tracing remains a live
    /// observation that may be repeated during handler redrive.
    pub fn with_trace_sink(mut self, sink: Arc<dyn lash_trace::TraceSink>) -> Self {
        self.trace = Some(RestateTraceObserver {
            sink: Arc::downgrade(&sink),
            base_context: lash_trace::TraceContext::default(),
            current_context: Mutex::new(None),
        });
        self
    }

    /// Observe durable steps while retaining the host's trace context.
    pub fn with_trace_sink_and_context(
        mut self,
        sink: Arc<dyn lash_trace::TraceSink>,
        base_context: lash_trace::TraceContext,
    ) -> Self {
        self.trace = Some(RestateTraceObserver {
            sink: Arc::downgrade(&sink),
            base_context,
            current_context: Mutex::new(None),
        });
        self
    }

    pub fn context(&self) -> &C {
        &self.context
    }

    pub fn options(&self) -> &RestateEffectControllerOptions {
        &self.options
    }

    fn emit_trace(
        &self,
        invocation: Option<&RuntimeEffectInvocation>,
        event: impl FnOnce() -> lash_trace::TraceEvent,
    ) {
        let Some(trace) = self.trace.as_ref() else {
            return;
        };
        let Some(sink) = trace.sink.upgrade() else {
            return;
        };
        let context = if let Some(invocation) = invocation {
            let context =
                trace_context_for_runtime_effect_invocation(trace.base_context.clone(), invocation);
            *trace
                .current_context
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(context.clone());
            context
        } else {
            trace
                .current_context
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .clone()
                .unwrap_or_else(|| trace.base_context.clone())
        };
        if let Err(error) = sink.append(&lash_trace::TraceRecord::new(context, event())) {
            tracing::warn!(%error, "failed to append Restate durable-step trace record");
        }
    }

    fn remember_trace_invocation(&self, invocation: &RuntimeEffectInvocation) {
        let Some(trace) = self.trace.as_ref() else {
            return;
        };
        if trace.sink.upgrade().is_none() {
            return;
        }
        *trace
            .current_context
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(
            trace_context_for_runtime_effect_invocation(trace.base_context.clone(), invocation),
        );
    }
}

impl<C> fmt::Debug for RestateRuntimeEffectController<'_, C> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RestateRuntimeEffectController")
            .field("options", &self.options)
            .finish_non_exhaustive()
    }
}

#[async_trait::async_trait]
impl<'ctx, C> AwaitEventResolver for RestateRuntimeEffectController<'ctx, C>
where
    C: RestateControllerContext<'ctx>,
{
    fn await_event_authority_binding_id(&self) -> Option<String> {
        Some(self.authority_id.binding_id().to_string())
    }

    async fn prepare_completion_key(
        &self,
        scope: &ExecutionScope,
        wait: AwaitEventWaitIdentity,
        may_defer: bool,
    ) -> Result<CompletionKeyPreparation, RuntimeError> {
        if !may_defer {
            return Ok(CompletionKeyPreparation::NotNeeded);
        }
        self.await_event_key(scope, wait)
            .await
            .map(CompletionKeyPreparation::Issued)
    }

    async fn await_event_key(
        &self,
        scope: &ExecutionScope,
        wait: AwaitEventWaitIdentity,
    ) -> Result<AwaitEventKey, RuntimeError> {
        scope.validate()?;
        restate_await_event_key_for_authority(&self.authority_id, scope, wait)
    }

    async fn resolve_await_event(
        &self,
        key: &AwaitEventKey,
        resolution: Resolution,
    ) -> Result<ResolveOutcome, RuntimeError> {
        if !restate_await_event_key_is_valid_for_authority(&self.authority_id, key) {
            return Ok(ResolveOutcome::UnknownOrRevoked);
        }
        resolve_restate_await_event(&self.context, &self.namespace, key, resolution).await
    }

    async fn publish_await_event(
        &self,
        key: &AwaitEventKey,
        resolution: Resolution,
    ) -> Result<Option<ResolveOutcome>, RuntimeError> {
        self.publish_resolve(key, resolution).await
    }

    async fn peek_await_event(
        &self,
        key: &AwaitEventKey,
    ) -> Result<Option<Resolution>, RuntimeError> {
        if !restate_await_event_key_is_valid_for_authority(&self.authority_id, key) {
            return Err(restate_unknown_or_revoked());
        }
        if turn_gate::is_turn_cancel_gate(key) {
            if let Some(resolution) = self.mirrored_turn_gate(key).await? {
                return Ok(Some(resolution));
            }
        } else {
            self.require_active_session(key.scope.session_id()).await?;
        }
        self.context
            .peek_event(
                &self.namespace,
                RestateDurableWaitAddress::for_key(key),
                key.key_id.clone(),
            )
            .await
            .map_err(|err| {
                RuntimeError::new(
                    lash_core::RuntimeErrorCode::EngineEffectController,
                    err.to_string(),
                )
            })
    }

    async fn await_await_event(
        &self,
        key: &AwaitEventKey,
        cancel: tokio_util::sync::CancellationToken,
        deadline: Option<std::time::Instant>,
    ) -> Result<Resolution, RuntimeError> {
        if !restate_await_event_key_is_valid_for_authority(&self.authority_id, key) {
            return Err(restate_unknown_or_revoked());
        }
        self.require_active_session(key.scope.session_id()).await?;
        let replay_key = key.key_id.clone();
        let request = journaled_restate_durable_wait_request(
            &self.context,
            key,
            deadline,
            crate::system_clock(),
        )
        .await
        .map_err(|err| {
            RuntimeError::new(
                lash_core::RuntimeErrorCode::EngineEffectController,
                err.to_string(),
            )
        })?;
        self.context
            .await_event(&self.namespace, request, replay_key, cancel)
            .await
            .map_err(|err| {
                RuntimeError::new(
                    lash_core::RuntimeErrorCode::EngineEffectController,
                    err.to_string(),
                )
            })
    }

    async fn revoke_await_events_for_session(
        &self,
        session_id: &SessionId,
    ) -> Result<(), RuntimeError> {
        self.context
            .update_session_waits(
                &self.namespace,
                SessionId::from(session_id.to_string()),
                true,
            )
            .await
            .map_err(|err| {
                RuntimeError::new(
                    lash_core::RuntimeErrorCode::EngineEffectController,
                    err.to_string(),
                )
            })
    }

    async fn cancel_await_events_for_session(
        &self,
        session_id: &SessionId,
    ) -> Result<(), RuntimeError> {
        self.context
            .update_session_waits(
                &self.namespace,
                SessionId::from(session_id.to_string()),
                false,
            )
            .await
            .map_err(|err| {
                RuntimeError::new(
                    lash_core::RuntimeErrorCode::EngineEffectController,
                    err.to_string(),
                )
            })
    }
}

#[async_trait::async_trait]
impl<'ctx, C> EffectHost for RestateRuntimeEffectController<'ctx, C>
where
    C: RestateControllerContext<'ctx> + Sync,
{
    fn turn_control_binding_id(&self) -> String {
        self.authority_id.binding_id().to_string()
    }

    fn await_event_resolver(&self) -> &dyn lash_core::AwaitEventResolver {
        self
    }

    fn scoped<'run>(
        &'run self,
        admitted: lash_core::AdmittedScope,
    ) -> Result<ScopedEffectController<'run>, RuntimeError> {
        self.scoped_effect_controller(admitted)
    }

    async fn prepare_tool_intent(
        &self,
        _sink: &dyn lash_core::ToolIntentOutcomeSink,
        _identity: &lash_core::ToolIntentIdentity,
        _intent: lash_core::ToolIntent,
    ) -> Result<lash_core::ToolIntentPreparation, RuntimeError> {
        Ok(lash_core::ToolIntentPreparation::ControllerOwned)
    }

    async fn record_tool_intent_outcome(
        &self,
        sink: &dyn lash_core::ToolIntentOutcomeSink,
        identity: &lash_core::ToolIntentIdentity,
        submitted: lash_core::ToolIntent,
        outcome: lash_core::ToolIntentExecutionOutcome,
    ) -> Result<(), RuntimeError> {
        sink.retain_in_journal(identity, submitted, outcome).await
    }

    /// A handler-scoped controller runs inside one journal and cannot see
    /// whether another will replay, so it never promises one settled
    /// (ADR 0113 §2.5); the deployment host answers the cleanup executor.
    async fn journal_replay(
        &self,
        _journal: &lash_sansio::EffectJournalIdentity,
    ) -> Result<lash_core::JournalReplay, RuntimeError> {
        Ok(lash_core::JournalReplay::MayReplay)
    }
}

impl<'ctx, C> RestateRuntimeEffectController<'ctx, C>
where
    C: RestateControllerContext<'ctx>,
{
    /// A process segment's signal wait (FIG-3673, FIG-3799): the event
    /// raced against the segment's cancel promise and the drain's hand-over.
    /// A hand-over is not a wait outcome the body sees: it answers
    /// [`RuntimeErrorCode::ProcessSignalWaitHandedOver`], on which the body
    /// stops at the wait and the segment hands it to its successor.
    async fn await_segment_signal(
        &self,
        invocation: &RuntimeEffectInvocation,
        request: crate::durable_wait::RestateDurableWaitAwaitRequest,
        replay_key: String,
        generation: lash_core::engine::BuildGeneration,
    ) -> Result<RuntimeEffectOutcome, RuntimeEffectControllerError> {
        let outcome = self
            .context
            .await_signal_or_segment_end(&self.namespace, request, replay_key, generation.clone())
            .await
            .map_err(|err| {
                self.emit_trace(Some(invocation), || {
                    lash_trace::TraceEvent::DurableWaitResolved {
                        wait_kind: "await_event".to_string(),
                        resolution: lash_trace::TraceDurableWaitResolution::Failed,
                    }
                });
                RuntimeEffectControllerError::new(
                    RuntimeErrorCode::EngineEffectController,
                    err.to_string(),
                )
            })?;
        match outcome {
            RestateTurnCancelRaceOutcome::Completed(context::SignalWaitOutcome::Resolved(
                resolution,
            )) => {
                self.emit_trace(Some(invocation), || {
                    lash_trace::TraceEvent::DurableWaitResolved {
                        wait_kind: "await_event".to_string(),
                        resolution: resolution_trace_label(&resolution),
                    }
                });
                Ok(RuntimeEffectOutcome::AwaitEvent { resolution })
            }
            RestateTurnCancelRaceOutcome::ProcessCancelled => {
                self.emit_trace(Some(invocation), || {
                    lash_trace::TraceEvent::DurableWaitResolved {
                        wait_kind: "await_event".to_string(),
                        resolution: lash_trace::TraceDurableWaitResolution::Cancelled,
                    }
                });
                Ok(RuntimeEffectOutcome::AwaitEvent {
                    resolution: Resolution::Cancelled,
                })
            }
            RestateTurnCancelRaceOutcome::Completed(context::SignalWaitOutcome::HandedOver) => {
                tracing::info!(
                    target: "lash::restate",
                    event = "restate.signal_wait_handed_over",
                    generation = generation.as_str(),
                    "a process segment's signal wait was handed over to its successor"
                );
                Err(RuntimeEffectControllerError::new(
                    RuntimeErrorCode::ProcessSignalWaitHandedOver,
                    format!(
                        "the drain of generation {} handed this signal wait to a successor segment",
                        generation.as_str()
                    ),
                ))
            }
            RestateTurnCancelRaceOutcome::TurnCancelled
            | RestateTurnCancelRaceOutcome::SessionRevoked { .. } => {
                Err(RuntimeEffectControllerError::new(
                    RuntimeErrorCode::EngineEffectController,
                    "a process signal wait observes no turn",
                ))
            }
        }
    }

    /// Opens `group` on behalf of `opener`, the admitted scope of the
    /// controller the group is opened through: the shape records it, and
    /// every child the dispatcher runs is admitted from it (FIG-3780).
    pub(crate) async fn open_effect_group_opened_by(
        &self,
        group: RuntimeEffectGroup,
        opener: &lash_core::AdmittedScope,
    ) -> Result<EffectGroupHandle, RuntimeEffectControllerError> {
        group.validate_execution_scope(opener.scope())?;
        let group_key = group.group_key().to_string();
        // An open (or a reopen) starts the caller's reads from the index.
        self.read_ahead.opened(&group_key);
        let handle = EffectGroupHandle::new(&group);
        let (shape, membership) = EffectGroupShape::from_group(&group, opener)?;
        // The route the dispatch is sent under is data (FIG-3795 S10): the
        // opener declares its own build's lane to the index, which retains
        // it, and the submit below goes to the recorded route the open
        // response reports — a reopen's retained route wins over the one
        // offered here. Every child the dispatcher sends goes to that same
        // lane, so the group runs on the build that opened it.
        let dispatch_route = self
            .namespace
            .own_or_stable(
                crate::LashService::EffectGroupDispatch,
                self.build_generation.as_ref(),
            )
            .name()
            .into_owned();
        let open_request = EffectGroupOpenRequest {
            shape,
            membership,
            dispatch_route: dispatch_route.clone(),
            content_checked: group.reopen() == lash_core::GroupReopen::RetainedContent,
        };
        self.refuse_over_budget_group_open(group.invocation(), &open_request)
            .await?;
        let shape = &open_request.shape;
        let probe = self
            .context
            .effect_group_probe(&self.namespace, group_key.clone())
            .await
            .map_err(|error| effect_group_engine_error("EffectGroupIndex/probe", error))?;
        if matches!(probe, EffectGroupProbeResponse::Absent)
            && let Some(position) = self
                .context
                .effect_group_preflight(
                    group_key.clone(),
                    group.children().to_vec(),
                    dispatch_route.clone(),
                )
                .await
                .map_err(|error| {
                    effect_group_engine_error("EffectGroupDispatch/preflight", error)
                })?
        {
            let replay_key = shape.replay_keys.get(position).ok_or_else(|| {
                group_shape_error(format!(
                    "effect group {group_key} preflight named child {position}, outside the {} children its shape carries",
                    shape.replay_keys.len()
                ))
            })?;
            return Err(group_shape_error(format!(
                "effect group {group_key} child {position} ({replay_key}) has no registered executor; refusing before group state is created"
            )));
        }
        let opened = self
            .context
            .effect_group_open(&self.namespace, group_key.clone(), open_request.clone())
            .await
            .map_err(|error| effect_group_engine_error("EffectGroupIndex/open", error))?;
        match opened {
            EffectGroupOpenResponse::OpenedFresh { dispatch_route }
            | EffectGroupOpenResponse::ReopenedPreparing { dispatch_route } => {
                self.context
                    .effect_group_submit(
                        EffectGroupDispatchRequest {
                            group_key: group_key.clone(),
                        },
                        dispatch_route,
                    )
                    .await
                    .map_err(|error| effect_group_engine_error("EffectGroupDispatch/run", error))?;
                let request = ready_wait_request(&shape.wait_scope, &group_key)?;
                let resolution = match self
                    .context
                    .await_effect_group_wait(
                        &self.namespace,
                        request,
                        group_key.clone(),
                        None,
                        context::ProcessCancelRace::NotRaced,
                    )
                    .await
                    .map_err(|error| {
                        effect_group_engine_error(
                            "LashDurableWaitWorkflow/await_resolution(READY)",
                            error,
                        )
                    })? {
                    RestateTurnCancelRaceOutcome::Completed(resolution) => resolution,
                    RestateTurnCancelRaceOutcome::TurnCancelled
                    | RestateTurnCancelRaceOutcome::ProcessCancelled
                    | RestateTurnCancelRaceOutcome::SessionRevoked { .. } => {
                        return Err(group_shape_error(format!(
                            "opening effect group {group_key} was cancelled while awaiting READY"
                        )));
                    }
                };
                match decode_wait_resolution(resolution)? {
                    EffectGroupWaitResolution::Ready => Ok(handle),
                    EffectGroupWaitResolution::Refused { reason } => Err(group_shape_error(
                        format!("effect group {group_key} routing was refused: {reason:?}"),
                    )),
                    EffectGroupWaitResolution::Retired => Err(group_shape_error(format!(
                        "effect group {group_key} was retired before it became ready"
                    ))),
                    other => Err(group_shape_error(format!(
                        "effect group {group_key} READY wait resolved as {other:?}"
                    ))),
                }
            }
            EffectGroupOpenResponse::ReopenedReady => Ok(handle),
            EffectGroupOpenResponse::ReopenedClosed { effective } => match effective {
                EffectGroupCloseDisposition::Refused { reason } => Err(group_shape_error(format!(
                    "effect group {group_key} routing was refused: {reason:?}"
                ))),
                EffectGroupCloseDisposition::RunToCompletion
                | EffectGroupCloseDisposition::Cancel => Ok(handle),
            },
            EffectGroupOpenResponse::Retired => Err(group_shape_error(format!(
                "effect group {group_key} is retired"
            ))),
            EffectGroupOpenResponse::ShapeMismatch if open_request.content_checked => Err(
                crate::effect_group::content_checked_shape_mismatch(&group_key),
            ),
            EffectGroupOpenResponse::ShapeMismatch => Err(group_shape_error(format!(
                "effect group {group_key} was reopened with a different durable shape"
            ))),
            // The engine-neutral divergence, exactly as a recorded run whose
            // envelope drifted reports it: the turn parks.
            EffectGroupOpenResponse::ContentMismatch { position } => {
                Err(crate::effect_group::content_mismatch(&group_key, position))
            }
        }
    }
}

#[async_trait::async_trait]
impl<'ctx, C> RuntimeEffectController for RestateRuntimeEffectController<'ctx, C>
where
    C: RestateControllerContext<'ctx>,
{
    fn owns_commit_backpressure(&self) -> bool {
        true
    }

    /// A bare controller admits a group under the group's own scope: a
    /// process scope names its minted id, so there is nothing to pin.
    async fn open_effect_group(
        &self,
        group: RuntimeEffectGroup,
    ) -> Result<EffectGroupHandle, RuntimeEffectControllerError> {
        let opener = lash_core::AdmittedScope::new(group.invocation().execution_scope().clone());
        self.open_effect_group_opened_by(group, &opener).await
    }

    async fn await_next_settlement(
        &self,
        handle: &mut EffectGroupHandle,
        cancel: lash_core::TurnCancelWait,
    ) -> Result<GroupSettlement, RuntimeEffectControllerError> {
        group_read::await_next_settlement(self, handle, cancel).await
    }

    /// The cursorless rank read the §6 incorporation record needs (ADR 0099
    /// §8); the body lives in [`group_read`] for the file-size budget.
    async fn read_group_settlement(
        &self,
        group_key: &str,
        rank: u64,
    ) -> Result<Option<RankedGroupSettlement>, RuntimeEffectControllerError> {
        group_read::read_group_settlement(self, group_key, rank).await
    }

    async fn close_effect_group(
        &self,
        handle: EffectGroupHandle,
        disposition: LoserPolicy,
    ) -> Result<(), RuntimeEffectControllerError> {
        let group_key = handle.group_key().to_string();
        // A closed group answers its caller from the index again: a rank
        // read ahead before the close is not served past it.
        self.read_ahead.closed(&group_key);
        let response = self
            .context
            .effect_group_close(
                &self.namespace,
                group_key.clone(),
                EffectGroupCloseRequest { disposition },
            )
            .await
            .map_err(|error| effect_group_engine_error("EffectGroupIndex/close", error))?;
        match response {
            EffectGroupCloseResponse::Closed | EffectGroupCloseResponse::AlreadyClosed => Ok(()),
            EffectGroupCloseResponse::WidenRefused => Err(group_shape_error(format!(
                "effect group {group_key} close attempted to widen its declared loser disposition"
            ))),
            EffectGroupCloseResponse::NotReady => Err(group_shape_error(format!(
                "effect group {group_key} cannot close before registration"
            ))),
            EffectGroupCloseResponse::UnknownGroup => Err(group_shape_error(format!(
                "effect group {group_key} is unknown"
            ))),
            EffectGroupCloseResponse::Retired => Err(group_shape_error(format!(
                "effect group {group_key} is retired"
            ))),
        }
    }

    /// The §4 boundary, routed through the durable membership record: the
    /// child's own scope index answers which group owns its replay key, and
    /// that group's index takes the commit. The serialized object handler —
    /// not any state this controller holds — is the linearization point, so
    /// a cancel decision racing the commit is fenced inside the index. The
    /// Restate index does not retain `drain_input`: the durable publication
    /// obligation is the committed-but-unseated child plus the dispatch
    /// workflow's own redrive, so `AlreadyCommitted` reports it `None`.
    async fn commit_group_child_final(
        &self,
        commit: lash_core::facade_support::GroupChildFinalCommit,
    ) -> Result<
        lash_core::facade_support::EffectGroupChildCommitOutcome,
        RuntimeEffectControllerError,
    > {
        group_commit::commit_group_child_final(&self.context, &self.namespace, commit).await
    }

    async fn await_group_child_drain_admission(
        &self,
        group_key: &str,
        commit_seq: u64,
    ) -> Result<(), RuntimeEffectControllerError> {
        group_commit::await_group_child_drain_admission(
            &self.context,
            &self.namespace,
            group_key,
            commit_seq,
        )
        .await
    }

    /// Restate replays the invocation journal by position and compares each
    /// entry's run name as it goes (JOURNAL_MISMATCH 570): a command issued
    /// out of recorded order meets a recorded entry of another name before
    /// anything is dispatched, so that check is the recorded-frontier fence
    /// here (FIG-3586).
    async fn read_recorded_journal(
        &self,
        _range: &lash_core::RecordedKeyRange,
    ) -> Result<lash_core::RecordedJournal, RuntimeEffectControllerError> {
        Ok(lash_core::RecordedJournal::Positional)
    }

    /// A journaled peek of the segment workflow's own cancel promise, for a
    /// process segment's drive, which never reads `lent_stop`. Any other
    /// controller drives no process segment and records no process
    /// cancellation fact, so it answers from the stop (FIG-3673).
    async fn observe_process_cancel(
        &self,
        lent_stop: &tokio_util::sync::CancellationToken,
    ) -> Result<bool, RuntimeEffectControllerError> {
        match self.options.process_cancel {
            context::ProcessCancelRace::NotRaced => Ok(lent_stop.is_cancelled()),
            context::ProcessCancelRace::Raced => self
                .context
                .peek_process_cancel_requested()
                .await
                .map_err(|err| {
                    RuntimeEffectControllerError::new(
                        RuntimeErrorCode::EngineProcessCancel,
                        err.to_string(),
                    )
                }),
        }
    }

    /// A journaled peek of the bound group child's cancel fact (FIG-3904).
    async fn observe_group_child_cancel(&self) -> Result<bool, RuntimeEffectControllerError> {
        self.peek_group_child_cancel().await
    }

    fn group_child_cancel_watch(&self) -> Option<Arc<dyn lash_core::GroupChildCancelWatch>> {
        RestateRuntimeEffectController::group_child_cancel_watch(self)
    }

    /// One `ctx.run` step named `name`: its answer is journaled and replayed
    /// (FIG-3673). A retryable fault ends the attempt unrecorded; any other
    /// refusal is recorded as the step's answer.
    async fn record_process_drive_step(
        &self,
        name: String,
        step: lash_core::ProcessDriveStep<'_>,
    ) -> Result<(), RuntimeEffectControllerError> {
        let Json(recorded) = self
            .context
            .run_json_or_retry_send::<Result<(), PluginError>, _>(name, async move {
                match step.await {
                    Ok(()) => Ok(Ok(())),
                    Err(error) if error.is_retryable() => Err(error.to_string()),
                    Err(error) => Ok(Err(error)),
                }
            })
            .await
            .map_err(|err| {
                RuntimeEffectControllerError::new(
                    RuntimeErrorCode::EngineEffectController,
                    err.to_string(),
                )
            })?;
        recorded.map_err(RuntimeEffectControllerError::from)
    }

    fn wants_segment_boundary(
        &self,
        progress: &lash_core::SegmentProgress,
    ) -> Option<lash_core::BoundaryReason> {
        let reason = (progress.effects_executed >= self.options.segment_effect_budget)
            .then_some(lash_core::BoundaryReason::JournalBudget);
        if let Some(reason) = reason {
            self.emit_trace(None, || lash_trace::TraceEvent::DurableSegmentBoundary {
                reason: match reason {
                    lash_core::BoundaryReason::JournalBudget => "journal_budget",
                    lash_core::BoundaryReason::HandOver => "hand_over",
                }
                .to_string(),
                effects_executed: progress.effects_executed,
                journaled_bytes_estimate: progress.journaled_bytes_estimate,
            });
        }
        reason
    }

    async fn execute_effect(
        &self,
        envelope: RuntimeEffectEnvelope,
        local_executor: RuntimeEffectLocalExecutor<'_>,
    ) -> Result<RuntimeEffectOutcome, RuntimeEffectControllerError> {
        let execution = restate_effect_execution(envelope)?;
        self.remember_trace_invocation(execution.invocation());
        live_frontier::refuse_outside_a_run(&execution, &local_executor)?;
        match execution {
            RestateEffectExecution::DirectProcess {
                invocation,
                command,
            } => match execute_restate_process_command(
                &self.context,
                &self.namespace,
                &self.authority_id,
                self.build_generation.as_ref(),
                self.options.process_cancel,
                &invocation,
                *command,
                local_executor,
                |wait_kind| {
                    self.emit_trace(Some(&invocation), || {
                        lash_trace::TraceEvent::DurableWaitParked {
                            wait_kind: wait_kind.to_string(),
                        }
                    });
                },
                |wait_kind, resolution| {
                    self.emit_trace(Some(&invocation), || {
                        lash_trace::TraceEvent::DurableWaitResolved {
                            wait_kind: wait_kind.to_string(),
                            resolution,
                        }
                    });
                },
            )
            .await
            {
                Ok(result) => Ok(RuntimeEffectOutcome::Process { result }),
                Err(error) => Err(self.group_child_process_failure(error).await),
            },
            RestateEffectExecution::DurableProcessCommand {
                invocation,
                command,
            } => {
                let envelope = RuntimeEffectEnvelope::new(
                    invocation.clone(),
                    RuntimeEffectCommand::Process {
                        command: command.clone(),
                    },
                );
                let recorded = self
                    .record_eager_effect(
                        &envelope,
                        Box::pin(async move {
                            execute_restate_process_command(
                                &self.context,
                                &self.namespace,
                                &self.authority_id,
                                self.build_generation.as_ref(),
                                self.options.process_cancel,
                                &invocation,
                                *command,
                                local_executor,
                                |_| {},
                                |_, _| {},
                            )
                            .await
                            .map(|result| RuntimeEffectOutcome::Process { result })
                        }),
                    )
                    .await;
                match recorded {
                    Err(error) => Err(self.group_child_process_failure(error).await),
                    recorded => recorded,
                }
            }
            RestateEffectExecution::DirectLocal { envelope } => {
                local_executor.execute(envelope).await
            }
            RestateEffectExecution::Timer { invocation, spec } => {
                // Every sleep journals its frontier marker first, so a
                // served-only one answers there (FIG-3779).
                live_frontier::pass_sleep_frontier(
                    &self.context,
                    &invocation,
                    local_executor.served_only().as_ref(),
                )
                .await?;
                let RuntimeSleepOptions {
                    cancellation: _,
                    observe_turn_cancel,
                    turn_cancel_scope,
                    clock,
                } = local_executor.into_sleep_options();
                let duration_ms = match spec {
                    SleepSpec::For { duration_ms } => duration_ms,
                    SleepSpec::Until { deadline_ms } => {
                        deadline_ms.saturating_sub(clock.timestamp_ms())
                    }
                };
                self.emit_trace(Some(&invocation), || {
                    lash_trace::TraceEvent::DurableTimerStarted { duration_ms }
                });
                let turn_cancel = restate_timer_turn_cancel_wait_request(
                    &self.authority_id,
                    &invocation,
                    observe_turn_cancel,
                    turn_cancel_scope.as_ref(),
                )?;
                match self
                    .sleep_raced(&invocation, duration_ms, turn_cancel)
                    .await?
                {
                    Ok(RestateTurnCancelRaceOutcome::Completed(())) => {}
                    Ok(RestateTurnCancelRaceOutcome::SessionRevoked { session_id }) => {
                        self.emit_trace(Some(&invocation), || {
                            lash_trace::TraceEvent::DurableTimerResolved {
                                duration_ms,
                                status: lash_trace::TraceDurableTimerStatus::SessionRevoked,
                            }
                        });
                        return Err(RuntimeEffectControllerError::from(
                            lash_core::StoreError::SessionDeleted { session_id },
                        ));
                    }
                    Ok(
                        RestateTurnCancelRaceOutcome::TurnCancelled
                        | RestateTurnCancelRaceOutcome::ProcessCancelled,
                    ) => {
                        self.emit_trace(Some(&invocation), || {
                            lash_trace::TraceEvent::DurableTimerResolved {
                                duration_ms,
                                status: lash_trace::TraceDurableTimerStatus::Cancelled,
                            }
                        });
                        return Err(RuntimeEffectControllerError::new(
                            RuntimeErrorCode::RuntimeEffectSleepCancelled,
                            "runtime effect sleep was cancelled",
                        ));
                    }
                    Err(err) => {
                        self.emit_trace(Some(&invocation), || {
                            lash_trace::TraceEvent::DurableTimerResolved {
                                duration_ms,
                                status: lash_trace::TraceDurableTimerStatus::Failed,
                            }
                        });
                        tracing_sleep_error(&invocation, &err);
                        return Err(RuntimeEffectControllerError::new(
                            RuntimeErrorCode::EngineEffectController,
                            err.to_string(),
                        ));
                    }
                }
                self.emit_trace(Some(&invocation), || {
                    lash_trace::TraceEvent::DurableTimerResolved {
                        duration_ms,
                        status: lash_trace::TraceDurableTimerStatus::Resolved,
                    }
                });
                Ok(RuntimeEffectOutcome::Sleep)
            }
            RestateEffectExecution::AwaitEvent { invocation, key } => {
                if !restate_await_event_key_is_valid_for_authority(&self.authority_id, &key) {
                    return Err(RuntimeEffectControllerError::from(
                        restate_unknown_or_revoked(),
                    ));
                }
                // Key creation may run inside a journaled ToolAttempt and is
                // skipped when Restate replays that recorded result. Emit the
                // revocation observation here, where every live and replayed
                // wait crosses the same durable command boundary.
                self.require_active_session(key.scope.session_id())
                    .await
                    .map_err(RuntimeEffectControllerError::from)?;
                // A turn's cancellation reaches this wait only through the
                // durable gate race below (FIG-3672 P9); a process drive's
                // wait that observes no turn, such as a process body's
                // `waitSignal`, races the segment's durable cancel promise
                // (FIG-3673). No live token reaches it.
                let RuntimeAwaitEventOptions {
                    cancellation: _,
                    deadline,
                    clock,
                    observe_turn_cancel,
                    turn_cancel_scope,
                } = local_executor.into_await_event_options()?;
                let turn_cancel = restate_await_event_turn_cancel_wait_request(
                    &self.authority_id,
                    &invocation,
                    observe_turn_cancel,
                    turn_cancel_scope.as_ref(),
                )?;
                self.emit_trace(Some(&invocation), || {
                    lash_trace::TraceEvent::DurableWaitParked {
                        wait_kind: "await_event".to_string(),
                    }
                });
                let request = journaled_restate_durable_wait_request(
                    &self.context,
                    &key,
                    deadline,
                    clock.as_ref(),
                )
                .await
                .map_err(|err| {
                    RuntimeEffectControllerError::new(
                        RuntimeErrorCode::EngineEffectController,
                        err.to_string(),
                    )
                })?;
                let replay_key = invocation.replay_key().to_string();
                // A process segment's signal wait also races the drain's
                // hand-over (FIG-3799); every other wait keeps its shape.
                if let (
                    None,
                    context::ProcessCancelRace::Raced,
                    Some(generation),
                    lash_core::AwaitEventWaitIdentity::ProcessSignal { .. },
                ) = (
                    &turn_cancel,
                    self.options.process_cancel,
                    &self.options.segment_generation,
                    &key.wait,
                ) {
                    // Boxed: the three-way race's state stays off the
                    // controller's own future.
                    return Box::pin(self.await_segment_signal(
                        &invocation,
                        request,
                        replay_key,
                        generation.as_ref().clone(),
                    ))
                    .await;
                }
                if let Some(cancel) = self.group_child_wait_race(turn_cancel.as_ref()) {
                    return self
                        .await_event_under_group_child_cancel(
                            &invocation,
                            request,
                            replay_key,
                            cancel,
                        )
                        .await;
                }
                match self
                    .context
                    .await_event_or_turn_cancel(
                        &self.namespace,
                        request,
                        replay_key,
                        turn_cancel,
                        self.options.process_cancel,
                    )
                    .await
                {
                    Ok(RestateTurnCancelRaceOutcome::Completed(resolution)) => {
                        self.emit_trace(Some(&invocation), || {
                            lash_trace::TraceEvent::DurableWaitResolved {
                                wait_kind: "await_event".to_string(),
                                resolution: resolution_trace_label(&resolution),
                            }
                        });
                        Ok(RuntimeEffectOutcome::AwaitEvent { resolution })
                    }
                    Ok(RestateTurnCancelRaceOutcome::SessionRevoked { session_id }) => {
                        self.emit_trace(Some(&invocation), || {
                            lash_trace::TraceEvent::DurableWaitResolved {
                                wait_kind: "await_event".to_string(),
                                resolution: lash_trace::TraceDurableWaitResolution::SessionRevoked,
                            }
                        });
                        Err(RuntimeEffectControllerError::from(
                            lash_core::StoreError::SessionDeleted { session_id },
                        ))
                    }
                    Ok(RestateTurnCancelRaceOutcome::ProcessCancelled) => {
                        self.emit_trace(Some(&invocation), || {
                            lash_trace::TraceEvent::DurableWaitResolved {
                                wait_kind: "await_event".to_string(),
                                resolution: lash_trace::TraceDurableWaitResolution::Cancelled,
                            }
                        });
                        Ok(RuntimeEffectOutcome::AwaitEvent {
                            resolution: Resolution::Cancelled,
                        })
                    }
                    Ok(RestateTurnCancelRaceOutcome::TurnCancelled) => {
                        self.emit_trace(Some(&invocation), || {
                            lash_trace::TraceEvent::DurableWaitResolved {
                                wait_kind: "await_event".to_string(),
                                resolution: lash_trace::TraceDurableWaitResolution::TurnCancelled,
                            }
                        });
                        Ok(RuntimeEffectOutcome::AwaitEvent {
                            resolution: Resolution::Cancelled,
                        })
                    }
                    // The engine's cancellation of a group child's invocation is
                    // the child's decided cancel, surfacing at this wait (FIG-3904).
                    Err(err) if self.is_group_child_engine_cancel(&err) => {
                        Err(group_child_cancelled())
                    }
                    Err(err) => {
                        self.emit_trace(Some(&invocation), || {
                            lash_trace::TraceEvent::DurableWaitResolved {
                                wait_kind: "await_event".to_string(),
                                resolution: lash_trace::TraceDurableWaitResolution::Failed,
                            }
                        });
                        Err(RuntimeEffectControllerError::new(
                            RuntimeErrorCode::EngineEffectController,
                            err.to_string(),
                        ))
                    }
                }
            }
            RestateEffectExecution::PeekAwaitEvent { key, .. } => self
                .peek_turn_gate(&key)
                .await
                .map(|resolution| RuntimeEffectOutcome::PeekAwaitEvent { resolution })
                .map_err(RuntimeEffectControllerError::from),
            RestateEffectExecution::JournaledRun {
                envelope,
                engine_faults,
            } => {
                let effect_kind = envelope.command.kind();
                let reconstructed_envelope = envelope.canonical_form()?;
                let replay_trace = local_executor.replay_validation_trace().cloned();
                let invocation = envelope.invocation.clone();
                self.emit_trace(Some(&invocation), || {
                    lash_trace::TraceEvent::JournaledEffectStarted {
                        effect_name: restate_effect_name(&invocation),
                        effect_kind: effect_kind.as_str().to_string(),
                    }
                });
                let recorded_envelope = Arc::new(reconstructed_envelope.clone());
                let recorded = self
                    .record_journaled_run(
                        &invocation,
                        &recorded_envelope,
                        envelope,
                        local_executor,
                        engine_faults,
                    )
                    .await;
                let recorded = match recorded {
                    Ok(recorded) => recorded,
                    Err(error) => {
                        self.emit_trace(Some(&invocation), || {
                            lash_trace::TraceEvent::JournaledEffectSettled {
                                effect_name: restate_effect_name(&invocation),
                                effect_kind: effect_kind.as_str().to_string(),
                                status: lash_trace::TraceJournaledEffectStatus::Failed,
                            }
                        });
                        if let RestateEffectError::Terminal { terminal, .. } = &error
                            && self.is_group_child_engine_cancel(terminal)
                        {
                            return Err(group_child_cancelled());
                        }
                        return Err(error.into());
                    }
                };
                let outcome = validate_recorded_effect_envelope(
                    recorded,
                    &reconstructed_envelope,
                    replay_trace.as_ref(),
                );
                let outcome = match outcome {
                    // A step that journals no fault can only hand back an
                    // error it recorded: its outcome on every replay, whatever
                    // its code (FIG-3528).
                    Ok(outcome) if engine_faults == EngineFaults::Retried => {
                        outcome.map_err(RuntimeEffectControllerError::into_journaled)
                    }
                    Ok(outcome) => outcome,
                    Err(error) => {
                        self.emit_trace(Some(&invocation), || {
                            lash_trace::TraceEvent::JournaledEffectSettled {
                                effect_name: restate_effect_name(&invocation),
                                effect_kind: effect_kind.as_str().to_string(),
                                status: lash_trace::TraceJournaledEffectStatus::Failed,
                            }
                        });
                        return Err(error);
                    }
                };
                self.emit_trace(Some(&invocation), || {
                    lash_trace::TraceEvent::JournaledEffectSettled {
                        effect_name: restate_effect_name(&invocation),
                        effect_kind: effect_kind.as_str().to_string(),
                        status: if outcome.is_ok() {
                            lash_trace::TraceJournaledEffectStatus::Completed
                        } else {
                            lash_trace::TraceJournaledEffectStatus::Failed
                        },
                    }
                });
                outcome
            }
        }
    }
}

fn effect_group_engine_error(
    operation: &str,
    error: TerminalError,
) -> RuntimeEffectControllerError {
    if let Some(refusal) = crate::object_state::stored_format_error_in(error.message()) {
        return refusal;
    }
    group_shape_error(format!(
        "Restate effect-group operation {operation} failed (verify the required services are registered): {error}"
    ))
}

fn resolution_trace_label(resolution: &Resolution) -> lash_trace::TraceDurableWaitResolution {
    use lash_trace::TraceDurableWaitResolution as Resolved;
    match resolution {
        Resolution::Ok(_) => Resolved::Ok,
        Resolution::Err(_) => Resolved::Error,
        Resolution::Timeout => Resolved::Timeout,
        Resolution::Cancelled => Resolved::Cancelled,
    }
}

async fn execute_restate_journaled_effect(
    envelope: RuntimeEffectEnvelope,
    local_executor: RuntimeEffectLocalExecutor<'_>,
) -> Result<RuntimeEffectOutcome, RuntimeEffectControllerError> {
    let RuntimeEffectEnvelope {
        invocation,
        command,
        group,
    } = envelope;
    match command {
        RuntimeEffectCommand::Trigger { command } => {
            refuse_unhonored_group_membership(group.as_deref(), "restate trigger")?;
            local_executor.execute_trigger(invocation, *command).await
        }
        command => {
            local_executor
                .execute(RuntimeEffectEnvelope {
                    invocation,
                    command,
                    group,
                })
                .await
        }
    }
}

mod process_command;
pub use process_command::PROCESS_COMMAND_JOURNAL_PAYLOAD_VERSION;
use process_command::execute_restate_process_command;
async fn signal_ordinal_for_event(
    registry: &dyn ProcessRegistry,
    process_id: &lash_core::ProcessId,
    signal_name: &str,
    event_type: &str,
    sequence: u64,
) -> Result<u64, PluginError> {
    if let Some(lash_core::WaitState {
        kind:
            lash_core::WaitKind::Signal {
                name,
                event_type: waiting_type,
                ordinal,
                ..
            },
        ..
    }) = registry
        .get_process(process_id)
        .await?
        .and_then(|record| record.wait)
        && name == signal_name
        && waiting_type == event_type
    {
        return Ok(ordinal);
    }
    // Count at the store without fetching the full event log.
    registry
        .count_events_through(process_id, event_type, sequence)
        .await
}

mod process_scheduling;
use process_scheduling::schedule_restate_process;

mod execution;
use execution::tracing_sleep_error;
pub(crate) use execution::{
    RestateEffectExecution, restate_effect_execution, restate_effect_name,
    validate_recorded_effect_envelope,
};

#[cfg(test)]
mod tests;
