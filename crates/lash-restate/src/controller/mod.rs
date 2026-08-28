//! Handler-scoped runtime-effect controller.
//!
//! One responsibility: map a Lash runtime effect onto the Restate journal
//! command that makes it durable — a `ctx.run` for journaled effects, a durable
//! timer for sleeps, a durable-wait call for await-events, and direct workflow
//! scheduling for process commands. The context seam those commands are issued
//! through lives in [`context`].

pub(crate) mod context;
pub(crate) mod journal_budget;
mod journaled_effect;

use std::collections::HashSet;
use std::fmt;
use std::marker::PhantomData;
use std::sync::{Arc, Mutex, Weak};
use std::time::Duration;

use lash_core::{
    AwaitEventKey, AwaitEventResolver, AwaitEventWaitIdentity, CompletionKeyPreparation,
    EffectGroupHandle, EffectHost, ExecutionScope, GroupSettlement, LoserPolicy, PluginError,
    ProcessCommand, ProcessEffectOutcome, ProcessExternalRef, ProcessRecord, ProcessRegistry,
    QueuedLaneAcquisition, QueuedLaneProbe, Resolution, ResolveOutcome, RuntimeEffectCommand,
    RuntimeEffectController, RuntimeEffectControllerError, RuntimeEffectEnvelope,
    RuntimeEffectFailureDisposition, RuntimeEffectGroup, RuntimeEffectKind,
    RuntimeEffectLocalExecutor, RuntimeEffectOutcome, RuntimeError, RuntimeErrorCode,
    RuntimeInvocation, ScopedEffectController, TurnControlParticipation,
    facade_support::CanonicalRuntimeEffectEnvelope, facade_support::RuntimeAwaitEventOptions,
    facade_support::RuntimeSleepOptions, facade_support::refuse_unhonored_group_membership,
    facade_support::validate_replayed_effect_envelope,
};
use restate_sdk::context::RunRetryPolicy;
use restate_sdk::errors::TerminalError;
use serde::Serialize;

use crate::durable_wait::{
    RestateDurableWaitAddress, RestateDurableWaitAwaitRequest, RestateDurableWaitResolveRequest,
    RestateTurnCancelRaceOutcome, restate_await_event_key, restate_await_event_key_is_valid,
    restate_durable_wait_request, restate_unknown_or_revoked,
};
use crate::effect_group::{
    EffectGroupCloseDisposition, EffectGroupCloseRequest, EffectGroupCloseResponse,
    EffectGroupDispatchRequest, EffectGroupOpenRequest, EffectGroupOpenResponse,
    EffectGroupPayloadGetResponse, EffectGroupProbeResponse, EffectGroupReadRankRequest,
    EffectGroupReadRankResponse, EffectGroupSettlementTerminal, EffectGroupShape,
    EffectGroupWaitResolution, decode_wait_resolution, group_shape_error, payload_key,
    rank_wait_request, ready_wait_request, settlement_from_payload,
};
use crate::process::RestateProcessCancelRequest;

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
    segment_duration_cap: Option<Duration>,
    segment_effect_budget: u64,
    journaled_effect_byte_budget: Option<u64>,
}

