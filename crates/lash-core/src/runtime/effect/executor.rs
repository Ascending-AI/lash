use crate::facade_support::RuntimeSessionStateFacadeOps;
use lash_sansio::sync::MutexExt;
use std::collections::HashMap;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Instant;

use tokio::sync::mpsc;
use tokio::sync::oneshot;
use tokio_util::sync::CancellationToken;

pub(crate) mod control;
mod controller_error;
mod inline_controller;
mod process_local;

mod language_runtime;
mod scoped;
mod task_panic;
mod trigger;

pub use control::{
    AwaitEventKey, AwaitEventResolver, AwaitEventWaitIdentity, BoundaryReason, EffectHost,
    EffectJournalIdentity, EffectJournalRetirement, ExecutionScope, ExternalCompletionError,
    Resolution, ResolveOutcome, RuntimeEffectController, ScopedEffectController, SegmentProgress,
};
pub(crate) use control::{
    EffectControllerTaskRequest, EffectTaskController, RuntimeEffectControllerHandle,
    drive_effect_controller_task,
};
pub use controller_error::RuntimeEffectControllerError;
pub use inline_controller::InlineRuntimeEffectController;
pub use trigger::TriggerLocalExecution;

use crate::LlmRequest as CoreLlmRequest;
use crate::ProcessRegistry;
use crate::RuntimeError;
use crate::provider::ProviderHandle;
use crate::runtime::{RuntimeStreamEvent, RuntimeTurnDriver};
use crate::sansio::LlmCallError;
use control::{RemoteLocalExecutionRequest, ScopedEffectControllerInner};

use super::envelope::{
    ProcessCommand, ProcessEffectOutcome, RuntimeDirectLlmOutcome, RuntimeEffectCommand,
    RuntimeEffectEnvelope, RuntimeEffectKind, RuntimeEffectOutcome,
};
use super::outcome::llm_call_error_from_transport;

/// Host controls attached to one external event wait.
///
/// Durable effect controllers consume these controls and translate them to
/// their engine-native cancellation and timer primitives.
pub struct RuntimeAwaitEventOptions {
    pub cancellation: CancellationToken,
    pub deadline: Option<Instant>,
    pub clock: Arc<dyn crate::Clock>,
    pub observe_turn_cancel: bool,
    pub turn_cancel_scope: Option<crate::ExecutionScope>,
}

/// Host controls attached to one sleep effect.
pub struct RuntimeSleepOptions {
    pub cancellation: CancellationToken,
    pub observe_turn_cancel: bool,
    pub turn_cancel_scope: Option<crate::ExecutionScope>,
}

// =============================================================================
// Local executor (per-effect borrowed runner state)
// =============================================================================

struct WaitControls {
    cancellation: CancellationToken,
    observe_turn_cancel: bool,
    turn_cancel_scope: Option<crate::ExecutionScope>,
}

#[async_trait::async_trait]
pub(crate) trait ProcessRunner: Send + Sync {
    async fn run_process(
        &self,
        registration: crate::ProcessRegistration,
        execution_context: crate::ProcessExecutionContext,
        registry: Arc<dyn ProcessRegistry>,
        scoped_effect_controller: crate::ScopedEffectController<'_>,
        cancellation: CancellationToken,
        handover: Option<crate::SegmentHandover>,
    ) -> Result<crate::ProcessRunOutcome, crate::ProcessInfraError>;
}

/// Valid turn-cancellation controls for a process operation.
///
/// Presence means the process operation observes the exact turn gate; absence
/// means it does not. Keeping token and scope together makes an enabled
/// observation without a durable scope unrepresentable.
#[derive(Clone)]
pub struct ProcessTurnCancellation {
    pub cancellation: CancellationToken,
    pub scope: crate::ExecutionScope,
}

impl ProcessTurnCancellation {
    pub fn new(cancellation: CancellationToken, scope: crate::ExecutionScope) -> Self {
        Self {
            cancellation,
            scope,
        }
    }
}

/// The complete turn-cancel trio for one wait: the cancellation token the wait
/// races against, whether the wait observes turn cancellation, and the
/// execution scope its turn-cancel gate registers under.
///
/// The observation flag is the presence of the scope, so a wait that observes
/// turn cancellation without naming a durable scope — and, more importantly, a
/// wait that stamps the scope while silently keeping the executor's default
/// observation — is unrepresentable. Every in-workspace wait builder takes this
/// value whole (`RuntimeEffectLocalExecutor::sleep_under`,
/// `RuntimeEffectLocalExecutor::await_event_under`,
/// `ProcessOpScope::with_turn_cancellation`), and it is produced by a single
/// accessor on the execution that owns the observation decision
/// (`RuntimeExecutionContext::turn_cancel_wait`, or
/// `ScopedEffectController::turn_cancel_wait` where no execution context
/// exists).
#[derive(Clone)]
pub(crate) struct TurnCancelWait {
    cancellation: CancellationToken,
    /// `Some` when the wait attaches the turn-cancel gate for that scope;
    /// `None` when the enclosing execution runs without turn observation, as
    /// process bodies do.
    observed_scope: Option<crate::ExecutionScope>,
}

impl TurnCancelWait {
    /// The wait races `cancellation` and observes the turn-cancel gate of
    /// `scope`.
    pub(crate) fn observing(cancellation: CancellationToken, scope: crate::ExecutionScope) -> Self {
        Self {
            cancellation,
            observed_scope: Some(scope),
        }
    }

    /// The wait races `cancellation` and never attaches a turn-cancel gate.
    pub(crate) fn unobserved(cancellation: CancellationToken) -> Self {
        Self {
            cancellation,
            observed_scope: None,
        }
    }

    /// The turn cancellation a process operation observes, if any.
    pub(crate) fn process_turn_cancellation(&self) -> Option<ProcessTurnCancellation> {
        self.observed_scope
            .as_ref()
            .map(|scope| ProcessTurnCancellation::new(self.cancellation.clone(), scope.clone()))
    }

    fn controls(&self) -> WaitControls {
        WaitControls {
            cancellation: self.cancellation.clone(),
            observe_turn_cancel: self.observed_scope.is_some(),
            turn_cancel_scope: self.observed_scope.clone(),
        }
    }
}

/// Observer invoked after a process side effect and before durable outcome
/// recording.
///
/// This is public for **effect-host implementors** and
/// **conformance-suite embedders** that must model a host crash in that exact
/// interval.
pub type ProcessOutcomeObserver = Arc<dyn Fn(&ProcessEffectOutcome) + Send + Sync + 'static>;

pub struct ProcessLocalExecution {
    pub registry: Arc<dyn ProcessRegistry>,
    pub process_work_driver: Option<crate::ProcessWorkDriver>,
    pub process_env_store: Option<Arc<dyn crate::ProcessExecutionEnvStore>>,
    pub turn_cancellation: Option<ProcessTurnCancellation>,
    pub effect_controller: Option<Arc<dyn RuntimeEffectController>>,
    pub(crate) outcome_observer: Option<ProcessOutcomeObserver>,
}

