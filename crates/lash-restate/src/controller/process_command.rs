use super::*;
use crate::process_attach::RestateProcessAttachRequest;
use restate_sdk::serde::Json;

/// Version stamped on the Restate-journaled process-command admission payload;
/// a replay refuses any other version.
pub const PROCESS_COMMAND_JOURNAL_PAYLOAD_VERSION: u32 = 2;

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct JournaledCancelCommandIdentity {
    process_id: lash_core::ProcessId,
    origin: lash_core::CancelOrigin,
    requester: String,
    attribution: Option<lash_core::RuntimeReplayAttribution>,
}

#[derive(Debug, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct JournaledCancelAdmission {
    version: u32,
    identity: JournaledCancelCommandIdentity,
    result: Result<Box<ProcessRecord>, PluginError>,
    /// Whether the admission recorded the cancellation or found the store
    /// already holding the same one (FIG-3070).
    ///
    /// Defaulted rather than version-bumped: a journal entry written before
    /// this field existed replays as `Realized`, which is exactly what the
    /// caller reported for it at the time, so no in-flight invocation is
    /// refused for want of a bit that did not exist when it was journaled.
    #[serde(
        default,
        skip_serializing_if = "lash_core::StoreRealization::is_realized"
    )]
    realization: lash_core::StoreRealization,
}

/// A process command's recorded outcome (FIG-3827): the outcome and the
/// realization the store reported for it, which a replay answers instead of
/// re-running the command against the registry as it is now.
#[derive(Debug, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct JournaledProcessOutcome {
    outcome: ProcessEffectOutcome,
    realization: lash_core::StoreRealization,
}

impl JournaledProcessOutcome {
    fn realized(outcome: ProcessEffectOutcome) -> Self {
        Self {
            outcome,
            realization: lash_core::StoreRealization::Realized,
        }
    }
}

/// A signal's recorded append (FIG-3827): the stored event, its realization
/// and the ordinal the signal's wait resolution is keyed by, all read in the
/// step that appended it.
#[derive(Debug, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct JournaledSignalAppend {
    event: Box<lash_core::ProcessEvent>,
    realization: lash_core::StoreRealization,
    ordinal: u64,
}

/// Runs a process command's store work as one recorded step named
/// `operation` (FIG-3827). The step records the value or the typed refusal
/// the store gave, so a replay answers the recorded result even after the
/// store moved on; a retryable store fault ends the attempt unrecorded and the
/// step runs again.
async fn recorded_process_step<'ctx, C, T, Fut>(
    context: &C,
    invocation: &RuntimeEffectInvocation,
    operation: &'static str,
    work: Fut,
) -> Result<T, RuntimeEffectControllerError>
where
    C: RestateControllerContext<'ctx> + ?Sized,
    T: serde::Serialize + serde::de::DeserializeOwned + Send + 'static,
    Fut: std::future::Future<Output = Result<T, PluginError>> + Send,
{
    let Json(recorded) = context
        .run_json_or_retry_send::<Result<T, PluginError>, _>(
            process_command_journal_name(invocation, operation),
            async move {
                match work.await {
                    Ok(value) => Ok(Ok(value)),
                    Err(error) if error.is_retryable() => Err(error.to_string()),
                    Err(error) => Ok(Err(error)),
                }
            },
        )
        .await
        .map_err(|error| process_command_journal_error(operation, error))?;
    Ok(recorded?)
}

pub(super) fn process_command_journal_name(
    invocation: &RuntimeEffectInvocation,
    operation: &str,
) -> String {
    format!("{}.{operation}:v1", restate_effect_name(invocation))
}

pub(super) fn process_command_journal_error(
    operation: &str,
    error: TerminalError,
) -> RuntimeEffectControllerError {
    RuntimeEffectControllerError::new(
        RuntimeErrorCode::EngineEffectController,
        format!("Restate process {operation} journaling failed: {error}"),
    )
}

