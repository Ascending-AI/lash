//! Handler-scoped runtime-effect controller.
//!
//! One responsibility: map a Lash runtime effect onto the Restate journal
//! command that makes it durable — a `ctx.run` for journaled effects, a durable
//! timer for sleeps, a durable-wait call for await-events, and direct workflow
//! scheduling for process commands. The context seam those commands are issued
//! through lives in [`context`].

use lash_sansio::SessionId;
mod await_event;
pub(crate) mod context;
pub(crate) mod effect_journal;
mod group_child_cancel;
mod group_commit;
use group_child_cancel::group_child_cancelled;
mod group_read;
pub(crate) mod journal_budget;
mod journal_payload;
mod journaled_effect;
use journaled_effect::EngineFaults;
mod live_frontier;
mod run_record;
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
use std::sync::{Arc, Mutex};
use std::time::Duration;

use lash_core::{
    AwaitEventKey, AwaitEventWaitIdentity, EffectGroupHandle, EffectHost, ExecutionScope,
    GroupSettlement, LoserPolicy, PluginError, ProcessCommand, ProcessEffectOutcome,
    ProcessExternalRef, ProcessRecord, ProcessRegistry, RankedGroupSettlement, Resolution,
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
use crate::durable_wait::{RestateDurableWaitAwaitRequest, restate_durable_wait_request};
use crate::effect_group::{
    EffectGroupCloseOutcome, EffectGroupCloseRequest, EffectGroupCloseResponse,
    EffectGroupDispatchRequest, EffectGroupNotice, EffectGroupNotification, EffectGroupOpenRequest,
    EffectGroupOpenResponse, EffectGroupProbeResponse, EffectGroupShape, group_shape_error,
};
use crate::ingress::RestateAuthorityId;
use crate::process::RestateProcessCancelRequest;

pub(crate) use live_frontier::LiveFrontier;

pub use context::{
    GroupChildCancelArm, GroupChildCancelRace, ProcessCancelRace, RestateControllerContext,
    SignalWaitOutcome, TurnSleepOutcome, TurnWaitOutcome,
};

/// How a controller observes its own handling of the effects it executes.
struct RestateTraceObserver {
    tracing: lash_core::facade_support::TraceRuntime,
    /// The effect this controller was handed last: what a decision it makes
    /// between effects (a segment boundary) is attributed to.
    current: Mutex<Option<CurrentEffectTrace>>,
}

#[derive(Clone)]
struct CurrentEffectTrace {
    /// What the shift that issued the effect lent it: the controller's own
    /// records stand where that shift stands.
    issue: lash_core::facade_support::StepIssue,
    standing: lash_core::facade_support::TraceStanding,
    context: lash_trace::TraceContext,
}

/// The records a journaled run makes of itself. They are made inside its
/// recorded body, so a replay that serves the run's entry makes none.
pub(super) struct JournaledRunTrace {
    tracing: lash_core::facade_support::TraceRuntime,
    issue: lash_core::facade_support::StepIssue,
    scope: Option<lash_trace::DurableTraceScope>,
    context: lash_trace::TraceContext,
    effect_name: String,
    effect_kind: String,
}

/// A journaled run whose body has started.
pub(super) struct StartedRunTrace {
    standing: lash_core::facade_support::TraceStanding,
    context: lash_trace::TraceContext,
    effect_name: String,
    effect_kind: String,
}

impl JournaledRunTrace {
    /// Called where the body starts running.
    pub(super) fn started(self) -> StartedRunTrace {
        let live = self.issue.begin_native();
        let started = StartedRunTrace {
            standing: self.tracing.body(self.scope, &live),
            context: self.context,
            effect_name: self.effect_name,
            effect_kind: self.effect_kind,
        };
        started.standing.observe(|| {
            (
                started.context.clone(),
                lash_trace::TraceEvent::JournaledEffectStarted {
                    effect_name: started.effect_name.clone(),
                    effect_kind: started.effect_kind.clone(),
                },
            )
        });
        started
    }
}

impl StartedRunTrace {
    /// Called where the body ends, with whether its outcome was a success.
    pub(super) fn settled(self, completed: bool) {
        self.standing.observe(|| {
            (
                self.context,
                lash_trace::TraceEvent::JournaledEffectSettled {
                    effect_name: self.effect_name,
                    effect_kind: self.effect_kind,
                    status: if completed {
                        lash_trace::TraceJournaledEffectStatus::Completed
                    } else {
                        lash_trace::TraceJournaledEffectStatus::Failed
                    },
                },
            )
        });
    }
}

/// Configuration for [`RestateRuntimeEffectController`].
#[derive(Clone)]
pub struct RestateEffectControllerOptions {
    run_retry_policy: Option<RunRetryPolicy>,
    segment_effect_budget: u64,
    journaled_effect_byte_budget: Option<u64>,
    /// Whether this controller executes a process segment, whose waits that
    /// observe no turn race the segment's durable cancel promise and whose
    /// cancel peeks read it (FIG-3673).
    process_cancel: context::ProcessCancelRace,
    /// The generation that admitted the segment this controller executes: its
    /// signal waits also race the segment's hand-over promise, and hand over
    /// to a drain wake naming this generation (FIG-3799). Shared, so every
    /// controller future that holds the options stays a pointer wider.
    segment_generation: Option<Arc<lash_core::engine::BuildGeneration>>,
    /// The cancel fact of the effect-group child this controller executes
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

    /// Mark the controller as a process segment's shift (FIG-3673): a wait
    /// it issues that observes no turn races the segment's durable cancel
    /// promise, and [`observe_process_cancel`](RuntimeEffectController::observe_process_cancel)
    /// is a journaled peek of that promise. Only the process workflow sets
    /// it, on the workflow context whose promise it is.
    pub(crate) fn process_segment_drive(mut self) -> Self {
        self.process_cancel = context::ProcessCancelRace::Raced;
        self
    }

    /// Mark a process segment's shift as admitted under `generation`
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
            RestateEffectError::Terminal { ref terminal, .. } => {
                crate::wire::typed_terminal(terminal.message()).unwrap_or_else(|| {
                    Self::new(RuntimeErrorCode::EngineEffectController, error.to_string())
                })
            }
            RestateEffectError::Refused(refusal) => refusal,
        }
    }
}