#[allow(private_interfaces)]
pub(crate) struct TurnEffectStateUpdate {
    pub(crate) policy: crate::RuntimeSessionPolicy,
    pub(crate) llm_stream_summaries: HashMap<usize, crate::runtime::LlmStreamSummary>,
    pub(crate) next_llm_ordinal: usize,
    pub(crate) pending_queue_claims: Vec<crate::QueuedWorkClaim>,
    pub(crate) pending_turn_input_claims: Vec<crate::runtime::turn_input_ingress::TurnInputDrive>,
    pub(crate) pending_checkpoint_turn_input_claim: Option<crate::TurnInputClaim>,
}

pub(super) struct LocalTurnEffectRunner {
    driver: RuntimeTurnDriver<'static>,
    protocol_iteration: usize,
    messages: crate::MessageSequence,
    event_tx: mpsc::Sender<RuntimeStreamEvent>,
    cancellation: CancellationToken,
    update: Arc<std::sync::Mutex<Option<TurnEffectStateUpdate>>>,
}

pub(super) struct LocalDirectEffectRunner {
    provider: ProviderHandle,
    attachment_store: Arc<crate::SessionAttachmentStore>,
}

struct LocalToolBatchEffectRunner<'run> {
    context: crate::RuntimeExecutionContext<'run>,
    child_trace_hooks: HashMap<String, crate::ToolChildExecutionTraceHook>,
    completion_key: Option<crate::AwaitEventKey>,
}

struct LocalPreparedToolAttemptEffectRunner<'run> {
    dispatch: Arc<crate::tool_dispatch::ToolDispatchContext<'run>>,
    tool_context: crate::ToolContext<'run>,
    completion_key: Option<crate::AwaitEventKey>,
}

struct RemoteEffectRunner {
    requests: mpsc::UnboundedSender<RemoteLocalExecutionRequest>,
}

#[async_trait::async_trait]
trait RuntimeEffectLocalRunner: Send {
    fn uses_task_boundary(&self, _command: &RuntimeEffectCommand) -> bool {
        false
    }

    async fn execute(
        self: Box<Self>,
        envelope: RuntimeEffectEnvelope,
    ) -> Result<RuntimeEffectOutcome, RuntimeEffectControllerError>;
}

type TestingRuntimeEffectLocalRunnerFn<'run> = dyn FnOnce(
        RuntimeEffectEnvelope,
    ) -> Pin<
        Box<
            dyn Future<Output = Result<RuntimeEffectOutcome, RuntimeEffectControllerError>>
                + Send
                + 'run,
        >,
    > + Send
    + 'run;

struct TestingRuntimeEffectLocalRunner<'run> {
    run: Box<TestingRuntimeEffectLocalRunnerFn<'run>>,
}

enum LocalTarget {
    Unavailable,
    SleepOnly {
        controls: WaitControls,
        clock: Arc<dyn crate::Clock>,
    },
    ExternalWaitOptions {
        controls: WaitControls,
        deadline: Option<Instant>,
        clock: Arc<dyn crate::Clock>,
    },
    Process(ProcessLocalExecution),
    Trigger(TriggerLocalExecution),
    TurnAcceptance(Arc<dyn crate::TurnInputStore>),
    OwnedRunner(Box<dyn RuntimeEffectLocalRunner + Send + 'static>),
}

enum RuntimeEffectLocalExecutorState<'run> {
    Target(LocalTarget),
    Runner(Box<dyn RuntimeEffectLocalRunner + Send + 'run>),
}

/// Scoped local executor provided to a [`RuntimeEffectController`] for one effect.
///
/// Durable controllers may ignore it and replay their own recorded result. The
/// default inline controller delegates to it, so local provider/tool/checkpoint
/// work still crosses the same `execute_effect` boundary as durable controllers.
pub struct RuntimeEffectLocalExecutor<'run> {
    state: RuntimeEffectLocalExecutorState<'run>,
    replay_trace: Option<super::RuntimeEffectReplayTrace>,
}

struct AbortEffectTaskOnDrop {
    handle: tokio::task::AbortHandle,
    armed: bool,
}

impl AbortEffectTaskOnDrop {
    fn new(handle: tokio::task::AbortHandle) -> Self {
        Self {
            handle,
            armed: true,
        }
    }

    fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for AbortEffectTaskOnDrop {
    fn drop(&mut self) {
        if self.armed {
            self.handle.abort();
        }
    }
}

impl<'run> RuntimeEffectLocalExecutor<'run> {
    /// Constructs a local path that rejects unavailable inline execution.
    pub fn unavailable() -> Self {
        Self {
            state: RuntimeEffectLocalExecutorState::Target(LocalTarget::Unavailable),
            replay_trace: None,
        }
    }

    /// Builds the cancellable inline sleep path using the system clock.
    pub fn sleep(cancellation: CancellationToken) -> Self {
        Self::sleep_with_clock(cancellation, Arc::new(crate::SystemClock))
    }

    /// Builds the inline sleep path with an injected clock for effect-host and conformance
    /// implementors testing deterministic deadline behavior.
    pub fn sleep_with_clock(cancellation: CancellationToken, clock: Arc<dyn crate::Clock>) -> Self {
        Self {
            state: RuntimeEffectLocalExecutorState::Target(LocalTarget::SleepOnly {
                controls: WaitControls {
                    cancellation,
                    observe_turn_cancel: true,
                    turn_cancel_scope: None,
                },
                clock,
            }),
            replay_trace: None,
        }
    }

    /// Builds the inline durable-wait path for effect-host implementors using the system clock and
    /// the supplied optional deadline.
    pub fn await_event(cancellation: CancellationToken, deadline: Option<Instant>) -> Self {
        Self::await_event_with_clock(cancellation, deadline, Arc::new(crate::SystemClock))
    }

    /// Builds the inline durable-wait path with an injected clock for effect-host and conformance
    /// implementors testing deterministic deadline behavior.
    pub fn await_event_with_clock(
        cancellation: CancellationToken,
        deadline: Option<Instant>,
        clock: Arc<dyn crate::Clock>,
    ) -> Self {
        Self {
            state: RuntimeEffectLocalExecutorState::Target(LocalTarget::ExternalWaitOptions {
                controls: WaitControls {
                    cancellation,
                    observe_turn_cancel: true,
                    turn_cancel_scope: None,
                },
                deadline,
                clock,
            }),
            replay_trace: None,
        }
    }

    /// Builds the inline sleep path from the complete turn-cancel trio, so an
    /// in-workspace sleep cannot be journaled with a half-stamped one.
    pub(crate) fn sleep_under(wait: &TurnCancelWait, clock: Arc<dyn crate::Clock>) -> Self {
        Self {
            state: RuntimeEffectLocalExecutorState::Target(LocalTarget::SleepOnly {
                controls: wait.controls(),
                clock,
            }),
            replay_trace: None,
        }
    }