fn encode_process_command_journal_payload<T: serde::Serialize>(
    operation: &str,
    payload: T,
) -> Result<serde_json::Value, PluginError> {
    serde_json::to_value(payload).map_err(|error| {
        PluginError::Runtime(RuntimeError::new(
            RuntimeErrorCode::RecordEncodingFailed,
            format!("failed to encode Restate process {operation} journal payload: {error}"),
        ))
    })
}

fn decode_process_command_journal_payload<T: serde::de::DeserializeOwned>(
    operation: &str,
    value: serde_json::Value,
) -> Result<T, RuntimeEffectControllerError> {
    serde_json::from_value(value).map_err(|error| {
        RuntimeEffectControllerError::new(
            RuntimeErrorCode::EngineProcessJournalPayloadIncompatible,
            format!("incompatible Restate process {operation} journal payload: {error}"),
        )
    })
}

fn validate_process_command_journal_payload_version(
    operation: &str,
    version: u32,
) -> Result<(), RuntimeEffectControllerError> {
    if version == PROCESS_COMMAND_JOURNAL_PAYLOAD_VERSION {
        return Ok(());
    }
    Err(RuntimeEffectControllerError::new(
        RuntimeErrorCode::EngineProcessJournalPayloadIncompatible,
        format!(
            "incompatible Restate process {operation} journal payload version {version}; expected {PROCESS_COMMAND_JOURNAL_PAYLOAD_VERSION}"
        ),
    ))
}

fn validate_process_command_journal_identity<T: PartialEq>(
    operation: &str,
    recorded: &T,
    current: &T,
) -> Result<(), RuntimeEffectControllerError> {
    if recorded == current {
        return Ok(());
    }
    Err(RuntimeEffectControllerError::new(
        RuntimeErrorCode::EngineProcessJournalIdentityDrift,
        format!("Restate process {operation} journal identity differs from the current command"),
    ))
}

/// Whether `process_ref` names a process incarnation the registry retains:
/// the refusal is typed, never a missing row read as a pending wait.
async fn await_existence_guard(
    registry: &dyn ProcessRegistry,
    process_id: &lash_core::ProcessId,
) -> Result<(), PluginError> {
    registry.require_process_id(process_id).await.map(|_| ())
}

/// The cancel a turn's stop owes the process it was awaiting, as the recorded
/// answer of the turn-stop admission step.
///
/// `Some` is the cancellation the store holds for the process: the one this
/// stop recorded, or one another requester recorded first, which the workflow
/// receives just as idempotently. `None` means the process ended before the
/// stop reached it, so there is nothing to cancel and the wait reads its
/// terminal. Any other store failure is an engine fault: it ends the attempt
/// unrecorded and the step runs again.
async fn turn_stop_process_cancel_admission(
    registry: &dyn ProcessRegistry,
    process_id: &lash_core::ProcessId,
    requester: String,
) -> Result<Option<RestateProcessCancelRequest>, PluginError> {
    let refusal = match registry
        .request_process_cancel(
            process_id,
            lash_core::CancelOrigin::TurnStopped,
            requester,
            None,
        )
        .await
    {
        Ok(record) => return RestateProcessCancelRequest::from_record(&record).map(Some),
        Err(refusal) => refusal,
    };
    match registry.get_process(process_id).await? {
        Some(record) if record.is_terminal() => Ok(None),
        Some(record) if record.cancel_request.is_some() => {
            RestateProcessCancelRequest::from_record(&record).map(Some)
        }
        _ => Err(refusal),
    }
}

