//! Handler-scoped runtime-effect controller.
//!
//! One responsibility: map a Lash runtime effect onto the Restate journal
//! command that makes it durable — a `ctx.run` for journaled effects, a durable
//! timer for sleeps, a durable-wait call for await-events, and direct workflow
//! scheduling for process commands. The context seam those commands are issued
//! through lives in [`context`].

pub(crate) mod context;
mod journal_budget;
mod journaled_effect;

use std::fmt;
use std::marker::PhantomData;
use std::sync::{Arc, Mutex, Weak};
use std::time::Duration;

use lash_core::{
    AwaitEventKey, AwaitEventResolver, AwaitEventWaitIdentity, EffectHost, ExecutionScope,
    PluginError, ProcessAwaitOutput, ProcessCommand, ProcessEffectOutcome, ProcessExternalRef,
    ProcessRecord, ProcessRegistry, Resolution, ResolveOutcome, RuntimeEffectCommand,
    RuntimeEffectController, RuntimeEffectControllerError, RuntimeEffectEnvelope,
    RuntimeEffectKind, RuntimeEffectLocalExecutor, RuntimeEffectOutcome, RuntimeError,
    RuntimeErrorCode, RuntimeInvocation, ScopedEffectController,
    facade_support::CanonicalRuntimeEffectEnvelope, facade_support::RuntimeAwaitEventOptions,
    facade_support::RuntimeSleepOptions, facade_support::validate_replayed_effect_envelope,
};
use restate_sdk::context::RunRetryPolicy;
use restate_sdk::errors::TerminalError;
use serde::Serialize;

