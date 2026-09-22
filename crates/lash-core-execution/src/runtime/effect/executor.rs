use crate::ClockWallTime;
use std::collections::HashMap;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Instant;

use tokio::sync::mpsc;
use tokio::sync::oneshot;
use tokio_util::sync::CancellationToken;

mod await_event_support;
#[doc(hidden)]
pub mod control;
mod controller_error;
mod native_controller;
mod process_local;

mod language_runtime;
mod scoped;
mod task_panic;
mod trigger;
mod turn_control_authority;

pub use await_event_support::await_event_scope_not_retirable;
pub use control::EffectControllerTaskRequest;
pub use control::RuntimeEffectControllerHandle;
pub use control::{
    AwaitEventKey, AwaitEventResolver, AwaitEventWaitIdentity, BoundaryReason,
    CompletionKeyPreparation, EffectHost, EffectJournalIdentity, EffectJournalRetirement,
    EffectRetirementGate, ExecutionScope, ExternalCompletionError, QueuedLaneAcquisition,
    QueuedLaneAttempt, QueuedLaneGuard, QueuedLaneHolder, QueuedLaneProbe, Resolution,
    ResolveOutcome, RuntimeEffectController, RuntimeEffectFailureDisposition, ScopeBoundController,
    ScopedEffectController, SegmentProgress, ToolIntentOutcomeSink, ToolIntentPreparation,
    ToolIntentSubmissionGuard, TurnCancelClosureOwnerBinding,
};
pub use control::{EffectTaskController, drive_effect_controller_task};
pub use controller_error::RuntimeEffectControllerError;
pub use lash_core_store::admitted_scope::{AdmittedScope, AdmittedScopeError};
pub use lash_core_store::effect_opener::EffectOpener;

/// The one typed refusal a controller that does not implement durable effect
/// groups returns from `open_effect_group`, `await_next_settlement` and
/// `close_effect_group`.
///
/// Since FIG-2266 the three group methods have no default bodies, so every
/// controller answers the question in its own source: it implements groups, or
/// it calls this. Before that a `supports_effect_groups()` flag defaulted to
/// `false` beside three methods that defaulted to refusing, which made "I have
/// not thought about groups" and "I refuse groups" the same program text — and
/// made a delegating wrapper that forgot to forward look coherent while
/// silently denying a capability its inner controller had.
///
/// `controller` names the refusing type, so the error says which link in a
/// wrapper chain answered rather than only that something did.
#[must_use]
pub fn effect_groups_unsupported(controller: &str) -> RuntimeEffectControllerError {
    RuntimeEffectControllerError::new(
        crate::RuntimeErrorCode::EffectGroupUnsupported,
        format!("{controller} does not implement durable effect groups"),
    )
}
pub use lash_core_store::turn_control_binding::admitted_turn_cancel_scope;
pub use lash_core_store::turn_control_binding::turn_control_binding_id_for_scope;
pub use lash_core_store::turn_control_binding::{TurnControlBindingId, TurnControlBindingIdError};
pub use native_controller::NativeRuntimeEffectController;
pub(crate) use native_controller::{NativeEffectGroups, NativeGroupClosing};
pub use trigger::TriggerLocalExecution;
pub use turn_control_authority::{
    TurnCancellationAuthority, TurnControlAttachment, TurnControlAuthorityOwner,
    TurnControlBinding, TurnControlParticipation, concrete_turn_cancellation_authority,
};

use crate::LlmRequest as CoreLlmRequest;
use crate::ProcessRegistry;
use crate::RuntimeError;
use crate::provider::ProviderHandle;
use crate::sansio::LlmCallError;
use control::{RemoteLocalExecutionRequest, ScopedEffectControllerInner};

use super::envelope::{
    ProcessCommand, ProcessEffectOutcome, RuntimeDirectLlmOutcome, RuntimeEffectCommand,
    RuntimeEffectEnvelope, RuntimeEffectOutcome,
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
    /// Selects the durable turn-cancel race shape. Restate-backed callers must
    /// keep this stable for a wait's lifetime; see
    /// `docs/adr/0012-durable-waits-via-effect-host-engines.md`.
    pub observe_turn_cancel: bool,
    pub turn_cancel_scope: Option<crate::ExecutionScope>,
}