#[allow(clippy::too_many_arguments)]
pub(super) async fn execute_restate_process_command<'ctx, C>(
    context: &C,
    authority_id: &RestateAuthorityId,
    process_cancel: context::ProcessCancelRace,
    invocation: &RuntimeEffectInvocation,
    command: ProcessCommand,
    local_executor: RuntimeEffectLocalExecutor<'_>,
    trace_park: impl Fn(&'static str),
    trace_resolve: impl Fn(&'static str, lash_trace::TraceDurableWaitResolution),
) -> Result<ProcessEffectOutcome, RuntimeEffectControllerError>
where
    C: RestateControllerContext<'ctx> + ?Sized,
{
    let mut local_executor = local_executor;
    let outcome_observer = local_executor.take_process_outcome_observer();
    if matches!(command, ProcessCommand::RegisterDefinition { .. }) {
        let outcome = local_executor
            .into_process_definitions()?
            .execute(invocation.replay_key(), command)
            .await?;
        if let Some(observer) = outcome_observer {
            observer(&outcome, lash_core::StoreRealization::Realized);
        }
        return Ok(outcome);
    }
    // Read before the executor is taken apart: a start answers its served-only
    // mark at its frontier marker (FIG-3779).
    let served_only = local_executor.served_only();
    let execution = local_executor.into_process()?;
    let registry = execution.registry;
    let process_env_store = execution.process_env_store;
    let process_engines = execution.process_engines;
    let turn_cancellation = execution.turn_cancellation;
    let outcome = match command {
        ProcessCommand::Start {
            registration,
            observers,
            env_spec,
            execution_context,
        } => {
            // A start is addressed by its key, never by the id it will be
            // minted (ADR 0107); every journaled start carries one.
            let Some(start_key) = registration.start_key.clone() else {
                return Err(RuntimeEffectControllerError::foreign(
                    "process_start_key_missing",
                    lash_core::TurnFailureCause::Outcome,
                    "a journaled process start must carry its start key",
                ));
            };
            // The marker comes first, before anything the start writes: a
            // start refused at its live frontier has acted on nothing
            // (FIG-3779).
            super::live_frontier::pass_process_start_frontier(
                context,
                invocation,
                &start_key,
                served_only.as_ref(),
            )
            .await?;
            // Registration runs inside one journaled step (ADR 0107): the
            // registrar mints an id once per start, and a replay of the
            // parent reads the recorded registration instead of registering
            // again, so a replay after the process was pruned still sends to
            // the recorded id rather than minting a second one. A store fault
            // ends the attempt unrecorded; a terminal refusal is the start's
            // recorded outcome. A served-only start whose registration step
            // runs live goes on only when a retained process already holds its
            // key: the attempt that issued it registered and died before the
            // step journaled. With none, nothing was started, and the start
            // refuses at its live frontier having acted on nothing (FIG-3779
            // option 3).
            let live = served_only
                .clone()
                .map(super::live_frontier::LiveFrontier::new);
            let closure_live = live.clone();
            let stored_registration = registration.clone();
            let run = context.run_json_or_retry_send(
                process_command_journal_name(invocation, "process-start-register"),
                async {
                    if let Some(live) = &closure_live {
                        // FIG-3779 option 3: the step runs live, so its result
                        // was never journaled. A retained process under the
                        // key is this start, registered by the attempt that
                        // died before journaling it, and is served; with none,
                        // the start is needed live.
                        match registry.get_process_by_start_key(&start_key).await {
                            Ok(Some(_)) => {}
                            Ok(None) => return live.reached().await,
                            Err(error) => return Err(error.to_string()),
                        }
                    }
                    let stores = lash_core::runtime::ProcessStartStores {
                        registry: registry.as_ref(),
                        env_store: process_env_store.as_ref(),
                        engines: process_engines.as_ref(),
                        engines_required: true,
                        executor: "Restate process start",
                    };
                    match lash_core::runtime::register_process_start(
                        &stores,
                        stored_registration,
                        &observers,
                        env_spec.as_ref(),
                    )
                    .await
                    {
                        Ok(started) => Ok(Ok(started)),
                        Err(error) if error.is_terminal() => Ok(Err(error)),
                        Err(error) => Err(error.to_string()),
                    }
                },
            );
            let journaled = match live {
                None => run.await,
                Some(live) => live.serve(run).await?,
            };
            let Json(recorded) = journaled
                .map_err(|error| process_command_journal_error("start registration", error))?;
            let started: lash_core::runtime::RegisteredProcessStart = recorded?;
            let registration = registration.with_execution_env_ref(started.env_ref.clone());
            let (record, realization) = schedule_restate_process(
                Arc::clone(&registry),
                started,
                registration,
                *execution_context,
                context,
                invocation,
            )
            .await?;
            Ok((
                ProcessEffectOutcome::Start {
                    record: Box::new(record),
                },
                realization,
            ))
        }
        // A listing, a transfer and a session delete each record their outcome
        // (FIG-3827): a replay answers what the first execution saw and did,
        // never a re-read or a re-write of the registry as it is now.
        ProcessCommand::List {
            session_scope,
            mode,
        } => {
            let step_registry = Arc::clone(&registry);
            recorded_process_step(context, invocation, "process-list", async move {
                let entries = match mode {
                    lash_core::ProcessListMode::Live => {
                        step_registry
                            .list_live_observed_by(&session_scope.session_id)
                            .await?
                    }
                    lash_core::ProcessListMode::All => {
                        step_registry
                            .list_observed_by(
                                &session_scope.session_id,
                                &lash_core::ProcessListFilter {
                                    status: lash_core::ProcessStatusFilter::Any,
                                    ..Default::default()
                                },
                            )
                            .await?
                    }
                };
                Ok(JournaledProcessOutcome::realized(
                    ProcessEffectOutcome::List { entries },
                ))
            })
            .await
            .map(|recorded| (recorded.outcome, recorded.realization))
        }
        ProcessCommand::Transfer {
            from_scope,
            to_scope,
            process_ids,
        } => {
            let step_registry = Arc::clone(&registry);
            recorded_process_step(context, invocation, "process-transfer", async move {
                step_registry
                    .transfer_observers(
                        &from_scope.session_id,
                        &to_scope.session_id,
                        &process_ids,
                        lash_core::ProcessObserverBy::host("restate-transfer"),
                    )
                    .await?;
                Ok(JournaledProcessOutcome::realized(
                    ProcessEffectOutcome::Transfer,
                ))
            })
            .await
            .map(|recorded| (recorded.outcome, recorded.realization))
        }
        ProcessCommand::DeleteSession { session_id } => {
            let step_registry = Arc::clone(&registry);
            recorded_process_step(context, invocation, "process-delete-session", async move {
                let report = step_registry
                    .delete_session_process_state(&session_id)
                    .await?;
                Ok(JournaledProcessOutcome::realized(
                    ProcessEffectOutcome::DeleteSession { report },
                ))
            })
            .await
            .map(|recorded| (recorded.outcome, recorded.realization))
        }
        ProcessCommand::Await { process_id } => {
            // The existence guard is a recorded step (FIG-3808): a guard the
            // first execution passed never fails on a replay, even after the
            // awaited process ended and its row was pruned. Its answer is Ok
            // or the typed refusal the registry gave; a retryable store fault
            // ends the attempt unrecorded and the step runs again.
            //
            // FIG-790 emits Process::Await before observing state; FIG-1521
            // keeps the pre-journal engine-admission gate pure; world
            // readiness belongs to ProcessEngine::run, after the Start command
            // replays.
            let guard_registry = Arc::clone(&registry);
            let guard_id = process_id.clone();
            let Json(guarded) = context
                .run_json_or_retry_send::<Result<(), PluginError>, _>(
                    process_command_journal_name(invocation, "process-await-guard"),
                    async move {
                        match await_existence_guard(guard_registry.as_ref(), &guard_id).await {
                            Ok(()) => Ok(Ok(())),
                            Err(error) if error.is_retryable() => Err(error.to_string()),
                            Err(error) => Ok(Err(error)),
                        }
                    },
                )
                .await
                .map_err(|error| process_command_journal_error("await guard", error))?;
            guarded?;
            // A process await that observes no turn races the awaiting
            // process segment's durable cancel promise when a process drive
            // issues it (FIG-3673).
            let turn_cancel = restate_process_turn_cancel_wait_request(
                authority_id,
                invocation,
                turn_cancellation.is_some(),
                turn_cancellation
                    .as_ref()
                    .map(|turn_cancellation| &turn_cancellation.scope),
            )?;
            trace_park("process");
            let first_wait = context
                .await_process_terminal_or_turn_cancel(
                    process_id.clone(),
                    turn_cancel,
                    process_cancel,
                )
                .await;
            let first_wait = match first_wait {
                Ok(outcome) => outcome,
                Err(err) => {
                    trace_resolve("process", lash_trace::TraceDurableWaitResolution::Failed);
                    return Err(RuntimeEffectControllerError::new(
                        RuntimeErrorCode::EngineProcessAwait,
                        err.to_string(),
                    ));
                }
            };
            let output = match first_wait {
                RestateTurnCancelRaceOutcome::Completed(output) => {
                    trace_resolve("process", lash_trace::TraceDurableWaitResolution::Resolved);
                    *output
                }
                RestateTurnCancelRaceOutcome::ProcessCancelled => {
                    // The awaiting process was cancelled while it waited: its
                    // await ends cancelled, which the drive records as its
                    // own cancellation. The awaited process is left to its
                    // own lifecycle; the ended parent scope's sweep, not this
                    // wait, owns its children.
                    trace_resolve("process", lash_trace::TraceDurableWaitResolution::Cancelled);
                    lash_core::ProcessAwaitOutput::from_tool_output(
                        lash_core::ToolCallOutput::cancelled(lash_core::ToolCancellation::runtime(
                            format!("awaiting process `{process_id}` was cancelled"),
                        )),
                    )
                }
                RestateTurnCancelRaceOutcome::TurnCancelled => {
                    trace_resolve(
                        "process",
                        lash_trace::TraceDurableWaitResolution::TurnCancelled,
                    );
                    let Some(turn_cancellation) = turn_cancellation.as_ref() else {
                        return Err(RuntimeEffectControllerError::new(
                            RuntimeErrorCode::EngineProcessTurnCancelContextMissing,
                            "process-await cancellation won without turn-cancellation context",
                        ));
                    };
                    let requester =
                        serde_json::to_string(&turn_cancellation.scope).map_err(|error| {
                            PluginError::Runtime(RuntimeError::new(
                                RuntimeErrorCode::RecordEncodingFailed,
                                error.to_string(),
                            ))
                        })?;
                    // The losing process wait is cancelled through a recorded
                    // step (ADR 0105 §3: `dispose(child, AwaitCancelled)`).
                    // The store answers differently once the cancel it asked
                    // for has ended the process, so a replay reads the
                    // recorded answer and issues the same cancel call, never
                    // the store (FIG-3752).
                    let admission_registry = Arc::clone(&registry);
                    let admission_process_id = process_id.clone();
                    let Json(cancel_request) = context
                        .run_json_or_retry_send(
                            process_command_journal_name(
                                invocation,
                                "process-await-turn-cancel-admission",
                            ),
                            async move {
                                turn_stop_process_cancel_admission(
                                    admission_registry.as_ref(),
                                    &admission_process_id,
                                    requester,
                                )
                                .await
                                .map_err(|error| error.to_string())
                            },
                        )
                        .await
                        .map_err(|error| {
                            process_command_journal_error("turn-stop cancel admission", error)
                        })?;
                    if let Some(cancel_request) = cancel_request {
                        context
                            .request_process_workflow_cancel(cancel_request)
                            .await
                            .map_err(|err| {
                                PluginError::Runtime(RuntimeError::new(
                                    RuntimeErrorCode::EngineProcessCancel,
                                    format!("Restate process cancellation failed: {err}"),
                                ))
                            })?;
                    }
                    trace_park("process_after_turn_cancel");
                    match context.await_process_terminal(process_id.clone()).await {
                        Ok(output) => {
                            trace_resolve(
                                "process_after_turn_cancel",
                                lash_trace::TraceDurableWaitResolution::Resolved,
                            );
                            output
                        }
                        Err(err) => {
                            trace_resolve(
                                "process_after_turn_cancel",
                                lash_trace::TraceDurableWaitResolution::Failed,
                            );
                            return Err(RuntimeEffectControllerError::new(
                                RuntimeErrorCode::EngineProcessAwaitAfterTurnCancel,
                                err.to_string(),
                            ));
                        }
                    }
                }
                RestateTurnCancelRaceOutcome::SessionRevoked { session_id } => {
                    trace_resolve(
                        "process",
                        lash_trace::TraceDurableWaitResolution::SessionRevoked,
                    );
                    return Err(lash_core::StoreError::SessionDeleted { session_id }.into());
                }
            };
            Ok((
                ProcessEffectOutcome::Await {
                    output: Box::new(output),
                },
                lash_core::StoreRealization::Realized,
            ))
        }
        ProcessCommand::AttachTerminal { process_id, key } => {
            // Prove the process is still retained before claiming the wait is
            // armed. The same retention exposure the `Await` branch documents
            // applies: a host that prunes a terminal row out from under a
            // waiter breaks the wait, and the refusal here is loud rather than
            // a silent park.
            // The guard is a recorded step, as the await's is (FIG-3808,
            // FIG-3827): a replay after the process was pruned answers the
            // recorded pass instead of refusing.
            let guard_registry = Arc::clone(&registry);
            let guard_id = process_id.clone();
            recorded_process_step(context, invocation, "process-attach-guard", async move {
                await_existence_guard(guard_registry.as_ref(), &guard_id).await
            })
            .await?;
            context
                .attach_process_terminal(RestateProcessAttachRequest { process_id, key })
                .await
                .map_err(|err| {
                    RuntimeEffectControllerError::new(
                        RuntimeErrorCode::EngineProcessAwait,
                        err.to_string(),
                    )
                })?;
            Ok((
                ProcessEffectOutcome::AttachTerminal,
                lash_core::StoreRealization::Realized,
            ))
        }
        ProcessCommand::Cancel {
            process_id,
            origin,
            requester,
            attribution,
        } => {
            let command_identity = JournaledCancelCommandIdentity {
                process_id,
                origin,
                requester,
                attribution,
            };
            let admission_registry = Arc::clone(&registry);
            let admitted_identity = command_identity.clone();
            let admission_process_id = command_identity.process_id.clone();
            let admission_requester = command_identity.requester.clone();
            let admission_attribution = command_identity.attribution.clone();
            let Json(admission_value) = context
                .run_json_send(
                    process_command_journal_name(invocation, "process-cancel-admission"),
                    None,
                    async move {
                        let admitted = admission_registry
                            .request_process_cancel_reporting_realization(
                                &admission_process_id,
                                origin,
                                admission_requester,
                                admission_attribution,
                            )
                            .await;
                        let realization = admitted
                            .as_ref()
                            .map(|(_, realization)| *realization)
                            .unwrap_or_default();
                        let result = admitted.map(|(record, _)| Box::new(record));
                        encode_process_command_journal_payload(
                            "cancel admission",
                            JournaledCancelAdmission {
                                version: PROCESS_COMMAND_JOURNAL_PAYLOAD_VERSION,
                                identity: admitted_identity,
                                result,
                                realization,
                            },
                        )
                    },
                )
                .await
                .map_err(|error| process_command_journal_error("cancel admission", error))?;
            let admission_value = admission_value?;
            let admission: JournaledCancelAdmission =
                decode_process_command_journal_payload("cancel admission", admission_value)?;
            validate_process_command_journal_payload_version(
                "cancel admission",
                admission.version,
            )?;
            validate_process_command_journal_identity(
                "cancel admission",
                &admission.identity,
                &command_identity,
            )?;
            let realization = admission.realization;
            let record = *admission.result?;
            context
                .request_process_workflow_cancel(RestateProcessCancelRequest::from_record(&record)?)
                .await
                .map_err(|err| {
                    RuntimeEffectControllerError::new(
                        RuntimeErrorCode::EngineProcessCancel,
                        format!("Restate process cancellation failed: {err}"),
                    )
                })?;
            Ok((
                ProcessEffectOutcome::Cancel {
                    record: Box::new(record),
                },
                realization,
            ))
        }
        ProcessCommand::CancelRefused { refusal, .. } => Ok((
            ProcessEffectOutcome::CancelRefused { refusal },
            lash_core::StoreRealization::Realized,
        )),
        ProcessCommand::Signal {
            process_id,
            signal_name,
            request,
            ..
        } => {
            // The append and the ordinal the resolution is keyed by are one
            // recorded step ahead of the resolution (FIG-3827): a replay after
            // the target was pruned resolves the recorded wait with the
            // recorded payload, never re-appending or re-counting a log that
            // is gone.
            let step_registry = Arc::clone(&registry);
            let step_process_id = process_id.clone();
            let JournaledSignalAppend {
                event,
                realization,
                ordinal,
            } = recorded_process_step(context, invocation, "process-signal-append", async move {
                let appended = step_registry
                    .append_event(&step_process_id, request)
                    .await?;
                let ordinal = signal_ordinal_for_event(
                    step_registry.as_ref(),
                    &step_process_id,
                    appended.event.event_type.as_str(),
                    appended.event.sequence,
                )
                .await?;
                Ok(JournaledSignalAppend {
                    event: Box::new(appended.event),
                    realization: appended.realization,
                    ordinal,
                })
            })
            .await?;
            let key = restate_await_event_key_for_authority(
                authority_id,
                &ExecutionScope::process(process_id.clone()),
                AwaitEventWaitIdentity::process_signal(process_id, signal_name, ordinal),
            )
            .map_err(PluginError::Runtime)?;
            context
                .resolve_event(RestateDurableWaitResolveRequest {
                    key,
                    resolution: Resolution::Ok(event.payload.clone()),
                })
                .await
                .map_err(|err| {
                    PluginError::Runtime(RuntimeError::new(
                        RuntimeErrorCode::EngineAwaitEventResolve,
                        format!("Restate process signal resolution failed: {err}"),
                    ))
                })?;
            Ok((ProcessEffectOutcome::Signal { event }, realization))
        }
        ProcessCommand::EmitEvent {
            process_id,
            request,
        } => {
            // The append records its receipt (FIG-3827), so a replay answers
            // the recorded event and wake delivery.
            let step_registry = Arc::clone(&registry);
            recorded_process_step(context, invocation, "process-emit-event", async move {
                let appended = step_registry.append_event(&process_id, request).await?;
                Ok(JournaledProcessOutcome {
                    outcome: ProcessEffectOutcome::EmitEvent {
                        event: Box::new(appended.event),
                        wake_delivery: appended.wake_delivery.map(Box::new),
                    },
                    realization: appended.realization,
                })
            })
            .await
            .map(|recorded| (recorded.outcome, recorded.realization))
        }
        // Served by the early arm above against the process-definition
        // executor; it never reaches the process executor.
        ProcessCommand::RegisterDefinition { .. } => Err(RuntimeEffectControllerError::new(
            RuntimeErrorCode::RuntimeEffectLocalExecutorUnavailable,
            "register-definition is served by the process-definition executor, \
             which the early arm requires before the process executor runs",
        )),
    };
    if let (Ok((outcome, realization)), Some(observer)) = (&outcome, outcome_observer) {
        observer(outcome, *realization);
    }
    outcome.map(|(outcome, _)| outcome)
}
