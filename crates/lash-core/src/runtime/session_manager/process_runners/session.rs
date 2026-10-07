use super::*;

impl RuntimeSessionServices {
    /// Mail a `ProcessInput::SessionTurn`'s turn (FIG-5208): initialize the
    /// recorded child session, or reopen the one an earlier pass created,
    /// and accept the turn's input into it under the child turn's id. The
    /// child's session actor runs the turn; the process waits for its end.
    ///
    /// A request this deployment can never run answers
    /// [`SessionTurnMail::Refused`](lash_core_execution::runtime::actor::process::SessionTurnMail)
    /// with the process's terminal failure; every other failure is an
    /// error another pass may not meet.
    pub(in crate::runtime) async fn mail_process_session_turn(
        &self,
        process_id: &crate::ProcessId,
        create_request: crate::SessionCreateRequest,
        turn_input: crate::TurnInput,
    ) -> Result<lash_core_execution::runtime::actor::process::SessionTurnMail, crate::PluginError>
    {
        let create_request = self.child_create_request(process_id, create_request);
        let turn_id = crate::runtime::process_session_turn_id(process_id);
        match Box::pin(self.initialize_session_and_mail_turn(
            create_request,
            process_id,
            &turn_id,
            turn_input,
        ))
        .await
        {
            Ok(_) => Ok(lash_core_execution::runtime::actor::process::SessionTurnMail::Mailed),
            // A recorded request this deployment cannot initialize fails
            // deterministically: no pass can run it, so the refusal is a
            // terminal failure, not an error another pass retries forever.
            Err(session_init::SessionTurnInitError::Refused { source }) => Ok(
                lash_core_execution::runtime::actor::process::SessionTurnMail::Refused(
                    crate::ProcessAwaitOutput::from_tool_output(crate::ToolCallOutput::failure(
                        crate::ToolFailure::tool(
                            crate::ToolFailureClass::Execution,
                            "process_session_turn_refused",
                            source.to_string(),
                        ),
                    )),
                ),
            ),
            Err(session_init::SessionTurnInitError::Create { session_id, source }) => {
                if let Some(session_id) = session_id {
                    tracing::debug!(
                        process_id = %process_id,
                        session_id = %session_id,
                        "a process's child session may be retained by a failed mail"
                    );
                }
                Err(*source)
            }
        }
    }

    /// Stop a `ProcessInput::SessionTurn`'s child turn: withdraw its input
    /// while no run has taken it, or else request the run's cancel, which
    /// the child's session actor honours.
    pub(in crate::runtime) async fn cancel_process_session_turn(
        &self,
        process_id: &crate::ProcessId,
        create_request: crate::SessionCreateRequest,
        requester: Option<String>,
    ) -> Result<lash_core_execution::runtime::actor::process::SessionTurnCancel, crate::PluginError>
    {
        use lash_core_execution::runtime::actor::process::SessionTurnCancel;
        let session_id = self
            .child_create_request(process_id, create_request)
            .session_id;
        let turn_id = crate::runtime::process_session_turn_id(process_id);
        let run = self
            .withdraw_process_child_inputs(session_id.as_ref(), process_id, &turn_id)
            .await?;
        let Some(session_id) = session_id else {
            return Ok(SessionTurnCancel::Withdrawn);
        };
        let request = crate::TurnCancelRequest::new(
            crate::TurnAddress::new(session_id.clone(), run.unwrap_or_else(|| turn_id.clone())),
            format!("process-cancel:{process_id}"),
            requester,
        );
        let receipt = crate::TurnWorkDriver::new(self.current.host.core.backend().clone())
            .request_cancel(request)
            .await
            .map_err(crate::PluginError::Runtime)?;
        Ok(match receipt.outcome {
            crate::TurnCancelOutcome::Requested(_)
            | crate::TurnCancelOutcome::Escalated(_)
            | crate::TurnCancelOutcome::AlreadyRequested(_)
            | crate::TurnCancelOutcome::PolicyConflict { .. } => SessionTurnCancel::Requested,
            // The run is not open: it ended, and its end resolved the
            // process's wait, or no run ever took the input, which the
            // withdrawal above cancelled.
            _ => match self.child_run_terminal(&session_id, &turn_id).await? {
                Some(_) => SessionTurnCancel::Ended,
                None => SessionTurnCancel::Withdrawn,
            },
        })
    }