    /// Builds the inline durable-wait path from the complete turn-cancel trio,
    /// so an in-workspace wait cannot be journaled with a half-stamped one.
    pub(crate) fn await_event_under(
        wait: &TurnCancelWait,
        deadline: Option<Instant>,
        clock: Arc<dyn crate::Clock>,
    ) -> Self {
        Self {
            state: RuntimeEffectLocalExecutorState::Target(LocalTarget::ExternalWaitOptions {
                controls: wait.controls(),
                deadline,
                clock,
            }),
            replay_trace: None,
        }
    }

    #[doc(hidden)]
    pub fn with_turn_cancel_observation(mut self, observe_turn_cancel: bool) -> Self {
        if let RuntimeEffectLocalExecutorState::Target(
            LocalTarget::SleepOnly { controls, .. }
            | LocalTarget::ExternalWaitOptions { controls, .. },
        ) = &mut self.state
        {
            controls.observe_turn_cancel = observe_turn_cancel;
        }
        self
    }

    /// Adds the turn execution scope that effect-host implementors use to distinguish turn
    /// cancellation from an ordinary wait cancellation.
    pub fn with_turn_cancel_scope(mut self, scope: crate::ExecutionScope) -> Self {
        if let RuntimeEffectLocalExecutorState::Target(
            LocalTarget::SleepOnly { controls, .. }
            | LocalTarget::ExternalWaitOptions { controls, .. },
        ) = &mut self.state
        {
            controls.turn_cancel_scope = Some(scope);
        }
        self
    }

    #[doc(hidden)]
    pub fn with_process_turn_cancellation(
        mut self,
        turn_cancellation: ProcessTurnCancellation,
    ) -> Self {
        if let RuntimeEffectLocalExecutorState::Target(LocalTarget::Process(
            ProcessLocalExecution {
                turn_cancellation: current,
                ..
            },
        )) = &mut self.state
        {
            *current = Some(turn_cancellation);
        }
        self
    }

    pub(crate) fn with_process_effect_controller(
        mut self,
        controller: Arc<dyn RuntimeEffectController>,
    ) -> Self {
        if let RuntimeEffectLocalExecutorState::Target(LocalTarget::Process(
            ProcessLocalExecution {
                effect_controller: current,
                ..
            },
        )) = &mut self.state
        {
            *current = Some(controller);
        }
        self
    }

    /// Installs an observer after a process side effect has completed but
    /// before a durable controller receives the outcome to record.
    ///
    /// This is an **effect-host implementor** and **conformance-suite
    /// embedder** seam. Panicking from the observer models a host crash in
    /// that exact interval.
    pub fn with_process_outcome_observer(mut self, observer: ProcessOutcomeObserver) -> Self {
        if let RuntimeEffectLocalExecutorState::Target(LocalTarget::Process(execution)) =
            &mut self.state
        {
            execution.outcome_observer = Some(observer);
        }
        self
    }

    /// Binds the durable environment store used after an admitted process
    /// start reaches local realization.
    ///
    /// This is an **integrator class 3: effect-host implementor** seam for
    /// executors that realize serialized process-start commands.
    pub fn with_process_env_store(
        mut self,
        store: Arc<dyn crate::ProcessExecutionEnvStore>,
    ) -> Self {
        if let RuntimeEffectLocalExecutorState::Target(LocalTarget::Process(execution)) =
            &mut self.state
        {
            execution.process_env_store = Some(store);
        }
        self
    }

    /// Removes and returns the process outcome observer.
    ///
    /// This is public for **effect-host implementors** that transfer local
    /// execution into a durable controller while preserving the conformance
    /// fault seam.
    pub fn take_process_outcome_observer(&mut self) -> Option<ProcessOutcomeObserver> {
        match &mut self.state {
            RuntimeEffectLocalExecutorState::Target(LocalTarget::Process(execution)) => {
                execution.outcome_observer.take()
            }
            _ => None,
        }
    }

    /// Binds process registry and optional work-driver services for effect-host implementors
    /// executing process effects inline.
    pub fn processes(
        registry: Arc<dyn ProcessRegistry>,
        process_work_driver: Option<crate::ProcessWorkDriver>,
    ) -> Self {
        Self {
            state: RuntimeEffectLocalExecutorState::Target(LocalTarget::Process(
                ProcessLocalExecution {
                    registry,
                    process_work_driver,
                    process_env_store: None,
                    turn_cancellation: None,
                    effect_controller: None,
                    outcome_observer: None,
                },
            )),
            replay_trace: None,
        }
    }

    /// Binds a turn-input store for effect-host implementors executing the
    /// durable turn-acceptance effect (ADR 0069 §6) inline.
    ///
    /// The acceptance write is the one store call a replaying engine must not
    /// repeat, so it crosses the runtime-effect envelope like every other
    /// journaled effect rather than being issued directly against the store.
    pub fn turn_acceptance(store: Arc<dyn crate::TurnInputStore>) -> Self {
        Self {
            state: RuntimeEffectLocalExecutorState::Target(LocalTarget::TurnAcceptance(store)),
            replay_trace: None,
        }
    }

    /// Binds a trigger store for effect-host implementors executing trigger effects inline without
    /// bypassing the runtime-effect envelope.
    pub fn triggers(store: Arc<dyn crate::TriggerStore>) -> Self {
        Self {
            state: RuntimeEffectLocalExecutorState::Target(LocalTarget::Trigger(
                TriggerLocalExecution { store },
            )),
            replay_trace: None,
        }
    }

    /// Build a local executor for Lash's own conformance helpers.
    ///
    /// This is deliberately hidden from the published default documentation;
    /// it is not an integrator seam for fabricating runtime effect outcomes.
    #[doc(hidden)]
    pub fn testing<F, Fut>(run: F) -> Self
    where
        F: FnOnce(RuntimeEffectEnvelope) -> Fut + Send + 'run,
        Fut: Future<Output = Result<RuntimeEffectOutcome, RuntimeEffectControllerError>>
            + Send
            + 'run,
    {
        Self {
            state: RuntimeEffectLocalExecutorState::Runner(Box::new(
                TestingRuntimeEffectLocalRunner {
                    run: Box::new(move |envelope| Box::pin(run(envelope))),
                },
            )),
            replay_trace: None,
        }
    }

