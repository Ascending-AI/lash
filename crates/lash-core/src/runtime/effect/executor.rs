use std::collections::HashMap;
#[cfg(any(test, feature = "testing"))]
use std::pin::Pin;
use std::sync::Arc;
use std::time::Instant;

use tokio::sync::mpsc;
use tokio::sync::oneshot;
use tokio_util::sync::CancellationToken;

mod control;
mod controller_error;
mod scoped;
mod trigger;

pub use control::{
    AwaitEventKey, AwaitEventResolver, AwaitEventWaitIdentity, BoundaryReason, EffectHost,
    ExecutionScope, ExternalCompletionError, Resolution, ResolveOutcome, RuntimeEffectController,
    ScopedEffectController, SegmentProgress,
};
pub(crate) use control::{
    EffectTaskController, RuntimeEffectControllerHandle, drive_effect_controller_task,
};
pub use controller_error::RuntimeEffectControllerError;
pub use trigger::TriggerLocalExecution;

use crate::LlmRequest as CoreLlmRequest;
use crate::ProcessRecord;
use crate::ProcessRegistry;
use crate::provider::ProviderHandle;
use crate::runtime::{RuntimeStreamEvent, RuntimeTurnDriver};
use crate::sansio::LlmCallError;
use crate::{PluginError, RuntimeError};
#[cfg(test)]
use control::EffectControllerTaskRequest;
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

use super::await_events::AwaitEventRegistry;

// =============================================================================
// Local executor (per-effect borrowed runner state)
// =============================================================================

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

pub struct ProcessLocalExecution {
    pub registry: Arc<dyn ProcessRegistry>,
    pub process_work_driver: Option<crate::ProcessWorkDriver>,
}

