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
        process_id: &lash_vocabulary::identity::ProcessId,
        create_request: lash_execution_contracts::session_types::SessionCreateRequest,
        turn_input: lash_core_store::turn_input_vocabulary::TurnInput,
        parent: Option<lash_trace::TraceParent>,
    ) -> Result<
        lash_core_execution::runtime::actor::process::SessionTurnMail,
        lash_execution_contracts::plugin_error::PluginError,
    > {
        let create_request = self.child_create_request(process_id, create_request);
        let turn_id = lash_execution_contracts::process::process_session_turn_id(process_id);
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
        process_id: &lash_vocabulary::identity::ProcessId,
        create_request: lash_execution_contracts::session_types::SessionCreateRequest,
        requester: Option<String>,
    ) -> Result<
        lash_core_execution::runtime::actor::process::SessionTurnCancel,
        lash_execution_contracts::plugin_error::PluginError,
    > {
        use lash_core_execution::runtime::actor::process::SessionTurnCancel;
        let session_id = self
            .child_create_request(process_id, create_request)
            .session_id;
        let turn_id = lash_execution_contracts::process::process_session_turn_id(process_id);
        let run = self
            .withdraw_process_child_inputs(session_id.as_ref(), process_id, &turn_id)
            .await?;
        let Some(session_id) = session_id else {
            return Ok(SessionTurnCancel::Withdrawn);
        };
        let request = lash_core_store::turn_control_vocabulary::TurnCancelRequest::new(
            lash_core_store::turn_control_vocabulary::TurnAddress::new(
                session_id.clone(),
                run.unwrap_or_else(|| turn_id.clone()),
            ),
            format!("process-cancel:{process_id}"),
            requester,
        );
        let receipt = crate::TurnWorkDriver::new(self.current.host.core.backend().clone())
            .request_cancel(request)
            .await
            .map_err(lash_execution_contracts::plugin_error::PluginError::Runtime)?;
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
        process_id: &lash_vocabulary::identity::ProcessId,
        create_request: lash_execution_contracts::session_types::SessionCreateRequest,
        result: &lash_execution_contracts::process::SessionTurnOutcome,
    ) -> Result<
        lash_execution_contracts::process::ProcessOutcome,
        lash_execution_contracts::plugin_error::PluginError,
    > {
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
        process_id: &lash_vocabulary::identity::ProcessId,
        create_request: lash_execution_contracts::session_types::SessionCreateRequest,
        result: &lash_execution_contracts::process::SessionTurnOutcome,
    ) -> Result<
        lash_execution_contracts::process::ProcessOutcome,
        lash_execution_contracts::plugin_error::PluginError,
    > {
        let session_id = self
            .child_create_request(process_id, create_request)
            .session_id
            .ok_or_else(|| {
                lash_execution_contracts::plugin_error::PluginError::Session(format!(
                    "process `{process_id}` names no child session"
                ))
            })?;
        let turn_id = lash_execution_contracts::process::process_session_turn_id(process_id);
        let cause = self
            .child_run_terminal(&session_id, &turn_id)
            .await?
            .ok_or_else(|| {
                lash_execution_contracts::plugin_error::PluginError::Session(format!(
                    "process `{process_id}`'s child turn in `{session_id}` has not ended"
                ))
            })?;
        let outcome = match cause {
            crate::store::RunTerminalCause::Committed { outcome, .. } => {
                lash_protocol_types::session_model::TurnOutcome::from(outcome)
            }
            crate::store::RunTerminalCause::Cancelled { evidence, .. } => {
                lash_protocol_types::session_model::TurnOutcome::Stopped(
                    lash_protocol_types::session_model::TurnStop::Cancelled { evidence },
                )
            }
            // The child run's refusal answers its process under its own
            // code, as it answers the run's inputs (FIG-5358).
            crate::store::RunTerminalCause::Refused { refusal } => {
                return Ok(refused_output(refusal.code.as_str(), refusal.message));
            }
            other => {
                return Err(
                    lash_execution_contracts::plugin_error::PluginError::Session(format!(
                        "process `{process_id}`'s child turn ended without an outcome: {other:?}"
                    )),
                );
            }
        };
        Ok(
            lash_execution_contracts::process::ProcessAwaitOutput::from_tool_output(
                output_from_process_turn(
                    process_id,
                    &session_id,
                    &turn_id,
                    outcome,
                    result,
                    self.current.host.core.tracing.diagnostic_limits(),
                    self.current.host.core.control.output_cuts,
                ),
            ),
        )
    }

    /// The child session's create request as the process runs it: caused by
    /// the process, and in the session its id derives when the start named
    /// none (ADR 0107). Everything else is what its creator stated: nothing
    /// is filled from the starter's environment (ADR 0134).
    fn child_create_request(
        &self,
        process_id: &lash_vocabulary::identity::ProcessId,
        create_request: lash_execution_contracts::session_types::SessionCreateRequest,
    ) -> lash_execution_contracts::session_types::SessionCreateRequest {
        let session =
            lash_execution_contracts::process::process_session_id(process_id, &create_request);
        create_request
            .with_caused_by(lash_protocol_types::causal::CausalRef::Process {
                process_id: process_id.clone(),
            })
            .with_session_id(session)
    }

    /// The child session's store.
    async fn child_store(
        &self,
        session_id: &SessionId,
    ) -> Result<crate::store::SessionStore, lash_execution_contracts::plugin_error::PluginError>
    {
        let factory = self.current.host.core.session_store_factory();
        lash_execution_contracts::deployment::live_session_view(&factory, session_id)
            .await
            .map_err(|error| {
                lash_execution_contracts::plugin_error::PluginError::of_store_error(
                    format_args!("failed to open child session `{session_id}`"),
                    error,
                )
            })?
            .ok_or_else(|| {
                lash_execution_contracts::plugin_error::PluginError::Session(format!(
                    "child session `{session_id}` is not live"
                ))
            })
    }

    /// How the child's run `turn_id` ended, if it did.
    async fn child_run_terminal(
        &self,
        session_id: &SessionId,
        turn_id: &lash_vocabulary::identity::TurnId,
    ) -> Result<
        Option<crate::store::RunTerminalCause>,
        lash_execution_contracts::plugin_error::PluginError,
    > {
        let store = self.child_store(session_id).await?;
        let terminal = store.run_terminal(turn_id).await.map_err(|error| {
            lash_execution_contracts::plugin_error::PluginError::of_store_error(
                format_args!("failed to read child run `{turn_id}` of `{session_id}`"),
                error,
            )
        })?;
        Ok(terminal.map(|terminal| terminal.cause))
    }
}