    pub(in crate::runtime) fn turn(
        driver: &mut RuntimeTurnDriver<'_>,
        machine: &crate::TurnMachine,
        event_tx: mpsc::Sender<RuntimeStreamEvent>,
        cancellation: CancellationToken,
        scoped_effect_controller: ScopedEffectController<'static>,
    ) -> (
        RuntimeEffectLocalExecutor<'static>,
        Arc<std::sync::Mutex<Option<TurnEffectStateUpdate>>>,
    ) {
        let replay_trace = super::RuntimeEffectReplayTrace::for_divergence(
            driver.host.core.tracing.trace_sink.as_ref(),
            driver.host.core.tracing.trace_context.clone(),
            driver.trace_context(machine.protocol_iteration()),
            Arc::clone(&driver.host.core.clock),
        );
        let update = Arc::new(std::sync::Mutex::new(None));
        let owned_driver = RuntimeTurnDriver {
            session: driver.session.clone_for_effect(),
            policy: driver.policy.clone(),
            host: driver.host.clone(),
            scoped_effect_controller,
            session_id: driver.session_id.clone(),
            turn_id: driver.turn_id.clone(),
            turn_index: driver.turn_index,
            turn_pipeline: crate::runtime::TurnBoundary::from_state_with_clock(
                driver.turn_pipeline.state().clone(),
                Arc::clone(&driver.host.core.clock),
                driver.turn_pipeline.state().turn_scope(&driver.turn_id),
                driver.host.core.durability.commit_budget,
            ),
            llm_stream_summaries: driver.llm_stream_summaries.clone(),
            llm_calls: Vec::new(),
            next_llm_ordinal: driver.next_llm_ordinal,
            session_services: Arc::clone(&driver.session_services),
            protocol_turn_options: driver.protocol_turn_options.clone(),
            protocol_extension: driver.protocol_extension.clone(),
            turn_context: driver.turn_context.clone(),
            turn_causes: driver.turn_causes.clone(),
            pending_queue_claims: driver.pending_queue_claims.clone(),
            pending_turn_input_claims: driver.pending_turn_input_claims.clone(),
            pending_checkpoint_turn_input_claim: driver.pending_checkpoint_turn_input_claim.clone(),
            checkpoint_messages: driver.checkpoint_messages.clone(),
            recorded_intent_outcomes: driver.recorded_intent_outcomes.clone(),
            session_execution_lease: driver.session_execution_lease.clone(),
            runtime_lease_owner: driver.runtime_lease_owner.clone(),
            turn_phase_probe: driver.turn_phase_probe.clone(),
            turn_control: Arc::clone(&driver.turn_control),
            observes_durable_cancel_after_llm: driver.observes_durable_cancel_after_llm,
        };
        (
            RuntimeEffectLocalExecutor {
                state: RuntimeEffectLocalExecutorState::Target(LocalTarget::OwnedRunner(Box::new(
                    LocalTurnEffectRunner {
                        driver: owned_driver,
                        protocol_iteration: machine.protocol_iteration(),
                        messages: machine.message_sequence(),
                        event_tx,
                        cancellation,
                        update: Arc::clone(&update),
                    },
                ))),
                replay_trace,
            },
            update,
        )
    }

    pub(in crate::runtime) fn direct(
        provider: ProviderHandle,
        attachment_store: Arc<crate::SessionAttachmentStore>,
        replay_trace: Option<super::RuntimeEffectReplayTrace>,
    ) -> Self {
        Self {
            state: RuntimeEffectLocalExecutorState::Target(LocalTarget::OwnedRunner(Box::new(
                LocalDirectEffectRunner {
                    provider,
                    attachment_store,
                },
            ))),
            replay_trace,
        }
    }

    pub(crate) fn tool_batch(
        context: crate::RuntimeExecutionContext<'run>,
        child_trace_hooks: HashMap<String, crate::ToolChildExecutionTraceHook>,
        completion_key: Option<crate::AwaitEventKey>,
    ) -> Self {
        let replay_trace = context.replay_validation_trace();
        if let Some(context) = context.to_static() {
            return Self {
                state: RuntimeEffectLocalExecutorState::Target(LocalTarget::OwnedRunner(Box::new(
                    LocalToolBatchEffectRunner {
                        context,
                        child_trace_hooks,
                        completion_key,
                    },
                ))),
                replay_trace,
            };
        }
        Self {
            state: RuntimeEffectLocalExecutorState::Runner(Box::new(LocalToolBatchEffectRunner {
                context,
                child_trace_hooks,
                completion_key,
            })),
            replay_trace,
        }
    }

    pub(crate) fn prepared_tool_attempt(
        dispatch: Arc<crate::tool_dispatch::ToolDispatchContext<'run>>,
        tool_context: crate::ToolContext<'run>,
        completion_key: Option<crate::AwaitEventKey>,
    ) -> Self {
        let replay_trace = tool_context.replay_validation_trace();
        if let (Some(dispatch), Some(tool_context)) =
            (dispatch.to_static(), tool_context.to_static())
        {
            return Self {
                state: RuntimeEffectLocalExecutorState::Target(LocalTarget::OwnedRunner(Box::new(
                    LocalPreparedToolAttemptEffectRunner {
                        dispatch: Arc::new(dispatch),
                        tool_context,
                        completion_key,
                    },
                ))),
                replay_trace,
            };
        }
        Self {
            state: RuntimeEffectLocalExecutorState::Runner(Box::new(
                LocalPreparedToolAttemptEffectRunner {
                    dispatch,
                    tool_context,
                    completion_key,
                },
            )),
            replay_trace,
        }
    }

    /// Exposes structured replay-comparison evidence to effect-host and conformance implementors,
    /// returning `None` when the runtime has no configured trace sink.
    pub fn replay_validation_trace(&self) -> Option<&super::RuntimeEffectReplayTrace> {
        self.replay_trace.as_ref()
    }

