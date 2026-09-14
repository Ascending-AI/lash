use super::*;
use crate::process_attach::RestateProcessAttachRequest;
use restate_sdk::serde::Json;

const PROCESS_COMMAND_JOURNAL_PAYLOAD_VERSION: u32 = 1;

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct JournaledCancelCommandIdentity {
    process_ref: lash_core::ProcessRef,
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

fn process_command_journal_name(invocation: &RuntimeEffectInvocation, operation: &str) -> String {
    format!("{}.{operation}:v1", restate_effect_name(invocation))
}

fn process_command_journal_error(
    operation: &str,
    error: TerminalError,
) -> RuntimeEffectControllerError {
    RuntimeEffectControllerError::new(
        RuntimeErrorCode::RestateEffectController,
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
            RuntimeErrorCode::RestateProcessJournalPayloadIncompatible,
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
        RuntimeErrorCode::RestateProcessJournalPayloadIncompatible,
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
        RuntimeErrorCode::RestateProcessJournalIdentityDrift,
        format!("Restate process {operation} journal identity differs from the current command"),
    ))
}

pub(super) async fn execute_restate_process_command<'ctx, C>(
    context: &C,
    authority_id: &RestateAuthorityId,
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
    let execution = local_executor.into_process()?;
    let registry = execution.registry;
    let process_env_store = execution.process_env_store;
    let process_engines = execution.process_engines;
    let turn_cancellation = execution.turn_cancellation;
    let outcome = match command {
        ProcessCommand::Start {
            mut registration,
            observers,
            env_spec,
            execution_context,
        } => {
            let staging_owner = lash_core::ArtifactOwner::process_start(&registration.id);
            let env_artifacts = if let Some(env_spec) = env_spec.as_ref() {
                let env_store = process_env_store.as_ref().ok_or_else(|| {
                    RuntimeEffectControllerError::foreign(
                        "process_env_store_unavailable",
                        "admitted Restate process start carries an execution environment but the executor has no environment store",
                    )
                })?;
                let expected_ref = env_spec.stable_ref().map_err(|error| {
                    lash_core::PluginError::Session(format!(
                        "failed to encode process execution environment: {error}"
                    ))
                })?;
                let bytes = env_spec.to_store_bytes().map_err(|error| {
                    lash_core::PluginError::Session(format!(
                        "failed to encode process execution environment: {error}"
                    ))
                })?;
                let (env_ref, staged) = match lash_core::runtime::publish_process_execution_env(
                    env_store.as_ref(),
                    &staging_owner,
                    env_spec,
                )
                .await
                {
                    Ok(env_ref) => (env_ref, true),
                    Err(publish_error)
                        if lash_core::runtime::artifact_owner_is_permanently_retired(
                            &publish_error,
                        ) =>
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
                        "admitted Restate process start references an execution environment but the executor has no environment store",
                    )
                })?;
                let bytes = env_store
                    .get_process_execution_env(env_ref)
                    .await?
                    .ok_or_else(|| {
                        lash_core::PluginError::Session(format!(
                            "missing process execution env `{env_ref}`"
                        ))
                    })?;
                let staged = if let Err(publish_error) = env_store
                    .publish_process_execution_env(&staging_owner, env_ref, &bytes)
                    .await
                {
                    if lash_core::runtime::artifact_owner_is_permanently_retired(&publish_error) {
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
                lash_core::ProcessInput::Engine { kind, payload } => {
                    let process_engines = process_engines.as_ref().ok_or_else(|| {
                        RuntimeEffectControllerError::foreign(
                            "process_engine_registry_unavailable",
                            "admitted Restate process start requires an engine but the executor has no process-engine registry",
                        )
                    })?;
                    let engine = process_engines.require(kind)?;
                    let staged = if let Err(protect_error) = engine
                        .protect_start_artifacts(&staging_owner, payload)
                        .await
                    {
                        if lash_core::runtime::artifact_owner_is_permanently_retired(&protect_error)
                        {
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
            let (record, realization) = match schedule_restate_process(
                Arc::clone(&registry),
                registration,
                observers,
                *execution_context,
                context,
            )
            .await
            {
                Ok(scheduled) => scheduled,
                // Registration, workflow submission, and external-ref persistence are
                // separate durable authorities. An error after any one of them is an
                // unknown/retriable start, not proof that the process was abandoned.
                // Keep the staging edges so an exact redrive can finish the transfer;
                // authoritative process retirement owns their eventual permanent fence.
                Err(error) => return Err(error.into()),
            };
            let process_owner =
                lash_core::ArtifactOwner::process(lash_core::ProcessRef::from_record(&record));
            if let (Some(store), Some((env_ref, bytes, staged))) =
                (process_env_store.as_ref(), env_artifacts.as_ref())
            {
                lash_core::runtime::settle_started_process_execution_env(
                    store.as_ref(),
                    &staging_owner,
                    &process_owner,
                    env_ref,
                    bytes,
                    *staged,
                )
                .await?;
            }
            if let Some((engine, payload, staged)) = engine_artifacts {
                lash_core::runtime::settle_started_process_engine_artifacts(
                    engine.as_ref(),
                    &staging_owner,
                    &process_owner,
                    &payload,
                    staged,
                )
                .await?;
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
                lash_core::ProcessListMode::Live => {
                    registry
                        .list_live_observed_by(&session_scope.session_id)
                        .await?
                }
                lash_core::ProcessListMode::All => {
                    registry
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
            Ok((
                ProcessEffectOutcome::List { entries },
                lash_core::StoreRealization::Realized,
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
                    lash_core::ProcessObserverBy::host("restate-transfer"),
                )
                .await?;
            Ok((
                ProcessEffectOutcome::Transfer,
                lash_core::StoreRealization::Realized,
            ))
        }
        ProcessCommand::DeleteSession { session_id } => {
            let report = registry.delete_session_process_state(&session_id).await?;
            Ok((
                ProcessEffectOutcome::DeleteSession { report },
                lash_core::StoreRealization::Realized,
            ))
        }
        ProcessCommand::Await { process_ref } => {
            registry.get_process_ref(&process_ref).await?;
            let process_id = process_ref.process_id.clone();
            // Replay-determinism class inventory: PR #166 removed the process
            // start gate. FIG-788 always redrives the process runner, retains
            // ordinal handovers until terminal delivery resolves, and schedules
            // each segment successor before reading cancellation. FIG-790 emits
            // Process::Await before observing state. FIG-793 emits LlmCall
            // before its durable cancel peek. FIG-806 makes TriggerRouter emit
            // the deterministic process start before consulting reservation
            // status. FIG-1126 keeps await-event key minting pure and performs
            // the revocation observation at the unconditional await boundary.
            //
            // FIG-1521 makes the pre-journal engine-admission gate mechanically
            // pure: its descriptor contains only a static kind and a
            // non-capturing function pointer over recorded payload/env inputs.
            // World readiness belongs to ProcessEngine::run, after the Start
            // command replays, so temporary store/catalog failure remains a
            // retryable process-infrastructure failure rather than a durable
            // command refusal.
            //
            // This existence guard remains an explicit retention exposure, not
            // a proof: registration precedes the effect, and terminal events
            // plus weak-observer removal retain the row, but a host can prune a
            // terminal row while this invocation is still replayable. There is
            // no finite waiter-lifetime bound against which the raw prune cutoff
            // can be validated. In that case `get_process` returns
            // `Err(ProcessNoLongerRetained)` at `?`, not `Ok(None)` at this
            // branch. Hosts must retain terminal rows beyond every such waiter.
            if registry.get_process(&process_id).await?.is_none() {
                return Err(
                    lash_core::runtime::registry_transitions::unknown_process(&process_id).into(),
                );
            }
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
                .await_process_terminal_or_turn_cancel(process_id.clone(), turn_cancel)
                .await;
            let first_wait = match first_wait {
                Ok(outcome) => outcome,
                Err(err) => {
                    trace_resolve("process", lash_trace::TraceDurableWaitResolution::Failed);
                    return Err(RuntimeEffectControllerError::new(
                        RuntimeErrorCode::RestateProcessAwait,
                        err.to_string(),
                    ));
                }
            };
            let output = match first_wait {
                RestateTurnCancelRaceOutcome::Completed(output) => {
                    trace_resolve("process", lash_trace::TraceDurableWaitResolution::Resolved);
                    *output
                }
                RestateTurnCancelRaceOutcome::TurnCancelled => {
                    trace_resolve(
                        "process",
                        lash_trace::TraceDurableWaitResolution::TurnCancelled,
                    );
                    let Some(turn_cancellation) = turn_cancellation.as_ref() else {
                        return Err(RuntimeEffectControllerError::new(
                            RuntimeErrorCode::RestateProcessTurnCancelContextMissing,
                            "process-await cancellation won without turn-cancellation context",
                        ));
                    };
                    turn_cancellation.cancellation.cancel();
                    let record = registry
                        .request_process_cancel(
                            &process_ref,
                            lash_core::CancelOrigin::TurnStopped,
                            serde_json::to_string(&turn_cancellation.scope).map_err(|error| {
                                PluginError::Runtime(RuntimeError::new(
                                    RuntimeErrorCode::RecordEncodingFailed,
                                    error.to_string(),
                                ))
                            })?,
                            None,
                        )
                        .await?;
                    context
                        .request_process_workflow_cancel(RestateProcessCancelRequest::from_record(
                            &record,
                        )?)
                        .await
                        .map_err(|err| {
                            PluginError::Runtime(RuntimeError::new(
                                RuntimeErrorCode::RestateProcessCancel,
                                format!("Restate process cancellation failed: {err}"),
                            ))
                        })?;
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
                                RuntimeErrorCode::RestateProcessAwaitAfterTurnCancel,
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
        ProcessCommand::AttachTerminal { process_ref, key } => {
            // Prove the incarnation still exists before claiming the wait is
            // armed. The same retention exposure the `Await` branch documents
            // applies: a host that prunes a terminal row out from under a
            // waiter breaks the wait, and the refusal here is loud rather than
            // a silent park.
            registry.get_process_ref(&process_ref).await?;
            context
                .attach_process_terminal(RestateProcessAttachRequest { process_ref, key })
                .await
                .map_err(|err| {
                    RuntimeEffectControllerError::new(
                        RuntimeErrorCode::RestateProcessAwait,
                        err.to_string(),
                    )
                })?;
            Ok((
                ProcessEffectOutcome::AttachTerminal,
                lash_core::StoreRealization::Realized,
            ))
        }
        ProcessCommand::Cancel {
            process_ref,
            origin,
            requester,
            attribution,
        } => {
            let command_identity = JournaledCancelCommandIdentity {
                process_ref,
                origin,
                requester,
                attribution,
            };
            let admission_registry = Arc::clone(&registry);
            let admitted_identity = command_identity.clone();
            let admission_process_ref = command_identity.process_ref.clone();
            let admission_requester = command_identity.requester.clone();
            let admission_attribution = command_identity.attribution.clone();
            let Json(admission_value) = context
                .run_json_send(
                    process_command_journal_name(invocation, "process-cancel-admission"),
                    None,
                    async move {
                        let admitted = admission_registry
                            .request_process_cancel_reporting_realization(
                                &admission_process_ref,
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
                        RuntimeErrorCode::RestateProcessCancel,
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
            process_ref,
            signal_name,
            request,
            ..
        } => {
            let result = registry.append_event_ref(&process_ref, request).await?;
            let realization = result.realization;
            let ordinal = signal_ordinal_for_event(
                registry.as_ref(),
                &process_ref,
                result.event.event_type.as_str(),
                result.event.sequence,
            )
            .await?;
            let key = restate_await_event_key_for_authority(
                authority_id,
                &ExecutionScope::process(process_ref.process_id.clone()),
                AwaitEventWaitIdentity::process_signal(
                    process_ref.process_id,
                    signal_name,
                    ordinal,
                ),
            )
            .map_err(PluginError::Runtime)?;
            context
                .resolve_event(RestateDurableWaitResolveRequest {
                    key,
                    resolution: Resolution::Ok(result.event.payload.clone()),
                })
                .await
                .map_err(|err| {
                    PluginError::Runtime(RuntimeError::new(
                        RuntimeErrorCode::RestateAwaitEventResolve,
                        format!("Restate process signal resolution failed: {err}"),
                    ))
                })?;
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
