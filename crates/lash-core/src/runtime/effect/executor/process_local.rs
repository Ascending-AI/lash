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
/// process resolves its waiters successfully with that terminal as the value,
/// exactly as the inline await path returns it. Only an unobservable terminal
/// is an error resolution.
pub(crate) fn process_terminal_resolution(output: crate::ProcessAwaitOutput) -> Resolution {
    match serde_json::to_value(&output) {
        Ok(value) => Resolution::Ok(value),
        Err(error) => Resolution::Err(crate::runtime::ExternalCompletionError {
            code: "process_terminal_encode".to_string(),
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
                let (record, realization) =
                    match NativeRuntimeEffectController::start_process_reporting_realization(
                        Arc::clone(&registry),
                        registration,
                        observers,
                    )
                    .await
                    {
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
                            NativeRuntimeEffectController::request_process_cancel_ref(
                                Arc::clone(&registry),
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
                                        code: "process_terminal_unobservable".to_string(),
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
                let (record, realization) =
                    NativeRuntimeEffectController::request_process_cancel_ref_reporting_realization(
                        registry,
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
        };
        if let (Ok((outcome, realization)), Some(observer)) = (&outcome, outcome_observer) {
            observer(outcome, *realization);
        }
        outcome.map(|(outcome, _)| outcome)
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

#[cfg(test)]
mod attach_terminal_tests {
    use super::*;
    use crate::{
        ProcessLeases as _, ProcessLifecycle as _, ProcessQuery as _, ProcessRegistrar as _,
    };

    const WAIT_DEADLINE: std::time::Duration = std::time::Duration::from_secs(5);

    fn registration(process_id: &str) -> crate::ProcessRegistration {
        crate::ProcessRegistration::new(
            process_id,
            crate::ProcessInput::ToolCall {
                call: crate::PreparedToolCall::from_parts(
                    process_id,
                    crate::ToolId::new("test-tool"),
                    "test_tool",
                    serde_json::Value::Null,
                    None,
                    serde_json::Value::Null,
                ),
            },
            crate::RecoveryContract::Rerunnable,
            crate::ProcessProvenance::host(),
            crate::ProcessLifecyclePolicy::new(
                crate::ParentScope::Host,
                crate::OnParentEnd::Abandon,
            ),
        )
        .with_execution_env_ref(Some(crate::ProcessExecutionEnvRef::new(format!(
            "process-env:{process_id}"
        ))))
    }

    struct AttachFixture {
        controller: Arc<NativeRuntimeEffectController>,
        registry: Arc<crate::TestLocalProcessRegistry>,
        process_ref: crate::ProcessRef,
        key: crate::AwaitEventKey,
        session_id: crate::SessionId,
    }

    impl AttachFixture {
        async fn new(process_id: &str) -> Self {
            let controller = Arc::new(NativeRuntimeEffectController::default());
            let registry = Arc::new(crate::TestLocalProcessRegistry::default());
            let record = registry
                .register_process(registration(process_id))
                .await
                .expect("register the process the parked call waits on");
            let session_id = crate::SessionId::from(format!("{process_id}-session"));
            let key = controller
                .await_event_key(
                    &crate::ExecutionScope::turn(session_id.clone(), "waiting-turn"),
                    crate::AwaitEventWaitIdentity::ToolCompletion {
                        tool_call_id: format!("{process_id}-await-call"),
                    },
                )
                .await
                .expect("reserve the parked call's completion key");
            Self {
                controller,
                process_ref: crate::ProcessRef::from_record(&record),
                registry,
                key,
                session_id,
            }
        }

        /// Runs the journaled arming exactly as a (re)driven turn does.
        async fn arm(&self, effect_id: &str) {
            let outcome = self
                .controller
                .execute_effect(
                    crate::RuntimeEffectEnvelope::new(
                        crate::RuntimeEffectInvocation::new(
                            crate::EffectAddress::new(
                                crate::ExecutionScope::runtime_operation("runtime"),
                                effect_id,
                            )
                            .expect("valid attach test address"),
                            crate::RuntimeAttribution::none(),
                            effect_id,
                        ),
                        crate::RuntimeEffectCommand::process(ProcessCommand::AttachTerminal {
                            process_ref: self.process_ref.clone(),
                            key: self.key.clone(),
                        }),
                    ),
                    crate::RuntimeEffectLocalExecutor::processes(
                        Arc::clone(&self.registry) as Arc<dyn crate::ProcessRegistry>,
                        Arc::new(crate::NativeProcessWork::for_registry(
                            Arc::clone(&self.registry) as Arc<dyn crate::ProcessRegistry>,
                        )),
                    )
                    .with_process_effect_controller(self.controller.clone()),
                )
                .await
                .expect("arming the process terminal must succeed");
            assert!(
                matches!(
                    outcome,
                    crate::RuntimeEffectOutcome::Process {
                        result: ProcessEffectOutcome::AttachTerminal
                    }
                ),
                "arming must return so the turn can park, never carry a terminal"
            );
        }

        /// Terminalizes the awaited process the way its executor does: under
        /// the lease that fences the row's single writer.
        async fn complete(&self, value: serde_json::Value) -> crate::ProcessAwaitOutput {
            let terminal =
                crate::ProcessAwaitOutput::from_tool_output(crate::ToolCallOutput::success(value));
            let process_id = self.process_ref.process_id.clone();
            let owner = crate::LeaseOwnerIdentity::opaque(
                "attach-terminal-test-writer",
                "attach-terminal-test-writer:001",
            );
            let crate::ProcessLeaseClaimOutcome::Acquired(lease) = self
                .registry
                .claim_process_lease(&process_id, &owner, 60_000)
                .await
                .expect("claim the awaited process row")
            else {
                panic!("the test is the only writer of this row")
            };
            self.registry
                .complete_process_with_lease(&lease, terminal.clone())
                .await
                .expect("the awaited process reaches its terminal");
            terminal
        }

        async fn resolution(&self) -> crate::Resolution {
            self.controller
                .await_await_event(
                    &self.key,
                    tokio_util::sync::CancellationToken::new(),
                    Some(std::time::Instant::now() + WAIT_DEADLINE),
                )
                .await
                .expect("the parked wait must settle")
        }
    }

    /// The park is only worth anything if the terminal actually lands on it:
    /// the call parks with nothing in hand, and the process finishing later is
    /// what resolves it.
    #[tokio::test]
    async fn an_armed_process_terminal_resolves_the_parked_wait_when_the_process_finishes() {
        let fixture = AttachFixture::new("attach-resolves").await;
        fixture.arm("attach-resolves").await;
        assert_eq!(
            fixture
                .controller
                .peek_await_event(&fixture.key)
                .await
                .expect("peek the parked wait"),
            None,
            "the wait must still be open while the process runs, or the call \
             never parked at all"
        );

        let terminal = fixture.complete(serde_json::json!({"done": true})).await;

        assert_eq!(
            fixture.resolution().await,
            process_terminal_resolution(terminal),
            "the terminal is the parked call's answer"
        );
    }

    /// Turn cancel disarms the *wait*, not the process: the waiter stops
    /// waiting, and the process it was watching keeps running to its own
    /// terminal, which a later reader can still observe.
    #[tokio::test]
    async fn turn_cancel_disarms_the_wait_while_the_process_reaches_its_terminal() {
        let fixture = AttachFixture::new("attach-cancelled").await;
        fixture.arm("attach-cancelled").await;

        // The cancel sweep reaches waits that are outstanding, so the waiter
        // has to be parked before it lands; retrying until the parked waiter
        // settles is what removes the race, not a weaker assertion.
        let mut waiter = Box::pin(fixture.resolution());
        let deadline = std::time::Instant::now() + WAIT_DEADLINE;
        let resolution = loop {
            assert!(
                std::time::Instant::now() < deadline,
                "turn cancel never reached the parked wait"
            );
            fixture
                .controller
                .cancel_await_events_for_session(&fixture.session_id)
                .await
                .expect("turn cancel reaches the parked wait");
            if let Ok(resolution) =
                tokio::time::timeout(std::time::Duration::from_millis(20), &mut waiter).await
            {
                break resolution;
            }
        };
        assert_eq!(
            resolution,
            crate::Resolution::Cancelled,
            "a cancelled turn must stop waiting instead of parking forever"
        );

        let terminal = fixture
            .complete(serde_json::json!({"done": "anyway"}))
            .await;
        let record = fixture
            .registry
            .get_process(&fixture.process_ref.process_id)
            .await
            .expect("read the awaited process")
            .expect("the cancelled wait must not have removed the process");
        assert!(
            record.is_terminal(),
            "cancelling the wait must not cancel the process it was watching"
        );
        assert_eq!(
            fixture
                .controller
                .peek_await_event(&fixture.key)
                .await
                .expect("peek the cancelled wait"),
            Some(crate::Resolution::Cancelled),
            "the terminal must not overwrite the cancellation the turn already \
             observed: {terminal:?}"
        );
    }

    /// A crash of the waiting turn redrives the journaled arming, so the arm
    /// runs again against a wait that is still open. The wait is
    /// single-assignment, so however many armings race the same terminal, the
    /// parked call is answered exactly once.
    #[tokio::test]
    async fn a_redriven_arming_resolves_the_same_wait_exactly_once() {
        let fixture = AttachFixture::new("attach-redrive").await;
        fixture.arm("attach-redrive").await;
        fixture.arm("attach-redrive").await;

        let terminal = fixture.complete(serde_json::json!({"done": "once"})).await;
        let resolved = process_terminal_resolution(terminal);
        assert_eq!(fixture.resolution().await, resolved);

        assert_eq!(
            fixture
                .controller
                .resolve_await_event(
                    &fixture.key,
                    crate::Resolution::Ok(serde_json::json!({"done": "twice"})),
                )
                .await
                .expect("a duplicate resolution is reported, not an error"),
            crate::ResolveOutcome::AlreadyResolved {
                terminal: resolved.clone()
            },
            "the second arming's resolution must not overwrite the first"
        );
        assert_eq!(
            fixture
                .controller
                .peek_await_event(&fixture.key)
                .await
                .expect("peek the settled wait"),
            Some(resolved),
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ProcessExecutionEnvStore as _;
    use crate::ProcessId;
    use crate::TestProcessRegistryWriteExt as _;
    use crate::{ProcessEventLog as _, ProcessQuery as _, ProcessRegistrar as _};
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn tool_registration(process_id: &str, marker: &str) -> crate::ProcessRegistration {
        crate::ProcessRegistration::new(
            process_id,
            crate::ProcessInput::ToolCall {
                call: crate::PreparedToolCall::from_parts(
                    process_id,
                    crate::ToolId::new("test-tool"),
                    "test_tool",
                    serde_json::json!({"marker": marker}),
                    None,
                    serde_json::Value::Null,
                ),
            },
            crate::RecoveryContract::Rerunnable,
            crate::ProcessProvenance::host(),
            crate::ProcessLifecyclePolicy::new(
                crate::ParentScope::Host,
                crate::OnParentEnd::Abandon,
            ),
        )
    }

    fn start_envelope(
        effect_id: &str,
        registration: crate::ProcessRegistration,
        env_spec: crate::ProcessExecutionEnvSpec,
    ) -> crate::RuntimeEffectEnvelope {
        crate::RuntimeEffectEnvelope::new(
            crate::RuntimeEffectInvocation::new(
                crate::EffectAddress::new(
                    crate::ExecutionScope::runtime_operation("runtime"),
                    effect_id,
                )
                .expect("valid process-start test address"),
                crate::RuntimeAttribution::none(),
                effect_id,
            ),
            crate::RuntimeEffectCommand::process(crate::ProcessCommand::Start {
                registration,
                observers: Vec::new(),
                env_spec: Some(env_spec),
                execution_context: Box::new(crate::ProcessExecutionContext::default()),
            }),
        )
    }

    #[tokio::test]
    async fn process_start_transfers_environment_and_replays_after_staging_retirement() {
        let process_id = ProcessId::from("owned-env-start");
        let registry = Arc::new(crate::TestLocalProcessRegistry::default());
        let env_store = Arc::new(crate::InMemoryProcessExecutionEnvStore::new());
        let env_spec = crate::ProcessExecutionEnvSpec::new(
            crate::PluginOptions::default(),
            crate::SessionPolicy::new(crate::TurnBudget::Unbounded),
        );
        let env_ref = env_spec.stable_ref().expect("stable environment reference");
        let command = start_envelope(
            "owned-env-start",
            tool_registration(process_id.as_str(), "original"),
            env_spec,
        );
        let executor = || {
            crate::RuntimeEffectLocalExecutor::processes(
                Arc::clone(&registry) as Arc<dyn crate::ProcessRegistry>,
                Arc::new(crate::NativeProcessWork::for_registry(
                    Arc::clone(&registry) as Arc<dyn crate::ProcessRegistry>,
                )),
            )
            .with_process_env_store(
                Arc::clone(&env_store) as Arc<dyn crate::ProcessExecutionEnvStore>
            )
        };
        let controller = NativeRuntimeEffectController::default();

        controller
            .execute_effect(command.clone(), executor())
            .await
            .expect("initial process start");
        controller
            .execute_effect(command, executor())
            .await
            .expect("replayed process start after staging retirement");
        let record = registry
            .get_process(&process_id)
            .await
            .expect("read registered process")
            .expect("registered process remains live");

        env_store
            .release_process_execution_env(
                &crate::ArtifactOwner::process(crate::ProcessRef::from_record(&record)),
                &env_ref,
            )
            .await
            .expect("release process environment owner");
        assert_eq!(
            env_store
                .get_process_execution_env(&env_ref)
                .await
                .expect("read reclaimed environment"),
            None,
            "the process owner must be the only surviving edge after transfer"
        );
    }

    /// A `ProcessExecutionEnvStore` that retires one staging owner at the exact
    /// moment a start settles its staged environment.
    ///
    /// The injected call is the one a concurrent attempt at the same start makes
    /// on its own: `process-start:<id>` is stable per process id, so a second
    /// attempt — a trigger delivery reconciled by the worker while the emitting
    /// turn is still starting it, or an attempt whose registration failed —
    /// retires the shared staging owner. The interleaving point is the reachable
    /// one: after this attempt's publication landed, before its transfer.
    struct RetireStagingOwnerBeforeFirstTransfer {
        inner: Arc<crate::InMemoryProcessExecutionEnvStore>,
        staging_owner: crate::ArtifactOwner,
        interleavings: AtomicUsize,
    }

    #[async_trait::async_trait]
    impl crate::ProcessExecutionEnvStore for RetireStagingOwnerBeforeFirstTransfer {
        async fn publish_process_execution_env(
            &self,
            owner: &crate::ArtifactOwner,
            env_ref: &crate::ProcessExecutionEnvRef,
            bytes: &[u8],
        ) -> Result<(), crate::PluginError> {
            self.inner
                .publish_process_execution_env(owner, env_ref, bytes)
                .await
        }

        async fn transfer_process_execution_env(
            &self,
            from: &crate::ArtifactOwner,
            to: &crate::ArtifactOwner,
            env_ref: &crate::ProcessExecutionEnvRef,
        ) -> Result<(), crate::PluginError> {
            if self.interleavings.fetch_add(1, Ordering::SeqCst) == 0 {
                self.inner
                    .retire_process_execution_env_owner(&self.staging_owner)
                    .await?;
            }
            self.inner
                .transfer_process_execution_env(from, to, env_ref)
                .await
        }

        async fn release_process_execution_env(
            &self,
            owner: &crate::ArtifactOwner,
            env_ref: &crate::ProcessExecutionEnvRef,
        ) -> Result<(), crate::PluginError> {
            self.inner
                .release_process_execution_env(owner, env_ref)
                .await
        }

        async fn retire_process_execution_env_owner(
            &self,
            owner: &crate::ArtifactOwner,
        ) -> Result<(), crate::PluginError> {
            self.inner.retire_process_execution_env_owner(owner).await
        }

        async fn get_process_execution_env(
            &self,
            env_ref: &crate::ProcessExecutionEnvRef,
        ) -> Result<Option<Vec<u8>>, crate::PluginError> {
            self.inner.get_process_execution_env(env_ref).await
        }
    }

    /// FIG-3090: a start whose staging edge is severed mid-flight still leaves
    /// the registered process owning its environment.
    ///
    /// This is the trigger-delivery shape: the registration names an environment
    /// a subscription already published, the start re-publishes it under the
    /// stable staging owner, and a concurrent attempt at the same start retires
    /// that owner before this attempt transfers. The store is right to refuse a
    /// transfer whose source edge is gone; this attempt still holds the exact
    /// content-addressed bytes, so it settles the destination edge itself
    /// instead of failing the delivery.
    #[tokio::test]
    async fn a_start_settles_its_environment_after_a_concurrent_attempt_retires_the_staging_owner()
    {
        let process_id = ProcessId::from("raced-staging-owner-start");
        let registry = Arc::new(crate::TestLocalProcessRegistry::default());
        let env_spec = crate::ProcessExecutionEnvSpec::new(
            crate::PluginOptions::default(),
            crate::SessionPolicy::new(crate::TurnBudget::Unbounded),
        );
        let env_ref = env_spec.stable_ref().expect("stable environment reference");
        let bytes = env_spec.to_store_bytes().expect("encode environment");
        let inner = Arc::new(crate::InMemoryProcessExecutionEnvStore::new());
        // The subscription's own edge, exactly as a registered trigger holds it.
        let subscription_owner = crate::ArtifactOwner::host("trigger-subscription");
        inner
            .publish_process_execution_env(&subscription_owner, &env_ref, &bytes)
            .await
            .expect("publish the subscription environment");
        let env_store = Arc::new(RetireStagingOwnerBeforeFirstTransfer {
            inner: Arc::clone(&inner),
            staging_owner: crate::ArtifactOwner::process_start(&process_id),
            interleavings: AtomicUsize::new(0),
        });
        let envelope = crate::RuntimeEffectEnvelope::new(
            crate::RuntimeEffectInvocation::new(
                crate::EffectAddress::new(
                    crate::ExecutionScope::runtime_operation("runtime"),
                    "raced-staging-owner-start",
                )
                .expect("valid process-start test address"),
                crate::RuntimeAttribution::none(),
                "raced-staging-owner-start",
            ),
            crate::RuntimeEffectCommand::process(crate::ProcessCommand::Start {
                registration: tool_registration(process_id.as_str(), "delivery")
                    .with_execution_env_ref(Some(env_ref.clone())),
                observers: Vec::new(),
                env_spec: None,
                execution_context: Box::new(crate::ProcessExecutionContext::default()),
            }),
        );
        let executor = crate::RuntimeEffectLocalExecutor::processes(
            Arc::clone(&registry) as Arc<dyn crate::ProcessRegistry>,
            Arc::new(crate::NativeProcessWork::for_registry(
                Arc::clone(&registry) as Arc<dyn crate::ProcessRegistry>,
            )),
        )
        .with_process_env_store(Arc::clone(&env_store) as Arc<dyn crate::ProcessExecutionEnvStore>);

        NativeRuntimeEffectController::default()
            .execute_effect(envelope, executor)
            .await
            .expect("a severed staging edge must not fail the start");

        assert_eq!(
            env_store.interleavings.load(Ordering::SeqCst),
            1,
            "the start must still attempt the transfer first"
        );
        let record = registry
            .get_process(&process_id)
            .await
            .expect("read registered process")
            .expect("registered process remains live");
        let process_owner = crate::ArtifactOwner::process(crate::ProcessRef::from_record(&record));
        inner
            .release_process_execution_env(&subscription_owner, &env_ref)
            .await
            .expect("release the subscription edge");
        assert_eq!(
            inner
                .get_process_execution_env(&env_ref)
                .await
                .expect("read the retained environment"),
            Some(bytes.clone()),
            "the registered process must own its environment after the race"
        );
        inner
            .release_process_execution_env(&process_owner, &env_ref)
            .await
            .expect("release the process edge");
        assert_eq!(
            inner
                .get_process_execution_env(&env_ref)
                .await
                .expect("read the reclaimed environment"),
            None,
            "the process owner must be the only surviving edge"
        );
    }

    /// A `ProcessWorkSubstrate` whose advisory poke always fails.
    struct PokeAlwaysFails {
        pokes: AtomicUsize,
    }

    #[async_trait::async_trait]
    impl crate::ProcessWorkSubstrate for PokeAlwaysFails {
        async fn admit_pending_processes(
            &self,
            _reason: &str,
        ) -> Result<crate::ProcessAdmissionReport, crate::PluginError> {
            self.pokes.fetch_add(1, Ordering::SeqCst);
            Err(crate::PluginError::Session(
                "injected worker poke failure".to_string(),
            ))
        }

        async fn await_process_terminal(
            &self,
            _process_ref: &crate::ProcessRef,
        ) -> Result<crate::ProcessTerminalWait, crate::PluginError> {
            unreachable!("poke witness does not await terminals")
        }
    }

    /// FIG-2964, native tier: the worker poke after registration is advisory.
    ///
    /// Registration already committed the durable row, and the row is the work
    /// queue — the recovery sweep runs it whether or not the nudge lands. A
    /// failed nudge surfaced as a start error would tell the caller the child
    /// does not exist while it is queued to run, and the caller's retry would
    /// then do the work twice.
    #[tokio::test]
    async fn a_failed_worker_poke_still_returns_the_started_record() {
        let process_id = ProcessId::from("advisory-poke-start");
        let registry = Arc::new(crate::TestLocalProcessRegistry::default());
        let env_spec = crate::ProcessExecutionEnvSpec::new(
            crate::PluginOptions::default(),
            crate::SessionPolicy::new(crate::TurnBudget::Unbounded),
        );
        let process_work = Arc::new(PokeAlwaysFails {
            pokes: AtomicUsize::new(0),
        });
        let executor = crate::RuntimeEffectLocalExecutor::processes(
            Arc::clone(&registry) as Arc<dyn crate::ProcessRegistry>,
            Arc::clone(&process_work) as Arc<dyn crate::ProcessWorkSubstrate>,
        )
        .with_process_env_store(Arc::new(crate::InMemoryProcessExecutionEnvStore::new())
            as Arc<dyn crate::ProcessExecutionEnvStore>);

        let outcome = NativeRuntimeEffectController::default()
            .execute_effect(
                start_envelope(
                    "advisory-poke-start",
                    tool_registration(process_id.as_str(), "advisory"),
                    env_spec,
                ),
                executor,
            )
            .await
            .expect("an advisory poke failure must not fail the start");
        let crate::RuntimeEffectOutcome::Process {
            result: ProcessEffectOutcome::Start { record },
        } = outcome
        else {
            panic!("wrong start outcome")
        };
        assert_eq!(record.id, process_id);
        assert_eq!(
            process_work.pokes.load(Ordering::SeqCst),
            1,
            "the start must still attempt the nudge"
        );

        let stored = registry
            .get_process(&process_id)
            .await
            .expect("read registered process")
            .expect("the registered row stands");
        assert!(
            !stored.is_terminal(),
            "a failed nudge must not terminalise the row the rescan will run"
        );
        assert!(
            stored.cancel_request.is_none(),
            "a failed nudge is not a start failure and must not request cancel"
        );
    }

    #[tokio::test]
    async fn failed_process_registration_retires_staging_environment_owner() {
        let process_id = ProcessId::from("failed-owned-env-start");
        let registry = Arc::new(crate::TestLocalProcessRegistry::default());
        let env_store = Arc::new(crate::InMemoryProcessExecutionEnvStore::new());
        let env_spec = crate::ProcessExecutionEnvSpec::new(
            crate::PluginOptions::default(),
            crate::SessionPolicy::new(crate::TurnBudget::Unbounded),
        );
        let env_ref = env_spec.stable_ref().expect("stable environment reference");
        registry
            .register_process(
                tool_registration(process_id.as_str(), "existing")
                    .with_execution_env_ref(Some(env_ref.clone())),
            )
            .await
            .expect("register conflicting process");
        let bytes = env_spec.to_store_bytes().expect("encode environment");
        let staging_owner = crate::ArtifactOwner::process_start(&process_id);
        let executor = crate::RuntimeEffectLocalExecutor::processes(
            Arc::clone(&registry) as Arc<dyn crate::ProcessRegistry>,
            Arc::new(crate::NativeProcessWork::for_registry(
                Arc::clone(&registry) as Arc<dyn crate::ProcessRegistry>,
            )),
        )
        .with_process_env_store(Arc::clone(&env_store) as Arc<dyn crate::ProcessExecutionEnvStore>);

        NativeRuntimeEffectController::default()
            .execute_effect(
                start_envelope(
                    "failed-owned-env-start",
                    tool_registration(process_id.as_str(), "conflict"),
                    env_spec,
                ),
                executor,
            )
            .await
            .expect_err("conflicting registration must fail");

        assert_eq!(
            env_store
                .get_process_execution_env(&env_ref)
                .await
                .expect("read reclaimed environment"),
            None,
            "failed registration must reclaim its staging edge"
        );
        assert!(
            env_store
                .publish_process_execution_env(&staging_owner, &env_ref, &bytes)
                .await
                .is_err(),
            "failed registration must fence a late staging publication"
        );
    }

    #[tokio::test]
    async fn signal_prefers_declared_wait_ordinal_when_event_count_diverges() {
        let process_id = "declared-signal-ordinal";
        let signal_name = "ready";
        let event_type =
            crate::runtime::process_signal_event_type(signal_name).expect("signal event type");
        let registry = Arc::new(crate::TestLocalProcessRegistry::default());
        let record = registry
            .register_process(
                crate::ProcessRegistration::new(
                    process_id,
                    crate::ProcessInput::External {
                        metadata: serde_json::Value::Null,
                    },
                    crate::RecoveryContract::ExternallyOwned,
                    crate::ProcessProvenance::host(),
                    crate::ProcessLifecyclePolicy::new(
                        crate::ParentScope::Host,
                        crate::OnParentEnd::Abandon,
                    ),
                )
                .with_extra_event_types([crate::ProcessEventType {
                    name: event_type.clone(),
                    payload_schema: crate::LashSchema::any(),
                    semantics: crate::ProcessEventSemanticsSpec::default(),
                }]),
            )
            .await
            .expect("register process");
        registry
            .set_process_wait(
                &ProcessId::from(process_id),
                crate::WaitState {
                    kind: crate::WaitKind::Signal {
                        name: signal_name.to_string(),
                        event_type: event_type.clone(),
                        key: crate::runtime::process_signal_wait_key(
                            &ProcessId::from(process_id),
                            signal_name,
                            7,
                        ),
                        ordinal: 7,
                    },
                    since_ms: 1,
                },
            )
            .await
            .expect("park process on deliberately divergent ordinal");

        let controller = Arc::new(NativeRuntimeEffectController::default());
        let payload = serde_json::json!({"value": "wake-seven"});
        let outcome = controller
            .execute_effect(
                crate::RuntimeEffectEnvelope::new(
                    crate::RuntimeEffectInvocation::new(
                        crate::EffectAddress::new(
                            crate::ExecutionScope::runtime_operation("runtime"),
                            "signal-divergent-ordinal",
                        )
                        .expect("valid signal test address"),
                        crate::RuntimeAttribution::none(),
                        "signal-divergent-ordinal",
                    ),
                    crate::RuntimeEffectCommand::process(crate::ProcessCommand::Signal {
                        process_ref: crate::ProcessRef::from_record(&record),
                        signal_name: signal_name.to_string(),
                        signal_id: "signal-1".to_string(),
                        request: crate::ProcessEventAppendRequest::new(
                            event_type.clone(),
                            payload.clone(),
                        )
                        .with_replay_key("signal-divergent-ordinal:1"),
                    }),
                ),
                crate::RuntimeEffectLocalExecutor::processes(
                    Arc::clone(&registry) as Arc<dyn crate::ProcessRegistry>,
                    Arc::new(crate::NativeProcessWork::for_registry(
                        Arc::clone(&registry) as Arc<dyn crate::ProcessRegistry>,
                    )),
                )
                .with_process_effect_controller(controller.clone()),
            )
            .await
            .expect("execute signal command");
        assert!(matches!(
            outcome,
            crate::RuntimeEffectOutcome::Process {
                result: crate::ProcessEffectOutcome::Signal { .. }
            }
        ));
        assert_eq!(
            registry
                .count_events_through(&ProcessId::from(process_id), &event_type, u64::MAX)
                .await
                .expect("count appended signal events"),
            1,
            "the event-count derivation must actually diverge from the declared wait ordinal"
        );

        let declared_key = controller
            .await_event_key(
                &crate::ExecutionScope::process(process_id),
                crate::AwaitEventWaitIdentity::process_signal(process_id, signal_name, 7),
            )
            .await
            .expect("derive declared wait key");
        assert_eq!(
            controller
                .peek_await_event(&declared_key)
                .await
                .expect("read declared wait key"),
            Some(crate::Resolution::Ok(payload))
        );
        let counted_key = controller
            .await_event_key(
                &crate::ExecutionScope::process(process_id),
                crate::AwaitEventWaitIdentity::process_signal(process_id, signal_name, 1),
            )
            .await
            .expect("derive event-count key");
        assert_eq!(
            controller
                .peek_await_event(&counted_key)
                .await
                .expect("read event-count key"),
            None,
            "the fallback count must not override a matching declared WaitState ordinal"
        );
    }
}
