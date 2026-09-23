use super::*;
use tracing::Instrument as _;

async fn await_process_terminal(
    process_work: &dyn crate::ProcessWorkSubstrate,
    process_ref: &crate::ProcessRef,
) -> Result<crate::ProcessAwaitOutput, crate::PluginError> {
    loop {
        match process_work.await_process_terminal(process_ref).await? {
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
                mut registration,
                observers,
                env_spec,
                execution_context: _,
            } => {
                let staging_owner = crate::ArtifactOwner::process_start(&registration.id);
                let env_artifacts = if let Some(env_spec) = env_spec.as_ref() {
                    let env_store = process_env_store.as_ref().ok_or_else(|| {
                        RuntimeEffectControllerError::foreign(
                            "process_env_store_unavailable",
crate::TurnFailureCause::Outcome,
                            "admitted process start carries an execution environment but the local executor has no environment store",
                        )
                    })?;
                    let expected_ref = env_spec.stable_ref().map_err(|error| {
                        crate::PluginError::Session(format!(
                            "failed to encode process execution environment: {error}"
                        ))
                    })?;
                    let bytes = env_spec.to_store_bytes().map_err(|error| {
                        crate::PluginError::Session(format!(
                            "failed to encode process execution environment: {error}"
                        ))
                    })?;
                    let (env_ref, staged) = match crate::publish_process_execution_env(
                        env_store.as_ref(),
                        &staging_owner,
                        env_spec,
                    )
                    .await
                    {
                        Ok(env_ref) => (env_ref, true),
                        Err(publish_error)
                            if crate::artifact_owner_is_permanently_retired(&publish_error) =>
                        {
                            (expected_ref, false)
                        }
                        Err(publish_error) => return Err(publish_error.into()),
                    };
                    registration = registration.with_execution_env_ref(Some(env_ref.clone()));
                    Some((env_ref, bytes, staged))
                } else if let Some(env_ref) = registration.env_ref.as_ref() {
                    let env_store = process_env_store.as_ref().ok_or_else(|| {
                        RuntimeEffectControllerError::foreign(
                            "process_env_store_unavailable",
crate::TurnFailureCause::Outcome,
                            "admitted process start references an execution environment but the local executor has no environment store",
                        )
                    })?;
                    let bytes = env_store
                        .get_process_execution_env(env_ref)
                        .await?
                        .ok_or_else(|| {
                            crate::PluginError::Session(format!(
                                "missing process execution env `{env_ref}`"
                            ))
                        })?;
                    let staged = if let Err(publish_error) = env_store
                        .publish_process_execution_env(&staging_owner, env_ref, &bytes)
                        .await
                    {
                        if crate::artifact_owner_is_permanently_retired(&publish_error) {
                            false
                        } else {
                            return Err(publish_error.into());
                        }
                    } else {
                        true
                    };
                    Some((env_ref.clone(), bytes, staged))
                } else {
                    None
                };
                let engine_artifacts = match registration.input.as_ref() {
                    crate::ProcessInput::Engine { kind, payload } if process_engines.is_some() => {
                        #[expect(
                            clippy::expect_used,
                            reason = "the match guard checked this option"
                        )]
                        let engine = process_engines
                            .as_ref()
                            .expect("checked above")
                            .require(kind)?;
                        let staged = if let Err(protect_error) = engine
                            .protect_start_artifacts(&staging_owner, payload)
                            .await
                        {
                            if crate::artifact_owner_is_permanently_retired(&protect_error) {
                                false
                            } else {
                                return Err(protect_error.into());
                            }
                        } else {
                            true
                        };
                        Some((engine, payload.clone(), staged))
                    }
                    _ => None,
                };
                // Registering the row is the whole start: the registry's
                // non-terminal row is the durable work queue, and the
                // host-owned process-work substrate is its sole executor. The
                // realization reports whether the registry inserted the row or
                // returned one it already held under the same registration
                // fingerprint (FIG-3070).
                let (record, realization) = match registry
                    .register_process_reporting_disposition(registration, &observers)
                    .await
                    .map(|outcome| {
                        let realization = crate::StoreRealization::from_wrote(outcome.is_created());
                        (outcome.record, realization)
                    }) {
                    Ok(outcome) => outcome,
                    Err(error) => {
                        if let Some(env_store) = process_env_store.as_ref() {
                            env_store
                                .retire_process_execution_env_owner(&staging_owner)
                                .await?;
                        }
                        if let Some((engine, _, _)) = engine_artifacts.as_ref() {
                            engine.retire_artifact_owner(&staging_owner).await?;
                        }
                        return Err(error.into());
                    }
                };
                let process_owner =
                    crate::ArtifactOwner::process(crate::ProcessRef::from_record(&record));
                if let (Some(env_store), Some((env_ref, bytes, staged))) =
                    (process_env_store.as_ref(), env_artifacts.as_ref())
                {
                    crate::settle_started_process_execution_env(
                        env_store.as_ref(),
                        &staging_owner,
                        &process_owner,
                        env_ref,
                        bytes,
                        *staged,
                    )
                    .await?;
                }
                if let Some((engine, payload, staged)) = engine_artifacts {
                    crate::settle_started_process_engine_artifacts(
                        engine.as_ref(),
                        &staging_owner,
                        &process_owner,
                        &payload,
                        staged,
                    )
                    .await?;
                }
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
                let report = registry.delete_session_process_state(&session_id).await?;
                Ok((
                    ProcessEffectOutcome::DeleteSession { report },
                    crate::StoreRealization::Realized,
                ))
            }
            ProcessCommand::Await { process_ref } => {
                let await_terminal = || await_process_terminal(process_work.as_ref(), &process_ref);
                let output = if let Some(turn_cancellation) = turn_cancellation {
                    tokio::select! {
                        biased;
                        output = await_terminal() => output?,
                        _ = turn_cancellation.cancellation.cancelled() => {
                            #[expect(clippy::expect_used, reason = "execution scopes are plain string identities")]
                            registry
                                .request_process_cancel(
                                    &process_ref,
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
            ProcessCommand::AttachTerminal { process_ref, key } => {
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
                        let resolution =
                            match await_process_terminal(process_work.as_ref(), &process_ref).await
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
                                process_id = %process_ref.process_id,
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
                process_ref,
                origin,
                requester,
                attribution,
            } => {
                // Reports whether this call recorded the request or found the
                // same cancellation already recorded (FIG-3070).
                let (record, realization) = registry
                    .request_process_cancel_reporting_realization(
                        &process_ref,
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
                process_ref,
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
                let result = registry.append_event_ref(&process_ref, request).await?;
                let realization = result.realization;
                let waiting_ordinal =
                    registry
                        .get_process_ref(&process_ref)
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
                            .count_events_through_ref(
                                &process_ref,
                                result.event.event_type.as_str(),
                                result.event.sequence,
                            )
                            .await?
                    }
                };
                if ordinal > 0 {
                    let key = effect_controller
                        .await_event_key(
                            &crate::ExecutionScope::process(&process_ref.process_id),
                            crate::AwaitEventWaitIdentity::process_signal(
                                &process_ref.process_id,
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
            process_ref: &crate::ProcessRef,
        ) -> Result<crate::ProcessTerminalWait, crate::PluginError> {
            assert_eq!(process_ref.process_id, "reattach-process");
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

        let output = await_process_terminal(
            &port,
            &crate::ProcessRef::new(
                "reattach-process",
                crate::ProcessIncarnation::from_registration_sequence(1),
            ),
        )
        .await
        .expect("reattachment reaches terminal output");

        assert_eq!(output, terminal);
        assert_eq!(port.waits.load(Ordering::SeqCst), 2);
    }
}