/// Host controls attached to one sleep effect.
pub struct RuntimeSleepOptions {
    pub cancellation: CancellationToken,
    /// Selects the durable turn-cancel race shape. Restate-backed callers must
    /// keep this stable for a wait's lifetime; see
    /// `docs/adr/0012-durable-waits-via-effect-host-engines.md`.
    pub observe_turn_cancel: bool,
    pub turn_cancel_scope: Option<crate::ExecutionScope>,
    /// The clock the sleep was dispatched under. A deadline-bearing sleep is
    /// resolved against this authority, never an ambient system clock, so the
    /// injected clock stays the single time source on the runtime path.
    pub clock: Arc<dyn crate::Clock>,
}

// =============================================================================
// Local executor (per-effect borrowed runner state)
// =============================================================================

struct WaitControls {
    cancellation: CancellationToken,
    observe_turn_cancel: bool,
    turn_cancel_scope: Option<crate::ExecutionScope>,
}

/// The process one run is admitted for.
///
/// The registration names a *reusable* process; the incarnation is the one the
/// worker's authority CAS admitted. The logical opener is the pair and never
/// the name alone (ADR 0099 §1), so they travel together: anything minted per
/// opener — group and child identity, cancellation and close fences — binds
/// both, and a re-registered name cannot reach its predecessor's work.
#[derive(Clone, Debug)]
pub struct AdmittedProcess {
    pub registration: crate::ProcessRegistration,
    pub incarnation: crate::ProcessIncarnation,
}

