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

/// The journal a local process start runs under: the scope of the effect
/// that caused it, or, for a start with no causal effect, the start's own
/// runtime-operation scope keyed by its start key (ADR 0113 §3.3).
fn process_start_starter(
    registration: &crate::ProcessStartRegistration,
    execution_context: &crate::ProcessExecutionContext,
) -> Result<lash_sansio::EffectJournalIdentity, RuntimeEffectControllerError> {
    let journal = match (
        execution_context
            .causal_invocation
            .as_ref()
            .map(|invocation| &invocation.subject),
        registration.start_key.as_ref(),
    ) {
        (Some(crate::RuntimeSubject::Effect { address, .. }), _) => {
            address.execution_scope.journal_identity()
        }
        (_, Some(start_key)) => crate::runtime::start_operation_journal(start_key),
        // Refused before staging: a start with no key stages nothing.
        (_, None) => {
            crate::ExecutionScope::runtime_operation(ProcessCommand::start_effect_id(None))
                .journal_identity()
        }
    };
    Ok(journal?)
}

/// Acquire what `output` delivers into `receiver` before the receiver records
/// it, answering the value to record (ADR 0124). A host with no attachment
/// store delivers the output as it is.
async fn delivered_output(
    attachments: Option<&Arc<dyn crate::AttachmentReferrers>>,
    receiver: &crate::ExecutionScope,
    output: crate::ProcessAwaitOutput,
) -> Result<crate::ProcessAwaitOutput, crate::PluginError> {
    match attachments {
        Some(attachments) => {
            crate::runtime::attachment_delivery::deliver_output(
                attachments.as_ref(),
                receiver,
                output,
            )
            .await
        }
        None => Ok(output),
    }
}

impl RuntimeEffectLocalExecutor<'_> {
    /// Binds the attachment referrers a delivered process terminal is
    /// acquired through before its receiver records it (ADR 0124).
    pub fn with_process_attachments(
        mut self,
        attachments: Arc<dyn crate::AttachmentReferrers>,
    ) -> Self {
        if let RuntimeEffectLocalExecutorState::Target(LocalTarget::Process(execution)) =
            &mut self.state
        {
            execution.attachments = Some(attachments);
        }
        self
    }
}

