use super::*;

async fn await_process_terminal(
    process_work: &dyn crate::ProcessWorkSubstrate,
    process_id: &lash_vocabulary::identity::ProcessId,
) -> Result<
    lash_execution_contracts::process::ProcessAwaitOutput,
    lash_execution_contracts::plugin_error::PluginError,
> {
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
/// [`ProcessAwaitOutput`](lash_execution_contracts::process::ProcessAwaitOutput) as the resolution payload.
/// Only an unobservable terminal is an error resolution.
///
/// That payload is the journaled fact, not the value a cell sees. The await
/// site converts it with
/// [`ProcessAwaitOutput::into_tool_output`](lash_execution_contracts::process::ProcessAwaitOutput::into_tool_output)
/// — the same conversion the inline await path performs — so
/// `await processes.await({ handle })` answers exactly what `await handle`
/// answers, in every terminal state. See
/// `crate::tool_result::tool_output_from_completion_resolution`.
pub(crate) fn process_terminal_resolution(
    output: lash_execution_contracts::process::ProcessAwaitOutput,
) -> Resolution {
    match serde_json::to_value(crate::Durable(&output)) {
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
    registration: &lash_execution_contracts::process::ProcessStartRegistration,
    execution_context: &lash_execution_contracts::process::ProcessExecutionContext,
) -> Result<lash_protocol_types::EffectJournalIdentity, RuntimeEffectControllerError> {
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
    output: lash_execution_contracts::process::ProcessAwaitOutput,
) -> Result<
    lash_execution_contracts::process::ProcessAwaitOutput,
    lash_execution_contracts::plugin_error::PluginError,
> {
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
            attachments,
            outcome_observer,
            actors,
        } = self;
        let mut observer =
            lash_execution_contracts::process::ProcessWorkObserver::new(Arc::clone(&registry));
        if let Some(actors) = actors {
            observer = observer.with_actor_parks(actors);
        }
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
                        phases: host_start.phases.as_deref(),
                        starter: &starter,
                    },
                    registration,
                    &observers,
                ))
                .await?;
                let realization = started.realization();
                let disposition = started.disposition;
                let record = started.record;
                let record =
                    lash_execution_contracts::process::observation::observe_process_record(
                        &observer, record,
                    )
                    .await?;
                if let Some(phases) = &host_start.phases {
                    phases.mark(lash_trace::ProcessStartPhase::Observe);
                }
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
                        lash_execution_contracts::process::ProcessListMode::Live => {
                            registry
                                .list_live_observed_by(&session_scope.session_id)
                                .await?
                        }
                        lash_execution_contracts::process::ProcessListMode::All => registry
                            .list_observed_by(
                                &session_scope.session_id,
                                &lash_execution_contracts::process::ProcessListFilter {
                                    status:
                                        lash_execution_contracts::process::ProcessStatusFilter::Any,
                                    ..Default::default()
                                },
                            )
                            .await?,
                    },
                    crate::ProcessListSelection::HostRunning => {
                        let filter = &lash_execution_contracts::process::ProcessListFilter {
                            // Live, not only `running`: a process that
                            // sleeps or awaits reads `waiting`.
                            status: lash_execution_contracts::process::ProcessStatusFilter::any_of(
                                lash_execution_contracts::process::ProcessStatus::ALL
                                    .iter()
                                    .copied()
                                    .filter(
                                        lash_execution_contracts::process::ProcessStatus::is_live,
                                    ),
                            ),
                            ..Default::default()
                        };
                        let mut records = Vec::new();
                        let mut continuation = None;
                        loop {
                            let page = registry
                                .list_processes_page(
                                    filter,
                                    std::num::NonZeroUsize::MIN
                                        .saturating_add(lash_execution_contracts::process::MAX_PROCESS_ROSTER_PAGE_SIZE - 1),
                                    continuation,
                                )
                                .await?;
                            records.extend(page.records);
                            continuation = page.continuation;
                            if continuation.is_none() {
                                break;
                            }
                        }
                        records
                    }
                };
                Ok((
                    ProcessEffectOutcome::List {
                        entries:
                            lash_execution_contracts::process::observation::observe_process_records(
                                &observer, entries,
                            )
                            .await?,
                    },
                    lash_execution_contracts::process::StoreRealization::Realized,
                ))
            }
            ProcessCommand::ValidateVisible { owner, process_ids } => {
                let mut not_visible = None;
                for process_id in process_ids {
                    let visible = match &owner {
                        lash_vocabulary::identity::RuntimeOwner::Session(session_id) => {
                            registry.is_observer(session_id, &process_id).await
                        }
                        lash_vocabulary::identity::RuntimeOwner::Process(starter) => {
                            let starter = crate::ScopeId::process(starter.clone());
                            registry.get_process(&process_id).await.map(|record| {
                                record.is_some_and(|record| {
                                    record.ancestry.starter() == Some(&starter)
                                })
                            })
                        }
                    };
                    match visible {
                        Ok(true) | Err(lash_execution_contracts::plugin_error::PluginError::ProcessNoLongerRetained { .. }) => {}
                        Ok(false) => {
                            not_visible = Some(process_id);
                            break;
                        }
                        Err(error) => return Err(error.into()),
                    }
                }
                Ok((
                    ProcessEffectOutcome::ValidateVisible { not_visible },
                    lash_execution_contracts::process::StoreRealization::Realized,
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
                        lash_execution_contracts::process::ProcessObserverBy::host(
                            "runtime-effect-transfer",
                        ),
                    )
                    .await?;
                Ok((
                    ProcessEffectOutcome::Transfer,
                    lash_execution_contracts::process::StoreRealization::Realized,
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
                    lash_execution_contracts::process::StoreRealization::Realized,
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
                    lash_execution_contracts::process::StoreRealization::Realized,
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
                        record: Box::new(
                            lash_execution_contracts::process::observation::observe_process_record(
                                &observer, record,
                            )
                            .await?,
                        ),
                    },
                    realization,
                ))
            }
            ProcessCommand::PublishDefinition { .. } | ProcessCommand::GetDefinition { .. } => {
                Err(RuntimeEffectControllerError::new(
                    crate::RuntimeErrorCode::RuntimeEffectLocalExecutorMismatch,
                    "publish/get-definition requires the definition executor",
                ))
            }
        };
        if let (Ok((_, realization)), Some(observer)) = (&outcome, outcome_observer) {
            observer(*realization);
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
        registration: lash_execution_contracts::process::ProcessStartRegistration,
        observers: Vec<lash_vocabulary::identity::SessionId>,
        execution_context: lash_execution_contracts::process::ProcessExecutionContext,
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
                phases: None,
                starter: &starter,
            },
            registration,
            &observers,
        ))
        .await
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
            ProcessCommand::PublishDefinition { draft } => {
                ports.publish_definition(&engines, &claim, &draft).await?
            }
            ProcessCommand::GetDefinition { definition_id } => {
                match ports
                    .acquire_definition(&engines, &claim, &definition_id)
                    .await?
                {
                    lash_execution_contracts::process::DefinitionAcquisition::Held(resolved) => {
                        resolved.definition
                    }
                    lash_execution_contracts::process::DefinitionAcquisition::Ended => {
                        return Err(lash_execution_contracts::plugin_error::PluginError::from(
                            lash_execution_contracts::module_artifacts::ArtifactStoreError::ReferrerEnded {
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
