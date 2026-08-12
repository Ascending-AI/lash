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
            ProcessCommand::Signal {
                process_id,
                request,
                ..
            } => {
                let result = registry.append_event(&process_id, request).await?;
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
                })
            }
        }
    }
}