fn process_terminal_state_for_turn(
    outcome: &lash_protocol_types::session_model::TurnOutcome,
) -> lash_execution_contracts::process::ProcessStatus {
    match outcome {
        lash_protocol_types::session_model::TurnOutcome::Finished(_)
        | lash_protocol_types::session_model::TurnOutcome::AgentFrameSwitch { .. } => {
            lash_execution_contracts::process::ProcessStatus::Completed
        }
        lash_protocol_types::session_model::TurnOutcome::Stopped(
            lash_protocol_types::session_model::TurnStop::Cancelled { .. },
        ) => lash_execution_contracts::process::ProcessStatus::Cancelled,
        lash_protocol_types::session_model::TurnOutcome::Stopped(_) => {
            lash_execution_contracts::process::ProcessStatus::Failed
        }
    }
}

/// Classify a non-cancelled child stop for the parent.
///
/// The `code` is the stop's own spelling, so a parent can tell a provider
/// error from a refusal without reading prose; the sentence is the fallback
/// message for a stop whose child authored no text of its own.
fn process_turn_stop_classification(
    stop: &lash_protocol_types::session_model::TurnStop,
) -> (
    lash_protocol_types::tool_output::ToolFailureClass,
    &'static str,
    &'static str,
) {
    use lash_protocol_types::tool_output::ToolFailureClass as Class;
    match stop {
        // Cancellation never reaches here: `output_from_process_turn` settles a
        // cancelled child before classifying a failure.
        lash_protocol_types::session_model::TurnStop::Cancelled { .. } => (
            Class::Execution,
            "process_session_turn_cancelled",
            "background session turn was cancelled",
        ),
        lash_protocol_types::session_model::TurnStop::Incomplete => (
            Class::Execution,
            "process_session_turn_incomplete",
            "background session turn ended before producing a result",
        ),
        lash_protocol_types::session_model::TurnStop::InvalidInput => (
            Class::InvalidRequest,
            "process_session_turn_invalid_input",
            "background session turn input was refused",
        ),
        lash_protocol_types::session_model::TurnStop::MaxTurns => (
            Class::ResourceLimit,
            "process_session_turn_max_turns",
            "background session turn reached its turn limit",
        ),
        lash_protocol_types::session_model::TurnStop::ToolPanicked { .. } => (
            Class::Internal,
            "tool_panicked",
            "a host tool panicked in the child turn",
        ),
        lash_protocol_types::session_model::TurnStop::ToolFailure => (
            Class::Execution,
            "process_session_turn_tool_failure",
            "background session turn stopped on a failed tool call",
        ),
        lash_protocol_types::session_model::TurnStop::ProviderError { .. } => (
            Class::External,
            "process_session_turn_provider_error",
            "background session turn stopped on a provider error",
        ),
        lash_protocol_types::session_model::TurnStop::ContextOverflow => (
            Class::ResourceLimit,
            "process_session_turn_context_overflow",
            "background session turn exceeded the model's context window",
        ),
        lash_protocol_types::session_model::TurnStop::PluginAbort => (
            Class::Execution,
            "process_session_turn_plugin_abort",
            "background session turn was aborted by a plugin",
        ),
        lash_protocol_types::session_model::TurnStop::AgentFrameSwitchLimit => (
            Class::ResourceLimit,
            "process_session_turn_agent_frame_switch_limit",
            "background session turn reached its agent frame switch limit",
        ),
        lash_protocol_types::session_model::TurnStop::RuntimeError { .. } => (
            Class::Internal,
            "process_session_turn_runtime_error",
            "background session turn stopped on a runtime error",
        ),
        lash_protocol_types::session_model::TurnStop::SubmittedError { .. } => (
            Class::Execution,
            "process_session_turn_submitted_error",
            "background session turn submitted a failure without a reason",
        ),
        lash_protocol_types::session_model::TurnStop::ToolError { .. } => (
            Class::Execution,
            "process_session_turn_tool_error",
            "background session turn stopped on a tool error without a message",
        ),
    }
}

