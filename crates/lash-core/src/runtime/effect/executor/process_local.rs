use super::*;

impl ProcessLocalExecution {
    pub async fn execute(
        self,
        command: ProcessCommand,
    ) -> Result<ProcessEffectOutcome, RuntimeEffectControllerError> {
        let Self {
            registry,
            process_work_driver,
            turn_cancellation,
            effect_controller,
        } = self;
        match command {
            ProcessCommand::Start {
                registration,
                observers,
                execution_context: _,
            } => {
                let record =
                    InlineRuntimeEffectController::start_process(registry, registration, observers)
                        .await?;
                if let Some(driver) = process_work_driver.as_ref() {
                    driver.claim_and_run_pending("process_start").await?;
                }
                Ok(ProcessEffectOutcome::Start {
                    record: Box::new(record),
                })
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
                        crate::ProcessObserverBy::host("runtime-effect-transfer"),
                    )
                    .await?;
                Ok(ProcessEffectOutcome::Transfer)
            }
            ProcessCommand::DeleteSession { session_id } => {
                let report = registry.delete_session_process_state(&session_id).await?;
                Ok(ProcessEffectOutcome::DeleteSession { report })
            }
            ProcessCommand::Await { process_id } => {
                let await_terminal = || async {
                    if let Some(driver) = process_work_driver.as_ref() {
                        driver.await_terminal(&process_id).await
                    } else {
                        crate::ProcessAwaiter::polling(Arc::clone(&registry))
                            .await_terminal(&process_id)
                            .await
                    }
                };
                let output = if let Some(turn_cancellation) = turn_cancellation {
                    tokio::select! {
                        biased;
                        output = await_terminal() => output?,
                        _ = turn_cancellation.cancellation.cancelled() => {
                            InlineRuntimeEffectController::request_process_cancel(
                                Arc::clone(&registry),
                                &process_id,
                                Some("turn cancelled while awaiting process".to_string()),
                            )
                            .await?;
                            await_terminal().await?
                        }
                    }
                } else {
                    await_terminal().await?
                };
                Ok(ProcessEffectOutcome::Await {
                    output: Box::new(output),
                })
            }
            ProcessCommand::Cancel { process_id, reason } => {
                let record = InlineRuntimeEffectController::request_process_cancel(
                    registry,
                    &process_id,
                    reason,
                )
                .await?;
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
                    crate::ProcessParentEndPolicy::Abandon => {
                        crate::ToolIntentParentEndOutcome::Abandoned {
                            identity,
                            process_id,
                        }
                    }
                    crate::ProcessParentEndPolicy::Cancel => {
                        match InlineRuntimeEffectController::request_process_cancel(
                            registry,
                            &process_id,
                            Some(reason),
                        )
                        .await
                        {
                            Ok(_) => crate::ToolIntentParentEndOutcome::Cancelled {
                                identity,
                                process_id,
                            },
                            Err(error) => {
                                let error = RuntimeEffectControllerError::from(error);
                                crate::ToolIntentParentEndOutcome::Refused {
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
                let effect_controller = effect_controller.ok_or_else(|| {
                    RuntimeEffectControllerError::new(
                        crate::RuntimeErrorCode::RuntimeEffectLocalExecutorUnavailable,
                        "local process signal execution requires its effect controller",
                    )
                })?;
                let result = registry.append_event(&process_id, request).await?;
                let ordinal = registry
                    .count_events_through(
                        &process_id,
                        result.event.event_type.as_str(),
                        result.event.sequence,
                    )
                    .await?;
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
                    .resolve_await_event(&key, crate::Resolution::Ok(result.event.payload.clone()))
                    .await?;
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
        }
    }
}