use crate::durable_wait::{
    RestateAwaitEventRaceOutcome, RestateDurableWaitAddress, RestateDurableWaitAwaitRequest,
    RestateDurableWaitResolveRequest, RestateSleepRaceOutcome, restate_await_event_key,
    restate_await_event_key_is_valid, restate_durable_wait_request, restate_unknown_or_revoked,
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
#[doc(hidden)]
#[derive(Clone, Debug, Serialize, serde::Deserialize)]
pub enum RestateProcessAwaitRaceOutcome {
    Terminal(Box<ProcessAwaitOutput>),
    TurnCancelled,
    SessionRevoked { session_id: String },
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
    #[error("Restate background scheduler error: {0}")]
    BackgroundScheduler(String),
}

impl RestateEffectError {
    pub(crate) fn into_plugin_error(self) -> PluginError {
        PluginError::Session(self.to_string())
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
            address: RestateDurableWaitAddress::for_key(key),
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
    fn replay_ownership(&self) -> lash_core::EffectReplayOwnership {
        lash_core::EffectReplayOwnership::Controller
    }

    /// Restate re-drives this handler invocation, so its retry policy - not a
    /// sleep inside one invocation - is the right place to pace a queued drain
    /// that found the session execution lane held by a live foreign executor.
    /// The deployment-level [`RestateEffectHost`](crate::RestateEffectHost)
    /// deliberately does not opt in: it serves requests from outside a handler,
    /// where nothing re-drives the caller.
    fn durable_workflow_controller(&self) -> bool {
        true
    }

    fn journal_addressing(&self) -> lash_core::EffectJournalAddressing {
        lash_core::EffectJournalAddressing::OrdinalAddressed
    }

    fn allows_process_lifetime_completion_keys(&self) -> bool {
        true
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
}

#[async_trait::async_trait]
impl<'ctx, C> RuntimeEffectController for RestateRuntimeEffectController<'ctx, C>
where
    C: RestateControllerContext<'ctx>,
{
    fn supports_concurrent_effects(&self) -> bool {
        false
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
        let execution = restate_effect_execution(envelope);
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
                            resolution: resolution.to_string(),
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
                let reconstructed_envelope = envelope.canonical_form()?;
                let recorded_envelope = Arc::new(reconstructed_envelope.clone());
                // The process command runs outside the run closure so its own
                // journal commands stay at the handler's journal level, which
                // makes the budget give-up this seam's own pre-flight duty: a
                // give-up decided afterwards would discard a completed process
                // command with no journal entry, and the next redrive would run
                // it again. The verdict is journaled ahead of the command so a
                // budget change between attempts cannot make a replay run it.
                let recorded = match self
                    .journaled_budget_give_up(&invocation, &recorded_envelope)
                    .await
                {
                    Some(gave_up) => gave_up,
                    None => {
                        let outcome = execute_restate_process_command(
                            &self.context,
                            &invocation,
                            *command,
                            local_executor,
                            |_| {},
                            |_, _| {},
                        )
                        .await
                        .map(|result| RuntimeEffectOutcome::Process { result });
                        let journaled_envelope = Arc::clone(&recorded_envelope);
                        self.record_effect(
                            &invocation,
                            &recorded_envelope,
                            Box::pin(async move {
                                RecordedRuntimeEffect {
                                    envelope: journaled_envelope,
                                    outcome,
                                }
                            }),
                        )
                        .await
                    }
                }
                .map_err(|error| {
                    RuntimeEffectControllerError::new(
                        RuntimeErrorCode::RestateEffectController,
                        error.to_string(),
                    )
                })?;
                validate_recorded_effect_envelope(recorded, &reconstructed_envelope, None)?
            }
            RestateEffectExecution::DirectLocal { envelope } => {
                local_executor.execute(envelope).await
            }
            RestateEffectExecution::DurableToolBatch { envelope } => {
                // Child attempts remain direct so they can emit their own journal
                // commands. Record the settled aggregate afterward: parent-end
                // teardown reconstructs exclusively from this durable outcome.
                let reconstructed_envelope = envelope.canonical_form()?;
                let invocation = envelope.invocation.clone();
                let recorded_envelope = Arc::new(reconstructed_envelope.clone());
                // The child attempts run outside the run closure, so the budget
                // give-up has to be decided before they run: giving up afterwards
                // would discard a settled batch that was never journaled and let
                // the next redrive re-execute every child. The verdict is
                // journaled ahead of the batch so a budget change between
                // attempts cannot make a replay run the children again.
                let recorded = match self
                    .journaled_budget_give_up(&invocation, &recorded_envelope)
                    .await
                {
                    Some(gave_up) => gave_up,
                    None => {
                        let outcome = local_executor.execute(envelope).await;
                        let journaled_envelope = Arc::clone(&recorded_envelope);
                        self.record_effect(
                            &invocation,
                            &recorded_envelope,
                            Box::pin(async move {
                                RecordedRuntimeEffect {
                                    envelope: journaled_envelope,
                                    outcome,
                                }
                            }),
                        )
                        .await
                    }
                }
                .map_err(|error| {
                    RuntimeEffectControllerError::new(
                        RuntimeErrorCode::RestateEffectController,
                        error.to_string(),
                    )
                })?;
                validate_recorded_effect_envelope(recorded, &reconstructed_envelope, None)?
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
                    Ok(RestateSleepRaceOutcome::Slept) => {}
                    Ok(RestateSleepRaceOutcome::Cancelled) => {
                        self.emit_trace(Some(&invocation), || {
                            lash_trace::TraceEvent::DurableTimerResolved {
                                duration_ms,
                                status: "cancelled".to_string(),
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
                                status: "failed".to_string(),
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
                        status: "resolved".to_string(),
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
                    Ok(RestateAwaitEventRaceOutcome::Event(resolution)) => {
                        self.emit_trace(Some(&invocation), || {
                            lash_trace::TraceEvent::DurableWaitResolved {
                                wait_kind: "await_event".to_string(),
                                resolution: resolution_trace_label(&resolution).to_string(),
                            }
                        });
                        Ok(RuntimeEffectOutcome::AwaitEvent { resolution })
                    }
                    Ok(RestateAwaitEventRaceOutcome::TurnCancelled) => {
                        self.emit_trace(Some(&invocation), || {
                            lash_trace::TraceEvent::DurableWaitResolved {
                                wait_kind: "await_event".to_string(),
                                resolution: "turn_cancelled".to_string(),
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
                                resolution: "failed".to_string(),
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
                                status: "failed".to_string(),
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
                                status: "failed".to_string(),
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
                            "completed"
                        } else {
                            "failed"
                        }
                        .to_string(),
                    }
                });
                outcome
            }
        }
    }
}

fn resolution_trace_label(resolution: &Resolution) -> &'static str {
    match resolution {
        Resolution::Ok(_) => "ok",
        Resolution::Err(_) => "error",
        Resolution::Timeout => "timeout",
        Resolution::Cancelled => "cancelled",
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
    } = envelope;
    match command {
        RuntimeEffectCommand::Trigger { command } => {
            local_executor.execute_trigger(invocation, *command).await
        }
        command => {
            local_executor
                .execute(RuntimeEffectEnvelope {
                    invocation,
                    command,
                })
                .await
        }
    }
}

async fn execute_restate_process_command<'ctx, C>(
    context: &C,
    invocation: &RuntimeInvocation,
    command: ProcessCommand,
    local_executor: RuntimeEffectLocalExecutor<'_>,
    trace_park: impl Fn(&'static str),
    trace_resolve: impl Fn(&'static str, &'static str),
) -> Result<ProcessEffectOutcome, RuntimeEffectControllerError>
where
    C: RestateControllerContext<'ctx> + ?Sized,
{
    let mut local_executor = local_executor;
    let outcome_observer = local_executor.take_process_outcome_observer();
    let execution = local_executor.into_process()?;
    let registry = execution.registry;
    let process_env_store = execution.process_env_store;
    let turn_cancellation = execution.turn_cancellation;
    let outcome = match command {
        ProcessCommand::Start {
            mut registration,
            observers,
            env_spec,
            execution_context,
        } => {
            if let Some(env_spec) = env_spec.as_ref() {
                let env_store = process_env_store.as_ref().ok_or_else(|| {
                    RuntimeEffectControllerError::foreign(
                        "process_env_store_unavailable",
                        "admitted Restate process start carries an execution environment but the executor has no environment store",
                    )
                })?;
                let env_ref =
                    lash_core::runtime::persist_process_execution_env(env_store.as_ref(), env_spec)
                        .await?;
                registration = registration.with_execution_env_ref(Some(env_ref));
            }
            let record = schedule_restate_process(
                registry,
                registration,
                observers,
                *execution_context,
                context,
            )
            .await?;
            Ok(ProcessEffectOutcome::Start {
                record: Box::new(record),
            })
        }
        ProcessCommand::List {
            session_scope,
            mode,
        } => {
            let entries = match mode {
                lash_core::ProcessListMode::Live => {
                    registry
                        .list_live_observed_by(&session_scope.session_id)
                        .await?
                }
                lash_core::ProcessListMode::All => {
                    registry.list_observed_by(&session_scope.session_id).await?
                }
            };
            Ok(ProcessEffectOutcome::List { entries })
        }
        ProcessCommand::Transfer {
            from_scope,
            to_scope,
            process_ids,
        } => {
            registry
                .transfer_observers(
                    &from_scope.session_id,
                    &to_scope.session_id,
                    &process_ids,
                    lash_core::ProcessObserverBy::host("restate-transfer"),
                )
                .await?;
            Ok(ProcessEffectOutcome::Transfer)
        }
        ProcessCommand::DeleteSession { session_id } => {
            let report = registry.delete_session_process_state(&session_id).await?;
            Ok(ProcessEffectOutcome::DeleteSession { report })
        }
        ProcessCommand::Await { process_id } => {
            // Replay-determinism class inventory: PR #166 removed the process
            // start gate. FIG-788 always redrives the process runner, retains
            // ordinal handovers until terminal delivery resolves, and schedules
            // each segment successor before reading cancellation. FIG-790 emits
            // Process::Await before observing state. FIG-793 emits LlmCall
            // before its durable cancel peek. FIG-806 makes TriggerRouter emit
            // the deterministic process start before consulting reservation
            // status. FIG-1126 keeps await-event key minting pure and performs
            // the revocation observation at the unconditional await boundary.
            //
            // This existence guard remains an explicit retention exposure, not
            // a proof: registration precedes the effect, and terminal events
            // plus weak-observer removal retain the row, but a host can prune a
            // terminal row while this invocation is still replayable. There is
            // no finite waiter-lifetime bound against which the raw prune cutoff
            // can be validated. In that case `get_process` returns
            // `Err(ProcessNoLongerRetained)` at `?`, not `Ok(None)` at this
            // branch. Hosts must retain terminal rows beyond every such waiter.
            if registry.get_process(&process_id).await?.is_none() {
                return Err(PluginError::Session(format!("unknown process `{process_id}`")).into());
            }
            let turn_cancel = restate_process_turn_cancel_wait_request(
                invocation,
                turn_cancellation.is_some(),
                turn_cancellation
                    .as_ref()
                    .map(|turn_cancellation| &turn_cancellation.scope),
            )?;
            trace_park("process");
            let first_wait = context
                .await_process_terminal_or_turn_cancel(process_id.clone(), turn_cancel)
                .await;
            let first_wait = match first_wait {
                Ok(outcome) => outcome,
                Err(err) => {
                    trace_resolve("process", "failed");
                    return Err(RuntimeEffectControllerError::new(
                        RuntimeErrorCode::RestateProcessAwait,
                        err.to_string(),
                    ));
                }
            };
            let output = match first_wait {
                RestateProcessAwaitRaceOutcome::Terminal(output) => {
                    trace_resolve("process", "resolved");
                    *output
                }
                RestateProcessAwaitRaceOutcome::TurnCancelled => {
                    trace_resolve("process", "turn_cancelled");
                    let Some(turn_cancellation) = turn_cancellation.as_ref() else {
                        return Err(RuntimeEffectControllerError::new(
                            RuntimeErrorCode::RestateProcessTurnCancelContextMissing,
                            "process-await cancellation won without turn-cancellation context",
                        ));
                    };
                    turn_cancellation.cancellation.cancel();
                    context
                        .request_process_workflow_cancel(RestateProcessCancelRequest {
                            process_id: process_id.clone(),
                            reason: Some("turn cancelled while awaiting process".to_string()),
                        })
                        .await
                        .map_err(|err| {
                            RestateEffectError::BackgroundScheduler(err.to_string())
                                .into_plugin_error()
                        })?;
                    trace_park("process_after_turn_cancel");
                    match context.await_process_terminal(process_id.clone()).await {
                        Ok(output) => {
                            trace_resolve("process_after_turn_cancel", "resolved");
                            output
                        }
                        Err(err) => {
                            trace_resolve("process_after_turn_cancel", "failed");
                            return Err(RuntimeEffectControllerError::new(
                                RuntimeErrorCode::RestateProcessAwaitAfterTurnCancel,
                                err.to_string(),
                            ));
                        }
                    }
                }
                RestateProcessAwaitRaceOutcome::SessionRevoked { session_id } => {
                    trace_resolve("process", "session_revoked");
                    return Err(lash_core::StoreError::SessionDeleted { session_id }.into());
                }
            };
            Ok(ProcessEffectOutcome::Await {
                output: Box::new(output),
            })
        }
        ProcessCommand::Cancel {
            process_id,
            reason,
            replay,
        } => {
            let record = registry
                .get_process(&process_id)
                .await?
                .ok_or_else(|| PluginError::Session(format!("unknown process `{process_id}`")))?;
            let mut request =
                lash_core::ProcessEventAppendRequest::cancel_requested(&process_id, reason.clone());
            if let Some(replay) = replay {
                request = request.with_optional_replay(Some(replay));
            }
            registry.append_event(&process_id, request).await?;
            context
                .request_process_workflow_cancel(RestateProcessCancelRequest { process_id, reason })
                .await
                .map_err(|err| {
                    RestateEffectError::BackgroundScheduler(err.to_string()).into_plugin_error()
                })?;
            Ok(ProcessEffectOutcome::Cancel {
                record: Box::new(record),
            })
        }
        ProcessCommand::ParentEnd {
            identity,
            process_id,
            policy,
            reason,
        } => {
            let outcome = match policy {
                lash_core::ProcessParentEndPolicy::Abandon => {
                    lash_core::ToolIntentParentEndOutcome::Abandoned {
                        identity,
                        process_id,
                    }
                }
                lash_core::ProcessParentEndPolicy::Cancel => {
                    let result: Result<(), lash_core::PluginError> = async {
                        registry.get_process(&process_id).await?.ok_or_else(|| {
                            lash_core::PluginError::Session(format!(
                                "unknown process `{process_id}`"
                            ))
                        })?;
                        registry
                            .append_event(
                                &process_id,
                                lash_core::ProcessEventAppendRequest::cancel_requested(
                                    &process_id,
                                    Some(reason.clone()),
                                ),
                            )
                            .await?;
                        context
                            .request_process_workflow_cancel(RestateProcessCancelRequest {
                                process_id: process_id.clone(),
                                reason: Some(reason),
                            })
                            .await
                            .map_err(|err| {
                                RestateEffectError::BackgroundScheduler(err.to_string())
                                    .into_plugin_error()
                            })?;
                        Ok(())
                    }
                    .await;
                    match result {
                        Ok(()) => lash_core::ToolIntentParentEndOutcome::Cancelled {
                            identity,
                            process_id,
                        },
                        Err(error) => {
                            let error = RuntimeEffectControllerError::from(error);
                            lash_core::ToolIntentParentEndOutcome::Refused {
                                identity,
                                process_id,
                                code: error.code.as_str().to_string(),
                                message: error.message,
                            }
                        }
                    }
                }
            };
            Ok(ProcessEffectOutcome::ParentEnd {
                outcome: Box::new(outcome),
            })
        }
        ProcessCommand::Signal {
            process_id,
            signal_name,
            request,
            ..
        } => {
            let result = registry.append_event(&process_id, request).await?;
            let ordinal = signal_ordinal_for_event(
                registry.as_ref(),
                &process_id,
                result.event.event_type.as_str(),
                result.event.sequence,
            )
            .await?;
            let key = restate_await_event_key(
                &ExecutionScope::process(process_id.clone()),
                AwaitEventWaitIdentity::process_signal(process_id.clone(), signal_name, ordinal),
            )
            .map_err(|err| PluginError::Session(err.to_string()))?;
            context
                .resolve_event(RestateDurableWaitResolveRequest {
                    address: RestateDurableWaitAddress::for_key(&key),
                    resolution: Resolution::Ok(result.event.payload.clone()),
                })
                .await
                .map_err(|err| {
                    RestateEffectError::BackgroundScheduler(err.to_string()).into_plugin_error()
                })?;
            Ok(ProcessEffectOutcome::Signal {
                event: Box::new(result.event),
            })
        }
        ProcessCommand::EmitEvent {
            process_id,
            request,
        } => {
            let result = registry.append_event(&process_id, request).await?;
            Ok(ProcessEffectOutcome::EmitEvent {
                event: Box::new(result.event),
                wake_delivery: result.wake_delivery.map(Box::new),
            })
        }
    };
    if let (Ok(outcome), Some(observer)) = (&outcome, outcome_observer) {
        observer(outcome);
    }
    outcome
}

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
            RestateEffectError::BackgroundScheduler(err.to_string()).into_plugin_error()
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
pub(crate) fn restate_effect_execution(envelope: RuntimeEffectEnvelope) -> RestateEffectExecution {
    let RuntimeEffectEnvelope {
        invocation,
        command,
    } = envelope;
    match command {
        RuntimeEffectCommand::Process { command }
            if matches!(
                command.as_ref(),
                ProcessCommand::ParentEnd { .. } | ProcessCommand::Signal { .. }
            ) =>
        {
            RestateEffectExecution::DurableProcessCommand {
                invocation,
                command,
            }
        }
        RuntimeEffectCommand::Process { command } => RestateEffectExecution::DirectProcess {
            invocation,
            command,
        },
        command @ RuntimeEffectCommand::ToolBatch { .. } => {
            RestateEffectExecution::DurableToolBatch {
                envelope: RuntimeEffectEnvelope {
                    invocation,
                    command,
                },
            }
        }
        command @ RuntimeEffectCommand::ExecCode { .. } => RestateEffectExecution::DirectLocal {
            envelope: RuntimeEffectEnvelope {
                invocation,
                command,
            },
        },
        RuntimeEffectCommand::Sleep { duration_ms } => RestateEffectExecution::Timer {
            invocation,
            duration_ms,
        },
        RuntimeEffectCommand::AwaitEvent { key } => {
            RestateEffectExecution::AwaitEvent { invocation, key }
        }
        RuntimeEffectCommand::PeekAwaitEvent { key } => {
            RestateEffectExecution::PeekAwaitEvent { invocation, key }
        }
        command @ (RuntimeEffectCommand::LlmCall { .. }
        | RuntimeEffectCommand::AssistantResponseHooks { .. }
        | RuntimeEffectCommand::Direct { .. }
        | RuntimeEffectCommand::ToolAttempt { .. }
        | RuntimeEffectCommand::Trigger { .. }
        | RuntimeEffectCommand::LanguageRuntimeValue { .. }
        | RuntimeEffectCommand::Checkpoint { .. }
        | RuntimeEffectCommand::SyncExecutionEnvironment { .. }) => {
            RestateEffectExecution::JournaledRun {
                envelope: RuntimeEffectEnvelope {
                    invocation,
                    command,
                },
            }
        }
    }
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
