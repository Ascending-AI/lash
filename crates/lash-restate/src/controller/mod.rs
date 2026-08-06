//! Handler-scoped runtime-effect controller.
//!
//! One responsibility: map a Lash runtime effect onto the Restate journal
//! command that makes it durable — a `ctx.run` for journaled effects, a durable
//! timer for sleeps, a durable-wait call for await-events, and direct workflow
//! scheduling for process commands. The context seam those commands are issued
//! through lives in [`context`].

pub(crate) mod context;

use std::fmt;
use std::future::Future;
use std::marker::PhantomData;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use lash_core::{
    AwaitEventKey, AwaitEventResolver, AwaitEventWaitIdentity, EffectHost, ExecutionScope,
    PluginError, ProcessAwaitOutput, ProcessCommand, ProcessEffectOutcome, ProcessExternalRef,
    ProcessRecord, ProcessRegistry, Resolution, ResolveOutcome, RuntimeEffectCommand,
    RuntimeEffectController, RuntimeEffectControllerError, RuntimeEffectEnvelope,
    RuntimeEffectKind, RuntimeEffectLocalExecutor, RuntimeEffectOutcome, RuntimeError,
    RuntimeInvocation, ScopedEffectController, facade_support::CanonicalRuntimeEffectEnvelope,
    facade_support::RuntimeAwaitEventOptions, facade_support::RuntimeSleepOptions,
    facade_support::validate_replayed_effect_envelope,
};
use restate_sdk::context::RunRetryPolicy;
use restate_sdk::errors::TerminalError;
use restate_sdk::serde::Json;
use serde::{Serialize, de::DeserializeOwned};

use crate::durable_wait::{
    RestateAwaitEventRaceOutcome, RestateDurableWaitAddress, RestateDurableWaitAwaitRequest,
    RestateDurableWaitResolveRequest, RestateSleepRaceOutcome, restate_await_event_key,
    restate_await_event_key_is_valid, restate_durable_wait_request, restate_unknown_or_revoked,
};
use crate::process::RestateProcessCancelRequest;

pub use context::RestateControllerContext;

/// Configuration for [`RestateRuntimeEffectController`].
#[derive(Clone)]
pub struct RestateEffectControllerOptions {
    run_retry_policy: Option<RunRetryPolicy>,
    segment_duration_cap: Option<Duration>,
    segment_effect_budget: u64,
}