    /// A `ProcessInput::SessionTurn`'s answer once its child turn ended: the
    /// turn's committed end, as the child session's store records it,
    /// projected onto what `result` asks for.
    pub(in crate::runtime) async fn process_session_turn_outcome(
        &self,
        process_id: &crate::ProcessId,
        create_request: crate::SessionCreateRequest,
        result: &crate::SessionTurnOutcome,
    ) -> Result<crate::ProcessOutcome, crate::PluginError> {
        let session_id = self
            .child_create_request(process_id, create_request)
            .session_id
            .ok_or_else(|| {
                crate::PluginError::Session(format!(
                    "process `{process_id}` names no child session"
                ))
            })?;
        let turn_id = crate::runtime::process_session_turn_id(process_id);
        let cause = self
            .child_run_terminal(&session_id, &turn_id)
            .await?
            .ok_or_else(|| {
                crate::PluginError::Session(format!(
                    "process `{process_id}`'s child turn in `{session_id}` has not ended"
                ))
            })?;
        let outcome = match cause {
            crate::store::RunTerminalCause::Committed { outcome, .. } => {
                crate::TurnOutcome::from(outcome)
            }
            crate::store::RunTerminalCause::Cancelled { evidence } => {
                crate::TurnOutcome::Stopped(crate::TurnStop::Cancelled { evidence })
            }
            crate::store::RunTerminalCause::Refused { code, message, .. } => {
                return Ok(crate::ProcessAwaitOutput::from_tool_output(
                    crate::ToolCallOutput::failure(crate::ToolFailure::tool(
                        crate::ToolFailureClass::Execution,
                        "process_session_turn_refused",
                        format!("{code}: {message}"),
                    )),
                ));
            }
            other => {
                return Err(crate::PluginError::Session(format!(
                    "process `{process_id}`'s child turn ended without an outcome: {other:?}"
                )));
            }
        };
        let turn = self.child_turn(&session_id, outcome).await?;
        let state = process_terminal_state_for_turn(&turn);
        Ok(crate::ProcessAwaitOutput::from_tool_output(
            output_from_process_turn(process_id, &session_id, turn, state, result),
        ))
    }

    /// The child session's create request as the process runs it: caused by
    /// the process, in the session its id derives when the start named none
    /// (ADR 0107), under the model its starter's environment recorded.
    fn child_create_request(
        &self,
        process_id: &crate::ProcessId,
        create_request: crate::SessionCreateRequest,
    ) -> crate::SessionCreateRequest {
        let mut create_request = create_request.with_caused_by(crate::CausalRef::Process {
            process_id: process_id.clone(),
        });
        if create_request.session_id.is_none() {
            create_request = create_request
                .with_session_id(crate::runtime::process_child_session_id(process_id));
        }
        // The child is resolved against the environment this process's start
        // captured — its starter's recorded policy and plugin config — the
        // facts the start admitted before its handoff (FIG-4396). A
        // `create_request` policy that selects no model runs that
        // environment's recorded one, copied as recorded rather than
        // re-resolved.
        self.inherit_session_turn_llm_profile(&mut create_request);
        create_request
    }

    /// The child session's store.
    async fn child_store(
        &self,
        session_id: &SessionId,
    ) -> Result<crate::store::SessionStore, crate::PluginError> {
        let factory = self.current.host.core.session_store_factory();
        crate::runtime::live_session_view(&factory, session_id)
            .await
            .map_err(|error| {
                crate::PluginError::of_store_error(
                    format_args!("failed to open child session `{session_id}`"),
                    error,
                )
            })?
            .ok_or_else(|| {
                crate::PluginError::Session(format!("child session `{session_id}` is not live"))
            })
    }

    /// How the child's run `turn_id` ended, if it did.
    async fn child_run_terminal(
        &self,
        session_id: &SessionId,
        turn_id: &crate::TurnId,
    ) -> Result<Option<crate::store::RunTerminalCause>, crate::PluginError> {
        let store = self.child_store(session_id).await?;
        let terminal = store.run_terminal(turn_id).await.map_err(|error| {
            crate::PluginError::of_store_error(
                format_args!("failed to read child run `{turn_id}` of `{session_id}`"),
                error,
            )
        })?;
        Ok(terminal.map(|terminal| terminal.cause))
    }

