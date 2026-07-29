use super::*;

impl RuntimeSessionServices {
    pub(in crate::runtime::session_manager::process_runners) async fn run_process_session_turn(
        &self,
        registration: crate::ProcessRegistration,
        mut create_request: crate::SessionCreateRequest,
        turn_input: crate::TurnInput,
        scoped_effect_controller: crate::ScopedEffectController<'_>,
        cancellation: tokio_util::sync::CancellationToken,
    ) -> crate::ProcessAwaitOutput {
        create_request = create_request.with_caused_by(crate::CausalRef::Process {
            process_id: registration.id.clone(),
        });
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
                return crate::ProcessAwaitOutput::from_tool_output(
                    crate::ToolCallOutput::failure(crate::ToolFailure::tool(
                        crate::ToolFailureClass::Execution,
                        "process_session_create_failed",
                        err.to_string(),
                    )),
                );
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
                let _ = self
                    .managed
                    .close_session(&self.current, &self.usage, &child_session_id)
                    .await;
                return crate::ProcessAwaitOutput::from_tool_output(
                    crate::ToolCallOutput::failure(crate::ToolFailure::tool(
                        crate::ToolFailureClass::Execution,
                        "process_session_turn_scope_failed",
                        err.to_string(),
                    )),
                );
            }
        };
        let child_scoped_effect_controller = match scoped_effect_controller.rescope(child_scope) {
            Ok(controller) => controller,
            Err(err) => {
                let _ = self
                    .managed
                    .close_session(&self.current, &self.usage, &child_session_id)
                    .await;
                return crate::ProcessAwaitOutput::from_tool_output(
                    crate::ToolCallOutput::failure(crate::ToolFailure::tool(
                        crate::ToolFailureClass::Execution,
                        "process_session_turn_scope_failed",
                        err.to_string(),
                    )),
                );
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
                let _ = self
                    .managed
                    .close_session(&self.current, &self.usage, &child_session_id)
                    .await;
                return crate::ProcessAwaitOutput::from_tool_output(
                    crate::ToolCallOutput::failure(crate::ToolFailure::tool(
                        crate::ToolFailureClass::Execution,
                        "process_session_turn_scope_failed",
                        err.to_string(),
                    )),
                );
            }
        };
        let mut turn = Box::pin(self.managed.start_turn(&self.current, &self.usage, request));
        let outcome = tokio::select! {
            _ = cancellation.cancelled() => None,
            outcome = turn.as_mut() => Some(outcome),
        };
        let Some(outcome) = outcome else {
            // Dropping the managed-turn future aborts its inherited task-local
            // execution before this outer process reacquires the shared slot.
            drop(turn);
            crate::runtime::process_worker::ensure_process_execution_permit().await;
            let _ = self
                .managed
                .close_session(&self.current, &self.usage, &child_session_id)
                .await;
            return crate::ProcessAwaitOutput::from_tool_output(crate::ToolCallOutput::cancelled(
                crate::ToolCancellation::runtime("background session turn was cancelled"),
            ));
        };
        let _ = self
            .managed
            .close_session(&self.current, &self.usage, &child_session_id)
            .await;
        match outcome {
            Ok(turn) => {
                let state = process_terminal_state_for_turn(&turn);
                crate::ProcessAwaitOutput::from_tool_output(output_from_process_turn(
                    &registration,
                    &child_session_id,
                    turn,
                    state,
                ))
            }
            Err(err) => crate::ProcessAwaitOutput::from_tool_output(
                crate::ToolCallOutput::failure(crate::ToolFailure::tool(
                    crate::ToolFailureClass::Execution,
                    "process_session_turn_failed",
                    err.to_string(),
                )),
            ),
        }
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

fn process_terminal_state_for_turn(turn: &crate::AssembledTurn) -> crate::ProcessStatus {
    match &turn.outcome {
        crate::TurnOutcome::Finished(_) | crate::TurnOutcome::AgentFrameSwitch { .. } => {
            crate::ProcessStatus::Completed
        }
        crate::TurnOutcome::Stopped(crate::TurnStop::Cancelled) => crate::ProcessStatus::Cancelled,
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
