async fn execute_restate_process_command<'ctx, C>(
    context: &C,
    invocation: &RuntimeInvocation,
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
                    registry.list_observed_by(&session_scope.session_id).await?
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
        ProcessCommand::Await { process_id } => {
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
            // FIG-1488 adds a pre-journal engine-admission gate on the start
            // routes that hold a live session: engine kind, payload validation,
            // and the identity stamp all resolve before the Start command is
            // emitted, so an admitted entry is always one whose engine this host
            // accepted. The gate reads no mutable state, but it is not free of
            // this class: it runs ahead of the journal, so on a redrive it runs
            // again before the recorded Start replays, and an engine whose
            // `validate_start` touches infrastructure can therefore fail a
            // command that already committed. Kind resolution and the identity
            // stamp are pure and cannot; `validate_start` is the open edge, and
            // the host front door (`ToolIntentIngress`) deliberately runs only
            // the pure part for that reason.
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
                    context
                        .request_process_workflow_cancel(RestateProcessCancelRequest {
                            process_id: process_id.clone(),
                            reason: Some("turn cancelled while awaiting process".to_string()),
                        })
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
            process_id,
            reason,
            replay,
        } => {
            let record = registry.get_process(&process_id).await?.ok_or_else(|| {
                lash_core::runtime::registry_transitions::unknown_process(&process_id)
            })?;
            let mut request =
                lash_core::ProcessEventAppendRequest::cancel_requested(&process_id, reason.clone());
            if let Some(replay) = replay {
                request = request.with_optional_replay(Some(replay));
            }
            registry.append_event(&process_id, request).await?;
            context
                .request_process_workflow_cancel(RestateProcessCancelRequest { process_id, reason })
                .await
                .map_err(|err| {
                    PluginError::Runtime(RuntimeError::new(
                        RuntimeErrorCode::RestateProcessCancel,
                        format!("Restate process cancellation failed: {err}"),
                    ))
                })?;
            Ok(ProcessEffectOutcome::Cancel {
                record: Box::new(record),
            })
        }
        ProcessCommand::ParentEnd {
            identity,
            process_id,
            policy,
            reason,
        } => {
            let outcome = match policy {
                lash_core::ProcessParentEndPolicy::Abandon => {
                    lash_core::ToolIntentParentEndOutcome::Abandoned {
                        identity,
                        process_id,
                    }
                }
                lash_core::ProcessParentEndPolicy::Cancel => {
                    let result: Result<(), lash_core::PluginError> = async {
                        registry.get_process(&process_id).await?.ok_or_else(|| {
                            lash_core::runtime::registry_transitions::unknown_process(&process_id)
                        })?;
                        registry
                            .append_event(
                                &process_id,
                                lash_core::ProcessEventAppendRequest::cancel_requested(
                                    &process_id,
                                    Some(reason.clone()),
                                ),
                            )
                            .await?;
                        context
                            .request_process_workflow_cancel(RestateProcessCancelRequest {
                                process_id: process_id.clone(),
                                reason: Some(reason),
                            })
                            .await
                            .map_err(|err| {
                                PluginError::Runtime(RuntimeError::new(
                                    RuntimeErrorCode::RestateProcessCancel,
                                    format!("Restate process cancellation failed: {err}"),
                                ))
                            })?;
                        Ok(())
                    }
                    .await;
                    match result {
                        Ok(()) => lash_core::ToolIntentParentEndOutcome::Cancelled {
                            identity,
                            process_id,
                        },
                        Err(error) => {
                            let error = RuntimeEffectControllerError::from(error);
                            lash_core::ToolIntentParentEndOutcome::Refused {
                                identity,
                                process_id,
                                code: error.code.as_str().to_string(),
                                message: error.message,
                            }
                        }
                    }
                }
            };
            Ok(ProcessEffectOutcome::ParentEnd {
                outcome: Box::new(outcome),
            })
        }
        ProcessCommand::Signal {
            process_id,
            signal_name,
            request,
            ..
        } => {
            let result = registry.append_event(&process_id, request).await?;
            let ordinal = signal_ordinal_for_event(
                registry.as_ref(),
                &process_id,
                result.event.event_type.as_str(),
                result.event.sequence,
            )
            .await?;
            let key = restate_await_event_key(
                &ExecutionScope::process(process_id.clone()),
                AwaitEventWaitIdentity::process_signal(process_id.clone(), signal_name, ordinal),
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