#[async_trait::async_trait]
pub trait ProcessRunner: Send + Sync {
    async fn run_process(
        &self,
        admitted: AdmittedProcess,
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
pub struct TurnCancelWait {
    cancellation: CancellationToken,
    /// `Some` when the wait attaches the turn-cancel gate for that scope;
    /// `None` when the enclosing execution runs without turn observation, as
    /// process bodies do.
    observed_scope: Option<crate::ExecutionScope>,
}

impl TurnCancelWait {
    /// The wait races `cancellation` and observes the turn-cancel gate of
    /// `scope`.
    pub fn observing(cancellation: CancellationToken, scope: crate::ExecutionScope) -> Self {
        Self {
            cancellation,
            observed_scope: Some(scope),
        }
    }

    /// The wait races `cancellation` and never attaches a turn-cancel gate.
    pub fn unobserved(cancellation: CancellationToken) -> Self {
        Self {
            cancellation,
            observed_scope: None,
        }
    }

    /// The cooperative cancellation the wait races, for callers that carry
    /// the trio whole and still need the token alone.
    pub(crate) fn cancellation(&self) -> &CancellationToken {
        &self.cancellation
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
/// The observer also receives the store's realization verdict for the command:
/// whether the durable write landed on this call or coalesced onto a fact the
/// store already held under the same durable key (FIG-3070). Commands that
/// carry no durable identity report [`StoreRealization::Realized`].
pub type ProcessOutcomeObserver =
    Arc<dyn Fn(&ProcessEffectOutcome, crate::StoreRealization) + Send + Sync + 'static>;

pub struct ProcessLocalExecution {
    pub registry: Arc<dyn ProcessRegistry>,
    pub process_work: Arc<dyn crate::ProcessWorkSubstrate>,
    pub process_env_store: Option<Arc<dyn crate::ProcessExecutionEnvStore>>,
    pub process_engines: Option<crate::ProcessEngineRegistry>,
    pub turn_cancellation: Option<ProcessTurnCancellation>,
    pub effect_controller: Option<Arc<dyn RuntimeEffectController>>,
    pub(crate) outcome_observer: Option<ProcessOutcomeObserver>,
}

/// Local execution target for the journaled process-definition CAS write
/// (FIG-3470): unlike [`ProcessLocalExecution`], which serves the process
/// service, this target binds only the definition registry the
/// `RegisterDefinition` command writes through.
pub struct ProcessDefinitionLocalExecution {
    pub registry: Arc<dyn crate::ProcessDefinitionRegistry>,
}

pub(super) struct LocalDirectEffectRunner {
    provider: ProviderHandle,
    charge_safety: crate::ChargeSafetyPolicy,
    attachment_store: Arc<crate::SessionAttachmentStore>,
}

struct LocalToolBatchEffectRunner<'run> {
    context: crate::RuntimeExecutionContext<'run>,
    child_trace_hooks: HashMap<String, crate::ToolChildExecutionTraceHook>,
    issuing_node_ids: Arc<HashMap<String, String>>,
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
pub trait RuntimeEffectLocalRunner: Send {
    fn uses_task_boundary(&self, _command: &RuntimeEffectCommand) -> bool {
        false
    }

    /// The handler-level driver this runner carries, when it is a tool child
    /// (ADR 0099 §2, FIG-2266).
    ///
    /// `execute` runs the runner's whole body to a terminal, which is correct
    /// only where the runner may build the child's admitted controller itself.
    /// A tier whose admitted controller is bound to a live handler context
    /// calls the returned driver with the controller *it* built instead, so
    /// the driver runs at handler level rather than inside a recorded body.
    /// `None` is the honest answer for every other kind of runner — leaf
    /// effects have no handler-level driver to hand out.
    fn tool_child_driver(&self) -> Option<&dyn super::tool_child_driver::ToolChildDriver> {
        None
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
    ProcessDefinitions(ProcessDefinitionLocalExecution),
    Trigger(TriggerLocalExecution),
    TurnAcceptance(Arc<dyn crate::TurnInputStore>),
    /// The recorded presentation boundary's local work (ADR 0099 §6,
    /// FIG-3420): run the session's ordered presentation steps once over the
    /// journaled `PresentToolResult` input.
    Presentation(PresentationLocalExecution),
    OwnedRunner(Box<dyn RuntimeEffectLocalRunner + Send + 'static>),
}

/// Everything the presentation boundary needs that is not on the journaled
/// command: the session's plugin chain, the settlement a step may read, the
/// store retained artifacts are `put` into, and the recorded
/// attachment-acceptance environment the materialization notices compute
/// under.
pub struct PresentationLocalExecution {
    pub plugins: Arc<crate::plugin::PluginSession>,
    pub settlement: Arc<super::ToolSettlement>,
    pub attachment_store: Arc<crate::SessionAttachmentStore>,
    pub attachment_acceptance: crate::provider::AttachmentCapabilitySnapshot,
}

impl PresentationLocalExecution {
    async fn execute(
        self,
        envelope: RuntimeEffectEnvelope,
    ) -> Result<RuntimeEffectOutcome, RuntimeEffectControllerError> {
        let RuntimeEffectCommand::PresentToolResult {
            call_id,
            tool_name,
            args,
            output,
            duration_ms,
        } = envelope.command
        else {
            return Err(RuntimeEffectControllerError::new(
                crate::RuntimeErrorCode::RuntimeEffectLocalExecutorMismatch,
                format!(
                    "presentation executor cannot execute {} command directly",
                    envelope.command.kind().as_str()
                ),
            ));
        };
        let artifacts = Arc::new(super::SessionPresentationArtifacts::new(Arc::clone(
            &self.attachment_store,
        )));
        let context = crate::plugin::ToolResultProjectionContext {
            session_id: crate::SessionId::from(self.plugins.session_id().to_string()),
            call_id,
            tool_name,
            args,
            output: *output,
            duration_ms,
            artifacts,
        };
        let presentation = self
            .plugins
            .present_tool_result(context, self.settlement, &self.attachment_acceptance)
            .await;
        Ok(RuntimeEffectOutcome::PresentToolResult {
            presentation: Box::new(presentation),
        })
    }
}

enum RuntimeEffectLocalExecutorState<'run> {
    Target(LocalTarget),
    Runner(Box<dyn RuntimeEffectLocalRunner + Send + 'run>),
}

/// Scoped local executor provided to a [`RuntimeEffectController`] for one effect.
///
/// Durable controllers may ignore it and replay their own recorded result. The
/// default native controller delegates to it, so local provider/tool/checkpoint
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
    /// Constructs a local path that rejects unavailable native execution.
    pub fn unavailable() -> Self {
        Self {
            state: RuntimeEffectLocalExecutorState::Target(LocalTarget::Unavailable),
            replay_trace: None,
        }
    }

    pub fn sleep(cancellation: CancellationToken) -> Self {
        Self::sleep_with_clock(cancellation, Arc::new(crate::SystemClock))
    }

    /// Builds the native sleep path with an injected clock for effect-host and conformance
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

    /// Builds the native durable-wait path for effect-host implementors using the system clock and
    /// the supplied optional deadline.
    pub fn await_event(cancellation: CancellationToken, deadline: Option<Instant>) -> Self {
        Self::await_event_with_clock(cancellation, deadline, Arc::new(crate::SystemClock))
    }

    /// Builds the native durable-wait path with an injected clock for effect-host and conformance
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

    /// Builds the native sleep path from the complete turn-cancel trio, so an
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

    /// Builds the native durable-wait path from the complete turn-cancel trio,
    /// so an in-workspace wait cannot be journaled with a half-stamped one.
    pub fn await_event_under(
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

    /// This is a replay-shape switch, not a live policy toggle. A Restate
    /// invocation must reconstruct the same value on every attempt.
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

    pub fn with_process_effect_controller(
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

    /// Binds process engines that own start-time artifact lifecycle hooks.
    pub fn with_process_engines(mut self, engines: crate::ProcessEngineRegistry) -> Self {
        if let RuntimeEffectLocalExecutorState::Target(LocalTarget::Process(execution)) =
            &mut self.state
        {
            execution.process_engines = Some(engines);
        }
        self
    }

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

    /// Binds process registry and required process-work services for effect-host implementors
    /// executing process effects natively.
    pub fn processes(
        registry: Arc<dyn ProcessRegistry>,
        process_work: Arc<dyn crate::ProcessWorkSubstrate>,
    ) -> Self {
        Self {
            state: RuntimeEffectLocalExecutorState::Target(LocalTarget::Process(
                ProcessLocalExecution {
                    registry,
                    process_work,
                    process_env_store: None,
                    process_engines: None,
                    turn_cancellation: None,
                    effect_controller: None,
                    outcome_observer: None,
                },
            )),
            replay_trace: None,
        }
    }

    /// Binds the process-definition registry for the journaled
    /// `RegisterDefinition` write (FIG-3470). This is the only command this
    /// executor serves; every other process command still requires
    /// [`Self::processes`].
    pub fn process_definitions(registry: Arc<dyn crate::ProcessDefinitionRegistry>) -> Self {
        Self {
            state: RuntimeEffectLocalExecutorState::Target(LocalTarget::ProcessDefinitions(
                ProcessDefinitionLocalExecution { registry },
            )),
            replay_trace: None,
        }
    }

    /// Binds a turn-input store for effect-host implementors executing the
    /// durable turn-acceptance effect (ADR 0069 §6) natively.
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

    /// Binds the session's plugin chain and artifact store for the journaled
    /// `PresentToolResult` boundary (ADR 0099 §6, FIG-3420): the ordered
    /// presentation steps run exactly once on the first execution and replay
    /// serves the recorded `ToolPresentation`.
    pub fn presentation(
        plugins: Arc<crate::plugin::PluginSession>,
        settlement: Arc<super::ToolSettlement>,
        attachment_store: Arc<crate::SessionAttachmentStore>,
        attachment_acceptance: crate::provider::AttachmentCapabilitySnapshot,
    ) -> Self {
        Self {
            state: RuntimeEffectLocalExecutorState::Target(LocalTarget::Presentation(
                PresentationLocalExecution {
                    plugins,
                    settlement,
                    attachment_store,
                    attachment_acceptance,
                },
            )),
            replay_trace: None,
        }
    }

    pub fn triggers(store: Arc<dyn crate::TriggerStore>) -> Self {
        Self {
            state: RuntimeEffectLocalExecutorState::Target(LocalTarget::Trigger(
                TriggerLocalExecution { store },
            )),
            replay_trace: None,
        }
    }

    pub(crate) fn language_runtime_value_with<F, Fut>(run: F) -> Self
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

    /// This is deliberately hidden from the published default documentation;
    /// it is not an integrator seam for fabricating runtime effect outcomes.
    pub fn testing<F, Fut>(run: F) -> Self
    where
        F: FnOnce(RuntimeEffectEnvelope) -> Fut + Send + 'run,
        Fut: Future<Output = Result<RuntimeEffectOutcome, RuntimeEffectControllerError>>
            + Send
            + 'run,
    {
        Self::language_runtime_value_with(run)
    }

    pub fn owned_runner(
        runner: Box<dyn RuntimeEffectLocalRunner + Send + 'static>,
        replay_trace: Option<super::RuntimeEffectReplayTrace>,
    ) -> Self {
        Self {
            state: RuntimeEffectLocalExecutorState::Target(LocalTarget::OwnedRunner(runner)),
            replay_trace,
        }
    }

    pub fn direct(
        provider: ProviderHandle,
        charge_safety: crate::ChargeSafetyPolicy,
        attachment_store: Arc<crate::SessionAttachmentStore>,
        replay_trace: Option<super::RuntimeEffectReplayTrace>,
    ) -> Self {
        Self {
            state: RuntimeEffectLocalExecutorState::Target(LocalTarget::OwnedRunner(Box::new(
                LocalDirectEffectRunner {
                    provider,
                    charge_safety,
                    attachment_store,
                },
            ))),
            replay_trace,
        }
    }

    pub(crate) fn tool_batch(
        context: crate::RuntimeExecutionContext<'run>,
        child_trace_hooks: HashMap<String, crate::ToolChildExecutionTraceHook>,
        issuing_node_ids: HashMap<String, String>,
        completion_key: Option<crate::AwaitEventKey>,
    ) -> Self {
        let replay_trace = context.replay_validation_trace();
        let issuing_node_ids = Arc::new(issuing_node_ids);
        if let Some(context) = context.to_static() {
            return Self {
                state: RuntimeEffectLocalExecutorState::Target(LocalTarget::OwnedRunner(Box::new(
                    LocalToolBatchEffectRunner {
                        context,
                        child_trace_hooks,
                        issuing_node_ids,
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
                issuing_node_ids,
                completion_key,
            })),
            replay_trace,
        }
    }

    pub fn prepared_tool_attempt(
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

    /// The handler-level driver this executor carries, when it is a tool
    /// child; `None` for every leaf executor (ADR 0099 §2, FIG-2266).
    ///
    /// Same answer the resolver gave: this only *reaches* the runner the
    /// resolver routed — it does not re-decide routing. A tier that drives
    /// tool children at handler level resolves once through
    /// [`GroupExecutors::executor_for`](super::group_drain::GroupExecutors::executor_for)
    /// and reads this.
    pub fn tool_child_driver(&self) -> Option<&dyn super::tool_child_driver::ToolChildDriver> {
        match &self.state {
            RuntimeEffectLocalExecutorState::Runner(runner) => runner.tool_child_driver(),
            RuntimeEffectLocalExecutorState::Target(LocalTarget::OwnedRunner(runner)) => {
                runner.tool_child_driver()
            }
            RuntimeEffectLocalExecutorState::Target(_) => None,
        }
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
                    lash_core_ids::execution_permit::inherit_process_execution_permit(
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
            RuntimeEffectLocalExecutorState::Target(LocalTarget::ProcessDefinitions(_)) => {
                Err(RuntimeEffectControllerError::new(
                    crate::RuntimeErrorCode::RuntimeEffectLocalExecutorMismatch,
                    format!(
                        "process-definition executor cannot execute {} command directly",
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
            RuntimeEffectLocalExecutorState::Target(LocalTarget::Presentation(execution)) => {
                execution.execute(envelope).await
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

    /// Extracts the process-definition registry for the journaled
    /// `RegisterDefinition` write (FIG-3470).
    pub fn into_process_definitions(
        self,
    ) -> Result<ProcessDefinitionLocalExecution, RuntimeEffectControllerError> {
        match self.state {
            RuntimeEffectLocalExecutorState::Target(LocalTarget::ProcessDefinitions(execution)) => {
                Ok(execution)
            }
            _ => Err(RuntimeEffectControllerError::new(
                crate::RuntimeErrorCode::RuntimeEffectLocalExecutorUnavailable,
                "no process-definition registry is available for the register-definition command",
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
        invocation: crate::RuntimeEffectInvocation,
        command: crate::TriggerCommand,
    ) -> Result<RuntimeEffectOutcome, RuntimeEffectControllerError> {
        let operation_id = invocation.effect_id().to_string();
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
                clock,
            }) => RuntimeSleepOptions {
                cancellation,
                observe_turn_cancel,
                turn_cancel_scope,
                clock,
            },
            _ => RuntimeSleepOptions {
                cancellation: CancellationToken::new(),
                observe_turn_cancel: false,
                turn_cancel_scope: None,
                clock: Arc::new(crate::SystemClock),
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
                    envelope.invocation.into_runtime_invocation(),
                    self.child_trace_hooks,
                    Arc::clone(&self.issuing_node_ids),
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
                    envelope.invocation.into_runtime_invocation(),
                    child_execution_trace_hook,
                    self.completion_key,
                ))
                .await?;
                Ok(RuntimeEffectOutcome::ToolAttempt {
                    launch: Box::new(outcome.launch),
                    triggers: outcome.triggers,
                    capture: (!outcome.capture.is_empty()).then(|| Box::new(outcome.capture)),
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
        dispatch.parent_invocation = Some(envelope.invocation.clone().into_runtime_invocation());
        dispatch.direct_completions = dispatch
            .direct_completions
            .with_tool_attempt_parent_invocation(
                envelope.invocation.clone().into_runtime_invocation(),
            )
            .with_usage_ledger(crate::runtime::ToolUsageLedger::for_attempt(attempt));
        dispatch.trigger_outcomes = crate::tool_dispatch::ToolTriggerOutcomeBuffer::default();
        // Attempt-local buffers: what this attempt commits is drained into the
        // journaled capture, never read out of a buffer it shares with
        // anything else.
        dispatch.checkpoint_messages = crate::tool_dispatch::CheckpointMessageBuffer::default();
        let dispatch = Arc::new(dispatch);
        let tool_context = self.tool_context.with_attempt_dispatch(
            Arc::clone(&dispatch),
            envelope.invocation.into_runtime_invocation(),
        );
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
            capture: (!outcome.capture.is_empty()).then(|| Box::new(outcome.capture)),
        })
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
            RuntimeEffectCommand::Sleep { spec } => {
                let duration_ms = sleep_duration(spec, crate::SystemClock.timestamp_ms());
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
                        code: Some(crate::FailureCode::lash(
                            crate::TurnFailureCode::AttachmentResolutionFailed,
                        )),
                        terminal_reason: crate::LlmTerminalReason::ProviderError,
                        request_body: None,
                        partial_response: None,
                    }),
                    None,
                );
            }
        };
        match self
            .provider
            .complete_with_charge_safety(request, self.charge_safety.clone())
            .await
        {
            Ok(completion) => (Ok(completion.response), Some(completion.call_record)),
            Err(failure) => (
                Err(llm_call_error_from_transport(failure.error)),
                Some(*failure.call_record),
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
        RuntimeEffectCommand::Sleep { spec } => {
            let duration_ms = sleep_duration(spec, clock.timestamp_ms());
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

pub fn sleep_duration(spec: crate::SleepSpec, now_ms: u64) -> u64 {
    match spec {
        crate::SleepSpec::For { duration_ms } => duration_ms,
        crate::SleepSpec::Until { deadline_ms } => deadline_ms.saturating_sub(now_ms),
    }
}

pub async fn sleep_with_cancellation(
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
    use crate::RuntimeEffectInvocation;
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
                    RuntimeEffectInvocation::new(
                        crate::EffectAddress::new(
                            crate::ExecutionScope::runtime_operation("task-boundary"),
                            "task-boundary:exec",
                        )
                        .expect("valid task-boundary address"),
                        crate::RuntimeAttribution::none(),
                        "exec",
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
        let controller = NativeRuntimeEffectController::default();
        let execution_scope = ExecutionScope::runtime_operation("replay-skips-local");
        let (proxy, mut requests) = EffectTaskController::scoped(
            &controller,
            crate::AdmittedScope::runtime_operation("replay-skips-local"),
        )
        .expect("task controller");
        let envelope = RuntimeEffectEnvelope::new(
            RuntimeEffectInvocation::new(
                crate::EffectAddress::new(execution_scope, "replay-skips-local:sleep")
                    .expect("valid task proxy address"),
                crate::RuntimeAttribution::none(),
                "sleep",
            ),
            RuntimeEffectCommand::Sleep {
                spec: crate::SleepSpec::For { duration_ms: 0 },
            },
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