    /// Executes execute work for effect-host implementors while executing or replaying a runtime
    /// effect.
    pub async fn execute(
        self,
        envelope: RuntimeEffectEnvelope,
    ) -> Result<RuntimeEffectOutcome, RuntimeEffectControllerError> {
        match self.state {
            RuntimeEffectLocalExecutorState::Runner(runner) => runner.execute(envelope).await,
            RuntimeEffectLocalExecutorState::Target(LocalTarget::OwnedRunner(runner)) => {
                if !runner.uses_task_boundary(&envelope.command) {
                    return runner.execute(envelope).await;
                }
                let panic_call = match &envelope.command {
                    RuntimeEffectCommand::ToolAttempt { call, .. } => Some(call.clone()),
                    _ => None,
                };
                let task = crate::task::spawn(
                    crate::runtime::process_worker::inherit_process_execution_permit(
                        runner.execute(envelope),
                    ),
                );
                let mut abort = AbortEffectTaskOnDrop::new(task.abort_handle());
                let result = match task.await {
                    Ok(result) => result,
                    Err(err) => task_panic::map_effect_task_join(err, panic_call),
                };
                abort.disarm();
                result
            }
            RuntimeEffectLocalExecutorState::Target(LocalTarget::SleepOnly {
                controls: WaitControls { cancellation, .. },
                clock,
            }) => execute_local_sleep(envelope, cancellation, clock.as_ref()).await,
            RuntimeEffectLocalExecutorState::Target(LocalTarget::ExternalWaitOptions {
                ..
            }) => Err(RuntimeEffectControllerError::new(
                crate::RuntimeErrorCode::RuntimeEffectLocalExecutorMismatch,
                format!(
                    "local await-event options cannot execute {} command directly",
                    envelope.command.kind().as_str()
                ),
            )),
            RuntimeEffectLocalExecutorState::Target(LocalTarget::Unavailable) => {
                Err(RuntimeEffectControllerError::new(
                    crate::RuntimeErrorCode::RuntimeEffectLocalExecutorUnavailable,
                    format!(
                        "no local executor is available for {}",
                        envelope.command.kind().as_str()
                    ),
                ))
            }
            RuntimeEffectLocalExecutorState::Target(LocalTarget::Process(_)) => {
                Err(RuntimeEffectControllerError::new(
                    crate::RuntimeErrorCode::RuntimeEffectLocalExecutorMismatch,
                    format!(
                        "process executor cannot execute {} command directly",
                        envelope.command.kind().as_str()
                    ),
                ))
            }
            RuntimeEffectLocalExecutorState::Target(LocalTarget::Trigger(_)) => {
                Err(RuntimeEffectControllerError::new(
                    crate::RuntimeErrorCode::RuntimeEffectLocalExecutorMismatch,
                    format!(
                        "trigger executor cannot execute {} command directly",
                        envelope.command.kind().as_str()
                    ),
                ))
            }
            RuntimeEffectLocalExecutorState::Target(LocalTarget::TurnAcceptance(store)) => {
                let RuntimeEffectCommand::AcceptTurnInput { draft } = envelope.command else {
                    return Err(RuntimeEffectControllerError::new(
                        crate::RuntimeErrorCode::RuntimeEffectLocalExecutorMismatch,
                        format!(
                            "turn-acceptance executor cannot execute {} command directly",
                            envelope.command.kind().as_str()
                        ),
                    ));
                };
                let accepted = store
                    .enqueue_pending_turn_input(*draft)
                    .await
                    .map_err(|err| {
                        RuntimeEffectControllerError::new(
                            crate::RuntimeErrorCode::StoreCommitFailed,
                            format!("turn acceptance commit failed: {err}"),
                        )
                    })?;
                Ok(RuntimeEffectOutcome::AcceptTurnInput {
                    accepted: Box::new(accepted),
                })
            }
        }
    }

    /// Extracts the process outcome for effect-host implementors while executing or replaying a
    /// runtime effect.
    pub fn into_process(self) -> Result<ProcessLocalExecution, RuntimeEffectControllerError> {
        match self.state {
            RuntimeEffectLocalExecutorState::Target(LocalTarget::Process(execution)) => {
                Ok(execution)
            }
            _ => Err(RuntimeEffectControllerError::new(
                crate::RuntimeErrorCode::RuntimeEffectLocalExecutorUnavailable,
                "no process executor is available for process command",
            )),
        }
    }

    fn into_remote_execution(
        self,
    ) -> (
        RuntimeEffectLocalExecutor<'static>,
        Option<(
            RuntimeEffectLocalExecutor<'run>,
            mpsc::UnboundedReceiver<RemoteLocalExecutionRequest>,
        )>,
    ) {
        let RuntimeEffectLocalExecutor {
            state,
            replay_trace,
        } = self;
        match state {
            RuntimeEffectLocalExecutorState::Runner(runner) => {
                let (requests, request_rx) = mpsc::unbounded_channel();
                (
                    RuntimeEffectLocalExecutor {
                        state: RuntimeEffectLocalExecutorState::Target(LocalTarget::OwnedRunner(
                            Box::new(RemoteEffectRunner { requests }),
                        )),
                        replay_trace: replay_trace.clone(),
                    },
                    Some((
                        RuntimeEffectLocalExecutor {
                            state: RuntimeEffectLocalExecutorState::Runner(runner),
                            replay_trace,
                        },
                        request_rx,
                    )),
                )
            }
            RuntimeEffectLocalExecutorState::Target(LocalTarget::OwnedRunner(runner)) => {
                let (requests, request_rx) = mpsc::unbounded_channel();
                (
                    RuntimeEffectLocalExecutor {
                        state: RuntimeEffectLocalExecutorState::Target(LocalTarget::OwnedRunner(
                            Box::new(RemoteEffectRunner { requests }),
                        )),
                        replay_trace: replay_trace.clone(),
                    },
                    Some((
                        RuntimeEffectLocalExecutor {
                            state: RuntimeEffectLocalExecutorState::Target(
                                LocalTarget::OwnedRunner(runner),
                            ),
                            replay_trace,
                        },
                        request_rx,
                    )),
                )
            }
            RuntimeEffectLocalExecutorState::Target(target) => (
                RuntimeEffectLocalExecutor {
                    state: RuntimeEffectLocalExecutorState::Target(target),
                    replay_trace,
                },
                None,
            ),
        }
    }

    async fn execute_forwarded(
        self,
        envelope: RuntimeEffectEnvelope,
    ) -> Result<RuntimeEffectOutcome, RuntimeEffectControllerError> {
        let RuntimeEffectEnvelope {
            invocation,
            command,
            group,
        } = envelope;
        match command {
            RuntimeEffectCommand::Trigger { command } => {
                crate::runtime::effect::refuse_unhonored_group_membership(
                    group.as_deref(),
                    "trigger",
                )?;
                self.execute_trigger(invocation, *command).await
            }
            command => {
                self.execute(RuntimeEffectEnvelope {
                    invocation,
                    command,
                    group,
                })
                .await
            }
        }
    }

    /// Extracts the trigger outcome for effect-host implementors while executing or replaying a
    /// runtime effect.
    pub fn into_trigger(self) -> Result<TriggerLocalExecution, RuntimeEffectControllerError> {
        match self.state {
            RuntimeEffectLocalExecutorState::Target(LocalTarget::Trigger(execution)) => {
                Ok(execution)
            }
            _ => Err(RuntimeEffectControllerError::new(
                crate::RuntimeErrorCode::RuntimeEffectLocalExecutorUnavailable,
                "no trigger executor is available for trigger command",
            )),
        }
    }

    /// Executes trigger work for effect-host implementors while executing or replaying a runtime
    /// effect.
    pub async fn execute_trigger(
        self,
        invocation: crate::RuntimeInvocation,
        command: crate::TriggerCommand,
    ) -> Result<RuntimeEffectOutcome, RuntimeEffectControllerError> {
        let operation_id = invocation
            .effect_id()
            .ok_or_else(|| {
                RuntimeEffectControllerError::new(
                    crate::RuntimeErrorCode::RuntimeEffectInvocationSubject,
                    "trigger effect requires an effect id",
                )
            })?
            .to_string();
        match self.state {
            RuntimeEffectLocalExecutorState::Target(LocalTarget::Trigger(execution)) => {
                let result = execution.execute(&operation_id, command).await?;
                Ok(RuntimeEffectOutcome::Trigger {
                    result: Box::new(result),
                })
            }
            RuntimeEffectLocalExecutorState::Runner(runner)
            | RuntimeEffectLocalExecutorState::Target(LocalTarget::OwnedRunner(runner)) => {
                runner
                    .execute(RuntimeEffectEnvelope::new(
                        invocation,
                        RuntimeEffectCommand::Trigger {
                            command: Box::new(command),
                        },
                    ))
                    .await
            }
            _ => Err(RuntimeEffectControllerError::new(
                crate::RuntimeErrorCode::RuntimeEffectLocalExecutorUnavailable,
                "no trigger executor is available for trigger command",
            )),
        }
    }

