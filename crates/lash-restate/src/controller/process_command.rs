use super::*;
use crate::process_attach::RestateProcessAttachRequest;
use restate_sdk::serde::Json;

/// Version stamped on the Restate-journaled process-command admission payload;
/// a replay refuses any other version.
///
/// version_guard(
///     roots(JournaledCancelAdmission),
///     roots(
///         path = "crates/lash-core-execution/src/runtime/process/start_staging.rs",
///         RegisteredProcessStart,
///     ),
/// )
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

/// The first observation of an await. A terminal carries its acquired value
/// in this step, so replay never consults a child that retention may prune.
#[derive(Debug, serde::Serialize, serde::Deserialize)]
#[serde(tag = "state", rename_all = "snake_case", deny_unknown_fields)]
enum JournaledProcessAwait {
    Waiting,
    Terminal {
        output: Box<lash_core::ProcessAwaitOutput>,
    },
}

impl JournaledProcessOutcome {
    fn realized(outcome: ProcessEffectOutcome) -> Self {
        Self {
            outcome,
            realization: lash_core::StoreRealization::Realized,
        }
    }
}

/// A signal's recorded admission (FIG-3827, FIG-4298, FIG-4301): the signal
/// as it was admitted, the event its append stored and the realization the
/// store reported, all from the step that appended it.
///
/// The event retains the wait its first append selected, so the resolution
/// is built from recorded fields alone. The admitted signal is what a replay
/// checks the command it reconstructed against before it resolves anything:
/// a changed signal under the same effect address is a divergence, never a
/// resolution with the recorded payload beside today's request.
#[derive(Debug, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct JournaledSignalAppend {
    signal: lash_core::ProcessSignal,
    event: Box<lash_core::ProcessEvent>,
    realization: lash_core::StoreRealization,
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
            async move { crate::process::journal_or_retry(work.await) },
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
    crate::wire::typed_terminal(error.message()).unwrap_or_else(|| {
        crate::wire::typed_terminal(error.message()).unwrap_or_else(|| {
            RuntimeEffectControllerError::new(
                RuntimeErrorCode::EngineEffectController,
                format!("Restate process {operation} journaling failed: {error}"),
            )
        })
    })
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

