use super::*;

impl RuntimeSessionServices {
    pub(in crate::runtime::session_manager::process_runners) async fn run_process_session_turn(
        &self,
        registration: crate::ProcessRegistration,
        mut create_request: crate::SessionCreateRequest,
        turn_input: crate::TurnInput,
        scoped_effect_controller: crate::ScopedEffectController<'_>,
        cancellation: tokio_util::sync::CancellationToken,
    ) -> Result<crate::ProcessAwaitOutput, crate::ProcessInfraError> {
        create_request = create_request.with_caused_by(crate::CausalRef::Process {
            process_id: registration.id.clone(),
        });
        let requested_child_session_id = create_request.session_id.clone();
        if cancellation.is_cancelled() {
            if let Some(child_session_id) = create_request.session_id.as_deref() {
                self.reclaim_prestart_cancelled_child_session(&registration.id, child_session_id)
                    .await?;
            }
            return Ok(cancelled_session_turn_output());
        }
        // `ProcessInput::SessionTurn` is durable input. Its `create_request`
        // carries only persisted policy, so fill an omitted provider_id from
        // the parent runtime policy before the child session is built.
        self.inherit_session_turn_provider_id(&mut create_request);
        let child = match Box::pin(self.managed.create_session(
            &self.current,
            &self.usage,
            create_request,
        ))
        .await
        {
            Ok(child) => child,
            Err(err) => {
                if cancellation.is_cancelled() {
                    if let Some(child_session_id) = requested_child_session_id.as_deref() {
                        self.reclaim_prestart_cancelled_child_session(
                            &registration.id,
                            child_session_id,
                        )
                        .await?;
                    }
                    return Ok(cancelled_session_turn_output());
                }
                return Ok(crate::ProcessAwaitOutput::from_tool_output(
                    crate::ToolCallOutput::failure(crate::ToolFailure::tool(
                        crate::ToolFailureClass::Execution,
                        "process_session_create_failed",
                        err.to_string(),
                    )),
                ));
            }
        };
        let child_session_id = child.session_id.clone();
        let child_turn_id = registration.id.clone();
        let child_scope = match self
            .current
            .turn_scope_by_id(&self.managed, &child_session_id, &child_turn_id)
            .await
        {
            Ok(scope) => scope,
            Err(err) => {
                if self
                    .close_or_reclaim_cancelled_session_turn(
                        &registration.id,
                        &child_session_id,
                        &cancellation,
                    )
                    .await?
                {
                    return Ok(cancelled_session_turn_output());
                }
                return Ok(crate::ProcessAwaitOutput::from_tool_output(
                    crate::ToolCallOutput::failure(crate::ToolFailure::tool(
                        crate::ToolFailureClass::Execution,
                        "process_session_turn_scope_failed",
                        err.to_string(),
                    )),
                ));
            }
        };
        let child_scoped_effect_controller = match scoped_effect_controller.rescope(child_scope) {
            Ok(controller) => controller,
            Err(err) => {
                if self
                    .close_or_reclaim_cancelled_session_turn(
                        &registration.id,
                        &child_session_id,
                        &cancellation,
                    )
                    .await?
                {
                    return Ok(cancelled_session_turn_output());
                }
                return Ok(crate::ProcessAwaitOutput::from_tool_output(
                    crate::ToolCallOutput::failure(crate::ToolFailure::tool(
                        crate::ToolFailureClass::Execution,
                        "process_session_turn_scope_failed",
                        err.to_string(),
                    )),
                ));
            }
        };
        let request = match crate::SessionTurnRequest::new(
            &child_session_id,
            &child_turn_id,
            turn_input,
            child_scoped_effect_controller,
        ) {
            Ok(request) => request,
            Err(err) => {
                if self
                    .close_or_reclaim_cancelled_session_turn(
                        &registration.id,
                        &child_session_id,
                        &cancellation,
                    )
                    .await?
                {
                    return Ok(cancelled_session_turn_output());
                }
                return Ok(crate::ProcessAwaitOutput::from_tool_output(
                    crate::ToolCallOutput::failure(crate::ToolFailure::tool(
                        crate::ToolFailureClass::Execution,
                        "process_session_turn_scope_failed",
                        err.to_string(),
                    )),
                ));
            }
        };
        let mut turn = Box::pin(self.managed.start_turn(&self.current, &self.usage, request));
        let outcome = tokio::select! {
            biased;
            _ = cancellation.cancelled() => None,
            outcome = turn.as_mut() => Some(outcome),
        };
        let Some(outcome) = outcome else {
            // Dropping the managed-turn future aborts its inherited task-local
            // execution before this outer process reacquires the shared slot.
            drop(turn);
            crate::runtime::process_worker::ensure_process_execution_permit().await;
            self.reclaim_cancelled_child_session(&registration.id, &child_session_id)
                .await?;
            return Ok(cancelled_session_turn_output());
        };
        if cancellation.is_cancelled() {
            self.reclaim_cancelled_child_session(&registration.id, &child_session_id)
                .await?;
            return Ok(cancelled_session_turn_output());
        }
        Ok(match outcome {
            Ok(turn) => {
                let state = process_terminal_state_for_turn(&turn);
                if matches!(state, crate::ProcessStatus::Cancelled) {
                    self.reclaim_cancelled_child_session(&registration.id, &child_session_id)
                        .await?;
                } else if self
                    .close_or_reclaim_cancelled_session_turn(
                        &registration.id,
                        &child_session_id,
                        &cancellation,
                    )
                    .await?
                {
                    return Ok(cancelled_session_turn_output());
                }
                crate::ProcessAwaitOutput::from_tool_output(output_from_process_turn(
                    &registration,
                    &child_session_id,
                    turn,
                    state,
                ))
            }
            Err(err) => {
                if self
                    .close_or_reclaim_cancelled_session_turn(
                        &registration.id,
                        &child_session_id,
                        &cancellation,
                    )
                    .await?
                {
                    return Ok(cancelled_session_turn_output());
                }
                crate::ProcessAwaitOutput::from_tool_output(crate::ToolCallOutput::failure(
                    crate::ToolFailure::tool(
                        crate::ToolFailureClass::Execution,
                        "process_session_turn_failed",
                        err.to_string(),
                    ),
                ))
            }
        })
    }

