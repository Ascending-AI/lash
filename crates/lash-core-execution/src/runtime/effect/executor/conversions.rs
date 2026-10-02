//! What a substrate takes out of a local executor to answer a command its
//! own way, and what the issuing drive lends the executor.

use super::*;

impl<'run> RuntimeEffectLocalExecutor<'run> {
    /// Extracts the process outcome for effect-host implementors while executing or replaying a
    /// runtime effect.
    pub fn into_process(mut self) -> Result<ProcessLocalExecution, RuntimeEffectControllerError> {
        // The substrate records this command its own way: no recorded body
        // of this executor's.
        self.issued.unrecorded();
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

    /// Extracts the definition executor for the journaled `PublishDefinition`
    /// / `GetDefinition` commands.
    pub fn into_definition_execution(
        mut self,
    ) -> Result<ProcessDefinitionLocalExecution, RuntimeEffectControllerError> {
        // The substrate records this command its own way: no recorded body
        // of this executor's.
        self.issued.unrecorded();
        match self.state {
            RuntimeEffectLocalExecutorState::Target(LocalTarget::Definition(execution)) => {
                Ok(execution)
            }
            _ => Err(RuntimeEffectControllerError::new(
                crate::RuntimeErrorCode::RuntimeEffectLocalExecutorUnavailable,
                "no definition executor is available for the publish/get-definition command",
            )),
        }
    }

    /// Extracts the trigger outcome for effect-host implementors while executing or replaying a
    /// runtime effect.
    pub fn into_trigger(mut self) -> Result<TriggerLocalExecution, RuntimeEffectControllerError> {
        // The substrate records this command its own way: no recorded body
        // of this executor's.
        self.issued.unrecorded();
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

    /// Extracts the await event options outcome for effect-host implementors while executing or
    /// replaying a runtime effect.
    pub fn into_await_event_options(
        mut self,
    ) -> Result<RuntimeAwaitEventOptions, RuntimeEffectControllerError> {
        // The substrate answers the wait itself: no recorded body.
        self.issued.unrecorded();
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
    pub fn into_sleep_options(mut self) -> RuntimeSleepOptions {
        // The substrate answers the timer itself: no recorded body.
        self.issued.unrecorded();
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

    /// Lends this effect's body the standing of the drive that issued it.
    pub(crate) fn issued_under(
        mut self,
        frontier: crate::trace::JournalFrontier,
        attempt: Option<lash_trace::AttemptObservation>,
    ) -> Self {
        self.issued = crate::trace::StepIssue::new(frontier, attempt);
        self
    }

    /// What the issuing drive lent this effect: a substrate that records a
    /// step of its own for the effect (a wait it installs, a timer it starts)
    /// begins that step's live body from it.
    pub fn step_issue(&self) -> &crate::trace::StepIssue {
        &self.issued
    }
}