impl Default for RestateEffectControllerOptions {
    fn default() -> Self {
        Self {
            run_retry_policy: None,
            segment_duration_cap: None,
            segment_effect_budget: 10_000,
            journaled_effect_byte_budget: None,
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

    /// Request a segment boundary once this handler incarnation has lived for
    /// at least `cap`. The actual cut remains the engine's quiescent post-effect
    /// point, shared with journal-budget boundaries.
    pub fn segment_duration_cap(mut self, cap: Duration) -> Self {
        self.segment_duration_cap = Some(cap);
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
    /// Effects that must run outside the run closure - a durable process command
    /// and a tool batch - journal their budget verdict in a slot of their own
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
}

impl fmt::Debug for RestateEffectControllerOptions {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RestateEffectControllerOptions")
            .field("run_retry_policy", &self.run_retry_policy)
            .field("segment_duration_cap", &self.segment_duration_cap)
            .field("segment_effect_budget", &self.segment_effect_budget)
            .field(
                "journaled_effect_byte_budget",
                &self.journaled_effect_byte_budget,
            )
            .finish()
    }
}
#[derive(Clone, Debug, Serialize, serde::Deserialize)]
pub(crate) struct RecordedRuntimeEffect {
    pub(crate) envelope: Arc<CanonicalRuntimeEffectEnvelope>,
    pub(crate) outcome: Result<RuntimeEffectOutcome, RuntimeEffectControllerError>,
}

/// Error raised while bridging a Lash effect to Restate.
#[derive(Debug, thiserror::Error)]
pub enum RestateEffectError {
    #[error("Restate terminal error while running `{effect}`: {terminal}")]
    Terminal {
        effect: String,
        terminal: TerminalError,
    },
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
                lash_core::RuntimeErrorCode::RestateEffectController,
                err.to_string(),
            )
        })
}
fn restate_turn_cancel_wait_request(
    invocation: &RuntimeInvocation,
    turn_cancel_scope: Option<&ExecutionScope>,
) -> Result<Option<RestateDurableWaitAwaitRequest>, RuntimeEffectControllerError> {
    let Some(turn_id) = invocation.scope.turn_id.as_deref() else {
        return Ok(None);
    };
    let Some(scope) = turn_cancel_scope else {
        return Err(RuntimeEffectControllerError::new(
            RuntimeErrorCode::RestateTurnCancelScopeMissing,
            "turn effects that observe cancellation require a durable turn-cancel scope",
        ));
    };
    if matches!(scope, ExecutionScope::Process { .. }) {
        return Ok(None);
    }
    let scope @ ExecutionScope::Turn { .. } = scope else {
        return Err(RuntimeEffectControllerError::new(
            RuntimeErrorCode::RestateTurnCancelScopeMismatch,
            "turn-cancel scope must be a matching turn scope or an explicit process scope",
        ));
    };
    scope
        .validate()
        .map_err(RuntimeEffectControllerError::from)?;
    if scope.session_id() != Some(invocation.scope.session_id.as_str())
        || scope.turn_id() != Some(turn_id)
    {
        return Err(RuntimeEffectControllerError::new(
            RuntimeErrorCode::RestateTurnCancelScopeMismatch,
            "turn-cancel scope must match the runtime effect invocation",
        ));
    }
    let key = restate_await_event_key(scope, AwaitEventWaitIdentity::TurnCancelGate)?;
    Ok(Some(restate_durable_wait_request(
        &key,
        None,
        &lash_core::facade_support::SystemClock,
    )))
}

pub(crate) fn restate_timer_turn_cancel_wait_request(
    invocation: &RuntimeInvocation,
    observe_turn_cancel: bool,
    turn_cancel_scope: Option<&ExecutionScope>,
) -> Result<Option<RestateDurableWaitAwaitRequest>, RuntimeEffectControllerError> {
    if !observe_turn_cancel {
        return Ok(None);
    }
    restate_turn_cancel_wait_request(invocation, turn_cancel_scope)
}

fn restate_process_turn_cancel_wait_request(
    invocation: &RuntimeInvocation,
    observe_turn_cancel: bool,
    turn_cancel_scope: Option<&ExecutionScope>,
) -> Result<Option<RestateDurableWaitAwaitRequest>, RuntimeEffectControllerError> {
    if !observe_turn_cancel {
        return Ok(None);
    }
    restate_turn_cancel_wait_request(invocation, turn_cancel_scope)
}

pub(crate) fn restate_await_event_turn_cancel_wait_request(
    invocation: &RuntimeInvocation,
    observe_turn_cancel: bool,
    turn_cancel_scope: Option<&ExecutionScope>,
) -> Result<Option<RestateDurableWaitAwaitRequest>, RuntimeEffectControllerError> {
    if !observe_turn_cancel {
        return Ok(None);
    }
    restate_turn_cancel_wait_request(invocation, turn_cancel_scope)
}
/// Lash [`RuntimeEffectController`] and [`EffectHost`] backed by a Restate handler context.
///
/// This type is intentionally handler-scoped. Create one inside the Restate
/// handler that owns the Lash operation, then pass
/// [`RestateRuntimeEffectController::scoped_effect_controller`] to Lash's
/// scoped API with a stable [`ExecutionScope`].
pub struct RestateRuntimeEffectController<'ctx, C> {
    context: C,
    options: RestateEffectControllerOptions,
    trace: Option<RestateTraceObserver>,
    closed_effect_groups: Mutex<HashSet<String>>,
    _ctx: PhantomData<&'ctx ()>,
}

