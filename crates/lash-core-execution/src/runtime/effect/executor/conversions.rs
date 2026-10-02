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

    /// A [`testing`](Self::testing) executor whose body is handed the live
    /// step the engine began for it, so a law can observe from inside a
    /// recorded body. Hidden from the published documentation like
    /// `testing`: it is not an integrator seam.
    #[doc(hidden)]
    pub fn testing_in_step<F, Fut>(run: F) -> Self
    where
        F: FnOnce(RuntimeEffectEnvelope, Arc<crate::trace::LiveStep>) -> Fut + Send + 'run,
        Fut: Future<Output = Result<RuntimeEffectOutcome, RuntimeEffectControllerError>>
            + Send
            + 'run,
    {
        Self {
            state: RuntimeEffectLocalExecutorState::Runner(Box::new(LiveStepRunner {
                run: Box::new(move |envelope, live| Box::pin(run(envelope, live))),
                live: None,
            })),
            replay_trace: None,
            served_only: None,
            issued: crate::trace::StepIssue::default(),
        }
    }
}

type LiveStepRunnerFn<'run> = dyn FnOnce(
        RuntimeEffectEnvelope,
        Arc<crate::trace::LiveStep>,
    ) -> Pin<
        Box<
            dyn Future<Output = Result<RuntimeEffectOutcome, RuntimeEffectControllerError>>
                + Send
                + 'run,
        >,
    > + Send
    + 'run;

struct LiveStepRunner<'run> {
    run: Box<LiveStepRunnerFn<'run>>,
    live: Option<Arc<crate::trace::LiveStep>>,
}

#[async_trait::async_trait]
impl RuntimeEffectLocalRunner for LiveStepRunner<'_> {
    fn bind_live_step(&mut self, live: Arc<crate::trace::LiveStep>) {
        self.live = Some(live);
    }

    async fn execute(
        self: Box<Self>,
        envelope: RuntimeEffectEnvelope,
        _usage_run: Option<crate::UsageRun>,
    ) -> Result<RuntimeEffectOutcome, RuntimeEffectControllerError> {
        let Some(live) = self.live else {
            return Err(RuntimeEffectControllerError::new(
                crate::RuntimeErrorCode::RuntimeEffectLocalExecutorMismatch,
                "a live-step executor runs only a recorded step's body",
            ));
        };
        (self.run)(envelope, live).await
    }
}