impl ProcessLocalExecution {
    /// Execute `command`, issued by an effect of `receiver`: the scope that
    /// records what the command returns.
    pub async fn execute(
        self,
        receiver: &crate::ExecutionScope,
        command: ProcessCommand,
    ) -> Result<ProcessEffectOutcome, RuntimeEffectControllerError> {
        let Self {
            registry,
            process_work,
            process_env_store,
            process_engines,
            host_start,
            turn_cancellation,
            effect_controller,
            attachments,
            outcome_observer,
        } = self;
        let outcome = match command {
            ProcessCommand::Start {
                registration,
                observers,
                execution_context,
            } => {
                let starter = process_start_starter(&registration, &execution_context)?;
                // Registration creates the process's actor ready in the same
                // transaction (ADR 0132 §12). A
                // runtime start derives its key from its admitted operation,
                // and a host start from its caller or its admitted scope
                // (ADR 0107). Boxed: staging holds the start and the
                // registration it resolves to, and inlining it would grow
                // every caller's future by both.
                let started = Box::pin(crate::runtime::register_process_start(
                    &crate::runtime::ProcessStartStores {
                        tracing: host_start.tracing.as_ref(),
                        registry: registry.as_ref(),
                        env_store: process_env_store.as_ref(),
                        engines: &process_engines,
                        session_catalog: host_start.session_catalog.as_deref(),
                        session_turn_admission: host_start.session_turn_admission.as_ref(),
                        executor: "process start on the local executor",
                        starter: &starter,
                        trigger_route: None,
                    },
                    registration,
                    &observers,
                ))
                .await?;
                let realization = started.realization();
                let disposition = started.disposition;
                let record = started.record;
                Ok((
                    ProcessEffectOutcome::Start {
                        record: Box::new(record),
                        disposition,
                    },
                    realization,
                ))
            }
            ProcessCommand::List { selection } => {
                let entries = match selection {
                    crate::ProcessListSelection::Observed {
                        session_scope,
                        mode,
                    } => match mode {
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
                    },
                    crate::ProcessListSelection::HostRunning => {
                        registry
                            .list_processes(&crate::ProcessListFilter {
                                status: crate::ProcessStatusFilter::any_of([
                                    crate::ProcessStatus::Running,
                                ]),
                                ..Default::default()
                            })
                            .await?
                    }
                };
                Ok((
                    ProcessEffectOutcome::List { entries },
                    crate::StoreRealization::Realized,
                ))
            }
            ProcessCommand::ValidateVisible { owner, process_ids } => {
                let mut not_visible = None;
                for process_id in process_ids {
                    let visible = match &owner {
                        crate::RuntimeOwner::Session(session_id) => {
                            registry.is_observer(session_id, &process_id).await
                        }
                        crate::RuntimeOwner::Process(starter) => {
                            let starter = crate::ScopeId::process(starter.clone());
                            registry.get_process(&process_id).await.map(|record| {
                                record.is_some_and(|record| {
                                    record.ancestry.starter() == Some(&starter)
                                })
                            })
                        }
                    };
                    match visible {
                        Ok(true) | Err(crate::PluginError::ProcessNoLongerRetained { .. }) => {}
                        Ok(false) => {
                            not_visible = Some(process_id);
                            break;
                        }
                        Err(error) => return Err(error.into()),
                    }
                }
                Ok((
                    ProcessEffectOutcome::ValidateVisible { not_visible },
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
                // retried deletion runs it again instead of reporting the
                // recorded failure.
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
                // Acquire, then return: the return is what the local executor
                // records (ADR 0124 §4). A direct await holds no consumer
                // hold, so a child pruned mid-wait answers the typed
                // source-gone failure.
                let output = delivered_output(attachments.as_ref(), receiver, output).await?;
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
                // redriven, the arming replays, and a new task is armed
                // against a wait that is still open. Resolution is idempotent,
                // so an arming that races a terminal it already missed resolves
                // immediately and a duplicate resolve reports
                // `AlreadyResolved`.
                let effect_controller = effect_controller.clone().ok_or_else(|| {
                    RuntimeEffectControllerError::foreign(
                        "process_terminal_resolver_unavailable",
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
                            // Acquire before the key resolves: the waiter's
                            // journal records the value the resolution carries
                            // (ADR 0124 §4).
                            // A store fault leaves the wait open rather than
                            // recording a failure the fault did not decide:
                            // the redriven turn re-arms and acquires again.
                            Ok(output) => match delivered_output(
                                attachments.as_ref(),
                                &key.scope,
                                output,
                            )
                            .await
                            {
                                Ok(output) => process_terminal_resolution(output),
                                Err(error) => {
                                    tracing::warn!(
                                        process_id = %process_id,
                                        key_id = %key.key_id,
                                        "armed process terminal could not acquire its delivered attachments; the wait stays open for the redrive: {error}"
                                    );
                                    return;
                                }
                            },
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
            ProcessCommand::Signal { signal } => {
                let process_id = signal.identity.process_id();
                // The append admits the signal and mails it to the process
                // actor in one store transaction; a redelivered signal is
                // served its admitted event and mails nothing (ADR 0132 §10).
                let result = registry
                    .append_event(process_id, signal.append_request())
                    .await?;
                let realization = result.realization;
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
            ProcessCommand::PublishDefinition { .. } | ProcessCommand::GetDefinition { .. } => {
                Err(RuntimeEffectControllerError::new(
                    crate::RuntimeErrorCode::RuntimeEffectLocalExecutorMismatch,
                    "publish/get-definition requires the definition executor",
                ))
            }
        };
        if let (Ok((outcome, realization)), Some(observer)) = (&outcome, outcome_observer) {
            observer(outcome, *realization);
        }
        outcome.map(|(outcome, _)| outcome)
    }
}

impl ProcessLocalExecution {
    /// Stage `registration`'s start as a store-local effect of the call
    /// that declares it (ADR 0132 §5): admitted and staged exactly as
    /// [`ProcessCommand::Start`] is, and registered by nothing here. The
    /// staged rows commit with the call's outcome.
    ///
    /// # Errors
    ///
    /// The start's refusal, and any store failure staging it.
    pub async fn stage_start(
        self,
        registration: crate::ProcessStartRegistration,
        observers: Vec<crate::SessionId>,
        execution_context: crate::ProcessExecutionContext,
    ) -> Result<crate::StagedProcessStart, RuntimeEffectControllerError> {
        let starter = process_start_starter(&registration, &execution_context)?;
        // Boxed: staging holds the start and the registration it resolves
        // to, and inlining it would grow every caller's future by both.
        Box::pin(crate::runtime::stage_store_local_start(
            &crate::runtime::ProcessStartStores {
                tracing: self.host_start.tracing.as_ref(),
                registry: self.registry.as_ref(),
                env_store: self.process_env_store.as_ref(),
                engines: &self.process_engines,
                session_catalog: self.host_start.session_catalog.as_deref(),
                session_turn_admission: self.host_start.session_turn_admission.as_ref(),
                executor: "process start staged by its call",
                starter: &starter,
                trigger_route: None,
            },
            registration,
            &observers,
        ))
        .await
    }

    /// Stage `signal` as a store-local effect of the call that sends it:
    /// its append is admitted against the target's row as it stands, and
    /// nothing is appended or mailed here. The call's outcome commits the
    /// signal's event, its mail and the target's wake.
    ///
    /// # Errors
    ///
    /// An unknown target, and the append's refusal.
    pub async fn stage_signal(
        &self,
        signal: &crate::ProcessSignal,
    ) -> Result<crate::runtime::actor::round::StoreLocalEffect, RuntimeEffectControllerError> {
        let process_id = signal.identity.process_id();
        let record = self
            .registry
            .get_process(process_id)
            .await?
            .ok_or_else(|| crate::PluginError::ProcessUnknown {
                process_id: process_id.clone(),
            })?;
        crate::runtime::admit_process_signal_append(&record, &signal.append_request())?;
        Ok(crate::runtime::actor::round::StoreLocalEffect::signal(
            signal,
        )?)
    }
}

impl ProcessDefinitionLocalExecution {
    /// Runs the journaled immutable-definition command: `PublishDefinition`
    /// stores the descriptor and its closure's edges, `GetDefinition`
    /// acquires the closure under the claim, both through the engine's
    /// artifact ports.
    pub async fn execute(
        self,
        command: ProcessCommand,
    ) -> Result<ProcessEffectOutcome, RuntimeEffectControllerError> {
        let Self { engines, claim } = self;
        let ports = engines.artifact_ports().ok_or_else(|| {
            RuntimeEffectControllerError::new(
                crate::RuntimeErrorCode::RuntimeEffectLocalExecutorUnavailable,
                "definition artifact ports are unavailable",
            )
        })?;
        let definition = match command {
            ProcessCommand::PublishDefinition { draft, module } => {
                if let Some(module) = module {
                    ports
                        .modules()
                        .publish_module_artifact(
                            &claim,
                            &module.module_ref,
                            module.bytes.as_bytes(),
                        )
                        .await
                        .map_err(crate::PluginError::from)?;
                }
                ports.publish_definition(&engines, &claim, &draft).await?
            }
            ProcessCommand::GetDefinition { definition_id } => {
                match ports
                    .acquire_definition(&engines, &claim, &definition_id)
                    .await?
                {
                    crate::DefinitionAcquisition::Held(resolved) => resolved.definition,
                    crate::DefinitionAcquisition::Ended => {
                        return Err(crate::PluginError::from(
                            crate::ArtifactStoreError::ReferrerEnded {
                                referrer: claim.referrer().clone(),
                            },
                        )
                        .into());
                    }
                }
            }
            _ => {
                return Err(RuntimeEffectControllerError::new(
                    crate::RuntimeErrorCode::RuntimeEffectLocalExecutorMismatch,
                    "the definition executor serves only publish/get-definition",
                ));
            }
        };
        Ok(ProcessEffectOutcome::Definition {
            definition: Box::new(definition),
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

        async fn deliver_cancel(
            &self,
            _process_id: &crate::ProcessId,
            _request: &crate::CancelRequest,
            _key: &str,
        ) -> Result<(), crate::PluginError> {
            unreachable!("terminal-wait witness does not deliver cancels")
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