/// Lash [`RuntimeEffectController`] and [`EffectHost`] backed by a Restate handler context.
///
/// This type is intentionally handler-scoped.
pub struct RestateRuntimeEffectController<'ctx, C> {
    context: C,
    attempt: Option<lash_trace::AttemptObservation>,
    authority_id: RestateAuthorityId,
    options: RestateEffectControllerOptions,
    trace: Option<RestateTraceObserver>,
    /// The drain generation of the build whose handler runs this controller
    /// (FIG-3795, FIG-4454): an effect group it opens dispatches on that
    /// build's lane, so the group's children run on the build that opened
    /// it, and the processes it starts carry it as their sender. Every
    /// controller names one — a lash handler's and one a host builds inside
    /// its own handler alike — so no group dispatches on a stable route.
    build_generation: lash_core::engine::BuildGeneration,
    /// The generation sentinel its first recorded entry carries (FIG-3980).
    folded_sentinel: Option<Arc<crate::sentinel::FoldedSentinel>>,
    /// The namespace of the deployment whose services this controller calls
    /// (FIG-3898): the durable waits, process workflow and effect groups it
    /// addresses are that namespace's.
    namespace: crate::RestateNamespace,
    /// The ranks this controller's run reads served (FIG-4088).
    read_ahead: group_read::GroupReadAhead,
    payloads: journal_payload::JournalPayloads,
    _ctx: PhantomData<&'ctx ()>,
}

impl<'ctx, C> RestateRuntimeEffectController<'ctx, C> {
    /// A controller over `context`, journaling under `authority_id`, of the
    /// build whose drain generation is `build_generation` — the engine's
    /// [`build_generation`](crate::RestateEngine::build_generation), whose
    /// lanes the deployment's endpoint binds.
    pub fn new(
        context: C,
        authority_id: RestateAuthorityId,
        build_generation: lash_core::engine::BuildGeneration,
    ) -> Self {
        Self::with_options(
            context,
            authority_id,
            build_generation,
            RestateEffectControllerOptions::default(),
        )
    }