    /// The child turn as its commit left it: the session's committed head
    /// and the turn's committed outcome.
    async fn child_turn(
        &self,
        session_id: &SessionId,
        outcome: crate::TurnOutcome,
    ) -> Result<crate::AssembledTurn, crate::PluginError> {
        let store = self.child_store(session_id).await?;
        let state =
            crate::store::load_session_window_state(&store, crate::store::WindowSelector::Current)
                .await
                .map_err(|error| {
                    crate::PluginError::of_store_error(
                        format_args!("failed to load child session `{session_id}`"),
                        error,
                    )
                })?
                .map(|loaded| loaded.state)
                .ok_or_else(|| {
                    crate::PluginError::Session(format!(
                        "child session `{session_id}` has no committed head"
                    ))
                })?;
        let text = match &outcome {
            crate::TurnOutcome::Finished(crate::TurnFinish::AssistantMessage { text }) => {
                Some(text.clone())
            }
            _ => None,
        };
        Ok(crate::AssembledTurn {
            state: state.to_snapshot(),
            outcome,
            assistant_output: crate::AssistantOutput {
                state: if text.is_some() {
                    crate::OutputState::Usable
                } else {
                    crate::OutputState::EmptyOutput
                },
                safe_text: text.clone().unwrap_or_default(),
                raw_text: text.unwrap_or_default(),
            },
            execution: Default::default(),
            token_usage: Default::default(),
            llm_calls: Vec::new(),
            tool_calls: Vec::new(),
            omitted: None,
            retained_outputs: Vec::new(),
            failure_evidence: Vec::new(),
            errors: Vec::new(),
            turn_input_acceptance: None,
            turn_cancel_input_outcome: Default::default(),
        })
    }

