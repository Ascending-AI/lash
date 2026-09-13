use super::*;
use restate_sdk::serde::Json;

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
enum JournaledParentEndCancelDecision {
    Issued { record: Box<ProcessRecord> },
    AlreadyStanding { record: Box<ProcessRecord> },
    Terminal,
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

pub(super) async fn execute_restate_process_command<'ctx, C>(
    context: &C,
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
    let turn_cancellation = execution.turn_cancellation;
    let outcome = match command {
        ProcessCommand::Start {
            mut registration,
            observers,
            env_spec,
            execution_context,
        } => {
            if let Some(env_spec) = env_spec.as_ref() {
                let env_store = process_env_store.as_ref().ok_or_else(|| {
                    RuntimeEffectControllerError::foreign(
                        "process_env_store_unavailable",
                        "admitted Restate process start carries an execution environment but the executor has no environment store",
                    )
                })?;
                let env_ref =
                    lash_core::runtime::persist_process_execution_env(env_store.as_ref(), env_spec)
                        .await?;
                registration = registration.with_execution_env_ref(Some(env_ref));
            }
            let record = schedule_restate_process(
                registry,
                registration,
                observers,
                *execution_context,
                context,
            )
            .await?;
            Ok(ProcessEffectOutcome::Start {
                record: Box::new(record),
            })
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
            Ok(ProcessEffectOutcome::List { entries })
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
            Ok(ProcessEffectOutcome::Transfer)
        }
        ProcessCommand::DeleteSession { session_id } => {
            let report = registry.delete_session_process_state(&session_id).await?;
            Ok(ProcessEffectOutcome::DeleteSession { report })
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
            Ok(ProcessEffectOutcome::Await {
                output: Box::new(output),
            })
        }
        ProcessCommand::Cancel {
            process_ref,
            origin,
            requester,
            attribution,
        } => {
            let admission_registry = Arc::clone(&registry);
            let Json(admission) = context
                .run_json_send(
                    process_command_journal_name(invocation, "process-cancel-admission"),
                    None,
                    async move {
                        admission_registry
                            .request_process_cancel(&process_ref, origin, requester, attribution)
                            .await
                    },
                )
                .await
                .map_err(|error| process_command_journal_error("cancel admission", error))?;
            let record = admission?;
            context
                .request_process_workflow_cancel(RestateProcessCancelRequest::from_record(&record)?)
                .await
                .map_err(|err| {
                    RuntimeEffectControllerError::new(
                        RuntimeErrorCode::RestateProcessCancel,
                        format!("Restate process cancellation failed: {err}"),
                    )
                })?;
            Ok(ProcessEffectOutcome::Cancel {
                record: Box::new(record),
            })
        }
        ProcessCommand::CancelRefused { refusal, .. } => {
            Ok(ProcessEffectOutcome::CancelRefused { refusal })
        }
        ProcessCommand::ParentEnd {
            identity,
            process_id,
            policy,
        } => {
            let outcome = match policy {
                lash_core::ProcessParentEndPolicy::Abandon => {
                    lash_core::ToolIntentParentEndOutcome::Abandoned {
                        identity,
                        process_id,
                    }
                }
                lash_core::ProcessParentEndPolicy::Cancel => {
                    let decision_registry = Arc::clone(&registry);
                    let decision_process_id = process_id.clone();
                    let decision_identity = identity.clone();
                    let decision = context
                        .run_json_send(
                            process_command_journal_name(
                                invocation,
                                "parent-end-cancel-decision",
                            ),
                            None,
                            async move {
                                let result: Result<JournaledParentEndCancelDecision, PluginError> = async {
                                    let process_ref = decision_registry
                                        .resolve_process_ref(&decision_process_id)
                                        .await?;
                                    let record = decision_registry
                                        .get_process_ref(&process_ref)
                                        .await?
                                        .ok_or_else(|| {
                                            lash_core::runtime::registry_transitions::unknown_process(
                                                &decision_process_id,
                                            )
                                        })?;
                                    if record.is_terminal() {
                                        return Ok(JournaledParentEndCancelDecision::Terminal);
                                    }
                                    if record.cancel_request.is_some() {
                                        return Ok(
                                            JournaledParentEndCancelDecision::AlreadyStanding {
                                                record: Box::new(record),
                                            },
                                        );
                                    }
                                    let requester = serde_json::to_string(&record.lifecycle.parent)
                                        .map_err(|error| {
                                            PluginError::Runtime(RuntimeError::new(
                                                RuntimeErrorCode::RecordEncodingFailed,
                                                error.to_string(),
                                            ))
                                        })?;
                                    let record = decision_registry
                                        .request_process_cancel(
                                            &process_ref,
                                            lash_core::CancelOrigin::ParentEnded,
                                            requester,
                                            Some(
                                                lash_core::RuntimeReplayAttribution::ToolIntent(
                                                    decision_identity,
                                                ),
                                            ),
                                        )
                                        .await?;
                                    Ok(JournaledParentEndCancelDecision::Issued {
                                        record: Box::new(record),
                                    })
                                }
                                .await;
                                result
                            },
                        )
                        .await
                        .map_err(|error| {
                            process_command_journal_error("parent-end decision", error)
                        });
                    let result: Result<(), RuntimeEffectControllerError> = match decision {
                        Ok(Json(Ok(JournaledParentEndCancelDecision::Issued { record })))
                        | Ok(Json(Ok(JournaledParentEndCancelDecision::AlreadyStanding {
                            record,
                        }))) => match RestateProcessCancelRequest::from_record(&record) {
                            Ok(request) => context
                                .request_process_workflow_cancel(request)
                                .await
                                .map_err(|err| {
                                    RuntimeEffectControllerError::new(
                                        RuntimeErrorCode::RestateProcessCancel,
                                        format!("Restate process cancellation failed: {err}"),
                                    )
                                }),
                            Err(error) => Err(error.into()),
                        },
                        Ok(Json(Ok(JournaledParentEndCancelDecision::Terminal))) => Ok(()),
                        Ok(Json(Err(error))) => Err(error.into()),
                        Err(error) => Err(error),
                    };
                    match result {
                        Ok(()) => lash_core::ToolIntentParentEndOutcome::Cancelled {
                            identity,
                            process_id,
                        },
                        Err(error) => lash_core::ToolIntentParentEndOutcome::Refused {
                            identity,
                            process_id,
                            code: error.code.as_str().to_string(),
                            message: error.message,
                        },
                    }
                }
            };
            Ok(ProcessEffectOutcome::ParentEnd {
                outcome: Box::new(outcome),
            })
        }
        ProcessCommand::Signal {
            process_ref,
            signal_name,
            request,
            ..
        } => {
            let result = registry.append_event_ref(&process_ref, request).await?;
            let ordinal = signal_ordinal_for_event(
                registry.as_ref(),
                &process_ref,
                result.event.event_type.as_str(),
                result.event.sequence,
            )
            .await?;
            let key = restate_await_event_key(
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
            Ok(ProcessEffectOutcome::Signal {
                event: Box::new(result.event),
            })
        }
        ProcessCommand::EmitEvent {
            process_id,
            request,
        } => {
            let result = registry.append_event(&process_id, request).await?;
            Ok(ProcessEffectOutcome::EmitEvent {
                event: Box::new(result.event),
                wake_delivery: result.wake_delivery.map(Box::new),
            })
        }
    };
    if let (Ok(outcome), Some(observer)) = (&outcome, outcome_observer) {
        observer(outcome);
    }
    outcome
}