    pub fn with_options(
        context: C,
        authority_id: RestateAuthorityId,
        build_generation: lash_core::engine::BuildGeneration,
        options: RestateEffectControllerOptions,
    ) -> Self {
        Self {
            context,
            attempt: crate::serve::current_attempt_observation(),
            authority_id,
            options,
            trace: None,
            build_generation,
            folded_sentinel: None,
            namespace: crate::RestateNamespace::default(),
            read_ahead: group_read::GroupReadAhead::default(),
            payloads: journal_payload::JournalPayloads::default(),
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

    #[cfg(test)]
    pub(crate) fn new_for_test(context: C) -> Self {
        Self::new(
            context,
            RestateAuthorityId::new("lash-restate-tests").expect("valid test authority"),
            crate::tests::test_build_generation(),
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
            crate::tests::test_build_generation(),
            options,
        )
    }

    /// Observe this controller's durable steps through the runtime's trace
    /// handle.
    ///
    /// Trace emission is best-effort and never crosses the Restate context
    /// seam: the journal remains truth. A step this controller serves from
    /// its journal is not observed again; its records are made by the
    /// attempt that really ran it.
    pub fn with_tracing(mut self, tracing: lash_core::facade_support::TraceRuntime) -> Self {
        self.trace = Some(RestateTraceObserver {
            tracing,
            current: Mutex::new(None),
        });
        self
    }

    pub fn context(&self) -> &C {
        &self.context
    }

    pub fn options(&self) -> &RestateEffectControllerOptions {
        &self.options
    }

    fn observer(&self) -> Option<&RestateTraceObserver> {
        self.trace
            .as_ref()
            .filter(|trace| trace.tracing.is_observed())
    }

    fn effect_scope(
        issue: &lash_core::facade_support::StepIssue,
    ) -> Option<lash_trace::DurableTraceScope> {
        issue.scope().cloned()
    }

    /// Observes this controller's handling of `invocation`, or, with none, a
    /// decision it made after the effect it was handed last.
    fn emit_trace(
        &self,
        invocation: Option<&RuntimeEffectInvocation>,
        event: impl FnOnce() -> lash_trace::TraceEvent,
    ) {
        let Some(trace) = self.observer() else {
            return;
        };
        let Some(current) = trace
            .current
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
        else {
            return;
        };
        match invocation {
            Some(invocation) => trace
                .tracing
                .issued(Self::effect_scope(&current.issue), &current.issue)
                .observe(|| {
                    (
                        trace_context_for_runtime_effect_invocation(
                            lash_trace::TraceContext::default(),
                            invocation,
                        ),
                        event(),
                    )
                }),
            None => current
                .standing
                .observe(|| (current.context.clone(), event())),
        }
    }

    fn remember_trace_invocation(
        &self,
        invocation: &RuntimeEffectInvocation,
        issue: &lash_core::facade_support::StepIssue,
    ) {
        let Some(trace) = self.observer() else {
            return;
        };
        *trace
            .current
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(CurrentEffectTrace {
            issue: issue.clone(),
            standing: trace.tracing.issued(Self::effect_scope(issue), issue),
            context: trace_context_for_runtime_effect_invocation(
                lash_trace::TraceContext::default(),
                invocation,
            ),
        });
    }

    /// The records the journaled run of `invocation` makes of itself, when
    /// anything observes them.
    pub(super) fn journaled_run_trace(
        &self,
        invocation: &RuntimeEffectInvocation,
        effect_kind: lash_core::RuntimeEffectKind,
        issue: &lash_core::facade_support::StepIssue,
    ) -> Option<JournaledRunTrace> {
        let trace = self.observer()?;
        Some(JournaledRunTrace {
            tracing: trace.tracing.clone(),
            issue: issue.clone(),
            scope: Self::effect_scope(issue),
            context: trace_context_for_runtime_effect_invocation(
                lash_trace::TraceContext::default(),
                invocation,
            ),
            effect_name: restate_effect_name(invocation),
            effect_kind: effect_kind.as_str().to_string(),
        })
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
            .generation(
                crate::LashService::EffectGroupDispatch,
                self.build_generation.clone(),
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
                // The group index's own readiness notice (FIG-4344).
                let notification = match self
                    .context
                    .await_effect_group_notice(
                        &self.namespace,
                        group_key.clone(),
                        EffectGroupNotice::Ready,
                        None,
                        context::ProcessCancelRace::NotRaced,
                    )
                    .await
                    .map_err(|error| {
                        effect_group_engine_error("EffectGroupIndex/subscribe(Ready)", error)
                    })? {
                    RestateTurnCancelRaceOutcome::Completed(notification) => notification,
                    RestateTurnCancelRaceOutcome::TurnCancelled
                    | RestateTurnCancelRaceOutcome::ProcessCancelled
                    | RestateTurnCancelRaceOutcome::SessionRevoked { .. } => {
                        return Err(group_shape_error(format!(
                            "opening effect group {group_key} was cancelled while awaiting READY"
                        )));
                    }
                };
                match notification {
                    EffectGroupNotification::Ready => Ok(handle),
                    EffectGroupNotification::Refused { reason } => Err(group_shape_error(format!(
                        "effect group {group_key} routing was refused: {reason:?}"
                    ))),
                    EffectGroupNotification::Retired => Err(group_shape_error(format!(
                        "effect group {group_key} was retired before it became ready"
                    ))),
                    other => Err(group_shape_error(format!(
                        "effect group {group_key} READY wait resolved as {other:?}"
                    ))),
                }
            }
            EffectGroupOpenResponse::ReopenedReady => Ok(handle),
            EffectGroupOpenResponse::ReopenedClosed { effective } => match effective {
                EffectGroupCloseOutcome::Refused { reason } => Err(group_shape_error(format!(
                    "effect group {group_key} routing was refused: {reason:?}"
                ))),
                EffectGroupCloseOutcome::RunToCompletion | EffectGroupCloseOutcome::Cancel => {
                    Ok(handle)
                }
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
    fn attempt_observation(&self) -> Option<lash_trace::AttemptObservation> {
        self.attempt.clone()
    }

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

    async fn await_group_child_drain_admission(
        &self,
        group_key: &str,
        rank: u64,
    ) -> Result<(), RuntimeEffectControllerError> {
        group_commit::await_group_child_drain_admission(
            &self.context,
            &self.namespace,
            group_key,
            rank,
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
    /// process segment's shift, which never reads `lent_stop`. Any other
    /// controller executes no process segment and records no process
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
                    crate::wire::lash_terminal(&err, RuntimeErrorCode::EngineProcessCancel)
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
                crate::process::journal_or_retry(step.await)
            })
            .await
            .map_err(|err| {
                crate::wire::lash_terminal(&err, RuntimeErrorCode::EngineEffectController)
            })?;
        recorded.map_err(RuntimeEffectControllerError::from)
    }

    /// One `ctx.run` step named `name` holding a Run record and the material
    /// it owns (FIG-4877): journaled once, served on every replay.
    async fn record_run_schedule(
        &self,
        name: String,
        step: lash_core::RunRecordStep<'_>,
    ) -> Result<lash_core::tool_run::RunJournalEntry, lash_core::RuntimeEffectControllerError> {
        self.journal_run_schedule(name, step).await
    }

    fn start_run_attempt<'run>(
        &'run self,
        name: String,
        step: lash_core::tool_dispatch::RunAttemptStep<'run>,
    ) -> lash_core::tool_dispatch::RunAttemptHandle<'run> {
        self.start_journal_run_attempt(name, step)
    }

    fn start_run_retry(&self, backoff_ms: u64) -> lash_core::tool_dispatch::RunRetryTimer<'_> {
        let timer = self
            .context
            .start_sleep_send(std::time::Duration::from_millis(backoff_ms));
        Box::pin(async move {
            timer.await.map_err(|error| {
                crate::wire::lash_terminal(
                    &error,
                    lash_core::RuntimeErrorCode::EngineEffectController,
                )
            })
        })
    }

