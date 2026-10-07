use crate::ClockWallTime;
use std::pin::Pin;
use std::sync::Arc;

use tokio_util::sync::CancellationToken;

mod await_event_support;
#[doc(hidden)]
pub mod control;
mod controller_error;
mod conversions;
mod direct;
mod process_local;

mod language_runtime;
mod plugin_state;
use plugin_state::record_plugin_state;
mod served_only;
pub use served_only::ServedOnly;
mod task_panic;
mod tool_attempt;
mod trigger;
mod turn_cancel_wait;
pub use turn_cancel_wait::{ProcessTurnCancellation, TurnCancelWait};

pub use await_event_support::await_event_scope_not_retirable;
pub use control::{
    AwaitEventKey, AwaitEventWaitIdentity, CommandJournalGuard, EffectJournalIdentity,
    EffectJournalRetirement, EffectRetirementGate, ExecutionScope, ExternalCompletionError,
    JournalReplay, ProcessDriveStep, RecordedKeyFence, RefusedWriteRange, Resolution,
    ResolveOutcome, SegmentProgress, ServedOnlyRange,
};
pub use controller_error::RuntimeEffectControllerError;
pub use lash_core_store::admitted_scope::AdmittedScope;
pub use lash_core_store::effect_opener::EffectOpener;
#[cfg(feature = "testing")]
pub(crate) use process_local::process_terminal_resolution;

pub use trigger::TriggerLocalExecution;

use crate::ProcessRegistry;

use super::envelope::{
    ProcessCommand, ProcessEffectOutcome, RuntimeEffectCommand, RuntimeEffectEnvelope,
    RuntimeEffectOutcome,
};

/// Host controls attached to one sleep effect.
pub struct RuntimeSleepOptions {
    pub cancellation: CancellationToken,
    /// Selects the durable turn-cancel race shape. Callers keep this stable
    /// for a wait's lifetime (ADR 0132 §6).
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
    pub(super) cancellation: CancellationToken,
    pub(super) observe_turn_cancel: bool,
    pub(super) turn_cancel_scope: Option<crate::ExecutionScope>,
    pub(super) transferable: bool,
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
    /// The required registry that admits every engine start inside its recorded step.
    pub process_engines: crate::ProcessEngineRegistry,
    /// What a host start's recorded admission consults: the session catalog
    /// its host session-lookup grant is checked against, and the mint of a
    /// session-turn start's default binding.
    pub host_start: Box<crate::runtime::HostStartAdmission>,
    pub turn_cancellation: Option<ProcessTurnCancellation>,
    /// The attachment referrers a delivered terminal is acquired through
    /// before the receiver records it (ADR 0124). `None` on a host with no
    /// durable attachment store: its terminals deliver nothing to hold.
    pub attachments: Option<Arc<dyn crate::AttachmentReferrers>>,
    pub(crate) outcome_observer: Option<ProcessOutcomeObserver>,
}

/// Local execution target for the journaled immutable-definition commands:
/// unlike [`ProcessLocalExecution`], which serves the process service, this
/// target binds only the engine registry and the parent's admitted claim that
/// `PublishDefinition` and `GetDefinition` acquire their artifact closures
/// under (ADR 0113 §3.6).
pub struct ProcessDefinitionLocalExecution {
    pub(crate) engines: crate::ProcessEngineRegistry,
    pub(crate) claim: crate::ReferrerClaim,
}

/// An admitted direct call, ready to send: its exact body and the live limit
/// its pinned deadline leaves.
#[derive(Clone, Debug)]
pub struct AdmittedDirectSend {
    pub body: crate::ProviderRequestBody,
    pub limit: crate::ExecutionLimit,
}

pub(super) struct LocalDirectEffectRunner {
    /// Bound only when this body runs, never on a replay (FIG-4404).
    binding: crate::LlmProfileBinding,
    charge_safety: crate::ChargeSafetyPolicy,
    /// The runtime's execution budgets and the enclosing limit the call is
    /// clipped to: the deadline its admission pinned.
    bounds: lash_core_llm::core_internal::ModelCallBounds,
    /// The call's exact provider body, as its admission stored it: every
    /// attempt sends it, and nothing lowers the call again.
    body: crate::ProviderRequestBody,
    /// Who the call spends for (ADR 0127).
    owner: crate::RuntimeOwner,
    /// The request is the body's own work, so its records are made inside
    /// the body and a replay that serves the recorded completion makes none.
    tracing: crate::trace::TraceRuntime,
    /// The body's live step, bound when the body really runs.
    live: Option<Arc<crate::trace::LiveStep>>,
}

