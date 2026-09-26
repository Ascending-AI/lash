use super::*;
use tracing::Instrument as _;

async fn await_process_terminal(
    process_work: &dyn crate::ProcessWorkSubstrate,
    process_id: &crate::ProcessId,
) -> Result<crate::ProcessAwaitOutput, crate::PluginError> {
    loop {
        match process_work.await_process_terminal(process_id).await? {
            crate::ProcessTerminalWait::Terminal(output) => return Ok(output),
            crate::ProcessTerminalWait::Reattach => continue,
        }
    }
}

/// The durable-wait resolution a process terminal delivers to every wait armed
/// on it.
///
/// A terminal is a *fact*, never an error of the wait: a failed or cancelled
/// process resolves its waiters successfully, carrying the whole
/// [`ProcessAwaitOutput`](crate::ProcessAwaitOutput) as the resolution payload.
/// Only an unobservable terminal is an error resolution.
///
/// That payload is the journaled fact, not the value a cell sees. The await
/// site converts it with
/// [`ProcessAwaitOutput::into_tool_output`](crate::ProcessAwaitOutput::into_tool_output)
/// — the same conversion the inline await path performs — so
/// `await processes.await({ handle })` answers exactly what `await handle`
/// answers, in every terminal state. See
/// `crate::tool_result::tool_output_from_completion_resolution`.
pub(crate) fn process_terminal_resolution(output: crate::ProcessAwaitOutput) -> Resolution {
    match serde_json::to_value(&output) {
        Ok(value) => Resolution::Ok(value),
        Err(error) => Resolution::Err(crate::runtime::ExternalCompletionError {
            code: crate::TurnFailureCode::from_wire("process_terminal_encode").into(),
            message: error.to_string(),
            raw: None,
        }),
    }
}

