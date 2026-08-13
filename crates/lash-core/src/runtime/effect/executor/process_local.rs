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
            outcome_observer,
        } = self;
        let outcome = match command {
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
                let waiting_ordinal =
                    registry
                        .get_process(&process_id)
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
                            .count_events_through(
                                &process_id,
                                result.event.event_type.as_str(),
                                result.event.sequence,
                            )
                            .await?
                    }
                };
                if ordinal > 0 {
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
                        .resolve_await_event(
                            &key,
                            crate::Resolution::Ok(result.event.payload.clone()),
                        )
                        .await?;
                }
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
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::TestProcessRegistryWriteExt as _;

    #[tokio::test]
    async fn signal_prefers_declared_wait_ordinal_when_event_count_diverges() {
        let process_id = "declared-signal-ordinal";
        let signal_name = "ready";
        let event_type =
            crate::runtime::process_signal_event_type(signal_name).expect("signal event type");
        let registry = Arc::new(crate::TestLocalProcessRegistry::default());
        registry
            .register_process(
                crate::ProcessRegistration::new(
                    process_id,
                    crate::ProcessInput::External {
                        metadata: serde_json::Value::Null,
                    },
                    crate::RecoveryDisposition::ExternallyOwned,
                    crate::ProcessProvenance::host(),
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
                process_id,
                crate::WaitState {
                    kind: crate::WaitKind::Signal {
                        name: signal_name.to_string(),
                        event_type: event_type.clone(),
                        key: crate::runtime::process_signal_wait_key(process_id, signal_name, 7),
                        ordinal: 7,
                    },
                    since_ms: 1,
                },
            )
            .await
            .expect("park process on deliberately divergent ordinal");

        let controller = Arc::new(InlineRuntimeEffectController::default());
        let payload = serde_json::json!({"value": "wake-seven"});
        let outcome = controller
            .execute_effect(
                crate::RuntimeEffectEnvelope::new(
                    crate::RuntimeInvocation::effect(
                        crate::RuntimeScope::new("runtime"),
                        "signal-divergent-ordinal",
                        crate::RuntimeEffectKind::Process,
                        "signal-divergent-ordinal",
                    ),
                    crate::RuntimeEffectCommand::process(crate::ProcessCommand::Signal {
                        process_id: process_id.to_string(),
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
                    None,
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
                .count_events_through(process_id, &event_type, u64::MAX)
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