struct LocalPreparedToolAttemptEffectRunner<'run> {
    dispatch: Arc<crate::tool_dispatch::ToolDispatchContext<'run>>,
    tool_context: crate::ToolContext<'run>,
}

#[async_trait::async_trait]
pub trait RuntimeEffectLocalRunner: Send {
    /// The session whose accepted state edits this callback body owns.
    /// An engine records these edits with the body result and restores them
    /// before it serves a completed result on replay.
    fn plugin_state_session(&self) -> Option<Arc<crate::PluginSession>> {
        None
    }

    fn uses_task_boundary(&self, _command: &RuntimeEffectCommand) -> bool {
        false
    }

    /// Hands the runner the live step of the body it is about to run: called
    /// once, right before [`execute`](Self::execute), and only for a body the
    /// engine records, never for one that replays by re-execution. A runner
    /// that observes nothing ignores it.
    fn bind_live_step(&mut self, _live: Arc<crate::trace::LiveStep>) {}

    /// Run the body with the enclosing attempt's typed fault latch. A nested
    /// bind fault ends that attempt even if a tool catches its local error.
    async fn execute(
        self: Box<Self>,
        envelope: RuntimeEffectEnvelope,
        effect_attempt: Option<crate::EffectAttempt>,
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
    Process(Box<ProcessLocalExecution>),
    Definition(ProcessDefinitionLocalExecution),
    Trigger(TriggerLocalExecution),
    TurnAcceptance(Arc<dyn crate::TurnInputStore>),
    /// The recorded presentation boundary's local work (ADR 0099 §6,
    /// FIG-3420): run the session's ordered presentation steps once over the
    /// journaled `PresentToolResult` input.
    Presentation(PresentationLocalExecution),
    /// The recorded execution-environment load's store read (FIG-3683).
    ExecutionEnvLoad(ExecutionEnvLoadExecution),
    OwnedRunner(Box<dyn RuntimeEffectLocalRunner + Send + 'static>),
}

/// The store a [`LoadExecutionEnv`](RuntimeEffectCommand::LoadExecutionEnv)
/// step reads, and whose environment it is, for the refusal's message.
pub struct ExecutionEnvLoadExecution {
    store: Arc<dyn crate::ProcessExecutionEnvStore>,
    subject: String,
}

impl ExecutionEnvLoadExecution {
    async fn execute(
        self,
        envelope: RuntimeEffectEnvelope,
    ) -> Result<RuntimeEffectOutcome, RuntimeEffectControllerError> {
        let claim = crate::session::execution_claim_of(envelope.invocation.execution_scope())?;
        let RuntimeEffectCommand::LoadExecutionEnv { env } = envelope.command else {
            return Err(RuntimeEffectControllerError::new(
                crate::RuntimeErrorCode::RuntimeEffectLocalExecutorMismatch,
                format!(
                    "execution-environment executor cannot execute {} command directly",
                    envelope.command.kind().as_str()
                ),
            ));
        };
        self.store
            .acquire_process_execution_env(&claim, &env)
            .await
            .map_err(|error| {
                unresolved_execution_env(
                    &self.subject,
                    &env,
                    crate::runtime::ProcessExecutionEnvLoadError::Store(error.into()),
                )
            })?;
        match crate::runtime::load_process_execution_env(self.store.as_ref(), &env).await {
            Ok(_) => Ok(RuntimeEffectOutcome::LoadExecutionEnv { env }),
            Err(error) => Err(unresolved_execution_env(&self.subject, &env, error)),
        }
    }
}

/// The refusal of a recorded execution environment that did not load.
///
/// A child never invents an environment (ADR 0099 §3), so either way it runs
/// nothing; what differs is whose fact the failure is (FIG-3575). An
/// environment the store holds but this build cannot reconstruct — absent,
/// bound to other bytes, undecodable — is the request's, and every redrive
/// meets it again: it is refused with its request-version outcome, which the
/// step records. A store that did not answer is this attempt's: its error
/// settles by its own cause, and a live fault is marked retryable, so an
/// engine runs the step again instead of recording it (FIG-3683) and a redrive
/// under a healthy store loads the environment.
pub(crate) fn unresolved_execution_env(
    subject: &str,
    env: &crate::ProcessExecutionEnvRef,
    error: crate::runtime::ProcessExecutionEnvLoadError,
) -> RuntimeEffectControllerError {
    let refusal = crate::RuntimeErrorCode::ProcessExecutionEnvRefused;
    let context = format!("{subject} could not resolve its recorded execution environment `{env}`");
    match error {
        crate::runtime::ProcessExecutionEnvLoadError::Store(store) => {
            let mut settled = RuntimeEffectControllerError::from(store.into_turn_failure(refusal));
            settled.message = format!("{context}: {}", settled.message);
            if settled.turn_failure_cause() == crate::TurnFailureCause::LiveFault {
                settled.retryable_uncommitted_derivation()
            } else {
                settled
            }
        }
        unresolved => RuntimeEffectControllerError::new(
            refusal,
            format!("{context}: {unresolved}; a child never invents an environment (ADR 0099 §3)"),
        ),
    }
}

/// Everything the presentation boundary needs that is not on the journaled
/// command: the session's plugin chain, the facts a step may read, the
/// store retained artifacts are `put` into, the recorded
/// attachment-acceptance environment the materialization notices compute
/// under, and how long the settled call took — an observation the steps may
/// read, never part of the command's recorded identity.
pub struct PresentationLocalExecution {
    pub plugins: Arc<crate::plugin::PluginSession>,
    pub facts: Arc<crate::plugin::ToolPresentationFacts>,
    pub attachment_store: Arc<crate::RuntimeAttachmentStore>,
    pub attachment_acceptance: crate::provider::AttachmentCapabilitySnapshot,
    pub duration_ms: u64,
}

impl PresentationLocalExecution {
    async fn execute(
        self,
        envelope: RuntimeEffectEnvelope,
    ) -> Result<RuntimeEffectOutcome, RuntimeEffectControllerError> {
        let RuntimeEffectCommand::PresentToolResult {
            plan,
            call_id,
            tool_id,
            tool_name,
            render,
            args,
            output,
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
            owner: self.plugins.owner().clone(),
            call_id,
            tool_id,
            tool_name,
            render,
            args,
            output: *output,
            duration_ms: self.duration_ms,
            artifacts,
        };
        let presentation = Box::pin(self.plugins.present_tool_result(
            context,
            self.facts,
            &plan,
            &self.attachment_acceptance,
        ))
        .await?;
        Ok(RuntimeEffectOutcome::PresentToolResult {
            presentation: Box::new(presentation),
        })
    }
}

enum RuntimeEffectLocalExecutorState<'run> {
    Target(LocalTarget),
    Runner(Box<dyn RuntimeEffectLocalRunner + Send + 'run>),
}

/// Scoped local executor an [`ActorContext`](crate::ActorContext) group method
/// runs for one effect.
///
/// A controller runs it on a first execution and replays its own recorded
/// result on a redrive, so local provider/tool/checkpoint work always crosses
/// the `execute_effect` boundary.
pub struct RuntimeEffectLocalExecutor<'run> {
    state: RuntimeEffectLocalExecutorState<'run>,
    replay_trace: Option<super::RuntimeEffectReplayTrace>,
    /// Set when the effect belongs to a replayed command that must be served
    /// only from the journal (FIG-3587, FIG-3719).
    served_only: Option<ServedOnly>,
    /// What the shift that issued this effect lends its body
    /// ([`Self::issued_under`]).
    issued: crate::trace::StepIssue,
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
            served_only: None,
            issued: crate::trace::StepIssue::default(),
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
                    transferable: false,
                },
                clock,
            }),
            replay_trace: None,
            served_only: None,
            issued: crate::trace::StepIssue::default(),
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
            served_only: None,
            issued: crate::trace::StepIssue::default(),
        }
    }

    /// This is a wait-shape switch, not a live policy toggle: every attempt
    /// of one wait constructs the same value.
    pub fn with_turn_cancel_observation(mut self, observe_turn_cancel: bool) -> Self {
        if let RuntimeEffectLocalExecutorState::Target(LocalTarget::SleepOnly {
            controls, ..
        }) = &mut self.state
        {
            controls.observe_turn_cancel = observe_turn_cancel;
        }
        self
    }

    /// Adds the turn execution scope that effect-host implementors use to distinguish turn
    /// cancellation from an ordinary wait cancellation.
    pub fn with_turn_cancel_scope(mut self, scope: crate::ExecutionScope) -> Self {
        if let RuntimeEffectLocalExecutorState::Target(LocalTarget::SleepOnly {
            controls, ..
        }) = &mut self.state
        {
            controls.turn_cancel_scope = Some(scope);
        }
        self
    }

    pub fn with_process_turn_cancellation(
        mut self,
        turn_cancellation: ProcessTurnCancellation,
    ) -> Self {
        if let RuntimeEffectLocalExecutorState::Target(LocalTarget::Process(execution)) =
            &mut self.state
        {
            execution.turn_cancellation = Some(turn_cancellation);
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

    /// Binds process services and the admission capabilities every start requires.
    ///
    /// Engine validation and identity stamping run inside the start's recorded
    /// registration, using this registry. An empty registry refuses engine starts.
    /// Host session grants and session-turn validation use `host_start`; a host
    /// without those grants supplies an explicit default that refuses them.
    pub fn processes(
        registry: Arc<dyn ProcessRegistry>,
        process_work: Arc<dyn crate::ProcessWorkSubstrate>,
        process_engines: crate::ProcessEngineRegistry,
        host_start: crate::runtime::HostStartAdmission,
    ) -> Self {
        Self {
            state: RuntimeEffectLocalExecutorState::Target(LocalTarget::Process(Box::new(
                ProcessLocalExecution {
                    registry,
                    process_work,
                    process_env_store: None,
                    process_engines,
                    host_start: Box::new(host_start),
                    turn_cancellation: None,
                    attachments: None,
                    outcome_observer: None,
                },
            ))),
            replay_trace: None,
            served_only: None,
            issued: crate::trace::StepIssue::default(),
        }
    }

    /// Binds the definition executor for the journaled `PublishDefinition` /
    /// `GetDefinition` commands: the engine registry the commands resolve
    /// against and the parent's admitted claim their artifact closures pin.
    /// Every other process command still requires [`Self::processes`].
    pub fn definition_artifacts(
        engines: crate::ProcessEngineRegistry,
        claim: crate::ReferrerClaim,
    ) -> Self {
        Self {
            state: RuntimeEffectLocalExecutorState::Target(LocalTarget::Definition(
                ProcessDefinitionLocalExecution { engines, claim },
            )),
            replay_trace: None,
            served_only: None,
            issued: crate::trace::StepIssue::default(),
        }
    }

    /// Binds a turn-input store for the turn-acceptance effect (ADR 0069
    /// §6): the producer's write of its pending input and the session's
    /// wake, in one store transaction.
    pub fn turn_acceptance(store: Arc<dyn crate::TurnInputStore>) -> Self {
        Self {
            state: RuntimeEffectLocalExecutorState::Target(LocalTarget::TurnAcceptance(store)),
            replay_trace: None,
            served_only: None,
            issued: crate::trace::StepIssue::default(),
        }
    }

    /// Binds the session's plugin chain and artifact store for the journaled
    /// `PresentToolResult` boundary (ADR 0099 §6, FIG-3420): the ordered
    /// presentation steps run exactly once on the first execution and replay
    /// serves the recorded `ToolPresentation`.
    pub(crate) fn presentation(
        plugins: Arc<crate::plugin::PluginSession>,
        facts: Arc<crate::plugin::ToolPresentationFacts>,
        attachment_store: Arc<crate::RuntimeAttachmentStore>,
        attachment_acceptance: crate::provider::AttachmentCapabilitySnapshot,
        duration_ms: u64,
        plan: &super::PresentationBinding,
    ) -> Self {
        let refusal = plugins.validate_tool_presentation_plan(plan).err();
        let executor = Self {
            state: RuntimeEffectLocalExecutorState::Target(LocalTarget::Presentation(
                PresentationLocalExecution {
                    plugins,
                    facts,
                    attachment_store,
                    attachment_acceptance,
                    duration_ms,
                },
            )),
            replay_trace: None,
            served_only: None,
            issued: crate::trace::StepIssue::default(),
        };
        match refusal {
            None => executor,
            Some(error) => executor
                .serving_only_from_journal(error.into(), Arc::new(CommandJournalGuard::open())),
        }
    }

    /// Binds the store a recorded
    /// [`LoadExecutionEnv`](RuntimeEffectCommand::LoadExecutionEnv) step reads
    /// validates and holds on its first execution; replay resolves the
    /// recorded digest. `subject` names whose environment it is in a refusal.
    pub fn execution_env_load(
        store: Arc<dyn crate::ProcessExecutionEnvStore>,
        subject: impl Into<String>,
    ) -> Self {
        Self {
            state: RuntimeEffectLocalExecutorState::Target(LocalTarget::ExecutionEnvLoad(
                ExecutionEnvLoadExecution {
                    store,
                    subject: subject.into(),
                },
            )),
            replay_trace: None,
            served_only: None,
            issued: crate::trace::StepIssue::default(),
        }
    }

    pub fn triggers(store: Arc<dyn crate::TriggerStore>) -> Self {
        Self {
            state: RuntimeEffectLocalExecutorState::Target(LocalTarget::Trigger(
                TriggerLocalExecution { store },
            )),
            replay_trace: None,
            served_only: None,
            issued: crate::trace::StepIssue::default(),
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
            served_only: None,
            issued: crate::trace::StepIssue::default(),
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

    /// The body of an admitted direct call: it sends `admitted`'s exact body
    /// within the limit its pinned deadline leaves.
    pub fn direct(
        binding: crate::LlmProfileBinding,
        charge_safety: crate::ChargeSafetyPolicy,
        budgets: crate::ExecutionBudgets,
        admitted: AdmittedDirectSend,
        owner: crate::RuntimeOwner,
        tracing: crate::trace::TraceRuntime,
        replay_trace: Option<super::RuntimeEffectReplayTrace>,
    ) -> Self {
        Self {
            state: RuntimeEffectLocalExecutorState::Target(LocalTarget::OwnedRunner(Box::new(
                LocalDirectEffectRunner {
                    binding,
                    charge_safety,
                    bounds: lash_core_llm::core_internal::ModelCallBounds {
                        budgets,
                        enclosing: Some(admitted.limit),
                    },
                    body: admitted.body,
                    owner,
                    tracing,
                    live: None,
                },
            ))),
            replay_trace,
            served_only: None,
            issued: crate::trace::StepIssue::default(),
        }
    }

    pub(crate) fn prepared_tool_attempt(
        dispatch: Arc<crate::tool_dispatch::ToolDispatchContext<'run>>,
        tool_context: crate::ToolContext<'run>,
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
                    },
                ))),
                replay_trace,
                served_only: None,
                issued: crate::trace::StepIssue::default(),
            };
        }
        Self {
            state: RuntimeEffectLocalExecutorState::Runner(Box::new(
                LocalPreparedToolAttemptEffectRunner {
                    dispatch,
                    tool_context,
                },
            )),
            replay_trace,
            served_only: None,
            issued: crate::trace::StepIssue::default(),
        }
    }

    /// Exposes structured replay-comparison evidence to effect-host and conformance implementors,
    /// returning `None` when the runtime has no configured trace sink.
    /// Marks this executor's effect as served only from the journal: it
    /// belongs to a replayed command whose tool binding drifted, and running
    /// it live would reach the drifted tool (FIG-3587, FIG-3719).
    pub(crate) fn serving_only_from_journal(
        mut self,
        refusal: RuntimeEffectControllerError,
        guard: Arc<CommandJournalGuard>,
    ) -> Self {
        self.served_only = Some(ServedOnly { refusal, guard });
        self
    }

    /// The engine-neutral served-only contract (FIG-3719).
    ///
    /// An engine that serves this effect's recorded outcome never asks. An
    /// engine about to run this executor live — its journal holds no outcome
    /// for the effect's replay key — asks first, and on `Some` returns that
    /// refusal instead, running nothing and recording nothing: neither an
    /// admission, nor a failure row, nor a run result. Asking trips the command's
    /// guard, so the run stops on the refusal however the effect's caller
    /// shapes the error. `None` means the effect may run live.
    pub fn served_only_refusal(&self) -> Option<RuntimeEffectControllerError> {
        self.served_only.as_ref().map(ServedOnly::refuse)
    }

    /// Whether this effect is served only from the journal, without refusing
    /// it: for an engine that can ask its journal before it starts the
    /// effect, and one that must keep the refusal for later
    /// ([`ServedOnly::refuse`]).
    pub fn served_only(&self) -> Option<ServedOnly> {
        self.served_only.clone()
    }

    pub fn replay_validation_trace(&self) -> Option<&super::RuntimeEffectReplayTrace> {
        self.replay_trace.as_ref()
    }

    /// Run the effect's body in place, recording nothing: what a VM effect
    /// does, whose durability is its execution's snapshot (ADR 0132 §8).
    /// Boxed and erased, so a caller's future neither carries the body's size
    /// nor its type.
    pub(crate) fn run_in_place(
        self,
        envelope: RuntimeEffectEnvelope,
    ) -> std::pin::Pin<
        Box<
            dyn std::future::Future<
                    Output = Result<RuntimeEffectOutcome, RuntimeEffectControllerError>,
                > + Send
                + 'run,
        >,
    > {
        Box::pin(self.run_body(envelope, None))
    }

    async fn run_body(
        self,
        envelope: RuntimeEffectEnvelope,
        effect_attempt: Option<crate::EffectAttempt>,
    ) -> Result<RuntimeEffectOutcome, RuntimeEffectControllerError> {
        // This is the engine's journaled-step boundary: a substrate that
        // serves the step from its journal never reaches it. A command that
        // replays by re-execution is no recorded step, so its body gets no
        // live step and its shift's frontier stays where it is.
        let plugins = if envelope.command.replays_by_reexecution() {
            None
        } else {
            self.plugin_state_session()
        };
        let kind = envelope.command.kind();
        let address = envelope.invocation.address().clone();
        let mut issued = self.issued;
        let live = if envelope.command.replays_by_reexecution() {
            issued.unrecorded();
            None
        } else {
            Some(issued.begin(effect_attempt.as_ref()))
        };
        match self.state {
            RuntimeEffectLocalExecutorState::Runner(mut runner) => {
                if let Some(live) = live {
                    runner.bind_live_step(live);
                }
                record_plugin_state(
                    plugins,
                    kind,
                    address,
                    runner.execute(envelope, effect_attempt),
                )
                .await
            }
            RuntimeEffectLocalExecutorState::Target(LocalTarget::OwnedRunner(mut runner)) => {
                if let Some(live) = live {
                    runner.bind_live_step(live);
                }
                if !runner.uses_task_boundary(&envelope.command) {
                    return record_plugin_state(
                        plugins,
                        kind,
                        address,
                        runner.execute(envelope, effect_attempt),
                    )
                    .await;
                }
                let panic_call = match &envelope.command {
                    RuntimeEffectCommand::ToolAttempt { call, .. } => Some(call.clone()),
                    _ => None,
                };
                let task = crate::task::spawn(record_plugin_state(
                    plugins,
                    kind,
                    address,
                    runner.execute(envelope, effect_attempt),
                ));
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
            RuntimeEffectLocalExecutorState::Target(LocalTarget::Definition(_)) => {
                Err(RuntimeEffectControllerError::new(
                    crate::RuntimeErrorCode::RuntimeEffectLocalExecutorMismatch,
                    format!(
                        "process-definition executor cannot execute {} command directly",
                        envelope.command.kind().as_str()
                    ),
                ))
            }
            RuntimeEffectLocalExecutorState::Target(LocalTarget::Trigger(execution)) => {
                // A store-backed replay driver hands every opened command to
                // `execute`; a trigger command runs on its own target.
                let RuntimeEffectCommand::Trigger { command } = envelope.command else {
                    return Err(RuntimeEffectControllerError::new(
                        crate::RuntimeErrorCode::RuntimeEffectLocalExecutorMismatch,
                        format!(
                            "trigger executor cannot execute {} command directly",
                            envelope.command.kind().as_str()
                        ),
                    ));
                };
                let operation_id = envelope.invocation.effect_id().to_string();
                let result = execution.execute(&operation_id, *command).await?;
                Ok(RuntimeEffectOutcome::Trigger {
                    result: Box::new(result),
                })
            }
            RuntimeEffectLocalExecutorState::Target(LocalTarget::Presentation(execution)) => {
                record_plugin_state(plugins, kind, address, execution.execute(envelope)).await
            }
            RuntimeEffectLocalExecutorState::Target(LocalTarget::ExecutionEnvLoad(execution)) => {
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
                    .accept_pending_turn_input(*draft)
                    .await
                    .map_err(|err| {
                        let error =
                            lash_core_store::runtime_error::runtime_error_from_turn_input_admission(
                                err,
                            );
                        RuntimeEffectControllerError::new(
                            error.code,
                            format!("turn acceptance commit failed: {}", error.message),
                        )
                    })?;
                Ok(RuntimeEffectOutcome::AcceptTurnInput {
                    accepted: Box::new(accepted),
                })
            }
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
                    .execute(
                        RuntimeEffectEnvelope::new(
                            invocation,
                            RuntimeEffectCommand::Trigger {
                                command: Box::new(command),
                            },
                        ),
                        None,
                    )
                    .await
            }
            _ => Err(RuntimeEffectControllerError::new(
                crate::RuntimeErrorCode::RuntimeEffectLocalExecutorUnavailable,
                "no trigger executor is available for trigger command",
            )),
        }
    }
}