    async fn arm_run_source(
        &self,
        descriptor: lash_core::tool_run::SourceDescriptor,
    ) -> Result<(), RuntimeEffectControllerError> {
        if !restate_await_event_key_is_valid_for_authority(&self.authority_id, &descriptor.source) {
            return Err(restate_unknown_or_revoked().into());
        }
        self.context
            .arm_run_source(&self.namespace, descriptor)
            .await
            .map_err(|error| {
                crate::wire::lash_terminal(&error, RuntimeErrorCode::EngineEffectController)
            })
    }

    async fn attach_run_process_terminal(
        &self,
        descriptor: lash_core::tool_run::SourceDescriptor,
    ) -> Result<(), RuntimeEffectControllerError> {
        if !restate_await_event_key_is_valid_for_authority(&self.authority_id, &descriptor.source) {
            return Err(restate_unknown_or_revoked().into());
        }
        self.context
            .attach_run_process_terminal(&self.namespace, descriptor)
            .await
            .map_err(|error| {
                crate::wire::lash_terminal(&error, RuntimeErrorCode::EngineProcessAwait)
            })
    }

    async fn cancel_run_source(
        &self,
        descriptor: lash_core::tool_run::SourceDescriptor,
    ) -> Result<lash_core::tool_run::SourceSeal, RuntimeEffectControllerError> {
        if !restate_await_event_key_is_valid_for_authority(&self.authority_id, &descriptor.source) {
            return Err(restate_unknown_or_revoked().into());
        }
        self.context
            .cancel_run_source(&self.namespace, descriptor)
            .await
            .map_err(|error| {
                crate::wire::lash_terminal(&error, RuntimeErrorCode::EngineEffectController)
            })
    }