impl Default for RestateEffectControllerOptions {
    fn default() -> Self {
        Self {
            run_retry_policy: None,
            segment_duration_cap: None,
            segment_effect_budget: 10_000,
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
}

impl fmt::Debug for RestateEffectControllerOptions {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RestateEffectControllerOptions")
            .field("run_retry_policy", &self.run_retry_policy)
            .field("segment_duration_cap", &self.segment_duration_cap)
            .field("segment_effect_budget", &self.segment_effect_budget)
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
        .map_err(|err| RuntimeError::new("restate_await_event_resolve", err.to_string()))
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
            "restate_turn_cancel_scope_missing",
            "turn effects that observe cancellation require a durable turn-cancel scope",
        ));
    };
    if matches!(scope, ExecutionScope::Process { .. }) {
        return Ok(None);
    }
    let scope @ ExecutionScope::Turn { .. } = scope else {
        return Err(RuntimeEffectControllerError::new(
            "restate_turn_cancel_scope_mismatch",
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
            "restate_turn_cancel_scope_mismatch",
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
            _ctx: PhantomData,
        }
    }

    pub fn context(&self) -> &C {
        &self.context
    }

    pub fn options(&self) -> &RestateEffectControllerOptions {
        &self.options
    }
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

    async fn record_effect<'run, T>(
        &'run self,
        metadata: RuntimeInvocation,
        // Keep the full journaled-effect executor behind one allocation. The
        // Restate SDK stores this future in its ctx.run state machine, so
        // accepting it inline here makes every composed turn carry the whole
        // executor frame through the durable adapter.
        future: Pin<Box<dyn Future<Output = T> + Send + 'run>>,
    ) -> Result<T, RestateEffectError>
    where
        'ctx: 'run,
        T: Serialize + DeserializeOwned + Send + 'static,
    {
        let effect_name = restate_effect_name(&metadata);
        let run_retry_policy = self.options.run_retry_policy.clone();
        let Json(value) = self
            .context
            .run_json_send(effect_name.clone(), run_retry_policy, future)
            .await
            .map_err(|source| RestateEffectError::Terminal {
                effect: effect_name,
                terminal: source,
            })?;
        Ok(value)
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
    fn replay_ownership(&self) -> lash_core::EffectReplayOwnership {
        lash_core::EffectReplayOwnership::Controller
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
        if let Some(session_id) = scope.session_id()
            && self
                .context
                .session_is_revoked(session_id.to_string())
                .await
                .map_err(|err| {
                    RuntimeError::new("restate_await_event_revocation_read", err.to_string())
                })?
        {
            return Err(restate_unknown_or_revoked());
        }
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
        if let Some(session_id) = key.scope.session_id()
            && self
                .context
                .session_is_revoked(session_id.to_string())
                .await
                .map_err(|err| {
                    RuntimeError::new("restate_await_event_revocation_read", err.to_string())
                })?
        {
            return Err(restate_unknown_or_revoked());
        }
        self.context
            .peek_event(RestateDurableWaitAddress::for_key(key))
            .await
            .map_err(|err| RuntimeError::new("restate_effect_controller", err.to_string()))
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
        if let Some(session_id) = key.scope.session_id()
            && self
                .context
                .session_is_revoked(session_id.to_string())
                .await
                .map_err(|err| {
                    RuntimeError::new("restate_await_event_revocation_read", err.to_string())
                })?
        {
            return Err(restate_unknown_or_revoked());
        }
        let clock = lash_core::facade_support::SystemClock;
        self.context
            .await_event(restate_durable_wait_request(key, deadline, &clock), cancel)
            .await
            .map_err(|err| RuntimeError::new("restate_effect_controller", err.to_string()))
    }

    async fn revoke_await_events_for_session(&self, session_id: &str) -> Result<(), RuntimeError> {
        self.context
            .update_session_waits(session_id.to_string(), true)
            .await
            .map_err(|err| RuntimeError::new("restate_await_event_revoke", err.to_string()))
    }

    async fn cancel_await_events_for_session(&self, session_id: &str) -> Result<(), RuntimeError> {
        self.context
            .update_session_waits(session_id.to_string(), false)
            .await
            .map_err(|err| RuntimeError::new("restate_await_event_cancel", err.to_string()))
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
        (progress.effects_executed >= self.options.segment_effect_budget)
            .then_some(lash_core::BoundaryReason::JournalBudget)
    }

    async fn execute_effect(
        &self,
        envelope: RuntimeEffectEnvelope,
        local_executor: RuntimeEffectLocalExecutor<'_>,
    ) -> Result<RuntimeEffectOutcome, RuntimeEffectControllerError> {
        match restate_effect_execution(&envelope.command) {
            RestateEffectExecution::DirectProcess => {
                let RuntimeEffectEnvelope {
                    invocation,
                    command: RuntimeEffectCommand::Process { command },
                } = envelope
                else {
                    unreachable!("direct process execution is only selected for process effects");
                };
                execute_restate_process_command(
                    &self.context,
                    &invocation,
                    *command,
                    local_executor,
                )
                .await
                .map(|result| RuntimeEffectOutcome::Process { result })
            }
            RestateEffectExecution::DirectLocal => local_executor.execute(envelope).await,
            RestateEffectExecution::Timer => {
                let RuntimeEffectCommand::Sleep { duration_ms } = &envelope.command else {
                    unreachable!("timer execution is only selected for sleep effects");
                };
                let duration = Duration::from_millis(*duration_ms);
                let RuntimeSleepOptions {
                    cancellation,
                    observe_turn_cancel,
                    turn_cancel_scope,
                } = local_executor.into_sleep_options();
                let turn_cancel = restate_timer_turn_cancel_wait_request(
                    &envelope.invocation,
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
                        cancellation.cancel();
                        return Err(RuntimeEffectControllerError::new(
                            "runtime_effect_sleep_cancelled",
                            "runtime effect sleep was cancelled",
                        ));
                    }
                    Err(err) => {
                        tracing_sleep_error(&envelope.invocation, &err);
                        return Err(RuntimeEffectControllerError::new(
                            "restate_effect_controller",
                            err.to_string(),
                        ));
                    }
                }
                Ok(RuntimeEffectOutcome::Sleep)
            }
            RestateEffectExecution::AwaitEvent => {
                let RuntimeEffectCommand::AwaitEvent { key } = envelope.command else {
                    unreachable!("await-event execution is only selected for event waits");
                };
                let RuntimeAwaitEventOptions {
                    cancellation,
                    deadline,
                    clock,
                    observe_turn_cancel,
                    turn_cancel_scope,
                } = local_executor.into_await_event_options()?;
                let turn_cancel = restate_await_event_turn_cancel_wait_request(
                    &envelope.invocation,
                    observe_turn_cancel,
                    turn_cancel_scope.as_ref(),
                )?;
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
                        Ok(RuntimeEffectOutcome::AwaitEvent { resolution })
                    }
                    Ok(RestateAwaitEventRaceOutcome::TurnCancelled) => {
                        cancellation.cancel();
                        Ok(RuntimeEffectOutcome::AwaitEvent {
                            resolution: Resolution::Cancelled,
                        })
                    }
                    Err(err) => Err(RuntimeEffectControllerError::new(
                        "restate_effect_controller",
                        err.to_string(),
                    )),
                }
            }
            RestateEffectExecution::PeekAwaitEvent => {
                let RuntimeEffectCommand::PeekAwaitEvent { key } = envelope.command else {
                    unreachable!("peek execution is only selected for await-event reads");
                };
                self.peek_await_event(&key)
                    .await
                    .map(|resolution| RuntimeEffectOutcome::PeekAwaitEvent { resolution })
                    .map_err(RuntimeEffectControllerError::from)
            }
            RestateEffectExecution::JournaledRun => {
                let reconstructed_envelope = envelope.canonical_form()?;
                let replay_trace = local_executor.replay_validation_trace().cloned();
                let invocation = envelope.invocation.clone();
                let recorded_envelope = Arc::new(reconstructed_envelope.clone());
                let recorded = self
                    .record_effect(
                        invocation,
                        Box::pin(async move {
                            let outcome =
                                execute_restate_journaled_effect(envelope, local_executor).await;
                            RecordedRuntimeEffect {
                                envelope: recorded_envelope,
                                outcome,
                            }
                        }),
                    )
                    .await
                    .map_err(|err| {
                        RuntimeEffectControllerError::new(
                            "restate_effect_controller",
                            err.to_string(),
                        )
                    })?;
                validate_recorded_effect_envelope(
                    recorded,
                    &reconstructed_envelope,
                    replay_trace.as_ref(),
                )?
            }
        }
    }
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
) -> Result<ProcessEffectOutcome, RuntimeEffectControllerError>
where
    C: RestateControllerContext<'ctx> + ?Sized,
{
    let execution = local_executor.into_process()?;
    let registry = execution.registry;
    let turn_cancellation = execution.turn_cancellation;
    match command {
        ProcessCommand::Start {
            registration,
            observers,
            execution_context,
        } => {
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
            // status.
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
            let output = match context
                .await_process_terminal_or_turn_cancel(process_id.clone(), turn_cancel)
                .await
                .map_err(|err| {
                    RuntimeEffectControllerError::new("restate_process_await", err.to_string())
                })? {
                RestateProcessAwaitRaceOutcome::Terminal(output) => *output,
                RestateProcessAwaitRaceOutcome::TurnCancelled => {
                    let Some(turn_cancellation) = turn_cancellation.as_ref() else {
                        return Err(RuntimeEffectControllerError::new(
                            "restate_process_turn_cancel_context_missing",
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
                    context
                        .await_process_terminal(process_id.clone())
                        .await
                        .map_err(|err| {
                            RuntimeEffectControllerError::new(
                                "restate_process_await_after_turn_cancel",
                                err.to_string(),
                            )
                        })?
                }
                RestateProcessAwaitRaceOutcome::SessionRevoked { session_id } => {
                    return Err(lash_core::StoreError::SessionDeleted { session_id }.into());
                }
            };
            Ok(ProcessEffectOutcome::Await {
                output: Box::new(output),
            })
        }
        ProcessCommand::Cancel { process_id, reason } => {
            let record = registry
                .get_process(&process_id)
                .await?
                .ok_or_else(|| PluginError::Session(format!("unknown process `{process_id}`")))?;
            registry
                .append_event(
                    &process_id,
                    lash_core::ProcessEventAppendRequest::cancel_requested(
                        &process_id,
                        reason.clone(),
                    ),
                )
                .await?;
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
    }
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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum RestateEffectExecution {
    DirectProcess,
    DirectLocal,
    Timer,
    AwaitEvent,
    PeekAwaitEvent,
    JournaledRun,
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
pub(crate) fn restate_effect_execution(command: &RuntimeEffectCommand) -> RestateEffectExecution {
    match command {
        RuntimeEffectCommand::Process { .. } => RestateEffectExecution::DirectProcess,
        RuntimeEffectCommand::ToolBatch { .. } | RuntimeEffectCommand::ExecCode { .. } => {
            RestateEffectExecution::DirectLocal
        }
        RuntimeEffectCommand::Sleep { .. } => RestateEffectExecution::Timer,
        RuntimeEffectCommand::AwaitEvent { .. } => RestateEffectExecution::AwaitEvent,
        RuntimeEffectCommand::PeekAwaitEvent { .. } => RestateEffectExecution::PeekAwaitEvent,
        RuntimeEffectCommand::LlmCall { .. }
        | RuntimeEffectCommand::Direct { .. }
        | RuntimeEffectCommand::ToolAttempt { .. }
        | RuntimeEffectCommand::Trigger { .. }
        | RuntimeEffectCommand::Checkpoint { .. }
        | RuntimeEffectCommand::SyncExecutionEnvironment { .. } => {
            RestateEffectExecution::JournaledRun
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
        "restate_effect_hash_mismatch",
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