    async fn reclaim_cancelled_child_session(
        &self,
        process_id: &str,
        child_session_id: &str,
    ) -> Result<(), crate::ProcessInfraError> {
        if let Some(factory) = self.current.host.session_store_factory.as_ref()
            && let Some(store) = factory
                .open_existing_store_by_id(child_session_id)
                .await
                .map_err(|error| {
                    crate::ProcessInfraError::new(crate::PluginError::Session(format!(
                        "failed to inspect cancelled child session `{child_session_id}`: {error}"
                    )))
                })?
        {
            self.require_process_owned_child_session(process_id, child_session_id, store.as_ref())
                .await?;
        }
        self.managed
            .close_session(&self.current, &self.usage, child_session_id)
            .await
            .map_err(crate::ProcessInfraError::new)?;
        let Some(factory) = self.current.host.session_store_factory.as_ref() else {
            return Ok(());
        };
        factory.delete_session(child_session_id).await.map_err(|failure| {
            crate::ProcessInfraError::new(crate::PluginError::Session(format!(
                "failed to reclaim cancelled child session `{child_session_id}`: {}; partial report: {:?}",
                failure.stop, failure.partial
            )))
        })?;
        Ok(())
    }

    async fn reclaim_prestart_cancelled_child_session(
        &self,
        process_id: &str,
        child_session_id: &str,
    ) -> Result<(), crate::ProcessInfraError> {
        let Some(factory) = self.current.host.session_store_factory.as_ref() else {
            return Ok(());
        };
        let Some(store) = factory
            .open_existing_store_by_id(child_session_id)
            .await
            .map_err(|error| {
                crate::ProcessInfraError::new(crate::PluginError::Session(format!(
                    "failed to inspect prestart cancelled child session `{child_session_id}`: {error}"
                )))
            })?
        else {
            return Ok(());
        };
        self.require_process_owned_child_session(process_id, child_session_id, store.as_ref())
            .await?;
        self.reclaim_cancelled_child_session(process_id, child_session_id)
            .await
    }