    /// Extracts the await event options outcome for effect-host implementors while executing or
    /// replaying a runtime effect.
    pub fn into_await_event_options(
        self,
    ) -> Result<RuntimeAwaitEventOptions, RuntimeEffectControllerError> {
        match self.state {
            RuntimeEffectLocalExecutorState::Target(LocalTarget::ExternalWaitOptions {
                controls:
                    WaitControls {
                        cancellation,
                        observe_turn_cancel,
                        turn_cancel_scope,
                    },
                deadline,
                clock,
            }) => Ok(RuntimeAwaitEventOptions {
                cancellation,
                deadline,
                clock,
                observe_turn_cancel,
                turn_cancel_scope,
            }),
            _ => Ok(RuntimeAwaitEventOptions {
                cancellation: CancellationToken::new(),
                deadline: None,
                clock: Arc::new(crate::SystemClock),
                observe_turn_cancel: false,
                turn_cancel_scope: None,
            }),
        }
    }

    /// Consumes a local executor for effect-host implementors, returning sleep options only when
    /// the effect was configured for sleep.
    pub fn into_sleep_options(self) -> RuntimeSleepOptions {
        match self.state {
            RuntimeEffectLocalExecutorState::Target(LocalTarget::SleepOnly {
                controls:
                    WaitControls {
                        cancellation,
                        observe_turn_cancel,
                        turn_cancel_scope,
                    },
                ..
            }) => RuntimeSleepOptions {
                cancellation,
                observe_turn_cancel,
                turn_cancel_scope,
            },
            _ => RuntimeSleepOptions {
                cancellation: CancellationToken::new(),
                observe_turn_cancel: false,
                turn_cancel_scope: None,
            },
        }
    }
}

#[async_trait::async_trait]
impl RuntimeEffectLocalRunner for TestingRuntimeEffectLocalRunner<'_> {
    async fn execute(
        self: Box<Self>,
        envelope: RuntimeEffectEnvelope,
    ) -> Result<RuntimeEffectOutcome, RuntimeEffectControllerError> {
        (self.run)(envelope).await
    }
}

#[async_trait::async_trait]
impl RuntimeEffectLocalRunner for LocalToolBatchEffectRunner<'_> {
    fn uses_task_boundary(&self, command: &RuntimeEffectCommand) -> bool {
        matches!(
            command,
            RuntimeEffectCommand::ToolBatch { .. } | RuntimeEffectCommand::ToolAttempt { .. }
        )
    }

    async fn execute(
        self: Box<Self>,
        envelope: RuntimeEffectEnvelope,
    ) -> Result<RuntimeEffectOutcome, RuntimeEffectControllerError> {
        match envelope.command {
            RuntimeEffectCommand::ToolBatch { batch } => {
                let outcome = Box::pin(self.context.execute_prepared_tool_batch_launches(
                    batch,
                    envelope.invocation,
                    self.child_trace_hooks,
                ))
                .await?;
                Ok(RuntimeEffectOutcome::ToolBatch {
                    launches: outcome.launches,
                    triggers: outcome.triggers,
                    settlement_order: outcome.settlement_order,
                })
            }
            RuntimeEffectCommand::ToolAttempt {
                call,
                execution_grant,
                attempt,
                max_attempts,
            } => {
                let child_execution_trace_hook = self.child_trace_hooks.get(&call.call_id).cloned();
                let outcome = Box::pin(self.context.execute_prepared_tool_attempt_effect(
                    call,
                    execution_grant,
                    attempt,
                    max_attempts,
                    envelope.invocation,
                    child_execution_trace_hook,
                    self.completion_key,
                ))
                .await?;
                Ok(RuntimeEffectOutcome::ToolAttempt {
                    launch: Box::new(outcome.launch),
                    triggers: outcome.triggers,
                })
            }
            command => Err(RuntimeEffectControllerError::new(
                crate::RuntimeErrorCode::RuntimeEffectLocalExecutorMismatch,
                format!(
                    "local tool executor cannot execute {} command",
                    command.kind().as_str()
                ),
            )),
        }
    }
}

#[async_trait::async_trait]
impl RuntimeEffectLocalRunner for LocalPreparedToolAttemptEffectRunner<'_> {
    fn uses_task_boundary(&self, command: &RuntimeEffectCommand) -> bool {
        matches!(command, RuntimeEffectCommand::ToolAttempt { .. })
    }

    async fn execute(
        self: Box<Self>,
        envelope: RuntimeEffectEnvelope,
    ) -> Result<RuntimeEffectOutcome, RuntimeEffectControllerError> {
        let RuntimeEffectCommand::ToolAttempt {
            call,
            execution_grant,
            attempt,
            max_attempts,
        } = envelope.command
        else {
            return Err(RuntimeEffectControllerError::new(
                crate::RuntimeErrorCode::RuntimeEffectLocalExecutorMismatch,
                "prepared tool attempt executor requires a tool_attempt command",
            ));
        };
        let mut dispatch = (*self.dispatch).clone();
        dispatch.parent_invocation = Some(envelope.invocation.clone());
        dispatch.direct_completions = dispatch
            .direct_completions
            .with_parent_invocation(Some(envelope.invocation.clone()));
        dispatch.trigger_outcomes = crate::tool_dispatch::ToolTriggerOutcomeBuffer::default();
        let dispatch = Arc::new(dispatch);
        let tool_context = self
            .tool_context
            .with_attempt_dispatch(Arc::clone(&dispatch), envelope.invocation);
        tool_context.install_prederived_completion_key(self.completion_key);
        let outcome = Box::pin(crate::tool_dispatch::execute_prepared_tool_attempt_effect(
            dispatch.as_ref(),
            call,
            execution_grant,
            attempt,
            max_attempts,
            tool_context,
        ))
        .await?;
        Ok(RuntimeEffectOutcome::ToolAttempt {
            launch: Box::new(outcome.launch),
            triggers: outcome.triggers,
        })
    }
}

#[async_trait::async_trait]
impl RuntimeEffectLocalRunner for LocalTurnEffectRunner {
    fn uses_task_boundary(&self, command: &RuntimeEffectCommand) -> bool {
        matches!(
            command,
            RuntimeEffectCommand::LlmCall { .. }
                | RuntimeEffectCommand::AssistantResponseHooks { .. }
                | RuntimeEffectCommand::ToolBatch { .. }
                | RuntimeEffectCommand::ExecCode { .. }
        )
    }