impl ProcessLocalExecution {
    pub async fn execute(
        self,
        command: ProcessCommand,
    ) -> Result<ProcessEffectOutcome, RuntimeEffectControllerError> {
        let Self {
            registry,
            process_work_driver,
        } = self;
        match command {
            ProcessCommand::Start {
                registration,
                grant,
                execution_context: _,
            } => {
                let record =
                    InlineRuntimeEffectController::start_process(registry, registration, grant)
                        .await?;
                if let Some(driver) = process_work_driver.as_ref() {
                    driver.claim_and_run_pending("process_start").await?;
                }
                Ok(ProcessEffectOutcome::Start {
                    record: Box::new(record),
                })
            }
            ProcessCommand::List {
                session_scope,
                mode,
            } => {
                let entries = match mode {
                    crate::ProcessListMode::Live => {
                        registry.list_live_handle_grants(&session_scope).await?
                    }
                    crate::ProcessListMode::All => {
                        registry.list_handle_grants(&session_scope).await?
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
                    .transfer_handle_grants(&from_scope, &to_scope, &process_ids)
                    .await?;
                Ok(ProcessEffectOutcome::Transfer)
            }
            ProcessCommand::DeleteSession { session_id } => {
                let report = registry.delete_session_process_state(&session_id).await?;
                Ok(ProcessEffectOutcome::DeleteSession { report })
            }
            ProcessCommand::Await { process_id } => {
                let output = if let Some(driver) = process_work_driver.as_ref() {
                    driver.await_terminal(&process_id).await?
                } else {
                    crate::ProcessAwaiter::polling(registry)
                        .await_terminal(&process_id)
                        .await?
                };
                Ok(ProcessEffectOutcome::Await {
                    output: Box::new(output),
                })
            }
            ProcessCommand::Cancel { process_id, reason } => {
                let record = InlineRuntimeEffectController::request_process_cancel(
                    registry,
                    &process_id,
                    reason,
                )
                .await?;
                Ok(ProcessEffectOutcome::Cancel {
                    record: Box::new(record),
                })
            }
            ProcessCommand::Signal {
                process_id,
                request,
                ..
            } => {
                let result = registry.append_event(&process_id, request).await?;
                Ok(ProcessEffectOutcome::Signal {
                    event: Box::new(result.event),
                })
            }
        }
    }
}

pub(crate) struct TurnEffectStateUpdate {
    pub(crate) policy: crate::RuntimeSessionPolicy,
    pub(crate) llm_stream_summaries: HashMap<usize, crate::runtime::LlmStreamSummary>,
    pub(crate) next_llm_ordinal: usize,
    pub(crate) pending_queue_claims: Vec<crate::QueuedWorkClaim>,
    pub(crate) pending_turn_input_claims: Vec<crate::TurnInputClaim>,
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
}

struct LocalPreparedToolAttemptEffectRunner<'run> {
    dispatch: Arc<crate::tool_dispatch::ToolDispatchContext<'run>>,
    tool_context: crate::ToolContext<'run>,
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

#[cfg(any(test, feature = "testing"))]
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

#[cfg(any(test, feature = "testing"))]
struct TestingRuntimeEffectLocalRunner<'run> {
    run: Box<TestingRuntimeEffectLocalRunnerFn<'run>>,
}

enum RuntimeEffectLocalExecutorState<'run> {
    Unavailable,
    SleepOnly {
        cancellation: CancellationToken,
        clock: Arc<dyn crate::Clock>,
        observe_turn_cancel: bool,
        turn_cancel_scope: Option<crate::ExecutionScope>,
    },
    ExternalWaitOptions {
        cancellation: CancellationToken,
        deadline: Option<Instant>,
        clock: Arc<dyn crate::Clock>,
        observe_turn_cancel: bool,
        turn_cancel_scope: Option<crate::ExecutionScope>,
    },
    Process(ProcessLocalExecution),
    Trigger(TriggerLocalExecution),
    Runner(Box<dyn RuntimeEffectLocalRunner + Send + 'run>),
    OwnedRunner(Box<dyn RuntimeEffectLocalRunner + Send + 'static>),
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
    pub fn unavailable() -> Self {
        Self {
            state: RuntimeEffectLocalExecutorState::Unavailable,
            replay_trace: None,
        }
    }

    pub fn sleep(cancellation: CancellationToken) -> Self {
        Self::sleep_with_clock(cancellation, Arc::new(crate::SystemClock))
    }

    pub fn sleep_with_clock(cancellation: CancellationToken, clock: Arc<dyn crate::Clock>) -> Self {
        Self {
            state: RuntimeEffectLocalExecutorState::SleepOnly {
                cancellation,
                clock,
                observe_turn_cancel: true,
                turn_cancel_scope: None,
            },
            replay_trace: None,
        }
    }

    pub fn await_event(cancellation: CancellationToken, deadline: Option<Instant>) -> Self {
        Self::await_event_with_clock(cancellation, deadline, Arc::new(crate::SystemClock))
    }

    pub fn await_event_with_clock(
        cancellation: CancellationToken,
        deadline: Option<Instant>,
        clock: Arc<dyn crate::Clock>,
    ) -> Self {
        Self {
            state: RuntimeEffectLocalExecutorState::ExternalWaitOptions {
                cancellation,
                deadline,
                clock,
                observe_turn_cancel: true,
                turn_cancel_scope: None,
            },
            replay_trace: None,
        }
    }

    #[doc(hidden)]
    pub fn with_turn_cancel_observation(mut self, observe_turn_cancel: bool) -> Self {
        match &mut self.state {
            RuntimeEffectLocalExecutorState::SleepOnly {
                observe_turn_cancel: current,
                ..
            }
            | RuntimeEffectLocalExecutorState::ExternalWaitOptions {
                observe_turn_cancel: current,
                ..
            } => *current = observe_turn_cancel,
            _ => {}
        }
        self
    }

    pub fn with_turn_cancel_scope(mut self, scope: crate::ExecutionScope) -> Self {
        match &mut self.state {
            RuntimeEffectLocalExecutorState::SleepOnly {
                turn_cancel_scope, ..
            }
            | RuntimeEffectLocalExecutorState::ExternalWaitOptions {
                turn_cancel_scope, ..
            } => *turn_cancel_scope = Some(scope),
            _ => {}
        }
        self
    }

    pub fn processes(
        registry: Arc<dyn ProcessRegistry>,
        process_work_driver: Option<crate::ProcessWorkDriver>,
    ) -> Self {
        Self {
            state: RuntimeEffectLocalExecutorState::Process(ProcessLocalExecution {
                registry,
                process_work_driver,
            }),
            replay_trace: None,
        }
    }

    pub fn triggers(store: Arc<dyn crate::TriggerStore>) -> Self {
        Self {
            state: RuntimeEffectLocalExecutorState::Trigger(TriggerLocalExecution { store }),
            replay_trace: None,
        }
    }

    #[cfg(any(test, feature = "testing"))]
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
        let replay_trace = super::RuntimeEffectReplayTrace::gated(
            driver.host.core.tracing.trace_level,
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
            session_execution_lease: driver.session_execution_lease.clone(),
            runtime_lease_owner: driver.runtime_lease_owner.clone(),
            turn_phase_probe: driver.turn_phase_probe.clone(),
        };
        (
            RuntimeEffectLocalExecutor {
                state: RuntimeEffectLocalExecutorState::OwnedRunner(Box::new(
                    LocalTurnEffectRunner {
                        driver: owned_driver,
                        protocol_iteration: machine.protocol_iteration(),
                        messages: machine.message_sequence(),
                        event_tx,
                        cancellation,
                        update: Arc::clone(&update),
                    },
                )),
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
            state: RuntimeEffectLocalExecutorState::OwnedRunner(Box::new(
                LocalDirectEffectRunner {
                    provider,
                    attachment_store,
                },
            )),
            replay_trace,
        }
    }

    pub(crate) fn tool_batch(
        context: crate::RuntimeExecutionContext<'run>,
        child_trace_hooks: HashMap<String, crate::ToolChildExecutionTraceHook>,
    ) -> Self {
        let replay_trace = context.replay_validation_trace();
        if let Some(context) = context.to_static() {
            return Self {
                state: RuntimeEffectLocalExecutorState::OwnedRunner(Box::new(
                    LocalToolBatchEffectRunner {
                        context,
                        child_trace_hooks,
                    },
                )),
                replay_trace,
            };
        }
        Self {
            state: RuntimeEffectLocalExecutorState::Runner(Box::new(LocalToolBatchEffectRunner {
                context,
                child_trace_hooks,
            })),
            replay_trace,
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
                state: RuntimeEffectLocalExecutorState::OwnedRunner(Box::new(
                    LocalPreparedToolAttemptEffectRunner {
                        dispatch: Arc::new(dispatch),
                        tool_context,
                    },
                )),
                replay_trace,
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
        }
    }

    pub fn replay_validation_trace(&self) -> Option<&super::RuntimeEffectReplayTrace> {
        self.replay_trace.as_ref()
    }

    pub async fn execute(
        self,
        envelope: RuntimeEffectEnvelope,
    ) -> Result<RuntimeEffectOutcome, RuntimeEffectControllerError> {
        match self.state {
            RuntimeEffectLocalExecutorState::Runner(runner) => runner.execute(envelope).await,
            RuntimeEffectLocalExecutorState::OwnedRunner(runner) => {
                if !runner.uses_task_boundary(&envelope.command) {
                    return runner.execute(envelope).await;
                }
                let task = crate::task::spawn(
                    crate::runtime::process_worker::inherit_process_execution_permit(
                        runner.execute(envelope),
                    ),
                );
                let mut abort = AbortEffectTaskOnDrop::new(task.abort_handle());
                let result = task.await.map_err(|err| {
                    RuntimeEffectControllerError::new(
                        "runtime_effect_task_join",
                        format!("spawned local effect task failed: {err}"),
                    )
                })?;
                abort.disarm();
                result
            }
            RuntimeEffectLocalExecutorState::SleepOnly {
                cancellation,
                clock,
                ..
            } => execute_local_sleep(envelope, cancellation, clock.as_ref()).await,
            RuntimeEffectLocalExecutorState::ExternalWaitOptions { .. } => {
                Err(RuntimeEffectControllerError::new(
                    "runtime_effect_local_executor_mismatch",
                    format!(
                        "local await-event options cannot execute {} command directly",
                        envelope.command.kind().as_str()
                    ),
                ))
            }
            RuntimeEffectLocalExecutorState::Unavailable => Err(RuntimeEffectControllerError::new(
                "runtime_effect_local_executor_unavailable",
                format!(
                    "no local executor is available for {}",
                    envelope.command.kind().as_str()
                ),
            )),
            RuntimeEffectLocalExecutorState::Process(_) => Err(RuntimeEffectControllerError::new(
                "runtime_effect_local_executor_mismatch",
                format!(
                    "process executor cannot execute {} command directly",
                    envelope.command.kind().as_str()
                ),
            )),
            RuntimeEffectLocalExecutorState::Trigger(_) => Err(RuntimeEffectControllerError::new(
                "runtime_effect_local_executor_mismatch",
                format!(
                    "trigger executor cannot execute {} command directly",
                    envelope.command.kind().as_str()
                ),
            )),
        }
    }

    pub fn into_process(self) -> Result<ProcessLocalExecution, RuntimeEffectControllerError> {
        match self.state {
            RuntimeEffectLocalExecutorState::Process(execution) => Ok(execution),
            _ => Err(RuntimeEffectControllerError::new(
                "runtime_effect_local_executor_unavailable",
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
                        state: RuntimeEffectLocalExecutorState::OwnedRunner(Box::new(
                            RemoteEffectRunner { requests },
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
            RuntimeEffectLocalExecutorState::OwnedRunner(runner) => {
                let (requests, request_rx) = mpsc::unbounded_channel();
                (
                    RuntimeEffectLocalExecutor {
                        state: RuntimeEffectLocalExecutorState::OwnedRunner(Box::new(
                            RemoteEffectRunner { requests },
                        )),
                        replay_trace: replay_trace.clone(),
                    },
                    Some((
                        RuntimeEffectLocalExecutor {
                            state: RuntimeEffectLocalExecutorState::OwnedRunner(runner),
                            replay_trace,
                        },
                        request_rx,
                    )),
                )
            }
            state => (
                RuntimeEffectLocalExecutor {
                    state: match state {
                        RuntimeEffectLocalExecutorState::Unavailable => {
                            RuntimeEffectLocalExecutorState::Unavailable
                        }
                        RuntimeEffectLocalExecutorState::SleepOnly {
                            cancellation,
                            clock,
                            observe_turn_cancel,
                            turn_cancel_scope,
                        } => RuntimeEffectLocalExecutorState::SleepOnly {
                            cancellation,
                            clock,
                            observe_turn_cancel,
                            turn_cancel_scope,
                        },
                        RuntimeEffectLocalExecutorState::ExternalWaitOptions {
                            cancellation,
                            deadline,
                            clock,
                            observe_turn_cancel,
                            turn_cancel_scope,
                        } => RuntimeEffectLocalExecutorState::ExternalWaitOptions {
                            cancellation,
                            deadline,
                            clock,
                            observe_turn_cancel,
                            turn_cancel_scope,
                        },
                        RuntimeEffectLocalExecutorState::Process(execution) => {
                            RuntimeEffectLocalExecutorState::Process(execution)
                        }
                        RuntimeEffectLocalExecutorState::Trigger(execution) => {
                            RuntimeEffectLocalExecutorState::Trigger(execution)
                        }
                        RuntimeEffectLocalExecutorState::Runner(_)
                        | RuntimeEffectLocalExecutorState::OwnedRunner(_) => {
                            unreachable!("runner states are handled above")
                        }
                    },
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
        } = envelope;
        match command {
            RuntimeEffectCommand::Trigger { command } => {
                self.execute_trigger(invocation, *command).await
            }
            command => {
                self.execute(RuntimeEffectEnvelope {
                    invocation,
                    command,
                })
                .await
            }
        }
    }

    pub fn into_trigger(self) -> Result<TriggerLocalExecution, RuntimeEffectControllerError> {
        match self.state {
            RuntimeEffectLocalExecutorState::Trigger(execution) => Ok(execution),
            _ => Err(RuntimeEffectControllerError::new(
                "runtime_effect_local_executor_unavailable",
                "no trigger executor is available for trigger command",
            )),
        }
    }

    pub async fn execute_trigger(
        self,
        invocation: crate::RuntimeInvocation,
        command: crate::TriggerCommand,
    ) -> Result<RuntimeEffectOutcome, RuntimeEffectControllerError> {
        let operation_id = invocation
            .effect_id()
            .ok_or_else(|| {
                RuntimeEffectControllerError::new(
                    "runtime_effect_invocation_subject",
                    "trigger effect requires an effect id",
                )
            })?
            .to_string();
        match self.state {
            RuntimeEffectLocalExecutorState::Trigger(execution) => {
                let result = execution.execute(&operation_id, command).await?;
                Ok(RuntimeEffectOutcome::Trigger {
                    result: Box::new(result),
                })
            }
            RuntimeEffectLocalExecutorState::Runner(runner)
            | RuntimeEffectLocalExecutorState::OwnedRunner(runner) => {
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
                "runtime_effect_local_executor_unavailable",
                "no trigger executor is available for trigger command",
            )),
        }
    }

    pub fn into_await_event_options(
        self,
    ) -> Result<RuntimeAwaitEventOptions, RuntimeEffectControllerError> {
        match self.state {
            RuntimeEffectLocalExecutorState::ExternalWaitOptions {
                cancellation,
                deadline,
                clock,
                observe_turn_cancel,
                turn_cancel_scope,
            } => Ok(RuntimeAwaitEventOptions {
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

    pub fn into_sleep_options(self) -> RuntimeSleepOptions {
        match self.state {
            RuntimeEffectLocalExecutorState::SleepOnly {
                cancellation,
                observe_turn_cancel,
                turn_cancel_scope,
                ..
            } => RuntimeSleepOptions {
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

#[cfg(any(test, feature = "testing"))]
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
                let outcome = self
                    .context
                    .execute_prepared_tool_batch_launches(
                        batch,
                        envelope.invocation,
                        self.child_trace_hooks,
                    )
                    .await?;
                Ok(RuntimeEffectOutcome::ToolBatch {
                    launches: outcome.launches,
                    triggers: outcome.triggers,
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
                ))
                .await?;
                Ok(RuntimeEffectOutcome::ToolAttempt {
                    launch: Box::new(outcome.launch),
                    triggers: outcome.triggers,
                })
            }
            command => Err(RuntimeEffectControllerError::new(
                "runtime_effect_local_executor_mismatch",
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
                "runtime_effect_local_executor_mismatch",
                "prepared tool attempt executor requires a tool_attempt command",
            ));
        };
        let mut dispatch = (*self.dispatch).clone();
        dispatch.parent_invocation = Some(envelope.invocation.clone());
        dispatch.trigger_outcomes = crate::tool_dispatch::ToolTriggerOutcomeBuffer::default();
        let dispatch = Arc::new(dispatch);
        let tool_context = self
            .tool_context
            .with_attempt_dispatch(Arc::clone(&dispatch), envelope.invocation);
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
            RuntimeEffectCommand::ToolBatch { batch } => runner
                .driver
                .run_tool_batch(
                    batch,
                    envelope.invocation,
                    &runner.event_tx,
                    &runner.cancellation,
                )
                .await
                .map(|outcome| RuntimeEffectOutcome::ToolBatch {
                    launches: outcome.launches,
                    triggers: outcome.triggers,
                }),
            RuntimeEffectCommand::ExecCode { language, code } => {
                Ok(RuntimeEffectOutcome::ExecCode {
                    result: Box::new(
                        runner
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
                            .await,
                    ),
                })
            }
            RuntimeEffectCommand::Checkpoint { checkpoint } => {
                Ok(RuntimeEffectOutcome::Checkpoint {
                    result: runner
                        .driver
                        .run_checkpoint(
                            runner.messages.clone(),
                            runner.protocol_iteration,
                            checkpoint,
                            &runner.event_tx,
                        )
                        .await
                        .map_err(RuntimeEffectControllerError::from),
                })
            }
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
                "runtime_effect_local_executor_mismatch",
                format!(
                    "local turn executor cannot execute {} command",
                    command.kind().as_str()
                ),
            )),
        };
        *runner.update.lock().expect("turn effect state update lock") =
            Some(TurnEffectStateUpdate {
                policy: runner.driver.policy,
                llm_stream_summaries: runner.driver.llm_stream_summaries,
                next_llm_ordinal: runner.driver.next_llm_ordinal,
                pending_queue_claims: runner.driver.pending_queue_claims,
                pending_turn_input_claims: runner.driver.pending_turn_input_claims,
                pending_checkpoint_turn_input_claim: runner
                    .driver
                    .pending_checkpoint_turn_input_claim,
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
                "runtime_effect_local_executor_mismatch",
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
                    "runtime_effect_local_task_closed",
                    "spawned effect local executor is no longer running",
                )
            })?;
        response_rx.await.map_err(|_| {
            RuntimeEffectControllerError::new(
                "runtime_effect_local_task_closed",
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
            "runtime_effect_local_executor_mismatch",
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
            "runtime_effect_sleep_cancelled",
            "runtime effect sleep was cancelled",
        )),
        _ = &mut sleep => Ok(()),
    }
}

// =============================================================================
// Default in-process effect controller
// =============================================================================

/// Default in-process effect controller.
///
/// The inline controller executes local runners in process and provides
/// in-memory await-event resolution. It does not make in-flight effects crash
/// durable; workflow adapters provide that by recording outcomes in history.
#[derive(Clone)]
pub struct InlineRuntimeEffectController {
    await_events: Arc<AwaitEventRegistry>,
    allow_process_lifetime_completion_keys: bool,
}

impl Default for InlineRuntimeEffectController {
    fn default() -> Self {
        Self {
            await_events: Arc::new(AwaitEventRegistry::new()),
            allow_process_lifetime_completion_keys: false,
        }
    }
}

#[async_trait::async_trait]
impl AwaitEventResolver for InlineRuntimeEffectController {
    fn allows_process_lifetime_completion_keys(&self) -> bool {
        self.allow_process_lifetime_completion_keys
    }

    async fn await_event_key(
        &self,
        scope: &ExecutionScope,
        wait: AwaitEventWaitIdentity,
    ) -> Result<AwaitEventKey, RuntimeError> {
        self.await_events.key_for(scope, wait)
    }

    async fn resolve_await_event(
        &self,
        key: &AwaitEventKey,
        resolution: Resolution,
    ) -> Result<ResolveOutcome, RuntimeError> {
        self.await_events.resolve(key, resolution)
    }

    async fn peek_await_event(
        &self,
        key: &AwaitEventKey,
    ) -> Result<Option<Resolution>, RuntimeError> {
        self.await_events.peek_resolution(key)
    }

    async fn await_await_event(
        &self,
        key: &AwaitEventKey,
        cancel: CancellationToken,
        deadline: Option<Instant>,
    ) -> Result<Resolution, RuntimeError> {
        self.await_events
            .await_resolution(key, cancel, deadline, &crate::SystemClock)
            .await
    }

    async fn revoke_await_events_for_session(&self, session_id: &str) -> Result<(), RuntimeError> {
        self.await_events.revoke_session(session_id)
    }

    async fn cancel_await_events_for_session(&self, session_id: &str) -> Result<(), RuntimeError> {
        self.await_events.cancel_session(session_id)
    }
}

#[async_trait::async_trait]
impl RuntimeEffectController for InlineRuntimeEffectController {
    async fn execute_effect(
        &self,
        envelope: RuntimeEffectEnvelope,
        local_executor: RuntimeEffectLocalExecutor<'_>,
    ) -> Result<RuntimeEffectOutcome, RuntimeEffectControllerError> {
        match envelope.command {
            RuntimeEffectCommand::PeekAwaitEvent { key } => {
                let resolution = self
                    .await_events
                    .peek_resolution(&key)
                    .map_err(RuntimeEffectControllerError::from)?;
                Ok(RuntimeEffectOutcome::PeekAwaitEvent { resolution })
            }
            RuntimeEffectCommand::AwaitEvent { key } => {
                let RuntimeAwaitEventOptions {
                    cancellation,
                    deadline,
                    clock,
                    ..
                } = local_executor.into_await_event_options()?;
                let resolution = self
                    .await_events
                    .await_resolution(&key, cancellation, deadline, clock.as_ref())
                    .await
                    .map_err(RuntimeEffectControllerError::from)?;
                Ok(RuntimeEffectOutcome::AwaitEvent { resolution })
            }
            RuntimeEffectCommand::Process { command } => {
                let execution = local_executor.into_process()?;
                if matches!(command.as_ref(), ProcessCommand::Await { .. }) {
                    let result = execution.execute(*command).await?;
                    return Ok(RuntimeEffectOutcome::Process { result });
                }
                let result = crate::task::spawn(
                    crate::runtime::process_worker::inherit_process_execution_permit(async move {
                        execution.execute(*command).await
                    }),
                )
                .await
                .map_err(|err| {
                    RuntimeEffectControllerError::new(
                        "runtime_effect_process_task_join",
                        format!("inline process effect task failed: {err}"),
                    )
                })??;
                Ok(RuntimeEffectOutcome::Process { result })
            }
            RuntimeEffectCommand::Trigger { command } => {
                local_executor
                    .execute_trigger(envelope.invocation, *command)
                    .await
            }
            _ => local_executor.execute(envelope).await,
        }
    }
}

impl InlineRuntimeEffectController {
    /// Opt into externally routable keys that remain valid only while this
    /// controller's process and owned registry remain alive.
    pub fn allow_process_lifetime_completion_keys(mut self) -> Self {
        self.allow_process_lifetime_completion_keys = true;
        self
    }
    /// Register the process (and any handle grant) into the durable registry.
    ///
    /// The inline controller no longer runs the process here: the registry's
    /// non-terminal row *is* the durable work queue, and the host-owned
    /// [`ProcessWorkDriver`](crate::ProcessWorkDriver) is the sole executor.
    /// Registering the row is all this path does; the control seam drives the
    /// host driver after a successful start.
    pub(crate) async fn start_process(
        registry: Arc<dyn crate::ProcessRegistry>,
        registration: crate::ProcessRegistration,
        grant: Option<crate::ProcessStartGrant>,
    ) -> Result<ProcessRecord, PluginError> {
        let registration_for_record = registration.clone();
        let record = registry.register_process(registration_for_record).await?;
        if let Some(grant) = grant {
            registry
                .grant_handle(&grant.session_scope, &registration.id, grant.descriptor)
                .await?;
        }
        Ok(record)
    }

    pub(crate) async fn request_process_cancel(
        registry: Arc<dyn crate::ProcessRegistry>,
        process_id: &str,
        reason: Option<String>,
    ) -> Result<ProcessRecord, PluginError> {
        // Cancellation is a durable signal: the cancel event is what the
        // runner-run process observes, so the inline controller appends it and
        // no longer tracks an in-process cancellation token.
        registry
            .append_event(
                process_id,
                crate::ProcessEventAppendRequest::cancel_requested(process_id, reason.clone()),
            )
            .await?;
        registry
            .get_process(process_id)
            .await
            .ok_or_else(|| PluginError::Session(format!("unknown process `{process_id}`")))
    }
}

impl std::fmt::Debug for InlineRuntimeEffectController {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("InlineRuntimeEffectController").finish()
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
            state: RuntimeEffectLocalExecutorState::OwnedRunner(Box::new(TaskIdentityRunner {
                observed: observed_tx,
            })),
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