    async fn require_process_owned_child_session(
        &self,
        process_id: &str,
        child_session_id: &str,
        store: &dyn crate::store::RuntimePersistence,
    ) -> Result<(), crate::ProcessInfraError> {
        let meta = store
            .load_session_meta()
            .await
            .map_err(|error| {
                crate::ProcessInfraError::new(crate::PluginError::Session(format!(
                    "failed to inspect prestart cancelled child session `{child_session_id}` metadata: {error}"
                )))
            })?
            .ok_or_else(|| {
                crate::ProcessInfraError::new(crate::PluginError::Session(format!(
                    "refusing to reclaim prestart cancelled child session `{child_session_id}` without durable ownership metadata"
                )))
            })?;
        let owned_by_process = matches!(
            &meta.relation,
            crate::SessionRelation::Child {
                caused_by: Some(crate::CausalRef::Process {
                    process_id: owner_process_id,
                }),
                ..
            } if owner_process_id == process_id
        );
        if !owned_by_process {
            return Err(crate::ProcessInfraError::new(crate::PluginError::Session(
                format!(
                    "refusing to reclaim prestart cancelled child session `{child_session_id}` not owned by process `{process_id}`"
                ),
            )));
        }
        Ok(())
    }

    async fn close_or_reclaim_cancelled_session_turn(
        &self,
        process_id: &str,
        child_session_id: &str,
        cancellation: &tokio_util::sync::CancellationToken,
    ) -> Result<bool, crate::ProcessInfraError> {
        if cancellation.is_cancelled() {
            self.reclaim_cancelled_child_session(process_id, child_session_id)
                .await?;
            return Ok(true);
        }
        let _ = self
            .managed
            .close_session(&self.current, &self.usage, child_session_id)
            .await;
        if cancellation.is_cancelled() {
            self.reclaim_cancelled_child_session(process_id, child_session_id)
                .await?;
            return Ok(true);
        }
        Ok(false)
    }

    fn inherit_session_turn_provider_id(&self, create_request: &mut crate::SessionCreateRequest) {
        let Some(policy) = create_request.policy.as_mut() else {
            return;
        };
        if policy.recorded_provider_id().is_empty() {
            policy.provider_id = self.current.policy.provider_id.clone();
        }
    }
}

fn cancelled_session_turn_output() -> crate::ProcessAwaitOutput {
    crate::ProcessAwaitOutput::from_tool_output(crate::ToolCallOutput::cancelled(
        crate::ToolCancellation::runtime("background session turn was cancelled"),
    ))
}

fn process_terminal_state_for_turn(turn: &crate::AssembledTurn) -> crate::ProcessStatus {
    match &turn.outcome {
        crate::TurnOutcome::Finished(_) | crate::TurnOutcome::AgentFrameSwitch { .. } => {
            crate::ProcessStatus::Completed
        }
        crate::TurnOutcome::Stopped(crate::TurnStop::Cancelled { .. }) => {
            crate::ProcessStatus::Cancelled
        }
        crate::TurnOutcome::Stopped(_) => crate::ProcessStatus::Failed,
    }
}

fn process_turn_summary(
    turn: &crate::AssembledTurn,
    state: crate::ProcessStatus,
) -> Option<String> {
    if state != crate::ProcessStatus::Failed {
        return None;
    }
    match &turn.outcome {
        crate::TurnOutcome::Stopped(
            crate::TurnStop::SubmittedError { value } | crate::TurnStop::ToolError { value, .. },
        ) => value
            .get("reason")
            .and_then(serde_json::Value::as_str)
            .map(ToOwned::to_owned),
        _ => Some("background session turn failed".to_string()),
    }
}