    async fn execute(
        self: Box<Self>,
        envelope: RuntimeEffectEnvelope,
    ) -> Result<RuntimeEffectOutcome, RuntimeEffectControllerError> {
        let mut runner = *self;
        let result = match envelope.command {
            RuntimeEffectCommand::LlmCall { request } => {
                let (result, text_streamed, call_record) = runner
                    .driver
                    .run_llm_call(
                        Arc::new((*request).into_request(None, None)),
                        runner.protocol_iteration,
                        envelope.invocation,
                        &runner.event_tx,
                        &runner.cancellation,
                    )
                    .await;
                Ok(RuntimeEffectOutcome::LlmCall {
                    result: Box::new(result),
                    text_streamed,
                    call_record,
                })
            }
            RuntimeEffectCommand::AssistantResponseHooks { response } => runner
                .driver
                .run_assistant_response_hooks(*response)
                .await
                .map(
                    |(response, events)| RuntimeEffectOutcome::AssistantResponseHooks {
                        response: Box::new(response),
                        events,
                    },
                ),
            RuntimeEffectCommand::ToolBatch { batch } => Box::pin(runner.driver.run_tool_batch(
                batch,
                envelope.invocation,
                &runner.event_tx,
                &runner.cancellation,
            ))
            .await
            .map(|outcome| RuntimeEffectOutcome::ToolBatch {
                launches: outcome.launches,
                triggers: outcome.triggers,
                settlement_order: outcome.settlement_order,
            }),
            RuntimeEffectCommand::ExecCode { language, code } => {
                let result = runner
                    .driver
                    .run_exec_code(
                        language,
                        &code,
                        runner.messages.clone(),
                        runner.protocol_iteration,
                        envelope.invocation,
                        &runner.event_tx,
                        &runner.cancellation,
                    )
                    .await?;
                Ok(RuntimeEffectOutcome::ExecCode {
                    result: Box::new(result),
                })
            }
            RuntimeEffectCommand::Checkpoint { checkpoint } => Ok(runner
                .driver
                .execute_checkpoint_locally(
                    runner.messages.clone(),
                    runner.protocol_iteration,
                    checkpoint,
                    &runner.event_tx,
                )
                .await),
            RuntimeEffectCommand::SyncExecutionEnvironment {
                update_machine_config,
            } => Ok(RuntimeEffectOutcome::SyncExecutionEnvironment {
                result: runner
                    .driver
                    .refresh_execution_environment(runner.messages.clone(), update_machine_config)
                    .await
                    .map_err(|err| err.to_string()),
            }),
            RuntimeEffectCommand::Sleep { duration_ms } => {
                sleep_with_cancellation(
                    duration_ms,
                    &runner.cancellation,
                    runner.driver.host.core.clock.as_ref(),
                )
                .await?;
                Ok(RuntimeEffectOutcome::Sleep)
            }
            command => Err(RuntimeEffectControllerError::new(
                crate::RuntimeErrorCode::RuntimeEffectLocalExecutorMismatch,
                format!(
                    "local turn executor cannot execute {} command",
                    command.kind().as_str()
                ),
            )),
        };
        *runner.update.lock_recover() = Some(TurnEffectStateUpdate {
            policy: runner.driver.policy,
            llm_stream_summaries: runner.driver.llm_stream_summaries,
            next_llm_ordinal: runner.driver.next_llm_ordinal,
            pending_queue_claims: runner.driver.pending_queue_claims,
            pending_turn_input_claims: runner.driver.pending_turn_input_claims,
            pending_checkpoint_turn_input_claim: runner.driver.pending_checkpoint_turn_input_claim,
        });
        result
    }
}

#[async_trait::async_trait]
impl RuntimeEffectLocalRunner for LocalDirectEffectRunner {
    fn uses_task_boundary(&self, command: &RuntimeEffectCommand) -> bool {
        matches!(command, RuntimeEffectCommand::Direct { .. })
    }

    async fn execute(
        mut self: Box<Self>,
        envelope: RuntimeEffectEnvelope,
    ) -> Result<RuntimeEffectOutcome, RuntimeEffectControllerError> {
        match envelope.command {
            RuntimeEffectCommand::Direct { request, .. } => {
                let (result, call_record) = self
                    .run_direct_llm_request((*request).into_request(
                        crate::session_model::transport_stream_events(&self.provider, None),
                        None,
                    ))
                    .await;
                Ok(RuntimeEffectOutcome::Direct {
                    result: Box::new(result),
                    call_record,
                })
            }
            RuntimeEffectCommand::Sleep { duration_ms } => {
                sleep_with_cancellation(
                    duration_ms,
                    &CancellationToken::new(),
                    &crate::SystemClock,
                )
                .await?;
                Ok(RuntimeEffectOutcome::Sleep)
            }
            command => Err(RuntimeEffectControllerError::new(
                crate::RuntimeErrorCode::RuntimeEffectLocalExecutorMismatch,
                format!(
                    "local direct executor cannot execute {} command",
                    command.kind().as_str()
                ),
            )),
        }
    }
}

#[async_trait::async_trait]
impl RuntimeEffectLocalRunner for RemoteEffectRunner {
    async fn execute(
        self: Box<Self>,
        envelope: RuntimeEffectEnvelope,
    ) -> Result<RuntimeEffectOutcome, RuntimeEffectControllerError> {
        let (response, response_rx) = oneshot::channel();
        self.requests
            .send(RemoteLocalExecutionRequest { envelope, response })
            .map_err(|_| {
                RuntimeEffectControllerError::new(
                    crate::RuntimeErrorCode::RuntimeEffectLocalTaskClosed,
                    "spawned effect local executor is no longer running",
                )
            })?;
        response_rx.await.map_err(|_| {
            RuntimeEffectControllerError::new(
                crate::RuntimeErrorCode::RuntimeEffectLocalTaskClosed,
                "spawned effect local executor response was dropped",
            )
        })?
    }
}

impl LocalDirectEffectRunner {
    async fn run_direct_llm_request(&mut self, request: CoreLlmRequest) -> RuntimeDirectLlmOutcome {
        let request = match crate::attachments::resolve_llm_request_attachments(
            request,
            self.attachment_store.as_ref(),
        )
        .await
        {
            Ok(request) => request,
            Err(err) => {
                return (
                    Err(LlmCallError {
                        message: err.to_string(),
                        retryable: false,
                        kind: crate::ProviderFailureKind::Unknown,
                        raw: None,
                        code: Some("attachment_resolution_failed".to_string()),
                        terminal_reason: crate::LlmTerminalReason::ProviderError,
                        request_body: None,
                        partial_response: None,
                    }),
                    None,
                );
            }
        };
        match self.provider.complete(request).await {
            Ok(completion) => (Ok(completion.response), Some(completion.call_record)),
            Err(failure) => (
                Err(llm_call_error_from_transport(failure.error)),
                Some(failure.call_record),
            ),
        }
    }
}