#[allow(clippy::too_many_arguments)]
async fn observed_process_terminal<'ctx, C>(
    context: &C,
    namespace: &crate::RestateNamespace,
    invocation: &RuntimeEffectInvocation,
    registry: &Arc<dyn ProcessRegistry>,
    attachments: Option<Arc<dyn lash_core::AttachmentReferrers>>,
    process_id: &lash_core::ProcessId,
    turn_cancel: Option<&crate::durable_wait::RestateDurableWaitAwaitRequest>,
    process_cancel: context::ProcessCancelRace,
) -> Result<Option<lash_core::ProcessAwaitOutput>, RuntimeEffectControllerError>
where
    C: RestateControllerContext<'ctx> + ?Sized,
{
    // Observe retention and the terminal in one step (FIG-4307).
    // Acquire the receiver's attachment edges before recording the
    // value (ADR 0124). A read or acquisition fault records nothing;
    // a typed retention refusal is recorded like every command's.
    let observation_registry = Arc::clone(registry);
    let observed_id = process_id.clone();
    let receiver = invocation.execution_scope().clone();
    let Json(observed) = context
        .run_json_or_retry_send::<Result<JournaledProcessAwait, PluginError>, _>(
            process_command_journal_name(invocation, "process-await-observation"),
            async move {
                let record = match crate::process::journal_or_retry(
                    observation_registry.get_process(&observed_id).await,
                )? {
                    Ok(Some(record)) => record,
                    Ok(None) => {
                        return Ok(Err(PluginError::ProcessUnknown {
                            process_id: observed_id,
                        }));
                    }
                    Err(error) => return Ok(Err(error)),
                };
                if record.is_terminal()
                    && record.status() != lash_core::ProcessStatus::Cancelled
                    && record.cancel_request.is_none()
                    && let Some(output) = record.outcome()
                {
                    let output = match attachments {
                        Some(attachments) => {
                            lash_core::runtime::attachment_delivery::deliver_output(
                                attachments.as_ref(),
                                &receiver,
                                output,
                            )
                            .await
                            .map_err(|error| error.to_string())?
                        }
                        None => output,
                    };
                    return Ok(Ok(JournaledProcessAwait::Terminal {
                        output: Box::new(output),
                    }));
                }
                Ok(Ok(JournaledProcessAwait::Waiting))
            },
        )
        .await
        .map_err(|error| process_command_journal_error("await observation", error))?;
    let observed = match observed {
        Ok(observed) => observed,
        // The observation recorded that the process was pruned: the await
        // answers the typed not-retained output from that record, as the
        // local awaiter does, so a replay after the prune returns it too
        // (ADR 0105 §1).
        Err(PluginError::ProcessNoLongerRetained {
            terminal_label,
            pruned_at_ms,
        }) => {
            return Ok(Some(lash_core::ProcessAwaitOutput::NoLongerRetained {
                terminal_label,
                pruned_at_ms,
            }));
        }
        Err(refusal) => return Err(refusal.into()),
    };
    // Cancellation and revocation remain journaled observations. A
    // closed gate or cancelled process drive takes the ordinary wait
    // path, which owns the cancellation race and its refusal.
    let return_terminal = if matches!(&observed, JournaledProcessAwait::Terminal { .. }) {
        let active = match turn_cancel {
            Some(request) => matches!(
                context
                    .peek_turn_gate(namespace, request.key.clone())
                    .await
                    .map_err(|error| process_command_journal_error("await control", error))?,
                crate::durable_wait::RestateTurnGatePeek::Open(None)
            ),
            None => match invocation.execution_scope().session_id() {
                Some(session_id) => !context
                    .session_is_revoked(namespace, session_id.clone())
                    .await
                    .map_err(|error| process_command_journal_error("await revocation", error))?,
                None => true,
            },
        };
        active
            && (process_cancel == context::ProcessCancelRace::NotRaced
                || !context
                    .peek_process_cancel_requested()
                    .await
                    .map_err(|error| {
                        process_command_journal_error("await process cancellation", error)
                    })?)
    } else {
        false
    };
    Ok(match observed {
        JournaledProcessAwait::Terminal { output } if return_terminal => Some(*output),
        _ => None,
    })
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
    namespace: &crate::RestateNamespace,
    authority_id: &RestateAuthorityId,
    sender_generation: Option<&lash_core::engine::BuildGeneration>,
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
    if matches!(
        command,
        ProcessCommand::PublishDefinition { .. } | ProcessCommand::GetDefinition { .. }
    ) {
        let execution = local_executor.into_definition_execution()?;
        return recorded_process_step(context, invocation, "process-definition", async move {
            let outcome = execution.execute(command).await?;
            if let Some(observer) = outcome_observer {
                observer(&outcome, lash_core::StoreRealization::Realized);
            }
            Ok(JournaledProcessOutcome {
                outcome,
                realization: lash_core::StoreRealization::Realized,
            })
        })
        .await
        .map(|recorded| recorded.outcome);
    }
    let recorded_local_operation = match &command {
        ProcessCommand::List { .. } => Some("process-list"),
        ProcessCommand::CompleteExternal { .. } => Some("process-complete-external"),
        ProcessCommand::ValidateVisible { .. } => Some("process-validate-visible"),
        _ => None,
    };
    if let Some(operation) = recorded_local_operation {
        let execution = local_executor.into_process()?;
        let receiver = &invocation.address().execution_scope;
        let recorded = recorded_process_step(context, invocation, operation, async move {
            let outcome = Box::pin(execution.execute(receiver, command))
                .await
                .map_err(PluginError::from)?;
            let realization = match &outcome {
                ProcessEffectOutcome::CompleteExternal { completion } => {
                    match completion.as_ref() {
                        lash_core::ProcessCompletionOutcome::Committed(_) => {
                            lash_core::StoreRealization::Realized
                        }
                        _ => lash_core::StoreRealization::Coalesced,
                    }
                }
                _ => lash_core::StoreRealization::Realized,
            };
            Ok(JournaledProcessOutcome {
                outcome,
                realization,
            })
        })
        .await?;
        if let Some(observer) = outcome_observer {
            observer(&recorded.outcome, recorded.realization);
        }
        return Ok(recorded.outcome);
    }
    // Read before the executor is taken apart: a start answers its served-only
    // mark at its frontier marker (FIG-3779).
    let served_only = local_executor.served_only();
    let execution = local_executor.into_process()?;
    let registry = execution.registry;
    let process_env_store = execution.process_env_store;
    let process_engines = execution.process_engines;
    let host_start = execution.host_start;
    let turn_cancellation = execution.turn_cancellation;
    let attachments = execution.attachments;
    let trigger_route = execution.trigger_route;
    let outcome = match command {
        ProcessCommand::Start {
            registration,
            observers,
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
            //
            // The start stages under `Start(key)`, guarded by the journal of
            // the scope that runs this step (ADR 0113 §3.3): once Restate
            // settles it, a start that never registered is ended.
            let starter = invocation
                .address
                .execution_scope
                .journal_identity()
                .map_err(RuntimeEffectControllerError::from)?;
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
                        starter: &starter,
                        session_catalog: lash_core::runtime::HostStartAdmission::catalog(
                            host_start.as_deref(),
                        ),
                        session_turn_admission:
                            lash_core::runtime::HostStartAdmission::session_turn_admission(
                                host_start.as_deref(),
                            ),
                        trigger_route: trigger_route.as_ref(),
                    };
                    match lash_core::runtime::register_process_start(
                        &stores,
                        stored_registration,
                        &observers,
                    )
                    .await
                    {
                        Ok(started) => Ok(Ok(started)),
                        Err(error) if error.is_terminal() => Ok(Err(error)),
                        // An unavailable route is the start's outcome too:
                        // recorded, a replay after the provider came back
                        // answers as this attempt did, and the delivery's
                        // recovery owns the retry (FIG-4554).
                        Err(error)
                            if error.code
                                == lash_core::RuntimeErrorCode::TriggerRouteUnavailable =>
                        {
                            Ok(Err(error))
                        }
                        Err(error) => Err(error.to_string()),
                    }
                },
            );
            let journaled = match live {
                None => run.await,
                Some(live) => live.serve(run).await?,
            };
            let recorded = match journaled {
                Ok(Json(recorded)) => recorded,
                // The engine's cancellation of this invocation (the call's
                // group decided its cancel) may surface at the step's await
                // after the row committed. A registered child must still
                // reach the engine: the call's abandonment cancels it, and
                // only a submitted run can honour that cancel (FIG-4127). The
                // start goes on with the row its key holds, read in a step of
                // its own; with none, nothing registered. The row is taken as
                // retained, so no compensation may touch it.
                Err(error) if context::is_engine_cancellation(&error) => {
                    let registry = Arc::clone(&registry);
                    let start_key = start_key.clone();
                    let Json(retained) = context
                        .run_json_or_retry_send(
                            process_command_journal_name(
                                invocation,
                                "process-start-register-after-cancel",
                            ),
                            async move {
                                registry
                                    .get_process_by_start_key(&start_key)
                                    .await
                                    .map_err(|error| error.to_string())
                            },
                        )
                        .await
                        .map_err(|error| {
                            process_command_journal_error("start registration", error)
                        })?;
                    let Some(record) = retained else {
                        return Err(process_command_journal_error("start registration", error));
                    };
                    Ok(lash_core::runtime::RegisteredProcessStart {
                        env_ref: record.env_ref.clone(),
                        record,
                        disposition: lash_core::ProcessRegistrationOutcome::Existing,
                    })
                }
                Err(error) => {
                    return Err(process_command_journal_error("start registration", error));
                }
            };
            let started: lash_core::runtime::RegisteredProcessStart = recorded?;
            let disposition = started.disposition;
            let registration = started
                .running_registration(registration)
                .with_execution_env_ref(started.env_ref.clone());
            let (record, realization) = schedule_restate_process(
                Arc::clone(&registry),
                execution.process_starts.clone(),
                started,
                registration,
                *execution_context,
                sender_generation.cloned(),
                context,
                namespace,
                invocation,
            )
            .await?;
            Ok((
                ProcessEffectOutcome::Start {
                    record: Box::new(record),
                    disposition,
                },
                realization,
            ))
        }
        // A listing, a transfer and a session delete each record their outcome
        // (FIG-3827): a replay answers what the first execution saw and did,
        // never a re-read or a re-write of the registry as it is now.
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
            let terminal = observed_process_terminal(
                context,
                namespace,
                invocation,
                &registry,
                attachments,
                &process_id,
                turn_cancel.as_ref(),
                process_cancel,
            )
            .await?;
            let output = match terminal {
                Some(output) => output,
                None => {
                    // The ordinary wait receives its terminal through the attach,
                    // which acquires this scope's attachment edges before resolving
                    // the wait (ADR 0124). Arm it on this command's own wait key.
                    let await_key = crate::durable_wait::restate_await_event_key_for_authority(
                        authority_id,
                        invocation.execution_scope(),
                        lash_core::AwaitEventWaitIdentity::Custom {
                            key: process_await_wait_key(&process_id, invocation.effect_id()),
                        },
                    )?;
                    context
                        .attach_process_terminal(
                            namespace,
                            RestateProcessAttachRequest {
                                process_id: process_id.clone(),
                                key: await_key.clone(),
                            },
                        )
                        .await
                        .map_err(|err| {
                            crate::wire::lash_terminal(&err, RuntimeErrorCode::EngineProcessAwait)
                        })?;
                    let await_request = crate::durable_wait::RestateDurableWaitAwaitRequest {
                        key: await_key.clone(),
                        deadline: None,
                    };
                    trace_park("process");
                    let first_wait = context
                        .await_event_or_turn_cancel(
                            namespace,
                            await_request,
                            await_key.key_id.clone(),
                            turn_cancel,
                            process_cancel,
                        )
                        .await;
                    let first_wait = match first_wait {
                        Ok(outcome) => outcome,
                        Err(err) => {
                            trace_resolve(
                                "process",
                                lash_trace::TraceDurableWaitResolution::Failed,
                            );
                            return Err(crate::wire::lash_terminal(
                                &err,
                                RuntimeErrorCode::EngineProcessAwait,
                            ));
                        }
                    };
                    match first_wait {
                        RestateTurnCancelRaceOutcome::Completed(resolution) => {
                            trace_resolve(
                                "process",
                                lash_trace::TraceDurableWaitResolution::Resolved,
                            );
                            process_await_output_from_resolution(resolution)?
                        }
                        RestateTurnCancelRaceOutcome::ProcessCancelled => {
                            // The awaiting process was cancelled while it waited: its
                            // await ends cancelled, which the drive records as its
                            // own cancellation. The awaited process is left to its
                            // own lifecycle; the ended parent scope's parent-end
                            // plan, not this wait, owns its children.
                            trace_resolve(
                                "process",
                                lash_trace::TraceDurableWaitResolution::Cancelled,
                            );
                            lash_core::ProcessAwaitOutput::from_tool_output(
                                lash_core::ToolCallOutput::cancelled(
                                    lash_core::ToolCancellation::runtime(format!(
                                        "awaiting process `{process_id}` was cancelled"
                                    )),
                                ),
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
                            let requester = serde_json::to_string(&turn_cancellation.scope)
                                .map_err(|error| {
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
                                    process_command_journal_error(
                                        "turn-stop cancel admission",
                                        error,
                                    )
                                })?;
                            if let Some(cancel_request) = cancel_request {
                                context
                                    .request_process_workflow_cancel(namespace, cancel_request)
                                    .await
                                    .map_err(|err| {
                                        PluginError::Runtime(
                                            crate::wire::typed_terminal(err.message())
                                                .unwrap_or_else(|| {
                                                    RuntimeEffectControllerError::new(
                                                        RuntimeErrorCode::EngineProcessCancel,
                                            format!("Restate process cancellation failed: {err}"),
                                                    )
                                                })
                                                .into_runtime_error(),
                                        )
                                    })?;
                            }
                            // The race released the first wait as cancelled, so the
                            // cancelled process's terminal arrives through an attach
                            // armed on a wait of its own, acquired like any other.
                            let after_key =
                                crate::durable_wait::restate_await_event_key_for_authority(
                                    authority_id,
                                    invocation.execution_scope(),
                                    lash_core::AwaitEventWaitIdentity::Custom {
                                        key: process_await_after_turn_cancel_wait_key(
                                            &process_id,
                                            invocation.effect_id(),
                                        ),
                                    },
                                )?;
                            context
                                .attach_process_terminal(
                                    namespace,
                                    RestateProcessAttachRequest {
                                        process_id: process_id.clone(),
                                        key: after_key.clone(),
                                    },
                                )
                                .await
                                .map_err(|err| {
                                    crate::wire::lash_terminal(
                                        &err,
                                        RuntimeErrorCode::EngineProcessAwaitAfterTurnCancel,
                                    )
                                })?;
                            trace_park("process_after_turn_cancel");
                            match context
                                .await_event(
                                    namespace,
                                    crate::durable_wait::RestateDurableWaitAwaitRequest {
                                        key: after_key.clone(),
                                        deadline: None,
                                    },
                                    after_key.key_id.clone(),
                                    tokio_util::sync::CancellationToken::new(),
                                )
                                .await
                            {
                                Ok(resolution) => {
                                    trace_resolve(
                                        "process_after_turn_cancel",
                                        lash_trace::TraceDurableWaitResolution::Resolved,
                                    );
                                    process_await_output_from_resolution(resolution)?
                                }
                                Err(err) => {
                                    trace_resolve(
                                        "process_after_turn_cancel",
                                        lash_trace::TraceDurableWaitResolution::Failed,
                                    );
                                    return Err(crate::wire::lash_terminal(
                                        &err,
                                        RuntimeErrorCode::EngineProcessAwaitAfterTurnCancel,
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
                    }
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
            let turn_cancel = restate_process_turn_cancel_wait_request(
                authority_id,
                invocation,
                turn_cancellation.is_some(),
                turn_cancellation
                    .as_ref()
                    .map(|turn_cancellation| &turn_cancellation.scope),
            )?;
            let terminal = observed_process_terminal(
                context,
                namespace,
                invocation,
                &registry,
                attachments,
                &process_id,
                turn_cancel.as_ref(),
                process_cancel,
            )
            .await?;
            let outcome = match terminal {
                Some(output) => ProcessEffectOutcome::Await {
                    output: Box::new(output),
                },
                None => {
                    context
                        .attach_process_terminal(
                            namespace,
                            RestateProcessAttachRequest { process_id, key },
                        )
                        .await
                        .map_err(|err| {
                            crate::wire::lash_terminal(&err, RuntimeErrorCode::EngineProcessAwait)
                        })?;
                    ProcessEffectOutcome::AttachTerminal
                }
            };
            Ok((outcome, lash_core::StoreRealization::Realized))
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
                .request_process_workflow_cancel(
                    namespace,
                    RestateProcessCancelRequest::from_record(&record)?,
                )
                .await
                .map_err(|err| {
                    crate::wire::typed_terminal(err.message()).unwrap_or_else(|| {
                        RuntimeEffectControllerError::new(
                            RuntimeErrorCode::EngineProcessCancel,
                            format!("Restate process cancellation failed: {err}"),
                        )
                    })
                })?;
            Ok((
                ProcessEffectOutcome::Cancel {
                    record: Box::new(record),
                },
                realization,
            ))
        }
        ProcessCommand::Signal { signal } => {
            // The append is the signal's admission, one recorded step ahead
            // of the resolution (FIG-3827): the store derives nothing from
            // the caller but the signal's identity, retains the wait its
            // first append selected on the event (FIG-4298), and the step
            // records the admitted signal beside it (FIG-4301). A replay
            // answers the recorded event, even after the target was pruned,
            // and never re-appends or re-reads a wait that has moved on.
            let step_registry = Arc::clone(&registry);
            let admitted = signal.clone();
            let JournaledSignalAppend {
                signal: recorded_signal,
                event,
                realization,
            } = recorded_process_step(context, invocation, "process-signal-append", async move {
                let appended = step_registry
                    .append_event(admitted.identity.process_id(), admitted.append_request())
                    .await?;
                Ok(JournaledSignalAppend {
                    signal: admitted,
                    event: Box::new(appended.event),
                    realization: appended.realization,
                })
            })
            .await?;
            if recorded_signal != signal {
                return Err(RuntimeEffectControllerError::new(
                    RuntimeErrorCode::EffectReplayDivergence,
                    format!(
                        "Restate process signal `{}` to `{}` replays a recorded admission \
                         of a different signal under the same effect address",
                        signal.identity.signal_id(),
                        signal.identity.process_id()
                    ),
                ));
            }
            let wait = lash_core::runtime::admitted_signal_wait(&event)?;
            let key = restate_await_event_key_for_authority(
                authority_id,
                &ExecutionScope::process(recorded_signal.identity.process_id().clone()),
                AwaitEventWaitIdentity::process_signal(
                    recorded_signal.identity.process_id().clone(),
                    recorded_signal.identity.signal_name(),
                    wait.ordinal,
                ),
            )
            .map_err(PluginError::Runtime)?;
            context
                .resolve_event(
                    namespace,
                    RestateDurableWaitResolveRequest {
                        key,
                        resolution: Resolution::Ok(event.payload.clone()),
                    },
                )
                .await
                .map_err(|err| {
                    PluginError::Runtime(
                        crate::wire::typed_terminal(err.message())
                            .unwrap_or_else(|| {
                                RuntimeEffectControllerError::new(
                                    RuntimeErrorCode::EngineAwaitEventResolve,
                                    format!("Restate process signal resolution failed: {err}"),
                                )
                            })
                            .into_runtime_error(),
                    )
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
        // These admissions are recorded by the early arms above.
        ProcessCommand::List { .. }
        | ProcessCommand::CompleteExternal { .. }
        | ProcessCommand::ValidateVisible { .. }
        | ProcessCommand::PublishDefinition { .. }
        | ProcessCommand::GetDefinition { .. } => Err(RuntimeEffectControllerError::new(
            RuntimeErrorCode::RuntimeEffectLocalExecutorUnavailable,
            "the command must run through its recorded admission",
        )),
    };
    if let (Ok((outcome, realization)), Some(observer)) = (&outcome, outcome_observer) {
        observer(outcome, *realization);
    }
    outcome.map(|(outcome, _)| outcome)
}

/// The wait a direct process await parks on: its own, named by the awaited
/// process and the awaiting command, so two awaits of one process in one
/// scope never share a wait (ADR 0124).
fn process_await_wait_key(process_id: &lash_core::ProcessId, effect_id: &str) -> String {
    format!("process-await:{process_id}:{effect_id}")
}

/// The wait a direct process await reads its terminal from after a turn
/// stop won its first wait: the race released that wait as cancelled.
fn process_await_after_turn_cancel_wait_key(
    process_id: &lash_core::ProcessId,
    effect_id: &str,
) -> String {
    format!("process-await:{process_id}:{effect_id}:after-turn-cancel")
}

/// The terminal a resolved process-await wait carries. The attach workflow
/// resolves the wait with the whole terminal as its value; an error
/// resolution is a terminal it could not observe.
fn process_await_output_from_resolution(
    resolution: lash_core::Resolution,
) -> Result<lash_core::ProcessAwaitOutput, RuntimeEffectControllerError> {
    match resolution {
        lash_core::Resolution::Ok(value) => serde_json::from_value(value).map_err(|error| {
            RuntimeEffectControllerError::new(
                RuntimeErrorCode::EngineProcessAwait,
                format!("process-await resolution does not decode as a terminal: {error}"),
            )
        }),
        lash_core::Resolution::Err(error) => Err(RuntimeEffectControllerError::new(
            RuntimeErrorCode::EngineProcessAwait,
            error.message,
        )),
        lash_core::Resolution::Timeout | lash_core::Resolution::Cancelled => {
            Err(RuntimeEffectControllerError::new(
                RuntimeErrorCode::EngineProcessAwait,
                "a process-await wait ended without the terminal it waits on",
            ))
        }
    }
}