fn output_from_process_turn(
    registration: &crate::ProcessRegistration,
    child_session_id: &str,
    turn: crate::AssembledTurn,
    state: crate::ProcessStatus,
) -> crate::ToolCallOutput {
    if state == crate::ProcessStatus::Cancelled {
        return crate::ToolCallOutput::cancelled(crate::ToolCancellation::runtime(
            "background session turn was cancelled",
        ));
    }
    if state == crate::ProcessStatus::Failed {
        return crate::ToolCallOutput::failure(crate::ToolFailure::tool(
            crate::ToolFailureClass::Execution,
            "process_session_turn_failed",
            process_turn_summary(&turn, state)
                .unwrap_or_else(|| "background session turn failed".to_string()),
        ));
    }
    crate::ToolCallOutput::success(serde_json::json!({
        "process_id": registration.id,
        "child_session_id": child_session_id,
        "turn": turn,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::llm::types::LlmStreamEvent;
    use crate::runtime::tests::helpers::{
        MockCall, mock_provider, named_turn_scope, runtime_with_plugins_and_tools_and_host,
    };
    use std::sync::Arc;

    struct ParkForever {
        started: tokio::sync::mpsc::Sender<()>,
    }

    #[async_trait::async_trait]
    impl crate::ToolProvider for ParkForever {
        fn tool_manifests(&self) -> Vec<crate::ToolManifest> {
            vec![park_forever_definition().manifest()]
        }

        fn resolve_contract(&self, name: &str) -> Option<Arc<crate::ToolContract>> {
            (name == "park_forever").then(|| Arc::new(park_forever_definition().contract()))
        }

        async fn execute(&self, _call: crate::ToolCall<'_>) -> crate::ToolOutcome {
            let _ = self.started.send(()).await;
            std::future::pending::<()>().await;
            unreachable!("the parked tool never completes")
        }
    }

    fn park_forever_definition() -> crate::ToolDefinition {
        crate::ToolDefinition::raw(
            "tool:park_forever",
            "park_forever",
            "park the calling turn forever",
            crate::ToolDefinition::default_input_schema(),
            serde_json::json!({ "type": "object", "additionalProperties": false }),
        )
    }

    async fn cancelled_mid_turn_subagent_reclaims_durable_child_rows(case: &str) {
        let child_session_id = format!("cancelled-{case}-subagent-child");
        let process_id = format!("process:subagent:cancelled-{case}");
        let factory = crate::InMemorySessionStoreFactory::new();
        let host = crate::EmbeddedRuntimeHost::new(crate::RuntimeHostConfig::in_memory(
            crate::CommitBudget::bounded(1024 * 1024, 512),
            crate::QueuedWorkBatchingConfig::new(1),
        ))
        .with_session_store_factory(Arc::new(factory.clone()));
        let (started_tx, mut started_rx) = tokio::sync::mpsc::channel(1);
        let transport = mock_provider(vec![MockCall {
            stream_events: vec![LlmStreamEvent::Part(crate::LlmOutputPart::ToolCall {
                call_id: format!("park-{case}"),
                tool_name: "park_forever".to_string(),
                input_json: "{}".to_string(),
                replay: None,
            })],
            response: Ok(crate::LlmResponse::default()),
        }]);
        let runtime = runtime_with_plugins_and_tools_and_host(
            Vec::new(),
            Arc::new(ParkForever {
                started: started_tx,
            }),
            transport,
            host,
        )
        .await;
        let services = runtime
            .runtime_session_services()
            .expect("runtime session services");
        let foreign_session_id = format!("unrelated-{case}-session");
        factory
            .create_store(&crate::SessionStoreCreateRequest {
                session_id: foreign_session_id.clone(),
                relation: crate::SessionRelation::Root,
                pending_observer_intents: Vec::new(),
                policy: runtime.state.policy().clone(),
            })
            .await
            .expect("materialize unrelated durable session");
        let foreign_process_id = format!("process:subagent:foreign-{case}");
        let foreign_create_request = crate::SessionCreateRequest::child_session(
            runtime.session_id(),
            crate::SessionStartPoint::Empty,
            crate::PluginOptions::default(),
        )
        .with_session_id(&foreign_session_id)
        .with_plugin_source(crate::SessionPluginSource::CurrentSessionFork);
        let foreign_registration = crate::ProcessRegistration::new(
            &foreign_process_id,
            crate::ProcessInput::SessionTurn {
                definition_key: "lash-subagent-session-turn:v1".to_string(),
                create_request: Box::new(foreign_create_request.clone()),
                turn_input: Box::new(crate::TurnInput::text("must not run")),
                output_contract: crate::ToolOutputContract::Static,
            },
            crate::RecoveryContract::Rerunnable,
            crate::ProcessProvenance::host(),
        );
        let foreign_cancellation = tokio_util::sync::CancellationToken::new();
        foreign_cancellation.cancel();
        assert!(
            services
                .run_process_session_turn(
                    foreign_registration,
                    foreign_create_request,
                    crate::TurnInput::text("must not run"),
                    named_turn_scope(&foreign_session_id, &foreign_process_id),
                    foreign_cancellation,
                )
                .await
                .is_err(),
            "a pre-cancelled process must refuse to reclaim an unrelated session id"
        );
        assert!(
            factory
                .raw_store_for_testing(&foreign_session_id)
                .and_then(|store| store.raw_session_meta_for_testing())
                .is_some(),
            "ownership refusal must leave the unrelated parent session durable and reopenable"
        );
        let create_request = crate::SessionCreateRequest::child_session(
            runtime.session_id(),
            crate::SessionStartPoint::Empty,
            crate::PluginOptions::default(),
        )
        .with_session_id(&child_session_id)
        .with_plugin_source(crate::SessionPluginSource::CurrentSessionFork);
        let registration = crate::ProcessRegistration::new(
            &process_id,
            crate::ProcessInput::SessionTurn {
                definition_key: "lash-subagent-session-turn:v1".to_string(),
                create_request: Box::new(create_request.clone()),
                turn_input: Box::new(crate::TurnInput::text("park the child turn")),
                output_contract: crate::ToolOutputContract::Static,
            },
            crate::RecoveryContract::Rerunnable,
            crate::ProcessProvenance::host(),
        );
        let replay_registration = registration.clone();
        let cancellation = tokio_util::sync::CancellationToken::new();
        let mut run = Box::pin(services.run_process_session_turn(
            registration,
            create_request.clone(),
            crate::TurnInput::text("park the child turn"),
            named_turn_scope(&child_session_id, &process_id),
            cancellation.clone(),
        ));
        tokio::select! {
            started = started_rx.recv() => assert_eq!(started, Some(())),
            outcome = run.as_mut() => panic!("{case} child turn completed before cancellation: {outcome:?}"),
        }

        let child_store = factory
            .raw_store_for_testing(&child_session_id)
            .expect("child durable store exists before cancellation");
        let before = [
            usize::from(child_store.raw_session_meta_for_testing().is_some()),
            usize::from(child_store.raw_head_revision_for_testing().is_some()),
            child_store.raw_graph_nodes_for_testing().len(),
            child_store.raw_pending_turn_inputs_for_testing().len(),
            child_store.raw_queued_work_for_testing().len(),
        ];
        assert!(
            before.iter().any(|count| *count > 0),
            "{case} child must materialize durable rows before cancellation"
        );

        cancellation.cancel();
        let output = tokio::time::timeout(std::time::Duration::from_secs(5), run)
            .await
            .expect("cancelled child process settles");
        assert!(matches!(
            output
                .expect("cancelled child cleanup succeeds")
                .into_tool_output()
                .outcome,
            crate::ToolCallOutcome::Cancelled(_)
        ));

        let after = [
            usize::from(child_store.raw_session_meta_for_testing().is_some()),
            usize::from(child_store.raw_head_revision_for_testing().is_some()),
            child_store.raw_graph_nodes_for_testing().len(),
            child_store.raw_pending_turn_inputs_for_testing().len(),
            child_store.raw_queued_work_for_testing().len(),
        ];
        assert_eq!(
            after,
            [0, 0, 0, 0, 0],
            "cancelled {case} subagent child retained [session_meta, session_head, active_graph_nodes, pending_turn_inputs, queued_work_batches]; before={before:?}"
        );
        assert!(
            factory
                .open_existing_store_by_id(&child_session_id)
                .await
                .expect("inspect reclaimed child")
                .is_none(),
            "cancelled {case} child store must no longer be openable"
        );

        let replay = services
            .run_process_session_turn(
                replay_registration,
                create_request,
                crate::TurnInput::text("replayed cancelled child turn"),
                named_turn_scope(&child_session_id, &process_id),
                cancellation,
            )
            .await
            .expect("cancelled child replay cleanup is idempotent");
        assert!(matches!(
            replay.into_tool_output().outcome,
            crate::ToolCallOutcome::Cancelled(_)
        ));
        assert!(
            factory
                .open_existing_store_by_id(&child_session_id)
                .await
                .expect("inspect replayed reclaimed child")
                .is_none(),
            "cancelled {case} replay must not recreate the child store"
        );
    }

    #[tokio::test]
    async fn cancelled_mid_turn_subagent_reclaims_durable_rows() {
        Box::pin(cancelled_mid_turn_subagent_reclaims_durable_child_rows(
            "mid-turn",
        ))
        .await;
    }
}