impl ProcessLocalExecution {
    pub async fn execute(
        self,
        command: ProcessCommand,
    ) -> Result<ProcessEffectOutcome, RuntimeEffectControllerError> {
        let Self {
            registry,
            process_work,
            process_env_store,
            process_engines,
            turn_cancellation,
            effect_controller,
            outcome_observer,
        } = self;
        let outcome = match command {
            ProcessCommand::Start {
                registration,
                observers,
                env_spec,
                execution_context: _,
            } => {
                // Registering the row is the whole start: the registry's
                // non-terminal row is the durable work queue, and the
                // host-owned process-work substrate is its sole executor. A
                // runtime start derives its key from its admitted operation,
                // and a host start from its caller or its admitted scope
                // (ADR 0107).
                let started = crate::runtime::register_process_start(
                    &crate::runtime::ProcessStartStores {
                        registry: registry.as_ref(),
                        env_store: process_env_store.as_ref(),
                        engines: process_engines.as_ref(),
                        engines_required: false,
                        executor: "process start on the local executor",
                    },
                    registration,
                    &observers,
                    env_spec.as_ref(),
                )
                .await?;
                let realization = started.realization();
                let record = started.record;
                // The poke is advisory. Registration already committed the
                // durable row, and the row is the work queue: the native
                // worker's idle dispatcher rescans pending rows on the
                // worker-sweep cadence (`WorkerSweepPolicy::rescan_interval`),
                // so the row runs whether or not this nudge lands. Turning a
                // failed nudge into a start error would tell the caller the
                // child does not exist while it is queued to run, and the
                // retry that follows does the work twice.
                if let Err(error) = process_work.admit_pending_processes("process_start").await {
                    tracing::warn!(
                        process_id = %record.id,
                        %error,
                        "process start registered; advisory worker poke failed, the recovery sweep owns the run"
                    );
                }
                Ok((
                    ProcessEffectOutcome::Start {
                        record: Box::new(record),
                    },
                    realization,
                ))
            }
            ProcessCommand::List {
                session_scope,
                mode,
            } => {
                let entries = match mode {
                    crate::ProcessListMode::Live => {
                        registry
                            .list_live_observed_by(&session_scope.session_id)
                            .await?
                    }
                    crate::ProcessListMode::All => {
                        registry
                            .list_observed_by(
                                &session_scope.session_id,
                                &crate::ProcessListFilter {
                                    status: crate::ProcessStatusFilter::Any,
                                    ..Default::default()
                                },
                            )
                            .await?
                    }
                };
                Ok((
                    ProcessEffectOutcome::List { entries },
                    crate::StoreRealization::Realized,
                ))
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
                        crate::ProcessObserverBy::host("runtime-effect-transfer"),
                    )
                    .await?;
                Ok((
                    ProcessEffectOutcome::Transfer,
                    crate::StoreRealization::Realized,
                ))
            }
            ProcessCommand::DeleteSession { session_id } => {
                // A session's deletion runs this after its close, the point
                // of no return (FIG-3600 S7, Q10): a retryable registry fault
                // is this attempt's, never the step's recorded outcome, so a
                // retried deletion runs it again instead of replaying the
                // failure, as the Restate process step does.
                let report = registry
                    .delete_session_process_state(&session_id)
                    .await
                    .map_err(|error| {
                        let retryable = error.is_retryable();
                        let fault = RuntimeEffectControllerError::from(error);
                        if retryable {
                            fault.retryable_uncommitted_derivation()
                        } else {
                            fault
                        }
                    })?;
                Ok((
                    ProcessEffectOutcome::DeleteSession { report },
                    crate::StoreRealization::Realized,
                ))
            }
            ProcessCommand::Await { process_id } => {
                let await_terminal = || await_process_terminal(process_work.as_ref(), &process_id);
                let output = if let Some(turn_cancellation) = turn_cancellation {
                    tokio::select! {
                        biased;
                        output = await_terminal() => output?,
                        _ = turn_cancellation.cancellation.cancelled() => {
                            #[expect(clippy::expect_used, reason = "execution scopes are plain string identities")]
                            registry
                                .request_process_cancel(
                                    &process_id,
                                    crate::CancelOrigin::TurnStopped,
                                    serde_json::to_string(&turn_cancellation.scope)
                                        .expect("execution scopes contain only serializable identities"),
                                    None,
                                )
                                .await?;
                            await_terminal().await?
                        }
                    }
                } else {
                    await_terminal().await?
                };
                Ok((
                    ProcessEffectOutcome::Await {
                        output: Box::new(output),
                    },
                    crate::StoreRealization::Realized,
                ))
            }
            ProcessCommand::AttachTerminal { process_id, key } => {
                // The in-process boundary has no separate invocation to hand
                // the wait to, so it arms a task: await the terminal, then
                // resolve the key through the same resolver the parked turn
                // awaits on. The task is deliberately fire-and-forget — the
                // arming command must return so the turn can park — and it is
                // deliberately not the durability story. Durability is the
                // journaled arming itself: a crash loses the task, the turn is
                // re-driven, the arming replays, and a new task is armed
                // against a wait that is still open. Resolution is idempotent,
                // so an arming that races a terminal it already missed resolves
                // immediately and a duplicate resolve reports
                // `AlreadyResolved`.
                let effect_controller = effect_controller.clone().ok_or_else(|| {
                    RuntimeEffectControllerError::foreign(
                        "process_attach_resolver_unavailable",
                        crate::TurnFailureCause::Outcome,
                        "arming a process terminal needs the effect controller that owns the wait",
                    )
                })?;
                let process_work = Arc::clone(&process_work);
                #[allow(
                    clippy::disallowed_methods,
                    reason = "the lint protects the caller's tracing context, which this task carries explicitly through the `instrument` below"
                )]
                tokio::spawn(
                    async move {
                        let resolution = match await_process_terminal(
                            process_work.as_ref(),
                            &process_id,
                        )
                        .await
                        {
                            Ok(output) => process_terminal_resolution(output),
                            Err(error) => {
                                Resolution::Err(crate::runtime::ExternalCompletionError {
                                    code: crate::TurnFailureCode::from_wire(
                                        "process_terminal_unobservable",
                                    )
                                    .into(),
                                    message: error.to_string(),
                                    raw: None,
                                })
                            }
                        };
                        if let Err(error) = effect_controller
                            .resolve_await_event(&key, resolution)
                            .await
                        {
                            tracing::warn!(
                                process_id = %process_id,
                                key_id = %key.key_id,
                                "armed process terminal could not resolve its durable wait: {error}"
                            );
                        }
                    }
                    .instrument(tracing::Span::current()),
                );
                Ok((
                    ProcessEffectOutcome::AttachTerminal,
                    crate::StoreRealization::Realized,
                ))
            }
            ProcessCommand::Cancel {
                process_id,
                origin,
                requester,
                attribution,
            } => {
                // Reports whether this call recorded the request or found the
                // same cancellation already recorded (FIG-3070).
                let (record, realization) = registry
                    .request_process_cancel_reporting_realization(
                        &process_id,
                        origin,
                        requester,
                        attribution,
                    )
                    .await?;
                Ok((
                    ProcessEffectOutcome::Cancel {
                        record: Box::new(record),
                    },
                    realization,
                ))
            }
            ProcessCommand::CancelRefused { refusal, .. } => Ok((
                ProcessEffectOutcome::CancelRefused { refusal },
                crate::StoreRealization::Realized,
            )),
            ProcessCommand::Signal {
                process_id,
                signal_name,
                request,
                ..
            } => {
                let effect_controller = effect_controller.ok_or_else(|| {
                    RuntimeEffectControllerError::new(
                        crate::RuntimeErrorCode::RuntimeEffectLocalExecutorUnavailable,
                        "local process signal execution requires its effect controller",
                    )
                })?;
                let result = registry.append_event(&process_id, request).await?;
                let realization = result.realization;
                let waiting_ordinal =
                    registry
                        .get_process(&process_id)
                        .await?
                        .and_then(|record| match record.wait {
                            Some(crate::WaitState {
                                kind:
                                    crate::WaitKind::Signal {
                                        name,
                                        event_type,
                                        ordinal,
                                        ..
                                    },
                                ..
                            }) if name == signal_name && event_type == result.event.event_type => {
                                Some(ordinal)
                            }
                            _ => None,
                        });
                let ordinal = match waiting_ordinal {
                    Some(ordinal) => ordinal,
                    None => {
                        registry
                            .count_events_through(
                                &process_id,
                                result.event.event_type.as_str(),
                                result.event.sequence,
                            )
                            .await?
                    }
                };
                if ordinal > 0 {
                    let key = effect_controller
                        .await_event_key(
                            &crate::ExecutionScope::process(&process_id),
                            crate::AwaitEventWaitIdentity::process_signal(
                                &process_id,
                                &signal_name,
                                ordinal,
                            ),
                        )
                        .await?;
                    let _ = effect_controller
                        .resolve_await_event(
                            &key,
                            crate::Resolution::Ok(result.event.payload.clone()),
                        )
                        .await?;
                }
                Ok((
                    ProcessEffectOutcome::Signal {
                        event: Box::new(result.event),
                    },
                    realization,
                ))
            }
            ProcessCommand::EmitEvent {
                process_id,
                request,
            } => {
                let result = registry.append_event(&process_id, request).await?;
                Ok((
                    ProcessEffectOutcome::EmitEvent {
                        event: Box::new(result.event),
                        wake_delivery: result.wake_delivery.map(Box::new),
                    },
                    result.realization,
                ))
            }
            ProcessCommand::RegisterDefinition { .. } => Err(RuntimeEffectControllerError::new(
                crate::RuntimeErrorCode::RuntimeEffectLocalExecutorMismatch,
                "register-definition requires the process-definition registry executor",
            )),
        };
        if let (Ok((outcome, realization)), Some(observer)) = (&outcome, outcome_observer) {
            observer(outcome, *realization);
        }
        outcome.map(|(outcome, _)| outcome)
    }
}