    fn inherit_session_turn_llm_profile(&self, create_request: &mut crate::SessionCreateRequest) {
        let Some(policy) = create_request.policy.as_mut() else {
            return;
        };
        if policy.model.is_none() {
            policy.model = self.current.policy.model.clone();
        }
    }
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

/// Classify a non-cancelled child stop for the parent.
///
/// The `code` is the stop's own spelling, so a parent can tell a provider
/// error from a refusal without reading prose; the sentence is the fallback
/// message for a stop whose child authored no text of its own.
fn process_turn_stop_classification(
    stop: &crate::TurnStop,
) -> (crate::ToolFailureClass, &'static str, &'static str) {
    use crate::ToolFailureClass as Class;
    match stop {
        // Cancellation never reaches here: `output_from_process_turn` settles a
        // cancelled child before classifying a failure.
        crate::TurnStop::Cancelled { .. } => (
            Class::Execution,
            "process_session_turn_cancelled",
            "background session turn was cancelled",
        ),
        crate::TurnStop::Incomplete => (
            Class::Execution,
            "process_session_turn_incomplete",
            "background session turn ended before producing a result",
        ),
        crate::TurnStop::InvalidInput => (
            Class::InvalidRequest,
            "process_session_turn_invalid_input",
            "background session turn input was refused",
        ),
        crate::TurnStop::MaxTurns => (
            Class::ResourceLimit,
            "process_session_turn_max_turns",
            "background session turn reached its turn limit",
        ),
        crate::TurnStop::ToolFailure => (
            Class::Execution,
            "process_session_turn_tool_failure",
            "background session turn stopped on a failed tool call",
        ),
        crate::TurnStop::ProviderError => (
            Class::External,
            "process_session_turn_provider_error",
            "background session turn stopped on a provider error",
        ),
        crate::TurnStop::ContextOverflow => (
            Class::ResourceLimit,
            "process_session_turn_context_overflow",
            "background session turn exceeded the model's context window",
        ),
        crate::TurnStop::PluginAbort => (
            Class::Execution,
            "process_session_turn_plugin_abort",
            "background session turn was aborted by a plugin",
        ),
        crate::TurnStop::RuntimeError => (
            Class::Internal,
            "process_session_turn_runtime_error",
            "background session turn stopped on a runtime error",
        ),
        crate::TurnStop::SubmittedError { .. } => (
            Class::Execution,
            "process_session_turn_submitted_error",
            "background session turn submitted a failure without a reason",
        ),
        crate::TurnStop::ToolError { .. } => (
            Class::Execution,
            "process_session_turn_tool_error",
            "background session turn stopped on a tool error without a message",
        ),
    }
}

/// The child's own text for a stop that carries one.
///
/// `submit_error` and every other `task.fail` spelling land as a projected
/// [`crate::ToolFailure`], whose human reason is `message`; a value that
/// carries a bare `reason` string (a host-authored `SubmittedError`) is read
/// from that field. Nothing is invented: a stop with no authored text yields
/// `None` and the caller falls back to the stop's own sentence.
fn authored_stop_text(value: &serde_json::Value) -> Option<String> {
    ["reason", "message"]
        .into_iter()
        .filter_map(|field| value.get(field))
        .filter_map(serde_json::Value::as_str)
        .map(str::trim)
        .find(|text| !text.is_empty())
        .map(ToOwned::to_owned)
}

/// The child turn's first blocking issue: the root cause the assembler
/// recorded, ahead of any consequence it recorded afterwards.
fn first_blocking_issue(turn: &crate::AssembledTurn) -> Option<&crate::TurnIssue> {
    turn.errors
        .iter()
        .find(|issue| issue.severity == crate::TurnIssueSeverity::Blocking)
}

/// Project a failed child turn onto the failure the parent's spawn result,
/// the child's process record and its terminal process event all carry.
///
/// The child's own reason is the message wherever the child authored one, the
/// stop's code keeps the categories apart, and the typed diagnostics — the
/// child's [`crate::TurnFailureKind`]/[`crate::TurnFailureCode`] and the
/// stop's projected value — ride the existing `raw` channel, bounded.
fn failure_from_process_turn(turn: &crate::AssembledTurn) -> crate::ToolFailure {
    let crate::TurnOutcome::Stopped(stop) = &turn.outcome else {
        return crate::ToolFailure::tool(
            crate::ToolFailureClass::Internal,
            "process_session_turn_failed",
            "background session turn failed",
        );
    };
    let (class, code, sentence) = process_turn_stop_classification(stop);
    let issue = first_blocking_issue(turn);
    let (authored, stop_value) = match stop {
        crate::TurnStop::SubmittedError { value } | crate::TurnStop::ToolError { value, .. } => {
            (authored_stop_text(value), Some(value))
        }
        _ => (
            issue
                .map(|issue| issue.message.trim())
                .filter(|message| !message.is_empty())
                .map(ToOwned::to_owned),
            None,
        ),
    };
    let message = match (authored, stop_value.is_some()) {
        // A child that authored its own terminal reason reaches the parent
        // verbatim: the parent model reads the child's words, not ours.
        (Some(text), true) => text,
        // A category stop has no child-authored terminal text, so the
        // assembler's blocking issue qualifies the stop's own sentence.
        (Some(text), false) => format!("{sentence}: {text}"),
        (None, _) => sentence.to_string(),
    };
    let mut failure = crate::ToolFailure::tool(
        class,
        code,
        lash_sansio::session_model::truncate_raw_error(&message),
    );
    failure.raw = process_turn_failure_raw(stop_value, issue).map(crate::ToolValue::untrusted_json);
    failure
}

/// Bounded diagnostics for a failed child turn, or `None` when the child
/// produced neither a projected stop value nor a blocking issue.
fn process_turn_failure_raw(
    stop_value: Option<&serde_json::Value>,
    issue: Option<&crate::TurnIssue>,
) -> Option<serde_json::Value> {
    let mut raw = serde_json::Map::new();
    if let Some(value) = stop_value {
        raw.insert(
            "stop".to_string(),
            serde_json::Value::String(lash_sansio::session_model::truncate_raw_error(
                &value.to_string(),
            )),
        );
    }
    if let Some(issue) = issue {
        raw.insert("kind".to_string(), issue.kind.as_str().into());
        if let Some(code) = issue.code.as_ref() {
            raw.insert("code".to_string(), code.namespaced().into());
        }
        if let Some(retryable) = issue.retryable {
            raw.insert("retryable".to_string(), retryable.into());
        }
        raw.insert(
            "issue".to_string(),
            serde_json::Value::String(lash_sansio::session_model::truncate_raw_error(
                issue.message.trim(),
            )),
        );
    }
    (!raw.is_empty()).then_some(serde_json::Value::Object(raw))
}

/// Project the child's ended turn onto the process's answer.
///
/// A cancelled or failed child answers its own cancellation or failure under
/// every [`crate::SessionTurnOutcome`]. A finished child answers its assembled
/// turn under `Turn`, and its final value under `FinalValue`.
fn output_from_process_turn(
    process_id: &crate::ProcessId,
    child_session_id: &SessionId,
    turn: crate::AssembledTurn,
    state: crate::ProcessStatus,
    result: &crate::SessionTurnOutcome,
) -> crate::ToolCallOutput {
    if state == crate::ProcessStatus::Cancelled {
        let cancellation = match &turn.outcome {
            crate::TurnOutcome::Stopped(crate::TurnStop::Cancelled { evidence }) => {
                crate::ToolCancellation {
                    message: evidence
                        .reason
                        .clone()
                        .unwrap_or_else(|| "background session turn was cancelled".to_string()),
                    source: crate::ToolFailureSource::Cancellation,
                    origin: Some(crate::CancelOrigin::TurnStopped),
                    raw: serde_json::to_value(evidence)
                        .ok()
                        .map(crate::ToolValue::untrusted_json),
                    forced: false,
                }
            }
            _ => crate::ToolCancellation::runtime("background session turn was cancelled"),
        };
        return crate::ToolCallOutput::cancelled(cancellation);
    }
    if state == crate::ProcessStatus::Failed {
        return crate::ToolCallOutput::failure(failure_from_process_turn(&turn));
    }
    match result {
        crate::SessionTurnOutcome::Turn => crate::ToolCallOutput::success(serde_json::json!({
            "process_id": process_id,
            "child_session_id": child_session_id,
            "turn": turn,
        })),
        crate::SessionTurnOutcome::FinalValue { schema } => {
            match final_value_of_turn(&turn)
                .and_then(|value| checked_final_value(value, schema.as_ref()))
            {
                Ok(value) => crate::ToolCallOutput::success(value),
                Err(failure) => crate::ToolCallOutput::failure(*failure),
            }
        }
    }
}

/// The value a finished child answers under `FinalValue`: its final value, the
/// value a terminal tool finished it with, or its trimmed assistant text.
///
/// A child that switched agent frames or stopped has no final value, and says
/// so as a typed failure rather than as an empty success.
fn final_value_of_turn(
    turn: &crate::AssembledTurn,
) -> Result<serde_json::Value, Box<crate::ToolFailure>> {
    match &turn.outcome {
        crate::TurnOutcome::Finished(crate::TurnFinish::FinalValue { value })
        | crate::TurnOutcome::Finished(crate::TurnFinish::ToolValue { value, .. }) => {
            Ok(value.clone())
        }
        crate::TurnOutcome::Finished(crate::TurnFinish::AssistantMessage { text }) => {
            let text = [
                text.as_str(),
                turn.assistant_output.safe_text.as_str(),
                turn.assistant_output.raw_text.as_str(),
            ]
            .into_iter()
            .map(str::trim)
            .find(|text| !text.is_empty())
            .unwrap_or_default();
            Ok(serde_json::Value::String(text.to_string()))
        }
        crate::TurnOutcome::AgentFrameSwitch { .. } => Err(Box::new(crate::ToolFailure::tool(
            crate::ToolFailureClass::Execution,
            "process_session_turn_frame_switch",
            "the child switched agent frames instead of producing a final value",
        ))),
        crate::TurnOutcome::Stopped(_) => Err(Box::new(crate::ToolFailure::tool(
            crate::ToolFailureClass::Internal,
            "process_session_turn_stopped",
            "the child turn stopped without producing a final value",
        ))),
    }
}

/// Checks the child's final value against the caller's declared schema. The
/// child session has ended, so a mismatch fails the call rather than asking
/// the child to repair it.
fn checked_final_value(
    value: serde_json::Value,
    schema: Option<&crate::JsonSchema>,
) -> Result<serde_json::Value, Box<crate::ToolFailure>> {
    let Some(schema) = schema else {
        return Ok(value);
    };
    schema.validate(&value).map(|()| value).map_err(|error| {
        Box::new(
            crate::ToolFailure::tool(
                crate::ToolFailureClass::Execution,
                "process_session_turn_result_schema_mismatch",
                format!(
                    "the child's final value did not match the declared output schema: {error}"
                ),
            )
            .with_cause(crate::ToolFailureCause::ValueMismatch { source: error }),
        )
    })
}

#[cfg(test)]
mod tests;
