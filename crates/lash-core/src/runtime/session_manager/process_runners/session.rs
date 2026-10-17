use super::*;

impl RuntimeSessionServices {
    /// Mail a `ProcessInput::SessionTurn`'s turn (FIG-5208): initialize the
    /// recorded child session against the deployment's session plugin host,
    /// or reopen the one an earlier pass created, and accept the turn's
    /// input into it under the child turn's id. The child's session actor
    /// runs the turn; the process waits for its end.
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
        parent: Option<lash_trace::TraceParent>,
    ) -> Result<lash_core_execution::runtime::actor::process::SessionTurnMail, crate::PluginError>
    {
        let create_request = self.child_create_request(process_id, create_request);
        let turn_id = crate::runtime::process_session_turn_id(process_id);
        match Box::pin(self.initialize_session_and_mail_turn(
            create_request,
            process_id,
            &turn_id,
            turn_input,
            parent,
        ))
        .await
        {
            Ok(_) => Ok(lash_core_execution::runtime::actor::process::SessionTurnMail::Mailed),
            // A recorded request this deployment cannot initialize fails
            // deterministically: no pass can run it, so the refusal is a
            // terminal failure, not an error another pass retries forever.
            // The store's own refusal keeps its code, as a session's open
            // answers its sender with it (FIG-5358).
            Err(session_init::SessionTurnInitError::Refused { source }) => Ok(
                lash_core_execution::runtime::actor::process::SessionTurnMail::Refused(
                    match session_init::store_refusal(&source) {
                        Some(refusal) => refused_output(refusal.code.as_str(), refusal.message),
                        None => refused_output("process_session_turn_refused", source.to_string()),
                    },
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
            // No run had taken the input: the cancel withdrew it.
            crate::TurnCancelOutcome::Withdrawn { .. } => SessionTurnCancel::Withdrawn,
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
        // The store's refusal of the child's committed end is met again by
        // every pass: the process ends with it (FIG-5358).
        match self
            .child_turn_outcome(process_id, create_request, result)
            .await
        {
            Err(error) => match session_init::store_refusal(&error) {
                Some(refusal) => Ok(refused_output(refusal.code.as_str(), refusal.message)),
                None => Err(error),
            },
            outcome => outcome,
        }
    }

    async fn child_turn_outcome(
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
            crate::store::RunTerminalCause::Cancelled { evidence, .. } => {
                crate::TurnOutcome::Stopped(crate::TurnStop::Cancelled { evidence })
            }
            // The child run's refusal answers its process under its own
            // code, as it answers the run's inputs (FIG-5358).
            crate::store::RunTerminalCause::Refused { refusal } => {
                return Ok(refused_output(refusal.code.as_str(), refusal.message));
            }
            other => {
                return Err(crate::PluginError::Session(format!(
                    "process `{process_id}`'s child turn ended without an outcome: {other:?}"
                )));
            }
        };
        Ok(crate::ProcessAwaitOutput::from_tool_output(
            output_from_process_turn(
                process_id,
                &session_id,
                &turn_id,
                outcome,
                result,
                self.current.host.core.tracing.diagnostic_limits(),
                self.current.host.core.control.output_cuts,
            ),
        ))
    }

    /// The child session's create request as the process runs it: caused by
    /// the process, and in the session its id derives when the start named
    /// none (ADR 0107). Everything else is what its creator stated: nothing
    /// is filled from the starter's environment (ADR 0134).
    fn child_create_request(
        &self,
        process_id: &crate::ProcessId,
        create_request: crate::SessionCreateRequest,
    ) -> crate::SessionCreateRequest {
        let session = crate::runtime::process_session_id(process_id, &create_request);
        create_request
            .with_caused_by(crate::CausalRef::Process {
                process_id: process_id.clone(),
            })
            .with_session_id(session)
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
}

fn process_terminal_state_for_turn(outcome: &crate::TurnOutcome) -> crate::ProcessStatus {
    match outcome {
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
        crate::TurnStop::ToolPanicked { .. } => (
            Class::Internal,
            "tool_panicked",
            "a host tool panicked in the child turn",
        ),
        crate::TurnStop::ToolFailure => (
            Class::Execution,
            "process_session_turn_tool_failure",
            "background session turn stopped on a failed tool call",
        ),
        crate::TurnStop::ProviderError { .. } => (
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
        crate::TurnStop::AgentFrameSwitchLimit => (
            Class::ResourceLimit,
            "process_session_turn_agent_frame_switch_limit",
            "background session turn reached its agent frame switch limit",
        ),
        crate::TurnStop::RuntimeError { .. } => (
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

/// Project a failed child turn onto the failure the parent's spawn result,
/// the child's process record and its terminal process event all carry.
///
/// The child's own reason is the message wherever the child authored one, the
/// stop's code keeps the categories apart, and the typed diagnostics — the
/// child's [`crate::TurnErrorCode`] and the
/// stop's projected value — ride the existing `raw` channel, bounded.
fn failure_from_process_turn(
    outcome: &crate::TurnOutcome,
    limits: lash_core_store::execution_limits::DiagnosticLimits,
    cuts: lash_protocol_types::session_model::RuntimeOutputCuts,
) -> crate::ToolFailure {
    let crate::TurnOutcome::Stopped(stop) = &outcome else {
        return crate::ToolFailure::tool(
            crate::ToolFailureClass::Internal,
            "process_session_turn_failed",
            "background session turn failed",
        );
    };
    let (class, code, sentence) = process_turn_stop_classification(stop);
    let cause = outcome.error_cause();
    let (authored, stop_value) = match stop {
        crate::TurnStop::SubmittedError { value } | crate::TurnStop::ToolError { value, .. } => {
            (authored_stop_text(value), Some(value))
        }
        _ => (
            cause
                .and_then(|cause| cause.detail())
                .map(str::trim)
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
        // committed cause qualifies the stop's own sentence.
        (Some(text), false) => format!("{sentence}: {text}"),
        (None, _) => sentence.to_string(),
    };
    let mut failure = crate::ToolFailure::tool(
        class,
        code,
        lash_protocol_types::session_model::truncate_raw_error(&message, cuts.raw_error_max_chars),
    );
    failure.raw =
        process_turn_failure_raw(stop, cause, limits).map(crate::ToolValue::untrusted_json);
    failure
}

/// Bounded stop diagnostics, with the committed cause and permitted detail when present.
fn process_turn_failure_raw(
    stop: &crate::TurnStop,
    cause: Option<&lash_protocol_types::TurnErrorCause>,
    limits: lash_core_store::execution_limits::DiagnosticLimits,
) -> Option<serde_json::Value> {
    let mut raw = serde_json::Map::new();
    raw.insert(
        "stop".into(),
        limits
            .diagnostic_error(&serde_json::json!(stop).to_string())
            .into(),
    );
    if let Some(cause) = cause {
        raw.insert("cause".into(), cause.code.code().into());
        raw.insert("detail".into(), serde_json::json!(cause.detail()));
    }
    (!raw.is_empty()).then_some(serde_json::Value::Object(raw))
}

/// Project the child's ended turn onto the process's answer.
///
/// A cancelled or failed child answers its own cancellation or failure under
/// every [`crate::SessionTurnOutcome`]. A finished child answers its committed
/// outcome and identity under `Turn`, and its final value under `FinalValue`.
fn output_from_process_turn(
    process_id: &crate::ProcessId,
    child_session_id: &SessionId,
    turn_id: &crate::TurnId,
    outcome: crate::TurnOutcome,
    result: &crate::SessionTurnOutcome,
    limits: lash_core_store::execution_limits::DiagnosticLimits,
    cuts: lash_protocol_types::session_model::RuntimeOutputCuts,
) -> crate::ToolCallOutput {
    let state = process_terminal_state_for_turn(&outcome);
    if state == crate::ProcessStatus::Cancelled {
        let cancellation = match &outcome {
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
    if let crate::TurnOutcome::Stopped(crate::TurnStop::ToolPanicked {
        tool_name,
        call_id,
        message,
    }) = &outcome
    {
        return crate::ToolCallOutput::failure(crate::ToolFailure::runtime(
            crate::ToolFailureClass::Internal, "tool_panicked",
            "The tool panicked. Outside work may already have happened; check outside state before calling again.",
        ).with_cause(crate::ToolFailureCause::Panicked {
            tool_name: tool_name.clone(), call_id: call_id.clone(), message: message.clone(),
        }));
    }
    if state == crate::ProcessStatus::Failed {
        return crate::ToolCallOutput::failure(failure_from_process_turn(&outcome, limits, cuts));
    }
    match result {
        crate::SessionTurnOutcome::Turn => crate::ToolCallOutput::success(serde_json::json!({
            "process_id": process_id,
            "child_session_id": child_session_id,
            "turn_id": turn_id,
            "outcome": outcome,
        })),
        crate::SessionTurnOutcome::FinalValue { schema } => {
            match final_value_of_turn(&outcome)
                .and_then(|value| checked_final_value(value, schema.as_ref()))
            {
                Ok(value) => crate::ToolCallOutput::success(value),
                Err(failure) => crate::ToolCallOutput::failure(*failure),
            }
        }
    }
}

/// The value a finished child answers under `FinalValue`: the value a
/// declared Finish control ended its turn with, or its unmodified assistant
/// text.
///
/// A child that switched agent frames or stopped has no final value, and says
/// so as a typed failure rather than as an empty success.
fn final_value_of_turn(
    outcome: &crate::TurnOutcome,
) -> Result<serde_json::Value, Box<crate::ToolFailure>> {
    match outcome {
        crate::TurnOutcome::Finished(crate::TurnFinish::Finished { value, .. }) => {
            Ok(value.clone())
        }
        crate::TurnOutcome::Finished(crate::TurnFinish::AssistantMessage { text }) => {
            Ok(serde_json::Value::String(text.clone()))
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

/// A session-turn process's refused end, failed under `code`.
fn refused_output(code: &str, message: String) -> crate::ProcessAwaitOutput {
    crate::ProcessAwaitOutput::from_tool_output(crate::ToolCallOutput::failure(
        crate::ToolFailure::tool(crate::ToolFailureClass::Execution, code, message),
    ))
}

#[cfg(test)]
mod tests;