async fn execute_local_sleep(
    envelope: RuntimeEffectEnvelope,
    cancellation: CancellationToken,
    clock: &dyn crate::Clock,
) -> Result<RuntimeEffectOutcome, RuntimeEffectControllerError> {
    match envelope.command {
        RuntimeEffectCommand::Sleep { duration_ms } => {
            sleep_with_cancellation(duration_ms, &cancellation, clock).await?;
            Ok(RuntimeEffectOutcome::Sleep)
        }
        command => Err(RuntimeEffectControllerError::new(
            crate::RuntimeErrorCode::RuntimeEffectLocalExecutorMismatch,
            format!(
                "local sleep executor cannot execute {} command",
                command.kind().as_str()
            ),
        )),
    }
}

async fn sleep_with_cancellation(
    duration_ms: u64,
    cancellation: &CancellationToken,
    clock: &dyn crate::Clock,
) -> Result<(), RuntimeEffectControllerError> {
    let sleep = clock.sleep(std::time::Duration::from_millis(duration_ms));
    tokio::pin!(sleep);
    tokio::select! {
        _ = cancellation.cancelled() => Err(RuntimeEffectControllerError::new(
            crate::RuntimeErrorCode::RuntimeEffectSleepCancelled,
            "runtime effect sleep was cancelled",
        )),
        _ = &mut sleep => Ok(()),
    }
}

#[cfg(test)]
mod task_boundary_tests {
    use super::*;
    use crate::RuntimeInvocation;
    use std::sync::atomic::{AtomicBool, Ordering};

    struct TaskIdentityRunner {
        observed: oneshot::Sender<tokio::task::Id>,
    }

    #[async_trait::async_trait]
    impl RuntimeEffectLocalRunner for TaskIdentityRunner {
        fn uses_task_boundary(&self, command: &RuntimeEffectCommand) -> bool {
            matches!(command, RuntimeEffectCommand::ExecCode { .. })
        }

        async fn execute(
            self: Box<Self>,
            _envelope: RuntimeEffectEnvelope,
        ) -> Result<RuntimeEffectOutcome, RuntimeEffectControllerError> {
            let _ = self.observed.send(tokio::task::id());
            Ok(RuntimeEffectOutcome::Sleep)
        }
    }

    #[tokio::test]
    async fn owned_heavy_effect_runs_on_a_fresh_task() {
        let (observed_tx, observed_rx) = oneshot::channel();
        let executor = RuntimeEffectLocalExecutor {
            state: RuntimeEffectLocalExecutorState::Target(LocalTarget::OwnedRunner(Box::new(
                TaskIdentityRunner {
                    observed: observed_tx,
                },
            ))),
            replay_trace: None,
        };
        let parent = crate::task::spawn(async move {
            let parent_id = tokio::task::id();
            let outcome = executor
                .execute(RuntimeEffectEnvelope::new(
                    RuntimeInvocation::effect(
                        crate::RuntimeScope::new("task-boundary"),
                        "exec",
                        RuntimeEffectKind::ExecCode,
                        "task-boundary:exec",
                    ),
                    RuntimeEffectCommand::ExecCode {
                        language: "text".to_string(),
                        code: String::new(),
                    },
                ))
                .await
                .expect("spawned effect");
            assert!(matches!(outcome, RuntimeEffectOutcome::Sleep));
            parent_id
        });
        let child_id = observed_rx.await.expect("effect task id");
        let parent_id = parent.await.expect("parent task");
        assert_ne!(child_id, parent_id);
    }

    #[test]
    fn sleep_wait_shape_round_trips_through_remote_execution() {
        for observe_turn_cancel in [false, true] {
            let cancellation = CancellationToken::new();
            let scope = ExecutionScope::runtime_operation(format!(
                "sleep-wait-shape-{observe_turn_cancel}"
            ));
            let (remote, local) = RuntimeEffectLocalExecutor::sleep(cancellation.clone())
                .with_turn_cancel_observation(observe_turn_cancel)
                .with_turn_cancel_scope(scope.clone())
                .into_remote_execution();

            assert!(local.is_none());
            let options = remote.into_sleep_options();
            assert_eq!(options.observe_turn_cancel, observe_turn_cancel);
            assert_eq!(options.turn_cancel_scope, Some(scope));

            cancellation.cancel();
            assert!(options.cancellation.is_cancelled());
        }
    }

    #[test]
    fn external_wait_shape_round_trips_through_remote_execution() {
        for observe_turn_cancel in [false, true] {
            let cancellation = CancellationToken::new();
            let deadline = Some(Instant::now() + std::time::Duration::from_secs(1));
            let scope = ExecutionScope::runtime_operation(format!(
                "external-wait-shape-{observe_turn_cancel}"
            ));
            let (remote, local) =
                RuntimeEffectLocalExecutor::await_event(cancellation.clone(), deadline)
                    .with_turn_cancel_observation(observe_turn_cancel)
                    .with_turn_cancel_scope(scope.clone())
                    .into_remote_execution();

            assert!(local.is_none());
            let options = remote
                .into_await_event_options()
                .expect("await-event options");
            assert_eq!(options.observe_turn_cancel, observe_turn_cancel);
            assert_eq!(options.turn_cancel_scope, Some(scope));
            assert_eq!(options.deadline, deadline);

            cancellation.cancel();
            assert!(options.cancellation.is_cancelled());
        }
    }

    #[tokio::test]
    async fn replayed_effect_may_skip_remote_local_execution() {
        let executed = Arc::new(AtomicBool::new(false));
        let local_executed = Arc::clone(&executed);
        let local_executor = RuntimeEffectLocalExecutor::testing(move |_| async move {
            local_executed.store(true, Ordering::SeqCst);
            Ok(RuntimeEffectOutcome::Sleep)
        });
        let controller = InlineRuntimeEffectController::default();
        let (proxy, mut requests) = EffectTaskController::scoped(
            &controller,
            ExecutionScope::runtime_operation("replay-skips-local"),
        )
        .expect("task controller");
        let envelope = RuntimeEffectEnvelope::new(
            RuntimeInvocation::effect(
                crate::RuntimeScope::new("replay-skips-local"),
                "sleep",
                RuntimeEffectKind::Sleep,
                "replay-skips-local:sleep",
            ),
            RuntimeEffectCommand::Sleep { duration_ms: 0 },
        );
        let invoke = proxy.controller().execute_effect(envelope, local_executor);
        let service = async {
            let Some(EffectControllerTaskRequest::Execute {
                local_executor,
                response,
                ..
            }) = requests.recv().await
            else {
                panic!("expected proxied execute request");
            };
            drop(local_executor);
            tokio::task::yield_now().await;
            response
                .send(Ok(RuntimeEffectOutcome::Sleep))
                .expect("proxy response receiver");
        };
        let (outcome, ()) = tokio::join!(invoke, service);
        assert!(matches!(outcome, Ok(RuntimeEffectOutcome::Sleep)));
        assert!(!executed.load(Ordering::SeqCst));
    }
}
