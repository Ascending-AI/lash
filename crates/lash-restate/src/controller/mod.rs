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
mod group_commit;
mod group_read;
pub(crate) mod journal_budget;
mod journaled_effect;
use journaled_effect::EngineFaults;
mod live_frontier;
mod scope_recording;
mod scoped;
mod turn_cancel_request;
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
    QueuedLaneAcquisition, QueuedLaneProbe, RankedGroupSettlement, Resolution, ResolveOutcome,
    RuntimeEffectCommand, RuntimeEffectController, RuntimeEffectControllerError,
    RuntimeEffectEnvelope, RuntimeEffectGroup, RuntimeEffectInvocation, RuntimeEffectLocalExecutor,
    RuntimeEffectOutcome, RuntimeError, RuntimeErrorCode, ScopedEffectController, SleepSpec,
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
    EffectGroupPayloadGetResponse, EffectGroupProbeResponse, EffectGroupReadRankRequest,
    EffectGroupReadRankResponse, EffectGroupSettlementTerminal, EffectGroupShape,
    EffectGroupWaitResolution, decode_wait_resolution, group_shape_error, payload_key,
    rank_wait_request, ready_wait_request, settlement_from_payload,
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
}

impl Default for RestateEffectControllerOptions {
    fn default() -> Self {
        Self {
            run_retry_policy: None,
            segment_effect_budget: 10_000,
            journaled_effect_byte_budget: None,
            drain_budget: lash_core::EffectGroupDrainBudget::DEFAULT.duration(),
            process_cancel: context::ProcessCancelRace::NotRaced,
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
    key: &AwaitEventKey,
    resolution: Resolution,
) -> Result<ResolveOutcome, RuntimeError>
where
    C: RestateControllerContext<'ctx> + ?Sized,
{
    context
        .resolve_event(RestateDurableWaitResolveRequest {
            key: key.clone(),
            resolution,
        })
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
            _ctx: PhantomData,
        }
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

impl<'ctx, C> RestateRuntimeEffectController<'ctx, C>
where
    C: RestateControllerContext<'ctx>,
{
    async fn require_active_session(
        &self,
        session_id: Option<&SessionId>,
    ) -> Result<(), RuntimeError> {
        if let Some(session_id) = session_id
            && self
                .context
                .session_is_revoked(SessionId::from(session_id.to_string()))
                .await
                .map_err(|err| {
                    RuntimeError::new(
                        lash_core::RuntimeErrorCode::EngineEffectController,
                        err.to_string(),
                    )
                })?
        {
            return Err(restate_unknown_or_revoked());
        }
        Ok(())
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

    /// Restate re-drives this handler invocation, so its retry policy - not a
    /// sleep inside one invocation - is the right place to pace a queued drain
    /// that found the session execution lane held by a live foreign executor.
    /// The deployment-level [`RestateEffectHost`](crate::RestateEffectHost)
    /// deliberately does not opt in: it serves requests from outside a handler,
    /// where nothing re-drives the caller.
    async fn acquire_queued_lane(
        &self,
        lane: Arc<dyn QueuedLaneProbe>,
        cancel: tokio_util::sync::CancellationToken,
    ) -> Result<QueuedLaneAcquisition, RuntimeError> {
        self.wait_out_crashed_lane_holder(lane, cancel).await
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
        resolve_restate_await_event(&self.context, key, resolution).await
    }

    async fn peek_await_event(
        &self,
        key: &AwaitEventKey,
    ) -> Result<Option<Resolution>, RuntimeError> {
        if !restate_await_event_key_is_valid_for_authority(&self.authority_id, key) {
            return Err(restate_unknown_or_revoked());
        }
        self.require_active_session(key.scope.session_id()).await?;
        self.context
            .peek_event(RestateDurableWaitAddress::for_key(key), key.key_id.clone())
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
        let clock = lash_core::facade_support::SystemClock;
        let replay_key = key.key_id.clone();
        let request = journaled_restate_durable_wait_request(&self.context, key, deadline, &clock)
            .await
            .map_err(|err| {
                RuntimeError::new(
                    lash_core::RuntimeErrorCode::EngineEffectController,
                    err.to_string(),
                )
            })?;
        self.context
            .await_event(request, replay_key, cancel)
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
            .update_session_waits(SessionId::from(session_id.to_string()), true)
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
            .update_session_waits(SessionId::from(session_id.to_string()), false)
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
}

impl<'ctx, C> RestateRuntimeEffectController<'ctx, C>
where
    C: RestateControllerContext<'ctx>,
{
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
        let handle = EffectGroupHandle::new(&group);
        let shape = EffectGroupShape::from_group(&group, opener)?;
        let open_request = EffectGroupOpenRequest {
            shape,
            content_checked: group.reopen() == lash_core::GroupReopen::RetainedContent,
        };
        self.refuse_over_budget_group_open(group.invocation(), &open_request)
            .await?;
        let shape = &open_request.shape;
        let probe = self
            .context
            .effect_group_probe(group_key.clone())
            .await
            .map_err(|error| effect_group_engine_error("EffectGroupIndex/probe", error))?;
        if matches!(probe, EffectGroupProbeResponse::Absent)
            && let Some(position) = self
                .context
                .effect_group_preflight(group_key.clone(), group.children().to_vec())
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
            .effect_group_open(group_key.clone(), open_request.clone())
            .await
            .map_err(|error| effect_group_engine_error("EffectGroupIndex/open", error))?;
        match opened {
            EffectGroupOpenResponse::OpenedFresh | EffectGroupOpenResponse::ReopenedPreparing => {
                self.context
                    .effect_group_submit(EffectGroupDispatchRequest {
                        group_key: group_key.clone(),
                    })
                    .await
                    .map_err(|error| effect_group_engine_error("EffectGroupDispatch/run", error))?;
                let request = ready_wait_request(&shape.wait_scope, &group_key)?;
                let resolution = match self
                    .context
                    .await_effect_group_wait(
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
        if handle.is_exhausted() {
            return Err(group_shape_error(format!(
                "effect group {} has no settlement after its {} children",
                handle.group_key(),
                handle.children()
            )));
        }
        let rank = u64::try_from(handle.consumed() + 1).map_err(|error| {
            group_shape_error(format!("effect group rank does not fit u64: {error}"))
        })?;
        let mut read = self
            .context
            .effect_group_read_rank(
                handle.group_key().to_string(),
                EffectGroupReadRankRequest {
                    rank,
                    for_caller: true,
                },
            )
            .await
            .map_err(|error| effect_group_engine_error("EffectGroupIndex/read_rank", error))?;
        if matches!(read, EffectGroupReadRankResponse::NotSettled) {
            let scope = ExecutionScope::runtime_operation(handle.group_key());
            let request = rank_wait_request(&scope, handle.group_key(), rank)?;
            // A turn-observing rank wait races the turn's durable cancellation
            // gate, and a process drive's rank wait that observes no turn
            // races the segment's durable cancel promise; never a live token.
            // The journal records which completed first (FIG-3672 P9,
            // FIG-3673).
            let turn_cancel = restate_group_turn_cancel_wait_request(&self.authority_id, &cancel)?;
            let resolution = match self
                .context
                .await_effect_group_wait(
                    request,
                    handle.group_key().to_string(),
                    turn_cancel,
                    self.options.process_cancel,
                )
                .await
                .map_err(|error| {
                    effect_group_engine_error(
                        "LashDurableWaitWorkflow/await_resolution(RANK)",
                        error,
                    )
                })? {
                RestateTurnCancelRaceOutcome::Completed(resolution) => resolution,
                RestateTurnCancelRaceOutcome::TurnCancelled
                | RestateTurnCancelRaceOutcome::ProcessCancelled => {
                    return Err(RuntimeEffectControllerError::new(
                        RuntimeErrorCode::RuntimeEffectGroupAwaitCancelled,
                        format!(
                            "awaiting effect group {} rank {rank} was cancelled",
                            handle.group_key()
                        ),
                    ));
                }
                RestateTurnCancelRaceOutcome::SessionRevoked { session_id } => {
                    return Err(RuntimeEffectControllerError::from(
                        lash_core::StoreError::SessionDeleted { session_id },
                    ));
                }
            };
            match decode_wait_resolution(resolution)? {
                EffectGroupWaitResolution::Rank => {}
                EffectGroupWaitResolution::Retired => {
                    return Err(group_shape_error(format!(
                        "effect group {} was retired while awaiting rank {rank}",
                        handle.group_key()
                    )));
                }
                other => {
                    return Err(group_shape_error(format!(
                        "effect group {} rank {rank} wait resolved as {other:?}",
                        handle.group_key()
                    )));
                }
            }
            read = self
                .context
                .effect_group_read_rank(
                    handle.group_key().to_string(),
                    EffectGroupReadRankRequest {
                        rank,
                        for_caller: true,
                    },
                )
                .await
                .map_err(|error| effect_group_engine_error("EffectGroupIndex/read_rank", error))?;
        }
        let record = match read {
            EffectGroupReadRankResponse::Settled { settlement, .. } => settlement,
            EffectGroupReadRankResponse::NotSettled => {
                return Err(group_shape_error(format!(
                    "effect group {} rank {rank} remained unsettled after its notification",
                    handle.group_key()
                )));
            }
            EffectGroupReadRankResponse::Closed => {
                return Err(group_shape_error(format!(
                    "effect group {} is closed to this caller",
                    handle.group_key()
                )));
            }
            EffectGroupReadRankResponse::UnknownGroup => {
                return Err(group_shape_error(format!(
                    "effect group {} is unknown",
                    handle.group_key()
                )));
            }
            EffectGroupReadRankResponse::Retired => {
                return Err(group_shape_error(format!(
                    "effect group {} is retired",
                    handle.group_key()
                )));
            }
        };
        let payload = if matches!(
            record.terminal,
            EffectGroupSettlementTerminal::StoredPayload
        ) {
            match self
                .context
                .effect_group_payload_get(payload_key(handle.group_key(), record.position))
                .await
                .map_err(|error| effect_group_engine_error("EffectGroupPayload/get", error))?
            {
                EffectGroupPayloadGetResponse::Stored { bytes } => Some(bytes),
                EffectGroupPayloadGetResponse::Missing => {
                    return Err(group_shape_error(format!(
                        "effect group {} rank {rank} refers to a missing payload",
                        handle.group_key()
                    )));
                }
                EffectGroupPayloadGetResponse::Retired => {
                    return Err(group_shape_error(format!(
                        "effect group {} payload was retired",
                        handle.group_key()
                    )));
                }
            }
        } else {
            None
        };
        let settlement = settlement_from_payload(record, payload)?;
        handle.advance()?;
        Ok(settlement)
    }

    /// The cursorless rank read the §6 incorporation record needs (ADR 0099
    /// §8); the body lives in [`group_read`] for the file-size budget.
    async fn read_group_settlement(
        &self,
        group_key: &str,
        rank: u64,
    ) -> Result<Option<RankedGroupSettlement>, RuntimeEffectControllerError> {
        group_read::read_group_settlement(&self.context, group_key, rank).await
    }

    async fn close_effect_group(
        &self,
        handle: EffectGroupHandle,
        disposition: LoserPolicy,
    ) -> Result<(), RuntimeEffectControllerError> {
        let group_key = handle.group_key().to_string();
        let response = self
            .context
            .effect_group_close(group_key.clone(), EffectGroupCloseRequest { disposition })
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
        group_commit::commit_group_child_final(&self.context, commit).await
    }

    async fn await_group_child_drain_admission(
        &self,
        group_key: &str,
        commit_seq: u64,
    ) -> Result<(), RuntimeEffectControllerError> {
        group_commit::await_group_child_drain_admission(&self.context, group_key, commit_seq).await
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
            } => execute_restate_process_command(
                &self.context,
                &self.authority_id,
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
            .map(|result| RuntimeEffectOutcome::Process { result }),
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
                self.record_eager_effect(
                    &envelope,
                    Box::pin(async move {
                        execute_restate_process_command(
                            &self.context,
                            &self.authority_id,
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
                .await
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
                let duration = Duration::from_millis(duration_ms);
                let turn_cancel = restate_timer_turn_cancel_wait_request(
                    &self.authority_id,
                    &invocation,
                    observe_turn_cancel,
                    turn_cancel_scope.as_ref(),
                )?;
                match self
                    .context
                    .sleep_or_turn_cancel(duration, turn_cancel, self.options.process_cancel)
                    .await
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
                match self
                    .context
                    .await_event_or_turn_cancel(
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
                .peek_await_event(&key)
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
    event_type: &str,
    sequence: u64,
) -> Result<u64, PluginError> {
    // COUNT at the store, not a full log fetch: per-signal cost must stay
    // flat for long-lived processes that accumulate large event histories.
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
mod identity_trace_tests {
    use super::*;

    #[test]
    fn restate_trace_projection_uses_shared_parent_precedence_and_scoped_nodes() {
        let parent_address = lash_core::EffectAddress::new(
            ExecutionScope::process(lash_core::ProcessId::fixture("restate-parent-process")),
            "shared-replay-key",
        )
        .expect("valid Restate causal address");
        let invocation = RuntimeEffectInvocation::new(
            lash_core::EffectAddress::new(
                ExecutionScope::turn("restate-session", "restate-turn"),
                "restate-child-key",
            )
            .expect("valid Restate child address"),
            lash_core::RuntimeAttribution::for_turn("restate-session", "restate-turn", 4, 2),
            "restate-child",
        )
        .with_caused_by(Some(lash_core::CausalRef::Effect {
            address: parent_address.clone(),
        }));

        let caused = trace_context_for_runtime_effect_invocation(
            lash_trace::TraceContext::default(),
            &invocation,
        );
        assert_eq!(
            caused.parent_graph_node_id.as_deref(),
            Some(parent_address.graph_key().as_str())
        );

        let explicit = lash_trace::TraceContext {
            parent_graph_node_id: Some("host:explicit-parent".to_string()),
            run_id: Some("restate-host-run".to_string()),
            ..Default::default()
        };
        let explicit = trace_context_for_runtime_effect_invocation(explicit.clone(), &invocation);
        assert_eq!(
            explicit.parent_graph_node_id.as_deref(),
            Some("host:explicit-parent")
        );
        assert_eq!(explicit.run_id.as_deref(), Some("restate-host-run"));
    }
}