#[async_trait::async_trait]
impl RuntimeEffectLocalRunner for TestingRuntimeEffectLocalRunner<'_> {
    async fn execute(
        self: Box<Self>,
        envelope: RuntimeEffectEnvelope,
        _effect_attempt: Option<crate::EffectAttempt>,
    ) -> Result<RuntimeEffectOutcome, RuntimeEffectControllerError> {
        (self.run)(envelope).await
    }
}

/// The recorded outcome of one tool attempt a local runner executed.
fn tool_attempt_outcome(outcome: crate::ToolAttemptEffectOutcome) -> RuntimeEffectOutcome {
    RuntimeEffectOutcome::ToolAttempt {
        launch: Box::new(outcome.launch),
        triggers: outcome.triggers,
    }
}

#[async_trait::async_trait]
impl RuntimeEffectLocalRunner for LocalDirectEffectRunner {
    fn uses_task_boundary(&self, command: &RuntimeEffectCommand) -> bool {
        matches!(command, RuntimeEffectCommand::Direct { .. })
    }

    fn bind_live_step(&mut self, live: Arc<crate::trace::LiveStep>) {
        self.live = Some(live);
    }

    async fn execute(
        mut self: Box<Self>,
        envelope: RuntimeEffectEnvelope,
        _effect_attempt: Option<crate::EffectAttempt>,
    ) -> Result<RuntimeEffectOutcome, RuntimeEffectControllerError> {
        match envelope.command {
            RuntimeEffectCommand::Direct {
                request,
                usage_source: _,
            } => {
                // An unjournaled completion binds its recorded model here; a
                // refusal leaves the step unsealed, never a recorded result.
                let provider = self.binding.bind_for_unjournaled_call()?;
                let request = (*request).into_request(
                    crate::session_model::transport_stream_events(&provider, None),
                    None,
                );
                self.run_direct(&envelope.invocation, provider, request)
                    .await
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

/// A local executor that runs `runner`, an owned [`RuntimeEffectLocalRunner`].
///
/// The runtime's seam for effects whose runner it builds itself;
/// `core_internal` re-exports it and the `lash` facade does not.
pub fn owned_runner_executor(
    runner: Box<dyn RuntimeEffectLocalRunner + Send + 'static>,
    replay_trace: Option<super::RuntimeEffectReplayTrace>,
) -> RuntimeEffectLocalExecutor<'static> {
    RuntimeEffectLocalExecutor {
        state: RuntimeEffectLocalExecutorState::Target(LocalTarget::OwnedRunner(runner)),
        replay_trace,
        served_only: None,
        issued: crate::trace::StepIssue::default(),
    }
}

#[cfg(test)]
mod served_only_tests;

#[cfg(test)]
mod unresolved_execution_env_tests;

#[cfg(test)]
mod task_boundary_tests {
    use super::*;
    use crate::RuntimeEffectInvocation;
    use tokio::sync::oneshot;

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
            _effect_attempt: Option<crate::EffectAttempt>,
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
            served_only: None,
            issued: crate::trace::StepIssue::default(),
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
}