impl<'ctx, C> RestateRuntimeEffectController<'ctx, C> {
    pub fn new(context: C) -> Self {
        Self::with_options(context, RestateEffectControllerOptions::default())
    }

    pub fn with_options(context: C, options: RestateEffectControllerOptions) -> Self {
        Self {
            context,
            options,
            trace: None,
            closed_effect_groups: Mutex::new(HashSet::new()),
            _ctx: PhantomData,
        }
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
        invocation: Option<&RuntimeInvocation>,
        event: impl FnOnce() -> lash_trace::TraceEvent,
    ) {
        let Some(trace) = self.trace.as_ref() else {
            return;
        };
        let Some(sink) = trace.sink.upgrade() else {
            return;
        };
        let context = if let Some(invocation) = invocation {
            let context = trace_context_for_invocation(trace, invocation);
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

    fn remember_trace_invocation(&self, invocation: &RuntimeInvocation) {
        let Some(trace) = self.trace.as_ref() else {
            return;
        };
        if trace.sink.upgrade().is_none() {
            return;
        }
        *trace
            .current_context
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) =
            Some(trace_context_for_invocation(trace, invocation));
    }
}

fn trace_context_for_invocation(
    trace: &RestateTraceObserver,
    invocation: &RuntimeInvocation,
) -> lash_trace::TraceContext {
    let mut context = trace.base_context.clone();
    context.session_id = Some(invocation.scope.session_id.clone());
    context.turn_id = invocation.scope.turn_id.clone();
    context.turn_index = invocation.scope.turn_index;
    context.protocol_iteration = invocation.scope.protocol_iteration;
    context.effect_id = invocation.effect_id().map(str::to_string);
    if context.parent_graph_node_id.is_none()
        && let Some(turn_id) = context.turn_id.as_deref()
    {
        context.parent_graph_node_id =
            Some(format!("turn:{}:{turn_id}", invocation.scope.session_id));
    }
    context
}

impl<'ctx, C> RestateRuntimeEffectController<'ctx, C>
where
    C: RestateControllerContext<'ctx>,
{
    pub fn scoped_effect_controller<'run>(
        &'run self,
        scope: ExecutionScope,
    ) -> Result<ScopedEffectController<'run>, RuntimeError> {
        scope.validate()?;
        ScopedEffectController::borrowed(self, scope)
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
    async fn require_active_session(&self, session_id: Option<&str>) -> Result<(), RuntimeError> {
        if let Some(session_id) = session_id
            && self
                .context
                .session_is_revoked(session_id.to_string())
                .await
                .map_err(|err| {
                    RuntimeError::new(
                        lash_core::RuntimeErrorCode::RestateEffectController,
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
        restate_await_event_key(scope, wait)
    }

    async fn resolve_await_event(
        &self,
        key: &AwaitEventKey,
        resolution: Resolution,
    ) -> Result<ResolveOutcome, RuntimeError> {
        if !restate_await_event_key_is_valid(key) {
            return Ok(ResolveOutcome::UnknownOrRevoked);
        }
        resolve_restate_await_event(&self.context, key, resolution).await
    }

    async fn peek_await_event(
        &self,
        key: &AwaitEventKey,
    ) -> Result<Option<Resolution>, RuntimeError> {
        if !restate_await_event_key_is_valid(key) {
            return Err(restate_unknown_or_revoked());
        }
        self.require_active_session(key.scope.session_id()).await?;
        self.context
            .peek_event(RestateDurableWaitAddress::for_key(key))
            .await
            .map_err(|err| {
                RuntimeError::new(
                    lash_core::RuntimeErrorCode::RestateEffectController,
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
        if !restate_await_event_key_is_valid(key) {
            return Err(restate_unknown_or_revoked());
        }
        self.require_active_session(key.scope.session_id()).await?;
        let clock = lash_core::facade_support::SystemClock;
        self.context
            .await_event(restate_durable_wait_request(key, deadline, &clock), cancel)
            .await
            .map_err(|err| {
                RuntimeError::new(
                    lash_core::RuntimeErrorCode::RestateEffectController,
                    err.to_string(),
                )
            })
    }

    async fn revoke_await_events_for_session(&self, session_id: &str) -> Result<(), RuntimeError> {
        self.context
            .update_session_waits(session_id.to_string(), true)
            .await
            .map_err(|err| {
                RuntimeError::new(
                    lash_core::RuntimeErrorCode::RestateEffectController,
                    err.to_string(),
                )
            })
    }

    async fn cancel_await_events_for_session(&self, session_id: &str) -> Result<(), RuntimeError> {
        self.context
            .update_session_waits(session_id.to_string(), false)
            .await
            .map_err(|err| {
                RuntimeError::new(
                    lash_core::RuntimeErrorCode::RestateEffectController,
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
    fn scoped<'run>(
        &'run self,
        scope: ExecutionScope,
    ) -> Result<ScopedEffectController<'run>, RuntimeError> {
        self.scoped_effect_controller(scope)
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

#[async_trait::async_trait]
impl<'ctx, C> RuntimeEffectController for RestateRuntimeEffectController<'ctx, C>
where
    C: RestateControllerContext<'ctx>,
{
    fn supports_concurrent_effects(&self) -> bool {
        false
    }

    fn supports_effect_groups(&self) -> bool {
        true
    }

    async fn open_effect_group(
        &self,
        group: RuntimeEffectGroup,
    ) -> Result<EffectGroupHandle, RuntimeEffectControllerError> {
        let group_key = group.group_key().to_string();
        let handle = EffectGroupHandle::new(&group);
        let shape = EffectGroupShape::from_group(&group)?;
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
            return Err(group_shape_error(format!(
                "effect group {group_key} child {position} ({}) has no registered executor; refusing before group state is created",
                shape.replay_keys[position]
            )));
        }
        let opened = self
            .context
            .effect_group_open(
                group_key.clone(),
                EffectGroupOpenRequest {
                    shape: shape.clone(),
                },
            )
            .await
            .map_err(|error| effect_group_engine_error("EffectGroupIndex/open", error))?;
        match opened {
            EffectGroupOpenResponse::OpenedFresh | EffectGroupOpenResponse::ReopenedPreparing => {
                self.context
                    .effect_group_submit(EffectGroupDispatchRequest {
                        group_key: group_key.clone(),
                        shape: shape.clone(),
                        children: group.children().to_vec(),
                    })
                    .await
                    .map_err(|error| effect_group_engine_error("EffectGroupDispatch/run", error))?;
                let request = ready_wait_request(&shape.wait_scope, &group_key)?;
                let resolution = self
                    .context
                    .await_effect_group_wait(request, tokio_util::sync::CancellationToken::new())
                    .await
                    .map_err(|error| {
                        effect_group_engine_error(
                            "LashDurableWaitWorkflow/await_resolution(READY)",
                            error,
                        )
                    })?
                    .ok_or_else(|| {
                        group_shape_error(format!(
                            "opening effect group {group_key} was cancelled while awaiting READY"
                        ))
                    })?;
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
            EffectGroupOpenResponse::ShapeMismatch => Err(group_shape_error(format!(
                "effect group {group_key} was reopened with a different durable shape"
            ))),
        }
    }

    async fn await_next_settlement(
        &self,
        handle: &mut EffectGroupHandle,
        cancel: tokio_util::sync::CancellationToken,
    ) -> Result<GroupSettlement, RuntimeEffectControllerError> {
        if handle.is_exhausted() {
            return Err(group_shape_error(format!(
                "effect group {} has no settlement after its {} children",
                handle.group_key(),
                handle.children()
            )));
        }
        if self
            .closed_effect_groups
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .contains(handle.group_key())
        {
            return Err(group_shape_error(format!(
                "effect group {} is closed to this caller",
                handle.group_key()
            )));
        }
        let rank = u64::try_from(handle.consumed() + 1).map_err(|error| {
            group_shape_error(format!("effect group rank does not fit u64: {error}"))
        })?;
        let mut read = self
            .context
            .effect_group_read_rank(
                handle.group_key().to_string(),
                EffectGroupReadRankRequest { rank },
            )
            .await
            .map_err(|error| effect_group_engine_error("EffectGroupIndex/read_rank", error))?;
        if matches!(read, EffectGroupReadRankResponse::NotSettled) {
            let scope = ExecutionScope::runtime_operation(handle.group_key());
            let request = rank_wait_request(&scope, handle.group_key(), rank)?;
            let Some(resolution) = self
                .context
                .await_effect_group_wait(request, cancel)
                .await
                .map_err(|error| {
                    effect_group_engine_error(
                        "LashDurableWaitWorkflow/await_resolution(RANK)",
                        error,
                    )
                })?
            else {
                return Err(RuntimeEffectControllerError::new(
                    RuntimeErrorCode::RuntimeEffectGroupAwaitCancelled,
                    format!(
                        "awaiting effect group {} rank {rank} was cancelled",
                        handle.group_key()
                    ),
                ));
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
                    EffectGroupReadRankRequest { rank },
                )
                .await
                .map_err(|error| effect_group_engine_error("EffectGroupIndex/read_rank", error))?;
        }
        let record = match read {
            EffectGroupReadRankResponse::Settled { settlement } => settlement,
            EffectGroupReadRankResponse::NotSettled => {
                return Err(group_shape_error(format!(
                    "effect group {} rank {rank} remained unsettled after its notification",
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
            EffectGroupCloseResponse::Closed | EffectGroupCloseResponse::AlreadyClosed => {
                self.closed_effect_groups
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .insert(group_key);
                Ok(())
            }
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

    async fn runtime_effect_failure_disposition(
        &self,
        _code: RuntimeErrorCode,
    ) -> Result<RuntimeEffectFailureDisposition, RuntimeError> {
        Ok(RuntimeEffectFailureDisposition::AbortInvocation)
    }

    async fn turn_control_participation(&self) -> Result<TurnControlParticipation, RuntimeError> {
        Ok(TurnControlParticipation::DurableJournaled)
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
                    lash_core::BoundaryReason::DurationCap => "duration_cap",
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
        match execution {
            RestateEffectExecution::DirectProcess {
                invocation,
                command,
            } => execute_restate_process_command(
                &self.context,
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
            RestateEffectExecution::DurableToolBatch { envelope } => {
                let run_envelope = envelope.clone();
                self.record_eager_effect(
                    &envelope,
                    Box::pin(async move { local_executor.execute(run_envelope).await }),
                )
                .await
            }
            RestateEffectExecution::Timer {
                invocation,
                duration_ms,
            } => {
                self.emit_trace(Some(&invocation), || {
                    lash_trace::TraceEvent::DurableTimerStarted { duration_ms }
                });
                let duration = Duration::from_millis(duration_ms);
                let RuntimeSleepOptions {
                    cancellation,
                    observe_turn_cancel,
                    turn_cancel_scope,
                } = local_executor.into_sleep_options();
                let turn_cancel = restate_timer_turn_cancel_wait_request(
                    &invocation,
                    observe_turn_cancel,
                    turn_cancel_scope.as_ref(),
                )?;
                match self
                    .context
                    .sleep_or_turn_cancel(duration, turn_cancel, cancellation.clone())
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
                        cancellation.cancel();
                        return Err(RuntimeEffectControllerError::from(
                            lash_core::StoreError::SessionDeleted { session_id },
                        ));
                    }
                    Ok(RestateTurnCancelRaceOutcome::TurnCancelled) => {
                        self.emit_trace(Some(&invocation), || {
                            lash_trace::TraceEvent::DurableTimerResolved {
                                duration_ms,
                                status: lash_trace::TraceDurableTimerStatus::Cancelled,
                            }
                        });
                        cancellation.cancel();
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
                            RuntimeErrorCode::RestateEffectController,
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
                if !restate_await_event_key_is_valid(&key) {
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
                let RuntimeAwaitEventOptions {
                    cancellation,
                    deadline,
                    clock,
                    observe_turn_cancel,
                    turn_cancel_scope,
                } = local_executor.into_await_event_options()?;
                let turn_cancel = restate_await_event_turn_cancel_wait_request(
                    &invocation,
                    observe_turn_cancel,
                    turn_cancel_scope.as_ref(),
                )?;
                self.emit_trace(Some(&invocation), || {
                    lash_trace::TraceEvent::DurableWaitParked {
                        wait_kind: "await_event".to_string(),
                    }
                });
                match self
                    .context
                    .await_event_or_turn_cancel(
                        restate_durable_wait_request(&key, deadline, clock.as_ref()),
                        turn_cancel,
                        cancellation.clone(),
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
                        cancellation.cancel();
                        Err(RuntimeEffectControllerError::from(
                            lash_core::StoreError::SessionDeleted { session_id },
                        ))
                    }
                    Ok(RestateTurnCancelRaceOutcome::TurnCancelled) => {
                        self.emit_trace(Some(&invocation), || {
                            lash_trace::TraceEvent::DurableWaitResolved {
                                wait_kind: "await_event".to_string(),
                                resolution: lash_trace::TraceDurableWaitResolution::TurnCancelled,
                            }
                        });
                        cancellation.cancel();
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
                            RuntimeErrorCode::RestateEffectController,
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
            RestateEffectExecution::JournaledRun { envelope } => {
                let reconstructed_envelope = envelope.canonical_form()?;
                let replay_trace = local_executor.replay_validation_trace().cloned();
                let invocation = envelope.invocation.clone();
                self.emit_trace(Some(&invocation), || {
                    lash_trace::TraceEvent::JournaledEffectStarted {
                        effect_name: restate_effect_name(&invocation),
                        effect_kind: trace_effect_kind(&invocation).to_string(),
                    }
                });
                let recorded_envelope = Arc::new(reconstructed_envelope.clone());
                let journaled_envelope = Arc::clone(&recorded_envelope);
                let recorded = self
                    .record_effect(
                        &invocation,
                        &recorded_envelope,
                        Box::pin(async move {
                            let outcome =
                                execute_restate_journaled_effect(envelope, local_executor).await;
                            RecordedRuntimeEffect {
                                envelope: journaled_envelope,
                                outcome,
                            }
                        }),
                    )
                    .await;
                let recorded = match recorded {
                    Ok(recorded) => recorded,
                    Err(error) => {
                        self.emit_trace(Some(&invocation), || {
                            lash_trace::TraceEvent::JournaledEffectSettled {
                                effect_name: restate_effect_name(&invocation),
                                effect_kind: trace_effect_kind(&invocation).to_string(),
                                status: lash_trace::TraceJournaledEffectStatus::Failed,
                            }
                        });
                        return Err(RuntimeEffectControllerError::new(
                            RuntimeErrorCode::RestateEffectController,
                            error.to_string(),
                        ));
                    }
                };
                let outcome = validate_recorded_effect_envelope(
                    recorded,
                    &reconstructed_envelope,
                    replay_trace.as_ref(),
                );
                let outcome = match outcome {
                    Ok(outcome) => outcome,
                    Err(error) => {
                        self.emit_trace(Some(&invocation), || {
                            lash_trace::TraceEvent::JournaledEffectSettled {
                                effect_name: restate_effect_name(&invocation),
                                effect_kind: trace_effect_kind(&invocation).to_string(),
                                status: lash_trace::TraceJournaledEffectStatus::Failed,
                            }
                        });
                        return Err(error);
                    }
                };
                self.emit_trace(Some(&invocation), || {
                    lash_trace::TraceEvent::JournaledEffectSettled {
                        effect_name: restate_effect_name(&invocation),
                        effect_kind: trace_effect_kind(&invocation).to_string(),
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

fn trace_effect_kind(invocation: &RuntimeInvocation) -> &'static str {
    invocation
        .effect_kind()
        .map_or("runtime_invocation", RuntimeEffectKind::as_str)
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

include!("process_command.rs");
async fn signal_ordinal_for_event(
    registry: &dyn ProcessRegistry,
    process_id: &str,
    event_type: &str,
    sequence: u64,
) -> Result<u64, PluginError> {
    // COUNT at the store, not a full log fetch: per-signal cost must stay
    // flat for long-lived processes that accumulate large event histories.
    registry
        .count_events_through(process_id, event_type, sequence)
        .await
}

async fn schedule_restate_process<'ctx, C>(
    registry: Arc<dyn ProcessRegistry>,
    registration: lash_core::ProcessRegistration,
    observers: Vec<String>,
    execution_context: lash_core::ProcessExecutionContext,
    context: &C,
) -> Result<ProcessRecord, PluginError>
where
    C: RestateControllerContext<'ctx> + ?Sized,
{
    let process_id = registration.id.clone();
    registry
        .register_process_with_observers(registration.clone(), &observers)
        .await?;
    let invocation_id = context
        .start_process_workflow(registration, execution_context)
        .await
        .map_err(|err| {
            PluginError::Runtime(RuntimeError::new(
                RuntimeErrorCode::RestateProcessIngressSubmit,
                format!("Restate process workflow start failed: {err}"),
            ))
        })?;
    registry
        .set_external_ref(
            &process_id,
            ProcessExternalRef {
                backend: "restate".to_string(),
                id: format!("LashProcessWorkflow/{process_id}"),
                metadata: Some(serde_json::json!({ "invocation_id": invocation_id })),
            },
        )
        .await
}

#[derive(Debug)]
pub(crate) enum RestateEffectExecution {
    DirectProcess {
        invocation: RuntimeInvocation,
        command: Box<ProcessCommand>,
    },
    DurableProcessCommand {
        invocation: RuntimeInvocation,
        command: Box<ProcessCommand>,
    },
    DirectLocal {
        envelope: RuntimeEffectEnvelope,
    },
    DurableToolBatch {
        envelope: RuntimeEffectEnvelope,
    },
    Timer {
        invocation: RuntimeInvocation,
        duration_ms: u64,
    },
    AwaitEvent {
        invocation: RuntimeInvocation,
        key: AwaitEventKey,
    },
    PeekAwaitEvent {
        invocation: RuntimeInvocation,
        key: AwaitEventKey,
    },
    JournaledRun {
        envelope: RuntimeEffectEnvelope,
    },
}

impl RestateEffectExecution {
    fn invocation(&self) -> &RuntimeInvocation {
        match self {
            Self::DirectProcess { invocation, .. }
            | Self::DurableProcessCommand { invocation, .. }
            | Self::Timer { invocation, .. }
            | Self::AwaitEvent { invocation, .. }
            | Self::PeekAwaitEvent { invocation, .. } => invocation,
            Self::DirectLocal { envelope }
            | Self::DurableToolBatch { envelope }
            | Self::JournaledRun { envelope } => &envelope.invocation,
        }
    }
}

/// Selects the Restate journal-command mapping for a Lash runtime effect.
///
/// # RT0016 journal-mismatch label warning
///
/// Restate SDK 0.10.0 reports command-type mismatches with the two human
/// labels swapped. `service_protocol/encoding.rs:96` constructs
/// `CommandTypeMismatchError` with `actual` equal to the journal entry popped
/// from `commands` and `expected` equal to the command the current execution
/// wants. `vm/errors.rs:188-200` then displays `expected` as what the previous
/// execution recorded and `actual` as what the current execution attempts.
///
/// Therefore, when reading RT0016, the line labelled "previous execution ran
/// and recorded" is actually this attempt's intended command, while "current
/// execution attempts" is actually the journal's recorded command. The
/// trailing `Command: ...[command index N]` metadata is captured from
/// `journal.last_command_metadata()` after `journal.transition(&expected)`, so
/// it also names the command this attempt was trying to write, not the
/// journal's contents. FIG-790 was the incident that exposed this SDK
/// diagnostic inversion.
///
/// Fallible because four of its arms rebuild the envelope into a target with no
/// slot for [`EffectGroupMembership`] — `Timer`, `AwaitEvent`, `PeekAwaitEvent`,
/// and both `Process` arms record no canonical envelope at all, so on this tier
/// those commands have no envelope-hash fence to fold a wake rule into. A grouped
/// child reaching them is refused rather than silently stripped of its
/// membership. Worth naming for the Restate layer: `Sleep` and `AwaitEvent` are
/// exactly the two children of the design's deadline/signal select, so this is
/// the refusal that layer must convert into real child invocations.
pub(crate) fn restate_effect_execution(
    envelope: RuntimeEffectEnvelope,
) -> Result<RestateEffectExecution, RuntimeEffectControllerError> {
    let RuntimeEffectEnvelope {
        invocation,
        command,
        group,
    } = envelope;
    Ok(match command {
        RuntimeEffectCommand::Process { command }
            if matches!(
                command.as_ref(),
                ProcessCommand::ParentEnd { .. } | ProcessCommand::Signal { .. }
            ) =>
        {
            refuse_unhonored_group_membership(group.as_deref(), "restate durable process command")?;
            RestateEffectExecution::DurableProcessCommand {
                invocation,
                command,
            }
        }
        RuntimeEffectCommand::Process { command } => {
            refuse_unhonored_group_membership(group.as_deref(), "restate direct process")?;
            RestateEffectExecution::DirectProcess {
                invocation,
                command,
            }
        }
        command @ RuntimeEffectCommand::ToolBatch { .. } => {
            RestateEffectExecution::DurableToolBatch {
                envelope: RuntimeEffectEnvelope {
                    invocation,
                    command,
                    group,
                },
            }
        }
        command @ RuntimeEffectCommand::ExecCode { .. } => RestateEffectExecution::DirectLocal {
            envelope: RuntimeEffectEnvelope {
                invocation,
                command,
                group,
            },
        },
        RuntimeEffectCommand::Sleep { duration_ms } => {
            refuse_unhonored_group_membership(group.as_deref(), "restate timer")?;
            RestateEffectExecution::Timer {
                invocation,
                duration_ms,
            }
        }
        RuntimeEffectCommand::AwaitEvent { key } => {
            refuse_unhonored_group_membership(group.as_deref(), "restate await event")?;
            RestateEffectExecution::AwaitEvent { invocation, key }
        }
        RuntimeEffectCommand::PeekAwaitEvent { key } => {
            refuse_unhonored_group_membership(group.as_deref(), "restate peek await event")?;
            RestateEffectExecution::PeekAwaitEvent { invocation, key }
        }
        command @ (RuntimeEffectCommand::LlmCall { .. }
        | RuntimeEffectCommand::AssistantResponseHooks { .. }
        | RuntimeEffectCommand::Direct { .. }
        | RuntimeEffectCommand::ToolAttempt { .. }
        | RuntimeEffectCommand::Trigger { .. }
        | RuntimeEffectCommand::LanguageRuntimeValue { .. }
        | RuntimeEffectCommand::AcceptTurnInput { .. }
        | RuntimeEffectCommand::Checkpoint { .. }
        | RuntimeEffectCommand::SyncExecutionEnvironment { .. }) => {
            RestateEffectExecution::JournaledRun {
                envelope: RuntimeEffectEnvelope {
                    invocation,
                    command,
                    group,
                },
            }
        }
    })
}

pub(crate) fn restate_effect_name(invocation: &RuntimeInvocation) -> String {
    if let Some(replay_key) = invocation.replay_key() {
        format!("lash:{replay_key}")
    } else if let (Some(kind), Some(effect_id)) = (invocation.effect_kind(), invocation.effect_id())
    {
        format!("lash:{}:{effect_id}", kind.as_str())
    } else {
        "lash:runtime-invocation".to_string()
    }
}

pub(crate) fn validate_recorded_effect_envelope(
    recorded: RecordedRuntimeEffect,
    reconstructed: &CanonicalRuntimeEffectEnvelope,
    trace: Option<&lash_core::facade_support::RuntimeEffectReplayTrace>,
) -> Result<Result<RuntimeEffectOutcome, RuntimeEffectControllerError>, RuntimeEffectControllerError>
{
    validate_replayed_effect_envelope(
        recorded.envelope.as_ref(),
        reconstructed,
        RuntimeErrorCode::RestateEffectHashMismatch,
        trace,
    )?;
    Ok(recorded.outcome)
}

fn tracing_sleep_error(invocation: &RuntimeInvocation, err: &TerminalError) {
    tracing::warn!(
        session_id = %invocation.scope.session_id,
        effect_id = invocation.effect_id().unwrap_or(""),
        effect_kind = %RuntimeEffectKind::Sleep.as_str(),
        error = %err,
        "Restate durable sleep failed"
    );
}