impl ProcessDefinitionLocalExecution {
    /// Runs the journaled definition CAS write (FIG-3470).
    ///
    /// `operation_id` is the envelope's replay key: the registry keys the
    /// idempotent CAS on it, so a redrive of the same admission re-attaches to
    /// the recorded registration instead of writing a second one.
    pub async fn execute(
        self,
        operation_id: &str,
        command: ProcessCommand,
    ) -> Result<ProcessEffectOutcome, RuntimeEffectControllerError> {
        let ProcessCommand::RegisterDefinition {
            owner_scope,
            name,
            pinned,
            expectation,
        } = command
        else {
            return Err(RuntimeEffectControllerError::new(
                crate::RuntimeErrorCode::RuntimeEffectLocalExecutorMismatch,
                "process-definition registry serves only the register-definition command",
            ));
        };
        let registration = self
            .registry
            .register_definition(
                operation_id,
                owner_scope,
                &name,
                pinned,
                expectation.as_ref(),
            )
            .await?;
        Ok(ProcessEffectOutcome::RegisterDefinition {
            registration: Box::new(registration),
        })
    }
}

#[cfg(test)]
mod terminal_wait_tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct ReattachOnce {
        waits: AtomicUsize,
        terminal: crate::ProcessAwaitOutput,
    }

    #[async_trait::async_trait]
    impl crate::ProcessWorkSubstrate for ReattachOnce {
        async fn admit_pending_processes(
            &self,
            _reason: &str,
        ) -> Result<crate::ProcessAdmissionReport, crate::PluginError> {
            unreachable!("terminal-wait witness does not admit work")
        }

        async fn await_process_terminal(
            &self,
            process_id: &crate::ProcessId,
        ) -> Result<crate::ProcessTerminalWait, crate::PluginError> {
            assert_eq!(process_id, &crate::process_id_for_test("reattach-process"));
            if self.waits.fetch_add(1, Ordering::SeqCst) == 0 {
                Ok(crate::ProcessTerminalWait::Reattach)
            } else {
                Ok(crate::ProcessTerminalWait::Terminal(self.terminal.clone()))
            }
        }
    }

    #[tokio::test]
    async fn process_local_reattaches_once_then_returns_terminal_output() {
        let terminal = crate::ProcessAwaitOutput::from_tool_output(crate::ToolCallOutput::success(
            serde_json::json!({"done": true}),
        ));
        let port = ReattachOnce {
            waits: AtomicUsize::new(0),
            terminal: terminal.clone(),
        };

        let output = await_process_terminal(&port, &crate::process_id_for_test("reattach-process"))
            .await
            .expect("reattachment reaches terminal output");

        assert_eq!(output, terminal);
        assert_eq!(port.waits.load(Ordering::SeqCst), 2);
    }
}