/// The child's own text for a stop that carries one.
///
/// `submit_error` and every other `task.fail` spelling land as a projected
/// [`lash_protocol_types::tool_output::ToolFailure`], whose human reason is `message`; a value that
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
/// child's [`lash_protocol_types::session_model::TurnErrorCode`] and the
/// stop's projected value — ride the existing `raw` channel, bounded.
fn failure_from_process_turn(
    outcome: &lash_protocol_types::session_model::TurnOutcome,
    limits: lash_core_store::execution_limits::DiagnosticLimits,
    cuts: lash_protocol_types::session_model::RuntimeOutputCuts,
) -> lash_protocol_types::tool_output::ToolFailure {
    let lash_protocol_types::session_model::TurnOutcome::Stopped(stop) = &outcome else {
        return lash_protocol_types::tool_output::ToolFailure::tool(
            lash_protocol_types::tool_output::ToolFailureClass::Internal,
            "process_session_turn_failed",
            "background session turn failed",
        );
    };
    let (class, code, sentence) = process_turn_stop_classification(stop);
    let cause = outcome.error_cause();
    let (authored, stop_value) = match stop {
        lash_protocol_types::session_model::TurnStop::SubmittedError { value }
        | lash_protocol_types::session_model::TurnStop::ToolError { value, .. } => {
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
    let mut failure = lash_protocol_types::tool_output::ToolFailure::tool(
        class,
        code,
        lash_protocol_types::session_model::truncate_raw_error(&message, cuts.raw_error_max_chars),
    );
    failure.raw = process_turn_failure_raw(stop, cause, limits)
        .map(lash_protocol_types::tool_output::ToolValue::untrusted_json);
    failure
}

/// Bounded stop diagnostics, with the committed cause and permitted detail when present.
fn process_turn_failure_raw(
    stop: &lash_protocol_types::session_model::TurnStop,
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
/// every [`lash_execution_contracts::process::SessionTurnOutcome`]. A finished child answers its committed
/// outcome and identity under `Turn`, and its final value under `FinalValue`.
fn output_from_process_turn(
    process_id: &lash_vocabulary::identity::ProcessId,
    child_session_id: &SessionId,
    turn_id: &lash_vocabulary::identity::TurnId,
    outcome: lash_protocol_types::session_model::TurnOutcome,
    result: &lash_execution_contracts::process::SessionTurnOutcome,
    limits: lash_core_store::execution_limits::DiagnosticLimits,
    cuts: lash_protocol_types::session_model::RuntimeOutputCuts,
) -> lash_protocol_types::tool_output::ToolCallOutput {
    let state = process_terminal_state_for_turn(&outcome);
    if state == lash_execution_contracts::process::ProcessStatus::Cancelled {
        let cancellation = match &outcome {
            lash_protocol_types::session_model::TurnOutcome::Stopped(
                lash_protocol_types::session_model::TurnStop::Cancelled { evidence },
            ) => lash_protocol_types::tool_output::ToolCancellation {
                message: evidence
                    .reason
                    .clone()
                    .unwrap_or_else(|| "background session turn was cancelled".to_string()),
                source: lash_protocol_types::tool_output::ToolFailureSource::Cancellation,
                origin: Some(lash_protocol_types::tool_output::CancelOrigin::TurnStopped),
                raw: serde_json::to_value(evidence)
                    .ok()
                    .map(lash_protocol_types::tool_output::ToolValue::untrusted_json),
                forced: false,
            },
            _ => lash_protocol_types::tool_output::ToolCancellation::runtime(
                "background session turn was cancelled",
            ),
        };
        return lash_protocol_types::tool_output::ToolCallOutput::cancelled(cancellation);
    }
    if let lash_protocol_types::session_model::TurnOutcome::Stopped(
        lash_protocol_types::session_model::TurnStop::ToolPanicked {
            tool_name,
            call_id,
            message,
        },
    ) = &outcome
    {
        return lash_protocol_types::tool_output::ToolCallOutput::failure(lash_protocol_types::tool_output::ToolFailure::runtime(
            lash_protocol_types::tool_output::ToolFailureClass::Internal, "tool_panicked",
            "The tool panicked. Outside work may already have happened; check outside state before calling again.",
        ).with_cause(lash_protocol_types::tool_output::ToolFailureCause::Panicked {
            tool_name: tool_name.clone(), call_id: call_id.clone(), message: message.clone(),
        }));
    }
    if state == lash_execution_contracts::process::ProcessStatus::Failed {
        return lash_protocol_types::tool_output::ToolCallOutput::failure(
            failure_from_process_turn(&outcome, limits, cuts),
        );
    }
    match result {
        lash_execution_contracts::process::SessionTurnOutcome::Turn => {
            lash_protocol_types::tool_output::ToolCallOutput::success(serde_json::json!({
                "process_id": process_id,
                "child_session_id": child_session_id,
                "turn_id": turn_id,
                "outcome": outcome,
            }))
        }
        lash_execution_contracts::process::SessionTurnOutcome::FinalValue { schema } => {
            match final_value_of_turn(&outcome)
                .and_then(|value| checked_final_value(value, schema.as_ref()))
            {
                Ok(value) => lash_protocol_types::tool_output::ToolCallOutput::success(value),
                Err(failure) => lash_protocol_types::tool_output::ToolCallOutput::failure(*failure),
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
    outcome: &lash_protocol_types::session_model::TurnOutcome,
) -> Result<serde_json::Value, Box<lash_protocol_types::tool_output::ToolFailure>> {
    match outcome {
        lash_protocol_types::session_model::TurnOutcome::Finished(
            lash_protocol_types::session_model::TurnFinish::Finished { value, .. },
        ) => Ok(value.clone()),
        lash_protocol_types::session_model::TurnOutcome::Finished(
            lash_protocol_types::session_model::TurnFinish::AssistantMessage { text },
        ) => Ok(serde_json::Value::String(text.clone())),
        lash_protocol_types::session_model::TurnOutcome::AgentFrameSwitch { .. } => Err(Box::new(
            lash_protocol_types::tool_output::ToolFailure::tool(
                lash_protocol_types::tool_output::ToolFailureClass::Execution,
                "process_session_turn_frame_switch",
                "the child switched agent frames instead of producing a final value",
            ),
        )),
        lash_protocol_types::session_model::TurnOutcome::Stopped(_) => Err(Box::new(
            lash_protocol_types::tool_output::ToolFailure::tool(
                lash_protocol_types::tool_output::ToolFailureClass::Internal,
                "process_session_turn_stopped",
                "the child turn stopped without producing a final value",
            ),
        )),
    }
}

/// Checks the child's final value against the caller's declared schema. The
/// child session has ended, so a mismatch fails the call rather than asking
/// the child to repair it.
fn checked_final_value(
    value: serde_json::Value,
    schema: Option<&lash_vocabulary::schema_contract::JsonSchema>,
) -> Result<serde_json::Value, Box<lash_protocol_types::tool_output::ToolFailure>> {
    let Some(schema) = schema else {
        return Ok(value);
    };
    schema.validate(&value).map(|()| value).map_err(|error| {
        Box::new(
            lash_protocol_types::tool_output::ToolFailure::tool(
                lash_protocol_types::tool_output::ToolFailureClass::Execution,
                "process_session_turn_result_schema_mismatch",
                format!(
                    "the child's final value did not match the declared output schema: {error}"
                ),
            )
            .with_cause(
                lash_protocol_types::tool_output::ToolFailureCause::ValueMismatch { source: error },
            ),
        )
    })
}

/// A session-turn process's refused end, failed under `code`.
fn refused_output(
    code: &str,
    message: String,
) -> lash_execution_contracts::process::ProcessAwaitOutput {
    lash_execution_contracts::process::ProcessAwaitOutput::from_tool_output(
        lash_protocol_types::tool_output::ToolCallOutput::failure(
            lash_protocol_types::tool_output::ToolFailure::tool(
                lash_protocol_types::tool_output::ToolFailureClass::Execution,
                code,
                message,
            ),
        ),
    )
}

#[cfg(test)]
mod tests;