    async fn await_run_sources(
        &self,
        subscriptions: Vec<lash_core::tool_run::SourceSubscription>,
        cancel: lash_core::TurnCancelWait,
    ) -> Result<(usize, lash_core::tool_run::SourceSeal), RuntimeEffectControllerError> {
        if subscriptions.iter().any(|subscription| {
            !restate_await_event_key_is_valid_for_authority(
                &self.authority_id,
                &subscription.source,
            )
        }) {
            return Err(restate_unknown_or_revoked().into());
        }
        let turn_cancel = cancel
            .observed_scope()
            .map(|scope| {
                restate_await_event_key_for_authority(
                    &self.authority_id,
                    scope,
                    AwaitEventWaitIdentity::TurnCancelGate,
                )
                .map(|key| RestateDurableWaitAwaitRequest { key })
            })
            .transpose()
            .map_err(RuntimeEffectControllerError::from)?;
        let outcome = self
            .context
            .await_run_sources(
                &self.namespace,
                subscriptions,
                turn_cancel,
                Some(self.build_generation.clone()),
                self.options.process_cancel,
            )
            .await
            .map_err(|error| {
                crate::wire::lash_terminal(&error, RuntimeErrorCode::EngineEffectController)
            })?;
        match outcome {
            RestateTurnCancelRaceOutcome::Completed(result) => Ok(result),
            RestateTurnCancelRaceOutcome::TurnCancelled
            | RestateTurnCancelRaceOutcome::ProcessCancelled => {
                Err(RuntimeEffectControllerError::new(
                    RuntimeErrorCode::RuntimeEffectGroupAwaitCancelled,
                    "the Run source wait was cancelled",
                ))
            }
            RestateTurnCancelRaceOutcome::SessionRevoked { .. } => {
                Err(restate_unknown_or_revoked().into())
            }
        }
    }

    async fn record_run_record(
        &self,
        name: String,
        step: lash_core::RunRecordStep<'_>,
    ) -> Result<lash_core::tool_run::RunJournalEntry, RuntimeEffectControllerError> {
        self.journal_run_record(name, step).await
    }

    /// An invocation is pinned to the deployment that started it, so a turn
    /// on a draining build ends at its next quiet point (FIG-4739).
    fn hands_over_turns(&self) -> bool {
        true
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
        self.remember_trace_invocation(execution.invocation(), local_executor.step_issue());
        live_frontier::refuse_outside_a_run(&execution, &local_executor)?;
        match execution {
            RestateEffectExecution::DirectProcess {
                invocation,
                command,
            } => match Box::pin(execute_restate_process_command(
                &self.context,
                &self.namespace,
                &self.authority_id,
                &self.build_generation,
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
            ))
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
                            Box::pin(execute_restate_process_command(
                                &self.context,
                                &self.namespace,
                                &self.authority_id,
                                &self.build_generation,
                                self.options.process_cancel,
                                &invocation,
                                *command,
                                local_executor,
                                |_| {},
                            ))
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
                let transferable = local_executor.wait_transferable();
                let served_only = local_executor.served_only();
                let issue = local_executor.step_issue().clone();
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
                // The timer's start is observed inside its marker step, which
                // the journal runs once.
                let started = self.observer().map(|trace| {
                    let tracing = trace.tracing.clone();
                    let scope = Self::effect_scope(&issue);
                    let context = trace_context_for_runtime_effect_invocation(
                        lash_trace::TraceContext::default(),
                        &invocation,
                    );
                    Box::new(move || {
                        tracing.body(scope, &issue.begin_native()).observe(|| {
                            (
                                context,
                                lash_trace::TraceEvent::DurableTimerStarted { duration_ms },
                            )
                        });
                    }) as Box<dyn FnOnce() + Send>
                });
                live_frontier::pass_sleep_frontier(
                    &self.context,
                    &invocation,
                    served_only.as_ref(),
                    started,
                    |terminal, fault| self.wait_step_failure(terminal, |_| fault),
                )
                .await?;
                let turn_cancel = restate_timer_turn_cancel_wait_request(
                    &self.authority_id,
                    &invocation,
                    observe_turn_cancel,
                    turn_cancel_scope.as_ref(),
                )?;
                match self
                    .sleep_raced(&invocation, duration_ms, turn_cancel, transferable)
                    .await?
                {
                    Ok(RestateTurnCancelRaceOutcome::Completed(
                        context::TurnSleepOutcome::Resolved,
                    )) => {}
                    Ok(RestateTurnCancelRaceOutcome::Completed(
                        context::TurnSleepOutcome::HandedOver,
                    )) => {
                        return Err(RuntimeEffectControllerError::new(
                            RuntimeErrorCode::TurnWaitHandedOver,
                            "the draining generation handed over the turn's sleep",
                        ));
                    }
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
                        return Err(crate::wire::lash_terminal(
                            &err,
                            RuntimeErrorCode::EngineEffectController,
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
                // A group child cancelled before it parks meets the engine's
                // cancellation at one of the steps ahead of its wait.
                let engine_fault = |err: TerminalError| {
                    crate::wire::lash_terminal(&err, RuntimeErrorCode::EngineEffectController)
                };
                if self
                    .session_revoked(key.scope.session_id())
                    .await
                    .map_err(|err| self.wait_step_failure(err, engine_fault))?
                {
                    return Err(RuntimeEffectControllerError::from(
                        restate_unknown_or_revoked(),
                    ));
                }
                // A turn's cancellation reaches this wait only through the
                // durable gate race below (FIG-3672 P9); a process drive's
                // wait that observes no turn, such as a process body's
                // `waitSignal`, races the segment's durable cancel promise
                // (FIG-3673). No live token reaches it.
                let transferable = local_executor.wait_transferable();
                let RuntimeAwaitEventOptions {
                    cancellation: _,

                    clock: _,
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
                let request = restate_durable_wait_request(&key);
                let replay_key = invocation.effect_replay_key().to_string();
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
                let raced = match turn_cancel {
                    Some(turn_cancel) if transferable => {
                        self.context
                            .await_event_or_turn_end(
                                &self.namespace,
                                request,
                                replay_key,
                                turn_cancel,
                                self.build_generation.clone(),
                            )
                            .await
                    }
                    turn_cancel => self
                        .context
                        .await_event_or_turn_cancel(
                            &self.namespace,
                            request,
                            replay_key,
                            turn_cancel,
                            self.options.process_cancel,
                        )
                        .await
                        .map(|outcome| outcome.map(context::TurnWaitOutcome::Resolved)),
                };
                match raced {
                    Ok(RestateTurnCancelRaceOutcome::Completed(
                        context::TurnWaitOutcome::HandedOver,
                    )) => {
                        return Err(RuntimeEffectControllerError::new(
                            RuntimeErrorCode::TurnWaitHandedOver,
                            "the draining generation handed over the turn's event wait",
                        ));
                    }
                    Ok(RestateTurnCancelRaceOutcome::Completed(
                        context::TurnWaitOutcome::Resolved(resolution),
                    )) => Ok(RuntimeEffectOutcome::AwaitEvent { resolution }),
                    Ok(RestateTurnCancelRaceOutcome::SessionRevoked { session_id }) => {
                        Err(RuntimeEffectControllerError::from(
                            lash_core::StoreError::SessionDeleted { session_id },
                        ))
                    }
                    Ok(RestateTurnCancelRaceOutcome::ProcessCancelled) => {
                        Ok(RuntimeEffectOutcome::AwaitEvent {
                            resolution: Resolution::Cancelled,
                        })
                    }
                    Ok(RestateTurnCancelRaceOutcome::TurnCancelled) => {
                        Ok(RuntimeEffectOutcome::AwaitEvent {
                            resolution: Resolution::Cancelled,
                        })
                    }
                    // The engine's cancellation of a group child's invocation is
                    // the child's decided cancel, surfacing at this wait (FIG-3904).
                    Err(err) if self.is_group_child_engine_cancel(&err) => {
                        Err(group_child_cancelled())
                    }
                    Err(err) => Err(crate::wire::lash_terminal(
                        &err,
                        RuntimeErrorCode::EngineEffectController,
                    )),
                }
            }
            RestateEffectExecution::ArmToolCompletion { key, .. } => {
                self.arm_tool_completion(key, local_executor).await
            }
            RestateEffectExecution::AwaitToolCompletions {
                invocation,
                waits,
                dispatch,
                transferable,
            } => {
                self.await_tool_completions(
                    invocation,
                    waits,
                    dispatch,
                    transferable,
                    local_executor,
                )
                .await
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
                // The run's start and settlement were observed inside its
                // recorded body, by the attempt that ran it.
                outcome
            }
        }
    }
}

fn effect_group_engine_error(
    operation: &str,
    error: TerminalError,
) -> RuntimeEffectControllerError {
    if let Some(typed) = crate::wire::typed_terminal(error.message()) {
        return typed;
    }
    group_shape_error(format!(
        "Restate effect-group operation {operation} failed (verify the required services are registered): {error}"
    ))
}

/// Run a journaled effect body, retaining its outcome and any live attempt fault.
async fn execute_restate_journaled_effect(
    envelope: RuntimeEffectEnvelope,
    local_executor: RuntimeEffectLocalExecutor<'_>,
) -> lash_core::RecordedEffectExecution {
    let RuntimeEffectEnvelope {
        invocation,
        command,
        group,
    } = envelope;
    match command {
        RuntimeEffectCommand::Trigger { command } => {
            let outcome =
                match refuse_unhonored_group_membership(group.as_deref(), "restate trigger") {
                    Ok(()) => local_executor.execute_trigger(invocation, *command).await,
                    Err(refusal) => Err(refusal),
                };
            lash_core::RecordedEffectExecution {
                outcome,
                attempt_fault: None,
            }
        }
        command => {
            local_executor
                .execute_recorded(RuntimeEffectEnvelope {
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

mod process_scheduling;
pub use process_scheduling::ProcessWorkflowStartFailure;
use process_scheduling::schedule_restate_process;

mod execution;
mod tool_completion;
use execution::tracing_sleep_error;
pub(crate) use execution::{
    RestateEffectExecution, restate_effect_execution, restate_effect_name,
    validate_recorded_effect_envelope,
};

#[cfg(test)]
mod tests;
